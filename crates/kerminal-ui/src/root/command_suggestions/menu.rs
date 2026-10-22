//! 命令建议的原生菜单视图；查询、过滤和接受仍由 Root 的唯一 controller 负责。
//!
//! @author kongweiguang

use std::sync::Arc;

use gpui_kit::{
    AnyElement, ClickEvent, Context, EventEmitter, IntoElement, Render, Window, div, prelude::*, px,
};
use kerminal_domain::commands::{CommandSuggestionCandidate, SuggestionProviderKind};

use super::TerminalSuggestionQueryState;
use crate::theme::ThemeTokens;

const MAX_MENU_HEIGHT: f32 = 360.0;

/// Root 提供短期只读候选快照；候选业务状态与接受权限不会复制到菜单。
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalSuggestionMenuProjection {
    pub request_generation: u64,
    pub state: TerminalSuggestionQueryState,
    pub partial_accept: bool,
    pub candidates: Arc<[CommandSuggestionCandidate]>,
}

/// 菜单只发出带查询代次的用户意图，Root 必须再次校验当前 session/context/CAS。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalCommandSuggestionMenuEvent {
    Accept {
        request_generation: u64,
        candidate_id: String,
        partial_accept: bool,
    },
    Retry {
        request_generation: u64,
    },
    Dismiss {
        request_generation: u64,
    },
}

/// Root 将终端焦点按键翻译成有限菜单键；普通文本与输入法事件不得交给此组件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalCommandSuggestionMenuKey {
    Up,
    Down,
    Enter,
    Escape,
}

/// 只保存键盘高亮所需的候选 ID 顺序，不保存候选正文或第二份业务查询状态。
#[derive(Default)]
struct MenuSelection {
    generation: Option<u64>,
    candidate_ids: Vec<String>,
    selected_index: usize,
}

impl MenuSelection {
    /// 新结果按 ID 保留仍存在的高亮项；请求代次变化时回到第一项，避免旧结果索引串用。
    fn reconcile(&mut self, generation: u64, candidates: &[CommandSuggestionCandidate]) {
        let same_generation = self.generation == Some(generation);
        let selected_id = same_generation
            .then(|| self.candidate_ids.get(self.selected_index))
            .flatten();
        let candidate_ids = candidates
            .iter()
            .map(|candidate| candidate.id.clone())
            .collect::<Vec<_>>();
        self.selected_index = if same_generation {
            selected_id
                .and_then(|selected| candidate_ids.iter().position(|id| id == selected))
                .unwrap_or(0)
        } else {
            0
        };
        self.generation = Some(generation);
        self.candidate_ids = candidate_ids;
        if self.candidate_ids.is_empty() {
            self.selected_index = 0;
        } else {
            self.selected_index = self.selected_index.min(self.candidate_ids.len() - 1);
        }
    }

    /// 在非空列表中环绕移动，保持 Up/Down 对首尾项的操作可预期。
    fn move_by(&mut self, delta: isize) -> bool {
        let count = self.candidate_ids.len();
        if count == 0 {
            return false;
        }
        self.selected_index = if delta < 0 {
            (self.selected_index + count - 1) % count
        } else {
            (self.selected_index + 1) % count
        };
        true
    }

    /// 只对当前 generation 的合法索引更新键盘高亮。
    fn select(&mut self, generation: u64, index: usize) -> bool {
        if self.generation != Some(generation) || index >= self.candidate_ids.len() {
            return false;
        }
        self.selected_index = index;
        true
    }

    /// 从只读 projection 中按当前高亮项解析候选 ID。
    fn selected_id(&self) -> Option<&str> {
        self.candidate_ids
            .get(self.selected_index)
            .map(String::as_str)
    }
}

/// 菜单视图只持有最近一次投影和键盘高亮，不持有服务或终端资源。
pub struct TerminalCommandSuggestionMenu {
    projection: TerminalSuggestionMenuProjection,
    selection: MenuSelection,
}

impl EventEmitter<TerminalCommandSuggestionMenuEvent> for TerminalCommandSuggestionMenu {}

impl TerminalCommandSuggestionMenu {
    /// 首次打开时以当前请求快照初始化；Root 仍负责决定菜单锚点和候选接受。
    pub fn new(projection: TerminalSuggestionMenuProjection) -> Self {
        let mut selection = MenuSelection::default();
        let candidates = if projection.state == TerminalSuggestionQueryState::Ready {
            projection.candidates.as_ref()
        } else {
            &[]
        };
        selection.reconcile(projection.request_generation, candidates);
        Self {
            projection,
            selection,
        }
    }

    /// 更新外部唯一候选投影并同步高亮项，迟到代次不会被组件自行接受。
    pub fn set_projection(
        &mut self,
        projection: TerminalSuggestionMenuProjection,
        cx: &mut Context<Self>,
    ) {
        let candidates = if projection.state == TerminalSuggestionQueryState::Ready {
            projection.candidates.as_ref()
        } else {
            &[]
        };
        self.selection
            .reconcile(projection.request_generation, candidates);
        self.projection = projection;
        cx.notify();
    }

    /// 处理菜单专属按键；打开菜单时即使无候选也拦截 Enter，避免误提交终端命令。
    pub fn handle_key(
        &mut self,
        key: TerminalCommandSuggestionMenuKey,
        cx: &mut Context<Self>,
    ) -> bool {
        let generation = self.projection.request_generation;
        if self.projection.state == TerminalSuggestionQueryState::Closed {
            return false;
        }
        match key {
            TerminalCommandSuggestionMenuKey::Up => {
                if self.projection.state == TerminalSuggestionQueryState::Ready
                    && self.selection.move_by(-1)
                {
                    cx.notify();
                }
            }
            TerminalCommandSuggestionMenuKey::Down => {
                if self.projection.state == TerminalSuggestionQueryState::Ready
                    && self.selection.move_by(1)
                {
                    cx.notify();
                }
            }
            TerminalCommandSuggestionMenuKey::Enter => match self.projection.state {
                TerminalSuggestionQueryState::Ready => {
                    if let Some(candidate_id) = self.selection.selected_id() {
                        cx.emit(TerminalCommandSuggestionMenuEvent::Accept {
                            request_generation: generation,
                            candidate_id: candidate_id.to_owned(),
                            partial_accept: self.projection.partial_accept,
                        });
                    }
                }
                TerminalSuggestionQueryState::Failed => {
                    cx.emit(TerminalCommandSuggestionMenuEvent::Retry {
                        request_generation: generation,
                    });
                }
                TerminalSuggestionQueryState::Closed
                | TerminalSuggestionQueryState::Loading
                | TerminalSuggestionQueryState::Empty => {}
            },
            TerminalCommandSuggestionMenuKey::Escape => {
                cx.emit(TerminalCommandSuggestionMenuEvent::Dismiss {
                    request_generation: generation,
                });
            }
        }
        true
    }

    /// 鼠标选择仅在当前 Ready projection 中接受候选，并将 generation 一并交给 Root 校验。
    fn emit_accept(&self, request_generation: u64, candidate_id: String, cx: &mut Context<Self>) {
        if self.projection.state != TerminalSuggestionQueryState::Ready
            || self.projection.request_generation != request_generation
            || !self
                .selection
                .candidate_ids
                .iter()
                .any(|current| current == &candidate_id)
        {
            return;
        }
        cx.emit(TerminalCommandSuggestionMenuEvent::Accept {
            request_generation,
            candidate_id,
            partial_accept: self.projection.partial_accept,
        });
    }

    /// 当前状态只有一个菜单入口；所有文案与候选说明都作为文本绘制，不解析成富内容，说明文字沿用可读字号并限制在候选宽度内换行。
    ///
    /// @author kongweiguang
    fn render_content(&mut self, theme: ThemeTokens, cx: &mut Context<Self>) -> AnyElement {
        match self.projection.state {
            TerminalSuggestionQueryState::Loading => div()
                .id("suggestion-menu-loading")
                .px_3()
                .py_4()
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "正在查找命令建议…",
                    "Loading command suggestions…",
                ))
                .into_any_element(),
            TerminalSuggestionQueryState::Empty => div()
                .id("suggestion-menu-empty")
                .px_3()
                .py_4()
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "没有适用于当前输入的建议。",
                    "No suggestions match the current input.",
                ))
                .into_any_element(),
            TerminalSuggestionQueryState::Failed => {
                let generation = self.projection.request_generation;
                div()
                    .id("suggestion-menu-failed")
                    .px_3()
                    .py_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_color(theme.error)
                    .child(crate::locale::text(
                        "建议查询失败。可以重试或关闭菜单。",
                        "Suggestion lookup failed. Retry or close the menu.",
                    ))
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("suggestion-menu-retry")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(theme.sidebar)
                            .text_color(theme.foreground)
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                cx.emit(TerminalCommandSuggestionMenuEvent::Retry {
                                    request_generation: generation,
                                });
                                cx.notify();
                            }))
                            .child(crate::locale::text("重试", "Retry")),
                    )
                    .into_any_element()
            }
            TerminalSuggestionQueryState::Ready => {
                let generation = self.projection.request_generation;
                let mut rows = div()
                    .id("suggestion-menu-candidates")
                    .max_h(px(MAX_MENU_HEIGHT))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_1();
                for (index, candidate) in self.projection.candidates.iter().enumerate() {
                    let candidate_id = candidate.id.clone();
                    let selected_id = self.selection.selected_id();
                    let selected = selected_id == Some(candidate.id.as_str());
                    let display_text = candidate.display_text.clone();
                    let description = candidate.description.as_ref().map(|value| value.trim());
                    let provider = provider_label(candidate.provider);
                    let activation = candidate.activation;
                    let sensitivity = candidate.sensitivity;
                    let row_background = if selected { theme.sidebar } else { theme.panel };
                    let row_id = format!("suggestion-menu-candidate-{index}");
                    let title_color = if sensitivity
                        == kerminal_domain::commands::CommandSuggestionSensitivity::Dangerous
                    {
                        theme.error
                    } else {
                        theme.foreground
                    };
                    let mut row = div()
                        .id(row_id)
                        .w_full()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(row_background)
                        .border_1()
                        .border_color(if selected {
                            theme.accent
                        } else {
                            row_background
                        })
                        .on_click(cx.listener(move |menu, _: &ClickEvent, _, cx| {
                            let _ = menu.selection.select(generation, index);
                            menu.emit_accept(generation, candidate_id.clone(), cx);
                        }))
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().flex_1().text_color(title_color).child(display_text))
                                .child(
                                    div()
                                        .text_size(theme.text_xs)
                                        .text_color(theme.muted)
                                        .child(provider),
                                ),
                        );
                    if let Some(description) = description.filter(|text| !text.is_empty()) {
                        row = row.child(
                            div()
                                .w_full()
                                .mt_1()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(description.chars().take(180).collect::<String>()),
                        );
                    }
                    if activation
                        == kerminal_domain::commands::CommandSuggestionActivation::OpenSnippetPanel
                    {
                        row = row.child(
                            div()
                                .w_full()
                                .mt_1()
                                .text_size(theme.text_xs)
                                .text_color(theme.accent)
                                .child(crate::locale::text(
                                    "选择后打开片段面板",
                                    "Selecting opens the snippets panel",
                                )),
                        );
                    }
                    rows = rows.child(row);
                }
                rows.into_any_element()
            }
            TerminalSuggestionQueryState::Closed => div().into_any_element(),
        }
    }
}

impl Render for TerminalCommandSuggestionMenu {
    /// 明暗主题共用 Kit 令牌；菜单仅在 Root 持有的活动投影范围内可见。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.projection.state == TerminalSuggestionQueryState::Closed {
            return div().id("suggestion-menu-closed");
        }
        let theme = ThemeTokens::current(window, cx);
        let generation = self.projection.request_generation;
        div()
            .id("terminal-command-suggestion-menu")
            .w(px(440.))
            .max_h(px(MAX_MENU_HEIGHT + 48.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded_lg()
            .border_1()
            .border_color(theme.muted)
            .bg(theme.panel)
            .text_color(theme.foreground)
            .child(
                div()
                    .w_full()
                    .px_3()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .bg(theme.sidebar)
                    .child(crate::locale::text("命令建议", "Command suggestions"))
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("suggestion-menu-dismiss")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_color(theme.muted)
                            .hover(|style| style.bg(theme.panel).text_color(theme.foreground))
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                cx.emit(TerminalCommandSuggestionMenuEvent::Dismiss {
                                    request_generation: generation,
                                });
                            }))
                            .child("×"),
                    ),
            )
            .child(self.render_content(theme, cx))
    }
}

/// 将 provider 枚举映射成不暴露 source ID 的短标签。
fn provider_label(provider: SuggestionProviderKind) -> gpui_kit::SharedString {
    match provider {
        SuggestionProviderKind::History => crate::locale::text("历史", "History"),
        SuggestionProviderKind::RemotePath => crate::locale::text("远程路径", "Remote path"),
        SuggestionProviderKind::RemoteCommand => crate::locale::text("远程命令", "Remote command"),
        SuggestionProviderKind::Git => "Git".into(),
        SuggestionProviderKind::Spec => crate::locale::text("规范", "Spec"),
        SuggestionProviderKind::Snippet => crate::locale::text("片段", "Snippet"),
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/root_command_suggestion_menu.rs"]
mod tests;
