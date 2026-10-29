//! 命令工作区主布局与主题导航。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement, Render, Window,
    component::button::{Button, ButtonVariants},
    div,
    prelude::*,
    px,
};

use crate::theme::ThemeTokens;

use super::{CommandsSection, CommandsView, SendOperationKind};
use kerminal_domain::sessions::SessionTarget;

#[path = "render/composer.rs"]
mod composer;
#[path = "render/history.rs"]
mod history;
pub(super) use history::history_unavailable_message;
#[path = "render/snippet_advanced.rs"]
mod snippet_advanced;
#[path = "render/snippets.rs"]
mod snippets;
#[path = "render/workflows.rs"]
mod workflows;

impl Render for CommandsView {
    /// 右栏片段把分类导航收进列表，只保留一次任务标题；其它紧凑页面仍使用共享导航，避免丢失工作区入口。
    /// 根层与状态条沿用统一材质；右栏使用局部中性色阶，避免影响同实体的中央工作区呈现。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        let theme = if self.compact_layout {
            theme.for_tool_panel()
        } else {
            theme
        };
        let show_navigation = !(self.compact_layout && self.section == CommandsSection::Snippets);
        let navigation =
            show_navigation.then(|| self.render_navigation(theme, cx).into_any_element());
        let content = match self.section {
            CommandsSection::History => self.render_history(theme, cx).into_any_element(),
            CommandsSection::Snippets => self.render_snippets(theme, window, cx).into_any_element(),
            CommandsSection::Workflows => {
                self.render_workflows(theme, window, cx).into_any_element()
            }
        };

        div()
            .id("commands-workspace")
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(
                if self.compact_layout && self.section == CommandsSection::Snippets {
                    theme.tool_panel_surface
                } else {
                    theme.materials.shell
                },
            )
            .text_color(theme.foreground)
            .child(self.render_header(theme))
            .when_some(navigation, |view, navigation| view.child(navigation))
            .child(
                div()
                    .id("commands-content-host")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(content),
            )
            .when(
                self.section == CommandsSection::History
                    || self.pending_broadcast.is_some()
                    || (self
                        .send_operation
                        .active
                        .is_some_and(|operation| operation.kind == SendOperationKind::Broadcast)
                        && self.broadcast_control.is_some()),
                |view| view.child(self.render_composer(theme, window, cx)),
            )
            .when_some(self.status.clone(), |view, status| {
                view.child(
                    div()
                        .id("commands-status")
                        .w_full()
                        .when(self.compact_layout, |status| status.px_2())
                        .when(!self.compact_layout, |status| status.px_4())
                        .py_2()
                        .bg(theme.materials.chrome)
                        .text_size(px(12.))
                        .text_color(theme.muted)
                        .child(status),
                )
            })
    }
}

impl CommandsView {
    /// 片段标题占用 main 抽屉内边距、标题和下方间距的总高度；其它紧凑页保留目标与加载提示。
    ///
    /// @author kongweiguang
    fn render_header(&self, theme: ThemeTokens) -> impl IntoElement {
        let session = self
            .session_info
            .as_ref()
            .map(|session| {
                let target = match &session.target {
                    SessionTarget::Local(_) => crate::locale::text("本地终端", "Local terminal"),
                    SessionTarget::Ssh(_) => crate::locale::text("SSH 终端", "SSH terminal"),
                    SessionTarget::Telnet(_) => {
                        crate::locale::text("Telnet 终端", "Telnet terminal")
                    }
                    SessionTarget::Serial(_) => crate::locale::text("串口终端", "Serial terminal"),
                    SessionTarget::Container(_) => {
                        crate::locale::text("容器终端", "Container terminal")
                    }
                };
                crate::locale::text(format!("目标：{target}"), format!("Target: {target}"))
                    .to_string()
            })
            .unwrap_or_else(|| {
                crate::locale::text("未选择终端", "No terminal selected").to_string()
            });
        let mut header = div().w_full().flex().items_center().gap_2().bg(
            if self.compact_layout && self.section == CommandsSection::Snippets {
                theme.tool_panel_surface
            } else {
                theme.materials.chrome
            },
        );
        header = if self.compact_layout && self.section == CommandsSection::Snippets {
            header.h(px(52.)).px_3()
        } else if self.compact_layout {
            header.h(px(40.)).px_2()
        } else {
            header.h(px(52.)).px_4()
        };
        header
            .child(
                div()
                    .flex_1()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_size(
                        if self.compact_layout && self.section == CommandsSection::Snippets {
                            px(18.)
                        } else if self.compact_layout {
                            theme.text_title
                        } else {
                            theme.text_heading
                        },
                    )
                    .child(if self.compact_layout {
                        match self.section {
                            CommandsSection::History => crate::locale::text("历史", "History"),
                            CommandsSection::Snippets => crate::locale::text("片段", "Snippets"),
                            CommandsSection::Workflows => {
                                crate::locale::text("工作流", "Workflows")
                            }
                        }
                    } else {
                        crate::locale::text("命令工作区", "Commands")
                    }),
            )
            .when(self.section != CommandsSection::Snippets, |header| {
                header.child(
                    div()
                        .max_w(px(if self.compact_layout { 150. } else { 340. }))
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(session),
                )
            })
            .when(self.loading, |bar| {
                bar.child(
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text("同步中…", "Refreshing…")),
                )
            })
    }

    /// 导航使用 chrome 材质区分工作区标题层与内部卡片，并在紧凑槽位换行保留操作空间。
    ///
    /// @author kongweiguang
    fn render_navigation(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let navigation = div()
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .bg(theme.materials.chrome);
        let navigation = if self.compact_layout {
            navigation.flex_wrap().px_2().py_1()
        } else {
            navigation.h(px(44.)).px_3()
        };
        navigation
            .child(self.section_button(
                crate::locale::text("历史", "History"),
                CommandsSection::History,
                theme,
                cx,
            ))
            .child(self.section_button(
                crate::locale::text("片段", "Snippets"),
                CommandsSection::Snippets,
                theme,
                cx,
            ))
            .child(self.section_button(
                crate::locale::text("工作流", "Workflows"),
                CommandsSection::Workflows,
                theme,
                cx,
            ))
    }

    /// 任务切换使用中性选中表面，把强调色留给保存、运行等明确提交操作。
    ///
    /// @author kongweiguang
    fn section_button(
        &mut self,
        label: gpui_kit::SharedString,
        section: CommandsSection,
        _theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Button {
        let active = self.section == section;
        Button::new(format!("commands-section-{section:?}"))
            .compact()
            .label(label)
            .when(active, Button::secondary)
            .when(!active, Button::ghost)
            .when(self.compact_layout, |button| button.flex_1())
            .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                view.select_section(section, cx);
            }))
    }
}
