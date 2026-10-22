// @author kongweiguang

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        ConnectionIntent, ExecRequest, HostKeyChallenge, HostKeyPeer, HostKeyTrustReceipt,
        HostKeyTrustRequest, OpenSftpRequest, RemoteConnectionHandle, RemoteConnectionLease,
        RemoteConnectionService, RemoteForwardHandle, RemoteForwardListener, RemoteForwardRequest,
        RemoteShellDialect, SftpChannelHandle, ShellScriptRequest, TerminalOpenRequest,
        TunnelChannelHandle, TunnelRequest,
    },
    operations::PortForwardRuntime,
    sessions::SessionTransport,
};
use kerminal_domain::{
    connections::{ConnectionCapabilities, RemoteProtocol},
    identity::HostId,
    operations::{
        ForwardEndpoint, ForwardKind, PortForwardConfig, ProxyApplyScope, ProxyProtocol,
        RemoteAccessScope,
    },
};
use kerminal_infrastructure::operations::SshPortForwardRuntime;
use tokio::{
    sync::Notify,
    time::{Duration, Instant, timeout},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum AcceptBehavior {
    CancelledEnd,
    UnexpectedEnd,
}

#[derive(Default)]
struct ListenerReceipts {
    accepted: AtomicUsize,
    closed: AtomicUsize,
    lease_closed: AtomicUsize,
    accept_started: Notify,
}

struct FakeRemoteConnections {
    host_id: HostId,
    behavior: AcceptBehavior,
    receipts: Arc<ListenerReceipts>,
}

#[async_trait]
impl RemoteConnectionService for FakeRemoteConnections {
    /// 测试只验证转发数据面生命周期，连接 fake 不访问配置、凭据或网络。
    async fn connect(
        &self,
        host_id: HostId,
        intent: ConnectionIntent,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        if host_id != self.host_id || intent != ConnectionIntent::Tunnel {
            return Err(AppError::invalid("测试转发目标不匹配"));
        }
        Ok(RemoteConnectionLease::new(Arc::new(FakeRemoteHandle {
            host_id: self.host_id.clone(),
            behavior: self.behavior,
            receipts: self.receipts.clone(),
        })))
    }

    /// 此测试不查询主机指纹，避免构造虚假的信任状态。
    async fn inspect_host_key(
        &self,
        _host_id: HostId,
        _peer: HostKeyPeer,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyChallenge> {
        Err(AppError::unsupported("测试不提供主机指纹"))
    }

    /// 此测试不写 known_hosts，也不伪造用户确认。
    async fn trust_host_key(
        &self,
        _request: HostKeyTrustRequest,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyTrustReceipt> {
        Err(AppError::unsupported("测试不提供主机信任"))
    }

    /// 此测试替身未覆盖主机密钥轮换，显式拒绝可避免伪造信任回执。
    ///
    /// @author kongweiguang
    async fn replace_host_key(
        &self,
        _request: kerminal_application::connections::HostKeyRotationRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> kerminal_application::AppResult<kerminal_application::connections::HostKeyRotationReceipt>
    {
        Err(kerminal_application::AppError::unsupported(
            "test fake does not support host-key rotation",
        ))
    }
}

struct FakeRemoteHandle {
    host_id: HostId,
    behavior: AcceptBehavior,
    receipts: Arc<ListenerReceipts>,
}

#[async_trait]
impl RemoteConnectionHandle for FakeRemoteHandle {
    /// 返回测试内稳定目标，阻止 fake lease 被借给其它 host。
    fn host_id(&self) -> &HostId {
        &self.host_id
    }

    /// fake 仅模拟已经过协议能力门禁的 SSH tunnel 连接。
    fn capabilities(&self) -> ConnectionCapabilities {
        ConnectionCapabilities::for_protocol(RemoteProtocol::Ssh)
    }

    /// 转发生命周期不需要 shell 方言，但 lease 接口仍要求返回确定值。
    fn shell_dialect(&self) -> RemoteShellDialect {
        RemoteShellDialect::Posix
    }

    /// 转发测试不创建交互终端。
    async fn open_terminal(
        &self,
        _request: TerminalOpenRequest,
        _cancel: CancellationToken,
    ) -> AppResult<SessionTransport> {
        Err(AppError::unsupported("测试不提供终端"))
    }

    /// 转发测试不执行远程命令。
    async fn exec_stream(
        &self,
        _request: ExecRequest,
        _cancel: CancellationToken,
    ) -> AppResult<kerminal_application::connections::CommandStreamHandle> {
        Err(AppError::unsupported("测试不提供命令流"))
    }

    /// 转发测试不执行原始远程脚本。
    async fn exec_shell(
        &self,
        _request: ShellScriptRequest,
        _cancel: CancellationToken,
    ) -> AppResult<kerminal_application::connections::CommandStreamHandle> {
        Err(AppError::unsupported("测试不提供 shell 命令"))
    }

    /// 转发测试不打开 SFTP channel。
    async fn open_sftp(
        &self,
        _request: OpenSftpRequest,
        _cancel: CancellationToken,
    ) -> AppResult<SftpChannelHandle> {
        Err(AppError::unsupported("测试不提供 SFTP"))
    }

    /// 远端 listener 不会接受客户端，因此不需要建立 direct-tcpip channel。
    async fn open_tunnel(
        &self,
        _request: TunnelRequest,
        _cancel: CancellationToken,
    ) -> AppResult<TunnelChannelHandle> {
        Err(AppError::unsupported("测试不提供 direct-tcpip"))
    }

    /// 创建能按用例等待取消或立即结束的 listener，覆盖 close 与 EOF 的竞态。
    async fn request_remote_forward(
        &self,
        _request: RemoteForwardRequest,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteForwardHandle> {
        Ok(RemoteForwardHandle::new(Box::new(
            FakeRemoteForwardListener {
                behavior: self.behavior,
                receipts: self.receipts.clone(),
            },
        )))
    }

    /// fake peer 的关闭是同步确定的，并留下资源释放回执供断言。
    async fn close_until(&self, _deadline: Instant, _reason: &str) -> AppResult<()> {
        self.receipts.lease_closed.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

struct FakeRemoteForwardListener {
    behavior: AcceptBehavior,
    receipts: Arc<ListenerReceipts>,
}

#[async_trait]
impl RemoteForwardListener for FakeRemoteForwardListener {
    /// 区分 stop 触发的正常 None 与未经取消的意外 EOF。
    async fn accept(
        &mut self,
        cancel: CancellationToken,
    ) -> AppResult<Option<TunnelChannelHandle>> {
        self.receipts.accepted.fetch_add(1, Ordering::AcqRel);
        self.receipts.accept_started.notify_one();
        match self.behavior {
            AcceptBehavior::CancelledEnd => {
                cancel.cancelled().await;
                Ok(None)
            }
            AcceptBehavior::UnexpectedEnd => Ok(None),
        }
    }

    /// listener 关闭回执用于确认 stop 和 EOF 故障都实际释放远端资源。
    async fn close(&mut self) -> AppResult<()> {
        self.receipts.closed.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// 返回非零 loopback 端点，满足应用层对 server bind 回执的校验。
    fn bind_endpoint(&self) -> (&str, u16) {
        ("127.0.0.1", 39_921)
    }
}

/// 构造 RemoteDynamic 配置，让真实 runtime 的 accept/close 任务承载测试竞态。
fn remote_dynamic_config(host_id: HostId) -> PortForwardConfig {
    PortForwardConfig {
        host_id,
        name: "listener-lifecycle-test".to_owned(),
        kind: ForwardKind::RemoteDynamic,
        bind: ForwardEndpoint::new("127.0.0.1", 39_921).expect("valid bind endpoint"),
        destination: None,
        proxy_protocol: Some(ProxyProtocol::Socks5),
        remote_access_scope: Some(RemoteAccessScope::Loopback),
        proxy_apply_scope: ProxyApplyScope::None,
    }
}

/// stop 与 accept 取消同时发生时，None 视为正常收尾并释放 listener 与 lease。
#[tokio::test]
async fn cancelled_remote_listener_end_is_a_clean_stop() {
    let host_id = HostId::new();
    let receipts = Arc::new(ListenerReceipts::default());
    let connections: Arc<dyn RemoteConnectionService> = Arc::new(FakeRemoteConnections {
        host_id: host_id.clone(),
        behavior: AcceptBehavior::CancelledEnd,
        receipts: receipts.clone(),
    });
    let runtime = SshPortForwardRuntime::new(connections);
    let handle = runtime
        .start(
            "cancelled-listener".to_owned(),
            remote_dynamic_config(host_id),
            CancellationToken::new(),
        )
        .await
        .expect("remote listener starts");

    timeout(Duration::from_secs(3), receipts.accept_started.notified())
        .await
        .expect("accept loop entered before stop");

    handle
        .close_until(Instant::now() + Duration::from_secs(3))
        .await
        .expect("cancellation-driven None is a clean stop");

    assert_eq!(receipts.accepted.load(Ordering::Acquire), 1);
    assert_eq!(receipts.closed.load(Ordering::Acquire), 1);
    assert_eq!(receipts.lease_closed.load(Ordering::Acquire), 1);
    assert!(handle.subscribe_failure().borrow().is_none());
}

/// 未取消时的 None 必须保持故障语义，且 listener 与 lease 清理回执仍被执行。
#[tokio::test]
async fn unexpected_remote_listener_end_remains_failure() {
    let host_id = HostId::new();
    let receipts = Arc::new(ListenerReceipts::default());
    let connections: Arc<dyn RemoteConnectionService> = Arc::new(FakeRemoteConnections {
        host_id: host_id.clone(),
        behavior: AcceptBehavior::UnexpectedEnd,
        receipts: receipts.clone(),
    });
    let runtime = SshPortForwardRuntime::new(connections);
    let handle = runtime
        .start(
            "unexpected-listener".to_owned(),
            remote_dynamic_config(host_id),
            CancellationToken::new(),
        )
        .await
        .expect("remote listener starts");
    let mut failure = handle.subscribe_failure();
    timeout(Duration::from_secs(3), failure.changed())
        .await
        .expect("unexpected EOF is published")
        .expect("failure watcher remains open");

    assert_eq!(
        *failure.borrow_and_update(),
        Some(kerminal_application::PublicErrorCode::Infrastructure)
    );
    assert!(
        handle
            .close_until(Instant::now() + Duration::from_secs(3))
            .await
            .is_err(),
        "closing an already-failed listener must retain its failure result"
    );
    assert_eq!(receipts.accepted.load(Ordering::Acquire), 1);
    assert_eq!(receipts.closed.load(Ordering::Acquire), 1);
    assert_eq!(receipts.lease_closed.load(Ordering::Acquire), 1);
}
