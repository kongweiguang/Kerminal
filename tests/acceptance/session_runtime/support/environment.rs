//! 采集 OS、编译器、源码版本和图形栈探针。
//!
//! 探针失败只记录为 unavailable，不阻断真实 session 场景。
//!
//! @author kongweiguang

use std::fs::File;
#[cfg(target_os = "linux")]
use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(target_os = "linux")]
use std::thread;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

#[cfg(target_os = "linux")]
const DISPLAY_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const DISPLAY_PROBE_OUTPUT_LIMIT: usize = 16 * 1024;

/// 按真实已编译 example 的 manifest 路径定位新 workspace 根目录。
pub(in crate::acceptance) fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

/// 环境字段区分 Vulkan 与 GLX，避免把 WSLg 的 OpenGL renderer 当成 Vulkan GPU。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::acceptance) struct SourceEnvironment {
    pub(in crate::acceptance) os: String,
    pub(in crate::acceptance) arch: String,
    pub(in crate::acceptance) kernel: Option<String>,
    pub(in crate::acceptance) wsl_distro: Option<String>,
    pub(in crate::acceptance) cpu_model: Option<String>,
    pub(in crate::acceptance) cpu_count: Option<usize>,
    pub(in crate::acceptance) total_memory_bytes: Option<u64>,
    pub(in crate::acceptance) rustc_version: Option<String>,
    pub(in crate::acceptance) cargo_version: Option<String>,
    pub(in crate::acceptance) cargo_target_dir: Option<String>,
    pub(in crate::acceptance) build_profile: &'static str,
    pub(in crate::acceptance) git_revision: Option<String>,
    pub(in crate::acceptance) git_dirty: Option<bool>,
    pub(in crate::acceptance) runner_binary_sha256: Option<String>,
    pub(in crate::acceptance) cargo_lock_sha256: Option<String>,
    pub(in crate::acceptance) isolated_config_root: bool,
    pub(in crate::acceptance) vulkan_probe: ProbeRecord,
    pub(in crate::acceptance) glx_probe: ProbeRecord,
    pub(in crate::acceptance) wayland_display: Option<String>,
    pub(in crate::acceptance) x11_display: Option<String>,
    pub(in crate::acceptance) vulkan_icd_override_present: bool,
}

/// 记录图形栈探针是否可执行及其受限摘要。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::acceptance) struct ProbeRecord {
    pub(in crate::acceptance) command: String,
    pub(in crate::acceptance) status: String,
    pub(in crate::acceptance) output: Option<String>,
}

/// 从当前主机、编译环境、Git 和独立图形栈探针采集可复现环境信息。
pub(in crate::acceptance) fn collect_environment(root: &Path) -> SourceEnvironment {
    let system = System::new_with_specifics(
        RefreshKind::new()
            .with_cpu(CpuRefreshKind::new().with_cpu_usage())
            .with_memory(MemoryRefreshKind::everything()),
    );
    let (vulkan_probe, glx_probe) = display_stack_probes();
    SourceEnvironment {
        os: System::long_os_version()
            .or_else(System::name)
            .unwrap_or_else(|| std::env::consts::OS.to_owned()),
        arch: std::env::consts::ARCH.to_owned(),
        kernel: System::kernel_version(),
        wsl_distro: std::env::var("WSL_DISTRO_NAME").ok(),
        cpu_model: system
            .cpus()
            .first()
            .map(|cpu| cpu.brand().trim())
            .filter(|brand| !brand.is_empty())
            .map(str::to_owned),
        cpu_count: (!system.cpus().is_empty()).then_some(system.cpus().len()),
        total_memory_bytes: reported_nonzero(system.total_memory()),
        rustc_version: command_version("rustc", "--version"),
        cargo_version: command_version("cargo", "--version"),
        cargo_target_dir: std::env::var("CARGO_TARGET_DIR").ok(),
        build_profile: std::env::current_exe()
            .ok()
            .map(|path| detect_build_profile(&path))
            .unwrap_or("unknown"),
        git_revision: git_text(root, &["rev-parse", "HEAD"]),
        git_dirty: git_text(root, &["status", "--porcelain", "--untracked-files=no"])
            .map(|status| !status.trim().is_empty()),
        runner_binary_sha256: std::env::current_exe()
            .ok()
            .and_then(|executable| sha256_file(&executable)),
        cargo_lock_sha256: sha256_file(&root.join("Cargo.lock")),
        isolated_config_root: true,
        vulkan_probe,
        glx_probe,
        wayland_display: std::env::var("WAYLAND_DISPLAY").ok(),
        x11_display: std::env::var("DISPLAY").ok(),
        vulkan_icd_override_present: std::env::var_os("VK_ICD_FILENAMES").is_some(),
    }
}

/// 仅从 Cargo example 可执行文件的标准目录读取 profile，custom/unknown 不猜测优化状态。
fn detect_build_profile(executable: &Path) -> &'static str {
    let Some(examples_dir) = executable.parent() else {
        return "unknown";
    };
    if examples_dir.file_name().and_then(|name| name.to_str()) != Some("examples") {
        return "unknown";
    }
    let Some(profile_name) = examples_dir
        .parent()
        .and_then(|profile_dir| profile_dir.file_name())
        .and_then(|name| name.to_str())
    else {
        return "unknown";
    };

    match profile_name {
        "debug" => "debug",
        "release" => "release",
        _ => "custom",
    }
}
/// 将 sysinfo 的未采集零值表达为缺失，避免把未知硬件容量报告成真实零。
fn reported_nonzero(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}

/// 在 Linux 记录 Vulkan 与 GLX 两项彼此独立的探针，其它平台如实标为不适用。
fn display_stack_probes() -> (ProbeRecord, ProbeRecord) {
    #[cfg(target_os = "linux")]
    {
        (
            run_probe("vulkaninfo", &["--summary"]),
            run_probe("glxinfo", &["-B"]),
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        (
            not_applicable_probe("vulkaninfo --summary"),
            not_applicable_probe("glxinfo -B"),
        )
    }
}

/// 在有限时间内运行图形栈探针并排空输出，避免驱动诊断卡住验收主流程。
#[cfg(target_os = "linux")]
fn run_probe(program: &str, args: &[&str]) -> ProbeRecord {
    let command_name = format!("{program} {}", args.join(" "));
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return ProbeRecord {
                command: command_name,
                status: format!("unavailable: {error}"),
                output: None,
            };
        }
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        return ProbeRecord {
            command: command_name,
            status: "stdout unavailable".to_owned(),
            output: None,
        };
    };
    let reader = thread::spawn(move || read_capped(stdout, DISPLAY_PROBE_OUTPUT_LIMIT));
    let deadline = Instant::now() + DISPLAY_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let exit_code = status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".to_owned());
                break if status.success() {
                    format!("success:{exit_code}")
                } else {
                    format!("failure:{exit_code}")
                };
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break "timeout".to_owned();
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break format!("wait-error: {error}");
            }
        }
    };
    let output = reader
        .join()
        .ok()
        .and_then(Result::ok)
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty());
    ProbeRecord {
        command: command_name,
        status,
        output,
    }
}

/// 将非 Linux 平台图形能力标注为本 runner 未探测，不推断其驱动状态。
#[cfg(not(target_os = "linux"))]
fn not_applicable_probe(command: &str) -> ProbeRecord {
    ProbeRecord {
        command: command.to_owned(),
        status: "not-applicable-on-this-platform".to_owned(),
        output: None,
    }
}

/// 持续排空子进程输出并仅保留固定上限，避免环境探针产生无界内存。
#[cfg(target_os = "linux")]
fn read_capped(mut reader: impl Read, limit: usize) -> io::Result<String> {
    let mut kept = Vec::with_capacity(limit.min(4096));
    let mut buffer = [0_u8; 4096];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(kept.len());
        kept.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(String::from_utf8_lossy(&kept).into_owned())
}

/// 读取不含环境变量、命令行或工作目录的工具版本字符串。
fn command_version(program: &str, argument: &str) -> Option<String> {
    let output = Command::new(program)
        .arg(argument)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// 通过 Git 只获取 revision 或简短工作区状态，报告不包含源码差异正文。
fn git_text(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// 流式散列验收可执行文件或锁文件，避免将大型二进制整体载入内存。
fn sha256_file(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];

    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Some(format!("{:x}", hasher.finalize()))
}
