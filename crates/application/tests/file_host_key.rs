//! 文件工作区首次信任的路由绑定、取消和未知结果回读回归。
//!
//! @author kongweiguang

use std::{collections::VecDeque, sync::Arc};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        ConnectionIntent, HostKeyChallenge, HostKeyPeer, HostKeyRotationReceipt,
        HostKeyRotationRequest, HostKeyStatus, HostKeyTrustReceipt, HostKeyTrustRequest,
        RemoteConnectionLease, RemoteConnectionService,
        file_host_key::trust_file_host_route_keys,
        management::{
            HostAuthenticationMode, HostDocumentRevision, HostDraft, HostGroupDeletionReceipt,
            HostGroupDraft, HostGroupSnapshot, HostManagementRepository, HostManagementService,
            HostRouteSettings, ManagedHost,
        },
    },
};
use kerminal_domain::{
    connections::{HostEndpoint, RemoteProtocol, SshAuthentication},
    identity::HostId,
    preferences::ConfigRevision,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

struct StaticHostRepository(ManagedHost);

#[async_trait]
impl HostManagementRepository for StaticHostRepository {
    /// 仅向用例提供指定主机；此测试仓储不允许产生任何配置写入。
    async fn ensure_host_writable(&self) -> AppResult<()> {
        Err(AppError::unsupported("unused test operation"))
    }

    /// 单主机信任用例不依赖目录列表。
    async fn list_hosts(&self) -> AppResult<Vec<ManagedHost>> {
        Err(AppError::unsupported("unused test operation"))
    }

    /// 只有请求 ID 与夹具一致时才返回固定路由快照。
    async fn load_host(&self, host_id: &HostId) -> AppResult<Option<ManagedHost>> {
        Ok((&self.0.id == host_id).then(|| self.0.clone()))
    }

    /// 主机元数据写入不属于本组只读测试。
    async fn save_host(
        &self,
        _host_id: HostId,
        _draft: HostDraft,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ManagedHost> {
        Err(AppError::unsupported("unused test operation"))
    }

    /// 主机删除不属于本组只读测试。
    async fn delete_host(
        &self,
        _host_id: &HostId,
        _expected_revision: &ConfigRevision,
    ) -> AppResult<ManagedHost> {
        Err(AppError::unsupported("unused test operation"))
    }

    /// 路由密钥绑定不依赖主机分组配置。
    async fn load_groups(&self) -> AppResult<HostGroupSnapshot> {
        Err(AppError::unsupported("unused test operation"))
    }

    /// 分组修改不属于该隔离测试仓储。
    async fn save_group(
        &self,
        _draft: HostGroupDraft,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<HostGroupSnapshot> {
        Err(AppError::unsupported("unused test operation"))
    }

    /// 分组迁移不属于该隔离测试仓储。
    async fn delete_group_and_reassign(
        &self,
        _group_id: String,
        _destination_group_id: Option<String>,
        _expected_group_revision: ConfigRevision,
        _expected_hosts: Vec<HostDocumentRevision>,
    ) -> AppResult<HostGroupDeletionReceipt> {
        Err(AppError::unsupported("unused test operation"))
    }
}

struct FakeRemoteState {
    inspections: VecDeque<AppResult<HostKeyChallenge>>,
    trust_result: Option<AppResult<HostKeyTrustReceipt>>,
    inspected_peers: Vec<HostKeyPeer>,
    trust_requests: Vec<HostKeyTrustRequest>,
}

struct FakeRemote(Mutex<FakeRemoteState>);

#[async_trait]
impl RemoteConnectionService for FakeRemote {
    /// 信任用例不应创建终端、SFTP 或其它已认证会话。
    async fn connect(
        &self,
        _host_id: HostId,
        _intent: ConnectionIntent,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        Err(AppError::unsupported("unused test operation"))
    }

    /// 按队列返回挑战，供测试模拟初次检查和精确状态回读。
    async fn inspect_host_key(
        &self,
        _host_id: HostId,
        peer: HostKeyPeer,
        cancel: CancellationToken,
    ) -> AppResult<HostKeyChallenge> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("test inspection cancelled"));
        }
        let mut state = self.0.lock().await;
        state.inspected_peers.push(peer);
        state
            .inspections
            .pop_front()
            .unwrap_or_else(|| Err(AppError::unsupported("no queued host-key challenge")))
    }

    /// 返回预设信任结果并保存请求字段以便断言绑定关系。
    async fn trust_host_key(
        &self,
        request: HostKeyTrustRequest,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyTrustReceipt> {
        let mut state = self.0.lock().await;
        state.trust_requests.push(request);
        state
            .trust_result
            .take()
            .unwrap_or_else(|| Err(AppError::unsupported("no queued trust result")))
    }

    /// 文件工作区信任流程不执行主机密钥轮换。
    async fn replace_host_key(
        &self,
        _request: HostKeyRotationRequest,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyRotationReceipt> {
        Err(AppError::unsupported("unused test operation"))
    }
}

/// 构造最小 SFTP 主机快照；可选字段不参与路由节点绑定。
fn host_snapshot() -> ManagedHost {
    ManagedHost {
        id: HostId::new(),
        group_id: None,
        name: "test host".to_owned(),
        note: None,
        protocol: RemoteProtocol::Sftp,
        endpoint: endpoint("target.example.test", 22),
        authentication: Some(SshAuthentication::Agent),
        authentication_mode: HostAuthenticationMode::Agent,
        rdp_password: None,
        route: HostRouteSettings::default(),
        serial: None,
        tags: Vec::new(),
        is_favorite: false,
        sort_order: 0,
        connect_timeout_ms: 10_000,
        keepalive_interval_ms: None,
        terminal_options: Default::default(),
        sftp_options: Default::default(),
        rdp_display: Default::default(),
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
        revision: ConfigRevision::from_sha256("revision-1".to_owned()),
    }
}

/// 构造测试端点，并保留作为精确路由身份一部分的用户名。
fn endpoint(host: &str, port: u16) -> HostEndpoint {
    HostEndpoint {
        host: host.to_owned(),
        port,
        username: "tester".to_owned(),
    }
}

/// 构造身份字段一致、状态可独立选择的挑战。
fn challenge(
    host_id: HostId,
    peer: HostKeyPeer,
    endpoint: HostEndpoint,
    fingerprint: &str,
    status: HostKeyStatus,
) -> HostKeyChallenge {
    HostKeyChallenge {
        host_id,
        peer,
        endpoint,
        algorithm: "ssh-ed25519".to_owned(),
        fingerprint_sha256: fingerprint.to_owned(),
        identity: kerminal_application::connections::HostKeyIdentity {
            algorithm: "ssh-ed25519".to_owned(),
            fingerprint_sha256: fingerprint.to_owned(),
            public_key_openssh: "ssh-ed25519 AAAATEST".to_owned(),
        },
        status,
    }
}

/// 使用相同的固定主机快照组装应用服务与远端服务替身。
fn services(
    host: ManagedHost,
    inspections: Vec<AppResult<HostKeyChallenge>>,
    trust_result: AppResult<HostKeyTrustReceipt>,
) -> (HostManagementService, Arc<FakeRemote>) {
    let hosts = HostManagementService::new(Arc::new(StaticHostRepository(host)));
    let remote = Arc::new(FakeRemote(Mutex::new(FakeRemoteState {
        inspections: inspections.into(),
        trust_result: Some(trust_result),
        inspected_peers: Vec::new(),
        trust_requests: Vec::new(),
    })));
    (hosts, remote)
}

/// 目标密钥已变化时，在任何信任写入前阻断文件工作区操作。
#[tokio::test]
async fn changed_target_key_is_never_trusted() {
    let host = host_snapshot();
    let host_id = host.id.clone();
    let expected_endpoint = host.endpoint.clone();
    let (hosts, remote) = services(
        host,
        vec![Ok(challenge(
            host_id.clone(),
            HostKeyPeer::Target,
            expected_endpoint,
            "SHA256:new",
            HostKeyStatus::Changed {
                previous_fingerprint_sha256: "SHA256:old".to_owned(),
            },
        ))],
        Err(AppError::unsupported("trust must not be called")),
    );

    let result =
        trust_file_host_route_keys(&hosts, remote.clone(), host_id, CancellationToken::new()).await;

    assert_eq!(
        result.expect_err("changed keys must fail").public_code(),
        kerminal_application::PublicErrorCode::HostKeyChanged
    );
    assert!(remote.0.lock().await.trust_requests.is_empty());
}

/// 信任操作失败后，只有同一节点精确读回 Known 才能判定成功。
#[tokio::test]
async fn failed_trust_is_resolved_by_exact_known_readback() {
    let host = host_snapshot();
    let host_id = host.id.clone();
    let expected_endpoint = host.endpoint.clone();
    let (hosts, remote) = services(
        host,
        vec![
            Ok(challenge(
                host_id.clone(),
                HostKeyPeer::Target,
                expected_endpoint.clone(),
                "SHA256:current",
                HostKeyStatus::Unknown,
            )),
            Ok(challenge(
                host_id.clone(),
                HostKeyPeer::Target,
                expected_endpoint,
                "SHA256:current",
                HostKeyStatus::Known,
            )),
        ],
        Err(AppError::outcome_unknown("test trust result lost")),
    );

    trust_file_host_route_keys(&hosts, remote.clone(), host_id, CancellationToken::new())
        .await
        .expect("exact Known readback proves the trust write completed");

    let state = remote.0.lock().await;
    assert_eq!(state.trust_requests.len(), 1);
    assert_eq!(
        state.inspected_peers,
        [HostKeyPeer::Target, HostKeyPeer::Target]
    );
}

/// 其它端点的读回结果不能把失败写入转换为成功。
#[tokio::test]
async fn mismatched_readback_cannot_prove_trust_success() {
    let host = host_snapshot();
    let host_id = host.id.clone();
    let expected_endpoint = host.endpoint.clone();
    let (hosts, remote) = services(
        host,
        vec![
            Ok(challenge(
                host_id.clone(),
                HostKeyPeer::Target,
                expected_endpoint,
                "SHA256:current",
                HostKeyStatus::Unknown,
            )),
            Ok(challenge(
                host_id.clone(),
                HostKeyPeer::Target,
                endpoint("other.example.test", 22),
                "SHA256:current",
                HostKeyStatus::Known,
            )),
        ],
        Err(AppError::infrastructure("test trust failed")),
    );

    let result =
        trust_file_host_route_keys(&hosts, remote, host_id, CancellationToken::new()).await;

    assert!(result.is_err(), "a different endpoint must not prove trust");
}

/// 写入结果未知后，同一端点报告指纹变化时返回专用的变化错误。
#[tokio::test]
async fn changed_readback_returns_host_key_changed() {
    let host = host_snapshot();
    let host_id = host.id.clone();
    let expected_endpoint = host.endpoint.clone();
    let (hosts, remote) = services(
        host,
        vec![
            Ok(challenge(
                host_id.clone(),
                HostKeyPeer::Target,
                expected_endpoint.clone(),
                "SHA256:current",
                HostKeyStatus::Unknown,
            )),
            Ok(challenge(
                host_id.clone(),
                HostKeyPeer::Target,
                expected_endpoint,
                "SHA256:rotated",
                HostKeyStatus::Changed {
                    previous_fingerprint_sha256: "SHA256:current".to_owned(),
                },
            )),
        ],
        Err(AppError::outcome_unknown("test trust result lost")),
    );

    let result =
        trust_file_host_route_keys(&hosts, remote, host_id, CancellationToken::new()).await;

    assert_eq!(
        result
            .expect_err("changed readback must block setup")
            .public_code(),
        kerminal_application::PublicErrorCode::HostKeyChanged
    );
}

/// 挑战端点与加载的文件路由不同时，必须在信任写入前拒绝。
#[tokio::test]
async fn challenge_must_match_loaded_route_endpoint() {
    let host = host_snapshot();
    let host_id = host.id.clone();
    let (hosts, remote) = services(
        host,
        vec![Ok(challenge(
            host_id.clone(),
            HostKeyPeer::Target,
            endpoint("stale.example.test", 22),
            "SHA256:current",
            HostKeyStatus::Unknown,
        ))],
        Err(AppError::unsupported("trust must not be called")),
    );

    let result =
        trust_file_host_route_keys(&hosts, remote.clone(), host_id, CancellationToken::new()).await;

    assert_eq!(
        result
            .expect_err("stale route challenge must fail")
            .public_code(),
        kerminal_application::PublicErrorCode::HostKeyTrustMismatch
    );
    assert!(remote.0.lock().await.trust_requests.is_empty());
}

/// 请求已取消时，应在读取或检查主机路由前退出。
#[tokio::test]
async fn cancelled_request_does_not_inspect_or_trust() {
    let host = host_snapshot();
    let host_id = host.id.clone();
    let (hosts, remote) = services(
        host,
        Vec::new(),
        Err(AppError::unsupported("trust must not be called")),
    );
    let cancel = CancellationToken::new();
    cancel.cancel();

    let result = trust_file_host_route_keys(&hosts, remote.clone(), host_id, cancel).await;

    assert_eq!(
        result
            .expect_err("cancelled request must fail")
            .public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
    let state = remote.0.lock().await;
    assert!(state.inspected_peers.is_empty());
    assert!(state.trust_requests.is_empty());
}
