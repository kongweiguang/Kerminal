//! 保留插件身份的只读冲突预览。
//!
//! 单入口是 [`preview_reserved_conflict`]：调用方只能提供权威 [`PluginStore`] 与
//! 期望 revision，状态由该 store 自己的根读出，因此不存在把任意 root 与任意
//! `PluginStoreState` 拼在一起的公开入口。入口先核对插件根自身：根不安全
//! （链接、重解析点或非目录）或不可读时返回专用类型化错误，不折叠成状态
//! 损坏，也不沿不安全根继续读取；状态加载失败、旧 schema、损坏文件都会返回
//! 类型化失败，绝不退化为空状态。
//!
//! 本模块只读：不创建、删除或移动任何配置目录，不取存储写锁（`StorageCoordinator::lock`
//! 会创建根目录并写锁文件），不读取 `plugins/data` 下的文件内容，也不执行清理。
//! revision、实际目录指纹和证据摘要只用于界面展示与陈旧检测，**不是执行授权**。
//! 真正清理前必须在存储锁内重新核对 revision、`state.plugins` 记录和目录实际指纹；
//! 外部进程不受该锁约束，指纹只能证明当前内容，不能证明内容来源。
//!
//! 未阻断只说明本预览没有发现该类证据，**不代表**数据迁移、布局启动门禁或整体
//! 清理已经安全；见 [`production_readiness`]。
//!
//! ## 已知未解决：读取无界与路径身份 TOCTOU
//!
//! [`layout::fingerprint_tree`] 每个文件在**真实读取**时限制 64 MiB、整棵树
//! 限制 10000 个文件条目，但没有整棵目录树的累计字节预算，恶意包可以拆成
//! 10000 个 64 MiB 文件让预览读取约 640 GB；目录条目枚举数量同样无界。
//! [`layout::scan_journals`] 枚举 `.migration` 时不限制 journal 数量，恢复
//! 扫描的条目上限只覆盖 `plugins/` 根一层。预先做一次 metadata 扫描既不能
//! 约束随后并发替换，也不能约束真实读取。`layout.rs` 不在本模块写入范围，
//! 因此这里不复制一份实现、也不以预检冒充已解决。落地方式应是复用布局模块
//! 的同一安全算法（no-follow 打开、单链接校验、逐文件限额），在**同一个
//! 读取流**上累计总字节并给目录条目设上限，超限即中止；这需要布局模块新增
//! 一个接受累计预算的内部入口，完成后才能接入生产路径。
//!
//! 另外，存储锁只约束同根的存储操作，不能阻止外部进程或用户在检查与使用
//! 之间替换目录；本模块按路径逐级 `symlink_metadata` 后再读取，存在路径
//! 身份 TOCTOU 窗口（`fingerprint_file` 对单个文件做了句柄级复核，但目录
//! 层级没有）。真正的清理执行必须在句柄级身份约束下（打开目录句柄后再
//! 相对寻址）核对 revision、`state.plugins` 记录和实际指纹，仅靠路径与
//! 存储锁不够。
//!
//! 状态恢复材料、未完成/不可解析 journal、`.staging`/`.backup` 同身份残留只做
//! 只读探测；无法可靠读取时输出类型化的“未核对/阻断”，绝不把未知风险表示为
//! 允许清理。本模块尚未被 `plugins/mod.rs` 声明，未接入生产路径。
//!
//! @author kongweiguang

use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs, io,
    path::{Component, Path, PathBuf},
};

use kerminal_application::plugins::{is_builtin_unit, is_reserved_plugin_id};
use kerminal_domain::plugins::{
    InstalledPlugin, PendingPluginChange, PluginFingerprint, PluginId, PluginInstallStatus,
    PluginSource, PluginStoreState, PluginUnsupportedReason, PluginVersion,
};
use sha2::{Digest, Sha256};

use super::{
    layout::{self, LayoutError, LayoutPhase},
    store::{PluginStore, PluginStoreError},
};

/// 兼容 provider 名不是合法 `PluginId`，只能作为原始精确保留名成为 `plugins/` 直接子级。
const RESERVED_PROVIDER_NAME: &str = "kerminal";

/// 扫描 `plugins/` 根目录时限制条目数；超过后不能断言“没有恢复材料”。
const MAX_RECOVERY_SCAN_ENTRIES: usize = 16 * 1024;

/// 预览入口错误只携带固定类别，避免把调用方路径或原始名字回显到界面。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReservedPreviewError {
    /// 存储根必须与 `PluginStore` 一样是绝对路径；相对路径会引入调用方当前目录。
    RootNotAbsolute,
    /// `plugins/` 根存在但不是安全目录（链接、重解析点或非目录）；
    /// 与状态损坏分开报告，否则界面会把目录安全问题说成状态文件问题。
    PluginRootUnsafe,
    /// 期望 revision 与权威状态不一致，说明界面快照已陈旧，必须重新加载。
    StaleRevision { expected: u64, actual: u64 },
    /// 状态文件不可读或 IO 失败；不能当成空状态继续。
    StateUnavailable,
    /// 状态文件损坏、字段非法或 schema 不受支持；同样不能丢弃材料后继续。
    StateInvalid,
    /// 状态提交结果未知或存在恢复材料，必须先由存储层显式处理。
    StateRecoveryRequired,
    /// 名字不在应用层保留身份集合内，可能是普通插件或任意用户输入。
    NotReservedIdentity,
    /// 名字属于保留集合但不是当前已知的内置单元或兼容 provider。
    UnsupportedReservedName,
    /// 内置单元无法形成合法 `PluginId`，不能安全地推导到直接子目录。
    ReservedNameNotPluginId,
    /// 派生路径不是单一普通路径段，拒绝任何目录穿越形态。
    UnsafeReservedName,
}

impl std::fmt::Display for ReservedPreviewError {
    /// 只输出固定说明，调用方原始名称和本机路径不进入错误文本。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RootNotAbsolute => formatter.write_str("插件存储根不是绝对路径"),
            Self::PluginRootUnsafe => formatter.write_str("插件根目录不安全，无法预览"),
            Self::StaleRevision { .. } => {
                formatter.write_str("插件状态 revision 已变化，请重新加载")
            }
            Self::StateUnavailable => formatter.write_str("插件状态暂时无法读取"),
            Self::StateInvalid => formatter.write_str("插件状态损坏或版本不受支持"),
            Self::StateRecoveryRequired => formatter.write_str("插件状态存在待处理的恢复材料"),
            Self::NotReservedIdentity => formatter.write_str("目标不是保留插件身份"),
            Self::UnsupportedReservedName => formatter.write_str("目标保留身份暂不支持预览"),
            Self::ReservedNameNotPluginId => formatter.write_str("保留身份无法形成插件 ID"),
            Self::UnsafeReservedName => formatter.write_str("保留身份不是安全目录名"),
        }
    }
}

impl std::error::Error for ReservedPreviewError {}

/// 把存储层错误折叠为固定类别；未知提交与恢复材料不能被当成“没有记录”。
///
/// `InvalidState` 在此被当作状态损坏：公开入口已先行核对插件根自身，根不安全
/// 会在读取状态前返回 [`ReservedPreviewError::PluginRootUnsafe`]，因此到达这里
/// 的 `InvalidState` 只应来自状态文件校验（两次检查之间的 TOCTOU 见模块文档，
/// 存储层未区分两种来源，本模块不引入第二套根检查语义去猜）。
fn map_store_error(error: PluginStoreError) -> ReservedPreviewError {
    match error {
        PluginStoreError::InvalidState | PluginStoreError::InvalidRecovery { .. } => {
            ReservedPreviewError::StateInvalid
        }
        PluginStoreError::Conflict
        | PluginStoreError::OutcomeUnknown { .. }
        | PluginStoreError::RecoveryRequired { .. }
        | PluginStoreError::RecoveryPreparationUnknown { .. }
        | PluginStoreError::RecoveryCleanupFailed { .. }
        | PluginStoreError::RecoveryResolutionUnknown { .. }
        | PluginStoreError::LayoutRecoveryRequired { .. }
        | PluginStoreError::LayoutOutcomeUnknown { .. }
        | PluginStoreError::Layout(_) => ReservedPreviewError::StateRecoveryRequired,
        PluginStoreError::Io(_) | PluginStoreError::Lock => ReservedPreviewError::StateUnavailable,
    }
}

/// 生产可用性必须显式可查，避免“预览通过”被误读为可以执行清理或启动迁移。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreviewReadiness {
    /// `layout::fingerprint_tree` 缺少整棵目录树的累计字节预算，见模块文档。
    pub fingerprint_total_budget_resolved: bool,
    /// 数据迁移与布局启动门禁的结论不由此预览提供。
    pub startup_gates_verified: bool,
    /// 预览快照不是执行授权；清理仍须在存储锁内复核 revision、记录与指纹。
    pub grants_authorization: bool,
}

/// 当前固定返回“未就绪”；只有指纹总量预算与门禁接线完成后才能改变。
pub fn production_readiness() -> PreviewReadiness {
    PreviewReadiness {
        fingerprint_total_budget_resolved: false,
        startup_gates_verified: false,
        grants_authorization: false,
    }
}

/// 经过应用层规则确认的保留身份；provider 不解析为 `PluginId`。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReservedPluginIdentity {
    /// 可同时出现在 `state.plugins` 和 `state.builtins` 的内置启停单元。
    BuiltinUnit { id: PluginId },
    /// 只能按原始精确保留名使用的兼容 provider，不是插件记录身份。
    BuiltinProvider,
}

impl ReservedPluginIdentity {
    /// 先使用应用层公开判定，再限定为已知形态；这样未来新增保留名不会被本模块
    /// 静默接受。provider 必须先于 `PluginId` 解析判断，否则 `kerminal` 会被误拒。
    pub fn parse(raw: &str) -> Result<Self, ReservedPreviewError> {
        if !is_reserved_plugin_id(raw) {
            return Err(ReservedPreviewError::NotReservedIdentity);
        }
        if raw == RESERVED_PROVIDER_NAME {
            return Ok(Self::BuiltinProvider);
        }
        if !is_builtin_unit(raw) {
            return Err(ReservedPreviewError::UnsupportedReservedName);
        }
        // 先解析再构造，避免把变体构造器包成多余闭包，也保留明确的失败映射。
        let id = PluginId::parse(raw).map_err(|_| ReservedPreviewError::ReservedNameNotPluginId)?;
        Ok(Self::BuiltinUnit { id })
    }

    /// 返回原始保留名，供界面显示和证据摘要使用。
    pub fn raw_name(&self) -> &str {
        match self {
            Self::BuiltinUnit { id } => id.as_str(),
            Self::BuiltinProvider => RESERVED_PROVIDER_NAME,
        }
    }

    /// provider 没有 `PluginId`，调用方不能凭名字把它路由到插件记录或启停状态。
    pub fn plugin_id(&self) -> Option<&PluginId> {
        match self {
            Self::BuiltinUnit { id } => Some(id),
            Self::BuiltinProvider => None,
        }
    }

    /// 目标只能是 `plugins/` 的直接子级；这里再次拒绝分隔符和点段，避免绕过
    /// 前面的保留名判定后把路径写到其他目录。
    fn directory_path(&self, plugin_root: &Path) -> Result<PathBuf, ReservedPreviewError> {
        let name = self.raw_name();
        let mut components = Path::new(name).components();
        let is_single_normal =
            matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
        if !is_single_normal || name.is_empty() {
            return Err(ReservedPreviewError::UnsafeReservedName);
        }
        Ok(plugin_root.join(name))
    }
}

/// 目录证据的固定失败原因；不使用底层错误正文或本机路径。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectoryInspectionFailure {
    /// 目标存在但不是目录，不能当成空目录继续。
    NotDirectory,
    /// 目标自身是符号链接、Windows junction 或其他重解析点。
    SymlinkOrReparsePoint,
    /// 目标内部有链接、重解析点、硬链接、未知类型或超限内容。
    /// 布局层只给同一类固定文案，本模块不解析文案，因此不再细分具体子原因。
    UnsafeTreeContent,
    /// 无法完成有界摘要（格式、读取期间变化或底层 IO 失败）。
    FingerprintUnavailable,
    /// `plugins/` 根本身不是安全目录。
    ParentDirectoryUnsafe,
    /// 未映射的安全拒绝或普通 IO 失败。
    Unreadable,
}

impl DirectoryInspectionFailure {
    /// 固定代码供证据摘要和界面分类使用，不随底层文案变化。
    pub fn code(self) -> &'static str {
        match self {
            Self::NotDirectory => "not_directory",
            Self::SymlinkOrReparsePoint => "symlink_or_reparse_point",
            Self::UnsafeTreeContent => "unsafe_tree_content",
            Self::FingerprintUnavailable => "fingerprint_unavailable",
            Self::ParentDirectoryUnsafe => "parent_directory_unsafe",
            Self::Unreadable => "unreadable",
        }
    }

    /// 固定中文说明供界面直接显示，避免底层错误泄露路径或插件提供的文本。
    pub fn message(self) -> &'static str {
        match self {
            Self::NotDirectory => "目标不是目录",
            Self::SymlinkOrReparsePoint => "目标目录是链接或重解析点",
            Self::UnsafeTreeContent => "目录内容不安全或超出限额（未细分原因）",
            Self::FingerprintUnavailable => "目录摘要无法安全完成",
            Self::ParentDirectoryUnsafe => "插件根目录不安全",
            Self::Unreadable => "目录无法安全读取",
        }
    }
}

/// 只读目录证据：安全目录一定带实际指纹，缺失与无法验证必须显式区分。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReservedDirectoryEvidence {
    Missing,
    Safe { fingerprint: PluginFingerprint },
    UnsafeOrUnreadable { reason: DirectoryInspectionFailure },
}

/// `state.plugins` 中以保留身份登记的外部记录，仅复制只读证据字段。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalPluginRecordEvidence {
    pub active_version: PluginVersion,
    pub fingerprint: Option<PluginFingerprint>,
    pub status: PluginInstallStatus,
    pub source: PluginSource,
    pub enabled: bool,
    pub confirmed: bool,
}

impl ExternalPluginRecordEvidence {
    /// 记录只是磁盘声明，不能替代实际目录指纹；这里只做字段投影。
    fn from_installed(installed: &InstalledPlugin) -> Self {
        Self {
            active_version: installed.active.clone(),
            fingerprint: installed.fingerprint.clone(),
            status: installed.status,
            source: installed.source,
            enabled: installed.enabled,
            confirmed: installed.confirmed,
        }
    }
}

/// `state.builtins` 与 `state.plugins` 是两套状态，同名出现时必须分别报告。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuiltinToggleEvidence {
    /// provider 不是启停单元，`state.builtins` 永远不会出现该名字。
    NotApplicable,
    /// 内置单元未被用户改过，按默认启用处理；这不是外部插件记录。
    DefaultEnabled,
    ExplicitEnabled,
    ExplicitDisabled,
}

impl BuiltinToggleEvidence {
    /// 固定代码用于区分证据来源，避免与外部记录的启用字段互相覆盖。
    pub fn code(self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::DefaultEnabled => "default_enabled",
            Self::ExplicitEnabled => "explicit_enabled",
            Self::ExplicitDisabled => "explicit_disabled",
        }
    }
}

/// 待处理意图作用的持久表；内置启停与外部目录记录不能混为一谈。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingChangeScope {
    /// 只改写 `state.builtins`，由启动阶段 `apply_builtin_change` 消费。
    BuiltinToggle,
    /// 针对 `state.plugins` 或外部目录；对保留身份而言属于异常材料。
    ExternalRecord,
}

impl PendingChangeScope {
    /// 固定代码用于界面解释阻断来源，避免把内置开关说成外部目录冲突。
    pub fn code(self) -> &'static str {
        match self {
            Self::BuiltinToggle => "builtin_toggle",
            Self::ExternalRecord => "external_record",
        }
    }
}

/// 与目标同 ID 的待处理意图类别；任何一项都阻止清理建议。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingChangeKind {
    Install,
    Update,
    Enable,
    Disable,
    Uninstall,
    Rollback,
}

impl PendingChangeKind {
    /// 只保留类别，不复制 staging 路径或其他可能变化的 payload。
    fn from_change(change: &PendingPluginChange) -> Self {
        match change {
            PendingPluginChange::Install { .. } => Self::Install,
            PendingPluginChange::Update { .. } => Self::Update,
            PendingPluginChange::Enable { .. } => Self::Enable,
            PendingPluginChange::Disable { .. } => Self::Disable,
            PendingPluginChange::Uninstall { .. } => Self::Uninstall,
            PendingPluginChange::Rollback { .. } => Self::Rollback,
        }
    }

    /// 固定代码供界面与摘要使用，保证分类改名不会悄悄改变证据含义。
    pub fn code(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Update => "update",
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Uninstall => "uninstall",
            Self::Rollback => "rollback",
        }
    }

    /// 只有内置单元的 Enable/Disable 属于 `state.builtins`；其余一律视为外部记录意图。
    fn scope(self, identity: &ReservedPluginIdentity) -> PendingChangeScope {
        match (identity, self) {
            (ReservedPluginIdentity::BuiltinUnit { .. }, Self::Enable | Self::Disable) => {
                PendingChangeScope::BuiltinToggle
            }
            _ => PendingChangeScope::ExternalRecord,
        }
    }
}

/// 与目标同 ID 的待处理证据；作用表与类别分开保存，避免误导界面。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingEvidence {
    /// 操作类别，不含 staging 路径等易变 payload。
    pub kind: PendingChangeKind,
    /// 该操作实际作用的持久表。
    pub scope: PendingChangeScope,
}

/// 恢复材料探测结果；任一 `unverified` 都表示本预览不能断言状态一致。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryEvidence {
    /// 存在状态恢复文件；必须由存储层显式处理。
    pub state_recovery_present: bool,
    /// 恢复文件枚举失败；不能据此断言“没有”。
    pub state_recovery_unverified: bool,
    /// 存在未完成布局 journal。
    pub pending_layout_journal: bool,
    /// journal 扫描失败；不能据此断言“没有”。
    pub layout_journal_unverified: bool,
    /// 同身份 `.staging` 残留；与恢复材料分开报告。
    pub staging_present: bool,
    /// `.staging` 无法枚举。
    pub staging_unverified: bool,
    /// 同身份 `.backup` 残留；卸载/更新未收尾时可能仍被引用。
    pub backup_present: bool,
    /// `.backup` 无法枚举。
    pub backup_unverified: bool,
}

impl RecoveryEvidence {
    /// 没有恢复材料且各级扫描都成功时才是明确。
    fn clear() -> Self {
        Self {
            state_recovery_present: false,
            state_recovery_unverified: false,
            pending_layout_journal: false,
            layout_journal_unverified: false,
            staging_present: false,
            staging_unverified: false,
            backup_present: false,
            backup_unverified: false,
        }
    }

    /// 插件根不可信时所有子项都无法核对，不能只标一部分。
    fn all_unverified() -> Self {
        Self {
            state_recovery_unverified: true,
            layout_journal_unverified: true,
            staging_unverified: true,
            backup_unverified: true,
            ..Self::clear()
        }
    }
}

/// 目录证据与 `state.plugins` 记录组合出的固定分类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReservedConflictClassification {
    /// 有外部记录，但目标目录不存在。
    RegisteredDirectoryMissing,
    /// 有外部记录，且实际目录指纹与记录一致。
    RegisteredFingerprintMatch,
    /// 有外部记录但没有可核对的指纹。
    RegisteredWithoutFingerprint,
    /// 有外部记录，实际目录指纹与记录不一致。
    RegisteredFingerprintMismatch,
    /// 没有外部记录，但存在安全目录。
    UnrecordedDirectorySafe,
    /// 没有外部记录，目录也不存在。
    UnrecordedDirectoryMissing,
    /// 触发了链接、重解析点、硬链接或无法有界读取等条件。
    UnsafeOrUnreadable,
}

impl ReservedConflictClassification {
    /// 固定代码让界面与测试引用同一分类，不依赖本地化文案。
    pub fn code(self) -> &'static str {
        match self {
            Self::RegisteredDirectoryMissing => "registered_directory_missing",
            Self::RegisteredFingerprintMatch => "registered_fingerprint_match",
            Self::RegisteredWithoutFingerprint => "registered_without_fingerprint",
            Self::RegisteredFingerprintMismatch => "registered_fingerprint_mismatch",
            Self::UnrecordedDirectorySafe => "unrecorded_directory_safe",
            Self::UnrecordedDirectoryMissing => "unrecorded_directory_missing",
            Self::UnsafeOrUnreadable => "unsafe_or_unreadable",
        }
    }
}

/// 阻断原因；只要存在一项，清理建议必须是 `Blocked`。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum PreviewBlocker {
    /// 目标目录或 `plugins/` 根不安全/不可读。
    DirectoryUnsafeOrUnreadable,
    /// 同 ID 的安装/更新/卸载/回退意图针对外部记录，属于清理冲突。
    PendingExternalChange,
    /// 外部记录缺少指纹，实际目录无法与记录建立身份关联；
    /// 在身份模型只能凭指纹核对的现状下不能建议移动目录。
    RecordIdentityUnverifiable,
    /// 同 ID 的内置启停只改写 `state.builtins`；仍阻断预览，但不描述成目录冲突。
    PendingBuiltinToggle,
    /// 存在 `state-recovery-*.json`，必须由存储层显式处理。
    StateRecoveryPresent,
    /// 无法枚举恢复材料，不能断言不存在。
    StateRecoveryUnverified,
    /// 存在未完成的布局 journal。
    LayoutJournalPending,
    /// journal 扫描或解析失败，无法断言没有未完成事务。
    LayoutJournalUnverified,
    /// 同身份 `.staging` 仍有残留，可能被 pending 或恢复引用。
    StagingForIdentity,
    /// 无法枚举 `.staging`，不能断言没有残留。
    StagingUnverified,
    /// 同身份 `.backup` 仍有残留，恢复可能仍会使用。
    BackupForIdentity,
    /// 无法枚举 `.backup`，不能断言没有残留。
    BackupUnverified,
}

impl PreviewBlocker {
    /// 固定代码便于界面分组展示阻断原因，也用于摘要的稳定输入。
    pub fn code(self) -> &'static str {
        match self {
            Self::DirectoryUnsafeOrUnreadable => "directory_unsafe_or_unreadable",
            Self::PendingExternalChange => "pending_external_change",
            Self::RecordIdentityUnverifiable => "record_identity_unverifiable",
            Self::PendingBuiltinToggle => "pending_builtin_toggle",
            Self::StateRecoveryPresent => "state_recovery_present",
            Self::StateRecoveryUnverified => "state_recovery_unverified",
            Self::LayoutJournalPending => "layout_journal_pending",
            Self::LayoutJournalUnverified => "layout_journal_unverified",
            Self::StagingForIdentity => "staging_for_identity",
            Self::StagingUnverified => "staging_unverified",
            Self::BackupForIdentity => "backup_for_identity",
            Self::BackupUnverified => "backup_unverified",
        }
    }
}

/// 仅用于预览的建议，不包含执行器；确认动作必须由调用方在锁内重新核对证据。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewCleanupSuggestion {
    /// 没有发现需要隔离或移除的材料；这不等于启动迁移门禁已通过。
    None,
    /// 外部记录存在但目录缺失，可考虑仅移除陈旧记录。
    RemoveStaleExternalRecord,
    /// 没有外部记录但存在安全目录，可考虑把目录移入隔离区。
    QuarantineDirectory,
    /// 外部记录和安全目录同时存在，可考虑移除记录后隔离目录。
    RemoveExternalRecordAndQuarantineDirectory,
    /// 存在待处理意图、恢复材料，或记录无指纹导致目录身份无法核对；先处理阻断原因。
    Blocked,
}

impl PreviewCleanupSuggestion {
    /// 固定代码只描述候选动作，调用方不能把它当作可执行指令。
    pub fn code(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::RemoveStaleExternalRecord => "remove_stale_external_record",
            Self::QuarantineDirectory => "quarantine_directory",
            Self::RemoveExternalRecordAndQuarantineDirectory => {
                "remove_external_record_and_quarantine_directory"
            }
            Self::Blocked => "blocked",
        }
    }
}

/// 单个保留身份的只读预览结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReservedConflictPreview {
    /// 推导证据时使用的权威 revision；执行清理前必须重新核对。
    pub revision: u64,
    pub identity: ReservedPluginIdentity,
    pub classification: ReservedConflictClassification,
    pub directory: ReservedDirectoryEvidence,
    pub external_record: Option<ExternalPluginRecordEvidence>,
    /// 与 `external_record.enabled` 相互独立，不能凭同一个 ID 互相改写。
    pub builtin_toggle: BuiltinToggleEvidence,
    pub pending_changes: Vec<PendingEvidence>,
    pub recovery: RecoveryEvidence,
    pub blockers: Vec<PreviewBlocker>,
    /// 仅描述下一步候选；不执行动作，也不是授权，更不代表启动迁移门禁通过。
    pub suggested_cleanup: PreviewCleanupSuggestion,
    /// revision、实际指纹和本次证据的陈旧判断值；不是授权凭据。
    pub evidence_digest: String,
}

impl ReservedConflictPreview {
    /// 任一阻断原因存在时都不得把预览当作可执行依据。
    pub fn is_blocked(&self) -> bool {
        !self.blockers.is_empty()
    }

    /// 未阻断只表示本次没有发现该类证据；清理与启动门禁仍需各自复核。
    pub fn is_clean_for_cleanup(&self) -> bool {
        self.blockers.is_empty() && self.suggested_cleanup != PreviewCleanupSuggestion::Blocked
    }

    /// 安全目录才有实际指纹；缺失和不安全目录明确返回 `None`，不伪造空目录摘要。
    pub fn observed_fingerprint(&self) -> Option<&PluginFingerprint> {
        match &self.directory {
            ReservedDirectoryEvidence::Safe { fingerprint } => Some(fingerprint),
            _ => None,
        }
    }

    /// 记录中的指纹只是声明；执行前仍要与锁内重新计算的指纹比较。
    pub fn recorded_fingerprint(&self) -> Option<&PluginFingerprint> {
        self.external_record
            .as_ref()
            .and_then(|record| record.fingerprint.as_ref())
    }
}

/// 单入口：状态与根都来自同一个权威 store，调用方只能声明期望 revision。
///
/// 这里刻意不取存储写锁（`lock()` 会创建根目录并写锁文件），而是复用存储层的
/// 只读加载路径。读取前先核对插件根自身：根存在但不安全（链接、重解析点或
/// 非目录）时返回 [`ReservedPreviewError::PluginRootUnsafe`]，避免存储层把同一
/// 情况折叠进 `InvalidState` 后被误报成状态损坏；根缺失或可读才继续加载状态。
/// 加载失败、旧 schema、损坏文件一律返回类型化错误，不会退化成空状态。
/// `expected_revision` 只用于陈旧检测，不是授权。
pub fn preview_reserved_conflict(
    store: &PluginStore,
    expected_revision: u64,
    requested: &str,
) -> Result<ReservedConflictPreview, ReservedPreviewError> {
    let plugin_root = layout::plugin_root(store.root());
    // 与存储层 `plugin_directory` 相同的判定，但结论独立成专用错误：存储层
    // 把根不安全折叠进 `InvalidState`，公开入口若直接透传会把“目录不安全”
    // 误说成“状态损坏”，界面提示会引导错误方向。
    match fs::symlink_metadata(&plugin_root) {
        Ok(metadata) if metadata.is_dir() && layout::safe_metadata(&metadata) => {}
        Ok(_) => return Err(ReservedPreviewError::PluginRootUnsafe),
        // 根缺失属于“尚未安装插件”，交给存储层按首次启动读取空状态；
        // 其余 IO 失败保留存储层原分类，不吞错。
        Err(_) => {}
    }
    let snapshot = store.read_unlocked().map_err(map_store_error)?;
    if snapshot.state.revision != expected_revision {
        return Err(ReservedPreviewError::StaleRevision {
            expected: expected_revision,
            actual: snapshot.state.revision,
        });
    }
    preview_from_state(store.root(), &snapshot.state, requested)
}

/// 纯计算入口，仅供本模块与其单元测试使用；不对外公开，避免任意 root/state 组合。
fn preview_from_state(
    root: &Path,
    state: &PluginStoreState,
    requested: &str,
) -> Result<ReservedConflictPreview, ReservedPreviewError> {
    if !root.is_absolute() {
        return Err(ReservedPreviewError::RootNotAbsolute);
    }
    let identity = ReservedPluginIdentity::parse(requested)?;
    let plugin_root = layout::plugin_root(root);
    let directory_path = identity.directory_path(&plugin_root)?;
    let root_state = inspect_plugin_root(&plugin_root);
    let recovery = inspect_recovery(root, &plugin_root, root_state, identity.raw_name());

    // 父目录不安全时不再继续探测子路径，否则可能沿链接读取插件根之外的内容。
    let directory = match root_state {
        PluginRootState::Missing => ReservedDirectoryEvidence::Missing,
        PluginRootState::Unsafe => ReservedDirectoryEvidence::UnsafeOrUnreadable {
            reason: DirectoryInspectionFailure::ParentDirectoryUnsafe,
        },
        PluginRootState::Unreadable => ReservedDirectoryEvidence::UnsafeOrUnreadable {
            reason: DirectoryInspectionFailure::Unreadable,
        },
        PluginRootState::Safe => inspect_directory(&directory_path),
    };

    let external_record = identity
        .plugin_id()
        .and_then(|id| state.plugins.get(id))
        .map(ExternalPluginRecordEvidence::from_installed);
    let builtin_toggle = match &identity {
        ReservedPluginIdentity::BuiltinProvider => BuiltinToggleEvidence::NotApplicable,
        ReservedPluginIdentity::BuiltinUnit { id } => match state.builtins.get(id) {
            None => BuiltinToggleEvidence::DefaultEnabled,
            Some(value) if *value => BuiltinToggleEvidence::ExplicitEnabled,
            Some(_) => BuiltinToggleEvidence::ExplicitDisabled,
        },
    };
    let pending_changes = collect_pending_changes(state, &identity);
    let classification = classify(external_record.as_ref(), &directory);
    let blockers = collect_blockers(classification, &pending_changes, recovery);
    let suggested_cleanup = if blockers.is_empty() {
        suggest_cleanup(classification)
    } else {
        PreviewCleanupSuggestion::Blocked
    };

    let mut preview = ReservedConflictPreview {
        revision: state.revision,
        identity,
        classification,
        directory,
        external_record,
        builtin_toggle,
        pending_changes,
        recovery,
        blockers,
        suggested_cleanup,
        evidence_digest: String::new(),
    };
    // 先算入局部变量再赋值，避免在构造后又对整个结果做一次借用后写入字段。
    let evidence_digest = digest_preview(&preview);
    preview.evidence_digest = evidence_digest;
    Ok(preview)
}

/// `plugins/` 根的状态决定能否继续读取；缺失、不安全与 IO 失败必须分开。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PluginRootState {
    Missing,
    Safe,
    Unsafe,
    Unreadable,
}

/// 只检查插件根自身，不跟随链接或重解析点。
fn inspect_plugin_root(plugin_root: &Path) -> PluginRootState {
    match fs::symlink_metadata(plugin_root) {
        Ok(metadata) if metadata.is_dir() && layout::safe_metadata(&metadata) => {
            PluginRootState::Safe
        }
        Ok(_) => PluginRootState::Unsafe,
        Err(error) if error.kind() == io::ErrorKind::NotFound => PluginRootState::Missing,
        Err(_) => PluginRootState::Unreadable,
    }
}

/// 只读探测恢复材料、目录事务和同身份暂存/备份；无法可靠读取时保留“未核对”语义。
fn inspect_recovery(
    root: &Path,
    plugin_root: &Path,
    root_state: PluginRootState,
    identity_name: &str,
) -> RecoveryEvidence {
    match root_state {
        PluginRootState::Missing => RecoveryEvidence::clear(),
        // 插件根本身不安全时不能再信任其子项名称，全部标记为未核对。
        PluginRootState::Unsafe | PluginRootState::Unreadable => RecoveryEvidence::all_unverified(),
        PluginRootState::Safe => {
            let (state_recovery_present, state_recovery_unverified) =
                scan_state_recovery(plugin_root);
            let (pending_layout_journal, layout_journal_unverified) = scan_layout_journals(root);
            let staging =
                scan_identity_work_directory(plugin_root, layout::STAGING_DIRECTORY, identity_name);
            let backup =
                scan_identity_work_directory(plugin_root, layout::BACKUP_DIRECTORY, identity_name);
            RecoveryEvidence {
                state_recovery_present,
                state_recovery_unverified,
                pending_layout_journal,
                layout_journal_unverified,
                staging_present: staging == WorkDirectoryEvidence::Present,
                staging_unverified: staging == WorkDirectoryEvidence::Unverified,
                backup_present: backup == WorkDirectoryEvidence::Present,
                backup_unverified: backup == WorkDirectoryEvidence::Unverified,
            }
        }
    }
}

/// 只按名称发现 `state-recovery-*.json`，不打开内容；条目过多时报告未核对。
fn scan_state_recovery(plugin_root: &Path) -> (bool, bool) {
    let entries = match fs::read_dir(plugin_root) {
        Ok(entries) => entries,
        Err(_) => return (false, true),
    };
    let mut scanned = 0usize;
    for entry in entries {
        scanned += 1;
        if scanned > MAX_RECOVERY_SCAN_ENTRIES {
            // 未能覆盖全部条目时不能断言“没有恢复材料”。
            return (false, true);
        }
        match entry {
            Ok(entry) => {
                if is_state_recovery_name(&entry.file_name()) {
                    return (true, false);
                }
            }
            Err(_) => return (false, true),
        }
    }
    (false, false)
}

/// 与存储层保持大小写不敏感的名称判定，同时覆盖损坏、链接和非普通文件。
fn is_state_recovery_name(name: &OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_lowercase();
    name.starts_with("state-recovery-") && name.ends_with(".json")
}

/// 复用布局模块的只读 journal 扫描；解析失败等同于“存在未核对恢复材料”。
fn scan_layout_journals(root: &Path) -> (bool, bool) {
    match layout::scan_journals(root) {
        Ok(journals) => {
            let pending = journals.iter().any(|(_, journal)| {
                !matches!(journal.phase, LayoutPhase::Committed | LayoutPhase::Cleaned)
            });
            (pending, false)
        }
        Err(_) => (false, true),
    }
}

/// 同身份工作目录的探测结论；三态避免把“读不到”说成“没有”。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkDirectoryEvidence {
    Absent,
    Present,
    Unverified,
}

/// 只探测 `plugins/.staging/<id>` 或 `plugins/.backup/<id>` 是否存在，不进入其内容。
fn scan_identity_work_directory(
    plugin_root: &Path,
    container: &str,
    identity_name: &str,
) -> WorkDirectoryEvidence {
    let container_path = plugin_root.join(container);
    match fs::symlink_metadata(&container_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return WorkDirectoryEvidence::Absent;
        }
        Err(_) => return WorkDirectoryEvidence::Unverified,
        Ok(metadata) if !metadata.is_dir() || !layout::safe_metadata(&metadata) => {
            return WorkDirectoryEvidence::Unverified;
        }
        Ok(_) => {}
    }
    match fs::symlink_metadata(container_path.join(identity_name)) {
        Ok(_) => WorkDirectoryEvidence::Present,
        Err(error) if error.kind() == io::ErrorKind::NotFound => WorkDirectoryEvidence::Absent,
        Err(_) => WorkDirectoryEvidence::Unverified,
    }
}

/// 包目录必须逐项通过安全目录与有界指纹检查；任何失败都返回固定原因。
fn inspect_directory(directory: &Path) -> ReservedDirectoryEvidence {
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return ReservedDirectoryEvidence::Missing;
        }
        Err(_) => {
            return ReservedDirectoryEvidence::UnsafeOrUnreadable {
                reason: DirectoryInspectionFailure::Unreadable,
            };
        }
    };
    // 先排除链接和重解析点：符号链接的 `is_dir` 为假，若先按“非目录”处理会
    // 丢失真正的风险类别，也无法让界面提示人工核对。
    if !layout::safe_metadata(&metadata) {
        return ReservedDirectoryEvidence::UnsafeOrUnreadable {
            reason: DirectoryInspectionFailure::SymlinkOrReparsePoint,
        };
    }
    if !metadata.is_dir() {
        let reason = if metadata.is_file() {
            DirectoryInspectionFailure::NotDirectory
        } else {
            DirectoryInspectionFailure::Unreadable
        };
        return ReservedDirectoryEvidence::UnsafeOrUnreadable { reason };
    }
    match layout::fingerprint_tree(directory) {
        Ok(fingerprint) => ReservedDirectoryEvidence::Safe { fingerprint },
        Err(error) => ReservedDirectoryEvidence::UnsafeOrUnreadable {
            reason: classify_layout_failure(error),
        },
    }
}

/// 把布局层错误折叠为固定类别。
///
/// 布局层对目录内部问题使用同一种 `Unsafe` 变体，仅文案不同；本模块不解析动态
/// 文案，因此不会把无法区分的链接、硬链接或非目录说成某个确定原因。
fn classify_layout_failure(error: LayoutError) -> DirectoryInspectionFailure {
    match error {
        LayoutError::Unsafe(_) => DirectoryInspectionFailure::UnsafeTreeContent,
        LayoutError::Fingerprint(_) => DirectoryInspectionFailure::FingerprintUnavailable,
        LayoutError::Io(_)
        | LayoutError::Missing(_)
        | LayoutError::Conflict(_)
        | LayoutError::RecoveryRequired { .. }
        | LayoutError::OutcomeUnknown { .. } => DirectoryInspectionFailure::Unreadable,
    }
}

/// 只根据 `state.plugins` 记录和目录证据分类；`state.builtins` 不冒充外部记录。
fn classify(
    record: Option<&ExternalPluginRecordEvidence>,
    directory: &ReservedDirectoryEvidence,
) -> ReservedConflictClassification {
    if matches!(
        directory,
        ReservedDirectoryEvidence::UnsafeOrUnreadable { .. }
    ) {
        return ReservedConflictClassification::UnsafeOrUnreadable;
    }
    match (record, directory) {
        (Some(_), ReservedDirectoryEvidence::Missing) => {
            ReservedConflictClassification::RegisteredDirectoryMissing
        }
        (Some(record), ReservedDirectoryEvidence::Safe { fingerprint }) => {
            match record.fingerprint.as_ref() {
                None => ReservedConflictClassification::RegisteredWithoutFingerprint,
                Some(expected) if expected == fingerprint => {
                    ReservedConflictClassification::RegisteredFingerprintMatch
                }
                Some(_) => ReservedConflictClassification::RegisteredFingerprintMismatch,
            }
        }
        (None, ReservedDirectoryEvidence::Missing) => {
            ReservedConflictClassification::UnrecordedDirectoryMissing
        }
        (None, ReservedDirectoryEvidence::Safe { .. }) => {
            ReservedConflictClassification::UnrecordedDirectorySafe
        }
        // 未经检查的缺失/安全组合不会到达这里；保守归为无法验证。
        (_, ReservedDirectoryEvidence::UnsafeOrUnreadable { .. }) => {
            ReservedConflictClassification::UnsafeOrUnreadable
        }
    }
}

/// 同 ID 的任意 pending 都阻止清理；provider 没有 `PluginId`，永远不会匹配。
/// 内置单元的 Enable/Disable 归入 `state.builtins` 作用域，不描述成目录冲突。
fn collect_pending_changes(
    state: &PluginStoreState,
    identity: &ReservedPluginIdentity,
) -> Vec<PendingEvidence> {
    identity
        .plugin_id()
        .map(|id| {
            state
                .pending
                .iter()
                .filter(|change| change.plugin_id() == id)
                .map(|change| {
                    let kind = PendingChangeKind::from_change(change);
                    PendingEvidence {
                        kind,
                        scope: kind.scope(identity),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 收集阻断原因；目录不安全、pending、恢复材料、同身份暂存/备份以及无法核对
/// 的记录身份都阻断建议。分类作为输入而不是由本函数重算，保证分类与阻断原因
/// 始终来自同一份目录证据。
fn collect_blockers(
    classification: ReservedConflictClassification,
    pending_changes: &[PendingEvidence],
    recovery: RecoveryEvidence,
) -> Vec<PreviewBlocker> {
    let mut blockers = BTreeSet::new();
    if matches!(
        classification,
        ReservedConflictClassification::UnsafeOrUnreadable
    ) {
        blockers.insert(PreviewBlocker::DirectoryUnsafeOrUnreadable);
    }
    if matches!(
        classification,
        ReservedConflictClassification::RegisteredWithoutFingerprint
    ) {
        blockers.insert(PreviewBlocker::RecordIdentityUnverifiable);
    }
    for pending in pending_changes {
        match pending.scope {
            PendingChangeScope::BuiltinToggle => {
                blockers.insert(PreviewBlocker::PendingBuiltinToggle);
            }
            PendingChangeScope::ExternalRecord => {
                blockers.insert(PreviewBlocker::PendingExternalChange);
            }
        }
    }
    if recovery.state_recovery_present {
        blockers.insert(PreviewBlocker::StateRecoveryPresent);
    }
    if recovery.state_recovery_unverified {
        blockers.insert(PreviewBlocker::StateRecoveryUnverified);
    }
    if recovery.pending_layout_journal {
        blockers.insert(PreviewBlocker::LayoutJournalPending);
    }
    if recovery.layout_journal_unverified {
        blockers.insert(PreviewBlocker::LayoutJournalUnverified);
    }
    if recovery.staging_present {
        blockers.insert(PreviewBlocker::StagingForIdentity);
    }
    if recovery.staging_unverified {
        blockers.insert(PreviewBlocker::StagingUnverified);
    }
    if recovery.backup_present {
        blockers.insert(PreviewBlocker::BackupForIdentity);
    }
    if recovery.backup_unverified {
        blockers.insert(PreviewBlocker::BackupUnverified);
    }
    blockers.into_iter().collect()
}

/// 建议只在未阻断时给出；隔离是移动候选而不是删除或执行。
fn suggest_cleanup(classification: ReservedConflictClassification) -> PreviewCleanupSuggestion {
    match classification {
        ReservedConflictClassification::UnrecordedDirectoryMissing => {
            PreviewCleanupSuggestion::None
        }
        ReservedConflictClassification::UnrecordedDirectorySafe => {
            PreviewCleanupSuggestion::QuarantineDirectory
        }
        ReservedConflictClassification::RegisteredDirectoryMissing => {
            PreviewCleanupSuggestion::RemoveStaleExternalRecord
        }
        // 已登记但无指纹时，实际目录无法与记录的安装内容建立身份关联：目录可能
        // 是该插件的数据，也可能是同路径的其他材料。在只能凭指纹核对身份的当前
        // 模型下不能证明一致，保守给出阻断而不是建议移动，避免把无法核对的目录
        // 当成可隔离的插件内容。
        ReservedConflictClassification::RegisteredWithoutFingerprint => {
            PreviewCleanupSuggestion::Blocked
        }
        ReservedConflictClassification::RegisteredFingerprintMatch
        | RegisteredConflictClassification::RegisteredFingerprintMismatch => {
            PreviewCleanupSuggestion::RemoveExternalRecordAndQuarantineDirectory
        }
        // 不安全目录已有阻断原因；即使被单独调用也只给阻断结论。
        ReservedConflictClassification::UnsafeOrUnreadable => PreviewCleanupSuggestion::Blocked,
    }
}

/// 计算证据摘要；revision 或实际目录内容变化都会改变摘要，供界面识别陈旧快照。
/// 摘要只覆盖本次只读证据，不代表授权，也不能证明内容来源。
fn digest_preview(preview: &ReservedConflictPreview) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"kerminal.reserved-conflict-preview.v2");
    hasher.update(preview.revision.to_le_bytes());
    hash_text(&mut hasher, preview.identity.raw_name());
    hash_text(&mut hasher, preview.classification.code());
    match &preview.directory {
        ReservedDirectoryEvidence::Missing => hash_text(&mut hasher, "missing"),
        ReservedDirectoryEvidence::Safe { fingerprint } => {
            hash_text(&mut hasher, "safe");
            hash_text(&mut hasher, fingerprint.as_str());
        }
        ReservedDirectoryEvidence::UnsafeOrUnreadable { reason } => {
            hash_text(&mut hasher, "unsafe");
            hash_text(&mut hasher, reason.code());
        }
    }
    match preview.external_record.as_ref() {
        Some(record) => {
            hasher.update([1u8]);
            let active_version = record.active_version.to_string();
            hash_text(&mut hasher, &active_version);
            hash_optional_fingerprint(&mut hasher, record.fingerprint.as_ref());
            hash_text(&mut hasher, status_code(record.status));
            hasher.update([u8::from(record.enabled), u8::from(record.confirmed)]);
            hash_text(&mut hasher, source_code(record.source));
        }
        None => hasher.update([0u8]),
    }
    hash_text(&mut hasher, preview.builtin_toggle.code());
    hasher.update((preview.pending_changes.len() as u64).to_le_bytes());
    for pending in &preview.pending_changes {
        hash_text(&mut hasher, pending.kind.code());
        hash_text(&mut hasher, pending.scope.code());
    }
    let recovery = preview.recovery;
    hasher.update([
        u8::from(recovery.state_recovery_present),
        u8::from(recovery.state_recovery_unverified),
        u8::from(recovery.pending_layout_journal),
        u8::from(recovery.layout_journal_unverified),
        u8::from(recovery.staging_present),
        u8::from(recovery.staging_unverified),
        u8::from(recovery.backup_present),
        u8::from(recovery.backup_unverified),
    ]);
    hasher.update((preview.blockers.len() as u64).to_le_bytes());
    for blocker in &preview.blockers {
        hash_text(&mut hasher, blocker.code());
    }
    hash_text(&mut hasher, preview.suggested_cleanup.code());
    // 使用独立前缀，避免被误当成 `PluginFingerprint` 的 `v1:` 格式。
    format!("preview-v2:{:x}", hasher.finalize())
}

/// 长度前缀避免字段边界拼接产生歧义。
fn hash_text(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

/// 缺失指纹与空字符串必须可区分。
fn hash_optional_fingerprint(hasher: &mut Sha256, fingerprint: Option<&PluginFingerprint>) {
    match fingerprint {
        Some(fingerprint) => {
            hasher.update([1u8]);
            hash_text(hasher, fingerprint.as_str());
        }
        None => hasher.update([0u8]),
    }
}

/// 状态代码只用于摘要稳定性，不暴露插件文本；`Unsupported` 必须带上具体 reason，
/// 否则身份冲突与旧 script 会被摘要成同一证据。
fn status_code(status: PluginInstallStatus) -> &'static str {
    match status {
        PluginInstallStatus::Ready => "ready",
        PluginInstallStatus::Missing => "missing",
        PluginInstallStatus::Corrupt => "corrupt",
        PluginInstallStatus::Unsupported { reason } => match reason {
            PluginUnsupportedReason::ScriptRemoved => "unsupported_script_removed",
            PluginUnsupportedReason::ReservedIdentity => "unsupported_reserved_identity",
        },
    }
}

/// 来源代码只用于摘要稳定性。
fn source_code(source: PluginSource) -> &'static str {
    match source {
        PluginSource::Official => "official",
        PluginSource::Local => "local",
        PluginSource::DropIn => "drop_in",
    }
}

// 仅在本模块被 `plugins/mod.rs` 声明后编译；当前阶段尚未接线，测试不会运行。
#[cfg(test)]
#[path = "../../tests/unit/plugin_reserved_conflicts_preview.rs"]
mod tests;
