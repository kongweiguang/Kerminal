//! Russh 运行时资源的同步登记与可重复异步回收。
//!
//! @author kongweiguang

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use kerminal_application::{AppError, AppResult};
use russh::client::Handle;
use tokio::{
    sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{
    HostEndpoint, HostKeyChallenge, HostKeyStatus, RusshHandler, RusshPeerSession,
    make_peer_session, russh_error,
};

const CLEANUP_GRACE: Duration = Duration::from_millis(500);
const MAX_ROUTE_OWNERS: usize = 256;

/// SSH adapter 清理 owner 的只读状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RusshConnectionCleanupStatus {
    /// 是否仍接受新的 SSH 连接请求。
    pub accepting_connections: bool,
    /// 已建立且最后一个 lease/channel owner 尚未释放的连接路由数量。
    pub active_peer_owners: usize,
    /// 活跃或待清理 SSH 路由占用的有界 admission 配额数。
    pub reserved_route_owners: usize,
    /// 等待 join 的 SSH session 数量。
    pub pending_sessions: usize,
    /// 等待握手 task 完成或 join 的数量。
    pub pending_handshakes: usize,
}

/// 持有握手 task 的 JoinHandle，使调用方取消后仍可继续回收。
pub(super) type ConnectTask = JoinHandle<Result<Handle<RusshHandler>, russh::Error>>;

struct PendingState {
    sessions: Vec<Arc<RusshPeerSession>>,
    session_ids: HashSet<usize>,
    session_reservations: HashMap<usize, Arc<RusshCleanupReservation>>,
    handshakes: HashMap<usize, Arc<PendingHandshakeCleanup>>,
}

/// 持有准入配额，直到 route owner 释放且该路由所有异步清理均确认完成。
pub(super) struct RusshCleanupReservation {
    state: StdMutex<ReservationState>,
}

struct ReservationState {
    route_active: bool,
    pending_cleanups: usize,
    permit: Option<OwnedSemaphorePermit>,
}

impl RusshCleanupReservation {
    /// 新增一个已登记的 session/handshake 清理项，阻止 route 配额过早归还。
    fn add_pending_cleanup(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.pending_cleanups += 1;
    }

    /// 清理项从 pending owner 移除后归还其计数；route 未结束时仍保留配额。
    fn complete_pending_cleanup(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.pending_cleanups = state.pending_cleanups.saturating_sub(1);
        Self::release_if_idle(&mut state);
    }

    /// 显式关闭或最后 Drop 完成后关闭 route admission；残留清理由 pending owner 继续计费。
    pub(super) fn finish_route(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.route_active = false;
        Self::release_if_idle(&mut state);
    }

    /// 仅在 route 与清理项都完成后释放 semaphore permit，避免清理 owner 超预算积累。
    fn release_if_idle(state: &mut ReservationState) {
        if !state.route_active && state.pending_cleanups == 0 {
            drop(state.permit.take());
        }
    }
}

/// 只拥有尚未完成的 SSH 清理项，不缓存活跃 peer 或 channel wrapper。
pub(super) struct PendingRusshCleanup {
    state: StdMutex<PendingState>,
    admission: Arc<Semaphore>,
    active_peer_owners: AtomicUsize,
    owner_released: Notify,
}

impl PendingRusshCleanup {
    /// 创建空的清理 owner；活跃连接只在最后一个 lease/channel owner 释放后才登记。
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: StdMutex::new(PendingState {
                sessions: Vec::new(),
                session_ids: HashSet::new(),
                session_reservations: HashMap::new(),
                handshakes: HashMap::new(),
            }),
            admission: Arc::new(Semaphore::new(MAX_ROUTE_OWNERS)),
            active_peer_owners: AtomicUsize::new(0),
            owner_released: Notify::new(),
        })
    }

    /// 返回清理状态快照，不把内部锁或 SSH handle 暴露给 composition root。
    pub(super) fn status(&self, accepting_connections: bool) -> RusshConnectionCleanupStatus {
        self.prune_completed();
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        RusshConnectionCleanupStatus {
            accepting_connections,
            active_peer_owners: self.active_peer_owners.load(Ordering::Acquire),
            reserved_route_owners: MAX_ROUTE_OWNERS - self.admission.available_permits(),
            pending_sessions: state.sessions.len(),
            pending_handshakes: state.handshakes.len(),
        }
    }

    /// 拒绝超出固定预算的新路由，避免待 join 的 SSH task/future 无界积累。
    pub(super) fn reserve(self: &Arc<Self>) -> AppResult<Arc<RusshCleanupReservation>> {
        self.prune_completed();
        let permit = Arc::clone(&self.admission)
            .try_acquire_owned()
            .map_err(|_| AppError::conflict("SSH cleanup owner capacity is exhausted"))?;
        Ok(Arc::new(RusshCleanupReservation {
            state: StdMutex::new(ReservationState {
                route_active: true,
                pending_cleanups: 0,
                permit: Some(permit),
            }),
        }))
    }

    /// 只从 pending store 移除已收到真实 join 确认的 owner，并归还对应 route 配额。
    fn prune_completed(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let completed_session_ids: Vec<_> = state
            .sessions
            .iter()
            .filter(|session| session.disconnected.load(Ordering::Acquire))
            .map(|session| Arc::as_ptr(session) as usize)
            .collect();
        state
            .sessions
            .retain(|session| !session.disconnected.load(Ordering::Acquire));
        for id in completed_session_ids {
            state.session_ids.remove(&id);
            if let Some(reservation) = state.session_reservations.remove(&id) {
                reservation.complete_pending_cleanup();
            }
        }
        let completed_handshake_ids: Vec<_> = state
            .handshakes
            .iter()
            .filter(|(_, handshake)| handshake.is_complete())
            .map(|(id, _)| *id)
            .collect();
        for id in completed_handshake_ids {
            if let Some(handshake) = state.handshakes.remove(&id) {
                handshake._reservation.complete_pending_cleanup();
            }
        }
    }

    /// 记录一条 lease 生命周期，使服务关闭能等待最后一个 channel clone 释放。
    fn owner_created(&self) {
        self.active_peer_owners.fetch_add(1, Ordering::AcqRel);
    }

    /// 先登记未完成 session 再减少 owner 数量，避免 shutdown 越过刚释放的资源。
    fn owner_released(&self) {
        self.active_peer_owners.fetch_sub(1, Ordering::AcqRel);
        self.owner_released.notify_waiters();
    }

    /// 同步取消并保留未完成的路由 session，按 Arc 身份去重。
    pub(super) fn retain_sessions(
        &self,
        sessions: &[Arc<RusshPeerSession>],
        reservation: &Arc<RusshCleanupReservation>,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for session in sessions {
            if session.disconnected.load(Ordering::Acquire) {
                continue;
            }
            session.transport_cancel.cancel();
            let id = Arc::as_ptr(session) as usize;
            if state.session_ids.insert(id) {
                reservation.add_pending_cleanup();
                state.sessions.push(Arc::clone(session));
            }
            state
                .session_reservations
                .entry(id)
                .or_insert_with(|| Arc::clone(reservation));
        }
    }

    /// 调用方取消或 future 被丢弃后，保留仍在运行的握手任务。
    fn retain_handshake(&self, handshake: Arc<PendingHandshakeCleanup>) {
        handshake.cancel();
        if handshake.is_complete() {
            return;
        }
        let id = Arc::as_ptr(&handshake) as usize;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let std::collections::hash_map::Entry::Vacant(entry) = state.handshakes.entry(id) {
            handshake._reservation.add_pending_cleanup();
            entry.insert(handshake);
        }
    }

    /// 等待所有 lease/channel owner 释放，再用同一绝对期限重试握手和 session 清理。
    pub(super) async fn drain_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        while self.active_peer_owners.load(Ordering::Acquire) != 0 {
            let released = self.owner_released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if self.active_peer_owners.load(Ordering::Acquire) == 0 {
                break;
            }
            tokio::time::timeout_at(deadline, released)
                .await
                .map_err(|_| AppError::timeout("SSH lease owners did not quiesce"))?;
        }

        let mut first_error = None;
        let sessions = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .sessions
            .clone();
        for session in sessions.iter().rev() {
            if session.disconnected.load(Ordering::Acquire) {
                continue;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(
                    first_error.unwrap_or_else(|| AppError::timeout("SSH cleanup deadline"))
                );
            }
            let graceful_deadline = deadline.checked_sub(CLEANUP_GRACE).unwrap_or(deadline);
            if let Err(error) = super::disconnect_one_session_until(
                session,
                "Kerminal SSH service shutdown",
                graceful_deadline,
                deadline,
            )
            .await
            {
                first_error.get_or_insert(error);
            }
        }

        let handshakes: Vec<_> = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .handshakes
            .values()
            .cloned()
            .collect();
        for handshake in handshakes {
            if tokio::time::Instant::now() >= deadline {
                return Err(
                    first_error.unwrap_or_else(|| AppError::timeout("SSH cleanup deadline"))
                );
            }
            if let Err(error) = handshake.close_until(deadline).await {
                first_error.get_or_insert(error);
            }
        }

        self.prune_completed();
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !state.sessions.is_empty() || !state.handshakes.is_empty() {
            return Err(
                first_error.unwrap_or_else(|| AppError::timeout("SSH cleanup remains pending"))
            );
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for PendingRusshCleanup {
    /// 最后一个 service owner 异常释放时同步取消底层 I/O；正常路径仍须先调用 shutdown_until 完成 join。
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner());
        for session in &state.sessions {
            session.transport_cancel.cancel();
        }
        for handshake in state.handshakes.values() {
            handshake.cancel();
        }
    }
}

/// 所有 terminal、exec、SFTP 和 tunnel peer clone 共用的最终 Drop owner。
pub(super) struct RusshPeerLifetimeOwner {
    cleanup: Arc<PendingRusshCleanup>,
    reservation: Arc<RusshCleanupReservation>,
    sessions: Vec<Arc<RusshPeerSession>>,
    closed: Arc<AtomicBool>,
    released: AtomicBool,
}

impl RusshPeerLifetimeOwner {
    /// 为每条路由计数一次，并让该 owner 被所有 peer/channel clone 共享。
    pub(super) fn new(
        cleanup: Arc<PendingRusshCleanup>,
        reservation: Arc<RusshCleanupReservation>,
        sessions: Vec<Arc<RusshPeerSession>>,
        closed: Arc<AtomicBool>,
    ) -> Arc<Self> {
        cleanup.owner_created();
        Arc::new(Self {
            cleanup,
            reservation,
            sessions,
            closed,
            released: AtomicBool::new(false),
        })
    }

    /// 只有所有 Russh task handle 确实完成后才释放活跃 owner 计数。
    pub(super) fn mark_closed(&self) {
        if self
            .sessions
            .iter()
            .all(|session| session.disconnected.load(Ordering::Acquire))
        {
            self.closed.store(true, Ordering::Release);
            self.reservation.finish_route();
            self.release_once();
        }
    }

    /// 幂等释放唯一的活跃 peer 计数，支持显式 close 与最后 Drop 两条收尾路径。
    fn release_once(&self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            self.cleanup.owner_released();
        }
    }
}

impl Drop for RusshPeerLifetimeOwner {
    /// 最后一个 clone Drop 时同步取消并转交未完成 session，不创建脱离 owner 的后台任务。
    fn drop(&mut self) {
        if !self
            .sessions
            .iter()
            .all(|session| session.disconnected.load(Ordering::Acquire))
        {
            self.cleanup
                .retain_sessions(&self.sessions, &self.reservation);
        } else {
            self.closed.store(true, Ordering::Release);
        }
        self.reservation.finish_route();
        self.release_once();
    }
}

/// 路由 guard 在握手链组装期间持有 session；失败 Drop 会登记清理并关闭 route owner。
pub(super) struct SessionSetGuard {
    cleanup: Arc<PendingRusshCleanup>,
    reservation: Arc<RusshCleanupReservation>,
    sessions: Vec<Arc<RusshPeerSession>>,
    armed: bool,
}

impl SessionSetGuard {
    /// 建立空路由 guard；Drop 路径只同步登记，不启动脱离 owner 的清理任务。
    pub(super) fn new(
        cleanup: Arc<PendingRusshCleanup>,
        reservation: Arc<RusshCleanupReservation>,
    ) -> Self {
        Self {
            cleanup,
            reservation,
            sessions: Vec::new(),
            armed: true,
        }
    }

    /// 立即接管已有路由，避免后续提前返回丢掉最后的 session owner。
    pub(super) fn with_sessions(
        cleanup: Arc<PendingRusshCleanup>,
        reservation: Arc<RusshCleanupReservation>,
        sessions: Vec<Arc<RusshPeerSession>>,
    ) -> Self {
        Self {
            cleanup,
            reservation,
            sessions,
            armed: true,
        }
    }

    /// 在下一个可能取消路由的 await 前接管已连接 hop。
    pub(super) fn push(&mut self, session: Arc<RusshPeerSession>) {
        self.sessions.push(session);
    }

    /// 按路由顺序借出 session，清理时会反向关闭跳板。
    pub(super) fn sessions(&self) -> &[Arc<RusshPeerSession>] {
        &self.sessions
    }

    /// 将完整路由交给下一 owner，转移期间不留下异步生命周期空档。
    pub(super) fn into_sessions(mut self) -> Vec<Arc<RusshPeerSession>> {
        self.armed = false;
        std::mem::take(&mut self.sessions)
    }

    /// 所有 session task 均已 join 后解除 Drop 清理。
    pub(super) fn disarm(&mut self) {
        if self
            .sessions
            .iter()
            .all(|session| session.disconnected.load(Ordering::Acquire))
        {
            self.armed = false;
            self.reservation.finish_route();
        }
    }
}

impl Drop for SessionSetGuard {
    /// 路由取消时保留所有未完成 Russh handle，不丢弃最后一个 Arc。
    fn drop(&mut self) {
        if self.armed {
            self.cleanup
                .retain_sessions(&self.sessions, &self.reservation);
            self.reservation.finish_route();
        }
    }
}

/// 持有握手 task 的 Tokio JoinHandle，以及成功握手后尚待关闭的 Russh session。
pub(super) struct PendingHandshakeCleanup {
    _reservation: Arc<RusshCleanupReservation>,
    endpoint: HostEndpoint,
    observed: Arc<StdMutex<Option<HostKeyChallenge>>>,
    decision_error: Arc<StdMutex<Option<AppError>>>,
    transport_cancel: CancellationToken,
    state: Mutex<HandshakeState>,
    complete: AtomicBool,
}

enum HandshakeState {
    Running(ConnectTask),
    Connected(Arc<RusshPeerSession>),
    Complete,
}

impl PendingHandshakeCleanup {
    /// 在调用方超时或取消前独占接收 `'static` 握手任务。
    pub(super) fn new(
        reservation: Arc<RusshCleanupReservation>,
        endpoint: HostEndpoint,
        observed: Arc<StdMutex<Option<HostKeyChallenge>>>,
        decision_error: Arc<StdMutex<Option<AppError>>>,
        transport_cancel: CancellationToken,
        task: ConnectTask,
    ) -> Arc<Self> {
        Arc::new(Self {
            _reservation: reservation,
            endpoint,
            observed,
            decision_error,
            transport_cancel,
            state: Mutex::new(HandshakeState::Running(task)),
            complete: AtomicBool::new(false),
        })
    }

    /// 等待握手结果；超时只退出等待，不丢弃 JoinHandle。
    pub(super) async fn session_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> AppResult<Option<Arc<RusshPeerSession>>> {
        let mut state = self.state.lock().await;
        match &mut *state {
            HandshakeState::Connected(session) => return Ok(Some(Arc::clone(session))),
            HandshakeState::Complete => return Ok(None),
            HandshakeState::Running(_) => {}
        }
        let joined = {
            let HandshakeState::Running(task) = &mut *state else {
                unreachable!("handshake state changed while mutex was held")
            };
            match tokio::time::timeout_at(deadline, &mut *task).await {
                Ok(joined) => joined,
                Err(_) => return Err(AppError::timeout("SSH handshake task")),
            }
        };
        match joined {
            Ok(Ok(handle)) => {
                let session = make_peer_session(
                    handle,
                    &self.endpoint,
                    &self.observed,
                    self.transport_cancel.clone(),
                );
                *state = HandshakeState::Connected(Arc::clone(&session));
                Ok(Some(session))
            }
            Ok(Err(error)) => {
                *state = HandshakeState::Complete;
                self.complete.store(true, Ordering::Release);
                Err(self.map_connect_error(error))
            }
            Err(error) => {
                *state = HandshakeState::Complete;
                self.complete.store(true, Ordering::Release);
                Err(AppError::infrastructure_with_source(
                    "SSH handshake task could not be joined",
                    error,
                ))
            }
        }
    }

    /// 登记前先取消 transport；保留的 JoinHandle 仍可由后续 drain join。
    pub(super) fn cancel(&self) {
        self.transport_cancel.cancel();
    }

    /// 判断握手任务及其创建的 Russh session 是否都已结束。
    pub(super) fn is_complete(&self) -> bool {
        self.complete.load(Ordering::Acquire)
    }

    /// 收尾遗弃的握手，并在同一 deadline 内关闭迟到成功的 session。
    async fn close_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        match self.session_until(deadline).await? {
            Some(session) => {
                if session.disconnected.load(Ordering::Acquire) {
                    self.complete.store(true, Ordering::Release);
                    return Ok(());
                }
                let graceful_deadline = deadline.checked_sub(CLEANUP_GRACE).unwrap_or(deadline);
                let result = super::disconnect_one_session_until(
                    &session,
                    "Kerminal abandoned SSH handshake",
                    graceful_deadline,
                    deadline,
                )
                .await;
                if session.disconnected.load(Ordering::Acquire) {
                    self.complete.store(true, Ordering::Release);
                }
                result
            }
            None => Ok(()),
        }
    }

    /// 保留 host-key challenge 与 handler 决策错误的既有分类，不把它们改成通用 SSH 错误。
    fn map_connect_error(&self, error: russh::Error) -> AppError {
        if let Some(error) = self
            .decision_error
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            return error;
        }
        if let Some(challenge) = self
            .observed
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
        {
            return match challenge.status {
                HostKeyStatus::Unknown => AppError::host_key_unknown(),
                HostKeyStatus::Changed { .. } => AppError::host_key_changed(),
                HostKeyStatus::Known => russh_error("SSH key exchange failed", error),
            };
        }
        russh_error("SSH key exchange failed", error)
    }
}

/// 将已取消或被丢弃的连接 future 同步转交给服务 cleanup owner。
pub(super) struct HandshakeRegistrationGuard {
    cleanup: Arc<PendingRusshCleanup>,
    handshake: Arc<PendingHandshakeCleanup>,
    armed: bool,
}

impl HandshakeRegistrationGuard {
    /// 保护从启动握手 task 到把 session 交给路由 owner 之间的生命周期空档。
    pub(super) fn new(
        cleanup: Arc<PendingRusshCleanup>,
        handshake: Arc<PendingHandshakeCleanup>,
    ) -> Self {
        Self {
            cleanup,
            handshake,
            armed: true,
        }
    }

    /// 任务失败或 session 已转交后，解除握手登记 guard。
    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }

    /// 返回超时错误前，把仍运行的握手 task 转给服务清理 owner。
    pub(super) fn retain(&mut self) {
        if self.armed {
            self.cleanup.retain_handshake(Arc::clone(&self.handshake));
            self.armed = false;
        }
    }
}

impl Drop for HandshakeRegistrationGuard {
    /// Rust future 被取消不会停止 russh task，因此同步保留可 join 的 task owner。
    fn drop(&mut self) {
        if self.armed {
            self.cleanup.retain_handshake(Arc::clone(&self.handshake));
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ssh_cleanup.rs"]
mod tests;
