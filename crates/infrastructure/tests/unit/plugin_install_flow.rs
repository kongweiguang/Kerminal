//! 通过隔离的真实签名和 HTTP 服务覆盖安装到重启的状态流转。
//! @author kongweiguang

use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use blake2::{Blake2b512, Digest as _};
use ed25519_dalek::{Signer, SigningKey};
use kerminal_application::extensions::CatalogSource;
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
    let manifest = serde_json::json!({"schema":1,"id":"acme.notes","name":{"en":"Notes"},"description":{"en":"Notes"},"version":version,"api":"^1.0","publisher":"acme","runtime":{"kind":"none"}}).to_string();
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

/// 真实脚本返回可辨认的笔记数据，验证不会由测试桩伪造握手或视图结果。
fn script_package(version: &str) -> Vec<u8> {
    let manifest = serde_json::json!({"schema":1,"id":"acme.notes","name":{"en":"Notes"},"description":{"en":"Notes"},"version":version,"api":"^1.0","publisher":"acme","runtime":{"kind":"script","main":"main.js","activation":"on-demand"}}).to_string();
    let script = format!(
        r#"kerminal.onRequest("view.open", (params) => ({{ data: {{ text: "note {version}", instance: params.instance }} }}));"#
    );
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(encoder);
    for (path, body) in [
        ("kerminal-plugin.json", manifest.as_bytes()),
        ("main.js", script.as_bytes()),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, path, body).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap()
}

/// 真实监督器持有进程到关闭回执；脚本宿主从冻结的版本目录加载插件。
async fn exercise_script_host(
    root: &std::path::Path,
    executable: &std::path::Path,
    plugin: &crate::plugins::boot::BootPlugin<
        kerminal_application::external_contributions::PreparedPlugin,
    >,
) {
    use crate::plugins::{process::PluginProcessSpec, supervisor::ProcessSupervisor};
    use kerminal_application::{ApplicationTaskExecutor, plugin_ports::PluginProcessPort};
    use std::time::{Duration, Instant};
    let supervisor = ProcessSupervisor::new(
        vec![PluginProcessSpec {
            id: plugin.id.clone(),
            version: plugin.version.clone(),
            executable: executable.to_path_buf(),
            arguments: vec![plugin.directory.as_os_str().to_os_string()],
            data_directory: root.join("plugins/data").join(plugin.id.as_str()),
            log_directory: root.join("plugins/logs"),
            locale: "en".into(),
            theme: "dark".into(),
            host_api: "1.0.0".into(),
        }],
        ApplicationTaskExecutor::new(tokio::runtime::Handle::current()),
    )
    .unwrap();
    let generation = supervisor
        .activate(&plugin.id, Instant::now() + Duration::from_secs(15))
        .await
        .expect("real script host handshake");
    let result = supervisor
        .call(
            &plugin.id,
            generation,
            "view.open",
            serde_json::json!({"instance":"notes-1","view":"notes.editor","scope":{},"state":null}),
            Instant::now() + Duration::from_secs(10),
            CancellationToken::new(),
        )
        .await
        .expect("open notes");
    assert_eq!(result["data"]["text"], format!("note {}", plugin.version));
    assert_eq!(result["data"]["instance"], "notes-1");
    supervisor
        .shutdown_until(Instant::now() + Duration::from_secs(10))
        .await
        .expect("reap script host");
}

/// 同一生命周期既可运行快速状态测试，也可在显式验收中使用真实脚本宿主。
async fn signed_catalog_lifecycle(host: Option<&std::path::Path>) {
    let host_digest = host.map(|path| file_digest(path).expect("read script host executable"));
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
        let bytes = if host.is_some() { script_package(version) } else { package(version) };
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
        if let Some(executable) = host {
            exercise_script_host(root.path(), executable, &boot.plugins[0]).await;
            assert_eq!(file_digest(executable).unwrap(), host_digest.unwrap());
        }
        store
            .mark_boot_phase(&boot.boot_id, PluginBootPhase::RegistryFrozen)
            .unwrap();
        store
            .mark_boot_phase(&boot.boot_id, PluginBootPhase::FirstFrame)
            .unwrap();
    }
    installer
        .change(ExtensionChange::Rollback(id.clone()))
        .await
        .unwrap();
    let boot = store.begin_startup(&validator, false).unwrap();
    assert_eq!(
        boot.plugins[0].version,
        PluginVersion::parse("1.0.0").unwrap()
    );
    if let Some(executable) = host {
        exercise_script_host(root.path(), executable, &boot.plugins[0]).await;
        assert_eq!(file_digest(executable).unwrap(), host_digest.unwrap());
    }
    store
        .mark_boot_phase(&boot.boot_id, PluginBootPhase::RegistryFrozen)
        .unwrap();
    store
        .mark_boot_phase(&boot.boot_id, PluginBootPhase::FirstFrame)
        .unwrap();
    installer
        .change(ExtensionChange::Uninstall(id))
        .await
        .unwrap();
    let boot = store.begin_startup(&validator, false).unwrap();
    assert!(boot.plugins.is_empty());
    assert_eq!(std::fs::read(data).unwrap(), b"keep");
    if let Some(executable) = host {
        assert_eq!(file_digest(executable).unwrap(), host_digest.unwrap());
    }
    server.abort();
}

/// 状态只在下次启动切换，卸载不会清除用户数据目录。
#[tokio::test]
async fn signed_catalog_install_update_rollback_uninstall_survives_restart() {
    signed_catalog_lifecycle(None).await;
}

/// 显式要求已构建的真实宿主；没有外部依赖时不能报告验收通过。
#[tokio::test]
#[ignore = "requires built KERMINAL_PLUGIN_HOST"]
async fn signed_catalog_script_host_lifecycle() {
    let executable = std::path::PathBuf::from(
        std::env::var_os("KERMINAL_PLUGIN_HOST")
            .expect("KERMINAL_PLUGIN_HOST must name a built script host"),
    );
    assert!(
        executable.is_absolute() && executable.is_file(),
        "KERMINAL_PLUGIN_HOST must be an existing absolute executable path"
    );
    signed_catalog_lifecycle(Some(&executable)).await;
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
