//! 操作系统文件管理器拖入路径的检查与上传队列流程。
//!
//! @author kongweiguang

use std::{collections::HashSet, path::PathBuf};

use gpui_kit::Context;
use kerminal_domain::files::{FileEndpoint, FileEntryKind, TransferKind, TransferRequest};

use super::super::{
    FilesPane, TransferEnqueueItem, batch_enqueue_message, child_endpoint, enqueue_transfer_items,
    localized_error, valid_item_name,
};
use crate::locale;

impl FilesPane {
    /// 操作系统路径只进入可见的远端或容器目录，并经 FileService 检查后创建上传任务。
    ///
    /// @author kongweiguang
    pub(in crate::features::files) fn handle_external_paths_drop(
        &mut self,
        destination_side: usize,
        destination_directory: FileEndpoint,
        destination_generation: u64,
        paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if self.transfer_submission_busy {
            let _ = self.begin_transfer_submission(cx);
            return;
        }
        if destination_side >= self.panels.len()
            || !matches!(
                &destination_directory,
                FileEndpoint::Remote { .. } | FileEndpoint::Container { .. }
            )
            || paths.is_empty()
        {
            self.status = Some(
                locale::text(
                    "这里只能将本机文件拖到已读取的远端或容器目录。",
                    "Drop local files onto a loaded remote or container directory.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let panel = &self.panels[destination_side];
        if panel.endpoint != destination_directory
            || panel.generation != destination_generation
            || panel.loading
            || !panel
                .listing
                .as_ref()
                .is_some_and(|listing| listing.directory == panel.endpoint)
        {
            self.status = Some(
                locale::text(
                    "目标目录已变化或尚未读取完成，请刷新后重新拖放。",
                    "The target directory changed or is still loading. Refresh it and drop again.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if !self.begin_transfer_submission(cx) {
            return;
        }

        let files = self.files.clone();
        let transfers = self.transfers.clone();
        let executor = self.executor.clone();
        let conflict_policy = self.queue.conflict_policy;
        let external_target_lease = self.external_target_lease_for_endpoint(&destination_directory);
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    let total = paths.len();
                    let mut seen = HashSet::with_capacity(total);
                    let mut items = Vec::with_capacity(total);
                    let mut issues = Vec::new();
                    for (index, path) in paths.into_iter().enumerate() {
                        if cancellation.is_cancelled() {
                            issues.push(format!(
                                "{} {} {}",
                                locale::text("已取消，", "Cancelled;"),
                                total.saturating_sub(index),
                                locale::text("项未处理。", "items were not processed.")
                            ));
                            break;
                        }
                        if !seen.insert(path.clone()) {
                            issues.push(format!(
                                "{}: {}",
                                path.display(),
                                locale::text("拖放中重复出现，已忽略。", "appeared more than once and was ignored.")
                            ));
                            continue;
                        }
                        let Some(name) = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .filter(|name| valid_item_name(name))
                            .map(str::to_owned)
                        else {
                            issues.push(format!(
                                "{}: {}",
                                path.display(),
                                locale::text(
                                    "文件名为空或无法安全表示。",
                                    "the name is empty or cannot be represented safely."
                                )
                            ));
                            continue;
                        };
                        let source = match files
                            .inspect_local_transfer_source(cancellation.child_token(), path.clone())
                            .await
                        {
                            Ok(source) => source,
                            Err(error) if cancellation.is_cancelled() => {
                                issues.push(format!("{}: {}", name, localized_error(&error)));
                                let remaining = total.saturating_sub(index + 1);
                                if remaining > 0 {
                                    issues.push(format!(
                                        "{} {} {}",
                                        locale::text("已取消，", "Cancelled;"),
                                        remaining,
                                        locale::text("项未检查。", "items were not inspected.")
                                    ));
                                }
                                break;
                            }
                            Err(error) => {
                                issues.push(format!(
                                    "{}: {}",
                                    name,
                                    localized_error(&error)
                                ));
                                continue;
                            }
                        };
                        let kind = match source.kind {
                            FileEntryKind::File => TransferKind::File,
                            FileEntryKind::Directory => TransferKind::Directory,
                            FileEntryKind::Symlink | FileEntryKind::Other => {
                                issues.push(format!(
                                    "{}: {}",
                                    name,
                                    locale::text(
                                        "只支持普通文件和目录，不跟随链接或特殊文件。",
                                        "only regular files and directories are accepted; links and special files are not followed."
                                    )
                                ));
                                continue;
                            }
                        };
                        let Some(destination) = child_endpoint(&destination_directory, &name) else {
                            issues.push(format!(
                                "{}: {}",
                                name,
                                locale::text(
                                    "目标文件系统不支持该名称。",
                                    "the destination does not support this name."
                                )
                            ));
                            continue;
                        };
                        let request = TransferRequest {
                            source: source.endpoint,
                            destination,
                            kind,
                            conflict_policy,
                            idle_timeout_seconds: None,
                        };
                        items.push(TransferEnqueueItem {
                            name,
                            external_target_lease: external_target_lease
                                .as_ref()
                                .filter(|lease| request.destination.host_id() == Some(lease.host_id()))
                                .cloned(),
                            request,
                        });
                    }
                    Ok(enqueue_transfer_items(
                        transfers,
                        cancellation,
                        items,
                        total,
                        0,
                        issues,
                    )
                    .await)
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    pane.end_transfer_submission();
                    pane.status = match result {
                        Ok(outcome) => Some(batch_enqueue_message(
                            "拖入上传任务已入队",
                            "Dropped upload tasks queued",
                            outcome,
                        )),
                        Err(error) => Some(localized_error(&error)),
                    };
                    pane.reload_transfers(cx);
                    cx.notify();
                });
            });
        })
        .detach();
    }
}
