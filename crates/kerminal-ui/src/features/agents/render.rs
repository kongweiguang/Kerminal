//! Agent 右栏的紧凑会话栏、历史列表与状态提示。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, InteractiveElement as _, IntoElement, Render, Window,
    assets::IconName,
    component::Side,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
        input::Input,
        menu::{DropdownMenu as _, PopupMenuItem},
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::{
    PublicErrorCode,
    agents::{
        AgentHistoryMode, AgentResumeCapability, AgentResumeRecoveryStatus, AgentSessionView,
    },
};
use kerminal_domain::{
    agents::{AgentProvider, AgentSessionStatus},
    identity::AgentSessionId,
    sessions::SessionLifecycle,
};

use crate::theme::ThemeTokens;

use super::{
    AgentPanelEvent, AgentRenameTarget, AgentTerminalFeedback, AgentTerminalFeedbackSource,
    AgentsPanel,
};

/// 一项历史筛选按钮的稳定身份、双语文案和实时计数；theme 与 GPUI 上下文仍显式传入。
///
/// @author kongweiguang
struct HistoryFilterOption {
    id: &'static str,
    label_zh: &'static str,
    label_en: &'static str,
    description_zh: &'static str,
    description_en: &'static str,
    count: usize,
    mode: AgentHistoryMode,
}

/// 历史 terminal 的退出分类只从 typed 生命周期与回执导出，不解析错误文案。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalExitProjection {
    Normal,
    Failure,
    Unknown,
    Closed,
    Failed,
}

/// 顶栏摘要保持短小，详细错误留在右栏反馈条和 tooltip 中。
///
/// @author kongweiguang
struct TerminalHeaderStatus {
    message: String,
    tone: TerminalStatusTone,
}

/// 终端成功退出、待核对与真实故障分别使用中性、警示和错误语义色。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalStatusTone {
    Neutral,
    Warning,
    Error,
}

impl Render for AgentsPanel {
    /// Shell 材质贯穿面板；landing 自己提供会话工具行，只有运行态保留必要的 Agent 页头。
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        let terminal_attached = has_selected_agent_terminal_surface(self.selected_view());

        let mut panel = div()
            .id("agents-panel")
            .w_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.materials.shell)
            .text_color(theme.foreground);
        if terminal_attached {
            panel = panel.child(self.render_header(theme, terminal_attached, cx));
        }

        if let Some(view) = self.selected_view()
            && let Some(recovery) = &view.resume_recovery
        {
            panel = panel.child(self.render_recovery_banner(
                view.session.agent_session_id.clone(),
                recovery.status,
                theme,
                cx,
            ));
        }

        if !terminal_attached {
            panel = panel
                .h_full()
                .min_h(px(0.))
                .child(self.render_landing_content(theme, cx));
        }

        if self.context_preview.is_some() {
            panel = panel.child(self.render_context_preview(theme, cx));
        }
        if let Some(feedback) = self.visible_terminal_feedback() {
            panel = panel.child(render_terminal_feedback(theme, feedback.message.clone()));
        }
        if let Some(status) = self
            .status
            .clone()
            .filter(|status| !terminal_attached || !is_terminal_success_notice(status))
        {
            let cancel_button =
                self.operation_cancellation.as_ref().map(|cancellation| {
                    let cancel_requested = cancellation.is_cancelled();
                    let (label_zh, label_en, tooltip_zh, tooltip_en) = if cancel_requested {
                        (
                            "正在取消…",
                            "Cancelling…",
                            "正在等待当前操作回执。",
                            "Waiting for the current operation result.",
                        )
                    } else {
                        (
                            "取消操作",
                            "Cancel operation",
                            "请求取消；界面会等待实际结果。",
                            "Request cancellation; the panel will wait for the actual result.",
                        )
                    };
                    Button::new("agents-cancel-operation")
                        .compact()
                        .secondary()
                        .label(crate::locale::text(label_zh, label_en))
                        .tooltip(crate::locale::text(tooltip_zh, tooltip_en))
                        .accessibility_label(crate::locale::text(label_zh, label_en))
                        .disabled(cancel_requested)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.cancel_agent_operation(cx)
                        }))
                });
            let launcher_settings_button = self.launcher_setup_prompt.then(|| {
                self.render_launcher_settings_button(
                    "agents-failure-launcher-settings",
                    "检查设置",
                    "Review settings",
                    cx,
                )
            });
            panel = panel.child(render_status(
                theme,
                status,
                cancel_button,
                launcher_settings_button,
            ));
        }
        panel
    }
}

impl AgentsPanel {
    /// 将 Pending/Unknown 的下一步直接呈现；未核对前只保留原事务，不误导用户归档。
    ///
    /// @author kongweiguang
    fn render_recovery_banner(
        &self,
        id: AgentSessionId,
        status: AgentResumeRecoveryStatus,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (title_zh, title_en) = match status {
            AgentResumeRecoveryStatus::Pending => ("恢复待核对", "Recovery needs readback"),
            AgentResumeRecoveryStatus::Unknown => ("恢复结果未知", "Recovery result unknown"),
        };
        div()
            .id("agents-recovery-banner")
            .flex_shrink_0()
            .mx_2()
            .mt_1()
            .p_2()
            .rounded_md()
            .bg(theme.secondary)
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.warning)
                    .child(crate::locale::text(title_zh, title_en)),
            )
            .child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        "核对仅处理原恢复事务，不启动新的 CLI。待核对期间不能改名、重绑或归档；可保留记录并先执行“核对恢复”。",
                        "Check only the original recovery without starting another CLI. Rename, rebinding, and archiving stay locked until verified; keep the record and reconcile first.",
                    )),
            )
            .child(
                Button::new("agents-reconcile-recovery")
                    .compact()
                    .secondary()
                    .label(crate::locale::text("核对恢复", "Reconcile recovery"))
                    .disabled(self.busy || self.loading || !self.sessions_verified)
                    .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                        if panel.ensure_menu_selection(&id, cx) && panel.can_reconcile_selected() {
                            panel.reconcile_agent(id.clone(), cx);
                        }
                    })),
            )
    }

    /// 只将仍选中的 Agent 运行态错误写入该面板，丢弃已切换或已归档会话的迟到结果。
    ///
    /// @author kongweiguang
    pub fn report_terminal_failure(
        &mut self,
        agent_session_id: &AgentSessionId,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if self.selected_agent.as_ref() != Some(agent_session_id) {
            return;
        }
        self.record_terminal_feedback(AgentTerminalFeedback {
            agent_session_id: agent_session_id.clone(),
            message,
            source: AgentTerminalFeedbackSource::RuntimeFailure,
        });
        cx.notify();
    }

    /// Root 将终端操作的 typed 回执与业务状态分离，且不以迟到操作拒绝覆盖真实 runtime 故障。
    ///
    /// @author kongweiguang
    pub fn report_terminal_operation_failure(
        &mut self,
        agent_session_id: &AgentSessionId,
        code: PublicErrorCode,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if self.selected_agent.as_ref() != Some(agent_session_id) {
            return;
        }
        let requires_exit_readback =
            AgentTerminalFeedbackSource::OperationFailure(code).clears_after_confirmed_exit();
        self.record_terminal_feedback(AgentTerminalFeedback {
            agent_session_id: agent_session_id.clone(),
            message,
            source: AgentTerminalFeedbackSource::OperationFailure(code),
        });
        if requires_exit_readback {
            self.reload(cx);
        }
        cx.notify();
    }

    /// 入口与历史共享有界居中列；landing 不重复绘制运行态页头，刷新归到“对话”标题行。
    /// @author kongweiguang
    fn render_landing_content(
        &self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id("agents-landing-scroll")
            .w_full()
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .child(
                div()
                    .id("agents-landing-scroll-content")
                    .w_full()
                    .h_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(div().w_full().flex_1().min_h(px(0.)))
                    .child(
                        div()
                            .id("agents-landing-column")
                            .w_full()
                            .max_w(px(320.))
                            .flex_shrink_0()
                            .px_3()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(self.render_launcher(theme, cx))
                            .child(self.render_session_history(theme, cx)),
                    )
                    .child(div().w_full().flex_1().min_h(px(0.))),
            )
    }

    /// 只有已有会话需要运行态页头和快速切换器；无终端时入口与刷新由正文统一承载。
    /// 会话标题与终端状态分行，避免窄右栏压缩到两者都不可读。
    ///
    /// @author kongweiguang
    fn render_header(
        &mut self,
        theme: ThemeTokens,
        terminal_attached: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let show_new_chat = self.selected_agent.is_some();
        let show_actions_menu = should_render_actions_menu(self.selected_view());
        let new_chat_disabled = self.busy || self.launcher_selection_busy;
        let continue_agent_id = self
            .selected_view()
            .filter(|view| should_render_continue(Some(view)))
            .map(|view| view.session.agent_session_id.clone());
        let terminal_status = terminal_header_status_row(
            self.selected_view(),
            terminal_attached,
            self.visible_terminal_feedback(),
            continue_agent_id.is_some(),
        );
        let status_is_separate = terminal_status.is_some();
        let mut header = div()
            .id("agents-header")
            .w_full()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .bg(theme.materials.chrome)
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .id("agents-header-toolbar")
                    .w_full()
                    .h(px(40.))
                    .flex_shrink_0()
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .pr_1()
                            .text_color(theme.foreground)
                            .child(IconName::Bot)
                            .child(
                                div()
                                    .text_size(theme.text_sm)
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .child("Agent"),
                            ),
                    )
                    .when(terminal_attached, |toolbar| {
                        toolbar.child(self.render_session_picker(status_is_separate, cx))
                    })
                    .when(show_new_chat, |header| {
                        header.child(
                            Button::new("agents-new-chat")
                                .compact()
                                .ghost()
                                .icon(IconName::Plus)
                                .accessibility_label(crate::locale::text("新建对话", "New chat"))
                                .tooltip(crate::locale::text("新建对话", "New chat"))
                                .disabled(new_chat_disabled)
                                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.show_new_chat(cx)
                                })),
                        )
                    })
                    .when(terminal_attached, |toolbar| {
                        toolbar.child(
                            Button::new("agents-refresh")
                                .compact()
                                .ghost()
                                .icon(IconName::RefreshCw)
                                .accessibility_label(crate::locale::text(
                                    "刷新会话",
                                    "Refresh sessions",
                                ))
                                .tooltip(crate::locale::text("刷新会话", "Refresh sessions"))
                                .disabled(
                                    self.loading
                                        || self.busy
                                        || self.launcher_catalog_loading
                                        || self.launcher_selection_busy,
                                )
                                .on_click(
                                    cx.listener(|panel, _: &ClickEvent, _, cx| {
                                        panel.reload_all(cx)
                                    }),
                                ),
                        )
                    })
                    .when(show_actions_menu, |header| {
                        header.child(self.render_actions_menu(cx))
                    }),
            );

        if let Some(status) = terminal_status {
            let tooltip_status = status.message.clone();
            let tone = status.tone;
            let icon = match tone {
                TerminalStatusTone::Neutral => IconName::CircleCheck,
                TerminalStatusTone::Warning | TerminalStatusTone::Error => IconName::CircleAlert,
            };
            let color = match tone {
                TerminalStatusTone::Neutral => theme.muted,
                TerminalStatusTone::Warning => theme.warning,
                TerminalStatusTone::Error => theme.error,
            };
            header = header.child(
                div()
                    .id("agents-terminal-status-row")
                    .w_full()
                    .min_h(px(28.))
                    .flex_shrink_0()
                    .px_2()
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("agents-terminal-status")
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_size(theme.text_xs)
                            .text_color(color)
                            .child(icon)
                            .child(div().flex_1().min_w(px(0.)).child(status.message))
                            .tooltip(move |window, cx| {
                                Tooltip::new(tooltip_status.clone()).build(window, cx)
                            }),
                    )
                    .when_some(continue_agent_id, |row, id| {
                        row.child(
                            Button::new("agents-continue-session")
                                .compact()
                                .secondary()
                                .icon(IconName::Play)
                                .label(crate::locale::text("继续", "Continue"))
                                .accessibility_label(crate::locale::text(
                                    "继续此 Agent 会话",
                                    "Continue this Agent chat",
                                ))
                                .tooltip(crate::locale::text(
                                    "继续此 Agent 会话",
                                    "Continue this Agent chat",
                                ))
                                .disabled(self.busy)
                                .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                                    panel.resume_agent(id.clone(), cx);
                                })),
                        )
                    }),
            );
        }
        header
    }

    /// 标题和状态分行时 selector 只显示会话标题，未分行的恢复状态仍与历史兼容标签同行。
    /// @author kongweiguang
    fn render_session_picker(
        &self,
        status_is_separate: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let current_label = self.selected_view().map_or_else(
            || {
                if self.loading {
                    crate::locale::text("读取会话…", "Loading sessions…").to_string()
                } else if !self.sessions_verified {
                    crate::locale::text("会话列表不可用", "Session list unavailable").to_string()
                } else {
                    crate::locale::text("选择会话", "Select a chat").to_string()
                }
            },
            |view| session_picker_label(view, status_is_separate),
        );
        let sessions = self
            .sessions
            .iter()
            .map(|view| {
                (
                    view.session.agent_session_id.clone(),
                    compact_title(session_picker_title(view)),
                    lifecycle_label(view),
                    self.selected_agent.as_ref() == Some(&view.session.agent_session_id),
                )
            })
            .collect::<Vec<_>>();
        let entity = cx.entity().clone();
        let disabled = self.loading || !self.sessions_verified;

        let picker = Button::new("agents-session-picker")
            .compact()
            .ghost()
            .w_full()
            .label(current_label)
            .icon(IconName::ChevronDown)
            .accessibility_label(crate::locale::text(
                "选择 Agent 会话",
                "Select an Agent chat",
            ))
            .disabled(disabled)
            .dropdown_menu(move |mut menu, _, _| {
                if sessions.is_empty() {
                    menu = menu.item(
                        PopupMenuItem::new(crate::locale::text(
                            "还没有历史会话",
                            "No chat history",
                        ))
                        .disabled(true),
                    );
                } else {
                    for (id, title, lifecycle, selected) in &sessions {
                        let id = id.clone();
                        let label = format!("{title} · {lifecycle}");
                        let entity = entity.clone();
                        menu = menu.item(PopupMenuItem::new(label).checked(*selected).on_click(
                            move |_, _, app| {
                                let id = id.clone();
                                entity.update(app, |panel, cx| panel.select_agent(id, cx));
                            },
                        ));
                    }
                }
                menu
            });
        div().flex_1().min_w(px(0.)).child(picker)
    }

    /// 次级操作使用明确动词和状态门禁；刷新统一放在视图标题行，避免出现重复入口。
    /// @author kongweiguang
    fn render_actions_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected_view();
        let has_selected = selected.is_some();
        let can_bind = selected.is_some_and(|view| {
            view.session.status != AgentSessionStatus::Archived
                && view.resume_recovery.is_none()
                && self.candidate.is_some()
        });
        let can_preview =
            selected.is_some_and(|view| view.session.status != AgentSessionStatus::Archived);
        let can_resume = selected.is_some_and(agent_can_resume);
        let can_reconcile = selected.is_some_and(agent_can_reconcile);
        let can_archive = selected.is_some_and(agent_can_archive);
        let disabled = self.busy || self.launcher_selection_busy || self.launcher_catalog_loading;
        let target_label = self.candidate.as_ref().map(|target| target.label.clone());
        let selected_id = selected.map(|view| view.session.agent_session_id.clone());
        let target_binding = self.candidate.as_ref().map(|target| target.binding.clone());
        let entity = cx.entity().clone();

        Button::new("agents-actions")
            .compact()
            .ghost()
            .icon(IconName::Ellipsis)
            .accessibility_label(crate::locale::text("更多 Agent 操作", "More Agent actions"))
            .tooltip(crate::locale::text("更多操作", "More actions"))
            .dropdown_menu(move |mut menu, _, _| {
                let bind_entity = entity.clone();
                let bind_agent_id = selected_id.clone();
                let bind_target = target_binding.clone();
                menu = menu.item(
                    PopupMenuItem::new(crate::locale::text(
                        "绑定当前终端",
                        "Bind current terminal",
                    ))
                    .icon(IconName::Link2)
                    .disabled(disabled || !can_bind)
                    .on_click(move |_, _, app| {
                        let (Some(agent_id), Some(target)) =
                            (bind_agent_id.clone(), bind_target.clone())
                        else {
                            return;
                        };
                        bind_entity.update(app, |panel, cx| {
                            if panel.ensure_menu_selection(&agent_id, cx)
                                && panel.ensure_menu_target(&target, cx)
                            {
                                panel.bind_candidate(cx);
                            }
                        });
                    }),
                );

                let preview_entity = entity.clone();
                let preview_agent_id = selected_id.clone();
                menu = menu.item(
                    PopupMenuItem::new(crate::locale::text(
                        "预览绑定目标上下文",
                        "Preview bound context",
                    ))
                    .icon(IconName::Eye)
                    .disabled(disabled || !can_preview)
                    .on_click(move |_, _, app| {
                        let Some(agent_id) = preview_agent_id.clone() else {
                            return;
                        };
                        preview_entity.update(app, |panel, cx| {
                            if panel.ensure_menu_selection(&agent_id, cx) {
                                panel.preview_context(cx);
                            }
                        });
                    }),
                );

                let reconcile_entity = entity.clone();
                let reconcile_agent_id = selected_id.clone();
                menu = menu.item(
                    PopupMenuItem::new(crate::locale::text("核对恢复", "Reconcile recovery"))
                        .disabled(disabled || !can_reconcile)
                        .on_click(move |_, _, app| {
                            let Some(agent_id) = reconcile_agent_id.clone() else {
                                return;
                            };
                            reconcile_entity.update(app, |panel, cx| {
                                if panel.ensure_menu_selection(&agent_id, cx)
                                    && panel.can_reconcile_selected()
                                {
                                    panel.reconcile_agent(agent_id, cx);
                                }
                            });
                        }),
                );

                if can_resume {
                    let resume_entity = entity.clone();
                    let resume_agent_id = selected_id.clone();
                    menu = menu.item(
                        PopupMenuItem::new(crate::locale::text("继续会话", "Resume chat"))
                            .icon(IconName::Play)
                            .disabled(disabled)
                            .on_click(move |_, _, app| {
                                let Some(agent_id) = resume_agent_id.clone() else {
                                    return;
                                };
                                resume_entity.update(app, |panel, cx| {
                                    if panel.ensure_menu_selection(&agent_id, cx) {
                                        if panel.can_resume_selected() {
                                            panel.resume_selected(cx);
                                        } else {
                                            panel.show_menu_notice(
                                                "启动器未确认支持继续，或 Agent 仍在运行/恢复结果待核对。",
                                                "Resume is not confirmed by the launcher, or the Agent is active or recovery is unresolved.",
                                                cx,
                                            );
                                        }
                                    }
                                });
                            }),
                    );
                }

                let archive_entity = entity.clone();
                let archive_agent_id = selected_id.clone();
                menu = menu.item(
                    PopupMenuItem::new(crate::locale::text("归档会话", "Archive chat"))
                        .icon(IconName::Archive)
                        .disabled(disabled || !can_archive)
                        .on_click(move |_, _, app| {
                            let Some(agent_id) = archive_agent_id.clone() else {
                                return;
                            };
                            archive_entity.update(app, |panel, cx| {
                                if panel.ensure_menu_selection(&agent_id, cx) {
                                    panel.archive_selected(cx);
                                }
                            });
                        }),
                );
                if can_reconcile {
                    menu = menu.item(PopupMenuItem::label(crate::locale::text(
                        "待核对记录不能归档；先核对恢复。",
                        "Unresolved recovery cannot be archived; reconcile it first.",
                    )));
                }

                if let Some(target) = target_label.as_ref() {
                    menu = menu.item(PopupMenuItem::label(crate::locale::text(
                        format!("新建目标候选：{target}"),
                        format!("New chat target candidate: {target}"),
                    )));
                } else if !has_selected {
                    menu = menu.item(PopupMenuItem::label(crate::locale::text(
                        "新建会话默认不绑定终端",
                        "New chats are unbound by default",
                    )));
                }
                menu
            })
    }

    /// 菜单可能在异步刷新期间保持打开；执行前复核原会话身份和当前列表/操作状态。
    /// @author kongweiguang
    fn ensure_menu_selection(
        &mut self,
        expected_id: &kerminal_domain::identity::AgentSessionId,
        cx: &mut Context<Self>,
    ) -> bool {
        let failure = if self.selected_agent.as_ref() != Some(expected_id) {
            Some((
                "所选 Agent 已变化，请重新打开操作菜单。",
                "The selected Agent changed. Reopen the actions menu.",
            ))
        } else if self.busy {
            Some((
                "另一个 Agent 操作正在进行。",
                "Another Agent action is in progress.",
            ))
        } else if self.loading {
            Some((
                "Agent 列表正在刷新，请稍后重试。",
                "The Agent list is refreshing.",
            ))
        } else if !self.sessions_verified {
            Some((
                "Agent 列表不可用，请刷新后重试。",
                "The Agent list is unavailable. Refresh and try again.",
            ))
        } else {
            match self.selected_view() {
                Some(view) if view.session.status != AgentSessionStatus::Archived => None,
                Some(_) => Some((
                    "归档会话不能执行此操作。",
                    "Archived chats cannot use this action.",
                )),
                None => Some((
                    "所选 Agent 已不存在，请刷新列表。",
                    "The selected Agent no longer exists.",
                )),
            }
        };
        if let Some((zh, en)) = failure {
            self.show_menu_notice(zh, en, cx);
            false
        } else {
            true
        }
    }

    /// 目标候选只用于新建或显式绑定；菜单过期后绝不把绑定套到后来切入的终端。
    /// @author kongweiguang
    fn ensure_menu_target(
        &mut self,
        expected: &kerminal_domain::agents::AgentTargetBinding,
        cx: &mut Context<Self>,
    ) -> bool {
        if self
            .candidate
            .as_ref()
            .is_some_and(|candidate| &candidate.binding == expected)
        {
            return true;
        }
        self.show_menu_notice(
            "当前终端候选已变化，请重新打开操作菜单。",
            "The terminal candidate changed. Reopen the actions menu.",
            cx,
        );
        false
    }

    /// 只有终端确已结束且没有待核对恢复记录时，菜单才能继续该会话。
    /// @author kongweiguang
    fn can_resume_selected(&self) -> bool {
        self.selected_view().is_some_and(agent_can_resume)
    }

    /// 待核对状态只开放原事务核对动作，避免把“继续”按钮变成第二次 CLI 启动。
    ///
    /// @author kongweiguang
    fn can_reconcile_selected(&self) -> bool {
        self.selected_view().is_some_and(agent_can_reconcile)
    }

    /// 过期菜单操作不静默失效也不自动选新目标，把具体原因留在面板状态区。
    /// @author kongweiguang
    pub(super) fn show_menu_notice(&mut self, zh: &str, en: &str, cx: &mut Context<Self>) {
        self.status = Some(crate::locale::text(zh, en).to_string());
        cx.notify();
    }

    /// 320px 栏内保留清楚的启动顺序；标题与目标详情分别提供可发现入口，标题编辑仍可用。
    /// @author kongweiguang
    fn render_launcher(&self, theme: ThemeTokens, cx: &mut Context<Self>) -> impl IntoElement {
        let launcher_catalog_empty = self
            .launcher_catalog
            .as_ref()
            .is_some_and(|catalog| catalog.options.is_empty());
        div()
            .id("agents-launcher")
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_size(theme.text_body)
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .text_color(theme.foreground)
                    .child(crate::locale::text("新建对话", "New chat"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new("agents-launcher-details")
                                    .compact()
                                    .ghost()
                                    .icon(IconName::Info)
                                    .accessibility_label(crate::locale::text(
                                        "启动器与目标详情",
                                        "Launcher and target details",
                                    ))
                                    .tooltip(self.launcher_technical_detail()),
                            )
                            .child(
                                Button::new("agents-title-editor-toggle")
                                    .compact()
                                    .ghost()
                                    .icon(if self.title_editor_visible {
                                        IconName::X
                                    } else {
                                        IconName::Pencil
                                    })
                                    .accessibility_label(crate::locale::text(
                                        if self.title_editor_visible {
                                            "隐藏标题"
                                        } else {
                                            "设置标题"
                                        },
                                        if self.title_editor_visible {
                                            "Hide title"
                                        } else {
                                            "Set title"
                                        },
                                    ))
                                    .tooltip(crate::locale::text(
                                        if self.title_editor_visible {
                                            "隐藏标题"
                                        } else {
                                            "设置标题"
                                        },
                                        if self.title_editor_visible {
                                            "Hide title"
                                        } else {
                                            "Set title"
                                        },
                                    ))
                                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                        panel.toggle_title_editor(cx)
                                    })),
                            ),
                    ),
            )
            .child(self.render_new_chat_target(theme))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(self.render_launcher_selector(theme, cx)),
                    )
                    .child(
                        Button::new("agents-launcher-start")
                            .compact()
                            .flex_none()
                            .h(px(44.))
                            .text_size(theme.text_body)
                            .primary()
                            .icon(IconName::ArrowRight)
                            .label(crate::locale::text("进入", "Open"))
                            .disabled(!self.can_create_with_selected_launcher())
                            .on_click(
                                cx.listener(|panel, _: &ClickEvent, _, cx| panel.create_agent(cx)),
                            ),
                    ),
            )
            .when(self.title_editor_visible, |element| {
                element.child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.subtle)
                                .child(crate::locale::text(
                                    "对话标题（可选）",
                                    "Chat title (optional)",
                                )),
                        )
                        .child(
                            Input::new(&self.title)
                                .w_full()
                                .h(px(32.))
                                .text_size(theme.text_sm),
                        ),
                )
            })
            .when_some(self.launcher_load_error.as_ref(), |element, error| {
                let retry_enabled =
                    !self.busy && !self.launcher_selection_busy && !self.launcher_catalog_loading;
                element.child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.error)
                                .child(crate::locale::text(
                                    format!("无法读取 Launcher 配置：{error}"),
                                    format!("Could not read launcher settings: {error}"),
                                )),
                        )
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .justify_end()
                                .gap_1()
                                .child(
                                    Button::new("agents-launcher-catalog-retry")
                                        .compact()
                                        .ghost()
                                        .label(crate::locale::text("重试", "Retry"))
                                        .disabled(!retry_enabled)
                                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                            panel.reload_launcher_catalog(cx)
                                        })),
                                )
                                .child(self.render_launcher_settings_button(
                                    "agents-launcher-load-error-settings",
                                    "设置",
                                    "Settings",
                                    cx,
                                )),
                        ),
                )
            })
            .when(launcher_catalog_empty, |element| {
                element.child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_1()
                        .child(
                            div()
                                .flex_1()
                                .text_size(theme.text_xs)
                                .text_color(theme.warning)
                                .child(crate::locale::text(
                                    "没有可用的 Agent 启动器。",
                                    "No Agent launchers are available.",
                                )),
                        )
                        .child(self.render_launcher_settings_button(
                            "agents-launcher-empty-settings",
                            "设置",
                            "Settings",
                            cx,
                        )),
                )
            })
            .when_some(self.launcher_selection_error.as_ref(), |element, error| {
                let retry_enabled = !self.busy && !self.launcher_selection_busy;
                element.child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_1()
                        .child(
                            div()
                                .flex_1()
                                .text_size(theme.text_xs)
                                .text_color(theme.warning)
                                .child(crate::locale::text(
                                    format!("选择未保存：{error}"),
                                    format!("Selection was not saved: {error}"),
                                )),
                        )
                        .child(
                            Button::new("agents-launcher-retry")
                                .compact()
                                .ghost()
                                .label(crate::locale::text("重试", "Retry"))
                                .disabled(!retry_enabled)
                                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.retry_launcher_selection(cx)
                                })),
                        ),
                )
            })
    }

    /// 将设置入口交给 Root 处理；面板不直接修改偏好，也不猜测具体启动失败原因。
    ///
    /// @author kongweiguang
    fn render_launcher_settings_button(
        &self,
        id: &'static str,
        label_zh: &'static str,
        label_en: &'static str,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id)
            .compact()
            .ghost()
            .label(crate::locale::text(label_zh, label_en))
            .accessibility_label(crate::locale::text(label_zh, label_en))
            .tooltip(crate::locale::text(
                "若错误涉及启动命令或参数，可搜索“Agent 启动器”检查配置；不会自动切换或重试。",
                "If the error involves the launch command or arguments, review Agent Launcher settings. No automatic switch or retry is performed.",
            ))
            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                cx.emit(AgentPanelEvent::OpenAgentLauncherSettings);
            }))
    }

    /// 选择器沿用目录身份；标准菜单项提供 UIA 名称，简短来源标签控制常见选项的菜单宽度。
    ///
    /// @author kongweiguang
    fn render_launcher_selector(
        &self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected_key = self.launcher_key.clone();
        let options = self
            .launcher_catalog
            .as_ref()
            .map(|catalog| {
                catalog
                    .options
                    .iter()
                    .map(|option| (option.key.clone(), option.label.clone(), option.provider))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let selected_provider = selected_key.as_ref().and_then(|key| {
            options
                .iter()
                .find(|(option_key, _, _)| option_key == key)
                .map(|(_, _, provider)| *provider)
        });
        let selected_icon = selected_provider.map_or(IconName::Bot, launcher_icon);
        let selected_descriptor = selected_provider.map(launcher_status_descriptor);
        let label = selected_key
            .as_ref()
            .and_then(|key| {
                options
                    .iter()
                    .find(|(option_key, _, _)| option_key == key)
                    .map(|(_, label, _)| label.clone())
            })
            .unwrap_or_else(|| {
                if self.launcher_catalog_loading {
                    crate::locale::text("正在读取 Agent 选项…", "Loading Agent options…")
                        .to_string()
                } else if self.launcher_load_error.is_some() {
                    crate::locale::text("Agent 选项不可用", "Agent options unavailable").to_string()
                } else if self
                    .launcher_catalog
                    .as_ref()
                    .is_some_and(|catalog| catalog.options.is_empty())
                {
                    crate::locale::text("未配置 Agent", "No Agent configured").to_string()
                } else {
                    crate::locale::text("选择 Agent", "Select an Agent").to_string()
                }
            });
        let accessibility_detail = selected_descriptor
            .as_deref()
            .map_or_else(String::new, |detail| format!(" · {detail}"));
        let accessibility_label = crate::locale::text(
            format!("选择 Agent · {label}{accessibility_detail}"),
            format!("Select an Agent · {label}{accessibility_detail}"),
        );
        let entity = cx.entity().clone();
        let disabled = self.busy || self.launcher_selection_busy || self.launcher_catalog_loading;

        Button::new("agents-launcher-selector")
            .compact()
            .secondary()
            .outline()
            .w_full()
            .min_w(px(0.))
            .h(px(44.))
            .dropdown_caret(true)
            .accessibility_label(accessibility_label)
            .tooltip(label.clone())
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .size(px(28.))
                            .rounded_md()
                            .bg(theme.secondary)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(selected_icon),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .justify_center()
                            .gap_0()
                            .child(
                                div()
                                    .text_size(theme.text_sm)
                                    .text_color(theme.foreground)
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(label),
                            )
                            .when_some(selected_descriptor, |element, detail| {
                                element.child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .text_size(theme.text_xs)
                                        .text_color(theme.muted)
                                        .child(launcher_configured_dot(theme))
                                        .child(detail),
                                )
                            }),
                    ),
            )
            .disabled(disabled || options.is_empty())
            .dropdown_menu(move |mut menu, _, _| {
                menu = menu.check_side(Side::Right);
                for (key, label, provider) in &options {
                    let selected = selected_key.as_ref() == Some(key);
                    let key = key.clone();
                    let label = label.clone();
                    let provider = *provider;
                    let entity = entity.clone();
                    let detail = launcher_menu_descriptor(provider);
                    let icon = launcher_icon(provider);
                    menu = menu.item(
                        PopupMenuItem::new(format!("{label} · {detail}"))
                            .icon(icon)
                            .checked(selected)
                            .on_click(move |_, _, app| {
                                let key = key.clone();
                                entity.update(app, |panel, cx| panel.select_launcher(key, cx));
                            }),
                    );
                }
                let manage_entity = entity.clone();
                menu.item(
                    PopupMenuItem::new(crate::locale::text(
                        "管理自定义 Agent…",
                        "Manage custom Agents…",
                    ))
                    .on_click(move |_, _, app| {
                        manage_entity.update(app, |_panel, cx| {
                            cx.emit(AgentPanelEvent::OpenCustomAgentManager);
                        });
                    }),
                )
            })
    }

    /// 主摘要复用当前真实目标名；技术引用只放在可访问的 Info 提示里，避免常驻显示内部 ID。
    /// @author kongweiguang
    fn render_new_chat_target(&self, theme: ThemeTokens) -> impl IntoElement {
        let label = self.candidate.as_ref().map_or_else(
            || crate::locale::text("当前目标 · 未绑定终端", "Current target · Unbound").to_string(),
            |candidate| {
                crate::locale::text(
                    format!("当前目标 · {}", candidate.label),
                    format!("Current target · {}", candidate.label),
                )
                .to_string()
            },
        );
        div()
            .w_full()
            .flex()
            .items_center()
            .gap_1()
            .text_size(theme.text_xs)
            .text_color(theme.muted)
            .child(IconName::Link2)
            .child(label)
    }

    /// 技术详情按需显示当前启动器的真实默认命令与目标类型；目录未提供的自定义命令不臆测。
    /// @author kongweiguang
    fn launcher_technical_detail(&self) -> String {
        let launcher = self
            .launcher_catalog
            .as_ref()
            .and_then(|catalog| {
                self.launcher_key
                    .as_ref()
                    .and_then(|key| catalog.options.iter().find(|option| &option.key == key))
            })
            .map(|option| {
                format!(
                    "{} · {}",
                    option.label,
                    launcher_command_detail(option.provider)
                )
            })
            .unwrap_or_else(|| {
                crate::locale::text("未选择启动器", "No launcher selected").to_string()
            });
        let target = self.candidate.as_ref().map_or_else(
            || crate::locale::text("未绑定终端", "Unbound").to_string(),
            |candidate| {
                let target_kind = candidate
                    .binding
                    .target_kind
                    .as_deref()
                    .map(target_kind_label)
                    .unwrap_or_else(|| crate::locale::text("未知类型", "Unknown type").to_string());
                let target_ref = candidate
                    .binding
                    .target_ref
                    .as_deref()
                    .map(|reference| {
                        format!(" · {} {reference}", crate::locale::text("目标引用", "ref"))
                    })
                    .unwrap_or_default();
                format!("{} · {target_kind}{target_ref}", candidate.label)
            },
        );
        crate::locale::text(
            format!("启动器：{launcher}\n目标：{target}"),
            format!("Launcher: {launcher}\nTarget: {target}"),
        )
        .to_string()
    }

    /// 只有目录中的当前 key 才能启动，避免保存失败或配置外部变更后回退到另一 Agent。
    /// @author kongweiguang
    fn can_create_with_selected_launcher(&self) -> bool {
        !self.busy
            && !self.launcher_selection_busy
            && !self.launcher_catalog_loading
            && self.launcher_catalog.as_ref().is_some_and(|catalog| {
                self.launcher_key
                    .as_ref()
                    .is_some_and(|key| catalog.options.iter().any(|option| &option.key == key))
            })
    }

    /// 两段主筛选保留准确计数与完整 AX 描述，归档由标题行的低强调入口承载。
    /// @author kongweiguang
    fn render_history_filter(
        &self,
        option: HistoryFilterOption,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let HistoryFilterOption {
            id,
            label_zh,
            label_en,
            description_zh,
            description_en,
            count,
            mode,
        } = option;
        let selected = self.history_mode == mode;
        let foreground = if selected {
            theme.foreground
        } else {
            theme.muted
        };
        let label = crate::locale::text(label_zh, label_en).to_string();
        let description = crate::locale::text(description_zh, description_en).to_string();
        let accessibility_label = format!("{description} · {count}");

        Button::new(id)
            .compact()
            .ghost()
            .flex_1()
            .min_w(px(0.))
            .h(px(30.))
            .accessibility_label(accessibility_label)
            .child(
                div()
                    .w_full()
                    .h_full()
                    .min_w(px(0.))
                    .px_1()
                    .rounded_sm()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_1()
                    .when(selected, |element| element.bg(theme.hover))
                    .child(
                        div()
                            .min_w(px(0.))
                            .text_size(theme.text_xs)
                            .text_color(foreground)
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(theme.text_xs)
                            .text_color(foreground)
                            .child(count.to_string()),
                    ),
            )
            .on_click(
                cx.listener(move |panel, _: &ClickEvent, _, cx| panel.set_history_mode(mode, cx)),
            )
    }

    /// Landing 中把刷新、归档收进对话标题行，主筛选只保留“当前/全部”两段。
    /// @author kongweiguang
    fn render_session_history(
        &self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut history = div()
            .id("agents-session-history")
            .w_full()
            .px_2()
            .pt_1()
            .pb_2()
            .flex()
            .flex_col()
            .gap_1();
        let title = if self.history_mode == AgentHistoryMode::Archived {
            crate::locale::text("已归档", "Archived")
        } else {
            crate::locale::text("对话", "Chats")
        };
        let refresh_disabled = self.loading
            || self.busy
            || self.launcher_catalog_loading
            || self.launcher_selection_busy;
        let archive_selected = self.history_mode == AgentHistoryMode::Archived;
        let archive_accessibility_label = if archive_selected {
            crate::locale::text("返回当前目标对话", "Back to current target chats")
        } else {
            crate::locale::text(
                format!("查看归档对话 · {}", self.archived_count),
                format!("Show archived chats · {}", self.archived_count),
            )
        };
        let archive_button = if archive_selected {
            Button::new("agents-history-archive")
                .compact()
                .secondary()
                .icon(IconName::Archive)
        } else {
            Button::new("agents-history-archive")
                .compact()
                .ghost()
                .icon(IconName::Archive)
        };
        history = history.child(
            div()
                .id("agents-history-heading")
                .w_full()
                .h(px(32.))
                .px_1()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_size(theme.text_sm)
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .text_color(theme.foreground)
                        .child(title)
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .font_weight(gpui_kit::FontWeight::NORMAL)
                                .text_color(theme.muted)
                                .child(self.visible_session_ids.len().to_string()),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            Button::new("agents-refresh")
                                .compact()
                                .ghost()
                                .icon(IconName::RefreshCw)
                                .accessibility_label(crate::locale::text(
                                    "刷新会话",
                                    "Refresh chats",
                                ))
                                .tooltip(crate::locale::text("刷新会话", "Refresh chats"))
                                .disabled(refresh_disabled)
                                .on_click(
                                    cx.listener(|panel, _: &ClickEvent, _, cx| {
                                        panel.reload_all(cx)
                                    }),
                                ),
                        )
                        .child(
                            archive_button
                                .accessibility_label(archive_accessibility_label.clone())
                                .tooltip(crate::locale::text(
                                    if archive_selected {
                                        "返回当前目标对话"
                                    } else {
                                        "归档对话"
                                    },
                                    if archive_selected {
                                        "Back to current target chats"
                                    } else {
                                        "Archived chats"
                                    },
                                ))
                                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    let mode = if panel.history_mode == AgentHistoryMode::Archived {
                                        AgentHistoryMode::CurrentTarget
                                    } else {
                                        AgentHistoryMode::Archived
                                    };
                                    panel.set_history_mode(mode, cx);
                                })),
                        ),
                ),
        );

        let mut filters = div()
            .id("agents-history-filter")
            .w_full()
            .flex()
            .items_center()
            .gap_1()
            .p_1()
            .rounded_md()
            .bg(theme.secondary)
            .border_1()
            .border_color(theme.border);
        for (id, label_zh, label_en, description_zh, description_en, count, mode) in [
            (
                "agents-history-current",
                "当前",
                "Current",
                "当前目标会话",
                "Sessions for the current target",
                self.current_target_count,
                AgentHistoryMode::CurrentTarget,
            ),
            (
                "agents-history-all",
                "全部",
                "All",
                "全部未归档会话",
                "All non-archived sessions",
                self.all_count,
                AgentHistoryMode::All,
            ),
        ] {
            filters = filters.child(self.render_history_filter(
                HistoryFilterOption {
                    id,
                    label_zh,
                    label_en,
                    description_zh,
                    description_en,
                    count,
                    mode,
                },
                theme,
                cx,
            ));
        }
        history = history.child(filters);
        if self.visible_session_ids.is_empty() {
            return history.child(self.render_empty_state(theme));
        }
        if self.loading || !self.sessions_verified {
            history = history.child(
                div()
                    .p_2()
                    .rounded_md()
                    .bg(theme.secondary)
                    .text_size(theme.text_xs)
                    .text_color(theme.warning)
                    .child(if self.loading {
                        crate::locale::text(
                            "正在刷新，暂时保留已确认记录。",
                            "Refreshing; confirmed records remain visible.",
                        )
                    } else {
                        crate::locale::text(
                            "列表读取失败，显示的是上次已确认结果。",
                            "The refresh failed; these are the last confirmed sessions.",
                        )
                    }),
            );
        }
        for view in &self.sessions {
            if self
                .visible_session_ids
                .contains(&view.session.agent_session_id)
            {
                history = history.child(self.render_session_row(
                    view,
                    self.selected_agent.as_ref() == Some(&view.session.agent_session_id),
                    self.sessions_verified && !self.loading,
                    cx,
                ));
            }
        }
        history
    }

    /// 每行绑定自己的 AgentSessionId；待核对时保留核对入口并禁用会冲突的归档写入。
    /// @author kongweiguang
    fn render_session_row(
        &self,
        view: &AgentSessionView,
        active: bool,
        selectable: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = view.session.agent_session_id.clone();
        let title = view.session.title.clone();
        let label = format!("{} · {}", compact_title(&title), lifecycle_label(view));
        let can_rename = view.resume_recovery.is_none();
        if self
            .rename_target
            .as_ref()
            .is_some_and(|(rename_id, _)| rename_id == &id)
        {
            return div()
                .id(format!("agent-session-rename-{}", id.as_str()))
                .w_full()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    Input::new(&self.rename_title)
                        .flex_1()
                        .min_w(px(0.))
                        .h(px(30.)),
                )
                .child(
                    Button::new(format!("agent-session-save-{}", id.as_str()))
                        .compact()
                        .primary()
                        .label(crate::locale::text("保存", "Save"))
                        .disabled(self.busy || !selectable || !can_rename)
                        .on_click(
                            cx.listener(|panel, _: &ClickEvent, _, cx| panel.save_agent_title(cx)),
                        ),
                )
                .child(
                    Button::new(format!("agent-session-cancel-{}", id.as_str()))
                        .compact()
                        .ghost()
                        .icon(IconName::X)
                        .accessibility_label(crate::locale::text("取消改名", "Cancel rename"))
                        .disabled(self.busy)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.cancel_agent_rename(cx)
                        })),
                );
        }

        let mode = self.history_mode;
        let scope = self.scope.clone();
        let candidate = self.candidate.as_ref().map(|value| value.binding.clone());
        let is_archived = view.session.status == AgentSessionStatus::Archived;
        let can_archive = agent_can_archive(view);
        let can_reconcile = agent_can_reconcile(view);
        let can_create_from_history = view.resume_recovery.is_none();
        let can_resume = agent_can_resume(view);
        let entity = cx.entity().clone();
        let select_id = id.clone();
        let primary = Button::new(format!("agent-session-{}", id.as_str()))
            .compact()
            .ghost()
            .flex_1()
            .min_w(px(0.))
            .label(label)
            .disabled(!selectable)
            .when(active, Button::primary)
            .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                panel.select_agent(select_id.clone(), cx)
            }));

        let menu_entity = entity.clone();
        let action_id = id.clone();
        let action_title = title.clone();
        let action_scope = scope.clone();
        let action_candidate = candidate.clone();
        let action_menu = Button::new(format!("agent-session-actions-{}", id.as_str()))
            .compact()
            .ghost()
            .icon(IconName::Ellipsis)
            .accessibility_label(crate::locale::text(
                format!("{} 的会话操作", title),
                format!("Actions for {}", title),
            ))
            .disabled(!selectable || self.busy)
            .dropdown_menu(move |mut menu, _, _| {
                let resume_entity = menu_entity.clone();
                let resume_id = action_id.clone();
                let resume_title = action_title.clone();
                let resume_scope = action_scope.clone();
                let resume_candidate = action_candidate.clone();
                let reconcile_entity = menu_entity.clone();
                let reconcile_id = action_id.clone();
                let reconcile_title = action_title.clone();
                let reconcile_scope = action_scope.clone();
                let reconcile_candidate = action_candidate.clone();
                menu = menu.item(
                    PopupMenuItem::new(crate::locale::text("核对恢复", "Reconcile recovery"))
                        .disabled(!can_reconcile)
                        .on_click(move |_, _, app| {
                            reconcile_entity.update(app, |panel, cx| {
                                if panel.ensure_history_action(
                                    &reconcile_id,
                                    &reconcile_title,
                                    mode,
                                    &reconcile_scope,
                                    reconcile_candidate.as_ref(),
                                    cx,
                                ) {
                                    panel.reconcile_agent(reconcile_id.clone(), cx);
                                }
                            });
                        }),
                );
                if can_resume {
                    menu = menu.item(
                        PopupMenuItem::new(crate::locale::text("继续会话", "Continue chat"))
                            .icon(IconName::Play)
                            .on_click(move |_, _, app| {
                                resume_entity.update(app, |panel, cx| {
                                    if panel.ensure_history_action(
                                        &resume_id,
                                        &resume_title,
                                        mode,
                                        &resume_scope,
                                        resume_candidate.as_ref(),
                                        cx,
                                    ) {
                                        panel.resume_agent(resume_id.clone(), cx);
                                    }
                                });
                            }),
                    );
                }

                let new_entity = menu_entity.clone();
                let new_id = action_id.clone();
                let new_title = action_title.clone();
                let new_scope = action_scope.clone();
                let new_candidate = action_candidate.clone();
                menu = menu.item(
                    PopupMenuItem::new(crate::locale::text(
                        "使用此启动配置新建",
                        "New chat from these launch settings",
                    ))
                    .icon(IconName::Plus)
                    .disabled(!can_create_from_history)
                    .on_click(move |_, _, app| {
                        new_entity.update(app, |panel, cx| {
                            panel.create_from_history(
                                new_id.clone(),
                                new_title.clone(),
                                mode,
                                new_scope.clone(),
                                new_candidate.clone(),
                                cx,
                            );
                        });
                    }),
                );

                let rename_entity = menu_entity.clone();
                let rename_id = action_id.clone();
                let rename_title = action_title.clone();
                let rename_scope = action_scope.clone();
                let rename_candidate = action_candidate.clone();
                menu = menu.item(
                    PopupMenuItem::new(crate::locale::text("重命名", "Rename"))
                        .icon(IconName::Pencil)
                        .disabled(!can_rename)
                        .on_click(move |_, window, app| {
                            rename_entity.update(app, |panel, cx| {
                                panel.begin_rename_agent(
                                    AgentRenameTarget {
                                        id: rename_id.clone(),
                                        expected_title: rename_title.clone(),
                                    },
                                    mode,
                                    rename_scope.clone(),
                                    rename_candidate.clone(),
                                    window,
                                    cx,
                                );
                            });
                        }),
                );

                if !is_archived {
                    let archive_entity = menu_entity.clone();
                    let archive_id = action_id.clone();
                    let archive_title = action_title.clone();
                    let archive_scope = action_scope.clone();
                    let archive_candidate = action_candidate.clone();
                    menu = menu.item(
                        PopupMenuItem::new(crate::locale::text("归档", "Archive"))
                            .icon(IconName::Archive)
                            .disabled(!can_archive)
                            .on_click(move |_, _, app| {
                                archive_entity.update(app, |panel, cx| {
                                    if panel.ensure_history_action(
                                        &archive_id,
                                        &archive_title,
                                        mode,
                                        &archive_scope,
                                        archive_candidate.as_ref(),
                                        cx,
                                    ) {
                                        panel.archive_agent(archive_id.clone(), cx);
                                    }
                                });
                            }),
                    );
                }
                if can_reconcile {
                    menu = menu.item(PopupMenuItem::label(crate::locale::text(
                        "待核对记录不能归档；先核对恢复。",
                        "Unresolved recovery cannot be archived; reconcile it first.",
                    )));
                }
                menu
            });

        div()
            .id(format!("agent-history-row-{}", id.as_str()))
            .w_full()
            .flex()
            .items_center()
            .gap_1()
            .child(primary)
            .child(action_menu)
    }

    /// 空态收进有边界的居中卡片，错误仍保留刷新恢复提示，降低列表区域的视觉噪声。
    /// @author kongweiguang
    fn render_empty_state(&self, theme: ThemeTokens) -> impl IntoElement {
        let (title, body) = if self.loading {
            (
                crate::locale::text("正在读取会话", "Loading chats"),
                crate::locale::text(
                    "读取完成后会显示最近会话。",
                    "Recent chats appear after loading.",
                ),
            )
        } else if !self.sessions_verified {
            (
                crate::locale::text("暂时无法读取会话", "Chats are unavailable"),
                crate::locale::text(
                    "刷新后重试；不会用空列表覆盖上次状态。",
                    "Refresh to retry; the last known state is preserved.",
                ),
            )
        } else if self.history_mode == AgentHistoryMode::Archived {
            (
                crate::locale::text("暂无归档会话", "No archived chats"),
                crate::locale::text(
                    "归档的会话会保留在这里。",
                    "Archived chats remain available here.",
                ),
            )
        } else if self.history_mode == AgentHistoryMode::CurrentTarget {
            (
                crate::locale::text("当前目标暂无会话", "No chats for this target"),
                crate::locale::text(
                    "可切换到全部历史查看其他会话。",
                    "Switch to All to view other chats.",
                ),
            )
        } else {
            (
                crate::locale::text("暂无历史会话", "No chat history"),
                crate::locale::text("新建后的会话会显示在这里。", "New chats will appear here."),
            )
        };
        div()
            .id("agents-empty-state")
            .w_full()
            .min_h(px(96.))
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_1()
            .child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(if self.loading || !self.sessions_verified {
                        theme.warning
                    } else {
                        theme.muted
                    })
                    .child(title),
            )
            .child(
                div()
                    .max_w(px(260.))
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .text_center()
                    .child(body),
            )
    }

    /// 预览只展示 application 返回的受限文本并限制可视高度；固定区不随 Launcher 视口收缩。
    /// @author kongweiguang
    fn render_context_preview(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let content = self.context_preview.clone().unwrap_or_default();
        div()
            .id("agents-context-preview")
            .w_full()
            .max_h(px(96.))
            .flex_shrink_0()
            .mx_2()
            .mb_1()
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .rounded_md()
            .overflow_hidden()
            .bg(theme.secondary)
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        "目标上下文预览",
                        "Bound target context",
                    ))
                    .child(
                        Button::new("agents-context-preview-close")
                            .compact()
                            .ghost()
                            .icon(IconName::X)
                            .accessibility_label(crate::locale::text("关闭预览", "Close preview"))
                            .tooltip(crate::locale::text("关闭预览", "Close preview"))
                            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                panel.context_preview = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("agents-context-preview-text")
                    .max_h(px(64.))
                    .overflow_y_scroll()
                    .text_size(theme.text_xs)
                    .text_color(theme.foreground)
                    .child(content),
            )
    }
}

/// 将图标身份与安全的短命令提示统一映射，菜单和当前选择不会出现相互矛盾的说明。
/// @author kongweiguang
fn launcher_icon(provider: AgentProvider) -> IconName {
    match provider {
        AgentProvider::Codex => IconName::Terminal,
        AgentProvider::Claude => IconName::Sparkles,
        AgentProvider::Pi => IconName::Pi,
        AgentProvider::Custom => IconName::Wrench,
    }
}

/// 明示内置来源或已保存状态，不将目录成员误写成 CLI 可运行；自定义命令内容留在管理器。
/// @author kongweiguang
fn launcher_status_descriptor(provider: AgentProvider) -> String {
    let descriptor = match provider {
        AgentProvider::Codex => {
            crate::locale::text("内置启动器 · codex", "Built-in launcher · codex")
        }
        AgentProvider::Claude => {
            crate::locale::text("内置启动器 · claude", "Built-in launcher · claude")
        }
        AgentProvider::Pi => crate::locale::text(
            "内置启动器 · pi --approve",
            "Built-in launcher · pi --approve",
        ),
        AgentProvider::Custom => {
            crate::locale::text("已保存 · 自定义命令", "Saved · Custom command")
        }
    };
    descriptor.to_string()
}

/// 菜单把短来源说明与完整名称合并，保留可访问名称并减少来源文案的横向占用。
///
/// @author kongweiguang
fn launcher_menu_descriptor(provider: AgentProvider) -> String {
    match provider {
        AgentProvider::Codex => crate::locale::text("内置 · codex", "Built-in · codex"),
        AgentProvider::Claude => crate::locale::text("内置 · claude", "Built-in · claude"),
        AgentProvider::Pi => crate::locale::text("内置 · pi --approve", "Built-in · pi --approve"),
        AgentProvider::Custom => {
            crate::locale::text("已保存 · 自定义命令", "Saved · custom command")
        }
    }
    .to_string()
}

/// 技术提示里的默认命令与应用层首次启动 argv 保持一致；可运行状态仍不由静态目录推断。
/// @author kongweiguang
fn launcher_command_detail(provider: AgentProvider) -> String {
    match provider {
        AgentProvider::Codex => {
            "codex --dangerously-bypass-approvals-and-sandbox --dangerously-bypass-hook-trust"
                .to_owned()
        }
        AgentProvider::Claude => "claude --dangerously-skip-permissions".to_owned(),
        AgentProvider::Pi => "pi --approve".to_owned(),
        AgentProvider::Custom => crate::locale::text(
            "自定义命令（在管理器中查看）",
            "Custom command (see manager)",
        )
        .to_string(),
    }
}

/// 使用中性点配合“内置/已保存”文字，避免绿色状态暗示已探测 CLI 可运行。
/// @author kongweiguang
fn launcher_configured_dot(theme: ThemeTokens) -> impl IntoElement {
    div().size(px(6.)).rounded_full().bg(theme.muted)
}

/// 把绑定类型转换成界面术语；未知的新类型保留原值，避免掩盖新增目标。
/// @author kongweiguang
fn target_kind_label(kind: &str) -> String {
    match kind {
        "local" => crate::locale::text("本机", "Local").to_string(),
        "ssh" => "SSH".to_owned(),
        "telnet" => "Telnet".to_owned(),
        "serial" => crate::locale::text("串口", "Serial").to_string(),
        "container" => crate::locale::text("容器", "Container").to_string(),
        other => other.to_owned(),
    }
}

/// 只在有业务回执时展开有限高度；长错误允许换行并提供完整提示，不能藏掉核对后再试的指引。
/// @author kongweiguang
fn render_status(
    theme: ThemeTokens,
    status: String,
    cancel_button: Option<Button>,
    launcher_settings_button: Option<Button>,
) -> impl IntoElement {
    let tooltip_status = status.clone();
    let mut row = div()
        .id("agents-status")
        .w_full()
        .max_h(px(96.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .py_1()
        .overflow_y_scroll()
        .bg(theme.materials.chrome)
        .border_t_1()
        .border_color(theme.border);
    if let Some(button) = cancel_button {
        row = row.child(button);
    }
    if let Some(button) = launcher_settings_button {
        row = row.child(button);
    }
    row.child(
        div()
            .id("agents-status-message")
            .flex_1()
            .min_w_0()
            .text_size(theme.text_xs)
            .text_color(theme.muted)
            .child(status)
            .tooltip(move |window, cx| Tooltip::new(tooltip_status.clone()).build(window, cx)),
    )
}

/// 故障与业务回执分别保留；有限换行和完整提示兼顾终端空间与失败恢复指引的可读性。
///
/// @author kongweiguang
fn render_terminal_feedback(theme: ThemeTokens, message: String) -> impl IntoElement {
    let tooltip_message = message.clone();
    div()
        .id("agents-terminal-feedback")
        .w_full()
        .max_h(px(96.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .py_1()
        .overflow_y_scroll()
        .bg(theme.materials.chrome)
        .border_t_1()
        .border_color(theme.border)
        .child(IconName::CircleAlert)
        .child(
            div()
                .id("agents-terminal-feedback-message")
                .flex_1()
                .min_w_0()
                .text_size(theme.text_xs)
                .text_color(theme.error)
                .child(message)
                .tooltip(move |window, cx| Tooltip::new(tooltip_message.clone()).build(window, cx)),
        )
}

/// Agent 启动确认已由终端画布证明，避免在右栏顶栏重复显示并挤压会话名称。
/// @author kongweiguang
fn is_terminal_success_notice(status: &str) -> bool {
    matches!(
        status,
        "Agent CLI 已启动。"
            | "Agent CLI started."
            | "Agent 已完成启动，已打开新会话。"
            | "Agent finished starting and opened a new chat."
            | "Agent 已继续运行。"
            | "Agent resumed."
    )
}

/// 保留活跃 Agent 的终端投影，包括 Exited 快照；归档与待核对恢复状态回到入口。
/// @author kongweiguang
fn has_agent_terminal_surface(view: &AgentSessionView) -> bool {
    view.session.status != AgentSessionStatus::Archived
        && view.resume_recovery.is_none()
        && view.terminal.is_some()
}

/// 没有选中的会话时绘制 Launcher；切换新建模式只隐藏原终端投影，不改变它的生命周期。
///
/// @author kongweiguang
fn has_selected_agent_terminal_surface(selected: Option<&AgentSessionView>) -> bool {
    selected.is_some_and(has_agent_terminal_surface)
}

/// 只对列表中仍可核验的会话显示上下文操作；Pending/Unknown 仍需通过菜单执行原事务核对。
///
/// @author kongweiguang
fn should_render_actions_menu(selected: Option<&AgentSessionView>) -> bool {
    selected.is_some()
}

/// 状态文案同时出现在会话下拉和历史行，统一判定避免两处界面给出不同生命周期。
/// @author kongweiguang
fn lifecycle_label(view: &AgentSessionView) -> String {
    if let Some(recovery) = &view.resume_recovery {
        let (zh, en) = match recovery.status {
            AgentResumeRecoveryStatus::Pending => ("待核对", "Needs readback"),
            AgentResumeRecoveryStatus::Unknown => ("结果未知", "Unknown result"),
        };
        return crate::locale::text(zh, en).to_string();
    }
    if view.session.status == AgentSessionStatus::Archived {
        return crate::locale::text("已归档", "Archived").to_string();
    }
    if has_running_agent_terminal(view) {
        return crate::locale::text("运行中", "Running").to_string();
    }
    if let Some(terminal) = view.terminal.as_ref() {
        let label = match terminal_exit_projection(view) {
            Some(TerminalExitProjection::Normal) => Some(("已正常退出", "Exited normally")),
            Some(TerminalExitProjection::Failure) => Some(("异常退出", "Exited with an error")),
            Some(TerminalExitProjection::Unknown) => {
                Some(("退出结果待确认", "Exit result unconfirmed"))
            }
            Some(TerminalExitProjection::Closed) => Some(("已关闭", "Closed")),
            Some(TerminalExitProjection::Failed) => Some(("运行失败", "Failed")),
            None => match terminal.lifecycle {
                SessionLifecycle::Starting => Some(("启动中", "Starting")),
                SessionLifecycle::Closing => Some(("正在关闭", "Closing")),
                SessionLifecycle::Closed => Some(("已关闭", "Closed")),
                SessionLifecycle::Failed => Some(("运行失败", "Failed")),
                SessionLifecycle::Running | SessionLifecycle::Exited => None,
            },
        };
        if let Some((zh, en)) = label {
            return crate::locale::text(zh, en).to_string();
        }
    }
    let (zh, en) = match view.resume_capability {
        AgentResumeCapability::Supported => ("可继续", "Ready to resume"),
        AgentResumeCapability::Unsupported => ("已结束", "Ended"),
        AgentResumeCapability::MetadataUnavailable => ("恢复信息待检查", "Check recovery metadata"),
    };
    crate::locale::text(zh, en).to_string()
}

/// ExitStatus 成功位不能替代明确零码；缺码/矛盾回执保持待确认，真实非零或 signal 显示失败。
///
/// @author kongweiguang
fn terminal_exit_projection(view: &AgentSessionView) -> Option<TerminalExitProjection> {
    let terminal = view.terminal.as_ref()?;
    match terminal.lifecycle {
        SessionLifecycle::Exited => match terminal.exit_status.as_ref() {
            Some(status) if status.success && status.code == Some(0) && status.signal.is_none() => {
                Some(TerminalExitProjection::Normal)
            }
            Some(status)
                if !status.success
                    || status.code.is_some_and(|code| code != 0)
                    || status.signal.is_some() =>
            {
                Some(TerminalExitProjection::Failure)
            }
            _ => Some(TerminalExitProjection::Unknown),
        },
        SessionLifecycle::Closed => Some(TerminalExitProjection::Closed),
        SessionLifecycle::Failed => Some(TerminalExitProjection::Failed),
        SessionLifecycle::Starting | SessionLifecycle::Running | SessionLifecycle::Closing => None,
    }
}

/// 将 typed 退出分类映射为短状态；非零细节保留退出码或 signal，不隐藏真实失败。
///
/// @author kongweiguang
fn terminal_exit_message(view: &AgentSessionView) -> Option<(String, TerminalStatusTone)> {
    let projection = terminal_exit_projection(view)?;
    let terminal = view.terminal.as_ref()?;
    let message = match projection {
        TerminalExitProjection::Normal => {
            crate::locale::text("已正常退出", "Exited normally").to_string()
        }
        TerminalExitProjection::Failure | TerminalExitProjection::Failed => {
            let detail = terminal.exit_status.as_ref().and_then(|status| {
                status
                    .code
                    .map(|code| {
                        crate::locale::text(format!("退出码 {code}"), format!("exit code {code}"))
                            .to_string()
                    })
                    .or_else(|| {
                        status.signal.as_ref().map(|signal| {
                            crate::locale::text(
                                format!("信号 {signal}"),
                                format!("signal {signal}"),
                            )
                            .to_string()
                        })
                    })
            });
            detail.map_or_else(
                || crate::locale::text("终端运行失败", "Terminal failed").to_string(),
                |detail| {
                    crate::locale::text(
                        format!("终端异常退出（{detail}）"),
                        format!("Terminal exited unsuccessfully ({detail})"),
                    )
                    .to_string()
                },
            )
        }
        TerminalExitProjection::Unknown => {
            crate::locale::text("已结束，退出结果待确认", "Exited; result unconfirmed").to_string()
        }
        TerminalExitProjection::Closed => {
            crate::locale::text("终端已关闭", "Terminal closed").to_string()
        }
    };
    let tone = match projection {
        TerminalExitProjection::Normal | TerminalExitProjection::Closed => {
            TerminalStatusTone::Neutral
        }
        TerminalExitProjection::Unknown => TerminalStatusTone::Warning,
        TerminalExitProjection::Failure | TerminalExitProjection::Failed => {
            TerminalStatusTone::Error
        }
    };
    Some((message, tone))
}

/// 真实 runtime 错误优先于派生退出提示；只有无 runtime 反馈时才呈现生命周期摘要。
///
/// @author kongweiguang
fn terminal_header_status(
    selected_view: Option<&AgentSessionView>,
    feedback: Option<&AgentTerminalFeedback>,
) -> Option<TerminalHeaderStatus> {
    if let Some(feedback) = feedback {
        return Some(TerminalHeaderStatus {
            message: feedback.message.clone(),
            tone: TerminalStatusTone::Error,
        });
    }
    let (message, tone) = terminal_exit_message(selected_view?)?;
    Some(TerminalHeaderStatus { message, tone })
}

/// 有活动终端或可继续会话时保留独立状态行；状态来自既有投影，不触发恢复或重新启动。
///
/// @author kongweiguang
fn terminal_header_status_row(
    selected_view: Option<&AgentSessionView>,
    terminal_attached: bool,
    feedback: Option<&AgentTerminalFeedback>,
    continue_visible: bool,
) -> Option<TerminalHeaderStatus> {
    let status = terminal_header_status(
        if terminal_attached {
            selected_view
        } else {
            None
        },
        feedback,
    );
    status.or_else(|| {
        if terminal_attached || continue_visible {
            selected_view.map(|view| TerminalHeaderStatus {
                message: lifecycle_label(view),
                tone: TerminalStatusTone::Neutral,
            })
        } else {
            None
        }
    })
}

/// Continue 只在现有恢复门禁开放时出现；Pending/Unknown 仍只能核对原事务。
///
/// @author kongweiguang
fn should_render_continue(view: Option<&AgentSessionView>) -> bool {
    view.is_some_and(agent_can_resume)
}

/// 只把实际处于 Running 的 CLI 当作活动终端，Exited 的历史句柄不能占据运行布局。
/// @author kongweiguang
fn has_running_agent_terminal(view: &AgentSessionView) -> bool {
    view.session.status == AgentSessionStatus::Active
        && view.resume_recovery.is_none()
        && view
            .terminal
            .as_ref()
            .is_some_and(|terminal| terminal.lifecycle == SessionLifecycle::Running)
}

/// 允许复用已退出的终端记录继续会话，但阻止 Starting、Running 或 Closing 时重复启动 CLI。
/// @author kongweiguang
fn terminal_can_be_resumed(terminal: Option<&kerminal_domain::sessions::SessionInfo>) -> bool {
    terminal.is_none_or(|terminal| {
        matches!(
            terminal.lifecycle,
            SessionLifecycle::Exited | SessionLifecycle::Closed | SessionLifecycle::Failed
        )
    })
}

/// 只有显式支持恢复且旧 PTY 已退出、无未决核对时才开放继续；不猜 Custom provider 能力。
///
/// @author kongweiguang
pub(super) fn agent_can_resume(view: &AgentSessionView) -> bool {
    view.resume_capability == AgentResumeCapability::Supported
        && view.session.status != AgentSessionStatus::Archived
        && view.resume_recovery.is_none()
        && terminal_can_be_resumed(view.terminal.as_ref())
}

/// 核对只针对可见的原恢复事务，与 terminal 是否仍运行无关，便于处理停止后的 Unknown。
///
/// @author kongweiguang
pub(super) fn agent_can_reconcile(view: &AgentSessionView) -> bool {
    view.session.status != AgentSessionStatus::Archived && view.resume_recovery.is_some()
}
/// 归档会改变 Agent 生命周期并与未完成恢复事务冲突，因此只允许归档已核对记录。
///
/// @author kongweiguang
pub(super) fn agent_can_archive(view: &AgentSessionView) -> bool {
    view.session.status != AgentSessionStatus::Archived && view.resume_recovery.is_none()
}

/// 下拉框只缩短视觉标签、不修改持久 title，避免历史记录占满右栏宽度。
/// @author kongweiguang
fn compact_title(title: &str) -> String {
    const MAX_CHARS: usize = 18;
    let mut chars = title.chars();
    let compact = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{compact}…")
    } else if compact.is_empty() {
        crate::locale::text("未命名会话", "Untitled chat").to_string()
    } else {
        compact
    }
}

/// Custom 未命名会话用保存的启动器标签替代通用 provider 标题，同时保留用户填写的会话标题。
///
/// @author kongweiguang
fn session_picker_title(view: &AgentSessionView) -> &str {
    let title = view.session.title.as_str();
    let launcher_label = view.session.launch.command_label.trim();
    if view.session.agent_id == AgentProvider::Custom
        && title == "Custom Agent"
        && !launcher_label.is_empty()
    {
        launcher_label
    } else {
        title
    }
}

/// 状态已单列时保留完整可读的会话名，避免将标题和退出回执竞争同一窄行。
///
/// @author kongweiguang
fn session_picker_label(view: &AgentSessionView, status_is_separate: bool) -> String {
    let title = compact_title(session_picker_title(view));
    if status_is_separate {
        title
    } else {
        format!("{title} · {}", lifecycle_label(view))
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/agent_recovery_projection.rs"]
mod recovery_projection_tests;
