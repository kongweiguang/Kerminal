//! Agent 连续性回收结果判定回归。
//!
//! 测试只构造结果值，不触碰会话服务：证明 close 失败不会被后续生命周期读回覆盖，
//! 且只有 close 成功后才依据注册表判断是否完成回收。
//!
//! @author kongweiguang

use std::{collections::BTreeSet, error::Error, io};

use kerminal_application::{AppError, AppResult, PublicErrorCode};
use kerminal_domain::{identity::SessionId, sessions::SessionLifecycle};

use super::{
    ContinuityConvergenceStep, ContinuityResources, ContinuitySessionIdentity, b_reuses_closed_a2,
    combine_continuity_failure, continuity_agent_not_ready, continuity_convergence_step,
    continuity_reclaim_result, continuity_running_identity_continues,
    continuity_session_observation, merge_continuity_outcomes,
};

/// 注册表条目缺失才代表关闭后已收敛：此时没有元数据结果，身份按 Closed 记录且 PID 为空。
///
/// @author kongweiguang
#[test]
fn missing_registry_entry_is_closed_without_process_identity() {
    let session_id = SessionId::new();
    let identity =
        continuity_session_observation(session_id.clone(), None, None).expect("converged entry");

    assert_eq!(identity.session_id, session_id);
    assert_eq!(identity.lifecycle, SessionLifecycle::Closed);
    assert_eq!(identity.process_id, None);
}

/// 条目仍存在时必须使用真实的元数据结果；读取失败原样上抛，不能退化成 `process_id=None`
/// 让后续 Running/PID 断言以含混差异失败。元数据本身没有进程号（`Ok(None)`）与读取失败
/// 是两种形态：前者是合法身份（PID 缺失），后者必须失败。
///
/// @author kongweiguang
#[test]
fn present_registry_entry_requires_metadata_and_preserves_its_error() {
    let session_id = SessionId::new();
    let running = continuity_session_observation(
        session_id.clone(),
        Some(SessionLifecycle::Running),
        Some(Ok(Some(4100))),
    )
    .expect("running identity");
    assert_eq!(running.lifecycle, SessionLifecycle::Running);
    assert_eq!(running.process_id, Some(4100));

    let without_pid = continuity_session_observation(
        session_id.clone(),
        Some(SessionLifecycle::Running),
        Some(Ok(None)),
    )
    .expect("metadata without a process id is a legal identity shape");
    assert_eq!(without_pid.process_id, None);
    assert_eq!(without_pid.lifecycle, SessionLifecycle::Running);

    let failure = continuity_session_observation(
        session_id,
        Some(SessionLifecycle::Running),
        Some(Err(AppError::infrastructure("metadata-unavailable"))),
    )
    .expect_err("metadata failure must not become a missing process id");
    assert_eq!(failure.to_string(), "metadata-unavailable");
}

/// 生命周期与元数据结果必须成对出现；只有一侧存在说明读取流程不一致，直接失败关闭。
///
/// @author kongweiguang
#[test]
fn lifecycle_and_metadata_presence_must_agree() {
    let session_id = SessionId::new();
    // 显式标注元素类型，避免依赖整函数体推断在数组字面量上传播 Result 的错误类型；
    // 内层 Option 对应 SessionRuntimeMetadata.process_id 本身可缺失。
    let cases: [(Option<SessionLifecycle>, Option<AppResult<Option<u32>>>); 2] = [
        (Some(SessionLifecycle::Running), None),
        (None, Some(Ok(Some(4100)))),
    ];
    for (lifecycle, metadata) in cases {
        let error = continuity_session_observation(session_id.clone(), lifecycle, metadata)
            .expect_err("inconsistent observation must fail");
        assert_eq!(
            error.public_code(),
            kerminal_application::PublicErrorCode::Conflict
        );
    }
}

/// close 已报超时但注册表随后显示 Exited/Closed 或已经没有该会话时，仍必须返回原始 close
/// 错误及其来源链；这样脚本自行退出或注册表移除都不会被当成显式回收完成。
///
/// @author kongweiguang
#[test]
fn close_failure_wins_over_later_terminal_registry_state() {
    let terminal = SessionId::new();
    for remaining in [
        Some(SessionLifecycle::Exited),
        Some(SessionLifecycle::Closed),
        None,
    ] {
        let close = Err(AppError::infrastructure_with_source(
            "close-proof",
            io::Error::other("close-source"),
        ));

        let error = continuity_reclaim_result(&terminal, close, Ok(remaining))
            .expect_err("close failure must remain a failure");

        assert_eq!(error.to_string(), "close-proof");
        assert_eq!(
            error.source().map(|source| source.to_string()).as_deref(),
            Some("close-source")
        );
    }
}

/// 单独覆盖注册表缺失分支，避免只有 Exited/Closed 的断言掩盖 `None` 被误判为成功。
///
/// @author kongweiguang
#[test]
fn close_failure_wins_over_missing_registry_entry() {
    let terminal = SessionId::new();
    let close = Err(AppError::infrastructure("close-timeout"));

    let error = continuity_reclaim_result(&terminal, close, Ok(None))
        .expect_err("close failure must remain a failure");

    assert_eq!(error.to_string(), "close-timeout");
}

/// close 成功且注册表条目已移除或进入 Closed 时，才报告本轮 PTY 已回收。`Exited` 只说明子进程
/// 结束，ManagedSession 仍持有 writer、引擎与待清理所有权，不能据此丢弃回收 ID。
///
/// @author kongweiguang
#[test]
fn close_success_requires_registry_convergence() {
    let terminal = SessionId::new();
    for remaining in [None, Some(SessionLifecycle::Closed)] {
        let detail = continuity_reclaim_result(&terminal, Ok(()), Ok(remaining))
            .expect("converged lifecycle after successful close");
        assert!(detail.contains(&terminal.to_string()));
    }

    for remaining in [
        Some(SessionLifecycle::Exited),
        Some(SessionLifecycle::Closing),
        Some(SessionLifecycle::Starting),
    ] {
        let error = continuity_reclaim_result(&terminal, Ok(()), Ok(remaining))
            .expect_err("unremoved registry entry must remain a failure");
        assert_eq!(error.to_string(), "Agent PTY 在显式关闭期限后仍未回收");
    }
}

/// close 成功但注册表仍处于 Running 时仍算未回收，不能因脚本稍后会退出而提前报成功。
///
/// @author kongweiguang
#[test]
fn close_success_with_running_lifecycle_is_still_a_failure() {
    let terminal = SessionId::new();
    let error = continuity_reclaim_result(&terminal, Ok(()), Ok(Some(SessionLifecycle::Running)))
        .expect_err("running lifecycle must remain a failure");

    assert_eq!(error.to_string(), "Agent PTY 在显式关闭期限后仍未回收");
}

/// `list` 找到条目后 `runtime_metadata` 恰逢关闭移除会返回 NotFound：回收轮询必须把它当作
/// 可重试竞态而不是收敛证据，且在期限到达后失败；其它错误必须保留类型和 source 立即返回，
/// 不能被窄化成重试或被推断成已关闭。
///
/// @author kongweiguang
#[test]
fn metadata_not_found_race_retries_only_before_the_deadline() {
    let session_id = SessionId::new();
    let identity = ContinuitySessionIdentity {
        session_id: session_id.clone(),
        process_id: Some(4100),
        lifecycle: SessionLifecycle::Closed,
    };

    match continuity_convergence_step(Ok(identity.clone()), false)
        .expect("observed identity is not an error")
    {
        ContinuityConvergenceStep::Observed(observed) => {
            assert_eq!(observed.lifecycle, identity.lifecycle)
        }
        ContinuityConvergenceStep::Retry => panic!("a real observation must not be retried"),
    }

    match continuity_convergence_step(Err(AppError::not_found("终端会话不存在")), false)
        .expect("a pre-deadline NotFound is retryable")
    {
        ContinuityConvergenceStep::Retry => {}
        ContinuityConvergenceStep::Observed(_) => panic!("NotFound must not become an observation"),
    }

    let expired = continuity_convergence_step(Err(AppError::not_found("终端会话不存在")), true)
        .expect_err("NotFound after the absolute deadline must fail");
    assert_eq!(expired.public_code(), PublicErrorCode::NotFound);

    for error in [
        AppError::infrastructure("metadata-unavailable"),
        AppError::cancelled("回收轮询已取消"),
        AppError::timeout("终端会话查找"),
        AppError::conflict("回收状态不一致"),
    ] {
        let expected = error.to_string();
        let expected_code = error.public_code();
        let returned = continuity_convergence_step(Err(error), false)
            .expect_err("non-NotFound errors must not be retried");
        assert_eq!(returned.to_string(), expected);
        assert_eq!(returned.public_code(), expected_code);
    }

    let with_source = continuity_convergence_step(
        Err(AppError::infrastructure_with_source(
            "metadata-unavailable",
            io::Error::other("metadata-source"),
        )),
        false,
    )
    .expect_err("infrastructure error must fail immediately");
    assert_eq!(
        with_source
            .source()
            .map(|source| source.to_string())
            .as_deref(),
        Some("metadata-source")
    );
}

/// 收尾只遍历本轮记录的 A1/A2/B 与 Agent PTY，不按 Tab owner 扫描；Agent PTY 只进入
/// 成功路径全集，普通本地清理不重复关闭它。缺失 Agent ID 时本地 ID 仍保留，保证后续目录清理。
///
/// @author kongweiguang
#[test]
fn cleanup_scope_contains_only_flow_owned_session_identities() {
    let agent = SessionId::new();
    let a1 = SessionId::new();
    let a2 = SessionId::new();
    let b = SessionId::new();
    let resources = ContinuityResources {
        agent_session_id: None,
        agent_terminal_session_id: Some(agent.clone()),
        a1_session_id: Some(a1.clone()),
        a2_session_id: Some(a2.clone()),
        b_session_id: Some(b.clone()),
        ..ContinuityResources::default()
    };

    assert_eq!(
        resources
            .local_terminal_session_ids()
            .into_iter()
            .collect::<BTreeSet<_>>(),
        [a1.clone(), a2.clone(), b.clone()].into_iter().collect()
    );
    assert_eq!(
        resources
            .all_terminal_session_ids()
            .into_iter()
            .collect::<BTreeSet<_>>(),
        [agent, a1.clone(), a2, b].into_iter().collect()
    );

    let deduplicated = ContinuityResources {
        a1_session_id: Some(a1.clone()),
        b_session_id: Some(a1.clone()),
        ..ContinuityResources::default()
    };
    assert_eq!(deduplicated.local_terminal_session_ids(), vec![a1]);
}

/// 只对 UI 明确报告的“尚未建立 pane/candidate/panel”重试；真实目标不匹配、工具错误和
/// renderer 身份错误必须立即返回，不能被有界重试掩盖。
///
/// @author kongweiguang
#[test]
fn only_explicit_not_ready_states_are_retried() {
    for message in [
        "Agent 验收没有活动的 fixture SSH pane",
        "Agent 验收没有活动的本地终端 pane",
        "Agent 验收没有完整的活动终端候选",
        "Agent panel 不可用",
    ] {
        assert!(continuity_agent_not_ready(message), "{message}");
    }
    for message in [
        "Agent 工具不是当前验收面板",
        "Agent 候选与活动 fixture SSH pane 不匹配",
        "Agent 候选与活动本地终端 pane 不匹配",
        "Agent renderer snapshot belongs to a different terminal",
    ] {
        assert!(!continuity_agent_not_ready(message), "{message}");
    }
}

/// 归档/目录清理单边失败时保留原错误及其 source；双边失败时主错误仍是 source，
/// 清理原因只作为附加信息，不能被后来的清理结果覆盖。
///
/// @author kongweiguang
#[test]
fn cleanup_merge_preserves_primary_failure_and_reports_both_sides() {
    let primary: Box<dyn Error> = Box::new(AppError::infrastructure_with_source(
        "primary-failure",
        io::Error::other("primary-source"),
    ));
    let single = merge_continuity_outcomes(Err(primary), Ok(()), "会话回收")
        .expect_err("primary failure must survive");
    assert_eq!(single.to_string(), "primary-failure");
    assert_eq!(
        single.source().map(|source| source.to_string()).as_deref(),
        Some("primary-source")
    );

    let single_cleanup = merge_continuity_outcomes(
        Ok(()),
        Err(Box::new(AppError::infrastructure("cleanup-only"))),
        "会话回收",
    )
    .expect_err("cleanup failure must survive");
    assert_eq!(single_cleanup.to_string(), "cleanup-only");

    let both = merge_continuity_outcomes(
        Err(Box::new(AppError::infrastructure_with_source(
            "archive-failed",
            io::Error::other("archive-source"),
        ))),
        Err(Box::new(AppError::infrastructure(
            "directory-cleanup-failed",
        ))),
        "运行目录清理",
    )
    .expect_err("both failures must be reported");
    let text = both.to_string();
    assert!(text.contains("archive-failed"), "{text}");
    assert!(text.contains("directory-cleanup-failed"), "{text}");
    assert_eq!(
        both.source().map(|source| source.to_string()).as_deref(),
        Some("archive-failed")
    );
    assert_eq!(
        both.source()
            .and_then(|source| source.source())
            .map(|source| source.to_string())
            .as_deref(),
        Some("archive-source")
    );
}

/// 失败路径直接复用同一合并器；清理非空时保留主错误链，空清理不额外包装。
///
/// @author kongweiguang
#[test]
fn failure_cleanup_details_do_not_replace_original_error() {
    let primary: Box<dyn Error> = Box::new(AppError::infrastructure_with_source(
        "flow-timeout",
        io::Error::other("deadline"),
    ));
    let combined =
        combine_continuity_failure(primary, vec!["Agent 回收失败：close timeout".to_owned()]);
    let text = combined.to_string();
    assert!(text.contains("flow-timeout"), "{text}");
    assert!(text.contains("close timeout"), "{text}");
    assert_eq!(
        combined
            .source()
            .and_then(|source| source.source())
            .map(|source| source.to_string())
            .as_deref(),
        Some("deadline")
    );
}

/// B 与已关闭 A2 的独立性必须同时比较 SessionId 与 PID；只比较其中一个会把 PID 复用或
/// 残留会话当成新 Tab 的独立资源。A2 证据缺失时不能伪造“不同”结论。
///
/// @author kongweiguang
#[test]
fn b_identity_must_exclude_both_closed_a2_session_and_process_id() {
    let a2 = ContinuitySessionIdentity {
        session_id: SessionId::new(),
        process_id: Some(4100),
        lifecycle: SessionLifecycle::Closed,
    };
    let same_session = ContinuitySessionIdentity {
        session_id: a2.session_id.clone(),
        process_id: Some(4200),
        lifecycle: SessionLifecycle::Running,
    };
    let same_process = ContinuitySessionIdentity {
        session_id: SessionId::new(),
        process_id: a2.process_id,
        lifecycle: SessionLifecycle::Running,
    };
    let independent = ContinuitySessionIdentity {
        session_id: SessionId::new(),
        process_id: Some(4300),
        lifecycle: SessionLifecycle::Running,
    };

    assert!(b_reuses_closed_a2(&same_session, Some(&a2)));
    assert!(b_reuses_closed_a2(&same_process, Some(&a2)));
    assert!(!b_reuses_closed_a2(&independent, Some(&a2)));
    assert!(!b_reuses_closed_a2(&same_session, None));
}

/// A1/B 跨 Tab 复核要求仍是同一 SessionId、同一非空 PID 且 Running；任一缺失或改变都要失败，
/// 防止用“仍看到旧画面”或同 ID 重建冒充原进程。
///
/// @author kongweiguang
#[test]
fn running_identity_continuity_requires_same_nonzero_process_and_running_lifecycle() {
    let before = ContinuitySessionIdentity {
        session_id: SessionId::new(),
        process_id: Some(4400),
        lifecycle: SessionLifecycle::Running,
    };
    let same = ContinuitySessionIdentity {
        session_id: before.session_id.clone(),
        process_id: before.process_id,
        lifecycle: SessionLifecycle::Running,
    };
    assert!(continuity_running_identity_continues(&before, &same));

    for current in [
        ContinuitySessionIdentity {
            lifecycle: SessionLifecycle::Exited,
            ..same.clone()
        },
        ContinuitySessionIdentity {
            process_id: None,
            ..same.clone()
        },
        ContinuitySessionIdentity {
            process_id: Some(4401),
            ..same.clone()
        },
        ContinuitySessionIdentity {
            session_id: SessionId::new(),
            ..same.clone()
        },
    ] {
        assert!(
            !continuity_running_identity_continues(&before, &current),
            "identity change must fail: {current:?}"
        );
    }
}

/// A2 经生产 UI 关闭路径回收后，待回收集合必须立即排除 A2，而 A1/B 仍保留到收尾；
/// A2 的关闭前身份证据不能随待回收 ID 一起清空，后续 B 复核还要用它排除复用。
/// 这固定“pane 已移除 ≠ 资源登记已清空”两侧的一致性，防止只改一处造成悬空回收。
///
/// @author kongweiguang
#[test]
fn closed_a2_leaves_reclaim_scope_but_keeps_closed_identity_evidence() {
    let a1 = SessionId::new();
    let a2 = SessionId::new();
    let b = SessionId::new();
    let mut resources = ContinuityResources {
        a1_session_id: Some(a1.clone()),
        a2_session_id: Some(a2.clone()),
        b_session_id: Some(b.clone()),
        a2_closed_identity: Some(ContinuitySessionIdentity {
            session_id: a2.clone(),
            process_id: Some(4500),
            lifecycle: SessionLifecycle::Closed,
        }),
        ..ContinuityResources::default()
    };

    // 关闭前 A2 在本地待回收集合中，异常收尾仍能按身份关闭它。
    let before_close: BTreeSet<_> = resources.local_terminal_session_ids().into_iter().collect();
    assert_eq!(
        before_close,
        [a1.clone(), a2.clone(), b.clone()].into_iter().collect()
    );

    // 生产关闭路径收敛后：A2 退出待回收集合，A1/B 保留；关闭前身份证据继续存在。
    resources.a2_session_id = None;
    let after_close: BTreeSet<_> = resources.local_terminal_session_ids().into_iter().collect();
    assert_eq!(after_close, [a1, b].into_iter().collect());
    let closed = resources
        .a2_closed_identity
        .as_ref()
        .expect("closed identity evidence survives the reclaim");
    assert_eq!(closed.session_id, a2);
    assert_eq!(closed.lifecycle, SessionLifecycle::Closed);
}

/// 回收循环把每个失败会话的原因聚合成动态明细，但主错误仍是首个类型化 `AppError`：
/// `AppError::conflict` 只接受 `&'static str` 公开文案，逐会话动态原因只能走
/// `ContinuityCombinedError` 的 detail 通道，不能反向改写错误构造约定。
///
/// @author kongweiguang
#[test]
fn reclaim_loop_keeps_typed_primary_error_and_dynamic_details() {
    // 该行为由 reclaim_tracked_sessions_if_present 的折叠保证：executor 内聚合
    // (primary, reasons)，外层经 combine_continuity_failure 合并。这里固定 detail
    // 通道的合并语义本身：typed 主错误 + 动态原因必须同时出现且主错误链保留。
    let primary: Box<dyn Error> =
        Box::new(AppError::conflict("Agent PTY 在显式关闭期限后仍未回收"));
    let combined = combine_continuity_failure(
        primary,
        vec!["本轮会话回收失败：session-a：close timeout".to_owned()],
    );
    let text = combined.to_string();
    assert!(
        text.contains("Agent PTY 在显式关闭期限后仍未回收"),
        "{text}"
    );
    assert!(text.contains("session-a：close timeout"), "{text}");
    assert_eq!(
        combined
            .source()
            .map(|source| source.to_string())
            .as_deref(),
        Some("Agent PTY 在显式关闭期限后仍未回收")
    );
}
