//! 把真实传输终态与 Agent 自然退出投影为系统通知，并保留可重试的关闭 owner。
//!
//! @author kongweiguang

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    agents::{AgentService, AgentSessionView},
    connections::management::HostManagementService,
    platform::{DesktopNotificationEvent, DesktopNotificationService, WindowVisibility},
    sessions::{SessionFilter, SessionService},
    transfers::{TransferQueueSubscription, TransferService},
};
use kerminal_domain::{
    agents::AgentSessionStatus,
    files::{FileEndpoint, TransferStatus, TransferSummary},
    identity::{AgentSessionId, HostId, SessionId, TransferId},
    sessions::{SessionInfo, SessionLifecycle, SessionOwner},
};
use tokio::{
    runtime::Handle,
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval, timeout_at},
};
use tokio_util::sync::CancellationToken;

const AGENT_SESSION_SCAN_INTERVAL: Duration = Duration::from_millis(500);
const MAX_NOTIFICATION_RETRY_DELAY: Duration = Duration::from_secs(30);

/// 唯一传输监听器；超时后保留 join handle，可用新绝对 deadline 重试关闭。
pub(crate) struct NotificationBridge {
    cancellation: CancellationToken,
    worker: Option<JoinHandle<()>>,
}

impl NotificationBridge {
    /// 先订阅再启动快照读取，避免 composition 返回前启动的传输落在事件订阅空窗。
    ///
    /// @author kongweiguang
    pub(crate) async fn start(
        runtime: Handle,
        transfers: TransferService,
        hosts: HostManagementService,
        sessions: Arc<dyn SessionService>,
        agents: Arc<dyn AgentService>,
        notifications: Arc<DesktopNotificationService>,
    ) -> Self {
        let subscription = transfers.subscribe();
        let initial_agent_sessions = match sessions.list(SessionFilter::default()).await {
            Ok(sessions) => Some(sessions),
            Err(error) => {
                tracing::warn!(
                    event = "INITIAL_AGENT_NOTIFICATION_SNAPSHOT_FAILED",
                    code = error.public_code().as_str()
                );
                None
            }
        };
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let worker = runtime.spawn(run_notification_bridge(
            NotificationBridgeServices {
                transfers,
                hosts,
                sessions,
                agents,
                notifications,
            },
            subscription,
            initial_agent_sessions,
            worker_cancellation,
        ));
        Self {
            cancellation,
            worker: Some(worker),
        }
    }

    /// 停止等待新的 transfer wakeup；当前系统通知发送完成后 worker 会正常退出。
    pub(crate) fn begin_shutdown(&self) {
        self.cancellation.cancel();
    }

    /// 在调用方的绝对 deadline 内 join 同一 worker，超时不丢弃尚未完成的 owner。
    pub(crate) async fn shutdown_until(&mut self, deadline: Instant) -> AppResult<()> {
        self.begin_shutdown();
        let Some(worker) = self.worker.as_mut() else {
            return Ok(());
        };
        match timeout_at(deadline, worker).await {
            Ok(Ok(())) => {
                self.worker.take();
                Ok(())
            }
            Ok(Err(error)) => {
                self.worker.take();
                Err(AppError::infrastructure_with_source(
                    "系统通知事件桥接任务异常终止",
                    error,
                ))
            }
            Err(_) => Err(AppError::timeout("等待系统通知事件桥接关闭")),
        }
    }
}

/// 将通知桥接所需的服务放入私有上下文，避免 worker 签名暴露过多独立依赖。
/// @author kongweiguang
struct NotificationBridgeServices {
    transfers: TransferService,
    hosts: HostManagementService,
    sessions: Arc<dyn SessionService>,
    agents: Arc<dyn AgentService>,
    notifications: Arc<DesktopNotificationService>,
}

/// 消费广播唤醒并从队列权威快照重建终态，不依赖可能合并或丢失的事件载荷。
///
/// @author kongweiguang
async fn run_notification_bridge(
    services: NotificationBridgeServices,
    mut subscription: TransferQueueSubscription,
    initial_agent_sessions: Option<Vec<SessionInfo>>,
    cancellation: CancellationToken,
) {
    let NotificationBridgeServices {
        transfers,
        hosts,
        sessions,
        agents,
        notifications,
    } = services;
    let mut active_since = HashMap::<TransferId, Instant>::new();
    let mut handled = HashSet::<TransferId>::new();
    let mut agent_tracker = AgentNotificationTracker::default();
    if let Some(initial_agent_sessions) = initial_agent_sessions {
        agent_tracker.establish_baseline(&initial_agent_sessions, Instant::now());
    }
    let initial = transfers.list().await;
    let now = Instant::now();
    for summary in &initial {
        if is_terminal(summary.status) {
            handled.insert(summary.id.clone());
        } else {
            active_since.insert(summary.id.clone(), now);
        }
    }

    let mut agent_scan = interval(AGENT_SESSION_SCAN_INTERVAL);
    agent_scan.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // interval 的首个 tick 立即触发；消费它后再按固定周期扫描，启动快照已先作为历史基线。
    agent_scan.tick().await;
    let mut transfer_subscription_open = true;
    loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            _ = agent_scan.tick() => {
                let snapshot = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return,
                    result = sessions.list(SessionFilter::default()) => match result {
                        Ok(snapshot) => snapshot,
                        Err(error) => {
                            tracing::warn!(
                                event = "AGENT_NOTIFICATION_SESSION_SCAN_FAILED",
                                code = error.public_code().as_str()
                            );
                            continue;
                        }
                    },
                };
                deliver_agent_notifications(
                    &sessions,
                    &agents,
                    &notifications,
                    &mut agent_tracker,
                    snapshot,
                    &cancellation,
                ).await;
            }
            wake = subscription.recv(cancellation.child_token()), if transfer_subscription_open => {
                if wake.is_none() {
                    transfer_subscription_open = false;
                    continue;
                }
                deliver_transfer_notifications(
                    &transfers,
                    &hosts,
                    &notifications,
                    &mut active_since,
                    &mut handled,
                    &cancellation,
                ).await;
            }
        }
    }
}

/// 以启动快照排除旧退出记录，只为运行期间观察到的 Agent 自然退出安排通知。
#[derive(Default)]
struct AgentNotificationTracker {
    observations: HashMap<SessionId, AgentObservation>,
    delivered: HashSet<AgentCompletionIdentity>,
    initialized: bool,
}

/// 一次 Agent CLI 运行的身份；稳定 SessionId 与 Agent owner 联合去重。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct AgentCompletionIdentity {
    agent_session_id: AgentSessionId,
    session_id: SessionId,
}

/// 保存开始时间、重试退避和是否已完成，不修改 Agent 自身的生命周期状态。
struct AgentObservation {
    identity: AgentCompletionIdentity,
    started_at: Option<Instant>,
    retry_after: Instant,
    retry_count: u8,
    suppressed: bool,
}

/// 单次真实 Exited 状态及其运行时观测时长。
struct AgentCompletionCandidate {
    identity: AgentCompletionIdentity,
    duration: Duration,
}

impl AgentNotificationTracker {
    /// 把 composition 启动前已经退出的会话设为历史，绝不在启动时补发旧通知。
    ///
    /// @author kongweiguang
    fn establish_baseline(&mut self, sessions: &[SessionInfo], now: Instant) {
        self.initialized = true;
        for session in sessions {
            let Some(agent_session_id) = agent_owner_id(session) else {
                continue;
            };
            let identity = AgentCompletionIdentity {
                agent_session_id: agent_session_id.clone(),
                session_id: session.session_id.clone(),
            };
            let suppressed = session.lifecycle == SessionLifecycle::Exited
                || matches!(
                    session.lifecycle,
                    SessionLifecycle::Closing | SessionLifecycle::Closed | SessionLifecycle::Failed
                );
            if suppressed {
                self.delivered.insert(identity.clone());
            }
            self.observations.insert(
                session.session_id.clone(),
                AgentObservation {
                    identity,
                    started_at: (session.lifecycle == SessionLifecycle::Running).then_some(now),
                    retry_after: now,
                    retry_count: 0,
                    suppressed,
                },
            );
        }
    }

    /// 记录本轮会话快照；历史快照只建基线，运行期 Exited 才产生一次候选。
    ///
    /// @author kongweiguang
    fn observe_snapshot(
        &mut self,
        sessions: &[SessionInfo],
        now: Instant,
    ) -> Vec<AgentCompletionCandidate> {
        let baseline = !self.initialized;
        self.initialized = true;
        let present = sessions
            .iter()
            .filter(|session| agent_owner_id(session).is_some())
            .count();
        let mut current_ids = HashSet::with_capacity(present);
        let mut candidates = Vec::new();

        for session in sessions {
            let Some(agent_session_id) = agent_owner_id(session) else {
                continue;
            };
            let session_id = session.session_id.clone();
            current_ids.insert(session_id.clone());
            let identity = AgentCompletionIdentity {
                agent_session_id: agent_session_id.clone(),
                session_id: session_id.clone(),
            };
            let observation = self
                .observations
                .entry(session_id.clone())
                .or_insert_with(|| AgentObservation {
                    identity: identity.clone(),
                    started_at: (session.lifecycle == SessionLifecycle::Running).then_some(now),
                    retry_after: now,
                    retry_count: 0,
                    suppressed: false,
                });
            if observation.identity != identity {
                self.delivered.remove(&observation.identity);
                *observation = AgentObservation {
                    identity: identity.clone(),
                    started_at: (session.lifecycle == SessionLifecycle::Running).then_some(now),
                    retry_after: now,
                    retry_count: 0,
                    suppressed: false,
                };
            }

            match session.lifecycle {
                SessionLifecycle::Starting => {}
                SessionLifecycle::Running => {
                    observation.started_at.get_or_insert(now);
                }
                SessionLifecycle::Exited if baseline => {
                    observation.suppressed = true;
                    self.delivered.insert(identity);
                }
                SessionLifecycle::Exited if session.exit_status.is_none() => {
                    observation.suppressed = true;
                    self.delivered.insert(identity);
                }
                SessionLifecycle::Exited
                    if !observation.suppressed
                        && !self.delivered.contains(&identity)
                        && now >= observation.retry_after =>
                {
                    candidates.push(AgentCompletionCandidate {
                        identity,
                        duration: observation
                            .started_at
                            .map(|started| now.saturating_duration_since(started))
                            .unwrap_or(Duration::ZERO),
                    });
                }
                SessionLifecycle::Exited => {}
                SessionLifecycle::Closing | SessionLifecycle::Closed | SessionLifecycle::Failed => {
                    observation.suppressed = true;
                    self.delivered.insert(identity);
                }
            }
        }

        self.observations
            .retain(|session_id, _| current_ids.contains(session_id));
        let live_identities = self
            .observations
            .values()
            .map(|observation| observation.identity.clone())
            .collect::<HashSet<_>>();
        self.delivered
            .retain(|identity| live_identities.contains(identity));
        candidates
    }

    /// 只在一次通知 API 返回错误时安排带上限的重试，避免 OS 通知故障导致 500ms 重试风暴。
    ///
    /// @author kongweiguang
    fn back_off_after_failure(&mut self, identity: &AgentCompletionIdentity, now: Instant) {
        let Some(observation) = self.observations.get_mut(&identity.session_id) else {
            return;
        };
        if observation.identity != *identity {
            return;
        }
        observation.retry_count = observation.retry_count.saturating_add(1).min(6);
        let delay_seconds = 1_u64 << observation.retry_count.saturating_sub(1);
        observation.retry_after =
            now + Duration::from_secs(delay_seconds).min(MAX_NOTIFICATION_RETRY_DELAY);
    }

    /// 已提交或按策略抑制的通知都不再补发；只有系统发送错误保留同一候选重试。
    ///
    /// @author kongweiguang
    fn mark_processed(&mut self, identity: &AgentCompletionIdentity) {
        if let Some(observation) = self.observations.get_mut(&identity.session_id)
            && observation.identity == *identity
        {
            observation.suppressed = true;
            self.delivered.insert(identity.clone());
        }
    }
}

/// 从 runtime owner 读取 Agent 身份，忽略 UI、MCP 与其它后台会话。
///
/// @author kongweiguang
fn agent_owner_id(session: &SessionInfo) -> Option<&AgentSessionId> {
    match &session.owner {
        SessionOwner::Agent { agent_session_id } => Some(agent_session_id),
        SessionOwner::Ui { .. } | SessionOwner::Mcp | SessionOwner::Service { .. } => None,
    }
}

/// 仅把仍 Active、仍指向同一个 Exited CLI 的 Agent 映射为旧版 AgentFinished 通知。
///
/// @author kongweiguang
fn agent_notification_for(
    identity: &AgentCompletionIdentity,
    view: &AgentSessionView,
    observed: &SessionInfo,
    duration: Duration,
) -> Option<DesktopNotificationEvent> {
    // AgentSessionService 会把自然退出的 Active 记录推进到 Stale；只有 view 仍指向
    // 该自然退出的同一个 runtime，才允许 Stale 进入通知，避免旧进程影响新运行。
    let view_terminal = view.terminal.as_ref()?;
    if !matches!(
        view.session.status,
        AgentSessionStatus::Active | AgentSessionStatus::Stale
    ) || view.resume_recovery.is_some()
        || view.session.agent_session_id != identity.agent_session_id
        || view_terminal.session_id != identity.session_id
    {
        return None;
    }
    let expected_owner = SessionOwner::Agent {
        agent_session_id: identity.agent_session_id.clone(),
    };
    if observed.lifecycle != SessionLifecycle::Exited
        || observed.exit_status.is_none()
        || observed.owner != expected_owner
        || view_terminal.lifecycle != SessionLifecycle::Exited
        || view_terminal.exit_status.is_none()
        || view_terminal.owner != expected_owner
        || observed.session_id != identity.session_id
    {
        return None;
    }
    let exit_code = observed.exit_status.as_ref()?.code;
    Some(DesktopNotificationEvent::AgentFinished {
        agent_name: notification_agent_name(&view.session.title),
        exit_code,
        duration,
        importance: None,
    })
}

/// 限制用户会话标题的控制字符和长度；凭据与路径正文仍交由统一通知服务脱敏。
///
/// @author kongweiguang
fn notification_agent_name(title: &str) -> String {
    let name = title
        .chars()
        .filter(|character| !character.is_control())
        .take(72)
        .collect::<String>();
    let name = name.trim();
    if name.is_empty() {
        "Agent".to_owned()
    } else {
        name.to_owned()
    }
}

/// 查询 Agent 的权威会话视图并发送真实退出通知；系统故障仅保留重试状态，不改 Agent 结果。
///
/// @author kongweiguang
async fn deliver_agent_notifications(
    sessions: &Arc<dyn SessionService>,
    agents: &Arc<dyn AgentService>,
    notifications: &Arc<DesktopNotificationService>,
    tracker: &mut AgentNotificationTracker,
    snapshot: Vec<SessionInfo>,
    cancellation: &CancellationToken,
) {
    let observed_at = Instant::now();
    let candidates = tracker.observe_snapshot(&snapshot, observed_at);
    for candidate in candidates {
        if cancellation.is_cancelled() {
            return;
        }
        let current = match tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            result = agents.get(
                candidate.identity.agent_session_id.clone(),
                cancellation.child_token(),
            ) => result,
        } {
            Ok(view) => view,
            Err(error) => {
                if cancellation.is_cancelled() {
                    return;
                }
                if error.public_code() == PublicErrorCode::NotFound {
                    tracker.mark_processed(&candidate.identity);
                    continue;
                }
                tracing::warn!(
                    event = "AGENT_COMPLETION_VIEW_FAILED",
                    code = error.public_code().as_str()
                );
                tracker.back_off_after_failure(&candidate.identity, Instant::now());
                continue;
            }
        };
        if cancellation.is_cancelled() {
            return;
        }

        let observed = match tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            result = sessions.list(SessionFilter {
                owner: Some(SessionOwner::Agent {
                    agent_session_id: candidate.identity.agent_session_id.clone(),
                }),
                target: None,
            }) => result,
        } {
            Ok(current) => current
                .into_iter()
                .find(|item| item.session_id == candidate.identity.session_id),
            Err(error) => {
                if cancellation.is_cancelled() {
                    return;
                }
                tracing::warn!(
                    event = "AGENT_COMPLETION_SESSION_REVALIDATION_FAILED",
                    code = error.public_code().as_str()
                );
                tracker.back_off_after_failure(&candidate.identity, Instant::now());
                continue;
            }
        };
        let Some(observed) = observed else {
            tracker.mark_processed(&candidate.identity);
            continue;
        };
        let Some(event) =
            agent_notification_for(&candidate.identity, &current, &observed, candidate.duration)
        else {
            // 归档、关闭、owner 变化或待确认恢复不是自然完成通知的合法来源。
            tracker.mark_processed(&candidate.identity);
            continue;
        };
        let visibility = match notifications.window_visibility() {
            Ok(visibility) => visibility,
            Err(error) => {
                tracing::warn!(
                    event = "WINDOW_VISIBILITY_UNAVAILABLE",
                    code = error.public_code().as_str()
                );
                WindowVisibility::Foreground
            }
        };
        if cancellation.is_cancelled() {
            return;
        }
        match notifications.notify(event, visibility).await {
            Ok(_) => tracker.mark_processed(&candidate.identity),
            Err(error) => {
                tracing::warn!(
                    event = "AGENT_COMPLETION_NOTIFICATION_FAILED",
                    code = error.public_code().as_str()
                );
                tracker.back_off_after_failure(&candidate.identity, Instant::now());
            }
        }
    }
}

/// 从队列权威快照重建 SFTP 终态，广播丢事件时仍不补发历史完成项。
///
/// @author kongweiguang
async fn deliver_transfer_notifications(
    transfers: &TransferService,
    hosts: &HostManagementService,
    notifications: &Arc<DesktopNotificationService>,
    active_since: &mut HashMap<TransferId, Instant>,
    handled: &mut HashSet<TransferId>,
    cancellation: &CancellationToken,
) {
    let summaries = transfers.list().await;
    let current_ids = summaries
        .iter()
        .map(|summary| summary.id.clone())
        .collect::<HashSet<_>>();
    active_since.retain(|id, _| current_ids.contains(id));
    handled.retain(|id| current_ids.contains(id));
    let observed_at = Instant::now();

    for summary in summaries {
        if cancellation.is_cancelled() {
            return;
        }
        if !is_terminal(summary.status) {
            active_since
                .entry(summary.id.clone())
                .or_insert(observed_at);
            continue;
        }
        if summary.cleanup_pending || !handled.insert(summary.id.clone()) {
            continue;
        }

        let Some(host_ids) = sftp_host_ids(&summary) else {
            active_since.remove(&summary.id);
            continue;
        };
        let duration = active_since
            .remove(&summary.id)
            .map(|started| observed_at.saturating_duration_since(started))
            .unwrap_or(Duration::ZERO);
        let host_label = resolve_host_label(hosts, &host_ids).await;
        if cancellation.is_cancelled() {
            return;
        }
        let Some(event) = notification_for(&summary, duration, host_label) else {
            continue;
        };
        let visibility = match notifications.window_visibility() {
            Ok(visibility) => visibility,
            Err(error) => {
                tracing::warn!(
                    event = "WINDOW_VISIBILITY_UNAVAILABLE",
                    code = error.public_code().as_str()
                );
                WindowVisibility::Foreground
            }
        };
        if cancellation.is_cancelled() {
            return;
        }
        if let Err(error) = notifications.notify(event, visibility).await {
            tracing::warn!(
                event = "SFTP_TRANSFER_NOTIFICATION_FAILED",
                code = error.public_code().as_str()
            );
        }
    }
}

/// 队列只以终态通知；取消、跳过和无变化是用户选择或正常结果，不冒充失败。
fn is_terminal(status: TransferStatus) -> bool {
    matches!(
        status,
        TransferStatus::Succeeded
            | TransferStatus::Skipped
            | TransferStatus::Unchanged
            | TransferStatus::Failed
            | TransferStatus::Cancelled
    )
}

/// 只有包含 SSH/SFTP Remote endpoint 且不涉及容器的文件任务才属于旧 SFTP 通知族。
fn sftp_host_ids(summary: &TransferSummary) -> Option<Vec<HostId>> {
    let mut host_ids = Vec::with_capacity(2);
    for endpoint in [&summary.request.source, &summary.request.destination] {
        match endpoint {
            FileEndpoint::Remote { host_id, .. } => {
                if !host_ids.contains(host_id) {
                    host_ids.push(host_id.clone());
                }
            }
            FileEndpoint::Container { .. } => return None,
            FileEndpoint::Local { .. } => {}
        }
    }
    (!host_ids.is_empty()).then_some(host_ids)
}

/// 用 host 配置的显示名组织提示；缺失或损坏的主机配置不阻断结果通知，也不暴露路径。
async fn resolve_host_label(hosts: &HostManagementService, host_ids: &[HostId]) -> Option<String> {
    let mut labels = Vec::with_capacity(host_ids.len());
    for host_id in host_ids {
        match hosts.load_host(host_id).await {
            Ok(Some(host)) if !host.name.trim().is_empty() => labels.push(host.name),
            Ok(Some(_)) | Ok(None) => {}
            Err(error) => tracing::warn!(
                event = "SFTP_NOTIFICATION_HOST_LABEL_FAILED",
                code = error.public_code().as_str()
            ),
        }
    }
    (!labels.is_empty()).then(|| labels.join(" → "))
}

/// 把可通知的终态映射到应用层已有事件，单项任务按一个 SFTP batch 呈现。
fn notification_for(
    summary: &TransferSummary,
    duration: Duration,
    host_label: Option<String>,
) -> Option<DesktopNotificationEvent> {
    match summary.status {
        TransferStatus::Succeeded => Some(DesktopNotificationEvent::SftpBatchCompleted {
            host_label,
            total_count: 1,
            succeeded_count: Some(1),
            failed_count: Some(0),
            duration,
            importance: None,
        }),
        TransferStatus::Failed => Some(DesktopNotificationEvent::SftpTransferFailed {
            host_label,
            failed_count: 1,
            importance: None,
        }),
        TransferStatus::Queued
        | TransferStatus::Connecting
        | TransferStatus::Transferring
        | TransferStatus::WaitingForRecovery
        | TransferStatus::Committing
        | TransferStatus::Cancelling
        | TransferStatus::Skipped
        | TransferStatus::Unchanged
        | TransferStatus::Cancelled => None,
    }
}

#[cfg(test)]
#[path = "../tests/support/notification_bridge_unit.rs"]
mod tests;
