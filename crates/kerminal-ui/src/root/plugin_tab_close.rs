//! 标签工具与原有内容共用一次关闭预算，工作区提交前保留各实例所有者。
//!
//! @author kongweiguang

use super::workspace_close::{CloseSurvivorFocusIntent, CloseTabFocusIntent};
use super::*;
use crate::features::workspace_controller::WorkspaceResourceOperation;
use crate::plugins::tools::{ToolContext, ToolPhase};
use std::{cell::RefCell, rc::Rc};

/// 各工具并发收尾并分别登记回执，避免前一项阻塞后续工具开始释放资源。
struct TabToolCloseBatch {
    remaining: usize,
    first_error: Option<String>,
}

impl RootView {
    /// 首次资源关闭建立绝对截止时间；工具、内容及终端层不得逐层重置预算。
    ///
    /// @author kongweiguang
    pub(super) fn plugin_tab_close_deadline(&mut self, tab_id: &TabId) -> std::time::Instant {
        *self
            .plugin_tab_close_deadlines
            .entry(tab_id.clone())
            .or_insert_with(|| {
                std::time::Instant::now()
                    + kerminal_application::sessions::DEFAULT_SESSION_CLOSE_BUDGET
            })
    }

    /// 先回收该标签的工具，再继续原有资源关闭；已完成项在重试时跳过。
    /// 即使另一个工具拒绝开始关闭，也等待已经开始的任务回执，避免遗留永久 Closing。
    ///
    /// @author kongweiguang
    pub(super) fn defer_plugin_tool_tab_close(
        &mut self,
        operation: &WorkspaceResourceOperation,
        close_focus: Option<CloseSurvivorFocusIntent>,
        close_tab_focus: Option<CloseTabFocusIntent>,
        cx: &mut Context<Self>,
    ) -> bool {
        let WorkspaceResourceOperation::CloseTab {
            operation_id,
            tab_id,
            ..
        } = operation
        else {
            return false;
        };
        let context = ToolContext::new(Some(tab_id.clone()), None);
        let tools = self.plugin_tools.tools_for_tab(tab_id);
        let deadline = self.plugin_tab_close_deadline(tab_id);
        let mut jobs = Vec::new();
        let mut first_error = None;
        for tool in tools {
            match self.plugin_tools.phase(tool, &context) {
                Some(ToolPhase::Closed) | None => continue,
                Some(ToolPhase::Closing) => {
                    first_error
                        .get_or_insert_with(|| "工具仍在关闭，请等待原操作完成。".to_owned());
                }
                Some(ToolPhase::Open | ToolPhase::CloseFailed) => {
                    match self.plugin_tools.begin_close(tool, &context, deadline, cx) {
                        Ok(job) => jobs.push(job),
                        Err(error) => {
                            first_error.get_or_insert_with(|| error.public_message().to_owned());
                        }
                    }
                }
            }
        }
        if jobs.is_empty() {
            if let Some(message) = first_error {
                self.complete_workspace_resource_failure(operation_id, message.into(), cx);
                return true;
            }
            return false;
        }
        let operation = operation.clone();
        let batch = Rc::new(RefCell::new(TabToolCloseBatch {
            remaining: jobs.len(),
            first_error,
        }));
        for (token, future) in jobs {
            let batch = Rc::clone(&batch);
            let operation = operation.clone();
            let close_focus = close_focus.clone();
            let close_tab_focus = close_tab_focus.clone();
            let executor = self.services.executor.clone();
            cx.spawn(async move |this, cx| {
                let result = executor
                    .run(move |_| {
                        kerminal_application::executor::await_until(
                            deadline,
                            "关闭标签工具",
                            future,
                        )
                    })
                    .await;
                let _ = this.update(cx, |root, cx| {
                    let finished = {
                        let mut batch = batch.borrow_mut();
                        if let Err(error) = root.plugin_tools.complete_close(token, result) {
                            batch
                                .first_error
                                .get_or_insert_with(|| error.public_message().to_owned());
                        }
                        batch.remaining -= 1;
                        (batch.remaining == 0).then(|| batch.first_error.take())
                    };
                    let Some(first_error) = finished else {
                        return;
                    };
                    let current = root
                        .workspace_controller
                        .as_ref()
                        .and_then(|controller| controller.pending_resource_operation())
                        .is_some_and(|current| current.operation_id() == operation.operation_id());
                    if !current {
                        return;
                    }
                    if let Some(message) = first_error {
                        root.complete_workspace_resource_failure(
                            operation.operation_id(),
                            message.into(),
                            cx,
                        );
                    } else if let WorkspaceResourceOperation::CloseTab {
                        operation_id, tab, ..
                    } = &operation
                        && !matches!(
                            tab.as_ref(),
                            WorkspaceSessionTab::Terminal { .. }
                                | WorkspaceSessionTab::Extension { .. }
                        )
                    {
                        // 文件草稿门禁已通过；不能重入通用路径再次发出可取消的询问。
                        root.complete_nonterminal_workspace_tab_close(
                            operation_id.clone(),
                            tab.id().clone(),
                            cx,
                        );
                    } else {
                        root.apply_workspace_resource_operation_with_focus(
                            operation,
                            close_focus,
                            close_tab_focus,
                            cx,
                        );
                    }
                });
            })
            .detach();
        }
        true
    }

    /// 文件编辑器已确认关闭后才回收插件工具；取消草稿询问不会触及工具 owner。
    /// 原 controller 身份仍须匹配，迟到的编辑器回执不能关闭另一个标签。
    ///
    /// @author kongweiguang
    pub(super) fn close_guarded_nonterminal_tab(
        &mut self,
        operation_id: String,
        tab_id: TabId,
        cx: &mut Context<Self>,
    ) {
        let operation = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.pending_resource_operation())
            .filter(|operation| {
                matches!(operation, WorkspaceResourceOperation::CloseTab {
                operation_id: current, tab_id: current_tab, ..
            } if current == &operation_id && current_tab == &tab_id)
            })
            .cloned();
        let Some(operation) = operation else {
            return;
        };
        if !self.defer_plugin_tool_tab_close(&operation, None, None, cx) {
            self.complete_nonterminal_workspace_tab_close(operation_id, tab_id, cx);
        }
    }

    /// 工作区确认移除后释放工具实体；失败时保留 owner，不能将超时视为已回收。
    ///
    /// @author kongweiguang
    pub(super) fn finalize_plugin_tab_tools(&mut self, tab_id: &TabId) {
        let context = ToolContext::new(Some(tab_id.clone()), None);
        for tool in self.plugin_tools.tools_for_tab(tab_id) {
            if let Err(error) = self.plugin_tools.finalize_close(tool, &context) {
                self.status = Some(error.public_message().to_owned().into());
            }
        }
        self.plugin_tab_close_deadlines.remove(tab_id);
    }
}
