//! Pi native MCP configuration regression tests.
//!
//! @author kongweiguang

use super::*;
use kerminal_lib::models::agent_session::PI_AGENT_LAUNCH_COMMAND;

#[test]
/// 验证 Pi 写入原生项目配置并保留用户项；adapter 兼容字段不再控制 readiness。
fn prepare_pi_writes_native_mcp_and_reports_compatible_status() {
    let temp = tempfile::tempdir().expect("tempdir");
    let native_config_path = temp.path().join(".pi").join("mcp.json");
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
    .expect("seed Pi config");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3030/mcp".to_owned()),
        true,
    );

    let spec = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "pi".to_owned(),
            agent_session_id: None,
            custom_command: None,
            resume_provider_session: false,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("prepare PI");
    assert_agent_launch_command(&spec, PI_AGENT_LAUNCH_COMMAND);
    assert_eq!(spec.title, "PI Agent");
    assert_eq!(spec.cwd, path_to_string(temp.path()));
    assert!(temp.path().join("AGENTS.md").is_file());
    assert!(temp.path().join(CONFIG_REFERENCE_FILE_NAME).is_file());
    assert!(native_config_path.is_file());
    assert!(!temp.path().join(".mcp.json").exists());
    assert!(!temp.path().join("CLAUDE.md").exists());
    assert!(!temp.path().join(".codex").exists());

    let mcp: Value = serde_json::from_str(
        &fs::read_to_string(&native_config_path).expect("Pi native MCP config"),
    )
    .expect("Pi MCP JSON");
    assert_eq!(
        mcp.pointer("/mcpServers/kerminal/url")
            .and_then(Value::as_str),
        Some("http://127.0.0.1:3030/mcp")
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
    let config_operation = spec
        .operations
        .iter()
        .find(|operation| operation.path == path_to_string(&native_config_path))
        .expect("Pi config operation");
    assert!(config_operation.changed);
    let backup_path = config_operation
        .backup_path
        .as_deref()
        .expect("Pi config backup");
    assert!(Path::new(backup_path).is_file());

    let status = service.status();
    assert_eq!(status.agents.pi.id, "pi");
    assert_eq!(status.agents.pi.title, "PI Agent");
    assert_eq!(status.agents.pi.cli_command, PI_AGENT_LAUNCH_COMMAND);
    assert!(status.agents.pi.adapter_available);
    assert!(status.agents.pi.config_ready);
    assert_eq!(
        status.agents.pi.config_path,
        path_to_string(&native_config_path)
    );
    assert!(!status.agents.pi.status_detail.contains("adapter"));
    let wire = serde_json::to_value(&status.agents.pi).expect("serialize PI status");
    assert_eq!(
        wire.get("adapterAvailable").and_then(Value::as_bool),
        Some(true)
    );
}
