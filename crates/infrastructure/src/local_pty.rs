//! portable-pty 本地进程适配器；会话关闭由可重试 owner 线程持有，输入输出通过有界 bridge 背压。
//!
//! @author kongweiguang

use std::io::{self, Read, Write};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle as NativeJoinHandle};

use async_trait::async_trait;
use kerminal_application::sessions::{
    SessionOpenCleanupOwner, SessionTransport, SessionTransportControl, SessionTransportFactory,
    SessionTransportOpenError, TerminalResizeCapability,
};
use kerminal_domain::connections::RemoteShellDialect;
use kerminal_domain::sessions::{
    LocalSessionTarget, SessionExitStatus, SessionTarget, TerminalSize,
};
use portable_pty::{Child, ChildKiller, MasterPty, PtySize, native_pty_system};
use tokio::io::duplex;
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::io::SyncIoBridge;
use tokio_util::sync::CancellationToken;

mod reader;
use reader::{PtyReaderStats, output_reader_channel, spawn_pty_reader};

#[cfg(windows)]
mod windows_launcher;

#[cfg(windows)]
use super::process_tree::PreparedWindowsJob;
use super::process_tree::ProcessTreeGuard;
use kerminal_application::{AppError, AppResult};

const PIPE_BUFFER_BYTES: usize = 64 * 1024;
const RESIZE_DIAGNOSTICS_ENV: &str = "KERMINAL_TERMINAL_RESIZE_DIAGNOSTICS";
static RESIZE_DIAGNOSTICS_ENABLED: OnceLock<bool> = OnceLock::new();

/// 只在明确 opt-in 时启用尺寸与光标坐标诊断；env 在首次检查时缓存，须在启动候选前设置。
/// 默认输出读取路径不检查终端字节或产生日志。
///
/// @author kongweiguang
pub(crate) fn resize_diagnostics_enabled() -> bool {
    *RESIZE_DIAGNOSTICS_ENABLED
        .get_or_init(|| std::env::var_os(RESIZE_DIAGNOSTICS_ENV).is_some_and(|value| value == "1"))
}

/// 记录一条脱敏的原生 PTY resize 阶段；字段均为白名单数值，不包含错误正文或进程输出。
///
/// @author kongweiguang
fn log_native_resize_diagnostic(
    process_id: u32,
    call_sequence: u64,
    size: TerminalSize,
    phase: u64,
    elapsed_ms: u64,
) {
    tracing::info!(
        event = "TERMINAL_RESIZE_DIAGNOSTIC",
        phase,
        native_process_id = process_id,
        native_resize_call_sequence = call_sequence,
        new_columns = u64::from(size.columns()),
        new_rows = u64::from(size.rows()),
        new_pixel_width = u64::from(size.pixel_width()),
        new_pixel_height = u64::from(size.pixel_height()),
        elapsed_ms,
    );
}

/// 本地目标的 `SessionTransportFactory`；协议 provider 由 composition root 组合。
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalPtySessionTransportFactory;

impl LocalPtySessionTransportFactory {
    /// 创建无状态 provider；实际程序、参数、环境和目录始终来自显式 Local target。
    ///
    /// @author kongweiguang
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SessionTransportFactory for LocalPtySessionTransportFactory {
    /// 在后台打开本地 PTY，并用有界 duplex 将同步 PTY 流接入唯一 session runtime。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        target: &SessionTarget,
        size: TerminalSize,
        cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        let SessionTarget::Local(target) = target else {
            return Err(AppError::unsupported("本地 PTY provider 不支持此会话目标").into());
        };
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("本地终端创建已取消").into());
        }

        let (
            master,
            child,
            killer,
            process_tree,
            pty_reader,
            pty_writer,
            shell_dialect,
            process_id,
            shell_integration_enabled,
        ) = spawn_local_process(target.clone(), size, cancel.clone()).await?;
        let (exit_sender, exit_receiver) = watch::channel(None);
        let closing = Arc::new(AtomicBool::new(false));
        let child_exited = Arc::new(AtomicBool::new(false));

        let monitor = spawn_child_monitor(child, exit_sender, Arc::clone(&child_exited));
        let (reader_stream, reader_sender) = output_reader_channel();
        let reader = spawn_pty_reader(
            pty_reader,
            reader_sender,
            Arc::clone(&closing),
            Arc::clone(&child_exited),
        );
        let (writer_bridge, writer_stream) = duplex(PIPE_BUFFER_BYTES);
        let writer = spawn_pty_writer(
            writer_stream,
            pty_writer,
            Arc::clone(&closing),
            Arc::clone(&child_exited),
        );

        let control = LocalPtyControl {
            master: Arc::new(Mutex::new(Some(master))),
            process_id,
            resize_call_sequence: AtomicU64::new(0),
            killer,
            process_tree,
            exit_receiver,
            close_worker: AsyncMutex::new(None),
            release_worker: AsyncMutex::new(None),
            release_worker_lost: AtomicBool::new(false),
            release_completed: AtomicBool::new(false),
            release_unsupported: AtomicBool::new(false),
            natural_exit_deadline: Mutex::new(None),
            drain_worker: AsyncMutex::new(None),
            drain_worker_lost: AtomicBool::new(false),
            workers: AsyncMutex::new(PtyWorkers {
                monitor: Some(monitor),
                reader: Some(reader),
                writer: Some(writer),
            }),
            closing,
        };
        let transport = SessionTransport::new(reader_stream, writer_bridge, control)
            .with_shell_dialect(shell_dialect)
            .with_process_id(process_id)
            .with_shell_integration_enabled(shell_integration_enabled);
        if cancel.is_cancelled() {
            return Err(SessionTransportOpenError::with_cleanup(
                AppError::cancelled("本地终端创建已取消"),
                Arc::new(TransportOpenCleanupOwner {
                    control: Arc::clone(&transport.control),
                }),
            ));
        }
        Ok(transport)
    }
}

/// 取消发生在本地 transport 完整组装后时保留控制句柄，直到 close 与 child wait 完成。
///
/// @author kongweiguang
struct TransportOpenCleanupOwner {
    control: Arc<dyn SessionTransportControl>,
}

#[async_trait]
impl SessionOpenCleanupOwner for TransportOpenCleanupOwner {
    /// 继续使用同一个 deadline 关闭已组装 transport，并等待 monitor/reader/writer 回收。
    ///
    /// @author kongweiguang
    async fn close_until(&self, deadline: Instant) -> AppResult<()> {
        self.control.close_until(deadline).await?;
        tokio::time::timeout_at(deadline, self.control.wait_exit())
            .await
            .map_err(|_| AppError::timeout("等待已取消的本地 PTY 退出"))?
            .map(|_| ())
    }
}

/// 启动命令、shell integration bootstrap 与 PTY 流；失败时结束并回收已创建进程。
///
/// @author kongweiguang
async fn spawn_local_process(
    target: LocalSessionTarget,
    size: TerminalSize,
    cancellation: CancellationToken,
) -> Result<
    (
        Box<dyn MasterPty + Send>,
        Box<dyn Child + Send + Sync>,
        Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
        Arc<ProcessTreeGuard>,
        Box<dyn Read + Send>,
        Box<dyn Write + Send>,
        RemoteShellDialect,
        Option<u32>,
        bool,
    ),
    SessionTransportOpenError,
> {
    tokio::task::spawn_blocking(move || {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("本地终端创建已取消").into());
        }
        let startup = super::shell_integration::prepare_local_shell(&target)?;
        let shell_integration_enabled = startup.bootstrap_status
            == super::shell_integration::ShellIntegrationBootstrapStatus::Prepared;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("本地终端创建已取消").into());
        }
        let pair = native_pty_system()
            .openpty(pty_size(size))
            .map_err(|error| pty_error("创建本地 PTY 失败", error))?;
        #[cfg(windows)]
        let (child, process_tree) = {
            let prepared_job = PreparedWindowsJob::new().map_err(|error| {
                AppError::infrastructure_with_source("准备本地 PTY 进程树 Job 失败", error)
            })?;
            let command = command_builder(&target, &startup.args, &startup.env, startup.dialect);
            let child = match prepared_job.spawn_command(pair.slave.as_ref(), command) {
                Ok(child) => child,
                Err(error) => return Err(pty_error("启动本地终端进程失败", error).into()),
            };
            let process_tree = Arc::new(prepared_job.into_guard(startup.lifetime_guard));
            (child, process_tree)
        };
        #[cfg(not(windows))]
        let (child, process_tree) = {
            let child = match pair.slave.spawn_command(command_builder(
                &target,
                &startup.args,
                &startup.env,
                startup.dialect,
            )) {
                Ok(child) => child,
                Err(error) => return Err(pty_error("启动本地终端进程失败", error).into()),
            };
            let process_tree = match ProcessTreeGuard::from_pty(
                pair.master.as_ref(),
                child.as_ref(),
                startup.lifetime_guard,
            ) {
                Ok(process_tree) => Arc::new(process_tree),
                Err((error, lifetime_guard)) => {
                    let primary =
                        AppError::infrastructure_with_source("建立本地进程树清理句柄失败", error);
                    return Err(cleanup_after_spawn_failure(
                        primary,
                        None,
                        lifetime_guard,
                        pair.master,
                        child,
                    ));
                }
            };
            (child, process_tree)
        };
        let process_id = child.process_id();
        drop(pair.slave);
        if cancellation.is_cancelled() {
            return Err(cleanup_after_spawn_failure(
                AppError::cancelled("本地终端创建已取消"),
                Some(Arc::clone(&process_tree)),
                None,
                pair.master,
                child,
            ));
        }

        let reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(error) => {
                return Err(cleanup_after_spawn_failure(
                    pty_error("打开本地 PTY 输出失败", error),
                    Some(Arc::clone(&process_tree)),
                    None,
                    pair.master,
                    child,
                ));
            }
        };
        if cancellation.is_cancelled() {
            return Err(cleanup_after_spawn_failure(
                AppError::cancelled("本地终端创建已取消"),
                Some(Arc::clone(&process_tree)),
                None,
                pair.master,
                child,
            ));
        }
        let writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(error) => {
                return Err(cleanup_after_spawn_failure(
                    pty_error("打开本地 PTY 输入失败", error),
                    Some(Arc::clone(&process_tree)),
                    None,
                    pair.master,
                    child,
                ));
            }
        };
        if cancellation.is_cancelled() {
            return Err(cleanup_after_spawn_failure(
                AppError::cancelled("本地终端创建已取消"),
                Some(Arc::clone(&process_tree)),
                None,
                pair.master,
                child,
            ));
        }
        let killer = Arc::new(Mutex::new(child.clone_killer()));
        Ok((
            pair.master,
            child,
            killer,
            process_tree,
            reader,
            writer,
            startup.dialect,
            process_id,
            shell_integration_enabled,
        ))
    })
    .await
    .map_err(|error| AppError::infrastructure_with_source("本地 PTY worker 失败", error))?
}

/// 把 PTY spawn 半成品与主错误交给 manager，避免同步 cleanup 失败后丢失进程句柄。
///
/// @author kongweiguang
fn cleanup_after_spawn_failure(
    primary: AppError,
    process_tree: Option<Arc<ProcessTreeGuard>>,
    lifetime_guard: Option<Box<dyn Send + Sync + 'static>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
) -> SessionTransportOpenError {
    let cleanup_owner = Arc::new(LocalSpawnCleanupOwner {
        state: AsyncMutex::new(Some(LocalSpawnCleanupState {
            process_tree,
            lifetime_guard,
            master: Some(master),
            child: Some(child),
        })),
        worker: AsyncMutex::new(None),
        cleanup_owner_lost: AtomicBool::new(false),
    });
    tracing::error!(
        operation = primary.public_code().as_str(),
        "local PTY setup failed; cleanup is transferred to SessionManager"
    );
    SessionTransportOpenError::with_cleanup(primary, cleanup_owner)
}

/// 轮询专用原生线程的完成状态；deadline 到期时保留 JoinHandle，避免把尚在关闭的资源当成已回收。
///
/// @author kongweiguang
async fn join_native_worker_until<T>(
    worker: &mut Option<NativeJoinHandle<T>>,
    deadline: Instant,
    timeout_context: &'static str,
    panic_context: &'static str,
) -> AppResult<T> {
    loop {
        let Some(handle) = worker.as_ref() else {
            return Err(AppError::conflict("本地 PTY 原生 worker 已被取走"));
        };
        if handle.is_finished() {
            let Some(handle) = worker.take() else {
                return Err(AppError::conflict("本地 PTY 原生 worker 已被取走"));
            };
            return handle.join().map_err(|_| {
                AppError::infrastructure_with_source(
                    panic_context,
                    io::Error::other("本地 PTY 原生 worker panic"),
                )
            });
        }

        tokio::time::timeout_at(
            deadline,
            tokio::time::sleep(std::time::Duration::from_millis(4)),
        )
        .await
        .map_err(|_| AppError::timeout(timeout_context))?;
    }
}

/// 在 manager 的绝对 deadline 内回收 PTY spawn 半成品，并保留超时 worker 供后续重试。
///
/// @author kongweiguang
struct LocalSpawnCleanupOwner {
    state: AsyncMutex<Option<LocalSpawnCleanupState>>,
    worker: AsyncMutex<Option<NativeJoinHandle<io::Result<LocalSpawnCleanupResult>>>>,
    cleanup_owner_lost: AtomicBool,
}

struct LocalSpawnCleanupState {
    process_tree: Option<Arc<ProcessTreeGuard>>,
    lifetime_guard: Option<Box<dyn Send + Sync + 'static>>,
    master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
}

struct LocalSpawnCleanupResult {
    state: LocalSpawnCleanupState,
    failure: Option<SpawnCleanupFailure>,
}

impl LocalSpawnCleanupState {
    /// 判断所有原生句柄与启动守卫是否已回收，供重复 close 安全返回。
    ///
    /// @author kongweiguang
    fn is_empty(&self) -> bool {
        self.process_tree.is_none()
            && self.lifetime_guard.is_none()
            && self.master.is_none()
            && self.child.is_none()
    }

    /// 只在专用原生线程终止进程树、等待直接 child 并释放 PTY master。
    ///
    /// @author kongweiguang
    fn cleanup_once(mut self) -> LocalSpawnCleanupResult {
        let process_tree_error = self
            .process_tree
            .as_ref()
            .and_then(|tree| tree.terminate().err());
        if process_tree_error.is_none() {
            self.process_tree.take();
        }
        let child_kill_error = self.child.as_mut().and_then(|child| child.kill().err());
        let child_wait_error = self.child.as_mut().and_then(|child| child.wait().err());
        if child_wait_error.is_none() {
            self.child.take();
        }
        // ConPTY reader 尚未启动；释放 master 可解除系统端占用，child/tree 句柄仍留在 state。
        self.master.take();
        if self.process_tree.is_none() && self.child.is_none() {
            self.lifetime_guard.take();
        }

        // wait 成功证明直接 child 已回收；若树清理失败，仍保留 tree guard 供下次重试。
        let child_kill_error = child_wait_error.as_ref().and(child_kill_error);
        let failure = if process_tree_error.is_none()
            && child_kill_error.is_none()
            && child_wait_error.is_none()
        {
            None
        } else {
            Some(SpawnCleanupFailure {
                process_tree: process_tree_error,
                child_kill: child_kill_error,
                child_wait: child_wait_error,
            })
        };
        LocalSpawnCleanupResult {
            state: self,
            failure,
        }
    }
}

#[async_trait]
impl SessionOpenCleanupOwner for LocalSpawnCleanupOwner {
    /// 每次最多运行一个专用原生 cleanup 线程；deadline 到期后保留线程与剩余 OS 句柄。
    ///
    /// @author kongweiguang
    async fn close_until(&self, deadline: Instant) -> AppResult<()> {
        if self.cleanup_owner_lost.load(Ordering::Acquire) {
            return Err(AppError::infrastructure(
                "本地 PTY spawn cleanup worker 异常退出，资源回收状态未知",
            ));
        }
        let mut worker = tokio::time::timeout_at(deadline, self.worker.lock())
            .await
            .map_err(|_| AppError::timeout("等待本地 PTY spawn cleanup worker 锁"))?;
        if worker.is_none() {
            let mut state_guard = tokio::time::timeout_at(deadline, self.state.lock())
                .await
                .map_err(|_| AppError::timeout("等待本地 PTY spawn cleanup 状态锁"))?;
            if state_guard
                .as_ref()
                .is_some_and(LocalSpawnCleanupState::is_empty)
            {
                return Ok(());
            }
            let Some(state) = state_guard.take() else {
                return Err(AppError::infrastructure(
                    "本地 PTY spawn cleanup 缺少资源 owner，回收状态未知",
                ));
            };
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            let native_worker = match thread::Builder::new()
                .name("kerminal-pty-open-cleanup".to_owned())
                .spawn(move || {
                    let state: LocalSpawnCleanupState = receiver.recv().map_err(|_| {
                        io::Error::other("本地 PTY cleanup worker 未接收到资源 owner")
                    })?;
                    Ok(state.cleanup_once())
                }) {
                Ok(worker) => worker,
                Err(error) => {
                    *state_guard = Some(state);
                    return Err(AppError::infrastructure_with_source(
                        "启动本地 PTY spawn cleanup worker 失败",
                        error,
                    ));
                }
            };
            if let Err(send_error) = sender.send(state) {
                *state_guard = Some(send_error.0);
                *worker = Some(native_worker);
                return Err(AppError::infrastructure(
                    "本地 PTY cleanup worker 未接收资源 owner",
                ));
            }
            *worker = Some(native_worker);
        }

        let joined = match join_native_worker_until(
            &mut worker,
            deadline,
            "回收本地 PTY spawn 进程树",
            "本地 PTY spawn cleanup worker panic",
        )
        .await
        {
            Ok(joined) => joined,
            Err(error) => {
                if worker.is_none() {
                    self.cleanup_owner_lost.store(true, Ordering::Release);
                }
                return Err(error);
            }
        };
        let result = joined.map_err(|error| {
            AppError::infrastructure_with_source("本地 PTY spawn cleanup worker 失败", error)
        })?;
        let cleanup_error = result.failure.map(|failure| {
            AppError::infrastructure_with_source("本地 PTY spawn 资源尚未完全回收", failure)
        });
        *self.state.lock().await = Some(result.state);
        drop(worker);
        cleanup_error.map_or(Ok(()), Err)
    }
}

/// 保留进程树终止与直接 child 回收错误类别，供 manager 和诊断日志检查。
///
/// @author kongweiguang
#[derive(Debug)]
struct SpawnCleanupFailure {
    process_tree: Option<io::Error>,
    child_kill: Option<io::Error>,
    child_wait: Option<io::Error>,
}

impl std::fmt::Display for SpawnCleanupFailure {
    /// 只展示 OS 错误类别，不输出本地路径或命令参数。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "tree={:?}, kill={:?}, wait={:?}",
            self.process_tree.as_ref().map(io::Error::kind),
            self.child_kill.as_ref().map(io::Error::kind),
            self.child_wait.as_ref().map(io::Error::kind),
        )
    }
}

impl std::error::Error for SpawnCleanupFailure {
    /// 以首个存在的清理错误作为标准 source，其余类别保存在该结构中。
    ///
    /// @author kongweiguang
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.process_tree
            .as_ref()
            .or(self.child_kill.as_ref())
            .or(self.child_wait.as_ref())
            .map(|error| error as &(dyn std::error::Error + 'static))
    }
}

/// 把 portable-pty 的 anyhow 根因包装成标准错误，供安全应用错误保留 cause 链。
///
/// @author kongweiguang
fn pty_error(context: &'static str, error: anyhow::Error) -> AppError {
    AppError::infrastructure_with_source(context, PtyErrorSource(error))
}

/// portable-pty anyhow 错误的最小标准包装，公开文案不携带路径或命令细节。
struct PtyErrorSource(anyhow::Error);

impl std::fmt::Display for PtyErrorSource {
    /// 用固定内部诊断描述包裹的 PTY 故障，不把其动态上下文投影到安全文案。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("portable-pty operation failed")
    }
}

impl std::fmt::Debug for PtyErrorSource {
    /// 仅记录来源类型，避免底层错误格式化泄露命令行或文件路径。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PtyErrorSource")
    }
}

impl std::error::Error for PtyErrorSource {
    /// 尽量保留 anyhow 中最底层的 OS error，便于内部错误分类与诊断。
    ///
    /// @author kongweiguang
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

/// 把领域网格转换成 ConPTY 或 Unix PTY 的系统尺寸结构。
///
/// @author kongweiguang
fn pty_size(size: TerminalSize) -> PtySize {
    PtySize {
        rows: size.rows(),
        cols: size.columns(),
        pixel_width: size.pixel_width(),
        pixel_height: size.pixel_height(),
    }
}

/// 先固定创建进程时的 PATH/PATHEXT，再按 target 与 shell 启动覆盖顺序设置环境。
///
/// portable-pty 在 Windows 会合并注册表环境，可能覆盖用于解析裸程序名的父进程搜索策略；
/// 显式写入当前 PATH/PATHEXT 可让解析顺序与调用方一致，同时仍允许目标配置和启动层覆盖。
///
/// @author kongweiguang
fn command_builder(
    target: &LocalSessionTarget,
    startup_args: &[std::ffi::OsString],
    startup_env: &std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    shell_dialect: RemoteShellDialect,
) -> portable_pty::CommandBuilder {
    let mut command = portable_pty::CommandBuilder::new(&target.program);
    command.args(startup_args);
    if let Some(directory) = &target.working_directory {
        command.cwd(directory);
    }
    #[cfg(windows)]
    windows_launcher::set_command_search_environment(
        &mut command,
        std::env::var_os("PATH"),
        std::env::var_os("PATHEXT"),
    );
    #[cfg(not(windows))]
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    for (name, value) in &target.environment {
        command.env(name, value);
    }
    for (name, value) in startup_env {
        command.env(name, value);
    }
    if !has_explicit_environment_override(target, startup_env, "TERM") {
        // Finder 等 GUI 启动通常不继承终端 TERM；真实 PTY 明确提供本引擎支持的 xterm 协议。
        command.env("TERM", "xterm-256color");
    }
    #[cfg(unix)]
    if shell_dialect != RemoteShellDialect::Unknown
        && !has_explicit_environment_override(target, startup_env, "SHELL")
    {
        // 只对已识别的 Unix 交互 shell 导出 SHELL，避免把自定义命令误标成 shell。
        command.env("SHELL", &target.program);
    }
    #[cfg(windows)]
    let _ = shell_dialect;
    command
}

/// 判断 Profile 或 shell integration 是否明确设置了环境键，保留用户覆盖的大小写语义。
///
/// @author kongweiguang
fn has_explicit_environment_override(
    target: &LocalSessionTarget,
    startup_env: &std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    expected: &str,
) -> bool {
    target
        .environment
        .keys()
        .chain(startup_env.keys())
        .any(|key| environment_key_matches(key, expected))
}

/// 按操作系统的环境键规则匹配，避免 Windows 上不同大小写绕过显式用户设置。
///
/// @author kongweiguang
fn environment_key_matches(key: &std::ffi::OsStr, expected: &str) -> bool {
    #[cfg(windows)]
    {
        key.to_str()
            .is_some_and(|key| key.eq_ignore_ascii_case(expected))
    }
    #[cfg(not(windows))]
    {
        key == std::ffi::OsStr::new(expected)
    }
}

/// 把退出状态从阻塞 wait 线程送入可 clone watch，并保留 JoinHandle 供关闭时收尾。
///
/// @author kongweiguang
fn spawn_child_monitor(
    mut child: Box<dyn Child + Send + Sync>,
    exit_sender: watch::Sender<Option<SessionExitStatus>>,
    child_exited: Arc<AtomicBool>,
) -> JoinHandle<io::Result<SessionExitStatus>> {
    tokio::task::spawn_blocking(move || match child.wait() {
        Ok(status) => {
            child_exited.store(true, Ordering::Release);
            let result = SessionExitStatus {
                code: i32::try_from(status.exit_code()).ok(),
                signal: status.signal().map(str::to_owned),
                success: status.success(),
            };
            let _ = exit_sender.send(Some(result.clone()));
            Ok(result)
        }
        Err(error) => Err(error),
    })
}

/// 判断 PTY 断流是否发生在已关闭会话的正常尾部，避免吞掉运行中真实 I/O 错误。
///
/// @author kongweiguang
fn expected_pty_end(error: &io::Error, closing: &AtomicBool, child_exited: &AtomicBool) -> bool {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::EIO) {
        return true;
    }
    if !(closing.load(Ordering::Acquire) || child_exited.load(Ordering::Acquire)) {
        return false;
    }
    if matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof
    ) {
        return true;
    }
    false
}

/// 在阻塞池把 Tokio writer 接到系统 PTY writer；duplex 容量提供背压，worker 错误留待资源回收报告。
///
/// @author kongweiguang
fn spawn_pty_writer(
    bridge: tokio::io::DuplexStream,
    pty_writer: Box<dyn Write + Send>,
    closing: Arc<AtomicBool>,
    child_exited: Arc<AtomicBool>,
) -> JoinHandle<io::Result<u64>> {
    tokio::task::spawn_blocking(move || {
        let mut bridge = SyncIoBridge::new(bridge);
        let mut writer = pty_writer;
        let copied = match io::copy(&mut bridge, &mut writer) {
            Ok(bytes) => bytes,
            Err(error) if expected_pty_end(&error, &closing, &child_exited) => 0,
            Err(error) => return Err(error),
        };
        match writer.flush() {
            Ok(()) => Ok(copied),
            Err(error) if expected_pty_end(&error, &closing, &child_exited) => Ok(copied),
            Err(error) => Err(error),
        }
    })
}

/// 保留所有阻塞池 worker；wait_exit 才消耗句柄，重复等待不会丢失尚未完成的回收。
struct PtyWorkers {
    monitor: Option<JoinHandle<io::Result<SessionExitStatus>>>,
    reader: Option<JoinHandle<io::Result<PtyReaderStats>>>,
    writer: Option<JoinHandle<io::Result<u64>>>,
}

/// 同时保留进程树终止和直系子进程兜底失败，供上层安全错误链诊断。
///
/// @author kongweiguang
#[derive(Debug)]
struct ProcessTreeTerminationFailure {
    process_tree: io::Error,
    fallback: Option<io::Error>,
}

impl std::fmt::Display for ProcessTreeTerminationFailure {
    /// 将两种清理失败都写入内部错误链，避免兜底失败被悄悄丢弃。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "进程树终止失败：{}", self.process_tree)?;
        if let Some(fallback) = &self.fallback {
            write!(formatter, "；直系子进程兜底终止也失败：{fallback}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ProcessTreeTerminationFailure {
    /// 保留进程树终止作为标准错误源；兜底错误由本错误对象持有并在诊断文本中呈现。
    ///
    /// @author kongweiguang
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.process_tree)
    }
}

/// 控制本地 PTY 尺寸和进程树，并在退出等待时 join 所有 PTY I/O worker。
struct LocalPtyControl {
    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    process_id: Option<u32>,
    resize_call_sequence: AtomicU64,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    process_tree: Arc<ProcessTreeGuard>,
    exit_receiver: watch::Receiver<Option<SessionExitStatus>>,
    close_worker: AsyncMutex<Option<NativeJoinHandle<io::Result<()>>>>,
    release_worker: AsyncMutex<Option<NativeJoinHandle<io::Result<bool>>>>,
    release_worker_lost: AtomicBool,
    release_completed: AtomicBool,
    release_unsupported: AtomicBool,
    natural_exit_deadline: Mutex<Option<Instant>>,
    drain_worker: AsyncMutex<Option<NativeJoinHandle<io::Result<()>>>>,
    drain_worker_lost: AtomicBool,
    workers: AsyncMutex<PtyWorkers>,
    closing: Arc<AtomicBool>,
}

#[async_trait]
impl SessionTransportControl for LocalPtyControl {
    /// 只在 Windows 的本地 native PTY 上声明 ConPTY；Unix local PTY 沿用 generic 策略。
    ///
    /// @author kongweiguang
    fn terminal_resize_capability(&self) -> TerminalResizeCapability {
        if cfg!(windows) {
            TerminalResizeCapability::WindowsConPty
        } else {
            TerminalResizeCapability::Generic
        }
    }

    /// 把原生 resize 放到阻塞池；opt-in 时仅记录 PTY PID、尺寸、阶段与耗时，默认不触碰日志热路径。
    ///
    /// @author kongweiguang
    async fn resize(&self, size: TerminalSize) -> AppResult<()> {
        if self.closing.load(Ordering::Acquire) {
            return Err(AppError::conflict("本地 PTY 正在关闭，不能调整尺寸"));
        }
        let diagnostics_enabled = resize_diagnostics_enabled();
        let process_id = self.process_id.unwrap_or_default();
        let call_sequence = diagnostics_enabled.then(|| {
            self.resize_call_sequence
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1)
        });
        let master = Arc::clone(&self.master);
        let closing = Arc::clone(&self.closing);
        tokio::task::spawn_blocking(move || {
            let mut master = master
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if closing.load(Ordering::Acquire) {
                return Err(anyhow::anyhow!("PTY is closing"));
            }
            let master = master
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("PTY master is already closed"))?;
            if let Some(call_sequence) = call_sequence {
                // phase 8=真正进入 MasterPty::resize，9=原生 ACK，10=原生错误；不记动态错误正文。
                log_native_resize_diagnostic(process_id, call_sequence, size, 8, 0);
            }
            let started = std::time::Instant::now();
            let result = master.resize(pty_size(size));
            if let Some(call_sequence) = call_sequence {
                let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                log_native_resize_diagnostic(
                    process_id,
                    call_sequence,
                    size,
                    if result.is_ok() { 9 } else { 10 },
                    elapsed_ms,
                );
            }
            result
        })
        .await
        .map_err(|error| {
            AppError::infrastructure_with_source("本地 PTY resize worker 失败", error)
        })?
        .map_err(|error| pty_error("调整本地 PTY 尺寸失败", error))
    }

    /// 等待本地 monitor 的原生 child 状态，不把输出 EOF 或 worker 回收混成同一事件。
    ///
    /// @author kongweiguang
    async fn observe_child_exit(&self) -> AppResult<Option<SessionExitStatus>> {
        let mut receiver = self.exit_receiver.clone();
        loop {
            if let Some(status) = receiver.borrow().clone() {
                return Ok(Some(status));
            }
            receiver.changed().await.map_err(|_| {
                AppError::infrastructure("本地子进程退出观察 worker 未能发布真实状态")
            })?;
        }
    }

    /// 在原生 child 退出后调用可选的非破坏性 PTY release，并以首次观察到的绝对 deadline 限制等待。
    ///
    /// 不关闭输入 writer；其 native handle 仍需活到 reader drain 完成，避免对后代发送关闭事件。
    ///
    /// @author kongweiguang
    async fn release_after_child_exit_until(&self, deadline: Instant) -> AppResult<()> {
        if self.exit_receiver.borrow().is_none() {
            return Err(AppError::conflict(
                "尚未确认本地子进程退出，不能释放 PTY client 引用",
            ));
        }
        if self.release_completed.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.release_unsupported.load(Ordering::Acquire) {
            return Err(AppError::unsupported(
                "当前 ConPTY backend 不支持自然退出后的非破坏性 release",
            ));
        }
        if self.closing.load(Ordering::Acquire) {
            return Err(AppError::conflict("本地 PTY 已进入显式关闭流程"));
        }
        if self.release_worker_lost.load(Ordering::Acquire) {
            return Err(AppError::infrastructure(
                "本地 PTY release worker 已异常退出，资源状态未知",
            ));
        }
        {
            let mut natural_deadline = self
                .natural_exit_deadline
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            natural_deadline.get_or_insert(deadline);
        }

        let mut worker = tokio::time::timeout_at(deadline, self.release_worker.lock())
            .await
            .map_err(|_| AppError::timeout("等待本地 PTY release owner 锁"))?;
        if worker.is_none() {
            let master = Arc::clone(&self.master);
            let closing = Arc::clone(&self.closing);
            let native_worker = thread::Builder::new()
                .name("kerminal-pty-natural-release".to_owned())
                .spawn(move || {
                    if closing.load(Ordering::Acquire) {
                        return Ok(false);
                    }
                    let mut master = master
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if closing.load(Ordering::Acquire) {
                        return Ok(false);
                    }
                    master
                        .as_mut()
                        .ok_or_else(|| io::Error::other("PTY master is no longer owned"))?
                        .release_after_child_exit()
                        .map_err(io::Error::other)
                })
                .map_err(|error| {
                    AppError::infrastructure_with_source(
                        "启动本地 PTY 自然 release worker 失败",
                        error,
                    )
                })?;
            *worker = Some(native_worker);
        }

        match join_native_worker_until(
            &mut worker,
            deadline,
            "等待本地 PTY 自然 release",
            "本地 PTY 自然 release worker panic",
        )
        .await
        {
            Ok(Ok(true)) => {
                self.release_completed.store(true, Ordering::Release);
                Ok(())
            }
            Ok(Ok(false)) if self.closing.load(Ordering::Acquire) => {
                Err(AppError::conflict("本地 PTY 已进入显式关闭流程"))
            }
            Ok(Ok(false)) => {
                self.release_unsupported.store(true, Ordering::Release);
                Err(AppError::unsupported(
                    "当前 ConPTY backend 不支持自然退出后的非破坏性 release",
                ))
            }
            Ok(Err(error)) => Err(AppError::infrastructure_with_source(
                "本地 PTY 自然 release 失败",
                error,
            )),
            Err(error) => {
                if worker.is_none() {
                    self.release_worker_lost.store(true, Ordering::Release);
                }
                Err(error)
            }
        }
    }

    /// 显式关闭时先终止子进程树以解除可能阻塞的 Release，再 join Release 并最终释放 master。
    ///
    /// @author kongweiguang
    async fn close_until(&self, deadline: Instant) -> AppResult<()> {
        self.closing.store(true, Ordering::Release);
        let mut drain_worker = tokio::time::timeout_at(deadline, self.drain_worker.lock())
            .await
            .map_err(|_| AppError::timeout("等待本地 PTY drain owner 锁"))?;
        if drain_worker.is_some() {
            match join_native_worker_until(
                &mut drain_worker,
                deadline,
                "等待自然 drain owner 后开始显式关闭",
                "本地 PTY 自然 drain worker panic",
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    return Err(AppError::infrastructure_with_source(
                        "本地 PTY 自然 drain worker 失败",
                        error,
                    ));
                }
                Err(error) if drain_worker.is_some() => return Err(error),
                Err(_) => self.drain_worker_lost.store(true, Ordering::Release),
            }
        }
        drop(drain_worker);
        let mut close_worker = tokio::time::timeout_at(deadline, self.close_worker.lock())
            .await
            .map_err(|_| AppError::timeout("等待本地 PTY 关闭任务锁"))?;
        if close_worker.is_none() {
            let tree = Arc::clone(&self.process_tree);
            let fallback = Arc::clone(&self.killer);
            let native_worker = thread::Builder::new()
                .name("kerminal-pty-close-tree".to_owned())
                .spawn(move || match tree.terminate() {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        let mut killer = fallback
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        let fallback_error = killer.kill().err();
                        Err(io::Error::other(ProcessTreeTerminationFailure {
                            process_tree: error,
                            fallback: fallback_error,
                        }))
                    }
                })
                .map_err(|error| {
                    AppError::infrastructure_with_source(
                        "启动本地 PTY 进程树终止 worker 失败",
                        error,
                    )
                })?;
            *close_worker = Some(native_worker);
        }
        let result = match join_native_worker_until(
            &mut close_worker,
            deadline,
            "终止本地终端进程树",
            "本地进程树关闭 worker panic",
        )
        .await
        {
            Ok(result) => result,
            Err(error) => return Err(error),
        };
        result.map_err(|error| {
            AppError::infrastructure_with_source("终止本地终端进程树失败", error)
        })?;
        drop(close_worker);

        // Release 可能等待尚存 client 断开；先结束显式关闭目标进程树，再用同一 deadline join 原 owner。
        let mut release_worker = tokio::time::timeout_at(deadline, self.release_worker.lock())
            .await
            .map_err(|_| AppError::timeout("等待本地 PTY release owner 锁"))?;
        if release_worker.is_some() {
            match join_native_worker_until(
                &mut release_worker,
                deadline,
                "等待已结束 client 的自然 release owner",
                "本地 PTY 自然 release worker panic",
            )
            .await
            {
                Ok(Ok(true)) => self.release_completed.store(true, Ordering::Release),
                Ok(Ok(false)) => self.release_unsupported.store(true, Ordering::Release),
                Ok(Err(error)) => {
                    return Err(AppError::infrastructure_with_source(
                        "本地 PTY 自然 release owner 失败",
                        error,
                    ));
                }
                Err(error) => return Err(error),
            }
        }
        drop(release_worker);

        // ClosePseudoConsole 只在树已终止且 release owner 已完成后执行；独立 reader 仍负责 drain。
        let mut drain_worker = tokio::time::timeout_at(deadline, self.drain_worker.lock())
            .await
            .map_err(|_| AppError::timeout("等待本地 PTY master drop owner 锁"))?;
        if drain_worker.is_none() {
            let master = Arc::clone(&self.master);
            let native_worker = thread::Builder::new()
                .name("kerminal-pty-master-drop".to_owned())
                .spawn(move || {
                    let master = master
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .take();
                    drop(master);
                    Ok(())
                })
                .map_err(|error| {
                    AppError::infrastructure_with_source(
                        "启动本地 PTY master drop worker 失败",
                        error,
                    )
                })?;
            *drain_worker = Some(native_worker);
        }
        match join_native_worker_until(
            &mut drain_worker,
            deadline,
            "释放本地 PTY master",
            "本地 PTY master drop worker panic",
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(AppError::infrastructure_with_source(
                "释放本地 PTY master 失败",
                error,
            )),
            Err(error) => Err(error),
        }
    }

    /// 等待真实退出状态后回收 monitor/read/write worker，且 receiver clone 不跨 await 持锁。
    ///
    /// @author kongweiguang
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        let mut receiver = self.exit_receiver.clone();
        tracing::trace!(
            stage = "child_status",
            "waiting for local PTY child exit notification"
        );
        let status = loop {
            if let Some(status) = receiver.borrow().clone() {
                break status;
            }
            if receiver.changed().await.is_err() {
                break SessionExitStatus {
                    code: None,
                    signal: None,
                    success: false,
                };
            }
        };

        tracing::trace!(
            stage = "monitor_join",
            "local PTY child status arrived; joining workers"
        );
        let mut workers = self.workers.lock().await;
        let mut first_error = None;
        tracing::trace!(worker = "monitor", "joining local PTY blocking worker");
        let monitor_result = match workers.monitor.as_mut() {
            Some(monitor) => Some(monitor.await),
            None => None,
        };
        if let Some(result) = monitor_result {
            workers.monitor.take();
            match result {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    first_error.get_or_insert(error);
                }
                Err(error) => {
                    first_error.get_or_insert_with(|| io::Error::other(error.to_string()));
                }
            }
        }
        tracing::trace!(worker = "reader", "joining local PTY blocking worker");
        let reader_result = match workers.reader.as_mut() {
            Some(reader) => Some(reader.await),
            None => None,
        };
        if let Some(result) = reader_result {
            workers.reader.take();
            match result {
                Ok(Ok(stats)) => {
                    if stats.end_tail_discarded_bytes > 0 {
                        tracing::warn!(
                            bytes_read = stats.bytes_read,
                            bytes_queued = stats.bytes_queued,
                            end_tail_discarded_bytes = stats.end_tail_discarded_bytes,
                            "本地 PTY 会话结束时为完成原生输出 drain 丢弃了队列尾部"
                        );
                    }
                }
                Ok(Err(error)) => {
                    first_error.get_or_insert(error);
                }
                Err(error) => {
                    first_error.get_or_insert_with(|| io::Error::other(error.to_string()));
                }
            }
        }
        tracing::trace!(worker = "writer", "joining local PTY blocking worker");
        let writer_result = match workers.writer.as_mut() {
            Some(writer) => Some(writer.await),
            None => None,
        };
        if let Some(result) = writer_result {
            workers.writer.take();
            match result {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    first_error.get_or_insert(error);
                }
                Err(error) => {
                    first_error.get_or_insert_with(|| io::Error::other(error.to_string()));
                }
            }
        }
        if let Some(error) = first_error {
            return Err(AppError::infrastructure_with_source(
                "本地 PTY 阻塞 I/O worker 失败",
                error,
            ));
        }
        // 自然 release 与最终 Close 是不同 owner；在读写 worker 已结束后先 join release，
        // 再启动/重试最终 master drop。Timeout 不会丢掉任一原生线程句柄。
        let natural_deadline = *self
            .natural_exit_deadline
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.closing.load(Ordering::Acquire) {
            let deadline = natural_deadline
                .ok_or_else(|| AppError::conflict("自然退出 drain 缺少原生退出的绝对 deadline"))?;
            if self.release_worker_lost.load(Ordering::Acquire) {
                return Err(AppError::infrastructure(
                    "本地 PTY release worker 异常退出，资源状态未知",
                ));
            }
            let mut release_worker = tokio::time::timeout_at(deadline, self.release_worker.lock())
                .await
                .map_err(|_| AppError::timeout("等待本地 PTY release owner 锁"))?;
            if release_worker.is_some() {
                match join_native_worker_until(
                    &mut release_worker,
                    deadline,
                    "等待自然 PTY release owner",
                    "本地 PTY release worker panic",
                )
                .await
                {
                    Ok(Ok(true)) => self.release_completed.store(true, Ordering::Release),
                    Ok(Ok(false)) => self.release_unsupported.store(true, Ordering::Release),
                    Ok(Err(error)) => {
                        return Err(AppError::infrastructure_with_source(
                            "本地 PTY release worker 失败",
                            error,
                        ));
                    }
                    Err(error) => {
                        if release_worker.is_none() {
                            self.release_worker_lost.store(true, Ordering::Release);
                        }
                        return Err(error);
                    }
                }
            }
            drop(release_worker);

            if self.drain_worker_lost.load(Ordering::Acquire) {
                return Err(AppError::infrastructure(
                    "本地 PTY drain worker 异常退出，master 回收状态未知",
                ));
            }
            let mut drain_worker = tokio::time::timeout_at(deadline, self.drain_worker.lock())
                .await
                .map_err(|_| AppError::timeout("等待本地 PTY drain owner 锁"))?;
            let master_still_owned = self
                .master
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some();
            if drain_worker.is_none() && master_still_owned {
                let master = Arc::clone(&self.master);
                let native_worker = thread::Builder::new()
                    .name("kerminal-pty-natural-drain".to_owned())
                    .spawn(move || {
                        let master = master
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .take();
                        drop(master);
                        Ok(())
                    })
                    .map_err(|error| {
                        AppError::infrastructure_with_source("启动本地 PTY drain owner 失败", error)
                    })?;
                *drain_worker = Some(native_worker);
            }
            if drain_worker.is_some() {
                match join_native_worker_until(
                    &mut drain_worker,
                    deadline,
                    "等待本地 PTY reader drain 后关闭 master",
                    "本地 PTY drain worker panic",
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Err(error) => {
                        if drain_worker.is_none() {
                            self.drain_worker_lost.store(true, Ordering::Release);
                        }
                        return Err(error);
                    }
                    Ok(Err(error)) => {
                        return Err(AppError::infrastructure_with_source(
                            "本地 PTY drain worker 失败",
                            error,
                        ));
                    }
                }
            }
        }
        Ok(status)
    }
}

#[cfg(test)]
#[path = "../tests/unit/local_pty_environment.rs"]
mod environment_tests;
