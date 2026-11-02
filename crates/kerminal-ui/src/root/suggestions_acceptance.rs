//! 命令提示原生验收只读取生产 Root，不改变输入、候选或会话绑定。
//!
//! @author kongweiguang

use gpui_kit::{Bounds, Context, ElementId, Pixels, Window};
use kerminal_application::sessions::{TerminalCommandContext, UiSessionBinding};
use kerminal_domain::{
    identity::HostId,
    sessions::{SessionOwner, SessionTarget, SshSessionTarget},
};

use super::{RootView, TerminalTargetRequest, command_suggestions::TerminalSuggestionQueryState};

const SSH_FIXTURE_HOST_ID: &str = "suggestions-ssh-fixture";

/// 单帧状态只包含本地验收所需证据，正文不从渲染内容逆向推断。
pub struct SuggestionsAcceptanceState {
    pub binding: UiSessionBinding,
    pub pane_id: String,
    pub ssh_fixture: bool,
    pub focused: bool,
    pub input_idle: bool,
    pub context: Option<TerminalCommandContext>,
    pub ghost: Option<String>,
    pub menu_state: Option<&'static str>,
    pub menu_candidates: Vec<String>,
    pub pane_bounds: Bounds<Pixels>,
    pub cursor_x: f32,
    pub cursor_y: f32,
    pub cell_width: f32,
    pub line_height: f32,
}

/// 只开放本地终端及验收专用主机，避免诊断接口把任意真实 SSH pane 当作隔离样本。
///
/// @author kongweiguang
fn is_acceptance_target(target: Option<&SessionTarget>) -> bool {
    matches!(target, Some(SessionTarget::Local(_)))
        || matches!(target, Some(SessionTarget::Ssh(ssh)) if ssh.host_id.as_str() == SSH_FIXTURE_HOST_ID)
}

/// Kit 的 ElementId 观察与 GPUI debug selector 是不同索引；先读取实际组件树再兼容原生 selector。
fn observed_bounds(window: &Window, id: &str) -> Option<Bounds<Pixels>> {
    gpui_kit::base::test_support::find(window, &[], &ElementId::from(id.to_owned()))
        .filter(|element| element.visible())
        .map(|element| element.bounds())
        .or_else(|| window.debug_selector_bounds(id))
}

impl RootView {
    /// 失败诊断只报告各层身份是否就绪，不泄露 Shell 文本、路径或实际标识。
    pub fn suggestions_acceptance_diagnostic(&self, window: &mut Window) -> String {
        let Some(session) = self.active_session_id() else {
            return "no-active-session".to_owned();
        };
        let Some(registration) = self.pane_input_handlers.get(&session) else {
            return "no-input-handler".to_owned();
        };
        let binding = &registration.binding;
        if !self.is_active_suggestion_binding(binding) {
            return "inactive-binding".to_owned();
        }
        let SessionOwner::Ui { pane_id, .. } = &binding.owner else {
            return "non-ui-owner".to_owned();
        };
        let Some(pane) = self.workspace_runtime.pane(pane_id) else {
            return "no-pane".to_owned();
        };
        if !is_acceptance_target(pane.target.as_ref()) {
            return "not-acceptance-target".to_owned();
        }
        if self.terminal_snapshot_for_binding(binding).is_none() {
            return "no-render-snapshot".to_owned();
        }
        if observed_bounds(
            window,
            &format!("workspace-terminal-canvas-{}", pane_id.as_str()),
        )
        .is_none()
        {
            return "no-canvas-selector".to_owned();
        }
        let snapshot = self
            .terminal_snapshot_for_binding(binding)
            .expect("checked snapshot");
        format!(
            "ready: {}; cursor={:?},selection={},native-prediction={},context={:?}",
            self.terminal_command_suggestions.acceptance_diagnostic(),
            snapshot.cursor,
            snapshot.selection.is_some(),
            super::suggestion_overlay::has_native_prediction(snapshot),
            self.terminal_command_contexts
                .get(&session)
                .map(|cached| cached.context.availability)
        )
    }

    /// 返回隔离 pane 的生产状态和同一帧光标坐标；不触发快照或输入，避免与生产探测竞争。
    ///
    /// @author kongweiguang
    pub fn suggestions_acceptance_state(
        &self,
        window: &mut Window,
        cx: &Context<Self>,
    ) -> Option<SuggestionsAcceptanceState> {
        let session_id = self.active_session_id()?;
        let registration = self.pane_input_handlers.get(&session_id)?;
        let binding = registration.binding.clone();
        if !self.is_active_suggestion_binding(&binding) {
            return None;
        }
        let SessionOwner::Ui { pane_id, .. } = &binding.owner else {
            return None;
        };
        let pane = self.workspace_runtime.pane(pane_id)?;
        if !is_acceptance_target(pane.target.as_ref()) {
            return None;
        }
        let snapshot = self.terminal_snapshot_for_binding(&binding)?;
        let pane_bounds = observed_bounds(
            window,
            &format!("workspace-terminal-canvas-{}", pane_id.as_str()),
        )?;
        let cell_width = self.terminal_metrics.cell_width(window);
        let cursor = self.terminal_metrics.cell_origin(
            pane_bounds.origin,
            snapshot.cursor.row,
            snapshot.cursor.column,
            cell_width,
        );
        let menu = (self.suggestion_menu_binding.as_ref() == Some(&binding))
            .then(|| self.terminal_command_suggestions.menu_projection())
            .flatten();
        let menu_state = menu.as_ref().map(|menu| match menu.state {
            TerminalSuggestionQueryState::Closed => "closed",
            TerminalSuggestionQueryState::Loading => "loading",
            TerminalSuggestionQueryState::Ready => "ready",
            TerminalSuggestionQueryState::Empty => "empty",
            TerminalSuggestionQueryState::Failed => "failed",
            TerminalSuggestionQueryState::Unavailable(_) => "unavailable",
            TerminalSuggestionQueryState::InputTooLong => "input-too-long",
        });
        let menu_candidates = menu
            .as_ref()
            .map(|menu| {
                menu.candidates
                    .iter()
                    .map(|candidate| candidate.replacement_text.clone())
                    .collect()
            })
            .unwrap_or_default();
        Some(SuggestionsAcceptanceState {
            ssh_fixture: matches!(pane.target.as_ref(), Some(SessionTarget::Ssh(_))),
            focused: self.pane_input_binding_is_focused(&binding, window, cx),
            input_idle: self.session_commands.is_idle(&session_id),
            context: self.suggestion_context_for_binding(&binding),
            ghost: self
                .terminal_command_suggestions
                .inline_ghost()
                .filter(|ghost| ghost.binding == binding)
                .filter(|_| {
                    observed_bounds(window, &format!("suggestion-ghost-{}", pane_id.as_str()))
                        .is_some()
                })
                .map(|ghost| ghost.text),
            pane_id: pane_id.as_str().to_owned(),
            binding,
            menu_state,
            menu_candidates,
            pane_bounds,
            cursor_x: cursor.x.as_f32(),
            cursor_y: cursor.y.as_f32(),
            cell_width: cell_width.as_f32(),
            line_height: self.terminal_metrics.line_height,
        })
    }

    /// 只为固定验收主机走生产创建链路，不能借测试接口打开用户的其它连接。
    ///
    /// @author kongweiguang
    pub fn suggestions_acceptance_open_ssh(
        &mut self,
        host_id: HostId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if host_id.as_str() != SSH_FIXTURE_HOST_ID || self.is_creating_session {
            return false;
        }
        self.create_terminal_for_target(
            TerminalTargetRequest::ExplicitHost {
                host_id: host_id.clone(),
                target: SessionTarget::Ssh(SshSessionTarget {
                    host_id,
                    working_directory: None,
                    command: None,
                }),
            },
            window,
            cx,
        );
        true
    }
}
