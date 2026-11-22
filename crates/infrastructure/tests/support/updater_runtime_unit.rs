//! updater 操作门禁排队取消回归。
//!
//! @author kongweiguang

use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::{CargoPackagerUpdater, lock_or_cancel};
use kerminal_application::updates::{
    UpdateAvailability, UpdateCandidateId, UpdatePort, UpdateProgress, UpdateRecoveryId,
    UpdateRecoveryState,
};
use tokio::sync::watch;

#[cfg(target_os = "linux")]
use super::{AppImageSnapshot, appimage_snapshot, reconcile_appimage_install};

/// 排队的检查或下载取消后立即退出等待，不抢占前一个操作持有的门禁。
#[tokio::test]
async fn queued_operation_cancellation_does_not_wait_for_gate_owner() {
    let gate = Mutex::new(());
    let held = gate.lock().await;
    let cancellation = CancellationToken::new();
    let cancel_waiter = cancellation.clone();
    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel_waiter.cancel();
    });
    let started = Instant::now();

    let error = tokio::time::timeout(
        Duration::from_millis(200),
        lock_or_cancel(&gate, &cancellation, "更新操作已取消"),
    )
    .await
    .expect("queued cancellation should finish promptly")
    .expect_err("a canceled waiter must not acquire the gate");

    cancel_task.await.expect("join cancellation timer");
    drop(held);
    assert!(started.elapsed() < Duration::from_millis(200));
    assert_eq!(error.public_message(), "更新操作已取消");
}

/// 缺少 feed 时检查和下载明确返回不可用，但恢复记录仍可读取并由用户确认清理。
#[tokio::test]
async fn missing_update_source_keeps_recovery_available() {
    let root = tempfile::tempdir().expect("create isolated updater recovery root");
    let adapter = CargoPackagerUpdater::new(None, root.path().join("updates/recovery"))
        .expect("construct updater recovery adapter without feed settings");
    let recovery_id = UpdateRecoveryId::new();
    let target = adapter
        .current_target
        .as_deref()
        .unwrap_or("unknown-target");
    adapter
        .recovery
        .persist_pending_bytes(
            recovery_id,
            "0.4.2",
            target,
            b"isolated verified package fixture",
        )
        .expect("persist recovery fixture");

    assert!(matches!(
        adapter.availability(),
        UpdateAvailability::Unavailable { .. }
    ));
    assert!(matches!(
        adapter.check(CancellationToken::new()).await,
        Err(kerminal_application::AppError::Unsupported { .. })
    ));
    let (progress, _) = watch::channel(UpdateProgress::default());
    assert!(matches!(
        adapter
            .download(UpdateCandidateId::new(), CancellationToken::new(), progress,)
            .await,
        Err(kerminal_application::AppError::Unsupported { .. })
    ));

    let records = adapter
        .recoveries()
        .await
        .expect("read recovery without update source");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, recovery_id);
    assert_eq!(records[0].state, UpdateRecoveryState::PendingConfirmation);
    adapter
        .acknowledge_recovery(recovery_id)
        .await
        .expect("explicitly acknowledge and clear selected recovery");
    assert!(
        adapter
            .recoveries()
            .await
            .expect("read empty recovery store")
            .is_empty()
    );
}

/// 目标快照未改变时 SDK 错误可安全归类为无副作用，保留同一包供重试。
#[cfg(target_os = "linux")]
#[test]
fn appimage_sdk_error_with_identical_target_is_retryable() {
    use std::{fs, io};

    let root = tempfile::tempdir().expect("create isolated AppImage target");
    let target = root.path().join("candidate.AppImage");
    fs::write(&target, b"original executable").expect("seed target");
    let before = appimage_snapshot(&target).expect("capture original target");
    let result = reconcile_appimage_install::<io::Error>(
        Err(io::Error::other("simulated SDK failure")),
        &target,
        before,
    );

    assert!(matches!(
        result.result,
        kerminal_application::UpdateInstallResult::FailedNoEffect(_)
    ));
    assert_eq!(
        appimage_snapshot(&target).expect("read retained target"),
        before
    );
}

/// 成功的 AppImage SDK 调用只记录实际目标摘要，不把压缩下载包摘要当作安装文件摘要。
#[cfg(target_os = "linux")]
#[test]
fn appimage_success_records_replaced_target_digest() {
    use std::{fs, os::unix::fs::PermissionsExt};

    let root = tempfile::tempdir().expect("create isolated AppImage target");
    let target = root.path().join("candidate.AppImage");
    fs::write(&target, b"old appimage").expect("seed target");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
        .expect("make target executable");
    let before = appimage_snapshot(&target).expect("capture original target");
    fs::write(&target, b"new appimage bytes").expect("simulate SDK replacement");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
        .expect("preserve executable mode");
    let after = appimage_snapshot(&target).expect("capture updated target");

    let result = reconcile_appimage_install::<std::io::Error>(Ok(()), &target, before);

    assert!(matches!(
        result.result,
        kerminal_application::UpdateInstallResult::Installed(
            kerminal_application::UpdateInstallDisposition::RestartRequired
        )
    ));
    assert_eq!(
        result.installed_target_sha256,
        Some(super::hex_sha256(&after.sha256))
    );
}

/// SDK 已报错而目标文件发生变化时必须保留 Unknown，禁止按可重试失败处理。
#[cfg(target_os = "linux")]
#[test]
fn appimage_sdk_error_after_target_change_is_unknown() {
    use std::{fs, io};

    let root = tempfile::tempdir().expect("create isolated AppImage target");
    let target = root.path().join("candidate.AppImage");
    fs::write(&target, b"original executable").expect("seed target");
    let before: AppImageSnapshot = appimage_snapshot(&target).expect("capture original target");
    fs::write(&target, b"partially replaced target").expect("simulate partial SDK write");

    let result = reconcile_appimage_install::<io::Error>(
        Err(io::Error::other("simulated SDK failure")),
        &target,
        before,
    );

    assert!(matches!(
        result.result,
        kerminal_application::UpdateInstallResult::OutcomeUnknown(_)
    ));
}
