//! 通过真实平台 PTY 验证 restart 保持身份、结束旧进程并在新代次恢复输出。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString, process::Command, sync::Arc, time::Duration};

#[cfg(windows)]
use std::os::windows::{io::AsRawHandle, process::CommandExt};

use kerminal_application::sessions::{
    CreateSessionRequest, PlainTextSnapshotRequest, SessionService,
};
use kerminal_domain::identity::{PaneId, TabId};
use kerminal_domain::sessions::{
    LocalSessionTarget, SessionLifecycle, SessionOwner, SessionTarget, TerminalSize,
};
use kerminal_infrastructure::local_pty::LocalPtySessionTransportFactory;
use kerminal_infrastructure::sessions::{SessionManager, SessionRuntimeConfig};
use tokio::{runtime::Handle, time::timeout};
use tokio_util::sync::CancellationToken;

const READY_PREFIX: &str = "KERMINAL_RESTART_PROCESS=";

/// 为测试生成不读取用户设置的本地 shell 命令；命令输出标记后保持进程运行供重启终止。
///
/// @author kongweiguang
fn restart_target() -> Option<LocalSessionTarget> {
    #[cfg(windows)]
    {
        let program = windows_shell_program()?;
        let script = format!(
            "[Console]::WriteLine('{}' + $PID); Start-Sleep -Seconds 300",
            READY_PREFIX
        );
        Some(LocalSessionTarget {
            program,
            args: vec![
                OsString::from("-NoLogo"),
                OsString::from("-NoProfile"),
                OsString::from("-Command"),
                OsString::from(script),
            ],
            working_directory: None,
            environment: BTreeMap::new(),
        })
    }
    #[cfg(unix)]
    {
        let script = format!("printf '{}%s\\n' \"$$\"; exec sleep 300", READY_PREFIX);
        Some(LocalSessionTarget {
            program: OsString::from("/bin/sh"),
            args: vec![OsString::from("-c"), OsString::from(script)],
            working_directory: None,
            environment: BTreeMap::new(),
        })
    }
}

/// 在 Windows 上探测隔离测试可用的系统 PowerShell，避免用户 profile 与 AutoRun 影响进程。
///
/// @author kongweiguang
#[cfg(windows)]
fn windows_shell_program() -> Option<OsString> {
    ["pwsh.exe", "powershell.exe"]
        .into_iter()
        .find_map(|program| {
            Command::new(program)
                .creation_flags(0x0800_0000)
                .args(["-NoLogo", "-NoProfile", "-Command", "exit 0"])
                .status()
                .ok()
                .filter(std::process::ExitStatus::success)
                .map(|_| OsString::from(program))
        })
}

/// 为每个测试创建独立 manager，确保只验证真实 local-PTY transport 的重建。
///
/// @author kongweiguang
fn manager() -> SessionManager {
    SessionManager::new(
        Arc::new(LocalPtySessionTransportFactory::new()),
        Handle::current(),
        SessionRuntimeConfig::default(),
    )
}

/// 轮询真实 VT 快照直到出现当前 PID 对应的 shell 标记，避免把旧代次残留当成新输出。
///
/// @author kongweiguang
async fn wait_for_marker(
    service: &dyn SessionService,
    session_id: kerminal_domain::identity::SessionId,
    expected_pid: u32,
) {
    let expected = format!("{READY_PREFIX}{expected_pid}");
    timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = service
                .plaintext_snapshot(
                    session_id.clone(),
                    PlainTextSnapshotRequest {
                        include_scrollback: true,
                        max_lines: 64,
                        max_bytes: Some(8 * 1024),
                    },
                )
                .await
                .expect("local PTY snapshot");
            if snapshot.text.lines().any(|line| line.trim() == expected) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("shell startup marker reached the VT engine");
}

/// 通过操作系统进程表确认 Unix shell 的 PID 已退出，僵尸进程也视为已结束执行。
///
/// @author kongweiguang
#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    let pid = pid.to_string();
    let Ok(output) = Command::new("ps")
        .args(["-o", "stat=", "-p", pid.as_str()])
        .output()
    else {
        return false;
    };
    output.status.success()
        && !String::from_utf8_lossy(&output.stdout)
            .trim_start()
            .starts_with('Z')
}

/// 在有界时间内等待 Unix shell 退出，避免成功重启后仍遗留第一代本地进程。
///
/// @author kongweiguang
#[cfg(unix)]
async fn wait_for_old_process_exit(pid: u32) {
    timeout(Duration::from_secs(5), async {
        while process_is_running(pid) {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("old local shell process exited after restart");
}

/// 固定 Windows 进程对象而非 PID 字符串，避免退出后 PID 重用造成错误判定。
///
/// @author kongweiguang
#[cfg(windows)]
fn open_process_handle(pid: u32) -> std::io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    // PTY child 属于当前测试用户，只请求等待与读取退出码所需的进程权限。
    let raw = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if raw.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: OpenProcess 返回新拥有的句柄，由 OwnedHandle 恰好关闭一次。
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// 读取 Windows 内核进程对象的实际退出码；STILL_ACTIVE 表示旧 PTY shell 尚未结束。
///
/// @author kongweiguang
#[cfg(windows)]
fn process_exit_code(handle: &std::os::windows::io::OwnedHandle) -> std::io::Result<Option<u32>> {
    use windows_sys::Win32::System::Threading::GetExitCodeProcess;

    let mut code = 0;
    let succeeded = unsafe { GetExitCodeProcess(handle.as_raw_handle() as _, &mut code) };
    if succeeded == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((code != 259).then_some(code))
}

/// 有界等待 Windows 固定进程对象变为 signaled，验证 ConPTY restart 已结束旧 PowerShell。
///
/// @author kongweiguang
#[cfg(windows)]
async fn wait_for_old_process_exit(handle: &std::os::windows::io::OwnedHandle) {
    timeout(Duration::from_secs(5), async {
        loop {
            if process_exit_code(handle)
                .expect("read old shell process exit code")
                .is_some()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("old local shell process exited after restart");
}

/// 使用真实 local-PTY 重启同一会话，核对旧进程退出、新代次输出以及最终 close 清理。
///
/// @author kongweiguang
#[tokio::test]
async fn local_pty_restart_reuses_identity_and_replaces_process() {
    let Some(target) = restart_target() else {
        // Windows 无系统 PowerShell 时无法创建本地 ConPTY shell；其余平台 shell 路径固定。
        return;
    };
    let manager = manager();
    let created = manager
        .create(CreateSessionRequest {
            target: SessionTarget::Local(target),
            owner: SessionOwner::Ui {
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
            },
            size: TerminalSize::new(100, 30, 0, 0).expect("valid test grid"),
            initial_palette: None,
        })
        .await
        .expect("spawn first local PTY generation");
    let before = manager
        .runtime_metadata(created.session_id.clone())
        .await
        .expect("first transport metadata");
    let old_pid = before
        .process_id
        .expect("local transport exposes child PID");
    wait_for_marker(&manager, created.session_id.clone(), old_pid).await;
    #[cfg(windows)]
    let old_process = open_process_handle(old_pid).expect("open original PowerShell handle");
    #[cfg(unix)]
    assert!(
        process_is_running(old_pid),
        "first local shell is still running"
    );
    let context_before = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("first command context");

    let restarted = timeout(
        Duration::from_secs(15),
        manager.restart_cancellable(created.session_id.clone(), CancellationToken::new()),
    )
    .await
    .expect("local PTY restart is bounded")
    .expect("local PTY restart succeeds");
    let context_after = manager
        .terminal_command_context(created.session_id.clone())
        .await
        .expect("new command context");
    let after = manager
        .runtime_metadata(created.session_id.clone())
        .await
        .expect("replacement transport metadata");
    let new_pid = after
        .process_id
        .expect("replacement local transport exposes child PID");
    wait_for_marker(&manager, created.session_id.clone(), new_pid).await;
    #[cfg(unix)]
    wait_for_old_process_exit(old_pid).await;
    #[cfg(windows)]
    wait_for_old_process_exit(&old_process).await;

    assert_eq!(restarted.session_id, created.session_id);
    assert_eq!(restarted.owner, created.owner);
    assert_eq!(restarted.target, created.target);
    assert_eq!(restarted.lifecycle, SessionLifecycle::Running);
    assert_eq!(
        context_after.session_generation,
        context_before.session_generation.wrapping_add(1)
    );

    manager
        .close(created.session_id.clone())
        .await
        .expect("close replacement PTY generation");
    assert!(
        manager
            .list(Default::default())
            .await
            .expect("list after final close")
            .is_empty()
    );
}
