//! 单次配置同步和 Vault key 编辑页面；I/O 与状态回执由 application 用例驱动。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PathBuilder, StatefulInteractiveElement, WeakEntity,
    assets::IconName,
    canvas,
    component::{
        Disableable, Icon, WindowExt as _,
        button::{Button, ButtonVariant, ButtonVariants},
        input::Textarea,
    },
    div, point,
    prelude::*,
    px,
};
use kerminal_application::configuration_sync::{
    ConfigurationSyncOutcome, ConfigurationSyncStatus, SyncRepositoryState,
};

use crate::theme::ThemeTokens;

use super::panel::{PreferencesPanel, RecoveryDirectoryStatus};

impl PreferencesPanel {
    /// 用真实仓库状态约束同步和恢复入口，避免缺上游或变基冲突被显示为远端同步成功。
    ///
    /// @author kongweiguang
    pub(super) fn render_configuration_sync(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let service_missing = self.configuration_sync.is_none();
        let key_service_missing = self.vault_keys.is_none();
        let status = self.sync_status.clone();
        let workspace = self.connectivity.workspace_info.clone();
        let sync_busy = self.connectivity.sync_running || self.sync_loading;
        let busy = sync_busy || self.connectivity.key_loading || self.connectivity.key_saving;
        let sync_available = self.sync_outcome_unknown
            || status.as_ref().is_some_and(|status| {
                status.git_available
                    && status.writable
                    && !status.rebase_in_progress
                    && status.conflicts.is_empty()
                    && matches!(
                        status.repository,
                        SyncRepositoryState::Ready | SyncRepositoryState::NoUpstream
                    )
            });
        let can_sync = !busy && sync_available && !service_missing;
        let rebase_in_progress = status
            .as_ref()
            .is_some_and(|status| status.rebase_in_progress);
        let conflicts = status
            .as_ref()
            .map_or_else(Vec::new, |status| status.conflicts.clone());
        let can_rebase = !busy
            && !self.sync_outcome_unknown
            && status
                .as_ref()
                .is_some_and(|status| status.writable && status.rebase_in_progress);
        let directory_pending = matches!(
            self.recovery_directory_status.as_ref(),
            Some(RecoveryDirectoryStatus::Requesting)
        );
        let directory_error = match self.recovery_directory_status.as_ref() {
            Some(RecoveryDirectoryStatus::Failed(message)) => Some(message.clone()),
            _ => None,
        };
        let sync_result = self.connectivity.sync_run_result.clone();
        let key_path = workspace.as_ref().map(|info| info.vault_key_path.as_str());
        let root_path = workspace.as_ref().map(|info| info.workspace_root.as_str());
        let key_editor_disabled = key_service_missing || self.connectivity.key_loading;
        let key_editor_height = self.connectivity.key_document_height;
        let panel_entity = cx.entity().downgrade();
        let key_resize_handle = div()
            .id("workspace-sync-key-resize-handle")
            .absolute()
            .right(px(2.))
            .bottom(px(2.))
            .w(px(14.))
            .h(px(14.))
            .flex()
            .items_center()
            .justify_center()
            .when(!key_editor_disabled, |handle| {
                handle
                    .child(key_resize_input_events(panel_entity.clone(), theme.border))
                    .cursor_row_resize()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|panel, event: &MouseDownEvent, _, cx| {
                            panel
                                .connectivity
                                .begin_key_document_resize(event.position.y.as_f32());
                            cx.notify();
                        }),
                    )
            });

        div()
            .id("preferences-configuration-sync")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.content_scroll)
            .flex()
            .flex_col()
            .gap_4()
            .child(
                sync_card(theme)
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_wrap()
                            .items_start()
                            .justify_between()
                            .gap_3()
                            .child(div().flex_1().min_w(px(0.)).flex().flex_col().gap_1()
                                .child(div().text_size(theme.text_sm).font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .child(crate::locale::text("同步", "Sync")))
                                .child(div().max_w(px(620.)).text_size(theme.text_xs).text_color(theme.muted)
                                    .child(crate::locale::text(
                                        "先提交本地配置，再拉取远端并推送待同步的提交。",
                                        "Commit local configuration, then pull and push pending commits.",
                                    ))))
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .justify_end()
                                    .gap_2()
                                    .child(Button::new("workspace-sync-open-directory")
                                        .compact()
                                        .outline()
                                        .icon(Icon::new(IconName::FolderOpen).size(px(14.)))
                                        .label(crate::locale::text("配置文件夹", "Config folder"))
                                        .disabled(self.configuration_directory.is_none() || directory_pending)
                                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                            panel.open_recovery_configuration_directory(cx);
                                        })))
                                    .child(Button::new("workspace-sync-refresh-status")
                                        .compact()
                                        .outline()
                                        .icon(Icon::new(IconName::Activity).size(px(14.)))
                                        .label(crate::locale::text("状态", "Status"))
                                        .disabled(busy || service_missing)
                                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                            panel.refresh_configuration_sync(cx);
                                        })))
                                    .child(
                                Button::new("workspace-sync-run")
                                    .compact()
                                    .primary()
                                    .icon(Icon::new(IconName::RefreshCw).size(px(16.)))
                                    .label(if busy {
                                        if sync_busy && self.sync_outcome_unknown {
                                            crate::locale::text("核实中…", "Verifying…")
                                        } else if sync_busy {
                                            crate::locale::text("同步中…", "Syncing…")
                                        } else {
                                            crate::locale::text("同步", "Sync")
                                        }
                                    } else if self.sync_outcome_unknown {
                                        crate::locale::text("刷新并核实", "Fetch and verify")
                                    } else {
                                        crate::locale::text("同步", "Sync")
                                    })
                                    .disabled(!can_sync)
                                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                        panel.run_workspace_sync(cx);
                                    })),
                                ),
                            ),
                    )
                    .child(div().flex().gap_3()
                        .child(div().min_w(px(0.)).flex_grow(1.).flex_basis(px(0.))
                            .child(sync_info_row(
                                IconName::GitBranch,
                                crate::locale::text("Git 状态", "Git status"),
                                status.as_ref().map_or_else(
                                    || if self.sync_loading { crate::locale::text("检查中", "Checking") } else { crate::locale::text("尚未读取", "Not checked") },
                                    repository_label,
                                ),
                                theme,
                            )))
                        .child(div().min_w(px(0.)).flex_grow(1.5).flex_basis(px(0.))
                            .child(sync_path_row(
                                IconName::FileKey,
                                "workspace-root-path",
                                crate::locale::text("工作区", "Workspace"),
                                root_path.unwrap_or(if service_missing { "同步服务不可用" } else { "正在读取路径…" }),
                                theme,
                            ))))
                    .when_some(directory_error, |card, error| card.child(sync_note(error, theme.error)))
                    .when_some(self.sync_result.as_ref().and_then(|result| result.commit_hash.clone()), |card, hash| {
                        card.child(sync_info_row(
                            IconName::GitCommitHorizontal,
                            crate::locale::text("最近提交", "Latest commit"),
                            hash,
                            theme,
                        ))
                    }),
            )
            .when(rebase_in_progress, |page| {
                let abort_entity = panel_entity.clone();
                page.child(
                    sync_card(theme)
                        .child(div().text_size(theme.text_sm).font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(crate::locale::text("变基需要处理", "Rebase needs attention")))
                        .child(sync_note(crate::locale::text(
                            "在配置目录中解决并暂存冲突，刷新状态后继续，或中止本次变基。",
                            "Resolve and stage conflicts in the configuration folder, then refresh and continue or abort the rebase.",
                        ), theme.muted))
                        .children(conflicts.iter().map(|path| {
                            div().w_full().min_w(px(0.)).text_size(theme.text_xs)
                                .font_family("monospace").text_color(theme.foreground)
                                .child(path.clone())
                        }))
                        .child(div().flex().flex_wrap().items_center().gap_2()
                            .child(Button::new("workspace-sync-rebase-continue")
                                .compact().outline()
                                .label(crate::locale::text("继续变基", "Continue rebase"))
                                .disabled(!can_rebase || !conflicts.is_empty())
                                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.continue_workspace_rebase(cx);
                                })))
                            .child(Button::new("workspace-sync-rebase-abort")
                                .compact().outline()
                                .label(crate::locale::text("中止变基", "Abort rebase"))
                                .disabled(!can_rebase)
                                .on_click(cx.listener(move |_, _: &ClickEvent, window, cx| {
                                    let entity = abort_entity.clone();
                                    window.open_alert_dialog(cx, move |alert, _, _| {
                                        let entity = entity.clone();
                                        alert.confirm()
                                            .title(crate::locale::text("中止变基？", "Abort rebase?"))
                                            .description(crate::locale::text(
                                                "未提交的冲突处理可能丢失。",
                                                "Uncommitted conflict resolutions may be lost.",
                                            ))
                                            .ok_text(crate::locale::text("中止变基", "Abort rebase"))
                                            .ok_variant(ButtonVariant::Danger)
                                            .cancel_text(crate::locale::text("取消", "Cancel"))
                                            .on_ok(move |_, _, app| {
                                                let _ = entity.update(app, |panel, cx| panel.abort_workspace_rebase(cx));
                                                true
                                            })
                                    });
                                }))))
                )
            })
            .child(
                sync_card(theme)
                    .child(div().flex().flex_wrap().items_start().justify_between().gap_3()
                            .child(div().flex_1().min_w(px(0.)).flex().flex_col().gap_1()
                            .child(div().flex().items_center().gap_2()
                                .child(Icon::new(IconName::FileKey).size(px(16.)).text_color(theme.accent))
                                .child(div().text_size(theme.text_sm).font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .child(crate::locale::text("密钥文件", "Key file"))))
                            .child(div().id("vault-key-path").w_full().min_w(px(0.)).truncate()
                                .text_size(theme.text_xs).font_family("monospace")
                                .text_color(theme.muted)
                                .child(key_path.unwrap_or(if key_service_missing { "Vault 密钥服务不可用" } else { "正在读取路径…" }).to_owned())))
                        .child(Button::new("workspace-sync-key-save")
                            .compact()
                            .icon(Icon::new(IconName::Save).size(px(16.)))
                            .label(if self.connectivity.key_saving {
                                crate::locale::text("保存中…", "Saving…")
                            } else {
                                crate::locale::text("保存", "Save")
                            })
                            .disabled(key_service_missing
                                || self.connectivity.key_loading
                                || self.connectivity.key_saving
                                || self.connectivity.sync_running
                                || self.sync_loading)
                            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                let workspace_sync_loading = panel.sync_loading;
                                panel.connectivity.save_key(
                                    panel.vault_keys.clone(),
                                    workspace_sync_loading,
                                    cx,
                                );
                            }))))
                    .child(
                        div()
                            .w_full()
                            .relative()
                            .child(
                                Textarea::new(&self.connectivity.key_document)
                                    .bg(theme.field_surface())
                                    .border_color(theme.border)
                                    .rounded(theme.radius_panel)
                                    .font_family("monospace")
                                    .text_size(theme.text_xs)
                                    .line_height(px(20.))
                                    .min_h(px(256.))
                                    .h(px(key_editor_height))
                                    .w_full()
                                    .disabled(key_editor_disabled)
                                    .aria_label(crate::locale::text(
                                        "密钥文件内容",
                                        "Key file content",
                                    )),
                            )
                            .child(key_resize_handle),
                    )
                    .child(div().text_size(theme.text_xs).text_color(theme.muted)
                        .child(crate::locale::text(
                            "保存时会校验 TOML 格式、密钥格式，并确认它能解密当前保险箱；校验失败不会写入文件。",
                            "Saving validates TOML and key formats, then confirms the key can decrypt the current vault. Validation failures do not write the file.",
                        ))),
            )
            .when(service_missing, |page| {
                page.child(sync_note(crate::locale::text(
                    "配置同步服务未由应用装配。",
                    "The configuration sync service is unavailable.",
                ), theme.error))
            })
            .when(self.sync_outcome_unknown, |page| {
                page.child(sync_note(crate::locale::text(
                    "上次同步结果仍需确认，读取状态完成前不会重复执行。",
                    "The previous sync result still needs verification. No retry will run until status is confirmed.",
                ), theme.warning))
            })
            .when_some(self.sync_error.clone(), |page, error| {
                page.child(sync_note(error, theme.error))
            })
            .when_some(sync_result, |page, result| {
                page.child(sync_run_result(&result.summary, result.outcome, theme))
            })
            .when(self.connectivity.key_loading, |page| {
                page.child(div().text_size(theme.text_xs).text_color(theme.muted)
                    .child(crate::locale::text("正在读取密钥文件…", "Reading key file…")))
            })
            .when_some(self.connectivity.key_error.clone(), |page, error| {
                page.child(sync_note(error, theme.error))
            })
            .when_some(self.connectivity.key_status.clone(), |page, message| {
                page.child(sync_note(message, theme.success))
            })
            .child(self.render_save_notice(theme))
    }
}

/// 在 textarea 右下角绘制原生 resize 抓手，并在 paint 阶段注册跨 hitbox 拖动事件。
///
/// GPUI Textarea 没有浏览器的 `resize-y`，因此用小型斜线保留相同位置与视觉语义；
/// 弱实体让窗口级事件不延长设置视图生命周期。
///
/// @author kongweiguang
fn key_resize_input_events(
    panel: WeakEntity<PreferencesPanel>,
    grip_color: gpui_kit::Hsla,
) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            for offset in [2.0_f32, 5.0, 8.0] {
                let mut grip = PathBuilder::stroke(px(1.));
                grip.move_to(point(bounds.left() + px(offset), bounds.bottom() - px(1.)));
                grip.line_to(point(bounds.right() - px(1.), bounds.bottom() - px(offset)));
                if let Ok(path) = grip.build() {
                    window.paint_path(path, grip_color);
                }
            }

            let move_panel = panel.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, app| {
                if !phase.bubble() {
                    return;
                }
                let _ = move_panel.update(app, |panel, cx| {
                    let changed = if event.dragging() {
                        panel
                            .connectivity
                            .resize_key_document(event.position.y.as_f32())
                    } else {
                        panel.connectivity.finish_key_document_resize()
                    };
                    if changed {
                        cx.notify();
                    }
                });
            });

            let up_panel = panel.clone();
            window.on_mouse_event(move |event: &MouseUpEvent, phase, _, app| {
                if !phase.bubble() || event.button != MouseButton::Left {
                    return;
                }
                let _ = up_panel.update(app, |panel, cx| {
                    if panel.connectivity.finish_key_document_resize() {
                        cx.notify();
                    }
                });
            });
        },
    )
    .absolute()
    .inset_0()
}

/// 变基通常临时处于 detached HEAD；优先显示恢复状态，避免提示用户错误切换分支。
///
/// @author kongweiguang
fn repository_label(status: &ConfigurationSyncStatus) -> gpui_kit::SharedString {
    if !status.git_available {
        crate::locale::text("Git 不可用", "Git unavailable")
    } else if status.rebase_in_progress {
        crate::locale::text("变基中", "Rebase in progress")
    } else {
        match status.repository {
            SyncRepositoryState::Missing => crate::locale::text("未初始化", "Not initialized"),
            SyncRepositoryState::Ready => crate::locale::text("已初始化", "Initialized"),
            SyncRepositoryState::Detached => crate::locale::text("detached HEAD", "detached HEAD"),
            SyncRepositoryState::NoUpstream => crate::locale::text("无上游分支", "No upstream"),
            SyncRepositoryState::PendingBusy => crate::locale::text("正在清理", "Cleaning up"),
        }
    }
}

/// 同步信息卡统一使用浅深主题表面和 8px 内圆角。
fn sync_card(theme: ThemeTokens) -> gpui_kit::Div {
    div()
        .w_full()
        .p_4()
        .flex()
        .flex_col()
        .gap_3()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.panel)
}

/// 状态项沿用 main 的图标、标签和值层级；列宽由外层按 main 的 1:1.5 比例分配。
fn sync_info_row(
    icon: IconName,
    label: impl Into<gpui_kit::SharedString>,
    value: impl Into<gpui_kit::SharedString>,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .min_w(px(0.))
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .w(px(32.))
                .h(px(32.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .bg(theme.hover)
                .child(Icon::new(icon).size(px(16.)).text_color(theme.muted)),
        )
        .child(
            div()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(label.into()),
                )
                .child(
                    div()
                        .min_w(px(0.))
                        .truncate()
                        .text_size(theme.text_sm)
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .child(value.into()),
                ),
        )
}

/// 路径项使用文件图标并截断绝对路径，避免它推挤相邻 Git 状态信息。
fn sync_path_row(
    icon: IconName,
    id: &'static str,
    label: impl Into<gpui_kit::SharedString>,
    value: &str,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .min_w(px(0.))
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .w(px(32.))
                .h(px(32.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .bg(theme.hover)
                .child(Icon::new(icon).size(px(16.)).text_color(theme.muted)),
        )
        .child(
            div()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(label.into()),
                )
                .child(
                    div()
                        .id(id)
                        .w_full()
                        .min_w(px(0.))
                        .truncate()
                        .text_size(theme.text_xs)
                        .font_family("monospace")
                        .child(value.to_owned()),
                ),
        )
}

/// 结果色和文字同时表达部分失败、冲突或未知状态。
fn sync_run_result(
    summary: &str,
    outcome: ConfigurationSyncOutcome,
    theme: ThemeTokens,
) -> impl IntoElement {
    let color = match outcome {
        ConfigurationSyncOutcome::Succeeded | ConfigurationSyncOutcome::NoChanges => theme.success,
        ConfigurationSyncOutcome::Conflict | ConfigurationSyncOutcome::OutcomeUnknown => {
            theme.error
        }
        ConfigurationSyncOutcome::Cancelled | ConfigurationSyncOutcome::Blocked => theme.warning,
    };
    div()
        .w_full()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .text_size(theme.text_xs)
        .text_color(color)
        .child(summary.to_owned())
}

/// 错误、保护状态和外部运行状态不只依赖颜色表达。
fn sync_note(text: impl Into<gpui_kit::SharedString>, color: gpui_kit::Hsla) -> impl IntoElement {
    div()
        .w_full()
        .text_size(px(12.))
        .text_color(color)
        .child(text.into())
}

#[cfg(test)]
#[path = "../../../tests/unit/preferences_sync_ui.rs"]
mod sync_ui_tests;
