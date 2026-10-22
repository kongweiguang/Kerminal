//! 创建真实 PTY 会话并测量 shell 响应与原始输出。
//!
//! @author kongweiguang

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kerminal_application::sessions::{
    CreateSessionRequest, PlainTextSnapshotRequest, SessionService,
};
use kerminal_domain::identity::SessionId;
use kerminal_domain::sessions::{
    LocalSessionTarget, SessionInfo, SessionOwner, SessionTarget, TerminalSize,
};
use tokio::time::{Instant as TokioInstant, sleep};
use uuid::Uuid;

use super::support::arguments::Options;
use super::support::measurements::RawOutputCounter;

const SNAPSHOT_LIMIT_BYTES: usize = 1024 * 1024;
const SNAPSHOT_LIMIT_LINES: usize = 2000;
const OUTPUT_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// 对真实 PTY 发送由 shell 拼接生成的唯一标记，避免输入行回显伪造响应。
pub(super) async fn run_echo_probe(
    service: &Arc<dyn SessionService>,
    output_counter: &RawOutputCounter,
    session_id: &SessionId,
) -> Result<(f64, u64), String> {
    let nonce = Uuid::new_v4().simple().to_string();
    let marker = format!("KERM_A{nonce}_B");
    let command = echo_command(&nonce);
    let baseline = output_counter.bytes(session_id);
    let start = Instant::now();
    service
        .write(session_id.clone(), format!("{command}\r").into_bytes())
        .await
        .map_err(|error| error.to_string())?;
    wait_for_text(service, session_id, &marker, OUTPUT_PROBE_TIMEOUT).await?;
    Ok((
        start.elapsed().as_secs_f64() * 1000.0,
        output_counter.bytes(session_id).saturating_sub(baseline),
    ))
}

/// 发送多行 shell 输出，以服务 reader 实收字节数除以 marker 完成时间计算吞吐。
pub(super) async fn run_throughput_probe(
    service: &Arc<dyn SessionService>,
    output_counter: &RawOutputCounter,
    session_id: &SessionId,
    lines: usize,
) -> Result<(f64, u64), String> {
    let nonce = Uuid::new_v4().simple().to_string();
    let end_marker = format!("KERM_END{nonce}_END");
    let command = throughput_command(&nonce, lines);
    let baseline = output_counter.bytes(session_id);
    let start = Instant::now();
    service
        .write(session_id.clone(), format!("{command}\r").into_bytes())
        .await
        .map_err(|error| error.to_string())?;
    wait_for_text(service, session_id, &end_marker, OUTPUT_PROBE_TIMEOUT).await?;
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    Ok((
        elapsed,
        output_counter.bytes(session_id).saturating_sub(baseline),
    ))
}

/// 按实际平台生成简短 shell 命令，输出完整标记由两段字符串拼接而成。
fn echo_command(nonce: &str) -> String {
    #[cfg(windows)]
    {
        format!("[Console]::WriteLine('KERM_A' + '{nonce}' + '_B')")
    }
    #[cfg(unix)]
    {
        format!("printf 'KERM_A%s_B\\n' '{nonce}'")
    }
}

/// 生成有限行输出负载，且结束标记只在 shell 执行后拼成连续文本。
fn throughput_command(nonce: &str, lines: usize) -> String {
    #[cfg(windows)]
    {
        format!(
            "1..{lines} | ForEach-Object {{ [Console]::WriteLine('KERM_A' + '{nonce}' + '_B') }}; [Console]::WriteLine('KERM_END' + '{nonce}' + '_END')"
        )
    }
    #[cfg(unix)]
    {
        format!(
            "i=0; while [ $i -lt {lines} ]; do printf 'KERM_A%s_B\\n' '{nonce}'; i=$((i+1)); done; printf 'KERM_END%s_END\\n' '{nonce}'"
        )
    }
}

/// 在有界快照中轮询 shell 输出，超时保留失败而不以 write 成功推断完成。
async fn wait_for_text(
    service: &Arc<dyn SessionService>,
    session_id: &SessionId,
    marker: &str,
    timeout: Duration,
) -> Result<String, String> {
    let deadline = TokioInstant::now() + timeout;
    loop {
        let snapshot = service
            .plaintext_snapshot(
                session_id.clone(),
                PlainTextSnapshotRequest {
                    include_scrollback: true,
                    max_lines: SNAPSHOT_LIMIT_LINES,
                    max_bytes: Some(SNAPSHOT_LIMIT_BYTES),
                },
            )
            .await
            .map_err(|error| format!("读取 session snapshot 失败：{error}"))?;
        if snapshot.text.contains(marker) {
            return Ok(snapshot.text);
        }
        if TokioInstant::now() >= deadline {
            return Err(format!("PTY 输出未在 {timeout:?} 内出现唯一完成标记"));
        }
        sleep(Duration::from_millis(10)).await;
    }
}

/// 使用本次运行的 HOME、临时目录与配置根创建实际 portable-pty session。
pub(super) async fn create_session(
    service: &Arc<dyn SessionService>,
    options: &Options,
    temp_root: &Path,
    config_root: &Path,
    home_root: &Path,
    temp_child_root: &Path,
) -> Result<SessionInfo, String> {
    let target = local_target(options, temp_root, config_root, home_root, temp_child_root);
    service
        .create(CreateSessionRequest {
            target: SessionTarget::Local(target),
            owner: SessionOwner::Service {
                name: "session-acceptance".to_owned(),
            },
            size: TerminalSize::default(),
        })
        .await
        .map_err(|error| format!("创建真实 local PTY session 失败：{error}"))
}

/// 构造平台 shell target；隔离 home 与配置路径，并显式关闭 PowerShell profile。
fn local_target(
    options: &Options,
    temp_root: &Path,
    config_root: &Path,
    home_root: &Path,
    temp_child_root: &Path,
) -> LocalSessionTarget {
    #[cfg(windows)]
    let (default_program, default_args) = (
        OsString::from("powershell.exe"),
        vec![OsString::from("-NoLogo"), OsString::from("-NoProfile")],
    );
    #[cfg(unix)]
    let (default_program, default_args) = (OsString::from("/bin/sh"), vec![OsString::from("-i")]);
    let program = options.shell_program.clone().unwrap_or(default_program);
    let powershell = Path::new(&program)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|name| {
            name.eq_ignore_ascii_case("powershell") || name.eq_ignore_ascii_case("pwsh")
        });
    let args = if powershell {
        vec![OsString::from("-NoLogo"), OsString::from("-NoProfile")]
    } else {
        default_args
    };
    let mut environment = BTreeMap::new();
    for (name, value) in [
        ("KERMINAL_CONFIG_ROOT", config_root),
        ("HOME", home_root),
        ("TMPDIR", temp_child_root),
    ] {
        environment.insert(OsString::from(name), value.as_os_str().to_owned());
    }
    #[cfg(windows)]
    {
        environment.insert(
            OsString::from("USERPROFILE"),
            home_root.as_os_str().to_owned(),
        );
        environment.insert(
            OsString::from("TEMP"),
            temp_child_root.as_os_str().to_owned(),
        );
        environment.insert(
            OsString::from("TMP"),
            temp_child_root.as_os_str().to_owned(),
        );
        environment.insert(
            OsString::from("APPDATA"),
            temp_root.join("appdata").as_os_str().to_owned(),
        );
        environment.insert(
            OsString::from("LOCALAPPDATA"),
            temp_root.join("local-appdata").as_os_str().to_owned(),
        );
    }
    #[cfg(unix)]
    {
        environment.insert(
            OsString::from("XDG_CONFIG_HOME"),
            temp_root.join("xdg-config").as_os_str().to_owned(),
        );
        environment.insert(
            OsString::from("XDG_CACHE_HOME"),
            temp_root.join("xdg-cache").as_os_str().to_owned(),
        );
    }
    LocalSessionTarget {
        program,
        args,
        working_directory: Some(temp_root.to_path_buf()),
        environment,
    }
}
