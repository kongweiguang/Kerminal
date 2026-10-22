//! Git 同步的生产 runner 进程、取消、预算和清理重试测试。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    sync::{Arc, Mutex as StdMutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    configuration_sync::ConfigurationSyncService,
    connections::{
        ConnectionIntent, HostKeyChallenge, HostKeyPeer, HostKeyTrustReceipt, HostKeyTrustRequest,
        RemoteConnectionLease, RemoteConnectionService,
    },
    operations::{
        CommandExecutionLimits, CommandExecutionResult, CommandExecutionTarget, CommandInvocation,
        CommandStreamCompletion, CommandStreamEvent, CommandStreamExit, CommandStreamInputMessage,
        CommandStreamState, ContainerStageCleanupCapability, ContainerStageCleanupRequest,
        OperationsCommandExecutor, OperationsCommandStream,
    },
};
use kerminal_domain::identity::HostId;
use tokio::{runtime::Runtime, sync::watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    configuration::ConfigurationStore,
    configuration_sync::{
        GitConfigurationSyncRepository,
        runner::{GitCommandRuntime, GitWorkerState},
    },
    operations::StructuredOperationsCommandExecutor,
    vault::VaultStore,
};

use super::{GitCommandError, GitOutput};

const HOLDER_ENV: &str = "KERMINAL_FAKE_GIT_HOLDER_PID_FILE";
const PARENT_ENV: &str = "KERMINAL_FAKE_GIT_PARENT_PID_FILE";
const DESCENDANT_ENV: &str = "KERMINAL_FAKE_GIT_DESCENDANT";
const LARGE_OUTPUT_ENV: &str = "KERMINAL_FAKE_GIT_LARGE_OUTPUT";
const PARENT_EXIT_STDOUT_MARKER: &[u8] = b"fake-git-parent-final-stdout";
const PARENT_EXIT_STDERR_MARKER: &[u8] = b"fake-git-parent-final-stderr";
const PARENT_EXITS_TEST: &str = "configuration_sync::git::tests::fake_git_parent_exits";
const PARENT_WAITS_TEST: &str = "configuration_sync::git::tests::fake_git_parent_waits";
const PIPE_HOLDER_TEST: &str = "configuration_sync::git::tests::fake_git_pipe_holder";
const LARGE_OUTPUT_TEST: &str = "configuration_sync::git::tests::fake_git_large_output";

/// 给每项进程测试创建独立临时目录，并在失败展开时清理由 fixture 启动的孤儿。
struct TestRoot(PathBuf);

impl TestRoot {
    /// 每次测试使用随机目录，避免触碰用户配置根或并发用例彼此干扰。
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "kerminal-git-process-test-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&path).expect("创建 Git 测试专属临时目录");
        Self(path)
    }

    /// 返回当前测试的独立工作目录。
    fn path(&self) -> &Path {
        &self.0
    }

    /// 返回测试进程写入 PID 文件的位置，供失败时精准清理自身 fixture。
    fn pid_file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestRoot {
    /// 仅终止本 fixture 明确记录的进程并移除随机测试目录。
    fn drop(&mut self) {
        terminate_recorded_process(&self.pid_file("parent.pid"));
        terminate_recorded_process(&self.pid_file("holder.pid"));
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 保持唯一 Tokio runtime 存活，并把测试程序注入实际生产 Git runner。
struct RunnerFixture {
    _runtime: Runtime,
    runner: Arc<GitCommandRuntime>,
}

impl RunnerFixture {
    /// 通过 StructuredOperationsCommandExecutor 测试真实生产流、预算和进程树收尾。
    fn new(
        root: &TestRoot,
        program: PathBuf,
        timeout: Duration,
        max_output_bytes: usize,
        extra_environment: &[(&str, &str)],
    ) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("创建隔离 runner runtime");
        let executor: Arc<dyn OperationsCommandExecutor> = Arc::new(
            StructuredOperationsCommandExecutor::new(Arc::new(NoRemoteConnections)),
        );
        let mut environment = BTreeMap::from([
            (
                HOLDER_ENV.to_owned(),
                root.pid_file("holder.pid").to_string_lossy().into_owned(),
            ),
            (
                PARENT_ENV.to_owned(),
                root.pid_file("parent.pid").to_string_lossy().into_owned(),
            ),
        ]);
        environment.extend(
            extra_environment
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        );
        let runner = Arc::new(GitCommandRuntime::with_configuration(
            executor,
            runtime.handle().clone(),
            program.to_string_lossy().into_owned(),
            environment,
            timeout,
            max_output_bytes,
        ));
        Self {
            _runtime: runtime,
            runner,
        }
    }

    /// 使用当前测试二进制作为 fake Git，仍由生产命令执行器按结构化 argv 启动。
    fn fake_git(
        root: &TestRoot,
        timeout: Duration,
        max_output_bytes: usize,
        extra_environment: &[(&str, &str)],
    ) -> Self {
        let executable = std::env::current_exe().expect("取得当前测试执行文件");
        Self::new(
            root,
            executable,
            timeout,
            max_output_bytes,
            extra_environment,
        )
    }
}

struct NoRemoteConnections;

#[async_trait]
impl RemoteConnectionService for NoRemoteConnections {
    /// 本机 runner 测试不允许意外建立远端连接。
    async fn connect(
        &self,
        _host_id: HostId,
        _intent: ConnectionIntent,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        Err(AppError::unsupported("Git fixture 不允许远端连接"))
    }

    /// Fixture 不读取或信任任何 SSH 主机密钥。
    async fn inspect_host_key(
        &self,
        _host_id: HostId,
        _peer: HostKeyPeer,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyChallenge> {
        Err(AppError::unsupported("Git fixture 不允许远端连接"))
    }

    /// Fixture 不写入 known_hosts。
    async fn trust_host_key(
        &self,
        _request: HostKeyTrustRequest,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyTrustReceipt> {
        Err(AppError::unsupported("Git fixture 不允许远端连接"))
    }

    /// 此测试替身未覆盖主机密钥轮换，显式拒绝可避免伪造信任回执。
    ///
    /// @author kongweiguang
    async fn replace_host_key(
        &self,
        _request: kerminal_application::connections::HostKeyRotationRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> kerminal_application::AppResult<kerminal_application::connections::HostKeyRotationReceipt>
    {
        Err(kerminal_application::AppError::unsupported(
            "test fake does not support host-key rotation",
        ))
    }
}

/// 用实际 runner 执行测试二进制中的指定 fake 子进程入口。
fn run_fake_git(
    runner: &GitCommandRuntime,
    root: &Path,
    test_name: &str,
    cancellation: &CancellationToken,
) -> Result<GitOutput, GitCommandError> {
    let arguments = [
        OsString::from("--ignored"),
        OsString::from("--exact"),
        OsString::from(test_name),
        OsString::from("--nocapture"),
        OsString::from("--test-threads=1"),
    ];
    runner.run_git(root, &arguments, None, cancellation, false, false)
}

/// 在有界时间内检查 fixture 子进程已写出 PID 文件。
fn wait_for_pid_file(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.is_file() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    path.is_file()
}

/// 在有界时间内等待 runner 公布指定清理状态，避免测试自身无限阻塞。
fn wait_for_worker_state(
    runner: &GitCommandRuntime,
    expected: GitWorkerState,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if runner.state() == expected {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    runner.state() == expected
}

/// 测试失败时只按 fixture PID 终止当前用例创建的残留进程。
fn terminate_recorded_process(path: &Path) {
    let Ok(pid) = fs::read_to_string(path) else {
        return;
    };
    let Ok(pid) = pid.trim().parse::<u32>() else {
        return;
    };

    #[cfg(unix)]
    {
        // SAFETY: PID 来自本测试私有的随机 fixture 文件。
        let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill.exe")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// 仅作为 fake Git 的管道持有进程入口，由生产进程树清理逻辑终止。
#[test]
#[ignore = "仅作为生产 Git runner 的隔离子进程入口"]
fn fake_git_pipe_holder() {
    if std::env::var_os(DESCENDANT_ENV).is_none() {
        return;
    }
    let pid_file = std::env::var_os(HOLDER_ENV).expect("fixture 提供 descendant PID 路径");
    fs::write(PathBuf::from(pid_file), std::process::id().to_string())
        .expect("写入 descendant PID");
    thread::sleep(Duration::from_secs(60));
}

/// 仅作为 fake Git 父进程入口，启动持有管道的后代、写入最终双流标记后自然退出。
///
/// @author kongweiguang
#[test]
#[ignore = "仅作为生产 Git runner 的隔离子进程入口"]
fn fake_git_parent_exits() {
    let parent_file = std::env::var_os(PARENT_ENV).expect("fixture 提供 parent PID 路径");
    fs::write(PathBuf::from(parent_file), std::process::id().to_string())
        .expect("写入 fake Git PID");
    spawn_pipe_holder_and_wait();
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(PARENT_EXIT_STDOUT_MARKER)
        .expect("写入 fake Git 父进程最终 stdout 标记");
    stdout.flush().expect("刷新 fake Git 父进程 stdout 标记");
    let mut stderr = io::stderr().lock();
    stderr
        .write_all(PARENT_EXIT_STDERR_MARKER)
        .expect("写入 fake Git 父进程最终 stderr 标记");
    stderr.flush().expect("刷新 fake Git 父进程 stderr 标记");
}

/// 仅作为 fake Git 父进程入口，保留运行态供取消和时限测试结束它。
#[test]
#[ignore = "仅作为生产 Git runner 的隔离子进程入口"]
fn fake_git_parent_waits() {
    let parent_file = std::env::var_os(PARENT_ENV).expect("fixture 提供 parent PID 路径");
    fs::write(PathBuf::from(parent_file), std::process::id().to_string())
        .expect("写入 fake Git PID");
    spawn_pipe_holder_and_wait();
    thread::sleep(Duration::from_secs(60));
}

/// 仅作为 fake Git 输出入口，制造超过 runner 上限的 stdout 以验证有界读取。
#[test]
#[ignore = "仅作为生产 Git runner 的隔离输出入口"]
fn fake_git_large_output() {
    if std::env::var_os(LARGE_OUTPUT_ENV).is_none() {
        return;
    }
    let output = vec![b'x'; 16 * 1024];
    io::stdout()
        .lock()
        .write_all(&output)
        .expect("写入 fake Git 输出");
}

/// 启动继承 stdout/stderr 的测试后代，并确认它已开始持有管道。
fn spawn_pipe_holder_and_wait() {
    let executable = std::env::current_exe().expect("取得当前测试执行文件");
    let child = Command::new(executable)
        .args([
            OsStr::new("--ignored"),
            OsStr::new("--exact"),
            OsStr::new(PIPE_HOLDER_TEST),
            OsStr::new("--nocapture"),
            OsStr::new("--test-threads=1"),
        ])
        .env(DESCENDANT_ENV, "1")
        .stdin(Stdio::null())
        .spawn()
        .expect("启动 fake Git 的管道持有后代");
    drop(child);

    let holder_file = std::env::var_os(HOLDER_ENV).expect("fixture 提供 descendant PID 路径");
    assert!(
        wait_for_pid_file(Path::new(&holder_file), Duration::from_secs(4)),
        "fake Git 后代未在期限内开始持有输出管道"
    );
}

/// 验证 fake Git 父进程成功退出后，生产 executor 回收持管道后代并保留父进程最后写入的双流字节。
///
/// @author kongweiguang
#[test]
fn parent_exit_terminates_descendant_before_output_completion() {
    let root = TestRoot::new();
    let fixture = RunnerFixture::fake_git(&root, Duration::from_secs(20), 64 * 1024, &[]);
    let started = Instant::now();
    let result = run_fake_git(
        &fixture.runner,
        root.path(),
        PARENT_EXITS_TEST,
        &CancellationToken::new(),
    );

    assert!(
        result.is_ok(),
        "fake Git 正常退出应保留成功状态: {result:?}"
    );
    let output = result.expect("父进程已正常退出且后代回收完成，应取得完整输出回执");
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "持管道后代未由生产进程树清理及时关闭"
    );
    assert!(
        output
            .stdout
            .windows(PARENT_EXIT_STDOUT_MARKER.len())
            .any(|window| window == PARENT_EXIT_STDOUT_MARKER),
        "父进程退出前写入的 stdout 尾部必须保留"
    );
    assert!(
        output
            .stderr_prefix
            .windows(PARENT_EXIT_STDERR_MARKER.len())
            .any(|window| window == PARENT_EXIT_STDERR_MARKER),
        "父进程退出前写入的 stderr 尾部必须保留"
    );
    assert!(root.pid_file("holder.pid").is_file());
}

/// 验证取消会通过同一个生产 executor 终止 fake Git 及持管道后代。
#[test]
fn cancellation_terminates_descendant_and_preserves_cancel_result() {
    let root = TestRoot::new();
    let fixture = RunnerFixture::fake_git(&root, Duration::from_secs(30), 64 * 1024, &[]);
    let cancellation = CancellationToken::new();
    let child_cancellation = cancellation.clone();
    let runner = fixture.runner.clone();
    let working_directory = root.path().to_path_buf();
    let (result_sender, result_receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = run_fake_git(
            &runner,
            &working_directory,
            PARENT_WAITS_TEST,
            &child_cancellation,
        );
        let _ = result_sender.send(result);
    });

    let descendant_started =
        wait_for_pid_file(&root.pid_file("holder.pid"), Duration::from_secs(6));
    cancellation.cancel();
    assert!(descendant_started, "取消前 fake Git 后代应已持有管道");
    let result = result_receiver
        .recv_timeout(Duration::from_secs(8))
        .expect("取消后生产 runner 应在期限内回收进程和管道");
    assert!(
        matches!(&result, Err(GitCommandError::Cancelled)),
        "取消应保留为主错误: {result:?}"
    );
    worker.join().expect("Git runner 管理线程应正常返回");
}

/// 验证生产命令预算会终止仍运行的进程树并保留 timeout 分类。
#[test]
fn command_deadline_terminates_running_descendant() {
    let root = TestRoot::new();
    let fixture = RunnerFixture::fake_git(&root, Duration::from_secs(5), 64 * 1024, &[]);
    let runner = fixture.runner.clone();
    let working_directory = root.path().to_path_buf();
    let (result_sender, result_receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = run_fake_git(
            &runner,
            &working_directory,
            PARENT_WAITS_TEST,
            &CancellationToken::new(),
        );
        let _ = result_sender.send(result);
    });

    assert!(
        wait_for_pid_file(&root.pid_file("holder.pid"), Duration::from_secs(6)),
        "deadline 到来前 fake Git 后代应已启动"
    );
    let result = result_receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("命令预算到期后应在期限内回收进程树");
    assert!(
        matches!(
            &result,
            Err(GitCommandError::Executor(error))
                if error.public_code() == PublicErrorCode::Timeout
        ),
        "应保留命令超时分类: {result:?}"
    );
    worker.join().expect("Git runner 管理线程应正常返回");
}

/// 验证生产 runner 限制 stdout 总量并在清理成功后返回明确超限结果。
#[test]
fn output_limit_is_enforced_by_production_runner() {
    let root = TestRoot::new();
    let fixture = RunnerFixture::fake_git(
        &root,
        Duration::from_secs(20),
        1024,
        &[(LARGE_OUTPUT_ENV, "1")],
    );
    let result = run_fake_git(
        &fixture.runner,
        root.path(),
        LARGE_OUTPUT_TEST,
        &CancellationToken::new(),
    );
    assert!(
        matches!(&result, Err(GitCommandError::OutputLimit)),
        "超过配置上限必须失败并清理: {result:?}"
    );
}

/// 验证无法启动的可执行文件仍映射为 Git 未安装，而非模糊执行失败。
#[test]
fn missing_executable_preserves_launch_classification() {
    let root = TestRoot::new();
    let fixture = RunnerFixture::new(
        &root,
        root.path().join("missing-git-executable"),
        Duration::from_secs(5),
        64 * 1024,
        &[],
    );
    let result = run_fake_git(
        &fixture.runner,
        root.path(),
        "unused",
        &CancellationToken::new(),
    );
    assert!(
        matches!(
            &result,
            Err(GitCommandError::Launch(error)) if error.kind() == io::ErrorKind::NotFound
        ),
        "缺少可执行文件应保留 NotFound 分类: {result:?}"
    );
}

/// 同步 runner 的清理重试专用 port：第一次关闭返回 Pending，第二次才回执完成。
struct PendingCleanupExecutor {
    starts: Arc<AtomicUsize>,
    pending_after_eof: Option<mpsc::Sender<()>>,
}

#[async_trait]
impl OperationsCommandExecutor for PendingCleanupExecutor {
    /// 清理回归不执行普通命令，所有调用必须走真实流式 Git 路径。
    async fn execute(
        &self,
        _target: CommandExecutionTarget,
        _invocation: CommandInvocation,
        _limits: CommandExecutionLimits,
        _cancel: CancellationToken,
    ) -> AppResult<CommandExecutionResult> {
        Err(AppError::unsupported("该 fixture 只提供命令流"))
    }

    /// 创建同一流句柄；两次 deadline 分别产生清理 Pending 与明确完成 ACK。
    ///
    /// 指定 EOF 见证时先保持输出通道打开，收到 Close 后保留其 ACK 再发布 Pending，
    /// 强制覆盖 writer 持有 EOF 锁期间输出失败的交错，而不依赖调度速度或额外等待。
    /// @author kongweiguang
    async fn execute_stream(
        &self,
        _target: CommandExecutionTarget,
        _invocation: CommandInvocation,
        _limits: CommandExecutionLimits,
        cancellation: CancellationToken,
    ) -> AppResult<OperationsCommandStream> {
        self.starts.fetch_add(1, Ordering::AcqRel);
        let (events_sender, events_receiver) = tokio::sync::mpsc::channel(4);
        let mut pending_after_eof = self.pending_after_eof.clone();
        let mut held_output_sender = if pending_after_eof.is_some() {
            Some(events_sender)
        } else {
            drop(events_sender);
            None
        };
        let (input_sender, mut input_receiver) =
            tokio::sync::mpsc::channel::<CommandStreamInputMessage>(4);
        let initial_state = if pending_after_eof.is_some() {
            CommandStreamState::Running { attempt: 1 }
        } else {
            CommandStreamState::CleanupPending {
                attempt: 1,
                error: AppError::timeout("fixture 初次清理仍在运行"),
            }
        };
        let (completion_sender, completion_receiver) = watch::channel(initial_state);
        let (cleanup_deadline_sender, mut cleanup_deadline_receiver) = watch::channel(None);
        let (_shutdown_ceiling_sender, shutdown_ceiling_receiver) = watch::channel(None);

        tokio::spawn(async move {
            let mut attempt = 1_u64;
            let mut held_eof_acknowledgement: Option<tokio::sync::oneshot::Sender<AppResult<()>>> =
                None;
            loop {
                tokio::select! {
                    changed = cleanup_deadline_receiver.changed() => {
                        if changed.is_err() || cleanup_deadline_receiver.borrow_and_update().is_none() {
                            break;
                        }
                        attempt += 1;
                        drop(held_output_sender.take());
                        if let Some(acknowledgement) = held_eof_acknowledgement.take() {
                            let _ = acknowledgement.send(Ok(()));
                        }
                        if attempt == 2 {
                            completion_sender.send_replace(CommandStreamState::CleanupPending {
                                attempt,
                                error: AppError::timeout("fixture 清理仍待重试"),
                            });
                        } else {
                            completion_sender.send_replace(CommandStreamState::Complete(
                                CommandStreamCompletion {
                                    execution: Ok(CommandStreamExit {
                                        exit_code: Some(0),
                                        stderr: Vec::new(),
                                        stderr_truncated: false,
                                        input_complete: true,
                                    }),
                                    cleanup: Ok(()),
                                },
                            ));
                            break;
                        }
                    }
                    message = input_receiver.recv() => match message {
                        Some(CommandStreamInputMessage::Data { acknowledgement, .. }) => {
                            // 此 fake 将收到消息视为内存 writer 已完整消费，回执必须发生在消费之后。
                            let _ = acknowledgement.send(Ok(()));
                        }
                        Some(CommandStreamInputMessage::Close { acknowledgement }) => {
                            if let Some(observed) = pending_after_eof.take() {
                                // 先不回 EOF ACK；原 writer 必须释放自己的锁才能进入同流 cleanup。
                                held_eof_acknowledgement = Some(acknowledgement);
                                completion_sender.send_replace(CommandStreamState::CleanupPending {
                                    attempt,
                                    error: AppError::timeout("fixture EOF 等待 ACK 时清理失败"),
                                });
                                let _ = observed.send(());
                            } else {
                                // 只有 fixture 确认处理 EOF 后，应用层才能认定输入完整。
                                let _ = acknowledgement.send(Ok(()));
                            }
                        }
                        None => break,
                    }
                }
            }
        });
        Ok(OperationsCommandStream::new(
            events_receiver,
            input_sender,
            completion_receiver,
            cancellation.child_token(),
            cleanup_deadline_sender,
            shutdown_ceiling_receiver,
        ))
    }

    /// 此用例不操作容器 staging，不签发 cleanup capability。
    fn reserve_container_stage_cleanup(
        &self,
        _request: ContainerStageCleanupRequest,
    ) -> AppResult<Arc<dyn ContainerStageCleanupCapability>> {
        Err(AppError::unsupported("fixture 不管理容器 staging"))
    }

    /// Fixture 不拥有命令 intake 状态。
    fn begin_shutdown(&self, _deadline: tokio::time::Instant) {}

    /// Fixture 不持有可在 service shutdown 中单独收尾的资源。
    async fn shutdown_until(&self, _deadline: tokio::time::Instant) -> AppResult<()> {
        Ok(())
    }
}

/// 验证 cleanup Pending 保留同一流；新 deadline 只唤醒该 owner，最终结果仍标为未知。
/// @author kongweiguang
#[test]
fn cleanup_pending_retries_same_stream_and_returns_unknown_outcome() {
    let root = TestRoot::new();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("创建隔离 runner runtime");
    let execution_starts = Arc::new(AtomicUsize::new(0));
    let executor: Arc<dyn OperationsCommandExecutor> = Arc::new(PendingCleanupExecutor {
        starts: execution_starts.clone(),
        pending_after_eof: None,
    });
    let runner = Arc::new(GitCommandRuntime::with_configuration(
        executor,
        runtime.handle().clone(),
        "fake-git-with-pending-cleanup".to_owned(),
        BTreeMap::new(),
        Duration::from_secs(10),
        1024,
    ));
    let thread_runner = runner.clone();
    let working_directory = root.path().to_path_buf();
    let (result_sender, result_receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = run_fake_git(
            &thread_runner,
            &working_directory,
            "unused",
            &CancellationToken::new(),
        );
        let _ = result_sender.send(result);
    });

    assert!(
        wait_for_worker_state(
            &runner,
            GitWorkerState::PendingCleanup,
            Duration::from_secs(4),
        ),
        "首次清理失败后应保留 PendingCleanup，实际状态: {:?}",
        runner.state(),
    );
    assert!(
        result_receiver.try_recv().is_err(),
        "首次 cleanup pending 时不能提前返回并丢弃流 owner"
    );
    runner.request_retry(tokio::time::Instant::now() + Duration::from_secs(3));
    let result = result_receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("新 deadline 应唤醒原 cleanup owner");
    assert!(
        matches!(result, Err(GitCommandError::OutcomeUnknown { .. })),
        "完成清理后仍需如实返回原 Git 结果未知: {result:?}"
    );
    assert_eq!(
        execution_starts.load(Ordering::Acquire),
        1,
        "cleanup retry 必须继续原 stream，不能启动第二个 Git 命令"
    );
    worker.join().expect("Git runner 管理线程应正常返回");
}

/// EOF ACK 未返回时注入输出清理失败，确保被放弃的 writer 先释放锁且重试仍属于原 stream。
///
/// fixture 收到 Close 才发布 Pending，并将 ACK 保留到 cleanup 请求，固定真实持锁交错；
/// 不通过延长生产或测试预算掩盖死锁，完成清理后仍要求原始失败保持 Unknown。
/// @author kongweiguang
#[test]
fn cleanup_pending_with_inflight_eof_releases_writer_before_retry() {
    let root = TestRoot::new();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("创建隔离 runner runtime");
    let execution_starts = Arc::new(AtomicUsize::new(0));
    let (eof_sender, eof_receiver) = mpsc::channel();
    let executor: Arc<dyn OperationsCommandExecutor> = Arc::new(PendingCleanupExecutor {
        starts: execution_starts.clone(),
        pending_after_eof: Some(eof_sender),
    });
    let runner = Arc::new(GitCommandRuntime::with_configuration(
        executor,
        runtime.handle().clone(),
        "fake-git-with-inflight-eof".to_owned(),
        BTreeMap::new(),
        Duration::from_secs(10),
        1024,
    ));
    let thread_runner = runner.clone();
    let working_directory = root.path().to_path_buf();
    let (result_sender, result_receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = run_fake_git(
            &thread_runner,
            &working_directory,
            "unused",
            &CancellationToken::new(),
        );
        let _ = result_sender.send(result);
    });

    eof_receiver
        .recv_timeout(Duration::from_secs(4))
        .expect("writer 必须先送达 EOF 并等待尚未回执的 ACK");
    assert!(
        wait_for_worker_state(
            &runner,
            GitWorkerState::PendingCleanup,
            Duration::from_secs(4),
        ),
        "输出失败后必须释放 EOF 锁以进入 cleanup，实际状态: {:?}",
        runner.state(),
    );
    assert!(
        matches!(result_receiver.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "首次 cleanup pending 时原 worker 必须继续持有流且不能提前返回"
    );
    runner.request_retry(tokio::time::Instant::now() + Duration::from_secs(3));
    let result = result_receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("新 deadline 应唤醒持有原 stream 的 cleanup owner");
    assert!(
        matches!(result, Err(GitCommandError::OutcomeUnknown { .. })),
        "EOF 竞争清理完成后原始输出失败仍须保持未知: {result:?}",
    );
    assert_eq!(
        execution_starts.load(Ordering::Acquire),
        1,
        "释放 writer 只用于重试同一流，不能启动替代命令",
    );
    worker.join().expect("Git runner 管理线程应正常返回");
}

/// 真实 Git runner 收到 exit 0 但 stdin EOF 未确认时只能返回未知，并保留有界回执且不重跑。
#[test]
fn commit_with_unconfirmed_stdin_exit_zero_keeps_receipt_without_replay() {
    const STDOUT_SENTINEL: &[u8] = b"fixture-commit-stdout-secret";
    const STDERR_SENTINEL: &[u8] = b"fixture-commit-stderr-secret";
    const PATHSPEC: &[u8] = b"fixture.txt\0";

    let root = TestRoot::new();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("创建隔离 runner runtime");
    let starts = Arc::new(AtomicUsize::new(0));
    let arguments = Arc::new(StdMutex::new(Vec::<Vec<String>>::new()));
    let received = Arc::new(StdMutex::new(Vec::<u8>::new()));
    let executor: Arc<dyn OperationsCommandExecutor> = Arc::new(IncompleteInputCommitExecutor {
        starts: starts.clone(),
        arguments: arguments.clone(),
        received: received.clone(),
    });
    let runner = GitCommandRuntime::with_configuration(
        executor,
        runtime.handle().clone(),
        "fixture-git".to_owned(),
        BTreeMap::new(),
        Duration::from_secs(10),
        1024,
    );
    let result = runner.run_git(
        root.path(),
        &[
            OsString::from("commit"),
            OsString::from("--file=-"),
            OsString::from("--cleanup=verbatim"),
        ],
        Some(PATHSPEC.to_vec()),
        &CancellationToken::new(),
        false,
        false,
    );

    let error = result.expect_err("未确认 stdin 的 exit 0 不能冒充 Git commit 成功");
    let GitCommandError::OutcomeUnknown {
        receipt: Some(receipt),
        cause: Some(cause),
    } = &error
    else {
        panic!("应保留未知结果与 bounded receipt: {error:?}");
    };
    assert_eq!(receipt.code, Some(0));
    assert_eq!(receipt.stdout, STDOUT_SENTINEL);
    assert_eq!(receipt.stderr_prefix, STDERR_SENTINEL);
    assert_eq!(cause.public_code(), PublicErrorCode::Conflict);
    assert_eq!(
        starts.load(Ordering::Acquire),
        1,
        "未知 commit 不得由 runner 自动重执行"
    );
    assert_eq!(
        arguments.lock().expect("读取 fixture argv").as_slice(),
        &[vec![
            "commit".to_owned(),
            "--file=-".to_owned(),
            "--cleanup=verbatim".to_owned(),
        ]],
        "唯一执行的命令必须是原始 commit argv"
    );
    assert_eq!(
        received.lock().expect("读取已确认的数据前缀").as_slice(),
        PATHSPEC,
        "fake executor 应收到本次 commit 输入"
    );
    let debug = format!("{error:?}");
    assert!(debug.contains("stdout_bytes"));
    assert!(!debug.contains(std::str::from_utf8(STDOUT_SENTINEL).unwrap()));
    assert!(!debug.contains(std::str::from_utf8(STDERR_SENTINEL).unwrap()));
    assert_eq!(error.to_string(), "Git 命令结果未知");
}

/// 只读 Git 命令没有 stdin payload，不因无关的 EOF 确认缺失被误报为未知。
#[test]
fn no_input_git_command_does_not_require_eof_ack() {
    let root = TestRoot::new();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("创建隔离 runner runtime");
    let starts = Arc::new(AtomicUsize::new(0));
    let runner = GitCommandRuntime::with_configuration(
        Arc::new(IncompleteInputCommitExecutor {
            starts: starts.clone(),
            arguments: Arc::new(StdMutex::new(Vec::new())),
            received: Arc::new(StdMutex::new(Vec::new())),
        }),
        runtime.handle().clone(),
        "fixture-git".to_owned(),
        BTreeMap::new(),
        Duration::from_secs(10),
        1024,
    );

    let output = runner
        .run_git(
            root.path(),
            &[OsString::from("status")],
            None,
            &CancellationToken::new(),
            false,
            false,
        )
        .expect("无输入只读命令不依赖 stdin EOF ACK");
    assert_eq!(output.code, Some(0));
    assert_eq!(starts.load(Ordering::Acquire), 1);
}

/// 配置仓储会在启动时校验现存设置，因此先写入 schema 1 的有效浅色主题 fixture；再验证
/// HEAD 变化不能覆盖 pathspec EOF 未确认结果。
///
/// @author kongweiguang
#[test]
fn commit_service_does_not_promote_unconfirmed_input_after_head_changes() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("跳过配置同步用例：当前环境未安装 Git");
        return;
    }

    let root = TestRoot::new();
    let settings_path = root.path().join("settings.toml");
    fs::write(
        &settings_path,
        "schema_version = 1\nthemeMode = \"light\"\n",
    )
    .expect("写入 schema 1 的 fixture 初始设置");
    let configuration = ConfigurationStore::initialize(root.path()).expect("初始化隔离配置仓储");
    assert_eq!(
        configuration.recovery_state().mode,
        kerminal_domain::preferences::ConfigurationMode::ReadWrite,
        "启动 validator 应接受该 schema 1 设置 fixture"
    );
    super::super::gitignore::ensure_ignore_rules(root.path())
        .expect("预先维护测试仓库的同步忽略规则");
    for arguments in [
        &(["init", "--quiet"][..]),
        &["config", "user.name", "Kerminal Fixture"],
        &["config", "user.email", "fixture@example.invalid"],
    ] {
        let output = run_fixture_git(root.path(), arguments);
        assert!(output.status.success(), "初始化隔离 Git 仓库失败");
    }
    let branch = run_fixture_git(root.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
    assert!(branch.status.success(), "设置隔离分支失败");
    let add = run_fixture_git(root.path(), &["add", "--all"]);
    assert!(add.status.success(), "暂存配置基线失败");
    let baseline = run_fixture_git(
        root.path(),
        &["commit", "--quiet", "-m", "fixture baseline"],
    );
    assert!(baseline.status.success(), "创建配置基线提交失败");
    let before = run_fixture_git(root.path(), &["rev-parse", "HEAD"]);
    assert!(before.status.success(), "读取配置基线提交失败");
    let before = String::from_utf8_lossy(&before.stdout).trim().to_owned();

    let mut settings = fs::read_to_string(&settings_path).expect("读取 fixture 初始设置");
    settings.push_str("\n# synthetic change for EOF acknowledgement regression\n");
    fs::write(settings_path, settings).expect("写入隔离设置变更");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("创建隔离同步 runtime");
    let commit_attempts = Arc::new(AtomicUsize::new(0));
    let input_attempts = Arc::new(AtomicUsize::new(0));
    let arguments = Arc::new(StdMutex::new(Vec::<Vec<String>>::new()));
    let received = Arc::new(StdMutex::new(Vec::<u8>::new()));
    let recorded_arguments = arguments.clone();
    let recorded_input = received.clone();
    let inner: Arc<dyn OperationsCommandExecutor> = Arc::new(
        StructuredOperationsCommandExecutor::new(Arc::new(NoRemoteConnections)),
    );
    let executor: Arc<dyn OperationsCommandExecutor> = Arc::new(CommitThenUnconfirmedExecutor {
        inner,
        commit_attempts: commit_attempts.clone(),
        input: IncompleteInputCommitExecutor {
            starts: input_attempts.clone(),
            arguments,
            received,
        },
    });
    let vault = VaultStore::open(configuration.storage_coordinator());
    let repository = GitConfigurationSyncRepository::new(
        configuration,
        vault,
        executor,
        runtime.handle().clone(),
    )
    .expect("装配隔离同步仓储");
    let service = ConfigurationSyncService::new(Arc::new(repository));
    let result = runtime
        .block_on(service.commit(CancellationToken::new()))
        .expect("commit 用例应返回可展示的操作结果");

    let after = run_fixture_git(root.path(), &["rev-parse", "HEAD"]);
    assert!(after.status.success(), "读取提交后的 HEAD 失败");
    let after = String::from_utf8_lossy(&after.stdout).trim().to_owned();
    assert_ne!(before, after, "fake transport 已在隔离仓库中产生真实提交");
    assert_eq!(
        result.outcome,
        kerminal_application::configuration_sync::ConfigurationSyncOutcome::OutcomeUnknown
    );
    assert_eq!(
        result.commit_hash, None,
        "EOF 未确认时不发布未经证明的提交哈希"
    );
    assert_eq!(commit_attempts.load(Ordering::Acquire), 1);
    assert_eq!(
        input_attempts.load(Ordering::Acquire),
        1,
        "不得自动重放 commit"
    );
    assert_eq!(
        recorded_arguments.lock().expect("读取 commit argv").len(),
        1,
        "生产 commit 路径只执行一次"
    );
    assert_eq!(
        recorded_input
            .lock()
            .expect("读取 pathspec 输入")
            .as_slice(),
        b":(top,literal)settings.toml\0",
        "fake stream 已收到完整合成 pathspec，只有 EOF 回执未知"
    );
}

/// 在本测试专属仓库中运行 Git fixture 命令，不读取用户级配置目录。
fn run_fixture_git(root: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(arguments)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("Git fixture 已确认可启动")
}

/// 在隔离仓库中先让真实 Git 完成提交，再模拟 stdin EOF ACK 不确定的流回执。
struct CommitThenUnconfirmedExecutor {
    inner: Arc<dyn OperationsCommandExecutor>,
    commit_attempts: Arc<AtomicUsize>,
    input: IncompleteInputCommitExecutor,
}

#[async_trait]
impl OperationsCommandExecutor for CommitThenUnconfirmedExecutor {
    /// 非流式调用交给结构化执行器，测试不会调用这些读路径写入输入。
    async fn execute(
        &self,
        target: CommandExecutionTarget,
        invocation: CommandInvocation,
        limits: CommandExecutionLimits,
        cancellation: CancellationToken,
    ) -> AppResult<CommandExecutionResult> {
        self.inner
            .execute(target, invocation, limits, cancellation)
            .await
    }

    /// 通用输入命令仍由同一个生产结构化执行器处理。
    async fn execute_with_input(
        &self,
        target: CommandExecutionTarget,
        invocation: CommandInvocation,
        input: Vec<u8>,
        limits: CommandExecutionLimits,
        cancellation: CancellationToken,
    ) -> AppResult<CommandExecutionResult> {
        self.inner
            .execute_with_input(target, invocation, input, limits, cancellation)
            .await
    }

    /// 只拦截生产 commit invocation，先让真实 Git 改变 HEAD，再提供未知 EOF 回执。
    async fn execute_stream(
        &self,
        target: CommandExecutionTarget,
        invocation: CommandInvocation,
        limits: CommandExecutionLimits,
        cancellation: CancellationToken,
    ) -> AppResult<OperationsCommandStream> {
        if invocation
            .arguments
            .first()
            .is_some_and(|argument| argument == "commit")
        {
            self.commit_attempts.fetch_add(1, Ordering::AcqRel);
            let working_directory = invocation
                .working_directory
                .as_deref()
                .ok_or_else(|| AppError::invalid("fixture commit 缺少隔离仓库目录"))?;
            let output = Command::new(&invocation.program)
                .args([
                    "commit",
                    "--only",
                    "-m",
                    "fixture real commit before uncertain EOF",
                    "--",
                    "settings.toml",
                ])
                .current_dir(working_directory)
                .envs(invocation.environment.iter())
                .output()
                .map_err(|error| {
                    AppError::infrastructure_with_source("运行隔离 Git commit fixture 失败", error)
                })?;
            if !output.status.success() {
                return Err(AppError::conflict("隔离 Git fixture 未能先创建真实提交"));
            }
            return self
                .input
                .execute_stream(target, invocation, limits, cancellation)
                .await;
        }

        self.inner
            .execute_stream(target, invocation, limits, cancellation)
            .await
    }

    /// 容器 staging 未被测试使用，保留结构化执行器的原能力。
    fn reserve_container_stage_cleanup(
        &self,
        request: ContainerStageCleanupRequest,
    ) -> AppResult<Arc<dyn ContainerStageCleanupCapability>> {
        self.inner.reserve_container_stage_cleanup(request)
    }

    /// 测试不改变底层 executor 的 intake 生命周期。
    fn begin_shutdown(&self, deadline: tokio::time::Instant) {
        self.inner.begin_shutdown(deadline);
    }

    /// 生命周期清理沿用同一个生产 executor。
    async fn shutdown_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        self.inner.shutdown_until(deadline).await
    }
}

/// 模拟 Git 进程已返回 0、但 EOF ACK 失败的流，验证调用方必须依赖输入回执。
struct IncompleteInputCommitExecutor {
    starts: Arc<AtomicUsize>,
    arguments: Arc<StdMutex<Vec<Vec<String>>>>,
    received: Arc<StdMutex<Vec<u8>>>,
}

#[async_trait]
impl OperationsCommandExecutor for IncompleteInputCommitExecutor {
    /// 此 fixture 只验证生产流式 runner，禁止绕过 stream 的命令路径。
    async fn execute(
        &self,
        _target: CommandExecutionTarget,
        _invocation: CommandInvocation,
        _limits: CommandExecutionLimits,
        _cancel: CancellationToken,
    ) -> AppResult<CommandExecutionResult> {
        Err(AppError::unsupported("commit input fixture 只提供命令流"))
    }

    /// 返回 exit 0 与未确认 EOF，并在 ACK 前先累计 bounded stdout/stderr 供 runner 排空。
    async fn execute_stream(
        &self,
        _target: CommandExecutionTarget,
        invocation: CommandInvocation,
        _limits: CommandExecutionLimits,
        cancellation: CancellationToken,
    ) -> AppResult<OperationsCommandStream> {
        self.starts.fetch_add(1, Ordering::AcqRel);
        self.arguments
            .lock()
            .expect("记录 fixture argv")
            .push(invocation.arguments);

        let (events_sender, events_receiver) = tokio::sync::mpsc::channel(4);
        events_sender
            .send(CommandStreamEvent::Stdout(
                b"fixture-commit-stdout-secret".to_vec(),
            ))
            .await
            .expect("写入 bounded stdout fixture");
        events_sender
            .send(CommandStreamEvent::Stderr(
                b"fixture-commit-stderr-secret".to_vec(),
            ))
            .await
            .expect("写入 bounded stderr fixture");
        drop(events_sender);

        let (input_sender, mut input_receiver) =
            tokio::sync::mpsc::channel::<CommandStreamInputMessage>(4);
        let (completion_sender, completion_receiver) =
            watch::channel(CommandStreamState::Running { attempt: 1 });
        let (cleanup_deadline_sender, _cleanup_deadline_receiver) = watch::channel(None);
        let (_shutdown_ceiling_sender, shutdown_ceiling_receiver) = watch::channel(None);
        let received = self.received.clone();

        tokio::spawn(async move {
            while let Some(message) = input_receiver.recv().await {
                match message {
                    CommandStreamInputMessage::Data {
                        bytes,
                        acknowledgement,
                    } => {
                        received.lock().expect("记录 fixture stdin").extend(bytes);
                        let _ = acknowledgement.send(Ok(()));
                    }
                    CommandStreamInputMessage::Close { acknowledgement } => {
                        let _ = acknowledgement.send(Err(AppError::conflict(
                            "fixture EOF acknowledgement is unknown",
                        )));
                        completion_sender.send_replace(CommandStreamState::Complete(
                            CommandStreamCompletion {
                                execution: Ok(CommandStreamExit {
                                    exit_code: Some(0),
                                    stderr: Vec::new(),
                                    stderr_truncated: false,
                                    input_complete: false,
                                }),
                                cleanup: Ok(()),
                            },
                        ));
                        break;
                    }
                }
            }
        });

        Ok(OperationsCommandStream::new(
            events_receiver,
            input_sender,
            completion_receiver,
            cancellation.child_token(),
            cleanup_deadline_sender,
            shutdown_ceiling_receiver,
        ))
    }

    /// 该 fixture 不管理容器 staging，也不签发清理能力。
    fn reserve_container_stage_cleanup(
        &self,
        _request: ContainerStageCleanupRequest,
    ) -> AppResult<Arc<dyn ContainerStageCleanupCapability>> {
        Err(AppError::unsupported("fixture 不管理容器 staging"))
    }

    /// 测试 executor 没有普通命令 intake 状态。
    fn begin_shutdown(&self, _deadline: tokio::time::Instant) {}

    /// 测试 executor 不拥有需要独立关闭的资源。
    async fn shutdown_until(&self, _deadline: tokio::time::Instant) -> AppResult<()> {
        Ok(())
    }
}
