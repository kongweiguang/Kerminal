//! 使用真实双向流验证 Sidecar 协议调度与关闭。
//! @author kongweiguang

use std::time::Duration;

use kerminal_plugin_sdk::{Error, ProgressParams, Router};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// 与宿主相同地写入单行帧，测试不会绕过协议分帧。
async fn send(writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>, value: Value) {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    writer.write_all(&bytes).await.unwrap();
}

/// 使用有限等待确认回复；协议阻塞时测试直接失败而非永久等待。
async fn receive(reader: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>) -> Value {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

/// 长请求取消不能阻止心跳和关闭，也不能将取消串到其他请求。
#[tokio::test]
async fn handshake_ping_cancel_and_shutdown() {
    let mut router = Router::new("acme.example", "1.0.0");
    router
        .method("work", |_, _, cancel| async move {
            cancel.cancelled().await;
            Err(Error::new(-32002, "cancelled"))
        })
        .unwrap();
    let (host, plugin) = tokio::io::duplex(8192);
    let (plugin_read, plugin_write) = tokio::io::split(plugin);
    let task = tokio::spawn(router.run_with_io(plugin_read, plugin_write));
    let (read, mut write) = tokio::io::split(host);
    let mut read = BufReader::new(read);
    send(&mut write, json!({"jsonrpc":"2.0","id":"h:1","method":"initialize","params":{"protocol":1,"host_api":"1.0","plugin":{"id":"acme.example","version":"1.0.0"},"locale":"en","theme":"dark","data_dir":"tmp","generation":1}})).await;
    assert_eq!(
        receive(&mut read).await["result"]["plugin"]["id"],
        "acme.example"
    );
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:2","method":"work","params":{}}),
    )
    .await;
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:3","method":"$/ping","params":{}}),
    )
    .await;
    assert_eq!(receive(&mut read).await["id"], "h:3");
    send(
        &mut write,
        json!({"jsonrpc":"2.0","method":"$/cancel","params":{"id":"h:2"}}),
    )
    .await;
    assert_eq!(receive(&mut read).await["error"]["code"], -32002);
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:4","method":"shutdown","params":{}}),
    )
    .await;
    assert_eq!(receive(&mut read).await["id"], "h:4");
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

/// 错误插件身份不能通过回显宿主参数冒充合法进程。
#[tokio::test]
async fn rejects_wrong_identity() {
    let (host, plugin) = tokio::io::duplex(8192);
    let (plugin_read, plugin_write) = tokio::io::split(plugin);
    let task =
        tokio::spawn(Router::new("acme.example", "1.0.0").run_with_io(plugin_read, plugin_write));
    let (_, mut write) = tokio::io::split(host);
    send(&mut write, json!({"jsonrpc":"2.0","id":"h:1","method":"initialize","params":{"protocol":1,"host_api":"1.0","plugin":{"id":"other.example","version":"1.0.0"},"locale":"en","theme":"dark","data_dir":"tmp","generation":1}})).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
}

/// 回调崩溃只结束该请求，之后仍可响应心跳并正常关闭。
#[tokio::test]
async fn handler_panic_returns_error_and_keeps_router_alive() {
    let mut router = Router::new("acme.example", "1.0.0");
    router
        .method("panic", |_, _, _| async move {
            panic!("fixture panic");
            #[allow(unreachable_code)]
            Ok(Value::Null)
        })
        .unwrap();
    let (host, plugin) = tokio::io::duplex(8192);
    let (plugin_read, plugin_write) = tokio::io::split(plugin);
    let task = tokio::spawn(router.run_with_io(plugin_read, plugin_write));
    let (read, mut write) = tokio::io::split(host);
    let mut read = BufReader::new(read);
    send(&mut write, json!({"jsonrpc":"2.0","id":"h:1","method":"initialize","params":{"protocol":1,"host_api":"1.0","plugin":{"id":"acme.example","version":"1.0.0"},"locale":"en","theme":"dark","data_dir":"tmp","generation":1}})).await;
    receive(&mut read).await;
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:2","method":"panic","params":{}}),
    )
    .await;
    assert_eq!(receive(&mut read).await["error"]["code"], -32603);
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:3","method":"shutdown","params":{}}),
    )
    .await;
    assert_eq!(receive(&mut read).await["id"], "h:3");
    task.await.unwrap().unwrap();
}

/// 输出已断开时不依赖输入继续到达，即可报告连接失败。
#[tokio::test]
async fn writer_failure_terminates_even_if_input_stays_open() {
    let (host_input, plugin_input) = tokio::io::duplex(8192);
    let (host_output, plugin_output) = tokio::io::duplex(8192);
    drop(host_output);
    let task =
        tokio::spawn(Router::new("acme.example", "1.0.0").run_with_io(plugin_input, plugin_output));
    let (_, mut write) = tokio::io::split(host_input);
    send(&mut write, json!({"jsonrpc":"2.0","id":"h:1","method":"initialize","params":{"protocol":1,"host_api":"1.0","plugin":{"id":"acme.example","version":"1.0.0"},"locale":"en","theme":"dark","data_dir":"tmp","generation":1}})).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
}

/// 上限包含换行符，与宿主监督器采用同一帧边界。
#[tokio::test]
async fn rejects_frame_when_newline_exceeds_limit() {
    let (host, plugin) = tokio::io::duplex(65536);
    let (plugin_read, plugin_write) = tokio::io::split(plugin);
    let task =
        tokio::spawn(Router::new("acme.example", "1.0.0").run_with_io(plugin_read, plugin_write));
    let (_, mut write) = tokio::io::split(host);
    let payload = vec![b' '; kerminal_plugin_protocol::MAX_RPC_BYTES];
    let _ = write.write_all(&payload).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
}

/// 达到并发上限后拒绝额外请求，取消释放槽位后仍能接受新请求。
#[tokio::test]
async fn bounds_requests_and_reuses_slot_after_cancellation() {
    let mut router = Router::new("acme.example", "1.0.0");
    router
        .method("block", |_, _, cancel| async move {
            cancel.cancelled().await;
            Err(Error::new(-32002, "cancelled"))
        })
        .unwrap();
    let (host, plugin) = tokio::io::duplex(65536);
    let (plugin_read, plugin_write) = tokio::io::split(plugin);
    let task = tokio::spawn(router.run_with_io(plugin_read, plugin_write));
    let (read, mut write) = tokio::io::split(host);
    let mut read = BufReader::new(read);
    send(&mut write, json!({"jsonrpc":"2.0","id":"h:0","method":"initialize","params":{"protocol":1,"host_api":"1.0","plugin":{"id":"acme.example","version":"1.0.0"},"locale":"en","theme":"dark","data_dir":"tmp","generation":1}})).await;
    receive(&mut read).await;
    for index in 0..=kerminal_plugin_protocol::MAX_PENDING_REQUESTS {
        send(
            &mut write,
            json!({"jsonrpc":"2.0","id":format!("h:work{index}"),"method":"block","params":{}}),
        )
        .await;
    }
    let overload = receive(&mut read).await;
    assert_eq!(
        overload["error"]["code"],
        kerminal_plugin_protocol::error_code::OVERLOADED
    );
    send(
        &mut write,
        json!({"jsonrpc":"2.0","method":"$/cancel","params":{"id":"h:work0"}}),
    )
    .await;
    assert_eq!(receive(&mut read).await["id"], "h:work0");
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:new","method":"block","params":{}}),
    )
    .await;
    send(
        &mut write,
        json!({"jsonrpc":"2.0","method":"$/cancel","params":{"id":"h:new"}}),
    )
    .await;
    assert_eq!(receive(&mut read).await["id"], "h:new");
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:end","method":"shutdown","params":{}}),
    )
    .await;
    assert_eq!(receive(&mut read).await["id"], "h:end");
    task.await.unwrap().unwrap();
}

/// 从 SDK 路由经真实编码流回读进度，再用唯一协议 DTO 解码，防止便捷 API 与线协议漂移。
#[tokio::test]
async fn progress_notification_uses_the_canonical_wire_contract() {
    let mut router = Router::new("acme.example", "1.0.0");
    router
        .method("work", |cx, _, _| async move {
            cx.progress(ProgressParams {
                token: "search-one".into(),
                message: Some("正在搜索".into()),
                percent: Some(25.0),
            })
            .await?;
            Ok(json!({}))
        })
        .unwrap();
    let (host, plugin) = tokio::io::duplex(8192);
    let (plugin_read, plugin_write) = tokio::io::split(plugin);
    let task = tokio::spawn(router.run_with_io(plugin_read, plugin_write));
    let (read, mut write) = tokio::io::split(host);
    let mut read = BufReader::new(read);
    send(&mut write, json!({"jsonrpc":"2.0","id":"h:init","method":"initialize","params":{"protocol":1,"host_api":"1.0","plugin":{"id":"acme.example","version":"1.0.0"},"locale":"zh-CN","theme":"dark","data_dir":"tmp","generation":1}})).await;
    receive(&mut read).await;
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:work","method":"work","params":{}}),
    )
    .await;
    let notification: kerminal_plugin_protocol::RpcNotification =
        serde_json::from_value(receive(&mut read).await).unwrap();
    notification.validate().unwrap();
    assert_eq!(notification.method, "$/progress");
    let progress: ProgressParams = serde_json::from_value(notification.params.unwrap()).unwrap();
    assert_eq!(progress.token, "search-one");
    assert_eq!(progress.message.as_deref(), Some("正在搜索"));
    assert_eq!(progress.percent, Some(25.0));
    assert_eq!(receive(&mut read).await["id"], "h:work");
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:stop","method":"shutdown","params":{}}),
    )
    .await;
    assert_eq!(receive(&mut read).await["id"], "h:stop");
    task.await.unwrap().unwrap();
}
