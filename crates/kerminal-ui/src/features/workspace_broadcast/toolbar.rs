//! 分屏命令栏只编辑按 Tab 保存的草稿并发出动作意图，真实发送由 Root 和 CommandsService 校验。
//!
//! @author kongweiguang

use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, EntityInputHandler, EventEmitter, Focusable as _,
    IntoElement, Render, ScrollStrategy, StyleRefinement, Subscription, UniformListScrollHandle,
    Window,
    assets::IconName,
    base::Selectable as _,
    component::{
        Disableable,
        button::{Button, ButtonVariants as _},
        input::{Input, InputEvent, InputState},
        popover::Popover,
    },
    div,
    prelude::*,
    px, uniform_list,
};
use kerminal_domain::identity::TabId;

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

/// 仅展示可插入的安全模板字段，片段选中动作不会启动终端写入。
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
    Close {
        tab_id: TabId,
    },
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

    /// 三种目标模式收在同一紧凑触发区，避免窄窗口重复标签挤压命令输入。
    fn render_target_mode(&self, tab_id: &TabId, count: usize, cx: &mut Context<Self>) -> Popover {
        let label = match self.projection.mode {
            WorkspaceBroadcastTargetMode::All => crate::locale::text("全部", "All"),
            WorkspaceBroadcastTargetMode::Current => crate::locale::text("当前", "Current"),
            WorkspaceBroadcastTargetMode::Custom => crate::locale::text("自选", "Custom"),
        };
        let weak = cx.weak_entity();
        let event_tab = tab_id.clone();
        let active_mode = self.projection.mode;
        Popover::new("workspace-broadcast-mode-picker")
            .trigger(
                Button::new("workspace-broadcast-mode")
                    .compact()
                    .ghost()
                    .label(format!("{label} · {count}")),
            )
            .trigger_style(StyleRefinement::default().flex_none())
            .open(self.target_mode_open)
            .on_open_change(cx.listener(|bar, open, _, cx| {
                bar.target_mode_open = *open;
                cx.notify();
            }))
            .content(move |_, _, _| {
                let rows = [
                    (
                        WorkspaceBroadcastTargetMode::All,
                        crate::locale::text("全部分屏", "All panes"),
                    ),
                    (
                        WorkspaceBroadcastTargetMode::Current,
                        crate::locale::text("当前分屏", "Current pane"),
                    ),
                    (
                        WorkspaceBroadcastTargetMode::Custom,
                        crate::locale::text("自定义目标", "Custom targets"),
                    ),
                ]
                .into_iter()
                .map(|(mode, label)| {
                    let weak = weak.clone();
                    let tab_id = event_tab.clone();
                    Button::new(format!("workspace-broadcast-mode-{mode:?}"))
                        .compact()
                        .ghost()
                        .w_full()
                        .justify_start()
                        .selected(active_mode == mode)
                        .label(label)
                        .on_click(move |_, _, app| {
                            if let Some(bar) = weak.upgrade() {
                                bar.update(app, |bar, cx| {
                                    bar.target_mode_open = false;
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
                div().w(px(180.)).flex().flex_col().gap_1().children(rows)
            })
    }

    /// 片段目录独立滚动并虚拟化，不裁剪候选，也不把目录展开高度计入终端画布。
    /// 搜索实体随工具栏保留；选择只提交稳定片段身份，渲染和目标校验仍由 Root 处理。
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
                                    let weak = list_weak.clone();
                                    let tab_id = row_tab.clone();
                                    Button::new(format!(
                                        "workspace-broadcast-snippet-{}",
                                        snippet.id
                                    ))
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
    /// 常用操作保持紧凑，窄窗口允许换行；片段目录独立弹出，画布尺寸由真实布局测量。
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

        let close_tab_id = tab_id.clone();
        row = row.child(self.render_target_mode(&tab_id, projection.selected_count, cx));
        let composition_draft = self.draft.clone();
        row = row.child(
            div()
                .w_0()
                .flex_1()
                .min_w(px(120.))
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
            Button::new("workspace-broadcast-sync")
                .compact()
                .ghost()
                .selected(projection.sync_enabled)
                .label(crate::locale::text("同步按键", "Sync input"))
                .disabled(projection.selected_count < 2 || projection.execution_busy)
                .tooltip(crate::locale::text(
                    "同步按键默认关闭",
                    "Synchronized input starts off",
                ))
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    cx.emit(WorkspaceBroadcastBarEvent::ToggleSync {
                        tab_id: sync_tab.clone(),
                    });
                })),
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

        let send_tab = tab_id.clone();
        row = row.child(
            Button::new("workspace-broadcast-send")
                .compact()
                .primary()
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
                })),
        );

        if projection.command_pending && projection.can_cancel {
            let cancel_tab = tab_id.clone();
            row = row.child(
                Button::new("workspace-broadcast-cancel")
                    .compact()
                    .ghost()
                    .label(crate::locale::text("取消", "Cancel"))
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        cx.emit(WorkspaceBroadcastBarEvent::CancelCommand {
                            tab_id: cancel_tab.clone(),
                        });
                    })),
            );
        }
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

        if !projection.result_details.is_empty() {
            let results_tab = tab_id.clone();
            row = row.child(
                Button::new("workspace-broadcast-results")
                    .compact()
                    .ghost()
                    .selected(projection.results_open)
                    .label(crate::locale::text("结果", "Results"))
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        cx.emit(WorkspaceBroadcastBarEvent::ToggleResults {
                            tab_id: results_tab.clone(),
                        });
                    })),
            );
        }

        if let Some(status) = projection.status {
            row = row.child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .max_w(px(220.))
                    .overflow_hidden()
                    .child(status),
            );
        }
        row = row.child(
            Button::new("workspace-broadcast-close")
                .compact()
                .ghost()
                .flex_shrink_0()
                .icon(IconName::Close)
                .accessibility_label(crate::locale::text("隐藏群发栏", "Hide broadcast bar"))
                .tooltip(crate::locale::text("隐藏群发栏", "Hide broadcast bar"))
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    cx.emit(WorkspaceBroadcastBarEvent::Close {
                        tab_id: close_tab_id.clone(),
                    });
                })),
        );
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
