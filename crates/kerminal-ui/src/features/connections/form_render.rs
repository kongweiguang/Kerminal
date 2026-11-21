//! 主机创建和编辑表单的原生绘制。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, Focusable as _, IntoElement, Window,
    component::{
        Disableable, Icon, IconName, Sizable as _,
        button::{Button, ButtonVariants},
        checkbox::Checkbox,
        input::{Input, InputContentType, Textarea},
        menu::{DropdownMenu as _, PopupMenuItem},
        popover::Popover,
        radio::{Radio, RadioGroup},
        scroll::ScrollableElement as _,
        select::Select,
        spinner::Spinner,
    },
    div,
    prelude::*,
    px, rems,
};
use kerminal_application::connections::management::HostAuthenticationMode;
use kerminal_domain::connections::RemoteProtocol;

use crate::theme::ThemeTokens;
use gpui_kit::base::{FocusTrapElement as _, Selectable};

use super::{
    form::{
        HostEditor, HostEditorField, HostEditorSection, JumpInputs, PrivateKeyInputMode,
        clear_input,
    },
    form_layout::{
        EditorField, EditorFieldScope, FORM_FIELD_LABEL_WIDTH_REM, compact_editor,
        form_field_row_max_width_rems,
    },
    local_hosts::{LocalHostEditorMode, LocalHostEditorSection},
    panel::{ConnectionsEditorKind, ConnectionsPanel, HostFilePickerTarget},
};

const ENDPOINT_PORT_LABEL_RESERVE_PX: f32 = 92.0;
const FORM_FIELD_GAP_PX: f32 = 12.0;
const ENDPOINT_PORT_INPUT_MIN_WIDTH_PX: f32 = 80.0;
const NETWORK_FIELD_LABEL_WIDTH_PX: f32 = FORM_FIELD_LABEL_WIDTH_REM * 16.0;
const NETWORK_FIELD_CONTROL_MAX_WIDTH_PX: f32 = 192.0;
const EDITOR_ACTION_WIDTH_REM: f32 = 9.0;

/// 主要底栏动作共享固定宽高，状态文案变化不会推动相邻按钮。
/// @author kongweiguang
fn editor_action_button(button: Button, theme: ThemeTokens) -> Button {
    button
        .compact()
        .w(rems(EDITOR_ACTION_WIDTH_REM))
        .h(theme.metrics.control_height)
        .flex_shrink_0()
}

/// 字段行尾操作使用统一的方形描边，并保留 Button 的原生焦点与禁用态。
/// @author kongweiguang
fn editor_icon_button(id: impl Into<gpui_kit::ElementId>, theme: ThemeTokens) -> Button {
    Button::new(id)
        .compact()
        .size(theme.metrics.control_height)
        .outline()
}

/// 侧栏图标占固定槽位，标签在两种编辑器中从同一基线开始。
/// @author kongweiguang
fn editor_navigation_content(icon: IconName, label: gpui_kit::SharedString) -> impl IntoElement {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .child(Icon::new(icon).size(px(16.)))
        .child(div().min_w_0().text_ellipsis().child(label))
}

/// 窄窗分类菜单沿用侧栏的图标与文字轴，只有展开箭头占据右端。
/// @author kongweiguang
fn editor_section_picker(
    id: &'static str,
    icon: IconName,
    label: gpui_kit::SharedString,
    disabled: bool,
) -> Button {
    Button::new(id)
        .debug_selector(move || id.to_owned())
        .w_full()
        .outline()
        .dropdown_caret(true)
        .accessibility_label(label.clone())
        .child(editor_navigation_content(icon, label))
        .disabled(disabled)
}

/// 端口列至少容纳内联端口标签、字段间距和可读的端口输入；地址列承担剩余伸缩。
/// @author kongweiguang
pub(super) fn endpoint_port_column_min_width_px() -> f32 {
    ENDPOINT_PORT_LABEL_RESERVE_PX + FORM_FIELD_GAP_PX + ENDPOINT_PORT_INPUT_MIN_WIDTH_PX
}

/// 网络数值行复用通用标签轴，单位与范围在输入下方显示；短数字控件保留可读上限。
/// @author kongweiguang
pub(super) fn network_field_widths_px() -> (f32, f32) {
    (
        NETWORK_FIELD_LABEL_WIDTH_PX,
        NETWORK_FIELD_CONTROL_MAX_WIDTH_PX,
    )
}

/// 按旧版编辑器的信息顺序固定协议栏、左侧分类和底部操作，让当前任务占据整张弹层。
/// 标题 X 与底部取消复用同一 owner 入口，不能绕过保存中阻断或 dirty/Unknown 确认。
/// @author kongweiguang
pub(super) fn render_editor(
    panel: &mut ConnectionsPanel,
    theme: ThemeTokens,
    window: &Window,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let short = window.viewport_size().height < window.rem_size() * 30.0;
    let compact = compact_editor(window) || short;
    if panel.plugin_forms.has_active() || panel.plugin_forms.is_loading() {
        return render_plugin_host_editor(panel, theme, short, cx);
    }
    match panel.active_editor_kind {
        Some(ConnectionsEditorKind::Local) => {
            if panel.local_hosts.editor.is_some() {
                render_local_host_editor(panel, theme, compact, short, cx)
            } else if panel.local_hosts.editor_load_intent.is_some() {
                render_local_host_editor_load(panel, theme, compact, short, cx)
            } else if panel.editor.is_some() {
                render_remote_host_editor(panel, theme, compact, short, cx).into_any_element()
            } else {
                div().into_any_element()
            }
        }
        Some(ConnectionsEditorKind::Remote) => {
            if panel.editor.is_some() {
                render_remote_host_editor(panel, theme, compact, short, cx).into_any_element()
            } else if panel.local_hosts.editor.is_some() {
                render_local_host_editor(panel, theme, compact, short, cx)
            } else if panel.local_hosts.editor_load_intent.is_some() {
                render_local_host_editor_load(panel, theme, compact, short, cx)
            } else {
                div().into_any_element()
            }
        }
        None => {
            if panel.editor.is_some() {
                render_remote_host_editor(panel, theme, compact, short, cx).into_any_element()
            } else if panel.local_hosts.editor.is_some() {
                render_local_host_editor(panel, theme, compact, short, cx)
            } else if panel.local_hosts.editor_load_intent.is_some() {
                render_local_host_editor_load(panel, theme, compact, short, cx)
            } else {
                div().into_any_element()
            }
        }
    }
}

/// 插件只提供稳定视图，读取、测试、保存及未知结果恢复仍由统一编辑器管理。
/// @author kongweiguang
fn render_plugin_host_editor(
    panel: &mut ConnectionsPanel,
    theme: ThemeTokens,
    short: bool,
    cx: &mut Context<ConnectionsPanel>,
) -> gpui_kit::AnyElement {
    let loading = panel.plugin_forms.is_loading();
    let load_error = panel.plugin_forms.loading_error().map(str::to_owned);
    let readonly =
        panel.recovery.mode == kerminal_domain::preferences::ConfigurationMode::ReadOnlyRecovery;
    let blocked = panel.plugin_forms.write_blocked();
    let busy = panel.plugin_forms.busy();
    let testing = panel.plugin_forms.testing();
    let unknown = panel.plugin_forms.unknown();
    let reading = panel.plugin_forms.reading();
    let mut page = div()
        .id("connections-plugin-editor-page")
        .flex_1()
        .min_h(px(0.))
        .min_w(px(0.))
        .overflow_y_scrollbar()
        .p_5();
    if loading {
        let message = load_error.clone().unwrap_or_else(|| {
            crate::locale::text("正在读取主机配置…", "Loading host configuration…").to_string()
        });
        page = page.child(status_strip(message, theme, load_error.is_some()));
        if load_error.is_some() {
            page = page.child(
                Button::new("connections-plugin-retry-load")
                    .compact()
                    .label(crate::locale::text("重试读取", "Retry loading"))
                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                        panel.retry_plugin_host_load(cx);
                    })),
            );
        }
    } else if let Some(view) = panel.plugin_forms.view() {
        page = page.child(view);
    }
    let mut footer = crate::ui_surface::dialog_actions(theme)
        .w_full()
        .min_h(theme.metrics.toolbar_height)
        .px(theme.metrics.panel_padding)
        .py(theme.metrics.gap / 2.)
        .border_t_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(
            div()
                .id("connections-plugin-status-lane")
                .flex_1()
                .min_w(px(0.))
                .h(theme.metrics.control_height)
                .flex()
                .items_center()
                .children(render_editor_message_lane(
                    panel.status.clone(),
                    false,
                    theme,
                )),
        )
        .child(
            editor_cancel_button("connections-plugin-editor-cancel", blocked, cx)
                .w(rems(10.0))
                .h(theme.metrics.control_height)
                .flex_shrink_0()
                .label(crate::locale::text("取消", "Cancel")),
        );
    if unknown {
        footer = footer.child(
            Button::new("connections-plugin-readback")
                .compact()
                .label(if reading {
                    crate::locale::text("核对中…", "Reading back…")
                } else {
                    crate::locale::text("读回核对", "Read back")
                })
                .disabled(reading)
                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                    panel.readback_plugin_form(cx);
                })),
        );
        if panel.plugin_forms.has_readback() {
            footer = footer.child(
                Button::new("connections-plugin-use-readback")
                    .compact()
                    .label(crate::locale::text(
                        "采用读回配置",
                        "Use stored configuration",
                    ))
                    .disabled(reading)
                    .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                        panel.use_plugin_readback(window, cx);
                    })),
            );
        }
    }
    if !loading && panel.plugin_forms.supports_test() {
        footer = footer.child(
            Button::new("connections-plugin-test")
                .compact()
                .w(rems(10.0))
                .h(theme.metrics.control_height)
                .label(if testing {
                    crate::locale::text("取消测试", "Cancel test")
                } else {
                    crate::locale::text("测试", "Test")
                })
                .disabled(readonly || (busy && !testing))
                .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                    panel.test_plugin_form(window, cx);
                })),
        );
    }
    if !loading {
        footer = footer.child(
            Button::new("connections-plugin-save")
                .debug_selector(|| "connections-plugin-save".to_owned())
                .compact()
                .w(rems(10.0))
                .h(theme.metrics.control_height)
                .label(if panel.saving {
                    crate::locale::text("保存中…", "Saving…")
                } else {
                    crate::locale::text("保存", "Save")
                })
                .disabled(readonly || busy)
                .when(!readonly && !busy, Button::primary)
                .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                    panel.save_plugin_form(window, cx);
                })),
        );
    }
    let mut form = div()
        .id("connections-plugin-host-editor")
        .size_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .bg(theme.background)
        .text_color(theme.foreground)
        .child(editor_header(
            if loading || panel.plugin_forms.active_is_existing() {
                crate::locale::text("编辑主机", "Edit host")
            } else {
                crate::locale::text("新建主机", "New host")
            },
            blocked,
            theme,
            short,
            cx,
        ));
    if !loading {
        form = form.child(
            div()
                .w_full()
                .flex_shrink_0()
                .px_5()
                .py_1()
                .flex()
                .items_center()
                .border_b_1()
                .border_color(theme.border)
                .child(protocol_picker(panel, None, blocked, theme, short, cx)),
        );
    }
    form = form.child(page).child(footer).relative();
    if panel.pending_editor_discard {
        form.child(render_discard_confirmation(panel, theme, cx))
            .into_any_element()
    } else {
        form.into_any_element()
    }
}

/// 状态反馈位于固定操作栏，统一动作尺寸并避免测试或校验结果改变字段起点。
/// @author kongweiguang
fn render_remote_host_editor(
    panel: &mut ConnectionsPanel,
    theme: ThemeTokens,
    compact: bool,
    short: bool,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let Some(editor) = panel.editor.as_ref() else {
        return div().into_any_element();
    };
    let readonly =
        panel.recovery.mode == kerminal_domain::preferences::ConfigurationMode::ReadOnlyRecovery;
    let saving = panel.saving;
    let reloading = panel.conflicted_host_editor_reload_in_progress();
    let disabled = panel.host_editor_fields_disabled();
    let is_new = editor.original.is_none();
    let protocol = editor.protocol;
    let active_section = editor.section;
    let validation_summary = editor
        .validation_issue
        .as_ref()
        .filter(|issue| {
            issue.section != HostEditorSection::Properties && issue.section != active_section
        })
        .map(|issue| issue.message.clone());
    let footer_message = panel.status.clone().or(validation_summary);
    let show_probe_status = footer_message.is_none();
    let mut editor_content = div()
        .id("connections-remote-editor-content")
        .w_full()
        .h_full()
        .flex()
        .flex_1()
        .items_stretch()
        .when(compact, |content| content.flex_col())
        .min_h(px(0.))
        .min_w(px(0.))
        .overflow_hidden()
        .child(render_editor_navigation(
            editor,
            active_section,
            disabled,
            theme,
            compact,
            cx,
        ));
    let mut page = div()
        .id("connections-remote-editor-page")
        .debug_selector(|| "connections-editor-viewport".into())
        .flex_1()
        .min_h(px(0.))
        .min_w(px(0.))
        .overflow_y_scroll()
        .track_scroll(&editor.scroll_handle)
        .vertical_scrollbar(&editor.scroll_handle)
        .flex()
        .flex_col()
        .gap_3();
    page = page.p_5();
    page = page.child(render_editor_section(
        panel,
        editor,
        active_section,
        disabled,
        theme,
        compact,
        cx,
    ));
    if let Some(readback) = render_host_save_readback_summary(panel, theme) {
        page = page.child(readback);
    }
    editor_content = editor_content.child(page);
    let content = div()
        .id("connections-editor-section-scroll")
        .flex_1()
        .min_h(px(0.))
        .min_w(px(0.))
        .overflow_hidden()
        .flex()
        .justify_center()
        .items_stretch()
        .child(editor_content);

    let footer = crate::ui_surface::dialog_actions(theme)
        .w_full()
        .min_h(theme.metrics.toolbar_height)
        .px(theme.metrics.panel_padding)
        .py(theme.metrics.gap / 2.)
        .border_t_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(
            div()
                .id("connections-editor-status-lane")
                .debug_selector(|| "connections-editor-status-lane".to_owned())
                .flex_1()
                .min_w(px(0.))
                .h(theme.metrics.control_height)
                .flex()
                .items_center()
                .children(render_editor_message_lane(
                    footer_message,
                    panel
                        .editor
                        .as_ref()
                        .is_some_and(|editor| editor.validation_issue.is_some()),
                    theme,
                ))
                .children(
                    show_probe_status
                        .then(|| render_probe_status(panel, theme))
                        .flatten(),
                ),
        )
        .child(
            editor_action_button(
                editor_cancel_button("connections-editor-footer-cancel", saving || reloading, cx),
                theme,
            )
            .outline()
            .label(crate::locale::text("取消", "Cancel")),
        )
        .when(panel.save_outcome_unknown, |footer| {
            footer
                .child(
                    Button::new("connections-editor-review-unknown-save")
                        .compact()
                        .label(if panel.unknown_host_save_review_in_progress() {
                            crate::locale::text("核对中…", "Reading back…")
                        } else {
                            crate::locale::text("读回核对", "Read back")
                        })
                        .disabled(saving || panel.unknown_host_save_review_in_progress())
                        .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                            panel.review_unknown_host_save(window, cx);
                        })),
                )
                .when(panel.unknown_host_save_readback_available(), |footer| {
                    footer.child(
                        Button::new("connections-editor-use-save-readback")
                            .compact()
                            .label(crate::locale::text(
                                "采用读回配置",
                                "Use stored configuration",
                            ))
                            .disabled(saving || panel.unknown_host_save_review_in_progress())
                            .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                                panel.use_host_save_readback(window, cx);
                            })),
                    )
                })
        })
        .children(render_conflict_reload_action(panel, cx))
        .children(render_probe_action(
            panel,
            saving || reloading,
            readonly,
            theme,
            cx,
        ))
        .child(
            editor_action_button(Button::new("connections-editor-save"), theme)
                .debug_selector(|| "connections-editor-save".to_owned())
                .label(if saving {
                    crate::locale::text("保存中…", "Saving…")
                } else if panel.save_outcome_unknown {
                    crate::locale::text("结果待核对", "Verify save result")
                } else if panel.is_sftp_host_creation_flow() {
                    crate::locale::text("保存并继续", "Save and continue")
                } else {
                    crate::locale::text("保存", "Save")
                })
                .disabled(
                    readonly
                        || saving
                        || reloading
                        || panel.probe_running
                        || panel.save_outcome_unknown,
                )
                .when(
                    !readonly
                        && !saving
                        && !reloading
                        && !panel.probe_running
                        && !panel.save_outcome_unknown,
                    Button::primary,
                )
                .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                    panel.save_editor(window, cx);
                })),
        );

    let form = div()
        .id("connections-host-editor")
        .size_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .bg(theme.background)
        .text_color(theme.foreground)
        .child(editor_header(
            if is_new {
                crate::locale::text("新建主机", "New host")
            } else {
                crate::locale::text("编辑主机", "Edit host")
            },
            saving || reloading,
            theme,
            short,
            cx,
        ))
        .child(
            div()
                .w_full()
                .flex_shrink_0()
                .px_5()
                .py_1()
                .flex()
                .items_center()
                .border_b_1()
                .border_color(theme.border)
                .child(protocol_picker(
                    panel,
                    Some(protocol),
                    disabled,
                    theme,
                    short,
                    cx,
                )),
        )
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.))
                .overflow_hidden()
                .child(content),
        )
        .child(footer)
        .relative();
    if panel.pending_editor_discard {
        form.child(render_discard_confirmation(panel, theme, cx))
            .into_any_element()
    } else {
        form.into_any_element()
    }
}

/// 本地主机保留稳定底栏；Profile 读取期间只阻止提交，继续允许编辑和取消，不提交尚未投影的参数。
/// @author kongweiguang
fn render_local_host_editor(
    panel: &mut ConnectionsPanel,
    theme: ThemeTokens,
    compact: bool,
    short: bool,
    cx: &mut Context<ConnectionsPanel>,
) -> gpui_kit::AnyElement {
    let Some(editor) = panel.local_hosts.editor.as_ref() else {
        return render_local_host_editor_load(panel, theme, compact, short, cx);
    };
    let field_scope = EditorFieldScope::new(
        editor.scroll_handle.clone(),
        format!("local-{:?}-{:?}", editor.name.entity_id(), editor.section),
    );
    let readonly =
        panel.recovery.mode == kerminal_domain::preferences::ConfigurationMode::ReadOnlyRecovery;
    let saving = panel.local_hosts.saving;
    let reloading = panel.conflicted_host_editor_reload_in_progress();
    let disabled = readonly || saving || reloading || panel.local_hosts.save_outcome_unknown;
    let can_save = !disabled
        && !panel.local_hosts.mutation_blocked()
        && panel.local_hosts.default_target_request.is_none();
    let title = match editor.mode {
        LocalHostEditorMode::Edit => crate::locale::text("编辑主机", "Edit host"),
        LocalHostEditorMode::Create | LocalHostEditorMode::Duplicate => {
            crate::locale::text("新建主机", "New host")
        }
    };
    let mut page = div()
        .id("connections-local-editor-page")
        .debug_selector(|| "connections-local-editor-viewport".to_owned())
        .flex_1()
        .min_h(px(0.))
        .min_w(px(0.))
        .track_scroll(&editor.scroll_handle)
        .overflow_y_scroll()
        .vertical_scrollbar(&editor.scroll_handle)
        .p_5()
        .flex()
        .flex_col()
        .gap_3()
        .when(!compact, |page| {
            page.child(editor_section_heading(
                local_section_label(editor.section),
                theme,
            ))
        });
    let local_status = if panel.local_hosts.save_outcome_unknown {
        None
    } else if editor.external_conflict {
        Some(crate::locale::text(
            "本地主机已被外部修改；当前草稿已保留，请重新读取并核对。",
            "This local host changed externally. Your draft is preserved; read it back and review before saving.",
        ).to_string())
    } else {
        panel
            .local_hosts
            .status
            .clone()
            .or_else(|| panel.status.clone())
            .or_else(|| editor.validation_issue.clone())
    };
    if panel.local_hosts.save_outcome_unknown {
        page = page.child(render_local_host_unknown_recovery(
            panel,
            &editor.profile_id,
            &field_scope,
            theme,
            cx,
        ));
    }
    let mut properties = div()
        .id("connections-local-editor-properties")
        .track_focus(&panel.editor_fields_focus)
        .w_full()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap_3();
    if editor.section == LocalHostEditorSection::Properties {
        properties = properties
            .child(input_field(
                crate::locale::text("名称", "Name"),
                Input::new(&editor.name).disabled(disabled).w_full(),
                theme,
                &field_scope,
            ))
            .child(group_field(
                Select::new(&editor.group_select)
                    .w_full()
                    .h(theme.metrics.control_height)
                    .disabled(disabled)
                    .accessibility_label(crate::locale::text("主机分组", "Host group")),
                theme,
                disabled,
                &field_scope,
                cx,
            ))
            .child(field(
                "Profile",
                local_profile_picker(panel, disabled, theme, cx),
                theme,
                &field_scope,
            ))
            .child(path_field(
                crate::locale::text("可执行文件", "Executable"),
                Input::new(&editor.shell).disabled(disabled).w_full(),
                HostFilePickerTarget::LocalShell,
                disabled,
                theme,
                &field_scope,
                cx,
            ))
            .child(render_local_shell_picker(
                panel,
                editor.shell.read(cx).value().to_string(),
                editor.args.read(cx).value().to_string(),
                &field_scope,
                disabled,
                theme,
                cx,
            ))
            .child(path_field(
                crate::locale::text("起始目录", "Working directory"),
                Input::new(&editor.cwd).disabled(disabled).w_full(),
                HostFilePickerTarget::LocalWorkingDirectory,
                disabled,
                theme,
                &field_scope,
                cx,
            ));
    } else {
        properties = properties
            .child(multiline_field(
                crate::locale::text("启动参数", "Arguments"),
                Textarea::new(&editor.args)
                    .h(rems(7.0))
                    .disabled(disabled)
                    .w_full(),
                theme,
                &field_scope,
            ))
            .child(multiline_field(
                crate::locale::text("环境变量", "Environment"),
                Textarea::new(&editor.env)
                    .h(rems(9.0))
                    .disabled(disabled)
                    .w_full(),
                theme,
                &field_scope,
            ))
            .child(authentication_note(
                crate::locale::text(
                    "空值或多行项用双引号包裹，项内双引号写成两个。",
                    "Quote empty or multiline items; double quotes inside an item.",
                ),
                theme,
            ));
    }
    page = page.child(properties);

    let mut editor_content = div()
        .id("connections-local-editor-content")
        .w_full()
        .h_full()
        .flex()
        .flex_1()
        .items_stretch()
        .when(compact, |content| content.flex_col())
        .min_h(px(0.))
        .min_w(px(0.))
        .overflow_hidden()
        .child(local_host_editor_navigation(
            editor.section,
            disabled,
            theme,
            compact,
            cx,
        ));
    editor_content = editor_content.child(page);
    let content = div()
        .id("connections-local-editor-scroll")
        .flex_1()
        .min_h(px(0.))
        .min_w(px(0.))
        .overflow_hidden()
        .flex()
        .justify_center()
        .items_stretch()
        .child(editor_content);
    let footer = crate::ui_surface::dialog_actions(theme)
        .w_full()
        .min_h(theme.metrics.toolbar_height)
        .px(theme.metrics.panel_padding)
        .py(theme.metrics.gap / 2.)
        .border_t_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(
            div()
                .id("connections-local-editor-status-lane")
                .debug_selector(|| "connections-local-editor-status-lane".to_owned())
                .flex_1()
                .min_w(px(0.))
                .h(theme.metrics.control_height)
                .flex()
                .items_center()
                .children(render_editor_message_lane(
                    local_status,
                    editor.validation_issue.is_some() || editor.external_conflict,
                    theme,
                )),
        )
        .child(
            editor_action_button(
                editor_cancel_button("connections-local-editor-cancel", saving || reloading, cx),
                theme,
            )
            .outline()
            .label(crate::locale::text("取消", "Cancel")),
        )
        .children(render_conflict_reload_action(panel, cx))
        .child(
            editor_action_button(Button::new("connections-local-editor-save"), theme)
                .debug_selector(|| "connections-local-editor-save".to_owned())
                .label(if saving {
                    crate::locale::text("保存中…", "Saving…")
                } else if panel.local_hosts.save_outcome_unknown {
                    crate::locale::text("结果待核对", "Verify save result")
                } else {
                    crate::locale::text("保存", "Save")
                })
                .disabled(!can_save)
                .when(can_save, Button::primary)
                .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                    panel.save_local_host(window, cx);
                })),
        );
    let form = div()
        .id("connections-local-host-editor")
        .size_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .bg(theme.background)
        .text_color(theme.foreground)
        .child(editor_header(title, saving || reloading, theme, short, cx))
        .child(
            div()
                .w_full()
                .flex_shrink_0()
                .px_5()
                .py_1()
                .flex()
                .items_center()
                .border_b_1()
                .border_color(theme.border)
                .child(protocol_picker(panel, None, disabled, theme, short, cx)),
        )
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.))
                .overflow_hidden()
                .child(content),
        )
        .child(footer)
        .relative();
    if panel.pending_editor_discard {
        form.child(render_discard_confirmation(panel, theme, cx))
            .into_any_element()
    } else {
        form.into_any_element()
    }
}

/// 候选按程序与 argv 标识；菜单由弹性容器定宽，内容变化不挤动相邻操作。
/// @author kongweiguang
fn render_local_shell_picker(
    panel: &ConnectionsPanel,
    current_program: String,
    current_args: String,
    field_scope: &EditorFieldScope,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let candidates = panel.local_hosts.shell_candidates.clone();
    let loading = panel.local_hosts.shell_discovery_loading;
    let error = panel.local_hosts.shell_discovery_error.clone();
    let owner = cx.entity().clone();
    let candidate_options = candidates
        .iter()
        .map(|candidate| {
            (
                candidate.id().to_owned(),
                candidate.display_name().to_owned(),
                super::local_hosts::local_shell_candidate_matches(
                    candidate,
                    &current_program,
                    &current_args,
                ),
            )
        })
        .collect::<Vec<_>>();
    let selected_name = candidates
        .iter()
        .find(|candidate| {
            super::local_hosts::local_shell_candidate_matches(
                candidate,
                &current_program,
                &current_args,
            )
        })
        .map(|candidate| candidate.display_name().to_owned());
    let current_label = selected_name
        .map(gpui_kit::SharedString::from)
        .unwrap_or_else(|| crate::locale::text("选择检测到的 Shell", "Select detected shell"));
    let state_text = if let Some(error) = error {
        crate::locale::text(
            format!("Shell 扫描失败：{error}"),
            format!("Shell scan failed: {error}"),
        )
    } else if panel.local_hosts.shell_discovery_loaded && candidates.is_empty() && !loading {
        crate::locale::text(
            "未发现 Shell，可手动输入程序路径。",
            "No shells found; enter a program path manually.",
        )
    } else {
        crate::locale::text("", "")
    };
    let state_color = if panel.local_hosts.shell_discovery_error.is_some() {
        theme.error
    } else {
        theme.muted
    };
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(field(
            crate::locale::text("检测到的 Shell", "Detected shells"),
            div()
                .w_full()
                .flex()
                .min_w(px(0.))
                .items_center()
                .gap_2()
                .child(
                    div().flex_1().min_w_0().child(
                        Button::new("connections-local-shell-candidates")
                            .debug_selector(|| "connections-local-shell-candidates".to_owned())
                            .w_full()
                            .h(theme.metrics.control_height)
                            .compact()
                            .outline()
                            .dropdown_caret(true)
                            .label(current_label)
                            .disabled(disabled || candidates.is_empty())
                            .dropdown_menu(move |mut menu, _, _| {
                                for (id, label, selected) in &candidate_options {
                                    let id = id.clone();
                                    let label = label.clone();
                                    let owner = owner.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(label).checked(*selected).on_click(
                                            move |_, window, app| {
                                                owner.update(app, |panel, cx| {
                                                    panel.select_local_shell_candidate(
                                                        &id, window, cx,
                                                    );
                                                });
                                            },
                                        ),
                                    );
                                }
                                if candidates.is_empty() {
                                    menu = menu.item(
                                        PopupMenuItem::new(crate::locale::text(
                                            "暂无候选",
                                            "No candidates",
                                        ))
                                        .disabled(true),
                                    );
                                }
                                menu
                            }),
                    ),
                )
                .child(
                    editor_icon_button("connections-local-shell-refresh", theme)
                        .debug_selector(|| "connections-local-shell-refresh".to_owned())
                        .icon(if loading {
                            IconName::LoaderCircle
                        } else {
                            IconName::RefreshCw
                        })
                        .accessibility_label(if loading {
                            crate::locale::text("取消 Shell 扫描", "Cancel shell scan")
                        } else {
                            crate::locale::text("重新扫描 Shell", "Rescan shells")
                        })
                        .tooltip(if loading {
                            crate::locale::text("取消扫描", "Cancel scan")
                        } else {
                            crate::locale::text("重新扫描", "Rescan")
                        })
                        .disabled(disabled)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            if panel.local_hosts.shell_discovery_loading {
                                panel.cancel_local_shell_discovery();
                                cx.notify();
                            } else {
                                panel.refresh_local_shell_candidates(cx);
                            }
                        })),
                ),
            theme,
            field_scope,
        ))
        .when(!state_text.is_empty(), |content| {
            content.child(
                div()
                    .w_full()
                    .text_xs()
                    .text_color(state_color)
                    .child(state_text),
            )
        })
}

/// 未知保存只允许读取目标当前版本；只有稳定 ID 匹配的读回可显式接纳或据此重新比较。
/// 缺失或读取失败时保留原写入所有权，不提供直接重放入口；恢复动作按按钮单独绑定正文焦点行。
/// @author kongweiguang
fn render_local_host_unknown_recovery(
    panel: &ConnectionsPanel,
    profile_id: &str,
    scope: &EditorFieldScope,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> gpui_kit::AnyElement {
    let in_flight = panel.local_hosts.readback_in_progress
        || panel.local_hosts.saving
        || panel.local_hosts.delete_checking;
    let readback = panel
        .local_hosts
        .unknown_readback
        .as_ref()
        .filter(|snapshot| snapshot.profile.id == profile_id)
        .map(|snapshot| snapshot.profile.name.clone());
    let missing = panel.local_hosts.unknown_target_missing;
    let message: gpui_kit::SharedString = if in_flight {
        crate::locale::text(
            "正在读取本地主机当前状态；保存结果仍未确认。",
            "Reading the current local host state; the save outcome is still unresolved.",
        )
    } else if let Some(name) = readback.as_ref() {
        crate::locale::text(
            format!(
                "已读回目标主机“{name}”。这不是上次保存成功的回执；请选择采用当前版本或保留草稿后重新比较。"
            ),
            format!(
                "The target host “{name}” was read back. This is not proof the previous save succeeded; choose the current version or keep the draft for a fresh compare."
            ),
        )
    } else if missing {
        crate::locale::text(
            "读回列表中未找到待核对目标。保存结果仍未知；可明确保留草稿后再另行保存，原请求不会自动重放。",
            "The target was not found in the readback list. The outcome remains unknown; you may explicitly keep the draft for a separate save, but the original request will not be replayed.",
        )
    } else {
        let readback_status = panel.local_hosts.status.clone();
        readback_status.map_or_else(
            || {
                crate::locale::text(
                    "保存结果仍未知。先刷新并核对当前主机状态，再决定如何处理草稿。",
                    "The save outcome is still unknown. Read back the current host state before deciding what to do with the draft.",
                )
            },
            |status| {
                crate::locale::text(
                    format!("保存结果仍未知；读回尚未确认：{status}"),
                    format!("The save outcome remains unknown; readback is not confirmed: {status}"),
                )
            },
        )
    };

    let mut recovery = div()
        .id("connections-local-save-recovery")
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(status_strip(message.to_string(), theme, true));
    let mut actions = div().w_full().flex().flex_wrap().items_center().gap_2();
    if readback.is_some() {
        actions = actions
            .child(
                scope.wrap(
                    "unknown-use-readback",
                    Button::new("connections-local-use-save-readback")
                        .compact()
                        .label(crate::locale::text("采用当前版本", "Use current version"))
                        .disabled(in_flight)
                        .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                            panel.use_local_host_readback(window, cx);
                        })),
                ),
            )
            .child(
                scope.wrap(
                    "unknown-keep-save-draft",
                    Button::new("connections-local-keep-save-draft")
                        .compact()
                        .label(crate::locale::text("保留草稿并继续", "Keep draft and continue"))
                        .disabled(
                            in_flight
                                || panel.recovery.mode
                                    == kerminal_domain::preferences::ConfigurationMode::ReadOnlyRecovery,
                        )
                        .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                            panel.continue_local_host_with_draft(cx);
                        })),
                ),
            );
        recovery = recovery.child(actions);
    } else if missing {
        actions = actions.child(
            scope.wrap(
                "unknown-keep-missing-save-draft",
                Button::new("connections-local-keep-missing-save-draft")
                    .compact()
                    .label(crate::locale::text(
                        "保留草稿，重新比较保存",
                        "Keep draft for a fresh save",
                    ))
                    .disabled(
                        in_flight
                            || panel.recovery.mode
                                == kerminal_domain::preferences::ConfigurationMode::ReadOnlyRecovery,
                    )
                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                        panel.continue_local_host_with_draft(cx);
                    })),
            ),
        );
        recovery = recovery.child(actions);
    } else {
        actions = actions.child(
            scope.wrap(
                "unknown-review-save-result",
                Button::new("connections-local-review-save-result")
                    .compact()
                    .label(if in_flight {
                        crate::locale::text("正在核对…", "Checking…")
                    } else {
                        crate::locale::text("刷新并核对", "Refresh and verify")
                    })
                    .disabled(in_flight)
                    .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                        panel.review_local_host_unknown(cx);
                    })),
            ),
        );
        recovery = recovery.child(actions);
    }
    recovery.into_any_element()
}

/// 编辑或复制的数据正在按稳定 ID 读取时保留统一弹层，并提供取消或安全重试而不落空白表单。
/// @author kongweiguang
fn render_local_host_editor_load(
    panel: &mut ConnectionsPanel,
    theme: ThemeTokens,
    compact: bool,
    short: bool,
    cx: &mut Context<ConnectionsPanel>,
) -> gpui_kit::AnyElement {
    let Some(intent) = panel.local_hosts.editor_load_intent.as_ref() else {
        return div().into_any_element();
    };
    let pending = panel.local_hosts.editor_load_pending;
    let disabled = pending || panel.local_hosts.save_outcome_unknown;
    let title = if intent.mode == LocalHostEditorMode::Edit {
        crate::locale::text("编辑主机", "Edit host")
    } else {
        crate::locale::text("新建主机", "New host")
    };
    let message = if pending {
        crate::locale::text("正在读取主机配置…", "Loading host configuration…").to_string()
    } else {
        panel
            .local_hosts
            .editor_load_error
            .clone()
            .unwrap_or_else(|| {
                crate::locale::text(
                    "读取主机配置失败；原操作未执行。",
                    "The host configuration could not be loaded; no action was applied.",
                )
                .to_string()
            })
    };
    let mut page = div()
        .id("connections-local-editor-load-page")
        .flex_1()
        .min_h(px(0.))
        .min_w(px(0.))
        .overflow_y_scrollbar()
        .p_5()
        .flex()
        .flex_col()
        .gap_3()
        .child(status_strip(message, theme, !pending));
    if !pending {
        page = page.child(
            Button::new("connections-local-editor-retry-load")
                .compact()
                .label(crate::locale::text("重试读取", "Retry loading"))
                .disabled(disabled)
                .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                    panel.retry_local_host_editor_load(window, cx);
                })),
        );
    }
    let content = div()
        .id("connections-local-editor-load-content")
        .w_full()
        .h_full()
        .flex()
        .flex_1()
        .items_stretch()
        .when(compact, |content| content.flex_col())
        .min_h(px(0.))
        .min_w(px(0.))
        .overflow_hidden()
        .child(local_host_editor_navigation(
            LocalHostEditorSection::Properties,
            true,
            theme,
            compact,
            cx,
        ))
        .child(page);
    let footer = crate::ui_surface::dialog_actions(theme)
        .w_full()
        .min_h(theme.metrics.toolbar_height)
        .px(theme.metrics.panel_padding)
        .py(theme.metrics.gap / 2.)
        .border_t_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(
            editor_cancel_button("connections-local-editor-load-cancel", false, cx)
                .w(rems(10.0))
                .h(theme.metrics.control_height)
                .label(crate::locale::text("取消", "Cancel")),
        );
    div()
        .id("connections-local-editor-load")
        .size_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .bg(theme.background)
        .text_color(theme.foreground)
        .child(editor_header(title, false, theme, short, cx))
        .child(
            div()
                .w_full()
                .flex_shrink_0()
                .px_5()
                .py_1()
                .flex()
                .items_center()
                .border_b_1()
                .border_color(theme.border)
                .child(protocol_picker(panel, None, disabled, theme, short, cx)),
        )
        .child(content)
        .child(footer)
        .into_any_element()
}

/// 本地启动项保留独立分类，窄窗的标题入口与远端共用同一菜单样式。
/// @author kongweiguang
fn local_host_editor_navigation(
    selected: LocalHostEditorSection,
    disabled: bool,
    theme: ThemeTokens,
    compact: bool,
    cx: &mut Context<ConnectionsPanel>,
) -> gpui_kit::AnyElement {
    let sections = [
        LocalHostEditorSection::Properties,
        LocalHostEditorSection::Terminal,
    ];
    if compact {
        let panel_entity = cx.entity().clone();
        return div()
            .debug_selector(|| "connections-editor-section-heading".to_owned())
            .px_4()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                editor_section_picker(
                    "connections-local-editor-section-picker",
                    local_section_icon(selected),
                    local_section_label(selected),
                    disabled,
                )
                .dropdown_menu(move |mut menu, _, _| {
                    for section in sections {
                        let panel_entity = panel_entity.clone();
                        menu = menu.item(
                            PopupMenuItem::new(local_section_label(section))
                                .checked(section == selected)
                                .on_click(move |_, window, app| {
                                    panel_entity.update(app, |panel, cx| {
                                        panel.select_local_host_section(section, window, cx)
                                    });
                                }),
                        );
                    }
                    menu
                }),
            )
            .into_any_element();
    }
    div()
        .id("connections-local-editor-navigation")
        .w(rems(10.0))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .border_r_1()
        .border_color(theme.border)
        .bg(theme.sidebar)
        .children(sections.into_iter().map(|section| {
            let label = local_section_label(section);
            Button::new(format!("connections-local-editor-section-{section:?}"))
                .debug_selector(move || format!("connections-local-editor-section-{section:?}"))
                .w_full()
                .h(rems(2.5))
                .compact()
                .accessibility_label(label.clone())
                .child(editor_navigation_content(
                    local_section_icon(section),
                    label,
                ))
                .disabled(disabled)
                .ghost()
                .when(section == selected, |button| {
                    button
                        .secondary()
                        .selected(true)
                        .bg(theme.selected)
                        .text_color(theme.selected_foreground)
                })
                .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                    panel.select_local_host_section(section, window, cx)
                }))
        }))
        .into_any_element()
}

/// 分类标签集中管理，菜单与侧栏的可访问名称保持一致。
fn local_section_label(section: LocalHostEditorSection) -> gpui_kit::SharedString {
    match section {
        LocalHostEditorSection::Properties => crate::locale::text("常规", "General"),
        LocalHostEditorSection::Terminal => crate::locale::text("终端", "Terminal"),
    }
}

/// 同一分类在侧栏和紧凑菜单入口保持相同图标，便于缩放时辨认位置。
/// @author kongweiguang
fn local_section_icon(section: LocalHostEditorSection) -> IconName {
    match section {
        LocalHostEditorSection::Properties => IconName::Settings2,
        LocalHostEditorSection::Terminal => IconName::SquareTerminal,
    }
}

/// 分组创建复用已有 CAS，方形边框按钮与相邻输入等高且始终可辨认。
fn group_field(
    select: impl IntoElement,
    theme: ThemeTokens,
    disabled: bool,
    scope: &EditorFieldScope,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    field(
        crate::locale::text("分组", "Group"),
        div()
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .child(div().flex_1().min_w_0().child(select))
            .child(
                editor_icon_button("connections-editor-create-group", theme)
                    .debug_selector(|| "connections-editor-create-group".to_owned())
                    .icon(IconName::Plus)
                    .tooltip(crate::locale::text("新建分组…", "New group…"))
                    .accessibility_label(crate::locale::text("新建分组", "New group"))
                    .disabled(disabled)
                    .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                        panel.begin_create_group_for_editor(window, cx);
                    })),
            ),
        theme,
        scope,
    )
}

/// 手动路径始终可编辑；等高按钮提示原生选择器可选使用，不成为必需依赖。
/// @author kongweiguang
fn path_field(
    label: impl Into<gpui_kit::SharedString>,
    input: Input,
    target: HostFilePickerTarget,
    disabled: bool,
    theme: ThemeTokens,
    scope: &EditorFieldScope,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    field(
        label,
        div()
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(input.h(theme.metrics.control_height)),
            )
            .child(
                editor_icon_button(format!("host-path-picker-{target:?}"), theme)
                    .icon(IconName::FolderOpen)
                    .tooltip(crate::locale::text("选择路径…", "Choose path…"))
                    .accessibility_label(crate::locale::text("选择路径", "Choose path"))
                    .disabled(disabled)
                    .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                        panel.request_host_file_picker(target, cx);
                    })),
            ),
        theme,
        scope,
    )
}

/// 菜单占据字段剩余宽度并为编辑按钮保留固定空间，读取期间仍可改选。
///
/// @author kongweiguang
fn local_profile_picker(
    panel: &ConnectionsPanel,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let profiles = panel
        .local_hosts
        .templates
        .iter()
        .map(|row| (row.profile.id.clone(), row.profile.name.clone()))
        .collect::<Vec<_>>();
    let selected_profile = panel.selected_local_profile_id(cx);
    let selected_label = profiles
        .iter()
        .find(|(id, _)| Some(id.as_str()) == selected_profile.as_deref())
        .map(|(_, name)| gpui_kit::SharedString::from(name.clone()))
        .unwrap_or_else(|| {
            if profiles.is_empty() {
                crate::locale::text("没有已保存的 Profile", "No saved profiles")
            } else {
                crate::locale::text("选择 Profile", "Select profile")
            }
        });
    let has_profiles = !profiles.is_empty();
    let resolving = panel.local_hosts.default_target_request.is_some();
    let panel_entity = cx.entity().clone();
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div().flex_1().min_w_0().child(
                Button::new("connections-local-profile-picker")
                    .w_full()
                    .h(theme.metrics.control_height)
                    .outline()
                    .dropdown_caret(true)
                    .label(selected_label)
                    .when(resolving, |button| button.child(Spinner::new().small()))
                    .disabled(disabled || !has_profiles)
                    .dropdown_menu(move |mut menu, _, _| {
                        for (id, name) in &profiles {
                            let panel_entity = panel_entity.clone();
                            let id = id.clone();
                            menu = menu.item(
                                PopupMenuItem::new(name.clone())
                                    .checked(Some(id.as_str()) == selected_profile.as_deref())
                                    .on_click(move |_, _, app| {
                                        panel_entity.update(app, |panel, cx| {
                                            panel.use_local_profile(id.clone(), cx)
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            ),
        )
        .child(
            editor_icon_button("connections-local-profile-edit", theme)
                .icon(gpui_kit::assets::IconName::Pencil)
                .accessibility_label(crate::locale::text("编辑 Profile", "Edit profiles"))
                .tooltip(crate::locale::text("编辑 Profile", "Edit profiles"))
                .disabled(disabled)
                .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                    cx.emit(super::panel::ConnectionsEvent::OpenProfileSettings);
                })),
        )
}

/// Local 的大字段以标签顶对齐；每行一个启动参数/环境变量，避免逗号拆分造成不可逆歧义。
/// @author kongweiguang
fn multiline_field(
    label: impl Into<gpui_kit::SharedString>,
    input: impl IntoElement,
    theme: ThemeTokens,
    scope: &EditorFieldScope,
) -> impl IntoElement {
    let label = label.into();
    EditorField::new(label.clone(), input, theme, true)
        .reveal_in(scope.scroll(), scope.identity(label.as_ref()))
}

/// 标题栏沿用共享几何并允许自然增高；关闭仍受草稿门禁控制。
/// @author kongweiguang
fn editor_header(
    title: impl Into<gpui_kit::SharedString>,
    saving: bool,
    theme: ThemeTokens,
    short: bool,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let header = crate::ui_surface::panel_header(theme)
        .w_full()
        .min_h(if short {
            theme.metrics.toolbar_height
        } else {
            theme.metrics.toolbar_height + theme.metrics.gap
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .justify_center()
                .text_size(theme.text_body)
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(title.into()),
        );
    header.child(
        editor_cancel_button("connections-editor-close", saving, cx)
            .size(theme.metrics.control_height)
            .flex_shrink_0()
            .ghost()
            .icon(IconName::Close)
            .accessibility_label(crate::locale::text(
                "关闭连接编辑器",
                "Close connection editor",
            ))
            .tooltip(crate::locale::text(
                "关闭连接编辑器",
                "Close connection editor",
            )),
    )
}

/// 两个可见取消入口共享禁用和回调，脏稿、未知保存及 probe 清理由 cancel_editor 唯一裁决。
/// 这里只禁止正在提交的保存；不能套用字段的 readonly/probe 锁而意外封死原有取消出口。
/// @author kongweiguang
fn editor_cancel_button(
    id: &'static str,
    saving: bool,
    cx: &mut Context<ConnectionsPanel>,
) -> Button {
    Button::new(id)
        .debug_selector(move || id.to_owned())
        .compact()
        .disabled(saving)
        .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
            panel.cancel_editor(window, cx);
        }))
}

/// 仅在真实 probe 服务存在时绘制测试动作；取消、一次性凭据与普通重试各有明确入口。
/// @author kongweiguang
fn render_probe_action(
    panel: &ConnectionsPanel,
    saving: bool,
    readonly: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> Option<Button> {
    panel.probe_service.as_ref()?;
    let running = panel.probe_running;
    // 协议被禁用后只能查看或保存配置，不再发起探测；运行中的取消入口保持可用。
    let protocol_disabled = panel
        .editor
        .as_ref()
        .is_some_and(|editor| !panel.hosts.protocol_enabled(editor.protocol));
    Some(
        editor_action_button(Button::new("connections-editor-probe"), theme)
            .debug_selector(|| "connections-editor-probe".to_owned())
            .outline()
            .when(running, |button| {
                button.child(Spinner::new().with_size(px(14.)).color(theme.muted))
            })
            .label(if running {
                crate::locale::text("取消测试", "Cancel test")
            } else {
                crate::locale::text("测试连接", "Test connection")
            })
            .disabled(
                !running && (saving || readonly || protocol_disabled || panel.save_outcome_unknown),
            )
            .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                if panel.probe_running {
                    panel.cancel_connection_probe(cx);
                } else {
                    panel.test_connection(window, cx);
                }
            })),
    )
}

/// 没有其他分类时保留正文标题，避免窄窗出现只有一个选项的下拉菜单。
/// @author kongweiguang
fn render_editor_section(
    panel: &ConnectionsPanel,
    editor: &HostEditor,
    section: HostEditorSection,
    disabled: bool,
    theme: ThemeTokens,
    compact: bool,
    cx: &mut Context<ConnectionsPanel>,
) -> gpui_kit::AnyElement {
    let content = match section {
        HostEditorSection::Properties | HostEditorSection::Authentication => {
            render_properties(panel, editor, disabled, theme, cx).into_any_element()
        }
        HostEditorSection::Proxy => with_validation_issue(
            render_proxy_route(editor, disabled, theme, cx),
            editor,
            HostEditorField::Proxy,
            theme,
        ),
        HostEditorSection::JumpHosts => with_validation_issue(
            render_jump_hosts(editor, disabled, theme, cx),
            editor,
            HostEditorField::JumpHosts,
            theme,
        ),
        HostEditorSection::Terminal => {
            if editor.protocol == RemoteProtocol::Serial {
                with_validation_issue(
                    render_serial_line_options(editor, disabled, theme),
                    editor,
                    HostEditorField::SerialBaud,
                    theme,
                )
            } else {
                with_validation_issue(
                    render_ssh_terminal(editor, disabled, theme),
                    editor,
                    HostEditorField::Terminal,
                    theme,
                )
            }
        }
        HostEditorSection::Network => with_validation_issue(
            render_network_options(editor, disabled, theme),
            editor,
            HostEditorField::Network,
            theme,
        ),
        HostEditorSection::Transfer => with_validation_issue(
            render_transfer_options(editor, disabled, theme, cx),
            editor,
            HostEditorField::Transfer,
            theme,
        ),
        HostEditorSection::Display => with_validation_issue(
            render_rdp_display(editor, disabled, theme, cx),
            editor,
            HostEditorField::Display,
            theme,
        ),
    };
    div()
        .id("connections-editor-selected-section")
        .track_focus(&panel.editor_fields_focus)
        .w_full()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap_3()
        .when(
            !compact || editor.protocol == RemoteProtocol::Telnet,
            |section_content| {
                section_content.child(editor_section_heading(
                    protocol_section_label(editor.protocol, section),
                    theme,
                ))
            },
        )
        .child(content)
        .into_any_element()
}

/// 常规和高级项共用分类目录，切到紧凑菜单时保留同一标签、图标与选中项。
/// @author kongweiguang
fn render_editor_navigation(
    editor: &HostEditor,
    selected: HostEditorSection,
    disabled: bool,
    theme: ThemeTokens,
    compact: bool,
    cx: &mut Context<ConnectionsPanel>,
) -> gpui_kit::AnyElement {
    let protocol = editor.protocol;
    let sections = HostEditorSection::for_protocol(editor.protocol)
        .iter()
        .copied()
        .filter(|section| *section != HostEditorSection::Authentication)
        .collect::<Vec<_>>();
    if compact {
        if protocol == RemoteProtocol::Telnet {
            return div().into_any_element();
        }
        let panel_entity = cx.entity().clone();
        return div()
            .debug_selector(|| "connections-editor-section-heading".to_owned())
            .px_4()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                editor_section_picker(
                    "connections-editor-section-picker",
                    editor_section_icon(selected),
                    protocol_section_label(editor.protocol, selected),
                    disabled,
                )
                .dropdown_menu(move |mut menu, _, _| {
                    for section in sections.iter().copied() {
                        let panel_entity = panel_entity.clone();
                        menu = menu.item(
                            PopupMenuItem::new(protocol_section_label(protocol, section))
                                .checked(section == selected)
                                .on_click(move |_, window, app| {
                                    panel_entity.update(app, |panel, cx| {
                                        panel.select_editor_section(section, window, cx);
                                    });
                                }),
                        );
                    }
                    menu
                }),
            )
            .into_any_element();
    }
    div()
        .id("connections-editor-navigation")
        .w(rems(10.0))
        .max_h_full()
        .overflow_y_scrollbar()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .border_r_1()
        .border_color(theme.border)
        .bg(theme.sidebar)
        .children(sections.into_iter().map(|section| {
            let label = protocol_section_label(protocol, section);
            let (_, detail) = editor_section_label(section);
            let button = Button::new(format!("connections-editor-section-{section:?}"))
                .debug_selector(move || format!("connections-editor-section-{section:?}"))
                .w_full()
                .h(rems(2.5))
                .compact()
                .accessibility_label(label.clone())
                .child(editor_navigation_content(
                    editor_section_icon(section),
                    label,
                ))
                .tooltip(detail)
                .disabled(disabled);
            let button = if section == selected {
                button
                    .secondary()
                    .selected(true)
                    .bg(theme.selected)
                    .text_color(theme.selected_foreground)
            } else {
                button.ghost()
            };
            button.on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                panel.select_editor_section(section, window, cx);
            }))
        }))
        .into_any_element()
}

/// 串口参数独立命名，分类仍复用原有状态枚举以保持校验定位与焦点路由一致。
fn protocol_section_label(
    protocol: RemoteProtocol,
    section: HostEditorSection,
) -> gpui_kit::SharedString {
    if section == HostEditorSection::Properties {
        crate::locale::text("常规", "General")
    } else if protocol == RemoteProtocol::Serial && section == HostEditorSection::Terminal {
        crate::locale::text("串口线制", "Serial line")
    } else {
        editor_section_label(section).0
    }
}

/// 每个表单分区沿用 GPUI Kit 图标语义，避免协议设置页依赖颜色或悬浮提示辨认。
/// @author kongweiguang
fn editor_section_icon(section: HostEditorSection) -> IconName {
    match section {
        HostEditorSection::Properties | HostEditorSection::Authentication => IconName::Settings2,
        HostEditorSection::Proxy => IconName::Network,
        HostEditorSection::JumpHosts => IconName::Building2,
        HostEditorSection::Terminal => IconName::SquareTerminal,
        HostEditorSection::Network => IconName::Network,
        HostEditorSection::Transfer => IconName::FolderOpen,
        HostEditorSection::Display => IconName::Frame,
    }
}

/// 常规页统一字段对齐，SSH 认证内部按行预留校验位，避免外层重复预留。
/// @author kongweiguang
fn render_properties(
    panel: &ConnectionsPanel,
    editor: &HostEditor,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let field_scope = host_editor_scope(editor, "properties");
    let mut fields = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_3()
        .child(with_validation_issue(
            input_field(
                crate::locale::text("名称", "Name"),
                Input::new(&editor.inputs.name)
                    .id("connections-editor-name-input")
                    .disabled(disabled)
                    .w_full(),
                theme,
                &field_scope,
            ),
            editor,
            HostEditorField::Name,
            theme,
        ))
        .child(with_validation_issue(
            group_field(
                Select::new(&editor.group_select)
                    .w_full()
                    .h(theme.metrics.control_height)
                    .disabled(disabled)
                    .accessibility_label(crate::locale::text("主机分组", "Host group")),
                theme,
                disabled,
                &field_scope,
                cx,
            ),
            editor,
            HostEditorField::Group,
            theme,
        ))
        .child(render_endpoint(editor, disabled, theme));
    let has_target = editor.has_target_credential();
    let has_passphrase = editor.has_target_passphrase();
    match editor.protocol {
        RemoteProtocol::Ssh | RemoteProtocol::Sftp => {
            fields = fields.child(render_ssh_auth(
                editor,
                has_target,
                has_passphrase,
                disabled,
                theme,
                cx,
            ));
        }
        RemoteProtocol::Rdp => {
            fields = fields.child(with_validation_issue(
                render_rdp_auth(editor, has_target, disabled, theme, cx),
                editor,
                HostEditorField::Authentication,
                theme,
            ));
        }
        RemoteProtocol::Telnet | RemoteProtocol::Serial => {}
    }
    if editor.protocol == RemoteProtocol::Serial {
        fields = fields.child(render_serial_connection(panel, editor, disabled, theme, cx));
    }
    fields = fields.child(with_validation_issue(
        input_field(
            crate::locale::text("标签", "Tags"),
            Input::new(&editor.inputs.tags).disabled(disabled).w_full(),
            theme,
            &field_scope,
        ),
        editor,
        HostEditorField::Tags,
        theme,
    ));
    if matches!(
        editor.protocol,
        RemoteProtocol::Ssh
            | RemoteProtocol::Sftp
            | RemoteProtocol::Rdp
            | RemoteProtocol::Telnet
            | RemoteProtocol::Serial
    ) {
        fields = fields.child(with_validation_issue(
            multiline_field(
                crate::locale::text("备注", "Notes"),
                Textarea::new(&editor.inputs.note)
                    .h(rems(7.0))
                    .disabled(disabled)
                    .w_full(),
                theme,
                &field_scope,
            ),
            editor,
            HostEditorField::Note,
            theme,
        ));
    }
    if editor.protocol == RemoteProtocol::Telnet {
        fields = fields.child(note_card(
            crate::locale::text(
                "Telnet 在终端内交互登录；此处不会保存用户名或密码。",
                "Telnet login is interactive in the terminal; this editor does not save a username or password.",
            ),
            theme,
        ));
    }
    fields
}

/// 菜单名称集中映射，供高级分区按钮和纯状态测试共享稳定标签。
/// @author kongweiguang
fn editor_section_label(
    section: HostEditorSection,
) -> (gpui_kit::SharedString, gpui_kit::SharedString) {
    match section {
        HostEditorSection::Properties => (
            crate::locale::text("常规", "General"),
            crate::locale::text(
                "连接信息与常用设置",
                "Connection details and common settings",
            ),
        ),
        HostEditorSection::Authentication => (
            crate::locale::text("认证", "Authentication"),
            crate::locale::text(
                "认证方式与凭据引用",
                "Authentication mode and credential refs",
            ),
        ),
        HostEditorSection::Proxy => (
            crate::locale::text("代理", "Proxy"),
            crate::locale::text("SSH/SFTP 代理路由", "SSH/SFTP proxy route"),
        ),
        HostEditorSection::JumpHosts => (
            crate::locale::text("跳板机", "Jump hosts"),
            crate::locale::text("有序 SSH 跳板链", "Ordered SSH jump chain"),
        ),
        HostEditorSection::Terminal => (
            crate::locale::text("终端", "Terminal"),
            crate::locale::text(
                "终端启动与协议参数",
                "Terminal startup and protocol options",
            ),
        ),
        HostEditorSection::Network => (
            crate::locale::text("网络", "Network"),
            crate::locale::text("连接超时与心跳", "Connection timeout and keepalive"),
        ),
        HostEditorSection::Transfer => (
            crate::locale::text("文件传输", "File transfer"),
            crate::locale::text(
                "起始目录与传输策略",
                "Initial directories and transfer policy",
            ),
        ),
        HostEditorSection::Display => (
            crate::locale::text("显示", "Display"),
            crate::locale::text("全屏与窗口尺寸", "Fullscreen and window size"),
        ),
    }
}

/// 底栏只显示一行真实测试结果；详情只展开测试消息，不再承载指纹确认。
/// @author kongweiguang
fn render_probe_status(
    panel: &ConnectionsPanel,
    theme: ThemeTokens,
) -> Option<gpui_kit::AnyElement> {
    let outcome = panel.probe_outcome.as_ref();
    let error = panel.probe_error.as_ref();
    let cancelled = panel.probe_cancelled;
    if outcome.is_none() && error.is_none() && !cancelled {
        return None;
    }
    let (message, warning) = if cancelled {
        (
            crate::locale::text("测试已取消。", "Test cancelled."),
            false,
        )
    } else {
        outcome.map_or_else(
            || {
                (
                    crate::locale::text(
                        "连接测试未完成。",
                        "The connection test did not complete.",
                    ),
                    true,
                )
            },
            probe_outcome_message,
        )
    };
    let summary = if cancelled {
        crate::locale::text("测试已取消", "Test cancelled")
    } else if error.is_some() {
        crate::locale::text("连接测试失败", "Connection test failed")
    } else {
        message.clone()
    };
    let detail: gpui_kit::SharedString = error
        .cloned()
        .map(Into::into)
        .unwrap_or_else(|| message.clone());
    let detail_color = if warning && !cancelled {
        theme.warning
    } else {
        theme.foreground
    };
    let popover = Popover::new("connections-editor-probe-details-popover")
        .auto_flip()
        .trigger(
            Button::new("connections-editor-probe-details")
                .compact()
                .size(theme.metrics.control_height)
                .ghost()
                .icon(IconName::Info)
                .accessibility_label(crate::locale::text(
                    "查看连接测试详情",
                    "View connection test details",
                ))
                .tooltip(crate::locale::text("查看详情", "View details")),
        )
        .content(move |_, _, _| {
            div()
                .w(rems(26.0))
                .max_w(rems(26.0))
                .max_h(rems(14.0))
                .overflow_y_scrollbar()
                .p_3()
                .flex()
                .flex_col()
                .gap_2()
                .text_size(theme.text_xs)
                .text_color(detail_color)
                .child(detail.clone())
        });
    Some(
        div()
            .flex_1()
            .min_w(px(0.))
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(theme.text_xs)
                    .text_color(if warning && !cancelled {
                        theme.warning
                    } else {
                        theme.muted
                    })
                    .child(summary),
            )
            .child(popover)
            .into_any_element(),
    )
}

/// 将本机 RDP 启动能力与远端 TCP 结果并列呈现，不把端口可达描述为 RDP 认证成功。
/// @author kongweiguang
fn probe_outcome_message(
    outcome: &kerminal_application::connections::probe::ConnectionProbeOutcome,
) -> (gpui_kit::SharedString, bool) {
    use kerminal_application::connections::probe::{
        ConnectionProbeOutcome, ProbeVerificationLevel, RdpLocalAvailability, RdpTcpFailure,
        RdpTcpReachability, SftpProbeStatus,
    };
    match outcome {
        ConnectionProbeOutcome::InvalidInput { field, .. } => {
            use kerminal_application::connections::probe::ProbeField;
            let (message, warning) = match field {
                ProbeField::SerialDevice => (
                    crate::locale::text("串口设备路径无效。", "Serial device path is invalid."),
                    true,
                ),
                ProbeField::SerialBaud => (
                    crate::locale::text("串口波特率无效。", "Serial baud rate is invalid."),
                    true,
                ),
            };
            (message, warning)
        }
        ConnectionProbeOutcome::SshAuthenticated { sftp } => match sftp {
            SftpProbeStatus::Available => (
                crate::locale::text(
                    "SSH 认证成功，SFTP 子系统可用。",
                    "SSH authentication succeeded; the SFTP subsystem is available.",
                ),
                false,
            ),
            SftpProbeStatus::Failed { message, .. } => (
                crate::locale::text(
                    format!("SSH 认证成功；SFTP 子系统不可用：{message}"),
                    format!("SSH authentication succeeded; SFTP is unavailable: {message}"),
                ),
                true,
            ),
        },
        ConnectionProbeOutcome::Verified(level) => (
            match level {
                ProbeVerificationLevel::SshAuthenticated => {
                    crate::locale::text("SSH 认证成功。", "SSH authentication succeeded.")
                }
                ProbeVerificationLevel::SftpSubsystemReady => {
                    crate::locale::text("SFTP 子系统已就绪。", "SFTP subsystem is ready.")
                }
                ProbeVerificationLevel::SftpDirectoryAccessible => crate::locale::text(
                    "SFTP 子系统及起始目录可访问。",
                    "The SFTP subsystem and initial directory are accessible.",
                ),
                ProbeVerificationLevel::TelnetTransportConnected => crate::locale::text(
                    "Telnet 传输已连接；尚未进行交互式登录。",
                    "Telnet transport connected; interactive login has not occurred.",
                ),
                ProbeVerificationLevel::SerialDeviceOpened => crate::locale::text(
                    "串口设备已成功打开并释放。",
                    "The serial device opened successfully and was released.",
                ),
            },
            false,
        ),
        ConnectionProbeOutcome::RdpConnectivity(result) => {
            let (client_zh, client_en, client_warning) = match result.local_availability {
                RdpLocalAvailability::ClientAvailable(client) => {
                    let name = match client {
                        kerminal_application::connections::probe::RdpClientKind::WindowsMstsc => {
                            ("Windows MSTSC", "Windows MSTSC")
                        }
                        kerminal_application::connections::probe::RdpClientKind::LinuxFreeRdp => {
                            ("Linux FreeRDP", "Linux FreeRDP")
                        }
                    };
                    (
                        format!("本机 RDP 客户端可用：{}。", name.0),
                        format!("Local RDP client available: {}.", name.1),
                        false,
                    )
                }
                RdpLocalAvailability::SystemLauncherAvailable {
                    association_verified: false,
                } => (
                    "本机系统启动器存在，RDP 应用关联未核验。".to_owned(),
                    "Local system launcher exists; RDP application association is unverified."
                        .to_owned(),
                    true,
                ),
                RdpLocalAvailability::SystemLauncherAvailable {
                    association_verified: true,
                } => (
                    "本机 RDP 系统启动器及应用关联可用。".to_owned(),
                    "Local RDP system launcher and application association are available."
                        .to_owned(),
                    false,
                ),
                RdpLocalAvailability::Unavailable => (
                    "本机没有可用的 RDP 客户端或系统启动器。".to_owned(),
                    "No local RDP client or system launcher is available.".to_owned(),
                    true,
                ),
            };
            let (remote_zh, remote_en, remote_warning) = match result.remote_tcp {
                RdpTcpReachability::Reachable => (
                    format!(
                        "远端 TCP {} 端口可达；尚未进行 RDP 协议握手或认证。",
                        result.remote_port
                    ),
                    format!(
                        "Remote TCP port {} is reachable; no RDP handshake or authentication was performed.",
                        result.remote_port
                    ),
                    false,
                ),
                RdpTcpReachability::Unreachable(reason) => {
                    let (reason_zh, reason_en) = match reason {
                        RdpTcpFailure::ConnectionRefused => ("连接被拒绝", "connection refused"),
                        RdpTcpFailure::TimedOut => ("连接超时", "connection timed out"),
                        RdpTcpFailure::NetworkError => ("网络连接失败", "network error"),
                    };
                    (
                        format!(
                            "远端 TCP {} 端口不可达（{}）；未进行 RDP 认证。",
                            result.remote_port, reason_zh
                        ),
                        format!(
                            "Remote TCP port {} is unreachable ({}); RDP authentication was not tested.",
                            result.remote_port, reason_en
                        ),
                        true,
                    )
                }
            };
            (
                crate::locale::text(
                    format!("{client_zh} {remote_zh}"),
                    format!("{client_en} {remote_en}"),
                ),
                client_warning || remote_warning,
            )
        }
    }
}

/// 冲突重读先交回 owner 的放弃确认，再按稳定 ID 读取；按钮不能用普通列表刷新替代恢复。
/// @author kongweiguang
fn render_conflict_reload_action(
    panel: &ConnectionsPanel,
    cx: &mut Context<ConnectionsPanel>,
) -> Option<impl IntoElement> {
    let loading = panel.conflicted_host_editor_reload_in_progress();
    let available = panel.can_reload_conflicted_host_editor();
    if !loading && !available {
        return None;
    }
    Some(
        Button::new("connections-editor-reload-conflict")
            .compact()
            .label(if loading {
                crate::locale::text("读取中…", "Reading…")
            } else {
                crate::locale::text("重新读取", "Reload")
            })
            .disabled(!available)
            .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                panel.reload_conflicted_host_editor(window, cx);
            })),
    )
}

/// 读回卡片只展示 owner 核对后的非秘密快照，避免把下方保留的原草稿误认为已存配置。
/// 摘要随正文滚动，未知结果期间的恢复动作始终留在固定底栏。
/// @author kongweiguang
fn render_host_save_readback_summary(
    panel: &ConnectionsPanel,
    theme: ThemeTokens,
) -> Option<impl IntoElement> {
    let snapshot = panel.unknown_host_save_readback_summary()?;
    let mut card = div()
        .id("connections-host-save-readback-summary")
        .w_full()
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .border_1()
        .border_color(theme.warning)
        .rounded_md()
        .bg(theme.panel)
        .text_size(theme.text_xs)
        .text_color(theme.foreground)
        .child(crate::locale::text(
            "当前已存配置",
            "Current stored configuration",
        ));
    if snapshot.exists {
        let protocol = match snapshot.protocol {
            Some(RemoteProtocol::Ssh) => "SSH",
            Some(RemoteProtocol::Sftp) => "SFTP",
            Some(RemoteProtocol::Rdp) => "RDP",
            Some(RemoteProtocol::Telnet) => "Telnet",
            Some(RemoteProtocol::Serial) => "Serial",
            None => "",
        };
        card = card
            .child(format!(
                "{} · {protocol}",
                snapshot.name.unwrap_or_default()
            ))
            .child(format!(
                "{}: {}    {}: {}",
                crate::locale::text("地址", "Address"),
                snapshot.host.unwrap_or_default(),
                crate::locale::text("端口", "Port"),
                snapshot
                    .port
                    .map(|port| port.to_string())
                    .unwrap_or_default(),
            ));
        if let Some(username) = snapshot.username.filter(|value| !value.is_empty()) {
            card = card.child(format!(
                "{}: {username}",
                crate::locale::text("用户名", "Username"),
            ));
        }
        if let Some(note) = snapshot.note.filter(|value| !value.is_empty()) {
            card = card.child(format!("{}: {note}", crate::locale::text("备注", "Note"),));
        }
        card = card.child(crate::locale::text(
            "采用后将关闭本次草稿；需要时可重新编辑已存主机。",
            "Using this configuration closes the current draft. Reopen the stored host to edit it.",
        ));
    } else {
        card = card.child(crate::locale::text(
            "该主机当前已不存在。采用读回结果后将关闭本次草稿。",
            "This host no longer exists. Using this result closes the current draft.",
        ));
    }
    Some(card)
}

/// 表单状态条使用语义色区分失败与普通进度，不把反馈解释成提交成功。
/// @author kongweiguang
fn status_strip(message: String, theme: ThemeTokens, error: bool) -> impl IntoElement {
    div()
        .w_full()
        .p_2()
        .rounded_sm()
        .bg(theme.panel)
        .text_size(theme.text_xs)
        .text_color(if error { theme.error } else { theme.muted })
        .child(message)
}

/// 普通状态只占固定单行；长文案通过可访问的详情入口查看，避免提示挤动表单布局。
/// @author kongweiguang
fn render_editor_message_lane(
    message: Option<String>,
    error: bool,
    theme: ThemeTokens,
) -> Option<gpui_kit::AnyElement> {
    let message = message?;
    let detail = message.clone();
    let color = if error { theme.error } else { theme.muted };
    let details = Popover::new("connections-editor-message-details-popover")
        .auto_flip()
        .trigger(
            Button::new("connections-editor-message-details")
                .compact()
                .size(theme.metrics.control_height)
                .ghost()
                .icon(IconName::Info)
                .accessibility_label(crate::locale::text("查看状态详情", "View status details"))
                .tooltip(crate::locale::text("查看详情", "View details")),
        )
        .content(move |_, _, _| {
            div()
                .w(rems(26.0))
                .max_w(rems(26.0))
                .max_h(rems(14.0))
                .overflow_y_scrollbar()
                .p_3()
                .text_size(theme.text_xs)
                .text_color(color)
                .child(detail.clone())
        });
    Some(
        div()
            .flex_1()
            .min_w(px(0.))
            .flex()
            .items_center()
            .gap_1()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(theme.text_xs)
                    .text_color(color)
                    .child(message),
            )
            .child(details)
            .into_any_element(),
    )
}

/// 脏表单关闭时要求明确选择继续编辑或放弃，遮罩关闭也复用同一确认状态。
/// @author kongweiguang
fn render_discard_confirmation(
    panel: &ConnectionsPanel,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let local_editor_active = panel.active_editor_kind == Some(ConnectionsEditorKind::Local);
    let local_outcome_unknown = local_editor_active && panel.local_hosts.save_outcome_unknown;
    let remote_outcome_unknown = !local_editor_active && panel.save_outcome_unknown;
    let outcome_unknown = local_outcome_unknown || remote_outcome_unknown;
    let title = if outcome_unknown {
        crate::locale::text("保存结果尚未确认", "Save outcome is not confirmed")
    } else {
        crate::locale::text("放弃未保存更改？", "Discard unsaved changes?")
    };
    let message = if local_outcome_unknown {
        crate::locale::text(
            "本地主机保存结果尚未确认。请返回核对读回状态；关闭或放弃草稿不能解除该写入封锁。",
            "The local host save outcome is unresolved. Return to the readback controls; closing or discarding the draft cannot clear the write guard.",
        )
    } else if remote_outcome_unknown {
        crate::locale::text(
            "关闭编辑器不会重试保存；请返回主机列表刷新并核对配置与凭据状态。",
            "Closing will not retry the save. Refresh the host list and verify configuration and credentials before retrying.",
        )
    } else if local_editor_active {
        crate::locale::text(
            "当前本地主机草稿尚未保存。",
            "This local host draft has not been saved.",
        )
    } else {
        crate::locale::text(
            "当前主机草稿尚未保存。",
            "This host draft has not been saved.",
        )
    };
    div()
        .id("connections-editor-discard-overlay")
        .absolute()
        .inset_0()
        .occlude()
        .p(theme.metrics.panel_padding)
        .flex()
        .items_center()
        .justify_center()
        .bg(theme.dialog_scrim)
        .child(
            div()
                .id("connections-editor-discard-card")
                .w_full()
                .max_w(rems(25.0))
                .max_h_full()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(theme.metrics.section_gap)
                .p(theme.metrics.dialog_padding)
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .bg(theme.panel)
                .text_color(theme.foreground)
                .child(section_title(title, theme))
                .child(message)
                .child({
                    let mut actions = crate::ui_surface::dialog_actions(theme).w_full().child(
                        Button::new("connections-editor-keep-editing")
                            .compact()
                            .label(if local_outcome_unknown {
                                crate::locale::text("返回核对", "Return to verification")
                            } else {
                                crate::locale::text("继续编辑", "Keep editing")
                            })
                            .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                                panel.keep_editor_open(window, cx);
                            })),
                    );
                    if !local_outcome_unknown {
                        actions = actions.child(
                            Button::new("connections-editor-discard")
                                .compact()
                                .label(if outcome_unknown {
                                    crate::locale::text(
                                        "关闭并保留核对提示",
                                        "Close and keep verification notice",
                                    )
                                } else {
                                    crate::locale::text("放弃更改", "Discard changes")
                                })
                                .danger()
                                .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                                    panel.confirm_discard_editor(window, cx);
                                })),
                        );
                    }
                    actions
                })
                .focus_trap("connections-editor-discard-focus", &panel.discard_focus),
        )
}

/// 协议候选只保留当前启用的内置类型，Local 不依赖插件启停。
///
/// 已锁定表单的当前协议即使刚被禁用也继续展示，避免热更新把用户正在编辑的
/// 宿主表单强制切换到其它协议；新建或可切换表单不保留禁用协议，防止继续产生
/// 无法连接的新配置。
///
/// @author kongweiguang
pub(super) fn protocol_options(
    mut enabled: impl FnMut(RemoteProtocol) -> bool,
    selected: Option<RemoteProtocol>,
) -> Vec<(Option<RemoteProtocol>, &'static str)> {
    [
        (Some(RemoteProtocol::Ssh), "SSH"),
        (Some(RemoteProtocol::Sftp), "SFTP"),
        (None, "Local"),
        (Some(RemoteProtocol::Rdp), "RDP"),
        (Some(RemoteProtocol::Telnet), "Telnet"),
        (Some(RemoteProtocol::Serial), "Serial"),
    ]
    .into_iter()
    .filter(|(protocol, _)| match protocol {
        Some(protocol) => selected == Some(*protocol) || enabled(*protocol),
        None => true,
    })
    .collect()
}

/// 常规窗口保留协议标签，极矮窗口收为选择器，为正文保留可滚动空间。
/// @author kongweiguang
fn protocol_picker(
    panel: &ConnectionsPanel,
    selected: Option<RemoteProtocol>,
    disabled: bool,
    theme: ThemeTokens,
    short: bool,
    cx: &mut Context<ConnectionsPanel>,
) -> gpui_kit::AnyElement {
    // 只有绑定已保存配置的编辑器才保留被禁用的当前协议；新建草稿仍从启用集合出发。
    let preserved = selected.filter(|_| panel.editor_protocol_locked());
    let options = protocol_options(|protocol| panel.hosts.protocol_enabled(protocol), preserved);
    // 下拉闭包需要在渲染后仍可独立执行；只捕获 Copy 的禁用协议身份，不持有面板引用。
    let preserved_disabled = preserved.filter(|protocol| !panel.hosts.protocol_enabled(*protocol));
    let plugin_options = panel.plugin_forms.options();
    let selected_plugin = panel.plugin_forms.active_type().map(str::to_owned);
    if short {
        let owner = cx.entity().clone();
        let label: gpui_kit::SharedString = selected_plugin
            .as_ref()
            .and_then(|active| plugin_options.iter().find(|(id, _)| id == active))
            .map(|(_, title)| title.clone())
            .unwrap_or_else(|| match selected {
                Some(RemoteProtocol::Ssh) => "SSH".into(),
                Some(RemoteProtocol::Sftp) => "SFTP".into(),
                Some(RemoteProtocol::Rdp) => "RDP".into(),
                Some(RemoteProtocol::Telnet) => "Telnet".into(),
                Some(RemoteProtocol::Serial) => "Serial".into(),
                None => "Local".into(),
            });
        return Button::new("connections-compact-protocol")
            .compact()
            .w_full()
            .secondary()
            .label(label)
            .dropdown_caret(true)
            .disabled(disabled || panel.editor_protocol_locked())
            .dropdown_menu(move |mut menu, _, _| {
                for (protocol, title) in &options {
                    let protocol = *protocol;
                    let title = *title;
                    let owner = owner.clone();
                    menu = menu.item(
                        PopupMenuItem::new(title)
                            .checked(selected_plugin.is_none() && protocol == selected)
                            .disabled(disabled || protocol == preserved_disabled)
                            .on_click(move |_, window, app| {
                                owner.update(app, |panel, cx| match protocol {
                                    Some(protocol) => {
                                        panel.activate_remote_protocol(protocol, window, cx)
                                    }
                                    None => panel.activate_local_host_editor(window, cx),
                                });
                            }),
                    );
                }
                for (type_id, title) in plugin_options.iter().cloned() {
                    let owner = owner.clone();
                    let checked = selected_plugin.as_deref() == Some(type_id.as_str());
                    menu = menu.item(PopupMenuItem::new(title).checked(checked).on_click(
                        move |_, window, app| {
                            owner.update(app, |panel, cx| {
                                panel.activate_plugin_form(&type_id, window, cx)
                            });
                        },
                    ));
                }
                menu
            })
            .into_any_element();
    }
    div()
        .w_full()
        .flex()
        .flex_wrap()
        .gap_1()
        .p_1()
        .children(options.iter().cloned().map(|(protocol, title)| {
            let is_local = protocol.is_none();
            // 被禁用但仍保留在表单中的原协议继续以既有禁用态呈现，不额外增加提示行。
            let protocol_enabled =
                protocol.is_none_or(|protocol| panel.hosts.protocol_enabled(protocol));
            let selected_item = if selected_plugin.is_some() {
                false
            } else if is_local {
                panel.active_editor_kind == Some(ConnectionsEditorKind::Local)
            } else {
                panel.active_editor_kind == Some(ConnectionsEditorKind::Remote)
                    && selected == protocol
            };
            choice_button(
                format!("connections-protocol-{protocol:?}"),
                title,
                selected_item,
                disabled || panel.editor_protocol_locked() || !protocol_enabled,
                theme,
            )
            .debug_selector(move || format!("connections-protocol-{protocol:?}"))
            .icon(match protocol {
                Some(RemoteProtocol::Ssh | RemoteProtocol::Telnet | RemoteProtocol::Serial) => {
                    IconName::SquareTerminal
                }
                Some(RemoteProtocol::Sftp) => IconName::FolderOpen,
                Some(RemoteProtocol::Rdp) => IconName::Frame,
                None => IconName::SquareTerminal,
            })
            .h(rems(2.5))
            .flex_shrink_0()
            .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                if is_local {
                    panel.activate_local_host_editor(window, cx);
                } else if let Some(protocol) = protocol {
                    panel.activate_remote_protocol(protocol, window, cx);
                }
            }))
        }))
        .children(plugin_options.into_iter().map(|(type_id, title)| {
            let selected = selected_plugin.as_deref() == Some(type_id.as_str());
            choice_button(
                format!("connections-protocol-plugin-{type_id}"),
                title,
                selected,
                disabled || panel.editor_protocol_locked(),
                theme,
            )
            .icon(gpui_kit::assets::IconName::Box)
            .h(rems(2.5))
            .flex_shrink_0()
            .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                panel.activate_plugin_form(&type_id, window, cx);
            }))
        }))
        .into_any_element()
}

/// 给草稿选项统一设置透明未选态和高对比选中态，确保不同主题下都能辨认当前选择。
/// @author kongweiguang
fn choice_button(
    id: impl Into<gpui_kit::ElementId>,
    label: impl Into<gpui_kit::SharedString>,
    selected: bool,
    disabled: bool,
    theme: ThemeTokens,
) -> Button {
    let button = Button::new(id)
        .compact()
        .text_size(theme.text_body)
        .label(label)
        .disabled(disabled);
    if selected {
        button
            .secondary()
            .selected(true)
            .bg(theme.selected)
            .text_color(theme.selected_foreground)
            .border_1()
            .border_color(theme.accent)
    } else {
        button.ghost()
    }
}

/// 地址与端口共用一个校验槽和字段右缘；错误仍按真实字段身份打开详情。
/// @author kongweiguang
fn render_endpoint(editor: &HostEditor, disabled: bool, theme: ThemeTokens) -> impl IntoElement {
    let field_scope = host_editor_scope(editor, "endpoint");
    let mut fields = div().w_full().flex().flex_col().gap_2().min_w(px(0.));
    if editor.protocol != RemoteProtocol::Serial {
        let issue_field = if editor
            .validation_issue
            .as_ref()
            .is_some_and(|issue| issue.field == HostEditorField::Port)
        {
            HostEditorField::Port
        } else {
            HostEditorField::Host
        };
        fields = fields.child(with_validation_issue(
            field(
                crate::locale::text("主机地址", "Host address"),
                div()
                    .w_full()
                    .flex()
                    .items_start()
                    .gap_3()
                    .child(
                        div().flex_1().min_w(px(0.)).child(
                            Input::new(&editor.inputs.host)
                                .h(theme.metrics.control_height)
                                .disabled(disabled)
                                .w_full(),
                        ),
                    )
                    .child(
                        div()
                            .w(rems(endpoint_port_column_min_width_px() / 16.0))
                            .min_w(rems(4.0))
                            .flex_shrink_0()
                            .child(
                                div()
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted)
                                            .child(crate::locale::text("端口", "Port")),
                                    )
                                    .child(
                                        Input::new(&editor.inputs.port)
                                            .id("connections-editor-port-input")
                                            .h(theme.metrics.control_height)
                                            .disabled(disabled)
                                            .w_full()
                                            .aria_label(crate::locale::text("端口", "Port")),
                                    ),
                            ),
                    ),
                theme,
                &field_scope,
            ),
            editor,
            issue_field,
            theme,
        ));
        if editor.protocol != RemoteProtocol::Telnet {
            fields = fields.child(with_validation_issue(
                input_field(
                    crate::locale::text("用户名", "Username"),
                    Input::new(&editor.inputs.username)
                        .disabled(disabled)
                        .w_full(),
                    theme,
                    &field_scope,
                ),
                editor,
                HostEditorField::Username,
                theme,
            ));
        }
    }
    fields
}

/// 认证字段共用表单右缘，每行最多留一个校验位，错误详情仍跟随实际出错的输入。
/// @author kongweiguang
fn render_ssh_auth(
    editor: &HostEditor,
    has_credential: bool,
    has_passphrase: bool,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let field_scope = host_editor_scope(editor, "authentication");
    let mode = editor.authentication_mode;
    let panel_entity = cx.entity().clone();
    let options = [
        (
            HostAuthenticationMode::Agent,
            crate::locale::text("SSH Agent", "SSH Agent"),
        ),
        (
            HostAuthenticationMode::Password,
            crate::locale::text("密码", "Password"),
        ),
        (
            HostAuthenticationMode::PrivateKey,
            crate::locale::text("私钥", "Private key"),
        ),
    ];
    let selected_label = options
        .iter()
        .find(|(candidate, _)| *candidate == mode)
        .map(|(_, label)| label.clone())
        .unwrap_or_else(|| crate::locale::text("SSH Agent", "SSH Agent"));
    let mode_picker = Button::new("connections-auth-mode")
        .w_full()
        .h(theme.metrics.control_height)
        .compact()
        .outline()
        .dropdown_caret(true)
        .label(selected_label)
        .accessibility_label(crate::locale::text("认证方式", "Authentication method"))
        .disabled(disabled)
        .dropdown_menu(move |mut menu, _, _| {
            for (candidate, label) in options.iter().cloned() {
                let panel_entity = panel_entity.clone();
                menu = menu.item(
                    PopupMenuItem::new(label)
                        .checked(candidate == mode)
                        .on_click(move |_, window, app| {
                            panel_entity.update(app, |panel, cx| {
                                panel.set_authentication_mode(candidate, window, cx)
                            });
                        }),
                );
            }
            menu
        });
    let mode_field = field(
        crate::locale::text("认证", "Authentication"),
        mode_picker,
        theme,
        &field_scope,
    );
    let mut section = div().w_full().flex().flex_col().gap_2();
    match mode {
        HostAuthenticationMode::Agent => {
            section = section.child(with_validation_issue(
                mode_field,
                editor,
                HostEditorField::Authentication,
                theme,
            ));
        }
        HostAuthenticationMode::Password => {
            let password_section = div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(mode_field)
                .child(input_field(
                    crate::locale::text("密码", "Password"),
                    password_input(Input::new(&editor.inputs.secret))
                        .disabled(disabled)
                        .w_full(),
                    theme,
                    &field_scope,
                ));
            let password_section = password_section.child(credential_status(
                &field_scope,
                "target-password-clear",
                has_credential,
                editor.clear_target_credential,
                disabled,
                crate::locale::text("密码", "Password"),
                cx.listener(|panel, _: &ClickEvent, window, cx| {
                    if let Some(editor) = panel.editor.as_mut() {
                        editor.clear_target_credential = !editor.clear_target_credential;
                        if editor.clear_target_credential {
                            clear_input(&editor.inputs.secret, window, cx);
                        }
                    }
                    panel.reset_connection_probe();
                    cx.notify();
                }),
            ));
            section = section.child(with_validation_issue(
                password_section,
                editor,
                HostEditorField::Authentication,
                theme,
            ));
        }
        HostAuthenticationMode::PrivateKey => {
            let source = RadioGroup::horizontal("host-private-key-source")
                .w_full()
                .disabled(disabled)
                .selected_index(Some(usize::from(
                    editor.private_key_input_mode == PrivateKeyInputMode::Paste,
                )))
                .children([
                    Radio::new("file").label(crate::locale::text("文件", "File")),
                    Radio::new("paste").label(crate::locale::text("粘贴内容", "Pasted key")),
                ])
                .on_change(cx.listener(|panel, index: &usize, window, cx| {
                    if let Some(editor) = panel.editor.as_mut() {
                        editor.select_private_key_input_mode(
                            if *index == 0 {
                                PrivateKeyInputMode::File
                            } else {
                                PrivateKeyInputMode::Paste
                            },
                            window,
                            cx,
                        );
                    }
                    panel.reset_connection_probe();
                    cx.notify();
                }));
            section = section.child(with_validation_issue(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(mode_field)
                    .child(field(
                        crate::locale::text("私钥来源", "Key source"),
                        source,
                        theme,
                        &field_scope,
                    )),
                editor,
                HostEditorField::Authentication,
                theme,
            ));
            section = section.child(match editor.private_key_input_mode {
                PrivateKeyInputMode::File => with_validation_issue(
                    path_field(
                        crate::locale::text("私钥文件", "Private key file"),
                        Input::new(&editor.inputs.key_path)
                            .disabled(disabled)
                            .w_full(),
                        HostFilePickerTarget::PrivateKey,
                        disabled,
                        theme,
                        &field_scope,
                        cx,
                    ),
                    editor,
                    HostEditorField::PrivateKey,
                    theme,
                ),
                PrivateKeyInputMode::Paste => with_validation_issue(
                    multiline_field(
                        crate::locale::text("私钥内容", "Private key"),
                        Textarea::new(&editor.inputs.private_key)
                            .h(rems(7.0))
                            .disabled(disabled)
                            .w_full(),
                        theme,
                        &field_scope,
                    ),
                    editor,
                    HostEditorField::PrivateKey,
                    theme,
                ),
            });
            let key_tail = div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(credential_status(
                    &field_scope,
                    "target-private-key-clear",
                    editor.has_target_private_key(),
                    editor.clear_private_key,
                    disabled,
                    crate::locale::text("私钥", "Private key"),
                    cx.listener(|panel, _: &ClickEvent, window, cx| {
                        if let Some(editor) = panel.editor.as_mut() {
                            editor.clear_private_key = !editor.clear_private_key;
                            if editor.clear_private_key {
                                editor
                                    .inputs
                                    .private_key
                                    .update(cx, |state, cx| state.set_value("", window, cx));
                            }
                        }
                        panel.reset_connection_probe();
                        cx.notify();
                    }),
                ));
            let key_tail = key_tail.child(input_field(
                crate::locale::text("私钥口令", "Key passphrase"),
                password_input(Input::new(&editor.inputs.passphrase))
                    .disabled(disabled)
                    .w_full(),
                theme,
                &field_scope,
            ));
            let key_tail = key_tail.child(credential_status(
                &field_scope,
                "target-passphrase-clear",
                has_passphrase,
                editor.clear_passphrase,
                disabled,
                crate::locale::text("私钥口令", "Key passphrase"),
                cx.listener(|panel, _: &ClickEvent, window, cx| {
                    if let Some(editor) = panel.editor.as_mut() {
                        editor.clear_passphrase = !editor.clear_passphrase;
                        if editor.clear_passphrase {
                            clear_input(&editor.inputs.passphrase, window, cx);
                        }
                    }
                    panel.reset_connection_probe();
                    cx.notify();
                }),
            ));
            section = section.child(with_validation_issue(key_tail, editor, None, theme));
        }
    }
    section.into_any_element()
}

/// RDP 只说明可用的客户端认证方式，已有密码可用独立状态与动作管理。
/// @author kongweiguang
fn render_rdp_auth(
    editor: &HostEditor,
    has_credential: bool,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let field_scope = host_editor_scope(editor, "rdp-authentication");
    let credential_note = if cfg!(target_os = "windows") {
        crate::locale::text(
            "也可在系统客户端中输入用户名和密码。",
            "You can also enter your username and password in the system client.",
        )
    } else {
        crate::locale::text(
            "请在系统 RDP 客户端中输入密码。",
            "Enter your password in the system RDP client.",
        )
    };
    let mut section = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .border_t_1()
        .border_color(theme.border)
        .pt_3()
        .child(section_title(
            crate::locale::text("RDP 凭据", "RDP credentials"),
            theme,
        ));
    if cfg!(target_os = "windows") {
        section = section
            .child(input_field(
                crate::locale::text("密码", "Password"),
                password_input(Input::new(&editor.inputs.secret))
                    .disabled(disabled)
                    .w_full(),
                theme,
                &field_scope,
            ))
            .child(credential_status(
                &field_scope,
                "rdp-password-clear",
                has_credential,
                editor.clear_rdp_password,
                disabled,
                crate::locale::text("RDP 密码", "RDP password"),
                cx.listener(|panel, _: &ClickEvent, window, cx| {
                    if let Some(editor) = panel.editor.as_mut() {
                        editor.clear_rdp_password = !editor.clear_rdp_password;
                        if editor.clear_rdp_password {
                            clear_input(&editor.inputs.secret, window, cx);
                        }
                    }
                    panel.reset_connection_probe();
                    cx.notify();
                }),
            ));
    }
    section.child(authentication_note(credential_note, theme))
}

/// SSH 高级项只在需要时展开；缺省值由共享 provider 解析，恢复目录不被主机默认值覆盖。
///
/// @author kongweiguang
fn render_ssh_terminal(
    editor: &HostEditor,
    disabled: bool,
    theme: ThemeTokens,
) -> impl IntoElement {
    let field_scope = host_editor_scope(editor, "terminal");
    div().w_full().flex().flex_col().gap_3()
        .child(input_field("TERM", Input::new(&editor.inputs.term).disabled(disabled).w_full(), theme, &field_scope))
        .child(input_field(crate::locale::text("默认目录（可选）", "Initial directory (optional)"), Input::new(&editor.inputs.working_directory).disabled(disabled).w_full(), theme, &field_scope))
        .child(note_card(crate::locale::text("仅用于新终端；恢复或显式指定的目录优先。", "Applies to new terminals only; restored or explicitly selected directories take precedence."), theme))
}

/// 网络预算仍按秒写入现有草稿；单位和范围作为次级提示，避免挤进主标签破坏可读性。
///
/// @author kongweiguang
fn render_network_options(
    editor: &HostEditor,
    disabled: bool,
    theme: ThemeTokens,
) -> impl IntoElement {
    let field_scope = host_editor_scope(editor, "network");
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_3()
        .child(network_option_input_field(
            crate::locale::text("连接超时", "Connect timeout"),
            crate::locale::text("秒 · 1–600", "Seconds · 1–600"),
            Input::new(&editor.inputs.connect_timeout)
                .disabled(disabled)
                .w_full(),
            theme,
            &field_scope,
        ))
        .child(network_option_input_field(
            crate::locale::text("心跳间隔", "Keepalive interval"),
            crate::locale::text("秒 · 0 表示关闭", "Seconds · 0 disables"),
            Input::new(&editor.inputs.keepalive)
                .disabled(disabled)
                .w_full(),
            theme,
            &field_scope,
        ))
}

#[derive(Clone, Copy)]
enum HostOptionToggle {
    ProxyEnabled,
    SftpEnabled,
    PreserveMtime,
    FollowSourceSymlinks,
}

/// 二值设置使用受控复选框，焦点留在设置本身且每次真实值变化都会使旧探测结果失效。
///
/// @author kongweiguang
fn host_option_toggle(
    id: &'static str,
    label: gpui_kit::SharedString,
    selected: bool,
    disabled: bool,
    option: HostOptionToggle,
    scope: &EditorFieldScope,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let checkbox = Checkbox::new(id)
        .w_full()
        .label(label)
        .disabled(disabled)
        .checked(selected)
        .on_change(cx.listener(move |panel, value: &bool, _, cx| {
            if let Some(editor) = panel.editor.as_mut() {
                match option {
                    HostOptionToggle::ProxyEnabled => editor.proxy_enabled = *value,
                    HostOptionToggle::SftpEnabled => editor.sftp_enabled = *value,
                    HostOptionToggle::PreserveMtime => editor.preserve_mtime = *value,
                    HostOptionToggle::FollowSourceSymlinks => {
                        editor.follow_source_symlinks = *value
                    }
                }
            }
            panel.reset_connection_probe();
            cx.notify();
        }));
    let checkbox = if matches!(option, HostOptionToggle::FollowSourceSymlinks) {
        checkbox.tooltip(crate::locale::text(
            "只跟随源端链接；目标写入和删除不会跟随链接。",
            "Follows source links only; destination writes and deletes never follow links.",
        ))
    } else {
        checkbox
    };
    let row = scope.wrap(id, checkbox);
    if matches!(option, HostOptionToggle::PreserveMtime) {
        row.debug_selector("connections-editor-preserve-mtime-row")
    } else {
        row
    }
}

/// SFTP 始终代表文件连接；远端目录错误绑定本字段，SSH 关闭文件能力后保留禁用草稿。
///
/// @author kongweiguang
fn render_transfer_options(
    editor: &HostEditor,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let field_scope = host_editor_scope(editor, "transfer");
    let sftp_enabled = editor.protocol == RemoteProtocol::Sftp || editor.sftp_enabled;
    let mut section = div().w_full().flex().flex_col().gap_3();
    if editor.protocol == RemoteProtocol::Ssh {
        section = section.child(host_option_toggle(
            "host-sftp-enabled",
            crate::locale::text("启用 SFTP 文件传输", "Enable SFTP file transfer"),
            editor.sftp_enabled,
            disabled,
            HostOptionToggle::SftpEnabled,
            &field_scope,
            cx,
        ));
    }
    section
        .child(path_field(
            crate::locale::text("本地起始目录", "Local start directory"),
            Input::new(&editor.inputs.sftp_local_directory)
                .disabled(disabled || !sftp_enabled)
                .w_full(),
            HostFilePickerTarget::SftpLocalDirectory,
            disabled || !sftp_enabled,
            theme,
            &field_scope,
            cx,
        ))
        .child(with_validation_issue(
            input_field(
                crate::locale::text("远端起始目录", "Remote start directory"),
                Input::new(&editor.inputs.sftp_remote_directory)
                    .disabled(disabled || !sftp_enabled)
                    .w_full(),
                theme,
                &field_scope,
            ),
            editor,
            HostEditorField::SftpRemoteDirectory,
            theme,
        ))
        .child(network_option_input_field(
            crate::locale::text("并发数", "Concurrency"),
            crate::locale::text(
                "1–8，留空继承全局",
                "1–8; leave empty to inherit the global setting",
            ),
            Input::new(&editor.inputs.sftp_concurrency)
                .disabled(disabled || !sftp_enabled)
                .w_full(),
            theme,
            &field_scope,
        ))
        .child(host_option_toggle(
            "host-sftp-mtime",
            crate::locale::text("保留修改时间", "Preserve modification time"),
            editor.preserve_mtime,
            disabled || !sftp_enabled,
            HostOptionToggle::PreserveMtime,
            &field_scope,
            cx,
        ))
        .child(host_option_toggle(
            "host-sftp-follow",
            crate::locale::text("跟随源端符号链接", "Follow source symbolic links"),
            editor.follow_source_symlinks,
            disabled || !sftp_enabled,
            HostOptionToggle::FollowSourceSymlinks,
            &field_scope,
            cx,
        ))
}

/// 显示模式互斥，宽高分别定位错误；全屏禁用尺寸草稿，成对校验仍由应用层执行。
///
/// @author kongweiguang
fn render_rdp_display(
    editor: &HostEditor,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let field_scope = host_editor_scope(editor, "display");
    let display_mode = RadioGroup::horizontal("host-rdp-display-mode")
        .w_full()
        .disabled(disabled)
        .selected_index(Some(usize::from(!editor.rdp_fullscreen)))
        .children([
            Radio::new("fullscreen").label(crate::locale::text("全屏", "Fullscreen")),
            Radio::new("windowed").label(crate::locale::text("窗口", "Windowed")),
        ])
        .on_change(cx.listener(|panel, index: &usize, _, cx| {
            if let Some(editor) = panel.editor.as_mut() {
                editor.rdp_fullscreen = *index == 0;
            }
            panel.reset_connection_probe();
            cx.notify();
        }));
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_3()
        .child(field(
            crate::locale::text("显示模式", "Display mode"),
            display_mode,
            theme,
            &field_scope,
        ))
        .child(with_validation_issue(
            network_option_input_field(
                crate::locale::text("窗口宽度（像素）", "Width (px)"),
                "",
                Input::new(&editor.inputs.rdp_width)
                    .disabled(disabled || editor.rdp_fullscreen)
                    .w_full(),
                theme,
                &field_scope,
            ),
            editor,
            HostEditorField::RdpWidth,
            theme,
        ))
        .child(with_validation_issue(
            network_option_input_field(
                crate::locale::text("窗口高度（像素）", "Height (px)"),
                "",
                Input::new(&editor.inputs.rdp_height)
                    .disabled(disabled || editor.rdp_fullscreen)
                    .w_full(),
                theme,
                &field_scope,
            ),
            editor,
            HostEditorField::RdpHeight,
            theme,
        ))
        .child(
            div()
                .max_w(rems(form_field_row_max_width_rems()))
                .text_xs()
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "宽、高同时留空时使用系统客户端默认尺寸。",
                    "Leave both dimensions blank to use the system client size.",
                )),
        )
}

/// 设备路径、候选菜单和刷新组成同一输入行；枚举反馈放在入口说明中，区分扫描中与空列表且不预留大块正文。
/// @author kongweiguang
fn render_serial_connection(
    panel: &ConnectionsPanel,
    editor: &HostEditor,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let field_scope = host_editor_scope(editor, "serial");
    let selected_device = editor.inputs.host.read(cx).value().trim().to_owned();
    let ports = panel.serial_ports.clone();
    let owner = cx.entity().clone();
    let state = serial_port_list_state(
        panel.serial_port_service.is_some(),
        panel.serial_ports_loading,
        panel.serial_ports_loaded,
        panel.serial_ports_issue,
        panel.serial_ports.len(),
    );
    let state_text = match state {
        SerialPortListState::ServiceUnavailable => crate::locale::text(
            "设备列表不可用，可手动输入路径。",
            "Device list unavailable; enter a path manually.",
        ),
        SerialPortListState::Loading => crate::locale::text("正在刷新设备…", "Refreshing devices…"),
        SerialPortListState::Cancelled => crate::locale::text("刷新已取消。", "Refresh cancelled."),
        SerialPortListState::Failed => crate::locale::text(
            "无法读取设备列表，请检查连接和访问权限。",
            "Could not read devices. Check the connection and access permissions.",
        ),
        SerialPortListState::NotEnumerated => crate::locale::text(
            "选择设备或手动输入路径。",
            "Select a device or enter a path.",
        ),
        SerialPortListState::Empty => crate::locale::text(
            "未发现设备。连接设备后刷新，或手动输入路径。",
            "No devices found. Connect one and refresh, or enter a path.",
        ),
        SerialPortListState::Available => crate::locale::text("", ""),
    };
    let menu_status_text = state_text.clone();

    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(with_validation_issue(
            field(
                crate::locale::text("设备路径", "Device path"),
                div()
                    .flex()
                    .flex_1()
                    .min_w(px(0.))
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(Input::new(&editor.inputs.host).disabled(disabled).w_full()),
                    )
                    .child(
                        editor_icon_button("connections-serial-device-picker", theme)
                            .icon(IconName::ChevronDown)
                            .tooltip(if state_text.is_empty() {
                                crate::locale::text(
                                    "选择设备或手动输入路径",
                                    "Select a device or enter a path",
                                )
                            } else {
                                state_text.clone()
                            })
                            .accessibility_label(crate::locale::text("串口设备", "Serial device"))
                            .disabled(disabled)
                            .dropdown_menu(move |mut menu, _, _| {
                                if ports.is_empty() {
                                    return menu.item(
                                        PopupMenuItem::new(if menu_status_text.is_empty() {
                                            crate::locale::text("暂无设备", "No devices available")
                                        } else {
                                            menu_status_text.clone()
                                        })
                                        .disabled(true),
                                    );
                                }
                                for port in &ports {
                                    let name = port.name.clone();
                                    let details =
                                        [port.manufacturer.as_deref(), port.product.as_deref()]
                                            .into_iter()
                                            .flatten()
                                            .filter(|value| !value.trim().is_empty())
                                            .collect::<Vec<_>>()
                                            .join(" · ");
                                    let label = if details.is_empty() {
                                        name.clone()
                                    } else {
                                        format!("{name} · {details}")
                                    };
                                    let owner = owner.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(label)
                                            .checked(name.as_str() == selected_device.as_str())
                                            .on_click(move |_, window, app| {
                                                let name = name.clone();
                                                owner.update(app, |panel, cx| {
                                                    panel.select_serial_port(name, window, cx);
                                                });
                                            }),
                                    );
                                }
                                menu
                            }),
                    )
                    .child(
                        field_scope.wrap(
                            "serial-refresh",
                            editor_icon_button("connections-serial-refresh", theme)
                                .icon(if panel.serial_ports_loading {
                                    IconName::LoaderCircle
                                } else if matches!(
                                    state,
                                    SerialPortListState::Failed
                                        | SerialPortListState::ServiceUnavailable
                                ) {
                                    IconName::CircleAlert
                                } else {
                                    IconName::RefreshCw
                                })
                                .accessibility_label(crate::locale::text(
                                    "刷新串口设备",
                                    "Refresh serial devices",
                                ))
                                .tooltip(if state_text.is_empty() {
                                    crate::locale::text("刷新设备", "Refresh devices")
                                } else {
                                    state_text.clone()
                                })
                                .disabled(
                                    disabled
                                        || panel.serial_ports_loading
                                        || panel.serial_port_service.is_none(),
                                )
                                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                                    panel.refresh_serial_ports(cx);
                                })),
                        ),
                    ),
                theme,
                &field_scope,
            ),
            editor,
            HostEditorField::Host,
            theme,
        ))
        .child(with_validation_issue(
            input_field(
                crate::locale::text("波特率", "Baud rate"),
                Input::new(&editor.inputs.serial_baud)
                    .disabled(disabled)
                    .w_full(),
                theme,
                &field_scope,
            ),
            editor,
            HostEditorField::SerialBaud,
            theme,
        ))
        .child(authentication_note(
            crate::locale::text(
                "300–4,000,000 · 依设备驱动",
                "300–4,000,000 · device dependent",
            ),
            theme,
        ))
}

/// 线制枚举沿同一字段轴排列，避免窄窗口中双列标签挤压输入。
/// @author kongweiguang
fn render_serial_line_options(
    editor: &HostEditor,
    disabled: bool,
    theme: ThemeTokens,
) -> impl IntoElement {
    let field_scope = host_editor_scope(editor, "serial-line");
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(field(
            crate::locale::text("数据位", "Data bits"),
            Select::new(&editor.serial_data_bits_select)
                .w_full()
                .h(theme.metrics.control_height)
                .disabled(disabled)
                .accessibility_label(crate::locale::text("数据位", "Data bits")),
            theme,
            &field_scope,
        ))
        .child(field(
            crate::locale::text("停止位", "Stop bits"),
            Select::new(&editor.serial_stop_bits_select)
                .w_full()
                .h(theme.metrics.control_height)
                .disabled(disabled)
                .accessibility_label(crate::locale::text("停止位", "Stop bits")),
            theme,
            &field_scope,
        ))
        .child(field(
            crate::locale::text("校验", "Parity"),
            Select::new(&editor.serial_parity_select)
                .w_full()
                .h(theme.metrics.control_height)
                .disabled(disabled)
                .accessibility_label(crate::locale::text("校验方式", "Parity")),
            theme,
            &field_scope,
        ))
        .child(field(
            crate::locale::text("流控", "Flow control"),
            Select::new(&editor.serial_flow_select)
                .w_full()
                .h(theme.metrics.control_height)
                .disabled(disabled)
                .accessibility_label(crate::locale::text("流控方式", "Flow control")),
            theme,
            &field_scope,
        ))
}

/// 空快照、失败与未注入服务必须有不同反馈，避免把驱动故障伪装成“没有设备”。
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SerialPortListState {
    ServiceUnavailable,
    Loading,
    Cancelled,
    Failed,
    NotEnumerated,
    Empty,
    Available,
}

/// 将服务、异步状态和快照大小转换为明确可测的串口列表展示状态。
/// @author kongweiguang
fn serial_port_list_state(
    service_available: bool,
    loading: bool,
    loaded: bool,
    issue: Option<super::panel::SerialPortScanIssue>,
    port_count: usize,
) -> SerialPortListState {
    if !service_available || issue == Some(super::panel::SerialPortScanIssue::ServiceUnavailable) {
        return SerialPortListState::ServiceUnavailable;
    }
    if loading {
        return SerialPortListState::Loading;
    }
    match issue {
        Some(super::panel::SerialPortScanIssue::Cancelled) => SerialPortListState::Cancelled,
        Some(super::panel::SerialPortScanIssue::EnumerationFailed) => SerialPortListState::Failed,
        Some(super::panel::SerialPortScanIssue::ServiceUnavailable) => {
            SerialPortListState::ServiceUnavailable
        }
        None if !loaded => SerialPortListState::NotEnumerated,
        None if port_count == 0 => SerialPortListState::Empty,
        None => SerialPortListState::Available,
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/connections_serial.rs"]
mod connections_serial_tests;

/// 代理开关卸载字段时交接表单焦点；密码仍走 CAS/vault，空输入保留原引用。
/// 端口和可选用户名独立成行，避免百分比宽度在无确定尺寸的横排中收缩，也便于窄窗口阅读。
/// @author kongweiguang
fn render_proxy_route(
    editor: &HostEditor,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let field_scope = host_editor_scope(editor, "proxy");
    let has_proxy_password = editor.has_proxy_password(cx);
    let mut section = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_3()
        .child(host_option_toggle(
            "connections-proxy-toggle",
            crate::locale::text("使用代理", "Enable proxy"),
            editor.proxy_enabled,
            disabled,
            HostOptionToggle::ProxyEnabled,
            &field_scope,
            cx,
        ));
    if editor.proxy_enabled {
        section = section
            .child(field(
                crate::locale::text("代理类型", "Proxy type"),
                proxy_protocol_picker(editor.proxy_protocol, disabled, theme, cx),
                theme,
                &field_scope,
            ))
            .child(input_field(
                crate::locale::text("代理地址", "Proxy address"),
                Input::new(&editor.inputs.proxy_host)
                    .disabled(disabled)
                    .w_full(),
                theme,
                &field_scope,
            ))
            .child(input_field(
                crate::locale::text("代理端口", "Proxy port"),
                Input::new(&editor.inputs.proxy_port)
                    .id("connections-proxy-port-input")
                    .disabled(disabled)
                    .w(rems(12.0)),
                theme,
                &field_scope,
            ))
            .child(input_field(
                crate::locale::text("代理用户名", "Proxy username"),
                Input::new(&editor.inputs.proxy_username)
                    .id("connections-proxy-username-input")
                    .disabled(disabled)
                    .w_full(),
                theme,
                &field_scope,
            ))
            .child(input_field(
                crate::locale::text("代理密码", "Proxy password"),
                password_input(Input::new(&editor.inputs.proxy_password))
                    .disabled(disabled)
                    .w_full(),
                theme,
                &field_scope,
            ))
            .child(credential_status(
                &field_scope,
                "proxy-password-clear",
                has_proxy_password,
                editor.clear_proxy_password,
                disabled,
                crate::locale::text("代理密码", "proxy password"),
                cx.listener(|panel, _: &ClickEvent, window, cx| {
                    if let Some(editor) = panel.editor.as_mut() {
                        editor.clear_proxy_password = !editor.clear_proxy_password;
                        if editor.clear_proxy_password {
                            clear_input(&editor.inputs.proxy_password, window, cx);
                        }
                    }
                    panel.reset_connection_probe();
                    cx.notify();
                }),
            ))
            .child(note_card(
                crate::locale::text(
                    "代理需要认证时，再填写用户名和密码。",
                    "Enter a username and password if the proxy requires authentication.",
                ),
                theme,
            ));
    }
    section
}

/// 工具行与有序跳板卡共用右缘，原顺序和凭据槽位保持不变。
/// @author kongweiguang
fn render_jump_hosts(
    editor: &HostEditor,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let field_scope = host_editor_scope(editor, "jump-hosts");
    let mut section = div().w_full().flex().flex_col().gap_3();
    section = section.child(
        div()
            .w_full()
            .max_w(rems(form_field_row_max_width_rems()))
            .flex()
            .items_center()
            .justify_end()
            .child(
                field_scope.wrap(
                    "jump-add",
                    Button::new("connections-add-jump")
                        .compact()
                        .label(crate::locale::text("添加跳板", "Add jump host"))
                        .disabled(disabled || editor.jumps.len() >= 8)
                        .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                            panel.add_jump(window, cx)
                        })),
                ),
            ),
    );
    if editor.jumps.is_empty() {
        section = section.child(note_card(
            crate::locale::text(
                "可选；连接顺序为代理 → 跳板 → 目标。最多 8 个跳板。",
                "Optional. Connection order is proxy → jump hosts → target. Up to 8 jump hosts.",
            ),
            theme,
        ));
    }
    for (index, jump) in editor.jumps.iter().enumerate() {
        section = section.child(render_jump(editor, index, jump, disabled, theme, cx));
    }
    section
}

/// 让用户切换 SOCKS5/HTTP CONNECT并标明当前选择，同时保留代理的 host 级 vault ref。
fn proxy_protocol_picker(
    selected: kerminal_domain::connections::ProxyProtocol,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    use kerminal_domain::connections::ProxyProtocol;
    div().flex().gap_2().children(
        [
            (ProxyProtocol::Socks5, "SOCKS5"),
            (ProxyProtocol::HttpConnect, "HTTP CONNECT"),
        ]
        .into_iter()
        .map(|(protocol, label)| {
            choice_button(
                format!("connections-proxy-protocol-{protocol:?}"),
                label,
                selected == protocol,
                disabled,
                theme,
            )
            .on_click(cx.listener(move |panel, _: &ClickEvent, _, cx| {
                if let Some(editor) = panel.editor.as_mut()
                    && editor.proxy_protocol != protocol
                {
                    editor.proxy_protocol = protocol;
                    panel.reset_connection_probe();
                    cx.notify();
                }
            }))
        }),
    )
}

/// 跳板身份字段沿同一标签轴排列；认证切换保留可见焦点与独立凭据槽位。
/// @author kongweiguang
fn render_jump(
    editor: &HostEditor,
    index: usize,
    jump: &JumpInputs,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let field_scope = EditorFieldScope::new(
        editor.scroll_handle.clone(),
        format!(
            "jump-{:?}-{:?}",
            editor.inputs.name.entity_id(),
            jump.host.entity_id()
        ),
    );
    let jump_id = format!("{:?}", jump.host.entity_id());
    let original = jump.original_authentication_ref();
    let mut card = div()
        .id(("connections-jump-card", jump.host.entity_id()))
        .w_full()
        .max_w(rems(form_field_row_max_width_rems()))
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(theme.sidebar)
        .child(
            div()
                .flex()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .text_size(px(13.))
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .child(crate::locale::text(
                            format!("跳板 {}", index + 1),
                            format!("Jump host {}", index + 1),
                        )),
                )
                .child(
                    field_scope.wrap(
                        "move-up",
                        editor_icon_button(format!("connections-jump-move-up-{jump_id}"), theme)
                            .icon(IconName::ArrowUp)
                            .accessibility_label(crate::locale::text(
                                "上移跳板",
                                "Move jump host up",
                            ))
                            .tooltip(crate::locale::text("上移", "Move up"))
                            .disabled(disabled || index == 0)
                            .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                                panel.move_jump(index, true, window, cx);
                            })),
                    ),
                )
                .child(
                    field_scope.wrap(
                        "move-down",
                        editor_icon_button(format!("connections-jump-move-down-{jump_id}"), theme)
                            .icon(IconName::ArrowDown)
                            .accessibility_label(crate::locale::text(
                                "下移跳板",
                                "Move jump host down",
                            ))
                            .tooltip(crate::locale::text("下移", "Move down"))
                            .disabled(disabled || index + 1 >= editor.jumps.len())
                            .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                                panel.move_jump(index, false, window, cx);
                            })),
                    ),
                )
                .child(
                    field_scope.wrap(
                        "remove",
                        Button::new(format!("connections-remove-jump-{jump_id}"))
                            .compact()
                            .label(crate::locale::text("移除", "Remove"))
                            .disabled(disabled)
                            .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                                panel.remove_jump(index, window, cx)
                            })),
                    ),
                ),
        )
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_3()
                .child(input_field(
                    crate::locale::text("名称", "Name"),
                    Input::new(&jump.name).disabled(disabled).w_full(),
                    theme,
                    &field_scope,
                ))
                .child(input_field(
                    crate::locale::text("地址", "Address"),
                    Input::new(&jump.host).disabled(disabled).w_full(),
                    theme,
                    &field_scope,
                ))
                .child(input_field(
                    crate::locale::text("端口", "Port"),
                    Input::new(&jump.port).disabled(disabled).w_full(),
                    theme,
                    &field_scope,
                ))
                .child(input_field(
                    crate::locale::text("用户名", "Username"),
                    Input::new(&jump.username).disabled(disabled).w_full(),
                    theme,
                    &field_scope,
                )),
        );
    let mode = jump.authentication_mode;
    let options = [
        (
            HostAuthenticationMode::Agent,
            crate::locale::text("Agent", "Agent"),
        ),
        (
            HostAuthenticationMode::Password,
            crate::locale::text("密码", "Password"),
        ),
        (
            HostAuthenticationMode::PrivateKey,
            crate::locale::text("私钥", "Private key"),
        ),
    ];
    card = card.child(
        field_scope.wrap(
            "authentication-mode",
            div()
                .flex()
                .gap_1()
                .p_1()
                .rounded_sm()
                .bg(theme.sidebar)
                .children(options.iter().cloned().map(|(mode, label)| {
                    let jump_id = jump_id.clone();
                    choice_button(
                        format!("connections-jump-{jump_id}-auth-{mode:?}"),
                        label,
                        jump.authentication_mode == mode,
                        disabled,
                        theme,
                    )
                    .on_click(cx.listener(
                        move |panel, _: &ClickEvent, window, cx| {
                            if let Some(editor) = panel.editor.as_mut()
                                && let Some(jump) = editor.jumps.get_mut(index)
                                && jump.authentication_mode != mode
                            {
                                jump.authentication_mode = mode;
                                window.focus(&jump.host.read(cx).focus_handle(cx), cx);
                                panel.reset_connection_probe();
                                cx.notify();
                            }
                        },
                    ))
                })),
        ),
    );
    match mode {
        HostAuthenticationMode::Agent => {
            card = card.child(note_card(
                crate::locale::text(
                    "跳板认证交给系统 SSH Agent。",
                    "Jump host authentication uses the system SSH Agent.",
                ),
                theme,
            ))
        }
        HostAuthenticationMode::Password => {
            card = card.child(input_field(
                crate::locale::text("跳板密码", "Jump host password"),
                password_input(Input::new(&jump.secret))
                    .disabled(disabled)
                    .w_full(),
                theme,
                &field_scope,
            ));
            if jump.has_password() {
                card = card.child(
                    field_scope.wrap(
                        "clear-password",
                        Button::new(format!("connections-jump-{jump_id}-clear-password"))
                            .compact()
                            .label(if jump.clear_credential {
                                crate::locale::text("保留原密码", "Keep existing password")
                            } else {
                                crate::locale::text("清除已保存密码", "Clear saved password")
                            })
                            .disabled(disabled)
                            .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                                if let Some(jump) = panel
                                    .editor
                                    .as_mut()
                                    .and_then(|editor| editor.jumps.get_mut(index))
                                {
                                    jump.clear_credential = !jump.clear_credential;
                                    if jump.clear_credential {
                                        clear_input(&jump.secret, window, cx);
                                    }
                                }
                                panel.reset_connection_probe();
                                cx.notify();
                            })),
                    ),
                );
            }
        }
        HostAuthenticationMode::PrivateKey => {
            card = card
                .child(input_field(
                    crate::locale::text("跳板私钥路径", "Jump host private key path"),
                    Input::new(&jump.key_path).disabled(disabled).w_full(),
                    theme,
                    &field_scope,
                ))
                .child(input_field(
                    crate::locale::text("私钥口令", "Key passphrase"),
                    password_input(Input::new(&jump.passphrase))
                        .disabled(disabled)
                        .w_full(),
                    theme,
                    &field_scope,
                ));
            if matches!(
                original,
                Some(
                    kerminal_domain::connections::SshAuthentication::PrivateKey {
                        passphrase: Some(_),
                        ..
                    }
                )
            ) {
                card = card.child(
                    field_scope.wrap(
                        "clear-passphrase",
                        Button::new(format!("connections-jump-{jump_id}-clear-passphrase"))
                            .compact()
                            .label(if jump.clear_passphrase {
                                crate::locale::text(
                                    "保留原私钥口令",
                                    "Keep existing key passphrase",
                                )
                            } else {
                                crate::locale::text(
                                    "清除已保存私钥口令",
                                    "Clear saved key passphrase",
                                )
                            })
                            .disabled(disabled)
                            .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                                if let Some(jump) = panel
                                    .editor
                                    .as_mut()
                                    .and_then(|editor| editor.jumps.get_mut(index))
                                {
                                    jump.clear_passphrase = !jump.clear_passphrase;
                                    if jump.clear_passphrase {
                                        clear_input(&jump.passphrase, window, cx);
                                    }
                                }
                                panel.reset_connection_probe();
                                cx.notify();
                            })),
                    ),
                );
            }
        }
    }
    card
}

/// 可校验字段与同列的普通字段预留同一状态位，描边不改变布局或焦点滚动边界。
/// @author kongweiguang
fn with_validation_issue(
    content: impl IntoElement,
    editor: &HostEditor,
    field: impl Into<Option<HostEditorField>>,
    theme: ThemeTokens,
) -> gpui_kit::AnyElement {
    let field = field.into();
    let issue = editor
        .validation_issue
        .as_ref()
        .filter(|issue| Some(issue.field) == field);
    let row =
        div()
            .w_full()
            .flex()
            .items_start()
            .gap_1()
            .when(field == Some(HostEditorField::Username), |row| {
                row.debug_selector(|| "connections-editor-username-row".into())
            })
            .child(div().flex_1().min_w_0().relative().child(content).when(
                issue.is_some(),
                |field| {
                    field.child(
                        div()
                            .absolute()
                            .inset_0()
                            .border_1()
                            .rounded_sm()
                            .border_color(theme.error),
                    )
                },
            ));
    let mut detail_slot = div()
        .w(theme.metrics.control_height)
        .h(theme.metrics.control_height)
        .flex_shrink_0();
    let Some(field) = field else {
        return row.child(detail_slot).into_any_element();
    };
    let mut row = row.id(("connections-validation-field", field as usize));
    if let Some(issue) = issue {
        let detail = issue.message.clone();
        let id = field as usize;
        let details = Popover::new(format!("connections-validation-{id}-details-popover"))
            .auto_flip()
            .trigger(
                Button::new(format!("connections-validation-{id}-details"))
                    .compact()
                    .size(theme.metrics.control_height)
                    .ghost()
                    .icon(IconName::CircleAlert)
                    .accessibility_label(crate::locale::text(
                        "查看字段错误详情",
                        "View field error details",
                    ))
                    .tooltip(crate::locale::text("查看错误详情", "View error details")),
            )
            .content(move |_, _, _| {
                div()
                    .w(rems(26.0))
                    .max_w(rems(26.0))
                    .max_h(rems(14.0))
                    .overflow_y_scrollbar()
                    .p_3()
                    .text_size(theme.text_xs)
                    .text_color(theme.error)
                    .child(detail.clone())
            });
        row = row.anchor_scroll(Some(editor.validation_anchor.clone()));
        detail_slot = detail_slot.child(details);
    }
    row.child(detail_slot).into_any_element()
}

/// 统一表单行的标签宽度与间距；端口列宽据此预留独立的最小输入区域。
/// @author kongweiguang
fn field(
    label: impl Into<gpui_kit::SharedString>,
    input: impl IntoElement,
    theme: ThemeTokens,
    scope: &EditorFieldScope,
) -> impl IntoElement {
    let label = label.into();
    EditorField::new(label.clone(), input, theme, false)
        .reveal_in(scope.scroll(), scope.identity(label.as_ref()))
}

/// 同一编辑器内再按分区隔离字段 key，协议或分类重绘不会借用别行的焦点实体。
/// @author kongweiguang
fn host_editor_scope(editor: &HostEditor, section: &str) -> EditorFieldScope {
    EditorFieldScope::new(
        editor.scroll_handle.clone(),
        format!(
            "host-{:?}-{:?}-{section}",
            editor.inputs.name.entity_id(),
            editor.protocol
        ),
    )
}

/// 认证说明保留就近可读性，但不增加另一层卡片背景或内边距。
/// @author kongweiguang
fn authentication_note(
    message: impl Into<gpui_kit::SharedString>,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .w_full()
        .text_size(theme.text_xs)
        .text_color(theme.muted)
        .child(message.into())
}

/// 秘密输入统一启用眼睛切换和密码语义；值仍由掩码 InputState 持有，
/// 不会为了显示而把已保存凭据回填到表单。
/// @author kongweiguang
fn password_input(input: Input) -> Input {
    input.mask_toggle().content_type(InputContentType::Password)
}

/// 输入和标签使用相对尺寸，保持界面缩放后的密度及窄窗标签上置行为一致。
/// @author kongweiguang
fn input_field(
    label: impl Into<gpui_kit::SharedString>,
    input: Input,
    theme: ThemeTokens,
    scope: &EditorFieldScope,
) -> impl IntoElement {
    let input = gpui_kit::Styled::h(input, theme.metrics.control_height);
    let input = gpui_kit::Styled::text_sm(input);
    field(label, input, theme, scope)
}

/// 数字字段的标签对齐输入框而非下方提示，短控件仍可随窄窗口收缩。
/// @author kongweiguang
fn network_option_input_field(
    label: impl Into<gpui_kit::SharedString>,
    hint: impl Into<gpui_kit::SharedString>,
    input: Input,
    theme: ThemeTokens,
    scope: &EditorFieldScope,
) -> impl IntoElement {
    let (_, control_max_width) = network_field_widths_px();
    let hint: gpui_kit::SharedString = hint.into();
    multiline_field(
        label,
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .w_full()
                    .max_w(rems(control_max_width / 16.0))
                    .child(input.h(theme.metrics.control_height)),
            )
            .when(!hint.is_empty(), |content| {
                content.child(authentication_note(hint, theme))
            }),
        theme,
        scope,
    )
}

/// 绘制表单分区的次级标题，让平面布局仍有清楚且一致的层级。
/// @author kongweiguang
fn editor_section_heading(
    label: impl Into<gpui_kit::SharedString>,
    theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .debug_selector(|| "connections-editor-section-heading".to_owned())
        .w_full()
        .max_w(rems(form_field_row_max_width_rems()))
        .pb_3()
        .border_b_1()
        .border_color(theme.border)
        .child(section_title(label, theme))
}

/// 各分区与内部凭据组共用标题字重，避免新增视觉层级。
/// @author kongweiguang
fn section_title(
    label: impl Into<gpui_kit::SharedString>,
    _theme: ThemeTokens,
) -> impl IntoElement {
    div()
        .text_sm()
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .child(label.into())
}

/// 帮助说明保持与字段行对齐的轻量文字，不再用背景和内边距扩成占位卡片。
/// @author kongweiguang
fn note_card(message: impl Into<gpui_kit::SharedString>, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .max_w(rems(form_field_row_max_width_rems()))
        .text_xs()
        .text_color(theme.muted)
        .child(message.into())
}

/// 只在已有凭据可管理时显示状态和按钮，避免空白表单反复显示无操作价值的提示。
/// 标记清除后仍保留原行与按钮，让用户保存前可以撤销，并区分待清除和实际未保存。
/// @author kongweiguang
fn credential_status(
    scope: &EditorFieldScope,
    identity: &str,
    exists: bool,
    clearing: bool,
    disabled: bool,
    label: impl Into<gpui_kit::SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
) -> impl IntoElement {
    if !exists {
        return div().hidden();
    }
    let label = label.into();
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_3()
        .child(div().flex_1().text_xs().child(if clearing {
            crate::locale::text("保存后清除", "Clears on save")
        } else {
            crate::locale::text("已关联加密凭据", "Encrypted credential linked")
        }))
        .child(
            scope.wrap(
                identity,
                Button::new(format!("connections-clear-{label}"))
                    .compact()
                    .label(if clearing {
                        crate::locale::text(
                            format!("保留原{label}"),
                            format!("Keep existing {label}"),
                        )
                    } else {
                        crate::locale::text(
                            format!("清除已保存{label}"),
                            format!("Clear saved {label}"),
                        )
                    })
                    .disabled(disabled)
                    .on_click(on_click),
            ),
        )
}
