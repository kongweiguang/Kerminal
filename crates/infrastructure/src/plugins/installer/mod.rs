//! 安装器只发布已校验的不可变版本，并通过状态仓储排队生效。
//! @author kongweiguang

mod archive;
mod download;
mod recovery;

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    extensions::{
        CatalogSelection, DEFAULT_PLUGIN_CATALOG_URL, DropInInspection, ExtensionChange,
        ExtensionReceipt, InstallProgress, LocalPackageInspection, PluginPackageStore,
    },
    plugin_settings::PluginSettingsService,
    plugins::{CORE_PLUGIN_ID, is_builtin_unit, is_reserved_plugin_id},
};
use kerminal_domain::plugins::{
    InstalledPlugin, PendingPluginChange, PluginFingerprint, PluginId, PluginInstallStatus,
    PluginSource, PluginStoreState, PluginUnsupportedReason, PluginVersion,
};
use kerminal_plugin_protocol::{ManifestPolicy, Platform};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};
use tokio_util::sync::CancellationToken;

use super::{
    boot::{BootPackageValidator, ProtocolBootValidator},
    catalog::CatalogClient,
    layout::{self, LayoutError},
    store::{PluginStore, PluginStoreError, PluginStoreSnapshot},
};

/// 所有入口共用目录信任、状态仓储和设置删除服务。
#[derive(Clone)]
pub struct PackageInstaller {
    store: PluginStore,
    catalog: Arc<CatalogClient>,
    settings: Arc<PluginSettingsService>,
    host_api: String,
    platform: Platform,
}

impl PackageInstaller {
    /// 平台和 API 由组合根固定，插件和界面不能替换宿主身份。
    pub fn new(
        store: PluginStore,
        catalog: Arc<CatalogClient>,
        settings: Arc<PluginSettingsService>,
        host_api: String,
        platform: Platform,
    ) -> Self {
        Self {
            store,
            catalog,
            settings,
            host_api,
            platform,
        }
    }

    /// 在同一可信快照中选择精确版本与平台；不能接受调用方拼接的 URL。
    ///
    /// 先按确认时冻结的目录身份重读同一来源缓存，再解析条目与资产：目录在确认期间
    /// 刷新到另一份原文或来源已切换时返回冲突，避免按 id/version/platform 在最新
    /// 目录里静默换包。身份比较发生在下载与暂存之前，失败不会留下任何安装材料。
    async fn selected_asset(
        &self,
        selection: &CatalogSelection,
    ) -> AppResult<(kerminal_application::extensions::CatalogAsset, String)> {
        if selection.platform != Platform::Any && selection.platform != self.platform {
            return Err(AppError::invalid("插件资产平台不匹配"));
        }
        let verified = self.catalog.cached_matching(&selection.identity).await?;
        let plugin = verified
            .snapshot
            .catalog
            .plugins
            .iter()
            .find(|plugin| plugin.id == selection.id)
            .ok_or_else(|| AppError::not_found("插件目录条目不存在"))?;
        let version = plugin
            .versions
            .iter()
            .find(|version| version.version == selection.version)
            .ok_or_else(|| AppError::not_found("插件目录版本不存在"))?;
        let asset = version
            .assets
            .iter()
            .find(|asset| asset.platform == selection.platform)
            .ok_or_else(|| AppError::not_found("插件目录资产不存在"))?;
        Ok((asset.clone(), version.api.clone()))
    }

    /// 开始前检查剩余空间；未知挂载或整数溢出都不伪装成空间充足。
    /// 解包先落在 `.staging/_incoming/<uuid>`，此时插件身份尚未解析，不能按 ID 分槽。
    fn stage(&self, bytes: u64) -> AppResult<Stage> {
        let root = self.store.root();
        let incoming = layout::ensure_directory(
            root,
            &[
                layout::PLUGINS_DIRECTORY,
                layout::STAGING_DIRECTORY,
                layout::INCOMING_DIRECTORY,
            ],
        )
        .map_err(layout_app_error)?;
        let canonical = fs::canonicalize(&incoming)
            .map_err(|e| AppError::infrastructure_with_source("解析插件暂存路径失败", e))?;
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let disk = disks
            .list()
            .iter()
            .filter(|disk| path_on_mount(&canonical, disk.mount_point()))
            .max_by_key(|disk| disk.mount_point().components().count())
            .ok_or_else(|| AppError::infrastructure("无法确定插件暂存磁盘剩余空间"))?;
        let required = bytes
            .checked_mul(3)
            .ok_or_else(|| AppError::invalid("插件安装包大小无效"))?;
        if disk.available_space() < required {
            return Err(AppError::invalid("插件安装空间不足"));
        }
        let operation_id = uuid::Uuid::new_v4().to_string();
        let directory = incoming.join(&operation_id);
        fs::create_dir(&directory)
            .map_err(|e| AppError::infrastructure_with_source("创建插件暂存失败", e))?;
        layout::verify_directory(&directory).map_err(layout_app_error)?;
        Ok(Stage {
            directory,
            operation_id,
        })
    }

    /// 完整校验后才排队；暂存目录按插件 ID 与操作 UUID 固化，启动时再由 boot 换位。
    ///
    /// `verified` 是本次安装的唯一信任锚（市场取签名目录资产、本地取预检确认值），
    /// 与实际解压的压缩流在同一次打开内核对；摘要验证失败时状态受理根本不会开始，
    /// 消除“先按路径验 SHA、再按路径解压”期间替换包的窗口。解压失败时目的目录
    /// 可能已写入部分成员，整体随暂存目录由调用方清理，状态与发布槽不受影响。
    fn prepare_and_commit(
        &self,
        stage: &Stage,
        source: PluginSource,
        expected: Option<(&CatalogSelection, &str)>,
        verified: &archive::ArchiveExpectation,
        progress: &tokio::sync::watch::Sender<InstallProgress>,
        cancellation: &CancellationToken,
    ) -> AppResult<ExtensionReceipt> {
        progress.send_replace(InstallProgress::Verifying);
        let extracted = stage.directory.join("package");
        fs::create_dir(&extracted)
            .map_err(|e| AppError::infrastructure_with_source("创建插件解压目录失败", e))?;
        // 测试注入点：模拟“验证后、解压前”窗口内的暂存包替换；run 在生产构建
        // 内部直接跳过，只对登记了本安装器配置根的动作生效，其他 fixture 的
        // 并行安装不受影响。
        test_hooks::before_unpack::run(self.store.root(), &stage.archive());
        archive::unpack_verified(&stage.archive(), &extracted, cancellation, verified)?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件安装已取消"));
        }
        // 官方发布者命名空间只对官方目录开放。自定义目录仍按市场来源记录，
        // 但清单校验降为第三方，避免自定义源自报 publisher=kerminal 冒充官方；
        // 保留 ID 策略在下面独立执行，不因来源切换而放宽。
        //
        // 目录安装一定带 selection，用身份里的实际来源判定；只有本地/测试等没有
        // 目录选择的路径才退回 PluginSource。这样自定义目录无法借 source=Official
        // 通过官方命名空间校验，同时保留“官方包也必须在移动前拒绝内置 ID”的门禁。
        let official = match expected {
            Some((selection, _)) => selection.identity.source_url == DEFAULT_PLUGIN_CATALOG_URL,
            None => source == PluginSource::Official,
        };
        let policy = if official {
            ManifestPolicy::Official
        } else {
            ManifestPolicy::ThirdParty
        };
        let package = kerminal_plugin_protocol::validate_package(&extracted, policy)
            .map_err(|e| AppError::infrastructure_with_source("插件包校验失败", e))?;
        let id = PluginId::parse(&package.manifest.id)
            .map_err(|_| AppError::invalid("插件包身份无效"))?;
        // 在移动暂存目录之前拒绝身份冲突：外部包不能覆盖内置单元，
        // 也不能留下无人引用的 staging 让后续启动把它误当恢复材料。
        if is_reserved_plugin_id(id.as_str()) {
            return Err(AppError::invalid("插件身份与内置单元冲突"));
        }
        let version = PluginVersion::parse(&package.manifest.version)
            .map_err(|_| AppError::invalid("插件包版本无效"))?;
        if let Some((selection, api)) = expected {
            if id != selection.id || version != selection.version || package.manifest.api != api {
                return Err(AppError::invalid("插件包与目录不匹配"));
            }
            package
                .validate_for_platform(selection.platform)
                .map_err(|_| AppError::invalid("插件包平台不匹配"))?;
        }
        let validator = ProtocolBootValidator {
            host_api: self.host_api.clone(),
            platform: self.platform,
        };
        validator
            .validate(&extracted, &id, &version, source)
            .map_err(|_| AppError::invalid("插件包与当前宿主不兼容"))?;
        let fingerprint = layout::fingerprint_tree(&extracted).map_err(layout_app_error)?;
        let expected_state = self.store.load().map_err(store_error)?;
        let mut next = expected_state.state.clone();
        if next.pending.iter().any(|item| item.plugin_id() == &id) {
            return Err(AppError::conflict("插件已有待处理操作"));
        }
        let change = match next.plugins.get(&id) {
            Some(existing) if existing.source != source => {
                return Err(AppError::conflict("插件安装来源不匹配"));
            }
            // 目录缺失的记录没有可用内容；同版本重装是修复缺失包，不是重复安装。
            Some(existing)
                if existing.active == version
                    && existing.status != PluginInstallStatus::Missing =>
            {
                return Err(AppError::conflict("插件版本已经安装"));
            }
            Some(_) => PendingPluginChange::Update {
                id: id.clone(),
                to: version.clone(),
                staged: stage.operation_id.clone(),
                fingerprint: Some(fingerprint.clone()),
            },
            None => PendingPluginChange::Install {
                id: id.clone(),
                to: version.clone(),
                staged: stage.operation_id.clone(),
                fingerprint: Some(fingerprint.clone()),
                source,
                installed_at: chrono::DateTime::<chrono::Utc>::from(SystemTime::now()).to_rfc3339(),
            },
        };
        // 身份确定后才移动到按 ID 分组的槽；UUID 保留同 ID 多次安装的残留证据。
        let slot = layout::staging_directory(self.store.root(), &id, &stage.operation_id)
            .map_err(layout_app_error)?;
        layout::ensure_directory(
            self.store.root(),
            &[
                layout::PLUGINS_DIRECTORY,
                layout::STAGING_DIRECTORY,
                id.storage_segment(),
            ],
        )
        .map_err(layout_app_error)?;
        #[cfg_attr(not(test), allow(unused_mut))]
        let mut move_result = layout::move_directory(&extracted, &slot, &stage.operation_id);
        // 测试注入点：模拟“改名成功、目录同步失败”的真实 CommitUnknown 形态；
        // run 在生产构建内部直接跳过，只对登记了本安装器配置根的动作生效。
        test_hooks::after_move::run(self.store.root(), &mut move_result);
        match move_result {
            // 改名成功后的目录同步未知意味着业务移动已经发生：槽位材料在磁盘
            // 上可见，把持久性未知伪装成“未移动”失败会让用户重试安装并与已
            // 存在的槽位冲突。这里按“已移动”继续受理状态，pending 与指纹就是
            // 崩溃后的恢复证据；若落盘确实丢失，下次启动恢复会以缺目录/恢复
            // 材料的形态如实暴露，而不是在这里伪称已回收或已失败。
            Err(LayoutError::OutcomeUnknown { .. }) => {}
            result => result.map_err(layout_app_error)?,
        }
        next.pending.push(change);
        if cancellation.is_cancelled() {
            // 取消只发生在状态受理之前；回收本次独占的暂存槽。
            reclaim_staging_slot(&slot);
            return Err(AppError::cancelled("插件安装已取消"));
        }
        progress.send_replace(InstallProgress::Publishing);
        let committed = self
            .store
            .compare_and_swap(&expected_state, next)
            .map_err(|error| {
                // 状态未受理时清掉本次槽，避免产生无引用的暂存材料；结果未知
                // 或清理失败但状态已提交时槽可能仍被 pending 引用，必须保留。
                if !matches!(
                    error,
                    PluginStoreError::OutcomeUnknown { .. }
                        | PluginStoreError::LayoutOutcomeUnknown { .. }
                        | PluginStoreError::RecoveryCleanupFailed {
                            state_committed: true,
                            ..
                        }
                ) {
                    reclaim_staging_slot(&slot);
                }
                store_error(error)
            })?;
        Ok(ExtensionReceipt {
            revision: committed.state.revision,
            restart_required: true,
            cleanup_warning: None,
        })
    }

    /// 确认动作同时绑定界面看到的指纹与修订；锁内重算一次，目录在确认前后
    /// 被替换时只返回冲突，绝不把已改变的内容标成已确认。
    fn confirm_drop_in_sync(
        &self,
        id: &PluginId,
        expected_fingerprint: &str,
        expected_revision: u64,
    ) -> AppResult<ExtensionReceipt> {
        // 确认入口只服务于外部 drop-in；保留身份不是可确认的外部插件。
        if is_reserved_plugin_id(id.as_str()) {
            return Err(AppError::invalid("插件身份与内置单元冲突"));
        }
        let expected = self.store.load().map_err(store_error)?;
        if expected.state.revision != expected_revision {
            return Err(AppError::conflict("插件状态已更改"));
        }
        let installed = expected
            .state
            .plugins
            .get(id)
            .ok_or_else(|| AppError::not_found("插件未发现"))?;
        if installed.source != PluginSource::DropIn {
            return Err(AppError::not_found("插件不是手动放入的插件"));
        }
        if installed.confirmed {
            return Err(AppError::conflict("插件已确认启用"));
        }
        if expected
            .state
            .pending
            .iter()
            .any(|item| item.plugin_id() == id)
        {
            return Err(AppError::conflict("插件已有待处理操作"));
        }
        // 指纹在检查时由界面展示；格式不对或内容变化都说明确认已过期。
        let requested = PluginFingerprint::parse(expected_fingerprint.to_owned())
            .map_err(|_| AppError::conflict("插件目录内容与检查结果不一致"))?;
        let directory = layout::package_directory(self.store.root(), id);
        let actual = layout::fingerprint_tree(&directory)
            .map_err(|_| AppError::conflict("插件目录内容与检查结果不一致"))?;
        if actual != requested {
            return Err(AppError::conflict("插件目录内容与检查结果不一致"));
        }
        // 完整协议校验会核对目录名、清单身份、API 与平台；失败同样视为内容不一致。
        ProtocolBootValidator {
            host_api: self.host_api.clone(),
            platform: self.platform,
        }
        .validate(&directory, id, &installed.active, PluginSource::DropIn)
        .map_err(|_| AppError::conflict("插件目录内容与检查结果不一致"))?;
        let mut next = expected.state.clone();
        let target = next
            .plugins
            .get_mut(id)
            .ok_or_else(|| AppError::not_found("插件未发现"))?;
        target.confirmed = true;
        target.enabled = true;
        target.fingerprint = Some(actual.clone());
        // drop-in 首次启用同样走 pending，运行中的贡献快照在重启后才更新。
        next.pending
            .push(PendingPluginChange::Enable { id: id.clone() });
        let directory_for_publish = directory;
        let expected_for_publish = expected;
        let snapshot = self
            .store
            .publish_and_compare(
                &expected_for_publish,
                next,
                move || match layout::fingerprint_tree(&directory_for_publish) {
                    Ok(current) if current == requested => Ok(()),
                    Ok(_) => Err(PluginStoreError::Conflict),
                    Err(LayoutError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                        Err(PluginStoreError::Conflict)
                    }
                    Err(error) => Err(PluginStoreError::Layout(error)),
                },
            )
            .map_err(store_error)?;
        Ok(ExtensionReceipt {
            revision: snapshot.state.revision,
            restart_required: true,
            cleanup_warning: None,
        })
    }
}

/// 暂存目录以 UUID 命名，解析身份后移动到按 ID 分组的槽；启动恢复可识别遗留材料。
struct Stage {
    directory: PathBuf,
    operation_id: String,
}
impl Stage {
    /// 显式清理失败必须反馈给调用方；崩溃时保留的目录由下次启动检查。
    fn cleanup(&self) -> AppResult<()> {
        layout::remove_tree_verified(&self.directory).map_err(|error| match error {
            LayoutError::Io(error) => {
                AppError::outcome_unknown_with_source("插件暂存清理失败", error)
            }
            error => AppError::outcome_unknown_with_source("插件暂存清理失败", error),
        })
    }
    /// 宿主固定归档名，不采纳插件路径。
    fn archive(&self) -> PathBuf {
        self.directory.join("pkg.kplugin")
    }
}
impl Drop for Stage {
    /// 临时目录只包含新建的普通文件；清理失败由下次启动恢复处理。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// 测试注入点：安装流测试借它在受控交错点模拟替换或同步失败，替代不可靠的睡眠时序。
///
/// 模块始终参与编译，让生产调用点保持普通语句而不依赖 cfg 属性；`run` 在非测试
/// 构建里由 `cfg!(test)` 常量分支直接跳过，真实安装不会执行任何注入动作。生产
/// 构建只调用 `run`，登记表与 set/clear 属于测试专用 API，用 allow(dead_code)
/// 抑制未使用告警，不改变产物行为。
///
/// 登记表按数据根键控，每个用例使用独立 tempdir 根登记后即可并行运行；
/// 锁的职责只限登记表的读取与更新，动作必须在锁外执行，否则动作一旦 panic
/// 会毒化互斥量，其他用例（包括守卫 Drop 里的清理）全部跟着失败，整个测试
/// 进程被一起拖垮。
#[cfg_attr(not(test), allow(dead_code))]
mod test_hooks {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, PoisonError};

    /// 锁中毒时取回内部数据继续工作：测试动作的 panic 不应把后续用例的
    /// 登记与清理也变成失败，尤其不能让 RAII 守卫 Drop 二次 panic。
    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 登记表而非单一槽位：每个用例使用独立 tempdir 作为配置根，按根登记
    /// 后不同用例可以并行登记、互不命中；否则一个全局动作会误改其他用例
    /// 正在安装的暂存归档，产生与摘要绑定无关的假失败。
    type Registry<T> = Mutex<Vec<(PathBuf, T)>>;
    /// 解压前动作与移动后动作的共享句柄；单独命名避免 clippy 把登记表类型判为过复杂。
    type BeforeUnpackAction = Arc<dyn Fn(&Path) + Send + Sync>;
    type AfterMoveAction = Arc<dyn Fn(&mut Result<(), super::LayoutError>) + Send + Sync>;
    static BEFORE_UNPACK: Registry<BeforeUnpackAction> = Mutex::new(Vec::new());
    static AFTER_MOVE: Registry<AfterMoveAction> = Mutex::new(Vec::new());

    /// 通用登记/查找/移除逻辑；查找按根等值匹配，前缀相近的 tempdir 不串扰。
    mod registry {
        use super::{Path, Registry, lock};

        /// 以配置根登记动作；同一根重复登记会覆盖旧动作，避免残留动作在
        /// 断言之间继续触发。调用方负责在用例结束时移除同一根。
        pub(in crate::plugins::installer) fn set<T>(
            registry: &Registry<T>,
            root: &Path,
            action: T,
        ) {
            let mut actions = lock(registry);
            actions.retain(|(existing, _)| existing != root);
            actions.push((root.to_path_buf(), action));
        }

        /// 只移除指定根的动作；用例失败路径也必须调用，避免动作泄漏。
        pub(in crate::plugins::installer) fn clear<T>(registry: &Registry<T>, root: &Path) {
            let mut actions = lock(registry);
            actions.retain(|(existing, _)| existing != root);
        }

        /// 取出登记在本根下的动作副本；锁在返回前释放，调用方在锁外执行。
        pub(in crate::plugins::installer) fn lookup<T: Clone>(
            registry: &Registry<T>,
            root: &Path,
        ) -> Option<T> {
            let actions = lock(registry);
            actions
                .iter()
                .find(|(registered, _)| registered == root)
                .map(|(_, action)| action.clone())
        }
    }

    /// 解压前动作注入：模拟“验证完成、解压未开始”窗口内的暂存包替换。
    pub(in crate::plugins::installer) mod before_unpack {
        use super::registry;
        use super::{Arc, BEFORE_UNPACK, Path};

        /// 以配置根登记解压前动作；同一根重复登记会覆盖旧动作。
        pub(in crate::plugins::installer) fn set(
            root: &Path,
            action: Box<dyn Fn(&Path) + Send + Sync>,
        ) {
            registry::set(&BEFORE_UNPACK, root, Arc::new(action));
        }

        /// 安装器在解压前同步调用：只执行登记在本次安装器配置根下的动作。
        ///
        /// 动作先在锁内克隆再释放锁执行：动作 panic 只会影响当前安装流程
        /// （测试里由调用方断言），不会毒化登记表锁，也不会阻断其他线程的
        /// 登记、命中或守卫清理。
        pub(in crate::plugins::installer) fn run(root: &Path, archive_path: &Path) {
            // 非测试构建直接跳过：用运行时常量分支替代 cfg 属性，让安装器调用点
            // 保持普通语句，同时保证生产安装绝不会被测试动作改写。
            if !cfg!(test) {
                return;
            }
            // 归档必须位于该根之下才执行；按分量比较，`/tmp/a` 不会误匹配 `/tmp/ab`。
            let action = (archive_path.starts_with(root))
                .then(|| registry::lookup(&BEFORE_UNPACK, root))
                .flatten();
            if let Some(action) = action {
                action(archive_path);
            }
        }

        /// 只移除指定根的动作；用例失败路径也必须调用，避免动作泄漏。
        pub(in crate::plugins::installer) fn clear(root: &Path) {
            registry::clear(&BEFORE_UNPACK, root);
        }
    }

    /// 移动后结果注入：模拟“目录改名成功、持久化同步失败”的 CommitUnknown
    /// 形态，用于验证安装器把它折叠成“已移动、继续受理”的可恢复语义。
    pub(in crate::plugins::installer) mod after_move {
        use super::registry;
        use super::{AFTER_MOVE, Arc, Path};
        use crate::plugins::layout::LayoutError;

        /// 移动后动作的入参类型；命名后避免 clippy 把注入签名判为过复杂。
        type AfterMoveHook = Box<dyn Fn(&mut Result<(), LayoutError>) + Send + Sync>;

        /// 以配置根登记移动后动作；同一根重复登记会覆盖旧动作。
        pub(in crate::plugins::installer) fn set(root: &Path, action: AfterMoveHook) {
            registry::set(&AFTER_MOVE, root, Arc::from(action));
        }

        /// 安装器在 `move_directory` 返回后同步调用；动作在锁外改写结果。
        pub(in crate::plugins::installer) fn run(
            root: &Path,
            result: &mut Result<(), LayoutError>,
        ) {
            // 与解压前注入同一契约：生产构建不读取登记表，也不改写移动结果。
            if !cfg!(test) {
                return;
            }
            let action = registry::lookup(&AFTER_MOVE, root);
            if let Some(action) = action {
                action(result);
            }
        }

        /// 只移除指定根的动作；用例失败路径也必须调用，避免动作泄漏。
        pub(in crate::plugins::installer) fn clear(root: &Path) {
            registry::clear(&AFTER_MOVE, root);
        }
    }
}

/// Windows 暂存路径含规范化的扩展前缀，挂载点也须规范化；按分量比较避免目录名前缀误匹配。
fn path_on_mount(path: &Path, mount: &Path) -> bool {
    #[cfg(windows)]
    {
        fs::canonicalize(mount).is_ok_and(|mount| path.starts_with(mount))
    }
    #[cfg(not(windows))]
    {
        path.starts_with(mount)
    }
}

/// Windows junction 和符号链接均不能进入发布路径。
fn safe_metadata(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        !metadata.file_type().is_symlink() && metadata.file_attributes() & 0x400 == 0
    }
    #[cfg(not(windows))]
    {
        !metadata.file_type().is_symlink()
    }
}

/// 复制离线包并受取消控制；用户源文件从不被修改。
fn copy_local(source: &Path, target: &Path, cancellation: &CancellationToken) -> AppResult<()> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|e| AppError::infrastructure_with_source("读取本地插件包失败", e))?;
    if !metadata.is_file()
        || !safe_metadata(&metadata)
        || metadata.len() == 0
        || metadata.len() > MAX_LOCAL_PACKAGE_BYTES
    {
        return Err(AppError::invalid("本地插件包无效"));
    }
    let mut input = File::open(source)
        .map_err(|e| AppError::infrastructure_with_source("读取本地插件包失败", e))?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(target)
        .map_err(|e| AppError::infrastructure_with_source("创建插件暂存失败", e))?;
    let mut copied = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件安装已取消"));
        }
        let read = input
            .read(&mut buffer)
            .map_err(|e| AppError::infrastructure_with_source("读取本地插件包失败", e))?;
        if read == 0 {
            break;
        }
        copied += read as u64;
        if copied > metadata.len() {
            return Err(AppError::conflict("本地插件包已更改"));
        }
        output
            .write_all(&buffer[..read])
            .map_err(|e| AppError::infrastructure_with_source("写入插件暂存失败", e))?;
    }
    if copied != metadata.len() {
        return Err(AppError::conflict("本地插件包已更改"));
    }
    output
        .sync_all()
        .map_err(|e| AppError::infrastructure_with_source("同步插件暂存失败", e))
}

/// 本地入口的包大小上限；与 copy_local、inspect_file 入口检查保持同一常量，
/// 摘要观测不得低于或高于入口门禁另立标准。
const MAX_LOCAL_PACKAGE_BYTES: u64 = 512 * 1024 * 1024;

/// 固定缓冲区计算文件摘要，已有版本再大也不会要求同等大小的内存。
///
/// 返回的字节数与摘要来自同一次顺序读取，是该观测点唯一的事实来源；调用方必须
/// 把两者一起用作流验证期望与展示值，不得再从另一个可变观察（如重新按名取
/// metadata）混搭第二个 size。复制源并发增长时读取只在 EOF 收口，单遍计数上限
/// 与本地包入口一致，声明大小再大也不会把这一步变成无界操作。
fn file_digest(path: &Path, cancellation: &CancellationToken) -> AppResult<(u64, String)> {
    let mut file = File::open(path)
        .map_err(|e| AppError::infrastructure_with_source("读取插件暂存失败", e))?;
    let mut hasher = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件安装已取消"));
        }
        let read = file
            .read(&mut buffer)
            .map_err(|e| AppError::infrastructure_with_source("读取插件暂存失败", e))?;
        if read == 0 {
            break;
        }
        count = count
            .checked_add(read as u64)
            .ok_or_else(|| AppError::invalid("插件包大小无效"))?;
        if count > MAX_LOCAL_PACKAGE_BYTES {
            return Err(AppError::invalid("本地插件包无效"));
        }
        hasher.update(&buffer[..read]);
    }
    Ok((count, format!("{:x}", hasher.finalize())))
}

/// 回收本次安装独占的暂存槽，并在父目录因此变空时一并移除空壳。
///
/// 槽位路径由 `staging_directory` 从插件 ID 与本次 UUID 派生，材料只可能属于
/// 本次操作，因此递归删除槽位本身是安全的。父目录 `.staging/<id>` 可能同时
/// 被其他安装（不同 UUID）使用，所以只允许用非递归 `remove_dir` 收尾：Windows
/// 与 POSIX 语义都保证它只删除空目录，目录非空或并发竞争时原样失败，绝不
/// 会删掉他人材料。清理失败保持静默：这里没有可回传的业务错误通道，残留
/// 的空壳目录由启动恢复统一判定，不阻塞主失败原因。
/// @author kongweiguang
fn reclaim_staging_slot(slot: &Path) {
    let _ = layout::remove_tree_verified(slot);
    let Some(parent) = slot.parent() else {
        return;
    };
    // 只对确认是真实目录的父路径尝试收尾；symlink 或 reparse 目录一律不动。
    let Ok(metadata) = fs::symlink_metadata(parent) else {
        return;
    };
    if !metadata.is_dir() || !safe_metadata(&metadata) {
        return;
    }
    // read_dir 只探测首项，不递归、不跟随任何子项；非空即放弃。
    let empty = fs::read_dir(parent).is_ok_and(|mut entries| entries.next().is_none());
    if empty {
        let _ = fs::remove_dir(parent);
    }
}

/// 保留冲突和未知提交类别，其余底层错误留在诊断链中。
fn store_error(error: PluginStoreError) -> AppError {
    match error {
        PluginStoreError::Conflict => AppError::conflict("插件状态已更改"),
        error @ (PluginStoreError::OutcomeUnknown { .. }
        | PluginStoreError::RecoveryRequired { .. }
        | PluginStoreError::RecoveryPreparationUnknown { .. }
        | PluginStoreError::RecoveryCleanupFailed { .. }
        | PluginStoreError::RecoveryResolutionUnknown { .. }
        | PluginStoreError::LayoutRecoveryRequired { .. }
        | PluginStoreError::LayoutOutcomeUnknown { .. }) => {
            AppError::outcome_unknown_with_source("插件状态需读回恢复", error)
        }
        PluginStoreError::Layout(error) => {
            AppError::infrastructure_with_source("插件目录布局操作失败", error)
        }
        error => AppError::infrastructure_with_source("插件存储操作失败", error),
    }
}

/// 布局错误在安装入口统一成带恢复含义的公共错误，不暴露本机路径。
fn layout_app_error(error: LayoutError) -> AppError {
    match error {
        LayoutError::OutcomeUnknown { .. } | LayoutError::RecoveryRequired { .. } => {
            AppError::outcome_unknown_with_source("插件目录变更结果未知", error)
        }
        LayoutError::Conflict(message) => AppError::conflict(message),
        LayoutError::Unsafe(message) | LayoutError::Missing(message) => AppError::invalid(message),
        error => AppError::infrastructure_with_source("插件目录布局操作失败", error),
    }
}

/// 受限清单探测只取展示与身份字段，不解析视图、不执行插件，也不写回状态。
///
/// drop-in 还没有经过安装确认，检查阶段只能读取这份最小快照；完整协议校验
/// 留到确认时执行，避免检查动作把尚未信任的目录升级成可信内容。
struct DropInManifestProbe {
    id: String,
    version: String,
    runtime_kind: String,
    name: BTreeMap<String, String>,
    permissions: Vec<String>,
}

/// 清单读取先检查元数据再以 no-follow 打开，并在句柄上复核类型与大小，
/// 防止检查与读取之间被替换成链接或更大的文件。
fn probe_drop_in_manifest(directory: &Path) -> Option<DropInManifestProbe> {
    let path = directory.join(kerminal_plugin_protocol::MANIFEST_FILE_NAME);
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file()
        || !safe_metadata(&metadata)
        || metadata.len() == 0
        || metadata.len() > kerminal_plugin_protocol::MAX_MANIFEST_BYTES as u64
    {
        return None;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(&path).ok()?;
    let opened = file.metadata().ok()?;
    if !opened.is_file() || !safe_metadata(&opened) || opened.len() != metadata.len() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(kerminal_plugin_protocol::MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > kerminal_plugin_protocol::MAX_MANIFEST_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let name = match value.get("name") {
        Some(value) => value
            .as_object()?
            .iter()
            .map(|(locale, text)| Some((locale.clone(), text.as_str()?.to_owned())))
            .collect::<Option<BTreeMap<_, _>>>()?,
        None => BTreeMap::new(),
    };
    let permissions = match value.get("permissions") {
        Some(value) => value
            .as_array()?
            .iter()
            .map(|permission| permission.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?,
        None => Vec::new(),
    };
    Some(DropInManifestProbe {
        id: value.get("id")?.as_str()?.to_owned(),
        version: value.get("version")?.as_str()?.to_owned(),
        runtime_kind: value.get("runtime")?.get("kind")?.as_str()?.to_owned(),
        name,
        permissions,
    })
}

/// 只读观察一个 drop-in 记录：磁盘缺失、损坏或内容与持久指纹不一致都如实标记。
///
/// 这里不能写回状态，否则一次界面刷新就会把用户尚未确认的目录当成已检查通过；
/// 确认动作会在存储锁内重新计算指纹并执行完整协议校验。
fn inspect_drop_in(
    root: &Path,
    id: &PluginId,
    installed: &InstalledPlugin,
    revision: u64,
) -> DropInInspection {
    let directory = layout::package_directory(root, id);
    let (status, fingerprint, probe) = match fs::symlink_metadata(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (PluginInstallStatus::Missing, None, None)
        }
        Ok(metadata) if !metadata.is_dir() || !safe_metadata(&metadata) => {
            (PluginInstallStatus::Corrupt, None, None)
        }
        Ok(_) => match layout::fingerprint_tree(&directory) {
            Ok(actual) => {
                let unchanged = installed.fingerprint.as_ref() == Some(&actual);
                let probe = probe_drop_in_manifest(&directory);
                // 记录指纹是启动时确认过的内容；目录被替换后即使清单可读也不能算就绪。
                let status = if !unchanged
                    || probe.is_none()
                    || probe.as_ref().is_some_and(|probe| {
                        probe.id != id.as_str() || probe.version != installed.active.to_string()
                    }) {
                    PluginInstallStatus::Corrupt
                } else if matches!(installed.status, PluginInstallStatus::Unsupported { .. })
                    || probe
                        .as_ref()
                        .is_some_and(|probe| probe.runtime_kind == "script")
                {
                    PluginInstallStatus::Unsupported {
                        reason: PluginUnsupportedReason::ScriptRemoved,
                    }
                } else {
                    PluginInstallStatus::Ready
                };
                (status, Some(actual), probe)
            }
            Err(_) => (PluginInstallStatus::Corrupt, None, None),
        },
        Err(_) => (PluginInstallStatus::Corrupt, None, None),
    };
    // 只有可读且未被篡改的目录才展示清单文本；损坏内容的名称/权限不进入界面。
    let (name, mut permissions, sidecar) = match (&status, probe) {
        (PluginInstallStatus::Ready | PluginInstallStatus::Unsupported { .. }, Some(probe)) => (
            probe.name,
            probe.permissions,
            probe.runtime_kind == "sidecar",
        ),
        _ => (BTreeMap::new(), Vec::new(), false),
    };
    if sidecar && !permissions.iter().any(|value| value == "full-access") {
        // 与本地预检保持一致：Sidecar 的完全访问是运行形态隐含的展示项。
        permissions.push("full-access".into());
    }
    DropInInspection {
        id: id.clone(),
        version: installed.active.clone(),
        name,
        permissions,
        fingerprint: fingerprint.map(|value| value.as_str().to_owned()),
        enabled: installed.enabled,
        confirmed: installed.confirmed,
        status,
        revision,
    }
}

/// 缺失记录移除只在锁内确认目录仍不存在后执行；记录删除不触碰私有数据。
fn remove_missing_record(
    store: &PluginStore,
    expected: &PluginStoreSnapshot,
    id: &PluginId,
) -> AppResult<ExtensionReceipt> {
    let installed = expected
        .state
        .plugins
        .get(id)
        .ok_or_else(|| AppError::conflict("插件记录不存在或不可移除"))?;
    if installed.status != PluginInstallStatus::Missing {
        return Err(AppError::conflict("插件记录不是目录缺失状态"));
    }
    if expected
        .state
        .pending
        .iter()
        .any(|item| item.plugin_id() == id)
    {
        return Err(AppError::conflict("插件已有待处理操作"));
    }
    let directory = layout::package_directory(store.root(), id);
    let mut next = expected.state.clone();
    next.plugins.remove(id);
    // 记录移除后失败文案与可撤销绑定不再有归属；这里没有 pending，清理是安全的。
    next.failures.remove(id);
    next.pending_failures.remove(id);
    let directory_for_publish = directory;
    let snapshot = store
        .publish_and_compare(expected, next, move || {
            // 目录可能在读取状态后又被创建；锁内再确认一次，避免删掉仍可加载的记录。
            match layout::exists(&directory_for_publish) {
                Ok(false) => Ok(()),
                Ok(true) => Err(PluginStoreError::Conflict),
                Err(error) => Err(PluginStoreError::Layout(error)),
            }
        })
        .map_err(store_error)?;
    Ok(ExtensionReceipt {
        revision: snapshot.state.revision,
        // 缺失记录没有被加载，移除后无需重启即可从列表消失。
        restart_required: false,
        cleanup_warning: None,
    })
}

#[async_trait]
impl PluginPackageStore for PackageInstaller {
    /// 应用读取通过共享仓储执行，不创建第二份状态文件。
    async fn state(&self) -> AppResult<PluginStoreState> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            store
                .load()
                .map(|snapshot| snapshot.state)
                .map_err(store_error)
        })
        .await
        .map_err(|e| AppError::infrastructure_with_source("读取插件状态任务失败", e))?
    }

    /// 只枚举已登记的手动放入插件；目录状态按当前磁盘证据计算，不写回状态。
    async fn inspect_drop_ins(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<DropInInspection>> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let snapshot = store.load().map_err(store_error)?;
            let revision = snapshot.state.revision;
            let mut inspections = Vec::new();
            for (id, installed) in &snapshot.state.plugins {
                if installed.source != PluginSource::DropIn {
                    continue;
                }
                if cancellation.is_cancelled() {
                    return Err(AppError::cancelled("手动放入插件检查已取消"));
                }
                inspections.push(inspect_drop_in(store.root(), id, installed, revision));
            }
            Ok(inspections)
        })
        .await
        .map_err(|e| AppError::infrastructure_with_source("手动放入插件检查任务失败", e))?
    }

    /// 确认只排队启用；所有校验和 CAS 在工作线程内完成，避免占用界面线程。
    async fn confirm_drop_in(
        &self,
        id: PluginId,
        expected_fingerprint: String,
        expected_revision: u64,
    ) -> AppResult<ExtensionReceipt> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            this.confirm_drop_in_sync(&id, &expected_fingerprint, expected_revision)
        })
        .await
        .map_err(|e| AppError::outcome_unknown_with_source("插件确认任务异常，请读回状态", e))?
    }

    /// 安装始终重新解析当前目录并核对选中身份，避免确认后目录内容漂移。
    async fn install_catalog(
        &self,
        selection: CatalogSelection,
        progress: tokio::sync::watch::Sender<InstallProgress>,
        cancellation: CancellationToken,
    ) -> AppResult<ExtensionReceipt> {
        let (asset, api) = self.selected_asset(&selection).await?;
        let this = self.clone();
        let size = asset.size;
        let stage = tokio::task::spawn_blocking(move || this.stage(size))
            .await
            .map_err(|e| AppError::infrastructure_with_source("创建插件暂存任务失败", e))??;
        if let Err(error) = download::download(
            &asset,
            &stage.archive(),
            // 目录不再维护主机白名单；下载器保留该参数只为兼容旧调用形状，
            // 实际地址边界由共享 URL 契约与传输层逐跳复核。
            false,
            &progress,
            &cancellation,
        )
        .await
        {
            // 下载已失败且未触碰状态，暂存里只有未知完整度的中间材料：清理尽力
            // 执行，失败时材料原样保留给 Stage::drop 与下次启动的 _incoming
            // 回收，不以清理错误掩盖下载主因，也不伪称材料已确认清除。
            let _ = tokio::task::spawn_blocking(move || stage.cleanup()).await;
            return Err(error);
        }
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let result = {
                // 期望值来自签名目录的选定资产，绑定实际解压的压缩流；先验后解的
                // 两段式已移除，暂存文件在验证与解压之间不存在可替换窗口。
                let verified = archive::ArchiveExpectation {
                    size: asset.size,
                    sha256: asset.sha256.clone(),
                };
                this.prepare_and_commit(
                    &stage,
                    PluginSource::Official,
                    Some((&selection, &api)),
                    &verified,
                    &progress,
                    &cancellation,
                )
            };
            // 状态已提交时安装必然生效：清理失败不能把成功回执吞成错误（会被
            // 当成未安装而重试），只能以 cleanup_warning 附在回执上如实上报；
            // 材料去留由下次启动的 _incoming/staging 回收判定。
            match result {
                Ok(receipt) => Ok(ExtensionReceipt {
                    cleanup_warning: stage.cleanup().err(),
                    ..receipt
                }),
                Err(error) => {
                    // 安装未受理时主因优先；清理失败只可能留下临时材料，原样
                    // 交给启动回收，不用 outcome-unknown 掩盖真实失败类别。
                    let _ = stage.cleanup();
                    Err(error)
                }
            }
        })
        .await
        .map_err(|e| AppError::outcome_unknown_with_source("插件安装任务异常，请读回状态", e))?
    }

    /// 使用与正式安装相同的解包和宿主校验，摘要将后续确认绑定到这份实际内容。
    async fn inspect_file(
        &self,
        path: PathBuf,
        cancellation: CancellationToken,
    ) -> AppResult<LocalPackageInspection> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| AppError::infrastructure_with_source("读取本地插件包失败", e))?;
            if !metadata.is_file()
                || !safe_metadata(&metadata)
                || metadata.len() == 0
                || metadata.len() > MAX_LOCAL_PACKAGE_BYTES
            {
                return Err(AppError::invalid("本地插件包无效"));
            }
            let stage = this.stage(metadata.len())?;
            let result = (|| {
                copy_local(&path, &stage.archive(), &cancellation)?;
                // 摘要与字节数来自同一次顺序读取，是本次预检唯一的事实来源；
                // 展示的 size/摘要与流验证期望共用这一对值，复制源并发增长
                // 只在入口与 copy_local 的门禁处拒绝，不在观测点静默混搭。
                let (observed_size, digest_hex) = file_digest(&stage.archive(), &cancellation)?;
                let extracted = stage.directory.join("package");
                fs::create_dir(&extracted)
                    .map_err(|e| AppError::infrastructure_with_source("创建插件解压目录失败", e))?;
                // 预检摘要先于展示值固定，解压在这一次打开内用同一摘要核对实际
                // 压缩流；被替换的暂存文件在这里失败，预检信息与展示内容由此
                // 不能分离。摘要在 stage 内部文件上重算一次不算双重校验：它把
                // 复制进来的内容固化为期望值，而 unpack_verified 的比对对象是
                // 实际解压流。
                let verified = archive::ArchiveExpectation {
                    size: observed_size,
                    sha256: digest_hex.clone(),
                };
                // 测试注入点：模拟“摘要计算后、解压前”窗口内的暂存包替换，
                // 验证展示摘要与实际解压流不可分离；run 在生产构建内部跳过，
                // 只对本安装器配置根生效。
                test_hooks::before_unpack::run(this.store.root(), &stage.archive());
                archive::unpack_verified(&stage.archive(), &extracted, &cancellation, &verified)?;
                let package = kerminal_plugin_protocol::validate_package(
                    &extracted,
                    ManifestPolicy::ThirdParty,
                )
                .map_err(|e| AppError::infrastructure_with_source("插件包校验失败", e))?;
                let id = PluginId::parse(&package.manifest.id)
                    .map_err(|_| AppError::invalid("插件包身份无效"))?;
                // 预检只返回待安装信息，不能在确认阶段才暴露保留身份。
                if is_reserved_plugin_id(id.as_str()) {
                    return Err(AppError::invalid("插件身份与内置单元冲突"));
                }
                let version = PluginVersion::parse(&package.manifest.version)
                    .map_err(|_| AppError::invalid("插件包版本无效"))?;
                ProtocolBootValidator {
                    host_api: this.host_api.clone(),
                    platform: this.platform,
                }
                .validate(&extracted, &id, &version, PluginSource::Local)
                .map_err(|_| AppError::invalid("插件包与当前宿主不兼容"))?;
                // 权限只作为界面展示：声明原样保留，Sidecar 的完全访问由运行形态隐含。
                let mut permissions = package.manifest.permissions.clone();
                if package.manifest.runtime.is_sidecar()
                    && !permissions.iter().any(|value| value == "full-access")
                {
                    permissions.push("full-access".into());
                }
                Ok(LocalPackageInspection {
                    id,
                    version,
                    name: package.manifest.name.clone(),
                    permissions,
                    size: observed_size,
                    sha256: digest_hex,
                })
            })();
            // 预检不触碰状态：清理失败时临时材料原样交给 Stage::drop 与启动
            // 回收；主因优先上抛，不用清理错误把真实失败类别改写成 outcome
            // unknown，也不吞掉已成功算出的预检结果。
            match result {
                Ok(inspection) => Ok(inspection),
                Err(error) => {
                    let _ = stage.cleanup();
                    Err(error)
                }
            }
        })
        .await
        .map_err(|e| AppError::infrastructure_with_source("插件预检任务失败", e))?
    }

    /// 本地安装独立于目录来源，但仍执行同样的包与宿主兼容性验证。
    async fn install_file(
        &self,
        path: PathBuf,
        inspected: LocalPackageInspection,
        progress: tokio::sync::watch::Sender<InstallProgress>,
        cancellation: CancellationToken,
    ) -> AppResult<ExtensionReceipt> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| AppError::infrastructure_with_source("读取本地插件包失败", e))?;
            let stage = this.stage(metadata.len())?;
            let result = (|| {
                copy_local(&path, &stage.archive(), &cancellation)?;
                // 期望值取自用户已确认的预检结果，绑定实际解压的压缩流；复制后
                // 的暂存文件若被替换，解压端摘要比对失败，状态不会受理。
                let verified = archive::ArchiveExpectation {
                    size: inspected.size,
                    sha256: inspected.sha256.clone(),
                };
                this.prepare_and_commit(
                    &stage,
                    PluginSource::Local,
                    None,
                    &verified,
                    &progress,
                    &cancellation,
                )
            })();
            // 与市场安装同一收尾契约：提交成功后清理失败以 cleanup_warning
            // 附在回执上，安装结果不被改写；未受理时主因优先，材料交启动回收。
            match result {
                Ok(receipt) => Ok(ExtensionReceipt {
                    cleanup_warning: stage.cleanup().err(),
                    ..receipt
                }),
                Err(error) => {
                    let _ = stage.cleanup();
                    Err(error)
                }
            }
        })
        .await
        .map_err(|e| AppError::outcome_unknown_with_source("插件安装任务异常，请读回状态", e))?
    }

    /// 普通操作只排队；失败撤销走同一 CAS 门禁，不能借恢复入口绕过未知提交围栏。
    async fn change(&self, change: ExtensionChange) -> AppResult<ExtensionReceipt> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let expected = store.load().map_err(store_error)?;
            let change = match change {
                ExtensionChange::CancelFailedChange {
                    expected_revision,
                    pending,
                } => {
                    return recovery::cancel_failed_change(
                        &store,
                        &expected,
                        expected_revision,
                        &pending,
                    );
                }
                // 缺失记录移除只看当前权威状态；目录是否存在在锁内再次确认。
                ExtensionChange::RemoveMissingRecord(id) => {
                    // 内置身份没有外部安装记录语义，直接拒绝可避免进入目录检查。
                    if is_reserved_plugin_id(id.as_str()) {
                        return Err(AppError::invalid("插件身份与内置单元冲突"));
                    }
                    return remove_missing_record(&store, &expected, &id);
                }
                change => change,
            };
            let mut next = expected.state.clone();
            if matches!(&change, ExtensionChange::Rollback(_)) {
                // 单版本布局没有可回退的上一版本；保留枚举只为界面编译兼容。
                return Err(AppError::invalid("插件回退功能已移除，请重新安装旧版本"));
            }
            let id = match &change {
                ExtensionChange::Enable(id)
                | ExtensionChange::Disable(id)
                | ExtensionChange::Uninstall(id) => id,
                ExtensionChange::Rollback(_) => {
                    return Err(AppError::invalid("插件回退功能已移除，请重新安装旧版本"));
                }
                ExtensionChange::CancelFailedChange { .. } => {
                    return Err(AppError::invalid("失败变更取消入口无效"));
                }
                ExtensionChange::RemoveMissingRecord(_) => {
                    return Err(AppError::invalid("缺失记录移除入口无效"));
                }
            };
            if next.pending.iter().any(|item| item.plugin_id() == id) {
                return Err(AppError::conflict("插件已有待处理操作"));
            }
            let pending = if is_builtin_unit(id.as_str()) {
                // 内置单元不是安装出来的包：开关用缺省启用表示，未写入等同 true。
                // 外部记录若占用同一身份仍留在 state.plugins，但这里只操作内置开关。
                match &change {
                    ExtensionChange::Enable(_) => {
                        if next.builtins.get(id).copied().unwrap_or(true) {
                            return Err(AppError::conflict("插件已处于目标状态"));
                        }
                        PendingPluginChange::Enable { id: id.clone() }
                    }
                    ExtensionChange::Disable(_) => {
                        if id.as_str() == CORE_PLUGIN_ID {
                            return Err(AppError::invalid("核心插件不可停用"));
                        }
                        if !next.builtins.get(id).copied().unwrap_or(true) {
                            return Err(AppError::conflict("插件已处于目标状态"));
                        }
                        PendingPluginChange::Disable { id: id.clone() }
                    }
                    ExtensionChange::Uninstall(_) => {
                        return Err(AppError::invalid("插件身份与内置单元冲突"));
                    }
                    _ => return Err(AppError::invalid("插件身份与内置单元冲突")),
                }
            } else {
                let installed = next
                    .plugins
                    .get(id)
                    .ok_or_else(|| AppError::not_found("插件未安装"))?;
                if matches!(&change, ExtensionChange::Enable(_))
                    && installed.source == PluginSource::DropIn
                    && !installed.confirmed
                {
                    // 普通 Enable 不核对目录指纹；未确认的 drop-in 只能走 confirm_drop_in，
                    // 否则旧界面可以直接把手工放入的内容提升成受信任插件。
                    return Err(AppError::conflict("手动放入的插件需要先确认启用"));
                }
                match change {
                    ExtensionChange::Enable(id) if !installed.enabled => {
                        PendingPluginChange::Enable { id }
                    }
                    ExtensionChange::Disable(id) if installed.enabled => {
                        PendingPluginChange::Disable { id }
                    }
                    ExtensionChange::Uninstall(id) => PendingPluginChange::Uninstall {
                        id,
                        // UUID 让卸载备份在崩溃后仍能被 journal 唯一定位。
                        operation_id: uuid::Uuid::new_v4().to_string(),
                    },
                    _ => return Err(AppError::conflict("插件已处于目标状态")),
                }
            };
            next.pending.push(pending);
            let snapshot = store
                .compare_and_swap(&expected, next)
                .map_err(store_error)?;
            Ok(ExtensionReceipt {
                revision: snapshot.state.revision,
                restart_required: true,
                cleanup_warning: None,
            })
        })
        .await
        .map_err(|e| AppError::outcome_unknown_with_source("插件状态任务异常，请读回状态", e))?
    }

    /// 设置服务负责设置、数据和 Vault 的分步删除与未知结果报告。
    async fn delete_data(&self, id: PluginId) -> AppResult<()> {
        self.settings
            .delete_data(&id, CancellationToken::new())
            .await
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/plugin_install_flow.rs"]
mod tests;
