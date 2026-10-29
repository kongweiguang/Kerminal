//! 分屏命令栏维护按 Tab 投影的草稿与动作意图，终端写入统一由工作台动作链执行。
//!
//! @author kongweiguang

use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, EntityInputHandler, EventEmitter, IntoElement,
    MouseButton, Render, StyleRefinement, Subscription, Window,
    assets::IconName,
    base::Selectable as _,
    component::{
        Disableable,
        button::{Button, ButtonCustomVariant, ButtonVariants as _},
        checkbox::Checkbox,
        input::{Input, InputEvent, InputState},
        popover::Popover,
        switch::Switch,
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::identity::{PaneId, TabId};

use super::{WorkspaceBroadcastTargetMode, broadcast_draft_error};
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
    pub result_summary: Option<String>,
    pub result_details: Vec<WorkspaceBroadcastDeliveryProjection>,
    pub status: Option<String>,
    pub status_is_error: bool,
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
}

/// 左列表达模式，右列只显示该模式的目标标题或数量，避免长标题挤压用户可比较的模式名。
///
/// @author kongweiguang
fn target_mode_option_labels(
    projection: &WorkspaceBroadcastBarProjection,
) -> [(WorkspaceBroadcastTargetMode, String, String); 3] {
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
            crate::locale::text("当前分屏", "Current pane").to_string(),
            current_title,
        ),
        (
            WorkspaceBroadcastTargetMode::All,
            crate::locale::text("全部分屏", "All panes").to_string(),
            crate::locale::text(
                format!("{all_count} 个目标"),
                format!("{all_count} targets"),
            )
            .to_string(),
        ),
        (
            WorkspaceBroadcastTargetMode::Custom,
            crate::locale::text("自定义", "Custom").to_string(),
            crate::locale::text(
                format!("{} 个已选", projection.selected_count),
                format!("{} selected", projection.selected_count),
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
    target_mode_open: bool,
    projection: WorkspaceBroadcastBarProjection,
    _draft_subscription: Subscription,
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
                    if !composing
                        && bar.can_send()
                        && let Some(tab_id) = bar.projection.tab_id.clone()
                    {
                        cx.emit(WorkspaceBroadcastBarEvent::SendCommand { tab_id });
                    }
                }
                InputEvent::PressEnter { .. } | InputEvent::Focus | InputEvent::Blur => {}
            },
        );
        Self {
            draft,
            target_mode_open: false,
            projection: WorkspaceBroadcastBarProjection::default(),
            _draft_subscription: draft_subscription,
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
        if tab_changed {
            self.target_mode_open = false;
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

    /// 当前与全部是一次选择；自定义和失败详情保持弹层打开，连续操作仍固定原 Tab 身份。
    /// 模式 Button 的 hover 由 Kit 变体拥有，避免在组件渲染时重复写入 GPUI hover 状态。
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
        let result_details = std::rc::Rc::new(self.projection.result_details.clone());
        let result_summary = self.projection.result_summary.clone();
        let current_error = self
            .projection
            .status_is_error
            .then(|| self.projection.status.clone())
            .flatten();
        let has_error = result_summary.is_some() || current_error.is_some();
        let current_available = targets
            .iter()
            .any(|target| target.current && target.enabled);
        let any_available = targets.iter().any(|target| target.enabled);
        let trigger_label = target_mode_trigger_label(&self.projection);
        let viewport_width = window.bounds().size.width.to_f64();
        let viewport_height = window.bounds().size.height.to_f64();
        let width = px((viewport_width - 24.).clamp(0., 360.) as f32);
        let max_height = px((viewport_height - 32.).clamp(160., 560.) as f32);
        let report_height = if result_summary.is_some() {
            124.
        } else if current_error.is_some() {
            52.
        } else {
            0.
        };
        let list_height = px(((targets.len().max(1) as f32 * 48.)
            .min(max_height.to_f64() as f32 - 116. - report_height))
        .max(44.));
        let theme = ThemeTokens::current(window, cx).for_workbench();
        let mode_button_variant = ButtonCustomVariant::new(cx)
            .foreground(theme.foreground)
            .hover(theme.surface_hover())
            .active(theme.surface_selected());
        let trigger_accessibility = if has_error {
            crate::locale::text(
                "发送目标；群发结果有异常，打开查看详情",
                "Broadcast targets; the last result has errors, open to review details",
            )
        } else {
            crate::locale::text(
                format!("发送目标：{trigger_label}"),
                format!("Broadcast targets: {trigger_label}"),
            )
        };
        Popover::new("workspace-broadcast-mode-picker")
            .appearance(false)
            .trigger(
                Button::new("workspace-broadcast-mode")
                    .debug_selector(|| "workspace-broadcast-mode-trigger".to_owned())
                    .compact()
                    .custom(
                        ButtonCustomVariant::new(cx)
                            .color(theme.floating_surface)
                            .foreground(theme.foreground)
                            .hover(theme.surface_hover())
                            .active(theme.surface_selected()),
                    )
                    .h(px(36.))
                    .rounded(px(12.))
                    .px(px(10.))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.floating_surface)
                    .max_w_40()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .open(self.target_mode_open)
                    .when(self.target_mode_open, |button| {
                        button.bg(theme.surface_selected())
                    })
                    .icon(IconName::ChevronDown)
                    .label(trigger_label)
                    .accessibility_label(trigger_accessibility.clone())
                    .tooltip(trigger_accessibility)
                    .when(has_error, |button| {
                        button.child(
                            div()
                                .flex_none()
                                .text_color(theme.error)
                                .child(IconName::CircleAlert),
                        )
                    }),
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
                    .map(|(mode, label, detail)| {
                        let weak = weak.clone();
                        let tab_id = event_tab.clone();
                        let label_selector = format!("workspace-broadcast-mode-label-{mode:?}");
                        let detail_selector = format!("workspace-broadcast-mode-detail-{mode:?}");
                        let accessible_label = format!("{label} · {detail}");
                        Button::new(format!("workspace-broadcast-mode-{mode:?}"))
                            .debug_selector(move || {
                                format!("workspace-broadcast-mode-option-{mode:?}")
                            })
                            .compact()
                            .custom(mode_button_variant)
                            .w_full()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .justify_start()
                            .selected(active_mode == mode)
                            .when(active_mode == mode, |button| {
                                button.bg(theme.surface_selected())
                            })
                            .disabled(match mode {
                                WorkspaceBroadcastTargetMode::Current => !current_available,
                                WorkspaceBroadcastTargetMode::All
                                | WorkspaceBroadcastTargetMode::Custom => !any_available,
                            })
                            .accessibility_label(accessible_label.clone())
                            .tooltip(accessible_label)
                            .child(
                                div()
                                    .w_full()
                                    .min_w(px(0.))
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .child(
                                        div()
                                            .debug_selector(move || label_selector.clone())
                                            .flex_none()
                                            .whitespace_nowrap()
                                            .child(label),
                                    )
                                    .child(
                                        div()
                                            .debug_selector(move || detail_selector.clone())
                                            .flex_1()
                                            .min_w(px(0.))
                                            .whitespace_nowrap()
                                            .truncate()
                                            .text_right()
                                            .text_color(theme.muted)
                                            .child(detail),
                                    ),
                            )
                            .on_click(move |_, _, app| {
                                if let Some(bar) = weak.upgrade() {
                                    bar.update(app, |bar, cx| {
                                        if bar.projection.tab_id.as_ref() != Some(&tab_id) {
                                            return;
                                        }
                                        bar.target_mode_open =
                                            mode == WorkspaceBroadcastTargetMode::Custom;
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
                        .rounded(theme.control_radius)
                        .when(target.selected, |checkbox| {
                            checkbox.bg(theme.surface_selected())
                        })
                        .hover(|style| style.bg(theme.surface_hover()))
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
                                bar.update(app, |bar, cx| {
                                    if bar.projection.tab_id.as_ref() != Some(&tab_id) {
                                        return;
                                    }
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
                    .id("workspace-broadcast-mode-panel")
                    .w(width)
                    .max_w(width)
                    .max_h(max_height)
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.floating_surface)
                    .p_2()
                    .overflow_y_scroll()
                    .children(mode_rows)
                    .child(div().my_1().h(px(1.)).w_full().bg(theme.border));
                if let Some(summary) = result_summary.clone() {
                    panel = panel.child(
                        div()
                            .id("workspace-broadcast-result-summary")
                            .debug_selector(|| "workspace-broadcast-result-summary".to_owned())
                            .w_full()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .rounded(theme.control_radius)
                            .border_1()
                            .border_color(theme.error.opacity(0.4))
                            .bg(theme.surface_selected())
                            .p_2()
                            .child(
                                div()
                                    .text_size(theme.text_xs)
                                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                                    .text_color(theme.error)
                                    .child(crate::locale::text("上次发送结果", "Last send result")),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .min_w(px(0.))
                                    .text_size(theme.text_xs)
                                    .text_color(theme.foreground)
                                    .child(summary),
                            )
                            .when(!result_details.is_empty(), |report| {
                                report.child(
                                    div()
                                        .id("workspace-broadcast-deliveries")
                                        .debug_selector(|| {
                                            "workspace-broadcast-deliveries".to_owned()
                                        })
                                        .w_full()
                                        .max_h(px(96.))
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .overflow_y_scroll()
                                        .children(result_details.iter().cloned().map(|detail| {
                                            div()
                                                .w_full()
                                                .flex()
                                                .gap_2()
                                                .text_size(theme.text_xs)
                                                .text_color(theme.muted)
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w(px(0.))
                                                        .truncate()
                                                        .child(detail.title),
                                                )
                                                .child(detail.state)
                                        })),
                                )
                            }),
                    );
                }
                if let Some(error) = current_error.clone() {
                    panel = panel.child(
                        div()
                            .id("workspace-broadcast-current-error")
                            .debug_selector(|| "workspace-broadcast-current-error".to_owned())
                            .w_full()
                            .min_w(px(0.))
                            .rounded(theme.control_radius)
                            .bg(theme.surface_hover())
                            .px_2()
                            .py_1()
                            .text_size(theme.text_xs)
                            .text_color(theme.error)
                            .child(error),
                    );
                }
                let target_count = targets.len();
                panel = panel.child(
                    div()
                        .id("workspace-broadcast-target-heading")
                        .debug_selector(|| "workspace-broadcast-target-heading".to_owned())
                        .w_full()
                        .min_w(px(0.))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(crate::locale::text("分屏目标", "Target panes"))
                        .child(
                            div()
                                .id("workspace-broadcast-target-count")
                                .debug_selector(|| "workspace-broadcast-target-count".to_owned())
                                .text_right()
                                .child(crate::locale::text(
                                    format!("{target_count} 个"),
                                    format!("{target_count}"),
                                )),
                        ),
                );
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
                crate::ui_backdrop::floating_surface(panel, theme, theme.radius)
            })
    }

    /// Enter 与图标共享同一准入，忙碌、空命令或目标失效时不发出重复的发送意图。
    ///
    /// @author kongweiguang
    pub(crate) fn can_send(&self) -> bool {
        !self.projection.execution_busy
            && !self.projection.command_pending
            && self.projection.selected_count > 0
            && !self.projection.draft.trim().is_empty()
            && broadcast_draft_error(&self.projection.draft).is_none()
    }
}

impl Render for WorkspaceBroadcastBar {
    /// 单一圆角表面承载目标、草稿与动作；异常进入目标弹层，避免终端上方增减反馈行。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx).for_workbench();
        let projection = self.projection.clone();
        let Some(tab_id) = projection.tab_id.clone() else {
            return div().into_any_element();
        };
        let enter_draft = self.draft.clone();
        let composition_draft = self.draft.clone();
        let sync_tab = tab_id.clone();
        let send_tab = tab_id.clone();
        let draft_error = broadcast_draft_error(&projection.draft);
        let send_label = if projection.command_pending {
            crate::locale::text("发送中", "Sending")
        } else if projection.execution_busy {
            crate::locale::text("另一项群发仍在收尾", "Another broadcast is still finishing")
        } else if let Some(error) = draft_error.as_ref() {
            error.clone()
        } else if projection.selected_count == 0 {
            crate::locale::text("没有可发送目标", "No available targets")
        } else if projection.draft.trim().is_empty() {
            crate::locale::text("输入命令后发送", "Enter a command to send")
        } else {
            crate::locale::text(
                format!("发送到 {} 个目标", projection.selected_count),
                format!("Send to {} targets", projection.selected_count),
            )
        };
        let send_icon = if projection.command_pending {
            IconName::LoaderCircle
        } else {
            IconName::Send
        };
        let row = div()
            .id("workspace-broadcast-bar")
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.floating_surface)
            .text_color(theme.foreground)
            .child(self.render_target_mode(&tab_id, window, cx))
            .child(
                div()
                    .w_0()
                    .flex_1()
                    .min_w_0()
                    .debug_selector(|| "workspace-broadcast-draft-input-slot".to_owned())
                    // Input 的 PressEnter 订阅晚于默认文本派发；必须在 action 捕获阶段
                    // 消费组合态 Enter，避免默认换行先清掉组合标记后误发命令。
                    .capture_action(move |_: &gpui_kit::component::input::Enter, window, cx| {
                        if enter_draft.read(cx).is_composing() {
                            window.prevent_default();
                            cx.stop_propagation();
                        }
                    })
                    .capture_action(move |_: &gpui_kit::component::input::Escape, window, cx| {
                        crate::features::input_composition::cancel_input_composition(
                            &composition_draft, window, cx,
                        );
                    })
                    .child(
                        gpui_kit::Styled::h(
                            Input::new(&self.draft)
                                .w_full()
                                .bg(theme.field_surface())
                                .rounded(theme.control_radius)
                                .border_color(theme.border)
                                .hover_style(
                                    StyleRefinement::default()
                                        .bg(theme.field_hover_surface())
                                        .border_color(theme.accent),
                                ),
                            px(36.),
                        ),
                    ),
            )
            .when(window.bounds().size.width >= px(1280.), |row| {
                row.child(
                    div()
                        .flex_none()
                        .rounded_lg()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .bg(theme.surface_hover())
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            format!("{} 个目标", projection.selected_count),
                            format!("{} targets", projection.selected_count),
                        )),
                )
            })
            .child(
                div()
                    .debug_selector(|| "workspace-broadcast-actions".to_owned())
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_2()
                            .capture_any_mouse_down(|event, window, _| {
                                if event.button == MouseButton::Left {
                                    window.prevent_default();
                                }
                            })
                            .child(div().text_xs().text_color(theme.muted).child(crate::locale::text("同步", "Sync")))
                            .child(
                                div()
                                    .debug_selector(|| "workspace-broadcast-sync-toggle".to_owned())
                                    .child(Switch::new("workspace-broadcast-sync")
                                    .checked(projection.sync_enabled)
                                    .disabled(!projection.sync_enabled && projection.selected_count < 2)
                                    .accessibility_label(crate::locale::text("同步终端输入", "Synchronize terminal input"))
                                    .tooltip(crate::locale::text(
                                        "同步所选分屏的输入（含粘贴、终端快捷键）；请确认各端命令行状态一致。",
                                        "Synchronize input, paste and shortcuts across selected panes. Keep their command lines aligned.",
                                    ))
                                    .on_change(cx.listener(move |_, _: &bool, _, cx| {
                                        cx.emit(WorkspaceBroadcastBarEvent::ToggleSync { tab_id: sync_tab.clone() });
                                    }))),
                            ),
                    )
                    .child(
                        Button::new("workspace-broadcast-send")
                            .debug_selector(|| "workspace-broadcast-send-button".to_owned())
                            .primary()
                            .size_9()
                            .flex_none()
                            .icon(send_icon)
                            .loading(projection.command_pending)
                            .accessibility_label(send_label.clone())
                            .tooltip(send_label)
                            .disabled(!self.can_send())
                            .on_click(cx.listener(move |bar, _: &ClickEvent, window, cx| {
                                if bar.can_send() && !bar.is_composing(window, cx) {
                                    cx.emit(WorkspaceBroadcastBarEvent::SendCommand { tab_id: send_tab.clone() });
                                }
                            })),
                    ),
            );
        let row = crate::ui_backdrop::floating_surface(row, theme, theme.radius);
        div()
            .id("workspace-broadcast-container")
            .w_full()
            .min_w_0()
            .child(row)
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/workspace_broadcast_toolbar.rs"]
mod toolbar_tests;

#[cfg(all(test, windows))]
#[path = "../../../tests/unit/workspace_broadcast_native.rs"]
mod native_tests;
