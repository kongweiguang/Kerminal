//! 扩展页用例与受信目录、安装存储之间的边界。
//! @author kongweiguang

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use kerminal_domain::plugins::{PendingPluginChange, PluginId, PluginStoreState, PluginVersion};
use kerminal_plugin_protocol::Platform;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{AppError, AppResult};

/// 目录是验签后的只读投影；来源由宿主固定，不由条目声明。
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

/// 版本与资产绑定在经过验签的同一目录中，不能从界面拼接 URL。
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

    /// 平台资产对应 Sidecar 包，按协议隐含完全访问权限。
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

/// 摘要和大小由目录签名保护，下载完成后仍需按原始字节核对。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogAsset {
    pub platform: Platform,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

/// 刷新失败时可显示已验签缓存，并明确提示目录未更新。
#[derive(Clone, Debug)]
pub struct CatalogSnapshot {
    pub catalog: PluginCatalog,
    pub stale: bool,
    pub fetched_at: u64,
}

/// 安装目标只包含可验证身份，目录来源和下载地址由基础设施决定。
#[derive(Clone, Debug)]
pub struct CatalogSelection {
    pub id: PluginId,
    pub version: PluginVersion,
    pub platform: Platform,
}

/// 不同操作只表达用户意图，状态迁移由存储端核对当前权威快照。
#[derive(Clone, Debug)]
pub enum ExtensionChange {
    Enable(PluginId),
    Disable(PluginId),
    Rollback(PluginId),
    Uninstall(PluginId),
    /// 取消必须绑定用户已看过的失败意图，避免旧页面撤销更新后的另一笔操作。
    CancelFailedChange {
        expected_revision: u64,
        pending: PendingPluginChange,
    },
}

/// 预检结果绑定待确认的文件摘要，展示权限包含 Sidecar 的隐含完全访问。
#[derive(Clone, Debug)]
pub struct LocalPackageInspection {
    pub id: PluginId,
    pub version: PluginVersion,
    pub name: BTreeMap<String, String>,
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
    /// 读取上次验签成功的目录，缺失时返回 None。
    async fn cached(&self) -> AppResult<Option<CatalogSnapshot>>;
    /// 只读取已验签目录中指定的 README 地址，正文由调用方按不可信文本处理。
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
    /// 数据删除是独立的显式操作，仓储核对插件生命周期与路径安全。
    async fn delete_data(&self, id: PluginId) -> AppResult<()>;
}

/// 应用层只编排受信端口；网络、磁盘和重启均由组合根提供。
pub struct ExtensionsService {
    catalog: Arc<dyn CatalogSource>,
    packages: Arc<dyn PluginPackageStore>,
}

impl ExtensionsService {
    /// 注入共享仓储和目录，避免界面构造另一套信任配置。
    pub fn new(catalog: Arc<dyn CatalogSource>, packages: Arc<dyn PluginPackageStore>) -> Self {
        Self { catalog, packages }
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
    /// README 的链接只能来自当前验签目录，界面不能传入任意 URL。
    pub async fn readme(
        &self,
        id: &PluginId,
        cancellation: CancellationToken,
    ) -> AppResult<Option<String>> {
        self.catalog.readme(id, cancellation).await
    }
    /// 查询在已验签快照上执行，分类与文本筛选不会改变目录或触发网络。
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

    /// 目录资产只通过稳定身份选择，防止页面伪造签名目录内容。
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
    pub async fn change(&self, change: ExtensionChange) -> AppResult<ExtensionReceipt> {
        self.packages.change(change).await
    }

    /// 删除数据需要界面传入明确确认，卸载不会隐式触碰数据。
    pub async fn delete_data(&self, id: PluginId, confirmed: bool) -> AppResult<()> {
        if !confirmed {
            return Err(AppError::invalid("删除插件数据尚未确认"));
        }
        self.packages.delete_data(id).await
    }
}
