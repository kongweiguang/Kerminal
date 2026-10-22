//! 更新下载适配器的私有传输与校验测试。
//!
//! @author kongweiguang

use std::{future::Future, time::Duration};

use cargo_packager_updater::reqwest::{Url, header::HeaderMap};
use kerminal_application::{AppError, UpdateProgress};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    task::JoinHandle,
};
use tokio_rustls::server::TlsStream;
use tokio_util::sync::CancellationToken;

use super::super::tls_test_support as tls;

const FIXTURE_PUBLIC_KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
const FIXTURE_SIGNATURE: &str = concat!(
    "untrusted comment: signature from minisign secret key\n",
    "RUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/",
    "z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\n",
    "trusted comment: timestamp:1556193335\tfile:test\n",
    "y/rUw2y8/hOUYjZU71eHp/Wo1KZ40fGy2VJEDl34XMJM+TX48Ss/17u3IvIfbVR1FkZZSNCisQbuQY+bHwhEBg=="
);

/// 创建仅供当前测试使用的 HTTPS 服务，保持与产品相同的 TLS 验证路径。
///
/// 连接接收或 TLS 握手失败时测试任务自然结束，只有握手成功才调用 handler。
async fn local_server<F, Fut>(handler: F) -> (Url, JoinHandle<()>)
where
    F: FnOnce(TlsStream<TcpStream>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (listener, address) = tls::bind_loopback().await;
    let acceptor = tls::test_tls_acceptor();
    let task = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await
            && let Ok(stream) = acceptor.accept(stream).await
        {
            handler(stream).await;
        }
    });
    (tls::https_url(address, "/update"), task)
}

/// 按 cargo-packager-updater 的双层 Base64 契约准备公开测试密钥，不包含私钥材料。
fn signature_material() -> (String, String) {
    use base64::{Engine, engine::general_purpose::STANDARD};

    let public_key = format!("untrusted comment: test key\n{FIXTURE_PUBLIC_KEY}");
    (
        STANDARD.encode(public_key),
        STANDARD.encode(FIXTURE_SIGNATURE),
    )
}

/// 写入完整固定 HTTPS/HTTP 响应；显式结束头部区，避免客户端把响应当成未完成头等待。
async fn write_response<S>(mut stream: S, status: &str, headers: &str, body: &[u8])
where
    S: AsyncWrite + Unpin,
{
    let head = format!("HTTP/1.1 {status}\r\n{headers}\r\n\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body).await;
    let _ = stream.flush().await;
}

/// 将固定测试参数映射到生产私有输入结构，保持同一传输与验签路径及原测试约束。
///
/// @author kongweiguang
fn start_download(
    url: Url,
    cancellation: CancellationToken,
    max_bytes: u64,
) -> (
    JoinHandle<Result<Vec<u8>, AppError>>,
    tokio::sync::watch::Receiver<UpdateProgress>,
) {
    use super::{DownloadControl, VerifiedDownloadRequest, download_verified};

    let (public_key, signature) = signature_material();
    let (progress, receiver) = tokio::sync::watch::channel(UpdateProgress::default());
    let trust_roots = vec![tls::trusted_test_root()];
    let task = tokio::spawn(async move {
        download_verified(
            VerifiedDownloadRequest {
                url,
                public_key,
                signature,
                headers: HeaderMap::new(),
                timeout: Some(Duration::from_secs(2)),
                max_bytes,
                additional_trust_roots: &trust_roots,
            },
            DownloadControl {
                cancellation,
                progress,
            },
        )
        .await
    });
    (task, receiver)
}

/// 等待单个隔离下载任务，统一解开测试用 JoinHandle。
async fn download(
    url: Url,
    cancellation: CancellationToken,
    max_bytes: u64,
) -> Result<Vec<u8>, AppError> {
    let (task, _) = start_download(url, cancellation, max_bytes);
    task.await.expect("join local download task")
}

/// 只有 SDK 同款 Minisign 测试签名覆盖的完整字节才能通过下载适配器。
#[tokio::test]
async fn verifies_complete_download_with_sdk_compatible_signature() {
    let (url, server) = local_server(|stream| async move {
        write_response(
            stream,
            "200 OK",
            "Content-Length: 4\r\nConnection: close",
            b"test",
        )
        .await;
    })
    .await;

    let bytes = download(url, CancellationToken::new(), 16)
        .await
        .expect("verify known Minisign vector");
    server.await.expect("join local server");
    assert_eq!(bytes, b"test");
}

/// 任意字节变化都必须在候选进入可安装状态前被签名校验阻断。
#[tokio::test]
async fn rejects_tampered_update_bytes() {
    let (url, server) = local_server(|stream| async move {
        write_response(
            stream,
            "200 OK",
            "Content-Length: 3\r\nConnection: close",
            b"bad",
        )
        .await;
    })
    .await;

    let error = download(url, CancellationToken::new(), 16)
        .await
        .expect_err("tampered payload must fail Minisign verification");
    server.await.expect("join local server");
    assert_eq!(error.public_message(), "更新包签名校验失败");
}

/// 真实 HTTPS client 收到降级跳转时保持原始 302，不连接明文目标。
#[tokio::test]
async fn https_download_client_does_not_follow_loopback_http_redirect() {
    let (url, server) = local_server(|stream| async move {
        write_response(
            stream,
            "302 Found",
            "Location: http://127.0.0.1:1/file\r\nContent-Length: 0\r\nConnection: close",
            b"",
        )
        .await;
    })
    .await;

    let error = download(url, CancellationToken::new(), 16)
        .await
        .expect_err("HTTP downgrade redirect must not return a package");
    server.await.expect("join downgrade redirect server");
    assert_eq!(error.public_message(), "更新服务器未能提供安装包");
}

/// Content-Length 超出策略时在读取响应体前拒绝，避免预留或缓存超限更新。
#[tokio::test]
async fn rejects_oversized_declared_body_before_reading_it() {
    let (url, server) = local_server(|stream| async move {
        write_response(
            stream,
            "200 OK",
            "Content-Length: 1024\r\nConnection: close",
            b"",
        )
        .await;
    })
    .await;

    let error = download(url, CancellationToken::new(), 16)
        .await
        .expect_err("declared oversized body must fail");
    server.await.expect("join local server");
    assert_eq!(error.public_message(), "更新包超过允许的大小限制");
}

/// 未声明总长度时仍按每个 body chunk 累加实施硬上限。
#[tokio::test]
async fn rejects_chunked_body_when_cumulative_bytes_exceed_limit() {
    let (url, server) = local_server(|mut stream| async move {
        let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(b"3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n").await;
        let _ = stream.flush().await;
    })
    .await;

    let error = download(url, CancellationToken::new(), 4)
        .await
        .expect_err("cumulative chunked body must fail");
    server.await.expect("join local server");
    assert_eq!(error.public_message(), "更新包超过允许的大小限制");
}

/// 网络等待必须响应 cancellation token，而不是等服务器读完或 SDK 全局超时。
#[tokio::test]
async fn cancels_while_waiting_for_next_body_chunk() {
    let (url, server) = local_server(|mut stream| async move {
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(b"test").await;
        let _ = stream.flush().await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = stream.write_all(b"test").await;
    })
    .await;
    let cancellation = CancellationToken::new();
    let (download_task, mut progress) = start_download(url, cancellation.clone(), 16);
    tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            if progress.borrow_and_update().received_bytes > 0 {
                break;
            }
            progress
                .changed()
                .await
                .expect("observe first response chunk");
        }
    })
    .await
    .expect("receive first chunk before testing cancellation");
    cancellation.cancel();

    let result = tokio::time::timeout(Duration::from_millis(300), download_task)
        .await
        .expect("cancellation must not wait for server timeout")
        .expect("join canceled downloader");
    server.abort();
    assert_eq!(
        result
            .expect_err("canceled response must not produce installable bytes")
            .public_message(),
        "更新下载已取消"
    );
}

/// 初始 HTTPS 与 HTTPS 重定向可用；降级、URL 凭据及带认证头跨源跳转会被阻断。
#[test]
fn rejects_unsafe_redirect_targets_and_authenticated_cross_origin_redirects() {
    let source = Url::parse("https://updates.example.com/file").expect("parse source URL");
    let https_target = Url::parse("https://assets.example.net/file").expect("parse HTTPS target");
    let http_target = Url::parse("http://127.0.0.1:8080/file").expect("parse loopback HTTP target");
    let credential_target =
        Url::parse("https://user:secret@assets.example.net/file").expect("parse credential URL");
    let same_origin_target =
        Url::parse("https://updates.example.com/next").expect("parse same-origin target");

    assert!(super::redirect_allowed(
        std::slice::from_ref(&source),
        &https_target,
        false
    ));
    assert!(!super::redirect_allowed(
        std::slice::from_ref(&source),
        &http_target,
        false
    ));
    assert!(!super::redirect_allowed(
        std::slice::from_ref(&source),
        &credential_target,
        false
    ));
    assert!(!super::redirect_allowed(
        std::slice::from_ref(&source),
        &https_target,
        true
    ));
    assert!(super::redirect_allowed(
        std::slice::from_ref(&source),
        &same_origin_target,
        false
    ));
}

/// 已取消任务在解析签名、建立客户端和打开网络连接前立即结束。
#[tokio::test]
async fn honors_cancellation_before_network_setup() {
    let url = Url::parse("https://updates.example.com/file").expect("parse update URL");
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let error = download(url, cancellation, 16)
        .await
        .expect_err("canceled update must not start");
    assert_eq!(error.public_message(), "更新下载已取消");
}
