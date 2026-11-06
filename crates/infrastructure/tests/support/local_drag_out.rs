//! 拖出暂存复用真实传输后端的生命周期回归。
//!
//! @author kongweiguang

use std::{path::PathBuf, sync::Arc, time::Duration};

use kerminal_application::files::{
    FileService,
    drag_out::{DragOutSource, prepare_drag_out, prepare_drag_out_observed_for_origin},
};
use kerminal_application::transfers::{
    TransferBackend, TransferExecutionContext, TransferExecutionError, TransferExecutionResult,
    TransferOrigin, TransferQueueOptions, TransferService,
};
use kerminal_domain::{
    files::{FileEndpoint, TransferKind, TransferStatus},
    identity::PaneId,
};
use kerminal_infrastructure::files::{JsonFileBookmarkRepository, LocalFileProvider};
use tokio_util::sync::CancellationToken;

use super::local_transfer_service;

/// 文件适配器使用隔离的书签根，测试不访问真实用户配置。
///
/// @author kongweiguang
fn file_service(root: PathBuf) -> FileService {
    FileService::new(
        vec![Arc::new(LocalFileProvider)],
        Arc::new(JsonFileBookmarkRepository::new(root)),
    )
    .expect("test file service")
}

/// 多选必须等所有真实文件和空目录提交后才返回，owner 释放后后台清理暂存。
///
/// @author kongweiguang
#[tokio::test]
async fn staging_multiple_entries_keeps_empty_directories_until_owner_is_dropped() {
    let fixture = tempfile::tempdir().expect("isolated sources");
    let source_file = fixture.path().join("one.txt");
    let source_dir = fixture.path().join("folder");
    tokio::fs::write(&source_file, b"test data")
        .await
        .expect("write source");
    tokio::fs::create_dir_all(source_dir.join("empty"))
        .await
        .expect("create empty directory");
    tokio::fs::write(source_dir.join("nested.txt"), b"nested")
        .await
        .expect("write nested source");
    let (transfers, _, _) = local_transfer_service();
    let prepared = prepare_drag_out(
        file_service(fixture.path().to_path_buf()),
        transfers.clone(),
        CancellationToken::new(),
        vec![
            DragOutSource {
                endpoint: FileEndpoint::Local { path: source_file },
                name: "one.txt".into(),
                kind: TransferKind::File,
                external_target_lease: None,
            },
            DragOutSource {
                endpoint: FileEndpoint::Local { path: source_dir },
                name: "folder".into(),
                kind: TransferKind::Directory,
                external_target_lease: None,
            },
        ],
    )
    .await
    .expect("all staged items have confirmed commits");
    assert_eq!(prepared.paths.len(), 2);
    let stage_root = prepared.paths[0]
        .0
        .parent()
        .expect("staging parent")
        .to_path_buf();
    assert_eq!(
        tokio::fs::read(&prepared.paths[0].0)
            .await
            .expect("staged file"),
        b"test data"
    );
    assert!(prepared.paths[1].0.join("empty").is_dir());
    assert_eq!(
        tokio::fs::read(prepared.paths[1].0.join("nested.txt"))
            .await
            .expect("staged nested file"),
        b"nested"
    );
    let summaries = transfers.list().await;
    assert_eq!(summaries.len(), 2);
    assert!(
        summaries
            .iter()
            .all(|summary| summary.status == TransferStatus::Succeeded
                && transfers.is_drag_staging(&summary.id))
    );
    let (_, remaining) = transfers
        .clear_completed(CancellationToken::new())
        .await
        .expect("clear ordinary history");
    assert_eq!(
        remaining, 2,
        "active drag evidence must survive history clearing"
    );
    assert!(
        summaries
            .iter()
            .all(|summary| transfers.is_drag_staging_pending_delivery(&summary.id))
    );
    let session = prepared.session.clone();
    drop(prepared);
    assert!(
        stage_root.exists(),
        "staging remains owned until target completion"
    );
    session
        .cleanup()
        .await
        .expect("confirmed delivery releases staging and evidence");
    tokio::time::timeout(Duration::from_secs(5), async {
        while stage_root.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("temporary staging is removed after its last owner");
    transfers.shutdown().await.expect("queue shutdown");
}

/// 整次多选拖出及未知结果证据都绑定发起面板；旧入口仍不归入任意面板。
///
/// @author kongweiguang
#[tokio::test]
async fn staging_keeps_original_origin_for_multiple_entries_and_recovery() {
    let fixture = tempfile::tempdir().expect("isolated sources");
    let first = fixture.path().join("first.txt");
    let second = fixture.path().join("second.txt");
    tokio::fs::write(&first, b"first")
        .await
        .expect("first source");
    tokio::fs::write(&second, b"second")
        .await
        .expect("second source");
    let source = |path: PathBuf, name: &str| DragOutSource {
        endpoint: FileEndpoint::Local { path },
        name: name.into(),
        kind: TransferKind::File,
        external_target_lease: None,
    };
    let (transfers, _, _) = local_transfer_service();
    let original = TransferOrigin::TerminalPane(PaneId::new());
    let other = TransferOrigin::TerminalPane(PaneId::new());
    let prepared = prepare_drag_out_observed_for_origin(
        file_service(fixture.path().to_path_buf()),
        transfers.clone(),
        CancellationToken::new(),
        vec![
            source(first.clone(), "first.txt"),
            source(second, "second.txt"),
        ],
        original.clone(),
        |_| {},
    )
    .await
    .expect("scoped staging succeeds");
    let legacy = prepare_drag_out(
        file_service(fixture.path().to_path_buf()),
        transfers.clone(),
        CancellationToken::new(),
        vec![source(first, "first.txt")],
    )
    .await
    .expect("legacy staging succeeds");

    let scoped = transfers.list_for_origin(&original).await;
    assert_eq!(scoped.len(), 2);
    assert!(transfers.list_for_origin(&other).await.is_empty());
    assert_eq!(transfers.list().await.len(), 3);
    assert_eq!(
        transfers
            .clear_completed_for_origin(&original, CancellationToken::new())
            .await
            .expect("active drag retains its evidence"),
        (0, 2)
    );

    prepared.session.retain_unknown().await;
    let recovery = transfers
        .list_recovery_for_origin(&original, CancellationToken::new())
        .await
        .expect("scoped recovery");
    assert_eq!(recovery.len(), 2);
    assert!(recovery.iter().all(|record| {
        record.origin.as_ref() == Some(&original)
            && record.drag_staging
            && scoped.iter().any(|summary| summary.id == record.id)
    }));
    assert!(
        transfers
            .list_recovery_for_origin(&other, CancellationToken::new())
            .await
            .expect("other recovery")
            .is_empty()
    );
    let retained = prepared.session.directory();
    assert!(prepared.session.cleanup().await.is_err());
    assert!(retained.join("first.txt").exists());
    assert!(retained.join("second.txt").exists());
    legacy
        .session
        .cleanup()
        .await
        .expect("release legacy staging");
    drop(legacy);
    drop(prepared);
    tokio::fs::remove_dir_all(retained)
        .await
        .expect("remove this test's retained staging");
    transfers.shutdown().await.expect("queue shutdown");
}

/// 目标名称冲突与取消不能产生第二个写入或向系统暴露半成品。
///
/// @author kongweiguang
#[tokio::test]
async fn staging_rejects_colliding_names_and_pre_cancelled_gestures() {
    let fixture = tempfile::tempdir().expect("isolated sources");
    let source = fixture.path().join("file.txt");
    tokio::fs::write(&source, b"data")
        .await
        .expect("write source");
    let item = DragOutSource {
        endpoint: FileEndpoint::Local { path: source },
        name: "file.txt".into(),
        kind: TransferKind::File,
        external_target_lease: None,
    };
    let (transfers, _, _) = local_transfer_service();
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(
        prepare_drag_out(
            file_service(fixture.path().to_path_buf()),
            transfers.clone(),
            cancelled,
            vec![item.clone()]
        )
        .await
        .is_err()
    );
    assert!(transfers.list().await.is_empty());
    let duplicate = DragOutSource {
        name: "FILE.TXT".into(),
        ..item.clone()
    };
    assert!(
        prepare_drag_out(
            file_service(fixture.path().to_path_buf()),
            transfers.clone(),
            CancellationToken::new(),
            vec![item, duplicate]
        )
        .await
        .is_err()
    );
    assert_eq!(transfers.list().await.len(), 1);
    transfers.shutdown().await.expect("queue shutdown");
}

/// 未确认的目标复制保留暂存和恢复证据，清理历史也不能抹掉恢复入口。
///
/// @author kongweiguang
#[tokio::test]
async fn unknown_delivery_exposes_evidence_after_history_is_cleared() {
    let fixture = tempfile::tempdir().expect("isolated sources");
    let source = fixture.path().join("file.txt");
    tokio::fs::write(&source, b"preserved")
        .await
        .expect("write source");
    let (transfers, _, _) = local_transfer_service();
    let prepared = prepare_drag_out(
        file_service(fixture.path().to_path_buf()),
        transfers.clone(),
        CancellationToken::new(),
        vec![DragOutSource {
            endpoint: FileEndpoint::Local { path: source },
            name: "file.txt".into(),
            kind: TransferKind::File,
            external_target_lease: None,
        }],
    )
    .await
    .expect("staging succeeds");
    let session = prepared.session.clone();
    let directory = session.directory();
    let transfer_id = transfers.list().await[0].id.clone();
    session.retain_unknown().await;
    assert!(session.is_preserved());
    assert!(session.cleanup().await.is_err());
    assert!(
        transfers
            .list_recovery(CancellationToken::new())
            .await
            .expect("list recovery")
            .iter()
            .any(|record| record.id == transfer_id)
    );
    transfers
        .clear_completed(CancellationToken::new())
        .await
        .expect("clear history");
    assert!(
        transfers
            .list_recovery(CancellationToken::new())
            .await
            .expect("list recovery after clearing")
            .iter()
            .any(|record| record.id == transfer_id)
    );
    assert_eq!(
        tokio::fs::read(directory.join("file.txt"))
            .await
            .expect("preserved file"),
        b"preserved"
    );
    drop(prepared);
    drop(session);
    tokio::fs::remove_dir_all(&directory)
        .await
        .expect("remove this test's isolated staging");
    transfers.shutdown().await.expect("queue shutdown");
}

/// 暂存适配器与虚拟文件清单使用同一 Windows 保留设备名边界。
///
/// @author kongweiguang
#[tokio::test]
async fn staging_rejects_windows_device_names_with_extensions() {
    let fixture = tempfile::tempdir().expect("isolated fixture");
    let staging = file_service(fixture.path().to_path_buf())
        .create_drag_staging()
        .await
        .expect("create isolated staging");
    for name in ["CON .txt", "CLOCK$.txt"] {
        assert!(staging.path_for(name).is_err(), "reserved name: {name}");
    }
    staging.cleanup().await.expect("remove test staging");
}

struct DelayedReleaseBackend {
    committed: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl TransferBackend for DelayedReleaseBackend {
    /// 后端只接收本机来源与目标，避免测试意外访问任何外部连接。
    ///
    /// @author kongweiguang
    fn supports(&self, request: &kerminal_domain::files::TransferRequest) -> bool {
        matches!(request.source, FileEndpoint::Local { .. })
            && matches!(request.destination, FileEndpoint::Local { .. })
    }

    /// 先确认文件提交，再延迟模拟协议资源关闭，复现提交与完整收尾之间的窗口。
    ///
    /// @author kongweiguang
    async fn execute(
        &self,
        context: TransferExecutionContext,
    ) -> Result<TransferExecutionResult, TransferExecutionError> {
        let destination = context.request.destination.clone();
        let FileEndpoint::Local { path } = &destination else {
            unreachable!()
        };
        tokio::fs::write(path, b"done")
            .await
            .expect("create staged test file");
        context
            .progress
            .begin_transfer(Some(4))
            .await
            .expect("start transfer");
        context
            .progress
            .begin_commit(destination.clone(), 4, Some(4), None)
            .await
            .expect("start commit");
        context
            .progress
            .record_committed_entry(4)
            .await
            .expect("record commit");
        context
            .progress
            .finish_commit()
            .await
            .expect("finish commit barrier");
        self.committed.notify_one();
        self.release.notified().await;
        Ok(TransferExecutionResult {
            actual_destination: destination,
            total_bytes: Some(4),
            skipped: false,
            unchanged: false,
        })
    }
}

/// Succeeded 只证明提交，原生拖放必须继续等待后端释放资源及 worker 收尾。
///
/// @author kongweiguang
#[tokio::test]
async fn staging_waits_for_backend_release_after_commit() {
    let fixture = tempfile::tempdir().expect("isolated sources");
    let committed = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let backend = Arc::new(DelayedReleaseBackend {
        committed: committed.clone(),
        release: release.clone(),
    });
    let transfers = TransferService::new(
        vec![backend],
        tokio::runtime::Handle::current(),
        TransferQueueOptions::default(),
    )
    .expect("test queue");
    let task = tokio::spawn(prepare_drag_out(
        file_service(fixture.path().to_path_buf()),
        transfers.clone(),
        CancellationToken::new(),
        vec![DragOutSource {
            endpoint: FileEndpoint::Local {
                path: fixture.path().join("source.txt"),
            },
            name: "source.txt".into(),
            kind: TransferKind::File,
            external_target_lease: None,
        }],
    ));
    tokio::time::timeout(Duration::from_secs(5), committed.notified())
        .await
        .expect("backend committed");
    assert!(
        !task.is_finished(),
        "commit acknowledgement alone cannot release staging"
    );
    release.notify_one();
    let prepared = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("preparation completed")
        .expect("task joined")
        .expect("staging succeeded");
    assert_eq!(
        tokio::fs::read(&prepared.paths[0].0)
            .await
            .expect("staged file"),
        b"done"
    );
    prepared
        .session
        .cleanup()
        .await
        .expect("release confirmed staging");
    drop(prepared);
    transfers.shutdown().await.expect("queue shutdown");
}
