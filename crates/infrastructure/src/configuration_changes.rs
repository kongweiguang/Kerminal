//! 配置文件变更监听器；把文件系统事件合并后交给共享 ConfigurationStore 重扫。
//!
//! @author kongweiguang

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use kerminal_application::{AppError, AppResult};
use notify::event::{AccessKind, AccessMode};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::configuration::ConfigurationStore;

const EVENT_QUEUE_CAPACITY: usize = 256;
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(120);
const MAX_REPORTED_PATHS: usize = 128;

enum WatchInput {
    Paths(Vec<PathBuf>),
    Rescan,
}

/// 唯一配置 watcher 的所有权对象；关闭时先停止系统观察，再等待合并线程退出。
pub struct ConfigurationChangeWatcher {
    stop: Arc<AtomicBool>,
    watcher: Option<RecommendedWatcher>,
    worker: Option<JoinHandle<()>>,
}

impl ConfigurationChangeWatcher {
    /// 订阅配置根目录并启动有界事件合并线程，避免 GPUI 主线程执行目录扫描。
    pub fn start(configuration: ConfigurationStore) -> AppResult<Self> {
        let root = configuration.root().to_path_buf();
        let (sender, receiver) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let overflowed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let callback_sender = sender.clone();
        let callback_overflow = Arc::clone(&overflowed);
        let mut watcher = RecommendedWatcher::new(
            move |result: notify::Result<Event>| match result {
                Ok(event) => enqueue_event(&callback_sender, &callback_overflow, event),
                Err(_) => enqueue_rescan(&callback_sender, &callback_overflow),
            },
            Config::default(),
        )
        .map_err(|source| AppError::infrastructure_with_source("无法创建配置变更监听器", source))?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|source| AppError::infrastructure_with_source("无法监听配置目录", source))?;

        let worker_stop = Arc::clone(&stop);
        let worker_overflow = Arc::clone(&overflowed);
        let worker = thread::Builder::new()
            .name("kerminal-configuration-watch".to_owned())
            .spawn(move || run_worker(configuration, root, receiver, worker_stop, worker_overflow))
            .map_err(|source| {
                AppError::infrastructure_with_source("无法启动配置变更线程", source)
            })?;

        Ok(Self {
            stop,
            watcher: Some(watcher),
            worker: Some(worker),
        })
    }

    /// 到绝对 deadline 前等待线程退出；超时保留 JoinHandle 供 composition root 再次关闭。
    pub async fn shutdown_until(&mut self, deadline: Instant) -> AppResult<()> {
        self.stop.store(true, Ordering::Release);
        self.watcher.take();
        loop {
            let Some(worker) = self.worker.as_ref() else {
                return Ok(());
            };
            if worker.is_finished() {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(AppError::timeout("等待配置变更线程退出超时"));
            }
            let next_check = deadline.min(now + Duration::from_millis(10));
            tokio::time::sleep_until(tokio::time::Instant::from_std(next_check)).await;
        }
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker.join().map_err(|_| {
            AppError::infrastructure_with_source(
                "配置变更线程异常终止",
                io::Error::other("configuration watcher worker panicked"),
            )
        })?;
        Ok(())
    }
}

/// 重扫标记优先处理；仅忽略明确的只读打开/读取事件，写关闭和未知事件仍进入有界队列。
/// Linux 会报告被监听配置目录与文件的 OPEN；这些事件来自本应用的只读扫描，若转成配置变化会自激重扫。
///
/// @author kongweiguang
fn enqueue_event(sender: &SyncSender<WatchInput>, overflowed: &AtomicBool, event: Event) {
    if event.need_rescan() {
        enqueue_rescan(sender, overflowed);
        return;
    }
    if matches!(
        &event.kind,
        EventKind::Access(
            AccessKind::Open(_) | AccessKind::Read | AccessKind::Close(AccessMode::Read)
        )
    ) {
        return;
    }

    match sender.try_send(WatchInput::Paths(event.paths)) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => overflowed.store(true, Ordering::Release),
        Err(TrySendError::Disconnected(_)) => {}
    }
}

/// 将系统 watcher 错误折叠为完整重扫，不把平台错误文本传给配置消费者。
fn enqueue_rescan(sender: &SyncSender<WatchInput>, overflowed: &AtomicBool) {
    match sender.try_send(WatchInput::Rescan) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => overflowed.store(true, Ordering::Release),
        Err(TrySendError::Disconnected(_)) => {}
    }
}

/// 在独立线程中合并短时间内的保存事件，再统一刷新恢复态和订阅通知。
fn run_worker(
    configuration: ConfigurationStore,
    root: PathBuf,
    receiver: Receiver<WatchInput>,
    stop: Arc<AtomicBool>,
    overflowed: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Acquire) {
        let first = match receiver.recv_timeout(DEBOUNCE_WINDOW) {
            Ok(input) => input,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let mut paths = BTreeSet::new();
        let mut full_rescan = false;
        merge_input(first, &root, &mut paths, &mut full_rescan);
        let deadline = Instant::now() + DEBOUNCE_WINDOW;
        while Instant::now() < deadline {
            let timeout = deadline.saturating_duration_since(Instant::now());
            match receiver.recv_timeout(timeout) {
                Ok(input) => merge_input(input, &root, &mut paths, &mut full_rescan),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        if overflowed.swap(false, Ordering::AcqRel) {
            full_rescan = true;
        }
        if paths.is_empty() && !full_rescan {
            continue;
        }
        if paths.len() > MAX_REPORTED_PATHS {
            full_rescan = true;
        }
        let paths = paths
            .into_iter()
            .take(MAX_REPORTED_PATHS)
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        if configuration
            .refresh_external_changes(paths, full_rescan)
            .is_err()
        {
            let _ = configuration.refresh_external_changes(Vec::new(), true);
        }
    }
}

/// 把批次内路径归一到配置根，只接受配置资产与本地 profile 恢复 journal。
fn merge_input(
    input: WatchInput,
    root: &Path,
    paths: &mut BTreeSet<PathBuf>,
    full_rescan: &mut bool,
) {
    match input {
        WatchInput::Rescan => *full_rescan = true,
        WatchInput::Paths(event_paths) => {
            for absolute in event_paths {
                let Ok(relative) = absolute.strip_prefix(root) else {
                    continue;
                };
                if is_relevant_path(relative, full_rescan) {
                    paths.insert(relative.to_path_buf());
                }
            }
        }
    }
}

/// 只把正式配置文档或默认 profile 的恢复 journal 交给业务层，忽略 Git、日志和 vault 事件。
fn is_relevant_path(relative: &Path, full_rescan: &mut bool) -> bool {
    let components = relative
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>();
    match components.as_slice() {
        ["settings.toml"] | ["hosts", "groups.toml"] => true,
        [".storage-transactions", "profile-default.json"] => true,
        [directory] if is_configuration_directory(directory) => {
            *full_rescan = true;
            true
        }
        [directory, file]
            if is_configuration_directory(directory)
                && Path::new(file)
                    .extension()
                    .is_some_and(|extension| extension == "toml") =>
        {
            true
        }
        _ => false,
    }
}

/// 识别当前文件化配置目录，不让运行态和密钥目录触发全目录解析。
fn is_configuration_directory(directory: &str) -> bool {
    matches!(directory, "profiles" | "hosts" | "snippets" | "workflows")
}
