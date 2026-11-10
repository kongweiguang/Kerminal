//! 网页桥接拒绝失效实例并确保取消和回执不会串到重用的请求身份。
//!
//! @author kongweiguang
use super::*;
use kerminal_domain::plugins::PluginId;

/// 夹具固定插件实例代次，便于检验页面被替换后的消息隔离。
fn bridge() -> WebViewBridgeSession {
    WebViewBridgeSession::new(
        PluginViewIdentity {
            plugin: PluginId::parse("acme.web").unwrap(),
            view: "page".into(),
            instance: "instance-a".into(),
            generation: 7,
        },
        3,
    )
    .unwrap()
}

/// 每条测试消息携带与浏览器 SDK 相同的外围实例信息。
fn frame(instance: &str, generation: u64, message: Value) -> String {
    json!({"view_instance":instance,"generation":generation,"message":message}).to_string()
}

/// 过期实例不能占用请求槽位；取消后的迟到结果也不能命中复用相同 ID 的新请求。
#[test]
fn stale_or_cancelled_request_cannot_finish_a_new_request() {
    let mut bridge = bridge();
    let request =
        json!({"jsonrpc":"2.0","id":"p:1","method":"view.action","params":{"action":"save"}});
    assert!(bridge.accept(&frame("old", 3, request.clone())).is_err());
    assert!(
        bridge
            .accept(&frame("instance-a", 2, request.clone()))
            .is_err()
    );
    let WebViewBridgeMessage::Request {
        sequence,
        cancellation,
        ..
    } = bridge
        .accept(&frame("instance-a", 3, request.clone()))
        .unwrap()
    else {
        panic!("request expected")
    };
    assert!(
        bridge
            .accept(&frame("instance-a", 3, request.clone()))
            .is_err()
    );
    let cancel = json!({"jsonrpc":"2.0","method":"$/cancel","params":{"id":"p:1"}});
    assert!(matches!(
        bridge.accept(&frame("instance-a", 3, cancel)),
        Ok(WebViewBridgeMessage::Cancelled)
    ));
    assert!(cancellation.is_cancelled());
    let WebViewBridgeMessage::Request { sequence: next, .. } =
        bridge.accept(&frame("instance-a", 3, request)).unwrap()
    else {
        panic!("request expected")
    };
    assert!(bridge.finish("p:1", sequence, Ok(json!({}))).is_none());
    assert!(
        bridge
            .finish("p:1", next, Ok(json!({"saved":true})))
            .is_some()
    );
}

/// 关闭必须传播取消，并拒绝后续请求与原页面的迟到回执。
#[test]
fn closing_cancels_pending_requests_and_rejects_new_messages() {
    let mut bridge = bridge();
    let request = frame(
        "instance-a",
        3,
        json!({"jsonrpc":"2.0","id":"p:2","method":"settings.get","params":{}}),
    );
    let WebViewBridgeMessage::Request {
        sequence,
        cancellation,
        ..
    } = bridge.accept(&request).unwrap()
    else {
        panic!("request expected")
    };
    bridge.close();
    assert!(cancellation.is_cancelled());
    assert!(bridge.finish("p:2", sequence, Ok(json!({}))).is_none());
    assert!(bridge.accept(&request).is_err());
}

/// 错误信封不能消费有效请求容量，也不能伪造相反方向的 RPC 身份。
#[test]
fn malformed_or_duplicate_envelope_fields_are_rejected() {
    let mut bridge = bridge();
    let duplicate = r#"{"view_instance":"wrong","view_instance":"instance-a","generation":3,"message":{"jsonrpc":"2.0","id":"p:1","method":"settings.get"}}"#;
    assert!(bridge.accept(duplicate).is_err());
    for message in [
        json!({"jsonrpc":"2.0","id":"p:","method":"settings.get"}),
        json!({"jsonrpc":"2.0","id":"h:1","method":"settings.get"}),
        json!({"jsonrpc":"2.0","id":"p:1","result":{}}),
        json!({"jsonrpc":"2.0","id":"p:1","method":"settings.get","result":{}}),
    ] {
        assert!(bridge.accept(&frame("instance-a", 3, message)).is_err());
    }
    let valid = json!({"jsonrpc":"2.0","id":"p:1","method":"settings.get"});
    assert!(matches!(
        bridge.accept(&frame("instance-a", 3, valid)),
        Ok(WebViewBridgeMessage::Request { .. })
    ));
}

/// 未登记、已超时或重复的生命周期回执不得唤醒任何宿主请求。
#[test]
fn only_pending_host_request_accepts_one_reply() {
    let mut bridge = bridge();
    let response = |id: &str| {
        frame(
            "instance-a",
            3,
            json!({"jsonrpc":"2.0","id":id,"result":{}}),
        )
    };
    assert!(bridge.accept(&response("h:1")).is_err());
    let (first, _) = bridge.begin_host_request("view.open", json!({})).unwrap();
    bridge.cancel_host_request(&first);
    assert!(bridge.accept(&response(&first)).is_err());
    let (second, _) = bridge.begin_host_request("view.open", json!({})).unwrap();
    assert_ne!(first, second);
    assert!(matches!(
        bridge.accept(&response(&second)),
        Ok(WebViewBridgeMessage::Response(_))
    ));
    assert!(bridge.accept(&response(&second)).is_err());
}

/// 同一宿主视图重建页面后仍保留原视图身份，但必须拒绝旧页面桥接代次。
#[test]
fn rebuilt_page_rejects_old_bridge_generation() {
    let mut first = bridge();
    let request = json!({"jsonrpc":"2.0","id":"p:1","method":"settings.get"});
    let WebViewBridgeMessage::Request { cancellation, .. } = first
        .accept(&frame("instance-a", 3, request.clone()))
        .unwrap()
    else {
        panic!("request expected")
    };
    first.close();
    assert!(cancellation.is_cancelled());
    let mut second = WebViewBridgeSession::new(
        PluginViewIdentity {
            plugin: PluginId::parse("acme.web").unwrap(),
            view: "page".into(),
            instance: "instance-a".into(),
            generation: 7,
        },
        4,
    )
    .unwrap();
    assert!(
        second
            .accept(&frame("instance-a", 3, request.clone()))
            .is_err()
    );
    assert!(matches!(
        second.accept(&frame("instance-a", 4, request)),
        Ok(WebViewBridgeMessage::Request { .. })
    ));
}

/// 保留 DOM 的进程恢复必须结算旧 Promise 并取消后台任务，迟到结果不再可发送。
#[test]
fn interrupted_process_rejects_all_page_waiters_without_reusing_sequence() {
    let mut bridge = bridge();
    let request = frame(
        "instance-a",
        3,
        json!({"jsonrpc":"2.0","id":"p:1","method":"view.action","params":{"action":"search"}}),
    );
    let WebViewBridgeMessage::Request {
        sequence,
        cancellation,
        ..
    } = bridge.accept(&request).unwrap()
    else {
        panic!("request expected")
    };
    let replies = bridge.abort_requests();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["message"]["error"]["code"], -32004);
    assert!(cancellation.is_cancelled());
    assert!(bridge.finish("p:1", sequence, Ok(json!({}))).is_none());
    let WebViewBridgeMessage::Request { sequence: next, .. } = bridge.accept(&request).unwrap()
    else {
        panic!("request expected")
    };
    assert!(next > sequence);
}

/// 心跳和命令面板只接受固定空参数，不能借通知发送任意宿主命令。
#[test]
fn host_control_notifications_are_typed_and_identity_checked() {
    let mut bridge = bridge();
    let heartbeat = json!({"jsonrpc":"2.0","method":"webview.heartbeat","params":{}});
    assert!(matches!(
        bridge.accept(&frame("instance-a", 3, heartbeat.clone())),
        Ok(WebViewBridgeMessage::Heartbeat)
    ));
    assert!(bridge.accept(&frame("old", 3, heartbeat)).is_err());
    let shortcut = json!({"jsonrpc":"2.0","method":"webview.commandPalette","params":{}});
    assert!(matches!(
        bridge.accept(&frame("instance-a", 3, shortcut)),
        Ok(WebViewBridgeMessage::CommandPalette)
    ));
    assert!(bridge.accept(&frame("instance-a",3,json!({"jsonrpc":"2.0","method":"webview.commandPalette","params":{"action":"quit"}}))).is_err());
}

/// 在途配额被占满时拒绝新增请求；主动取消释放槽位，不影响其他请求身份。
#[test]
fn request_capacity_is_bounded_and_cancellation_releases_one_slot() {
    let mut bridge = bridge();
    for index in 0..MAX_IN_FLIGHT {
        let request = frame(
            "instance-a",
            3,
            json!({"jsonrpc":"2.0","id":format!("p:{index}"),"method":"settings.get"}),
        );
        assert!(matches!(
            bridge.accept(&request),
            Ok(WebViewBridgeMessage::Request { .. })
        ));
    }
    let overflow = frame(
        "instance-a",
        3,
        json!({"jsonrpc":"2.0","id":"p:overflow","method":"settings.get"}),
    );
    assert!(bridge.accept(&overflow).is_err());
    bridge
        .accept(&frame(
            "instance-a",
            3,
            json!({"jsonrpc":"2.0","method":"$/cancel","params":{"id":"p:0"}}),
        ))
        .unwrap();
    assert!(matches!(
        bridge.accept(&overflow),
        Ok(WebViewBridgeMessage::Request { .. })
    ));
}
