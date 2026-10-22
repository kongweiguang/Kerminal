//! 终端视口命令的 UI 准入；绘制快照仅回读引擎位置。
//!
//! @author kongweiguang

use gpui_kit::{Context, Window};
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::sessions::SessionOwner;
use kerminal_terminal::snapshot::RenderSnapshot;

use super::{CenterPanel, RootView, RuntimePanePhase, SessionCommand};

impl RootView {
    /// 历史页持有 Root 的只读焦点；明确更新逻辑 pane 后才聚焦，避免分屏间的复制落错目标。
    pub(crate) fn focus_terminal_history_target(
        &mut self,
        binding: &UiSessionBinding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.terminal_snapshot_for_binding(binding).is_none()
            || self.pane_input_handlers.contains_key(&binding.session_id)
        {
            return false;
        }
        match &binding.owner {
            SessionOwner::Ui { tab_id, pane_id } => {
                self.focus_workspace_pane(tab_id.clone(), pane_id.clone(), window, cx);
            }
            SessionOwner::Agent { .. } => self.focused_agent_terminal = true,
            SessionOwner::Mcp | SessionOwner::Service { .. } => return false,
        }
        window.focus(&self.focus_handle, cx);
        self.set_reported_focus_session(None, cx);
        self.cancel_unfocused_pane_compositions(window, cx);
        true
    }

    /// 滚动条和跳转只操作事件捕获的可见 owner；迟到事件不能把新 pane 的视口改回旧位置。
    /// 实际夹紧由持有网格锁的服务完成，避免 UI 用过期 history_size 推断可滚动范围。
    ///
    /// @author kongweiguang
    pub(crate) fn handle_terminal_viewport_scroll(
        &mut self,
        binding: UiSessionBinding,
        offset: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        self.terminal_snapshot_for_binding(&binding).is_some()
            && self.enqueue_session_command(SessionCommand::ScrollTo { binding, offset }, cx)
    }

    /// 边缘拖选逐帧读取最近确认的网格；旧 Canvas 捕获的行号不能跨滚动或重连继续使用。
    /// 返回值只借用当前投影，不复制整段 scrollback，也不改变引擎视口。
    ///
    /// @author kongweiguang
    pub(crate) fn terminal_snapshot_for_binding(
        &self,
        binding: &UiSessionBinding,
    ) -> Option<&RenderSnapshot> {
        if self.terminal_view_bindings.get(&binding.session_id) != Some(binding) {
            return None;
        }
        let snapshot = match &binding.owner {
            SessionOwner::Ui { tab_id, pane_id } => {
                let active = self.center_panel == CenterPanel::Terminal
                    && self
                        .workspace_controller
                        .as_ref()
                        .is_some_and(|controller| {
                            controller.snapshot().active_tab_id.as_ref() == Some(tab_id)
                        });
                if !active {
                    return None;
                }
                let pane = self.workspace_runtime.pane(pane_id)?;
                if (pane.phase != RuntimePanePhase::Ready
                    && !self
                        .workspace_runtime
                        .close_failed_history_binding_matches(pane_id, binding))
                    || pane.tab_id != *tab_id
                    || pane.session_id.as_ref() != Some(&binding.session_id)
                {
                    return None;
                }
                pane.snapshot.as_ref()
            }
            SessionOwner::Agent { agent_session_id } => {
                if self.selected_agent_session_id.as_ref() != Some(agent_session_id)
                    || self.agent_terminal_session_id.as_ref() != Some(&binding.session_id)
                {
                    return None;
                }
                self.agent_terminal_snapshot.as_ref()
            }
            SessionOwner::Mcp | SessionOwner::Service { .. } => None,
        };
        snapshot.filter(|snapshot| snapshot.session_id == binding.session_id)
    }
}
