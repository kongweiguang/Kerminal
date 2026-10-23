//! SFTP 传输运行时状态、事件和进度跟踪。
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use tauri::{Emitter, Window};
use tokio::{
    sync::Notify,
    time::{interval, MissedTickBehavior},
};

use crate::{
    error::{AppError, AppResult},
    models::sftp::{SftpTransferStatus, SftpTransferSummary},
};

use super::transfer_io::RecoveryCheckpointHolder;
use super::unix_timestamp_millis;
use super::{unix_timestamp, SftpRuntimeSettings};

mod progress;

const SFTP_TRANSFER_UPDATED_EVENT: &str = "sftp-transfer-updated";
const SFTP_TRANSFER_PROGRESS_EMIT_INTERVAL_MS: u64 = 200;

#[derive(Debug, Clone)]
pub(super) struct TransferTask {
    pub(super) summary: SftpTransferSummary,
    pub(super) cancel_requested: Arc<AtomicBool>,
    /// 唤醒网络等待中的 watchdog；原子标志本身不会唤醒尚未再次 poll 的 I/O future。
    pub(super) cancel_notify: Arc<Notify>,
    /// 任务级断点 holder 在自动恢复和人工 retry 间共享，避免把未确认 partial 长度当作偏移。
    pub(super) recovery: RecoveryCheckpointHolder,
    pub(super) speed: TransferSpeedTracker,
}

#[derive(Debug, Clone)]
pub(super) struct TransferSpeedTracker {
    last_sample_at_ms: u64,
    last_sample_bytes: u64,
    speed_bytes_per_second: u64,
}

impl TransferSpeedTracker {
    pub(super) fn new(started_at_ms: u64) -> Self {
        Self {
            last_sample_at_ms: started_at_ms,
            last_sample_bytes: 0,
            speed_bytes_per_second: 0,
        }
    }

    fn update(&mut self, transferred_bytes: u64, now_ms: u64) -> u64 {
        let elapsed_ms = now_ms.saturating_sub(self.last_sample_at_ms);
        if elapsed_ms < SFTP_TRANSFER_PROGRESS_EMIT_INTERVAL_MS {
            return self.speed_bytes_per_second;
        }
        let delta_bytes = transferred_bytes.saturating_sub(self.last_sample_bytes);
        self.speed_bytes_per_second = delta_bytes.saturating_mul(1000) / elapsed_ms.max(1);
        self.last_sample_at_ms = now_ms;
        self.last_sample_bytes = transferred_bytes;
        self.speed_bytes_per_second
    }

    /// 人工后继任务以继承的确认偏移为速度基线，首个新 ACK 不会虚报旧字节的瞬时速度。
    pub(super) fn reset(&mut self, transferred_bytes: u64, now_ms: u64) {
        self.last_sample_at_ms = now_ms;
        self.last_sample_bytes = transferred_bytes;
        self.speed_bytes_per_second = 0;
    }
}

#[derive(Clone)]
pub(super) struct TransferEventEmitter {
    state: Arc<Mutex<TransferEventEmitterState>>,
    window: Window,
}

impl fmt::Debug for TransferEventEmitter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransferEventEmitter")
            .field("event", &SFTP_TRANSFER_UPDATED_EVENT)
            .finish()
    }
}

#[derive(Debug, Default)]
struct TransferEventEmitterState {
    last_emit_ms: u64,
}

impl TransferEventEmitter {
    pub(super) fn new(window: Window) -> Self {
        Self {
            state: Arc::new(Mutex::new(TransferEventEmitterState::default())),
            window,
        }
    }

    pub(super) fn emit(&self, summary: &SftpTransferSummary, force: bool) {
        let now_ms = unix_timestamp_millis();
        let should_emit = if force {
            true
        } else if let Ok(mut state) = self.state.lock() {
            let due = now_ms.saturating_sub(state.last_emit_ms)
                >= SFTP_TRANSFER_PROGRESS_EMIT_INTERVAL_MS;
            if due {
                state.last_emit_ms = now_ms;
            }
            due
        } else {
            true
        };

        if should_emit {
            if let Ok(mut state) = self.state.lock() {
                state.last_emit_ms = now_ms;
            }
            let _ = self
                .window
                .emit(SFTP_TRANSFER_UPDATED_EVENT, summary.clone());
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct TransferLimiter {
    state: Mutex<TransferLimiterState>,
    notify: Notify,
}

#[derive(Debug, Default)]
struct TransferLimiterState {
    global_running: usize,
    host_running: HashMap<String, usize>,
}

impl TransferLimiter {
    /// 只在真正拿到槽后启动连接；等待阶段持续检查取消且不计入无进度预算。
    pub(super) async fn acquire(
        self: &Arc<Self>,
        host_id: String,
        settings: SftpRuntimeSettings,
        progress: TransferProgress,
    ) -> AppResult<TransferLimitPermit> {
        loop {
            progress.ensure_not_cancelled()?;
            {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| AppError::StateLockPoisoned("sftp transfer limits"))?;
                if state.can_start(&host_id, settings) {
                    state.global_running = state.global_running.saturating_add(1);
                    *state.host_running.entry(host_id.clone()).or_insert(0) += 1;
                    return Ok(TransferLimitPermit {
                        limiter: self.clone(),
                        host_id,
                        transfer_id: progress.transfer_id.clone(),
                    });
                }
            }

            tokio::select! {
                _ = self.notify.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
    }

    /// 跨主机任务一次性获取所需槽，避免先占一个主机再等另一个造成循环等待。
    pub(super) async fn acquire_many(
        self: &Arc<Self>,
        host_ids: Vec<String>,
        settings: SftpRuntimeSettings,
        progress: TransferProgress,
    ) -> AppResult<Vec<TransferLimitPermit>> {
        let mut required_by_host = HashMap::<String, usize>::new();
        for host_id in &host_ids {
            *required_by_host.entry(host_id.clone()).or_insert(0) += 1;
        }
        let required_global = host_ids.len();

        loop {
            progress.ensure_not_cancelled()?;
            {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| AppError::StateLockPoisoned("sftp transfer limits"))?;
                if state.can_start_many(&required_by_host, required_global, settings) {
                    state.global_running = state.global_running.saturating_add(required_global);
                    for (host_id, count) in &required_by_host {
                        *state.host_running.entry(host_id.clone()).or_insert(0) += count;
                    }
                    return Ok(host_ids
                        .into_iter()
                        .map(|host_id| TransferLimitPermit {
                            limiter: self.clone(),
                            host_id,
                            transfer_id: progress.transfer_id.clone(),
                        })
                        .collect());
                }
            }

            tokio::select! {
                _ = self.notify.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
    }
}

impl TransferLimiterState {
    fn can_start(&self, host_id: &str, settings: SftpRuntimeSettings) -> bool {
        let host_running = self.host_running.get(host_id).copied().unwrap_or(0);
        self.global_running < settings.global_transfers && host_running < settings.host_transfers
    }

    fn can_start_many(
        &self,
        required_by_host: &HashMap<String, usize>,
        required_global: usize,
        settings: SftpRuntimeSettings,
    ) -> bool {
        if self.global_running.saturating_add(required_global) > settings.global_transfers {
            return false;
        }
        required_by_host.iter().all(|(host_id, required)| {
            self.host_running
                .get(host_id)
                .copied()
                .unwrap_or(0)
                .saturating_add(*required)
                <= settings.host_transfers
        })
    }
}

#[derive(Debug)]
pub(super) struct TransferLimitPermit {
    limiter: Arc<TransferLimiter>,
    host_id: String,
    transfer_id: Option<String>,
}

impl Drop for TransferLimitPermit {
    /// permit 析构才是实际释放并发槽的边界；日志只记录任务 ID 和释放结果，不写主机或路径。
    fn drop(&mut self) {
        let mut released = false;
        if let Ok(mut state) = self.limiter.state.lock() {
            state.global_running = state.global_running.saturating_sub(1);
            if let Some(host_running) = state.host_running.get_mut(&self.host_id) {
                *host_running = host_running.saturating_sub(1);
                if *host_running == 0 {
                    state.host_running.remove(&self.host_id);
                }
            }
            released = true;
        }
        self.limiter.notify.notify_waiters();
        if let Some(transfer_id) = &self.transfer_id {
            tauri_plugin_log::log::info!(
                "sftp transfer slot release transferId={transfer_id} released={released}"
            );
        }
    }
}

#[derive(Clone)]
pub(super) struct TransferProgress {
    transfer_id: Option<String>,
    transfers: Option<Arc<Mutex<HashMap<String, TransferTask>>>>,
    pub(super) cancel_requested: Arc<AtomicBool>,
    cancel_notify: Arc<Notify>,
    recovery: RecoveryCheckpointHolder,
    idle_timeout_pending: Arc<AtomicBool>,
    commit_outcome_uncertain: Arc<AtomicBool>,
    commit_rollback_confirmed: Arc<AtomicBool>,
    commit_recovery_detail: Arc<Mutex<Option<String>>>,
    last_activity_at_ms: Arc<AtomicU64>,
    diagnostic_operation: Arc<Mutex<&'static str>>,
    event_emitter: Option<TransferEventEmitter>,
}

impl fmt::Debug for TransferProgress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransferProgress")
            .field("transfer_id", &self.transfer_id)
            .field(
                "cancel_requested",
                &self.cancel_requested.load(Ordering::SeqCst),
            )
            .field("event_emitter", &self.event_emitter.is_some())
            .finish()
    }
}

/// 用单个定时器监督后台网络工作；取消提交时保留回执等待窗口，只有超时才视为结果不明。
pub(super) async fn run_with_idle_watchdog<T>(
    progress: &TransferProgress,
    idle_timeout_seconds: u64,
    future: impl std::future::Future<Output = AppResult<T>>,
) -> AppResult<T> {
    let mut future = std::pin::pin!(future);
    let mut ticker = interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            result = &mut future => return result,
            _ = progress.cancel_notified() => {
                progress.log_stall_diagnostic("cancelRequested");
                if commit_is_in_progress(progress) {
                    return wait_for_commit_after_interruption(progress, &mut future).await;
                }
                return Err(AppError::Sftp("传输已取消".to_owned()));
            }
            _ = ticker.tick() => {
                if progress.is_cancelled() {
                    progress.log_stall_diagnostic("cancelRequested");
                    if commit_is_in_progress(progress) {
                        return wait_for_commit_after_interruption(progress, &mut future).await;
                    }
                    return Err(AppError::Sftp("传输已取消".to_owned()));
                }
                progress.mark_waiting_if_stalled(unix_timestamp_millis());
                if progress.idle_timeout_elapsed(idle_timeout_seconds, unix_timestamp_millis()) {
                    progress.log_stall_diagnostic("idleTimeout");
                    if commit_is_in_progress(progress) {
                        return wait_for_commit_after_interruption(progress, &mut future).await;
                    }
                    progress.note_idle_timeout(idle_timeout_seconds);
                    return Err(AppError::Sftp(idle_timeout_message(idle_timeout_seconds)));
                }
            }
        }
    }
}

/// 三种中断分支共用提交确认宽限期，避免 committing future 被直接丢弃而漏掉 unknown 标记。
async fn wait_for_commit_after_interruption<T, F>(
    progress: &TransferProgress,
    future: &mut F,
) -> AppResult<T>
where
    F: std::future::Future<Output = AppResult<T>> + Unpin,
{
    commit_result_after_cancel(
        progress,
        tokio::time::timeout(Duration::from_secs(2), future).await,
    )
}

/// 仅提交阶段需要等待回执；其余阶段可立即取消并保留安全 partial。
fn commit_is_in_progress(progress: &TransferProgress) -> bool {
    progress
        .with_summary(|summary| summary.phase.as_deref() == Some("committing"))
        .unwrap_or(false)
}

/// 已收到的提交错误是确定结果；只有等待窗口耗尽、future 仍未返回时才标记回执不明。
fn commit_result_after_cancel<T>(
    progress: &TransferProgress,
    completion: Result<AppResult<T>, tokio::time::error::Elapsed>,
) -> AppResult<T> {
    match completion {
        Ok(result) => result,
        Err(_) => {
            progress.note_commit_outcome_uncertain();
            Err(AppError::Sftp("提交结果未确认，请核对目标文件".to_owned()))
        }
    }
}

#[cfg(test)]
mod commit_cancel_tests {
    use super::{commit_result_after_cancel, TransferProgress};
    use crate::error::{AppError, AppResult};
    use std::{future::pending, time::Duration};

    /// 确定的服务器提交拒绝必须保留原错误语义，避免误设 CommitUnknown 并封锁人工重试。
    #[tokio::test]
    async fn known_commit_error_after_cancel_is_not_unknown() {
        let progress = TransferProgress::detached();
        let error = AppError::Sftp("服务器明确拒绝提交".to_owned());

        let result: AppResult<()> = commit_result_after_cancel(&progress, Ok(Err(error)));

        assert!(matches!(result, Err(AppError::Sftp(message)) if message == "服务器明确拒绝提交"));
        assert!(!progress.commit_outcome_uncertain());
    }

    /// future 在取消宽限期内没有返回时仍须保守保护，避免重试覆盖未知提交结果。
    #[tokio::test]
    async fn pending_commit_after_cancel_is_unknown() {
        let progress = TransferProgress::detached();
        let timed_out = tokio::time::timeout(Duration::ZERO, pending::<AppResult<()>>()).await;

        let result = commit_result_after_cancel(&progress, timed_out);

        assert!(result.is_err());
        assert!(progress.commit_outcome_uncertain());
    }
}

/// 将无进度阈值转换为稳定、可在浅深色界面与 MCP 中复用的用户可见文案。
fn idle_timeout_message(seconds: u64) -> String {
    if seconds.is_multiple_of(60) {
        format!("网络连续 {} 分钟无响应", seconds / 60)
    } else {
        format!("网络连续 {seconds} 秒无响应")
    }
}

/// 纯时间判断便于覆盖长总时长但持续有进度的状态机边界。
fn idle_timeout_elapsed(last_activity_at_ms: u64, idle_timeout_seconds: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(last_activity_at_ms) >= idle_timeout_seconds.saturating_mul(1000)
}

#[cfg(test)]
mod idle_timeout_tests {
    use super::{idle_timeout_elapsed, idle_timeout_message};

    /// 验证每次确认字节都会重置无进度时钟，因此累计超过一分钟不构成传输总时长限制。
    #[test]
    fn continuous_progress_can_run_past_a_minute_without_idle_failure() {
        let idle_timeout_seconds = 30;
        let mut last_activity_at_ms = 0;

        for now_ms in [29_000, 58_000, 87_000, 116_000, 145_000] {
            assert!(
                !idle_timeout_elapsed(last_activity_at_ms, idle_timeout_seconds, now_ms),
                "byte progress before the threshold must keep a long task alive"
            );
            last_activity_at_ms = now_ms;
        }
    }

    /// 验证 watchdog 只在连续无字节进度达到阈值后触发，并维持脱敏的稳定用户文案。
    #[test]
    fn idle_timeout_requires_a_full_inactive_interval() {
        assert!(!idle_timeout_elapsed(10_000, 180, 189_999));
        assert!(idle_timeout_elapsed(10_000, 180, 190_000));
        assert_eq!(idle_timeout_message(180), "网络连续 3 分钟无响应");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_speed_tracker_samples_bytes_per_second() {
        let mut tracker = TransferSpeedTracker::new(1_000);

        assert_eq!(tracker.update(10_000, 1_100), 0);
        assert_eq!(tracker.update(20_000, 1_250), 80_000);
        assert_eq!(tracker.update(30_000, 1_300), 80_000);
        assert_eq!(tracker.update(50_000, 1_500), 120_000);

        tracker.reset(50_000, 1_500);
        assert_eq!(tracker.update(60_000, 1_750), 40_000);
    }
}
