//! 更新恢复存储的跨进程状态回归；所有文件只写入独立临时目录。
//!
//! @author kongweiguang

use std::{error::Error, fs, io, path::Path};

use kerminal_application::AppResult;
use kerminal_application::updates::{UpdateRecoveryId, UpdateRecoveryState};
use sha2::{Digest, Sha256};
use tempfile::tempdir;

use super::RecoveryStore;

const MAX_ARTIFACT_BYTES: u64 = 1024 * 1024;
const VERIFIED_ARTIFACT: &[u8] = b"verified package bytes";

/// 失败时输出有限深度的 source 链与 I/O 分类，定位 Windows OS 错误且不打印路径或包正文。
fn expect_recovery<T>(result: AppResult<T>, operation: &str) -> T {
    result.unwrap_or_else(|error| {
        panic!("{operation}: {}", bounded_error_chain(&error));
    })
}

/// 保留错误链定位信息但仅对 I/O 原因打印 kind/raw code，避免测试日志泄漏动态数据。
fn bounded_error_chain(error: &(dyn Error + 'static)) -> String {
    const MAX_CHAIN_DEPTH: usize = 8;
    const MAX_DETAIL_CHARS: usize = 160;

    let mut entries = Vec::new();
    let mut current = Some(error);
    for depth in 0..MAX_CHAIN_DEPTH {
        let Some(cause) = current else {
            break;
        };
        let detail = if let Some(io_error) = cause.downcast_ref::<io::Error>() {
            format!(
                "io(kind={:?}, raw_os_error={:?})",
                io_error.kind(),
                io_error.raw_os_error()
            )
        } else {
            cause.to_string()
        };
        let bounded = detail.chars().take(MAX_DETAIL_CHARS).collect::<String>();
        entries.push(format!("{depth}:{bounded}"));
        current = cause.source();
    }
    if current.is_some() {
        entries.push("source chain truncated".to_owned());
    }
    entries.join(" -> ")
}

/// 以临时根创建恢复仓库，避免测试读取真实用户配置或安装路径。
fn store_at(root: &Path) -> RecoveryStore {
    expect_recovery(
        RecoveryStore::new(root.to_path_buf(), MAX_ARTIFACT_BYTES),
        "create isolated updater recovery store",
    )
}

/// 使用与生产恢复收据相同的小写编码计算摘要，固定 fixture 的预期值。
fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
/// 安装前提交以目录重命名原子发布，收据和已验签 artifact 总是成对可见。
fn pending_artifact_and_receipt_are_committed_together() {
    let root = tempdir().expect("create temporary recovery root");
    let store = store_at(root.path());
    let id = UpdateRecoveryId::new();

    let record_dir = expect_recovery(
        store.persist_pending_bytes(id, "0.4.2", "windows-x86_64", VERIFIED_ARTIFACT),
        "persist verified package and receipt",
    );
    let visible = expect_recovery(
        store.list("0.4.1", "windows-x86_64", None),
        "list pending recovery",
    );

    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].id, id);
    assert_eq!(visible[0].state, UpdateRecoveryState::PendingConfirmation);
    assert_eq!(
        visible[0].artifact_sha256,
        digest(b"verified package bytes")
    );
    assert!(record_dir.join("verified-update.bin").is_file());
    assert!(record_dir.join("receipt.pending.json").is_file());
    assert_eq!(
        fs::read_dir(root.path())
            .expect("read recovery root")
            .count(),
        1
    );
}

#[test]
/// 生产下载路径从验签内存字节直接提交，避免存在无 durable owner 的中间更新包。
fn verified_bytes_commit_as_a_readable_recovery_record() {
    let root = tempdir().expect("create temporary recovery root");
    let store = store_at(root.path());
    let id = UpdateRecoveryId::new();
    let bytes = b"signature-verified bytes";

    let directory = expect_recovery(
        store.persist_pending_bytes(id, "0.4.2", "windows-x86_64", bytes),
        "persist verified bytes",
    );
    let recovered = expect_recovery(store.read_verified_artifact(id), "verify artifact receipt");

    assert_eq!(recovered, bytes);
    assert_eq!(
        expect_recovery(
            store.list("0.4.1", "windows-x86_64", None),
            "list durable record",
        )[0]
        .artifact_sha256,
        digest(bytes)
    );
    assert!(directory.join("verified-update.bin").is_file());
}

#[test]
/// 安装前回读若发现包内容被替换，必须拒绝把它交给 SDK。
fn modified_verified_artifact_is_rejected_before_install() {
    let root = tempdir().expect("create temporary recovery root");
    let store = store_at(root.path());
    let id = UpdateRecoveryId::new();
    let directory = expect_recovery(
        store.persist_pending_bytes(id, "0.4.2", "windows-x86_64", b"original bytes"),
        "persist verified bytes",
    );
    fs::write(directory.join("verified-update.bin"), b"modified bytes")
        .expect("replace recovery artifact fixture");

    assert!(store.read_verified_artifact(id).is_err());
    assert!(directory.join("verified-update.bin").is_file());
}

#[test]
/// Windows 版本和 SDK target 命中仍只提示人工确认，artifact 摘要不能冒充 exe 摘要。
fn installing_receipt_never_claims_windows_install_from_archive_digest() {
    let root = tempdir().expect("create temporary recovery root");
    let store = store_at(root.path());
    let id = UpdateRecoveryId::new();
    let record_dir = expect_recovery(
        store.persist_pending_bytes(id, "0.4.2", "windows-x86_64", VERIFIED_ARTIFACT),
        "persist pending recovery",
    );
    expect_recovery(store.mark_installing(id), "persist install boundary");

    let visible = expect_recovery(
        store.list("0.4.2", "windows-x86_64", None),
        "list version-matched recovery",
    );

    assert_eq!(visible.len(), 1);
    assert_eq!(
        visible[0].state,
        UpdateRecoveryState::VersionMatchedAwaitingConfirmation
    );
    assert!(record_dir.join("verified-update.bin").is_file());
    assert!(record_dir.join("receipt.installing.json").is_file());
}

#[test]
/// 已证明无副作用的失败必须恢复成可持久读取、可再次进入 Installing 的 Pending 阶段。
fn failed_no_effect_restores_pending_receipt_for_safe_retry() {
    let root = tempdir().expect("create temporary recovery root");
    let store = store_at(root.path());
    let id = UpdateRecoveryId::new();
    let record_dir = expect_recovery(
        store.persist_pending_bytes(id, "0.4.2", "windows-x86_64", VERIFIED_ARTIFACT),
        "persist pending recovery",
    );

    fs::write(
        record_dir.join("receipt.next.tmp"),
        b"interrupted transition",
    )
    .expect("seed stale legacy transition temp");
    expect_recovery(store.mark_installing(id), "persist install boundary");
    expect_recovery(store.restore_pending(id), "restore safe retry receipt");
    let visible = expect_recovery(
        store.list("0.4.1", "windows-x86_64", None),
        "list pending recovery",
    );

    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].state, UpdateRecoveryState::PendingConfirmation);
    assert!(record_dir.join("receipt.pending.json").is_file());
    assert!(!record_dir.join("receipt.installing.json").exists());
    expect_recovery(
        store.mark_installing(id),
        "persist second safe install attempt",
    );
    assert!(record_dir.join("receipt.installing.json").is_file());
}

#[test]
/// 新进程版本或 target 不匹配时保留 Unknown 材料，不因 SDK 调用曾开始而自动清理。
fn unknown_install_material_survives_restart_listing() {
    let root = tempdir().expect("create temporary recovery root");
    let store = store_at(root.path());
    let id = UpdateRecoveryId::new();
    let record_dir = expect_recovery(
        store.persist_pending_bytes(id, "0.4.2", "windows-x86_64", VERIFIED_ARTIFACT),
        "persist pending recovery",
    );
    expect_recovery(store.mark_installing(id), "persist install boundary");

    let visible = expect_recovery(
        store.list("0.4.1", "windows-x86_64", None),
        "list unresolved recovery",
    );

    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].state, UpdateRecoveryState::OutcomeUnknown);
    assert!(record_dir.join("verified-update.bin").is_file());
    assert!(record_dir.exists());
}

#[test]
/// 目标摘要由 SDK 成功返回后的目标文件实测值提供，吻合时才自动确认并清除收据。
fn matching_returned_target_digest_confirms_and_removes_record() {
    let root = tempdir().expect("create temporary recovery root");
    let store = store_at(root.path());
    let id = UpdateRecoveryId::new();
    let record_dir = expect_recovery(
        store.persist_pending_bytes(id, "0.4.2", "macos-aarch64", VERIFIED_ARTIFACT),
        "persist pending recovery",
    );
    expect_recovery(store.mark_installing(id), "persist install boundary");
    expect_recovery(
        store.mark_awaiting_confirmation(id, Some(digest(b"installed executable bytes"))),
        "persist SDK returned target digest",
    );

    assert!(record_dir.join("verified-update.bin").is_file());
    assert_eq!(
        expect_recovery(
            store.list(
                "0.4.2",
                "macos-aarch64",
                Some(&digest(b"installed executable bytes")),
            ),
            "reconcile installed target",
        ),
        Vec::new()
    );
    assert!(!record_dir.exists());
}

#[test]
/// 已返回但目标摘要不匹配时只保留 Unknown，不把包摘要当成目标文件摘要。
fn mismatched_returned_target_digest_stays_unknown() {
    let root = tempdir().expect("create temporary recovery root");
    let store = store_at(root.path());
    let id = UpdateRecoveryId::new();
    let record_dir = expect_recovery(
        store.persist_pending_bytes(id, "0.4.2", "linux-x86_64", VERIFIED_ARTIFACT),
        "persist pending recovery",
    );
    expect_recovery(store.mark_installing(id), "persist install boundary");
    expect_recovery(
        store.mark_awaiting_confirmation(id, Some(digest(b"new AppImage bytes"))),
        "persist target digest",
    );

    let visible = expect_recovery(
        store.list("0.4.2", "linux-x86_64", Some(&digest(b"different file"))),
        "list target mismatch",
    );

    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].state, UpdateRecoveryState::OutcomeUnknown);
    assert!(
        record_dir
            .join("receipt.awaiting-confirmation.json")
            .is_file()
    );
}

#[test]
/// 用户显式确认才删除未知记录，确认路径不会启动任何安装逻辑。
fn explicit_acknowledgement_removes_only_the_selected_record() {
    let root = tempdir().expect("create temporary recovery root");
    let store = store_at(root.path());
    let id = UpdateRecoveryId::new();
    let record_dir = expect_recovery(
        store.persist_pending_bytes(id, "0.4.2", "windows-x86_64", VERIFIED_ARTIFACT),
        "persist pending recovery",
    );
    expect_recovery(store.mark_installing(id), "persist install boundary");

    expect_recovery(store.remove(id), "explicit user acknowledgement cleanup");

    assert!(!record_dir.exists());
    assert!(
        expect_recovery(
            store.list("0.4.2", "windows-x86_64", None),
            "refresh recovery list",
        )
        .is_empty()
    );
}

#[test]
/// 收据字段损坏时保留原目录并拒绝猜测状态或自动删除安装包。
fn corrupt_receipt_fails_closed_without_removing_artifact() {
    let root = tempdir().expect("create temporary recovery root");
    let store = store_at(root.path());
    let id = UpdateRecoveryId::new();
    let record_dir = expect_recovery(
        store.persist_pending_bytes(id, "0.4.2", "windows-x86_64", VERIFIED_ARTIFACT),
        "persist pending recovery",
    );
    fs::write(record_dir.join("receipt.pending.json"), b"{broken").expect("corrupt test receipt");

    assert!(store.list("0.4.2", "windows-x86_64", None).is_err());
    assert!(record_dir.join("verified-update.bin").is_file());
    assert!(record_dir.exists());
}

#[test]
/// 超限安装包不会产生可见恢复目录，避免将不完整数据冒充已验签候选。
fn oversized_artifact_is_not_committed() {
    let root = tempdir().expect("create temporary recovery root");
    let store = store_at(root.path());

    assert!(
        store
            .persist_pending_bytes(
                UpdateRecoveryId::new(),
                "0.4.2",
                "windows-x86_64",
                &vec![b'x'; MAX_ARTIFACT_BYTES as usize + 1],
            )
            .is_err()
    );
    assert_eq!(
        fs::read_dir(root.path())
            .expect("read recovery root")
            .count(),
        0
    );
}
