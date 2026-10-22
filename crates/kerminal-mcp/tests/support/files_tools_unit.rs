// @author kongweiguang

use std::path::PathBuf;

use kerminal_domain::{
    files::{
        ConflictPolicy, FileEndpoint, FileEntry, FileEntryKind, FilePreview, FileRevision,
        TransferCommitSummary, TransferFailureKind, TransferKind, TransferPhase, TransferRequest,
        TransferStatus, TransferSummary,
    },
    identity::{HostId, TransferId},
};
use serde_json::json;

use super::*;

/// 验证 canonical endpoint 中不含凭据字段且按明示冲突策略进入队列请求。
#[test]
fn canonical_transfer_request_preserves_policy_and_idle_budget() {
    let host_id = HostId::new();
    let args = json!({
        "source": { "type": "local", "path": "C:/upload/file.txt" },
        "destination": { "type": "remote", "hostId": host_id.as_str(), "path": "/upload/file.txt" },
        "kind": "file",
        "conflictPolicy": "rename",
        "idleTimeoutSeconds": 90
    })
    .as_object()
    .expect("object arguments")
    .clone();

    let request = transfer_request(args).expect("valid canonical transfer request");
    assert_eq!(request.kind, TransferKind::File);
    assert_eq!(request.conflict_policy, ConflictPolicy::Rename);
    assert_eq!(request.idle_timeout_seconds, Some(90));
    assert!(matches!(request.source, FileEndpoint::Local { .. }));
    assert!(matches!(
        &request.destination,
        FileEndpoint::Remote { host_id: actual, path }
            if actual == &host_id && path.as_str() == "/upload/file.txt"
    ));

    let snake_case_host_id = json!({
        "source": { "type": "local", "path": "C:/upload/file.txt" },
        "destination": { "type": "remote", "host_id": host_id.as_str(), "path": "/upload/file.txt" },
        "kind": "file",
        "conflictPolicy": "rename"
    })
    .as_object()
    .expect("object arguments")
    .clone();
    assert!(transfer_request(snake_case_host_id).is_err());
}

/// 兼容旧 flat 上传参数，并保证路径方向不会在 MCP adapter 中翻转。
#[test]
fn legacy_transfer_request_maps_upload_direction() {
    let host_id = HostId::new();
    let args = json!({
        "hostId": host_id.as_str(),
        "localPath": "C:/upload/file.txt",
        "remotePath": "/upload/file.txt",
        "direction": "upload",
        "kind": "file",
        "conflictPolicy": "overwrite"
    })
    .as_object()
    .expect("object arguments")
    .clone();

    let request = transfer_request(args).expect("valid legacy upload request");
    assert!(matches!(request.source, FileEndpoint::Local { .. }));
    assert!(matches!(request.destination, FileEndpoint::Remote { .. }));
    assert_eq!(request.conflict_policy, ConflictPolicy::Overwrite);
}

/// 锁定旧版 transfer 响应中的 camelCase endpoint 字段，防止 Rust 字段名泄漏到 wire 契约。
///
/// @author kongweiguang
#[test]
fn transfer_projection_preserves_canonical_endpoint_and_result_fields() {
    let host_id = HostId::new();
    let mut summary = TransferSummary {
        id: TransferId::new(),
        request: TransferRequest {
            source: FileEndpoint::remote(host_id.clone(), "/download/report.txt")
                .expect("valid remote source"),
            destination: FileEndpoint::Local {
                path: PathBuf::from("C:/download/report.txt"),
            },
            kind: TransferKind::File,
            conflict_policy: ConflictPolicy::Overwrite,
            idle_timeout_seconds: Some(90),
        },
        status: TransferStatus::Transferring,
        phase: Some(TransferPhase::Verifying),
        bytes_transferred: 0,
        total_bytes: Some(128),
        speed_bytes_per_second: 42,
        last_progress_at: Some(1_793_000_000_123),
        effective_idle_timeout_seconds: Some(90),
        retryable: false,
        resumable: false,
        failure_kind: None,
        safe_error: None,
        actual_destination: None,
        automatic_recovery_used: false,
        retry_successor: None,
        commit_summary: TransferCommitSummary::empty(),
        partial_commit: false,
        cleanup_pending: false,
    };

    let projected = transfer_projection(&summary);
    assert_eq!(projected["phase"], "transferring");
    assert_eq!(projected["operation"], "download");
    assert_eq!(projected["source"]["type"], "remote");
    assert_eq!(projected["source"]["hostId"], host_id.as_str());
    assert_eq!(projected["source"]["path"], "/download/report.txt");
    assert_eq!(projected["destination"]["type"], "local");
    assert_eq!(projected["destination"]["path"], "C:/download/report.txt");
    assert_eq!(projected["conflictPolicy"], "overwrite");
    assert_eq!(projected["idleTimeoutSeconds"], 90);
    assert_eq!(projected["progress"]["totalBytes"], 128);
    assert_eq!(projected["progress"]["speedBytesPerSecond"], 42);
    assert_eq!(projected["lastProgressAt"], 1_793_000_000_123_u64);
    assert!(projected.get("commitSummary").is_none());
    assert!(projected.get("cleanupPending").is_none());
    summary.cleanup_pending = true;
    let pending_projection = transfer_projection(&summary);
    assert_eq!(pending_projection["cleanupPending"], true);
}

/// 同一远端目录复制拒绝将目标嵌套进源路径或反向嵌套。
#[test]
fn remote_directory_transfer_rejects_nested_paths() {
    let host_id = HostId::new();
    let args = json!({
        "source": { "type": "remote", "hostId": host_id.as_str(), "path": "/srv/data" },
        "destination": { "type": "remote", "hostId": host_id.as_str(), "path": "/srv/data/archive" },
        "kind": "directory",
        "conflictPolicy": "skip"
    })
    .as_object()
    .expect("object arguments")
    .clone();

    assert!(transfer_request(args).is_err());
}

/// 外部远端路径遵循历史反斜杠规范化，并拒绝危险的根目录变更目标。
#[test]
fn remote_endpoint_normalizes_legacy_separators_and_rejects_root_mutation() {
    let host_id = HostId::new();
    let endpoint = remote_endpoint(host_id.as_str(), r"\srv\logs", false)
        .expect("legacy backslash path normalizes to POSIX separators");
    assert!(matches!(endpoint, FileEndpoint::Remote { path, .. } if path.as_str() == "/srv/logs"));
    assert!(remote_endpoint(host_id.as_str(), "/", false).is_err());
    assert!(remote_endpoint(host_id.as_str(), "/", true).is_ok());
}

/// 预览边界截断在多字节 UTF-8 码点时丢弃不完整尾部，而不产生替换字符。
#[test]
fn truncated_utf8_preview_drops_only_incomplete_tail() {
    let preview = FilePreview {
        endpoint: FileEndpoint::Local {
            path: PathBuf::from("preview.txt"),
        },
        bytes: vec![b'a', 0xe4, 0xb8],
        total_size: Some(4),
        truncated: true,
        binary: false,
        revision: Some(FileRevision {
            size: 4,
            modified: None,
            permissions: None,
            content_digest: None,
        }),
    };
    assert_eq!(preview_content(&preview), "a");
}

/// 内部 Skipped 终态保持明确 wire 值，不能伪装成 succeeded 或 failed。
///
/// @author kongweiguang
#[test]
fn skipped_transfer_projection_stays_distinct() {
    let summary = TransferSummary {
        id: TransferId::new(),
        request: TransferRequest {
            source: FileEndpoint::Local {
                path: PathBuf::from("source.txt"),
            },
            destination: FileEndpoint::remote(HostId::new(), "/target.txt")
                .expect("valid remote target"),
            kind: TransferKind::File,
            conflict_policy: ConflictPolicy::Skip,
            idle_timeout_seconds: None,
        },
        status: TransferStatus::Skipped,
        phase: None,
        bytes_transferred: 0,
        total_bytes: None,
        speed_bytes_per_second: 0,
        last_progress_at: None,
        effective_idle_timeout_seconds: Some(180),
        retryable: false,
        resumable: false,
        failure_kind: None,
        safe_error: None,
        actual_destination: None,
        automatic_recovery_used: false,
        retry_successor: None,
        commit_summary: TransferCommitSummary::empty(),
        partial_commit: false,
        cleanup_pending: false,
    };

    let projected = transfer_projection(&summary);
    assert_eq!(projected["status"], "skipped");
    assert_eq!(projected["phase"], "skipped");
    assert_eq!(projected["conflictPolicy"], "skip");
    assert_eq!(projected["progress"]["bytesTransferred"], 0);
}

/// 无变更目录沿用旧版 completed wire 状态，并单独说明零目标回执与零提交字节。
///
/// @author kongweiguang
#[test]
fn unchanged_directory_projection_preserves_completed_contract_and_reports_zero_commits() {
    let summary = TransferSummary {
        id: TransferId::new(),
        request: TransferRequest {
            source: FileEndpoint::remote(HostId::new(), "/source-tree")
                .expect("valid remote source"),
            destination: FileEndpoint::Local {
                path: PathBuf::from("target-tree"),
            },
            kind: TransferKind::Directory,
            conflict_policy: ConflictPolicy::Overwrite,
            idle_timeout_seconds: None,
        },
        status: TransferStatus::Unchanged,
        phase: None,
        bytes_transferred: 0,
        total_bytes: Some(0),
        speed_bytes_per_second: 0,
        last_progress_at: None,
        effective_idle_timeout_seconds: Some(180),
        retryable: false,
        resumable: false,
        failure_kind: None,
        safe_error: None,
        actual_destination: Some(FileEndpoint::Local {
            path: PathBuf::from("target-tree"),
        }),
        automatic_recovery_used: false,
        retry_successor: None,
        commit_summary: TransferCommitSummary::empty(),
        partial_commit: false,
        cleanup_pending: false,
    };

    let projected = transfer_projection(&summary);
    assert_eq!(projected["status"], "succeeded");
    assert_eq!(projected["phase"], "succeeded");
    assert_eq!(projected["commitSummary"]["outcome"], "unchanged");
    assert_eq!(projected["commitSummary"]["committedEntries"], 0);
    assert_eq!(projected["commitSummary"]["committedBytes"], 0);
}

/// CommitUnknown 在外部恢复字段中不可重试，且只公开稳定失败类别。
///
/// @author kongweiguang
#[test]
fn commit_unknown_projection_does_not_enable_retry() {
    let summary = TransferSummary {
        id: TransferId::new(),
        request: TransferRequest {
            source: FileEndpoint::remote(HostId::new(), "/source.bin")
                .expect("valid remote source"),
            destination: FileEndpoint::Local {
                path: PathBuf::from("target.bin"),
            },
            kind: TransferKind::Directory,
            conflict_policy: ConflictPolicy::Overwrite,
            idle_timeout_seconds: None,
        },
        status: TransferStatus::Failed,
        phase: None,
        bytes_transferred: 100,
        total_bytes: Some(100),
        speed_bytes_per_second: 8,
        last_progress_at: Some(1_793_000_001_456),
        effective_idle_timeout_seconds: Some(180),
        retryable: false,
        resumable: false,
        failure_kind: Some(TransferFailureKind::CommitUnknown),
        safe_error: Some("无法确认目标文件是否已提交，请先检查目标".to_owned()),
        actual_destination: None,
        automatic_recovery_used: false,
        retry_successor: None,
        commit_summary: {
            let mut summary = TransferCommitSummary::empty();
            summary
                .record_confirmed_entry(17)
                .expect("known committed directory item is counted");
            summary
        },
        partial_commit: true,
        cleanup_pending: false,
    };

    let projected = transfer_projection(&summary);
    assert_eq!(projected["failureKind"], "commitUnknown");
    assert_eq!(projected["retryable"], false);
    assert_eq!(projected["resumable"], false);
    assert_eq!(projected["commitSummary"]["outcome"], "partial");
    assert_eq!(projected["commitSummary"]["committedEntries"], 1);
    assert_eq!(projected["commitSummary"]["committedBytes"], 17);
}

/// 列表权限保持 ls 十位格式并区分 setuid、setgid、sticky 的大小写语义。
#[test]
fn permission_projection_keeps_file_type_and_special_bits() {
    assert_eq!(permission_string(FileEntryKind::File, 0o4755), "-rwsr-xr-x");
    assert_eq!(permission_string(FileEntryKind::File, 0o4644), "-rwSr--r--");
    assert_eq!(
        permission_string(FileEntryKind::Directory, 0o1777),
        "drwxrwxrwt"
    );
}

/// MCP 保留 raw wire 字段，并且仅输出领域适配器提供的值。
#[test]
fn file_entry_projection_preserves_optional_raw_line() {
    let host_id = HostId::new();
    let entry = FileEntry {
        endpoint: FileEndpoint::remote(host_id, "/var/data/report.txt")
            .expect("valid remote file path"),
        name: "report.txt".to_owned(),
        kind: FileEntryKind::File,
        size: Some(4),
        modified: None,
        permissions: Some(0o640),
        is_hidden: false,
        raw: Some("-rw-r----- ? kong staff 4 @123 report.txt".to_owned()),
    };
    let value = file_entry_value(&entry).expect("remote entry projects to MCP");
    assert_eq!(value["permissions"], "-rw-r-----");
    assert_eq!(value["raw"], "-rw-r----- ? kong staff 4 @123 report.txt");

    let mut without_raw = entry;
    without_raw.raw = None;
    let value = file_entry_value(&without_raw).expect("entry without source raw still projects");
    assert!(value["raw"].is_null());
}

/// 只把 retry 操作中的未知提交结果映射为冻结的不可恢复错误族。
#[test]
fn unknown_commit_retry_keeps_legacy_unavailable_error_family() {
    let response = retry_failure(AppError::outcome_unknown("提交结果未确认"));
    let value = response
        .structured_content
        .expect("retry refusal is structured");
    assert_eq!(value["errorKind"], "sftpTransferRetryUnavailable");
    assert_eq!(value["recoverable"], false);

    let response = retry_failure(AppError::conflict("其它重试冲突"));
    let value = response
        .structured_content
        .expect("other retry refusal is structured");
    assert_eq!(value["errorKind"], "conflict");
}
