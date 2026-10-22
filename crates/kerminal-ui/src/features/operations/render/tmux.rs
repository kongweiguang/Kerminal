//! tmux 专属绘制与交互入口；独立模块避免与其它运维工具争用公共渲染。
//!
//! @author kongweiguang

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
        Disableable,
        button::{Button, ButtonVariants},
        input::Input,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::operations::TmuxCapability;
use kerminal_domain::operations::{TmuxSession, TmuxWindow};

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

    /// 将目标、能力状态和会话列表放进同一视觉层级；创建、改名与结束确认由 Root 唯一模态宿主承载。
    ///
    /// @author kongweiguang
    pub(super) fn render_tmux(
        &self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
        let detach_item = tmux_shortcuts().first().copied();
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

/// 创建、改名和结束共用同一紧凑卡片，避免业务控件各自模拟遮罩或管理焦点。
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
        .p_4()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.materials.chrome)
        .text_color(theme.foreground)
        .child(
            div()
                .text_size(theme.text_heading)
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(content)
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
        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
            if view.busy {
                view.cancel_operation_mutation(cx);
            } else {
                view.cancel_root_confirmation(cx);
            }
        }))
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
