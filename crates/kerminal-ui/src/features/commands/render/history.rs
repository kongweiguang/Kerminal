//! 命令历史搜索、回填、删除和范围清理界面。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement,
    component::input::Input,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::commands::{CommandHistoryAvailability, CommandHistoryUnavailableReason};

use crate::theme::ThemeTokens;
use crate::ui_button::icon_button;

use super::super::CommandsView;

impl CommandsView {
    /// 历史搜索显式限定当前会话；一眼可识别的搜索改用图标，其余清理仍标明范围并经过确认。
    /// 搜索等待时保留原槽位并禁用重复提交；失败后仍可重试，点击记录只回填草稿。
    ///
    /// @author kongweiguang
    pub(super) fn render_history(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rows = self.history.iter().map(|entry| {
            let entry_id = entry.id.clone();
            let command = entry.command.clone();
            let display_command = entry.command.clone();
            let context = format!(
                "{:?} · {:?} · {}",
                entry.source,
                entry.target,
                entry.cwd.as_deref().unwrap_or(
                    if crate::locale::current_language() == crate::locale::UiLanguage::ZhCn {
                        "工作目录未知"
                    } else {
                        "Working directory unknown"
                    }
                )
            );
            let created_at = entry.created_at.clone();
            let delete_id = entry.id.clone();
            div()
                .id(format!("command-history-row-{entry_id}"))
                .w_full()
                .p_3()
                .flex()
                .items_center()
                .gap_2()
                .rounded_md()
                .bg(theme.panel)
                .cursor_pointer()
                .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                    view.use_history_command(command.clone(), window, cx);
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().text_size(px(13.)).child(display_command))
                        .child(
                            div()
                                .w_full()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(format!("{context} · {created_at}")),
                        ),
                )
                .child(
                    Button::new(format!("command-history-delete-{delete_id}"))
                        .compact()
                        .label(crate::locale::text("删除", "Delete"))
                        .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                            view.delete_history_entry(entry_id.clone(), cx);
                        })),
                )
        });
        let all_history = self.session_id.is_none();
        let history_available = self.commands.history_availability();
        let unavailable_message = match history_available {
            CommandHistoryAvailability::Available => None,
            CommandHistoryAvailability::Unavailable { reason } => {
                Some(history_unavailable_message(reason))
            }
        };
        let available = unavailable_message.is_none();
        let mut content = div()
            .id("command-history-view")
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .when(self.compact_layout, |row| row.flex_wrap())
                    .items_center()
                    .gap_2()
                    .child(Input::new(&self.history_query).w_full())
                    .child(
                        icon_button(
                            "command-history-search",
                            gpui_kit::assets::IconName::Search,
                            crate::locale::text("搜索命令历史", "Search command history"),
                        )
                        .primary()
                        .loading(self.loading || self.history_read.is_loading())
                        .disabled(!available || self.loading || self.history_read.is_loading())
                        .on_click(cx.listener(
                            |view, _: &ClickEvent, _, cx| {
                                view.search_history(cx);
                            },
                        )),
                    )
                    .child(
                        Button::new("command-history-clear")
                            .compact()
                            .label(crate::locale::text(
                                if all_history {
                                    "清空全部"
                                } else {
                                    "清空本会话"
                                },
                                if all_history {
                                    "Clear all"
                                } else {
                                    "Clear this session"
                                },
                            ))
                            .disabled(!available || self.history.is_empty())
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.confirm_clear_history = true;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("command-history-list")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(rows)
                    .when(
                        self.history.is_empty() && !self.loading && available,
                        |list| {
                            list.child(empty_state(
                                crate::locale::text(
                                    "没有符合条件的命令历史",
                                    "No matching command history",
                                ),
                                theme,
                            ))
                        },
                    ),
            );

        if let Some(message) = unavailable_message {
            content = content.child(
                div()
                    .id("command-history-unavailable")
                    .w_full()
                    .p_3()
                    .rounded_md()
                    .bg(theme.panel)
                    .text_color(theme.error)
                    .text_size(px(12.))
                    .child(message),
            );
        }

        if self.confirm_clear_history {
            let target = if all_history {
                crate::locale::text("全部命令历史", "all command history")
            } else {
                crate::locale::text("当前会话的命令历史", "this session's command history")
            };
            content = content.child(
                div()
                    .p_3()
                    .flex()
                    .when(self.compact_layout, |row| row.flex_col().items_start())
                    .when(!self.compact_layout, |row| row.items_center())
                    .flex_wrap()
                    .gap_3()
                    .rounded_md()
                    .bg(theme.panel)
                    .child(crate::locale::text(
                        format!("确认清理{target}？此操作无法撤销。"),
                        format!("Clear {target}? This action cannot be undone."),
                    ))
                    .child(
                        Button::new("command-history-clear-confirm")
                            .compact()
                            .danger()
                            .label(crate::locale::text("确认清理", "Confirm clear"))
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.clear_history(cx);
                            })),
                    )
                    .child(
                        Button::new("command-history-clear-cancel")
                            .compact()
                            .label(crate::locale::text("取消", "Cancel"))
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.confirm_clear_history = false;
                                cx.notify();
                            })),
                    ),
            );
        }

        content
    }
}

/// 将仓储启动故障映射为可行动的 UI 提示，不把损坏历史显示为空结果。
///
/// @author kongweiguang
pub(in crate::features::commands) fn history_unavailable_message(
    reason: CommandHistoryUnavailableReason,
) -> String {
    match reason {
        CommandHistoryUnavailableReason::MigrationRequired(_) => super::super::command_message(
            "命令历史需要先执行0.4数据迁移。",
            "Command history requires the 0.4 data migration before it can be used.",
        ),
        CommandHistoryUnavailableReason::UnsupportedSchemaVersion(version) => {
            super::super::command_message(
                format!("命令历史使用不支持的数据库版本 {version}，已保留原文件并停用历史功能。"),
                format!(
                    "Command history uses unsupported database version {version}. The original file was preserved and history is disabled."
                ),
            )
        }
        CommandHistoryUnavailableReason::CorruptDatabase => super::super::command_message(
            "命令历史数据库损坏，应用已保留原文件并停用历史功能。",
            "Command history database is corrupt. The original file was preserved and history is disabled.",
        ),
        CommandHistoryUnavailableReason::StorageUnavailable => super::super::command_message(
            "无法访问命令历史存储，其他终端操作仍可继续。",
            "Command history storage is unavailable; terminal operations can continue.",
        ),
        CommandHistoryUnavailableReason::BackupFailed => super::super::command_message(
            "命令历史安全备份失败，未启动迁移或改写，历史功能已停用。",
            "The command history backup failed. Migration was not started and history is disabled.",
        ),
        CommandHistoryUnavailableReason::StorageLockUnavailable => super::super::command_message(
            "无法取得命令历史存储锁，历史功能已停用。",
            "Could not acquire the command history storage lock; history is disabled.",
        ),
    }
}

/// 空状态告知当前范围，不在列表为空时暗示历史读取已经失败。
fn empty_state(message: gpui_kit::SharedString, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .justify_center()
        .py_8()
        .text_size(px(12.))
        .text_color(theme.muted)
        .child(message)
}
