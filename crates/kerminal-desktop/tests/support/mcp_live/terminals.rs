//! 验收真实本地/SSH PTY 与绑定真实 UI owner 的 Agent scoped HTTP 上下文。
//!
//! @author kongweiguang

use std::{path::Path, time::Duration};

use kerminal_application::{
    agents::{AgentService, CreateAgentSessionRequest},
    preferences::settings::{AgentLauncherSettings, CustomAgentDefinition},
    sessions::CreateSessionRequest,
};
use kerminal_domain::{
    agents::{AgentSessionScope, AgentTargetBinding, AgentTargetLiveStatus},
    identity::{HostId, PaneId, TabId},
    sessions::{SessionOwner, SessionTarget, SshSessionTarget, TerminalSize},
};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ClientInfo},
    transport::StreamableHttpClientTransport,
};
use serde_json::{Value, json};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::{DesktopComposition, Harness, text};

/// 输出 marker 必须由 shell 拼接产生，避免只读到了终端输入回显也被判为执行成功。
pub(super) async fn run(h: &Harness, composition: &DesktopComposition, endpoint: &str) {
    let local = h
        .call(
            "terminal.create",
            json!({"cwd": h.root, "cols": 120, "rows": 30}),
        )
        .await;
    let local_id = text(&local, "sessionId");
    assert_eq!(local["headless"], true);
    assert_eq!(local["ui"]["tabCreated"], false);
    let list = h.call("terminal.list", json!({})).await;
    assert!(
        list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == local_id)
    );
    let write = h
        .call(
            "terminal.write",
            json!({"sessionId": local_id, "data": "Write-Output ('MCP_LOCAL_' + 'EXECUTED')\r"}),
        )
        .await;
    assert!(write["bytesWritten"].as_u64().unwrap() > 0);
    wait_output(h, &local_id, "MCP_LOCAL_EXECUTED").await;
    h.call(
        "terminal.resize",
        json!({"sessionId": local_id, "cols": 101, "rows": 37}),
    )
    .await;
    let snapshot = h
        .call("terminal.snapshot", json!({"sessionId": local_id}))
        .await;
    assert_eq!(snapshot["session"]["cols"], 101);
    assert_eq!(snapshot["session"]["rows"], 37);
    h.call("terminal.close", json!({"sessionId": local_id}))
        .await;
    assert!(
        !h.call("terminal.list", json!({})).await["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == local_id)
    );
    h.error(
        "terminal.write",
        json!({"sessionId": local_id, "data": "must not reach a different terminal"}),
    )
    .await;
    h.error("terminal.create", json!({"cols": 0})).await;

    let remote = h.call("terminal.create", json!({"target": "ssh", "hostId": "right-rail-fixture", "cwd": h.fixture["remoteSftpDirectory"]})).await;
    let remote_id = text(&remote, "sessionId");
    h.call(
        "terminal.write",
        json!({"sessionId": remote_id, "data": "printf 'MCP_REMOTE_%s\\n' 'EXECUTED'\r"}),
    )
    .await;
    wait_output(h, &remote_id, "MCP_REMOTE_EXECUTED").await;
    h.call("terminal.close", json!({"sessionId": remote_id}))
        .await;
    scoped_agent_target(h, composition, endpoint).await;
}

/// 以有界快照轮询等待执行效果，不依赖固定 sleep 或等待用户窗口状态。
async fn wait_output(h: &Harness, id: &str, marker: &str) {
    timeout(Duration::from_secs(20), async {
        loop {
            let snapshot = h.call("terminal.snapshot", json!({"sessionId": id})).await;
            if text(&snapshot["snapshot"], "data").contains(marker) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("real PTY must execute and return the marker");
}

/// 用同一 SSH 夹具和明确 UI owner 覆盖用户终端路径，避免后台 PTY 误当作回显通过。
///
/// @author kongweiguang
async fn scoped_agent_target(h: &Harness, composition: &DesktopComposition, endpoint: &str) {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let host_id = HostId::parse("right-rail-fixture").unwrap();
    let session = composition
        .sessions
        .create(CreateSessionRequest {
            target: SessionTarget::Ssh(SshSessionTarget {
                host_id: host_id.clone(),
                working_directory: Some(text(&h.fixture, "remoteSftpDirectory").into()),
                command: None,
            }),
            owner: SessionOwner::Ui {
                tab_id: tab_id.clone(),
                pane_id: pane_id.clone(),
            },
            size: TerminalSize::default(),
            initial_palette: None,
        })
        .await
        .unwrap();
    let mut binding = AgentTargetBinding::unbound(0);
    binding.terminal_session_id = Some(session.session_id);
    binding.tab_id = Some(tab_id);
    binding.pane_id = Some(pane_id);
    binding.target_kind = Some("ssh".into());
    binding.target_ref = Some(host_id.to_string());
    binding.live_status = AgentTargetLiveStatus::Ready;
    agent_context(h, composition, endpoint, binding).await;
}

/// 从实际生成的端点连入，并在同一客户端验证开关实时生效、命令回读和目标失效。
async fn agent_context(
    h: &Harness,
    composition: &DesktopComposition,
    endpoint: &str,
    binding: AgentTargetBinding,
) {
    let terminal_id = binding.terminal_session_id.as_ref().unwrap().to_string();
    let settings = composition.preferences.load_settings().await.unwrap();
    let mut updated = settings.settings;
    let definition = CustomAgentDefinition::new(
        "MCP acceptance local CLI",
        "Write-Output 'LOCAL_AGENT_READY'; Start-Sleep -Seconds 180",
    );
    let key = format!("custom:{}", definition.id);
    updated.agent_launcher = AgentLauncherSettings {
        selected_agent_key: key,
        custom_agents: vec![definition],
    };
    updated.terminal.agent_commands_in_terminal = true;
    composition
        .preferences
        .save_settings(updated, settings.revision)
        .await
        .unwrap();
    let agent = composition
        .agent_manager
        .create(
            CreateAgentSessionRequest {
                title: Some("MCP live acceptance".into()),
                scope: AgentSessionScope::Global,
                target: binding,
                launcher_key: None,
                initial_palette: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let id = agent.session.agent_session_id.as_str();
    let context_file = Path::new(&agent.session.session_root).join("context/mcp-endpoint.json");
    let generated: Value = serde_json::from_slice(&std::fs::read(context_file).unwrap()).unwrap();
    let scoped_endpoint = text(&generated, "endpoint");
    assert_eq!(generated["agentSessionId"], id);
    assert_eq!(scoped_endpoint, format!("{endpoint}/agents/{id}"));
    let client = ClientInfo::default()
        .serve(StreamableHttpClientTransport::from_uri(scoped_endpoint))
        .await
        .unwrap();
    assert_eq!(client.peer().list_all_tools().await.unwrap().len(), 21);
    let call = async |tool: &'static str, args: Value, expect_error: bool| {
        let result = timeout(
            Duration::from_secs(30),
            client.peer().call_tool(
                CallToolRequestParams::new(tool).with_arguments(args.as_object().unwrap().clone()),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let value = result.structured_content.unwrap();
        assert_eq!(
            result.is_error.unwrap_or(false),
            expect_error,
            "{tool}: {value}"
        );
        value
    };
    let value = call(
        "kerminal.agent.target_context",
        json!({"agentSessionId": id}),
        false,
    )
    .await;
    assert_eq!(
        value["data"]["agentSession"]["session"]["agent_session_id"],
        id
    );
    assert_eq!(value["data"]["terminal"]["session"]["id"], terminal_id);
    let generation = value["data"]["targetBinding"]["generation"]
        .as_u64()
        .unwrap();
    let ssh_args = json!({"name": "Right rail fixture", "command": "printf MCP_AGENT_BACKGROUND"});
    let blocked = call("ssh.command", ssh_args.clone(), true).await;
    assert_eq!(blocked["data"]["route"], "visible");
    assert_eq!(blocked["data"]["hostId"], "right-rail-fixture");
    let visible_write =
        json!({"bindingGeneration": generation, "data": "printf 'MCP_AGENT_%s\\n' 'VISIBLE'\r"});
    let written = call("terminal.write", visible_write.clone(), false).await;
    assert_eq!(
        written["data"]["bytesWritten"],
        text(&visible_write, "data").len()
    );
    wait_output(h, &terminal_id, "MCP_AGENT_VISIBLE").await;
    let readback = call(
        "terminal.snapshot",
        json!({"sessionId": terminal_id}),
        false,
    )
    .await;
    assert!(text(&readback["data"]["snapshot"], "data").contains("MCP_AGENT_VISIBLE"));

    let settings = composition.preferences.load_settings().await.unwrap();
    let mut updated = settings.settings;
    updated.terminal.agent_commands_in_terminal = false;
    composition
        .preferences
        .save_settings(updated, settings.revision)
        .await
        .unwrap();
    call("terminal.write", visible_write.clone(), true).await;
    let background = call("ssh.command", ssh_args.clone(), false).await;
    assert_eq!(background["data"]["output"]["exitCode"], 0);
    assert_eq!(
        background["data"]["output"]["stdout"],
        "MCP_AGENT_BACKGROUND"
    );
    let readback = call(
        "terminal.snapshot",
        json!({"sessionId": terminal_id}),
        false,
    )
    .await;
    assert!(!text(&readback["data"]["snapshot"], "data").contains("MCP_AGENT_BACKGROUND"));

    let settings = composition.preferences.load_settings().await.unwrap();
    let mut updated = settings.settings;
    updated.terminal.agent_commands_in_terminal = true;
    composition
        .preferences
        .save_settings(updated, settings.revision)
        .await
        .unwrap();
    h.call("terminal.close", json!({"sessionId": terminal_id}))
        .await;
    call("terminal.write", visible_write, true).await;
    assert_eq!(
        call("ssh.command", ssh_args, true).await["data"]["route"],
        "visible"
    );
    h.verified
        .lock()
        .await
        .insert("kerminal.agent.target_context".into());
    println!("live MCP OK: generated Agent endpoint and bound SSH routing");
    client.cancel().await.unwrap();
    composition
        .agent_manager
        .archive(agent.session.agent_session_id, CancellationToken::new())
        .await
        .unwrap();
}
