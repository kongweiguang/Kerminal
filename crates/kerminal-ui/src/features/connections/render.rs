//! 连接导航、主机摘要和可见操作的 GPUI 绘制。
//!
//! @author kongweiguang

use chrono::{DateTime, Datelike, SecondsFormat, Utc};
use gpui_kit::{
    ClickEvent, Context, Focusable, IntoElement, Render, Window,
    component::{
        Disableable, FocusTrapElement as _, Sizable as _,
        button::{Button, ButtonVariants},
        input::Input,
    },
    div,
    prelude::*,
    px, rems,
};
use kerminal_application::connections::management::ManagedHost;
use kerminal_application::preferences::VersionedProfile;
use kerminal_domain::{
    connections::{ConnectionCapabilities, RemoteProtocol, SshAuthentication},
    preferences::{ConfigurationMode, TerminalProfile},
};

use crate::theme::ThemeTokens;
use crate::ui_button::icon_button;

use super::{
    external::{render_external_password_prompt, render_external_username_prompt},
    form_render::render_editor,
    panel::ConnectionsPanel,
    trust::render_trust_confirmation,
};

impl Render for ConnectionsPanel {
    /// 编辑时表单占用独立工作区；普通模式只绘制左栏选中主机详情，避免重复中央目录。
    /// 无编辑器时的未知本地主机写入提示挂在面板层，避免切换选择后丢失唯一核对入口。
    /// 从 Kit 全局主题取色，确保编辑页与左侧导航保持相同的浅深主题语义。
    /// 编辑器内分组作为最上层小弹层呈现，原表单实体和草稿持续保留，Tab 不越过当前弹层。
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.start_configuration_watch(cx);
        self.refresh_clean_editor(window, cx);
        let theme = ThemeTokens::current(window, cx);
        let readonly = self.recovery.mode == ConfigurationMode::ReadOnlyRecovery;
        let editing = self.editor_open();
        let editing_group = self.presentation.shows_group_editor();
        let content = if editing {
            render_editor(self, theme, window, cx).into_any_element()
        } else if editing_group {
            render_group_rename_editor(self, readonly, theme, cx).into_any_element()
        } else if let Some(host) = self.selected_host_snapshot() {
            render_host_details(self, &host, theme, cx).into_any_element()
        } else if let Some(profile) = self
            .local_hosts
            .selected
            .as_ref()
            .map(|row| row.profile.clone())
        {
            render_local_host_details(self, &profile, theme, cx).into_any_element()
        } else if self.local_hosts.loading && self.local_hosts.selected_id.is_some() {
            local_host_loading_state(theme).into_any_element()
        } else {
            empty_selection(theme).into_any_element()
        };
        let body = div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .overflow_hidden()
            .child(content);

        let mut panel = div()
            .id("connections-panel")
            .capture_key_down(cx.listener(|panel, event, window, cx| {
                panel.handle_editor_save_key(event, window, cx);
            }))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.background)
            .text_color(theme.foreground);
        panel = panel.child(body);
        if editing && editing_group {
            let focus_handle = self.group_name_input.read(cx).focus_handle(cx);
            panel = panel.relative().child(
                div()
                    .id("connections-inline-group-overlay")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .p_4()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme.background.opacity(0.8))
                    .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                        panel.cancel_group_rename(window, cx)
                    }))
                    .child(
                        div()
                            .id("connections-inline-group-card")
                            .w_full()
                            .max_w(rems(32.0))
                            .h(rems(24.0))
                            .max_h_full()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .rounded_lg()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.panel)
                            .on_click(|_, _, cx| cx.stop_propagation())
                            .child(render_group_rename_editor(self, readonly, theme, cx))
                            .focus_trap("connections-inline-group-focus-trap", &focus_handle),
                    ),
            );
        }
        if !editing
            && !editing_group
            && self.local_hosts.save_outcome_unknown
            && self.local_hosts.pending_delete.is_none()
        {
            panel = panel.child(render_local_host_unknown_banner(self, theme, cx));
        }
        if self.pending_group_deletion.is_some() {
            panel = panel.child(render_group_deletion_confirmation(self, theme, cx));
        }
        if let Some(pending_delete) = self.local_hosts.pending_delete.clone() {
            panel = panel.child(render_local_host_deletion_confirmation(
                self,
                &pending_delete,
                theme,
                cx,
            ));
        }
        if !editing {
            let status = if self.local_hosts.selected.is_some() {
                self.local_hosts
                    .status
                    .clone()
                    .or_else(|| self.status.clone())
            } else {
                self.status.clone()
            };
            panel = panel.child(render_status(status, theme));
        }

        panel
            .children(render_trust_confirmation(
                self.pending_trust.as_ref(),
                self.recovery.mode,
                theme,
                cx,
            ))
            .children(render_external_password_prompt(self, theme, cx))
            .children(render_external_username_prompt(self, theme, cx))
    }
}

/// 将本地主机作为普通连接详情呈现；紧凑编辑图标节省操作栏宽度，连接、默认项和配置复制仍保留明确文字与原目标绑定。
/// 环境变量只展示名称以免把令牌值暴露在连接目录中；写入仍受共享配置门禁保护。
///
/// @author kongweiguang
fn render_local_host_details(
    panel: &mut ConnectionsPanel,
    profile: &TerminalProfile,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let profile_id = profile.id.clone();
    let can_write = panel.recovery.mode != ConfigurationMode::ReadOnlyRecovery
        && !panel.local_hosts.loading
        && !panel.local_hosts.saving
        && !panel.local_hosts.save_outcome_unknown;
    let can_connect = !panel.local_hosts.loading && !panel.local_hosts.save_outcome_unknown;
    let mut detail = div()
        .id("connections-local-host-details")
        .size_full()
        .flex()
        .flex_col()
        .gap_4()
        .overflow_y_scroll()
        .p_6()
        .bg(theme.background)
        .child(
            div()
                .w_full()
                .flex()
                .items_start()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(23.))
                                .font_weight(gpui_kit::FontWeight::BOLD)
                                .child(profile.name.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(theme.muted)
                                .child(crate::locale::text("本地主机", "Local host")),
                        ),
                )
                .child(
                    Button::new("connections-local-host-deselect")
                        .compact()
                        .ghost()
                        .label(crate::locale::text("取消选择", "Deselect"))
                        .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                            panel.select_local_host(None, window, cx)
                        })),
                ),
        );

    detail = detail.child(local_host_configuration_card(profile, theme));

    let connect_profile_id = profile_id.clone();
    let default_profile_id = profile_id.clone();
    let edit_profile_id = profile_id.clone();
    let duplicate_profile_id = profile_id.clone();
    let delete_profile_id = profile_id;
    let is_default = profile.is_default;
    let actions = div()
        .w_full()
        .flex()
        .flex_wrap()
        .gap_2()
        .child(
            Button::new("connections-local-host-connect")
                .compact()
                .when(true, Button::primary)
                .label(crate::locale::text("打开终端", "Open terminal"))
                .disabled(!can_connect)
                .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                    panel.request_connect_local_host(connect_profile_id.clone(), cx)
                })),
        )
        .child({
            let label = crate::locale::text("编辑本地主机", "Edit local host");
            icon_button(
                "connections-local-host-edit",
                gpui_kit::assets::IconName::Pencil,
                label.clone(),
            )
            .with_size(gpui_kit::component::Size::Medium)
            .tooltip(label)
            .disabled(!can_write)
            .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                panel.begin_edit_local_host(edit_profile_id.clone(), window, cx)
            }))
        });
    let actions = actions.child(
        Button::new("connections-local-host-set-default")
            .compact()
            .label(if is_default {
                crate::locale::text("已默认", "Default")
            } else {
                crate::locale::text("设为默认", "Set as default")
            })
            .disabled(!can_write || is_default)
            .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                panel.request_set_default_local_host(default_profile_id.clone(), cx)
            })),
    );
    let actions = if can_write {
        actions
            .child(
                Button::new("connections-local-host-duplicate")
                    .compact()
                    .label(crate::locale::text("复制配置…", "Duplicate config…"))
                    .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                        panel.begin_duplicate_local_host(duplicate_profile_id.clone(), window, cx)
                    })),
            )
            .child(
                Button::new("connections-local-host-delete")
                    .compact()
                    .danger()
                    .label(crate::locale::text("删除…", "Delete…"))
                    .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                        panel.request_delete_local_host(delete_profile_id.clone(), window, cx)
                    })),
            )
    } else {
        actions
    };
    detail.child(actions)
}

/// 未知的 Profile 写入必须先读回真实配置；独立横幅不依赖当前选中项，并阻止重复读回。
/// @author kongweiguang
fn render_local_host_unknown_banner(
    panel: &mut ConnectionsPanel,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let reading = panel.local_hosts.loading || panel.local_hosts.readback_in_progress;
    let disabled = reading || panel.local_hosts.saving;
    div()
        .id("connections-local-host-unknown-reconciliation")
        .w_full()
        .px_4()
        .py_3()
        .flex()
        .items_center()
        .gap_3()
        .bg(theme.panel)
        .border_1()
        .border_color(theme.warning)
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "本地主机更改结果未知；不会自动重放，请先刷新并核对实际配置。",
                    "A local host change has an unknown result. It will not be replayed; read back and review the stored configuration first.",
                )),
        )
        .child(
            Button::new("connections-review-local-host-unknown-banner")
                .compact()
                .label(if reading {
                    crate::locale::text("正在核对…", "Reviewing…")
                } else {
                    crate::locale::text("刷新并核对", "Refresh and review")
                })
                .disabled(disabled)
                .when(!disabled, Button::primary)
                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                    panel.review_local_host_unknown(cx)
                })),
        )
}

/// 异步读取所选本地主机期间保留明确的加载反馈，避免把暂时空快照误显示为无选择。
///
/// @author kongweiguang
fn local_host_loading_state(theme: ThemeTokens) -> impl IntoElement {
    div()
        .id("connections-local-host-loading")
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(theme.background)
        .text_size(theme.text_body)
        .text_color(theme.muted)
        .child(crate::locale::text(
            "正在读取本地主机…",
            "Loading local host…",
        ))
}

/// 详情卡片把启动参数逐项显示，并隐藏环境变量值，避免把它们误当作安全的配置摘要。
///
/// @author kongweiguang
fn local_host_configuration_card(
    profile: &TerminalProfile,
    theme: ThemeTokens,
) -> impl IntoElement {
    let mut card = div()
        .id("connections-local-host-configuration")
        .w_full()
        .p_4()
        .flex()
        .flex_col()
        .gap_3()
        .rounded_md()
        .bg(theme.panel)
        .child(
            div()
                .text_size(px(14.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(crate::locale::text("启动设置", "Launch settings")),
        )
        .child(local_host_detail_row(
            crate::locale::text("Shell", "Shell"),
            profile.shell.clone(),
            theme,
        ));

    if !profile.args.is_empty() {
        card = card.child(
            div()
                .w_full()
                .flex()
                .gap_2()
                .child(local_host_detail_label(
                    crate::locale::text("参数", "Arguments"),
                    theme,
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .children(profile.args.iter().enumerate().map(|(index, argument)| {
                            div()
                                .id(format!("connections-local-host-argument-{index}"))
                                .text_size(theme.text_xs)
                                .text_color(theme.foreground)
                                .child(if argument.is_empty() {
                                    "\"\"".to_owned()
                                } else {
                                    argument.clone()
                                })
                        })),
                ),
        );
    }

    card = card.child(local_host_detail_row(
        crate::locale::text("工作目录", "Working directory"),
        profile
            .cwd
            .clone()
            .unwrap_or_else(|| crate::locale::text("由系统选择", "System default").to_string()),
        theme,
    ));

    if !profile.env.is_empty() {
        let names = profile.env.keys().cloned().collect::<Vec<_>>().join(" · ");
        card = card.child(local_host_detail_row(
            crate::locale::text("环境变量", "Environment"),
            crate::locale::text(
                format!("{names}（值已隐藏）"),
                format!("{names} (values hidden)"),
            )
            .to_string(),
            theme,
        ));
    }

    card
}

/// 用统一的标签列对齐本地主机详情，长路径由父级纵向滚动区域承载。
///
/// @author kongweiguang
fn local_host_detail_row(
    label: impl Into<gpui_kit::SharedString>,
    value: impl Into<gpui_kit::SharedString>,
    theme: ThemeTokens,
) -> impl IntoElement {
    let label: gpui_kit::SharedString = label.into();
    let value: gpui_kit::SharedString = value.into();
    div()
        .w_full()
        .flex()
        .gap_3()
        .child(local_host_detail_label(label, theme))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_size(theme.text_xs)
                .text_color(theme.foreground)
                .child(value),
        )
}

/// 固定标签宽度以便不同本地启动字段保持可扫读的纵向层级。
///
/// @author kongweiguang
fn local_host_detail_label(
    label: impl Into<gpui_kit::SharedString>,
    theme: ThemeTokens,
) -> impl IntoElement {
    let label: gpui_kit::SharedString = label.into();
    div()
        .w(px(112.))
        .flex_shrink_0()
        .text_size(theme.text_xs)
        .text_color(theme.muted)
        .child(label)
}

impl ConnectionsPanel {
    /// 返回选中主机快照副本，避免绘制代码持有跨事件可变借用。
    fn selected_host_snapshot(&self) -> Option<ManagedHost> {
        let selected = self.selected_host.as_ref()?;
        self.rows.iter().find(|host| &host.id == selected).cloned()
    }
}

/// 分组创建可嵌在主机编辑弹层内；两种入口共用输入、CAS 和未知结果核对流程。
///
/// @author kongweiguang
fn render_group_rename_editor(
    panel: &mut ConnectionsPanel,
    readonly: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let disabled =
        readonly || panel.configuration_mutation_busy || panel.group_save_reconciliation.is_some();
    let creating = panel.editing_group_id.is_none();
    let save_label = if panel.configuration_mutation_busy {
        crate::locale::text("正在保存…", "Saving…")
    } else if creating {
        crate::locale::text("创建分组", "Create group")
    } else {
        crate::locale::text("保存", "Save")
    };
    let reconciliation_view = panel.group_save_reconciliation.map(|state| match state {
        super::panel::GroupSaveReconciliation::NeedsReadback => div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .text_size(theme.text_xs)
            .text_color(theme.warning)
            .child(crate::locale::text(
                "保存结果未知；先读取当前配置，不会重放上次请求。",
                "The save outcome is unknown. Read current configuration first; the prior request will not be replayed.",
            ))
            .child(
                Button::new("connections-verify-group-save")
                    .compact()
                    .label(if panel.loading {
                        crate::locale::text("正在核对…", "Checking…")
                    } else {
                        crate::locale::text("刷新并核对", "Refresh and review")
                    })
                    .disabled(panel.loading || panel.configuration_mutation_busy)
                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                        panel.verify_group_save_outcome(cx)
                    }))
                    .into_any_element(),
            )
            .into_any_element(),
        super::panel::GroupSaveReconciliation::AwaitingUserDecision => {
            let draft_name = panel
                .group_name_input
                .update(cx, |input, _| input.value().to_string());
            let review_facts = super::panel::group_save_review_facts(
                &panel.groups,
                panel.editing_group_id.as_deref(),
                &draft_name,
            );
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "已重新读取配置（仅为当前事实，不代表上次保存成功）：",
                    "Configuration was re-read (current facts only; this does not confirm the prior save):",
                ))
                .child(div().text_color(theme.foreground).child(review_facts))
                .child(
                    Button::new("connections-acknowledge-group-review")
                        .compact()
                        .label(crate::locale::text(
                            "我已核对，允许再次保存",
                            "Reviewed; allow a new save",
                        ))
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.acknowledge_group_save_review(cx)
                        }))
                        .into_any_element(),
                )
                .into_any_element()
        }
    });
    div()
        .id("connections-group-rename-editor")
        .flex_1()
        .min_w(px(0.))
        .min_h(px(0.))
        .flex()
        .justify_center()
        .p_6()
        .overflow_y_scroll()
        .child(
            div()
                .w_full()
                .max_w(px(560.))
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    div()
                        .text_size(theme.text_body)
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .child(if creating {
                            crate::locale::text("新建分组", "New group")
                        } else {
                            crate::locale::text("重命名分组", "Rename group")
                        }),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .text_size(theme.text_xs)
                                .text_color(theme.muted)
                                .child(crate::locale::text("分组名称", "Group name")),
                        )
                        .child(
                            Input::new(&panel.group_name_input)
                                .w_full()
                                .disabled(disabled),
                        ),
                )
                .children(reconciliation_view)
                .when(panel.editor_open(), |content| {
                    content.child(render_status(panel.status.clone(), theme))
                })
                .child(
                    div()
                        .w_full()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("connections-cancel-group-rename")
                                .compact()
                                .ghost()
                                .label(crate::locale::text("取消", "Cancel"))
                                .disabled(panel.configuration_mutation_busy)
                                .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                                    panel.cancel_group_rename(window, cx)
                                })),
                        )
                        .child(
                            Button::new("connections-save-group-editor")
                                .compact()
                                .label(save_label)
                                .disabled(disabled)
                                .when(!disabled, Button::primary)
                                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.create_group(cx)
                                })),
                        ),
                ),
        )
}

/// 选中主机展示协议能力与显式连接按钮；编辑图标只打开当前主机草稿，连接操作仍由用户明确选择。
fn render_host_details(
    panel: &mut ConnectionsPanel,
    host: &ManagedHost,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let capabilities = ConnectionCapabilities::for_protocol(host.protocol);
    let host_id = host.id.clone();
    let can_write = panel.recovery.mode != ConfigurationMode::ReadOnlyRecovery;
    let mut detail = div()
        .id("connections-host-details")
        .size_full()
        .flex()
        .flex_col()
        .gap_4()
        .overflow_y_scroll()
        .p_6()
        .bg(theme.background)
        .child(
            div()
                .w_full()
                .flex()
                .items_start()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(23.))
                                .font_weight(gpui_kit::FontWeight::BOLD)
                                .child(host.name.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(theme.muted)
                                .child(endpoint_label(host)),
                        ),
                )
                .child({
                    let label = crate::locale::text("编辑连接", "Edit connection");
                    icon_button(
                        "connections-edit-host",
                        gpui_kit::assets::IconName::Pencil,
                        label.clone(),
                    )
                    .with_size(gpui_kit::component::Size::Medium)
                    .tooltip(label)
                    .disabled(!can_write)
                    .on_click(cx.listener(
                        move |panel, _: &ClickEvent, window, cx| {
                            panel.edit_host(&host_id, window, cx)
                        },
                    ))
                }),
        )
        .child(render_capability_card(host, capabilities, theme));

    let mut actions = div().w_full().flex().gap_2();
    if capabilities.terminal {
        actions = actions.child(
            Button::new("connections-connect")
                .compact()
                .label(if host.protocol == RemoteProtocol::Serial {
                    crate::locale::text("打开串口终端", "Open serial terminal")
                } else if host.protocol == RemoteProtocol::Telnet {
                    crate::locale::text("打开 Telnet", "Open Telnet")
                } else {
                    crate::locale::text("连接终端", "Connect terminal")
                })
                .when(true, Button::primary)
                .on_click(
                    cx.listener(|panel, _: &ClickEvent, _, cx| panel.connect_selected(false, cx)),
                ),
        );
    } else if host.protocol == RemoteProtocol::Sftp {
        actions = actions.child(
            Button::new("connections-connect-sftp-only")
                .compact()
                .label(crate::locale::text("打开 SFTP", "Open SFTP"))
                .when(true, Button::primary)
                .on_click(
                    cx.listener(|panel, _: &ClickEvent, _, cx| panel.connect_selected(false, cx)),
                ),
        );
    }
    if capabilities.sftp && host.protocol != RemoteProtocol::Sftp {
        actions = actions.child(
            Button::new("connections-open-files")
                .compact()
                .label(crate::locale::text("浏览文件", "Browse files"))
                .on_click(
                    cx.listener(|panel, _: &ClickEvent, _, cx| panel.connect_selected(true, cx)),
                ),
        );
    }
    if capabilities.launch_rdp {
        actions = actions.child(
            Button::new("connections-launch-rdp")
                .compact()
                .label(crate::locale::text("启动远程桌面", "Launch remote desktop"))
                .when(true, Button::primary)
                .on_click(
                    cx.listener(|panel, _: &ClickEvent, _, cx| panel.connect_selected(false, cx)),
                ),
        );
    }
    if can_write {
        let duplicate_host_id = host.id.clone();
        let delete_host_id = host.id.clone();
        actions = actions
            .child(
                Button::new("connections-duplicate-host")
                    .compact()
                    .label(crate::locale::text("复制配置…", "Duplicate config…"))
                    .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                        panel.duplicate_saved_host(&duplicate_host_id, window, cx)
                    })),
            )
            .child(
                Button::new("connections-delete-host")
                    .compact()
                    .danger()
                    .label(crate::locale::text("删除…", "Delete…"))
                    .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                        panel.request_delete_host(&delete_host_id, cx)
                    })),
            );
    }
    detail = detail.child(actions).child(render_auth_card(host, theme));
    if panel
        .pending_host_deletion
        .as_ref()
        .is_some_and(|pending| pending.host_id == host.id)
    {
        detail = detail.child(render_host_deletion_confirmation(panel, host, theme, cx));
    }
    if let Some((status, is_error)) = panel.rdp_status_for_host(&host.id) {
        detail = detail.child(
            div()
                .id("connections-rdp-status")
                .w_full()
                .p_3()
                .rounded_md()
                .bg(theme.panel)
                .text_size(theme.text_xs)
                .text_color(if is_error { theme.error } else { theme.muted })
                .child(status),
        );
    }
    if !host.tags.is_empty() {
        detail = detail.child(div().w_full().flex().flex_wrap().gap_2().children(
            host.tags.iter().map(|tag| {
                div()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(theme.sidebar)
                    .text_size(theme.text_xs)
                    .child(tag.clone())
            }),
        ));
    }
    detail
}

/// 删除分组前显示受影响主机数，并要求用户明确选择目标分组或未分组。
fn render_group_deletion_confirmation(
    panel: &mut ConnectionsPanel,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let Some(pending) = panel.pending_group_deletion.clone() else {
        return div().into_any_element();
    };
    let group = panel
        .groups
        .iter()
        .find(|group| group.id == pending.group_id);
    let group_name = group.map_or_else(
        || crate::locale::text("此分组", "this group"),
        |group| crate::locale::text(group.name.clone(), group.name.clone()),
    );
    let affected = panel
        .rows
        .iter()
        .filter(|host| host.group_id.as_deref() == Some(pending.group_id.as_str()))
        .count();
    let mut card = div()
        .id("connections-delete-group-confirmation")
        .w_full()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .bg(theme.panel)
        .border_1()
        .border_color(theme.warning)
        .child(
            div()
                .text_size(theme.text_body)
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(crate::locale::text(
                    format!("删除“{group_name}”并迁移 {affected} 台主机？"),
                    format!("Delete “{group_name}” and move {affected} hosts?"),
                )),
        )
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "主机归属与分组删除在一个配置事务中提交。",
                    "Host reassignment and group deletion are committed in one configuration transaction.",
                )),
        );
    for destination in panel
        .groups
        .iter()
        .filter(|candidate| candidate.id != pending.group_id)
        .cloned()
        .collect::<Vec<_>>()
    {
        let id = destination.id.clone();
        card = card.child(
            Button::new(format!("connections-delete-group-move-{}", id))
                .compact()
                .label(crate::locale::text(
                    format!("迁移到 {} 并删除", destination.name),
                    format!("Move to {} and delete", destination.name),
                ))
                .disabled(panel.configuration_mutation_busy)
                .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                    panel.confirm_group_deletion(Some(id.clone()), cx)
                })),
        );
    }
    card = card.child(
        Button::new("connections-delete-group-ungroup")
            .compact()
            .danger()
            .label(crate::locale::text(
                "迁移到未分组并删除",
                "Move to ungrouped and delete",
            ))
            .disabled(panel.configuration_mutation_busy)
            .on_click(
                cx.listener(|panel, _: &ClickEvent, _, cx| panel.confirm_group_deletion(None, cx)),
            ),
    );
    card.child(
        Button::new("connections-delete-group-cancel")
            .compact()
            .ghost()
            .label(crate::locale::text("取消", "Cancel"))
            .disabled(panel.configuration_mutation_busy)
            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| panel.cancel_group_deletion(cx))),
    )
    .into_any_element()
}

/// 删除主机前确认精确名称和配置后果；凭据是否可清理由回执单独说明。
fn render_host_deletion_confirmation(
    panel: &mut ConnectionsPanel,
    host: &ManagedHost,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    div()
        .id("connections-delete-host-confirmation")
        .w_full()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .bg(theme.panel)
        .border_1()
        .border_color(theme.error)
        .child(
            div()
                .text_size(theme.text_body)
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(crate::locale::text(
                    format!("删除主机配置“{}”？", host.name),
                    format!("Delete host configuration “{}”?", host.name),
                )),
        )
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "主机文件删除后，仍被其他主机引用或无法确认安全的 vault 凭据会保留并单独提示。",
                    "After the host file is deleted, vault credentials still referenced elsewhere or not safe to verify are retained and reported separately.",
                )),
        )
        .child(
            div()
                .flex()
                .gap_2()
                .child(
                    Button::new("connections-delete-host-confirm")
                        .compact()
                        .danger()
                        .label(if panel.configuration_mutation_busy {
                            crate::locale::text("正在删除…", "Deleting…")
                        } else {
                            crate::locale::text("确认删除配置", "Delete configuration")
                        })
                        .disabled(panel.configuration_mutation_busy)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.confirm_host_deletion(cx)
                        })),
                )
                .child(
                    Button::new("connections-delete-host-cancel")
                        .compact()
                        .ghost()
                        .label(crate::locale::text("取消", "Cancel"))
                        .disabled(panel.configuration_mutation_busy)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.cancel_host_deletion(cx)
                        })),
                ),
        )
}

/// 删除确认始终绑定同一 versioned 快照，展示稳定 ID，并在未知回执时只允许显式读回。
/// @author kongweiguang
fn render_local_host_deletion_confirmation(
    panel: &mut ConnectionsPanel,
    pending: &VersionedProfile,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let profile_id = &pending.profile.id;
    let readonly = panel.recovery.mode == ConfigurationMode::ReadOnlyRecovery;
    let saving = panel.local_hosts.saving;
    let outcome_unknown = panel.local_hosts.save_outcome_unknown;
    let reading = panel.local_hosts.loading || panel.local_hosts.readback_in_progress;
    let detail = if outcome_unknown {
        crate::locale::text(
            "删除回执未知，不会自动重试。请先读回并核对当前配置。",
            "The delete receipt is unknown and will not be retried. Read back and review the stored configuration first.",
        )
    } else if saving {
        crate::locale::text(
            "正在等待删除结果；请勿重复提交。",
            "Waiting for the delete result. Do not submit it again.",
        )
    } else if readonly {
        crate::locale::text(
            "配置处于只读恢复模式，当前只能取消此操作。",
            "Configuration is read-only during recovery; this request can only be cancelled.",
        )
    } else {
        crate::locale::text(
            "只删除本地主机配置，不会关闭已经打开的终端。",
            "Only the local host configuration is deleted; already-open terminals are not closed.",
        )
    };
    let mut card = div()
        .id("connections-delete-local-host-confirmation")
        .w_full()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .bg(theme.panel)
        .border_1()
        .border_color(theme.error)
        .child(
            div()
                .text_size(theme.text_body)
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(crate::locale::text(
                    format!("删除本地主机配置“{}”？", pending.profile.name),
                    format!(
                        "Delete local host configuration “{}”?",
                        pending.profile.name
                    ),
                )),
        )
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(crate::locale::text(
                    format!("精确目标：{}", profile_id),
                    format!("Exact target: {}", profile_id),
                )),
        )
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(detail),
        );

    if outcome_unknown {
        card = card.child(
            Button::new("connections-review-local-host-delete")
                .compact()
                .label(if reading {
                    crate::locale::text("正在核对…", "Reviewing…")
                } else {
                    crate::locale::text("刷新并核对", "Refresh and review")
                })
                .disabled(reading || saving)
                .when(!reading && !saving, Button::primary)
                .on_click(
                    cx.listener(|panel, _: &ClickEvent, _, cx| panel.review_local_host_unknown(cx)),
                ),
        );
    } else {
        card = card.child(
            Button::new("connections-delete-local-host-confirm")
                .compact()
                .danger()
                .label(if saving {
                    crate::locale::text("正在删除…", "Deleting…")
                } else {
                    crate::locale::text("确认删除配置", "Delete configuration")
                })
                .disabled(readonly || saving || panel.local_hosts.loading)
                .on_click(
                    cx.listener(|panel, _: &ClickEvent, _, cx| panel.confirm_delete_local_host(cx)),
                ),
        );
    }

    card.child(
        Button::new("connections-delete-local-host-cancel")
            .compact()
            .ghost()
            .label(crate::locale::text("取消", "Cancel"))
            .disabled(saving || outcome_unknown)
            .on_click(
                cx.listener(|panel, _: &ClickEvent, _, cx| panel.cancel_delete_local_host(cx)),
            ),
    )
}

/// 显示协议 capabilities 的真实边界，避免按钮对不支持的连接误导用户。
/// 能力卡同时展示配置时间；时间统一为项目使用的 UTC ISO 格式，坏值明确显示缺失。
///
/// @author kongweiguang
fn render_capability_card(
    host: &ManagedHost,
    capabilities: ConnectionCapabilities,
    theme: ThemeTokens,
) -> impl IntoElement {
    let created_at = display_host_timestamp(&host.created_at);
    let updated_at = display_host_timestamp(&host.updated_at);
    let mut names = vec![protocol_label(host.protocol).to_owned()];
    if capabilities.terminal {
        names.push(crate::locale::text("终端", "Terminal").to_string());
    }
    if capabilities.exec {
        names.push(crate::locale::text("远程命令", "Remote commands").to_string());
    }
    if capabilities.sftp {
        names.push(crate::locale::text("SFTP 文件", "SFTP files").to_string());
    }
    if capabilities.tunnel {
        names.push(crate::locale::text("端口转发", "Port forwarding").to_string());
    }
    if capabilities.launch_rdp {
        names.push(crate::locale::text("系统 RDP 客户端", "System RDP client").to_string());
    }
    div()
        .w_full()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .rounded_md()
        .bg(theme.panel)
        .child(
            div()
                .text_size(px(14.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(crate::locale::text("能力", "Capabilities")),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme.muted)
                .child(names.join(" · ")),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme.muted)
                .child(crate::locale::text(
                    format!("创建于 {} · 更新于 {}", created_at, updated_at),
                    format!("Created {} · updated {}", created_at, updated_at),
                )),
        )
}

/// 将保存的 Unix 毫秒或 RFC3339 时间规范化为 UTC；无效和超出四位年份的值统一显示为未知。
/// 使用 Chrono 处理日历边界，避免手写日期算法并保持与工作区元数据相同的 UTC 展示。
/// @author kongweiguang
fn display_host_timestamp(value: &str) -> String {
    let value = value.trim();
    let parsed = if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        value
            .parse::<i64>()
            .ok()
            .and_then(DateTime::<Utc>::from_timestamp_millis)
    } else {
        DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|timestamp| timestamp.with_timezone(&Utc))
    };

    parsed
        .filter(|timestamp| (1..=9999).contains(&timestamp.year()))
        .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|| crate::locale::text("未知", "Unknown").to_string())
}

/// 认证详情只展示模式与 opaque ref 状态；非 Windows RDP 明示外部客户端不会自动收到保存密码。
fn render_auth_card(host: &ManagedHost, theme: ThemeTokens) -> impl IntoElement {
    let authentication = match host.protocol {
        RemoteProtocol::Ssh | RemoteProtocol::Sftp => match host.authentication.as_ref() {
            Some(SshAuthentication::Agent) => "SSH Agent".to_owned(),
            Some(SshAuthentication::Password { .. }) => crate::locale::text(
                "密码认证（vault 引用）",
                "Password authentication (vault reference)",
            )
            .to_string(),
            Some(SshAuthentication::PrivateKey {
                path,
                secret,
                passphrase,
            }) => {
                let path_state = if path.is_some() {
                    ("已设置", "set")
                } else {
                    ("未设置", "not set")
                };
                let secret_state = if secret.is_some() {
                    ("已设置", "set")
                } else {
                    ("未设置", "not set")
                };
                let passphrase_state = if passphrase.is_some() {
                    ("已设置", "set")
                } else {
                    ("未设置", "not set")
                };
                crate::locale::text(
                    format!(
                        "私钥文件{} · 私钥密文{} · 私钥口令{}",
                        path_state.0, secret_state.0, passphrase_state.0
                    ),
                    format!(
                        "Private key file {} · private key secret {} · key passphrase {}",
                        path_state.1, secret_state.1, passphrase_state.1
                    ),
                )
                .to_string()
            }
            None => crate::locale::text(
                "认证引用缺失，请编辑主机补齐",
                "Authentication reference is missing; edit the host to add it",
            )
            .to_string(),
        },
        RemoteProtocol::Rdp => if host.rdp_password.is_some() {
            crate::locale::text("RDP 密码（vault 引用）", "RDP password (vault reference)")
        } else {
            crate::locale::text(
                "由 RDP 客户端提示凭据",
                "Credentials prompted by RDP client",
            )
        }
        .to_string(),
        RemoteProtocol::Telnet => {
            crate::locale::text("终端内交互式认证", "Interactive terminal authentication")
                .to_string()
        }
        RemoteProtocol::Serial => {
            crate::locale::text("本地设备连接", "Local device connection").to_string()
        }
    };
    let mut card = div()
        .w_full()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .rounded_md()
        .bg(theme.panel)
        .child(
            div()
                .text_size(px(14.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(crate::locale::text("认证", "Authentication")),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme.muted)
                .child(authentication),
        )
        .child(if host.route.jump_hosts.is_empty() {
            crate::locale::text("路由：直连", "Route: direct").to_string()
        } else {
            crate::locale::text(
                format!("路由：{} 个 SSH 跳板", host.route.jump_hosts.len()),
                format!("Route: {} SSH jump hosts", host.route.jump_hosts.len()),
            )
            .to_string()
        });
    if host.protocol == RemoteProtocol::Rdp && !cfg!(target_os = "windows") {
        card = card.child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "此平台由外部 RDP 客户端处理认证；保存的密码不会自动填入客户端。",
                    "The external RDP client handles authentication on this platform; saved passwords are not entered automatically.",
                )),
        );
    }
    card
}

/// 对应协议使用稳定中文名，列表和详情保持一致的识别文案。
fn protocol_label(protocol: RemoteProtocol) -> &'static str {
    match protocol {
        RemoteProtocol::Ssh => "SSH",
        RemoteProtocol::Sftp => "SFTP-only",
        RemoteProtocol::Telnet => "Telnet",
        RemoteProtocol::Serial => "Serial",
        RemoteProtocol::Rdp => "RDP",
    }
}

/// 把保存端点映射为只读展示字符串，串口详情使用 typed device path。
fn endpoint_label(host: &ManagedHost) -> String {
    match host.protocol {
        RemoteProtocol::Serial => host.serial.as_ref().map_or_else(
            || host.endpoint.host.clone(),
            |serial| serial.device.to_string_lossy().into_owned(),
        ),
        _ => format!(
            "{}@{}:{}",
            host.endpoint.username, host.endpoint.host, host.endpoint.port
        ),
    }
}

/// 无主机选中时提供明确信息，而不是隐藏或显示未接线的占位按钮。
fn empty_selection(theme: ThemeTokens) -> impl IntoElement {
    div()
        .id("connections-empty-state")
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .bg(theme.background)
        .child(
            div()
                .text_size(px(17.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(crate::locale::text("选择一台主机", "Select a host")),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "新建连接后可在左侧分组中查找和管理。",
                    "Create a connection, then find and manage it in the groups on the left.",
                )),
        )
}

/// 仅展示应用确认的状态或错误；无本地状态时留空，避免与 Root 全局状态栏重复显示“就绪”。
fn render_status(status: Option<String>, theme: ThemeTokens) -> impl IntoElement {
    match status {
        Some(status) => div()
            .w_full()
            .min_h(px(30.))
            .px_4()
            .py_1()
            .bg(theme.panel)
            .text_size(theme.text_xs)
            .text_color(theme.muted)
            .child(status),
        None => div().h(px(0.)).overflow_hidden(),
    }
}
