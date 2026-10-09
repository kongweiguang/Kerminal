//! External agent workspace behavior tests.
//!
//! @author kongweiguang

#[path = "external_agent_workspace/pi_native_mcp.rs"]
mod pi_native_mcp;
mod support;

use std::{fs, path::Path};

use kerminal_lib::services::external_agent_workspace::{
    rules, ExternalAgentFileAction, ExternalAgentOverwritePolicy, ExternalAgentWorkspaceService,
    PrepareExternalAgentWorkspaceRequest,
};
use serde_json::Value;
#[cfg(windows)]
use support::external_agent_workspace::windows_command_available;
use support::external_agent_workspace::{
    assert_agent_launch_command, assert_launch_parts, path_to_string, prepare_custom_command_spec,
};

const CONFIG_REFERENCE_FILE_NAME: &str = "kerminal-config.md";
const MANAGED_BLOCK_START: &str = "<!-- KERMINAL_EXTERNAL_AGENT_START -->";

#[test]
/// 验证 Codex 模板保留用户内容，并生成与 MCP canonical SFTP 契约一致的配置指南。
fn prepare_codex_writes_managed_files_without_clobbering_user_content() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3000/mcp".to_owned()),
        true,
    );
    fs::write(temp.path().join("AGENTS.md"), "# User notes\n").expect("seed agents");

    let spec = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "codex".to_owned(),
            agent_session_id: None,
            custom_command: None,
            resume_provider_session: false,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("prepare codex");

    assert_agent_launch_command(&spec, "codex");
    assert_eq!(spec.cwd, path_to_string(temp.path()));
    let agents = fs::read_to_string(temp.path().join("AGENTS.md")).expect("agents");
    assert!(agents.contains("# User notes"));
    assert!(agents.contains(MANAGED_BLOCK_START));
    let codex = fs::read_to_string(temp.path().join(".codex").join("config.toml")).expect("codex");
    assert!(codex.contains("[mcp_servers.kerminal]"));
    assert!(codex.contains("http://127.0.0.1:3000/mcp"));
    let config_reference =
        fs::read_to_string(temp.path().join(CONFIG_REFERENCE_FILE_NAME)).expect("config guide");
    assert!(config_reference.contains("Kerminal Configuration Guide"));
    assert!(config_reference.contains("File Relationships"));
    assert!(config_reference.contains("Required Field Matrix"));
    assert!(config_reference.contains("Common Change Recipes"));
    assert!(config_reference.contains("Runtime MCP Boundaries"));
    assert!(config_reference.contains("Terminal Execution"));
    assert!(config_reference.contains("global terminal scope across all Kerminal tabs"));
    assert!(config_reference.contains("targetBinding"));
    assert!(config_reference.contains("External MCP calls default to background execution"));
    assert!(config_reference.contains("For non-interactive SSH commands"));
    assert!(config_reference.contains("An interactive shell does not imply UI-Tab interaction"));
    assert!(config_reference.contains("when the task completes"));
    assert!(config_reference.contains("Only when the user explicitly asks to operate"));
    assert!(config_reference.contains("must not fall back to a visible Tab"));
    assert!(config_reference.contains("not replaced with another Tab"));
    assert!(!config_reference.contains("For ordinary commands and interactive work, inspect"));
    assert!(config_reference.contains("kerminal.app_guide"));
    assert!(config_reference.contains("kerminal.config_guide"));
    assert!(config_reference.contains("kerminal.tool_help"));
    assert!(config_reference.contains("container.files.write_text"));
    assert!(config_reference.contains("container.files.delete"));
    assert!(config_reference.contains("[agentLauncher]"));
    assert!(config_reference.contains("[[agentLauncher.customAgents]]"));
    assert!(config_reference.contains("Custom commands are stored as plaintext"));
    assert!(config_reference.contains("hosts/groups.toml"));
    assert!(config_reference.contains("Host creation checklist"));
    assert!(config_reference.contains("schema_version = 2"));
    assert!(config_reference.contains("every host uses schema v2 with an explicit `protocol`"));
    assert!(config_reference.contains("`ssh`, `sftp`, `rdp`, `telnet`, or `serial`"));
    assert!(config_reference.contains("normal loader rejects schema v1"));
    assert!(config_reference.contains("one-time startup upgrader"));
    assert!(config_reference.contains("recoverable transaction backup"));
    assert!(config_reference.contains("protocol = \"sftp\""));
    assert!(config_reference.contains("SFTP-only hosts may use proxy, jump, and transfer settings"));
    assert!(config_reference.contains("## SFTP Transfers"));
    assert!(config_reference
        .contains("canonical `source`, `destination`, `kind`, and `conflictPolicy`"));
    assert!(config_reference.contains(
        "local -> remote, remote -> local, same-host remote copy, and cross-host remote copy"
    ));
    assert!(
        config_reference.contains("\"source\": { \"type\": \"remote\", \"hostId\": \"server-a\"")
    );
    assert!(config_reference
        .contains("\"destination\": { \"type\": \"remote\", \"hostId\": \"server-b\""));
    assert!(config_reference.contains("sftp.transfer.list` with the returned `transfer.id`"));
    assert!(config_reference.contains("sftp.transfer.retry"));
    assert!(config_reference.contains("retryable=true"));
    assert!(config_reference.contains("resumable=false` starts a fresh transfer"));
    assert!(config_reference.contains("one idempotent successor"));
    assert!(config_reference.contains("automatically reconnects once"));
    assert!(config_reference.contains("failureKind = \"commitUnknown\""));
    assert!(config_reference.contains("transportMode` automatically"));
    assert!(config_reference.contains("[sftp].idleTimeoutSeconds"));
    assert!(config_reference.contains("60-second MCP client call guard"));
    assert!(config_reference.contains("failureKind = \"idleTimeout\""));
    assert!(config_reference.contains("sftp.upload`, `sftp.upload_directory`"));
    assert!(config_reference.contains("Do not use SFTP transfer for local -> local copies"));
    assert!(config_reference.contains(r#"cwd = "~/.kerminal""#));
    assert!(config_reference.contains(r#"credential_ref = "~/.ssh/id_ed25519""#));
    assert!(config_reference.contains("secrets/vault.toml"));
    assert!(config_reference.contains("secret_ref"));
    assert!(config_reference.contains("key_passphrase_ref"));
    assert!(config_reference.contains("inline_private_key"));
    assert!(config_reference.contains("[[ssh_options.jump_hosts]]"));
    assert!(config_reference.contains("kerminal.host.upsert_with_credential"));
    assert!(config_reference.contains("kerminal.vault.encrypt_secret"));
    assert!(config_reference.contains("Never add plaintext keys such as `password`"));
    assert!(!config_reference.contains("secrets/hosts/*.toml"));
    assert!(!config_reference.contains("production ="));
    assert!(!config_reference.contains("`production`"));
    assert!(config_reference.contains("kerminal.config.validate"));
    assert!(!config_reference.contains("validate-kerminal-config.mjs"));
    assert!(config_reference.contains("macOS/Linux example"));
    assert!(config_reference.contains(r#"shell = "zsh""#));
    assert!(config_reference.contains("Windows example"));
    assert!(config_reference.contains(r#"shell = "pwsh""#));
    assert!(config_reference.contains("Kerminal expands `~`, `~/...`, and `~\\...`"));
    assert!(config_reference.contains("kerminal.config.validate"));
    assert!(!config_reference.contains("validate-kerminal-config.mjs"));
    assert!(!config_reference.contains("requires Node.js"));
    assert!(!config_reference.contains("C:/Users/me"));
    assert!(!config_reference.contains("C:/dev"));
    assert!(agents.contains("Kerminal runtime workspace"));
    assert!(agents.contains("Operate Kerminal through MCP"));
    assert!(agents.contains("MCP host policy owns confirmation"));
    assert!(agents.contains("Prefer direct file edits"));
    assert!(agents.contains("terminal.write"));
    assert!(agents.contains("targetBinding"));
    assert!(agents.contains("External MCP calls default to background execution"));
    assert!(agents.contains("non-interactive SSH commands"));
    assert!(agents.contains("headless and does not open a Tab"));
    assert!(agents.contains("An interactive shell does not imply UI-Tab interaction"));
    assert!(agents.contains("when the task completes"));
    assert!(agents.contains("Only when the user explicitly asks to operate"));
    assert!(agents.contains("must not fall back to a visible Tab"));
    assert!(agents.contains("not replaced with another Tab"));
    assert!(!agents.contains("For ordinary commands, inspect the current `targetBinding`"));
    assert!(agents.contains("sessionId"));
    assert!(agents.contains("tab"));
    assert!(agents.contains("global"));
    assert!(agents.contains("terminal.reconnect"));
    assert!(!agents.contains("bindingGeneration"));
    assert!(!agents.contains("rebind"));
    assert!(agents.contains("kerminal.agent.target_context"));
    assert!(agents.contains("kerminal.app_guide"));
    assert!(agents.contains("kerminal.config_guide"));
    assert!(agents.contains("kerminal.capabilities"));
    assert!(agents.contains("kerminal.tool_help"));
    assert!(agents.contains("kerminal.operation_guide"));
    assert!(agents.contains("kerminal.runtime_snapshot"));
    assert!(agents.contains("sftp.transfer.retry"));
    assert!(agents.contains("failureKind=commitUnknown"));
    assert!(agents.contains("Before editing Kerminal configuration files"));
    assert!(agents.contains(CONFIG_REFERENCE_FILE_NAME));
    assert!(agents.contains("Do not use Kerminal MCP tools for settings"));
    assert!(agents.contains("remote_host.*"));
    assert!(agents.contains("history.search"));
    assert!(agents.contains("tmux.*"));
    assert!(agents.contains("container.files.write_text"));
    assert!(agents.contains("container.files.delete"));
    assert!(agents.contains("kerminal.config.validate"));
    assert!(agents.contains("kerminal.host.upsert_with_credential"));
    assert!(agents.contains("kerminal.vault.encrypt_secret"));
    assert!(agents.contains("host files only keep `secret_ref` / `key_passphrase_ref`"));
    assert!(
        agents.contains("never write `password =`, `credential_secret`, or `inline_private_key`")
    );
    assert!(!agents.contains("use `credential_secret`, never `password`"));
    assert!(!agents.contains("validate-kerminal-config.mjs"));
    assert!(!agents.contains("C:/Users"));
    assert!(!agents.contains("C:\\Users"));
    assert!(!agents.contains("C:/dev"));
    assert!(!agents.contains("C:\\dev"));
}

#[test]
/// 启动初始化同时生成各内置 provider 的原生配置文件，供全局 workspace 直接发现。
fn default_initialization_writes_codex_claude_and_config_guide_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3002/mcp".to_owned()),
        false,
    );

    service.ensure_default_agent_files().expect("default files");

    assert!(temp.path().join("AGENTS.md").is_file());
    assert!(temp.path().join("CLAUDE.md").is_file());
    assert!(temp.path().join(CONFIG_REFERENCE_FILE_NAME).is_file());
    assert!(temp.path().join(".codex").join("config.toml").is_file());
    assert!(temp.path().join(".mcp.json").is_file());
    assert!(temp.path().join(".pi").join("mcp.json").is_file());
    assert!(!temp.path().join("custom-agent.toml").exists());
    assert!(!temp.path().join(".custom-agent").exists());
}

#[test]
/// 验证 Claude 模板与 Codex 使用相同的 tab/global 终端 scope 规则。
fn prepare_claude_merges_mcp_json() {
    let temp = tempfile::tempdir().expect("tempdir");
    fs::write(
        temp.path().join(".mcp.json"),
        r#"{"mcpServers":{"other":{"type":"http","url":"http://x"}}}"#,
    )
    .expect("seed mcp");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3001/mcp".to_owned()),
        true,
    );

    let spec = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "claude".to_owned(),
            agent_session_id: None,
            custom_command: None,
            resume_provider_session: false,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("prepare claude");

    assert_agent_launch_command(&spec, "claude");
    let root: Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join(".mcp.json")).expect("mcp"))
            .expect("json");
    assert_eq!(
        root.pointer("/mcpServers/other/url")
            .and_then(Value::as_str),
        Some("http://x")
    );
    assert_eq!(
        root.pointer("/mcpServers/kerminal/url")
            .and_then(Value::as_str),
        Some("http://127.0.0.1:3001/mcp")
    );
    let claude = fs::read_to_string(temp.path().join("CLAUDE.md")).expect("claude");
    assert!(claude.contains("@AGENTS.md"));
    assert!(claude.contains("Follow `AGENTS.md` first"));
    assert!(claude.contains("Kerminal runtime workspace"));
    assert!(claude.contains("MCP host policy owns confirmation"));
    assert!(claude.contains("terminal.write"));
    assert!(claude.contains("targetBinding"));
    assert!(claude.contains("External MCP calls default to background execution"));
    assert!(claude.contains("For non-interactive SSH commands"));
    assert!(claude.contains("headless `terminal.create`"));
    assert!(claude.contains("returned `sessionId` explicitly"));
    assert!(claude.contains("Do not open, switch, or create a UI Tab unless"));
    assert!(claude.contains("An interactive shell does not imply UI-Tab interaction"));
    assert!(claude.contains("when the task completes"));
    assert!(claude.contains("Only when the user explicitly asks to operate"));
    assert!(claude.contains("must not fall back to a visible Tab"));
    assert!(claude.contains("not replaced with another Tab"));
    assert!(!claude.contains("For ordinary commands and interactive work, use `terminal.snapshot`"));
    assert!(claude.contains("sessionId"));
    assert!(claude.contains("terminal.reconnect"));
    assert!(!claude.contains("bindingGeneration"));
    assert!(!claude.contains("rebind"));
    assert!(claude.contains("kerminal.app_guide"));
    assert!(claude.contains("kerminal.config_guide"));
    assert!(claude.contains("kerminal.capabilities"));
    assert!(claude.contains("kerminal.tool_help"));
    assert!(claude.contains("kerminal.operation_guide"));
    assert!(claude.contains("kerminal.runtime_snapshot"));
    assert!(claude.contains("context/target-binding.json"));
    assert!(claude.contains("context/terminal-snapshot.json"));
    assert!(claude.contains(CONFIG_REFERENCE_FILE_NAME));
    assert!(claude.contains("Prefer direct file edits"));
    assert!(claude.contains("Do not use Kerminal MCP tools for settings"));
    assert!(claude.contains("remote_host.*"));
    assert!(claude.contains("history.search"));
    assert!(claude.contains("tmux"));
    assert!(claude.contains("container.files.write_text"));
    assert!(claude.contains("container.files.delete"));
    assert!(claude.contains("ssh.command_on_resolved_host"));
    assert!(claude.contains("server_info.snapshot"));
    assert!(claude.contains("kerminal.config.validate"));
    assert!(claude.contains("kerminal.host.upsert_with_credential"));
    assert!(claude.contains("kerminal.vault.encrypt_secret"));
    assert!(claude.contains("key_passphrase_ref"));
    assert!(claude.contains("inline_private_key"));
    assert!(!claude.contains("use `credential_secret`, never `password`"));
    assert!(!claude.contains("validate-kerminal-config.mjs"));
}

#[test]
fn prepare_custom_uses_explicit_command_without_default_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3003/mcp".to_owned()),
        true,
    );

    let spec = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "custom".to_owned(),
            agent_session_id: None,
            custom_command: Some("qwen --model max".to_owned()),
            resume_provider_session: false,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("prepare custom");

    assert_agent_launch_command(&spec, "qwen --model max");
    assert_eq!(spec.cwd, path_to_string(temp.path()));
    assert!(!temp.path().join("custom-agent.toml").exists());
    assert!(!temp.path().join(".custom-agent").exists());
    assert!(!temp.path().join(".codex").exists());
    assert!(!temp.path().join(".mcp.json").exists());
}

#[test]
fn custom_command_parser_preserves_windows_paths() {
    let plain = prepare_custom_command_spec(r#"C:\Tools\kimi.exe --fast"#);
    assert_launch_parts(&plain.shell, &plain.args, r#"C:\Tools\kimi.exe --fast"#);

    let quoted = prepare_custom_command_spec(r#""C:\Program Files\Kimi\kimi.exe" --fast"#);
    assert_launch_parts(
        &quoted.shell,
        &quoted.args,
        r#""C:\Program Files\Kimi\kimi.exe" --fast"#,
    );
}

#[test]
fn windows_agent_launch_command_prefers_pwsh_when_available() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3004/mcp".to_owned()),
        true,
    );

    let spec = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "codex".to_owned(),
            agent_session_id: None,
            custom_command: None,
            resume_provider_session: false,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("prepare codex");

    assert_launch_parts(&spec.shell, &spec.args, "codex");
    #[cfg(windows)]
    if windows_command_available("pwsh.exe") {
        assert_eq!(spec.shell, "pwsh.exe");
    }
}

#[test]
fn windows_executable_names_skip_extensionless_shims() {
    let names = rules::executable_names("codex")
        .into_iter()
        .map(|name| name.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    #[cfg(windows)]
    {
        assert!(names
            .iter()
            .any(|name| name.eq_ignore_ascii_case("codex.cmd")));
        assert!(!names.iter().any(|name| name == "codex"));
    }

    #[cfg(not(windows))]
    {
        assert_eq!(names, vec!["codex"]);
    }
}

#[test]
fn dry_run_codex_reports_diff_without_writing_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3010/mcp".to_owned()),
        true,
    );

    let spec = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "codex".to_owned(),
            agent_session_id: None,
            custom_command: None,
            resume_provider_session: false,
            dry_run: true,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("dry run codex");

    assert!(spec.dry_run);
    assert_eq!(spec.operations.len(), 3);
    assert!(spec.operations.iter().all(|operation| operation.dry_run));
    assert!(spec.operations.iter().all(|operation| operation.changed));
    assert!(spec.operations.iter().any(|operation| operation
        .diff
        .as_deref()
        .unwrap_or_default()
        .contains("3010")));
    assert!(!temp.path().join("AGENTS.md").exists());
    assert!(!temp.path().join(CONFIG_REFERENCE_FILE_NAME).exists());
    assert!(!temp.path().join(".codex").exists());
}

#[test]
fn endpoint_change_updates_managed_codex_table_and_reports_backup() {
    let temp = tempfile::tempdir().expect("tempdir");
    let codex_path = temp.path().join(".codex").join("config.toml");
    fs::create_dir_all(codex_path.parent().expect("parent")).expect("mkdir");
    fs::write(
        &codex_path,
        r#"[profile]
name = "keep"

[mcp_servers.kerminal]
url = "http://127.0.0.1:3000/mcp"

[mcp_servers.other]
url = "http://other"
"#,
    )
    .expect("seed codex");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3011/mcp".to_owned()),
        true,
    );

    let spec = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "codex".to_owned(),
            agent_session_id: None,
            custom_command: None,
            resume_provider_session: false,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("prepare codex");

    let codex_operation = spec
        .operations
        .iter()
        .find(|operation| operation.path.ends_with("config.toml"))
        .expect("codex operation");
    assert_eq!(codex_operation.action, ExternalAgentFileAction::Updated);
    let backup_path = codex_operation.backup_path.as_ref().expect("backup path");
    assert!(Path::new(backup_path).is_file());
    assert!(codex_operation
        .diff
        .as_deref()
        .unwrap_or_default()
        .contains("3011"));

    let next = fs::read_to_string(&codex_path).expect("codex");
    assert!(next.contains("name = \"keep\""));
    assert!(next.contains("[mcp_servers.other]"));
    assert!(next.contains("http://127.0.0.1:3011/mcp"));
    assert!(!next.contains("http://127.0.0.1:3000/mcp"));
}

#[test]
fn preserve_policy_rejects_invalid_claude_mcp_json() {
    let temp = tempfile::tempdir().expect("tempdir");
    fs::write(temp.path().join(".mcp.json"), "not json").expect("seed mcp");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3012/mcp".to_owned()),
        true,
    );

    let error = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "claude".to_owned(),
            agent_session_id: None,
            custom_command: None,
            resume_provider_session: false,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::PreserveUserContent,
        })
        .expect_err("invalid json should fail");

    assert!(error.to_string().contains("backupAndReplaceInvalid"));
    assert_eq!(
        fs::read_to_string(temp.path().join(".mcp.json")).expect("mcp"),
        "not json"
    );
}

#[test]
fn default_policy_repairs_invalid_claude_mcp_json_with_backup() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mcp_path = temp.path().join(".mcp.json");
    fs::write(&mcp_path, "not json").expect("seed mcp");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3013/mcp".to_owned()),
        true,
    );

    let spec = service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "claude".to_owned(),
            agent_session_id: None,
            custom_command: None,
            resume_provider_session: false,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("repair mcp json");

    let mcp_operation = spec
        .operations
        .iter()
        .find(|operation| operation.path.ends_with(".mcp.json"))
        .expect("mcp operation");
    assert_eq!(mcp_operation.action, ExternalAgentFileAction::Updated);
    let backup_path = mcp_operation.backup_path.as_ref().expect("backup path");
    assert_eq!(fs::read_to_string(backup_path).expect("backup"), "not json");
    let root: Value =
        serde_json::from_str(&fs::read_to_string(&mcp_path).expect("mcp")).expect("json");
    assert_eq!(
        root.pointer("/mcpServers/kerminal/url")
            .and_then(Value::as_str),
        Some("http://127.0.0.1:3013/mcp")
    );
}

#[test]
/// 验证无 session 的共享工作区说明 global scope、显式 sessionId 和断线恢复。
fn shared_agents_instructions_include_config_boundaries_and_validator() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = ExternalAgentWorkspaceService::new(
        temp.path(),
        Some("http://127.0.0.1:3014/mcp".to_owned()),
        true,
    );

    service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "codex".to_owned(),
            agent_session_id: None,
            custom_command: None,
            resume_provider_session: false,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("prepare codex");

    let agents = fs::read_to_string(temp.path().join("AGENTS.md")).expect("agents");
    assert!(agents.contains("settings.toml"));
    assert!(agents.contains(CONFIG_REFERENCE_FILE_NAME));
    assert!(agents.contains("file purposes"));
    assert!(agents.contains("profiles/*.toml"));
    assert!(agents.contains("hosts/*.toml"));
    assert!(agents.contains("data/"));
    assert!(agents.contains("logs/"));
    assert!(agents.contains("secrets/"));
    assert!(agents.contains("kerminal.config.validate"));
    assert!(!agents.contains("validate-kerminal-config.mjs"));
    assert!(agents.contains("Kerminal runtime workspace"));
    assert!(agents.contains("MCP host policy owns confirmation"));
    assert!(agents.contains("terminal.write"));
    assert!(agents.contains("targetBinding"));
    assert!(agents.contains("External MCP calls default to background execution"));
    assert!(agents.contains("For non-interactive SSH commands"));
    assert!(agents.contains("headless and does not open a Tab"));
    assert!(agents.contains("An interactive shell does not imply UI-Tab interaction"));
    assert!(agents.contains("when the task completes"));
    assert!(agents.contains("Only when the user explicitly asks to operate"));
    assert!(agents.contains("must not fall back to a visible Tab"));
    assert!(agents.contains("not replaced with another Tab"));
    assert!(!agents.contains("For ordinary commands, inspect the current `targetBinding`"));
    assert!(agents.contains("sessionId"));
    assert!(agents.contains("tab"));
    assert!(agents.contains("global"));
    assert!(agents.contains("terminal.reconnect"));
    assert!(!agents.contains("bindingGeneration"));
    assert!(!agents.contains("rebind"));
    assert!(agents.contains("kerminal.agent.target_context"));
    assert!(agents.contains("kerminal.app_guide"));
    assert!(agents.contains("kerminal.config_guide"));
    assert!(agents.contains("kerminal.capabilities"));
    assert!(agents.contains("kerminal.tool_help"));
    assert!(agents.contains("kerminal.operation_guide"));
    assert!(agents.contains("kerminal.runtime_snapshot"));
    assert!(agents.contains("context/target-binding.json"));
    assert!(agents.contains("context/terminal-snapshot.json"));
    assert!(agents.contains("Prefer direct file edits"));
    assert!(agents.contains("use MCP for runtime operation"));
    assert!(agents.contains("Do not use Kerminal MCP tools for settings"));
    assert!(agents.contains("settings.*"));
    assert!(agents.contains("terminal.create"));
    assert!(agents.contains("data/command.sqlite"));
    assert!(agents.contains("tmux.*"));
    assert!(agents.contains("container.files.write_text"));
    assert!(agents.contains("container.files.delete"));
    assert!(agents.contains("kerminal.host.upsert_with_credential"));
    assert!(agents.contains("kerminal.vault.encrypt_secret"));
}

#[test]
fn default_kerminal_workspace_paths_are_home_relative_in_agent_instructions() {
    let temp = tempfile::tempdir().expect("tempdir");
    let workspace_root = temp.path().join(".kerminal");
    let service = ExternalAgentWorkspaceService::new(
        &workspace_root,
        Some("http://127.0.0.1:3015/mcp".to_owned()),
        true,
    );
    let agent_session_id = "ags_display_20260624";

    service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "codex".to_owned(),
            agent_session_id: Some(agent_session_id.to_owned()),
            custom_command: None,
            resume_provider_session: false,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("prepare codex session");

    let session_agents = fs::read_to_string(
        workspace_root
            .join("agents")
            .join("sessions")
            .join(agent_session_id)
            .join("AGENTS.md"),
    )
    .expect("session agents");
    assert!(session_agents.contains("Kerminal workspace root: `~/.kerminal`"));
    assert!(session_agents
        .contains("This session root: `~/.kerminal/agents/sessions/ags_display_20260624`"));
    assert!(!session_agents.contains(&path_to_string(temp.path())));
    let agents = fs::read_to_string(workspace_root.join("AGENTS.md")).expect("agents");
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
    assert!(!agents.contains("validate-kerminal-config.mjs"));
    assert!(!agents.contains(&path_to_string(temp.path())));
}

#[test]
/// 验证全局外部 MCP 后台规则与内置右栏 session 的 targetBinding 规则保持分层且不互相覆盖。
fn session_workspace_keeps_right_panel_binding_distinct_from_global_mcp_default() {
    let temp = tempfile::tempdir().expect("tempdir");
    let workspace_root = temp.path().join(".kerminal");
    let service = ExternalAgentWorkspaceService::new(
        &workspace_root,
        Some("http://127.0.0.1:3016/mcp".to_owned()),
        true,
    );
    let agent_session_id = "ags_right_panel_policy";

    service
        .prepare(&PrepareExternalAgentWorkspaceRequest {
            agent_id: "claude".to_owned(),
            agent_session_id: Some(agent_session_id.to_owned()),
            custom_command: None,
            resume_provider_session: false,
            dry_run: false,
            overwrite_policy: ExternalAgentOverwritePolicy::BackupAndReplaceInvalid,
        })
        .expect("prepare Claude session");

    let global_agents =
        fs::read_to_string(workspace_root.join("AGENTS.md")).expect("global agents");
    assert!(global_agents.contains("External MCP calls default to background execution"));
    assert!(global_agents.contains("For non-interactive SSH commands"));
    assert!(!global_agents.contains("For ordinary commands, inspect the current `targetBinding`"));

    let session_root = workspace_root
        .join("agents")
        .join("sessions")
        .join(agent_session_id);
    let session_agents =
        fs::read_to_string(session_root.join("AGENTS.md")).expect("session agents");
    assert!(session_agents.contains("built-in Kerminal right-panel Agent"));
    assert!(session_agents.contains("exception to the external-MCP background default"));
    assert!(
        session_agents.contains("use the session's `targetBinding` as the bound terminal target")
    );
    assert!(session_agents.contains("terminal.snapshot` for the current `targetBinding`"));
    assert!(session_agents.contains("visible PTY"));
    assert!(session_agents.contains("sftp.transfer.retry"));
    assert!(session_agents.contains("failureKind=commitUnknown"));
    assert!(!session_agents.contains("External MCP calls default to background execution"));

    let session_claude =
        fs::read_to_string(session_root.join("CLAUDE.md")).expect("session Claude");
    assert!(session_claude.contains("built-in right-panel Agent session workspace"));
    assert!(session_claude
        .contains("existing `targetBinding` binding and scope/permissions remain in force"));
    assert!(session_claude.contains("visible PTY"));
    assert!(!session_claude.contains("External MCP calls default to background execution"));
}
