//! Root Canvas resize 只接受仍匹配注册输入 Entity 的完整 binding。
//!
//! @author kongweiguang

use super::*;
use crate::root::session_reconnect::SessionPanelObservation;
use kerminal_application::sessions::UiSessionBinding;
use kerminal_application::workspace::WorkspaceSessionTab;
use kerminal_domain::{
    identity::{AgentSessionId, PaneId, SessionId, TabId},
    sessions::{SessionLifecycle, SessionOwner, SessionTarget, TelnetSessionTarget, TerminalSize},
};

/// 建立 UI pane binding，代次来自受保护的会话上下文而非当前焦点。
fn ui_binding(session: &str, tab: &str, pane: &str, generation: u64) -> UiSessionBinding {
    UiSessionBinding {
        session_id: SessionId::parse(session).expect("测试 session id 有效"),
        owner: SessionOwner::Ui {
            tab_id: TabId::parse(tab).expect("测试 tab id 有效"),
            pane_id: PaneId::parse(pane).expect("测试 pane id 有效"),
        },
        session_generation: generation,
    }
}

/// 同标签的真实 close 尚待 ACK 时，旧 deferred probe 即使代次仍相等也不得恢复任何被关闭的 pane。
///
/// @author kongweiguang
#[test]
fn pending_tab_close_blocks_deferred_probe_for_ready_and_unstarted_siblings() {
    let tab_id = TabId::new();
    let ready_pane = PaneId::new();
    let deferred_pane = PaneId::new();
    let layout = kerminal_domain::workspace::TerminalLayoutNode::pane(ready_pane.clone())
        .split_pane(
            &ready_pane,
            deferred_pane.clone(),
            kerminal_domain::workspace::SplitDirection::Horizontal,
            "mixed-probe".to_owned(),
            false,
        )
        .expect("both mixed-close panes belong to the typed layout");
    let operation = WorkspaceResourceOperation::CloseTab {
        operation_id: "close-mixed-tab".to_owned(),
        tab_id: tab_id.clone(),
        tab: Box::new(WorkspaceSessionTab::Terminal {
            id: tab_id.clone(),
            tab_group_id: None,
            title: "mixed".to_owned(),
            machine_id: "local".to_owned(),
            layout,
        }),
        pane_ids: vec![ready_pane.clone(), deferred_pane.clone()],
    };
    assert!(restore_probe_generation_matches(None, None));
    for pane_id in [&ready_pane, &deferred_pane] {
        assert!(restore_probe_is_owned_by_close(
            Some(&operation),
            &tab_id,
            pane_id
        ));
    }
    assert!(!restore_probe_is_owned_by_close(
        Some(&operation),
        &TabId::new(),
        &deferred_pane,
    ));
}

/// 单 pane 关闭只阻断原身份，不能冻结同标签其它尚未显示的 pane。
///
/// @author kongweiguang
#[test]
fn pending_pane_close_blocks_only_its_exact_probe_identity() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let operation = WorkspaceResourceOperation::ClosePane {
        operation_id: "close-exact-pane".to_owned(),
        tab_id: tab_id.clone(),
        pane_id: pane_id.clone(),
    };
    assert!(restore_probe_is_owned_by_close(
        Some(&operation),
        &tab_id,
        &pane_id
    ));
    assert!(!restore_probe_is_owned_by_close(
        Some(&operation),
        &tab_id,
        &PaneId::new()
    ));
    assert!(!restore_probe_is_owned_by_close(None, &tab_id, &pane_id));
}

/// 构造固定 UI pane 或 Agent owner 的几何事件。
fn request(binding: UiSessionBinding) -> CanvasResizeRequest {
    CanvasResizeRequest {
        binding,
        size: TerminalSize::new(120, 36, 960, 648).expect("测试尺寸有效"),
    }
}

/// 历史网格 resize 只接受当前只读 binding 的 Exited/Failed UI pane。
///
/// @author kongweiguang
#[test]
fn history_resize_requires_exact_ended_view_binding() {
    let binding = ui_binding("session-a", "tab-a", "pane-a", 4);
    let observation = |lifecycle| SessionPanelObservation {
        binding: binding.clone(),
        target: SessionTarget::Telnet(TelnetSessionTarget {
            host: "localhost".to_owned(),
            port: 23,
        }),
        lifecycle,
    };

    for lifecycle in [SessionLifecycle::Exited, SessionLifecycle::Failed] {
        assert!(history_resize_binding_is_current(
            &binding,
            Some(&binding),
            Some(&observation(lifecycle)),
        ));
    }
    for lifecycle in [
        SessionLifecycle::Starting,
        SessionLifecycle::Running,
        SessionLifecycle::Closing,
        SessionLifecycle::Closed,
    ] {
        assert!(!history_resize_binding_is_current(
            &binding,
            Some(&binding),
            Some(&observation(lifecycle)),
        ));
    }
    assert!(!history_resize_binding_is_current(
        &binding,
        None,
        Some(&observation(SessionLifecycle::Exited)),
    ));

    let mut stale = binding.clone();
    stale.session_generation += 1;
    assert!(!history_resize_binding_is_current(
        &stale,
        Some(&binding),
        Some(&observation(SessionLifecycle::Exited)),
    ));
}

/// 同一 pane 重连后，迟到的旧代次绘制回调不会 resize 新 PTY。
#[test]
fn stale_generation_is_rejected_after_reconnect() {
    let old = ui_binding("session-a", "tab-a", "pane-a", 1);
    let current = ui_binding("session-a", "tab-a", "pane-a", 2);

    assert!(!binding_matches_current(&request(old), &current));
}

/// 重试前拍摄的首帧尺寸不能启动或 resize 新代次。
///
/// @author kongweiguang
#[test]
fn stale_restore_probe_generation_is_rejected() {
    assert!(!restore_probe_generation_matches(None, Some(1)));
    assert!(!restore_probe_generation_matches(Some(4), Some(5)));
    assert!(restore_probe_generation_matches(Some(5), Some(5)));
    assert!(restore_probe_generation_matches(None, None));
}

/// pane 被另一 session 接管时，旧 Canvas 不能只凭相同 pane ID 调整新 session。
#[test]
fn replaced_session_is_rejected_even_when_owner_is_same() {
    let old = ui_binding("session-old", "tab-a", "pane-a", 1);
    let current = ui_binding("session-new", "tab-a", "pane-a", 1);

    assert!(!binding_matches_current(&request(old), &current));
}

/// Agent owner 变化后，迟到 resize 不会转发到右栏当前选中的另一个 Agent。
#[test]
fn replaced_agent_owner_is_rejected() {
    let old = UiSessionBinding {
        session_id: SessionId::parse("session-agent-a").expect("测试 session id 有效"),
        owner: SessionOwner::Agent {
            agent_session_id: AgentSessionId::parse("agent-a").expect("测试 agent id 有效"),
        },
        session_generation: 9,
    };
    let current = UiSessionBinding {
        session_id: SessionId::parse("session-agent-b").expect("测试 session id 有效"),
        owner: SessionOwner::Agent {
            agent_session_id: AgentSessionId::parse("agent-b").expect("测试 agent id 有效"),
        },
        session_generation: 9,
    };

    assert!(!binding_matches_current(&request(old), &current));
}
