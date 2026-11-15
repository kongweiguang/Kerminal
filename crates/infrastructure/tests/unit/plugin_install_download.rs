//! 可恢复下载、安全网络边界和主动取消的本地传输回归。
//!
//! 所有用例只连接 127.0.0.1 上临时启动的服务端，不访问互联网；真实公网/TLS、
//! 代理环境与跨平台 DNS 行为仍需单独接线验收，不能由本文件结果外推。超时路径
//! 通过 `download_until` 注入短 deadline 验证，不等待真实下载预算。
//!
//! 传输完成不代表完整性确认：size 与 SHA-256 由安装器最终核验，本文件只证明
//! 传输层契约（续传判别、取消、策略、重试上限与落盘时机）。
//!
//! @author kongweiguang

use kerminal_application::{
    AppError, PublicErrorCode,
    extensions::{CatalogAsset, InstallProgress},
};
use kerminal_plugin_protocol::Platform;
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{oneshot, watch},
};
use tokio_util::sync::CancellationToken;

use super::{
    EtagPresence, StagingCreate, await_sync_then_verify, classify_etag, download, download_until,
    drain_inflight, guard_staging_side_effect, open_verified_staging, sync_verified_complete,
};
use reqwest::header::{ETAG, HeaderMap, HeaderValue};

/// 注入 deadline 的慢 runner 余量：CI 上线程调度可能停顿数十到数百毫秒，
/// 50ms 级窗口会在用例断言“deadline 尚未到期”之前就过期，把真实超时路径
/// 误判成环境抖动。这些用例随后都显式 sleep 到 deadline，放宽只增加等待上限，
/// 不改变“到期后必须报 Timeout”的断言。
const INJECTED_DEADLINE_SLACK: Duration = Duration::from_secs(1);

/// 按 RFC 9110 entity-tag 语法直接判别 `classify_etag` 的三态分类。
///
/// 合法强值、含逗号强值、obs-text 强值都判定为 Strong；弱值、内部引号、
/// 多头、非引号形态都不得当作强锚点。每个 case 注明判定理由，防回归时
/// 把逗号或 obs-text 误判回非法。实现放在本测试文件内、经 `#[path]` 子模块
/// 引用源码，保持私有分类函数可访问而源码不内联测试体。
fn single(value: &[u8]) -> HeaderValue {
    HeaderValue::from_bytes(value).expect("header value")
}

/// 锁定 `classify_etag` 对三类易错形态的契约，防止判定标准回归漂移。
///
/// 弱值必须留在 Suspicious：`W/"…"` 一旦被误判成强值就会作为 If-Range 锚点
/// 发出，服务器据此做版本判定等于用弱校验值续传，曾在本任务中真实发生；
/// 逗号必须放行为 Strong：etagc 字节范围包含 0x2C，`"a,b"` 是合法强值，误拒
/// 会让可用锚点退化为整包重取；obs-text 必须放行为 Strong：分类走
/// `HeaderValue::as_bytes` 原始字节而非 `to_str`，合法非 UTF-8 标签不能被
/// UTF-8 约束误拒。三态的其余形态（内部引号、重复头、非引号、未闭合）一并
/// 锚定，保证 200 锚点与 206 回显两条生产路径共用同一标准。
#[test]
fn classify_etag_follows_rfc_9110_entity_tag_syntax() {
    let strong = |value: &[u8]| {
        let mut headers = HeaderMap::new();
        headers.insert(ETAG, single(value));
        assert!(
            matches!(classify_etag(&headers), EtagPresence::Strong(_)),
            "{value:?} must be a strong etag"
        );
    };
    let suspicious = |headers: HeaderMap, why: &str| {
        assert!(
            matches!(classify_etag(&headers), EtagPresence::Suspicious),
            "{why} must be suspicious"
        );
    };
    // 基本强值。
    strong(b"\"v1\"");
    // etagc 允许逗号："a,b" 是合法强值，不能因逗号误拒。
    strong(b"\"a,b\"");
    // obs-text（0x80..=0xFF）在 etagc 中合法；HeaderValue 原始字节校验
    // 必须放行，不经过 to_str 的 UTF-8 约束。
    strong(b"\"\xE4\xB8\xAD\"");
    // 空引号体仍是合法强值。
    strong(b"\"\"");
    // 弱值不是强校验值，绝不能发进 If-Range。
    let mut weak = HeaderMap::new();
    weak.insert(ETAG, single(b"W/\"weak\""));
    suspicious(weak, "weak etag");
    // 内部 DQUOTE 使 "a","b" 非法；逗号合法但引号不合法。
    let mut internal_quote = HeaderMap::new();
    internal_quote.insert(ETAG, single(b"\"a\",\"b\""));
    suspicious(internal_quote, "internal dquote");
    // 重复头语义不明。
    let mut duplicated = HeaderMap::new();
    duplicated.insert(ETAG, single(b"\"v1\""));
    duplicated.append(ETAG, single(b"\"v1\""));
    suspicious(duplicated, "duplicate headers");
    // 非引号形态与未闭合引号。
    let mut unquoted = HeaderMap::new();
    unquoted.insert(ETAG, single(b"v1"));
    suspicious(unquoted, "unquoted");
    let mut unclosed = HeaderMap::new();
    unclosed.insert(ETAG, single(b"\"v1"));
    suspicious(unclosed, "unclosed quote");
}

/// 无 ETag 头是 Absent：206 回显缺失时依赖该态放行。
#[test]
fn classify_etag_reports_absent_for_missing_header() {
    let headers = HeaderMap::new();
    assert!(matches!(classify_etag(&headers), EtagPresence::Absent));
}

/// 读到请求头结束并统一小写，方便断言方法与 Range/If-Range；body 不在测试范围内。
async fn read_request_head(stream: &mut TcpStream) -> String {
    let mut request = [0u8; 4096];
    let mut read = 0;
    while !request[..read].ends_with(b"\r\n\r\n") {
        let count = stream.read(&mut request[read..]).await.expect("request");
        assert!(count > 0 && read + count < request.len());
        read += count;
    }
    String::from_utf8_lossy(&request[..read]).to_ascii_lowercase()
}

/// 带超时接受连接；超时表示客户端不再发起请求，服务端任务据此自然结束，
/// 这样“策略拒绝不重试”“重试上限为四次”都能用连接次数做断言。
async fn accept_before(listener: &TcpListener, timeout: Duration) -> Option<TcpStream> {
    match tokio::time::timeout(timeout, listener.accept()).await {
        Ok(Ok((stream, _))) => Some(stream),
        _ => None,
    }
}

/// 组装最小资产条目，避免每个用例重复填写无关字段。
fn asset(url: String, size: u64) -> CatalogAsset {
    CatalogAsset {
        platform: Platform::Any,
        url,
        size,
        sha256: "0".repeat(64),
    }
}

/// 目录已验签的资产可能来自局域网或回环；旧验收开关为 false 时也必须能下载。
///
/// 资产授权已由签名目录完成，下载器不能再用旧白名单二次拒绝初始源；真正的
/// 网络边界由共享 session 在请求前完成 URL、DNS 和地址分类。
#[tokio::test]
async fn signed_loopback_asset_downloads_without_acceptance_switch() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let mut stream = listener.accept().await.expect("accept").0;
        read_request_head(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nlan")
            .await
            .expect("response");
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    download(
        &asset(format!("http://{address}/plugin"), 3),
        &path,
        false,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect("signed loopback asset");
    server.await.expect("server");
    assert_eq!(std::fs::read(path).expect("download bytes"), b"lan");
}

/// 策略拒绝必须先于网络连接：非 HTTP(S) 或带 userinfo 的地址不能被重试吞掉。
#[tokio::test]
async fn unsupported_or_credential_urls_are_rejected_before_connecting() {
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    for url in [
        "ftp://127.0.0.1/plugin".to_owned(),
        "http://user:pass@127.0.0.1/plugin".to_owned(),
    ] {
        let error = download(
            &asset(url, 3),
            &path,
            true,
            &progress,
            &CancellationToken::new(),
        )
        .await
        .expect_err("invalid asset url");
        assert_eq!(error.public_code(), PublicErrorCode::Invalid);
        assert!(!path.exists(), "policy rejection must not create staging");
    }
}

/// 实际重定向要经过共享 session 的逐跳授权，最终响应仍写进同一暂存文件。
#[tokio::test]
async fn follows_authorized_redirect_to_signed_asset() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        for index in 0..2 {
            let mut stream = listener.accept().await.expect("accept").0;
            let request = read_request_head(&mut stream).await;
            if index == 0 {
                assert!(request.starts_with("get /plugin "), "{request}");
                stream
                    .write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: /mirror\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("redirect");
            } else {
                assert!(request.starts_with("get /mirror "), "{request}");
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\ncdn",
                    )
                    .await
                    .expect("redirected body");
            }
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    download(
        &asset(format!("http://{address}/plugin"), 3),
        &path,
        false,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect("redirected download");
    server.await.expect("server");
    assert_eq!(std::fs::read(path).expect("download bytes"), b"cdn");
}

/// 重定向到组播等禁止目标时由策略直接终止，不能当成传输抖动重试或继续连接。
#[tokio::test]
async fn prohibited_redirect_target_is_rejected_without_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        // 策略拒绝发生在第二跳连接之前，因此只能观察到第一跳请求。
        // 首跳窗口是环境准备余量：慢 runner 上客户端完成 URL 校验、pinning 和
        // 建 client 可能远超 500ms，窗口过短会让连接错误（Infrastructure）冒充
        // 策略拒绝（Invalid），把用例变成环境抖动；这里只放宽等待时间，
        // “策略拒绝不重试”仍由请求次数断言保证，不涉及任何超时语义。
        while let Some(mut stream) = accept_before(&listener, Duration::from_secs(5)).await {
            counter.fetch_add(1, Ordering::SeqCst);
            read_request_head(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://224.0.0.1:9/asset\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("redirect");
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = download(
        &asset(format!("http://{address}/plugin"), 3),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect_err("prohibited redirect");
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    server.await.expect("server");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "policy rejection must not be retried"
    );
    assert!(!path.exists(), "policy rejection must not create staging");
}

/// 目录大小是硬上限：声明长度超过目录大小的整包响应在读取前就被拒绝。
#[tokio::test]
async fn oversized_content_length_is_rejected_before_body_write() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let mut stream = listener.accept().await.expect("accept").0;
        read_request_head(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\n")
            .await
            .expect("headers");
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = download(
        &asset(format!("http://{address}/plugin"), 3),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect_err("oversized response");
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    server.await.expect("server");
}

/// 续传时服务器换了 ETag 说明内容已变，不能把新字节拼到旧前缀后面。
#[tokio::test]
async fn changed_etag_on_resume_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        while counter.load(Ordering::SeqCst) < 2 {
            let Some(mut stream) = accept_before(&listener, Duration::from_secs(2)).await else {
                break;
            };
            let index = counter.fetch_add(1, Ordering::SeqCst);
            let request = read_request_head(&mut stream).await;
            if index == 0 {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nabc",
                    )
                    .await
                    .expect("first response");
            } else {
                assert!(request.contains("if-range: \"v1\""), "{request}");
                // 客户端可能在读完响应头后立即因 ETag 不匹配断开，因此这里不把
                // 写回失败当测试失败；断言点在客户端返回的错误与请求次数。
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nETag: \"v2\"\r\nConnection: close\r\n\r\ndef",
                    )
                    .await;
            }
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = download(
        &asset(format!("http://{address}/plugin"), 6),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect_err("changed etag");
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    server.await.expect("server");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "must stop after the mismatch"
    );
    assert_eq!(std::fs::read(path).expect("partial bytes"), b"abc");
}

/// 无强 ETag 时不得发送 Range：中断后只能整包重取，旧暂存不拼接其他版本前缀。
#[tokio::test]
async fn resume_without_strong_etag_refetches_whole_resource() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        while counter.load(Ordering::SeqCst) < 2 {
            let Some(mut stream) = accept_before(&listener, Duration::from_secs(2)).await else {
                break;
            };
            let index = counter.fetch_add(1, Ordering::SeqCst);
            let request = read_request_head(&mut stream).await;
            if index == 0 {
                // 首次响应不带任何 ETag，且在声明长度前截断。
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc",
                    )
                    .await
                    .expect("truncated response");
            } else {
                // 续传请求绝不能带 Range/If-Range，否则旧前缀可能属于另一版本。
                assert!(!request.contains("range:"), "{request}");
                assert!(!request.contains("if-range:"), "{request}");
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nxyzdef",
                    )
                    .await
                    .expect("full refetch");
            }
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    download(
        &asset(format!("http://{address}/plugin"), 6),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect("full refetch must succeed");
    server.await.expect("server");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "one truncated attempt plus one refetch"
    );
    assert_eq!(
        std::fs::read(path).expect("refetched bytes"),
        b"xyzdef",
        "stale prefix must be replaced, not appended to"
    );
}

/// 已有部分文件但没有可验证的强 ETag 时，从零重取而不是对未知前缀发 Range。
#[tokio::test]
async fn existing_partial_without_etag_refetches_from_zero() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let mut stream = listener.accept().await.expect("accept").0;
        let request = read_request_head(&mut stream).await;
        assert!(!request.contains("range:"), "{request}");
        assert!(!request.contains("if-range:"), "{request}");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nwhole!")
            .await
            .expect("response");
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    // 上一会话遗留的部分文件，本次下载没有任何已确认的强 ETag。
    std::fs::write(&path, b"old").expect("stale partial");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    download(
        &asset(format!("http://{address}/plugin"), 6),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect("refetch over stale partial");
    server.await.expect("server");
    assert_eq!(
        std::fs::read(path).expect("replaced bytes"),
        b"whole!",
        "unverifiable prefix must not survive as a suffix base"
    );
}

/// 畸形或重复的 ETag 头不能成为续传锚点：截断 200 之后必须整包重取。
///
/// 单头 `"a","b"`（内部引号）按 entity-tag 字节语法不是合法强值；重复 ETag
/// 头同样语义不明。若旧实现放行其中之一，第二个请求会带上无效 If-Range；
/// 正确行为是与无锚点路径一致——断言第二请求不带 Range/If-Range，且随后
/// 合法整包 200 成功，不从疑似异版本前缀续传。
#[tokio::test]
async fn malformed_or_duplicate_etag_anchor_does_not_send_range() {
    for (label, anchor) in [
        ("malformed internal quotes", "ETag: \"a\",\"b\"\r\n"),
        ("duplicate etag headers", "ETag: \"v1\"\r\nETag: \"v1\"\r\n"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let address = listener.local_addr().expect("address");
        let anchor = anchor.to_owned();
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            while counter.load(Ordering::SeqCst) < 2 {
                let Some(mut stream) = accept_before(&listener, Duration::from_secs(2)).await
                else {
                    break;
                };
                let index = counter.fetch_add(1, Ordering::SeqCst);
                let request = read_request_head(&mut stream).await;
                if index == 0 {
                    // 截断响应携带可疑锚点：内容同长度但版本不同。
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: 6\r\n{anchor}Connection: close\r\n\r\nabc"
                    );
                    stream
                        .write_all(response.as_bytes())
                        .await
                        .expect("truncated response with suspicious anchor");
                } else {
                    assert!(!request.contains("range:"), "{label}: {request}");
                    assert!(!request.contains("if-range:"), "{label}: {request}");
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"fresh\"\r\nConnection: close\r\n\r\nwhole!",
                        )
                        .await
                        .expect("clean full response");
                }
            }
        });
        let root = tempfile::tempdir().expect("temporary root");
        let path = root.path().join("download");
        let (progress, _) = watch::channel(InstallProgress::Preparing);
        let result = download(
            &asset(format!("http://{address}/plugin"), 6),
            &path,
            true,
            &progress,
            &CancellationToken::new(),
        )
        .await;
        assert!(
            result.is_ok(),
            "{label}: clean refetch must succeed, got {result:?}"
        );
        server.await.expect("server");
        assert_eq!(
            std::fs::read(&path).expect("refetched bytes"),
            b"whole!",
            "{label}: suspicious anchor must fall back to a full refetch"
        );
    }
}

/// 弱 ETag（W/ 前缀）不是强校验值：即使出现也不允许带 If-Range 续传。
#[tokio::test]
async fn weak_etag_is_not_used_for_conditional_resume() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        while counter.load(Ordering::SeqCst) < 2 {
            let Some(mut stream) = accept_before(&listener, Duration::from_secs(2)).await else {
                break;
            };
            let index = counter.fetch_add(1, Ordering::SeqCst);
            let request = read_request_head(&mut stream).await;
            if index == 0 {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: W/\"weak\"\r\nConnection: close\r\n\r\nabc",
                    )
                    .await
                    .expect("truncated response");
            } else {
                // 弱值不能作为锚点：不得发送 Range，也不得携带 If-Range。
                assert!(!request.contains("range:"), "{request}");
                assert!(!request.contains("if-range:"), "{request}");
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nwhole!",
                    )
                    .await
                    .expect("full refetch");
            }
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    download(
        &asset(format!("http://{address}/plugin"), 6),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect("weak etag must trigger full refetch");
    server.await.expect("server");
    assert_eq!(
        std::fs::read(path).expect("refetched bytes"),
        b"whole!",
        "weak etag must not validate the stale prefix"
    );
}

/// 有强 ETag 时中断后必须带 Range 与 If-Range 续传，剩余字节拼接后完整。
#[tokio::test]
async fn interrupted_download_resumes_with_matching_range() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        for index in 0..2 {
            let mut stream = listener.accept().await.expect("accept").0;
            let request = read_request_head(&mut stream).await;
            if index == 0 {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"test\"\r\nConnection: close\r\n\r\nabc",
                    )
                    .await
                    .expect("first response");
            } else {
                assert!(request.contains("range: bytes=3-"), "{request}");
                assert!(request.contains("if-range: \"test\""), "{request}");
                stream
                    .write_all(
                        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nETag: \"test\"\r\nConnection: close\r\n\r\ndef",
                    )
                    .await
                    .expect("second response");
            }
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, receiver) = watch::channel(InstallProgress::Preparing);
    download(
        &asset(format!("http://{address}/plugin"), 6),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect("download");
    server.await.expect("server");
    assert_eq!(std::fs::read(path).expect("download bytes"), b"abcdef");
    assert_eq!(
        *receiver.borrow(),
        InstallProgress::Downloading {
            received: 6,
            total: 6
        }
    );
}

/// 续传请求被服务器以 200 整包回应（合法行为）时，旧前缀必须被整包重写。
#[tokio::test]
async fn resume_answered_with_full_200_rewrites_from_zero() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        for index in 0..2 {
            let mut stream = listener.accept().await.expect("accept").0;
            let request = read_request_head(&mut stream).await;
            if index == 0 {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nabc",
                    )
                    .await
                    .expect("first response");
            } else {
                // 服务器对带 If-Range 的请求回 200：可能是版本已变或临时不支持
                // Range；这是合法响应，客户端必须从零重写而不是报错。
                assert!(request.contains("range: bytes=3-"), "{request}");
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"v2\"\r\nConnection: close\r\n\r\nfresh!",
                    )
                    .await
                    .expect("full 200 response");
            }
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    download(
        &asset(format!("http://{address}/plugin"), 6),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect("200 fallback must succeed");
    server.await.expect("server");
    assert_eq!(
        std::fs::read(path).expect("rewritten bytes"),
        b"fresh!",
        "200 answer must replace the stale prefix entirely"
    );
}

/// 未发送 Range 的请求收到意外 206 时必须拒绝：没有 If-Range 保护就不存在
/// 可核对的前缀语义，不能把剩余区间拼到未验证的旧暂存后面。
///
/// 注：本用例只覆盖"未发 Range 却收到 206"的门禁分支；206 内部的
/// Content-Range 总量/区间/ETag 校验由 `resume_206_*` 系列在真正发出
/// Range+If-Range 的前提下逐一覆盖。
#[tokio::test]
async fn unexpected_206_without_range_request_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        while counter.load(Ordering::SeqCst) < 2 {
            let Some(mut stream) = accept_before(&listener, Duration::from_secs(2)).await else {
                break;
            };
            let index = counter.fetch_add(1, Ordering::SeqCst);
            let request = read_request_head(&mut stream).await;
            if index == 0 {
                // 首次截断且无 ETag，客户端下一轮只能整包重取。
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc",
                    )
                    .await
                    .expect("truncated response");
            } else {
                // 客户端没发 Range；服务器违规回 206。Content-Range 故意与“假如有
                // 旧前缀”匹配，用来证明拒绝来自“没发 Range”，而不是范围不匹配。
                assert!(!request.contains("range:"), "{request}");
                stream
                    .write_all(
                        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nConnection: close\r\n\r\ndef",
                    )
                    .await
                    .expect("unexpected 206");
            }
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = download(
        &asset(format!("http://{address}/plugin"), 6),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect_err("206 without a Range request must be rejected");
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    server.await.expect("server");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "rejection must not trigger another retry"
    );
    // 从零重取的请求从未确认旧前缀，暂存只能保留首回合已落盘的字节。
    assert_eq!(std::fs::read(path).expect("stale prefix"), b"abc");
}

/// 已取消请求不应建立网络连接或创建下载文件。
#[tokio::test]
async fn cancelled_download_does_not_create_staging_file() {
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(
        download(
            &asset("http://127.0.0.1:1/plugin".into(), 1),
            &path,
            true,
            &progress,
            &cancellation
        )
        .await
        .is_err()
    );
    assert!(!path.exists());
}

/// 服务器保留半响应不结束，确保取消命中真实流读取等待，并验证连接、重试与落盘均收尾。
#[tokio::test]
async fn cancellation_during_partial_response_closes_connection_and_stops_writes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let mut stream = listener.accept().await.expect("accept").0;
        read_request_head(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"partial\"\r\nConnection: keep-alive\r\n\r\nabc")
            .await
            .expect("partial response");
        stream.flush().await.expect("partial response flush");

        let mut pending = [0u8; 1];
        let closed = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut pending))
            .await
            .expect("cancelled download must close its connection");
        match closed {
            Ok(0) => {}
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            result => panic!("cancelled connection remained active: {result:?}"),
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "cancellation must not start a resumed request"
        );
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, mut receiver) = watch::channel(InstallProgress::Preparing);
    let cancellation = CancellationToken::new();
    let task_path = path.clone();
    let task_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        download(
            &asset(format!("http://{address}/plugin"), 6),
            &task_path,
            true,
            &progress,
            &task_cancellation,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            receiver.changed().await.expect("download progress");
            if matches!(
                *receiver.borrow_and_update(),
                InstallProgress::Downloading {
                    received: 3,
                    total: 6
                }
            ) {
                break;
            }
        }
    })
    .await
    .expect("download must consume the partial body before cancellation");

    cancellation.cancel();
    let error = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("active stream cancellation must return promptly")
        .expect("download task")
        .expect_err("cancelled download");
    assert_eq!(
        error.public_code(),
        PublicErrorCode::Cancelled,
        "cancellation must not be reported as success"
    );
    let bytes_after_cancel = tokio::fs::read(&path)
        .await
        .expect("cancelled partial bytes");
    assert_eq!(bytes_after_cancel, b"abc");
    assert_eq!(
        *receiver.borrow(),
        InstallProgress::Downloading {
            received: 3,
            total: 6
        }
    );
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("server must observe closed connection")
        .expect("server");
    assert_eq!(
        tokio::fs::read(&path).await.expect("stable partial bytes"),
        bytes_after_cancel,
        "no further writes may occur after cancellation"
    );
}

/// 已完整但未 sync 的遗留暂存不发任何网络请求，补一次落盘同步后声明完成；
/// 最终完整性仍由安装器 SHA 门禁核验。取消语义由另一用例单独保证。
#[tokio::test]
async fn complete_staging_syncs_without_any_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let server = tokio::spawn(async move {
        // 任何连接都说明完整暂存路径误发了网络请求。
        if let Some(mut stream) = accept_before(&listener, Duration::from_millis(300)).await {
            let request = read_request_head(&mut stream).await;
            panic!("complete staging must not send requests: {request}");
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    std::fs::write(&path, b"abcdef").expect("complete staging");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    download(
        &asset("http://127.0.0.1:1/plugin".into(), 6),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect("complete staging only needs a local sync");
    server.await.expect("server");
    assert_eq!(std::fs::read(path).expect("unchanged bytes"), b"abcdef");
}

/// 取消优先于“完整暂存直接完成”的捷径：已取消的调用不得声称成功。
#[tokio::test]
async fn cancelled_call_rejects_even_complete_staging() {
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    std::fs::write(&path, b"abcdef").expect("complete staging");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = download(
        &asset("http://127.0.0.1:1/plugin".into(), 6),
        &path,
        true,
        &progress,
        &cancellation,
    )
    .await
    .expect_err("cancellation must win over the early-return shortcut");
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
    // 取消只决定本次调用的结果，不销毁既有的完整暂存。
    assert_eq!(std::fs::read(path).expect("staging untouched"), b"abcdef");
}

/// 已到期的截止时间同样不得借“完整暂存捷径”伪报成功。
#[tokio::test]
async fn expired_deadline_rejects_complete_staging_shortcut() {
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    std::fs::write(&path, b"abcdef").expect("complete staging");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = download_until(
        &asset("http://127.0.0.1:1/plugin".into(), 6),
        &path,
        &progress,
        &CancellationToken::new(),
        // 已经过去的 deadline：捷径不得绕过同一绝对截止时间。
        tokio::time::Instant::now() - Duration::from_millis(1),
    )
    .await
    .expect_err("expired deadline must reject the shortcut");
    assert_eq!(error.public_code(), PublicErrorCode::Timeout);
}

/// 元数据检查后文件被删除：句柄级复核必须失败，且绝不重建零字节文件伪报成功。
///
/// 通过 `sync_verified_complete` 注入点在确定状态下复现竞态窗口的两种结局；
/// 全链路窗口仍由下载器内部顺序执行，无法从外部确定性交错。
#[tokio::test]
async fn complete_staging_deleted_before_open_fails_without_recreating() {
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    // 文件在入口检查后被删除的确定等价状态：打开时已不存在。
    let error = sync_verified_complete(&path, 6)
        .await
        .expect_err("missing staging must fail instead of being recreated");
    assert_eq!(error.public_code(), PublicErrorCode::Infrastructure);
    assert!(
        !path.exists(),
        "sync shortcut must never recreate a deleted staging file"
    );
}

/// 元数据检查后文件被不同大小的文件替换：按句柄证据失败，不沿用旧长度。
#[tokio::test]
async fn complete_staging_replaced_with_wrong_size_is_rejected() {
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    std::fs::write(&path, b"xy").expect("replacement of different size");
    let error = sync_verified_complete(&path, 6)
        .await
        .expect_err("size mismatch between handle and expectation must fail");
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    assert_eq!(
        std::fs::read(path).expect("replacement untouched"),
        b"xy",
        "mismatched replacement must not be synced as a download result"
    );
}

/// 打开校验入口对正确状态的既有暂存必须放行：no-follow 打开常规文件、
/// 长度与期望一致、无别名链接，是续传与完整捷径路径的公共前置。
#[tokio::test]
async fn open_verified_staging_accepts_regular_file_of_expected_length() {
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    std::fs::write(&path, b"abc").expect("existing partial");
    open_verified_staging(&path, 3, StagingCreate::Existing)
        .await
        .expect("regular staging file must pass the verified open");
}

/// 暂存路径是符号链接时，no-follow 打开必须拒绝且绝不写入链接目标。
///
/// Windows 上不带 OPEN_REPARSE_POINT 的按名打开会拿到目标句柄、句柄级
/// metadata 看不到链接；本用例证明打开阶段的 no-follow 标志真正生效。创建
/// 符号链接需要特权（SeCreateSymbolicLink），不可用时跳过而不是误报。
#[cfg(unix)]
#[tokio::test]
async fn open_verified_staging_rejects_symlink_without_touching_target() {
    let root = tempfile::tempdir().expect("temporary root");
    let target = root.path().join("target");
    std::fs::write(&target, b"precious").expect("victim file");
    let link = root.path().join("download");
    std::os::unix::fs::symlink(&target, &link).expect("symlink available on unix");
    let error = open_verified_staging(&link, 0, StagingCreate::Existing)
        .await
        .expect_err("symlinked staging must be rejected by no-follow open");
    assert_eq!(error.public_code(), PublicErrorCode::Infrastructure);
    assert_eq!(
        std::fs::read(&target).expect("target untouched"),
        b"precious",
        "no-follow open must never write through the link"
    );
}

/// Windows 重解析点（junction/符号链接）同样被 no-follow 打开拒绝。
///
/// 创建 junction/symlink 通常需要管理员或开发者模式；探测失败时跳过该
/// 用例，权限可用时证明 OPEN_REPARSE_POINT 打开让链接自身属性可见并被拒。
#[cfg(windows)]
#[tokio::test]
async fn open_verified_staging_rejects_windows_reparse_points() {
    let root = tempfile::tempdir().expect("temporary root");
    let target_dir = root.path().join("reparse-target");
    std::fs::create_dir(&target_dir).expect("target directory");
    let link = root.path().join("download");
    let created = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&target_dir)
        .output()
        .expect("spawn mklink");
    if !created.status.success() {
        // 无创建重解析点权限的环境：如实跳过，不冒充覆盖。
        return;
    }
    let error = open_verified_staging(&link, 0, StagingCreate::Existing)
        .await
        .expect_err("reparse-point staging must be rejected");
    // 重解析点若被 OPEN_REPARSE_POINT 成功打开，句柄属性复核报 Invalid；若
    // Windows 以对有写权限的目录句柄拒绝打开（ERROR_ACCESS_DENIED），则表现为
    // Infrastructure。两种结局都必须在写入前失败，不能用单一错误类别排除平台差异。
    assert!(
        matches!(
            error.public_code(),
            PublicErrorCode::Invalid | PublicErrorCode::Infrastructure
        ),
        "reparse point must be rejected before any write: {error:?}"
    );
}

/// 硬链接别名：另一个名字指向同一 inode 时必须拒绝。
///
/// 别名路径允许外部在本下载器之外继续改写"已验证"的 inode；硬链接创建
/// 无需特权，本用例在默认测试环境即可确定性运行（Windows 与 Unix 同语义）。
#[tokio::test]
async fn open_verified_staging_rejects_hardlink_aliases() {
    let root = tempfile::tempdir().expect("temporary root");
    let original = root.path().join("original");
    std::fs::write(&original, b"abc").expect("original file");
    let alias = root.path().join("alias");
    #[cfg(unix)]
    std::fs::hard_link(&original, &alias).expect("hard link available on unix");
    #[cfg(windows)]
    {
        let linked = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/H"])
            .arg(&alias)
            .arg(&original)
            .output()
            .expect("spawn mklink");
        assert!(linked.status.success(), "hard links need no privilege");
    }
    let error = open_verified_staging(&alias, 3, StagingCreate::Existing)
        .await
        .expect_err("multi-link staging must be rejected");
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    // 原名文件本身未被动过：拒绝发生在打开校验，不写任何字节。
    assert_eq!(std::fs::read(&original).expect("original"), b"abc");
}

/// 入口确认存在、打开时却已消失：必须明确失败，绝不重建暂存。
///
/// 直接调用打开入口并传 `Existing`，在确定状态下复现“检查后消失”的结局：
/// 重建会把下载成果绑定到已被第三方改动的路径，也会把失败伪装成可从零开始
/// 的新会话。全链路窗口由同一次调用的入口观察 + 延迟打开顺序保证，无法从
/// 外部确定性交错，因此这里只判别打开入口在该结局下的确定行为。
#[tokio::test]
async fn existing_staging_vanished_before_open_fails_without_recreating() {
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let error = open_verified_staging(&path, 3, StagingCreate::Existing)
        .await
        .expect_err("staging observed at entry must not be recreated when missing");
    assert_eq!(error.public_code(), PublicErrorCode::Infrastructure);
    assert!(
        !path.exists(),
        "vanished staging must never be recreated by the downloader"
    );
}

/// 入口确认不存在后路径被并发占用：独占创建必须失败且不覆盖对方。
///
/// 覆盖竞态的“文件先出现”结局：`Fresh` 先于任何写入按名创建，若此时路径已被
/// 占用（并发创建、链接替换），`create_new` / `O_EXCL` 返回已存在，下载器据此
/// 明确失败；绝不能回退成按名打开后截断，把未知对象当成自己的暂存。内容保持
/// 原样是断言关键：失败路径没有任何写入时机。
#[tokio::test]
async fn fresh_creation_never_overwrites_concurrently_appeared_file() {
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    std::fs::write(&path, b"attacker").expect("concurrently appeared object");
    let error = open_verified_staging(&path, 0, StagingCreate::Fresh)
        .await
        .expect_err("exclusive creation must reject an existing path");
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    assert_eq!(
        error.public_message(),
        "插件下载暂存被并发创建",
        "error must identify the concurrent creation conflict"
    );
    assert_eq!(
        std::fs::read(&path).expect("object untouched"),
        b"attacker",
        "exclusive creation must never truncate or overwrite the existing object"
    );
}

/// 首次市场安装：入口不存在时以独占创建得到常规暂存文件并完成首次下载。
///
/// 生产安装器的 `stage` 只创建目录，`pkg.kplugin` 在首次下载前并不存在；
/// 旧实现用 `create(false)` 打开，导致所有首次下载在发网络请求前失败。本用例
/// 从空目录开始走完整下载链路，断言请求是整包、字节与进度正确、最终文件是
/// 常规文件，证明 Fresh 模式恢复首次安装路径而不只是打开入口能返回句柄。
#[tokio::test]
async fn missing_staging_supports_first_download_end_to_end() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let mut stream = listener.accept().await.expect("accept").0;
        let request = read_request_head(&mut stream).await;
        assert!(!request.contains("range:"), "{request}");
        assert!(!request.contains("if-range:"), "{request}");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nwhole!")
            .await
            .expect("response");
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    assert!(
        !path.exists(),
        "first market install starts without a staging file"
    );
    let (progress, receiver) = watch::channel(InstallProgress::Preparing);
    download(
        &asset(format!("http://{address}/plugin"), 6),
        &path,
        false,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect("first download must create its own staging file");
    server.await.expect("server");
    assert_eq!(std::fs::read(&path).expect("download bytes"), b"whole!");
    assert!(
        std::fs::symlink_metadata(&path).expect("staging").is_file(),
        "exclusive creation must produce a regular staging file"
    );
    assert_eq!(
        *receiver.borrow(),
        InstallProgress::Downloading {
            received: 6,
            total: 6
        }
    );
}

/// 续传仍走已验证句柄：既有前缀 + 强 ETag 场景在加固后继续成功。
#[tokio::test]
async fn verified_handle_resume_completes_from_existing_prefix() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        for index in 0..2 {
            let mut stream = listener.accept().await.expect("accept").0;
            let request = read_request_head(&mut stream).await;
            if index == 0 {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"hard\"\r\nConnection: close\r\n\r\nabc",
                    )
                    .await
                    .expect("first response");
            } else {
                assert!(request.contains("range: bytes=3-"), "{request}");
                assert!(request.contains("if-range: \"hard\""), "{request}");
                stream
                    .write_all(
                        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nConnection: close\r\n\r\ndef",
                    )
                    .await
                    .expect("second response");
            }
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, receiver) = watch::channel(InstallProgress::Preparing);
    download(
        &asset(format!("http://{address}/plugin"), 6),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect("resume through the verified handle");
    server.await.expect("server");
    assert_eq!(
        std::fs::read(path).expect("download bytes"),
        b"abcdef",
        "verified handle must preserve resume semantics"
    );
    assert_eq!(
        *receiver.borrow(),
        InstallProgress::Downloading {
            received: 6,
            total: 6
        }
    );
}

/// 取消恰在“chunk 已返回、写入未启动”窗口：写入不得启动，前缀原样。
///
/// 生产写循环在每个 chunk 写入前经 `await_sync_then_verify`（flush 排空 +
/// 预检）把关；本用例用门闩受控 future 直接复现该序列的确定时序：先证明
/// flush 已进入等待、取消在写入启动前到达，再放行，断言 Cancelled。真实
/// 生产循环是否调用同一把关由代码结构保证（唯一写入预检点）。
#[tokio::test]
async fn cancellation_between_chunk_and_write_prevents_the_write() {
    use std::task::Poll;
    let (flush_released, flush_gate) = oneshot::channel::<()>();
    let cancellation = CancellationToken::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    // 受控序列 = 生产预检点的实际调用：flush（此处以挂起门闩代表在途写排空）
    // 之后、写入启动之前复核取消/截止。
    let mut check = std::pin::pin!(await_sync_then_verify(
        async {
            let _ = flush_gate.await;
            Ok(())
        },
        &cancellation,
        deadline,
    ));
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        matches!(check.as_mut().poll(&mut context), Poll::Pending),
        "flush must be entered before the cancellation is observed"
    );
    // flush 等待期间取消：后置复核必须改报取消，写入不会启动。
    cancellation.cancel();
    flush_released.send(()).expect("release flush");
    let error = check
        .await
        .expect_err("cancellation before the next write must stop the loop");
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
}

/// chunk 到手但预算已过期：写入不得启动。
///
/// 与取消用例同一预检点，截止时间维度：先证明把关已进入等待且 deadline
/// 未到，再等真实到期、放行 flush，断言 Timeout 而非继续写入。
#[tokio::test]
async fn expired_deadline_between_chunk_and_write_prevents_the_write() {
    use std::task::Poll;
    let (flush_released, flush_gate) = oneshot::channel::<()>();
    let cancellation = CancellationToken::new();
    let deadline = tokio::time::Instant::now() + INJECTED_DEADLINE_SLACK;
    let mut check = std::pin::pin!(await_sync_then_verify(
        async {
            let _ = flush_gate.await;
            Ok(())
        },
        &cancellation,
        deadline,
    ));
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        matches!(check.as_mut().poll(&mut context), Poll::Pending),
        "flush must be entered before the deadline expires"
    );
    assert!(
        tokio::time::Instant::now() < deadline,
        "deadline must still be in the future when flush starts waiting"
    );
    tokio::time::sleep_until(deadline).await;
    flush_released.send(()).expect("release flush");
    let error = check
        .await
        .expect_err("expired budget must stop before the next write");
    assert_eq!(error.public_code(), PublicErrorCode::Timeout);
}

/// 全链路：chunk 间取消不得启动后续写入，已写前缀保留且在途写已排空。
///
/// 服务器先发 3 字节（触发一次真实 write_all，write_all 返回时 OS 写可能在
/// 途），随后等客户端取消再确认连接收尾；客户端在等待第二个 chunk 的
/// `chunk()` 挂起期间被取消。取消后严格断言文件就是前缀 `abc`：进度到 3/6
/// 只证明 `write_all` 返回，不证明落盘；若提前返回路径缺少排空（drop 句柄
/// 不等待在途阻塞任务），本断言会与后台写任务竞争且在慢盘上观察到空文件或
/// 缺失前缀。诚实边界：测试无法控制“write_all 已返回但 OS 写仍在途”的精确
/// 时刻（真实磁盘 IO 不可注入），排空是否生效由代码结构（唯一出口经
/// `drain_inflight`）与该强断言共同约束，存在理论上仍观察不到的窗口；OS 写
/// 一旦启动不能撤回，测试只断言可观察状态。
#[tokio::test]
async fn cancellation_between_chunks_writes_no_further_bytes_and_drains_inflight() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let mut stream = listener.accept().await.expect("accept").0;
        read_request_head(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc")
            .await
            .expect("partial body");
        stream.flush().await.expect("flush");
        // 客户端取消后必须关闭连接：读到 EOF（Ok(0)）或重置都算收尾。
        let mut pending = [0u8; 1];
        let closed = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut pending)).await;
        match closed {
            Ok(Ok(0)) | Ok(Err(_)) => {}
            other => panic!("cancelled download kept the connection open: {other:?}"),
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, mut receiver) = watch::channel(InstallProgress::Preparing);
    let cancellation = CancellationToken::new();
    let task = tokio::spawn({
        let cancellation = cancellation.clone();
        let path = path.clone();
        async move {
            download(
                &asset(format!("http://{address}/plugin"), 6),
                &path,
                true,
                &progress,
                &cancellation,
            )
            .await
        }
    });
    // 门闩语义：进度到 3/6 说明第一段已 write_all 返回；此时取消。
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            receiver.changed().await.expect("download progress");
            if matches!(
                *receiver.borrow_and_update(),
                InstallProgress::Downloading {
                    received: 3,
                    total: 6
                }
            ) {
                break;
            }
        }
    })
    .await
    .expect("first chunk must be written before cancellation");
    cancellation.cancel();
    let error = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("cancellation must resolve promptly")
        .expect("download task")
        .expect_err("cancelled download must not report success");
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
    server.await.expect("server");
    // 严格断言：write_all 已成功返回、且提前返回路径排空在途写之后，磁盘上
    // 必须是确定的前缀 abc。若排空缺失，drop 句柄会让在途阻塞任务带着
    // Arc<File> 竞争完成，慢环境下可观察到空文件或部分字节。
    assert_eq!(
        std::fs::read(&path).expect("staging readable after cancel"),
        b"abc",
        "the written prefix must be fully observable after cancellation"
    );
}

/// 短 body 正常 EOF 且长度不足：EOF 出口必须先排空在途写，再按长度不匹配拒绝。
///
/// 服务器用 `Transfer-Encoding: chunked` 发送 3 字节后以干净的 0 长度终止块
/// 结束（真实 `Ok(None)` EOF）：对带 Content-Length 的响应，提前关闭在
/// hyper 中是 `IncompleteBody` 传输错误（已核对 hyper-1.12.0 decode.rs），
/// 不是正常 EOF，因此必须用 chunked 干净终止来产生 `chunk() == Ok(None)` 的
/// 确定路径。EOF 时 offset=3 < response_end=6：若 EOF 出口直接 break 返回而
/// 不 flush，drop 句柄会让最后一次 write_all 的在途任务在后台静默完成、错误
/// 丢失，返回“长度不匹配”时磁盘状态未知。本用例在错误返回后立即读文件，
/// 严格断言 3 字节全部可见（排空生效）；错误为长度不匹配，证明走到了长度
/// 判定而不是写盘或传输错误。
#[tokio::test]
async fn short_body_eof_length_mismatch_drains_inflight_before_returning() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let mut stream = listener.accept().await.expect("accept").0;
        read_request_head(&mut stream).await;
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
            )
            .await
            .expect("short chunked body with clean terminator");
        stream.flush().await.expect("flush");
        // 终止块之后 drop：chunk() 在干净帧结束处返回 Ok(None)。
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = download(
        &asset(format!("http://{address}/plugin"), 6),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect_err("short body must not be accepted as complete");
    assert_eq!(
        error.public_message(),
        "插件下载响应长度不匹配",
        "clean EOF with fewer bytes must fail at the length gate after draining"
    );
    server.await.expect("server");
    // 排空生效的直接证据：错误返回后读到的就是已写满的 3 字节，而不是与
    // 后台写任务竞争的不确定状态。
    assert_eq!(
        std::fs::read(&path).expect("drained prefix"),
        b"abc",
        "every written byte must be observable once the error path drained inflight writes"
    );
}

/// 合法 206 分段收尾后下一轮立即失败：句柄放回槽位前必须已排空在途写。
///
/// 三轮构造，全部走真实生产路径：
/// 1. 200 带 Content-Length: 6 的前缀 "abc" 后异常截断（无终止块）——在
///    hyper 中是 IncompleteBody 传输错误，可重试分支记录 last_error 并
///    保留前缀，第一轮的写入经排空后落盘可知；
/// 2. 续传请求（Range+If-Range）回合法 206 `bytes 3-4/6`，只补 2 字节，
///    本轮 offset(3) == response_end(5) 且未满目录大小，正常进入下一轮，
///    句柄带已排空状态放回槽位；
/// 3. 续传请求收到 404，触发不可重试的“插件续传响应无效”，立即失败。
/// 确定断言：失败返回后前缀 abcde 全部可见。404 而非连接重置：重置在头部
/// 阶段可能被 reqwest 归类为可重试传输错误进入第四轮，时序不确定。所有
/// 连接与 join 带有限超时，避免服务器缺陷卡死测试。
#[tokio::test]
async fn resume_round_with_immediate_failure_keeps_prefix_observable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        // 第一轮：声明 6 字节、只发 3 字节后关闭 → IncompleteBody 可重试。
        let mut first = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("first accept in time")
            .expect("first connection")
            .0;
        read_request_head(&mut first).await;
        first
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"keep\"\r\nConnection: close\r\n\r\nabc")
            .await
            .expect("truncated first response");
        first.flush().await.expect("first flush");
        drop(first);
        // 第二轮：合法 206 只补 2 字节（3..5），干净收尾。
        let mut second = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("second accept in time")
            .expect("second connection")
            .0;
        let request = read_request_head(&mut second).await;
        assert!(request.contains("range: bytes=3-"), "{request}");
        assert!(request.contains("if-range: \"keep\""), "{request}");
        second
            .write_all(
                b"HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nContent-Range: bytes 3-4/6\r\nConnection: close\r\n\r\nde",
            )
            .await
            .expect("partial 206 response");
        second.flush().await.expect("second flush");
        drop(second);
        // 第三轮：续传请求直接 404，不可重试，立即失败。
        let mut third = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("third accept in time")
            .expect("third connection")
            .0;
        let request = read_request_head(&mut third).await;
        assert!(request.contains("range: bytes=5-"), "{request}");
        assert!(request.contains("if-range: \"keep\""), "{request}");
        third
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .expect("rejecting third response");
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        download(
            &asset(format!("http://{address}/plugin"), 6),
            &path,
            true,
            &progress,
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("download must not hang")
    .expect_err("rejected resume round must fail");
    assert_eq!(
        error.public_message(),
        "插件续传响应无效",
        "404 on a resume request must fail immediately without retry"
    );
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("server must finish in time")
        .expect("server task");
    assert_eq!(
        std::fs::read(&path).expect("drained prefix"),
        b"abcde",
        "all writes from earlier rounds must be drained and observable before the failure returns"
    );
}

/// 受控 AsyncWrite 桩：`poll_flush` 挂在门闩上，直接驱动生产 `drain_inflight`。
///
/// 生产入口已泛化为 `AsyncWrite + Unpin`，因此测试可以传桩而不再经由另一层
/// helper：本桩的 flush 只在收到释放信号后才 Ready（成功路径），或在首次
/// poll 即返回预设错误（失败路径）。它直接证明排空契约本身——先无条件等待
/// 已启动 IO，再复核取消/截止。
struct GatedFlush {
    gate: oneshot::Receiver<()>,
    /// 预设的 flush 错误：Some 时首次 poll 即返回（失败路径不依赖门闩）。
    flush_error: Option<std::io::Error>,
    /// 首次 poll 标记：证明取消/过期没有把 flush 跳过。用共享原子而非普通
    /// 字段：`pin!(drain_inflight(&mut stub, ..))` 在整个 future 生命周期内
    /// 独占 `stub`，外部直接读 `stub.polled` 会触发 E0503（借用冲突）；原子
    /// 让测试与桩各自持有一份句柄，互不借用。
    polled: Arc<AtomicBool>,
}

impl tokio::io::AsyncWrite for GatedFlush {
    /// 排空路径不写入，只观察 flush；误用立即 panic，防止静默假绿。
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        panic!("GatedFlush must never be written to");
    }

    /// 门闩 flush：首次 poll 记录已进入等待；预设错误存在时立即返回该错误
    /// （失败优先契约的注入点），否则等释放信号。锁定 Tokio 1.53.1 的
    /// `oneshot::error::TryRecvError` 只有 `Empty`（尚未发送）与 `Closed`
    /// （发送端已丢弃）两个变体；后者按完成收尾，避免测试自身挂死。
    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        this.polled.store(true, Ordering::SeqCst);
        if let Some(error) = this.flush_error.take() {
            return std::task::Poll::Ready(Err(error));
        }
        match this.gate.try_recv() {
            Ok(()) => std::task::Poll::Ready(Ok(())),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                std::task::Poll::Ready(Ok(()))
            }
        }
    }

    /// 关闭复用 flush 门闩：生产 `drain_inflight` 只调用 flush，从不 shutdown，
    /// 因此这里让 shutdown 委托给同一门闩，避免为测试桩引入第二套“完成”语义
    /// 而掩盖排空契约；若将来生产改为 shutdown，本桩的行为仍与 flush 一致。
    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.poll_flush(cx)
    }
}

/// 已取消后调用生产排空入口：仍必须等待未完成的 flush，再按取消报告。
///
/// 直接驱动生产 `drain_inflight`（AsyncWrite 泛型注入受控桩）：桩已取消的
/// token 下仍被 poll 到 Pending 并等待门闩释放，之后入口先接受排空结果
/// （成功）再复核取消，返回 Cancelled。若将来在外层重新加入前置取消检查，
/// 桩的 flush 不会被 poll（`polled == false`），`Poll::Pending` 断言失败，
/// 回归被本用例直接捕获。
#[tokio::test]
async fn drain_inflight_with_cancelled_token_still_awaits_flush_before_reporting() {
    use std::task::Poll;
    let (flush_released, gate) = oneshot::channel::<()>();
    let polled = Arc::new(AtomicBool::new(false));
    let mut stub = GatedFlush {
        gate,
        flush_error: None,
        polled: Arc::clone(&polled),
    };
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut check = std::pin::pin!(drain_inflight(&mut stub, &cancellation, deadline));
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        matches!(check.as_mut().poll(&mut context), Poll::Pending),
        "cancelled token must not skip the flush wait"
    );
    assert!(
        polled.load(Ordering::SeqCst),
        "flush must have been polled despite cancellation"
    );
    flush_released.send(()).expect("release controlled flush");
    let error = check
        .await
        .expect_err("drain must report the cancellation after the flush settled");
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
}

/// 调用时预算已过期，排空仍必须先行：先 flush 等待，再报 Timeout。
///
/// deadline 在**首次 poll 之前**就构造为已过期（`now - 1ms`），并在 poll 前
/// 断言这一前提：若生产入口在外层先检查 timeout 再 flush，本用例会直接返回
/// Timeout、flush 永不被 poll，`Poll::Pending` 与 `polled` 断言同时失败。
/// 门闩不释放时 future 保持 Pending（证明已进入真实 flush 等待，而非同步
/// 立刻返回）；随后释放门闩，排空结果先被接受，再报 Timeout。无需任何固定
/// 睡眠，时序由门闩而非时钟控制。
#[tokio::test]
async fn drain_inflight_with_expired_deadline_still_awaits_flush_before_reporting() {
    use std::task::Poll;
    let (flush_released, gate) = oneshot::channel::<()>();
    let polled = Arc::new(AtomicBool::new(false));
    let mut stub = GatedFlush {
        gate,
        flush_error: None,
        polled: Arc::clone(&polled),
    };
    let cancellation = CancellationToken::new();
    // 调用时已过期：首次 poll 前就成立，而不是等待期间才到期。
    let deadline = tokio::time::Instant::now() - Duration::from_millis(1);
    assert!(
        tokio::time::Instant::now() >= deadline,
        "deadline must already be expired before the first poll"
    );
    let mut check = std::pin::pin!(drain_inflight(&mut stub, &cancellation, deadline));
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        matches!(check.as_mut().poll(&mut context), Poll::Pending),
        "an already-expired deadline must not skip the flush wait"
    );
    assert!(
        polled.load(Ordering::SeqCst),
        "flush must have been polled even though the deadline had expired"
    );
    flush_released.send(()).expect("release controlled flush");
    let error = check
        .await
        .expect_err("drain must report the timeout after the flush settled");
    assert_eq!(error.public_code(), PublicErrorCode::Timeout);
}

/// flush 自身失败时写盘错误优先于取消/截止：坏盘不能被伪报成用户取消。
///
/// 桩的 flush 注入真实 io::Error；调用发生在已取消、已过期的最坏组合下，
/// 排空结果仍必须先被接受并以 Infrastructure 上浮。
#[tokio::test]
async fn drain_inflight_reports_flush_failure_over_cancellation() {
    let (flush_released, gate) = oneshot::channel::<()>();
    let mut stub = GatedFlush {
        gate,
        flush_error: Some(std::io::Error::other("simulated flush failure")),
        polled: Arc::new(AtomicBool::new(false)),
    };
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = drain_inflight(&mut stub, &cancellation, tokio::time::Instant::now())
        .await
        .expect_err("flush failure must surface even when cancelled");
    assert_eq!(
        error.public_code(),
        PublicErrorCode::Infrastructure,
        "real write failure must outrank cancellation"
    );
    // 失败路径在首次 poll 即返回：门闩从未被消费，发送端仍持有。
    assert!(
        flush_released.send(()).is_ok(),
        "the flush error path must not wait for the gate release"
    );
}

/// 生产收尾路径确实经过后置复核：sync 挂起期间到达的取消不得伪报成功。
///
/// 用受控 future 构成确定性时序：把 `await_sync_then_verify` pin 在本地后先
/// 轮询到 Pending（证明把关入口已真正等待 sync 且尚未完成），再取消 token，
/// 然后释放 sync 让它正常完成——若收尾缺少取消复核，这一步会错误地返回 Ok。
/// 测试辅助的受控 future 不执行真实磁盘 IO；两条生产分支是否真实调用同一把关
/// 函数由代码结构保证（两处收尾都调用 `await_sync_then_verify`），删除该把关
/// 时本用例会直接失败。
#[tokio::test]
async fn cancellation_arriving_during_sync_is_reported_after_completion() {
    use std::task::Poll;
    let (release, gate) = oneshot::channel::<()>();
    // pin! 的 future 内部借用 token，token 必须绑定到变量，生命周期覆盖整个 await。
    let cancellation = CancellationToken::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut check = std::pin::pin!(await_sync_then_verify(
        async {
            // 受控 sync：收到释放信号前一直挂起，模拟不可取消的磁盘调用。
            let _ = gate.await;
            Ok(())
        },
        &cancellation,
        deadline,
    ));
    // 先驱动到 Pending：复核入口确实在等待 sync，此刻取消发生在完成之前。
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        matches!(check.as_mut().poll(&mut context), Poll::Pending),
        "controlled sync must still be pending before cancellation"
    );
    cancellation.cancel();
    release.send(()).expect("release controlled sync");
    let error = check
        .await
        .expect_err("cancellation during sync must not report success");
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
}

/// 受控 sync 等待期间 deadline 到期：复核必须在 sync 返回后报告超时。
///
/// 截止点设在未来、经断言确认未到期后才进入等待，再用真实时钟 sleep 到期并
/// 释放受控 sync——时序“Pending → deadline 到期 → sync 完成”由此完全确定；
/// 若收尾缺少截止复核，sync 成功会被错误地当成下载成功。
#[tokio::test]
async fn deadline_expiring_during_sync_is_reported_after_completion() {
    use std::task::Poll;
    let (release, gate) = oneshot::channel::<()>();
    let cancellation = CancellationToken::new();
    let deadline = tokio::time::Instant::now() + INJECTED_DEADLINE_SLACK;
    let mut check = std::pin::pin!(await_sync_then_verify(
        async {
            let _ = gate.await;
            Ok(())
        },
        &cancellation,
        deadline,
    ));
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        matches!(check.as_mut().poll(&mut context), Poll::Pending),
        "controlled sync must still be pending before the deadline"
    );
    assert!(
        tokio::time::Instant::now() < deadline,
        "deadline must still be in the future when sync starts waiting"
    );
    // 等到 deadline 真实到期后再释放 sync：过期发生在 sync 等待期间。
    tokio::time::sleep_until(deadline).await;
    release.send(()).expect("release controlled sync");
    let error = check
        .await
        .expect_err("deadline expiring during sync must not report success");
    assert_eq!(error.public_code(), PublicErrorCode::Timeout);
}

/// sync 本身失败时错误原样透传，复核不吞也不改写失败原因。
#[tokio::test]
async fn sync_failure_is_passed_through_before_any_success() {
    let error = await_sync_then_verify(
        async { Err(kerminal_application::AppError::infrastructure("同步失败")) },
        &CancellationToken::new(),
        tokio::time::Instant::now() + Duration::from_secs(60),
    )
    .await
    .expect_err("sync failure must surface as-is");
    assert_eq!(error.public_code(), PublicErrorCode::Infrastructure);
}

/// 正常同步且无取消、截止未到时才允许成功；取消与截止同时成立时取消优先。
#[tokio::test]
async fn successful_sync_with_live_token_and_future_deadline_succeeds() {
    await_sync_then_verify(
        async { Ok(()) },
        &CancellationToken::new(),
        tokio::time::Instant::now() + Duration::from_secs(60),
    )
    .await
    .expect("live token and future deadline pass");
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        await_sync_then_verify(async { Ok(()) }, &cancelled, tokio::time::Instant::now())
            .await
            .expect_err("cancelled plus expired must still report cancellation")
            .public_code(),
        PublicErrorCode::Cancelled
    );
}

/// 取消先于副作用：首次路径的独占创建必须在启动前被拦截。
///
/// 预检在 `CREATE_NEW` 之前；令牌已取消时打开 future 不会被轮询，路径保持
/// 不存在。删除预检会让空暂存在取消后仍被创建，本用例即回归该缺陷。
#[tokio::test]
async fn cancelled_staging_creation_never_starts() {
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = guard_staging_side_effect(
        &cancellation,
        tokio::time::Instant::now() + Duration::from_secs(5),
        open_verified_staging(&path, 0, StagingCreate::Fresh),
    )
    .await
    .expect_err("pre-cancelled creation must not start");
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
    assert!(
        !path.exists(),
        "creation must not start once cancellation was already observed"
    );
}

/// 取消先于副作用：既有前缀不得在收到取消后仍被截断。
///
/// 打开验证通过后直接调用带守卫的 `set_len`：令牌已取消时预检拦在截断前，
/// 文件内容必须原样保留。删除守卫会让 body 读取处的取消晚于一次真实截断。
/// 断言分两步：先用**句柄自身**确认长度未变，再显式 drop 句柄后按路径读
/// 完整前缀——`share_mode(0)` 的独占句柄存活时 Windows 按名读取会得到
/// ERROR_SHARING_VIOLATION，测试不能把这一借用冲突误判成生产截断。
#[tokio::test]
async fn cancelled_staging_truncation_never_starts() {
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    std::fs::write(&path, b"abc").expect("existing prefix");
    let file = open_verified_staging(&path, 3, StagingCreate::Existing)
        .await
        .expect("verified existing staging");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = guard_staging_side_effect(
        &cancellation,
        tokio::time::Instant::now() + Duration::from_secs(5),
        async {
            file.set_len(0)
                .await
                .map_err(|e| AppError::infrastructure_with_source("重置插件下载暂存失败", e))
        },
    )
    .await
    .expect_err("pre-cancelled truncation must not start");
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
    // metadata 走句柄信息查询而非按名打开，独占共享模式下仍可读。
    assert_eq!(
        file.metadata().await.expect("prefix via handle").len(),
        3,
        "cancelled truncation must leave the prefix length untouched"
    );
    drop(file);
    assert_eq!(
        std::fs::read(&path).expect("prefix bytes"),
        b"abc",
        "an existing prefix must survive cancellation observed before truncation"
    );
}

/// 截止时间已过：首次路径的创建同样不得启动。
#[tokio::test]
async fn expired_deadline_blocks_staging_creation_before_it_starts() {
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let cancellation = CancellationToken::new();
    let error = guard_staging_side_effect(
        &cancellation,
        tokio::time::Instant::now() - Duration::from_millis(1),
        open_verified_staging(&path, 0, StagingCreate::Fresh),
    )
    .await
    .expect_err("expired budget must not start creation");
    assert_eq!(error.public_code(), PublicErrorCode::Timeout);
    assert!(
        !path.exists(),
        "expired deadline must be reported before any staging side effect"
    );
}

/// 取消在等待创建期间到达：按取消报告，但不伪装成“没有创建过”。
///
/// oneshot 门闩把时序固定为“operation 已进入等待 → 取消 → 放行真实打开”：
/// 后置复核必须把原本成功的打开改报为 Cancelled；同时断言空暂存确实存在，
/// 诚实呈现 `CREATE_NEW` 这个 OS 副作用已启动、无法原子撤回的边界。实现不得
/// 按名删除或丢弃仍在运行的 future 来伪装回收。
#[tokio::test]
async fn cancellation_during_staging_creation_is_reported_after_side_effect() {
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let cancellation = CancellationToken::new();
    let (release, gate) = oneshot::channel::<()>();
    let (started_sender, started) = oneshot::channel::<()>();
    let guarded = tokio::spawn({
        let cancellation = cancellation.clone();
        let path = path.clone();
        async move {
            let operation = async move {
                let _ = started_sender.send(());
                let _ = gate.await;
                open_verified_staging(&path, 0, StagingCreate::Fresh).await
            };
            guard_staging_side_effect(
                &cancellation,
                tokio::time::Instant::now() + Duration::from_secs(5),
                operation,
            )
            .await
        }
    });
    started.await.expect("operation must reach its await point");
    cancellation.cancel();
    release.send(()).expect("release creation");
    let error = guarded
        .await
        .expect("join guarded creation")
        .expect_err("post-await check must report the cancellation");
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
    assert!(
        path.exists(),
        "the OS creation already ran; it must not be reported as never having happened"
    );
    assert_eq!(
        std::fs::metadata(&path).expect("created staging").len(),
        0,
        "no bytes may be written into staging after cancellation"
    );
}

/// 取消在等待截断期间到达：按取消报告，已执行的前缀截断如实保留。
///
/// 与创建用例对称：`set_len(0)` 在取消之后才放行，后置复核改报 Cancelled；
/// 文件已被截为空是 OS 副作用，不能原子恢复，也不应按名重建旧前缀。
/// 时序全部由门闩与一次确定 poll 控制（无 sleep）：Pending 证明放行前截断
/// 未启动；future 用拥有型 `Pin<Box<_>>` 持有，await 完成时随 future 一起
/// drop 内部独占句柄，之后才按路径读取——`pin!` 的临时值会活到函数末尾，
/// 在 `share_mode(0)` 下会让 Windows 按名读取得到 ERROR_SHARING_VIOLATION。
#[tokio::test]
async fn cancellation_during_staging_truncation_is_reported_after_side_effect() {
    use std::task::Poll;
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    std::fs::write(&path, b"abc").expect("existing prefix");
    let file = open_verified_staging(&path, 3, StagingCreate::Existing)
        .await
        .expect("verified existing staging");
    let cancellation = CancellationToken::new();
    // 本用例只验证取消维度；给足预算，避免慢机器上被超时分支抢先（无 sleep，
    // 正常路径不会接近该预算）。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let (release, gate) = oneshot::channel::<()>();
    let (started_sender, started) = oneshot::channel::<()>();
    let guarded_cancellation = cancellation.clone();
    // 拥有型 future：内部 `file` 的所有权随 future 一起释放，await 结束即
    // 关闭独占句柄；不用 `pin!` 以免临时值被延长到函数末尾。
    let mut guarded = Box::pin(async move {
        let operation = async move {
            let _ = started_sender.send(());
            let _ = gate.await;
            file.set_len(0)
                .await
                .map_err(|e| AppError::infrastructure_with_source("重置插件下载暂存失败", e))
        };
        guard_staging_side_effect(&guarded_cancellation, deadline, operation).await
    });
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        matches!(guarded.as_mut().poll(&mut context), Poll::Pending),
        "truncation must be parked at the gate before it is released"
    );
    started.await.expect("operation must reach its await point");
    // 门闩尚未释放：上面的 Pending 已证明 `set_len` 未启动；入口长度 3 由
    // `open_verified_staging` 的 expected_offset 在打开时复核过，这里不再
    // 按名读取（独占句柄仍被 future 持有）。
    cancellation.cancel();
    release.send(()).expect("release truncation");
    let error = guarded
        .await
        .expect_err("post-await check must report the cancellation");
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
    // await 消费了拥有型 future，其内部独占句柄已随之 drop；此时按路径读取
    // 不会与测试自身的句柄冲突，截断这一已发生的副作用如实保留。
    assert_eq!(
        std::fs::read(&path).expect("truncated staging").len(),
        0,
        "the OS truncation already ran and must be reported as such"
    );
}

/// 截止时间在等待截断期间到期：按超时报告，截断已发生时不回滚。
#[tokio::test]
async fn expired_deadline_during_staging_truncation_is_reported_after_side_effect() {
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    std::fs::write(&path, b"abc").expect("existing prefix");
    let file = open_verified_staging(&path, 3, StagingCreate::Existing)
        .await
        .expect("verified existing staging");
    let cancellation = CancellationToken::new();
    let deadline = tokio::time::Instant::now() + INJECTED_DEADLINE_SLACK;
    let (release, gate) = oneshot::channel::<()>();
    let (started_sender, started) = oneshot::channel::<()>();
    let guarded = tokio::spawn({
        let cancellation = cancellation.clone();
        async move {
            let operation = async move {
                let _ = started_sender.send(());
                let _ = gate.await;
                file.set_len(0)
                    .await
                    .map_err(|e| AppError::infrastructure_with_source("重置插件下载暂存失败", e))
            };
            guard_staging_side_effect(&cancellation, deadline, operation).await
        }
    });
    started.await.expect("operation must reach its await point");
    assert!(
        tokio::time::Instant::now() < deadline,
        "deadline must still be in the future when truncation starts waiting"
    );
    // 让截止时间在截断等待期间真实到期，再放行真实的 OS 截断。
    tokio::time::sleep_until(deadline).await;
    release.send(()).expect("release truncation");
    let error = guarded
        .await
        .expect("join guarded truncation")
        .expect_err("expired budget must be reported after the truncation ran");
    assert_eq!(error.public_code(), PublicErrorCode::Timeout);
    assert_eq!(
        std::fs::read(&path).expect("truncated staging").len(),
        0,
        "truncation already ran; timeout must not pretend nothing happened"
    );
}

/// 恶意服务器不能把范围声明接不到请求前缀之后：真正发出 Range 后，总量与
/// 起点任一不符都必须拒绝。
#[tokio::test]
async fn mismatched_content_range_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut stream = listener.accept().await.unwrap().0;
        read_request_head(&mut stream).await;
        stream
            .write_all(
                b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 2-4/6\r\nConnection: close\r\n\r\ndef",
            )
            .await
            .unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("download");
    std::fs::write(&path, b"abc").unwrap();
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    assert!(
        download(
            &asset(format!("http://{address}/plugin"), 6),
            &path,
            true,
            &progress,
            &CancellationToken::new()
        )
        .await
        .is_err()
    );
    server.await.unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"abc");
}

/// 合法 206 不必回显 ETag：确实缺失时只要范围校验通过就允许续传。
///
/// 截断 200 建立强 ETag 锚点，第二次真正发出 Range+If-Range 后服务器回 206
/// 且不带任何 ETag 头——旧实现把缺失 ETag 当不匹配错误终止；最终完整性仍由
/// 安装器 SHA/size 门禁保证，本用例只证明传输层接受这种合法形态。
#[tokio::test]
async fn resume_206_without_echoed_etag_succeeds() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        for index in 0..2 {
            let mut stream = listener.accept().await.expect("accept").0;
            let request = read_request_head(&mut stream).await;
            if index == 0 {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"keep\"\r\nConnection: close\r\n\r\nabc",
                    )
                    .await
                    .expect("first response");
            } else {
                // 确认续传锚点确实发出，且 206 不回显任何 ETag。
                assert!(request.contains("range: bytes=3-"), "{request}");
                assert!(request.contains("if-range: \"keep\""), "{request}");
                stream
                    .write_all(
                        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nConnection: close\r\n\r\ndef",
                    )
                    .await
                    .expect("second response without etag");
            }
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    download(
        &asset(format!("http://{address}/plugin"), 6),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect("206 without echoed etag is legal");
    server.await.expect("server");
    assert_eq!(
        std::fs::read(path).expect("download bytes"),
        b"abcdef",
        "resume must complete when the range check passes without an etag"
    );
}

/// 206 回显了弱 ETag：有头但不是强值，说明状态可疑，必须保守拒绝。
#[tokio::test]
async fn resume_206_with_weak_echoed_etag_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        for index in 0..2 {
            let mut stream = listener.accept().await.expect("accept").0;
            let request = read_request_head(&mut stream).await;
            if index == 0 {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nabc",
                    )
                    .await
                    .expect("first response");
            } else {
                assert!(request.contains("if-range: \"v1\""), "{request}");
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nETag: W/\"v1\"\r\nConnection: close\r\n\r\ndef",
                    )
                    .await;
            }
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = download(
        &asset(format!("http://{address}/plugin"), 6),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect_err("weak echoed etag must not pass as missing");
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    server.await.expect("server");
    assert_eq!(
        std::fs::read(path).expect("prefix untouched"),
        b"abc",
        "suspicious echo must not extend the staging prefix"
    );
}

/// 206 回显畸形或多个 ETag 值：解析失败说明响应状态可疑，不能当缺失放行。
#[tokio::test]
async fn resume_206_with_malformed_or_multiple_etags_is_rejected() {
    // 同一参数化框架跑两种可疑回显：非法引号形态、重复 ETag 头。
    for (label, echo) in [
        ("malformed", "ETag: v1\r\n"),
        ("multiple", "ETag: \"v1\"\r\nETag: \"v1\"\r\n"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let address = listener.local_addr().expect("address");
        let echo = echo.to_owned();
        let server = tokio::spawn(async move {
            for index in 0..2 {
                let mut stream = listener.accept().await.expect("accept").0;
                let request = read_request_head(&mut stream).await;
                if index == 0 {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nabc",
                        )
                        .await
                        .expect("first response");
                } else {
                    assert!(request.contains("if-range: \"v1\""), "{request}");
                    let response = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\n{echo}Connection: close\r\n\r\ndef"
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                }
            }
        });
        let root = tempfile::tempdir().expect("temporary root");
        let path = root.path().join("download");
        std::fs::write(&path, b"abc").expect("seed partial");
        let (progress, _) = watch::channel(InstallProgress::Preparing);
        // 直接从有偏移、有强 ETag 锚点的状态起步需要完整两步握手；这里复用
        // 截断建立锚点的同构流程，让第二次请求真正携带 Range+If-Range。
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            download(
                &asset(format!("http://{address}/plugin"), 6),
                &path,
                true,
                &progress,
                &CancellationToken::new(),
            ),
        )
        .await
        .expect("download must not hang")
        .expect_err(&format!(
            "{label} echoed etag must be rejected, not treated as missing"
        ));
        assert_eq!(error.public_code(), PublicErrorCode::Invalid);
        server.await.expect("server");
        // 旧前缀不能被可疑响应拼接；首轮截断字节保留。
        let prefix = std::fs::read(&path).expect("staging bytes");
        assert!(
            prefix.starts_with(b"abc") && prefix.len() <= 6,
            "{label}: suspicious echo must not extend staging: {prefix:?}"
        );
    }
}

/// 206 回显了不同的强 ETag：版本已变，拒绝与既有用例同一判别。
#[tokio::test]
async fn resume_206_with_explicitly_different_etag_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        for index in 0..2 {
            let mut stream = listener.accept().await.expect("accept").0;
            let request = read_request_head(&mut stream).await;
            if index == 0 {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nabc",
                    )
                    .await
                    .expect("first response");
            } else {
                assert!(request.contains("if-range: \"v1\""), "{request}");
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nETag: \"v2\"\r\nConnection: close\r\n\r\ndef",
                    )
                    .await;
            }
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = download(
        &asset(format!("http://{address}/plugin"), 6),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect_err("different strong etag must be rejected");
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    server.await.expect("server");
    assert_eq!(
        std::fs::read(path).expect("prefix untouched"),
        b"abc",
        "mismatched echo must not extend the staging prefix"
    );
}

/// 真正发出 Range 后，206 的区间起点与请求前缀不衔接必须拒绝。
///
/// 样本 `bytes 1-5/6` 的 start 与请求 `bytes=3-` 不符，在前缀匹配处失败；
/// end=5 并不小于 offset=3，本用例只覆盖起点不衔接分支，终点校验由
/// `resume_206_with_*` 其余用例分别命中。
#[tokio::test]
async fn resume_206_with_mismatched_range_start_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        while counter.load(Ordering::SeqCst) < 2 {
            let Some(mut stream) = accept_before(&listener, Duration::from_secs(2)).await else {
                break;
            };
            let index = counter.fetch_add(1, Ordering::SeqCst);
            let request = read_request_head(&mut stream).await;
            if index == 0 {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nabc",
                    )
                    .await
                    .expect("first response");
            } else {
                assert!(request.contains("range: bytes=3-"), "{request}");
                // 请求从字节 3 续传，响应却声称从字节 1 开始：起点不衔接。
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 5\r\nContent-Range: bytes 1-5/6\r\nConnection: close\r\n\r\ndef",
                    )
                    .await;
            }
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = download(
        &asset(format!("http://{address}/plugin"), 6),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect_err("range start disconnected from the confirmed prefix must be rejected");
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    // public_message 区分拒绝来源：起点不衔接在前缀匹配处失败，报"范围无效"；
    // 后续响应长度校验（offset != response_end）也会报 Invalid，但文案是
    // "响应长度不匹配"，因此固定文案能把头部范围校验与其余 Invalid 路径区分开。
    assert_eq!(error.public_message(), "插件续传范围无效");
    server.await.expect("server");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "header-level rejection must not trigger further requests; the count is a \
         sanity signal, the distinguishing proof is the message above"
    );
    assert_eq!(
        std::fs::read(path).expect("prefix untouched"),
        b"abc",
        "disconnected range must not be appended"
    );
}

/// 真正发出 Range 后，206 区间终点低于已确认前缀必须拒绝。
///
/// 样本 `bytes 3-2/6`：start 与请求衔接，但 end=2 < offset=3，命中
/// `end < offset` 校验；total=6 与目录一致，不会先被总量检查抢先拒绝。
/// 请求次数断言把头部范围校验的即时拒绝与 body/传输错误（会重试）区分开。
#[tokio::test]
async fn resume_206_with_end_below_confirmed_offset_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        while counter.load(Ordering::SeqCst) < 2 {
            let Some(mut stream) = accept_before(&listener, Duration::from_secs(2)).await else {
                break;
            };
            let index = counter.fetch_add(1, Ordering::SeqCst);
            let request = read_request_head(&mut stream).await;
            if index == 0 {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nabc",
                    )
                    .await
                    .expect("first response");
            } else {
                assert!(request.contains("range: bytes=3-"), "{request}");
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 0\r\nContent-Range: bytes 3-2/6\r\nConnection: close\r\n\r\n",
                    )
                    .await;
            }
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = download(
        &asset(format!("http://{address}/plugin"), 6),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect_err("range end below the confirmed prefix must be rejected");
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    // end < offset 在头部校验处失败，报"范围不匹配"；删除该校验后空响应会在
    // 后续长度检查报另一条 Invalid 文案，固定文案因此把两者区分开。
    assert_eq!(error.public_message(), "插件续传范围不匹配");
    server.await.expect("server");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "header-level rejection must not trigger further requests; the count is a \
         sanity signal, the distinguishing proof is the message above"
    );
    assert_eq!(
        std::fs::read(path).expect("prefix untouched"),
        b"abc",
        "end below the confirmed prefix must not be appended"
    );
}

/// 真正发出 Range 后，206 区间终点越过总量边界必须拒绝。
///
/// 样本 `bytes 3-6/6`：start 衔接、total=6 与目录一致，但 6 字节资源的合法
/// 终点最多是 5，`end >= total` 必须拒绝；响应体不再提供任何数据，错误只能
/// 来自头部校验。请求次数断言同样排除 body/传输错误路径的假绿。
#[tokio::test]
async fn resume_206_with_end_at_or_beyond_total_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        while counter.load(Ordering::SeqCst) < 2 {
            let Some(mut stream) = accept_before(&listener, Duration::from_secs(2)).await else {
                break;
            };
            let index = counter.fetch_add(1, Ordering::SeqCst);
            let request = read_request_head(&mut stream).await;
            if index == 0 {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nabc",
                    )
                    .await
                    .expect("first response");
            } else {
                assert!(request.contains("range: bytes=3-"), "{request}");
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 0\r\nContent-Range: bytes 3-6/6\r\nConnection: close\r\n\r\n",
                    )
                    .await;
            }
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = download(
        &asset(format!("http://{address}/plugin"), 6),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect_err("range end at or beyond the total must be rejected");
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    // end >= total 在头部校验处失败，报"范围不匹配"；空响应若无此校验会落到
    // 后续长度检查的另一条 Invalid 文案，固定文案把两者区分开。
    assert_eq!(error.public_message(), "插件续传范围不匹配");
    server.await.expect("server");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "header-level rejection must not trigger further requests; the count is a \
         sanity signal, the distinguishing proof is the message above"
    );
    assert_eq!(
        std::fs::read(path).expect("prefix untouched"),
        b"abc",
        "end beyond the total must not be appended"
    );
}

/// 真正发出 Range 后，206 声称的总量与目录大小不符必须拒绝。
#[tokio::test]
async fn resume_206_with_wrong_total_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        for index in 0..2 {
            let mut stream = listener.accept().await.expect("accept").0;
            let request = read_request_head(&mut stream).await;
            if index == 0 {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nabc",
                    )
                    .await
                    .expect("first response");
            } else {
                assert!(request.contains("range: bytes=3-"), "{request}");
                // 区间衔接但总量与签名目录不符：另一版本或伪造声明。
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 7\r\nContent-Range: bytes 3-9/10\r\nConnection: close\r\n\r\ndefghij",
                    )
                    .await;
            }
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = download(
        &asset(format!("http://{address}/plugin"), 6),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect_err("total mismatch against the signed size must be rejected");
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    // total 不符在头部校验处失败，报"范围不匹配"；删除该校验后截断响应会落到
    // 后续长度检查的另一条 Invalid 文案，固定文案把两者区分开。
    assert_eq!(error.public_message(), "插件续传范围不匹配");
    server.await.expect("server");
    assert_eq!(
        std::fs::read(path).expect("prefix untouched"),
        b"abc",
        "wrong-total range must not be appended"
    );
}

/// etagc 允许逗号：合法强值 "a,b" 必须作为锚点发送 If-Range 并成功续传。
///
/// 若分类误把逗号当非法，此用例会退化为整包重取，第二请求不再携带
/// If-Range，断言即失败——防止逗号误拒回归。
#[tokio::test]
async fn comma_inside_etag_is_a_valid_strong_anchor() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        for index in 0..2 {
            let mut stream = listener.accept().await.expect("accept").0;
            let request = read_request_head(&mut stream).await;
            if index == 0 {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"a,b\"\r\nConnection: close\r\n\r\nabc",
                    )
                    .await
                    .expect("first response");
            } else {
                // 锚点确实以合法强值形态发出，206 按同值回显。
                assert!(request.contains("range: bytes=3-"), "{request}");
                assert!(request.contains("if-range: \"a,b\""), "{request}");
                stream
                    .write_all(
                        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nETag: \"a,b\"\r\nConnection: close\r\n\r\ndef",
                    )
                    .await
                    .expect("second response");
            }
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    download(
        &asset(format!("http://{address}/plugin"), 6),
        &path,
        true,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .expect("comma etag is a valid strong anchor");
    server.await.expect("server");
    assert_eq!(
        std::fs::read(path).expect("download bytes"),
        b"abcdef",
        "valid comma etag must resume, not refetch"
    );
}

/// body 以传输错误收尾时，即使已收满声明字节也不能把异常吞成成功。
///
/// 用不发送终止块的 chunked 响应让错误确定性出现：已发送的数据正好等于声明
/// 长度，但缺少终结块（0 长度块），读取端只能得到传输错误或 deadline 超时，
/// 不可能在流正常结束上误判成功。
#[tokio::test]
async fn body_error_at_full_size_is_a_failure_not_success() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let mut stream = listener.accept().await.expect("accept").0;
        read_request_head(&mut stream).await;
        // 声明“再读 6 字节就完”的 chunked 帧，但永远不发送 0 长度终止块；
        // 6 个数据字节与目录大小一致，可成功落满暂存。
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n6\r\nabcdef\r\n",
            )
            .await
            .expect("chunked body without terminator");
        stream.flush().await.expect("flush");
        // 保持连接打开：客户端要么在等待终止块时撞上注入的 deadline，要么因
        // 连接被外部关闭得到传输错误；两种路径都必须失败而非成功。
        let mut pending = [0u8; 1];
        let _ = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut pending)).await;
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    // 注入 deadline 验证超时路径，不等待真实下载预算；5 秒是慢 runner 的
    // 环境余量（macOS CI 上客户端可能在 800ms 内还没发完请求头就断开，
    // 服务端会因此看不到完整请求）。服务端不发终止块，deadline 仍必然在
    // body 读取阶段到期，断言内容不变；外层窗口相应留出 5 秒收尾余量。
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        download_until(
            &asset(format!("http://{address}/plugin"), 6),
            &path,
            &progress,
            &CancellationToken::new(),
            tokio::time::Instant::now() + Duration::from_secs(5),
        ),
    )
    .await
    .expect("download must not hang past the injected deadline")
    .expect_err("missing terminal chunk must not report success");
    assert_eq!(
        error.public_code(),
        PublicErrorCode::Timeout,
        "deterministic path is the injected deadline expiring during body read"
    );
    server.await.expect("server");
    assert_eq!(
        std::fs::read(path).expect("received bytes").len(),
        6,
        "all declared bytes arrived; only the framing failed"
    );
}

/// 重试次数用尽后必须返回失败，且服务器只收到四次请求，不能无限续传。
#[tokio::test]
async fn exhausted_attempts_return_failure_after_four_requests() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        while counter.load(Ordering::SeqCst) < 4 {
            let Some(mut stream) = accept_before(&listener, Duration::from_secs(2)).await else {
                break;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            read_request_head(&mut stream).await;
            // 每次都在声明长度前断开，让客户端只能通过续传继续；无 ETag 时每次
            // 都是整包重取，请求次数仍是四次。
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc")
                .await
                .expect("truncated response");
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    assert!(
        download(
            &asset(format!("http://{address}/plugin"), 6),
            &path,
            true,
            &progress,
            &CancellationToken::new()
        )
        .await
        .is_err(),
        "exhausted retries must not report success"
    );
    server.await.expect("server");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        4,
        "retry cap must stay at four attempts"
    );
}
