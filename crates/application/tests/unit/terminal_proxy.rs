//! 当前/后续终端代理用例的独立测试。
//!
//! @author kongweiguang

use kerminal_domain::{
    connections::RemoteShellDialect,
    identity::{HostId, PaneId, SessionId, TabId},
    operations::ForwardKind,
    sessions::SessionOwner,
};

use crate::operations::terminal_proxy::{
    FutureAttemptClaim, FutureAttemptKey, FutureAttemptRetryClaim, FutureProxyApplyRequest,
    FutureProxyPolicyStore, ProxyScriptExecutionContext, TerminalCommandAvailability,
    TerminalCommandContext, TerminalProxyApplyOutcome, TerminalProxyApplyResult,
    TerminalProxyUnavailableReason, scripts,
};
use crate::sessions::ShellCommandUnknownReason;

/// 测试使用稳定的非空标识，避免把随机 ID 生成行为混入策略断言。
fn host_id(value: &str) -> HostId {
    HostId::parse(value).expect("测试主机标识有效")
}

/// 测试使用稳定的非空标识，避免把随机 ID 生成行为混入策略断言。
fn session_id(value: &str) -> SessionId {
    SessionId::parse(value).expect("测试会话标识有效")
}

/// 为重试状态测试创建策略已启用且尚未开始的唯一尝试。
fn retryable_attempt(
    store: &FutureProxyPolicyStore,
    host: &HostId,
    session: &str,
) -> FutureAttemptKey {
    store
        .set(FutureProxyApplyRequest {
            host_id: host.clone(),
            expected_revision: 0,
            forward_id: Some("forward-a".to_owned()),
        })
        .expect("测试策略有效");
    let key = FutureAttemptKey {
        session_id: session_id(session),
        session_generation: 7,
        policy_revision: 1,
        forward_id: "forward-a".to_owned(),
    };
    store
        .claim_attempt(host, "forward-a", key.clone())
        .expect("测试尝试可被领取");
    key
}

/// 将 typed 代理结论包装成分类测试所需的最小结果。
fn apply_result(outcome: TerminalProxyApplyOutcome) -> TerminalProxyApplyResult {
    TerminalProxyApplyResult {
        forward_id: "forward-a".to_owned(),
        session_id: session_id("session-a"),
        session_generation: 7,
        outcome,
        history_recorded: None,
        history_skip_reason: None,
        history_error_code: None,
    }
}

/// 创建带稳定 owner 和 generation 的提示符快照，测试只覆盖领域边界映射。
fn clean_context() -> TerminalCommandContext {
    TerminalCommandContext {
        session_id: session_id("session-a"),
        owner: SessionOwner::Ui {
            tab_id: TabId::parse("tab-a").expect("测试 Tab 标识有效"),
            pane_id: PaneId::parse("pane-a").expect("测试 Pane 标识有效"),
        },
        session_generation: 7,
        prompt_epoch: 11,
        input_generation: 19,
        shell_dialect: RemoteShellDialect::Posix,
        prompt_ready: true,
        prompt_line_empty: true,
        alternate_screen: false,
        availability: TerminalCommandAvailability::Available,
    }
}

#[test]
/// 策略更新采用 revision CAS，避免旧 UI 面板覆盖最新选择。
fn future_policy_uses_revision_compare_and_swap() {
    let store = FutureProxyPolicyStore::new();
    let host = host_id("host-a");
    let initial = store.state(&host);
    assert_eq!(initial.revision, 0);
    assert_eq!(initial.forward_id, None);

    let selected = store
        .set(FutureProxyApplyRequest {
            host_id: host.clone(),
            expected_revision: initial.revision,
            forward_id: Some("forward-a".to_owned()),
        })
        .expect("初次选择转发成功");
    assert_eq!(selected.revision, 1);
    assert_eq!(selected.forward_id.as_deref(), Some("forward-a"));

    assert!(
        store
            .set(FutureProxyApplyRequest {
                host_id: host.clone(),
                expected_revision: initial.revision,
                forward_id: None,
            })
            .is_err()
    );

    let disabled = store
        .set(FutureProxyApplyRequest {
            host_id: host,
            expected_revision: selected.revision,
            forward_id: None,
        })
        .expect("显式关闭策略成功");
    assert_eq!(disabled.revision, 2);
    assert_eq!(disabled.forward_id, None);
}

#[test]
/// 转发停止应清除同 forward 的策略并取消运行中的等待任务。
fn stopping_forward_cancels_only_its_policy_attempts() {
    let store = FutureProxyPolicyStore::new();
    let host_a = host_id("host-a");
    let host_b = host_id("host-b");
    store
        .set(FutureProxyApplyRequest {
            host_id: host_a.clone(),
            expected_revision: 0,
            forward_id: Some("forward-a".to_owned()),
        })
        .expect("host A 策略有效");
    store
        .set(FutureProxyApplyRequest {
            host_id: host_b.clone(),
            expected_revision: 0,
            forward_id: Some("forward-b".to_owned()),
        })
        .expect("host B 策略有效");
    let attempt = FutureAttemptKey {
        session_id: session_id("session-a"),
        session_generation: 3,
        policy_revision: 1,
        forward_id: "forward-a".to_owned(),
    };
    let cancellation = store
        .claim_attempt(&host_a, "forward-a", attempt.clone())
        .expect("唯一尝试可被领取");
    assert!(store.policy_is_current(&host_a, 1, &attempt));

    assert_eq!(store.cancel_by_forward("forward-a"), 2);
    assert!(cancellation.is_cancelled());
    assert_eq!(store.state(&host_a).forward_id, None);
    assert_eq!(store.state(&host_a).revision, 2);
    assert_eq!(
        store.state(&host_b).forward_id.as_deref(),
        Some("forward-b")
    );
    assert!(!store.policy_is_current(&host_a, 1, &attempt));
}

#[test]
/// 同一会话代次与策略版本的重放保持拒绝，新的 revision 才能领取新尝试。
fn repeated_ready_event_cannot_claim_the_same_attempt_twice() {
    let store = FutureProxyPolicyStore::new();
    let host = host_id("host-a");
    store
        .set(FutureProxyApplyRequest {
            host_id: host.clone(),
            expected_revision: 0,
            forward_id: Some("forward-a".to_owned()),
        })
        .expect("策略创建成功");
    let attempt = FutureAttemptKey {
        session_id: session_id("session-a"),
        session_generation: 1,
        policy_revision: 1,
        forward_id: "forward-a".to_owned(),
    };
    let _cancellation = store
        .claim_attempt(&host, "forward-a", attempt.clone())
        .expect("首次事件领取成功");

    assert!(matches!(
        store.claim_attempt(&host, "forward-a", attempt),
        Err(FutureAttemptClaim::AlreadyProcessed)
    ));
}

#[test]
/// 只让 typed 提示符瞬态结果获得重试授权，未知、失败和其它未应用原因均不开放重放。
fn future_retry_is_limited_to_typed_prompt_transient_results() {
    for reason in [
        TerminalProxyUnavailableReason::PromptNotReady,
        TerminalProxyUnavailableReason::PromptNotEmpty,
        TerminalProxyUnavailableReason::AlternateScreen,
    ] {
        let result = apply_result(TerminalProxyApplyOutcome::NotApplied { reason });
        assert_eq!(super::retryable_prompt_reason(&result), Some(reason));
    }

    for reason in [
        TerminalProxyUnavailableReason::InitialCommandPresent,
        TerminalProxyUnavailableReason::ShellIntegrationUnavailable,
        TerminalProxyUnavailableReason::PolicyChanged,
        TerminalProxyUnavailableReason::AlreadyApplied,
    ] {
        let result = apply_result(TerminalProxyApplyOutcome::NotApplied { reason });
        assert_eq!(super::retryable_prompt_reason(&result), None);
    }
    assert_eq!(
        super::retryable_prompt_reason(&apply_result(TerminalProxyApplyOutcome::Applied)),
        None
    );
    assert_eq!(
        super::retryable_prompt_reason(&apply_result(TerminalProxyApplyOutcome::Failed {
            exit_code: 1,
        })),
        None
    );
    assert_eq!(
        super::retryable_prompt_reason(&apply_result(TerminalProxyApplyOutcome::Unknown {
            reason: ShellCommandUnknownReason::MissingCompletion,
        })),
        None
    );
}

#[test]
/// 一次性能力只能消费一次，重试后仅新的 typed 瞬态结论可签发下一枚能力。
fn future_retry_capability_is_one_shot_and_renewed_after_safe_result() {
    let store = FutureProxyPolicyStore::new();
    let host = host_id("host-a");
    let key = retryable_attempt(&store, &host, "session-a");
    let first_capability = store
        .finish_attempt(
            &host,
            &key,
            Some(TerminalProxyUnavailableReason::PromptNotReady),
            false,
        )
        .expect("可信提示符瞬态状态签发首次能力");

    let retry_cancellation = store
        .begin_retry(&host, &key, first_capability)
        .expect("首次显式重试领取成功");
    assert!(!retry_cancellation.is_cancelled());
    assert!(store.policy_is_current(&host, 1, &key));
    assert!(matches!(
        store.begin_retry(&host, &key, first_capability),
        Err(FutureAttemptRetryClaim::InFlight)
    ));

    let second_capability = store
        .finish_attempt(
            &host,
            &key,
            Some(TerminalProxyUnavailableReason::AlternateScreen),
            false,
        )
        .expect("新的提示符瞬态状态签发下一枚能力");
    assert_ne!(first_capability, second_capability);
    assert!(matches!(
        store.begin_retry(&host, &key, first_capability),
        Err(FutureAttemptRetryClaim::CapabilityChanged)
    ));
    store
        .begin_retry(&host, &key, second_capability)
        .expect("新能力领取成功");
}

#[test]
/// 策略 revision 改变后，旧 pane 的 intent 必须失效且不会重绑定到新策略。
fn future_retry_capability_is_rejected_after_policy_change() {
    let store = FutureProxyPolicyStore::new();
    let host = host_id("host-a");
    let key = retryable_attempt(&store, &host, "session-a");
    let capability = store
        .finish_attempt(
            &host,
            &key,
            Some(TerminalProxyUnavailableReason::PromptNotEmpty),
            false,
        )
        .expect("可信提示符瞬态状态签发能力");
    store
        .set(FutureProxyApplyRequest {
            host_id: host.clone(),
            expected_revision: 1,
            forward_id: None,
        })
        .expect("关闭策略并提高 revision");

    assert!(matches!(
        store.begin_retry(&host, &key, capability),
        Err(FutureAttemptRetryClaim::PolicyChanged)
    ));
}

#[test]
/// 非允许的终态或调用方取消不能签发可重试能力。
fn future_retry_capability_requires_uncancelled_safe_not_applied_result() {
    let store = FutureProxyPolicyStore::new();
    let host = host_id("host-a");
    let key = retryable_attempt(&store, &host, "session-a");
    assert_eq!(
        store.finish_attempt(
            &host,
            &key,
            Some(TerminalProxyUnavailableReason::InitialCommandPresent),
            false,
        ),
        None
    );

    let second_key = FutureAttemptKey {
        session_id: session_id("session-b"),
        ..key
    };
    store
        .claim_attempt(&host, "forward-a", second_key.clone())
        .expect("第二个测试尝试可被领取");
    assert_eq!(
        store.finish_attempt(
            &host,
            &second_key,
            Some(TerminalProxyUnavailableReason::PromptNotReady),
            true,
        ),
        None
    );
}

#[test]
/// 只允许已观测、空行且不在备用屏幕中的可信 shell 提示符接收脚本。
fn proxy_injection_requires_an_available_empty_prompt() {
    let mut context = clean_context();
    assert_eq!(super::clean_prompt_reason(&context), None);

    context.prompt_line_empty = false;
    assert_eq!(
        super::clean_prompt_reason(&context),
        Some(TerminalProxyUnavailableReason::PromptNotEmpty)
    );

    context.prompt_line_empty = true;
    context.alternate_screen = true;
    assert_eq!(
        super::clean_prompt_reason(&context),
        Some(TerminalProxyUnavailableReason::AlternateScreen)
    );

    context.alternate_screen = false;
    context.availability = TerminalCommandAvailability::Unavailable(
        crate::sessions::TerminalCommandUnavailableReason::ShellIntegrationNotObserved,
    );
    assert_eq!(
        super::clean_prompt_reason(&context),
        Some(TerminalProxyUnavailableReason::ShellIntegrationUnavailable)
    );
}

#[test]
/// 所有受支持 shell 都保留原环境到 shell 私有变量，并用值比较保护后续用户修改。
fn scripts_are_reversible_without_embedding_previous_environment_values() {
    let proxy_url = "socks5h://127.0.0.1:1080";
    let sentinel = "DO_NOT_EMBED_OLD_ENV_VALUE";
    let cases = [
        (RemoteShellDialect::Posix, "ALL_PROXY", "if [ "),
        (
            RemoteShellDialect::Fish,
            "set -gx ALL_PROXY",
            "if test \"$ALL_PROXY\"",
        ),
        (RemoteShellDialect::PowerShell, "$env:ALL_PROXY", "-ceq"),
    ];

    for (dialect, set_marker, restore_guard) in cases {
        let scripts =
            scripts::build_scripts(proxy_url, dialect, "token123").expect("受支持 shell 生成脚本");
        assert!(scripts.apply_script.contains(set_marker));
        assert!(scripts.apply_script.contains(proxy_url));
        assert!(scripts.restore_script.contains(restore_guard));
        assert!(scripts.restore_script.contains(proxy_url));
        assert!(!scripts.apply_script.contains(sentinel));
        assert!(!scripts.restore_script.contains(sentinel));
        assert!(scripts.restore_script.contains("NO_PROXY"));
        assert!(scripts.apply_script.contains("ALL_PROXY_UPPER"));
        assert!(scripts.apply_script.contains("ALL_PROXY_LOWER"));
        assert!(scripts.apply_script.contains("NO_PROXY_UPPER"));
        assert!(scripts.apply_script.contains("NO_PROXY_LOWER"));
        assert!(scripts.apply_script.contains("_APPLIED"));
        assert!(scripts.restore_script.contains("_RESTORE_RESULT"));
    }
}

/// 回读脚本只比较私有 token 和布尔状态，不把保存的旧环境值写入探针命令。
#[test]
fn reconciliation_probes_hide_environment_values_and_clear_all_private_markers() {
    let sentinel = "DO_NOT_EMBED_OLD_ENV_VALUE";
    for dialect in [
        RemoteShellDialect::Posix,
        RemoteShellDialect::Fish,
        RemoteShellDialect::PowerShell,
    ] {
        let probes = super::reconciliation::build_probe_scripts(
            "socks5h://127.0.0.1:1080",
            dialect,
            "token123",
        )
        .expect("受支持方言生成只读探针");
        for script in [
            &probes.restore_full,
            &probes.restore_partial,
            &probes.active,
            &probes.applied,
            &probes.backups_complete,
            &probes.values_match,
            &probes.clear_restore_marker,
        ] {
            assert!(!script.contains(sentinel));
        }
        for marker in [
            "_RESTORE_RESULT",
            "_ACTIVE",
            "_APPLIED",
            "_ALL_PROXY_UPPER_VALUE",
            "_ALL_PROXY_LOWER_VALUE",
            "_NO_PROXY_UPPER_VALUE",
            "_NO_PROXY_LOWER_VALUE",
        ] {
            assert!(probes.clear_restore_marker.contains(marker));
        }
    }
}

/// Dynamic 的回环端点只供本机脚本审阅，远端 SSH 自动注入要求远端监听。
#[test]
fn proxy_script_context_distinguishes_local_and_remote_dynamic_forwards() {
    assert_eq!(
        super::proxy_script_execution_context(&ForwardKind::Dynamic),
        Ok(ProxyScriptExecutionContext::LocalShell)
    );
    assert_eq!(
        super::proxy_script_execution_context(&ForwardKind::RemoteDynamic),
        Ok(ProxyScriptExecutionContext::RemoteShell)
    );
    assert_eq!(
        super::proxy_script_execution_context(&ForwardKind::Local),
        Err(TerminalProxyUnavailableReason::WrongForwardKind)
    );
}

#[test]
/// 方言未确认时不生成脚本，长度超限时也不截断或部分构造。
fn unsupported_or_oversized_script_input_is_rejected() {
    assert!(matches!(
        scripts::build_scripts(
            "socks5h://127.0.0.1:1080",
            RemoteShellDialect::Unknown,
            "token123"
        ),
        Err(crate::AppError::CapabilityNotSupported { .. })
    ));

    let oversized_proxy = format!("socks5h://{}", "a".repeat(2_048));
    assert!(matches!(
        scripts::build_scripts(&oversized_proxy, RemoteShellDialect::Posix, "token123"),
        Err(crate::AppError::Invalid { .. })
    ));
}
