//! Unix process group 与 Windows Job Object 的共用子进程树回收原语。
//!
//! @author kongweiguang

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

#[path = "process_tree/spawn.rs"]
pub(crate) mod spawn;

#[cfg(unix)]
const PROCESS_GROUP_GRACE: std::time::Duration = std::time::Duration::from_millis(120);

/// 保存操作系统进程树句柄，并可延长 shell 启动资源的生命周期。
pub(crate) struct ProcessTreeGuard {
    #[cfg(unix)]
    process_group: libc::pid_t,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
    terminated: AtomicBool,
    _lifetime_guard: Option<Box<dyn Send + Sync + 'static>>,
}

/// Windows ConPTY spawn 前配置好的 kill-on-close Job；普通程序原子加入，应用别名在首指令前加入。
///
/// @author kongweiguang
#[cfg(windows)]
pub(crate) struct PreparedWindowsJob {
    job: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl PreparedWindowsJob {
    /// 创建并配置空 Job，使后续 `PROC_THREAD_ATTRIBUTE_JOB_LIST` 原子加入的进程受句柄关闭保护。
    ///
    /// @author kongweiguang
    pub(crate) fn new() -> io::Result<Self> {
        use std::os::windows::io::{FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        // SAFETY: null security/name requests an anonymous Job Object owned by this value.
        let raw_job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw_job.is_null() {
            return Err(io::Error::from_raw_os_error(
                unsafe { GetLastError() } as i32
            ));
        }

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: limits is initialized and its exact size is passed to the Win32 API.
        let configured = unsafe {
            SetInformationJobObject(
                raw_job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            let error = unsafe { GetLastError() } as i32;
            // SAFETY: raw_job was returned by CreateJobObjectW and is not yet owned elsewhere.
            unsafe { CloseHandle(raw_job) };
            return Err(io::Error::from_raw_os_error(error));
        }

        // SAFETY: successful CreateJobObjectW returned one owned handle, adopted exactly once.
        let job = unsafe { OwnedHandle::from_raw_handle(raw_job) };
        Ok(Self { job })
    }

    /// 借出带生命周期约束的 Job handle，供 portable-pty 写入同一进程创建属性表。
    ///
    /// @author kongweiguang
    pub(crate) fn borrowed_handle(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        use std::os::windows::io::AsHandle;
        self.job.as_handle()
    }

    /// 只调用 PTY 的受管入口；普通程序原子加入 Job，Store 兼容路径也在恢复主线程前加入。
    /// 不支持受管创建的后端仍拒绝启动，绝不退回可能遗留后代的普通 spawn。
    ///
    /// @author kongweiguang
    pub(crate) fn spawn_command(
        &self,
        slave: &dyn portable_pty::SlavePty,
        command: portable_pty::CommandBuilder,
    ) -> anyhow::Result<Box<dyn portable_pty::Child + Send + Sync>> {
        slave.spawn_command_in_job(command, self.borrowed_handle())
    }

    /// 在 PTY child 已于首指令前加入 Job 后转交给既有树 guard，统一负责终止和 handle 生命周期。
    ///
    /// @author kongweiguang
    pub(crate) fn into_guard(
        self,
        lifetime_guard: Option<Box<dyn Send + Sync + 'static>>,
    ) -> ProcessTreeGuard {
        ProcessTreeGuard {
            job: self.job,
            terminated: AtomicBool::new(false),
            _lifetime_guard: lifetime_guard,
        }
    }
}

impl ProcessTreeGuard {
    /// 在 Unix portable-pty 已建立独立进程组后取得其回收句柄；Windows 必须使用 spawn 前 Job-list 路径。
    ///
    /// @author kongweiguang
    #[cfg(not(windows))]
    pub(crate) fn from_pty(
        master: &dyn portable_pty::MasterPty,
        child: &dyn portable_pty::Child,
        lifetime_guard: Option<Box<dyn Send + Sync + 'static>>,
    ) -> Result<Self, (io::Error, Option<Box<dyn Send + Sync + 'static>>)> {
        #[cfg(unix)]
        {
            let _ = child;
            let process_group = master.process_group_leader().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "portable-pty did not expose the child process group",
                )
            });
            match process_group.and_then(|group| Self::from_process_group(group, None)) {
                Ok(mut guard) => {
                    guard._lifetime_guard = lifetime_guard;
                    Ok(guard)
                }
                Err(error) => Err((error, lifetime_guard)),
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (master, child);
            Err((
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "process tree cleanup is unsupported on this platform",
                ),
                lifetime_guard,
            ))
        }
    }

    /// 建立 Unix process group guard；拒绝非正 PGID，避免信号落到调用进程。
    ///
    /// @author kongweiguang
    #[cfg(unix)]
    fn from_process_group(
        process_group: libc::pid_t,
        lifetime_guard: Option<Box<dyn Send + Sync + 'static>>,
    ) -> io::Result<Self> {
        if process_group <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process group id must be positive",
            ));
        }
        Ok(Self {
            process_group,
            terminated: AtomicBool::new(false),
            _lifetime_guard: lifetime_guard,
        })
    }

    /// 阻塞式终止记录的 Unix process group 或当前 Windows Job 成员；Unix 先 HUP 再宽限后 KILL。
    ///
    /// 调用方应在阻塞池运行该方法，并自行在其 deadline 内完成 child wait/reap。
    ///
    /// @author kongweiguang
    pub(crate) fn terminate(&self) -> io::Result<()> {
        if self.terminated.load(Ordering::Acquire) {
            return Ok(());
        }
        let result = {
            #[cfg(unix)]
            {
                terminate_unix_process_group(self.process_group)
            }
            #[cfg(windows)]
            {
                use windows_sys::Win32::Foundation::GetLastError;
                use windows_sys::Win32::System::JobObjects::TerminateJobObject;
                // SAFETY: the Job handle is owned by this guard and remains valid for the call.
                let result = unsafe {
                    TerminateJobObject(
                        std::os::windows::io::AsRawHandle::as_raw_handle(&self.job) as _,
                        1,
                    )
                };
                if result == 0 {
                    Err(io::Error::from_raw_os_error(
                        unsafe { GetLastError() } as i32
                    ))
                } else {
                    Ok(())
                }
            }
            #[cfg(not(any(unix, windows)))]
            {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "process tree cleanup is unsupported on this platform",
                ))
            }
        };
        if result.is_ok() {
            self.terminated.store(true, Ordering::Release);
        }
        result
    }
}

/// 关闭指定进程组并忽略已自然退出的 ESRCH，避免把正常退出误报为清理失败。
///
/// @author kongweiguang
#[cfg(unix)]
fn terminate_unix_process_group(process_group: libc::pid_t) -> io::Result<()> {
    // SAFETY: negative pgid targets only the isolated process group created for this child.
    if unsafe { libc::kill(-process_group, libc::SIGHUP) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(error);
    }
    std::thread::sleep(PROCESS_GROUP_GRACE);
    // SAFETY: the same isolated process group is the sole signal target; ESRCH means it exited.
    if unsafe { libc::kill(-process_group, libc::SIGKILL) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(error);
    }
    Ok(())
}
