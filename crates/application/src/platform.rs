//! 桌面平台边界：系统托盘、系统通知与外部启动请求。  
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use uuid::Uuid;

use crate::{AppError, AppResult};

pub(crate) mod command_line;
pub mod external_launch;
pub mod window_state;
pub use external_launch::ExternalActivation;

/// 系统托盘菜单保留旧版显示、隐藏和退出三种明确动作。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayAction {
    Show,
    Hide,
    Quit,
}

/// 平台能力可能随系统托盘宿主或系统策略变化，UI 可将条件和原因直接展示给用户。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlatformAvailability {
    Available,
    Conditional { reason: String },
    Unsupported { reason: String },
}

/// 原生托盘安装接口不要求 Send，确保 macOS 实现可以由 GPUI 主线程持有和销毁。
pub trait TrayPort {
    /// 必须在平台要求的 UI 线程与事件循环时序内调用；调用方持有 adapter 以维持托盘生命周期。
    fn install(&mut self, actions: tokio::sync::mpsc::Sender<TrayAction>) -> AppResult<()>;

    /// 返回当前平台与桌面托盘宿主的支持边界，不把编译成功误报为图标一定可见。
    fn availability(&self) -> PlatformAvailability;
}

/// 通知事件重要程度用于实现设置过滤和前台短任务抑制。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationImportance {
    Low,
    Normal,
    High,
}

/// 前台窗口只压制短时普通任务，后台或最小化窗口保留完成提醒。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowVisibility {
    Foreground,
    Hidden,
    Minimized,
}

/// 旧版已有的通知类型以结构化事件承载，避免调用者拼接易泄露本机路径的自由文本。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DesktopNotificationEvent {
    SftpBatchCompleted {
        host_label: Option<String>,
        total_count: u32,
        succeeded_count: Option<u32>,
        failed_count: Option<u32>,
        duration: Duration,
        importance: Option<NotificationImportance>,
    },
    SftpTransferFailed {
        host_label: Option<String>,
        failed_count: u32,
        importance: Option<NotificationImportance>,
    },
    AgentFinished {
        agent_name: String,
        exit_code: Option<i32>,
        duration: Duration,
        importance: Option<NotificationImportance>,
    },
    UpdateAvailable {
        version: String,
        current_version: Option<String>,
        importance: Option<NotificationImportance>,
    },
    McpServerFailed {
        port: Option<u16>,
        reason: Option<String>,
        importance: Option<NotificationImportance>,
    },
}

impl DesktopNotificationEvent {
    /// 对应旧通知族建立稳定节流键；外部可变标签只参与内存键，不进入日志或系统通知标题。
    fn key(&self) -> String {
        match self {
            Self::SftpBatchCompleted { host_label, .. } => event_key(
                "sftp.batch.completed",
                host_label.as_deref().unwrap_or("unknown"),
            ),
            Self::SftpTransferFailed { host_label, .. } => event_key(
                "sftp.transfer.failed",
                host_label.as_deref().unwrap_or("unknown"),
            ),
            Self::AgentFinished { agent_name, .. } => {
                event_key("agent.process.finished", agent_name)
            }
            Self::UpdateAvailable { version, .. } => event_key("updater.available", version),
            Self::McpServerFailed { port, .. } => {
                format!(
                    "mcp.server.failed:{}",
                    port.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
                )
            }
        }
    }

    /// 错误事件默认高优先级，成功事件默认普通；调用方可为明确业务场景覆盖默认值。
    fn importance(&self) -> NotificationImportance {
        let override_value = match self {
            Self::SftpBatchCompleted { importance, .. }
            | Self::SftpTransferFailed { importance, .. }
            | Self::AgentFinished { importance, .. }
            | Self::UpdateAvailable { importance, .. }
            | Self::McpServerFailed { importance, .. } => *importance,
        };
        override_value.unwrap_or(match self {
            Self::SftpTransferFailed { .. } | Self::McpServerFailed { .. } => {
                NotificationImportance::High
            }
            Self::SftpBatchCompleted { .. }
            | Self::AgentFinished { .. }
            | Self::UpdateAvailable { .. } => NotificationImportance::Normal,
        })
    }

    /// 生成只包含任务摘要的文案，凭据字段、命令行和本机路径不会进入通知内容。
    fn payload(&self) -> NotificationPayload {
        let (title, body) = match self {
            Self::SftpBatchCompleted {
                host_label,
                total_count,
                succeeded_count,
                failed_count,
                ..
            } => {
                let failed = failed_count.unwrap_or(0).min(*total_count);
                let succeeded = succeeded_count
                    .unwrap_or_else(|| total_count.saturating_sub(failed))
                    .min(total_count.saturating_sub(failed));
                let title = if failed > 0 {
                    "SFTP 传输完成，部分失败"
                } else {
                    "SFTP 传输完成"
                };
                (
                    title.to_owned(),
                    format!(
                        "{succeeded}/{total_count} 项已传输{}。",
                        target_suffix(host_label.as_deref())
                    ),
                )
            }
            Self::SftpTransferFailed {
                host_label,
                failed_count,
                ..
            } => (
                "SFTP 传输失败".to_owned(),
                format!(
                    "{failed_count} 项传输失败{}。",
                    target_suffix(host_label.as_deref())
                ),
            ),
            Self::AgentFinished {
                agent_name,
                exit_code,
                ..
            } => {
                let result =
                    exit_code.map_or_else(|| "已结束".to_owned(), |code| format!("退出码 {code}"));
                (
                    "Agent 任务完成".to_owned(),
                    format!("{}：{result}。", sanitize_notification_text(agent_name, 72)),
                )
            }
            Self::UpdateAvailable {
                version,
                current_version,
                ..
            } => {
                let version = sanitize_notification_text(version, 72);
                let body = current_version.as_deref().map_or_else(
                    || format!("版本 {version} 已可用。"),
                    |current| {
                        format!(
                            "版本 {version} 已可用；当前版本 {}。",
                            sanitize_notification_text(current, 72)
                        )
                    },
                );
                ("Kerminal 更新可用".to_owned(), body)
            }
            Self::McpServerFailed { port, reason, .. } => {
                let port_text = port.map_or_else(String::new, |value| format!("（端口 {value}）"));
                let detail = reason.as_deref().map_or_else(
                    || "打开 Kerminal 查看详情。".to_owned(),
                    |value| format!("{}。", sanitize_notification_text(value, 120)),
                );
                (
                    "MCP 服务异常".to_owned(),
                    format!("本地 MCP 服务{port_text}失败：{detail}"),
                )
            }
        };
        NotificationPayload {
            title: sanitize_notification_text(&title, 72),
            body: sanitize_notification_text(&body, 180),
        }
    }

    /// 普通前台通知按实际耗时抑制；失败等高优先级事件不受这个阈值影响。
    fn duration(&self) -> Duration {
        match self {
            Self::SftpBatchCompleted { duration, .. } | Self::AgentFinished { duration, .. } => {
                *duration
            }
            _ => Duration::ZERO,
        }
    }
}

/// 通知设置采用旧版默认值；应用设置模块负责持久化及字段归一化。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopNotificationSettings {
    pub enabled: bool,
    pub background_only: bool,
    pub important_only: bool,
    pub min_duration: Duration,
    pub throttle: Duration,
}

impl From<crate::preferences::settings::DesktopNotificationSettings>
    for DesktopNotificationSettings
{
    /// 把持久化的毫秒偏好投影为运行时策略，并在消费者边界再次钳制范围。
    fn from(settings: crate::preferences::settings::DesktopNotificationSettings) -> Self {
        Self {
            enabled: settings.enabled,
            background_only: settings.background_only,
            important_only: settings.important_only,
            min_duration: Duration::from_millis(
                u64::from(settings.min_duration_ms).clamp(1_000, 120_000),
            ),
            throttle: Duration::from_millis(u64::from(settings.throttle_ms).min(600_000)),
        }
    }
}

impl Default for DesktopNotificationSettings {
    /// 默认保持通知关闭，避免升级后绕过用户此前的明确设置。
    fn default() -> Self {
        Self {
            enabled: false,
            background_only: true,
            important_only: false,
            min_duration: Duration::from_secs(10),
            throttle: Duration::from_secs(30),
        }
    }
}

/// 清理后的系统通知文案；实现层仍必须由操作系统通知服务决定是否实际呈现。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationPayload {
    pub title: String,
    pub body: String,
}

/// 系统通知发送接口；平台适配器负责将阻塞 API 放入专用线程池。
#[async_trait]
pub trait NotificationPort: Send + Sync + 'static {
    /// 返回成功只表示 OS 通知 API 接受请求，不代表用户已看到或授权通知。
    async fn send(&self, payload: NotificationPayload) -> AppResult<()>;
}

/// 一次通知的结果区分用户设置过滤、节流和已提交给系统服务。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationDisposition {
    Submitted,
    Disabled,
    BackgroundOnly,
    ImportantOnly,
    ForegroundShortEvent,
    Throttled,
}

/// 应用层通知策略负责设置、前台抑制、节流和脱敏，基础设施只发送最终 payload。
#[derive(Clone)]
pub struct DesktopNotificationService {
    port: Arc<dyn NotificationPort>,
    settings: Arc<RwLock<DesktopNotificationSettings>>,
    visibility: Arc<RwLock<WindowVisibility>>,
    last_sent_at: Arc<Mutex<HashMap<String, (Instant, Uuid)>>>,
}

impl DesktopNotificationService {
    /// 要求 composition 注入已读取的用户偏好，避免默认策略覆盖现存配置。
    pub fn new(
        port: Arc<dyn NotificationPort>,
        settings: crate::preferences::settings::DesktopNotificationSettings,
    ) -> Self {
        Self {
            port,
            settings: Arc::new(RwLock::new(settings.into())),
            // 未收到真实窗口状态前按前台处理，确保 background_only 不会误报后台通知。
            visibility: Arc::new(RwLock::new(WindowVisibility::Foreground)),
            last_sent_at: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 由窗口生命周期事件更新共享可见性；缺少状态时仍保持前台安全默认值。
    pub fn set_window_visibility(&self, visibility: WindowVisibility) -> AppResult<()> {
        *self
            .visibility
            .write()
            .map_err(|_| AppError::infrastructure("通知窗口状态不可用"))? = visibility;
        Ok(())
    }

    /// 返回 composition 与 UI 共用的最新窗口状态，未知状态以初始化的前台值表示。
    pub fn window_visibility(&self) -> AppResult<WindowVisibility> {
        self.visibility
            .read()
            .map(|visibility| *visibility)
            .map_err(|_| AppError::infrastructure("通知窗口状态不可用"))
    }

    /// 设置保存后更新同一应用服务实例，后续通知立即使用新策略而不读文件或 UI 状态。
    pub fn update_settings(
        &self,
        settings: crate::preferences::settings::DesktopNotificationSettings,
    ) -> AppResult<()> {
        *self
            .settings
            .write()
            .map_err(|_| AppError::infrastructure("通知策略不可用"))? = settings.into();
        Ok(())
    }

    /// 发送前短暂锁定去重预留；异步系统调用期间不持锁，失败只撤销本请求的 UUID 预留。
    ///
    /// @author kongweiguang
    pub async fn notify(
        &self,
        event: DesktopNotificationEvent,
        visibility: WindowVisibility,
    ) -> AppResult<NotificationDisposition> {
        let settings = *self
            .settings
            .read()
            .map_err(|_| AppError::infrastructure("通知策略不可用"))?;
        if !settings.enabled {
            return Ok(NotificationDisposition::Disabled);
        }

        let importance = event.importance();
        if settings.important_only && importance != NotificationImportance::High {
            return Ok(NotificationDisposition::ImportantOnly);
        }
        if settings.background_only && visibility == WindowVisibility::Foreground {
            return Ok(NotificationDisposition::BackgroundOnly);
        }
        let min_duration = settings
            .min_duration
            .max(Duration::from_secs(1))
            .min(Duration::from_secs(120));
        let throttle = settings.throttle.min(Duration::from_secs(600));
        if visibility == WindowVisibility::Foreground
            && importance != NotificationImportance::High
            && event.duration() < min_duration
        {
            return Ok(NotificationDisposition::ForegroundShortEvent);
        }

        let key = event.key();
        let now = Instant::now();
        let reservation_id = Uuid::new_v4();
        {
            let mut last_sent_at = self
                .last_sent_at
                .lock()
                .map_err(|_| AppError::infrastructure("通知节流状态不可用"))?;
            last_sent_at.retain(|_, (sent_at, _)| {
                now.saturating_duration_since(*sent_at) < Duration::from_secs(30 * 60)
            });
            if last_sent_at
                .get(&key)
                .is_some_and(|(sent_at, _)| now.saturating_duration_since(*sent_at) < throttle)
            {
                return Ok(NotificationDisposition::Throttled);
            }
            if last_sent_at.len() >= 256
                && let Some(oldest_key) = last_sent_at
                    .iter()
                    .min_by_key(|(_, reservation)| reservation.0)
                    .map(|(key, _)| key.clone())
            {
                last_sent_at.remove(&oldest_key);
            }
            last_sent_at.insert(key.clone(), (now, reservation_id));
        }

        if let Err(error) = self.port.send(event.payload()).await {
            if let Ok(mut last_sent_at) = self.last_sent_at.lock()
                && last_sent_at
                    .get(&key)
                    .is_some_and(|(_, current_id)| *current_id == reservation_id)
            {
                last_sent_at.remove(&key);
            }
            return Err(error);
        }
        Ok(NotificationDisposition::Submitted)
    }
}

/// Windows 协议处理程序查询结果只暴露当前 Kerminal 是否拥有用户级关联。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExternalProtocolStatus {
    pub supported: bool,
    pub registered: bool,
}

/// 外部协议注册端口仅供设置界面的显式用户动作调用。
pub trait ExternalProtocolPort: Send + Sync + 'static {
    /// 查询系统级能力与当前程序关联，不在应用启动时自动修改系统设置。
    fn status(&self) -> AppResult<ExternalProtocolStatus>;

    /// 用户明确启用 kerminal:// 后才建立当前账户的协议关联。
    fn register_for_current_user(&self) -> AppResult<ExternalProtocolStatus>;

    /// 只移除指向当前 Kerminal 可执行文件的当前账户关联。
    fn unregister_for_current_user(&self) -> AppResult<ExternalProtocolStatus>;
}

/// 外部协议设置用例保持操作显式，避免启动时覆盖用户已经选择的 URL handler。
#[derive(Clone)]
pub struct ExternalProtocolService {
    port: Arc<dyn ExternalProtocolPort>,
}

impl ExternalProtocolService {
    /// composition root 装配平台 adapter；非 Windows adapter 只报告不可用。
    pub fn new(port: Arc<dyn ExternalProtocolPort>) -> Self {
        Self { port }
    }

    /// 将可能阻塞的平台注册表读取放进 Tokio blocking pool，避免占住应用异步 worker。
    ///
    /// @author kongweiguang
    pub async fn status(&self) -> AppResult<ExternalProtocolStatus> {
        let port = Arc::clone(&self.port);
        tokio::task::spawn_blocking(move || port.status())
            .await
            .map_err(|error| {
                AppError::infrastructure_with_source("外部协议状态读取后台任务失败", error)
            })?
    }

    /// 注册可能跨越系统调用边界；worker 异常时回执未知，必须由调用方显式回读确认。
    ///
    /// @author kongweiguang
    pub async fn enable_for_current_user(&self) -> AppResult<ExternalProtocolStatus> {
        let port = Arc::clone(&self.port);
        tokio::task::spawn_blocking(move || port.register_for_current_user())
            .await
            .map_err(|error| AppError::outcome_unknown_with_source("外部协议注册结果未知", error))?
    }

    /// 注销也可能在系统已写入后失去 worker 回执，因此异常不伪装成确定失败。
    ///
    /// @author kongweiguang
    pub async fn disable_for_current_user(&self) -> AppResult<ExternalProtocolStatus> {
        let port = Arc::clone(&self.port);
        tokio::task::spawn_blocking(move || port.unregister_for_current_user())
            .await
            .map_err(|error| AppError::outcome_unknown_with_source("外部协议注销结果未知", error))?
    }
}

/// 限制通知标题和正文长度，并屏蔽常见凭据格式及用户目录，避免 OS 通知泄露本机信息；已生成的占位符需保持稳定，防止后续规则覆盖脱敏类别。
fn sanitize_notification_text(value: &str, max_length: usize) -> String {
    use std::sync::OnceLock;

    static PATTERNS: OnceLock<Result<Vec<(regex::Regex, &'static str)>, regex::Error>> =
        OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            (
                r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
                "[已隐藏私钥]",
            ),
            (r"\bBearer\s+[A-Za-z0-9._~+/=-]+", "Bearer [已隐藏]"),
            (r"\bsk-[A-Za-z0-9_-]{12,}\b", "[已隐藏令牌]"),
            (
                r"\b(password|passwd|token|api[_-]?key|secret)\s*[:=]\s*([^,\s;]+)",
                "$1=[已隐藏]",
            ),
            (r"\b[A-Za-z]:\\Users\\[^\\\s]+\\[^\s,;]+", "[本机路径]"),
            (r"\b(?:/Users|/home)/[^/\s]+/[^\s,;]+", "[本机路径]"),
        ]
        .into_iter()
        .map(|(pattern, replacement)| {
            regex::Regex::new(pattern).map(|compiled| (compiled, replacement))
        })
        .collect()
    });

    let Ok(patterns) = patterns else {
        return "[通知内容已省略]".to_owned();
    };
    let mut text = value.to_owned();
    for (pattern, replacement) in patterns {
        if *replacement == "$1=[已隐藏]" {
            text = pattern
                .replace_all(&text, |captures: &regex::Captures<'_>| {
                    let value = captures.get(2).map_or("", |capture| capture.as_str());
                    if matches!(
                        value,
                        "[已隐藏]" | "[已隐藏令牌]" | "[已隐藏私钥]" | "[本机路径]"
                    ) {
                        captures
                            .get(0)
                            .map_or("", |capture| capture.as_str())
                            .to_owned()
                    } else {
                        format!(
                            "{}=[已隐藏]",
                            captures.get(1).map_or("", |capture| capture.as_str())
                        )
                    }
                })
                .into_owned();
        } else {
            text = pattern.replace_all(&text, *replacement).into_owned();
        }
    }
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.len() <= max_length {
        return normalized;
    }
    let mut boundary = max_length.saturating_sub(3).min(normalized.len());
    while !normalized.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    format!("{}...", normalized[..boundary].trim_end())
}

/// 主机标签仅作为 SFTP 通知的可选说明，并由调用方统一脱敏和长度限制。
fn target_suffix(host_label: Option<&str>) -> String {
    host_label
        .map(|label| format!("（{}）", sanitize_notification_text(label, 72)))
        .unwrap_or_default()
}

/// 用有界固定长度哈希生成节流键，避免任意主机标签或 Agent 名称扩张长期内存状态。
fn event_key(family: &str, value: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{family}:{:016x}", hasher.finish())
}
