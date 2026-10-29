//! 固定右栏 tmux 生命周期验收桥；只把明确的 fixture 目标交给 Operations 生产流程。
//!
//! @author kongweiguang

use gpui_kit::{Context, Window};
use kerminal_application::preferences::settings::ToolRailToolId;
use kerminal_domain::identity::HostId;

use super::RootView;

impl RootView {
    /// 经 fixture pane、活动 tmux 工具和生产创建弹框创建本轮随机命名的会话。
    ///
    /// @author kongweiguang
    pub fn create_right_rail_acceptance_tmux_session(
        &mut self,
        host_id: &HostId,
        session_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.validate_right_rail_acceptance_tmux_fixture(host_id)?;
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Tmux)
            .ok_or_else(|| "tmux Operations 视图不可用".to_owned())?;
        operations.update(cx, |view, cx| {
            view.right_rail_acceptance_create_tmux_session(host_id, session_name, window, cx)
        })
    }

    /// 返回创建名称对应的真实 session ID；异步创建或生产列表刷新期间返回 None。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_tmux_created_session_id(
        &self,
        host_id: &HostId,
        session_name: &str,
        cx: &Context<Self>,
    ) -> Result<Option<String>, String> {
        if !self.right_rail_acceptance_tmux_fixture_ready(host_id)? {
            return Ok(None);
        }
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Tmux)
            .ok_or_else(|| "tmux Operations 视图不可用".to_owned())?;
        operations
            .read(cx)
            .right_rail_acceptance_tmux_created_session_id(host_id, session_name)
    }

    /// 仅改名当前快照中名称与 ID 均匹配本轮 fixture 的 session。
    ///
    /// @author kongweiguang
    pub fn rename_right_rail_acceptance_tmux_session(
        &mut self,
        host_id: &HostId,
        session_id: &str,
        current_name: &str,
        next_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.validate_right_rail_acceptance_tmux_fixture(host_id)?;
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Tmux)
            .ok_or_else(|| "tmux Operations 视图不可用".to_owned())?;
        operations.update(cx, |view, cx| {
            view.right_rail_acceptance_rename_tmux_session(
                host_id,
                session_id,
                current_name,
                next_name,
                window,
                cx,
            )
        })
    }

    /// 等待正式改名后的同一 session ID 出现在 fixture host 的 Ready 快照中。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_tmux_rename_ready(
        &self,
        host_id: &HostId,
        session_id: &str,
        expected_name: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        if !self.right_rail_acceptance_tmux_fixture_ready(host_id)? {
            return Ok(false);
        }
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Tmux)
            .ok_or_else(|| "tmux Operations 视图不可用".to_owned())?;
        operations.read(cx).right_rail_acceptance_tmux_rename_ready(
            host_id,
            session_id,
            expected_name,
        )
    }

    /// 只向本轮创建或改名后的真实会话提交生产结束确认，拒绝种子或其他会话。
    ///
    /// @author kongweiguang
    pub fn kill_right_rail_acceptance_tmux_session(
        &mut self,
        host_id: &HostId,
        session_id: &str,
        expected_name: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        self.validate_right_rail_acceptance_tmux_fixture(host_id)?;
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Tmux)
            .ok_or_else(|| "tmux Operations 视图不可用".to_owned())?;
        operations.update(cx, |view, cx| {
            view.right_rail_acceptance_kill_tmux_session(host_id, session_id, expected_name, cx)
        })
    }

    /// 只有生产结束回执触发的列表回读确认 session ID 消失后才报告成功。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_tmux_kill_ready(
        &self,
        host_id: &HostId,
        session_id: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        if !self.right_rail_acceptance_tmux_fixture_ready(host_id)? {
            return Ok(false);
        }
        let operations = self
            .right_rail_acceptance_operations_view(ToolRailToolId::Tmux)
            .ok_or_else(|| "tmux Operations 视图不可用".to_owned())?;
        operations
            .read(cx)
            .right_rail_acceptance_tmux_kill_ready(host_id, session_id)
    }

    /// 每次读写调用都重新核对活动工具、Root fixture pane 和已就绪的固定 tmux 目标。
    ///
    /// @author kongweiguang
    fn validate_right_rail_acceptance_tmux_fixture(&self, host_id: &HostId) -> Result<(), String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Tmux) {
            return Err("tmux 工具不是当前验收面板".to_owned());
        }
        if self.root_confirmation_owner.is_some() {
            return Err("Root 有其他确认弹层在途".to_owned());
        }
        if self
            .right_rail_acceptance_fixture_binding(host_id)?
            .is_none()
        {
            return Err("当前 Root pane 尚未绑定到验收 SSH host".to_owned());
        }
        Ok(())
    }

    /// 等待时若活动工具或 fixture pane 暂时不可用就继续等待，显式连接错误仍向 driver 传播。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_tmux_fixture_ready(&self, host_id: &HostId) -> Result<bool, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Tmux) {
            return Ok(false);
        }
        Ok(self
            .right_rail_acceptance_fixture_binding(host_id)?
            .is_some())
    }
}
