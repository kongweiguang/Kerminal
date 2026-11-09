//! 内置内容与扩展内容的绘制组合；只消费宿主已准备好的视图。
//!
//! @author kongweiguang

use super::*;
use crate::plugins::builtin_content::{self, BuiltinContentInput};

/// 贡献工厂输入不匹配属于可见接线错误，不能用空白正文伪装成成功。
///
/// @author kongweiguang
fn render_builtin(id: &str, input: BuiltinContentInput) -> AnyElement {
    builtin_content::render(id, input).unwrap_or_else(|error| {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p_4()
            .child(error.public_message().to_owned())
            .into_any_element()
    })
}

impl RootView {
    /// 只消费已建立的视图实例；从窗口外壳分离正文分派，避免插件工厂获得整个 Root。
    /// 导航、焦点交接和异步激活在各自事件路径完成，绘制阶段不建立新的业务资源。
    ///
    /// @author kongweiguang
    pub(super) fn render_center_content(
        &mut self,
        active_workspace_tab: Option<&WorkspaceSessionTab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = ThemeTokens::current(window, cx);
        match self.center_panel {
            CenterPanel::Contributed => self.render_plugin_content(cx),
            CenterPanel::Terminal => match active_workspace_tab {
                Some(WorkspaceSessionTab::Terminal { id, .. }) => self
                    .workspace_renders
                    .get(id)
                    .cloned()
                    .map(|render| {
                        let inset =
                            preferences::interface_density_metrics(self).terminal_content_inset;
                        let broadcast_bar = self.render_workspace_broadcast_bar(id, window, cx);
                        render_builtin(
                            "terminal",
                            BuiltinContentInput::Terminal {
                                view: render.into_any_element(),
                                broadcast: broadcast_bar,
                                inset,
                            },
                        )
                    })
                    .unwrap_or_else(|| {
                        empty_workbench_slot(
                            "终端工作区正在准备。",
                            "The terminal workspace is preparing.",
                            theme.muted,
                        )
                    }),
                Some(WorkspaceSessionTab::Unsupported { kind, .. }) => div()
                    .id("unsupported-workspace-tab")
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p_4()
                    .text_color(theme.muted)
                    .child(format!("当前版本不支持此工作区标签：{kind}"))
                    .into_any_element(),
                Some(_) => empty_workbench_slot(
                    "请使用对应的文件工作区查看此标签。",
                    "Open this tab in its matching file workspace.",
                    theme.muted,
                ),
                None => empty_workbench_slot(
                    "从左侧主机导航打开连接，或新建本地终端。",
                    "Open a connection from host navigation or create a local terminal.",
                    theme.muted,
                ),
            },
            CenterPanel::Files => {
                let pane = match &self.files_workspace_selection {
                    workspace_files::FilesWorkspaceSelection::Workbench => self.files_pane.clone(),
                    workspace_files::FilesWorkspaceSelection::TransferTab(tab_id) => {
                        self.workspace_files.files_pane(tab_id)
                    }
                };
                pane.map_or_else(
                    || {
                        empty_workbench_slot(
                            "文件服务尚未配置。",
                            "File services are unavailable.",
                            theme.muted,
                        )
                    },
                    |pane| {
                        render_builtin(
                            if matches!(
                                &self.files_workspace_selection,
                                workspace_files::FilesWorkspaceSelection::TransferTab(_)
                            ) {
                                "sftpTransfer"
                            } else {
                                "files"
                            },
                            BuiltinContentInput::Files {
                                view: pane.into_any_element(),
                            },
                        )
                    },
                )
            }
            CenterPanel::Editor => self.file_editor_workspace.clone().map_or_else(
                || {
                    empty_workbench_slot(
                        "文件编辑服务尚未配置。",
                        "The file editor service is unavailable.",
                        theme.muted,
                    )
                },
                |editor| {
                    render_builtin(
                        if matches!(
                            active_workspace_tab,
                            Some(WorkspaceSessionTab::WorkspaceFile { .. })
                        ) {
                            "workspaceFile"
                        } else {
                            "editor"
                        },
                        BuiltinContentInput::WorkspaceFile {
                            view: editor.into_any_element(),
                        },
                    )
                },
            ),
            CenterPanel::Preferences => render_builtin(
                "preferences",
                BuiltinContentInput::Preferences {
                    view: empty_workbench_slot(
                        "设置当前作为模态窗口显示。",
                        "Settings are shown in a modal window.",
                        theme.muted,
                    ),
                },
            ),
            CenterPanel::Commands => self.commands_view.clone().map_or_else(
                || {
                    empty_workbench_slot(
                        "命令服务尚未配置。",
                        "Command services are unavailable.",
                        theme.muted,
                    )
                },
                |commands| {
                    render_builtin(
                        "commands",
                        BuiltinContentInput::Commands {
                            view: commands.into_any_element(),
                        },
                    )
                },
            ),
            CenterPanel::Connections if self.connections_modal_open => render_builtin(
                "connections",
                BuiltinContentInput::Connections {
                    view: empty_workbench_slot(
                        "主机编辑器当前作为模态窗口显示。",
                        "The host editor is currently shown in a modal window.",
                        theme.muted,
                    ),
                },
            ),
            CenterPanel::Connections => self.connections_panel.clone().map_or_else(
                || {
                    empty_workbench_slot(
                        "主机连接服务尚未配置。",
                        "Host connection services are unavailable.",
                        theme.muted,
                    )
                },
                |connections| {
                    render_builtin(
                        "connections",
                        BuiltinContentInput::Connections {
                            view: connections.into_any_element(),
                        },
                    )
                },
            ),
            CenterPanel::Agents => render_builtin(
                "agents",
                BuiltinContentInput::Agents {
                    view: empty_workbench_slot(
                        "Agent 会话固定显示在右侧工作区。",
                        "Agent sessions are shown in the right workspace.",
                        theme.muted,
                    ),
                },
            ),
        }
    }
}
