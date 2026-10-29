//! Compose YAML acceptance actions routed through the production Operations and workspace flows.
//!
//! @author kongweiguang

use gpui_kit::{Context, Window};
use kerminal_application::{
    preferences::settings::ToolRailToolId,
    workspace::{
        WorkspaceFileAccess, WorkspaceFileSource, WorkspaceSessionTab, WorkspaceTargetRef,
    },
};
use kerminal_domain::{
    files::{FileEndpoint, RemotePath},
    identity::{HostId, TabId},
    operations::ComposeProjectTarget,
    sessions::{SessionOwner, SessionTarget},
};

use crate::features::operations::ContainerPanelContext;

use super::RootView;

/// 跨 crate 传递一次 Compose 验收打开动作的固定身份和路径，避免调用方在轮询期间替换目标。
pub struct ComposeAcceptanceOpenRequest {
    host_id: HostId,
    terminal_tab_id: TabId,
    target: ComposeProjectTarget,
    path: RemotePath,
    selection_name: String,
    root_path: RemotePath,
}

impl ComposeAcceptanceOpenRequest {
    /// 拥有化本轮验收目标；私有字段让校验入口只接受完整、不可拆改的请求值。
    ///
    /// @author kongweiguang
    pub fn new(
        host_id: HostId,
        terminal_tab_id: TabId,
        target: ComposeProjectTarget,
        path: RemotePath,
        selection_name: String,
        root_path: RemotePath,
    ) -> Self {
        Self {
            host_id,
            terminal_tab_id,
            target,
            path,
            selection_name,
            root_path,
        }
    }
}

impl RootView {
    /// Returns the active fixture pane's owning workspace tab, which remains available for restoration.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_compose_terminal_tab_id(
        &self,
        host_id: &HostId,
    ) -> Result<Option<TabId>, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Containers) {
            return Err("容器工具不是当前验收面板".to_owned());
        }
        let Some(binding) = self.right_rail_acceptance_fixture_binding(host_id)? else {
            return Ok(None);
        };
        match binding.owner {
            SessionOwner::Ui { tab_id, .. } => Ok(Some(tab_id)),
            _ => Err("fixture SSH pane 不属于 UI 工作区".to_owned()),
        }
    }

    /// Reads the exact fixture project from the host snapshot so unrelated projects cannot satisfy acceptance.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_compose_target(
        &self,
        host_id: &HostId,
        fixture_project: &str,
        cx: &Context<Self>,
    ) -> Result<Option<ComposeProjectTarget>, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Containers)
            || self
                .right_rail_acceptance_fixture_binding(host_id)?
                .is_none()
        {
            return Ok(None);
        }
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Containers)
            .ok_or_else(|| "容器 Operations 视图不可用".to_owned())?;
        operations
            .read(cx)
            .right_rail_acceptance_compose_target(host_id, fixture_project)
    }

    /// Selects the real project through the mounted Containers view's production detail state.
    ///
    /// @author kongweiguang
    pub fn select_right_rail_acceptance_compose_target(
        &mut self,
        host_id: &HostId,
        target: &ComposeProjectTarget,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Containers)
            || self
                .right_rail_acceptance_fixture_binding(host_id)?
                .is_none()
        {
            return Ok(false);
        }
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Containers)
            .ok_or_else(|| "容器 Operations 视图不可用".to_owned())?;
        operations.update(cx, |view, cx| {
            view.right_rail_acceptance_select_compose_target(host_id, target, cx)
        })?;
        Ok(true)
    }

    /// Confirms project details still show the exact target after workspace activation changes the host context.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_compose_target_selected(
        &self,
        host_id: &HostId,
        target: &ComposeProjectTarget,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Containers) {
            return Ok(false);
        }
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Containers)
            .ok_or_else(|| "容器 Operations 视图不可用".to_owned())?;
        operations
            .read(cx)
            .right_rail_acceptance_compose_target_selected(host_id, target)
    }

    /// Verifies the returned terminal context still belongs to the original fixture host and project.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_compose_terminal_context_ready(
        &self,
        host_id: &HostId,
        terminal_tab_id: &TabId,
        target: &ComposeProjectTarget,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let Some(binding) = self.right_rail_acceptance_fixture_binding(host_id)? else {
            return Ok(false);
        };
        if !matches!(
            &binding.owner,
            SessionOwner::Ui { tab_id, .. } if tab_id == terminal_tab_id
        ) {
            return Err("活动 fixture pane 已切换到其它 terminal tab".to_owned());
        }
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Containers)
            .ok_or_else(|| "容器 Operations 视图不可用".to_owned())?;
        let context = operations.read(cx).container_panel_context().cloned();
        match context.as_ref() {
            Some(ContainerPanelContext::Host {
                binding: context_binding,
                host_id: Some(context_host),
            }) if context_binding == &binding && context_host == host_id => operations
                .read(cx)
                .right_rail_acceptance_compose_target_selected(host_id, target),
            Some(ContainerPanelContext::Host {
                host_id: Some(context_host),
                ..
            }) if context_host != host_id => Err("容器面板已切换到其它 SSH host".to_owned()),
            _ => Ok(false),
        }
    }

    /// Captures the workspace tab count around Compose activation to detect duplicate registrations.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_workspace_tab_count(&self) -> Option<usize> {
        self.workspace_controller
            .as_ref()
            .map(|controller| controller.snapshot().terminal_tabs.len())
    }

    /// Submits one frozen fixture request; asynchronous tab registration is observed by the separate readiness poll.
    ///
    /// @author kongweiguang
    pub fn open_right_rail_acceptance_compose_config(
        &mut self,
        request: &ComposeAcceptanceOpenRequest,
        cx: &mut Context<Self>,
    ) -> Result<Option<()>, String> {
        let ComposeAcceptanceOpenRequest {
            host_id,
            terminal_tab_id,
            target,
            path,
            selection_name,
            root_path,
        } = request;
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Containers) {
            return Err("容器工具不是当前验收面板".to_owned());
        }
        let Some(binding) = self.right_rail_acceptance_fixture_binding(host_id)? else {
            return Ok(None);
        };
        if !matches!(
            &binding.owner,
            SessionOwner::Ui { tab_id, .. } if tab_id == terminal_tab_id
        ) {
            return Err("当前 fixture SSH pane 已离开保存的 terminal tab".to_owned());
        }
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Containers)
            .ok_or_else(|| "容器 Operations 视图不可用".to_owned())?;
        if !operations
            .read(cx)
            .right_rail_acceptance_compose_target_selected(host_id, target)?
        {
            return Ok(None);
        }
        let context = operations
            .read(cx)
            .container_panel_context()
            .cloned()
            .ok_or_else(|| "容器面板缺少 fixture host 上下文".to_owned())?;
        if !matches!(
            &context,
            ContainerPanelContext::Host {
                binding: current,
                host_id: Some(current_host),
            } if current == &binding && current_host == host_id
        ) {
            return Err("容器面板上下文与当前 fixture SSH pane 不一致".to_owned());
        }

        let directory = RemotePath::parse(root_path.as_str())
            .map_err(|_| "Compose fixture 根目录无效".to_owned())?;
        let file_path = RemotePath::parse(path.as_str())
            .map_err(|_| "Compose fixture 文件路径无效".to_owned())?;
        let file_name = file_path.as_str().rsplit('/').next().unwrap_or_default();
        if file_path.parent().as_ref() != Some(&directory)
            || file_name != selection_name.as_str()
            || selection_name != "compose.yml"
            || target.host_id.as_ref() != Some(host_id)
            || target.working_dir != directory.as_str()
            || target.config_files.len() != 1
            || target.config_files.first().map(String::as_str) != Some(file_path.as_str())
        {
            return Err(
                "Compose fixture 文件必须是实时项目唯一配置路径下的 compose.yml".to_owned(),
            );
        }

        let requested = operations.update(cx, |view, cx| {
            view.right_rail_acceptance_request_compose_config(
                host_id,
                target,
                file_path.as_str(),
                cx,
            )
        })?;
        if !requested {
            return Ok(None);
        }

        Ok(Some(()))
    }

    /// 先固定原 SSH Tab 身份，再复用真实标签订阅链；Controller 接受激活后才能投影 Root 与焦点。
    ///
    /// @author kongweiguang
    pub fn activate_right_rail_acceptance_compose_terminal_tab(
        &mut self,
        host_id: &HostId,
        terminal_tab_id: &TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some(snapshot) = self
            .workspace_controller
            .as_ref()
            .map(|controller| controller.snapshot())
        else {
            return Ok(false);
        };
        let Some(pane_ids) = snapshot.terminal_tabs.iter().find_map(|tab| match tab {
            WorkspaceSessionTab::Terminal { id, layout, .. } if id == terminal_tab_id => {
                Some(layout.pane_ids())
            }
            _ => None,
        }) else {
            return Err("fixture terminal tab 已不存在".to_owned());
        };
        let has_fixture_pane = pane_ids.iter().any(|pane_id| {
            self.workspace_runtime.pane(pane_id).is_some_and(|pane| {
                matches!(
                    pane.target.as_ref(),
                    Some(SessionTarget::Ssh(target)) if &target.host_id == host_id
                )
            })
        });
        if !has_fixture_pane {
            return Err("fixture terminal tab 不再包含验收 SSH host 的活动 pane".to_owned());
        }
        self.activate_right_rail_acceptance_shell_tab(terminal_tab_id, window, cx);
        Ok(self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.snapshot().active_tab_id.as_ref())
            == Some(terminal_tab_id))
    }

    /// Waits for the exact Compose WorkspaceFile descriptor and its loaded, clean, read-only editor session.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_compose_file_ready(
        &self,
        host_id: &HostId,
        path: &str,
        selection_name: &str,
        root_path: &str,
        expected_content: &str,
        cx: &Context<Self>,
    ) -> Result<Option<(TabId, usize)>, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Containers) {
            return Ok(None);
        }
        let Some(controller) = self.workspace_controller.as_ref() else {
            return Ok(None);
        };
        let snapshot = controller.snapshot();
        let Some((tab_id, endpoint)) = snapshot.terminal_tabs.iter().find_map(|tab| match tab {
            WorkspaceSessionTab::WorkspaceFile {
                id,
                title,
                target:
                    WorkspaceTargetRef::Ssh {
                        host_id: target_host,
                    },
                path: target_path,
                root_path: Some(target_root),
                source: WorkspaceFileSource::ComposeYaml,
                access: WorkspaceFileAccess::Readonly,
                ..
            } if target_host == host_id
                && target_path == path
                && title == selection_name
                && target_root == root_path =>
            {
                Some((
                    id.clone(),
                    FileEndpoint::Remote {
                        host_id: host_id.clone(),
                        path: RemotePath::parse(path).ok()?,
                    },
                ))
            }
            _ => None,
        }) else {
            return Ok(None);
        };
        if snapshot.active_tab_id.as_ref() != Some(&tab_id) {
            return Ok(None);
        }
        if !self
            .workspace_files
            .restore_state(&tab_id)
            .is_some_and(|state| {
                matches!(
                    state,
                    super::workspace_files::WorkspaceFileRestoreState::Ready
                )
            })
        {
            return Ok(None);
        }
        let Some(editor) = self.file_editor_workspace.as_ref() else {
            return Err("文件编辑工作区不可用".to_owned());
        };
        let ready = editor.read(cx).right_rail_acceptance_readonly_file_ready(
            &endpoint,
            expected_content,
            cx,
        );
        Ok(ready.then_some((tab_id, snapshot.terminal_tabs.len())))
    }

    /// Requires the current file-host projection and Operations project to remain bound to the same identities.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_compose_context_ready(
        &self,
        host_id: &HostId,
        compose_tab_id: &TabId,
        target: &ComposeProjectTarget,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Containers) {
            return Ok(false);
        }
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Containers)
            .ok_or_else(|| "容器 Operations 视图不可用".to_owned())?;
        if self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.snapshot().active_tab_id.as_ref())
            != Some(compose_tab_id)
        {
            return Ok(false);
        }
        let context = operations.read(cx).container_panel_context().cloned();
        match context.as_ref() {
            Some(ContainerPanelContext::WorkspaceHost {
                tab_id,
                host_id: context_host,
            }) if tab_id == compose_tab_id && context_host == host_id => operations
                .read(cx)
                .right_rail_acceptance_compose_target_selected(host_id, target),
            Some(ContainerPanelContext::WorkspaceHost {
                host_id: context_host,
                ..
            }) if context_host != host_id => Err("容器面板上下文已切换到其它 SSH host".to_owned()),
            _ => Ok(false),
        }
    }
}
