//! 命令建议的原生菜单视图；查询、过滤和接受仍由 Root 的唯一 controller 负责。
//!
//! @author kongweiguang

use std::sync::Arc;

use gpui_kit::{
    AnyElement, ClickEvent, Context, EventEmitter, IntoElement, Render, ScrollHandle, Window,
    assets::IconName,
    component::button::{Button, ButtonVariants},
    component::tooltip::Tooltip,
    div,
    prelude::*,
    px,
};
use kerminal_application::sessions::ShellEditingUnavailableReason;
use kerminal_domain::commands::{CommandSuggestionCandidate, SuggestionProviderKind};

use super::TerminalSuggestionQueryState;
use crate::theme::ThemeTokens;

const MAX_MENU_ROWS_REM: f32 = 15.0;
const MAX_MENU_REM: f32 = 18.0;

/// Root 提供短期只读候选快照；候选业务状态与接受权限不会复制到菜单。
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalSuggestionMenuProjection {
    pub request_generation: u64,
    pub state: TerminalSuggestionQueryState,
    pub partial_accept: bool,
    pub candidates: Arc<[CommandSuggestionCandidate]>,
    pub(super) selection_keys: Arc<[String]>,
    pub(super) partial_failure: bool,
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
    candidate_keys: Vec<String>,
    selected_index: Option<usize>,
    explicitly_selected: bool,
}

impl MenuSelection {
    /// 同代结果必须保留追加内容，ID 相同时也不能把显式选择转移到已变化的命令。
    fn reconcile(
        &mut self,
        generation: u64,
        candidates: &[CommandSuggestionCandidate],
        keys: &[String],
    ) {
        let same_generation = self.generation == Some(generation);
        let selected_id = self
            .selected_index
            .filter(|_| same_generation)
            .and_then(|index| self.candidate_ids.get(index));
        let selected_key = self
            .selected_index
            .filter(|_| same_generation)
            .and_then(|index| self.candidate_keys.get(index));
        let candidate_ids = candidates
            .iter()
            .map(|candidate| candidate.id.clone())
            .collect::<Vec<_>>();
        let retained_index = if same_generation {
            selected_key.and_then(|selected_key| {
                selected_id
                    .and_then(|selected_id| {
                        candidate_ids
                            .iter()
                            .zip(keys)
                            .position(|(id, key)| id == selected_id && key == selected_key)
                    })
                    .or_else(|| {
                        keys.iter()
                            .take(candidate_ids.len())
                            .position(|key| key == selected_key)
                    })
            })
        } else {
            self.explicitly_selected = false;
            None
        };
        self.selected_index = retained_index
            .or_else(|| (!self.explicitly_selected && !candidate_ids.is_empty()).then_some(0));
        self.generation = Some(generation);
        self.candidate_ids = candidate_ids;
        self.candidate_keys = keys.to_vec();
    }

    /// 在非空列表中环绕移动；无选择时由方向键明确选中首项或末项。
    fn move_by(&mut self, delta: isize) -> bool {
        let count = self.candidate_ids.len();
        if count == 0 {
            return false;
        }
        self.selected_index = Some(match (self.selected_index, delta < 0) {
            (Some(index), true) => (index + count - 1) % count,
            (Some(index), false) => (index + 1) % count,
            (None, true) => count - 1,
            (None, false) => 0,
        });
        self.explicitly_selected = true;
        true
    }

    /// 只对当前 generation 的合法索引记录用户显式选择。
    fn select(&mut self, generation: u64, index: usize) -> bool {
        if self.generation != Some(generation) || index >= self.candidate_ids.len() {
            return false;
        }
        self.selected_index = Some(index);
        self.explicitly_selected = true;
        true
    }

    /// 从只读 projection 中按当前高亮项解析候选 ID。
    fn selected_id(&self) -> Option<&str> {
        self.selected_index
            .and_then(|index| self.candidate_ids.get(index))
            .map(String::as_str)
    }
}

/// 菜单视图只持有最近一次投影和键盘高亮，不持有服务或终端资源。
pub struct TerminalCommandSuggestionMenu {
    projection: TerminalSuggestionMenuProjection,
    selection: MenuSelection,
    scroll_handle: ScrollHandle,
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
        let keys = if projection.state == TerminalSuggestionQueryState::Ready {
            projection.selection_keys.as_ref()
        } else {
            &[]
        };
        selection.reconcile(projection.request_generation, candidates, keys);
        Self {
            projection,
            selection,
            scroll_handle: ScrollHandle::new(),
        }
    }

    /// 更新外部唯一候选投影；仅在高亮位置变化时请求滚动，避免后台增量结果打断用户手动浏览。
    pub fn set_projection(
        &mut self,
        projection: TerminalSuggestionMenuProjection,
        cx: &mut Context<Self>,
    ) {
        let previous_generation = self.projection.request_generation;
        let previous_state = self.projection.state;
        let previous_index = self.selection.selected_index;
        let candidates = if projection.state == TerminalSuggestionQueryState::Ready {
            projection.candidates.as_ref()
        } else {
            &[]
        };
        let keys = if projection.state == TerminalSuggestionQueryState::Ready {
            projection.selection_keys.as_ref()
        } else {
            &[]
        };
        self.selection
            .reconcile(projection.request_generation, candidates, keys);
        if projection.state == TerminalSuggestionQueryState::Ready
            && (previous_generation != projection.request_generation
                || previous_state != TerminalSuggestionQueryState::Ready
                || previous_index != self.selection.selected_index)
            && let Some(selected_index) = self.selection.selected_index
        {
            self.scroll_handle.scroll_to_item(selected_index);
        }
        self.projection = projection;
        cx.notify();
    }

    /// 无可选项时把方向键交还 Shell，但仍拦截 Enter 以免误提交终端命令。
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
                if self.projection.state != TerminalSuggestionQueryState::Ready
                    || !self.selection.move_by(-1)
                {
                    return false;
                }
                if let Some(selected_index) = self.selection.selected_index {
                    self.scroll_handle.scroll_to_item(selected_index);
                }
                cx.notify();
            }
            TerminalCommandSuggestionMenuKey::Down => {
                if self.projection.state != TerminalSuggestionQueryState::Ready
                    || !self.selection.move_by(1)
                {
                    return false;
                }
                if let Some(selected_index) = self.selection.selected_index {
                    self.scroll_handle.scroll_to_item(selected_index);
                }
                cx.notify();
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
                | TerminalSuggestionQueryState::Empty
                | TerminalSuggestionQueryState::InputTooLong
                | TerminalSuggestionQueryState::Unavailable(_) => {}
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

    /// 菜单仅展示投影文本；截断和有界悬浮说明避免长候选挤占来源与其他操作。
    /// 列表上限跟随界面字体缩放，高亮使用共享的选中表面并保持行几何稳定。
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
                    .child(div().min_w_0().flex_1().child(crate::locale::text(
                        "建议查询失败。可以重试或关闭菜单。",
                        "Suggestion lookup failed. Retry or close the menu.",
                    )))
                    .child(
                        Button::new("suggestion-menu-retry")
                            .compact()
                            .ghost()
                            .label(crate::locale::text("重试", "Retry"))
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                cx.emit(TerminalCommandSuggestionMenuEvent::Retry {
                                    request_generation: generation,
                                });
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            }
            TerminalSuggestionQueryState::Unavailable(reason) => div()
                .id("suggestion-menu-unavailable")
                .debug_selector(|| "suggestion-menu-unavailable".to_owned())
                .px_3()
                .py_3()
                .text_color(theme.muted)
                .child(unavailable_message(reason))
                .into_any_element(),
            TerminalSuggestionQueryState::InputTooLong => div()
                .id("suggestion-menu-input-too-long")
                .debug_selector(|| "suggestion-menu-input-too-long".to_owned())
                .px_3()
                .py_3()
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "当前输入过长，缩短后会恢复建议。",
                    "Shorten the input to restore suggestions.",
                ))
                .into_any_element(),
            TerminalSuggestionQueryState::Ready => {
                let generation = self.projection.request_generation;
                let mut rows = div()
                    .id("suggestion-menu-candidates")
                    .max_h(gpui_kit::rems(MAX_MENU_ROWS_REM))
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_1();
                for (index, candidate) in self.projection.candidates.iter().enumerate() {
                    let candidate_id = candidate.id.clone();
                    let selection_key = self.projection.selection_keys.get(index).cloned();
                    let selected_id = self.selection.selected_id();
                    let selected = selected_id == Some(candidate.id.as_str());
                    let display_text = candidate.display_text.clone();
                    let description = candidate_description(candidate);
                    let provider = provider_label(candidate.provider);
                    let activation = candidate.activation;
                    let sensitivity = candidate.sensitivity;
                    let row_background = if selected {
                        theme.surface_selected()
                    } else {
                        theme.panel
                    };
                    let row_id = format!(
                        "suggestion-menu-candidate-{generation}-{}:{}:{selection_key:?}",
                        candidate.id.len(),
                        candidate.id,
                    );
                    let title_color = if sensitivity
                        == kerminal_domain::commands::CommandSuggestionSensitivity::Dangerous
                    {
                        theme.error
                    } else {
                        theme.foreground
                    };
                    let mut row = div()
                        .id(row_id)
                        .debug_selector(move || format!("suggestion-menu-candidate-{index}"))
                        .w_full()
                        .px_3()
                        .py_2()
                        .rounded(theme.control_radius)
                        .bg(row_background)
                        .when(!selected, |row| {
                            row.hover(|style| style.bg(theme.surface_hover()))
                        })
                        .on_click(cx.listener(move |menu, _: &ClickEvent, _, cx| {
                            if menu.selection.candidate_ids.get(index) == Some(&candidate_id)
                                && selection_key.as_ref().is_some_and(|key| {
                                    menu.selection.candidate_keys.get(index) == Some(key)
                                })
                                && menu.selection.select(generation, index)
                            {
                                menu.emit_accept(generation, candidate_id.clone(), cx);
                            }
                        }))
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .id("suggestion-menu-candidate-title")
                                        .debug_selector(move || {
                                            format!("suggestion-menu-candidate-title-{index}")
                                        })
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(title_color)
                                        .tooltip({
                                            let text = bounded_tooltip_text(&display_text);
                                            move |window, cx| {
                                                let size = suggestion_tooltip_max_size(
                                                    window.viewport_size(),
                                                    window.rem_size(),
                                                );
                                                Tooltip::new(text.clone())
                                                    .max_w(size.width)
                                                    .max_h(size.height)
                                                    .overflow_hidden()
                                                    .build(window, cx)
                                            }
                                        })
                                        .child(display_text),
                                )
                                .child(
                                    div()
                                        .id("suggestion-menu-candidate-provider")
                                        .debug_selector(move || {
                                            format!("suggestion-menu-candidate-provider-{index}")
                                        })
                                        .flex_shrink_0()
                                        .text_size(theme.text_xs)
                                        .text_color(theme.muted)
                                        .child(provider),
                                ),
                        );
                    if let Some(description) = description {
                        let tooltip_text = bounded_tooltip_text(description);
                        row = row.child(
                            div()
                                .id("suggestion-menu-candidate-description")
                                .debug_selector(move || {
                                    format!("suggestion-menu-candidate-description-{index}")
                                })
                                .w_full()
                                .min_w_0()
                                .mt_1()
                                .truncate()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .tooltip(move |window, cx| {
                                    let size = suggestion_tooltip_max_size(
                                        window.viewport_size(),
                                        window.rem_size(),
                                    );
                                    Tooltip::new(tooltip_text.clone())
                                        .max_w(size.width)
                                        .max_h(size.height)
                                        .overflow_hidden()
                                        .build(window, cx)
                                })
                                .child(description.to_owned()),
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
    /// 明暗主题共用浮层与面板头令牌；视口和字号共同约束尺寸，不改变终端焦点及目标。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.projection.state == TerminalSuggestionQueryState::Closed {
            return div().id("suggestion-menu-closed");
        }
        let theme = ThemeTokens::current(window, cx);
        let generation = self.projection.request_generation;
        div()
            .id("terminal-command-suggestion-menu")
            .debug_selector(|| "terminal-command-suggestion-menu".to_owned())
            .w_full()
            .max_h((window.rem_size() * MAX_MENU_REM).min(window.viewport_size().height))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(theme.radius_panel)
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .text_color(theme.foreground)
            .child(
                crate::ui_surface::panel_header(theme)
                    .w_full()
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .child(crate::locale::text("命令建议", "Command suggestions")),
                    )
                    .child(
                        Button::new("suggestion-menu-dismiss")
                            .compact()
                            .ghost()
                            .icon(IconName::X)
                            .accessibility_label(crate::locale::text(
                                "关闭建议",
                                "Close suggestions",
                            ))
                            .tooltip(crate::locale::text("关闭建议", "Close suggestions"))
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                cx.emit(TerminalCommandSuggestionMenuEvent::Dismiss {
                                    request_generation: generation,
                                });
                            })),
                    ),
            )
            .child(self.render_content(theme, cx))
            .when(
                self.projection.partial_failure
                    && self.projection.state == TerminalSuggestionQueryState::Ready,
                |menu| {
                    menu.child(
                        div()
                            .w_full()
                            .flex_shrink_0()
                            .px_3()
                            .py_1()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .truncate()
                                    .child(crate::locale::text(
                                        "部分来源暂不可用",
                                        "Some sources are unavailable",
                                    )),
                            )
                            .child(
                                Button::new("suggestion-menu-retry-partial")
                                    .compact()
                                    .ghost()
                                    .label(crate::locale::text("重试", "Retry"))
                                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                        cx.emit(TerminalCommandSuggestionMenuEvent::Retry {
                                            request_generation: generation,
                                        });
                                    })),
                            ),
                    )
                },
            )
    }
}

/// 说明层按当前窗口与 rem 缩放并留出边距，避免长命令或说明在小窗口越界。
///
/// @author kongweiguang
fn suggestion_tooltip_max_size(
    viewport: gpui_kit::Size<gpui_kit::Pixels>,
    rem: gpui_kit::Pixels,
) -> gpui_kit::Size<gpui_kit::Pixels> {
    gpui_kit::size(
        (viewport.width - rem * 2.).max(px(0.)).min(rem * 22.5),
        (viewport.height - rem * 2.).max(px(0.)).min(rem * 10.),
    )
}

/// 历史提供方的固定说明与来源标签重复；其他说明保留，以免隐藏实际上下文。
fn candidate_description(candidate: &CommandSuggestionCandidate) -> Option<&str> {
    candidate
        .description
        .as_deref()
        .map(str::trim)
        .filter(|text| {
            !text.is_empty()
                && !(candidate.provider == SuggestionProviderKind::History && *text == "命令历史")
        })
}

/// 超长文本只在悬停时展示有界预览，避免 tooltip 抢占整个终端或构造无界布局。
fn bounded_tooltip_text(text: &str) -> String {
    const MAX_TOOLTIP_CHARS: usize = 512;
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(MAX_TOOLTIP_CHARS).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

/// 根据已确认的能力状态提示可行恢复路径；未知错误不在此被包装成可重复的操作。
fn unavailable_message(reason: ShellEditingUnavailableReason) -> gpui_kit::SharedString {
    match reason {
        ShellEditingUnavailableReason::Unsupported => crate::locale::text(
            "当前 Shell 不支持命令建议，普通终端输入仍可使用。",
            "Command suggestions are unavailable for this shell. Terminal input still works.",
        ),
        ShellEditingUnavailableReason::IntegrationNotObserved => crate::locale::text(
            "未检测到 Shell 集成，可检查启动配置或重新连接。",
            "Shell integration was not detected. Check the shell setup or reconnect.",
        ),
        ShellEditingUnavailableReason::KeyBindingConflict => crate::locale::text(
            "Shell 快捷键已被占用，原有绑定已保留。可检查绑定后重新连接。",
            "A shell key binding is already in use and was preserved. Check it and reconnect.",
        ),
        ShellEditingUnavailableReason::PromptNotReady => crate::locale::text(
            "Shell 尚未准备好输入，请返回命令提示符后再次打开建议。",
            "The shell is not ready for input. Return to the prompt and reopen suggestions.",
        ),
        ShellEditingUnavailableReason::ProbeTimedOut => crate::locale::text(
            "读取 Shell 编辑状态超时，请新建终端或重新连接。",
            "The shell did not respond. Open a new terminal or reconnect.",
        ),
        ShellEditingUnavailableReason::SessionChanged => crate::locale::text(
            "终端会话已变化，请在目标终端重新打开建议。",
            "The terminal session changed. Reopen suggestions in the intended terminal.",
        ),
    }
}

/// 将 provider 枚举映射成不暴露 source ID 的短标签。
fn provider_label(provider: SuggestionProviderKind) -> gpui_kit::SharedString {
    match provider {
        SuggestionProviderKind::History => crate::locale::text("历史", "History"),
        SuggestionProviderKind::LocalPath => crate::locale::text("本地路径", "Local path"),
        SuggestionProviderKind::LocalCommand => crate::locale::text("本地命令", "Local command"),
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
