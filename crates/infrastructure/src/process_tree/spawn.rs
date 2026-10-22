//! 统一启动受管 Tokio 子进程并保留平台进程组或 Job 的生命周期。
//!
//! 该模块已由 `process_tree.rs` 注册，运维命令和 RDP 连接共用同一 spawn wrapper。
//! @author kongweiguang

use std::{future::Future, io, pin::Pin, process::ExitStatus};

/// 子进程平台选项；Windows 创建标志必须经 process-wrap wrapper 合并，不能直接写回 Command。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProcessSpawnOptions {
    /// Tokio child drop 时是否要求整个 Windows Job 或 Unix process group 一起终止。
    pub(crate) kill_on_drop: bool,
    /// Windows `PROCESS_CREATION_FLAGS` 位；其他平台忽略该平台专用值。
    pub(crate) windows_creation_flags: u32,
}

/// 拥有 Tokio child 和平台进程树 wrapper 的单一回收句柄。
pub(crate) struct ManagedTokioChild {
    inner: Box<dyn process_wrap::tokio::ChildWrapper>,
}

impl super::ProcessTreeGuard {
    /// 配置并启动 Tokio child；统一保留当前 kill-on-drop 与 Windows 创建标志语义。
    pub(crate) fn spawn_tokio(
        command: tokio::process::Command,
        options: ProcessSpawnOptions,
    ) -> io::Result<ManagedTokioChild> {
        #[cfg(unix)]
        {
            let mut wrapped = process_wrap::tokio::CommandWrap::from(command);
            wrapped.wrap(process_wrap::tokio::ProcessGroup::leader());
            if options.kill_on_drop {
                wrapped.wrap(process_wrap::tokio::KillOnDrop);
            }
            wrapped.spawn().map(|inner| ManagedTokioChild { inner })
        }
        #[cfg(windows)]
        {
            use windows::Win32::System::Threading::{
                CREATE_BREAKAWAY_FROM_JOB, CREATE_SUSPENDED, PROCESS_CREATION_FLAGS,
            };

            validate_windows_creation_flags(
                options.windows_creation_flags,
                CREATE_BREAKAWAY_FROM_JOB.0 | CREATE_SUSPENDED.0,
            )?;

            let mut wrapped = process_wrap::tokio::CommandWrap::from(command);
            wrapped.wrap(process_wrap::tokio::CreationFlags(PROCESS_CREATION_FLAGS(
                options.windows_creation_flags,
            )));
            wrapped.wrap(process_wrap::tokio::JobObject);
            if options.kill_on_drop {
                wrapped.wrap(process_wrap::tokio::KillOnDrop);
            }
            wrapped.spawn().map(|inner| ManagedTokioChild { inner })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (command, options);
            Err(unsupported_platform())
        }
    }
}

impl ManagedTokioChild {
    /// 暴露被 wrapper 持有的 PID，供诊断使用；进程树身份仍由本对象持有。
    pub(crate) fn id(&self) -> Option<u32> {
        self.inner.id()
    }

    /// 允许现有 Tokio 命令沿用原有异步 stdin pump。
    pub(crate) fn stdin(&mut self) -> &mut Option<tokio::process::ChildStdin> {
        self.inner.stdin()
    }

    /// 允许现有 Tokio 命令沿用原有异步 stdout reader。
    pub(crate) fn stdout(&mut self) -> &mut Option<tokio::process::ChildStdout> {
        self.inner.stdout()
    }

    /// 允许现有 Tokio 命令沿用原有异步 stderr reader。
    pub(crate) fn stderr(&mut self) -> &mut Option<tokio::process::ChildStderr> {
        self.inner.stderr()
    }

    /// 查看直接 child 的退出状态，不消费拥有 Job/process group 的 wrapper。
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.inner.try_wait()
    }

    /// 返回借用式 wait future；deadline 超时只丢 future，调用方仍持有 child 可重试清理。
    pub(crate) fn wait(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = io::Result<ExitStatus>> + Send + '_>> {
        self.inner.wait()
    }

    /// Windows Job 的立即终止路径；调用方仍须在原 deadline 内 wait/reap。
    /// Unix 必须走带宽限期的进程组收尾，因此此方法只在 Windows 编译。
    #[cfg(windows)]
    pub(crate) fn start_kill_tree(&mut self) -> io::Result<()> {
        start_kill_tokio(&mut *self.inner)
    }

    /// Unix 先向进程组发送 HUP，再在 deadline 内等待原有宽限期并 KILL；Windows 直接终止 Job。
    pub(crate) async fn terminate_tree_until(
        &mut self,
        deadline: tokio::time::Instant,
    ) -> io::Result<()> {
        #[cfg(unix)]
        {
            terminate_tokio_process_group(&mut *self.inner, deadline).await
        }
        #[cfg(windows)]
        {
            let _ = deadline;
            self.start_kill_tree()
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = deadline;
            Err(unsupported_platform())
        }
    }
}

/// Unix 上 signal 仅命中由 ProcessGroup wrapper 创建的子进程组，宽限等待不越过共享 deadline。
#[cfg(unix)]
async fn terminate_tokio_process_group(
    child: &mut dyn process_wrap::tokio::ChildWrapper,
    deadline: tokio::time::Instant,
) -> io::Result<()> {
    match child.signal(libc::SIGHUP) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(()),
        Err(error) => return Err(error),
    }

    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    let grace = remaining.min(super::PROCESS_GROUP_GRACE);
    if !grace.is_zero() {
        tokio::time::sleep(grace).await;
    }
    start_kill_tokio(child)
}

/// 对 Tokio child 统一映射 process-wrap 的进程树 kill，并只忽略进程组已消失的 ESRCH。
fn start_kill_tokio(child: &mut dyn process_wrap::tokio::ChildWrapper) -> io::Result<()> {
    match child.start_kill() {
        Ok(()) => Ok(()),
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        Err(error) => Err(error),
    }
}

/// 不支持的平台拒绝创建裸子进程，防止调用方误以为已获得进程树所有权。
#[cfg(not(any(unix, windows)))]
fn unsupported_platform() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "managed process-tree spawning is unsupported on this platform",
    )
}

/// 不允许调用方绕过 Job membership 或让子进程在 wrapper 完成分配后仍保持挂起。
#[cfg(windows)]
fn validate_windows_creation_flags(flags: u32, forbidden: u32) -> io::Result<()> {
    if flags & forbidden != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows process flags cannot request breakaway or explicit suspension",
        ));
    }
    Ok(())
}
