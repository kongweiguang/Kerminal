//! SOCKS 代理面板的 GPUI Kit 视图；绘制只依赖只读状态与 typed actions。
//!
//! @author kongweiguang

use crate::theme::ThemeTokens;
use gpui_kit::{
    ClickEvent, Context, IntoElement, Render, Window,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
        scroll::ScrollableElement as _,
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::identity::HostId;

use super::{
    ProxyApplicationPanel, ProxyCurrentApplication, ProxyCurrentAvailability,
    ProxyFutureProjection, ProxyHistoryResult, ProxyOperationStatus, ProxyReviewScript,
    eligible_socks_forwards, unavailable_reason_text,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CurrentRecoveryActions {
    restore: bool,
    reconcile: bool,
}

/// 恢复入口依赖唯一原记录和可信提示符，不依赖当前活动转发列表是否为空。
///
/// @author kongweiguang
fn current_recovery_actions(
    application: Option<ProxyCurrentApplication>,
    has_recovery_record: bool,
    availability: Option<ProxyCurrentAvailability>,
    busy: bool,
) -> CurrentRecoveryActions {
    let ready = availability == Some(ProxyCurrentAvailability::Ready);
    CurrentRecoveryActions {
        restore: !busy
            && has_recovery_record
            && ready
            && application == Some(ProxyCurrentApplication::Applied),
        reconcile: !busy
            && has_recovery_record
            && ready
            && application == Some(ProxyCurrentApplication::OutcomeUnknown),
    }
}

/// 仅在没有活动目标、应用状态或恢复事项时隐藏重复的 SOCKS 空提示。
///
/// @author kongweiguang
fn should_suppress_compact_empty_hint(
    has_active_forwards: bool,
    current_host: Option<&HostId>,
    current_application: Option<super::ProxyCurrentApplication>,
    future: &ProxyFutureProjection,
    has_operation_or_recovery_state: bool,
) -> bool {
    let current_requires_recovery = matches!(
        current_application,
        Some(
            super::ProxyCurrentApplication::Applied
                | super::ProxyCurrentApplication::OutcomeUnknown
        )
    );
    let future_is_known_clear = match future {
        ProxyFutureProjection::Unavailable { host_id: None } => current_host.is_none(),
        ProxyFutureProjection::Ready {
            host_id,
            forward_id: None,
            ..
        } => current_host == Some(host_id),
        _ => false,
    };

    !has_active_forwards
        && !current_requires_recovery
        && future_is_known_clear
        && !has_operation_or_recovery_state
}

impl Render for ProxyApplicationPanel {
    /// 干净空态不重复展示 SOCKS 提示；任何运行、回执、历史或恢复信息都走原详细视图。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        let forwards = eligible_socks_forwards(&self.projection);
        let selected_id = self.selected_forward_id.clone();
        let busy = self.pending.is_some();
        let selected = self.selected_forward().cloned();
        let current_reason = selected.as_ref().and_then(|_| self.current_binding().err());
        let current_application = self
            .projection
            .current_session
            .as_ref()
            .map(|session| session.application);
        let current_availability = self
            .projection
            .current_session
            .as_ref()
            .map(|session| session.availability);
        let recovery_actions = current_recovery_actions(
            current_application,
            self.recovery_forward_id.is_some(),
            current_availability,
            busy,
        );
        let current_host = self.projection.host_id.as_ref();
        if should_suppress_compact_empty_hint(
            !forwards.is_empty(),
            current_host,
            current_application,
            &self.projection.future,
            busy || self.last_status.is_some()
                || self.last_history_result.is_some()
                || self.review_script.is_some()
                || self.recovery_forward_id.is_some(),
        ) {
            return div().id("operations-proxy-empty-suppressed").w_full();
        }
        let future_enabled = selected.as_ref().is_some_and(|forward| {
            matches!(&self.projection.future, ProxyFutureProjection::Ready {
                host_id,
                forward_id: Some(forward_id),
                ..
            } if current_host == Some(host_id)
                && host_id == &forward.forward.config.host_id
                && forward_id == &forward.forward.id)
        });
        let future_unknown = matches!(
            &self.projection.future,
            ProxyFutureProjection::OutcomeUnknown { host_id } if current_host == Some(host_id)
        );
        let future_loading = matches!(
            &self.projection.future,
            ProxyFutureProjection::Loading { host_id } if current_host == Some(host_id)
        );
        let future_available = matches!(
            &self.projection.future,
            ProxyFutureProjection::Ready { host_id, .. } if current_host == Some(host_id)
        );
        let future_unavailable_without_host = current_host.is_none()
            && matches!(
                &self.projection.future,
                ProxyFutureProjection::Unavailable { host_id: None }
            );
        let mut root = div()
            .id("operations-proxy-application")
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_md()
            .bg(theme.panel)
            .text_color(theme.foreground)
            .child(
                div()
                    .text_size(px(14.))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(crate::locale::text("SOCKS 代理应用", "SOCKS proxy application")),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        "隧道需另行应用到指定终端。后续策略仅本次运行有效，停止转发或退出即清除；重新启用需选活动转发。不改用户配置。",
                        "Apply tunnels to explicit terminals. Future policy lasts for this run; stopping its forward or quitting clears it. Select an active forward to re-enable. User config stays unchanged.",
                    )),
            );

        if forwards.is_empty() {
            root = root.child(
                div()
                    .id("operations-proxy-empty")
                    .text_size(px(12.))
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        "当前没有可用的活动 SOCKS5 转发。添加并启动动态转发后即可应用代理。",
                        "No active SOCKS5 forward is available. Add and start a dynamic forward to apply a proxy.",
                    )),
            );
        } else {
            root = root.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .children(forwards.into_iter().map(|forward| {
                        let id = forward.forward.id.clone();
                        let is_selected = selected_id.as_deref() == Some(id.as_str());
                        let label =
                            format!("{} · {}", forward.forward.config.name, forward.actual_bind);
                        let mut button = Button::new(format!("proxy-forward-{id}"))
                            .compact()
                            .label(label)
                            .disabled(busy)
                            .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                                panel.select_forward(id.clone(), cx)
                            }));
                        if is_selected {
                            button = button.primary();
                        }
                        button
                    })),
            );
        }

        let selected_host_matches = selected.as_ref().is_some_and(|forward| {
            self.projection.host_id.as_ref() == Some(&forward.forward.config.host_id)
        });
        let current_enabled = selected.is_some()
            && selected_host_matches
            && current_reason.is_none()
            && current_application == Some(super::ProxyCurrentApplication::NotApplied)
            && !busy;
        let restore_enabled = recovery_actions.restore;
        let future_enabled_button =
            selected.is_some() && selected_host_matches && future_available && !busy;
        root = root.child(
            div()
                // 操作行必须采用卡片实际宽度并允许换行，避免窄栏把后续操作裁出可视区。
                .w_full()
                .min_w(px(0.))
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(
                    Button::new("proxy-apply-current")
                        .compact()
                        .primary()
                        .label(crate::locale::text(
                            "应用到当前终端",
                            "Apply to current terminal",
                        ))
                        .disabled(!current_enabled)
                        .on_click(
                            cx.listener(|panel, _: &ClickEvent, _, cx| panel.apply_to_current(cx)),
                        ),
                )
                .child(
                    Button::new("proxy-restore-current")
                        .compact()
                        .label(crate::locale::text(
                            "恢复当前终端",
                            "Restore current terminal",
                        ))
                        .disabled(!restore_enabled)
                        .on_click(
                            cx.listener(|panel, _: &ClickEvent, _, cx| panel.restore_current(cx)),
                        ),
                )
                .child(
                    Button::new("proxy-reconcile-current")
                        .compact()
                        .label(crate::locale::text("核对终端状态", "Check terminal state"))
                        .disabled(!recovery_actions.reconcile)
                        .on_click(
                            cx.listener(|panel, _: &ClickEvent, _, cx| panel.reconcile_current(cx)),
                        ),
                )
                .child(
                    Button::new("proxy-toggle-future")
                        .compact()
                        .label(if future_enabled {
                            crate::locale::text("停止后续应用", "Stop future application")
                        } else {
                            crate::locale::text("应用到后续终端", "Apply to future terminals")
                        })
                        .disabled(!future_enabled_button || future_unknown)
                        .on_click(
                            cx.listener(|panel, _: &ClickEvent, _, cx| panel.toggle_future(cx)),
                        ),
                )
                .child(
                    Button::new("proxy-review-profile-script")
                        .compact()
                        .label(crate::locale::text("查看配置脚本", "Review profile script"))
                        .disabled(selected.is_none() || busy || !self.profile_script_supported())
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.request_profile_script(cx)
                        })),
                )
                .when(busy, |row| {
                    let cancellable = self
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.cancellable);
                    if cancellable {
                        row.child(
                            Button::new("proxy-cancel-operation")
                                .compact()
                                .label(crate::locale::text("取消", "Cancel"))
                                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.request_cancel(cx)
                                })),
                        )
                    } else {
                        row
                    }
                }),
        );

        if let Some(reason) = current_reason {
            root = root.child(
                div()
                    .id("proxy-current-unavailable")
                    .text_size(px(11.))
                    .text_color(theme.muted)
                    .child(unavailable_reason_text(reason)),
            );
        }
        if future_unknown {
            root = root.child(
                div()
                    .id("proxy-future-unknown")
                    .text_size(px(11.))
                    .text_color(theme.error)
                    .child(crate::locale::text(
                        "未来终端策略结果未知；请先重新读取策略，不要重复提交。",
                        "The future-terminal policy result is unknown. Refresh it before retrying.",
                    )),
            );
        } else if !future_available {
            root = root.child(
                div()
                    .id("proxy-future-unavailable")
                    .text_size(px(11.))
                    .text_color(theme.muted)
                    .child(if future_unavailable_without_host {
                        crate::locale::text(
                            "请选择 SSH 主机并启动活动 SOCKS5 转发后，再设置后续终端。",
                            "Select an SSH host and start an active SOCKS5 forward to configure future terminals.",
                        )
                    } else if future_loading {
                        crate::locale::text(
                            "正在读取此主机的后续策略。",
                            "Loading the future policy for this host.",
                        )
                    } else {
                        crate::locale::text(
                            "未能读取后续终端的代理设置，请刷新后重试。",
                            "Couldn't read proxy settings for future terminals. Refresh and try again.",
                        )
                    }),
            );
        } else if selected.is_some() {
            root = root.child(
                div()
                    .id("proxy-future-scope")
                    .text_size(px(11.))
                    .text_color(theme.muted)
                    .child(if future_enabled {
                        crate::locale::text(
                            "仅同一主机之后新建、已就绪且无初始命令的 SSH 终端适用。",
                            "Applies only to later ready interactive SSH sessions for this host, with no initial command.",
                        )
                    } else {
                        crate::locale::text(
                            "后续应用默认关闭；启动隧道不会自动设置此项。",
                            "Future application is off by default; starting a tunnel does not enable it.",
                        )
                    }),
            );
        }

        if let Some(status) = self.render_status(theme) {
            root = root.child(status);
        }
        if let Some(history) = self.render_history_result(theme) {
            root = root.child(history);
        }
        if let Some(script) = self.review_script.as_ref() {
            root = root.child(self.render_script(script, theme, cx));
        }
        root
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/proxy_application_panel.rs"]
mod proxy_application_panel_tests;

impl ProxyApplicationPanel {
    /// 将最近结果锚定到请求时主机，并使用稳定状态码映射文案。
    fn render_status(&self, theme: ThemeTokens) -> Option<impl IntoElement> {
        let status = self.last_status.as_ref()?;
        let target =
            self.last_status_context
                .as_ref()
                .map_or_else(String::new, |(host, forward)| {
                    host.as_ref().map_or_else(
                        || forward.clone(),
                        |host| format!("{} · {}", host.as_str(), forward),
                    )
                });
        let (label, color) = match status {
            ProxyOperationStatus::Applying => {
                (crate::locale::text("正在应用…", "Applying…"), theme.accent)
            }
            ProxyOperationStatus::Applied => (
                crate::locale::text(
                    "代理已应用到明确目标。",
                    "Proxy applied to the explicit target.",
                ),
                theme.accent,
            ),
            ProxyOperationStatus::Restored => (
                crate::locale::text(
                    "已从明确终端恢复本次代理环境修改。",
                    "The proxy environment change was restored in the explicit terminal.",
                ),
                theme.muted,
            ),
            ProxyOperationStatus::Reconciled => (
                crate::locale::text("终端代理状态已核对。", "Terminal proxy state was checked."),
                theme.muted,
            ),
            ProxyOperationStatus::NotApplied(reason) => {
                (unavailable_reason_text(*reason), theme.muted)
            }
            ProxyOperationStatus::OutcomeUnknown => (
                crate::locale::text(
                    "代理结果未知；先核对原终端状态，不会自动重试。",
                    "Proxy result is unknown. Check the original terminal state; nothing is replayed automatically.",
                ),
                theme.error,
            ),
            ProxyOperationStatus::Failed(message) => (message.clone(), theme.error),
            ProxyOperationStatus::CancelRequested => (
                crate::locale::text(
                    "正在取消；等待用例确认最终结果。",
                    "Cancellation requested; waiting for the final result.",
                ),
                theme.muted,
            ),
            ProxyOperationStatus::Cancelled => (
                crate::locale::text("操作已取消。", "Operation cancelled."),
                theme.muted,
            ),
            ProxyOperationStatus::FutureEnabled => (
                crate::locale::text(
                    "已启用同主机后续 SSH 终端策略。",
                    "Future SSH terminals for this host are enabled.",
                ),
                theme.accent,
            ),
            ProxyOperationStatus::FutureDisabled => (
                crate::locale::text(
                    "已关闭同主机后续 SSH 终端策略。",
                    "Future SSH terminals for this host are disabled.",
                ),
                theme.muted,
            ),
            ProxyOperationStatus::ScriptReady => (
                crate::locale::text(
                    "脚本已生成，请审阅后手动决定是否使用。",
                    "Script is ready. Review it before choosing whether to use it.",
                ),
                theme.muted,
            ),
        };
        Some(
            div()
                .id("proxy-operation-status")
                .w_full()
                .p_2()
                .rounded_md()
                .bg(theme.sidebar)
                .text_size(px(11.))
                .text_color(color)
                .child(if target.is_empty() {
                    label
                } else {
                    crate::locale::text(
                        format!("{target} · {}", label),
                        format!("{target} · {}", label),
                    )
                }),
        )
    }

    /// 历史保存结果与终端命令结果分开展示，避免把历史故障冒充为代理应用失败。
    fn render_history_result(&self, theme: ThemeTokens) -> Option<impl IntoElement> {
        let result = self.last_history_result.as_ref()?;
        let (label, color) = match result {
            ProxyHistoryResult::Recorded => (
                crate::locale::text("命令历史已记录。", "Command history recorded."),
                theme.muted,
            ),
            ProxyHistoryResult::NotSubmitted => (
                crate::locale::text(
                    "此次操作没有提交命令历史。",
                    "No command history was submitted for this operation.",
                ),
                theme.muted,
            ),
            ProxyHistoryResult::Skipped(reason) => (
                crate::locale::text(
                    format!("命令历史未记录：{reason}"),
                    format!("Command history was skipped: {reason}"),
                ),
                theme.muted,
            ),
            ProxyHistoryResult::Failed(code) => (
                crate::locale::text(
                    format!("命令历史写入失败（{code}）；代理命令结果单独显示。"),
                    format!(
                        "Command history failed ({code}); proxy command result is shown separately."
                    ),
                ),
                theme.error,
            ),
            ProxyHistoryResult::Unavailable => (
                crate::locale::text(
                    "命令历史状态不可用；代理命令结果单独显示。",
                    "Command history status is unavailable; proxy command result is shown separately.",
                ),
                theme.muted,
            ),
        };
        Some(
            div()
                .id("proxy-command-history-result")
                .text_size(px(10.))
                .text_color(color)
                .child(label),
        )
    }

    /// 只把人工请求的脚本写入剪贴板，不调用配置仓储、不执行远端命令。
    fn render_script(
        &self,
        script: &ProxyReviewScript,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id("proxy-profile-script-review")
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded_md()
            .bg(theme.sidebar)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(12.)).child(crate::locale::text(
                        format!("配置脚本 · {:?}", script.shell_dialect),
                        format!("Profile script · {:?}", script.shell_dialect),
                    ))),
            )
            .child(self.render_script_action("apply", "应用环境脚本", "Apply environment script", &script.apply, theme, cx))
            .child(self.render_script_action("restore", "恢复环境脚本", "Restore environment script", &script.restore, theme, cx))
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        "内容仅供审阅和手动复制；应用不会编辑或覆盖远端 profile。",
                        "Review and copy manually. The application does not edit or overwrite the remote profile.",
                    )),
            )
    }

    /// 脚本操作仅将当前展示内容复制到系统剪贴板，用户自行决定是否执行或写入配置。
    fn render_script_action(
        &self,
        id: &'static str,
        title_zh: &'static str,
        title_en: &'static str,
        text: &str,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let script = text.to_owned();
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(11.))
                            .child(crate::locale::text(title_zh, title_en)),
                    )
                    .child(
                        Button::new(format!("proxy-copy-{id}-script"))
                            .compact()
                            .label(crate::locale::text("复制", "Copy"))
                            .disabled(script.is_empty())
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                    script.clone(),
                                ));
                            })),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .max_h(px(120.))
                    .overflow_y_scrollbar()
                    .p_2()
                    .rounded_sm()
                    .bg(theme.background)
                    .text_size(px(11.))
                    .child(text.to_owned()),
            )
    }
}
