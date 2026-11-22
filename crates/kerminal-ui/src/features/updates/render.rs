//! 更新面板的 GPUI Kit 呈现层；所有颜色取自工作台主题令牌。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement, Render, Window,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::updates::UpdateCandidate;

use super::{
    UpdatePanelState, UpdatesPanel, about::APP_VERSION, about::LICENSE_SPDX, about::LICENSE_TEXT,
    about::repository_url, can_check_for_updates, can_exit_after_install_failure, format_progress,
    install_result_summary, localized,
};
use crate::theme::ThemeTokens;

impl Render for UpdatesPanel {
    /// 使用 Kit 按钮和语义令牌表达当前更新操作，不把未配置 feed 绘成“没有更新”。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        let busy = self.is_busy();
        let host_terminal = self.is_host_terminal();
        let checking = matches!(&self.state, UpdatePanelState::Checking);
        let downloading = matches!(&self.state, UpdatePanelState::Downloading(_));
        let candidate = match &self.state {
            UpdatePanelState::Available(candidate) | UpdatePanelState::Downloading(candidate) => {
                Some(candidate)
            }
            _ => None,
        };
        let downloaded = match &self.state {
            UpdatePanelState::ReadyToInstall(update)
            | UpdatePanelState::AwaitingInstallGuard(update)
            | UpdatePanelState::Installing(update)
            | UpdatePanelState::Discarding(update) => Some(update),
            UpdatePanelState::StoppedUpdateFailed { update, .. } => Some(update),
            _ => None,
        };
        let status = self.status.clone();
        let notification_error = self.notification_error.clone();
        let progress = self.progress;
        let status_color = if self.status_is_error {
            theme.error
        } else {
            theme.muted
        };
        let unavailable = self.service.as_ref().is_none_or(|service| {
            service.availability() != kerminal_application::updates::UpdateAvailability::Configured
        });

        let mut actions = div().flex().items_center().gap_2();
        actions = actions.child(
            Button::new("updates-check")
                .compact()
                .primary()
                .label(if checking {
                    localized("检查中…", "Checking…")
                } else {
                    localized("检查更新", "Check for updates")
                })
                .disabled(
                    unavailable || busy || host_terminal || !can_check_for_updates(&self.state),
                )
                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| panel.check_for_updates(cx))),
        );
        if checking || downloading {
            actions = actions.child(
                Button::new("updates-cancel")
                    .compact()
                    .label(if self.cancel_requested {
                        localized("正在取消…", "Cancelling…")
                    } else {
                        localized("取消", "Cancel")
                    })
                    .disabled(self.cancel_requested)
                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                        panel.cancel_active_operation(cx)
                    })),
            );
        }
        if matches!(&self.state, UpdatePanelState::Available(_)) {
            actions = actions.child(
                Button::new("updates-download")
                    .compact()
                    .primary()
                    .label(localized("下载并验证", "Download and verify"))
                    .disabled(busy)
                    .on_click(
                        cx.listener(|panel, _: &ClickEvent, _, cx| panel.download_update(cx)),
                    ),
            );
        }
        if let UpdatePanelState::ReadyToInstall(_) = &self.state {
            actions = actions
                .child(
                    Button::new("updates-install")
                        .compact()
                        .primary()
                        .label(localized("安装并重启", "Install and restart"))
                        .on_click(
                            cx.listener(|panel, _: &ClickEvent, _, cx| panel.request_install(cx)),
                        ),
                )
                .child(
                    Button::new("updates-discard")
                        .compact()
                        .label(localized("丢弃", "Discard"))
                        .on_click(
                            cx.listener(|panel, _: &ClickEvent, _, cx| panel.discard_download(cx)),
                        ),
                );
        }
        if let UpdatePanelState::StoppedUpdateFailed { can_retry, .. } = &self.state
            && *can_retry
        {
            actions = actions.child(
                Button::new("updates-retry-install")
                    .compact()
                    .primary()
                    .label(localized("安全重试安装", "Retry installation"))
                    .on_click(
                        cx.listener(|panel, _: &ClickEvent, _, cx| panel.retry_host_install(cx)),
                    ),
            );
        }
        if can_exit_after_install_failure(&self.state) {
            actions = actions.child(
                Button::new("updates-exit-after-failure")
                    .compact()
                    .label(localized("退出应用", "Exit app"))
                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                        panel.exit_after_update_failure(cx)
                    })),
            );
        }

        let mut content = div()
            .id("updates-panel")
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .overflow_y_scroll()
            .p_4()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(18.))
                            .font_weight(gpui_kit::FontWeight::BOLD)
                            .child(localized("软件更新", "Software updates")),
                    )
                    .child(
                        div()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(localized(
                                "手动检查 · 签名验证后安装",
                                "Manual checks · signature-verified installs",
                            )),
                    ),
            )
            .child(actions);

        if unavailable {
            if let Some(status) = status.clone() {
                content = content.child(message_card(
                    localized("更新暂不可用", "Updates unavailable"),
                    status,
                    theme.error,
                    theme,
                ));
            }
        } else if let Some(status) = status {
            if host_terminal {
                let (title, detail) = match &self.state {
                    UpdatePanelState::StoppedUpdateFailed {
                        update, can_retry, ..
                    } => (
                        if *can_retry {
                            localized(
                                "安装未完成，可安全重试",
                                "Installation did not complete; safe retry is available",
                            )
                        } else {
                            localized("安装结果未知", "Installation result is unknown")
                        },
                        format!(
                            "{} {}。{status}",
                            localized("版本", "Version"),
                            update.version
                        ),
                    ),
                    UpdatePanelState::RestartPending(update) => (
                        localized("正在重新启动", "Restarting Kerminal"),
                        format!(
                            "{} {}。{status}",
                            localized("版本", "Version"),
                            update.version
                        ),
                    ),
                    UpdatePanelState::RestartFailed(update) => (
                        localized(
                            "更新已安装，但新进程启动失败",
                            "The update is installed, but the new process failed to start",
                        ),
                        format!(
                            "{} {}。{status}",
                            localized("版本", "Version"),
                            update.version
                        ),
                    ),
                    UpdatePanelState::HostRecoveryFailed {
                        update,
                        original_result,
                        safe_message,
                    } => (
                        localized(
                            "更新后单实例监听器未恢复",
                            "The single-instance listener was not restored after update",
                        ),
                        format!(
                            "{} {}。{} {}。{}",
                            localized("版本", "Version"),
                            update.version,
                            install_result_summary(original_result),
                            localized("监听器恢复", "Listener recovery"),
                            safe_message,
                        ),
                    ),
                    _ => (localized("更新状态", "Update status"), status),
                };
                content = content.child(message_card(title, detail, status_color, theme));
            } else {
                content = content.child(
                    div()
                        .w_full()
                        .text_size(px(12.))
                        .text_color(status_color)
                        .child(status),
                );
            }
        }

        if downloading {
            content = content.child(message_card(
                localized("下载进度", "Download progress"),
                format_progress(progress),
                theme.accent,
                theme,
            ));
        }
        if let Some(error) = notification_error {
            content = content.child(message_card(
                localized("更新提醒未能发送", "Could not send the update notification"),
                error,
                theme.error,
                theme,
            ));
        }
        if let Some(candidate) = candidate {
            content = content.child(candidate_card(candidate, theme));
        }
        if let Some(update) = downloaded {
            content = content.child(message_card(
                match &self.state {
                    UpdatePanelState::ReadyToInstall(_) => {
                        localized("已准备安装", "Ready to install")
                    }
                    UpdatePanelState::AwaitingInstallGuard(_) => {
                        localized("等待退出保护", "Waiting for exit checks")
                    }
                    UpdatePanelState::Installing(_) => localized("正在安装", "Installing"),
                    UpdatePanelState::Discarding(_) => localized("正在丢弃", "Discarding"),
                    UpdatePanelState::StoppedUpdateFailed { .. } => {
                        localized("安装已停止", "Installation stopped")
                    }
                    _ => localized("更新包", "Update package"),
                },
                format!("{} {}", localized("版本", "Version"), update.version),
                theme.accent,
                theme,
            ));
        }
        content = content.child(about_card(self.about_license_expanded, theme, cx));
        content
    }
}

/// 在既有更新面板末尾呈现运行版本、许可证与仓库入口，候选版本仍由上方更新状态显示。
///
/// @author kongweiguang
fn about_card(
    license_expanded: bool,
    theme: ThemeTokens,
    cx: &mut Context<UpdatesPanel>,
) -> impl IntoElement {
    let mut actions = div().flex().flex_wrap().items_center().gap_2();
    if let Some(repository) = repository_url() {
        let repository = repository.to_owned();
        actions = actions.child(
            Button::new("updates-about-repository")
                .compact()
                .ghost()
                .label(localized("打开项目仓库", "Open project repository"))
                .accessibility_label(localized(
                    "在默认浏览器打开 Kerminal 项目仓库",
                    "Open the Kerminal project repository in the default browser",
                ))
                .tooltip(localized(
                    "打开 Kerminal GitHub 仓库",
                    "Open the Kerminal GitHub repository",
                ))
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    cx.open_url(&repository);
                })),
        );
    } else {
        actions = actions.child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(localized(
                    "项目仓库链接不可用。",
                    "The project repository link is unavailable.",
                )),
        );
    }
    actions = actions.child(
        Button::new("updates-about-license")
            .compact()
            .label(if license_expanded {
                localized("收起许可证正文", "Hide license text")
            } else {
                localized("查看许可证正文", "View license text")
            })
            .accessibility_label(if license_expanded {
                localized("收起 GNU GPL 许可证正文", "Hide the GNU GPL license text")
            } else {
                localized("展开 GNU GPL 许可证正文", "Show the GNU GPL license text")
            })
            .tooltip(localized(
                "在此面板内展开仓库许可证全文",
                "Expand the full repository license in this panel",
            ))
            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                panel.toggle_about_license(cx);
            })),
    );

    let mut card = div()
        .id("updates-about-card")
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .p_4()
        .rounded_md()
        .bg(theme.panel)
        .child(
            div()
                .text_size(px(15.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(localized("关于 Kerminal", "About Kerminal")),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(theme.text_sm)
                        .child(format!("{} {APP_VERSION}", localized("当前版本：", "Current version:"))),
                )
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(localized(
                            "这是当前运行的应用版本；若有候选更新，会在上方单独显示。",
                            "This is the running application version. Any update candidate is shown separately above.",
                        )),
                ),
        )
        .child(
            div()
                .text_size(theme.text_sm)
                .child(format!("{} {LICENSE_SPDX}", localized("许可证：", "License:"))),
        )
        .child(actions);

    if license_expanded {
        card = card.child(
            div()
                .id("updates-about-license-text")
                .w_full()
                .max_h(px(360.))
                .overflow_y_scroll()
                .p_3()
                .rounded_md()
                .bg(theme.background)
                .text_size(theme.text_xs)
                .text_color(theme.foreground)
                .child(LICENSE_TEXT),
        );
    }
    card
}

/// 以独立卡片呈现候选版本、日期和发布说明，长说明允许在面板内滚动查看。
fn candidate_card(candidate: &UpdateCandidate, theme: ThemeTokens) -> impl IntoElement {
    let published_at = candidate
        .published_at
        .as_deref()
        .map(|date| format!("{} {date}", localized("发布时间：", "Published:")))
        .unwrap_or_else(|| localized("已发现新版本", "A new version is available"));
    let notes = candidate
        .notes
        .clone()
        .filter(|notes| !notes.trim().is_empty())
        .map(|notes| {
            div()
                .mt_2()
                .text_size(px(12.))
                .text_color(theme.foreground)
                .child(notes)
        });
    let mut card = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .p_4()
        .rounded_md()
        .bg(theme.panel)
        .child(
            div()
                .text_size(px(15.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(format!("Kerminal {}", candidate.version)),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(theme.muted)
                .child(published_at),
        );
    if let Some(notes) = notes {
        card = card.child(notes);
    }
    card
}

/// 复用同一提示卡片显示系统状态或进度，不引入额外的只读组件依赖。
fn message_card(
    title: String,
    detail: String,
    title_color: gpui_kit::Hsla,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .p_3()
        .rounded_md()
        .bg(theme.panel)
        .child(
            div()
                .text_size(px(12.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(title_color)
                .child(title),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme.foreground)
                .child(detail),
        )
}
