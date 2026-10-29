#![cfg(feature = "right-rail-acceptance")]

//! 固定右栏 SSH 隧道验收桥；每次调用都重新绑定活动 fixture pane。
//!
//! @author kongweiguang

use gpui_kit::{Context, Window};
use kerminal_domain::operations::{ForwardState, PortForwardConfig};

use super::RootView;

impl RootView {
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

    /// 只有同一稳定 ID 的停止定义可调用生产删除动作。
    ///
    /// @author kongweiguang
    pub fn delete_right_rail_acceptance_forward(
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
