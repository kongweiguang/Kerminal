//! 历史填入复用可见终端 writer，焦点返回由 Root 核对当前标签、目标和用户焦点意图。
//!
//! @author kongweiguang

use std::sync::Arc;

use gpui_kit::{AppContext as _, Context, Entity, Subscription, Window};
use kerminal_application::{commands::SnippetExecutionService, preferences::ToolRailToolId};

use crate::features::commands::{LogsTerminalFillCompleted, LogsView};

use super::{AppServices, CenterPanel, RootView};

impl RootView {
    /// 每个标签独立保留历史展示状态，但填入仍使用同一应用服务与 session writer。
    ///
    /// @author kongweiguang
    pub(super) fn create_logs_view(
        services: &AppServices,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<LogsView>> {
        let commands = services.commands.clone()?;
        let fill = Arc::new(SnippetExecutionService::new(
            commands.clone(),
            services.sessions.clone(),
        ));
        let executor = services.executor.clone();
        Some(cx.new(|cx| {
            let mut view = LogsView::new(commands, executor, window, cx);
            view.set_terminal_fill_service(Some(fill), cx);
            view
        }))
    }

    /// 订阅随历史 Entity 在标签缓存间转移，旧面板回执不能抢当前终端的焦点。
    ///
    /// @author kongweiguang
    pub(super) fn subscribe_logs_view(
        view: Option<&Entity<LogsView>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Subscription> {
        let view = view?;
        Some(cx.subscribe_in(
            view,
            window,
            |root, source, event: &LogsTerminalFillCompleted, window, cx| {
                if root.center_panel != CenterPanel::Terminal
                    || !root.tab_tool_contents_are_current()
                    || root.tool_panel_focus_is_blocked_except_snippet_editor(window, cx, false)
                    || root.quick_open_modal_open
                    || window.focused(cx) != event.expected_focus
                    || root
                        .logs_view
                        .as_ref()
                        .is_none_or(|view| view.entity_id() != source.entity_id())
                    || !source.read(cx).terminal_fill_completion_is_current(event)
                    || root.logical_focused_terminal_view_binding(cx).as_ref()
                        != Some(&event.binding)
                {
                    return;
                }
                let panels = root
                    .tool_rail_state
                    .clone()
                    .project(&root.current_tool_rail_settings())
                    .panels;
                if ![panels.attached, panels.left, panels.bottom, panels.center]
                    .contains(&Some(ToolRailToolId::Logs))
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
                let Some(tools) = root.workspace_tools.get(&event.binding.session_id).cloned()
                else {
                    return;
                };
                root.focus_terminal_canvas_target(&event.binding, handler, tools, window, cx);
            },
        ))
    }
}
