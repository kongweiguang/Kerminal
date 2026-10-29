//! 设置页内的外部启动与 MCP 状态；页面直接呈现复用的运行时服务。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement, StatefulInteractiveElement, Window,
    assets::IconName,
    component::{
        Disableable, Icon,
        button::{Button, ButtonVariants},
        switch::Switch,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::{mcp_runtime::McpRuntimeState, preferences::ExternalLaunchToolSetting};

use crate::theme::ThemeTokens;

use super::panel::PreferencesPanel;

const LAUNCH_COMMANDS: [(&str, &str); 9] = [
    (
        "PuTTY 参数",
        "kerminal.exe -ssh ${USER}@${HOST} -P ${PORT} -pw \"<PASSWORD_FROM_PLATFORM>\"",
    ),
    (
        "MobaXterm 参数",
        "kerminal.exe -newtab \"ssh -p ${PORT} ${USER}@${HOST}\"",
    ),
    (
        "Xshell 参数",
        "kerminal.exe -url \"ssh://${USER}:<PASSWORD_FROM_PLATFORM>@${HOST}:${PORT}\"",
    ),
    (
        "SecureCRT 参数",
        "kerminal.exe /SSH2 /L ${USER} /P ${PORT} /PASSWORD \"<PASSWORD_FROM_PLATFORM>\" ${HOST}",
    ),
    ("OpenSSH 参数", "kerminal.exe -p ${PORT} -l ${USER} ${HOST}"),
    (
        "WinSCP / FileZilla SFTP URL",
        "kerminal.exe \"sftp://${USER}:<PASSWORD_FROM_PLATFORM>@${HOST}:${PORT}/path/\"",
    ),
    (
        "Kerminal flags",
        "kerminal.exe --external-ssh --host ${HOST} --port ${PORT} --user ${USER}",
    ),
    (
        "Kerminal JSON",
        "kerminal.exe --external-ssh-json {\"host\":\"${HOST}\",\"port\":${PORT},\"username\":\"${USER}\"}",
    ),
    (
        "Kerminal URL",
        "kerminal://ssh?host=${HOST}&port=${PORT}&user=${USER}&openSftp=true",
    ),
];

const LAUNCH_SOURCES: [(ExternalLaunchToolSetting, &str, &str); 7] = [
    (
        ExternalLaunchToolSetting::Putty,
        "PuTTY",
        "PuTTY / Plink 参数 persona。",
    ),
    (
        ExternalLaunchToolSetting::Mobaxterm,
        "MobaXterm",
        "MobaXterm newtab / exec 参数。",
    ),
    (
        ExternalLaunchToolSetting::Xshell,
        "Xshell",
        "Xshell ssh:// URL 或 b64 参数。",
    ),
    (
        ExternalLaunchToolSetting::Securecrt,
        "SecureCRT",
        "SecureCRT /SSH2 参数。",
    ),
    (
        ExternalLaunchToolSetting::Openssh,
        "OpenSSH",
        "OpenSSH ssh 命令参数。",
    ),
    (
        ExternalLaunchToolSetting::SftpClient,
        "SFTP 客户端参数",
        "WinSCP / FileZilla / 通用 SFTP URL 参数。",
    ),
    (
        ExternalLaunchToolSetting::KerminalNative,
        "Kerminal native",
        "Kerminal flags / JSON / URL。",
    ),
];

impl PreferencesPanel {
    /// 来源卡整卡承载 switch 语义；网格断点读取窗口逻辑视口，避免窄窗中控件相互挤压。
    /// 滚动根继承设置弹框表面，命令示例仍保留独立底色。
    /// @author kongweiguang
    pub(super) fn render_external_launch(
        &mut self,
        theme: ThemeTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(settings) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            let message = self.settings_error.clone().unwrap_or_else(|| {
                if self.loading_settings {
                    crate::locale::text("正在读取设置…", "Loading preferences…").to_string()
                } else {
                    crate::locale::text("设置快照不可用。", "The settings snapshot is unavailable.")
                        .to_string()
                }
            });
            return div()
                .id("preferences-external-launch-loading")
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(if self.settings_error.is_some() {
                    theme.error
                } else {
                    theme.muted
                })
                .child(message);
        };
        let launch = settings.external_launch;
        let viewport_width = window.viewport_size().width;
        let launch_columns = if viewport_width < px(1024.) { 1 } else { 2 };
        let source_columns = if viewport_width < px(768.) {
            1
        } else if viewport_width < px(1280.) {
            2
        } else {
            3
        };
        let locked = self.settings_controls_locked();
        let protocol_action = super::panel::protocol::external_protocol_action(
            self.external_protocol.is_some(),
            self.external_protocol_status,
            self.external_protocol_loading,
            self.external_protocol_requires_refresh,
        );
        let protocol_status = if self.external_protocol_loading {
            crate::locale::text("正在读取协议状态…", "Reading protocol status…")
        } else if let Some(status) = self.external_protocol_status {
            if !status.supported {
                crate::locale::text(
                    "当前平台不提供生产级协议注册",
                    "Protocol registration is unavailable on this platform",
                )
            } else if status.registered {
                crate::locale::text("已为当前用户注册", "Registered for the current user")
            } else {
                crate::locale::text(
                    "未注册，外部链接不会唤起 Kerminal",
                    "Not registered; external links will not open Kerminal",
                )
            }
        } else {
            crate::locale::text(
                "无法读取协议注册状态",
                "Could not read protocol registration status",
            )
        };

        let allowed_source_count = LAUNCH_SOURCES
            .iter()
            .filter(|(tool, _, _)| !launch.disabled_tools.contains(tool))
            .count();
        let source_cards = LAUNCH_SOURCES
            .iter()
            .map(|(tool, label, description)| {
                let enabled = !launch.disabled_tools.contains(tool);
                let selected = *tool;
                Button::new(format!("external-source-{selected:?}"))
                    .role(gpui_kit::Role::Switch)
                    .toggled(enabled)
                    .accessibility_label(crate::locale::text(
                        format!("允许 {label}"),
                        format!("Allow {label}"),
                    ))
                    .ghost()
                    .outline()
                    .w_full()
                    .min_w(px(0.))
                    .min_h(px(80.))
                    .h_auto()
                    .p_3()
                    .flex()
                    .flex_col()
                    .items_start()
                    .justify_start()
                    .gap_2()
                    .rounded_md()
                    .border_color(if enabled {
                        theme.accent.opacity(0.35)
                    } else {
                        theme.border
                    })
                    .bg(if enabled { theme.selected } else { theme.panel })
                    .disabled(locked)
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .items_start()
                            .gap_2()
                            .whitespace_normal()
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
                                            .min_w(px(0.))
                                            .flex_1()
                                            .whitespace_normal()
                                            .text_size(theme.text_sm)
                                            .child(*label),
                                    )
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .px_2p5()
                                            .py_1()
                                            .rounded_full()
                                            .border_1()
                                            .border_color(if enabled {
                                                theme.accent.opacity(0.35)
                                            } else {
                                                theme.border
                                            })
                                            .bg(if enabled {
                                                theme.accent.opacity(0.15)
                                            } else {
                                                theme.hover
                                            })
                                            .text_size(px(11.))
                                            .text_color(if enabled {
                                                theme.accent
                                            } else {
                                                theme.muted
                                            })
                                            .child(if enabled {
                                                crate::locale::text("允许", "Allowed")
                                            } else {
                                                crate::locale::text("禁用", "Disabled")
                                            }),
                                    ),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .whitespace_normal()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(*description),
                            ),
                    )
                    .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                        panel.edit_settings(
                            |settings| {
                                let disabled = &mut settings.external_launch.disabled_tools;
                                if let Some(index) =
                                    disabled.iter().position(|item| *item == selected)
                                {
                                    disabled.remove(index);
                                } else {
                                    disabled.push(selected);
                                }
                            },
                            cx,
                        );
                    }))
            })
            .collect::<Vec<_>>();

        let page = div()
            .id("preferences-external-launch")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.content_scroll)
            .flex()
            .flex_col()
            .gap_4()
            .child(
                settings_card(theme)
                    .child(div().flex().items_center().gap_2().text_size(theme.text_sm)
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .child(Icon::new(IconName::Route).text_color(theme.accent))
                        .child(crate::locale::text("外部 SSH 启动", "External SSH launch")))
                    .child(div().flex().items_start().gap_2().rounded_md().border_1()
                        .border_color(theme.warning.opacity(0.25))
                        .bg(theme.warning.opacity(0.10)).px_3().py_2()
                        .child(Icon::new(IconName::ShieldAlert).size(px(16.)).text_color(theme.warning))
                        .child(div().text_size(theme.text_xs).text_color(theme.warning)
                            .child(crate::locale::text(
                                "外部传入的密码和私钥口令仅用于当前会话，不写入连接配置。",
                                "Passwords and key passphrases from external launch requests are used only for the current session and are not saved in connection settings.",
                            ))))
                    .child(
                        div()
                            .grid()
                            .grid_cols(launch_columns)
                            .gap_3()
                            .child(
                                div()
                                    .id("external-launch-enabled-row")
                                    .w_full()
                                    .min_h(px(44.))
                                    .p_3()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_3()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(if launch.enabled {
                                        theme.accent.opacity(0.30)
                                    } else {
                                        theme.border
                                    })
                                    .bg(if launch.enabled {
                                        theme.selected
                                    } else {
                                        theme.panel
                                    })
                                    .child(
                                        div()
                                            .min_w(px(0.))
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                Icon::new(IconName::Power)
                                                    .size(px(16.))
                                                    .text_color(theme.muted),
                                            )
                                            .child(
                                                div()
                                                    .min_w(px(0.))
                                                    .text_size(theme.text_sm)
                                                    .child(crate::locale::text(
                                                        "启用外部 SSH 启动",
                                                        "Enable external SSH launch",
                                                    )),
                                            ),
                                    )
                                    .child(
                                        Switch::new("external-launch-enabled")
                                            .track_size(px(40.), px(24.))
                                            .checked(launch.enabled)
                                            .disabled(locked)
                                            .accessibility_label(crate::locale::text(
                                                "启用外部 SSH 启动",
                                                "Enable external SSH launch",
                                            ))
                                            .on_change(cx.listener(
                                                |panel, value: &bool, _, cx| {
                                                    cx.stop_propagation();
                                                    panel.edit_settings(
                                                        |settings| {
                                                            settings.external_launch.enabled =
                                                                *value;
                                                        },
                                                        cx,
                                                    );
                                                },
                                            )),
                                    )
                                    .when(!locked, |row| {
                                        row.cursor_pointer()
                                            .hover(|style| style.bg(theme.hover))
                                            .on_click(cx.listener(
                                                |panel, _: &ClickEvent, _, cx| {
                                                    panel.edit_settings(
                                                        |settings| {
                                                            settings.external_launch.enabled =
                                                                !settings.external_launch.enabled;
                                                        },
                                                        cx,
                                                    );
                                                },
                                            ))
                                    }),
                            )
                            .child(
                                div()
                                    .id("external-launch-vendor-args-row")
                                    .w_full()
                                    .min_h(px(44.))
                                    .p_3()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_3()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(if launch.accept_vendor_args {
                                        theme.accent.opacity(0.30)
                                    } else {
                                        theme.border
                                    })
                                    .bg(if launch.accept_vendor_args {
                                        theme.selected
                                    } else {
                                        theme.panel
                                    })
                                    .child(
                                        div()
                                            .min_w(px(0.))
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                Icon::new(IconName::Terminal)
                                                    .size(px(16.))
                                                    .text_color(theme.muted),
                                            )
                                            .child(
                                                div()
                                                    .min_w(px(0.))
                                                    .text_size(theme.text_sm)
                                                    .child(crate::locale::text(
                                                        "接受常见终端参数",
                                                        "Accept common terminal arguments",
                                                    )),
                                            ),
                                    )
                                    .child(
                                        Switch::new("external-launch-vendor-args")
                                            .track_size(px(40.), px(24.))
                                            .checked(launch.accept_vendor_args)
                                            .disabled(locked)
                                            .accessibility_label(crate::locale::text(
                                                "接受常见终端参数",
                                                "Accept common terminal arguments",
                                            ))
                                            .on_change(cx.listener(
                                                |panel, value: &bool, _, cx| {
                                                    cx.stop_propagation();
                                                    panel.edit_settings(
                                                        |settings| {
                                                            settings.external_launch
                                                                .accept_vendor_args = *value;
                                                        },
                                                        cx,
                                                    );
                                                },
                                            )),
                                    )
                                    .when(!locked, |row| {
                                        row.cursor_pointer()
                                            .hover(|style| style.bg(theme.hover))
                                            .on_click(cx.listener(
                                                |panel, _: &ClickEvent, _, cx| {
                                                    panel.edit_settings(
                                                        |settings| {
                                                            settings.external_launch
                                                                .accept_vendor_args = !settings
                                                                .external_launch
                                                                .accept_vendor_args;
                                                        },
                                                        cx,
                                                    );
                                                },
                                            ))
                                    }),
                            )
                            .child(
                                div()
                                    .id("external-launch-auto-sftp-row")
                                    .w_full()
                                    .min_h(px(44.))
                                    .p_3()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_3()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(if launch.auto_open_sftp {
                                        theme.accent.opacity(0.30)
                                    } else {
                                        theme.border
                                    })
                                    .bg(if launch.auto_open_sftp {
                                        theme.selected
                                    } else {
                                        theme.panel
                                    })
                                    .child(
                                        div()
                                            .min_w(px(0.))
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                Icon::new(IconName::FolderOpen)
                                                    .size(px(16.))
                                                    .text_color(theme.muted),
                                            )
                                            .child(
                                                div()
                                                    .min_w(px(0.))
                                                    .text_size(theme.text_sm)
                                                    .child(crate::locale::text(
                                                        "连接后自动打开 SFTP",
                                                        "Open SFTP after connecting",
                                                    )),
                                            ),
                                    )
                                    .child(
                                        Switch::new("external-launch-auto-sftp")
                                            .track_size(px(40.), px(24.))
                                            .checked(launch.auto_open_sftp)
                                            .disabled(locked)
                                            .accessibility_label(crate::locale::text(
                                                "连接后自动打开 SFTP",
                                                "Open SFTP after connecting",
                                            ))
                                            .on_change(cx.listener(
                                                |panel, value: &bool, _, cx| {
                                                    cx.stop_propagation();
                                                    panel.edit_settings(
                                                        |settings| {
                                                            settings.external_launch
                                                                .auto_open_sftp = *value;
                                                        },
                                                        cx,
                                                    );
                                                },
                                            )),
                                    )
                                    .when(!locked, |row| {
                                        row.cursor_pointer()
                                            .hover(|style| style.bg(theme.hover))
                                            .on_click(cx.listener(
                                                |panel, _: &ClickEvent, _, cx| {
                                                    panel.edit_settings(
                                                        |settings| {
                                                            settings.external_launch
                                                                .auto_open_sftp = !settings
                                                                .external_launch
                                                                .auto_open_sftp;
                                                        },
                                                        cx,
                                                    );
                                                },
                                            ))
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .mt_2()
                            .py_3()
                            .border_y_1()
                            .border_color(theme.border)
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(div().flex_1().min_w(px(0.)).flex().flex_col().gap_1()
                                .child(div().flex().items_center().gap_2().text_size(theme.text_sm)
                                    .child(Icon::new(IconName::Link2).size(px(14.)).text_color(theme.muted))
                                    .child("kerminal:// 系统协议"))
                                .child(div().text_size(theme.text_xs).text_color(theme.muted).child(protocol_status)))
                            .when_some(protocol_action, |row, action| {
                                row.child(Button::new("external-protocol-registration")
                                    .compact()
                                    .label(match action {
                                        super::panel::protocol::ExternalProtocolAction::Register => crate::locale::text("注册协议", "Register protocol"),
                                        super::panel::protocol::ExternalProtocolAction::Unregister => crate::locale::text("注销协议", "Unregister protocol"),
                                    })
                                    .disabled(self.external_protocol_loading)
                                    .when(action == super::panel::protocol::ExternalProtocolAction::Register, Button::primary)
                                    .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                                        panel.change_external_protocol(action, cx);
                                    })))
                            })
                            .when(self.external_protocol_error.is_some(), |row| {
                                row.child(div().text_size(theme.text_xs).text_color(theme.error)
                                    .child(self.external_protocol_error.clone().unwrap_or_default()))
                            }),
                    )
                    .child(
                        div()
                            .w_full()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.panel)
                            .child(
                                Button::new("external-launch-compatibility-toggle")
                                    .custom(super::render_settings::settings_ghost_variant(theme, cx))
                                    .w_full()
                                    .h(px(44.))
                                    .px_0()
                                    .py_0()
                                    .toggled(self.connectivity.external_compatibility_expanded)
                                    .accessibility_label(crate::locale::text(
                                        "参数兼容性",
                                        "Parameter compatibility",
                                    ))
                                    .child(
                                        div()
                                            .w_full()
                                            .min_h(px(44.))
                                            .px_3()
                                            .py_2()
                                            .flex()
                                            .items_center()
                                            .gap_3()
                                            .child(
                                                Icon::new(if self
                                                    .connectivity
                                                    .external_compatibility_expanded
                                                {
                                                    IconName::ChevronDown
                                                } else {
                                                    IconName::ChevronRight
                                                })
                                                .size(px(16.))
                                                .text_color(theme.muted),
                                            )
                                            .child(
                                                div()
                                                    .min_w(px(0.))
                                                    .flex_1()
                                                    .text_size(theme.text_sm)
                                                    .child(crate::locale::text(
                                                        "参数兼容性",
                                                        "Parameter compatibility",
                                                    )),
                                            )
                                            .child(
                                                div()
                                                    .min_w(px(0.))
                                                    .max_w(px(240.))
                                                    .truncate()
                                                    .text_size(theme.text_xs)
                                                    .text_color(theme.muted)
                                                    .child(crate::locale::text(
                                                        format!("{allowed_source_count} 个来源"),
                                                        format!("{allowed_source_count} sources"),
                                                    )),
                                            ),
                                    )
                                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                        panel.connectivity.external_compatibility_expanded = !panel
                                            .connectivity
                                            .external_compatibility_expanded;
                                        cx.notify();
                                    })),
                            )
                            .when(self.connectivity.external_compatibility_expanded, |section| {
                                section.child(
                                    div()
                                        .border_t_1()
                                        .border_color(theme.border)
                                        .p_3()
                                        .flex()
                                        .flex_col()
                                        .gap_3()
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                .text_size(theme.text_sm)
                                                .child(
                                                    Icon::new(IconName::Ban)
                                                        .size(px(14.))
                                                        .text_color(theme.muted),
                                                )
                                                .child(crate::locale::text(
                                                    "允许来源",
                                                    "Allowed sources",
                                                )),
                                        )
                                        .child(
                                            div()
                                                .grid()
                                                .grid_cols(source_columns)
                                                .gap_2()
                                                .children(source_cards),
                                        )
                                        .child(
                                            div()
                                                .pt_2()
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                .text_size(theme.text_sm)
                                                .child(
                                                    Icon::new(IconName::Terminal)
                                                        .size(px(14.))
                                                        .text_color(theme.muted),
                                                )
                                                .child(crate::locale::text(
                                                    "命令模板",
                                                    "Command templates",
                                                )),
                                        )
                                        .child(launch_command_rows(theme)),
                                )
                            }),
                    ),
            );
        page.child(self.render_save_notice(theme))
    }

    /// 连接摘要复用真实 listener；滚动根继承设置弹框表面，复制失败贴近入口并避免误报成功。
    pub(super) fn render_mcp_settings(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let snapshot = self.connectivity.current_mcp_snapshot();
        let busy = self.connectivity.mcp_operation.is_some();
        let running = snapshot.state == McpRuntimeState::Running;
        let status_label = mcp_state_label(snapshot.state);
        let json = self.connectivity.pretty_client_json(&snapshot);
        let json_display = json.clone().unwrap_or_else(|| {
            crate::locale::text("启动后显示", "Available after startup").to_string()
        });
        let connection_summary = if running {
            crate::locale::text("本机 HTTP", "Local HTTP")
        } else {
            crate::locale::text("启动后可用", "Available after startup")
        };
        let can_toggle = match snapshot.state {
            McpRuntimeState::Unavailable => false,
            McpRuntimeState::Running | McpRuntimeState::Stopping => {
                snapshot.endpoint.is_some() || snapshot.endpoint_sync_pending
            }
            McpRuntimeState::Stopped
            | McpRuntimeState::Starting
            | McpRuntimeState::Synchronizing
            | McpRuntimeState::Failed => true,
        };
        let status_color = mcp_state_color(theme, snapshot.state);
        let toggle = Button::new("mcp-settings-toggle")
            .compact()
            .outline()
            .ghost()
            .icon(
                Icon::new(if running {
                    IconName::PowerOff
                } else {
                    IconName::Power
                })
                .size(px(16.)),
            )
            .label(if busy {
                if self.connectivity.mcp_starting() {
                    crate::locale::text("启动中…", "Starting…")
                } else {
                    crate::locale::text("停止中…", "Stopping…")
                }
            } else if running {
                crate::locale::text("停止", "Stop")
            } else {
                crate::locale::text("启动", "Start")
            })
            .disabled(busy || !can_toggle)
            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                if panel.connectivity.current_mcp_snapshot().state == McpRuntimeState::Running {
                    panel.connectivity.stop_mcp(cx);
                } else {
                    panel.connectivity.start_mcp(cx);
                }
            }));
        let heading = div()
            .flex()
            .flex_wrap()
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
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .rounded_full()
                            .border_1()
                            .border_color(status_color.opacity(0.25))
                            .bg(status_color.opacity(0.10))
                            .text_size(theme.text_xs)
                            .text_color(status_color)
                            .child(status_label),
                    )
                    .child(toggle),
            );
        let connection_toggle = div()
            .w_full()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .child(
                Button::new("mcp-connection-info-toggle")
                    .custom(super::render_settings::settings_ghost_variant(theme, cx))
                    .w_full()
                    .h(px(44.))
                    .px_0()
                    .py_0()
                    .toggled(self.connectivity.mcp_connection_expanded)
                    .accessibility_label(crate::locale::text("连接信息", "Connection information"))
                    .child(
                        div()
                            .w_full()
                            .min_h(px(44.))
                            .px_3()
                            .py_2()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                Icon::new(if self.connectivity.mcp_connection_expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size(px(16.))
                                .text_color(theme.muted),
                            )
                            .child(
                                div().min_w(px(0.)).flex_1().text_size(theme.text_sm).child(
                                    crate::locale::text("连接信息", "Connection information"),
                                ),
                            )
                            .child(
                                div()
                                    .min_w(px(0.))
                                    .max_w(px(240.))
                                    .truncate()
                                    .text_size(theme.text_xs)
                                    .text_color(theme.muted)
                                    .child(connection_summary),
                            ),
                    )
                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                        panel.connectivity.mcp_connection_expanded =
                            !panel.connectivity.mcp_connection_expanded;
                        cx.notify();
                    })),
            )
            .when(self.connectivity.mcp_connection_expanded, |section| {
                section.child(
                    div()
                        .border_t_1()
                        .border_color(theme.border)
                        .p_3()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(render_info_row(
                            "endpoint",
                            snapshot.endpoint.as_deref().unwrap_or("启动后显示"),
                            theme,
                        ))
                        .child(render_json_row(&json_display, theme))
                        .child(
                            Button::new("mcp-settings-copy-json")
                                .compact()
                                .label(if self.connectivity.mcp_copied {
                                    crate::locale::text("已复制", "Copied")
                                } else {
                                    crate::locale::text("复制 JSON", "Copy JSON")
                                })
                                .icon(
                                    Icon::new(if self.connectivity.mcp_copied {
                                        IconName::Check
                                    } else {
                                        IconName::Copy
                                    })
                                    .size(px(14.)),
                                )
                                .disabled(json.is_none())
                                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.connectivity.copy_mcp_json(cx)
                                })),
                        ),
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
                settings_card(theme)
                    .child(heading)
                    .child(connection_toggle)
                    .when_some(self.connectivity.mcp_copy_error.clone(), |card, error| {
                        card.child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.error)
                                .child(error),
                        )
                    })
                    .when_some(self.connectivity.mcp_status.clone(), |card, status| {
                        card.child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(
                                    if self.connectivity.mcp_status_is_error
                                        || snapshot.failure_reason.is_some()
                                        || snapshot.unavailable_reason.is_some()
                                    {
                                        theme.error
                                    } else {
                                        theme.muted
                                    },
                                )
                                .child(status),
                        )
                    }),
            )
            .child(self.render_save_notice(theme))
    }

    /// 搜索定位应展开命中的原生 disclosure，而密钥编辑区始终可见，不维护重复展开状态。
    pub(in crate::features::preferences) fn reveal_connectivity_target(
        &mut self,
        target: &str,
        cx: &mut Context<Self>,
    ) {
        let changed = match target {
            "external-sources" => {
                let changed = !self.connectivity.external_compatibility_expanded;
                self.connectivity.external_compatibility_expanded = true;
                changed
            }
            "mcp-connection" => {
                let changed = !self.connectivity.mcp_connection_expanded;
                self.connectivity.mcp_connection_expanded = true;
                changed
            }
            "sync-vault-key" => false,
            _ => false,
        };
        if changed {
            cx.notify();
        }
    }
}

/// 命令模板保持纯展示，匹配 main 不额外提供逐项复制动作。
fn launch_command_rows(theme: ThemeTokens) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .children(LAUNCH_COMMANDS.iter().map(|(label, command)| {
            div()
                .w_full()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .py_2()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .w(px(112.))
                        .text_size(theme.text_xs)
                        .text_color(theme.muted)
                        .child(*label),
                )
                .child(
                    div()
                        .id(format!("external-command-template-{label}"))
                        .flex_1()
                        .min_w(px(0.))
                        .p_2()
                        .rounded_md()
                        .bg(theme.background)
                        .overflow_x_scroll()
                        .font_family("monospace")
                        .text_size(theme.text_xs)
                        .child(*command),
                )
        }))
}

/// 卡片只提供共享表面；各页可按 main 将标题与状态组合在同一行。
fn settings_card(theme: ThemeTokens) -> gpui_kit::Div {
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
}

/// 呈现 endpoint 标签和值，长地址换行而不挤出同步面板。
fn render_info_row(label: &'static str, value: &str, theme: ThemeTokens) -> impl IntoElement {
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
                .w_full()
                .min_w(px(0.))
                .font_family("monospace")
                .text_size(px(11.))
                .child(value.to_owned()),
        )
}

/// JSON 保留缩进和滚动边界，避免端口或 token 在显示区横向撑开页面。
fn render_json_row(value: &str, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
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
                .child(value.to_owned()),
        )
}

/// 将 runtime 状态投影为稳定中文标签，不能把正在同步 endpoint 显示为已运行。
fn mcp_state_label(state: McpRuntimeState) -> gpui_kit::SharedString {
    match state {
        McpRuntimeState::Unavailable => crate::locale::text("不可用", "Unavailable"),
        McpRuntimeState::Stopped => crate::locale::text("已停止", "Stopped"),
        McpRuntimeState::Starting => crate::locale::text("正在启动", "Starting"),
        McpRuntimeState::Synchronizing => {
            crate::locale::text("正在同步连接信息", "Synchronizing endpoint")
        }
        McpRuntimeState::Running => crate::locale::text("运行中", "Running"),
        McpRuntimeState::Stopping => crate::locale::text("正在停止", "Stopping"),
        McpRuntimeState::Failed => crate::locale::text("启动失败", "Failed"),
    }
}

/// 状态色只作辅助信息，标签仍完整表达 loading、失败与不可用类别。
fn mcp_state_color(theme: ThemeTokens, state: McpRuntimeState) -> gpui_kit::Hsla {
    match state {
        McpRuntimeState::Running => theme.success,
        McpRuntimeState::Failed | McpRuntimeState::Unavailable => theme.error,
        McpRuntimeState::Starting | McpRuntimeState::Synchronizing | McpRuntimeState::Stopping => {
            theme.warning
        }
        McpRuntimeState::Stopped => theme.muted,
    }
}
