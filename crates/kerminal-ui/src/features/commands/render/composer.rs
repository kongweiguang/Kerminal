//! 交互式命令草稿、建议和广播确认。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement, Window,
    component::input::Input,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
    },
    div,
    prelude::*,
    px,
};

use crate::theme::ThemeTokens;
use kerminal_domain::commands::CommandSuggestionActivation;

use super::super::CommandsView;

impl CommandsView {
    /// 命令始终对应顶栏显示的 session；输入与提交共享首行，次级操作放在可换行的第二行，给历史列表留出空间。
    ///
    /// @author kongweiguang
    pub(super) fn render_composer(
        &mut self,
        theme: ThemeTokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let command_is_empty = self.command_input.read(cx).value().trim().is_empty();
        let suggestions = self
            .suggestions
            .iter()
            .filter(|candidate| !candidate.allowed_presentations.is_empty())
            .take(6)
            .map(|candidate| {
                let id = candidate.id.clone();
                let label = candidate.display_text.clone();
                let description = candidate.description.clone().unwrap_or_default();
                div()
                    .w_full()
                    .flex()
                    .when(self.compact_layout, |row| row.flex_wrap())
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(theme.background)
                    .child(
                        Button::new(format!("command-suggestion-{id}"))
                            .compact()
                            .label(label)
                            .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                                view.accept_suggestion(id.clone(), window, cx);
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(11.))
                            .text_color(theme.muted)
                            .child(description),
                    )
            });
        let pending = self
            .pending_broadcast
            .as_ref()
            .map(|(command, targets, _)| {
                (
                    command.clone(),
                    targets
                        .iter()
                        .map(|target| target.title.clone())
                        .collect::<Vec<_>>(),
                )
            });

        let mut panel = div()
            .id("commands-composer")
            .w_full()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .bg(theme.panel)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Input::new(&self.command_input)
                            .flex_1()
                            .min_w(px(0.))
                            .h(px(32.)),
                    )
                    .child(
                        Button::new("commands-send")
                            .compact()
                            .w(px(88.))
                            .h(px(32.))
                            .primary()
                            .label(crate::locale::text(
                                if self.send_operation.active.is_some() { "发送中…" } else { "提交" },
                                if self.send_operation.active.is_some() { "Sending…" } else { "Send" },
                            ))
                            .disabled(
                                command_is_empty
                                    || self.send_operation.active.is_some()
                                    || self.session_id.is_none()
                                    || self.ui_session_binding.is_none(),
                            )
                            .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                view.send_command(window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .when(self.compact_layout, |row| row.flex_wrap())
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("commands-record-toggle")
                            .compact()
                            .label(crate::locale::text(
                                if self.record_command { "记录历史：开" } else { "记录历史：关" },
                                if self.record_command { "Record history: On" } else { "Record history: Off" },
                            ))
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.record_command = !view.record_command;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("commands-suggest")
                            .compact()
                            .label(crate::locale::text("命令建议", "Suggestions"))
                            .disabled(self.session_id.is_none())
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.load_suggestions(cx);
                            })),
                    )
                    .child(
                        Button::new("commands-broadcast-preview")
                            .compact()
                            .label(crate::locale::text("预览广播", "Preview broadcast"))
                            .disabled(self.send_operation.active.is_some())
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.prepare_broadcast(cx);
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(11.))
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "提交会写入当前交互终端；退出码不跟踪",
                                "Send writes to the current interactive terminal; exit status is not tracked",
                            )),
                    ),
            );

        if self
            .send_operation
            .active
            .is_some_and(|token| token.kind == super::super::SendOperationKind::Broadcast)
            && self.broadcast_control.is_some()
        {
            panel = panel.child(
                Button::new("commands-broadcast-stop")
                    .compact()
                    .when(self.compact_layout, |button| button.w_full())
                    .danger()
                    .label(crate::locale::text("停止广播", "Stop broadcast"))
                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                        view.cancel_broadcast(cx);
                    })),
            );
        }

        if self
            .suggestions
            .iter()
            .any(|candidate| !candidate.allowed_presentations.is_empty())
        {
            panel = panel.child(div().flex().flex_col().gap_1().children(suggestions));
        }

        if let Some((command, targets)) = pending {
            let target_list = targets.join(
                if crate::locale::current_language() == crate::locale::UiLanguage::ZhCn {
                    "、"
                } else {
                    ", "
                },
            );
            panel = panel.child(
                div()
                    .id("commands-broadcast-confirm")
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .rounded_md()
                    .bg(theme.background)
                    .text_size(px(12.))
                    .child(crate::locale::text(
                        format!("将把以下命令写入 {target_list}："),
                        format!("This command will be sent to {target_list}:"),
                    ))
                    .child(div().text_color(theme.accent).child(command))
                    .child(
                        div()
                            .flex()
                            .when(self.compact_layout, |row| row.flex_wrap())
                            .gap_2()
                            .child(
                                Button::new("commands-broadcast-confirm")
                                    .compact()
                                    .danger()
                                    .disabled(self.send_operation.active.is_some())
                                    .label(crate::locale::text("确认广播", "Confirm broadcast"))
                                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                        view.execute_broadcast(true, cx);
                                    })),
                            )
                            .child(
                                Button::new("commands-broadcast-cancel")
                                    .compact()
                                    .label(crate::locale::text("取消", "Cancel"))
                                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                        view.pending_broadcast = None;
                                        view.status = Some(super::super::command_message(
                                            "已取消命令广播",
                                            "Broadcast cancelled",
                                        ));
                                        cx.notify();
                                    })),
                            ),
                    ),
            );
        }

        panel
    }

    /// 应用候选的 Unicode 字符范围，危险候选只插入草稿且绝不代替用户提交。
    pub(super) fn accept_suggestion(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(candidate) = self.suggestions.iter().find(|item| item.id == id).cloned() else {
            self.status = Some(super::super::command_message(
                "命令建议已过期，请重新查询",
                "Suggestion expired; request suggestions again",
            ));
            cx.notify();
            return;
        };
        if candidate.activation == CommandSuggestionActivation::OpenSnippetPanel {
            self.select_snippets(cx);
            if let Some(snippet_id) = candidate.source_id {
                self.open_snippet(snippet_id, window, cx);
            } else {
                self.status = Some(super::super::command_message(
                    "此建议要求打开片段目录",
                    "This suggestion requires opening the snippets panel",
                ));
                cx.notify();
            }
            return;
        }
        let current = self
            .command_input
            .update(cx, |input, _| input.value().to_string());
        let mut characters = current.chars().collect::<Vec<_>>();
        let start = candidate.replacement_range.start;
        let end = candidate.replacement_range.end;
        if start > end || end > characters.len() {
            self.status = Some(super::super::command_message(
                "建议替换范围已失效，请重新查询",
                "Suggestion range is stale; request suggestions again",
            ));
            cx.notify();
            return;
        }
        characters.splice(start..end, candidate.replacement_text.chars());
        let mut value = characters.into_iter().collect::<String>();
        value.push_str(&candidate.suffix);
        self.command_input
            .update(cx, |input, cx| input.set_value(value, window, cx));
        self.suggestions.clear();
        self.record_command = candidate.sensitivity
            != kerminal_domain::commands::CommandSuggestionSensitivity::Sensitive;
        self.status = Some(super::super::command_message(
            "建议已填入命令草稿，请检查后提交",
            "Suggestion copied to the command draft; review it before sending",
        ));
        cx.notify();
    }
}
