//! Frozen v0.3.45 readback checks, injected only into the archived acceptance copy.
//!
//! @author kongweiguang

use std::{
    fs,
    path::{Path, PathBuf},
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use kerminal_lib::{
    models::agent_session::{
        AgentId, AgentMcpTransport, AgentProvider, AgentSessionId, AgentSessionScope,
        AgentSessionStatus, AgentTargetBindingStatus, AgentTerminalSnapshotContext,
        AgentWorkspaceSnapshotContext,
    },
    models::remote_host::parse_vault_secret_ref,
    paths::KerminalPaths,
    services::encrypted_vault_service::EncryptedVaultService,
    services::{agent_session_service::AgentSessionService, settings_service::SettingsService},
    storage::{command_sqlite::CommandSqliteStore, config_file_store::ConfigFileStore},
};
use russh::keys::{known_hosts, HashAlg};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// 通过冻结服务和协议解析器逐项读取，避免把新旧结构“能反序列化”误当作行为兼容。
#[test]
fn frozen_legacy_services_read_generated_configuration_and_data() {
    let root = read_path_env("KERMINAL_LEGACY_READBACK_ROOT");
    let expected_path = read_path_env("KERMINAL_LEGACY_READBACK_EXPECTED");
    let expected: Value =
        serde_json::from_slice(&fs::read(expected_path).expect("读取 fixture expected.json"))
            .expect("解析 fixture expected.json");
    assert_eq!(expected["contractVersion"].as_u64(), Some(1));

    let profile_id = manifest_string(&expected, "profileId");
    let host_id = manifest_string(&expected, "hostId");
    let group_id = manifest_string(&expected, "groupId");
    let snippet_id = manifest_string(&expected, "snippetId");
    let workflow_id = manifest_string(&expected, "workflowId");
    for (name, value) in [
        ("profileId", profile_id),
        ("hostId", host_id),
        ("groupId", group_id),
        ("snippetId", snippet_id),
        ("workflowId", workflow_id),
    ] {
        assert!(safe_file_id(value), "{name} 必须是安全合成 ID");
    }

    let config = ConfigFileStore::new(root.clone());
    let settings = SettingsService::new(config.clone())
        .load_settings()
        .expect("通过旧版 SettingsService 读取 settings.toml");
    assert_eq!(
        settings.agent_launcher.selected_agent_key,
        manifest_string(&expected, "agent.selectedAgentKey"),
        "旧版设置服务应读取所选 Custom launcher"
    );
    let custom_agent = settings
        .agent_launcher
        .custom_agents
        .iter()
        .find(|agent| agent.id == manifest_string(&expected, "agent.definitionId"))
        .expect("旧版设置服务应读取生成的 Custom Agent 定义");
    assert_eq!(
        custom_agent.name,
        manifest_string(&expected, "agent.definitionName")
    );
    assert_eq!(
        custom_agent.command,
        manifest_string(&expected, "agent.definitionCommand")
    );
    let settings = serde_json::to_value(settings).expect("投影旧版 settings");
    assert_eq!(
        settings["themeMode"], expected["settings"]["themeMode"],
        "旧版应读取设置主题，而不是仅成功解析默认值"
    );

    let profiles = config.list_profiles().expect("旧版读取 Profiles");
    let profile = profiles
        .iter()
        .find(|profile| profile.id == profile_id)
        .expect("旧版读取生成的 Profile");
    assert_eq!(profile.name, manifest_string(&expected, "profile.name"));
    assert_eq!(profile.shell, manifest_string(&expected, "profile.shell"));
    assert_eq!(profile.args, vec!["--fixture".to_owned()]);
    assert_eq!(
        profile
            .env
            .get("KERMINAL_READBACK_MODE")
            .map(String::as_str),
        Some(manifest_string(&expected, "profile.envMode"))
    );
    let hosts = config.list_remote_hosts().expect("旧版读取 hosts");
    let host = hosts
        .iter()
        .find(|host| host.id == host_id)
        .expect("旧版读取生成的 Host");
    assert_eq!(host.name, manifest_string(&expected, "host.name"));
    assert_eq!(host.host, manifest_string(&expected, "host.host"));
    assert_eq!(
        u64::from(host.port),
        expected["host"]["port"]
            .as_u64()
            .expect("manifest host.port 必须是整数")
    );
    assert_eq!(host.username, manifest_string(&expected, "host.username"));
    assert_eq!(
        serde_json::to_value(host.protocol).expect("序列化旧 host protocol"),
        expected["host"]["protocol"]
    );
    assert_eq!(
        serde_json::to_value(host.auth_type).expect("序列化旧 host authentication"),
        expected["host"]["authType"]
    );
    assert_eq!(
        host.group_id.as_deref(),
        Some(manifest_string(&expected, "groupId"))
    );
    assert_eq!(
        host.tags,
        expected["host"]["tags"]
            .as_array()
            .expect("manifest host.tags 必须是数组")
            .iter()
            .map(|tag| tag.as_str().expect("tag 必须是字符串").to_owned())
            .collect::<Vec<_>>()
    );
    let groups = config
        .list_remote_host_groups()
        .expect("旧版读取 host groups");
    let group = groups
        .iter()
        .find(|group| group.id == group_id)
        .expect("旧版读取生成的 Host group");
    assert_eq!(group.name, manifest_string(&expected, "groupName"));
    let snippets = config.list_snippets().expect("旧版读取 snippets");
    assert!(snippets.iter().any(|snippet| snippet.id == snippet_id));
    let workflows = config.list_workflows().expect("旧版读取 workflows");
    assert!(workflows.iter().any(|workflow| workflow.id == workflow_id));

    let host_path = root.join("hosts").join(format!("{host_id}.toml"));
    let host_source = fs::read_to_string(host_path).expect("读取 host schema 原文");
    let host_document: toml::Value = toml::from_str(&host_source).expect("解析 host TOML");
    assert_eq!(
        host_document["schema_version"].as_integer(),
        expected["hostSchemaVersion"].as_i64(),
        "旧版 host 文档必须保留冻结的 schema 版本"
    );
    assert!(!contains_inline_credential(&host_document));

    let secret_ref = host
        .secret_ref
        .as_deref()
        .expect("旧版 Host 应保存指向 encrypted vault 的 secret_ref");
    assert_eq!(secret_ref, manifest_string(&expected, "host.secretRef"));
    let parsed_reference =
        parse_vault_secret_ref(secret_ref).expect("旧版解析器应接受 Host 保存的 v1 vault 引用");
    assert_eq!(parsed_reference.kind, "ssh-host");
    assert_eq!(parsed_reference.host_id, host_id);
    assert_eq!(parsed_reference.scope, "target");
    assert_eq!(parsed_reference.material, "password");
    let linked_entry_id = parsed_reference.entry_id();
    assert_eq!(
        linked_entry_id,
        manifest_string(&expected, "vault.entryId"),
        "Host 引用必须解析到实际写入的 vault entry"
    );
    assert_vault_entry(&root, &expected, &linked_entry_id);
    assert_legacy_known_hosts(&root, &expected);
    assert_command_history(&root, &expected);
    assert_legacy_agent_readback(&root, &expected);
}

/// 通过冻结版 Agent 服务和 context 模型回读新版生成的归档 Custom 会话。
/// 复制 fixture 不会重写已持久化的绝对路径，因此从 launch.cwd 还原原生成根校验会话路径；context 文件路径则按 readback 根验证。
fn assert_legacy_agent_readback(root: &std::path::Path, expected: &Value) {
    let session_id = AgentSessionId::new(manifest_string(expected, "agent.sessionId"))
        .expect("manifest Agent session ID 必须有效");
    assert!(
        safe_file_id(session_id.as_str()),
        "Agent session ID 必须安全"
    );
    let record = AgentSessionService::new(root.to_path_buf())
        .get_session(&session_id)
        .expect("冻结版 AgentSessionService 应能读取新版 Agent 文件");

    assert_eq!(record.session.agent_id, AgentId::Custom);
    assert_eq!(
        serde_json::to_value(record.session.agent_id).expect("序列化旧 Agent 类型"),
        expected["agent"]["provider"]
    );
    assert_eq!(
        record.session.schema_version,
        expected["agent"]["schemaVersion"]
            .as_u64()
            .expect("manifest Agent schemaVersion 必须是整数") as u32
    );
    assert_eq!(record.session.status, AgentSessionStatus::Archived);
    assert_eq!(
        serde_json::to_value(record.session.status).expect("序列化旧 Agent 状态"),
        expected["agent"]["status"]
    );
    assert_eq!(
        record.session.launcher_key.as_deref(),
        Some(manifest_string(expected, "agent.launcherKey"))
    );
    assert_eq!(
        record.session.launch.command_label,
        manifest_string(expected, "agent.launch.commandLabel")
    );
    assert_eq!(
        record.session.launch.shell,
        manifest_string(expected, "agent.launch.shell")
    );
    let expected_args = expected["agent"]["launch"]["args"]
        .as_array()
        .expect("manifest launch args 必须是数组")
        .iter()
        .map(|argument| {
            argument
                .as_str()
                .expect("manifest launch arg 必须是字符串")
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(record.session.launch.args, expected_args);
    let session_root_relative =
        PathBuf::from(manifest_string(expected, "agent.sessionRootRelative"));
    assert!(
        !session_root_relative.is_absolute(),
        "fixture sessionRootRelative 必须是相对路径"
    );
    let expected_session_root =
        canonical_existing_path(PathBuf::from(manifest_string(expected, "agent.launch.cwd")));
    let fixture_config_root = expected_session_root
        .ancestors()
        .nth(session_root_relative.components().count())
        .expect("fixture launch.cwd 必须包含完整的 sessionRootRelative 路径")
        .to_path_buf();
    assert_eq!(
        canonical_existing_path(fixture_config_root.join(&session_root_relative)),
        expected_session_root,
        "fixture launch.cwd 必须由原生成根和 sessionRootRelative 组成"
    );
    assert_eq!(
        canonical_existing_path(PathBuf::from(&record.session.session_root)),
        expected_session_root
    );
    assert_eq!(
        canonical_existing_path(PathBuf::from(&record.session.launch.cwd)),
        expected_session_root,
        "旧版 launch.cwd 应仍指向原生成根中的 session 目录"
    );
    assert!(
        record.diagnostics.is_empty(),
        "合成 Agent 文件不应产生恢复诊断"
    );

    let provider = record
        .provider
        .as_ref()
        .expect("冻结版 Agent 服务应回读 provider.toml");
    assert_eq!(provider.provider, AgentProvider::Custom);
    assert_eq!(
        serde_json::to_value(provider.provider).expect("序列化旧 provider 类型"),
        expected["agent"]["provider"]
    );
    assert_eq!(provider.provider_session_id, None);
    assert_eq!(provider.resume_command, None);
    assert!(!provider.resume_supported);
    assert_eq!(provider.last_resume_at, None);

    let target_binding = record
        .target_binding
        .as_ref()
        .expect("冻结版 Agent 服务应回读 target-binding context");
    assert_eq!(target_binding.agent_session_id, session_id);
    assert_eq!(
        target_binding.binding.status,
        AgentTargetBindingStatus::Unbound
    );
    assert!(!target_binding.binding.stale);
    assert!(target_binding.binding.target_terminal_session_id.is_none());
    assert!(matches!(
        target_binding.effective_scope(),
        AgentSessionScope::Global
    ));

    for (relative_field, actual_path) in [
        (
            "agent.contextPaths.mcpEndpoint",
            record.paths.context.mcp_endpoint_json.as_str(),
        ),
        (
            "agent.contextPaths.targetBinding",
            record.paths.context.target_binding_json.as_str(),
        ),
        (
            "agent.contextPaths.terminalSnapshot",
            record.paths.context.terminal_snapshot_json.as_str(),
        ),
        (
            "agent.contextPaths.workspaceSnapshot",
            record.paths.context.workspace_snapshot_json.as_str(),
        ),
    ] {
        assert_eq!(
            canonical_existing_path(PathBuf::from(actual_path)),
            canonical_existing_path(root.join(manifest_string(expected, relative_field))),
            "旧版 context 路径必须指向原生成根中的真实文件"
        );
    }

    let mcp_endpoint = record
        .mcp_endpoint
        .as_ref()
        .expect("冻结版 Agent 服务应回读 mcp-endpoint context");
    assert_eq!(mcp_endpoint.agent_session_id, session_id);
    assert_eq!(mcp_endpoint.endpoint, None);
    assert_eq!(mcp_endpoint.transport, AgentMcpTransport::StreamableHttp);
    assert!(mcp_endpoint.tools_only);

    let terminal_snapshot: AgentTerminalSnapshotContext = serde_json::from_slice(
        &fs::read(&record.paths.context.terminal_snapshot_json)
            .expect("读取隔离 terminal-snapshot.json"),
    )
    .expect("冻结版 terminal snapshot 模型应读取新版 JSON");
    terminal_snapshot
        .validate()
        .expect("冻结版 terminal snapshot schema 校验应通过");
    assert_eq!(terminal_snapshot.agent_session_id, session_id);
    assert_eq!(terminal_snapshot.output, "");
    assert_eq!(terminal_snapshot.captured_bytes, 0);

    let workspace_snapshot: AgentWorkspaceSnapshotContext = serde_json::from_slice(
        &fs::read(&record.paths.context.workspace_snapshot_json)
            .expect("读取隔离 workspace-snapshot.json"),
    )
    .expect("冻结版 workspace snapshot 模型应读取新版 JSON");
    workspace_snapshot
        .validate()
        .expect("冻结版 workspace snapshot schema 校验应通过");
    assert_eq!(workspace_snapshot.agent_session_id, session_id);
    assert_eq!(
        canonical_existing_path(PathBuf::from(&workspace_snapshot.session_root)),
        expected_session_root,
        "workspace snapshot 的 sessionRoot 应指向原生成 session 目录"
    );
    assert_eq!(
        canonical_existing_path(PathBuf::from(&workspace_snapshot.workspace_session_json)),
        canonical_existing_path(fixture_config_root.join(manifest_string(
            expected,
            "agent.workspaceSessionJsonRelative",
        ))),
        "workspace snapshot 文件路径必须仍在原生成根"
    );
}

/// 以文件系统 canonical path 比较路径身份，并让缺失或不可访问目标直接失败。
fn canonical_existing_path(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    fs::canonicalize(path)
        .unwrap_or_else(|error| panic!("验收路径必须存在且可解析 ({}): {error}", path.display()))
}

/// 读取 harness 显式传入的路径，避免旧 API 因缺变量回退到真实用户目录。
fn read_path_env(name: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| panic!("缺少隔离环境变量 {name}"))
}

/// 从 manifest 读取固定字符串字段，让缺失或类型不符直接使验收失败。
fn manifest_string<'a>(manifest: &'a Value, field: &str) -> &'a str {
    field
        .split('.')
        .try_fold(manifest, |value, segment| value.get(segment))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("expected.json 缺少字符串字段 {field}"))
}

/// 限制由测试 manifest 拼出的相对 TOML 名称只使用安全文件名字符。
fn safe_file_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// 递归拒绝 host 配置中存放凭据正文的键，凭据只能由旧 vault 读取。
fn contains_inline_credential(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(table) => table.iter().any(|(key, nested)| {
            matches!(
                key.as_str(),
                "password" | "credential_secret" | "inline_private_key" | "key_passphrase"
            ) || contains_inline_credential(nested)
        }),
        toml::Value::Array(values) => values.iter().any(contains_inline_credential),
        _ => false,
    }
}

/// 通过旧 vault 的真实解密 API 校验 Host 引用命中的 entry 和合成明文摘要。
fn assert_vault_entry(root: &std::path::Path, expected: &Value, linked_entry_id: &str) {
    let paths = KerminalPaths::from_root(root);
    let vault = EncryptedVaultService::new(paths);
    let key = vault.read_key().expect("旧版读取 vault key");
    assert_eq!(key.key_id, manifest_string(expected, "vault.keyId"));
    let vault_file = vault.read_vault().expect("旧版读取 encrypted vault");
    let entry_id = manifest_string(expected, "vault.entryId");
    assert_eq!(linked_entry_id, entry_id);
    let entry = vault_file
        .entries
        .iter()
        .find(|entry| entry.id == entry_id)
        .expect("旧版 vault 应读取到目标合成条目");
    let associated_data = BASE64
        .decode(&entry.associated_data)
        .expect("vault associated data 应为 base64");
    let mut plaintext = vault
        .decrypt_secret(&key, entry, &associated_data)
        .expect("旧版 vault 应使用同一 key 解密条目");
    let digest = Sha256::digest(&plaintext);
    let digest_hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    plaintext.fill(0);
    assert_eq!(
        digest_hex,
        manifest_string(expected, "vault.plaintextSha256").to_ascii_lowercase()
    );
}

/// 用冻结版 russh known_hosts parser 读取新 adapter 写入的 OpenSSH 记录，不自行解析文本。
fn assert_legacy_known_hosts(root: &std::path::Path, expected: &Value) {
    let host = manifest_string(expected, "knownHosts.host");
    let port = expected["knownHosts"]["port"]
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .expect("manifest knownHosts.port 必须是 u16");
    let path = root.join("known_hosts");
    let records = known_hosts::known_host_keys_path(host, port, &path)
        .expect("冻结版 russh parser 应读取新 adapter 写入的 known_hosts");
    let algorithm = manifest_string(expected, "knownHosts.algorithm");
    let fingerprint = manifest_string(expected, "knownHosts.fingerprintSha256");
    assert!(
        records.iter().any(|(_, key)| {
            key.algorithm().to_string() == algorithm
                && key.fingerprint(HashAlg::Sha256).to_string() == fingerprint
        }),
        "冻结版 russh parser 应找到匹配的算法与 SHA-256 指纹"
    );
}

/// 经旧版 SQLite 初始化/迁移和历史查询入口确认一条命令数据真实可读。
fn assert_command_history(root: &std::path::Path, expected: &Value) {
    let paths = KerminalPaths::from_root(root);
    let store = CommandSqliteStore::open(&paths).expect("旧版打开并迁移命令数据库");
    assert_eq!(store.schema_version().expect("读取旧 schema version"), 3);
    let entry_id = manifest_string(expected, "history.entryId");
    let command = manifest_string(expected, "history.command");
    let history = store.list_command_history().expect("旧版列出命令历史");
    let entry = history
        .iter()
        .find(|entry| entry.id == entry_id)
        .expect("旧版历史列表应包含生成器写入的条目");
    assert_eq!(entry.command, command);
}
