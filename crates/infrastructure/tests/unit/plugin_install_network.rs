//! 插件资产安全网络模块的授权、DNS 固定、重定向与取消回归。
//!
//! 这里只覆盖可离线执行的策略和本机回环连接；真实公网/TLS、代理环境与跨平台
//! DNS 行为仍需在单独接线验收中执行，不能由本文件结果外推。
//!
//! @author kongweiguang

use super::*;
use async_trait::async_trait;
use kerminal_application::PublicErrorCode;
use reqwest::{
    StatusCode, Url,
    header::{
        AUTHORIZATION, COOKIE, ETAG, HOST, HeaderMap, HeaderValue, IF_RANGE, PROXY_AUTHORIZATION,
        RANGE, REFERER,
    },
};
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

/// 回环网络用例的绝对期限：macOS 慢 runner 上建立连接、读取响应头或完成三跳
/// 重定向可能停顿数秒，5 秒会把环境抖动误报成 Timeout。这里只放宽等待上限，
/// 各用例对状态码、重定向次数、body 超时和拒绝语义的断言都不变。
const SLOW_RUNNER_DEADLINE: Duration = Duration::from_secs(30);

/// 仅测试使用的连接超时构造器：连接超时是单次尝试预算而非安全边界，测试需要
/// 缩短它才能确定性触发连接阶段超时；生产入口仍固定 10 秒。构造器定义在测试
/// 模块里，让该入口在生产目标里不参与编译，也避免 src 保留 cfg(test) 项。
impl NetworkPolicy {
    fn with_connect_timeout(
        resolver: Arc<dyn AssetAddressResolver>,
        connect_timeout: Duration,
    ) -> Self {
        Self {
            resolver,
            connect_timeout,
        }
    }
}

/// 静态解析器记录调用次数，用来证明重试和同域名重定向不会再次解析。
struct CountingResolver {
    answers: Mutex<HashMap<String, Vec<IpAddr>>>,
    calls: AtomicUsize,
}

impl CountingResolver {
    /// 只保存 IP 而由 resolve 补齐端口，确保测试覆盖“同一解析结果用于不同端口”的 pin 复用。
    fn new(answers: Vec<(&str, Vec<IpAddr>)>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(
                answers
                    .into_iter()
                    .map(|(host, ips)| (host.to_owned(), ips))
                    .collect(),
            ),
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl AssetAddressResolver for CountingResolver {
    /// 把测试预置的 IP 补齐目标端口；调用计数在真正返回结果前递增。
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let ips = self
            .answers
            .lock()
            .map_err(|_| io::Error::other("resolver poisoned"))?
            .get(host)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing host"))?;
        Ok(ips
            .into_iter()
            .map(|ip| SocketAddr::new(ip, port))
            .collect())
    }
}

/// 永不返回的解析器用于验证取消和绝对截止时间能中断 DNS 等待。
struct PendingResolver;

#[async_trait]
impl AssetAddressResolver for PendingResolver {
    /// 永不完成的解析器把测试控制点放在 DNS 等待上，避免真实网络或解析缓存干扰。
    async fn resolve(&self, _host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
        std::future::pending::<io::Result<Vec<SocketAddr>>>().await
    }
}

/// 读取完整请求头，测试据此断言 Range、凭据和 Host 是否被正确保留或剥离。
async fn read_request_head(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = stream.read(&mut chunk).await.expect("request read");
        assert!(count > 0, "client closed before sending a request head");
        buffer.extend_from_slice(&chunk[..count]);
        assert!(buffer.len() <= 16 * 1024, "request head unexpectedly large");
    }
    String::from_utf8(buffer).expect("request head is valid UTF-8")
}

/// 写出完整 HTTP/1.1 响应并刷新，避免测试服务器在客户端读取前提前退出。
async fn write_response(stream: &mut TcpStream, response: &[u8]) {
    stream.write_all(response).await.expect("response write");
    stream.flush().await.expect("response flush");
}

/// 依次返回指定次数的相对 302，最后返回 200，用于验证允许的三跳链。
async fn serve_redirects_then_ok(listener: TcpListener, redirects: usize) {
    for index in 0..=redirects {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_head(&mut stream).await;
        if index < redirects {
            let location = format!("/hop/{index}");
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            write_response(&mut stream, response.as_bytes()).await;
        } else {
            write_response(
                &mut stream,
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndone",
            )
            .await;
        }
    }
}

/// 只返回 302；第四跳必须被客户端拒绝，因此服务器只需要处理四次请求。
async fn serve_only_redirects(listener: TcpListener, redirects: usize) {
    for index in 0..redirects {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_head(&mut stream).await;
        let location = format!("/hop/{index}");
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        write_response(&mut stream, response.as_bytes()).await;
    }
}

/// 发送响应头与部分 body 后保持连接，用于证明 body 阶段仍受取消和截止时间约束。
/// `tokio::spawn` 无需 await，这里必须是普通函数：若声明为 async fn，调用方
/// 直接拿到未展开的 future，会让外层 timeout 只等到 JoinHandle 而跳过服务器。
fn serve_stalled_body(listener: TcpListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_head(&mut stream).await;
        write_response(
            &mut stream,
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabc",
        )
        .await;
        let mut pending = [0u8; 1];
        let _ = stream.read(&mut pending).await;
    })
}

/// 接受 TCP 连接后保持流打开、不回复任何 TLS 字节的服务器：客户端的 TLS 握手
/// 在发出 ClientHello 后只能等 `connect_timeout` 到期，且服务器从未参与握手，
/// 因此不依赖证书信任或握手完成，连接阶段超时确定性触发。
fn serve_stalled_tls_handshake(listener: TcpListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        // 持续读但永不写：单次 read 会立即读到客户端的 ClientHello 字节并返回，
        // 若随之退出 task 会 drop 流、提前关闭连接，客户端收到的是连接关闭而非
        // 停留在握手等待；只有读到 EOF/错误（测试结束、客户端 abort）才退出，
        // 保证 TLS 连接始终停在建立阶段，直到 connect_timeout 或绝对截止时间到期。
        let mut pending = [0u8; 64];
        loop {
            match stream.read(&mut pending).await {
                Ok(0) | Err(_) => break,
                Ok(_) => continue,
            }
        }
    })
}

/// 测试样本只接受合法 IP 字面量，避免字符串分类表掩盖 URL 解析差异。
fn parse_ip(value: &str) -> IpAddr {
    value.parse().expect("valid IP literal")
}

/// 初始 URL 只放宽协议和主机；凭据、片段、缺失主机仍必须在发送前拒绝。
#[test]
fn asset_urls_require_http_scheme_and_reject_credentials_fragments_and_missing_hosts() {
    let accepted = validate_asset_url("https://[::1]:8443/asset?mirror=cn").expect("ipv6 asset");
    assert_eq!(accepted.query(), Some("mirror=cn"));
    assert!(validate_asset_url("http://127.0.0.1:8080/asset").is_ok());
    assert!(validate_asset_url("http://lan-host.local/asset").is_ok());

    assert!(matches!(
        validate_asset_url("ftp://example.com/asset"),
        Err(AssetNetworkError::InvalidUrl(
            UrlRejection::UnsupportedScheme
        ))
    ));
    assert!(matches!(
        validate_asset_url("data:text/plain,asset"),
        Err(AssetNetworkError::InvalidUrl(
            UrlRejection::UnsupportedScheme
        ))
    ));
    assert!(matches!(
        validate_asset_url("https://user:pass@example.com/asset"),
        Err(AssetNetworkError::InvalidUrl(UrlRejection::UserInfo))
    ));
    assert!(matches!(
        validate_asset_url("https://user@example.com/asset"),
        Err(AssetNetworkError::InvalidUrl(UrlRejection::UserInfo))
    ));
    assert!(matches!(
        validate_asset_url("https://example.com/asset#fragment"),
        Err(AssetNetworkError::InvalidUrl(UrlRejection::Fragment))
    ));
    assert!(validate_asset_url("").is_err());
    assert!(validate_asset_url("https:///missing").is_err());
    assert!(validate_asset_url("http:").is_err());
    // 0 端口随共享契约在语法层失败关闭；市场登记侧没有传输层兜底，两端必须一致。
    assert!(matches!(
        validate_asset_url("http://example.com:0/asset"),
        Err(AssetNetworkError::InvalidUrl(UrlRejection::ZeroPort))
    ));
}

/// 解析器会静默修复歧义 authority，或丢弃空 userinfo；必须在原文边界拒绝。
///
/// 锁定 url 2.5.8 的 `tests/unit.rs` 明确 `http:///mozilla.org` 可恢复解析，
/// 且 `http://@emptyuser.com/`、`http://:@emptypass.com/` 序列化时丢掉空凭据，
/// 因此仅检查解析后的 `username()`/`password()` 不足以覆盖这些输入。
#[test]
fn raw_authority_ambiguity_and_empty_userinfo_are_rejected() {
    for value in [
        "https:///missing",
        "https:////example.com/asset",
        "https:/example.com/asset",
        "http:example.com/asset",
        "http://@emptyuser.com/asset",
        "http://:@emptypass.com/asset",
        "http://user:@host.example/asset",
        "http://:pass@host.example/asset",
        "https://@/asset",
    ] {
        assert!(validate_asset_url(value).is_err(), "{value}");
    }
    assert!(matches!(
        validate_asset_url("http://@emptyuser.com/asset"),
        Err(AssetNetworkError::InvalidUrl(UrlRejection::UserInfo))
    ));
    assert!(matches!(
        validate_asset_url("http://:@emptypass.com/asset"),
        Err(AssetNetworkError::InvalidUrl(UrlRejection::UserInfo))
    ));

    // 控制字符、DEL 与反斜杠会被解析器裁剪、忽略或当成斜杠，属于解析歧义。
    for value in [
        "https://example.com/a\tb",
        " https://example.com/asset",
        "https://example.com/asset ",
        "https://exa\u{7f}mple.com/asset",
        "https://example.com/a\\b",
        "https://example.com\\asset",
    ] {
        assert!(validate_asset_url(value).is_err(), "{value:?}");
    }
}

/// 原文检查只针对 authority，不能误伤 path/query 中的 `@`、Unicode 与 IDN。
#[test]
fn raw_authority_check_keeps_legitimate_paths_queries_and_idn() {
    for value in [
        "https://example.com/@user/asset",
        "https://example.com/a?x=@y&mirror=cn",
        "https://example.com/路径@名/asset",
        "https://example.com/a b/asset",
        "https://例子.测试/asset",
        "https://example.com:8443/asset",
        "https://[2001:db8::1]:8443/asset",
        "HTTPS://example.com/asset",
    ] {
        // 片段必须由解析后的统一校验拒绝，这里只断言原文检查没有提前误判。
        let result = validate_asset_url(value);
        assert!(
            !matches!(
                result,
                Err(AssetNetworkError::InvalidUrl(
                    UrlRejection::Malformed | UrlRejection::UserInfo
                ))
            ),
            "{value}"
        );
    }
    // 可规范化 IDN 交由 url crate 做 IDNA 转换，不因原文含 Unicode 而被拒绝。
    let idn = validate_asset_url("https://例子.测试/asset").expect("idn asset");
    assert!(
        idn.host_str().is_some_and(|host| host.starts_with("xn--")),
        "{idn}"
    );
    assert!(validate_asset_url("https://example.com/路径@名/asset").is_ok());
    // 宿主 idna 1.1.0 已证实的 A-label 必须与共享契约一致地接受，防止市场登记
    // 侧再次出现误拒（如 xn--2ba 解码为 ¿）。
    assert!(validate_asset_url("https://xn--2ba.tv/asset").is_ok());
    assert!(validate_asset_url("https://xn--fsq.tv/path").is_ok());
}

/// 分类表覆盖公网、私网、回环、链路本地、CGNAT、保留、过渡和 IPv4 映射形式。
#[test]
fn ip_classification_is_conservative_for_special_and_transition_ranges() {
    let cases: &[(&str, IpClass)] = &[
        ("8.8.8.8", IpClass::Public),
        ("1.1.1.1", IpClass::Public),
        ("10.0.0.1", IpClass::NonPublic),
        ("172.16.0.1", IpClass::NonPublic),
        ("172.31.255.255", IpClass::NonPublic),
        ("172.32.0.1", IpClass::Public),
        ("192.168.1.1", IpClass::NonPublic),
        ("127.0.0.1", IpClass::NonPublic),
        ("169.254.1.1", IpClass::NonPublic),
        ("100.64.0.1", IpClass::NonPublic),
        ("192.0.0.1", IpClass::NonPublic),
        ("192.0.2.1", IpClass::NonPublic),
        ("192.88.99.1", IpClass::NonPublic),
        ("198.18.0.1", IpClass::NonPublic),
        ("198.51.100.1", IpClass::NonPublic),
        ("203.0.113.1", IpClass::NonPublic),
        ("240.0.0.1", IpClass::NonPublic),
        (
            "0.0.0.0",
            IpClass::Prohibited(ProhibitedAddress::Unspecified),
        ),
        (
            "224.0.0.1",
            IpClass::Prohibited(ProhibitedAddress::Multicast),
        ),
        (
            "255.255.255.255",
            IpClass::Prohibited(ProhibitedAddress::Broadcast),
        ),
        ("::ffff:127.0.0.1", IpClass::NonPublic),
        ("::ffff:8.8.8.8", IpClass::Public),
        ("::ffff:100.64.0.1", IpClass::NonPublic),
        ("::ffff:169.254.1.1", IpClass::NonPublic),
        (
            "::ffff:224.0.0.1",
            IpClass::Prohibited(ProhibitedAddress::Multicast),
        ),
        (
            "::ffff:255.255.255.255",
            IpClass::Prohibited(ProhibitedAddress::Broadcast),
        ),
        ("::1", IpClass::NonPublic),
        ("::", IpClass::Prohibited(ProhibitedAddress::Unspecified)),
        ("ff02::1", IpClass::Prohibited(ProhibitedAddress::Multicast)),
        ("fe80::1", IpClass::NonPublic),
        ("fc00::1", IpClass::NonPublic),
        ("64:ff9b::808:808", IpClass::NonPublic),
        ("2002:0808:0808::1", IpClass::NonPublic),
        ("2001:0:4136:e378::1", IpClass::NonPublic),
        ("2001:db8::1", IpClass::NonPublic),
        ("3fff::1", IpClass::NonPublic),
        ("5f00::1", IpClass::NonPublic),
        ("2606:4700:4700::1111", IpClass::Public),
        // reviewer 证实过的漏网段：尾部曾默认 Public，会破坏保守边界。
        ("::ffff:0:10.0.0.1", IpClass::NonPublic),
        ("::1:0:0:0:1", IpClass::NonPublic),
        ("fe00::1", IpClass::NonPublic),
        ("febf::1", IpClass::NonPublic),
        ("4000::1", IpClass::NonPublic),
        ("4000::ffff", IpClass::NonPublic),
        ("8000::1", IpClass::NonPublic),
        ("c000::1", IpClass::NonPublic),
        ("3ffe::1", IpClass::NonPublic),
        ("::ffff:0:8.8.8.8", IpClass::NonPublic),
        // 全球单播 2000::/3 的正常公网地址不被过度阻断。
        ("2000::1", IpClass::Public),
        ("2606:4700::1", IpClass::Public),
        ("2a00::1", IpClass::Public),
        ("3f00::1", IpClass::Public),
    ];

    for (value, expected) in cases {
        assert_eq!(classify_ip(parse_ip(value)), *expected, "{value}");
    }
}

/// 公网与私网混合的 DNS 必须在构造连接前拒绝，且只解析一次。
#[tokio::test]
async fn mixed_public_and_private_dns_is_rejected_before_connecting() {
    let resolver = CountingResolver::new(vec![(
        "mixed.test",
        vec![parse_ip("127.0.0.1"), parse_ip("8.8.8.8")],
    )]);
    let mut session = AssetNetworkSession::new(NetworkPolicy::new(resolver.clone()));
    let error = match session
        .get(
            "http://mixed.test:9/asset",
            HeaderMap::new(),
            Instant::now() + SLOW_RUNNER_DEADLINE,
            &CancellationToken::new(),
        )
        .await
    {
        Ok(_) => panic!("mixed DNS must be rejected"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), AssetNetworkErrorKind::Policy);
    assert!(
        !error.is_retryable(),
        "policy rejection must not be retried"
    );
    assert!(matches!(
        error,
        AssetNetworkError::PolicyDenied(PolicyRejection::MixedDns)
    ));
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
}

/// 解析结果含组播等禁止目标时整体拒绝，不能挑选其中“安全”的地址继续连接。
#[tokio::test]
async fn prohibited_dns_answers_are_rejected_before_connecting() {
    let resolver = CountingResolver::new(vec![(
        "prohibited.test",
        vec![parse_ip("8.8.8.8"), parse_ip("224.0.0.1")],
    )]);
    let mut session = AssetNetworkSession::new(NetworkPolicy::new(resolver));
    let error = match session
        .get(
            "http://prohibited.test:9/asset",
            HeaderMap::new(),
            Instant::now() + SLOW_RUNNER_DEADLINE,
            &CancellationToken::new(),
        )
        .await
    {
        Ok(_) => panic!("prohibited DNS answer must be rejected"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        AssetNetworkError::PolicyDenied(PolicyRejection::MulticastAddress)
    ));
}

/// 空解析结果不是“没有限制”，而是解析失败，不能继续建立连接。
#[tokio::test]
async fn empty_dns_answer_is_a_resolve_failure() {
    let resolver = CountingResolver::new(vec![("empty.test", Vec::new())]);
    let mut session = AssetNetworkSession::new(NetworkPolicy::new(resolver));
    let error = match session
        .get(
            "http://empty.test:9/asset",
            HeaderMap::new(),
            Instant::now() + SLOW_RUNNER_DEADLINE,
            &CancellationToken::new(),
        )
        .await
    {
        Ok(_) => panic!("empty DNS answer must fail"),
        Err(error) => error,
    };
    assert!(matches!(error, AssetNetworkError::Resolve { .. }));
    assert!(!error.is_retryable());
}

/// 签名初始源允许回环/局域网；重试复用同一 session 时不能重新解析或换地址。
#[tokio::test]
async fn signed_loopback_source_is_allowed_and_resolution_stays_pinned_across_retries() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let port = listener.local_addr().expect("address").port();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let _ = read_request_head(&mut stream).await;
            write_response(
                &mut stream,
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nok",
            )
            .await;
        }
    });

    let resolver = CountingResolver::new(vec![("pin.test", vec![parse_ip("127.0.0.1")])]);
    let mut session = AssetNetworkSession::new(NetworkPolicy::new(resolver.clone()));
    let url = format!("http://pin.test:{port}/asset");
    for _ in 0..2 {
        let mut response = session
            .get(
                &url,
                HeaderMap::new(),
                Instant::now() + SLOW_RUNNER_DEADLINE,
                &CancellationToken::new(),
            )
            .await
            .expect("pinned loopback request");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.content_length(), Some(2));
        assert_eq!(
            response
                .headers()
                .get(ETAG)
                .and_then(|value| value.to_str().ok()),
            Some("\"v1\"")
        );
        let chunk = response.chunk().await.unwrap().unwrap();
        assert_eq!(&chunk[..], &b"ok"[..]);
    }

    server.await.expect("server");
    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        1,
        "retry must reuse the pinned resolution"
    );
}

/// 三跳同域名相对重定向应成功且只解析一次；第四个重定向响应必须终止。
#[tokio::test]
async fn three_redirects_are_followed_and_a_fourth_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let port = listener.local_addr().expect("address").port();
    let server = tokio::spawn(serve_redirects_then_ok(listener, 3));
    let resolver = CountingResolver::new(vec![("redirect.test", vec![parse_ip("127.0.0.1")])]);
    let mut session = AssetNetworkSession::new(NetworkPolicy::new(resolver.clone()));
    let mut response = session
        .get(
            &format!("http://redirect.test:{port}/start"),
            HeaderMap::new(),
            Instant::now() + SLOW_RUNNER_DEADLINE,
            &CancellationToken::new(),
        )
        .await
        .expect("three redirects");
    let chunk = response.chunk().await.unwrap().unwrap();
    assert_eq!(&chunk[..], &b"done"[..]);
    server.await.expect("server");
    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        1,
        "same-domain redirects must reuse the pinned resolution"
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let port = listener.local_addr().expect("address").port();
    let server = tokio::spawn(serve_only_redirects(listener, 4));
    let error = match session
        .get(
            &format!("http://redirect.test:{port}/start"),
            HeaderMap::new(),
            Instant::now() + SLOW_RUNNER_DEADLINE,
            &CancellationToken::new(),
        )
        .await
    {
        Ok(_) => panic!("fourth redirect must be rejected"),
        Err(error) => error,
    };
    assert!(matches!(error, AssetNetworkError::RedirectLimit));
    server.await.expect("server");
    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        1,
        "retry must reuse the pinned resolution"
    );
}

/// Location 的降级、凭据和片段检查必须在解析目标后、建立连接前完成。
#[test]
fn redirect_target_rejects_downgrades_and_credentials_fragments() {
    let secure = Url::parse("https://public.example/asset").expect("url");
    assert!(matches!(
        redirect_target(&secure, "http://public.example/asset"),
        Err(AssetNetworkError::PolicyDenied(
            PolicyRejection::HttpsDowngrade
        ))
    ));
    assert!(matches!(
        redirect_target(&secure, "http://user:pass@public.example/asset"),
        Err(AssetNetworkError::InvalidRedirect)
    ));
    assert!(matches!(
        redirect_target(&secure, "#fragment"),
        Err(AssetNetworkError::InvalidRedirect)
    ));
    assert!(matches!(
        redirect_target(&secure, "http:///missing"),
        Err(AssetNetworkError::InvalidRedirect)
    ));
    assert!(matches!(
        redirect_target(&secure, "ftp://public.example/asset"),
        Err(AssetNetworkError::InvalidRedirect)
    ));
    assert!(redirect_target(&secure, "/next").is_ok());
    assert!(redirect_target(&secure, "https://cdn.example/asset").is_ok());

    // 网络路径引用同样会经过原文 authority 检查：空 userinfo 不能在 join 后被丢弃。
    for location in [
        "//@host/path",
        "//:@host/path",
        "///host/path",
        "//host\\path",
    ] {
        assert!(redirect_target(&secure, location).is_err(), "{location}");
    }
    // 普通相对路径、带 @ 的 path/query 与安全的网络路径引用必须继续可用。
    assert_eq!(
        redirect_target(&secure, "//cdn.example/asset")
            .expect("network path reference")
            .as_str(),
        "https://cdn.example/asset"
    );
    assert_eq!(
        redirect_target(&secure, "/@user/asset")
            .expect("path with at sign")
            .as_str(),
        "https://public.example/@user/asset"
    );
    assert_eq!(
        redirect_target(&secure, "?token=@value")
            .expect("query with at sign")
            .as_str(),
        "https://public.example/asset?token=@value"
    );
}

/// 可跟随的重定向状态缺少 Location 时不能退化成“成功”或继续当前请求。
#[tokio::test]
async fn redirect_without_location_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let port = listener.local_addr().expect("address").port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_head(&mut stream).await;
        write_response(
            &mut stream,
            b"HTTP/1.1 302 Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
    });

    let mut session = AssetNetworkSession::system();
    let error = match session
        .get(
            &format!("http://127.0.0.1:{port}/start"),
            HeaderMap::new(),
            Instant::now() + SLOW_RUNNER_DEADLINE,
            &CancellationToken::new(),
        )
        .await
    {
        Ok(_) => panic!("redirect without Location must be rejected"),
        Err(error) => error,
    };
    assert!(matches!(error, AssetNetworkError::InvalidRedirect));
    server.await.expect("server");
}

/// 公网进入非公网的边界规则与非公网转公网/非公网内部的允许方向。
#[test]
fn public_to_non_public_redirects_are_rejected() {
    assert!(matches!(
        authorize_redirect(AddressNature::Public, AddressNature::NonPublic),
        Err(AssetNetworkError::PolicyDenied(
            PolicyRejection::PublicToNonPublicRedirect
        ))
    ));
    assert!(authorize_redirect(AddressNature::NonPublic, AddressNature::Public).is_ok());
    assert!(authorize_redirect(AddressNature::NonPublic, AddressNature::NonPublic).is_ok());
    assert!(authorize_redirect(AddressNature::Public, AddressNature::Public).is_ok());
}

/// 曾被误判 Public 的 IPv6 目标在公网→目标同 scheme 跳转中必须按非公网拒绝；
/// 不经过 HTTPS 降级路径，直接用 http→http 验证地址分类本身。
#[test]
fn public_to_misclassified_v6_ranges_is_rejected_as_non_public() {
    for target in [
        "::ffff:0:10.0.0.1",
        "::1:0:0:0:1",
        "fe00::1",
        "febf::1",
        "4000::1",
        "8000::1",
        "c000::1",
    ] {
        // 先确认分类层已经把目标归为非公网，再断言跳转授权会拒绝。
        assert_eq!(
            classify_ip(parse_ip(target)),
            IpClass::NonPublic,
            "{target}"
        );
        let url = Url::parse(&format!("http://[{target}]/asset")).expect("target url");
        let info = host_info(&url).expect("host info");
        let pinned =
            classify_resolved(&[SocketAddr::new(info_id_ip(&info), 80)]).expect("resolved address");
        assert_eq!(pinned.nature, AddressNature::NonPublic, "{target}");
        assert!(matches!(
            authorize_redirect(AddressNature::Public, pinned.nature),
            Err(AssetNetworkError::PolicyDenied(
                PolicyRejection::PublicToNonPublicRedirect
            ))
        ));
    }
}

/// 提取 HostInfo 中的 IP 字面量，测试用；域名形式不存在直接 IP。
fn info_id_ip(info: &HostInfo) -> IpAddr {
    match &info.id {
        HostId::Ip(ip) => *ip,
        HostId::Domain(domain) => panic!("unexpected domain host: {domain}"),
    }
}

/// 两个混合 DNS 场景：IPv4 公网/私网组合与上述 IPv6 漏网段参与组合，都必须拒绝。
#[tokio::test]
async fn mixed_dns_including_misclassified_v6_ranges_is_rejected() {
    for (host, answers) in [
        (
            "mixed-v4.test",
            vec![parse_ip("127.0.0.1"), parse_ip("8.8.8.8")],
        ),
        (
            "mixed-v6.test",
            vec![parse_ip("2606:4700::1"), parse_ip("::ffff:0:10.0.0.1")],
        ),
        (
            "mixed-reserved.test",
            vec![parse_ip("2606:4700::1"), parse_ip("4000::1")],
        ),
    ] {
        let resolver = CountingResolver::new(vec![(host, answers)]);
        let mut session = AssetNetworkSession::new(NetworkPolicy::new(resolver.clone()));
        let error = match session
            .get(
                &format!("http://{host}:9/asset"),
                HeaderMap::new(),
                Instant::now() + SLOW_RUNNER_DEADLINE,
                &CancellationToken::new(),
            )
            .await
        {
            Ok(_) => panic!("mixed DNS on {host} must be rejected"),
            Err(error) => error,
        };
        assert!(
            matches!(
                error,
                AssetNetworkError::PolicyDenied(PolicyRejection::MixedDns)
            ),
            "{host}"
        );
        assert_eq!(
            resolver.calls.load(Ordering::SeqCst),
            1,
            "{host} must resolve exactly once"
        );
    }
}

/// 同 origin 重定向保留 Authorization/Cookie 与 Range/If-Range，但始终不发 Referer。
#[tokio::test]
async fn same_origin_redirect_keeps_authorization_cookie_and_range() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let port = listener.local_addr().expect("address").port();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.expect("accept");
        let head = read_request_head(&mut first).await.to_ascii_lowercase();
        assert!(head.contains("range: bytes=2-"), "{head}");
        assert!(head.contains("if-range: \"v1\""), "{head}");
        assert!(head.contains("authorization: bearer secret"), "{head}");
        assert!(head.contains("cookie: session=secret"), "{head}");
        assert!(!head.contains("referer:"), "{head}");
        assert!(!head.contains("host: evil.example"), "{head}");
        write_response(
            &mut first,
            b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;

        let (mut second, _) = listener.accept().await.expect("accept");
        let head = read_request_head(&mut second).await.to_ascii_lowercase();
        assert!(head.contains("range: bytes=2-"), "{head}");
        assert!(head.contains("if-range: \"v1\""), "{head}");
        assert!(head.contains("authorization: bearer secret"), "{head}");
        assert!(head.contains("cookie: session=secret"), "{head}");
        write_response(
            &mut second,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        )
        .await;
    });

    let mut headers = HeaderMap::new();
    headers.insert(RANGE, HeaderValue::from_static("bytes=2-"));
    headers.insert(IF_RANGE, HeaderValue::from_static("\"v1\""));
    headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
    headers.insert(COOKIE, HeaderValue::from_static("session=secret"));
    headers.insert(
        REFERER,
        HeaderValue::from_static("http://origin/secret?token=x"),
    );
    headers.insert(HOST, HeaderValue::from_static("evil.example"));

    let mut session = AssetNetworkSession::system();
    let mut response = session
        .get(
            &format!("http://127.0.0.1:{port}/start"),
            headers,
            Instant::now() + SLOW_RUNNER_DEADLINE,
            &CancellationToken::new(),
        )
        .await
        .expect("same-origin redirect");
    let chunk = response.chunk().await.unwrap().unwrap();
    assert_eq!(&chunk[..], &b"ok"[..]);
    server.await.expect("server");
}

/// 跨 origin 只允许携带安全业务头，Authorization/Cookie/Referer/代理凭据必须清空。
#[tokio::test]
async fn cross_origin_redirect_strips_credentials_but_keeps_range() {
    let target = TcpListener::bind("127.0.0.1:0").await.expect("target");
    let target_port = target.local_addr().expect("address").port();
    let target_server = tokio::spawn(async move {
        let (mut stream, _) = target.accept().await.expect("accept");
        let head = read_request_head(&mut stream).await.to_ascii_lowercase();
        assert!(head.contains("range: bytes=2-"), "{head}");
        assert!(head.contains("if-range: \"v1\""), "{head}");
        let expected_host = format!("host: 127.0.0.1:{target_port}");
        assert!(head.contains(expected_host.as_str()), "{head}");
        assert!(!head.contains("authorization:"), "{head}");
        assert!(!head.contains("cookie:"), "{head}");
        assert!(!head.contains("referer:"), "{head}");
        assert!(!head.contains("proxy-authorization:"), "{head}");
        write_response(
            &mut stream,
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nz",
        )
        .await;
    });

    let origin = TcpListener::bind("127.0.0.1:0").await.expect("origin");
    let origin_port = origin.local_addr().expect("address").port();
    let redirect_server = tokio::spawn(async move {
        let (mut stream, _) = origin.accept().await.expect("accept");
        let head = read_request_head(&mut stream).await.to_ascii_lowercase();
        assert!(head.contains("authorization: bearer secret"), "{head}");
        assert!(head.contains("cookie: session=secret"), "{head}");
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        write_response(&mut stream, response.as_bytes()).await;
    });

    let mut headers = HeaderMap::new();
    headers.insert(RANGE, HeaderValue::from_static("bytes=2-"));
    headers.insert(IF_RANGE, HeaderValue::from_static("\"v1\""));
    headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
    headers.insert(COOKIE, HeaderValue::from_static("session=secret"));
    headers.insert(
        REFERER,
        HeaderValue::from_static("http://origin/secret?token=x"),
    );
    headers.insert(
        PROXY_AUTHORIZATION,
        HeaderValue::from_static("Basic secret"),
    );

    let mut session = AssetNetworkSession::system();
    let mut response = session
        .get(
            &format!("http://127.0.0.1:{origin_port}/start"),
            headers,
            Instant::now() + SLOW_RUNNER_DEADLINE,
            &CancellationToken::new(),
        )
        .await
        .expect("cross-origin redirect");
    let chunk = response.chunk().await.unwrap().unwrap();
    assert_eq!(&chunk[..], &b"z"[..]);

    redirect_server.await.expect("redirect server");
    target_server.await.expect("target server");
}

/// IP 字面量不经过 reqwest resolver 覆盖，因此禁止地址必须在请求前由本模块拒绝。
#[tokio::test]
async fn prohibited_ip_literal_targets_are_rejected_without_connecting() {
    for (url, expected) in [
        (
            "http://224.0.0.1:9/asset",
            PolicyRejection::MulticastAddress,
        ),
        (
            "http://0.0.0.0:9/asset",
            PolicyRejection::UnspecifiedAddress,
        ),
        (
            "http://255.255.255.255:9/asset",
            PolicyRejection::BroadcastAddress,
        ),
        ("http://[::]:9/asset", PolicyRejection::UnspecifiedAddress),
    ] {
        let mut session = AssetNetworkSession::system();
        let error = match session
            .get(
                url,
                HeaderMap::new(),
                Instant::now() + Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
        {
            Ok(_) => panic!("prohibited literal {url} must be rejected"),
            Err(error) => error,
        };
        assert!(
            matches!(error, AssetNetworkError::PolicyDenied(reason) if reason == expected),
            "{url}"
        );
    }
}

/// DNS 等待必须同时响应取消和绝对截止时间，不能先建立连接再检查。
#[tokio::test]
async fn dns_wait_observes_cancellation_and_absolute_deadline() {
    let mut session = AssetNetworkSession::new(NetworkPolicy::new(Arc::new(PendingResolver)));
    let error = match session
        .get(
            "http://pending.test/asset",
            HeaderMap::new(),
            Instant::now() + Duration::from_millis(100),
            &CancellationToken::new(),
        )
        .await
    {
        Ok(_) => panic!("deadline must expire during DNS"),
        Err(error) => error,
    };
    assert!(matches!(error, AssetNetworkError::Timeout));

    let mut session = AssetNetworkSession::new(NetworkPolicy::new(Arc::new(PendingResolver)));
    let cancellation = CancellationToken::new();
    let cancel = cancellation.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();
    });
    let error = match session
        .get(
            "http://pending.test/asset",
            HeaderMap::new(),
            Instant::now() + SLOW_RUNNER_DEADLINE,
            &cancellation,
        )
        .await
    {
        Ok(_) => panic!("cancellation must stop DNS wait"),
        Err(error) => error,
    };
    assert!(matches!(error, AssetNetworkError::Cancelled));
    canceller.await.expect("canceller");
}

/// 响应头已返回但 body 未结束也必须受同一绝对截止时间约束。
#[tokio::test]
async fn body_read_observes_absolute_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let port = listener.local_addr().expect("address").port();
    let server = serve_stalled_body(listener);

    let mut session = AssetNetworkSession::system();
    // 放宽后的预算只是给 macOS 慢 runner 留出建立回环连接和读完响应头的环境余量；
    // 被验证的语义仍是 body 读取受同一绝对期限约束，而不是放宽超时判断。
    let mut response = session
        .get(
            &format!("http://127.0.0.1:{port}/stalled"),
            HeaderMap::new(),
            Instant::now() + SLOW_RUNNER_DEADLINE,
            &CancellationToken::new(),
        )
        .await
        .expect("headers");
    let chunk = response.chunk().await.unwrap().unwrap();
    assert_eq!(&chunk[..], &b"abc"[..]);
    let error = match response.chunk().await {
        Ok(_) => panic!("body deadline must expire"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), AssetNetworkErrorKind::Timeout);
    assert!(matches!(error, AssetNetworkError::Timeout));

    drop(response);
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("server must observe closed connection")
        .expect("server");
}

/// body 读取等待期间取消必须立即生效，并关闭连接而不是继续缓冲。
#[tokio::test]
async fn body_read_observes_cancellation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let port = listener.local_addr().expect("address").port();
    let server = serve_stalled_body(listener);

    let cancellation = CancellationToken::new();
    let mut session = AssetNetworkSession::system();
    let mut response = session
        .get(
            &format!("http://127.0.0.1:{port}/stalled"),
            HeaderMap::new(),
            Instant::now() + SLOW_RUNNER_DEADLINE,
            &cancellation,
        )
        .await
        .expect("headers");
    let chunk = response.chunk().await.unwrap().unwrap();
    assert_eq!(&chunk[..], &b"abc"[..]);

    let cancel = cancellation.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();
    });
    let error = match response.chunk().await {
        Ok(_) => panic!("body cancellation must stop reads"),
        Err(error) => error,
    };
    assert!(matches!(error, AssetNetworkError::Cancelled));
    canceller.await.expect("canceller");

    drop(response);
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("server must observe closed connection")
        .expect("server");
}

/// 网络错误转成应用错误时保留取消、超时和策略边界，供未来下载器直接接线。
#[test]
fn network_errors_map_to_stable_application_codes() {
    let cases = [
        (
            AssetNetworkError::InvalidUrl(UrlRejection::Malformed),
            PublicErrorCode::Invalid,
        ),
        (
            AssetNetworkError::PolicyDenied(PolicyRejection::MixedDns),
            PublicErrorCode::Invalid,
        ),
        (AssetNetworkError::RedirectLimit, PublicErrorCode::Invalid),
        (AssetNetworkError::Timeout, PublicErrorCode::Timeout),
        (AssetNetworkError::Cancelled, PublicErrorCode::Cancelled),
        (
            AssetNetworkError::Resolve {
                source: io::Error::new(io::ErrorKind::NotFound, "missing"),
            },
            PublicErrorCode::Infrastructure,
        ),
    ];
    for (error, expected) in cases {
        assert_eq!(error.into_app_error().public_code(), expected);
    }
}

/// 传输错误只能暴露固定文案，不能把 reqwest 错误里携带的 query 令牌带回上层。
#[tokio::test]
async fn transport_errors_do_not_expose_query_tokens() {
    let mut session = AssetNetworkSession::system();
    let error = match session
        .get(
            "http://127.0.0.1:1/asset?token=topsecret",
            HeaderMap::new(),
            Instant::now() + Duration::from_secs(2),
            &CancellationToken::new(),
        )
        .await
    {
        Ok(_) => panic!("connection to port 1 unexpectedly succeeded"),
        Err(error) => error,
    };
    let rendered = format!("{error:?} {error}");
    assert!(!rendered.contains("topsecret"), "{rendered}");
    assert!(!rendered.contains("token="), "{rendered}");
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    while let Some(current) = source {
        let rendered = format!("{current}");
        assert!(!rendered.contains("topsecret"), "{rendered}");
        assert!(!rendered.contains("token="), "{rendered}");
        source = std::error::Error::source(current);
    }
}

/// 连接阶段的 `connect_timeout` 到期而绝对截止时间尚有余量时，错误必须是可重试
/// 的传输错误，而不是把单次尝试预算伪报成整段下载超时。
///
/// 确定性来源：服务器接受 TCP 后把 TLS 握手挂起，`connect_timeout` 覆盖整个
/// 连接建立（reqwest connect.rs 的 `with_timeout` 包住 connector 调用），因此
/// 300ms 的连接窗口在回环上必然先于 10s 的绝对截止时间耗尽。HTTPS 响应头永远
/// 不会返回，error 只能来自连接阶段，分类归属无歧义。
#[tokio::test]
async fn connect_phase_timeout_is_a_retryable_transport_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = serve_stalled_tls_handshake(listener);

    let mut session = AssetNetworkSession::new(NetworkPolicy::with_connect_timeout(
        Arc::new(SingleAddressResolver {
            address: SocketAddr::new(address.ip(), address.port()),
        }),
        Duration::from_millis(300),
    ));
    let error = match session
        .get(
            &format!("https://stalled-tls.test:{}/asset", address.port()),
            HeaderMap::new(),
            Instant::now() + Duration::from_secs(10),
            &CancellationToken::new(),
        )
        .await
    {
        Ok(_) => panic!("stalled TLS handshake must hit connect timeout"),
        Err(error) => error,
    };

    let AssetNetworkError::Transport { retryable, .. } = &error else {
        panic!("connect timeout must map to Transport, got {error:?}");
    };
    assert_eq!(error.kind(), AssetNetworkErrorKind::Transport);
    assert!(retryable, "connect-phase timeout must remain retryable");
    assert!(error.is_retryable());

    server.abort();
    let _ = server.await;
}

/// 绝对截止时间先于连接阶段耗尽时，`with_control` 的 Timeout 必须优先上抛，
/// 不能被后来的 reqwest 超时映射覆盖；这是取消/截止时间与单次尝试预算的边界。
///
/// 用同一个 TLS 停滞服务器：截止时间 100ms 远短于 5s 的 connect_timeout，
/// biased select 的 deadline 分支必然先触发。
#[tokio::test]
async fn absolute_deadline_still_wins_over_connect_phase() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = serve_stalled_tls_handshake(listener);

    let mut session = AssetNetworkSession::new(NetworkPolicy::with_connect_timeout(
        Arc::new(SingleAddressResolver {
            address: SocketAddr::new(address.ip(), address.port()),
        }),
        Duration::from_secs(5),
    ));
    let error = match session
        .get(
            &format!("https://stalled-tls.test:{}/asset", address.port()),
            HeaderMap::new(),
            Instant::now() + Duration::from_millis(100),
            &CancellationToken::new(),
        )
        .await
    {
        Ok(_) => panic!("expired deadline must surface as Timeout"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), AssetNetworkErrorKind::Timeout);
    assert!(matches!(error, AssetNetworkError::Timeout));
    assert!(
        !error.is_retryable(),
        "deadline exhaustion is not retryable"
    );

    server.abort();
    let _ = server.await;
}

/// 只返回单个固定地址的解析器，让 TLS 测试把回环地址固定给任意域名，
/// 与生产 `resolve_to_addrs` 的 pin 路径一致，不触发真实 DNS。
struct SingleAddressResolver {
    address: SocketAddr,
}

#[async_trait]
impl AssetAddressResolver for SingleAddressResolver {
    async fn resolve(&self, _host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new(self.address.ip(), port)])
    }
}
