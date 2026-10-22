//! Commands 目标名称不得脱离其不可变 UI binding 或 session generation。
//!
//! @author kongweiguang

use super::CommandsTargetPresentation;
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    sessions::SessionOwner,
};

/// 构造真实类型的 UI pane binding，确保测试覆盖 generation 比较而非字符串标签。
///
/// @author kongweiguang
fn binding(
    session_id: SessionId,
    tab_id: TabId,
    pane_id: PaneId,
    generation: u64,
) -> UiSessionBinding {
    UiSessionBinding {
        session_id,
        owner: SessionOwner::Ui { tab_id, pane_id },
        session_generation: generation,
    }
}

/// 旧的展示名称在重连 generation 变化后立即不可见，避免沿用旧 pane 资料。
///
/// @author kongweiguang
#[test]
fn label_is_available_only_for_its_exact_binding_generation() {
    let captured = binding(SessionId::new(), TabId::new(), PaneId::new(), 4);
    let presentation = CommandsTargetPresentation {
        binding: captured.clone(),
        label: "Workspace · PowerShell".to_owned(),
    };

    assert_eq!(
        presentation.label_for_binding(Some(&captured)),
        Some("Workspace · PowerShell")
    );

    let SessionOwner::Ui { tab_id, pane_id } = &captured.owner else {
        unreachable!("fixture uses UI ownership");
    };
    let reconnected = binding(
        captured.session_id.clone(),
        tab_id.clone(),
        pane_id.clone(),
        5,
    );
    assert!(
        presentation.label_for_binding(Some(&reconnected)).is_none(),
        "a new handler generation must not inherit the old target label"
    );
    assert!(presentation.label_for_binding(None).is_none());
}
