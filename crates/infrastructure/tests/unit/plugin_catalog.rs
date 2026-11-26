//! 可配置目录、来源隔离与无签名缓存的回归。
//! @author kongweiguang

use super::*;
use kerminal_domain::plugins::PluginId;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::{Duration, timeout},
};

/// 用真实目录缓存和本地 HTTP 响应验证图标过滤与进程内复用。
#[tokio::test]
async fn catalog_icon_accepts_svg_and_rejects_html() -> Result<(), Box<dyn std::error::Error>> {
    for (body, accepted) in [("<svg></svg>", true), ("<html><svg></svg></html>", false)] {
        let (url, server) = serve_once("200 OK", body).await;
        let root = tempfile::tempdir()?;
        let client = CatalogClient::new(StorageCoordinator::new(root.path()), "")?;
        let mut cached = cache_with_readme("fixture", 1, &url);
        let mut raw: serde_json::Value = serde_json::from_str(&cached.raw)?;
        raw["plugins"][0]["icon_url"] = serde_json::Value::String(url);
        cached.raw = serde_json::to_string(&raw)?;
        client.persist(&state(&client), &cached)?;
        let id = PluginId::parse("acme.tool")?;
        let icon = client.icon(&id, CancellationToken::new()).await?;
        assert_eq!(icon.is_some(), accepted);
        server.await?;
        if accepted {
            assert_eq!(
                client.icon(&id, CancellationToken::new()).await?.as_deref(),
                Some(body.as_bytes())
            );
        }
    }
    Ok(())
}

/// 缺少图标地址时不发网络请求，目录条目仍可正常展示。
#[tokio::test]
async fn catalog_icon_missing_url_returns_none() -> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let client = CatalogClient::new(StorageCoordinator::new(root.path()), "")?;
    client.persist(
        &state(&client),
        &cache_with_readme("fixture", 1, "http://127.0.0.1/readme"),
    )?;
    let id = PluginId::parse("acme.tool")?;
    assert!(client.icon(&id, CancellationToken::new()).await?.is_none());
    Ok(())
}

/// 测试装配保留旧的三参数调用形状，避免安装流程测试为了删公钥而大改调用点。
///
/// 放在外置测试模块而不是产品 impl：架构门禁只允许 src 保留指向 tests/ 的
/// cfg(test) 模块引用，测试专用构造器必须随测试实现留在 tests/；本模块只在
/// 测试构建编译，生产二进制不会带上这个入口。
impl CatalogClient {
    /// 旧三参数形状中的 key 已无用途，保留参数只为不惊动安装流程测试的调用点。
    pub fn testing(storage: StorageCoordinator, url: String, _key: String) -> Self {
        Self::new(storage, &url).expect("测试目录地址必须有效")
    }
}

/// 当前来源快照；切换来源后必须重新获取，测试不能复用旧快照写缓存。
fn state(client: &CatalogClient) -> SourceState {
    client.source_snapshot().expect("source snapshot")
}

/// 缓存路径由来源快照决定，测试直接按同一文件名核对落盘与隔离。
fn cache_path(root: &Path, source: &SourceState) -> PathBuf {
    root.join("plugins")
        .join("catalog-cache")
        .join(&source.cache_file)
}

/// 最小合法目录原文；`spaced` 只改变排版，用来证明身份摘要绑定原始字节。
fn raw_catalog(source_id: &str, sequence: u64, spaced: bool) -> String {
    if spaced {
        format!(
            "{{ \"schema\":1, \"source_id\":\"{source_id}\", \"sequence\":{sequence}, \"plugins\":[]}}"
        )
    } else {
        format!(
            "{{\"schema\":1,\"source_id\":\"{source_id}\",\"sequence\":{sequence},\"plugins\":[]}}"
        )
    }
}

/// 新缓存不再生成签名；`signature` 留空以验证写入格式与旧缓存读取兼容。
fn cache(source_id: &str, sequence: u64, spaced: bool) -> Cache {
    Cache {
        raw: raw_catalog(source_id, sequence, spaced),
        signature: None,
        fetched_at: 1,
    }
}

/// 构造带单个平台资产的目录，把结论锚定在资产 URL 契约而不是其它字段。
fn cache_with_assets(source_id: &str, sequence: u64, urls: &[&str]) -> Cache {
    let assets: Vec<String> = urls
        .iter()
        .map(|url| {
            format!(
                r#"{{"platform":"any","url":{url},"sha256":"{}","size":1}}"#,
                "0".repeat(64)
            )
        })
        .collect();
    let raw = format!(
        r#"{{"schema":1,"source_id":"{source_id}","sequence":{sequence},"plugins":[{{"id":"acme.tool","name":{{"en":"Tool"}},"description":{{"en":"Tool"}},"publisher":"acme","categories":["tool"],"icon_url":null,"readme_url":null,"repository":"https://github.com/acme/tool","permissions":[],"versions":[{{"version":"1.0.0","api":"*","published_at":"2026-01-01T00:00:00Z","changelog_url":null,"assets":[{}]}}]}}]}}"#,
        assets.join(",")
    );
    Cache {
        raw,
        signature: None,
        fetched_at: 1,
    }
}

/// README 地址与资产共用同一条 URL 契约，单独构造以覆盖目录级校验。
fn cache_with_readme(source_id: &str, sequence: u64, readme: &str) -> Cache {
    let raw = format!(
        r#"{{"schema":1,"source_id":"{source_id}","sequence":{sequence},"plugins":[{{"id":"acme.tool","name":{{"en":"Tool"}},"description":{{"en":"Tool"}},"publisher":"acme","categories":["tool"],"icon_url":null,"readme_url":"{readme}","repository":"https://github.com/acme/tool","permissions":[],"versions":[{{"version":"1.0.0","api":"*","published_at":"2026-01-01T00:00:00Z","changelog_url":null,"assets":[{{"platform":"any","url":"https://github.com/acme/tool/releases/download/v1/tool.zip","sha256":"{}","size":1}}]}}]}}]}}"#,
        "0".repeat(64)
    );
    Cache {
        raw,
        signature: None,
        fetched_at: 1,
    }
}

/// 读到完整请求头即返回；连接提前断开或超限都让服务任务显式失败。
async fn read_request(stream: &mut tokio::net::TcpStream) {
    let mut bytes = Vec::new();
    let mut part = [0u8; 1024];
    loop {
        let n = stream.read(&mut part).await.expect("read request");
        assert!(n > 0, "client disconnected before full request");
        bytes.extend_from_slice(&part[..n]);
        assert!(bytes.len() <= 64 * 1024, "request exceeds header bound");
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
}

/// 只响应一次请求的 loopback 服务；刷新成功即证明客户端没有额外请求签名文件。
async fn serve_once(status: &str, body: &str) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let status = status.to_owned();
    let body = body.to_owned();
    let handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        read_request(&mut stream).await;
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write response");
        stream.shutdown().await.ok();
    });
    (format!("http://{address}/catalog.json"), handle)
}

/// 收到请求后保持连接不响应，让测试确定地取消尚在等待的 fetch。
async fn serve_pending() -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        read_request(&mut stream).await;
        let _ = seen_tx.send(());
        let mut part = [0u8; 1024];
        let _ = timeout(Duration::from_secs(10), stream.read(&mut part)).await;
    });
    (format!("http://{address}/catalog.json"), seen_rx, handle)
}

/// 请求到达后等待测试放行再响应，用来制造“下载期间切换来源”的真实时序。
async fn serve_after_release(
    body: &str,
) -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let body = body.to_owned();
    let handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        read_request(&mut stream).await;
        let _ = seen_tx.send(());
        release_rx.await.expect("release refresh");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write response");
        stream.shutdown().await.ok();
    });
    (
        format!("http://{address}/catalog.json"),
        seen_rx,
        release_tx,
        handle,
    )
}

/// 连续返回 302 的服务；计数用于证明重定向在三次跟随之后停止。
async fn serve_redirects() -> (
    String,
    std::sync::Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let count = std::sync::Arc::new(AtomicUsize::new(0));
    let handle = tokio::spawn({
        let count = std::sync::Arc::clone(&count);
        async move {
            for _ in 0..6 {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let index = count.fetch_add(1, Ordering::SeqCst);
                read_request(&mut stream).await;
                let location = format!("http://{address}/r{}", index + 1);
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write redirect");
                stream.shutdown().await.ok();
            }
        }
    });
    (format!("http://{address}/r0"), count, handle)
}

/// 在单条 keep-alive 连接上连续响应指定次数，并统计实际接受的连接数。
///
/// 这是连接复用回归的服务端证据：若客户端每次抓取都新建 `Client`，第二次请求会
/// 另开一条连接并被 accept 计数捕捉；若复用同一连接，则只 accept 一次。响应不带
/// `Connection: close`，客户端读尽正文后会把连接放回池中供下一次 `fetch` 使用。
/// 第一条连接若在第二个请求前被对端关闭，仍退回 accept 新连接完成剩余请求，
/// 让断言只依赖“连接数是否为 1”，不会因超时把失败伪装成挂起。
async fn serve_keep_alive_requests(
    requests: usize,
) -> (
    String,
    std::sync::Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let accepted = std::sync::Arc::new(AtomicUsize::new(0));
    let handle = tokio::spawn({
        let accepted = std::sync::Arc::clone(&accepted);
        async move {
            let mut served = 0;
            'accept: while served < requests {
                let Ok(Ok((mut stream, _))) =
                    timeout(Duration::from_secs(10), listener.accept()).await
                else {
                    break;
                };
                accepted.fetch_add(1, Ordering::SeqCst);
                while served < requests {
                    let mut request = Vec::new();
                    let mut part = [0u8; 1024];
                    loop {
                        let n = match timeout(Duration::from_secs(5), stream.read(&mut part)).await
                        {
                            Ok(Ok(n)) => n,
                            _ => continue 'accept,
                        };
                        if n == 0 {
                            continue 'accept;
                        }
                        request.extend_from_slice(&part[..n]);
                        assert!(request.len() <= 64 * 1024, "request exceeds header bound");
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    served += 1;
                    let body = raw_catalog("fixture", served as u64, false);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
                        body.len()
                    );
                    stream
                        .write_all(response.as_bytes())
                        .await
                        .expect("write keep-alive response");
                }
            }
        }
    });
    (format!("http://{address}/catalog.json"), accepted, handle)
}

/// 正常路径最后等待服务任务退出，避免测试留下悬挂连接。
async fn join_server(server: tokio::task::JoinHandle<()>) {
    timeout(Duration::from_secs(10), server)
        .await
        .expect("server must finish in time")
        .expect("server task must not panic");
}

/// 在缓存读取挂起时切换来源，确定复现“旧来源结果不得冒充新来源”的竞态。
///
/// 先持有存储锁再首次 poll：readme 会同步冻结旧来源，随后阻塞读取被锁挡住而
/// 必然返回 Pending；此时切换来源并释放锁，不依赖 sleep 或调度顺序。若让阻塞
/// 读取在首次 poll 前完成，JoinHandle 可能直接 Ready，测试就无法稳定落在竞态点。
fn switch_source_mid_read(
    client: &CatalogClient,
    future: std::pin::Pin<&mut impl std::future::Future>,
    url: &str,
) {
    let guard = client.storage.lock().expect("hold storage lock");
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        std::future::Future::poll(future, &mut context).is_pending(),
        "readme must suspend while the storage lock is held"
    );
    client.set_source_url(url).expect("switch source");
    drop(guard);
}

/// 绑定后立即释放回环端口，得到确定拒绝连接的 README 地址。
///
/// 用它区分“切换来源后根本不该请求旧 README”和“请求失败”：修复前会先连接该
/// 地址并得到基础设施错误，修复后应在查找条目前直接返回 Conflict。
async fn closed_loopback_readme_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    drop(listener);
    format!("http://{address}/readme.md")
}

/// 空配置折叠为官方地址，自定义地址按稳定哈希派生缓存名，避免 URL 直接进入路径。
#[test]
fn source_state_normalizes_default_and_derives_isolated_cache_names() {
    let official = SourceState::new("").expect("empty config");
    assert_eq!(official.url, DEFAULT_PLUGIN_CATALOG_URL);
    assert_eq!(official.cache_file, "official.json");
    assert_eq!(
        SourceState::new("   ").expect("blank config").url,
        DEFAULT_PLUGIN_CATALOG_URL
    );
    assert_eq!(
        SourceState::new(DEFAULT_PLUGIN_CATALOG_URL)
            .expect("official url")
            .cache_file,
        "official.json"
    );

    let first = SourceState::new("http://127.0.0.1:8080/catalog.json").expect("loopback");
    assert_eq!(first.url, "http://127.0.0.1:8080/catalog.json");
    assert!(first.cache_file.starts_with("catalog-"));
    assert!(first.cache_file.ends_with(".json"));
    assert_eq!(
        first.cache_file.len(),
        "catalog-".len() + 64 + ".json".len()
    );
    let second = SourceState::new("https://downloads.example.com/catalog.json").expect("custom");
    assert_ne!(first.cache_file, second.cache_file);
    assert_eq!(
        first,
        SourceState::new(" http://127.0.0.1:8080/catalog.json ").expect("trimmed")
    );

    let root = tempfile::tempdir().expect("root");
    let storage = StorageCoordinator::new(root.path());
    assert_eq!(
        CatalogClient::official(storage.clone()).source_url(),
        DEFAULT_PLUGIN_CATALOG_URL
    );
    assert_eq!(
        CatalogClient::new(storage, "  ")
            .expect("blank")
            .source_url(),
        DEFAULT_PLUGIN_CATALOG_URL
    );
    assert_eq!(
        CatalogClient::new(
            StorageCoordinator::new(root.path()),
            "ftp://example.com/catalog.json"
        )
        .err()
        .expect("ftp rejected")
        .public_code(),
        kerminal_application::PublicErrorCode::Invalid
    );
    let long = format!("https://example.com/{}", "a".repeat(2048));
    assert_eq!(
        CatalogClient::new(StorageCoordinator::new(root.path()), &long)
            .err()
            .expect("too long")
            .public_code(),
        kerminal_application::PublicErrorCode::Invalid
    );
}

/// 自定义来源标识只要求非空有界；身份同时绑定实际地址，防止跨来源复用确认。
#[tokio::test]
async fn custom_source_id_is_bounded_and_identity_binds_actual_url() {
    let root = tempfile::tempdir().expect("root");
    let url = "http://127.0.0.1:8080/catalog.json";
    let client = CatalogClient::new(StorageCoordinator::new(root.path()), url).expect("client");
    let source = state(&client);
    let expected = cache("fixture", 1, false);
    let verified = client
        .persist(&source, &expected)
        .expect("custom source id");
    assert_eq!(verified.identity().source_id, "fixture");
    assert_eq!(verified.identity().source_url, url);
    assert_eq!(
        verified.identity().raw_sha256,
        sha256_hex(expected.raw.as_bytes())
    );

    let long = "x".repeat(129);
    for source_id in ["", "   ", "bad\nid", long.as_str()] {
        assert_eq!(
            client
                .persist(&source, &cache(source_id, 2, false))
                .expect_err("invalid source id")
                .public_code(),
            kerminal_application::PublicErrorCode::Invalid
        );
    }
    let cached = client
        .cached_verified()
        .await
        .expect("read cache")
        .expect("cache present");
    assert_eq!(cached.identity().sequence, 1);
}

/// 升级前的官方签名缓存仍可读取；新写入不再产生签名，也不会把旧文件当成损坏。
#[tokio::test]
async fn old_signed_official_cache_remains_readable_and_new_writes_omit_signature() {
    let root = tempfile::tempdir().expect("root");
    let storage = StorageCoordinator::new(root.path());
    let directory = root.path().join("plugins/catalog-cache");
    fs::create_dir_all(&directory).expect("cache directory");
    let old = json!({
        "raw": raw_catalog("official", 1, false),
        "signature": "untrusted comment: old\nAAAA\ntrusted comment: old\nBBBB\n",
        "fetched_at": 7,
    });
    fs::write(
        directory.join("official.json"),
        serde_json::to_vec(&old).expect("encode old cache"),
    )
    .expect("seed old cache");

    let client = CatalogClient::official(storage);
    let cached = client
        .cached_verified()
        .await
        .expect("read old cache")
        .expect("cache present");
    assert_eq!(cached.identity().source_id, "official");
    assert_eq!(cached.identity().source_url, DEFAULT_PLUGIN_CATALOG_URL);
    assert_eq!(cached.snapshot.fetched_at, 7);

    let source = state(&client);
    client
        .persist(&source, &cache("official", 2, false))
        .expect("new write");
    let written = fs::read_to_string(directory.join("official.json")).expect("read new cache");
    assert!(
        !written.contains("\"signature\""),
        "new cache must omit signature"
    );
    let value: serde_json::Value = serde_json::from_str(&written).expect("new cache json");
    assert_eq!(value["fetched_at"], 1);
    assert_eq!(
        client
            .cached_verified()
            .await
            .expect("read new cache")
            .expect("cache present")
            .identity()
            .sequence,
        2
    );
}

/// 序列只是身份元数据：同序列新内容可以刷新，但旧确认必须因摘要变化而冲突。
#[tokio::test]
async fn same_sequence_new_content_refreshes_and_changes_identity() {
    let root = tempfile::tempdir().expect("root");
    let client = CatalogClient::new(
        StorageCoordinator::new(root.path()),
        "http://127.0.0.1:8080/catalog.json",
    )
    .expect("client");
    let source = state(&client);
    let first = client
        .persist(&source, &cache("fixture", 1, false))
        .expect("first");
    let first_identity = first.identity().clone();
    let second = client
        .persist(&source, &cache("fixture", 1, true))
        .expect("same sequence refresh");
    assert_ne!(first_identity.raw_sha256, second.identity().raw_sha256);
    assert_eq!(second.identity().sequence, 1);
    assert_eq!(
        client
            .cached_matching(&first_identity)
            .await
            .expect_err("stale selection")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    assert!(client.cached_matching(second.identity()).await.is_ok());

    let lower = client
        .persist(&source, &cache("fixture", 0, false))
        .expect("sequence is metadata");
    assert_eq!(lower.identity().sequence, 0);
    assert_eq!(
        client
            .cached_matching(second.identity())
            .await
            .expect_err("stale selection")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
}

/// 切换来源后旧缓存与旧确认都不可见；克隆共享当前来源，无效切换保留原来源。
#[tokio::test]
async fn source_switch_isolates_caches_and_rejects_old_identity() {
    let root = tempfile::tempdir().expect("root");
    let url_a = "http://127.0.0.1:8080/a.json";
    let url_b = "https://downloads.example.com/b.json";
    let client = CatalogClient::new(StorageCoordinator::new(root.path()), url_a).expect("client");
    let source_a = state(&client);
    let identity_a = client
        .persist(&source_a, &cache("fixture-a", 1, false))
        .expect("seed a")
        .identity()
        .clone();
    assert!(cache_path(root.path(), &source_a).is_file());

    let clone = client.clone();
    client.set_source_url(url_b).expect("switch to b");
    assert_eq!(client.source_url(), url_b);
    assert_eq!(clone.source_url(), url_b, "clones share current source");
    let source_b = state(&client);
    assert!(!cache_path(root.path(), &source_b).exists());
    assert!(client.cached_verified().await.expect("read b").is_none());
    assert_eq!(
        client
            .cached_matching(&identity_a)
            .await
            .expect_err("old source identity")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );

    let identity_b = client
        .persist(&source_b, &cache("fixture-b", 2, false))
        .expect("seed b")
        .identity()
        .clone();
    assert_eq!(
        client
            .cached_verified()
            .await
            .expect("read b")
            .expect("cache present")
            .identity()
            .sequence,
        2
    );
    client.set_source_url(url_a).expect("switch back to a");
    assert_eq!(
        client
            .cached_verified()
            .await
            .expect("read a")
            .expect("cache present")
            .identity()
            .sequence,
        1
    );
    assert_eq!(
        client
            .cached_matching(&identity_b)
            .await
            .expect_err("other source identity")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );

    assert_eq!(
        client
            .set_source_url("https://user:pass@example.com/catalog.json")
            .expect_err("invalid switch")
            .public_code(),
        kerminal_application::PublicErrorCode::Invalid
    );
    assert_eq!(
        client.source_url(),
        url_a,
        "invalid switch keeps old source"
    );
}

/// 损坏缓存必须如实报错并保留原文件，不能被下一次刷新静默覆盖。
#[tokio::test]
async fn corrupt_cache_reports_error_and_blocks_overwrite() {
    let root = tempfile::tempdir().expect("root");
    let client = CatalogClient::new(
        StorageCoordinator::new(root.path()),
        "http://127.0.0.1:8080/catalog.json",
    )
    .expect("client");
    let source = state(&client);
    client
        .persist(&source, &cache("fixture", 1, false))
        .expect("seed cache");
    let path = cache_path(root.path(), &source);
    fs::write(&path, b"not json").expect("corrupt cache");

    assert_eq!(
        client
            .cached_verified()
            .await
            .expect_err("corrupt cache must fail")
            .public_code(),
        kerminal_application::PublicErrorCode::Invalid
    );
    assert_eq!(
        client
            .persist(&source, &cache("fixture", 2, false))
            .expect_err("corrupt cache blocks persist")
            .public_code(),
        kerminal_application::PublicErrorCode::Invalid
    );
    assert_eq!(
        fs::read(&path).expect("cache remains"),
        b"not json".to_vec()
    );
}

/// 目录、README 和资产共用同一 URL 契约：允许局域网与回环，拒绝歧义原文。
#[test]
fn catalog_and_asset_urls_use_shared_contract() {
    for url in [
        "http://127.0.0.1:8080/catalog.json",
        "http://192.168.1.10:8443/catalog.json",
        "https://downloads.example.com/catalog.json",
    ] {
        assert!(SourceState::new(url).is_ok(), "accept {url}");
    }
    for url in [
        "https://user:pass@example.com/catalog.json",
        "https://example.com/catalog.json#frag",
        "https://example.com:0/catalog.json",
        "ftp://example.com/catalog.json",
        "https://example.com\\catalog.json",
    ] {
        assert!(SourceState::new(url).is_err(), "reject {url}");
    }

    let root = tempfile::tempdir().expect("root");
    let client = CatalogClient::new(
        StorageCoordinator::new(root.path()),
        "http://127.0.0.1:8080/catalog.json",
    )
    .expect("client");
    let source = state(&client);
    assert!(
        client
            .persist(
                &source,
                &cache_with_assets(
                    "fixture",
                    1,
                    &["\"http://192.168.1.10:8443/acme/tool.zip\""],
                )
            )
            .is_ok()
    );
    for url in [
        "\"https://user@downloads.example.com/acme/tool.zip\"",
        "\"https://downloads.example.com/acme/tool.zip#frag\"",
        "\"https://downloads.example.com:0/acme/tool.zip\"",
    ] {
        assert_eq!(
            client
                .persist(&source, &cache_with_assets("fixture", 2, &[url]))
                .expect_err("bad asset url")
                .public_code(),
            kerminal_application::PublicErrorCode::Invalid
        );
    }
    assert!(
        client
            .persist(
                &source,
                &cache_with_readme("fixture", 3, "http://127.0.0.1:8080/readme.md")
            )
            .is_ok()
    );
    assert_eq!(
        client
            .persist(
                &source,
                &cache_with_readme("fixture", 4, "https://user@example.com/readme.md")
            )
            .expect_err("bad readme url")
            .public_code(),
        kerminal_application::PublicErrorCode::Invalid
    );
}

/// 刷新只请求目录 JSON；成功后写入当前来源缓存并返回非 stale 快照。
#[tokio::test]
async fn refresh_success_uses_only_json_and_writes_current_source_cache() {
    let body = raw_catalog("fixture", 1, false);
    let (url, server) = serve_once("200 OK", &body).await;
    let root = tempfile::tempdir().expect("root");
    let client = CatalogClient::new(StorageCoordinator::new(root.path()), &url).expect("client");
    let snapshot = client
        .refresh_verified(CancellationToken::new())
        .await
        .expect("refresh");
    assert!(!snapshot.snapshot.stale);
    assert_eq!(snapshot.identity().source_id, "fixture");
    assert_eq!(snapshot.identity().source_url, url);
    assert!(cache_path(root.path(), &state(&client)).is_file());
    join_server(server).await;
}

/// 连续抓取复用同一个 HTTP 连接，证明网络客户端只在首次请求时建立。
///
/// 旧实现每次 `fetch` 都新建 `Client`，连接池随实例丢弃，第二次请求必然重新
/// 握手；客户端移入 `CatalogClient` 并延迟初始化后，回环服务只需 accept 一次
/// 就能完成两次抓取。断言连接数而不是只断言成功，才能区分“真的复用”与
/// “碰巧两次都成功”；同时校验两次目录正文，确认复用没有串响应。
#[tokio::test]
async fn repeated_fetch_reuses_keep_alive_connection() {
    let (url, accepted, server) = serve_keep_alive_requests(2).await;
    let root = tempfile::tempdir().expect("root");
    let client = CatalogClient::new(StorageCoordinator::new(root.path()), &url).expect("client");
    for expected in 1..=2 {
        let body = timeout(
            Duration::from_secs(10),
            client.fetch(&url, MAX_CATALOG, &CancellationToken::new()),
        )
        .await
        .expect("fetch must finish in time")
        .expect("fetch succeeds");
        assert_eq!(
            parse_catalog(&body).expect("catalog body").sequence,
            expected,
            "each response must match its own request"
        );
    }
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "keep-alive connection must be reused across fetches"
    );
    join_server(server).await;
}

/// 目录格式错误时保留已有缓存，只把本次刷新标记为 stale。
#[tokio::test]
async fn invalid_catalog_body_preserves_cache() {
    let (url, server) = serve_once("200 OK", "not json").await;
    let root = tempfile::tempdir().expect("root");
    let client = CatalogClient::new(StorageCoordinator::new(root.path()), &url).expect("client");
    let source = state(&client);
    client
        .persist(&source, &cache("fixture", 1, false))
        .expect("seed cache");
    let path = cache_path(root.path(), &source);
    let before = fs::read(&path).expect("read cache");

    let snapshot = client
        .refresh_verified(CancellationToken::new())
        .await
        .expect("stale fallback");
    assert!(snapshot.snapshot.stale);
    assert_eq!(snapshot.identity().sequence, 1);
    assert_eq!(fs::read(&path).expect("cache remains"), before);
    join_server(server).await;
}

/// 下载失败同样只回退到同来源缓存，不因 HTTP 错误清空用户已有目录。
#[tokio::test]
async fn http_error_preserves_cache() {
    let (url, server) = serve_once("500 Internal Server Error", "").await;
    let root = tempfile::tempdir().expect("root");
    let client = CatalogClient::new(StorageCoordinator::new(root.path()), &url).expect("client");
    let source = state(&client);
    client
        .persist(&source, &cache("fixture", 1, false))
        .expect("seed cache");

    let snapshot = client
        .refresh_verified(CancellationToken::new())
        .await
        .expect("stale fallback");
    assert!(snapshot.snapshot.stale);
    assert_eq!(snapshot.identity().sequence, 1);
    join_server(server).await;
}

/// 取消落在等待响应期间时必须上抛取消，不能把已有缓存包装成刷新成功。
#[tokio::test]
async fn cancellation_during_pending_fetch_stays_cancelled() {
    let (url, seen, server) = serve_pending().await;
    let root = tempfile::tempdir().expect("root");
    let client = CatalogClient::new(StorageCoordinator::new(root.path()), &url).expect("client");
    let source = state(&client);
    client
        .persist(&source, &cache("fixture", 1, false))
        .expect("seed cache");

    let cancellation = CancellationToken::new();
    let token = cancellation.clone();
    let (result, ()) = tokio::join!(async { client.refresh_verified(token).await }, async {
        timeout(Duration::from_secs(10), seen)
            .await
            .expect("server must see the request in time")
            .expect("oneshot sender alive");
        cancellation.cancel();
    });
    assert_eq!(
        result
            .expect_err("cancel during pending fetch")
            .public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
    assert_eq!(
        client
            .cached_verified()
            .await
            .expect("read cache")
            .expect("cache present")
            .identity()
            .sequence,
        1
    );
    join_server(server).await;
}

/// 重定向链有硬上限：初始请求加两次跟随即停止，避免被无限跳转拖住。
#[tokio::test]
async fn redirect_limit_stops_after_two_follows() {
    let (url, count, server) = serve_redirects().await;
    let root = tempfile::tempdir().expect("root");
    let client = CatalogClient::new(StorageCoordinator::new(root.path()), &url).expect("client");
    let result = timeout(
        Duration::from_secs(10),
        client.fetch(&url, MAX_CATALOG, &CancellationToken::new()),
    )
    .await
    .expect("fetch must be bounded");
    assert!(result.is_err());
    assert_eq!(count.load(Ordering::SeqCst), 3);
    server.abort();
    let _ = server.await;
}

/// 回退入口以已取消 token 直接驱动：错误与取消同时就绪时必须返回取消。
#[tokio::test]
async fn stale_fallback_yields_to_ready_cancellation() {
    let root = tempfile::tempdir().expect("root");
    let client = CatalogClient::new(
        StorageCoordinator::new(root.path()),
        "http://127.0.0.1:8080/catalog.json",
    )
    .expect("client");
    let source = state(&client);
    client
        .persist(&source, &cache("fixture", 1, false))
        .expect("seed cache");

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = client
        .stale_fallback(
            &source,
            &cancellation,
            AppError::invalid("插件目录响应无效"),
        )
        .await
        .expect_err("ready cancellation must win the fallback");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
    assert_eq!(
        client
            .cached_verified()
            .await
            .expect("read cache")
            .expect("cache present")
            .identity()
            .sequence,
        1
    );
}

/// 回退读取完成时取消才就绪：取消必须同时压过读取错误与 stale 成功。
#[tokio::test]
async fn cancellation_during_read_beats_read_error_and_stale_success() {
    let root = tempfile::tempdir().expect("root");
    let client = CatalogClient::new(
        StorageCoordinator::new(root.path()),
        "http://127.0.0.1:8080/catalog.json",
    )
    .expect("client");
    let source = state(&client);
    client
        .persist(&source, &cache("fixture", 1, false))
        .expect("seed cache");

    let cancellation = CancellationToken::new();
    let read_during = cancellation.clone();
    let read = async move {
        tokio::task::yield_now().await;
        read_during.cancel();
        Err::<Option<VerifiedCatalog>, _>(AppError::infrastructure("读取目录缓存失败"))
    };
    let error = client
        .stale_fallback_with(&cancellation, AppError::invalid("插件目录响应无效"), read)
        .await
        .expect_err("mid-read cancellation must beat the read error");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );

    let cancellation = CancellationToken::new();
    let read_during = cancellation.clone();
    let cached = client.cached_verified().await.expect("read cache");
    let read = async move {
        tokio::task::yield_now().await;
        read_during.cancel();
        Ok::<_, AppError>(cached)
    };
    let error = client
        .stale_fallback_with(&cancellation, AppError::invalid("插件目录响应无效"), read)
        .await
        .expect_err("mid-read cancellation must beat stale success");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
    assert_eq!(
        client
            .cached_verified()
            .await
            .expect("read cache")
            .expect("cache present")
            .identity()
            .sequence,
        1
    );
}

/// persist 结果的最终裁决：成功不因取消改写，未知提交与取消不进缓存回退。
#[tokio::test]
async fn settle_refresh_keeps_commit_success_and_unknown_outcome_out_of_fallback() {
    let root = tempfile::tempdir().expect("root");
    let client = CatalogClient::new(
        StorageCoordinator::new(root.path()),
        "http://127.0.0.1:8080/catalog.json",
    )
    .expect("client");
    let source = state(&client);
    let fresh = client
        .persist(&source, &cache("fixture", 2, false))
        .expect("first");

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let settled = client
        .settle_refresh(&source, &cancellation, Ok(fresh))
        .await
        .expect("committed result must survive cancellation");
    assert_eq!(settled.identity().sequence, 2);
    assert!(!settled.snapshot.stale);

    let unknown = || {
        AppError::outcome_unknown_with_source(
            "目录缓存提交结果未知",
            std::io::Error::other("join lost"),
        )
    };
    for cancelled in [false, true] {
        let cancellation = CancellationToken::new();
        if cancelled {
            cancellation.cancel();
        }
        let error = client
            .settle_refresh(&source, &cancellation, Err(unknown()))
            .await
            .expect_err("unknown outcome must never become stale success");
        assert_eq!(
            error.public_code(),
            kerminal_application::PublicErrorCode::OutcomeUnknown
        );
        assert!(std::error::Error::source(&error).is_some());
    }

    let empty_root = tempfile::tempdir().expect("empty root");
    let empty = CatalogClient::new(
        StorageCoordinator::new(empty_root.path()),
        "http://127.0.0.1:8080/catalog.json",
    )
    .expect("empty client");
    let error = empty
        .settle_refresh(
            &state(&empty),
            &CancellationToken::new(),
            Err(AppError::invalid("插件目录响应无效")),
        )
        .await
        .expect_err("fallback without cache returns the original error");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Invalid
    );
}

/// persist 任务失联必须按未知结果上报，不得落入缓存回退。
#[tokio::test]
async fn lost_persist_task_reports_unknown_outcome_instead_of_stale_fallback() {
    let root = tempfile::tempdir().expect("root");
    let client = CatalogClient::new(
        StorageCoordinator::new(root.path()),
        "http://127.0.0.1:8080/catalog.json",
    )
    .expect("client");
    let source = state(&client);
    client
        .persist(&source, &cache("fixture", 1, false))
        .expect("seed cache");

    let join = tokio::task::spawn_blocking(|| -> AppResult<VerifiedCatalog> {
        panic!("persist task lost");
    });
    let join_error = join.await.expect_err("panicking task must yield JoinError");
    assert!(join_error.is_panic());
    let mapped = super::persist_result_lost(join_error);
    assert_eq!(
        mapped.public_code(),
        kerminal_application::PublicErrorCode::OutcomeUnknown
    );
    assert!(std::error::Error::source(&mapped).is_some());

    let join = tokio::spawn(async { std::future::pending::<AppResult<VerifiedCatalog>>().await });
    join.abort();
    let join_error = join.await.expect_err("aborted task must yield JoinError");
    assert!(join_error.is_cancelled());
    let mapped = super::persist_result_lost(join_error);
    assert_eq!(
        mapped.public_code(),
        kerminal_application::PublicErrorCode::OutcomeUnknown
    );

    assert_eq!(
        client
            .cached_verified()
            .await
            .expect("read cache")
            .expect("cache present")
            .identity()
            .sequence,
        1
    );
}

/// 下载期间切换来源：旧刷新不得写入新来源缓存，也不得改写旧来源已有缓存。
#[tokio::test]
async fn refresh_during_source_switch_does_not_pollute_new_source() {
    let body = raw_catalog("fixture", 2, false);
    let (url_a, seen, release, server) = serve_after_release(&body).await;
    let url_b = "https://downloads.example.com/b.json";
    let root = tempfile::tempdir().expect("root");
    let client = CatalogClient::new(StorageCoordinator::new(root.path()), &url_a).expect("client");
    let source_a = state(&client);
    client
        .persist(&source_a, &cache("fixture", 1, false))
        .expect("seed cache");
    let before = fs::read(cache_path(root.path(), &source_a)).expect("read cache");

    let (result, ()) = tokio::join!(
        async { client.refresh_verified(CancellationToken::new()).await },
        async {
            timeout(Duration::from_secs(10), seen)
                .await
                .expect("server must see the request in time")
                .expect("oneshot sender alive");
            client.set_source_url(url_b).expect("switch source");
            release.send(()).expect("release server");
        }
    );
    assert_eq!(
        result
            .expect_err("switch during refresh must conflict")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    assert_eq!(client.source_url(), url_b);
    assert!(
        !cache_path(root.path(), &state(&client)).exists(),
        "old refresh must not write new source cache"
    );
    assert_eq!(
        fs::read(cache_path(root.path(), &source_a)).expect("old cache remains"),
        before,
        "old refresh must not rewrite old cache"
    );
    join_server(server).await;
}

/// 缓存读取期间切换来源：旧来源无缓存时不得把“尚未加载”当成新来源结果。
#[tokio::test]
async fn readme_switch_during_cache_read_conflicts_instead_of_missing_cache() {
    let root = tempfile::tempdir().expect("root");
    let url_a = "http://127.0.0.1:8080/a.json";
    let url_b = "https://downloads.example.com/b.json";
    let client = CatalogClient::new(StorageCoordinator::new(root.path()), url_a).expect("client");
    let id = PluginId::parse("acme.tool").expect("plugin id");
    let mut readme = std::pin::pin!(client.readme(&id, CancellationToken::new()));
    switch_source_mid_read(&client, readme.as_mut(), url_b);
    assert_eq!(
        readme
            .await
            .expect_err("old source has no cache")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
}

/// 旧来源有缓存但缺目标条目时同样不能冒充新来源的“条目不存在”。
#[tokio::test]
async fn readme_switch_during_cache_read_conflicts_instead_of_missing_entry() {
    let root = tempfile::tempdir().expect("root");
    let url_a = "http://127.0.0.1:8080/a.json";
    let url_b = "https://downloads.example.com/b.json";
    let client = CatalogClient::new(StorageCoordinator::new(root.path()), url_a).expect("client");
    let source_a = state(&client);
    client
        .persist(
            &source_a,
            &cache_with_readme("fixture", 1, "http://127.0.0.1:8080/readme.md"),
        )
        .expect("seed cache");
    let id = PluginId::parse("other.tool").expect("plugin id");
    let mut readme = std::pin::pin!(client.readme(&id, CancellationToken::new()));
    switch_source_mid_read(&client, readme.as_mut(), url_b);
    assert_eq!(
        readme
            .await
            .expect_err("old source has no such entry")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
}

/// 切换来源后不应为旧来源的 README 地址发出网络请求。
#[tokio::test]
async fn readme_switch_during_cache_read_skips_old_readme_fetch() {
    let readme_url = closed_loopback_readme_url().await;
    let root = tempfile::tempdir().expect("root");
    let url_a = "http://127.0.0.1:8080/a.json";
    let url_b = "https://downloads.example.com/b.json";
    let client = CatalogClient::new(StorageCoordinator::new(root.path()), url_a).expect("client");
    let source_a = state(&client);
    client
        .persist(&source_a, &cache_with_readme("fixture", 1, &readme_url))
        .expect("seed cache");
    let id = PluginId::parse("acme.tool").expect("plugin id");
    let mut readme = std::pin::pin!(client.readme(&id, CancellationToken::new()));
    switch_source_mid_read(&client, readme.as_mut(), url_b);
    assert_eq!(
        readme
            .await
            .expect_err("old readme must not be fetched")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
}

/// Windows 替换被外部句柄阻塞时保留暂存与原缓存，调用方可先回读再决定后续动作。
#[cfg(windows)]
#[test]
fn blocked_replacement_preserves_staging_for_unknown_commit() {
    use std::os::windows::fs::OpenOptionsExt;

    let root = tempfile::tempdir().expect("root");
    let client = CatalogClient::new(
        StorageCoordinator::new(root.path()),
        "http://127.0.0.1:8080/catalog.json",
    )
    .expect("client");
    let source = state(&client);
    client
        .persist(&source, &cache("fixture", 1, false))
        .expect("first");
    let directory = root.path().join("plugins/catalog-cache");
    let path = cache_path(root.path(), &source);
    let _external_reader = fs::OpenOptions::new()
        .read(true)
        .share_mode(3)
        .open(&path)
        .expect("reader without delete sharing");

    let error = client
        .persist(&source, &cache("fixture", 2, false))
        .expect_err("blocked replacement");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::OutcomeUnknown
    );
    let platform = std::error::Error::source(&error)
        .and_then(std::error::Error::source)
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .expect("platform error remains available");
    assert_eq!(platform.raw_os_error(), Some(32));
    let current = client
        .read_cache_at(&path)
        .expect("readback")
        .expect("cache");
    assert_eq!(parse_catalog(&current.raw).expect("current").sequence, 1);

    let staged: Vec<_> = fs::read_dir(&directory)
        .expect("cache directory")
        .map(|entry| entry.expect("entry").path())
        .filter(|entry| entry != &path)
        .collect();
    assert_eq!(
        staged.len(),
        1,
        "unknown commit must retain one staging file"
    );
    let pending = client
        .read_cache_at(&staged[0])
        .expect("staged readback")
        .expect("staged cache");
    assert_eq!(parse_catalog(&pending.raw).expect("staged").sequence, 2);
}
