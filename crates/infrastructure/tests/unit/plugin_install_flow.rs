//! 通过隔离的真实签名和 HTTP 服务覆盖安装到重启的状态流转。
//! @author kongweiguang

use super::*;
use crate::plugins::boot::BootPackageError;
use crate::plugins::layout::LayoutError;
use base64::{Engine, engine::general_purpose::STANDARD};
use blake2::{Blake2b512, Digest as _};
use ed25519_dalek::{Signer, SigningKey};
use kerminal_application::{PublicErrorCode, extensions::CatalogSource};
use kerminal_domain::plugins::PluginBootPhase;
use std::{collections::BTreeMap, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::watch,
};

#[path = "plugin_install_recovery.rs"]
mod recovery;

/// 每次运行只在内存生成测试私钥，模拟真实目录签名而不接触正式信任根。
fn signed_catalog(raw: &[u8]) -> (String, String) {
    let mut seed = [0u8; 32];
    seed[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    seed[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    let key = SigningKey::from_bytes(&seed);
    let id = *b"test-key";
    let mut public = b"Ed".to_vec();
    public.extend_from_slice(&id);
    public.extend_from_slice(key.verifying_key().as_bytes());
    let signature = key.sign(&Blake2b512::digest(raw)).to_bytes();
    let mut payload = b"ED".to_vec();
    payload.extend_from_slice(&id);
    payload.extend_from_slice(&signature);
    let comment = "isolated";
    let mut global = signature.to_vec();
    global.extend_from_slice(comment.as_bytes());
    let global_signature = key.sign(&global).to_bytes();
    (
        STANDARD.encode(public),
        format!(
            "untrusted comment: isolated\n{}\ntrusted comment: {}\n{}\n",
            STANDARD.encode(payload),
            comment,
            STANDARD.encode(global_signature)
        ),
    )
}

/// 用协议最小清单构建真实压缩包，确保安装器执行解包和内容校验。
fn package(version: &str) -> Vec<u8> {
    package_with_id("acme.notes", version)
}

/// 身份可指定的安装包用于覆盖保留身份拒绝；publisher 必须与身份一致，
/// 否则协议校验会先以清单错误拒绝，测不到安装器自己的身份门禁。
fn package_with_id(id: &str, version: &str) -> Vec<u8> {
    let publisher = id.split_once('.').map(|(value, _)| value).unwrap_or(id);
    let manifest = serde_json::json!({"schema":1,"id":id,"name":{"en":"Notes"},"description":{"en":"Notes"},"version":version,"api":"^1.0","publisher":publisher,"runtime":{"kind":"none"}}).to_string();
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "kerminal-plugin.json", manifest.as_bytes())
        .unwrap();
    tar.into_inner().unwrap().finish().unwrap()
}

/// 同一生命周期覆盖安装、重启状态切换与卸载；回退入口已移除，只验证显式拒绝。
async fn signed_catalog_lifecycle() {
    let root = tempfile::tempdir().unwrap();
    let config = crate::configuration::ConfigurationStore::initialize(root.path()).unwrap();
    let store = PluginStore::new(config.storage_coordinator()).unwrap();
    let settings = Arc::new(PluginSettingsService::new(
        Arc::new(config.clone()),
        Arc::new(config.clone()),
        Arc::new(crate::vault::VaultStore::open(config.storage_coordinator())),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut routes = BTreeMap::new();
    let versions: Vec<_> = ["1.0.0", "1.1.0"].into_iter().map(|version| {
        let bytes = package(version);
        let path = format!("/{version}.kplugin");
        let sha = format!("{:x}", Sha256::digest(&bytes));
        let asset = serde_json::json!({"platform":"any","url":format!("http://{address}{path}"),"size":bytes.len(),"sha256":sha});
        routes.insert(path, bytes);
        serde_json::json!({"version":version,"api":"^1.0","published_at":"2026-01-01T00:00:00Z","assets":[asset]})
    }).collect();
    let raw = serde_json::json!({"schema":1,"source_id":"official","sequence":1,"plugins":[{"id":"acme.notes","name":{"en":"Notes"},"description":{"en":"Notes"},"publisher":"acme","categories":[],"repository":"https://github.com/acme/notes","permissions":[],"versions":versions}]}).to_string();
    let (public, signature) = signed_catalog(raw.as_bytes());
    routes.insert("/catalog.json".into(), raw.into_bytes());
    routes.insert("/catalog.json.minisig".into(), signature.into_bytes());
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                let mut part = [0u8; 1024];
                let n = stream.read(&mut part).await.unwrap();
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&part[..n]);
            }
            let request = String::from_utf8_lossy(&bytes);
            let path = request.split_whitespace().nth(1).unwrap_or("");
            let body = routes.get(path).expect("known route");
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(body).await.unwrap();
        }
    });
    let catalog = Arc::new(CatalogClient::testing(
        config.storage_coordinator(),
        format!("http://{address}/catalog.json"),
        public,
    ));
    catalog.refresh(CancellationToken::new()).await.unwrap();
    let platform = Platform::current().unwrap();
    let installer =
        PackageInstaller::new(store.clone(), catalog, settings, "1.0.0".into(), platform);
    let validator = ProtocolBootValidator {
        host_api: "1.0.0".into(),
        platform,
    };
    let id = PluginId::parse("acme.notes").unwrap();
    let data = root.path().join("plugins/data/acme.notes/keep");
    std::fs::create_dir_all(data.parent().unwrap()).unwrap();
    std::fs::write(&data, b"keep").unwrap();
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    for version in ["1.0.0", "1.1.0"] {
        installer
            .install_catalog(
                CatalogSelection {
                    id: id.clone(),
                    version: PluginVersion::parse(version).unwrap(),
                    platform: Platform::Any,
                },
                progress.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(store.load().unwrap().state.pending.len(), 1);
        let boot = store.begin_startup(&validator, false).unwrap();
        assert_eq!(boot.plugins.len(), 1);
        assert_eq!(
            boot.plugins[0].version,
            PluginVersion::parse(version).unwrap()
        );
        store
            .mark_boot_phase(&boot.boot_id, PluginBootPhase::RegistryFrozen)
            .unwrap();
        store
            .mark_boot_phase(&boot.boot_id, PluginBootPhase::FirstFrame)
            .unwrap();
    }
    // 单版本布局不再提供上一版本回退；入口必须明确拒绝而不是静默排队。
    let error = installer
        .change(ExtensionChange::Rollback(id.clone()))
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    assert!(store.load().unwrap().state.pending.is_empty());
    assert_eq!(
        store.load().unwrap().state.plugins[&id].active,
        PluginVersion::parse("1.1.0").unwrap()
    );
    installer
        .change(ExtensionChange::Uninstall(id))
        .await
        .unwrap();
    let boot = store.begin_startup(&validator, false).unwrap();
    assert!(boot.plugins.is_empty());
    assert_eq!(std::fs::read(data).unwrap(), b"keep");
    server.abort();
}

/// 状态只在下次启动切换，卸载不会清除用户数据目录。
#[tokio::test]
async fn signed_catalog_install_update_uninstall_survives_restart() {
    signed_catalog_lifecycle().await;
}

/// drop-in 最小清单与真实启动扫描共用协议字段，便于覆盖目录名、名称与权限展示。
fn drop_in_manifest(version: &str) -> String {
    serde_json::json!({
        "schema": 1,
        "id": "acme.notes",
        "name": {"en": "Notes", "zh-CN": "便签"},
        "description": {"en": "Notes"},
        "version": version,
        "api": "^1.0",
        "publisher": "acme",
        "runtime": {"kind": "none"},
        "permissions": ["network:example.com"]
    })
    .to_string()
}

/// 每次测试使用独立数据根；插件目录在初始化前写入，模拟用户直接拷贝文件夹。
struct DropInFixture {
    root: tempfile::TempDir,
    store: PluginStore,
    installer: PackageInstaller,
    validator: ProtocolBootValidator,
    id: PluginId,
}

fn drop_in_fixture(version: &str) -> DropInFixture {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("plugins/acme.notes");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("kerminal-plugin.json"),
        drop_in_manifest(version),
    )
    .unwrap();
    let config = crate::configuration::ConfigurationStore::initialize(root.path()).unwrap();
    let store = PluginStore::new(config.storage_coordinator()).unwrap();
    let settings = Arc::new(PluginSettingsService::new(
        Arc::new(config.clone()),
        Arc::new(config.clone()),
        Arc::new(crate::vault::VaultStore::open(config.storage_coordinator())),
    ));
    let catalog = Arc::new(CatalogClient::testing(
        config.storage_coordinator(),
        "http://127.0.0.1/catalog.json".into(),
        String::new(),
    ));
    let platform = Platform::current().unwrap();
    let installer =
        PackageInstaller::new(store.clone(), catalog, settings, "1.0.0".into(), platform);
    let validator = ProtocolBootValidator {
        host_api: "1.0.0".into(),
        platform,
    };
    DropInFixture {
        root,
        store,
        installer,
        validator,
        id: PluginId::parse("acme.notes").unwrap(),
    }
}

/// 测试直接改写权威状态时必须走同一 CAS，保持 revision 与恢复门禁一致。
fn update_drop_in_state(store: &PluginStore, update: impl FnOnce(&mut PluginStoreState)) {
    let snapshot = store.load().unwrap();
    let mut next = snapshot.state.clone();
    update(&mut next);
    store.compare_and_swap(&snapshot, next).unwrap();
}

/// 不预置插件目录的安装器，用于覆盖安装器自身的记录状态分支。
fn bare_installer() -> (tempfile::TempDir, PluginStore, PackageInstaller) {
    let root = tempfile::tempdir().unwrap();
    let config = crate::configuration::ConfigurationStore::initialize(root.path()).unwrap();
    let store = PluginStore::new(config.storage_coordinator()).unwrap();
    let settings = Arc::new(PluginSettingsService::new(
        Arc::new(config.clone()),
        Arc::new(config.clone()),
        Arc::new(crate::vault::VaultStore::open(config.storage_coordinator())),
    ));
    let catalog = Arc::new(CatalogClient::testing(
        config.storage_coordinator(),
        "http://127.0.0.1/catalog.json".into(),
        String::new(),
    ));
    let installer = PackageInstaller::new(
        store.clone(),
        catalog,
        settings,
        "1.0.0".into(),
        Platform::current().unwrap(),
    );
    (root, store, installer)
}

/// 目录缺失的同版本重装是修复缺失包，应排队 Update 而不是误报已安装。
#[tokio::test]
async fn missing_record_same_version_reinstall_creates_update_pending() {
    let (root, store, installer) = bare_installer();
    let id = PluginId::parse("acme.notes").unwrap();
    update_drop_in_state(&store, |state| {
        state.plugins.insert(
            id.clone(),
            InstalledPlugin {
                active: PluginVersion::parse("1.0.0").unwrap(),
                previous: None,
                enabled: false,
                source: PluginSource::Local,
                installed_at: "2026-01-01T00:00:00Z".into(),
                // 缺失记录没有目录可摘要，允许指纹缺省。
                fingerprint: None,
                status: PluginInstallStatus::Missing,
                confirmed: true,
            },
        );
    });
    let path = root.path().join("local.kplugin");
    std::fs::write(&path, package("1.0.0")).unwrap();
    let inspection = installer
        .inspect_file(path.clone(), CancellationToken::new())
        .await
        .unwrap();
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let receipt = installer
        .install_file(path, inspection, progress, CancellationToken::new())
        .await
        .unwrap();
    assert!(receipt.restart_required);
    let state = store.load().unwrap().state;
    assert!(matches!(
        state.pending.as_slice(),
        [PendingPluginChange::Update { id: pending_id, to, fingerprint: Some(_), .. }]
            if pending_id == &id && to == &PluginVersion::parse("1.0.0").unwrap()
    ));
}

/// 目录仍在的同版本安装仍必须拒绝，避免把重复安装写成无意义的 Update。
#[tokio::test]
async fn installed_same_version_is_still_rejected() {
    let (root, store, installer) = bare_installer();
    let id = PluginId::parse("acme.notes").unwrap();
    update_drop_in_state(&store, |state| {
        state.plugins.insert(
            id.clone(),
            InstalledPlugin {
                active: PluginVersion::parse("1.0.0").unwrap(),
                previous: None,
                enabled: true,
                source: PluginSource::Local,
                installed_at: "2026-01-01T00:00:00Z".into(),
                fingerprint: Some(
                    PluginFingerprint::parse(format!("v1:{}", "0".repeat(64))).unwrap(),
                ),
                status: PluginInstallStatus::Ready,
                confirmed: true,
            },
        );
    });
    let path = root.path().join("local.kplugin");
    std::fs::write(&path, package("1.0.0")).unwrap();
    let inspection = installer
        .inspect_file(path.clone(), CancellationToken::new())
        .await
        .unwrap();
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = installer
        .install_file(path, inspection, progress, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert!(store.load().unwrap().state.pending.is_empty());
}

/// 检查只报告观察结果；确认必须同时绑定界面看到的指纹与 revision。
#[tokio::test]
async fn drop_in_inspection_and_confirmation_bind_fingerprint_and_revision() {
    let fixture = drop_in_fixture("1.0.0");
    let boot = fixture
        .store
        .begin_startup(&fixture.validator, false)
        .unwrap();
    assert!(boot.store.state.pending.is_empty());
    let inspections = fixture
        .installer
        .inspect_drop_ins(CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(inspections.len(), 1);
    let item = &inspections[0];
    assert_eq!(item.id, fixture.id);
    assert_eq!(item.version, PluginVersion::parse("1.0.0").unwrap());
    assert_eq!(item.name.get("zh-CN").map(String::as_str), Some("便签"));
    assert_eq!(item.permissions, vec!["network:example.com".to_string()]);
    assert!(!item.enabled && !item.confirmed);
    assert_eq!(item.status, PluginInstallStatus::Ready);
    let fingerprint = item.fingerprint.clone().expect("可读目录应有指纹");
    assert!(fingerprint.starts_with("v1:"));
    let revision = item.revision;
    assert_eq!(revision, boot.store.state.revision);

    // 与展示值不一致的指纹说明确认已过期，不能只凭 id 启用。
    let stale = format!("v1:{}", "0".repeat(64));
    let error = fixture
        .installer
        .confirm_drop_in(fixture.id.clone(), stale, revision)
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert!(fixture.store.load().unwrap().state.pending.is_empty());

    // revision 变化同样代表页面快照过期。
    let bumped = fixture
        .store
        .begin_startup(&fixture.validator, false)
        .unwrap();
    let error = fixture
        .installer
        .confirm_drop_in(fixture.id.clone(), fingerprint.clone(), revision)
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert!(fixture.store.load().unwrap().state.pending.is_empty());

    let receipt = fixture
        .installer
        .confirm_drop_in(
            fixture.id.clone(),
            fingerprint.clone(),
            bumped.store.state.revision,
        )
        .await
        .unwrap();
    assert!(receipt.restart_required);
    let state = fixture.store.load().unwrap().state;
    let installed = &state.plugins[&fixture.id];
    assert!(installed.enabled && installed.confirmed);
    assert_eq!(
        installed.fingerprint.as_ref().map(|value| value.as_str()),
        Some(fingerprint.as_str())
    );
    assert!(
        matches!(state.pending.as_slice(), [PendingPluginChange::Enable { id }] if id == &fixture.id)
    );

    // 已确认后重复确认必须固定冲突，且不能追加第二条 pending。
    let error = fixture
        .installer
        .confirm_drop_in(fixture.id.clone(), fingerprint.clone(), receipt.revision)
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(fixture.store.load().unwrap().state.pending.len(), 1);

    // 未知 id 不能伪造确认。
    let unknown = PluginId::parse("acme.unknown").unwrap();
    let error = fixture
        .installer
        .confirm_drop_in(unknown, fingerprint, receipt.revision)
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::NotFound);
}

/// 确认后发现目录内容被替换时必须拒绝，不能启用未经检查的内容。
#[tokio::test]
async fn drop_in_confirmation_rejects_changed_directory() {
    let fixture = drop_in_fixture("1.0.0");
    fixture
        .store
        .begin_startup(&fixture.validator, false)
        .unwrap();
    let item = fixture
        .installer
        .inspect_drop_ins(CancellationToken::new())
        .await
        .unwrap()
        .remove(0);
    let fingerprint = item.fingerprint.clone().unwrap();
    std::fs::write(
        fixture.root.path().join("plugins/acme.notes/extra.txt"),
        b"changed",
    )
    .unwrap();
    let error = fixture
        .installer
        .confirm_drop_in(fixture.id.clone(), fingerprint, item.revision)
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    let state = fixture.store.load().unwrap().state;
    let installed = &state.plugins[&fixture.id];
    assert!(!installed.enabled && !installed.confirmed);
    assert!(state.pending.is_empty());
}

/// 普通 Enable 不能绕过指纹确认；启动时也不得替 drop-in 自动补 confirmed。
#[tokio::test]
async fn unconfirmed_drop_in_requires_confirmation_before_enable() {
    let fixture = drop_in_fixture("1.0.0");
    fixture
        .store
        .begin_startup(&fixture.validator, false)
        .unwrap();
    let before = fixture.store.load().unwrap().state;
    let error = fixture
        .installer
        .change(ExtensionChange::Enable(fixture.id.clone()))
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(fixture.store.load().unwrap().state, before);

    // 即便旧 pending 混入状态，启动应用也必须记录失败而不是产生非法已启用记录。
    update_drop_in_state(&fixture.store, |state| {
        state.pending.push(PendingPluginChange::Enable {
            id: fixture.id.clone(),
        });
    });
    let boot = fixture
        .store
        .begin_startup(&fixture.validator, false)
        .unwrap();
    let installed = &boot.store.state.plugins[&fixture.id];
    assert!(!installed.enabled && !installed.confirmed);
    assert!(boot.rejected.contains_key(&fixture.id));
    assert!(
        boot.store.state.pending.iter().any(|item| {
            matches!(item, PendingPluginChange::Enable { id } if id == &fixture.id)
        })
    );
}

/// 确认后重启可加载；已确认的 drop-in 再次禁用后仍能正常启用。
#[tokio::test]
async fn confirmed_drop_in_enables_after_restart_and_can_toggle() {
    let fixture = drop_in_fixture("1.0.0");
    fixture
        .store
        .begin_startup(&fixture.validator, false)
        .unwrap();
    let item = fixture
        .installer
        .inspect_drop_ins(CancellationToken::new())
        .await
        .unwrap()
        .remove(0);
    fixture
        .installer
        .confirm_drop_in(fixture.id.clone(), item.fingerprint.unwrap(), item.revision)
        .await
        .unwrap();
    let boot = fixture
        .store
        .begin_startup(&fixture.validator, false)
        .unwrap();
    assert!(
        boot.enabled_plugins()
            .any(|plugin| plugin.id == fixture.id && plugin.enabled)
    );

    fixture
        .installer
        .change(ExtensionChange::Disable(fixture.id.clone()))
        .await
        .unwrap();
    let boot = fixture
        .store
        .begin_startup(&fixture.validator, false)
        .unwrap();
    assert!(boot.enabled_plugins().all(|plugin| plugin.id != fixture.id));
    let installed = &boot.store.state.plugins[&fixture.id];
    assert!(!installed.enabled && installed.confirmed);

    fixture
        .installer
        .change(ExtensionChange::Enable(fixture.id.clone()))
        .await
        .unwrap();
    let boot = fixture
        .store
        .begin_startup(&fixture.validator, false)
        .unwrap();
    assert!(
        boot.enabled_plugins()
            .any(|plugin| plugin.id == fixture.id && plugin.enabled)
    );
}

/// 缺失记录移除需要目录确实消失且没有 pending；私有数据不随记录删除。
#[tokio::test]
async fn missing_record_removal_requires_missing_directory_without_pending() {
    let fixture = drop_in_fixture("1.0.0");
    fixture
        .store
        .begin_startup(&fixture.validator, false)
        .unwrap();
    let id = fixture.id.clone();
    let directory = fixture.root.path().join("plugins/acme.notes");
    let data = fixture.root.path().join("plugins/data/acme.notes/keep");
    std::fs::create_dir_all(data.parent().unwrap()).unwrap();
    std::fs::write(&data, b"keep").unwrap();

    // 目录仍存在时，即使记录被标记缺失也不能删记录。
    update_drop_in_state(&fixture.store, |state| {
        state.plugins.get_mut(&id).unwrap().status = PluginInstallStatus::Missing;
    });
    let error = fixture
        .installer
        .change(ExtensionChange::RemoveMissingRecord(id.clone()))
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);

    // 目录已缺失但仍有排队意图时同样拒绝。
    std::fs::remove_dir_all(&directory).unwrap();
    update_drop_in_state(&fixture.store, |state| {
        state
            .pending
            .push(PendingPluginChange::Enable { id: id.clone() });
    });
    let error = fixture
        .installer
        .change(ExtensionChange::RemoveMissingRecord(id.clone()))
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert!(
        fixture
            .store
            .load()
            .unwrap()
            .state
            .plugins
            .contains_key(&id)
    );

    // 记录不是 Missing 状态时不能借移除入口改写其它状态。
    update_drop_in_state(&fixture.store, |state| {
        state.pending.clear();
        state.plugins.get_mut(&id).unwrap().status = PluginInstallStatus::Ready;
    });
    let error = fixture
        .installer
        .change(ExtensionChange::RemoveMissingRecord(id.clone()))
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);

    // 条件满足才 CAS 删除记录，`data/<id>` 保持原样。
    update_drop_in_state(&fixture.store, |state| {
        state.plugins.get_mut(&id).unwrap().status = PluginInstallStatus::Missing;
    });
    let receipt = fixture
        .installer
        .change(ExtensionChange::RemoveMissingRecord(id.clone()))
        .await
        .unwrap();
    assert!(!receipt.restart_required);
    let state = fixture.store.load().unwrap().state;
    assert!(!state.plugins.contains_key(&id));
    assert!(!state.failures.contains_key(&id));
    assert_eq!(std::fs::read(&data).unwrap(), b"keep");
}

/// 回退入口已移除；拒绝时不写 pending，revision 也保持不变。
#[tokio::test]
async fn rollback_is_rejected_without_writing_pending() {
    let fixture = drop_in_fixture("1.0.0");
    let before = fixture.store.load().unwrap().state;
    let error = fixture
        .installer
        .change(ExtensionChange::Rollback(fixture.id.clone()))
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    assert_eq!(fixture.store.load().unwrap().state, before);
}

/// 权限确认绑定真实包摘要，确认后替换同一路径也不能安装另一份内容。
#[tokio::test]
async fn local_inspection_rejects_replaced_package() {
    let root = tempfile::tempdir().unwrap();
    let config = crate::configuration::ConfigurationStore::initialize(root.path()).unwrap();
    let store = PluginStore::new(config.storage_coordinator()).unwrap();
    let settings = Arc::new(PluginSettingsService::new(
        Arc::new(config.clone()),
        Arc::new(config.clone()),
        Arc::new(crate::vault::VaultStore::open(config.storage_coordinator())),
    ));
    let catalog = Arc::new(CatalogClient::testing(
        config.storage_coordinator(),
        "http://127.0.0.1/catalog.json".into(),
        String::new(),
    ));
    let installer = PackageInstaller::new(
        store.clone(),
        catalog,
        settings,
        "1.0.0".into(),
        Platform::current().unwrap(),
    );
    let path = root.path().join("local.kplugin");
    std::fs::write(&path, package("1.0.0")).unwrap();
    let inspection = installer
        .inspect_file(path.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(inspection.id, PluginId::parse("acme.notes").unwrap());
    assert_eq!(inspection.version, PluginVersion::parse("1.0.0").unwrap());
    assert!(inspection.permissions.is_empty());
    std::fs::write(&path, package("1.1.0")).unwrap();
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    assert!(
        installer
            .install_file(path, inspection, progress, CancellationToken::new())
            .await
            .is_err()
    );
    assert!(store.load().unwrap().state.pending.is_empty());
}

/// 市场与本地安装统一使用的前置夹具：隔离数据根、安装器与进度通道。
struct FlowFixture {
    root: tempfile::TempDir,
    store: PluginStore,
    installer: PackageInstaller,
}

fn flow_fixture() -> FlowFixture {
    let root = tempfile::tempdir().unwrap();
    let config = crate::configuration::ConfigurationStore::initialize(root.path()).unwrap();
    let store = PluginStore::new(config.storage_coordinator()).unwrap();
    let settings = Arc::new(PluginSettingsService::new(
        Arc::new(config.clone()),
        Arc::new(config.clone()),
        Arc::new(crate::vault::VaultStore::open(config.storage_coordinator())),
    ));
    let catalog = Arc::new(CatalogClient::testing(
        config.storage_coordinator(),
        "http://127.0.0.1/catalog.json".into(),
        String::new(),
    ));
    let installer = PackageInstaller::new(
        store.clone(),
        catalog,
        settings,
        "1.0.0".into(),
        Platform::current().unwrap(),
    );
    FlowFixture {
        root,
        store,
        installer,
    }
}

/// 构造与既有用例同形态的另一份合法包，身份与版本都不同保证内容摘要必然不同。
fn package_other_version(version: &str) -> Vec<u8> {
    package_with_id("acme.other", version)
}

/// 串行化“登记/撤销注入点”这两步瞬时操作；它只保护登记表的同一根写入，
/// 不承担跨 fixture 隔离，也绝不跨安装等待持有。跨 fixture 隔离由动作登记的
/// 配置根与暂存路径的分量前缀比较保证。锁中毒时取回内部数据：注入动作 panic
/// 不应让守卫 Drop 的清理二次 panic，把整个测试进程一起拖垮。
fn lock_test_hooks<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static UNPACK_HOOK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 注入守卫：作用域结束（含断言失败）时只清除本 fixture 配置根的动作，
/// 不影响其他并行用例登记的动作。
struct UnpackHookGuard {
    root: std::path::PathBuf,
}
impl Drop for UnpackHookGuard {
    fn drop(&mut self) {
        // 只在瞬时撤销时取锁；此时没有跨 await 的持锁，不会与安装等待互相阻塞。
        let _registration = lock_test_hooks(&UNPACK_HOOK_LOCK);
        test_hooks::before_unpack::clear(&self.root);
    }
}

/// 移动后注入守卫：覆盖整个安装等待，退出时只撤销本根动作。
struct AfterMoveHookGuard {
    root: std::path::PathBuf,
}
impl Drop for AfterMoveHookGuard {
    fn drop(&mut self) {
        let _registration = lock_test_hooks(&UNPACK_HOOK_LOCK);
        test_hooks::after_move::clear(&self.root);
    }
}

/// 确定性地模拟“确认/下载完成后、解压前”的暂存包替换：把替换动作登记到
/// 本次 fixture 的配置根，暂存归档必须落在该根下才会被覆写。守卫必须覆盖
/// 整个安装等待，退出时只撤销本根动作。
fn swap_stage_archive_before_unpack(
    fixture: &FlowFixture,
    replacement: Vec<u8>,
) -> UnpackHookGuard {
    let root = fixture.store.root().to_path_buf();
    {
        let _registration = lock_test_hooks(&UNPACK_HOOK_LOCK);
        test_hooks::before_unpack::set(
            &root,
            Box::new(move |archive_path: &std::path::Path| {
                // 暂存归档属于安装器私有文件，此刻没有其他读取者，原位覆写是安全的。
                std::fs::write(archive_path, &replacement).unwrap();
            }),
        );
    }
    UnpackHookGuard { root }
}

/// 正向路径用的注入动作：与替换注入相同的触发时机，但内容保持不变，用于
/// 证明拒绝来自摘要绑定而非注入本身。
fn touch_stage_archive_before_unpack(fixture: &FlowFixture) -> UnpackHookGuard {
    let root = fixture.store.root().to_path_buf();
    {
        let _registration = lock_test_hooks(&UNPACK_HOOK_LOCK);
        test_hooks::before_unpack::set(&root, Box::new(|_: &std::path::Path| {}));
    }
    UnpackHookGuard { root }
}

/// 把 `move_directory` 的成功结果改写成“改名已发生、目录同步失败”的
/// CommitUnknown 形态：验证安装器把它折叠成已移动并继续受理状态，而不是
/// 伪称未移动。守卫覆盖整个安装等待，退出时只撤销本根动作。
fn force_unknown_after_staging_move(fixture: &FlowFixture) -> AfterMoveHookGuard {
    let root = fixture.store.root().to_path_buf();
    {
        let _registration = lock_test_hooks(&UNPACK_HOOK_LOCK);
        test_hooks::after_move::set(
            &root,
            Box::new(|result: &mut Result<(), LayoutError>| {
                // 注入只对成功路径生效：真实 CommitUnknown 只发生在改名成功后。
                if result.is_ok() {
                    *result = Err(LayoutError::OutcomeUnknown {
                        operation_id: "test".into(),
                        source: std::io::Error::other("注入的目录同步失败"),
                    });
                }
            }),
        );
    }
    AfterMoveHookGuard { root }
}

/// 构造生产同形态的暂存归档路径：安装器只在 `<root>/plugins/.staging/_incoming/<op>/pkg.kplugin`
/// 下调用注入点，回归必须用这个形态而不是随意子路径。
fn staged_archive_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join("plugins")
        .join(".staging")
        .join("_incoming")
        .join("operation")
        .join("pkg.kplugin")
}

/// 注入动作按配置根隔离的回归：登记 A 根后只有 A 根下的暂存归档命中；
/// 其他 tempdir、以及字符串前缀相近的兄弟目录都不命中，清除后也不再命中。
/// 这样并行运行的未注入安装用例不会被别的用例覆写暂存包。
#[test]
fn unpack_hook_is_scoped_to_registered_data_root() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    // 与 A 根同前缀但多一个字符的兄弟路径，验证按路径分量而非字符串前缀比较。
    let sibling_name = format!(
        "{}x",
        first
            .path()
            .file_name()
            .expect("tempdir has a name")
            .to_string_lossy()
    );
    let sibling = first.path().with_file_name(sibling_name);

    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = hits.clone();
    {
        let _registration = lock_test_hooks(&UNPACK_HOOK_LOCK);
        test_hooks::before_unpack::set(
            first.path(),
            Box::new(move |_| {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
        );
    }

    test_hooks::before_unpack::run(first.path(), &staged_archive_path(first.path()));
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "登记根下的暂存归档必须命中"
    );
    // 生产调用形态：另一安装器以自己的根调用 run(B, B 归档)，A 的动作不得触发。
    test_hooks::before_unpack::run(second.path(), &staged_archive_path(second.path()));
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "A 注册期间 run(B, B 归档) 不得触发 A 的动作"
    );
    test_hooks::before_unpack::run(first.path(), &staged_archive_path(second.path()));
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "以 A 根查询 B 的归档也不得命中"
    );
    test_hooks::before_unpack::run(first.path(), &staged_archive_path(&sibling));
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "前缀相近的兄弟目录不得命中"
    );

    test_hooks::before_unpack::clear(first.path());
    test_hooks::before_unpack::run(first.path(), &staged_archive_path(first.path()));
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "撤销后本根也不得再命中"
    );
}

/// 线程内注册的 RAII 守卫：无论正常返回还是 panic 展开，都只清理自己登记的根。
///
/// 用它替代线程末尾的手工 `clear`，保证某线程中途失败时不会把动作留给后续用例；
/// 主线程另有兜底清理，但不会把它当成“线程已完成清理”的证据。
struct ThreadRootGuard {
    root: std::path::PathBuf,
}
impl Drop for ThreadRootGuard {
    fn drop(&mut self) {
        let _registration = lock_test_hooks(&UNPACK_HOOK_LOCK);
        test_hooks::before_unpack::clear(&self.root);
    }
}

/// 两个真实临时根各自注册动作时，并发调用只命中各自的动作，清理一个不影响另一个。
///
/// 用每线程独占接收端的 `mpsc::recv_timeout` 分三段同步（两根都登记完 →
/// 两根都查询完 → A 已清理），替代睡眠；对端 panic 断开或超时都只会让等待方
/// 在有限时间内失败，不会永久挂住测试进程。每个线程用 RAII 守卫清理自己的根，
/// 主线程先收集 join 结果并兜底清理，再断言。
#[test]
fn unpack_hook_isolates_concurrent_registrations_per_root() {
    use std::sync::mpsc;
    use std::time::Duration;

    /// 两个线程之间推进的阶段信号；每线程独占自己的接收端，避免共享 Receiver。
    #[derive(Clone, Copy, Debug)]
    enum Stage {
        Registered,
        Queried,
    }

    // 有界等待只用于防止线程互相挂死；正常路径由对端信号立即推进，不依赖时长。
    const SIGNAL_TIMEOUT: Duration = Duration::from_secs(10);

    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let first_root = first.path().to_path_buf();
    let second_root = second.path().to_path_buf();
    let first_hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let second_hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // 两个方向各一条通道：A 只读 to_a、只写 to_b，B 反之；cleared 只从 A 流向 B。
    let (to_a_tx, to_a_rx) = mpsc::channel::<Stage>();
    let (to_b_tx, to_b_rx) = mpsc::channel::<Stage>();
    let (cleared_tx, cleared_rx) = mpsc::channel::<()>();

    // 线程 A：登记 A → 通知 B → 等 B 登记 → 查询 A/A 与 A/B → 通知 B →
    // 等 B 查询完 → 清理 A（RAII 兜底）→ 通知 B 已清理。
    let thread_a = {
        let root = first_root.clone();
        let other = second_root.clone();
        let hits = first_hits.clone();
        // 发送端直接移入线程、主线程不留副本：A 若提前 panic，B 的接收端立即
        // 断开而不是等满超时。
        std::thread::spawn(move || {
            {
                let _registration = lock_test_hooks(&UNPACK_HOOK_LOCK);
                test_hooks::before_unpack::set(
                    &root,
                    Box::new(move |_| {
                        hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }),
                );
            }
            let guard = ThreadRootGuard { root: root.clone() };
            to_b_tx.send(Stage::Registered).expect("通知 B 登记");
            assert!(
                matches!(to_a_rx.recv_timeout(SIGNAL_TIMEOUT), Ok(Stage::Registered)),
                "应等到 B 完成登记"
            );
            test_hooks::before_unpack::run(&root, &staged_archive_path(&root));
            // 以本根查询对方归档：不得命中本根动作。
            test_hooks::before_unpack::run(&root, &staged_archive_path(&other));
            to_b_tx.send(Stage::Queried).expect("通知 B 查询完成");
            assert!(
                matches!(to_a_rx.recv_timeout(SIGNAL_TIMEOUT), Ok(Stage::Queried)),
                "应等到 B 查询完成"
            );
            // 只清理本根；另一根的动作必须保持有效（守卫同样只清本根）。
            {
                let _registration = lock_test_hooks(&UNPACK_HOOK_LOCK);
                test_hooks::before_unpack::clear(&root);
            }
            cleared_tx.send(()).expect("通知 B 已清理");
            drop(guard);
        })
    };

    // 线程 B：登记 B → 通知 A → 等 A 登记 → 查询 B/B 与 B/A → 通知 A →
    // 等 A 查询完 → 等 A 已清理 → 在 A 已清理前提下再查 B/B 与 A/A → 退出，
    // 守卫负责清理 B。
    let thread_b = {
        let root = second_root.clone();
        let other = first_root.clone();
        let hits = second_hits.clone();
        std::thread::spawn(move || {
            {
                let _registration = lock_test_hooks(&UNPACK_HOOK_LOCK);
                test_hooks::before_unpack::set(
                    &root,
                    Box::new(move |_| {
                        hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }),
                );
            }
            let _guard = ThreadRootGuard { root: root.clone() };
            to_a_tx.send(Stage::Registered).expect("通知 A 登记");
            assert!(
                matches!(to_b_rx.recv_timeout(SIGNAL_TIMEOUT), Ok(Stage::Registered)),
                "应等到 A 完成登记"
            );
            test_hooks::before_unpack::run(&root, &staged_archive_path(&root));
            // 以本根查询对方归档：不得命中本根动作。
            test_hooks::before_unpack::run(&root, &staged_archive_path(&other));
            to_a_tx.send(Stage::Queried).expect("通知 A 查询完成");
            assert!(
                matches!(to_b_rx.recv_timeout(SIGNAL_TIMEOUT), Ok(Stage::Queried)),
                "应等到 A 查询完成"
            );
            // 等 A 完成清理；对端断开或超时都在有限时间内失败，不会永久等待。
            cleared_rx
                .recv_timeout(SIGNAL_TIMEOUT)
                .expect("应等到 A 完成清理");
            // A 已清理：B 的动作仍须命中，且以 A 根查询 A 归档不得命中。
            test_hooks::before_unpack::run(&root, &staged_archive_path(&root));
            test_hooks::before_unpack::run(&other, &staged_archive_path(&other));
        })
    };

    // 先收集 join 结果并清理，再断言：失败时不会跳过清理，也不会误称已清理。
    let result_a = thread_a.join();
    let result_b = thread_b.join();
    test_hooks::before_unpack::clear(&first_root);
    test_hooks::before_unpack::clear(&second_root);
    result_a.expect("第一个根的动作线程不应 panic");
    result_b.expect("第二个根的动作线程不应 panic");

    // A 只在并发查询阶段命中一次；B 在并发查询与“A 已清理”阶段各命中一次。
    assert_eq!(
        first_hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "第一个根只应命中自己的动作一次，清理后不得再命中"
    );
    assert_eq!(
        second_hits.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "清理第一个根不得影响第二个根的动作"
    );

    // 两个线程都已清理，两个根都不再命中。
    test_hooks::before_unpack::run(&first_root, &staged_archive_path(&first_root));
    test_hooks::before_unpack::run(&second_root, &staged_archive_path(&second_root));
    assert_eq!(
        first_hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "线程清理后第一个根不得再命中"
    );
    assert_eq!(
        second_hits.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "线程清理后第二个根不得再命中"
    );
}

/// 断言本次失败安装没有产生任何状态或暂存残留：不入队、不发布、
/// _incoming 无遗留、按 ID 分组的槽不存在。
fn assert_no_state_or_staging_left(fixture: &FlowFixture) {
    let state = fixture.store.load().unwrap().state;
    assert!(state.pending.is_empty());
    assert!(state.plugins.is_empty());
    let staging = fixture.root.path().join("plugins/.staging");
    assert!(!staging.join("acme.notes").exists());
    assert!(!staging.join("acme.other").exists());
    let incoming = staging.join("_incoming");
    if incoming.exists() {
        assert_eq!(std::fs::read_dir(incoming).unwrap().count(), 0);
    }
}

/// 市场安装：下载完成、解压前被替换的暂存包必须被解压端摘要比对拒绝——
/// 期望值来自签名目录的选定资产，与实际解压流在同一次打开内核对，替换包
/// 不入队、不发布、不留暂存残留。
#[tokio::test]
async fn catalog_install_rejects_package_swapped_before_unpack() {
    let root = tempfile::tempdir().unwrap();
    let config = crate::configuration::ConfigurationStore::initialize(root.path()).unwrap();
    let store = PluginStore::new(config.storage_coordinator()).unwrap();
    let settings = Arc::new(PluginSettingsService::new(
        Arc::new(config.clone()),
        Arc::new(config.clone()),
        Arc::new(crate::vault::VaultStore::open(config.storage_coordinator())),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let original = package("1.0.0");
    let sha = format!("{:x}", Sha256::digest(&original));
    let asset = serde_json::json!({"platform":"any","url":format!("http://{address}/a.kplugin"),"size":original.len(),"sha256":sha});
    let raw = serde_json::json!({"schema":1,"source_id":"official","sequence":1,"plugins":[{"id":"acme.notes","name":{"en":"Notes"},"description":{"en":"Notes"},"publisher":"acme","categories":[],"repository":"https://github.com/acme/notes","permissions":[],"versions":[{"version":"1.0.0","api":"^1.0","published_at":"2026-01-01T00:00:00Z","assets":[asset]}]}]}).to_string();
    let (public, signature) = signed_catalog(raw.as_bytes());
    let mut routes = BTreeMap::new();
    routes.insert("/catalog.json".to_string(), raw.into_bytes());
    routes.insert("/catalog.json.minisig".to_string(), signature.into_bytes());
    routes.insert("/a.kplugin".to_string(), original.clone());
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                let mut part = [0u8; 1024];
                let n = stream.read(&mut part).await.unwrap();
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&part[..n]);
            }
            let request = String::from_utf8_lossy(&bytes);
            let path = request.split_whitespace().nth(1).unwrap_or("");
            let body = routes.get(path).expect("known route");
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(body).await.unwrap();
        }
    });
    let catalog = Arc::new(CatalogClient::testing(
        config.storage_coordinator(),
        format!("http://{address}/catalog.json"),
        public,
    ));
    catalog.refresh(CancellationToken::new()).await.unwrap();
    let installer = PackageInstaller::new(
        store.clone(),
        catalog,
        settings,
        "1.0.0".into(),
        Platform::current().unwrap(),
    );
    let fixture = FlowFixture {
        root,
        store,
        installer,
    };
    // 替换包是合法归档但不在签名目录内：解压端摘要比对必须失败。
    let _hook = swap_stage_archive_before_unpack(&fixture, package_other_version("9.9.9"));
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = fixture
        .installer
        .install_catalog(
            CatalogSelection {
                id: PluginId::parse("acme.notes").unwrap(),
                version: PluginVersion::parse("1.0.0").unwrap(),
                platform: Platform::Any,
            },
            progress,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    drop(_hook);
    server.abort();
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    assert_no_state_or_staging_left(&fixture);
}

/// 本地确认安装：预检确认的摘要绑定实际解压流，确认后替换暂存包不得进入
/// 状态受理；失败类别、store/pending 与清理语义与市场路径一致。
#[tokio::test]
async fn local_install_rejects_package_swapped_after_inspection() {
    let fixture = flow_fixture();
    let original = package("1.0.0");
    let path = fixture.root.path().join("local.kplugin");
    std::fs::write(&path, &original).unwrap();
    let inspection = fixture
        .installer
        .inspect_file(path.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(inspection.size, original.len() as u64);
    assert_eq!(
        inspection.sha256,
        format!("{:x}", Sha256::digest(&original))
    );
    let _hook = swap_stage_archive_before_unpack(&fixture, package_other_version("9.9.9"));
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = fixture
        .installer
        .install_file(path, inspection, progress, CancellationToken::new())
        .await
        .unwrap_err();
    drop(_hook);
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    assert_no_state_or_staging_left(&fixture);
}

/// 本地预检：暂存归档摘要观测后、解压前的替换包必须让预检失败；预检成功
/// 返回的 size/摘要与展示内容（清单身份、版本）永远来自同一份实际解压流，
/// 不存在“展示 A 包清单、摘要对应 B 包”的分离状态。
#[tokio::test]
async fn inspect_file_binds_displayed_digest_to_actual_unpack_stream() {
    let fixture = flow_fixture();
    let path = fixture.root.path().join("local.kplugin");
    std::fs::write(&path, package("1.0.0")).unwrap();
    let _hook = swap_stage_archive_before_unpack(&fixture, package_other_version("9.9.9"));
    let error = fixture
        .installer
        .inspect_file(path, CancellationToken::new())
        .await
        .unwrap_err();
    drop(_hook);
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    // 预检只读不写状态，也不留暂存材料。
    assert_no_state_or_staging_left(&fixture);
}

/// 正向路径：注入点按真实时机触发但不改变内容，解压照常通过、安装完整入队，
/// 证明拒绝来自摘要绑定而非注入机制本身；提交成功后清理无警告。
#[tokio::test]
async fn local_install_succeeds_with_verified_expectation_binding() {
    let fixture = flow_fixture();
    let original = package("1.0.0");
    let path = fixture.root.path().join("local.kplugin");
    std::fs::write(&path, &original).unwrap();
    let inspection = fixture
        .installer
        .inspect_file(path.clone(), CancellationToken::new())
        .await
        .unwrap();
    let _hook = touch_stage_archive_before_unpack(&fixture);
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let receipt = fixture
        .installer
        .install_file(path, inspection, progress, CancellationToken::new())
        .await
        .unwrap();
    drop(_hook);
    assert!(receipt.restart_required);
    assert!(receipt.cleanup_warning.is_none());
    let state = fixture.store.load().unwrap().state;
    assert!(matches!(
        state.pending.as_slice(),
        [PendingPluginChange::Install { id, to, fingerprint: Some(_), .. }]
            if id == &PluginId::parse("acme.notes").unwrap()
                && to == &PluginVersion::parse("1.0.0").unwrap()
    ));
    // 成功安装按设计保留被 pending 引用的暂存槽；_incoming 的临时材料清空。
    assert!(
        fixture
            .root
            .path()
            .join("plugins/.staging/acme.notes")
            .exists()
    );
    let incoming = fixture.root.path().join("plugins/.staging/_incoming");
    if incoming.exists() {
        assert_eq!(std::fs::read_dir(incoming).unwrap().count(), 0);
    }
}

/// 摘要观测前源文件被整包替换：预检观测到的就是替换后的内容，展示的
/// size/摘要/身份必须如实描述实际包，预检信息因此不会是过期快照。
#[tokio::test]
async fn inspect_file_reports_current_content_of_source_package() {
    let fixture = flow_fixture();
    let path = fixture.root.path().join("local.kplugin");
    std::fs::write(&path, package("1.0.0")).unwrap();
    let replaced = package_other_version("9.9.9");
    std::fs::write(&path, &replaced).unwrap();
    let inspection = fixture
        .installer
        .inspect_file(path, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        inspection.sha256,
        format!("{:x}", Sha256::digest(&replaced))
    );
    assert_eq!(inspection.size, replaced.len() as u64);
    assert_eq!(
        inspection.id,
        PluginId::parse("acme.other").unwrap(),
        "预检展示的身份必须来自实际解压流对应的包"
    );
}

/// 预检确认后源文件被追加：安装按已确认观测值核对实际解压流，增长使
/// 压缩输入越过预检大小上限而被立即拒绝，追加内容不可能混入安装。
#[tokio::test]
async fn install_file_rejects_source_grown_after_inspection() {
    let fixture = flow_fixture();
    let original = package("1.0.0");
    let path = fixture.root.path().join("local.kplugin");
    std::fs::write(&path, &original).unwrap();
    let inspection = fixture
        .installer
        .inspect_file(path.clone(), CancellationToken::new())
        .await
        .unwrap();
    let mut grown = original;
    grown.extend_from_slice(b"appended-after-inspection");
    std::fs::write(&path, &grown).unwrap();
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = fixture
        .installer
        .install_file(path, inspection, progress, CancellationToken::new())
        .await
        .unwrap_err();
    // 实际流超过期望大小+1 的输入上限时解压端即时失败，属于数据错误类别。
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    assert_no_state_or_staging_left(&fixture);
}

/// 第三方预检先被官方命名空间规则拒绝；身份门禁仍是提交阶段的第二道防线。
#[tokio::test]
async fn inspect_file_rejects_builtin_namespace_package() {
    let (root, store, installer) = bare_installer();
    let path = root.path().join("reserved.kplugin");
    std::fs::write(&path, package_with_id("kerminal.core", "1.0.0")).unwrap();
    let error = installer
        .inspect_file(path, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Infrastructure);
    assert_eq!(store.load().unwrap().state.pending.len(), 0);
}

/// 即使绕过预检伪造检查结果，提交阶段仍须在暂存移动前拒绝，
/// 并清理本次独占的 incoming 目录，不留下可被下次启动误认的残留。
#[tokio::test]
async fn install_file_rejects_forged_builtin_namespace_before_commit() {
    let (root, store, installer) = bare_installer();
    let path = root.path().join("reserved.kplugin");
    let bytes = package_with_id("kerminal.core", "1.0.0");
    std::fs::write(&path, &bytes).unwrap();
    let id = PluginId::parse("kerminal.core").unwrap();
    let inspection = LocalPackageInspection {
        id: id.clone(),
        version: PluginVersion::parse("1.0.0").unwrap(),
        name: BTreeMap::new(),
        permissions: Vec::new(),
        size: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    };
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = installer
        .install_file(path, inspection, progress, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Infrastructure);
    assert!(store.load().unwrap().state.pending.is_empty());
    assert!(!root.path().join("plugins/kerminal.core").exists());
    assert!(!root.path().join("plugins/.staging/kerminal.core").exists());
    let incoming = root.path().join("plugins/.staging/_incoming");
    if incoming.exists() {
        assert_eq!(std::fs::read_dir(incoming).unwrap().count(), 0);
    }
}

/// 确认入口只服务外部 drop-in；保留身份必须在读取状态和目录前拒绝。
#[tokio::test]
async fn confirm_drop_in_rejects_reserved_identity() {
    let (_root, store, installer) = bare_installer();
    let before = store.load().unwrap().state;
    let error = installer
        .confirm_drop_in(
            PluginId::parse("kerminal.core").unwrap(),
            "bad-fingerprint".into(),
            0,
        )
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    assert_eq!(store.load().unwrap().state, before);
}

/// 内置单元启停只改覆盖项：核心不可停用、默认启用不能重复启用、
/// 停用后可以启用，卸载与缺失记录移除不进入外部安装路径。
#[tokio::test]
async fn builtin_changes_use_builtin_state_not_plugin_records() {
    let (_root, store, installer) = bare_installer();
    let core = PluginId::parse("kerminal.core").unwrap();
    let ssh = PluginId::parse("kerminal.ssh").unwrap();
    let before = store.load().unwrap().state;

    let error = installer
        .change(ExtensionChange::Disable(core.clone()))
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    assert_eq!(store.load().unwrap().state, before);

    let error = installer
        .change(ExtensionChange::Enable(ssh.clone()))
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);

    let receipt = installer
        .change(ExtensionChange::Disable(ssh.clone()))
        .await
        .unwrap();
    assert!(receipt.restart_required);
    let state = store.load().unwrap().state;
    assert!(matches!(
        state.pending.as_slice(),
        [PendingPluginChange::Disable { id }] if id == &ssh
    ));
    assert!(!state.plugins.contains_key(&ssh));

    let validator = ProtocolBootValidator {
        host_api: "1.0.0".into(),
        platform: Platform::current().unwrap(),
    };
    let boot = store.begin_startup(&validator, false).unwrap();
    assert!(!boot.store.state.builtins[&ssh]);
    assert!(boot.store.state.pending.is_empty());

    let steady = store.load().unwrap().state;
    let error = installer
        .change(ExtensionChange::Disable(ssh.clone()))
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(store.load().unwrap().state, steady);

    installer
        .change(ExtensionChange::Enable(ssh.clone()))
        .await
        .unwrap();
    let boot = store.begin_startup(&validator, false).unwrap();
    assert!(!boot.store.state.builtins.contains_key(&ssh));

    let error = installer
        .change(ExtensionChange::Uninstall(ssh.clone()))
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    let error = installer
        .change(ExtensionChange::RemoveMissingRecord(ssh.clone()))
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);

    let unknown = PluginId::parse("acme.unknown").unwrap();
    let error = installer
        .change(ExtensionChange::Enable(unknown))
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::NotFound);
}

/// 提交阶段在解包和完整校验之后仍有独立身份门禁：即使官方目录包能通过
/// 命名空间规则，内置单元也不允许被安装成外部插件，且不会产生暂存槽。
#[test]
fn prepare_rejects_reserved_identity_before_staging_move() {
    let (root, store, installer) = bare_installer();
    let operation_id = uuid::Uuid::new_v4().to_string();
    let directory = root
        .path()
        .join("plugins/.staging/_incoming")
        .join(&operation_id);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("pkg.kplugin"),
        package_with_id("kerminal.core", "1.0.0"),
    )
    .unwrap();
    let stage = Stage {
        directory: directory.clone(),
        operation_id,
    };
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    // 期望值直接取自落盘归档本身：该用例验证的是身份门禁，摘要期望只需与
    // 实际解压流一致即可通过流验证、进入后续校验。
    let (size, digest_hex) = file_digest(&stage.archive(), &CancellationToken::new()).unwrap();
    let verified = archive::ArchiveExpectation {
        size,
        sha256: digest_hex,
    };
    let error = installer
        .prepare_and_commit(
            &stage,
            PluginSource::Official,
            None,
            &verified,
            &progress,
            &CancellationToken::new(),
        )
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    assert_eq!(error.to_string(), "插件身份与内置单元冲突");
    assert!(store.load().unwrap().state.pending.is_empty());
    assert!(!root.path().join("plugins/kerminal.core").exists());
    assert!(!root.path().join("plugins/.staging/kerminal.core").exists());
    assert!(directory.join("package").exists());
    stage.cleanup().unwrap();
}

/// 保留身份记录上的旧卸载 pending 必须被启动清理：否则失败绑定会拒绝撤销，
/// 也会让后续每次启动重复失败。清理后记录仍可见，内置启停可以正常排队。
#[tokio::test]
async fn reserved_record_failed_uninstall_clears_pending_and_allows_builtin_toggle() {
    let (_root, store, installer) = bare_installer();
    let id = PluginId::parse("kerminal.ssh").unwrap();
    update_drop_in_state(&store, |state| {
        state.plugins.insert(
            id.clone(),
            InstalledPlugin {
                active: PluginVersion::parse("1.0.0").unwrap(),
                previous: None,
                enabled: false,
                source: PluginSource::Local,
                installed_at: "2026-01-01T00:00:00Z".into(),
                // 迁移可合法产出缺失目录且无摘要的保留身份记录。
                fingerprint: None,
                status: PluginInstallStatus::Missing,
                confirmed: true,
            },
        );
        state.failures.insert(id.clone(), "旧失败".into());
        state.pending.push(PendingPluginChange::Uninstall {
            id: id.clone(),
            operation_id: uuid::Uuid::new_v4().to_string(),
        });
    });

    let validator = ProtocolBootValidator {
        host_api: "1.0.0".into(),
        platform: Platform::current().unwrap(),
    };
    let boot = store.begin_startup(&validator, false).unwrap();
    assert_eq!(boot.rejected[&id], BootPackageError::ReservedIdentity);
    assert!(boot.store.state.pending.is_empty());
    assert!(boot.store.state.pending_failures.is_empty());
    assert_eq!(boot.store.state.failures[&id], "插件身份与内置单元冲突");
    let stored = &boot.store.state.plugins[&id];
    assert_eq!(
        stored.status,
        PluginInstallStatus::Unsupported {
            reason: PluginUnsupportedReason::ReservedIdentity
        }
    );

    installer
        .change(ExtensionChange::Disable(id.clone()))
        .await
        .unwrap();
    assert!(matches!(
        store.load().unwrap().state.pending.as_slice(),
        [PendingPluginChange::Disable { id: pending }] if pending == &id
    ));
    let boot = store.begin_startup(&validator, false).unwrap();
    assert!(!boot.store.state.builtins[&id]);

    installer
        .change(ExtensionChange::Enable(id.clone()))
        .await
        .unwrap();
    let boot = store.begin_startup(&validator, false).unwrap();
    assert!(!boot.store.state.builtins.contains_key(&id));
}

/// 已知 CAS 失败（同版本重复安装）在暂存槽移走后发生：槽与因此变空的
/// `.staging/<id>` 父目录都必须回收；父目录里并发安装的同 ID 其他 UUID 槽
/// 不能被波及，非空时父目录原样保留。
#[test]
fn reclaim_staging_slot_removes_only_verified_empty_parent() {
    let (root, _store, _installer) = bare_installer();
    let id = PluginId::parse("acme.notes").unwrap();
    let staging = root
        .path()
        .join("plugins")
        .join(".staging")
        .join(id.storage_segment());

    // 空父目录：槽删除后父目录一并消失。
    let slot = staging.join("11111111-1111-1111-1111-111111111111");
    std::fs::create_dir_all(&slot).unwrap();
    reclaim_staging_slot(&slot);
    assert!(!slot.exists());
    assert!(!staging.exists(), "变空的 .staging/<id> 应一并回收");

    // 父目录里有并发安装的兄弟槽：只回收本次槽，父目录与兄弟槽原样保留。
    let sibling = staging.join("22222222-2222-2222-2222-222222222222");
    std::fs::create_dir_all(&sibling).unwrap();
    let slot = staging.join("33333333-3333-3333-3333-333333333333");
    std::fs::create_dir_all(&slot).unwrap();
    reclaim_staging_slot(&slot);
    assert!(!slot.exists());
    assert!(staging.exists(), "非空父目录不得被删除");
    assert!(sibling.exists(), "并发安装的兄弟槽不得被波及");

    // 父目录里有未知文件：证明探测只认“空目录”，不递归也不误删他人材料。
    let slot = staging.join("44444444-4444-4444-4444-444444444444");
    std::fs::create_dir_all(&slot).unwrap();
    std::fs::write(staging.join("unknown-material"), b"keep").unwrap();
    reclaim_staging_slot(&slot);
    assert!(!slot.exists());
    assert!(staging.exists(), "含未知材料的父目录不得被删除");
    assert_eq!(
        std::fs::read(staging.join("unknown-material")).unwrap(),
        b"keep",
        "未知材料必须原样保留"
    );
}

/// 移动后目录同步报 OutcomeUnknown（真实 CommitUnknown 形态：改名已发生）：
/// 安装器必须按“已移动”继续受理状态，安装成功入队并保留槽位；不得把持久性
/// 未知伪称成未移动失败，也不能留下无 pending 引用的孤儿槽。
#[tokio::test]
async fn install_succeeds_when_directory_sync_outcome_is_unknown() {
    let fixture = flow_fixture();
    let path = fixture.root.path().join("local.kplugin");
    std::fs::write(&path, package("1.0.0")).unwrap();
    let inspection = fixture
        .installer
        .inspect_file(path.clone(), CancellationToken::new())
        .await
        .unwrap();
    let _hook = force_unknown_after_staging_move(&fixture);
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let receipt = fixture
        .installer
        .install_file(path, inspection, progress, CancellationToken::new())
        .await
        .unwrap();
    drop(_hook);
    assert!(receipt.restart_required);
    assert!(receipt.cleanup_warning.is_none());
    let state = fixture.store.load().unwrap().state;
    let staged = match state.pending.as_slice() {
        [PendingPluginChange::Install { id, to, staged, .. }]
            if id == &PluginId::parse("acme.notes").unwrap()
                && to == &PluginVersion::parse("1.0.0").unwrap() =>
        {
            staged.clone()
        }
        other => panic!("应排队一次 Install pending，实际 {other:?}"),
    };
    // pending 引用本操作 UUID 的槽：材料必须保留，等待启动恢复裁决。
    let slot = fixture
        .root
        .path()
        .join("plugins/.staging/acme.notes")
        .join(staged);
    assert!(slot.exists(), "OutcomeUnknown 后槽位材料必须保留作恢复证据");
}

/// 改名成功后取消（同步在移动返回后触发）：安装失败，但槽位已真实存在于
/// 磁盘上，取消路径仍应回收本次独占的槽与空父目录，不留孤儿材料。
#[tokio::test]
async fn cancel_after_move_reclaims_slot_and_empty_parent() {
    let fixture = flow_fixture();
    let path = fixture.root.path().join("local.kplugin");
    std::fs::write(&path, package("1.0.0")).unwrap();
    let inspection = fixture
        .installer
        .inspect_file(path.clone(), CancellationToken::new())
        .await
        .unwrap();
    let cancellation = CancellationToken::new();
    // after_move 动作在移动返回后立即取消：此时槽位已真实落盘，取消分支
    // 必须面对“已移动”的事实回收材料，而不是把槽留给下次启动误认。
    let root = fixture.store.root().to_path_buf();
    let cancel = cancellation.clone();
    let guard = {
        let _registration = lock_test_hooks(&UNPACK_HOOK_LOCK);
        test_hooks::after_move::set(
            &root,
            Box::new(move |result: &mut Result<(), LayoutError>| {
                if result.is_ok() {
                    cancel.cancel();
                }
            }),
        );
        AfterMoveHookGuard { root }
    };
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let error = fixture
        .installer
        .install_file(path, inspection, progress, cancellation)
        .await
        .unwrap_err();
    drop(guard);
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
    assert!(fixture.store.load().unwrap().state.pending.is_empty());
    // 槽与空壳父目录都被回收；_incoming 的临时材料清空。
    assert!(
        !fixture
            .root
            .path()
            .join("plugins/.staging/acme.notes")
            .exists()
    );
    let incoming = fixture.root.path().join("plugins/.staging/_incoming");
    if incoming.exists() {
        assert_eq!(std::fs::read_dir(incoming).unwrap().count(), 0);
    }
}

/// 注入动作 panic 不得毒化登记表锁：panic 之后，其他线程的登记、命中、
/// 清理（含 RAII 守卫 Drop）都必须照常工作，测试进程不被一次动作失败拖垮。
#[test]
fn hook_action_panic_does_not_poison_registry_lock() {
    let poisoned = tempfile::tempdir().unwrap();
    // 1) 在锁外执行的动作 panic：run 的调用方按自己的语义处理，这里证明
    //    panic 传播但不破坏登记表（unwind 后锁未中毒）。
    test_hooks::before_unpack::set(
        poisoned.path(),
        Box::new(|_: &std::path::Path| panic!("注入动作本身失败")),
    );
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        test_hooks::before_unpack::run(poisoned.path(), &staged_archive_path(poisoned.path()));
    }));
    assert!(panicked.is_err(), "动作 panic 应如实传播给调用方");
    test_hooks::before_unpack::clear(poisoned.path());

    // 2) panic 之后并行线程的登记/命中/清理全部正常，守卫 Drop 不因中毒失败。
    let healthy = tempfile::tempdir().unwrap();
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = hits.clone();
    let root = healthy.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        {
            let _registration = lock_test_hooks(&UNPACK_HOOK_LOCK);
            test_hooks::before_unpack::set(
                &root,
                Box::new(move |_| {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }),
            );
        }
        let guard = ThreadRootGuard { root: root.clone() };
        test_hooks::before_unpack::run(&root, &staged_archive_path(&root));
        {
            let _registration = lock_test_hooks(&UNPACK_HOOK_LOCK);
            test_hooks::before_unpack::clear(&root);
        }
        drop(guard);
    });
    handle
        .join()
        .expect("panic 后并行登记/清理线程必须正常完成");
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "panic 后其他根的动作必须照常命中一次"
    );
    // 守卫与显式清理后不再命中。
    test_hooks::before_unpack::run(healthy.path(), &staged_archive_path(healthy.path()));
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
}
