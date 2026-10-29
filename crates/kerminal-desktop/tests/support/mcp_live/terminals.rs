//! 验收真实本地/SSH PTY 与绑定真实 UI owner 的 Agent scoped HTTP 上下文。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString, time::Duration};

use kerminal_application::{
    SessionService,
    agents::{AgentService, CreateAgentSessionRequest},
    preferences::settings::{AgentLauncherSettings, CustomAgentDefinition},
    sessions::{CreateSessionRequest, SessionFilter},
};
use kerminal_domain::{
    agents::{AgentSessionScope, AgentTargetBinding, AgentTargetLiveStatus},
    identity::{PaneId, SessionId, TabId},
    sessions::{LocalSessionTarget, SessionOwner, SessionTarget, TerminalSize},
};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ClientInfo},
    transport::StreamableHttpClientTransport,
};
use serde_json::json;
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

/// 用明确 UI owner 的真实 PTY 建立 Agent 目标，禁止借用当前窗口或隐式重连。
///
/// @author kongweiguang
async fn scoped_agent_target(h: &Harness, composition: &DesktopComposition, endpoint: &str) {
    let pane_id = PaneId::new();
    let session = composition
        .sessions
        .create(CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from("pwsh"),
                args: vec![OsString::from("-NoLogo"), OsString::from("-NoProfile")],
                working_directory: Some(h.root.clone()),
                environment: BTreeMap::new(),
            }),
            owner: SessionOwner::Ui {
                tab_id: TabId::new(),
                pane_id: pane_id.clone(),
            },
            size: TerminalSize::default(),
            initial_palette: None,
        })
        .await
        .unwrap();
    let id = session.session_id.as_str();
    h.call(
        "terminal.write",
        json!({"sessionId": id, "data": "Write-Output ('MCP_AGENT_TARGET_' + 'EXECUTED')\r"}),
    )
    .await;
    wait_output(h, id, "MCP_AGENT_TARGET_EXECUTED").await;
    agent_context(h, composition, endpoint, id).await;
    h.call("terminal.close", json!({"sessionId": id})).await;
}

/// Custom Agent 仅运行本地无收费 CLI 的等待进程，仍经过正式持久化和 session-scoped HTTP 校验。
async fn agent_context(
    h: &Harness,
    composition: &DesktopComposition,
    endpoint: &str,
    local_id: &str,
) {
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
    composition
        .preferences
        .save_settings(updated, settings.revision)
        .await
        .unwrap();
    let mut binding = AgentTargetBinding::unbound(0);
    binding.terminal_session_id = Some(SessionId::parse(local_id).unwrap());
    let target = composition
        .sessions
        .list(SessionFilter::default())
        .await
        .unwrap()
        .into_iter()
        .find(|session| session.session_id.as_str() == local_id)
        .unwrap();
    let SessionOwner::Ui { tab_id, pane_id } = target.owner else {
        panic!("Agent fixture requires an explicit UI owner");
    };
    binding.tab_id = Some(tab_id);
    binding.pane_id = Some(pane_id);
    binding.target_kind = Some("local".into());
    binding.live_status = AgentTargetLiveStatus::Ready;
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
    let client = ClientInfo::default()
        .serve(StreamableHttpClientTransport::from_uri(format!(
            "{endpoint}/agents/{id}"
        )))
        .await
        .unwrap();
    assert_eq!(client.peer().list_all_tools().await.unwrap().len(), 21);
    let result = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("kerminal.agent.target_context")
                .with_arguments(json!({"agentSessionId": id}).as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    assert_ne!(
        result.is_error,
        Some(true),
        "scoped target context: {:?}",
        result.structured_content
    );
    let value = result.structured_content.unwrap();
    assert_eq!(
        value["data"]["agentSession"]["session"]["agent_session_id"],
        id
    );
    assert_eq!(value["data"]["terminal"]["session"]["id"], local_id);
    assert!(
        text(&value["data"]["terminal"]["snapshot"], "data").contains("MCP_AGENT_TARGET_EXECUTED")
    );
    h.verified
        .lock()
        .await
        .insert("kerminal.agent.target_context".into());
    println!("live MCP OK: kerminal.agent.target_context");
    client.cancel().await.unwrap();
    composition
        .agent_manager
        .archive(agent.session.agent_session_id, CancellationToken::new())
        .await
        .unwrap();
}
