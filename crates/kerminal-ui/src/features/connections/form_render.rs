//! 主机创建和编辑表单的原生绘制。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, Focusable as _, IntoElement, Window,
    component::{
        Disableable, IconName,
        button::{Button, ButtonVariants},
        input::{Input, Textarea},
        menu::{DropdownMenu as _, PopupMenuItem},
        scroll::ScrollableElement as _,
        select::Select,
    },
    div,
    prelude::*,
    px, rems,
};
use kerminal_application::connections::{
    management::HostAuthenticationMode, probe::ConnectionProbeOutcome,
};
use kerminal_domain::connections::RemoteProtocol;

use crate::theme::ThemeTokens;
use gpui_kit::base::{FocusTrapElement as _, Selectable};

use super::{
    form::{HostEditor, HostEditorField, HostEditorSection, JumpInputs, clear_input},
    form_layout::{EditorField, EditorFieldScope, compact_editor},
    local_hosts::{LocalHostEditorMode, LocalHostEditorSection},
    panel::{ConnectionsEditorKind, ConnectionsPanel, HostFilePickerTarget},
};

const FORM_FIELD_LABEL_WIDTH_PX: f32 = 92.0;
const FORM_FIELD_GAP_PX: f32 = 12.0;
const ENDPOINT_PORT_INPUT_MIN_WIDTH_PX: f32 = 80.0;
const NETWORK_FIELD_LABEL_WIDTH_PX: f32 = 96.0;
const NETWORK_FIELD_CONTROL_MAX_WIDTH_PX: f32 = 192.0;

/// 端口列至少容纳通用字段标签、字段间距和可读的端口输入；地址列承担剩余伸缩。
/// @author kongweiguang
pub(super) fn endpoint_port_column_min_width_px() -> f32 {
    FORM_FIELD_LABEL_WIDTH_PX + FORM_FIELD_GAP_PX + ENDPOINT_PORT_INPUT_MIN_WIDTH_PX
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

/// 远程 Host 仍由现有草稿和写入用例驱动；Local 分支在统一弹层中并行呈现独立 Profile 草稿。
/// 未知保存保留明确读回入口，只有 owner 验证过的快照才允许接纳，不能把刷新按钮当成重试。
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
    if let Some(status) = panel.status.clone() {
        page = page.child(status_strip(status, theme, false));
    } else if let Some(issue) = validation_summary {
        page = page.child(status_strip(issue, theme, true));
    }
    if let Some(readback) = render_host_save_readback_summary(panel, theme) {
        page = page.child(readback);
    }
    if let Some(probe_status) = render_probe_status(panel, theme) {
        page = page.child(probe_status);
    }
    page = page.child(render_editor_section(
        panel,
        editor,
        active_section,
        disabled,
        theme,
        cx,
    ));
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

    let footer = div()
        .w_full()
        .min_h(rems(3.75))
        .flex_shrink_0()
        .px_4()
        .py_2()
        .when(short, |footer| footer.min_h(rems(2.5)).py_1())
        .flex()
        .flex_wrap()
        .items_center()
        .justify_end()
        .gap_2()
        .border_t_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(
            editor_cancel_button("connections-editor-footer-cancel", saving || reloading, cx)
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
            cx,
        ))
        .child(
            Button::new("connections-editor-save")
                .compact()
                .label(if saving {
                    crate::locale::text("保存中…", "Saving…")
                } else if panel.save_outcome_unknown {
                    crate::locale::text("结果待核对", "Verify save result")
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
            Some(editor),
            saving || reloading,
            disabled,
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

/// 持久本地主机与远程连接使用同一协议栏、双栏内容和固定 footer，避免本地功能再次分裂成独立页面。
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
    let title = match editor.mode {
        LocalHostEditorMode::Edit => crate::locale::text("编辑主机", "Edit host"),
        LocalHostEditorMode::Create | LocalHostEditorMode::Duplicate => {
            crate::locale::text("新建主机", "New host")
        }
    };
    let mut page = div()
        .id("connections-local-editor-page")
        .flex_1()
        .min_h(px(0.))
        .min_w(px(0.))
        .track_scroll(&editor.scroll_handle)
        .overflow_y_scroll()
        .vertical_scrollbar(&editor.scroll_handle)
        .p_5()
        .flex()
        .flex_col()
        .gap_3();
    if !panel.local_hosts.save_outcome_unknown
        && let Some(status) = panel
            .local_hosts
            .status
            .clone()
            .or_else(|| editor.validation_issue.clone())
    {
        page = page.child(status_strip(
            status,
            theme,
            editor.validation_issue.is_some(),
        ));
    }
    if editor.external_conflict {
        page = page.child(status_strip(
            crate::locale::text(
                "本地主机已被外部修改；当前草稿已保留，请重新读取并核对。",
                "This local host changed externally. Your draft is preserved; read it back and review before saving.",
            ).to_string(),
            theme,
            true,
        ));
    }
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
                    .h(rems(2.25))
                    .disabled(disabled)
                    .accessibility_label(crate::locale::text("主机分组", "Host group")),
                theme,
                disabled,
                &field_scope,
                cx,
            ))
            .child(field(
                crate::locale::text("Shell", "Shell"),
                local_shell_picker(panel, disabled, cx),
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
            .child(path_field(
                crate::locale::text("起始目录", "Working directory"),
                Input::new(&editor.cwd).disabled(disabled).w_full(),
                HostFilePickerTarget::LocalWorkingDirectory,
                disabled,
                theme,
                &field_scope,
                cx,
            ));
        if let Some(error) = panel.local_hosts.shell_error.as_ref() {
            properties = properties.child(authentication_note(error.clone(), theme));
        }
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
                    "启动参数每行一项；环境变量每行 NAME=value。",
                    "One argument per line; environment variables use NAME=value.",
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
    let footer = div()
        .w_full()
        .min_h(rems(3.75))
        .flex_shrink_0()
        .px_4()
        .py_2()
        .when(short, |footer| footer.min_h(rems(2.5)).py_1())
        .flex()
        .flex_wrap()
        .items_center()
        .justify_end()
        .gap_2()
        .border_t_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(
            editor_cancel_button("connections-local-editor-cancel", saving || reloading, cx)
                .label(crate::locale::text("取消", "Cancel")),
        )
        .children(render_conflict_reload_action(panel, cx))
        .child(
            Button::new("connections-local-editor-save")
                .compact()
                .label(if saving {
                    crate::locale::text("保存中…", "Saving…")
                } else if panel.local_hosts.save_outcome_unknown {
                    crate::locale::text("结果待核对", "Verify save result")
                } else {
                    crate::locale::text("保存", "Save")
                })
                .disabled(readonly || saving || reloading || panel.local_hosts.save_outcome_unknown)
                .when(
                    !readonly && !saving && !reloading && !panel.local_hosts.save_outcome_unknown,
                    Button::primary,
                )
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
        .child(editor_header(
            title,
            None,
            saving || reloading,
            disabled,
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
    let footer = div()
        .w_full()
        .min_h(px(58.))
        .px_4()
        .py_2()
        .when(short, |footer| footer.min_h(rems(2.5)).py_1())
        .flex()
        .items_center()
        .justify_end()
        .border_t_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(
            editor_cancel_button("connections-local-editor-load-cancel", false, cx)
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
        .child(editor_header(title, None, false, true, theme, short, cx))
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

/// 本地常用字段和多行启动项分开，窄窗通过同一 Kit 菜单选择分类。
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
            .px_4()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                Button::new("connections-local-editor-section-picker")
                    .w_full()
                    .secondary()
                    .dropdown_caret(true)
                    .disabled(disabled)
                    .label(local_section_label(selected))
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
            Button::new(format!("connections-local-editor-section-{section:?}"))
                .w_full()
                .h(rems(2.5))
                .compact()
                .icon(if section == LocalHostEditorSection::Properties {
                    IconName::Settings2
                } else {
                    IconName::SquareTerminal
                })
                .label(local_section_label(section))
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
        LocalHostEditorSection::Properties => crate::locale::text("属性", "Properties"),
        LocalHostEditorSection::Terminal => crate::locale::text("终端", "Terminal"),
    }
}

/// 分组创建复用已有 CAS 与未知结果恢复流程，主机草稿仍由编辑器所有者保管。
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
                Button::new("connections-editor-create-group")
                    .ghost()
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

/// 手动路径始终可编辑，原生选择器只回填当前请求所属草稿，不成为必需依赖。
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
            .child(div().flex_1().min_w_0().child(input.h(rems(2.25))))
            .child(
                Button::new(format!("host-path-picker-{target:?}"))
                    .ghost()
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

/// 候选由系统异步发现；发现失败或没有候选时仍保留自定义可执行文件输入。
fn local_shell_picker(
    panel: &ConnectionsPanel,
    disabled: bool,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let candidates = panel.local_hosts.shell_candidates.clone();
    let selected_program = panel
        .local_hosts
        .editor
        .as_ref()
        .map(|editor| editor.shell.read(cx).value().to_string())
        .unwrap_or_default();
    let selected_id = candidates
        .iter()
        .find(|candidate| candidate.program().to_string_lossy() == selected_program)
        .map(|candidate| candidate.id().to_owned());
    let selected_label = candidates
        .iter()
        .find(|candidate| Some(candidate.id()) == selected_id.as_deref())
        .map(|candidate| gpui_kit::SharedString::from(candidate.display_name().to_owned()))
        .unwrap_or_else(|| {
            if selected_program.is_empty() {
                crate::locale::text("默认终端 Profile", "Default terminal profile")
            } else {
                crate::locale::text("自定义 Shell", "Custom shell")
            }
        });
    let panel_entity = cx.entity().clone();
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .child(
            Button::new("connections-local-shell-picker")
                .flex_1()
                .secondary()
                .dropdown_caret(true)
                .label(selected_label)
                .disabled(disabled)
                .dropdown_menu(move |mut menu, _, _| {
                    let default_panel = panel_entity.clone();
                    menu = menu.item(
                        PopupMenuItem::new(crate::locale::text(
                            "使用默认终端 Profile",
                            "Use default terminal profile",
                        ))
                        .on_click(move |_, window, app| {
                            default_panel
                                .update(app, |panel, cx| panel.use_default_local_shell(window, cx));
                        }),
                    );
                    for candidate in &candidates {
                        let panel_entity = panel_entity.clone();
                        let candidate_id = candidate.id().to_owned();
                        menu = menu.item(
                            PopupMenuItem::new(candidate.display_name().to_owned())
                                .checked(Some(candidate.id()) == selected_id.as_deref())
                                .on_click(move |_, window, app| {
                                    panel_entity.update(app, |panel, cx| {
                                        panel.select_local_shell(&candidate_id, window, cx)
                                    });
                                }),
                        );
                    }
                    menu
                }),
        )
        .child(
            Button::new("connections-local-shell-refresh")
                .ghost()
                .icon(IconName::RefreshCw)
                .tooltip(crate::locale::text(
                    "重新发现 Shell",
                    "Refresh installed shells",
                ))
                .disabled(disabled || panel.local_hosts.shell_loading)
                .on_click(
                    cx.listener(|panel, _: &ClickEvent, _, cx| panel.discover_local_shells(cx)),
                ),
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

/// 标题栏收藏服从字段门禁；关闭保留独立出口，由面板统一裁决 dirty/write gate。
/// @author kongweiguang
fn editor_header(
    title: impl Into<gpui_kit::SharedString>,
    editor: Option<&HostEditor>,
    saving: bool,
    fields_disabled: bool,
    theme: ThemeTokens,
    short: bool,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let header = div()
        .w_full()
        .h(rems(3.5))
        .when(short, |header| header.h(rems(2.25)))
        .flex_shrink_0()
        .px_4()
        .flex()
        .items_center()
        .gap_3()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .justify_center()
                .text_size(theme.text_heading)
                .font_weight(gpui_kit::FontWeight::BOLD)
                .child(title.into()),
        );
    let header = if let Some(editor) = editor {
        header.child(
            Button::new("connections-editor-favorite")
                .compact()
                .ghost()
                .icon(IconName::Star)
                .accessibility_label(if editor.favorite {
                    crate::locale::text("取消收藏主机", "Remove host from favorites")
                } else {
                    crate::locale::text("收藏主机", "Add host to favorites")
                })
                .tooltip(if editor.favorite {
                    crate::locale::text("已收藏", "Favorited")
                } else {
                    crate::locale::text("收藏", "Add to favorites")
                })
                .disabled(fields_disabled)
                .selected(editor.favorite)
                .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| panel.toggle_favorite(cx))),
        )
    } else {
        header
    };
    header.child(
        editor_cancel_button("connections-editor-close", saving, cx)
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
        .compact()
        .disabled(saving)
        .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
            panel.cancel_editor(window, cx);
        }))
}

/// 仅在真实 probe 服务存在时绘制测试动作；取消、一次性信任与普通重试各有明确入口。
/// @author kongweiguang
fn render_probe_action(
    panel: &ConnectionsPanel,
    saving: bool,
    readonly: bool,
    cx: &mut Context<ConnectionsPanel>,
) -> Option<Button> {
    panel.probe_service.as_ref()?;
    let button = if panel.probe_running {
        Button::new("connections-editor-probe-cancel")
            .compact()
            .label(crate::locale::text("取消测试", "Cancel test"))
            .on_click(cx.listener(|panel, _: &ClickEvent, _, cx| {
                panel.cancel_connection_probe(cx);
            }))
    } else if matches!(
        panel.probe_outcome,
        Some(
            kerminal_application::connections::probe::ConnectionProbeOutcome::RequiresHostKeyTrust(
                _
            )
        )
    ) {
        Button::new("connections-editor-probe-trust")
            .compact()
            .label(crate::locale::text(
                "仅本次信任并重试",
                "Trust once and retry",
            ))
            .disabled(saving || readonly || panel.save_outcome_unknown)
            .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                panel.confirm_probe_host_key(window, cx);
            }))
    } else {
        Button::new("connections-editor-probe")
            .compact()
            .label(crate::locale::text("测试连接", "Test connection"))
            .disabled(saving || readonly || panel.save_outcome_unknown)
            .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                panel.test_connection(window, cx);
            }))
    };
    Some(button)
}

/// 基础连接字段始终在首屏；协议专属选项只在展开后用紧凑导航切换。
/// @author kongweiguang
fn render_editor_section(
    panel: &ConnectionsPanel,
    editor: &HostEditor,
    section: HostEditorSection,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> gpui_kit::AnyElement {
    let content = match section {
        HostEditorSection::Properties | HostEditorSection::Authentication => {
            render_properties(editor, disabled, theme, cx).into_any_element()
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
                    render_serial(panel, editor, disabled, theme, cx),
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
        .child(
            div()
                .pb_3()
                .border_b_1()
                .border_color(theme.border)
                .child(section_title(
                    protocol_section_label(editor.protocol, section),
                    theme,
                )),
        )
        .child(content)
        .into_any_element()
}

/// 将常用属性和高级协议项放在同一左侧目录，切换只改变当前表单视图。
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
        let panel_entity = cx.entity().clone();
        return div()
            .px_4()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                Button::new("connections-editor-section-picker")
                    .w_full()
                    .label(protocol_section_label(editor.protocol, selected))
                    .icon(editor_section_icon(selected))
                    .dropdown_caret(true)
                    .secondary()
                    .disabled(disabled)
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
                .w_full()
                .h(rems(2.5))
                .compact()
                .icon(editor_section_icon(section))
                .label(label)
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
    if protocol == RemoteProtocol::Serial && section == HostEditorSection::Terminal {
        crate::locale::text("串口", "Serial")
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

/// 将最常修改的主机属性平铺为对齐表单，分组与标签直接可见，认证就近归入属性页。
/// @author kongweiguang
fn render_properties(
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
                Input::new(&editor.inputs.name).disabled(disabled).w_full(),
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
                    .h(rems(2.25))
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
            fields = fields.child(with_validation_issue(
                render_ssh_auth(editor, has_target, has_passphrase, disabled, theme, cx),
                editor,
                HostEditorField::Authentication,
                theme,
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
        RemoteProtocol::Rdp | RemoteProtocol::Telnet | RemoteProtocol::Serial
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
            crate::locale::text("属性", "Properties"),
            crate::locale::text("名称、分组、地址与标签", "Name, group, endpoint and tags"),
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

/// 在滚动区域展示真实保存/probe 状态，底部动作栏保持固定不被长表单推走。
/// @author kongweiguang
fn render_probe_status(
    panel: &ConnectionsPanel,
    theme: ThemeTokens,
) -> Option<gpui_kit::AnyElement> {
    if let Some(error) = panel.probe_error.as_ref() {
        return Some(status_strip(error.clone(), theme, true).into_any_element());
    }
    let outcome = panel.probe_outcome.as_ref()?;
    let (message, warning) = probe_outcome_message(outcome);
    let mut card = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(if warning { theme.warning } else { theme.border })
        .bg(theme.panel)
        .text_size(theme.text_xs)
        .text_color(if warning {
            theme.warning
        } else {
            theme.foreground
        })
        .child(message);
    if let ConnectionProbeOutcome::RequiresHostKeyTrust(challenge) = outcome {
        card = card
            .child(format!(
                "{} · {}",
                challenge.algorithm, challenge.fingerprint_sha256
            ))
            .child(crate::locale::text(
                "此确认只对当前表单的下一次测试有效，不会写入 known_hosts。",
                "This confirmation applies only to the next test and does not write known_hosts.",
            ));
    }
    if let ConnectionProbeOutcome::HostKeyChanged(changed) = outcome {
        card = card
            .child(crate::locale::text(
                format!("旧指纹：{}", changed.previous_fingerprint_sha256),
                format!(
                    "Previous fingerprint: {}",
                    changed.previous_fingerprint_sha256
                ),
            ))
            .child(crate::locale::text(
                format!("新指纹：{}", changed.fingerprint_sha256),
                format!("Current fingerprint: {}", changed.fingerprint_sha256),
            ));
    }
    Some(card.into_any_element())
}

/// 将本机 RDP 启动能力与远端 TCP 结果并列呈现，不把端口可达描述为 RDP 认证成功。
/// @author kongweiguang
fn probe_outcome_message(
    outcome: &kerminal_application::connections::probe::ConnectionProbeOutcome,
) -> (gpui_kit::SharedString, bool) {
    use kerminal_application::connections::probe::{
        ConnectionProbeOutcome, ProbeVerificationLevel, RdpLocalAvailability, RdpTcpFailure,
        RdpTcpReachability,
    };
    match outcome {
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
        ConnectionProbeOutcome::RequiresHostKeyTrust(_) => (
            crate::locale::text(
                "此 SSH 主机密钥尚未核验。请检查下方指纹，再选择仅本次信任。",
                "This SSH host key is unknown. Review the fingerprint below before trusting it once.",
            ),
            true,
        ),
        ConnectionProbeOutcome::HostKeyChanged(_) => (
            crate::locale::text(
                "主机密钥已变化，连接测试已阻止。请通过独立的信任管理流程核验。",
                "The host key changed, so the test was blocked. Verify it through host trust management.",
            ),
            true,
        ),
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

/// 表单状态条使用语义色区分失败与普通进度，不改变固定动作栏布局。
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
        .p_3()
        .flex()
        .items_center()
        .justify_center()
        .bg(theme.background.opacity(0.8))
        .child(
            div()
                .id("connections-editor-discard-card")
                .w_full()
                .max_w(rems(25.0))
                .max_h_full()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_3()
                .p_5()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .bg(theme.panel)
                .text_color(theme.foreground)
                .child(section_title(title, theme))
                .child(message)
                .child({
                    let mut actions = div().w_full().flex().justify_end().gap_2().child(
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
    let options = [
        (Some(RemoteProtocol::Ssh), "SSH"),
        (Some(RemoteProtocol::Sftp), "SFTP"),
        (None, "Local"),
        (Some(RemoteProtocol::Rdp), "RDP"),
        (Some(RemoteProtocol::Telnet), "Telnet"),
        (Some(RemoteProtocol::Serial), "Serial"),
    ];
    if short {
        let owner = cx.entity().clone();
        let label = options
            .iter()
            .find(|(protocol, _)| *protocol == selected)
            .map_or("Local", |(_, label)| *label);
        return Button::new("connections-compact-protocol")
            .compact()
            .w_full()
            .secondary()
            .label(label)
            .dropdown_caret(true)
            .disabled(disabled || panel.editor_protocol_locked())
            .dropdown_menu(move |mut menu, _, _| {
                for (protocol, title) in options {
                    let owner = owner.clone();
                    menu = menu.item(
                        PopupMenuItem::new(title)
                            .checked(protocol == selected)
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
            let selected_item = if is_local {
                panel.active_editor_kind == Some(ConnectionsEditorKind::Local)
            } else {
                panel.active_editor_kind == Some(ConnectionsEditorKind::Remote)
                    && selected == protocol
            };
            choice_button(
                format!("connections-protocol-{protocol:?}"),
                title,
                selected_item,
                disabled || panel.editor_protocol_locked(),
                theme,
            )
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

/// 地址与端口共用一个字段组，避免端口再次占用完整标签列；串口设备由独立串口分类编辑。
/// @author kongweiguang
fn render_endpoint(editor: &HostEditor, disabled: bool, theme: ThemeTokens) -> impl IntoElement {
    let field_scope = host_editor_scope(editor, "endpoint");
    let mut fields = div().w_full().flex().flex_col().gap_2().min_w(px(0.));
    if editor.protocol != RemoteProtocol::Serial {
        fields = fields.child(field(
            crate::locale::text("主机地址", "Host address"),
            div()
                .w_full()
                .flex()
                .items_start()
                .gap_3()
                .child(
                    div().flex_1().min_w(px(0.)).child(with_validation_issue(
                        Input::new(&editor.inputs.host)
                            .h(rems(2.25))
                            .disabled(disabled)
                            .w_full(),
                        editor,
                        HostEditorField::Host,
                        theme,
                    )),
                )
                .child(
                    div()
                        .w(rems(endpoint_port_column_min_width_px() / 16.0))
                        .min_w(rems(4.0))
                        .flex_shrink_0()
                        .child(with_validation_issue(
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
                                        .h(rems(2.25))
                                        .disabled(disabled)
                                        .w_full()
                                        .aria_label(crate::locale::text("端口", "Port")),
                                ),
                            editor,
                            HostEditorField::Port,
                            theme,
                        )),
                ),
            theme,
            &field_scope,
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

/// SSH/SFTP 认证以轻量分隔承接主机字段；认证提示保留为正文，不再包入深色卡片。
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
        .h(rems(2.25))
        .compact()
        .secondary()
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
    let mut section = div().w_full().flex().flex_col().gap_2().child(field(
        crate::locale::text("认证", "Authentication"),
        mode_picker,
        theme,
        &field_scope,
    ));
    match mode {
        HostAuthenticationMode::Agent => {}
        HostAuthenticationMode::Password => {
            section = section.child(input_field(
                crate::locale::text("新密码", "New password"),
                Input::new(&editor.inputs.secret)
                    .disabled(disabled)
                    .w_full(),
                theme,
                &field_scope,
            ));
            section = section.child(credential_status(
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
                        cx.notify();
                    }
                }),
            ));
        }
        HostAuthenticationMode::PrivateKey => {
            section = section.child(path_field(
                crate::locale::text("私钥文件路径", "Private key file path"),
                Input::new(&editor.inputs.key_path)
                    .disabled(disabled)
                    .w_full(),
                HostFilePickerTarget::PrivateKey,
                disabled,
                theme,
                &field_scope,
                cx,
            ));
            section = section.child(with_validation_issue(
                multiline_field(
                    crate::locale::text("粘贴私钥", "Paste private key"),
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
            ));
            section = section.child(authentication_note(
                crate::locale::text(
                    "选择文件或粘贴私钥。粘贴内容仅存入加密 Vault；留空保留已保存的私钥。",
                    "Choose a file or paste a key. Pasted keys are stored only in the encrypted vault; leave empty to keep the saved key.",
                ),
                theme,
            ));
            section = section.child(credential_status(
                &field_scope,
                "target-private-key-clear",
                editor.has_target_private_key(),
                editor.clear_private_key,
                disabled,
                crate::locale::text("已保存私钥", "Saved private key"),
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
            section = section.child(input_field(
                crate::locale::text("新私钥口令", "New key passphrase"),
                Input::new(&editor.inputs.passphrase)
                    .disabled(disabled)
                    .w_full(),
                theme,
                &field_scope,
            ));
            section = section.child(credential_status(
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
                        cx.notify();
                    }
                }),
            ));
        }
    }
    section.into_any_element()
}

/// RDP 标签保持短小；可选、留空保留和客户端差异放在输入旁说明，避免窄列多行标签。
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
            "用户名可留空，由系统客户端提示输入。密码只进入加密 vault；未选择清除时，留空保留已保存密码。未保存密码时由客户端提示。",
            "Username is optional; the system client can prompt. Passwords are stored only in the encrypted vault. Leave password empty to keep a saved password unless removal is selected. The client can prompt when none is saved.",
        )
    } else {
        crate::locale::text(
            "用户名可留空。密码只进入加密 vault；未选择清除时，留空保留已保存密码。此平台由外部 RDP 客户端处理认证，保存的密码不会自动填入客户端。",
            "Username is optional. Passwords are stored only in the encrypted vault. Leave password empty to keep a saved password unless removal is selected. The external RDP client handles authentication on this platform; saved passwords are not entered automatically.",
        )
    };
    div()
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
        ))
        .child(input_field(
            crate::locale::text("密码", "Password"),
            Input::new(&editor.inputs.secret)
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
                    cx.notify();
                }
            }),
        ))
        .child(authentication_note(credential_note, theme))
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
    SftpEnabled,
    PreserveMtime,
    FollowSourceSymlinks,
    RdpFullscreen,
}

/// 条件字段开关同步交接焦点，避免关闭传输或全屏时按键留在隐藏输入；普通选项保留焦点。
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
    let row = Button::new(id)
        .compact()
        .w_full()
        .label(format!("{} {label}", if selected { "✓" } else { "○" }))
        .accessibility_label(crate::locale::text(
            format!("{label}，{}", if selected { "已开启" } else { "已关闭" }),
            format!("{label}, {}", if selected { "on" } else { "off" }),
        ))
        .selected(selected)
        .disabled(disabled)
        .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
            if let Some(editor) = panel.editor.as_mut() {
                match option {
                    HostOptionToggle::SftpEnabled => editor.sftp_enabled = !editor.sftp_enabled,
                    HostOptionToggle::PreserveMtime => {
                        editor.preserve_mtime = !editor.preserve_mtime
                    }
                    HostOptionToggle::FollowSourceSymlinks => {
                        editor.follow_source_symlinks = !editor.follow_source_symlinks
                    }
                    HostOptionToggle::RdpFullscreen => {
                        editor.rdp_fullscreen = !editor.rdp_fullscreen
                    }
                }
            }
            if matches!(
                option,
                HostOptionToggle::SftpEnabled | HostOptionToggle::RdpFullscreen
            ) {
                panel.focus_editor_primary_input(window, cx);
            }
            panel.reset_connection_probe();
            cx.notify();
        }));
    let row = scope.wrap(id, row);
    if matches!(option, HostOptionToggle::PreserveMtime) {
        row.debug_selector("connections-editor-preserve-mtime-row")
    } else {
        row
    }
}

/// 主机传输选项只改变后续任务；禁用 SFTP 不影响 SSH 终端或强杀在途传输。
///
/// @author kongweiguang
fn render_transfer_options(
    editor: &HostEditor,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let field_scope = host_editor_scope(editor, "transfer");
    div().w_full().flex().flex_col().gap_3()
        .child(host_option_toggle("host-sftp-enabled", crate::locale::text("启用 SFTP", "Enable SFTP"), editor.sftp_enabled, disabled, HostOptionToggle::SftpEnabled, &field_scope, cx))
        .child(path_field(crate::locale::text("本地起始目录", "Local start directory"), Input::new(&editor.inputs.sftp_local_directory).disabled(disabled || !editor.sftp_enabled).w_full(), HostFilePickerTarget::SftpLocalDirectory, disabled || !editor.sftp_enabled, theme, &field_scope, cx))
        .child(input_field(crate::locale::text("远端起始目录", "Remote start directory"), Input::new(&editor.inputs.sftp_remote_directory).disabled(disabled || !editor.sftp_enabled).w_full(), theme, &field_scope))
        .child(network_option_input_field(crate::locale::text("并发数", "Concurrency"), crate::locale::text("1–8，留空继承全局", "1–8; leave empty to inherit the global setting"), Input::new(&editor.inputs.sftp_concurrency).disabled(disabled || !editor.sftp_enabled).w_full(), theme, &field_scope))
        .child(host_option_toggle("host-sftp-mtime", crate::locale::text("保留修改时间", "Preserve modification time"), editor.preserve_mtime, disabled || !editor.sftp_enabled, HostOptionToggle::PreserveMtime, &field_scope, cx))
        .child(host_option_toggle("host-sftp-follow", crate::locale::text("跟随源端符号链接", "Follow source symbolic links"), editor.follow_source_symlinks, disabled || !editor.sftp_enabled, HostOptionToggle::FollowSourceSymlinks, &field_scope, cx))
        .child(note_card(crate::locale::text("仅源端解引用并检查范围和环路；目标写入及删除始终不跟随链接。并发仍受全局上限约束。", "Only source links are dereferenced with scope and cycle checks; destination writes/deletes never follow links. The global concurrency cap still applies."), theme))
}

/// 窗口尺寸保留用户输入但只在窗口模式应用；成对校验由应用层统一执行。
///
/// @author kongweiguang
fn render_rdp_display(
    editor: &HostEditor,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let field_scope = host_editor_scope(editor, "display");
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_3()
        .child(host_option_toggle(
            "host-rdp-fullscreen",
            crate::locale::text("全屏打开", "Open fullscreen"),
            editor.rdp_fullscreen,
            disabled,
            HostOptionToggle::RdpFullscreen,
            &field_scope,
            cx,
        ))
        .child(network_option_input_field(
            crate::locale::text("窗口宽度", "Window width"),
            crate::locale::text("640–8192 像素", "640–8192 pixels"),
            Input::new(&editor.inputs.rdp_width)
                .disabled(disabled || editor.rdp_fullscreen)
                .w_full(),
            theme,
            &field_scope,
        ))
        .child(network_option_input_field(
            crate::locale::text("窗口高度", "Window height"),
            crate::locale::text("480–8192 像素", "480–8192 pixels"),
            Input::new(&editor.inputs.rdp_height)
                .disabled(disabled || editor.rdp_fullscreen)
                .w_full(),
            theme,
            &field_scope,
        ))
        .child(note_card(
            crate::locale::text(
                "尺寸同时留空时由系统客户端决定。",
                "Leave both dimensions empty to use the system client default.",
            ),
            theme,
        ))
}

/// 显示可选串口设备和受限线制；候选按钮逐项绑定焦点行，避免换行设备簇被整体测量。
/// 枚举不可用时仍保留手动设备路径与清晰状态反馈。
/// @author kongweiguang
fn render_serial(
    panel: &ConnectionsPanel,
    editor: &HostEditor,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let field_scope = host_editor_scope(editor, "serial");
    let selected_device = editor.inputs.host.read(cx).value().trim().to_owned();
    let mut devices = div().w_full().flex().flex_wrap().gap_2();
    for port in &panel.serial_ports {
        let name = port.name.clone();
        let details = [port.manufacturer.as_deref(), port.product.as_deref()]
            .into_iter()
            .flatten()
            .filter(|value| !value.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        let label = if details.is_empty() {
            port.name.clone()
        } else {
            format!("{} · {}", port.name, details)
        };
        devices = devices.child(
            field_scope.wrap(
                format!("serial-device:{}", port.name),
                Button::new(format!("connections-serial-port-{}", port.name))
                    .compact()
                    .selected(selected_device == port.name)
                    .label(label)
                    .disabled(disabled)
                    .on_click(cx.listener(move |panel, _: &ClickEvent, window, cx| {
                        panel.select_serial_port(name.clone(), window, cx);
                    })),
            ),
        );
    }
    let state = serial_port_list_state(
        panel.serial_port_service.is_some(),
        panel.serial_ports_loading,
        panel.serial_ports_loaded,
        panel.serial_ports_issue,
        panel.serial_ports.len(),
    );
    let state_text = match state {
        SerialPortListState::ServiceUnavailable => crate::locale::text(
            "串口设备列表当前不可用；仍可手工填写设备路径。",
            "Serial device enumeration is unavailable; you can still enter a device path manually.",
        ),
        SerialPortListState::Loading => {
            crate::locale::text("正在刷新串口设备…", "Refreshing serial devices…")
        }
        SerialPortListState::Cancelled => crate::locale::text(
            "串口设备枚举已取消；可手工填写路径或重新刷新。",
            "Serial device enumeration was cancelled; enter a path manually or refresh again.",
        ),
        SerialPortListState::Failed => crate::locale::text(
            "无法枚举串口设备。请检查设备连接和系统访问权限后重试，或手工填写路径。",
            "Serial devices could not be enumerated. Check the device connection and system access, then retry or enter a path manually.",
        ),
        SerialPortListState::NotEnumerated => crate::locale::text(
            "尚未枚举设备；可以手工填写设备路径。",
            "Devices have not been enumerated yet; you can enter a device path manually.",
        ),
        SerialPortListState::Empty => crate::locale::text(
            "未发现串口设备。连接设备后刷新，或手工填写设备路径。",
            "No serial devices were found. Connect a device and refresh, or enter a device path manually.",
        ),
        SerialPortListState::Available => crate::locale::text(
            "选择当前检测到的设备，或继续手工填写路径。设备可能随时断开。",
            "Choose a detected device or keep entering a path manually. Devices may disconnect at any time.",
        ),
    };

    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(section_title(
                    crate::locale::text("可用设备", "Available devices"),
                    theme,
                ))
                .child(
                    field_scope.wrap(
                        "serial-refresh",
                        Button::new("connections-serial-refresh")
                            .compact()
                            .label(if panel.serial_ports_loading {
                                crate::locale::text("刷新中…", "Refreshing…")
                            } else {
                                crate::locale::text("刷新设备", "Refresh devices")
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
        )
        .child(note_card(state_text, theme))
        .when(state == SerialPortListState::Available, |section| {
            section.child(devices)
        })
        .child(with_validation_issue(
            input_field(
                crate::locale::text("设备路径", "Device path"),
                Input::new(&editor.inputs.host).disabled(disabled).w_full(),
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
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .gap_3()
                        .child(
                            div().flex_1().min_w(px(0.)).child(field(
                                crate::locale::text("数据位", "Data bits"),
                                Select::new(&editor.serial_data_bits_select)
                                    .w_full()
                                    .h(rems(2.25))
                                    .disabled(disabled)
                                    .accessibility_label(crate::locale::text(
                                        "数据位",
                                        "Data bits",
                                    )),
                                theme,
                                &field_scope,
                            )),
                        )
                        .child(
                            div().flex_1().min_w(px(0.)).child(field(
                                crate::locale::text("停止位", "Stop bits"),
                                Select::new(&editor.serial_stop_bits_select)
                                    .w_full()
                                    .h(rems(2.25))
                                    .disabled(disabled)
                                    .accessibility_label(crate::locale::text(
                                        "停止位",
                                        "Stop bits",
                                    )),
                                theme,
                                &field_scope,
                            )),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .gap_3()
                        .child(
                            div().flex_1().min_w(px(0.)).child(field(
                                crate::locale::text("校验", "Parity"),
                                Select::new(&editor.serial_parity_select)
                                    .w_full()
                                    .h(rems(2.25))
                                    .disabled(disabled)
                                    .accessibility_label(crate::locale::text("校验方式", "Parity")),
                                theme,
                                &field_scope,
                            )),
                        )
                        .child(
                            div().flex_1().min_w(px(0.)).child(field(
                                crate::locale::text("流控", "Flow control"),
                                Select::new(&editor.serial_flow_select)
                                    .w_full()
                                    .h(rems(2.25))
                                    .disabled(disabled)
                                    .accessibility_label(crate::locale::text(
                                        "流控方式",
                                        "Flow control",
                                    )),
                                theme,
                                &field_scope,
                            )),
                        ),
                ),
        )
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
/// @author kongweiguang
fn render_proxy_route(
    editor: &HostEditor,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let field_scope = host_editor_scope(editor, "proxy");
    let has_proxy_password = editor.has_proxy_password(cx);
    let mut section = div().w_full().flex().flex_col().gap_2().child(
        field_scope.wrap(
            "proxy-toggle",
            Button::new("connections-proxy-toggle")
                .compact()
                .label(if editor.proxy_enabled {
                    crate::locale::text("✓ 使用代理", "✓ Proxy enabled")
                } else {
                    crate::locale::text("使用代理", "Enable proxy")
                })
                .disabled(disabled)
                .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                    if let Some(editor) = panel.editor.as_mut() {
                        editor.proxy_enabled = !editor.proxy_enabled;
                        panel.focus_editor_primary_input(window, cx);
                        cx.notify();
                    }
                })),
        ),
    );
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
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(input_field(
                        crate::locale::text("代理端口", "Proxy port"),
                        Input::new(&editor.inputs.proxy_port)
                            .disabled(disabled)
                            .w_full(),
                        theme,
                        &field_scope,
                    ))
                    .child(input_field(
                        crate::locale::text("代理用户名", "Proxy username"),
                        Input::new(&editor.inputs.proxy_username)
                            .disabled(disabled)
                            .w_full(),
                        theme,
                        &field_scope,
                    )),
            )
            .child(input_field(
                crate::locale::text("新代理密码", "New proxy password"),
                Input::new(&editor.inputs.proxy_password)
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
                        cx.notify();
                    }
                }),
            ))
            .child(note_card(
                crate::locale::text(
                    "代理密码只写入加密 vault。密码留空会保留现有引用；用户名和密码都填写后才会启用代理认证。",
                    "The proxy password is stored only in the encrypted vault. An empty password keeps the saved reference; proxy authentication is used only when both username and password are present.",
                ),
                theme,
            ));
    }
    section
}

/// 跳板配置保持原有顺序与凭据槽位，只在用户打开跳板页时占用编辑区域。
/// @author kongweiguang
fn render_jump_hosts(
    editor: &HostEditor,
    disabled: bool,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> impl IntoElement {
    let field_scope = host_editor_scope(editor, "jump-hosts");
    let mut section = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .rounded_md()
        .bg(theme.panel)
        .child(section_title(
            crate::locale::text("跳板链", "Jump host chain"),
            theme,
        ));
    section = section.child(
        div()
            .w_full()
            .flex()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .text_size(px(14.))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(crate::locale::text("跳板链", "Jump host chain")),
            )
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
                if let Some(editor) = panel.editor.as_mut() {
                    editor.proxy_protocol = protocol;
                    cx.notify();
                }
            }))
        }),
    )
}

/// 跳板凭据独立占用槽位；切换认证先交接到保留的地址字段，避免秘密输入卸载后丢失按键。
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
    let original = jump.original_authentication_ref();
    let mut card = div()
        .w_full()
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
                        "remove",
                        Button::new(format!("connections-remove-jump-{index}"))
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
                .flex()
                .gap_3()
                .child(div().flex_1().child(input_field(
                    crate::locale::text("名称", "Name"),
                    Input::new(&jump.name).disabled(disabled).w_full(),
                    theme,
                    &field_scope,
                )))
                .child(div().flex_1().child(input_field(
                    crate::locale::text("地址", "Address"),
                    Input::new(&jump.host).disabled(disabled).w_full(),
                    theme,
                    &field_scope,
                )))
                .child(div().w(px(96.)).child(input_field(
                    crate::locale::text("端口", "Port"),
                    Input::new(&jump.port).disabled(disabled).w_full(),
                    theme,
                    &field_scope,
                )))
                .child(div().flex_1().child(input_field(
                    crate::locale::text("用户名", "Username"),
                    Input::new(&jump.username).disabled(disabled).w_full(),
                    theme,
                    &field_scope,
                ))),
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
                    choice_button(
                        format!("connections-jump-{index}-auth-{mode:?}"),
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
                crate::locale::text("新跳板密码", "New jump host password"),
                Input::new(&jump.secret).disabled(disabled).w_full(),
                theme,
                &field_scope,
            ));
            if jump.has_password() {
                card = card.child(
                    field_scope.wrap(
                        "clear-password",
                        Button::new(format!("connections-jump-{index}-clear-password"))
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
                                    cx.notify();
                                }
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
                    crate::locale::text("新私钥口令", "New key passphrase"),
                    Input::new(&jump.passphrase).disabled(disabled).w_full(),
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
                        Button::new(format!("connections-jump-{index}-clear-passphrase"))
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
                                    cx.notify();
                                }
                            })),
                    ),
                );
            }
        }
    }
    card
}

/// 将应用公开错误放到对应输入附近，未能可靠定位的错误仍由表单状态区承接。
/// @author kongweiguang
fn with_validation_issue(
    content: impl IntoElement,
    editor: &HostEditor,
    field: HostEditorField,
    theme: ThemeTokens,
) -> gpui_kit::AnyElement {
    let mut row = div()
        .id(("connections-validation-field", field as usize))
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(content)
        .when(field == HostEditorField::Username, |row| {
            row.debug_selector(|| "connections-editor-username-row".into())
        });
    if let Some(issue) = editor
        .validation_issue
        .as_ref()
        .filter(|issue| issue.field == field)
    {
        row = row
            .anchor_scroll(Some(editor.validation_anchor.clone()))
            .child(
                div()
                    .w_full()
                    .text_size(theme.text_xs)
                    .text_color(theme.error)
                    .child(issue.message.clone()),
            );
    }
    row.into_any_element()
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

/// 输入和标签使用相对尺寸，保持界面缩放后的密度及窄窗标签上置行为一致。
/// @author kongweiguang
fn input_field(
    label: impl Into<gpui_kit::SharedString>,
    input: Input,
    theme: ThemeTokens,
    scope: &EditorFieldScope,
) -> impl IntoElement {
    let input = gpui_kit::Styled::h(input, rems(2.25));
    let input = gpui_kit::Styled::text_sm(input);
    field(label, input, theme, scope)
}

/// 网络超时与心跳的主标签独立可读，范围/单位降为说明，数字控件不再铺满整行。
/// 输入保留弹性以适应窄视口，宽度不足时由控件收缩而不挤压标签或固定 footer。
/// @author kongweiguang
fn network_option_input_field(
    label: impl Into<gpui_kit::SharedString>,
    hint: impl Into<gpui_kit::SharedString>,
    input: Input,
    theme: ThemeTokens,
    scope: &EditorFieldScope,
) -> impl IntoElement {
    let (_, control_max_width) = network_field_widths_px();
    field(
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
                    .child(input.h(rems(2.25))),
            )
            .child(authentication_note(hint, theme)),
        theme,
        scope,
    )
}

/// 绘制表单分区的次级标题，让平面布局仍有清楚且一致的层级。
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

/// 轻量提示说明真实协议能力和秘密保存边界，不显示静态装饰型未接线操作。
fn note_card(message: impl Into<gpui_kit::SharedString>, theme: ThemeTokens) -> impl IntoElement {
    div()
        .w_full()
        .p_3()
        .rounded_sm()
        .bg(theme.sidebar)
        .text_xs()
        .text_color(theme.muted)
        .child(message.into())
}

/// 同时表达凭据状态且单独绑定清除按钮的焦点行；独立身份避免同页多个秘密槽共用 reveal 状态。
/// 点击回调遵守 Kit 的 App 级事件契约，不读取 vault。
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
    let label = label.into();
    let mut status =
        div()
            .w_full()
            .flex()
            .items_center()
            .gap_3()
            .child(div().flex_1().text_xs().child(if exists && !clearing {
                crate::locale::text("已关联加密凭据", "Encrypted credential linked")
            } else {
                crate::locale::text("未保存凭据", "No saved credential")
            }));
    if exists {
        status = status.child(
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
        );
    }
    status
}
