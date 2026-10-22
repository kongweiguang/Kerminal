//! 未保存主机连接测试的协议适配器；只复用现有 SSH、终端会话和 RDP 客户端探测。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::probe::{
        ConnectionProbeBackend, ConnectionProbeOutcome, ConnectionProbeTarget, ProbeCredential,
        ProbeCredentialMaterial, ProbeHostKeyChallenge, ProbeHostKeyChanged,
        ProbeHostKeyConfirmation, ProbeVerificationLevel, RdpClientKind, RdpConnectivityProbe,
        RdpLocalAvailability, RdpTcpFailure, RdpTcpReachability,
    },
    connections::{
        ChannelLane, ConnectionIntent, CredentialResolver, HostConnectionRepository,
        HostKeyIdentity, HostKeyPeer, HostKeyStatus, KnownHostsRepository, OpenSftpRequest,
        RemoteConnectionService, ResolvedCredential,
    },
    sessions::{
        DEFAULT_SESSION_CLOSE_BUDGET, SessionOpenCleanupOwner, SessionTransportControl,
        SessionTransportFactory, SessionTransportOpenError,
    },
};
use kerminal_domain::{
    connections::{HostConnectionProfile, HostEndpoint, RemoteProtocol},
    files::RemotePath,
    sessions::{SessionTarget, TelnetSessionTarget, TerminalSize},
};
use russh_sftp::{
    client::{SftpSession, error::Error as SftpError},
    protocol::StatusCode,
};
use tokio::{
    net::TcpStream,
    sync::{Mutex, oneshot},
    time::timeout_at,
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use zeroize::Zeroizing;

use super::rdp::RdpProbeAvailability;
use super::{RusshRemoteConnectionDependencies, RusshRemoteConnectionService, rdp};

/// 保存探测期间需要的适配器和失败清理 owner，不注册应用终端会话。
#[derive(Clone)]
pub struct SystemConnectionProbe {
    credentials: Arc<dyn CredentialResolver>,
    known_hosts: Arc<dyn KnownHostsRepository>,
    transports: Arc<dyn SessionTransportFactory>,
    workers: Arc<ProbeWorkerSupervisor>,
    pending_cleanup: Arc<ProbeCleanupRegistry>,
}

/// 在同一把 intake 锁内登记 worker，保证 shutdown 封口后没有迟到任务。
#[derive(Clone)]
struct ProbeWorkerSupervisor {
    state: Arc<StdMutex<ProbeWorkerState>>,
    tasks: TaskTracker,
}

/// probe 的活动取消令牌只服务于任务协调，不承担连接或持久化资源注册。
#[derive(Default)]
struct ProbeWorkerState {
    accepting: bool,
    next_id: u64,
    active: BTreeMap<u64, CancellationToken>,
}

/// 一次探测共用的操作和资源清理绝对截止时间。
#[derive(Clone, Copy)]
struct ProbeDeadlines {
    operation: tokio::time::Instant,
    cleanup: tokio::time::Instant,
}

impl ProbeDeadlines {
    /// 从一次开始时刻固定总探测时间与有限清理窗口，后续阶段不重置计时。
    ///
    /// @author kongweiguang
    fn from_timeout(timeout: Duration) -> Self {
        let operation = tokio::time::Instant::now() + timeout;
        Self {
            operation,
            cleanup: operation + DEFAULT_SESSION_CLOSE_BUDGET,
        }
    }
}

impl ProbeWorkerSupervisor {
    /// 建立只在 desktop composition 的共享 Tokio runtime 内运行的受监督任务集合。
    ///
    /// @author kongweiguang
    fn new() -> Self {
        Self {
            state: Arc::new(StdMutex::new(ProbeWorkerState {
                accepting: true,
                ..ProbeWorkerState::default()
            })),
            tasks: TaskTracker::new(),
        }
    }

    /// 检查关闭状态、登记取消源并立刻把 future 放进 tracker，避免 spawn/shutdown 竞态。
    ///
    /// @author kongweiguang
    fn spawn<T, F>(
        &self,
        cancellation: CancellationToken,
        future: F,
    ) -> AppResult<oneshot::Receiver<AppResult<T>>>
    where
        T: Send + 'static,
        F: std::future::Future<Output = AppResult<T>> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.accepting {
            return Err(AppError::cancelled("应用正在关闭，拒绝新的连接测试"));
        }
        let worker_id = state.next_id;
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or_else(|| AppError::infrastructure("连接测试 worker 标识耗尽"))?;
        state.active.insert(worker_id, cancellation);

        let supervisor = self.clone();
        self.tasks.spawn(async move {
            let _completion = ProbeWorkerCompletion {
                supervisor,
                worker_id,
            };
            let result = future.await;
            let _ = sender.send(result);
        });
        Ok(receiver)
    }

    /// 关闭 intake 并取消快照内全部活动 worker；重复调用可继续等待上次超时的任务。
    ///
    /// @author kongweiguang
    fn begin_shutdown(&self) {
        let cancellations = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.accepting = false;
            state.active.values().cloned().collect::<Vec<_>>()
        };
        self.tasks.close();
        for cancellation in cancellations {
            cancellation.cancel();
        }
    }

    /// 保留 TaskTracker 作为唯一 join owner，超时只结束本次等待而不丢 worker future。
    ///
    /// @author kongweiguang
    async fn wait_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        timeout_at(deadline, self.tasks.wait())
            .await
            .map_err(|_| AppError::timeout("连接测试 worker 收尾"))
    }

    /// worker 发送结果后移除其取消源；TaskTracker 仍负责确认 task 完全退出。
    ///
    /// @author kongweiguang
    fn finish(&self, worker_id: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active.remove(&worker_id);
    }
}

/// worker future panic 或正常返回时都释放活动取消令牌，TaskTracker 另行保留 join 生命周期。
struct ProbeWorkerCompletion {
    supervisor: ProbeWorkerSupervisor,
    worker_id: u64,
}

impl Drop for ProbeWorkerCompletion {
    /// 用 drop guard 避免 panic 路径将取消令牌留在活动表中。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        self.supervisor.finish(self.worker_id);
    }
}

/// 只保留上次有限清理窗口内仍未关闭的资源，供应用退出时用统一 deadline 收尾。
#[derive(Default)]
struct ProbeCleanupRegistry {
    owners: Mutex<Vec<PendingProbeCleanup>>,
}

/// 探测器能够重试关闭的资源所有者；成功关闭后即从集合移除。
enum PendingProbeCleanup {
    Open(Arc<dyn SessionOpenCleanupOwner>),
    Transport(Arc<dyn SessionTransportControl>),
    Ssh(Arc<RusshRemoteConnectionService>),
}

impl PendingProbeCleanup {
    /// 用应用退出传入的同一绝对 deadline 再次关闭资源，不在 owner 间重置预算。
    async fn close_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        match self {
            Self::Open(owner) => owner.close_until(deadline).await,
            Self::Transport(control) => control.close_until(deadline).await,
            Self::Ssh(service) => service.shutdown_until(deadline).await,
        }
    }
}

impl ProbeCleanupRegistry {
    /// 保留仍持有底层 handle 的 owner，避免超时错误返回后 task/control 一起失去管理者。
    ///
    /// @author kongweiguang
    async fn retain(&self, owner: PendingProbeCleanup) {
        self.owners.lock().await.push(owner);
    }

    /// 应用退出时批量重试尚未释放的 probe 资源，并保留失败项供下一次 shutdown 调用。
    ///
    /// @author kongweiguang
    async fn shutdown_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        let owners = {
            let mut owners = self.owners.lock().await;
            std::mem::take(&mut *owners)
        };
        let mut remaining = Vec::new();
        let mut errors: Option<AppError> = None;
        let mut owners = owners.into_iter();

        while let Some(owner) = owners.next() {
            if tokio::time::Instant::now() >= deadline {
                errors = append_cleanup_error(errors, AppError::timeout("连接测试资源清理"));
                remaining.push(owner);
                remaining.extend(owners);
                break;
            }
            if let Err(error) = owner.close_until(deadline).await {
                errors = append_cleanup_error(errors, error);
                remaining.push(owner);
            }
        }

        let mut shared = self.owners.lock().await;
        remaining.append(&mut *shared);
        *shared = remaining;
        if !shared.is_empty() {
            errors = append_cleanup_error(
                errors,
                AppError::conflict("关闭连接测试期间仍有资源等待清理"),
            );
        }
        errors.map_or(Ok(()), Err)
    }
}

impl SystemConnectionProbe {
    /// 复用 composition root 的 credentials、trust 和 transport factory，避免创建应用会话。
    /// 活动 worker 与遗留清理 owner 分开持有，调用方可单独关闭并排空本适配器。
    ///
    /// @author kongweiguang
    pub fn new(
        credentials: Arc<dyn CredentialResolver>,
        known_hosts: Arc<dyn KnownHostsRepository>,
        transports: Arc<dyn SessionTransportFactory>,
    ) -> Self {
        Self {
            credentials,
            known_hosts,
            transports,
            workers: Arc::new(ProbeWorkerSupervisor::new()),
            pending_cleanup: Arc::new(ProbeCleanupRegistry::default()),
        }
    }

    /// 应用退出时使用共享绝对 deadline 收尾超时探测留下的 owner。
    /// 先封闭并等待 worker，再清理它们可能刚登记的资源，避免 shutdown 后出现迟到 owner。
    ///
    /// @author kongweiguang
    pub async fn shutdown_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        self.workers.begin_shutdown();
        self.workers.wait_until(deadline).await?;
        self.pending_cleanup.shutdown_until(deadline).await
    }

    /// 按目标协议选用唯一适配器；RDP 分别检查本机启动能力和远端 TCP，不读取密码。
    /// 为每次请求固定总探测时限与后续清理上限，取消只打断 I/O 不丢失 worker 所有权。
    ///
    /// @author kongweiguang
    async fn probe_inner(
        &self,
        target: ConnectionProbeTarget,
        caller_cancellation: CancellationToken,
        deadlines: ProbeDeadlines,
    ) -> AppResult<ConnectionProbeOutcome> {
        if caller_cancellation.is_cancelled() {
            return Err(AppError::cancelled("连接测试已取消"));
        }
        let operation_cancellation = caller_cancellation.child_token();
        let operation_token = operation_cancellation.clone();
        let operation = async move {
            match target.profile.protocol {
                RemoteProtocol::Ssh | RemoteProtocol::Sftp => {
                    self.probe_ssh(target, operation_token.clone(), deadlines)
                        .await
                }
                RemoteProtocol::Telnet | RemoteProtocol::Serial => {
                    self.probe_terminal_transport(target, operation_token.clone(), deadlines)
                        .await
                }
                RemoteProtocol::Rdp => {
                    probe_rdp_connectivity(
                        &target.profile.endpoint,
                        &operation_token,
                        deadlines.operation,
                    )
                    .await
                }
            }
        };
        tokio::pin!(operation);
        let result = tokio::select! {
            biased;
            _ = caller_cancellation.cancelled() => {
                operation_cancellation.cancel();
                operation.as_mut().await
            }
            _ = tokio::time::sleep_until(deadlines.operation) => {
                operation_cancellation.cancel();
                operation.as_mut().await
            }
            result = operation.as_mut() => result,
        };

        if caller_cancellation.is_cancelled() {
            Err(AppError::cancelled("连接测试已取消"))
        } else if operation_cancellation.is_cancelled()
            || tokio::time::Instant::now() >= deadlines.operation
        {
            Err(AppError::timeout("连接测试"))
        } else {
            result
        }
    }

    /// 先对所有 route peer 做只读 key 检查，再以精确的一次性 pin 重建临时 SSH service。
    /// 对 SSH profile 先查信任再认证，并在两个 service 阶段复用同一组截止时间。
    ///
    /// @author kongweiguang
    async fn probe_ssh(
        &self,
        mut target: ConnectionProbeTarget,
        cancellation: CancellationToken,
        deadlines: ProbeDeadlines,
    ) -> AppResult<ConnectionProbeOutcome> {
        let credentials: Arc<dyn CredentialResolver> = Arc::new(ProbeCredentialResolver {
            saved: Arc::clone(&self.credentials),
            transient: std::mem::take(&mut target.transient_credentials),
        });
        let hosts: Arc<dyn HostConnectionRepository> = Arc::new(ProbeHostRepository {
            profile: target.profile.clone(),
        });
        let inspector = create_ssh_service(
            Arc::clone(&hosts),
            Arc::clone(&credentials),
            Arc::clone(&self.known_hosts),
        );
        let inspection = await_probe_operation(
            inspect_route(
                inspector.as_ref(),
                &target,
                &cancellation,
                deadlines.operation,
            ),
            &cancellation,
            deadlines.operation,
            "SSH 主机指纹检查",
        )
        .await;
        let inspection_cleanup = self.close_ssh_service(&inspector, deadlines.cleanup).await;
        let inspection = combine_cleanup(inspection, inspection_cleanup)?;
        if let Some(outcome) = inspection {
            return Ok(outcome);
        }
        ensure_probe_active(&cancellation, deadlines.operation)?;

        let known_hosts: Arc<dyn KnownHostsRepository> = match target.host_key_confirmation.clone()
        {
            Some(confirmation) => {
                if ssh_peer_endpoint_count(&target.profile, &confirmation.endpoint) != 1 {
                    return Err(AppError::unsupported(
                        "同一 SSH endpoint 在路由中重复出现，无法安全限定本次指纹确认",
                    ));
                }
                Arc::new(ProbeKnownHostsRepository {
                    saved: Arc::clone(&self.known_hosts),
                    confirmation,
                    consumed: AtomicBool::new(false),
                })
            }
            None => Arc::clone(&self.known_hosts),
        };
        let service = create_ssh_service(hosts, credentials, known_hosts);
        let result = connect_and_check_sftp(
            service.as_ref(),
            &target.profile,
            target.sftp_remote_start_directory.as_ref(),
            &cancellation,
            deadlines,
        )
        .await;
        let shutdown = self.close_ssh_service(&service, deadlines.cleanup).await;
        combine_cleanup(result, shutdown)
    }

    /// 直接复用 composition 注入的 factory，打开临时 transport 并在返回前关闭，不创建 SessionInfo。
    /// 临时 open 不登记会话；transport 或 open 错误 owner 只能按统一清理预算关闭。
    ///
    /// @author kongweiguang
    async fn probe_terminal_transport(
        &self,
        target: ConnectionProbeTarget,
        cancellation: CancellationToken,
        deadlines: ProbeDeadlines,
    ) -> AppResult<ConnectionProbeOutcome> {
        let (session_target, level) = match target.profile.protocol {
            RemoteProtocol::Telnet => (
                SessionTarget::Telnet(TelnetSessionTarget {
                    host: target.profile.endpoint.host,
                    port: target.profile.endpoint.port,
                }),
                ProbeVerificationLevel::TelnetTransportConnected,
            ),
            RemoteProtocol::Serial => (
                SessionTarget::Serial(
                    target
                        .serial
                        .ok_or_else(|| AppError::invalid("串口连接参数不完整"))?,
                ),
                ProbeVerificationLevel::SerialDeviceOpened,
            ),
            _ => return Err(AppError::invalid("终端传输探测协议不匹配")),
        };
        let size = TerminalSize::new(80, 24, 0, 0)
            .map_err(|_| AppError::invalid("连接测试终端尺寸无效"))?;
        ensure_probe_active(&cancellation, deadlines.operation)?;
        let transport = match self
            .transports
            .open(&session_target, size, cancellation.clone())
            .await
        {
            Ok(transport) => transport,
            Err(error) => return self.finish_open_error(error, deadlines.cleanup).await,
        };

        let outcome =
            if cancellation.is_cancelled() || tokio::time::Instant::now() >= deadlines.operation {
                Err(AppError::cancelled("连接测试已取消"))
            } else {
                Ok(ConnectionProbeOutcome::Verified(level))
            };
        let control = Arc::clone(&transport.control);
        let cleanup = self.close_transport(control, deadlines.cleanup).await;
        drop(transport);
        combine_cleanup(outcome, cleanup)
    }

    /// 超时后把 open 错误携带的唯一 owner 留到应用 shutdown，遵守 factory 的清理契约。
    /// Factory 失败若携带资源 owner，就在本次窗口内关闭或转交应用退出清理集合。
    ///
    /// @author kongweiguang
    async fn finish_open_error(
        &self,
        error: SessionTransportOpenError,
        deadline: tokio::time::Instant,
    ) -> AppResult<ConnectionProbeOutcome> {
        let (operation_error, cleanup_owner) = error.into_parts();
        let cleanup = match cleanup_owner {
            Some(owner) => {
                let result = owner.close_until(deadline).await;
                if result.is_err() {
                    self.pending_cleanup
                        .retain(PendingProbeCleanup::Open(owner))
                        .await;
                }
                result
            }
            None => Ok(()),
        };
        combine_cleanup(Err(operation_error), cleanup)
    }

    /// 在固定清理 deadline 内关闭成功 transport；失败时保留 control 供 app shutdown 处理。
    /// 关闭失败的 control 必须继续存活，避免其内部 JoinHandle 随局部 transport 丢失。
    ///
    /// @author kongweiguang
    async fn close_transport(
        &self,
        control: Arc<dyn SessionTransportControl>,
        deadline: tokio::time::Instant,
    ) -> AppResult<()> {
        let result = control.close_until(deadline).await;
        if result.is_err() {
            self.pending_cleanup
                .retain(PendingProbeCleanup::Transport(control))
                .await;
        }
        result
    }

    /// SSH service 关闭失败时保留其 pending cleanup 队列，不丢失后续 shutdown 入口。
    /// 保留 russh service 的 pending peer cleanup，让 composition 在 shutdown 时统一重试。
    ///
    /// @author kongweiguang
    async fn close_ssh_service(
        &self,
        service: &Arc<RusshRemoteConnectionService>,
        deadline: tokio::time::Instant,
    ) -> AppResult<()> {
        let result = service.shutdown_until(deadline).await;
        if result.is_err() {
            self.pending_cleanup
                .retain(PendingProbeCleanup::Ssh(Arc::clone(service)))
                .await;
        }
        result
    }
}

#[async_trait]
impl ConnectionProbeBackend for SystemConnectionProbe {
    /// 把 worker 放进现有 Tokio runtime，使调用方取消或销毁 future 时仍能走清理路径。
    /// 受 supervisor 的 intake gate 与 TaskTracker 共同约束，确保应用 shutdown 可取消和 join。
    ///
    /// @author kongweiguang
    async fn probe(
        &self,
        target: ConnectionProbeTarget,
        cancellation: CancellationToken,
    ) -> AppResult<ConnectionProbeOutcome> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("连接测试已取消"));
        }
        let deadlines = ProbeDeadlines::from_timeout(target.timeout);
        let worker_cancellation = cancellation.child_token();
        let mut cancellation_guard = CancelWorkerOnDrop {
            token: worker_cancellation.clone(),
            completed: false,
        };
        let worker = self.clone();
        let worker_token = worker_cancellation.clone();
        let result_receiver = self.workers.spawn(worker_cancellation, async move {
            worker.probe_inner(target, worker_token, deadlines).await
        })?;
        let result = result_receiver
            .await
            .map_err(|_| AppError::infrastructure("连接测试 worker 意外终止"))?;
        cancellation_guard.completed = true;
        result
    }
}

/// 在 UI future 被销毁时通知已脱离调用栈的 worker 取消，并让 worker 自己完成资源收尾。
struct CancelWorkerOnDrop {
    token: CancellationToken,
    completed: bool,
}

impl Drop for CancelWorkerOnDrop {
    /// 只在调用方提前离开时取消 worker；正常完成后不打断已经返回的结果。
    fn drop(&mut self) {
        if !self.completed {
            self.token.cancel();
        }
    }
}

/// 将主机草稿副本暴露给既有 SSH 连接服务，不接触磁盘主机目录。
struct ProbeHostRepository {
    profile: HostConnectionProfile,
}

#[async_trait]
impl HostConnectionRepository for ProbeHostRepository {
    /// 只返回本次探测的单一 profile，避免临时 HostId 指向其它已保存主机。
    async fn load(
        &self,
        host_id: &kerminal_domain::identity::HostId,
    ) -> AppResult<HostConnectionProfile> {
        if host_id == &self.profile.host_id {
            Ok(self.profile.clone())
        } else {
            Err(AppError::not_found("临时连接目标不存在"))
        }
    }
}

/// 优先解析这次输入的零化材料，其余 opaque refs 仍交给正式 vault resolver。
struct ProbeCredentialResolver {
    saved: Arc<dyn CredentialResolver>,
    transient: Vec<ProbeCredential>,
}

#[async_trait]
impl CredentialResolver for ProbeCredentialResolver {
    /// 只按随机引用匹配本次输入；未匹配时不猜材料，退回原 vault 解析器。
    async fn resolve(
        &self,
        reference: &kerminal_domain::connections::CredentialRef,
        cancel: &CancellationToken,
    ) -> AppResult<ResolvedCredential> {
        if let Some(credential) = self
            .transient
            .iter()
            .find(|credential| credential.reference == *reference)
        {
            return Ok(match &credential.material {
                ProbeCredentialMaterial::Password(secret) => {
                    ResolvedCredential::Password(Zeroizing::new(secret.as_str().to_owned()))
                }
                ProbeCredentialMaterial::PrivateKey(secret) => {
                    ResolvedCredential::PrivateKey(Zeroizing::new(secret.to_vec()))
                }
                ProbeCredentialMaterial::KeyPassphrase(secret) => {
                    ResolvedCredential::KeyPassphrase(Zeroizing::new(secret.as_str().to_owned()))
                }
            });
        }
        self.saved.resolve(reference, cancel).await
    }
}

/// 把 persisted known_hosts 包装成只对一个已确认 endpoint/fingerprint 生效的单次 pin。
struct ProbeKnownHostsRepository {
    saved: Arc<dyn KnownHostsRepository>,
    confirmation: ProbeHostKeyConfirmation,
    consumed: AtomicBool,
}

#[async_trait]
impl KnownHostsRepository for ProbeKnownHostsRepository {
    /// 变化指纹始终阻断；仅未知且完全匹配的指纹可以在一次连接握手中临时视为可信。
    async fn inspect(
        &self,
        endpoint: &HostEndpoint,
        identity: &HostKeyIdentity,
    ) -> AppResult<HostKeyStatus> {
        let status = self.saved.inspect(endpoint, identity).await?;
        if status == HostKeyStatus::Unknown
            && endpoint == &self.confirmation.endpoint
            && identity.fingerprint_sha256 == self.confirmation.fingerprint_sha256
            && !self.consumed.swap(true, Ordering::AcqRel)
        {
            Ok(HostKeyStatus::Known)
        } else {
            Ok(status)
        }
    }

    /// 探测端口不持有持久化信任写权限，任何错误接线都显式失败。
    async fn trust(&self, _endpoint: &HostEndpoint, _identity: &HostKeyIdentity) -> AppResult<()> {
        Err(AppError::unsupported("连接测试不会写入 known_hosts"))
    }

    /// 连通性探测只读取信任状态，不能替用户信任或轮换 known_hosts 中的主机密钥。
    async fn replace(
        &self,
        _endpoint: &HostEndpoint,
        _expected_previous_fingerprint_sha256: &str,
        _identity: &HostKeyIdentity,
    ) -> AppResult<()> {
        Err(AppError::unsupported(
            "连接测试不会修改 known_hosts 信任记录",
        ))
    }
}

/// 构造只持有本次 profile 与应用正式 SSH 端口的 russh service。
fn create_ssh_service(
    hosts: Arc<dyn HostConnectionRepository>,
    credentials: Arc<dyn CredentialResolver>,
    known_hosts: Arc<dyn KnownHostsRepository>,
) -> Arc<RusshRemoteConnectionService> {
    Arc::new(RusshRemoteConnectionService::new(
        RusshRemoteConnectionDependencies {
            hosts,
            credentials,
            known_hosts,
        },
    ))
}

/// 按 proxy → jumps → target 顺序只读检查 SSH host key；未知指纹需同一 peer 的确认。
async fn inspect_route(
    service: &dyn RemoteConnectionService,
    target: &ConnectionProbeTarget,
    cancellation: &CancellationToken,
    operation_deadline: tokio::time::Instant,
) -> AppResult<Option<ConnectionProbeOutcome>> {
    let profile = &target.profile;
    for index in 0..profile.route.jump_hosts.len() {
        ensure_probe_active(cancellation, operation_deadline)?;
        if let Some(outcome) = inspect_peer(
            service,
            profile,
            HostKeyPeer::Jump { index },
            target.host_key_confirmation.as_ref(),
            cancellation,
            operation_deadline,
        )
        .await?
        {
            return Ok(Some(outcome));
        }
    }
    ensure_probe_active(cancellation, operation_deadline)?;
    inspect_peer(
        service,
        profile,
        HostKeyPeer::Target,
        target.host_key_confirmation.as_ref(),
        cancellation,
        operation_deadline,
    )
    .await
}

/// 对一个 peer 生成安全 challenge，绝不因为用户确认而改写 saved known_hosts。
async fn inspect_peer(
    service: &dyn RemoteConnectionService,
    profile: &HostConnectionProfile,
    peer: HostKeyPeer,
    confirmation: Option<&ProbeHostKeyConfirmation>,
    cancellation: &CancellationToken,
    operation_deadline: tokio::time::Instant,
) -> AppResult<Option<ConnectionProbeOutcome>> {
    ensure_probe_active(cancellation, operation_deadline)?;
    let challenge = service
        .inspect_host_key(profile.host_id.clone(), peer.clone(), cancellation.clone())
        .await?;
    ensure_probe_active(cancellation, operation_deadline)?;
    let endpoint = endpoint_for_peer(profile, &peer)?;
    match challenge.status {
        HostKeyStatus::Known => Ok(None),
        HostKeyStatus::Unknown => {
            let confirmed = confirmation.is_some_and(|confirmed| {
                confirmed.peer == peer
                    && confirmed.endpoint == endpoint
                    && confirmed.fingerprint_sha256 == challenge.fingerprint_sha256
            });
            if confirmed {
                Ok(None)
            } else {
                Ok(Some(ConnectionProbeOutcome::RequiresHostKeyTrust(
                    ProbeHostKeyChallenge {
                        peer,
                        endpoint,
                        algorithm: challenge.algorithm,
                        fingerprint_sha256: challenge.fingerprint_sha256,
                    },
                )))
            }
        }
        HostKeyStatus::Changed {
            previous_fingerprint_sha256,
        } => Ok(Some(ConnectionProbeOutcome::HostKeyChanged(
            ProbeHostKeyChanged {
                peer,
                endpoint,
                algorithm: challenge.algorithm,
                previous_fingerprint_sha256,
                fingerprint_sha256: challenge.fingerprint_sha256,
            },
        ))),
    }
}

/// 解析一个 route peer 的真实 endpoint，用于把用户确认绑定到可展示的具体目标。
fn endpoint_for_peer(
    profile: &HostConnectionProfile,
    peer: &HostKeyPeer,
) -> AppResult<HostEndpoint> {
    match peer {
        HostKeyPeer::Target => Ok(profile.endpoint.clone()),
        HostKeyPeer::Jump { index } => profile
            .route
            .jump_hosts
            .get(*index)
            .map(|jump| jump.endpoint.clone())
            .ok_or_else(|| AppError::not_found("SSH 跳板目标不存在")),
    }
}

/// 建立 SSH 认证连接；SFTP-only 目标只读列出配置的起始目录，并逐阶段保留取消与清理 owner。
/// 本函数自己收拢 SFTP session 与连接 lease，避免外层取消选择直接丢弃仍持有资源的 future。
///
/// @author kongweiguang
async fn connect_and_check_sftp(
    service: &dyn RemoteConnectionService,
    profile: &HostConnectionProfile,
    remote_start_directory: Option<&RemotePath>,
    cancellation: &CancellationToken,
    deadlines: ProbeDeadlines,
) -> AppResult<ConnectionProbeOutcome> {
    ensure_probe_active(cancellation, deadlines.operation)?;
    let lease = await_probe_operation(
        service.connect(
            profile.host_id.clone(),
            ConnectionIntent::Sftp,
            cancellation.clone(),
        ),
        cancellation,
        deadlines.operation,
        "SSH/SFTP 认证",
    )
    .await?;
    if profile.protocol != RemoteProtocol::Sftp {
        let result = Ok(ConnectionProbeOutcome::Verified(
            ProbeVerificationLevel::SshAuthenticated,
        ));
        let cleanup = lease
            .close_until(deadlines.cleanup, "connection probe complete")
            .await;
        return combine_cleanup(result, cleanup);
    }

    let channel = match await_probe_operation(
        lease.open_sftp(
            OpenSftpRequest {
                lane: ChannelLane::Bulk,
            },
            cancellation.clone(),
        ),
        cancellation,
        deadlines.operation,
        "SFTP subsystem",
    )
    .await
    {
        Ok(channel) => channel,
        Err(error) => {
            return combine_cleanup(
                Err(error),
                lease
                    .close_until(deadlines.cleanup, "connection probe SFTP setup failed")
                    .await,
            );
        }
    };
    let session = match await_probe_operation(
        async {
            SftpSession::new(channel).await.map_err(|error| {
                AppError::infrastructure_with_source("SFTP subsystem 初始化失败", error)
            })
        },
        cancellation,
        deadlines.operation,
        "SFTP subsystem 初始化",
    )
    .await
    {
        Ok(session) => session,
        Err(error) => {
            return combine_cleanup(
                Err(error),
                lease
                    .close_until(
                        deadlines.cleanup,
                        "connection probe SFTP initialization failed",
                    )
                    .await,
            );
        }
    };
    let directory =
        remote_start_directory.map_or_else(|| "/".to_owned(), |path| path.as_str().to_owned());
    let result = await_probe_operation(
        async {
            session
                .read_dir(directory)
                .await
                .map_err(map_sftp_probe_error)
        },
        cancellation,
        deadlines.operation,
        "SFTP 起始目录读取",
    )
    .await
    .map(|_| ConnectionProbeOutcome::Verified(ProbeVerificationLevel::SftpDirectoryAccessible));
    let session_cleanup = close_probe_sftp_session(&session, deadlines.cleanup).await;
    let result = combine_cleanup(result, session_cleanup);
    let lease_cleanup = lease
        .close_until(deadlines.cleanup, "connection probe complete")
        .await;
    combine_cleanup(result, lease_cleanup)
}

/// 在统一资源回收期限内结束只读探测使用的 SFTP session。
///
/// @author kongweiguang
async fn close_probe_sftp_session(
    session: &SftpSession,
    cleanup_deadline: tokio::time::Instant,
) -> AppResult<()> {
    match timeout_at(cleanup_deadline, session.close()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(AppError::infrastructure_with_source(
            "SFTP 测试通道关闭失败",
            error,
        )),
        Err(_) => Err(AppError::timeout("SFTP 测试通道回收")),
    }
}

/// 把目录存在性与权限错误映射为稳定应用错误，不在公开文案中回显远端路径。
///
/// @author kongweiguang
fn map_sftp_probe_error(error: SftpError) -> AppError {
    match &error {
        SftpError::Status(status) if status.status_code == StatusCode::NoSuchFile => {
            AppError::not_found("SFTP 起始目录不存在")
        }
        SftpError::Status(status) if status.status_code == StatusCode::PermissionDenied => {
            AppError::conflict("SFTP 起始目录不可访问")
        }
        _ => AppError::infrastructure_with_source("SFTP 起始目录读取失败", error),
    }
}

/// 同时报告本机启动能力与真实 TCP 结果，防止本机客户端状态替代远端探测。
///
/// @author kongweiguang
async fn probe_rdp_connectivity(
    endpoint: &HostEndpoint,
    cancellation: &CancellationToken,
    operation_deadline: tokio::time::Instant,
) -> AppResult<ConnectionProbeOutcome> {
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("RDP 连接测试已取消"));
    }
    let local_availability = match rdp::probe_client_availability() {
        Ok(RdpProbeAvailability::WindowsMstsc) => {
            RdpLocalAvailability::ClientAvailable(RdpClientKind::WindowsMstsc)
        }
        Ok(RdpProbeAvailability::LinuxFreeRdp) => {
            RdpLocalAvailability::ClientAvailable(RdpClientKind::LinuxFreeRdp)
        }
        Ok(RdpProbeAvailability::MacSystemLauncher) => {
            RdpLocalAvailability::SystemLauncherAvailable {
                association_verified: false,
            }
        }
        Err(_) => RdpLocalAvailability::Unavailable,
    };
    let remote_tcp = probe_rdp_tcp(endpoint, cancellation, operation_deadline).await?;
    Ok(ConnectionProbeOutcome::RdpConnectivity(
        RdpConnectivityProbe {
            local_availability,
            remote_tcp,
            remote_port: endpoint.port,
        },
    ))
}

/// 使用 Tokio TCP 连接验证目标端口；连接成功后立即关闭 socket，不发送 RDP 数据。
///
/// @author kongweiguang
async fn probe_rdp_tcp(
    endpoint: &HostEndpoint,
    cancellation: &CancellationToken,
    operation_deadline: tokio::time::Instant,
) -> AppResult<RdpTcpReachability> {
    probe_rdp_tcp_with(
        TcpStream::connect((endpoint.host.as_str(), endpoint.port)),
        cancellation,
        operation_deadline,
    )
    .await
}

/// 将连接 future 与同一绝对截止时间和取消令牌绑定，便于 loopback 测试稳定覆盖边界。
///
/// @author kongweiguang
async fn probe_rdp_tcp_with<F>(
    connect: F,
    cancellation: &CancellationToken,
    operation_deadline: tokio::time::Instant,
) -> AppResult<RdpTcpReachability>
where
    F: std::future::Future<Output = std::io::Result<TcpStream>>,
{
    await_probe_operation(
        async move {
            Ok(match connect.await {
                Ok(stream) => {
                    drop(stream);
                    RdpTcpReachability::Reachable
                }
                Err(error) => {
                    RdpTcpReachability::Unreachable(classify_rdp_tcp_failure(error.kind()))
                }
            })
        },
        cancellation,
        operation_deadline,
        "RDP TCP 连接测试",
    )
    .await
}

/// 把 socket 错误归入不含主机名的稳定类别，供 UI 解释可达性但不泄露输入地址。
///
/// @author kongweiguang
fn classify_rdp_tcp_failure(kind: std::io::ErrorKind) -> RdpTcpFailure {
    match kind {
        std::io::ErrorKind::ConnectionRefused => RdpTcpFailure::ConnectionRefused,
        std::io::ErrorKind::TimedOut => RdpTcpFailure::TimedOut,
        _ => RdpTcpFailure::NetworkError,
    }
}

/// 在 SSH 路由中统计相同 endpoint 出现次数，避免缺少 peer 参数的下层 trust port 扩大 pin。
fn ssh_peer_endpoint_count(profile: &HostConnectionProfile, endpoint: &HostEndpoint) -> usize {
    profile
        .route
        .jump_hosts
        .iter()
        .filter(|jump| &jump.endpoint == endpoint)
        .count()
        + usize::from(&profile.endpoint == endpoint)
}

/// 把取消或总 deadline 统一映射为可读错误，避免路由继续启动新的 SSH peer。
///
/// @author kongweiguang
fn ensure_probe_active(
    cancellation: &CancellationToken,
    operation_deadline: tokio::time::Instant,
) -> AppResult<()> {
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("连接测试已取消"));
    }
    if tokio::time::Instant::now() >= operation_deadline {
        return Err(AppError::timeout("连接测试"));
    }
    Ok(())
}

/// 为可安全丢弃的 TCP 阶段和持有清理 owner 的 SSH 阶段统一应用取消与绝对 deadline。
/// Telnet/Serial 的 `factory.open` 必须活到返回 transport 或 cleanup owner，不能在 deadline 丢弃。
///
/// @author kongweiguang
async fn await_probe_operation<T, F>(
    future: F,
    cancellation: &CancellationToken,
    deadline: tokio::time::Instant,
    operation_name: &'static str,
) -> AppResult<T>
where
    F: std::future::Future<Output = AppResult<T>>,
{
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(AppError::cancelled("连接测试已取消")),
        result = timeout_at(deadline, future) => {
            result.unwrap_or_else(|_| Err(AppError::timeout(operation_name)))
        }
    }
}

/// 追加清理错误作为诊断根因，保留首个错误作为稳定的主错误。
///
/// @author kongweiguang
fn append_cleanup_error(primary: Option<AppError>, next: AppError) -> Option<AppError> {
    Some(match primary {
        Some(primary) => primary.with_source(next),
        None => next,
    })
}

/// 保留主要失败原因，同时将资源释放失败挂入脱敏诊断链。
fn combine_cleanup<T>(operation: AppResult<T>, cleanup: AppResult<()>) -> AppResult<T> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(error.with_source(cleanup_error)),
    }
}

impl fmt::Debug for SystemConnectionProbe {
    /// 依赖对象不用于诊断输出，避免连接探测日志携带存储或主机索引信息。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SystemConnectionProbe([redacted dependencies])")
    }
}

#[cfg(test)]
#[path = "../../tests/support/connections_probe.rs"]
mod tests;
