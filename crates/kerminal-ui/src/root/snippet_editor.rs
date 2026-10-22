//! 命令片段编辑器的全工作台模态宿主与唯一挂载策略。
//!
//! @author kongweiguang

use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::{
    AnyElement, ClickEvent, Context, Entity, IntoElement, StatefulInteractiveElement as _, Window,
    div, prelude::*, px,
};

use crate::features::commands::{CommandsView, SnippetTerminalFillCompleted};
use crate::theme::ThemeTokens;
use kerminal_application::preferences::ToolRailToolId;

use super::RootView;

/// 成功回执只接管仍可见片段面板的原终端；同一 UI 回调内校验后聚焦，避免延迟帧抢走新导航或弹层焦点。
/// 完整 owner/generation 与点击时焦点共同约束意图，写入成功本身不能授权激活旧 Tab。
/// 编辑弹层已由专用门禁核验；普通终端输入解析会拒绝所有弹层，不能再用它否决这次明确的焦点交回。
///
/// @author kongweiguang
pub(super) fn focus_terminal_after_fill(
    root: &mut RootView,
    commands: &Entity<CommandsView>,
    event: &SnippetTerminalFillCompleted,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    if root.center_panel != super::CenterPanel::Terminal
        || root.tool_panel_focus_is_blocked_except_snippet_editor(
            window,
            cx,
            event.snippet_editor_generation.is_some(),
        )
        || root.quick_open_modal_open
        || window.focused(cx) != event.expected_focus
        || root
            .commands_view
            .as_ref()
            .is_none_or(|current| current.entity_id() != commands.entity_id())
        || !commands
            .read(cx)
            .snippet_terminal_fill_completion_is_current(event)
        || root.current_commands_target(cx).0.as_ref() != Some(&event.binding)
        || (event.snippet_editor_generation.is_none()
            && root.logical_focused_terminal_view_binding(cx).as_ref() != Some(&event.binding))
    {
        return;
    }
    let panels = root
        .tool_rail_state
        .clone()
        .project(&root.current_tool_rail_settings())
        .panels;
    if ![panels.attached, panels.left, panels.bottom, panels.center]
        .contains(&Some(ToolRailToolId::Snippets))
    {
        return;
    }
    let Some(handler) = root
        .pane_input_handlers
        .get(&event.binding.session_id)
        .filter(|registration| registration.binding == event.binding)
        .map(|registration| registration.entity.clone())
    else {
        return;
    };
    let Some(tools) = root.workspace_tools.get(&event.binding.session_id).cloned() else {
        return;
    };
    if event.snippet_editor_generation.is_some() {
        commands.update(cx, |view, cx| view.close_snippet_editor(cx));
        if commands.read(cx).snippet_editor_open() {
            return;
        }
        sync_visibility(root, false, false, window, cx);
        // 本次 Fill 接管焦点，取消关闭弹层时已捕获的下一帧旧按钮焦点恢复。
        root.snippet_editor_modal_generation = root.snippet_editor_modal_generation.wrapping_add(1);
    }
    root.focus_terminal_canvas_target(&event.binding, handler, tools, window, cx);
}

/// 从 CommandsView 读取真实弹层状态，不复制草稿或维护第二份开关。
///
/// @author kongweiguang
pub(super) fn is_open(root: &RootView) -> bool {
    root.snippet_editor_modal_open
}

/// 打开时先把焦点交给弹层陷阱；关闭时只恢复仍在工作区焦点树中的旧焦点，隐藏目标改聚焦工作区。
/// 代次和模态门禁避免延迟回调抢回焦点；打开即撤销 pane 意图，关闭后同句柄也不复活旧请求。
///
/// @author kongweiguang
pub(super) fn sync_visibility(
    root: &mut RootView,
    open: bool,
    focus_command_draft: bool,
    window: &mut Window,
    cx: &mut Context<RootView>,
) {
    if root.snippet_editor_modal_open == open {
        return;
    }

    root.snippet_editor_modal_generation = root.snippet_editor_modal_generation.wrapping_add(1);
    let generation = root.snippet_editor_modal_generation;
    root.snippet_editor_modal_open = open;
    if open {
        root.clear_pending_pane_focus();
        root.snippet_editor_modal_return_focus = window.focused(cx);
        window.focus(&root.snippet_editor_modal_focus_handle, cx);
        cx.notify();
        let root_entity = cx.entity();
        window.on_next_frame(move |window, app| {
            root_entity.update(app, |root, cx| {
                if !root.snippet_editor_modal_open
                    || root.snippet_editor_modal_generation != generation
                    || root.custom_agent_manager.is_some()
                    || root.preferences_modal_open
                    || root.connections_modal_open
                {
                    return;
                }
                if let Some(commands) = root.commands_view.clone() {
                    commands.update(cx, |commands, cx| {
                        commands.focus_snippet_editor(window, cx);
                    });
                }
            });
        });
    } else {
        let return_focus = root.snippet_editor_modal_return_focus.take();
        cx.notify();
        if focus_command_draft || return_focus.is_some() {
            let root_entity = cx.entity();
            window.on_next_frame(move |window, app| {
                root_entity.update(app, |root, cx| {
                    if root.snippet_editor_modal_open
                        || root.snippet_editor_modal_generation != generation
                        || root.custom_agent_manager.is_some()
                        || root.preferences_modal_open
                        || root.connections_modal_open
                        || window.has_active_prompt()
                    {
                        return;
                    }
                    if focus_command_draft {
                        let focused_command = root.commands_view.clone().is_some_and(|commands| {
                            commands
                                .update(cx, |commands, cx| commands.focus_command_draft(window, cx))
                        });
                        if focused_command {
                            return;
                        }
                    }
                    if let Some(return_focus) = return_focus {
                        if root.focus_handle.contains(&return_focus, window) {
                            window.focus(&return_focus, cx);
                        } else {
                            window.focus(&root.focus_handle, cx);
                        }
                    } else {
                        window.focus(&root.focus_handle, cx);
                    }
                });
            });
        }
    }
}

/// 把 Escape 交回 Commands 的查询清除与草稿关闭门禁，不在 Root 重写编辑器语义。
///
/// @author kongweiguang
pub(super) fn handle_escape(root: &mut RootView, cx: &mut Context<RootView>) {
    if let Some(commands) = root.commands_view.clone() {
        commands.update(cx, |commands, cx| commands.handle_snippet_editor_escape(cx));
    }
}

/// 将编辑对话框提升到工作台根层；基础内容自然定高，长正文在视口上限内滚动，固定操作保持可见。
///
/// @author kongweiguang
pub(super) fn render_overlay(
    root: &mut RootView,
    theme: ThemeTokens,
    window: &mut Window,
    cx: &mut Context<RootView>,
) -> Option<AnyElement> {
    let commands = root.commands_view.clone()?;
    if !root.snippet_editor_modal_open || !commands.read(cx).snippet_editor_open() {
        return None;
    }

    let dialog = commands.update(cx, |commands, cx| {
        commands
            .render_snippet_editor_modal(theme, window, cx)
            .into_any_element()
    });
    let backdrop_commands = commands.clone();
    let focus_handle = root.snippet_editor_modal_focus_handle.clone();
    // 覆盖层上下各留 16px；矮窗口同步缩小弹层上限，避免内容越过可视区。
    let max_surface_height = px((window.viewport_size().height.as_f32() - 32.0).clamp(0.0, 800.0));

    Some(
        div()
            .id("workbench-snippet-editor-modal-overlay")
            .absolute()
            .inset_0()
            .occlude()
            .p(px(16.))
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui_kit::hsla(0.0, 0.0, 0.0, 0.38))
            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                backdrop_commands.update(cx, |commands, cx| {
                    commands.close_snippet_editor(cx);
                });
            }))
            .child(
                div()
                    .id("workbench-snippet-editor-modal-surface")
                    .w_full()
                    .max_w(px(672.))
                    .max_h(max_surface_height)
                    .min_w(px(0.))
                    .min_h(px(0.))
                    .occlude()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.panel)
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(dialog)
                    .focus_trap("workbench-snippet-editor-focus-trap", &focus_handle),
            )
            .into_any_element(),
    )
}

/// 在编辑弹层占用唯一 CommandsView 时提供无交互占位，绝不构造第二份编辑表单。
///
/// @author kongweiguang
pub(super) fn suspended_renderer() -> AnyElement {
    div()
        .id("workbench-snippet-editor-suspended-slot")
        .size_full()
        .into_any_element()
}
