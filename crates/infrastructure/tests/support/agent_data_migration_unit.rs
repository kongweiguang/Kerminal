//! Agent 会话一次迁移规划的隔离文件回归；所有样本仅使用合成身份与本地临时目录。
//!
//! @author kongweiguang

use std::{fs, path::Path};

use kerminal_application::{
    PublicErrorCode,
    agents::{AgentProviderSessionStatus, AgentWorkspaceRepository, initial_provider_session},
};
use kerminal_domain::{
    agents::{AgentProvider, AgentProviderSession, AgentSessionStatus, AgentTargetLiveStatus},
    identity::AgentSessionId,
};
use tokio_util::sync::CancellationToken;

use super::plan_agent_migration;
use crate::agents::FileAgentWorkspaceRepository;

/// 在临时根下写入当前 Store 可读取的 session.toml，并返回固定会话目录。
///
/// @author kongweiguang
fn write_session(root: &Path, id: &str, status: &str, target_status: &str) -> std::path::PathBuf {
    let session_root = root.join("agents").join("sessions").join(id);
    fs::create_dir_all(session_root.join("context")).expect("isolated session directory");
    let session_file = format!(
        r#"schema_version = 1
agent_session_id = "{id}"
agent_id = "codex"
launcher_key = "builtin:codex"
title = "Migration fixture"
created_at = "100"
updated_at = "101"
status = "{status}"
workspace_root = "C:/fixture/.kerminal"
session_root = "C:/fixture/.kerminal/agents/sessions/{id}"
scope = {{ kind = "global" }}

[launch]
command_label = "Codex"
shell = "codex"
args = ["--fixture"]
cwd = "C:/fixture/.kerminal/agents/sessions/{id}"

[target]
binding_id = "binding-fixture"
binding_generation = 7
pane_id = "pane-fixture"
tab_id = "tab-fixture"
target_terminal_session_id = "target-terminal-fixture"
target_ref = "local"
target_kind = "local"
cwd = "C:/fixture/workspace"
shell = "pwsh"
live_status = "{target_status}"
last_seen_at = "102"
"#
    );
    fs::write(session_root.join("session.toml"), session_file).expect("synthetic session file");
    session_root
}

/// 从已知有效样本派生 Custom session，使用合法 launcher key 验证迁移不猜测用户恢复命令。
///
/// @author kongweiguang
fn write_custom_session(root: &Path, id: &str, status: &str) -> std::path::PathBuf {
    let session_root = write_session(root, id, status, "closed");
    let path = session_root.join("session.toml");
    let original = fs::read_to_string(&path).expect("synthetic session text");
    let custom = original
        .replace("agent_id = \"codex\"", "agent_id = \"custom\"")
        .replace(
            "launcher_key = \"builtin:codex\"",
            "launcher_key = \"custom:00000000-0000-0000-0000-000000000001\"",
        )
        .replace(
            "command_label = \"Codex\"",
            "command_label = \"Custom Agent\"",
        )
        .replace("shell = \"codex\"", "shell = \"custom-agent\"");
    assert_ne!(custom, original, "fixture provider identity should change");
    fs::write(path, custom).expect("synthetic Custom session");
    session_root
}

/// 从同一合成 session 生成旧版 target 键，避免 fixture 与实际 session 其余字段分叉。
///
/// @author kongweiguang
fn write_legacy_target_alias_session(root: &Path, id: &str) -> std::path::PathBuf {
    write_legacy_target_alias_session_with_spellings(root, id, "generation", "terminalSessionId")
}

/// 生成同义历史字段的其它受支持写法，避免 mapper 只覆盖默认 alias。
///
/// @author kongweiguang
fn write_legacy_target_alias_session_with_spellings(
    root: &Path,
    id: &str,
    generation_alias: &str,
    terminal_session_alias: &str,
) -> std::path::PathBuf {
    let session_root = write_session(root, id, "active", "ready");
    let path = session_root.join("session.toml");
    let original = fs::read_to_string(&path).expect("synthetic session text");
    let aliases = [
        ("binding_id", "bindingId"),
        ("binding_generation", generation_alias),
        ("pane_id", "paneId"),
        ("tab_id", "tabId"),
        ("target_terminal_session_id", terminal_session_alias),
        ("target_ref", "targetRef"),
        ("target_kind", "targetKind"),
        ("live_status", "liveStatus"),
        ("last_seen_at", "lastSeenAt"),
    ];
    let mut in_target = false;
    let mut legacy = String::with_capacity(original.len());
    for line in original.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        if body.trim() == "[target]" {
            in_target = true;
        }
        if in_target && let Some((key, value)) = body.split_once('=') {
            let canonical = key.trim();
            if let Some((_, alias)) = aliases.iter().find(|(name, _)| *name == canonical) {
                legacy.push_str(alias);
                legacy.push_str(" =");
                legacy.push_str(value);
                legacy.push('\n');
                continue;
            }
        }
        legacy.push_str(line);
    }
    legacy = legacy.replace(
        "bindingId = \"binding-fixture\"",
        "bindingId = \"binding-fixture\" # retain target note",
    );
    fs::write(path, legacy).expect("legacy alias session file");
    session_root
}

/// 写入真实 provider 值对象序列化后的合法 v1 文件，避免测试只覆盖手写字段拼写。
///
/// @author kongweiguang
fn write_provider(session_root: &Path, provider: AgentProvider) -> Vec<u8> {
    let document = AgentProviderSession {
        schema_version: 1,
        provider,
        provider_session_id: Some("provider-session-fixture".to_owned()),
        resume_command: Some("codex resume --last".to_owned()),
        resume_supported: true,
        last_resume_at: Some("103".to_owned()),
    };
    let bytes = toml::to_string(&document)
        .expect("synthetic provider metadata should serialize")
        .into_bytes();
    fs::write(session_root.join("provider.toml"), &bytes).expect("synthetic provider file");
    bytes
}

/// 合成当前或旧版缺恢复命令的 provider 文件，并保留稳定 ID 与时间供迁移核对。
///
/// @author kongweiguang
fn write_provider_without_resume_command(
    session_root: &Path,
    provider: AgentProvider,
    provider_id: &str,
    resume_supported: bool,
) -> Vec<u8> {
    let document = AgentProviderSession {
        schema_version: 1,
        provider,
        provider_session_id: Some(provider_id.to_owned()),
        resume_command: None,
        resume_supported,
        last_resume_at: Some("103".to_owned()),
    };
    let bytes = toml::to_string(&document)
        .expect("synthetic legacy provider metadata should serialize")
        .into_bytes();
    fs::write(session_root.join("provider.toml"), &bytes)
        .expect("synthetic provider file without resume command");
    bytes
}

/// 构造与当前 Agent workspace writer 一致的 endpoint envelope。
///
/// @author kongweiguang
fn endpoint_document(id: &str, endpoint: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": 1,
        "agentSessionId": id,
        "endpoint": endpoint,
        "transport": "streamable-http",
        "toolsOnly": true,
        "generatedAt": "104",
        "futureMetadata": {"keep": true}
    })
}

/// 构造带目标定义与 Agent runtime owner 的当前 target-binding context。
///
/// @author kongweiguang
fn target_binding_document(
    id: &str,
    status: &str,
    stale: bool,
    agent_terminal_status: &str,
) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": 1,
        "agentSessionId": id,
        "scope": {"kind": "global"},
        "binding": {
            "bindingId": "binding-fixture",
            "generation": 7,
            "status": status,
            "stale": stale,
            "paneId": "pane-fixture",
            "tabId": "tab-fixture",
            "targetTerminalSessionId": "target-terminal-fixture",
            "targetRef": "local",
            "cwd": "C:/fixture/workspace",
            "shell": "pwsh",
            "futureBindingMetadata": "retain"
        },
        "agentTerminal": {
            "sessionId": "agent-terminal-fixture",
            "status": agent_terminal_status
        },
        "generatedAt": "105",
        "futureContextMetadata": {"retain": [1, 2, 3]}
    })
}

/// 构造 terminal-snapshot，并让 capturedBytes 与 UTF-8 输出长度真实一致。
///
/// @author kongweiguang
fn terminal_snapshot_document(id: &str, target_id: serde_json::Value) -> serde_json::Value {
    let output = "synthetic historical output\n";
    serde_json::json!({
        "schemaVersion": 1,
        "agentSessionId": id,
        "targetTerminalSessionId": target_id,
        "capturedBytes": output.len(),
        "maxBytes": 4096,
        "truncated": true,
        "redacted": true,
        "output": output,
        "generatedAt": "106",
        "futureSnapshotMetadata": "retain"
    })
}

/// 写入合成 JSON context 并返回提交前原始字节，供 planner 的纯读性质比较。
///
/// @author kongweiguang
fn write_context(session_root: &Path, name: &str, document: &serde_json::Value) -> Vec<u8> {
    let bytes = serde_json::to_vec_pretty(document).expect("synthetic context should serialize");
    let path = session_root.join("context").join(name);
    fs::write(path, &bytes).expect("synthetic context file");
    bytes
}

/// 从规范样本生成旧 context 头，确保迁移测试只改变已确认的两个历史字段名。
///
/// @author kongweiguang
fn legacy_context_headers(mut document: serde_json::Value) -> serde_json::Value {
    let object = document
        .as_object_mut()
        .expect("synthetic context envelope is an object");
    for (current, legacy) in [
        ("schemaVersion", "schema_version"),
        ("agentSessionId", "agent_session_id"),
    ] {
        let value = object
            .remove(current)
            .expect("canonical context header should exist");
        object.insert(legacy.to_owned(), value);
    }
    document
}

/// 旧 scope alias 与缺失 scope 只在一次性迁移中归一化，当前 Store 前后读回同一目标归属。
///
/// @author kongweiguang
#[tokio::test]
async fn legacy_scope_alias_and_missing_scope_migrate_to_current_required_scope() {
    let temporary = tempfile::tempdir().expect("isolated migration root");
    let root = temporary.path();
    let alias_id = "agent-migration-scope-alias-fixture";
    let alias_root = write_session(root, alias_id, "archived", "closed");
    write_provider(&alias_root, AgentProvider::Codex);
    let alias_path = alias_root.join("session.toml");
    let alias_text = fs::read_to_string(&alias_path).expect("synthetic session text");
    fs::write(
        &alias_path,
        alias_text.replace(
            "scope = { kind = \"global\" }",
            "scope = { kind = \"tab\", tab_id = \"tab-fixture\" }",
        ),
    )
    .expect("legacy tab scope alias");

    let missing_id = "agent-migration-scope-missing-fixture";
    let missing_root = write_session(root, missing_id, "archived", "closed");
    write_provider(&missing_root, AgentProvider::Codex);
    let missing_path = missing_root.join("session.toml");
    let missing_text = fs::read_to_string(&missing_path).expect("synthetic session text");
    fs::write(
        &missing_path,
        missing_text
            .lines()
            .filter(|line| !line.starts_with("scope ="))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .expect("legacy session without scope");

    let repository = FileAgentWorkspaceRepository::new(root.to_path_buf());
    for id in [alias_id, missing_id] {
        assert!(
            repository
                .read_session(
                    AgentSessionId::parse(id).expect("valid fixture id"),
                    CancellationToken::new(),
                )
                .await
                .is_err(),
            "ordinary reader must reject pre-migration scope for {id}"
        );
    }
    let alias_before = fs::read(&alias_path).expect("legacy alias bytes");
    let missing_before = fs::read(&missing_path).expect("missing-scope bytes");
    let edits = plan_agent_migration(root).expect("known scope formats should migrate");
    assert_eq!(edits.len(), 2);
    assert_eq!(
        fs::read(&alias_path).expect("planner is read-only"),
        alias_before
    );
    assert_eq!(
        fs::read(&missing_path).expect("planner is read-only"),
        missing_before
    );

    for edit in &edits {
        fs::write(root.join(&edit.relative_path), &edit.after)
            .expect("apply isolated scope migration plan");
    }
    for id in [alias_id, missing_id] {
        let snapshot = repository
            .read_session(
                AgentSessionId::parse(id).expect("valid fixture id"),
                CancellationToken::new(),
            )
            .await
            .expect("current reader accepts migrated required scope");
        assert_eq!(snapshot.session.agent_session_id.as_str(), id);
        assert_eq!(
            snapshot
                .session
                .scope
                .as_ref()
                .and_then(|scope| scope.tab_id())
                .map(|tab_id| tab_id.as_str()),
            Some("tab-fixture")
        );
        assert_eq!(
            snapshot
                .session
                .target
                .as_ref()
                .and_then(|target| target.tab_id.as_ref())
                .map(|tab_id| tab_id.as_str()),
            Some("tab-fixture")
        );
    }
    let canonical_alias: toml::Value =
        toml::from_str(&fs::read_to_string(&alias_path).expect("canonical alias session"))
            .expect("canonical session TOML");
    assert_eq!(canonical_alias["scope"]["kind"].as_str(), Some("tab"));
    assert_eq!(
        canonical_alias["scope"]["tabId"].as_str(),
        Some("tab-fixture")
    );
    assert!(canonical_alias["scope"].get("tab_id").is_none());
    assert!(
        plan_agent_migration(root)
            .expect("migration is idempotent")
            .is_empty()
    );
}

/// 同一 scope 同时出现 canonical 与旧字段名时，迁移拒绝猜测并保持输入字节不变。
///
/// @author kongweiguang
#[tokio::test]
async fn conflicting_scope_alias_is_rejected_without_rewriting_the_session() {
    let temporary = tempfile::tempdir().expect("isolated migration root");
    let root = temporary.path();
    let id = "agent-migration-scope-conflict-fixture";
    let session_root = write_session(root, id, "archived", "closed");
    write_provider(&session_root, AgentProvider::Codex);
    let path = session_root.join("session.toml");
    let original = fs::read_to_string(&path)
        .expect("synthetic session text")
        .replace(
            "scope = { kind = \"global\" }",
            "scope = { kind = \"tab\", tabId = \"tab-fixture\", tab_id = \"other-tab\" }",
        );
    fs::write(&path, &original).expect("conflicting synthetic scope");

    let repository = FileAgentWorkspaceRepository::new(root.to_path_buf());
    assert!(
        repository
            .read_session(
                AgentSessionId::parse(id).expect("valid fixture id"),
                CancellationToken::new(),
            )
            .await
            .is_err(),
        "ordinary reader must reject unknown and duplicate legacy scope keys"
    );
    assert!(plan_agent_migration(root).is_err());
    assert_eq!(
        fs::read(&path).expect("rejected input remains"),
        original.as_bytes()
    );
}

/// context 的旧 snake_case 身份头只由迁移重命名，且 JSON 主体与当前身份保持不变。
///
/// @author kongweiguang
#[test]
fn legacy_context_headers_migrate_even_when_runtime_references_are_already_clear() {
    let temporary = tempfile::tempdir().expect("isolated migration root");
    let root = temporary.path();
    let id = "agent-migration-context-header-fixture";
    let session_root = write_session(root, id, "archived", "closed");
    write_provider(&session_root, AgentProvider::Codex);
    let endpoint_before = write_context(
        &session_root,
        "mcp-endpoint.json",
        &legacy_context_headers(endpoint_document(id, serde_json::Value::Null)),
    );
    let mut target_binding = target_binding_document(id, "stale", true, "stopped");
    target_binding["agentTerminal"] = serde_json::Value::Null;
    let target_before = write_context(
        &session_root,
        "target-binding.json",
        &legacy_context_headers(target_binding),
    );
    let snapshot_before = write_context(
        &session_root,
        "terminal-snapshot.json",
        &legacy_context_headers(terminal_snapshot_document(id, serde_json::Value::Null)),
    );

    let edits = plan_agent_migration(root).expect("known context headers should migrate");
    assert_eq!(edits.len(), 3);
    assert_eq!(
        fs::read(session_root.join("context/mcp-endpoint.json")).expect("planner is read-only"),
        endpoint_before
    );
    assert_eq!(
        fs::read(session_root.join("context/target-binding.json")).expect("planner is read-only"),
        target_before
    );
    assert_eq!(
        fs::read(session_root.join("context/terminal-snapshot.json"))
            .expect("planner is read-only"),
        snapshot_before
    );
    for edit in &edits {
        fs::write(root.join(&edit.relative_path), &edit.after)
            .expect("apply isolated context header migration");
    }

    let endpoint: serde_json::Value = serde_json::from_slice(
        &fs::read(session_root.join("context/mcp-endpoint.json")).expect("canonical endpoint"),
    )
    .expect("canonical endpoint JSON");
    assert_eq!(endpoint["schemaVersion"], 1);
    assert_eq!(endpoint["agentSessionId"], id);
    assert_eq!(endpoint["endpoint"], serde_json::Value::Null);
    assert_eq!(endpoint["futureMetadata"]["keep"], true);

    let target: serde_json::Value = serde_json::from_slice(
        &fs::read(session_root.join("context/target-binding.json")).expect("canonical binding"),
    )
    .expect("canonical target-binding JSON");
    assert_eq!(target["schemaVersion"], 1);
    assert_eq!(target["agentSessionId"], id);
    assert_eq!(target["scope"]["kind"], "global");
    assert_eq!(
        target["binding"]["targetTerminalSessionId"],
        "target-terminal-fixture"
    );
    assert_eq!(target["futureContextMetadata"]["retain"][2], 3);

    let snapshot: serde_json::Value = serde_json::from_slice(
        &fs::read(session_root.join("context/terminal-snapshot.json"))
            .expect("canonical terminal snapshot"),
    )
    .expect("canonical terminal snapshot JSON");
    assert_eq!(snapshot["schemaVersion"], 1);
    assert_eq!(snapshot["agentSessionId"], id);
    assert_eq!(snapshot["targetTerminalSessionId"], serde_json::Value::Null);
    assert_eq!(snapshot["output"], "synthetic historical output\n");
    assert_eq!(snapshot["futureSnapshotMetadata"], "retain");
    for value in [&endpoint, &target, &snapshot] {
        assert!(value.get("schema_version").is_none());
        assert!(value.get("agent_session_id").is_none());
    }
    assert!(
        plan_agent_migration(root)
            .expect("migration is idempotent")
            .is_empty()
    );
}

/// 同一 context 身份头的 camelCase 与 snake_case 双写时，迁移拒绝覆盖原文档。
///
/// @author kongweiguang
#[test]
fn conflicting_context_header_aliases_are_rejected_without_rewriting_the_document() {
    let temporary = tempfile::tempdir().expect("isolated migration root");
    let root = temporary.path();
    let id = "agent-migration-context-header-conflict-fixture";
    let session_root = write_session(root, id, "archived", "closed");
    write_provider(&session_root, AgentProvider::Codex);
    let mut endpoint = endpoint_document(id, serde_json::Value::Null);
    endpoint["schema_version"] = serde_json::json!(2);
    let original = write_context(&session_root, "mcp-endpoint.json", &endpoint);

    assert!(plan_agent_migration(root).is_err());
    assert_eq!(
        fs::read(session_root.join("context/mcp-endpoint.json"))
            .expect("rejected document remains"),
        original
    );
}

/// 通过真实 session/provider/context 文件验证 live 引用清理、目标定义保留和前滚计划幂等。
///
/// @author kongweiguang
#[test]
fn active_agent_migration_clears_only_runtime_references_and_preserves_history() {
    let temporary = tempfile::tempdir().expect("isolated migration root");
    let root = temporary.path();
    let id = "agent-migration-active-fixture";
    let session_root = write_session(root, id, "active", "ready");
    let provider_before = write_provider(&session_root, AgentProvider::Codex);
    let session_path = session_root.join("session.toml");
    let session_before = fs::read(&session_path).expect("session bytes");
    let endpoint_before = write_context(
        &session_root,
        "mcp-endpoint.json",
        &endpoint_document(
            id,
            serde_json::Value::String(format!("http://127.0.0.1:37657/mcp/agents/{id}")),
        ),
    );
    let target_before = write_context(
        &session_root,
        "target-binding.json",
        &target_binding_document(id, "ready", false, "running"),
    );
    let snapshot_before = write_context(
        &session_root,
        "terminal-snapshot.json",
        &terminal_snapshot_document(
            id,
            serde_json::Value::String("target-terminal-fixture".to_owned()),
        ),
    );
    let workspace_snapshot = b"workspace snapshot remains outside this migration";
    fs::write(
        session_root.join("context").join("workspace-snapshot.json"),
        workspace_snapshot,
    )
    .expect("synthetic workspace snapshot");

    let edits = plan_agent_migration(root).expect("valid legacy session should produce a plan");
    assert_eq!(edits.len(), 4);
    assert_eq!(
        fs::read(&session_path).expect("planner is read-only"),
        session_before
    );
    assert_eq!(
        fs::read(session_root.join("provider.toml")).expect("provider remains unchanged"),
        provider_before
    );
    assert_eq!(
        fs::read(session_root.join("context/mcp-endpoint.json"))
            .expect("endpoint remains unchanged"),
        endpoint_before
    );
    assert_eq!(
        fs::read(session_root.join("context/target-binding.json"))
            .expect("binding remains unchanged"),
        target_before
    );
    assert_eq!(
        fs::read(session_root.join("context/terminal-snapshot.json"))
            .expect("snapshot remains unchanged"),
        snapshot_before
    );

    let find_edit = |relative: &str| {
        edits
            .iter()
            .find(|edit| edit.relative_path == Path::new(relative))
            .unwrap_or_else(|| panic!("missing planned edit: {relative}"))
    };
    let session_edit = find_edit(&format!("agents/sessions/{id}/session.toml"));
    assert_eq!(
        session_edit.before.as_deref(),
        Some(session_before.as_slice())
    );
    let session_after: toml::Value =
        toml::from_str(std::str::from_utf8(&session_edit.after).expect("planned TOML is UTF-8"))
            .expect("planned session TOML");
    assert_eq!(session_after["status"].as_str(), Some("stale"));
    assert!(
        session_after["updated_at"]
            .as_str()
            .and_then(|timestamp| timestamp.parse::<u64>().ok())
            .is_some()
    );
    assert_eq!(
        session_after["target"]["live_status"].as_str(),
        Some("ready")
    );
    assert_eq!(
        session_after["target"]["target_terminal_session_id"].as_str(),
        Some("target-terminal-fixture")
    );
    assert_eq!(
        session_after["launch"]["cwd"].as_str(),
        Some(format!("C:/fixture/.kerminal/agents/sessions/{id}").as_str())
    );

    let endpoint_after: serde_json::Value = serde_json::from_slice(
        &find_edit(&format!("agents/sessions/{id}/context/mcp-endpoint.json")).after,
    )
    .expect("planned endpoint JSON");
    assert!(endpoint_after["endpoint"].is_null());
    assert_eq!(endpoint_after["futureMetadata"]["keep"], true);

    let target_after: serde_json::Value = serde_json::from_slice(
        &find_edit(&format!("agents/sessions/{id}/context/target-binding.json")).after,
    )
    .expect("planned target JSON");
    assert_eq!(target_after["binding"]["status"], "stale");
    assert_eq!(target_after["binding"]["stale"], true);
    assert_eq!(
        target_after["binding"]["targetTerminalSessionId"],
        "target-terminal-fixture"
    );
    assert_eq!(target_after["binding"]["generation"], 7);
    assert_eq!(target_after["binding"]["paneId"], "pane-fixture");
    assert_eq!(target_after["binding"]["tabId"], "tab-fixture");
    assert_eq!(target_after["binding"]["targetRef"], "local");
    assert_eq!(target_after["binding"]["cwd"], "C:/fixture/workspace");
    assert_eq!(target_after["binding"]["shell"], "pwsh");
    assert!(target_after["agentTerminal"]["sessionId"].is_null());
    assert_eq!(target_after["agentTerminal"]["status"], "stopped");
    assert_eq!(target_after["futureContextMetadata"]["retain"][2], 3);

    let snapshot_after: serde_json::Value = serde_json::from_slice(
        &find_edit(&format!(
            "agents/sessions/{id}/context/terminal-snapshot.json"
        ))
        .after,
    )
    .expect("planned snapshot JSON");
    assert!(snapshot_after["targetTerminalSessionId"].is_null());
    assert_eq!(snapshot_after["output"], "synthetic historical output\n");
    assert_eq!(
        snapshot_after["capturedBytes"],
        "synthetic historical output\n".len()
    );
    assert_eq!(snapshot_after["maxBytes"], 4096);
    assert_eq!(snapshot_after["truncated"], true);
    assert_eq!(snapshot_after["redacted"], true);
    assert_eq!(snapshot_after["futureSnapshotMetadata"], "retain");
    assert_eq!(
        fs::read(session_root.join("context/workspace-snapshot.json"))
            .expect("workspace snapshot was not scanned"),
        workspace_snapshot
    );

    for edit in &edits {
        let path = root.join(&edit.relative_path);
        fs::write(path, &edit.after).expect("apply only to isolated fixture for idempotence check");
    }
    assert!(
        plan_agent_migration(root)
            .expect("already normalized files should be valid current data")
            .is_empty()
    );
}

/// 已归档会话与 Closed 目标不改状态，只清理仍残留的 scoped runtime endpoint/owner 元数据。
///
/// @author kongweiguang
#[test]
fn archived_and_closed_history_keeps_terminal_states() {
    let temporary = tempfile::tempdir().expect("isolated migration root");
    let root = temporary.path();
    let id = "agent-migration-archived-fixture";
    let session_root = write_session(root, id, "archived", "closed");
    write_provider(&session_root, AgentProvider::Codex);
    write_context(
        &session_root,
        "mcp-endpoint.json",
        &endpoint_document(
            id,
            serde_json::Value::String("http://127.0.0.1:37657/mcp".to_owned()),
        ),
    );
    write_context(
        &session_root,
        "target-binding.json",
        &target_binding_document(id, "closed", true, "exited"),
    );
    write_context(
        &session_root,
        "terminal-snapshot.json",
        &terminal_snapshot_document(id, serde_json::Value::Null),
    );

    let edits = plan_agent_migration(root).expect("valid archived session should be inspected");
    assert_eq!(edits.len(), 2);
    assert!(!edits.iter().any(|edit| {
        edit.relative_path
            == Path::new("agents/sessions/agent-migration-archived-fixture/session.toml")
    }));
    let target = edits
        .iter()
        .find(|edit| edit.relative_path.ends_with("target-binding.json"))
        .expect("runtime terminal projection should be normalized");
    let target: serde_json::Value = serde_json::from_slice(&target.after).expect("target JSON");
    assert_eq!(target["binding"]["status"], "closed");
    assert_eq!(target["binding"]["stale"], true);
    assert_eq!(
        target["binding"]["targetTerminalSessionId"],
        "target-terminal-fixture"
    );
    assert_eq!(target["agentTerminal"]["status"], "stopped");
    assert!(target["agentTerminal"]["sessionId"].is_null());
}

/// 缺少当前 context 必填的可空 owner 字段时阻止整个计划，不伪造已清理状态。
///
/// @author kongweiguang
#[test]
fn malformed_target_context_preserves_live_target_evidence() {
    for missing_field in ["binding.targetTerminalSessionId", "agentTerminal.sessionId"] {
        let temporary = tempfile::tempdir().expect("isolated migration root");
        let root = temporary.path();
        let id = "agent-migration-target-shape-fixture";
        let session_root = write_session(root, id, "active", "ready");
        let original_session = fs::read(session_root.join("session.toml")).expect("session bytes");
        let mut target = target_binding_document(id, "ready", false, "running");
        let (owner, field) = missing_field.split_once('.').expect("fixed nested field");
        target[owner]
            .as_object_mut()
            .expect("synthetic object")
            .remove(field);
        let target_before = write_context(&session_root, "target-binding.json", &target);

        assert!(
            plan_agent_migration(root).is_err(),
            "must reject missing {missing_field}"
        );
        assert_eq!(
            fs::read(session_root.join("session.toml")).expect("session preserved"),
            original_session
        );
        assert_eq!(
            fs::read(session_root.join("context/target-binding.json")).expect("target preserved"),
            target_before
        );
    }
}

/// 未知 endpoint path、authority、凭据或 query 均整体阻断迁移，并保留输入字节供恢复。
///
/// @author kongweiguang
#[test]
fn malformed_or_foreign_endpoint_is_never_cleared_as_success() {
    let id = "agent-migration-endpoint-fixture";
    let endpoints = [
        "http://127.0.0.1:37657/mcp/agents/another-session",
        "http://127.0.0.1:37657/mcp/extra",
        "http://127.0.0.1:37657/mcp?token=fixture",
        "http://127.0.0.1:37657/mcp#fragment",
        "http://fixture@127.0.0.1:37657/mcp",
        "https://127.0.0.1:37657/mcp",
        "http://localhost:37657/mcp",
        "http://127.0.0.1/mcp",
        "http://127.0.0.1:0/mcp",
    ];
    for endpoint in endpoints {
        let temporary = tempfile::tempdir().expect("isolated migration root");
        let root = temporary.path();
        let session_root = write_session(root, id, "active", "ready");
        let original_session = fs::read(session_root.join("session.toml")).expect("session bytes");
        let original_endpoint = write_context(
            &session_root,
            "mcp-endpoint.json",
            &endpoint_document(id, serde_json::Value::String(endpoint.to_owned())),
        );

        assert!(
            plan_agent_migration(root).is_err(),
            "endpoint must reject: {endpoint}"
        );
        assert_eq!(
            fs::read(session_root.join("session.toml")).expect("session remains untouched"),
            original_session
        );
        assert_eq!(
            fs::read(session_root.join("context/mcp-endpoint.json"))
                .expect("invalid endpoint remains available for repair"),
            original_endpoint
        );
    }
}

/// Provider mismatch、session id 错配和未来 context schema 都阻止整批计划，不产生部分伪成功。
///
/// @author kongweiguang
#[test]
fn identity_provider_and_schema_errors_preserve_all_files() {
    let temporary = tempfile::tempdir().expect("isolated migration root");
    let root = temporary.path();
    let id = "agent-migration-invalid-fixture";
    let session_root = write_session(root, id, "active", "ready");
    let session_before = fs::read(session_root.join("session.toml")).expect("session bytes");
    let provider_before = write_provider(&session_root, AgentProvider::Claude);
    let endpoint_before = write_context(
        &session_root,
        "mcp-endpoint.json",
        &endpoint_document(
            id,
            serde_json::Value::String("http://127.0.0.1:37657/mcp".to_owned()),
        ),
    );
    assert!(plan_agent_migration(root).is_err());
    assert_eq!(
        fs::read(session_root.join("session.toml")).expect("session preserved"),
        session_before
    );
    assert_eq!(
        fs::read(session_root.join("provider.toml")).expect("provider preserved"),
        provider_before
    );
    assert_eq!(
        fs::read(session_root.join("context/mcp-endpoint.json")).expect("endpoint preserved"),
        endpoint_before
    );

    let mut invalid_context: serde_json::Value =
        serde_json::from_slice(&endpoint_before).expect("endpoint JSON");
    invalid_context["agentSessionId"] = serde_json::Value::String("other-agent".to_owned());
    let invalid_bytes = write_context(&session_root, "mcp-endpoint.json", &invalid_context);
    assert!(plan_agent_migration(root).is_err());
    assert_eq!(
        fs::read(session_root.join("context/mcp-endpoint.json")).expect("mismatched id preserved"),
        invalid_bytes
    );

    invalid_context["agentSessionId"] = serde_json::Value::String(id.to_owned());
    invalid_context["schemaVersion"] = serde_json::json!(2);
    let future_bytes = write_context(&session_root, "mcp-endpoint.json", &invalid_context);
    assert!(plan_agent_migration(root).is_err());
    assert_eq!(
        fs::read(session_root.join("context/mcp-endpoint.json")).expect("future schema preserved"),
        future_bytes
    );
}

/// 未恢复的 Agent workspace journal 表示文件可能处于混合提交态，迁移必须保留现场并停止。
///
/// @author kongweiguang
#[test]
fn unresolved_agent_workspace_journal_blocks_migration() {
    let temporary = tempfile::tempdir().expect("isolated migration root");
    let root = temporary.path();
    let id = "agent-migration-journal-fixture";
    let session_root = write_session(root, id, "active", "ready");
    let original = fs::read(session_root.join("session.toml")).expect("session bytes");
    fs::write(
        session_root.join(".agent-workspace-transaction.json"),
        b"{}",
    )
    .expect("journal");

    assert!(plan_agent_migration(root).is_err());
    assert_eq!(
        fs::read(session_root.join("session.toml")).expect("session remains"),
        original
    );
}

/// 不存在的配置根保持不存在，避免预检本身创建用户数据目录。
///
/// @author kongweiguang
#[test]
fn missing_agent_root_is_a_read_only_empty_plan() {
    let temporary = tempfile::tempdir().expect("isolated parent");
    let root = temporary.path().join("missing-kerminal-root");
    assert!(
        plan_agent_migration(&root)
            .expect("missing root has no Agent data")
            .is_empty()
    );
    assert!(!root.exists());
}

/// 旧 target alias 先由 planner 规范化，再必须通过当前严格 FileAgentWorkspaceRepository 读取。
///
/// @author kongweiguang
#[tokio::test]
async fn target_aliases_migrate_to_canonical_keys_and_read_as_current_session() {
    let temporary = tempfile::tempdir().expect("isolated migration root");
    let root = temporary.path();
    let id = "agent-migration-target-alias-fixture";
    let session_root = write_legacy_target_alias_session(root, id);
    let session_path = session_root.join("session.toml");
    let legacy_bytes = fs::read(&session_path).expect("legacy session bytes");
    let legacy_text = std::str::from_utf8(&legacy_bytes).expect("legacy session UTF-8");
    let legacy: toml::Value = toml::from_str(legacy_text).expect("legacy session TOML");
    let legacy_target = legacy["target"].as_table().expect("legacy target table");
    for alias in [
        "bindingId",
        "generation",
        "paneId",
        "tabId",
        "terminalSessionId",
        "targetRef",
        "targetKind",
        "liveStatus",
        "lastSeenAt",
    ] {
        assert!(legacy_target.contains_key(alias), "fixture alias {alias}");
    }
    let edits = plan_agent_migration(root).expect("legacy aliases should produce a safe plan");
    assert_eq!(edits.len(), 2);
    assert_eq!(
        fs::read(&session_path).expect("planner is read-only"),
        legacy_bytes
    );
    let edit = edits
        .iter()
        .find(|edit| edit.relative_path == Path::new(&format!("agents/sessions/{id}/session.toml")))
        .expect("legacy session field edit");
    assert_eq!(
        edit.relative_path,
        Path::new(&format!("agents/sessions/{id}/session.toml"))
    );
    assert_eq!(edit.before.as_deref(), Some(legacy_bytes.as_slice()));
    let after_text = std::str::from_utf8(&edit.after).expect("canonical session UTF-8");
    let canonical: toml::Value = toml::from_str(after_text).expect("canonical session TOML");
    let target = canonical["target"]
        .as_table()
        .expect("canonical target table");
    for canonical_key in [
        "binding_id",
        "binding_generation",
        "pane_id",
        "tab_id",
        "target_terminal_session_id",
        "target_ref",
        "target_kind",
        "live_status",
        "last_seen_at",
    ] {
        assert!(
            target.contains_key(canonical_key),
            "canonical key {canonical_key}"
        );
    }
    for alias in [
        "bindingId",
        "generation",
        "paneId",
        "tabId",
        "terminal_session_id",
        "targetTerminalSessionId",
        "terminalSessionId",
        "targetRef",
        "targetKind",
        "liveStatus",
        "lastSeenAt",
    ] {
        assert!(!target.contains_key(alias), "legacy key {alias}");
    }
    assert!(after_text.contains("# retain target note"));

    let provider_edit = edits
        .iter()
        .find(|edit| {
            edit.relative_path == Path::new(&format!("agents/sessions/{id}/provider.toml"))
        })
        .expect("missing legacy provider is created by migration");
    assert!(provider_edit.before.is_none());
    for edit in &edits {
        fs::write(root.join(&edit.relative_path), &edit.after)
            .expect("apply isolated planner edit");
    }
    let repository = FileAgentWorkspaceRepository::new(root.to_path_buf());
    let snapshot = repository
        .read_session(
            AgentSessionId::parse(id).expect("valid fixture session id"),
            CancellationToken::new(),
        )
        .await
        .expect("ordinary strict reader accepts migrated target");
    assert_eq!(snapshot.session.status, AgentSessionStatus::Stale);
    let target = snapshot
        .session
        .target
        .expect("target definition remains available");
    assert_eq!(target.binding_id.as_deref(), Some("binding-fixture"));
    assert_eq!(target.generation, 7);
    assert_eq!(
        target.pane_id.as_ref().map(|value| value.as_str()),
        Some("pane-fixture")
    );
    assert_eq!(
        target.tab_id.as_ref().map(|value| value.as_str()),
        Some("tab-fixture")
    );
    assert_eq!(
        target
            .terminal_session_id
            .as_ref()
            .map(|value| value.as_str()),
        Some("target-terminal-fixture")
    );
    assert_eq!(target.target_ref.as_deref(), Some("local"));
    assert_eq!(target.target_kind.as_deref(), Some("local"));
    assert_eq!(target.live_status, AgentTargetLiveStatus::Ready);
    assert_eq!(target.last_seen_at.as_deref(), Some("102"));

    let provider_snapshot = repository
        .read_provider_session(
            AgentSessionId::parse(id).expect("valid fixture session id"),
            CancellationToken::new(),
        )
        .await
        .expect("ordinary provider reader accepts migrated metadata");
    assert_eq!(
        provider_snapshot.status,
        AgentProviderSessionStatus::Available(initial_provider_session(AgentProvider::Codex))
    );
}

/// 缺失 Custom provider 只生成显式不可恢复的当前文件，并由实际 Store 读取回相同值。
///
/// @author kongweiguang
#[tokio::test]
async fn missing_custom_provider_migrates_without_inventing_resume_metadata() {
    let temporary = tempfile::tempdir().expect("isolated migration root");
    let root = temporary.path();
    let id = "agent-migration-custom-provider-fixture";
    let _session_root = write_custom_session(root, id, "archived");

    let edits = plan_agent_migration(root).expect("verified legacy Custom session can migrate");
    assert_eq!(edits.len(), 1);
    let provider_edit = &edits[0];
    assert_eq!(
        provider_edit.relative_path,
        Path::new(&format!("agents/sessions/{id}/provider.toml"))
    );
    assert!(provider_edit.before.is_none());
    let planned: AgentProviderSession = toml::from_str(
        std::str::from_utf8(&provider_edit.after).expect("canonical provider TOML is UTF-8"),
    )
    .expect("provider migration emits current schema");
    assert_eq!(planned, initial_provider_session(AgentProvider::Custom));
    assert!(!planned.resume_supported);
    assert_eq!(planned.resume_command, None);
    assert_eq!(planned.provider_session_id, None);

    fs::write(
        root.join(&provider_edit.relative_path),
        &provider_edit.after,
    )
    .expect("apply isolated provider migration");
    let repository = FileAgentWorkspaceRepository::new(root.to_path_buf());
    let snapshot = repository
        .read_provider_session(
            AgentSessionId::parse(id).expect("valid Custom session id"),
            CancellationToken::new(),
        )
        .await
        .expect("ordinary provider reader accepts migrated Custom metadata");
    assert_eq!(
        snapshot.status,
        AgentProviderSessionStatus::Available(planned)
    );
}

/// 仅将旧 builtin 的缺失命令规划为 CAS 更新，并经当前 provider reader 回读验证。
///
/// @author kongweiguang
#[tokio::test]
async fn missing_builtin_resume_command_migrates_and_reads_current_policy_back() {
    let temporary = tempfile::tempdir().expect("isolated migration root");
    let root = temporary.path();
    let id = "agent-migration-builtin-resume-fixture";
    let session_root = write_session(root, id, "archived", "closed");
    let before = write_provider_without_resume_command(
        &session_root,
        AgentProvider::Codex,
        "thread-42",
        true,
    );

    let edits = plan_agent_migration(root).expect("verified builtin metadata can be normalized");
    assert_eq!(edits.len(), 1);
    let provider_edit = &edits[0];
    assert_eq!(
        provider_edit.relative_path,
        Path::new(&format!("agents/sessions/{id}/provider.toml"))
    );
    assert_eq!(provider_edit.before.as_deref(), Some(before.as_slice()));
    let migrated: AgentProviderSession = toml::from_str(
        std::str::from_utf8(&provider_edit.after).expect("canonical provider TOML is UTF-8"),
    )
    .expect("migration emits a current provider document");
    assert_eq!(migrated.provider, AgentProvider::Codex);
    assert_eq!(
        migrated.resume_command,
        initial_provider_session(AgentProvider::Codex).resume_command
    );
    assert_eq!(migrated.provider_session_id.as_deref(), Some("thread-42"));
    assert_eq!(migrated.last_resume_at.as_deref(), Some("103"));
    assert!(migrated.resume_supported);

    fs::write(
        root.join(&provider_edit.relative_path),
        &provider_edit.after,
    )
    .expect("apply synthetic provider migration");
    let repository = FileAgentWorkspaceRepository::new(root.to_path_buf());
    let snapshot = repository
        .read_provider_session(
            AgentSessionId::parse(id).expect("valid fixture session id"),
            CancellationToken::new(),
        )
        .await
        .expect("ordinary provider reader accepts normalized metadata");
    assert_eq!(
        snapshot.status,
        AgentProviderSessionStatus::Available(migrated)
    );
}

/// Pi 的已记录 ID 当前没有冻结 argv 映射，迁移必须拒绝且保留原文件字节。
///
/// @author kongweiguang
#[tokio::test]
async fn pi_resume_migration_rejects_recorded_id_without_rewriting_provider_file() {
    let temporary = tempfile::tempdir().expect("isolated Pi migration root");
    let root = temporary.path();
    let id = "agent-migration-pi-provider-id";
    let session_root = write_session(root, id, "archived", "closed");
    let session_path = session_root.join("session.toml");
    let codex_session = fs::read_to_string(&session_path).expect("synthetic session text");
    let pi_session = codex_session
        .replace("agent_id = \"codex\"", "agent_id = \"pi\"")
        .replace(
            "launcher_key = \"builtin:codex\"",
            "launcher_key = \"builtin:pi\"",
        )
        .replace("command_label = \"Codex\"", "command_label = \"Pi\"")
        .replace("shell = \"codex\"", "shell = \"pi\"");
    assert_ne!(
        pi_session, codex_session,
        "fixture must be a valid Pi session"
    );
    fs::write(&session_path, &pi_session).expect("valid Pi session fixture");
    let before = write_provider_without_resume_command(
        &session_root,
        AgentProvider::Pi,
        "pi-session-42",
        true,
    );

    let error = plan_agent_migration(root)
        .expect_err("Pi identity must not be normalized into an unusable default command");
    assert_eq!(error.public_code(), PublicErrorCode::Unsupported);
    assert_eq!(
        fs::read(&session_path).expect("Pi session bytes remain unchanged"),
        pi_session.as_bytes()
    );
    assert_eq!(
        fs::read(session_root.join("provider.toml")).expect("Pi bytes remain unchanged"),
        before
    );
}

/// Custom 缺少命令时保持原始 ID 和字节；未来 provider 名称与损坏 ID 均阻断迁移。
///
/// @author kongweiguang
#[tokio::test]
async fn missing_custom_resume_command_is_preserved_and_untrusted_builtin_id_is_rejected() {
    let temporary = tempfile::tempdir().expect("isolated Custom migration root");
    let root = temporary.path();
    let id = "agent-migration-custom-resume-fixture";
    let session_root = write_custom_session(root, id, "archived");
    let before = write_provider_without_resume_command(
        &session_root,
        AgentProvider::Custom,
        "existing-custom-id",
        true,
    );

    assert!(
        plan_agent_migration(root)
            .expect("Custom metadata must not be guessed")
            .is_empty()
    );
    assert_eq!(
        fs::read(session_root.join("provider.toml")).expect("Custom bytes remain"),
        before
    );
    let repository = FileAgentWorkspaceRepository::new(root.to_path_buf());
    let snapshot = repository
        .read_provider_session(
            AgentSessionId::parse(id).expect("valid Custom session id"),
            CancellationToken::new(),
        )
        .await
        .expect("ordinary reader accepts preserved Custom metadata");
    let AgentProviderSessionStatus::Available(custom) = snapshot.status else {
        panic!("existing Custom metadata should stay readable");
    };
    assert_eq!(
        custom.provider_session_id.as_deref(),
        Some("existing-custom-id")
    );
    assert_eq!(custom.resume_command, None);

    let invalid_root = tempfile::tempdir().expect("isolated invalid-ID migration root");
    let invalid_id = "agent-migration-invalid-provider-id";
    let invalid_session = write_session(invalid_root.path(), invalid_id, "archived", "closed");
    let invalid_before = write_provider_without_resume_command(
        &invalid_session,
        AgentProvider::Codex,
        "thread\ninvalid",
        true,
    );
    assert!(plan_agent_migration(invalid_root.path()).is_err());
    assert_eq!(
        fs::read(invalid_session.join("provider.toml")).expect("invalid source remains"),
        invalid_before
    );

    let unknown_root = tempfile::tempdir().expect("isolated unknown-provider migration root");
    let unknown_id = "agent-migration-unknown-provider";
    let unknown_session = write_session(unknown_root.path(), unknown_id, "archived", "closed");
    let unknown_provider = b"schema_version = 1\nprovider = \"future\"\nresume_supported = true\n";
    fs::write(unknown_session.join("provider.toml"), unknown_provider)
        .expect("write unknown provider fixture");
    assert!(plan_agent_migration(unknown_root.path()).is_err());
    assert_eq!(
        fs::read(unknown_session.join("provider.toml")).expect("unknown source remains"),
        unknown_provider
    );
}

/// 同一领域键的其它历史别名也归一化后通过严格 reader，不增加长期兼容路径。
///
/// @author kongweiguang
#[tokio::test]
async fn alternate_target_alias_spellings_read_after_one_time_normalization() {
    for (index, (generation_alias, terminal_session_alias)) in [
        ("bindingGeneration", "terminal_session_id"),
        ("generation", "targetTerminalSessionId"),
    ]
    .into_iter()
    .enumerate()
    {
        let temporary = tempfile::tempdir().expect("isolated migration root");
        let root = temporary.path();
        let id = format!("agent-migration-target-alias-variant-{index}");
        let session_root = write_legacy_target_alias_session_with_spellings(
            root,
            &id,
            generation_alias,
            terminal_session_alias,
        );
        write_provider(&session_root, AgentProvider::Codex);
        let edits = plan_agent_migration(root).expect("known aliases should be normalized");
        assert_eq!(edits.len(), 1);
        let edit = &edits[0];
        let canonical: toml::Value =
            toml::from_str(std::str::from_utf8(&edit.after).expect("canonical session UTF-8"))
                .expect("canonical session TOML");
        assert_eq!(
            canonical["target"]["binding_generation"].as_integer(),
            Some(7)
        );
        assert_eq!(
            canonical["target"]["target_terminal_session_id"].as_str(),
            Some("target-terminal-fixture")
        );

        fs::write(session_root.join("session.toml"), &edit.after)
            .expect("apply isolated canonicalization");
        let repository = FileAgentWorkspaceRepository::new(root.to_path_buf());
        let snapshot = repository
            .read_session(
                AgentSessionId::parse(&id).expect("valid fixture session id"),
                CancellationToken::new(),
            )
            .await
            .expect("strict store reads normalized alias variant");
        let target = snapshot
            .session
            .target
            .expect("target remains bound by definition");
        assert_eq!(target.generation, 7);
        assert_eq!(
            target
                .terminal_session_id
                .as_ref()
                .map(|value| value.as_str()),
            Some("target-terminal-fixture")
        );
    }
}

/// 同一目标字段同时以 alias 和 canonical 出现时拒绝整份迁移并保留原始字节。
///
/// @author kongweiguang
#[test]
fn target_alias_collision_preserves_original_session() {
    let temporary = tempfile::tempdir().expect("isolated migration root");
    let root = temporary.path();
    let id = "agent-migration-target-collision-fixture";
    let session_root = write_legacy_target_alias_session(root, id);
    let session_path = session_root.join("session.toml");
    let original = fs::read_to_string(&session_path).expect("legacy session text");
    let conflicting = original.replace(
        "bindingId = \"binding-fixture\"",
        "bindingId = \"binding-fixture\"\nbinding_id = \"conflicting-binding\"",
    );
    fs::write(&session_path, &conflicting).expect("conflicting legacy session");

    assert!(plan_agent_migration(root).is_err());
    assert_eq!(
        fs::read(&session_path).expect("conflict evidence remains available"),
        conflicting.as_bytes()
    );
}

/// 已是 canonical snake_case 的历史文件不因迁移而重新编码或改变字节。
///
/// @author kongweiguang
#[test]
fn canonical_target_session_bytes_are_unchanged() {
    let temporary = tempfile::tempdir().expect("isolated migration root");
    let root = temporary.path();
    let id = "agent-migration-target-canonical-fixture";
    let session_root = write_session(root, id, "archived", "ready");
    write_provider(&session_root, AgentProvider::Codex);
    let path = session_root.join("session.toml");
    let original = fs::read(&path).expect("canonical session bytes");

    assert!(
        plan_agent_migration(root)
            .expect("canonical current session needs no migration")
            .is_empty()
    );
    assert_eq!(
        fs::read(path).expect("canonical bytes remain stable"),
        original
    );
}
