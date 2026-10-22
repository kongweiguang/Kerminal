//! 脱离工作区布局但仍由 SessionService 持有的终端清理入口。
//!
//! 面板只投影 WorkspaceRuntime 的唯一 owner；关闭意图和回执始终使用原 SessionId。
//!
//! @author kongweiguang

use std::future::Future;

use gpui_kit::{
    ClickEvent, Context, EventEmitter, IntoElement, Render, SharedString, Window,
    component::{
        Disableable as _,
        button::{Button, ButtonVariants as _},
        scroll::ScrollableElement as _,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::sessions::{DEFAULT_SESSION_CLOSE_BUDGET, SessionDeadline};
use kerminal_domain::identity::{PaneId, SessionId, TabId};

use super::{RootView, workspace_runtime::WorkspaceRuntime};
use crate::theme::ThemeTokens;

const ORPHAN_CLEANUP_PAGE_SIZE: usize = 8;

/// Orphan 列表行从 Runtime owner 生成，不保存 target 凭据或第二份清理状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WorkspaceOrphanCleanupItem {
    pub session_id: SessionId,
    pub tab_id: TabId,
    pub pane_id: PaneId,
    pub status: SharedString,
    pub cleanup_in_flight: bool,
}

/// 面板只表达按原 SessionId 重试；批量事件由 Root 用同一个绝对 deadline 顺序执行。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WorkspaceOrphanCleanupEvent {
    Retry(SessionId),
    RetryAll,
}

/// 清理面板不保留资源 owner；列表滚动状态由 Kit 的 Scrollable 元素按身份持有。
pub(super) struct WorkspaceOrphanCleanupPanel {
    items: Vec<WorkspaceOrphanCleanupItem>,
    page: usize,
}

impl EventEmitter<WorkspaceOrphanCleanupEvent> for WorkspaceOrphanCleanupPanel {}

impl WorkspaceOrphanCleanupPanel {
    /// 创建无业务状态的面板；真实清理记录只由 Root 从 WorkspaceRuntime 注入。
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            page: 0,
        }
    }

    /// 替换 Runtime 投影并保留滚动位置，避免显示层成为第二份孤立资源注册表。
    pub fn set_items(
        &mut self,
        mut items: Vec<WorkspaceOrphanCleanupItem>,
        cx: &mut Context<Self>,
    ) {
        items.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        self.items = items;
        let page_count = self.items.len().div_ceil(ORPHAN_CLEANUP_PAGE_SIZE).max(1);
        self.page = self.page.min(page_count - 1);
        cx.notify();
    }

    /// 告知 Root 面板是否应占据全局清理区域，空状态不留下无意义横幅。
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 按真实 owner projection 绘制有界滚动列表，清理失败项一直保留可重试身份。
    fn render_items(
        &self,
        theme: ThemeTokens,
        start: usize,
        cx: &mut Context<Self>,
    ) -> Vec<gpui_kit::AnyElement> {
        self.items
            .iter()
            .skip(start)
            .take(ORPHAN_CLEANUP_PAGE_SIZE)
            .map(|item| {
                let session_id = item.session_id.clone();
                let status = item.status.clone();
                let session_label = SharedString::from(item.session_id.as_str());
                let origin = crate::locale::text(
                    format!("原标签 {} · pane {}", item.tab_id, item.pane_id),
                    format!("Original tab {} · pane {}", item.tab_id, item.pane_id),
                );
                let retry_label = if item.cleanup_in_flight {
                    crate::locale::text("正在清理…", "Cleaning…")
                } else {
                    crate::locale::text("重试", "Retry")
                };
                div()
                    .id(format!("orphan-session-{}", item.session_id))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(theme.background)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme.muted)
                                    .child(origin),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme.foreground)
                                    .child(session_label),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(if item.cleanup_in_flight {
                                        theme.muted
                                    } else {
                                        theme.error
                                    })
                                    .child(status),
                            ),
                    )
                    .child(
                        Button::new(format!("retry-orphan-{}", item.session_id))
                            .compact()
                            .ghost()
                            .disabled(item.cleanup_in_flight)
                            .label(retry_label)
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                cx.emit(WorkspaceOrphanCleanupEvent::Retry(session_id.clone()));
                            })),
                    )
                    .into_any_element()
            })
            .collect()
    }
}

impl Default for WorkspaceOrphanCleanupPanel {
    /// 让 GPUI entity 装配保持轻量，初始投影为空且不推断旧资源状态。
    fn default() -> Self {
        Self::new()
    }
}

impl Render for WorkspaceOrphanCleanupPanel {
    /// 清理面板跟随 Kit 全局浅深主题，列表高度与可见行数受控。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        let count = self.items.len();
        let page_count = count.div_ceil(ORPHAN_CLEANUP_PAGE_SIZE).max(1);
        let page = self.page.min(page_count - 1);
        let can_retry_all = self.items.iter().any(|item| !item.cleanup_in_flight);
        let items = self.render_items(theme, page * ORPHAN_CLEANUP_PAGE_SIZE, cx);

        div()
            .id("workspace-orphan-cleanup")
            .w_full()
            .max_h(px(280.))
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .rounded_md()
            .bg(theme.panel)
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .text_color(theme.foreground)
                            .child(crate::locale::text(
                                format!("有 {count} 个终端等待清理"),
                                format!("{count} orphan terminals need cleanup"),
                            )),
                    )
                    .child(
                        Button::new("retry-all-orphan-sessions")
                            .compact()
                            .primary()
                            .disabled(!can_retry_all)
                            .label(crate::locale::text("重试全部", "Retry all"))
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                cx.emit(WorkspaceOrphanCleanupEvent::RetryAll);
                            })),
                    )
                    .child(div().text_size(px(11.)).text_color(theme.muted).child(
                        crate::locale::text(
                            format!("第 {} / {} 页", page + 1, page_count),
                            format!("Page {} / {}", page + 1, page_count),
                        ),
                    ))
                    .child(
                        Button::new("workspace-orphan-cleanup-previous")
                            .compact()
                            .ghost()
                            .disabled(page == 0)
                            .label("‹")
                            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                panel.page = panel.page.saturating_sub(1);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("workspace-orphan-cleanup-next")
                            .compact()
                            .ghost()
                            .disabled(page + 1 >= page_count)
                            .label("›")
                            .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                                panel.page = (panel.page + 1).min(page_count - 1);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("workspace-orphan-cleanup-list")
                    .w_full()
                    .max_h(px(224.))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .overflow_y_scrollbar()
                    .children(items),
            )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OrphanCleanupTicket {
    session_id: SessionId,
    generation: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum OrphanCleanupCompletion {
    Removed,
    Retryable,
    Stale,
}

/// 只把匹配在途代次的明确 close ACK 应用到 owner；失败与取消都保留原 owner。
/// 返回原 owner 供唯一收尾入口释放附属资源，迟到或重复 ACK 不产生释放凭据。
///
/// @author kongweiguang
fn apply_orphan_cleanup_result(
    runtime: &mut WorkspaceRuntime,
    ticket: &OrphanCleanupTicket,
    result: Result<(), String>,
    failure_status: SharedString,
) -> (
    OrphanCleanupCompletion,
    Option<super::workspace_runtime::OrphanCleanupSession>,
) {
    match result {
        Ok(()) => match runtime.finish_orphan_cleanup(&ticket.session_id, ticket.generation) {
            Some(owner) => (OrphanCleanupCompletion::Removed, Some(owner)),
            None => (OrphanCleanupCompletion::Stale, None),
        },
        Err(_) => {
            if runtime.fail_orphan_cleanup(&ticket.session_id, ticket.generation, failure_status) {
                (OrphanCleanupCompletion::Retryable, None)
            } else {
                (OrphanCleanupCompletion::Stale, None)
            }
        }
    }
}

/// 序列化执行批量 close；单并发和同一绝对 deadline 限制退出前清理占用。
async fn close_orphan_cleanup_batch<F, Fut>(
    tickets: Vec<OrphanCleanupTicket>,
    deadline: SessionDeadline,
    mut close_one: F,
) -> Vec<(OrphanCleanupTicket, Result<(), String>)>
where
    F: FnMut(SessionId, SessionDeadline) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let mut results = Vec::with_capacity(tickets.len());
    for ticket in tickets {
        let result = close_one(ticket.session_id.clone(), deadline).await;
        results.push((ticket, result));
    }
    results
}

/// 根视图通过此 helper 同步孤立 owner 投影，业务事实仍全部来自 Runtime。
pub(super) fn orphan_cleanup_items(runtime: &WorkspaceRuntime) -> Vec<WorkspaceOrphanCleanupItem> {
    runtime
        .orphan_cleanup_sessions()
        .map(|(_, owner)| WorkspaceOrphanCleanupItem {
            session_id: owner.session_id.clone(),
            tab_id: owner.tab_id.clone(),
            pane_id: owner.pane_id.clone(),
            status: owner.status.clone(),
            cleanup_in_flight: owner.cleanup_in_flight,
        })
        .collect()
}

impl RootView {
    /// 三种关闭回执共用 owner 退役门禁；只有当前代次成功才清理工具并刷新原容器目标。
    /// 先从 Runtime 取回原 owner，避免以当前 pane 或布局操作代次误清理新资源。
    ///
    /// @author kongweiguang
    pub(super) fn apply_workspace_orphan_cleanup_result(
        &mut self,
        session_id: &SessionId,
        generation: u64,
        result: Result<(), String>,
        cx: &mut Context<RootView>,
    ) -> OrphanCleanupCompletion {
        let failure_status = result
            .as_ref()
            .err()
            .map(|message| {
                crate::locale::text(
                    format!("关闭未确认，资源仍保留，可重试：{message}"),
                    format!(
                        "Close was not confirmed; the resource is retained for retry: {message}"
                    ),
                )
            })
            .unwrap_or_else(|| crate::locale::text("清理完成。", "Cleanup complete."));
        let ticket = OrphanCleanupTicket {
            session_id: session_id.clone(),
            generation,
        };
        let (completion, removed_owner) = apply_orphan_cleanup_result(
            &mut self.workspace_runtime,
            &ticket,
            result,
            failure_status,
        );
        if let Some(owner) = removed_owner {
            let cancelled_split_operation = self
                .workspace_resource_operations
                .get(&owner.pane_id)
                .filter(|entry| entry.cancel_requested())
                .map(|entry| entry.operation_id().to_owned());
            self.mark_container_exec_target_dirty(&owner.pane_id, cx);
            if let Some(tools) = self.workspace_tools.remove(&owner.session_id) {
                self.terminal_tools_runtime.cancel_source(tools.entity_id());
            }
            self.workspace_tool_subscriptions.remove(&owner.session_id);
            if matches!(&completion, OrphanCleanupCompletion::Removed)
                && let Some(operation_id) = cancelled_split_operation
            {
                self.schedule_cancelled_workspace_split_finalize(
                    &owner.tab_id,
                    &owner.pane_id,
                    &operation_id,
                    cx,
                );
            }
        }
        completion
    }

    /// 面板订阅收到的重试事件只用明确 session ID 查找唯一 Runtime owner。
    /// 回执与布局关闭共用退役入口，失败或过期结果均保留 owner 及附属资源。
    ///
    /// @author kongweiguang
    pub(super) fn handle_workspace_orphan_cleanup_event(
        &mut self,
        event: WorkspaceOrphanCleanupEvent,
        cx: &mut Context<RootView>,
    ) {
        let session_ids = match event {
            WorkspaceOrphanCleanupEvent::Retry(session_id) => vec![session_id],
            WorkspaceOrphanCleanupEvent::RetryAll => self
                .workspace_runtime
                .orphan_cleanup_sessions()
                .map(|(session_id, _)| session_id.clone())
                .collect(),
        };
        let mut tickets = Vec::new();
        for session_id in session_ids {
            let generation = self.workspace_runtime.begin_orphan_cleanup(
                &session_id,
                crate::locale::text(
                    "正在关闭；资源 owner 保留到确认完成。",
                    "Closing; the resource owner stays until confirmed.",
                ),
            );
            if let Ok(generation) = generation {
                tickets.push(OrphanCleanupTicket {
                    session_id,
                    generation,
                });
            }
        }
        for ticket in &tickets {
            self.cancel_workspace_session_input_for_close(&ticket.session_id, cx);
            if let Some(watcher) = self.session_watch_cancellations.remove(&ticket.session_id) {
                watcher.cancel();
            }
            self.terminal_tools_runtime
                .cancel_session(&ticket.session_id);
        }
        self.refresh_workspace_orphan_cleanup(cx);
        if tickets.is_empty() {
            return;
        }

        let close_deadline = SessionDeadline::now() + DEFAULT_SESSION_CLOSE_BUDGET;
        let completion_tickets = tickets.clone();
        let services = self.services.clone();
        let session_service = services.sessions.clone();
        let executor = services.executor.clone();
        cx.spawn(async move |this, cx| {
            let results = executor
                .run(move |_cancellation| async move {
                    Ok(close_orphan_cleanup_batch(
                        tickets,
                        close_deadline,
                        move |session_id, deadline| {
                            let sessions = session_service.clone();
                            async move {
                                sessions
                                    .close_until(session_id, deadline)
                                    .await
                                    .map_err(|error| error.public_message().to_owned())
                            }
                        },
                    )
                    .await)
                })
                .await;
            let results = match results {
                Ok(results) => results,
                Err(error) => completion_tickets
                    .into_iter()
                    .map(|ticket| (ticket, Err(error.public_message().to_owned())))
                    .collect(),
            };
            let _ = this.update(cx, |root, cx| {
                for (ticket, result) in results {
                    root.apply_workspace_orphan_cleanup_result(
                        &ticket.session_id,
                        ticket.generation,
                        result,
                        cx,
                    );
                }
                root.refresh_workspace_orphan_cleanup(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// 只刷新真实 orphan projection，Root 决定是否把空面板从全局位置隐藏。
    pub(super) fn refresh_workspace_orphan_cleanup(&mut self, cx: &mut Context<RootView>) {
        let items = orphan_cleanup_items(&self.workspace_runtime);
        if let Some(panel) = self.workspace_orphan_cleanup_panel.as_ref() {
            panel.update(cx, |panel, cx| panel.set_items(items, cx));
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_workspace_orphan_cleanup.rs"]
mod tests;
