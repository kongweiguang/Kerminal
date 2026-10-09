//! External agent session workspace behavior tests.
//!
//! @author kongweiguang

#[path = "external_agent_workspace_session/pi_native_mcp.rs"]
mod pi_native_mcp;
mod support;

use std::fs;

use kerminal_lib::{
    models::agent_session::{
        AgentId, AgentProviderSession, AgentSession, AgentSessionId, AgentSessionLaunch,
        AgentSessionScope, AgentSessionStatus, AgentSessionTarget, AgentTargetLiveStatus,
        AGENT_SESSION_SCHEMA_VERSION,
    },
    services::{
        agent_session_file_store::AgentSessionFileStore,
        external_agent_workspace::{
            ExternalAgentOverwritePolicy, ExternalAgentWorkspaceService,
            PrepareExternalAgentWorkspaceRequest,
        },
    },
};
use serde_json::Value;
use support::external_agent_workspace::{
    assert_agent_launch_command, assert_launch_parts, assert_session_env, path_to_string,
};

const CONFIG_REFERENCE_FILE_NAME: &str = "kerminal-config.md";

#[test]
/// 验证 session workspace 模板写入 scope-aware MCP 说明及运行态环境变量。
fn prepare_codex_agent_session_workspace_writes_scoped_files_and_env() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3020/mcp".to_owned()),
        true,
    );
    let agent_session_id = "ags_20260624_203124_ab12";
    let scoped_endpoint = format!("http://127.0.0.1:3020/mcp/agents/{agent_session_id}");

    let spec = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "codex".to_owned(),
            agent_session_id: Some(agent_session_id.to_owned()),
            custom_command: None,
            resume_provider_session: false,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("prepare codex session");

    let session_root = temp
        .path()
        .join("agents")
        .join("sessions")
        .join(agent_session_id);
    assert_agent_launch_command(&spec, "codex");
    assert_eq!(spec.cwd, path_to_string(&session_root));
    assert_session_env(
        &spec,
        agent_session_id,
        temp.path(),
        &session_root,
        &scoped_endpoint,
    );
    assert!(temp.path().join("AGENTS.md").is_file());
    assert!(temp.path().join(CONFIG_REFERENCE_FILE_NAME).is_file());
    assert!(temp.path().join(".codex").join("config.toml").is_file());
    assert!(session_root.join("AGENTS.md").is_file());
    assert!(session_root.join("CLAUDE.md").is_file());
    assert!(session_root.join(".codex").join("config.toml").is_file());
    assert!(session_root.join(".mcp.json").is_file());
    assert!(session_root
        .join("context")
        .join("target-binding.json")
        .is_file());
    assert!(session_root
        .join("context")
        .join("terminal-snapshot.json")
        .is_file());

    let agents = fs::read_to_string(session_root.join("AGENTS.md")).expect("session agents");
    assert!(agents.contains(agent_session_id));
    assert!(agents.contains("Kerminal MCP is tools-only"));
    assert!(agents.contains("file-first"));
    assert!(agents.contains(CONFIG_REFERENCE_FILE_NAME));
    assert!(agents.contains("agentSessionId"));
    assert!(agents.contains("sessionId"));
    assert!(agents.contains("tab"));
    assert!(agents.contains("global"));
    assert!(agents.contains("terminal.reconnect"));
    assert!(!agents.contains("bindingGeneration"));
    assert!(agents.contains("terminal.write"));
    assert!(agents.contains("disconnected"));
    assert!(!agents.contains("rebind"));
    assert!(agents.contains("kerminal.config.validate"));
    assert!(agents.contains("kerminal.app_guide"));
    assert!(agents.contains("kerminal.config_guide"));
    assert!(agents.contains("kerminal.capabilities"));
    assert!(agents.contains("kerminal.tool_help"));
    assert!(agents.contains("kerminal.operation_guide"));
    assert!(agents.contains("kerminal.runtime_snapshot"));
    assert!(agents.contains("tmux.*"));
    assert!(agents.contains("container.files.write_text"));
    assert!(agents.contains("container.files.delete"));
    assert!(agents.contains("kerminal.host.upsert_with_credential"));
    assert!(agents.contains("kerminal.vault.encrypt_secret"));
    assert!(agents.contains("key_passphrase_ref"));
    assert!(agents.contains("inline_private_key"));
    assert!(!agents.contains("use `credential_secret`, never `password`"));
    assert!(!agents.contains("validate-kerminal-config.mjs"));

    let codex = fs::read_to_string(session_root.join(".codex").join("config.toml")).expect("codex");
    assert!(codex.contains("[mcp_servers.kerminal]"));
    assert!(codex.contains(&scoped_endpoint));

    let mcp_root: Value =
        serde_json::from_str(&fs::read_to_string(session_root.join(".mcp.json")).expect("mcp"))
            .expect("mcp json");
    assert_eq!(
        mcp_root
            .pointer("/mcpServers/kerminal/url")
            .and_then(Value::as_str),
        Some(scoped_endpoint.as_str())
    );

    let endpoint_context: Value = serde_json::from_str(
        &fs::read_to_string(session_root.join("context").join("mcp-endpoint.json"))
            .expect("endpoint context"),
    )
    .expect("endpoint json");
    assert_eq!(
        endpoint_context
            .pointer("/endpoint")
            .and_then(Value::as_str),
        Some(scoped_endpoint.as_str())
    );
    assert_eq!(
        endpoint_context
            .pointer("/agentSessionId")
            .and_then(Value::as_str),
        Some(agent_session_id)
    );
    assert_eq!(
        endpoint_context
            .pointer("/env/KERMINAL_AGENT_SESSION_ID")
            .and_then(Value::as_str),
        Some(agent_session_id)
    );
    assert_eq!(
        endpoint_context
            .pointer("/toolsOnly")
            .and_then(Value::as_bool),
        Some(true)
    );

    let target_context: Value = serde_json::from_str(
        &fs::read_to_string(session_root.join("context").join("target-binding.json"))
            .expect("target context"),
    )
    .expect("target json");
    assert_eq!(
        target_context
            .pointer("/agentSessionId")
            .and_then(Value::as_str),
        Some(agent_session_id)
    );
    assert_eq!(
        target_context
            .pointer("/scope/kind")
            .and_then(Value::as_str),
        Some("global")
    );

    let terminal_snapshot: Value = serde_json::from_str(
        &fs::read_to_string(session_root.join("context").join("terminal-snapshot.json"))
            .expect("terminal snapshot"),
    )
    .expect("terminal snapshot json");
    assert_eq!(
        terminal_snapshot
            .pointer("/agentSessionId")
            .and_then(Value::as_str),
        Some(agent_session_id)
    );
    assert_eq!(
        terminal_snapshot
            .pointer("/capturedBytes")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        terminal_snapshot.pointer("/output").and_then(Value::as_str),
        Some("")
    );
    assert_eq!(
        terminal_snapshot
            .pointer("/maxBytes")
            .and_then(Value::as_u64),
        Some(24 * 1024)
    );
}

#[test]
/// 验证旧 session TOML 的 tab metadata 会以 global 能力写入 target context。
fn prepare_agent_session_workspace_seeds_scope_from_session_toml() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3026/mcp".to_owned()),
        true,
    );
    let store = AgentSessionFileStore::new(temp.path());
    let agent_session_id = AgentSessionId::new("ags_bound_target_20260629".to_owned()).expect("id");
    let session_root = temp
        .path()
        .join("agents")
        .join("sessions")
        .join(agent_session_id.as_str());
    store
        .write_session(&AgentSession {
            schema_version: AGENT_SESSION_SCHEMA_VERSION,
            agent_session_id: agent_session_id.clone(),
            agent_id: AgentId::Codex,
            launcher_key: None,
            title: "Codex".to_owned(),
            created_at: "20260629200000".to_owned(),
            updated_at: "20260629200000".to_owned(),
            status: AgentSessionStatus::Active,
            workspace_root: path_to_string(temp.path()),
            session_root: path_to_string(&session_root),
            launch: AgentSessionLaunch {
                command_label: "codex".to_owned(),
                shell: "codex".to_owned(),
                args: Vec::new(),
                cwd: path_to_string(&session_root),
            },
            scope: Some(AgentSessionScope::Tab {
                tab_id: "tab-1".to_owned(),
            }),
            target: Some(AgentSessionTarget {
                binding_id: Some("binding-1".to_owned()),
                binding_generation: 7,
                pane_id: Some("pane-1".to_owned()),
                tab_id: Some("tab-1".to_owned()),
                target_terminal_session_id: Some("terminal-1".to_owned()),
                target_ref: Some("ssh:prod-web".to_owned()),
                target_kind: Some("ssh".to_owned()),
                cwd: Some("/srv/app".to_owned()),
                shell: Some("bash".to_owned()),
                live_status: AgentTargetLiveStatus::Ready,
                last_seen_at: Some("20260629200001".to_owned()),
            }),
        })
        .expect("write session");

    service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "codex".to_owned(),
            agent_session_id: Some(agent_session_id.as_str().to_owned()),
            custom_command: None,
            resume_provider_session: false,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("prepare codex session");

    let target_context: Value = serde_json::from_str(
        &fs::read_to_string(session_root.join("context").join("target-binding.json"))
            .expect("target context"),
    )
    .expect("target json");
    assert_eq!(
        target_context
            .pointer("/scope/kind")
            .and_then(Value::as_str),
        Some("global")
    );
}

#[test]
fn prepare_codex_agent_session_resume_uses_provider_resume_command() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3023/mcp".to_owned()),
        true,
    );
    let agent_session_id = "ags_codex_resume_20260624";
    let session_root = temp
        .path()
        .join("agents")
        .join("sessions")
        .join(agent_session_id);
    fs::create_dir_all(&session_root).expect("session root");
    fs::write(
        session_root.join("provider.toml"),
        toml::to_string_pretty(&AgentProviderSession::for_agent(AgentId::Codex))
            .expect("provider toml"),
    )
    .expect("write provider");

    let spec = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "codex".to_owned(),
            agent_session_id: Some(agent_session_id.to_owned()),
            custom_command: None,
            resume_provider_session: true,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("prepare codex resume session");

    assert_agent_launch_command(&spec, "codex resume --last");
    assert_eq!(spec.cwd, path_to_string(&session_root));
}

#[test]
fn prepare_claude_agent_session_resume_uses_directory_scoped_continue_command() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3024/mcp".to_owned()),
        true,
    );
    let agent_session_id = "ags_claude_resume_20260624";
    let session_root = temp
        .path()
        .join("agents")
        .join("sessions")
        .join(agent_session_id);
    fs::create_dir_all(&session_root).expect("session root");
    fs::write(
        session_root.join("provider.toml"),
        toml::to_string_pretty(&AgentProviderSession::for_agent(AgentId::Claude))
            .expect("provider toml"),
    )
    .expect("write provider");

    let spec = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "claude".to_owned(),
            agent_session_id: Some(agent_session_id.to_owned()),
            custom_command: None,
            resume_provider_session: true,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("prepare claude resume session");

    assert_agent_launch_command(&spec, "claude --continue");
    assert_eq!(spec.cwd, path_to_string(&session_root));
}

#[test]
fn prepare_claude_agent_session_resume_migrates_legacy_provider_without_command() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3024/mcp".to_owned()),
        true,
    );
    let agent_session_id = "ags_claude_legacy_resume_20260624";
    let session_root = temp
        .path()
        .join("agents")
        .join("sessions")
        .join(agent_session_id);
    fs::create_dir_all(&session_root).expect("session root");
    let mut legacy_provider = AgentProviderSession::for_agent(AgentId::Claude);
    legacy_provider.resume_command = None;
    fs::write(
        session_root.join("provider.toml"),
        toml::to_string_pretty(&legacy_provider).expect("legacy provider toml"),
    )
    .expect("write legacy provider");

    let spec = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "claude".to_owned(),
            agent_session_id: Some(agent_session_id.to_owned()),
            custom_command: None,
            resume_provider_session: true,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("prepare legacy claude resume session");

    assert_agent_launch_command(&spec, "claude --continue");
}

#[test]
/// 验证恢复 session 时 scope 信息可以从持久化 session 文件同步回来。
fn prepare_agent_session_resume_syncs_launch_back_to_session_toml() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3025/mcp".to_owned()),
        true,
    );
    let store = AgentSessionFileStore::new(temp.path());
    let agent_session_id =
        AgentSessionId::new("ags_codex_sync_resume_20260624".to_owned()).expect("id");
    let session_root = temp
        .path()
        .join("agents")
        .join("sessions")
        .join(agent_session_id.as_str());
    store
        .write_session(&AgentSession {
            schema_version: AGENT_SESSION_SCHEMA_VERSION,
            agent_session_id: agent_session_id.clone(),
            agent_id: AgentId::Codex,
            launcher_key: None,
            title: "Codex".to_owned(),
            created_at: "20260624200000".to_owned(),
            updated_at: "20260624200000".to_owned(),
            status: AgentSessionStatus::Active,
            workspace_root: path_to_string(temp.path()),
            session_root: path_to_string(&session_root),
            launch: AgentSessionLaunch {
                command_label: "codex".to_owned(),
                shell: "codex".to_owned(),
                args: Vec::new(),
                cwd: path_to_string(&session_root),
            },
            scope: Some(AgentSessionScope::Global),
            target: None,
        })
        .expect("write session");
    fs::write(
        session_root.join("provider.toml"),
        toml::to_string_pretty(&AgentProviderSession::for_agent(AgentId::Codex))
            .expect("provider toml"),
    )
    .expect("write provider");

    let spec = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "codex".to_owned(),
            agent_session_id: Some(agent_session_id.as_str().to_owned()),
            custom_command: None,
            resume_provider_session: true,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("prepare codex resume session");

    assert_agent_launch_command(&spec, "codex resume --last");
    let saved = store.read_session(&agent_session_id).expect("read session");
    assert_eq!(saved.launch.command_label, "codex resume --last");
    assert_eq!(saved.launch.cwd, path_to_string(&session_root));
    assert_launch_parts(
        &saved.launch.shell,
        &saved.launch.args,
        "codex resume --last",
    );
}

#[test]
/// 验证 Claude session workspace 也使用显式 scope member 和断线恢复流程。
fn prepare_claude_agent_session_workspace_writes_default_provider_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3021/mcp/".to_owned()),
        true,
    );
    let agent_session_id = "ags_claude_20260624";
    let scoped_endpoint = format!("http://127.0.0.1:3021/mcp/agents/{agent_session_id}");

    let spec = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "claude".to_owned(),
            agent_session_id: Some(agent_session_id.to_owned()),
            custom_command: None,
            resume_provider_session: false,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("prepare claude session");

    let session_root = temp
        .path()
        .join("agents")
        .join("sessions")
        .join(agent_session_id);
    assert_agent_launch_command(&spec, "claude");
    assert_eq!(spec.cwd, path_to_string(&session_root));
    assert_session_env(
        &spec,
        agent_session_id,
        temp.path(),
        &session_root,
        &scoped_endpoint,
    );

    let claude = fs::read_to_string(session_root.join("CLAUDE.md")).expect("claude");
    assert!(claude.contains("@AGENTS.md"));
    assert!(claude.contains("tools-only"));
    assert!(claude.contains("MCP host policy owns confirmation"));
    assert!(claude.contains("sessionId"));
    assert!(claude.contains("terminal.reconnect"));
    assert!(!claude.contains("bindingGeneration"));
    assert!(claude.contains(CONFIG_REFERENCE_FILE_NAME));
    assert!(!claude.contains("rebind"));
    assert!(claude.contains("kerminal.config.validate"));
    assert!(claude.contains("kerminal.app_guide"));
    assert!(claude.contains("kerminal.config_guide"));
    assert!(claude.contains("kerminal.capabilities"));
    assert!(claude.contains("kerminal.operation_guide"));
    assert!(claude.contains("kerminal.runtime_snapshot"));
    assert!(claude.contains("tmux.*"));
    assert!(claude.contains("container.files.write_text"));
    assert!(claude.contains("container.files.delete"));
    assert!(claude.contains("ssh.command_on_resolved_host"));
    assert!(claude.contains("server_info.snapshot"));
    assert!(claude.contains("kerminal.host.upsert_with_credential"));
    assert!(claude.contains("kerminal.vault.encrypt_secret"));
    assert!(claude.contains("key_passphrase_ref"));
    assert!(claude.contains("inline_private_key"));
    assert!(!claude.contains("use `credential_secret`, never `password`"));
    assert!(!claude.contains("validate-kerminal-config.mjs"));
    assert!(session_root.join(".codex").join("config.toml").is_file());
    let mcp_root: Value =
        serde_json::from_str(&fs::read_to_string(session_root.join(".mcp.json")).expect("mcp"))
            .expect("mcp json");
    assert_eq!(
        mcp_root
            .pointer("/mcpServers/kerminal/url")
            .and_then(Value::as_str),
        Some(scoped_endpoint.as_str())
    );
}

#[test]
/// 验证 Custom session 保留独立标题/命令快照，并生成通用 MCP 文件而非 provider 配置。
fn prepare_custom_agent_session_workspace_writes_standard_mcp_without_provider_configs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3022/mcp".to_owned()),
        true,
    );
    let agent_session_id = "ags_custom_20260624";
    let scoped_endpoint = format!("http://127.0.0.1:3022/mcp/agents/{agent_session_id}");
    let session_root = temp
        .path()
        .join("agents")
        .join("sessions")
        .join(agent_session_id);
    AgentSessionFileStore::new(temp.path())
        .write_session(&AgentSession {
            schema_version: AGENT_SESSION_SCHEMA_VERSION,
            agent_session_id: AgentSessionId::new(agent_session_id).expect("session id"),
            agent_id: AgentId::Custom,
            launcher_key: Some("custom:9d045678-983a-4ed1-ab39-bd46bccb1fa3".to_owned()),
            title: "PI Agent".to_owned(),
            created_at: "20260624200000".to_owned(),
            updated_at: "20260624200000".to_owned(),
            status: AgentSessionStatus::Active,
            workspace_root: path_to_string(temp.path()),
            session_root: path_to_string(&session_root),
            launch: AgentSessionLaunch {
                command_label: "qwen --model max".to_owned(),
                shell: "qwen".to_owned(),
                args: vec!["--model".to_owned(), "max".to_owned()],
                cwd: path_to_string(&session_root),
            },
            scope: Some(AgentSessionScope::Global),
            target: None,
        })
        .expect("write custom session snapshot");

    let spec = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "custom".to_owned(),
            agent_session_id: Some(agent_session_id.to_owned()),
            custom_command: Some("qwen --model max".to_owned()),
            resume_provider_session: false,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("prepare custom session");

    assert_agent_launch_command(&spec, "qwen --model max");
    assert_eq!(spec.title, "PI Agent");
    assert_eq!(spec.cwd, path_to_string(&session_root));
    assert_session_env(
        &spec,
        agent_session_id,
        temp.path(),
        &session_root,
        &scoped_endpoint,
    );
    assert!(session_root.join("AGENTS.md").is_file());
    assert!(session_root
        .join("context")
        .join("mcp-endpoint.json")
        .is_file());
    assert!(session_root
        .join("context")
        .join("target-binding.json")
        .is_file());
    assert!(session_root
        .join("context")
        .join("terminal-snapshot.json")
        .is_file());
    assert!(!session_root.join("CLAUDE.md").exists());
    assert!(!session_root.join(".codex").exists());
    let mcp_root: Value =
        serde_json::from_str(&fs::read_to_string(session_root.join(".mcp.json")).expect("mcp"))
            .expect("mcp json");
    assert_eq!(
        mcp_root
            .pointer("/mcpServers/kerminal/url")
            .and_then(Value::as_str),
        Some(scoped_endpoint.as_str())
    );

    let endpoint_context: Value = serde_json::from_str(
        &fs::read_to_string(session_root.join("context").join("mcp-endpoint.json"))
            .expect("endpoint context"),
    )
    .expect("endpoint json");
    assert_eq!(
        endpoint_context
            .pointer("/endpoint")
            .and_then(Value::as_str),
        Some(scoped_endpoint.as_str())
    );
    assert_eq!(
        endpoint_context
            .pointer("/toolsOnly")
            .and_then(Value::as_bool),
        Some(true)
    );
}
