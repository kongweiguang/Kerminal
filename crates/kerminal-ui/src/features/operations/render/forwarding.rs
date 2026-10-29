//! SSH 端口转发的紧凑列表与根模态表单。
//!
//! @author kongweiguang

use super::super::OperationsView;
use super::super::forms::{ForwardBindScope, ForwardBindScopeOption};
use super::{localized_text, section_feedback, section_page};
use crate::theme::ThemeTokens;
use crate::ui_button::icon_button;
use gpui_kit::{
    AnyElement, ClickEvent, Context, ElementId, Entity, IntoElement, SharedString, Window,
    assets::IconName,
    base::Button as BaseButton,
    component::{
        ActiveTheme as _, Disableable, Icon, Sizable as _,
        button::{Button, ButtonCustomVariant, ButtonVariants},
        input::Input,
        select::Select,
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::operations::ManagedPortForwardSnapshot;
use kerminal_application::operations::PortForwardDefinition;
use kerminal_domain::identity::HostId;
use kerminal_domain::operations::{ForwardKind, ForwardState};
use std::net::IpAddr;

#[path = "forwarding_proxy_scripts.rs"]
mod proxy_scripts;

/// 隧道控件沿用 Kit 的焦点、禁用和事件语义，仅投影 main 的 36px 描边按钮与控制圆角。
fn forward_action_button(
    theme: ThemeTokens,
    id: impl Into<ElementId>,
    icon: IconName,
    label: impl Into<SharedString>,
) -> Button {
    let label = label.into();
    icon_button(id, icon, label.clone())
        .secondary()
        .compact()
        .w(px(36.))
        .h(px(36.))
        .rounded(theme.control_radius)
        .tooltip(label)
}

/// 运行时清单供全局代理使用；右栏必须单独按固定 SSH 主机过滤，不能显示其它主机的会话。
fn forward_matches_host(forward: &ManagedPortForwardSnapshot, host_id: Option<&HostId>) -> bool {
    host_id.is_some_and(|host_id| forward.forward.config.host_id == *host_id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ForwardScenarioCard {
    HostService,
    LocalService,
    Socks,
}

/// 把四种领域类型投影为截图中的三张操作卡，保留 SOCKS 本地/远端方向作为内部选择。
///
/// @author kongweiguang
fn forward_scenario_card(kind: ForwardKind) -> ForwardScenarioCard {
    match kind {
        ForwardKind::Local => ForwardScenarioCard::HostService,
        ForwardKind::Remote => ForwardScenarioCard::LocalService,
        ForwardKind::Dynamic | ForwardKind::RemoteDynamic => ForwardScenarioCard::Socks,
    }
}

/// 选择固定方向时使用对应 SSH 类型；进入 SOCKS 卡时默认本地 -D，并保留已有 SOCKS 子方向。
///
/// @author kongweiguang
fn forward_kind_for_scenario(scenario: ForwardScenarioCard, current: ForwardKind) -> ForwardKind {
    match scenario {
        ForwardScenarioCard::HostService => ForwardKind::Local,
        ForwardScenarioCard::LocalService => ForwardKind::Remote,
        ForwardScenarioCard::Socks => match current {
            ForwardKind::Dynamic | ForwardKind::RemoteDynamic => current,
            ForwardKind::Local | ForwardKind::Remote => ForwardKind::Dynamic,
        },
    }
}

/// 在 SOCKS 高级卡内部映射 -D 与远端 -R，避免把展示卡片直接变成新领域类型。
///
/// @author kongweiguang
fn forward_socks_kind(remote: bool) -> ForwardKind {
    if remote {
        ForwardKind::RemoteDynamic
    } else {
        ForwardKind::Dynamic
    }
}

/// 将四种领域方向投影成 main 路由区的流向与 OpenSSH 参数两行文案。
///
/// @author kongweiguang
fn forward_direction_labels(kind: ForwardKind) -> (SharedString, SharedString) {
    let (flow_zh, flow_en, option_zh, option_en) = match kind {
        ForwardKind::Local => ("本机 -> 主机", "Local -> host", "-L", "-L"),
        ForwardKind::Remote => ("主机 -> 本机", "Host -> local", "-R", "-R"),
        ForwardKind::Dynamic => ("本机 -> 主机网络", "Local -> host network", "-D", "-D"),
        ForwardKind::RemoteDynamic => (
            "主机 -> 本机网络",
            "Host -> local network",
            "remote -R SOCKS",
            "remote -R SOCKS",
        ),
    };
    (
        crate::locale::text(flow_zh, flow_en),
        crate::locale::text(option_zh, option_en),
    )
}

impl OperationsView {
    /// main 的标题卡与会话容器分别成组；成功和错误在标题内反馈，不增设全局结果卡。
    /// 容器目标显示专用提示且不改变 HostId 写入门禁。
    ///
    /// @author kongweiguang
    pub(super) fn render_forwards(
        &self,
        theme: ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected_host = super::super::resolve_forward_add_host(
            self.tool_rail_target.as_ref(),
            self.host_target.as_ref(),
        );
        if self.section_selection.is_fixed() && selected_host.is_none() {
            let (hint_zh, hint_en) =
                super::super::forward_unavailable_hint(self.tool_rail_container_target.as_ref());
            return section_page(
                crate::locale::text("SSH 隧道", "SSH tunnels"),
                div()
                    .w_full()
                    .p_4()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.panel)
                    .child(
                        div()
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .child(crate::locale::text("SSH 隧道", "SSH tunnels")),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_color(theme.muted)
                            .child(crate::locale::text(hint_zh, hint_en)),
                    ),
                theme,
                self.compact_layout,
                true,
            );
        }
        let rows = self
            .forwards
            .data()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|forward| {
                !self.section_selection.is_fixed()
                    || forward_matches_host(forward, selected_host.as_ref())
            })
            .collect::<Vec<_>>();
        let empty = rows.is_empty();
        let target_label = self.tool_rail_target_label();
        let endpoint_label = self
            .host_target
            .as_ref()
            .and_then(|host_id| self.host_subtitles.get(host_id))
            .map(|subtitle| forward_host_endpoint_label(subtitle.as_ref()).to_owned())
            .unwrap_or(target_label);
        let count_label = crate::locale::text(
            format!("{} 个会话", rows.len()),
            format!("{} sessions", rows.len()),
        );
        let mut body = div().w_full().flex().flex_col().gap_3();
        body = body.child(
            div()
                .id("operations-forward-header-card")
                .w_full()
                .flex()
                .flex_col()
                .gap_4()
                .p_4()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .bg(theme.panel)
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_start()
                        .justify_between()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(theme.text_heading)
                                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                                        .text_color(theme.foreground)
                                        .child(crate::locale::text("SSH 隧道", "SSH tunnels")),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .font_family("monospace")
                                        .text_size(theme.text_xs)
                                        .text_color(theme.muted)
                                        .child(endpoint_label),
                                ),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(11.))
                                .text_color(theme.muted)
                                .child(count_label),
                        ),
                )
                .children(self.compact_operation_feedback(theme))
                .when_some(
                    self.forward_success_notice
                        .as_ref()
                        .filter(|_| self.compact_layout && self.status.is_none()),
                    |header, message| {
                        header.child(
                            div()
                                .text_size(theme.text_sm)
                                .text_color(theme.success)
                                .child(message.clone()),
                        )
                    },
                )
                .when(
                    self.forwards.error().is_some()
                        || self.forwards.phase() == super::super::state::SectionPhase::Cancelled,
                    |header| {
                        header.child(section_feedback(
                            &self.forwards,
                            false,
                            SharedString::default(),
                            theme,
                        ))
                    },
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            Button::new("operations-forward-add")
                                .compact()
                                .w(px(36.))
                                .h(px(36.))
                                .rounded(theme.control_radius)
                                .custom(
                                    ButtonCustomVariant::new(cx)
                                        .color(theme.tunnel_action)
                                        .foreground(cx.theme().primary_foreground)
                                        .hover(theme.tunnel_action_hover)
                                        .active(theme.tunnel_action)
                                        .shadow(true),
                                )
                                .icon(IconName::Plus)
                                .accessibility_label(crate::locale::text("添加隧道", "Add tunnel"))
                                .tooltip(crate::locale::text("添加隧道", "Add tunnel"))
                                .disabled(
                                    self.busy
                                        || self.forward_save_pending.is_some()
                                        || !self.can_add_forward(),
                                )
                                .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                                    view.prepare_forward_add(window, cx);
                                })),
                        )
                        .child(
                            forward_action_button(
                                theme,
                                "operations-forwards-refresh",
                                IconName::RefreshCw,
                                crate::locale::text("刷新隧道列表", "Refresh tunnel list"),
                            )
                            .with_size(gpui_kit::component::Size::Medium)
                            .h(px(36.))
                            .disabled(
                                self.forwards.phase() == super::super::state::SectionPhase::Loading,
                            )
                            .on_click(
                                cx.listener(|view, _: &ClickEvent, _, cx| view.load_forwards(cx)),
                            ),
                        ),
                ),
        );

        let mut sessions = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .when(
                self.forwards.phase() == super::super::state::SectionPhase::Loading,
                |sessions| {
                    sessions.child(
                        div()
                            .px_2()
                            .py_1()
                            .text_right()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(crate::locale::text("正在同步", "Synchronizing")),
                    )
                },
            );
        if empty {
            sessions = sessions.child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_8()
                    .child(
                        div()
                            .text_size(theme.text_body)
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "当前主机暂无隧道会话。",
                                "This host has no tunnel sessions.",
                            )),
                    ),
            );
        }

        for forward in rows {
            let id = forward.forward.id.clone();
            let detail_state =
                window
                    .use_keyed_state(format!("operations-forward-details-{id}"), cx, |_, _| false);
            let details_open = *detail_state.read(cx);
            let toggle_detail_state = detail_state.clone();
            let copy_address = forward_copy_address(&forward);
            let copy_feedback_state = window.use_keyed_state(
                format!("operations-forward-copy-feedback-{id}"),
                cx,
                |_, _| None::<String>,
            );
            let copy_feedback = copy_feedback_state.read(cx).clone();
            let stopped = forward.forward.state == ForwardState::Stopped;
            let start_id = id.clone();
            let stop_id = id.clone();
            let edit_snapshot = forward.clone();
            let definition = PortForwardDefinition {
                id: id.clone(),
                config: forward.forward.config.clone(),
            };
            let delete_definition = definition.clone();
            let kind = forward.forward.config.kind;
            let is_remote = matches!(kind, ForwardKind::Remote | ForwardKind::RemoteDynamic);
            let is_remote_socks = kind == ForwardKind::RemoteDynamic;
            let (host_endpoint, local_endpoint) = forward_session_endpoints(&forward);
            let route = if is_remote {
                format!("{host_endpoint} → {local_endpoint}")
            } else {
                format!("{local_endpoint} → {host_endpoint}")
            };
            let host_id = &forward.forward.config.host_id;
            let host_id_details = forward_host_id_detail(host_id);
            let route_accessible = crate::locale::text(
                format!("{route}，{host_id_details}"),
                format!("{route}; {host_id_details}"),
            );
            let route_tooltip = host_id_details;
            let proxy_action_state = self
                .proxy_panel
                .as_ref()
                .map(|panel| panel.read(cx).action_state_for_forward(&id));
            let proxy_feedback = self
                .proxy_panel
                .as_ref()
                .and_then(|panel| panel.read(cx).feedback_for_forward(&id));
            let proxy_url = forward_proxy_url(&forward);
            let user_setup_script = (is_remote_socks
                && forward.forward.state == ForwardState::Active)
                .then(|| {
                    proxy_url
                        .as_deref()
                        .map(|url| proxy_scripts::user_proxy_setup_script(&id, url))
                })
                .flatten();
            let user_undo_script =
                is_remote_socks.then(|| proxy_scripts::user_proxy_undo_script(&id));
            let has_user_proxy_scripts = user_setup_script.is_some() && user_undo_script.is_some();
            let (state_color, state_description) = match forward.forward.state {
                ForwardState::Active => (theme.success, crate::locale::text("运行中", "Running")),
                ForwardState::Starting => {
                    (theme.warning, crate::locale::text("正在启动", "Starting"))
                }
                ForwardState::Closing => {
                    (theme.warning, crate::locale::text("正在停止", "Stopping"))
                }
                ForwardState::Stopped => (theme.muted, crate::locale::text("已退出", "Exited")),
                ForwardState::Failed => (theme.error, crate::locale::text("异常", "Failed")),
            };
            let failure = forward.failure_code.map_or_else(String::new, |code| {
                format!(" · {} {code:?}", localized_text("错误", "error"))
            });

            let mut row = div()
                .w_full()
                .flex()
                .when(self.compact_layout, |row| row.flex_col().items_stretch())
                .when(!self.compact_layout, |row| row.items_center())
                .gap_3()
                .when(self.compact_layout, |row| row.gap_0())
                .p_3()
                .rounded_xl()
                .border_1()
                .border_color(theme.border)
                .bg(theme.muted_surface())
                .child(
                    div()
                        .when(self.compact_layout, |content| content.w_full())
                        .when(!self.compact_layout, |content| content.flex_1())
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(
                                    Icon::new(if is_remote_socks {
                                        IconName::Wifi
                                    } else {
                                        IconName::Network
                                    })
                                    .size_4()
                                    .text_color(theme.tunnel_action),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(theme.text_sm)
                                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                        .text_color(theme.foreground)
                                        .child(forward.forward.config.name.clone()),
                                )
                                .child(
                                    div()
                                        .px_2()
                                        .py_1()
                                        .rounded_lg()
                                        .bg(state_color.opacity(0.1))
                                        .text_size(theme.text_xs)
                                        .text_color(state_color)
                                        .child(state_description),
                                ),
                        )
                        .child(
                            div()
                                .id(format!("operations-forward-route-{id}"))
                                .mt_2()
                                .pl(px(28.))
                                .text_size(px(11.))
                                .font_family("monospace")
                                .text_color(theme.muted)
                                .child(route)
                                .tooltip(move |window, cx| {
                                    Tooltip::new(route_tooltip.clone()).build(window, cx)
                                })
                                .aria_label(route_accessible),
                        )
                        .when(!failure.is_empty(), |content| {
                            content.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme.subtle)
                                    .child(failure.clone()),
                            )
                        }),
                );
            let mut actions = div().mt_3().flex().flex_wrap().gap_2().child(
                Button::new(format!("forward-details-{id}"))
                    .compact()
                    .secondary()
                    .w(px(36.))
                    .h(px(36.))
                    .rounded(theme.control_radius)
                    .icon(if details_open {
                        IconName::ChevronUp
                    } else {
                        IconName::ChevronDown
                    })
                    .accessibility_label(if details_open {
                        crate::locale::text("收起隧道详情", "Collapse tunnel details")
                    } else {
                        crate::locale::text("展开隧道详情", "Expand tunnel details")
                    })
                    .tooltip(if details_open {
                        crate::locale::text("收起详情", "Hide details")
                    } else {
                        crate::locale::text("隧道详情", "Tunnel details")
                    })
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        toggle_detail_state.update(cx, |expanded, cx| {
                            *expanded = !*expanded;
                            cx.notify();
                        });
                    })),
            );

            let copy_state = copy_feedback_state.clone();
            let copied_address = copy_address.clone();
            actions = actions.child(
                forward_action_button(
                    theme,
                    format!("forward-copy-address-{id}"),
                    IconName::Copy,
                    crate::locale::text("复制地址", "Copy address"),
                )
                .with_size(gpui_kit::component::Size::Medium)
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                        copied_address.clone(),
                    ));
                    let verified = cx
                        .read_from_clipboard()
                        .and_then(|item| item.text())
                        .is_some_and(|text| text == copied_address);
                    let message = if verified {
                        crate::locale::text("地址已复制。", "Address copied.")
                    } else {
                        crate::locale::text(
                            "无法核对剪贴板结果；请重试或手动复制地址。",
                            "Clipboard could not be verified; retry or copy the address manually.",
                        )
                    }
                    .to_string();
                    copy_state.update(cx, |feedback, cx| {
                        *feedback = Some(message);
                        cx.notify();
                    });
                })),
            );

            let edit_button = forward_action_button(
                theme,
                format!("forward-edit-{id}"),
                IconName::Pencil,
                crate::locale::text("编辑隧道", "Edit tunnel"),
            )
            .with_size(gpui_kit::component::Size::Medium)
            .disabled(self.busy)
            .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                view.prepare_forward_edit(edit_snapshot.clone(), window, cx)
            }));
            actions = actions.child(edit_button);

            if is_remote_socks {
                let apply_id = id.clone();
                let reconcile_current = proxy_action_state
                    .as_ref()
                    .is_some_and(|state| state.reconcile_current);
                let apply_enabled = proxy_action_state
                    .as_ref()
                    .is_some_and(|state| state.current_enabled)
                    || reconcile_current;
                let apply_tooltip = proxy_action_state
                    .as_ref()
                    .filter(|_| !reconcile_current)
                    .and_then(|state| state.current_reason.clone())
                    .unwrap_or_else(|| {
                        if reconcile_current {
                            crate::locale::text(
                                "核对当前终端代理状态",
                                "Check current terminal proxy state",
                            )
                        } else {
                            crate::locale::text("注入当前终端", "Inject into current terminal")
                        }
                    });
                let current_button = forward_action_button(
                    theme,
                    format!("forward-proxy-current-{id}"),
                    IconName::Terminal,
                    apply_tooltip,
                )
                .with_size(gpui_kit::component::Size::Medium)
                .disabled(!apply_enabled || self.busy)
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    if let Some(panel) = view.proxy_panel.clone() {
                        panel.update(cx, |panel, cx| {
                            if reconcile_current {
                                panel.select_forward(apply_id.clone(), cx);
                                panel.reconcile_current(cx);
                            } else {
                                panel.apply_to_current_forward(apply_id.clone(), cx);
                            }
                        });
                    }
                }));
                actions = actions.child(current_button);

                let future_id = id.clone();
                let future_active = proxy_action_state
                    .as_ref()
                    .is_some_and(|state| state.future_active);
                let future_enabled = proxy_action_state
                    .as_ref()
                    .is_some_and(|state| state.future_enabled);
                let future_tooltip = if future_active {
                    crate::locale::text("关闭新终端自动使用", "Disable proxy for new terminals")
                } else {
                    proxy_action_state
                        .as_ref()
                        .and_then(|state| state.future_reason.clone())
                        .unwrap_or_else(|| {
                            crate::locale::text("新终端自动使用", "Use for new terminals")
                        })
                };
                let mut future_button = forward_action_button(
                    theme,
                    format!("forward-proxy-future-{id}"),
                    IconName::Terminal,
                    future_tooltip,
                )
                .with_size(gpui_kit::component::Size::Medium)
                .disabled(!future_enabled || self.busy)
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    if let Some(panel) = view.proxy_panel.clone() {
                        panel.update(cx, |panel, cx| {
                            panel.toggle_future_for_forward(future_id.clone(), cx)
                        });
                    }
                }));
                if future_active {
                    future_button = future_button.primary();
                }
                actions = actions.child(future_button);

                if let Some(script) = user_setup_script {
                    let script_state = copy_feedback_state.clone();
                    actions = actions.child(
                        forward_action_button(theme,
                            format!("forward-copy-user-setup-{id}"),
                            IconName::Copy,
                            crate::locale::text(
                                "复制用户级配置脚本，需在远端终端手动执行",
                                "Copy user-level setup script; run it manually in the remote terminal",
                            ),
                        )
                        .with_size(gpui_kit::component::Size::Medium)
                        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                script.clone(),
                            ));
                            let verified = cx
                                .read_from_clipboard()
                                .and_then(|item| item.text())
                                .is_some_and(|text| text == script);
                            let message = if verified {
                                crate::locale::text("地址已复制。", "Address copied.")
                            } else {
                                crate::locale::text(
                                    "无法核对剪贴板结果；请重试或手动复制地址。",
                                    "Clipboard could not be verified; retry or copy the address manually.",
                                )
                            }
                            .to_string();
                            script_state.update(cx, |feedback, cx| {
                                *feedback = Some(message);
                                cx.notify();
                            });
                        })),
                    );
                }

                if let Some(script) = user_undo_script {
                    let script_state = copy_feedback_state.clone();
                    actions = actions.child(
                        forward_action_button(theme,
                            format!("forward-copy-user-undo-{id}"),
                            IconName::Copy,
                            crate::locale::text(
                                "复制撤销脚本，需在远端终端手动执行",
                                "Copy undo script; run it manually in the remote terminal",
                            ),
                        )
                        .with_size(gpui_kit::component::Size::Medium)
                        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                script.clone(),
                            ));
                            let verified = cx
                                .read_from_clipboard()
                                .and_then(|item| item.text())
                                .is_some_and(|text| text == script);
                            let message = if verified {
                                crate::locale::text("地址已复制。", "Address copied.")
                            } else {
                                crate::locale::text(
                                    "无法核对剪贴板结果；请重试或手动复制地址。",
                                    "Clipboard could not be verified; retry or copy the address manually.",
                                )
                            }
                            .to_string();
                            script_state.update(cx, |feedback, cx| {
                                *feedback = Some(message);
                                cx.notify();
                            });
                        })),
                    );
                }
            }

            if stopped && forward.persistent {
                actions = actions.child(
                    forward_action_button(
                        theme,
                        format!("forward-start-{id}"),
                        IconName::Play,
                        crate::locale::text("启动隧道", "Start tunnel"),
                    )
                    .with_size(gpui_kit::component::Size::Medium)
                    .disabled(self.busy)
                    .on_click(cx.listener(
                        move |view, _: &ClickEvent, _, cx| {
                            view.start_saved_forward(start_id.clone(), cx)
                        },
                    )),
                );
            } else if !stopped {
                actions = actions.child(
                    forward_action_button(
                        theme,
                        format!("forward-stop-{id}"),
                        IconName::Square,
                        crate::locale::text("停止隧道", "Stop tunnel"),
                    )
                    .with_size(gpui_kit::component::Size::Medium)
                    .disabled(self.busy)
                    .on_click(cx.listener(
                        move |view, _: &ClickEvent, _, cx| view.stop_forward(stop_id.clone(), cx),
                    )),
                );
            }

            if forward.persistent {
                actions = actions.child(
                    forward_action_button(
                        theme,
                        format!("forward-delete-{id}"),
                        IconName::Trash,
                        crate::locale::text("删除隧道", "Delete tunnel"),
                    )
                    .text_color(theme.error)
                    .with_size(gpui_kit::component::Size::Medium)
                    .disabled(self.busy)
                    .on_click(cx.listener(
                        move |view, _: &ClickEvent, _, cx| {
                            view.delete_forward_definition(delete_definition.clone(), cx)
                        },
                    )),
                );
            } else {
                let delete_runtime_id = id.clone();
                actions = actions.child(
                    forward_action_button(
                        theme,
                        format!("forward-delete-{id}"),
                        IconName::Trash,
                        crate::locale::text("删除隧道", "Delete tunnel"),
                    )
                    .text_color(theme.error)
                    .with_size(gpui_kit::component::Size::Medium)
                    .disabled(self.busy)
                    .on_click(cx.listener(
                        move |view, _: &ClickEvent, _, cx| {
                            view.stop_forward(delete_runtime_id.clone(), cx)
                        },
                    )),
                );
            }

            if let Some(feedback) = proxy_feedback {
                actions = actions.child(
                    div()
                        .w_full()
                        .text_size(theme.text_xs)
                        .text_color(if feedback.is_error {
                            theme.error
                        } else {
                            theme.muted
                        })
                        .child(feedback.message),
                );
            }
            if let Some(feedback) = copy_feedback {
                actions = actions.child(
                    div()
                        .w_full()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(feedback),
                );
            }
            if details_open {
                let direction = match kind {
                    ForwardKind::Local => crate::locale::text("本机 → 主机", "Local → host"),
                    ForwardKind::Remote => crate::locale::text("主机 → 本机", "Host → local"),
                    ForwardKind::Dynamic => {
                        crate::locale::text("本机 → 主机网络", "Local → host network")
                    }
                    ForwardKind::RemoteDynamic => {
                        crate::locale::text("主机 → 本机网络", "Host → local network")
                    }
                };
                let origin = if forward.persistent {
                    crate::locale::text("手动", "Manual")
                } else {
                    crate::locale::text("MCP 工具", "MCP tool")
                };
                let mut details = div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .mt_3()
                    .pl(px(28.))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(forward_detail_fact(
                                crate::locale::text("方向", "Direction"),
                                direction.to_string(),
                                theme,
                            ))
                            .child(forward_detail_fact(
                                crate::locale::text("来源", "Source"),
                                origin.to_string(),
                                theme,
                            ))
                            .child(forward_detail_fact(
                                crate::locale::text("主机端点", "Host endpoint"),
                                host_endpoint.clone(),
                                theme,
                            ))
                            .child(forward_detail_fact(
                                crate::locale::text("本机端点", "Local endpoint"),
                                local_endpoint.clone(),
                                theme,
                            )),
                    );
                if let Some(proxy_url) = proxy_url.as_ref()
                    && is_remote_socks
                {
                    details = details.child(
                        div()
                            .w_full()
                            .min_w_0()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(theme.secondary)
                            .text_size(theme.text_xs)
                            .font_family("monospace")
                            .text_color(theme.muted)
                            .child(proxy_url.clone()),
                    );
                }
                if is_remote_socks && has_user_proxy_scripts {
                    details = details.child(
                        div()
                            .w_full()
                            .p_2()
                            .rounded_sm()
                            .border_1()
                            .border_color(theme.accent.opacity(0.2))
                            .bg(theme.accent.opacity(0.1))
                            .text_size(theme.text_xs)
                            .text_color(theme.accent)
                            .child(crate::locale::text(
                                "脚本只写当前用户 home，不需要 root；提供备份和撤销脚本。",
                                "Scripts write only under the current user's home, need no root, and provide backup and undo steps.",
                            )),
                    );
                }
                row = row.child(details);
            }
            row = row.child(actions);
            sessions = sessions.child(row);
        }
        body = body.child(sessions);

        section_page(
            crate::locale::text("SSH 隧道", "SSH tunnels"),
            body,
            theme,
            self.compact_layout,
            self.section_selection.is_fixed(),
        )
    }

    /// Root 提供单一玻璃表面、遮罩与焦点；正文保持透明，避免遮住背景采样并在圆角外漏出矩形底色。
    /// 表单只对齐 ModalShell 内容间距并独立于右栏布局。
    /// 宽度封顶 1024px，低于 720px 时纵向排列端点，避免窄窗口压缩输入控件；忙碌时继续等待真实用例回执。
    /// 标题、校验反馈与操作栏固定，只有字段正文滚动；高度使用 Root 扣除边框后的预算。
    ///
    /// @author kongweiguang
    pub(in crate::features::operations) fn render_forward_form_modal(
        &self,
        theme: ThemeTokens,
        available_width: f32,
        available_height: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(inputs) = self.inputs.as_ref() else {
            return div()
                .p_4()
                .text_color(theme.foreground)
                .child(crate::locale::text(
                    "表单输入尚未初始化，请关闭后重新打开。",
                    "Form inputs are unavailable. Close this dialog and reopen it.",
                ))
                .into_any_element();
        };

        let compact_layout = available_width.min(1024.) < 720.;
        let kind = self.forward_kind;
        let dynamic = matches!(kind, ForwardKind::Dynamic | ForwardKind::RemoteDynamic);
        let remote_listener = matches!(kind, ForwardKind::Remote | ForwardKind::RemoteDynamic);
        let host_id = self.forward_form_host_id();
        let host_summary = host_id.as_ref().map_or_else(
            || {
                div()
                    .mt_1()
                    .text_size(px(13.))
                    .line_height(px(20.))
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        "表单没有固定 SSH 主机；请取消后从明确的 SSH 主机重新打开。",
                        "This form has no frozen SSH host. Cancel it and reopen from an explicit SSH host.",
                    ))
                    .into_any_element()
            },
            |host_id| {
                let host_label = self
                    .host_subtitles
                    .get(host_id)
                    .map(|subtitle| forward_host_endpoint_label(subtitle.as_ref()).to_owned())
                    .unwrap_or_else(|| {
                        forward_host_label(
                            host_id,
                            Some(super::server::preferred_host_display_name(
                                host_id,
                                &self.host_display_names,
                            )),
                        )
                    });
                let host_id_details = forward_host_id_detail(host_id);
                let accessible_label = crate::locale::text(
                    format!("{host_label}，{host_id_details}"),
                    format!("{host_label}; {host_id_details}"),
                );
                div()
                    .id("operations-forward-frozen-host-summary")
                    .mt_1()
                    .text_size(px(13.))
                    .line_height(px(20.))
                    .text_color(theme.muted)
                    .child(host_label)
                    .tooltip(move |window, cx| {
                        Tooltip::new(host_id_details.clone()).build(window, cx)
                    })
                    .aria_label(accessible_label)
                    .into_any_element()
            },
        );
        let disabled = self.busy || self.forward_save_pending.is_some();
        let current_scenario = forward_scenario_card(kind);
        let mut scenario_picker = div().w_full().flex().flex_wrap().gap_2();
        let selected_scenario_border = theme.tunnel_action_hover.opacity(0.30);
        for (scenario, title, description, option) in [
            (
                ForwardScenarioCard::HostService,
                crate::locale::text("访问主机服务", "Reach a host service"),
                crate::locale::text(
                    "把主机目标服务映射到本机端口",
                    "Forward a host service to a local port",
                ),
                "-L",
            ),
            (
                ForwardScenarioCard::LocalService,
                crate::locale::text("暴露本机服务", "Expose a local service"),
                crate::locale::text(
                    "把本机服务映射到 SSH 主机端口",
                    "Forward a local service to a port on the SSH host",
                ),
                "-R",
            ),
            (
                ForwardScenarioCard::Socks,
                crate::locale::text("SOCKS / 高级", "SOCKS / Advanced"),
                crate::locale::text(
                    "选择 SOCKS5 代理方向",
                    "Choose where the SOCKS5 proxy listens",
                ),
                "-D / remote -R",
            ),
        ] {
            let selected = scenario == current_scenario;
            let next_kind = forward_kind_for_scenario(scenario, kind);
            scenario_picker = scenario_picker.child(
                BaseButton::new(format!("forward-scenario-{scenario:?}"))
                    .flex_1()
                    .min_w(px(210.))
                    .min_h(px(80.))
                    .flex()
                    .flex_col()
                    .items_start()
                    .justify_start()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.tool_panel_border)
                    .bg(theme.muted_surface())
                    .selected(selected)
                    .styles(|styles| {
                        styles
                            .selected(|style| {
                                style
                                    .border_color(selected_scenario_border)
                                    .bg(theme.surface_selected())
                                    .text_color(theme.selected_foreground)
                                    .shadow_sm()
                            })
                            .disabled(|style| style.opacity(0.55))
                    })
                    .when(!disabled && !selected, |button| {
                        button.hover(|style| {
                            style
                                .border_color(theme.tool_panel_border)
                                .bg(theme.surface_hover())
                        })
                    })
                    .focus_visible(|style| style.border_2().border_color(theme.accent))
                    .accessibility_label(crate::locale::text(
                        format!("{}，{}", title, description),
                        format!("{}; {}", title, description),
                    ))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .text_color(if selected {
                                        theme.selected_foreground
                                    } else {
                                        theme.muted
                                    })
                                    .child(title),
                            )
                            .child(
                                div()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded_md()
                                    .bg(theme.surface_hover())
                                    .text_size(px(10.))
                                    .text_color(theme.foreground.opacity(0.84))
                                    .font_family("monospace")
                                    .child(option),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .line_height(px(16.))
                            .text_color(theme.muted)
                            .child(description),
                    )
                    .disabled(disabled)
                    .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                        view.switch_forward_kind(next_kind, window, cx);
                    })),
            );
        }

        let cancel_disabled = self.busy;
        let header = div()
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_start()
            .justify_between()
            .gap_4()
            .px_5()
            .py_4()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(17.))
                            .line_height(px(24.))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(if self.forward_edit_expected.is_some() {
                                crate::locale::text("编辑 SSH 隧道", "Edit SSH tunnel")
                            } else {
                                crate::locale::text("添加 SSH 隧道", "Add SSH tunnel")
                            }),
                    )
                    .child(host_summary),
            )
            .child(
                Button::new("forward-form-close")
                    .ghost()
                    .w(px(36.))
                    .h(px(36.))
                    .icon(IconName::X)
                    .accessibility_label(crate::locale::text("关闭弹窗", "Close dialog"))
                    .tooltip(crate::locale::text("关闭弹窗", "Close dialog"))
                    .disabled(cancel_disabled)
                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                        view.cancel_forward_form(cx);
                    })),
            );
        let mut content = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_4()
            .px_5()
            .py_4()
            .child(scenario_picker)
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(field_label(crate::locale::text("名称", "Name"), theme))
                    .child(forward_form_input(&inputs.forward_name, theme, disabled)),
            );

        let local_selected = kind == ForwardKind::Dynamic;
        let direction_buttons = div()
            .w_full()
            .flex()
            .gap_1()
            .p_1()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.tool_panel_border)
            .bg(theme.field_surface())
            .child(
                Button::new("forward-socks-local")
                    .compact()
                    .flex_1()
                    .rounded_lg()
                    .px_2()
                    .py_1p5()
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .custom(
                        ButtonCustomVariant::new(cx)
                            .color(if local_selected {
                                theme.surface_selected()
                            } else {
                                theme.panel.alpha(0.)
                            })
                            .foreground(if local_selected {
                                theme.selected_foreground
                            } else {
                                theme.muted
                            })
                            .hover(if local_selected {
                                theme.surface_selected()
                            } else {
                                theme.surface_hover()
                            })
                            .active(theme.surface_selected()),
                    )
                    .label(crate::locale::text("本机 -D", "Local -D"))
                    .disabled(disabled)
                    .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                        view.switch_forward_kind(forward_socks_kind(false), window, cx);
                    })),
            )
            .child(
                Button::new("forward-socks-remote")
                    .compact()
                    .flex_1()
                    .rounded_lg()
                    .px_2()
                    .py_1p5()
                    .text_size(px(12.))
                    .line_height(px(16.))
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .custom(
                        ButtonCustomVariant::new(cx)
                            .color(if !local_selected {
                                theme.surface_selected()
                            } else {
                                theme.panel.alpha(0.)
                            })
                            .foreground(if !local_selected {
                                theme.selected_foreground
                            } else {
                                theme.muted
                            })
                            .hover(if !local_selected {
                                theme.surface_selected()
                            } else {
                                theme.surface_hover()
                            })
                            .active(theme.surface_selected()),
                    )
                    .label(crate::locale::text("远端 SOCKS", "Remote SOCKS"))
                    .disabled(disabled)
                    .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                        view.switch_forward_kind(forward_socks_kind(true), window, cx);
                    })),
            );

        let bind_host = self.input_value(&inputs.forward_bind_host, cx);
        let bind_port = self.input_value(&inputs.forward_bind_port, cx);
        let bind_mode = inputs
            .forward_bind_scope
            .read(cx)
            .selected_value()
            .copied()
            .unwrap_or_else(|| bind_scope_mode(&bind_host));
        let bind_fields = bind_address_fields(
            BindAddressFieldSpec {
                scope_label: crate::locale::text(
                    if remote_listener {
                        "主机监听范围"
                    } else {
                        "本机监听范围"
                    },
                    if remote_listener {
                        "Host listen scope"
                    } else {
                        "Local listen scope"
                    },
                ),
                port_label: crate::locale::text(
                    match kind {
                        ForwardKind::Dynamic => "本机 SOCKS 端口",
                        ForwardKind::RemoteDynamic => "主机 SOCKS 端口",
                        ForwardKind::Local => "本机监听端口",
                        ForwardKind::Remote => "主机监听端口",
                    },
                    match kind {
                        ForwardKind::Dynamic => "Local SOCKS port",
                        ForwardKind::RemoteDynamic => "Host SOCKS port",
                        ForwardKind::Local => "Local listen port",
                        ForwardKind::Remote => "Host listen port",
                    },
                ),
                scope_select: &inputs.forward_bind_scope,
                selected_scope: bind_mode,
                host_input: &inputs.forward_bind_host,
                port_input: &inputs.forward_bind_port,
                id_prefix: "forward-bind",
                disabled,
            },
            theme,
        );

        let destination_fields = endpoint_fields(
            crate::locale::text(
                if remote_listener {
                    "本机目标地址"
                } else {
                    "主机目标地址"
                },
                if remote_listener {
                    "Local destination address"
                } else {
                    "Host destination address"
                },
            ),
            &inputs.forward_destination_host,
            &inputs.forward_destination_port,
            crate::locale::text(
                if remote_listener {
                    "本机目标端口"
                } else {
                    "主机目标端口"
                },
                if remote_listener {
                    "Local destination port"
                } else {
                    "Host destination port"
                },
            ),
            theme,
            "forward-destination",
            disabled,
        )
        .into_any_element();
        let host_socks_hint = endpoint_value_preview(
            crate::locale::text("出口", "Egress"),
            crate::locale::text("主机网络", "Host network"),
            theme,
        );
        let local_socks_hint = endpoint_value_preview(
            crate::locale::text("远端注入", "Remote setup"),
            forward_socks_proxy_preview(&bind_host, &bind_port),
            theme,
        );
        let (host_content, local_endpoint_content) = match kind {
            ForwardKind::Local => (destination_fields, bind_fields),
            ForwardKind::Remote => (bind_fields, destination_fields),
            ForwardKind::Dynamic => (host_socks_hint, bind_fields),
            ForwardKind::RemoteDynamic => (bind_fields, local_socks_hint),
        };
        let local_content = if dynamic {
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_3()
                .child(direction_buttons)
                .child(local_endpoint_content)
                .into_any_element()
        } else {
            local_endpoint_content
        };
        let (host_title, local_title) = match kind {
            ForwardKind::Local => (
                crate::locale::text("主机目标服务", "Host destination service"),
                crate::locale::text("本机监听", "Local listener"),
            ),
            ForwardKind::Remote => (
                crate::locale::text("主机监听", "Host listener"),
                crate::locale::text("本机服务", "Local service"),
            ),
            ForwardKind::Dynamic => (
                crate::locale::text("主机网络出口", "Host network egress"),
                crate::locale::text("本机 SOCKS", "Local SOCKS"),
            ),
            ForwardKind::RemoteDynamic => (
                crate::locale::text("主机 SOCKS", "Host SOCKS"),
                crate::locale::text("本机网络出口", "Local network egress"),
            ),
        };
        let (host_description, local_description) = match kind {
            ForwardKind::Local => (
                crate::locale::text("SSH 连接另一侧", "Across the SSH connection"),
                crate::locale::text("本机入口", "Local entry point"),
            ),
            ForwardKind::Remote => (
                crate::locale::text("远端入口", "Remote entry point"),
                crate::locale::text("本机真实服务", "Local service"),
            ),
            ForwardKind::Dynamic => (
                crate::locale::text("经由 SSH 主机", "Through the SSH host"),
                crate::locale::text("本机应用配置 SOCKS", "Local apps use SOCKS"),
            ),
            ForwardKind::RemoteDynamic => (
                crate::locale::text("远端 SOCKS 代理", "Remote SOCKS proxy"),
                crate::locale::text("本机网络出口", "Local network egress"),
            ),
        };
        let host_panel = endpoint_panel(
            crate::locale::text("主机", "Host"),
            host_title,
            host_description,
            host_content,
            theme,
        );
        let local_panel = endpoint_panel(
            crate::locale::text("本机", "This device"),
            local_title,
            local_description,
            local_content,
            theme,
        );
        let (flow_label, openssh_label) = forward_direction_labels(kind);
        let arrow_icon = match (compact_layout, remote_listener) {
            (true, true) => IconName::ArrowDown,
            (true, false) => IconName::ArrowUp,
            (false, true) => IconName::ArrowRight,
            (false, false) => IconName::ArrowLeft,
        };
        let direction_arrow = div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .when(compact_layout, |view| view.w_full())
            .when(!compact_layout, |view| view.min_w(px(56.)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p_2()
                    .rounded_full()
                    .border_1()
                    .border_color(theme.tunnel_action_hover.opacity(0.25))
                    .bg(theme.surface_selected())
                    .text_color(theme.selected_foreground)
                    .shadow_sm()
                    .child(Icon::new(arrow_icon).size(px(16.))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .text_size(px(11.))
                    .line_height(px(16.))
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .text_color(theme.muted)
                    .child(flow_label)
                    .child(div().font_family("monospace").child(openssh_label)),
            );
        let mut endpoints = div().w_full().gap_2();
        endpoints = if compact_layout {
            endpoints.flex().flex_col()
        } else {
            endpoints.flex().items_stretch()
        };
        content = content.child(
            endpoints
                .child(host_panel)
                .child(direction_arrow)
                .child(local_panel),
        );

        if is_exposed_bind(&bind_host) {
            let warning = if remote_listener {
                crate::locale::text(
                    "该地址允许 SSH 主机上的其他设备连接；服务端策略可能仍会限制监听。",
                    "Other devices may reach this listener on the SSH host; server policy can still restrict it.",
                )
            } else {
                crate::locale::text(
                    "该地址可能允许本机网络中的其他设备连接。",
                    "Other devices on the local network may be able to connect.",
                )
            };
            content = content.child(
                div()
                    .w_full()
                    .p_3()
                    .rounded_md()
                    .bg(theme.warning.opacity(0.12))
                    .text_size(theme.text_xs)
                    .text_color(theme.warning)
                    .child(warning),
            );
        }

        if let Some(status) = self.status.as_ref() {
            content = content.child(
                div()
                    .id("forward-form-feedback")
                    .w_full()
                    .p_3()
                    .rounded_md()
                    .bg(theme.error.opacity(0.1))
                    .text_size(theme.text_xs)
                    .text_color(theme.error)
                    .child(status.clone()),
            );
        }

        let mut footer = div()
            .w_full()
            .flex_shrink_0()
            .flex()
            .px_5()
            .py_4()
            .border_t_1()
            .border_color(theme.border);
        let disabled =
            self.busy || (self.forward_save_pending.is_none() && !self.can_save_forward());
        // main 用与 floating-surface 相同的颜色作为不透明 surface-solid 按钮表面。
        let solid_action_surface = theme.floating_surface.alpha(1.);
        let footer_action_style = ButtonCustomVariant::new(cx)
            .color(solid_action_surface)
            .foreground(theme.foreground)
            .hover(theme.surface_hover())
            .active(theme.surface_hover());
        footer = footer.child(
            div()
                .w_full()
                .flex()
                .flex_wrap()
                .items_center()
                .justify_end()
                .gap_2()
                .child(
                    Button::new("forward-cancel")
                        .h(px(36.))
                        .px(px(14.))
                        .text_size(px(16.))
                        .line_height(px(24.))
                        .rounded(theme.control_radius)
                        .border_1()
                        .border_color(theme.tool_panel_border)
                        .custom(footer_action_style)
                        .label(crate::locale::text("取消", "Cancel"))
                        .disabled(cancel_disabled)
                        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                            view.cancel_forward_form(cx);
                        })),
                )
                .child(
                    Button::new("forward-start")
                        .h(px(36.))
                        .px(px(14.))
                        .text_size(px(16.))
                        .line_height(px(24.))
                        .rounded(theme.control_radius)
                        .border_1()
                        .border_color(theme.tool_panel_border)
                        .custom(footer_action_style)
                        .when(
                            self.forward_edit_expected.is_none() && !self.busy,
                            |button| button.icon(IconName::Plus),
                        )
                        .label(if self.busy {
                            crate::locale::text("正在处理…", "Working…")
                        } else if self.forward_edit_expected.is_some() {
                            crate::locale::text("保存修改", "Save changes")
                        } else {
                            crate::locale::text("开启隧道", "Start tunnel")
                        })
                        .disabled(disabled)
                        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                            if view.forward_save_pending.is_some() {
                                view.reconcile_forward_save(cx);
                            } else {
                                view.save_forward(true, cx);
                            }
                        })),
                ),
        );

        div()
            .id("forward-form-modal")
            .w_full()
            .max_h(px(available_height))
            .min_h(px(0.))
            .flex()
            .flex_col()
            .text_color(theme.foreground)
            .when_some(self.confirmation_focus.as_ref(), |view, focus| {
                view.track_focus(focus)
            })
            .child(header)
            .child(
                div()
                    .id("forward-form-modal-content")
                    .w_full()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .child(content),
            )
            .child(footer)
            .into_any_element()
    }
}

/// Kit 的 Input::h 只控制多行编辑器；单行输入由外层样式精炼设定尺寸与字段表面，并保留 Kit 焦点环。
///
/// @author kongweiguang
fn forward_form_input(
    input: &Entity<gpui_kit::component::input::InputState>,
    theme: ThemeTokens,
    disabled: bool,
) -> AnyElement {
    gpui_kit::Styled::h(Input::new(input).disabled(disabled).w_full(), px(36.))
        .px(px(12.))
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.strong_border())
        .bg(theme.field_surface())
        .text_size(px(12.))
        .line_height(px(16.))
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .when(!disabled, |input| {
            input.hover_style(gpui_kit::StyleRefinement::default().bg(theme.field_hover_surface()))
        })
        .into_any_element()
}

/// 服务地址和端口固定上下全宽，避免宽屏并排挤压字段并保持表单阅读顺序。
///
/// @author kongweiguang
fn endpoint_fields(
    host_label: impl IntoElement,
    host_input: &Entity<gpui_kit::component::input::InputState>,
    port_input: &Entity<gpui_kit::component::input::InputState>,
    port_label: impl IntoElement,
    theme: ThemeTokens,
    id_prefix: &'static str,
    disabled: bool,
) -> impl IntoElement {
    div()
        .id(id_prefix)
        .w_full()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(field_label(host_label, theme))
                .child(forward_form_input(host_input, theme, disabled)),
        )
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(field_label(port_label, theme))
                .child(forward_form_input(port_input, theme, disabled)),
        )
}

/// 同一监听端点的字段状态与动作门禁作为一个参数传递，避免调用处错配；只有自定义模式显示地址。
///
/// @author kongweiguang
struct BindAddressFieldSpec<'a> {
    scope_label: SharedString,
    port_label: SharedString,
    scope_select: &'a Entity<gpui_kit::component::select::SelectState<Vec<ForwardBindScopeOption>>>,
    selected_scope: ForwardBindScope,
    host_input: &'a Entity<gpui_kit::component::input::InputState>,
    port_input: &'a Entity<gpui_kit::component::input::InputState>,
    id_prefix: &'static str,
    disabled: bool,
}

/// 同一监听端点由一个规格驱动，确保范围选择、地址输入和端口状态保持配对。
///
/// @author kongweiguang
fn bind_address_fields(fields: BindAddressFieldSpec<'_>, theme: ThemeTokens) -> AnyElement {
    let BindAddressFieldSpec {
        scope_label,
        port_label,
        scope_select,
        selected_scope,
        host_input,
        port_input,
        id_prefix,
        disabled,
    } = fields;
    let scope_field = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(field_label(scope_label.clone(), theme))
        .child(
            Select::new(scope_select)
                .w_full()
                .h(px(36.))
                .rounded(theme.control_radius)
                .border_1()
                .border_color(theme.strong_border())
                .bg(theme.field_surface())
                .text_size(px(16.))
                .line_height(px(24.))
                .when(!disabled, |select| {
                    select.hover_style(
                        gpui_kit::StyleRefinement::default().bg(theme.field_hover_surface()),
                    )
                })
                .disabled(disabled)
                .accessibility_label(scope_label),
        );
    let mut address = div().w_full().flex().flex_col().gap_2().child(scope_field);
    if selected_scope == ForwardBindScope::Custom {
        address = address.child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(field_label(
                    crate::locale::text("自定义监听地址", "Custom listen address"),
                    theme,
                ))
                .child(forward_form_input(host_input, theme, disabled)),
        );
    }

    div()
        .id(id_prefix)
        .w_full()
        .flex()
        .flex_col()
        .gap_3()
        .child(address)
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(field_label(port_label, theme))
                .child(forward_form_input(port_input, theme, disabled)),
        )
        .into_any_element()
}

/// 将主机侧/本机侧说明包装为同一张轻量面板，方向卡只改变映射而不改变字段契约。
///
/// @author kongweiguang
fn endpoint_panel(
    side: impl IntoElement,
    title: impl IntoElement,
    description: impl IntoElement,
    content: AnyElement,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .items_start()
        .gap_3()
        .p_3()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.tool_panel_border)
        .child(
            div()
                .flex()
                .px_2()
                .py_1()
                .rounded_lg()
                .border_1()
                .border_color(theme.tool_panel_border)
                .bg(theme.field_surface())
                .text_size(px(11.))
                .line_height(px(16.5))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(theme.foreground.opacity(0.84))
                .child(side),
        )
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(14.))
                        .line_height(px(20.))
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .line_height(px(16.))
                        .text_color(theme.muted)
                        .child(description),
                ),
        )
        .child(div().w_full().child(content))
}

/// 将 main 的只读 SOCKS 预览放入端点字段布局，不把预览误呈现为可编辑的服务目标。
///
/// @author kongweiguang
fn endpoint_value_preview(
    label: impl IntoElement,
    value: impl IntoElement,
    theme: ThemeTokens,
) -> AnyElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(field_label(label, theme))
        .child(
            div()
                .w_full()
                .px_3()
                .py_2()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.tool_panel_border)
                .bg(theme.field_surface())
                .text_size(theme.text_xs)
                .line_height(px(20.))
                .font_family("monospace")
                .text_color(theme.foreground)
                .whitespace_normal()
                .child(value),
        )
        .into_any_element()
}

/// 预览按 main 归一化远端可达地址并做 POSIX 单引号转义；只生成提示文本，不改变隧道配置。
///
/// @author kongweiguang
fn forward_socks_proxy_preview(bind_host: &str, bind_port: &str) -> String {
    let host = match bind_host.trim() {
        "" | "0.0.0.0" => "127.0.0.1",
        "::" => "::1",
        host => host,
    };
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let port = bind_port
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|port| *port != 0.0 && !port.is_nan())
        .map_or_else(
            || "0".to_owned(),
            |port| {
                if port.is_infinite() {
                    if port.is_sign_negative() {
                        "-Infinity".to_owned()
                    } else {
                        "Infinity".to_owned()
                    }
                } else {
                    port.to_string()
                }
            },
        );
    let proxy_url = format!("socks5h://{host}:{port}");
    let quoted_proxy_url = format!("'{}'", proxy_url.replace('\'', "'\\''"));
    format!("export ALL_PROXY={quoted_proxy_url}")
}

/// main 的表单字段标签为 12px、中等字重和 16px 行高，主题色来自语义令牌。
///
/// @author kongweiguang
fn field_label(label: impl IntoElement, theme: ThemeTokens) -> impl IntoElement {
    div()
        .text_size(theme.text_xs)
        .line_height(px(16.))
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .text_color(theme.muted)
        .child(label)
}

/// 以保存名称作为可见主机标签；无名称或仅有 ID 时回退为中性文案，并去掉已有 SSH 展示前缀。
///
/// @author kongweiguang
fn forward_host_label(
    host_id: &kerminal_domain::identity::HostId,
    display_name: Option<&str>,
) -> String {
    let display_name = display_name.map(str::trim).filter(|name| !name.is_empty());
    let display_name = display_name
        .and_then(|name| name.strip_prefix("SSH ·").map(str::trim).or(Some(name)))
        .filter(|name| !name.is_empty() && *name != host_id.as_str());
    display_name.map_or_else(|| localized_text("SSH 主机", "SSH host"), str::to_owned)
}

/// 主机导航的 SSH 副标题已带协议后缀；隧道 UI 与 main 一样只显示连接端点。
///
/// @author kongweiguang
fn forward_host_endpoint_label(subtitle: &str) -> &str {
    subtitle.strip_suffix(" · SSH").unwrap_or(subtitle)
}

/// 完整 HostId 仅用于可展开身份详情，不将内部标识放进列表或表单的可见主标题。
///
/// @author kongweiguang
fn forward_host_id_detail(host_id: &kerminal_domain::identity::HostId) -> String {
    crate::locale::text(
        format!("主机 ID：{}", host_id.as_str()),
        format!("Host ID: {}", host_id.as_str()),
    )
    .to_string()
}

/// 非回环或无法判定的绑定地址默认提示风险；不会替用户改写监听目标。
///
/// @author kongweiguang
fn is_exposed_bind(host: &str) -> bool {
    let host = host.trim();
    if host.eq_ignore_ascii_case("localhost") {
        return false;
    }
    match host.parse::<IpAddr>() {
        Ok(address) => !address.is_loopback(),
        Err(_) => !host.is_empty(),
    }
}

/// 从实际输入推导范围选择态，避免范围标签与提交给 SSH 的监听地址发生漂移。
///
/// @author kongweiguang
fn bind_scope_mode(host: &str) -> ForwardBindScope {
    ForwardBindScope::from_host(host)
}

/// 展开信息统一字段对齐并允许长地址换行，窄栏和宽栏共用同一语义层级。
///
/// @author kongweiguang
fn forward_detail_fact(
    label: SharedString,
    value: impl Into<SharedString>,
    theme: ThemeTokens,
) -> impl IntoElement {
    let value: SharedString = value.into();
    div()
        .min_w_0()
        .flex_1()
        .min_w(px(200.))
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .w_full()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(label),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(theme.text_xs)
                .font_family("monospace")
                .text_color(theme.foreground)
                .child(value),
        )
}

/// 按 main 的端点语义投影监听端和目的端；SOCKS 的动态目的地使用固定说明文案。
///
/// @author kongweiguang
fn forward_session_endpoints(
    forward: &kerminal_application::operations::ManagedPortForwardSnapshot,
) -> (String, String) {
    let config = &forward.forward.config;
    let listener = forward
        .actual_bind
        .as_ref()
        .unwrap_or(&config.bind)
        .to_string();
    let destination = || {
        config
            .destination
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| localized_text("未设置", "Not set"))
    };

    match config.kind {
        ForwardKind::Local => (destination(), listener),
        ForwardKind::Remote => (listener, destination()),
        ForwardKind::Dynamic => (
            localized_text("主机网络出口", "Host network egress"),
            listener,
        ),
        ForwardKind::RemoteDynamic => (
            listener,
            localized_text("Kerminal 本机网络出口", "Kerminal local network egress"),
        ),
    }
}

/// 远端 SOCKS 优先复制运行态 URL；停止状态按 main 从配置监听地址构造可复用 URL。
///
/// @author kongweiguang
fn forward_proxy_url(
    forward: &kerminal_application::operations::ManagedPortForwardSnapshot,
) -> Option<String> {
    if forward.forward.config.kind != ForwardKind::RemoteDynamic {
        return None;
    }
    if let Some(proxy_url) = forward
        .proxy_url
        .as_deref()
        .filter(|proxy_url| proxy_url.starts_with("socks5h://"))
    {
        return Some(proxy_url.to_owned());
    }

    let bind = forward
        .actual_bind
        .as_ref()
        .unwrap_or(&forward.forward.config.bind);
    let host = match bind.host.as_str() {
        "0.0.0.0" => "127.0.0.1",
        "::" => "::1",
        host => host,
    };
    kerminal_domain::operations::ForwardEndpoint::new(host, bind.port.get())
        .ok()
        .map(|endpoint| format!("socks5h://{endpoint}"))
}

/// 复制语义与 main 一致：远端普通转发取主机监听端点，其余取本机端点。
///
/// @author kongweiguang
fn forward_copy_address(
    forward: &kerminal_application::operations::ManagedPortForwardSnapshot,
) -> String {
    if let Some(proxy_url) = forward_proxy_url(forward) {
        return proxy_url;
    }
    let (host_endpoint, local_endpoint) = forward_session_endpoints(forward);
    if forward.forward.config.kind == ForwardKind::Remote {
        host_endpoint
    } else {
        local_endpoint
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/operations_render_forwarding.rs"]
mod tests;
