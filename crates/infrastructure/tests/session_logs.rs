//! SessionLogStore 的真实文件、取消、脱敏与截断集成测试。
//!
//! 所有记录均使用人工合成文本和独立临时配置根，不读取用户目录。
//!
//! @author kongweiguang

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use kerminal_application::{
    AppError,
    sessions::logging::{
        OutputRecordResult, SessionLogFailureReason, SessionLogService, SessionLogStartRequest,
        SessionLogState, SessionOutputRecorder,
    },
};
use kerminal_domain::identity::SessionId;
use kerminal_infrastructure::{session_logs::SessionLogStore, storage::StorageCoordinator};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// 每项测试使用随机 data root，防止意外触碰正式 `~/.kerminal/logs`。
struct TestRoot(PathBuf);

impl TestRoot {
    /// 创建测试专属临时目录，名称不含 secret 或用户数据。
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "kerminal-session-log-test-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&path).expect("创建隔离测试根目录");
        Self(path)
    }

    /// 返回当前测试唯一的数据根路径。
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    /// 只清理当前随机临时目录及其日志文件。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 创建 session 日志服务并保持 recorder 可向 SessionManager hook 注入。
fn test_service(root: &TestRoot) -> (SessionLogService, Arc<SessionLogStore>) {
    let storage = StorageCoordinator::new(root.path().to_path_buf());
    let store = Arc::new(SessionLogStore::open(storage).expect("打开临时日志 adapter"));
    (SessionLogService::new(store.clone()), store)
}

/// 轮询 writer 的状态转移，避免测试用固定 sleep 猜测阻塞线程调度时机。
async fn wait_for_log_state(
    service: &SessionLogService,
    session_id: &SessionId,
    matches: impl Fn(&SessionLogState) -> bool,
) -> SessionLogState {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let state = service
            .state(CancellationToken::new(), session_id.clone())
            .await
            .expect("query writer state");
        if matches(&state) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "writer state transition timed out"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// 构造仅含人工 shell 元数据的日志启动请求。
fn start_request(session_id: SessionId) -> SessionLogStartRequest {
    SessionLogStartRequest {
        session_id,
        shell: "powershell.exe".to_owned(),
        cwd: Some("C:\\fixture\\workspace".to_owned()),
    }
}

/// 验证只记录 start 后原始字节、兼容旧头部与文件名并保留 stop 回执；路径按组件比较以兼容 Windows 分隔符。
///
/// @author kongweiguang
#[tokio::test]
async fn records_new_raw_output_with_legacy_header_and_redaction() {
    let root = TestRoot::new();
    let (service, store) = test_service(&root);
    let session_id = SessionId::parse("session-1234567890").expect("parse test id");
    let request = start_request(session_id.clone());
    let started = service
        .start(CancellationToken::new(), request.clone())
        .await
        .expect("start recording");
    assert!(started.active);
    let log_path = Path::new(started.path.as_deref().expect("started log path"));
    assert_eq!(
        log_path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str()),
        Some("sessions")
    );
    assert_eq!(
        log_path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str()),
        Some("logs")
    );
    assert!(
        log_path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("session-"))
    );
    assert!(
        started
            .path
            .as_deref()
            .unwrap()
            .ends_with("-session12345.log")
    );
    assert_eq!(
        service
            .start(CancellationToken::new(), request)
            .await
            .expect("idempotent start")
            .path,
        started.path
    );

    assert_eq!(
        store.try_record_output(&session_id, 41, b"TOKEN=split-"),
        OutputRecordResult::Queued
    );
    assert_eq!(
        store.try_record_output(
            &session_id,
            42,
            b"secret-value\nBearer abcdefghijklmnopqrstuvwxyz\n"
        ),
        OutputRecordResult::Queued
    );
    assert_eq!(
        store.try_record_output(
            &session_id,
            43,
            b"sk-abcdefghijklmnopqrstuv\npassword='second-secret'\n"
        ),
        OutputRecordResult::Queued
    );

    let stopped = service
        .stop(CancellationToken::new(), session_id.clone())
        .await
        .expect("flush and stop");
    assert!(!stopped.active);
    assert!(!stopped.truncated);
    assert!(stopped.bytes_written > 0);
    let path = PathBuf::from(stopped.path.clone().expect("final file path"));
    let contents = fs::read_to_string(path).expect("read final log");
    assert!(contents.starts_with("Kerminal session log\nsession_id: session-1234567890\n"));
    assert!(contents.contains("TOKEN=[已脱敏]"));
    assert!(contents.contains("Bearer [已脱敏]"));
    assert!(contents.contains("[已脱敏:api-key]"));
    assert!(contents.contains("password=[已脱敏]"));
    for secret in [
        "split-secret-value",
        "abcdefghijklmnopqrstuvwxyz",
        "abcdefghijklmnopqrstuv",
        "second-secret",
    ] {
        assert!(!contents.contains(secret), "log must not contain {secret}");
    }

    let inactive = service
        .state(CancellationToken::new(), session_id)
        .await
        .expect("state after successful stop");
    assert_eq!(inactive, stopped);
    let json = serde_json::to_value(inactive).expect("serialize old state shape");
    assert_eq!(
        json,
        serde_json::json!({
            "active": false,
            "path": stopped.path,
            "startedAt": stopped.started_at,
            "bytesWritten": stopped.bytes_written
        })
    );
}

/// 验证终端生命周期自动关闭录制后，state 仍返回成功 flush 的完整终态回执。
#[tokio::test]
async fn automatic_session_finish_retains_completed_receipt() {
    let root = TestRoot::new();
    let (service, store) = test_service(&root);
    let session_id = SessionId::parse("session-auto-finish").expect("parse session id");
    service
        .start(CancellationToken::new(), start_request(session_id.clone()))
        .await
        .expect("start recording");
    assert_eq!(
        store.try_record_output(&session_id, 1, b"finished output\n"),
        OutputRecordResult::Queued
    );

    let finished = store
        .finish_session(&session_id, Instant::now() + Duration::from_secs(3))
        .await
        .expect("session close flushes recording");
    assert!(!finished.active);
    assert!(!finished.truncated);
    assert!(finished.path.is_some());
    assert!(finished.started_at.is_some());
    assert!(finished.bytes_written > 0);

    let queried = service
        .state(CancellationToken::new(), session_id)
        .await
        .expect("query completed receipt after automatic close");
    assert_eq!(queried, finished);
    let log = fs::read_to_string(finished.path.expect("completed path"))
        .expect("read flushed terminal log");
    assert!(log.contains("finished output"));
}

/// 验证新录制发布后优先返回新 active 状态，并替换同 session 的旧完成回执。
#[tokio::test]
async fn successful_new_start_replaces_completed_receipt() {
    let root = TestRoot::new();
    let (service, _store) = test_service(&root);
    let session_id = SessionId::parse("session-restart-log").expect("parse session id");
    let first_start = service
        .start(CancellationToken::new(), start_request(session_id.clone()))
        .await
        .expect("start first recording");
    let first_finished = service
        .stop(CancellationToken::new(), session_id.clone())
        .await
        .expect("finish first recording");
    assert_eq!(first_start.path, first_finished.path);
    assert_eq!(
        service
            .state(CancellationToken::new(), session_id.clone())
            .await
            .expect("read first completion")
            .path,
        first_finished.path
    );

    let second_start = service
        .start(CancellationToken::new(), start_request(session_id.clone()))
        .await
        .expect("start replacement recording");
    assert!(second_start.active);
    assert_ne!(second_start.path, first_finished.path);
    assert_eq!(
        service
            .state(CancellationToken::new(), session_id.clone())
            .await
            .expect("active state takes precedence over old completion"),
        second_start
    );

    let second_finished = service
        .stop(CancellationToken::new(), session_id.clone())
        .await
        .expect("finish replacement recording");
    assert_eq!(
        service
            .state(CancellationToken::new(), session_id)
            .await
            .expect("read replacement completion")
            .path,
        second_finished.path
    );
}

/// 验证中文 UTF-8 字符被 PTY reader 拆到两个 chunk 时先合并原始字节再解码；状态字段由生命周期用例覆盖，本例只需 recorder 启动成功。
///
/// @author kongweiguang
#[tokio::test]
async fn preserves_utf8_characters_fragmented_across_reader_chunks() {
    let root = TestRoot::new();
    let (service, store) = test_service(&root);
    let session_id = SessionId::parse("session-utf8-fragment").expect("parse test id");
    service
        .start(CancellationToken::new(), start_request(session_id.clone()))
        .await
        .expect("start recording");
    let output = "中文输出 TOKEN=utf8-fragment-fixture\n";
    let bytes = output.as_bytes();
    let split_at = 4;

    assert_eq!(
        store.try_record_output(&session_id, 51, &bytes[..split_at]),
        OutputRecordResult::Queued
    );
    assert_eq!(
        store.try_record_output(&session_id, 52, &bytes[split_at..]),
        OutputRecordResult::Queued
    );
    let stopped = service
        .stop(CancellationToken::new(), session_id)
        .await
        .expect("flush fragmented UTF-8 output");

    let contents =
        fs::read_to_string(stopped.path.expect("final log path")).expect("read final log");
    assert!(contents.contains("中文输出"));
    assert!(!contents.contains('�'));
    assert!(!contents.contains("utf8-fragment-fixture"));
}

/// 保留每个 PTY sequence 的独立字节边界，避免跨行尾部合并后报告旧序号。
#[tokio::test]
async fn reports_first_unwritten_sequence_after_prior_line_prefixes_are_flushed() {
    let root = TestRoot::new();
    let (service, store) = test_service(&root);
    let session_id = SessionId::parse("session-sequence-boundary").expect("parse test id");
    let started = service
        .start(CancellationToken::new(), start_request(session_id.clone()))
        .await
        .expect("start recording");

    assert_eq!(
        store.try_record_output(&session_id, 10, b"first line\npartial-from-first"),
        OutputRecordResult::Queued
    );
    let first_flush = wait_for_log_state(&service, &session_id, |state| {
        state.bytes_written > started.bytes_written
    })
    .await;
    assert_eq!(
        store.try_record_output(&session_id, 11, b"completes-tail\npartial-from-second"),
        OutputRecordResult::Queued
    );
    wait_for_log_state(&service, &session_id, |state| {
        state.bytes_written > first_flush.bytes_written
    })
    .await;

    assert_eq!(
        store.try_record_output(&session_id, 12, &vec![b'a'; 40 * 1024]),
        OutputRecordResult::Queued
    );
    assert_eq!(
        store.try_record_output(&session_id, 13, &vec![b'b'; 30 * 1024]),
        OutputRecordResult::Queued
    );
    let failed = wait_for_log_state(&service, &session_id, |state| state.truncated).await;

    assert_eq!(failed.first_missing_sequence, Some(11));
    assert_eq!(
        failed.failure_reason,
        Some(SessionLogFailureReason::RedactionSafetyLimit)
    );
    assert!(
        service
            .stop(CancellationToken::new(), session_id)
            .await
            .is_err()
    );
}

/// 验证 raw reader 序号断档会停止该条日志并向状态查询显式报告首个缺失序号。
#[tokio::test]
async fn sequence_gap_is_reported_as_truncated_and_never_silently_skipped() {
    let root = TestRoot::new();
    let (service, store) = test_service(&root);
    let session_id = SessionId::parse("session-gap").expect("parse test id");
    service
        .start(CancellationToken::new(), start_request(session_id.clone()))
        .await
        .expect("start recording");
    assert_eq!(
        store.try_record_output(&session_id, 1, b"first\n"),
        OutputRecordResult::Queued
    );
    assert_eq!(
        store.try_record_output(&session_id, 3, b"missing second\n"),
        OutputRecordResult::Failed {
            first_missing_sequence: 2,
            reason: SessionLogFailureReason::SequenceGap,
        }
    );
    let stop_error = service
        .stop(CancellationToken::new(), session_id.clone())
        .await
        .expect_err("truncated writer must report IO failure");
    assert!(matches!(stop_error, AppError::Infrastructure { .. }));
    let state = service
        .state(CancellationToken::new(), session_id)
        .await
        .expect("read failed-state diagnostic");
    assert!(!state.active);
    assert!(state.truncated);
    assert_eq!(state.first_missing_sequence, Some(2));
    assert_eq!(
        state.failure_reason,
        Some(SessionLogFailureReason::SequenceGap)
    );
}

/// 验证取消在日志文件发布前生效，不创建空文件或留下活动 recorder。
#[tokio::test]
async fn cancelled_start_does_not_publish_a_log_file() {
    let root = TestRoot::new();
    let (service, _store) = test_service(&root);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let result = service
        .start(
            cancellation,
            start_request(SessionId::parse("session-cancelled").expect("parse test id")),
        )
        .await;
    assert!(matches!(result, Err(AppError::Cancelled { .. })));
    let files = fs::read_dir(root.path().join("logs/sessions"))
        .expect("read isolated session logs")
        .count();
    assert_eq!(files, 0);
}

/// 验证应用关闭 hook 在 deadline 内关闭 writer 并将尚未换行的尾部脱敏后 flush。
#[tokio::test]
async fn shutdown_flushes_partial_output_before_deadline() {
    let root = TestRoot::new();
    let (service, store) = test_service(&root);
    let session_id = SessionId::parse("session-shutdown").expect("parse test id");
    let started = service
        .start(CancellationToken::new(), start_request(session_id.clone()))
        .await
        .expect("start recording");
    assert_eq!(
        store.try_record_output(&session_id, 7, b"TOKEN=shutdown-secret"),
        OutputRecordResult::Queued
    );
    store
        .shutdown_until(Instant::now() + Duration::from_secs(5))
        .await
        .expect("shutdown recorder before deadline");
    let path = PathBuf::from(started.path.expect("started file path"));
    let contents = fs::read_to_string(path).expect("read shutdown log");
    assert!(contents.contains("TOKEN=[已脱敏]"));
    assert!(!contents.contains("shutdown-secret"));
}

/// 验证 Unix 日志目录和单文件不受 umask 放宽。
#[cfg(unix)]
#[test]
fn log_directory_and_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let root = TestRoot::new();
    let storage = StorageCoordinator::new(root.path().to_path_buf());
    let store = SessionLogStore::open(storage).expect("open isolated store");
    let session_id = SessionId::parse("session-permissions").expect("parse id");
    let request = start_request(session_id);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("create runtime");
    let state = runtime
        .block_on(
            SessionLogService::new(Arc::new(store.clone()))
                .start(CancellationToken::new(), request),
        )
        .expect("start log");
    let directory = root.path().join("logs/sessions");
    assert_eq!(
        fs::metadata(&directory)
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(state.path.unwrap())
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

/// 验证失败日志状态的序列化只增加明确的截断诊断字段。
#[test]
fn failed_state_serializes_reason_in_camel_case() {
    let state = SessionLogState {
        active: false,
        path: Some("synthetic-log-path".to_owned()),
        started_at: Some("1700000000".to_owned()),
        bytes_written: 42,
        truncated: true,
        first_missing_sequence: Some(9),
        failure_reason: Some(SessionLogFailureReason::RedactionSafetyLimit),
    };
    let value: Value = serde_json::to_value(state).expect("serialize failure state");
    assert_eq!(value["failureReason"], "redactionSafetyLimit");
}
