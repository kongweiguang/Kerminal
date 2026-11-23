//! 使用任务私有 WSL lrzsz 对接生产 SessionManager，验证字节流和落盘结果。
//!
//! @author kongweiguang

#![cfg(windows)]

use std::{path::Path, process::Stdio, sync::Arc, time::Duration};

use async_trait::async_trait;
use kerminal_application::{
    AppResult,
    sessions::{
        CreateSessionRequest, PlainTextSnapshotRequest, SessionService, SessionTransport,
        SessionTransportControl, SessionTransportFactory, SessionTransportOpenError,
        UiSessionBinding, ZmodemDecision, ZmodemDirection, ZmodemPhase,
    },
};
use kerminal_domain::{
    connections::RemoteShellDialect,
    identity::{HostId, PaneId, TabId},
    sessions::{SessionExitStatus, SessionOwner, SessionTarget, SshSessionTarget, TerminalSize},
};
use kerminal_infrastructure::sessions::{SessionManager, SessionRuntimeConfig};
use tokio::{process::Command, sync::Notify, time::Instant};
use tokio_util::sync::CancellationToken;

struct LrzszFactory {
    command: String,
    cwd: String,
}

struct LrzszControl {
    cancel: CancellationToken,
    exited: Arc<Notify>,
}

#[async_trait]
impl SessionTransportFactory for LrzszFactory {
    /// 测试显式使用临时 WSL 进程的管道，生产 reader/writer 仍是唯一的会话所有者。
    async fn open(
        &self,
        _target: &SessionTarget,
        _size: TerminalSize,
        _cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        let mut child = Command::new("wsl.exe")
            .args([
                "-d",
                "Ubuntu-22.04",
                "--cd",
                &self.cwd,
                "--",
                "/bin/sh",
                "-c",
                &self.command,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("start isolated WSL lrzsz");
        let reader = child.stdout.take().expect("child stdout");
        let writer = child.stdin.take().expect("child stdin");
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let exited = Arc::new(Notify::new());
        let task_exited = Arc::clone(&exited);
        tokio::spawn(async move {
            tokio::select! {
                _ = child.wait() => {},
                _ = task_cancel.cancelled() => { let _ = child.kill().await; },
            }
            task_exited.notify_one();
        });
        Ok(
            SessionTransport::new(reader, writer, LrzszControl { cancel, exited })
                .with_shell_dialect(RemoteShellDialect::Posix)
                .with_shell_integration_enabled(true),
        )
    }
}

#[async_trait]
impl SessionTransportControl for LrzszControl {
    /// 虚拟 SSH fixture 不依赖窗口尺寸，避免把真实传输与 UI 几何耦合。
    async fn resize(&self, _size: TerminalSize) -> AppResult<()> {
        Ok(())
    }

    /// 清理只结束由本测试创建的子进程，避免影响用户其他 WSL 会话。
    async fn close_until(&self, _deadline: Instant) -> AppResult<()> {
        self.cancel.cancel();
        Ok(())
    }

    /// 此 fixture 的生命周期由测试显式结束，避免进程等待与关闭竞争同一锁。
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        self.exited.notified().await;
        Ok(SessionExitStatus {
            code: Some(0),
            signal: None,
            success: true,
        })
    }
}

/// 将 Windows 临时目录转换成 WSL 能访问的驱动器路径，拒绝网络共享和可疑路径。
fn wsl_path(path: &Path) -> String {
    let absolute = path.canonicalize().expect("resolve fixture directory");
    let path = absolute
        .to_str()
        .expect("UTF-8 fixture directory")
        .trim_start_matches(r"\\?\");
    let (drive, rest) = path.split_once(':').expect("drive path");
    assert_eq!(drive.len(), 1);
    format!(
        "/mnt/{}/{}",
        drive.to_ascii_lowercase(),
        rest.trim_start_matches('\\').replace('\\', "/")
    )
}

/// 启动真实 lrzsz 作为 SSH transport peer，通过真实服务获取 session generation。
async fn session(command: String, cwd: &Path) -> (Arc<SessionManager>, UiSessionBinding) {
    let factory = Arc::new(LrzszFactory {
        command,
        cwd: wsl_path(cwd),
    });
    let manager = Arc::new(SessionManager::new(
        factory,
        tokio::runtime::Handle::current(),
        SessionRuntimeConfig::default(),
    ));
    let owner = SessionOwner::Ui {
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
    };
    let info = manager
        .create(CreateSessionRequest {
            target: SessionTarget::Ssh(SshSessionTarget {
                host_id: HostId::new(),
                working_directory: None,
                command: None,
            }),
            owner: owner.clone(),
            size: TerminalSize::new(80, 24, 0, 0).expect("terminal size"),
            initial_palette: None,
        })
        .await
        .expect("create SSH fixture");
    let context = manager
        .terminal_command_context(info.session_id.clone())
        .await
        .expect("session generation");
    (
        manager,
        UiSessionBinding {
            session_id: info.session_id,
            owner,
            session_generation: context.session_generation,
        },
    )
}

/// 仅接受安全的 WSL 绝对目录以插入 fixture shell 脚本，默认指向本任务解包的工具。
fn lrzsz_dir() -> String {
    let directory = std::env::var("KERMINAL_TEST_LRZSZ_WSL_DIR")
        .unwrap_or_else(|_| "/tmp/kerminal-zmodem-acceptance/root/usr/bin".to_owned());
    assert!(
        directory.starts_with('/')
            && directory
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric()
                    || matches!(byte, b'/' | b'_' | b'-' | b'.'))
    );
    directory
}

/// 等待终端快照出现明确标记，避免以时间推测远端已返回 shell。
async fn wait_text(manager: &SessionManager, binding: &UiSessionBinding, expected: &str) {
    let mut last = String::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let text = manager
                .plaintext_snapshot(
                    binding.session_id.clone(),
                    PlainTextSnapshotRequest::default(),
                )
                .await
                .expect("snapshot")
                .text;
            if text.contains(expected) {
                return;
            }
            last = text;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("terminal text timeout for {expected}, last={last:?}"));
}

/// 只读取应用层可见的状态，检查失败阶段而不靠等待时间猜测完成。
async fn wait_status(
    manager: &SessionManager,
    binding: &UiSessionBinding,
    predicate: impl Fn(ZmodemPhase) -> bool,
) -> kerminal_application::sessions::ZmodemStatus {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(status) = manager
                .zmodem_status(binding.clone())
                .await
                .expect("read status")
            {
                if predicate(status.phase)
                    || matches!(
                        status.phase,
                        ZmodemPhase::Failed | ZmodemPhase::NeedsReconnect
                    )
                {
                    return status;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("lrzsz status timeout")
}

/// `sz` 发送包含 OSC 133 的二进制内容，确保真实接收和文件字节完整。
#[tokio::test]
#[ignore = "requires task-private WSL lrzsz binaries at /tmp/kerminal-zmodem-acceptance/root/usr/bin"]
async fn real_lrzsz_upload_and_download_through_session_manager() {
    let root = tempfile::tempdir().expect("isolated test files");
    let remote = root.path().join("remote");
    let local = root.path().join("local");
    std::fs::create_dir(&remote).expect("remote fixture");
    std::fs::create_dir(&local).expect("local fixture");
    let payload = b"binary \0 \x1b]133;A\x07 content \xff \x18 end";
    let local_file = local.join("upload.bin");
    std::fs::write(&local_file, payload).expect("upload input");

    let (upload, binding) = session(
        format!(
            "{}/rz -b; printf '\\033]133;A\\007'; read done",
            lrzsz_dir()
        ),
        &remote,
    )
    .await;
    let start = wait_status(&upload, &binding, |phase| {
        phase == ZmodemPhase::WaitingForSelection
    })
    .await;
    assert_eq!(start.direction, ZmodemDirection::Upload);
    upload
        .respond_zmodem(
            binding.clone(),
            start.id,
            ZmodemDecision::Upload(vec![local_file]),
        )
        .await
        .expect("select upload");
    let end = wait_status(&upload, &binding, |phase| phase == ZmodemPhase::Finished).await;
    assert_eq!(end.phase, ZmodemPhase::Finished, "upload failed: {end:?}");
    assert_eq!(
        std::fs::read(remote.join("upload.bin")).expect("remote file"),
        payload
    );

    std::fs::write(remote.join("payload.bin"), payload).expect("download input");
    let (download, binding) = session(
        format!(
            "{}/sz -b payload.bin; printf '\\033]133;A\\007'; read done",
            lrzsz_dir()
        ),
        &remote,
    )
    .await;
    let start = wait_status(&download, &binding, |phase| {
        phase == ZmodemPhase::WaitingForSelection
    })
    .await;
    assert_eq!(start.direction, ZmodemDirection::Download);
    download
        .respond_zmodem(
            binding.clone(),
            start.id,
            ZmodemDecision::Download(local.clone()),
        )
        .await
        .expect("select download");
    let end = wait_status(&download, &binding, |phase| phase == ZmodemPhase::Finished).await;
    assert_eq!(end.phase, ZmodemPhase::Finished, "download failed: {end:?}");
    assert_eq!(
        std::fs::read(local.join("payload.bin")).expect("downloaded file"),
        payload
    );
}

/// 选择前取消或远端先退出都应保留 D/A/B；管道不能替代真实 PTY 的输入恢复验证。
#[tokio::test]
#[ignore = "requires task-private WSL lrzsz binaries"]
async fn lrzsz_cancellation_and_peer_exit_restore_prompt() {
    let root = tempfile::tempdir().expect("isolated test files");
    for peer_aborts in [false, true] {
        let command = format!(
            "{} {}/rz -b; printf '\\033]133;D;128\\007\\033]133;A\\007RESTORED\\033]133;B\\007'; sleep 10",
            if peer_aborts { "timeout -s INT 2" } else { "" },
            lrzsz_dir()
        );
        let (manager, binding) = session(command, root.path()).await;
        let start = wait_status(&manager, &binding, |phase| {
            phase == ZmodemPhase::WaitingForSelection
        })
        .await;
        assert_eq!(start.phase, ZmodemPhase::WaitingForSelection);
        if !peer_aborts {
            manager
                .respond_zmodem(binding.clone(), start.id, ZmodemDecision::Cancel)
                .await
                .expect("cancel selected transfer");
        }
        let end = wait_status(&manager, &binding, |phase| phase == ZmodemPhase::Cancelled).await;
        assert_eq!(end.phase, ZmodemPhase::Cancelled, "cancel failed: {end:?}");
        wait_text(&manager, &binding, "RESTORED").await;
        let context = manager
            .terminal_command_context(binding.session_id.clone())
            .await
            .expect("restored context");
        assert!(
            context.prompt_ready,
            "D/A/B prompt was not restored: {context:?}"
        );
    }
}

/// 真实 rz -p 对已存在文件发出 ZSKIP，后续文件仍上传且结果不能误报全成功。
#[tokio::test]
#[ignore = "requires task-private WSL lrzsz binaries"]
async fn lrzsz_skip_preserves_existing_file_and_reports_failure() {
    let root = tempfile::tempdir().expect("isolated test files");
    let remote = root.path().join("remote");
    let local = root.path().join("local");
    std::fs::create_dir(&remote).expect("remote fixture");
    std::fs::create_dir(&local).expect("local fixture");
    std::fs::write(remote.join("exists.bin"), b"original").expect("existing target");
    std::fs::write(local.join("exists.bin"), b"replacement").expect("skipped source");
    std::fs::write(local.join("next.bin"), b"next content").expect("next source");
    let (manager, binding) = session(
        format!(
            "{}/rz -b -p; printf '\\033]133;A\\007'; read done",
            lrzsz_dir()
        ),
        &remote,
    )
    .await;
    let start = wait_status(&manager, &binding, |phase| {
        phase == ZmodemPhase::WaitingForSelection
    })
    .await;
    manager
        .respond_zmodem(
            binding.clone(),
            start.id,
            ZmodemDecision::Upload(vec![local.join("exists.bin"), local.join("next.bin")]),
        )
        .await
        .expect("select files");
    let end = wait_status(&manager, &binding, |phase| {
        matches!(phase, ZmodemPhase::Finished | ZmodemPhase::Failed)
    })
    .await;
    assert_eq!(
        end.phase,
        ZmodemPhase::Failed,
        "skipped transfer must not succeed: {end:?}"
    );
    assert_eq!(
        end.completed, 1,
        "only the second file was transferred: {end:?}"
    );
    assert_eq!(
        std::fs::read(remote.join("exists.bin")).expect("existing target"),
        b"original"
    );
    assert_eq!(
        std::fs::read(remote.join("next.bin")).expect("next target"),
        b"next content"
    );
}

/// 远端在文件传输过程中退出时必须终止本地写入，并清理未完成的临时文件。
#[tokio::test]
#[ignore = "requires task-private WSL lrzsz binaries"]
async fn lrzsz_peer_interrupt_during_download_cleans_partial_file() {
    let root = tempfile::tempdir().expect("isolated test files");
    let remote = root.path().join("remote");
    let local = root.path().join("local");
    std::fs::create_dir(&remote).expect("remote fixture");
    std::fs::create_dir(&local).expect("local fixture");
    let file = std::fs::File::create(remote.join("large.bin")).expect("remote file");
    file.set_len(512 * 1024 * 1024)
        .expect("large sparse fixture");
    let (manager, binding) = session(
        format!(
            "timeout -s INT 2 {}/sz -b large.bin; printf '\\033]133;D;130\\007\\033]133;A\\007RESTORED\\033]133;B\\007'; sleep 10",
            lrzsz_dir()
        ),
        &remote,
    )
    .await;
    let start = wait_status(&manager, &binding, |phase| {
        phase == ZmodemPhase::WaitingForSelection
    })
    .await;
    manager
        .respond_zmodem(
            binding.clone(),
            start.id,
            ZmodemDecision::Download(local.clone()),
        )
        .await
        .expect("select download directory");
    let active = wait_status(&manager, &binding, |phase| {
        phase == ZmodemPhase::Transferring
    })
    .await;
    assert_eq!(
        active.phase,
        ZmodemPhase::Transferring,
        "transfer never started: {active:?}"
    );
    let end = wait_status(&manager, &binding, |phase| {
        matches!(
            phase,
            ZmodemPhase::Cancelled | ZmodemPhase::Failed | ZmodemPhase::NeedsReconnect
        )
    })
    .await;
    assert_ne!(
        end.phase,
        ZmodemPhase::Finished,
        "interrupted transfer succeeded"
    );
    wait_text(&manager, &binding, "RESTORED").await;
    assert_eq!(
        std::fs::read_dir(&local).expect("local directory").count(),
        0
    );
}
