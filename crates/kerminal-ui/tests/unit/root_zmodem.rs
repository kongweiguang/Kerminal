//! ZMODEM 分屏交互的会话身份和取消门禁回归。
//!
//! @author kongweiguang

use kerminal_application::sessions::{
    UiSessionBinding, ZmodemDirection, ZmodemPhase, ZmodemStatus,
};
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    sessions::SessionOwner,
};

use super::ZmodemUiState;

/// 真实领域 ID 与代次共同决定选择器结果归属。
fn binding() -> UiSessionBinding {
    UiSessionBinding {
        session_id: SessionId::new(),
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
        session_generation: 1,
    }
}

/// 精简投影保留影响取消和选择权限的字段，避免测试依赖文件 I/O。
fn transfer(id: u64, phase: ZmodemPhase) -> ZmodemStatus {
    ZmodemStatus {
        id,
        direction: ZmodemDirection::Upload,
        phase,
        file_name: None,
        bytes: 0,
        total: None,
        completed: 0,
        error: None,
        save_directory: None,
    }
}

/// 对话框返回时重新校验代次、传输身份和阶段，旧结果不能落到新连接。
#[test]
fn picker_result_requires_original_binding_transfer_and_phase() {
    let mut state = ZmodemUiState::default();
    let current = binding();
    let _ = state.update(
        current.clone(),
        Some(transfer(7, ZmodemPhase::WaitingForSelection)),
    );
    assert!(state.accepts_selection(&current, 7));
    assert!(!state.accepts_selection(&current, 8));
    let mut reconnected = current.clone();
    reconnected.session_generation += 1;
    assert!(!state.accepts_selection(&reconnected, 7));
    let _ = state.update(
        current.clone(),
        Some(transfer(7, ZmodemPhase::Transferring)),
    );
    assert!(!state.accepts_selection(&current, 7));
    let _ = state.update(
        reconnected.clone(),
        Some(transfer(8, ZmodemPhase::WaitingForSelection)),
    );
    let _ = state.update(current.clone(), None);
    assert!(state.accepts_selection(&reconnected, 8));
    assert!(!state.accepts_selection(&current, 7));
}

/// Enter 仅对原绑定的等待选择状态开放，换代和其他阶段继续保持终端原有行为。
#[test]
fn selection_shortcut_requires_waiting_original_binding() {
    let mut state = ZmodemUiState::default();
    let current = binding();
    assert_eq!(state.selection(&current), None);
    let _ = state.update(
        current.clone(),
        Some(transfer(7, ZmodemPhase::WaitingForSelection)),
    );
    assert_eq!(
        state.selection(&current),
        Some((7, ZmodemDirection::Upload))
    );
    let mut stale = current.clone();
    stale.session_generation += 1;
    assert_eq!(state.selection(&stale), None);
    let _ = state.update(
        current.clone(),
        Some(transfer(7, ZmodemPhase::Transferring)),
    );
    assert_eq!(state.selection(&current), None);
}

/// 对话框在释放后才能打开，按住、切换分屏和丢失释放时不会把旧动作交给新目标。
#[test]
fn enter_claim_survives_focus_change_and_fresh_press_clears_it() {
    let mut state = ZmodemUiState::default();
    let original = binding();
    let other = binding();
    let _ = state.update(
        original.clone(),
        Some(transfer(9, ZmodemPhase::WaitingForSelection)),
    );
    assert!(state.claim_enter(&original));
    assert!(state.enter_is_pending());
    assert_eq!(
        state.take_enter(),
        Some((original.clone(), 9, ZmodemDirection::Upload))
    );
    assert!(!state.enter_is_pending());
    assert!(state.claim_enter(&original));
    let _ = state.take_enter();
    assert!(!state.claim_enter(&other));
    assert!(!state.enter_is_pending());
    let _ = state.update(original.clone(), Some(transfer(9, ZmodemPhase::Cancelled)));
    assert!(!state.claim_enter(&original));
}

/// Windows 能取消失效原生对话框；其他平台保留在途任务直到系统对话框结束。
#[test]
fn stale_picker_lifetime_matches_platform_cancellation() {
    let mut state = ZmodemUiState::default();
    let current = binding();
    let key = (current.session_id.clone(), current.session_generation, 7);
    let _ = state.update(
        current.clone(),
        Some(transfer(7, ZmodemPhase::WaitingForSelection)),
    );
    state.pickers.insert(key.clone(), gpui_kit::Task::ready(()));
    let _ = state.update(current, Some(transfer(7, ZmodemPhase::Cancelled)));
    assert_eq!(
        state.pickers.contains_key(&key),
        !cfg!(target_os = "windows")
    );
}

/// 取消按键只在选择或传输阶段生效，成功、失败和重连状态保留普通键盘操作。
#[test]
fn cancellation_keyboard_gate_only_claims_active_transfer() {
    let mut state = ZmodemUiState::default();
    let current = binding();
    assert_eq!(state.cancellable_id(&current), None);
    for phase in [ZmodemPhase::WaitingForSelection, ZmodemPhase::Transferring] {
        let _ = state.update(current.clone(), Some(transfer(7, phase)));
        assert_eq!(state.cancellable_id(&current), Some(7));
    }
    for phase in [
        ZmodemPhase::Cancelling,
        ZmodemPhase::Finished,
        ZmodemPhase::Cancelled,
        ZmodemPhase::Failed,
        ZmodemPhase::NeedsReconnect,
    ] {
        let _ = state.update(current.clone(), Some(transfer(7, phase)));
        assert_eq!(state.cancellable_id(&current), None);
    }
}
