//! 本机到容器的 tar 流式上传与逐项目标提交。
//!
//! @author kongweiguang

use std::{
    io::{self, Write},
    path::Path,
    sync::Arc,
    time::Duration,
};

use kerminal_application::{
    AppError, AppResult,
    operations::{
        CommandExecutionLimits, CommandStreamEvent, ContainerStageCleanupRequest,
        OperationsCommandExecutor, StageCreationResult,
    },
    transfers::{
        TransferCleanupState, TransferExecutionContext, TransferExecutionError,
        TransferExecutionFailure, TransferExecutionResult, TransferPhase,
    },
};
use kerminal_domain::files::{ConflictPolicy, FileEndpoint, TransferFailureKind, TransferKind};
use tokio::{
    sync::{Mutex, mpsc},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use super::{
    archive::build_local_archive,
    backend::{
        ContainerPostStreamCleanup, ContainerStageCleanupOwner, ContainerStreamFailure,
        RemoteObjectKind, app_failure, app_failure_with_cleanup, cleanup_stage_capability,
        close_failed_stream_with_cleanup, container_invocation, ensure_same_destination_kind,
        ensure_success, execute_control, probe_container, remote_parent_name, report_progress,
        select_remote_destination, stage_owner_marker, with_cleanup_error,
    },
    model::{LocalManifest, collect_local_manifest, joined_remote_path, remote_child},
};

const ARCHIVE_BUFFER_CHUNKS: usize = 8;
const ARCHIVE_CHUNK_BYTES: usize = 32 * 1024;
const STAGE_SCRIPT: &str = "set -eu; stage=$1; owner=$2; marker=\"$stage/.kerminal-owner\"; [ -d \"$stage\" ] && [ ! -L \"$stage\" ] || exit 40; [ -f \"$marker\" ] && [ ! -L \"$marker\" ] && [ \"$(cat -- \"$marker\")\" = \"$owner\" ] || exit 41; [ ! -e \"$stage/payload\" ] && [ ! -L \"$stage/payload\" ] || exit 42; mkdir -- \"$stage/payload\"; tar -x -f - -C \"$stage/payload\" --no-same-owner --no-same-permissions";
const MKDIR_SCRIPT: &str = "set -eu; d=$1; if [ -L \"$d\" ]; then exit 42; elif [ -d \"$d\" ]; then exit 43; elif [ -e \"$d\" ]; then exit 41; else mkdir -- \"$d\"; fi";
const COMMIT_FILE_SCRIPT: &str = "set -eu; s=$1; d=$2; overwrite=$3; [ -f \"$s\" ] && [ ! -L \"$s\" ] || exit 40; if [ -L \"$d\" ]; then exit 42; fi; if [ -e \"$d\" ]; then [ \"$overwrite\" = yes ] && [ -f \"$d\" ] || exit 41; fi; if [ \"$overwrite\" = yes ]; then mv -fT -- \"$s\" \"$d\"; else mv -nT -- \"$s\" \"$d\"; [ ! -e \"$s\" ] || exit 41; fi";

/// 打包源、staging owner 和提交目录作为同一发送请求，避免归档身份在参数间错配。
struct ArchiveSendRequest<'a> {
    endpoint: &'a FileEndpoint,
    owner: &'a str,
    post_stream_cleanup: Arc<dyn ContainerPostStreamCleanup>,
    source: &'a Path,
    kind: TransferKind,
    archive_name: &'a str,
}

/// 完成打包后经有界 stdin 写入容器 staging；数据提交屏障与可重试 staging 清理分开线性化。
///
/// @author kongweiguang
pub(super) async fn upload(
    commands: &dyn OperationsCommandExecutor,
    context: &TransferExecutionContext,
    source: &Path,
) -> Result<TransferExecutionResult, Box<TransferExecutionError>> {
    let manifest = collect_local_manifest(source, context.request.kind)
        .map_err(|error| app_failure(error, context.actual_destination.clone(), false))?;
    if context.cancellation.is_cancelled() {
        return Err(TransferExecutionError::Cancelled {
            checkpoint: None,
            actual_destination: context.actual_destination.clone(),
        }
        .into());
    }
    let requested = match &context.request.destination {
        endpoint @ FileEndpoint::Container { .. } => endpoint,
        _ => {
            return Err(app_failure(
                AppError::unsupported("容器上传目标类型无效"),
                context.actual_destination.clone(),
                false,
            )
            .into());
        }
    };
    if let Some(actual) = context.actual_destination.as_ref() {
        ensure_same_destination_kind(requested, actual)
            .map_err(|error| app_failure(error, Some(actual.clone()), false))?;
    }
    let Some(destination) = select_remote_destination(
        commands,
        requested,
        context.request.kind,
        context.request.conflict_policy,
        context.cancellation.clone(),
        context.actual_destination.as_ref(),
    )
    .await
    .map_err(|error| app_failure(error, context.actual_destination.clone(), true))?
    else {
        let destination = requested.clone();
        context
            .progress
            .begin_transfer(Some(manifest.total_bytes))
            .await
            .map_err(|error| app_failure(error, Some(destination.clone()), false))?;
        return Ok(TransferExecutionResult {
            actual_destination: destination,
            total_bytes: Some(0),
            skipped: true,
            unchanged: false,
        });
    };
    let (parent, _destination_name) = remote_parent_name(&destination)
        .map_err(|error| app_failure(error, Some(destination.clone()), false))?;
    let parent_endpoint = endpoint_at(&destination, parent.clone())
        .map_err(|error| app_failure(error, Some(destination.clone()), false))?;
    let parent_object = probe_container(
        commands,
        &parent_endpoint,
        parent.as_str(),
        context.cancellation.clone(),
    )
    .await
    .map_err(|error| app_failure(error, Some(destination.clone()), true))?;
    if parent_object.kind != RemoteObjectKind::Directory {
        return Err(app_failure(
            AppError::not_found("容器传输目标父目录不存在"),
            Some(destination),
            false,
        )
        .into());
    }
    let owner = stage_owner_marker(&context.transfer_id);
    let stage_name = format!(".kerminal-{owner}.stage");
    let stage_path = remote_child(&parent, &stage_name)
        .map_err(|error| app_failure(error, Some(destination.clone()), false))?;
    let stage_endpoint = endpoint_at(&destination, stage_path.clone())
        .map_err(|error| app_failure(error, Some(destination.clone()), false))?;
    let Some(archive_name) = source
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
    else {
        return Err(app_failure(
            AppError::invalid("本机传输源必须有可编码文件名"),
            Some(destination),
            false,
        )
        .into());
    };
    context
        .progress
        .begin_transfer(Some(manifest.total_bytes))
        .await
        .map_err(|error| app_failure(error, Some(destination.clone()), false))?;
    report_progress(
        context,
        0,
        Some(manifest.total_bytes),
        &destination,
        TransferPhase::Transferring,
    );

    let cleanup_capability = commands
        .reserve_container_stage_cleanup(ContainerStageCleanupRequest {
            transfer_id: context.transfer_id.clone(),
            staging_endpoint: stage_endpoint.clone(),
        })
        .map_err(|error| app_failure(error, Some(destination.clone()), true))?;
    match cleanup_capability
        .create_stage_until(cleanup_deadline(context))
        .await
    {
        Ok(StageCreationResult::Created) => {}
        Ok(StageCreationResult::AlreadyExists) => {
            return Err(app_failure(
                AppError::conflict("容器 staging 路径已被其他资源占用"),
                Some(destination),
                false,
            )
            .into());
        }
        Err(error) => {
            let (cleanup_result, cleanup_state) =
                cleanup_stage_capability(cleanup_capability, cleanup_deadline(context)).await;
            return Err(app_failure_with_cleanup(
                with_cleanup_error(error, cleanup_result),
                Some(destination),
                false,
                cleanup_state,
            )
            .into());
        }
    }
    let post_stream_cleanup: Arc<dyn ContainerPostStreamCleanup> =
        Arc::new(ContainerStageCleanupOwner::new(cleanup_capability.clone()));

    let stream_result = send_archive(
        commands,
        context,
        ArchiveSendRequest {
            endpoint: &stage_endpoint,
            owner: &owner,
            post_stream_cleanup,
            source,
            kind: context.request.kind,
            archive_name: &archive_name,
        },
    )
    .await;
    if let Err(failure) = stream_result {
        let ContainerStreamFailure {
            error,
            cleanup,
            retryable_after_cleanup,
        } = failure;
        let (error, cleanup) = if cleanup.is_released() {
            let (cleanup_result, cleanup_state) =
                cleanup_stage_capability(cleanup_capability.clone(), cleanup_deadline(context))
                    .await;
            (with_cleanup_error(error, cleanup_result), cleanup_state)
        } else {
            // command 仍在后台清理时，TransferService owner 会先 close stream 再清理 staging。
            (error, cleanup)
        };
        if retryable_after_cleanup {
            return Err(incomplete_input_failure(Some(destination), cleanup).into());
        }
        return Err(app_failure_with_cleanup(error, Some(destination), true, cleanup).into());
    }
    match collect_local_manifest(source, context.request.kind) {
        Ok(current) if current == manifest => {}
        Ok(_) => {
            let (cleanup_result, cleanup_state) =
                cleanup_stage_capability(cleanup_capability.clone(), cleanup_deadline(context))
                    .await;
            return Err(app_failure_with_cleanup(
                with_cleanup_error(
                    AppError::conflict("本机传输源在归档期间发生变化"),
                    cleanup_result,
                ),
                Some(destination),
                false,
                cleanup_state,
            )
            .into());
        }
        Err(error) => {
            let (cleanup_result, cleanup_state) =
                cleanup_stage_capability(cleanup_capability.clone(), cleanup_deadline(context))
                    .await;
            return Err(app_failure_with_cleanup(
                with_cleanup_error(error, cleanup_result),
                Some(destination),
                false,
                cleanup_state,
            )
            .into());
        }
    }
    report_progress(
        context,
        manifest.total_bytes,
        Some(manifest.total_bytes),
        &destination,
        TransferPhase::Committing,
    );

    let commit = commit_manifest(
        commands,
        context,
        &destination,
        &stage_path,
        &archive_name,
        &manifest,
    )
    .await;
    let changed = match commit {
        Ok(changed) => changed,
        Err(CommitStepError::Unknown) => {
            let (cleanup_result, cleanup_state) =
                cleanup_stage_capability(cleanup_capability.clone(), cleanup_deadline(context))
                    .await;
            let failure = unknown_commit_with_cleanup(Some(destination), cleanup_state);
            if let Err(cleanup_error) = cleanup_result {
                tracing::warn!(
                    error_code = cleanup_error.public_code().as_str(),
                    "容器提交结果未知，staging 清理仍待重试"
                );
            }
            return Err(failure.into());
        }
        Err(CommitStepError::Known(error)) => {
            let (cleanup_result, cleanup_state) =
                cleanup_stage_capability(cleanup_capability.clone(), cleanup_deadline(context))
                    .await;
            return Err(app_failure_with_cleanup(
                with_cleanup_error(error, cleanup_result),
                Some(destination),
                false,
                cleanup_state,
            )
            .into());
        }
    };
    if changed {
        context
            .progress
            .finish_commit()
            .await
            .map_err(|error| app_failure(error, Some(destination.clone()), false))?;
    } else if context.request.kind == TransferKind::Directory {
        context
            .progress
            .finish_no_changes()
            .await
            .map_err(|error| app_failure(error, Some(destination.clone()), false))?;
    } else {
        return Err(app_failure(
            AppError::infrastructure("文件传输计划没有目标提交项"),
            Some(destination),
            false,
        )
        .into());
    }
    let (cleanup_result, cleanup_state) =
        cleanup_stage_capability(cleanup_capability, cleanup_deadline(context)).await;
    if let Err(cleanup_error) = cleanup_result {
        let owner = match cleanup_state {
            TransferCleanupState::Pending(owner) => owner,
            TransferCleanupState::Released => {
                return Err(app_failure(cleanup_error, Some(destination.clone()), false).into());
            }
        };
        if let Err(retain_error) = context.progress.retain_cleanup_owner(owner.clone()).await {
            return Err(app_failure_with_cleanup(
                cleanup_error.with_source(retain_error),
                Some(destination.clone()),
                false,
                TransferCleanupState::pending(owner),
            )
            .into());
        }
        tracing::warn!(
            transfer_id = %context.transfer_id,
            error_code = cleanup_error.public_code().as_str(),
            "容器数据已提交，staging 清理待重试"
        );
    }
    Ok(TransferExecutionResult {
        actual_destination: destination,
        total_bytes: Some(manifest.total_bytes),
        skipped: false,
        unchanged: !changed,
    })
}

/// 启动 streaming worker 并并行推进 tar producer 与 stdin/output consumer，取消仍归原传输 owner。
/// finish 回执结束后释放 stream 锁，再进入可能重试同一 owner 的失败清理。
///
/// @author kongweiguang
async fn send_archive(
    commands: &dyn OperationsCommandExecutor,
    context: &TransferExecutionContext,
    request: ArchiveSendRequest<'_>,
) -> Result<(), ContainerStreamFailure> {
    let ArchiveSendRequest {
        endpoint,
        owner,
        post_stream_cleanup,
        source,
        kind,
        archive_name,
    } = request;
    let stage_path = container_path(endpoint).map_err(ContainerStreamFailure::released)?;
    let (target, invocation) =
        container_invocation(endpoint, STAGE_SCRIPT, vec![stage_path, owner.to_owned()])
            .map_err(ContainerStreamFailure::released)?;
    let stream = commands
        .execute_stream(
            target,
            invocation,
            CommandExecutionLimits {
                timeout: super::backend::STREAM_TIMEOUT,
                max_output_bytes: 64 * 1024,
            },
            context.cancellation.clone(),
        )
        .await
        .map_err(ContainerStreamFailure::released)?;
    let stream = Arc::new(Mutex::new(stream));
    let input = stream.lock().await.input();
    let (sender, mut receiver) = mpsc::channel::<Vec<u8>>(ARCHIVE_BUFFER_CHUNKS);
    let producer_source = source.to_path_buf();
    let producer_name = archive_name.to_owned();
    let producer = tokio::task::spawn_blocking(move || {
        build_local_archive(
            &producer_source,
            kind,
            &producer_name,
            ChannelWriter { sender },
            |_| {},
        )
    });
    let mut producer = Some(producer);
    let mut producer_error = None;
    let mut input_error = None;
    let mut output_finished = false;
    loop {
        if output_finished {
            break;
        }
        tokio::select! {
            _ = context.cancellation.cancelled() => {
                drop(receiver);
                let producer_cleanup = join_archive_producer(producer.take()).await;
                let failure = close_failed_stream_with_cleanup(
                    stream.clone(),
                    AppError::cancelled("容器归档上传已取消"),
                    cleanup_deadline(context),
                    Some(post_stream_cleanup.clone()),
                ).await;
                let ContainerStreamFailure {
                    error, cleanup, ..
                } = failure;
                return Err(ContainerStreamFailure {
                    error: with_cleanup_error(error, producer_cleanup),
                    cleanup,
                    retryable_after_cleanup: false,
                });
            }
            chunk = receiver.recv(), if producer.is_some() => match chunk {
                Some(bytes) => {
                    if let Err(error) = input.write(bytes).await {
                        input_error = Some(error);
                        break;
                    }
                }
                None => {
                    let result = join_archive_producer(producer.take()).await;
                    producer_error = result.err();
                    let input_close = input.close().await;
                    input_error = input_close.err();
                }
            },
            event = async { stream.lock().await.next_event().await } => match event {
                Ok(Some(CommandStreamEvent::Stdout(_))) | Ok(Some(CommandStreamEvent::Stderr(_))) => {},
                Ok(None) => { output_finished = true; }
                Err(error) => { producer_error = Some(error); break; }
            },
        }
    }
    if !output_finished {
        drop(receiver);
        let producer_cleanup = join_archive_producer(producer.take()).await;
        let error = input_error
            .take()
            .or_else(|| producer_error.take())
            .unwrap_or_else(|| AppError::infrastructure("容器 tar 命令提前结束"));
        let failure = close_failed_stream_with_cleanup(
            stream.clone(),
            error,
            cleanup_deadline(context),
            Some(post_stream_cleanup.clone()),
        )
        .await;
        let ContainerStreamFailure { error, cleanup, .. } = failure;
        return Err(ContainerStreamFailure {
            error: with_cleanup_error(error, producer_cleanup),
            cleanup,
            retryable_after_cleanup: false,
        });
    }
    if producer.is_some() {
        drop(receiver);
        let producer_cleanup = join_archive_producer(producer.take()).await;
        let error = producer_error
            .unwrap_or_else(|| AppError::infrastructure("容器 tar 在上传输入完成前退出"));
        let failure = close_failed_stream_with_cleanup(
            stream.clone(),
            error,
            cleanup_deadline(context),
            Some(post_stream_cleanup.clone()),
        )
        .await;
        let ContainerStreamFailure { error, cleanup, .. } = failure;
        return Err(ContainerStreamFailure {
            error: with_cleanup_error(error, producer_cleanup),
            cleanup,
            retryable_after_cleanup: false,
        });
    }
    // 结束回执可能失败并需要再次锁定同一个 stream 清理；先结束锁的作用域，避免错误分支自锁。
    let finish_result = {
        let mut guard = stream.lock().await;
        guard.finish().await
    };
    let exit = match finish_result {
        Ok(exit) => exit,
        Err(error) => {
            return Err(close_failed_stream_with_cleanup(
                stream.clone(),
                error,
                cleanup_deadline(context),
                Some(post_stream_cleanup.clone()),
            )
            .await);
        }
    };
    if let Some(error) = producer_error {
        return Err(ContainerStreamFailure::released(error));
    }
    if exit.exit_code != Some(0) {
        return Err(ContainerStreamFailure::released(stream_exit_error(
            exit.exit_code,
        )));
    }
    if !exit.input_complete {
        tracing::warn!(
            exit_code = ?exit.exit_code,
            stderr_tail_bytes = exit.stderr.len(),
            stderr_truncated = exit.stderr_truncated,
            "容器 tar 命令成功退出，但 stdin 输入回执不完整"
        );
        let error = input_error.unwrap_or_else(|| {
            AppError::infrastructure("容器 tar 输入未完整送达；目标提交尚未开始")
        });
        return Err(close_failed_stream_with_cleanup(
            stream,
            error,
            cleanup_deadline(context),
            Some(post_stream_cleanup),
        )
        .await
        .retryable_after_cleanup());
    }
    if let Some(error) = input_error {
        return Err(ContainerStreamFailure::released(error));
    }
    Ok(())
}

/// 在目标提交前交付回执缺失时创建可重试失败；队列仍会先拒绝尚有 cleanup owner 的重试。
fn incomplete_input_failure(
    destination: Option<FileEndpoint>,
    cleanup: TransferCleanupState,
) -> TransferExecutionError {
    TransferExecutionError::Failed(
        TransferExecutionFailure::new(
            TransferFailureKind::Connection,
            true,
            false,
            None,
            destination,
            cleanup,
        )
        .with_safe_message("容器归档输入未确认完整送达；清理 staging 后可显式重试"),
    )
}

/// 将 tar 写入拆为小块，以固定内存上限在 blocking producer 与异步 SSH/CLI writer 间传递。
struct ChannelWriter {
    sender: mpsc::Sender<Vec<u8>>,
}

impl Write for ChannelWriter {
    /// 有界分块发送；consumer 取消或命令提前退出后向 tar reader 返回 BrokenPipe。
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        for chunk in buffer.chunks(ARCHIVE_CHUNK_BYTES) {
            self.sender
                .blocking_send(chunk.to_vec())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "tar consumer stopped"))?;
        }
        Ok(buffer.len())
    }

    /// mpsc 在 send 成功时已把内容交给唯一 consumer，无额外用户态缓冲需要 flush。
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Join blocking tar 遍历并将 I/O 与 worker panic 映射为固定安全错误。
async fn join_archive_producer(producer: Option<JoinHandle<io::Result<()>>>) -> AppResult<()> {
    let Some(producer) = producer else {
        return Ok(());
    };
    producer
        .await
        .map_err(|source| {
            AppError::infrastructure_with_source("本机 tar producer 未正常退出", source)
        })?
        .map_err(|source| AppError::infrastructure_with_source("读取本机归档源失败", source))
}

/// 对目录项逐一申请不可取消提交许可，文件以同目录 mv 原子发布，目录只创建缺失节点。
async fn commit_manifest(
    commands: &dyn OperationsCommandExecutor,
    context: &TransferExecutionContext,
    destination: &FileEndpoint,
    stage_path: &kerminal_domain::files::RemotePath,
    archive_name: &str,
    manifest: &LocalManifest,
) -> Result<bool, CommitStepError> {
    let target = super::model::container_endpoint(destination).map_err(CommitStepError::Known)?;
    let mut changed = false;
    for entry in &manifest.entries {
        if context.cancellation.is_cancelled() {
            return Err(CommitStepError::Known(AppError::cancelled(
                "容器目录传输已取消",
            )));
        }
        let destination_path =
            path_with_relative(target.path, &entry.relative).map_err(CommitStepError::Known)?;
        let item_endpoint =
            endpoint_at(destination, destination_path.clone()).map_err(CommitStepError::Known)?;
        if entry.directory {
            let existing = probe_container(
                commands,
                &item_endpoint,
                destination_path.as_str(),
                context.cancellation.clone(),
            )
            .await
            .map_err(CommitStepError::Known)?;
            match existing.kind {
                RemoteObjectKind::Directory => continue,
                RemoteObjectKind::Missing => {}
                _ => {
                    return Err(CommitStepError::Known(AppError::conflict(
                        "容器目录项目标类型发生冲突",
                    )));
                }
            }
            context
                .progress
                .begin_commit(
                    destination.clone(),
                    manifest.total_bytes,
                    Some(manifest.total_bytes),
                    None,
                )
                .await
                .map_err(CommitStepError::Known)?;
            let result = execute_control(
                commands,
                destination,
                MKDIR_SCRIPT,
                vec![destination_path.as_str().to_owned()],
                CancellationToken::new(),
            )
            .await
            .map_err(|_| CommitStepError::Unknown)?;
            if result.exit_code == Some(43) {
                context
                    .progress
                    .confirm_no_change()
                    .await
                    .map_err(CommitStepError::Known)?;
                continue;
            } else if ensure_success(&result).is_err() {
                return Err(CommitStepError::Known(AppError::conflict(
                    "创建容器目录项失败",
                )));
            }
            context
                .progress
                .record_committed_entry(0)
                .await
                .map_err(|_| CommitStepError::Unknown)?;
            changed = true;
            continue;
        }
        let payload_path = remote_child(stage_path, "payload").map_err(CommitStepError::Known)?;
        let staged_path = joined_remote_path(&payload_path, archive_name, &entry.relative)
            .map_err(CommitStepError::Known)?;
        context
            .progress
            .begin_commit(
                destination.clone(),
                manifest.total_bytes,
                Some(manifest.total_bytes),
                None,
            )
            .await
            .map_err(CommitStepError::Known)?;
        let overwrite = context.request.conflict_policy == ConflictPolicy::Overwrite;
        let result = execute_control(
            commands,
            destination,
            COMMIT_FILE_SCRIPT,
            vec![
                staged_path.as_str().to_owned(),
                destination_path.as_str().to_owned(),
                if overwrite { "yes" } else { "no" }.to_owned(),
            ],
            CancellationToken::new(),
        )
        .await
        .map_err(|_| CommitStepError::Unknown)?;
        if let Err(error) = ensure_success(&result) {
            return if matches!(result.exit_code, Some(40 | 41 | 42 | 127)) {
                Err(CommitStepError::Known(error))
            } else {
                Err(CommitStepError::Unknown)
            };
        }
        context
            .progress
            .record_committed_entry(entry.bytes)
            .await
            .map_err(|_| CommitStepError::Unknown)?;
        changed = true;
    }
    Ok(changed)
}

/// 由容器 endpoint 更换 path 而保留 host/runtime/container 身份。
fn endpoint_at(
    endpoint: &FileEndpoint,
    path: kerminal_domain::files::RemotePath,
) -> Result<FileEndpoint, AppError> {
    let FileEndpoint::Container {
        host_id,
        runtime,
        container_id,
        ..
    } = endpoint
    else {
        return Err(AppError::unsupported("容器传输端点类型无效"));
    };
    Ok(FileEndpoint::Container {
        host_id: host_id.clone(),
        runtime: *runtime,
        container_id: container_id.clone(),
        path,
    })
}

/// 将 manifest 相对项安全追加到目标根路径，文件系统段已由本机扫描和 tar validator 校验。
fn path_with_relative(
    base: &kerminal_domain::files::RemotePath,
    relative: &[String],
) -> AppResult<kerminal_domain::files::RemotePath> {
    let mut path = base.clone();
    for name in relative {
        path = remote_child(&path, name)?;
    }
    Ok(path)
}

/// 清理与命令必须共享队列绝对 deadline；普通传输使用短固定预算完成取消回收。
fn cleanup_deadline(context: &TransferExecutionContext) -> Instant {
    context
        .shutdown_deadline()
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(5))
}

/// 对远端目标提交命令错误做确定退出码映射；其它返回丢失都可能已发生 rename。
enum CommitStepError {
    Known(AppError),
    Unknown,
}

/// 只把容器文件 endpoint 转为 argv 文本，不使用宿主平台路径规则。
fn container_path(endpoint: &FileEndpoint) -> AppResult<String> {
    let FileEndpoint::Container { path, .. } = endpoint else {
        return Err(AppError::unsupported("容器 staging endpoint 类型无效"));
    };
    Ok(path.as_str().to_owned())
}

/// 提交回执未知时禁止自动重试，但仍把 staging capability 交给 cleanup 队列。
fn unknown_commit_with_cleanup(
    destination: Option<FileEndpoint>,
    cleanup: TransferCleanupState,
) -> TransferExecutionError {
    TransferExecutionError::Failed(TransferExecutionFailure::new(
        TransferFailureKind::CommitUnknown,
        false,
        false,
        None,
        destination,
        cleanup,
    ))
}

/// 转换 tar 命令的明确非零退出；缺失状态与普通 remote exit 分别保持可见。
fn stream_exit_error(code: Option<i32>) -> AppError {
    match code {
        Some(40) => AppError::not_found("容器传输源不存在"),
        Some(41) => AppError::conflict("容器 staging 路径已被占用"),
        Some(127) => AppError::unsupported("容器需要 POSIX sh 与 tar"),
        _ => AppError::remote_exit(code),
    }
}
