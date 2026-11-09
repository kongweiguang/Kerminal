//! 主机目录管理用例；只暴露类型化连接元数据与 vault 引用。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use kerminal_domain::{
    connections::credentials::{
        CredentialKind, CredentialMaterial, CredentialReference, CredentialScope,
    },
    connections::{CredentialRef, HostEndpoint, ProxyProfile, RemoteProtocol, SshAuthentication},
    identity::HostId,
    preferences::{ConfigDocumentPath, ConfigRevision},
    sessions::SerialSessionTarget,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    AppError, AppResult,
    connections::{
        CredentialResolver,
        credentials::{CredentialService, SaveCredentialRequest, SecretValue},
        host_types::{PluginHost, PluginHostDraft, valid_host_type_id},
    },
};

mod local_hosts;
pub use local_hosts::validate_local_environment_reference;

/// 主机备注采用字符数而非 UTF-8 字节数限制，便于中文文本与多行备注保持直观边界。
pub const HOST_NOTE_MAX_CHARS: usize = 4096;

/// 仅将无业务内容的备注折叠为 None，避免保留空白字段；有内容时逐字保留用户换行。
///
/// @author kongweiguang
pub fn normalize_host_note(note: Option<String>) -> Option<String> {
    note.filter(|value| !value.trim().is_empty())
}

/// 备注支持常见文本换行和制表符，但拒绝其它控制字符以避免终端和诊断输出异常。
///
/// @author kongweiguang
pub fn host_note_is_valid(note: &str) -> bool {
    note.chars().count() <= HOST_NOTE_MAX_CHARS
        && note
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
}

/// 连接目录中可编辑的 SSH 跳板字段；口令和私钥正文只能通过 vault 引用表达。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedJumpHost {
    pub name: String,
    pub endpoint: HostEndpoint,
    pub authentication: Option<SshAuthentication>,
    pub authentication_mode: HostAuthenticationMode,
}

/// SSH/SFTP 的可编辑路由；终端与传输默认值独立建模，不与跳板凭据所有权混合。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HostRouteSettings {
    pub proxy: Option<ProxyProfile>,
    pub jump_hosts: Vec<ManagedJumpHost>,
}

/// SSH 交互终端的主机默认值；显式启动请求仍优先，空值让 provider 保持平台默认。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HostTerminalOptions {
    pub term: Option<String>,
    pub initial_working_directory: Option<String>,
}

/// 单份主机 SFTP 设置供首次文件工作区和队列策略共用，避免 UI 与执行器各自缓存配置。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostSftpOptions {
    pub enabled: bool,
    pub local_start_directory: Option<PathBuf>,
    pub remote_start_directory: Option<String>,
    pub preserve_mtime: bool,
    pub follow_source_symlinks: bool,
    pub concurrency: Option<u16>,
}

impl Default for HostSftpOptions {
    /// 新主机保留既有 SFTP 能力和修改时间语义；可选并发由全局策略决定。
    fn default() -> Self {
        Self {
            enabled: true,
            local_start_directory: None,
            remote_start_directory: None,
            preserve_mtime: true,
            follow_source_symlinks: false,
            concurrency: None,
        }
    }
}

impl HostSftpOptions {
    /// SFTP-only 主机无条件具备文件能力；该开关只控制 SSH 上的可选子系统。
    /// @author kongweiguang
    pub const fn enabled_for(protocol: RemoteProtocol, configured: bool) -> bool {
        match protocol {
            RemoteProtocol::Sftp => true,
            RemoteProtocol::Ssh => configured,
            RemoteProtocol::Rdp | RemoteProtocol::Telnet | RemoteProtocol::Serial => false,
        }
    }
}

/// RDP 显示选项以同一配置快照驱动平台启动，不让客户端 adapter 猜测用户选择。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RdpDisplayOptions {
    pub fullscreen: bool,
    pub width: Option<u16>,
    pub height: Option<u16>,
}

impl Default for RdpDisplayOptions {
    /// 保持当前 launcher 的全屏默认值；缺省尺寸交由客户端选择。
    fn default() -> Self {
        Self {
            fullscreen: true,
            width: None,
            height: None,
        }
    }
}

impl RdpDisplayOptions {
    /// 平台适配器和配置入口共用成对尺寸规则，拒绝半份或越界配置而非猜测补齐。
    ///
    /// @author kongweiguang
    pub fn validate(&self) -> AppResult<()> {
        if self.width.is_some() != self.height.is_some()
            || self
                .width
                .is_some_and(|width| !(640..=8192).contains(&width))
            || self
                .height
                .is_some_and(|height| !(480..=8192).contains(&height))
        {
            return Err(AppError::invalid("RDP 窗口尺寸必须成对填写且在支持范围内"));
        }
        Ok(())
    }
}

/// 主机目录条目；连接配置与 UI 状态共用同一份 revision，避免旧表单覆盖外部编辑。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedHost {
    pub id: HostId,
    pub group_id: Option<String>,
    pub name: String,
    /// 用户维护的多行说明，不参与连接目标或凭据解析。
    pub note: Option<String>,
    pub protocol: RemoteProtocol,
    pub endpoint: HostEndpoint,
    pub authentication: Option<SshAuthentication>,
    pub authentication_mode: HostAuthenticationMode,
    pub rdp_password: Option<CredentialRef>,
    pub route: HostRouteSettings,
    pub serial: Option<SerialSessionTarget>,
    pub tags: Vec<String>,
    pub sort_order: i64,
    pub connect_timeout_ms: u64,
    pub keepalive_interval_ms: Option<u64>,
    pub terminal_options: HostTerminalOptions,
    pub sftp_options: HostSftpOptions,
    pub rdp_display: RdpDisplayOptions,
    pub created_at: String,
    pub updated_at: String,
    pub revision: ConfigRevision,
}

/// 本地主机独立于终端模板保存；列表不解密环境变量，避免目录读取依赖可用密钥。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedLocalHost {
    pub id: HostId,
    pub group_id: Option<String>,
    pub name: String,
    pub shell: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub environment_ref: Option<CredentialRef>,
    pub sort_order: i64,
    pub created_at: String,
    pub updated_at: String,
    pub revision: ConfigRevision,
    pub migration_origin: Option<String>,
}

/// 编辑器提交完整环境快照；调试输出不泄露环境变量或其内容。
///
/// @author kongweiguang
#[derive(Clone, Eq, PartialEq)]
pub struct LocalHostDraft {
    pub id: Option<HostId>,
    pub group_id: Option<String>,
    pub name: String,
    pub shell: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub sort_order: i64,
}

impl std::fmt::Debug for LocalHostDraft {
    /// 环境可能含敏感值，日志只记录字段存在性而不复制变量名或内容。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalHostDraft")
            .field("id", &self.id)
            .field("group_id", &self.group_id)
            .field("name", &self.name)
            .field("shell", &self.shell)
            .field("args", &self.args)
            .field("cwd", &self.cwd)
            .field("env", &"[redacted]")
            .field("sort_order", &self.sort_order)
            .finish()
    }
}

/// 保存结果未知时保留本次新旧密文身份，供确认主机快照后安全清理。
///
/// @author kongweiguang
#[derive(Clone, Eq, PartialEq)]
pub struct LocalHostSaveRecoveryToken {
    operation_id: String,
    host_id: HostId,
    baseline_revision: Option<ConfigRevision>,
    old_reference: Option<CredentialRef>,
    new_reference: Option<CredentialRef>,
}

impl LocalHostSaveRecoveryToken {
    /// 只接受本地主机专属引用，确保恢复日志不能用于清理其他凭据。
    ///
    /// @author kongweiguang
    pub fn from_parts(
        operation_id: String,
        host_id: HostId,
        baseline_revision: Option<ConfigRevision>,
        old_reference: Option<CredentialRef>,
        new_reference: Option<CredentialRef>,
    ) -> AppResult<Self> {
        Uuid::parse_str(&operation_id).map_err(|_| AppError::invalid("本地主机恢复标识无效"))?;
        ConfigDocumentPath::Host(host_id.as_str().to_owned())
            .relative_path()
            .map_err(|_| AppError::invalid("本地主机恢复目标无效"))?;
        if baseline_revision.as_ref().is_some_and(|revision| {
            revision.as_str().len() != 64
                || !revision
                    .as_str()
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }) {
            return Err(AppError::invalid("本地主机恢复 revision 无效"));
        }
        for reference in old_reference.iter().chain(new_reference.iter()) {
            validate_local_environment_reference(&host_id, reference)?;
        }
        Ok(Self {
            operation_id,
            host_id,
            baseline_revision,
            old_reference,
            new_reference,
        })
    }

    /// 持久化日志以随机操作 ID 区分不同保存，避免清理迟到的旧事务。
    ///
    /// @author kongweiguang
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    /// 将恢复动作绑定到原主机，避免迟到读回处理其他编辑器。
    ///
    /// @author kongweiguang
    pub fn host_id(&self) -> &HostId {
        &self.host_id
    }

    /// 保留原 CAS 基线，供编辑器区分提交前状态与更新后的第三方内容。
    ///
    /// @author kongweiguang
    pub fn baseline_revision(&self) -> Option<&ConfigRevision> {
        self.baseline_revision.as_ref()
    }

    /// 只供仓储持久化 opaque 引用，不提供环境变量明文。
    ///
    /// @author kongweiguang
    pub fn old_reference(&self) -> Option<&CredentialRef> {
        self.old_reference.as_ref()
    }

    /// 保存候选引用以便崩溃后判断是否需要清理。
    ///
    /// @author kongweiguang
    pub fn new_reference(&self) -> Option<&CredentialRef> {
        self.new_reference.as_ref()
    }
}

impl std::fmt::Debug for LocalHostSaveRecoveryToken {
    /// 恢复材料不应进入日志，避免暴露 vault 引用或本地主机身份。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LocalHostSaveRecoveryToken([redacted])")
    }
}

/// 区分确认提交与未知结果，避免调用方对可能已提交的写入盲目重试。
///
/// @author kongweiguang
pub enum RecoverableLocalHostSaveOutcome {
    Saved {
        host: ManagedLocalHost,
        credential_cleanup: CredentialCleanupStatus,
    },
    OutcomeUnknown {
        error: AppError,
        recovery: LocalHostSaveRecoveryToken,
    },
}

/// 主机编辑草稿；不含明文凭据，也不承担磁盘 revision 的生成责任。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostDraft {
    pub id: Option<HostId>,
    pub group_id: Option<String>,
    pub name: String,
    /// 保存前以空白折叠为 None；非空内容及其换行按原样持久化。
    pub note: Option<String>,
    pub protocol: RemoteProtocol,
    pub endpoint: HostEndpoint,
    pub authentication: Option<SshAuthentication>,
    pub authentication_mode: HostAuthenticationMode,
    pub rdp_password: Option<CredentialRef>,
    pub route: HostRouteSettings,
    pub serial: Option<SerialSessionTarget>,
    pub tags: Vec<String>,
    pub sort_order: i64,
    pub connect_timeout_ms: u64,
    pub keepalive_interval_ms: Option<u64>,
    pub terminal_options: HostTerminalOptions,
    pub sftp_options: HostSftpOptions,
    pub rdp_display: RdpDisplayOptions,
}

/// 当前 host 文档 auth_type 的显式编辑状态；密码缺失时仍需保留用户选择的认证模式。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HostAuthenticationMode {
    #[default]
    Agent,
    Password,
    PrivateKey,
}

/// 一次凭据保存对应的字段位置；scope 每次更新都会唯一化以保证 host CAS 失败不覆盖旧密钥。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HostCredentialSlot {
    TargetPassword,
    TargetPrivateKey,
    TargetKeyPassphrase,
    RdpPassword,
    ProxyPassword,
    JumpPassword(usize),
    JumpPrivateKey(usize),
    JumpKeyPassphrase(usize),
}

/// 明确表达替换/清除；未列出的槽位沿用草稿中已有的 vault 引用。
pub enum HostCredentialChange {
    Replace {
        slot: HostCredentialSlot,
        secret: SecretValue,
    },
    Clear {
        slot: HostCredentialSlot,
    },
}

/// 组合保存回执：新 refs 与 host 快照来自同一已确认提交，清理失败会显式保留状态。
pub struct HostSaveReceipt {
    pub host: ManagedHost,
    pub credentials: Vec<HostCredentialReceipt>,
    pub cleanup: CredentialCleanupStatus,
}

/// Host CAS 结果未知时供显式读回恢复使用；引用仅在进程内保留且不进入格式化输出。
///
/// @author kongweiguang
#[derive(Clone, Eq, PartialEq)]
pub struct HostSaveRecoveryToken {
    host_id: HostId,
    baseline_revision: Option<ConfigRevision>,
    created: Vec<HostCredentialReceipt>,
    old_references: Vec<CredentialRef>,
}

impl std::fmt::Debug for HostSaveRecoveryToken {
    /// 仅显示恢复定位与候选数量，避免 opaque vault 引用意外进入日志或诊断。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostSaveRecoveryToken")
            .field("host_id", &self.host_id)
            .field("baseline_revision", &self.baseline_revision)
            .field("created_count", &self.created.len())
            .field("old_reference_count", &self.old_references.len())
            .finish()
    }
}

impl HostSaveRecoveryToken {
    /// 返回恢复所属主机，供 UI 将令牌绑定到同一 HostId 的定向读回。
    ///
    /// @author kongweiguang
    pub fn host_id(&self) -> &HostId {
        &self.host_id
    }

    /// 返回本次 CAS 的基线 revision，供 UI 区分未提交、已提交和第三种内容。
    ///
    /// @author kongweiguang
    pub fn baseline_revision(&self) -> Option<&ConfigRevision> {
        self.baseline_revision.as_ref()
    }
}

/// 保留 host 写入结果未知的区别，并将仅含 opaque refs 的恢复令牌交给上层状态机。
///
/// @author kongweiguang
#[must_use = "host 写入结果未知时，调用方需保留恢复令牌并完成读回核对"]
pub enum RecoverableHostSaveOutcome {
    Saved(Box<HostSaveReceipt>),
    OutcomeUnknown {
        error: AppError,
        recovery: HostSaveRecoveryToken,
    },
}

/// 返回给调用方的已密封引用；它只在上层需要写 host metadata 时短暂暴露。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostCredentialReceipt {
    pub slot: HostCredentialSlot,
    pub reference: CredentialRef,
}

/// 删除候选 ref 失败或无法证明无引用时保留条目，并报告需重试的数量。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialCleanupStatus {
    Complete,
    Deferred { remaining: usize },
}

/// 汇总无法清理的候选，避免格式错误的引用被已有清理结果掩盖。
///
/// @author kongweiguang
fn add_deferred(status: CredentialCleanupStatus, additional: usize) -> CredentialCleanupStatus {
    let remaining = match status {
        CredentialCleanupStatus::Complete => additional,
        CredentialCleanupStatus::Deferred { remaining } => remaining.saturating_add(additional),
    };
    if remaining == 0 {
        CredentialCleanupStatus::Complete
    } else {
        CredentialCleanupStatus::Deferred { remaining }
    }
}

/// 已引用候选的处理取决于调用方是否还需保留恢复材料。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReferencedCredentialBehavior {
    Preserve,
    Defer,
}

/// 同一存储事务内判断主机引用并清理凭据，避免扫描后并发发布新引用。
///
/// @author kongweiguang
#[async_trait]
pub trait HostCredentialCleanup: Send + Sync + 'static {
    /// 未能证明全部引用或删除结果未知时返回待清理状态，不能报告伪成功。
    ///
    /// @author kongweiguang
    async fn cleanup(
        &self,
        candidates: &[CredentialRef],
        referenced: ReferencedCredentialBehavior,
    ) -> AppResult<CredentialCleanupStatus>;
}

/// 主机分组摘要；revision 属于 groups.toml 整份文件，而不是单个分组。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostGroup {
    pub id: String,
    pub name: String,
    pub sort_order: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// 当前分组集合及其 CAS revision；缺失文件代表用户尚未创建自定义分组。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HostGroupSnapshot {
    pub groups: Vec<HostGroup>,
    pub revision: Option<ConfigRevision>,
}

/// 新建或编辑分组时传入的最小字段。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostGroupDraft {
    pub id: Option<String>,
    pub name: String,
    pub sort_order: i64,
}

/// 分组删除前读取的主机 revision；事务用完整集合发现并发新增、删除和编辑。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostDocumentRevision {
    pub host_id: HostId,
    pub revision: ConfigRevision,
}

/// 分组删除的确认回执；迁移计数来自原子提交的完整 host 快照。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostGroupDeletionReceipt {
    pub groups: HostGroupSnapshot,
    pub reassigned_hosts: usize,
}

/// Host 文件已确认删除后的回执；vault 清理失败不回滚主机删除。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostDeletionReceipt {
    pub credential_cleanup: CredentialCleanupStatus,
}

/// 只有仓储确认全部主机类型的引用均可解释时，调用方才能清理候选凭据。
///
/// @author kongweiguang
pub enum HostCredentialReferenceCoverage {
    Complete {
        remote_hosts: Vec<ManagedHost>,
        additional_references: Vec<CredentialRef>,
    },
    Unproven,
}

impl HostCredentialReferenceCoverage {
    /// 保留引用类型边界，仅在完整覆盖时回答候选是否仍被任一主机使用。
    ///
    /// @author kongweiguang
    pub fn contains_reference(&self, reference: &CredentialRef) -> Option<bool> {
        match self {
            Self::Complete {
                remote_hosts,
                additional_references,
            } => Some(
                additional_references.contains(reference)
                    || remote_hosts
                        .iter()
                        .any(|host| collect_host_references(host).contains(reference.as_str())),
            ),
            Self::Unproven => None,
        }
    }

    /// 统一合并内置路由、本地环境和插件引用，避免清理路径遗漏某类主机。
    ///
    /// @author kongweiguang
    fn into_reference_set(self) -> Option<std::collections::HashSet<String>> {
        match self {
            Self::Complete {
                remote_hosts,
                additional_references,
            } => {
                let mut references = remote_hosts
                    .iter()
                    .flat_map(collect_host_references)
                    .collect::<std::collections::HashSet<_>>();
                references.extend(
                    additional_references
                        .into_iter()
                        .map(|reference| reference.as_str().to_owned()),
                );
                Some(references)
            }
            Self::Unproven => None,
        }
    }
}

/// 连接配置仓储端口；实现层负责 schema v3 校验、未知字段保留与原子 CAS。
#[async_trait]
pub trait HostManagementRepository: Send + Sync + 'static {
    /// 在副作用前查询主机配置专用写门禁；最终 CAS 仍必须再次由适配器原子校验。
    async fn ensure_host_writable(&self) -> AppResult<()>;

    /// 列出受支持的 schema v3 主机；任一损坏主机必须显式失败，不能静默漏项。
    async fn list_hosts(&self) -> AppResult<Vec<ManagedHost>>;

    /// 按稳定主机 ID 读取配置；缺失以 None 表达，不创建默认主机。
    async fn load_host(&self, host_id: &HostId) -> AppResult<Option<ManagedHost>>;

    /// 独立投影扩展主机，缺少提供方的记录仍需进入目录而不能伪装成内置协议。
    async fn list_plugin_hosts(&self) -> AppResult<Vec<PluginHost>> {
        Ok(Vec::new())
    }

    /// 默认实现仅在不存在插件主机时证明覆盖；支持插件的仓储须覆盖并解释所有类型。
    ///
    /// @author kongweiguang
    async fn credential_reference_coverage(&self) -> AppResult<HostCredentialReferenceCoverage> {
        if !self.list_plugin_hosts().await?.is_empty() {
            return Ok(HostCredentialReferenceCoverage::Unproven);
        }
        let remote_hosts = self.list_hosts().await?;
        let additional_references = self
            .list_local_hosts()
            .await?
            .into_iter()
            .filter_map(|host| host.environment_ref)
            .collect();
        Ok(HostCredentialReferenceCoverage::Complete {
            remote_hosts,
            additional_references,
        })
    }

    /// 按稳定身份重读扩展主机，避免打开动作使用过期草稿。
    async fn load_plugin_host(&self, _host_id: &HostId) -> AppResult<Option<PluginHost>> {
        Ok(None)
    }

    /// 仓储负责 codec、原始 TOML 保留与最终 revision CAS。
    async fn save_plugin_host(
        &self,
        _host_id: HostId,
        _draft: PluginHostDraft,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<PluginHost> {
        Err(AppError::unsupported("插件主机保存不可用"))
    }

    /// 本地变体与远程主机共用目录；旧测试仓储未实现时保持无本地主机。
    ///
    /// @author kongweiguang
    async fn list_local_hosts(&self) -> AppResult<Vec<ManagedLocalHost>> {
        Ok(Vec::new())
    }

    /// 不把本地主机解释成远程 endpoint，缺失以 None 表示。
    ///
    /// @author kongweiguang
    async fn load_local_host(&self, _host_id: &HostId) -> AppResult<Option<ManagedLocalHost>> {
        Ok(None)
    }

    /// 仓储只持有已密封环境引用，不能从草稿把环境明文写入 host TOML。
    ///
    /// @author kongweiguang
    async fn save_local_host(
        &self,
        _host_id: HostId,
        _draft: LocalHostDraft,
        _environment_ref: Option<CredentialRef>,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ManagedLocalHost> {
        Err(AppError::unsupported("本地主机保存不可用"))
    }

    /// 删除使用同一 host CAS，调用方在结果确认后才清理环境密文。
    ///
    /// @author kongweiguang
    async fn delete_local_host(
        &self,
        _host_id: &HostId,
        _expected_revision: &ConfigRevision,
    ) -> AppResult<ManagedLocalHost> {
        Err(AppError::unsupported("本地主机删除不可用"))
    }

    /// 租约覆盖整个异步操作，真实仓储用独立 OS 锁阻止其他进程抢先恢复。
    ///
    /// @author kongweiguang
    async fn acquire_local_host_operation(&self) -> AppResult<Box<dyn Send>> {
        Ok(Box::new(()))
    }

    /// 保存密文前登记恢复意图；真实仓储必须持久化，测试替身可保持内存行为。
    ///
    /// @author kongweiguang
    async fn begin_local_host_recovery(
        &self,
        _token: &LocalHostSaveRecoveryToken,
    ) -> AppResult<()> {
        Ok(())
    }

    /// 读取上次未结束的本地主机意图，缺失表示没有待恢复操作。
    ///
    /// @author kongweiguang
    async fn pending_local_host_recovery(&self) -> AppResult<Option<LocalHostSaveRecoveryToken>> {
        Ok(None)
    }

    /// 删除前验证操作身份；重复清理缺失日志按幂等成功处理。
    ///
    /// @author kongweiguang
    async fn finish_local_host_recovery(
        &self,
        _token: &LocalHostSaveRecoveryToken,
    ) -> AppResult<()> {
        Ok(())
    }

    /// 按已读取 revision 保存主机；创建只允许 revision 缺失的文件路径。
    async fn save_host(
        &self,
        host_id: HostId,
        draft: HostDraft,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ManagedHost>;

    /// 仅修改既有主机的分组归属；不经过草稿、凭据或本地环境明文。
    ///
    /// @author kongweiguang
    async fn move_host_to_group(
        &self,
        _host_id: HostId,
        _group_id: Option<String>,
        _expected_revision: ConfigRevision,
    ) -> AppResult<()> {
        Err(AppError::unsupported("主机分组移动不可用"))
    }

    /// 以已确认 revision 删除单份 host 文件；未知提交结果必须作为 OutcomeUnknown 返回。
    async fn delete_host(
        &self,
        host_id: &HostId,
        expected_revision: &ConfigRevision,
    ) -> AppResult<ManagedHost>;

    /// 读取 groups.toml；缺失文档返回空集合和 None revision。
    async fn load_groups(&self) -> AppResult<HostGroupSnapshot>;

    /// 创建或编辑一个分组，以整份文件 revision 保护并发修改。
    async fn save_group(
        &self,
        draft: HostGroupDraft,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<HostGroupSnapshot>;

    /// 在一个 host-group journal 事务中迁移引用并删除组，防止崩溃后留下悬空 group_id。
    async fn delete_group_and_reassign(
        &self,
        group_id: String,
        destination_group_id: Option<String>,
        expected_group_revision: ConfigRevision,
        expected_hosts: Vec<HostDocumentRevision>,
    ) -> AppResult<HostGroupDeletionReceipt>;
}

/// UI 共用的 host/group 管理用例；数据访问和 TOML 语法均留在 infrastructure。
#[derive(Clone)]
pub struct HostManagementService {
    repository: Arc<dyn HostManagementRepository>,
    credentials: Option<CredentialService>,
    credential_cleanup: Option<Arc<dyn HostCredentialCleanup>>,
    local_environment_resolver: Option<Arc<dyn CredentialResolver>>,
    local_host_gate: Arc<tokio::sync::Mutex<()>>,
}

impl HostManagementService {
    /// 只读用例不默认装配凭据写入，避免测试或只读场景触碰 Vault。
    ///
    /// @author kongweiguang
    pub fn new(repository: Arc<dyn HostManagementRepository>) -> Self {
        Self {
            repository,
            credentials: None,
            credential_cleanup: None,
            local_environment_resolver: None,
            local_host_gate: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// 默认保留可注入 Vault 的测试路径；生产组合根另行注入原子清理端口。
    ///
    /// @author kongweiguang
    pub fn with_credentials(
        repository: Arc<dyn HostManagementRepository>,
        credentials: CredentialService,
    ) -> Self {
        Self {
            repository,
            credentials: Some(credentials),
            credential_cleanup: None,
            local_environment_resolver: None,
            local_host_gate: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// 生产仓储和 Vault 必须共享同一锁域，避免配置引用与密文删除交错。
    ///
    /// @author kongweiguang
    pub fn with_credential_cleanup(mut self, cleanup: Arc<dyn HostCredentialCleanup>) -> Self {
        self.credential_cleanup = Some(cleanup);
        self
    }

    /// 本地环境仅在编辑或启动时解密，目录与普通远程连接仍无需读取秘密。
    ///
    /// @author kongweiguang
    pub fn with_local_environment_resolver(
        mut self,
        resolver: Arc<dyn CredentialResolver>,
    ) -> Self {
        self.local_environment_resolver = Some(resolver);
        self
    }

    /// 返回主机目录的稳定排序视图；目录损坏会回到调用方显示错误，而非部分成功。
    pub async fn list_hosts(&self) -> AppResult<Vec<ManagedHost>> {
        let mut hosts = self.repository.list_hosts().await?;
        hosts.sort_by(|left, right| {
            left.sort_order
                .cmp(&right.sort_order)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.id.as_str().cmp(right.id.as_str()))
        });
        Ok(hosts)
    }

    /// 在编辑前按稳定 ID 刷新单条主机，供外部文件变更后的安全重载使用。
    pub async fn load_host(&self, host_id: &HostId) -> AppResult<Option<ManagedHost>> {
        self.repository.load_host(host_id).await
    }

    /// 扩展主机按现有目录排序语义返回，包括保留原文的不可用提供方。
    pub async fn list_plugin_hosts(&self) -> AppResult<Vec<PluginHost>> {
        let mut hosts = self.repository.list_plugin_hosts().await?;
        hosts.sort_by(|left, right| {
            left.sort_order
                .cmp(&right.sort_order)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.id.as_str().cmp(right.id.as_str()))
        });
        Ok(hosts)
    }

    /// 执行前重读同一主机，确保外部配置变更不会被旧视图覆盖。
    pub async fn load_plugin_host(&self, host_id: &HostId) -> AppResult<Option<PluginHost>> {
        self.repository.load_plugin_host(host_id).await
    }

    /// 公共字段和分组先在用例层核对，配置 codec 与最终 CAS 由仓储负责。
    pub async fn save_plugin_host(
        &self,
        mut draft: PluginHostDraft,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<PluginHost> {
        if !valid_host_type_id(&draft.provider_id)
            || !valid_host_type_id(&draft.type_id)
            || draft.provider_id == "kerminal"
            || draft.configuration_version == 0
            || draft.name.trim().is_empty()
            || draft.name.chars().any(char::is_control)
        {
            return Err(AppError::invalid("插件主机字段无效"));
        }
        draft.name = draft.name.trim().to_owned();
        let host_id = draft.id.take().unwrap_or_default();
        if ConfigDocumentPath::Host(host_id.as_str().to_owned())
            .relative_path()
            .is_err()
        {
            return Err(AppError::invalid("主机标识不能用于配置文件名"));
        }
        if let Some(group_id) = draft.group_id.as_deref() {
            let groups = self.repository.load_groups().await?;
            if !groups.groups.iter().any(|group| group.id == group_id) {
                return Err(AppError::not_found("主机所属分组不存在"));
            }
        }
        self.repository.ensure_host_writable().await?;
        self.repository
            .save_plugin_host(host_id, draft, expected_revision)
            .await
    }

    /// 安全复制只重用非秘密连接与显示选项，同时复制有效主机默认值以免意外改变操作习惯。
    ///
    /// @author kongweiguang
    pub async fn draft_for_copy(&self, source_id: &HostId, name: String) -> AppResult<HostDraft> {
        let source = self
            .repository
            .load_host(source_id)
            .await?
            .ok_or_else(|| AppError::not_found("待复制的主机不存在"))?;
        let mut route = source.route;
        if let Some(proxy) = route.proxy.as_mut() {
            proxy.password = None;
        }
        for jump in &mut route.jump_hosts {
            jump.authentication = authentication_without_credentials(jump.authentication.take());
        }
        Ok(HostDraft {
            id: None,
            group_id: source.group_id,
            name,
            note: source.note,
            protocol: source.protocol,
            endpoint: source.endpoint,
            authentication: authentication_without_credentials(source.authentication),
            authentication_mode: source.authentication_mode,
            rdp_password: None,
            route,
            serial: source.serial,
            tags: source.tags,
            sort_order: source.sort_order,
            connect_timeout_ms: source.connect_timeout_ms,
            keepalive_interval_ms: source.keepalive_interval_ms,
            terminal_options: source.terminal_options,
            sftp_options: source.sftp_options,
            rdp_display: source.rdp_display,
        })
    }

    /// 只有 host 文件删除结果已确认后才检查并清理引用；未知结果绝不触碰 vault。
    pub async fn delete_host(
        &self,
        host_id: &HostId,
        expected_revision: &ConfigRevision,
    ) -> AppResult<HostDeletionReceipt> {
        self.repository.ensure_host_writable().await?;
        let deleted = self
            .repository
            .delete_host(host_id, expected_revision)
            .await?;
        let references = collect_host_references(&deleted);
        let cleanup = if references.is_empty() {
            CredentialCleanupStatus::Complete
        } else if let Some(credentials) = self.credentials.as_ref() {
            self.cleanup_old(credentials, &references, &std::collections::HashSet::new())
                .await
        } else {
            CredentialCleanupStatus::Deferred {
                remaining: references.len(),
            }
        };
        Ok(HostDeletionReceipt {
            credential_cleanup: cleanup,
        })
    }

    /// 先归一化备注与协议能力，再校验并执行 host CAS，保持 SFTP-only 配置与运行语义一致。
    ///
    /// @author kongweiguang
    pub async fn save_host(
        &self,
        mut draft: HostDraft,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ManagedHost> {
        draft.note = normalize_host_note(draft.note);
        normalize_protocol_host_options(&mut draft);
        validate_host_draft(&draft)?;
        // 新建表单允许先分配稳定 ID，以便先把秘密写入以该 ID 派生的 vault 引用。
        let host_id = draft.id.take().unwrap_or_default();
        if kerminal_domain::preferences::ConfigDocumentPath::Host(host_id.as_str().to_owned())
            .relative_path()
            .is_err()
        {
            return Err(AppError::invalid("主机标识不能用于配置文件名"));
        }

        if let Some(group_id) = draft.group_id.as_deref() {
            let groups = self.repository.load_groups().await?;
            if !groups.groups.iter().any(|group| group.id == group_id) {
                return Err(AppError::not_found("主机所属分组不存在"));
            }
        }
        validate_proxy_credential_association(&draft)?;

        self.repository
            .save_host(host_id, draft, expected_revision)
            .await
    }

    /// 只把已存在的远端或本地主机移到指定分组；None 表示未分组，最终 CAS 由仓储保证。
    ///
    /// @author kongweiguang
    pub async fn move_host_to_group(
        &self,
        host_id: HostId,
        group_id: Option<String>,
        expected_revision: ConfigRevision,
    ) -> AppResult<()> {
        self.repository
            .move_host_to_group(host_id, group_id, expected_revision)
            .await
    }

    /// 兼容旧调用方的错误返回；未知结果仍以 OutcomeUnknown 返回，但旧接口不提供恢复令牌。
    ///
    /// @author kongweiguang
    pub async fn save_host_with_credentials(
        &self,
        draft: HostDraft,
        expected_revision: Option<ConfigRevision>,
        changes: Vec<HostCredentialChange>,
        cancel: CancellationToken,
    ) -> AppResult<HostSaveReceipt> {
        match self
            .save_host_with_credentials_recoverable(draft, expected_revision, changes, cancel)
            .await?
        {
            RecoverableHostSaveOutcome::Saved(receipt) => Ok(*receipt),
            RecoverableHostSaveOutcome::OutcomeUnknown { error, .. } => Err(error),
        }
    }

    /// 归一化备注与协议能力后复核写门禁并执行凭据 CAS；未知结果保留显式读回材料。
    ///
    /// @author kongweiguang
    pub async fn save_host_with_credentials_recoverable(
        &self,
        draft: HostDraft,
        expected_revision: Option<ConfigRevision>,
        changes: Vec<HostCredentialChange>,
        cancel: CancellationToken,
    ) -> AppResult<RecoverableHostSaveOutcome> {
        let credentials = self
            .credentials
            .as_ref()
            .ok_or_else(|| AppError::unsupported("连接凭据保存不可用"))?;
        let mut draft = draft;
        let host_id = draft.id.clone().unwrap_or_default();
        draft.id = Some(host_id.clone());
        draft.note = normalize_host_note(draft.note);
        normalize_protocol_host_options(&mut draft);
        validate_host_draft(&draft)?;
        if kerminal_domain::preferences::ConfigDocumentPath::Host(host_id.as_str().to_owned())
            .relative_path()
            .is_err()
        {
            return Err(AppError::invalid("主机标识不能用于配置文件名"));
        }
        if let Some(group_id) = draft.group_id.as_deref() {
            let groups = self.repository.load_groups().await?;
            if !groups.groups.iter().any(|group| group.id == group_id) {
                return Err(AppError::not_found("主机所属分组不存在"));
            }
        }
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("主机保存已取消"));
        }

        let existing = self.repository.load_host(&host_id).await?;
        match (expected_revision.as_ref(), existing.as_ref()) {
            (None, None) => {}
            (Some(expected), Some(current)) if expected == &current.revision => {}
            _ => return Err(AppError::conflict("主机已发生变化，请重新读取后再保存")),
        }
        let old_references = existing
            .as_ref()
            .map(collect_host_references)
            .unwrap_or_default();
        let old_reference_candidates = old_references
            .iter()
            .cloned()
            .map(CredentialRef::new)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| AppError::invalid("主机现有凭据引用格式无效"))?;
        let mut seen = std::collections::HashSet::new();
        if changes.iter().any(|change| !seen.insert(change.slot())) {
            return Err(AppError::invalid("同一凭据字段不能在一次保存中重复修改"));
        }
        if changes.iter().any(|change| {
            matches!(
                change,
                HostCredentialChange::Replace {
                    slot: HostCredentialSlot::ProxyPassword,
                    ..
                }
            ) && draft
                .route
                .proxy
                .as_ref()
                .is_some_and(|proxy| proxy.endpoint.username.trim().is_empty())
        }) {
            return Err(AppError::invalid("代理密码必须绑定有效的代理用户名"));
        }
        let plans = changes
            .iter()
            .map(|change| credential_components(&draft, change.slot()))
            .collect::<AppResult<Vec<_>>>()?;

        // 先拒绝已知只读恢复态，避免无谓密封凭据；随后保存仍由 host CAS 最终裁决。
        self.repository.ensure_host_writable().await?;

        let baseline_revision = expected_revision.clone();
        let mut created = Vec::new();
        for (change, (kind, material, scope)) in changes.into_iter().zip(plans) {
            if cancel.is_cancelled() {
                self.remove_unpublished(credentials, &host_id, &created)
                    .await;
                return Err(AppError::cancelled("主机保存已取消"));
            }
            let slot = change.slot();
            match change {
                HostCredentialChange::Replace { secret, .. } => {
                    let scope = unique_scope(&scope)?;
                    let attempted_reference = match CredentialReference::new(
                        kind.clone(),
                        host_id.clone(),
                        scope.clone(),
                        material,
                    ) {
                        Ok(reference) => reference.reference().clone(),
                        Err(_) => {
                            self.remove_unpublished(credentials, &host_id, &created)
                                .await;
                            return Err(AppError::invalid("凭据引用参数无效"));
                        }
                    };
                    let reference = match credentials
                        .save(SaveCredentialRequest {
                            kind,
                            host_id: host_id.clone(),
                            scope,
                            material,
                            secret,
                        })
                        .await
                    {
                        Ok(reference) => reference,
                        Err(error) => {
                            if error.public_code() == crate::error::PublicErrorCode::OutcomeUnknown
                            {
                                created.push(HostCredentialReceipt {
                                    slot,
                                    reference: attempted_reference,
                                });
                                return Ok(RecoverableHostSaveOutcome::OutcomeUnknown {
                                    error,
                                    recovery: HostSaveRecoveryToken {
                                        host_id,
                                        baseline_revision,
                                        created,
                                        old_references: old_reference_candidates,
                                    },
                                });
                            }
                            self.remove_unpublished(credentials, &host_id, &created)
                                .await;
                            return Err(error);
                        }
                    };
                    if let Err(error) =
                        apply_credential_reference(&mut draft, slot, Some(reference.clone()))
                    {
                        self.remove_unpublished(credentials, &host_id, &created)
                            .await;
                        let current = [HostCredentialReceipt { slot, reference }];
                        self.remove_unpublished(credentials, &host_id, &current)
                            .await;
                        return Err(error);
                    }
                    created.push(HostCredentialReceipt { slot, reference });
                }
                HostCredentialChange::Clear { .. } => {
                    if let Err(error) = apply_credential_reference(&mut draft, slot, None) {
                        self.remove_unpublished(credentials, &host_id, &created)
                            .await;
                        return Err(error);
                    }
                }
            }
        }

        if cancel.is_cancelled() {
            self.remove_unpublished(credentials, &host_id, &created)
                .await;
            return Err(AppError::cancelled("主机保存已取消"));
        }
        if let Err(error) = validate_proxy_credential_association(&draft) {
            self.remove_unpublished(credentials, &host_id, &created)
                .await;
            return Err(error);
        }
        let saved = match self
            .repository
            .save_host(host_id.clone(), draft, expected_revision)
            .await
        {
            Ok(saved) => saved,
            Err(error) => {
                if error.public_code() == crate::error::PublicErrorCode::OutcomeUnknown {
                    return Ok(RecoverableHostSaveOutcome::OutcomeUnknown {
                        error,
                        recovery: HostSaveRecoveryToken {
                            host_id,
                            baseline_revision,
                            created,
                            old_references: old_reference_candidates,
                        },
                    });
                }
                self.remove_unpublished(credentials, &host_id, &created)
                    .await;
                return Err(error);
            }
        };
        let references = collect_host_references(&saved);
        let cleanup = self
            .cleanup_old(credentials, &old_references, &references)
            .await;
        Ok(RecoverableHostSaveOutcome::Saved(Box::new(
            HostSaveReceipt {
                host: saved,
                credentials: created,
                cleanup,
            },
        )))
    }

    /// 只在调用方完成同一 HostId 的显式读回后调用；本用例再读一次并扫描全量引用后清理候选。
    ///
    /// @author kongweiguang
    pub async fn recover_unknown_host_save(
        &self,
        recovery: &HostSaveRecoveryToken,
    ) -> AppResult<CredentialCleanupStatus> {
        let credentials = self
            .credentials
            .as_ref()
            .ok_or_else(|| AppError::unsupported("连接凭据保存不可用"))?;
        let _readback = self.repository.load_host(&recovery.host_id).await?;

        let mut candidates = recovery.old_references.clone();
        candidates.extend(
            recovery
                .created
                .iter()
                .map(|credential| credential.reference.clone()),
        );
        Ok(self
            .cleanup_unreferenced_references(credentials, &recovery.host_id, &candidates)
            .await)
    }

    /// 在已知未发布的失败路径中尽力清理本次 ref；未知结果必须改走保留令牌的显式恢复用例。
    ///
    /// @author kongweiguang
    async fn remove_unpublished(
        &self,
        credentials: &CredentialService,
        host_id: &HostId,
        created: &[HostCredentialReceipt],
    ) -> CredentialCleanupStatus {
        if created.is_empty() {
            return CredentialCleanupStatus::Complete;
        }
        let references = created
            .iter()
            .map(|credential| credential.reference.clone())
            .collect::<Vec<_>>();
        self.cleanup_unreferenced_references(credentials, host_id, &references)
            .await
    }

    /// 先全量读取 host 引用，再只删除本次主机创建或替换的候选；读取或删除不确定时报告 Deferred。
    ///
    /// @author kongweiguang
    async fn cleanup_unreferenced_references(
        &self,
        credentials: &CredentialService,
        host_id: &HostId,
        candidates: &[CredentialRef],
    ) -> CredentialCleanupStatus {
        let mut unique = std::collections::HashSet::new();
        let candidates = candidates
            .iter()
            .filter(|reference| unique.insert(reference.as_str().to_owned()))
            .cloned()
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return CredentialCleanupStatus::Complete;
        }
        let mut remaining = 0;
        let mut valid = Vec::new();
        for reference in candidates {
            let belongs_to_host = CredentialReference::parse(reference.clone())
                .is_ok_and(|parsed| parsed.host_id() == host_id);
            if !belongs_to_host {
                remaining += 1;
            } else {
                valid.push(reference);
            }
        }
        let cleanup = self
            .cleanup_credential_candidates(
                credentials,
                &valid,
                ReferencedCredentialBehavior::Preserve,
            )
            .await;
        add_deferred(cleanup, remaining)
    }

    /// CAS 已提交后确认所有主机引用均有覆盖，未知插件或读取失败时保留旧凭据。
    ///
    /// @author kongweiguang
    async fn cleanup_old(
        &self,
        credentials: &CredentialService,
        old: &std::collections::HashSet<String>,
        current: &std::collections::HashSet<String>,
    ) -> CredentialCleanupStatus {
        let obsolete = old.difference(current).cloned().collect::<Vec<_>>();
        if obsolete.is_empty() {
            return CredentialCleanupStatus::Complete;
        }
        let mut remaining = 0;
        let mut valid = Vec::new();
        for reference in obsolete {
            let Ok(reference) = CredentialRef::new(reference) else {
                remaining += 1;
                continue;
            };
            if CredentialReference::parse(reference.clone()).is_err() {
                remaining += 1;
            } else {
                valid.push(reference);
            }
        }
        let cleanup = self
            .cleanup_credential_candidates(credentials, &valid, ReferencedCredentialBehavior::Defer)
            .await;
        add_deferred(cleanup, remaining)
    }

    /// 生产使用原子端口；内存及故障注入仓储保留独立 Vault 路径以验证失败传播。
    ///
    /// @author kongweiguang
    async fn cleanup_credential_candidates(
        &self,
        credentials: &CredentialService,
        candidates: &[CredentialRef],
        behavior: ReferencedCredentialBehavior,
    ) -> CredentialCleanupStatus {
        if candidates.is_empty() {
            return CredentialCleanupStatus::Complete;
        }
        if let Some(cleanup) = self.credential_cleanup.as_ref() {
            return cleanup.cleanup(candidates, behavior).await.unwrap_or(
                CredentialCleanupStatus::Deferred {
                    remaining: candidates.len(),
                },
            );
        }
        let referenced = match self.repository.credential_reference_coverage().await {
            Ok(coverage) => coverage.into_reference_set(),
            Err(_) => None,
        };
        let Some(referenced) = referenced else {
            return CredentialCleanupStatus::Deferred {
                remaining: candidates.len(),
            };
        };
        let mut remaining = 0;
        for reference in candidates {
            if referenced.contains(reference.as_str()) {
                if behavior == ReferencedCredentialBehavior::Defer {
                    remaining += 1;
                }
            } else if credentials.remove(reference).await.is_err() {
                remaining += 1;
            }
        }
        add_deferred(CredentialCleanupStatus::Complete, remaining)
    }

    /// 返回分组与整文件 revision，供分组弹窗按 CAS 保存。
    pub async fn load_groups(&self) -> AppResult<HostGroupSnapshot> {
        self.repository.load_groups().await
    }

    /// 创建或重命名一个分组；删除由原子迁移用例单独处理。
    pub async fn save_group(
        &self,
        mut draft: HostGroupDraft,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<HostGroupSnapshot> {
        let name = draft.name.trim();
        if name.is_empty() || name.chars().any(char::is_control) || name.chars().count() > 64 {
            return Err(AppError::invalid("分组名称必须为 1 到 64 个可见字符"));
        }
        draft.name = name.to_owned();
        if let Some(id) = draft.id.as_deref() {
            if !is_safe_group_id(id) {
                return Err(AppError::invalid("分组标识无效"));
            }
        } else {
            draft.id = Some(Uuid::new_v4().to_string());
        }
        self.repository.ensure_host_writable().await?;
        self.repository.save_group(draft, expected_revision).await
    }

    /// 固定本地和远程主机的完整 revision 集合，避免分组事务漏掉并发编辑。
    ///
    /// @author kongweiguang
    pub async fn delete_group(
        &self,
        group_id: String,
        destination_group_id: Option<String>,
        expected_group_revision: ConfigRevision,
    ) -> AppResult<HostGroupDeletionReceipt> {
        if !is_safe_group_id(&group_id) {
            return Err(AppError::invalid("分组标识无效"));
        }
        if destination_group_id
            .as_deref()
            .is_some_and(|destination| !is_safe_group_id(destination))
        {
            return Err(AppError::invalid("目标分组标识无效"));
        }
        if destination_group_id.as_deref() == Some(group_id.as_str()) {
            return Err(AppError::invalid("删除分组不能将主机迁回同一分组"));
        }

        self.repository.ensure_host_writable().await?;
        let groups = self.repository.load_groups().await?;
        if groups.revision.as_ref() != Some(&expected_group_revision) {
            return Err(AppError::conflict("主机分组已变化，请刷新后再删除"));
        }
        if !groups.groups.iter().any(|group| group.id == group_id) {
            return Err(AppError::not_found("主机分组不存在"));
        }
        if destination_group_id
            .as_ref()
            .is_some_and(|destination| !groups.groups.iter().any(|group| &group.id == destination))
        {
            return Err(AppError::not_found("主机迁移目标分组不存在"));
        }

        let mut expected_hosts = self
            .repository
            .list_hosts()
            .await?
            .into_iter()
            .map(|host| HostDocumentRevision {
                host_id: host.id,
                revision: host.revision,
            })
            .collect::<Vec<_>>();
        expected_hosts.extend(
            self.repository
                .list_local_hosts()
                .await?
                .into_iter()
                .map(|host| HostDocumentRevision {
                    host_id: host.id,
                    revision: host.revision,
                }),
        );
        expected_hosts.extend(
            self.repository
                .list_plugin_hosts()
                .await?
                .into_iter()
                .map(|host| HostDocumentRevision {
                    host_id: host.id,
                    revision: host.revision,
                }),
        );
        self.repository
            .delete_group_and_reassign(
                group_id,
                destination_group_id,
                expected_group_revision,
                expected_hosts,
            )
            .await
    }
}

/// 复制认证时保留用户选择的模式和密钥路径，但不泄漏任何源 host 的 vault 引用。
fn authentication_without_credentials(
    authentication: Option<SshAuthentication>,
) -> Option<SshAuthentication> {
    match authentication {
        Some(SshAuthentication::Password { .. }) => None,
        Some(SshAuthentication::PrivateKey { path, .. }) => Some(SshAuthentication::PrivateKey {
            path,
            secret: None,
            passphrase: None,
        }),
        Some(SshAuthentication::Agent) => Some(SshAuthentication::Agent),
        None => None,
    }
}

/// 隐藏选项保留有效历史值；失效草稿投影为规范默认，避免保存不可重读的配置。
/// SFTP-only 的旧 enabled=false 仅在显式保存时归一化，RDP 全屏尺寸不参与窗口校验。
/// @author kongweiguang
fn normalize_protocol_host_options(draft: &mut HostDraft) {
    if draft.protocol == RemoteProtocol::Sftp {
        draft.sftp_options.enabled = true;
    } else if draft.protocol == RemoteProtocol::Ssh
        && !draft.sftp_options.enabled
        && !sftp_options_are_valid(&draft.sftp_options)
    {
        draft.sftp_options = HostSftpOptions {
            enabled: false,
            ..HostSftpOptions::default()
        };
    }
    if draft.protocol == RemoteProtocol::Rdp
        && draft.rdp_display.fullscreen
        && draft.rdp_display.validate().is_err()
    {
        draft.rdp_display.width = None;
        draft.rdp_display.height = None;
    } else if draft.protocol != RemoteProtocol::Rdp && draft.rdp_display.validate().is_err() {
        draft.rdp_display = RdpDisplayOptions::default();
    }
    if draft.protocol != RemoteProtocol::Ssh && !terminal_options_are_valid(&draft.terminal_options)
    {
        draft.terminal_options = HostTerminalOptions::default();
    }
}

/// 只校验 SFTP 的持久字段形状；关闭 SSH 文件能力时可用它判断是否沿用旧值。
/// @author kongweiguang
fn sftp_options_are_valid(options: &HostSftpOptions) -> bool {
    options.local_start_directory.as_ref().is_none_or(|path| {
        path.to_str().is_some_and(|value| {
            !value.trim().is_empty() && !value.chars().any(char::is_control) && path.is_absolute()
        })
    }) && options
        .remote_start_directory
        .as_deref()
        .is_none_or(valid_remote_start_directory)
        && options
            .concurrency
            .is_none_or(|concurrency| (1..=8).contains(&concurrency))
}

/// SSH 专属终端值只在 SSH 主机启用，其他协议无效的隐藏值投影为空配置。
/// @author kongweiguang
fn terminal_options_are_valid(options: &HostTerminalOptions) -> bool {
    options.term.as_ref().is_none_or(|term| {
        !term.trim().is_empty()
            && term.trim() == term
            && term.len() <= 128
            && !term
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
    }) && options
        .initial_working_directory
        .as_ref()
        .is_none_or(|directory| {
            !directory.trim().is_empty() && !directory.chars().any(char::is_control)
        })
}

/// 校验连接与协议专属高级选项，避免保存后由不同平台以不一致的默认值解释配置。
///
/// @author kongweiguang
fn validate_host_draft(draft: &HostDraft) -> AppResult<()> {
    if draft.name.trim().is_empty()
        || draft.name.chars().any(char::is_control)
        || draft.name.chars().count() > 128
    {
        return Err(AppError::invalid("主机名称必须为 1 到 128 个可见字符"));
    }
    if draft
        .note
        .as_deref()
        .is_some_and(|note| !host_note_is_valid(note))
    {
        return Err(AppError::invalid(
            "主机备注不能超过 4096 个字符或包含不支持的控制字符",
        ));
    }
    if draft.endpoint.host.trim().is_empty()
        || draft.endpoint.host.chars().any(char::is_control)
        || (draft.endpoint.port == 0 && draft.protocol != RemoteProtocol::Serial)
    {
        return Err(AppError::invalid("主机地址不能为空，网络协议需要非零端口"));
    }
    match draft.protocol {
        RemoteProtocol::Ssh | RemoteProtocol::Sftp
            if draft.endpoint.username.trim().is_empty()
                || draft.endpoint.username.chars().any(char::is_control) =>
        {
            return Err(AppError::invalid("此协议需要有效的用户名"));
        }
        RemoteProtocol::Rdp if draft.endpoint.username.chars().any(char::is_control) => {
            return Err(AppError::invalid("RDP 用户名不能包含控制字符"));
        }
        RemoteProtocol::Serial if draft.serial.is_none() => {
            return Err(AppError::invalid("串口连接必须提供完整串口参数"));
        }
        RemoteProtocol::Serial
            if draft.serial.as_ref().is_some_and(|serial| {
                serial.device.to_str().is_none_or(|device| {
                    device.trim() != device || device != draft.endpoint.host.as_str()
                }) || draft.endpoint.port != 0
                    || !draft.endpoint.username.is_empty()
            }) =>
        {
            return Err(AppError::invalid(
                "串口 endpoint 必须由设备路径投影，且不包含网络端口或用户名",
            ));
        }
        RemoteProtocol::Serial
            if draft
                .serial
                .as_ref()
                .is_some_and(|serial| serial.baud_rate == 0) =>
        {
            return Err(AppError::invalid("串口波特率必须大于零"));
        }
        RemoteProtocol::Ssh | RemoteProtocol::Sftp => {
            match (draft.authentication_mode, draft.authentication.as_ref()) {
                (HostAuthenticationMode::Agent, None | Some(SshAuthentication::Agent))
                | (
                    HostAuthenticationMode::Password,
                    None | Some(SshAuthentication::Password { .. }),
                )
                | (
                    HostAuthenticationMode::PrivateKey,
                    None | Some(SshAuthentication::PrivateKey { .. }),
                ) => {}
                _ => return Err(AppError::invalid("主机认证模式与凭据字段不匹配")),
            }
        }
        RemoteProtocol::Rdp if draft.authentication.is_some() => {
            return Err(AppError::invalid("RDP 认证不能使用 SSH 凭据字段"));
        }
        RemoteProtocol::Telnet | RemoteProtocol::Serial
            if draft.authentication.is_some() || draft.rdp_password.is_some() =>
        {
            return Err(AppError::invalid("此协议不接受保存的连接凭据"));
        }
        _ => {}
    }
    if !matches!(draft.protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp)
        && (draft.route.proxy.is_some() || !draft.route.jump_hosts.is_empty())
    {
        return Err(AppError::invalid("只有 SSH 与 SFTP 支持代理或跳板路由"));
    }
    if !(1_000..=600_000).contains(&draft.connect_timeout_ms)
        || !draft.connect_timeout_ms.is_multiple_of(1000)
        || draft.keepalive_interval_ms.is_some_and(|interval| {
            !(1_000..=600_000).contains(&interval) || !interval.is_multiple_of(1000)
        })
    {
        return Err(AppError::invalid(
            "SSH 超时和心跳间隔必须在 1 到 600 秒之间",
        ));
    }
    if draft.protocol == RemoteProtocol::Ssh && !terminal_options_are_valid(&draft.terminal_options)
    {
        return Err(AppError::invalid("SSH 终端类型或默认目录无效"));
    }
    if HostSftpOptions::enabled_for(draft.protocol, draft.sftp_options.enabled)
        && !sftp_options_are_valid(&draft.sftp_options)
    {
        return Err(AppError::invalid("SFTP 起始目录或主机并发数无效"));
    }
    if draft.protocol == RemoteProtocol::Rdp && !draft.rdp_display.fullscreen {
        draft.rdp_display.validate()?;
    }
    if let Some(proxy) = &draft.route.proxy
        && (proxy.endpoint.host.trim().is_empty()
            || proxy.endpoint.port == 0
            || proxy.endpoint.host.chars().any(char::is_control)
            || proxy.endpoint.username.chars().any(char::is_control))
    {
        return Err(AppError::invalid("代理地址或凭据配置无效"));
    }
    if draft.route.jump_hosts.iter().any(|jump| {
        jump.name.chars().any(char::is_control)
            || jump.endpoint.host.trim().is_empty()
            || jump.endpoint.username.trim().is_empty()
            || jump.endpoint.port == 0
            || !matches!(
                (jump.authentication_mode, jump.authentication.as_ref()),
                (
                    HostAuthenticationMode::Agent,
                    None | Some(SshAuthentication::Agent)
                ) | (
                    HostAuthenticationMode::Password,
                    None | Some(SshAuthentication::Password { .. })
                ) | (
                    HostAuthenticationMode::PrivateKey,
                    None | Some(SshAuthentication::PrivateKey { .. })
                )
            )
    }) {
        return Err(AppError::invalid("跳板名称、认证模式或连接地址无效"));
    }
    if draft
        .tags
        .iter()
        .any(|tag| tag.chars().any(char::is_control))
    {
        return Err(AppError::invalid("主机标签不能包含控制字符"));
    }
    if draft
        .tags
        .iter()
        .any(|tag| is_reserved_serial_host_tag(tag))
    {
        return Err(AppError::invalid(
            "串口参数必须填写在串口设置中，不能作为主机标签",
        ));
    }
    Ok(())
}

/// 只接受绝对 POSIX 目录并拒绝父目录跳转，确保默认路径不会被解释为另一目标。
///
/// @author kongweiguang
pub(super) fn valid_remote_start_directory(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path.split('/').all(|segment| segment != "..")
}

/// 判断主机输入标签是否占用串口保留键，供用例和基础设施适配器共享同一规则。
///
/// @author kongweiguang
pub fn is_reserved_serial_host_tag(tag: &str) -> bool {
    let key = tag.split_once(':').map_or(tag, |(key, _)| key).trim();
    [
        "serial",
        "serial-port",
        "serial-baud",
        "serial-data-bits",
        "serial-stop-bits",
        "serial-parity",
        "serial-flow",
    ]
    .iter()
    .any(|reserved| key.eq_ignore_ascii_case(reserved))
}

/// 仅对最终提交状态要求已保存代理密码带用户名，允许先清除旧的无效引用。
fn validate_proxy_credential_association(draft: &HostDraft) -> AppResult<()> {
    if draft
        .route
        .proxy
        .as_ref()
        .is_some_and(|proxy| proxy.password.is_some() && proxy.endpoint.username.trim().is_empty())
    {
        return Err(AppError::invalid("代理密码必须绑定有效的代理用户名"));
    }
    Ok(())
}

/// 限制分组文件中的稳定 ID 字符集，避免 UI 创建后无法安全映射 TOML 结构。
fn is_safe_group_id(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

impl HostCredentialChange {
    /// 暴露本次变更的槽位，使组合用例在写入 vault 之前完成唯一性和协议校验。
    fn slot(&self) -> HostCredentialSlot {
        match self {
            Self::Replace { slot, .. } | Self::Clear { slot } => *slot,
        }
    }
}

/// 按协议、认证模式和路由位置解析兼容 ref 组件，代理密码使用独立 host 级 scope。
fn credential_components(
    draft: &HostDraft,
    slot: HostCredentialSlot,
) -> AppResult<(CredentialKind, CredentialMaterial, String)> {
    if slot == HostCredentialSlot::ProxyPassword {
        if !matches!(draft.protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp)
            || draft.route.proxy.is_none()
        {
            return Err(AppError::invalid("代理凭据字段与连接路由不匹配"));
        }
        return Ok((
            CredentialKind::parse("proxy-host").map_err(|_| AppError::invalid("凭据类型无效"))?,
            CredentialMaterial::Password,
            "proxy".to_owned(),
        ));
    }
    let (kind, material, base_scope, mode) = match slot {
        HostCredentialSlot::TargetPassword => (
            "ssh-host",
            CredentialMaterial::Password,
            "target".to_owned(),
            draft.authentication_mode,
        ),
        HostCredentialSlot::TargetPrivateKey => (
            "ssh-host",
            CredentialMaterial::PrivateKey,
            "target".to_owned(),
            draft.authentication_mode,
        ),
        HostCredentialSlot::TargetKeyPassphrase => (
            "ssh-host",
            CredentialMaterial::KeyPassphrase,
            "target".to_owned(),
            draft.authentication_mode,
        ),
        HostCredentialSlot::RdpPassword => (
            "rdp-host",
            CredentialMaterial::Password,
            "target".to_owned(),
            HostAuthenticationMode::Password,
        ),
        HostCredentialSlot::ProxyPassword => unreachable!("handled before ordinary auth mapping"),
        HostCredentialSlot::JumpPassword(index) => {
            jump_credential_components(draft, index, CredentialMaterial::Password)?
        }
        HostCredentialSlot::JumpPrivateKey(index) => {
            jump_credential_components(draft, index, CredentialMaterial::PrivateKey)?
        }
        HostCredentialSlot::JumpKeyPassphrase(index) => {
            jump_credential_components(draft, index, CredentialMaterial::KeyPassphrase)?
        }
    };
    let valid_target = match slot {
        HostCredentialSlot::RdpPassword => draft.protocol == RemoteProtocol::Rdp,
        HostCredentialSlot::JumpPassword(_)
        | HostCredentialSlot::JumpPrivateKey(_)
        | HostCredentialSlot::JumpKeyPassphrase(_) => {
            matches!(draft.protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp)
        }
        _ => matches!(draft.protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp),
    };
    if !valid_target {
        return Err(AppError::invalid("凭据字段与连接协议不匹配"));
    }
    let mode_matches = match material {
        CredentialMaterial::Password => mode == HostAuthenticationMode::Password,
        CredentialMaterial::PrivateKey | CredentialMaterial::KeyPassphrase => {
            mode == HostAuthenticationMode::PrivateKey
        }
    };
    if !mode_matches {
        return Err(AppError::invalid("凭据字段与认证模式不匹配"));
    }
    let kind = CredentialKind::parse(kind).map_err(|_| AppError::invalid("凭据类型无效"))?;
    Ok((kind, material, base_scope))
}

/// 读取目标跳板的认证模式并绑定 jump-N scope，避免 UI 顺序调整时写错凭据。
fn jump_credential_components(
    draft: &HostDraft,
    index: usize,
    material: CredentialMaterial,
) -> AppResult<(
    &'static str,
    CredentialMaterial,
    String,
    HostAuthenticationMode,
)> {
    let jump = draft
        .route
        .jump_hosts
        .get(index)
        .ok_or_else(|| AppError::not_found("凭据所属跳板不存在"))?;
    Ok((
        "jump-host",
        material,
        format!("jump-{index}"),
        jump.authentication_mode,
    ))
}

/// 每次替换生成兼容格式的唯一 scope，避免 CAS 失败覆盖现有引用指向的密文。
fn unique_scope(base_scope: &str) -> AppResult<CredentialScope> {
    CredentialScope::parse(&format!("{base_scope}-{}", Uuid::new_v4().simple()))
        .map_err(|_| AppError::invalid("无法生成凭据引用作用域"))
}

/// 把新 ref 或清除操作应用到目标、代理或跳板草稿；未修改的 key path/passphrase 保持不变。
fn apply_credential_reference(
    draft: &mut HostDraft,
    slot: HostCredentialSlot,
    reference: Option<CredentialRef>,
) -> AppResult<()> {
    match slot {
        HostCredentialSlot::RdpPassword => draft.rdp_password = reference,
        HostCredentialSlot::ProxyPassword => {
            let proxy = draft
                .route
                .proxy
                .as_mut()
                .ok_or_else(|| AppError::invalid("代理凭据字段与连接路由不匹配"))?;
            proxy.password = reference;
        }
        HostCredentialSlot::TargetPassword => {
            draft.authentication = reference.map(|secret| SshAuthentication::Password { secret });
        }
        HostCredentialSlot::TargetPrivateKey | HostCredentialSlot::TargetKeyPassphrase => {
            let auth = draft.authentication.take();
            let (path, secret, passphrase) = match auth {
                Some(SshAuthentication::PrivateKey {
                    path,
                    secret,
                    passphrase,
                }) => (path, secret, passphrase),
                None => (None, None, None),
                _ => return Err(AppError::invalid("私钥凭据字段与认证模式不匹配")),
            };
            let (secret, passphrase) = match slot {
                HostCredentialSlot::TargetPrivateKey => (reference, passphrase),
                HostCredentialSlot::TargetKeyPassphrase => (secret, reference),
                _ => (secret, passphrase),
            };
            draft.authentication = Some(SshAuthentication::PrivateKey {
                path,
                secret,
                passphrase,
            });
        }
        HostCredentialSlot::JumpPassword(index)
        | HostCredentialSlot::JumpPrivateKey(index)
        | HostCredentialSlot::JumpKeyPassphrase(index) => {
            let jump = draft
                .route
                .jump_hosts
                .get_mut(index)
                .ok_or_else(|| AppError::not_found("凭据所属跳板不存在"))?;
            if slot == HostCredentialSlot::JumpPassword(index) {
                jump.authentication =
                    reference.map(|secret| SshAuthentication::Password { secret });
            } else {
                let auth = jump.authentication.take();
                let (path, secret, passphrase) = match auth {
                    Some(SshAuthentication::PrivateKey {
                        path,
                        secret,
                        passphrase,
                    }) => (path, secret, passphrase),
                    None => (None, None, None),
                    _ => return Err(AppError::invalid("跳板私钥凭据字段与认证模式不匹配")),
                };
                let (secret, passphrase) = match slot {
                    HostCredentialSlot::JumpPrivateKey(_) => (reference, passphrase),
                    HostCredentialSlot::JumpKeyPassphrase(_) => (secret, reference),
                    _ => (secret, passphrase),
                };
                jump.authentication = Some(SshAuthentication::PrivateKey {
                    path,
                    secret,
                    passphrase,
                });
            }
        }
    }
    Ok(())
}

/// 收集 host 与路由内的所有 vault ref，清理时使用完整目录扫描防止删去共享引用。
fn collect_host_references(host: &ManagedHost) -> std::collections::HashSet<String> {
    let mut references = std::collections::HashSet::new();
    add_authentication_references(&mut references, &host.authentication);
    references.extend(
        host.rdp_password
            .iter()
            .map(|reference| reference.as_str().to_owned()),
    );
    references.extend(host.route.proxy.iter().filter_map(|proxy| {
        proxy
            .password
            .as_ref()
            .map(|reference| reference.as_str().to_owned())
    }));
    for jump in &host.route.jump_hosts {
        add_authentication_references(&mut references, &jump.authentication);
    }
    references
}

/// 将单个认证值中的 opaque vault ref 追加到集合，避免闭包长期占用可变借用。
fn add_authentication_references(
    references: &mut std::collections::HashSet<String>,
    authentication: &Option<SshAuthentication>,
) {
    match authentication {
        Some(SshAuthentication::Password { secret }) => {
            references.insert(secret.as_str().to_owned());
        }
        Some(SshAuthentication::PrivateKey {
            secret, passphrase, ..
        }) => {
            references.extend(secret.iter().map(|reference| reference.as_str().to_owned()));
            references.extend(
                passphrase
                    .iter()
                    .map(|reference| reference.as_str().to_owned()),
            );
        }
        Some(SshAuthentication::Agent) | None => {}
    }
}
