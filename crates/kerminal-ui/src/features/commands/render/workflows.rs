//! 顺序工作流编辑、风险确认与执行状态视图。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
        input::{Input, Textarea},
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::commands::{WorkflowRun, WorkflowRunState, WorkflowStepState};
use kerminal_domain::sessions::SessionTarget;

use super::super::{CommandsView, WorkflowUnknownReview};
use crate::theme::ThemeTokens;

impl CommandsView {
    /// 工作流待确认时锁定定义并展示完整脚本；未知结果保留原目标摘要并在核对前禁用新运行。
    /// 步骤卡片使用自然高度并共用外层滚动，避免运行面板扩展时内层 flex 视口收缩到零。
    ///
    /// @author kongweiguang
    pub(super) fn render_workflows(
        &mut self,
        theme: ThemeTokens,
        _window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entries = self
            .workflows
            .iter()
            .map(|workflow| {
                let id = workflow.id.clone();
                let title = workflow.title.clone();
                let active = self
                    .selected_workflow
                    .as_ref()
                    .is_some_and(|selected| selected.definition.id == id);
                div()
                    .id(format!("workflow-entry-{id}"))
                    .w_full()
                    .p_3()
                    .rounded_md()
                    .bg(if active { theme.accent } else { theme.panel })
                    .text_color(if active {
                        theme.background
                    } else {
                        theme.foreground
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                        view.open_workflow(id.clone(), window, cx);
                    }))
                    .child(title)
            })
            .collect::<Vec<_>>();
        let selected_title = self
            .selected_workflow
            .as_ref()
            .map(|selected| selected.definition.title.clone())
            .unwrap_or_else(|| crate::locale::text("新建工作流", "New workflow").to_string());
        let progress_for_run = self.workflow_progress.as_ref().filter(|progress| {
            self.workflow_run
                .as_ref()
                .is_some_and(|run| run.id() == progress.run_id)
        });
        let current_step_index = progress_for_run
            .and_then(|progress| progress.current_step_index)
            .or_else(|| self.workflow_run.as_ref().map(WorkflowRun::current_step));
        let run_state = progress_for_run
            .map(|progress| progress.state)
            .or_else(|| self.workflow_run.as_ref().map(|run| run.state()));
        let current_step = progress_for_run
            .and_then(|progress| {
                progress
                    .current_step_index
                    .and_then(|index| progress.steps.get(index).map(|step| step.state))
            })
            .or_else(|| {
                self.workflow_run
                    .as_ref()
                    .and_then(|run| run.current_result())
                    .map(|step| step.state)
            });
        let can_retry = self.workflow_binding_is_current()
            && self.workflow_run.as_ref().is_some_and(|run| {
                run.state() == WorkflowRunState::Failed
                    && matches!(
                        run.current_result().map(|step| step.state),
                        Some(WorkflowStepState::Failed | WorkflowStepState::NotStarted)
                    )
            });
        let is_waiting = run_state == Some(WorkflowRunState::AwaitingConfirmation);
        let is_running = self.workflow_running;
        let is_cancelling = is_running
            && self.workflow_control.as_ref().is_some_and(
                kerminal_application::commands::WorkflowExecutionControl::is_cancelled,
            );
        let editor_locked = self.workflow_editor_locked();
        let approval_target = self
            .workflow_session_info
            .as_ref()
            .or(self.session_info.as_ref())
            .map(|session| {
                let pane = super::super::session_title(session);
                match &session.target {
                    SessionTarget::Local(_) => crate::locale::text(
                        format!("本地终端 · {pane}"),
                        format!("Local terminal · {pane}"),
                    )
                    .to_string(),
                    SessionTarget::Ssh(target) => crate::locale::text(
                        format!("SSH 主机 {} · {pane}", target.host_id),
                        format!("SSH host {} · {pane}", target.host_id),
                    )
                    .to_string(),
                    _ => crate::locale::text("不支持的终端目标", "Unsupported terminal target")
                        .to_string(),
                }
            });
        let current_definition_step = self.selected_workflow.as_ref().and_then(|selected| {
            current_step_index.and_then(|index| selected.definition.steps.get(index))
        });
        let approval_script = current_definition_step.map(|step| step.command.clone());
        let approval_script_lines = approval_script
            .as_deref()
            .unwrap_or("")
            .split('\n')
            .map(|line| {
                div()
                    .w_full()
                    .font_family("monospace")
                    .child(line.to_owned())
            })
            .collect::<Vec<_>>();
        let approval_step = current_definition_step.map(|step| {
            crate::locale::text(
                format!("步骤：{}", step.title),
                format!("Step: {}", step.title),
            )
            .to_string()
        });
        let can_delete = self
            .selected_workflow
            .as_ref()
            .is_some_and(|selected| selected.revision.is_some());
        let can_run = self.workflow_draft_matches_saved(cx)
            && self.session_id.is_some()
            && self
                .ui_session_binding
                .as_ref()
                .is_some_and(|binding| self.session_id.as_ref() == Some(&binding.session_id))
            && !self
                .workflow_unknown_review
                .as_ref()
                .is_some_and(WorkflowUnknownReview::blocks_new_run)
            && !editor_locked;
        let attempts = self
            .workflow_attempts
            .iter()
            .filter(|step| progress_for_run.is_none() || step.history_error_code.is_some())
            .map(|step| {
                let status = step
                    .outcome
                    .as_ref()
                    .map(|outcome| format!("{outcome:?}"))
                    .or_else(|| {
                        step.error_code.map(|code| {
                            super::super::command_message(
                                format!("未注入：{code:?}"),
                                format!("Not injected: {code:?}"),
                            )
                        })
                    })
                    .unwrap_or_else(|| {
                        crate::locale::text("没有完成结果", "No result recorded").to_string()
                    });
                div()
                    .w_full()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .rounded_md()
                    .bg(theme.panel)
                    .child(div().text_size(px(12.)).child(crate::locale::text(
                        format!("步骤 {}", step.step_id),
                        format!("Step {}", step.step_id),
                    )))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.muted)
                            .child(status),
                    )
                    .when(step.history_error_code.is_some(), |row| {
                        row.child(div().text_size(px(10.)).text_color(theme.error).child(
                            crate::locale::text(
                                "命令结果已保留，但历史记录写入失败",
                                "Command result was retained, but history could not be saved",
                            ),
                        ))
                    })
            })
            .collect::<Vec<_>>();
        let progress_panel = progress_for_run.map(|progress| {
            let pane = self
                .workflow_session_info
                .as_ref()
                .filter(|session| session.session_id == progress.session_id);
            let target = workflow_progress_target_label(progress, pane);
            let rows = progress
                .steps
                .iter()
                .enumerate()
                .map(|(index, step)| {
                    let status = step
                        .exit_code
                        .map(|code| {
                            crate::locale::text(
                                format!("退出码 {code}"),
                                format!("Exit code {code}"),
                            )
                            .to_string()
                        })
                        .or_else(|| {
                            step.error_code.map(|code| {
                                super::super::command_message(
                                    format!("操作未完成：{code:?}"),
                                    format!("Operation did not complete: {code:?}"),
                                )
                            })
                        })
                        .or_else(|| step.message.clone())
                        .unwrap_or_else(|| workflow_step_label(step.state));
                    let failed = matches!(
                        step.state,
                        WorkflowStepState::Failed | WorkflowStepState::Unknown
                    );
                    div()
                        .w_full()
                        .p_2()
                        .rounded_md()
                        .bg(if progress.current_step_index == Some(index) {
                            theme.background
                        } else {
                            theme.panel
                        })
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().text_size(px(12.)).child(format!(
                            "{}. {} · {}",
                            index + 1,
                            step.title,
                            workflow_step_label(step.state)
                        )))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(if failed { theme.error } else { theme.muted })
                                .child(status),
                        )
                })
                .collect::<Vec<_>>();
            div()
                .w_full()
                .p_3()
                .rounded_md()
                .bg(theme.panel)
                .flex()
                .flex_col()
                .gap_2()
                .child(div().text_size(px(12.)).child(crate::locale::text(
                    format!("运行目标：{target}"),
                    format!("Run target: {target}"),
                )))
                .children(rows)
                .into_any_element()
        });
        let unknown_review_panel =
            self.workflow_unknown_review.clone().map(|review| {
                let requires_review = review.blocks_new_run();
                div()
                    .id("workflow-unknown-review")
                    .w_full()
                    .p_3()
                    .rounded_md()
                    .bg(theme.warning.opacity(0.1))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_size(px(12.)).child(crate::locale::text(
                        "上次工作流回执未知；原结果保留为未知",
                        "The previous workflow receipt is unknown; its result remains unknown",
                    )))
                    .child(div().text_size(px(11.)).text_color(theme.muted).child(
                        crate::locale::text(
                            format!("原运行：{}", review.run_id),
                            format!("Original run: {}", review.run_id),
                        ),
                    ))
                    .child(div().text_size(px(11.)).child(crate::locale::text(
                        format!("原目标：{}", review.target),
                        format!("Original target: {}", review.target),
                    )))
                    .child(div().text_size(px(11.)).child(crate::locale::text(
                        format!("未确认步骤：{}", review.step_title),
                        format!("Unconfirmed step: {}", review.step_title),
                    )))
                    .when(requires_review, |panel| {
                        panel.child(
                            Button::new("workflow-unknown-review-acknowledge")
                                .compact()
                                .primary()
                                .label(crate::locale::text(
                                    "我已核对原终端",
                                    "I checked the original terminal",
                                ))
                                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                    view.acknowledge_workflow_unknown_review(cx);
                                })),
                        )
                    })
                    .when(!requires_review, |panel| {
                        panel.child(div().text_size(px(11.)).text_color(theme.muted).child(
                            crate::locale::text(
                                "已核对；原运行仍记录为未知",
                                "Reviewed; the original run is still recorded as unknown",
                            ),
                        ))
                    })
                    .into_any_element()
            });
        let step_editors = self
            .workflow_steps
            .iter()
            .enumerate()
            .map(|(index, step)| {
                let confirmation = step.definition.requires_confirmation;
                let can_move_up = index > 0;
                let can_move_down = index + 1 < self.workflow_steps.len();
                let can_remove = self.workflow_steps.len() > 1;
                div()
                    .id(format!("workflow-step-editor-{index}"))
                    .w_full()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .rounded_md()
                    .bg(theme.panel)
                    .child(
                        div()
                            .flex()
                            .when(self.compact_layout, |row| row.flex_wrap())
                            .items_center()
                            .gap_2()
                            .child(div().flex_1().text_size(px(12.)).child(crate::locale::text(
                                format!("步骤 {}", index + 1),
                                format!("Step {}", index + 1),
                            )))
                            .child(
                                Button::new(format!("workflow-step-up-{index}"))
                                    .compact()
                                    .label(crate::locale::text("上移", "Move up"))
                                    .disabled(!can_move_up || editor_locked)
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.move_workflow_step(index, -1, cx);
                                    })),
                            )
                            .child(
                                Button::new(format!("workflow-step-down-{index}"))
                                    .compact()
                                    .label(crate::locale::text("下移", "Move down"))
                                    .disabled(!can_move_down || editor_locked)
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.move_workflow_step(index, 1, cx);
                                    })),
                            )
                            .child(
                                Button::new(format!("workflow-step-confirmation-{index}"))
                                    .compact()
                                    .label(crate::locale::text(
                                        if confirmation {
                                            "执行前确认：开"
                                        } else {
                                            "执行前确认：关"
                                        },
                                        if confirmation {
                                            "Confirm before run: On"
                                        } else {
                                            "Confirm before run: Off"
                                        },
                                    ))
                                    .when(confirmation, Button::primary)
                                    .disabled(editor_locked)
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.toggle_workflow_step_confirmation(index, cx);
                                    })),
                            )
                            .child(
                                Button::new(format!("workflow-step-remove-{index}"))
                                    .compact()
                                    .danger()
                                    .label(crate::locale::text("删除步骤", "Remove step"))
                                    .disabled(!can_remove || editor_locked)
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.remove_workflow_step(index, cx);
                                    })),
                            ),
                    )
                    .child(Input::new(&step.title).disabled(editor_locked).w_full())
                    .child(
                        Textarea::new(&step.command)
                            .disabled(editor_locked)
                            .h(px(150.))
                            .w_full(),
                    )
            })
            .collect::<Vec<_>>();

        let workflow_list = div()
            .when(self.compact_layout, |list| {
                list.w_full().flex_1().min_h(px(0.))
            })
            .when(!self.compact_layout, |list| {
                list.w(px(260.)).flex_shrink_0()
            })
            .flex()
            .flex_col()
            .gap_2()
            .id("workflow-list")
            .overflow_y_scroll()
            .child(
                Button::new("workflow-new")
                    .compact()
                    .primary()
                    .label(crate::locale::text("新建工作流", "New workflow"))
                    .disabled(editor_locked)
                    .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                        view.new_workflow(window, cx);
                    })),
            )
            .children(entries)
            .when(self.workflows.is_empty(), |list| {
                list.child(
                    div()
                        .p_3()
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "暂无工作流，可先新建一个工作流。",
                            "No workflows yet. Create a workflow to get started.",
                        )),
                )
            });
        let workflow_editor = div()
                    .when(self.compact_layout, |editor| editor.w_full())
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .id("workflow-editor-panel")
                    .overflow_y_scroll()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when(self.compact_layout, |bar| {
                                bar.child(
                                    Button::new("workflow-back")
                                        .compact()
                                        .label(crate::locale::text("返回", "Back"))
                                        .disabled(
                                            self.workflow_saving
                                                || self.workflow_loading
                                                || self.workflow_deleting
                                                || self.workflow_run_blocks_editor()
                                                || self.pending_workflow_transition.is_some()
                                                || self.confirm_workflow_delete,
                                        )
                                        .on_click(cx.listener(
                                            |view, _: &ClickEvent, _, cx| {
                                                view.back_to_compact_list(cx);
                                            },
                                        )),
                                )
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .text_size(px(if self.compact_layout { 15. } else { 17. }))
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .child(selected_title),
                            ),
                    )
                    .child(Input::new(&self.workflow_title).disabled(editor_locked).w_full())
                    .child(
                        div()
                            .id("workflow-step-list")
                            .flex()
                            .flex_col()
                            .gap_2()
                            .children(step_editors),
                    )
                    .child(
                        Button::new("workflow-step-add")
                            .compact()
                            .label(crate::locale::text("添加步骤", "Add step"))
                            .disabled(self.workflow_steps.len() >= 40 || editor_locked)
                            .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                view.add_workflow_step(window, cx);
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "顺序按步骤卡片执行；每步保留完整多行 shell 脚本。未安装可靠 shell integration 时会拒绝执行。",
                                "Steps run in order and keep their complete multiline shell scripts. Runs are rejected when reliable shell integration is unavailable.",
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .when(self.compact_layout, |row| row.flex_wrap())
                            .gap_2()
                            .child(
                                Button::new("workflow-save")
                                    .compact()
                                    .primary()
                                    .label(crate::locale::text("保存", "Save"))
                                    .disabled(editor_locked)
                                    .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                        view.save_workflow(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("workflow-delete")
                                    .compact()
                                    .danger()
                                    .label(crate::locale::text("删除", "Delete"))
                                    .disabled(!can_delete || editor_locked)
                                    .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                        view.request_workflow_delete(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("workflow-run")
                                    .compact()
                                    .label(crate::locale::text("运行", "Run"))
                                    .disabled(!can_run)
                                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                        view.run_workflow(cx);
                                    })),
                            )
                            .when(is_waiting, |bar| {
                                bar.child(
                                    Button::new("workflow-confirm-step")
                                        .compact()
                                        .primary()
                                        .label(crate::locale::text("确认并继续", "Confirm and continue"))
                                        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                            view.confirm_workflow_step(cx);
                                        })),
                                )
                            })
                            .when(can_retry, |bar| {
                                bar.child(
                                    Button::new("workflow-retry-step")
                                        .compact()
                                        .label(crate::locale::text("显式重试失败步骤", "Retry failed step"))
                                        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                            view.retry_workflow_step(cx);
                                        })),
                                )
                            })
                            .when(is_running || is_waiting, |bar| {
                                bar.child(
                                    Button::new("workflow-cancel")
                                        .compact()
                                        .danger()
                                        .label(if is_cancelling {
                                            crate::locale::text("正在取消…", "Cancelling…")
                                        } else {
                                            crate::locale::text("取消", "Cancel")
                                        })
                                        .disabled(is_cancelling)
                                        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                            view.cancel_workflow(cx);
                                        })),
                                )
                            }),
                    )
                    .when(self.pending_workflow_transition.is_some(), |panel| {
                        panel.child(
                            div()
                                .w_full()
                                .p_3()
                                .rounded_md()
                                .bg(theme.panel)
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(crate::locale::text(
                                    "当前工作流草稿有未保存修改。选择如何处理后再切换。",
                                    "The workflow draft has unsaved changes. Choose how to proceed before switching.",
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .when(self.compact_layout, |row| row.flex_wrap())
                                        .gap_2()
                                        .child(
                                            Button::new("workflow-transition-save")
                                                .compact()
                                                .primary()
                                                .label(crate::locale::text("保存后继续", "Save and continue"))
                                                .disabled(self.workflow_saving || self.workflow_loading || self.workflow_deleting)
                                                .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                                    view.save_workflow(window, cx);
                                                })),
                                        )
                                        .child(
                                            Button::new("workflow-transition-discard")
                                                .compact()
                                                .label(crate::locale::text("丢弃并继续", "Discard and continue"))
                                                .disabled(self.workflow_saving || self.workflow_loading || self.workflow_deleting)
                                                .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                                    view.discard_workflow_transition(window, cx);
                                                })),
                                        )
                                        .child(
                                            Button::new("workflow-transition-cancel")
                                                .compact()
                                                .label(crate::locale::text("取消", "Cancel"))
                                                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                                    view.cancel_workflow_transition(cx);
                                                })),
                                        ),
                                ),
                        )
                    })
                    .when(self.confirm_workflow_delete, |panel| {
                        panel.child(
                            div()
                                .w_full()
                                .p_3()
                                .rounded_md()
                                .bg(theme.panel)
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(crate::locale::text(
                                    "删除使用当前 revision；此操作不可撤销。",
                                    "Deletion uses the current revision and cannot be undone.",
                                ))
                                .child(
                                    div()
                                        .flex()
                                        .when(self.compact_layout, |row| row.flex_wrap())
                                        .gap_2()
                                        .child(
                                            Button::new("workflow-delete-confirm")
                                                .compact()
                                                .danger()
                                                .label(crate::locale::text("确认删除", "Confirm delete"))
                                                .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                                    view.confirm_delete_workflow(window, cx);
                                                })),
                                        )
                                        .child(
                                            Button::new("workflow-delete-cancel")
                                                .compact()
                                                .label(crate::locale::text("取消", "Cancel"))
                                                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                                    view.cancel_workflow_delete(cx);
                                                })),
                                        ),
                                ),
                        )
                    })
                    .when_some(run_state, |panel, state| {
                        let current = current_step
                            .map(|step| {
                                super::super::command_message(
                                    format!(" · 当前步骤：{}", workflow_step_label(step)),
                                    format!(" · Current step: {}", workflow_step_label(step)),
                                )
                            })
                            .unwrap_or_default();
                        panel.child(
                            div()
                                .w_full()
                                .p_3()
                                .rounded_md()
                                .bg(theme.panel)
                                .text_size(px(12.))
                                .child(crate::locale::text(
                                    format!("运行状态：{}{current}", workflow_run_label(state)),
                                    format!("Run status: {}{current}", workflow_run_label(state)),
                                )),
                        )
                        .when(is_waiting, |panel| {
                            let target = approval_target.clone().unwrap_or_else(|| {
                                crate::locale::text(
                                    "目标终端身份暂不可确认",
                                    "Terminal target identity is unavailable",
                                )
                                .to_string()
                            });
                            let step = approval_step.clone().unwrap_or_else(|| {
                                crate::locale::text(
                                    "当前步骤详情不可用；请取消运行并重新加载工作流。",
                                    "Current step details are unavailable; cancel and reload the workflow.",
                                )
                                .to_string()
                            });
                            panel.child(
                                div()
                                    .w_full()
                                    .p_3()
                                    .rounded_md()
                                    .bg(theme.panel)
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(div().text_size(px(12.)).child(crate::locale::text(
                                        format!("确认目标：{target}"),
                                        format!("Confirm target: {target}"),
                                    )))
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(theme.muted)
                                            .child(step),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme.muted)
                                    .child(crate::locale::text("完整脚本", "Complete script")),
                            )
                            .child(
                                div()
                                    .id("workflow-approval-script")
                                    .w_full()
                                    .h(px(220.))
                                    .p_2()
                                    .rounded_md()
                                    .bg(theme.background)
                                    .overflow_y_scroll()
                                    .flex()
                                    .flex_col()
                                    .children(approval_script_lines),
                            )
                        })
                    })
                    .when_some(progress_panel, |panel, progress_panel| {
                        panel.child(progress_panel)
                    })
                    .when_some(unknown_review_panel, |panel, review_panel| {
                        panel.child(review_panel)
                    })
                    .children(attempts);
        if self.compact_layout {
            let detail = if self.compact_detail == Some(super::super::CommandsSection::Workflows) {
                workflow_editor.into_any_element()
            } else {
                workflow_list.into_any_element()
            };
            div()
                .id("command-workflows-view-compact")
                .size_full()
                .flex()
                .flex_col()
                .gap_3()
                .p_4()
                .overflow_hidden()
                .child(detail)
                .into_any_element()
        } else {
            div()
                .id("command-workflows-view-wide")
                .size_full()
                .flex()
                .gap_3()
                .p_4()
                .overflow_hidden()
                .child(workflow_list)
                .child(workflow_editor)
                .into_any_element()
        }
    }
}

/// 标签始终由进度携带的目标生成；pane 名只在 session ID 相同的运行快照中补充。
/// @author kongweiguang
fn workflow_progress_target_label(
    progress: &kerminal_application::commands::WorkflowRunProgress,
    session: Option<&kerminal_domain::sessions::SessionInfo>,
) -> String {
    let target = match &progress.target {
        SessionTarget::Local(_) => crate::locale::text("本地终端", "Local terminal").to_string(),
        SessionTarget::Ssh(target) => crate::locale::text(
            format!("SSH 主机 {}", target.host_id),
            format!("SSH host {}", target.host_id),
        )
        .to_string(),
        SessionTarget::Telnet(_) => {
            crate::locale::text("Telnet 终端", "Telnet terminal").to_string()
        }
        SessionTarget::Serial(_) => crate::locale::text("串口终端", "Serial terminal").to_string(),
        SessionTarget::Container(_) => {
            crate::locale::text("容器终端", "Container terminal").to_string()
        }
    };
    session.map_or(target.clone(), |session| {
        format!("{target} · {}", super::super::session_title(session))
    })
}

/// 将领域运行状态映射为当前界面的稳定文案，而不是暴露 Rust 枚举名。
fn workflow_run_label(state: WorkflowRunState) -> String {
    match state {
        WorkflowRunState::Ready => super::super::command_message("就绪", "Ready"),
        WorkflowRunState::Running => super::super::command_message("运行中", "Running"),
        WorkflowRunState::AwaitingConfirmation => {
            super::super::command_message("等待确认", "Awaiting confirmation")
        }
        WorkflowRunState::Completed => super::super::command_message("已完成", "Completed"),
        WorkflowRunState::Failed => super::super::command_message("失败", "Failed"),
        WorkflowRunState::Cancelled => super::super::command_message("已取消", "Cancelled"),
    }
}

/// 将当前步骤状态翻译成面板用语，不把内部状态名作为用户操作提示。
fn workflow_step_label(state: WorkflowStepState) -> String {
    match state {
        WorkflowStepState::Pending => super::super::command_message("等待执行", "Pending"),
        WorkflowStepState::AwaitingConfirmation => {
            super::super::command_message("等待确认", "Awaiting confirmation")
        }
        WorkflowStepState::Running => super::super::command_message("运行中", "Running"),
        WorkflowStepState::Succeeded => super::super::command_message("成功", "Succeeded"),
        WorkflowStepState::Failed => super::super::command_message("失败", "Failed"),
        WorkflowStepState::NotStarted => super::super::command_message("未开始", "Not started"),
        WorkflowStepState::Unknown => super::super::command_message("结果未知", "Unknown result"),
        WorkflowStepState::Skipped => super::super::command_message("已跳过", "Skipped"),
    }
}
