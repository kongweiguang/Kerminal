//! Kerminal Telnet adapter 的线缆级契约回归测试。
//!
//! @author kongweiguang

use std::{net::SocketAddr, time::Duration};

use kerminal_application::{
    AppResult,
    sessions::{SessionTransportControl, SessionTransportFactory},
};
use kerminal_domain::sessions::{
    SessionExitStatus, SessionTarget, TelnetSessionTarget, TerminalSize,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Mutex, Semaphore, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use super::{
    TelnetControlState, TelnetProtocol, TelnetRuntimeConfig, TelnetSessionTransportControl,
    TelnetSessionTransportFactory,
};

/// 创建领域尺寸时使用有效网格，避免测试夹具绕过生产尺寸不变量。
fn size(columns: u16, rows: u16) -> TerminalSize {
    TerminalSize::new(columns, rows, 0, 0).expect("non-zero test grid")
}

/// 关闭 deadline 用尽时保留 actor owner，之后可 join 同一 TCP 清理任务。
///
/// @author kongweiguang
#[tokio::test]
async fn close_timeout_retains_telnet_actor_for_retry() {
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let gate = std::sync::Arc::new(Semaphore::new(0));
    let worker_gate = std::sync::Arc::clone(&gate);
    let task: JoinHandle<AppResult<SessionExitStatus>> = tokio::spawn(async move {
        task_cancellation.cancelled().await;
        let _permit = worker_gate.acquire().await.expect("test gate remains open");
        Ok(SessionExitStatus {
            code: None,
            signal: Some("telnet session closed".to_owned()),
            success: true,
        })
    });
    let (size_sender, _size_receiver) = watch::channel(size(80, 24));
    let control = TelnetSessionTransportControl {
        cancellation,
        size_sender,
        state: Mutex::new(TelnetControlState {
            task: Some(task),
            finished: None,
        }),
    };

    assert!(
        control
            .close_until(tokio::time::Instant::now() + Duration::from_millis(20))
            .await
            .is_err()
    );
    assert!(control.state.lock().await.task.is_some());

    gate.add_permits(1);
    control
        .close_until(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .expect("retry closes after the actor releases its permit");
    assert!(control.state.lock().await.task.is_none());
}

/// 从已绑定回环 listener 构造显式 Telnet 目标，测试不依赖外部服务。
fn target(address: SocketAddr) -> SessionTarget {
    SessionTarget::Telnet(TelnetSessionTarget {
        host: address.ip().to_string(),
        port: address.port(),
    })
}

/// 确认双转义 IAC 在任何 TCP 分块边界下只恢复为一个终端数据字节。
#[test]
fn preserves_escaped_iac_and_nvt_cr_nul_across_chunks() {
    let mut protocol = TelnetProtocol::new(size(80, 24));
    let _ = protocol.take_transmit();
    let wire = [b'H', b'i', 0xff, 0xff, b'!', b'\r', 0x00];
    let mut terminal_data = Vec::new();

    for byte in wire {
        terminal_data.extend(protocol.receive(&[byte]));
    }

    assert_eq!(terminal_data, [b'H', b'i', 0xff, b'!', b'\r']);
}

/// 确认 BINARY 按方向协商：peer 的 WONT 不会关闭本地发送侧的 BINARY。
///
/// @author kongweiguang
#[test]
fn binary_negotiation_preserves_bytes_and_nvt_encodes_enter() {
    let mut protocol = TelnetProtocol::new(size(80, 24));
    let _ = protocol.take_transmit();

    assert!(protocol.receive(&[0xff, 0xfb, 0x00]).is_empty()); // WILL BINARY from peer.
    assert_eq!(protocol.take_transmit(), [0xff, 0xfd, 0x00]); // DO BINARY.
    assert_eq!(protocol.receive(b"\r\0"), b"\r\0");

    assert!(protocol.receive(&[0xff, 0xfd, 0x00]).is_empty()); // DO BINARY from peer.
    assert_eq!(protocol.take_transmit(), [0xff, 0xfb, 0x00]); // WILL BINARY.
    protocol.send_data(b"a\rb\xff");
    assert_eq!(protocol.take_transmit(), [b'a', b'\r', b'b', 0xff, 0xff]);

    assert!(protocol.receive(&[0xff, 0xfc, 0x00]).is_empty()); // WONT BINARY.
    assert_eq!(protocol.take_transmit(), [0xff, 0xfe, 0x00]); // DONT BINARY.
    protocol.send_data(b"\r");
    assert_eq!(protocol.take_transmit(), [b'\r']); // Remote WONT does not alter local BINARY.

    assert!(protocol.receive(&[0xff, 0xfe, 0x00]).is_empty()); // DONT BINARY from peer.
    assert_eq!(protocol.take_transmit(), [0xff, 0xfc, 0x00]); // WONT BINARY.
    protocol.send_data(b"\r");
    assert_eq!(protocol.take_transmit(), [b'\r', b'\n']);
}

/// 确认启动时发出的 WILL TTYPE 被 DO 确认，而非重复发起同一协商。
///
/// @author kongweiguang
#[test]
fn answers_split_terminal_type_subnegotiation() {
    let mut protocol = TelnetProtocol::new(size(80, 24));
    let initial = protocol.take_transmit();
    assert!(initial.windows(3).any(|bytes| bytes == [0xff, 0xfb, 0x18])); // WILL TTYPE.

    assert!(protocol.receive(&[0xff]).is_empty());
    assert!(protocol.receive(&[0xfd, 0x18]).is_empty()); // DO TTYPE.
    assert!(protocol.take_transmit().is_empty()); // DO acknowledges the outstanding WILL.

    for chunk in [
        &[0xff, 0xfa][..],
        &[0x18, 0x01][..],
        &[0xff][..],
        &[0xf0][..],
    ] {
        assert!(protocol.receive(chunk).is_empty());
    }
    assert_eq!(
        protocol.take_transmit(),
        [
            0xff, 0xfa, 0x18, 0x00, b'x', b't', b'e', b'r', b'm', b'-', b'2', b'5', b'6', b'c',
            b'o', b'l', b'o', b'r', 0xff, 0xf0,
        ]
    );
}

/// 确认初始 WILL NAWS、DO 确认后的尺寸子协商及运行时 resize 符合 wire 格式。
///
/// @author kongweiguang
#[test]
fn sends_naws_on_negotiate_and_resize_with_iac_escaping() {
    let mut protocol = TelnetProtocol::new(size(511, 30));
    let initial = protocol.take_transmit();
    assert!(initial.windows(3).any(|bytes| bytes == [0xff, 0xfb, 0x1f])); // WILL NAWS.

    assert!(protocol.receive(&[0xff, 0xfd, 0x1f]).is_empty()); // DO NAWS.
    assert_eq!(
        protocol.take_transmit(),
        [0xff, 0xfa, 0x1f, 0x01, 0xff, 0xff, 0x00, 0x1e, 0xff, 0xf0]
    );

    protocol.set_window_size(size(640, 255));
    assert_eq!(
        protocol.take_transmit(),
        [0xff, 0xfa, 0x1f, 0x02, 0x80, 0x00, 0xff, 0xff, 0xff, 0xf0]
    );
}

/// 确认连接取消会释放当前 socket，且同一 factory 可以新建下一条独立会话。
#[tokio::test]
async fn cancellation_and_reconnect_release_each_tcp_session() {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("listener address");
    let first_peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept first session");
        let mut bytes = [0_u8; 1];
        timeout(Duration::from_secs(2), stream.read(&mut bytes))
            .await
            .expect("first session closes")
            .expect("read first FIN");
    });
    let factory = TelnetSessionTransportFactory::new(TelnetRuntimeConfig::default())
        .expect("valid runtime config");
    let first = factory
        .open(&target(address), size(80, 24), CancellationToken::new())
        .await
        .expect("first session opens");
    first.control.close().await.expect("first session closes");
    first_peer.await.expect("first peer exits");

    let second_listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind second loopback");
    let second_address = second_listener
        .local_addr()
        .expect("second listener address");
    let second_peer = tokio::spawn(async move {
        let (mut stream, _) = second_listener
            .accept()
            .await
            .expect("accept second session");
        let mut bytes = [0_u8; 1];
        timeout(Duration::from_secs(2), stream.read(&mut bytes))
            .await
            .expect("second session closes")
            .expect("read second FIN");
    });
    let second = factory
        .open(
            &target(second_address),
            size(80, 24),
            CancellationToken::new(),
        )
        .await
        .expect("second session opens");
    second.control.close().await.expect("second session closes");
    second_peer.await.expect("second peer exits");
}

/// 确认终端输入半关闭仅发送 FIN，peer 的最终输出仍能通过同一 session 读回。
#[tokio::test]
async fn half_close_keeps_receiving_peer_tail() {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("listener address");
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept session");
        let mut input = Vec::new();
        timeout(Duration::from_secs(2), stream.read_to_end(&mut input))
            .await
            .expect("observe client FIN")
            .expect("read negotiation and FIN");
        stream
            .write_all(b"tail")
            .await
            .expect("send tail after FIN");
        stream.shutdown().await.expect("close peer output");
    });
    let factory = TelnetSessionTransportFactory::new(TelnetRuntimeConfig::default())
        .expect("valid runtime config");
    let mut transport = factory
        .open(&target(address), size(80, 24), CancellationToken::new())
        .await
        .expect("session opens");
    transport
        .writer
        .shutdown()
        .await
        .expect("half-close terminal input");

    let mut tail = [0_u8; 4];
    timeout(
        Duration::from_secs(2),
        transport.reader.read_exact(&mut tail),
    )
    .await
    .expect("terminal keeps receiving after input half-close")
    .expect("read final peer data");
    assert_eq!(&tail, b"tail");
    let status = transport
        .control
        .wait_exit()
        .await
        .expect("peer closes cleanly");
    assert_eq!(status.signal.as_deref(), Some("telnet peer closed"));
    peer.await.expect("peer task exits");
}

/// 确认取消 token 在 connect 前已关闭时不会创建 socket 或返回成功 transport。
///
/// @author kongweiguang
#[tokio::test]
async fn cancelled_open_does_not_connect() {
    let factory = TelnetSessionTransportFactory::new(TelnetRuntimeConfig::default())
        .expect("valid runtime config");
    let cancel = CancellationToken::new();
    cancel.cancel();

    let result = factory
        .open(
            &SessionTarget::Telnet(TelnetSessionTarget {
                host: "127.0.0.1".to_owned(),
                port: 1,
            }),
            size(80, 24),
            cancel,
        )
        .await;

    let error = result.err().expect("cancelled open must fail");
    let (_, cleanup) = error.into_parts();
    assert!(cleanup.is_none());
}
