//! SOCKS 应用面板空态投影的纯状态回归测试。
//!
//! @author kongweiguang

use super::super::{ProxyCurrentApplication, ProxyCurrentAvailability};
use super::{ProxyFutureProjection, current_recovery_actions, should_suppress_compact_empty_hint};
use kerminal_domain::identity::HostId;

/// 干净空态只保留面板占位，运行态与恢复状态由详细视图负责承载。
///
/// @author kongweiguang
#[test]
fn clean_unapplied_empty_panel_suppresses_redundant_idle_hint() {
    assert!(should_suppress_compact_empty_hint(
        false,
        None,
        None,
        &ProxyFutureProjection::Unavailable { host_id: None },
        false,
    ));
    assert!(should_suppress_compact_empty_hint(
        false,
        Some(&host("host-a")),
        Some(ProxyCurrentApplication::NotApplied),
        &ProxyFutureProjection::Ready {
            host_id: host("host-a"),
            forward_id: None,
            revision: 2,
        },
        false,
    ));
    assert!(!should_suppress_compact_empty_hint(
        true,
        None,
        None,
        &ProxyFutureProjection::Unavailable { host_id: None },
        false,
    ));
}

/// 当前/未来应用、异步任务、操作回执、历史结果及可审阅脚本都必须保留完整恢复视图。
///
/// @author kongweiguang
#[test]
fn empty_forward_list_keeps_recovery_and_in_flight_states_visible() {
    let unresolved_futures = [
        ProxyFutureProjection::Loading {
            host_id: host("host-a"),
        },
        ProxyFutureProjection::OutcomeUnknown {
            host_id: host("host-a"),
        },
        ProxyFutureProjection::Unavailable {
            host_id: Some(host("host-a")),
        },
        ProxyFutureProjection::Ready {
            host_id: host("host-a"),
            forward_id: Some("forward-a".to_owned()),
            revision: 3,
        },
    ];
    for future in unresolved_futures {
        assert!(!should_suppress_compact_empty_hint(
            false, None, None, &future, false,
        ));
    }

    assert!(!should_suppress_compact_empty_hint(
        false,
        Some(&host("host-a")),
        None,
        &ProxyFutureProjection::Unavailable { host_id: None },
        false,
    ));
    assert!(!should_suppress_compact_empty_hint(
        false,
        None,
        None,
        &ProxyFutureProjection::Ready {
            host_id: host("host-a"),
            forward_id: None,
            revision: 3,
        },
        false,
    ));

    for current in [
        ProxyCurrentApplication::Applied,
        ProxyCurrentApplication::OutcomeUnknown,
    ] {
        assert!(!should_suppress_compact_empty_hint(
            false,
            None,
            Some(current),
            &ProxyFutureProjection::Unavailable { host_id: None },
            false,
        ));
    }

    assert!(!should_suppress_compact_empty_hint(
        false,
        None,
        None,
        &ProxyFutureProjection::Unavailable { host_id: None },
        true,
    ));
}

/// 原 forward 停止后，已核对的应用仍可恢复；未知结果只显示核对入口，不直接执行恢复。
///
/// @author kongweiguang
#[test]
fn stopped_forward_keeps_restore_or_reconciliation_entry_reachable() {
    let future = ProxyFutureProjection::Unavailable {
        host_id: Some(host("host-a")),
    };
    for (application, restore, reconcile) in [
        (ProxyCurrentApplication::Applied, true, false),
        (ProxyCurrentApplication::OutcomeUnknown, false, true),
    ] {
        assert!(!should_suppress_compact_empty_hint(
            false,
            Some(&host("host-a")),
            Some(application),
            &future,
            true,
        ));
        assert_eq!(
            current_recovery_actions(
                Some(application),
                true,
                Some(ProxyCurrentAvailability::Ready),
                false,
            ),
            super::CurrentRecoveryActions { restore, reconcile },
        );
    }
}

/// 缺少唯一原记录、提示符未就绪或存在请求时，恢复动作必须关闭。
///
/// @author kongweiguang
#[test]
fn recovery_actions_fail_closed_without_unique_record_or_ready_prompt() {
    assert_eq!(
        current_recovery_actions(
            Some(ProxyCurrentApplication::Applied),
            false,
            Some(ProxyCurrentAvailability::Ready),
            false,
        ),
        Default::default(),
    );
    assert_eq!(
        current_recovery_actions(
            Some(ProxyCurrentApplication::OutcomeUnknown),
            true,
            Some(ProxyCurrentAvailability::PromptNotReady),
            false,
        ),
        Default::default(),
    );
    assert_eq!(
        current_recovery_actions(
            Some(ProxyCurrentApplication::Applied),
            true,
            Some(ProxyCurrentAvailability::Ready),
            true,
        ),
        Default::default(),
    );
}

/// 构造隔离用 HostId，测试不读取真实配置或运行时目标。
///
/// @author kongweiguang
fn host(value: &str) -> HostId {
    HostId::parse(value).expect("test host id is valid")
}
