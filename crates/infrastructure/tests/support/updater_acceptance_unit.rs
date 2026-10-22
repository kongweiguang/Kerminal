//! updater 的本机 acceptance：真实 SDK manifest、HTTPS 下载验签和平台实际安装目标。
//!
//! @author kongweiguang

#[cfg(target_os = "linux")]
use std::os::unix::fs::PermissionsExt;
#[cfg(windows)]
use std::time::Instant;
#[cfg(any(target_os = "linux", windows))]
use std::{path::PathBuf, time::Duration};

#[cfg(target_os = "linux")]
use base64::{Engine, engine::general_purpose::STANDARD};
#[cfg(any(target_os = "linux", windows))]
use cargo_packager_updater::reqwest::Url;
#[cfg(target_os = "linux")]
use kerminal_application::{UpdateInstallDisposition, UpdateInstallResult, UpdateService};
#[cfg(target_os = "linux")]
use serde_json::Value;
#[cfg(target_os = "linux")]
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
#[cfg(target_os = "linux")]
use tokio_rustls::server::TlsStream;
#[cfg(target_os = "linux")]
use tokio_util::sync::CancellationToken;

#[cfg(target_os = "linux")]
use super::super::{CargoPackagerUpdater, settings::UpdaterSettings};

#[cfg(target_os = "linux")]
use super::super::tls_test_support as tls;

#[cfg(target_os = "linux")]
const TEST_PUBLIC_KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
#[cfg(target_os = "linux")]
const TEST_SIGNATURE: &str = concat!(
    "untrusted comment: signature from minisign secret key\n",
    "RUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/",
    "z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\n",
    "trusted comment: timestamp:1556193335\tfile:test\n",
    "y/rUw2y8/hOUYjZU71eHp/Wo1KZ40fGy2VJEDl34XMJM+TX48Ss/17u3IvIfbVR1FkZZSNCisQbuQY+bHwhEBg=="
);
#[cfg(target_os = "linux")]
const TEST_ARTIFACT: &[u8] = b"test";

/// 复用 SDK 公开 target 键改写冻结 manifest，避免为测试扩大 metadata 私有 API。
#[cfg(target_os = "linux")]
fn local_sdk_manifest(artifact_url: &Url) -> Vec<u8> {
    let fixture = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/updater/cargo-packager-updater-0.2.3.json"
    ))
    .expect("read packaging-generated SDK manifest");
    let mut manifest: Value = serde_json::from_slice(&fixture).expect("parse SDK manifest JSON");
    let json_target = cargo_packager_updater::target().expect("read SDK target");
    let platform = manifest["platforms"][&json_target]
        .as_object_mut()
        .expect("packaging manifest contains current SDK target");
    platform.insert("url".to_owned(), Value::String(artifact_url.to_string()));
    platform.insert(
        "signature".to_owned(),
        Value::String(STANDARD.encode(TEST_SIGNATURE)),
    );
    serde_json::to_vec(&manifest).expect("encode SDK-compatible local manifest")
}

/// 在隔离 loopback HTTPS listener 上响应 SDK metadata 和制品请求。
#[cfg(target_os = "linux")]
async fn local_feed_server(artifact: Vec<u8>) -> (Url, JoinHandle<()>) {
    let (listener, address) = tls::bind_loopback().await;
    let acceptor = tls::test_tls_acceptor();
    let artifact_url = tls::https_url(address, "/candidate.AppImage");
    let manifest = local_sdk_manifest(&artifact_url);
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let Ok(Ok((stream, _))) =
                tokio::time::timeout(Duration::from_secs(3), listener.accept()).await
            else {
                break;
            };
            let Ok(stream) =
                tokio::time::timeout(Duration::from_secs(3), acceptor.accept(stream)).await
            else {
                break;
            };
            let Ok(stream) = stream else {
                break;
            };
            serve_local_request(stream, &manifest, &artifact).await;
        }
    });
    (tls::https_url(address, "/feed.json"), server)
}

/// 按路径返回 SDK feed 或 Minisign 已签名制品，不记录请求头或外部身份数据。
#[cfg(target_os = "linux")]
async fn serve_local_request(stream: TlsStream<TcpStream>, manifest: &[u8], artifact: &[u8]) {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await.is_err() || request_line.len() > 4096 {
        return;
    }
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).await.is_err() || header.len() > 4096 {
            return;
        }
        if header == "\r\n" || header.is_empty() {
            break;
        }
    }
    let Some(path) = request_line.split_whitespace().nth(1) else {
        return;
    };
    let (content_type, body) = match path {
        "/feed.json" => ("application/json", manifest),
        "/candidate.AppImage" => ("application/octet-stream", artifact),
        _ => ("text/plain", b"not found".as_slice()),
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut stream = reader.into_inner();
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.write_all(body).await;
    let _ = stream.flush().await;
}

/// 用真实策略构造 HTTPS feed；目标路径由子进程的 APPIMAGE 运行态解析。
#[cfg(target_os = "linux")]
fn local_settings(feed_url: Url) -> UpdaterSettings {
    let test_public_key = STANDARD.encode(format!(
        "untrusted comment: isolated updater acceptance key\n{TEST_PUBLIC_KEY}"
    ));
    UpdaterSettings::new(
        "0.3.9",
        vec![feed_url],
        test_public_key,
        Duration::from_secs(5),
        1024 * 1024,
    )
    .expect("construct strict HTTPS updater settings")
}

/// 以真实 APPIMAGE 环境变量在子进程验证平台探测和安装路径，不修改测试进程环境。
#[cfg(target_os = "linux")]
#[test]
fn linux_appimage_upgrade_uses_real_runtime_path_in_subprocess() {
    use std::process::Command;

    for case in ["success", "preflight-retry"] {
        let root = tempfile::tempdir().expect("create isolated updater acceptance directory");
        let install_path = root.path().join("Kerminal-test.AppImage");
        std::fs::write(&install_path, b"previous test payload").expect("seed test AppImage target");
        std::fs::set_permissions(&install_path, std::fs::Permissions::from_mode(0o755))
            .expect("make isolated target executable");
        let test_binary = std::env::current_exe().expect("locate current Rust test binary");
        let status = Command::new(test_binary)
            .args([
                "--exact",
                "platform::updater::runtime::acceptance_tests::linux_appimage_install_child",
                "--nocapture",
            ])
            .env("APPIMAGE", &install_path)
            .env("KERMINAL_TEST_UPDATER_CASE", case)
            .env("KERMINAL_TEST_UPDATER_ROOT", root.path())
            .status()
            .expect("start isolated AppImage updater child");
        assert!(status.success(), "isolated updater child failed for {case}");
        assert_eq!(
            std::fs::read(&install_path).expect("read child-updated temporary target"),
            TEST_ARTIFACT,
            "SDK should replace only the child's APPIMAGE target"
        );
    }
}

/// 子进程使用正常 runtime 检测与同一 HTTPS/验签路径，测试数据全部留在父测临时目录。
#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_appimage_install_child() {
    let Some(case) = std::env::var_os("KERMINAL_TEST_UPDATER_CASE") else {
        return;
    };
    let root = PathBuf::from(
        std::env::var_os("KERMINAL_TEST_UPDATER_ROOT").expect("isolated test root is provided"),
    );
    let install_path = PathBuf::from(std::env::var_os("APPIMAGE").expect("child APPIMAGE path"));
    let original = b"previous test payload";
    let (feed_url, server) = local_feed_server(TEST_ARTIFACT.to_vec()).await;
    let adapter = CargoPackagerUpdater::from_settings(
        Some(local_settings(feed_url)),
        root.join("recovery"),
        vec![tls::trusted_test_root()],
    )
    .expect("initialize strict updater with an isolated trust root");
    let service = UpdateService::new(std::sync::Arc::new(adapter));
    let candidate = service
        .check(CancellationToken::new())
        .await
        .expect("check local SDK feed")
        .expect("new SDK release is available");
    assert_eq!(candidate.version, "0.4.0-dev.0");

    let (reporter, progress) = UpdateService::progress_channel();
    let downloaded = service
        .download_with_cancellation(
            candidate.id,
            CancellationToken::new(),
            CancellationToken::new(),
            reporter,
        )
        .await
        .expect("download and verify signed test candidate");
    let pending = service
        .recoveries()
        .await
        .expect("read durable pending update recovery");
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].state,
        kerminal_application::updates::UpdateRecoveryState::PendingConfirmation
    );
    assert_eq!(
        progress
            .next(CancellationToken::new())
            .await
            .expect("observe completed download progress")
            .received_bytes,
        TEST_ARTIFACT.len() as u64
    );

    if case == "preflight-retry" {
        std::fs::remove_file(&install_path).expect("simulate a missing AppImage target");
        assert!(matches!(
            service.install(downloaded.id).await,
            UpdateInstallResult::NotStarted(_)
        ));
        std::fs::write(&install_path, original).expect("restore isolated target for retry");
        std::fs::set_permissions(&install_path, std::fs::Permissions::from_mode(0o755))
            .expect("restore executable permission for retry");
    } else {
        assert_eq!(case, "success", "child case must be known");
    }

    let disposition = service.install(downloaded.id).await;
    server.await.expect("join local SDK HTTPS feed");
    assert!(matches!(
        disposition,
        UpdateInstallResult::Installed(UpdateInstallDisposition::RestartRequired)
    ));
    assert_eq!(
        std::fs::read(&install_path).expect("read isolated SDK install target"),
        TEST_ARTIFACT
    );
    let unresolved = service
        .recoveries()
        .await
        .expect("read retained post-install recovery");
    assert_eq!(unresolved.len(), 1);
    assert!(
        unresolved[0]
            .recovery_path
            .join("verified-update.bin")
            .is_file()
    );
}
/// Windows 父测在隔离子进程中验证 SDK 会退出宿主并且退出发生在 executor 收尾之后。
#[cfg(windows)]
#[test]
fn windows_sdk_installer_exit_is_contained_in_child_process() {
    use std::{process::Command, thread};

    let root = tempfile::tempdir().expect("create isolated Windows SDK test directory");
    let installer = compile_noop_windows_installer(root.path());
    let shutdown_marker = root.path().join("executor-shutdown.marker");
    let install_started_marker = root.path().join("installer-started.marker");
    let test_binary = std::env::current_exe().expect("locate current Rust test binary");
    let status = Command::new(test_binary)
        .arg("windows_sdk_install_exit_child")
        .arg("--nocapture")
        .env("KERMINAL_TEST_UPDATER_CHILD_SHUTDOWN", &shutdown_marker)
        .env("KERMINAL_TEST_UPDATER_CHILD_INSTALLER", &installer)
        .env(
            "KERMINAL_TEST_UPDATER_INSTALLER_MARKER",
            &install_started_marker,
        )
        .status()
        .expect("start isolated SDK child test");

    assert!(
        status.success(),
        "SDK child should exit through its NSIS path"
    );
    assert_eq!(
        std::fs::read(&shutdown_marker).expect("read pre-install shutdown marker"),
        b"executor-shutdown"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !install_started_marker.exists() {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        install_started_marker.exists(),
        "SDK did not start the inert test installer"
    );
}

/// 子进程只在父测设置隔离路径后运行，并在 SDK exit 前确认任务入口已关闭且队列已收尾。
///
/// 夹具只覆盖需要验证的 Passive 安装选项，其余 SDK 配置保留默认，避免耦合无关字段。
#[cfg(windows)]
#[test]
fn windows_sdk_install_exit_child() {
    use cargo_packager_updater::{
        Config, Update, UpdateFormat, WindowsConfig, WindowsUpdateInstallMode,
        reqwest::header::HeaderMap,
    };
    use kerminal_application::ApplicationTaskExecutor;
    use std::time::Duration;

    let (Some(shutdown_marker), Some(installer_path), Some(installer_marker)) = (
        std::env::var_os("KERMINAL_TEST_UPDATER_CHILD_SHUTDOWN"),
        std::env::var_os("KERMINAL_TEST_UPDATER_CHILD_INSTALLER"),
        std::env::var_os("KERMINAL_TEST_UPDATER_INSTALLER_MARKER"),
    ) else {
        return;
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build isolated app runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    runtime.block_on(async {
        executor
            .begin_shutdown()
            .expect("close application task intake");
        executor
            .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
            .expect("join application tasks before SDK install");
    });
    std::fs::write(&shutdown_marker, b"executor-shutdown")
        .expect("record completed application executor shutdown");

    assert!(PathBuf::from(&installer_marker).is_absolute());
    let installer_path = PathBuf::from(installer_path);
    let installer_bytes = std::fs::read(&installer_path).expect("read inert installer stub");
    let config = Config {
        windows: Some(WindowsConfig {
            installer_args: None,
            install_mode: Some(WindowsUpdateInstallMode::Passive),
        }),
        ..Config::default()
    };
    let update = Update {
        config,
        body: None,
        current_version: "0.3.9".to_owned(),
        version: "0.4.0-dev.0".to_owned(),
        date: None,
        target: "windows".to_owned(),
        extract_path: PathBuf::from("unused-test-extract-path"),
        download_url: Url::parse("https://updates.invalid/test.exe").expect("test URL"),
        signature: String::new(),
        timeout: None,
        headers: HeaderMap::new(),
        format: UpdateFormat::Nsis,
    };
    update
        .install(installer_bytes)
        .expect("start inert SDK installer");
    panic!("cargo-packager-updater NSIS install is expected to exit the child process");
}

/// 由当前 Windows Rust 工具链生成无副作用 installer stub，子进程不会修改安装目录或注册表。
#[cfg(windows)]
fn compile_noop_windows_installer(root: &std::path::Path) -> PathBuf {
    use std::process::Command;

    let source = root.join("inert_installer.rs");
    let binary = root.join("inert_installer.exe");
    std::fs::write(
        &source,
        "fn main() { if let Ok(path) = std::env::var(\"KERMINAL_TEST_UPDATER_INSTALLER_MARKER\") { let _ = std::fs::write(path, b\"started\"); } }",
    )
    .expect("write inert installer source");
    let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let result = Command::new(compiler)
        .arg("--edition=2024")
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .status()
        .expect("run current Rust compiler for inert installer");
    assert!(result.success(), "inert test installer compilation failed");
    binary
}
