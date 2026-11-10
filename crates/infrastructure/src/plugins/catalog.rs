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
const OFFICIAL_KEY: &str = "RWRHAzmVz9i4dybg8RQmxYXve/GtXm6e/+FUcDAHXrkFr/NTQrxXpSD9";
const MAX_CATALOG: usize = 4 * 1024 * 1024;
const MAX_SIGNATURE: usize = 16 * 1024;

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
}

impl CatalogClient {
    /// 官方 URL 和公钥由宿主编译固定，不能由目录内容修改。
    pub fn official(storage: StorageCoordinator) -> Self {
        Self {
            storage,
            url: OFFICIAL_URL.into(),
            key: OFFICIAL_KEY.into(),
            allow_loopback: false,
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
        })
    }

    /// 下载器继承同一验收边界，正式实例永远不会允许本地 HTTP。
    pub(super) fn allows_loopback(&self) -> bool {
        self.allow_loopback
    }
    /// 目录校验总是在 JSON 解析前验签；缓存也必须重新执行同样的校验。
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
        if catalog.schema != 1 || catalog.source_id != "official" || catalog.plugins.len() > 4096 {
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
                    if !platforms.insert(asset.platform)
                        || !allowed_url(&asset.url, self.allow_loopback)
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
    fn persist(&self, cache: &Cache) -> AppResult<PluginCatalog> {
        let incoming = self.verify(cache)?;
        let _guard = self
            .storage
            .lock()
            .map_err(|e| AppError::infrastructure_with_source("插件目录存储锁不可用", e))?;
        let directory = cache_directory(self.storage.root(), true)?;
        let path = directory.join("official.json");
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
        Ok(incoming)
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
            this.read_cache_at(&directory.join("official.json"))?
                .map(|cache| {
                    this.verify(&cache).map(|catalog| CatalogSnapshot {
                        catalog,
                        stale: false,
                        fetched_at: cache.fetched_at,
                    })
                })
                .transpose()
        })
        .await
        .map_err(|e| AppError::infrastructure_with_source("读取目录缓存任务失败", e))?
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
            let catalog = tokio::task::spawn_blocking(move || {
                this.persist(&Cache {
                    raw,
                    signature,
                    fetched_at,
                })
            })
            .await
            .map_err(|e| AppError::infrastructure_with_source("保存目录缓存任务失败", e))??;
            Ok(CatalogSnapshot {
                catalog,
                stale: false,
                fetched_at,
            })
        }
        .await;
        match result {
            Ok(snapshot) => Ok(snapshot),
            Err(error @ (AppError::Cancelled { .. } | AppError::OutcomeUnknown { .. })) => {
                Err(error)
            }
            Err(error) => match self.cached().await? {
                Some(mut snapshot) => {
                    snapshot.stale = true;
                    Ok(snapshot)
                }
                None => Err(error),
            },
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/plugin_catalog.rs"]
mod tests;
