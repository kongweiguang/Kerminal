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
