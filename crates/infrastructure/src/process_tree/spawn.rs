//! 统一启动受管 Tokio 子进程并保留平台进程组或 Job 的生命周期。
//!
//! 该模块已由 `process_tree.rs` 注册，运维命令和 RDP 连接共用同一 spawn wrapper。
//! @author kongweiguang

use std::{
    future::Future,
    io,
    pin::Pin,
    process::ExitStatus,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

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
    kill_on_drop: bool,
    /// 是否已确认直接子进程被回收；Drop 只在尚未确认时补发终止。
    ///
    /// 用 `Arc` 而非普通 bool，是因为 `wait()` 返回借用式 future，需要在 future
    /// 完成后从外部写入该标志；确认回收后再次 killpg 可能命中已被复用的 pgid，
    /// 误杀属于其它进程组的进程。
    reaped: Arc<AtomicBool>,
}

/// Drop 必须先让 wrapper 终止整个进程组或 Job，再交给字段析构。
///
/// process-wrap 的 `KillOnDrop` 只是把 `kill_on_drop` 写进 Tokio Command；Tokio 在 Child
/// 析构时仅向直接子进程发信号。Unix 上直接子进程是进程组组长，孙进程仍留在组内继续运行，
/// 会逃过回收并占住继承的管道；Windows 上 Job 句柄关闭虽会兜底，但显式终止能让回收时机确定。
/// 只有在尚未确认回收时才补发终止：组长已退出后 pgid 可能被系统复用，再次 killpg 会误杀
/// 无关进程组。Drop 无法传播错误，进程组已消失（ESRCH）或 Job 已空都属于正常情况。
impl Drop for ManagedTokioChild {
    fn drop(&mut self) {
        if self.kill_on_drop && !self.reaped.load(Ordering::Acquire) {
            let _ = self.inner.start_kill();
        }
    }
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
            wrapped.spawn().map(|inner| ManagedTokioChild {
                inner,
                kill_on_drop: options.kill_on_drop,
                reaped: Arc::new(AtomicBool::new(false)),
            })
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
            wrapped.spawn().map(|inner| ManagedTokioChild {
                inner,
                kill_on_drop: options.kill_on_drop,
                reaped: Arc::new(AtomicBool::new(false)),
            })
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
    ///
    /// 一旦观察到退出状态就记录“已确认回收”，避免后续 Drop 对可能被复用的 pgid 再次 killpg。
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.inner.try_wait()?;
        if status.is_some() {
            self.reaped.store(true, Ordering::Release);
        }
        Ok(status)
    }

    /// 返回借用式 wait future；deadline 超时只丢 future，调用方仍持有 child 可重试清理。
    ///
    /// future 成功返回后写入“已确认回收”标志；Drop 需要看到该标志才能跳过重复终止。
    pub(crate) fn wait(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = io::Result<ExitStatus>> + Send + '_>> {
        let reaped = Arc::clone(&self.reaped);
        Box::pin(async move {
            let result = self.inner.wait().await;
            if result.is_ok() {
                reaped.store(true, Ordering::Release);
            }
            result
        })
    }

    /// Windows Job 的立即终止路径；调用方仍须在原 deadline 内 wait/reap。
    /// Unix 必须走带宽限期的进程组收尾，因此此方法只在 Windows 编译。
    #[cfg(windows)]
    pub(crate) fn start_kill_tree(&mut self) -> io::Result<()> {
        start_kill_tokio(&mut *self.inner)
    }

    /// Unix 先向进程组发送 HUP，再在 deadline 内等待原有宽限期并 KILL；Windows 直接终止 Job。
    ///
    /// 这里不能因为 `Ok(())` 就把 `reaped` 置位：`terminate_tokio_process_group` 在 SIGHUP
    /// 返回 ESRCH/EPERM 时会提前返回，尚未发送 SIGKILL；Windows 的 Job 终止也只表示请求已提交。
    /// 只有 `try_wait` 观察到 Some 或 `wait` 成功才代表直接子进程确实被回收，未确认时 Drop
    /// 仍需要按平台兜底终止，否则残留的进程组或 Job 可能继续占用管道。
    pub(crate) async fn terminate_tree_until(
        &mut self,
        deadline: tokio::time::Instant,
    ) -> io::Result<()> {
        #[cfg(unix)]
        let result = terminate_tokio_process_group(&mut *self.inner, deadline).await;
        #[cfg(windows)]
        let result = {
            let _ = deadline;
            self.start_kill_tree()
        };
        #[cfg(not(any(unix, windows)))]
        let result = {
            let _ = deadline;
            Err(unsupported_platform())
        };
        if result.is_ok() {
            // 终止路径可能已在宽限期后观察到直接子进程退出；同步“已回收”标志，避免 Drop
            // 对可能被复用的 pgid 再补发信号。确认失败时保持未回收状态，交给 wait/reap 路径。
            let _ = self.try_wait();
        }
        result
    }
}

/// Unix 上 signal 仅命中由 ProcessGroup wrapper 创建的子进程组，宽限等待不越过共享 deadline。
///
/// 宽限之后先确认直接子进程是否已退出：`ProcessGroupChild::try_wait` 只有在整个进程组
/// 都被回收（`waitpid` 返回 ECHILD）时才给出 `Some`，此时 pgid 可能已被系统复用，再发
/// SIGKILL 会误伤无关进程组。`try_wait` 自身失败时不阻断终止，仍按原路径尝试关闭进程组，
/// 由信号侧对 ESRCH/EPERM 的容错兜底。
#[cfg(unix)]
async fn terminate_tokio_process_group(
    child: &mut dyn process_wrap::tokio::ChildWrapper,
    deadline: tokio::time::Instant,
) -> io::Result<()> {
    match child.signal(libc::SIGHUP) {
        Ok(()) => {}
        Err(error) if process_group_is_gone(&error) => return Ok(()),
        Err(error) => return Err(error),
    }

    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    let grace = remaining.min(super::PROCESS_GROUP_GRACE);
    if !grace.is_zero() {
        tokio::time::sleep(grace).await;
    }
    if matches!(child.try_wait(), Ok(Some(_))) {
        return Ok(());
    }
    start_kill_tokio(child)
}

/// 判断进程组信号失败是否只表示“组已消失或不再属于我们”。
///
/// 进程组由本模块创建并拥有，正常路径不会出现 EPERM；一旦出现，说明组长退出后 pgid
/// 已被系统复用或组已脱离，继续发信号只会误伤无关进程。其它错误仍原样返回，不吞掉。
#[cfg(unix)]
fn process_group_is_gone(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ESRCH) | Some(libc::EPERM))
}

/// 对 Tokio child 统一映射 process-wrap 的进程树 kill；进程组已消失或不再属于我们时视为成功。
fn start_kill_tokio(child: &mut dyn process_wrap::tokio::ChildWrapper) -> io::Result<()> {
    match child.start_kill() {
        Ok(()) => Ok(()),
        #[cfg(unix)]
        Err(error) if process_group_is_gone(&error) => Ok(()),
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
