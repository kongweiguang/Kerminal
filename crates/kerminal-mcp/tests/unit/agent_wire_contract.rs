//! Agent MCP 响应与 v0.3.45 记录及 snapshot 形状的独立契约测试。
//!
//! @author kongweiguang

use std::collections::BTreeSet;

use kerminal_application::{
    AppError,
    agents::{
        AgentMcpEndpointContextProjection, AgentResumeCapability, AgentResumeRecovery,
        AgentResumeRecoveryStatus, AgentSessionContextPathsProjection,
        AgentSessionLaunchProjection, AgentSessionPathsProjection, AgentSessionRecordDiagnostic,
        AgentSessionRecordProjection, AgentSessionRecordSessionProjection, AgentSessionView,
        AgentTargetBindingContextBindingProjection, AgentTargetBindingContextProjection,
        AgentTargetContext, AgentTargetResolution,
    },
    sessions::PlainTextSnapshot,
};
use kerminal_domain::{
    agents::{
        AgentProvider, AgentProviderSession, AgentSession, AgentSessionLaunch, AgentSessionScope,
        AgentSessionStatus, AgentTargetBinding, AgentTargetLiveStatus,
    },
    identity::{AgentSessionId, PaneId, SessionId, TabId},
};
use serde_json::{Value, json};

use super::{
    resume_recovery_data, session_record_value, target_binding_entity_value, target_binding_value,
    target_context_value,
};

/// 构造经领域构造器校验的固定业务值，使 wire 断言能够检查真实字段映射。
fn record_fixture() -> (
    AgentSessionRecordProjection,
    AgentSession,
    AgentTargetResolution,
) {
    let agent_id = AgentSessionId::new();
    let target_session_id = SessionId::new();
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let target = AgentTargetBinding {
        binding_id: Some("binding-fixture-17".to_owned()),
        generation: 17,
        pane_id: Some(pane_id.clone()),
        tab_id: Some(tab_id.clone()),
        terminal_session_id: Some(target_session_id.clone()),
        target_ref: Some("local:developer".to_owned()),
        target_kind: Some("local".to_owned()),
        cwd: Some("C:\\work\\project".to_owned()),
        shell: Some("pwsh.exe".to_owned()),
        live_status: AgentTargetLiveStatus::Ready,
        last_seen_at: Some("1792200000000".to_owned()),
    };
    let launch = AgentSessionLaunch {
        command_label: "codex".to_owned(),
        shell: "codex.exe".to_owned(),
        args: vec!["--resume".to_owned(), "provider-session-42".to_owned()],
        cwd: "C:\\work\\project".to_owned(),
    };
    let session = AgentSession::new(
        agent_id.clone(),
        AgentProvider::Codex,
        None,
        "Workspace assistant".to_owned(),
        "1792200000000".to_owned(),
        "C:\\Users\\tester\\.kerminal".to_owned(),
        "C:\\Users\\tester\\.kerminal\\agents\\sessions\\fixture".to_owned(),
        launch,
        Some(AgentSessionScope::Tab {
            tab_id: tab_id.clone(),
        }),
        Some(target.clone()),
    )
    .expect("fixture domain session must be valid");
    let projection = AgentSessionRecordProjection {
        session: AgentSessionRecordSessionProjection {
            schema_version: 1,
            agent_session_id: agent_id.to_string(),
            agent_id: AgentProvider::Codex,
            launcher_key: Some("codex/default".to_owned()),
            title: session.title.clone(),
            created_at: session.created_at.clone(),
            updated_at: session.updated_at.clone(),
            status: AgentSessionStatus::Active,
            workspace_root: session.workspace_root.clone(),
            session_root: session.session_root.clone(),
            launch: AgentSessionLaunchProjection {
                command_label: session.launch.command_label.clone(),
                shell: session.launch.shell.clone(),
                args: session.launch.args.clone(),
                cwd: session.launch.cwd.clone(),
            },
            scope: session.scope.clone(),
            target: session.target.clone(),
        },
        provider: Some(AgentProviderSession {
            schema_version: 1,
            provider: AgentProvider::Codex,
            provider_session_id: Some("provider-session-42".to_owned()),
            resume_command: Some("codex resume provider-session-42".to_owned()),
            resume_supported: true,
            last_resume_at: Some("1792200000000".to_owned()),
        }),
        resume_recovery: None,
        target_binding: Some(AgentTargetBindingContextProjection {
            schema_version: 1,
            agent_session_id: agent_id.to_string(),
            scope: Some(AgentSessionScope::Global),
            binding: AgentTargetBindingContextBindingProjection {
                binding_id: target.binding_id.clone(),
                generation: target.generation,
                status: "ready".to_owned(),
                stale: false,
                pane_id: Some(pane_id.to_string()),
                tab_id: Some(tab_id.to_string()),
                target_terminal_session_id: Some(target_session_id.to_string()),
                target_ref: target.target_ref.clone(),
                cwd: target.cwd.clone(),
                shell: target.shell.clone(),
            },
            agent_terminal: Some(
                kerminal_application::agents::AgentTerminalContextProjection {
                    session_id: Some("agent-cli-session-5".to_owned()),
                    status: "running".to_owned(),
                },
            ),
            generated_at: "1792200000001".to_owned(),
        }),
        mcp_endpoint: Some(AgentMcpEndpointContextProjection {
            schema_version: 1,
            agent_session_id: agent_id.to_string(),
            endpoint: Some("http://127.0.0.1:37657/mcp/agents/fixture".to_owned()),
            transport: "streamable-http".to_owned(),
            tools_only: true,
            generated_at: "1792200000002".to_owned(),
        }),
        paths: AgentSessionPathsProjection {
            workspace_root: session.workspace_root.clone(),
            session_root: session.session_root.clone(),
            session_toml: "C:\\Users\\tester\\.kerminal\\agents\\sessions\\fixture\\session.toml"
                .to_owned(),
            provider_toml:
                "C:\\Users\\tester\\.kerminal\\agents\\sessions\\fixture\\provider.toml"
                    .to_owned(),
            context: AgentSessionContextPathsProjection {
                target_binding_json:
                    "C:\\Users\\tester\\.kerminal\\agents\\sessions\\fixture\\context\\target-binding.json"
                        .to_owned(),
                mcp_endpoint_json:
                    "C:\\Users\\tester\\.kerminal\\agents\\sessions\\fixture\\context\\mcp-endpoint.json"
                        .to_owned(),
                terminal_snapshot_json:
                    "C:\\Users\\tester\\.kerminal\\agents\\sessions\\fixture\\context\\terminal-snapshot.json"
                        .to_owned(),
                workspace_snapshot_json:
                    "C:\\Users\\tester\\.kerminal\\agents\\sessions\\fixture\\context\\workspace-snapshot.json"
                        .to_owned(),
            },
        },
        diagnostics: vec![AgentSessionRecordDiagnostic {
            path: Some("C:\\Users\\tester\\.kerminal\\agents\\sessions\\fixture\\provider.toml".to_owned()),
            code: "providerRecovered".to_owned(),
            message: "Provider metadata is available".to_owned(),
            line: Some(1),
            column: Some(1),
        }],
    };
    let resolution = AgentTargetResolution {
        agent_session_id: agent_id,
        binding: target,
        resolved_session_id: Some(target_session_id),
        status: AgentTargetLiveStatus::Ready,
        observed_at_ms: 1792200000003,
    };
    (projection, session, resolution)
}

/// 对比完整旧 AgentSessionRecord wire key 集合，并确认 launch env 未跨 MCP 暴露。
///
/// @author kongweiguang
#[test]
fn current_session_keeps_full_legacy_record_shape_and_omits_launch_environment() {
    let (projection, _, _) = record_fixture();
    let value = session_record_value(&projection).expect("wire record must serialize");

    assert_eq!(
        object_keys(&value),
        keys(&[
            "diagnostics",
            "mcpEndpoint",
            "paths",
            "provider",
            "session",
            "targetBinding",
        ])
    );
    assert_eq!(
        object_keys(&value["session"]),
        keys(&[
            "agent_id",
            "agent_session_id",
            "created_at",
            "launch",
            "launcher_key",
            "scope",
            "session_root",
            "status",
            "target",
            "title",
            "updated_at",
            "workspace_root",
            "schema_version",
        ])
    );
    assert_eq!(value["session"]["status"], "active");
    assert_eq!(value["session"]["target"]["live_status"], "ready");
    assert_eq!(value["session"]["scope"]["kind"], "tab");
    assert_eq!(
        object_keys(&value["session"]["launch"]),
        keys(&["args", "command_label", "cwd", "shell"])
    );
    assert!(value["session"]["launch"].get("env").is_none());
    assert_eq!(
        object_keys(&value["session"]["target"]),
        keys(&[
            "binding_generation",
            "binding_id",
            "cwd",
            "last_seen_at",
            "live_status",
            "pane_id",
            "shell",
            "tab_id",
            "target_kind",
            "target_ref",
            "target_terminal_session_id",
        ])
    );
    assert_eq!(
        object_keys(&value["provider"]),
        keys(&[
            "last_resume_at",
            "provider",
            "provider_session_id",
            "resume_command",
            "resume_supported",
            "schema_version",
        ])
    );
    assert_eq!(
        object_keys(&value["targetBinding"]),
        keys(&[
            "agentSessionId",
            "agentTerminal",
            "binding",
            "generatedAt",
            "schemaVersion",
            "scope",
        ])
    );
    assert_eq!(
        object_keys(&value["targetBinding"]["binding"]),
        keys(&[
            "bindingId",
            "cwd",
            "generation",
            "paneId",
            "shell",
            "stale",
            "status",
            "tabId",
            "targetRef",
            "targetTerminalSessionId",
        ])
    );
    assert_eq!(value["targetBinding"]["binding"]["status"], "ready");
    assert_eq!(
        object_keys(&value["mcpEndpoint"]),
        keys(&[
            "agentSessionId",
            "endpoint",
            "generatedAt",
            "schemaVersion",
            "toolsOnly",
            "transport",
        ])
    );
    assert_eq!(
        object_keys(&value["paths"]),
        keys(&[
            "context",
            "providerToml",
            "sessionRoot",
            "sessionToml",
            "workspaceRoot",
        ])
    );
    assert_eq!(
        object_keys(&value["paths"]["context"]),
        keys(&[
            "mcpEndpointJson",
            "targetBindingJson",
            "terminalSnapshotJson",
            "workspaceSnapshotJson",
        ])
    );
    assert_eq!(
        object_keys(&value["diagnostics"][0]),
        keys(&["code", "column", "line", "message", "path"])
    );
    assert_eq!(value["provider"]["provider"], "codex");
    assert_eq!(
        value["provider"]["provider_session_id"],
        "provider-session-42"
    );
    assert_eq!(value["targetBinding"]["binding"]["generation"], 17);
    assert_eq!(value["mcpEndpoint"]["transport"], "streamable-http");
    assert!(value["paths"]["context"]["terminalSnapshotJson"].is_string());
}

/// 保留旧 AgentSession 对缺失可选 scope/target 的 null 编码，避免读取方区分语义漂移。
#[test]
fn current_session_preserves_null_optional_session_fields() {
    let (mut projection, _, _) = record_fixture();
    projection.session.scope = None;
    projection.session.target = None;
    let value = session_record_value(&projection).expect("wire record must serialize");

    assert!(value["session"].get("scope").is_some());
    assert!(value["session"]["scope"].is_null());
    assert!(value["session"].get("target").is_some());
    assert!(value["session"]["target"].is_null());
}

/// 对比旧 targetBinding snapshot 与 entity 字段名，并拒绝不完整的无绑定结果。
#[test]
fn target_resolution_keeps_legacy_snapshot_and_entity_names() {
    let (_, _, resolution) = record_fixture();
    let binding = target_binding_value(&resolution).expect("binding must serialize");
    let entity = target_binding_entity_value(&resolution).expect("entity must serialize");

    assert_eq!(
        object_keys(&binding),
        keys(&[
            "agentSessionId",
            "bindingId",
            "cwd",
            "generation",
            "live",
            "paneId",
            "shell",
            "stale",
            "status",
            "tabId",
            "targetRef",
            "targetTerminalSessionId",
            "updatedAtMs",
        ])
    );
    assert_eq!(binding["status"], "live");
    assert_eq!(binding["updatedAtMs"], 1792200000003_u64);
    assert_eq!(entity["type"], "agentTargetBinding");
    assert_eq!(
        object_keys(&entity),
        keys(&["agentSessionId", "status", "terminalSessionId", "type"])
    );
    assert_eq!(
        entity["terminalSessionId"],
        binding["targetTerminalSessionId"]
    );
    assert_eq!(entity["status"], "live");

    let mut unbound = resolution;
    unbound.status = AgentTargetLiveStatus::Unbound;
    unbound.binding = AgentTargetBinding::unbound(18);
    assert!(target_binding_value(&unbound).is_err());
}

/// 锁定 target_context wire 字段；内部恢复能力不属于该 MCP 契约。
///
/// @author kongweiguang
#[test]
fn target_context_keeps_legacy_top_level_and_snapshot_envelope() {
    let (projection, session, resolution) = record_fixture();
    let snapshot_session_id = resolution
        .resolved_session_id
        .clone()
        .expect("fixture target id");
    let context = AgentTargetContext {
        session: AgentSessionView {
            session,
            terminal: None,
            resume_recovery: None,
            resume_capability: AgentResumeCapability::MetadataUnavailable,
        },
        record: projection,
        target: resolution,
        snapshot: Some(PlainTextSnapshot {
            session_id: snapshot_session_id,
            text: "safe output".to_owned(),
            first_line: 10,
            line_count: 1,
            total_scrollback_lines: 15,
            truncated: true,
        }),
        captured_bytes: 32,
        redacted: true,
    };
    let value = target_context_value(
        &context,
        vec![json!({
            "sessionId": "user-session-1",
            "paneId": "user-pane-1",
            "tabId": "user-tab-1",
            "status": "running"
        })],
        Some(json!({
            "id": "user-session-1",
            "sessionId": "user-session-1",
            "owner": "ui",
            "shell": "pwsh.exe",
            "headless": false
        })),
        24 * 1024,
    )
    .expect("target context must serialize");

    assert_eq!(
        object_keys(&value),
        keys(&[
            "agentSession",
            "scope",
            "targetBinding",
            "terminal",
            "terminals",
        ])
    );
    assert_eq!(
        value["agentSession"]["session"]["agent_session_id"],
        context.target.agent_session_id.as_str()
    );
    assert_eq!(value["scope"]["kind"], "global");
    assert_eq!(value["terminals"][0]["paneId"], "user-pane-1");
    assert_eq!(
        object_keys(&value["terminal"]["snapshot"]),
        keys(&["capturedBytes", "data", "maxBytes", "redacted", "truncated",])
    );
    assert_eq!(value["terminal"]["snapshot"]["capturedBytes"], 32);
    assert_eq!(value["terminal"]["snapshot"]["maxBytes"], 24 * 1024);
    assert_eq!(value["terminal"]["snapshot"]["data"], "safe output");
    assert!(value["terminal"]["snapshot"].get("firstLine").is_none());
}

/// MCP 错误携带真实 terminalSessionId 与小写恢复状态，避免调用方误把未确认恢复当成功。
#[test]
fn resume_recovery_data_preserves_owner_and_explicit_wire_status() {
    let terminal_session_id = SessionId::new();
    let recovery = AgentResumeRecovery {
        terminal_session_id: terminal_session_id.clone(),
        status: AgentResumeRecoveryStatus::Pending,
    };

    assert_eq!(
        resume_recovery_data(&recovery),
        json!({
            "terminalSessionId": terminal_session_id.as_str(),
            "status": "pending"
        })
    );

    let unknown = AgentResumeRecovery {
        terminal_session_id: terminal_session_id.clone(),
        status: AgentResumeRecoveryStatus::Unknown,
    };
    assert_eq!(
        resume_recovery_data(&unknown),
        json!({
            "terminalSessionId": terminal_session_id.as_str(),
            "status": "unknown"
        })
    );

    let response = crate::terminal_tools::failure_with_data(
        AppError::outcome_unknown("Agent resume metadata needs readback"),
        resume_recovery_data(&recovery),
    );
    let response = response
        .structured_content
        .expect("resume recovery failure is structured");
    assert_eq!(response["errorKind"], "outcome_unknown");
    assert_eq!(response["recoverable"], false);
    assert_eq!(
        response["data"]["terminalSessionId"],
        terminal_session_id.as_str()
    );
    assert_eq!(response["data"]["status"], "pending");
}

/// 返回对象的精确 key 集合，避免只测部分字段而漏掉字段名回归。
fn object_keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .expect("wire object")
        .keys()
        .cloned()
        .collect()
}

/// 统一构造字段名集合，JSON object 的序列化顺序不属于契约。
fn keys(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}
