//! SSH 端口转发的紧凑列表与根模态表单。
//!
//! @author kongweiguang

use super::super::OperationsView;
use super::{localized_text, section_feedback, section_page, should_suppress_ready_empty_feedback};
use crate::theme::ThemeTokens;
use crate::ui_button::icon_button;
use gpui_kit::{
    AnyElement, ClickEvent, Context, Entity, IntoElement, SharedString, Window,
    assets::IconName,
    base::Button as BaseButton,
    component::{
        Disableable, Sizable as _,
        button::{Button, ButtonVariants},
        input::Input,
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::operations::PortForwardDefinition;
use kerminal_domain::operations::{
    ForwardKind, ForwardState, ProxyApplicationOutcome, ProxyApplicationReason, RemoteAccessScope,
};
use std::net::IpAddr;

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

impl OperationsView {
    /// 标题卡集中承载目标与操作；仅把对象明确的刷新、编辑和复制收为图标。
    /// 删除和启动保留文字以说明后果，行详情继续按稳定 ID 投影。
    ///
    /// @author kongweiguang
    pub(super) fn render_forwards(
        &self,
        theme: ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rows = self.forwards.data().cloned().unwrap_or_default();
        let has_data = self.forwards.data().is_some();
        let empty = has_data && rows.is_empty();
        let ready_empty = empty
            && self.forwards.phase() == super::super::state::SectionPhase::Ready
            && self.forwards.error().is_none();
        let show_empty_feedback =
            empty && !should_suppress_ready_empty_feedback(self.forwards.phase(), has_data, empty);
        let selected_proxy_forward = self
            .proxy_panel
            .as_ref()
            .and_then(|panel| panel.read(cx).selected_forward_id().map(str::to_owned));
        let mut proxy_panel_in_details = false;
        let target_label = self.tool_rail_target_label();
        let count_label = if has_data {
            crate::locale::text(
                format!("{} 个隧道", rows.len()),
                format!("{} tunnels", rows.len()),
            )
        } else {
            crate::locale::text("等待读取", "Not loaded")
        };
        let mut body = div().w_full().flex().flex_col().gap_3();
        body = body.child(
            div()
                .id("operations-forward-header-card")
                .w_full()
                .flex()
                .flex_col()
                .gap_3()
                .p_3()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.panel)
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(180.))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(theme.text_heading)
                                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                        .text_color(theme.foreground)
                                        .child(crate::locale::text("SSH 隧道", "SSH tunnels")),
                                )
                                .child(
                                    div()
                                        .text_size(theme.text_sm)
                                        .text_color(theme.muted)
                                        .child(target_label),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_size(theme.text_sm)
                                        .text_color(theme.muted)
                                        .child(count_label),
                                )
                                .child(
                                    Button::new("operations-forward-add")
                                        .compact()
                                        .h(px(32.))
                                        .primary()
                                        .icon(IconName::Plus)
                                        .label(crate::locale::text("添加", "Add"))
                                        .disabled(
                                            self.busy
                                                || self.forward_save_pending.is_some()
                                                || !self.can_add_forward(),
                                        )
                                        .on_click(cx.listener(
                                            |view, _: &ClickEvent, window, cx| {
                                                view.prepare_forward_add(window, cx);
                                            },
                                        )),
                                )
                                .child(
                                    icon_button(
                                        "operations-forwards-refresh",
                                        IconName::RefreshCw,
                                        crate::locale::text("刷新隧道列表", "Refresh tunnel list"),
                                    )
                                    .with_size(gpui_kit::component::Size::Medium)
                                    .h(px(32.))
                                    .disabled(
                                        self.forwards.phase()
                                            == super::super::state::SectionPhase::Loading,
                                    )
                                    .on_click(cx.listener(
                                        |view, _: &ClickEvent, _, cx| view.load_forwards(cx),
                                    )),
                                ),
                        ),
                ),
        );

        body = body.child(section_feedback(
            &self.forwards,
            show_empty_feedback,
            crate::locale::text(
                "暂无转发定义。添加后可单独启动或停止。",
                "No forward definitions yet. Add one, then start or stop it here.",
            ),
            theme,
        ));

        if ready_empty {
            body = body.child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .p_5()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.panel)
                    .child(
                        div()
                            .text_size(theme.text_body)
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .text_color(theme.foreground)
                            .child(crate::locale::text(
                                "当前主机暂无 SSH 隧道",
                                "This host has no SSH tunnels yet",
                            )),
                    )
                    .child(
                        div()
                            .text_size(theme.text_sm)
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "添加后可单独启动、停止或编辑隧道。",
                                "Add a tunnel to start, stop, or edit it here.",
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
                |_, _| None::<(String, String)>,
            );
            let copy_feedback = copy_feedback_state
                .read(cx)
                .as_ref()
                .filter(|(address, _)| address == &copy_address)
                .map(|(_, message)| message.clone());
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
            let route = forward_route_label(
                kind,
                &forward.forward.config.bind.to_string(),
                forward
                    .forward
                    .config
                    .destination
                    .as_ref()
                    .map(ToString::to_string)
                    .as_deref(),
            );
            let host_id = &forward.forward.config.host_id;
            let host_name = forward_host_label(
                host_id,
                Some(super::server::preferred_host_display_name(
                    host_id,
                    &self.host_display_names,
                )),
            );
            let route = format!("{route} · {host_name}");
            let host_id_details = forward_host_id_detail(host_id);
            let route_accessible = crate::locale::text(
                format!("{route}，{host_id_details}"),
                format!("{route}; {host_id_details}"),
            );
            let route_tooltip = host_id_details;
            let (state_color, state_description) = match forward.forward.state {
                ForwardState::Active => (theme.success, crate::locale::text("运行中", "Running")),
                ForwardState::Starting => {
                    (theme.warning, crate::locale::text("正在启动", "Starting"))
                }
                ForwardState::Closing => {
                    (theme.warning, crate::locale::text("正在停止", "Stopping"))
                }
                ForwardState::Stopped => (theme.muted, crate::locale::text("已停止", "Stopped")),
                ForwardState::Failed => (theme.error, crate::locale::text("异常", "Failed")),
            };
            let failure = forward.failure_code.map_or_else(String::new, |code| {
                format!(" · {} {code:?}", localized_text("错误", "error"))
            });
            let proxy_status = forward
                .proxy_application
                .map_or_else(String::new, |outcome| {
                    format!(" · {}", proxy_application_message(outcome))
                });

            let mut row = div()
                .w_full()
                .flex()
                .when(self.compact_layout, |row| row.flex_col().items_stretch())
                .when(!self.compact_layout, |row| row.items_center())
                .gap_3()
                .p_3()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.panel)
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
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .text_size(theme.text_sm)
                                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                                        .text_color(theme.foreground)
                                        .child(forward.forward.config.name.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(theme.text_xs)
                                        .text_color(state_color)
                                        .child(state_description),
                                ),
                        )
                        .child(
                            div()
                                .id(format!("operations-forward-route-{id}"))
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(route)
                                .tooltip(move |window, cx| {
                                    Tooltip::new(route_tooltip.clone()).build(window, cx)
                                })
                                .aria_label(route_accessible),
                        )
                        .when(!failure.is_empty() || !proxy_status.is_empty(), |content| {
                            content.child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.subtle)
                                    .child(format!("{failure}{proxy_status}")),
                            )
                        }),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_1()
                        .child(
                            Button::new(format!("forward-details-{id}"))
                                .compact()
                                .ghost()
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
                        )
                        .when(stopped && forward.persistent, |actions| {
                            actions
                                .child(
                                    Button::new(format!("forward-start-{id}"))
                                        .compact()
                                        .h(px(32.))
                                        .primary()
                                        .label(crate::locale::text("启动", "Start"))
                                        .disabled(self.busy)
                                        .on_click(cx.listener(
                                            move |view, _: &ClickEvent, _, cx| {
                                                view.start_saved_forward(start_id.clone(), cx)
                                            },
                                        )),
                                )
                                .child(
                                    icon_button(
                                        format!("forward-edit-{id}"),
                                        IconName::Pencil,
                                        crate::locale::text("编辑隧道", "Edit tunnel"),
                                    )
                                    .with_size(gpui_kit::component::Size::Medium)
                                    .h(px(32.))
                                    .disabled(self.busy)
                                    .on_click(cx.listener(
                                        move |view, _: &ClickEvent, window, cx| {
                                            view.prepare_forward_edit(
                                                edit_snapshot.clone(),
                                                window,
                                                cx,
                                            )
                                        },
                                    )),
                                )
                                .child(
                                    Button::new(format!("forward-delete-{id}"))
                                        .compact()
                                        .h(px(32.))
                                        .ghost()
                                        .label(crate::locale::text("删除", "Delete"))
                                        .disabled(self.busy)
                                        .on_click(cx.listener(
                                            move |view, _: &ClickEvent, _, cx| {
                                                view.request_forward_delete(
                                                    delete_definition.clone(),
                                                    cx,
                                                );
                                            },
                                        )),
                                )
                        })
                        .when(!stopped, |actions| {
                            actions.child(
                                Button::new(format!("forward-stop-{id}"))
                                    .compact()
                                    .h(px(32.))
                                    .label(crate::locale::text("停止", "Stop"))
                                    .disabled(self.busy)
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.stop_forward(stop_id.clone(), cx)
                                    })),
                            )
                        }),
                );
            if details_open {
                let config = &forward.forward.config;
                let is_remote = matches!(kind, ForwardKind::Remote | ForwardKind::RemoteDynamic);
                let is_socks = matches!(kind, ForwardKind::Dynamic | ForwardKind::RemoteDynamic);
                let source_label = match kind {
                    ForwardKind::Local => crate::locale::text("本机侧入口", "Local endpoint"),
                    ForwardKind::Remote => crate::locale::text("主机侧入口", "Host-side endpoint"),
                    ForwardKind::Dynamic => {
                        crate::locale::text("本机 SOCKS 入口", "Local SOCKS endpoint")
                    }
                    ForwardKind::RemoteDynamic => {
                        crate::locale::text("主机 SOCKS 入口", "Host SOCKS endpoint")
                    }
                };
                let target_label = if is_socks {
                    crate::locale::text("目标服务", "Destination service")
                } else if is_remote {
                    crate::locale::text("本机目标", "Local target")
                } else {
                    crate::locale::text("主机目标", "Host target")
                };
                let source_endpoint = forward
                    .actual_bind
                    .as_ref()
                    .map_or_else(|| config.bind.to_string(), ToString::to_string);
                let target_endpoint = if is_socks {
                    crate::locale::text("由代理请求指定", "Chosen by proxy requests").to_string()
                } else {
                    config.destination.as_ref().map_or_else(
                        || crate::locale::text("—", "—").to_string(),
                        ToString::to_string,
                    )
                };
                let actual_endpoint = forward.actual_bind.as_ref().map_or_else(
                    || crate::locale::text("未监听", "Not listening").to_string(),
                    ToString::to_string,
                );
                let scope = config.remote_access_scope.map_or_else(
                    || {
                        if is_remote {
                            crate::locale::text("默认", "Default").to_string()
                        } else {
                            "—".to_owned()
                        }
                    },
                    remote_access_scope_label,
                );
                let proxy_endpoint = if is_socks {
                    copy_address.clone()
                } else {
                    "—".to_owned()
                };
                let copy_message = copy_feedback.as_deref().map(|message| {
                    div()
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(message.to_owned())
                });
                let mut details = div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.secondary)
                    .child(forward_detail_fact(source_label, source_endpoint, theme))
                    .child(forward_detail_fact(target_label, target_endpoint, theme))
                    .child(forward_detail_fact(
                        crate::locale::text("实际监听", "Active listener"),
                        actual_endpoint,
                        theme,
                    ))
                    .child(forward_detail_fact(
                        crate::locale::text("监听范围", "Access scope"),
                        scope,
                        theme,
                    ));
                if is_socks {
                    details = details.child(forward_detail_fact(
                        crate::locale::text("SOCKS 代理地址", "SOCKS proxy address"),
                        proxy_endpoint,
                        theme,
                    ));
                }
                if let Some(failure) = forward.failure_code {
                    details = details.child(forward_detail_fact(
                        crate::locale::text("错误", "Error"),
                        format!("{failure:?}"),
                        theme,
                    ));
                }
                if kind == ForwardKind::RemoteDynamic {
                    let proxy_application = forward.proxy_application.map_or_else(
                        || crate::locale::text("尚未应用", "Not applied").to_string(),
                        |outcome| proxy_application_message(outcome).to_string(),
                    );
                    details = details.child(forward_detail_fact(
                        crate::locale::text("代理应用", "Proxy application"),
                        proxy_application,
                        theme,
                    ));
                    let panel_is_selected = selected_proxy_forward.as_deref() == Some(id.as_str());
                    let panel_for_row = panel_is_selected
                        .then(|| self.proxy_panel.clone())
                        .flatten();
                    if panel_for_row.is_some() {
                        proxy_panel_in_details = true;
                    }
                    let select_proxy_id = id.clone();
                    details = details.child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                Button::new(format!("forward-proxy-panel-{id}"))
                                    .compact()
                                    .ghost()
                                    .label(if panel_is_selected {
                                        crate::locale::text(
                                            "代理面板已在此处打开",
                                            "Proxy panel is open here",
                                        )
                                    } else {
                                        crate::locale::text("打开代理设置", "Open proxy settings")
                                    })
                                    .disabled(self.busy || self.proxy_panel.is_none())
                                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                                        view.select_proxy_panel_forward(
                                            select_proxy_id.clone(),
                                            cx,
                                        );
                                    })),
                            )
                            .when_some(panel_for_row, |details, panel| {
                                details.child(div().w_full().child(panel))
                            }),
                    );
                }
                let copy_state = copy_feedback_state.clone();
                let copied_address = copy_address.clone();
                details = details.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(
                            icon_button(
                                format!("forward-copy-address-{id}"),
                                IconName::Copy,
                                crate::locale::text("复制隧道地址", "Copy tunnel address"),
                            )
                                .tooltip(crate::locale::text(
                                    "复制隧道地址",
                                    "Copy tunnel address",
                                ))
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
                                        *feedback = Some((copied_address.clone(), message));
                                        cx.notify();
                                    });
                                })),
                        )
                        .when_some(copy_message, |actions, message| actions.child(message)),
                );
                row = row.child(details);
            }
            body = body.child(row);
        }

        if !proxy_panel_in_details && let Some(proxy_panel) = self.proxy_panel.clone() {
            body = body.child(proxy_panel);
        }

        section_page(
            crate::locale::text("SSH 隧道", "SSH tunnels"),
            body,
            theme,
            self.compact_layout,
            self.section_selection.is_fixed(),
        )
    }

    /// 根层模态保留完整隧道字段；布局依据弹窗可用宽度判断，不继承右栏紧凑模式。
    /// 宽度封顶 960px，低于 720px 时纵向排列端点，避免窄窗口压缩输入控件；忙碌时继续等待真实用例回执。
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

        let compact_layout = available_width.min(960.) < 720.;
        let kind = self.forward_kind;
        let dynamic = matches!(kind, ForwardKind::Dynamic | ForwardKind::RemoteDynamic);
        let remote_listener = matches!(kind, ForwardKind::Remote | ForwardKind::RemoteDynamic);
        let host_id = self.forward_form_host_id();
        let host_summary = host_id.as_ref().map_or_else(
            || {
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        "表单没有固定 SSH 主机；请取消后从明确的 SSH 主机重新打开。",
                        "This form has no frozen SSH host. Cancel it and reopen from an explicit SSH host.",
                    ))
                    .into_any_element()
            },
            |host_id| {
                let host_label = forward_host_label(
                    host_id,
                    Some(super::server::preferred_host_display_name(
                        host_id,
                        &self.host_display_names,
                    )),
                );
                let host_id_details = forward_host_id_detail(host_id);
                let accessible_label = crate::locale::text(
                    format!("{host_label}，{host_id_details}"),
                    format!("{host_label}; {host_id_details}"),
                );
                div()
                    .id("operations-forward-frozen-host-summary")
                    .text_size(theme.text_xs)
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
        let mut scenario_picker = div().w_full().flex().flex_wrap().gap_3();
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
                "-D / -R",
            ),
        ] {
            let selected = scenario == current_scenario;
            let next_kind = forward_kind_for_scenario(scenario, kind);
            scenario_picker = scenario_picker.child(
                BaseButton::new(format!("forward-scenario-{scenario:?}"))
                    .flex_1()
                    .min_w(px(210.))
                    .min_h(px(88.))
                    .flex()
                    .flex_col()
                    .items_start()
                    .justify_start()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.secondary)
                    .selected(selected)
                    .styles(|styles| {
                        styles
                            .selected(|style| style.border_color(theme.accent).bg(theme.selected))
                            .disabled(|style| style.opacity(0.55))
                    })
                    .when(!disabled, |button| {
                        button.hover(|style| style.border_color(theme.accent))
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
                                    .text_size(theme.text_body)
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .text_color(theme.foreground)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .bg(theme.sidebar)
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(option),
                            ),
                    )
                    .child(
                        div()
                            .text_size(theme.text_sm)
                            .text_color(theme.muted)
                            .child(description),
                    )
                    .disabled(disabled)
                    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                        view.forward_kind = next_kind;
                        cx.notify();
                    })),
            );
        }

        let header = div()
            .w_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_size(theme.text_heading)
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(if self.forward_edit_expected.is_some() {
                        crate::locale::text("编辑 SSH 隧道", "Edit SSH tunnel")
                    } else {
                        crate::locale::text("添加 SSH 隧道", "Add SSH tunnel")
                    }),
            )
            .child(host_summary);
        let mut content = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .child(scenario_picker)
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(field_label(crate::locale::text("名称", "Name"), theme))
                    .child(
                        Input::new(&inputs.forward_name)
                            .h(px(32.))
                            .disabled(disabled)
                            .w_full(),
                    ),
            );

        if dynamic {
            let local_selected = kind == ForwardKind::Dynamic;
            let direction_buttons = div()
                .w_full()
                .flex()
                .flex_wrap()
                .gap_2()
                .child(
                    Button::new("forward-socks-local")
                        .compact()
                        .h(px(32.))
                        .when(local_selected, |button| button.primary())
                        .when(!local_selected, |button| button.ghost())
                        .label(crate::locale::text("本机代理 · -D", "Local proxy · -D"))
                        .disabled(disabled)
                        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                            view.forward_kind = forward_socks_kind(false);
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("forward-socks-remote")
                        .compact()
                        .h(px(32.))
                        .when(!local_selected, |button| button.primary())
                        .when(local_selected, |button| button.ghost())
                        .label(crate::locale::text(
                            "SSH 主机代理 · -R",
                            "SSH host proxy · -R",
                        ))
                        .disabled(disabled)
                        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                            view.forward_kind = forward_socks_kind(true);
                            cx.notify();
                        })),
                );
            content = content.child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.secondary)
                    .child(
                        div()
                            .text_size(theme.text_sm)
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "SOCKS5 仅支持 CONNECT，不需要固定目标地址。",
                                "SOCKS5 supports CONNECT only; no fixed destination is needed.",
                            )),
                    )
                    .child(direction_buttons),
            );
        }

        let bind_host = self.input_value(&inputs.forward_bind_host, cx);
        let bind_mode = bind_scope_mode(&bind_host);
        let mut bind_scope_options = div().w_full().flex().flex_wrap().items_center().gap_2();
        for (mode, label, value) in [
            (
                "loopback",
                if remote_listener {
                    crate::locale::text("仅 SSH 主机", "SSH host only")
                } else {
                    crate::locale::text("仅本机", "This device only")
                },
                Some("127.0.0.1"),
            ),
            (
                "all",
                crate::locale::text("所有网卡", "All interfaces"),
                Some("0.0.0.0"),
            ),
            ("custom", crate::locale::text("自定义", "Custom"), None),
        ] {
            let selected = bind_mode == mode;
            bind_scope_options = bind_scope_options.child(
                Button::new(format!("forward-bind-scope-{mode}"))
                    .compact()
                    .h(px(32.))
                    .when(selected, |button| button.primary())
                    .when(!selected, |button| button.ghost())
                    .label(label)
                    .disabled(disabled)
                    .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                        if let Some(inputs) = view.inputs.as_ref() {
                            inputs.forward_bind_host.update(cx, |state, cx| {
                                if let Some(value) = value {
                                    state.set_value(value.to_owned(), window, cx);
                                } else if !selected {
                                    state.set_value(String::new(), window, cx);
                                }
                            });
                        }
                        cx.notify();
                    })),
            );
        }
        let bind_scope_controls = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(field_label(
                crate::locale::text(
                    if remote_listener {
                        "SSH 主机监听范围"
                    } else {
                        "本机监听范围"
                    },
                    if remote_listener {
                        "SSH host listen scope"
                    } else {
                        "Local listen scope"
                    },
                ),
                theme,
            ))
            .child(bind_scope_options);
        let bind_fields = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(endpoint_fields(
                crate::locale::text("监听地址", "Listen address"),
                &inputs.forward_bind_host,
                &inputs.forward_bind_port,
                crate::locale::text("监听端口", "Listen port"),
                theme,
                "forward-bind",
                disabled,
            ))
            .child(bind_scope_controls)
            .into_any_element();

        let destination_fields = endpoint_fields(
            crate::locale::text("服务地址", "Service address"),
            &inputs.forward_destination_host,
            &inputs.forward_destination_port,
            crate::locale::text("服务端口", "Service port"),
            theme,
            "forward-destination",
            disabled,
        )
        .into_any_element();
        let host_socks_hint = endpoint_hint(
            crate::locale::text("按需访问主机服务", "Reach host services on demand"),
            crate::locale::text(
                "代理客户端提供目标；域名由 SSH 主机解析。",
                "The client supplies each destination; hostnames resolve on the SSH host.",
            ),
            theme,
        );
        let local_socks_hint = endpoint_hint(
            crate::locale::text("无需固定本机目标", "No fixed local destination"),
            crate::locale::text(
                "SOCKS5 请求经 SSH 通道处理。",
                "SOCKS5 requests are handled through the SSH channel.",
            ),
            theme,
        );
        let (host_content, local_content) = match kind {
            ForwardKind::Local => (destination_fields, bind_fields),
            ForwardKind::Remote => (bind_fields, destination_fields),
            ForwardKind::Dynamic => (host_socks_hint, bind_fields),
            ForwardKind::RemoteDynamic => (bind_fields, local_socks_hint),
        };
        let (host_title, local_title) = match kind {
            ForwardKind::Local => (
                crate::locale::text("主机目标服务", "Host destination service"),
                crate::locale::text("本机监听", "Local listener"),
            ),
            ForwardKind::Remote => (
                crate::locale::text("主机监听", "Host listener"),
                crate::locale::text("本机目标服务", "Local destination service"),
            ),
            ForwardKind::Dynamic => (
                crate::locale::text("SOCKS5 目标", "SOCKS5 destination"),
                crate::locale::text("本机代理监听", "Local proxy listener"),
            ),
            ForwardKind::RemoteDynamic => (
                crate::locale::text("SSH 主机代理监听", "SSH host proxy listener"),
                crate::locale::text("SOCKS5 请求", "SOCKS5 requests"),
            ),
        };
        let (host_description, local_description) = match kind {
            ForwardKind::Local => (
                crate::locale::text("经 SSH 可访问的服务", "Service reachable through SSH"),
                crate::locale::text("本机应用连接的入口", "Entry point for local apps"),
            ),
            ForwardKind::Remote => (
                crate::locale::text("SSH 主机上的连接入口", "Entry point on the SSH host"),
                crate::locale::text(
                    "SSH 通道返回的本机服务",
                    "Local service reached through SSH",
                ),
            ),
            ForwardKind::Dynamic => (
                crate::locale::text(
                    "目标由 SOCKS 客户端按需指定",
                    "The SOCKS client chooses each destination",
                ),
                crate::locale::text(
                    "应用连接本机代理端口",
                    "Apps connect to the local proxy port",
                ),
            ),
            ForwardKind::RemoteDynamic => (
                crate::locale::text(
                    "SSH 主机接受 SOCKS5 连接",
                    "SOCKS5 connections arrive on the SSH host",
                ),
                crate::locale::text("请求经 SSH 通道转发", "Requests are forwarded through SSH"),
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
        let arrow = if compact_layout {
            if remote_listener { "↓" } else { "↑" }
        } else if remote_listener {
            "→"
        } else {
            "←"
        };
        let direction = crate::locale::text(
            match kind {
                ForwardKind::Local => "本机 → 主机 · -L",
                ForwardKind::Remote => "主机 → 本机 · -R",
                ForwardKind::Dynamic => "本机 → 主机 · -D",
                ForwardKind::RemoteDynamic => "主机 → 本机 · -R",
            },
            match kind {
                ForwardKind::Local => "Local → host · -L",
                ForwardKind::Remote => "Host → local · -R",
                ForwardKind::Dynamic => "Local → host · -D",
                ForwardKind::RemoteDynamic => "Host → local · -R",
            },
        );
        let direction_arrow = div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_1()
            .when(compact_layout, |view| view.w_full().h(px(48.)))
            .when(!compact_layout, |view| view.w(px(112.)))
            .child(
                div()
                    .text_size(px(24.))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_color(theme.accent)
                    .child(arrow),
            )
            .child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(direction),
            );
        let mut endpoints = div().w_full().gap_3();
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
        content = content.child(
            div()
                .w_full()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(forward_kind_description(kind)),
        );

        if let Some(pending) = self.forward_save_pending.as_ref() {
            content = content.child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .bg(theme.warning.opacity(0.12))
                    .text_color(theme.foreground)
                    .child(crate::locale::text(
                        format!(
                            "保存结果未知：{} · {}。先核对原 ID，避免重复创建或覆盖。",
                            pending.config.name, pending.id
                        ),
                        format!(
                            "Save outcome is unknown: {} · {}. Review the original ID before creating or replacing anything.",
                            pending.config.name, pending.id
                        ),
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                Button::new("forward-reconcile-save")
                                    .compact()
                                    .h(px(32.))
                                    .primary()
                                    .label(crate::locale::text("核对保存结果", "Review save result"))
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                        view.reconcile_forward_save(cx)
                                    })),
                            )
                            .child(
                                Button::new("forward-archive-save-review")
                                    .compact()
                                    .h(px(32.))
                                    .ghost()
                                    .label(crate::locale::text("结束核对", "End review"))
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                                        view.archive_forward_save_review(cx)
                                    })),
                            ),
                    ),
            );
        }

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

        let mut footer = div()
            .w_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(theme.border);
        if let Some(status) = self.status.as_ref() {
            footer = footer.child(
                div()
                    .id("forward-form-feedback")
                    .w_full()
                    .max_h(px(96.))
                    .overflow_y_scroll()
                    .p_3()
                    .rounded_md()
                    .bg(theme.error.opacity(0.1))
                    .text_size(theme.text_xs)
                    .text_color(theme.error)
                    .child(status.clone()),
            );
        }

        let disabled = self.busy || self.forward_save_pending.is_some() || !self.can_save_forward();
        let cancellation_requested = self
            .mutation_cancellation
            .as_ref()
            .is_some_and(|cancellation| cancellation.is_cancelled());
        let cancel_disabled = self.forward_save_pending.is_some()
            || (self.busy && (self.mutation_cancellation.is_none() || cancellation_requested));
        let cancel_label = if self.busy {
            if cancellation_requested {
                crate::locale::text("正在取消…", "Cancelling…")
            } else {
                crate::locale::text("取消操作", "Cancel operation")
            }
        } else {
            crate::locale::text("取消", "Cancel")
        };
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
                        .compact()
                        .h(px(32.))
                        .ghost()
                        .label(cancel_label)
                        .disabled(cancel_disabled)
                        .on_click(cx.listener(|view, _: &ClickEvent, _, cx| {
                            if view.busy {
                                view.cancel_operation_mutation(cx);
                            } else {
                                view.cancel_root_confirmation(cx);
                            }
                        })),
                )
                .child(
                    Button::new("forward-save")
                        .compact()
                        .h(px(32.))
                        .label(crate::locale::text("仅保存", "Save only"))
                        .disabled(disabled)
                        .on_click(
                            cx.listener(|view, _: &ClickEvent, _, cx| view.save_forward(false, cx)),
                        ),
                )
                .child(
                    Button::new("forward-start")
                        .compact()
                        .h(px(32.))
                        .primary()
                        .label(if self.busy {
                            crate::locale::text("正在处理…", "Working…")
                        } else {
                            crate::locale::text("保存并启动", "Save and start")
                        })
                        .disabled(disabled)
                        .on_click(
                            cx.listener(|view, _: &ClickEvent, _, cx| view.save_forward(true, cx)),
                        ),
                ),
        );

        div()
            .id("forward-form-modal")
            .w_full()
            .h(px(available_height))
            .min_h(px(0.))
            .flex()
            .flex_col()
            .bg(theme.panel)
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
        .gap_2()
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(field_label(host_label, theme))
                .child(
                    Input::new(host_input)
                        .h(px(32.))
                        .disabled(disabled)
                        .w_full(),
                ),
        )
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(field_label(port_label, theme))
                .child(
                    Input::new(port_input)
                        .h(px(32.))
                        .disabled(disabled)
                        .w_full(),
                ),
        )
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
        .gap_2()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .bg(theme.sidebar)
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(side),
                )
                .child(
                    div()
                        .text_size(theme.text_body)
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child(title),
                ),
        )
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(description),
        )
        .child(content)
}

/// 动态 SOCKS 端点没有固定目的地址；用说明卡明确行为，不伪造一个可编辑的目标输入。
///
/// @author kongweiguang
fn endpoint_hint(
    title: impl IntoElement,
    description: impl IntoElement,
    theme: ThemeTokens,
) -> AnyElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .rounded_md()
        .bg(theme.sidebar)
        .child(
            div()
                .text_size(theme.text_sm)
                .font_weight(gpui_kit::FontWeight::MEDIUM)
                .text_color(theme.foreground)
                .child(title),
        )
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(description),
        )
        .into_any_element()
}

/// 统一表单标签文字色，明暗主题沿用产品语义令牌。
///
/// @author kongweiguang
fn field_label(label: impl IntoElement, theme: ThemeTokens) -> impl IntoElement {
    div()
        .text_size(theme.text_xs)
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

/// 将类型和监听端点压缩成一行可扫描的目标摘要。
///
/// @author kongweiguang
fn forward_route_label(kind: ForwardKind, bind: &str, destination: Option<&str>) -> String {
    let (zh, en) = match kind {
        ForwardKind::Local => (
            format!("本机 {bind} → SSH {}", destination.unwrap_or("?")),
            format!("Local {bind} → SSH {}", destination.unwrap_or("?")),
        ),
        ForwardKind::Remote => (
            format!("SSH {bind} → 本机 {}", destination.unwrap_or("?")),
            format!("SSH {bind} → local {}", destination.unwrap_or("?")),
        ),
        ForwardKind::Dynamic => (
            format!("本机 SOCKS5 · {bind}"),
            format!("Local SOCKS5 · {bind}"),
        ),
        ForwardKind::RemoteDynamic => (
            format!("SSH SOCKS5 · {bind}"),
            format!("SSH SOCKS5 · {bind}"),
        ),
    };
    crate::locale::text(zh, en).to_string()
}

/// 表单说明遵循 SSH -L/-R 方向，帮助用户知道监听端和目的端由哪一侧访问。
///
/// @author kongweiguang
fn forward_kind_description(kind: ForwardKind) -> gpui_kit::SharedString {
    crate::locale::text(
        match kind {
            ForwardKind::Local => "本机监听，经 SSH 转发到 SSH 主机可访问的服务。",
            ForwardKind::Remote => "SSH 主机监听，经 SSH 转发回本机可访问的服务。",
            ForwardKind::Dynamic => "在本机创建 SOCKS5 代理，目标由代理客户端按需指定。",
            ForwardKind::RemoteDynamic => {
                "在 SSH 主机创建 SOCKS5 代理，服务端策略可能限制外部访问。"
            }
        },
        match kind {
            ForwardKind::Local => {
                "Listen locally and forward through SSH to a service reachable from the SSH host."
            }
            ForwardKind::Remote => {
                "Listen on the SSH host and forward through SSH to a service reachable locally."
            }
            ForwardKind::Dynamic => {
                "Create a local SOCKS5 proxy; each client chooses its destination."
            }
            ForwardKind::RemoteDynamic => {
                "Create a SOCKS5 proxy on the SSH host; server policy may restrict external access."
            }
        },
    )
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
fn bind_scope_mode(host: &str) -> &'static str {
    let host = host.trim();
    if host.eq_ignore_ascii_case("localhost") {
        return "loopback";
    }
    match host.parse::<IpAddr>() {
        Ok(address) if address.is_loopback() => "loopback",
        Ok(address) if address.is_unspecified() => "all",
        _ => "custom",
    }
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
        .w_full()
        .flex()
        .flex_wrap()
        .items_start()
        .gap_2()
        .child(
            div()
                .w(px(112.))
                .flex_shrink_0()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(label),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .text_size(theme.text_sm)
                .text_color(theme.foreground)
                .child(value),
        )
}

/// 复制已公布的 SOCKS URL；普通转发优先使用真实监听端点，否则复制配置端点供预先分享。
///
/// @author kongweiguang
fn forward_copy_address(
    forward: &kerminal_application::operations::ManagedPortForwardSnapshot,
) -> String {
    forward
        .proxy_url
        .as_ref()
        .cloned()
        .or_else(|| forward.actual_bind.as_ref().map(ToString::to_string))
        .unwrap_or_else(|| forward.forward.config.bind.to_string())
}

/// 显示声明的远端监听范围；它是请求范围，运行态地址单独显示。
///
/// @author kongweiguang
fn remote_access_scope_label(scope: RemoteAccessScope) -> String {
    let (zh, en) = match scope {
        RemoteAccessScope::Loopback => ("仅回环", "Loopback only"),
        RemoteAccessScope::PrivateNetwork => ("私有网络", "Private network"),
        RemoteAccessScope::AllInterfaces => ("所有网卡", "All interfaces"),
        RemoteAccessScope::Custom => ("自定义地址", "Custom address"),
    };
    localized_text(zh, en)
}

/// 明确区分隧道监听与代理配置应用，避免展示 URL 时暗示终端环境已自动修改。
///
/// @author kongweiguang
fn proxy_application_message(outcome: ProxyApplicationOutcome) -> String {
    if outcome.applied {
        return localized_text(
            "代理已应用到选定范围。",
            "Proxy applied to the selected scope.",
        );
    }
    let (zh, en) = match outcome.reason {
        ProxyApplicationReason::TunnelOnly => (
            "仅创建代理隧道；终端环境未修改。",
            "Tunnel only; terminal environment was not changed.",
        ),
        ProxyApplicationReason::CallerMustApplyProxyUrl => (
            "隧道已创建；调用方需自行配置代理地址。",
            "Tunnel started; the caller must configure the proxy URL.",
        ),
        ProxyApplicationReason::NoProxyEndpoint => (
            "未返回代理地址；终端环境未修改。",
            "No proxy URL was returned; the terminal environment was not changed.",
        ),
        ProxyApplicationReason::NoTerminalTarget => (
            "没有明确的终端目标；代理未应用。",
            "No explicit terminal target; the proxy was not applied.",
        ),
        ProxyApplicationReason::NoFutureTerminalIntegration => (
            "新终端代理集成不可用；代理未应用。",
            "Future terminal integration is unavailable; the proxy was not applied.",
        ),
        ProxyApplicationReason::NoExplicitProfileTarget => (
            "没有明确的 Profile 目标；代理未应用。",
            "No explicit profile target; the proxy was not applied.",
        ),
    };
    localized_text(zh, en)
}

#[cfg(test)]
#[path = "../../../../tests/unit/operations_render_forwarding.rs"]
mod tests;
