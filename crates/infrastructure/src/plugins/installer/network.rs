//! 插件资产下载的安全网络边界。
//!
//! 本模块只处理已签名资产地址：允许任意 HTTP(S) 主机，但在请求发出前完成
//! URL 授权、DNS 全量分类和地址固定，并逐跳接管重定向。签名目录、大小与摘要、
//! 暂存提交由现有下载链路负责；本模块经 `download.rs` 的私有子模块接入安装器，
//! 只接收已通过签名目录校验的资产，不对外暴露任意 URL 下载能力。
//!
//! @author kongweiguang

use async_trait::async_trait;
use kerminal_application::AppError;
use kerminal_plugin_protocol::{
    AssetUrlRejection, validate_asset_url as shared_validate_asset_url,
    validate_redirect_target as shared_validate_redirect_target,
};
use reqwest::{
    Client, Response, Url,
    header::{HeaderMap, LOCATION},
    redirect,
};
use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    future::Future,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6},
    sync::Arc,
    time::Duration,
};
use tokio::time::Instant;
use tokio_util::{bytes::Bytes, sync::CancellationToken};
use url::Host;

/// 策略固定允许三次跟随；次数不能由调用方放大，避免改变已确认的资产授权边界。
const MAX_REDIRECTS: usize = 3;

/// 初始请求只有这些头会进入逐跳循环；`Host` 由 reqwest 按已授权 URL 重算，
/// 逐跳头、Referer 和代理凭据一律不发送，防止调用方把传输层细节带入重定向。
const SANITIZED_REQUEST_HEADERS: [&str; 9] = [
    "host",
    "referer",
    "proxy-authorization",
    "connection",
    "te",
    "transfer-encoding",
    "upgrade",
    "proxy-connection",
    "keep-alive",
];

/// 地址解析端口允许注入静态结果，测试可在不触发真实 DNS 的前提下覆盖混合解析。
///
/// 实现必须返回该主机名的全部 A/AAAA 结果；只返回首个地址会让安全判定漏掉
/// 公网与非公网混合的情况。
#[async_trait]
pub(super) trait AssetAddressResolver: Send + Sync {
    /// 返回主机名的全部解析结果；端口由调用方给出，便于同一 pin 在不同端口复用。
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
}

/// 生产解析器直接复用 tokio 的系统解析；取消只能停止等待方，底层阻塞线程
/// 仍可能继续运行到系统解析返回，因此错误与文档都不能声称线程已被终止。
pub(super) struct SystemAssetResolver;

#[async_trait]
impl AssetAddressResolver for SystemAssetResolver {
    /// 直接返回系统解析的全部地址；端口由调用方传入，不能只取首个结果，
    /// 否则混合 DNS 的公网/非公网组合会逃过安全判定。
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(tokio::net::lookup_host((host, port)).await?.collect())
    }
}

/// 网络策略中的共享依赖与连接超时；每个下载任务各自持有 pin 状态。
pub(super) struct NetworkPolicy {
    resolver: Arc<dyn AssetAddressResolver>,
    connect_timeout: Duration,
}

impl NetworkPolicy {
    /// 注入解析器让上层和测试都能复用同一套 URL/地址授权逻辑。
    pub(super) fn new(resolver: Arc<dyn AssetAddressResolver>) -> Self {
        Self {
            resolver,
            connect_timeout: Duration::from_secs(10),
        }
    }

    /// 生产默认使用系统解析；调用方仍可显式注入替换实现。
    pub(super) fn system() -> Self {
        Self::new(Arc::new(SystemAssetResolver))
    }
}

/// URL 语法拒绝原因；对外只展示固定文案，不带原始 URL、query 或凭据。
///
/// 变体与协议层共享契约一一对应，映射只保留传输层的错误语义，不重新实现判定。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UrlRejection {
    Malformed,
    UnsupportedScheme,
    MissingHost,
    UserInfo,
    Fragment,
    ZeroPort,
}

impl From<AssetUrlRejection> for UrlRejection {
    /// 共享契约是判定的唯一来源；这里只把原因翻译成网络模块的稳定错误分类，
    /// 保证安装器与未来市场侧对同一 URL 得出同一结论。
    fn from(value: AssetUrlRejection) -> Self {
        match value {
            AssetUrlRejection::Malformed => Self::Malformed,
            AssetUrlRejection::UnsupportedScheme => Self::UnsupportedScheme,
            AssetUrlRejection::MissingHost => Self::MissingHost,
            AssetUrlRejection::UserInfo => Self::UserInfo,
            AssetUrlRejection::Fragment => Self::Fragment,
            AssetUrlRejection::ZeroPort => Self::ZeroPort,
        }
    }
}

/// 地址策略拒绝原因；与传输错误分开，调用方不能把它当成可重试成功。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PolicyRejection {
    UnspecifiedAddress,
    MulticastAddress,
    BroadcastAddress,
    MixedDns,
    PublicToNonPublicRedirect,
    HttpsDowngrade,
}

/// 资产网络错误的稳定分类，供上层决定取消、超时、策略终止或连接重试。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AssetNetworkErrorKind {
    InvalidUrl,
    InvalidRedirect,
    Policy,
    RedirectLimit,
    Resolve,
    Transport,
    Timeout,
    Cancelled,
}

/// 传输错误保留 reqwest source 供内部诊断，但 Debug/Display 只输出固定文案，
/// 避免 reqwest 错误里的完整 URL（可能含 query 令牌）进入日志或界面。
pub(super) enum AssetNetworkError {
    InvalidUrl(UrlRejection),
    InvalidRedirect,
    PolicyDenied(PolicyRejection),
    RedirectLimit,
    Resolve {
        source: io::Error,
    },
    Transport {
        source: reqwest::Error,
        retryable: bool,
    },
    Timeout,
    Cancelled,
}

impl AssetNetworkError {
    /// 上层按稳定 kind 分支，不需要解析错误文案；策略拒绝永远不会报告为可重试。
    pub(super) fn kind(&self) -> AssetNetworkErrorKind {
        match self {
            Self::InvalidUrl(_) => AssetNetworkErrorKind::InvalidUrl,
            Self::InvalidRedirect => AssetNetworkErrorKind::InvalidRedirect,
            Self::PolicyDenied(_) => AssetNetworkErrorKind::Policy,
            Self::RedirectLimit => AssetNetworkErrorKind::RedirectLimit,
            Self::Resolve { .. } => AssetNetworkErrorKind::Resolve,
            Self::Transport { .. } => AssetNetworkErrorKind::Transport,
            Self::Timeout => AssetNetworkErrorKind::Timeout,
            Self::Cancelled => AssetNetworkErrorKind::Cancelled,
        }
    }

    /// 只有连接/请求阶段错误可重试；策略、重定向上限、取消和已耗尽的截止时间
    /// 都不应被调用方自动重放，否则会绕开一次明确的安全判定。连接阶段的
    /// `connect_timeout` 属于单次尝试预算，按可重试传输错误返回（见
    /// `map_reqwest_error`）；`AssetNetworkError::Timeout` 保留给绝对截止时间，
    /// 两者语义不同。DNS 解析失败（`Resolve`）刻意保持不可重试：pin 语义下
    /// 重试必须复用同一 session，重放解析会引入二次解析的 rebinding 风险。
    pub(super) fn is_retryable(&self) -> bool {
        match self {
            Self::Transport { retryable, .. } => *retryable,
            _ => false,
        }
    }

    /// 接入现有下载器时统一转成应用错误；动态原因只保留在 source 中。
    pub(super) fn into_app_error(self) -> AppError {
        match self {
            Self::InvalidUrl(_) => AppError::invalid("插件资产地址无效"),
            Self::InvalidRedirect => AppError::invalid("插件资产重定向地址无效"),
            Self::PolicyDenied(_) => AppError::invalid("插件资产地址被安全策略拒绝"),
            Self::RedirectLimit => AppError::invalid("插件资产重定向次数超限"),
            Self::Resolve { source } => {
                AppError::infrastructure_with_source("解析插件资产主机失败", source)
            }
            Self::Transport { source, .. } => {
                AppError::infrastructure_with_source("插件资产网络请求失败", source)
            }
            Self::Timeout => AppError::timeout("插件资产下载"),
            Self::Cancelled => AppError::cancelled("插件资产下载已取消"),
        }
    }
}

impl UrlRejection {
    /// 原因文案是源码常量，不能拼接调用方 URL，避免 query 或 userinfo 泄露。
    fn describe(self) -> &'static str {
        match self {
            Self::Malformed => "格式无效",
            Self::UnsupportedScheme => "协议不受支持",
            Self::MissingHost => "缺少主机",
            Self::UserInfo => "包含用户信息",
            Self::Fragment => "包含片段",
            Self::ZeroPort => "端口为 0",
        }
    }
}

impl PolicyRejection {
    /// 仅描述被拒绝的地址类别，不回显具体 IP 或域名。
    fn describe(self) -> &'static str {
        match self {
            Self::UnspecifiedAddress => "目标为未指定地址",
            Self::MulticastAddress => "目标为组播地址",
            Self::BroadcastAddress => "目标为广播地址",
            Self::MixedDns => "DNS 同时包含公网与非公网地址",
            Self::PublicToNonPublicRedirect => "重定向从公网进入非公网地址",
            Self::HttpsDowngrade => "禁止 HTTPS 降级到 HTTP",
        }
    }
}

impl fmt::Display for AssetNetworkError {
    /// Display 只输出固定安全文案，避免错误被 UI 或日志直接展示时带回 URL。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(reason) => {
                write!(formatter, "插件资产地址无效（{}）", reason.describe())
            }
            Self::InvalidRedirect => formatter.write_str("插件资产重定向地址无效"),
            Self::PolicyDenied(reason) => {
                write!(
                    formatter,
                    "插件资产地址被安全策略拒绝（{}）",
                    reason.describe()
                )
            }
            Self::RedirectLimit => formatter.write_str("插件资产重定向次数超限"),
            Self::Resolve { .. } => formatter.write_str("解析插件资产主机失败"),
            Self::Transport { .. } => formatter.write_str("插件资产网络请求失败"),
            Self::Timeout => formatter.write_str("插件资产下载超时"),
            Self::Cancelled => formatter.write_str("插件资产下载已取消"),
        }
    }
}

impl fmt::Debug for AssetNetworkError {
    /// 自定义 Debug 是敏感信息边界的一部分：底层 reqwest/io 错误可能包含 URL，
    /// 这里只暴露稳定分类和是否可重试。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(reason) => formatter.debug_tuple("InvalidUrl").field(reason).finish(),
            Self::InvalidRedirect => formatter.write_str("InvalidRedirect"),
            Self::PolicyDenied(reason) => {
                formatter.debug_tuple("PolicyDenied").field(reason).finish()
            }
            Self::RedirectLimit => formatter.write_str("RedirectLimit"),
            Self::Resolve { .. } => formatter.write_str("Resolve"),
            Self::Transport { retryable, .. } => formatter
                .debug_struct("Transport")
                .field("retryable", retryable)
                .finish(),
            Self::Timeout => formatter.write_str("Timeout"),
            Self::Cancelled => formatter.write_str("Cancelled"),
        }
    }
}

impl std::error::Error for AssetNetworkError {
    /// source 仅供内部错误链审计；公开文案与 Debug 不会展开它，避免令牌泄露。
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Resolve { source } => Some(source),
            Self::Transport { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// 公网 / 非公网只表达可否作为跨边界跳转目标；非公网仍可作为签名初始源。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AddressNature {
    Public,
    NonPublic,
}

/// 单个地址的最终分类；未指定、组播、广播即使在初始源中也不允许。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IpClass {
    Public,
    NonPublic,
    Prohibited(ProhibitedAddress),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProhibitedAddress {
    Unspecified,
    Multicast,
    Broadcast,
}

impl From<ProhibitedAddress> for PolicyRejection {
    /// 把内部分类收敛成稳定的策略原因，调用方不需要理解具体地址判定细节。
    fn from(value: ProhibitedAddress) -> Self {
        match value {
            ProhibitedAddress::Unspecified => Self::UnspecifiedAddress,
            ProhibitedAddress::Multicast => Self::MulticastAddress,
            ProhibitedAddress::Broadcast => Self::BroadcastAddress,
        }
    }
}

/// pin 的主机键不带端口：DNS 结果与端口无关，同域名跨端口重定向仍复用同一结果。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum HostId {
    Domain(String),
    Ip(IpAddr),
}

/// 一个主机的固定解析结果；后续请求只从这里构造 SocketAddr，不再触发二次解析。
/// 保留 IPv6 scope_id，避免链路本地地址在去重或换端口时丢失接口作用域。
#[derive(Clone, Debug)]
struct PinnedHost {
    addresses: Vec<SocketAddr>,
    nature: AddressNature,
}

/// 授权后的 URL 主机信息；域名额外保留规范化文本供 reqwest 覆盖解析。
struct HostInfo {
    id: HostId,
    domain: Option<String>,
    port: u16,
}

/// 一次资产下载共享的 pin 集合。重试必须复用同一 session，否则会重新解析并
/// 失去 DNS rebinding 防护；重定向也在这里累积每一跳的固定地址。
pub(super) struct AssetNetworkSession {
    policy: NetworkPolicy,
    pins: BTreeMap<HostId, PinnedHost>,
}

impl AssetNetworkSession {
    /// 每个下载任务独立创建，pin 生命周期与调用方的重试循环保持一致。
    pub(super) fn new(policy: NetworkPolicy) -> Self {
        Self {
            policy,
            pins: BTreeMap::new(),
        }
    }

    /// 生产入口只使用默认系统解析器和固定连接超时。
    pub(super) fn system() -> Self {
        Self::new(NetworkPolicy::system())
    }

    /// 发送资产 GET，并手动完成每一跳的 URL、DNS 与网络边界授权。
    ///
    /// 返回的响应仍受同一绝对截止时间和取消令牌约束；body 必须通过
    /// `AssetResponse::chunk` 读取，才能让 DNS、请求和 body 使用同一份契约。
    pub(super) async fn get(
        &mut self,
        value: &str,
        headers: HeaderMap,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<AssetResponse, AssetNetworkError> {
        ensure_live(deadline, cancellation)?;
        let mut current = validate_asset_url(value)?;
        let mut headers = sanitize_headers(headers);
        let mut redirects = 0usize;

        loop {
            ensure_live(deadline, cancellation)?;
            let pinned = self.pin_for(&current, deadline, cancellation).await?;
            let client = self.build_client(&current, &pinned, deadline)?;
            let request = client.get(current.clone()).headers(headers.clone());
            let response = with_control(
                async move { request.send().await.map_err(map_reqwest_error) },
                deadline,
                cancellation,
            )
            .await?;

            if is_followable_redirect(response.status()) {
                if redirects >= MAX_REDIRECTS {
                    return Err(AssetNetworkError::RedirectLimit);
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or(AssetNetworkError::InvalidRedirect)?;
                let next = redirect_target(&current, location)?;
                let next_pinned = self.pin_for(&next, deadline, cancellation).await?;
                authorize_redirect(pinned.nature, next_pinned.nature)?;
                strip_headers_for_redirect(&mut headers, &current, &next);
                redirects += 1;
                current = next;
                continue;
            }

            return Ok(AssetResponse {
                response,
                deadline,
                cancellation: cancellation.clone(),
            });
        }
    }

    /// 先查 pin 再解析：同一域名在下载、重试和重定向生命周期内只解析一次，
    /// 已授权地址不会因再次解析而被替换。
    async fn pin_for(
        &mut self,
        url: &Url,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<PinnedHost, AssetNetworkError> {
        let info = host_info(url)?;
        if let Some(pinned) = self.pins.get(&info.id) {
            return Ok(pinned.clone());
        }

        let addresses = match &info.id {
            // IP 字面量由 reqwest 直接连接，resolver 覆盖不会参与拦截，因此必须在
            // 构造请求前自行分类；这也是不依赖 peer 地址复核的关键原因。
            HostId::Ip(ip) => vec![SocketAddr::new(*ip, info.port)],
            HostId::Domain(_) => {
                let host = info
                    .domain
                    .as_deref()
                    .ok_or(AssetNetworkError::InvalidUrl(UrlRejection::MissingHost))?
                    .to_owned();
                let resolver = Arc::clone(&self.policy.resolver);
                let port = info.port;
                let addresses = with_control(
                    async move {
                        resolver
                            .resolve(&host, port)
                            .await
                            .map_err(|source| AssetNetworkError::Resolve { source })
                    },
                    deadline,
                    cancellation,
                )
                .await?;
                unique_addresses(addresses)
            }
        };

        let pinned = classify_resolved(&addresses)?;
        self.pins.insert(info.id, pinned.clone());
        Ok(pinned)
    }

    /// 每一跳单独构建客户端，只为本跳域名固定地址；`no_proxy` 防止环境代理
    /// 在远端解析后绕过本地地址校验，`Policy::none` 禁止 reqwest 自动跳转。
    /// 不调用任何跳过 TLS 证书或主机名验证的 API，连接目标固定在已授权 IP 上，
    /// 但 SNI 与证书校验仍以 URL 主机名为准。
    /// `connect_timeout` 只约束单次连接尝试；不设整体 `.timeout()`，让绝对
    /// 截止时间保持在 `with_control` 一处判定，避免两套超时语义互相抢跑。
    fn build_client(
        &self,
        url: &Url,
        pinned: &PinnedHost,
        deadline: Instant,
    ) -> Result<Client, AssetNetworkError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(AssetNetworkError::Timeout);
        }
        let mut builder = Client::builder()
            .no_proxy()
            .referer(false)
            .redirect(redirect::Policy::none())
            .connect_timeout(self.policy.connect_timeout.min(remaining));

        if let Some(Host::Domain(domain)) = url.host() {
            let port = url
                .port_or_known_default()
                .ok_or(AssetNetworkError::InvalidUrl(UrlRejection::Malformed))?;
            let addresses = pinned
                .addresses
                .iter()
                .map(|address| with_port(*address, port))
                .collect::<Vec<_>>();
            builder = builder.resolve_to_addrs(domain, &addresses);
        }

        builder.build().map_err(map_reqwest_error)
    }
}

/// `reqwest::Response` 的安全包装；把尚未读取的 body 也纳入同一截止时间和取消令牌。
pub(super) struct AssetResponse {
    response: Response,
    deadline: Instant,
    cancellation: CancellationToken,
}

impl AssetResponse {
    /// 状态码原样返回，成功、重试和错误响应的业务判断仍由下载器负责。
    pub(super) fn status(&self) -> reqwest::StatusCode {
        self.response.status()
    }

    /// 响应头用于复用既有 ETag/Content-Range 逻辑，读取头不会绕过 body 控制。
    pub(super) fn headers(&self) -> &HeaderMap {
        self.response.headers()
    }

    /// 现有下载器依赖响应长度做大小上限判断，这里保留同一语义。
    pub(super) fn content_length(&self) -> Option<u64> {
        self.response.content_length()
    }

    /// 每次读取 body 都同时等待取消和绝对截止时间；调用方不能绕过该契约直接
    /// 取走底层响应，否则 body 阶段的超时与取消将不再成立。
    pub(super) async fn chunk(&mut self) -> Result<Option<Bytes>, AssetNetworkError> {
        let deadline = self.deadline;
        let cancellation = self.cancellation.clone();
        let response = &mut self.response;
        with_control(
            async move { response.chunk().await.map_err(map_reqwest_error) },
            deadline,
            &cancellation,
        )
        .await
    }
}

/// 仅接受 HTTP(S)，并拒绝 userinfo、fragment、0 端口与缺失主机；query 原样保留但不记录。
///
/// 判定委托给协议层共享契约，本模块不再维护第二套 URL 语法实现；这样市场登记
/// 侧与安装器对同一 URL 的接受/拒绝及原因完全一致。
pub(super) fn validate_asset_url(value: &str) -> Result<Url, AssetNetworkError> {
    shared_validate_asset_url(value).map_err(|reason| AssetNetworkError::InvalidUrl(reason.into()))
}

/// 相对 Location 以当前 URL 为基准解析；拒绝降级、畸形地址和 userinfo/fragment/0 端口。
///
/// 语法与歧义判定（含 Location 原文的 authority 检查）全部委托共享契约；
/// HTTPS 降级是传输层边界策略，不属于 URL 语法，保留在本模块。
fn redirect_target(previous: &Url, location: &str) -> Result<Url, AssetNetworkError> {
    let next = shared_validate_redirect_target(previous, location)
        .map_err(|_| AssetNetworkError::InvalidRedirect)?;
    if previous.scheme() == "https" && next.scheme() == "http" {
        return Err(AssetNetworkError::PolicyDenied(
            PolicyRejection::HttpsDowngrade,
        ));
    }
    Ok(next)
}

/// 公网跳入非公网会跨过签名初始源授权的网络边界；非公网转到公网或非公网内部
/// 仍允许，因为初始本地源可能重定向到 CDN 或同一局域网内的镜像。
fn authorize_redirect(
    previous: AddressNature,
    next: AddressNature,
) -> Result<(), AssetNetworkError> {
    match (previous, next) {
        (AddressNature::Public, AddressNature::NonPublic) => Err(AssetNetworkError::PolicyDenied(
            PolicyRejection::PublicToNonPublicRedirect,
        )),
        _ => Ok(()),
    }
}

/// 提取主机身份与端口；域名统一小写，保证 pin 键与 reqwest 的覆盖键一致。
fn host_info(url: &Url) -> Result<HostInfo, AssetNetworkError> {
    let host = url
        .host()
        .ok_or(AssetNetworkError::InvalidUrl(UrlRejection::MissingHost))?;
    let port = url
        .port_or_known_default()
        .ok_or(AssetNetworkError::InvalidUrl(UrlRejection::Malformed))?;
    match host {
        Host::Ipv4(ip) => Ok(HostInfo {
            id: HostId::Ip(IpAddr::V4(ip)),
            domain: None,
            port,
        }),
        Host::Ipv6(ip) => Ok(HostInfo {
            id: HostId::Ip(IpAddr::V6(ip)),
            domain: None,
            port,
        }),
        Host::Domain(domain) => {
            let domain = domain.to_ascii_lowercase();
            Ok(HostInfo {
                id: HostId::Domain(domain.clone()),
                domain: Some(domain),
                port,
            })
        }
    }
}

/// 解析结果按出现顺序去重，保留系统解析给出的优先顺序和 IPv6 scope_id。
/// 用集合判重避免异常 DNS 返回大量重复地址时退化为平方复杂度。
fn unique_addresses(addresses: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let mut unique = Vec::with_capacity(addresses.len());
    let mut seen = HashSet::with_capacity(addresses.len());
    for address in addresses {
        if seen.insert((address.ip(), ipv6_scope(&address))) {
            unique.push(address);
        }
    }
    unique
}

/// IPv4 没有作用域，IPv6 的 scope_id 用于区分同一链路本地地址所在的接口。
fn ipv6_scope(address: &SocketAddr) -> u32 {
    match address {
        SocketAddr::V4(_) => 0,
        SocketAddr::V6(address) => address.scope_id(),
    }
}

/// 需要在同一 pin 上换端口时保留 scope_id，否则链路本地 IPv6 连接会失去接口。
fn with_port(address: SocketAddr, port: u16) -> SocketAddr {
    match address {
        SocketAddr::V4(address) => SocketAddr::new(IpAddr::V4(*address.ip()), port),
        SocketAddr::V6(address) => SocketAddr::V6(SocketAddrV6::new(
            *address.ip(),
            port,
            address.flowinfo(),
            address.scope_id(),
        )),
    }
}

/// 先把全部 A/AAAA 分类再决定 pin，任何未指定/组播/广播地址都直接拒绝；
/// 公网与非公网混合同样拒绝，避免连接回退跨过安全边界。
fn classify_resolved(addresses: &[SocketAddr]) -> Result<PinnedHost, AssetNetworkError> {
    if addresses.is_empty() {
        return Err(AssetNetworkError::Resolve {
            source: io::Error::new(io::ErrorKind::NotFound, "asset host has no address"),
        });
    }

    let mut public = false;
    let mut non_public = false;
    for address in addresses {
        match classify_ip(address.ip()) {
            IpClass::Prohibited(reason) => {
                return Err(AssetNetworkError::PolicyDenied(reason.into()));
            }
            IpClass::Public => public = true,
            IpClass::NonPublic => non_public = true,
        }
    }
    if public && non_public {
        return Err(AssetNetworkError::PolicyDenied(PolicyRejection::MixedDns));
    }

    Ok(PinnedHost {
        addresses: addresses.to_vec(),
        nature: if public {
            AddressNature::Public
        } else {
            AddressNature::NonPublic
        },
    })
}

/// IPv4-mapped IPv6 复用同一 IPv4 规则，防止用映射形式把私网或组播伪装成 IPv6。
fn classify_ip(ip: IpAddr) -> IpClass {
    match ip {
        IpAddr::V4(ip) => classify_ipv4(ip),
        IpAddr::V6(ip) => classify_ipv6(ip),
    }
}

/// 按 IANA 特殊用途段保守分类；保留、文档和过渡网段都不视为公网。
fn classify_ipv4(ip: Ipv4Addr) -> IpClass {
    let octets = ip.octets();
    if ip.is_unspecified() {
        return IpClass::Prohibited(ProhibitedAddress::Unspecified);
    }
    if ip.is_multicast() {
        return IpClass::Prohibited(ProhibitedAddress::Multicast);
    }
    if ip.is_broadcast() {
        return IpClass::Prohibited(ProhibitedAddress::Broadcast);
    }

    if ip.is_loopback()
        || octets[0] == 10
        || (octets[0] == 172 && (16..=31).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 168)
    {
        return IpClass::NonPublic;
    }
    // CGNAT 共享地址段。
    if octets[0] == 100 && (64..=127).contains(&octets[1]) {
        return IpClass::NonPublic;
    }
    // IPv4 链路本地。
    if octets[0] == 169 && octets[1] == 254 {
        return IpClass::NonPublic;
    }
    // IETF 协议分配、TEST-NET-1/2/3、6to4 中继任播与基准测试段。
    if (octets[0] == 192 && octets[1] == 0 && matches!(octets[2], 0 | 2))
        || (octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
        || (octets[0] == 198 && matches!(octets[1], 18 | 19))
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
    {
        return IpClass::NonPublic;
    }
    // 0/8 “本网络”与 240/4 保留段（广播地址已在前面的专用判定中拒绝）。
    if octets[0] == 0 || octets[0] >= 240 {
        return IpClass::NonPublic;
    }

    IpClass::Public
}

/// IPv6 的过渡、隧道和文档网段无法证明出口为公网，统一按非公网处理；
/// `::/96` 中包含已废弃的 IPv4-compatible 形式，也保守归入非公网。
fn classify_ipv6(ip: Ipv6Addr) -> IpClass {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return classify_ipv4(mapped);
    }
    if ip.is_unspecified() {
        return IpClass::Prohibited(ProhibitedAddress::Unspecified);
    }
    if ip.is_loopback() {
        return IpClass::NonPublic;
    }
    if ip.is_multicast() {
        return IpClass::Prohibited(ProhibitedAddress::Multicast);
    }

    let segments = ip.segments();
    if segments[..6].iter().all(|segment| *segment == 0) {
        return IpClass::NonPublic;
    }
    // NAT64（64:ff9b::/96 与本地 64:ff9b:1::/48）无法证明最终出口为公网。
    if segments[0] == 0x0064 && segments[1] == 0xff9b {
        return IpClass::NonPublic;
    }
    // 100::/64 丢弃前缀。
    if segments[0] == 0x0100 && segments[1..4].iter().all(|segment| *segment == 0) {
        return IpClass::NonPublic;
    }
    // 2001::/23 IETF 协议分配（含 Teredo、基准测试与 ORCHID）。
    if segments[0] == 0x2001 && segments[1] < 0x0200 {
        return IpClass::NonPublic;
    }
    // 2001:db8::/32 文档地址。
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return IpClass::NonPublic;
    }
    // 2002::/16 6to4、3ffe::/16 旧 6bone、3fff::/20 新文档段、5f00::/16 保留段。
    if segments[0] == 0x2002
        || segments[0] == 0x3ffe
        || (segments[0] == 0x3fff && (segments[1] & 0xf000) == 0)
        || segments[0] == 0x5f00
    {
        return IpClass::NonPublic;
    }
    // fc00::/7 唯一本地、fe80::/10 链路本地、fec0::/10 已废弃站点本地。
    if (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
    {
        return IpClass::NonPublic;
    }

    // 全球单播只包含 2000::/3；reviewer 证实过的漏网段如 IPv4-translated
    // `::ffff:0:0:0/96`、`::1:0:0:0:1`（回环形式残留）、fe00::/9 未分配段和
    // 4000::/3、8000::/3、c000::/3 预留空间，统一保守归入非公网，防止把无法
    // 证明为公网的目标当成公网继续跳转或稀释混合 DNS 判定。
    if segments[0] & 0xe000 == 0x2000 {
        IpClass::Public
    } else {
        IpClass::NonPublic
    }
}

/// 进入请求前先剔除会被 reqwest 重算或可能泄露的传输层头；Range/If-Range
/// 等业务头保留，供后续大小、摘要和续传逻辑继续使用。
fn sanitize_headers(mut headers: HeaderMap) -> HeaderMap {
    for name in SANITIZED_REQUEST_HEADERS {
        headers.remove(name);
    }
    headers
}

/// 跨 origin 不得携带 Authorization 与 Cookie；Referer、代理凭据和 Host
/// 无论是否跨 origin 都不转发，Range/If-Range 等安全头继续保留。
fn strip_headers_for_redirect(headers: &mut HeaderMap, previous: &Url, next: &Url) {
    if previous.origin() != next.origin() {
        headers.remove("authorization");
        headers.remove("cookie");
    }
    for name in ["host", "referer", "proxy-authorization"] {
        headers.remove(name);
    }
}

/// 只有标准重定向状态码才进入逐跳流程；304 等响应应原样交给上层判断。
fn is_followable_redirect(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
}

/// 统一等待取消、绝对截止时间和实际 future；biased 保证已取消/已超时不会
/// 再启动一次 DNS 或连接副作用。
async fn with_control<F, T>(
    future: F,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<T, AssetNetworkError>
where
    F: Future<Output = Result<T, AssetNetworkError>>,
{
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(AssetNetworkError::Cancelled),
        _ = tokio::time::sleep_until(deadline) => Err(AssetNetworkError::Timeout),
        result = future => result,
    }
}

/// 每跳开始和每次 await 前复用的快速检查，避免已经到期的尝试继续构造客户端。
fn ensure_live(
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<(), AssetNetworkError> {
    if cancellation.is_cancelled() {
        Err(AssetNetworkError::Cancelled)
    } else if Instant::now() >= deadline {
        Err(AssetNetworkError::Timeout)
    } else {
        Ok(())
    }
}

/// reqwest 错误可能携带完整 URL（含 query），先调用 `without_url` 再保留 source，
/// 防止上层展开错误链时重新泄露令牌。
///
/// 超时分类：本模块的 builder 只设 `connect_timeout`，从不设整体 `.timeout()` 或
/// 读超时，因此 reqwest 层出现的 `TimedOut` 只能是连接阶段（或 DNS）的单次尝试
/// 预算耗尽，整体绝对截止时间由 `with_control` 独占判定。连接超时与已耗尽的绝对
/// 截止时间是不同语义——前者下一次尝试可能成功——所以按可重试传输错误返回，并保留
/// `is_connect()` 判定让上层能区分；绝对 deadline 与取消由 biased select 优先
/// 上抛，不会被这里的映射伪报。
fn map_reqwest_error(error: reqwest::Error) -> AssetNetworkError {
    let error = error.without_url();
    if error.is_timeout() {
        return AssetNetworkError::Transport {
            source: error,
            retryable: true,
        };
    }
    let retryable = error.is_connect() || error.is_request() || error.is_body();
    AssetNetworkError::Transport {
        source: error,
        retryable,
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/plugin_install_network.rs"]
mod tests;
