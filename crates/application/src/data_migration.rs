//! 0.4 用户数据格式迁移用例与安全结果契约。
//!
//! @author kongweiguang

use std::sync::Arc;

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
    FutureDataFormat { data_format: u32 },
    UnsupportedDataFormat { data_format: u32 },
    FutureMigrationContract { migration_contract: u32 },
    UnsupportedMigrationContract { migration_contract: u32 },
    UnknownNonEmptyRoot,
    PendingLegacyTransaction { relative_path: String },
    CorruptLegacyTransaction { relative_path: String },
    CorruptMigrationJournal,
    InputConflict { relative_path: String },
    ManagedDataInvalid { relative_path: String },
    ConfigurationNotMigratable,
    AgentWorkspaceNotMigratable,
    WindowStateNotMigratable,
    UnsupportedWorkspaceSessionSchema { version: Option<u32> },
    UnsupportedCommandHistorySchema { schema_version: u32 },
    CorruptCommandHistoryDatabase,
    OrphanedMigrationData,
}

/// 只回报不会单独阻断迁移的范围提示；WAL 会使主库头部版本仅作提示，完整校验留给 apply。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DataMigrationNotice {
    SqliteWalPresent,
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
}

#[cfg(test)]
#[path = "../tests/unit/data_migration.rs"]
mod tests;
