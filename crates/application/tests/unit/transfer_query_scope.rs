//! 范围查询与关闭门禁回归；仅注入内存事实，不连接真实主机或访问用户配置。
//!
//! @author kongweiguang

use super::*;
use crate::transfers::{
    SftpTransferPolicyStore, TransferQueryScope, TransferRecoveryPage, TransferRecoveryRecord,
    TransferRecoveryRepository,
};

/// 将规范化与直接构造都纳入契约，避免调用方把空集合或重复别名误解成全局。
///
/// @author kongweiguang
#[test]
fn scope_normalization_preserves_exact_identity_and_empty_semantics() {
    let tab = TabId::new();
    let legacy = TransferOrigin::WorkspaceFile(tab.clone());
    let workspace = TransferOrigin::SftpWorkspace(tab);
    let scope = TransferQueryScope::origins([legacy.clone(), workspace.clone(), legacy.clone()]);
    assert_eq!(
        scope,
        TransferQueryScope::Origins(vec![legacy.clone(), workspace])
    );
    assert!(scope.matches_origin(Some(&legacy)));
    assert!(!scope.matches_origin(None));
    assert!(!TransferQueryScope::origins([]).matches_origin(Some(&legacy)));
    assert!(!TransferQueryScope::origins([]).matches_origin(None));
    assert!(TransferQueryScope::All.matches_origin(None));
}

/// 全局必须保留未绑定历史；集合仍按全局序号排序，且不能因同 TabId 的不同来源种类而串入。
///
/// @author kongweiguang
#[tokio::test]
async fn live_scopes_include_unbound_history_without_reassigning_origins() {
    let (service, _, _, _) = service_for(BackendMode::NoChanges);
    let tab = TabId::new();
    let legacy = TransferOrigin::WorkspaceFile(tab.clone());
    let same_tab_other_kind = TransferOrigin::SftpWorkspace(tab);
    let terminal = TransferOrigin::TerminalPane(PaneId::new());
    let origins = [
        None,
        Some(legacy.clone()),
        Some(same_tab_other_kind),
        Some(terminal.clone()),
    ];
    let mut ids = Vec::new();
    for (sequence, origin) in origins.iter().enumerate() {
        let (id, mut record) = terminal_record(TransferFailureKind::Connection, sequence as u64);
        record.origin = origin.clone();
        service
            .inner
            .records
            .lock()
            .await
            .insert(id.clone(), record);
        ids.push(id);
    }
    assert_eq!(
        service.list_for_scope(&TransferQueryScope::All).await,
        service.list().await
    );
    let scope = TransferQueryScope::Origins(vec![legacy.clone(), terminal, legacy.clone()]);
    let summaries = service.list_for_scope(&scope).await;
    assert_eq!(
        summaries
            .into_iter()
            .map(|item| item.id)
            .collect::<Vec<_>>(),
        vec![ids[3].clone(), ids[1].clone()]
    );
    assert_eq!(
        service
            .list_for_scope(&TransferQueryScope::origins([legacy.clone()]))
            .await,
        service.list_for_origin(&legacy).await
    );
    assert!(
        service
            .list_for_scope(&TransferQueryScope::origins([]))
            .await
            .is_empty()
    );
    let records = service.inner.records.lock().await;
    for (id, origin) in ids.iter().zip(origins) {
        assert_eq!(records.get(id).unwrap().origin, origin);
    }
}

/// 单 ID 查询必须与列表使用相同的来源边界，并且不能把孤儿恢复证据当作活任务授权。
///
/// @author kongweiguang
#[tokio::test]
async fn single_id_query_matches_live_scopes_without_authorizing_recovery() {
    let (service, _, _, _) = service_for(BackendMode::NoChanges);
    let tab = TabId::new();
    let primary = TransferOrigin::WorkspaceFile(TabId::new());
    let legacy_alias = TransferOrigin::WorkspaceFile(tab.clone());
    let other_kind = TransferOrigin::SftpWorkspace(tab);
    let origins = [
        None,
        Some(primary.clone()),
        Some(legacy_alias.clone()),
        Some(other_kind.clone()),
    ];
    let mut ids = Vec::new();
    for (sequence, origin) in origins.iter().enumerate() {
        let (id, mut record) = terminal_record(TransferFailureKind::Connection, sequence as u64);
        record.origin = origin.clone();
        service
            .inner
            .records
            .lock()
            .await
            .insert(id.clone(), record);
        ids.push(id);
    }
    let (orphan_id, mut orphan) = terminal_record(TransferFailureKind::CommitUnknown, 10);
    orphan.origin = Some(legacy_alias.clone());
    service.persist_recovery_record(&orphan).await.unwrap();
    assert!(
        service
            .list_recovery_for_scope(&TransferQueryScope::All, CancellationToken::new())
            .await
            .unwrap()
            .iter()
            .any(|record| record.id == orphan_id)
    );
    ids.extend([orphan_id, TransferId::new()]);

    let scopes = [
        TransferQueryScope::All,
        TransferQueryScope::Origins(vec![primary, legacy_alias.clone(), legacy_alias]),
        TransferQueryScope::origins([other_kind]),
        TransferQueryScope::origins([]),
    ];
    for scope in &scopes {
        let visible = service.list_for_scope(scope).await;
        for id in &ids {
            let expected = visible.iter().find(|summary| &summary.id == id).cloned();
            let result = service.get_for_scope(id, scope).await;
            match expected {
                Some(summary) => assert_eq!(result.unwrap(), summary),
                None => assert_eq!(result.unwrap_err().public_code(), PublicErrorCode::NotFound),
            }
        }
    }
}

/// 全局单项查询沿用原列表清理时机，精确来源查询不能产生额外的全局清理副作用。
///
/// @author kongweiguang
#[tokio::test]
async fn single_id_query_preserves_scope_specific_drag_marker_cleanup() {
    let (service, _, _, _) = service_for(BackendMode::NoChanges);
    let origin = TransferOrigin::WorkspaceFile(TabId::new());
    let (live_id, mut live) = terminal_record(TransferFailureKind::Connection, 1);
    live.origin = Some(origin.clone());
    service
        .inner
        .records
        .lock()
        .await
        .insert(live_id.clone(), live);
    let stale_id = TransferId::new();
    {
        let mut markers = service.inner.drag_staging_ids.lock().unwrap();
        markers.insert(live_id.clone(), true);
        markers.insert(stale_id.clone(), true);
    }

    service
        .get_for_scope(&live_id, &TransferQueryScope::origins([origin]))
        .await
        .unwrap();
    assert!(
        service
            .inner
            .drag_staging_ids
            .lock()
            .unwrap()
            .contains_key(&stale_id)
    );
    assert_eq!(
        service
            .get_for_scope(&TransferId::new(), &TransferQueryScope::All)
            .await
            .unwrap_err()
            .public_code(),
        PublicErrorCode::NotFound
    );
    let markers = service.inner.drag_staging_ids.lock().unwrap();
    assert!(!markers.contains_key(&stale_id));
    assert!(markers.contains_key(&live_id));
}

/// 单项查询只是快照；后续动作仍以真实状态、提交回执和活记录存在性为准。
///
/// @author kongweiguang
#[tokio::test]
async fn single_id_snapshot_does_not_bypass_state_or_removal_guards() {
    let (service, _, _, _) = service_for(BackendMode::NoChanges);
    let origin = TransferOrigin::WorkspaceFile(TabId::new());
    let scope = TransferQueryScope::origins([origin.clone()]);
    let (id, mut record) = terminal_record(TransferFailureKind::Connection, 1);
    record.origin = Some(origin);
    record.task = TransferTask::queued(id.clone(), record.task.request().clone()).unwrap();
    service
        .inner
        .records
        .lock()
        .await
        .insert(id.clone(), record);
    assert_eq!(
        service.get_for_scope(&id, &scope).await.unwrap().status,
        TransferStatus::Queued
    );

    {
        let mut records = service.inner.records.lock().await;
        let record = records.get_mut(&id).unwrap();
        let destination = record.task.request().destination.clone();
        record.task.begin_connecting().unwrap();
        record.task.begin_transferring(Some(1)).unwrap();
        record.task.record_progress(1, Some(1)).unwrap();
        record.task.begin_commit(destination).unwrap();
        record
            .task
            .fail(
                TransferFailureKind::CommitUnknown,
                "受控未知提交".to_owned(),
                true,
                false,
            )
            .unwrap();
        service.persist_recovery_record(record).await.unwrap();
    }
    assert_eq!(
        service.retry(&id).await.unwrap_err().public_code(),
        PublicErrorCode::OutcomeUnknown
    );
    assert_eq!(
        service.cancel(&id).await.unwrap().status,
        TransferStatus::Failed
    );
    assert_eq!(
        service
            .get_for_scope(&id, &scope)
            .await
            .unwrap()
            .failure_kind,
        Some(TransferFailureKind::CommitUnknown)
    );

    service.inner.records.lock().await.remove(&id);
    assert!(
        service
            .list_recovery_for_scope(&scope, CancellationToken::new())
            .await
            .unwrap()
            .iter()
            .any(|record| record.id == id)
    );
    for lookup_scope in [&scope, &TransferQueryScope::All] {
        assert_eq!(
            service
                .get_for_scope(&id, lookup_scope)
                .await
                .unwrap_err()
                .public_code(),
            PublicErrorCode::NotFound
        );
    }
    assert_eq!(
        service.cancel(&id).await.unwrap_err().public_code(),
        PublicErrorCode::NotFound
    );
    assert_eq!(
        service.retry(&id).await.unwrap_err().public_code(),
        PublicErrorCode::NotFound
    );
    assert_eq!(
        service.resume(&id).await.unwrap_err().public_code(),
        PublicErrorCode::NotFound
    );
}

/// 读取之后新出现的清理 owner 仍由原重试门禁持有，旧摘要不能授予重放资格。
///
/// @author kongweiguang
#[tokio::test]
async fn single_id_snapshot_cannot_bypass_new_cleanup_owner() {
    let (service, _, _, _) = service_for(BackendMode::NoChanges);
    let origin = TransferOrigin::WorkspaceFile(TabId::new());
    let scope = TransferQueryScope::origins([origin.clone()]);
    let (id, mut record) = terminal_record(TransferFailureKind::Connection, 1);
    record.origin = Some(origin);
    service
        .inner
        .records
        .lock()
        .await
        .insert(id.clone(), record);
    assert!(
        !service
            .get_for_scope(&id, &scope)
            .await
            .unwrap()
            .cleanup_pending
    );

    let owner = Arc::new(RetryableCleanup {
        attempts: AtomicUsize::new(0),
        cleaned: AtomicBool::new(false),
    });
    service
        .inner
        .records
        .lock()
        .await
        .get_mut(&id)
        .unwrap()
        .cleanup_owner = Some(owner.clone());
    assert_eq!(
        service.retry(&id).await.unwrap_err().public_code(),
        PublicErrorCode::Conflict
    );
    assert!(
        service
            .get_for_scope(&id, &scope)
            .await
            .unwrap()
            .cleanup_pending
    );
    assert_eq!(owner.attempts.load(Ordering::Acquire), 0);
}

/// 恢复投影必须复用 live 去重规则；旧未绑定和已退役文件来源仍能从 All 访问。
///
/// @author kongweiguang
#[tokio::test]
async fn recovery_scopes_keep_legacy_unbound_and_authoritative_live_filtering() {
    let (service, _, _, _) = service_for(BackendMode::NoChanges);
    let legacy = TransferOrigin::WorkspaceFile(TabId::new());
    let selected = TransferOrigin::TerminalPane(PaneId::new());
    let other = TransferOrigin::TerminalPane(PaneId::new());
    let origins = [
        None,
        Some(legacy.clone()),
        Some(selected.clone()),
        Some(other),
    ];
    let mut ids = Vec::new();
    for (sequence, origin) in origins.iter().enumerate() {
        let (id, mut record) = terminal_record(TransferFailureKind::Connection, sequence as u64);
        record.origin = origin.clone();
        service.persist_recovery_record(&record).await.unwrap();
        ids.push(id);
    }
    let (live_id, mut live) = terminal_record(TransferFailureKind::Connection, 9);
    live.origin = Some(selected.clone());
    service.persist_recovery_record(&live).await.unwrap();
    service
        .inner
        .records
        .lock()
        .await
        .insert(live_id.clone(), live);
    let all = service
        .list_recovery_for_scope(&TransferQueryScope::All, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(all.len(), origins.len());
    assert!(all.iter().any(|item| item.origin.is_none()));
    assert!(!all.iter().any(|item| item.id == live_id));
    let scope = TransferQueryScope::Origins(vec![selected.clone(), legacy.clone(), legacy]);
    let scoped = service
        .list_recovery_for_scope(&scope, CancellationToken::new())
        .await
        .unwrap();
    let found = scoped
        .iter()
        .map(|item| item.id.clone())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        found,
        std::collections::HashSet::from([ids[1].clone(), ids[2].clone()])
    );
    assert!(
        service
            .list_recovery_for_scope(&TransferQueryScope::origins([]), CancellationToken::new())
            .await
            .unwrap()
            .is_empty()
    );
    let saved = service
        .inner
        .recovery_repository
        .list(CancellationToken::new())
        .await
        .unwrap();
    for (id, origin) in ids.iter().zip(origins) {
        assert_eq!(
            saved.iter().find(|item| &item.id == id).unwrap().origin,
            origin
        );
    }
}

/// 第一来源只有普通历史时，后续别名的 worker/join/cleanup/未知提交/拖出仍必须阻止整组关闭。
///
/// @author kongweiguang
#[tokio::test]
async fn scope_guard_checks_every_origin_and_real_lifecycle_owners() {
    let (service, _, _, _) = service_for(BackendMode::NoChanges);
    let safe_origin = TransferOrigin::WorkspaceFile(TabId::new());
    let blocked_origin = TransferOrigin::WorkspaceFile(TabId::new());
    let scope = TransferQueryScope::origins([safe_origin.clone(), blocked_origin.clone()]);
    let (safe_id, mut safe) = terminal_record(TransferFailureKind::Connection, 1);
    safe.origin = Some(safe_origin.clone());
    service.inner.records.lock().await.insert(safe_id, safe);
    assert!(
        !service
            .scope_has_close_blockers(&scope, CancellationToken::new())
            .await
            .unwrap()
    );
    for blocker in 0..7 {
        let kind = if blocker == 0 {
            TransferFailureKind::CommitUnknown
        } else {
            TransferFailureKind::Connection
        };
        let (id, mut record) = terminal_record(kind, 2);
        record.origin = Some(blocked_origin.clone());
        match blocker {
            1 => record.attempt_finished = false,
            2 => record.worker_join_pending = true,
            3 => {
                record.cleanup_owner = Some(Arc::new(RetryableCleanup {
                    attempts: AtomicUsize::new(0),
                    cleaned: AtomicBool::new(false),
                }))
            }
            4 => {
                service
                    .inner
                    .drag_staging_ids
                    .lock()
                    .unwrap()
                    .insert(id.clone(), true);
                record.drag_staging = true;
                record.drag_evidence_pending = true;
            }
            5 => {
                record.worker = Some(tokio::spawn(std::future::pending()));
            }
            6 => {
                let (_, mut paused, _, _) = cancelling_checkpoint_record();
                paused.task.confirm_cancelled(true).unwrap();
                paused.origin = Some(blocked_origin.clone());
                let paused_id = paused.task.id().clone();
                service
                    .inner
                    .records
                    .lock()
                    .await
                    .insert(paused_id.clone(), paused);
                assert!(
                    service
                        .scope_has_close_blockers(&scope, CancellationToken::new())
                        .await
                        .unwrap()
                );
                service.inner.records.lock().await.remove(&paused_id);
                continue;
            }
            _ => {}
        }
        service
            .inner
            .records
            .lock()
            .await
            .insert(id.clone(), record);
        assert!(
            service
                .scope_has_close_blockers(&scope, CancellationToken::new())
                .await
                .unwrap()
        );
        assert!(
            !service
                .origin_has_close_blockers(&safe_origin, CancellationToken::new())
                .await
                .unwrap()
        );
        let record = service.inner.records.lock().await.remove(&id).unwrap();
        if let Some(worker) = record.worker {
            worker.abort();
            assert!(worker.await.unwrap_err().is_cancelled());
        }
        service.inner.drag_staging_ids.lock().unwrap().remove(&id);
    }
}

/// 未绑定的孤儿不得隐式归给任何主机，但全局关闭核对不能漏掉它；空集合也不会提升到 All。
///
/// @author kongweiguang
#[tokio::test]
async fn scope_guard_keeps_unbound_recovery_and_empty_scope_distinct() {
    let (service, _, _, _) = service_for(BackendMode::NoChanges);
    let (_, orphan) = terminal_record(TransferFailureKind::CommitUnknown, 1);
    service.persist_recovery_record(&orphan).await.unwrap();
    assert!(
        service
            .scope_has_close_blockers(&TransferQueryScope::All, CancellationToken::new())
            .await
            .unwrap()
    );
    let scopes = [
        TransferQueryScope::origins([]),
        TransferQueryScope::origins([TransferOrigin::SharedWorkbench]),
    ];
    for scope in scopes {
        assert!(
            !service
                .scope_has_close_blockers(&scope, CancellationToken::new())
                .await
                .unwrap()
        );
    }
}

/// 注入不可靠读取而非真实磁盘故障，固定范围聚合的去重、错误和晚到取消边界。
struct QueryRepository {
    records: Vec<TransferRecoveryRecord>,
    fail: bool,
    cancel_on_read: bool,
}

#[async_trait]
impl TransferRecoveryRepository for QueryRepository {
    /// 仓储可能返回重复项或晚到结果，服务不得把这些结果解释成重复任务或取消成功。
    /// @author kongweiguang
    async fn list(&self, token: CancellationToken) -> AppResult<Vec<TransferRecoveryRecord>> {
        if self.fail {
            return Err(AppError::infrastructure("受控恢复读取失败"));
        }
        if self.cancel_on_read {
            token.cancel();
        }
        Ok(self.records.clone())
    }

    /// 只读替身禁止写入，测试不能通过伪写成功隐藏副作用。
    /// @author kongweiguang
    async fn upsert(&self, _: TransferRecoveryRecord, _: CancellationToken) -> AppResult<()> {
        Err(AppError::unsupported("只读范围查询替身"))
    }

    /// 只读替身不承诺已删除任何证据。
    /// @author kongweiguang
    async fn remove(&self, _: &TransferId, _: CancellationToken) -> AppResult<()> {
        Err(AppError::unsupported("只读范围查询替身"))
    }

    /// 只读替身不承诺已归档任何证据。
    /// @author kongweiguang
    async fn archive(&self, _: &TransferId, _: CancellationToken) -> AppResult<()> {
        Err(AppError::unsupported("只读范围查询替身"))
    }

    /// 不测试冷档分页，显式拒绝该能力以防范围查询误接归档入口。
    /// @author kongweiguang
    async fn list_archived(
        &self,
        _: Option<TransferId>,
        _: usize,
        _: CancellationToken,
    ) -> AppResult<TransferRecoveryPage> {
        Err(AppError::unsupported("只读范围查询替身"))
    }
}

/// 在共享测试 runtime 中替换仓储，不创建 worker 或第二套运行时。
///
/// @author kongweiguang
fn query_service(repository: QueryRepository) -> TransferService {
    let (backend, _) = FakeTransferBackend::new(BackendMode::NoChanges);
    TransferService::new_with_recovery_repository(
        vec![Arc::new(backend)],
        Handle::current(),
        TransferQueueOptions::default(),
        SftpTransferPolicyStore::new(SftpPerformanceSettings::default()).unwrap(),
        Arc::new(repository),
        None,
    )
    .unwrap()
}

/// 重复来源与重复恢复 ID 都只投影一次，不能变更保留下来的原始来源。
///
/// @author kongweiguang
#[tokio::test]
async fn recovery_scope_deduplicates_by_transfer_id() {
    let (fixture, _, _, _) = service_for(BackendMode::NoChanges);
    let origin = TransferOrigin::WorkspaceFile(TabId::new());
    let (_, mut record) = terminal_record(TransferFailureKind::CommitUnknown, 1);
    record.origin = Some(origin.clone());
    fixture.persist_recovery_record(&record).await.unwrap();
    let saved = fixture
        .inner
        .recovery_repository
        .list(CancellationToken::new())
        .await
        .unwrap()
        .remove(0);
    let service = query_service(QueryRepository {
        records: vec![saved.clone(), saved.clone()],
        fail: false,
        cancel_on_read: false,
    });
    for scope in [
        TransferQueryScope::All,
        TransferQueryScope::Origins(vec![origin.clone(), origin]),
    ] {
        assert_eq!(
            service
                .list_recovery_for_scope(&scope, CancellationToken::new())
                .await
                .unwrap(),
            vec![saved.clone()]
        );
    }
}

/// 队列锁被其它只读或收尾持有时仍响应取消，关闭不能无限等待后把撤销查询当成空集合。
///
/// @author kongweiguang
#[tokio::test]
async fn scope_queries_cancel_while_waiting_for_records_lock() {
    let (service, _, _, _) = service_for(BackendMode::NoChanges);
    let _records = service.inner.records.lock().await;
    for guard_query in [false, true] {
        let token = CancellationToken::new();
        let query = async {
            if guard_query {
                service
                    .scope_has_close_blockers(&TransferQueryScope::All, token.clone())
                    .await
                    .map(|_| ())
            } else {
                service
                    .list_recovery_for_scope(&TransferQueryScope::All, token.clone())
                    .await
                    .map(|_| ())
            }
        };
        let cancel = async {
            tokio::task::yield_now().await;
            token.cancel();
        };
        let (result, ()) = timeout(Duration::from_secs(1), async {
            tokio::join!(query, cancel)
        })
        .await
        .expect("查询取消不应等待记录锁释放");
        assert_eq!(
            result.unwrap_err().public_code(),
            PublicErrorCode::Cancelled
        );
    }
}

/// 即使范围为空，读取失败和前后取消都不能返回“没有 blocker”的伪成功。
///
/// @author kongweiguang
#[tokio::test]
async fn scope_queries_and_guard_preserve_errors_and_cancellation() {
    for (fail, cancel_on_read) in [(true, false), (false, true)] {
        let service = query_service(QueryRepository {
            records: Vec::new(),
            fail,
            cancel_on_read,
        });
        for scope in [TransferQueryScope::All, TransferQueryScope::origins([])] {
            assert!(
                service
                    .list_recovery_for_scope(&scope, CancellationToken::new())
                    .await
                    .is_err()
            );
            assert!(
                service
                    .scope_has_close_blockers(&scope, CancellationToken::new())
                    .await
                    .is_err()
            );
        }
    }
    let (service, _, _, _) = service_for(BackendMode::NoChanges);
    let token = CancellationToken::new();
    token.cancel();
    assert_eq!(
        service
            .list_recovery_for_scope(&TransferQueryScope::All, token.clone())
            .await
            .unwrap_err()
            .public_code(),
        PublicErrorCode::Cancelled
    );
    assert_eq!(
        service
            .scope_has_close_blockers(&TransferQueryScope::origins([]), token)
            .await
            .unwrap_err()
            .public_code(),
        PublicErrorCode::Cancelled
    );
}
