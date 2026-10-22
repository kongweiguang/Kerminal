//! 固定 pane 输入 binding、IME composition 与重连失效行为测试。
//!
//! @author kongweiguang

use super::*;
use kerminal_domain::{
    identity::{AgentSessionId, PaneId, SessionId, TabId},
    sessions::SessionOwner,
};

/// 为纯状态测试构造完整 UI owner 与重连 generation。
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

/// 构造 Agent 输入身份，确认共用类型会保留 Agent owner 与 session generation。
fn agent_binding(session: &str, agent: &str, generation: u64) -> UiSessionBinding {
    UiSessionBinding {
        session_id: SessionId::parse(session).expect("测试 session id 有效"),
        owner: SessionOwner::Agent {
            agent_session_id: AgentSessionId::parse(agent).expect("测试 agent id 有效"),
        },
        session_generation: generation,
    }
}

/// 多次预编辑保留同一组合代次，commit 事件携带原 pane 与 UTF-8 文本。
#[test]
fn composition_commit_keeps_binding_and_generation() {
    let target = ui_binding("session-a", "tab-a", "pane-a", 4);
    let mut state = PaneInputState::new(target.clone());

    let generation = state
        .update_composition("A😀中", Some(3..4))
        .expect("有效 handler 应开始组合");
    assert_eq!(
        state
            .composition
            .as_ref()
            .map(ImeCompositionBinding::generation),
        Some(1)
    );
    assert_eq!(state.selection_bytes, 5..8);
    assert!(!state.can_send_kitty_associated_text());

    let (text, origin) = state.commit_text("结果").expect("组合提交应被接受");
    assert_eq!(text, "结果");
    assert_eq!(origin, ImeTextSource::Composition { generation });
    assert_eq!(state.binding, target);
    assert!(state.can_send_kitty_associated_text());
}

/// 切换或重连先 invalidate 旧实体；旧回调不得进入新 generation 的输入通道。
#[test]
fn invalidated_old_generation_cannot_retarget_commit() {
    let old_binding = ui_binding("session-a", "tab-a", "pane-a", 4);
    let new_binding = ui_binding("session-a", "tab-a", "pane-a", 5);
    let mut old_state = PaneInputState::new(old_binding.clone());
    old_state.update_composition("迟到", None);
    old_state.invalidate();

    assert_eq!(old_state.binding, old_binding);
    assert!(old_state.commit_text("旧提交").is_none());
    assert!(!old_state.can_send_kitty_associated_text());

    let mut new_state = PaneInputState::new(new_binding.clone());
    let (text, source) = new_state
        .commit_text("新文本")
        .expect("新 handler 可接收输入");
    assert_eq!(text, "新文本");
    assert_eq!(source, ImeTextSource::Plain);
    assert_eq!(new_state.binding, new_binding);
}

/// 显式取消 composition 后恢复普通文本与 Kitty 关联文本，不依赖时间窗口猜测。
#[test]
fn cancelled_composition_restores_plain_input() {
    let target = ui_binding("session-a", "tab-a", "pane-a", 2);
    let mut state = PaneInputState::new(target);
    state.update_composition("未提交", None);
    assert!(!state.can_send_kitty_associated_text());

    state.cancel_composition();
    assert!(state.buffer.is_empty());
    assert!(state.can_send_kitty_associated_text());
    assert_eq!(
        state.commit_text("普通文本").unwrap().1,
        ImeTextSource::Plain
    );
}

/// Agent owner 仍携带应用层验证所需的准确 SessionId、owner 和 generation。
#[test]
fn agent_binding_is_preserved_without_live_focus_lookup() {
    let target = agent_binding("session-agent", "agent-a", 8);
    let mut state = PaneInputState::new(target.clone());

    let committed = state
        .commit_text("agent 输入")
        .expect("Agent handler 可提交文本");
    assert_eq!(committed.1, ImeTextSource::Plain);
    assert_eq!(state.binding, target);
}

/// 失焦后的迟到提交不能被当作 Plain；只有重新聚焦后对应的新物理按键可开放一次提交。
///
/// @author kongweiguang
#[test]
fn blur_rejects_late_commit_until_matching_fresh_keydown() {
    let target = ui_binding("session-a", "tab-a", "pane-a", 4);
    let mut state = PaneInputState::new(target);
    state.update_composition("预编辑", None);

    state.lose_focus();
    assert!(state.commit_text("迟到提交").is_none());
    assert!(state.focus_loss_pending);

    state.note_plain_keydown_after_focus_loss(Some("新字符"));
    let (text, source) = state
        .commit_text("新字符")
        .expect("新焦点下同一按键的 plain 回调应可接受");
    assert_eq!(text, "新字符");
    assert_eq!(source, ImeTextSource::Plain);
    assert!(!state.focus_loss_pending);
}

/// 新焦点开始新 composition 会建立新代次，失焦时的旧预编辑不会污染它。
///
/// @author kongweiguang
#[test]
fn refocused_handler_can_start_a_new_composition() {
    let target = ui_binding("session-a", "tab-a", "pane-a", 4);
    let mut state = PaneInputState::new(target);
    let old_generation = state.update_composition("旧组合", None).unwrap();

    state.lose_focus();
    let new_generation = state.update_composition("新组合", None).unwrap();
    assert!(new_generation > old_generation);

    let (text, source) = state.commit_text("新提交").unwrap();
    assert_eq!(text, "新提交");
    assert_eq!(
        source,
        ImeTextSource::Composition {
            generation: new_generation
        }
    );
}

/// 终端按键只接受完全相同的 session、owner 与重连代次，防止跨 Tab/split/Agent 路由。
///
/// @author kongweiguang
#[test]
fn focused_input_gate_requires_exact_binding_identity() {
    let original = ui_binding("session-a", "tab-a", "pane-a", 4);
    let other_tab = ui_binding("session-a", "tab-b", "pane-a", 4);
    let other_pane = ui_binding("session-a", "tab-a", "pane-b", 4);
    let other_generation = ui_binding("session-a", "tab-a", "pane-a", 5);
    let agent = agent_binding("session-a", "agent-a", 4);

    assert!(input_binding_is_focused(
        &original,
        &original,
        Some(&original),
        true
    ));
    for stale in [&other_tab, &other_pane, &other_generation, &agent] {
        assert!(!input_binding_is_focused(
            &original,
            &original,
            Some(stale),
            true
        ));
    }
    assert!(!input_binding_is_focused(
        &original,
        &original,
        Some(&original),
        false
    ));
}

/// 候选框投影拒绝旧 generation 和不匹配的快照 session，不能借用另一个 pane 光标。
#[test]
fn cursor_projection_requires_exact_binding_and_session() {
    let original = ui_binding("session-a", "tab-a", "pane-a", 4);
    let reconnected = ui_binding("session-a", "tab-a", "pane-a", 5);
    let other_session = SessionId::parse("session-b").expect("测试 session id 有效");
    let session_a = SessionId::parse("session-a").expect("测试 session id 有效");

    assert!(projection_matches_binding(
        &original, &original, &session_a, true
    ));
    assert!(!projection_matches_binding(
        &original,
        &reconnected,
        &session_a,
        true
    ));
    assert!(!projection_matches_binding(
        &original,
        &original,
        &other_session,
        true
    ));
    assert!(!projection_matches_binding(
        &original, &original, &session_a, false
    ));
}

/// UTF-16 偏移不会切开 UTF-8 字符或 emoji surrogate pair，来回转换保持字符边界。
#[test]
fn utf16_selection_round_trip_keeps_unicode_boundaries() {
    let text = "A😀中";
    let bytes = utf16_range_to_bytes(text, 1..4);
    assert_eq!(bytes, 1..8);
    assert_eq!(bytes_to_utf16_range(text, bytes), 1..4);

    assert_eq!(utf16_range_to_bytes(text, 2..3), 1..5);
    assert_eq!(utf16_range_to_bytes(text, 99..101), text.len()..text.len());
}
