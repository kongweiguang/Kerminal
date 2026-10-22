//! 文件工作区的传输表单策略与清理重试交互。
//!
//! @author kongweiguang

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_kit::Context;
use kerminal_application::transfers::{TransferRecoveryPage, TransferRecoveryRecord};
use kerminal_application::{AppError, AppResult};
use kerminal_domain::{files::ConflictPolicy, identity::TransferId};
use tokio_util::sync::CancellationToken;

use super::FilesPane;
use crate::locale;

/// 同一视图一次仅运行一个清理重试，token 只取消当前等待，资源 owner 仍由队列持有。
struct CleanupRetry {
    id: TransferId,
    cancellation: CancellationToken,
    active_cancellation: Arc<Mutex<Option<CancellationToken>>>,
}

/// UI 队列状态与应用传输记录分离；冲突策略只用于之后提交的请求。
pub(super) struct TransferQueueUi {
    pub(super) conflict_policy: ConflictPolicy,
    pub(super) history_expanded: bool,
    cleanup_retry: Option<CleanupRetry>,
    clear_completed_cancellation: Option<CancellationToken>,
    recovery_cancellation: Option<CancellationToken>,
    recovery_id: Option<TransferId>,
    pub(super) recovery_list_loading: bool,
    pub(super) recovery_list_error: Option<String>,
    expanded_recovery: Option<TransferId>,
    pub(super) archive_records: Vec<TransferRecoveryRecord>,
    pub(super) archive_next_cursor: Option<TransferId>,
    pub(super) archive_loading: bool,
    pub(super) archive_loaded: bool,
    pub(super) archive_error: Option<String>,
    archive_generation: u64,
    refresh_generation: u64,
}

impl Default for TransferQueueUi {
    /// 默认不加载无限历史；归档只保留一页，Rename 避免未经明确选择覆盖目标。
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            conflict_policy: ConflictPolicy::Rename,
            history_expanded: false,
            cleanup_retry: None,
            clear_completed_cancellation: None,
            recovery_cancellation: None,
            recovery_id: None,
            recovery_list_loading: false,
            recovery_list_error: None,
            expanded_recovery: None,
            archive_records: Vec::new(),
            archive_next_cursor: None,
            archive_loading: false,
            archive_loaded: false,
            archive_error: None,
            archive_generation: 0,
            refresh_generation: 0,
        }
    }
}

impl TransferQueueUi {
    /// 手动刷新和队列事件共用代次，归档后的新读取使旧活任务与恢复材料回执同时失效。
    /// @author kongweiguang
    pub(super) fn begin_transfer_refresh(&mut self) -> u64 {
        self.refresh_generation = self.refresh_generation.wrapping_add(1);
        self.recovery_list_loading = true;
        self.refresh_generation
    }

    /// 一次读取的活队列、恢复材料及错误提示共用同一道门禁，不能分开接受旧快照。
    /// @author kongweiguang
    pub(super) fn accepts_transfer_refresh(&self, generation: u64) -> bool {
        self.refresh_generation == generation
    }

    /// 恢复列表错误独立于文件目录状态；只接受当前读取回执，成功后才清除旧错误。
    /// @author kongweiguang
    pub(super) fn finish_recovery_refresh(
        &mut self,
        generation: u64,
        result: AppResult<()>,
    ) -> bool {
        if !self.accepts_transfer_refresh(generation) {
            return false;
        }
        self.recovery_list_loading = false;
        match result {
            Ok(()) => self.recovery_list_error = None,
            Err(error) => self.recovery_list_error = Some(super::localized_error(&error)),
        }
        true
    }

    /// 每次读取使此前代次失效，取消或迟到回执不能把新页的加载状态提前解除。
    /// @author kongweiguang
    fn begin_archive_page(&mut self) -> u64 {
        self.archive_generation = self.archive_generation.wrapping_add(1);
        self.archive_loading = true;
        self.archive_error = None;
        self.archive_generation
    }

    /// 仅接受当前代次；失败保留上一确认页供定位，成功则替换而非累加无限历史。
    /// @author kongweiguang
    fn finish_archive_page(
        &mut self,
        generation: u64,
        result: AppResult<TransferRecoveryPage>,
    ) -> bool {
        if self.archive_generation != generation {
            return false;
        }
        self.archive_loading = false;
        self.archive_loaded = true;
        match result {
            Ok(page) => {
                self.archive_records = page.records;
                self.archive_next_cursor = page.next_cursor;
            }
            Err(error) => self.archive_error = Some(super::localized_error(&error)),
        }
        true
    }

    /// 同一面板只核对一个原事务，避免迟到回执覆盖另一项恢复反馈。
    pub(super) fn recovery_busy(&self) -> bool {
        self.recovery_cancellation.is_some()
    }

    /// 恢复按钮按精确 owner 显示进度，而不是根据当前文件面板推断目标。
    pub(super) fn recovery_id(&self) -> Option<&TransferId> {
        self.recovery_id.as_ref()
    }

    /// 暂存证据按需展开，一次只展示一个 owner，避免大量中断材料挤占终端工作区。
    pub(super) fn is_recovery_expanded(&self, id: &TransferId) -> bool {
        self.expanded_recovery.as_ref() == Some(id)
    }
    /// 返回正在运行的清理任务 ID，让渲染层为单个 owner 显示 busy 状态。
    ///
    /// @author kongweiguang
    pub(super) fn cleanup_retry_id(&self) -> Option<&TransferId> {
        self.cleanup_retry.as_ref().map(|retry| &retry.id)
    }

    /// 判断是否已有清理请求，防止同一 UI 对串行清理锁重复排队。
    ///
    /// @author kongweiguang
    pub(super) fn is_retrying_cleanup(&self) -> bool {
        self.cleanup_retry.is_some()
    }

    /// 取消只终止当前清理尝试的等待；待清理 owner 不从队列移除，可再次重试。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_cleanup_retry(&self, id: &TransferId) -> bool {
        let Some(retry) = self.cleanup_retry.as_ref().filter(|retry| &retry.id == id) else {
            return false;
        };
        retry.cancellation.cancel();
        if let Ok(active) = retry.active_cancellation.lock()
            && let Some(cancellation) = active.as_ref()
        {
            cancellation.cancel();
        }
        true
    }

    /// 同一面板只允许一个历史清理用例，避免重复提交竞争同一组 worker 句柄。
    ///
    /// @author kongweiguang
    pub(super) fn begin_clear_completed_history(&mut self) -> Option<CancellationToken> {
        if self.clear_completed_cancellation.is_some() {
            return None;
        }
        let cancellation = CancellationToken::new();
        self.clear_completed_cancellation = Some(cancellation.clone());
        Some(cancellation)
    }

    /// 保持 busy 状态直到服务端确认结束，取消只通知用例，不提前释放重入门闩。
    ///
    /// @author kongweiguang
    pub(super) fn finish_clear_completed_history(&mut self) -> bool {
        self.clear_completed_cancellation.take().is_some()
    }

    /// 用户取消只触发当前用例 token；应用层在删除线性化点前停止并保留记录。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_clear_completed_history(&self) -> bool {
        let Some(cancellation) = self.clear_completed_cancellation.as_ref() else {
            return false;
        };
        cancellation.cancel();
        true
    }

    /// 将面板卸载映射为协作取消，避免关闭文件面板后后台继续清理历史。
    ///
    /// @author kongweiguang
    pub(super) fn is_clearing_completed_history(&self) -> bool {
        self.clear_completed_cancellation.is_some()
    }
}

impl Drop for TransferQueueUi {
    /// 面板关闭时取消尚未越过删除线性化点的历史清理请求。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        if let Some(cancellation) = self.clear_completed_cancellation.as_ref() {
            cancellation.cancel();
        }
        if let Some(cancellation) = self.recovery_cancellation.as_ref() {
            cancellation.cancel();
        }
    }
}

impl FilesPane {
    /// 每次只替换一页冷档，不累加历史；代次阻止迟到页面覆盖刷新或另一页的权威回执。
    /// @author kongweiguang
    pub(super) fn load_transfer_archive_page(
        &mut self,
        cursor: Option<TransferId>,
        cx: &mut Context<Self>,
    ) {
        let generation = self.queue.begin_archive_page();
        let transfers = self.transfers.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    transfers
                        .list_archived_recovery(cursor, 50, cancellation)
                        .await
                })
                .await;
            let _ = this.update(cx, |pane, cx| {
                if pane.queue.finish_archive_page(generation, result) {
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// 展开只改变证据视图，不连接目标或推断暂存对象存在性。
    pub(super) fn toggle_recovery_staging(&mut self, id: TransferId, cx: &mut Context<Self>) {
        self.queue.expanded_recovery = if self.queue.is_recovery_expanded(&id) {
            None
        } else {
            Some(id)
        };
        cx.notify();
    }
    /// 恢复核对只读取持久目标证据；结果未知时仍保留记录，并明确给出下一步。
    pub(super) fn inspect_transfer_recovery(&mut self, id: TransferId, cx: &mut Context<Self>) {
        self.run_transfer_recovery(id, false, cx);
    }

    /// 归档是人工确认后的记录动作，不冒充目标提交成功，也不会释放未知 cleanup owner。
    pub(super) fn archive_transfer_recovery(&mut self, id: TransferId, cx: &mut Context<Self>) {
        self.run_transfer_recovery(id, true, cx);
    }

    /// 取消只通知当前核对等待，不提前清除 busy 状态或恢复材料。
    pub(super) fn cancel_transfer_recovery(&mut self, cx: &mut Context<Self>) {
        if let Some(cancellation) = self.queue.recovery_cancellation.as_ref() {
            cancellation.cancel();
            self.status = Some(locale::text("正在取消核对…", "Cancelling inspection…").to_string());
            cx.notify();
        }
    }

    /// 核对与归档不能创建传输 worker；归档回执后重新读取有界首页，不把旧核对结论改写为成功。
    /// @author kongweiguang
    fn run_transfer_recovery(&mut self, id: TransferId, archive: bool, cx: &mut Context<Self>) {
        if self.queue.recovery_busy() {
            return;
        }
        let cancellation = CancellationToken::new();
        self.queue.recovery_cancellation = Some(cancellation.clone());
        self.queue.recovery_id = Some(id.clone());
        self.status = Some(
            if archive {
                locale::text(
                    "正在归档原传输证据…",
                    "Archiving the original transfer evidence…",
                )
            } else {
                locale::text("正在核对原传输…", "Inspecting the original transfer…")
            }
            .to_string(),
        );
        let transfers = self.transfers.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor.run_cancellable(cancellation, move |token| async move {
                if archive {
                    transfers.archive_recovery(&id, token).await?;
                    Ok(None)
                } else {
                    transfers.inspect_recovery(&id, token).await.map(Some)
                }
            }).await;
            let _ = this.update(cx, |pane, cx| {
                pane.queue.recovery_cancellation = None;
                pane.queue.recovery_id = None;
                pane.status = Some(match result {
                    Ok(None) => locale::text("已人工归档；未知结果和清理材料保持原样，可在历史中查看，不代表传输成功。", "Manually archived. Unknown results and cleanup evidence are retained in history; this is not a successful transfer.").to_string(),
                    Ok(Some(record)) => match record.inspection {
                        kerminal_application::transfers::TransferRecoveryInspection::DestinationMatches => locale::text("目标内容与保存的完整证据一致；请检查后归档，不会自动重放。", "Destination matches the saved complete evidence. Review and archive; nothing is replayed.").to_string(),
                        kerminal_application::transfers::TransferRecoveryInspection::DestinationMissing => locale::text("原目标不存在；恢复记录已保留，请检查原路径后决定是否重新创建任务。", "The original destination is missing. Evidence is retained; review the path before creating a new task.").to_string(),
                        kerminal_application::transfers::TransferRecoveryInspection::DestinationDiffers => locale::text("原目标内容不同；不要覆盖或盲目重试，请先人工核对。", "Destination differs. Review it manually before overwriting or retrying.").to_string(),
                        kerminal_application::transfers::TransferRecoveryInspection::Insufficient => locale::text("现有证据不足以确定提交结果；记录继续保留，请在原目标人工核对。", "Evidence is insufficient. The record remains pending; inspect the original destination manually.").to_string(),
                    },
                    Err(error) => crate::features::files::localized_error(&error),
                });
                pane.reload_transfers(cx);
                if archive {
                    pane.load_transfer_archive_page(None, cx);
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    /// 后续传输按钮读取该值并写进 TransferRequest，入队后不会被 UI 更改影响。
    ///
    /// @author kongweiguang
    pub(super) fn set_conflict_policy(&mut self, policy: ConflictPolicy, cx: &mut Context<Self>) {
        self.queue.conflict_policy = policy;
        cx.notify();
    }

    /// 展开或折叠终态和冷档历史；未归档的未知提交和 cleanup_pending 始终由 renderer 保留。
    ///
    /// @author kongweiguang
    pub(super) fn toggle_transfer_history(&mut self, cx: &mut Context<Self>) {
        self.queue.history_expanded = !self.queue.history_expanded;
        if self.queue.history_expanded {
            self.load_transfer_archive_page(None, cx);
        }
        cx.notify();
    }

    /// 通过统一队列用例清理安全终态；未知提交和待清理资源由应用服务保留。
    ///
    /// @author kongweiguang
    pub(super) fn clear_completed_transfer_history(&mut self, cx: &mut Context<Self>) {
        let Some(cancellation) = self.queue.begin_clear_completed_history() else {
            self.status = Some(
                locale::text(
                    "传输历史正在清理。",
                    "Transfer history is already being cleared.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let transfers = self.transfers.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |task_cancellation| async move {
                    transfers.clear_completed(task_cancellation).await
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    pane.queue.finish_clear_completed_history();
                    pane.status = Some(match result {
                        Ok((removed, remaining)) => locale::text(
                            format!(
                                "已清理 {removed} 条可清理历史，仍保留 {remaining} 个任务。"
                            ),
                            format!(
                                "Removed {removed} clearable history entries; {remaining} tasks remain."
                            ),
                        )
                        .to_string(),
                        Err(error)
                            if error.public_code()
                                == kerminal_application::PublicErrorCode::Cancelled =>
                        {
                            locale::text(
                                "已取消历史清理；需要核对或仍待清理的任务继续保留。",
                                "History cleanup was cancelled; tasks needing review or resource cleanup remain.",
                            )
                            .to_string()
                        }
                        Err(error) => crate::features::files::localized_error(&error),
                    });
                    pane.reload_transfers(cx);
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 取消当前清理尝试但保留 busy 状态，直到队列返回权威完成或取消结果。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_completed_transfer_history(&mut self, cx: &mut Context<Self>) {
        if self.queue.cancel_clear_completed_history() {
            self.status = Some(
                locale::text(
                    "已请求取消历史清理；正在确认任务状态。",
                    "History cleanup cancellation requested; confirming queue state.",
                )
                .to_string(),
            );
            cx.notify();
        }
    }

    /// 用同一个队列 owner 重试资源关闭；取消或失败后刷新权威快照以保留待办入口。
    ///
    /// @author kongweiguang
    pub(super) fn retry_cleanup(&mut self, id: TransferId, cx: &mut Context<Self>) {
        if self.queue.is_retrying_cleanup() {
            self.status = Some(
                locale::text(
                    "已有清理重试正在执行。",
                    "A cleanup retry is already running.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }

        let cancellation = CancellationToken::new();
        let active_cancellation = Arc::new(Mutex::new(None));
        self.queue.cleanup_retry = Some(CleanupRetry {
            id: id.clone(),
            cancellation: cancellation.clone(),
            active_cancellation: Arc::clone(&active_cancellation),
        });
        let transfers = self.transfers.clone();
        let executor = self.executor.clone();
        let task_id = id.clone();
        let inspect_id = id.clone();
        let task_active_cancellation = active_cancellation;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |task_cancellation| async move {
                    if let Ok(mut active) = task_active_cancellation.lock() {
                        *active = Some(task_cancellation.clone());
                    }
                    if cancellation.is_cancelled() {
                        task_cancellation.cancel();
                    }
                    let deadline = std::time::Instant::now() + Duration::from_secs(30);
                    match task_cancellation
                        .run_until_cancelled(
                            transfers.retry_cleanup_until(&task_id, deadline.into()),
                        )
                        .await
                    {
                        Some(result) => result,
                        None => match transfers.get(&inspect_id).await {
                            Ok(summary) if !summary.cleanup_pending => Ok(()),
                            _ => Err(AppError::cancelled("清理重试已取消；资源 owner 仍保留")),
                        },
                    }
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |pane, cx| {
                    if pane.queue.cleanup_retry_id() == Some(&id) {
                        pane.queue.cleanup_retry = None;
                    }
                    pane.status = Some(match result {
                        Ok(()) => locale::text(
                            "资源已确认清理。",
                            "The resources were confirmed released.",
                        )
                        .to_string(),
                        Err(error) => localized_cleanup_error(&error),
                    });
                    pane.reload_transfers(cx);
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 取消当前 owner 的清理重试等待，不修改目标数据、不删除待清理记录。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_cleanup_retry(&mut self, id: &TransferId, cx: &mut Context<Self>) {
        if self.queue.cancel_cleanup_retry(id) {
            self.status = Some(
                locale::text(
                    "已请求停止清理尝试；待清理资源仍保留，可再次重试。",
                    "Cleanup retry cancellation requested; the owner remains available to retry.",
                )
                .to_string(),
            );
            cx.notify();
        }
    }
}

/// 让未知提交和资源清理错误使用明确提示，不把 owner 保留误报为数据写入失败。
///
/// @author kongweiguang
fn localized_cleanup_error(error: &AppError) -> String {
    if error.public_code() == kerminal_application::PublicErrorCode::Cancelled {
        return locale::text(
            "清理尝试已停止；待清理资源仍保留。",
            "Cleanup attempt stopped; the resource owner is still retained.",
        )
        .to_string();
    }
    if error.public_code() == kerminal_application::PublicErrorCode::Timeout {
        return locale::text(
            "清理超过本次等待期限；资源仍待处理。",
            "Cleanup exceeded this attempt's deadline; resources remain pending.",
        )
        .to_string();
    }
    crate::features::files::localized_error(error)
}

#[cfg(test)]
#[path = "../../../tests/unit/files_transfer_queue_state.rs"]
mod tests;
