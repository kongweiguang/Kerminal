//! 核心 MCP 工具的真实组合根与一次性 SSH 夹具验收。
//!
//! @author kongweiguang

use std::{io::Write, net::TcpListener, time::Duration};

use kerminal_application::connections::{CredentialResolver, ResolvedCredential};
use kerminal_domain::{
    commands::{CommandHistoryRecordRequest, CommandHistorySource, CommandHistoryTarget},
    connections::CredentialRef,
};
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;

use super::{DesktopComposition, Harness, text};

const HOST_ID: &str = "right-rail-fixture";

/// 真实远端操作必须依次等待完成并回读，隔离 fixture 由父测试统一回收。
///
/// @author kongweiguang
pub(super) async fn run(h: &Harness, composition: &DesktopComposition) {
    discovery_and_configuration(h).await;
    credentials(h, composition).await;
    ssh(h).await;
    files_and_transfers(h).await;
    port_forward(h).await;
    history(h, composition).await;
}

/// 帮助各主题和每个配置域都通过同一运行中 endpoint 检查，报告必须是有效业务结果。
///
/// @author kongweiguang
async fn discovery_and_configuration(h: &Harness) {
    let overview = h.call("kerminal.help", json!({})).await;
    assert_eq!(overview["schemaVersion"], 1);
    assert_eq!(overview["helpTopics"].as_array().unwrap().len(), 5);
    let app = h.call("kerminal.help", json!({"topic": "app"})).await;
    assert!(
        app["areas"]
            .as_array()
            .unwrap()
            .iter()
            .any(|area| area["id"] == "hosts")
    );
    let config = h.call("kerminal.help", json!({"topic": "config"})).await;
    assert_eq!(config["guideFile"], "kerminal-config.md");
    assert!(text(&config, "markdown").contains("kerminal.config.validate"));
    let tools = h
        .call(
            "kerminal.help",
            json!({"topic": "tools", "toolId": "ssh.command"}),
        )
        .await;
    assert_eq!(tools["matchedToolCount"], 1);
    assert_eq!(tools["toolReference"][0]["toolId"], "ssh.command");
    assert!(tools["toolReference"][0]["inputSchema"].is_object());
    let operation = h
        .call(
            "kerminal.help",
            json!({"topic": "operation", "intent": "ssh-command"}),
        )
        .await;
    assert_eq!(operation["intent"], "ssh-command");
    assert!(
        operation["availableReferencedToolIds"]
            .as_array()
            .unwrap()
            .contains(&json!("ssh.command"))
    );

    let snapshot = h.call("kerminal.runtime_snapshot", json!({})).await;
    assert_eq!(snapshot["schemaVersion"], 1);
    assert!(snapshot["terminalSessions"].is_array());
    assert!(snapshot["agentSessions"].is_array());
    assert!(snapshot["portForwards"].is_array());
    assert!(snapshot["diagnostics"].as_array().unwrap().is_empty());

    for scope in [
        "all",
        "settings",
        "profiles",
        "hosts",
        "snippets",
        "workflows",
    ] {
        let result = h
            .call("kerminal.config.validate", json!({"scope": scope}))
            .await;
        let report = &result["report"];
        assert_eq!(report["scope"], scope);
        assert_eq!(
            report["valid"], true,
            "fixture config validation failed: {report}"
        );
        assert_eq!(report["errorCount"], 0);
    }
}

/// 测试值仅进入隔离 vault；解密回读证明加密内容可用，同时核对主机文件没有明文。
///
/// @author kongweiguang
async fn credentials(h: &Harness, composition: &DesktopComposition) {
    const HOST_PASSWORD: &str = "fixture-pass-39d6c8f3";
    const EXTRA_SECRET: &str = "fixture-extra-a61fe258";
    let host = h
        .call(
            "kerminal.host.upsert_with_credential",
            json!({
                "name": "MCP disposable credential fixture", "host": "192.0.2.123", "port": 2222,
                "username": "fixture-user", "password": HOST_PASSWORD
            }),
        )
        .await;
    assert_eq!(host["credentialStatus"], "vault");
    let host_id = text(&host, "hostId");
    let host_ref = text(&host, "secretRef");
    let extra = h
        .call(
            "kerminal.vault.encrypt_secret",
            json!({
                "kind": "ssh-host", "hostId": host_id, "scope": "target",
                "material": "key-passphrase", "plaintext": EXTRA_SECRET
            }),
        )
        .await;
    let extra_ref = text(&extra, "secretRef");
    assert_ne!(host_ref, extra_ref);

    assert_password(composition, &host_ref, HOST_PASSWORD, false).await;
    assert_password(composition, &extra_ref, EXTRA_SECRET, true).await;
    let vault = std::fs::read_to_string(h.root.join("secrets/vault.toml")).unwrap();
    assert!(vault.contains("ciphertext"));
    assert!(!vault.contains(HOST_PASSWORD) && !vault.contains(EXTRA_SECRET));
    let host_documents = std::fs::read_dir(h.root.join("hosts"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "toml")
        })
        .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
        .collect::<Vec<_>>();
    let saved = host_documents
        .iter()
        .find(|doc| doc.contains("MCP disposable credential fixture"))
        .expect("created host is persisted");
    assert!(saved.contains(&host_ref));
    assert!(!saved.contains(HOST_PASSWORD) && !saved.contains(EXTRA_SECRET));
}

/// 通过生产 vault resolver 验证可恢复原值，断言消息不输出一次性凭据或引用。
///
/// @author kongweiguang
async fn assert_password(
    composition: &DesktopComposition,
    reference: &str,
    expected: &str,
    passphrase: bool,
) {
    let reference = CredentialRef::new(reference.to_owned()).unwrap();
    let resolved = composition
        ._vault
        .resolve(&reference, &CancellationToken::new())
        .await
        .unwrap();
    match resolved {
        ResolvedCredential::Password(value) if !passphrase => assert!(value.as_str() == expected),
        ResolvedCredential::KeyPassphrase(value) if passphrase => {
            assert!(value.as_str() == expected)
        }
        _ => panic!("vault returned unexpected credential material"),
    }
}

/// 使用已保存主机和名称选择器执行真实命令，并验证非零退出、截断和失效目标的错误契约。
///
/// @author kongweiguang
async fn ssh(h: &Harness) {
    let selected = h
        .call(
            "ssh.command",
            json!({
                "name": "Right rail fixture", "username": "root", "port": h.fixture["sshPort"],
                "command": "printf selector-ok"
            }),
        )
        .await;
    assert_eq!(selected["hostId"], HOST_ID);
    assert_eq!(selected["output"]["exitCode"], 0);
    assert_eq!(selected["output"]["stdout"], "selector-ok");

    let truncated = h.call("ssh.command", json!({
        "hostId": HOST_ID, "command": "head -c 4096 /dev/zero | tr '\\0' X", "maxOutputBytes": 256
    })).await;
    assert_eq!(truncated["output"]["exitCode"], 0);
    assert_eq!(truncated["output"]["stdoutBytes"], 256);
    assert_eq!(truncated["output"]["stdoutTruncated"], true);

    let failed = expect_error(
        h,
        "ssh.command",
        json!({"hostId": HOST_ID, "command": "printf fail >&2; exit 7"}),
    )
    .await;
    assert_eq!(failed["errorKind"], "remoteCommandFailed");
    assert_eq!(failed["data"]["output"]["exitCode"], 7);
    assert_eq!(failed["data"]["output"]["stderr"], "fail");
    let missing = expect_error(
        h,
        "ssh.command",
        json!({"name": "nonexistent-mcp-fixture", "command": "pwd"}),
    )
    .await;
    assert_eq!(missing["errorKind"], "targetNotFound");
}

/// 预期失败也必须经过真实 HTTP，并检查工具层错误而不是忽略 transport 故障。
///
/// @author kongweiguang
async fn expect_error(h: &Harness, name: &'static str, args: Value) -> Value {
    let response = timeout(
        Duration::from_secs(65),
        h.client.peer().call_tool(
            CallToolRequestParams::new(name).with_arguments(args.as_object().unwrap().clone()),
        ),
    )
    .await
    .unwrap()
    .expect("request reaches the live server");
    assert_eq!(
        response.is_error,
        Some(true),
        "expected tool error for {name}"
    );
    response.structured_content.expect("structured tool error")
}

/// 传输只触碰经 fixture 根校验的专属远端目录，并使用 SSH 回读实际文件内容。
///
/// @author kongweiguang
async fn files_and_transfers(h: &Harness) {
    let remote = format!("{}/mcp-live-core", text(&h.fixture, "remoteSftpDirectory"));
    h.ssh(&format!("mkdir -- '{remote}'")).await;

    let local_source = h.root.join("mcp-upload.txt");
    let local_download = h.root.join("mcp-download.txt");
    let payload = format!("mcp-live-transfer-{}\n", text(&h.fixture, "runId"));
    std::fs::write(&local_source, &payload).unwrap();
    let first = format!("{remote}/uploaded.txt");
    let upload = h
        .call(
            "sftp.transfer.enqueue",
            json!({
                "source": {"type": "local", "path": local_source},
                "destination": {"type": "remote", "hostId": HOST_ID, "path": first},
                "kind": "file", "conflictPolicy": "overwrite"
            }),
        )
        .await;
    let uploaded = h.wait_transfer(&text(&upload["transfer"], "id")).await;
    assert_eq!(uploaded["status"], "succeeded", "upload failed: {uploaded}");
    assert_eq!(h.ssh(&format!("cat -- '{first}'")).await, payload);
    let download = h
        .call(
            "sftp.transfer.enqueue",
            json!({
                "source": {"type": "remote", "hostId": HOST_ID, "path": first},
                "destination": {"type": "local", "path": local_download},
                "kind": "file", "conflictPolicy": "overwrite"
            }),
        )
        .await;
    let downloaded = h.wait_transfer(&text(&download["transfer"], "id")).await;
    assert_eq!(
        downloaded["status"], "succeeded",
        "download failed: {downloaded}"
    );
    assert_eq!(std::fs::read_to_string(&local_download).unwrap(), payload);
    cancel_and_retry(h, &remote).await;
}

/// 大文件先取消并等待可重试终态，后继传输必须完成且远端 SHA-256 与本机一致。
///
/// @author kongweiguang
async fn cancel_and_retry(h: &Harness, remote: &str) {
    let source = h.root.join("mcp-cancel-retry.bin");
    let mut file = std::fs::File::create(&source).unwrap();
    let block = vec![0x5au8; 64 * 1024];
    let mut digest = Sha256::new();
    for _ in 0..512 {
        file.write_all(&block).unwrap();
        digest.update(&block);
    }
    drop(file);
    let expected_hash = format!("{:x}", digest.finalize());
    let destination = format!("{remote}/retry.bin");
    let queued = h
        .call(
            "sftp.transfer.enqueue",
            json!({
                "source": {"type": "local", "path": source},
                "destination": {"type": "remote", "hostId": HOST_ID, "path": destination},
                "kind": "file", "conflictPolicy": "overwrite"
            }),
        )
        .await;
    let id = text(&queued["transfer"], "id");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let listed = h
            .call("sftp.transfer.list", json!({"transferId": id}))
            .await;
        let status = text(&listed["transfers"][0], "status");
        if status == "running" {
            break;
        }
        assert_eq!(
            status, "queued",
            "transfer ended before cancellation could be exercised"
        );
        assert!(Instant::now() < deadline, "transfer did not start");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    h.call("sftp.transfer.cancel", json!({"transferId": id}))
        .await;
    let cancelled = h.wait_transfer(&id).await;
    assert_eq!(
        cancelled["status"], "canceled",
        "transfer completed before cancellation: {cancelled}"
    );
    assert_eq!(
        cancelled["retryable"], true,
        "cancelled transfer must permit retry"
    );
    let retried = h
        .call("sftp.transfer.retry", json!({"transferId": id}))
        .await;
    assert_eq!(retried["retry"]["accepted"], true);
    let successor = text(&retried["retry"], "transferId");
    assert_ne!(successor, id);
    let completed = h.wait_transfer(&successor).await;
    assert_eq!(
        completed["status"], "succeeded",
        "retry failed: {completed}"
    );
    let hash_output = h.ssh(&format!("sha256sum '{destination}'")).await;
    assert_eq!(
        hash_output.split_whitespace().next().unwrap(),
        expected_hash
    );
}

/// 只删除已知的本次 fixture 文件；目录中出现未知条目时保留并报告，避免递归删除。
///
/// @author kongweiguang
pub(super) async fn cleanup(h: &Harness) {
    let remote = format!("{}/mcp-live-core", text(&h.fixture, "remoteSftpDirectory"));
    let command = format!(
        "set -e; if test -d '{remote}'; then rm -f -- '{remote}/uploaded.txt' '{remote}/retry.bin'; rmdir -- '{remote}'; fi; test ! -e '{remote}'; printf cleaned"
    );
    assert_eq!(h.ssh(&command).await, "cleaned");
}

/// 真实 SSH 隧道需完成 HTTP 数据往返，关闭后再次连接应失败且列表不再保留实例。
///
/// @author kongweiguang
async fn port_forward(h: &Harness) {
    let reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let forward = h
        .call(
            "port_forward.create",
            json!({
                "hostId": HOST_ID, "kind": "local", "sourcePort": port,
                "targetHost": "127.0.0.1", "targetPort": h.fixture["containerTargetPort"]
            }),
        )
        .await;
    assert_eq!(forward["state"], "active");
    let id = text(&forward, "id");
    let list = h.call("port_forward.list", json!({})).await;
    assert!(
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == id)
    );
    let mut socket = timeout(
        Duration::from_secs(5),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .unwrap()
    .unwrap();
    socket
        .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(10), socket.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&response).contains("fixture-ok"));
    let closed = h.call("port_forward.close", json!({"forwardId": id})).await;
    assert_eq!(closed["closed"], true);
    let after = h.call("port_forward.list", json!({})).await;
    assert!(
        !after["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == id)
    );
    assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
}

/// 历史写入经生产应用服务并读取 MCP 摘要，避免把空数据库查询当作历史工具通过。
///
/// @author kongweiguang
async fn history(h: &Harness, composition: &DesktopComposition) {
    let record = composition
        .commands
        .record_history(CommandHistoryRecordRequest {
            command: "echo mcp-live-history-marker".to_owned(),
            source: CommandHistorySource::Tool,
            target: CommandHistoryTarget::Local,
            record: Some(true),
            session_id: None,
            pane_id: None,
            tab_id: None,
            profile_id: None,
            remote_host_id: None,
            cwd: None,
            shell: None,
        })
        .await
        .unwrap();
    assert!(record.recorded);
    let result = h
        .envelope(
            "history.search",
            json!({"query": "mcp-live-history-marker", "source": "tool"}),
        )
        .await;
    assert!(text(&result, "summary").contains("echo mcp-live-history-marker"));
}
