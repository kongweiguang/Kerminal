//! 外部协议动作只在有新鲜、受支持的状态时开放。
//!
//! @author kongweiguang

use kerminal_application::platform::ExternalProtocolStatus;

use super::{ExternalProtocolAction, external_protocol_action};

/// 注册动作要求服务、支持状态和当前状态都已确认。
#[test]
fn offers_register_only_for_confirmed_unregistered_state() {
    assert_eq!(
        external_protocol_action(
            true,
            Some(ExternalProtocolStatus {
                supported: true,
                registered: false,
            }),
            false,
            false,
        ),
        Some(ExternalProtocolAction::Register)
    );
    assert_eq!(
        external_protocol_action(
            true,
            Some(ExternalProtocolStatus {
                supported: true,
                registered: true,
            }),
            false,
            false,
        ),
        Some(ExternalProtocolAction::Unregister)
    );
}

/// 未装配、不支持、操作中或待回读时不允许重复提交系统变更。
#[test]
fn blocks_actions_until_status_is_read_back() {
    let ready = Some(ExternalProtocolStatus {
        supported: true,
        registered: false,
    });
    assert_eq!(external_protocol_action(false, ready, false, false), None);
    assert_eq!(external_protocol_action(true, None, false, false), None);
    assert_eq!(
        external_protocol_action(
            true,
            Some(ExternalProtocolStatus {
                supported: false,
                registered: false,
            }),
            false,
            false,
        ),
        None
    );
    assert_eq!(external_protocol_action(true, ready, true, false), None);
    assert_eq!(external_protocol_action(true, ready, false, true), None);
}
