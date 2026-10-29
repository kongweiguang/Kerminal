//! 文件工作区可见的 SSH 路由主机密钥信任用例。
//!
//! @author kongweiguang

use std::sync::Arc;

use kerminal_domain::{
    connections::{HostEndpoint, RemoteProtocol},
    identity::HostId,
};
use tokio_util::sync::CancellationToken;

use super::{
    HostKeyChallenge, HostKeyPeer, HostKeyStatus, HostKeyTrustReceipt, HostKeyTrustRequest,
    RemoteConnectionService, management::HostManagementService,
};
use crate::{AppError, AppResult};

/// 对打开文件工作区时加载的主机快照逐一信任其 SSH 路由节点。
///
/// 使用一次目录快照固定跳板与目标端点，避免中途配置变更导致信任操作转向新目标；
/// 该入口只检查服务器身份，不创建连接或文件会话。
///
/// @author kongweiguang
pub async fn trust_file_host_route_keys(
    hosts: &HostManagementService,
    remote: Arc<dyn RemoteConnectionService>,
    host_id: HostId,
    cancel: CancellationToken,
) -> AppResult<()> {
    if cancel.is_cancelled() {
        return Err(AppError::cancelled("文件主机密钥信任已取消"));
    }

    let host = hosts
        .load_host(&host_id)
        .await?
        .ok_or_else(|| AppError::not_found("文件主机不存在"))?;
    if cancel.is_cancelled() {
        return Err(AppError::cancelled("文件主机密钥信任已取消"));
    }
    if host.id != host_id {
        return Err(AppError::host_key_trust_mismatch());
    }
    if !matches!(host.protocol, RemoteProtocol::Ssh | RemoteProtocol::Sftp) {
        return Err(AppError::unsupported("当前主机协议不支持 SSH 主机密钥信任"));
    }

    for (index, jump) in host.route.jump_hosts.iter().enumerate() {
        trust_route_peer(
            Arc::clone(&remote),
            host_id.clone(),
            HostKeyPeer::Jump { index },
            jump.endpoint.clone(),
            cancel.clone(),
        )
        .await?;
    }

    trust_route_peer(remote, host_id, HostKeyPeer::Target, host.endpoint, cancel).await
}

/// 检查单个路由节点，只信任与已观察端点和指纹完全绑定的密钥。
///
/// 已变化的密钥必须停止处理；写入失败或回执不匹配时读取同一节点，只有精确匹配的
/// Known 状态才能证明信任已落盘。
///
/// @author kongweiguang
async fn trust_route_peer(
    remote: Arc<dyn RemoteConnectionService>,
    host_id: HostId,
    peer: HostKeyPeer,
    endpoint: HostEndpoint,
    cancel: CancellationToken,
) -> AppResult<()> {
    if cancel.is_cancelled() {
        return Err(AppError::cancelled("文件主机密钥信任已取消"));
    }

    let challenge = remote
        .inspect_host_key(host_id.clone(), peer.clone(), cancel.clone())
        .await?;
    if cancel.is_cancelled() {
        return Err(AppError::cancelled("文件主机密钥信任已取消"));
    }
    if !challenge_matches_route(&challenge, &host_id, &peer, &endpoint) {
        return Err(AppError::host_key_trust_mismatch());
    }

    match &challenge.status {
        HostKeyStatus::Known => return Ok(()),
        HostKeyStatus::Changed { .. } => return Err(AppError::host_key_changed()),
        HostKeyStatus::Unknown => {}
    }
    if cancel.is_cancelled() {
        return Err(AppError::cancelled("文件主机密钥信任已取消"));
    }

    let request = HostKeyTrustRequest {
        host_id: host_id.clone(),
        peer: peer.clone(),
        endpoint: endpoint.clone(),
        expected_fingerprint_sha256: challenge.fingerprint_sha256.clone(),
    };
    match remote.trust_host_key(request, cancel).await {
        Ok(receipt) if trust_receipt_matches(&receipt, &challenge) => Ok(()),
        trust_result => {
            let receipt_mismatch = trust_result.is_ok();
            let trust_error = trust_result.err();
            let readback = remote
                .inspect_host_key(host_id.clone(), peer.clone(), CancellationToken::new())
                .await;
            if let Ok(actual) = readback.as_ref()
                && challenge_matches_route(actual, &host_id, &peer, &endpoint)
            {
                if matches!(&actual.status, HostKeyStatus::Changed { .. }) {
                    return Err(AppError::host_key_changed());
                }
                if challenge_is_known_for_route(actual, &challenge) {
                    return Ok(());
                }
            }

            match (receipt_mismatch, trust_error) {
                (true, _) => Err(AppError::host_key_trust_mismatch()),
                (false, Some(error)) => Err(error),
                (false, None) => Err(AppError::host_key_trust_mismatch()),
            }
        }
    }
}

/// 将检查结果绑定到已加载的主机路由，并拒绝身份字段不一致的挑战。
///
/// @author kongweiguang
fn challenge_matches_route(
    challenge: &HostKeyChallenge,
    host_id: &HostId,
    peer: &HostKeyPeer,
    endpoint: &HostEndpoint,
) -> bool {
    challenge.host_id == *host_id
        && challenge.peer == *peer
        && challenge.endpoint == *endpoint
        && challenge.identity.fingerprint_sha256 == challenge.fingerprint_sha256
}

/// 信任回执必须证明与检查结果相同的主机、节点、端点和指纹。
///
/// @author kongweiguang
fn trust_receipt_matches(receipt: &HostKeyTrustReceipt, challenge: &HostKeyChallenge) -> bool {
    receipt.host_id == challenge.host_id
        && receipt.peer == challenge.peer
        && receipt.endpoint == challenge.endpoint
        && receipt.fingerprint_sha256 == challenge.fingerprint_sha256
}

/// 只有完整匹配且状态为 Known 的读回结果可以消除写入结果的不确定性。
///
/// @author kongweiguang
fn challenge_is_known_for_route(actual: &HostKeyChallenge, expected: &HostKeyChallenge) -> bool {
    actual.host_id == expected.host_id
        && actual.peer == expected.peer
        && actual.endpoint == expected.endpoint
        && actual.fingerprint_sha256 == expected.fingerprint_sha256
        && actual.identity.fingerprint_sha256 == expected.fingerprint_sha256
        && actual.status == HostKeyStatus::Known
}
