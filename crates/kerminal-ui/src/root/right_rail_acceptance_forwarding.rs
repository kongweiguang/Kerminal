#![cfg(feature = "right-rail-acceptance")]

//! 固定右栏 SSH 隧道验收桥；每次调用都重新绑定活动 fixture pane。
//!
//! @author kongweiguang

use gpui_kit::{Context, ElementId, Entity, Window};
use kerminal_application::{preferences::settings::ToolRailToolId, sessions::UiSessionBinding};
use kerminal_domain::{
    identity::HostId,
    operations::{ForwardState, PortForwardConfig},
};

use crate::features::operations::{OperationsView, proxy_panel::ProxyCurrentSession};

use super::RootView;

impl RootView {
    /// 读取实际 Ports 面板的忙碌和恢复动作，仍核对原 fixture 的 session 与 generation。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_proxy_action(
        &self,
        expected: &PortForwardConfig,
        forward_id: &str,
        binding: &UiSessionBinding,
        cx: &Context<Self>,
    ) -> Result<(bool, bool), String> {
        self.validate_right_rail_acceptance_forward_fixture(expected)?;
        if self
            .right_rail_acceptance_fixture_binding(&expected.host_id)?
            .as_ref()
            != Some(binding)
        {
            return Err("代理验收终端绑定已变化".to_owned());
        }
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Ports)
            .ok_or_else(|| "SSH 隧道 Operations 视图不可用".to_owned())?;
        operations
            .read(cx)
            .right_rail_acceptance_proxy_action(expected, forward_id, binding, cx)
    }

    /// 只检查生产列表是否真正进入 Loading；测试不伪造或注入刷新状态。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_forward_loading(
        &self,
        host_id: &HostId,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Ports)
            || self
                .right_rail_acceptance_fixture_binding(host_id)?
                .is_none()
        {
            return Err("刷新验收目标已变化".to_owned());
        }
        self.right_rail_acceptance_operations_view(ToolRailToolId::Ports)
            .map(|view| view.read(cx).right_rail_acceptance_forward_loading())
            .ok_or_else(|| "SSH 隧道 Operations 视图不可用".to_owned())
    }

    /// 只在原 fixture 的生产表单中设置可恢复的无效输入，返回真实 Input 标识供几何核对。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_invalid_forward_port(
        &mut self,
        host_id: &HostId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<ElementId, String> {
        let view = self.right_rail_acceptance_forward_form_view(host_id)?;
        view.update(cx, |view, cx| {
            view.right_rail_acceptance_invalid_forward_port(host_id, window, cx)
        })
    }

    /// 每次校验前清空旧 toast，以证明新反馈来自当前表单动作。
    ///
    /// @author kongweiguang
    pub fn clear_right_rail_acceptance_forward_form_status(
        &mut self,
        host_id: &HostId,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.right_rail_acceptance_forward_form_view(host_id)?;
        self.status = None;
        self.status_toast_observed = None;
        self.status_toast_revision = self.status_toast_revision.wrapping_add(1).max(1);
        cx.notify();
        Ok(())
    }

    /// 反馈和表单都必须归属于当前 fixture；不能把后台其它面板的提示当成本次校验结果。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_forward_form_status(
        &self,
        host_id: &HostId,
    ) -> Result<Option<String>, String> {
        self.right_rail_acceptance_forward_form_view(host_id)?;
        Ok(self.status.as_ref().map(ToString::to_string))
    }

    /// 验收允许在该弹层打开时读写原视图，但拒绝切换到其他确认或会话。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_forward_form_view(
        &self,
        host_id: &HostId,
    ) -> Result<Entity<OperationsView>, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Ports)
            || self
                .right_rail_acceptance_fixture_binding(host_id)?
                .is_none()
            || self.root_confirmation_owner != Some(super::RootConfirmationOwner::Operations)
        {
            return Err("隧道表单不再属于原 fixture".to_owned());
        }
        let view = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Ports)
            .ok_or_else(|| "SSH 隧道 Operations 视图不可用".to_owned())?;
        if self
            .root_confirmation_operations_view
            .as_ref()
            .is_none_or(|owner| owner.entity_id() != view.entity_id())
        {
            return Err("隧道表单确认所有者已变化".to_owned());
        }
        Ok(view)
    }

    /// 将代理投影限定到仍活动的 fixture pane；原隧道删除后也保留对同一 binding 的核对能力。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_proxy_session(
        &self,
        expected: &PortForwardConfig,
        forward_id: &str,
        cx: &Context<Self>,
    ) -> Result<Option<(ProxyCurrentSession, Option<String>, String)>, String> {
        if !self.right_rail_acceptance_forward_fixture_ready(expected)? {
            return Ok(None);
        }
        let binding = self
            .right_rail_acceptance_fixture_binding(&expected.host_id)?
            .ok_or_else(|| "fixture 终端绑定不可用".to_owned())?;
        let operations = self
            .right_rail_acceptance_operations_view(
                kerminal_application::preferences::settings::ToolRailToolId::Ports,
            )
            .ok_or_else(|| "SSH 隧道 Operations 视图不可用".to_owned())?;
        let mut state = operations
            .read(cx)
            .right_rail_acceptance_proxy_session(expected, forward_id, cx)?;
        if state
            .as_ref()
            .is_some_and(|(current, _, _)| current.binding != binding)
        {
            return Err("代理投影与 fixture 终端 binding 不一致".to_owned());
        }
        if let Some((_, _, diagnostic)) = state.as_mut()
            && let Some(panel) = self.proxy_panel.as_ref()
        {
            let current = panel.read(cx);
            diagnostic.push_str(&format!(
                ", main_panel={:?}, main_session={:?}, main_recovery={:?}",
                panel.entity_id(),
                current.current_session(),
                current.current_recovery_forward_id()
            ));
        }
        Ok(state)
    }

    /// 测试读回只检查当前 fixture 的右下角反馈，防止复用前一次操作的残留消息。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_forward_status(
        &self,
        expected: &PortForwardConfig,
    ) -> Result<Option<String>, String> {
        self.validate_right_rail_acceptance_forward_fixture(expected)?;
        Ok(self.status.as_ref().map(ToString::to_string))
    }

    /// 隔离 fixture 执行动作前清除旧反馈，确保新 toast 来自该次菜单操作。
    ///
    /// @author kongweiguang
    pub fn clear_right_rail_acceptance_forward_status(
        &mut self,
        expected: &PortForwardConfig,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.validate_right_rail_acceptance_forward_fixture(expected)?;
        self.status = None;
        cx.notify();
        Ok(())
    }

    /// 检查 shell 探针执行后的真实提示符状态，不能借旧投影提交下一次验收操作。
    ///
    /// @author kongweiguang
    pub fn refresh_right_rail_acceptance_proxy(
        &mut self,
        expected: &PortForwardConfig,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.validate_right_rail_acceptance_forward_fixture(expected)?;
        self.refresh_proxy_panel(cx);
        Ok(())
    }

    /// 只读取原 fixture 窗格可见网格，供验收判断受管命令是否污染终端显示。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_forward_terminal_text(
        &self,
        expected: &PortForwardConfig,
    ) -> Result<Option<String>, String> {
        self.validate_right_rail_acceptance_forward_fixture(expected)?;
        let Some(binding) = self.right_rail_acceptance_fixture_binding(&expected.host_id)? else {
            return Ok(None);
        };
        Ok(self
            .terminal_snapshot_for_binding(&binding)
            .map(|snapshot| {
                snapshot
                    .rows
                    .iter()
                    .map(|row| {
                        row.runs
                            .iter()
                            .map(|run| run.text.as_ref())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }))
    }

    /// 通过当前活动 fixture SSH pane 的生产表单创建精确配置的四类隧道。
    ///
    /// @author kongweiguang
    pub fn create_right_rail_acceptance_forward(
        &mut self,
        expected: &PortForwardConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.validate_right_rail_acceptance_forward_fixture(expected)?;
        let operations = self
            .right_rail_acceptance_operations_view(
                kerminal_application::preferences::settings::ToolRailToolId::Ports,
            )
            .ok_or_else(|| "SSH 隧道 Operations 视图不可用".to_owned())?;
        operations.update(cx, |view, view_cx| {
            view.right_rail_acceptance_create_forward(expected, window, view_cx)
        })
    }

    /// 从当前 fixture host 的生产转发清单回读保存配置的稳定 ID。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_forward_id(
        &self,
        expected: &PortForwardConfig,
        cx: &Context<Self>,
    ) -> Result<Option<String>, String> {
        if !self.right_rail_acceptance_forward_fixture_ready(expected)? {
            return Ok(None);
        }
        let operations = self
            .right_rail_acceptance_operations_view(
                kerminal_application::preferences::settings::ToolRailToolId::Ports,
            )
            .ok_or_else(|| "SSH 隧道 Operations 视图不可用".to_owned())?;
        operations
            .read(cx)
            .right_rail_acceptance_forward_id(expected)
    }

    /// 只对完整配置和稳定 ID 同时匹配的 fixture 定义调用正式启动动作。
    ///
    /// @author kongweiguang
    pub fn start_right_rail_acceptance_forward(
        &mut self,
        expected: &PortForwardConfig,
        forward_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.validate_right_rail_acceptance_forward_fixture(expected)?;
        let operations = self
            .right_rail_acceptance_operations_view(
                kerminal_application::preferences::settings::ToolRailToolId::Ports,
            )
            .ok_or_else(|| "SSH 隧道 Operations 视图不可用".to_owned())?;
        operations.update(cx, |view, view_cx| {
            view.right_rail_acceptance_start_forward(expected, forward_id, view_cx)
        })
    }

    /// 当前状态只从活动 Ports 面板绑定的 fixture host 生产清单回读。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_forward_state(
        &self,
        expected: &PortForwardConfig,
        forward_id: &str,
        cx: &Context<Self>,
    ) -> Result<Option<ForwardState>, String> {
        if !self.right_rail_acceptance_forward_fixture_ready(expected)? {
            return Ok(None);
        }
        let operations = self
            .right_rail_acceptance_operations_view(
                kerminal_application::preferences::settings::ToolRailToolId::Ports,
            )
            .ok_or_else(|| "SSH 隧道 Operations 视图不可用".to_owned())?;
        operations
            .read(cx)
            .right_rail_acceptance_forward_state(expected, forward_id)
    }

    /// 状态只有来自同一 fixture 配置及 ID 时才可结束等待。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_forward_state_ready(
        &self,
        expected: &PortForwardConfig,
        forward_id: &str,
        expected_state: ForwardState,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        Ok(
            self.right_rail_acceptance_forward_state(expected, forward_id, cx)?
                == Some(expected_state),
        )
    }

    /// 停止时复核原配置和 ID，覆盖活动或失败的同一运行实例。
    ///
    /// @author kongweiguang
    pub fn stop_right_rail_acceptance_forward(
        &mut self,
        expected: &PortForwardConfig,
        forward_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.validate_right_rail_acceptance_forward_fixture(expected)?;
        let operations = self
            .right_rail_acceptance_operations_view(
                kerminal_application::preferences::settings::ToolRailToolId::Ports,
            )
            .ok_or_else(|| "SSH 隧道 Operations 视图不可用".to_owned())?;
        operations.update(cx, |view, view_cx| {
            view.right_rail_acceptance_stop_forward(expected, forward_id, view_cx)
        })
    }

    /// 删除必须经过生产确认入口，测试同样冻结原配置和稳定 ID。
    ///
    /// @author kongweiguang
    pub fn open_right_rail_acceptance_forward_delete(
        &mut self,
        expected: &PortForwardConfig,
        forward_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.validate_right_rail_acceptance_forward_fixture(expected)?;
        let operations = self
            .right_rail_acceptance_operations_view(
                kerminal_application::preferences::settings::ToolRailToolId::Ports,
            )
            .ok_or_else(|| "SSH 隧道 Operations 视图不可用".to_owned())?;
        operations.update(cx, |view, view_cx| {
            view.right_rail_acceptance_open_forward_delete_confirmation(
                expected, forward_id, view_cx,
            )
        })
    }

    /// 确认弹层在 Root 挂载期间仍可回读原视图，不受新操作的弹层互斥门禁阻挡。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_forward_delete_confirmation_ready(
        &self,
        expected: &PortForwardConfig,
        forward_id: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        if !self.right_rail_acceptance_forward_fixture_ready(expected)? {
            return Ok(false);
        }
        let operations = self
            .right_rail_acceptance_operations_view(
                kerminal_application::preferences::settings::ToolRailToolId::Ports,
            )
            .ok_or_else(|| "SSH 隧道 Operations 视图不可用".to_owned())?;
        if self.root_confirmation_owner != Some(super::RootConfirmationOwner::Operations)
            || self
                .root_confirmation_operations_view
                .as_ref()
                .is_none_or(|owner| owner.entity_id() != operations.entity_id())
        {
            return Ok(false);
        }
        operations
            .read(cx)
            .right_rail_acceptance_forward_delete_confirmation_ready(expected, forward_id)
    }

    /// 取消走生产关闭动作，随后由测试回读原定义仍然存在且未执行删除。
    ///
    /// @author kongweiguang
    pub fn cancel_right_rail_acceptance_forward_delete(
        &mut self,
        expected: &PortForwardConfig,
        forward_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if !self.right_rail_acceptance_forward_fixture_ready(expected)? {
            return Ok(false);
        }
        let operations = self
            .right_rail_acceptance_operations_view(
                kerminal_application::preferences::settings::ToolRailToolId::Ports,
            )
            .ok_or_else(|| "SSH 隧道 Operations 视图不可用".to_owned())?;
        operations.update(cx, |view, view_cx| {
            view.right_rail_acceptance_cancel_forward_delete_confirmation(
                expected, forward_id, view_cx,
            )
        })
    }

    /// 只有同一稳定 ID 的停止定义和已打开的确认可提交生产删除动作。
    ///
    /// @author kongweiguang
    pub fn delete_right_rail_acceptance_forward(
        &mut self,
        expected: &PortForwardConfig,
        forward_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if !self.right_rail_acceptance_forward_fixture_ready(expected)? {
            return Ok(false);
        }
        let operations = self
            .right_rail_acceptance_operations_view(
                kerminal_application::preferences::settings::ToolRailToolId::Ports,
            )
            .ok_or_else(|| "SSH 隧道 Operations 视图不可用".to_owned())?;
        operations.update(cx, |view, view_cx| {
            view.right_rail_acceptance_delete_forward(expected, forward_id, view_cx)
        })
    }

    /// 只在当前 fixture host 的 Ready 清单不含本轮 ID 和唯一名称时确认删除完成。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_forward_deleted(
        &self,
        expected: &PortForwardConfig,
        forward_id: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        if !self.right_rail_acceptance_forward_fixture_ready(expected)? {
            return Ok(false);
        }
        let operations = self
            .right_rail_acceptance_operations_view(
                kerminal_application::preferences::settings::ToolRailToolId::Ports,
            )
            .ok_or_else(|| "SSH 隧道 Operations 视图不可用".to_owned())?;
        operations
            .read(cx)
            .right_rail_acceptance_forward_deleted(expected, forward_id)
    }

    /// 每个写入动作重核验 Ports 面板、Root 确认所有者和活动 fixture pane。
    ///
    /// @author kongweiguang
    fn validate_right_rail_acceptance_forward_fixture(
        &self,
        expected: &PortForwardConfig,
    ) -> Result<(), String> {
        use kerminal_application::preferences::settings::ToolRailToolId;

        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Ports) {
            return Err("Ports 工具不是当前验收面板".to_owned());
        }
        if self.root_confirmation_owner.is_some() {
            return Err("Root 有其他确认弹层在途".to_owned());
        }
        if self
            .right_rail_acceptance_fixture_binding(&expected.host_id)?
            .is_none()
        {
            return Err("当前 Root pane 尚未绑定验收 SSH host".to_owned());
        }
        Ok(())
    }

    /// 状态读取只等待活动 Ports 工具和明确 fixture binding，不投递到其它 pane。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_forward_fixture_ready(
        &self,
        expected: &PortForwardConfig,
    ) -> Result<bool, String> {
        use kerminal_application::preferences::settings::ToolRailToolId;

        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Ports) {
            return Ok(false);
        }
        Ok(self
            .right_rail_acceptance_fixture_binding(&expected.host_id)?
            .is_some())
    }
}
