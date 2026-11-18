//! 可配置 HTTP(S) 目录、按来源隔离的缓存与原子提交。
//! @author kongweiguang

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    extensions::{
        CatalogIdentity, CatalogSnapshot, CatalogSource, DEFAULT_PLUGIN_CATALOG_URL, PluginCatalog,
    },
};
use reqwest::{Client, redirect};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, RwLock},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

use crate::storage::{AtomicCommitError, CommitMode, StorageCoordinator, commit_staged_file};

/// 官方地址使用固定缓存名，保证升级后仍能读到旧签名缓存。
const OFFICIAL_CACHE_FILE: &str = "official.json";
const MAX_CATALOG: usize = 4 * 1024 * 1024;
/// 旧签名缓存仍可能带有最多 16 KiB 的签名；只为读取兼容保留上限，新写入不再产生签名。
const MAX_SIGNATURE: usize = 16 * 1024;
const MAX_SOURCE_ID: usize = 128;
const MAX_SOURCE_URL: usize = 2048;

/// 目录来源快照把规范化地址和缓存文件绑定在一起。
///
/// 每次异步操作开始时克隆一份快照，后续网络、解析和提交都只使用这份快照；
/// 运行期切换来源只会替换共享状态里的下一份快照，不会让已经在途的刷新把结果
/// 写进新来源的缓存，也不会让旧来源的确认落到新来源。
#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceState {
    /// 规范化后的实际 URL；空配置在构造时已经折叠为官方默认地址。
    url: String,
    /// 缓存文件名由地址稳定派生，官方地址继续使用 `official.json`。
    cache_file: String,
}

impl SourceState {
    /// 官方默认来源不经过 URL 解析，避免启动路径依赖网络库的解析细节。
    fn official() -> Self {
        Self {
            url: DEFAULT_PLUGIN_CATALOG_URL.to_owned(),
            cache_file: OFFICIAL_CACHE_FILE.to_owned(),
        }
    }

    /// 空配置表示官方默认；自定义地址先按共享 URL 契约验证再派生缓存文件。
    ///
    /// 缓存名使用规范化 URL 的 SHA-256，而不是把 URL 直接拼进文件名：自定义地址
    /// 可能包含路径分隔符、查询串或超长片段，直接拼接会越出缓存目录或触发平台
    /// 文件名限制。官方地址保留 `official.json` 是为了读取升级前的签名缓存。
    fn new(configured: &str) -> AppResult<Self> {
        let configured = configured.trim();
        if configured.is_empty() {
            return Ok(Self::official());
        }
        if configured.len() > MAX_SOURCE_URL {
            return Err(AppError::invalid("插件目录地址过长"));
        }
        let url = kerminal_plugin_protocol::validate_asset_url(configured)
            .map_err(|_| AppError::invalid("插件目录地址无效"))?
            .to_string();
        let cache_file = if url == DEFAULT_PLUGIN_CATALOG_URL {
            OFFICIAL_CACHE_FILE.to_owned()
        } else {
            format!("catalog-{}.json", sha256_hex(url.as_bytes()))
        };
        Ok(Self { url, cache_file })
    }
}

/// 已加载目录的身份锚点，供后续安装冻结用户实际看到的那份原文。
///
/// 身份类型定义在应用层，基础设施只负责从原始 JSON 计算摘要并构造它；这样界面与
/// 安装用例都不必依赖本模块，跨层比较也不会出现两套字段各自漂移。
///
/// 摘要只基于加载时的原始 JSON 字节，而不是重新序列化的结构体：空白、字段顺序和
/// 未知但合法的排版都属于用户看到的内容，重新序列化会抹平这些差异，无法证明安装
/// 选择仍指向同一份目录。`source_url` 同时绑定当前实际来源，防止两个来源恰好提供
/// 相同原文时，旧确认被拿到另一来源继续安装。
fn identity_from_raw(source_url: &str, raw: &str, catalog: &PluginCatalog) -> CatalogIdentity {
    CatalogIdentity::new(
        catalog.source_id.clone(),
        catalog.sequence,
        sha256_hex(raw.as_bytes()),
    )
    .with_source_url(source_url)
}

/// 目录快照和身份锚点一起返回；只返回快照会让调用方丢失“用户看到的是哪一份原文”。
///
/// 身份只保存在 `snapshot.identity` 一处，`identity()` 是只读投影，避免两个字段在
/// 后续修改中失去同步。快照本身已经携带身份，安装选择直接克隆它即可。
#[derive(Clone, Debug)]
pub struct VerifiedCatalog {
    pub snapshot: CatalogSnapshot,
}

impl VerifiedCatalog {
    /// 身份锚点从快照投影，调用方不能单独替换身份而让快照仍指向旧原文。
    pub fn identity(&self) -> &CatalogIdentity {
        &self.snapshot.identity
    }
}

/// 原文与抓取时间存入同一原子文件；旧签名字段只保留读取兼容。
///
/// 新写入不再产生签名，因此 `signature` 使用 `Option` 并在序列化时省略；升级前
/// 的缓存仍带签名字段，反序列化后直接忽略，不需要为旧格式维护第二套读取分支。
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cache {
    raw: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    fetched_at: u64,
}

/// 目录端口：来源可在运行期切换，但每次操作都使用开始时冻结的来源快照。
pub struct CatalogClient {
    storage: StorageCoordinator,
    /// 克隆共享同一份来源状态；切换只替换下一份快照，不打断在途操作。
    source: Arc<RwLock<SourceState>>,
    /// HTTP 客户端首次抓取时才建立；克隆共享同一实例，连接池才能跨调用复用。
    http: Arc<OnceLock<Client>>,
}

impl Clone for CatalogClient {
    /// 克隆必须共享来源状态，否则设置页切换来源后安装器仍读旧地址。
    ///
    /// HTTP 客户端也必须共享：每次克隆各建一份连接池会让刷新、README 和目录
    /// 读取之间的空闲连接互不可见，等于退回“每次请求重新握手”的开销。
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            source: Arc::clone(&self.source),
            http: Arc::clone(&self.http),
        }
    }
}

impl CatalogClient {
    /// 官方默认来源；默认地址是编译期常量，不需要再解析验证。
    pub fn official(storage: StorageCoordinator) -> Self {
        Self {
            storage,
            source: Arc::new(RwLock::new(SourceState::official())),
            http: Arc::new(OnceLock::new()),
        }
    }

    /// 按配置装配目录端口；空配置表示官方默认，自定义地址立即验证。
    ///
    /// 验证失败在构造期返回，避免把无效地址带进运行态后才在刷新时暴露。
    pub fn new(storage: StorageCoordinator, configured: &str) -> AppResult<Self> {
        let source = SourceState::new(configured)?;
        Ok(Self {
            storage,
            source: Arc::new(RwLock::new(source)),
            http: Arc::new(OnceLock::new()),
        })
    }

    /// 组合根入口：验收构建允许用环境变量覆盖目录地址，正式构建只读设置。
    ///
    /// 验收覆盖仍要求显式隔离配置根，避免测试地址写入真实用户目录；不再读取公钥
    /// 环境变量，目录只按 JSON 加载，自定义 HTTP 来源与官方来源走同一条路径。
    pub fn for_runtime(storage: StorageCoordinator, configured: &str) -> AppResult<Self> {
        #[cfg(feature = "acceptance")]
        {
            if let Ok(url) = std::env::var("KERMINAL_PLUGIN_CATALOG_URL") {
                if std::env::var_os("KERMINAL_CONFIG_ROOT").is_none() {
                    return Err(AppError::invalid("插件验收目录不安全"));
                }
                if url.trim().is_empty() {
                    return Err(AppError::invalid("插件验收目录地址为空"));
                }
                return Self::new(storage, &url);
            }
        }
        Self::new(storage, configured)
    }

    /// 读取当前来源快照；锁中毒说明运行态已不可信，不能继续用旧地址发请求。
    fn source_snapshot(&self) -> AppResult<SourceState> {
        self.source
            .read()
            .map(|source| source.clone())
            .map_err(|_| AppError::infrastructure("插件目录来源锁不可用"))
    }

    /// 比较当前来源是否仍是操作开始时冻结的那一份。
    fn source_unchanged(&self, expected: &SourceState) -> AppResult<bool> {
        Ok(self.source_snapshot()? == *expected)
    }

    /// 延迟创建并复用唯一的 HTTP 客户端。
    ///
    /// 客户端配置是静态的（连接超时、整请求超时、逐跳重定向校验），与目录来源
    /// 无关；若每次抓取都新建 `Client`，连接池随实例一起丢弃，连续刷新或读取
    /// README 时每次都要重新 TCP/TLS 握手。这里把构建推迟到第一次真正发请求，
    /// 既不让官方默认装配在启动期依赖网络栈，也让构建失败只在需要网络时暴露。
    ///
    /// 使用 `OnceLock` 而不是构造期字段：`official()` 无法返回错误，构造期失败会
    /// 逼上层改签名；延迟到 `fetch` 后仍按原有 `AppError` 路径上报。构建失败不缓存，
    /// 下一次请求会重试；并发首次调用保留先写入的实例，避免连接池分裂。
    fn http_client(&self) -> AppResult<Client> {
        if let Some(client) = self.http.get() {
            return Ok(client.clone());
        }
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .redirect(redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 3
                    || kerminal_plugin_protocol::validate_asset_url(attempt.url().as_str()).is_err()
                {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|e| AppError::infrastructure_with_source("创建插件网络客户端失败", e))?;
        Ok(self.http.get_or_init(|| client).clone())
    }

    /// 重定向逐跳核验，限制每个响应的大小并在等待时响应取消。
    ///
    /// 目录、README 和资产共用 `validate_asset_url` 语法契约：允许 HTTP(S)、
    /// 局域网、回环和非默认端口，拒绝 userinfo、fragment、歧义原文与 0 端口。
    /// 这里不再维护 GitHub 主机白名单，自定义来源与官方来源行为一致。
    ///
    /// 请求前的 URL 校验、客户端构建、取消与发送的先后顺序保持不变：无效地址
    /// 仍先于网络错误返回，客户端构建失败也仍在发起请求之前暴露。
    async fn fetch(
        &self,
        url: &str,
        maximum: usize,
        cancellation: &CancellationToken,
    ) -> AppResult<String> {
        if kerminal_plugin_protocol::validate_asset_url(url).is_err() {
            return Err(AppError::invalid("插件目录地址无效"));
        }
        let client = self.http_client()?;
        let mut response = tokio::select! {
            _ = cancellation.cancelled() => return Err(AppError::cancelled("插件目录刷新已取消")),
            result = client.get(url).send() => result.map_err(|e| AppError::infrastructure_with_source("获取插件目录失败", e))?,
        };
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|n| n > maximum as u64)
        {
            return Err(AppError::invalid("插件目录响应无效"));
        }
        let mut bytes = Vec::new();
        loop {
            let chunk = tokio::select! {
                _ = cancellation.cancelled() => return Err(AppError::cancelled("插件目录刷新已取消")),
                result = response.chunk() => result.map_err(|e| AppError::infrastructure_with_source("读取插件目录失败", e))?,
            };
            let Some(chunk) = chunk else {
                break;
            };
            if chunk.len() > maximum.saturating_sub(bytes.len()) {
                return Err(AppError::invalid("插件目录超过限额"));
            }
            bytes.extend_from_slice(&chunk);
        }
        String::from_utf8(bytes).map_err(|_| AppError::invalid("插件目录编码无效"))
    }

    /// 在共享存储锁内复核旧缓存并原子提交；提交前关闭暂存写句柄，避免 Windows
    /// 替换被自身句柄阻塞。未知提交保留暂存字节供回读核对，只清理确定未提交的失败。
    ///
    /// 旧缓存只做可读性校验，不再比较序列：`sequence` 是身份元数据，不要求手工
    /// 递增，同一序列的新内容也必须能刷新。但旧缓存损坏时必须返回错误而不是覆盖，
    /// 否则用户原始证据会被一次刷新静默抹掉。
    ///
    /// 同步磁盘提交阶段无法响应取消：进入本函数后提交可能已经完成，此时即使调用方
    /// 随后取消，也必须如实返回成功的新缓存，绝不能声称“未提交”或丢弃已写入结果。
    fn persist(&self, source: &SourceState, cache: &Cache) -> AppResult<VerifiedCatalog> {
        let incoming = parse_catalog(&cache.raw)?;
        let _guard = self
            .storage
            .lock()
            .map_err(|e| AppError::infrastructure_with_source("插件目录存储锁不可用", e))?;
        let directory = cache_directory(self.storage.root(), true)?;
        let path = directory.join(&source.cache_file);
        if let Some(previous) = self.read_cache_at(&path)? {
            // 旧缓存必须仍可解析；损坏时保留原文件并上抛，让用户能看见真实故障。
            parse_catalog(&previous.raw)?;
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&directory)
            .map_err(|e| AppError::infrastructure_with_source("创建目录缓存失败", e))?;
        serde_json::to_writer(&mut temporary, cache)
            .map_err(|e| AppError::infrastructure_with_source("编码目录缓存失败", e))?;
        temporary
            .flush()
            .and_then(|_| temporary.as_file().sync_all())
            .map_err(|e| AppError::infrastructure_with_source("同步目录缓存失败", e))?;
        let temporary = temporary
            .into_temp_path()
            .keep()
            .map_err(|e| AppError::infrastructure_with_source("准备目录缓存提交失败", e))?;
        let mode = if path.exists() {
            CommitMode::ReplaceExisting
        } else {
            CommitMode::CreateNew
        };
        commit_staged_file(&temporary, &path, mode).map_err(|e| match e {
            AtomicCommitError::NotCommitted(_) => {
                if let Err(cleanup) = fs::remove_file(&temporary)
                    && cleanup.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(
                        event = "catalog_staging_cleanup_failed",
                        error_kind = ?cleanup.kind(),
                        raw_os_error = cleanup.raw_os_error(),
                    );
                }
                AppError::infrastructure_with_source("目录缓存提交失败", e)
            }
            AtomicCommitError::CommitUnknown(_) => {
                AppError::outcome_unknown_with_source("目录缓存提交结果未知", e)
            }
        })?;
        Ok(VerifiedCatalog {
            snapshot: CatalogSnapshot {
                identity: identity_from_raw(&source.url, &cache.raw, &incoming),
                catalog: incoming,
                stale: false,
                fetched_at: cache.fetched_at,
            },
        })
    }

    /// 检查缓存是有界普通文件，避免跟随目录中的链接。
    fn read_cache_at(&self, path: &Path) -> AppResult<Option<Cache>> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(AppError::infrastructure_with_source("读取目录缓存失败", e)),
        };
        if !safe_metadata(&metadata)
            || !metadata.is_file()
            || metadata.len() > (MAX_CATALOG * 6 + MAX_SIGNATURE * 6 + 65536) as u64
        {
            return Err(AppError::invalid("插件目录缓存不安全"));
        }
        let mut bytes = Vec::new();
        fs::File::open(path)
            .and_then(|f| {
                f.take((MAX_CATALOG * 6 + MAX_SIGNATURE * 6 + 65537) as u64)
                    .read_to_end(&mut bytes)
            })
            .map_err(|e| AppError::infrastructure_with_source("读取目录缓存失败", e))?;
        let cache =
            serde_json::from_slice(&bytes).map_err(|_| AppError::invalid("插件目录缓存损坏"))?;
        Ok(Some(cache))
    }

    /// 同步读取指定来源的缓存；调用方已经冻结来源，不能在这里重新读共享状态。
    fn read_verified_sync(&self, source: &SourceState) -> AppResult<Option<VerifiedCatalog>> {
        let _guard = self
            .storage
            .lock()
            .map_err(|e| AppError::infrastructure_with_source("插件目录存储锁不可用", e))?;
        let directory = match cache_directory(self.storage.root(), false) {
            Ok(p) => p,
            Err(AppError::Infrastructure {
                source: Some(ref e),
                ..
            }) if e
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        self.read_cache_at(&directory.join(&source.cache_file))?
            .map(|cache| {
                parse_catalog(&cache.raw).map(|catalog| VerifiedCatalog {
                    snapshot: CatalogSnapshot {
                        identity: identity_from_raw(&source.url, &cache.raw, &catalog),
                        catalog,
                        stale: false,
                        fetched_at: cache.fetched_at,
                    },
                })
            })
            .transpose()
    }

    /// 缓存读取放到阻塞线程，避免大目录解析占用异步执行器。
    async fn read_verified(&self, source: &SourceState) -> AppResult<Option<VerifiedCatalog>> {
        let this = self.clone();
        let source = source.clone();
        tokio::task::spawn_blocking(move || this.read_verified_sync(&source))
            .await
            .map_err(|e| AppError::infrastructure_with_source("读取目录缓存任务失败", e))?
    }
}

/// persist 任务失联时保守上报未知结果，绝不当成普通失败进入缓存回退。
///
/// persist 在 `spawn_blocking` 中执行，join 失败只说明调用方拿不到结果：任务可能
/// 在原子提交完成后才 panic，join 被放弃时阻塞任务也可能仍在继续写盘。两种情况
/// 都无法证明提交未发生，若按普通 Infrastructure 错误进入 stale fallback，会把
/// “提交结果未知”伪装成确定失败并返回旧缓存，调用方可能据此重试或清理已提交的
/// 状态。OutcomeUnknown 保留 JoinError 作为源错误，要求外围编排先回读核对，
/// 符合共享存储层的未知提交契约；取消不改写这一分类，由编排层自行判定。
fn persist_result_lost(join: tokio::task::JoinError) -> AppError {
    AppError::outcome_unknown_with_source("目录缓存提交任务失联", join)
}

/// 目录结构校验与旧签名无关：只检查 schema、来源标识、条目上限和资产完整性。
///
/// 自定义来源只需要提供非空且有界的 `source_id`，不再与固定轨道比较；`sequence`
/// 仅作为身份元数据保留，不要求手工递增。资产和 README 地址统一走共享 URL 契约，
/// 允许局域网、回环与非默认端口，拒绝歧义原文、userinfo、fragment 和 0 端口。
fn parse_catalog(raw: &str) -> AppResult<PluginCatalog> {
    if raw.len() > MAX_CATALOG {
        return Err(AppError::invalid("插件目录超过限额"));
    }
    let catalog: PluginCatalog =
        serde_json::from_str(raw).map_err(|_| AppError::invalid("插件目录格式无效"))?;
    if catalog.schema != 1 || !valid_source_id(&catalog.source_id) || catalog.plugins.len() > 4096 {
        return Err(AppError::invalid("插件目录来源或版本无效"));
    }
    let mut ids = BTreeSet::new();
    for plugin in &catalog.plugins {
        if !ids.insert(plugin.id.clone())
            || plugin.versions.len() > 1024
            || plugin.publisher != plugin.id.as_str().split('.').next().unwrap_or_default()
        {
            return Err(AppError::invalid("插件目录条目重复或超限"));
        }
        if let Some(readme) = &plugin.readme_url
            && kerminal_plugin_protocol::validate_asset_url(readme).is_err()
        {
            return Err(AppError::invalid("插件目录资产无效"));
        }
        let mut versions = BTreeSet::new();
        for version in &plugin.versions {
            if !versions.insert(version.version.clone())
                || version.assets.is_empty()
                || version.assets.len() > 7
                || (version.api.len() > 256 || node_semver::Range::parse(&version.api).is_err())
            {
                return Err(AppError::invalid("插件目录版本重复或超限"));
            }
            let mut platforms = BTreeSet::new();
            if version.assets.len() > 1
                && version
                    .assets
                    .iter()
                    .any(|asset| asset.platform == kerminal_plugin_protocol::Platform::Any)
            {
                return Err(AppError::invalid("插件目录平台资产冲突"));
            }
            for asset in &version.assets {
                if !platforms.insert(asset.platform)
                    || kerminal_plugin_protocol::validate_asset_url(&asset.url).is_err()
                    || asset.size == 0
                    || asset.sha256.len() != 64
                    || !asset.sha256.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return Err(AppError::invalid("插件目录资产无效"));
                }
            }
        }
    }
    Ok(catalog)
}

/// 来源标识只做有界非空校验，允许自定义来源使用自己的命名。
///
/// 它不参与缓存文件名或路径拼接，因此不需要限制为 ASCII 标识符；只拒绝空白、
/// 控制字符和超长值，避免身份字段被用来隐藏内容或撑大缓存。
fn valid_source_id(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= MAX_SOURCE_ID && !value.chars().any(char::is_control)
}

/// 每级缓存目录拒绝链接和 Windows 重解析点，防止写入逃离配置根。
fn cache_directory(root: &Path, create: bool) -> AppResult<PathBuf> {
    let mut path = root.to_path_buf();
    for name in ["plugins", "catalog-cache"] {
        path.push(name);
        if create {
            match fs::create_dir(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(AppError::infrastructure_with_source(
                        "创建插件目录缓存失败",
                        e,
                    ));
                }
            }
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|e| AppError::infrastructure_with_source("读取插件目录缓存失败", e))?;
        if !metadata.is_dir() || !safe_metadata(&metadata) {
            return Err(AppError::invalid("插件目录缓存路径不安全"));
        }
    }
    Ok(path)
}

/// 目录身份摘要统一使用小写十六进制 SHA-256，后续安装冻结只比较这一种稳定编码。
///
/// 使用完全限定的 trait 调用，避免把 `Digest` 名称带进模块作用域；测试同时引入
/// blake2 的签名 trait，同名 trait 共存会让方法解析依赖实现细节。
fn sha256_hex(bytes: &[u8]) -> String {
    <sha2::Sha256 as sha2::Digest>::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Windows junction 同样是不受信的重解析点。
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

#[async_trait]
impl CatalogSource for CatalogClient {
    /// 返回当前实际来源；锁中毒时保守回显官方默认，避免界面拿到空地址。
    fn source_url(&self) -> String {
        self.source
            .read()
            .map(|source| source.url.clone())
            .unwrap_or_else(|_| DEFAULT_PLUGIN_CATALOG_URL.to_owned())
    }

    /// 先验证再原子替换；失败保留旧来源，避免界面把无效配置当成已应用。
    fn set_source_url(&self, source_url: &str) -> AppResult<()> {
        let next = SourceState::new(source_url)?;
        let mut current = self
            .source
            .write()
            .map_err(|_| AppError::infrastructure("插件目录来源锁不可用"))?;
        *current = next;
        Ok(())
    }

    /// 缓存损坏时返回真实错误，不静默把其变成可覆盖的新安装。
    async fn cached(&self) -> AppResult<Option<CatalogSnapshot>> {
        Ok(self
            .cached_verified()
            .await?
            .map(|verified| verified.snapshot))
    }

    /// README 地址由当前目录提供，读取正文仍限制重定向和大小。
    async fn readme(
        &self,
        id: &kerminal_domain::plugins::PluginId,
        cancellation: CancellationToken,
    ) -> AppResult<Option<String>> {
        // 先冻结来源再读缓存：切换来源后不能继续使用旧目录里的 README 地址。
        let source = self.source_snapshot()?;
        let snapshot = self.read_verified(&source).await;
        // 缓存读取期间来源可能已切换：旧来源的“无缓存”“无条目”“无 README”都只是
        // 旧来源的事实，不能冒充当前来源的结果；先核来源再查条目，也避免为旧来源的
        // README 地址发出无谓的网络请求。网络完成后的检查仍然保留，覆盖下载期间才
        // 发生的切换。
        if !self.source_unchanged(&source)? {
            return Err(AppError::conflict("插件目录来源已切换，请重新刷新"));
        }
        let snapshot = snapshot?.ok_or_else(|| AppError::not_found("插件目录尚未加载"))?;
        let plugin = snapshot
            .snapshot
            .catalog
            .plugins
            .iter()
            .find(|plugin| &plugin.id == id)
            .ok_or_else(|| AppError::not_found("插件目录条目不存在"))?;
        match &plugin.readme_url {
            Some(url) => {
                let body = self.fetch(url, 1024 * 1024, &cancellation).await?;
                // 下载期间来源被切换时，旧来源的 README 不能作为当前目录内容展示。
                if !self.source_unchanged(&source)? {
                    return Err(AppError::conflict("插件目录来源已切换，请重新刷新"));
                }
                Ok(Some(body))
            }
            None => Ok(None),
        }
    }

    /// 下载成功后原子替换；失败时返回当前来源的旧缓存。
    async fn refresh(&self, cancellation: CancellationToken) -> AppResult<CatalogSnapshot> {
        Ok(self.refresh_verified(cancellation).await?.snapshot)
    }
}

impl CatalogClient {
    /// 读取当前来源缓存并附带身份锚点；安装冻结必须同时核对来源、序列和原文摘要。
    ///
    /// 来源在进入阻塞读取前冻结，缓存文件名也来自同一份快照；切换来源不会让这次
    /// 读取误读另一来源的文件。
    pub async fn cached_verified(&self) -> AppResult<Option<VerifiedCatalog>> {
        let source = self.source_snapshot()?;
        let verified = self.read_verified(&source).await?;
        // 读取期间来源被切换时，旧来源快照不能冒充当前来源的缓存结果。
        if !self.source_unchanged(&source)? {
            return Err(AppError::conflict("插件目录来源已切换，请重新刷新"));
        }
        Ok(verified)
    }

    /// 按调用方冻结的身份锚点重新读取当前来源缓存，不一致时明确冲突。
    ///
    /// 安装绑定直接复用这一入口：用户确认后来源若已切换、缓存已刷新到另一份原文，
    /// 这里必须拒绝而不是按 id/version/platform 在最新目录里静默改选。先比较来源
    /// 再读取缓存，避免旧来源的确认读到新来源恰好同名的缓存文件。
    pub async fn cached_matching(&self, expected: &CatalogIdentity) -> AppResult<VerifiedCatalog> {
        let source = self.source_snapshot()?;
        if expected.source_url != source.url {
            return Err(AppError::conflict("插件目录来源已切换，请重新确认"));
        }
        let verified = self
            .read_verified(&source)
            .await?
            .ok_or_else(|| AppError::not_found("插件目录缓存不存在"))?;
        // 读取期间来源被切换时，旧来源的确认不能继续落到新来源。
        if !self.source_unchanged(&source)? {
            return Err(AppError::conflict("插件目录来源已切换，请重新确认"));
        }
        if verified.identity() != expected {
            return Err(AppError::conflict("插件目录已变化，请重新确认"));
        }
        Ok(verified)
    }

    /// 刷新并返回身份锚点；取消和未知提交原样上抛，其它失败才回退到同一来源缓存。
    ///
    /// 取消优先级由显式检查点定义：入口、fetch 内部、普通错误回退前、回退读取
    /// 完成后。网络普通错误与取消同时就绪时取消优先；读取缓存期间到达的取消同样
    /// 上抛，绝不返回 stale 成功。取消前缓存内容并没有被本次刷新改动，所以上抛取消
    /// 而不附带任何快照是安全的。
    ///
    /// 两个语义不得被取消改写：`persist` 成功返回的 Ok 原样返回（提交已发生，取消
    /// 不能否认它）；`OutcomeUnknown` 原样上抛（提交结果未知必须回读核对，比取消更
    /// 需要用户处理）。未知结果除 commit_staged_file 的边界外还包括 persist 任务
    /// 失联（panic 或 join 中断）：join 失败不能证明提交未发生，必须按 OutcomeUnknown
    /// 上报而不是进入缓存回退。这两个分支排在取消复核之前，不受后续检查影响。
    ///
    /// 来源切换与刷新并发时，旧来源的请求和提交只使用开始时冻结的快照；提交完成后
    /// 若当前来源已变化，不把旧快照返回给调用方，而是要求按新来源重新刷新。
    ///
    /// 残余窗口如实说明：最后一个检查点之后、返回 Ok(stale) 之前到达的取消无法再
    /// 拦截；此时返回的快照带 `stale = true`，从不冒充一次完成的刷新。
    pub async fn refresh_verified(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<VerifiedCatalog> {
        // 先检查再发请求：已取消的刷新连网络都不应启动，否则连接错误可能先于取消
        // 返回，进而被后面的缓存回退当成普通失败，掩盖用户已经撤销这次刷新的事实。
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件目录刷新已取消"));
        }
        let source = self.source_snapshot()?;
        let result = async {
            let raw = self.fetch(&source.url, MAX_CATALOG, &cancellation).await?;
            if cancellation.is_cancelled() {
                return Err(AppError::cancelled("插件目录刷新已取消"));
            }
            // 来源在下载期间切换时，旧内容不再有展示意义；在提交前拒绝，避免把
            // 旧来源的刷新结果写盘后又被界面当成新来源内容。
            if !self.source_unchanged(&source)? {
                return Err(AppError::conflict("插件目录来源已切换，请重新刷新"));
            }
            let fetched_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| AppError::infrastructure("系统时间无效"))?
                .as_secs();
            let this = self.clone();
            let persist_source = source.clone();
            tokio::task::spawn_blocking(move || {
                this.persist(
                    &persist_source,
                    &Cache {
                        raw,
                        signature: None,
                        fetched_at,
                    },
                )
            })
            // JoinError 只说明调用方拿不到结果，不代表提交未发生：任务可能在原子
            // 提交完成后才 panic，join 被放弃时阻塞任务也可能仍在继续写盘。
            .await
            .map_err(persist_result_lost)?
        }
        .await;
        let verified = self.settle_refresh(&source, &cancellation, result).await?;
        // 提交或回退期间来源已切换：旧快照不能冒充当前来源结果。
        if !self.source_unchanged(&source)? {
            return Err(AppError::conflict("插件目录来源已切换，请重新刷新"));
        }
        Ok(verified)
    }

    /// persist 结果的最终裁决：成功如实返回、取消与未知提交上抛、普通错误才回退。
    ///
    /// 从 `refresh_verified` 提取成可注入纯结果的独立函数，是为了让测试能确定性地
    /// 驱动这组分支：端到端只能让 persist 走正常路径，无法在“成功/未知提交已产生”
    /// 之后再去竞争取消，直接注入 `AppResult` 即可覆盖每个分支的组合语义。
    ///
    /// 三个不可回退分支的优先序是刻意安排的：`Ok` 排第一——persist 返回成功时提交
    /// 已发生，即使取消同时到达也不能否认它；`OutcomeUnknown` 与 `Cancelled` 排在
    /// 取消复核之前——未知提交必须回读核对，不能被后续任何取消检查改写成取消或
    /// stale 成功；普通错误才轮到回退，由回退自身的取消检查点决定 stale 还是取消。
    async fn settle_refresh(
        &self,
        source: &SourceState,
        cancellation: &CancellationToken,
        result: AppResult<VerifiedCatalog>,
    ) -> AppResult<VerifiedCatalog> {
        match result {
            // 提交阶段不响应取消：persist 已把缓存写盘并返回成功时，即使取消已到达，
            // 也必须如实返回新快照，不能把它当失败再走缓存回退或声称未提交。
            Ok(verified) => Ok(verified),
            // 取消与未知提交原样上抛；未知提交优先于取消——它比取消更需要用户回读
            // 核对，绝不能把未知结果当成取消，也不能让它落入缓存回退。
            Err(error @ (AppError::Cancelled { .. } | AppError::OutcomeUnknown { .. })) => {
                Err(error)
            }
            Err(error) => self.stale_fallback(source, cancellation, error).await,
        }
    }

    /// 普通错误后的同来源缓存回退；取消在读取前后都优先于 stale 成功与缓存读取错误。
    ///
    /// 独立成方法是为了给测试一个确定性入口：端到端竞态里“取消恰好落在错误返回与
    /// 回退读取之间”无法可靠注入，这里直接以已取消的 token 调用即可覆盖同一代码
    /// 路径。读取前后的取消检查都必须保留：前者挡住“错误与取消同时就绪”，后者
    /// 挡住“回退读取期间才到达的取消”；缓存读取本身失败时同样先复核取消再传播，
    /// 避免读取错误抢先掩盖用户已撤销这次刷新的事实。
    async fn stale_fallback(
        &self,
        source: &SourceState,
        cancellation: &CancellationToken,
        error: AppError,
    ) -> AppResult<VerifiedCatalog> {
        self.stale_fallback_with(cancellation, error, self.read_verified(source))
            .await
    }

    /// 带可注入读取 future 的回退裁决；`stale_fallback` 的可测内核。
    ///
    /// 读取以 future 参数注入而不是固定调用缓存读取，测试因此能把“取消在读取期间
    /// 到达 + 读取返回错误/成功”两个交叠事件确定地放进同一次裁决里：旧实现用
    /// `.await?` 直接传播读取错误时，注入 future 返回 Err 就会抢在第二次取消检查
    /// 之前变成最终结果，该测试随即失败。生产路径传入真实来源的缓存读取。
    async fn stale_fallback_with(
        &self,
        cancellation: &CancellationToken,
        error: AppError,
        read: impl std::future::Future<Output = AppResult<Option<VerifiedCatalog>>>,
    ) -> AppResult<VerifiedCatalog> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件目录刷新已取消"));
        }
        // 先拿 Result 而不是直接 `?`：缓存读取错误与取消在读取期间同时发生时，
        // 必须由取消检查决定返回什么，而不是让读取错误抢先变成最终结果。
        let cached = read.await;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件目录刷新已取消"));
        }
        match cached? {
            Some(mut verified) => {
                verified.snapshot.stale = true;
                Ok(verified)
            }
            None => Err(error),
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/plugin_catalog.rs"]
mod tests;
