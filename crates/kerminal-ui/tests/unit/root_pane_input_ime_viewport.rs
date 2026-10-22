//! IME candidate geometry and cancellation regressions for pane input handlers.
//!
//! @author kongweiguang

use super::*;
use kerminal_application::preferences::TerminalSettings;
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    sessions::SessionOwner,
};
use kerminal_terminal::snapshot::TerminalCursorShape;

/// 使用生产默认度量构造投影，确保回归测试验证候选定位共用的视口门禁。
///
/// @author kongweiguang
fn ime_projection(scrollback_offset: usize) -> PaneImeProjection {
    PaneImeProjection {
        cursor: TerminalCursor {
            row: 2,
            column: 5,
            shape: TerminalCursorShape::Block,
            style_from_terminal: false,
            blinking: false,
            visible: true,
        },
        scrollback_offset,
        metrics: TerminalMetrics::from_settings(&TerminalSettings::default(), &[]),
    }
}

/// 历史快照仍有 viewport 光标坐标，但不能为 IME 候选框提供锚点。
///
/// @author kongweiguang
#[test]
fn historical_ime_projection_cannot_supply_candidate_anchor() {
    let historical = ime_projection(7);
    let live = ime_projection(0);

    assert!(!historical.allows_candidate_position());
    assert!(live.allows_candidate_position());
}

/// 取消会清空本地 marked text；unmark 回调自身不得把预编辑内容变成终端提交。
///
/// @author kongweiguang
#[test]
fn cancelling_marked_text_clears_preedit_without_commit_state() {
    let binding = UiSessionBinding {
        session_id: SessionId::parse("session-ime").expect("测试 session id 有效"),
        owner: SessionOwner::Ui {
            tab_id: TabId::parse("tab-ime").expect("测试 tab id 有效"),
            pane_id: PaneId::parse("pane-ime").expect("测试 pane id 有效"),
        },
        session_generation: 3,
    };
    let mut state = PaneInputState::new(binding);

    let _ = state.update_composition("取消的预编辑", None);
    assert!(state.composition.is_some());
    assert_eq!(state.buffer, "取消的预编辑");

    state.cancel_composition();
    assert!(state.composition.is_none());
    assert!(state.buffer.is_empty());
    assert!(state.can_send_kitty_associated_text());
}
