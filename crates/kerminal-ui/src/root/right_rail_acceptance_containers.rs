//! 容器验收只委托当前 fixture 视图，不通过直接改快照绕过生产确认和 Root 目标门禁。
//!
//! @author kongweiguang

use std::future::Future;

use gpui_kit::{Bounds, Context, Entity, Pixels, Window};
use kerminal_application::{
    AppError,
    preferences::ToolRailToolId,
    workspace::{WorkspaceFileAccess, WorkspaceSessionTab, WorkspaceTargetRef},
};
use kerminal_domain::{
    files::{FileEndpoint, RemotePath},
    identity::{HostId, TabId},
    operations::{ComposeProjectTarget, ContainerAction, ContainerStatus},
    sessions::{SessionTarget, SshInitialCommand},
};

use crate::features::{operations::OperationsView, workspace_tabs::WorkspaceTabEvent};

use crate::root::{RootView, RuntimePanePhase};

impl RootView {
    /// 向后台验收暴露目标绑定编辑器的真实绘制边界，既不改变内容也不发出额外文件请求。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_compose_editor_bounds(
        &self,
        host: &HostId,
        target: &ComposeProjectTarget,
        cx: &Context<Self>,
    ) -> Result<Option<(Bounds<Pixels>, Option<Bounds<Pixels>>)>, String> {
        self.right_rail_acceptance_container_view(host)?
            .read(cx)
            .right_rail_acceptance_compose_editor_bounds(host, target, cx)
    }

    /// 将夹具项目的展示状态限定在当前容器视图，只提供读回，不代替生产点击触发导航。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_compose_presentation(
        &self,
        host: &HostId,
        target: &ComposeProjectTarget,
        cx: &Context<Self>,
    ) -> Result<Option<(bool, bool, bool, bool, bool)>, String> {
        self.right_rail_acceptance_container_view(host)?
            .read(cx)
            .right_rail_acceptance_compose_presentation(host, target)
    }

    /// 读取前先核对真实列表中的唯一所有权标签，future 只捕获本轮固定端点而不暴露文件服务。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_read_compose_fixture(
        &self,
        host: &HostId,
        target: &ComposeProjectTarget,
        cx: &Context<Self>,
    ) -> Result<impl Future<Output = Result<String, String>> + Send + 'static + use<>, String> {
        let endpoint = compose_fixture_endpoint(host, target)?;
        let owned = self
            .right_rail_acceptance_compose_owned_state(host, target, cx)?
            .ok_or_else(|| "Compose fixture 列表尚未就绪".to_owned())?;
        if !matches!(owned.as_slice(), [(_, ContainerStatus::Running)]) {
            return Err("Compose fixture 缺少唯一运行中的归属容器".to_owned());
        }
        let files = self
            .services
            .files
            .clone()
            .ok_or_else(|| "文件服务不可用".to_owned())?;
        let executor = self.services.executor.clone();
        Ok(async move {
            executor
                .run(move |cancellation| async move {
                    let document = files
                        .read_text_file(cancellation, endpoint.clone(), 1024 * 1024)
                        .await?;
                    if document.endpoint != endpoint
                        || document.truncated
                        || document.content.trim().is_empty()
                    {
                        return Err(AppError::conflict(
                            "Compose fixture 文件读取不完整或目标不一致",
                        ));
                    }
                    Ok(document.content)
                })
                .await
                .map_err(|error| error.public_message().to_owned())
        })
    }

    /// 普通文件打开复用 Root 的生产订阅入口，并要求当前终端或文件上下文仍属于原项目。
    ///
    /// @author kongweiguang
    pub fn open_right_rail_acceptance_compose_fixture_as_file(
        &mut self,
        host: &HostId,
        terminal_tab_id: &TabId,
        target: &ComposeProjectTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let endpoint = compose_fixture_endpoint(host, target)?;
        if !self.right_rail_acceptance_compose_fixture_context(host, terminal_tab_id, target, cx)?
            || self.root_confirmation_owner.is_some()
        {
            return Ok(false);
        }
        self.open_file_endpoint_with_lease(endpoint, None, window, cx);
        Ok(true)
    }

    /// Compose 入口保持 Operations 事件与 Root live gate，文件 Tab 活动时也核对其宿主和身份。
    ///
    /// @author kongweiguang
    pub fn request_right_rail_acceptance_compose_fixture_config(
        &mut self,
        host: &HostId,
        terminal_tab_id: &TabId,
        target: &ComposeProjectTarget,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let endpoint = compose_fixture_endpoint(host, target)?;
        if !self.right_rail_acceptance_compose_fixture_context(host, terminal_tab_id, target, cx)?
            || self.root_confirmation_owner.is_some()
        {
            return Ok(false);
        }
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Containers)
            .ok_or_else(|| "容器 Operations 视图不可用".to_owned())?;
        let Some(owned) = operations
            .read(cx)
            .right_rail_acceptance_compose_owned_state(host, target)?
        else {
            return Ok(false);
        };
        if !matches!(owned.as_slice(), [(_, ContainerStatus::Running)]) {
            return Err("Compose fixture 缺少唯一运行中的归属容器".to_owned());
        }
        let FileEndpoint::Remote { path, .. } = endpoint else {
            return Err("Compose fixture 端点不是 SSH 文件".to_owned());
        };
        operations.update(cx, |view, cx| {
            view.right_rail_acceptance_request_compose_config(host, target, path.as_str(), cx)
        })
    }

    /// 同时核对导航、运行态和编辑器的唯一身份、权限与真实正文，加载或脏稿不能冒充就绪。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_compose_fixture_file_ready(
        &self,
        host: &HostId,
        target: &ComposeProjectTarget,
        expected: &str,
        readonly: bool,
        cx: &Context<Self>,
    ) -> Result<Option<TabId>, String> {
        let endpoint = compose_fixture_endpoint(host, target)?;
        let Some(controller) = self.workspace_controller.as_ref() else {
            return Ok(None);
        };
        let snapshot = controller.snapshot();
        let matching = compose_fixture_tabs(snapshot.terminal_tabs.as_slice(), host, &endpoint);
        if matching.len() > 1 {
            return Err("同一 Compose 文件出现多个导航标签".to_owned());
        }
        let Some((tab_id, access)) = matching.first().copied() else {
            return Ok(None);
        };
        let expected_access = if readonly {
            WorkspaceFileAccess::Readonly
        } else {
            WorkspaceFileAccess::Editable
        };
        if *access != expected_access
            || snapshot.active_tab_id.as_ref() != Some(tab_id)
            || !matches!(self.center_panel, crate::root::CenterPanel::Editor)
            || self.workspace_files.file_endpoint(tab_id).as_ref() != Some(&endpoint)
            || !matches!(
                self.workspace_files.restore_state(tab_id),
                Some(crate::root::workspace_files::WorkspaceFileRestoreState::Ready)
            )
            || self.root_confirmation_owner.is_some()
        {
            return Ok(None);
        }
        let Some(editor) = self.file_editor_workspace.as_ref() else {
            return Err("文件编辑工作区不可用".to_owned());
        };
        let editor = editor.read(cx);
        let projection = editor.workspace_projection();
        let matches = projection
            .tabs
            .iter()
            .filter(|tab| tab.endpoint == endpoint)
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            return Err("同一 Compose 文件出现多个编辑器缓冲".to_owned());
        }
        let Some(tab) = matches.first() else {
            return Ok(None);
        };
        let ready = tab.active
            && !tab.loading
            && !tab.dirty
            && tab.readonly == readonly
            && if readonly {
                editor.right_rail_acceptance_readonly_file_ready(&endpoint, expected, cx)
            } else {
                editor.right_rail_acceptance_file_ready(&endpoint, expected, cx)
            };
        Ok(ready.then(|| tab_id.clone()))
    }

    /// 就绪后仅派发一次生产关闭；处理器的 bool 只用于激活，完成必须另由双投影消失证明。
    ///
    /// @author kongweiguang
    pub fn close_right_rail_acceptance_compose_fixture_file(
        &mut self,
        host: &HostId,
        target: &ComposeProjectTarget,
        expected: &str,
        readonly: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some(tab_id) = self.right_rail_acceptance_compose_fixture_file_ready(
            host, target, expected, readonly, cx,
        )?
        else {
            return Ok(false);
        };
        self.handle_workspace_tab_event(&WorkspaceTabEvent::Close(tab_id), window, cx);
        Ok(true)
    }

    /// 两个投影都移除精确端点才认定关闭完成，避免异步关闭留下重复导航或缓冲。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_compose_fixture_file_closed(
        &self,
        host: &HostId,
        target: &ComposeProjectTarget,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let endpoint = compose_fixture_endpoint(host, target)?;
        let Some(controller) = self.workspace_controller.as_ref() else {
            return Ok(false);
        };
        let nav_empty =
            compose_fixture_tabs(&controller.snapshot().terminal_tabs, host, &endpoint).is_empty();
        let editor_empty = self.file_editor_workspace.as_ref().is_some_and(|editor| {
            !editor
                .read(cx)
                .workspace_projection()
                .tabs
                .iter()
                .any(|tab| tab.endpoint == endpoint)
        });
        Ok(nav_empty && editor_empty)
    }

    /// 活动终端与文件投影分别验证真实上下文，避免借后台终端向其他主机发送项目请求。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_compose_fixture_context(
        &self,
        host: &HostId,
        terminal_tab_id: &TabId,
        target: &ComposeProjectTarget,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let endpoint = compose_fixture_endpoint(host, target)?;
        let Some(active) = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.snapshot().active_tab_id.as_ref())
        else {
            return Ok(false);
        };
        if active == terminal_tab_id {
            return self.right_rail_acceptance_compose_terminal_context_ready(
                host,
                terminal_tab_id,
                target,
                cx,
            );
        }
        if self.workspace_files.file_endpoint(active).as_ref() != Some(&endpoint) {
            return Err("活动标签不是本轮 Compose fixture 文件或终端".to_owned());
        }
        self.right_rail_acceptance_compose_context_ready(host, active, target, cx)
    }

    /// Compose 验收只读取当前 fixture 宿主中的完整项目快照，不跨宿主寻找同名容器。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_compose_owned_state(
        &self,
        host: &HostId,
        target: &ComposeProjectTarget,
        cx: &Context<Self>,
    ) -> Result<Option<Vec<(String, ContainerStatus)>>, String> {
        self.right_rail_acceptance_container_view(host)?
            .read(cx)
            .right_rail_acceptance_compose_owned_state(host, target)
    }

    /// 拆除后的项目仍通过原宿主视图选择已知目标，禁止切换到其他同名项目。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_select_owned_compose(
        &mut self,
        host: &HostId,
        target: &ComposeProjectTarget,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.right_rail_acceptance_container_view(host)?
            .update(cx, |view, cx| {
                view.right_rail_acceptance_select_owned_compose(host, target, cx)
            })
    }

    /// 实际根确认必须仍对应原宿主和完整项目，不能把其他模态当成拆除确认。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_compose_down_confirmation_ready(
        &self,
        host: &HostId,
        target: &ComposeProjectTarget,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        self.right_rail_acceptance_container_view(host)?
            .read(cx)
            .right_rail_acceptance_compose_down_confirmation_ready(host, target)
    }

    /// 取消仅调用生产模态的关闭路径，后续状态必须由原宿主列表回读证明。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_cancel_compose_down(
        &mut self,
        host: &HostId,
        target: &ComposeProjectTarget,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.right_rail_acceptance_container_view(host)?
            .update(cx, |view, cx| {
                view.right_rail_acceptance_cancel_compose_down(host, target, cx)
            })
    }

    /// 提交通过 Operations 生产事件和 Root live gate，不直接调用 Compose 服务。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_confirm_compose_down(
        &mut self,
        host: &HostId,
        target: &ComposeProjectTarget,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.right_rail_acceptance_container_view(host)?
            .update(cx, |view, cx| {
                view.right_rail_acceptance_confirm_compose_down(host, target, cx)
            })
    }

    /// 日志必须来自当前可写日志 PTY 的实际网格；同宿主普通 shell 和旧快照都不能满足回读。
    pub fn right_rail_acceptance_container_logs_text(
        &self,
        host: &HostId,
        id: &str,
    ) -> Result<Option<String>, String> {
        for (_, pane) in self.workspace_runtime.panes() {
            let Some(SessionTarget::Ssh(target)) = pane.target.as_ref() else {
                continue;
            };
            let Some(SshInitialCommand::Process { program, args }) = target.command.as_ref() else {
                continue;
            };
            if &target.host_id != host
                || !matches!(program.as_str(), "docker" | "podman")
                || args
                    .iter()
                    .map(String::as_str)
                    .ne(["logs", "--tail", "200", "-f", id])
                || !pane.transient
                || pane.phase != RuntimePanePhase::Ready
            {
                continue;
            }
            let Some(session_id) = pane.session_id.as_ref() else {
                continue;
            };
            let Some(binding) = self.terminal_view_bindings.get(session_id) else {
                continue;
            };
            if !self.pane_input_handlers.contains_key(session_id) {
                continue;
            }
            let Some(snapshot) = self.terminal_snapshot_for_binding(binding) else {
                continue;
            };
            let text = snapshot
                .rows
                .iter()
                .map(|row| {
                    row.runs
                        .iter()
                        .map(|run| run.text.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(Some(text));
        }
        Ok(None)
    }

    /// 校验活动 pane 后取得原视图；测试失去目标也必须失败，不能借左栏选择改投。
    fn right_rail_acceptance_container_view(
        &self,
        host: &HostId,
    ) -> Result<Entity<OperationsView>, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Containers)
            || self.right_rail_acceptance_fixture_binding(host)?.is_none()
        {
            return Err("容器验收已离开原 fixture SSH pane".to_owned());
        }
        self.right_rail_acceptance_operations_view(ToolRailToolId::Containers)
            .ok_or_else(|| "容器验收视图不可用".to_owned())
    }

    /// 状态来自生产列表回执，同时保留容器 ID，防止停止再启动时误认同名新对象。
    pub fn right_rail_acceptance_container_state(
        &self,
        host: &HostId,
        name: &str,
        cx: &Context<Self>,
    ) -> Result<Option<(String, ContainerStatus)>, String> {
        self.right_rail_acceptance_container_view(host)?
            .read(cx)
            .right_rail_acceptance_container_state(host, name)
    }

    /// 确认仍由原生产方法创建，Root 通过既有事件承载模态和焦点约束。
    pub fn right_rail_acceptance_open_container_lifecycle_confirmation(
        &mut self,
        host: &HostId,
        name: &str,
        action: ContainerAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.right_rail_acceptance_container_view(host)?
            .update(cx, |view, cx| {
                view.right_rail_acceptance_open_container_lifecycle_confirmation(
                    host, name, action, window, cx,
                )
            })
    }

    /// 等待原 Operations 草稿就绪，不能把发出打开事件当作确认框已挂载。
    pub fn right_rail_acceptance_container_confirmation_ready(
        &self,
        host: &HostId,
        name: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        self.right_rail_acceptance_container_view(host)?
            .read(cx)
            .right_rail_acceptance_container_confirmation_ready(host, name)
    }

    /// 取消走同一个生产关闭入口，保留未执行操作的对象和列表用于回读。
    pub fn right_rail_acceptance_cancel_container_confirmation(
        &mut self,
        host: &HostId,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.right_rail_acceptance_container_view(host)?
            .update(cx, |view, cx| {
                view.right_rail_acceptance_cancel_container_confirmation(host, name, cx)
            })
    }

    /// 提交仍经冻结 ticket 和正常事件回验，测试不直接调用 application 变更服务。
    pub fn right_rail_acceptance_confirm_container_confirmation(
        &mut self,
        host: &HostId,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.right_rail_acceptance_container_view(host)?
            .update(cx, |view, cx| {
                view.right_rail_acceptance_confirm_container_confirmation(host, name, cx)
            })
    }

    /// main 的启动直接提交，保持与停止和重启的确认交互不同。
    pub fn right_rail_acceptance_start_container(
        &mut self,
        host: &HostId,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.right_rail_acceptance_container_view(host)?
            .update(cx, |view, cx| {
                view.right_rail_acceptance_start_container(host, name, cx)
            })
    }

    /// 日志入口委托生产事件创建正常工作区 Tab，后续必须核对真实终端目标与输出。
    pub fn right_rail_acceptance_request_container_logs(
        &mut self,
        host: &HostId,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.right_rail_acceptance_container_view(host)?
            .update(cx, |view, cx| {
                view.right_rail_acceptance_request_container_logs(host, name, cx)
            })
    }

    /// 使用真实 inspect 的启动时间证明 Restart，不能用操作前后的相同 Running 状态冒充回读。
    pub fn right_rail_acceptance_container_started_at(
        &self,
        host: &HostId,
        id: &str,
        cx: &Context<Self>,
    ) -> Result<Option<String>, String> {
        self.right_rail_acceptance_container_view(host)?
            .read(cx)
            .right_rail_acceptance_container_started_at(host, id)
    }
}

/// 只构造当前命名空间下的唯一 Compose 文件，所有参数必须指向同一宿主和运行 ID。
///
/// @author kongweiguang
fn compose_fixture_endpoint(
    host: &HostId,
    target: &ComposeProjectTarget,
) -> Result<FileEndpoint, String> {
    let run_id = target
        .project
        .strip_prefix("kerminal-rr-compose-")
        .filter(|id| {
            id.len() == 12
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or_else(|| "Compose fixture 项目缺少有效运行 ID".to_owned())?;
    let root = format!("/run/kerminal-right-rail-{run_id}");
    let file = format!("{root}/compose.yml");
    if target.host_id.as_ref() != Some(host)
        || target.working_dir != root
        || target.config_files.len() != 1
        || target.config_files.first().map(String::as_str) != Some(file.as_str())
    {
        return Err("Compose fixture 宿主、目录或唯一配置路径不匹配".to_owned());
    }
    Ok(FileEndpoint::Remote {
        host_id: host.clone(),
        path: RemotePath::parse(&file).map_err(|_| "Compose fixture 文件路径无效".to_owned())?,
    })
}

/// 描述符按宿主和完整路径计数，避免只依赖运行态索引而漏掉重复或未恢复的标签。
///
/// @author kongweiguang
fn compose_fixture_tabs<'a>(
    tabs: &'a [WorkspaceSessionTab],
    host: &HostId,
    endpoint: &FileEndpoint,
) -> Vec<(&'a TabId, &'a WorkspaceFileAccess)> {
    let FileEndpoint::Remote { path, .. } = endpoint else {
        return Vec::new();
    };
    tabs.iter()
        .filter_map(|tab| match tab {
            WorkspaceSessionTab::WorkspaceFile {
                id,
                target: WorkspaceTargetRef::Ssh { host_id },
                path: tab_path,
                access,
                ..
            } if host_id == host && tab_path == path.as_str() => Some((id, access)),
            _ => None,
        })
        .collect()
}
