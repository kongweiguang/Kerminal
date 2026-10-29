//! 将远端文件下载到本机并发布为系统文件剪贴板。
//!
//! @author kongweiguang

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use kerminal_domain::{
    files::{
        ConflictPolicy, DirectoryListing, FileEndpoint, FileEntryKind, TransferKind,
        TransferRequest, TransferStatus, TransferSummary,
    },
    identity::TransferId,
};
use tokio_util::sync::CancellationToken;

use crate::{
    AppError, AppResult,
    files::FileService,
    transfers::{TransferQueueSubscription, TransferService},
};

const TRANSFER_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// 一条复制请求携带来源和对象类型；文件系统 endpoint 本身不保留目录项类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteClipboardSource {
    pub endpoint: FileEndpoint,
    pub kind: TransferKind,
}

/// 为应用层隔离本机下载路径分配、系统剪贴板和失败清理能力。
#[async_trait]
pub trait FileClipboardPort: Send + Sync + 'static {
    /// 在系统下载目录中独占预留路径，避免并发复制覆盖既有用户文件。
    async fn reserve_download_target(&self, name: String, kind: TransferKind)
    -> AppResult<PathBuf>;

    /// 将已完整下载且存在的本机路径作为系统文件列表发布。
    async fn publish_files(&self, paths: Vec<PathBuf>) -> AppResult<()>;

    /// 删除本次复制失败留下的预留目标或部分结果。
    async fn remove_download_target(&self, path: PathBuf, kind: TransferKind) -> AppResult<()>;

    /// 在发起任何远端读取前报告当前平台是否支持系统文件剪贴板。
    fn ensure_supported(&self) -> AppResult<()>;
}

/// 复用文件目录查询与统一传输队列，只有下载完成后才更新系统剪贴板。
#[derive(Clone)]
pub struct FileClipboardDownloadService {
    files: FileService,
    transfers: TransferService,
    clipboard: Arc<dyn FileClipboardPort>,
}

impl FileClipboardDownloadService {
    /// 注入共享文件服务、传输队列和系统剪贴板端口，避免建立第二套 SFTP 运行时。
    pub fn new(
        files: FileService,
        transfers: TransferService,
        clipboard: Arc<dyn FileClipboardPort>,
    ) -> Self {
        Self {
            files,
            transfers,
            clipboard,
        }
    }

    /// 校验远端目录项后逐个下载；只有全部成功时才原子替换系统文件剪贴板。
    pub async fn copy_remote_files_to_clipboard(
        &self,
        cancellation: CancellationToken,
        sources: Vec<RemoteClipboardSource>,
    ) -> AppResult<()> {
        self.clipboard.ensure_supported()?;
        if sources.is_empty() {
            return Err(AppError::invalid("请先选择可复制的远端文件或目录"));
        }
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("远端文件复制已取消"));
        }

        let names = self
            .resolve_source_names(cancellation.clone(), &sources)
            .await?;
        let mut downloaded = Vec::<(PathBuf, TransferKind)>::new();

        for (source, name) in sources.iter().zip(names) {
            if cancellation.is_cancelled() {
                self.remove_targets_best_effort(&downloaded).await;
                return Err(AppError::cancelled("远端文件复制已取消"));
            }

            let target = match self
                .clipboard
                .reserve_download_target(name, source.kind)
                .await
            {
                Ok(target) => target,
                Err(error) => {
                    self.remove_targets_best_effort(&downloaded).await;
                    return Err(error);
                }
            };
            if cancellation.is_cancelled() {
                self.remove_one_best_effort(&target, source.kind).await;
                self.remove_targets_best_effort(&downloaded).await;
                return Err(AppError::cancelled("远端文件复制已取消"));
            }
            let request = TransferRequest {
                source: source.endpoint.clone(),
                destination: FileEndpoint::Local {
                    path: target.clone(),
                },
                kind: source.kind,
                conflict_policy: ConflictPolicy::Overwrite,
                idle_timeout_seconds: None,
            };
            let mut subscription = self.transfers.subscribe();
            let summary = match self.transfers.enqueue(request).await {
                Ok(summary) => summary,
                Err(error) => {
                    self.remove_one_best_effort(&target, source.kind).await;
                    self.remove_targets_best_effort(&downloaded).await;
                    return Err(error);
                }
            };

            let completion = self
                .wait_for_terminal(&summary.id, &mut subscription, &cancellation)
                .await;
            match completion {
                Ok(summary)
                    if matches!(
                        summary.status,
                        TransferStatus::Succeeded | TransferStatus::Unchanged
                    ) =>
                {
                    downloaded.push((target, source.kind));
                }
                Ok(summary) => {
                    let preserve_target = summary.failure_kind
                        == Some(kerminal_domain::files::TransferFailureKind::CommitUnknown);
                    if preserve_target {
                        self.remove_targets_best_effort(&downloaded).await;
                    } else {
                        self.remove_one_best_effort(&target, source.kind).await;
                        self.remove_targets_best_effort(&downloaded).await;
                    }
                    return Err(transfer_failure(&summary));
                }
                Err(error) => {
                    let terminal = self.cancel_and_drain(&summary.id).await;
                    if terminal.as_ref().is_some_and(|summary| {
                        summary.failure_kind
                            == Some(kerminal_domain::files::TransferFailureKind::CommitUnknown)
                    }) || terminal.is_none()
                    {
                        self.remove_targets_best_effort(&downloaded).await;
                    } else {
                        self.remove_one_best_effort(&target, source.kind).await;
                        self.remove_targets_best_effort(&downloaded).await;
                    }
                    return Err(error);
                }
            }
        }

        if cancellation.is_cancelled() {
            self.remove_targets_best_effort(&downloaded).await;
            return Err(AppError::cancelled("远端文件复制已取消"));
        }

        let paths = downloaded.iter().map(|(path, _)| path.clone()).collect();
        self.clipboard.publish_files(paths).await
    }

    /// 按父目录缓存目录快照，确认路径仍存在且类型与调用方捕获的类型一致。
    async fn resolve_source_names(
        &self,
        cancellation: CancellationToken,
        sources: &[RemoteClipboardSource],
    ) -> AppResult<Vec<String>> {
        let mut seen = HashSet::new();
        let mut listings = HashMap::<FileEndpoint, DirectoryListing>::new();
        let mut names = Vec::with_capacity(sources.len());

        for source in sources {
            if cancellation.is_cancelled() {
                return Err(AppError::cancelled("远端文件复制已取消"));
            }
            let FileEndpoint::Remote { host_id, path } = &source.endpoint else {
                return Err(AppError::unsupported("系统文件剪贴板只支持 SFTP 远端来源"));
            };
            if !seen.insert(source.endpoint.clone()) {
                return Err(AppError::invalid("系统文件剪贴板不能重复复制同一远端项目"));
            }
            let parent = path
                .parent()
                .ok_or_else(|| AppError::invalid("不能复制远端根目录"))?;
            let directory = FileEndpoint::Remote {
                host_id: host_id.clone(),
                path: parent,
            };
            if !listings.contains_key(&directory) {
                let listing = self
                    .files
                    .list_directory(cancellation.clone(), directory.clone())
                    .await?;
                listings.insert(directory.clone(), listing);
            }
            let entry = listings
                .get(&directory)
                .and_then(|listing| {
                    listing
                        .entries
                        .iter()
                        .find(|entry| entry.endpoint == source.endpoint)
                })
                .ok_or_else(|| AppError::not_found("远端文件或目录已不存在，请刷新后重试"))?;
            if !entry_matches_transfer_kind(entry.kind, source.kind) {
                return Err(AppError::conflict("远端项目类型已变化，请刷新后重试"));
            }
            names.push(entry.name.clone());
        }
        Ok(names)
    }

    /// 以队列快照为权威状态，通知仅作为唤醒信号且会在每次唤醒后重新读取。
    async fn wait_for_terminal(
        &self,
        id: &TransferId,
        subscription: &mut TransferQueueSubscription,
        cancellation: &CancellationToken,
    ) -> AppResult<TransferSummary> {
        loop {
            if cancellation.is_cancelled() {
                return Err(AppError::cancelled("远端文件复制已取消"));
            }
            let summary = self.transfers.get(id).await?;
            if is_terminal(summary.status) {
                return Ok(summary);
            }
            if subscription.recv(cancellation.clone()).await.is_none() {
                return Err(AppError::cancelled("远端文件复制已取消"));
            }
        }
    }

    /// 取消仍在工作的当前项并限时等待队列终态；超时保留目标以免删除仍在写入的文件。
    async fn cancel_and_drain(&self, id: &TransferId) -> Option<TransferSummary> {
        let mut subscription = self.transfers.subscribe();
        let drain_cancellation = CancellationToken::new();
        let _ = self.transfers.cancel(id).await;
        tokio::time::timeout(TRANSFER_DRAIN_TIMEOUT, async {
            loop {
                let summary = self.transfers.get(id).await.ok()?;
                if is_terminal(summary.status) {
                    return Some(summary);
                }
                subscription.recv(drain_cancellation.clone()).await?;
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// 尽力清理已终态失败的本机目标，保留主要传输错误并只记录脱敏错误类别。
    async fn remove_targets_best_effort(&self, targets: &[(PathBuf, TransferKind)]) {
        for (path, kind) in targets {
            self.remove_one_best_effort(path, *kind).await;
        }
    }

    /// 通过平台端口删除本次创建的单个目标，避免 application 直接访问本机文件系统。
    async fn remove_one_best_effort(&self, path: &Path, kind: TransferKind) {
        if let Err(error) = self
            .clipboard
            .remove_download_target(path.to_path_buf(), kind)
            .await
        {
            tracing::warn!(
                code = error.public_code().as_str(),
                "remote clipboard download cleanup failed"
            );
        }
    }
}

/// 只允许普通文件和目录进入文件系统剪贴板，不将链接或特殊项当作可下载对象。
fn entry_matches_transfer_kind(entry: FileEntryKind, requested: TransferKind) -> bool {
    matches!(
        (entry, requested),
        (FileEntryKind::File, TransferKind::File)
            | (FileEntryKind::Directory, TransferKind::Directory)
    )
}

/// 汇总状态按领域终态判定，避免把队列受理或提交中当成可剪贴的完整文件。
fn is_terminal(status: TransferStatus) -> bool {
    matches!(
        status,
        TransferStatus::Succeeded
            | TransferStatus::Skipped
            | TransferStatus::Unchanged
            | TransferStatus::Failed
            | TransferStatus::Cancelled
    )
}

/// 失败文案不拼接远端路径或 adapter detail，保持与传输队列的脱敏规则一致。
fn transfer_failure(summary: &TransferSummary) -> AppError {
    match summary.status {
        TransferStatus::Cancelled => AppError::cancelled("远端文件复制已取消"),
        TransferStatus::Skipped | TransferStatus::Unchanged => {
            AppError::conflict("远端文件未能完整下载到本机")
        }
        _ => AppError::infrastructure("远端文件下载失败"),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/file_clipboard_download.rs"]
mod tests;
