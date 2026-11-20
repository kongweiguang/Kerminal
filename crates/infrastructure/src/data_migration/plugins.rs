//! schema 1 插件目录的只读分类、迁移事务与受限旧清单读取。
//!
//! schema 1 的 `plugins/packages/<id>/<version>/` 与 schema 2 的单目录布局不能同时
//! 表达；迁移必须先备份整棵旧树，再按持久 journal 把活动版本移入 `plugins/<id>/`，
//! 最后才写 schema 2 状态。任何改名、状态 CAS 或删除的结果未知时都先回读证据，
//! 证据不足就保留现场并阻断普通启动，不能猜成未提交后清理材料。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
};

use kerminal_application::{
    AppError, AppResult,
    data_migration::{
        DataMigrationBlocker, DataMigrationNotice, PluginLayoutOutcome, PluginLayoutReadiness,
        PluginLayoutReport,
    },
    plugins::is_reserved_plugin_id,
};
use kerminal_domain::plugins::{
    InstalledPlugin, PendingPluginChange, PluginBootMarker, PluginFingerprint, PluginId,
    PluginInstallStatus, PluginSource, PluginStoreState, PluginUnsupportedReason, PluginVersion,
};
use kerminal_plugin_protocol::{ConfigurationSchema, ManifestPolicy, PluginManifest};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    plugins::{
        layout::{self, LayoutError},
        store::validate_state,
    },
    storage::{AtomicCommitError, CommitMode, StorageCoordinator, commit_staged_file},
};

use super::journal;

const PLUGINS_DIRECTORY: &str = "plugins";
const LEGACY_PACKAGES_DIRECTORY: &str = "packages";
const LEGACY_STAGING_DIRECTORY: &str = "staging";
const INCOMING_DIRECTORY: &str = "_incoming";
const LEGACY_BACKUP_DIRECTORY: &str = "backup";
const STATE_FILE: &str = "state.json";
const MANIFEST_FILE: &str = "kerminal-plugin.json";
const DATA_DIRECTORY: &str = "data";
const LOGS_DIRECTORY: &str = "logs";
const STAGING_DIRECTORY: &str = ".staging";
const BACKUP_DIRECTORY: &str = ".backup";
const MIGRATION_DIRECTORY: &str = ".migration";
const JOURNAL_FILE: &str = "journal.json";
const BEFORE_DIRECTORY: &str = "before";
const STATE_STAGE_FILE: &str = "state.json.staged";
const MIGRATION_JOURNAL_KIND: &str = "plugin_layout_migration";
const MIGRATION_JOURNAL_SCHEMA: u32 = 1;
const LEGACY_STATE_SCHEMA: u32 = 1;
const CURRENT_STATE_SCHEMA: u32 = 2;
const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_SCHEMA_BYTES: u64 = 1024 * 1024;
const MAX_JOURNAL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PLUGINS: usize = 4096;
const MAX_PENDING: usize = 16384;
/// 丢弃旧安装/更新/回退意图时写固定失败记录，用户可据同一服务重新发起操作。
const DISCARDED_PENDING_FAILURE: &str = "升级后请重新执行该操作";

/// 分类结果直接决定迁移屏障是否放行；不能把未知材料混入 fresh/current。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginLayoutClass {
    Fresh,
    Current,
    Schema1Legacy,
    Schema1Empty,
    Residue,
    Recovery,
}

/// 只读分类携带固定安全分类，不回传插件路径或清单正文。
pub struct PluginLayoutInspection {
    pub class: PluginLayoutClass,
    pub blockers: Vec<DataMigrationBlocker>,
    pub notices: Vec<DataMigrationNotice>,
    pub migration_pending: bool,
    /// 只有 schema 1 自动迁移时才出现；桌面入口据此准备只读主机 codec。
    pub(crate) legacy: Option<LegacyInventory>,
}

impl PluginLayoutInspection {
    /// 数据迁移预检需要单独判断插件屏障，不能只读完旧状态就放行普通启动。
    pub fn readiness(&self) -> PluginLayoutReadiness {
        if !self.blockers.is_empty() {
            return PluginLayoutReadiness::Blocked;
        }
        match self.class {
            PluginLayoutClass::Fresh => PluginLayoutReadiness::Fresh,
            PluginLayoutClass::Current => {
                if self.migration_pending {
                    PluginLayoutReadiness::RecoveryPending
                } else {
                    PluginLayoutReadiness::Current
                }
            }
            PluginLayoutClass::Schema1Legacy | PluginLayoutClass::Schema1Empty => {
                PluginLayoutReadiness::Legacy
            }
            PluginLayoutClass::Recovery => PluginLayoutReadiness::RecoveryPending,
            PluginLayoutClass::Residue => PluginLayoutReadiness::Blocked,
        }
    }

    /// 插件布局仍有待执行或恢复的迁移时才阻止当前格式快路径。
    pub fn requires_migration(&self) -> bool {
        matches!(
            self.class,
            PluginLayoutClass::Schema1Legacy | PluginLayoutClass::Schema1Empty
        ) || self.migration_pending
    }
}

/// 只读旧清单输出；目录只指向活动版本，不执行插件代码也不写状态。
pub struct LegacyPluginDeclaration {
    pub id: PluginId,
    pub version: PluginVersion,
    pub directory: PathBuf,
    pub name: String,
    pub enabled: bool,
    pub host_types: Vec<LegacyHostTypeDeclaration>,
}

/// 旧插件主机 codec 的冻结输入；schema 在读取时已编译校验。
pub struct LegacyHostTypeDeclaration {
    pub id: String,
    pub title: String,
    pub configuration_type: String,
    pub schema: ConfigurationSchema,
}

/// 旧状态只在本模块解析；schema 1 与 schema 2 字段分开，避免缺失字段被默认值掩盖。
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyStoreState {
    schema: u32,
    revision: u64,
    plugins: BTreeMap<PluginId, LegacyInstalledPlugin>,
    #[serde(default)]
    pending: Vec<PendingPluginChange>,
    #[serde(default)]
    failures: BTreeMap<PluginId, String>,
    #[serde(default)]
    pending_failures: BTreeSet<PluginId>,
    #[serde(default)]
    boot: Option<PluginBootMarker>,
}

/// 旧安装记录缺少 schema 2 的摘要/状态/确认字段；后来补写的字段仍需识别，
/// 否则 `deny_unknown_fields` 会把可迁移的旧状态误判为损坏。
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyInstalledPlugin {
    active: PluginVersion,
    #[serde(default)]
    previous: Option<PluginVersion>,
    enabled: bool,
    source: PluginSource,
    installed_at: String,
    #[serde(default)]
    #[allow(dead_code)]
    fingerprint: Option<PluginFingerprint>,
    #[serde(default)]
    #[allow(dead_code)]
    status: Option<PluginInstallStatus>,
    #[serde(default)]
    #[allow(dead_code)]
    confirmed: Option<bool>,
}

/// 清单探测保留脚本标记与已校验主机 schema，迁移和只读目录共用同一读取结论。
#[derive(Clone, Debug)]
struct PackageProbe {
    script: bool,
    name: String,
    host_types: Vec<HostTypeProbe>,
}

#[derive(Clone, Debug)]
struct HostTypeProbe {
    id: String,
    title: String,
    configuration_type: String,
    schema: ConfigurationSchema,
}

/// 分类阶段已核对状态、目录身份和清单；迁移阶段只再计算目录摘要并执行事务。
struct LegacyPackage {
    id: PluginId,
    active: PluginVersion,
    /// 只有 previous 目录确实存在才算被本次备份移除，缺失时报告不虚增。
    previous_present: bool,
    enabled: bool,
    source: PluginSource,
    installed_at: String,
    directory: Option<PathBuf>,
    probe: Option<PackageProbe>,
}

/// 旧树分类结果只作为同一锁内迁移输入，不跨越预检到 apply 的边界。
pub(crate) struct LegacyInventory {
    state: LegacyStoreState,
    state_sha256: String,
    packages: Vec<LegacyPackage>,
    packages_present: bool,
}

/// 持久 journal 的迁移阶段只允许单向前进；恢复依据阶段与回读证据决定动作。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PluginMigrationPhase {
    Prepared,
    OldMoved,
    NewInstalled,
    StateCommitted,
    Committed,
    Cleaned,
}

/// 目录事务的固定证据；相对路径只允许普通段并强制留在插件根内。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PluginMigrationEntry {
    id: PluginId,
    version: PluginVersion,
    /// 备份根内的 `packages/<id>/<version>`；目录已缺失时为空。
    source: String,
    /// 插件根内的目标目录，固定为 `<id>`。
    target: String,
    fingerprint: Option<PluginFingerprint>,
    status: PluginInstallStatus,
}

/// 独立 journal 记录完整迁移计划；不复用安装/更新/卸载事务外形，避免阶段语义混淆。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PluginMigrationJournal {
    kind: String,
    schema: u32,
    operation_id: String,
    phase: PluginMigrationPhase,
    backup_relative: String,
    legacy_state_sha256: String,
    target_state: PluginStoreState,
    target_state_sha256: String,
    packages_present: bool,
    packages_fingerprint: Option<PluginFingerprint>,
    entries: Vec<PluginMigrationEntry>,
    removed_previous_versions: usize,
    discarded_pending: usize,
}

/// `.migration` 目录同时被启动安装事务使用；扫描先区分两种 journal，未知内容阻断。
#[derive(Default)]
struct JournalScan {
    migration: Option<PluginMigrationJournal>,
    boot_journals: usize,
    blockers: Vec<DataMigrationBlocker>,
}

/// 只读入口：不创建根目录、锁、日志或状态；供数据迁移预检和只读 catalogs 共用。
pub fn inspect(root: &Path) -> AppResult<PluginLayoutInspection> {
    let plugins = root.join(PLUGINS_DIRECTORY);
    let metadata = match fs::symlink_metadata(&plugins) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(inspection(
                PluginLayoutClass::Fresh,
                Vec::new(),
                Vec::new(),
                false,
                None,
            ));
        }
        Err(source) => return Err(io_error("检查插件目录失败", source)),
    };
    if !metadata.is_dir() || !layout::safe_metadata(&metadata) {
        return Ok(inspection(
            PluginLayoutClass::Recovery,
            vec![DataMigrationBlocker::PluginLayoutUnsupportedMaterial],
            Vec::new(),
            false,
            None,
        ));
    }

    let entries = list_entries(&plugins)?;
    if entries.is_empty() {
        return Ok(inspection(
            PluginLayoutClass::Fresh,
            Vec::new(),
            Vec::new(),
            false,
            None,
        ));
    }

    let journals = scan_migration_materials(&plugins)?;
    if !journals.blockers.is_empty() {
        return Ok(inspection(
            PluginLayoutClass::Recovery,
            journals.blockers,
            Vec::new(),
            false,
            None,
        ));
    }
    let migration_pending = journals.migration.is_some();

    let state = read_state_probe(&plugins.join(STATE_FILE))?;
    match state {
        StateProbe::Missing => {
            if migration_pending {
                return Ok(inspection(
                    PluginLayoutClass::Recovery,
                    Vec::new(),
                    vec![DataMigrationNotice::PluginLayoutMigrationResume],
                    true,
                    None,
                ));
            }
            // 0.4 的目录客户端在首次刷新时就可能建好缓存目录，而插件状态要到第一次
            // 安装或启用才写出；只有迁移控制目录和产品缓存目录时仍应视为 fresh。
            // 缓存被替换成文件、链接或重解析点时仍按未知材料阻断，绝不顺着它读写。
            let mut only_control_directories = true;
            let mut unsafe_catalog_cache = false;
            for (name, metadata) in &entries {
                match name.as_str() {
                    MIGRATION_DIRECTORY => {}
                    layout::CATALOG_CACHE_DIRECTORY => {
                        unsafe_catalog_cache |= !catalog_cache_is_safe(metadata);
                    }
                    _ => only_control_directories = false,
                }
            }
            if only_control_directories {
                let (class, blockers) = if unsafe_catalog_cache {
                    (
                        PluginLayoutClass::Recovery,
                        vec![DataMigrationBlocker::PluginLayoutUnsupportedMaterial],
                    )
                } else {
                    (PluginLayoutClass::Fresh, Vec::new())
                };
                return Ok(inspection(class, blockers, Vec::new(), false, None));
            }
            Ok(inspection(
                PluginLayoutClass::Recovery,
                vec![DataMigrationBlocker::PluginLayoutResidualMaterial],
                Vec::new(),
                false,
                None,
            ))
        }
        StateProbe::Invalid(schema) => Ok(inspection(
            PluginLayoutClass::Recovery,
            vec![DataMigrationBlocker::PluginLayoutStateInvalid { schema }],
            Vec::new(),
            migration_pending,
            None,
        )),
        StateProbe::Current(state) => {
            let outcome = classify_current(&plugins, &entries, &journals, state)?;
            Ok(inspection(
                outcome.class,
                outcome.blockers,
                outcome.notices,
                migration_pending,
                None,
            ))
        }
        StateProbe::Legacy(state) => {
            if migration_pending {
                return Ok(inspection(
                    PluginLayoutClass::Recovery,
                    Vec::new(),
                    vec![DataMigrationNotice::PluginLayoutMigrationResume],
                    true,
                    None,
                ));
            }
            let outcome = classify_legacy(&plugins, &entries, &journals, state)?;
            Ok(inspection(
                outcome.class,
                outcome.blockers,
                outcome.notices,
                migration_pending,
                outcome.legacy,
            ))
        }
    }
}

/// 只读旧清单：schema 2 走静态快照；schema 1 只读活动目录与 host schema，不写状态。
pub fn legacy_declarations(root: &Path) -> AppResult<Option<Vec<LegacyPluginDeclaration>>> {
    let inspection = inspect(root)?;
    if !inspection.blockers.is_empty() {
        return Err(AppError::conflict(
            "插件布局存在阻断项，无法准备只读主机目录",
        ));
    }
    let plugins = root.join(PLUGINS_DIRECTORY);
    if !matches!(
        inspection.class,
        PluginLayoutClass::Schema1Legacy | PluginLayoutClass::Schema1Empty
    ) && !inspection.migration_pending
    {
        return Ok(None);
    }

    // 已开始迁移后旧状态可能已在 before/state.json；journal 保存了完整目标状态与来源。
    let (state_plugins, journal) = match inspection.legacy {
        Some(inventory) => (
            inventory
                .state
                .plugins
                .iter()
                .map(|(id, installed)| (id.clone(), installed.active.clone(), installed.enabled))
                .collect::<Vec<_>>(),
            None,
        ),
        None => {
            let journal = scan_migration_materials(&plugins)?
                .migration
                .ok_or_else(|| AppError::conflict("插件布局迁移证据缺失"))?;
            let plugins = journal
                .target_state
                .plugins
                .iter()
                .map(|(id, installed)| (id.clone(), installed.active.clone(), installed.enabled))
                .collect::<Vec<_>>();
            (plugins, Some(journal))
        }
    };

    let mut declarations = Vec::new();
    for (id, version, enabled) in state_plugins {
        let directory = locate_active_directory(root, &plugins, &id, &version, journal.as_ref())?;
        let Some(directory) = directory else {
            continue;
        };
        let probe = probe_package(&directory, &id, &version)?;
        declarations.push(LegacyPluginDeclaration {
            id: id.clone(),
            version: version.clone(),
            directory,
            name: probe.name.clone(),
            enabled,
            host_types: probe
                .host_types
                .into_iter()
                .map(|host| LegacyHostTypeDeclaration {
                    id: host.id,
                    title: host.title,
                    configuration_type: host.configuration_type,
                    schema: host.schema,
                })
                .collect(),
        });
    }
    Ok(Some(declarations))
}

/// 独立入口也取得同一把共享根锁，测试与维护脚本不必依赖桌面 repository。
pub fn migrate(root: &Path) -> AppResult<PluginLayoutReport> {
    let storage = StorageCoordinator::new(root.to_path_buf());
    let _guard = storage
        .lock()
        .map_err(|_| AppError::infrastructure("插件布局迁移存储锁不可用"))?;
    migrate_locked(root)
}

/// 调用方必须已持有共享根锁；锁内重检并先恢复旧 journal，再执行新的 schema 1 转换。
pub fn migrate_locked(root: &Path) -> AppResult<PluginLayoutReport> {
    let inspection = inspect(root)?;
    if !inspection.blockers.is_empty() {
        return Err(AppError::conflict("插件布局预检存在阻断项"));
    }
    let plugins = root.join(PLUGINS_DIRECTORY);
    let scan = scan_migration_materials(&plugins)?;
    if scan.migration.is_some() && scan.boot_journals > 0 {
        return Err(AppError::conflict(
            "插件布局迁移与启动事务同时存在，需要人工恢复",
        ));
    }
    if let Some(mut journal) = scan.migration {
        validate_journal(&plugins, &journal)?;
        // Cleaned 表示清理被中断；删除操作本身可重入，必须继续清理，否则 journal 永久阻止启动。
        advance(root, &plugins, &mut journal)?;
        return Ok(report(
            PluginLayoutOutcome::Recovered,
            &journal,
            Some(journal.backup_relative.clone()),
        ));
    }
    if scan.boot_journals > 0 {
        // 当前格式的安装事务由启动快照恢复；旧格式迁移不能与它共用目录阶段。
        if matches!(
            inspection.class,
            PluginLayoutClass::Schema1Legacy | PluginLayoutClass::Schema1Empty
        ) {
            return Err(AppError::conflict("插件安装事务尚未恢复，不能开始布局迁移"));
        }
    }

    match inspection.class {
        PluginLayoutClass::Fresh | PluginLayoutClass::Current => Ok(report(
            PluginLayoutOutcome::AlreadyCurrent,
            &empty_journal(),
            None,
        )),
        PluginLayoutClass::Schema1Legacy | PluginLayoutClass::Schema1Empty => {
            let inventory = inspection
                .legacy
                .ok_or_else(|| AppError::conflict("插件旧布局分类缺少迁移输入"))?;
            begin_migration(root, &plugins, inventory)
        }
        PluginLayoutClass::Residue | PluginLayoutClass::Recovery => {
            Err(AppError::conflict("插件布局材料不足以安全迁移"))
        }
    }
}

/// 分类只读检查 `.migration` 时同时识别本模块 journal 与启动安装 journal。
///
/// 两者语义不同：前者要搬旧树并写 schema 2，后者是已提交/schema 2 下的安装事务；
/// 混在一起时不能猜测先执行哪一份。
fn scan_migration_materials(plugins: &Path) -> AppResult<JournalScan> {
    let directory = plugins.join(MIGRATION_DIRECTORY);
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(JournalScan::default()),
        Err(source) => return Err(io_error("检查插件迁移目录失败", source)),
    };
    if !metadata.is_dir() || !layout::safe_metadata(&metadata) {
        return Ok(JournalScan {
            blockers: vec![DataMigrationBlocker::PluginLayoutRecoveryRequired],
            ..JournalScan::default()
        });
    }

    let mut scan = JournalScan::default();
    for entry in
        fs::read_dir(&directory).map_err(|source| io_error("读取插件迁移目录失败", source))?
    {
        let entry = entry.map_err(|source| io_error("读取插件迁移目录失败", source))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let journal_path = entry.path().join(JOURNAL_FILE);
        let entry_metadata = fs::symlink_metadata(entry.path())
            .map_err(|source| io_error("检查插件迁移操作目录失败", source))?;
        if !entry_metadata.is_dir()
            || !layout::safe_metadata(&entry_metadata)
            || Uuid::parse_str(&name).is_err()
        {
            scan.blockers
                .push(DataMigrationBlocker::PluginLayoutResidualMaterial);
            continue;
        }
        match read_bounded_regular(&journal_path, MAX_JOURNAL_BYTES) {
            Err(_) => {
                scan.blockers
                    .push(DataMigrationBlocker::PluginLayoutRecoveryRequired);
            }
            Ok(None) => {
                scan.blockers
                    .push(DataMigrationBlocker::PluginLayoutRecoveryRequired);
            }
            Ok(Some(bytes)) => match serde_json::from_slice::<PluginMigrationJournal>(&bytes) {
                Ok(journal) if validate_journal(plugins, &journal).is_ok() => {
                    if journal.operation_id != name
                        || scan.migration.is_some()
                        || scan.boot_journals > 0
                    {
                        scan.blockers
                            .push(DataMigrationBlocker::PluginLayoutRecoveryRequired);
                    } else {
                        scan.migration = Some(journal);
                    }
                }
                _ => match layout::read_journal(&journal_path) {
                    Ok(_journal) if scan.migration.is_none() => {
                        scan.boot_journals += 1;
                    }
                    _ => scan
                        .blockers
                        .push(DataMigrationBlocker::PluginLayoutRecoveryRequired),
                },
            },
        }
    }
    Ok(scan)
}

/// 只把 pending 明确引用的安装槽当已知材料；安装内容仍由启动事务重新校验。
///
/// 正常安装/更新在写 pending 后、下次启动建 journal 前会先留下 `.staging/<id>/<uuid>`；
/// 若此时按残留阻断，启动快照永远无法应用 pending，安装窗口会卡死整台应用。
fn staging_material_is_owned(staging: &Path, state: &PluginStoreState) -> AppResult<bool> {
    if !directory_has_entries(staging)? {
        return Ok(true);
    }
    for (name, metadata) in list_entries(staging)? {
        if !metadata.is_dir() || !layout::safe_metadata(&metadata) {
            return Ok(false);
        }
        if name == INCOMING_DIRECTORY {
            // 解包预热目录在身份确定后已清空；仍有内容说明安装更早中断且无归属证据。
            if directory_has_entries(&staging.join(&name))? {
                return Ok(false);
            }
            continue;
        }
        let Ok(id) = PluginId::parse(&name) else {
            return Ok(false);
        };
        for (slot, slot_metadata) in list_entries(&staging.join(&name))? {
            if !slot_metadata.is_dir() || !layout::safe_metadata(&slot_metadata) {
                return Ok(false);
            }
            let referenced = state.pending.iter().any(|change| match change {
                PendingPluginChange::Install {
                    id: pending,
                    staged,
                    ..
                }
                | PendingPluginChange::Update {
                    id: pending,
                    staged,
                    ..
                } => pending == &id && staged == &slot,
                _ => false,
            });
            if !referenced {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// 目录缓存是产品自建控制目录，不是插件材料；预检只确认目录本身安全。
///
/// 缓存正文由目录客户端在读取时逐文件限制大小并拒绝链接，因此迁移不需要遍历或
/// 解析它，避免一次网络缓存（包括中断留下的临时文件）把正常升级挡在恢复界面。
/// 但目录本身若是指向配置根之外的链接或 Windows 重解析点，必须继续阻断，否则
/// 后续缓存写入会越过数据根。
///
/// @author kongweiguang
fn catalog_cache_is_safe(metadata: &fs::Metadata) -> bool {
    metadata.is_dir() && layout::safe_metadata(metadata)
}

/// 当前 schema 2 只接受已知布局材料；未知文件、孤立 staging/backup 或恢复记录阻断。
fn classify_current(
    plugins: &Path,
    entries: &[(String, fs::Metadata)],
    journals: &JournalScan,
    state: PluginStoreState,
) -> AppResult<ClassOutcome> {
    let mut blockers = Vec::new();
    let mut recovery_record = false;
    let mut staging_residual = false;
    let mut backup_material = false;
    for (name, metadata) in entries {
        if name == STATE_FILE {
            continue;
        }
        if is_recovery_name(name) {
            recovery_record = true;
            continue;
        }
        let safe_dir = metadata.is_dir() && layout::safe_metadata(metadata);
        match name.as_str() {
            DATA_DIRECTORY | LOGS_DIRECTORY => {
                if !safe_dir {
                    blockers.push(DataMigrationBlocker::PluginLayoutUnsupportedMaterial);
                }
            }
            // 0.4 起目录客户端会在插件根下建立缓存；正文由客户端读取时逐文件校验，
            // 预检只确认它本身是安全普通目录，避免任意缓存内容被当成插件材料阻断升级。
            layout::CATALOG_CACHE_DIRECTORY => {
                if !catalog_cache_is_safe(metadata) {
                    blockers.push(DataMigrationBlocker::PluginLayoutUnsupportedMaterial);
                }
            }
            STAGING_DIRECTORY => {
                if !safe_dir {
                    blockers.push(DataMigrationBlocker::PluginLayoutUnsupportedMaterial);
                } else {
                    // 安装先写 pending 再等下次启动建 journal；此窗口内按 pending 归属识别。
                    staging_residual = !staging_material_is_owned(&plugins.join(name), &state)?;
                }
            }
            BACKUP_DIRECTORY => {
                if !safe_dir {
                    blockers.push(DataMigrationBlocker::PluginLayoutUnsupportedMaterial);
                } else {
                    backup_material = directory_has_entries(&plugins.join(name))?;
                }
            }
            MIGRATION_DIRECTORY => {
                if !safe_dir {
                    blockers.push(DataMigrationBlocker::PluginLayoutRecoveryRequired);
                }
            }
            _ => {
                if PluginId::parse(name).is_err() || !safe_dir {
                    blockers.push(DataMigrationBlocker::PluginLayoutUnsupportedMaterial);
                }
            }
        }
    }
    if recovery_record {
        blockers.push(DataMigrationBlocker::PluginLayoutRecoveryRequired);
    }
    // 没有 journal 证明归属的残留材料不能被启动静默删除；pending 引用的安装槽除外。
    if (staging_residual || backup_material) && journals.boot_journals == 0 {
        blockers.push(DataMigrationBlocker::PluginLayoutResidualMaterial);
    }
    if validate_state(&state).is_err() {
        blockers.push(DataMigrationBlocker::PluginLayoutStateInvalid {
            schema: Some(CURRENT_STATE_SCHEMA),
        });
    }
    Ok(ClassOutcome {
        class: if blockers.is_empty() {
            PluginLayoutClass::Current
        } else {
            PluginLayoutClass::Recovery
        },
        blockers,
        notices: Vec::new(),
        legacy: None,
    })
}

/// schema 1 只有与已知 active/previous 布局完全一致才进入自动迁移。
fn classify_legacy(
    plugins: &Path,
    entries: &[(String, fs::Metadata)],
    journals: &JournalScan,
    state: LegacyStoreState,
) -> AppResult<ClassOutcome> {
    let mut blockers = Vec::new();
    let mut residue = false;
    let mut packages_present = false;
    for (name, metadata) in entries {
        if name == STATE_FILE {
            continue;
        }
        let safe_dir = metadata.is_dir() && layout::safe_metadata(metadata);
        match name.as_str() {
            LEGACY_PACKAGES_DIRECTORY => {
                if !safe_dir {
                    blockers.push(DataMigrationBlocker::PluginLayoutUnsupportedMaterial);
                }
                packages_present = true;
            }
            LEGACY_STAGING_DIRECTORY | LEGACY_BACKUP_DIRECTORY => {
                // 旧 staging/backup 的结果未知；没有归属证据时原样保留。
                residue = true;
            }
            DATA_DIRECTORY | LOGS_DIRECTORY => {
                if !safe_dir {
                    blockers.push(DataMigrationBlocker::PluginLayoutUnsupportedMaterial);
                }
            }
            // schema 1 根同样可能已经产生目录缓存；它是产品控制目录而不是旧插件材料，
            // 迁移只移走 packages/state，缓存保留原处即可，不能因此进入残留阻断。
            layout::CATALOG_CACHE_DIRECTORY => {
                if !catalog_cache_is_safe(metadata) {
                    blockers.push(DataMigrationBlocker::PluginLayoutUnsupportedMaterial);
                }
            }
            MIGRATION_DIRECTORY => {
                if !safe_dir || journals.migration.is_none() {
                    residue = true;
                }
            }
            _ => residue = true,
        }
    }
    if residue {
        return Ok(ClassOutcome {
            class: PluginLayoutClass::Residue,
            blockers: vec![DataMigrationBlocker::PluginLayoutResidualMaterial],
            notices: Vec::new(),
            legacy: None,
        });
    }
    if !blockers.is_empty() {
        return Ok(ClassOutcome {
            class: PluginLayoutClass::Recovery,
            blockers,
            notices: Vec::new(),
            legacy: None,
        });
    }
    if state.schema != LEGACY_STATE_SCHEMA {
        return Ok(ClassOutcome {
            class: PluginLayoutClass::Recovery,
            blockers: vec![DataMigrationBlocker::PluginLayoutStateInvalid {
                schema: Some(state.schema),
            }],
            notices: Vec::new(),
            legacy: None,
        });
    }
    if validate_legacy_state(&state).is_err() {
        return Ok(ClassOutcome {
            class: PluginLayoutClass::Recovery,
            blockers: vec![DataMigrationBlocker::PluginLayoutStateInvalid {
                schema: Some(LEGACY_STATE_SCHEMA),
            }],
            notices: Vec::new(),
            legacy: None,
        });
    }

    let package_root = plugins.join(LEGACY_PACKAGES_DIRECTORY);
    let package_entries = if packages_present {
        list_entries(&package_root)?
    } else {
        Vec::new()
    };
    let mut on_disk = BTreeMap::new();
    for (name, metadata) in &package_entries {
        if !metadata.is_dir() || !layout::safe_metadata(metadata) {
            return Ok(recovery_outcome(
                DataMigrationBlocker::PluginLayoutUnsupportedMaterial,
            ));
        }
        let Ok(id) = PluginId::parse(name) else {
            return Ok(recovery_outcome(
                DataMigrationBlocker::PluginLayoutUnsupportedMaterial,
            ));
        };
        if !state.plugins.contains_key(&id) {
            return Ok(recovery_outcome(
                DataMigrationBlocker::PluginLayoutUnsupportedMaterial,
            ));
        }
        on_disk.insert(id, metadata);
    }

    let mut packages = Vec::new();
    let mut script_count = 0usize;
    for (id, installed) in &state.plugins {
        let directory = package_root.join(id.as_str());
        let versions = if on_disk.contains_key(id) {
            list_entries(&directory)?
        } else {
            Vec::new()
        };
        let mut version_dirs = BTreeMap::new();
        for (name, metadata) in &versions {
            if !metadata.is_dir() || !layout::safe_metadata(metadata) {
                return Ok(recovery_outcome(
                    DataMigrationBlocker::PluginLayoutUnsupportedMaterial,
                ));
            }
            let Ok(version) = PluginVersion::parse(name) else {
                return Ok(recovery_outcome(
                    DataMigrationBlocker::PluginLayoutUnsupportedMaterial,
                ));
            };
            version_dirs.insert(version, metadata);
        }
        // 已知 v1 只允许 active 与一个 previous；额外版本属于未归类材料。
        let allowed = |version: &PluginVersion| {
            version == &installed.active || installed.previous.as_ref() == Some(version)
        };
        if version_dirs.keys().any(|version| !allowed(version)) {
            return Ok(recovery_outcome(
                DataMigrationBlocker::PluginLayoutUnsupportedMaterial,
            ));
        }
        let previous_present = installed
            .previous
            .as_ref()
            .is_some_and(|previous| version_dirs.contains_key(previous));
        let (active_directory, probe) = if version_dirs.contains_key(&installed.active) {
            let active_directory = directory.join(installed.active.to_string());
            let probe = match probe_package(&active_directory, id, &installed.active) {
                Ok(probe) => probe,
                Err(_) => {
                    return Ok(recovery_outcome(
                        DataMigrationBlocker::PluginLayoutPackageInvalid,
                    ));
                }
            };
            if probe.script {
                script_count += 1;
            }
            (Some(active_directory), Some(probe))
        } else if !version_dirs.is_empty() {
            // 活动版本缺失但目录里还有其它版本，无法判断旧选择是否完整。
            return Ok(recovery_outcome(
                DataMigrationBlocker::PluginLayoutUnsupportedMaterial,
            ));
        } else {
            (None, None)
        };
        packages.push(LegacyPackage {
            id: id.clone(),
            active: installed.active.clone(),
            previous_present,
            enabled: installed.enabled,
            source: installed.source,
            installed_at: installed.installed_at.clone(),
            directory: active_directory,
            probe,
        });
    }

    let state_bytes = read_bounded_regular(&plugins.join(STATE_FILE), MAX_STATE_BYTES)?
        .ok_or_else(|| AppError::conflict("插件旧状态在分类期间消失"))?;
    let legacy = LegacyInventory {
        state,
        state_sha256: journal::sha256(&state_bytes),
        packages,
        packages_present,
    };
    let class = if legacy.packages.is_empty()
        && !legacy.packages_present
        && legacy.state.pending.is_empty()
    {
        PluginLayoutClass::Schema1Empty
    } else {
        PluginLayoutClass::Schema1Legacy
    };
    let mut notices = vec![DataMigrationNotice::PluginLayoutLegacyMigration];
    if script_count > 0 {
        notices.push(DataMigrationNotice::PluginLayoutScriptPackages {
            count: script_count,
        });
    }
    Ok(ClassOutcome {
        class,
        blockers: Vec::new(),
        notices,
        legacy: Some(legacy),
    })
}

/// 开始新迁移前先完整规划目标状态、摘要与 journal；任何目录写入都在 journal 之后。
fn begin_migration(
    root: &Path,
    plugins: &Path,
    inventory: LegacyInventory,
) -> AppResult<PluginLayoutReport> {
    let mut journal_value = plan_migration(plugins, inventory)?;
    advance(root, plugins, &mut journal_value)?;
    Ok(report(
        PluginLayoutOutcome::Migrated,
        &journal_value,
        Some(journal_value.backup_relative.clone()),
    ))
}

/// 规划与首次 journal 写入单独成步，使中断恢复测试能在任何阶段冻结同一份计划。
fn plan_migration(plugins: &Path, inventory: LegacyInventory) -> AppResult<PluginMigrationJournal> {
    let operation_id = Uuid::new_v4().to_string();
    let operation_relative = format!("{PLUGINS_DIRECTORY}/{MIGRATION_DIRECTORY}/{operation_id}");
    let backup_relative = format!("{operation_relative}/{BEFORE_DIRECTORY}");

    let mut entries = Vec::with_capacity(inventory.packages.len());
    let mut installed = BTreeMap::new();
    let mut removed_previous_versions = 0usize;
    for package in &inventory.packages {
        let fingerprint = match &package.directory {
            Some(directory) => Some(
                layout::fingerprint(directory)
                    .map_err(|_| AppError::conflict("插件包内容无法安全摘要，已保留原目录"))?,
            ),
            None => None,
        };
        let status = match &package.probe {
            Some(probe) if probe.script => PluginInstallStatus::Unsupported {
                reason: PluginUnsupportedReason::ScriptRemoved,
            },
            Some(_) => PluginInstallStatus::Ready,
            None => PluginInstallStatus::Missing,
        };
        if package.previous_present {
            removed_previous_versions += 1;
        }
        let record = InstalledPlugin {
            active: package.active.clone(),
            previous: None,
            enabled: package.enabled,
            source: package.source,
            installed_at: if package.installed_at.is_empty() {
                timestamp_now()
            } else {
                package.installed_at.clone()
            },
            fingerprint: fingerprint.clone(),
            status,
            confirmed: match package.source {
                PluginSource::DropIn => package.enabled,
                PluginSource::Official | PluginSource::Local => true,
            },
        };
        installed.insert(package.id.clone(), record);
        entries.push(PluginMigrationEntry {
            id: package.id.clone(),
            version: package.active.clone(),
            source: package
                .directory
                .as_ref()
                .map(|_| {
                    format!(
                        "{LEGACY_PACKAGES_DIRECTORY}/{}/{}",
                        package.id, package.active
                    )
                })
                .unwrap_or_default(),
            target: package.id.storage_segment().to_owned(),
            fingerprint,
            status,
        });
    }

    let (pending, discarded_pending) = convert_pending(&inventory.state);
    let mut failures = inventory.state.failures.clone();
    for change in &inventory.state.pending {
        if is_discarded_change(change) {
            failures.insert(
                change.plugin_id().clone(),
                DISCARDED_PENDING_FAILURE.to_owned(),
            );
        }
    }
    let mut pending_failures = inventory.state.pending_failures.clone();
    pending_failures.retain(|id| {
        failures.contains_key(id) && pending.iter().any(|change| change.plugin_id() == id)
    });
    let target_state = PluginStoreState {
        schema: CURRENT_STATE_SCHEMA,
        revision: inventory.state.revision,
        plugins: installed,
        builtins: BTreeMap::new(),
        pending,
        failures,
        pending_failures,
        boot: None,
    };
    validate_state(&target_state)
        .map_err(|_| AppError::conflict("插件迁移目标状态不符合 schema 2，已保留旧状态"))?;
    let target_state_bytes = state_bytes(&target_state)?;

    // 分类已确认 packages 是安全目录；迁移前必须冻结整棵旧树摘要，恢复阶段据此判断移动结果。
    let packages_fingerprint = if inventory.packages_present {
        Some(
            layout::fingerprint(&plugins.join(LEGACY_PACKAGES_DIRECTORY))
                .map_err(|_| AppError::conflict("旧插件包树无法安全摘要"))?,
        )
    } else {
        None
    };
    let journal_value = PluginMigrationJournal {
        kind: MIGRATION_JOURNAL_KIND.to_owned(),
        schema: MIGRATION_JOURNAL_SCHEMA,
        operation_id: operation_id.clone(),
        phase: PluginMigrationPhase::Prepared,
        backup_relative,
        legacy_state_sha256: inventory.state_sha256.clone(),
        target_state,
        target_state_sha256: journal::sha256(&target_state_bytes),
        packages_present: inventory.packages_present,
        packages_fingerprint,
        entries,
        removed_previous_versions,
        discarded_pending,
    };
    validate_journal(plugins, &journal_value)?;
    write_journal(plugins, &journal_value, true)?;
    Ok(journal_value)
}

/// pending 只保留重启后仍可安全执行的外部插件启停与卸载；安装/更新/回退需要重新发起。
fn convert_pending(state: &LegacyStoreState) -> (Vec<PendingPluginChange>, usize) {
    let mut pending = Vec::new();
    let mut discarded = 0usize;
    for change in &state.pending {
        // schema 1 没有内置启停表，保留身份只可能来自同名外部记录；schema 2 的
        // apply_change 会优先按内置单元解释，若照搬会静默改写用户未请求的内置状态。
        if is_reserved_plugin_id(change.plugin_id().as_str()) {
            discarded += 1;
            continue;
        }
        match change {
            PendingPluginChange::Enable { .. } | PendingPluginChange::Disable { .. } => {
                pending.push(change.clone());
            }
            PendingPluginChange::Uninstall { id, operation_id } => {
                let operation_id = if Uuid::parse_str(operation_id).is_ok() {
                    operation_id.clone()
                } else {
                    Uuid::new_v4().to_string()
                };
                pending.push(PendingPluginChange::Uninstall {
                    id: id.clone(),
                    operation_id,
                });
            }
            PendingPluginChange::Install { .. }
            | PendingPluginChange::Update { .. }
            | PendingPluginChange::Rollback { .. } => discarded += 1,
        }
    }
    (pending, discarded)
}

/// 丢弃范围与文档一致；保留 pending_failures 前也复用同一判断。
fn is_discarded_change(change: &PendingPluginChange) -> bool {
    matches!(
        change,
        PendingPluginChange::Install { .. }
            | PendingPluginChange::Update { .. }
            | PendingPluginChange::Rollback { .. }
    )
}

/// 按持久阶段前滚；阶段先写入 journal，任何未知结果都先回读。
fn advance(
    root: &Path,
    plugins: &Path,
    journal_value: &mut PluginMigrationJournal,
) -> AppResult<()> {
    let backup = layout::resolve_relative(root, &backup_relative_in_plugins(journal_value)?)
        .map_err(layout_error)?;
    loop {
        match journal_value.phase {
            PluginMigrationPhase::Prepared => {
                layout::ensure_directory(
                    root,
                    &[
                        PLUGINS_DIRECTORY,
                        MIGRATION_DIRECTORY,
                        &journal_value.operation_id,
                    ],
                )
                .map_err(layout_error)?;
                journal_value.phase = PluginMigrationPhase::OldMoved;
                write_journal(plugins, journal_value, false)?;
            }
            PluginMigrationPhase::OldMoved => {
                move_old_tree(plugins, &backup, journal_value)?;
                journal_value.phase = PluginMigrationPhase::NewInstalled;
                write_journal(plugins, journal_value, false)?;
            }
            PluginMigrationPhase::NewInstalled => {
                move_active_packages(plugins, &backup, journal_value)?;
                journal_value.phase = PluginMigrationPhase::StateCommitted;
                write_journal(plugins, journal_value, false)?;
            }
            PluginMigrationPhase::StateCommitted => {
                commit_state(plugins, journal_value)?;
                journal_value.phase = PluginMigrationPhase::Committed;
                write_journal(plugins, journal_value, false)?;
            }
            PluginMigrationPhase::Committed => {
                verify_committed(plugins, journal_value)?;
                journal_value.phase = PluginMigrationPhase::Cleaned;
                write_journal(plugins, journal_value, false)?;
            }
            PluginMigrationPhase::Cleaned => {
                cleanup(plugins, &backup, journal_value)?;
                return Ok(());
            }
        }
    }
}

/// 备份整棵旧 packages、旧 staging 与旧 state；每项用分类时冻结的摘要核对。
fn move_old_tree(
    plugins: &Path,
    backup: &Path,
    journal_value: &PluginMigrationJournal,
) -> AppResult<()> {
    layout::ensure_directory(
        plugins,
        &[
            MIGRATION_DIRECTORY,
            &journal_value.operation_id,
            BEFORE_DIRECTORY,
        ],
    )
    .map_err(layout_error)?;
    if journal_value.packages_present {
        let fingerprint = journal_value
            .packages_fingerprint
            .as_ref()
            .ok_or_else(|| AppError::conflict("插件迁移缺少旧包树摘要"))?;
        move_directory_verified(
            &plugins.join(LEGACY_PACKAGES_DIRECTORY),
            &backup.join(LEGACY_PACKAGES_DIRECTORY),
            fingerprint,
            &journal_value.operation_id,
        )?;
    }
    move_file_verified(
        &plugins.join(STATE_FILE),
        &backup.join(STATE_FILE),
        &journal_value.legacy_state_sha256,
    )
}

/// 活动版本目录从备份树移入单目录目标；缺失记录只确认目标确实不存在。
fn move_active_packages(
    plugins: &Path,
    backup: &Path,
    journal_value: &PluginMigrationJournal,
) -> AppResult<()> {
    for entry in &journal_value.entries {
        let target = layout::resolve_relative(plugins, &entry.target).map_err(layout_error)?;
        match (&entry.fingerprint, entry.source.is_empty()) {
            (Some(fingerprint), false) => {
                let source =
                    layout::resolve_relative(backup, &entry.source).map_err(layout_error)?;
                move_directory_verified(
                    &source,
                    &target,
                    fingerprint,
                    &journal_value.operation_id,
                )?;
            }
            (None, true) => {
                if layout::exists(&target).map_err(layout_error)? {
                    return Err(AppError::conflict(
                        "插件记录为缺失但目标目录已存在，保留现场等待人工核对",
                    ));
                }
            }
            _ => {
                return Err(AppError::conflict("插件迁移 journal 的目录证据不完整"));
            }
        }
    }
    Ok(())
}

/// schema 2 状态使用同目录 staging 原子创建；已存在的目标必须与 journal 摘要一致。
fn commit_state(plugins: &Path, journal_value: &PluginMigrationJournal) -> AppResult<()> {
    let bytes = serde_json::to_vec_pretty(&journal_value.target_state)
        .map_err(|_| AppError::infrastructure("插件迁移目标状态序列化失败"))?;
    if journal::sha256(&bytes) != journal_value.target_state_sha256 {
        return Err(AppError::conflict("插件迁移目标状态与 journal 摘要不一致"));
    }
    let target = plugins.join(STATE_FILE);
    if layout::exists(&target).map_err(layout_error)? {
        let actual = read_bounded_regular(&target, MAX_STATE_BYTES)?
            .ok_or_else(|| AppError::conflict("插件状态在提交前消失"))?;
        if journal::sha256(&actual) != journal_value.target_state_sha256 {
            return Err(AppError::conflict("插件状态已存在但内容与迁移计划不一致"));
        }
        return Ok(());
    }
    let directory = plugins
        .join(MIGRATION_DIRECTORY)
        .join(&journal_value.operation_id);
    let stage = directory.join(STATE_STAGE_FILE);
    match read_bounded_regular(&stage, MAX_STATE_BYTES)? {
        Some(existing) if journal::sha256(&existing) == journal_value.target_state_sha256 => {}
        Some(_) => return Err(AppError::conflict("插件状态 staging 与 journal 不一致")),
        None => journal::write_new_file(plugins, &stage, &bytes)?,
    }
    // 状态 staging 位于操作目录而目标在插件根；MoveStagedNoReplace 允许跨父目录且先同步内容。
    match commit_staged_file(&stage, &target, CommitMode::MoveStagedNoReplace) {
        Ok(()) => {}
        Err(AtomicCommitError::CommitUnknown(source)) => {
            // 只有回读到与计划一致的目标才能吸收未知结果；缺失或不一致都保留原始原因。
            match read_bounded_regular(&target, MAX_STATE_BYTES)? {
                Some(actual) if journal::sha256(&actual) == journal_value.target_state_sha256 => {}
                _ => {
                    return Err(AppError::outcome_unknown_with_source(
                        "插件状态提交结果未知",
                        source,
                    ));
                }
            }
        }
        Err(AtomicCommitError::NotCommitted(source)) => {
            return Err(io_error("插件状态原子提交失败", source));
        }
    }
    let actual = read_bounded_regular(&target, MAX_STATE_BYTES)?
        .ok_or_else(|| AppError::outcome_unknown("插件状态提交后回读缺失"))?;
    if journal::sha256(&actual) != journal_value.target_state_sha256 {
        return Err(AppError::outcome_unknown("插件状态提交后回读摘要不一致"));
    }
    Ok(())
}

/// 清理前重新确认每个活动目录和权威状态；只有全部可证明已提交才删除备份。
fn verify_committed(plugins: &Path, journal_value: &PluginMigrationJournal) -> AppResult<()> {
    let actual = read_bounded_regular(&plugins.join(STATE_FILE), MAX_STATE_BYTES)?
        .ok_or_else(|| AppError::conflict("插件迁移提交后状态缺失"))?;
    if journal::sha256(&actual) != journal_value.target_state_sha256 {
        return Err(AppError::conflict("插件迁移提交后状态摘要不一致"));
    }
    for entry in &journal_value.entries {
        let target = layout::resolve_relative(plugins, &entry.target).map_err(layout_error)?;
        match &entry.fingerprint {
            Some(fingerprint) => {
                if !layout::matches_fingerprint(&target, fingerprint).map_err(layout_error)? {
                    return Err(AppError::conflict("插件迁移目标目录与 journal 摘要不一致"));
                }
            }
            None => {
                if layout::exists(&target).map_err(layout_error)? {
                    return Err(AppError::conflict("插件迁移缺失记录对应的目标已出现"));
                }
            }
        }
    }
    Ok(())
}

/// 已证明提交后删除本事务备份与 journal；先删备份，再删操作目录并收掉空的 `.migration`。
fn cleanup(plugins: &Path, backup: &Path, journal_value: &PluginMigrationJournal) -> AppResult<()> {
    layout::remove_tree_verified(backup).map_err(layout_error)?;
    let operation = plugins
        .join(MIGRATION_DIRECTORY)
        .join(&journal_value.operation_id);
    layout::remove_tree_verified(&operation).map_err(layout_error)?;
    let migration = plugins.join(MIGRATION_DIRECTORY);
    if let Ok(metadata) = fs::symlink_metadata(&migration)
        && metadata.is_dir()
        && layout::safe_metadata(&metadata)
        && fs::read_dir(&migration)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false)
    {
        let _ = fs::remove_dir(&migration);
    }
    Ok(())
}

/// 目录移动先核对源摘要；结果未知时回读源/目标，证据不足不继续下一步。
fn move_directory_verified(
    source: &Path,
    target: &Path,
    expected: &PluginFingerprint,
    operation_id: &str,
) -> AppResult<()> {
    let source_exists = layout::exists(source).map_err(layout_error)?;
    let target_exists = layout::exists(target).map_err(layout_error)?;
    match (source_exists, target_exists) {
        (false, true) => {
            if !layout::matches_fingerprint(target, expected).map_err(layout_error)? {
                return Err(AppError::conflict("插件迁移目标内容与 journal 不一致"));
            }
            Ok(())
        }
        (true, false) => {
            if !layout::matches_fingerprint(source, expected).map_err(layout_error)? {
                return Err(AppError::conflict("插件迁移源内容与 journal 不一致"));
            }
            match layout::move_directory(source, target, operation_id) {
                Ok(()) => {}
                Err(LayoutError::OutcomeUnknown {
                    source: io_source, ..
                }) => {
                    let moved = !layout::exists(source).map_err(layout_error)?
                        && layout::exists(target).map_err(layout_error)?
                        && layout::matches_fingerprint(target, expected).map_err(layout_error)?;
                    if !moved {
                        return Err(AppError::outcome_unknown_with_source(
                            "插件目录移动结果未知",
                            io_source,
                        ));
                    }
                }
                Err(error) => return Err(layout_error(error)),
            }
            if !layout::matches_fingerprint(target, expected).map_err(layout_error)? {
                return Err(AppError::outcome_unknown("插件目录移动后摘要不一致"));
            }
            Ok(())
        }
        (true, true) => Err(AppError::conflict(
            "插件迁移源与目标同时存在，保留现场等待人工核对",
        )),
        (false, false) => Err(AppError::outcome_unknown(
            "插件迁移源与目标均缺失，无法证明移动结果",
        )),
    }
}

/// 普通文件移动回读完整 SHA-256；状态原文是迁移后仍可人工核对的唯一基线。
fn move_file_verified(source: &Path, target: &Path, expected_sha256: &str) -> AppResult<()> {
    let source_hash =
        read_bounded_regular(source, MAX_STATE_BYTES)?.map(|bytes| journal::sha256(&bytes));
    let target_hash =
        read_bounded_regular(target, MAX_STATE_BYTES)?.map(|bytes| journal::sha256(&bytes));
    match (source_hash, target_hash) {
        (None, Some(actual)) if actual == expected_sha256 => Ok(()),
        (Some(actual), None) if actual == expected_sha256 => {
            match commit_staged_file(source, target, CommitMode::MoveNoReplace) {
                Ok(()) => {}
                Err(AtomicCommitError::CommitUnknown(source_error)) => {
                    let target_hash = read_bounded_regular(target, MAX_STATE_BYTES)?
                        .map(|bytes| journal::sha256(&bytes));
                    if target_hash.as_deref() != Some(expected_sha256) {
                        return Err(AppError::outcome_unknown_with_source(
                            "插件旧状态备份结果未知",
                            source_error,
                        ));
                    }
                }
                Err(AtomicCommitError::NotCommitted(source_error)) => {
                    return Err(io_error("插件旧状态备份失败", source_error));
                }
            }
            Ok(())
        }
        (Some(_), Some(_)) => Err(AppError::conflict(
            "插件旧状态与备份同时存在，保留现场等待人工核对",
        )),
        (None, None) => Err(AppError::outcome_unknown(
            "插件旧状态与备份均缺失，无法证明备份结果",
        )),
        _ => Err(AppError::conflict("插件旧状态内容与 journal 摘要不一致")),
    }
}

/// journal 写入先落 staging 再原子发布；未知提交通过回读同一个 journal 判定。
fn write_journal(
    plugins: &Path,
    journal_value: &PluginMigrationJournal,
    create: bool,
) -> AppResult<()> {
    validate_journal(plugins, journal_value)?;
    let directory = plugins
        .join(MIGRATION_DIRECTORY)
        .join(&journal_value.operation_id);
    layout::ensure_directory(plugins, &[MIGRATION_DIRECTORY, &journal_value.operation_id])
        .map_err(layout_error)?;
    let target = directory.join(JOURNAL_FILE);
    let stage = directory.join(format!(".journal-{}.tmp", Uuid::new_v4().simple()));
    let bytes = serde_json::to_vec_pretty(journal_value)
        .map_err(|_| AppError::infrastructure("插件迁移 journal 序列化失败"))?;
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(AppError::conflict("插件迁移 journal 超过限额"));
    }
    journal::write_new_file(plugins, &stage, &bytes)?;
    let mode = if create {
        CommitMode::CreateNew
    } else {
        CommitMode::ReplaceExisting
    };
    match commit_staged_file(&stage, &target, mode) {
        Ok(()) => Ok(()),
        Err(AtomicCommitError::CommitUnknown(source)) => {
            let actual = read_bounded_regular(&target, MAX_JOURNAL_BYTES)?
                .and_then(|bytes| serde_json::from_slice::<PluginMigrationJournal>(&bytes).ok())
                .is_some_and(|actual| same_journal(&actual, journal_value));
            if actual {
                Ok(())
            } else {
                Err(AppError::outcome_unknown_with_source(
                    "插件迁移 journal 提交结果未知",
                    source,
                ))
            }
        }
        Err(AtomicCommitError::NotCommitted(source)) => {
            let _ = fs::remove_file(&stage);
            Err(io_error("插件迁移 journal 提交失败", source))
        }
    }
}

/// journal 只接受固定 kind、UUID 操作身份和受控相对路径；未知阶段/字段不参与恢复。
fn validate_journal(plugins: &Path, journal_value: &PluginMigrationJournal) -> AppResult<()> {
    if journal_value.kind != MIGRATION_JOURNAL_KIND
        || journal_value.schema != MIGRATION_JOURNAL_SCHEMA
        || Uuid::parse_str(&journal_value.operation_id).is_err()
        || journal_value.entries.len() > MAX_PLUGINS
        || validate_state(&journal_value.target_state).is_err()
        || journal::sha256(&state_bytes(&journal_value.target_state)?)
            != journal_value.target_state_sha256
    {
        return Err(AppError::conflict("插件迁移 journal 内容无效"));
    }
    let expected_backup = backup_relative_in_plugins(journal_value)?;
    if journal_value.backup_relative != expected_backup {
        return Err(AppError::conflict(
            "插件迁移 journal 备份路径与操作身份不一致",
        ));
    }
    let mut seen = BTreeSet::new();
    for entry in &journal_value.entries {
        // 缺失记录没有可搬迁目录，允许空 source；其余记录必须精确指向旧包自身，
        // 否则恢复阶段可能把错误目录当成证据。空 source 绝不允许携带摘要。
        let source_ok = if entry.source.is_empty() {
            entry.fingerprint.is_none() && matches!(entry.status, PluginInstallStatus::Missing)
        } else {
            entry.fingerprint.is_some()
                && !matches!(entry.status, PluginInstallStatus::Missing)
                && entry.source
                    == format!("{LEGACY_PACKAGES_DIRECTORY}/{}/{}", entry.id, entry.version)
        };
        if !seen.insert(entry.id.clone())
            || entry.target != entry.id.storage_segment()
            || !source_ok
        {
            return Err(AppError::conflict("插件迁移 journal 目录证据无效"));
        }
        layout::resolve_relative(plugins, &entry.target).map_err(layout_error)?;
        if !entry.source.is_empty() {
            layout::resolve_relative(
                &plugins
                    .join(MIGRATION_DIRECTORY)
                    .join(&journal_value.operation_id)
                    .join(BEFORE_DIRECTORY),
                &entry.source,
            )
            .map_err(layout_error)?;
        }
    }
    Ok(())
}

/// 只比较规范序列化结果，避免字段顺序或空白差异掩盖真实恢复差异。
fn same_journal(left: &PluginMigrationJournal, right: &PluginMigrationJournal) -> bool {
    serde_json::to_vec(left).ok() == serde_json::to_vec(right).ok()
}

/// 备份相对路径固定由 plugins 根与操作 UUID 推导，journal 不能自选位置。
fn backup_relative_in_plugins(journal_value: &PluginMigrationJournal) -> AppResult<String> {
    if Uuid::parse_str(&journal_value.operation_id).is_err() {
        return Err(AppError::conflict("插件迁移操作标识无效"));
    }
    Ok(format!(
        "{PLUGINS_DIRECTORY}/{MIGRATION_DIRECTORY}/{}/{BEFORE_DIRECTORY}",
        journal_value.operation_id
    ))
}

/// 目标 schema 2 状态的稳定字节；journal 同时保存摘要，恢复时不需要重算业务字段。
fn state_bytes(state: &PluginStoreState) -> AppResult<Vec<u8>> {
    serde_json::to_vec_pretty(state).map_err(|_| AppError::infrastructure("插件状态序列化失败"))
}

/// 只读状态探测区分缺失、损坏和已知 schema；未来 schema 不能被套默认值。
enum StateProbe {
    Missing,
    Current(PluginStoreState),
    Legacy(LegacyStoreState),
    Invalid(Option<u32>),
}

/// 状态文件只读普通小文件；读取后按 schema 严格解析，损坏或未来版本不覆盖原文。
fn read_state_probe(path: &Path) -> AppResult<StateProbe> {
    let Some(bytes) = read_bounded_regular(path, MAX_STATE_BYTES)? else {
        return Ok(StateProbe::Missing);
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Ok(StateProbe::Invalid(None)),
    };
    let schema = value
        .get("schema")
        .and_then(serde_json::Value::as_u64)
        .and_then(|schema| u32::try_from(schema).ok());
    match schema {
        Some(CURRENT_STATE_SCHEMA) => match serde_json::from_slice::<PluginStoreState>(&bytes) {
            Ok(state) if validate_state(&state).is_ok() => Ok(StateProbe::Current(state)),
            _ => Ok(StateProbe::Invalid(schema)),
        },
        Some(LEGACY_STATE_SCHEMA) => match serde_json::from_slice::<LegacyStoreState>(&bytes) {
            Ok(state) => Ok(StateProbe::Legacy(state)),
            Err(_) => Ok(StateProbe::Invalid(schema)),
        },
        other => Ok(StateProbe::Invalid(other)),
    }
}

/// 旧状态只校验迁移会用到的边界；pending 的具体变体由领域反序列化拒绝未知外形。
fn validate_legacy_state(state: &LegacyStoreState) -> Result<(), ()> {
    if state.schema != LEGACY_STATE_SCHEMA
        || state.plugins.len() > MAX_PLUGINS
        || state.pending.len() > MAX_PENDING
        || state.failures.len() > MAX_PLUGINS
        || state.failures.values().any(|message| message.len() > 1024)
        || state
            .boot
            .as_ref()
            .is_some_and(|boot| Uuid::parse_str(&boot.id).is_err())
        || state
            .plugins
            .values()
            .any(|plugin| plugin.installed_at.is_empty() || plugin.installed_at.len() > 64)
    {
        return Err(());
    }
    // schema 2 每个插件只允许一条 pending；旧状态出现重复时无法明确保留哪一条，交人工恢复。
    let mut pending_ids = BTreeSet::new();
    if state
        .pending
        .iter()
        .any(|change| !pending_ids.insert(change.plugin_id().clone()))
    {
        return Err(());
    }
    Ok(())
}

/// 旧 active 目录只在单层 `packages/<id>/<version>/` 内解析；任何链接或缺失都交回分类处理。
fn probe_package(
    directory: &Path,
    id: &PluginId,
    version: &PluginVersion,
) -> AppResult<PackageProbe> {
    let manifest_path = directory.join(MANIFEST_FILE);
    let Some(bytes) = read_bounded_regular(&manifest_path, MAX_MANIFEST_BYTES)? else {
        return Err(AppError::conflict("插件包清单缺失"));
    };
    let policy = if id.as_str().starts_with("kerminal.") {
        ManifestPolicy::Official
    } else {
        ManifestPolicy::ThirdParty
    };
    match kerminal_plugin_protocol::parse_manifest(&bytes, policy) {
        Ok(manifest) => probe_valid_manifest(directory, id, version, manifest),
        Err(_) => probe_script_manifest(directory, id, version, &bytes),
    }
}

/// 当前协议可解析的旧包只提取 host schema；清单身份必须与目录和状态完全一致。
fn probe_valid_manifest(
    directory: &Path,
    id: &PluginId,
    version: &PluginVersion,
    manifest: PluginManifest,
) -> AppResult<PackageProbe> {
    if manifest.id != id.as_str() || manifest.version != version.to_string() {
        return Err(AppError::conflict("插件包清单身份与目录或状态不一致"));
    }
    let mut host_types = Vec::new();
    for host in &manifest.contributes.host_types {
        host_types.push(read_host_type(
            directory,
            &host.id,
            &host.title,
            &host.schema,
            id,
        )?);
    }
    Ok(PackageProbe {
        script: false,
        name: localized_name(&manifest.name),
        host_types,
    })
}

/// 旧 script 清单不再进入当前协议，但可受限读取 id/version/host_types 以准备 codec。
///
/// 只读取固定普通文件字段，不执行、不解析视图、不接受外部引用；缺字段时报阻断。
fn probe_script_manifest(
    directory: &Path,
    id: &PluginId,
    version: &PluginVersion,
    bytes: &[u8],
) -> AppResult<PackageProbe> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| AppError::conflict("插件清单不是有效 JSON"))?;
    let script = value
        .get("runtime")
        .and_then(|runtime| runtime.get("kind"))
        .and_then(serde_json::Value::as_str)
        == Some("script");
    if !script {
        return Err(AppError::conflict("旧插件清单不符合当前协议"));
    }
    if value.get("id").and_then(serde_json::Value::as_str) != Some(id.as_str())
        || value.get("version").and_then(serde_json::Value::as_str)
            != Some(version.to_string().as_str())
    {
        return Err(AppError::conflict("旧 script 清单身份与目录或状态不一致"));
    }
    let name = value
        .get("name")
        .and_then(serde_json::Value::as_object)
        .and_then(|names| {
            names
                .get("en")
                .or_else(|| names.values().next())
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or(id.as_str())
        .to_owned();
    let mut host_types = Vec::new();
    let contributions = value
        .get("contributes")
        .and_then(serde_json::Value::as_object);
    if let Some(items) = contributions
        .and_then(|contributions| contributions.get("host_types"))
        .and_then(serde_json::Value::as_array)
    {
        for item in items {
            let Some(object) = item.as_object() else {
                return Err(AppError::conflict("旧 script 清单的主机声明无效"));
            };
            let host_id = object
                .get("id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| AppError::conflict("旧 script 主机声明缺少身份"))?;
            let schema = object
                .get("schema")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| AppError::conflict("旧 script 主机声明缺少 schema"))?;
            let title = object
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(host_id);
            host_types.push(read_host_type(directory, host_id, title, schema, id)?);
        }
    }
    Ok(PackageProbe {
        script: true,
        name,
        host_types,
    })
}

/// host schema 路径先按包路径规则验证，再读取普通小文件并编译，禁止越界或外部引用。
fn read_host_type(
    directory: &Path,
    host_id: &str,
    title: &str,
    schema: &str,
    provider: &PluginId,
) -> AppResult<HostTypeProbe> {
    kerminal_plugin_protocol::validate_package_path(schema)
        .map_err(|_| AppError::conflict("插件主机 schema 路径不安全"))?;
    let mut current = directory.to_path_buf();
    let mut segments = schema.split('/').peekable();
    while let Some(segment) = segments.next() {
        current.push(segment);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|source| io_error("检查插件主机 schema 路径失败", source))?;
        let is_last = segments.peek().is_none();
        if !layout::safe_metadata(&metadata)
            || if is_last {
                !metadata.is_file()
            } else {
                !metadata.is_dir()
            }
        {
            return Err(AppError::conflict("插件主机 schema 路径不安全"));
        }
    }
    let path = directory.join(schema);
    let Some(bytes) = read_bounded_regular(&path, MAX_SCHEMA_BYTES)? else {
        return Err(AppError::conflict("插件主机 schema 缺失"));
    };
    let schema = ConfigurationSchema::parse(&bytes)
        .map_err(|_| AppError::conflict("插件主机 schema 无效"))?;
    Ok(HostTypeProbe {
        id: host_id.to_owned(),
        title: title.to_owned(),
        configuration_type: format!("{provider}/{host_id}"),
        schema,
    })
}

/// 目录名和清单共同确定活动目录；已迁移阶段优先读最终目录，其次读备份与旧树。
fn locate_active_directory(
    root: &Path,
    plugins: &Path,
    id: &PluginId,
    version: &PluginVersion,
    journal: Option<&PluginMigrationJournal>,
) -> AppResult<Option<PathBuf>> {
    let mut candidates = vec![plugins.join(id.as_str())];
    if let Some(journal) = journal {
        let backup = plugins
            .join(MIGRATION_DIRECTORY)
            .join(&journal.operation_id)
            .join(BEFORE_DIRECTORY);
        candidates.push(
            backup
                .join(LEGACY_PACKAGES_DIRECTORY)
                .join(id.as_str())
                .join(version.to_string()),
        );
    }
    candidates.push(
        plugins
            .join(LEGACY_PACKAGES_DIRECTORY)
            .join(id.as_str())
            .join(version.to_string()),
    );
    for candidate in candidates {
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.is_dir() && layout::safe_metadata(&metadata) => {
                let relative = candidate
                    .strip_prefix(root)
                    .map_err(|_| AppError::conflict("插件活动目录越过数据根"))?;
                if relative
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
                {
                    return Err(AppError::conflict("插件活动目录路径不安全"));
                }
                return Ok(Some(candidate));
            }
            Ok(_) => return Err(AppError::conflict("插件活动目录不是安全目录")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error("检查插件活动目录失败", source)),
        }
    }
    Ok(None)
}

/// 分类结果只携带安全类别；调用方不需要复制 DTO 构造规则。
fn inspection(
    class: PluginLayoutClass,
    blockers: Vec<DataMigrationBlocker>,
    notices: Vec<DataMigrationNotice>,
    migration_pending: bool,
    legacy: Option<LegacyInventory>,
) -> PluginLayoutInspection {
    PluginLayoutInspection {
        class,
        blockers,
        notices,
        migration_pending,
        legacy,
    }
}

struct ClassOutcome {
    class: PluginLayoutClass,
    blockers: Vec<DataMigrationBlocker>,
    notices: Vec<DataMigrationNotice>,
    legacy: Option<LegacyInventory>,
}

/// 未知材料统一进入 recovery，不根据目录名猜测其归属。
fn recovery_outcome(blocker: DataMigrationBlocker) -> ClassOutcome {
    ClassOutcome {
        class: PluginLayoutClass::Recovery,
        blockers: vec![blocker],
        notices: Vec::new(),
        legacy: None,
    }
}

/// 只列出单层条目并保留元数据；调用方随后逐项检查类型与链接。
fn list_entries(directory: &Path) -> AppResult<Vec<(String, fs::Metadata)>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory).map_err(|source| io_error("读取插件目录失败", source))?
    {
        let entry = entry.map_err(|source| io_error("读取插件目录失败", source))?;
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| AppError::conflict("插件目录包含非 UTF-8 名称"))?
            .to_owned();
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|source| io_error("读取插件目录项失败", source))?;
        entries.push((name, metadata));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(entries)
}

/// 空目录是可继续处理的材料；读取失败不能当成空目录。
fn directory_has_entries(directory: &Path) -> AppResult<bool> {
    match fs::read_dir(directory) {
        Ok(mut entries) => Ok(entries.next().is_some()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(io_error("读取插件材料目录失败", source)),
    }
}

/// 只读普通文件：缺失返回 None；链接、非普通文件或超限直接阻断。
fn read_bounded_regular(path: &Path, maximum: u64) -> AppResult<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error("检查插件文件失败", source)),
    };
    if !metadata.is_file() || !layout::safe_metadata(&metadata) || metadata.len() > maximum {
        return Err(AppError::conflict("插件文件不是安全普通文件"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // 句柄指向重解析点本身，避免检查后跟随链接读取目标文件。
        options.custom_flags(0x0020_0000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|source| io_error("打开插件文件失败", source))?;
    let opened = file
        .metadata()
        .map_err(|source| io_error("检查插件文件失败", source))?;
    if !opened.is_file() || !layout::safe_metadata(&opened) || opened.len() != metadata.len() {
        return Err(AppError::conflict("插件文件在读取期间发生变化"));
    }
    let mut bytes = Vec::new();
    file.take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| io_error("读取插件文件失败", source))?;
    if bytes.len() as u64 > maximum {
        return Err(AppError::conflict("插件文件超过读取限额"));
    }
    Ok(Some(bytes))
}

/// 兼容旧版 `state-recovery-*.json` 的恢复材料，必须由存储恢复入口显式处理。
fn is_recovery_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("state-recovery-") && name.ends_with(".json")
}

/// 本地时区无关的固定 UTC 秒级时间，用于旧记录缺少 installed_at 的极端情况。
fn timestamp_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// 旧清单名称优先英文，其次第一个本地化值，最后退回稳定插件身份。
fn localized_name(names: &BTreeMap<String, String>) -> String {
    names
        .get("en")
        .or_else(|| names.values().next())
        .cloned()
        .unwrap_or_default()
}

/// 把布局错误映射为稳定公开类别，不把本机路径写入报告。
fn layout_error(error: LayoutError) -> AppError {
    match error {
        LayoutError::OutcomeUnknown { source, .. } => {
            AppError::outcome_unknown_with_source("插件布局变更结果未知", source)
        }
        LayoutError::Io(source) | LayoutError::Fingerprint(source) => {
            AppError::infrastructure_with_source("插件布局读写失败", source)
        }
        other => AppError::conflict(match other {
            LayoutError::Unsafe(_) | LayoutError::Conflict(_) | LayoutError::Missing(_) => {
                "插件布局证据不一致，已保留现场"
            }
            LayoutError::RecoveryRequired { .. } => "插件布局需要人工恢复",
            _ => "插件布局操作失败",
        }),
    }
}

/// 保留 I/O 原因供内部诊断，公开摘要不带路径。
fn io_error(context: &'static str, source: io::Error) -> AppError {
    AppError::infrastructure_with_source(context, source)
}

/// 迁移回执只使用数量与固定相对路径；空计划由本函数构造稳定默认值。
fn report(
    outcome: PluginLayoutOutcome,
    journal_value: &PluginMigrationJournal,
    backup_relative_path: Option<String>,
) -> PluginLayoutReport {
    PluginLayoutReport {
        outcome,
        migrated_plugins: journal_value
            .entries
            .iter()
            .filter(|entry| !entry.source.is_empty())
            .count(),
        removed_previous_versions: journal_value.removed_previous_versions,
        discarded_pending: journal_value.discarded_pending,
        backup_relative_path,
    }
}

/// AlreadyCurrent 不需要 journal；用空计划让回执字段保持确定。
fn empty_journal() -> PluginMigrationJournal {
    PluginMigrationJournal {
        kind: MIGRATION_JOURNAL_KIND.to_owned(),
        schema: MIGRATION_JOURNAL_SCHEMA,
        operation_id: Uuid::nil().to_string(),
        phase: PluginMigrationPhase::Cleaned,
        backup_relative: String::new(),
        legacy_state_sha256: journal::sha256(b""),
        target_state: PluginStoreState::default(),
        target_state_sha256: journal::sha256(b""),
        packages_present: false,
        packages_fingerprint: None,
        entries: Vec::new(),
        removed_previous_versions: 0,
        discarded_pending: 0,
    }
}

#[cfg(test)]
#[path = "../../tests/support/plugin_layout_migration_unit.rs"]
mod tests;
