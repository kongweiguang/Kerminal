//! Pi native MCP configuration regression tests.
//!
//! @author kongweiguang

use super::*;
use kerminal_lib::models::agent_session::{PI_AGENT_LAUNCH_COMMAND, PI_AGENT_RESUME_COMMAND};

#[test]
/// 验证 Pi session 使用原生配置，并把旧 provider/launch 命令归一后再恢复。
fn prepare_pi_agent_session_uses_native_mcp_configuration() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3031/mcp".to_owned()),
        true,
    );
    let agent_session_id = "ags_pi_20260824";
    let scoped_endpoint = format!("http://127.0.0.1:3031/mcp/agents/{agent_session_id}");
    let session_root = temp
        .path()
        .join("agents")
        .join("sessions")
        .join(agent_session_id);
    let native_config_path = session_root.join(".pi").join("mcp.json");
    fs::create_dir_all(native_config_path.parent().expect("Pi config parent"))
        .expect("create Pi config parent");
    fs::write(
        &native_config_path,
        serde_json::json!({
            "customSetting": "preserve-me",
            "mcpServers": {
                "other": {"type": "http", "url": "http://other/mcp"},
                "kerminal": {"type": "http", "url": "http://old/mcp", "timeout": 1}
            }
        })
        .to_string(),
    )
    .expect("seed Pi native config");

    let start = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "pi".to_owned(),
            agent_session_id: Some(agent_session_id.to_owned()),
            custom_command: None,
            resume_provider_session: false,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("prepare PI session");
    assert_agent_launch_command(&start, PI_AGENT_LAUNCH_COMMAND);
    assert_eq!(start.title, "PI Agent");
    assert_eq!(start.cwd, path_to_string(&session_root));
    assert_session_env(
        &start,
        agent_session_id,
        temp.path(),
        &session_root,
        &scoped_endpoint,
    );
    assert!(session_root.join("AGENTS.md").is_file());
    assert!(session_root.join(".mcp.json").is_file());
    assert!(native_config_path.is_file());
    assert!(!session_root.join("CLAUDE.md").exists());
    assert!(!session_root.join(".codex").exists());
    let mcp: Value =
        serde_json::from_str(&fs::read_to_string(&native_config_path).expect("PI MCP config"))
            .expect("PI MCP JSON");
    assert_eq!(
        mcp.pointer("/mcpServers/kerminal/url")
            .and_then(Value::as_str),
        Some(scoped_endpoint.as_str())
    );
    assert_eq!(
        mcp.pointer("/mcpServers/kerminal/timeout")
            .and_then(Value::as_u64),
        Some(60)
    );
    assert_eq!(
        mcp.pointer("/mcpServers/other/url").and_then(Value::as_str),
        Some("http://other/mcp")
    );
    assert_eq!(
        mcp.pointer("/customSetting").and_then(Value::as_str),
        Some("preserve-me")
    );
    let initial_config_operation = start
        .operations
        .iter()
        .find(|operation| operation.path == path_to_string(&native_config_path))
        .expect("Pi config operation");
    assert!(initial_config_operation
        .backup_path
        .as_deref()
        .is_some_and(|path| std::path::Path::new(path).is_file()));
    let endpoint_context: Value = serde_json::from_str(
        &fs::read_to_string(session_root.join("context").join("mcp-endpoint.json"))
            .expect("session MCP endpoint context"),
    )
    .expect("MCP endpoint JSON");
    assert_eq!(
        endpoint_context
            .pointer("/endpoint")
            .and_then(Value::as_str),
        Some(scoped_endpoint.as_str())
    );

    let store = AgentSessionFileStore::new(temp.path());
    let session_id = AgentSessionId::new(agent_session_id.to_owned()).expect("session id");
    store
        .write_session(&AgentSession {
            schema_version: AGENT_SESSION_SCHEMA_VERSION,
            agent_session_id: session_id.clone(),
            agent_id: AgentId::Pi,
            launcher_key: None,
            title: "PI Agent".to_owned(),
            created_at: "20260824120000".to_owned(),
            updated_at: "20260824120000".to_owned(),
            status: AgentSessionStatus::Active,
            workspace_root: path_to_string(temp.path()),
            session_root: path_to_string(&session_root),
            launch: AgentSessionLaunch {
                command_label: "pi --approve --mcp-config .mcp.json".to_owned(),
                shell: "pi".to_owned(),
                args: vec![
                    "--approve".to_owned(),
                    "--mcp-config".to_owned(),
                    ".mcp.json".to_owned(),
                    "--continue".to_owned(),
                ],
                cwd: path_to_string(&session_root),
            },
            scope: Some(AgentSessionScope::Global),
            target: None,
        })
        .expect("write legacy session snapshot");
    let mut legacy_provider = AgentProviderSession::for_agent(AgentId::Pi);
    legacy_provider.resume_command =
        Some("pi --approve --mcp-config .mcp.json --continue".to_owned());
    store
        .write_provider(&session_id, &legacy_provider)
        .expect("write legacy provider command");
    fs::write(
        &native_config_path,
        serde_json::json!({
            "mcpServers": {
                "other": {"type": "http", "url": "http://other/mcp"},
                "kerminal": {"type": "http", "url": "http://stale/mcp", "timeout": 5}
            }
        })
        .to_string(),
    )
    .expect("seed stale session endpoint");

    let resumed = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "pi".to_owned(),
            agent_session_id: Some(agent_session_id.to_owned()),
            custom_command: None,
            resume_provider_session: true,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("resume PI session");
    assert_agent_launch_command(&resumed, PI_AGENT_RESUME_COMMAND);
    assert_eq!(resumed.cwd, path_to_string(&session_root));
    let resumed_config: Value = serde_json::from_str(
        &fs::read_to_string(&native_config_path).expect("updated Pi native config"),
    )
    .expect("updated Pi MCP JSON");
    assert_eq!(
        resumed_config
            .pointer("/mcpServers/kerminal/url")
            .and_then(Value::as_str),
        Some(scoped_endpoint.as_str())
    );
    assert_eq!(
        resumed_config
            .pointer("/mcpServers/kerminal/timeout")
            .and_then(Value::as_u64),
        Some(60)
    );
    assert_eq!(
        resumed_config
            .pointer("/mcpServers/other/url")
            .and_then(Value::as_str),
        Some("http://other/mcp")
    );
    let endpoint_context: Value = serde_json::from_str(
        &fs::read_to_string(session_root.join("context").join("mcp-endpoint.json"))
            .expect("refreshed session MCP endpoint context"),
    )
    .expect("refreshed MCP endpoint JSON");
    assert_eq!(
        endpoint_context
            .pointer("/endpoint")
            .and_then(Value::as_str),
        Some(scoped_endpoint.as_str())
    );
    let saved = store
        .read_session(&session_id)
        .expect("read resumed session");
    assert_eq!(saved.launch.command_label, PI_AGENT_RESUME_COMMAND);
    assert!(!saved.launch.command_label.contains("--mcp-config"));
    assert_launch_parts(
        &saved.launch.shell,
        &saved.launch.args,
        PI_AGENT_RESUME_COMMAND,
    );
}

#[test]
/// 验证 Pi dry-run 展示原生配置变更但不写文件，也不改历史启动快照。
fn prepare_pi_agent_session_dry_run_preserves_native_config_and_launch_snapshot() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3032/mcp".to_owned()),
        true,
    );
    let agent_session_id = "ags_pi_dry_run_20260824";
    let session_root = temp
        .path()
        .join("agents")
        .join("sessions")
        .join(agent_session_id);
    let native_config_path = session_root.join(".pi").join("mcp.json");
    fs::create_dir_all(native_config_path.parent().expect("Pi config parent"))
        .expect("create Pi config parent");
    let original_config =
        r#"{"mcpServers":{"kerminal":{"type":"http","url":"http://old/mcp","timeout":1}}}"#;
    fs::write(&native_config_path, original_config).expect("write existing Pi config");

    let store = AgentSessionFileStore::new(temp.path());
    let session_id = AgentSessionId::new(agent_session_id.to_owned()).expect("session id");
    store
        .write_session(&AgentSession {
            schema_version: AGENT_SESSION_SCHEMA_VERSION,
            agent_session_id: session_id.clone(),
            agent_id: AgentId::Pi,
            launcher_key: None,
            title: "PI Agent".to_owned(),
            created_at: "20260824120000".to_owned(),
            updated_at: "20260824120000".to_owned(),
            status: AgentSessionStatus::Active,
            workspace_root: path_to_string(temp.path()),
            session_root: path_to_string(&session_root),
            launch: AgentSessionLaunch {
                command_label: "pi --approve --mcp-config .mcp.json".to_owned(),
                shell: "pi".to_owned(),
                args: vec!["--mcp-config".to_owned(), ".mcp.json".to_owned()],
                cwd: path_to_string(&session_root),
            },
            scope: Some(AgentSessionScope::Global),
            target: None,
        })
        .expect("write Pi session");
    let mut provider = AgentProviderSession::for_agent(AgentId::Pi);
    provider.resume_command = Some("pi --approve --mcp-config .mcp.json --continue".to_owned());
    store
        .write_provider(&session_id, &provider)
        .expect("write Pi provider");

    let spec = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "pi".to_owned(),
            agent_session_id: Some(agent_session_id.to_owned()),
            custom_command: None,
            resume_provider_session: true,
            dry_run: true,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("preview Pi resume");

    assert!(spec.dry_run);
    assert_agent_launch_command(&spec, PI_AGENT_RESUME_COMMAND);
    assert!(spec.operations.iter().all(|operation| operation.dry_run));
    assert!(spec.operations.iter().any(|operation| {
        operation.path == path_to_string(&native_config_path) && operation.changed
    }));
    assert!(!temp.path().join(".pi").join("mcp.json").exists());
    assert_eq!(
        fs::read_to_string(&native_config_path).expect("read unchanged Pi config"),
        original_config
    );
    let saved = store
        .read_session(&session_id)
        .expect("read unchanged session");
    assert_eq!(
        saved.launch.command_label,
        "pi --approve --mcp-config .mcp.json"
    );
    assert!(saved.launch.command_label.contains("--mcp-config"));
}
