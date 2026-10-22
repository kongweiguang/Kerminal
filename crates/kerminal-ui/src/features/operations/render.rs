//! 运维工作台各区块的原生交互绘制。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement, Render, SharedString, Window,
    assets::IconName,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme::ThemeTokens;

use super::{
    OperationsSection, OperationsView,
    state::{ContainerStatusFilter, SectionPhase, SectionState},
};

mod container_panel;
mod forwarding;
pub(super) mod server;
mod tmux;

impl Render for OperationsView {
    /// 完整工作台和固定 Rail 面板共享绘制逻辑；Compose 组头显示实际目录与配置序列，避免同名项目目标难以区分。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_inputs(window, cx);
        if !self.initialized {
            self.initialized = true;
            if self.active {
                self.start_visible_services(cx);
                self.reload_non_server(cx);
            }
        }
        let theme = ThemeTokens::current(window, cx);
        let dedicated_header = self.section_selection.is_fixed()
            && self.section_selection.active() != OperationsSection::Containers;
        let mut section_content = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .when(self.compact_layout, |body| body.gap_2());
        match self.section_selection.active() {
            OperationsSection::ServerInfo => {
                section_content = section_content.child(self.render_server(theme, cx));
            }
            OperationsSection::Containers => {
                section_content = section_content.child(self.render_containers(theme, window, cx));
            }
            OperationsSection::Tmux => {
                section_content = section_content.child(self.render_tmux(theme, cx));
            }
            OperationsSection::PortForwards => {
                section_content = section_content.child(self.render_forwards(theme, window, cx));
            }
        }
        let body = div()
            .id("operations-scroll-content")
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .when(self.compact_layout, |body| {
                body.flex_1().overflow_y_scroll().gap_2()
            })
            .when(
                dedicated_header && self.mutation_cancellation.is_some(),
                |body| {
                    body.child(
                        Button::new("operations-dedicated-cancel")
                            .compact()
                            .ghost()
                            .label(crate::locale::text("取消操作", "Cancel operation"))
                            .disabled(
                                self.mutation_cancellation
                                    .as_ref()
                                    .is_some_and(|token| token.is_cancelled()),
                            )
                            .on_click(
                                cx.listener(|view, _, _, cx| view.cancel_operation_mutation(cx)),
                            ),
                    )
                },
            )
            .child(section_content)
            .when_some(self.status.clone(), |view, message| {
                view.child(section(
                    crate::locale::text("操作结果", "Action result"),
                    div().text_size(px(12.)).child(message),
                    theme,
                    self.compact_layout,
                ))
            })
            .when(
                self.container_output.has_state()
                    && !matches!(
                        self.container_panel_context,
                        Some(super::ContainerPanelContext::ContainerInfo { .. })
                    ),
                |view| {
                    let mut output = div().w_full().flex().flex_col().gap_1();
                    if self.container_output.phase() == super::DetailReadPhase::Loading {
                        output = output.child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(crate::locale::text("正在读取…", "Loading…")),
                        );
                    }
                    if let Some(message) = self.container_output.feedback() {
                        output = output.child(error_row(message, theme));
                    }
                    if let Some(snapshot) = self.container_output.data() {
                        output =
                            output.child(div().w_full().text_size(px(12.)).child(snapshot.clone()));
                    }
                    let title = self
                        .container_output_target_label
                        .clone()
                        .unwrap_or_else(|| crate::locale::text("输出", "Output").to_string());
                    view.child(section(title, output, theme, self.compact_layout))
                },
            );
        let mut root = div()
            .id("operations-view")
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.materials.shell)
            .text_color(theme.foreground);
        if self.compact_layout {
            root = root
                .gap_2()
                .overflow_hidden()
                .p_3()
                .when(!dedicated_header, |root| {
                    root.child(self.render_header(theme, cx))
                })
                .when(!self.section_selection.is_fixed(), |view| {
                    view.child(self.render_section_navigation(cx))
                })
                .child(body);
        } else {
            root = root
                .gap_3()
                .overflow_y_scroll()
                .p_4()
                .when(!dedicated_header, |root| {
                    root.child(self.render_header(theme, cx))
                })
                .when(!self.section_selection.is_fixed(), |view| {
                    view.child(self.render_section_navigation(cx))
                })
                .child(body);
        }
        root
    }
}

impl OperationsView {
    /// 将各领域区块作为明确导航项展示，活动状态不影响其共享的请求与连接生命周期。
    fn render_section_navigation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let server_info = Button::new("operations-section-server-info")
            .compact()
            .label(crate::locale::text("服务器", "Server"))
            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                view.select_section(OperationsSection::ServerInfo, cx)
            }));
        let active_section = self.section_selection.active();
        let server_info = if active_section == OperationsSection::ServerInfo {
            server_info.primary()
        } else {
            server_info.ghost()
        };
        let tmux = Button::new("operations-section-tmux")
            .compact()
            .label("tmux")
            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                view.select_section(OperationsSection::Tmux, cx)
            }));
        let tmux = if active_section == OperationsSection::Tmux {
            tmux.primary()
        } else {
            tmux.ghost()
        };
        let forwards = Button::new("operations-section-port-forwards")
            .compact()
            .label(crate::locale::text("端口转发", "Port forwards"))
            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                view.select_section(OperationsSection::PortForwards, cx)
            }));
        let forwards = if active_section == OperationsSection::PortForwards {
            forwards.primary()
        } else {
            forwards.ghost()
        };

        div()
            .id("operations-section-navigation")
            .w_full()
            .flex()
            .flex_wrap()
            .gap_2()
            .child(server_info)
            .child(tmux)
            .child(forwards)
    }

    /// 目标只由连接导航显式提供；固定容器刷新的票据和分页在绘制时冻结，迟到回调不能改绑新目标。
    ///
    /// @author kongweiguang
    fn render_header(&self, theme: ThemeTokens, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match self.section_selection.active() {
            OperationsSection::ServerInfo => crate::locale::text("服务器信息", "Server info"),
            OperationsSection::Containers => crate::locale::text("容器", "Containers"),
            OperationsSection::Tmux => crate::locale::text("tmux", "tmux"),
            OperationsSection::PortForwards => crate::locale::text("端口转发", "Port forwards"),
        };
        let icon = section_icon(self.section_selection.active());
        let refresh_label = if self.section_selection.is_fixed() {
            crate::locale::text("刷新", "Refresh")
        } else {
            crate::locale::text("刷新全部", "Refresh all")
        };
        let container_refresh = (self.section_selection.is_fixed()
            && self.section_selection.active() == OperationsSection::Containers)
            .then(|| {
                let target = match self.container_panel_context.as_ref() {
                    Some(super::ContainerPanelContext::ContainerInfo { target, .. }) => {
                        Some(target.clone())
                    }
                    _ => None,
                };
                let info_refresh = target
                    .clone()
                    .map(|target| (target, self.container_info_tab));
                (
                    self.runtime,
                    self.container_action_ticket(target),
                    info_refresh,
                )
            });
        let refresh = Button::new("operations-refresh")
            .compact()
            .ghost()
            .label(refresh_label)
            .on_click(cx.listener({
                let container_refresh = container_refresh.clone();
                move |view, _: &ClickEvent, _, cx| {
                    if let Some((_, ticket, Some((target, kind)))) = container_refresh.as_ref() {
                        if view.container_info_tab == *kind {
                            view.request_container_read(target.clone(), *kind, ticket, cx);
                        }
                    } else if let Some((runtime, ticket, None)) = container_refresh.as_ref() {
                        view.request_container_panel_refresh(*runtime, ticket, cx);
                    } else {
                        view.reload(cx);
                    }
                }
            }));
        let target_label = self.host_target.as_ref().map_or_else(
            || crate::locale::text("本机", "Local").to_string(),
            |host| server::operations_target_label(host, &self.host_display_names),
        );
        let mut target_label_element = div()
            .id("operations-target-label")
            .min_w_0()
            .flex_1()
            .text_size(theme.text_xs)
            .text_color(theme.muted)
            .truncate()
            .child(target_label.clone());
        if let Some(host_id) = self.host_target.as_ref() {
            let id = host_id.as_str();
            let target_tooltip =
                crate::locale::text(format!("主机 ID：{id}"), format!("Host ID: {id}"));
            target_label_element = target_label_element
                .tooltip(move |window, cx| Tooltip::new(target_tooltip.clone()).build(window, cx))
                .aria_label(crate::locale::text(
                    format!("{target_label}，主机 ID：{id}"),
                    format!("{target_label}; Host ID: {id}"),
                ));
        }
        div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .p_2()
            .rounded_md()
            .bg(theme.materials.chrome)
            .when(!self.compact_layout, |header| {
                header.child(
                    div()
                        .flex_1()
                        .text_size(px(18.))
                        .font_weight(gpui_kit::FontWeight::BOLD)
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(gpui_kit::component::Icon::new(icon).text_color(theme.accent))
                        .child(if self.section_selection.is_fixed() {
                            title.clone()
                        } else {
                            crate::locale::text("运维工作台", "Operations")
                        }),
                )
            })
            .when(
                self.compact_layout && self.section_selection.is_fixed(),
                |header| {
                    header.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_size(px(13.))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(gpui_kit::component::Icon::new(icon).text_color(theme.accent))
                            .child(title.clone()),
                    )
                },
            )
            .child(target_label_element)
            .child(refresh)
            .when_some(
                self.mutation_cancellation.as_ref(),
                |header, cancellation| {
                    let cancel_requested = cancellation.is_cancelled();
                    header.child(
                        Button::new("operations-cancel-current-mutation")
                            .compact()
                            .ghost()
                            .label(if cancel_requested {
                                crate::locale::text("正在取消…", "Cancelling…")
                            } else {
                                crate::locale::text("取消操作", "Cancel operation")
                            })
                            .disabled(cancel_requested)
                            .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                view.cancel_operation_mutation(cx)
                            })),
                    )
                },
            )
    }

    /// 固定容器面板只消费 Root 注入的类型化上下文；无上下文时明确展示不可用状态。
    ///
    /// @author kongweiguang
    fn render_containers(
        &mut self,
        theme: ThemeTokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let context = self.container_panel_context.clone().unwrap_or(
            super::ContainerPanelContext::Unavailable {
                reason: super::ContainerPanelUnavailableReason::NoTerminal,
            },
        );
        container_panel::render(self, &context, theme, cx)
    }
    /// 按领域状态筛选，不通过本地化状态文案推断容器是否运行。
    ///
    /// @author kongweiguang
    fn render_container_status_filters(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = div().w_full().flex().flex_wrap().gap_1();
        for (filter, zh, en, id) in [
            (ContainerStatusFilter::All, "全部", "All", "all"),
            (
                ContainerStatusFilter::Running,
                "运行中",
                "Running",
                "running",
            ),
            (
                ContainerStatusFilter::Stopped,
                "已停止",
                "Stopped",
                "stopped",
            ),
        ] {
            let button = Button::new(format!("operations-container-filter-{id}"))
                .compact()
                .label(crate::locale::text(zh, en))
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    if view.container_status_filter != filter {
                        view.container_status_filter = filter;
                        cx.notify();
                    }
                }));
            row = row.child(if self.container_status_filter == filter {
                button.primary()
            } else {
                button.ghost()
            });
        }
        row
    }

    /// 紧凑面板将键值纵向排列；中心视图继续使用固定标签列，避免改变原有信息密度。
    ///
    /// @author kongweiguang
    fn layout_info_row(
        &self,
        label: impl Into<String>,
        value: impl Into<String>,
        theme: ThemeTokens,
    ) -> impl IntoElement + 'static {
        info_row_for_layout(label, value, theme, self.compact_layout)
    }
}

/// Section 的 loading、失败、成功空列表和未读取提示必须彼此可区分。
fn section_feedback<T>(
    state: &SectionState<T>,
    empty: bool,
    empty_message: SharedString,
    theme: ThemeTokens,
) -> impl IntoElement {
    let message = if let Some(error) = state.error() {
        Some(error.to_owned())
    } else {
        match state.phase() {
            SectionPhase::NotLoaded => Some(empty_message.to_string()),
            SectionPhase::Loading => Some(crate::locale::text("正在读取…", "Loading…").to_string()),
            SectionPhase::Cancelled => Some(
                crate::locale::text(
                    "读取已取消；可以重新读取。",
                    "Read cancelled; you can retry.",
                )
                .to_string(),
            ),
            SectionPhase::Ready if empty => Some(
                crate::locale::text("读取成功，当前没有项目。", "Loaded successfully; no items.")
                    .to_string(),
            ),
            SectionPhase::Ready | SectionPhase::Failed => None,
        }
    };
    div().w_full().when_some(message, |row, message| {
        row.text_size(theme.text_xs)
            .text_color(if state.error().is_some() {
                theme.error
            } else {
                theme.muted
            })
            .child(message)
    })
}

/// 只抑制转发列表的 Ready 空结果提示；失败、取消、加载和未读取仍由共享反馈决定。
///
/// @author kongweiguang
fn should_suppress_ready_empty_feedback(
    phase: SectionPhase,
    has_data: bool,
    rows_empty: bool,
) -> bool {
    matches!(phase, SectionPhase::Ready) && has_data && rows_empty
}

/// 概览字段在窄右栏纵向排列，并显式折开长值，避免 GPUI 单行文本裁切掉目标细节。
///
/// @author kongweiguang
fn container_inspect(
    details: &kerminal_domain::operations::ContainerInspect,
    theme: ThemeTokens,
) -> impl IntoElement {
    let arrays = |values: &[String]| {
        if values.is_empty() {
            "—".to_owned()
        } else {
            values.join(" · ")
        }
    };
    let container_id = details.id.clone();
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .p_2()
        .rounded_md()
        .bg(theme.sidebar)
        .child(container_detail_row(
            &localized_text("容器详情", "Container details"),
            format!("{} · {}", details.name, details.status),
            theme,
        ))
        .child(container_detail_row(
            &localized_text("容器 ID", "Container ID"),
            container_id,
            theme,
        ))
        .child(container_detail_row(
            &localized_text("镜像", "Image"),
            details.image.clone(),
            theme,
        ))
        .child(container_detail_row(
            &localized_text("入口程序", "Entrypoint"),
            arrays(&details.entrypoint),
            theme,
        ))
        .child(container_detail_row(
            &localized_text("命令", "Command"),
            arrays(&details.command),
            theme,
        ))
        .child(container_detail_row(
            &localized_text("工作目录 / 用户", "Working directory / User"),
            format!(
                "{} · {}",
                details.working_dir.as_deref().unwrap_or("—"),
                details.user.as_deref().unwrap_or("—")
            ),
            theme,
        ))
        .child(container_detail_row(
            &localized_text("端口", "Ports"),
            arrays(&details.ports),
            theme,
        ))
        .child(container_detail_row(
            &localized_text("网络", "Networks"),
            arrays(&details.networks),
            theme,
        ))
        .child(container_detail_row(
            &localized_text("创建 / 启动 / 结束", "Created / Started / Finished"),
            format!(
                "{} · {} · {}",
                details.created.as_deref().unwrap_or("—"),
                details.started_at.as_deref().unwrap_or("—"),
                details.finished_at.as_deref().unwrap_or("—")
            ),
            theme,
        ))
}

/// 概览标签保持低对比层级、值使用前景色；软断点让换行由实际栏宽决定。
///
/// @author kongweiguang
fn container_detail_row(
    label: impl Into<String>,
    value: impl Into<String>,
    theme: ThemeTokens,
) -> impl IntoElement + 'static {
    let label = label.into();
    let value: String = value.into();
    let display_value = add_container_detail_soft_breaks(&value);
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .w_full()
                .min_w_0()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(label),
        )
        .child(
            div()
                .w_full()
                .min_w_0()
                .text_size(px(12.))
                .text_color(theme.foreground)
                .child(display_value),
        )
}

/// 只给过长的连续词插入零宽断点，让 GPUI 按当前槽宽折行且不改复制源文本。
///
/// @author kongweiguang
fn add_container_detail_soft_breaks(value: &str) -> String {
    const TOKEN_BREAK_INTERVAL: usize = 12;

    let mut display = String::with_capacity(
        value
            .len()
            .saturating_add(value.len() / TOKEN_BREAK_INTERVAL),
    );
    let mut token_graphemes = 0;
    let mut graphemes = value.graphemes(true).peekable();
    let is_natural_break = |grapheme: &str| {
        grapheme.chars().any(|character| {
            character.is_whitespace()
                || matches!(
                    character,
                    '/' | '\\' | '.' | ':' | '_' | '-' | '·' | ',' | ';' | '='
                )
        })
    };
    let is_soft_break_delimiter = |grapheme: &str| {
        grapheme.chars().any(|character| {
            matches!(
                character,
                '/' | '\\' | '.' | ':' | '_' | '-' | '·' | ',' | ';' | '='
            )
        })
    };
    while let Some(grapheme) = graphemes.next() {
        display.push_str(grapheme);
        if is_natural_break(grapheme) {
            token_graphemes = 0;
            if is_soft_break_delimiter(grapheme)
                && graphemes.peek().is_some_and(|next| !is_natural_break(next))
            {
                display.push('\u{200b}');
            }
        } else {
            token_graphemes += 1;
            if token_graphemes == TOKEN_BREAK_INTERVAL
                && graphemes.peek().is_some_and(|next| !is_natural_break(next))
            {
                display.push('\u{200b}');
                token_graphemes = 0;
            }
        }
    }
    display
}

/// 同一行信息按面板实际槽宽选择标签列或纵向堆叠，并允许窄栏中的长值收缩换行。
///
/// @author kongweiguang
fn info_row_for_layout(
    label: impl Into<String>,
    value: impl Into<String>,
    theme: ThemeTokens,
    compact: bool,
) -> impl IntoElement + 'static {
    let label = label.into();
    let value = value.into();
    div()
        .w_full()
        .min_w_0()
        .flex()
        .when(compact, |row| row.flex_col().items_start().gap_1())
        .when(!compact, |row| row.items_center().gap_2())
        .child(
            div()
                .when(compact, |label| label.w_full())
                .when(!compact, |label| label.w(px(150.)))
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(label),
        )
        .child(
            div()
                .when(compact, |value| value.w_full())
                .when(!compact, |value| value.flex_1())
                .min_w_0()
                .text_size(px(12.))
                .child(value),
        )
}

/// 按全局应用语言生成 owned 标签，让返回的元素不借用 locale 临时值。
fn localized_text(zh_cn: &str, en_us: &str) -> String {
    crate::locale::text(zh_cn, en_us).to_string()
}

/// 错误行使用成对主题颜色显示 application 公共错误信息。
fn error_row(message: &str, theme: ThemeTokens) -> impl IntoElement {
    div()
        .text_size(theme.text_xs)
        .text_color(theme.error)
        .child(message.to_owned())
}

/// Section 卡片共用面板令牌，窄面板缩小留白以给正文保留可用宽度。
///
/// @author kongweiguang
fn section(
    title: impl Into<SharedString>,
    content: impl IntoElement,
    theme: ThemeTokens,
    compact: bool,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .when(compact, |section| section.gap_1().p_2())
        .when(!compact, |section| section.p_3())
        .rounded_md()
        .bg(theme.panel)
        .child(
            div()
                .text_size(px(14.))
                .font_weight(gpui_kit::FontWeight::BOLD)
                .child(title.into()),
        )
        .child(content)
}

/// 固定面板复用页头标题；窄固定栏去掉第二层卡片，避免工具栏内容被重复包壳。
///
/// @author kongweiguang
fn section_page(
    title: impl Into<SharedString>,
    content: impl IntoElement,
    theme: ThemeTokens,
    compact: bool,
    fixed: bool,
) -> impl IntoElement {
    let title = title.into();
    let flattened = is_fixed_compact_operations_panel(compact, fixed);
    let mut page = div().w_full().flex().flex_col().gap_2();
    if !flattened {
        page = page
            .when(compact, |page| page.gap_1().p_2())
            .when(!compact, |page| page.p_3())
            .rounded_md()
            .bg(theme.panel);
    }
    if !fixed {
        page = page.child(
            div()
                .text_size(px(if compact { 13. } else { 15. }))
                .font_weight(gpui_kit::FontWeight::BOLD)
                .child(title),
        );
    }
    page.child(content)
}

/// 固定窄栏已有独立标题和刷新栏，正文不再重复增加一张面板卡片。
///
/// @author kongweiguang
fn is_fixed_compact_operations_panel(compact: bool, fixed: bool) -> bool {
    compact && fixed
}

/// 章节图标和 ToolRail 工具身份保持一致，便于窄栏快速识别当前能力。
///
/// @author kongweiguang
fn section_icon(section: OperationsSection) -> IconName {
    match section {
        OperationsSection::ServerInfo => IconName::Server,
        OperationsSection::Containers => IconName::Container,
        OperationsSection::Tmux => IconName::SquareTerminal,
        OperationsSection::PortForwards => IconName::Network,
    }
}

/// 正常可接入时不占正文；受限时区分缺少终端和目标不匹配，避免泄漏内部状态术语。
///
/// @author kongweiguang
fn tmux_attach_guidance(
    attach_ready: bool,
    has_binding: bool,
) -> Option<(&'static str, &'static str)> {
    if attach_ready {
        None
    } else if has_binding {
        Some((
            "当前终端与列表目标不一致，请切换到对应终端。",
            "The focused terminal does not match this list; switch to the matching terminal.",
        ))
    } else {
        Some((
            "请聚焦当前目标对应的终端，再连接会话。",
            "Focus a terminal for this target to attach.",
        ))
    }
}

/// 日志和 pane capture 限制展示字符数，并按 Unicode 字符截断避免越过 UTF-8 边界。
pub(super) fn bounded_text(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let mut result = value.chars().take(limit).collect::<String>();
    result.push_str("\n…（界面已截断）");
    result
}

#[cfg(test)]
#[path = "../../../tests/unit/operations_render_tmux.rs"]
mod tmux_tests;

#[cfg(test)]
#[path = "../../../tests/unit/operations_render_container.rs"]
mod container_render_tests;
