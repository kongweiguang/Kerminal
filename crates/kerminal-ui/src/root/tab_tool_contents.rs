//! 标签工具随工作区保活；容器导航按 main 的 host/runtime 作用域保留在 Root。
//!
//! @author kongweiguang

use std::collections::{BTreeMap, HashSet};

use gpui_kit::{AppContext as _, Context, Entity, Subscription, Window};
use kerminal_application::preferences::ToolRailToolId;

use crate::features::{
    commands::{
        CommandsView, LogsView, SnippetCurrentCommandRequested, SnippetEditorVisibilityChanged,
    },
    files::FilesPane,
    operations::{OperationsView, proxy_panel::ProxyApplicationPanel},
};

use super::{AppServices, RootView, snippet_editor, workspace_files};

#[derive(Default)]
pub(super) struct RetainedTabToolContents {
    commands_view: Option<Entity<CommandsView>>,
    snippet_editor_subscription: Option<Subscription>,
    snippet_fill_subscription: Option<Subscription>,
    snippet_current_command_subscription: Option<Subscription>,
    logs_view: Option<Entity<LogsView>>,
    sftp_panel: Option<Entity<FilesPane>>,
    sftp_panel_subscription: Option<workspace_files::FilesPaneSubscriptions>,
    operations_views: BTreeMap<ToolRailToolId, Entity<OperationsView>>,
    operations_subscriptions: BTreeMap<ToolRailToolId, Subscription>,
    proxy_panels: BTreeMap<ToolRailToolId, Entity<ProxyApplicationPanel>>,
    proxy_subscriptions: BTreeMap<ToolRailToolId, Subscription>,
}

impl RetainedTabToolContents {
    /// 切出标签先释放确认并停用读取；容器实体留在 Root，避免同宿主文件 Tab 丢失项目导航。
    ///
    /// @author kongweiguang
    fn take_from_root(
        root: &mut RootView,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) -> Self {
        // 切换标签不等同于关闭草稿，也不能让旧弹层下一帧恢复焦点到新标签。
        root.snippet_editor_modal_return_focus = None;
        snippet_editor::sync_visibility(root, false, false, window, cx);
        if let Some(commands) = root.commands_view.clone() {
            commands.update(cx, |view, cx| {
                view.cancel_pending_snippet_command_request(cx)
            });
        }
        if let Some(view) = root.logs_view.clone() {
            view.update(cx, |view, cx| view.set_active(false, cx));
        }
        if let Some(panel) = root.sftp_panel.clone() {
            root.sync_file_delete_root_confirmation(panel, false, window, cx);
        }
        let mut operations_views = std::mem::take(&mut root.tool_rail_operations_views);
        for view in operations_views.values() {
            view.update(cx, |view, cx| view.set_active(false, cx));
            root.sync_operations_root_confirmation(view.clone(), false, window, cx);
        }
        let mut operations_subscriptions =
            std::mem::take(&mut root._tool_rail_operations_subscriptions);
        // 开关集合仍按 Tab；容器详情只由后续 host/runtime 投影决定是否重置。
        if let Some(containers) = operations_views.remove(&ToolRailToolId::Containers) {
            root.tool_rail_operations_views
                .insert(ToolRailToolId::Containers, containers);
        }
        if let Some(subscription) = operations_subscriptions.remove(&ToolRailToolId::Containers) {
            root._tool_rail_operations_subscriptions
                .insert(ToolRailToolId::Containers, subscription);
        }

        Self {
            commands_view: root.commands_view.take(),
            snippet_editor_subscription: root._snippet_editor_visibility_subscription.take(),
            snippet_fill_subscription: root._snippet_terminal_fill_subscription.take(),
            snippet_current_command_subscription: root._snippet_current_command_subscription.take(),
            logs_view: root.logs_view.take(),
            sftp_panel: root.sftp_panel.take(),
            sftp_panel_subscription: root._sftp_panel_subscriptions.take(),
            operations_views,
            operations_subscriptions,
            proxy_panels: std::mem::take(&mut root.tool_rail_proxy_panels),
            proxy_subscriptions: std::mem::take(&mut root._tool_rail_proxy_panel_subscriptions),
        }
    }

    /// 恢复标签已有实体并保留 Root 的容器实体，使同 host/runtime 切换与 main 的稳定挂载一致。
    ///
    /// @author kongweiguang
    fn restore_into_root(
        self,
        root: &mut RootView,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        root.commands_view = self.commands_view;
        root._snippet_editor_visibility_subscription = self.snippet_editor_subscription;
        root._snippet_terminal_fill_subscription = self.snippet_fill_subscription;
        root._snippet_current_command_subscription = self.snippet_current_command_subscription;
        root.logs_view = self.logs_view;
        if root.commands_view.is_none() {
            root.commands_view = RootView::create_commands_view(&root.services, window, cx);
            (
                root._snippet_editor_visibility_subscription,
                root._snippet_terminal_fill_subscription,
                root._snippet_current_command_subscription,
            ) = RootView::subscribe_commands_view(root.commands_view.as_ref(), window, cx);
        }
        if root.logs_view.is_none() {
            root.logs_view = root.services.commands.clone().map(|commands| {
                let executor = root.services.executor.clone();
                cx.new(|cx| LogsView::new(commands, executor, window, cx))
            });
        }
        root.sftp_panel = self.sftp_panel;
        root._sftp_panel_subscriptions = self.sftp_panel_subscription;
        root.tool_rail_operations_views
            .extend(self.operations_views);
        root._tool_rail_operations_subscriptions
            .extend(self.operations_subscriptions);
        root.tool_rail_proxy_panels = self.proxy_panels;
        root._tool_rail_proxy_panel_subscriptions = self.proxy_subscriptions;

        if let Some(panel) = root.sftp_panel.clone() {
            let density = super::preferences::current_interface_density(root);
            panel.update(cx, |panel, cx| {
                panel.set_interface_density(density, cx);
                panel.refresh_locale(window, cx);
            });
        }
        let theme_mode = root.theme_mode;
        if let Some(view) = root.commands_view.clone() {
            view.update(cx, |view, cx| {
                view.set_theme_mode(theme_mode, cx);
                view.refresh_locale(window, cx);
            });
            snippet_editor::sync_visibility(
                root,
                view.read(cx).snippet_overlay_open(),
                false,
                window,
                cx,
            );
        }
        if let Some(view) = root.logs_view.clone() {
            view.update(cx, |view, cx| {
                view.set_theme_mode(theme_mode, cx);
                view.refresh_locale(window, cx);
            });
        }
        let operation_views = root
            .tool_rail_operations_views
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for view in operation_views {
            view.update(cx, |view, cx| {
                view.set_theme_mode(theme_mode, cx);
                view.refresh_locale(window, cx);
            });
        }
        root.refresh_proxy_panel(cx);
    }
}

impl RootView {
    /// Controller 先提交活动标签，视图随后换挂；过渡期间拒绝向旧实体投影新目标。
    ///
    /// @author kongweiguang
    pub(super) fn tab_tool_contents_are_current(&self) -> bool {
        self.tab_tool_contents_tab_id
            == self
                .workspace_controller
                .as_ref()
                .and_then(|controller| controller.snapshot().active_tab_id.clone())
    }

    /// 所有标签使用同一装配入口和应用服务，分别保留查询、变量与编辑草稿，不复制业务数据。
    ///
    /// @author kongweiguang
    pub(super) fn create_commands_view(
        services: &AppServices,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<CommandsView>> {
        services.commands.clone().map(|commands| {
            let executor = services.executor.clone();
            let execution = kerminal_application::commands::SnippetExecutionService::new(
                commands.clone(),
                services.sessions.clone(),
            );
            cx.new(|cx| {
                let mut view = CommandsView::new(commands, executor, window, cx)
                    .with_snippet_execution_service(execution);
                if let Some(service) = services.configuration_validation.clone() {
                    view = view.with_configuration_validation(service);
                }
                if let Some(service) = services.configuration_directory.clone() {
                    view = view.with_configuration_directory(service);
                }
                if let Some(service) = services.snippet_transfer_files.clone() {
                    view = view.with_snippet_transfer_files(service);
                }
                view
            })
        })
    }

    /// 后台标签可完成自身操作，但迟到事件不得打开当前标签的编辑器或夺取终端焦点。
    ///
    /// @author kongweiguang
    pub(super) fn subscribe_commands_view(
        view: Option<&Entity<CommandsView>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (
        Option<Subscription>,
        Option<Subscription>,
        Option<Subscription>,
    ) {
        let Some(view) = view else {
            return (None, None, None);
        };
        let editor = cx.subscribe_in(
            view,
            window,
            |root, source, event: &SnippetEditorVisibilityChanged, window, cx| {
                if root.commands_view.as_ref() == Some(source) {
                    snippet_editor::sync_visibility(
                        root,
                        event.open,
                        event.focus_command_draft,
                        window,
                        cx,
                    );
                }
            },
        );
        let fill = cx.subscribe_in(view, window, |root, source, event, window, cx| {
            snippet_editor::focus_terminal_after_fill(root, source, event, window, cx);
        });
        let current_command = cx.subscribe_in(
            view,
            window,
            |root, source, event: &SnippetCurrentCommandRequested, window, cx| {
                if root.commands_view.as_ref() == Some(source) {
                    super::command_blocks::request_snippet_current_command(
                        root,
                        source.clone(),
                        event.clone(),
                        window,
                        cx,
                    );
                }
            },
        );
        (Some(editor), Some(fill), Some(current_command))
    }

    /// 隐藏标签中的迟到导航事件不能切换共享编辑器，也不能打开根确认。
    ///
    /// @author kongweiguang
    pub(super) fn is_cached_sftp_panel(&self, pane: &Entity<FilesPane>) -> bool {
        self.retained_tab_tool_contents.values().any(|contents| {
            contents
                .sftp_panel
                .as_ref()
                .is_some_and(|current| current.entity_id() == pane.entity_id())
        })
    }

    /// 工作区标签变化时换挂既有实体；仅清理已关闭标签的视图缓存。
    ///
    /// @author kongweiguang
    pub(super) fn reconcile_tab_tool_contents(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(controller) = self.workspace_controller.as_ref() else {
            return;
        };
        let snapshot = controller.snapshot();
        let active_tab_id = snapshot.active_tab_id.clone();
        let live_tab_ids = snapshot
            .terminal_tabs
            .iter()
            .map(|tab| tab.id().clone())
            .collect::<HashSet<_>>();

        if self.tab_tool_contents_tab_id != active_tab_id {
            let outgoing = RetainedTabToolContents::take_from_root(self, window, cx);
            if self
                .tab_tool_contents_tab_id
                .as_ref()
                .is_none_or(|tab_id| live_tab_ids.contains(tab_id))
            {
                self.retained_tab_tool_contents
                    .insert(self.tab_tool_contents_tab_id.clone(), outgoing);
            }

            let incoming = self
                .retained_tab_tool_contents
                .remove(&active_tab_id)
                .unwrap_or_default();
            self.tab_tool_contents_tab_id = active_tab_id;
            incoming.restore_into_root(self, window, cx);
            self.refresh_commands_session(cx);
        }

        self.retained_tab_tool_contents
            .retain(|tab_id, _| tab_id.as_ref().is_none_or(|id| live_tab_ids.contains(id)));
    }
}
