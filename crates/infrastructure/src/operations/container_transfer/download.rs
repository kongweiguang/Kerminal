//! 容器到本机的 tar 流式下载、受限 staging 与原子逐项合并。
//!
//! @author kongweiguang

use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    operations::{CommandStreamEvent, OperationsCommandExecutor},
    transfers::{
        TransferCleanupState, TransferExecutionContext, TransferExecutionError,
        TransferExecutionFailure, TransferExecutionResult, TransferPhase, TransferResourceCleanup,
    },
};
use kerminal_domain::files::{FileEndpoint, TransferFailureKind, TransferKind};
use tokio::{
    sync::{Mutex, mpsc},
    task::JoinHandle,
    time::{Instant, timeout_at},
};

use super::{
    archive::extract_local_archive_into_existing,
    backend::{
        ContainerPostStreamCleanup, ContainerStreamFailure, app_failure, app_failure_with_cleanup,
        close_failed_stream_with_cleanup, open_stream, probe_container, remote_parent_name,
        report_progress, select_local_destination, unknown_commit, validate_remote_source,
        with_cleanup_error,
    },
    local_commit::{LocalCommitError, commit_local_action, plan_local_archive},
    model::{container_endpoint, validate_archive_name},
};

const ARCHIVE_BUFFER_CHUNKS: usize = 8;
const STREAM_SCRIPT: &str = "set -eu; p=$1; parent=$2; name=$3; [ ! -L \"$p\" ] || exit 42; if [ -f \"$p\" ]; then :; elif [ -d \"$p\" ]; then :; else exit 40; fi; tar -c -f - -C \"$parent\" -- \"$name\"";

/// 一次下载归档的可信源、私有 staging 和提交目标绑定在一起，避免参数错配。
struct ArchiveReceiveRequest<'a> {
    endpoint: &'a FileEndpoint,
    source_parent: &'a kerminal_domain::files::RemotePath,
    source_name: &'a str,
    staging_root: &'a Path,
    kind: TransferKind,
    destination: &'a FileEndpoint,
    total_bytes: Option<u64>,
}

/// 经容器 tar stdout 流式落入本机私有 staging，校验通过后按文件原子合并到目标树。
///
/// @author kongweiguang
pub(super) async fn download(
    commands: &dyn OperationsCommandExecutor,
    context: &TransferExecutionContext,
    requested_path: &Path,
) -> Result<TransferExecutionResult, Box<TransferExecutionError>> {
    let source = match &context.request.source {
        endpoint @ FileEndpoint::Container { .. } => endpoint,
        _ => {
            return Err(app_failure(
                AppError::unsupported("容器下载源类型无效"),
                context.actual_destination.clone(),
                false,
            )
            .into());
        }
    };
    let source_target = container_endpoint(source)
        .map_err(|error| app_failure(error, context.actual_destination.clone(), false))?;
    let (source_parent, source_name) = remote_parent_name(source)
        .map_err(|error| app_failure(error, context.actual_destination.clone(), false))?;
    validate_archive_name(&source_name)
        .map_err(|error| app_failure(error, context.actual_destination.clone(), false))?;
    let source_metadata = probe_container(
        commands,
        source,
        source_target.path.as_str(),
        context.cancellation.clone(),
    )
    .await
    .map_err(|error| app_failure(error, context.actual_destination.clone(), true))?;
    let total_bytes = validate_remote_source(source_metadata, context.request.kind)
        .map_err(|error| app_failure(error, context.actual_destination.clone(), false))?;

    let Some(destination) = select_local_destination(
        requested_path,
        context.request.kind,
        context.request.conflict_policy,
        context.actual_destination.as_ref(),
    )
    .map_err(|error| app_failure(error, context.actual_destination.clone(), true))?
    else {
        let endpoint = context.request.destination.clone();
        context
            .progress
            .begin_transfer(total_bytes)
            .await
            .map_err(|error| app_failure(error, Some(endpoint.clone()), false))?;
        return Ok(TransferExecutionResult {
            actual_destination: endpoint,
            total_bytes: Some(0),
            skipped: true,
            unchanged: false,
        });
    };
    let destination_endpoint = FileEndpoint::Local {
        path: destination.clone(),
    };
    context
        .progress
        .begin_transfer(total_bytes)
        .await
        .map_err(|error| app_failure(error, Some(destination_endpoint.clone()), false))?;
    let owner = safe_owner(context.transfer_id.as_str());
    let staging_root = staging_root_for(&destination, &owner)
        .map_err(|error| app_failure(error, Some(destination_endpoint.clone()), false))?;
    fs::create_dir(&staging_root).map_err(|error| {
        app_failure(
            AppError::infrastructure_with_source("创建本机私有传输 staging 失败", error),
            Some(destination_endpoint.clone()),
            false,
        )
    })?;
    let transferred_bytes = match receive_archive(
        commands,
        context,
        ArchiveReceiveRequest {
            endpoint: source,
            source_parent: &source_parent,
            source_name: &source_name,
            staging_root: &staging_root,
            kind: context.request.kind,
            destination: &destination_endpoint,
            total_bytes,
        },
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(ContainerStreamFailure { error, cleanup, .. }) => {
            if !cleanup.is_released() {
                return Err(app_failure_with_cleanup(
                    error,
                    Some(destination_endpoint),
                    true,
                    cleanup,
                )
                .into());
            }
            return Err(cleanup_staging_failure(
                &staging_root,
                error,
                Some(destination_endpoint),
                true,
            )
            .await
            .into());
        }
    };
    let actions = match plan_local_archive(
        &staging_root,
        &source_name,
        &destination,
        context.request.kind,
        context.request.conflict_policy,
    ) {
        Ok(actions) => actions,
        Err(error) => {
            let failure = map_local_commit_error(error, Some(destination_endpoint.clone()), false);
            return Err(cleanup_staging_transfer_failure(
                &staging_root,
                failure,
                Some(destination_endpoint),
            )
            .await
            .into());
        }
    };
    if actions.is_empty() {
        if context.request.kind != TransferKind::Directory {
            return Err(cleanup_staging_failure(
                &staging_root,
                AppError::infrastructure("文件传输计划没有目标提交项"),
                Some(destination_endpoint),
                false,
            )
            .await
            .into());
        }
        cleanup_staging_before_success(&staging_root, &destination_endpoint).await?;
        context
            .progress
            .finish_no_changes()
            .await
            .map_err(|error| app_failure(error, Some(destination_endpoint.clone()), false))?;
        return Ok(TransferExecutionResult {
            actual_destination: destination_endpoint,
            total_bytes,
            skipped: false,
            unchanged: true,
        });
    }
    for action in &actions {
        if context.cancellation.is_cancelled() {
            return Err(cleanup_staging_failure(
                &staging_root,
                AppError::cancelled("容器文件下载已取消"),
                Some(destination_endpoint),
                false,
            )
            .await
            .into());
        }
        report_progress(
            context,
            transferred_bytes,
            total_bytes,
            &destination_endpoint,
            TransferPhase::Committing,
        );
        if let Err(error) = context
            .progress
            .begin_commit(
                destination_endpoint.clone(),
                transferred_bytes,
                total_bytes,
                None,
            )
            .await
        {
            return Err(cleanup_staging_failure(
                &staging_root,
                error,
                Some(destination_endpoint),
                false,
            )
            .await
            .into());
        }
        let action = action.clone();
        let owner_for_commit = owner.clone();
        let result =
            tokio::task::spawn_blocking(move || commit_local_action(&action, &owner_for_commit))
                .await
                .map_err(|source| {
                    AppError::infrastructure_with_source("本机文件提交 worker 未正常退出", source)
                });
        let committed_bytes = match result {
            Err(_) => return Err(unknown_commit(Some(destination_endpoint)).into()),
            Ok(Err(error @ LocalCommitError::Unknown(_))) => {
                return Err(
                    map_local_commit_error(error, Some(destination_endpoint), false).into(),
                );
            }
            Ok(Err(LocalCommitError::Known(error))) => {
                return Err(cleanup_staging_failure(
                    &staging_root,
                    AppError::infrastructure_with_source("本机传输目标提交失败", error),
                    Some(destination_endpoint),
                    false,
                )
                .await
                .into());
            }
            Ok(Ok(bytes)) => bytes,
        };
        if context
            .progress
            .record_committed_entry(committed_bytes)
            .await
            .is_err()
        {
            return Err(unknown_commit(Some(destination_endpoint)).into());
        }
    }
    cleanup_staging_before_success(&staging_root, &destination_endpoint).await?;
    context
        .progress
        .finish_commit()
        .await
        .map_err(|error| app_failure(error, Some(destination_endpoint.clone()), false))?;
    Ok(TransferExecutionResult {
        actual_destination: destination_endpoint,
        total_bytes,
        skipped: false,
        unchanged: false,
    })
}

/// 从容器 exec stdout 送入有界 blocking tar reader；请求保持源端、目标与 staging 的配对。
///
/// @author kongweiguang
async fn receive_archive(
    commands: &dyn OperationsCommandExecutor,
    context: &TransferExecutionContext,
    request: ArchiveReceiveRequest<'_>,
) -> Result<u64, ContainerStreamFailure> {
    let ArchiveReceiveRequest {
        endpoint,
        source_parent,
        source_name,
        staging_root,
        kind,
        destination,
        total_bytes,
    } = request;
    let stream = open_stream(
        commands,
        endpoint,
        STREAM_SCRIPT,
        vec![
            endpoint
                .remote_path()
                .ok_or_else(|| {
                    ContainerStreamFailure::released(AppError::invalid("容器源缺少远端路径"))
                })?
                .as_str()
                .to_owned(),
            source_parent.as_str().to_owned(),
            source_name.to_owned(),
        ],
        context.cancellation.clone(),
    )
    .await
    .map_err(ContainerStreamFailure::released)?;
    let stream = Arc::new(Mutex::new(stream));
    let (sender, receiver) = mpsc::channel(ARCHIVE_BUFFER_CHUNKS);
    let stage = staging_root.to_path_buf();
    let name = source_name.to_owned();
    let cancellation = context.clone();
    let destination = destination.clone();
    let bytes = Arc::new(AtomicU64::new(0));
    let byte_counter = bytes.clone();
    let extraction = tokio::task::spawn_blocking(move || {
        extract_local_archive_into_existing(
            ChannelReader::new(receiver),
            &stage,
            &name,
            kind,
            move |delta| {
                let confirmed = byte_counter
                    .fetch_add(delta, Ordering::Relaxed)
                    .saturating_add(delta);
                report_progress(
                    &cancellation,
                    confirmed,
                    total_bytes,
                    &destination,
                    TransferPhase::Transferring,
                );
            },
        )
    });
    let mut extraction = Some(extraction);
    let mut extraction_result = None;
    let mut sender = Some(sender);
    let mut output_finished = false;
    loop {
        if output_finished && extraction.is_none() {
            break;
        }
        tokio::select! {
            _ = context.cancellation.cancelled() => {
                drop(sender.take());
                let extract_result = join_extractor_if_running(extraction.take()).await;
                let error = with_cleanup_error(
                    AppError::cancelled("容器文件下载已取消"),
                    extract_result,
                );
                return Err(close_failed_stream_with_cleanup(
                    stream.clone(),
                    error,
                    cleanup_deadline(context),
                    Some(Arc::new(LocalStagingCleanup {
                        path: staging_root.to_path_buf(),
                    })),
                ).await);
            }
            event = async { stream.lock().await.next_event().await }, if !output_finished => match event {
                Ok(Some(CommandStreamEvent::Stdout(bytes))) => {
                    if extraction.is_some() {
                        let send_result = tokio::select! {
                            _ = context.cancellation.cancelled() => Err(AppError::cancelled("容器文件下载已取消")),
                            result = async {
                                match sender.as_ref() {
                                    Some(sender) => sender.send(bytes).await,
                                    None => Err(mpsc::error::SendError(Vec::new())),
                                }
                            } => result.map_err(|_| AppError::infrastructure("本机 tar 解包器提前结束")),
                        };
                        if let Err(error) = send_result {
                            drop(sender.take());
                            let extract_result = join_extractor_if_running(extraction.take()).await;
                            let error = with_cleanup_error(error, extract_result);
                            return Err(close_failed_stream_with_cleanup(
                                stream.clone(),
                                error,
                                cleanup_deadline(context),
                                Some(Arc::new(LocalStagingCleanup {
                                    path: staging_root.to_path_buf(),
                                })),
                            ).await);
                        }
                    }
                }
                Ok(Some(CommandStreamEvent::Stderr(_))) => {},
                Ok(None) => {
                    output_finished = true;
                    drop(sender.take());
                }
                Err(error) => {
                    drop(sender.take());
                    let extract_result = join_extractor_if_running(extraction.take()).await;
                    let error = with_cleanup_error(error, extract_result);
                    return Err(close_failed_stream_with_cleanup(
                        stream.clone(),
                        error,
                        cleanup_deadline(context),
                        Some(Arc::new(LocalStagingCleanup {
                            path: staging_root.to_path_buf(),
                        })),
                    ).await);
                }
            },
                result = async {
                    match extraction.as_mut() {
                        Some(extractor) => Some(extractor.await),
                        None => None,
                    }
                }, if extraction.is_some() => {
                extraction = None;
                    match result.map(map_extractor_result) {
                        Some(Ok(path)) => extraction_result = Some(path),
                        Some(Err(error)) => {
                            drop(sender.take());
                            return Err(close_failed_stream_with_cleanup(
                                stream.clone(),
                                error,
                                cleanup_deadline(context),
                                Some(Arc::new(LocalStagingCleanup {
                                    path: staging_root.to_path_buf(),
                                })),
                            ).await);
                        }
                        None => return Err(ContainerStreamFailure::released(AppError::infrastructure("本机 tar 解包 worker 缺少完成结果"))),
                    }
            },
        }
    }
    let exit = match stream.lock().await.finish().await {
        Ok(exit) => exit,
        Err(error) => {
            return Err(close_failed_stream_with_cleanup(
                stream.clone(),
                error,
                cleanup_deadline(context),
                Some(Arc::new(LocalStagingCleanup {
                    path: staging_root.to_path_buf(),
                })),
            )
            .await);
        }
    };
    if exit.exit_code != Some(0) {
        return Err(ContainerStreamFailure::released(stream_exit_error(
            exit.exit_code,
        )));
    }
    if extraction_result.is_none() {
        extraction_result = match join_extractor(extraction.take()).await {
            Ok(path) => Some(path),
            Err(error) => return Err(ContainerStreamFailure::released(error)),
        };
    }
    extraction_result.ok_or_else(|| {
        ContainerStreamFailure::released(AppError::infrastructure(
            "容器归档未产生本机 staging 对象",
        ))
    })?;
    Ok(bytes.load(Ordering::Relaxed))
}

/// 让传输队列在 stream 未关闭时暂存 staging 所有权，避免并发删除仍在写入的文件树。
struct LocalStagingCleanup {
    path: PathBuf,
}

#[async_trait]
impl ContainerPostStreamCleanup for LocalStagingCleanup {
    /// stream 已确认关闭后才删除本次私有目录；超时保留 owner 供队列后续重试。
    async fn cleanup_until(&self, deadline: Instant) -> AppResult<()> {
        self.remove_until(deadline).await
    }
}

#[async_trait]
impl TransferResourceCleanup for LocalStagingCleanup {
    /// 将本机私有 staging 作为真实 cleanup owner 保存到 transfer 队列，直到删除确认。
    async fn cleanup_until(&self, deadline: Instant) -> AppResult<()> {
        self.remove_until(deadline).await
    }
}

impl LocalStagingCleanup {
    /// 通过 blocking pool 删除私有树，并在 deadline 到期时保留同一个路径 owner。
    async fn remove_until(&self, deadline: Instant) -> AppResult<()> {
        let path = self.path.clone();
        let cleanup = tokio::task::spawn_blocking(move || remove_owned_staging_checked(&path));
        timeout_at(deadline, cleanup)
            .await
            .map_err(|_| AppError::timeout("本机私有 staging 清理"))?
            .map_err(|source| {
                AppError::infrastructure_with_source("本机 staging 清理 worker 失败", source)
            })?
            .map_err(|source| {
                AppError::infrastructure_with_source("本机私有 staging 清理失败", source)
            })
    }
}

/// 清理失败仍保留原错误与可重复执行的本机 staging owner。
async fn cleanup_staging_failure(
    path: &Path,
    error: AppError,
    destination: Option<FileEndpoint>,
    retryable: bool,
) -> TransferExecutionError {
    let owner = Arc::new(LocalStagingCleanup {
        path: path.to_path_buf(),
    });
    match owner
        .remove_until(Instant::now() + Duration::from_secs(5))
        .await
    {
        Ok(()) => app_failure(error, destination, retryable),
        Err(cleanup_error) => app_failure_with_cleanup(
            error.with_source(cleanup_error),
            destination,
            retryable,
            TransferCleanupState::pending(owner),
        ),
    }
}

/// 清理 staging 时保留原有失败分类，未清理成功则把同一路径 owner 交给队列重试。
async fn cleanup_staging_transfer_failure(
    path: &Path,
    failure: TransferExecutionError,
    destination: Option<FileEndpoint>,
) -> TransferExecutionError {
    let owner = Arc::new(LocalStagingCleanup {
        path: path.to_path_buf(),
    });
    match owner
        .remove_until(Instant::now() + Duration::from_secs(5))
        .await
    {
        Ok(()) => failure,
        Err(cleanup_error) => match failure {
            TransferExecutionError::Failed(failure) => {
                tracing::warn!(
                    error_code = ?failure.kind(),
                    cleanup_error_code = cleanup_error.public_code().as_str(),
                    "本机传输 staging 清理失败，保留原失败类别和 cleanup owner"
                );
                TransferExecutionError::Failed(TransferExecutionFailure::new(
                    failure.kind(),
                    failure.retryable(),
                    failure.resumable(),
                    failure.checkpoint().cloned(),
                    failure.actual_destination().cloned().or(destination),
                    TransferCleanupState::pending(owner),
                ))
            }
            TransferExecutionError::Cancelled {
                actual_destination, ..
            } => {
                tracing::warn!(
                    cleanup_error_code = cleanup_error.public_code().as_str(),
                    "取消后的本机传输 staging 未清理，保留 cleanup owner"
                );
                TransferExecutionError::Failed(TransferExecutionFailure::new(
                    TransferFailureKind::Internal,
                    false,
                    false,
                    None,
                    actual_destination.or(destination),
                    TransferCleanupState::pending(owner),
                ))
            }
        },
    }
}

/// 成功或 Unchanged 屏障前必须确认临时树已删除；装箱仅缩小错误返回值，不改变 cleanup owner。
///
/// @author kongweiguang
async fn cleanup_staging_before_success(
    path: &Path,
    destination: &FileEndpoint,
) -> Result<(), Box<TransferExecutionError>> {
    let owner = Arc::new(LocalStagingCleanup {
        path: path.to_path_buf(),
    });
    owner
        .remove_until(Instant::now() + Duration::from_secs(5))
        .await
        .map_err(|cleanup_error| {
            Box::new(app_failure_with_cleanup(
                AppError::infrastructure_with_source("本机传输 staging 清理未完成", cleanup_error),
                Some(destination.clone()),
                false,
                TransferCleanupState::pending(owner),
            ))
        })
}

/// 若解包 worker 已结束则不构造缺失句柄错误，否则 join 后才允许删除 staging。
async fn join_extractor_if_running(
    extractor: Option<JoinHandle<io::Result<PathBuf>>>,
) -> AppResult<()> {
    match extractor {
        Some(extractor) => map_extractor_result(extractor.await).map(|_| ()),
        None => Ok(()),
    }
}

/// 实现阻塞 Read 以便 tar crate 在 blocking pool 内解析有界异步数据通道。
struct ChannelReader {
    receiver: mpsc::Receiver<Vec<u8>>,
    current: Vec<u8>,
    offset: usize,
}

impl ChannelReader {
    /// 初始化 tar reader 的当前缓冲，避免将完整目录归档载入内存。
    fn new(receiver: mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            receiver,
            current: Vec::new(),
            offset: 0,
        }
    }
}

impl Read for ChannelReader {
    /// 仅在当前块耗尽时向有界 channel 取下一块，sender 关闭后报告归档 EOF。
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.offset >= self.current.len() {
            let Some(next) = self.receiver.blocking_recv() else {
                return Ok(0);
            };
            self.current = next;
            self.offset = 0;
        }
        let count = output
            .len()
            .min(self.current.len().saturating_sub(self.offset));
        output[..count].copy_from_slice(&self.current[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}

/// 将解包 JoinHandle 的 panic 与 archive layout 错误转换为安全应用错误。
async fn join_extractor(extractor: Option<JoinHandle<io::Result<PathBuf>>>) -> AppResult<PathBuf> {
    let Some(extractor) = extractor else {
        return Err(AppError::infrastructure(
            "本机 tar 解包 worker 缺少完成句柄",
        ));
    };
    map_extractor_result(extractor.await)
}

/// 保留归档格式错误 source，固定用户文案不暴露 staging 路径。
fn map_extractor_result(
    result: Result<io::Result<PathBuf>, tokio::task::JoinError>,
) -> AppResult<PathBuf> {
    result
        .map_err(|source| {
            AppError::infrastructure_with_source("本机 tar 解包 worker 未正常退出", source)
        })?
        .map_err(|source| {
            AppError::infrastructure_with_source("容器 tar 归档不完整或包含不安全路径", source)
        })
}

/// 仅递归删除已确认是普通目录的任务 staging，避免清理时跟随被替换的链接。
fn remove_owned_staging_checked(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "staging path no longer names a regular directory",
        )),
        Err(error) => Err(error),
    }
}

/// 选择目标同父目录内的传输专属 staging 名称，create_dir 的成功建立了本次所有权。
fn staging_root_for(destination: &Path, owner: &str) -> AppResult<PathBuf> {
    let parent = destination
        .parent()
        .ok_or_else(|| AppError::invalid("本机目标缺少父目录"))?;
    Ok(parent.join(format!(".kerminal-transfer-{owner}.stage")))
}

/// 将唯一 transfer ID 缩限为文件名字符；路径分隔符无法进入 staging 名称。
fn safe_owner(value: &str) -> String {
    let safe = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .take(64)
        .collect::<String>();
    if safe.is_empty() {
        "transfer".to_owned()
    } else {
        safe
    }
}

/// 正常流结束后校验 tar exit status，缺失命令或源路径按明确错误类别处理。
fn stream_exit_error(code: Option<i32>) -> AppError {
    match code {
        Some(40) => AppError::not_found("容器传输源不存在"),
        Some(42) => AppError::conflict("容器源不能是符号链接"),
        Some(127) => AppError::unsupported("容器需要 POSIX sh 与 tar"),
        _ => AppError::remote_exit(code),
    }
}

/// 将本机原子提交错误保留为已知失败或回执未知，并记录未知回执的底层 I/O 类别。
fn map_local_commit_error(
    error: LocalCommitError,
    destination: Option<FileEndpoint>,
    retryable: bool,
) -> TransferExecutionError {
    match error {
        LocalCommitError::Known(error) => app_failure(
            AppError::infrastructure_with_source("本机传输目标预检失败", error),
            destination,
            retryable,
        ),
        LocalCommitError::Unknown(error) => {
            tracing::warn!(
                error_kind = ?error.kind(),
                "本机文件提交回执未知，队列将禁止自动重试"
            );
            unknown_commit(destination)
        }
    }
}

/// 清理使用同一应用 shutdown deadline，普通传输无总预算时回退到短任务清理时间。
fn cleanup_deadline(context: &TransferExecutionContext) -> Instant {
    context
        .shutdown_deadline()
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(5))
}
