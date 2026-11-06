//! 连接导航、主机摘要和可见操作的 GPUI 绘制。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, ClickEvent, Context, Focusable, InteractiveElement as _, IntoElement, Render,
    StatefulInteractiveElement as _, Window,
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
use kerminal_domain::preferences::{ConfigurationMode, TerminalProfile};

use crate::theme::ThemeTokens;
use crate::ui_button::icon_button;

use super::{
    external::{render_external_password_prompt, render_external_username_prompt},
    form_render::render_editor,
    panel::ConnectionsPanel,
    trust::render_trust_confirmation,
};

impl Render for ConnectionsPanel {
    /// 编辑时表单占用独立工作区；普通模式不绘制远端主机详情页，目录选择留在左侧。
    /// 无编辑器时的未知本地主机写入提示挂在面板层，避免切换选择后丢失唯一核对入口。
    /// 从 Kit 全局主题取色，确保编辑页与左侧导航保持相同的浅深主题语义。
    /// 编辑器内分组作为最上层小弹层呈现，原表单实体和草稿持续保留，Tab 不越过当前弹层。
    /// 主机与分组的删除确认同样挂在面板最上层，以居中弹层出现，不参与内容流排版。
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.start_configuration_watch(cx);
        self.refresh_clean_editor(window, cx);
        let theme = ThemeTokens::current(window, cx);
        let readonly = self.recovery.mode == ConfigurationMode::ReadOnlyRecovery;
        let editing = self.editor_open();
        let editing_group = self.presentation.shows_group_editor();
        let selected_host = self.selected_host_snapshot();
        let content = if editing {
            render_editor(self, theme, window, cx).into_any_element()
        } else if editing_group {
            render_group_rename_editor(self, readonly, theme, cx).into_any_element()
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
                panel.handle_confirmation_escape(event, window, cx);
                panel.handle_editor_save_key(event, window, cx);
            }))
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme.background)
            .text_color(theme.foreground);
        panel = panel.child(body);
        if editing && editing_group {
            let focus_handle = self.group_name_input.read(cx).focus_handle(cx);
            panel = panel.child(
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
            && (self.local_hosts.save_outcome_unknown || self.local_hosts.cleanup_pending)
            && self.local_hosts.pending_delete.is_none()
        {
            panel = panel.child(render_local_host_unknown_banner(self, theme, cx));
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

        // 删除确认统一挂在面板最上层的居中弹层里，确认卡不再参与内容流排版，
        // 避免在主机空状态下把整块工作区拉成横跨窗口的大弹框。
        if let Some(pending_deletion) = self.pending_host_deletion.clone()
            && let Some(host) = selected_host
                .as_ref()
                .filter(|host| host.id == pending_deletion.host_id)
        {
            panel = panel.child(render_host_deletion_confirmation(self, host, theme, cx));
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

        panel
            .children(render_trust_confirmation(
                self.pending_trust.as_ref(),
                self.recovery.mode,
                theme,
                window,
                cx,
            ))
            .children(render_external_password_prompt(self, theme, cx))
            .children(render_external_username_prompt(self, theme, cx))
    }
}

/// 本地主机详情只提供与主机有关的操作；默认启动模板由 Profile 设置页管理。
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
        && !panel.local_hosts.save_outcome_unknown
        && !panel.local_hosts.cleanup_pending;
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
    let edit_profile_id = profile_id.clone();
    let duplicate_profile_id = profile_id.clone();
    let delete_profile_id = profile_id;
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
                .child(if panel.local_hosts.cleanup_pending {
                    crate::locale::text(
                        "本地主机更改已完成；部分旧凭据尚待清理，请重试清理。",
                        "The local host change is complete; some old credentials still need cleanup. Retry cleanup.",
                    )
                } else {
                    crate::locale::text(
                        "本地主机更改结果未知；不会自动重放，请先刷新并核对实际配置。",
                        "A local host change has an unknown result. It will not be replayed; read back and review the stored configuration first.",
                    )
                }),
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

/// 删除确认的共用弹层外壳：居中、限定宽度、超出可滚动。
/// 确认卡脱离面板内容流，避免空状态工作区被整块撑成横跨窗口的弹框。
/// 两个 id 均为静态字符串，绘制路径不再为每帧拼装 ElementId。
/// 卡片提供稳定身份后才能持有滚动状态，避免无状态 Div 无法调用滚动 API。
/// 点击遮罩等同取消；卡片内部吞掉点击，避免操作正文时误触关闭。
///
/// @author kongweiguang
fn deletion_confirmation_overlay(
    id: &'static str,
    card_debug_id: &'static str,
    theme: ThemeTokens,
    card: AnyElement,
    dismiss: Option<impl Fn(&ClickEvent, &mut Window, &mut gpui_kit::App) + 'static>,
) -> AnyElement {
    div()
        .id(id)
        .debug_selector(move || format!("{id}-bounds"))
        .absolute()
        .inset_0()
        .occlude()
        .p(theme.metrics.panel_padding)
        .flex()
        .items_center()
        .justify_center()
        .bg(theme.dialog_scrim)
        .when_some(dismiss, |overlay, dismiss| overlay.on_click(dismiss))
        .child(
            div()
                .id(card_debug_id)
                .debug_selector(move || card_debug_id.to_owned())
                .w(rems(30.0))
                .max_w_full()
                .max_h_full()
                .overflow_y_scroll()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .bg(theme.panel)
                .text_color(theme.foreground)
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(card),
        )
        .into_any_element()
}

/// 删除分组只确认一次；组内主机由后端在同一事务中统一归入未分组，界面不再提供迁移目标。
/// @author kongweiguang
fn render_group_deletion_confirmation(
    panel: &mut ConnectionsPanel,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> AnyElement {
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
    let consequence = if affected == 0 {
        crate::locale::text("分组内没有主机。", "This group has no hosts.").to_string()
    } else {
        crate::locale::text(
            format!("组内 {affected} 台主机将归入未分组。"),
            format!("{affected} hosts in this group will become ungrouped."),
        )
        .to_string()
    };
    let card = div()
        .id("connections-delete-group-confirmation")
        .w_full()
        .p(theme.metrics.dialog_padding)
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .text_size(theme.text_body)
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(crate::locale::text(
                    format!("删除分组“{group_name}”？"),
                    format!("Delete group “{group_name}”?"),
                )),
        )
        .child(
            div()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(consequence),
        )
        .child(
            crate::ui_surface::dialog_actions(theme)
                .w_full()
                .child(
                    Button::new("connections-delete-group-cancel")
                        .compact()
                        .ghost()
                        .label(crate::locale::text("取消", "Cancel"))
                        .disabled(panel.configuration_mutation_busy)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.cancel_group_deletion(cx)
                        })),
                )
                .child(
                    Button::new("connections-delete-group-confirm")
                        .compact()
                        .danger()
                        .label(crate::locale::text("删除", "Delete"))
                        .disabled(panel.configuration_mutation_busy)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.confirm_group_deletion(cx)
                        })),
                ),
        );
    deletion_confirmation_overlay(
        "connections-delete-group-overlay",
        "rr-connections-delete-group-card",
        theme,
        card.into_any_element(),
        Some(cx.listener(|panel, _: &ClickEvent, _, cx| panel.cancel_group_deletion(cx))),
    )
}

/// 删除主机前确认精确名称和配置后果；凭据是否可清理由回执单独说明。
/// @author kongweiguang
fn render_host_deletion_confirmation(
    panel: &mut ConnectionsPanel,
    host: &ManagedHost,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> AnyElement {
    let card = div()
        .id("connections-delete-host-confirmation")
        .w_full()
        .p(theme.metrics.dialog_padding)
        .flex()
        .flex_col()
        .gap_3()
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
            crate::ui_surface::dialog_actions(theme)
                .w_full()
                .child(
                    Button::new("connections-delete-host-cancel")
                        .compact()
                        .ghost()
                        .label(crate::locale::text("取消", "Cancel"))
                        .disabled(panel.configuration_mutation_busy)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.cancel_host_deletion(cx)
                        })),
                )
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
                ),
        );
    deletion_confirmation_overlay(
        "connections-delete-host-overlay",
        "rr-connections-delete-host-card",
        theme,
        card.into_any_element(),
        Some(cx.listener(|panel, _: &ClickEvent, _, cx| panel.cancel_host_deletion(cx))),
    )
}

/// 删除确认绑定同一 versioned 快照，按钮门禁跟随真实写入状态，未知回执只允许读回。
/// @author kongweiguang
fn render_local_host_deletion_confirmation(
    panel: &mut ConnectionsPanel,
    pending: &VersionedProfile,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> AnyElement {
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
    let card = div()
        .id("connections-delete-local-host-confirmation")
        .w_full()
        .p(theme.metrics.dialog_padding)
        .flex()
        .flex_col()
        .gap_3()
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

    let mut actions = crate::ui_surface::dialog_actions(theme).w_full().child(
        Button::new("connections-delete-local-host-cancel")
            .compact()
            .ghost()
            .label(crate::locale::text("取消", "Cancel"))
            .disabled(saving || outcome_unknown)
            .on_click(
                cx.listener(|panel, _: &ClickEvent, _, cx| panel.cancel_delete_local_host(cx)),
            ),
    );
    if outcome_unknown {
        actions = actions.child(
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
        actions = actions.child(
            Button::new("connections-delete-local-host-confirm")
                .compact()
                .danger()
                .label(if saving {
                    crate::locale::text("正在删除…", "Deleting…")
                } else {
                    crate::locale::text("确认删除配置", "Delete configuration")
                })
                .disabled(
                    readonly || panel.local_hosts.loading || panel.local_hosts.mutation_blocked(),
                )
                .on_click(
                    cx.listener(|panel, _: &ClickEvent, _, cx| panel.confirm_delete_local_host(cx)),
                ),
        );
    }

    deletion_confirmation_overlay(
        "connections-delete-local-host-overlay",
        "rr-connections-delete-local-host-card",
        theme,
        card.child(actions).into_any_element(),
        Some(cx.listener(|panel, _: &ClickEvent, _, cx| panel.cancel_delete_local_host(cx))),
    )
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
