//! 为 Windows 外部文件拖放冻结文件选择与受管暂存任务。
//!
//! @author kongweiguang

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use futures::{channel::oneshot, executor::block_on};
use gpui_kit::{
    Context, DeferredFileDrag, DeferredFileDragProvider, ExternalDragPayload,
    ExternalFileDescriptor, ExternalFileDragOutcome, ExternalFileDragOwner, FileDragPaths,
    PreparedExternalFileDrag,
};
use kerminal_application::{
    ApplicationTaskExecutor, PublicErrorCode,
    files::{
        FileService,
        drag_manifest::{DragManifest, build_drag_manifest, validate_prepared_drag_manifest},
        drag_out::{DragOutSession, DragOutSource, prepare_drag_out_observed},
    },
    transfers::TransferService,
};
use kerminal_domain::files::{FileEndpoint, FileEntryKind, TransferKind};
use tokio_util::sync::CancellationToken;

use super::super::{FilesPane, endpoint_name, localized_error, valid_item_name};
use super::{
    DropDirectorySnapshot, FileTransferDrag, drop_directory_is_current,
    endpoint_is_same_or_descendant,
};
use crate::locale;

struct DragPreparation {
    files: FileService,
    transfers: TransferService,
    executor: ApplicationTaskExecutor,
    sources: Vec<DragOutSource>,
}

struct DragProvider {
    preparation: DragPreparation,
    cancellation: CancellationToken,
    staging: Arc<Mutex<Option<Arc<DragOutSession>>>>,
    uncertain: Arc<AtomicBool>,
    manifest: OnceLock<Result<Arc<DragManifest>, String>>,
    completion: Mutex<Option<oneshot::Sender<DragCompletion>>>,
}

/// 只有暂存确实保留时才向原面板传递本机恢复目录。
struct DragCompletion {
    outcome: ExternalFileDragOutcome,
    preserved_directory: Option<PathBuf>,
}

struct DragOwner(Arc<DragOutSession>);

impl ExternalFileDragOwner for DragOwner {
    /// 未收到目标完成回执时保留原暂存目录，不能在释放鼠标后提前删除。
    ///
    /// @author kongweiguang
    fn preserve(&self) {
        self.0.preserve();
    }
}

impl DeferredFileDragProvider for DragProvider {
    /// 首次按需读取目录清单，后续格式探测复用同一冻结快照。
    ///
    /// @author kongweiguang
    fn describe(&self) -> Result<Vec<ExternalFileDescriptor>, String> {
        let manifest = self.manifest()?;
        Ok(manifest
            .entries
            .iter()
            .map(|entry| {
                ExternalFileDescriptor::new(
                    entry.relative_path.clone(),
                    entry.kind == TransferKind::Directory,
                    entry.size,
                    entry.root_index,
                )
            })
            .collect())
    }

    /// 队列任务在共享应用执行器运行，本机路径可直接供系统使用。
    ///
    /// @author kongweiguang
    fn prepare(&self) -> Result<PreparedExternalFileDrag, String> {
        let DragPreparation {
            files,
            transfers,
            executor,
            sources,
        } = &self.preparation;
        let manifest = self.manifest()?;
        let files = files.clone();
        let transfers = transfers.clone();
        let sources = sources.clone();
        let staging = self.staging.clone();
        let observed_staging = staging.clone();
        let uncertain = self.uncertain.clone();
        let observed_uncertain = uncertain.clone();
        let prepared = block_on(executor.run_cancellable(
            self.cancellation.clone(),
            move |cancellation| async move {
                let result = prepare_drag_out_observed(
                    files.clone(),
                    transfers,
                    cancellation.clone(),
                    sources,
                    move |owner| {
                        if observed_uncertain.load(Ordering::Acquire) {
                            owner.preserve();
                        }
                        *staging.lock().unwrap_or_else(|error| error.into_inner()) = Some(owner);
                    },
                )
                .await;
                if uncertain.load(Ordering::Acquire) {
                    let session = observed_staging
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone();
                    if let Some(session) = session {
                        session.retain_unknown().await;
                    }
                }
                let prepared = result?;
                validate_prepared_drag_manifest(files, cancellation, &manifest, &prepared.paths)
                    .await?;
                Ok(prepared)
            },
        ))
        .map_err(|error| {
            if error.public_code() == PublicErrorCode::OutcomeUnknown
                && let Some(session) = self
                    .staging
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
            {
                session.preserve();
            }
            localized_error(&error)
        })?;
        Ok(PreparedExternalFileDrag::new(
            FileDragPaths::new(prepared.paths),
            Arc::new(DragOwner(prepared.session)),
        ))
    }

    /// 鼠标释放或 Escape 协作取消正在执行的应用任务及队列传输。
    ///
    /// @author kongweiguang
    fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// OLE 只对已取消且未能收尾的准备使用异常截止。
    ///
    /// @author kongweiguang
    fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    /// 结果只回送原始面板；窗口关闭时接收端自然丢弃，不重新选择活动目标。
    ///
    /// @author kongweiguang
    fn finish(&self, outcome: ExternalFileDragOutcome) {
        if matches!(outcome, ExternalFileDragOutcome::OutcomeUnknown) {
            self.uncertain.store(true, Ordering::Release);
        }
        let session = self
            .staging
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let mut outcome = outcome;
        if let Some(session) = session.as_ref() {
            let session = session.clone();
            let unknown = matches!(outcome, ExternalFileDragOutcome::OutcomeUnknown)
                || session.is_uncertain();
            if unknown {
                session.preserve();
            }
            let result = block_on(self.preparation.executor.run(move |_| async move {
                if unknown {
                    session.retain_unknown().await;
                    Ok(())
                } else {
                    session.cleanup().await
                }
            }));
            if let Err(error) = result {
                outcome = ExternalFileDragOutcome::Failed(localized_error(&error));
            }
        }
        let preserved_directory = session
            .as_ref()
            .filter(|session| session.is_preserved())
            .map(|session| session.directory());
        if let Some(sender) = self
            .completion
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            let _ = sender.send(DragCompletion {
                outcome,
                preserved_directory,
            });
        }
    }
}

impl DragProvider {
    /// 清单使用原始来源与 lease，在应用执行器中读取且只生成一次。
    ///
    /// @author kongweiguang
    fn manifest(&self) -> Result<Arc<DragManifest>, String> {
        self.manifest
            .get_or_init(|| {
                let DragPreparation {
                    files,
                    executor,
                    sources,
                    ..
                } = &self.preparation;
                let files = files.clone();
                let sources = sources.clone();
                block_on(executor.run_cancellable(
                    self.cancellation.clone(),
                    move |cancellation| async move {
                        build_drag_manifest(files, cancellation, sources)
                            .await
                            .map(Arc::new)
                    },
                ))
                .map_err(|error| localized_error(&error))
            })
            .clone()
    }
}

impl FilesPane {
    /// 在系统接管前重验原目录代次，确保多选和临时主机仍指向拖动开始时的来源。
    /// 入口仅供文件功能内的列表和树渲染调用，不向其它功能暴露拖拽状态。
    ///
    /// @author kongweiguang
    pub(in crate::features::files) fn external_drag_payload(
        &mut self,
        drag: &FileTransferDrag,
        cx: &mut Context<Self>,
    ) -> Option<ExternalDragPayload> {
        if std::env::var_os("KERMINAL_EXTERNAL_DRAG_DIAGNOSTICS").is_some_and(|value| value == "1")
        {
            eprintln!("KERMINAL_EXTERNAL_DRAG payload_requested");
        }
        let panel = self.panels.get(drag.source_side)?;
        if self.transfer_submission_busy
            || !drop_directory_is_current(
                &drag.source_directory,
                drag.source_generation,
                DropDirectorySnapshot {
                    endpoint: &panel.endpoint,
                    generation: panel.generation,
                    loading: panel.loading,
                    listing_directory: panel.listing.as_ref().map(|listing| &listing.directory),
                },
            )
        {
            self.status = Some(
                locale::text(
                    "来源目录已变化，请刷新后重新拖出。",
                    "The source folder changed. Refresh and try again.",
                )
                .to_string(),
            );
            cx.notify();
            return None;
        }
        let mut sources = Vec::with_capacity(drag.entries().len());
        let mut local_paths = Vec::with_capacity(drag.entries().len());
        let source_is_local = matches!(drag.source_directory, FileEndpoint::Local { .. });
        for entry in drag.entries() {
            let kind = match entry.kind {
                FileEntryKind::File => TransferKind::File,
                FileEntryKind::Directory => TransferKind::Directory,
                _ => {
                    self.status = Some(
                        locale::text(
                            "拖出仅支持普通文件和目录。",
                            "Only regular files and folders can be dragged out.",
                        )
                        .to_string(),
                    );
                    cx.notify();
                    return None;
                }
            };
            if !valid_item_name(&entry.name)
                || endpoint_name(&entry.endpoint).as_deref() != Some(entry.name.as_str())
                || !endpoint_is_same_or_descendant(&entry.endpoint, &drag.source_directory)
            {
                self.status = Some(
                    locale::text(
                        "拖出项目已变化，请刷新后重试。",
                        "The dragged item changed. Refresh and try again.",
                    )
                    .to_string(),
                );
                cx.notify();
                return None;
            }
            if source_is_local {
                let FileEndpoint::Local { path } = &entry.endpoint else {
                    return None;
                };
                if !path.is_absolute() {
                    return None;
                }
                local_paths.push((path.clone(), kind == TransferKind::Directory));
            } else {
                sources.push(DragOutSource {
                    endpoint: entry.endpoint.clone(),
                    name: entry.name.clone(),
                    kind,
                    external_target_lease: self.external_target_lease_for_endpoint(&entry.endpoint),
                });
            }
        }
        if drag.entries().is_empty() {
            return None;
        }
        if std::env::var_os("KERMINAL_EXTERNAL_DRAG_DIAGNOSTICS").is_some_and(|value| value == "1")
        {
            eprintln!("KERMINAL_EXTERNAL_DRAG payload_ready");
        }
        self.drag_out_generation = self.drag_out_generation.wrapping_add(1);
        let generation = self.drag_out_generation;
        let waiting = if source_is_local {
            None
        } else {
            self.queue.history_expanded = true;
            let message = locale::text("正在准备拖出…", "Preparing files…").to_string();
            self.status = Some(message.clone());
            Some(message)
        };
        let (sender, receiver) = oneshot::channel::<DragCompletion>();
        cx.spawn(async move |this, cx| {
            if let Ok(completion) = receiver.await {
                let _ = cx.update(|app| this.update(app, |pane, cx| {
                    if pane.drag_out_generation != generation { return; }
                    match completion.outcome {
                        ExternalFileDragOutcome::Cancelled => {
                            if let Some(path) = completion.preserved_directory {
                                pane.status = Some(locale::text(format!("拖出已取消；暂存文件保留于 {}", path.display()), format!("Drag cancelled. Staged files are retained at {}", path.display())).to_string());
                            } else if waiting.is_some() && pane.status == waiting { pane.status = None; }
                        }
                        ExternalFileDragOutcome::Completed => {
                            if waiting.is_some() && pane.status == waiting { pane.status = None; }
                        }
                        ExternalFileDragOutcome::OutcomeUnknown => pane.status = Some(match completion.preserved_directory {
                            Some(path) => locale::text(format!("无法确认目标复制是否完成；暂存文件保留于 {}", path.display()), format!("The target did not confirm completion. Staged files are retained at {}", path.display())).to_string(),
                            None => locale::text("无法确认目标复制是否完成。", "The target did not confirm completion.").to_string(),
                        }),
                        ExternalFileDragOutcome::Failed(error) => pane.status = Some(match completion.preserved_directory {
                            Some(path) => locale::text(format!("{error}；暂存文件保留于 {}", path.display()), format!("{error}. Staged files are retained at {}", path.display())).to_string(),
                            None => error,
                        }),
                    }
                    cx.notify();
                }));
            }
        }).detach();
        cx.notify();
        if source_is_local {
            return Some(ExternalDragPayload::Files(
                FileDragPaths::new(local_paths).on_finished(move |outcome| {
                    let _ = sender.send(DragCompletion {
                        outcome,
                        preserved_directory: None,
                    });
                }),
            ));
        }
        let provider = Arc::new(DragProvider {
            preparation: DragPreparation {
                files: self.files.clone(),
                transfers: self.transfers.clone(),
                executor: self.executor.clone(),
                sources,
            },
            cancellation: CancellationToken::new(),
            staging: Arc::new(Mutex::new(None)),
            uncertain: Arc::new(AtomicBool::new(false)),
            manifest: OnceLock::new(),
            completion: Mutex::new(Some(sender)),
        });
        Some(ExternalDragPayload::DeferredFiles(DeferredFileDrag(
            provider,
        )))
    }
}
