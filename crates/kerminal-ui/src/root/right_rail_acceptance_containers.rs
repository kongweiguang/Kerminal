//! 容器验收只委托当前 fixture 视图，不通过直接改快照绕过生产确认和 Root 目标门禁。
//!
//! @author kongweiguang

use gpui_kit::{Context, Entity, Window};
use kerminal_application::preferences::ToolRailToolId;
use kerminal_domain::{
    identity::HostId,
    operations::{ContainerAction, ContainerStatus},
    sessions::{SessionTarget, SshInitialCommand},
};

use crate::features::operations::OperationsView;

use crate::root::{RootView, RuntimePanePhase};

impl RootView {
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
