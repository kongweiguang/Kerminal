//! 原始 session 输出的脱敏日志 adapter。
//!
//! @author kongweiguang

use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    sessions::logging::{
        OutputRecordResult, SessionLogFailureReason, SessionLogRepository, SessionLogStartRequest,
        SessionLogState, SessionOutputRecorder, redact_terminal_text,
    },
};
use kerminal_domain::identity::SessionId;
use tokio::task;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::storage::StorageCoordinator;

const DEFAULT_QUEUE_CAPACITY: usize = 128;
const MAX_QUEUE_CAPACITY: usize = 1_024;
const MAX_OUTPUT_CHUNK_BYTES: usize = 64 * 1024;
const MAX_ACTIVE_RECORDINGS: usize = 64;
const MAX_REDACTION_BUFFER_BYTES: usize = 64 * 1024;
const COMPLETED_STATE_LIMIT: usize = 128;
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const LOG_RELATIVE_PATH: &str = "logs/sessions";

/// 单次 PTY read 只复制到有界队列；磁盘写入始终由专属阻塞线程完成。
enum WriterMessage {
    Output {
        sequence: u64,
        bytes: Zeroizing<Vec<u8>>,
    },
}

/// 同步 reader 与后台 writer 共享的轻量状态；正文不进入诊断字段。
struct RecordingControl {
    state: SessionLogState,
    next_sequence: Option<u64>,
    sender: Option<SyncSender<WriterMessage>>,
}

/// 每条录制占用一个 bounded queue 和 writer thread，避免磁盘延迟阻塞 PTY reader。
struct Recording {
    recording_id: u64,
    control: Mutex<RecordingControl>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

/// 保存一次录制的不可变完成快照，并用内部 generation 阻止旧完成覆盖新录制。
struct CompletedState {
    recording_id: u64,
    state: SessionLogState,
}

/// 按完成顺序有界保留终态；超过容量时淘汰最早完成的 session。
#[derive(Default)]
struct CompletedStates {
    states: HashMap<SessionId, CompletedState>,
    order: VecDeque<(SessionId, u64)>,
}

/// 同一用户数据根下的 session 日志集合与并发边界。
struct SessionLogInner {
    log_root: PathBuf,
    queue_capacity: usize,
    next_recording_id: AtomicU64,
    recordings: Mutex<HashMap<SessionId, Arc<Recording>>>,
    completed_states: Mutex<CompletedStates>,
    operation_lock: Mutex<()>,
}

impl CompletedStates {
    /// 只接受不旧于现有 generation 的完成快照，并按 FIFO 完成顺序限制缓存大小。
    fn remember(&mut self, session_id: SessionId, recording_id: u64, state: SessionLogState) {
        if self
            .states
            .get(&session_id)
            .is_some_and(|existing| existing.recording_id > recording_id)
        {
            return;
        }
        self.order.retain(|(existing, _)| existing != &session_id);
        self.order.push_back((session_id.clone(), recording_id));
        self.states.insert(
            session_id,
            CompletedState {
                recording_id,
                state,
            },
        );
        while self.order.len() > COMPLETED_STATE_LIMIT {
            if let Some((expired, expired_id)) = self.order.pop_front()
                && self
                    .states
                    .get(&expired)
                    .is_some_and(|existing| existing.recording_id == expired_id)
            {
                self.states.remove(&expired);
            }
        }
    }

    /// 按 session 返回最近一次完成快照，不把内部 generation 暴露给应用或 MCP。
    fn get(&self, session_id: &SessionId) -> Option<SessionLogState> {
        self.states
            .get(session_id)
            .map(|completed| completed.state.clone())
    }

    /// 成功发布新录制后移除同 session 的旧终态与 FIFO 索引。
    fn remove(&mut self, session_id: &SessionId) {
        self.states.remove(session_id);
        self.order.retain(|(existing, _)| existing != session_id);
    }
}

/// 可同时装配为 MCP 日志用例 repository 与 SessionManager 原始输出 recorder。
#[derive(Clone)]
pub struct SessionLogStore {
    inner: Arc<SessionLogInner>,
}

impl SessionLogStore {
    /// 使用统一配置根创建 logs/sessions，并将队列容量限制在有界范围内。
    pub fn open(storage: StorageCoordinator) -> AppResult<Self> {
        Self::with_queue_capacity(storage, DEFAULT_QUEUE_CAPACITY)
    }

    /// 允许测试与桌面装配调整队列上限，但拒绝零容量和无界队列。
    pub fn with_queue_capacity(
        storage: StorageCoordinator,
        queue_capacity: usize,
    ) -> AppResult<Self> {
        let root = storage.root();
        fs::create_dir_all(root).map_err(|source| {
            AppError::infrastructure_with_source("准备终端日志目录失败", source)
        })?;
        let log_root = root.join(LOG_RELATIVE_PATH);
        ensure_directory_tree(root, &log_root).map_err(|source| {
            AppError::infrastructure_with_source("准备终端日志目录失败", source)
        })?;
        Ok(Self {
            inner: Arc::new(SessionLogInner {
                log_root,
                queue_capacity: queue_capacity.clamp(1, MAX_QUEUE_CAPACITY),
                next_recording_id: AtomicU64::new(1),
                recordings: Mutex::new(HashMap::new()),
                completed_states: Mutex::new(CompletedStates::default()),
                operation_lock: Mutex::new(()),
            }),
        })
    }

    /// 在阻塞池内创建 header 后才发布 recorder，避免调用方误以为日志已启动。
    fn start_sync(
        &self,
        cancellation: CancellationToken,
        request: SessionLogStartRequest,
    ) -> AppResult<SessionLogState> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("启动终端日志已取消"));
        }
        let _operation = self
            .inner
            .operation_lock
            .lock()
            .map_err(|_| AppError::infrastructure("终端日志状态锁不可用"))?;

        if let Some(existing) = self.recording(&request.session_id)? {
            let state = recording_state(&existing)?;
            if state.active {
                return Ok(state);
            }
            self.remove_and_finish(&request.session_id, Instant::now() + STOP_TIMEOUT)?;
        }
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("启动终端日志已取消"));
        }
        let active_count = self
            .inner
            .recordings
            .lock()
            .map_err(|_| AppError::infrastructure("终端日志状态锁不可用"))?
            .len();
        if active_count >= MAX_ACTIVE_RECORDINGS {
            return Err(AppError::conflict("同时录制的终端日志数量已达上限"));
        }

        let recording_id = self.reserve_recording_id()?;
        let started_at = unix_timestamp();
        let path = create_log_file(
            &self.inner.log_root,
            &request.session_id,
            &request.shell,
            request.cwd.as_deref(),
            &started_at,
        )
        .map_err(|source| AppError::infrastructure_with_source("创建终端日志文件失败", source))?;
        if cancellation.is_cancelled() {
            let _ = fs::remove_file(&path);
            return Err(AppError::cancelled("启动终端日志已取消"));
        }

        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|source| {
                let _ = fs::remove_file(&path);
                AppError::infrastructure_with_source("打开终端日志文件失败", source)
            })?;
        let (sender, receiver) = mpsc::sync_channel(self.inner.queue_capacity);
        let bytes_written = match fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(source) => {
                drop(file);
                let _ = fs::remove_file(&path);
                return Err(AppError::infrastructure_with_source(
                    "读取终端日志状态失败",
                    source,
                ));
            }
        };
        let initial_state = SessionLogState {
            active: true,
            path: Some(path.to_string_lossy().into_owned()),
            started_at: Some(started_at),
            bytes_written,
            truncated: false,
            first_missing_sequence: None,
            failure_reason: None,
        };
        let recording = Arc::new(Recording {
            recording_id,
            control: Mutex::new(RecordingControl {
                state: initial_state.clone(),
                next_sequence: None,
                sender: Some(sender),
            }),
            worker: Mutex::new(None),
        });
        let mut recordings = self
            .inner
            .recordings
            .lock()
            .map_err(|_| AppError::infrastructure("终端日志状态锁不可用"))?;
        let mut completed = self
            .inner
            .completed_states
            .lock()
            .map_err(|_| AppError::infrastructure("终端日志状态锁不可用"))?;
        let writer_recording = Arc::clone(&recording);
        let worker = thread::Builder::new()
            .name("kerminal-session-log".to_owned())
            .spawn(move || run_writer(file, receiver, writer_recording))
            .map_err(|source| {
                let _ = fs::remove_file(&path);
                AppError::infrastructure_with_source("启动终端日志 writer 失败", source)
            })?;
        *recording
            .worker
            .lock()
            .map_err(|_| AppError::infrastructure("终端日志 writer 状态锁不可用"))? = Some(worker);

        completed.remove(&request.session_id);
        recordings.insert(request.session_id.clone(), recording);
        Ok(initial_state)
    }

    /// 在一次不可阻塞的队列操作内检查序号、提交字节或明确标记本次录制截断。
    fn record_output(
        &self,
        session_id: &SessionId,
        sequence: u64,
        bytes: &[u8],
    ) -> OutputRecordResult {
        if bytes.is_empty() {
            return OutputRecordResult::Ignored;
        }
        let recording = match self.recording(session_id) {
            Ok(Some(recording)) => recording,
            Ok(None) => return OutputRecordResult::Ignored,
            Err(_) => {
                return OutputRecordResult::Failed {
                    first_missing_sequence: sequence,
                    reason: SessionLogFailureReason::WriterStopped,
                };
            }
        };
        let Ok(mut control) = recording.control.lock() else {
            return OutputRecordResult::Failed {
                first_missing_sequence: sequence,
                reason: SessionLogFailureReason::WriterStopped,
            };
        };
        if !control.state.active {
            return match control.state.failure_reason {
                Some(reason) => OutputRecordResult::Failed {
                    first_missing_sequence: control
                        .state
                        .first_missing_sequence
                        .unwrap_or(sequence),
                    reason,
                },
                None => OutputRecordResult::Ignored,
            };
        }
        if bytes.len() > MAX_OUTPUT_CHUNK_BYTES {
            fail_recording(
                &mut control,
                sequence,
                SessionLogFailureReason::RedactionSafetyLimit,
            );
            return OutputRecordResult::Failed {
                first_missing_sequence: sequence,
                reason: SessionLogFailureReason::RedactionSafetyLimit,
            };
        }
        let expected = *control.next_sequence.get_or_insert(sequence);
        if sequence != expected {
            fail_recording(&mut control, expected, SessionLogFailureReason::SequenceGap);
            return OutputRecordResult::Failed {
                first_missing_sequence: expected,
                reason: SessionLogFailureReason::SequenceGap,
            };
        }
        let Some(next_sequence) = sequence.checked_add(1) else {
            fail_recording(&mut control, sequence, SessionLogFailureReason::SequenceGap);
            return OutputRecordResult::Failed {
                first_missing_sequence: sequence,
                reason: SessionLogFailureReason::SequenceGap,
            };
        };
        let message = WriterMessage::Output {
            sequence,
            bytes: Zeroizing::new(bytes.to_vec()),
        };
        let Some(sender) = control.sender.as_ref().cloned() else {
            fail_recording(
                &mut control,
                sequence,
                SessionLogFailureReason::WriterStopped,
            );
            return OutputRecordResult::Failed {
                first_missing_sequence: sequence,
                reason: SessionLogFailureReason::WriterStopped,
            };
        };
        match sender.try_send(message) {
            Ok(()) => {
                control.next_sequence = Some(next_sequence);
                OutputRecordResult::Queued
            }
            Err(TrySendError::Full(_)) => {
                fail_recording(&mut control, sequence, SessionLogFailureReason::QueueFull);
                OutputRecordResult::Failed {
                    first_missing_sequence: sequence,
                    reason: SessionLogFailureReason::QueueFull,
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                fail_recording(
                    &mut control,
                    sequence,
                    SessionLogFailureReason::WriterStopped,
                );
                OutputRecordResult::Failed {
                    first_missing_sequence: sequence,
                    reason: SessionLogFailureReason::WriterStopped,
                }
            }
        }
    }

    /// 完成并 join 后才从活动表移除；传播 finish 错误可保留原记录供后续重试收尾。
    /// 发布终态仍放在成功完成之后，避免失败被误记为已收尾。
    ///
    /// @author kongweiguang
    fn remove_and_finish(
        &self,
        session_id: &SessionId,
        deadline: Instant,
    ) -> AppResult<Option<SessionLogState>> {
        let recording = self.recording(session_id)?;
        let Some(recording) = recording else {
            return Ok(None);
        };
        let state = finish_recording(&recording, deadline)?;
        self.publish_completed_state(session_id, &recording, state.clone())?;
        Ok(Some(state))
    }

    /// 只有仍占据当前 session 槽位的同一 generation 才能发布终态并退出活动表。
    fn publish_completed_state(
        &self,
        session_id: &SessionId,
        recording: &Arc<Recording>,
        state: SessionLogState,
    ) -> AppResult<()> {
        let mut recordings = self
            .inner
            .recordings
            .lock()
            .map_err(|_| AppError::infrastructure("终端日志状态锁不可用"))?;
        let is_current = recordings.get(session_id).is_some_and(|current| {
            current.recording_id == recording.recording_id && Arc::ptr_eq(current, recording)
        });
        if !is_current {
            return Ok(());
        }
        let mut completed = self
            .inner
            .completed_states
            .lock()
            .map_err(|_| AppError::infrastructure("终端日志状态锁不可用"))?;
        completed.remember(session_id.clone(), recording.recording_id, state);
        recordings.remove(session_id);
        Ok(())
    }

    /// 为成功构造的录制分配进程内 generation，溢出时 fail closed 而不复用旧 ID。
    fn reserve_recording_id(&self) -> AppResult<u64> {
        self.inner
            .next_recording_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| AppError::infrastructure("终端日志录制标识已耗尽"))
    }

    /// 读取 recorder handle；锁只保护 map 查找，不覆盖慢速 writer 生命周期。
    fn recording(&self, session_id: &SessionId) -> AppResult<Option<Arc<Recording>>> {
        self.inner
            .recordings
            .lock()
            .map(|recordings| recordings.get(session_id).cloned())
            .map_err(|_| AppError::infrastructure("终端日志状态锁不可用"))
    }

    /// 查询最近成功或失败的终态；没有历史记录时由调用方返回 legacy inactive。
    fn completed_state(&self, session_id: &SessionId) -> AppResult<Option<SessionLogState>> {
        self.inner
            .completed_states
            .lock()
            .map(|completed| completed.get(session_id))
            .map_err(|_| AppError::infrastructure("终端日志状态锁不可用"))
    }

    /// 关闭指定录制并回传最后状态；MCP stop 会把截断转成明确 IO 错误。
    fn stop_sync(
        &self,
        cancellation: CancellationToken,
        session_id: SessionId,
        deadline: Instant,
        report_truncation: bool,
    ) -> AppResult<SessionLogState> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("停止终端日志已取消"));
        }
        let _operation = self
            .inner
            .operation_lock
            .lock()
            .map_err(|_| AppError::infrastructure("终端日志状态锁不可用"))?;
        let state = match self.remove_and_finish(&session_id, deadline)? {
            Some(state) => state,
            None => self
                .completed_state(&session_id)?
                .unwrap_or_else(SessionLogState::inactive),
        };
        if report_truncation && state.truncated {
            return Err(AppError::infrastructure("终端日志写入失败，日志内容已截断"));
        }
        Ok(state)
    }

    /// 先关闭所有队列入口再逐个 join，单个 writer 超时不能令其它 writer 被跳过。
    fn shutdown_sync(&self, deadline: Instant) -> AppResult<()> {
        let _operation = self
            .inner
            .operation_lock
            .lock()
            .map_err(|_| AppError::infrastructure("终端日志状态锁不可用"))?;
        let recordings = self
            .inner
            .recordings
            .lock()
            .map_err(|_| AppError::infrastructure("终端日志状态锁不可用"))?
            .iter()
            .map(|(session_id, recording)| (session_id.clone(), Arc::clone(recording)))
            .collect::<Vec<_>>();

        let mut first_error = None;
        for (_, recording) in &recordings {
            if let Err(error) = close_recording_sender(recording) {
                first_error.get_or_insert(error);
            }
        }
        for (session_id, _) in recordings {
            if let Err(error) = self.remove_and_finish(&session_id, deadline) {
                first_error.get_or_insert(error);
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }
}

#[async_trait]
impl SessionLogRepository for SessionLogStore {
    /// 创建一份只从 start 之后收取的新输出日志；取消发生在发布状态前才中止创建。
    async fn start(
        &self,
        cancellation: CancellationToken,
        request: SessionLogStartRequest,
    ) -> AppResult<SessionLogState> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("启动终端日志已取消"));
        }
        let store = self.clone();
        task::spawn_blocking(move || store.start_sync(cancellation, request))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("启动终端日志任务失败", source)
            })?
    }

    /// 排空队列后停止日志；启动关闭阶段后会完成 flush，不因后续取消伪报未提交。
    async fn stop(
        &self,
        cancellation: CancellationToken,
        session_id: SessionId,
    ) -> AppResult<SessionLogState> {
        let store = self.clone();
        task::spawn_blocking(move || {
            store.stop_sync(
                cancellation,
                session_id,
                Instant::now() + STOP_TIMEOUT,
                true,
            )
        })
        .await
        .map_err(|source| AppError::infrastructure_with_source("停止终端日志任务失败", source))?
    }

    /// 返回活动状态或最近一次成功/失败终态；从未录制时保持 legacy inactive 形状。
    async fn state(
        &self,
        cancellation: CancellationToken,
        session_id: SessionId,
    ) -> AppResult<SessionLogState> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("查询终端日志已取消"));
        }
        if let Some(recording) = self.recording(&session_id)? {
            return recording_state(&recording);
        }
        let completed = self.completed_state(&session_id)?;
        // 完成缓存读取期间可能有新 start 发布；二次读取使活动状态优先于旧回执。
        if let Some(recording) = self.recording(&session_id)? {
            return recording_state(&recording);
        }
        Ok(completed.unwrap_or_else(SessionLogState::inactive))
    }
}

#[async_trait]
impl SessionOutputRecorder for SessionLogStore {
    /// reader hook 只执行短时状态锁与 bounded try_send，绝不等待磁盘或队列空间。
    fn try_record_output(
        &self,
        session_id: &SessionId,
        sequence: u64,
        bytes: &[u8],
    ) -> OutputRecordResult {
        self.record_output(session_id, sequence, bytes)
    }

    /// session 关闭等待该日志排空；IO 截断由返回状态显式暴露，避免阻断 PTY 关闭。
    async fn finish_session(
        &self,
        session_id: &SessionId,
        deadline: Instant,
    ) -> AppResult<SessionLogState> {
        let store = self.clone();
        let session_id = session_id.clone();
        task::spawn_blocking(move || {
            store.stop_sync(CancellationToken::new(), session_id, deadline, false)
        })
        .await
        .map_err(|source| AppError::infrastructure_with_source("关闭终端日志任务失败", source))?
    }

    /// 应用关闭时按给定绝对 deadline flush 并 join 当前所有 writer。
    async fn shutdown_until(&self, deadline: Instant) -> AppResult<()> {
        let store = self.clone();
        task::spawn_blocking(move || store.shutdown_sync(deadline))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("关闭终端日志任务失败", source)
            })?
    }
}

/// 根据旧状态 snapshot 克隆轻量 DTO，不暴露 writer 内部锁或输出正文。
fn recording_state(recording: &Recording) -> AppResult<SessionLogState> {
    recording
        .control
        .lock()
        .map(|control| control.state.clone())
        .map_err(|_| AppError::infrastructure("终端日志状态锁不可用"))
}

/// 关闭队列 sender 后排空并 join writer；超时保留 JoinHandle，供后续调用继续等待。
fn finish_recording(recording: &Recording, deadline: Instant) -> AppResult<SessionLogState> {
    let next_sequence = close_recording_sender(recording)?;
    let mut worker_slot = recording
        .worker
        .lock()
        .map_err(|_| AppError::infrastructure("终端日志 writer 状态锁不可用"))?;
    let Some(worker) = worker_slot.as_ref() else {
        return recording_state(recording);
    };
    while !worker.is_finished() {
        if Instant::now() >= deadline {
            return Err(AppError::timeout("终端日志写入与关闭"));
        }
        thread::sleep(Duration::from_millis(2));
    }
    let Some(worker) = worker_slot.take() else {
        return recording_state(recording);
    };
    drop(worker_slot);
    if worker.join().is_err() {
        mark_writer_failure(recording, next_sequence, SessionLogFailureReason::WriterIo);
        return Err(AppError::infrastructure("终端日志 writer 异常退出"));
    }
    recording_state(recording)
}

/// 关闭新输出入口并返回 reader 的下一序号；重复调用可安全重试收尾。
fn close_recording_sender(recording: &Recording) -> AppResult<u64> {
    let (sender, next_sequence) = {
        let mut control = recording
            .control
            .lock()
            .map_err(|_| AppError::infrastructure("终端日志状态锁不可用"))?;
        control.state.active = false;
        (control.sender.take(), control.next_sequence.unwrap_or(1))
    };
    drop(sender);
    Ok(next_sequence)
}

/// 将队列溢出、序号断档或 writer 退出转成可查询的失败状态并关闭发送端。
fn fail_recording(
    control: &mut RecordingControl,
    first_missing_sequence: u64,
    reason: SessionLogFailureReason,
) {
    control.state.active = false;
    control.state.truncated = true;
    control
        .state
        .first_missing_sequence
        .get_or_insert(first_missing_sequence);
    control.state.failure_reason = Some(reason);
    control.sender.take();
}

/// writer 消费有序队列；输出只能在完整行或安全 tail 边界经脱敏后写入。
fn run_writer(file: File, receiver: Receiver<WriterMessage>, recording: Arc<Recording>) {
    let mut pending = Zeroizing::new(Vec::new());
    let mut pending_sequences = VecDeque::<(u64, usize)>::new();
    let mut write_failed = false;
    while let Ok(WriterMessage::Output { sequence, bytes }) = receiver.recv() {
        if !bytes.is_empty() {
            pending_sequences.push_back((sequence, bytes.len()));
            pending.extend_from_slice(bytes.as_slice());
        }
        let Some(flush_end) = redactable_prefix_end(&pending) else {
            if pending.len() > MAX_REDACTION_BUFFER_BYTES {
                let first_missing = pending_sequences
                    .front()
                    .map(|(sequence, _)| *sequence)
                    .unwrap_or(sequence);
                mark_writer_failure(
                    &recording,
                    first_missing,
                    SessionLogFailureReason::RedactionSafetyLimit,
                );
                write_failed = true;
                break;
            }
            continue;
        };
        if let Err(failure_sequence) = write_redacted_prefix(
            &file,
            &recording,
            &mut pending,
            &mut pending_sequences,
            flush_end,
        ) {
            mark_writer_failure(
                &recording,
                failure_sequence,
                SessionLogFailureReason::WriterIo,
            );
            write_failed = true;
            break;
        }
    }

    if !write_failed && !pending.is_empty() {
        let failure_sequence = pending_sequences
            .front()
            .map(|(sequence, _)| *sequence)
            .unwrap_or(1);
        let length = pending.len();
        if write_redacted_prefix(
            &file,
            &recording,
            &mut pending,
            &mut pending_sequences,
            length,
        )
        .is_err()
        {
            mark_writer_failure(
                &recording,
                failure_sequence,
                SessionLogFailureReason::WriterIo,
            );
            write_failed = true;
        }
    }
    if !write_failed {
        if file.sync_data().is_err() {
            let next_sequence = recording
                .control
                .lock()
                .ok()
                .and_then(|control| control.next_sequence)
                .unwrap_or(1);
            mark_writer_failure(&recording, next_sequence, SessionLogFailureReason::WriterIo);
        } else if let Ok(mut control) = recording.control.lock() {
            control.state.active = false;
        }
    }
}

/// 写脱敏后的单段输出，并只在完整写入后推进 bytesWritten 与源序号游标。
fn write_redacted_prefix(
    file: &File,
    recording: &Recording,
    pending: &mut Zeroizing<Vec<u8>>,
    pending_sequences: &mut VecDeque<(u64, usize)>,
    flush_end: usize,
) -> Result<(), u64> {
    let failure_sequence = pending_sequences
        .front()
        .map(|(sequence, _)| *sequence)
        .unwrap_or(1);
    let tail = Zeroizing::new(pending.split_off(flush_end));
    let raw = std::mem::replace(pending, tail);
    // 延迟到完整输出边界再解码，使跨 reader chunk 的 UTF-8 字节合并后再解析。
    let data = Zeroizing::new(String::from_utf8_lossy(&raw).into_owned());
    let (redacted, _) = redact_terminal_text(&data);
    let mut writer = file;
    if writer.write_all(redacted.as_bytes()).is_err() {
        return Err(failure_sequence);
    }
    consume_source_prefix(pending_sequences, flush_end);
    if let Ok(mut control) = recording.control.lock() {
        control.state.bytes_written = control
            .state
            .bytes_written
            .saturating_add(redacted.len() as u64);
    }
    Ok(())
}

/// 从 pending sequence 队列移除已写入字符，保留失败时首个未落盘的原始 PTY 序号。
fn consume_source_prefix(pending_sequences: &mut VecDeque<(u64, usize)>, mut consumed: usize) {
    while consumed > 0 {
        let Some((_, length)) = pending_sequences.front() else {
            break;
        };
        if *length <= consumed {
            consumed -= *length;
            pending_sequences.pop_front();
        } else {
            if let Some((_, length)) = pending_sequences.front_mut() {
                *length -= consumed;
            }
            consumed = 0;
        }
    }
}

/// 将 writer 线程中的失败保存为稳定状态；source 错误不进入公开状态或日志。
fn mark_writer_failure(
    recording: &Recording,
    first_missing_sequence: u64,
    reason: SessionLogFailureReason,
) {
    if let Ok(mut control) = recording.control.lock() {
        fail_recording(&mut control, first_missing_sequence, reason);
    }
}

/// 创建兼容 legacy 命名与头部的日志文件，并在写 header 前收紧文件权限。
fn create_log_file(
    log_root: &Path,
    session_id: &SessionId,
    shell: &str,
    cwd: Option<&str>,
    started_at: &str,
) -> io::Result<PathBuf> {
    let suffix = safe_session_suffix(session_id.as_str());
    for collision in 0..100_u32 {
        let timestamp = if collision == 0 {
            started_at.to_owned()
        } else {
            format!("{started_at}-{collision}")
        };
        let path = log_root.join(format!("session-{timestamp}-{suffix}.log"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        if let Err(error) = set_private_file_permissions(&path) {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        let safe_shell = safe_header_value(shell);
        let safe_cwd = safe_header_value(cwd.unwrap_or("-"));
        let header = format!(
            "Kerminal session log\nsession_id: {}\nshell: {}\ncwd: {}\nstarted_at: {}\n\n",
            session_id.as_str(),
            safe_shell,
            safe_cwd,
            started_at
        );
        let (header, _) = redact_terminal_text(&header);
        if let Err(error) = file
            .write_all(header.as_bytes())
            .and_then(|()| file.sync_all())
        {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        return Ok(path);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "session log filename collisions exceeded the bounded retry count",
    ))
}

/// 拒绝 logs 路径中的符号链接，并逐层创建本模块持有的目录。
fn ensure_directory_tree(root: &Path, destination: &Path) -> io::Result<()> {
    let relative = destination.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "session log directory escaped root",
        )
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "session log path contains a non-directory component",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(error) => return Err(error),
        }
        set_private_directory_permissions(&current)?;
    }
    Ok(())
}

/// Unix 日志目录只允许当前用户访问；Windows 继续沿用应用数据根的当前用户 ACL。
fn set_private_directory_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// 保证独立创建的日志文件以 owner-only 权限保存，避免继承宽松 umask。
fn set_private_file_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// 将 shell/cwd 的控制字符折叠为空格，避免元数据伪造额外头部行。
fn safe_header_value(value: &str) -> String {
    let normalized = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(4_096)
        .collect::<String>();
    if value.chars().count() > 4_096 {
        format!("{normalized}...")
    } else {
        normalized
    }
}

/// 沿用旧版文件名仅含 12 位 ASCII 字母数字的 session 后缀规则。
fn safe_session_suffix(session_id: &str) -> String {
    session_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(12)
        .collect()
}

/// 只把完整行交给凭据正则；按原始字节保留 UTF-8 carry 与 reader 序号边界。
fn redactable_prefix_end(input: &[u8]) -> Option<usize> {
    input
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|index| index + 1)
}

/// 返回与旧 MCP 文件名一致的秒级 UTC Unix 时间戳文本。
fn unix_timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

#[cfg(test)]
#[path = "../tests/support/session_logs_unit.rs"]
mod tests;
