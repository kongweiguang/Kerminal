//! 输入法组合绑定契约测试，不启动原生窗口或创建终端会话。
//!
//! @author kongweiguang

#[path = "../../src/root/terminal_ime.rs"]
mod terminal_ime;

use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    sessions::SessionOwner,
};
use terminal_ime::{ImeInputDecision, ImeTextSource, TerminalImeState};

/// 构造具有明确 pane owner 和 session generation 的目标，测试不依赖真实 Root 状态。
fn binding(session: &str, tab: &str, pane: &str, generation: u64) -> UiSessionBinding {
    UiSessionBinding {
        session_id: SessionId::parse(session).expect("测试 session id 有效"),
        owner: SessionOwner::Ui {
            tab_id: TabId::parse(tab).expect("测试 tab id 有效"),
            pane_id: PaneId::parse(pane).expect("测试 pane id 有效"),
        },
        session_generation: generation,
    }
}

/// 同一组合的多次 preedit 更新保留 token，普通文本不可重复进入活跃组合。
///
/// @author kongweiguang
#[test]
fn preedit_updates_keep_binding_and_commit_returns_original_target() {
    let target = binding("session-a", "tab-a", "pane-a", 4);
    let mut state = TerminalImeState::new(target.clone());

    let first = state.begin_or_update_composition();
    let updated = state.begin_or_update_composition();
    assert_eq!(first, updated);
    assert_eq!(
        state.accept_plain_text(Some(&target), "中文"),
        ImeInputDecision::CompositionActive
    );

    let decision = state.commit_composition(Some(&updated), Some(&target), "中文");
    let ImeInputDecision::Accepted(committed) = decision else {
        panic!("原绑定上的有效组合应可提交");
    };
    assert_eq!(committed.session, target);
    assert_eq!(committed.text, "中文");
    assert_eq!(
        committed.source,
        ImeTextSource::Composition { generation: 1 }
    );
}

/// Handler 因重连代次或 pane owner 失焦后，旧 composition token 不得被新目标接受。
///
/// @author kongweiguang
#[test]
fn stale_composition_is_rejected_after_binding_changes() {
    let old_target = binding("session-a", "tab-a", "pane-a", 4);
    let new_target = binding("session-a", "tab-a", "pane-a", 5);
    let mut state = TerminalImeState::new(old_target.clone());
    let old_token = state.begin_or_update_composition();

    // 每个 handler 固定一个 binding；目标变化时真实调用方结束旧 handler 并撤销组合。
    let retired = state
        .cancel_for_focus_loss()
        .expect("失焦的旧 handler 应返回撤销 token");
    assert_eq!(retired, old_token);
    assert_eq!(
        state.commit_composition(Some(&old_token), Some(&new_target), "迟到文本"),
        ImeInputDecision::Stale {
            expected: old_target,
        }
    );
}

/// 旧组合回调不得把文本送到切换后的另一个会话。
#[test]
fn old_session_commit_cannot_be_retargeted_to_new_focus() {
    let old_target = binding("session-a", "tab-a", "pane-a", 2);
    let new_target = binding("session-b", "tab-b", "pane-b", 1);
    let mut state = TerminalImeState::new(old_target.clone());
    let token = state.begin_or_update_composition();

    assert_eq!(
        state.commit_composition(Some(&token), Some(&new_target), "迟到提交"),
        ImeInputDecision::Stale {
            expected: old_target,
        }
    );
}

/// Pane close 撤销旧 token 后，迟到回调不能清除同一 handler 后续开始的新组合。
///
/// @author kongweiguang
#[test]
fn stale_token_does_not_cancel_a_newer_composition() {
    let target = binding("session-a", "tab-a", "pane-a", 2);
    let mut state = TerminalImeState::new(target.clone());
    let old_token = state.begin_or_update_composition();
    assert_eq!(state.cancel_for_close(), Some(old_token.clone()));
    let current_token = state.begin_or_update_composition();

    assert!(current_token.generation() > old_token.generation());
    assert!(matches!(
        state.commit_composition(Some(&old_token), Some(&target), "旧文本"),
        ImeInputDecision::Stale { .. }
    ));
    assert_eq!(
        state.accept_plain_text(Some(&target), "duplicate"),
        ImeInputDecision::CompositionActive
    );
    assert!(matches!(
        state.commit_composition(Some(&current_token), Some(&target), "新文本"),
        ImeInputDecision::Accepted(_)
    ));
}

/// 组合期间拒绝重复普通文本，真实取消后允许绑定正确的普通文本。
///
/// Kitty 关联文本的生产门禁由 PaneInputState 单独覆盖，避免镜像测试状态 helper。
/// @author kongweiguang
#[test]
fn composition_suppresses_duplicate_text_but_plain_input_recovers_after_cancel() {
    let target = binding("session-a", "tab-a", "pane-a", 2);
    let mut state = TerminalImeState::new(target.clone());
    let _token = state.begin_or_update_composition();

    assert_eq!(
        state.accept_plain_text(Some(&target), "n"),
        ImeInputDecision::CompositionActive
    );
    assert_eq!(
        state.accept_plain_text(Some(&target), "duplicate"),
        ImeInputDecision::CompositionActive
    );

    assert!(state.cancel_for_close().is_some());
    assert!(matches!(
        state.accept_plain_text(Some(&target), "x"),
        ImeInputDecision::Accepted(_)
    ));
}

/// 失去焦点的 handler 不能把普通平台文本按当前焦点重绑定。
#[test]
fn stale_handler_plain_text_is_not_retargeted() {
    let old_target = binding("session-a", "tab-a", "pane-a", 2);
    let new_target = binding("session-b", "tab-b", "pane-b", 1);
    let state = TerminalImeState::new(old_target.clone());

    assert_eq!(
        state.accept_plain_text(Some(&new_target), "unexpected"),
        ImeInputDecision::Stale {
            expected: old_target,
        }
    );
}

/// 缺少组合 token 的文本回调不可伪装成有效 IME commit。
#[test]
fn unbound_composition_commit_is_rejected() {
    let target = binding("session-a", "tab-a", "pane-a", 2);
    let mut state = TerminalImeState::new(target.clone());

    assert_eq!(
        state.commit_composition(None, Some(&target), "未绑定文本"),
        ImeInputDecision::Unbound
    );
}
