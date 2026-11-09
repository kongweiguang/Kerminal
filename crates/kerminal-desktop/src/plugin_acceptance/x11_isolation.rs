//! Linux 插件验收的私有 Xvfb 身份核对，仅在验收 feature 中使用。
//!
//! @author kongweiguang

use std::{ffi::OsStr, fs, io, sync::OnceLock};

use serde_json::{Value, json};

const MAX_PARENT_DEPTH: usize = 4;
static SERVER: OnceLock<ServerIdentity> = OnceLock::new();

/// 验收专用截图读取已核实的私有 X11 像素；Linux GPUI 尚未实现离屏截图。
/// 使用有界子进程和 PNG 头部校验，工具缺失或显示身份变化都必须使验收失败。
///
/// @author kongweiguang
pub(super) fn save_frame(
    path: &std::path::Path,
    origin: (u32, u32),
    size: (u32, u32),
) -> Result<(), io::Error> {
    let evidence = capture()?;
    let display = evidence["display"]
        .as_str()
        .ok_or_else(|| io::Error::other("私有显示证据缺少 DISPLAY"))?;
    let output = std::process::Command::new("timeout")
        .args([
            "15s",
            "ffmpeg",
            "-nostdin",
            "-loglevel",
            "error",
            "-f",
            "x11grab",
        ])
        .args([
            "-framerate",
            "1",
            "-video_size",
            &format!("{}x{}", size.0, size.1),
        ])
        .args(["-i", &format!("{display}+{},{}", origin.0, origin.1)])
        .args([
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "pipe:1",
        ])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "私有 X11 截图失败：{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let png = output.stdout;
    if png.len() < 33
        || &png[..8] != b"\x89PNG\r\n\x1a\n"
        || &png[12..16] != b"IHDR"
        || png[16..20] != size.0.to_be_bytes()
        || png[20..24] != size.1.to_be_bytes()
    {
        return Err(io::Error::other("私有 X11 截图格式或尺寸无效"));
    }
    capture()?;
    fs::write(path, png)
}

#[derive(Clone)]
struct ServerIdentity {
    display: String,
    pid: u32,
    owner_pid: u32,
    start_time: u64,
}

#[derive(Debug, Eq, PartialEq)]
struct ProcessStatus {
    parent_pid: u32,
    uids: [u32; 4],
}

/// 每次采样都复核显示、进程身份和亲缘关系；缓存只避免反复全量枚举 `/proc`。
///
/// @author kongweiguang
pub(super) fn capture() -> Result<Value, io::Error> {
    let raw_display =
        std::env::var("DISPLAY").map_err(|_| io::Error::other("插件验收缺少本地 X11 显示"))?;
    let display = normalize_display(&raw_display)
        .ok_or_else(|| io::Error::other("插件验收只接受本地 X11 显示"))?;
    let process_id = std::process::id();
    let current = read_status(process_id)?;
    let parents = parent_chain(&current)?;
    let identity = if let Some(identity) = SERVER.get() {
        identity.clone()
    } else {
        let identity = find_server(display, &current, &parents)?;
        let _ = SERVER.set(identity.clone());
        identity
    };
    verify_server(&identity, display, &current, &parents)?;
    Ok(json!({
        "platformSupported": true,
        "isolationProven": true,
        "display": display,
        "processId": process_id,
        "xvfbPid": identity.pid,
        "xvfbParentPid": identity.owner_pid,
        "commonOwnerPid": identity.owner_pid,
        "xvfbStartTimeTicks": identity.start_time,
        "effectiveUid": current.uids[1],
    }))
}

/// 只接受无主机名的 `:n` 或 `:n.0`；其余屏幕和传输形式不能证明是专属显示。
///
/// @author kongweiguang
fn normalize_display(display: &str) -> Option<&str> {
    let display = display.strip_suffix(".0").unwrap_or(display);
    let number = display.strip_prefix(':')?;
    if number.is_empty()
        || !number.bytes().all(|byte| byte.is_ascii_digit())
        || number.parse::<u32>().is_err()
    {
        return None;
    }
    Some(display)
}

/// 解析完整 UID 四元组和唯一 PPid，避免缺失字段或格式错误伪装成同一用户。
///
/// @author kongweiguang
fn parse_status(source: &str) -> Option<ProcessStatus> {
    let mut parent_pid = None;
    let mut uids = None;
    for line in source.lines() {
        if let Some(value) = line.strip_prefix("PPid:") {
            if parent_pid.is_some() {
                return None;
            }
            let mut values = value.split_whitespace();
            parent_pid = Some(values.next()?.parse().ok()?);
            if values.next().is_some() {
                return None;
            }
        } else if let Some(value) = line.strip_prefix("Uid:") {
            if uids.is_some() {
                return None;
            }
            let mut values = value.split_whitespace();
            uids = Some([
                values.next()?.parse().ok()?,
                values.next()?.parse().ok()?,
                values.next()?.parse().ok()?,
                values.next()?.parse().ok()?,
            ]);
            if values.next().is_some() {
                return None;
            }
        }
    }
    Some(ProcessStatus {
        parent_pid: parent_pid?,
        uids: uids?,
    })
}

/// 精确匹配可执行名、第一个显示参数及相邻的 `-nolisten tcp`，拒绝重新启用 TCP。
///
/// @author kongweiguang
fn matches_command(command: &[u8], display: &str) -> bool {
    if !command.ends_with(&[0]) {
        return false;
    }
    let arguments: Vec<_> = command.split(|byte| *byte == 0).collect();
    if arguments
        .first()
        .and_then(|program| program.rsplit(|byte| *byte == b'/').next())
        != Some(b"Xvfb".as_slice())
        || arguments
            .get(1)
            .and_then(|value| std::str::from_utf8(value).ok())
            .and_then(normalize_display)
            != Some(display)
    {
        return false;
    }
    arguments
        .windows(2)
        .any(|pair| pair == [b"-nolisten".as_slice(), b"tcp".as_slice()])
        && !arguments
            .windows(2)
            .any(|pair| pair == [b"-listen".as_slice(), b"tcp".as_slice()])
}

/// 父进程必须是本进程近四级祖先且不是 init；完整 UID 匹配避免跨用户误认。
///
/// @author kongweiguang
fn server_is_owned(server: &ProcessStatus, current: &ProcessStatus, parents: &[u32]) -> bool {
    server.uids == current.uids && server.parent_pid > 1 && parents.contains(&server.parent_pid)
}

/// 仅向上读取有限祖先，进程退出或状态损坏时停止验收而非猜测归属。
///
/// @author kongweiguang
fn parent_chain(current: &ProcessStatus) -> Result<Vec<u32>, io::Error> {
    let mut parents = Vec::with_capacity(MAX_PARENT_DEPTH);
    let mut next = current.parent_pid;
    for _ in 0..MAX_PARENT_DEPTH {
        if next <= 1 {
            break;
        }
        if parents.contains(&next) {
            return Err(io::Error::other("插件验收父进程链无效"));
        }
        parents.push(next);
        next = read_status(next)?.parent_pid;
    }
    Ok(parents)
}

/// 所有进程字段来自内核状态而不是可被单独设置的环境变量。
///
/// @author kongweiguang
fn read_status(pid: u32) -> Result<ProcessStatus, io::Error> {
    let source = fs::read_to_string(format!("/proc/{pid}/status"))?;
    parse_status(&source).ok_or_else(|| io::Error::other("无法解析插件验收进程身份"))
}

/// 使用内核启动时间识别 PID 复用；进程名可含括号，因此从最后一个右括号定位字段。
///
/// @author kongweiguang
fn read_start_time(pid: u32) -> Result<u64, io::Error> {
    let source = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    source
        .rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::other("无法解析 Xvfb 启动身份"))
}

/// 首次只接受唯一匹配的活进程；其他用户或不属于验收进程树的 Xvfb 一律忽略。
///
/// @author kongweiguang
fn find_server(
    display: &str,
    current: &ProcessStatus,
    parents: &[u32],
) -> Result<ServerIdentity, io::Error> {
    let mut found = None;
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid <= 1 {
            continue;
        }
        let Ok(command) = fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if !matches_command(&command, display) {
            continue;
        }
        let Ok(status) = read_status(pid) else {
            continue;
        };
        if !server_is_owned(&status, current, parents) {
            continue;
        }
        let Ok(start_time) = read_start_time(pid) else {
            continue;
        };
        let identity = ServerIdentity {
            display: display.to_owned(),
            pid,
            owner_pid: status.parent_pid,
            start_time,
        };
        if verify_server(&identity, display, current, parents).is_err() {
            continue;
        }
        if found.replace(identity).is_some() {
            return Err(io::Error::other("存在多个匹配的 Xvfb 验收进程"));
        }
    }
    found.ok_or_else(|| io::Error::other("未找到属于本次验收的 Xvfb 显示"))
}

/// 重读可执行文件、参数、UID 和启动时间，防止显示切换、进程退出或 PID 复用后沿用旧证据。
///
/// @author kongweiguang
fn verify_server(
    identity: &ServerIdentity,
    display: &str,
    current: &ProcessStatus,
    parents: &[u32],
) -> Result<(), io::Error> {
    if identity.display != display || read_start_time(identity.pid)? != identity.start_time {
        return Err(io::Error::other("Xvfb 显示或进程身份已变化"));
    }
    let executable = fs::read_link(format!("/proc/{}/exe", identity.pid))?;
    let command = fs::read(format!("/proc/{}/cmdline", identity.pid))?;
    let status = read_status(identity.pid)?;
    if executable.file_name() != Some(OsStr::new("Xvfb"))
        || !matches_command(&command, display)
        || status.parent_pid != identity.owner_pid
        || !server_is_owned(&status, current, parents)
        || read_start_time(identity.pid)? != identity.start_time
    {
        return Err(io::Error::other("Xvfb 不再满足插件验收隔离条件"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/support/x11_isolation_unit.rs"]
mod tests;
