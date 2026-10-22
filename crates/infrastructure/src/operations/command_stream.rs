//! 运维命令双向流 worker；stdout/stderr 有界背压，资源回收由 executor 生命周期持有。
//!
//! @author kongweiguang

use std::{io, process::Stdio, sync::Arc, time::Duration};

use crate::process_tree::{
    ProcessTreeGuard,
    spawn::{ManagedTokioChild, ProcessSpawnOptions},
};
use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    connections::{CommandEvent, ConnectionIntent, ExecRequest, RemoteConnectionService},
    operations::{
        CommandExecutionLimits, CommandExecutionTarget, CommandInvocation, CommandStreamCompletion,
        CommandStreamEvent, CommandStreamExit, CommandStreamInputMessage, CommandStreamState,
    },
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{ChildStdin, Command},
    sync::{mpsc, watch},
    time::{Instant, sleep_until, timeout_at},
};
use tokio_util::sync::CancellationToken;

const OUTPUT_CHUNK_BYTES: usize = 32 * 1024;
const STDERR_CAPTURE_MAX_BYTES: usize = 64 * 1024;
const CLEANUP_FALLBACK: Duration = Duration::from_secs(5);

/// 将流的两个方向及生命周期信号作为同一组所有权传递，避免拆散取消与完成回执。
struct StreamWorkerChannels {
    input: mpsc::Receiver<CommandStreamInputMessage>,
    output: mpsc::Sender<CommandStreamEvent>,
    cancellation: CancellationToken,
    cleanup_deadline: watch::Receiver<Option<Instant>>,
    completion_state: watch::Sender<CommandStreamState>,
}

/// 启动 worker 所需的完整输入；channel 与 cancellation/deadline 必须来自同一次 stream 创建。
pub(super) struct StreamWorkerRequest {
    pub(super) remote_connections: Arc<dyn RemoteConnectionService>,
    pub(super) target: CommandExecutionTarget,
    pub(super) invocation: CommandInvocation,
    pub(super) limits: CommandExecutionLimits,
    pub(super) input: mpsc::Receiver<CommandStreamInputMessage>,
    pub(super) output: mpsc::Sender<CommandStreamEvent>,
    pub(super) cancellation: CancellationToken,
    pub(super) cleanup_deadline: watch::Receiver<Option<Instant>>,
    pub(super) completion_state: watch::Sender<CommandStreamState>,
}

/// 调用方一次性移交同一 stream 的参数；spawn owner 持有 permit 直到完成回执发送后释放。
///
/// @author kongweiguang
pub(super) async fn run_stream_worker(request: StreamWorkerRequest) -> CommandStreamCompletion {
    let StreamWorkerRequest {
        remote_connections,
        target,
        invocation,
        limits,
        input,
        output,
        cancellation,
        cleanup_deadline,
        completion_state,
    } = request;
    match target {
        CommandExecutionTarget::Local => {
            run_local(
                invocation,
                limits,
                input,
                output,
                cancellation,
                cleanup_deadline,
                completion_state,
            )
            .await
        }
        CommandExecutionTarget::Host(host_id) => {
            run_remote(
                remote_connections,
                host_id,
                invocation,
                limits,
                StreamWorkerChannels {
                    input,
                    output,
                    cancellation,
                    cleanup_deadline,
                    completion_state,
                },
            )
            .await
        }
    }
}

/// 后台管道命令必须禁止创建 Windows 控制台；创建标志经 wrapper 合并以保留 Job 生命周期。
/// 在 spawn 时取得整棵进程树 owner，并让 pipe 与事件背压共用命令 deadline。
/// 直接 child 先退出且 pipe 仍开时先回收后代，再继续 drain 以保留已写尾部。
///
/// @author kongweiguang
async fn run_local(
    invocation: CommandInvocation,
    limits: CommandExecutionLimits,
    mut input: mpsc::Receiver<CommandStreamInputMessage>,
    output: mpsc::Sender<CommandStreamEvent>,
    cancellation: CancellationToken,
    mut cleanup_deadline: watch::Receiver<Option<Instant>>,
    completion_state: watch::Sender<CommandStreamState>,
) -> CommandStreamCompletion {
    let mut command = Command::new(&invocation.program);
    command
        .args(&invocation.arguments)
        .envs(&invocation.environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let has_working_directory = invocation.working_directory.is_some();
    if let Some(directory) = invocation.working_directory {
        command.current_dir(directory);
    }
    let mut child = match ProcessTreeGuard::spawn_tokio(
        command,
        ProcessSpawnOptions {
            kill_on_drop: true,
            windows_creation_flags: if cfg!(windows) { 0x0800_0000 } else { 0 },
        },
    ) {
        Ok(child) => child,
        Err(source) => {
            return completion(
                Err(local_spawn_failure(
                    &invocation.program,
                    has_working_directory,
                    source,
                )),
                Ok(()),
            );
        }
    };
    let Some(mut stdin) = child.stdin().take() else {
        // 启动阶段已经无法建立完整管道，清理可能重试，不能让 finish 等 sender 存活。
        drop(output);
        let cleanup =
            retry_child_cleanup(&mut child, &mut cleanup_deadline, &completion_state).await;
        return completion(
            Err(AppError::infrastructure("本机命令标准输入管道不可用")),
            cleanup,
        );
    };
    let Some(mut stdout) = child.stdout().take() else {
        // 缺少输出管道时没有可发送事件，先断开输出再等待有界清理结果。
        drop(output);
        let cleanup =
            retry_child_cleanup(&mut child, &mut cleanup_deadline, &completion_state).await;
        return completion(
            Err(AppError::infrastructure("本机命令标准输出管道不可用")),
            cleanup,
        );
    };
    let Some(mut stderr) = child.stderr().take() else {
        // 缺少错误输出管道时同样停止事件生产，避免清理失败被 finish 隐藏。
        drop(output);
        let cleanup =
            retry_child_cleanup(&mut child, &mut cleanup_deadline, &completion_state).await;
        return completion(
            Err(AppError::infrastructure("本机命令错误输出管道不可用")),
            cleanup,
        );
    };

    let writer_task_cancel = cancellation.clone();
    let mut writer = Some(tokio::spawn(async move {
        pump_local_input(&mut stdin, &mut input, writer_task_cancel).await
    }));
    let command_deadline = Instant::now() + limits.timeout;
    let capture_limit = limits.max_output_bytes.clamp(1, STDERR_CAPTURE_MAX_BYTES);
    let mut captured_stderr = Vec::new();
    let mut stderr_truncated = false;
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut status = None;
    let mut tree_terminated_after_parent_exit = false;
    let mut execution_error = None;
    let mut stdout_buffer = [0; OUTPUT_CHUNK_BYTES];
    let mut stderr_buffer = [0; OUTPUT_CHUNK_BYTES];

    while !(stdout_done && stderr_done && status.is_some()) {
        if status.is_some() && (!stdout_done || !stderr_done) && !tree_terminated_after_parent_exit
        {
            tree_terminated_after_parent_exit = true;
            if let Err(source) = child.terminate_tree_until(command_deadline).await {
                execution_error = Some(AppError::infrastructure_with_source(
                    "本机命令父进程退出后的后代回收失败",
                    source,
                ));
                break;
            }
        }

        tokio::select! {
            _ = cancellation.cancelled() => {
                execution_error = Some(AppError::cancelled("本机运维命令已取消"));
                break;
            }
            _ = sleep_until(command_deadline) => {
                cancellation.cancel();
                execution_error = Some(AppError::timeout("本机运维命令流"));
                break;
            }
            read = stdout.read(&mut stdout_buffer), if !stdout_done => match read {
                Ok(0) => stdout_done = true,
                Ok(count) => {
                    let event = CommandStreamEvent::Stdout(stdout_buffer[..count].to_vec());
                    if let Err(error) = send_output(&output, event, &cancellation, command_deadline).await {
                        execution_error = Some(error);
                        break;
                    }
                }
                Err(source) => {
                    execution_error = Some(AppError::infrastructure_with_source("本机命令 stdout 读取失败", source));
                    break;
                }
            },
            read = stderr.read(&mut stderr_buffer), if !stderr_done => match read {
                Ok(0) => stderr_done = true,
                Ok(count) => {
                    append_stderr_tail(&mut captured_stderr, &stderr_buffer[..count], capture_limit, &mut stderr_truncated);
                    let event = CommandStreamEvent::Stderr(stderr_buffer[..count].to_vec());
                    if let Err(error) = send_output(&output, event, &cancellation, command_deadline).await {
                        execution_error = Some(error);
                        break;
                    }
                }
                Err(source) => {
                    execution_error = Some(AppError::infrastructure_with_source("本机命令 stderr 读取失败", source));
                    break;
                }
            },
            result = child.wait(), if status.is_none() => match result {
                Ok(exit_status) => status = Some(exit_status),
                Err(source) => {
                    execution_error = Some(AppError::infrastructure_with_source("本机运维命令等待失败", source));
                    break;
                }
            },
        }
    }

    if execution_error.is_some() {
        cancellation.cancel();
    }
    cancellation.cancel();
    // 进入可能多轮等待的资源清理后不再写事件；queued events 仍留在 receiver 中供 finish drain。
    drop(output);
    let terminate_tree =
        execution_error.is_some() || status.is_none() || !stdout_done || !stderr_done;
    let mut cleanup_attempt = 0_u64;
    loop {
        let deadline = resolve_cleanup_deadline(&mut cleanup_deadline);
        let cleanup_result = {
            let mut resources = LocalCleanupResources {
                child: &mut child,
                stdout: &mut stdout,
                stderr: &mut stderr,
                stdout_done: &mut stdout_done,
                stderr_done: &mut stderr_done,
                status: &mut status,
                writer: &mut writer,
            };
            cleanup_local_resources(&mut resources, terminate_tree, deadline).await
        };
        match cleanup_result {
            Ok(diagnostics) => {
                for error in diagnostics {
                    tracing::warn!(
                        error_code = error.public_code().as_str(),
                        "本机运维命令清理中遇到可恢复诊断"
                    );
                }
                break;
            }
            Err(error) => {
                publish_cleanup_pending(&completion_state, &mut cleanup_attempt, error);
                wait_for_cleanup_retry(&mut cleanup_deadline).await;
            }
        }
    }
    let execution = if let Some(error) = execution_error {
        Err(error)
    } else {
        status
            .as_ref()
            .map(|status| CommandStreamExit {
                exit_code: status.code(),
                stderr: captured_stderr,
                stderr_truncated,
                input_complete: true,
            })
            .ok_or_else(|| AppError::infrastructure("本机命令缺少退出状态"))
    };
    completion(execution, Ok(()))
}

/// 使用独立 task 写 stdin，并允许取消打断阻塞写入或 stdin 半关闭以进入有界回收。
///
/// @author kongweiguang
async fn pump_local_input(
    stdin: &mut ChildStdin,
    input: &mut mpsc::Receiver<CommandStreamInputMessage>,
    cancellation: CancellationToken,
) -> AppResult<()> {
    loop {
        let message = tokio::select! {
            _ = cancellation.cancelled() => return Err(AppError::cancelled("本机命令 stdin 已取消")),
            message = input.recv() => message,
        };
        match message {
            Some(CommandStreamInputMessage::Data {
                bytes,
                acknowledgement,
            }) => {
                let result = tokio::select! {
                    _ = cancellation.cancelled() => return Err(AppError::cancelled("本机命令 stdin 已取消")),
                    result = stdin.write_all(&bytes) => {
                        result.map_err(|source| AppError::infrastructure_with_source("本机命令 stdin 写入失败", source))
                    },
                };
                let _ = acknowledgement.send(result.clone());
                result?;
            }
            Some(CommandStreamInputMessage::Close { acknowledgement }) => {
                let result = tokio::select! {
                    _ = cancellation.cancelled() => Err(AppError::cancelled("本机命令 stdin 关闭已取消")),
                    result = stdin.shutdown() => result.map_err(|source| {
                        AppError::infrastructure_with_source("本机命令 stdin 关闭失败", source)
                    }),
                };
                let _ = acknowledgement.send(result.clone());
                return result;
            }
            None => {
                return tokio::select! {
                    _ = cancellation.cancelled() => Err(AppError::cancelled("本机命令 stdin 关闭已取消")),
                    result = stdin.shutdown() => result.map_err(|source| {
                        AppError::infrastructure_with_source("本机命令 stdin 关闭失败", source)
                    }),
                };
            }
        }
    }
}

/// 远程命令沿用 profile 安全编码和独立 lease；流通道成组移交以统一取消与清理 deadline。
///
/// @author kongweiguang
async fn run_remote(
    remote_connections: Arc<dyn RemoteConnectionService>,
    host_id: kerminal_domain::identity::HostId,
    invocation: CommandInvocation,
    limits: CommandExecutionLimits,
    channels: StreamWorkerChannels,
) -> CommandStreamCompletion {
    let StreamWorkerChannels {
        mut input,
        output,
        cancellation,
        mut cleanup_deadline,
        completion_state,
    } = channels;
    let lease = match remote_connections
        .connect(host_id, ConnectionIntent::Exec, cancellation.clone())
        .await
    {
        Ok(lease) => lease,
        Err(error) => return completion(Err(error), Ok(())),
    };
    let request = ExecRequest {
        program: invocation.program,
        args: invocation.arguments,
        environment: invocation.environment,
        working_directory: invocation.working_directory,
    };
    let mut stream = match lease.exec_stream(request, cancellation.clone()).await {
        Ok(stream) => stream,
        Err(error) => {
            // exec channel 未创建成功，后续只重试 lease 关闭，不再有输出事件。
            drop(output);
            let mut attempt = 0_u64;
            loop {
                let deadline = resolve_cleanup_deadline(&mut cleanup_deadline);
                match lease
                    .close_until(deadline, "stream command start failed")
                    .await
                {
                    Ok(()) => return completion(Err(error), Ok(())),
                    Err(cleanup_error) => {
                        publish_cleanup_pending(&completion_state, &mut attempt, cleanup_error);
                        wait_for_cleanup_retry(&mut cleanup_deadline).await;
                    }
                }
            }
        }
    };
    let remote_input = stream.input_handle();
    let writer_task_cancel = cancellation.clone();
    let mut writer = Some(tokio::spawn(async move {
        pump_remote_input(remote_input, &mut input, writer_task_cancel).await
    }));
    let command_deadline = Instant::now() + limits.timeout;
    let capture_limit = limits.max_output_bytes.clamp(1, STDERR_CAPTURE_MAX_BYTES);
    let mut captured_stderr = Vec::new();
    let mut stderr_truncated = false;
    let mut exit_code = None;
    let mut received_exit = false;
    let mut execution_error = None;
    loop {
        let event = tokio::select! {
            _ = cancellation.cancelled() => {
                execution_error = Some(AppError::cancelled("SSH 运维命令已取消"));
                break;
            }
            _ = sleep_until(command_deadline) => {
                cancellation.cancel();
                execution_error = Some(AppError::timeout("SSH 运维命令流"));
                break;
            }
            event = stream.next_event() => match event {
                Ok(event) => event,
                Err(error) => {
                    execution_error = Some(error);
                    break;
                }
            }
        };
        let Some(event) = event else {
            if !received_exit {
                execution_error = Some(AppError::infrastructure("SSH 命令流在退出事件前结束"));
            }
            break;
        };
        let stream_event = match event {
            CommandEvent::Stdout(bytes) => Some(CommandStreamEvent::Stdout(bytes)),
            CommandEvent::Stderr(bytes) => {
                append_stderr_tail(
                    &mut captured_stderr,
                    &bytes,
                    capture_limit,
                    &mut stderr_truncated,
                );
                Some(CommandStreamEvent::Stderr(bytes))
            }
            CommandEvent::Exited { code, .. } => {
                if received_exit {
                    execution_error = Some(AppError::infrastructure("SSH 命令流重复发送退出状态"));
                    break;
                }
                received_exit = true;
                exit_code = code.and_then(|value| i32::try_from(value).ok());
                None
            }
        };
        if let Some(event) = stream_event
            && let Err(error) = send_output(&output, event, &cancellation, command_deadline).await
        {
            execution_error = Some(error);
            break;
        }
    }

    if execution_error.is_some() {
        cancellation.cancel();
    }
    cancellation.cancel();
    // channel 已停止产出；关闭 sender 使 finish 能在清理等待期间观察到 EOF 与状态变化。
    drop(output);
    let mut cleanup_attempt = 0_u64;
    loop {
        let deadline = resolve_cleanup_deadline(&mut cleanup_deadline);
        // SSH channel 与独立 lease 并行关闭；失败时两侧真实 owner 留在 worker 重试。
        let (stream_close, lease_close) = tokio::join!(
            stream.close_until(deadline),
            lease.close_until(deadline, "stream command complete"),
        );
        match combine_cleanup(stream_close, lease_close) {
            Ok(()) => {
                if let Some(writer_handle) = writer.as_mut() {
                    match timeout_at(deadline, join_remote_writer(writer_handle)).await {
                        Ok(()) => {
                            writer.take();
                            break;
                        }
                        Err(_) => {
                            publish_cleanup_pending(
                                &completion_state,
                                &mut cleanup_attempt,
                                AppError::timeout("SSH 运维命令 stdin worker 回收"),
                            );
                            wait_for_cleanup_retry(&mut cleanup_deadline).await;
                        }
                    }
                } else {
                    break;
                }
            }
            Err(error) => {
                publish_cleanup_pending(&completion_state, &mut cleanup_attempt, error);
                wait_for_cleanup_retry(&mut cleanup_deadline).await;
            }
        }
    }
    let execution = if let Some(error) = execution_error {
        Err(error)
    } else if received_exit {
        Ok(CommandStreamExit {
            exit_code,
            stderr: captured_stderr,
            stderr_truncated,
            input_complete: true,
        })
    } else {
        Err(AppError::infrastructure("SSH 命令流缺少退出状态"))
    };
    completion(execution, Ok(()))
}

/// 将 stdin 消息映射到独立 SSH writer；取消可中断写入和半关闭，reader 仍负责 drain 到 EOF。
///
/// @author kongweiguang
async fn pump_remote_input(
    remote_input: kerminal_application::connections::CommandInputHandle,
    input: &mut mpsc::Receiver<CommandStreamInputMessage>,
    cancellation: CancellationToken,
) -> AppResult<()> {
    loop {
        let message = tokio::select! {
            _ = cancellation.cancelled() => return Err(AppError::cancelled("SSH 命令 stdin 已取消")),
            message = input.recv() => message,
        };
        match message {
            Some(CommandStreamInputMessage::Data {
                bytes,
                acknowledgement,
            }) => {
                let result = tokio::select! {
                    _ = cancellation.cancelled() => return Err(AppError::cancelled("SSH 命令 stdin 已取消")),
                    result = remote_input.write_stdin(bytes) => result,
                };
                let _ = acknowledgement.send(result.clone());
                result?;
            }
            Some(CommandStreamInputMessage::Close { acknowledgement }) => {
                let result = tokio::select! {
                    _ = cancellation.cancelled() => Err(AppError::cancelled("SSH 命令 stdin 关闭已取消")),
                    result = remote_input.close_stdin() => result,
                };
                let _ = acknowledgement.send(result.clone());
                return result;
            }
            None => {
                return tokio::select! {
                    _ = cancellation.cancelled() => Err(AppError::cancelled("SSH 命令 stdin 关闭已取消")),
                    result = remote_input.close_stdin() => result,
                };
            }
        }
    }
}

/// 只在 stderr 回执中留最近尾部；全部字节仍完整发送给 UI 或传输 consumer。
fn append_stderr_tail(captured: &mut Vec<u8>, bytes: &[u8], limit: usize, truncated: &mut bool) {
    if bytes.len() >= limit {
        captured.clear();
        captured.extend_from_slice(&bytes[bytes.len() - limit..]);
        *truncated = true;
        return;
    }
    let excess = captured
        .len()
        .saturating_add(bytes.len())
        .saturating_sub(limit);
    if excess > 0 {
        captured.drain(..excess);
        *truncated = true;
    }
    captured.extend_from_slice(bytes);
}

/// 同时监听取消和命令绝对 deadline，队列背压不能遮住命令超时。
///
/// @author kongweiguang
async fn send_output(
    sender: &mpsc::Sender<CommandStreamEvent>,
    event: CommandStreamEvent,
    cancellation: &CancellationToken,
    command_deadline: Instant,
) -> AppResult<()> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(AppError::cancelled("运维命令输出消费已取消")),
        _ = sleep_until(command_deadline) => Err(AppError::timeout("运维命令输出队列")),
        result = sender.send(event) => result.map_err(|_| AppError::cancelled("运维命令输出端已关闭")),
    }
}

/// 初始化管道失败时持有 managed child 与进程树，只有明确 wait/reap 后才释放 worker permit。
async fn retry_child_cleanup(
    child: &mut ManagedTokioChild,
    cleanup_deadline: &mut watch::Receiver<Option<Instant>>,
    completion_state: &watch::Sender<CommandStreamState>,
) -> AppResult<()> {
    let mut attempt = 0_u64;
    loop {
        let deadline = resolve_cleanup_deadline(cleanup_deadline);
        let cleanup = async {
            child
                .terminate_tree_until(deadline)
                .await
                .map_err(|source| {
                    AppError::infrastructure_with_source("本机命令进程树终止失败", source)
                })?;
            timeout_at(deadline, child.wait())
                .await
                .map_err(|_| AppError::timeout("本机命令子进程回收"))?
                .map_err(|source| {
                    AppError::infrastructure_with_source("本机运维命令回收失败", source)
                })?;
            Ok::<(), AppError>(())
        };
        match cleanup.await {
            Ok(()) => return Ok(()),
            Err(error) => {
                publish_cleanup_pending(completion_state, &mut attempt, error);
                wait_for_cleanup_retry(cleanup_deadline).await;
            }
        }
    }
}

/// 清理过程始终借用同一 child、两条 pipe 与 stdin worker，不转移资源 owner。
struct LocalCleanupResources<'a, Stdout, Stderr> {
    child: &'a mut ManagedTokioChild,
    stdout: &'a mut Stdout,
    stderr: &'a mut Stderr,
    stdout_done: &'a mut bool,
    stderr_done: &'a mut bool,
    status: &'a mut Option<std::process::ExitStatus>,
    writer: &'a mut Option<tokio::task::JoinHandle<AppResult<()>>>,
}

/// 按单次绝对 deadline 清理同一组 child/pipe/writer owner，失败后由原 worker 安全重试。
///
/// @author kongweiguang
async fn cleanup_local_resources<Stdout, Stderr>(
    resources: &mut LocalCleanupResources<'_, Stdout, Stderr>,
    terminate_tree: bool,
    deadline: Instant,
) -> AppResult<Vec<AppError>>
where
    Stdout: AsyncRead + Unpin,
    Stderr: AsyncRead + Unpin,
{
    let child = &mut *resources.child;
    let stdout = &mut *resources.stdout;
    let stderr = &mut *resources.stderr;
    let stdout_done = &mut *resources.stdout_done;
    let stderr_done = &mut *resources.stderr_done;
    let status = &mut *resources.status;
    let writer = &mut *resources.writer;
    let mut diagnostics = Vec::new();
    if terminate_tree {
        child
            .terminate_tree_until(deadline)
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("本机命令进程树终止失败", source)
            })?;
    }
    if status.is_none() {
        *status = Some(
            timeout_at(deadline, child.wait())
                .await
                .map_err(|_| AppError::timeout("本机命令子进程回收"))?
                .map_err(|source| {
                    AppError::infrastructure_with_source("本机运维命令回收失败", source)
                })?,
        );
    }
    if !*stdout_done {
        match timeout_at(deadline, drain_pipe(stdout)).await {
            Err(_) => return Err(AppError::timeout("本机 stdout 管道回收")),
            Ok(Ok(())) => *stdout_done = true,
            Ok(Err(error)) => {
                diagnostics.push(error);
                *stdout_done = true;
            }
        }
    }
    if !*stderr_done {
        match timeout_at(deadline, drain_pipe(stderr)).await {
            Err(_) => return Err(AppError::timeout("本机 stderr 管道回收")),
            Ok(Ok(())) => *stderr_done = true,
            Ok(Err(error)) => {
                diagnostics.push(error);
                *stderr_done = true;
            }
        }
    }
    if let Some(handle) = writer.as_mut() {
        let result = timeout_at(deadline, join_local_writer(handle))
            .await
            .map_err(|_| AppError::timeout("本机命令 stdin worker 回收"))?;
        writer.take();
        if let Err(error) = result {
            diagnostics.push(error);
        }
    }
    Ok(diagnostics)
}

/// 每次清理失败都发布 attempt 回执，调用方可据此保留资源 owner 并提交下一条 deadline。
fn publish_cleanup_pending(
    sender: &watch::Sender<CommandStreamState>,
    attempt: &mut u64,
    error: AppError,
) {
    *attempt = attempt.saturating_add(1);
    sender.send_replace(CommandStreamState::CleanupPending {
        attempt: *attempt,
        error,
    });
}

/// 资源清理失败后保持 worker/permit 存活，直到调用方或应用 shutdown 提交新绝对 deadline。
async fn wait_for_cleanup_retry(deadline: &mut watch::Receiver<Option<Instant>>) {
    if deadline.changed().await.is_err() {
        tokio::time::sleep(CLEANUP_FALLBACK).await;
    }
}

/// 提前取消后继续 drain 已关闭进程遗留的管道，防止子进程退出等待被 pipe 占用。
async fn drain_pipe<R>(reader: &mut R) -> AppResult<()>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0; OUTPUT_CHUNK_BYTES];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(source) => {
                return Err(AppError::infrastructure_with_source(
                    "运维命令输出管道回收失败",
                    source,
                ));
            }
        }
    }
}

/// 回收 stdin worker；取消是清理路径预期结果，其它写入或 Join 失败保留为诊断。
///
/// @author kongweiguang
async fn join_local_writer(writer: &mut tokio::task::JoinHandle<AppResult<()>>) -> AppResult<()> {
    match writer.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) if error.public_code() == PublicErrorCode::Cancelled => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(source) => Err(AppError::infrastructure_with_source(
            "本机运维命令 stdin worker 未正常结束",
            source,
        )),
    }
}

/// SSH 输入 pump 错误不阻断独立 channel/lease 关闭；日志只记录静态错误类别，不输出动态来源。
async fn join_remote_writer(writer: &mut tokio::task::JoinHandle<AppResult<()>>) {
    match writer.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) if error.public_code() == PublicErrorCode::Cancelled => {}
        Ok(Err(error)) => {
            tracing::warn!(
                error_code = error.public_code().as_str(),
                "SSH stdin pump 已终结并返回写入错误"
            );
        }
        Err(source) => {
            let failure_kind = if source.is_cancelled() {
                "cancelled"
            } else if source.is_panic() {
                "panic"
            } else {
                "other"
            };
            tracing::warn!(failure_kind, "SSH stdin pump 已终结但 join 返回异常");
        }
    }
}

/// 若调用方尚未提供 deadline，普通命令错误路径使用固定回收预算；shutdown deadline 原样传递。
fn resolve_cleanup_deadline(deadline: &mut watch::Receiver<Option<Instant>>) -> Instant {
    deadline
        .borrow_and_update()
        .unwrap_or_else(|| Instant::now() + CLEANUP_FALLBACK)
}

/// 执行完成与清理结果分开回传，允许 close_until 忽略命令退出码但不能忽略回收失败。
fn completion(
    execution: AppResult<CommandStreamExit>,
    cleanup: AppResult<()>,
) -> CommandStreamCompletion {
    CommandStreamCompletion { execution, cleanup }
}

/// 同时保留 stream close、lease close 与 writer join 中发生的错误。
fn combine_cleanup<T>(result: AppResult<T>, cleanup: AppResult<()>) -> AppResult<T> {
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(operation), Err(cleanup)) => Err(operation.with_source(cleanup)),
    }
}

/// 包装本机 CLI 启动错误时保留 Infrastructure code 与原始 source，只细化可安全确认的固定提示。
///
/// @author kongweiguang
fn local_spawn_failure(program: &str, has_working_directory: bool, source: io::Error) -> AppError {
    let context = local_spawn_failure_context(program, has_working_directory, source.kind());
    AppError::infrastructure_with_source(context, source)
}

/// 仅将无自定义工作目录歧义的 Docker/Podman 本机 CLI 错误映射为可恢复提示。
///
/// cwd、自定义可执行文件及其它错误继续使用原提示，且不输出命令参数或本机路径。
///
/// @author kongweiguang
fn local_spawn_failure_context(
    program: &str,
    has_working_directory: bool,
    error_kind: io::ErrorKind,
) -> &'static str {
    if has_working_directory {
        return "本机运维命令启动失败";
    }

    let (missing, denied) = match program {
        "docker" => (
            "无法找到或启动本机 Docker CLI（docker），请确认已安装且命令位于 PATH 中",
            "本机 Docker CLI（docker）启动被拒绝，请检查可执行权限或系统安全策略",
        ),
        "podman" => (
            "无法找到或启动本机 Podman CLI（podman），请确认已安装且命令位于 PATH 中",
            "本机 Podman CLI（podman）启动被拒绝，请检查可执行权限或系统安全策略",
        ),
        _ => return "本机运维命令启动失败",
    };

    match error_kind {
        io::ErrorKind::NotFound => missing,
        io::ErrorKind::PermissionDenied => denied,
        _ => "本机运维命令启动失败",
    }
}

#[cfg(test)]
#[path = "../../tests/support/command_spawn_error_context.rs"]
mod command_spawn_error_context_tests;
