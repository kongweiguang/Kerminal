//! 用 Windows ConPTY 验证 Job 在进程首条指令前生效，立即派生的后代也由关闭 Job 收回。
//!
//! @author kongweiguang

#![cfg(windows)]

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use portable_pty::{CommandBuilder, PtySize, PtySystem, SlavePty, native_pty_system};
use windows_sys::Win32::Foundation::{GetLastError, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

const HELPER_SOURCE: &str = "tests/support/portable_pty_job_tree_helper.rs";
const MARKER_ENVIRONMENT_VARIABLE: &str = "KERMINAL_PTY_JOB_MARKER";
const DESCENDANT_START_TIMEOUT: Duration = Duration::from_secs(10);
const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(8);

/// 持有测试专属 Job；关闭最后一个 owner handle 会终止仍在其内的进程树。
///
/// @author kongweiguang
struct KillOnCloseJob(OwnedHandle);

impl KillOnCloseJob {
    /// 在任何 PTY child 创建前配置 kill-on-close，避免测试自身依赖 post-spawn assignment。
    ///
    /// @author kongweiguang
    fn create() -> io::Result<Self> {
        let raw_job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw_job.is_null() {
            return Err(io::Error::from_raw_os_error(
                unsafe { GetLastError() } as i32
            ));
        }
        // SAFETY: CreateJobObjectW returned one owned handle; this wrapper closes it exactly once.
        let job = unsafe { OwnedHandle::from_raw_handle(raw_job) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job.as_raw_handle() as _,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            return Err(io::Error::from_raw_os_error(
                unsafe { GetLastError() } as i32
            ));
        }
        Ok(Self(job))
    }

    /// 借出短期 spawn 所需的安全 handle view；Job 所有权始终留在测试 owner 中。
    ///
    /// @author kongweiguang
    fn borrowed(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        self.0.as_handle()
    }
}

/// 用当前 Rust 工具链编译仅依赖标准库的进程探针，避免测试二进制参数承担 helper 状态机。
///
/// @author kongweiguang
fn compile_helper(temp_root: &Path) -> Result<PathBuf> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(HELPER_SOURCE);
    let executable = temp_root.join("portable-pty-job-tree-helper.exe");
    let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
    let output = Command::new(compiler)
        .arg("--edition=2024")
        .arg(source)
        .arg("-o")
        .arg(&executable)
        .output()
        .context("编译测试专用 Windows 子进程探针")?;
    ensure!(
        output.status.success(),
        "测试 helper 编译失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(executable)
}

/// 等待 helper 发布孙进程 PID；有限轮询使超时路径仍由拥有的 Job 句柄清理。
///
/// @author kongweiguang
fn wait_for_descendant_pid(marker: &Path) -> Result<u32> {
    let deadline = Instant::now() + DESCENDANT_START_TIMEOUT;
    loop {
        match fs::read_to_string(marker) {
            Ok(contents) => {
                if let Ok(pid) = contents.trim().parse::<u32>() {
                    return Ok(pid);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("读取测试 helper 发布的孙进程 PID"),
        }
        ensure!(
            Instant::now() < deadline,
            "测试 helper 未在 deadline 内启动孙进程"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// 打开指定 PID 的同步句柄，以免查询进程状态时持有对测试树的额外控制权。
///
/// @author kongweiguang
fn open_process_for_wait(pid: u32) -> io::Result<OwnedHandle> {
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return Err(io::Error::from_raw_os_error(
            unsafe { GetLastError() } as i32
        ));
    }
    // SAFETY: OpenProcess returned one owned process handle; this wrapper closes it exactly once.
    Ok(unsafe { OwnedHandle::from_raw_handle(process) })
}

/// 以有界等待确认 Job close 已真正终止指定进程，而不是只确认发送了清理请求。
///
/// @author kongweiguang
fn process_signaled(process: &OwnedHandle, timeout: Duration) -> io::Result<bool> {
    let millis = timeout.as_millis().min(u32::MAX as u128) as u32;
    match unsafe { WaitForSingleObject(process.as_raw_handle() as _, millis) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

/// 通过真实 ConPTY 的 Job List 属性启动急速派生后代的 child，再验证 Job close 同时结束两者。
///
/// @author kongweiguang
#[test]
fn conpty_job_list_contains_a_descendant_spawned_at_startup() -> Result<()> {
    let temp_root = tempfile::tempdir().context("创建隔离 PTY Job 测试目录")?;
    let helper = compile_helper(temp_root.path())?;
    let marker = temp_root.path().join("descendant-pid.txt");
    let job = KillOnCloseJob::create().context("在 PTY spawn 前创建 kill-on-close Job")?;
    let pty = native_pty_system()
        .openpty(PtySize::default())
        .context("创建 Windows ConPTY")?;
    let mut command = CommandBuilder::new(helper.as_os_str());
    command.args(["root"]);
    command.env(MARKER_ENVIRONMENT_VARIABLE, marker.as_os_str());

    let mut child = pty
        .slave
        .spawn_command_in_job(command, job.borrowed())
        .context("通过同一 STARTUPINFOEX 属性表启动 Job 内的 ConPTY child")?;
    let root_pid = child
        .process_id()
        .context("portable-pty 未返回 ConPTY child 的 PID")?;
    let root_process = open_process_for_wait(root_pid).context("打开 ConPTY 根进程同步句柄")?;
    let descendant_pid = wait_for_descendant_pid(&marker)?;
    let descendant = open_process_for_wait(descendant_pid).context("打开立即派生的孙进程句柄")?;
    ensure!(
        !process_signaled(&descendant, Duration::ZERO)?,
        "孙进程在 Job close 前已退出，无法验证包含关系"
    );

    drop(job);

    ensure!(
        process_signaled(&descendant, PROCESS_EXIT_TIMEOUT)?,
        "关闭 Job 后立即派生的孙进程仍存活"
    );
    ensure!(
        process_signaled(&root_process, PROCESS_EXIT_TIMEOUT)?,
        "关闭 Job 后 ConPTY 根进程仍存活"
    );
    child.wait().context("回收已被 Job 终止的 ConPTY child")?;
    Ok(())
}
