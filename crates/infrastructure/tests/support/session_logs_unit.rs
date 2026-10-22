//! Session log 私有生命周期边界测试；实现位于独立 tests/support 文件。
//!
//! @author kongweiguang

use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use kerminal_application::sessions::logging::SessionLogState;
use kerminal_domain::identity::SessionId;
use uuid::Uuid;

use super::{COMPLETED_STATE_LIMIT, CompletedStates, Recording, RecordingControl, SessionLogStore};
use crate::storage::StorageCoordinator;

/// 每个生命周期用例使用随机临时配置根，不触碰用户的正式日志目录。
struct TestRoot(PathBuf);

impl TestRoot {
    /// 创建仅供当前用例读写的目录，确保 writer 不接触真实用户数据。
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "kerminal-session-log-unit-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&path).expect("create isolated test root");
        Self(path)
    }

    /// 返回当前用例专属的配置根路径。
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TestRoot {
    /// 只删除当前测试随机生成的目录和合成日志。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 构造一个可由测试释放的 writer，稳定覆盖 deadline 超时而不依赖磁盘时延。
fn blocked_recording(recording_id: u64) -> (Arc<Recording>, mpsc::Sender<()>) {
    let (release_sender, release_receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let _ = release_receiver.recv_timeout(Duration::from_secs(10));
    });
    let (sender, _receiver) = mpsc::sync_channel(1);
    let mut state = SessionLogState::inactive();
    state.active = true;
    state.path = Some("synthetic-log-path".to_owned());
    (
        Arc::new(Recording {
            recording_id,
            control: Mutex::new(RecordingControl {
                state,
                next_sequence: Some(8),
                sender: Some(sender),
            }),
            worker: Mutex::new(Some(worker)),
        }),
        release_sender,
    )
}

/// 创建不启动后台线程的合成录制，用于覆盖旧 generation 的完成竞态。
fn synthetic_recording(recording_id: u64, state: SessionLogState) -> Arc<Recording> {
    Arc::new(Recording {
        recording_id,
        control: Mutex::new(RecordingControl {
            state,
            next_sequence: None,
            sender: None,
        }),
        worker: Mutex::new(None),
    })
}

/// 构造最小终态 DTO，避免测试使用任何真实用户数据或凭据。
fn terminal_state(path: String) -> SessionLogState {
    SessionLogState {
        active: false,
        path: Some(path),
        started_at: Some("2026-10-16T00:00:00Z".to_owned()),
        bytes_written: 64,
        truncated: false,
        first_missing_sequence: None,
        failure_reason: None,
    }
}

/// 超时后保留活动表记录和 JoinHandle，第二次收尾必须真实 join 后才清理。
#[test]
fn timeout_keeps_recording_for_retry_and_join() {
    let root = TestRoot::new();
    let store = SessionLogStore::open(StorageCoordinator::new(root.path().to_path_buf()))
        .expect("open isolated store");
    let session_id = SessionId::parse("session-finish-retry").expect("parse session id");
    let (recording, release_worker) = blocked_recording(1);
    store
        .inner
        .recordings
        .lock()
        .expect("recording lock")
        .insert(session_id.clone(), recording.clone());

    let first_finish =
        store.remove_and_finish(&session_id, Instant::now() + Duration::from_millis(10));
    assert!(first_finish.is_err());
    let retained = store
        .recording(&session_id)
        .expect("lookup after timeout")
        .expect("timeout must keep the recording");
    assert!(retained.worker.lock().expect("worker lock").is_some());
    assert!(
        retained
            .control
            .lock()
            .expect("control lock")
            .sender
            .is_none()
    );

    release_worker.send(()).expect("release blocked writer");
    let final_state = store
        .remove_and_finish(&session_id, Instant::now() + Duration::from_secs(1))
        .expect("retry writer join")
        .expect("recording is still registered");
    assert!(!final_state.active);
    assert!(
        store
            .recording(&session_id)
            .expect("lookup after join")
            .is_none()
    );
    assert_eq!(
        store
            .completed_state(&session_id)
            .expect("completed-state lock")
            .expect("completed receipt after join")
            .path
            .as_deref(),
        Some("synthetic-log-path")
    );
}

/// 应用关闭先关闭全部 sender；首个 writer 超时后仍尝试并保留所有待 join handle。
#[test]
fn shutdown_closes_every_sender_and_retries_remaining_workers() {
    let root = TestRoot::new();
    let store = SessionLogStore::open(StorageCoordinator::new(root.path().to_path_buf()))
        .expect("open isolated store");
    let first_id = SessionId::parse("session-shutdown-a").expect("parse first session id");
    let second_id = SessionId::parse("session-shutdown-b").expect("parse second session id");
    let (first, release_first) = blocked_recording(1);
    let (second, release_second) = blocked_recording(2);
    {
        let mut recordings = store.inner.recordings.lock().expect("recording lock");
        recordings.insert(first_id.clone(), first.clone());
        recordings.insert(second_id.clone(), second.clone());
    }

    let first_shutdown = store.shutdown_sync(Instant::now() + Duration::from_millis(10));
    assert!(first_shutdown.is_err());
    for recording in [&first, &second] {
        assert!(
            recording
                .control
                .lock()
                .expect("control lock")
                .sender
                .is_none(),
            "shutdown must close all output senders before waiting"
        );
        assert!(
            recording.worker.lock().expect("worker lock").is_some(),
            "timed out workers must remain joinable"
        );
    }

    release_first.send(()).expect("release first writer");
    release_second.send(()).expect("release second writer");
    store
        .shutdown_sync(Instant::now() + Duration::from_secs(1))
        .expect("retry and join every writer");
    assert!(
        store
            .recording(&first_id)
            .expect("lookup first session")
            .is_none()
    );
    assert!(
        store
            .recording(&second_id)
            .expect("lookup second session")
            .is_none()
    );
    assert!(
        store
            .completed_state(&first_id)
            .expect("first completion lock")
            .is_some()
    );
    assert!(
        store
            .completed_state(&second_id)
            .expect("second completion lock")
            .is_some()
    );
}

/// 缓存只保留最近 128 个 session，并拒绝旧 generation 覆盖同 session 的新终态。
#[test]
fn completed_state_cache_is_bounded_and_rejects_stale_generation() {
    let mut completed = CompletedStates::default();
    let mut oldest = None;
    let mut newest = None;
    for index in 0..=COMPLETED_STATE_LIMIT {
        let session_id = SessionId::parse(&format!("session-cache-{index}"))
            .expect("parse synthetic session id");
        if index == 0 {
            oldest = Some(session_id.clone());
        }
        if index == COMPLETED_STATE_LIMIT {
            newest = Some(session_id.clone());
        }
        completed.remember(
            session_id,
            index as u64 + 1,
            terminal_state(format!("synthetic-path-{index}")),
        );
    }

    assert_eq!(completed.states.len(), COMPLETED_STATE_LIMIT);
    assert!(completed.get(&oldest.expect("oldest session")).is_none());
    let newest = newest.expect("newest session");
    let latest_path = completed
        .get(&newest)
        .expect("newest completion remains cached")
        .path;
    completed.remember(
        newest.clone(),
        1,
        terminal_state("stale-generation-path".to_owned()),
    );
    assert_eq!(
        completed.get(&newest).and_then(|state| state.path),
        latest_path
    );
    assert_eq!(completed.order.len(), COMPLETED_STATE_LIMIT);
}

/// 旧 recorder 延迟完成时不能移除较新活动录制，也不能发布旧 session 回执。
#[test]
fn stale_completion_cannot_replace_new_active_recording() {
    let root = TestRoot::new();
    let store = SessionLogStore::open(StorageCoordinator::new(root.path().to_path_buf()))
        .expect("open isolated store");
    let session_id = SessionId::parse("session-stale-generation").expect("parse session id");
    let mut active_state = terminal_state("new-active-path".to_owned());
    active_state.active = true;
    let current = synthetic_recording(2, active_state);
    let stale = synthetic_recording(1, terminal_state("old-completed-path".to_owned()));
    store
        .inner
        .recordings
        .lock()
        .expect("recording lock")
        .insert(session_id.clone(), current.clone());

    store
        .publish_completed_state(
            &session_id,
            &stale,
            terminal_state("old-completed-path".to_owned()),
        )
        .expect("ignore stale completion");

    let registered = store
        .recording(&session_id)
        .expect("recording lookup")
        .expect("new recording remains registered");
    assert_eq!(registered.recording_id, current.recording_id);
    assert!(
        store
            .completed_state(&session_id)
            .expect("completed-state lookup")
            .is_none()
    );
}
