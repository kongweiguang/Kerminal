//! 脱离布局会话清理的 owner、deadline 与重试回执回归测试。
//!
//! @author kongweiguang

use crate::root::{
    workspace_orphan_cleanup::{
        OrphanCleanupCompletion, OrphanCleanupTicket, apply_orphan_cleanup_result,
        close_orphan_cleanup_batch, orphan_cleanup_items,
    },
    workspace_runtime::{OrphanCleanupInput, WorkspaceRuntime, WorkspaceRuntimeError},
};
use gpui_kit::SharedString;
use kerminal_application::{
    error::AppResult,
    platform::external_launch::{ExternalTargetLease, ExternalTargetLeaseOwner},
    sessions::SessionDeadline,
};
use kerminal_domain::{
    files::ContainerRuntime,
    identity::{HostId, PaneId, SessionId, TabId},
    sessions::{ContainerSessionTarget, LocalSessionTarget, SessionTarget, TerminalSize},
};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context as TaskContext, Poll, Waker},
    time::Duration,
};

/// 使用真实 RAII lease 观察资源退役次数，不以列表行消失替代 owner 释放证据。
#[derive(Default)]
struct LeaseReleaseProbe {
    releases: AtomicUsize,
}

impl ExternalTargetLeaseOwner for LeaseReleaseProbe {
    /// 测试只注册独立初始 scope；保留端口完整契约，避免引入连接仓储或配置根。
    ///
    /// @author kongweiguang
    fn retain_scope(&self, _host_id: &HostId) -> AppResult<()> {
        Ok(())
    }

    /// 最后一个真实 lease clone 释放才递增，重复 ACK 不应再次触发本端口。
    ///
    /// @author kongweiguang
    fn release_scope(&self, _host_id: &HostId) {
        self.releases.fetch_add(1, Ordering::SeqCst);
    }
}

/// 固定原始容器 pane 和目标，确保成功 ACK 返回足够资料供统一入口刷新原目标。
///
/// @author kongweiguang
fn retain_container_orphan(
    runtime: &mut WorkspaceRuntime,
    tab_id: TabId,
    pane_id: PaneId,
    session_id: SessionId,
    lease_owner: &Arc<LeaseReleaseProbe>,
) -> SessionTarget {
    let host_id = HostId::parse("external:orphan-cleanup-test")
        .expect("test uses a valid opaque external target identity");
    let target = SessionTarget::Container(ContainerSessionTarget {
        host_id: Some(host_id.clone()),
        runtime: ContainerRuntime::Docker,
        container_id: "orphan-container".to_owned(),
        command: vec![OsString::from("sh")],
    });
    let lease = ExternalTargetLease::new_registered(host_id, lease_owner.clone())
        .expect("test registers an isolated in-memory lease");
    runtime
        .retain_orphan_cleanup(OrphanCleanupInput {
            tab_id,
            pane_id,
            session_id,
            target: target.clone(),
            size: TerminalSize::default(),
            snapshot: None,
            status: SharedString::from("close acknowledgement is pending"),
            external_lease: Some(lease),
            transient: true,
        })
        .expect("container owner remains uniquely held by Runtime");
    target
}

/// 用稳定 session ID 建立最小 Local owner，不把界面展示字符串当作资源身份。
fn retain_orphan(runtime: &mut WorkspaceRuntime, session_id: SessionId) {
    runtime
        .retain_orphan_cleanup(OrphanCleanupInput {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
            session_id,
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("configured-shell"),
                args: Vec::new(),
                working_directory: None,
                environment: BTreeMap::new(),
            }),
            size: TerminalSize::default(),
            snapshot: None,
            status: SharedString::from("close acknowledgement is pending"),
            external_lease: None,
            transient: false,
        })
        .expect("orphan resource owner is unique");
}

/// 推进立即完成的生产 batch helper，不另建异步 close 编排用于测试。
fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::noop();
    let mut context = TaskContext::from_waker(waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// RetryAll 使用同一绝对 deadline 串行发出每个 session close，单项失败不跳过后续 owner。
#[test]
fn retry_all_keeps_shared_deadline_and_retains_only_unconfirmed_session() {
    let mut runtime = WorkspaceRuntime::default();
    let session_ids = (0..3).map(|_| SessionId::new()).collect::<Vec<_>>();
    for session_id in &session_ids {
        retain_orphan(&mut runtime, session_id.clone());
    }
    let tickets = session_ids
        .iter()
        .map(|session_id| OrphanCleanupTicket {
            session_id: session_id.clone(),
            generation: runtime
                .begin_orphan_cleanup(session_id, SharedString::from("closing"))
                .expect("owner can be claimed once"),
        })
        .collect::<Vec<_>>();
    let failed_session = session_ids[1].clone();
    let failed_session_for_close = failed_session.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let first_deadline = SessionDeadline::now() + Duration::from_secs(5);
    let observed_calls = Arc::clone(&calls);
    let results = block_on(close_orphan_cleanup_batch(
        tickets,
        first_deadline,
        move |session_id, deadline| {
            let calls = Arc::clone(&observed_calls);
            let failed_session = failed_session_for_close.clone();
            async move {
                calls
                    .lock()
                    .expect("call log lock")
                    .push((session_id.clone(), deadline));
                if session_id == failed_session {
                    Err("simulated close failure".to_owned())
                } else {
                    Ok(())
                }
            }
        },
    ));

    let observed = calls.lock().expect("call log lock");
    assert_eq!(
        observed
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>(),
        session_ids,
        "a failed session cannot skip later close requests"
    );
    assert!(
        observed
            .iter()
            .all(|(_, deadline)| *deadline == first_deadline)
    );
    drop(observed);

    for (ticket, result) in results {
        let message = result
            .as_ref()
            .err()
            .map(|error| format!("retryable: {error}"))
            .unwrap_or_else(|| "cleanup complete".to_owned());
        let (completion, _) =
            apply_orphan_cleanup_result(&mut runtime, &ticket, result, SharedString::from(message));
        if ticket.session_id == failed_session {
            assert_eq!(completion, OrphanCleanupCompletion::Retryable);
        } else {
            assert_eq!(completion, OrphanCleanupCompletion::Removed);
        }
    }

    let rows = orphan_cleanup_items(&runtime);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_id, failed_session);
    assert!(!rows[0].cleanup_in_flight);
}

/// 取消和迟到 ACK 不释放 owner；只有当前代次的成功回执才能清除同一 SessionId。
#[test]
fn cancelled_or_stale_receipt_keeps_exact_owner_until_retry_ack() {
    let mut runtime = WorkspaceRuntime::default();
    let session_id = SessionId::new();
    retain_orphan(&mut runtime, session_id.clone());
    let first = OrphanCleanupTicket {
        session_id: session_id.clone(),
        generation: runtime
            .begin_orphan_cleanup(&session_id, SharedString::from("closing"))
            .expect("first retry starts"),
    };
    assert!(matches!(
        runtime.begin_orphan_cleanup(&session_id, SharedString::from("duplicate")),
        Err(WorkspaceRuntimeError::OrphanCleanupInProgress)
    ));

    let (cancelled, _) = apply_orphan_cleanup_result(
        &mut runtime,
        &first,
        Err("executor cancelled before acknowledgement".to_owned()),
        SharedString::from("close not confirmed; owner retained"),
    );
    assert_eq!(cancelled, OrphanCleanupCompletion::Retryable);
    assert_eq!(
        runtime.orphan_cleanup(&session_id).unwrap().session_id,
        session_id
    );

    let second = OrphanCleanupTicket {
        session_id: session_id.clone(),
        generation: runtime
            .begin_orphan_cleanup(&session_id, SharedString::from("retrying"))
            .expect("retained owner can retry"),
    };
    let (stale, _) =
        apply_orphan_cleanup_result(&mut runtime, &first, Ok(()), SharedString::from("unused"));
    assert_eq!(stale, OrphanCleanupCompletion::Stale);
    assert!(runtime.orphan_cleanup(&session_id).is_some());

    let (closed, removed) =
        apply_orphan_cleanup_result(&mut runtime, &second, Ok(()), SharedString::from("unused"));
    assert_eq!(closed, OrphanCleanupCompletion::Removed);
    assert_eq!(removed.unwrap().session_id, session_id);
    assert!(runtime.orphan_cleanup(&session_id).is_none());
}

/// 失败和旧代次均不给收尾入口释放凭据；成功返回原容器 owner，重复 ACK 不会二次释放 lease。
///
/// @author kongweiguang
#[test]
fn container_ack_preserves_original_owner_until_current_success_and_retires_once() {
    let mut runtime = WorkspaceRuntime::default();
    let lease_owner = Arc::new(LeaseReleaseProbe::default());
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let session_id = SessionId::new();
    let target = retain_container_orphan(
        &mut runtime,
        tab_id.clone(),
        pane_id.clone(),
        session_id.clone(),
        &lease_owner,
    );
    let first = OrphanCleanupTicket {
        session_id: session_id.clone(),
        generation: runtime
            .begin_orphan_cleanup(&session_id, SharedString::from("closing"))
            .expect("first close owns its generation"),
    };
    let (failed, removed) = apply_orphan_cleanup_result(
        &mut runtime,
        &first,
        Err("close not confirmed".to_owned()),
        SharedString::from("retry required"),
    );
    assert_eq!(failed, OrphanCleanupCompletion::Retryable);
    assert!(removed.is_none(), "failure cannot release UI resources");
    let retained = runtime
        .orphan_cleanup(&session_id)
        .expect("failure keeps the original container owner");
    assert_eq!(retained.tab_id, tab_id);
    assert_eq!(retained.pane_id, pane_id);
    assert_eq!(retained.target, target);
    assert!(retained.external_lease.is_some());
    assert!(!retained.cleanup_in_flight);
    assert_eq!(lease_owner.releases.load(Ordering::SeqCst), 0);

    let retry = OrphanCleanupTicket {
        session_id: session_id.clone(),
        generation: runtime
            .begin_orphan_cleanup(&session_id, SharedString::from("retrying"))
            .expect("retry acquires a new generation of the same owner"),
    };
    for result in [Ok(()), Err("late failure".to_owned())] {
        let (stale, removed) = apply_orphan_cleanup_result(
            &mut runtime,
            &first,
            result,
            SharedString::from("old result must not replace retry status"),
        );
        assert_eq!(stale, OrphanCleanupCompletion::Stale);
        assert!(
            removed.is_none(),
            "old ACK cannot clear tools or dirty a pane"
        );
        let retained = runtime.orphan_cleanup(&session_id).unwrap();
        assert_eq!(retained.cleanup_generation, retry.generation);
        assert!(retained.cleanup_in_flight);
        assert_eq!(retained.status.as_ref(), "retrying");
        assert_eq!(lease_owner.releases.load(Ordering::SeqCst), 0);
    }

    let (closed, removed) =
        apply_orphan_cleanup_result(&mut runtime, &retry, Ok(()), SharedString::from("unused"));
    assert_eq!(closed, OrphanCleanupCompletion::Removed);
    let owner = removed.expect("successful ACK returns the exact owner to the shared finalizer");
    assert_eq!(owner.tab_id, tab_id);
    assert_eq!(
        owner.pane_id, pane_id,
        "dirty targets the original container pane"
    );
    assert_eq!(owner.session_id, session_id);
    assert_eq!(owner.target, target);
    assert!(owner.transient);
    assert!(owner.external_lease.is_some());
    assert!(runtime.orphan_cleanup(&session_id).is_none());
    assert_eq!(lease_owner.releases.load(Ordering::SeqCst), 0);
    drop(owner);
    assert_eq!(lease_owner.releases.load(Ordering::SeqCst), 1);

    let (duplicate, removed) =
        apply_orphan_cleanup_result(&mut runtime, &retry, Ok(()), SharedString::from("unused"));
    assert_eq!(duplicate, OrphanCleanupCompletion::Stale);
    assert!(
        removed.is_none(),
        "duplicate ACK has no second cleanup owner"
    );
    assert_eq!(lease_owner.releases.load(Ordering::SeqCst), 1);
}

/// 同一 pane 已有新会话时，旧 SessionId 的迟到成功或失败不得释放新 owner 及 lease。
///
/// @author kongweiguang
#[test]
fn retired_session_ack_cannot_release_new_container_owner_on_same_pane() {
    let mut runtime = WorkspaceRuntime::default();
    let lease_owner = Arc::new(LeaseReleaseProbe::default());
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let old_session_id = SessionId::new();
    retain_container_orphan(
        &mut runtime,
        tab_id.clone(),
        pane_id.clone(),
        old_session_id.clone(),
        &lease_owner,
    );
    let old_ticket = OrphanCleanupTicket {
        session_id: old_session_id.clone(),
        generation: runtime
            .begin_orphan_cleanup(&old_session_id, SharedString::from("closing old"))
            .unwrap(),
    };
    let (completion, owner) = apply_orphan_cleanup_result(
        &mut runtime,
        &old_ticket,
        Ok(()),
        SharedString::from("unused"),
    );
    assert_eq!(completion, OrphanCleanupCompletion::Removed);
    drop(owner);
    assert_eq!(lease_owner.releases.load(Ordering::SeqCst), 1);

    let new_session_id = SessionId::new();
    retain_container_orphan(
        &mut runtime,
        tab_id.clone(),
        pane_id.clone(),
        new_session_id.clone(),
        &lease_owner,
    );
    let new_ticket = OrphanCleanupTicket {
        session_id: new_session_id.clone(),
        generation: runtime
            .begin_orphan_cleanup(&new_session_id, SharedString::from("closing new"))
            .unwrap(),
    };
    assert_eq!(old_ticket.generation, new_ticket.generation);
    for result in [Ok(()), Err("old close failed".to_owned())] {
        let (stale, owner) = apply_orphan_cleanup_result(
            &mut runtime,
            &old_ticket,
            result,
            SharedString::from("unused"),
        );
        assert_eq!(stale, OrphanCleanupCompletion::Stale);
        assert!(owner.is_none());
        let retained = runtime.orphan_cleanup(&new_session_id).unwrap();
        assert_eq!(retained.tab_id, tab_id);
        assert_eq!(retained.pane_id, pane_id);
        assert_eq!(retained.session_id, new_session_id);
        assert!(retained.cleanup_in_flight);
        assert!(retained.external_lease.is_some());
        assert_eq!(lease_owner.releases.load(Ordering::SeqCst), 1);
    }
    let (completion, owner) = apply_orphan_cleanup_result(
        &mut runtime,
        &new_ticket,
        Ok(()),
        SharedString::from("unused"),
    );
    assert_eq!(completion, OrphanCleanupCompletion::Removed);
    drop(owner);
    assert_eq!(lease_owner.releases.load(Ordering::SeqCst), 2);
}
