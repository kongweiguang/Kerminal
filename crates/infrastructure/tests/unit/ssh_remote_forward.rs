//! 固定端口与动态端口在 RFC 4254 / russh 回执边界的内部回归。
//!
//! @author kongweiguang

use super::{
    ForwardRouter, effective_remote_forward_port, is_definitive_remote_forward_request_failure,
};
use kerminal_application::PublicErrorCode;
use tokio::sync::mpsc;

/// 固定端口成功回执没有端口字段时必须回落到原请求值。
///
/// @author kongweiguang
#[test]
fn fixed_port_accepts_empty_or_matching_server_reply() {
    assert_eq!(effective_remote_forward_port(22_222, 0).unwrap(), 22_222);
    assert_eq!(
        effective_remote_forward_port(22_222, 22_222).unwrap(),
        22_222
    );
}

/// 动态请求必须采用服务器分配值，并拒绝零端口或超出 TCP 端口范围的答复。
///
/// @author kongweiguang
#[test]
fn dynamic_port_requires_valid_server_assignment() {
    assert_eq!(effective_remote_forward_port(0, 31_111).unwrap(), 31_111);
    assert!(effective_remote_forward_port(0, 0).is_err());
    assert!(effective_remote_forward_port(0, 65_536).is_err());
}

/// 固定请求收到其它非零端口时必须失败，避免把 listener 错绑到另一个目标。
///
/// @author kongweiguang
#[test]
fn fixed_port_rejects_mismatching_server_assignment() {
    assert!(effective_remote_forward_port(22_222, 22_223).is_err());
}

/// 清理 pending 请求不动 fixed listener；关闭 fixed listener 的墓碑只屏蔽同端口兜底。
///
/// @author kongweiguang
#[test]
fn retired_fixed_port_blocks_pending_fallback_without_clearing_it() {
    let router = ForwardRouter::default();
    let (fixed_sender, _fixed_receiver) = mpsc::channel::<russh::Channel<super::Msg>>(1);
    let (pending_sender, _pending_receiver) = mpsc::channel::<russh::Channel<super::Msg>>(1);
    router.register(22_222, fixed_sender).unwrap();
    router.register_pending(pending_sender).unwrap();

    router.remove_request(0);
    assert!(router.sender(22_222).is_some());
    assert!(router.sender(22_223).is_none());

    let (pending_sender, _pending_receiver) = mpsc::channel::<russh::Channel<super::Msg>>(1);
    router.register_pending(pending_sender).unwrap();

    router.retire(22_222).unwrap();
    assert!(router.sender(22_222).is_none());
    assert!(router.sender(22_223).is_some()); // Other unknown ports still cover dynamic promote race.
}

/// 未知固定申请必须屏蔽旧端口，直到收到取消确认后才允许安全复用。
///
/// @author kongweiguang
#[test]
fn unknown_fixed_request_stays_retired_until_cancel_ack() {
    let router = ForwardRouter::default();
    let (old_sender, _old_receiver) = mpsc::channel::<russh::Channel<super::Msg>>(1);
    router.register(22_222, old_sender).unwrap();

    router.retain_unknown_request(22_222);
    router.remove_request(22_222);
    assert!(router.sender(22_222).is_none());

    let (retry_sender, _retry_receiver) = mpsc::channel::<russh::Channel<super::Msg>>(1);
    assert!(router.register(22_222, retry_sender).is_err());

    // 该调用代表同一 peer 已处理成功 cancel ACK；ACK 前不得提前做这一步。
    router.confirm_cancel(22_222);
    let (reused_sender, _reused_receiver) = mpsc::channel::<russh::Channel<super::Msg>>(1);
    router.register(22_222, reused_sender).unwrap();
    assert!(router.sender(22_222).is_some());
}

/// 动态未知结果阻止新 pending 重试，但保留精确路由；只有 peer 清理才能解除阻塞。
///
/// @author kongweiguang
#[test]
fn unknown_dynamic_request_blocks_retry_until_peer_clear() {
    let router = ForwardRouter::default();
    let (exact_sender, _exact_receiver) = mpsc::channel::<russh::Channel<super::Msg>>(1);
    router.register(22_222, exact_sender).unwrap();
    let (pending_sender, _pending_receiver) = mpsc::channel::<russh::Channel<super::Msg>>(1);
    router.register_pending(pending_sender).unwrap();

    router.retain_unknown_request(0);
    router.remove_request(0);
    router.confirm_cancel(0);

    assert!(router.sender(22_222).is_some());
    assert!(router.sender(22_223).is_none());

    let (retry_sender, _retry_receiver) = mpsc::channel::<russh::Channel<super::Msg>>(1);
    let error = router.register_pending(retry_sender).unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(
        error.public_message(),
        "此前动态转发建立结果未知，请关闭此连接并重新连接后重试"
    );

    let (promoted_sender, _promoted_receiver) = mpsc::channel::<russh::Channel<super::Msg>>(1);
    let error = router.promote_pending(22_224, promoted_sender).unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(
        error.public_message(),
        "此前动态转发建立结果未知，请关闭此连接并重新连接后重试"
    );
    assert!(router.sender(22_222).is_some());
    assert!(router.sender(22_225).is_none());

    router.clear();
    let (new_pending_sender, _new_pending_receiver) =
        mpsc::channel::<russh::Channel<super::Msg>>(1);
    router.register_pending(new_pending_sender).unwrap();
    assert!(router.sender(22_225).is_some());
}

/// RequestDenied 也可能来自畸形成功回执，只有本地未能入队可作为确定失败处理。
///
/// @author kongweiguang
#[test]
fn only_unqueued_remote_forward_request_is_definitive() {
    assert!(!is_definitive_remote_forward_request_failure(
        &russh::Error::RequestDenied
    ));
    assert!(is_definitive_remote_forward_request_failure(
        &russh::Error::SendError
    ));
    assert!(!is_definitive_remote_forward_request_failure(
        &russh::Error::Disconnect
    ));
    assert!(!is_definitive_remote_forward_request_failure(
        &russh::Error::IO(std::io::Error::other("transport result is unknown"))
    ));
}

/// 只有成功 cancel ACK 才释放墓碑；此前 fixed 与 dynamic 都不能抢占该端口。
///
/// @author kongweiguang
#[test]
fn retired_port_can_be_reused_after_confirmed_cancel() {
    let router = ForwardRouter::default();
    router.retire(22_222).unwrap();

    let (fixed_sender, _fixed_receiver) = mpsc::channel::<russh::Channel<super::Msg>>(1);
    assert!(router.register(22_222, fixed_sender).is_err());

    let (pending_sender, _pending_receiver) = mpsc::channel::<russh::Channel<super::Msg>>(1);
    router.register_pending(pending_sender).unwrap();
    let (promoted_sender, _promoted_receiver) = mpsc::channel::<russh::Channel<super::Msg>>(1);
    assert!(router.promote_pending(22_222, promoted_sender).is_err());
    assert!(router.sender(22_222).is_none());
    assert!(router.sender(22_223).is_none()); // A failed promotion consumes only its own pending slot.

    router.confirm_cancel(22_222);
    let (reused_sender, _reused_receiver) = mpsc::channel::<russh::Channel<super::Msg>>(1);
    router.register(22_222, reused_sender).unwrap();
    assert!(router.sender(22_222).is_some());
}

/// 退休端口只接受有效的非零 u16 值，避免无限制扩大墓碑命名空间。
///
/// @author kongweiguang
#[test]
fn retired_port_rejects_values_outside_tcp_port_domain() {
    let router = ForwardRouter::default();

    assert!(router.retire(0).is_err());
    assert!(router.retire(65_536).is_err());
    assert!(router.sender(0).is_none());
    assert!(router.sender(65_536).is_none());
}

/// peer teardown 清空路由和墓碑，释放该 SSH 连接独占的有限端口状态。
///
/// @author kongweiguang
#[test]
fn clearing_peer_router_releases_retired_ports() {
    let router = ForwardRouter::default();
    router.retire(22_222).unwrap();

    let (pending_sender, _pending_receiver) = mpsc::channel::<russh::Channel<super::Msg>>(1);
    router.register_pending(pending_sender).unwrap();
    assert!(router.sender(22_222).is_none());

    router.clear();
    let (reused_sender, _reused_receiver) = mpsc::channel::<russh::Channel<super::Msg>>(1);
    router.register(22_222, reused_sender).unwrap();
    assert!(router.sender(22_222).is_some());
    assert!(router.sender(22_223).is_none());
}
