//! 传输恢复仓储使用任务私有目录验证当前格式，不连接 SSH 或访问真实用户数据。
//!
//! @author kongweiguang

use std::time::Duration;

use kerminal_application::connections::{HostKeyIdentity, HostKeyStatus, KnownHostsRepository};
use kerminal_application::transfers::{
    TransferCheckpoint, TransferRecoveryInspection, TransferRecoveryRecord,
    TransferRecoveryRepository, TransferStagingOwnership,
};
use kerminal_domain::{
    connections::HostEndpoint,
    files::{ConflictPolicy, FileEndpoint, FileRevision, TransferKind, TransferRequest},
    identity::TransferId,
};
use kerminal_infrastructure::connections::KnownHostsStore;
use kerminal_infrastructure::files::{
    JsonTransferRecoveryRepository, validate_transfer_recovery_document,
};
use kerminal_infrastructure::storage::{StorageCoordinator, StorageLockError};
use russh::keys::{HashAlg, ssh_key::PublicKey};
use tokio_util::sync::CancellationToken;

/// 隔离样本模拟继任任务沿用旧 owner；短暂存名绑定原 owner，不从最终文件名推导。
/// @author kongweiguang
fn record(root: &std::path::Path) -> TransferRecoveryRecord {
    let destination = FileEndpoint::Local {
        path: root.join("requested.txt"),
    };
    let actual = FileEndpoint::Local {
        path: root.join("requested (1).txt"),
    };
    let staging_owner = TransferId::new();
    let staging = FileEndpoint::Local {
        path: root.join(format!(
            ".kerminal-transfer-{}-{}.partial",
            staging_owner.as_str(),
            "c".repeat(32)
        )),
    };
    TransferRecoveryRecord {
        id: TransferId::new(),
        request: TransferRequest {
            source: FileEndpoint::Local {
                path: root.join("source.txt"),
            },
            destination,
            kind: TransferKind::File,
            conflict_policy: ConflictPolicy::Rename,
            idle_timeout_seconds: Some(180),
        },
        actual_destination: Some(actual.clone()),
        checkpoint: Some(TransferCheckpoint {
            source_revision: FileRevision {
                size: 10,
                modified: None,
                permissions: None,
                content_digest: Some("b".repeat(64)),
            },
            confirmed_bytes: 10,
            confirmed_prefix_sha256: "b".repeat(64),
            staging_owner: staging_owner.clone(),
            staging_destination: staging.clone(),
            staging_token: "a".repeat(32),
            actual_destination: actual.clone(),
        }),
        staging_ownership: vec![TransferStagingOwnership {
            staging_destination: staging,
            actual_destination: actual,
            staging_owner,
            staging_token: "a".repeat(32),
        }],
        cleanup_pending: true,
        commit_unknown: true,
        inspection: TransferRecoveryInspection::Insufficient,
        archived: false,
    }
}

/// 同一 owner 的完整非秘密事实可跨实例读取，实际目标和 stage token 无损保留。
#[tokio::test]
async fn recovery_round_trip_preserves_exact_target_and_ownership() {
    let temporary = tempfile::tempdir().expect("isolated fixture");
    let expected = record(temporary.path());
    assert_ne!(
        expected.id,
        expected
            .checkpoint
            .as_ref()
            .expect("checkpoint")
            .staging_owner
    );
    let repository = JsonTransferRecoveryRepository::new(temporary.path());
    repository
        .upsert(expected.clone(), CancellationToken::new())
        .await
        .expect("save recovery");
    let reopened = JsonTransferRecoveryRepository::new(temporary.path());
    assert_eq!(
        reopened
            .list(CancellationToken::new())
            .await
            .expect("read recovery"),
        vec![expected]
    );
    let source = tokio::fs::read_to_string(temporary.path().join("data/transfer-recovery.json"))
        .await
        .expect("document");
    validate_transfer_recovery_document(&source).expect("same validator as migration");
    for excluded in [
        "password",
        "privateKey",
        "passphrase",
        "externalTargetLease",
        "cleanupOwner",
        "pid",
    ] {
        assert!(
            !source.contains(excluded),
            "no runtime or credential field {excluded}"
        );
    }
}

/// 损坏或未来文档不能用空默认值覆盖，保存失败后逐字保留旧材料。
#[tokio::test]
async fn corrupt_document_blocks_writes_without_overwriting_evidence() {
    let temporary = tempfile::tempdir().expect("isolated fixture");
    tokio::fs::create_dir_all(temporary.path().join("data"))
        .await
        .expect("fixture directory");
    let path = temporary.path().join("data/transfer-recovery.json");
    let original = "{\"schemaVersion\":99,\"records\":[]}";
    tokio::fs::write(&path, original)
        .await
        .expect("fixture corruption");
    let repository = JsonTransferRecoveryRepository::new(temporary.path());
    assert!(repository.list(CancellationToken::new()).await.is_err());
    assert!(
        repository
            .upsert(record(temporary.path()), CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(
        tokio::fs::read_to_string(path)
            .await
            .expect("original retained"),
        original
    );
}

/// 已取消用例不创建存储目录或恢复文件，避免关闭后的迟到任务产生新写入。
#[tokio::test]
async fn cancelled_recovery_read_has_no_storage_side_effects() {
    let temporary = tempfile::tempdir().expect("isolated fixture");
    let root = temporary.path().join("not-created");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let repository = JsonTransferRecoveryRepository::new(&root);
    assert!(repository.list(cancellation).await.is_err());
    assert!(!root.exists());
}

/// 冷归档保存完整事实；普通更新、删除和重复归档都不能覆盖或擦除核对与 ownership 证据。
/// @author kongweiguang
#[tokio::test]
async fn archived_recovery_keeps_checkpoint_and_cleanup_ownership() {
    let temporary = tempfile::tempdir().expect("isolated fixture");
    let repository = JsonTransferRecoveryRepository::new(temporary.path());
    let mut expected = record(temporary.path());
    expected.inspection = TransferRecoveryInspection::Insufficient;
    repository
        .upsert(expected.clone(), CancellationToken::new())
        .await
        .expect("save active facts");
    repository
        .archive(&expected.id, CancellationToken::new())
        .await
        .expect("archive facts");
    assert!(
        repository
            .list(CancellationToken::new())
            .await
            .expect("active list")
            .is_empty()
    );
    assert!(
        repository
            .upsert(expected.clone(), CancellationToken::new())
            .await
            .is_err()
    );
    repository
        .remove(&expected.id, CancellationToken::new())
        .await
        .expect("active-only removal");
    repository
        .archive(&expected.id, CancellationToken::new())
        .await
        .expect("idempotent archive");
    expected.archived = true;
    assert_eq!(
        repository
            .list_archived(None, 100, CancellationToken::new())
            .await
            .expect("retained facts")
            .records,
        vec![expected]
    );
}

/// 真实根锁冲突必须保留 typed source；调用方显式重读后恢复，不做忙等或写重试。
/// @author kongweiguang
#[tokio::test]
async fn recovery_list_preserves_storage_lock_source_for_explicit_retry() {
    let temporary = tempfile::tempdir().expect("isolated fixture");
    let root = temporary.path();
    let held_storage = StorageCoordinator::new(root);
    let held_lock = held_storage.lock().expect("hold the real storage lock");
    let repository = JsonTransferRecoveryRepository::new(root);

    let error = repository
        .list(CancellationToken::new())
        .await
        .expect_err("a separate coordinator owns the lock");
    let source = std::error::Error::source(&error).expect("typed storage error source");
    let lock_error = source
        .downcast_ref::<StorageLockError>()
        .expect("preserved StorageLockError category");
    assert!(matches!(lock_error, &StorageLockError::Locked));
    assert!(!root.join("data/transfer-recovery.json").exists());
    assert!(root.join(".storage.lock").exists());

    drop(held_lock);
    assert!(
        repository
            .list(CancellationToken::new())
            .await
            .expect("explicit retry after releasing lock")
            .is_empty()
    );
    assert!(!root.join("data/transfer-recovery.json").exists());
}

/// 并发恢复读取与主机信任读取必须在共享根锁下排队，而非被误判为另一个进程的活锁。
/// 预热阻塞池后先持有同一 coordinator 的锁，再观察两项异步读取保持等待；释放后均应成功。
///
/// @author kongweiguang
#[tokio::test]
async fn recovery_and_known_hosts_serialize_on_the_shared_process_coordinator() {
    let temporary = tempfile::tempdir().expect("isolated fixture");
    let root = temporary.path();
    let storage = StorageCoordinator::new(root);
    let recovery = JsonTransferRecoveryRepository::with_storage_coordinator(storage.clone());
    let known_hosts = KnownHostsStore::new(storage.clone(), true);
    let endpoint = HostEndpoint {
        host: "shared-lock.example.test".to_owned(),
        port: 22461,
        username: "test".to_owned(),
    };
    let public_key =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    let key = PublicKey::from_openssh(public_key).expect("valid isolated test key");
    let identity = HostKeyIdentity {
        algorithm: key.algorithm().to_string(),
        fingerprint_sha256: key.fingerprint(HashAlg::Sha256).to_string(),
        public_key_openssh: public_key.to_owned(),
    };

    tokio::task::spawn_blocking(|| ())
        .await
        .expect("warm the blocking worker");
    let held_lock = storage.lock().expect("hold the shared root lock");
    let operations = async {
        tokio::join!(
            recovery.list(CancellationToken::new()),
            known_hosts.inspect(&endpoint, &identity),
        )
    };
    tokio::pin!(operations);

    assert!(
        tokio::time::timeout(Duration::from_millis(250), &mut operations)
            .await
            .is_err(),
        "both same-process operations must wait on the held shared coordinator"
    );
    drop(held_lock);

    let (records, trust) = tokio::time::timeout(Duration::from_secs(5), &mut operations)
        .await
        .expect("shared-lock operations complete after release");
    assert!(records.expect("recovery list succeeds").is_empty());
    assert!(matches!(
        trust.expect("known_hosts inspect succeeds"),
        HostKeyStatus::Unknown
    ));
    assert!(!root.join("data/transfer-recovery.json").exists());
}

/// 旧版本把归档留在 active；读取必须无副作用且可见，明确写操作才先存冷档再释放槽位。
/// @author kongweiguang
#[tokio::test]
async fn legacy_archives_are_visible_read_only_then_migrate_before_upsert() {
    let temporary = tempfile::tempdir().expect("isolated fixture");
    let repository = JsonTransferRecoveryRepository::new(temporary.path());
    let mut original = record(temporary.path());
    original.inspection = TransferRecoveryInspection::DestinationMatches;
    repository
        .upsert(original.clone(), CancellationToken::new())
        .await
        .expect("save fixture");
    let path = temporary.path().join("data/transfer-recovery.json");
    let mut document: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&path).await.expect("active document"))
            .expect("fixture JSON");
    document["records"][0]["archived"] = true.into();
    let legacy = serde_json::to_vec(&document).expect("legacy bytes");
    tokio::fs::write(&path, &legacy)
        .await
        .expect("write legacy fixture");

    assert!(
        repository
            .list(CancellationToken::new())
            .await
            .expect("read active")
            .is_empty()
    );
    let page = repository
        .list_archived(None, 1, CancellationToken::new())
        .await
        .expect("read legacy archive");
    original.archived = true;
    assert_eq!(page.records, vec![original.clone()]);
    assert_eq!(
        tokio::fs::read(&path).await.expect("unchanged document"),
        legacy
    );
    assert!(
        !temporary
            .path()
            .join("data/transfer-recovery-archive")
            .exists()
    );

    let next = record(temporary.path());
    repository
        .upsert(next.clone(), CancellationToken::new())
        .await
        .expect("migrate then save new task");
    assert_eq!(
        repository
            .list(CancellationToken::new())
            .await
            .expect("new active"),
        vec![next]
    );
    assert_eq!(
        repository
            .list_archived(None, 1, CancellationToken::new())
            .await
            .expect("cold archive")
            .records,
        vec![original]
    );
}

/// 模拟冷档已提交但 active 移除前中断；相同双份可幂等收敛，矛盾双份必须保留并报错。
/// @author kongweiguang
#[tokio::test]
async fn archive_duplicate_copy_is_idempotent_but_conflicting_facts_are_rejected() {
    let temporary = tempfile::tempdir().expect("isolated fixture");
    let repository = JsonTransferRecoveryRepository::new(temporary.path());
    let original = record(temporary.path());
    repository
        .upsert(original.clone(), CancellationToken::new())
        .await
        .expect("save fixture");
    let active_path = temporary.path().join("data/transfer-recovery.json");
    let before = tokio::fs::read(&active_path)
        .await
        .expect("original active bytes");
    repository
        .archive(&original.id, CancellationToken::new())
        .await
        .expect("durable archive");
    tokio::fs::write(&active_path, &before)
        .await
        .expect("simulate interrupted active removal");
    let reopened = JsonTransferRecoveryRepository::new(temporary.path());
    assert!(
        reopened
            .list(CancellationToken::new())
            .await
            .expect("deduplicated active")
            .is_empty()
    );
    assert_eq!(
        reopened
            .list_archived(None, 10, CancellationToken::new())
            .await
            .expect("one archive")
            .records
            .len(),
        1
    );
    assert_eq!(
        tokio::fs::read(&active_path)
            .await
            .expect("read did not migrate"),
        before
    );
    reopened
        .archive(&original.id, CancellationToken::new())
        .await
        .expect("idempotent removal");

    tokio::fs::write(&active_path, &before)
        .await
        .expect("restore interrupted fixture");
    let archive_path = std::fs::read_dir(temporary.path().join("data/transfer-recovery-archive"))
        .expect("archive directory")
        .next()
        .expect("archive entry")
        .expect("entry")
        .path();
    let mut archived: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&archive_path).await.expect("cold bytes"))
            .expect("cold JSON");
    archived["records"][0]["inspection"] = "destinationDiffers".into();
    let conflicting = serde_json::to_vec(&archived).expect("conflicting fixture bytes");
    tokio::fs::write(&archive_path, &conflicting)
        .await
        .expect("inject conflicting copy");
    assert!(
        reopened
            .archive(&original.id, CancellationToken::new())
            .await
            .is_err()
    );
    assert!(reopened.list(CancellationToken::new()).await.is_err());
    assert!(
        reopened
            .upsert(record(temporary.path()), CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(
        tokio::fs::read(&active_path)
            .await
            .expect("preserved active"),
        before
    );
    assert_eq!(
        tokio::fs::read(&archive_path)
            .await
            .expect("preserved archive"),
        conflicting
    );
}

/// active 真实达到 5000 条后归档一项即可重新入队；冷历史不占原文档的计数和字节预算。
/// @author kongweiguang
#[tokio::test]
async fn archiving_releases_a_slot_at_the_actual_active_capacity_limit() {
    let temporary = tempfile::tempdir().expect("isolated fixture");
    let repository = JsonTransferRecoveryRepository::new(temporary.path());
    let mut base = record(temporary.path());
    base.checkpoint = None;
    base.staging_ownership.clear();
    base.cleanup_pending = false;
    repository
        .upsert(base.clone(), CancellationToken::new())
        .await
        .expect("save schema fixture");
    let path = temporary.path().join("data/transfer-recovery.json");
    let mut document: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&path).await.expect("schema bytes"))
            .expect("schema JSON");
    let template = document["records"][0].clone();
    document["records"] = (0..5_000)
        .map(|index| {
            let mut entry = template.clone();
            entry["id"] = format!("capacity-{index:04}").into();
            entry
        })
        .collect::<Vec<_>>()
        .into();
    let full = serde_json::to_vec(&document).expect("full active fixture");
    assert!(full.len() <= 8 * 1024 * 1024);
    tokio::fs::write(&path, full)
        .await
        .expect("write full fixture");
    assert!(
        repository
            .upsert(base.clone(), CancellationToken::new())
            .await
            .is_err()
    );
    repository
        .archive(
            &TransferId::parse("capacity-0000").expect("fixture ID"),
            CancellationToken::new(),
        )
        .await
        .expect("free one active slot");
    repository
        .upsert(base, CancellationToken::new())
        .await
        .expect("new task after archive");
    assert_eq!(
        repository
            .list(CancellationToken::new())
            .await
            .expect("active remains bounded")
            .len(),
        5_000
    );
    assert_eq!(
        repository
            .list_archived(None, 100, CancellationToken::new())
            .await
            .expect("separate cold history")
            .records
            .len(),
        1
    );
}

/// 分页只解析本页档案；其它页损坏不会被预先全量加载，访问该页时必须明确失败而非跳过。
/// @author kongweiguang
#[tokio::test]
async fn archive_pages_are_bounded_and_corruption_is_not_silently_skipped() {
    let temporary = tempfile::tempdir().expect("isolated fixture");
    let repository = JsonTransferRecoveryRepository::new(temporary.path());
    for _ in 0..3 {
        let item = record(temporary.path());
        repository
            .upsert(item.clone(), CancellationToken::new())
            .await
            .expect("save item");
        repository
            .archive(&item.id, CancellationToken::new())
            .await
            .expect("archive item");
    }
    let mut paths = std::fs::read_dir(temporary.path().join("data/transfer-recovery-archive"))
        .expect("cold directory")
        .map(|entry| entry.expect("entry").path())
        .collect::<Vec<_>>();
    paths.sort();
    let corrupt_path = paths.last().expect("three archive files");
    tokio::fs::write(corrupt_path, b"not-json")
        .await
        .expect("inject later-page corruption");
    let first = repository
        .list_archived(None, 1, CancellationToken::new())
        .await
        .expect("first page does not decode later files");
    assert_eq!(first.records.len(), 1);
    assert_eq!(
        first.next_cursor.as_ref(),
        first.records.last().map(|record| &record.id)
    );
    let second = repository
        .list_archived(first.next_cursor, 1, CancellationToken::new())
        .await
        .expect("second page");
    assert_eq!(second.records.len(), 1);
    assert!(second.next_cursor.is_some());
    assert_eq!(
        second.next_cursor.as_ref(),
        second.records.last().map(|record| &record.id)
    );
    assert!(
        repository
            .list_archived(second.next_cursor, 1, CancellationToken::new())
            .await
            .is_err()
    );
    assert!(
        repository
            .list_archived(None, 0, CancellationToken::new())
            .await
            .is_err()
    );
    assert!(
        repository
            .list_archived(None, 101, CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(
        tokio::fs::read(corrupt_path)
            .await
            .expect("corrupt evidence retained"),
        b"not-json"
    );
}

/// 旧任意 ID 仅进入内容，磁盘使用固定键；错误文件名与内容 ID 的组合不能被当作可信冷档。
/// @author kongweiguang
#[tokio::test]
async fn archive_file_keys_are_safe_and_must_match_the_original_identity() {
    let temporary = tempfile::tempdir().expect("isolated fixture");
    let repository = JsonTransferRecoveryRepository::new(temporary.path());
    let mut item = record(temporary.path());
    item.id = TransferId::parse("../../旧ID:中文\\target").expect("opaque legacy ID");
    repository
        .upsert(item.clone(), CancellationToken::new())
        .await
        .expect("save old ID");
    repository
        .archive(&item.id, CancellationToken::new())
        .await
        .expect("safe hashed archive key");
    let path = std::fs::read_dir(temporary.path().join("data/transfer-recovery-archive"))
        .expect("cold directory")
        .next()
        .expect("one archive")
        .expect("entry")
        .path();
    assert_eq!(
        path.file_name().expect("file name").to_string_lossy().len(),
        69
    );
    let mut document: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&path).await.expect("cold bytes"))
            .expect("cold JSON");
    document["records"][0]["id"] = TransferId::new().as_str().into();
    tokio::fs::write(
        &path,
        serde_json::to_vec(&document).expect("mismatched fixture"),
    )
    .await
    .expect("inject ID mismatch");
    assert!(
        repository
            .list_archived(None, 10, CancellationToken::new())
            .await
            .is_err()
    );
}

/// 取消发生在提交前不得创建冷档或删除 active；未知目录项不能因形似内部暂存名被吞掉。
/// @author kongweiguang
#[tokio::test]
async fn archive_cancellation_and_unrecognized_entries_preserve_evidence() {
    let temporary = tempfile::tempdir().expect("isolated fixture");
    let repository = JsonTransferRecoveryRepository::new(temporary.path());
    let item = record(temporary.path());
    repository
        .upsert(item.clone(), CancellationToken::new())
        .await
        .expect("save fixture");
    let active_path = temporary.path().join("data/transfer-recovery.json");
    let before = tokio::fs::read(&active_path).await.expect("active bytes");
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(
        repository
            .archive(&item.id, cancelled.clone())
            .await
            .is_err()
    );
    assert!(repository.list_archived(None, 1, cancelled).await.is_err());
    let directory = temporary.path().join("data/transfer-recovery-archive");
    assert!(!directory.exists());
    assert_eq!(
        tokio::fs::read(&active_path)
            .await
            .expect("active preserved"),
        before
    );

    repository
        .archive(&item.id, CancellationToken::new())
        .await
        .expect("archive fixture");
    let unexpected = directory.join(".kr-malformed.tmp");
    tokio::fs::write(&unexpected, b"unrecognized evidence")
        .await
        .expect("unexpected entry");
    assert!(
        repository
            .list_archived(None, 1, CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(
        tokio::fs::read(unexpected)
            .await
            .expect("unrecognized file retained"),
        b"unrecognized evidence"
    );
}

/// 不相关目标、伪 owner 和跨目录断点不能作为有效恢复材料写入，失败不创建新文档。
/// @author kongweiguang
#[tokio::test]
async fn invalid_recovery_scope_and_staging_identity_are_rejected() {
    let temporary = tempfile::tempdir().expect("isolated fixture");
    let repository = JsonTransferRecoveryRepository::new(temporary.path());
    let base = record(temporary.path());
    let mut unrelated = base.clone();
    let outside = FileEndpoint::Local {
        path: temporary.path().join("outside/requested.txt"),
    };
    unrelated.actual_destination = Some(outside.clone());
    unrelated
        .checkpoint
        .as_mut()
        .expect("checkpoint")
        .actual_destination = outside.clone();
    unrelated.staging_ownership[0].actual_destination = outside;
    let mut wrong_owner = base.clone();
    wrong_owner.staging_ownership[0].staging_owner = TransferId::new();
    let mut cross_directory = base.clone();
    let checkpoint = cross_directory.checkpoint.as_mut().expect("checkpoint");
    let filename = match &checkpoint.staging_destination {
        FileEndpoint::Local { path } => path.file_name().expect("short stage").to_owned(),
        _ => unreachable!("local fixture"),
    };
    checkpoint.staging_destination = FileEndpoint::Local {
        path: temporary.path().join("unrelated").join(filename),
    };
    for invalid in [unrelated, wrong_owner, cross_directory] {
        assert!(
            repository
                .upsert(invalid, CancellationToken::new())
                .await
                .is_err()
        );
    }
    assert!(
        !temporary
            .path()
            .join("data/transfer-recovery.json")
            .exists()
    );
}

/// 目录合并允许根内子项，但不能把名字相似的旁系目录或含 .. 的路径当成后代。
/// @author kongweiguang
#[tokio::test]
async fn directory_recovery_accepts_children_without_expanding_root_scope() {
    let temporary = tempfile::tempdir().expect("isolated fixture");
    let repository = JsonTransferRecoveryRepository::new(temporary.path());
    let mut expected = record(temporary.path());
    expected.request.kind = TransferKind::Directory;
    expected.checkpoint = None;
    let root = temporary.path().join("requested (1).txt");
    let stage_name = format!(".kerminal-file-{}.partial", "d".repeat(32));
    expected.staging_ownership[0].actual_destination = FileEndpoint::Local {
        path: root.join("nested/payload.bin"),
    };
    expected.staging_ownership[0].staging_destination = FileEndpoint::Local {
        path: root.join("nested").join(&stage_name),
    };
    repository
        .upsert(expected.clone(), CancellationToken::new())
        .await
        .expect("root-owned child");
    for invalid_parent in [
        temporary.path().join("requested (1).txt-elsewhere"),
        root.join("../outside"),
    ] {
        let mut invalid = expected.clone();
        invalid.staging_ownership[0].actual_destination = FileEndpoint::Local {
            path: invalid_parent.join("payload.bin"),
        };
        invalid.staging_ownership[0].staging_destination = FileEndpoint::Local {
            path: invalid_parent.join(&stage_name),
        };
        assert!(
            repository
                .upsert(invalid, CancellationToken::new())
                .await
                .is_err()
        );
    }
    assert_eq!(
        repository
            .list(CancellationToken::new())
            .await
            .expect("original retained"),
        vec![expected]
    );
}
