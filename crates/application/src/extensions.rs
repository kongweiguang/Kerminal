//! 扩展页用例与目录、安装存储之间的边界。
//! @author kongweiguang

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use kerminal_domain::plugins::{
    PendingPluginChange, PluginId, PluginInstallStatus, PluginStoreState, PluginVersion,
};
use kerminal_plugin_protocol::Platform;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{
    AppError, AppResult,
    plugins::{CORE_PLUGIN_ID, PluginRegistry, is_builtin_unit},
};

/// 官方目录地址是产品缺省值；设置留空时组合根也归一化为该地址。
///
/// 放在应用层是为了让设置校验、组合根和界面共用同一个稳定常量，避免各层各自
/// 复制字符串后出现“空值到底代表哪个源”的分歧。自定义源只需要替换运行期地址，
/// 不改变目录 JSON 结构或安装记录格式。
///
/// @author kongweiguang
pub const DEFAULT_PLUGIN_CATALOG_URL: &str =
    "https://github.com/kongweiguang/Kerminal-Plugins/releases/download/catalog-v1/catalog.json";

/// 目录是宿主加载并校验后的只读投影；来源由当前配置的目录地址决定，不由条目声明。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCatalog {
    pub schema: u32,
    pub source_id: String,
    pub sequence: u64,
    pub plugins: Vec<CatalogPlugin>,
}

/// 本地化文案保留目录原文，界面可按当前语言选择。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogPlugin {
    pub id: PluginId,
    pub name: BTreeMap<String, String>,
    pub description: BTreeMap<String, String>,
    pub publisher: String,
    pub categories: Vec<String>,
    pub icon_url: Option<String>,
    pub readme_url: Option<String>,
    pub repository: String,
    pub permissions: Vec<String>,
    pub versions: Vec<CatalogVersion>,
}

/// 版本与资产绑定在同一份目录中，不能从界面拼接 URL。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogVersion {
    pub version: PluginVersion,
    pub api: String,
    pub published_at: String,
    pub changelog_url: Option<String>,
    pub assets: Vec<CatalogAsset>,
}

impl CatalogVersion {
    /// 与清单校验共享 npm 范围语义，避免可安装版本被界面错误过滤。
    pub fn compatible_with(&self, host_api: &str) -> AppResult<bool> {
        let requirement = node_semver::Range::parse(&self.api)
            .map_err(|_| AppError::invalid("插件目录 API 范围无效"))?;
        let version = node_semver::Version::parse(host_api)
            .map_err(|_| AppError::invalid("宿主 API 版本无效"))?;
        Ok(requirement.satisfies(&version))
    }

    /// 平台资产对应 Sidecar 包；完全访问只用于界面展示，不再限制运行能力。
    pub fn requires_full_access(&self) -> bool {
        self.assets
            .iter()
            .any(|asset| asset.platform != Platform::Any)
    }
}

/// 安装进度按阶段报告，完成状态由操作回执而非进度事件证明。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstallProgress {
    Preparing,
    Downloading { received: u64, total: u64 },
    Verifying,
    Publishing,
}
/// 由应用层创建进度通道，界面无需直接依赖运行时实现。
pub fn install_progress_channel() -> (
    watch::Sender<InstallProgress>,
    watch::Receiver<InstallProgress>,
) {
    watch::channel(InstallProgress::Preparing)
}

/// 摘要和大小来自目录声明；目录不再验签，因此下载后仍必须按原始字节核对完整性。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogAsset {
    pub platform: Platform,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

/// 目录身份锚点：来源地址、目录来源标识、序列与原始字节摘要共同证明用户看到的是哪一份目录。
///
/// 摘要必须基于加载时的原始字节，而不是重新序列化后的结构体：字段顺序、空白等
/// 排版差异都属于用户实际看到的同一份原文，重新序列化会把这些差异抹平，使
/// “确认后目录已变化”无法被识别。`source_url` 额外绑定当前配置的目录地址，
/// 防止两个来源恰好提供相同原文时，旧确认被拿到另一来源继续安装。
/// 应用层只承载身份，摘要计算留在基础设施。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogIdentity {
    pub source_id: String,
    pub sequence: u64,
    pub raw_sha256: String,
    /// 当前生效的目录地址；由基础设施在构造身份时写入，界面只做透传。
    pub source_url: String,
}

impl CatalogIdentity {
    /// 由加载结果构造身份；调用方必须传入同一次加载的原文摘要，不能事后重新计算。
    ///
    /// 保留三参数构造是为了让不关心来源地址的测试替身继续可用；生产路径必须再调用
    /// `with_source_url` 绑定实际地址，否则安装器会拒绝跨来源确认。
    ///
    /// @author kongweiguang
    pub fn new(source_id: impl Into<String>, sequence: u64, raw_sha256: impl Into<String>) -> Self {
        Self {
            source_id: source_id.into(),
            sequence,
            raw_sha256: raw_sha256.into(),
            source_url: String::new(),
        }
    }

    /// 绑定当前目录地址；地址是确认与安装之间的来源边界，不能由界面单独替换。
    ///
    /// @author kongweiguang
    pub fn with_source_url(mut self, source_url: impl Into<String>) -> Self {
        self.source_url = source_url.into();
        self
    }
}

/// 刷新失败时可显示已加载缓存，并明确提示目录未更新。
///
/// 快照与身份一起传递，界面选择安装目标时才能把“用户看到的目录”冻结到确认动作；
/// 只带结构体会让刷新竞态在确认期间静默换包。
///
/// @author kongweiguang
#[derive(Clone, Debug)]
pub struct CatalogSnapshot {
    pub catalog: PluginCatalog,
    pub identity: CatalogIdentity,
    pub stale: bool,
    pub fetched_at: u64,
}

/// 安装目标只包含可验证身份，目录来源和下载地址由基础设施决定。
///
/// `identity` 冻结确认时看到的目录；安装器在解析资产前必须按它重新读取当前来源
/// 缓存，来源地址、目录已刷新到另一序列或另一份原文时返回冲突，要求用户重新确认
/// 而不是静默换包。
///
/// @author kongweiguang
#[derive(Clone, Debug)]
pub struct CatalogSelection {
    pub id: PluginId,
    pub version: PluginVersion,
    pub platform: Platform,
    pub identity: CatalogIdentity,
}

/// 扩展页展示的内置单元静态身份，不包含运行时启停结果。
///
/// 内置单元不存在于插件安装记录中，因此这里只承载身份与可关闭性：启用态仍以
/// 权威 `PluginStoreState.builtins`（缺省 true）为准。页面通过本 DTO 枚举条目，
/// 无需复制内置名单或直接持有 `PluginRegistry`。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltinUnit {
    /// 固定稳定身份；显示名变化不影响持久引用。
    pub id: PluginId,
    /// 注册表原文名称；本地化由界面按 `id` 处理，应用层不做语言选择。
    pub name: String,
    /// 核心单元为 false，其余内置单元允许关闭；持久化受理由存储端负责。
    pub can_disable: bool,
}

/// 不同操作只表达用户意图，状态迁移由存储端核对当前权威快照。
#[derive(Clone, Debug)]
pub enum ExtensionChange {
    Enable(PluginId),
    Disable(PluginId),
    /// 单版本目录已没有可回退版本；变体只为 UI 编译兼容保留，服务层固定拒绝。
    Rollback(PluginId),
    Uninstall(PluginId),
    /// 目录缺失的登记项只移除记录，不触碰 `data/` 与磁盘材料；有无 pending 由存储端核对。
    RemoveMissingRecord(PluginId),
    /// 取消必须绑定用户已看过的失败意图，避免旧页面撤销更新后的另一笔操作。
    CancelFailedChange {
        expected_revision: u64,
        pending: PendingPluginChange,
    },
}

/// 手动放入插件的只读观察结果；确认时用指纹与 revision 绑定用户实际看到的目录内容。
#[derive(Clone, Debug)]
pub struct DropInInspection {
    pub id: PluginId,
    pub version: PluginVersion,
    /// 名称按语言保留原文，界面自行选择；目录损坏或清单不可读时为空。
    pub name: BTreeMap<String, String>,
    /// 权限仅作描述；Sidecar 隐含的完全访问由基础设施追加。
    pub permissions: Vec<String>,
    /// 检查时目录的内容摘要；目录缺失或无法安全读取时为空，界面不能据此确认。
    pub fingerprint: Option<String>,
    pub enabled: bool,
    pub confirmed: bool,
    /// 如实报告磁盘证据，不把损坏、缺失或旧运行时伪造成已就绪。
    pub status: PluginInstallStatus,
    /// 本次观察对应的状态修订，确认时用于拒绝过期页面。
    pub revision: u64,
}

/// 预检结果绑定待确认的文件摘要；权限字段仅用于界面展示，安装时不再校验授权。
#[derive(Clone, Debug)]
pub struct LocalPackageInspection {
    pub id: PluginId,
    pub version: PluginVersion,
    pub name: BTreeMap<String, String>,
    /// 清单声明原样保留；Sidecar 追加 "full-access" 仅作展示。
    pub permissions: Vec<String>,
    pub size: u64,
    pub sha256: String,
}

/// 实际提交的修订供界面回读；运行态需重启后才应用 pending。
#[derive(Clone, Debug)]
pub struct ExtensionReceipt {
    pub revision: u64,
    pub restart_required: bool,
    /// 状态已提交但危险残留未删除；调用方仍须回读状态并明确展示清理问题。
    pub cleanup_warning: Option<AppError>,
}

#[async_trait]
pub trait CatalogSource: Send + Sync + 'static {
    /// 当前生效的目录地址；始终返回规范化后的实际 URL。
    ///
    /// 空配置在构造时已经折叠为官方默认地址，因此这里不会返回空串：界面与安装身份
    /// 都拿同一个实际地址比较，避免“空串=默认”的隐式规则在跨层传递中丢失。
    ///
    /// @author kongweiguang
    fn source_url(&self) -> String;

    /// 切换目录地址；先按统一 URL 规则验证，成功才原子替换，失败保留旧来源。
    ///
    /// 不提供静默 no-op 默认实现：不支持切源的实现必须显式报错，否则界面会把
    /// “未切换”当成“已保存并应用”。空串表示回到官方默认地址。
    ///
    /// @author kongweiguang
    fn set_source_url(&self, source_url: &str) -> AppResult<()>;

    /// 读取当前来源上次加载成功的目录，缺失时返回 None。
    async fn cached(&self) -> AppResult<Option<CatalogSnapshot>>;
    /// 只读取当前目录中指定的 README 地址，正文由调用方按不可信文本处理。
    async fn readme(
        &self,
        id: &PluginId,
        cancellation: CancellationToken,
    ) -> AppResult<Option<String>>;
    /// 刷新失败不得覆盖缓存；支持取消网络读取。
    async fn refresh(&self, cancellation: CancellationToken) -> AppResult<CatalogSnapshot>;
}

#[async_trait]
pub trait PluginPackageStore: Send + Sync + 'static {
    /// 读取唯一权威状态，损坏或未知提交不能伪装为空状态。
    async fn state(&self) -> AppResult<PluginStoreState>;
    /// 自行从受信目录重新解析选择，不能采纳界面给出的 URL 或摘要。
    async fn install_catalog(
        &self,
        selection: CatalogSelection,
        progress: watch::Sender<InstallProgress>,
        cancellation: CancellationToken,
    ) -> AppResult<ExtensionReceipt>;
    /// 展示确认界面前先安全解包并校验，不能从文件名推测权限。
    async fn inspect_file(
        &self,
        path: PathBuf,
        cancellation: CancellationToken,
    ) -> AppResult<LocalPackageInspection>;
    /// 确认后的本地文件仍需完整包校验，来源固定为 local。
    async fn install_file(
        &self,
        path: PathBuf,
        inspected: LocalPackageInspection,
        progress: watch::Sender<InstallProgress>,
        cancellation: CancellationToken,
    ) -> AppResult<ExtensionReceipt>;
    /// 每个意图基于当前状态执行 CAS，不重放未知结果。
    async fn change(&self, change: ExtensionChange) -> AppResult<ExtensionReceipt>;
    /// 只读列出手动放入的插件；目录缺失、损坏或旧运行时也必须如实报告，不能修改状态。
    async fn inspect_drop_ins(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<DropInInspection>>;
    /// 确认必须绑定检查时的指纹与修订；成功后只排队启用，运行态在重启后生效。
    async fn confirm_drop_in(
        &self,
        id: PluginId,
        expected_fingerprint: String,
        expected_revision: u64,
    ) -> AppResult<ExtensionReceipt>;
    /// 数据删除是独立的显式操作，仓储核对插件生命周期与路径安全。
    async fn delete_data(&self, id: PluginId) -> AppResult<()>;
}

/// 本次运行的模式独立于磁盘上的插件启用偏好；显式安全模式会随受管重启保留。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalPluginStartupMode {
    Normal,
    AutomaticSafeMode,
    ExplicitSafeMode,
}

/// 应用层只编排受信端口；网络、磁盘和重启均由组合根提供。
pub struct ExtensionsService {
    catalog: Arc<dyn CatalogSource>,
    packages: Arc<dyn PluginPackageStore>,
    startup_mode: ExternalPluginStartupMode,
}

impl ExtensionsService {
    /// 注入共享仓储和目录，避免界面构造另一套信任配置。未注入启动模式的调用方按普通启动处理。
    pub fn new(catalog: Arc<dyn CatalogSource>, packages: Arc<dyn PluginPackageStore>) -> Self {
        Self {
            catalog,
            packages,
            startup_mode: ExternalPluginStartupMode::Normal,
        }
    }

    /// 构建时接收冻结的实际启动模式，扩展页无需根据可变安装状态猜测运行许可。
    pub fn with_startup_mode(mut self, startup_mode: ExternalPluginStartupMode) -> Self {
        self.startup_mode = startup_mode;
        self
    }

    /// 返回整个进程保持不变的加载模式，让刷新、筛选和网络失败都不会清除恢复提示。
    pub fn startup_mode(&self) -> ExternalPluginStartupMode {
        self.startup_mode
    }

    /// 回显当前实际目录地址；官方默认来源也返回完整 URL，界面据此展示当前来源。
    ///
    /// @author kongweiguang
    pub fn source_url(&self) -> String {
        self.catalog.source_url()
    }

    /// 设置保存成功并回读后切换到新目录来源。
    ///
    /// 只切换运行态端口，不在这里写设置：写入失败时来源必须保持原值，
    /// 否则界面会显示一份并未落盘的配置。切换失败原样上抛，由界面提示并保留旧来源；
    /// 切换后旧来源的刷新结果由端口按来源快照丢弃，不会写进新来源缓存。
    ///
    /// @author kongweiguang
    pub fn set_source_url(&self, source_url: &str) -> AppResult<()> {
        self.catalog.set_source_url(source_url)
    }

    /// 枚举内置单元供扩展页展示；只读冻结目录，不触碰目录端口或安装存储。
    ///
    /// 页面在 activate 时同步调用即可，顺序与 `providers()` 的注册顺序一致，
    /// 避免界面另维护名单后与启动装配漂移。身份转换失败说明内置声明本身无效，
    /// 属于启动级基础设施问题，因此带 source 返回而不是静默丢弃条目。
    ///
    /// @author kongweiguang
    pub fn builtin_units(&self) -> AppResult<Vec<BuiltinUnit>> {
        let registry = PluginRegistry::builtin()?;
        registry
            .providers()
            .iter()
            .map(|descriptor| {
                let id = PluginId::parse(descriptor.id.as_str()).map_err(|source| {
                    AppError::infrastructure_with_source("内置插件身份无效", source)
                })?;
                Ok(BuiltinUnit {
                    id,
                    name: descriptor.name.clone(),
                    can_disable: descriptor.can_disable,
                })
            })
            .collect()
    }

    /// 缓存和刷新分开，页面可以先显示已有内容再异步更新。
    pub async fn cached_catalog(&self) -> AppResult<Option<CatalogSnapshot>> {
        self.catalog.cached().await
    }

    /// 取消由调用方持有，端口负责停止下载并保留可信缓存。
    pub async fn refresh_catalog(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<CatalogSnapshot> {
        self.catalog.refresh(cancellation).await
    }

    /// 日常刷新以缓存时间为界；系统时钟倒退时保守刷新而非延长缓存寿命。
    pub async fn refresh_if_due(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<CatalogSnapshot> {
        if let Some(snapshot) = self.catalog.cached().await? {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| AppError::infrastructure("系统时间无效"))?
                .as_secs();
            if now >= snapshot.fetched_at && now - snapshot.fetched_at < 24 * 60 * 60 {
                return Ok(snapshot);
            }
        }
        self.catalog.refresh(cancellation).await
    }
    /// README 的链接只能来自当前已加载目录，界面不能传入任意 URL。
    pub async fn readme(
        &self,
        id: &PluginId,
        cancellation: CancellationToken,
    ) -> AppResult<Option<String>> {
        self.catalog.readme(id, cancellation).await
    }
    /// 查询在当前已加载快照上执行，分类与文本筛选不会改变目录或触发网络。
    pub fn search<'a>(
        &self,
        catalog: &'a PluginCatalog,
        query: &str,
        category: Option<&str>,
    ) -> Vec<&'a CatalogPlugin> {
        let query = query.trim().to_lowercase();
        catalog
            .plugins
            .iter()
            .filter(|plugin| {
                let category_matches = category
                    .is_none_or(|category| plugin.categories.iter().any(|value| value == category));
                category_matches
                    && (query.is_empty()
                        || plugin.id.as_str().contains(&query)
                        || plugin.publisher.to_lowercase().contains(&query)
                        || plugin
                            .name
                            .values()
                            .chain(plugin.description.values())
                            .any(|value| value.to_lowercase().contains(&query)))
            })
            .collect()
    }
    /// 展示始终以权威状态为准，不能以请求已受理推断已经生效。
    pub async fn state(&self) -> AppResult<PluginStoreState> {
        self.packages.state().await
    }

    /// 目录资产只通过稳定身份选择，防止页面把界面显示的内容替换成另一份目录。
    ///
    /// 目录不再验签，因此身份里绑定来源地址和原始字节摘要；安装器据此拒绝
    /// 已切换来源或已刷新目录的过期确认。
    ///
    /// @author kongweiguang
    pub async fn install(
        &self,
        selection: CatalogSelection,
        progress: watch::Sender<InstallProgress>,
        cancellation: CancellationToken,
    ) -> AppResult<ExtensionReceipt> {
        self.packages
            .install_catalog(selection, progress, cancellation)
            .await
    }

    /// 在展示权限前校验实际包，预检不会安排安装或更改权威状态。
    pub async fn inspect_file(
        &self,
        path: PathBuf,
        cancellation: CancellationToken,
    ) -> AppResult<LocalPackageInspection> {
        self.packages.inspect_file(path, cancellation).await
    }

    /// 本地包必须先经用户确认，调用方不能把未审核文件伪装成官方包。
    pub async fn install_file(
        &self,
        path: PathBuf,
        inspected: LocalPackageInspection,
        confirmed: bool,
        progress: watch::Sender<InstallProgress>,
        cancellation: CancellationToken,
    ) -> AppResult<ExtensionReceipt> {
        if !confirmed {
            return Err(AppError::invalid("本地插件尚未确认未经市场审核"));
        }
        self.packages
            .install_file(path, inspected, progress, cancellation)
            .await
    }

    /// 生命周期操作只排队；取消失败意图只撤销该次排队，不热替换运行中的贡献。
    ///
    /// 回退变体只为界面编译保留：单版本目录没有旧版本可切回，固定返回类型化错误，
    /// 且不把任何排队意图写入存储。内置单元不存在安装记录，卸载类意图在转发前拒绝；
    /// 核心单元也不能停用。这些校验都发生在触达存储之前，避免仓储承担 UI 语义判断。
    ///
    /// @author kongweiguang
    pub async fn change(&self, change: ExtensionChange) -> AppResult<ExtensionReceipt> {
        if matches!(&change, ExtensionChange::Rollback(_)) {
            return Err(AppError::invalid("插件回退已移除"));
        }
        match &change {
            // 核心单元是应用基本可用的前提，必须始终启用；先于通用内置判断返回明确文案。
            ExtensionChange::Disable(id) if id.as_str() == CORE_PLUGIN_ID => {
                return Err(AppError::invalid("核心插件不可停用"));
            }
            // 内置单元没有安装记录，卸载与移除缺失记录都无从执行；启停则正常排队。
            ExtensionChange::Uninstall(id) | ExtensionChange::RemoveMissingRecord(id)
                if is_builtin_unit(id.as_str()) =>
            {
                return Err(AppError::invalid("内置单元没有安装记录"));
            }
            _ => {}
        }
        self.packages.change(change).await
    }

    /// 手动放入插件默认未确认；这里只转发只读观察，是否可确认由目录证据决定。
    pub async fn inspect_drop_ins(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<DropInInspection>> {
        self.packages.inspect_drop_ins(cancellation).await
    }

    /// 指纹与修订都来自用户看过的检查结果；存储端在锁内重算后再提交启用。
    pub async fn confirm_drop_in(
        &self,
        id: PluginId,
        expected_fingerprint: String,
        expected_revision: u64,
    ) -> AppResult<ExtensionReceipt> {
        self.packages
            .confirm_drop_in(id, expected_fingerprint, expected_revision)
            .await
    }

    /// 删除数据需要界面传入明确确认，卸载不会隐式触碰数据。
    pub async fn delete_data(&self, id: PluginId, confirmed: bool) -> AppResult<()> {
        if !confirmed {
            return Err(AppError::invalid("删除插件数据尚未确认"));
        }
        self.packages.delete_data(id).await
    }
}
