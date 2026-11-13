//! 0.4 用户数据格式迁移用例与安全结果契约。
//!
//! @author kongweiguang

use std::{path::Path, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::AppResult;

/// 用受控分类描述迁移预检阻断原因；相对路径仅由 adapter 使用固定 Kerminal 路径填充。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DataMigrationBlocker {
    MarkerCorrupt,
    FutureDataFormat {
        data_format: u32,
    },
    UnsupportedDataFormat {
        data_format: u32,
    },
    FutureMigrationContract {
        migration_contract: u32,
    },
    UnsupportedMigrationContract {
        migration_contract: u32,
    },
    UnknownNonEmptyRoot,
    PendingLegacyTransaction {
        relative_path: String,
    },
    CorruptLegacyTransaction {
        relative_path: String,
    },
    CorruptMigrationJournal,
    InputConflict {
        relative_path: String,
    },
    ManagedDataInvalid {
        relative_path: String,
    },
    ConfigurationNotMigratable,
    AgentWorkspaceNotMigratable,
    WindowStateNotMigratable,
    UnsupportedWorkspaceSessionSchema {
        version: Option<u32>,
    },
    UnsupportedCommandHistorySchema {
        schema_version: u32,
    },
    CorruptCommandHistoryDatabase,
    OrphanedMigrationData,
    /// 插件目录需要人工恢复；固定类别避免把本机路径或插件内容带进报告。
    PluginLayoutRecoveryRequired,
    /// 发现无法归类或不足以安全迁移的插件材料，原文件必须保留。
    PluginLayoutUnsupportedMaterial,
    /// 状态文件、清单或目录身份不一致；不覆盖原状态。
    PluginLayoutStateInvalid {
        schema: Option<u32>,
    },
    /// 残留 staging/backup 等未证明归属的材料，不能盲删。
    PluginLayoutResidualMaterial,
    /// 插件包清单或主机 schema 无法安全读取，迁移所需 codec 不完整。
    PluginLayoutPackageInvalid,
}

/// 只回报不会单独阻断迁移的范围提示；WAL 会使主库头部版本仅作提示，完整校验留给 apply。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DataMigrationNotice {
    SqliteWalPresent,
    /// 旧插件布局可自动迁移；通知数量固定，不回显插件身份。
    PluginLayoutLegacyMigration,
    /// 旧 script 包会原样搬目录并在新协议下显示不支持，避免静默丢失用户数据。
    PluginLayoutScriptPackages {
        count: usize,
    },
    /// 已存在插件布局 journal，下一次 apply 会按原证据前滚。
    PluginLayoutMigrationResume,
}

/// 插件布局与数据格式分开预检，桌面屏障据此决定能否冻结插件目录。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PluginLayoutReadiness {
    /// 数据根尚无插件目录或目录为空。
    #[default]
    Fresh,
    /// schema 2 且没有待处理的布局迁移。
    Current,
    /// 已知 schema 1，可由迁移入口转换。
    Legacy,
    /// 存在可恢复的布局 journal；不得直接进入普通启动。
    RecoveryPending,
    /// 未知、混合或证据不足，必须保留现场。
    Blocked,
}

impl PluginLayoutReadiness {
    /// 只有已确认的当前/空布局才允许跳过插件迁移快路径；Fresh 没有待迁移材料。
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Fresh | Self::Current)
    }
}

/// 插件布局迁移回执只报告数量与受控备份相对路径，不包含插件内容。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginLayoutReport {
    pub outcome: PluginLayoutOutcome,
    pub migrated_plugins: usize,
    pub removed_previous_versions: usize,
    pub discarded_pending: usize,
    /// 迁移事务内使用的备份目录；成功后按证明式清理规则移除。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_relative_path: Option<String>,
}

/// 区分首次转换、恢复中断事务和重复 apply 的幂等结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PluginLayoutOutcome {
    AlreadyCurrent,
    Migrated,
    Recovered,
}

/// 描述格式标记与迁移可行性预检；不承诺 SQLite 全面健康，`recovery_pending` 表示可按原 journal 前滚。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataMigrationInspection {
    pub readiness: DataMigrationReadiness,
    pub data_format: Option<u32>,
    pub migration_contract: Option<u32>,
    pub blockers: Vec<DataMigrationBlocker>,
    pub notices: Vec<DataMigrationNotice>,
    pub recovery_pending: bool,
    /// 插件布局单独分类；旧插件留在旧目录时数据格式也不能被认为已完整当前。
    #[serde(default)]
    pub plugin_layout: PluginLayoutReadiness,
}

/// 标记当前数据代、已知旧数据、干净新根或必须人工处理的安全状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DataMigrationReadiness {
    Fresh,
    Current,
    Legacy,
    Blocked,
}

/// 迁移回执只返回变更数量和根目录内的恢复材料相对路径，不回传文件正文或绝对路径。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataMigrationReport {
    pub outcome: DataMigrationOutcome,
    pub changed_file_count: usize,
    pub resumed: bool,
    pub backup_relative_path: Option<String>,
    /// 连续恢复旧事务再升级新契约时保留全部恢复目录；旧单路径字段继续指向最后一份。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backup_relative_paths: Vec<String>,
    /// 本次调用同时完成的插件布局步骤；纯数据迁移或幂等读回时缺省。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_layout: Option<PluginLayoutReport>,
}

/// 预演仅暴露数量和是否需要更新密文，不包含配置正文、路径或凭据。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileImportPreview {
    pub source_profiles: usize,
    pub new_profiles: usize,
    pub new_hosts: usize,
    pub groups_changed: bool,
    pub vault_changed: bool,
}

/// 用有限结果区分空根初始化、旧数据转换、重启前滚与当前格式幂等回执。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DataMigrationOutcome {
    AlreadyCurrent,
    Initialized,
    Migrated,
    Resumed,
}

/// 桌面与 console 共用的同步迁移入口；检查和迁移在 runtime 服务装配前执行。
pub trait DataMigrationRepository: Send + Sync + 'static {
    /// 只读取根目录和格式状态，禁止创建目录、锁、日志、备份或 SQLite 辅助文件。
    fn inspect(&self) -> AppResult<DataMigrationInspection>;

    /// 在 adapter 的共享根锁内重新检查输入 revision，再按 journal 执行或恢复迁移。
    fn migrate(&self) -> AppResult<DataMigrationReport>;

    /// 导入预演不写来源或目标，旧测试端口默认明确报告未实现。
    fn preview_legacy_profiles(&self, _source_root: &Path) -> AppResult<ProfileImportPreview> {
        Err(crate::AppError::unsupported(
            "当前数据仓储不支持旧 Profile 导入",
        ))
    }

    /// 显式导入使用目标当前契约的独立事务；不允许自动选择来源。
    fn import_legacy_profiles(&self, _source_root: &Path) -> AppResult<DataMigrationReport> {
        Err(crate::AppError::unsupported(
            "当前数据仓储不支持旧 Profile 导入",
        ))
    }
}

/// desktop/console 共用的用例 facade；磁盘事务细节留在 repository adapter。
#[derive(Clone)]
pub struct DataMigrationService {
    repository: Arc<dyn DataMigrationRepository>,
}

impl DataMigrationService {
    /// 注入同一数据根的迁移 adapter，避免 application 直接访问文件系统或 SQLite。
    pub fn new(repository: Arc<dyn DataMigrationRepository>) -> Self {
        Self { repository }
    }

    /// 预检只返回安全 DTO；启动入口据 readiness 决定继续迁移还是进入可见恢复状态。
    pub fn inspect(&self) -> AppResult<DataMigrationInspection> {
        self.repository.inspect()
    }

    /// 执行一次迁移或幂等读回；结果未知时 adapter 保留 journal 供后续安全前滚。
    pub fn migrate(&self) -> AppResult<DataMigrationReport> {
        self.repository.migrate()
    }

    /// 只读计算导入影响，供命令行或界面在执行前展示真实数量。
    pub fn preview_legacy_profiles(&self, source_root: &Path) -> AppResult<ProfileImportPreview> {
        self.repository.preview_legacy_profiles(source_root)
    }

    /// 由已选择的源根触发导入；冲突和结果未知分类由唯一仓储原样返回。
    pub fn import_legacy_profiles(&self, source_root: &Path) -> AppResult<DataMigrationReport> {
        self.repository.import_legacy_profiles(source_root)
    }
}

#[cfg(test)]
#[path = "../tests/unit/data_migration.rs"]
mod tests;
