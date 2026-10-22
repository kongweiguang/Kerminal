//! RDP endpoint、临时文件脱敏和客户端探测的隔离契约测试。
//!
//! @author kongweiguang

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use kerminal_application::{AppError, connections::RdpLaunchTarget};
use kerminal_domain::connections::HostEndpoint;
use tokio::sync::{Semaphore, watch};
use uuid::Uuid;

use super::{ManagedClient, SystemRdpPlatformLauncher, artifact, platform};

/// 为平台测试创建唯一目录，避免并行运行时覆盖用户文件或其它测试产物。
fn temporary_directory() -> PathBuf {
    std::env::temp_dir().join(format!("kerminal-rdp-test-{}", Uuid::new_v4()))
}

/// 构造能覆盖 IPv6、可选用户名语义的 RDP endpoint。
fn endpoint(username: &str) -> HostEndpoint {
    HostEndpoint {
        host: "2001:db8::20".to_owned(),
        port: 3390,
        username: username.to_owned(),
    }
}

/// 确认空用户名允许系统客户端提示，IPv6 端点保持无歧义且不虚构 password 字段。
#[test]
fn rdp_file_omits_empty_username_and_requests_prompt() {
    let content = artifact::rdp_file_content(&endpoint("  "), None, &Default::default());

    assert!(content.contains("full address:s:[2001:db8::20]:3390\r\n"));
    assert!(content.contains("prompt for credentials:i:1\r\n"));
    assert!(!content.contains("username:s:"));
    assert!(!content.contains("password 51:b:"));
}

/// 确认已有用户名和 DPAPI 密文字段只按 RDP 的受控格式序列化。
#[test]
fn rdp_file_contains_only_supplied_ciphertext() {
    let content =
        artifact::rdp_file_content(&endpoint("alice"), Some("01aabbff"), &Default::default());

    assert!(content.contains("username:s:alice\r\n"));
    assert!(content.contains("prompt for credentials:i:0\r\n"));
    assert!(content.contains("password 51:b:01aabbff\r\n"));
    assert!(!content.contains("plain-password"));
}

/// 显示配置必须进入实际 RDP 文件，不允许表单保存成功但客户端始终以全屏启动。
///
/// @author kongweiguang
#[test]
fn rdp_file_respects_window_mode_and_paired_dimensions() {
    let display = kerminal_application::connections::management::RdpDisplayOptions {
        fullscreen: false,
        width: Some(1440),
        height: Some(900),
    };
    let content = artifact::rdp_file_content(&endpoint("alice"), None, &display);
    assert!(content.contains("screen mode id:i:1\r\n"));
    assert!(content.contains("desktopwidth:i:1440\r\n"));
    assert!(content.contains("desktopheight:i:900\r\n"));
    let fullscreen = artifact::rdp_file_content(&endpoint("alice"), None, &Default::default());
    assert!(fullscreen.contains("screen mode id:i:2\r\n"));
    assert!(!fullscreen.contains("desktopwidth:"));
}

/// 确认 CR/LF 注入和空 endpoint 被拒绝，不能附加额外 RDP 属性或改写目标。
#[test]
fn rejects_endpoint_line_injection() {
    let mut invalid = endpoint("alice\r\npassword 51:b:plaintext");
    assert!(artifact::validate_endpoint(&invalid).is_err());
    invalid.username = "alice".to_owned();
    invalid.host = "\r\nredirectclipboard:i:0".to_owned();
    assert!(artifact::validate_endpoint(&invalid).is_err());
}

/// 确认受管 `.rdp` 文件使用安全创建路径，并在进程 owner 完成后删除。
#[tokio::test]
async fn explicit_cleanup_removes_rdp_artifact() {
    let directory = temporary_directory();
    let artifact = artifact::create_rdp_artifact(
        &directory,
        &endpoint("alice"),
        Some("aabbcc"),
        &Default::default(),
    )
    .expect("create temporary RDP file");
    let path = artifact.path().to_path_buf();
    let content = fs::read_to_string(&path).expect("read generated RDP file");
    assert!(content.contains("password 51:b:aabbcc\r\n"));
    assert!(!content.contains("plaintext"));

    artifact.cleanup().await.expect("remove temporary RDP file");

    assert!(!path.exists());
    fs::remove_dir(&directory).expect("remove empty test directory");
}

/// 超过关闭 deadline 时保留 owner task，允许应用在清理宽限期内再次等待同一 worker。
///
/// @author kongweiguang
#[tokio::test]
async fn shutdown_timeout_retains_client_worker_for_retry() {
    let launcher =
        SystemRdpPlatformLauncher::new(temporary_directory()).expect("runtime directory is valid");
    let gate = Arc::new(Semaphore::new(0));
    let worker_gate = Arc::clone(&gate);
    let cancellation = launcher.lifetime.child_token();
    let (launch_id, status) = launcher
        .status_hub
        .register(kerminal_application::connections::RdpClientLifecycle::ManagedProcess)
        .expect("status capacity is available");
    let (cleanup_deadline, deadline_observer) = watch::channel(None);
    let mut worker_deadline_observer = deadline_observer.clone();
    let worker_status = status.clone();
    let task = tokio::spawn(async move {
        cancellation.cancelled().await;
        let first_deadline = (*worker_deadline_observer.borrow_and_update())
            .expect("shutdown publishes its first deadline before cancellation");
        worker_deadline_observer
            .changed()
            .await
            .expect("launcher retains the retry deadline sender");
        let retry_deadline = *worker_deadline_observer.borrow_and_update();
        assert!(retry_deadline.expect("retry publishes a deadline") > first_deadline);
        let _permit = worker_gate.acquire().await.expect("test gate remains open");
        worker_status
            .publish(kerminal_application::connections::RdpLaunchStatus::Closed)
            .expect("worker publishes its terminal state");
        Ok::<(), AppError>(())
    });
    launcher.clients.lock().await.insert(
        launch_id.as_uuid(),
        ManagedClient {
            cancellation: launcher.lifetime.child_token(),
            cleanup_deadline,
            task,
            status,
        },
    );

    let first_deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(20);
    assert!(launcher.shutdown_until(first_deadline).await.is_err());
    assert_eq!(*deadline_observer.borrow(), Some(first_deadline));
    assert_eq!(launcher.clients.lock().await.len(), 1);

    gate.add_permits(1);
    let retry_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    let closed_clients = launcher
        .shutdown_until(retry_deadline)
        .await
        .expect("retry joins the client after the worker exits");
    assert_eq!(closed_clients, 1);
    assert_eq!(*deadline_observer.borrow(), Some(retry_deadline));
    assert!(launcher.clients.lock().await.is_empty());
}

/// 验证 RDP child 经统一 process-tree wrapper 创建后仍可正常等待和回收。
///
/// @author kongweiguang
#[cfg(any(unix, windows))]
#[tokio::test]
async fn managed_process_tree_spawn_reaps_successful_child() {
    use crate::process_tree::{ProcessTreeGuard, spawn::ProcessSpawnOptions};

    #[cfg(unix)]
    let command = {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "exit 0"]);
        command
    };
    #[cfg(windows)]
    let command = {
        let mut command = tokio::process::Command::new("cmd.exe");
        command.args(["/C", "exit 0"]);
        command
    };

    let mut child = ProcessTreeGuard::spawn_tokio(
        command,
        ProcessSpawnOptions {
            kill_on_drop: true,
            windows_creation_flags: if cfg!(windows) { 0x0800_0000 } else { 0 },
        },
    )
    .expect("managed child starts without a fallback to bare spawn");
    assert!(child.id().is_some());
    let status = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
        .await
        .expect("child exits within the test deadline")
        .expect("child wait succeeds");
    assert!(status.success());
}

/// 确认 PATH 搜索按候选目录和名称顺序选择真实可执行文件。
#[test]
fn executable_probe_uses_ordered_directories() {
    let first = temporary_directory();
    let second = temporary_directory();
    fs::create_dir_all(&first).expect("create first directory");
    fs::create_dir_all(&second).expect("create second directory");
    let expected = second.join("xfreerdp3");
    fs::write(&expected, b"test executable marker").expect("create executable marker");
    mark_executable(&expected);

    let found = platform::find_executable_in([first.as_path(), second.as_path()], &["xfreerdp3"]);

    assert_eq!(found, Some(expected.clone()));
    fs::remove_file(&expected).expect("remove executable marker");
    fs::remove_dir(&first).expect("remove first directory");
    fs::remove_dir(&second).expect("remove second directory");
}

/// 在 Unix 测试文件上设置执行权限，使探测规则与 FreeRDP 启动要求一致。
fn mark_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)
            .expect("read marker metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).expect("set executable mode");
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Windows 主机实际调用 DPAPI 并检查密文、文件内容与 owner cleanup。
#[cfg(target_os = "windows")]
#[tokio::test]
async fn windows_launch_plan_keeps_dpapi_password_out_of_argv_and_cleans_file() {
    let secret = "kerminal-rdp-secret-check";
    let directory = temporary_directory();
    let plan = platform::build_launch_plan(
        &directory,
        RdpLaunchTarget {
            endpoint: endpoint("alice"),
            password: Some(secret.to_owned().into()),
            display: Default::default(),
        },
    )
    .await
    .expect("Windows MSTSC and DPAPI launch plan are available");
    let path = plan
        .artifact
        .as_ref()
        .expect("Windows launch owns a temporary RDP file")
        .path()
        .to_path_buf();
    let content = fs::read_to_string(&path).expect("read protected RDP file");
    let args = plan
        .command()
        .as_std()
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(args, vec![path.as_os_str().to_string_lossy().into_owned()]);
    assert!(plan.spawn_options().kill_on_drop);
    assert_eq!(plan.spawn_options().windows_creation_flags, 0x0800_0000);
    assert!(!args.iter().any(|argument| argument.contains(secret)));
    let encrypted = content
        .lines()
        .find_map(|line| line.strip_prefix("password 51:b:"))
        .expect("RDP file includes MSTSC protected credential field");
    assert!(!encrypted.is_empty());
    assert!(encrypted.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(!content.contains(secret));
    plan.cleanup().await.expect("clean launch plan artifact");
    assert!(!path.exists());
    fs::remove_dir(&directory).expect("remove empty test directory");
}
