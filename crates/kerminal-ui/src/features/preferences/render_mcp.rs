//! MCP 监听与连接信息的原生设置页面。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement,
    assets::IconName,
    base::TestSupportExt as _,
    component::{
        Disableable as _, Icon, Selectable as _,
        button::{Button, ButtonGroup, ButtonVariants as _},
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::{mcp_runtime::McpRuntimeState, preferences::settings::McpBindMode};

use super::panel::{McpConfigTarget, PreferencesPanel};
use crate::theme::ThemeTokens;

impl PreferencesPanel {
    /// 监听草稿与真实连接信息分别呈现，避免把下次启动的端口显示为已生效。
    /// 分组行使用同一对齐与控件尺寸；草稿说明不冒充运行状态，窄窗允许整组换行。
    pub(super) fn render_mcp_settings(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let snapshot = self.connectivity.current_mcp_snapshot();
        let busy = self.connectivity.mcp_operation.is_some() || self.mcp_settings.busy;
        let state = if busy {
            if self.mcp_settings.busy || self.connectivity.mcp_starting() {
                McpRuntimeState::Starting
            } else {
                McpRuntimeState::Stopping
            }
        } else {
            snapshot.state
        };
        let running = state == McpRuntimeState::Running;
        let stopping = state == McpRuntimeState::Stopping;
        let toggle_label = if busy || state == McpRuntimeState::Starting {
            if state == McpRuntimeState::Starting {
                crate::locale::text("启动中…", "Starting…")
            } else {
                crate::locale::text("停止中…", "Stopping…")
            }
        } else if running {
            crate::locale::text("停止", "Stop")
        } else if stopping {
            crate::locale::text("重试停止", "Retry stop")
        } else if state == McpRuntimeState::Synchronizing {
            crate::locale::text("重试同步", "Retry sync")
        } else {
            crate::locale::text("启动", "Start")
        };
        let toggle_available = match state {
            McpRuntimeState::Unavailable | McpRuntimeState::Starting => false,
            McpRuntimeState::Running | McpRuntimeState::Stopping => {
                snapshot.endpoint.is_some() || snapshot.endpoint_sync_pending
            }
            _ => true,
        };
        let current_lan = snapshot.bind_mode == McpBindMode::Lan;
        let running_port = snapshot.endpoint.as_deref().and_then(endpoint_port);
        let selected_address = self
            .mcp_settings
            .selected_address
            .filter(|address| self.mcp_settings.addresses.contains(address));
        let lan_target =
            running && current_lan && self.mcp_settings.config_target == McpConfigTarget::Lan;
        let addresses_pending =
            self.mcp_settings.addresses_loading || self.mcp_settings.addresses_generation == 0;
        let lan_target_ready = lan_target && selected_address.is_some();
        let lan_target_loading = lan_target && selected_address.is_none() && addresses_pending;
        let lan_target_empty = lan_target && selected_address.is_none() && !addresses_pending;
        let lan_url = selected_address
            .zip(running_port)
            .map(|(address, port)| format!("http://{address}:{port}/mcp"));
        let local_url = snapshot
            .endpoint
            .as_deref()
            .map(|endpoint| listening_address(endpoint, false));
        let target_url = if lan_target_ready {
            lan_url.clone()
        } else {
            local_url.clone()
        };
        let listen_endpoint = snapshot.endpoint.as_deref().map_or_else(
            || crate::locale::text("启动后显示", "Available after startup").to_string(),
            |endpoint| listening_address(endpoint, current_lan),
        );
        let local_json = if busy {
            None
        } else {
            self.connectivity.pretty_client_json(&snapshot)
        };
        let json_display = if !running {
            crate::locale::text("启动后显示", "Available after startup").to_string()
        } else if lan_target_loading {
            crate::locale::text("正在读取可用网卡地址…", "Loading network addresses…").to_string()
        } else if lan_target_empty {
            crate::locale::text("未发现可用的局域网地址", "No local network address found")
                .to_string()
        } else if lan_target_ready {
            lan_url.as_deref().map(masked_lan_json).unwrap_or_else(|| {
                crate::locale::text("启动后显示", "Available after startup").to_string()
            })
        } else {
            local_json.clone().unwrap_or_else(|| {
                crate::locale::text("启动后显示", "Available after startup").to_string()
            })
        };
        let connect_display = if lan_target_loading {
            crate::locale::text("正在读取…", "Loading…").to_string()
        } else if lan_target_empty {
            crate::locale::text("未发现可用的局域网地址", "No local network address found")
                .to_string()
        } else {
            target_url.clone().unwrap_or_else(|| {
                crate::locale::text("启动后显示", "Available after startup").to_string()
            })
        };
        let copy_is_lan = lan_target;
        let target_available = if copy_is_lan {
            lan_target_ready
        } else {
            local_json.is_some()
        };
        let copy_enabled = !busy && !self.mcp_settings.busy && target_available;
        let locked = busy || self.mcp_settings.busy || self.settings_controls_locked();
        let invalid_port = self.settings.is_some() && self.mcp_settings.valid_port(cx).is_none();
        let restart_pending = running
            && (self.mcp_settings.bind_mode != snapshot.bind_mode
                || self.mcp_settings.valid_port(cx) != running_port);
        let draft_lan = self.mcp_settings.bind_mode == McpBindMode::Lan;
        let port = super::render_settings::settings_input(&self.mcp_settings.port, theme)
            .disabled(locked || self.mcp_settings.outcome_unknown)
            .when(invalid_port, |input| input.border_color(theme.error))
            .aria_label(crate::locale::text("监听端口", "Listening port"));
        let port = gpui_kit::Styled::w(port, gpui_kit::rems(7.));

        let mode_button = |mode, id, label| {
            Button::new(id)
                .compact()
                .outline()
                .selected(self.mcp_settings.bind_mode == mode)
                .toggled(self.mcp_settings.bind_mode == mode)
                .label(label)
                .disabled(locked || self.mcp_settings.outcome_unknown)
                .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                    panel.select_mcp_bind_mode(mode, cx)
                }))
        };
        let heading = div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Server)
                            .size(px(16.))
                            .text_color(theme.accent),
                    )
                    .child(
                        div()
                            .text_size(theme.text_sm)
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child("MCP"),
                    ),
            )
            .child(
                div().flex().items_center().gap_2().child(
                    Button::new("mcp-settings-toggle")
                        .compact()
                        .outline()
                        .ghost()
                        .icon(
                            Icon::new(if running || stopping {
                                IconName::PowerOff
                            } else {
                                IconName::Power
                            })
                            .size(px(16.)),
                        )
                        .label(toggle_label)
                        .loading(busy)
                        .disabled(
                            !toggle_available
                                || locked
                                || (!running
                                    && !stopping
                                    && (invalid_port
                                        || self.mcp_settings.conflict
                                        || self.mcp_settings.outcome_unknown)),
                        )
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.toggle_mcp_settings(cx)
                        })),
                ),
            );

        let settings = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(crate::locale::text("监听设置", "Listening settings")),
            )
            .child(
                div()
                    .w_full()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .p_3()
                            .child(setting_label(
                                crate::locale::text("监听端口", "Listening port"),
                                crate::locale::text(
                                    "1–65535，下次启动时生效",
                                    "1–65535. Used on next start.",
                                ),
                                theme,
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(div().id("mcp-settings-port").child(port))
                                    .when(invalid_port, |field| {
                                        field.child(
                                            div()
                                                .id("mcp-port-validation")
                                                .text_size(theme.text_xs)
                                                .text_color(theme.error)
                                                .child(crate::locale::text(
                                                    "请输入 1–65535",
                                                    "Enter 1–65535",
                                                )),
                                        )
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .p_3()
                            .border_t_1()
                            .border_color(theme.border)
                            .child(setting_label(
                                crate::locale::text("监听范围", "Listening scope"),
                                if draft_lan {
                                    crate::locale::text(
                                        "需令牌连接，仅在可信网络使用。",
                                        "Token required. Use on trusted networks only.",
                                    )
                                } else {
                                    crate::locale::text(
                                        "仅允许此电脑上的客户端连接。",
                                        "Only clients on this computer can connect.",
                                    )
                                },
                                theme,
                            ))
                            .child(
                                ButtonGroup::new("mcp-bind-mode")
                                    .disabled(locked || self.mcp_settings.outcome_unknown)
                                    .child(mode_button(
                                        McpBindMode::Loopback,
                                        "mcp-bind-loopback",
                                        crate::locale::text("仅本机", "Local only"),
                                    ))
                                    .child(mode_button(
                                        McpBindMode::Lan,
                                        "mcp-bind-lan",
                                        crate::locale::text("局域网", "Local network"),
                                    )),
                            ),
                    )
                    .when(restart_pending, |settings| {
                        settings.child(
                            div()
                                .id("mcp-settings-restart-hint")
                                .w_full()
                                .px_3()
                                .pb_3()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(crate::locale::text(
                                    "监听范围或端口与当前运行服务不一致，停止后重新启动生效。",
                                    "Listening scope or port differs from the running server. Stop and start again to apply.",
                                ))
                                .test_support(),
                        )
                    }),
            )
            .when(
                self.mcp_settings.conflict || self.mcp_settings.outcome_unknown,
                |settings| {
                    settings.child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .when(!self.mcp_settings.outcome_unknown, |row| {
                                row.child(
                                    Button::new("mcp-settings-reset")
                                        .compact()
                                        .ghost()
                                        .label(crate::locale::text("重新读取", "Reload"))
                                        .disabled(locked)
                                        .on_click(cx.listener(
                                            |panel, _: &ClickEvent, window, cx| {
                                                panel.reset_mcp_settings(window, cx)
                                            },
                                        )),
                                )
                            })
                            .when(self.mcp_settings.outcome_unknown, |row| {
                                row.child(
                                    Button::new("mcp-settings-check")
                                        .compact()
                                        .outline()
                                        .label(crate::locale::text("核对结果", "Check result"))
                                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                            panel.check_mcp_settings(cx)
                                        })),
                                )
                            })
                            .when(
                                self.mcp_settings.conflict && !self.mcp_settings.outcome_unknown,
                                |row| {
                                    row.child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.error)
                                    .child(crate::locale::text(
                                        "设置已在其他位置更改，请先重新读取。",
                                        "Settings changed elsewhere. Reload before starting.",
                                    )),
                            )
                                },
                            ),
                    )
                },
            );

        let disclosure = div()
            .w_full()
            .border_t_1()
            .border_color(theme.border)
            .child(
                Button::new("mcp-connection-info-toggle")
                    .custom(super::render_settings::settings_ghost_variant(theme, cx))
                    .w_full()
                    .h(px(44.))
                    .toggled(self.connectivity.mcp_connection_expanded)
                    .accessibility_label(crate::locale::text("连接信息", "Connection information"))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Icon::new(if self.connectivity.mcp_connection_expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size(px(16.))
                                .text_color(theme.muted),
                            )
                            .child(crate::locale::text("连接信息", "Connection information")),
                    )
                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                        panel.connectivity.mcp_connection_expanded =
                            !panel.connectivity.mcp_connection_expanded;
                        if panel.connectivity.mcp_connection_expanded {
                            panel.refresh_mcp_addresses(cx);
                        }
                        cx.notify();
                    })),
            )
            .when(self.connectivity.mcp_connection_expanded, |section| {
                section.child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(info_row(
                            "mcp-listen-address",
                            crate::locale::text("监听地址", "Listen address"),
                            &listen_endpoint,
                            theme,
                        ))
                        .when(current_lan && running, |body| {
                            body.child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(crate::locale::text("连接配置", "Connection")),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Button::new("mcp-config-local")
                                            .compact()
                                            .outline()
                                            .selected(!lan_target)
                                            .toggled(!lan_target)
                                            .when(!lan_target, |button| {
                                                button.icon(
                                                    Icon::new(IconName::Check).size(px(12.)),
                                                )
                                            })
                                            .label(crate::locale::text("本机", "Local"))
                                            .on_click(cx.listener(
                                                |panel, _: &ClickEvent, _, cx| {
                                                    panel.select_mcp_config_target(
                                                        McpConfigTarget::Local,
                                                        None,
                                                        cx,
                                                    )
                                                },
                                            )),
                                    )
                                    .children(self.mcp_settings.addresses.iter().copied().map(
                                        |address| {
                                            let selected = lan_target_ready
                                                && selected_address == Some(address);
                                            Button::new(format!("mcp-lan-address-{address}"))
                                                .compact()
                                                .outline()
                                                .selected(selected)
                                                .toggled(selected)
                                                .when(selected, |button| {
                                                    button.icon(
                                                        Icon::new(IconName::Check).size(px(12.)),
                                                    )
                                                })
                                                .label(address.to_string())
                                                .on_click(cx.listener(
                                                    move |panel, _: &ClickEvent, _, cx| {
                                                        panel.select_mcp_config_target(
                                                            McpConfigTarget::Lan,
                                                            Some(address),
                                                            cx,
                                                        )
                                                    },
                                                ))
                                        },
                                    ))
                                    .child(
                                        Button::new("mcp-lan-refresh")
                                            .compact()
                                            .ghost()
                                            .icon(IconName::RefreshCw)
                                            .label(crate::locale::text("重新读取", "Reload"))
                                            .on_click(cx.listener(
                                                |panel, _: &ClickEvent, _, cx| {
                                                    panel.refresh_mcp_addresses(cx)
                                                },
                                            )),
                                    ),
                            )
                            .when(self.mcp_settings.addresses.is_empty(), |body| {
                                body.child(
                                    div()
                                        .id("mcp-lan-addresses-empty")
                                        .text_size(theme.text_xs)
                                        .text_color(theme.muted)
                                        .child(
                                            if lan_target_loading
                                                || self.mcp_settings.addresses_loading
                                            {
                                                crate::locale::text(
                                                    "正在读取可用网卡地址…",
                                                    "Loading network addresses…",
                                                )
                                            } else {
                                                crate::locale::text(
                                                    "未发现可用的局域网地址",
                                                    "No local network address found",
                                                )
                                            },
                                        )
                                        .test_support(),
                                )
                            })
                        })
                        .when(current_lan && running, |body| {
                            body.child(info_row(
                                "mcp-connect-address",
                                crate::locale::text("连接地址", "Connect address"),
                                &connect_display,
                                theme,
                            ))
                        })
                        .child(
                            Button::new("mcp-settings-copy-json")
                                .compact()
                                .outline()
                                .icon(
                                    Icon::new(if self.connectivity.mcp_copied {
                                        IconName::Check
                                    } else {
                                        IconName::Copy
                                    })
                                    .size(px(14.)),
                                )
                                .label(if self.connectivity.mcp_copied {
                                    crate::locale::text("已复制", "Copied")
                                } else if copy_is_lan {
                                    crate::locale::text("复制局域网配置", "Copy LAN config")
                                } else {
                                    crate::locale::text("复制 JSON", "Copy JSON")
                                })
                                .disabled(!copy_enabled)
                                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.copy_mcp_config(cx)
                                })),
                        )
                        .when_some(self.connectivity.mcp_copy_error.clone(), |body, error| {
                            body.child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.error)
                                    .child(error),
                            )
                        })
                        .when(lan_target_ready, |body| {
                            body.child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(crate::locale::text(
                                        "访问令牌仅在复制时填入，不会显示在页面中。",
                                        "The access token is only added when copying and is never shown here.",
                                    )),
                            )
                        })
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child("JSON"),
                        )
                        .child(
                            div()
                                .id("mcp-json-preview")
                                .w_full()
                                .max_h(px(176.))
                                .overflow_y_scroll()
                                .overflow_x_scroll()
                                .p_3()
                                .rounded_md()
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.background)
                                .font_family("monospace")
                                .text_size(px(11.))
                                .child(json_display),
                        )
                )
            });

        div()
            .id("preferences-mcp-settings")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.content_scroll)
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .w_full()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.panel)
                    .child(heading)
                    .child(settings)
                    .when_some(self.mcp_settings.message.clone(), |card, status| {
                        card.child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(if self.mcp_settings.is_error {
                                    theme.error
                                } else {
                                    theme.muted
                                })
                                .child(status),
                        )
                    })
                    .child(disclosure)
                    .when_some(
                        self.connectivity
                            .mcp_status
                            .clone()
                            .filter(|_| self.connectivity.mcp_status_is_error),
                        |card, status| {
                            card.child(
                                div()
                                    .text_size(theme.text_xs)
                                    .text_color(if self.connectivity.mcp_status_is_error {
                                        theme.error
                                    } else {
                                        theme.muted
                                    })
                                    .child(status),
                            )
                        },
                    ),
            )
    }
}

/// 两条设置行共享文字层级，描述随可用宽度换行，不给每个字段建立新的状态实体。
fn setting_label(
    label: gpui_kit::SharedString,
    description: gpui_kit::SharedString,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_size(theme.text_sm).child(label))
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(description),
        )
}

/// 长地址保留独立行，避免压缩端口和操作按钮；文本同时作为可观察的无障碍标签。
fn info_row(
    id: &'static str,
    label: gpui_kit::SharedString,
    value: &str,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(label),
        )
        .child(
            div()
                .id(id)
                .aria_label(value.to_owned())
                .w_full()
                .min_w(px(0.))
                .font_family("monospace")
                .text_size(px(11.))
                .child(value.to_owned())
                .test_support(),
        )
}

/// 从 runtime endpoint 读取实际端口，供草稿与运行 listener 的差异比较。
fn endpoint_port(endpoint: &str) -> Option<u16> {
    endpoint
        .strip_prefix("http://")?
        .split('/')
        .next()?
        .rsplit_once(':')?
        .1
        .parse()
        .ok()
}

/// 局域网预览只公开地址和掩码；真实 Bearer 令牌仍只在复制时由 runtime 读取。
fn masked_lan_json(url: &str) -> String {
    let value = serde_json::json!({
        "mcpServers": {
            "kerminal": {
                "type": "http",
                "url": url,
                "headers": {
                    "Authorization": crate::locale::text(
                        "Bearer <复制时自动填入>",
                        "Bearer <filled on copy>",
                    ),
                },
            },
        },
    });
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
}

/// 已运行 listener 的展示地址跟随实际绑定范围，端口仍取真实 endpoint；
/// 本机目标展示回环客户端配置，局域网目标展示所选网卡的掩码配置。
fn listening_address(endpoint: &str, lan: bool) -> String {
    let Some(rest) = endpoint.strip_prefix("http://") else {
        return endpoint.to_owned();
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, "mcp"));
    let port = authority
        .rsplit_once(':')
        .map_or(authority, |(_, port)| port);
    let host = if lan { "0.0.0.0" } else { "127.0.0.1" };
    format!("http://{host}:{port}/{path}")
}
