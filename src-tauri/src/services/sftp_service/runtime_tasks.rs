//! @author kongweiguang

use std::{
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
};

use tokio::{fs, sync::Notify};

use crate::models::sftp::SftpTransferConflictPolicy;

use super::backend::SftpEndpoint;
use super::*;

impl SftpService {
    /// 内部同步调用仍借用同一并发限制器，但不注册队列终态或自动重试。
    pub(super) async fn run_transfer_now(
        &self,
        paths: &KerminalPaths,
        request: SftpManagedTransferRequest,
    ) -> AppResult<bool> {
        let settings = load_sftp_runtime_settings(paths)?;
        let endpoint = self.resolve_endpoint(paths, &request.host_id)?;
        let request = normalize_managed_transfer_request(request)?;
        let settings = settings.for_bulk_transfer_target(&endpoint);
        let progress = TransferProgress::detached();
        let _transfer_permit = self
            .transfer_limiter
            .clone()
            .acquire(request.host_id.clone(), settings, progress.clone())
            .await?;
        self.backend
            .transfer(endpoint, request, progress, settings)
            .await?;
        Ok(true)
    }

    /// 持有目标写入锁贯穿同 ID 自动恢复；目录 checkpoint 逐项登记，不能据当前已提交项推断遍历完成。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn spawn_transfer_task(
        &self,
        transfer_id: String,
        endpoint: SftpEndpoint,
        request: SftpManagedTransferRequest,
        settings: SftpRuntimeSettings,
        cancel_requested: Arc<AtomicBool>,
        cancel_notify: Arc<Notify>,
        recovery: RecoveryCheckpointHolder,
        event_emitter: Option<TransferEventEmitter>,
    ) {
        let backend = self.backend.clone();
        let transfers = self.transfers.clone();
        let transfer_limiter = self.transfer_limiter.clone();
        let host_id = request.host_id.clone();
        let writer = self.target_writer(&request);

        tauri::async_runtime::spawn(async move {
            let progress = TransferProgress::tracked(
                transfer_id.clone(),
                transfers.clone(),
                cancel_requested,
                cancel_notify,
                recovery,
                event_emitter,
            );

            let writer = match writer {
                Ok(writer) => writer,
                Err(error) => {
                    progress.fail(error.to_string());
                    return;
                }
            };
            let _writer_guard = tokio::select! {
                guard = writer.lock_owned() => guard,
                _ = progress.cancel_notified() => { progress.cancel(); return; }
            };

            let result = run_managed_transfer_with_recovery(
                &progress,
                settings,
                transfer_limiter,
                host_id,
                request.kind == SftpTransferKind::File,
                || {
                    backend.transfer(
                        endpoint.clone(),
                        request.clone(),
                        progress.clone(),
                        settings,
                    )
                },
            )
            .await;
            match result {
                Ok(()) if progress.is_cancelled() && !progress.all_committed() => progress.cancel(),
                Ok(()) => progress.succeed(),
                Err(error) if progress.commit_outcome_uncertain() => {
                    progress.fail_commit_outcome_uncertain(error.to_string())
                }
                Err(_)
                    if committed_transfer_error_is_complete(
                        request.kind,
                        progress.all_committed(),
                        progress.commit_outcome_uncertain(),
                    ) =>
                {
                    progress.succeed()
                }
                Err(_) if progress.is_cancelled() => progress.cancel(),
                Err(_) if progress.failed_with_idle_timeout() => {}
                Err(error) => progress.fail(error.to_string()),
            }
        });
    }

    /// 同一目标在恢复等待期间仍归原任务所有，防止两个任务交错覆盖同一 partial。
    fn target_writer(
        &self,
        request: &SftpManagedTransferRequest,
    ) -> AppResult<Arc<tokio::sync::Mutex<()>>> {
        let key = match request.direction {
            SftpTransferDirection::Upload => {
                format!("remote:{}:{}", request.host_id, request.remote_path)
            }
            SftpTransferDirection::Download => format!(
                "local:{}",
                Path::new(&request.local_path)
                    .components()
                    .collect::<PathBuf>()
                    .to_string_lossy()
                    .to_lowercase()
            ),
        };
        let mut writers = self
            .target_writers
            .lock()
            .map_err(|_| AppError::StateLockPoisoned("SFTP target writers"))?;
        writers.retain(|_, writer| writer.strong_count() > 0);
        if let Some(writer) = writers.get(&key).and_then(std::sync::Weak::upgrade) {
            return Ok(writer);
        }
        let writer = Arc::new(tokio::sync::Mutex::new(()));
        writers.insert(key, Arc::downgrade(&writer));
        Ok(writer)
    }

    /// 复合复制缺少跨端原子检查点，只监督取消与闲置并保持目标副作用可观察。
    pub(super) fn spawn_remote_copy_task(&self, task: RemoteCopyTaskInput) {
        let RemoteCopyTaskInput {
            transfer_id,
            source_endpoint,
            target_endpoint,
            request,
            temp_root,
            settings,
            cancel_requested,
            cancel_notify,
            recovery,
            event_emitter,
        } = task;
        let backend = self.backend.clone();
        let transfers = self.transfers.clone();
        let transfer_limiter = self.transfer_limiter.clone();

        tauri::async_runtime::spawn(async move {
            let progress = TransferProgress::tracked(
                transfer_id.clone(),
                transfers.clone(),
                cancel_requested,
                cancel_notify,
                recovery,
                event_emitter,
            );
            let result = if should_stage_remote_copy(&request, settings) {
                run_staged_remote_copy(StagedRemoteCopyTask {
                    backend,
                    transfer_limiter,
                    source_endpoint,
                    target_endpoint,
                    request,
                    temp_root,
                    transfer_id,
                    settings,
                    progress: progress.clone(),
                })
                .await
            } else {
                run_streamed_remote_copy(
                    backend,
                    transfer_limiter,
                    source_endpoint,
                    target_endpoint,
                    request,
                    settings,
                    progress.clone(),
                )
                .await
            };

            match result {
                Ok(()) if progress.is_cancelled() => progress.cancel(),
                Ok(()) => progress.succeed(),
                Err(error) if progress.commit_outcome_uncertain() => {
                    progress.fail_commit_outcome_uncertain(error.to_string())
                }
                Err(_) if progress.is_cancelled() => progress.cancel(),
                Err(_) if progress.failed_with_idle_timeout() => {}
                Err(error) => progress.fail(error.to_string()),
            }
        });
    }

    /// 启动归档下载，并只在实际远程传输段施加无进度保护。
    ///
    /// ZIP 打包是本地 CPU/磁盘工作，不应被网络无进度阈值误杀；下载段复用普通传输的单个
    /// watchdog，既保留 partial 续传，也避免归档任务成为绕过超时策略的长连接入口。
    pub(super) fn spawn_archive_download_task(&self, task: ArchiveDownloadTaskInput) {
        let ArchiveDownloadTaskInput {
            transfer_id,
            endpoint,
            request,
            temp_root,
            settings,
            cancel_requested,
            cancel_notify,
            recovery,
            event_emitter,
        } = task;
        let backend = self.backend.clone();
        let transfers = self.transfers.clone();
        let transfer_limiter = self.transfer_limiter.clone();

        tauri::async_runtime::spawn(async move {
            let progress = TransferProgress::tracked(
                transfer_id.clone(),
                transfers.clone(),
                cancel_requested,
                cancel_notify,
                recovery,
                event_emitter,
            );
            let job_temp_dir = temp_root.join("sftp-archive-download").join(&transfer_id);
            let archive_root_name =
                remote_path_file_name(&request.source_remote_path, request.kind);
            let local_stage_path = job_temp_dir.join(&archive_root_name);
            let local_stage = local_stage_path.to_string_lossy().into_owned();
            let target_local_path = PathBuf::from(&request.target_local_path);

            let result: AppResult<()> = async {
                let transfer_permit = transfer_limiter
                    .acquire(request.host_id.clone(), settings, progress.clone())
                    .await?;
                if progress.is_cancelled() {
                    drop(transfer_permit);
                    return Err(AppError::Sftp("传输已取消".to_owned()));
                }
                progress.mark_running();
                progress.mark_phase("downloading", Some(request.source_remote_path.clone()));
                run_with_idle_watchdog(
                    &progress,
                    settings.idle_timeout_seconds,
                    backend.transfer(
                        endpoint,
                        SftpManagedTransferRequest {
                            direction: SftpTransferDirection::Download,
                            host_id: request.host_id.clone(),
                            kind: request.kind,
                            local_path: local_stage,
                            remote_path: request.source_remote_path.clone(),
                            conflict_policy: SftpTransferConflictPolicy::Overwrite,
                            view_scope: None,
                            idle_timeout_seconds: Some(settings.idle_timeout_seconds as u16),
                        },
                        progress.clone(),
                        settings,
                    ),
                )
                .await?;
                drop(transfer_permit);

                progress.ensure_not_cancelled()?;
                progress.mark_phase(
                    "archiving",
                    Some(target_local_path.to_string_lossy().into_owned()),
                );
                let archive_kind =
                    archive_kind_for_staged_path(&local_stage_path, request.kind).await;
                let zip_cancel = progress.cancel_requested.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    zip_local_path_to_file_with_conflict(
                        &local_stage_path,
                        &target_local_path,
                        &archive_root_name,
                        archive_kind,
                        zip_cancel,
                        request.conflict_policy,
                    )
                })
                .await
                .map_err(|error| AppError::Sftp(format!("ZIP 归档任务失败: {error}")))??;
                Ok(())
            }
            .await;

            let _ = fs::remove_dir_all(&job_temp_dir).await;
            match result {
                Ok(()) if progress.is_cancelled() => progress.cancel(),
                Ok(()) => progress.succeed(),
                Err(error) if progress.commit_outcome_uncertain() => {
                    progress.fail_commit_outcome_uncertain(error.to_string())
                }
                Err(_) if progress.is_cancelled() => progress.cancel(),
                Err(_) if progress.failed_with_idle_timeout() => {}
                Err(error) => progress.fail(error.to_string()),
            }
        });
    }

    /// 启动归档上传，并把无进度预算限定在完成归档后的远程写入段。
    ///
    /// 本地压缩阶段会刷新阶段活动时间但不创建网络 timer；真正上传时使用单个 watchdog，避免
    /// 大归档的持续字节进度被总时长截断，同时让失联后的 partial 保持可恢复。
    pub(super) fn spawn_archive_upload_task(&self, task: ArchiveUploadTaskInput) {
        let ArchiveUploadTaskInput {
            transfer_id,
            endpoint,
            request,
            temp_root,
            settings,
            cancel_requested,
            cancel_notify,
            recovery,
            event_emitter,
        } = task;
        let backend = self.backend.clone();
        let transfers = self.transfers.clone();
        let transfer_limiter = self.transfer_limiter.clone();

        tauri::async_runtime::spawn(async move {
            let progress = TransferProgress::tracked(
                transfer_id.clone(),
                transfers.clone(),
                cancel_requested,
                cancel_notify,
                recovery,
                event_emitter,
            );
            let job_temp_dir = temp_root.join("sftp-archive-upload").join(&transfer_id);
            let source_local_path = PathBuf::from(&request.source_local_path);
            let archive_root_name = local_path_file_name(&source_local_path, request.kind);
            let local_stage_path = job_temp_dir.join(format!(
                "{}.zip",
                zip_safe_entry_name(&archive_root_name, "archive")
            ));
            let local_stage = local_stage_path.to_string_lossy().into_owned();

            let result: AppResult<()> = async {
                progress.mark_running();
                progress.mark_phase(
                    "archiving",
                    Some(source_local_path.to_string_lossy().into_owned()),
                );
                let zip_cancel = progress.cancel_requested.clone();
                let zip_source_path = source_local_path.clone();
                let zip_target_path = local_stage_path.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    zip_local_path_to_file(
                        &zip_source_path,
                        &zip_target_path,
                        &archive_root_name,
                        request.kind,
                        zip_cancel,
                    )
                })
                .await
                .map_err(|error| AppError::Sftp(format!("ZIP 归档任务失败: {error}")))??;

                progress.ensure_not_cancelled()?;
                progress.mark_phase("uploading", Some(request.target_remote_path.clone()));
                let transfer_permit = transfer_limiter
                    .acquire(request.host_id.clone(), settings, progress.clone())
                    .await?;
                run_with_idle_watchdog(
                    &progress,
                    settings.idle_timeout_seconds,
                    backend.transfer(
                        endpoint,
                        SftpManagedTransferRequest {
                            direction: SftpTransferDirection::Upload,
                            host_id: request.host_id.clone(),
                            kind: SftpTransferKind::File,
                            local_path: local_stage,
                            remote_path: request.target_remote_path.clone(),
                            conflict_policy: request.conflict_policy,
                            view_scope: None,
                            idle_timeout_seconds: Some(settings.idle_timeout_seconds as u16),
                        },
                        progress.clone(),
                        settings,
                    ),
                )
                .await?;
                drop(transfer_permit);
                Ok(())
            }
            .await;

            let _ = fs::remove_dir_all(&job_temp_dir).await;
            match result {
                Ok(()) if progress.is_cancelled() => progress.cancel(),
                Ok(()) => progress.succeed(),
                Err(error) if progress.commit_outcome_uncertain() => {
                    progress.fail_commit_outcome_uncertain(error.to_string())
                }
                Err(_) if progress.is_cancelled() => progress.cancel(),
                Err(_) if progress.failed_with_idle_timeout() => {}
                Err(error) => progress.fail(error.to_string()),
            }
        });
    }

    /// 启动剪贴板下载，确保文件获取与普通队列共享相同的断线恢复边界。
    ///
    /// 写入系统剪贴板发生在下载成功以后，不属于网络活动；watchdog 只包住远程读取，因而不会
    /// 因 UI 或系统剪贴板短暂阻塞而误报无进度。
    pub(super) fn spawn_clipboard_download_task(&self, task: ClipboardDownloadTaskInput) {
        let ClipboardDownloadTaskInput {
            transfer_id,
            endpoint,
            request,
            target_local_path,
            settings,
            cancel_requested,
            cancel_notify,
            recovery,
            copy_to_clipboard,
            event_emitter,
        } = task;
        let backend = self.backend.clone();
        let transfers = self.transfers.clone();
        let transfer_limiter = self.transfer_limiter.clone();

        tauri::async_runtime::spawn(async move {
            let progress = TransferProgress::tracked(
                transfer_id.clone(),
                transfers.clone(),
                cancel_requested,
                cancel_notify,
                recovery,
                event_emitter,
            );
            let target_local_path_string = target_local_path.to_string_lossy().into_owned();

            let result: AppResult<()> = async {
                if let Some(parent) = target_local_path
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                {
                    fs::create_dir_all(parent).await?;
                }

                let transfer_permit = transfer_limiter
                    .acquire(request.host_id.clone(), settings, progress.clone())
                    .await?;
                if progress.is_cancelled() {
                    drop(transfer_permit);
                    return Err(AppError::Sftp("传输已取消".to_owned()));
                }
                progress.mark_running();
                run_with_idle_watchdog(
                    &progress,
                    settings.idle_timeout_seconds,
                    backend.transfer(
                        endpoint,
                        SftpManagedTransferRequest {
                            direction: SftpTransferDirection::Download,
                            host_id: request.host_id.clone(),
                            kind: request.kind,
                            local_path: target_local_path_string,
                            remote_path: request.source_remote_path.clone(),
                            conflict_policy: SftpTransferConflictPolicy::Overwrite,
                            view_scope: None,
                            idle_timeout_seconds: Some(settings.idle_timeout_seconds as u16),
                        },
                        progress.clone(),
                        settings,
                    ),
                )
                .await?;
                drop(transfer_permit);

                progress.ensure_not_cancelled()?;
                if copy_to_clipboard {
                    let clipboard_target = target_local_path.clone();
                    tauri::async_runtime::spawn_blocking(move || {
                        write_local_file_clipboard(&[clipboard_target])
                    })
                    .await
                    .map_err(|error| {
                        AppError::Sftp(format!("写入系统文件剪贴板失败: {error}"))
                    })??;
                }
                Ok(())
            }
            .await;

            match result {
                Ok(()) if progress.is_cancelled() => progress.cancel(),
                Ok(()) => progress.succeed(),
                Err(error) if progress.commit_outcome_uncertain() => {
                    progress.fail_commit_outcome_uncertain(error.to_string())
                }
                Err(_) if progress.is_cancelled() => progress.cancel(),
                Err(_) if progress.failed_with_idle_timeout() => {}
                Err(error) => progress.fail(error.to_string()),
            }
        });
    }
}

/// 跨主机槽位不足或目录复制会包含自身时改走中转，避免并发等待或递归复制。
pub(super) fn should_stage_remote_copy(
    request: &SftpRemoteCopyRequest,
    settings: SftpRuntimeSettings,
) -> bool {
    if request.source_host_id != request.target_host_id {
        return settings.global_transfers < 2;
    }
    request.kind == SftpTransferKind::Directory
        && is_remote_descendant_path(&request.source_remote_path, &request.target_remote_path)
}

/// 跨主机流式复制必须同时拿齐槽位；无安全双端检查点时闲置失败不重放副作用。
async fn run_streamed_remote_copy(
    backend: Arc<dyn SftpBackend>,
    transfer_limiter: Arc<TransferLimiter>,
    source_endpoint: SftpEndpoint,
    target_endpoint: SftpEndpoint,
    request: SftpRemoteCopyRequest,
    settings: SftpRuntimeSettings,
    progress: TransferProgress,
) -> AppResult<()> {
    let host_ids = if request.source_host_id == request.target_host_id {
        vec![request.source_host_id.clone()]
    } else {
        vec![
            request.source_host_id.clone(),
            request.target_host_id.clone(),
        ]
    };
    let transfer_permits = transfer_limiter
        .acquire_many(host_ids, settings, progress.clone())
        .await?;
    if progress.is_cancelled() {
        drop(transfer_permits);
        return Err(AppError::Sftp("传输已取消".to_owned()));
    }
    progress.mark_running();
    progress.mark_phase("connecting", None);
    run_transfer_with_recovery(&progress, settings.idle_timeout_seconds, || {
        backend.remote_copy(
            source_endpoint.clone(),
            target_endpoint.clone(),
            request.clone(),
            progress.clone(),
            settings,
        )
    })
    .await?;
    drop(transfer_permits);
    Ok(())
}

/// 本地中转依次持有源和目标槽位，下载阶段的隐藏字节也刷新共享无进度时钟。
async fn run_staged_remote_copy(task: StagedRemoteCopyTask) -> AppResult<()> {
    let StagedRemoteCopyTask {
        backend,
        transfer_limiter,
        source_endpoint,
        target_endpoint,
        request,
        temp_root,
        transfer_id,
        settings,
        progress,
    } = task;
    let job_temp_dir = temp_root.join("sftp-remote-copy").join(&transfer_id);
    let local_stage_path = job_temp_dir.join(remote_path_file_name(
        &request.source_remote_path,
        request.kind,
    ));
    let local_stage = local_stage_path.to_string_lossy().into_owned();

    let result: AppResult<()> = async {
        let source_permit = transfer_limiter
            .acquire(request.source_host_id.clone(), settings, progress.clone())
            .await?;
        if progress.is_cancelled() {
            drop(source_permit);
            return Err(AppError::Sftp("传输已取消".to_owned()));
        }
        progress.mark_running();
        progress.mark_phase("connecting", None);
        let source_progress = progress.detached_child();
        run_transfer_with_recovery(&progress, settings.idle_timeout_seconds, || {
            backend.transfer(
                source_endpoint.clone(),
                SftpManagedTransferRequest {
                    direction: SftpTransferDirection::Download,
                    host_id: request.source_host_id.clone(),
                    kind: request.kind,
                    local_path: local_stage.clone(),
                    remote_path: request.source_remote_path.clone(),
                    conflict_policy: SftpTransferConflictPolicy::Overwrite,
                    view_scope: None,
                    idle_timeout_seconds: Some(settings.idle_timeout_seconds as u16),
                },
                source_progress.clone(),
                settings,
            )
        })
        .await?;
        drop(source_permit);

        progress.ensure_not_cancelled()?;
        let target_permit = transfer_limiter
            .acquire(request.target_host_id.clone(), settings, progress.clone())
            .await?;
        progress.mark_phase("connecting", None);
        run_transfer_with_recovery(&progress, settings.idle_timeout_seconds, || {
            backend.transfer(
                target_endpoint.clone(),
                SftpManagedTransferRequest {
                    direction: SftpTransferDirection::Upload,
                    host_id: request.target_host_id.clone(),
                    kind: request.kind,
                    local_path: local_stage.clone(),
                    remote_path: request.target_remote_path.clone(),
                    conflict_policy: request.conflict_policy,
                    view_scope: None,
                    idle_timeout_seconds: Some(settings.idle_timeout_seconds as u16),
                },
                progress.clone(),
                settings,
            )
        })
        .await?;
        drop(target_permit);
        Ok(())
    }
    .await;

    let _ = fs::remove_dir_all(&job_temp_dir).await;
    result
}

async fn archive_kind_for_staged_path(
    staged_path: &Path,
    requested_kind: SftpTransferKind,
) -> SftpTransferKind {
    match fs::metadata(staged_path).await {
        Ok(metadata) if metadata.is_dir() => SftpTransferKind::Directory,
        Ok(metadata) if metadata.is_file() => SftpTransferKind::File,
        _ => requested_kind,
    }
}

struct StagedRemoteCopyTask {
    backend: Arc<dyn SftpBackend>,
    transfer_limiter: Arc<TransferLimiter>,
    pub(super) source_endpoint: SftpEndpoint,
    pub(super) target_endpoint: SftpEndpoint,
    pub(super) request: SftpRemoteCopyRequest,
    pub(super) temp_root: PathBuf,
    pub(super) transfer_id: String,
    pub(super) settings: SftpRuntimeSettings,
    progress: TransferProgress,
}

mod types;
pub(super) use types::*;

mod recovery;
use recovery::{
    committed_transfer_error_is_complete, run_managed_transfer_with_recovery,
    run_transfer_with_recovery,
};
