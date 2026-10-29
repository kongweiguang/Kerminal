//! 通知、外部启动、传输、工具栏与 Agent Launcher 偏好控件。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, IntoElement, StatefulInteractiveElement, Window,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
        input::Input,
        switch::Switch,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::preferences::{
    CustomAgentDefinition, ExternalLaunchToolSetting, ToolRailPanelPlacement, ToolRailSettings,
    ToolRailToolId,
};

use crate::theme::ThemeTokens;

use super::super::render::settings_error_banner;
use super::forms::set_input;
use super::{PreferencesPanel, PreferencesSection};

/// 仅在当前可见分区内收集可排序工具，隐藏项与保留兼容的 Context 不参与相邻交换。
///
/// @author kongweiguang
fn visible_tool_rail_indices(settings: &ToolRailSettings, bottom: bool) -> Vec<usize> {
    settings
        .order
        .iter()
        .enumerate()
        .filter_map(|(index, tool)| {
            (*tool != ToolRailToolId::Context
                && !settings.hidden.contains(tool)
                && settings.bottom.contains(tool) == bottom)
                .then_some(index)
        })
        .collect()
}

/// 交换指定可见工具与同一主区/底部分区内的相邻可见项，不让隐藏或旧 Context 项截断排序。
///
/// @author kongweiguang
fn move_visible_tool_rail_item(
    settings: &mut ToolRailSettings,
    tool: ToolRailToolId,
    direction: i32,
) -> bool {
    if !matches!(direction, -1 | 1) || tool == ToolRailToolId::Context {
        return false;
    }
    let Some(index) = settings
        .order
        .iter()
        .position(|candidate| *candidate == tool)
    else {
        return false;
    };
    if settings.hidden.contains(&tool) {
        return false;
    }

    let bottom = settings.bottom.contains(&tool);
    let zone = visible_tool_rail_indices(settings, bottom);
    let Some(position) = zone.iter().position(|candidate| *candidate == index) else {
        return false;
    };
    let target = position as i32 + direction;
    let Some(target_index) = usize::try_from(target)
        .ok()
        .and_then(|position| zone.get(position).copied())
    else {
        return false;
    };
    settings.order.swap(index, target_index);
    true
}

/// 统一拒绝当前工作台右键快捷保存的状态，避免把偏好页内其它草稿一并写入。
///
/// @author kongweiguang
fn tool_rail_edit_readiness_error(panel: &PreferencesPanel) -> Option<String> {
    if panel.loading_settings {
        return Some(
            crate::locale::text(
                "设置仍在读取，请稍后重试。",
                "Settings are still loading. Try again shortly.",
            )
            .to_string(),
        );
    }
    if panel.recovery.mode == kerminal_domain::preferences::ConfigurationMode::ReadOnlyRecovery {
        return Some(
            crate::locale::text(
                "配置处于只读恢复模式，工具栏变更未保存。",
                "Configuration is in read-only recovery mode; the tool rail was not changed.",
            )
            .to_string(),
        );
    }
    let Some(draft) = panel.settings.as_ref() else {
        return Some(
            crate::locale::text(
                "设置尚未读取，请打开设置后重试。",
                "Settings are not loaded. Open Settings and try again.",
            )
            .to_string(),
        );
    };
    if draft.status() != super::super::DraftStatus::Ready {
        return Some(match draft.status() {
            super::super::DraftStatus::Saving => crate::locale::text(
                "设置正在保存，请稍后重试。",
                "Settings are being saved. Try again shortly.",
            )
            .to_string(),
            super::super::DraftStatus::ExternalConflict => crate::locale::text(
                "设置已被外部修改，请先重载核对。",
                "Settings changed externally. Reload them before changing the tool rail.",
            )
            .to_string(),
            super::super::DraftStatus::OutcomeUnknown => crate::locale::text(
                "上次保存结果未知，请先读回核对设置。",
                "The previous save has an unknown result. Read settings back before continuing.",
            )
            .to_string(),
            super::super::DraftStatus::Failed => crate::locale::text(
                "上次设置保存失败，请先打开设置处理后重试。",
                "The previous settings save failed. Open Settings and resolve it before retrying.",
            )
            .to_string(),
            super::super::DraftStatus::Ready => crate::locale::text(
                "设置状态暂不可用，请打开设置后重试。",
                "The settings state is unavailable. Open Settings and try again.",
            )
            .to_string(),
        });
    }
    if draft.is_dirty() {
        return Some(
            crate::locale::text(
                "设置页有未保存改动，请先保存或放弃后再调整工具栏。",
                "Settings have unsaved edits. Save or discard them before changing the tool rail.",
            )
            .to_string(),
        );
    }
    if !panel.settings_input_errors.is_empty() {
        return Some(
            crate::locale::text(
                "请先修正设置中的输入错误，再调整工具栏。",
                "Fix the settings input errors before changing the tool rail.",
            )
            .to_string(),
        );
    }
    None
}

impl PreferencesPanel {
    /// 集中展示已经有 typed settings 与应用端持久化用例的扩展选项，避免伪造未接线操作。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn render_integrations(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(settings) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return div()
                .id("preferences-integrations-loading")
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted)
                .child(self.settings_error.clone().unwrap_or_else(|| {
                    if self.loading_settings {
                        crate::locale::text("正在读取设置…", "Loading preferences…").to_string()
                    } else {
                        crate::locale::text(
                            "设置快照不可用。",
                            "The settings snapshot is unavailable.",
                        )
                        .to_string()
                    }
                }));
        };
        let locked = self.settings_controls_locked();
        let notification = settings.desktop_notifications;
        let external = settings.external_launch;
        let selected_agent = settings.agent_launcher.selected_agent_key.clone();
        let custom_agents = settings.agent_launcher.custom_agents.clone();
        let tool_rail = settings.tool_rail.clone();
        let protocol_error_is_operation = self.external_protocol_error_is_operation;
        let protocol_unknown_readback_supported = !self.external_protocol_requires_refresh
            && self
                .external_protocol_status
                .is_some_and(|status| status.supported);
        let protocol_action = super::protocol::external_protocol_action(
            self.external_protocol.is_some(),
            self.external_protocol_status,
            self.external_protocol_loading,
            self.external_protocol_requires_refresh,
        );
        let protocol_message = if self.external_protocol_loading {
            match self.external_protocol_operation {
                Some(super::protocol::ExternalProtocolAction::Register) => {
                    crate::locale::text("正在请求注册 kerminal://…", "Registering kerminal://…")
                }
                Some(super::protocol::ExternalProtocolAction::Unregister) => {
                    crate::locale::text("正在请求注销 kerminal://…", "Unregistering kerminal://…")
                }
                None => crate::locale::text("正在读取协议状态…", "Reading protocol status…"),
            }
        } else if self.external_protocol_last_result.is_some() {
            match self.external_protocol_last_result {
                Some(super::protocol::ExternalProtocolAction::Register) => crate::locale::text(
                    "注册已完成，系统状态回读已确认。",
                    "Registration completed and confirmed by a system readback.",
                ),
                Some(super::protocol::ExternalProtocolAction::Unregister) => crate::locale::text(
                    "注销已完成，系统状态回读已确认。",
                    "Unregistration completed and confirmed by a system readback.",
                ),
                None => crate::locale::text("协议状态已回读。", "Protocol status was read back."),
            }
        } else {
            match self.external_protocol_status {
                Some(status) if !status.supported => crate::locale::text(
                    "当前平台尚不支持检查 kerminal:// 注册状态。",
                    "This platform cannot inspect kerminal:// registration status.",
                ),
                Some(status) if status.registered => crate::locale::text(
                    "kerminal:// 已注册到当前用户。",
                    "kerminal:// is registered for the current user.",
                ),
                Some(_) => crate::locale::text(
                    "当前用户尚未注册 kerminal://。",
                    "kerminal:// is not registered for the current user.",
                ),
                None if self.external_protocol_error.is_some() => crate::locale::text(
                    "无法确认当前 kerminal:// 状态。",
                    "The current kerminal:// status could not be confirmed.",
                ),
                None => crate::locale::text(
                    "尚未检查协议状态。",
                    "Protocol status has not been checked.",
                ),
            }
        };

        let source_rows = external_launch_sources()
            .into_iter()
            .map(|(tool, label)| {
                let disabled = external.disabled_tools.contains(&tool);
                let id = format!("external-launch-source-{tool:?}");
                let label = if disabled {
                    crate::locale::text(format!("{label}（已禁用）"), format!("{label} (disabled)"))
                } else {
                    crate::locale::text(format!("{label}（已启用）"), format!("{label} (enabled)"))
                };
                let selected_tool = tool;
                Button::new(id)
                    .compact()
                    .label(label)
                    .disabled(locked)
                    .when(disabled, Button::primary)
                    .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                        panel.edit_settings(
                            |settings| {
                                let disabled_tools = &mut settings.external_launch.disabled_tools;
                                if let Some(index) = disabled_tools
                                    .iter()
                                    .position(|candidate| *candidate == selected_tool)
                                {
                                    disabled_tools.remove(index);
                                } else {
                                    disabled_tools.push(selected_tool);
                                }
                            },
                            cx,
                        );
                    }))
            })
            .collect::<Vec<_>>();

        let agent_rows = custom_agents
            .iter()
            .map(|agent| {
                let key = format!("custom:{}", agent.id);
                let selected = selected_agent == key;
                let agent_id = agent.id.clone();
                let choose_key = key.clone();
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_2()
                    .bg(theme.panel)
                    .rounded_md()
                    .child(
                        Button::new(format!("agent-select-{}", agent.id))
                            .compact()
                            .label(if selected {
                                crate::locale::text(
                                    format!("{}（当前）", agent.name),
                                    format!("{} (current)", agent.name),
                                )
                            } else {
                                agent.name.clone().into()
                            })
                            .disabled(locked)
                            .when(selected, Button::primary)
                            .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                                panel.edit_settings(
                                    |settings| {
                                        settings.agent_launcher.selected_agent_key =
                                            choose_key.clone();
                                    },
                                    cx,
                                );
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(11.))
                            .text_color(theme.muted)
                            .child(agent.command.clone()),
                    )
                    .child(
                        Button::new(format!("agent-remove-{}", agent.id))
                            .compact()
                            .label(crate::locale::text("移除", "Remove"))
                            .disabled(locked)
                            .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                                panel.remove_custom_agent(&agent_id, cx);
                            })),
                    )
            })
            .collect::<Vec<_>>();

        let tool_rows = tool_rail
            .order
            .iter()
            .copied()
            .filter(|tool| *tool != ToolRailToolId::Context)
            .map(|tool| {
                let label = tool_rail_label(tool);
                let hidden = tool_rail.hidden.contains(&tool);
                let bottom = tool_rail.bottom.contains(&tool);
                let placement = tool_rail.effective_panel_placement(tool);
                let zone = visible_tool_rail_indices(&tool_rail, bottom);
                let position = zone
                    .iter()
                    .position(|index| tool_rail.order[*index] == tool);
                let previous_disabled = hidden || position.is_none_or(|index| index == 0) || locked;
                let next_disabled =
                    hidden || position.is_none_or(|index| index + 1 == zone.len()) || locked;
                let move_up = tool;
                let move_down = tool;
                let mut row = div()
                    .w_full()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .bg(theme.panel)
                    .rounded_md()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().flex_1().child(label))
                            .child(
                                Button::new(format!("tool-up-{tool:?}"))
                                    .compact()
                                    .label(crate::locale::text("上移", "Move up"))
                                    .disabled(previous_disabled)
                                    .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                                        panel.move_tool_rail_item(move_up, -1, cx);
                                    })),
                            )
                            .child(
                                Button::new(format!("tool-down-{tool:?}"))
                                    .compact()
                                    .label(crate::locale::text("下移", "Move down"))
                                    .disabled(next_disabled)
                                    .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                                        panel.move_tool_rail_item(move_down, 1, cx);
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_4()
                            .child(
                                Switch::new(format!("tool-visible-{tool:?}"))
                                    .checked(!hidden)
                                    .disabled(locked)
                                    .label(crate::locale::text("在工具栏显示", "Show in tool rail"))
                                    .on_change(cx.listener(move |panel, visible: &bool, _, cx| {
                                        panel.edit_settings(
                                            |settings| {
                                                let hidden = &mut settings.tool_rail.hidden;
                                                if *visible {
                                                    hidden.retain(|candidate| *candidate != tool);
                                                } else if !hidden.contains(&tool) {
                                                    hidden.push(tool);
                                                }
                                            },
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Switch::new(format!("tool-bottom-{tool:?}"))
                                    .checked(bottom)
                                    .disabled(locked)
                                    .label(crate::locale::text(
                                        "入口固定到底部",
                                        "Pin entry to bottom",
                                    ))
                                    .on_change(cx.listener(move |panel, enabled: &bool, _, cx| {
                                        panel.edit_settings(
                                            |settings| {
                                                let bottom = &mut settings.tool_rail.bottom;
                                                if *enabled {
                                                    if !bottom.contains(&tool) {
                                                        bottom.push(tool);
                                                    }
                                                } else {
                                                    bottom.retain(|candidate| *candidate != tool);
                                                }
                                            },
                                            cx,
                                        );
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.muted)
                            .child(crate::locale::text("面板位置", "Panel placement")),
                    );
                for (placement_id, placement_label, candidate) in placement_options(tool) {
                    let selected = placement == candidate;
                    row = row.child(
                        Button::new(format!("tool-placement-{tool:?}-{placement_id}"))
                            .compact()
                            .label(placement_label)
                            .disabled(locked || !can_change_tool_rail_placement(tool))
                            .when(selected, Button::primary)
                            .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                                if can_change_tool_rail_placement(tool) {
                                    panel.edit_settings(
                                        |settings| {
                                            settings
                                                .tool_rail
                                                .panel_placements
                                                .insert(tool, candidate);
                                        },
                                        cx,
                                    );
                                }
                            })),
                    );
                }
                row
            })
            .collect::<Vec<_>>();

        div()
            .id("preferences-integrations")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.content_scroll)
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .bg(theme.background)
            .child(integrations_heading(theme))
            .child(
                section_card(crate::locale::text("桌面通知", "Desktop notifications"), theme)
                    .child(
                        Switch::new("notification-enabled")
                            .checked(notification.enabled)
                            .disabled(locked)
                            .label(crate::locale::text("启用桌面通知", "Enable desktop notifications"))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.edit_settings(
                                    |settings| settings.desktop_notifications.enabled = *enabled,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Switch::new("notification-background-only")
                            .checked(notification.background_only)
                            .disabled(locked || !notification.enabled)
                            .label(crate::locale::text("仅后台会话通知", "Notify only for background sessions"))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.edit_settings(
                                    |settings| {
                                        settings.desktop_notifications.background_only = *enabled
                                    },
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Switch::new("notification-important-only")
                            .checked(notification.important_only)
                            .disabled(locked || !notification.enabled)
                            .label(crate::locale::text("仅重要事件通知", "Notify only for important events"))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.edit_settings(
                                    |settings| {
                                        settings.desktop_notifications.important_only = *enabled
                                    },
                                    cx,
                                );
                            })),
                    )
                    .child(number_field(
                        crate::locale::text("最短显示时长（毫秒）", "Minimum display duration (ms)"),
                        &self.settings_inputs.notification_min_duration,
                        locked || !notification.enabled,
                        theme,
                    ))
                    .child(number_field(
                        crate::locale::text("相同通知节流（毫秒）", "Duplicate notification interval (ms)"),
                        &self.settings_inputs.notification_throttle,
                        locked || !notification.enabled,
                        theme,
                    )),
            )
            .child(
                section_card(crate::locale::text("外部启动", "External launch"), theme)
                    .child(
                        Switch::new("external-launch-enabled")
                            .checked(external.enabled)
                            .disabled(locked)
                            .label(crate::locale::text("接收外部 SSH 启动请求", "Accept external SSH launch requests"))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.edit_settings(
                                    |settings| settings.external_launch.enabled = *enabled,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Switch::new("external-launch-vendor-args")
                            .checked(external.accept_vendor_args)
                            .disabled(locked || !external.enabled)
                            .label(crate::locale::text("兼容第三方命令行参数", "Accept third-party command-line arguments"))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.edit_settings(
                                    |settings| {
                                        settings.external_launch.accept_vendor_args = *enabled
                                    },
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Switch::new("external-launch-auto-sftp")
                            .checked(external.auto_open_sftp)
                            .disabled(locked || !external.enabled)
                            .label(crate::locale::text("外部 SFTP 请求自动打开文件面板", "Open the file panel for external SFTP requests"))
                            .on_change(cx.listener(|panel, enabled: &bool, _, cx| {
                                panel.edit_settings(
                                    |settings| settings.external_launch.auto_open_sftp = *enabled,
                                    cx,
                                );
                            })),
                    )
                    .child(div().text_size(px(12.)).child(crate::locale::text("允许的来源", "Allowed sources")))
                    .children(source_rows),
            )
            .child(
                section_card(crate::locale::text("SFTP 传输", "SFTP transfers"), theme)
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "并发和 pipeline 范围会在保存时按传输预算归一化。",
                                "Concurrency and pipeline values are normalized to the transfer budget when saved.",
                            )),
                    )
                    .child(number_field(
                        crate::locale::text("全局并发任务", "Global concurrent transfers"),
                        &self.settings_inputs.sftp_global_transfers,
                        locked,
                        theme,
                    ))
                    .child(number_field(
                        crate::locale::text("单主机并发任务", "Per-host concurrent transfers"),
                        &self.settings_inputs.sftp_host_transfers,
                        locked,
                        theme,
                    ))
                    .child(number_field(
                        crate::locale::text("Pipeline 深度", "Pipeline depth"),
                        &self.settings_inputs.sftp_pipeline_depth,
                        locked,
                        theme,
                    ))
                    .child(number_field(
                        crate::locale::text("数据包字节数", "Packet size (bytes)"),
                        &self.settings_inputs.sftp_packet_bytes,
                        locked,
                        theme,
                    ))
                    .child(number_field(
                        crate::locale::text("空闲超时（秒）", "Idle timeout (seconds)"),
                        &self.settings_inputs.sftp_idle_timeout,
                        locked,
                        theme,
                    )),
            )
            .child(
                section_card(crate::locale::text("Agent 启动器", "Agent Launcher"), theme)
                    .child(div().text_size(px(12.)).child(crate::locale::text("默认启动器", "Default launcher")))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .children(builtin_agent_buttons(&selected_agent, locked, cx)),
                    )
                    .children(agent_rows)
                    .child(div().text_size(px(12.)).child(crate::locale::text("添加自定义启动器", "Add a custom launcher")))
                    .child(div().text_size(px(11.)).text_color(theme.muted).child(crate::locale::text("名称", "Name")))
                    .child(
                        Input::new(&self.agent_launcher_inputs.name)
                            .disabled(locked)
                            .w(px(360.)),
                    )
                    .child(div().text_size(px(11.)).text_color(theme.muted).child(crate::locale::text("启动命令", "Launch command")))
                    .child(
                        Input::new(&self.agent_launcher_inputs.command)
                            .disabled(locked)
                            .w(px(520.)),
                    )
                    .child(
                        div().text_size(px(11.)).text_color(theme.muted).child(
                            crate::locale::text(
                                "启动命令以明文保存在 settings.toml；请勿填写密码、令牌或私钥。",
                                "The launch command is stored in plain text in settings.toml. Do not enter passwords, tokens, or private keys.",
                            ),
                        ),
                    )
                    .child(
                        Button::new("agent-add-custom")
                            .compact()
                            .primary()
                            .label(crate::locale::text("添加自定义 Agent", "Add custom agent"))
                            .disabled(locked)
                            .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                                panel.add_custom_agent(window, cx);
                            })),
                    ),
            )
            .child(
                section_card(crate::locale::text("工具栏布局", "Tool rail layout"), theme)
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "上下移动调整顺序；至少保留一个可见入口。",
                                "Move entries to change their order. Keep at least one entry visible.",
                            )),
                    )
                    .children(tool_rows),
            )
            .child(
                section_card(crate::locale::text("kerminal:// 链接", "kerminal:// links"), theme)
                    .child(
                        div()
                            .w_full()
                            .text_size(px(12.))
                            .text_color(theme.foreground)
                            .child(protocol_message),
                    )
                    .when_some(self.external_protocol_error.as_ref(), |card, error| {
                        card.child(
                            div()
                                .w_full()
                                .text_size(px(12.))
                                .text_color(theme.error)
                                .child(crate::locale::text(
                                    if protocol_error_is_operation {
                                        format!("上次操作：{error}")
                                    } else {
                                        format!("状态读取失败：{error}")
                                    },
                                    if protocol_error_is_operation {
                                        format!("Last operation: {error}")
                                    } else {
                                        format!("Status refresh failed: {error}")
                                    },
                                )),
                        )
                    })
                    .when(self.external_protocol_requires_refresh, |card| {
                        card.child(
                            div()
                                .w_full()
                                .text_size(px(12.))
                                .text_color(theme.warning)
                                .child(crate::locale::text(
                                    "请先刷新状态，再决定是否继续。",
                                    "Refresh the status before deciding whether to continue.",
                                )),
                        )
                    })
                    .when(self.external_protocol_outcome_unknown, |card| {
                        card.child(
                            div()
                                .w_full()
                                .text_size(px(12.))
                                .text_color(theme.warning)
                                .child(if self.external_protocol_requires_refresh {
                                    crate::locale::text(
                                        "上次操作结果未知，当前状态需要重新读取。",
                                        "The previous operation result is unknown; read the current state again.",
                                    )
                                } else if protocol_unknown_readback_supported {
                                    crate::locale::text(
                                        "上次操作结果未知；当前状态已回读，但不能证明上次操作成功。",
                                        "The previous operation result is unknown. The current state was read back, but this does not prove the operation succeeded.",
                                    )
                                } else {
                                    crate::locale::text(
                                        "上次操作结果未知；当前平台无法确认当前关联状态。",
                                        "The previous operation result is unknown, and this platform cannot confirm the current handler state.",
                                    )
                                }),
                        )
                    })
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .flex_wrap()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .text_size(px(12.))
                                    .text_color(theme.muted)
                                    .child(crate::locale::text(
                                        "可为当前用户注册或移除协议；不会覆盖其他程序的关联。",
                                        "Register or remove the current user's handler. Another application's association will not be overwritten.",
                                    )),
                            )
                            .when_some(protocol_action, |row, action| {
                                row.child(
                                    Button::new("external-protocol-change")
                                        .compact()
                                        .primary()
                                        .label(match (self.external_protocol_loading, action) {
                                            (true, super::protocol::ExternalProtocolAction::Register) => {
                                                crate::locale::text("注册中…", "Registering…")
                                            }
                                            (true, super::protocol::ExternalProtocolAction::Unregister) => {
                                                crate::locale::text("注销中…", "Unregistering…")
                                            }
                                            (false, super::protocol::ExternalProtocolAction::Register) => {
                                                crate::locale::text("注册协议", "Register handler")
                                            }
                                            (false, super::protocol::ExternalProtocolAction::Unregister) => {
                                                crate::locale::text("注销协议", "Unregister handler")
                                            }
                                        })
                                        .disabled(self.external_protocol_loading)
                                        .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                                            panel.change_external_protocol(action, cx);
                                        })),
                                )
                            })
                            .child(
                                Button::new("external-protocol-refresh")
                                    .compact()
                                    .ghost()
                                    .label(if self.external_protocol_loading {
                                        crate::locale::text("读取中…", "Reading…")
                                    } else {
                                        crate::locale::text("刷新状态", "Refresh status")
                                    })
                                    .disabled(
                                        self.external_protocol_loading
                                            || self.external_protocol.is_none(),
                                    )
                                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                        panel.refresh_external_protocol(cx);
                                    })),
                            ),
                    ),
            )
            .children(
                self.settings_input_errors
                    .values()
                    .map(|message| issue_row(message, theme)),
            )
            .when_some(
                settings_error_banner(self.settings_error.as_deref(), theme),
                |root, banner| root.child(banner),
            )
    }

    /// Agent Launcher 草稿只在普通输入框更新，直到明确添加才成为可持久化设置。
    pub(super) fn update_custom_agent_text(
        &mut self,
        field: super::AgentLauncherTextField,
        value: String,
        cx: &mut Context<Self>,
    ) {
        if self.settings_controls_locked() {
            return;
        }
        match field {
            super::AgentLauncherTextField::Name => self.custom_agent_name = value,
            super::AgentLauncherTextField::Command => self.custom_agent_command = value,
        }
        self.settings_input_errors.remove("custom_agent");
        cx.notify();
    }

    /// 自定义 Agent 仅通过 application 定义的身份构造器加入设置快照，不在 UI 生成 ID。
    pub(super) fn add_custom_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_controls_locked() {
            return;
        }
        let name = self.custom_agent_name.trim();
        let command = self.custom_agent_command.trim();
        let Some(settings) = self.settings.as_ref().map(|draft| draft.value()) else {
            return;
        };
        let duplicate = settings
            .agent_launcher
            .custom_agents
            .iter()
            .any(|agent| agent.name.eq_ignore_ascii_case(name));
        if name.is_empty() || command.is_empty() || duplicate || name.chars().count() > 64 {
            self.settings_input_errors.insert(
                "custom_agent",
                if duplicate {
                    crate::locale::text(
                        "自定义 Agent 名称不能重复。",
                        "Custom agent names must be unique.",
                    )
                    .to_string()
                } else {
                    crate::locale::text(
                        "请输入名称和启动命令；名称不能超过 64 个字符。",
                        "Enter a name and launch command. Names must be at most 64 characters.",
                    )
                    .to_string()
                },
            );
            cx.notify();
            return;
        }
        if settings.agent_launcher.custom_agents.len() >= 32 {
            self.settings_input_errors.insert(
                "custom_agent",
                crate::locale::text(
                    "自定义 Agent 最多保存 32 个。",
                    "At most 32 custom agents can be saved.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let new_agent = CustomAgentDefinition::new(name.to_owned(), command.to_owned());
        let selected_key = format!("custom:{}", new_agent.id);
        self.edit_settings(
            |settings| {
                settings.agent_launcher.custom_agents.push(new_agent);
                settings.agent_launcher.selected_agent_key = selected_key;
            },
            cx,
        );
        self.custom_agent_name.clear();
        self.custom_agent_command.clear();
        set_input(&self.agent_launcher_inputs.name, String::new(), window, cx);
        set_input(
            &self.agent_launcher_inputs.command,
            String::new(),
            window,
            cx,
        );
        self.settings_input_errors.remove("custom_agent");
    }

    /// 删除已选中 launcher 时先回退到稳定内置项，避免保存悬空的 custom ID。
    pub(super) fn remove_custom_agent(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.settings_controls_locked() {
            return;
        }
        self.edit_settings(
            |settings| {
                settings
                    .agent_launcher
                    .custom_agents
                    .retain(|agent| agent.id != id);
                if settings.agent_launcher.selected_agent_key == format!("custom:{id}") {
                    settings.agent_launcher.selected_agent_key = "builtin:codex".to_owned();
                }
            },
            cx,
        );
    }

    /// 偏好页排序与右键排序共用分区规则，避免 hidden 或兼容项参与可见顺序调整。
    ///
    /// @author kongweiguang
    pub(super) fn move_tool_rail_item(
        &mut self,
        tool: ToolRailToolId,
        direction: isize,
        cx: &mut Context<Self>,
    ) {
        if self.settings_controls_locked() {
            return;
        }
        let Some(mut candidate) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return;
        };
        let Ok(direction) = i32::try_from(direction) else {
            return;
        };
        if !move_visible_tool_rail_item(&mut candidate.tool_rail, tool, direction) {
            return;
        }
        self.edit_settings(|settings| settings.tool_rail = candidate.tool_rail, cx);
    }

    /// Root 右键菜单以 CAS 快速保存排序；若偏好草稿不干净，调用者收到可显示的拒绝原因。
    ///
    /// @author kongweiguang
    pub fn apply_tool_rail_order_change(
        &mut self,
        tool: ToolRailToolId,
        direction: i32,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if !matches!(direction, -1 | 1) {
            return Err(crate::locale::text(
                "工具栏排序方向无效。",
                "The tool rail move is invalid.",
            )
            .to_string());
        }
        self.apply_tool_rail_change(
            tool,
            move |settings| {
                move_visible_tool_rail_item(settings, tool, direction);
            },
            cx,
        )
    }

    /// 从工作台右键菜单快速固定或取消固定入口；移出底部时保留原有全局顺序。
    ///
    /// @author kongweiguang
    pub fn apply_tool_rail_bottom_change(
        &mut self,
        tool: ToolRailToolId,
        pinned: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.apply_tool_rail_change(
            tool,
            move |settings| {
                if pinned {
                    if !settings.bottom.contains(&tool) {
                        settings.bottom.push(tool);
                    }
                } else {
                    settings.bottom.retain(|candidate| *candidate != tool);
                }
            },
            cx,
        )
    }

    /// 工具位置通过设置草稿与 CAS 保存；每项都允许四种宿主并拒绝迟到的无效写入。
    ///
    /// @author kongweiguang
    pub fn apply_tool_rail_placement_change(
        &mut self,
        tool: ToolRailToolId,
        placement: ToolRailPanelPlacement,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.apply_tool_rail_change(
            tool,
            move |settings| {
                settings.panel_placements.insert(tool, placement);
            },
            cx,
        )
    }

    /// 自定义器把整套可见 rail 草稿交给同一次 CAS；退役 Context 字段严格沿用已读取值。
    ///
    /// @author kongweiguang
    pub fn apply_tool_rail_settings_change(
        &mut self,
        requested: ToolRailSettings,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if let Some(message) = tool_rail_edit_readiness_error(self) {
            return Err(message);
        }
        let Some(original) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return Err(
                crate::locale::text("设置尚未读取，请重试。", "Settings are not loaded.")
                    .to_string(),
            );
        };

        let mut accepted = requested;
        let current = &original.tool_rail;
        let default_order = ToolRailSettings::default().order;
        let context_order_index = current
            .order
            .iter()
            .position(|tool| *tool == ToolRailToolId::Context);
        accepted
            .order
            .retain(|tool| *tool != ToolRailToolId::Context);
        let mut order = Vec::new();
        for tool in accepted.order {
            if is_customizable_tool_rail_tool(tool) && !order.contains(&tool) {
                order.push(tool);
            }
        }
        for tool in default_order.iter().copied() {
            if !order.contains(&tool) {
                order.push(tool);
            }
        }
        if !order.is_empty()
            && order.iter().all(|tool| accepted.hidden.contains(tool))
            && let Some(first) = order.first()
        {
            accepted.hidden.retain(|tool| tool != first);
        }
        if let Some(index) = context_order_index {
            order.insert(index.min(order.len()), ToolRailToolId::Context);
        }
        accepted.order = order;

        accepted
            .hidden
            .retain(|tool| is_customizable_tool_rail_tool(*tool));
        if current.hidden.contains(&ToolRailToolId::Context) {
            accepted.hidden.push(ToolRailToolId::Context);
        }
        accepted
            .bottom
            .retain(|tool| is_customizable_tool_rail_tool(*tool));
        if current.bottom.contains(&ToolRailToolId::Context) {
            accepted.bottom.push(ToolRailToolId::Context);
        }
        accepted
            .panel_placements
            .retain(|tool, _| is_customizable_tool_rail_tool(*tool));
        for tool in default_order {
            accepted.panel_placements.entry(tool).or_default();
        }
        if let Some(placement) = current.panel_placements.get(&ToolRailToolId::Context) {
            accepted
                .panel_placements
                .insert(ToolRailToolId::Context, *placement);
        }

        let mut candidate = original.clone();
        candidate.tool_rail = accepted;
        if candidate == original {
            return Ok(());
        }
        let issues = super::super::validate_settings(&candidate, super::keyboard_platform());
        if let Some(issue) = issues.iter().find(|issue| issue.blocks_save()) {
            return Err(issue.message.clone());
        }
        let Some(draft) = self.settings.as_mut() else {
            return Err(crate::locale::text(
                "设置已失效，请重新读取后重试。",
                "Settings are unavailable. Reload them before retrying.",
            )
            .to_string());
        };
        if draft.replace_value(candidate.clone()).is_err() {
            return Err(crate::locale::text(
                "设置状态已变化，请打开设置核对后重试。",
                "Settings changed while applying the tool rail update. Review them before retrying.",
            )
            .to_string());
        }
        self.settings_issues = issues;
        self.status = None;
        self.settings_success_status = None;
        self.pending_tool_rail_save = true;
        if !self.save_settings(cx) {
            self.pending_tool_rail_save = false;
            if let Some(draft) = self.settings.as_mut() {
                let _ = draft.replace_value(original);
            }
            self.refresh_settings_issues();
            return Err(self.status.clone().unwrap_or_else(|| {
                crate::locale::text(
                    "工具栏设置未能提交，请打开设置检查。",
                    "The tool rail change was not submitted. Open Settings to review it.",
                )
                .to_string()
            }));
        }
        Ok(())
    }

    /// Agent 创建入口进入现有设置锚点，不重建草稿或改变其 revision。
    ///
    /// @author kongweiguang
    pub fn show_agent_launcher_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.navigation_search_query.is_empty() {
            self.navigation_search_query.clear();
            set_input(&self.navigation_search, String::new(), window, cx);
        }
        self.section = PreferencesSection::Integrations;
        self.selected_navigation_target = None;
        self.pending_navigation_search_scroll = None;
        let scroll = self.content_scroll.clone();
        window.on_next_frame(move |_, _| scroll.scroll_to_top_of_item(4));
        cx.notify();
    }

    /// 快捷保存只接受干净 Ready 草稿，并在应用确认提交后由保存完成路径发出成功/失败事件。
    ///
    /// @author kongweiguang
    fn apply_tool_rail_change(
        &mut self,
        tool: ToolRailToolId,
        edit: impl FnOnce(&mut ToolRailSettings),
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if let Some(message) = tool_rail_edit_readiness_error(self) {
            return Err(message);
        }
        let Some(original) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return Err(
                crate::locale::text("设置尚未读取，请重试。", "Settings are not loaded.")
                    .to_string(),
            );
        };
        let tool_rail = &original.tool_rail;
        if tool == ToolRailToolId::Context
            || !tool_rail.order.contains(&tool)
            || tool_rail.hidden.contains(&tool)
        {
            return Err(crate::locale::text(
                "此工具入口当前不可调整。",
                "This tool entry cannot be changed right now.",
            )
            .to_string());
        }

        let mut candidate = original.clone();
        edit(&mut candidate.tool_rail);
        if candidate == original {
            return Ok(());
        }
        let issues = super::super::validate_settings(&candidate, super::keyboard_platform());
        if let Some(issue) = issues.iter().find(|issue| issue.blocks_save()) {
            return Err(issue.message.clone());
        }
        let Some(draft) = self.settings.as_mut() else {
            return Err(crate::locale::text(
                "设置已失效，请重新读取后重试。",
                "Settings are unavailable. Reload them before retrying.",
            )
            .to_string());
        };
        if draft.replace_value(candidate.clone()).is_err() {
            return Err(crate::locale::text("设置状态已变化，请打开设置核对后重试。", "Settings changed while applying the tool rail update. Review them before retrying.").to_string());
        }
        self.settings_issues = issues;
        self.status = None;
        self.settings_success_status = None;
        self.pending_tool_rail_save = true;
        if !self.save_settings(cx) {
            self.pending_tool_rail_save = false;
            if let Some(draft) = self.settings.as_mut() {
                let _ = draft.replace_value(original);
            }
            self.refresh_settings_issues();
            return Err(self.status.clone().unwrap_or_else(|| {
                crate::locale::text(
                    "工具栏设置未能提交，请打开设置检查。",
                    "The tool rail change was not submitted. Open Settings to review it.",
                )
                .to_string()
            }));
        }
        Ok(())
    }
}

/// 单个数值字段统一使用 Kit Input，输入错误由 settings 草稿即时校验。
fn number_field(
    label: impl Into<gpui_kit::SharedString>,
    input: &gpui_kit::Entity<gpui_kit::component::input::InputState>,
    disabled: bool,
    _theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_3()
        .child(div().w(px(200.)).text_size(px(12.)).child(label.into()))
        .child(Input::new(input).disabled(disabled).w(px(170.)))
}

/// 扩展设置卡片复用当前全局主题令牌，浅色、深色与系统模式拥有一致对比度。
fn section_card(title: impl Into<gpui_kit::SharedString>, theme: ThemeTokens) -> gpui_kit::Div {
    div()
        .w_full()
        .p_4()
        .flex()
        .flex_col()
        .gap_3()
        .bg(theme.panel)
        .rounded_md()
        .child(
            div()
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(title.into()),
        )
}

/// 错误提示沿用主题语义色，避免恢复和输入问题在某一主题下不可读。
fn issue_row(message: &str, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .px_2()
        .py_1()
        .text_size(px(12.))
        .text_color(theme.error)
        .child(message.to_owned())
}

/// 页面说明只交代这些偏好存放在本机，避免暴露内部文件布局或误报固定路径。
///
/// @author kongweiguang
fn integrations_heading(theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(20.))
                .font_weight(gpui_kit::FontWeight::BOLD)
                .child(crate::locale::text(
                    "通知与集成",
                    "Notifications & integrations",
                )),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "这些偏好保存在本机配置目录。",
                    "These preferences are stored in the local configuration folder.",
                )),
        )
}

/// 内置启动器 key 与 application 用例共享同一 stable key 契约。
fn builtin_agent_buttons(
    selected_agent: &str,
    locked: bool,
    cx: &mut Context<PreferencesPanel>,
) -> Vec<Button> {
    [
        ("builtin:codex", "Codex"),
        ("builtin:claude", "Claude"),
        ("builtin:pi", "Pi"),
    ]
    .into_iter()
    .map(move |(key, label)| {
        let key = key.to_owned();
        let selected = selected_agent == key;
        Button::new(format!("agent-select-{key}"))
            .compact()
            .label(label)
            .disabled(locked)
            .when(selected, Button::primary)
            .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                let selected_key = key.clone();
                panel.edit_settings(
                    |settings| settings.agent_launcher.selected_agent_key = selected_key,
                    cx,
                );
            }))
    })
    .collect()
}

/// 以 stable enum 列出旧外部启动来源，禁用状态仅更新 settings 草稿。
fn external_launch_sources() -> [(ExternalLaunchToolSetting, gpui_kit::SharedString); 7] {
    [
        (ExternalLaunchToolSetting::Putty, "PuTTY".into()),
        (ExternalLaunchToolSetting::Mobaxterm, "MobaXterm".into()),
        (ExternalLaunchToolSetting::Xshell, "Xshell".into()),
        (ExternalLaunchToolSetting::Securecrt, "SecureCRT".into()),
        (ExternalLaunchToolSetting::Openssh, "OpenSSH".into()),
        (
            ExternalLaunchToolSetting::SftpClient,
            crate::locale::text("SFTP 客户端", "SFTP client"),
        ),
        (ExternalLaunchToolSetting::KerminalNative, "Kerminal".into()),
    ]
}

/// 工具面板枚举固定对外显示名称，不依赖 serde 字符串作反向映射。
///
/// @author kongweiguang
fn tool_rail_label(tool: ToolRailToolId) -> gpui_kit::SharedString {
    match tool {
        ToolRailToolId::Context => crate::locale::text("上下文", "Context"),
        ToolRailToolId::AgentLauncher => "Agent Launcher".into(),
        ToolRailToolId::Sftp => "SFTP".into(),
        ToolRailToolId::Containers => crate::locale::text("容器", "Containers"),
        ToolRailToolId::Snippets => crate::locale::text("命令片段", "Command snippets"),
        ToolRailToolId::Tmux => "tmux".into(),
        ToolRailToolId::Ports => crate::locale::text("端口转发", "Port forwarding"),
        ToolRailToolId::System => crate::locale::text("系统信息", "System information"),
        ToolRailToolId::Logs => crate::locale::text("日志", "Logs"),
    }
}

/// 每个可配置工具都显示四种打开位置，与右栏自定义器保持一致。
///
/// @author kongweiguang
fn placement_options(
    _tool: ToolRailToolId,
) -> Vec<(&'static str, gpui_kit::SharedString, ToolRailPanelPlacement)> {
    vec![
        (
            "attached",
            crate::locale::text("右栏", "Right rail"),
            ToolRailPanelPlacement::Attached,
        ),
        (
            "left",
            crate::locale::text("左侧", "Left"),
            ToolRailPanelPlacement::Left,
        ),
        (
            "bottom",
            crate::locale::text("底部", "Bottom"),
            ToolRailPanelPlacement::Bottom,
        ),
        (
            "center",
            crate::locale::text("中央", "Center"),
            ToolRailPanelPlacement::Center,
        ),
    ]
}

/// 固定工具拒绝迟到 UI 回调，避免重写仍需兼容读取的存量位置配置。
///
/// @author kongweiguang
fn can_change_tool_rail_placement(_tool: ToolRailToolId) -> bool {
    true
}

/// 批量草稿只接受本次八项，退役 Context 由已读配置单独恢复。
///
/// @author kongweiguang
fn is_customizable_tool_rail_tool(tool: ToolRailToolId) -> bool {
    tool != ToolRailToolId::Context
}

#[cfg(test)]
#[path = "../../../../tests/unit/preferences_tool_rail_order.rs"]
mod tool_rail_order_tests;

#[cfg(test)]
#[path = "../../../../tests/unit/preferences_tool_rail_placement.rs"]
mod tool_rail_placement_tests;
