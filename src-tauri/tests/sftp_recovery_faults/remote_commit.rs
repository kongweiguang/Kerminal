//! 覆盖提交的 fallback、回滚、未知回执和取消边界集成测试。
//!
//! @author kongweiguang

use super::*;

/// 不支持覆盖的服务器应经同目录 backup fallback 完成 overwrite，并保留未被本任务认领的旧 partial。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overwrite_fallback_commits_and_preserves_untrusted_partial() {
    let server_root = tempdir().expect("create fault server root");
    let client_root = tempdir().expect("create client fixture root");
    let source = client_root.path().join("replacement.bin");
    write_pattern(&source, 256 * 1024).await;
    fs::write(server_root.path().join("artifact.bin"), b"original-content")
        .await
        .expect("seed original final");
    let orphan = server_root.path().join("artifact.bin.kerminal-part");
    fs::write(&orphan, b"old untrusted partial")
        .await
        .expect("seed untrusted old partial");
    let server = start_fault_server(server_root.path().to_path_buf(), FaultMode::Normal).await;
    let (_home, state) = test_state();
    let host_id = create_loopback_host(&state, "overwrite-conflict", server.addr.port());
    trust_loopback_host(&state, &host_id).await;
    let task = state
        .sftp()
        .enqueue_transfer(
            state.paths(),
            upload_request(&host_id, &source, "/artifact.bin"),
        )
        .expect("enqueue overwrite probe");
    let completed = wait_for_terminal(&state, &task.id, Duration::from_secs(5)).await;
    assert_eq!(completed.status, SftpTransferStatus::Succeeded);
    assert_eq!(
        sha256_file(&source).await,
        sha256_file(&server_root.path().join("artifact.bin")).await
    );
    assert_eq!(
        fs::read(&orphan).await.expect("read untrusted partial"),
        b"old untrusted partial"
    );
    let mut entries = fs::read_dir(server_root.path()).await.expect("directory");
    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await.expect("entry") {
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    assert_eq!(
        names,
        vec!["artifact.bin", "artifact.bin.kerminal-part"],
        "successful fallback removes its backup and owned partial only"
    );
}

/// Acknowledged rollback restores the previous final; switching the injected server back to normal
/// proves that retry consumes the retained complete partial and closes the backup lifecycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_fallback_commit_rolls_back_and_remains_retryable() {
    let server_root = tempdir().expect("create fault server root");
    let client_root = tempdir().expect("create client fixture root");
    let source = client_root.path().join("replacement.bin");
    write_pattern(&source, 128 * 1024).await;
    fs::write(server_root.path().join("artifact.bin"), b"original-content")
        .await
        .expect("seed original final");
    let server = start_fault_server(
        server_root.path().to_path_buf(),
        FaultMode::RejectPartialCommit,
    )
    .await;
    let (_home, state) = test_state();
    let host_id = create_loopback_host(&state, "fallback-rollback", server.addr.port());
    trust_loopback_host(&state, &host_id).await;
    let task = state
        .sftp()
        .enqueue_transfer(
            state.paths(),
            upload_request(&host_id, &source, "/artifact.bin"),
        )
        .expect("enqueue overwrite with injected commit refusal");

    let failed = wait_for_terminal(&state, &task.id, Duration::from_secs(5)).await;
    assert_eq!(failed.status, SftpTransferStatus::Failed);
    assert!(
        failed.retryable,
        "confirmed rollback keeps the upload retryable: {failed:?}"
    );
    assert_ne!(
        failed.failure_kind,
        Some(SftpTransferFailureKind::CommitUnknown)
    );
    assert_eq!(
        fs::read(server_root.path().join("artifact.bin"))
            .await
            .expect("restored final"),
        b"original-content"
    );
    assert_eq!(
        sha256_file(&source).await,
        sha256_file(&server_root.path().join("artifact.bin.kerminal-part")).await
    );
    let mut entries = fs::read_dir(server_root.path()).await.expect("directory");
    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await.expect("entry") {
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    assert_eq!(names, vec!["artifact.bin", "artifact.bin.kerminal-part"]);

    server.set_mode(FaultMode::Normal);
    let successor = state
        .sftp()
        .retry_transfer(
            state.paths(),
            SftpTransferRetryRequest {
                transfer_id: failed.id.clone(),
                view_scope: None,
            },
        )
        .expect("confirmed rollback must allow a retry");
    let completed = wait_for_terminal(&state, &successor.id, Duration::from_secs(5)).await;
    assert_eq!(completed.status, SftpTransferStatus::Succeeded);
    assert_eq!(
        sha256_file(&source).await,
        sha256_file(&server_root.path().join("artifact.bin")).await,
        "retry must commit the original source bytes"
    );
    let mut entries = fs::read_dir(server_root.path())
        .await
        .expect("directory after retry");
    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await.expect("entry after retry") {
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    assert_eq!(
        names,
        vec!["artifact.bin"],
        "retry removes owned part and backup"
    );
}

/// If the server also refuses backup restoration, retain both recovery inputs and reject blind retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_fallback_rollback_is_unknown_and_keeps_backup_and_partial() {
    let server_root = tempdir().expect("create fault server root");
    let client_root = tempdir().expect("create client fixture root");
    let source = client_root.path().join("replacement.bin");
    write_pattern(&source, 128 * 1024).await;
    fs::write(server_root.path().join("artifact.bin"), b"original-content")
        .await
        .expect("seed original final");
    let server = start_fault_server(
        server_root.path().to_path_buf(),
        FaultMode::RejectCommitAndRollback,
    )
    .await;
    let (_home, state) = test_state();
    let host_id = create_loopback_host(&state, "fallback-unknown", server.addr.port());
    trust_loopback_host(&state, &host_id).await;
    let task = state
        .sftp()
        .enqueue_transfer(
            state.paths(),
            upload_request(&host_id, &source, "/artifact.bin"),
        )
        .expect("enqueue overwrite with injected rollback refusal");

    let failed = wait_for_terminal(&state, &task.id, Duration::from_secs(5)).await;
    assert_eq!(failed.status, SftpTransferStatus::Failed);
    assert_eq!(
        failed.failure_kind,
        Some(SftpTransferFailureKind::CommitUnknown)
    );
    assert!(
        !failed.retryable,
        "unknown commit must not be blindly retried"
    );
    let error = failed.error.as_deref().expect("recovery detail");
    assert!(error.contains("artifact.bin"), "{error}");
    assert!(error.contains(".kerminal-part"), "{error}");
    assert!(error.contains(".kerminal-backup"), "{error}");

    assert!(!server_root.path().join("artifact.bin").exists());
    assert_eq!(
        sha256_file(&source).await,
        sha256_file(&server_root.path().join("artifact.bin.kerminal-part")).await
    );
    let mut entries = fs::read_dir(server_root.path()).await.expect("directory");
    let mut backup_contents = None;
    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await.expect("entry") {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != "artifact.bin.kerminal-part" {
            backup_contents = Some(fs::read(entry.path()).await.expect("read backup"));
        }
        names.push(name);
    }
    assert_eq!(
        names.len(),
        2,
        "partial and backup remain for manual recovery"
    );
    assert_eq!(
        backup_contents.as_deref(),
        Some(b"original-content".as_slice())
    );

    let retry = state.sftp().retry_transfer(
        state.paths(),
        SftpTransferRetryRequest {
            transfer_id: failed.id.clone(),
            view_scope: None,
        },
    );
    assert!(
        retry.is_err(),
        "unknown commit must reject blind retry: {retry:?}"
    );
    let still_unknown = state
        .sftp()
        .list_transfers()
        .expect("list transfers after rejected retry");
    assert_eq!(still_unknown.len(), 1, "rejected retry creates no writer");
    assert_eq!(still_unknown[0].id, failed.id);
    assert_eq!(
        still_unknown[0].failure_kind,
        Some(SftpTransferFailureKind::CommitUnknown)
    );
}

/// Cancel after the final is backed up and the partial rename is pending; elapsed ACK uncertainty
/// must retain both recovery files, freeze retry, and finish within the bounded cancel grace period.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_during_fallback_commit_keeps_backup_and_partial_as_unknown() {
    let server_root = tempdir().expect("create fault server root");
    let client_root = tempdir().expect("create client fixture root");
    let source = client_root.path().join("replacement.bin");
    write_pattern(&source, 128 * 1024).await;
    fs::write(server_root.path().join("artifact.bin"), b"original-content")
        .await
        .expect("seed original final");
    let server = start_fault_server(
        server_root.path().to_path_buf(),
        FaultMode::StallCommitAfterBackup,
    )
    .await;
    let (_home, state) = test_state();
    let host_id = create_loopback_host(&state, "fallback-cancel-unknown", server.addr.port());
    trust_loopback_host(&state, &host_id).await;
    let task = state
        .sftp()
        .enqueue_transfer(
            state.paths(),
            upload_request(&host_id, &source, "/artifact.bin"),
        )
        .expect("enqueue overwrite with a stalled fallback commit");

    wait_for_phase(&state, &task.id, "committing", Duration::from_secs(5)).await;
    let started = Instant::now();
    let backup_path = loop {
        let mut entries = fs::read_dir(server_root.path())
            .await
            .expect("inspect fallback backup");
        let mut found = None;
        while let Some(entry) = entries.next_entry().await.expect("backup entry") {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".kerminal-backup") {
                found = Some(entry.path());
                break;
            }
        }
        if let Some(path) = found.filter(|_| server.commit_stalls() > 0) {
            break path;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "fallback did not reach the pending partial rename"
        );
        sleep(Duration::from_millis(20)).await;
    };

    let cancel_started = Instant::now();
    state
        .sftp()
        .cancel_transfer(SftpTransferCancelRequest {
            transfer_id: task.id.clone(),
            view_scope: None,
        })
        .expect("request cancellation during pending commit");
    let failed = wait_for_terminal(&state, &task.id, Duration::from_secs(4)).await;
    assert!(
        cancel_started.elapsed() <= Duration::from_secs(3),
        "uncertain commit cancellation must respect its bounded wait"
    );
    assert_eq!(failed.status, SftpTransferStatus::Failed);
    assert_eq!(
        failed.failure_kind,
        Some(SftpTransferFailureKind::CommitUnknown)
    );
    assert!(failed.cancel_requested);
    assert!(
        !failed.retryable,
        "pending commit with missing ACK cannot be retried blindly"
    );
    let error = failed.error.as_deref().expect("commit recovery detail");
    assert!(error.contains("artifact.bin"), "{error}");
    assert!(error.contains(".kerminal-backup"), "{error}");
    assert!(error.contains(".kerminal-part"), "{error}");

    assert!(
        !server_root.path().join("artifact.bin").exists(),
        "the pending commit did not receive an ACK, so the partial must not be assumed final"
    );
    assert_eq!(
        fs::read(&backup_path).await.expect("read retained backup"),
        b"original-content"
    );
    let partial = server_root.path().join("artifact.bin.kerminal-part");
    assert_eq!(sha256_file(&source).await, sha256_file(&partial).await);

    let retry = state.sftp().retry_transfer(
        state.paths(),
        SftpTransferRetryRequest {
            transfer_id: failed.id.clone(),
            view_scope: None,
        },
    );
    assert!(
        retry.is_err(),
        "unknown commit must reject blind retry: {retry:?}"
    );
}
