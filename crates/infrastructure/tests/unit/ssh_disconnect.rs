//! Russh 主动断开完成态分类的私有单元测试。
//!
//! @author kongweiguang

use super::{
    CancellablePeerStream, is_confirmed_remote_transport_end, is_expected_local_disconnect,
    local_ssh_transport_cancelled_error,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// 只有已排队的本地请求允许 Disconnect sentinel；单纯取消标志不能掩盖其它任务错误。
///
/// @author kongweiguang
#[test]
fn expected_disconnect_requires_local_request_and_exact_sentinel() {
    assert!(is_expected_local_disconnect(
        true,
        false,
        &russh::Error::Disconnect
    ));
    assert!(!is_expected_local_disconnect(
        false,
        true,
        &russh::Error::Disconnect
    ));
    assert!(!is_expected_local_disconnect(
        true,
        true,
        &russh::Error::RequestDenied
    ));
    assert!(!is_expected_local_disconnect(
        true,
        true,
        &russh::Error::SendError
    ));
}

/// 相同 ErrorKind 或错误文案不构成本地取消证据，必须同时匹配 token 状态与私有 source 类型。
///
/// @author kongweiguang
#[test]
fn expected_transport_cancel_requires_owned_source_and_cancelled_session() {
    let owned = russh::Error::IO(local_ssh_transport_cancelled_error());
    assert!(is_expected_local_disconnect(false, true, &owned));
    assert!(!is_expected_local_disconnect(false, false, &owned));
    let unrelated = russh::Error::IO(std::io::Error::new(
        std::io::ErrorKind::ConnectionAborted,
        "SSH transport was cancelled locally",
    ));
    assert!(!is_expected_local_disconnect(true, true, &unrelated));
    let reset = russh::Error::IO(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
    assert!(!is_expected_local_disconnect(true, true, &reset));
}

/// 仅将已 join 的 Russh transport 终止类错误视为关闭 ACK，普通 I/O/协议错误仍需报告。
///
/// @author kongweiguang
#[test]
fn remote_transport_end_classification_is_narrow() {
    for kind in [
        std::io::ErrorKind::BrokenPipe,
        std::io::ErrorKind::ConnectionAborted,
        std::io::ErrorKind::ConnectionReset,
        std::io::ErrorKind::UnexpectedEof,
    ] {
        assert!(is_confirmed_remote_transport_end(&russh::Error::IO(
            std::io::Error::from(kind)
        )));
    }
    assert!(is_confirmed_remote_transport_end(&russh::Error::Disconnect));
    assert!(!is_confirmed_remote_transport_end(&russh::Error::IO(
        std::io::Error::other("ordinary writer failure")
    )));
    assert!(!is_confirmed_remote_transport_end(&russh::Error::IO(
        local_ssh_transport_cancelled_error()
    )));
    assert!(!is_confirmed_remote_transport_end(
        &russh::Error::RequestDenied
    ));
}

/// 从四个真实 Async IO 入口验证取消均保留同一私有 source，而不是只测试手工构造的分类输入。
///
/// @author kongweiguang
#[tokio::test]
async fn cancelled_peer_stream_preserves_owned_source_in_every_io_operation() {
    let (socket, _peer) = tokio::io::duplex(32);
    let cancellation = CancellationToken::new();
    let mut stream = CancellablePeerStream::new(Box::new(socket), cancellation.clone());
    cancellation.cancel();
    let mut byte = [0_u8];
    let errors = [
        stream.read(&mut byte).await.expect_err("cancelled read"),
        stream.write(b"input").await.expect_err("cancelled write"),
        stream.flush().await.expect_err("cancelled flush"),
        stream.shutdown().await.expect_err("cancelled shutdown"),
    ];
    for error in errors {
        assert!(is_expected_local_disconnect(
            false,
            true,
            &russh::Error::IO(error)
        ));
    }
}
