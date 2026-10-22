//! 更新 metadata 传输边界与打包清单 SDK 契约测试。
//!
//! @author kongweiguang

use std::time::Duration;

use cargo_packager_updater::{
    RemoteRelease,
    reqwest::{Client, Response},
};
use semver::Version;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::super::tls_test_support as tls;

const MAX_BODY_BYTES: u64 = 64;

/// 在独立 HTTPS listener 读完客户端请求头后才回包，避免测试服务端抢在请求提交前响应。
async fn local_response(
    status: &str,
    headers: &str,
    body: Vec<u8>,
    chunked: bool,
) -> (Response, JoinHandle<()>) {
    let (listener, address) = tls::bind_loopback().await;
    let acceptor = tls::test_tls_acceptor();
    // Tokio task 需要 `'static` 所有权；复制原始文本可保持测试响应的 HTTP 字节语义不变。
    let status = status.to_owned();
    let headers = headers.to_owned();
    let server = tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .expect("accept local HTTPS test client");
        let mut stream = acceptor
            .accept(stream)
            .await
            .expect("complete private TLS fixture handshake");
        tokio::time::timeout(Duration::from_secs(2), read_request_head(&mut stream))
            .await
            .expect("receive local metadata request before fixture response")
            .expect("read complete local metadata request head");
        write_response(stream, &status, &headers, &body, chunked).await;
    });
    let client = super::build_client_from_builder(Client::builder(), &[tls::trusted_test_root()])
        .expect("build production-policy metadata client with test trust root");
    let response = client
        .get(tls::https_url(address, "/metadata"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .expect("receive local metadata response");
    (response, server)
}

/// 只读到 CRLFCRLF 后即停止，避免 fixture 在响应前吞掉额外请求数据或无限读取。
async fn read_request_head<S>(stream: &mut S) -> std::io::Result<()>
where
    S: AsyncRead + Unpin,
{
    const MAX_REQUEST_HEAD_BYTES: usize = 16 * 1024;

    let mut delimiter = [0_u8; 4];
    let mut total_bytes = 0_usize;
    let mut byte = [0_u8; 1];
    loop {
        if stream.read(&mut byte).await? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "fixture client closed before completing its request head",
            ));
        }
        total_bytes = total_bytes.saturating_add(1);
        if total_bytes > MAX_REQUEST_HEAD_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "fixture request head exceeded its limit",
            ));
        }
        delimiter.copy_within(1.., 0);
        delimiter[3] = byte[0];
        if total_bytes >= delimiter.len() && delimiter == *b"\r\n\r\n" {
            return Ok(());
        }
    }
}

/// 写入完整普通或 chunked HTTPS/HTTP 响应，保留头部终止空行供 reqwest 正常解析。
async fn write_response<S>(mut stream: S, status: &str, headers: &str, body: &[u8], chunked: bool)
where
    S: AsyncWrite + Unpin,
{
    let head = format!("HTTP/1.1 {status}\r\n{headers}\r\n\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    if chunked {
        let chunk_header = format!("{:X}\r\n", body.len());
        let _ = stream.write_all(chunk_header.as_bytes()).await;
        let _ = stream.write_all(body).await;
        let _ = stream.write_all(b"\r\n0\r\n\r\n").await;
    } else {
        let _ = stream.write_all(body).await;
    }
    let _ = stream.flush().await;
}

/// 读取由打包清单生成器冻结的真实 JSON，再通过 SDK RemoteRelease 自定义解析器解码。
fn packaging_manifest_fixture() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/updater/cargo-packager-updater-0.2.3.json"
    ))
    .expect("read packaging-generated updater manifest")
}

/// SDK 解析打包器生成的多平台 feed 后，应继续提供签名、URL 和安装格式契约。
#[tokio::test]
async fn reads_packaging_generated_manifest_through_sdk_remote_release() {
    let fixture = packaging_manifest_fixture();
    let (response, server) = local_response(
        "200 OK",
        "Transfer-Encoding: chunked\r\nConnection: close",
        fixture,
        true,
    )
    .await;

    let release = super::read_release_body(response, CancellationToken::new(), 1024 * 1024)
        .await
        .expect("bounded SDK metadata read")
        .expect("release manifest exists");
    server.await.expect("join metadata fixture server");

    assert_eq!(release.version, Version::parse("0.4.0-dev.0").unwrap());
    assert_eq!(
        release.download_url("windows-x86_64").unwrap().as_str(),
        "https://updates.invalid/kerminal/0.4.0/Kerminal_0.4.0_x64-setup.exe"
    );
    assert_eq!(release.format("macos-aarch64").unwrap().to_string(), "app");
    assert!(!release.signature("linux-x86_64").unwrap().is_empty());
}

/// 声明的响应体超限时必须在读取和分配 body 前立即拒绝。
#[tokio::test]
async fn rejects_metadata_with_oversized_content_length() {
    let (response, server) = local_response(
        "200 OK",
        "Content-Length: 1024\r\nConnection: close",
        Vec::new(),
        false,
    )
    .await;

    let error = super::read_release_body(response, CancellationToken::new(), MAX_BODY_BYTES)
        .await
        .expect_err("oversized declared body must fail");
    server.await.expect("join oversized metadata server");
    assert_eq!(error.public_message(), "更新 metadata 超过允许的大小限制");
}

/// 未声明 Content-Length 时仍需按 chunk 累加，防止 chunked feed 绕过内存上限。
#[tokio::test]
async fn rejects_chunked_metadata_when_cumulative_bytes_exceed_limit() {
    let (response, server) = local_response(
        "200 OK",
        "Transfer-Encoding: chunked\r\nConnection: close",
        vec![b'x'; (MAX_BODY_BYTES + 1) as usize],
        true,
    )
    .await;

    let error = super::read_release_body(response, CancellationToken::new(), MAX_BODY_BYTES)
        .await
        .expect_err("oversized chunked response must fail");
    server.await.expect("join chunked metadata server");
    assert_eq!(error.public_message(), "更新 metadata 超过允许的大小限制");
}

/// SDK 约定用 204 表示无更新；不继续解析空 body 或尝试后续 endpoint。
#[tokio::test]
async fn maps_no_content_to_no_update() {
    let (response, server) = local_response(
        "204 No Content",
        "Content-Length: 0\r\nConnection: close",
        Vec::new(),
        false,
    )
    .await;

    let result = super::read_release_body(response, CancellationToken::new(), MAX_BODY_BYTES)
        .await
        .expect("204 means no update");
    server.await.expect("join no-content metadata server");
    assert!(result.is_none());
}

/// metadata client 必须在网络层停止 HTTPS 到 HTTP 的跳转，不能请求降级目标。
#[tokio::test]
async fn metadata_client_does_not_follow_loopback_http_redirect() {
    let (response, server) = local_response(
        "302 Found",
        "Location: http://127.0.0.1:1/feed.json\r\nContent-Length: 0\r\nConnection: close",
        Vec::new(),
        false,
    )
    .await;

    let error = super::read_release_response(response, CancellationToken::new(), MAX_BODY_BYTES)
        .await
        .expect_err("metadata HTTP downgrade redirect must fail closed");
    server.await.expect("join metadata redirect server");
    assert_eq!(error.public_message(), "更新源未能提供版本信息");
}

/// 本地测试根证书只放宽证书链，不允许 loopback HTTP 绕过 metadata 的传输策略。
#[test]
fn metadata_policy_rejects_loopback_http_even_with_a_test_root() {
    let insecure = cargo_packager_updater::reqwest::Url::parse("http://127.0.0.1:8080/feed.json")
        .expect("parse loopback HTTP URL");
    let secure = cargo_packager_updater::reqwest::Url::parse("https://127.0.0.1:8080/feed.json")
        .expect("parse loopback HTTPS URL");

    assert!(!super::is_safe_metadata_url(&insecure));
    assert!(super::is_safe_metadata_url(&secure));
}

/// 新版本排序及平台缺失错误沿用 SDK RemoteRelease 与 semver 类型。
#[test]
fn uses_sdk_version_ordering_and_reports_missing_target() {
    let release: RemoteRelease =
        serde_json::from_slice(&packaging_manifest_fixture()).expect("parse SDK release");
    let current = Version::parse("0.4.0-dev.0").expect("parse current version");
    let older = Version::parse("0.3.9").expect("parse older version");

    assert!(!super::release_is_newer(&current, &release));
    assert!(super::release_is_newer(&older, &release));
    assert!(release.download_url("windows-arm64").is_err());
}

/// 错误版本必须经 SDK 的 SemVer 解析失败，不能因字符串排序生成候选。
#[test]
fn rejects_invalid_sdk_release_version() {
    let invalid = br#"{
      "version": "latest",
      "notes": "invalid",
      "platforms": {
        "windows-x86_64": {
          "url": "https://updates.invalid/update.exe",
          "signature": "test",
          "format": "nsis"
        }
      }
    }"#;

    assert!(serde_json::from_slice::<RemoteRelease>(invalid).is_err());
}

/// 映射为 SDK Update 后继续让 cargo-packager 决定平台安装目标和格式操作。
#[test]
fn maps_real_packaging_release_to_sdk_update() {
    use cargo_packager_updater::reqwest::Url;

    let release: RemoteRelease =
        serde_json::from_slice(&packaging_manifest_fixture()).expect("parse SDK release");
    let settings = super::super::settings::UpdaterSettings::new(
        "0.3.9",
        vec![Url::parse("https://updates.invalid/kerminal/0.4.0").unwrap()],
        "test-only-public-key".to_owned(),
        Duration::from_secs(30),
        1024 * 1024,
    )
    .expect("construct test updater settings");
    let (target, json_target, _) = super::current_target().expect("current SDK target");

    let update = super::update_from_release(
        &settings,
        release,
        Version::parse("0.3.9").unwrap(),
        &target,
        &json_target,
    )
    .expect("map SDK release to SDK update");

    assert_eq!(update.version, "0.4.0-dev.0");
    assert_eq!(update.target, target);
    assert_eq!(
        update.format.to_string(),
        if target == "windows" {
            "nsis"
        } else if target == "linux" {
            "appimage"
        } else {
            "app"
        }
    );
    assert!(
        update
            .download_url
            .as_str()
            .starts_with("https://updates.invalid/")
    );
}
