//! Windows 的真实 ConPTY 与 PSReadLine 回调；仅使用临时目录和禁用 profile 的子进程。
//!
//! @author kongweiguang
#![cfg(windows)]

use kerminal_application::sessions::{SessionTransport, SessionTransportFactory};
use kerminal_domain::{
    identity::SessionId,
    sessions::{LocalSessionTarget, SessionTarget, TerminalSize},
};
use kerminal_infrastructure::local_pty::LocalPtySessionTransportFactory;
use kerminal_terminal::{
    ShellIntegrationMarker, TerminalEngine, TerminalLimits,
    shell_editing::{EditingMessage, ShellEditingFrameFilter, parse_editing_message},
};
use std::{collections::BTreeMap, ffi::OsString, pin::Pin, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{Mutex, mpsc},
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;

type Writer = Arc<Mutex<Pin<Box<dyn AsyncWrite + Send>>>>;
enum Observation {
    Ready,
    Editing(EditingMessage),
    Output(Vec<u8>),
}

/// 同一个 writer 同时承载测试输入与终端查询回复，保持真实 ConPTY 的串行字节边界。
async fn send(writer: &Writer, bytes: &[u8]) -> Result<(), String> {
    let mut writer = writer.lock().await;
    writer
        .write_all(bytes)
        .await
        .map_err(|_| "PTY write failed".to_owned())?;
    writer
        .flush()
        .await
        .map_err(|_| "PTY flush failed".to_owned())
}

/// 有界等待真实快照；重绘提示符只在失败时汇总，避免诊断输出包含编辑正文。
///
/// @author kongweiguang
async fn snapshot(
    writer: &Writer,
    receiver: &mut mpsc::UnboundedReceiver<Observation>,
    stage: &str,
) -> Result<(String, usize, Option<String>), String> {
    send(writer, b"\x1b[24;8~").await?;
    let mut output_bytes = 0usize;
    let mut prompt_events = 0usize;
    timeout(Duration::from_secs(8), async {
        while let Some(value) = receiver.recv().await {
            match value {
                Observation::Output(bytes) => output_bytes += bytes.len(),
                Observation::Ready => prompt_events += 1,
                Observation::Editing(EditingMessage::Snapshot {
                    input, cursor, cwd, ..
                }) => return Ok((input, cursor, cwd)),
                Observation::Editing(EditingMessage::InputTooLong { .. }) => {
                    return Err(format!("{stage}: snapshot refused as oversized (prompt events: {prompt_events})"));
                }
                _ => {}
            }
        }
        Err(format!("{stage}: PTY closed before snapshot (prompt events: {prompt_events})"))
    })
    .await
    .map_err(|_| format!("{stage}: snapshot timed out (output bytes: {output_bytes}, prompt events: {prompt_events})"))?
}

/// 超限只消费固定拒绝帧并核对握手身份，不能把长正文当作快照输出。
///
/// @author kongweiguang
async fn expect_oversized_refusal(
    writer: &Writer,
    receiver: &mut mpsc::UnboundedReceiver<Observation>,
    nonce: &str,
    version: &str,
    stage: &str,
) -> Result<(), String> {
    send(writer, b"\x1b[24;8~").await?;
    let mut output_bytes = 0usize;
    let mut prompt_events = 0usize;
    timeout(Duration::from_secs(8), async {
        while let Some(value) = receiver.recv().await {
            match value {
                Observation::Editing(EditingMessage::InputTooLong {
                    nonce: observed_nonce,
                    shell: kerminal_terminal::shell_editing::EditingShell::PowerShell,
                    version: observed_version,
                }) if observed_nonce == nonce && observed_version == version => return Ok(()),
                Observation::Editing(EditingMessage::Snapshot { .. }) => {
                    return Err(format!(
                        "{stage}: oversized input was emitted as a snapshot (prompt events: {prompt_events})"
                    ));
                }
                Observation::Ready => prompt_events += 1,
                Observation::Output(bytes) => output_bytes += bytes.len(),
                _ => {}
            }
        }
        Err(format!("{stage}: PTY closed before oversized refusal (prompt events: {prompt_events})"))
    })
    .await
    .map_err(|_| format!("{stage}: oversized refusal timed out (output bytes: {output_bytes}, prompt events: {prompt_events})"))?
}

/// 重绘可能重复发送 prompt 标记；只有随后只读快照确认空缓冲，才可开始下一阶段。
///
/// @author kongweiguang
async fn wait_prompt(
    writer: &Writer,
    receiver: &mut mpsc::UnboundedReceiver<Observation>,
    stage: &str,
) -> Result<(), String> {
    let (input, cursor, _) = snapshot(writer, receiver, stage).await?;
    if !input.is_empty() || cursor != 0 {
        return Err(format!(
            "{stage}: expected an empty editing buffer (characters: {}, cursor: {cursor})",
            input.chars().count()
        ));
    }
    Ok(())
}

/// 伪造的重绘标记先于真实快照到达时，非空缓冲仍必须阻止下一阶段。
///
/// @author kongweiguang
#[tokio::test]
async fn repaint_marker_does_not_certify_nonempty_edit_buffer() {
    for input in ["still editing", ""] {
        let (stream, mut peer) = tokio::io::duplex(128);
        let stream: Pin<Box<dyn AsyncWrite + Send>> = Box::pin(stream);
        let writer = Arc::new(Mutex::new(stream));
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let fixture = tokio::spawn(async move {
            let mut probe = [0; 7];
            peer.read_exact(&mut probe).await.expect("probe");
            assert_eq!(&probe, b"\x1b[24;8~");
            assert!(sender.send(Observation::Ready).is_ok());
            assert!(
                sender
                    .send(Observation::Editing(EditingMessage::Snapshot {
                        nonce: "0123456789abcdef0123456789abcdef".to_owned(),
                        shell: kerminal_terminal::shell_editing::EditingShell::PowerShell,
                        version: "test".to_owned(),
                        cwd: None,
                        input: input.to_owned(),
                        cursor: input.chars().count(),
                        prediction_visible: None,
                    }))
                    .is_ok()
            );
        });
        let result = wait_prompt(&writer, &mut receiver, "fixture").await;
        fixture.await.expect("fixture task");
        if input.is_empty() {
            assert!(result.is_ok());
        } else {
            let error = result.expect_err("redraw must not prove an empty buffer");
            assert!(error.contains("expected an empty editing buffer"));
            assert!(!error.contains(input));
        }
    }
}

/// 私有帧先分离，普通字节进入生产终端引擎以回答 DSR，避免模拟固定光标造成 PSReadLine 失真。
fn start_reader(
    transport: SessionTransport,
    size: TerminalSize,
) -> (
    Writer,
    mpsc::UnboundedReceiver<Observation>,
    tokio::task::JoinHandle<()>,
    Arc<dyn kerminal_application::sessions::SessionTransportControl>,
) {
    let mut reader = transport.reader;
    let control = transport.control;
    let writer = Arc::new(Mutex::new(transport.writer));
    let response_writer = writer.clone();
    let (sender, receiver) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let mut engine =
            TerminalEngine::new(SessionId::new(), size, TerminalLimits::default()).unwrap();
        let mut filter = ShellEditingFrameFilter::default();
        let mut buffer = vec![0; 16384];
        while let Ok(count) = reader.read(&mut buffer).await {
            if count == 0 {
                break;
            }
            let filtered = filter.advance(&buffer[..count]);
            for frame in filtered.frames {
                if let Some(message) = parse_editing_message(&frame.payload) {
                    let _ = sender.send(Observation::Editing(message));
                }
            }
            let output = engine.process_output(&filtered.visible);
            let _ = sender.send(Observation::Output(filtered.visible));
            for bytes in output.pty_writes {
                if send(&response_writer, &bytes).await.is_err() {
                    return;
                }
            }
            for marker in output.shell_integration {
                if marker == ShellIntegrationMarker::PromptEnd {
                    let _ = sender.send(Observation::Ready);
                }
            }
        }
    });
    (writer, receiver, task, control)
}

/// 两个 PowerShell 版本都走真实编辑器；失败也先关闭自己持有的进程树再报告断言。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn powershell_versions_read_real_unicode_buffer_without_executing() {
    for program in ["pwsh.exe", "powershell.exe"] {
        let root = tempfile::tempdir().unwrap();
        let size = TerminalSize::new(120, 30, 0, 0).unwrap();
        let mut environment = BTreeMap::new();
        // PSReadLine 仍会在 NoProfile 下记录已执行命令；子进程 KnownFolder 必须指向临时用户根。
        for (name, path) in [
            ("USERPROFILE", root.path().to_path_buf()),
            ("APPDATA", root.path().join("AppData/Roaming")),
            ("LOCALAPPDATA", root.path().join("AppData/Local")),
        ] {
            std::fs::create_dir_all(&path).unwrap();
            environment.insert(OsString::from(name), path.into_os_string());
        }
        if program == "powershell.exe" {
            let windows = std::env::var_os("SystemRoot").unwrap();
            environment.insert(
                OsString::from("PSModulePath"),
                std::env::join_paths([
                    std::path::PathBuf::from(std::env::var_os("ProgramFiles").unwrap())
                        .join("WindowsPowerShell/Modules"),
                    std::path::PathBuf::from(windows)
                        .join("System32/WindowsPowerShell/v1.0/Modules"),
                ])
                .unwrap(),
            );
        }
        let target = SessionTarget::Local(LocalSessionTarget {
            program: program.into(),
            args: vec!["-NoLogo".into(), "-NoProfile".into()],
            working_directory: Some(root.path().to_path_buf()),
            environment,
        });
        let transport = LocalPtySessionTransportFactory::new()
            .open(&target, size, CancellationToken::new())
            .await
            .expect("open isolated PowerShell");
        let nonce = transport
            .shell_editing_nonce
            .clone()
            .expect("bootstrap nonce");
        let (writer, mut receiver, task, control) = start_reader(transport, size);
        let result: Result<(), String> = async {
            let mut startup_output = Vec::new();
            let mut negotiated_version = String::new();
            timeout(Duration::from_secs(15), async {
                let mut hello = false;
                while let Some(value) = receiver.recv().await {
                    match value {
                        Observation::Editing(EditingMessage::Hello {
                            nonce: observed,
                            installed,
                            version,
                            ..
                        }) => {
                            if !installed || observed != nonce {
                                return Err("capability negotiation failed".to_owned());
                            }
                            negotiated_version = version;
                            hello = true;
                        }
                        Observation::Ready if hello => return Ok(()),
                        Observation::Output(bytes) if startup_output.len() < 16384 => {
                            startup_output.extend(bytes);
                        }
                        _ => {}
                    }
                }
                Err("PTY closed before prompt".to_owned())
            })
            .await
            .map_err(|_| {
                format!(
                    "prompt timed out: {}",
                    String::from_utf8_lossy(&startup_output)
                )
            })??;
            let before = snapshot(&writer, &mut receiver, "initial snapshot").await?;
            if !before.0.is_empty() {
                return Err("initial line was not empty".to_owned());
            }
            let input = "Write-Output '中文😀e\u{301}'";
            send(&writer, input.as_bytes()).await?;
            let read = snapshot(&writer, &mut receiver, "unicode fixture").await?;
            if read.0 != input || read.1 != input.chars().count() {
                return Err("Unicode buffer or cursor differed".to_owned());
            }
            send(&writer, b" suffix").await?;
            let after = snapshot(&writer, &mut receiver, "unicode suffix").await?;
            if after.0 != format!("{input} suffix") || after.2 != before.2 {
                return Err("read callback changed or executed input".to_owned());
            }
            send(&writer, b"\x03").await?;
            wait_prompt(&writer, &mut receiver, "cancel unicode fixture").await?;
            send(&writer, b"Write-Output KERMINAL_HISTORY\r").await?;
            wait_prompt(&writer, &mut receiver, "execute history fixture").await?;
            send(&writer, b"\x1b[A").await?;
            if snapshot(&writer, &mut receiver, "history fixture").await?.0
                != "Write-Output KERMINAL_HISTORY"
            {
                return Err("native history buffer differed".to_owned());
            }
            send(&writer, b"\x03").await?;
            wait_prompt(&writer, &mut receiver, "cancel history fixture").await?;
            std::fs::write(root.path().join("kerminal_completion_target"), b"").unwrap();
            send(&writer, b"Get-Item ./kerminal_completion_\t").await?;
            if !snapshot(&writer, &mut receiver, "tab fixture")
                .await?
                .0
                .contains("kerminal_completion_target")
            {
                return Err("native Tab buffer differed".to_owned());
            }
            send(&writer, b"\x03").await?;
            wait_prompt(&writer, &mut receiver, "cancel tab fixture").await?;
            let folder = root.path().join("cwd 中文 '()");
            std::fs::create_dir(&folder).unwrap();
            let command = format!(
                "Set-Location -LiteralPath '{}'\r",
                folder.to_string_lossy().replace('\'', "''")
            );
            send(&writer, command.as_bytes()).await?;
            wait_prompt(&writer, &mut receiver, "change directory").await?;
            let current = snapshot(&writer, &mut receiver, "changed directory").await?;
            if current.2.as_deref() != folder.to_str() {
                return Err("live cwd with punctuation differed".to_owned());
            }
            for (label, input, too_long) in [
                ("ascii-4000", "x".repeat(4000), false),
                ("unicode-4000", "😀".repeat(4000), false),
                ("ascii-4001", "x".repeat(4001), true),
                ("unicode-4001", "😀".repeat(4001), true),
                ("ascii-8001", "x".repeat(8001), true),
            ] {
                send(&writer, input.as_bytes()).await?;
                if too_long {
                    expect_oversized_refusal(
                        &writer,
                        &mut receiver,
                        &nonce,
                        &negotiated_version,
                        label,
                    )
                    .await?;
                } else {
                    let actual = snapshot(&writer, &mut receiver, label).await?;
                    if actual.0 != input || actual.1 != 4000 {
                        return Err(format!("{label} boundary snapshot differed"));
                    }
                }
                send(&writer, b"\x03").await?;
                wait_prompt(&writer, &mut receiver, &format!("cancel {label}")).await?;
                if too_long {
                    send(&writer, b"git sta").await?;
                    if snapshot(&writer, &mut receiver, &format!("{label} recovery"))
                        .await?
                        .0
                        != "git sta"
                    {
                        return Err(format!("{label} shorter input did not recover"));
                    }
                    send(&writer, b"\x03").await?;
                    wait_prompt(&writer, &mut receiver, &format!("cancel {label} recovery"))
                        .await?;
                }
            }
            let history = std::fs::read_to_string(root.path().join(
                "AppData/Roaming/Microsoft/Windows/PowerShell/PSReadLine/ConsoleHost_history.txt",
            ))
            .map_err(|_| "PSReadLine history did not use the isolated user root".to_owned())?;
            if !history
                .lines()
                .any(|line| line == "Write-Output KERMINAL_HISTORY")
            {
                return Err("isolated history did not contain the executed fixture".to_owned());
            }
            Ok(())
        }
        .await;
        let closed = control
            .close_until(Instant::now() + Duration::from_secs(8))
            .await;
        task.abort();
        let _ = task.await;
        assert!(closed.is_ok(), "{program}: isolated PTY cleanup failed");
        assert!(result.is_ok(), "{program}: {}", result.unwrap_err());
    }
}
