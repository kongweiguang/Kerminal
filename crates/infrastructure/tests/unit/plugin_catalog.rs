//! 签名目录及缓存防回退的隔离回归。
//! @author kongweiguang

use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use blake2::{Blake2b512, Digest as _};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::Value;

impl CatalogClient {
    /// 单元测试使用一次性密钥和本地服务，不修改环境变量或正式信任根。
    /// 可见范围限于插件模块，以供安装流程及恢复等兄弟测试复用。
    ///
    /// @author kongweiguang
    pub(in crate::plugins) fn testing(
        storage: StorageCoordinator,
        url: String,
        key: String,
    ) -> Self {
        Self::testing_on_track(storage, CatalogTrack::OfficialV1, url, key)
    }

    /// 需要覆盖轨道身份时显式指定轨道；默认 `testing` 继续固定兼容轨，
    /// 避免既有安装测试因为新增轨道而悄悄改变来源标识。
    ///
    /// @author kongweiguang
    pub(in crate::plugins) fn testing_on_track(
        storage: StorageCoordinator,
        track: CatalogTrack,
        url: String,
        key: String,
    ) -> Self {
        Self {
            storage,
            url,
            key,
            allow_loopback: true,
            track,
        }
    }
}

/// 每次运行只在内存生成测试私钥，模拟真实目录签名而不接触正式信任根。
fn signing_key() -> SigningKey {
    let mut seed = [0u8; 32];
    seed[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    seed[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    SigningKey::from_bytes(&seed)
}

/// 复用安装流程测试的 Minisign 编码，确保目录测试与真实验签路径使用同一格式。
fn public_key(key: &SigningKey) -> String {
    let mut public = b"Ed".to_vec();
    public.extend_from_slice(b"test-key");
    public.extend_from_slice(key.verifying_key().as_bytes());
    STANDARD.encode(public)
}

/// 生成预哈希 Minisign 签名；签名只覆盖 raw 原文，与宿主验签参数保持一致。
fn sign(key: &SigningKey, raw: &str) -> String {
    let signature = key.sign(&Blake2b512::digest(raw.as_bytes())).to_bytes();
    let mut payload = b"ED".to_vec();
    payload.extend_from_slice(b"test-key");
    payload.extend_from_slice(&signature);
    let comment = "isolated";
    let mut global = signature.to_vec();
    global.extend_from_slice(comment.as_bytes());
    let global_signature = key.sign(&global).to_bytes();
    format!(
        "untrusted comment: isolated\n{}\ntrusted comment: {}\n{}\n",
        STANDARD.encode(payload),
        comment,
        STANDARD.encode(global_signature)
    )
}

/// 轨道专属原文由调用方给出序列和空白差异，便于验证来源绑定和原始字节摘要。
fn cache(key: &SigningKey, track: CatalogTrack, sequence: u64, spaced: bool) -> Cache {
    let raw = if spaced {
        format!(
            "{{ \"schema\":1, \"source_id\":\"{}\", \"sequence\":{sequence}, \"plugins\":[]}}",
            track.source_id()
        )
    } else {
        format!(
            "{{\"schema\":1,\"source_id\":\"{}\",\"sequence\":{sequence},\"plugins\":[]}}",
            track.source_id()
        )
    };
    Cache {
        signature: sign(key, &raw),
        raw,
        fetched_at: 1,
    }
}

/// 构造带单个平台资产的签名目录：每个 URL 生成一个 any 平台资产条目。
///
/// 字段集合与 application 的 `PluginCatalog` 投影逐一对齐（serde 会拒绝未知字段），
/// sha256 用 64 个十六进制占位字符，让目录校验只在资产 URL 语法这一处产生分歧，
/// 从而把测试结论锚定在轨道 URL 边界上。
fn cache_with_assets(key: &SigningKey, track: CatalogTrack, sequence: u64, urls: &[&str]) -> Cache {
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
        r#"{{"schema":1,"source_id":"{}","sequence":{sequence},"plugins":[{{"id":"acme.tool","name":{{"en":"Tool"}},"description":{{"en":"Tool"}},"publisher":"acme","categories":["tool"],"icon_url":null,"readme_url":null,"repository":"https://github.com/acme/tool","permissions":[],"versions":[{{"version":"1.0.0","api":"*","published_at":"2026-01-01T00:00:00Z","changelog_url":null,"assets":[{}]}}]}}]}}"#,
        track.source_id(),
        assets.join(",")
    );
    Cache {
        signature: sign(key, &raw),
        raw,
        fetched_at: 1,
    }
}

/// 固定测试密钥只覆盖离线签名样本，与正式信任根完全隔离。
fn fixture() -> (tempfile::TempDir, CatalogClient, Vec<Cache>) {
    let root = tempfile::tempdir().expect("temporary root");
    let document: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/plugin-catalog-p5.json"
    ))
    .expect("fixture JSON");
    let client = CatalogClient::testing_on_track(
        StorageCoordinator::new(root.path()),
        CatalogTrack::OfficialV1,
        "http://127.0.0.1/catalog.json".into(),
        document["publicKey"].as_str().expect("key").into(),
    );
    let caches = document["fixtures"]
        .as_array()
        .expect("fixtures")
        .iter()
        .map(|entry| Cache {
            raw: entry["raw"].as_str().expect("raw").into(),
            signature: entry["signature"].as_str().expect("signature").into(),
            fetched_at: 1,
        })
        .collect();
    (root, client, caches)
}

/// 两轨的地址、来源标识和缓存文件必须同时不同，任一项复用都会让跨轨内容互相覆盖。
#[test]
fn tracks_keep_distinct_identity_and_cache_paths() {
    assert_eq!(CatalogTrack::OfficialV1.source_id(), "official");
    assert_eq!(CatalogTrack::OfficialV2.source_id(), "official-v2");
    assert_eq!(CatalogTrack::OfficialV1.cache_file(), "official.json");
    assert_eq!(CatalogTrack::OfficialV2.cache_file(), "official-v2.json");
    assert_ne!(
        CatalogTrack::OfficialV1.catalog_url(),
        CatalogTrack::OfficialV2.catalog_url()
    );
    assert!(
        CatalogTrack::OfficialV2
            .catalog_url()
            .contains("catalog-v2")
    );

    let root = tempfile::tempdir().expect("temporary root");
    let storage = StorageCoordinator::new(root.path());
    assert_eq!(
        CatalogClient::official(storage.clone()).track(),
        CatalogTrack::OfficialV1
    );
    assert_eq!(
        CatalogClient::for_track(storage, CatalogTrack::OfficialV2).track(),
        CatalogTrack::OfficialV2
    );
}

/// 同一份合法签名原文被复制到另一轨时必须拒绝，防止跨轨重放绕过来源绑定。
#[test]
fn cross_track_replay_is_rejected() {
    let root = tempfile::tempdir().expect("temporary root");
    let storage = StorageCoordinator::new(root.path());
    let key = signing_key();
    let public = public_key(&key);
    let v1 = CatalogClient::testing_on_track(
        storage.clone(),
        CatalogTrack::OfficialV1,
        "http://127.0.0.1/catalog.json".into(),
        public.clone(),
    );
    let v2 = CatalogClient::testing_on_track(
        storage,
        CatalogTrack::OfficialV2,
        "http://127.0.0.1/catalog.json".into(),
        public,
    );

    let v1_cache = cache(&key, CatalogTrack::OfficialV1, 1, false);
    let v2_cache = cache(&key, CatalogTrack::OfficialV2, 1, false);
    assert!(v1.persist(&v1_cache).is_ok());
    assert!(v2.persist(&v2_cache).is_ok());
    // 签名本身有效，拒绝只能来自来源标识与轨道不一致。
    assert_eq!(
        v1.persist(&v2_cache).expect_err("v2 replay").public_code(),
        kerminal_application::PublicErrorCode::Invalid
    );
    assert_eq!(
        v2.persist(&v1_cache).expect_err("v1 replay").public_code(),
        kerminal_application::PublicErrorCode::Invalid
    );
}

/// 两轨缓存文件和序列彼此独立；一轨的高序列不能让另一轨的低序列被判成回退。
#[test]
fn tracks_keep_independent_caches_and_sequences() {
    let root = tempfile::tempdir().expect("temporary root");
    let storage = StorageCoordinator::new(root.path());
    let key = signing_key();
    let public = public_key(&key);
    let v1 = CatalogClient::testing_on_track(
        storage.clone(),
        CatalogTrack::OfficialV1,
        "http://127.0.0.1/catalog.json".into(),
        public.clone(),
    );
    let v2 = CatalogClient::testing_on_track(
        storage.clone(),
        CatalogTrack::OfficialV2,
        "http://127.0.0.1/catalog.json".into(),
        public,
    );

    assert_eq!(
        v1.persist(&cache(&key, CatalogTrack::OfficialV1, 9, false))
            .expect("v1 sequence 9")
            .identity
            .sequence,
        9
    );
    assert_eq!(
        v2.persist(&cache(&key, CatalogTrack::OfficialV2, 1, false))
            .expect("v2 sequence 1")
            .identity
            .sequence,
        1
    );
    // 同轨回退和同序列不同原文仍必须拒绝。
    assert_eq!(
        v2.persist(&cache(&key, CatalogTrack::OfficialV2, 0, false))
            .expect_err("v2 rollback")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    assert_eq!(
        v2.persist(&cache(&key, CatalogTrack::OfficialV2, 1, true))
            .expect_err("v2 duplicate sequence")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    // 兼容轨本身继续接受更高序列，证明两条序列没有互相污染。
    assert!(
        v1.persist(&cache(&key, CatalogTrack::OfficialV1, 10, false))
            .is_ok()
    );

    let directory = storage.root().join("plugins/catalog-cache");
    assert!(directory.join("official.json").is_file());
    assert!(directory.join("official-v2.json").is_file());
}

/// 身份锚点必须来自验签通过的原始字节；同一语义、不同排版必须得到不同摘要。
///
/// 两份原文的 source_id 和 sequence 完全相同，只有字段间空白不同，语义上等价——
/// 通过 serde_json 解析后再比较可以证明这一点。若摘要由结构体重新序列化生成，
/// 两者会折叠成同一摘要，安装冻结就无法区分用户看到的是哪一份签名原文，所以
/// 这里显式要求摘要不同，并用直接验签证明两份原文各自独立成立。
#[test]
fn identity_digest_binds_verified_raw_bytes() {
    let key = signing_key();
    // 同一 source_id 与 sequence，只有字段间空白不同：语义等价、字节不同。
    let compact_raw =
        r#"{"schema":1,"source_id":"official-v2","sequence":2,"plugins":[]}"#.to_string();
    let spaced_raw =
        r#"{ "schema":1, "source_id":"official-v2", "sequence":2, "plugins":[] }"#.to_string();
    let compact = Cache {
        signature: sign(&key, &compact_raw),
        raw: compact_raw,
        fetched_at: 1,
    };
    let spaced = Cache {
        signature: sign(&key, &spaced_raw),
        raw: spaced_raw,
        fetched_at: 1,
    };

    // 两份原文解析后语义完全一致，证明差异只在排版。
    // CatalogPlugin 只 derive Clone/Debug/Deserialize/Serialize，没有 PartialEq；
    // 样本两份 plugins 都是空数组，逐字段比较长度与元素数即可证明等价，
    // 不为测试扩大生产类型的派生 API。
    let compact_catalog: PluginCatalog =
        serde_json::from_str(&compact.raw).expect("compact parses");
    let spaced_catalog: PluginCatalog = serde_json::from_str(&spaced.raw).expect("spaced parses");
    assert_eq!(compact_catalog.source_id, spaced_catalog.source_id);
    assert_eq!(compact_catalog.sequence, spaced_catalog.sequence);
    assert_eq!(compact_catalog.plugins.len(), spaced_catalog.plugins.len());
    assert!(compact_catalog.plugins.is_empty());
    assert!(spaced_catalog.plugins.is_empty());

    // 各自的签名对各自原文有效，说明两份都是合法的独立签名产物。
    let public = public_key(&key);
    // TempDir 必须绑定变量传 `.path()`：TempDir 只有 AsRef<Path>，不满足
    // StorageCoordinator::new 的 Into<PathBuf>，内联传递既无法编译也会丢清理句柄。
    let verify_root = tempfile::tempdir().expect("verify root");
    let verifier = CatalogClient::testing_on_track(
        StorageCoordinator::new(verify_root.path()),
        CatalogTrack::OfficialV2,
        "http://127.0.0.1/catalog.json".into(),
        public,
    );
    verifier.verify(&compact).expect("compact signature");
    verifier.verify(&spaced).expect("spaced signature");

    // 摘要必须逐字节对应各自原文，不能被重新序列化抹平。
    assert_eq!(
        CatalogCacheIdentity::from_verified(&compact, &compact_catalog).raw_sha256,
        sha256_hex(compact.raw.as_bytes())
    );
    assert_eq!(
        CatalogCacheIdentity::from_verified(&spaced, &spaced_catalog).raw_sha256,
        sha256_hex(spaced.raw.as_bytes())
    );
    assert_ne!(
        CatalogCacheIdentity::from_verified(&compact, &compact_catalog).raw_sha256,
        CatalogCacheIdentity::from_verified(&spaced, &spaced_catalog).raw_sha256
    );
}

/// 损坏缓存继续返回真实错误，不能被当成“没有缓存”后静默重新安装。
#[tokio::test]
async fn corrupt_track_cache_reports_error() {
    let root = tempfile::tempdir().expect("temporary root");
    let storage = StorageCoordinator::new(root.path());
    let key = signing_key();
    let public = public_key(&key);
    let v2 = CatalogClient::testing_on_track(
        storage.clone(),
        CatalogTrack::OfficialV2,
        "http://127.0.0.1/catalog.json".into(),
        public,
    );
    v2.persist(&cache(&key, CatalogTrack::OfficialV2, 1, false))
        .expect("seed cache");
    let path = storage
        .root()
        .join("plugins/catalog-cache/official-v2.json");
    fs::write(&path, b"not json").expect("corrupt cache");
    // 读取路径必须把损坏如实上报，而不是返回 None 让上层当成首次安装。
    let error = v2
        .cached_verified()
        .await
        .expect_err("corrupt cache must fail");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Invalid
    );

    // 损坏的本轨缓存让下一次提交也无法比较序列，必须报错而不是覆盖原文。
    let error = v2
        .persist(&cache(&key, CatalogTrack::OfficialV2, 2, false))
        .expect_err("corrupt cache blocks persist");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Invalid
    );
    assert_eq!(
        fs::read(&path).expect("cache remains"),
        b"not json".to_vec()
    );
    // 兼容轨缓存文件没有被创建，证明损坏只影响本轨。
    assert!(
        !storage
            .root()
            .join("plugins/catalog-cache/official.json")
            .exists()
    );
}

/// 已取消的刷新必须原样返回取消，不能把已有 stale 缓存伪装成这次刷新成功。
///
/// 这里特意预置了一份可用的本轨缓存：若取消错误被回退分支吞掉，调用方会拿到
/// “看似成功的 stale 快照”，把用户已撤销的刷新当成完成，后续安装就可能基于
/// 未经确认的旧目录继续执行。取消优先级必须高于缓存回退。
///
/// 提交阶段（persist）是同步磁盘操作，进入后无法响应取消；取消前已完成的提交会
/// 以成功结果如实返回，这是记录在 `refresh_verified`/`persist` 注释中的已知边界，
/// 不是“已提交却报告取消”。
#[tokio::test]
async fn cancelled_refresh_does_not_fall_back_to_cache() {
    let root = tempfile::tempdir().expect("temporary root");
    let key = signing_key();
    let client = CatalogClient::testing_on_track(
        StorageCoordinator::new(root.path()),
        CatalogTrack::OfficialV2,
        "http://127.0.0.1/catalog.json".into(),
        public_key(&key),
    );
    client
        .persist(&cache(&key, CatalogTrack::OfficialV2, 1, false))
        .expect("seed cache");

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = client
        .refresh_verified(cancellation)
        .await
        .expect_err("cancelled refresh");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
    let cached = client
        .cached_verified()
        .await
        .expect("read cache")
        .expect("cache present");
    assert_eq!(cached.identity.sequence, 1);
}

/// 刷新失败只能回退到本轨已验签缓存，不能借用另一轨内容。
///
/// 用一个被 URL 边界直接拒绝的地址制造确定失败，测试不依赖外网、端口占用或超时；
/// 若回退实现误读另一轨文件，这里会看到错误的来源或序列，从而暴露跨轨兜底。
#[tokio::test]
async fn failed_refresh_falls_back_only_to_same_track_cache() {
    let root = tempfile::tempdir().expect("temporary root");
    let storage = StorageCoordinator::new(root.path());
    let key = signing_key();
    let public = public_key(&key);
    let url = "https://catalog.invalid/catalog.json".to_string();
    let v1 = CatalogClient::testing_on_track(
        storage.clone(),
        CatalogTrack::OfficialV1,
        url.clone(),
        public.clone(),
    );
    let v2 = CatalogClient::testing_on_track(storage, CatalogTrack::OfficialV2, url, public);
    v1.persist(&cache(&key, CatalogTrack::OfficialV1, 5, false))
        .expect("seed v1");
    v2.persist(&cache(&key, CatalogTrack::OfficialV2, 1, false))
        .expect("seed v2");

    let snapshot = v2
        .refresh_verified(CancellationToken::new())
        .await
        .expect("stale fallback");
    assert!(snapshot.snapshot.stale);
    assert_eq!(snapshot.identity.source_id, "official-v2");
    assert_eq!(snapshot.identity.sequence, 1);
}

/// 取消在第二次 fetch 仍在等待响应时到达：结果必须是取消，缓存保持原状。
///
/// 与其名场面相反，这里不声称“错误已返回、取消与回退竞争”——那个交叠只能由
/// `stale_fallback_with` 的注入测试确定地覆盖（既有用例保留）。端到端能确定
/// 验证的真实场景是：第一连接成功返回 catalog（`Connection: close`，客户端
/// 据此关闭并新建第二连接），第二连接收到 `.minisig` 请求后在返回任何响应
/// 之前经 oneshot 通知测试；测试取消 token，此时 fetch 还停在它的取消选择点，
/// 取消必然从那里上抛，绝不落入缓存回退。断言三者：结果是 Cancelled、缓存
/// 序列未变、受控 server 正常退出（所有等待都有界，失败也不悬挂）。
///
/// 局限如实说明：测试函数自身因 panic/断言失败退出时会取消 service task，
/// 此后无法再保证“服务器已回收”的陈述；断言失败路径不做此类声称，只在
/// 正常路径 await 确认。实现不依赖 sleep 对齐竞态。
#[tokio::test]
async fn cancellation_during_pending_minisig_fetch_stays_cancelled() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::{Duration, timeout},
    };

    let root = tempfile::tempdir().expect("temporary root");
    let storage = StorageCoordinator::new(root.path());
    let key = signing_key();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("local addr");
    // oneshot 有记忆性且单次消费，不存在 Notify 在 notified() 注册前发通知
    // 导致丢失的问题；测试端拿到 permit 的时机就是服务端看到 minisig 请求。
    let (minisig_seen_tx, minisig_seen_rx) = tokio::sync::oneshot::channel::<()>();
    let mut server = tokio::spawn(async move {
        // 读到完整请求头或确定失败；EOF/读错/超限立即 panic 结束任务，
        // 绝不把空切片追加回循环造成忙等。
        let (mut stream, _) = listener.accept().await.expect("accept catalog");
        let mut bytes = Vec::new();
        let mut part = [0u8; 1024];
        loop {
            let n = stream
                .read(&mut part)
                .await
                .expect("read catalog request must not error");
            assert!(n > 0, "catalog client disconnected before full request");
            bytes.extend_from_slice(&part[..n]);
            assert!(
                bytes.len() <= 64 * 1024,
                "catalog request exceeds header bound"
            );
            if String::from_utf8_lossy(&bytes).contains("\r\n\r\n") {
                break;
            }
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .expect("write catalog");
        drop(stream);

        // 第二连接：读到 minisig 请求行后在返回任何响应之前通知测试——
        // 客户端此刻确定停在 fetch 的取消选择点等待响应。
        let (mut stream, _) = listener.accept().await.expect("accept minisig");
        let mut bytes = Vec::new();
        loop {
            let n = stream
                .read(&mut part)
                .await
                .expect("read minisig request must not error");
            assert!(n > 0, "minisig client disconnected before full request");
            bytes.extend_from_slice(&part[..n]);
            assert!(
                bytes.len() <= 64 * 1024,
                "minisig request exceeds header bound"
            );
            let request = String::from_utf8_lossy(&bytes);
            if request.contains("\r\n\r\n") {
                assert!(
                    request
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .ends_with(".minisig"),
                    "second request must target the minisig path"
                );
                break;
            }
        }
        let _ = minisig_seen_tx.send(());
        // 不写任何响应，保持连接到测试结束；测试断言后回收本任务并断连。
        // 等待客户端侧取消完成即可退出：客户端取消后 fetch 被丢弃，连接随之关闭。
        let _ = timeout(Duration::from_secs(10), stream.read(&mut part)).await;
    });

    let client = CatalogClient::testing_on_track(
        storage.clone(),
        CatalogTrack::OfficialV2,
        format!("http://{address}/catalog.json"),
        public_key(&key),
    );
    client
        .persist(&cache(&key, CatalogTrack::OfficialV2, 1, false))
        .expect("seed cache");

    // 客户端发出第二次请求（服务端确认已收到）后取消：结果确定是取消。
    let cancellation = CancellationToken::new();
    let client = std::sync::Arc::new(client);
    let token = cancellation.clone();
    // join! 的取消分支返回 ()，解构显式匹配；取消语义由下方 result 断言承担。
    let (result, ()) = tokio::join!(async { client.refresh_verified(token).await }, async {
        // 通知等待本身有界：服务端异常退出时测试报错而不是悬挂。
        timeout(Duration::from_secs(10), minisig_seen_rx)
            .await
            .expect("server must see the minisig request in time")
            .expect("oneshot sender alive");
        cancellation.cancel();
    });
    let error = result.expect_err("cancel during pending fetch must stay cancelled");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
    // 取消未改写缓存：内容仍是种子序列，后续读取保持已验签原状。
    let cached = client
        .cached_verified()
        .await
        .expect("read cache")
        .expect("cache present");
    assert_eq!(cached.identity.sequence, 1);

    // 正常路径最后回收受控 server。timeout 返回 Ok 时 JoinHandle 已完成，
    // 首次 join_result 就是唯一一次轮询的结果，直接检查它，绝不再 await
    //（二次轮询已完成的 JoinHandle 触发 Tokio 的 polled-after-completion
    // panic，还会丢失首次结果里的 panic 信息）。Err 时 abort 后再有界等待
    // 一次：只要首次超时，测试就必须失败——内层无论任务超时后才完成（正常
    // 结束或 panic）都显式 panic，再次超时则报告终止未确认，绝不静默通过。
    // 此前的断言若已失败，测试退出会取消任务，局限已在文档注释声明。
    match timeout(Duration::from_secs(10), &mut server).await {
        Ok(join_result) => join_result.expect("server task must not panic"),
        Err(_) => {
            server.abort();
            match timeout(Duration::from_secs(10), &mut server).await {
                Ok(Ok(())) => panic!("server task finished only after the timeout elapsed"),
                Ok(Err(join_error)) => {
                    panic!("server task aborted after timeout; join error: {join_error}")
                }
                Err(_) => panic!("server task did not terminate after abort; state unconfirmed"),
            }
        }
    }
}

/// 第二连接确实返回 404 且全程无取消：刷新按普通失败回退到本轨 stale 缓存。
///
/// 与上一个用例互补：证明同样的两连接服务在无取消时走“错误 → 回退”路径，
/// 返回带 `stale = true` 的种子快照。这把“回退真实可达”与“取消优先”分开
/// 钉住；错误送达客户端与回退交叠的极端时序仍由注入测试覆盖，端到端不声称。
#[tokio::test]
async fn minisig_404_without_cancellation_returns_stale_fallback() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::{Duration, timeout},
    };

    let root = tempfile::tempdir().expect("temporary root");
    let storage = StorageCoordinator::new(root.path());
    let key = signing_key();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("local addr");
    let (minisig_done_tx, minisig_done_rx) = tokio::sync::oneshot::channel::<()>();
    let mut server = tokio::spawn(async move {
        // 读请求头的循环对 EOF/读错/超限立即失败，绝不忙等。
        let (mut stream, _) = listener.accept().await.expect("accept catalog");
        let mut bytes = Vec::new();
        let mut part = [0u8; 1024];
        loop {
            let n = stream
                .read(&mut part)
                .await
                .expect("read catalog request must not error");
            assert!(n > 0, "catalog client disconnected before full request");
            bytes.extend_from_slice(&part[..n]);
            assert!(
                bytes.len() <= 64 * 1024,
                "catalog request exceeds header bound"
            );
            if String::from_utf8_lossy(&bytes).contains("\r\n\r\n") {
                break;
            }
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .expect("write catalog");
        drop(stream);

        // 第二连接：minisig 404 写回并关闭，随后通知测试响应已发出。
        let (mut stream, _) = listener.accept().await.expect("accept minisig");
        let mut bytes = Vec::new();
        loop {
            let n = stream
                .read(&mut part)
                .await
                .expect("read minisig request must not error");
            assert!(n > 0, "minisig client disconnected before full request");
            bytes.extend_from_slice(&part[..n]);
            assert!(
                bytes.len() <= 64 * 1024,
                "minisig request exceeds header bound"
            );
            if String::from_utf8_lossy(&bytes).contains("\r\n\r\n") {
                break;
            }
        }
        stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .expect("write 404");
        drop(stream);
        let _ = minisig_done_tx.send(());
    });

    let client = CatalogClient::testing_on_track(
        storage.clone(),
        CatalogTrack::OfficialV2,
        format!("http://{address}/catalog.json"),
        public_key(&key),
    );
    client
        .persist(&cache(&key, CatalogTrack::OfficialV2, 1, false))
        .expect("seed cache");

    let snapshot = timeout(
        Duration::from_secs(10),
        client.refresh_verified(CancellationToken::new()),
    )
    .await
    .expect("refresh must finish in time")
    .expect("404 falls back to stale cache");
    // 服务端确认响应已发出。
    timeout(Duration::from_secs(10), minisig_done_rx)
        .await
        .expect("server must send minisig response in time")
        .expect("oneshot sender alive");

    assert!(snapshot.snapshot.stale);
    assert_eq!(snapshot.identity.source_id, "official-v2");
    assert_eq!(snapshot.identity.sequence, 1);

    // 正常路径最后回收受控 server；语义与上一个用例的回收块一致：首次
    // join_result 直接检查，Err 才 abort 后再有界等待一次，内层完成一律
    // 显式失败，终止未确认不吞掉，首次超时绝不让测试静默通过。
    match timeout(Duration::from_secs(10), &mut server).await {
        Ok(join_result) => join_result.expect("server task must not panic"),
        Err(_) => {
            server.abort();
            match timeout(Duration::from_secs(10), &mut server).await {
                Ok(Ok(())) => panic!("server task finished only after the timeout elapsed"),
                Ok(Err(join_error)) => {
                    panic!("server task aborted after timeout; join error: {join_error}")
                }
                Err(_) => panic!("server task did not terminate after abort; state unconfirmed"),
            }
        }
    }
}

/// 回退入口以已取消 token 直接驱动：错误与取消同时就绪时必须返回取消。
///
/// 端到端测试只能覆盖取消落在“错误返回之后、回退读取之中”的时序；取消恰好在
/// 普通错误返回的同一时刻就绪属于同一代码路径，但端到端无法可靠对齐两个事件的
/// 就绪顺序，所以这里绕过网络直接调用 `stale_fallback`。这证明回退自身的第一
/// 取消检查点真实生效；读取期间才到达的取消由 `stale_fallback_with` 的注入
/// 测试覆盖，两者缺一不可。
#[tokio::test]
async fn stale_fallback_yields_to_ready_cancellation() {
    let root = tempfile::tempdir().expect("temporary root");
    let key = signing_key();
    let client = CatalogClient::testing_on_track(
        StorageCoordinator::new(root.path()),
        CatalogTrack::OfficialV2,
        "http://127.0.0.1/catalog.json".into(),
        public_key(&key),
    );
    client
        .persist(&cache(&key, CatalogTrack::OfficialV2, 1, false))
        .expect("seed cache");

    // 模拟“普通网络错误已产生，且取消同时已就绪”：错误被作为参数带入，
    // token 已取消。回退必须上抛取消，不能把种子缓存包装成 stale 成功。
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = client
        .stale_fallback(&cancellation, AppError::invalid("插件目录响应无效"))
        .await
        .expect_err("ready cancellation must win the fallback");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
    // 缓存未被改写：内容仍是种子序列。
    let cached = client
        .cached_verified()
        .await
        .expect("read cache")
        .expect("cache present");
    assert_eq!(cached.identity.sequence, 1);
}

/// 回退读取完成时取消才就绪：取消必须同时压过读取错误与读取成功的 stale 返回。
///
/// 直接以预取消 token 调用只会命中 `stale_fallback` 的第一个检查点，根本到不了
/// 读取之后的裁决。这里用 `stale_fallback_with` 注入一个自定义 future：它在
/// `.await` 期间（首次 poll 后）取消 token，然后分别返回 Err 与 Ok，覆盖两条
/// 读取结果路径——
/// 1. 读取返回 Err：旧实现 `.await?` 会在第二次取消检查前直接上抛 invalid，
///    这里断言最终结果是 Cancelled，正是旧缺陷的判别性回归；
/// 2. 读取返回 Ok：取消在读取期间到达时，stale 成功必须让位给取消。
/// 两个场景的初始 token 都未取消，保证第一检查点放行、裁决发生在读取完成之后。
#[tokio::test]
async fn cancellation_during_read_beats_read_error_and_stale_success() {
    let root = tempfile::tempdir().expect("temporary root");
    let key = signing_key();
    let client = CatalogClient::testing_on_track(
        StorageCoordinator::new(root.path()),
        CatalogTrack::OfficialV2,
        "http://127.0.0.1/catalog.json".into(),
        public_key(&key),
    );
    client
        .persist(&cache(&key, CatalogTrack::OfficialV2, 1, false))
        .expect("seed cache");

    // 读取 future：首次被 poll 后取消 token，模拟“取消落在读取执行期间”。
    let cancellation = CancellationToken::new();
    let read_during = cancellation.clone();
    let read = async move {
        tokio::task::yield_now().await;
        read_during.cancel();
        // 情形一：读取本身失败——错误必须让位于读取期间到达的取消。
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

    // 情形二：同样的读取时序但读取成功——stale 成功同样必须让位于取消，
    // 旧实现这里会返回带 stale 的快照，掩盖用户已经撤销刷新的事实。
    // future 输出必须是 `Result<Option<_>, _>`：用 `Ok(cached)` 包一层，
    // 与 `cached_verified` 的真实返回类型对齐。
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
    // 缓存未被改写：内容仍是种子序列。
    let cached = client
        .cached_verified()
        .await
        .expect("read cache")
        .expect("cache present");
    assert_eq!(cached.identity.sequence, 1);
}

/// persist 结果的最终裁决分支：成功不因取消改写、未知提交与取消不进缓存回退。
///
/// 通过 `settle_refresh` 直接注入纯结果，端到端无法在 persist 已产生
/// “成功/未知提交”之后再与取消竞争，这里逐分支钉住优先序：
/// 1. Ok + 已取消 token：提交已发生，必须如实返回新快照，绝不能伪报取消；
/// 2. OutcomeUnknown + 未取消：未知提交原样上抛，不落入 stale 回退；
/// 3. OutcomeUnknown + 已取消：未知提交优先于取消，仍原样上抛，不当取消处理；
/// 4. 普通错误 + 未取消 + 无缓存：回退返回原始错误（stale 成功路径由既有
///    回退测试覆盖）。
#[tokio::test]
async fn settle_refresh_keeps_commit_success_and_unknown_outcome_out_of_fallback() {
    let root = tempfile::tempdir().expect("temporary root");
    let key = signing_key();
    let client = CatalogClient::testing_on_track(
        StorageCoordinator::new(root.path()),
        CatalogTrack::OfficialV2,
        "http://127.0.0.1/catalog.json".into(),
        public_key(&key),
    );
    let fresh = client
        .persist(&cache(&key, CatalogTrack::OfficialV2, 2, false))
        .expect("first");

    // 成功结果 + 已取消 token：提交不能被取消否认。
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let settled = client
        .settle_refresh(&cancellation, Ok(fresh.clone()))
        .await
        .expect("committed result must survive cancellation");
    assert_eq!(settled.identity.sequence, 2);
    assert!(!settled.snapshot.stale);

    // 未知提交（无论取消与否）：原样上抛，绝不变成 stale 成功或取消。
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
            .settle_refresh(&cancellation, Err(unknown()))
            .await
            .expect_err("unknown outcome must never become stale success");
        assert_eq!(
            error.public_code(),
            kerminal_application::PublicErrorCode::OutcomeUnknown
        );
        assert!(std::error::Error::source(&error).is_some());
    }

    // 普通错误且无任何缓存：回退无处可去，只能返回原始错误而不是编造快照。
    // TempDir 只实现 AsRef<Path>，必须绑定变量传 `.path()`（满足 Into<PathBuf>），
    // 并让目录活到测试结束，不能转手 PathBuf 丢掉清理句柄。
    let empty_root = tempfile::tempdir().expect("empty root");
    let empty = CatalogClient::testing_on_track(
        StorageCoordinator::new(empty_root.path()),
        CatalogTrack::OfficialV2,
        "http://127.0.0.1/catalog.json".into(),
        public_key(&key),
    );
    let error = empty
        .settle_refresh(
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

/// persist 任务失联（panic 或 join 中断）必须按未知结果上报，不得落入缓存回退。
///
/// persist 在 `spawn_blocking` 中执行，join 失败只说明调用方拿不到结果：任务
/// 可能在原子提交完成后才 panic，被放弃时阻塞任务也可能仍在写盘。两种情况都
/// 不能证明提交未发生，若按普通 Infrastructure 错误进入 stale fallback，会把
/// “提交结果未知”伪装成确定失败并返回旧缓存。分两层钉住：
/// 1. panic 与 abort 两个真实入口产生的 JoinError 经 `persist_result_lost` 映射
///    后都必须是 OutcomeUnknown，且源错误链保留原始 JoinError；
/// 2. `refresh_verified` 的匹配分支把 OutcomeUnknown 与取消、persist 成功并列
///    为不可回退分支，stale 成功只能来自普通错误（由既有回退测试覆盖）。
/// 注入方式说明：persist 内部依赖的 minisign-verify 0.2.5 在所有切片前都做了
/// 长度校验，没有可用输入能确定性触发 panic；直接 abort 已运行的阻塞任务也不
/// 可靠（任务可能已完成）。所以两个 JoinError 用确定性场景产生：panic 来自
/// 立即失联的阻塞任务，abort 来自停在 await 点的异步任务（必定被取消），
/// 映射函数对两类 JoinError 逐例断言。
#[tokio::test]
async fn lost_persist_task_reports_unknown_outcome_instead_of_stale_fallback() {
    let root = tempfile::tempdir().expect("temporary root");
    let key = signing_key();
    let client = CatalogClient::testing_on_track(
        StorageCoordinator::new(root.path()),
        CatalogTrack::OfficialV2,
        "http://127.0.0.1/catalog.json".into(),
        public_key(&key),
    );
    client
        .persist(&cache(&key, CatalogTrack::OfficialV2, 1, false))
        .expect("seed cache");

    // panic 路径：任务在闭包内立即失联，等价于 persist 在提交前后任意时刻
    // panic；join 得到的 JoinError 就是生产路径被 `persist_result_lost` 映射的
    // 同一输入。
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
    // 源错误链保留原始 JoinError：外围编排需要回读材料，不能只剩安全文案。
    assert!(std::error::Error::source(&mapped).is_some());

    // abort 路径：任务停在 await 点后被放弃，join 被取消不代表提交未发生，
    // 也绝不与刷新取消混淆成 Cancelled。
    let join = tokio::spawn(async { std::future::pending::<AppResult<VerifiedCatalog>>().await });
    join.abort();
    let join_error = join.await.expect_err("aborted task must yield JoinError");
    assert!(join_error.is_cancelled());
    let mapped = super::persist_result_lost(join_error);
    assert_eq!(
        mapped.public_code(),
        kerminal_application::PublicErrorCode::OutcomeUnknown
    );

    // 未知结果不允许触发任何缓存改写或清理：内容仍是种子序列。
    let cached = client
        .cached_verified()
        .await
        .expect("read cache")
        .expect("cache present");
    assert_eq!(cached.identity.sequence, 1);
}

/// 跨轨缓存读取必须彼此不可见：本轨无缓存时不得把另一轨文件当成自己的快照。
///
/// 只写入 V1 缓存文件后，V2 客户端读取必须返回 None 而不是 V1 内容；反之亦然。
/// 未来“新目录失败回退到兼容目录”属于上层组合源的行为，由它显式切轨并保留真实
/// source_id；本层若偷偷读取另一轨，上层回退就失去了“独立验签”的前提。
#[tokio::test]
async fn cross_track_cache_reads_are_isolated() {
    let root = tempfile::tempdir().expect("temporary root");
    let storage = StorageCoordinator::new(root.path());
    let key = signing_key();
    let public = public_key(&key);
    let v1 = CatalogClient::testing_on_track(
        storage.clone(),
        CatalogTrack::OfficialV1,
        "http://127.0.0.1/catalog.json".into(),
        public.clone(),
    );
    let v2 = CatalogClient::testing_on_track(
        storage.clone(),
        CatalogTrack::OfficialV2,
        "http://127.0.0.1/catalog.json".into(),
        public,
    );

    // 只写 V1：V2 读取必须为空，绝不能把 official.json 当成自己的快照。
    v1.persist(&cache(&key, CatalogTrack::OfficialV1, 3, false))
        .expect("seed v1");
    assert!(v2.cached_verified().await.expect("v2 read").is_none());
    // 只写 V2：V1 读取同样必须为空。
    v2.persist(&cache(&key, CatalogTrack::OfficialV2, 1, false))
        .expect("seed v2");
    // 两轨都在后，各自只能看到自己的来源与序列。
    let seen_v1 = v1
        .cached_verified()
        .await
        .expect("v1 read")
        .expect("v1 cache");
    assert_eq!(seen_v1.identity.source_id, "official");
    assert_eq!(seen_v1.identity.sequence, 3);
    let seen_v2 = v2
        .cached_verified()
        .await
        .expect("v2 read")
        .expect("v2 cache");
    assert_eq!(seen_v2.identity.source_id, "official-v2");
    assert_eq!(seen_v2.identity.sequence, 1);
    // 锚点按来源绑定：拿另一轨的 identity 做匹配必须冲突，不能静默通过。
    assert_eq!(
        v1.cached_matching(&seen_v2.identity)
            .await
            .expect_err("cross-track identity")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    assert_eq!(
        v2.cached_matching(&seen_v1.identity)
            .await
            .expect_err("cross-track identity")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
}

/// 兼容轨的 `CatalogSource` 入口继续返回原快照，不因双轨基础改动而改变行为。
#[tokio::test]
async fn official_track_keeps_existing_cached_contract() {
    let (_root, client, caches) = fixture();
    client.persist(&caches[1]).expect("seed");
    let snapshot = client
        .cached()
        .await
        .expect("read cache")
        .expect("cache present");
    assert!(!snapshot.stale);
    assert_eq!(snapshot.catalog.source_id, "official");
    assert_eq!(snapshot.catalog.sequence, 2);
    let verified = client
        .cached_verified()
        .await
        .expect("read verified")
        .expect("cache present");
    assert_eq!(verified.identity.source_id, "official");
    assert_eq!(verified.identity.sequence, 2);
    assert_eq!(
        verified.identity.raw_sha256,
        sha256_hex(caches[1].raw.as_bytes())
    );
}

/// 冻结锚点在缓存刷新后必须拒绝，避免安装按 id/version/platform 静默改选新目录。
///
/// 这里模拟用户看到序列 1 后缓存已推进到序列 2：同 id/version/platform 仍存在，
/// 但来源快照不同，绑定入口只能返回冲突，后续集成据此要求用户重新确认。
#[tokio::test]
async fn frozen_identity_rejects_refreshed_snapshot() {
    let root = tempfile::tempdir().expect("temporary root");
    let storage = StorageCoordinator::new(root.path());
    let key = signing_key();
    let client = CatalogClient::testing_on_track(
        storage,
        CatalogTrack::OfficialV2,
        "http://127.0.0.1/catalog.json".into(),
        public_key(&key),
    );
    let frozen = client
        .persist(&cache(&key, CatalogTrack::OfficialV2, 1, false))
        .expect("first")
        .identity;
    client
        .persist(&cache(&key, CatalogTrack::OfficialV2, 2, false))
        .expect("second");

    assert_eq!(
        client
            .cached_matching(&frozen)
            .await
            .expect_err("stale identity")
            .public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    // 同一份快照重新绑定仍然成功，证明拒绝只来自内容变化而不是缓存不可读。
    let current = client
        .cached_verified()
        .await
        .expect("read current")
        .expect("cache present")
        .identity;
    assert!(client.cached_matching(&current).await.is_ok());
}

/// 连续提交覆盖 Windows 暂存句柄生命周期，签名错误、回退和同序列异文不能覆盖缓存。
#[test]
fn signed_cache_rejects_tampering_and_sequence_rollback() {
    let (_root, client, caches) = fixture();
    assert_eq!(
        client.persist(&caches[0]).expect("first").identity.sequence,
        1
    );
    assert_eq!(
        client
            .persist(&caches[1])
            .expect("second")
            .identity
            .sequence,
        2
    );
    assert!(client.persist(&caches[0]).is_err());
    assert!(client.persist(&caches[2]).is_err());
    let tampered = Cache {
        raw: caches[1].raw.replace('2', "3"),
        signature: caches[1].signature.clone(),
        fetched_at: 2,
    };
    assert!(client.persist(&tampered).is_err());
    let path = client
        .storage
        .root()
        .join("plugins/catalog-cache/official.json");
    let cached = client
        .read_cache_at(&path)
        .expect("cache")
        .expect("present");
    assert_eq!(client.verify(&cached).expect("verified").sequence, 2);
}

/// Windows 替换被外部句柄阻塞时保留签名暂存与原缓存，调用方可先回读再决定后续动作。
#[cfg(windows)]
#[test]
fn blocked_replacement_preserves_staging_for_unknown_commit() {
    use std::os::windows::fs::OpenOptionsExt;

    let (_root, client, caches) = fixture();
    client.persist(&caches[0]).expect("first");
    let directory = client.storage.root().join("plugins/catalog-cache");
    let path = directory.join("official.json");
    let _external_reader = fs::OpenOptions::new()
        .read(true)
        .share_mode(3)
        .open(&path)
        .expect("reader without delete sharing");

    let error = client.persist(&caches[1]).expect_err("blocked replacement");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::OutcomeUnknown
    );
    let source = std::error::Error::source(&error)
        .and_then(std::error::Error::source)
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .expect("platform error remains available");
    assert_eq!(source.raw_os_error(), Some(32));
    let current = client
        .read_cache_at(&path)
        .expect("readback")
        .expect("cache");
    assert_eq!(
        client.verify(&current).expect("current signature").sequence,
        1
    );

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
    assert_eq!(
        client.verify(&pending).expect("staged signature").sequence,
        2
    );
}

/// 资产域名、端口和用户信息均逐项校验，README 的 Raw 例外不作用于资产。
#[test]
fn asset_and_document_url_boundaries_are_distinct() {
    assert!(allowed_url(
        "https://github.com/vendor/project/releases/download/v1/pkg",
        false
    ));
    assert!(allowed_url(
        "https://release-assets.githubusercontent.com/pkg",
        false
    ));
    assert!(!allowed_url(
        "https://release-assets.githubusercontent.com.evil.test/pkg",
        false
    ));
    assert!(!allowed_url(
        "http://release-assets.githubusercontent.com/pkg",
        false
    ));
    assert!(!allowed_url("https://github.com.evil.test/pkg", false));
    assert!(!allowed_url("https://github.com@evil.test/pkg", false));
    assert!(!allowed_url("https://github.com:444/pkg", false));
    assert!(!allowed_url("http://127.0.0.1/pkg", false));
    assert!(allowed_url("http://127.0.0.1/pkg", true));
    assert!(!allowed_url(
        "https://raw.githubusercontent.com/a/b/main/readme.md",
        false
    ));
    assert!(allowed_fetch_url(
        "https://raw.githubusercontent.com/a/b/main/readme.md",
        false,
        true
    ));
}

/// 真实签名 fixture 的两轨资产 URL 对照：V1 白名单不变，V2 放开合法 HTTP(S)。
///
/// 这里走完整的 `persist`（验签 → 解析 → 逐资产校验 → 序列比较 → 缓存提交），
/// 而不是直接调用纯函数，因此结论覆盖“已验签目录中的资产地址”这一真实路径。
/// 结果按三类分别判别：
/// 1. 白名单差异（LAN/公网非 GitHub/IDN）：V2 接受（新轨契约），V1 拒绝——
///    V1 的拒绝来自 GitHub 白名单语义，不代表地址语法无效；
/// 2. 两轨语法拒绝（userinfo/fragment/0 端口，统一用 GitHub host）：
///    V1 在白名单内仍拒绝，证明拒绝来自它自己的显式规则；V2 由共享契约拒绝；
/// 3. V1 不覆盖的原文歧义（反斜杠/控制字符，GitHub host）：V2 必须拒绝；
///    V1 没有原文歧义检查，不在此处断言统一拒绝，保留旧行为并如实注明。
///
/// 序列污染防护：V2 每次成功 persist 递增序列，地址样本永不复用同序列；
/// 预期失败的 persist 断言 `Invalid`（URL 拒绝）而不是笼统 `is_err()`——序列
/// 比较失败是 `Conflict`，两者可判别；失败后立刻核对缓存序列未变，证明拒绝
/// 发生在资产校验阶段。
#[tokio::test]
async fn asset_url_boundaries_differ_by_track_with_signed_fixtures() {
    let root = tempfile::tempdir().expect("temporary root");
    let storage = StorageCoordinator::new(root.path());
    let key = signing_key();
    let public = public_key(&key);
    let v1 = CatalogClient::testing_on_track(
        storage.clone(),
        CatalogTrack::OfficialV1,
        "http://127.0.0.1/catalog.json".into(),
        public.clone(),
    );
    let v2 = CatalogClient::testing_on_track(
        storage,
        CatalogTrack::OfficialV2,
        "http://127.0.0.1/catalog.json".into(),
        public,
    );

    // GitHub 白名单地址：两轨都接受，序列各自从 1 推进。
    assert!(
        v1.persist(&cache_with_assets(
            &key,
            CatalogTrack::OfficialV1,
            1,
            &["\"https://github.com/acme/tool/releases/download/v1/tool.zip\""]
        ))
        .is_ok()
    );
    assert!(
        v2.persist(&cache_with_assets(
            &key,
            CatalogTrack::OfficialV2,
            1,
            &["\"https://github.com/acme/tool/releases/download/v1/tool.zip\""]
        ))
        .is_ok()
    );

    // 预期 V2 拒绝时断言 Invalid 而非笼统 is_err：序列冲突是 Conflict，
    // 混淆两者会让测试结论失去判别力；随后核对缓存序列未变。helper 用嵌套
    // async fn 而不是返回 async 块的闭包：闭包形式无法为输出 future 声明随
    // 参数引用变化的生存期（lifetime may not live long enough），具名异步
    // 函数由编译器正确绑定 HRTB；序列由调用方显式传入，无共享可变捕获。
    async fn v2_accept(client: &CatalogClient, key: &SigningKey, url: &str, sequence: u64) {
        assert!(
            client
                .persist(&cache_with_assets(
                    key,
                    CatalogTrack::OfficialV2,
                    sequence,
                    &[url]
                ))
                .is_ok(),
            "v2 must accept {url}"
        );
        assert_eq!(
            client
                .cached_verified()
                .await
                .expect("read cache")
                .expect("cache present")
                .identity
                .sequence,
            sequence
        );
    }
    async fn v2_reject(client: &CatalogClient, key: &SigningKey, url: &str, current: u64) {
        assert_eq!(
            client
                .persist(&cache_with_assets(
                    key,
                    CatalogTrack::OfficialV2,
                    current + 1,
                    &[url]
                ))
                .expect_err("v2 must reject {url}")
                .public_code(),
            kerminal_application::PublicErrorCode::Invalid
        );
        // 拒绝不得落盘：缓存必须仍停留在 current 序列，证明失败在资产校验而非
        // 序列比较。
        assert_eq!(
            client
                .cached_verified()
                .await
                .expect("read cache")
                .expect("cache present")
                .identity
                .sequence,
            current
        );
    }

    let mut v2_sequence = 1u64;
    // 白名单差异组：V2 全部接受（序列递进），V1 全部拒绝且属白名单语义。
    let lan = "\"http://192.168.1.10:8443/acme/tool.zip\"";
    let public_net = "\"https://downloads.example.com/acme/tool.zip\"";
    let idn = "\"https://工具.example.cn/acme/tool.zip\"";
    for url in [lan, public_net, idn] {
        v2_sequence += 1;
        v2_accept(&v2, &key, url, v2_sequence).await;
        assert!(
            v1.persist(&cache_with_assets(
                &key,
                CatalogTrack::OfficialV1,
                2,
                &[url]
            ))
            .is_err()
        );
    }

    // 两轨语法拒绝组：统一 GitHub host（V1 白名单内），拒绝只能来自语法规则。
    // userinfo、fragment、0 端口（https）在 V1 的显式检查清单上；空 userinfo
    // 不在此组——url 解析器会把它规范化丢弃，V1 检查解析后的 username/password
    // 看不到原文，该样本归入 V2 原文歧义组。
    let syntax_rejected = [
        "\"https://user:pass@github.com/acme/tool.zip\"",
        "\"https://github.com/acme/tool.zip#frag\"",
        "\"https://github.com:0/acme/tool.zip\"",
    ];
    for url in syntax_rejected {
        v2_reject(&v2, &key, url, v2_sequence).await;
        assert_eq!(
            v1.persist(&cache_with_assets(
                &key,
                CatalogTrack::OfficialV1,
                2,
                &[url]
            ))
            .expect_err("v1 must reject {url}")
            .public_code(),
            kerminal_application::PublicErrorCode::Invalid
        );
    }

    // V1 不覆盖的原文歧义组：空 userinfo、反斜杠、控制字符。空 userinfo 在
    // url 解析后被规范化丢弃，V1 检查解析后的 username/password 看不到原文，
    // GitHub host 上按实现应当接受；反斜杠（JSON 原文 `\\` 解码后是单个
    // 反斜杠）与 DEL（`\u007f`）同理，V1 没有原文歧义检查。只对 V2 断言契约
    // 拒绝；V1 的实际取舍用 allowed_url 纯函数单独核对，且必须先做与
    // `serde_json` 一致的 JSON 解码——`trim_matches('"')` 只去引号不解码，
    // 反斜杠样本会带着两条反斜杠、DEL 样本会带着字面 `\u007f` 进检查，
    // 与签名目录里真正的 URL 字符串不一致。运行时若 V1 对解码后输入的判定
    // 与这里的预期不同，按实际结果反馈，不改 V1 生产逻辑。
    let ambiguous = [
        ("\"https://@github.com/acme/tool.zip\"", None),
        ("\"https://github.com/acme/\\\\tool.zip\"", Some((92, 1))),
        (
            "\"https://github.com/acme/to\\u007fol.zip\"",
            Some((127, 1)),
        ),
    ];
    for (url, marker) in ambiguous {
        v2_reject(&v2, &key, url, v2_sequence).await;
        // 与目录校验同一条解码路径：JSON 字符串解码后的值才是 verify 看到的原文。
        let decoded: String = serde_json::from_str(url).expect("fixture is valid JSON");
        if let Some((code, count)) = marker {
            // 解码结果必须含指定码点的指定次数，证明输入的就是该缺陷本身。
            let marked: String = char::from_u32(code)
                .map(|c| c.to_string())
                .expect("marker is a valid char");
            assert_eq!(decoded.matches(&marked).count(), count);
        }
        assert!(allowed_url(&decoded, false));
    }
}

/// 跨轨重放在带资产目录上仍然成立：另一轨来源的带资产目录必须被拒。
///
/// 这里钉住两层防线同时存在：来源绑定（source_id 与轨道不符 → invalid）先于
/// 资产校验触发，所以宽边界轨道的带资产目录整体进入窄边界轨道时必然失败，
/// 攻击者无法通过重放把 V2 的宽松资产边界带进 V1；反向同理。URL 边界本身的
/// 轨道区分已由上一个测试的 V1/V2 对照证明，这里确认它叠加在来源绑定之上。
#[test]
fn cross_track_replay_rejects_v2_asset_in_v1_catalog() {
    let root = tempfile::tempdir().expect("temporary root");
    let storage = StorageCoordinator::new(root.path());
    let key = signing_key();
    let public = public_key(&key);
    let v1 = CatalogClient::testing_on_track(
        storage.clone(),
        CatalogTrack::OfficialV1,
        "http://127.0.0.1/catalog.json".into(),
        public.clone(),
    );
    let v2 = CatalogClient::testing_on_track(
        storage,
        CatalogTrack::OfficialV2,
        "http://127.0.0.1/catalog.json".into(),
        public,
    );

    let lan = "\"http://192.168.1.10:8443/acme/tool.zip\"";
    let v2_catalog = cache_with_assets(&key, CatalogTrack::OfficialV2, 1, &[lan]);
    assert!(v2.persist(&v2_catalog).is_ok());
    // V2 目录（LAN 资产、V2 来源）整体进入 V1：来源绑定先行拒绝。
    assert_eq!(
        v1.persist(&v2_catalog)
            .expect_err("v2 asset must not replay on v1")
            .public_code(),
        kerminal_application::PublicErrorCode::Invalid
    );

    // V1 自身资产在 V1 目录继续可用：拒绝只来自来源，不是地址本身。
    let v1_catalog = cache_with_assets(
        &key,
        CatalogTrack::OfficialV1,
        1,
        &["\"https://github.com/acme/tool/releases/download/v1/tool.zip\""],
    );
    assert!(v1.persist(&v1_catalog).is_ok());
    assert!(v2.persist(&v1_catalog).is_err());
}

/// 带资产的已验签目录写入缓存后，重读路径执行同样的按轨 URL 校验。
///
/// 缓存不是旁路：`cached_verified`/`cached_matching` 复用 `verify`，这里直接
/// 用缓存重读验证 V2 的宽松边界在持久化后仍然生效，而不是只在提交时检查一次。
#[tokio::test]
async fn cached_reread_applies_track_asset_boundaries() {
    let root = tempfile::tempdir().expect("temporary root");
    let storage = StorageCoordinator::new(root.path());
    let key = signing_key();
    let public = public_key(&key);
    let v2 = CatalogClient::testing_on_track(
        storage.clone(),
        CatalogTrack::OfficialV2,
        "http://127.0.0.1/catalog.json".into(),
        public.clone(),
    );
    let url = "\"https://downloads.example.com/acme/tool.zip\"";
    let verified = v2
        .persist(&cache_with_assets(
            &key,
            CatalogTrack::OfficialV2,
            1,
            &[url],
        ))
        .expect("v2 lan asset accepted");
    let frozen = verified.identity;

    // 缓存重读通过：内容未变，锚点匹配仍然成功。
    let reread = v2.cached_matching(&frozen).await.expect("cache reread");
    assert_eq!(reread.identity.sequence, 1);
    assert!(
        reread.snapshot.catalog.plugins[0].versions[0].assets[0]
            .url
            .contains("downloads.example.com")
    );

    // 缓存文件被替换成带坏地址（userinfo）的同序列 V2 签名目录：重读必须拒绝，
    // 证明 URL 校验在缓存路径上没有被跳过。
    let directory = storage.root().join("plugins/catalog-cache");
    let tampered = cache_with_assets(
        &key,
        CatalogTrack::OfficialV2,
        1,
        &["\"https://user@downloads.example.com/acme/tool.zip\""],
    );
    fs::write(
        directory.join("official-v2.json"),
        serde_json::to_vec(&tampered).expect("encode"),
    )
    .expect("overwrite cache");
    assert!(v2.cached_verified().await.is_err());
    assert_eq!(
        v2.cached_matching(&frozen)
            .await
            .expect_err("tampered cache must not match")
            .public_code(),
        kerminal_application::PublicErrorCode::Invalid
    );
}
