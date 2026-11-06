//! 按已确认提交回执推导目录回读范围，不改变传输任务状态。
//!
//! @author kongweiguang

use std::path::{Component, Path};

use gpui_kit::Context;
use kerminal_domain::files::{FileEndpoint, TransferKind, TransferStatus, TransferSummary};

use super::{FilesPane, panel_refresh_target, parent_endpoint};

/// 单文件只影响父目录；目录传输还可能改变目标树内已打开的后代目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TransferRefreshScope {
    Directory(FileEndpoint),
    Tree(FileEndpoint),
    All,
}

impl FilesPane {
    /// 无法取得队列快照时仍保守回读已展示目录，避免删除事实随读取失败丢失。
    ///
    /// @author kongweiguang
    pub(super) fn refresh_after_uncertain_transfer_change(&mut self, cx: &mut Context<Self>) {
        let scopes = [TransferRefreshScope::All];
        self.invalidate_inactive_transfer_directories(&scopes);
        for side in 0..self.panels.len() {
            let panel = &self.panels[side];
            if panel_needs_transfer_refresh(
                &panel.endpoint,
                panel.requested_endpoint.as_ref(),
                panel.loading,
                panel.listing.is_some(),
                &scopes,
            ) {
                self.refresh_panel(side, cx);
            }
        }
    }
}

/// 只消费新进入可回读终态且已有明确提交回执的任务；未知提交仍保留原状态。
/// 同一任务的终态已处理后不重复刷新，实际目标缺失时不猜测冲突改名后的路径。
///
/// @author kongweiguang
pub(super) fn completed_transfer_refresh_directories(
    previous: &[TransferSummary],
    current: &[TransferSummary],
) -> Vec<TransferRefreshScope> {
    let mut scopes = Vec::new();
    for summary in current {
        if !has_refreshable_commit(summary)
            || previous
                .iter()
                .any(|old| old.id == summary.id && has_refreshable_commit(old))
        {
            continue;
        }
        let Some(destination) = summary.actual_destination.as_ref() else {
            continue;
        };
        if let Some(parent) = parent_endpoint(destination) {
            let scope = TransferRefreshScope::Directory(parent);
            if !scopes.contains(&scope) {
                scopes.push(scope);
            }
        }
        if summary.request.kind == TransferKind::Directory {
            let scope = TransferRefreshScope::Tree(destination.clone());
            if !scopes.contains(&scope) {
                scopes.push(scope);
            }
        }
    }
    scopes
}

/// 失败或取消可含已确认的目录子项；只读刷新不改变失败和未知提交的语义。
///
/// @author kongweiguang
fn has_refreshable_commit(summary: &TransferSummary) -> bool {
    matches!(
        summary.status,
        TransferStatus::Succeeded | TransferStatus::Failed | TransferStatus::Cancelled
    ) && summary.commit_summary.committed_entries() > 0
        && summary.actual_destination.is_some()
}

/// 保留用户当前或在途请求的目标，只在该精确端点属于确认提交的影响范围时回读。
///
/// @author kongweiguang
pub(super) fn panel_needs_transfer_refresh(
    committed_endpoint: &FileEndpoint,
    requested_endpoint: Option<&FileEndpoint>,
    loading: bool,
    has_listing: bool,
    refresh_scopes: &[TransferRefreshScope],
) -> bool {
    (has_listing || loading)
        && panel_refresh_target(committed_endpoint, requested_endpoint, loading).is_some_and(
            |target| {
                refresh_scopes.iter().any(|scope| match scope {
                    TransferRefreshScope::Directory(directory) => directory == target,
                    TransferRefreshScope::Tree(root) => endpoint_in_tree(root, target),
                    TransferRefreshScope::All => true,
                })
            },
        )
}

/// 后代匹配先检查端点身份，再按各自平台的路径组件比较；不把相似前缀当成目录。
///
/// @author kongweiguang
fn endpoint_in_tree(root: &FileEndpoint, candidate: &FileEndpoint) -> bool {
    if !root.same_filesystem(candidate) {
        return false;
    }
    match (root, candidate) {
        (FileEndpoint::Local { path: root }, FileEndpoint::Local { path: candidate }) => {
            if root == Path::new(".") && candidate.is_relative() {
                return !candidate
                    .components()
                    .any(|component| component == Component::ParentDir);
            }
            candidate.strip_prefix(root).is_ok_and(|suffix| {
                !suffix
                    .components()
                    .any(|component| component == Component::ParentDir)
            })
        }
        _ => match (root.remote_path(), candidate.remote_path()) {
            (Some(root), Some(candidate)) => {
                let root = root.as_str();
                let candidate = candidate.as_str();
                let suffix = if root == candidate {
                    Some("")
                } else if root == "/" {
                    candidate.strip_prefix('/')
                } else if root == "." && !candidate.starts_with('/') {
                    Some(candidate)
                } else {
                    candidate
                        .strip_prefix(root)
                        .and_then(|rest| rest.strip_prefix('/'))
                };
                suffix.is_some_and(|suffix| !suffix.split('/').any(|part| part == ".."))
            }
            _ => false,
        },
    }
}
