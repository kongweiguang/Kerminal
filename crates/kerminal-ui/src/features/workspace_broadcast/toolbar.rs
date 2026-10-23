//! 分屏命令栏维护按 Tab 投影的草稿与动作意图，终端写入统一由工作台动作链执行。
//!
//! @author kongweiguang

use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, EntityInputHandler, EventEmitter, Focusable as _,
    IntoElement, MouseButton, Render, ScrollStrategy, StyleRefinement, Subscription,
    UniformListScrollHandle, Window,
    assets::IconName,
    base::Selectable as _,
    component::{
        Disableable,
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
        input::{Input, InputEvent, InputState},
        popover::Popover,
        switch::Switch,
    },
    div,
    prelude::*,
    px, uniform_list,
};
use kerminal_domain::identity::{PaneId, TabId};

use super::WorkspaceBroadcastTargetMode;
use crate::theme::ThemeTokens;

/// Root 每次渲染投影一份有界展示状态；异步执行和业务校验仍由 Root 持有。
///
/// @author kongweiguang
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct WorkspaceBroadcastBarProjection {
    pub tab_id: Option<TabId>,
    pub pane_count: usize,
    pub selected_count: usize,
    pub targets: Vec<WorkspaceBroadcastTargetProjection>,
    pub mode: WorkspaceBroadcastTargetMode,
    pub draft: String,
    pub sync_enabled: bool,
    pub command_pending: bool,
    pub execution_busy: bool,
    pub confirmation_required: bool,
    pub can_cancel: bool,
    pub can_retry_safe: bool,
    pub result_details: Vec<WorkspaceBroadcastDeliveryProjection>,
    pub results_open: bool,
    pub snippets_open: bool,
    pub snippets: Vec<WorkspaceBroadcastSnippetProjection>,
    pub status: Option<String>,
}

/// Root 投影当前 Tab 的 pane 展示和可选状态；组件不从 pane 自行读取运行态。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceBroadcastTargetProjection {
    pub pane_id: PaneId,
    pub title: String,
    pub detail: String,
    pub protocol: String,
    pub selected: bool,
    pub enabled: bool,
    pub current: bool,
}

/// 片段选择只发出稳定意图，由 Root 统一完成发送与结果回写，避免组件绕过广播生命周期。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceBroadcastSnippetProjection {
    pub id: String,
    pub title: String,
}

/// 每个目标单独投影发送确定性，避免部分失败被汇总成全成功。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceBroadcastDeliveryProjection {
    pub title: String,
    pub state: String,
}

/// 工具栏事件只携带稳定 Tab 身份与用户意图；Root 在异步边界重新检查目标代次。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceBroadcastBarEvent {
    DraftChanged {
        tab_id: TabId,
        draft: String,
    },
    SetMode {
        tab_id: TabId,
        mode: WorkspaceBroadcastTargetMode,
    },
    ToggleTarget {
        tab_id: TabId,
        pane_id: PaneId,
    },
    ToggleSync {
        tab_id: TabId,
    },
    SendCommand {
        tab_id: TabId,
    },
    ConfirmSend {
        tab_id: TabId,
    },
    Interrupt {
        tab_id: TabId,
    },
    CancelCommand {
        tab_id: TabId,
    },
    RetrySafeFailures {
        tab_id: TabId,
    },
    ToggleResults {
        tab_id: TabId,
    },
    OpenSnippets {
        tab_id: TabId,
    },
    CloseSnippets {
        tab_id: TabId,
    },
    ChooseSnippet {
        tab_id: TabId,
        snippet_id: String,
    },
}

/// 模式行使用固定次序；人数按可发送或当前有效目标计算，避免把布局总数误报成发送数。
///
/// @author kongweiguang
fn target_mode_option_labels(
    projection: &WorkspaceBroadcastBarProjection,
) -> [(WorkspaceBroadcastTargetMode, String); 3] {
    let current_title = projection
        .targets
        .iter()
        .find(|target| target.current)
        .map(|target| target.title.as_str())
        .filter(|title| !title.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| crate::locale::text("无当前分屏", "No current pane").to_string());
    let all_count = projection
        .targets
        .iter()
        .filter(|target| target.enabled)
        .count();
    [
        (
            WorkspaceBroadcastTargetMode::Current,
            crate::locale::text(
                format!("当前分屏 · {current_title}"),
                format!("Current pane · {current_title}"),
            )
            .to_string(),
        ),
        (
            WorkspaceBroadcastTargetMode::All,
            crate::locale::text(
                format!("全部分屏 · {all_count} 个目标"),
                format!("All panes · {all_count} targets"),
            )
            .to_string(),
        ),
        (
            WorkspaceBroadcastTargetMode::Custom,
            crate::locale::text(
                format!("自定义 · {} 个已选", projection.selected_count),
                format!("Custom · {} selected", projection.selected_count),
            )
            .to_string(),
        ),
    ]
}

/// 触发区保持短且稳定，当前 pane 的完整标题放在弹层选项中以免挤压命令输入。
///
/// @author kongweiguang
fn target_mode_trigger_label(projection: &WorkspaceBroadcastBarProjection) -> String {
    let count = match projection.mode {
        WorkspaceBroadcastTargetMode::All => projection
            .targets
            .iter()
            .filter(|target| target.enabled)
            .count(),
        WorkspaceBroadcastTargetMode::Current | WorkspaceBroadcastTargetMode::Custom => {
            projection.selected_count
        }
    };
    let mode = match projection.mode {
        WorkspaceBroadcastTargetMode::All => {
            crate::locale::text("全部分屏", "All panes").to_string()
        }
        WorkspaceBroadcastTargetMode::Current => {
            crate::locale::text("当前分屏", "Current pane").to_string()
        }
        WorkspaceBroadcastTargetMode::Custom => crate::locale::text("自定义", "Custom").to_string(),
    };
    format!("{mode} · {count}")
}

/// 输入实体可以跨 workspace render 重用；projection 的 Tab id 门禁阻止旧 UI 回调污染新草稿。
pub(crate) struct WorkspaceBroadcastBar {
    draft: Entity<InputState>,
    snippet_search: Entity<InputState>,
    snippet_query: String,
    snippet_scroll: UniformListScrollHandle,
    target_mode_open: bool,
    projection: WorkspaceBroadcastBarProjection,
    _draft_subscription: Subscription,
    _snippet_subscription: Subscription,
}

impl EventEmitter<WorkspaceBroadcastBarEvent> for WorkspaceBroadcastBar {}

impl WorkspaceBroadcastBar {
    /// 在拥有 GPUI Window 的创建点构造输入状态，Root 仅替换投影而不在后台初始化控件。
    ///
    /// @author kongweiguang
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let draft = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text(
                "向所选分屏发送命令…",
                "Send a command to selected panes…",
            ))
        });
        let draft_for_event = draft.clone();
        let draft_subscription = cx.subscribe_in(
            &draft,
            window,
            move |bar, _, event, window, cx| match event {
                InputEvent::Change => {
                    let draft = draft_for_event.read(cx).value().to_string();
                    let Some(tab_id) = bar.projection.tab_id.clone() else {
                        return;
                    };
                    if draft != bar.projection.draft {
                        bar.projection.draft = draft.clone();
                        cx.emit(WorkspaceBroadcastBarEvent::DraftChanged { tab_id, draft });
                    }
                    cx.notify();
                }
                InputEvent::PressEnter {
                    secondary: false,
                    shift: false,
                } => {
                    let composing = draft_for_event.update(cx, |input, cx| {
                        input.marked_text_range(window, cx).is_some()
                    });
                    if !composing && let Some(tab_id) = bar.projection.tab_id.clone() {
                        cx.emit(WorkspaceBroadcastBarEvent::SendCommand { tab_id });
                    }
                }
                InputEvent::PressEnter { .. } | InputEvent::Focus | InputEvent::Blur => {}
            },
        );
        let snippet_search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::locale::text("搜索命令片段…", "Search snippets…"))
        });
        let snippet_for_event = snippet_search.clone();
        let snippet_subscription =
            cx.subscribe_in(&snippet_search, window, move |bar, _, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    bar.snippet_query = snippet_for_event.read(cx).value().to_string();
                    bar.snippet_scroll.scroll_to_item(0, ScrollStrategy::Top);
                    cx.notify();
                }
            });
        Self {
            draft,
            snippet_search,
            snippet_query: String::new(),
            snippet_scroll: UniformListScrollHandle::new(),
            target_mode_open: false,
            projection: WorkspaceBroadcastBarProjection::default(),
            _draft_subscription: draft_subscription,
            _snippet_subscription: snippet_subscription,
        }
    }

    /// Root 的 per-Tab 草稿是唯一真值；更新控件前先更新比较基线，忽略程序化 Change 回声。
    ///
    /// @author kongweiguang
    pub(crate) fn set_projection(
        &mut self,
        projection: WorkspaceBroadcastBarProjection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.projection == projection {
            return;
        }
        let tab_changed = self.projection.tab_id != projection.tab_id;
        let reset_snippet_search =
            tab_changed || (!self.projection.snippets_open && projection.snippets_open);
        if self.projection.draft != projection.draft {
            let draft = projection.draft.clone();
            self.projection = projection;
            self.draft.update(cx, |input, cx| {
                if tab_changed {
                    input.unmark_text(window, cx);
                }
                input.set_value(draft, window, cx);
            });
        } else {
            self.projection = projection;
            if tab_changed {
                self.draft
                    .update(cx, |input, cx| input.unmark_text(window, cx));
            }
        }
        if reset_snippet_search {
            self.target_mode_open = false;
            self.snippet_query.clear();
            self.snippet_scroll.scroll_to_item(0, ScrollStrategy::Top);
            self.snippet_search.update(cx, |input, cx| {
                input.unmark_text(window, cx);
                input.set_value("", window, cx);
            });
        }
        cx.notify();
    }

    /// 展开入口卸载前会失去焦点；重开时只恢复到仍然可见的同一 Tab 草稿。
    ///
    /// @author kongweiguang
    pub(crate) fn focus_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.draft.update(cx, |input, cx| input.focus(window, cx));
    }

    /// IME composition 期间不改焦点或提交命令，确保输入法的 Enter 只完成组合文本。
    ///
    /// @author kongweiguang
    fn is_composing(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.draft.update(cx, |input, cx| {
            input.marked_text_range(window, cx).is_some()
        })
    }

    /// 三种目标模式与 pane 勾选放入同一弹层，切换模式后保留弹层便于连续微调目标。
    ///
    /// @author kongweiguang
    fn render_target_mode(
        &self,
        tab_id: &TabId,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Popover {
        let weak = cx.weak_entity();
        let event_tab = tab_id.clone();
        let active_mode = self.projection.mode;
        let labels = target_mode_option_labels(&self.projection);
        let targets = std::rc::Rc::new(self.projection.targets.clone());
        let trigger_label = target_mode_trigger_label(&self.projection);
        let viewport_width = window.bounds().size.width.to_f64();
        let viewport_height = window.bounds().size.height.to_f64();
        let width = px((viewport_width - 24.).clamp(0., 360.) as f32);
        let max_height = px((viewport_height - 32.).clamp(160., 560.) as f32);
        let list_height = px(((targets.len().max(1) as f32 * 48.)
            .min(max_height.to_f64() as f32 - 116.))
        .max(44.));
        let theme = ThemeTokens::current(window, cx);
        Popover::new("workspace-broadcast-mode-picker")
            .trigger(
                Button::new("workspace-broadcast-mode")
                    .debug_selector(|| "workspace-broadcast-mode-trigger".to_owned())
                    .compact()
                    .ghost()
                    .label(trigger_label),
            )
            .trigger_style(StyleRefinement::default().flex_none())
            .open(self.target_mode_open)
            .on_open_change(cx.listener(|bar, open, _, cx| {
                bar.target_mode_open = *open;
                cx.notify();
            }))
            .content(move |_, _, _| {
                let mode_rows = labels
                    .iter()
                    .cloned()
                    .map(|(mode, label)| {
                        let weak = weak.clone();
                        let tab_id = event_tab.clone();
                        Button::new(format!("workspace-broadcast-mode-{mode:?}"))
                            .debug_selector(move || {
                                format!("workspace-broadcast-mode-option-{mode:?}")
                            })
                            .compact()
                            .ghost()
                            .w_full()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .justify_start()
                            .selected(active_mode == mode)
                            .tooltip(label.clone())
                            .label(label)
                            .on_click(move |_, _, app| {
                                if let Some(bar) = weak.upgrade() {
                                    bar.update(app, |_, cx| {
                                        cx.emit(WorkspaceBroadcastBarEvent::SetMode {
                                            tab_id: tab_id.clone(),
                                            mode,
                                        });
                                        cx.notify();
                                    });
                                }
                            })
                    })
                    .collect::<Vec<_>>();
                let target_rows = targets
                    .iter()
                    .map(|target| {
                        let weak = weak.clone();
                        let tab_id = event_tab.clone();
                        let pane_id = target.pane_id.clone();
                        let title = target.title.clone();
                        let target_selector =
                            format!("workspace-broadcast-target-{}", target.pane_id.as_str());
                        let mut detail = target.detail.trim().to_string();
                        if !target.protocol.trim().is_empty() {
                            if detail.is_empty() {
                                detail = target.protocol.clone();
                            } else {
                                detail.push_str(" · ");
                                detail.push_str(target.protocol.trim());
                            }
                        }
                        Checkbox::new(format!(
                            "workspace-broadcast-target-{}",
                            target.pane_id.as_str()
                        ))
                        .debug_selector(move || target_selector.clone())
                        .w_full()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .min_h(px(44.))
                        .px_2()
                        .py_1()
                        .checked(target.selected)
                        .disabled(!target.enabled)
                        .accessibility_label(crate::locale::text(
                            format!("选择群发目标：{title}"),
                            format!("Select broadcast target: {title}"),
                        ))
                        .tooltip(title.clone())
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_change(move |_, _, app| {
                            if let Some(bar) = weak.upgrade() {
                                bar.update(app, |_, cx| {
                                    cx.emit(WorkspaceBroadcastBarEvent::ToggleTarget {
                                        tab_id: tab_id.clone(),
                                        pane_id: pane_id.clone(),
                                    });
                                });
                            }
                        })
                        .child(div().w_full().min_w(px(0.)).truncate().child(title))
                        .when(!detail.is_empty(), |checkbox| {
                            checkbox.child(
                                div()
                                    .w_full()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(detail),
                            )
                        })
                    })
                    .collect::<Vec<_>>();
                let mut panel = div()
                    .w(width)
                    .max_w(width)
                    .max_h(max_height)
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_1()
                    .overflow_hidden()
                    .children(mode_rows)
                    .child(div().my_1().h(px(1.)).w_full().bg(theme.border));
                if target_rows.is_empty() {
                    panel = panel.child(
                        div()
                            .min_h(px(44.))
                            .px_2()
                            .flex()
                            .items_center()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "当前没有可选分屏",
                                "No panes to select",
                            )),
                    );
                } else {
                    panel = panel.child(
                        div()
                            .id("workspace-broadcast-target-list")
                            .w_full()
                            .h(list_height)
                            .min_h(px(0.))
                            .flex()
                            .flex_col()
                            .overflow_y_scroll()
                            .children(target_rows),
                    );
                }
                panel
            })
    }

    /// 片段目录独立滚动并虚拟化，不裁剪候选，也不把目录展开高度计入终端画布。
    /// 搜索实体随工具栏保留；选择只提交稳定片段身份，发送与结果回写由 Root 统一处理。
    ///
    /// @author kongweiguang
    fn render_snippet_picker(&self, tab_id: &TabId, cx: &mut Context<Self>) -> Popover {
        let query = self.snippet_query.trim().to_lowercase();
        let snippets = std::rc::Rc::new(
            self.projection
                .snippets
                .iter()
                .filter(|snippet| snippet.title.to_lowercase().contains(&query))
                .cloned()
                .collect::<Vec<_>>(),
        );
        let search = self.snippet_search.clone();
        let focus = search.read(cx).focus_handle(cx);
        let scroll = self.snippet_scroll.clone();
        let weak = cx.weak_entity();
        let event_tab = tab_id.clone();
        Popover::new("workspace-broadcast-snippet-picker")
            .trigger(
                Button::new("workspace-broadcast-snippets")
                    .compact()
                    .ghost()
                    .label(crate::locale::text("片段", "Snippets")),
            )
            .trigger_style(StyleRefinement::default().flex_none())
            .track_focus(&focus)
            .open(self.projection.snippets_open)
            .on_open_change(cx.listener(move |_, open, _, cx| {
                cx.emit(if *open {
                    WorkspaceBroadcastBarEvent::OpenSnippets {
                        tab_id: event_tab.clone(),
                    }
                } else {
                    WorkspaceBroadcastBarEvent::CloseSnippets {
                        tab_id: event_tab.clone(),
                    }
                });
            }))
            .content({
                let event_tab = tab_id.clone();
                move |_, window, _| {
                    let width =
                        px((window.bounds().size.width.to_f64() - 64.).clamp(120., 360.) as f32);
                    let max_height =
                        (window.bounds().size.height.to_f64() - 180.).clamp(72., 280.) as f32;
                    let height = px((snippets.len().max(1) as f32 * 32.).min(max_height));
                    let rows = snippets.clone();
                    let list_weak = weak.clone();
                    let row_tab = event_tab.clone();
                    let list = uniform_list(
                        "workspace-broadcast-snippet-rows",
                        rows.len(),
                        move |range, _, _| {
                            range
                                .map(|index| {
                                    let snippet = rows[index].clone();
                                    let selector =
                                        format!("workspace-broadcast-snippet-row-{}", snippet.id);
                                    let weak = list_weak.clone();
                                    let tab_id = row_tab.clone();
                                    Button::new(format!(
                                        "workspace-broadcast-snippet-{}",
                                        snippet.id
                                    ))
                                    .debug_selector(move || selector.clone())
                                    .ghost()
                                    .compact()
                                    .w_full()
                                    .h(px(32.))
                                    .min_w(px(0.))
                                    .justify_start()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .label(snippet.title.clone())
                                    .tooltip(snippet.title)
                                    .on_click(
                                        move |_, _, app| {
                                            if let Some(bar) = weak.upgrade() {
                                                bar.update(app, |_, cx| {
                                                    cx.emit(
                                                        WorkspaceBroadcastBarEvent::ChooseSnippet {
                                                            tab_id: tab_id.clone(),
                                                            snippet_id: snippet.id.clone(),
                                                        },
                                                    );
                                                });
                                            }
                                        },
                                    )
                                })
                                .collect::<Vec<_>>()
                        },
                    )
                    .h(height)
                    .w_full()
                    .track_scroll(&scroll);
                    let composition_input = search.clone();
                    let enter_input = search.clone();
                    let enter_weak = weak.clone();
                    let enter_tab = event_tab.clone();
                    div()
                        .key_context(crate::features::input_composition::PICKER_INPUT_CONTEXT)
                        .w(width)
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .capture_action(
                            move |_: &gpui_kit::component::input::Escape, window, cx| {
                                crate::features::input_composition::cancel_input_composition(
                                    &composition_input,
                                    window,
                                    cx,
                                );
                            },
                        )
                        // 单行 Input 会传播 Enter 给 Popover Confirm；这里统一拥有选择动作，
                        // 避免自动关闭使异步片段回执失效，也保留 IME 对确认候选的控制。
                        .capture_action(move |_: &gpui_kit::component::input::Enter, window, cx| {
                            cx.stop_propagation();
                            if enter_input.read(cx).is_composing() {
                                return;
                            }
                            window.prevent_default();
                            if let Some(bar) = enter_weak.upgrade() {
                                bar.update(cx, |bar, cx| {
                                    if bar.projection.tab_id.as_ref() != Some(&enter_tab)
                                        || !bar.projection.snippets_open
                                    {
                                        return;
                                    }
                                    let query = bar.snippet_query.trim().to_lowercase();
                                    if let Some(snippet) =
                                        bar.projection.snippets.iter().find(|snippet| {
                                            snippet.title.to_lowercase().contains(&query)
                                        })
                                    {
                                        cx.emit(WorkspaceBroadcastBarEvent::ChooseSnippet {
                                            tab_id: enter_tab.clone(),
                                            snippet_id: snippet.id.clone(),
                                        });
                                    }
                                });
                            }
                        })
                        .child(Input::new(&search).w_full())
                        .when(snippets.is_empty(), |view| {
                            view.child(crate::locale::text(
                                "没有匹配的命令片段",
                                "No matching snippets",
                            ))
                        })
                        .child(list)
                }
            })
    }
}

impl Render for WorkspaceBroadcastBar {
    /// 状态与可取消操作固定占位，异步发送状态变化不应挤动工具栏并反复重测终端网格。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        let projection = self.projection.clone();
        let Some(tab_id) = projection.tab_id.clone() else {
            return div().into_any_element();
        };
        let mut row = div()
            .id("workspace-broadcast-bar")
            .w_full()
            .flex()
            .flex_wrap()
            .min_h(px(42.))
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .bg(theme.panel)
            .text_color(theme.foreground)
            .border_b_1()
            .border_color(theme.border);

        row = row.child(self.render_target_mode(&tab_id, window, cx));
        let composition_draft = self.draft.clone();
        row = row.child(
            div()
                .w_0()
                .flex_1()
                .min_w(px(120.))
                .debug_selector(|| "workspace-broadcast-draft-input-slot".to_owned())
                .capture_action(move |_: &gpui_kit::component::input::Escape, window, cx| {
                    crate::features::input_composition::cancel_input_composition(
                        &composition_draft,
                        window,
                        cx,
                    );
                })
                .child(Input::new(&self.draft).w_full()),
        );
        row = row.child(self.render_snippet_picker(&tab_id, cx));

        let interrupt_tab = tab_id.clone();
        row = row.child(
            Button::new("workspace-broadcast-interrupt")
                .compact()
                .ghost()
                .label("Ctrl+C")
                .disabled(projection.selected_count == 0 || projection.execution_busy)
                .tooltip(crate::locale::text(
                    "向所选终端发送 Ctrl+C",
                    "Send Ctrl+C to selected panes",
                ))
                .on_click(cx.listener(move |bar, _: &ClickEvent, window, cx| {
                    if !bar.is_composing(window, cx) {
                        cx.emit(WorkspaceBroadcastBarEvent::Interrupt {
                            tab_id: interrupt_tab.clone(),
                        });
                    }
                })),
        );

        let sync_tab = tab_id.clone();
        row = row.child(
            div()
                .debug_selector(|| "workspace-broadcast-sync-toggle".to_owned())
                .flex_none()
                .child(
                    Switch::new("workspace-broadcast-sync")
                        .checked(projection.sync_enabled)
                        .label(crate::locale::text("同步按键", "Sync input"))
                        .disabled(projection.selected_count < 2 || projection.execution_busy)
                        .flex_none()
                        .accessibility_label(crate::locale::text("同步按键", "Synchronized input"))
                        .tooltip(crate::locale::text(
                            "同步按键默认关闭",
                            "Synchronized input starts off",
                        ))
                        .on_change(cx.listener(move |_, _: &bool, _, cx| {
                            cx.emit(WorkspaceBroadcastBarEvent::ToggleSync {
                                tab_id: sync_tab.clone(),
                            });
                        })),
                ),
        );

        if projection.confirmation_required {
            let confirm_tab = tab_id.clone();
            row = row.child(
                Button::new("workspace-broadcast-confirm")
                    .compact()
                    .danger()
                    .label(crate::locale::text("确认发送", "Confirm send"))
                    .disabled(projection.execution_busy)
                    .on_click(cx.listener(move |bar, _: &ClickEvent, window, cx| {
                        if !bar.is_composing(window, cx) {
                            cx.emit(WorkspaceBroadcastBarEvent::ConfirmSend {
                                tab_id: confirm_tab.clone(),
                            });
                        }
                    })),
            );
        }

        // 发送与取消共享同一槽位，避免忙闲切换挤压输入，也不为未显示的操作留下空白按钮位。
        let send_button = if projection.command_pending && projection.can_cancel {
            let cancel_tab = tab_id.clone();
            Button::new("workspace-broadcast-cancel")
                .debug_selector(|| "workspace-broadcast-cancel-button".to_owned())
                .compact()
                .ghost()
                .w_full()
                .h_full()
                .label(crate::locale::text("取消", "Cancel"))
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    cx.emit(WorkspaceBroadcastBarEvent::CancelCommand {
                        tab_id: cancel_tab.clone(),
                    });
                }))
        } else {
            let send_tab = tab_id.clone();
            Button::new("workspace-broadcast-send")
                .compact()
                .primary()
                .w_full()
                .h_full()
                .label(crate::locale::text("发送", "Send"))
                .accessibility_label(crate::locale::text(
                    "发送到所选分屏",
                    "Send to selected panes",
                ))
                .tooltip(crate::locale::text(
                    "向所选终端发送命令",
                    "Send command to selected panes",
                ))
                .disabled(
                    projection.execution_busy
                        || projection.selected_count == 0
                        || projection.draft.trim().is_empty(),
                )
                .on_click(cx.listener(move |bar, _: &ClickEvent, window, cx| {
                    if !bar.is_composing(window, cx) {
                        cx.emit(WorkspaceBroadcastBarEvent::SendCommand {
                            tab_id: send_tab.clone(),
                        });
                    }
                }))
        };
        row = row.child(
            div()
                .w(px(72.))
                .h(px(32.))
                .flex_shrink_0()
                .child(send_button),
        );
        if projection.can_retry_safe {
            let retry_tab = tab_id.clone();
            row = row.child(
                Button::new("workspace-broadcast-retry-safe")
                    .compact()
                    .ghost()
                    .label(crate::locale::text("续发未发送目标", "Retry unsent"))
                    .disabled(projection.execution_busy)
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        cx.emit(WorkspaceBroadcastBarEvent::RetrySafeFailures {
                            tab_id: retry_tab.clone(),
                        });
                    })),
            );
        }

        let mut status_slot = div()
            .id("workspace-broadcast-status-slot")
            .w(px(28.))
            .h(px(28.))
            .flex_shrink_0();
        if let Some(status) = projection.status {
            let results_tab = tab_id.clone();
            let has_results = !projection.result_details.is_empty();
            status_slot = status_slot.child(
                Button::new("workspace-broadcast-status")
                    .compact()
                    .ghost()
                    .w_full()
                    .h_full()
                    .icon(IconName::Info)
                    .selected(projection.results_open)
                    .accessibility_label(status.clone())
                    .tooltip(status)
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        if has_results {
                            cx.emit(WorkspaceBroadcastBarEvent::ToggleResults {
                                tab_id: results_tab.clone(),
                            });
                        }
                    })),
            );
        }
        row = row.child(status_slot);
        let details = if projection.results_open {
            let rows = projection
                .result_details
                .into_iter()
                .take(64)
                .map(|detail| {
                    div()
                        .w_full()
                        .flex()
                        .justify_between()
                        .gap_2()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(detail.title)
                        .child(detail.state)
                })
                .collect::<Vec<_>>();
            Some(
                div()
                    .id("workspace-broadcast-result-details")
                    .max_h(px(112.))
                    .w_full()
                    .px_3()
                    .py_1()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .children(rows),
            )
        } else {
            None
        };
        div()
            .id("workspace-broadcast-container")
            .w_full()
            .flex()
            .flex_col()
            .child(row)
            .when_some(details, |container, details| container.child(details))
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/workspace_broadcast_toolbar.rs"]
mod toolbar_tests;
