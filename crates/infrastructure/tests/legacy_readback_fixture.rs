//! 为新旧配置回读验收生成真实、隔离且可重复定位的合成数据。
//!
//! 此测试默认忽略；显式设置 `KERMINAL_LEGACY_READBACK_RUN_ID` 后才会在仓库 target 下新建输出。
//!
//! @author kongweiguang

use std::{
    env, fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use kerminal_application::{
    agents::{
        AgentWorkspaceDocument, AgentWorkspaceDocumentContent, AgentWorkspaceDocumentKind,
        AgentWorkspaceRepository, AgentWorkspaceRevision,
    },
    commands::{
        CommandHistoryDraft, CommandHistoryRepository, SnippetRepository, WorkflowRepository,
    },
    connections::{
        CredentialResolver, HostKeyIdentity, HostKeyStatus, KnownHostsRepository,
        ResolvedCredential,
        credentials::{
            CredentialService, SaveCredentialRequest, SecretValue, VaultKeyManagementService,
        },
        management::{HostAuthenticationMode, HostDraft, HostGroupDraft, HostManagementService},
    },
    preferences::{
        AgentLauncherSettings, ConfigurationRepository, CustomAgentDefinition,
        DefaultLocalShellProvider, PreferencesService, ProfileDraft, ProfileRepository,
        ProfileService, ThemeMode,
    },
    workspace::{
        WorkspaceSessionDocument, WorkspaceSessionExpectedRevision, WorkspaceSessionLoad,
        WorkspaceSessionRepository, WorkspaceSessionService,
    },
};
use kerminal_domain::{
    agents::{
        AgentLauncherKey, AgentProvider, AgentProviderSession, AgentSession, AgentSessionLaunch,
        AgentSessionScope, AgentSessionStatus,
    },
    commands::{
        CommandHistorySource, CommandHistoryTarget, CommandWorkflow, CommandWorkflowStep,
        SnippetDefaultAction, SnippetDefinition, SnippetRisk, SnippetScope, WorkflowScope,
    },
    connections::{
        HostEndpoint, RemoteProtocol, SshAuthentication,
        credentials::{CredentialKind, CredentialMaterial, CredentialScope},
    },
    identity::{AgentSessionId, HostId, PaneId, TabId},
};
use kerminal_infrastructure::{
    agents::FileAgentWorkspaceRepository,
    command_history::SqliteCommandHistoryRepository,
    commands::FileCommandCatalogRepository,
    configuration::ConfigurationStore,
    connections::{KnownHostsStore, management::HostConfigurationRepository},
    platform::default_shell::SystemDefaultLocalShellProvider,
    vault::VaultStore,
    workspace::WorkspaceSessionStore,
};
use russh::keys::{HashAlg, ssh_key::PublicKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

const RUN_ID_ENV: &str = "KERMINAL_LEGACY_READBACK_RUN_ID";
const PROFILE_ID: &str = "legacy-readback-profile";
const HOST_ID: &str = "legacy-readback-host";
const GROUP_ID: &str = "legacy-readback-group";
const SNIPPET_ID: &str = "legacy-readback-snippet";
const WORKFLOW_ID: &str = "legacy-readback-workflow";
const CUSTOM_AGENT_ID: &str = "a17e4f12-8c30-4b91-a562-5de74a130001";
const AGENT_SESSION_ID: &str = "legacy-readback-agent-session";
const CUSTOM_AGENT_NAME: &str = "Legacy Readback Agent";
const CUSTOM_AGENT_COMMAND: &str = "fixture-agent --legacy-readback-only";
const AGENT_TIMESTAMP: &str = "2026-10-17T00:00:00Z";
const ACTIVE_TAB_ID: &str = "legacy-readback-tab";
const SECOND_TAB_ID: &str = "legacy-readback-second-tab";
const WORKSPACE_GROUP_ID: &str = "legacy-readback-workspace-group";
const LEFT_PANE_ID: &str = "legacy-readback-left-pane";
const FOCUSED_PANE_ID: &str = "legacy-readback-pane";
const RIGHT_PANE_ID: &str = "legacy-readback-right-pane";
const SECOND_TAB_PANE_ID: &str = "legacy-readback-second-tab-pane";
const ROOT_SPLIT_ID: &str = "legacy-readback-root-split";
const NESTED_SPLIT_ID: &str = "legacy-readback-nested-split";
const KNOWN_HOST_PUBLIC_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
const FIXTURE_SECRET: &str = "synthetic-legacy-readback-secret-2026";
const FIXTURE_COMMAND: &str = "echo legacy_readback_fixture_marker";

/// 经安全校验后记录一次生成结果；只包含旧回读脚本定位所需的合成业务值。
///
/// @author kongweiguang
#[tokio::test]
#[ignore = "仅由隔离的旧新数据回读验收显式调用"]
async fn generate_real_legacy_readback_fixture() {
    let run_id = env::var(RUN_ID_ENV)
        .unwrap_or_else(|_| panic!("显式设置 {RUN_ID_ENV} 后才能生成隔离验收数据"));
    assert!(is_safe_run_id(&run_id), "验收 run-id 格式无效");

    let (config_root, generated_root) =
        prepare_output_directories(&run_id).expect("准备仓库内全新隔离输出目录");
    let configuration = ConfigurationStore::initialize(&config_root).expect("初始化隔离配置仓储");
    let storage = configuration.storage_coordinator();
    let writable = configuration.recovery_state().mode
        == kerminal_domain::preferences::ConfigurationMode::ReadWrite;
    assert!(writable, "新建隔离配置根必须处于可写状态");

    let preferences_repository: Arc<dyn ConfigurationRepository> = Arc::new(configuration.clone());
    let preferences = PreferencesService::new(preferences_repository);
    let initial_settings = preferences
        .load_settings()
        .await
        .expect("通过 PreferencesService 读取默认设置");
    let mut settings = initial_settings.settings;
    settings.theme_mode = ThemeMode::Dark;
    settings.agent_launcher = AgentLauncherSettings {
        selected_agent_key: format!("custom:{CUSTOM_AGENT_ID}"),
        custom_agents: vec![CustomAgentDefinition {
            id: CUSTOM_AGENT_ID.to_owned(),
            name: CUSTOM_AGENT_NAME.to_owned(),
            command: CUSTOM_AGENT_COMMAND.to_owned(),
        }],
    };
    let saved_settings = preferences
        .save_settings(settings, initial_settings.revision)
        .await
        .expect("通过 PreferencesService 真实保存设置");
    assert_eq!(saved_settings.settings.theme_mode, ThemeMode::Dark);
    let readback_settings = preferences
        .load_settings()
        .await
        .expect("重新从配置仓储读取设置");
    assert_eq!(readback_settings.settings.theme_mode, ThemeMode::Dark);
    assert_eq!(
        readback_settings.settings.agent_launcher.selected_agent_key,
        format!("custom:{CUSTOM_AGENT_ID}")
    );
    let theme_mode = theme_mode_key(readback_settings.settings.theme_mode);

    let profile_repository: Arc<dyn ProfileRepository> = Arc::new(configuration.clone());
    let default_shell: Arc<dyn DefaultLocalShellProvider> =
        Arc::new(SystemDefaultLocalShellProvider::new());
    let profiles = ProfileService::new(profile_repository, default_shell);
    let saved_profile = profiles
        .save_profile(
            ProfileDraft {
                id: Some(PROFILE_ID.to_owned()),
                name: "Legacy Readback Profile".to_owned(),
                shell: "fixture-shell.exe".to_owned(),
                args: vec!["--fixture".to_owned()],
                cwd: None,
                env: [("KERMINAL_READBACK_MODE".to_owned(), "synthetic".to_owned())]
                    .into_iter()
                    .collect(),
                sidebar_group_id: None,
                sort_order: 7,
            },
            None,
        )
        .await
        .expect("通过 ProfileService 真实保存 profile");
    let readback_profile = profiles
        .get_profile(&saved_profile.profile.id)
        .await
        .expect("读取 profile")
        .expect("profile 应存在");
    assert_eq!(readback_profile.profile.id, PROFILE_ID);
    assert_eq!(readback_profile.profile.name, "Legacy Readback Profile");
    assert_eq!(readback_profile.profile.shell, "fixture-shell.exe");
    assert_eq!(readback_profile.profile.args, vec!["--fixture".to_owned()]);
    assert_eq!(
        readback_profile
            .profile
            .env
            .get("KERMINAL_READBACK_MODE")
            .map(String::as_str),
        Some("synthetic")
    );

    let vault = VaultStore::open(storage.clone());
    let key_management = VaultKeyManagementService::new(Arc::new(vault.clone()));
    key_management
        .ensure()
        .await
        .expect("在隔离根初始化加密 vault key");
    let credentials = CredentialService::new(Arc::new(vault.clone()));
    let host_id = HostId::parse(HOST_ID).expect("合成 host ID 有效");
    let saved_credential = credentials
        .save_with_metadata(SaveCredentialRequest {
            kind: CredentialKind::parse("ssh-host").expect("SSH credential kind 有效"),
            host_id: host_id.clone(),
            scope: CredentialScope::target(),
            material: CredentialMaterial::Password,
            secret: SecretValue::new(FIXTURE_SECRET.to_owned()),
        })
        .await
        .expect("通过 CredentialService 加密保存合成凭据");
    let resolved = vault
        .resolve(&saved_credential.reference, &CancellationToken::new())
        .await
        .expect("从加密 vault 解密回读合成凭据");
    let plaintext_sha256 = match resolved {
        ResolvedCredential::Password(plaintext) => sha256_hex(plaintext.as_bytes()),
        ResolvedCredential::PrivateKey(_) | ResolvedCredential::KeyPassphrase(_) => {
            panic!("合成凭据材料类型不匹配")
        }
    };

    let host_repository = HostConfigurationRepository::new(configuration.clone());
    let hosts = HostManagementService::new(Arc::new(host_repository.clone()));
    let groups = hosts
        .save_group(
            HostGroupDraft {
                id: Some(GROUP_ID.to_owned()),
                name: "Legacy Readback Group".to_owned(),
                sort_order: 3,
            },
            None,
        )
        .await
        .expect("通过主机目录用例保存分组");
    assert!(groups.groups.iter().any(|group| group.id == GROUP_ID));
    let readback_groups = hosts.load_groups().await.expect("重新读取主机分组");
    let readback_group = readback_groups
        .groups
        .iter()
        .find(|group| group.id == GROUP_ID)
        .expect("分组应从配置仓储读回");
    let saved_host = hosts
        .save_host(
            HostDraft {
                id: Some(host_id.clone()),
                group_id: Some(GROUP_ID.to_owned()),
                name: "Legacy Readback Host".to_owned(),
                note: None,
                protocol: RemoteProtocol::Ssh,
                endpoint: HostEndpoint {
                    host: "fixture.example.invalid".to_owned(),
                    port: 22,
                    username: "fixture-user".to_owned(),
                },
                authentication: Some(SshAuthentication::Password {
                    secret: saved_credential.reference.clone(),
                }),
                authentication_mode: HostAuthenticationMode::Password,
                rdp_password: None,
                route: Default::default(),
                serial: None,
                tags: vec!["legacy-readback".to_owned()],
                is_favorite: false,
                sort_order: 4,
                connect_timeout_ms: 30_000,
                keepalive_interval_ms: Some(60_000),
                terminal_options: Default::default(),
                sftp_options: Default::default(),
                rdp_display: Default::default(),
            },
            None,
        )
        .await
        .expect("通过主机目录用例保存 schema-v3 host");
    assert_eq!(saved_host.id.as_str(), HOST_ID);
    let readback_host = hosts
        .load_host(&host_id)
        .await
        .expect("读取 host")
        .expect("host 应存在");
    assert_eq!(readback_host.group_id.as_deref(), Some(GROUP_ID));
    assert!(matches!(
        readback_host.authentication,
        Some(SshAuthentication::Password { ref secret })
            if secret.as_str() == saved_credential.reference.as_str()
    ));
    let host_source = fs::read_to_string(config_root.join("hosts").join(format!("{HOST_ID}.toml")))
        .expect("只读检查刚写出的 host schema");
    let host_toml: toml::Value = toml::from_str(&host_source).expect("host 输出应为有效 TOML");
    let host_schema_version = host_toml
        .get("schema_version")
        .and_then(toml::Value::as_integer)
        .expect("host 有 schema_version") as u32;
    assert_eq!(host_schema_version, 3);

    // 该合成公钥只模拟用户已确认的信任记录；adapter 仍按正式 OpenSSH 格式落盘。
    let public_key = PublicKey::from_openssh(KNOWN_HOST_PUBLIC_KEY)
        .expect("known_hosts 验收公钥必须是有效 OpenSSH 公钥");
    let known_host_identity = HostKeyIdentity {
        algorithm: public_key.algorithm().to_string(),
        fingerprint_sha256: public_key.fingerprint(HashAlg::Sha256).to_string(),
        public_key_openssh: KNOWN_HOST_PUBLIC_KEY.to_owned(),
    };
    let known_hosts = KnownHostsStore::new(storage.clone(), writable);
    assert_eq!(
        known_hosts
            .inspect(&readback_host.endpoint, &known_host_identity)
            .await
            .expect("读取隔离 known_hosts 状态"),
        HostKeyStatus::Unknown
    );
    known_hosts
        .trust(&readback_host.endpoint, &known_host_identity)
        .await
        .expect("通过 known_hosts adapter 写入合成信任记录");
    assert_eq!(
        known_hosts
            .inspect(&readback_host.endpoint, &known_host_identity)
            .await
            .expect("重新读取隔离 known_hosts 状态"),
        HostKeyStatus::Known
    );

    let command_catalog = FileCommandCatalogRepository::new(storage.clone(), writable);
    let saved_snippet = command_catalog
        .save_user_versioned(snippet_definition(), None)
        .await
        .expect("通过用户片段 repository 保存定义");
    assert_eq!(saved_snippet.definition.id, SNIPPET_ID);
    let readback_snippet = command_catalog
        .get_user(SNIPPET_ID.to_owned())
        .await
        .expect("重新读取用户片段")
        .expect("用户片段应存在");
    assert_eq!(readback_snippet.title, "Legacy Readback Snippet");
    let saved_workflow = command_catalog
        .save_versioned(workflow_definition(), None)
        .await
        .expect("通过工作流 repository 保存定义");
    assert_eq!(saved_workflow.definition.id, WORKFLOW_ID);
    let readback_workflow = command_catalog
        .get(WORKFLOW_ID.to_owned())
        .await
        .expect("重新读取工作流")
        .expect("工作流应存在");
    assert_eq!(readback_workflow.steps[0].id, "legacy-readback-step");

    let history = SqliteCommandHistoryRepository::open_or_unavailable(storage.clone());
    assert_eq!(
        history.availability(),
        kerminal_application::commands::CommandHistoryAvailability::Available,
        "合成 history 数据库应成功初始化"
    );
    let history_entry = history
        .append(CommandHistoryDraft {
            command: FIXTURE_COMMAND.to_owned(),
            source: CommandHistorySource::User,
            target: CommandHistoryTarget::Local,
            session_id: None,
            pane_id: None,
            tab_id: None,
            profile_id: Some(PROFILE_ID.to_owned()),
            remote_host_id: None,
            cwd: None,
            shell: Some("fixture-shell.exe".to_owned()),
        })
        .await
        .expect("通过 SQLite command history port 真实追加记录");
    let history_rows = history
        .list(kerminal_domain::commands::CommandHistoryListRequest::default())
        .await
        .expect("通过 SQLite port 回读历史");
    let readback_history = history_rows
        .iter()
        .find(|entry| entry.id == history_entry.id)
        .expect("追加的命令历史应从 SQLite 读回");
    assert_eq!(readback_history.command, FIXTURE_COMMAND);

    let workspace_repository: Arc<dyn WorkspaceSessionRepository> =
        Arc::new(WorkspaceSessionStore::new(storage.clone(), writable));
    let workspace = WorkspaceSessionService::new(workspace_repository);
    let workspace_document = workspace_document();
    workspace
        .save(
            CancellationToken::new(),
            &workspace_document,
            WorkspaceSessionExpectedRevision::Missing,
        )
        .await
        .expect("通过 WorkspaceSessionService 保存 v3 layout");
    let loaded_workspace = workspace
        .load(CancellationToken::new())
        .await
        .expect("通过 workspace port 回读 session");
    let WorkspaceSessionLoad::Loaded { document, .. } = loaded_workspace else {
        panic!("保存的 workspace 必须按 v3 读回");
    };
    let workspace_version = document
        .as_value()
        .get("version")
        .and_then(Value::as_u64)
        .expect("已保存 workspace 必须有整数版本");
    let snapshot = document
        .workspace_snapshot()
        .expect("解析 v3 typed snapshot");
    assert_eq!(workspace_version, 3);
    assert_eq!(
        snapshot.active_tab_id.as_ref().map(TabId::as_str),
        Some(ACTIVE_TAB_ID)
    );
    assert_eq!(
        snapshot.focused_pane_id.as_ref().map(PaneId::as_str),
        Some(FOCUSED_PANE_ID)
    );

    let agent_workspace: Arc<dyn AgentWorkspaceRepository> =
        Arc::new(FileAgentWorkspaceRepository::with_storage_coordinator(
            config_root.clone(),
            storage.clone(),
        ));
    let agent_fixture = write_archived_agent_fixture(
        agent_workspace.as_ref(),
        &config_root,
        &config_root.join("workspace").join("session.json"),
    )
    .await;

    let manifest = json!({
        "contractVersion": 1,
        "settings": { "themeMode": theme_mode },
        "profileId": readback_profile.profile.id,
        "profile": {
            "name": readback_profile.profile.name,
            "shell": readback_profile.profile.shell,
            "args": readback_profile.profile.args,
            "envMode": readback_profile.profile.env.get("KERMINAL_READBACK_MODE"),
        },
        "hostId": readback_host.id.as_str(),
        "hostSchemaVersion": host_schema_version,
        "host": {
            "name": readback_host.name,
            "host": readback_host.endpoint.host,
            "port": readback_host.endpoint.port,
            "username": readback_host.endpoint.username,
            "protocol": "ssh",
            "authType": "password",
            "secretRef": saved_credential.reference.as_str(),
            "tags": readback_host.tags,
        },
        "groupId": readback_group.id,
        "groupName": readback_group.name,
        "snippetId": saved_snippet.definition.id,
        "workflowId": saved_workflow.definition.id,
        "vault": {
            "keyId": saved_credential.key_id,
            "entryId": saved_credential.entry_id,
            "plaintextSha256": plaintext_sha256,
        },
        "history": {
            "entryId": history_entry.id,
            "command": history_entry.command,
        },
        "workspace": {
            "version": workspace_version,
            "activeTabId": snapshot.active_tab_id.expect("活动 tab 存在").as_str(),
            "focusedPaneId": snapshot.focused_pane_id.expect("焦点 pane 存在").as_str(),
            "secondTabId": SECOND_TAB_ID,
            "groupId": WORKSPACE_GROUP_ID,
            "groupTitle": "Legacy Readback Group",
            "paneIds": [
                LEFT_PANE_ID,
                FOCUSED_PANE_ID,
                RIGHT_PANE_ID,
                SECOND_TAB_PANE_ID,
            ],
            "rootSplitId": ROOT_SPLIT_ID,
            "nestedSplitId": NESTED_SPLIT_ID,
        },
        "knownHosts": {
            "host": readback_host.endpoint.host,
            "port": readback_host.endpoint.port,
            "algorithm": known_host_identity.algorithm,
            "fingerprintSha256": known_host_identity.fingerprint_sha256,
        },
        "agent": agent_fixture,
    });
    write_manifest(&generated_root.join("expected.json"), &manifest)
        .expect("以 create-new 方式写出安全 expected manifest");

    assert!(config_root.join("settings.toml").is_file());
    assert!(
        config_root
            .join("profiles")
            .join(format!("{PROFILE_ID}.toml"))
            .is_file()
    );
    assert!(
        config_root
            .join("hosts")
            .join(format!("{HOST_ID}.toml"))
            .is_file()
    );
    assert!(config_root.join("hosts/groups.toml").is_file());
    assert!(
        config_root
            .join("snippets")
            .join(format!("{SNIPPET_ID}.toml"))
            .is_file()
    );
    assert!(
        config_root
            .join("workflows")
            .join(format!("{WORKFLOW_ID}.toml"))
            .is_file()
    );
    assert!(config_root.join("secrets/vault.toml").is_file());
    assert!(config_root.join("secrets/vault-key.toml").is_file());
    assert!(config_root.join("known_hosts").is_file());
    assert!(config_root.join("data/command.sqlite").is_file());
    assert!(config_root.join("workspace/session.json").is_file());
    let agent_root = config_root
        .join("agents")
        .join("sessions")
        .join(AGENT_SESSION_ID);
    assert!(agent_root.join("session.toml").is_file());
    assert!(agent_root.join("provider.toml").is_file());
    assert!(agent_root.join("context/mcp-endpoint.json").is_file());
    assert!(agent_root.join("context/target-binding.json").is_file());
    assert!(agent_root.join("context/terminal-snapshot.json").is_file());
    assert!(agent_root.join("context/workspace-snapshot.json").is_file());
}

/// 通过 Agent workspace 仓储写入 Custom 归档样本，避免启动 Agent CLI 或创建 PTY。
///
/// @author kongweiguang
async fn write_archived_agent_fixture(
    repository: &dyn AgentWorkspaceRepository,
    config_root: &Path,
    workspace_session_json: &Path,
) -> Value {
    let agent_session_id =
        AgentSessionId::parse(AGENT_SESSION_ID).expect("合成 Agent session ID 有效");
    let session_root = repository
        .create_session_root(agent_session_id.clone(), CancellationToken::new())
        .await
        .expect("通过 Agent workspace port 创建独占 session 目录");
    let workspace_root = repository.workspace_root();
    let launcher_key =
        AgentLauncherKey::new(format!("custom:{CUSTOM_AGENT_ID}"), AgentProvider::Custom)
            .expect("Custom launcher key 与 provider 匹配");
    let session = AgentSession::restore(
        1,
        agent_session_id.clone(),
        AgentProvider::Custom,
        Some(launcher_key),
        CUSTOM_AGENT_NAME.to_owned(),
        AGENT_TIMESTAMP.to_owned(),
        AGENT_TIMESTAMP.to_owned(),
        AgentSessionStatus::Archived,
        workspace_root,
        session_root.clone(),
        AgentSessionLaunch {
            command_label: CUSTOM_AGENT_NAME.to_owned(),
            shell: "fixture-agent".to_owned(),
            args: vec!["--legacy-readback-only".to_owned()],
            cwd: session_root.clone(),
        },
        Some(AgentSessionScope::Global),
        None,
    )
    .expect("合成的归档 Agent session 满足领域不变量");
    let provider = AgentProviderSession {
        schema_version: 1,
        provider: AgentProvider::Custom,
        provider_session_id: None,
        resume_command: None,
        resume_supported: false,
        last_resume_at: None,
    };
    let session_root_relative = Path::new(&session_root)
        .strip_prefix(config_root)
        .expect("Agent session 根应位于隔离配置根内")
        .to_string_lossy()
        .replace('\\', "/");
    let workspace_session_path = workspace_session_json.to_string_lossy().into_owned();

    let text_document = |kind, value: Value| AgentWorkspaceDocument {
        kind,
        content: AgentWorkspaceDocumentContent::Text(
            serde_json::to_string_pretty(&value).expect("序列化合成 Agent context JSON"),
        ),
        expected_revision: AgentWorkspaceRevision::Missing,
    };
    let documents = vec![
        AgentWorkspaceDocument {
            kind: AgentWorkspaceDocumentKind::Session,
            content: AgentWorkspaceDocumentContent::Session(Box::new(session)),
            expected_revision: AgentWorkspaceRevision::Missing,
        },
        AgentWorkspaceDocument {
            kind: AgentWorkspaceDocumentKind::Provider,
            content: AgentWorkspaceDocumentContent::Provider(provider),
            expected_revision: AgentWorkspaceRevision::Missing,
        },
        text_document(
            AgentWorkspaceDocumentKind::McpEndpointContext,
            json!({
                "schemaVersion": 1,
                "agentSessionId": AGENT_SESSION_ID,
                "endpoint": null,
                "transport": "streamable-http",
                "toolsOnly": true,
                "generatedAt": AGENT_TIMESTAMP,
            }),
        ),
        text_document(
            AgentWorkspaceDocumentKind::TargetBindingContext,
            json!({
                "schemaVersion": 1,
                "agentSessionId": AGENT_SESSION_ID,
                "scope": { "kind": "global" },
                "binding": {
                    "bindingId": null,
                    "generation": 0,
                    "status": "unbound",
                    "stale": false,
                    "paneId": null,
                    "tabId": null,
                    "targetTerminalSessionId": null,
                    "targetRef": null,
                    "cwd": null,
                    "shell": null,
                },
                "agentTerminal": null,
                "generatedAt": AGENT_TIMESTAMP,
            }),
        ),
        text_document(
            AgentWorkspaceDocumentKind::TerminalSnapshotContext,
            json!({
                "schemaVersion": 1,
                "agentSessionId": AGENT_SESSION_ID,
                "targetTerminalSessionId": null,
                "capturedBytes": 0,
                "maxBytes": 16384,
                "truncated": false,
                "redacted": false,
                "output": "",
                "generatedAt": AGENT_TIMESTAMP,
            }),
        ),
        text_document(
            AgentWorkspaceDocumentKind::WorkspaceSnapshotContext,
            json!({
                "schemaVersion": 1,
                "agentSessionId": AGENT_SESSION_ID,
                "workspaceRoot": config_root.to_string_lossy(),
                "sessionRoot": session_root,
                "workspaceSessionJson": workspace_session_path,
                "generatedAt": AGENT_TIMESTAMP,
            }),
        ),
    ];
    repository
        .write_documents(agent_session_id, documents, CancellationToken::new())
        .await
        .expect("通过 Agent workspace port 按缺失 revision 提交 session/provider/context");

    json!({
        "definitionId": CUSTOM_AGENT_ID,
        "definitionName": CUSTOM_AGENT_NAME,
        "definitionCommand": CUSTOM_AGENT_COMMAND,
        "launcherKey": format!("custom:{CUSTOM_AGENT_ID}"),
        "sessionId": AGENT_SESSION_ID,
        "schemaVersion": 1,
        "sessionRootRelative": session_root_relative,
        "status": "archived",
        "provider": "custom",
        "providerSessionId": null,
        "resumeCommand": null,
        "resumeSupported": false,
        "selectedAgentKey": format!("custom:{CUSTOM_AGENT_ID}"),
        "launch": {
            "commandLabel": CUSTOM_AGENT_NAME,
            "shell": "fixture-agent",
            "args": ["--legacy-readback-only"],
            "cwd": session_root,
        },
        "workspaceSessionJsonRelative": "workspace/session.json",
        "contextPaths": {
            "mcpEndpoint": format!("{session_root_relative}/context/mcp-endpoint.json"),
            "targetBinding": format!("{session_root_relative}/context/target-binding.json"),
            "terminalSnapshot": format!("{session_root_relative}/context/terminal-snapshot.json"),
            "workspaceSnapshot": format!("{session_root_relative}/context/workspace-snapshot.json"),
        },
    })
}

/// 仅接受安全单路径 run-id，避免验收参数逃逸到仓库输出目录之外。
fn is_safe_run_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// 将已回读的设置枚举映射到 v0.3.45 规则中的稳定小写名称。
fn theme_mode_key(theme_mode: ThemeMode) -> &'static str {
    match theme_mode {
        ThemeMode::Dark => "dark",
        ThemeMode::Light => "light",
        ThemeMode::System => "system",
    }
}

/// 逐层创建普通仓库目录并核对 canonical containment，拒绝已有 run 以免覆盖旧样本。
fn prepare_output_directories(run_id: &str) -> io::Result<(PathBuf, PathBuf)> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repository_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "无法定位仓库根"))?
        .canonicalize()?;
    let mut current = repository_root.clone();
    for component in ["target", "acceptance", "legacy-readback"] {
        current.push(component);
        create_owned_directory(&current, &repository_root)?;
    }

    let run_root = current.join(run_id);
    fs::create_dir(&run_root)?;
    ensure_owned_directory(&run_root, &repository_root)?;
    let generated_root = run_root.join("generated");
    fs::create_dir(&generated_root)?;
    ensure_owned_directory(&generated_root, &repository_root)?;
    let config_root = generated_root.join("config");
    reject_sensitive_overlap(&config_root)?;
    fs::create_dir(&config_root)?;
    ensure_owned_directory(&config_root, &repository_root)?;
    Ok((config_root, generated_root))
}

/// 只创建直接子目录；若目录已存在则拒绝符号链接、文件和仓库外重解析。
fn create_owned_directory(path: &Path, repository_root: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => ensure_owned_directory(path, repository_root),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            ensure_owned_directory(path, repository_root)
        }
        Err(error) => Err(error),
    }
}

/// 确认已存在的目录不是链接，且 canonical 路径仍在目标仓库内。
fn ensure_owned_directory(path: &Path, repository_root: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "验收输出路径不是普通目录",
        ));
    }
    let canonical = path.canonicalize()?;
    if !canonical.starts_with(repository_root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "验收输出目录解析到仓库之外",
        ));
    }
    Ok(())
}

/// 拒绝与当前用户 home 或显式运行配置根重叠，防止验收意外改写真实数据。
fn reject_sensitive_overlap(candidate: &Path) -> io::Result<()> {
    let candidate = absolute_normalized(candidate)?;
    let home = dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "无法确认用户 home，拒绝生成"))?;
    let home = absolute_normalized(&home)?;
    if paths_overlap(&candidate, &home) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "验收输出与用户 home 重叠",
        ));
    }
    if let Some(config_root) = env::var_os("KERMINAL_CONFIG_ROOT") {
        let config_root = absolute_normalized(Path::new(&config_root))?;
        if paths_overlap(&candidate, &config_root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "验收输出与 KERMINAL_CONFIG_ROOT 重叠",
            ));
        }
    }
    Ok(())
}

/// 将路径化为绝对词法路径，使尚不存在的 KERMINAL_CONFIG_ROOT 也能参与隔离判断。
fn absolute_normalized(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

/// 比较路径时同时防止根路径大小写或斜杠差异绕过 home/config-root 隔离。
fn paths_overlap(left: &Path, right: &Path) -> bool {
    fn normalized_text(path: &Path) -> String {
        path.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    }
    let left = normalized_text(left);
    let right = normalized_text(right);
    is_path_prefix(&left, &right) || is_path_prefix(&right, &left)
}

/// 仅把完整路径段前缀视为包含，避免 `Kerminal2` 被误判成 `Kerminal` 的子路径。
fn is_path_prefix(prefix: &str, path: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('\\'))
}

/// 生成最小有效用户片段，通过真实领域定义和文件仓储完成校验、编码及落盘。
fn snippet_definition() -> SnippetDefinition {
    SnippetDefinition {
        id: SNIPPET_ID.to_owned(),
        title: "Legacy Readback Snippet".to_owned(),
        command: "whoami".to_owned(),
        description: Some("Synthetic read-only command for compatibility checks".to_owned()),
        tags: vec!["legacy-readback".to_owned()],
        sort_order: 2,
        scope: SnippetScope::Local,
        category: Some("validation".to_owned()),
        risk: SnippetRisk::Inspect,
        default_action: SnippetDefaultAction::Insert,
        variables: Vec::new(),
        context_bindings: Vec::new(),
        derived_from: None,
    }
}

/// 生成含一个只读命令步骤的有效工作流，时间值使用确定的合成标记便于旧侧比较。
fn workflow_definition() -> CommandWorkflow {
    CommandWorkflow {
        id: WORKFLOW_ID.to_owned(),
        title: "Legacy Readback Workflow".to_owned(),
        description: Some("Synthetic workflow for schema compatibility checks".to_owned()),
        tags: vec!["legacy-readback".to_owned()],
        scope: WorkflowScope::Local,
        steps: vec![CommandWorkflowStep {
            id: "legacy-readback-step".to_owned(),
            title: "Read identity".to_owned(),
            command: "whoami".to_owned(),
            description: Some("Read-only identity probe".to_owned()),
            scope: Some(WorkflowScope::Local),
            requires_confirmation: false,
            sort_order: 0,
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        }],
        sort_order: 1,
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
    }
}

/// 构造含分组、多 Tab 与递归分屏的 v3 样本，先由正式 codec 验证再交仓储 CAS 保存。
fn workspace_document() -> WorkspaceSessionDocument {
    let mut terminal_tab_groups = serde_json::Map::new();
    terminal_tab_groups.insert(
        WORKSPACE_GROUP_ID.to_owned(),
        json!({
            "title": "Legacy Readback Group",
            "color": "teal",
            "collapsed": false,
        }),
    );
    WorkspaceSessionDocument::try_from_value(json!({
        "version": 3,
        "activeTabId": ACTIVE_TAB_ID,
        "focusedPaneId": FOCUSED_PANE_ID,
        "selectedMachineId": "legacy-readback-machine",
        "sidebarMachines": [],
        "terminalTabGroups": terminal_tab_groups,
        "terminalPanes": [
            workspace_pane(LEFT_PANE_ID, "Legacy Readback Left Pane"),
            workspace_pane(FOCUSED_PANE_ID, "Legacy Readback Focused Pane"),
            workspace_pane(RIGHT_PANE_ID, "Legacy Readback Right Pane"),
            workspace_pane(SECOND_TAB_PANE_ID, "Legacy Readback Second Tab Pane"),
        ],
        "terminalTabs": [{
            "id": ACTIVE_TAB_ID,
            "kind": "terminal",
            "title": "Legacy Readback Workspace",
            "machineId": "legacy-readback-machine",
            "tabGroupId": WORKSPACE_GROUP_ID,
            "layout": {
                "type": "split",
                "id": ROOT_SPLIT_ID,
                "direction": "horizontal",
                "children": [
                    { "type": "pane", "paneId": LEFT_PANE_ID },
                    {
                        "type": "split",
                        "id": NESTED_SPLIT_ID,
                        "direction": "vertical",
                        "children": [
                            { "type": "pane", "paneId": FOCUSED_PANE_ID },
                            { "type": "pane", "paneId": RIGHT_PANE_ID },
                        ],
                    },
                ],
            },
        }, {
            "id": SECOND_TAB_ID,
            "kind": "terminal",
            "title": "Legacy Readback Second Tab",
            "machineId": "legacy-readback-machine",
            "tabGroupId": WORKSPACE_GROUP_ID,
            "layout": { "type": "pane", "paneId": SECOND_TAB_PANE_ID },
        }]
    }))
    .expect("synthetic workspace v3 document must satisfy the real codec")
}

/// 复用相同目标与 profile 构造 Pane，使兼容样本关注布局字段而不引入运行态资源。
fn workspace_pane(id: &str, title: &str) -> Value {
    json!({
        "id": id,
        "title": title,
        "machineId": "legacy-readback-machine",
        "mode": "local",
        "target": { "kind": "local", "profileId": PROFILE_ID },
        "profileId": PROFILE_ID,
        "status": "offline",
        "prompt": "PS>"
    })
}

/// 计算只用于旧侧 secret 解密回读的 SHA-256；明文本身不进入 manifest 或输出。
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 以 create-new 写入纯合成 expected manifest，禁止覆盖同 run-id 下的既有结果。
fn write_manifest(path: &Path, manifest: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()
}
