//! GPUI FilesPane 的应用内 SFTP 剪贴板；不占用系统文本或文件剪贴板。
//!
//! @author kongweiguang

use kerminal_application::files::clipboard_download::RemoteClipboardSource;
use kerminal_domain::files::{
    FileEndpoint, FileEntry, FileEntryKind, TransferKind, TransferRequest,
};

use super::{
    BatchEnqueueLabels, FilesPane, FilesPaneEvent, TransferBatchPlan, TransferEnqueueItem,
    batch_enqueue_message, child_endpoint, valid_item_name,
};
use crate::locale;

/// 复制缓冲区保留来源 endpoint，而非只保存主机标签，确保粘贴不依赖活动面板重建身份。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RemoteFileClipboard {
    entries: Vec<RemoteClipboardEntry>,
}

/// 一项剪贴板来源带原始端点和类型，供同主机复制及跨主机传输使用。
#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteClipboardEntry {
    name: String,
    source: FileEndpoint,
    kind: TransferKind,
}

impl FilesPane {
    /// 系统文本剪贴板只接收无损路径字符串，非 Unicode 本机路径保持原状并返回可读错误。
    ///
    /// @author kongweiguang
    pub(super) fn copy_path_to_clipboard(
        &mut self,
        endpoint: &FileEndpoint,
        cx: &mut gpui_kit::Context<Self>,
    ) {
        let path = match endpoint {
            FileEndpoint::Local { path } => path.to_str().map(str::to_owned),
            FileEndpoint::Remote { path, .. } | FileEndpoint::Container { path, .. } => {
                Some(path.as_str().to_owned())
            }
        };
        let Some(path) = path.filter(|path| !path.chars().any(char::is_control)) else {
            self.status = Some(
                locale::text(
                    "此路径无法无损复制到文本剪贴板。",
                    "This path cannot be copied losslessly to the text clipboard.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(path.clone()));
        self.status = Some(
            locale::text(
                format!("已复制路径：{path}"),
                format!("Copied path: {path}"),
            )
            .to_string(),
        );
        cx.notify();
    }

    /// 右键当前已选行时复制整组选区，右键未选行则只复制该行，保持 List 与 Tree 一致。
    ///
    /// @author kongweiguang
    pub(super) fn copy_to_remote_clipboard(
        &mut self,
        side: usize,
        context_entry: Option<FileEntry>,
        cx: &mut gpui_kit::Context<Self>,
    ) {
        let selected_context_entry = context_entry.as_ref().is_some_and(|entry| {
            self.selected_entries(side)
                .iter()
                .any(|selected| selected.endpoint == entry.endpoint)
        });
        let selected = if selected_context_entry || context_entry.is_none() {
            self.selected_entries(side)
        } else {
            context_entry.into_iter().collect()
        };
        let entries = selected
            .into_iter()
            .filter_map(remote_clipboard_entry)
            .collect::<Vec<_>>();
        if entries.is_empty() {
            self.status = Some(
                locale::text(
                    "请选择可复制的远程文件或目录。",
                    "Select remote files or folders that can be copied.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let message = if entries.len() == 1 {
            format!(
                "{}：{}",
                locale::text("已复制到 SFTP 剪贴板", "Copied to the SFTP clipboard"),
                entries[0]
                    .source
                    .remote_path()
                    .map_or("".to_owned(), |path| path.as_str().to_owned())
            )
        } else {
            format!(
                "{}：{} {}",
                locale::text("已复制到 SFTP 剪贴板", "Copied to the SFTP clipboard"),
                entries.len(),
                locale::text("个远程项目", "remote items")
            )
        };
        self.remote_clipboard = Some(RemoteFileClipboard { entries });
        self.status = Some(message);
        cx.notify();
    }

    /// 单个 SFTP 项使用系统文件剪贴板；下载在 Root 的共享任务执行器中完成并回执原请求。
    ///
    /// @author kongweiguang
    pub(super) fn request_remote_files_to_system_clipboard(
        &mut self,
        side: usize,
        context_entry: Option<FileEntry>,
        cx: &mut gpui_kit::Context<Self>,
    ) {
        let selected_context_entry = context_entry.as_ref().is_some_and(|entry| {
            self.selected_entries(side)
                .iter()
                .any(|selected| selected.endpoint == entry.endpoint)
        });
        let selected = if selected_context_entry || context_entry.is_none() {
            self.selected_entries(side)
        } else {
            context_entry.into_iter().collect()
        };
        let sources = selected
            .into_iter()
            .filter_map(remote_clipboard_entry)
            .map(|entry| RemoteClipboardSource {
                endpoint: entry.source,
                kind: entry.kind,
            })
            .collect::<Vec<_>>();
        if sources.is_empty() {
            self.status = Some(
                locale::text(
                    "请选择可复制到剪贴板的远程文件或目录。",
                    "Select a remote file or folder that can be copied to the clipboard.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }

        self.next_clipboard_download_request_id = self
            .next_clipboard_download_request_id
            .wrapping_add(1)
            .max(1);
        let request_id = self.next_clipboard_download_request_id;
        if let Some(previous_request_id) = self
            .active_clipboard_download_request_id
            .replace(request_id)
        {
            cx.emit(FilesPaneEvent::CancelClipboardDownload {
                request_id: previous_request_id,
            });
        }
        self.status = Some(
            locale::text(
                "正在下载远端项目并更新系统剪贴板…",
                "Downloading remote items and updating the system clipboard…",
            )
            .to_string(),
        );
        cx.emit(FilesPaneEvent::CopyRemoteFilesToClipboard {
            request_id,
            sources,
        });
        cx.notify();
    }

    /// 只接受当前 pane 的最新请求回执，丢弃因重试或切换项而迟到的结果。
    ///
    /// @author kongweiguang
    pub(crate) fn complete_clipboard_download(
        &mut self,
        request_id: u64,
        result: Result<(), String>,
        cx: &mut gpui_kit::Context<Self>,
    ) {
        if self.active_clipboard_download_request_id != Some(request_id) {
            return;
        }
        self.active_clipboard_download_request_id = None;
        self.status = result.err();
        self.status_details_expanded = false;
        cx.notify();
    }

    /// 将缓冲区项目按其原主机路径复制到目标目录，并由共用队列承接部分失败与外部主机租约。
    ///
    /// @author kongweiguang
    pub(super) fn paste_remote_clipboard(
        &mut self,
        destination_directory: FileEndpoint,
        cx: &mut gpui_kit::Context<Self>,
    ) {
        let Some(clipboard) = self.remote_clipboard.clone() else {
            self.status =
                Some(locale::text("SFTP 剪贴板为空。", "The SFTP clipboard is empty.").to_string());
            cx.notify();
            return;
        };
        if !matches!(destination_directory, FileEndpoint::Remote { .. }) {
            self.status = Some(
                locale::text(
                    "SFTP 剪贴板只能粘贴到 SSH/SFTP 目录。",
                    "The SFTP clipboard can only be pasted into an SSH/SFTP directory.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let Some(target) = self.capture_transfer_target_for_directory(&destination_directory)
        else {
            self.status = Some(
                locale::text(
                    "目标目录尚未读取完成，请刷新后重试。",
                    "The destination folder is not ready. Refresh it and try again.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };

        let conflict_policy = self.queue.conflict_policy;
        let total = clipboard.entries.len();
        let mut issues = Vec::new();
        let mut items = Vec::new();
        for entry in clipboard.entries {
            let Some(destination) = pasted_remote_endpoint(&destination_directory, &entry) else {
                issues.push(format!(
                    "{}: {}",
                    entry.name,
                    locale::text(
                        "无法构造安全的远程目标路径。",
                        "could not build a safe remote destination path."
                    )
                ));
                continue;
            };
            let lease = self
                .external_target_lease_for_endpoint(&entry.source)
                .or_else(|| self.external_target_lease_for_endpoint(&destination));
            items.push(TransferEnqueueItem {
                name: entry.name,
                request: TransferRequest {
                    source: entry.source,
                    destination,
                    kind: entry.kind,
                    conflict_policy,
                    idle_timeout_seconds: None,
                },
                external_target_lease: lease,
            });
        }
        if items.is_empty() {
            self.status = Some(batch_enqueue_message(
                "远程复制任务已入队",
                "Remote copy tasks queued",
                super::BatchEnqueueOutcome {
                    total,
                    planned_tasks: 0,
                    queued: 0,
                    included_by_selected_directories: 0,
                    issues,
                    cancelled_remaining: 0,
                },
            ));
            cx.notify();
            return;
        }
        self.enqueue_transfer_batch(
            TransferBatchPlan {
                items,
                total,
                included_by_selected_directories: 0,
                issues,
                labels: BatchEnqueueLabels {
                    action_zh: "远程复制任务已入队",
                    action_en: "Remote copy tasks queued",
                },
                target,
            },
            cx,
        );
    }
}

/// 只允许文件、符号链接和目录进入剪贴板；特殊文件不映射成可传输类型。
///
/// @author kongweiguang
fn remote_clipboard_entry(entry: FileEntry) -> Option<RemoteClipboardEntry> {
    let kind = match entry.kind {
        FileEntryKind::File | FileEntryKind::Symlink => TransferKind::File,
        FileEntryKind::Directory => TransferKind::Directory,
        FileEntryKind::Other => return None,
    };
    matches!(&entry.endpoint, FileEndpoint::Remote { .. }).then_some(RemoteClipboardEntry {
        name: entry.name,
        source: entry.endpoint,
        kind,
    })
}

/// 同一路径粘贴采用 main 的 `.copy` 命名，避免把源文件作为自己的覆盖目标。
///
/// @author kongweiguang
fn pasted_remote_endpoint(
    destination_directory: &FileEndpoint,
    entry: &RemoteClipboardEntry,
) -> Option<FileEndpoint> {
    if !valid_item_name(&entry.name) {
        return None;
    }
    let destination = child_endpoint(destination_directory, &entry.name)?;
    if destination != entry.source {
        return Some(destination);
    }
    let dot = entry.name.rfind('.').filter(|index| *index > 0);
    let copy_name = match dot {
        Some(index) => format!("{}.copy{}", &entry.name[..index], &entry.name[index..]),
        None => format!("{}.copy", entry.name),
    };
    child_endpoint(destination_directory, &copy_name)
}
