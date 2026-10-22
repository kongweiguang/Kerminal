//! 共享存储锁的旧版 v1 互读、故障关闭与并发回归。
//!
//! @author kongweiguang

use std::{
    fs,
    fs::OpenOptions,
    path::Path,
    sync::mpsc,
    sync::{
        Arc, Barrier, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use kerminal_infrastructure::storage::{StorageCoordinator, StorageLockError};
use sysinfo::{Pid, System};

/// 写入与冻结旧版 FileStore 一致的人工 owner 记录，不包含任何用户数据或秘密。
fn write_v1_owner(path: &Path, pid: u32, process_started_at_unix_seconds: u64, nonce: &str) {
    let created_at_unix_ms = 1_800_000_000_000_u64;
    let text = format!(
        "schema_version = 1\npid = {pid}\nprocess_started_at_unix_seconds = {process_started_at_unix_seconds}\ncreated_at_unix_ms = {created_at_unix_ms}\nnonce = \"{nonce}\"\n"
    );
    fs::write(path, text).expect("write synthetic legacy lock metadata");
}

/// 读取当前测试进程启动时间，用于构造旧版能识别的活跃 owner。
fn current_process_started_at() -> u64 {
    let system = System::new_all();
    system
        .process(Pid::from_u32(std::process::id()))
        .map(|process| process.start_time())
        .filter(|started_at| *started_at > 0)
        .expect("read current test process start time")
}

/// 选择当前平台可表示范围内的高位 PID，避免 Unix 将测试 PID 转换溢出。
fn unused_process_id() -> u32 {
    let system = System::new_all();
    #[cfg(windows)]
    let mut candidates = (u32::MAX - 1024..=u32::MAX).rev();
    #[cfg(unix)]
    let mut candidates = ((i32::MAX as u32) - 1024..=i32::MAX as u32).rev();

    candidates
        .find(|pid| !system.processes().contains_key(&Pid::from_u32(*pid)))
        .expect("find an unused synthetic process id")
}

/// 锁持有期间仍允许旧 v1 reader 从第二句柄读取固定字段，释放时只删自己的 nonce 文件。
#[test]
fn publishes_legacy_v1_metadata_and_removes_owned_lock() {
    let root = tempfile::tempdir().expect("create isolated storage root");
    let storage = StorageCoordinator::new(root.path());
    let lock_path = root.path().join(".storage.lock");

    let guard = storage.lock().expect("acquire storage lock");
    let source = fs::read_to_string(&lock_path).expect("read published owner metadata");
    let metadata: toml::Value = toml::from_str(&source).expect("decode old-compatible TOML");
    let table = metadata.as_table().expect("owner must be a TOML table");
    let mut names = table.keys().map(String::as_str).collect::<Vec<_>>();
    names.sort_unstable();

    assert_eq!(
        names,
        [
            "created_at_unix_ms",
            "nonce",
            "pid",
            "process_started_at_unix_seconds",
            "schema_version",
        ]
    );
    assert_eq!(table["schema_version"].as_integer(), Some(1));
    assert_eq!(
        table["pid"].as_integer(),
        Some(i64::from(std::process::id()))
    );
    assert!(
        table["nonce"]
            .as_str()
            .is_some_and(|nonce| !nonce.is_empty())
    );

    drop(guard);
    assert!(!lock_path.exists(), "released owner record must be removed");
}

/// 有效的旧版活动 owner 必须被新 adapter 识别为冲突，而不能按陈旧文件覆盖。
#[test]
fn rejects_live_legacy_v1_owner_without_changing_it() {
    let root = tempfile::tempdir().expect("create isolated storage root");
    let lock_path = root.path().join(".storage.lock");
    write_v1_owner(
        &lock_path,
        std::process::id(),
        current_process_started_at(),
        "synthetic-legacy-live-owner",
    );
    let original = fs::read(&lock_path).expect("capture synthetic owner bytes");

    let storage = StorageCoordinator::new(root.path());
    let result = storage.lock();

    assert!(matches!(result, Err(StorageLockError::Locked)));
    assert_eq!(
        fs::read(&lock_path).expect("preserved owner bytes"),
        original
    );
}

/// 空文件、损坏 TOML 和未来版本都必须保留并报告，不得被误清理成可写状态。
#[test]
fn rejects_invalid_or_future_metadata_without_overwriting() {
    for invalid in [
        String::new(),
        "schema_version = [broken".to_owned(),
        "schema_version = 2\npid = 1\nprocess_started_at_unix_seconds = 1\ncreated_at_unix_ms = 1\nnonce = \"future\"\n".to_owned(),
        "schema_version = 1\npid = 1\nprocess_started_at_unix_seconds = 0\ncreated_at_unix_ms = 1\nnonce = \"unknown-start-time\"\n".to_owned(),
    ] {
        let root = tempfile::tempdir().expect("create isolated storage root");
        let lock_path = root.path().join(".storage.lock");
        fs::write(&lock_path, &invalid).expect("write invalid synthetic metadata");

        let storage = StorageCoordinator::new(root.path());
        let result = storage.lock();

        assert!(matches!(result, Err(StorageLockError::InvalidMetadata)));
        assert_eq!(
            fs::read_to_string(&lock_path).expect("invalid owner remains available for recovery"),
            invalid
        );
    }
}

/// 只有 PID 不存在的旧 owner 可回收；新 owner 发布后释放回到无锁状态。
#[test]
fn reclaims_only_provably_stale_legacy_owner() {
    let root = tempfile::tempdir().expect("create isolated storage root");
    let lock_path = root.path().join(".storage.lock");
    write_v1_owner(&lock_path, unused_process_id(), 1, "synthetic-stale-owner");

    let storage = StorageCoordinator::new(root.path());
    let guard = storage
        .lock()
        .expect("reclaim dead owner and publish a new one");
    let new_source = fs::read_to_string(&lock_path).expect("read replacement owner");
    let new_metadata: toml::Value = toml::from_str(&new_source).expect("decode replacement owner");
    assert_eq!(
        new_metadata["pid"].as_integer(),
        Some(i64::from(std::process::id()))
    );
    assert_ne!(
        new_metadata["nonce"].as_str(),
        Some("synthetic-stale-owner")
    );

    drop(guard);
    assert!(!lock_path.exists());
}

#[cfg(unix)]
/// Unix pid_t 转换溢出属于未知状态，必须保留原 owner 而不能回收。
#[test]
fn preserves_legacy_owner_when_pid_exceeds_pid_t_range() {
    let root = tempfile::tempdir().expect("create isolated storage root");
    let lock_path = root.path().join(".storage.lock");
    write_v1_owner(&lock_path, u32::MAX, 1, "synthetic-out-of-range-owner");
    let original = fs::read(&lock_path).expect("capture synthetic owner bytes");

    let storage = StorageCoordinator::new(root.path());
    assert!(matches!(storage.lock(), Err(StorageLockError::Locked)));
    assert_eq!(
        fs::read(&lock_path).expect("unknown PID owner remains available for recovery"),
        original
    );
}

/// nonce 已变化时旧守卫必须保留新 owner 文件，避免释放路径误删接管者状态。
#[test]
fn release_preserves_replaced_owner_nonce() {
    let root = tempfile::tempdir().expect("create isolated storage root");
    let lock_path = root.path().join(".storage.lock");
    let storage = StorageCoordinator::new(root.path());
    let guard = storage.lock().expect("acquire storage lock");
    write_v1_owner(
        &lock_path,
        std::process::id(),
        current_process_started_at(),
        "synthetic-replacement-owner",
    );
    let replacement = fs::read(&lock_path).expect("capture replacement owner bytes");

    drop(guard);

    assert_eq!(
        fs::read(&lock_path).expect("replacement owner preserved"),
        replacement
    );
}

/// 已被其他 handle 排他锁定的陈旧文件不能被新 coordinator 回收或改写。
#[test]
fn stale_recovery_respects_an_existing_os_file_lock() {
    let root = tempfile::tempdir().expect("create isolated storage root");
    let lock_path = root.path().join(".storage.lock");
    write_v1_owner(&lock_path, unused_process_id(), 1, "synthetic-stale-owner");
    let original = fs::read(&lock_path).expect("capture stale owner bytes before locking");
    let stale_owner = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open synthetic stale owner");
    stale_owner.lock().expect("hold OS advisory lock");

    let storage = StorageCoordinator::new(root.path());
    assert!(matches!(storage.lock(), Err(StorageLockError::Locked)));

    drop(stale_owner);
    assert_eq!(
        fs::read(&lock_path).expect("owner remains on disk"),
        original
    );
    let guard = storage
        .lock()
        .expect("reclaim after advisory lock owner releases it");
    drop(guard);
    assert!(!lock_path.exists());
}

/// 多个共享同一 coordinator 的线程同时争锁时，stale 回收与受保护区始终串行。
#[test]
fn concurrent_stale_recovery_is_serialized_for_shared_coordinator() {
    const WORKERS: usize = 6;
    let root = tempfile::tempdir().expect("create isolated storage root");
    let lock_path = root.path().join(".storage.lock");
    write_v1_owner(
        &lock_path,
        unused_process_id(),
        1,
        "synthetic-racing-stale-owner",
    );
    let storage = StorageCoordinator::new(root.path());
    let start = Arc::new(Barrier::new(WORKERS + 1));
    let active = Arc::new(AtomicUsize::new(0));
    let overlap_detected = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::with_capacity(WORKERS);

    for _ in 0..WORKERS {
        let storage = storage.clone();
        let start = Arc::clone(&start);
        let active = Arc::clone(&active);
        let overlap_detected = Arc::clone(&overlap_detected);
        workers.push(thread::spawn(move || {
            start.wait();
            let guard = storage
                .lock()
                .expect("shared coordinator serializes lock owners");
            if active.fetch_add(1, Ordering::SeqCst) != 0 {
                overlap_detected.store(true, Ordering::SeqCst);
            }
            thread::sleep(Duration::from_millis(2));
            active.fetch_sub(1, Ordering::SeqCst);
            drop(guard);
        }));
    }

    start.wait();
    for worker in workers {
        worker.join().expect("join lock worker");
    }

    assert!(
        !overlap_detected.load(Ordering::SeqCst),
        "cloned coordinators never overlap protected owners"
    );
    assert!(!lock_path.exists(), "last owner removes its v1 metadata");
}

/// 独立 coordinator 通过旧文件对象锁串行回收，且并发接管时至多一方取得 owner。
#[test]
fn concurrent_stale_recovery_is_serialized_across_coordinators() {
    const WORKERS: usize = 8;
    let root = tempfile::tempdir().expect("create isolated storage root");
    let lock_path = root.path().join(".storage.lock");
    write_v1_owner(
        &lock_path,
        unused_process_id(),
        1,
        "synthetic-cross-coordinator-stale-owner",
    );

    let start = Arc::new(Barrier::new(WORKERS + 1));
    let active = Arc::new(AtomicUsize::new(0));
    let successes = Arc::new(AtomicUsize::new(0));
    let locked = Arc::new(AtomicUsize::new(0));
    let unexpected = Arc::new(AtomicUsize::new(0));
    let overlap_detected = Arc::new(AtomicBool::new(false));
    let release_owner = Arc::new((Mutex::new(false), Condvar::new()));
    let (attempt_tx, attempt_rx) = mpsc::channel();
    let mut workers = Vec::with_capacity(WORKERS);

    for _ in 0..WORKERS {
        let storage = StorageCoordinator::new(root.path());
        let start = Arc::clone(&start);
        let active = Arc::clone(&active);
        let successes = Arc::clone(&successes);
        let locked = Arc::clone(&locked);
        let unexpected = Arc::clone(&unexpected);
        let overlap_detected = Arc::clone(&overlap_detected);
        let release_owner = Arc::clone(&release_owner);
        let attempt_tx = attempt_tx.clone();
        workers.push(thread::spawn(move || {
            start.wait();
            let guard = match storage.lock() {
                Ok(guard) => {
                    successes.fetch_add(1, Ordering::SeqCst);
                    if active.fetch_add(1, Ordering::SeqCst) != 0 {
                        overlap_detected.store(true, Ordering::SeqCst);
                    }
                    Some(guard)
                }
                Err(StorageLockError::Locked) => {
                    locked.fetch_add(1, Ordering::SeqCst);
                    None
                }
                Err(_) => {
                    unexpected.fetch_add(1, Ordering::SeqCst);
                    None
                }
            };
            attempt_tx.send(()).expect("report lock attempt");

            if let Some(guard) = guard {
                let (release_mutex, release_condition) = &*release_owner;
                let mut released = release_mutex.lock().expect("lock test release state");
                while !*released {
                    released = release_condition
                        .wait(released)
                        .expect("wait for test owner release");
                }
                active.fetch_sub(1, Ordering::SeqCst);
                drop(guard);
            }
        }));
    }
    drop(attempt_tx);

    start.wait();
    for _ in 0..WORKERS {
        attempt_rx.recv().expect("collect every lock attempt");
    }
    let observed_successes = successes.load(Ordering::SeqCst);
    let observed_locked = locked.load(Ordering::SeqCst);
    let observed_unexpected = unexpected.load(Ordering::SeqCst);
    let observed_active = active.load(Ordering::SeqCst);
    let observed_overlap = overlap_detected.load(Ordering::SeqCst);

    let (release_mutex, release_condition) = &*release_owner;
    *release_mutex.lock().expect("lock test release state") = true;
    release_condition.notify_all();
    for worker in workers {
        worker.join().expect("join independent coordinator");
    }

    assert_eq!(observed_successes, 1);
    assert_eq!(observed_locked, WORKERS - 1);
    assert_eq!(observed_unexpected, 0);
    assert_eq!(
        observed_active, 1,
        "the winning owner stays held through all attempts"
    );
    assert!(
        !observed_overlap,
        "independent coordinators never overlap owners"
    );
    assert!(
        !lock_path.exists(),
        "the single winning owner releases its v1 record"
    );
}
