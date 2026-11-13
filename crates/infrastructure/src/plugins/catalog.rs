//! 固定来源的签名目录、单调序列及原子缓存。
//! @author kongweiguang

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    extensions::{CatalogSnapshot, CatalogSource, PluginCatalog},
};
use minisign_verify::{PublicKey, Signature};
use reqwest::{Client, Url, redirect};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

use crate::storage::{AtomicCommitError, CommitMode, StorageCoordinator, commit_staged_file};

const OFFICIAL_URL: &str =
    "https://github.com/kongweiguang/Kerminal-Plugins/releases/download/catalog-v1/catalog.json";
const OFFICIAL_V2_URL: &str =
    "https://github.com/kongweiguang/Kerminal-Plugins/releases/download/catalog-v2/catalog.json";
const OFFICIAL_KEY: &str = "RWRHAzmVz9i4dybg8RQmxYXve/GtXm6e/+FUcDAHXrkFr/NTQrxXpSD9";
const MAX_CATALOG: usize = 4 * 1024 * 1024;
const MAX_SIGNATURE: usize = 16 * 1024;

/// 目录轨道把固定地址、验签来源和缓存文件名绑定成一个不可拆分的身份。
///
/// 旧宿主会逐条校验目录资产，只有仍满足旧 URL 规则的兼容目录才能让它继续工作；
/// 新目录放宽资产来源后必须独立签名、独立序列、独立缓存，否则两轨会互相覆盖序列，
/// 让跨轨重放或回退绕过单调性。轨道只描述信任与存储位置，不代表 UI 已切到新轨。
///
/// 变体名共用 `Official` 前缀是为了和固定 Release 标签 `catalog-v1`/`catalog-v2`
/// 与来源标识 `official`/`official-v2` 一一对应；后续接线不应为命名美观改动，
/// 否则很容易把轨道身份、缓存文件和签名来源记错。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogTrack {
    /// 兼容轨：现有生产入口继续使用，来源标识保持 `official`，缓存文件保持原名。
    OfficialV1,
    /// 新轨：来源标识为 `official-v2`，与兼容轨共用固定公钥但独立缓存与序列。
    OfficialV2,
}

impl CatalogTrack {
    /// 来源标识同时写进签名原文和缓存文件名，验签后必须与本轨道严格一致，
    /// 防止一份合法目录被复制到另一轨后仍被接受。
    pub const fn source_id(self) -> &'static str {
        match self {
            Self::OfficialV1 => "official",
            Self::OfficialV2 => "official-v2",
        }
    }

    /// 缓存文件名按轨道固定，保证两轨序列比较只在各自文件内进行。
    pub const fn cache_file(self) -> &'static str {
        match self {
            Self::OfficialV1 => "official.json",
            Self::OfficialV2 => "official-v2.json",
        }
    }

    /// 地址和标签由宿主编译固定，调用方不能传入任意 URL 改变信任来源。
    pub const fn catalog_url(self) -> &'static str {
        match self {
            Self::OfficialV1 => OFFICIAL_URL,
            Self::OfficialV2 => OFFICIAL_V2_URL,
        }
    }
}

/// 已验签目录的身份锚点，供后续安装冻结用户实际看到的那份签名快照。
///
/// 摘要只基于验签通过的原始 JSON 字节，而不是重新序列化的结构体：空白、字段顺序和
/// 未知但合法的排版都属于用户看到的内容，重新序列化会抹平这些差异，无法证明安装
/// 选择仍指向同一份目录。后续集成必须同时比较来源、序列和摘要，不能只按
/// id/version/platform 在刷新后的目录里重新选源。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogCacheIdentity {
    pub source_id: String,
    pub sequence: u64,
    pub raw_sha256: String,
}

impl CatalogCacheIdentity {
    /// 从同一次验签结果生成锚点，避免调用方分别读取原文和结构体时出现错配。
    fn from_verified(cache: &Cache, catalog: &PluginCatalog) -> Self {
        Self {
            source_id: catalog.source_id.clone(),
            sequence: catalog.sequence,
            raw_sha256: sha256_hex(cache.raw.as_bytes()),
        }
    }
}

/// 验签快照和身份锚点一起返回；只返回快照会让调用方丢失“用户看到的是哪一份原文”。
#[derive(Clone, Debug)]
pub struct VerifiedCatalog {
    pub snapshot: CatalogSnapshot,
    pub identity: CatalogCacheIdentity,
}

/// 签名与原文存入同一原子文件，避免崩溃后形成不配对的缓存。
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cache {
    raw: String,
    signature: String,
    fetched_at: u64,
}

/// 正式构建只接受固定信任根；测试覆盖仅在 acceptance feature 中存在。
#[derive(Clone)]
pub struct CatalogClient {
    storage: StorageCoordinator,
    url: String,
    key: String,
    allow_loopback: bool,
    /// 轨道决定验签后允许的来源标识和缓存文件名，不能由调用方在运行期改写。
    track: CatalogTrack,
}

impl CatalogClient {
    /// 官方 URL 和公钥由宿主编译固定，不能由目录内容修改。
    pub fn official(storage: StorageCoordinator) -> Self {
        Self::for_track(storage, CatalogTrack::OfficialV1)
    }

    /// 按轨道装配固定来源、固定公钥和独立缓存；这是后续组合根的接入口。
    ///
    /// 当前生产入口仍只调用 `official()`。在用户所见签名快照与安装选择完成绑定之前，
    /// 不能把 UI 或安装静默切到新轨，否则刷新竞态会让安装落到用户未确认的另一份目录。
    pub fn for_track(storage: StorageCoordinator, track: CatalogTrack) -> Self {
        Self {
            storage,
            url: track.catalog_url().into(),
            key: OFFICIAL_KEY.into(),
            allow_loopback: false,
            track,
        }
    }

    /// 验收覆盖必须同时提供 URL、公钥和显式隔离配置根；正式构建始终使用官方源。
    pub fn for_runtime(storage: StorageCoordinator) -> AppResult<Self> {
        #[cfg(feature = "acceptance")]
        {
            let url = std::env::var("KERMINAL_PLUGIN_CATALOG_URL").ok();
            let key = std::env::var("KERMINAL_PLUGIN_CATALOG_PUBKEY").ok();
            match (url, key) {
                (Some(url), Some(key)) => return Self::acceptance(storage, url, key),
                (None, None) => {}
                _ => return Err(AppError::invalid("插件验收目录配置不完整")),
            }
        }
        Ok(Self::official(storage))
    }
    /// 仅隔离配置根的验收构建允许本地服务和测试公钥，正式版本不包含覆盖入口。
    ///
    /// 验收目录沿用兼容轨的来源标识与缓存文件，避免测试环境意外写入生产新轨缓存。
    #[cfg(feature = "acceptance")]
    pub fn acceptance(storage: StorageCoordinator, url: String, key: String) -> AppResult<Self> {
        if std::env::var_os("KERMINAL_CONFIG_ROOT").is_none() || !allowed_url(&url, true) {
            return Err(AppError::invalid("插件验收目录不安全"));
        }
        Ok(Self {
            storage,
            url,
            key,
            allow_loopback: true,
            track: CatalogTrack::OfficialV1,
        })
    }

    /// 目录与 README fetch 的回环例外开关；不是资产下载的本地源许可。
    ///
    /// 资产传输边界由两层共同决定：签名目录已授权资产地址（V2 契约允许
    /// LAN/回环），`AssetNetworkSession` 再按 DNS 分类与重定向策略复核——初始
    /// 非公网目标本身允许连接，只有“公网跳非公网”的重定向被拒。所以正式实例
    /// 下载签名目录授权的回环/局域网资产是设计内行为，与本开关无关。
    pub(super) fn allows_loopback(&self) -> bool {
        self.allow_loopback
    }

    /// 轨道身份是只读的，避免验签通过后又被替换来源标识。
    pub fn track(&self) -> CatalogTrack {
        self.track
    }

    /// 验签目录内资产地址的语法判定，按轨道区分信任级别。
    ///
    /// 兼容轨继续使用既有 GitHub 白名单（验收构建附带回环例外），保证旧宿主
    /// 逐条校验资产的行为完全不变；新轨改用共享契约 `validate_asset_url`，
    /// 它与安装器 `AssetNetworkSession` 共用同一入口，允许合法 HTTP(S) 的
    /// 公网/LAN/回环、IDN 与非默认端口，拒绝歧义原文、userinfo、fragment 与
    /// 0 端口。目录来源与 README 的 fetch 白名单不在此路径上，不受影响。
    fn asset_url_allowed(&self, value: &str) -> bool {
        match self.track {
            CatalogTrack::OfficialV1 => allowed_url(value, self.allow_loopback),
            CatalogTrack::OfficialV2 => kerminal_plugin_protocol::validate_asset_url(value).is_ok(),
        }
    }
    /// 目录校验总是在 JSON 解析前验签；缓存也必须重新执行同样的校验。
    ///
    /// 验签只证明原文来自持有私钥的发布方，不能证明它属于当前轨道。因此解析后必须把
    /// `source_id` 与轨道固定值严格比较，否则同一份签名目录可被复制到另一轨重放。
    fn verify(&self, cache: &Cache) -> AppResult<PluginCatalog> {
        if cache.raw.len() > MAX_CATALOG || cache.signature.len() > MAX_SIGNATURE {
            return Err(AppError::invalid("插件目录超过限额"));
        }
        let key = if self.key.contains('\n') {
            PublicKey::decode(&self.key)
        } else {
            PublicKey::from_base64(&self.key)
        }
        .map_err(|_| AppError::invalid("插件目录公钥无效"))?;
        let signature = Signature::decode(&cache.signature)
            .map_err(|_| AppError::invalid("插件目录签名无效"))?;
        key.verify(cache.raw.as_bytes(), &signature, false)
            .map_err(|_| AppError::invalid("插件目录签名无效"))?;
        let catalog: PluginCatalog =
            serde_json::from_str(&cache.raw).map_err(|_| AppError::invalid("插件目录格式无效"))?;
        if catalog.schema != 1
            || catalog.source_id != self.track.source_id()
            || catalog.plugins.len() > 4096
        {
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
                    // 资产地址语法按轨道区分：兼容轨保持既有 GitHub 白名单与验收
                    // 回环行为完全不变；新轨改用与安装器网络模块共用的
                    // `validate_asset_url` 契约，允许合法 HTTP(S) 公网/LAN/回环、
                    // IDN 与非默认端口，拒绝歧义原文、userinfo、fragment 和 0 端口。
                    // 这里只扩大签名目录内 asset.url 的语法校验，目录与 README 的
                    // fetch 来源仍走 `allowed_fetch_url` 的既有白名单。
                    if !platforms.insert(asset.platform)
                        || !self.asset_url_allowed(&asset.url)
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

    /// 重定向逐跳核验，限制每个响应的大小并在等待时响应取消。
    async fn fetch(
        &self,
        url: &str,
        maximum: usize,
        document: bool,
        cancellation: &CancellationToken,
    ) -> AppResult<String> {
        if !allowed_fetch_url(url, self.allow_loopback, document) {
            return Err(AppError::invalid("插件下载地址不受信任"));
        }
        let allow = self.allow_loopback;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .redirect(redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= 3
                    || !allowed_fetch_url(attempt.url().as_str(), allow, document)
                {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|e| AppError::infrastructure_with_source("创建插件网络客户端失败", e))?;
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

    /// 在共享存储锁内复核序列；提交前关闭暂存写句柄，避免 Windows 替换被自身句柄阻塞。
    /// 未知提交保留暂存字节供回读核对，只清理确定未提交的失败，不自动重放写入。
    ///
    /// 回退判定只读取本轨道的缓存文件：两轨序列彼此独立，拿另一轨的序列比较会让
    /// 新轨一次正常发布被旧轨的更大序列误判成回退，反之也会掩盖真实回退。
    ///
    /// 同步磁盘提交阶段无法响应取消：进入本函数后提交可能已经完成，此时即使调用方
    /// 随后取消，也必须如实返回成功的新缓存，绝不能声称“未提交”或丢弃已写入结果。
    fn persist(&self, cache: &Cache) -> AppResult<VerifiedCatalog> {
        let incoming = self.verify(cache)?;
        let _guard = self
            .storage
            .lock()
            .map_err(|e| AppError::infrastructure_with_source("插件目录存储锁不可用", e))?;
        let directory = cache_directory(self.storage.root(), true)?;
        let path = directory.join(self.track.cache_file());
        if let Some(previous) = self.read_cache_at(&path)? {
            let old = self.verify(&previous)?;
            if incoming.sequence < old.sequence
                || (incoming.sequence == old.sequence && cache.raw != previous.raw)
            {
                return Err(AppError::conflict("插件目录序列回退或重复"));
            }
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
            identity: CatalogCacheIdentity::from_verified(cache, &incoming),
            snapshot: CatalogSnapshot {
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

/// 拒绝 URL 用户信息、非标准端口和相似域名，验收仅开放本机 HTTP。
pub(crate) fn allowed_url(value: &str, allow_loopback: bool) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return false;
    }
    if allow_loopback
        && url.scheme() == "http"
        && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
    {
        return true;
    }
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && matches!(
            url.host_str(),
            Some(
                "github.com"
                    | "objects.githubusercontent.com"
                    | "release-assets.githubusercontent.com"
            )
        )
}

/// README 可从官方仓库的 Raw 镜像读取；此例外不会扩大包资产下载白名单。
fn allowed_fetch_url(value: &str, allow_loopback: bool, document: bool) -> bool {
    if allowed_url(value, allow_loopback) {
        return true;
    }
    if !document {
        return false;
    }
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str() == Some("raw.githubusercontent.com")
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
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
    /// 缓存损坏时返回真实错误，不静默把其变成可覆盖的新安装。
    async fn cached(&self) -> AppResult<Option<CatalogSnapshot>> {
        Ok(self
            .cached_verified()
            .await?
            .map(|verified| verified.snapshot))
    }

    /// README 地址由受信目录提供，读取正文仍限制重定向和大小。
    async fn readme(
        &self,
        id: &kerminal_domain::plugins::PluginId,
        cancellation: CancellationToken,
    ) -> AppResult<Option<String>> {
        let snapshot = self
            .cached()
            .await?
            .ok_or_else(|| AppError::not_found("插件目录尚未加载"))?;
        let plugin = snapshot
            .catalog
            .plugins
            .iter()
            .find(|plugin| &plugin.id == id)
            .ok_or_else(|| AppError::not_found("插件目录条目不存在"))?;
        match &plugin.readme_url {
            Some(url) => self
                .fetch(url, 1024 * 1024, true, &cancellation)
                .await
                .map(Some),
            None => Ok(None),
        }
    }
    /// 下载和验签成功后原子替换；失败时返回经过重新验签的旧缓存。
    async fn refresh(&self, cancellation: CancellationToken) -> AppResult<CatalogSnapshot> {
        Ok(self.refresh_verified(cancellation).await?.snapshot)
    }
}

impl CatalogClient {
    /// 读取缓存并附带身份锚点；安装冻结后续必须同时核对来源、序列和原始字节摘要。
    ///
    /// 摘要基于验签通过的 raw 字节而不是重新序列化的结构体，因为用户看到的目录排版、
    /// 字段顺序都属于同一份签名原文，重新序列化会把不同原文折叠成同一摘要。
    pub async fn cached_verified(&self) -> AppResult<Option<VerifiedCatalog>> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = this
                .storage
                .lock()
                .map_err(|e| AppError::infrastructure_with_source("插件目录存储锁不可用", e))?;
            let directory = match cache_directory(this.storage.root(), false) {
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
            this.read_cache_at(&directory.join(this.track.cache_file()))?
                .map(|cache| {
                    this.verify(&cache).map(|catalog| VerifiedCatalog {
                        identity: CatalogCacheIdentity::from_verified(&cache, &catalog),
                        snapshot: CatalogSnapshot {
                            catalog,
                            stale: false,
                            fetched_at: cache.fetched_at,
                        },
                    })
                })
                .transpose()
        })
        .await
        .map_err(|e| AppError::infrastructure_with_source("读取目录缓存任务失败", e))?
    }

    /// 按调用方冻结的身份锚点重新读取本轨缓存，不一致时明确冲突。
    ///
    /// 后续安装绑定直接复用这一入口：用户确认后缓存若已刷新到另一序列或另一份原文，
    /// 这里必须拒绝而不是按 id/version/platform 在最新目录里静默改选。比较的是
    /// 已验签原文摘要，因此同序列不同排版也会被视为不同快照。
    pub async fn cached_matching(
        &self,
        expected: &CatalogCacheIdentity,
    ) -> AppResult<VerifiedCatalog> {
        let verified = self
            .cached_verified()
            .await?
            .ok_or_else(|| AppError::not_found("插件目录缓存不存在"))?;
        if &verified.identity != expected {
            return Err(AppError::conflict("插件目录已变化，请重新确认"));
        }
        Ok(verified)
    }

    /// 刷新并返回身份锚点；取消和未知提交原样上抛，其它失败才回退到本轨已验签缓存。
    ///
    /// 取消优先级由显式检查点定义：入口、两次 fetch 内部、普通错误回退前、回退读取
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
    /// 残余窗口如实说明：最后一个检查点之后、返回 Ok(stale) 之前到达的取消无法再
    /// 拦截；此时返回的快照带 `stale = true`，从不冒充一次完成的刷新。
    ///
    /// 回退必须用本轨缓存，不能拿另一轨内容兜底：跨轨内容会让调用方在未察觉的情况下
    /// 看到不同来源的目录，安装冻结也就失去意义。设计文档中“新目录失败回退到兼容
    /// 目录”属于未来上层组合源的行为——由它显式切轨并保留各自真实 source_id；
    /// 本层不读取、不合并另一轨缓存，该回退仍未实现。
    ///
    /// 取消可能在 `persist` 已开始后才到达：同步提交无法中断，取消前完成的提交会以
    /// 成功结果返回，不会虚报为未提交或已取消。
    pub async fn refresh_verified(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<VerifiedCatalog> {
        // 先检查再发请求：已取消的刷新连网络都不应启动，否则连接错误可能先于取消
        // 返回，进而被后面的缓存回退当成普通失败，掩盖用户已经撤销这次刷新的事实。
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件目录刷新已取消"));
        }
        let result = async {
            let raw = self
                .fetch(&self.url, MAX_CATALOG, false, &cancellation)
                .await?;
            let signature = self
                .fetch(
                    &format!("{}.minisig", self.url),
                    MAX_SIGNATURE,
                    false,
                    &cancellation,
                )
                .await?;
            if cancellation.is_cancelled() {
                return Err(AppError::cancelled("插件目录刷新已取消"));
            }
            let fetched_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| AppError::infrastructure("系统时间无效"))?
                .as_secs();
            let this = self.clone();
            tokio::task::spawn_blocking(move || {
                this.persist(&Cache {
                    raw,
                    signature,
                    fetched_at,
                })
            })
            // JoinError 只说明调用方拿不到结果，不代表提交未发生：任务可能在原子
            // 提交完成后才 panic，join 被放弃时阻塞任务也可能仍在继续写盘。
            .await
            .map_err(persist_result_lost)?
        }
        .await;
        self.settle_refresh(&cancellation, result).await
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
            Err(error) => self.stale_fallback(cancellation, error).await,
        }
    }

    /// 普通错误后的本轨缓存回退；取消在读取前后都优先于 stale 成功与缓存读取错误。
    ///
    /// 独立成方法是为了给测试一个确定性入口：端到端竞态里“取消恰好落在错误返回与
    /// 回退读取之间”无法可靠注入，这里直接以已取消的 token 调用即可覆盖同一代码
    /// 路径。读取前后的取消检查都必须保留：前者挡住“错误与取消同时就绪”，后者
    /// 挡住“回退读取期间才到达的取消”；缓存读取本身失败时同样先复核取消再传播，
    /// 避免读取错误抢先掩盖用户已撤销这次刷新的事实。
    async fn stale_fallback(
        &self,
        cancellation: &CancellationToken,
        error: AppError,
    ) -> AppResult<VerifiedCatalog> {
        self.stale_fallback_with(cancellation, error, self.cached_verified())
            .await
    }

    /// 带可注入读取 future 的回退裁决；`stale_fallback` 的可测内核。
    ///
    /// 读取以 future 参数注入而不是固定调用 `cached_verified`，测试因此能把“取消
    /// 在读取期间到达 + 读取返回错误/成功”两个交叠事件确定地放进同一次裁决里：
    /// 旧实现用 `.await?` 直接传播读取错误时，注入 future 返回 Err 就会抢在第二次
    /// 取消检查之前变成最终结果，该测试随即失败。生产路径传入真实的本轨缓存读取。
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
