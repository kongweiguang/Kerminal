//! 单实例协议和 CLI 参数边界测试。
//!
//! @author kongweiguang

#[path = "../src/external_activation.rs"]
mod external_activation;

use std::ffi::OsString;

use external_activation::{
    ActivationRequest, activation_from_os, activation_queue, decode_message, encode_message,
};
use kerminal_application::platform::ExternalActivation;

/// 确认版本化 envelope 往返保留 Unicode 参数与工作目录，调试输出不展开原始参数。
#[test]
fn versioned_external_payload_round_trips_without_debugging_arguments() {
    let request = ActivationRequest::External(ExternalActivation {
        arguments: vec![
            "kerminal.exe".to_owned(),
            "kerminal://ssh/用户@example.com".to_owned(),
        ],
        working_directory: Some("C:\\work\\项目".to_owned()),
    });
    let wire = encode_message(&request).expect("encode request");
    assert_eq!(decode_message(&wire).expect("decode request"), request);
    assert!(!format!("{request:?}").contains("用户@example.com"));
}

/// 保留现有简单二次启动消息，同时拒绝未知版本、未知字段和不完整外部请求。
#[test]
fn decoder_accepts_legacy_focus_but_fails_closed_on_unknown_contracts() {
    assert_eq!(
        decode_message("activate").expect("legacy focus"),
        ActivationRequest::FocusWindow
    );
    assert_eq!(
        decode_message(&encode_message(&ActivationRequest::FocusWindow).expect("encode focus"))
            .expect("decode focus"),
        ActivationRequest::FocusWindow
    );

    for malformed in [
        r#"{"version":2,"kind":"activate"}"#,
        r#"{"version":1,"kind":"unknown"}"#,
        r#"{"version":1,"kind":"activate","extra":true}"#,
        r#"{"version":1,"kind":"external","arguments":[],"workingDirectory":null}"#,
    ] {
        assert!(decode_message(malformed).is_err());
    }
}

/// 证明外层 serde JSON string frame 也受限，避免内部 JSON 转义后突破 IPC 出站上限。
#[test]
fn encoder_rejects_payloads_larger_than_transport_frame_limit() {
    let request = ActivationRequest::External(ExternalActivation {
        arguments: vec![
            "kerminal.exe".to_owned(),
            "\"".repeat(15 * 1024),
            "\"".repeat(15 * 1024),
            "\"".repeat(15 * 1024),
            "\"".repeat(15 * 1024),
        ],
        working_directory: None,
    });
    assert!(encode_message(&request).is_err());
}

/// 首次进程和二次进程的 OS 参数都拒绝 lossy UTF-8 转换，避免改变目标身份。
#[test]
fn native_non_unicode_arguments_and_working_directory_are_rejected() {
    let invalid = non_unicode_os_string();
    assert!(
        activation_from_os(
            [OsString::from("kerminal.exe"), invalid.clone()],
            Some(OsString::from("C:\\work")),
        )
        .is_err()
    );
    assert!(
        activation_from_os(
            [OsString::from("kerminal.exe"), OsString::from("ssh://host")],
            Some(invalid),
        )
        .is_err()
    );
}

/// 无外部参数时返回 None，让主实例正常启动而二次实例单独编码为聚焦请求。
#[test]
fn ordinary_process_start_has_no_external_activation() {
    assert!(
        activation_from_os([OsString::from("kerminal.exe")], None)
            .expect("ordinary launch")
            .is_none()
    );
    assert!(
        activation_from_os([non_unicode_os_string()], None)
            .expect("ordinary launch does not need argv0 conversion")
            .is_none()
    );
}

/// 请求队列固定为 16 项，满载时拒绝额外同步提交而不悄悄丢失旧请求。
#[test]
fn activation_queue_has_a_hard_capacity() {
    let (sender, mut receiver) = activation_queue();
    for _ in 0..16 {
        sender
            .try_submit(ActivationRequest::FocusWindow)
            .expect("submit within capacity");
    }
    assert!(sender.try_submit(ActivationRequest::FocusWindow).is_err());
    for _ in 0..16 {
        assert_eq!(
            receiver.try_recv().expect("queued request"),
            ActivationRequest::FocusWindow
        );
    }
}

/// 确认异步 IPC 转发在队列满时等待空位，并在容量释放后保留原请求。
#[tokio::test]
async fn async_submit_waits_for_capacity_without_losing_request() {
    let (sender, mut receiver) = activation_queue();
    for _ in 0..16 {
        sender
            .try_submit(ActivationRequest::FocusWindow)
            .expect("fill activation queue");
    }

    let expected = ActivationRequest::External(ExternalActivation {
        arguments: vec!["kerminal.exe".to_owned(), "ssh://example.test".to_owned()],
        working_directory: None,
    });
    let pending_sender = sender.clone();
    let pending_request = expected.clone();
    let (started, started_rx) = tokio::sync::oneshot::channel();
    let pending = tokio::spawn(async move {
        let _ = started.send(());
        pending_sender
            .submit(pending_request.clone())
            .await
            .map(|()| pending_request)
    });
    started_rx.await.expect("sender reached bounded queue");
    tokio::task::yield_now().await;
    assert!(
        !pending.is_finished(),
        "full queue must keep async send pending"
    );

    for _ in 0..16 {
        assert_eq!(
            receiver.recv().await.expect("queued focus request"),
            ActivationRequest::FocusWindow
        );
    }
    assert_eq!(
        pending.await.expect("submit task").expect("submit result"),
        expected
    );
    assert_eq!(receiver.recv().await, Some(expected));
}

/// 在平台原生扩展上构造不能表示为 UTF-8 的 OsString，不把字符替换成错误主机名。
fn non_unicode_os_string() -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(vec![0xff])
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[0xd800])
    }
    #[cfg(not(any(unix, windows)))]
    {
        OsString::from("fallback")
    }
}
