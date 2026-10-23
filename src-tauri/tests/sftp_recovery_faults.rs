//! SFTP 卡顿、取消、断点和自动恢复的真实 loopback 集成测试。
//!
//! 测试通过真实 russh SSH 握手和 bssh-russh-sftp subsystem 注入故障；只使用随机端口、
//! 临时 host key、临时配置和临时文件，不把 fake backend 当作生产网络证据。慢速 watchdog
//! 场景显式标记为 ignored，普通验证不会因为等待 30--60 秒而拖慢日常 Rust 测试。
//!
//! @author kongweiguang

use kerminal_lib::{
    models::sftp::{
        SftpManagedTransferRequest, SftpTransferCancelRequest, SftpTransferConflictPolicy,
        SftpTransferDirection, SftpTransferFailureKind, SftpTransferKind, SftpTransferRetryRequest,
        SftpTransferStatus, SftpTransferSummary, SftpTrustHostKeyRequest,
    },
    state::AppState,
};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    time::{Duration, Instant},
};
use tempfile::tempdir;
use tokio::{fs, time::sleep};

#[path = "support/sftp_recovery_faults/mod.rs"]
mod support;

use support::{
    create_loopback_host,
    server::{start_fault_server, FaultMode, FaultSftpServer},
    test_state,
};

/// 上传任务的公共请求构造器固定为 overwrite，避免测试把冲突策略分支混入取消断言。
fn upload_request(
    host_id: &str,
    local_path: &Path,
    remote_path: &str,
) -> SftpManagedTransferRequest {
    SftpManagedTransferRequest {
        host_id: host_id.to_owned(),
        remote_path: remote_path.to_owned(),
        local_path: local_path.to_string_lossy().into_owned(),
        direction: SftpTransferDirection::Upload,
        kind: SftpTransferKind::File,
        conflict_policy: SftpTransferConflictPolicy::Overwrite,
        view_scope: None,
        idle_timeout_seconds: Some(30),
    }
}

/// 下载任务的公共请求构造器让读确认故障复用同一队列和状态机路径。
fn download_request(
    host_id: &str,
    local_path: &Path,
    remote_path: &str,
) -> SftpManagedTransferRequest {
    SftpManagedTransferRequest {
        host_id: host_id.to_owned(),
        remote_path: remote_path.to_owned(),
        local_path: local_path.to_string_lossy().into_owned(),
        direction: SftpTransferDirection::Download,
        kind: SftpTransferKind::File,
        conflict_policy: SftpTransferConflictPolicy::Overwrite,
        view_scope: None,
        idle_timeout_seconds: Some(30),
    }
}

/// 为大文件场景生成可重复内容，哈希断言可以证明恢复没有重复或跳过字节。
async fn write_pattern(path: &Path, size: usize) {
    let payload = (0..size)
        .map(|index| (index as u8).wrapping_mul(31).wrapping_add(7))
        .collect::<Vec<_>>();
    fs::write(path, payload)
        .await
        .expect("write deterministic transfer fixture");
}

/// 使用同一 SHA-256 证明远端最终目标与本地源逐字节一致。
async fn sha256_file(path: &Path) -> [u8; 32] {
    let bytes = fs::read(path)
        .await
        .unwrap_or_else(|error| panic!("read hash fixture {}: {error}", path.display()));
    Sha256::digest(bytes).into()
}

/// 等待服务端真正收到故障请求，避免取消断言只测到排队阶段。
async fn wait_for_server_requests(
    server: &FaultSftpServer,
    writes: bool,
    minimum: usize,
    timeout: Duration,
) {
    let started = Instant::now();
    loop {
        let observed = if writes {
            server.write_requests()
        } else {
            server.read_requests()
        };
        if observed >= minimum {
            return;
        }
        assert!(
            started.elapsed() < timeout,
            "fault server did not receive {} request(s), observed {observed}",
            minimum
        );
        sleep(Duration::from_millis(20)).await;
    }
}

/// 轮询单个任务的终态；查询走真实 registry，不依赖窗口事件或 UI。
async fn wait_for_terminal(
    state: &AppState,
    transfer_id: &str,
    timeout: Duration,
) -> SftpTransferSummary {
    let started = Instant::now();
    loop {
        let task = state
            .sftp()
            .list_transfers()
            .expect("list transfer summaries")
            .into_iter()
            .find(|summary| summary.id == transfer_id);
        if let Some(task) = task {
            if matches!(
                task.status,
                SftpTransferStatus::Succeeded
                    | SftpTransferStatus::Failed
                    | SftpTransferStatus::Canceled
            ) {
                return task;
            }
        }
        assert!(
            started.elapsed() < timeout,
            "transfer {transfer_id} did not reach a terminal state"
        );
        sleep(Duration::from_millis(25)).await;
    }
}

/// 等待任务进入 running，确保槽位和底层通道已经被占用后再验证队列行为。
async fn wait_for_running(state: &AppState, transfer_id: &str, timeout: Duration) {
    let started = Instant::now();
    loop {
        let summary = state
            .sftp()
            .list_transfers()
            .expect("list running transfer")
            .into_iter()
            .find(|summary| summary.id == transfer_id);
        if summary.is_some_and(|summary| summary.status == SftpTransferStatus::Running) {
            return;
        }
        assert!(
            started.elapsed() < timeout,
            "transfer {transfer_id} did not start running"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

/// 恢复窗口只有两秒，按阶段等待可证明取消命中重排队间隙而非第一次传输。
async fn wait_for_phase(state: &AppState, transfer_id: &str, phase: &str, timeout: Duration) {
    let started = Instant::now();
    loop {
        let found = state
            .sftp()
            .list_transfers()
            .expect("list transfer phase")
            .into_iter()
            .any(|summary| summary.id == transfer_id && summary.phase.as_deref() == Some(phase));
        if found {
            return;
        }
        assert!(
            started.elapsed() < timeout,
            "transfer {transfer_id} did not reach phase {phase}"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

/// 取消单个真实网络任务并确认两秒内释放到 canceled 终态。
async fn cancel_within_two_seconds(state: &AppState, transfer_id: &str) -> SftpTransferSummary {
    let started = Instant::now();
    state
        .sftp()
        .cancel_transfer(SftpTransferCancelRequest {
            transfer_id: transfer_id.to_owned(),
            view_scope: None,
        })
        .expect("request transfer cancellation");
    let canceled = wait_for_terminal(state, transfer_id, Duration::from_secs(2)).await;
    assert_eq!(canceled.status, SftpTransferStatus::Canceled);
    assert!(
        started.elapsed() <= Duration::from_secs(2),
        "cancellation should release a stalled SFTP task within two seconds"
    );
    canceled
}

/// 显式信任本测试随机 host key，连接边界仍走生产 SftpService。
async fn trust_loopback_host(state: &AppState, host_id: &str) {
    state
        .sftp()
        .trust_host_key(
            state.paths(),
            SftpTrustHostKeyRequest {
                host_id: host_id.to_owned(),
            },
        )
        .await
        .expect("trust temporary loopback host key");
}

/// 永久不回 write ACK 时，取消必须中断底层 future、保留非空 partial 且不产生正式文件。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_write_cancel_is_fast_and_preserves_partial_boundary() {
    let server_root = tempdir().expect("create fault server root");
    let client_root = tempdir().expect("create client fixture root");
    let source = client_root.path().join("source.bin");
    write_pattern(&source, 512 * 1024).await;

    let server = start_fault_server(
        server_root.path().to_path_buf(),
        FaultMode::NeverCompletesWrite,
    )
    .await;
    let (_home, state) = test_state();
    let host_id = create_loopback_host(&state, "stalled-write", server.addr.port());
    trust_loopback_host(&state, &host_id).await;
    let summary = state
        .sftp()
        .enqueue_transfer(
            state.paths(),
            upload_request(&host_id, &source, "/artifact.bin"),
        )
        .expect("enqueue stalled upload");

    wait_for_running(&state, &summary.id, Duration::from_secs(2)).await;
    wait_for_server_requests(&server, true, 1, Duration::from_secs(2)).await;
    let canceled = cancel_within_two_seconds(&state, &summary.id).await;
    assert!(canceled.cancel_requested);
    assert!(!server_root.path().join("artifact.bin").exists());
    let partial = server_root.path().join("artifact.bin.kerminal-part");
    assert!(
        fs::metadata(&partial)
            .await
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(false),
        "a stalled write that reached the server must leave non-empty partial bytes"
    );
}

/// 两个卡住任务占满单主机槽位后，取消其中一个必须让第三个任务获得槽位并成功完成。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_releases_host_slot_and_unblocks_next_queued_transfer() {
    let server_root = tempdir().expect("create fault server root");
    let client_root = tempdir().expect("create client fixture root");
    let first_source = client_root.path().join("first.bin");
    let second_source = client_root.path().join("second.bin");
    let third_source = client_root.path().join("third.bin");
    write_pattern(&first_source, 256 * 1024).await;
    write_pattern(&second_source, 256 * 1024).await;
    write_pattern(&third_source, 256 * 1024).await;

    let server = start_fault_server(
        server_root.path().to_path_buf(),
        FaultMode::NeverCompletesWrite,
    )
    .await;
    let (_home, state) = test_state();
    let host_id = create_loopback_host(&state, "slot-release", server.addr.port());
    trust_loopback_host(&state, &host_id).await;
    let first = state
        .sftp()
        .enqueue_transfer(
            state.paths(),
            upload_request(&host_id, &first_source, "/first.bin"),
        )
        .expect("enqueue first stalled upload");
    let second = state
        .sftp()
        .enqueue_transfer(
            state.paths(),
            upload_request(&host_id, &second_source, "/second.bin"),
        )
        .expect("enqueue second stalled upload");
    wait_for_running(&state, &first.id, Duration::from_secs(2)).await;
    wait_for_running(&state, &second.id, Duration::from_secs(2)).await;
    wait_for_server_requests(&server, true, 2, Duration::from_secs(2)).await;

    server.set_mode(FaultMode::Normal);
    let third = state
        .sftp()
        .enqueue_transfer(
            state.paths(),
            upload_request(&host_id, &third_source, "/third.bin"),
        )
        .expect("enqueue third transfer behind occupied host slots");
    sleep(Duration::from_millis(150)).await;
    let queued = state
        .sftp()
        .list_transfers()
        .expect("list queued third transfer")
        .into_iter()
        .find(|summary| summary.id == third.id)
        .expect("third transfer summary");
    assert_eq!(queued.status, SftpTransferStatus::Queued);

    cancel_within_two_seconds(&state, &first.id).await;
    let completed_third = wait_for_terminal(&state, &third.id, Duration::from_secs(3)).await;
    assert_eq!(completed_third.status, SftpTransferStatus::Succeeded);
    assert_eq!(
        sha256_file(&third_source).await,
        sha256_file(&server_root.path().join("third.bin")).await
    );

    cancel_within_two_seconds(&state, &second.id).await;
}

/// 永久不回 read ACK 时，下载取消不能把 worker 永久留在占用槽位的等待中。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_read_cancel_is_fast_and_does_not_commit_final_file() {
    let server_root = tempdir().expect("create fault server root");
    let client_root = tempdir().expect("create client fixture root");
    fs::write(
        server_root.path().join("remote.bin"),
        vec![9_u8; 512 * 1024],
    )
    .await
    .expect("seed remote read fixture");
    let server = start_fault_server(
        server_root.path().to_path_buf(),
        FaultMode::NeverCompletesRead,
    )
    .await;
    let (_home, state) = test_state();
    let host_id = create_loopback_host(&state, "stalled-read", server.addr.port());
    trust_loopback_host(&state, &host_id).await;
    let target = client_root.path().join("download.bin");
    let summary = state
        .sftp()
        .enqueue_transfer(
            state.paths(),
            download_request(&host_id, &target, "/remote.bin"),
        )
        .expect("enqueue stalled download");
    wait_for_running(&state, &summary.id, Duration::from_secs(2)).await;
    wait_for_server_requests(&server, false, 1, Duration::from_secs(2)).await;
    cancel_within_two_seconds(&state, &summary.id).await;
    assert!(
        !target.exists(),
        "canceled download must not commit final path"
    );
}

/// 孤儿临时文件旁的新上传应固定独立临时路径；取消后重试不能误用孤儿或漂移正式目标。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_keeps_the_first_renamed_target_and_final_hash() {
    let server_root = tempdir().expect("create fault server root");
    let client_root = tempdir().expect("create client fixture root");
    let source = client_root.path().join("source.bin");
    write_pattern(&source, 512 * 1024).await;
    fs::write(server_root.path().join("artifact.bin"), b"pre-existing")
        .await
        .expect("seed rename conflict target");
    let orphan = server_root.path().join("artifact (1).bin.kerminal-part");
    fs::write(&orphan, b"untrusted orphan")
        .await
        .expect("seed orphan beside renamed target");

    let server = start_fault_server(
        server_root.path().to_path_buf(),
        FaultMode::NeverCompletesWrite,
    )
    .await;
    let (_home, state) = test_state();
    let host_id = create_loopback_host(&state, "stable-rename", server.addr.port());
    trust_loopback_host(&state, &host_id).await;
    let mut request = upload_request(&host_id, &source, "/artifact.bin");
    request.conflict_policy = SftpTransferConflictPolicy::Rename;
    let first = state
        .sftp()
        .enqueue_transfer(state.paths(), request)
        .expect("enqueue rename upload");
    wait_for_running(&state, &first.id, Duration::from_secs(2)).await;
    wait_for_server_requests(&server, true, 1, Duration::from_secs(2)).await;
    cancel_within_two_seconds(&state, &first.id).await;

    server.set_mode(FaultMode::Normal);
    let successor = state
        .sftp()
        .retry_transfer(
            state.paths(),
            SftpTransferRetryRequest {
                transfer_id: first.id.clone(),
                view_scope: None,
            },
        )
        .expect("retry canceled rename upload from retained partial");
    let duplicate = state
        .sftp()
        .retry_transfer(
            state.paths(),
            SftpTransferRetryRequest {
                transfer_id: first.id.clone(),
                view_scope: None,
            },
        )
        .expect("duplicate retry must return original successor");
    assert_eq!(
        duplicate.id, successor.id,
        "repeated retry cannot create a second writer"
    );
    assert_ne!(
        successor.id, first.id,
        "manual retry creates a successor id"
    );
    let completed = wait_for_terminal(&state, &successor.id, Duration::from_secs(5)).await;
    assert_eq!(completed.status, SftpTransferStatus::Succeeded);
    assert_eq!(
        sha256_file(&source).await,
        sha256_file(&server_root.path().join("artifact (1).bin")).await
    );
    assert_eq!(
        fs::read(server_root.path().join("artifact.bin"))
            .await
            .expect("read original rename target"),
        b"pre-existing"
    );
    assert!(
        !server_root.path().join("artifact (2).bin").exists(),
        "retry must reuse the chosen rename target"
    );
    assert_eq!(
        fs::read(&orphan).await.expect("preserved orphan"),
        b"untrusted orphan"
    );
    let mut entries = fs::read_dir(server_root.path()).await.expect("directory");
    let mut count = 0;
    while entries.next_entry().await.expect("entry").is_some() {
        count += 1;
    }
    assert_eq!(
        count, 3,
        "original, committed renamed file and orphan only; owned partial was committed"
    );
}

/// 覆盖提交的 fallback、回滚、未知回执和取消边界集成测试。
#[path = "sftp_recovery_faults/remote_commit.rs"]
mod remote_commit;

/// 一个 bulk channel 被取消时，另一个独立 SSH/SFTP 连接仍应把同主机文件提交完整。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_one_bulk_connection_keeps_peer_transfer_alive() {
    let server_root = tempdir().expect("create fault server root");
    let client_root = tempdir().expect("create client fixture root");
    let first_source = client_root.path().join("cancelled.bin");
    let second_source = client_root.path().join("survivor.bin");
    write_pattern(&first_source, 4 * 1024 * 1024).await;
    write_pattern(&second_source, 4 * 1024 * 1024).await;
    let server = start_fault_server(server_root.path().to_path_buf(), FaultMode::DelayedAck).await;
    let (_home, state) = test_state();
    let host_id = create_loopback_host(&state, "independent-bulk", server.addr.port());
    trust_loopback_host(&state, &host_id).await;
    let first = state
        .sftp()
        .enqueue_transfer(
            state.paths(),
            upload_request(&host_id, &first_source, "/cancelled.bin"),
        )
        .expect("enqueue first bulk upload");
    let second = state
        .sftp()
        .enqueue_transfer(
            state.paths(),
            upload_request(&host_id, &second_source, "/survivor.bin"),
        )
        .expect("enqueue second bulk upload");
    wait_for_server_requests(&server, true, 2, Duration::from_secs(3)).await;
    cancel_within_two_seconds(&state, &first.id).await;
    let completed = wait_for_terminal(&state, &second.id, Duration::from_secs(12)).await;
    assert_eq!(completed.status, SftpTransferStatus::Succeeded);
    assert_eq!(
        sha256_file(&second_source).await,
        sha256_file(&server_root.path().join("survivor.bin")).await
    );
}

/// 第一次无进度超时应自动用同一任务 ID 恢复一次并最终成功。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "真实 idle watchdog 场景需要等待至少 30 秒"]
async fn idle_timeout_recovers_once_with_the_same_transfer_id() {
    let server_root = tempdir().expect("create fault server root");
    let client_root = tempdir().expect("create client fixture root");
    let source = client_root.path().join("recover-once.bin");
    write_pattern(&source, 512 * 1024).await;
    let server =
        start_fault_server(server_root.path().to_path_buf(), FaultMode::StallOnceWrite).await;
    let (_home, state) = test_state();
    let host_id = create_loopback_host(&state, "recover-once", server.addr.port());
    trust_loopback_host(&state, &host_id).await;
    let first = state
        .sftp()
        .enqueue_transfer(
            state.paths(),
            upload_request(&host_id, &source, "/recover-once.bin"),
        )
        .expect("enqueue auto-recovery upload");
    wait_for_server_requests(&server, true, 1, Duration::from_secs(2)).await;
    let completed = wait_for_terminal(&state, &first.id, Duration::from_secs(45)).await;
    assert_eq!(completed.id, first.id);
    assert_eq!(completed.status, SftpTransferStatus::Succeeded);
    assert_eq!(completed.recovery_attempt, 1);
    assert_eq!(
        sha256_file(&source).await,
        sha256_file(&server_root.path().join("recover-once.bin")).await
    );
}

/// 自动恢复的两秒等待不占槽，取消在这个间隙必须让原 ID 终结且不再发起第二次写入。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "真实恢复等待取消需要先经过 30 秒 idle 阈值"]
async fn cancellation_during_recovery_wait_stops_requeue() {
    let server_root = tempdir().expect("create fault server root");
    let client_root = tempdir().expect("create client fixture root");
    let source = client_root.path().join("cancel-recovery.bin");
    write_pattern(&source, 512 * 1024).await;
    let server =
        start_fault_server(server_root.path().to_path_buf(), FaultMode::StallOnceWrite).await;
    let (_home, state) = test_state();
    let host_id = create_loopback_host(&state, "cancel-recovery", server.addr.port());
    trust_loopback_host(&state, &host_id).await;
    let summary = state
        .sftp()
        .enqueue_transfer(
            state.paths(),
            upload_request(&host_id, &source, "/cancel-recovery.bin"),
        )
        .expect("enqueue recovery cancellation probe");
    wait_for_server_requests(&server, true, 1, Duration::from_secs(2)).await;
    wait_for_phase(&state, &summary.id, "recovering", Duration::from_secs(35)).await;
    let canceled = cancel_within_two_seconds(&state, &summary.id).await;
    assert_eq!(canceled.recovery_attempt, 1);
    sleep(Duration::from_secs(3)).await;
    assert_eq!(
        server.write_requests(),
        1,
        "canceled recovery may not requeue a second write"
    );
    assert!(!server_root.path().join("cancel-recovery.bin").exists());
}

/// 第二次无进度超时必须形成可识别失败并保留可重试 partial，而不是永久占槽。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "真实 idle watchdog 场景需要等待两次 30 秒阈值"]
async fn second_idle_timeout_is_failed_and_retryable() {
    let server_root = tempdir().expect("create fault server root");
    let client_root = tempdir().expect("create client fixture root");
    let source = client_root.path().join("recover-fail.bin");
    write_pattern(&source, 512 * 1024).await;
    let server = start_fault_server(
        server_root.path().to_path_buf(),
        FaultMode::StallEveryConnection,
    )
    .await;
    let (_home, state) = test_state();
    let host_id = create_loopback_host(&state, "recover-fail", server.addr.port());
    trust_loopback_host(&state, &host_id).await;
    let first = state
        .sftp()
        .enqueue_transfer(
            state.paths(),
            upload_request(&host_id, &source, "/recover-fail.bin"),
        )
        .expect("enqueue repeated-stall upload");
    let failed = wait_for_terminal(&state, &first.id, Duration::from_secs(70)).await;
    assert_eq!(failed.status, SftpTransferStatus::Failed);
    assert_eq!(
        failed.failure_kind,
        Some(SftpTransferFailureKind::IdleTimeout)
    );
    assert_eq!(failed.recovery_attempt, 1);
    assert!(failed.retryable);
    assert!(
        !failed.resumable,
        "zero acknowledged bytes require a safe restart, not a resume claim"
    );
    assert!(server_root
        .path()
        .join("recover-fail.bin.kerminal-part")
        .exists());
}

/// 慢速但持续确认的传输累计超过 60 秒也必须成功，证明没有总时长 watchdog。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "真实慢速传输需要约 65 秒，用于发布前 Windows 验收"]
async fn continuous_progress_can_run_past_sixty_seconds() {
    let server_root = tempdir().expect("create fault server root");
    let client_root = tempdir().expect("create client fixture root");
    let source = client_root.path().join("long.bin");
    write_pattern(&source, 65 * 1024 * 1024).await;
    let server = start_fault_server(server_root.path().to_path_buf(), FaultMode::DelayedAck).await;
    let (_home, state) = test_state();
    let host_id = create_loopback_host(&state, "long-progress", server.addr.port());
    trust_loopback_host(&state, &host_id).await;
    let first = state
        .sftp()
        .enqueue_transfer(
            state.paths(),
            upload_request(&host_id, &source, "/long.bin"),
        )
        .expect("enqueue slow progress upload");
    let started = Instant::now();
    let completed = wait_for_terminal(&state, &first.id, Duration::from_secs(90)).await;
    assert_eq!(completed.status, SftpTransferStatus::Succeeded);
    assert!(
        started.elapsed() >= Duration::from_secs(60),
        "the observed transfer must actually exceed the former 60-second call limit"
    );
    assert_eq!(completed.failure_kind, None);
    assert_eq!(
        sha256_file(&source).await,
        sha256_file(&server_root.path().join("long.bin")).await
    );
}
