//! 诊断用例、白名单数据契约与端口定义。
//!
//! @author kongweiguang

use std::{
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::AppResult;

const DIAGNOSTIC_SCHEMA: &str = "kerminal.diagnostics.v1";
const BUNDLE_SECTIONS: [&str; 8] = [
    "app",
    "environment",
    "runtimeHealth",
    "paths",
    "logs",
    "commandDatabase",
    "settings",
    "terminalSessions",
];

/// 采集源缺失或读取失败时使用的固定分类，不携带底层路径和错误文本。
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticUnavailableReason {
    NotFound,
    InvalidData,
    PermissionDenied,
    Unsupported,
    CollectionFailed,
}

/// 标明一项诊断数据是否真实取得，避免把缺失状态伪装成空列表或数值零。
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "reason", rename_all = "camelCase")]
pub enum DiagnosticSourceStatus {
    Available,
    Unavailable(DiagnosticUnavailableReason),
}

/// 一份带采集状态的诊断数据；只有 Available 才包含真实采集值。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "value", rename_all = "camelCase")]
pub enum DiagnosticSource<T> {
    Available(T),
    Unavailable(DiagnosticUnavailableReason),
}

impl<T> DiagnosticSource<T> {
    /// 生成与数据值分离的状态字段，兼容旧诊断分区并明确标出缺失来源。
    pub fn status(&self) -> DiagnosticSourceStatus {
        match self {
            Self::Available(_) => DiagnosticSourceStatus::Available,
            Self::Unavailable(reason) => DiagnosticSourceStatus::Unavailable(*reason),
        }
    }
}

/// 当前 Kerminal 进程资源快照；进程不存在时采集器应返回错误而不伪造健康值。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeProcessHealth {
    pub pid: u32,
    pub name: String,
    pub memory_bytes: Option<u64>,
    pub virtual_memory_bytes: Option<u64>,
    pub cpu_usage_percent: Option<f32>,
    pub started_at_seconds: Option<u64>,
    pub uptime_seconds: Option<u64>,
    pub disk_read_bytes: Option<u64>,
    pub disk_written_bytes: Option<u64>,
}

/// 本机系统资源快照；各指标为空时由 availability 中的来源状态解释。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSystemHealth {
    pub os: String,
    pub arch: String,
    pub host_name: Option<String>,
    pub kernel_version: Option<String>,
    pub os_version: Option<String>,
    pub cpu_count: Option<usize>,
    pub global_cpu_usage_percent: Option<f32>,
    pub cpu_core_usage_percents: Option<Vec<f32>>,
    pub total_memory_bytes: Option<u64>,
    pub used_memory_bytes: Option<u64>,
    pub available_memory_bytes: Option<u64>,
    pub total_swap_bytes: Option<u64>,
    pub used_swap_bytes: Option<u64>,
    pub uptime_seconds: Option<u64>,
    pub boot_time_seconds: Option<u64>,
    pub gpus: Option<Vec<RuntimeGpuHealth>>,
}

/// 已识别显卡的可用计量；不支持的指标保持 None，不以零代替未知。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeGpuHealth {
    pub name: String,
    pub vendor: Option<String>,
    pub driver_version: Option<String>,
    pub memory_total_bytes: Option<u64>,
    pub memory_used_bytes: Option<u64>,
    pub utilization_percent: Option<f32>,
    pub temperature_celsius: Option<f32>,
}

/// Kerminal 用户数据目录的受控路径和可选文件统计。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStorageHealth {
    pub root: String,
    pub command_database_file: String,
    pub app_log_file: String,
    pub logs: String,
    pub diagnostics: String,
    pub root_size_bytes: Option<u64>,
    pub command_database_file_size_bytes: Option<u64>,
    pub app_log_file_size_bytes: Option<u64>,
    pub app_log_max_file_size_bytes: Option<u64>,
    pub app_log_rotation_keep_files: Option<usize>,
}

/// 运行体检各采集源的实际状态；GPU 或存储不可读取时仍保留其余健康数据。
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeHealthAvailability {
    pub process: DiagnosticSourceStatus,
    pub system: DiagnosticSourceStatus,
    pub gpu: DiagnosticSourceStatus,
    pub storage: DiagnosticSourceStatus,
    pub command_database: DiagnosticSourceStatus,
    pub app_logs: DiagnosticSourceStatus,
}

/// CPU 两次刷新所采用的来源和间隔，帮助读者判断瞬时采样的口径。
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSamplingInfo {
    pub source: String,
    pub cpu_sample_interval_ms: u64,
    pub cpu_refreshed_twice: bool,
}

/// 对应旧版 runtime_health 快照，并补充每个可选数据源的可用性。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeHealthSnapshot {
    pub captured_at: String,
    pub process: RuntimeProcessHealth,
    pub system: RuntimeSystemHealth,
    pub storage: RuntimeStorageHealth,
    pub sampling: RuntimeSamplingInfo,
    pub availability: RuntimeHealthAvailability,
    pub redacted: bool,
}

/// 诊断包生成结果；relative_path 由应用固定生成，调用方不能传入或覆盖文件路径。
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticBundle {
    pub id: String,
    pub created_at: String,
    pub file_name: String,
    pub relative_path: String,
    pub bytes_written: u64,
    pub sections: Vec<String>,
    pub redacted: bool,
}

/// 仅包含产品标识和打包版本，不读取用户设置或凭据。
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticAppMetadata {
    pub name: String,
    pub version: String,
    pub rust_package: String,
}

/// 运行环境的公开平台信息，不包含用户名、环境变量或进程启动参数。
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticEnvironment {
    pub os: String,
    pub arch: String,
    pub family: String,
}

/// 诊断包中的固定路径别名，避免把真实 home 目录用户名带入可分享文件。
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticPaths {
    pub root: String,
    pub command_database_file: String,
    pub app_log_file: String,
    pub logs: String,
    pub cache: String,
    pub diagnostics: String,
}

/// 应用日志只输出文件统计，不把日志正文复制进默认诊断包。
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticLogs {
    pub directory: String,
    pub active_file: String,
    pub active_file_size_bytes: Option<u64>,
    pub max_file_size_bytes: Option<u64>,
    pub rotation_keep_files: Option<usize>,
    pub content_included: bool,
}

/// 命令数据库只暴露 schema 与文件大小，命令历史正文从不进入诊断报告。
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticCommandDatabase {
    pub schema_version: Option<u32>,
    pub file_size_bytes: Option<u64>,
}

/// 终端设置的白名单子集；任意字体文本在生成报告前还会经过凭据脱敏。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticTerminalSettings {
    pub font_family: String,
    pub font_size: u16,
    pub line_height: f64,
    pub cursor_blink: bool,
    pub scrollback: u32,
}

/// 保持旧诊断包中对主题、终端和快捷键数量的有效摘要。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticSettings {
    pub theme_mode: String,
    pub terminal: DiagnosticTerminalSettings,
    pub keybinding_count: usize,
}

/// 单条会话摘要只暴露必要的目标类别、所有者类别、生命周期和网格尺寸。
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticTerminalSession {
    pub target_kind: String,
    pub owner_kind: String,
    pub lifecycle: String,
    pub columns: u16,
    pub rows: u16,
    pub exit_code: Option<i32>,
}

/// 终端会话总数和状态计数；会话 ID、命令、cwd、主机引用和输出正文均不在白名单中。
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticTerminalSessions {
    pub total: usize,
    pub running: usize,
    pub exited: usize,
    pub items: Vec<DiagnosticTerminalSession>,
    pub raw_output_included: bool,
}

/// 配置恢复态只保留模式和问题计数，不携带字段值或主机文件名。
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticConfigurationRecovery {
    pub read_only: bool,
    pub issue_count: usize,
}

/// 所有本机诊断上下文的显式白名单投影。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticContextSnapshot {
    pub app: DiagnosticAppMetadata,
    pub environment: DiagnosticEnvironment,
    pub paths: DiagnosticPaths,
    pub logs: DiagnosticSource<DiagnosticLogs>,
    pub command_database: DiagnosticSource<DiagnosticCommandDatabase>,
    pub settings: DiagnosticSource<DiagnosticSettings>,
    pub terminal_sessions: DiagnosticSource<DiagnosticTerminalSessions>,
    pub configuration_recovery: DiagnosticConfigurationRecovery,
}

/// 脱敏后写入 ZIP 的完整业务报告；字段沿用 v0.3.45 的诊断分区名称。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticBundleReport {
    pub schema: String,
    pub created_at: String,
    pub app: DiagnosticAppMetadata,
    pub environment: DiagnosticEnvironment,
    pub runtime_health: RuntimeHealthSnapshot,
    pub paths: DiagnosticPaths,
    pub logs: Option<DiagnosticLogs>,
    pub command_database: Option<DiagnosticCommandDatabase>,
    pub settings: Option<DiagnosticSettings>,
    pub terminal_sessions: Option<DiagnosticTerminalSessions>,
    pub configuration_recovery: DiagnosticConfigurationRecovery,
    pub availability: DiagnosticBundleAvailability,
    pub security: DiagnosticSecurityPolicy,
    pub sections: Vec<String>,
}

/// 报告中每个历史诊断分区对应的采集状态，帮助区分缺项和真实空数据。
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticBundleAvailability {
    pub logs: DiagnosticSourceStatus,
    pub command_database: DiagnosticSourceStatus,
    pub settings: DiagnosticSourceStatus,
    pub terminal_sessions: DiagnosticSourceStatus,
}

/// 默认诊断包安全策略；明确禁止把历史、凭据、日志正文或终端输出复制进包。
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticSecurityPolicy {
    pub secret_redaction: bool,
    pub raw_terminal_output_included: bool,
    pub command_history_included: bool,
    pub credential_values_included: bool,
    pub log_content_included: bool,
}

/// 提供明确的健康数据和支持包上下文；适配器可分别报告局部缺失来源。
#[async_trait]
pub trait DiagnosticsContextProvider: Send + Sync + 'static {
    /// 采集设置、会话和存储的安全摘要，不读取命令正文或配置原文到报告中。
    async fn collect(&self) -> AppResult<DiagnosticContextSnapshot>;
}

/// 把系统探测隔离在基础设施层，应用用例不依赖 sysinfo 或平台命令。
#[async_trait]
pub trait RuntimeHealthCollector: Send + Sync + 'static {
    /// 采集当前进程、系统和数据根状态；缺失指标必须用 None 与 availability 表示。
    async fn collect(&self) -> AppResult<RuntimeHealthSnapshot>;
}

/// 归档端口仅接收已完成脱敏的报告，不接受任意目标路径或原始资源句柄。
#[async_trait]
pub trait DiagnosticBundleArchive: Send + Sync + 'static {
    /// 将安全报告写入应用管理的位置，并返回不可由调用方改写的相对引用。
    async fn create(&self, report: DiagnosticBundleReport) -> AppResult<DiagnosticBundle>;
}

/// diagnostics application service 协调只读采集与固定位置归档。
#[derive(Clone)]
pub struct DiagnosticsService {
    context_provider: Arc<dyn DiagnosticsContextProvider>,
    runtime_health_collector: Arc<dyn RuntimeHealthCollector>,
    archive: Arc<dyn DiagnosticBundleArchive>,
}

impl DiagnosticsService {
    /// 显式注入上下文、系统采集和归档端口，避免 MCP handler 拼接数据或持有文件路径。
    pub fn new(
        context_provider: Arc<dyn DiagnosticsContextProvider>,
        runtime_health_collector: Arc<dyn RuntimeHealthCollector>,
        archive: Arc<dyn DiagnosticBundleArchive>,
    ) -> Self {
        Self {
            context_provider,
            runtime_health_collector,
            archive,
        }
    }

    /// 查询本机运行健康；阻塞平台探测由基础设施 adapter 移入专用线程池。
    pub async fn runtime_health(&self) -> AppResult<RuntimeHealthSnapshot> {
        let mut snapshot = self.runtime_health_collector.collect().await?;
        redact_runtime_health(&mut snapshot);
        Ok(snapshot)
    }

    /// 生成完整 v1 支持包；会话与设置采集失败会在 availability 中显式标出，不伪装成功。
    pub async fn create_bundle(&self) -> AppResult<DiagnosticBundle> {
        let (mut context, mut runtime_health) = tokio::try_join!(
            self.context_provider.collect(),
            self.runtime_health_collector.collect(),
        )?;
        sanitize_context(&mut context);
        redact_runtime_health(&mut runtime_health);

        let created_at = unix_timestamp_string();
        let availability = DiagnosticBundleAvailability {
            logs: context.logs.status(),
            command_database: context.command_database.status(),
            settings: context.settings.status(),
            terminal_sessions: context.terminal_sessions.status(),
        };
        let report = DiagnosticBundleReport {
            schema: DIAGNOSTIC_SCHEMA.to_owned(),
            created_at,
            app: context.app,
            environment: context.environment,
            runtime_health,
            paths: context.paths,
            logs: source_value(context.logs),
            command_database: source_value(context.command_database),
            settings: source_value(context.settings),
            terminal_sessions: source_value(context.terminal_sessions),
            configuration_recovery: context.configuration_recovery,
            availability,
            security: DiagnosticSecurityPolicy {
                secret_redaction: true,
                raw_terminal_output_included: false,
                command_history_included: false,
                credential_values_included: false,
                log_content_included: false,
            },
            sections: BUNDLE_SECTIONS
                .iter()
                .map(|section| (*section).to_owned())
                .collect(),
        };
        self.archive.create(report).await
    }
}

/// 取出来源中真实存在的内容；Unavailable 原因会同步放入报告的 availability 区域。
fn source_value<T>(source: DiagnosticSource<T>) -> Option<T> {
    match source {
        DiagnosticSource::Available(value) => Some(value),
        DiagnosticSource::Unavailable(_) => None,
    }
}

/// 对所有可自由输入的上下文文本应用统一脱敏，阻止适配器意外传入原始凭据。
fn sanitize_context(context: &mut DiagnosticContextSnapshot) {
    context.app.name = redact_diagnostic_text(&context.app.name);
    context.app.version = redact_diagnostic_text(&context.app.version);
    context.app.rust_package = redact_diagnostic_text(&context.app.rust_package);
    context.environment.os = redact_diagnostic_text(&context.environment.os);
    context.environment.arch = redact_diagnostic_text(&context.environment.arch);
    context.environment.family = redact_diagnostic_text(&context.environment.family);
    context.paths.root = redact_diagnostic_text(&context.paths.root);
    context.paths.command_database_file =
        redact_diagnostic_text(&context.paths.command_database_file);
    context.paths.app_log_file = redact_diagnostic_text(&context.paths.app_log_file);
    context.paths.logs = redact_diagnostic_text(&context.paths.logs);
    context.paths.cache = redact_diagnostic_text(&context.paths.cache);
    context.paths.diagnostics = redact_diagnostic_text(&context.paths.diagnostics);
    if let DiagnosticSource::Available(logs) = &mut context.logs {
        logs.directory = redact_diagnostic_text(&logs.directory);
        logs.active_file = redact_diagnostic_text(&logs.active_file);
        logs.content_included = false;
    }
    if let DiagnosticSource::Available(settings) = &mut context.settings {
        settings.theme_mode = redact_diagnostic_text(&settings.theme_mode);
        settings.terminal.font_family = redact_diagnostic_text(&settings.terminal.font_family);
    }
    if let DiagnosticSource::Available(sessions) = &mut context.terminal_sessions {
        for session in &mut sessions.items {
            session.target_kind = redact_diagnostic_text(&session.target_kind);
            session.owner_kind = redact_diagnostic_text(&session.owner_kind);
            session.lifecycle = redact_diagnostic_text(&session.lifecycle);
        }
        sessions.raw_output_included = false;
    }
}

/// 清理系统自由文本字段；数值为空时保留为空，不把探测缺失转换成零。
fn redact_runtime_health(snapshot: &mut RuntimeHealthSnapshot) {
    snapshot.process.name = redact_diagnostic_text(&snapshot.process.name);
    snapshot.system.os = redact_diagnostic_text(&snapshot.system.os);
    snapshot.system.arch = redact_diagnostic_text(&snapshot.system.arch);
    snapshot.system.host_name = snapshot
        .system
        .host_name
        .as_deref()
        .map(redact_diagnostic_text);
    snapshot.system.kernel_version = snapshot
        .system
        .kernel_version
        .as_deref()
        .map(redact_diagnostic_text);
    snapshot.system.os_version = snapshot
        .system
        .os_version
        .as_deref()
        .map(redact_diagnostic_text);
    if let Some(gpus) = &mut snapshot.system.gpus {
        for gpu in gpus {
            gpu.name = redact_diagnostic_text(&gpu.name);
            gpu.vendor = gpu.vendor.as_deref().map(redact_diagnostic_text);
            gpu.driver_version = gpu.driver_version.as_deref().map(redact_diagnostic_text);
        }
    }
    snapshot.storage.root = redact_diagnostic_text(&snapshot.storage.root);
    snapshot.storage.command_database_file =
        redact_diagnostic_text(&snapshot.storage.command_database_file);
    snapshot.storage.app_log_file = redact_diagnostic_text(&snapshot.storage.app_log_file);
    snapshot.storage.logs = redact_diagnostic_text(&snapshot.storage.logs);
    snapshot.redacted = true;
}

/// 屏蔽常见赋值凭据、Bearer/API key 与私钥块；规则初始化失败时整段内容直接省略。
pub fn redact_diagnostic_text(value: &str) -> String {
    static PATTERNS: OnceLock<Option<Vec<Regex>>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        let pem = RegexBuilder::new(
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
        )
        .dot_matches_new_line(true)
        .build();
        let bearer = Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+");
        let assignment = Regex::new(
            r#"(?i)((?:"?(?:password|passwd|secret|token|api[_-]?key|access[_-]?token|refresh[_-]?token|client[_-]?secret|key[_-]?passphrase|authorization)"?)\s*[:=]\s*)(?:"[^"]*"|'[^']*'|[^\s,;}\]]+)"#,
        );
        let provider_key = Regex::new(r"\b(?:sk-[A-Za-z0-9_-]{8,}|gh[pousr]_[A-Za-z0-9_]{20,})\b");
        match (pem, bearer, assignment, provider_key) {
            (Ok(pem), Ok(bearer), Ok(assignment), Ok(provider_key)) => {
                Some(vec![pem, bearer, assignment, provider_key])
            }
            _ => None,
        }
    });

    let Some(patterns) = patterns else {
        return "[诊断文本因脱敏器不可用而省略]".to_owned();
    };
    let mut sanitized = value.to_owned();
    for (index, pattern) in patterns.iter().enumerate() {
        let replacement = match index {
            0 => "[REDACTED_PRIVATE_KEY]",
            1 | 3 => "[REDACTED]",
            _ => "$1\"[REDACTED]\"",
        };
        sanitized = pattern.replace_all(&sanitized, replacement).into_owned();
    }
    sanitized
}

/// 生成兼容 v0.3.45 的 Unix 秒字符串；时钟早于 epoch 时明确写 unknown。
fn unix_timestamp_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "unknown".to_owned())
}

/// 按旧诊断摘要展示内存与容量；缺少真实读数时返回“未知”而非虚构百分比。
pub fn percent_summary(used: Option<u64>, total: Option<u64>) -> String {
    match (used, total) {
        (Some(used), Some(total)) if total > 0 => {
            format!("{:.1}%", used as f64 * 100.0 / total as f64)
        }
        _ => "未知".to_owned(),
    }
}

/// 以二进制单位格式化实际可用的计数值。
pub fn byte_size_summary(bytes: Option<u64>) -> String {
    let Some(bytes) = bytes else {
        return "未知".to_owned();
    };
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.1} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}
