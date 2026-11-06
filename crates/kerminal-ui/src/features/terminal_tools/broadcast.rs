//! 终端输入广播目标选择器；只有用户明确开始后才把所选 SessionId 交给 Root。
//!
//! @author kongweiguang

use std::collections::BTreeSet;

use gpui_kit::{
    ClickEvent, Context, EventEmitter, IntoElement, Render, Window,
    component::{
        Disableable, Sizable as _,
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::identity::SessionId;

use crate::theme::ThemeTokens;

const MAX_BROADCAST_TARGETS: usize = 64;

/// 每个目标的可用状态来自当前会话投影；选择器不自行列会话或迁移目标。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BroadcastTargetProjection {
    pub session_id: SessionId,
    pub title: String,
    pub available: bool,
}

/// 广播启动和取消都有显式事件，Root 仍需重新校验完整 SessionId 集合。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BroadcastEvent {
    Start { session_ids: Vec<SessionId> },
    Stop,
    Cancel,
}

/// 目标勾选属于一次明确的用户交互草稿，启动后输入路由由应用根统一执行。
pub struct BroadcastTargetPicker {
    targets: Vec<BroadcastTargetProjection>,
    selected: BTreeSet<SessionId>,
    active: bool,
    pending: bool,
    error: Option<String>,
}

impl EventEmitter<BroadcastEvent> for BroadcastTargetPicker {}

impl BroadcastTargetPicker {
    /// 注入显式目标投影，广播面板不会通过活动 Tab 猜测其它会话。
    pub fn new(targets: Vec<BroadcastTargetProjection>) -> Self {
        Self {
            targets,
            selected: BTreeSet::new(),
            active: false,
            pending: false,
            error: None,
        }
    }

    /// 更新会话清单时保留失效目标的身份并标记不可用，避免替换成其它活跃 Session。
    pub fn set_targets(
        &mut self,
        mut targets: Vec<BroadcastTargetProjection>,
        cx: &mut Context<Self>,
    ) {
        for stale_id in self.selected.iter() {
            if !targets.iter().any(|target| &target.session_id == stale_id) {
                targets.push(BroadcastTargetProjection {
                    session_id: stale_id.clone(),
                    title: crate::locale::text(
                        "目标会话已失效",
                        "The target session is no longer available",
                    )
                    .to_string(),
                    available: false,
                });
            }
        }
        self.targets = targets;
        cx.notify();
    }

    /// 宿主确认广播模式变化后回投当前状态，停止时清空草稿避免下次误选旧目标。
    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        self.active = active;
        self.pending = false;
        self.error = None;
        if !active {
            self.selected.clear();
        }
        cx.notify();
    }

    /// Root 在广播用例运行期间锁住启动和停止，成功或失败回投后再解除。
    pub fn set_pending(&mut self, pending: bool, cx: &mut Context<Self>) {
        self.pending = pending;
        if pending {
            self.error = None;
        }
        cx.notify();
    }

    /// 广播用例失败时解除 pending 并保留仍然有效的广播状态。
    pub fn set_error(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.pending = false;
        self.error = Some(message.into());
        cx.notify();
    }

    /// 返回稳定选择快照供宿主建立会话绑定，不从标签显示顺序重建身份。
    pub fn selected_targets(&self) -> Vec<SessionId> {
        self.selected.iter().cloned().collect()
    }

    /// 让 Root 查询已由应用用例确认的当前广播状态，而不读取本地勾选草稿。
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// 展示正在执行的广播请求时，右栏折叠按钮不能隐藏尚未完成的状态。
    pub fn is_pending(&self) -> bool {
        self.pending
    }

    /// 取消未启动的目标草稿并发布显式取消事件；活动广播必须先由 Root 停止。
    pub fn cancel_selection(&mut self, cx: &mut Context<Self>) {
        self.cancel(cx);
    }

    /// 接受复选控件请求的目标值而非盲目翻转，避免重复事件或迟到事件改变已提交的选择。
    /// 失效目标仍可取消选择，新增目标必须通过当前会话投影和数量上限检查。
    ///
    /// @author kongweiguang
    fn set_target_selected(
        &mut self,
        session_id: &SessionId,
        selected: bool,
        cx: &mut Context<Self>,
    ) {
        if self.pending || self.active {
            return;
        }
        let changed = if selected {
            let available = self
                .targets
                .iter()
                .any(|target| &target.session_id == session_id && target.available);
            available
                && self.selected.len() < MAX_BROADCAST_TARGETS
                && self.selected.insert(session_id.clone())
        } else {
            self.selected.remove(session_id)
        };
        if changed {
            cx.notify();
        }
    }

    /// 开始前再次确认所有身份仍在当前可用投影中，防止打开面板期间静默换目标。
    fn start(&mut self, cx: &mut Context<Self>) {
        if self.active
            || self.pending
            || self.selected.is_empty()
            || self.selected.len() > MAX_BROADCAST_TARGETS
        {
            return;
        }
        let all_available = self.selected.iter().all(|session_id| {
            self.targets
                .iter()
                .any(|target| &target.session_id == session_id && target.available)
        });
        if all_available {
            self.pending = true;
            self.error = None;
            cx.emit(BroadcastEvent::Start {
                session_ids: self.selected_targets(),
            });
            cx.notify();
        }
    }

    /// 取消时清空本地选择并发出关闭意图，已经活跃的广播必须走 Stop 显式退出。
    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.active || self.pending {
            return;
        }
        self.selected.clear();
        self.error = None;
        cx.emit(BroadcastEvent::Cancel);
        cx.notify();
    }
}

impl Render for BroadcastTargetPicker {
    /// 复用受控复选框的键盘和无障碍语义，目标变多时只滚动列表并保留固定的提交操作。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        if self.active {
            let error = self.error.clone();
            let mut panel = div()
                .id("terminal-broadcast-active")
                .px_2()
                .py_1()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .bg(theme.panel)
                .text_color(theme.accent)
                .child(div().flex_1().min_w_0().child(crate::locale::text(
                    format!("广播输入已启用 · {} 个目标", self.selected.len()),
                    format!(
                        "Broadcast input is active · {} targets",
                        self.selected.len()
                    ),
                )))
                .child(
                    Button::new("terminal-broadcast-stop")
                        .compact()
                        .outline()
                        .label(if self.pending {
                            crate::locale::text("正在停止…", "Stopping…")
                        } else {
                            crate::locale::text("停止广播", "Stop broadcast")
                        })
                        .disabled(self.pending)
                        .on_click(cx.listener(|picker, _: &ClickEvent, _, cx| {
                            if picker.pending {
                                return;
                            }
                            picker.pending = true;
                            cx.emit(BroadcastEvent::Stop);
                            cx.notify();
                        })),
                )
                .into_any_element();
            if let Some(error) = error {
                panel = div()
                    .id("terminal-broadcast-active-with-error")
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(panel)
                    .child(
                        div()
                            .text_size(theme.text_xs)
                            .text_color(theme.error)
                            .child(error),
                    )
                    .into_any_element();
            }
            return panel;
        }

        let target_rows = self
            .targets
            .iter()
            .map(|target| {
                let session_id = target.session_id.clone();
                let selected = self.selected.contains(&session_id);
                let unavailable = !target.available;
                let disabled = self.pending
                    || unavailable && !selected
                    || self.selected.len() >= MAX_BROADCAST_TARGETS && !selected;
                let label = if unavailable {
                    crate::locale::text(
                        format!("{} · 不可用", target.title),
                        format!("{} · unavailable", target.title),
                    )
                } else {
                    crate::locale::text(target.title.clone(), target.title.clone())
                };
                let selector = format!("terminal-broadcast-target-{}", session_id.as_str());
                Checkbox::new(selector.clone())
                    .debug_selector(move || selector)
                    .w_full()
                    .min_w(px(0.))
                    .py_1()
                    .small()
                    .checked(selected)
                    .disabled(disabled)
                    .label(label)
                    .on_change(cx.listener(move |picker, checked: &bool, _, cx| {
                        picker.set_target_selected(&session_id, *checked, cx);
                    }))
            })
            .collect::<Vec<_>>();
        let can_start = !self.pending
            && !self.selected.is_empty()
            && self.selected.len() <= MAX_BROADCAST_TARGETS
            && self.selected.iter().all(|session_id| {
                self.targets
                    .iter()
                    .any(|target| &target.session_id == session_id && target.available)
            });
        let error = self.error.clone();
        let mut panel = div()
            .id("terminal-broadcast-picker")
            .debug_selector(|| "terminal-broadcast-picker".to_owned())
            .w_full()
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .rounded_md()
            .bg(theme.panel)
            .text_color(theme.foreground)
            .child(crate::locale::text(
                "选择要接收后续键盘输入的终端",
                "Select the terminals that will receive subsequent keyboard input.",
            ))
            .child(
                div()
                    .id("terminal-broadcast-target-list")
                    .w_full()
                    .min_h(px(0.))
                    .max_h(gpui_kit::rems(16.))
                    .overflow_y_scroll()
                    .children(target_rows),
            )
            .when(self.targets.is_empty(), |panel| {
                panel.child(
                    div()
                        .py_2()
                        .text_color(theme.muted)
                        .child(crate::locale::text(
                            "没有可广播的终端",
                            "No terminals are available for broadcast.",
                        )),
                )
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(crate::locale::text(
                        format!("已选 {} / {MAX_BROADCAST_TARGETS}", self.selected.len()),
                        format!("{} / {MAX_BROADCAST_TARGETS} selected", self.selected.len()),
                    )))
                    .child(
                        Button::new("terminal-broadcast-cancel")
                            .debug_selector(|| "terminal-broadcast-cancel".to_owned())
                            .compact()
                            .label(crate::locale::text("取消", "Cancel"))
                            .disabled(self.pending)
                            .on_click(cx.listener(|picker, _: &ClickEvent, _, cx| {
                                picker.cancel(cx);
                            })),
                    )
                    .child(
                        Button::new("terminal-broadcast-start")
                            .debug_selector(|| "terminal-broadcast-start".to_owned())
                            .compact()
                            .primary()
                            .label(if self.pending {
                                crate::locale::text("正在启动…", "Starting…")
                            } else {
                                crate::locale::text("开始广播", "Start broadcast")
                            })
                            .disabled(!can_start)
                            .on_click(cx.listener(|picker, _: &ClickEvent, _, cx| {
                                picker.start(cx);
                            })),
                    ),
            );
        if let Some(error) = error {
            panel = panel.child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.error)
                    .child(error),
            );
        }
        panel.into_any_element()
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/terminal_broadcast_picker.rs"]
mod tests;
