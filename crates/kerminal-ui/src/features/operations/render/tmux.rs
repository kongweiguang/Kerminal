//! tmux 专属绘制与交互入口；独立模块避免与其它运维工具争用公共渲染。
//!
//! @author kongweiguang

use std::collections::HashSet;

use super::super::TmuxDetailState;
use super::super::quickref::{commands as tmux_commands, shortcuts as tmux_shortcuts};
use super::{
    OperationsView, SectionPhase, bounded_text, error_row, localized_text, section_feedback,
    section_page, tmux_attach_guidance,
};
use crate::theme::ThemeTokens;
use gpui_kit::{
    AnyElement, ClickEvent, Context, IntoElement, SharedString,
    assets::IconName,
    component::{
        ActiveTheme, Disableable,
        button::{Button, ButtonCustomVariant, ButtonVariants},
        input::Input,
    },
    div,
    prelude::*,
    px, relative,
};
use kerminal_application::operations::TmuxCapability;
use kerminal_domain::operations::{TmuxSession, TmuxWindow};

/// 固定右栏只保留可见列表本身的折叠状态，避免详情读取状态与展示状态互相牵连。
///
/// @author kongweiguang
#[derive(Debug, Default, Eq, PartialEq)]
pub(in crate::features::operations) struct TmuxPanelPresentation {
    pub(super) expanded_session_ids: HashSet<String>,
    pub(super) quickref_open: bool,
}

impl TmuxPanelPresentation {
    /// 同一面板可独立展开多个会话，稳定 session ID 使列表刷新和重排不改变用户选择。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn toggle_session_details(
        &mut self,
        session_id: &str,
    ) -> bool {
        if self.expanded_session_ids.remove(session_id) {
            false
        } else {
            self.expanded_session_ids.insert(session_id.to_owned());
            true
        }
    }

    /// 速查表是固定栏独立的渐进披露区，不复用完整工作台的折叠状态。
    ///
    /// @author kongweiguang
    pub(super) fn toggle_quickref(&mut self) {
        self.quickref_open = !self.quickref_open;
    }
}

impl OperationsView {
    /// Root 统一拥有遮罩、焦点与关闭语义；此方法只从既有 tmux 意图投影紧凑内容，
    /// 让失败继续留在同一输入草稿中，并让结束确认使用根层冻结目标的提交门禁。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn render_tmux_dialog(
        &self,
        theme: ThemeTokens,
        available_width: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.section_selection.is_fixed() {
            return self.render_tmux_panel_dialog(theme, available_width, cx);
        }

        if let Some(session_id) = self.tmux_pending_kill.as_deref() {
            let session_name = self
                .tmux
                .data()
                .and_then(|snapshot| {
                    snapshot
                        .sessions
                        .iter()
                        .find(|session| session.id == session_id)
                })
                .map(|session| {
                    if session.name.is_empty() {
                        session.id.clone()
                    } else {
                        session.name.clone()
                    }
                })
                .unwrap_or_else(|| session_id.to_owned());
            return Some(
                tmux_dialog_frame(
                    "tmux-end-session-dialog",
                    TmuxDialogCopy {
                        title: crate::locale::text("结束 tmux 会话", "End tmux session"),
                        message: crate::locale::text(
                            format!(
                                "会话：{session_name}\n\n结束后会断开所有客户端并停止会话内正在运行的任务。此操作无法撤销。"
                            ),
                            format!(
                                "Session: {session_name}\n\nEnding it disconnects all clients and stops tasks running in the session. This cannot be undone."
                            ),
                        ),
                    },
                    self.status.clone(),
                    true,
                    theme,
                    available_width,
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .when(available_width < 340., |row| row.flex_col().items_stretch())
                        .when(available_width >= 340., |row| row.items_center().justify_end())
                        .gap_2()
                        .child(tmux_dialog_refresh_button(self.busy, cx))
                        .child(tmux_dialog_cancel_button(self, cx))
                        .child(
                            Button::new("tmux-end-session-confirm")
                                .compact()
                                .danger()
                                .label(crate::locale::text("结束会话", "End session"))
                                .disabled(!self.tmux_dialog_submission_enabled())
                                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                    view.confirm_root_confirmation(cx)
                                })),
                        ),
                )
                .into_any_element(),
            );
        }

        if let Some(session_id) = self.tmux_rename_target.as_deref() {
            let Some(inputs) = self.inputs.as_ref() else {
                return Some(
                    tmux_dialog_frame(
                        "tmux-rename-dialog",
                        TmuxDialogCopy {
                            title: crate::locale::text("重命名 tmux 会话", "Rename tmux session"),
                            message: crate::locale::text(
                                "输入状态尚未初始化。关闭对话框后重新打开即可恢复。",
                                "The input state is unavailable. Close this dialog and reopen it to retry.",
                            ),
                        },
                        None,
                        false,
                        theme,
                        available_width,
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(tmux_dialog_refresh_button(self.busy, cx))
                            .child(tmux_dialog_cancel_button(self, cx)),
                    )
                    .into_any_element(),
                );
            };
            let current_name = self
                .tmux
                .data()
                .and_then(|snapshot| {
                    snapshot
                        .sessions
                        .iter()
                        .find(|session| session.id == session_id)
                })
                .map(|session| {
                    if session.name.is_empty() {
                        session.id.clone()
                    } else {
                        session.name.clone()
                    }
                })
                .unwrap_or_else(|| session_id.to_owned());
            let feedback = self.status.clone();
            let content = div()
                .w_full()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .text_size(theme.text_sm)
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            format!("当前名称：{current_name}。重命名不会关闭会话。"),
                            format!(
                                "Current name: {current_name}. Renaming does not close the session."
                            ),
                        )),
                )
                .child(Input::new(&inputs.tmux_rename).w_full())
                .when_some(feedback, |content, message| {
                    content.child(tmux_dialog_feedback(message, theme))
                })
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .when(available_width < 340., |row| row.flex_col().items_stretch())
                        .when(available_width >= 340., |row| {
                            row.items_center().justify_end()
                        })
                        .gap_2()
                        .child(tmux_dialog_refresh_button(self.busy, cx))
                        .child(tmux_dialog_cancel_button(self, cx))
                        .child(
                            Button::new("tmux-rename-confirm")
                                .compact()
                                .primary()
                                .icon(IconName::Check)
                                .label(crate::locale::text("保存名称", "Save name"))
                                .disabled(!self.tmux_dialog_submission_enabled())
                                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                    view.rename_tmux_session(cx)
                                })),
                        ),
                );
            return Some(
                tmux_dialog_card(
                    "tmux-rename-dialog",
                    crate::locale::text("重命名 tmux 会话", "Rename tmux session"),
                    content,
                    theme,
                )
                .into_any_element(),
            );
        }

        if self.tmux_create_form_open {
            let Some(inputs) = self.inputs.as_ref() else {
                return Some(
                    tmux_dialog_frame(
                        "tmux-create-dialog",
                        TmuxDialogCopy {
                            title: crate::locale::text("新建 tmux 会话", "Create tmux session"),
                            message: crate::locale::text(
                                "输入状态尚未初始化。关闭对话框后重新打开即可恢复。",
                                "The input state is unavailable. Close this dialog and reopen it to retry.",
                            ),
                        },
                        None,
                        false,
                        theme,
                        available_width,
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(tmux_dialog_refresh_button(self.busy, cx))
                            .child(tmux_dialog_cancel_button(self, cx)),
                    )
                    .into_any_element(),
                );
            };
            let create_enabled = super::super::actions::tmux_session_creation_enabled(
                self.busy,
                self.tmux_target_available(),
                self.tmux.data().map(|snapshot| &snapshot.capability),
            );
            let feedback = self.status.clone();
            let content = div()
                .w_full()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .text_size(theme.text_sm)
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "会话将在当前目标上创建；名称和工作目录均可选。",
                            "The session will be created on the current target. Name and working directory are optional.",
                        )),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(tmux_dialog_field_label(
                            crate::locale::text("名称", "Name"),
                            theme,
                        ))
                        .child(Input::new(&inputs.tmux_name).w_full()),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(tmux_dialog_field_label(
                            crate::locale::text("工作目录（可选）", "Working directory (optional)"),
                            theme,
                        ))
                        .child(Input::new(&inputs.tmux_working_directory).w_full()),
                )
                .when_some(feedback, |content, message| {
                    content.child(tmux_dialog_feedback(message, theme))
                })
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .when(available_width < 340., |row| row.flex_col().items_stretch())
                        .when(available_width >= 340., |row| row.items_center().justify_end())
                        .gap_2()
                        .child(tmux_dialog_refresh_button(self.busy, cx))
                        .child(tmux_dialog_cancel_button(self, cx))
                        .child(
                            Button::new("tmux-create-confirm")
                                .compact()
                                .primary()
                                .icon(IconName::Plus)
                                .label(crate::locale::text("创建会话", "Create session"))
                                .disabled(
                                    !self.tmux_dialog_submission_enabled() || !create_enabled,
                                )
                                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                    view.create_tmux_session(cx)
                                })),
                        ),
                );
            return Some(
                tmux_dialog_card(
                    "tmux-create-dialog",
                    crate::locale::text("新建 tmux 会话", "Create tmux session"),
                    content,
                    theme,
                )
                .into_any_element(),
            );
        }

        None
    }

    /// 固定右栏弹框复刻 main 的名称输入与会话结束确认，避免把完整页的工作目录选项带入窄栏。
    ///
    /// @author kongweiguang
    fn render_tmux_panel_dialog(
        &self,
        theme: ThemeTokens,
        available_width: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if let Some(session_id) = self.tmux_pending_kill.as_deref() {
            let session_name = self
                .tmux_dialog_session
                .as_ref()
                .filter(|session| session.id == session_id)
                .map(tmux_session_name)
                .or_else(|| {
                    self.tmux.data().and_then(|snapshot| {
                        snapshot
                            .sessions
                            .iter()
                            .find(|session| session.id == session_id)
                            .map(tmux_session_name)
                    })
                })
                .unwrap_or_else(|| session_id.to_owned());
            let confirm = Button::new("tmux-panel-end-session-confirm")
                .compact()
                .danger()
                .icon(IconName::Check)
                .label(crate::locale::text("结束会话", "End session"))
                .disabled(!self.tmux_dialog_submission_enabled())
                .on_click(
                    cx.listener(|view, _: &ClickEvent, _, cx| view.confirm_root_confirmation(cx)),
                );
            let content = div()
                .w_full()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .text_size(theme.text_sm)
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "会话中的运行任务会一并停止，且无法从 Kerminal 恢复。",
                            "Running tasks in this session will stop, and Kerminal cannot restore it.",
                        )),
                )
                .when_some(self.status.clone(), |content, message| {
                    content.child(tmux_dialog_feedback(message, theme))
                });
            let actions = tmux_panel_dialog_actions(
                available_width,
                theme,
                tmux_dialog_cancel_button(self, cx),
                confirm,
            );
            return Some(
                tmux_panel_dialog_shell(
                    "tmux-panel-end-session-dialog",
                    crate::locale::text("结束 tmux 会话", "End tmux session"),
                    Some(crate::locale::text(
                        format!("将结束会话“{session_name}”。"),
                        format!("End session “{session_name}”."),
                    )),
                    content,
                    actions,
                    tmux_panel_dialog_close_button(self, theme, cx),
                    theme,
                )
                .into_any_element(),
            );
        }

        if self.tmux_rename_target.is_some() {
            let Some(inputs) = self.inputs.as_ref() else {
                return Some(
                    tmux_panel_input_unavailable_dialog(
                        "tmux-panel-rename-dialog",
                        crate::locale::text("重命名 tmux 会话", "Rename tmux session"),
                        self,
                        theme,
                        cx,
                    )
                    .into_any_element(),
                );
            };
            let name_is_empty = self.input_value(&inputs.tmux_rename, cx).trim().is_empty();
            let save = Button::new("tmux-panel-rename-confirm")
                .compact()
                .primary()
                .icon(IconName::Check)
                .label(crate::locale::text("保存", "Save"))
                .disabled(name_is_empty || !self.tmux_dialog_submission_enabled())
                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| view.rename_tmux_session(cx)));
            let content = div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(tmux_panel_dialog_field_label(
                    crate::locale::text("会话名称", "Session name"),
                    theme,
                ))
                .child(tmux_panel_dialog_input(
                    &inputs.tmux_rename,
                    theme,
                    cx.theme().font_family.clone(),
                ))
                .when_some(self.status.clone(), |content, message| {
                    content.child(tmux_dialog_feedback(message, theme))
                });
            let actions = tmux_panel_dialog_actions(
                available_width,
                theme,
                tmux_dialog_cancel_button(self, cx),
                save,
            );
            return Some(
                tmux_panel_dialog_shell(
                    "tmux-panel-rename-dialog",
                    crate::locale::text("重命名 tmux 会话", "Rename tmux session"),
                    None,
                    content,
                    actions,
                    tmux_panel_dialog_close_button(self, theme, cx),
                    theme,
                )
                .into_any_element(),
            );
        }

        if self.tmux_create_form_open {
            let Some(inputs) = self.inputs.as_ref() else {
                return Some(
                    tmux_panel_input_unavailable_dialog(
                        "tmux-panel-create-dialog",
                        crate::locale::text("新建 tmux 会话", "Create tmux session"),
                        self,
                        theme,
                        cx,
                    )
                    .into_any_element(),
                );
            };
            let name_is_empty = self.input_value(&inputs.tmux_name, cx).trim().is_empty();
            let create_enabled = super::super::actions::tmux_session_creation_enabled(
                self.busy,
                self.tmux_target_available(),
                self.tmux.data().map(|snapshot| &snapshot.capability),
            );
            let create = Button::new("tmux-panel-create-confirm")
                .compact()
                .primary()
                .icon(IconName::Check)
                .label(crate::locale::text("创建", "Create"))
                .disabled(
                    name_is_empty || !self.tmux_dialog_submission_enabled() || !create_enabled,
                )
                .on_click(cx.listener(|view, _: &ClickEvent, _, cx| view.create_tmux_session(cx)));
            let content = div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(tmux_panel_dialog_field_label(
                    crate::locale::text("会话名称", "Session name"),
                    theme,
                ))
                .child(tmux_panel_dialog_input(
                    &inputs.tmux_name,
                    theme,
                    cx.theme().font_family.clone(),
                ))
                .when_some(self.status.clone(), |content, message| {
                    content.child(tmux_dialog_feedback(message, theme))
                });
            let actions = tmux_panel_dialog_actions(
                available_width,
                theme,
                tmux_dialog_cancel_button(self, cx),
                create,
            );
            return Some(
                tmux_panel_dialog_shell(
                    "tmux-panel-create-dialog",
                    crate::locale::text("新建 tmux 会话", "Create tmux session"),
                    None,
                    content,
                    actions,
                    tmux_panel_dialog_close_button(self, theme, cx),
                    theme,
                )
                .into_any_element(),
            );
        }

        None
    }

    /// 将目标、能力状态和会话列表放进同一视觉层级；创建、改名与结束确认由 Root 唯一模态宿主承载。
    ///
    /// @author kongweiguang
    pub(super) fn render_tmux(&self, theme: ThemeTokens, cx: &mut Context<Self>) -> AnyElement {
        if self.section_selection.is_fixed() {
            return self.render_tmux_tool_rail(theme, cx).into_any_element();
        }

        let sessions = self
            .tmux
            .data()
            .map(|snapshot| snapshot.sessions.clone())
            .unwrap_or_default();
        let attach_ready = self
            .tmux_attach_binding
            .as_ref()
            .is_some_and(|binding| binding.target.matches_tmux_target(&self.tmux_target()));
        let capability = self.tmux.data().map(|snapshot| &snapshot.capability);
        let create_enabled = super::super::actions::tmux_session_creation_enabled(
            self.busy,
            self.tmux_target_available(),
            capability,
        );
        let mut body = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.render_tmux_header(theme, create_enabled, attach_ready, cx));
        if !self.tmux_target_available() {
            body = body.child(tmux_empty_state(
                crate::locale::text("暂无可用目标", "No available target"),
                crate::locale::text(
                    "请选择 SSH 主机或恢复本机终端，再聚焦对应终端以连接会话。",
                    "Select an SSH host or restore the local terminal, then focus its terminal to attach.",
                ),
                theme,
            ));
        } else {
            body = body.child(section_feedback(
                &self.tmux,
                false,
                crate::locale::text(
                    "尚未读取 tmux 会话。",
                    "tmux sessions have not been loaded.",
                ),
                theme,
            ));
            if let Some((zh_cn, en_us)) =
                tmux_attach_guidance(attach_ready, self.tmux_attach_binding.is_some())
            {
                body = body.child(
                    div()
                        .w_full()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text(zh_cn, en_us)),
                );
            }
            if self.tmux.phase() == SectionPhase::Ready
                && let Some(snapshot) = self.tmux.data()
            {
                if !snapshot.capability.available {
                    let reason = snapshot.capability.reason.clone().unwrap_or_else(|| {
                        crate::locale::text(
                            "目标未安装 tmux。",
                            "tmux is not installed on this target.",
                        )
                        .to_string()
                    });
                    body = body.child(tmux_empty_state(
                        crate::locale::text("tmux 不可用", "tmux unavailable"),
                        reason,
                        theme,
                    ));
                } else if sessions.is_empty() {
                    body = body.child(tmux_empty_state(
                        crate::locale::text("暂无会话", "No sessions"),
                        crate::locale::text(
                            "新建一个 tmux 会话即可开始。",
                            "Create a tmux session to get started.",
                        ),
                        theme,
                    ));
                }
            }
            for session in sessions {
                body = body.child(self.render_tmux_session(session, theme, cx));
            }
            body = body.child(self.render_tmux_quickref(theme, cx));
        }
        section_page(
            crate::locale::text("tmux", "tmux"),
            body,
            theme,
            self.compact_layout,
            self.section_selection.is_fixed(),
        )
        .into_any_element()
    }

    /// 固定右栏沿用 main 的 16px 卡片间距；无文案反馈不挂载为空节点，避免多占一组 flex gap。
    ///
    /// @author kongweiguang
    fn render_tmux_tool_rail(
        &self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let capability = self.tmux.data().map(|snapshot| &snapshot.capability);
        let create_enabled = super::super::actions::tmux_session_creation_enabled(
            self.busy,
            self.tmux_target_available(),
            capability,
        );
        let mut body = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_4()
            .child(self.render_tmux_tool_rail_header(theme, create_enabled, cx))
            .children(self.compact_operation_feedback(theme));

        if self.tmux_target_available() {
            if tmux_tool_rail_feedback_visible(self.tmux.phase(), self.tmux.error().is_some()) {
                body = body.child(section_feedback(
                    &self.tmux,
                    false,
                    crate::locale::text(
                        "尚未读取 tmux 会话。",
                        "tmux sessions have not been loaded.",
                    ),
                    theme,
                ));
            }
            body = body.child(self.render_tmux_tool_rail_sessions(theme, cx));
        } else {
            body = body.child(tmux_empty_state(
                crate::locale::text("目标不可用", "Target unavailable"),
                self.tmux_tool_rail_target_label(),
                theme,
            ));
        }
        body = body.child(self.render_tmux_tool_rail_quickref(theme, cx));

        section_page(
            crate::locale::text("tmux", "tmux"),
            body,
            theme,
            self.compact_layout,
            true,
        )
    }

    /// 固定栏标题保留目标别名；只有 SSH 目标去掉工具面板通用 helper 添加的传输前缀。
    ///
    /// @author kongweiguang
    fn tmux_tool_rail_target_label(&self) -> String {
        let host_id = match self.tool_rail_target.as_ref() {
            Some(super::super::ToolRailOperationTarget::Ssh { host_id }) => Some(host_id),
            None => self.host_target.as_ref(),
            Some(_) => None,
        };
        let Some(host_id) = host_id else {
            return self.tool_rail_target_label();
        };
        let display_name = self
            .host_display_names
            .get(host_id)
            .map(|name| name.to_string());
        tmux_ssh_host_display_label(display_name.as_deref(), host_id.as_str())
    }

    /// 状态卡沿用 main 的中性描边图标表面；动作保留当前 pane 身份门禁，版本补充能力状态。
    ///
    /// @author kongweiguang
    fn render_tmux_tool_rail_header(
        &self,
        theme: ThemeTokens,
        create_enabled: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let capability = self.tmux.data().map(|snapshot| &snapshot.capability);
        let status_tone = tmux_header_tone(self.tmux.phase(), capability);
        let status_color = match status_tone {
            TmuxHeaderTone::Ready => theme.ready_connection_indicator(),
            TmuxHeaderTone::Loading => theme.accent,
            TmuxHeaderTone::Error => theme.error,
            TmuxHeaderTone::Neutral => theme.muted,
        };
        let mono_font_family = cx.theme().mono_font_family.clone();
        let available = self.tmux_target_available()
            && self.tmux.phase() == SectionPhase::Ready
            && capability.is_some_and(|capability| capability.available);
        let status = tmux_tool_rail_header_status(
            self.tmux.phase(),
            capability,
            self.tmux_target_available(),
        );
        let refresh_enabled = self.tmux_target_available()
            && !self.busy
            && self.tmux.phase() != SectionPhase::Loading;
        let detach_enabled = self.current_tmux_attach_intent().is_some()
            && self.tmux_detach_pending.is_none()
            && !self.busy;

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_xl()
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .size_8()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.secondary)
                            .child(
                                gpui_kit::component::Icon::new(IconName::Terminal)
                                    .text_color(theme.muted),
                            ),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .line_height(relative(1.5))
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .font_family(mono_font_family.clone())
                                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                            .text_color(theme.muted)
                                            .child("TMUX"),
                                    )
                                    .when(available, |row| {
                                        row.child(
                                            div().size(px(6.)).rounded_full().bg(status_color),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(theme.text_body)
                                    .font_family(mono_font_family)
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .text_color(theme.foreground)
                                    .line_height(relative(10. / 7.))
                                    .child(self.tmux_tool_rail_target_label()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new("operations-tmux-refresh")
                                    .compact()
                                    .ghost()
                                    .icon(IconName::RefreshCw)
                                    .accessibility_label(crate::locale::text("刷新", "Refresh"))
                                    .tooltip(crate::locale::text("刷新", "Refresh"))
                                    .disabled(!refresh_enabled)
                                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                        view.load_tmux(cx)
                                    })),
                            )
                            .child(
                                Button::new("tmux-create-open")
                                    .compact()
                                    .ghost()
                                    .icon(IconName::Plus)
                                    .accessibility_label(crate::locale::text(
                                        "新建会话",
                                        "Create session",
                                    ))
                                    .tooltip(crate::locale::text("新建会话", "Create session"))
                                    .disabled(self.busy || !create_enabled)
                                    .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                        view.request_tmux_create_dialog(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("tmux-detach-current")
                                    .compact()
                                    .ghost()
                                    .icon(IconName::LogOut)
                                    .accessibility_label(crate::locale::text(
                                        "退出 tmux",
                                        "Exit tmux",
                                    ))
                                    .tooltip(crate::locale::text("退出 tmux", "Exit tmux"))
                                    .disabled(!detach_enabled)
                                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                        view.request_tmux_detach_current(cx);
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .text_size(px(11.))
                    .line_height(relative(1.5))
                    .font_family("monospace")
                    .text_color(theme.muted)
                    .child(status),
            )
    }

    /// 将会话状态压缩成 main 的四种空态，并只为已读到的可用列表创建稳定身份行。
    ///
    /// @author kongweiguang
    fn render_tmux_tool_rail_sessions(
        &self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match self.tmux.phase() {
            SectionPhase::Loading => {
                return tmux_empty_state(
                    crate::locale::text("正在读取会话", "Loading sessions"),
                    crate::locale::text(
                        "正在检测 tmux 能力与会话。",
                        "Checking tmux availability and sessions.",
                    ),
                    theme,
                )
                .into_any_element();
            }
            SectionPhase::NotLoaded | SectionPhase::Cancelled => {
                return tmux_empty_state(
                    crate::locale::text("暂无会话", "No sessions"),
                    crate::locale::text(
                        "新建一个会话即可开始。",
                        "Create a session to get started.",
                    ),
                    theme,
                )
                .into_any_element();
            }
            SectionPhase::Ready | SectionPhase::Failed => {}
        }

        let Some(snapshot) = self.tmux.data() else {
            return tmux_empty_state(
                crate::locale::text("暂无会话", "No sessions"),
                crate::locale::text("新建一个会话即可开始。", "Create a session to get started."),
                theme,
            )
            .into_any_element();
        };
        if !snapshot.capability.available {
            let reason = snapshot.capability.reason.clone().unwrap_or_else(|| {
                crate::locale::text("目标未安装 tmux。", "tmux is not installed on this target.")
                    .to_string()
            });
            return tmux_empty_state(
                crate::locale::text("tmux 不可用", "tmux unavailable"),
                reason,
                theme,
            )
            .into_any_element();
        }
        if snapshot.sessions.is_empty() {
            return tmux_empty_state(
                crate::locale::text("暂无会话", "No sessions"),
                crate::locale::text("新建一个会话即可开始。", "Create a session to get started."),
                theme,
            )
            .into_any_element();
        }

        let current_session_id = self.tmux_panel_current_session_id();
        let current_session_name = self.tmux_current_session_name.as_deref();
        let sessions = sort_tmux_panel_sessions(
            snapshot.sessions.clone(),
            current_session_id,
            current_session_name,
            &snapshot.capability.target_ref,
        );
        let count = sessions.len();
        let mut rows = div()
            .w_full()
            .flex()
            .flex_col()
            .rounded_xl()
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel);
        for (index, session) in sessions.into_iter().enumerate() {
            rows = rows.child(self.render_tmux_tool_rail_session(
                session,
                index + 1 < count,
                theme,
                cx,
            ));
        }
        rows.into_any_element()
    }

    /// 状态点与标题保持 main 的紧凑间距；结束操作保留危险色，展开只展示快照摘要而不发起网络读取。
    ///
    /// @author kongweiguang
    fn render_tmux_tool_rail_session(
        &self,
        session: TmuxSession,
        has_next: bool,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let display_name = tmux_session_name(&session);
        let expanded = self.tmux_panel.expanded_session_ids.contains(&session.id);
        let current = self.tmux_panel_session_is_current(&session);
        let (status_zh, status_en) = tmux_tool_rail_session_status(current, session.attached);
        let collapse_label = crate::locale::text(
            format!("收起 {} 详情", display_name),
            format!("Collapse {} details", display_name),
        );
        let expand_label = crate::locale::text(
            format!("展开 {} 详情", display_name),
            format!("Expand {} details", display_name),
        );
        let status_color = if current {
            theme.active_connection_indicator()
        } else if session.attached {
            theme.ready_connection_indicator()
        } else {
            theme.muted
        };
        let mono_font_family = cx.theme().mono_font_family.clone();
        let state_id = session.id.clone();
        let attach_id = session.id.clone();
        let rename_id = session.id.clone();
        let kill_id = session.id.clone();
        let pending_kill = self.tmux_pending_kill.as_deref() == Some(session.id.as_str());
        let row = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .px_2p5()
            .py_2()
            .when(has_next, |row| row.border_b_1().border_color(theme.border))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .child(
                                div()
                                    .size(if current { px(10.) } else { px(6.) })
                                    .rounded_full()
                                    .when(current, |dot| {
                                        dot.flex()
                                            .items_center()
                                            .justify_center()
                                            .border_1()
                                            .border_color(theme.active_connection_indicator())
                                            .child(
                                                div().size(px(4.)).rounded_full().bg(status_color),
                                            )
                                    })
                                    .when(!current, |dot| dot.bg(status_color)),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(px(13.))
                                            .font_family(mono_font_family.clone())
                                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                            .child(display_name.clone()),
                                    )
                                    .child(
                                        div()
                                            .min_w_0()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .text_size(px(11.))
                                            .text_color(theme.muted)
                                            .child(crate::locale::text(status_zh, status_en))
                                            .child(div().min_w_0().truncate().child(format!(
                                                "{} {} · {} {}",
                                                session.windows,
                                                localized_text(
                                                    "个窗口",
                                                    if session.windows == 1 {
                                                        "window"
                                                    } else {
                                                        "windows"
                                                    },
                                                ),
                                                session.clients,
                                                localized_text(
                                                    "个客户端",
                                                    if session.clients == 1 {
                                                        "client"
                                                    } else {
                                                        "clients"
                                                    },
                                                ),
                                            ))),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new(format!("tmux-panel-details-{}", session.id))
                                    .compact()
                                    .ghost()
                                    .text_color(theme.muted)
                                    .icon(if expanded {
                                        IconName::ChevronUp
                                    } else {
                                        IconName::ChevronDown
                                    })
                                    .accessibility_label(if expanded {
                                        collapse_label.clone()
                                    } else {
                                        expand_label.clone()
                                    })
                                    .tooltip(if expanded {
                                        collapse_label
                                    } else {
                                        expand_label
                                    })
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.tmux_panel.toggle_session_details(&state_id);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new(format!("tmux-panel-attach-{}", session.id))
                                    .compact()
                                    .ghost()
                                    .text_color(theme.muted)
                                    .icon(IconName::Link2)
                                    .accessibility_label(crate::locale::text(
                                        "连接到当前终端",
                                        "Attach to current terminal",
                                    ))
                                    .tooltip(crate::locale::text("连接", "Attach"))
                                    .disabled(
                                        self.busy
                                            || self.tmux_detach_pending.is_some()
                                            || !self.tmux_target_available()
                                            || !self.tmux_attach_ready(),
                                    )
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.request_tmux_attach(
                                            attach_id.clone(),
                                            display_name.clone(),
                                            cx,
                                        )
                                    })),
                            )
                            .child(
                                Button::new(format!("tmux-panel-rename-{}", session.id))
                                    .compact()
                                    .ghost()
                                    .text_color(theme.muted)
                                    .icon(IconName::Pencil)
                                    .accessibility_label(crate::locale::text(
                                        "重命名会话",
                                        "Rename session",
                                    ))
                                    .tooltip(crate::locale::text("重命名", "Rename"))
                                    .disabled(self.busy || !self.tmux_target_available())
                                    .on_click(cx.listener(
                                        move |view, _: &ClickEvent, window, cx| {
                                            view.request_tmux_rename_dialog(
                                                rename_id.clone(),
                                                window,
                                                cx,
                                            )
                                        },
                                    )),
                            )
                            .child(
                                Button::new(format!("tmux-panel-kill-{}", session.id))
                                    .compact()
                                    .ghost()
                                    .custom(
                                        ButtonCustomVariant::new(cx)
                                            .color(theme.panel.opacity(0.))
                                            .foreground(theme.error)
                                            .hover(theme.error.opacity(0.1))
                                            .active(theme.error.opacity(0.16)),
                                    )
                                    .icon(if pending_kill {
                                        IconName::CircleCheck
                                    } else {
                                        IconName::Trash
                                    })
                                    .accessibility_label(if pending_kill {
                                        crate::locale::text("确认结束会话", "Confirm end session")
                                    } else {
                                        crate::locale::text("结束会话", "End session")
                                    })
                                    .tooltip(crate::locale::text("删除", "Delete"))
                                    .disabled(self.busy || !self.tmux_target_available())
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.request_tmux_kill_dialog(kill_id.clone(), cx);
                                    })),
                            ),
                    ),
            );
        let row = if expanded {
            row.child(tmux_tool_rail_session_details(
                &session,
                theme,
                mono_font_family,
            ))
        } else {
            row
        };
        row.into_any_element()
    }

    /// 速查标签保持 main 的字号、计数靠右贴近展开图标；发送仍只看当前 pane binding 与忙碌状态。
    ///
    /// @author kongweiguang
    fn render_tmux_tool_rail_quickref(
        &self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let expanded = self.tmux_panel.quickref_open;
        let entity = cx.entity().clone();
        let mono_font_family = cx.theme().mono_font_family.clone();
        let mut panel = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_xl()
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("tmux-panel-quickref-toggle")
                            .compact()
                            .h_5()
                            .ghost()
                            .icon(
                                gpui_kit::component::Icon::new(IconName::CircleDot)
                                    .text_color(theme.accent),
                            )
                            .accessibility_label(if expanded {
                                crate::locale::text("折叠快捷命令", "Collapse quick commands")
                            } else {
                                crate::locale::text("展开快捷命令", "Expand quick commands")
                            })
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .child(crate::locale::text("快捷命令", "Quick commands")),
                            )
                            .on_click(move |_, _, app| {
                                entity.update(app, |view, cx| {
                                    view.tmux_panel.toggle_quickref();
                                    cx.notify();
                                });
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.muted)
                            .child(crate::locale::text("20 项", "20 items")),
                    )
                    .child(
                        gpui_kit::component::Icon::new(if expanded {
                            IconName::ChevronUp
                        } else {
                            IconName::ChevronDown
                        })
                        .text_color(theme.muted),
                    ),
            );
        if expanded {
            panel = panel
                .child(
                    div()
                        .mb_1p5()
                        .font_family(mono_font_family.clone())
                        .text_size(px(10.))
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .text_color(theme.muted)
                        .child(crate::locale::text("命令", "Commands")),
                )
                .child(self.render_tmux_tool_rail_quickref_items(
                    "command",
                    tmux_commands(),
                    theme,
                    cx,
                ))
                .child(
                    div()
                        .mb_1p5()
                        .font_family(mono_font_family)
                        .text_size(px(10.))
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .text_color(theme.muted)
                        .child(crate::locale::text("快捷键", "Shortcuts")),
                )
                .child(self.render_tmux_tool_rail_quickref_items(
                    "shortcut",
                    tmux_shortcuts(),
                    theme,
                    cx,
                ));
        }
        panel
    }

    /// 命令原文与解释分行显示，复制和发送分别使用系统剪贴板及 Root 精确 pane 路由。
    ///
    /// @author kongweiguang
    fn render_tmux_tool_rail_quickref_items(
        &self,
        group: &'static str,
        items: &[super::super::quickref::TmuxQuickrefItem],
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let send_disabled = self.tmux_quickref_binding.is_none() || self.busy;
        let send_reason = if self.tmux_quickref_binding.is_none() {
            crate::locale::text("先聚焦左侧终端", "Focus a terminal first")
        } else if self.busy {
            crate::locale::text("正在执行操作", "An operation is in progress")
        } else {
            crate::locale::text("发送到当前终端", "Send to current terminal")
        };
        let mut rows = div().w_full().flex().flex_col().gap_1p5();
        let mono_font_family = cx.theme().mono_font_family.clone();
        for (index, item) in items.iter().copied().enumerate() {
            let copy_text = tmux_quickref_copy_text(item).to_owned();
            let copy_label = if item.kind == super::super::TmuxQuickrefKind::Shortcut {
                crate::locale::text("复制快捷键", "Copy shortcut")
            } else {
                crate::locale::text("复制命令", "Copy command")
            };
            rows = rows.child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2p5()
                    .py_2()
                    .rounded_xl()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.subtle_background())
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(11.))
                                    .font_family(mono_font_family.clone())
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .child(item.display),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(theme.muted)
                                    .child(crate::locale::text(item.label_zh, item.label_en)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new(format!("tmux-panel-{group}-{index}-copy"))
                                    .compact()
                                    .ghost()
                                    .icon(IconName::Copy)
                                    .accessibility_label(copy_label.clone())
                                    .tooltip(copy_label)
                                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                            copy_text.clone(),
                                        ));
                                    })),
                            )
                            .child(
                                Button::new(format!("tmux-panel-{group}-{index}-send"))
                                    .compact()
                                    .ghost()
                                    .icon(IconName::Send)
                                    .accessibility_label(crate::locale::text(
                                        "发送到当前终端",
                                        "Send to current terminal",
                                    ))
                                    .tooltip(send_reason.clone())
                                    .disabled(send_disabled)
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.send_tmux_quickref(item, cx)
                                    })),
                            ),
                    ),
            );
        }
        rows
    }

    /// 只读取 Root 已核验的 pane tmux binding，面板本地展开或 attach 意图不决定当前会话。
    ///
    /// @author kongweiguang
    fn tmux_panel_current_session_id(&self) -> Option<&str> {
        self.tmux_current_session_id.as_deref()
    }

    /// 状态点和标签共用同一身份判定，避免把其它 tmux client 的 attached 状态误报为当前终端。
    ///
    /// @author kongweiguang
    fn tmux_panel_session_is_current(&self, session: &TmuxSession) -> bool {
        let Some(snapshot) = self.tmux.data() else {
            return false;
        };
        tmux_panel_session_matches_current(
            session,
            self.tmux_panel_current_session_id(),
            self.tmux_current_session_name.as_deref(),
            &snapshot.capability.target_ref,
        )
    }

    /// 显示 tmux 目标和能力状态；新建入口提交 Root 确认意图，分离仍通过当前 pane 身份门禁。
    ///
    /// @author kongweiguang
    fn render_tmux_header(
        &self,
        theme: ThemeTokens,
        create_enabled: bool,
        attach_ready: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let capability = self.tmux.data().map(|snapshot| &snapshot.capability);
        let tone = tmux_header_tone(self.tmux.phase(), capability);
        let status = tmux_header_status(self.tmux.phase(), capability);
        let refresh_enabled = self.tmux_target_available()
            && !self.busy
            && self.tmux.phase() != SectionPhase::Loading;
        let selected_session = self.tmux_details.session_id.as_deref().and_then(|id| {
            self.tmux
                .data()
                .and_then(|snapshot| snapshot.sessions.iter().find(|session| session.id == id))
        });
        let selected_session_attached = self.tmux.phase() == SectionPhase::Ready
            && selected_session.is_some_and(|session| session.attached);
        let has_current_attach_intent = self.current_tmux_attach_intent().is_some();
        let can_detach_current = selected_session_attached
            && (self.tmux_quickref_binding.is_some() || has_current_attach_intent)
            && self.tmux_detach_pending.is_none()
            && attach_ready
            && !self.busy;
        let target_label = self.tool_rail_target_label();
        let version = self
            .tmux
            .data()
            .and_then(|snapshot| snapshot.capability.version.as_deref());
        let status_color = match tone {
            TmuxHeaderTone::Ready => theme.success,
            TmuxHeaderTone::Loading => theme.accent,
            TmuxHeaderTone::Error => theme.error,
            TmuxHeaderTone::Neutral => theme.muted,
        };
        let detach_item = tmux_detach_shortcut();
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .size(px(32.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .bg(theme.secondary)
                            .child(
                                gpui_kit::component::Icon::new(IconName::SquareTerminal)
                                    .text_color(theme.accent),
                            ),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_size(theme.text_heading)
                                            .font_weight(gpui_kit::FontWeight::BOLD)
                                            .child("TMUX"),
                                    )
                                    .child(div().size(px(8.)).rounded_full().bg(status_color)),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(theme.text_body)
                                    .text_color(theme.muted)
                                    .child(target_label),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new("operations-tmux-refresh")
                                    .compact()
                                    .ghost()
                                    .icon(IconName::RefreshCw)
                                    .accessibility_label(crate::locale::text(
                                        "刷新 tmux 会话",
                                        "Refresh tmux sessions",
                                    ))
                                    .tooltip(crate::locale::text(
                                        "刷新 tmux 会话",
                                        "Refresh tmux sessions",
                                    ))
                                    .disabled(!refresh_enabled)
                                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                        view.load_tmux(cx)
                                    })),
                            )
                            .child(
                                Button::new("tmux-create-open")
                                    .compact()
                                    .primary()
                                    .icon(IconName::Plus)
                                    .accessibility_label(crate::locale::text(
                                        "新建会话",
                                        "Create session",
                                    ))
                                    .tooltip(crate::locale::text("新建会话", "Create session"))
                                    .disabled(self.busy || !create_enabled)
                                    .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                        view.request_tmux_create_dialog(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("tmux-detach-current")
                                    .compact()
                                    .ghost()
                                    .icon(IconName::LogOut)
                                    .accessibility_label(crate::locale::text(
                                        "发送默认 tmux 分离快捷键",
                                        "Send the default tmux detach shortcut",
                                    ))
                                    .tooltip(crate::locale::text(
                                        "发送 Ctrl+B,d。若目标修改了 prefix，请从快捷键参考中选择对应按键。",
                                        "Sends Ctrl+B,d. For a custom prefix, use the shortcut reference.",
                                    ))
                                    .disabled(!can_detach_current)
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        if view.tmux_quickref_binding.is_some() {
                                            if let Some(item) = detach_item {
                                                view.send_tmux_quickref(item, cx);
                                            }
                                        } else {
                                            view.request_tmux_detach_current(cx);
                                        }
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(status.to_owned())
                    .when_some(version, |row, version| {
                        row.child(div().truncate().child(version.to_owned()))
                    }),
            )
    }

    /// 将 20 项固定速查收在会话列表之后，默认折叠以把空间留给当前主机状态。
    ///
    /// @author kongweiguang
    fn render_tmux_quickref(&self, theme: ThemeTokens, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.tmux_quickref_open;
        let entity = cx.entity().clone();
        let mut panel = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("tmux-quickref-toggle")
                            .compact()
                            .ghost()
                            .icon(IconName::CircleDot)
                            .accessibility_label(if open {
                                crate::locale::text("折叠快捷命令", "Collapse quick commands")
                            } else {
                                crate::locale::text("展开快捷命令", "Expand quick commands")
                            })
                            .label(crate::locale::text("快捷命令", "Quick commands"))
                            .on_click(move |_, _, app| {
                                entity.update(app, |view, cx| {
                                    view.tmux_quickref_open = !view.tmux_quickref_open;
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(crate::locale::text("20 项", "20 items")),
                    )
                    .child(
                        gpui_kit::component::Icon::new(if open {
                            IconName::ChevronUp
                        } else {
                            IconName::ChevronDown
                        })
                        .text_color(theme.muted),
                    ),
            );
        if open {
            panel = panel
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text("命令", "Commands")),
                )
                .child(self.render_tmux_quickref_items("command", tmux_commands(), theme, cx))
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text("快捷键", "Shortcuts")),
                )
                .child(self.render_tmux_quickref_items("shortcut", tmux_shortcuts(), theme, cx));
        }
        panel
    }

    /// 单条固定速查支持复制；发送只在选择了会话并持有 Root 投影的 pane binding 时开放。
    ///
    /// @author kongweiguang
    fn render_tmux_quickref_items(
        &self,
        group: &'static str,
        items: &[super::super::quickref::TmuxQuickrefItem],
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let send_available =
            self.tmux_details.session_id.is_some() && self.tmux_quickref_binding.is_some();
        let mut rows = div().w_full().flex().flex_col().gap_1();
        for (index, item) in items.iter().copied().enumerate() {
            let payload = item.payload.to_owned();
            let copy_id = format!("tmux-quickref-{group}-{index}-copy");
            let send_id = format!("tmux-quickref-{group}-{index}-send");
            rows = rows.child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .p_2()
                    .rounded_md()
                    .bg(theme.secondary)
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .text_size(theme.text_xs)
                            .child(format!(
                                "{} · {}",
                                crate::locale::text(item.label_zh, item.label_en),
                                item.display,
                            )),
                    )
                    .child(
                        Button::new(copy_id)
                            .compact()
                            .ghost()
                            .icon(IconName::Copy)
                            .accessibility_label(crate::locale::text("复制", "Copy"))
                            .tooltip(crate::locale::text("复制", "Copy"))
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                    payload.clone(),
                                ));
                            })),
                    )
                    .child(
                        Button::new(send_id)
                            .compact()
                            .ghost()
                            .icon(IconName::Send)
                            .accessibility_label(crate::locale::text(
                                "发送到当前 tmux 会话",
                                "Send to current tmux session",
                            ))
                            .tooltip(crate::locale::text(
                                "发送到当前 tmux 会话",
                                "Send to current tmux session",
                            ))
                            .disabled(!send_available)
                            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                view.send_tmux_quickref(item, cx)
                            })),
                    ),
            );
        }
        rows
    }

    /// 以短状态、窗口和客户端数量组织会话行；结束会话会停止会话内任务且不可恢复，
    /// 因此二次确认必须绑定并显示精确会话名，避免用户确认错行或误认为只断开客户端。
    ///
    /// @author kongweiguang
    fn render_tmux_session(
        &self,
        session: TmuxSession,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let expanded = self.tmux_details.session_id.as_deref() == Some(session.id.as_str());
        let attach_id = format!("tmux-attach-{}", session.id);
        let rename_id = format!("tmux-rename-{}", session.id);
        let kill_id = format!("tmux-kill-{}", session.id);
        let select_id = session.id.clone();
        let select_name = session.name.clone();
        let attach_session_id = session.id.clone();
        let attach_session_name = session.name.clone();
        let rename_session_id = session.id.clone();
        let kill_session_id = session.id.clone();
        let pending_kill = self.tmux_pending_kill.as_deref() == Some(session.id.as_str());
        let state_color = if session.attached {
            theme.success
        } else {
            theme.muted
        };
        let state_label = if session.attached {
            crate::locale::text("已连接", "Attached")
        } else {
            crate::locale::text("未连接", "Detached")
        };
        let window_count = format!(
            "{} {}",
            session.windows,
            localized_text(
                "个窗口",
                if session.windows == 1 {
                    "window"
                } else {
                    "windows"
                },
            ),
        );
        let client_count = format!(
            "{} {}",
            session.clients,
            localized_text(
                "个客户端",
                if session.clients == 1 {
                    "client"
                } else {
                    "clients"
                },
            ),
        );
        let mut row = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .child(
                div()
                    .w_full()
                    .flex()
                    .when(self.compact_layout, |line| line.flex_col().items_stretch())
                    .when(!self.compact_layout, |line| line.items_center())
                    .flex_wrap()
                    .gap_2()
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().size(px(7.)).rounded_full().bg(state_color))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(theme.text_body)
                                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                            .child(if session.name.is_empty() {
                                                session.id.clone()
                                            } else {
                                                session.name.clone()
                                            }),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_wrap()
                                            .items_center()
                                            .gap_2()
                                            .text_size(theme.text_xs)
                                            .text_color(theme.muted)
                                            .child(state_label)
                                            .child(format!("{window_count} · {client_count}")),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new(format!("tmux-details-{}", session.id))
                                    .compact()
                                    .ghost()
                                    .icon(if expanded {
                                        IconName::ChevronUp
                                    } else {
                                        IconName::ChevronDown
                                    })
                                    .accessibility_label(if expanded {
                                        crate::locale::text(
                                            "收起会话详情",
                                            "Collapse session details",
                                        )
                                    } else {
                                        crate::locale::text(
                                            "展开会话详情",
                                            "Expand session details",
                                        )
                                    })
                                    .tooltip(if expanded {
                                        crate::locale::text(
                                            "收起会话详情",
                                            "Collapse session details",
                                        )
                                    } else {
                                        crate::locale::text(
                                            "展开会话详情",
                                            "Expand session details",
                                        )
                                    })
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.toggle_tmux_session_details(
                                            select_id.clone(),
                                            select_name.clone(),
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new(attach_id)
                                    .compact()
                                    .ghost()
                                    .icon(IconName::Link2)
                                    .accessibility_label(crate::locale::text(
                                        "连接到当前终端",
                                        "Attach to current terminal",
                                    ))
                                    .tooltip(crate::locale::text(
                                        "连接到当前终端",
                                        "Attach to current terminal",
                                    ))
                                    .disabled(
                                        self.busy
                                            || self.tmux_detach_pending.is_some()
                                            || !self.tmux_target_available()
                                            || !self.tmux_attach_ready(),
                                    )
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.request_tmux_attach(
                                            attach_session_id.clone(),
                                            attach_session_name.clone(),
                                            cx,
                                        )
                                    })),
                            )
                            .child(
                                Button::new(rename_id)
                                    .compact()
                                    .ghost()
                                    .icon(IconName::Pencil)
                                    .accessibility_label(crate::locale::text(
                                        "重命名会话",
                                        "Rename session",
                                    ))
                                    .tooltip(crate::locale::text("重命名会话", "Rename session"))
                                    .disabled(self.busy || !self.tmux_target_available())
                                    .on_click(cx.listener(
                                        move |view, _: &ClickEvent, window, cx| {
                                            view.request_tmux_rename_dialog(
                                                rename_session_id.clone(),
                                                window,
                                                cx,
                                            )
                                        },
                                    )),
                            )
                            .child(
                                Button::new(kill_id)
                                    .compact()
                                    .ghost()
                                    .danger()
                                    .icon(if pending_kill {
                                        IconName::CircleCheck
                                    } else {
                                        IconName::Trash
                                    })
                                    .accessibility_label(if pending_kill {
                                        crate::locale::text("确认结束会话", "Confirm end session")
                                    } else {
                                        crate::locale::text("结束会话", "End session")
                                    })
                                    .tooltip(if pending_kill {
                                        crate::locale::text("确认结束会话", "Confirm end session")
                                    } else {
                                        crate::locale::text("结束会话", "End session")
                                    })
                                    .disabled(self.busy || !self.tmux_target_available())
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.request_tmux_kill_dialog(kill_session_id.clone(), cx);
                                    })),
                            ),
                    ),
            );
        if expanded {
            row = row.child(self.render_tmux_details(theme, cx));
        }
        row
    }

    /// 折叠时取消仍在途的详情读取并增加世代，迟到回执不能重新打开旧会话。
    ///
    /// @author kongweiguang
    fn toggle_tmux_session_details(
        &mut self,
        session_id: String,
        session_name: String,
        cx: &mut Context<Self>,
    ) {
        if !self.tmux_target_available() {
            return;
        }
        if self.tmux_details.session_id.as_deref() != Some(session_id.as_str()) {
            self.select_tmux_session(session_id, session_name, cx);
            return;
        }
        self.tmux_details.cancel_pending_reads();
        let generation = self.tmux_details.generation.wrapping_add(1).max(1);
        self.tmux_details = TmuxDetailState {
            generation,
            ..TmuxDetailState::default()
        };
        cx.notify();
    }
    /// 将分离选定 session 的全部客户端作为低频操作放入展开区，标题栏动作仍只分离当前终端。
    ///
    /// @author kongweiguang
    fn render_tmux_details(&self, theme: ThemeTokens, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(session_id) = self.tmux_details.session_id.as_deref() else {
            return div().into_any_element();
        };
        let session_name = self
            .tmux_details
            .session_name
            .as_deref()
            .unwrap_or(session_id);
        let session_clients = self
            .tmux
            .data()
            .and_then(|snapshot| {
                snapshot
                    .sessions
                    .iter()
                    .find(|session| session.id.as_str() == session_id)
                    .map(|session| session.clients)
            })
            .unwrap_or(0);
        let can_detach_session_clients =
            session_clients > 0 && !self.busy && self.tmux_target_available();
        let detach_session_id = session_id.to_owned();
        let mut body = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .child(self.layout_info_row(
                &localized_text("会话详情", "Session details"),
                session_name,
                theme,
            ));
        if self.tmux_details.windows_loading {
            body = body.child(self.layout_info_row(
                &localized_text("窗口", "Windows"),
                crate::locale::text("正在读取…", "Loading…").to_string(),
                theme,
            ));
        }
        if let Some(error) = &self.tmux_details.windows_error {
            body = body.child(error_row(error, theme));
        }
        if let Some(windows) = &self.tmux_details.windows {
            if windows.is_empty() {
                body = body.child(
                    self.layout_info_row(
                        &localized_text("窗口", "Windows"),
                        crate::locale::text(
                            "当前 session 没有窗口。",
                            "This session has no windows.",
                        )
                        .to_string(),
                        theme,
                    ),
                );
            }
            for window in windows {
                body = body.child(self.render_tmux_window(window.clone(), cx));
            }
        }
        if self.tmux_details.panes_loading {
            body = body.child(self.layout_info_row(
                &localized_text("分屏", "Panes"),
                crate::locale::text("正在读取…", "Loading…").to_string(),
                theme,
            ));
        }
        if let Some(error) = &self.tmux_details.panes_error {
            body = body.child(error_row(error, theme));
        }
        if let Some(panes) = &self.tmux_details.panes {
            if panes.is_empty() {
                body = body.child(
                    self.layout_info_row(
                        &localized_text("分屏", "Panes"),
                        crate::locale::text("当前窗口没有 pane。", "This window has no panes.")
                            .to_string(),
                        theme,
                    ),
                );
            }
            for pane in panes {
                let id = pane.id.clone();
                body = body.child(
                    div()
                        .w_full()
                        .flex()
                        .when(self.compact_layout, |row| row.flex_col().items_stretch())
                        .when(!self.compact_layout, |row| row.items_center())
                        .flex_wrap()
                        .gap_2()
                        .child(
                            div()
                                .when(self.compact_layout, |text| text.w_full())
                                .when(!self.compact_layout, |text| text.flex_1())
                                .child(format!(
                                    "{} · {} · {}×{}{}",
                                    pane.title,
                                    pane.current_command,
                                    pane.width,
                                    pane.height,
                                    if pane.dead {
                                        localized_text(" · 已退出", " · exited")
                                    } else {
                                        String::new()
                                    },
                                )),
                        )
                        .child(
                            Button::new(format!("tmux-capture-{}", pane.id))
                                .compact()
                                .when(self.compact_layout, |button| button.w_full())
                                .label(crate::locale::text("捕获 200 行", "Capture 200 lines"))
                                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                    view.capture_tmux_pane(id.clone(), cx)
                                })),
                        ),
                );
            }
        }
        if let Some(capture) = &self.tmux_details.capture {
            body = body.child(
                div()
                    .p_2()
                    .rounded_sm()
                    .bg(theme.sidebar)
                    .text_size(theme.text_xs)
                    .child(format!(
                        "pane {} · {} {}{}\n{}",
                        capture.pane_id,
                        capture.captured_lines,
                        localized_text("行", "lines"),
                        if capture.truncated {
                            localized_text(" · 内容已截断", " · truncated")
                        } else {
                            String::new()
                        },
                        bounded_text(&capture.text, 12_000),
                    )),
            );
        }
        if self.tmux_details.capture_loading {
            body = body.child(self.layout_info_row(
                &localized_text("终端输出", "Pane capture"),
                crate::locale::text("正在捕获…", "Capturing…").to_string(),
                theme,
            ));
        }
        if let Some(error) = &self.tmux_details.capture_error {
            body = body.child(error_row(error, theme));
        }
        body = body.child(
            div()
                .w_full()
                .flex()
                .when(self.compact_layout, |row| row.flex_col().items_stretch())
                .when(!self.compact_layout, |row| row.items_center())
                .flex_wrap()
                .justify_between()
                .gap_2()
                .p_2()
                .rounded_md()
                .bg(theme.panel)
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            format!("将分离此会话的所有客户端（当前 {session_clients} 个）。"),
                            format!("Detach all clients from this session ({session_clients} connected)."),
                        )),
                )
                .child(
                    Button::new(format!("tmux-detach-session-clients-{session_id}"))
                        .compact()
                        .when(self.compact_layout, |button| button.w_full())
                        .ghost()
                        .icon(IconName::LogOut)
                        .label(crate::locale::text(
                            "分离此会话的客户端",
                            "Detach this session's clients",
                        ))
                        .disabled(!can_detach_session_clients)
                        .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                            view.detach_tmux_session(detach_session_id.clone(), cx)
                        })),
                ),
        );
        body.into_any_element()
    }

    /// 选中的 window ID 作为唯一 pane 查询范围传入 application，不使用窗口索引猜目标。
    ///
    /// @author kongweiguang
    fn render_tmux_window(&self, window: TmuxWindow, cx: &mut Context<Self>) -> impl IntoElement {
        let target_id = window.id.clone();
        div()
            .w_full()
            .flex()
            .when(self.compact_layout, |row| row.flex_col().items_stretch())
            .when(!self.compact_layout, |row| row.items_center())
            .flex_wrap()
            .gap_2()
            .child(
                div()
                    .when(self.compact_layout, |text| text.w_full())
                    .when(!self.compact_layout, |text| text.flex_1())
                    .child(format!(
                        "{}{} · {} panes · {}×{}",
                        if window.active { "● " } else { "" },
                        window.name,
                        window.pane_count,
                        window.width,
                        window.height,
                    )),
            )
            .child(
                Button::new(format!("tmux-panes-{}", window.id))
                    .compact()
                    .when(self.compact_layout, |button| button.w_full())
                    .label(crate::locale::text("查看分屏", "View panes"))
                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                        view.load_tmux_panes(target_id.clone(), cx)
                    })),
            )
    }
}

/// 将标题和说明作为同一文案投影传入，避免参数增长同时保持两种语言的内容配对。
///
/// @author kongweiguang
struct TmuxDialogCopy {
    title: SharedString,
    message: SharedString,
}

/// Root 提供遮罩和窗口级焦点；内容壳只约束宽度并按短栏宽度折叠操作按钮。
/// 文案单独成组，以维持 helper 的参数上限而不把布局或行为选项混入调用方。
///
/// @author kongweiguang
fn tmux_dialog_frame(
    id: &'static str,
    copy: TmuxDialogCopy,
    feedback: Option<String>,
    warning: bool,
    theme: ThemeTokens,
    available_width: f32,
    actions: impl IntoElement,
) -> impl IntoElement {
    let TmuxDialogCopy { title, message } = copy;
    tmux_dialog_card(
        id,
        title,
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_size(theme.text_sm)
                    .text_color(if warning { theme.warning } else { theme.muted })
                    .child(message),
            )
            .when_some(feedback, |content, message| {
                content.child(tmux_dialog_feedback(message, theme))
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .when(available_width < 340., |row| row.flex_col().items_stretch())
                    .when(available_width >= 340., |row| {
                        row.items_center().justify_end()
                    })
                    .gap_2()
                    .child(actions),
            ),
        theme,
    )
}

/// 创建、改名和结束共用同一内容布局；Root 承载唯一表面，避免嵌套边框和圆角。
///
/// @author kongweiguang
fn tmux_dialog_card(
    id: &'static str,
    title: SharedString,
    content: impl IntoElement,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .id(id)
        .w_full()
        .max_w(px(448.))
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap_3()
        .px_5()
        .py_4()
        .text_color(theme.foreground)
        .child(
            div()
                .text_size(theme.text_heading)
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(content)
}

/// 固定右栏复用 main 的标题及描述层级与 ModalShell 分区；Root 仍提供唯一表面和视口约束。
///
/// @author kongweiguang
fn tmux_panel_dialog_shell(
    id: &'static str,
    title: SharedString,
    description: Option<SharedString>,
    content: impl IntoElement,
    actions: impl IntoElement,
    close: Button,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .id(id)
        .w_full()
        .max_w(px(576.))
        .min_w(px(0.))
        .flex()
        .flex_col()
        .text_color(theme.foreground)
        .child(
            div()
                .w_full()
                .flex_shrink_0()
                .flex()
                .items_start()
                .justify_between()
                .gap_4()
                .border_b_1()
                .border_color(theme.border)
                .px_5()
                .py_4()
                .child(
                    div()
                        .min_w(px(0.))
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(17.))
                                .line_height(px(24.))
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .child(title),
                        )
                        .when_some(description, |heading, description| {
                            heading.child(
                                div()
                                    .text_size(px(13.))
                                    .line_height(px(20.))
                                    .text_color(theme.muted)
                                    .child(description),
                            )
                        }),
                )
                .child(close),
        )
        .child(
            div()
                .w_full()
                .min_h(px(0.))
                .flex()
                .flex_col()
                .px_5()
                .py_4()
                .child(content),
        )
        .child(
            div()
                .w_full()
                .flex_shrink_0()
                .border_t_1()
                .border_color(theme.border)
                .px_5()
                .py_4()
                .child(actions),
        )
}

/// 用较弱字号标注字段，输入仍由既有 InputState 保持 IME、焦点和校验生命周期。
///
/// @author kongweiguang
fn tmux_dialog_field_label(label: SharedString, theme: ThemeTokens) -> impl IntoElement {
    div()
        .text_size(theme.text_xs)
        .text_color(theme.muted)
        .child(label)
}

/// 右栏表单标签沿用 main 的 12px 中号字重，完整页表单继续保留原有紧凑层级。
///
/// @author kongweiguang
fn tmux_panel_dialog_field_label(label: SharedString, theme: ThemeTokens) -> impl IntoElement {
    div()
        .text_size(px(12.))
        .line_height(px(16.))
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .text_color(theme.muted)
        .child(label)
}

/// 单行 Input 的高度由 Styled 外层控制；保留 InputState/Kit 焦点行为并复用 main 字段表面。
///
/// @author kongweiguang
fn tmux_panel_dialog_input(
    input: &gpui_kit::Entity<gpui_kit::component::input::InputState>,
    theme: ThemeTokens,
    system_font_family: SharedString,
) -> impl IntoElement {
    gpui_kit::Styled::h(Input::new(input).w_full(), px(36.))
        .px(px(12.))
        .rounded(theme.control_radius)
        .border_1()
        .border_color(theme.strong_border())
        .bg(theme.field_surface())
        .text_size(px(16.))
        .line_height(px(24.))
        .font_weight(gpui_kit::FontWeight::NORMAL)
        .font_family(system_font_family)
        .text_color(theme.foreground)
        .hover_style(gpui_kit::StyleRefinement::default().bg(theme.field_hover_surface()))
}

/// 成功后 Root 会关闭对话框；失败状态留在表单中，因此只把既有回执作为反馈呈现。
///
/// @author kongweiguang
fn tmux_dialog_feedback(message: String, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .text_size(theme.text_xs)
        .text_color(theme.error)
        .child(message)
}

/// 忙时仅请求取消并等待真实回执，已发出取消后锁住按钮；空闲时才关闭弹层意图。
///
/// @author kongweiguang
fn tmux_dialog_cancel_button(view: &OperationsView, cx: &mut Context<OperationsView>) -> Button {
    let cancellation_requested = view
        .mutation_cancellation
        .as_ref()
        .is_some_and(|cancellation| cancellation.is_cancelled());
    let label = if cancellation_requested {
        crate::locale::text("正在取消…", "Cancelling…")
    } else if view.busy {
        crate::locale::text("请求取消", "Request cancellation")
    } else {
        crate::locale::text("取消", "Cancel")
    };
    Button::new("tmux-dialog-cancel")
        .compact()
        .ghost()
        .label(label)
        .disabled(cancellation_requested)
        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| tmux_dialog_dismiss(view, cx)))
}

/// 标题栏关闭与底部取消共享忙碌策略，避免关闭动作绕过在途 tmux 操作的取消门禁。
///
/// @author kongweiguang
fn tmux_panel_dialog_close_button(
    view: &OperationsView,
    theme: ThemeTokens,
    cx: &mut Context<OperationsView>,
) -> Button {
    let cancellation_requested = view
        .mutation_cancellation
        .as_ref()
        .is_some_and(|cancellation| cancellation.is_cancelled());
    let label = crate::locale::text("关闭弹框", "Close dialog");
    Button::new("tmux-panel-dialog-close")
        .w(px(36.))
        .h(px(36.))
        .rounded(theme.control_radius)
        .flex_shrink_0()
        .ghost()
        .icon(IconName::X)
        .accessibility_label(label.clone())
        .tooltip(label)
        .disabled(cancellation_requested)
        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| tmux_dialog_dismiss(view, cx)))
}

/// 忙时请求一次协作取消，空闲时释放根确认意图；取消回执仍由原提交流程收尾。
///
/// @author kongweiguang
fn tmux_dialog_dismiss(view: &mut OperationsView, cx: &mut Context<OperationsView>) {
    if view.busy {
        view.cancel_operation_mutation(cx);
    } else {
        view.cancel_root_confirmation(cx);
    }
}

/// 失败或未知回执后允许用户重新读取原目标；按钮复用既有刷新动作且忙碌时不重复排队。
///
/// @author kongweiguang
fn tmux_dialog_refresh_button(busy: bool, cx: &mut Context<OperationsView>) -> Button {
    let label = crate::locale::text("重新核对会话状态", "Recheck session status");
    Button::new("tmux-dialog-refresh")
        .compact()
        .ghost()
        .icon(IconName::RefreshCw)
        .accessibility_label(label.clone())
        .tooltip(label)
        .disabled(busy)
        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| view.load_tmux(cx)))
}

/// 弹框按钮匹配 main 的控件高度与圆角；窄栏仍换成纵向布局并保持取消在前。
///
/// @author kongweiguang
fn tmux_panel_dialog_actions(
    available_width: f32,
    theme: ThemeTokens,
    cancel: Button,
    submit: Button,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_wrap()
        .when(available_width < 340., |row| row.flex_col().items_stretch())
        .when(available_width >= 340., |row| {
            row.items_center().justify_end()
        })
        .gap_2()
        .child(cancel.h(px(32.)).rounded(theme.control_radius))
        .child(submit.h(px(32.)).rounded(theme.control_radius))
}

/// 输入实体暂不可用时仍保留关闭路径，并说明可通过重新打开恢复。
///
/// @author kongweiguang
fn tmux_panel_input_unavailable_dialog(
    id: &'static str,
    title: SharedString,
    view: &OperationsView,
    theme: ThemeTokens,
    cx: &mut Context<OperationsView>,
) -> impl IntoElement {
    let content = div().w_full().flex().flex_col().gap_3().child(
        div()
            .text_size(theme.text_sm)
            .text_color(theme.muted)
            .child(crate::locale::text(
                "输入状态尚未初始化。关闭后重新打开即可恢复。",
                "The input is unavailable. Close and reopen this dialog to retry.",
            )),
    );
    let actions = div().w_full().flex().justify_end().child(
        tmux_dialog_cancel_button(view, cx)
            .h(px(32.))
            .rounded(theme.control_radius),
    );
    tmux_panel_dialog_shell(
        id,
        title,
        None,
        content,
        actions,
        tmux_panel_dialog_close_button(view, theme, cx),
        theme,
    )
}

/// 空会话名回退显示稳定 ID，使行标题和结束确认都指向同一 tmux 对象。
///
/// @author kongweiguang
fn tmux_session_name(session: &TmuxSession) -> String {
    if session.name.is_empty() {
        session.id.clone()
    } else {
        session.name.clone()
    }
}

/// 沿用 main 的当前会话、已连接、未连接优先级，并以活动时间和名称稳定排序。
///
/// @author kongweiguang
pub(super) fn sort_tmux_panel_sessions(
    mut sessions: Vec<TmuxSession>,
    current_session_id: Option<&str>,
    current_session_name: Option<&str>,
    target_ref: &str,
) -> Vec<TmuxSession> {
    sessions.sort_by(|left, right| {
        tmux_panel_session_priority(right, current_session_id, current_session_name, target_ref)
            .cmp(&tmux_panel_session_priority(
                left,
                current_session_id,
                current_session_name,
                target_ref,
            ))
            .then_with(|| {
                right
                    .activity_at
                    .unwrap_or_default()
                    .cmp(&left.activity_at.unwrap_or_default())
            })
            .then_with(|| left.name.cmp(&right.name))
    });
    sessions
}

/// ID 优先；名称回退仍要求 target_ref 相同，防止跨目标的同名会话获得当前优先级。
///
/// @author kongweiguang
fn tmux_panel_session_priority(
    session: &TmuxSession,
    current_session_id: Option<&str>,
    current_session_name: Option<&str>,
    target_ref: &str,
) -> u8 {
    if tmux_panel_session_matches_current(
        session,
        current_session_id,
        current_session_name,
        target_ref,
    ) {
        2
    } else if session.attached {
        1
    } else {
        0
    }
}

/// 当前标记只允许匹配同一 target scope，名称回退限于持久绑定中的非空 session name。
///
/// @author kongweiguang
pub(super) fn tmux_panel_session_matches_current(
    session: &TmuxSession,
    current_session_id: Option<&str>,
    current_session_name: Option<&str>,
    target_ref: &str,
) -> bool {
    session.target_ref == target_ref
        && (current_session_id == Some(session.id.as_str())
            || current_session_name
                .is_some_and(|name| !name.trim().is_empty() && name == session.name))
}

/// 按 main 的目录或目标名与本地时间戳生成默认会话名，并在同一目标已存在时找首个可用后缀。
///
/// @author kongweiguang
pub(in crate::features::operations) fn default_tmux_session_name(
    cwd: Option<&str>,
    target_label: &str,
    timestamp: &str,
    existing_names: &[String],
) -> String {
    let directory_name = cwd
        .map(|cwd| {
            let normalized = cwd.trim().replace('\\', "/");
            normalized
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .unwrap_or_default();
    let source = if directory_name.trim().is_empty() {
        target_label.trim()
    } else {
        directory_name.trim()
    };
    let stem = sanitize_tmux_session_name_component(source);
    let base = format!("{stem}-{timestamp}");
    if !existing_names.iter().any(|name| name == &base) {
        return base;
    }

    let mut suffix = 2usize;
    loop {
        let candidate = format!("{base}-{suffix}");
        if !existing_names.iter().any(|name| name == &candidate) {
            return candidate;
        }
        let Some(next_suffix) = suffix.checked_add(1) else {
            return base;
        };
        suffix = next_suffix;
    }
}

/// 将目录或目标标签规整到 tmux session 名称允许的 ASCII 字符集，和 main 的 sanitizer 保持一致。
///
/// @author kongweiguang
fn sanitize_tmux_session_name_component(value: &str) -> String {
    let mut sanitized = String::new();
    let mut previous_was_invalid = false;
    for character in value.chars() {
        let character = if matches!(character, '.' | ':') {
            '_'
        } else {
            character
        };
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
            sanitized.push(character);
            previous_was_invalid = false;
        } else if !previous_was_invalid {
            sanitized.push('-');
            previous_was_invalid = true;
        }
    }
    let stem = sanitized
        .trim_matches('-')
        .chars()
        .take(32)
        .collect::<String>();
    if stem.is_empty() {
        "kerminal".to_owned()
    } else {
        stem
    }
}

/// 退出按钮按意图查找固定 Ctrl-B+d 项，避免速查列表重排后退出按钮发出其它按键。
///
/// @author kongweiguang
pub(super) fn tmux_detach_shortcut() -> Option<super::super::quickref::TmuxQuickrefItem> {
    tmux_shortcuts()
        .iter()
        .copied()
        .find(|item| item.display == "Ctrl-b d" && item.payload == "\u{2}d")
}

/// 状态标签与颜色使用相同优先级：当前终端优先于其它已连接客户端。
///
/// @author kongweiguang
pub(super) fn tmux_tool_rail_session_status(
    current: bool,
    attached: bool,
) -> (&'static str, &'static str) {
    if current {
        ("当前终端", "Current terminal")
    } else if attached {
        ("已连接", "Attached")
    } else {
        ("未连接", "Detached")
    }
}

/// 有实际提示时保留反馈；就绪或无错误失败时省略空节点，避免 flex 布局增加不可见间距。
///
/// @author kongweiguang
pub(super) fn tmux_tool_rail_feedback_visible(phase: SectionPhase, has_error: bool) -> bool {
    has_error
        || matches!(
            phase,
            SectionPhase::NotLoaded | SectionPhase::Loading | SectionPhase::Cancelled
        )
}

/// 速查复制使用人类可读的显示形式；发送仍使用可能包含 tmux 前缀控制字节的 payload。
///
/// @author kongweiguang
pub(super) fn tmux_quickref_copy_text(
    item: super::super::quickref::TmuxQuickrefItem,
) -> &'static str {
    item.display
}

/// 标题副行优先给出 tmux 版本，缺少版本时回退到可恢复的能力状态。
///
/// @author kongweiguang
pub(super) fn tmux_tool_rail_header_status(
    phase: SectionPhase,
    capability: Option<&TmuxCapability>,
    target_available: bool,
) -> String {
    if !target_available {
        return localized_text("目标不可用", "Target unavailable");
    }
    match phase {
        SectionPhase::NotLoaded => localized_text("等待读取", "Waiting to load"),
        SectionPhase::Loading => localized_text("正在检测 tmux…", "Checking tmux…"),
        SectionPhase::Ready => match capability {
            Some(capability) if capability.available => capability
                .version
                .clone()
                .unwrap_or_else(|| localized_text("tmux 可用", "tmux available")),
            Some(capability) => capability.reason.clone().unwrap_or_else(|| {
                localized_text("目标未安装 tmux", "tmux is unavailable on this target")
            }),
            None => localized_text("状态未知", "Status unknown"),
        },
        SectionPhase::Failed => localized_text("读取失败，可重试", "Read failed; retry available"),
        SectionPhase::Cancelled => {
            localized_text("读取已取消，可重试", "Read cancelled; retry available")
        }
    }
}

/// 右栏主机别名承担目标身份，SSH 协议由工具语境表达；缺失或空白别名回退到 HostId。
///
/// @author kongweiguang
pub(super) fn tmux_ssh_host_display_label(alias: Option<&str>, host_id: &str) -> String {
    alias
        .filter(|alias| !alias.trim().is_empty())
        .unwrap_or(host_id)
        .to_owned()
}

/// 保留 ID、路径和 target_ref 三项 main 详情；紧凑栏逐项换行，长值不会撑宽面板。
///
/// @author kongweiguang
fn tmux_tool_rail_session_details(
    session: &TmuxSession,
    theme: ThemeTokens,
    mono_font_family: SharedString,
) -> impl IntoElement {
    div()
        .w_full()
        .grid()
        .grid_cols(2)
        .gap_2()
        .mt_2()
        .px_3()
        .py_2()
        .rounded_xl()
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .child(tmux_tool_rail_session_detail(
            crate::locale::text("会话 ID", "Session ID"),
            session.id.clone(),
            theme,
            mono_font_family.clone(),
        ))
        .child(tmux_tool_rail_session_detail(
            crate::locale::text("当前目录", "Current directory"),
            session.current_path.as_deref().unwrap_or("~").to_owned(),
            theme,
            mono_font_family.clone(),
        ))
        .child(tmux_tool_rail_session_detail(
            crate::locale::text("目标", "Target"),
            session.target_ref.clone(),
            theme,
            mono_font_family,
        ))
}

/// 让详情字段共享标签和等宽值排布，长目录按可见宽度自然换行。
///
/// @author kongweiguang
fn tmux_tool_rail_session_detail(
    label: SharedString,
    value: String,
    theme: ThemeTokens,
    mono_font_family: SharedString,
) -> impl IntoElement {
    div()
        .min_w_0()
        .w_full()
        .flex()
        .flex_col()
        .gap_0p5()
        .child(
            div()
                .text_size(px(11.))
                .text_color(theme.muted)
                .child(label),
        )
        .child(
            div()
                .min_w_0()
                .text_size(px(11.))
                .whitespace_normal()
                .font_family(mono_font_family)
                .font_weight(gpui_kit::FontWeight::MEDIUM)
                .child(value),
        )
}

/// tmux 标题点的颜色语义只表达最近一次读取状态，不把缺失结果画成在线。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TmuxHeaderTone {
    Ready,
    Loading,
    Error,
    Neutral,
}

/// 将最近一次查询阶段投影为简短状态文案，版本号仍作为独立字段展示。
///
/// @author kongweiguang
pub(super) fn tmux_header_status(
    phase: SectionPhase,
    capability: Option<&TmuxCapability>,
) -> String {
    match phase {
        SectionPhase::NotLoaded => localized_text("等待读取", "Waiting to load"),
        SectionPhase::Loading => localized_text("正在读取", "Loading"),
        SectionPhase::Ready => match capability {
            Some(capability) if capability.available => {
                localized_text("tmux 可用", "tmux available")
            }
            Some(capability) => capability.reason.clone().unwrap_or_else(|| {
                localized_text("目标未安装 tmux", "tmux is unavailable on this target")
            }),
            None => localized_text("状态未知", "Status unknown"),
        },
        SectionPhase::Failed => localized_text("读取失败，可重试", "Read failed; retry available"),
        SectionPhase::Cancelled => {
            localized_text("读取已取消，可重试", "Read cancelled; retry available")
        }
    }
}

/// 最近一次读取成功且 tmux 探测可用时才显示在线色，其它阶段使用对应语义色。
///
/// @author kongweiguang
pub(super) fn tmux_header_tone(
    phase: SectionPhase,
    capability: Option<&TmuxCapability>,
) -> TmuxHeaderTone {
    match phase {
        SectionPhase::Loading => TmuxHeaderTone::Loading,
        SectionPhase::Failed => TmuxHeaderTone::Error,
        SectionPhase::Ready if capability.is_some_and(|capability| capability.available) => {
            TmuxHeaderTone::Ready
        }
        SectionPhase::Ready if capability.is_some() => TmuxHeaderTone::Error,
        SectionPhase::NotLoaded | SectionPhase::Cancelled | SectionPhase::Ready => {
            TmuxHeaderTone::Neutral
        }
    }
}

/// 空目标、无 tmux 和零会话共用克制卡片，同时保留各自可执行的说明。
///
/// @author kongweiguang
fn tmux_empty_state(
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(
            div()
                .text_size(theme.text_body)
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(title.into()),
        )
        .child(
            div()
                .text_size(theme.text_sm)
                .text_color(theme.muted)
                .child(detail.into()),
        )
}
