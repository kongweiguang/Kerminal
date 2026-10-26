//! ConfigFileStore integration tests.
//!
//! @author kongweiguang

use std::{collections::HashMap, fs, path::Path};

use kerminal_lib::{
    models::{
        profile::TerminalProfile,
        remote_host::{
            RemoteHost, RemoteHostAuthType, RemoteHostCredentialStatus, RemoteHostProtocol,
            SshJumpHostOptions, SshOptions,
        },
        settings::{
            AppSettings, TerminalKeywordHighlightMatchMode, TerminalKeywordHighlightRule,
            TerminalKeywordHighlightStyle, TerminalRendererType, ThemeMode, ToolRailPanelPlacement,
            ToolRailToolId,
        },
    },
    storage::{config_file_store::ConfigFileStore, file_store::FileStoreError},
};
use tempfile::tempdir;

/// 配置往返必须保存完整的工具目录和逐工具位置。
#[test]
fn settings_toml_roundtrip_keeps_runtime_model() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());
    let mut settings = AppSettings {
        theme_mode: ThemeMode::Light,
        ..AppSettings::default()
    };
    settings.terminal.renderer_type = TerminalRendererType::Gpu;
    settings.terminal.keyword_highlights.rules = vec![TerminalKeywordHighlightRule {
        id: "java-errors".to_string(),
        enabled: true,
        pattern: "error|exception".to_string(),
        match_mode: TerminalKeywordHighlightMatchMode::Regex,
        case_sensitive: false,
        note: "Java 错误".to_string(),
        style: TerminalKeywordHighlightStyle::Red,
        custom_colors: None,
    }];
    settings.tool_rail.order = vec![ToolRailToolId::Logs, ToolRailToolId::Context];
    settings.tool_rail.hidden = vec![ToolRailToolId::System];
    settings.tool_rail.bottom = vec![ToolRailToolId::Context];
    settings
        .tool_rail
        .panel_placements
        .insert(ToolRailToolId::Context, ToolRailPanelPlacement::Left);
    settings
        .tool_rail
        .panel_placements
        .insert(ToolRailToolId::System, ToolRailPanelPlacement::Bottom);
    settings.tool_rail = settings.tool_rail.clone().normalized();

    store.write_settings(&settings).expect("write settings");
    let loaded = store.read_settings().expect("read settings");
    let source = fs::read_to_string(temp.path().join("settings.toml")).expect("settings source");

    assert_eq!(loaded, settings);
    assert!(source.contains("schema_version = 1"));
    assert!(source.contains("themeMode = \"light\""));
    assert!(source.contains("rendererType = \"gpu\""));
    assert!(source.contains("[terminal.keywordHighlights]"));
    assert!(source.contains("[[terminal.keywordHighlights.rules]]"));
    assert!(source.contains("matchMode = \"regex\""));
    assert!(source.contains("[toolRail]"));
    assert!(source.contains("agentLauncher"));
    assert!(source.contains("containers"));
    assert!(source.contains("bottom = [\"context\"]"));
    assert!(source.contains("[toolRail.panelPlacements]"));
    assert!(source.contains("context = \"left\""));
    assert!(source.contains("system = \"bottom\""));
}

#[test]
fn settings_toml_defaults_missing_terminal_renderer_type_to_cpu() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());
    let settings = AppSettings::default();

    store.write_settings(&settings).expect("write settings");
    let source = fs::read_to_string(temp.path().join("settings.toml")).expect("settings source");
    let source_without_renderer = source.replace("rendererType = \"cpu\"\n", "");
    fs::write(temp.path().join("settings.toml"), source_without_renderer)
        .expect("write legacy settings");

    let loaded = store.read_settings().expect("read legacy settings");

    assert_eq!(loaded.terminal.renderer_type, TerminalRendererType::Cpu);
    assert_eq!(
        loaded.terminal.keyword_highlights,
        AppSettings::default().terminal.keyword_highlights
    );
    assert_eq!(loaded.tool_rail, AppSettings::default().tool_rail);
}

/// 旧版全隐藏列表归一化时要补齐新工具，并恢复原排序第一项。
#[test]
fn settings_toml_normalizes_duplicate_and_all_hidden_tool_rail_entries() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());
    fs::write(
        temp.path().join("settings.toml"),
        concat!(
            "schema_version = 1\n",
            "[toolRail]\n",
            "order = [\"logs\", \"logs\"]\n",
            "hidden = [\"logs\", \"context\", \"agentLauncher\", \"sftp\", \"snippets\", \"tmux\", \"ports\", \"system\", \"containers\"]\n",
            "bottom = [\"context\", \"logs\", \"context\"]\n",
        ),
    )
    .expect("write tool rail settings");

    let loaded = store.read_settings().expect("read tool rail settings");

    assert_eq!(loaded.tool_rail.order[0], ToolRailToolId::Logs);
    assert!(!loaded.tool_rail.hidden.contains(&ToolRailToolId::Logs));
    assert_eq!(loaded.tool_rail.order.len(), 9);
    assert_eq!(
        loaded.tool_rail.bottom,
        vec![ToolRailToolId::Logs, ToolRailToolId::Context]
    );
}

/// 没有逐工具位置配置的旧文件为新增容器面板补上 attached 默认值。
#[test]
fn settings_toml_defaults_legacy_tool_rail_section_to_logs_bottom_and_attached_panel() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());
    fs::write(
        temp.path().join("settings.toml"),
        concat!(
            "schema_version = 1\n",
            "[toolRail]\n",
            "order = [\"context\", \"logs\"]\n",
            "hidden = []\n",
        ),
    )
    .expect("write legacy tool rail settings");

    let loaded = store
        .read_settings()
        .expect("read legacy tool rail settings");

    assert_eq!(loaded.tool_rail.bottom, vec![ToolRailToolId::Logs]);
    assert_eq!(
        loaded.tool_rail.panel_placements.get(&ToolRailToolId::Logs),
        Some(&ToolRailPanelPlacement::Attached)
    );
    assert_eq!(
        loaded
            .tool_rail
            .panel_placements
            .get(&ToolRailToolId::Containers),
        Some(&ToolRailPanelPlacement::Attached)
    );
}

/// 旧版全局展示位置要扩展到全部工具（含新增项），并在下一次写回时只保留逐工具格式。
#[test]
fn settings_toml_migrates_legacy_global_tool_panel_placement() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());
    fs::write(
        temp.path().join("settings.toml"),
        concat!(
            "schema_version = 1\n",
            "[toolRail]\n",
            "panelPlacement = \"center\"\n",
        ),
    )
    .expect("write global tool panel placement");

    let loaded = store
        .read_settings()
        .expect("migrate global tool panel placement");
    assert_eq!(loaded.tool_rail.panel_placements.len(), 9);
    assert!(loaded
        .tool_rail
        .panel_placements
        .values()
        .all(|placement| *placement == ToolRailPanelPlacement::Center));

    store
        .write_settings(&loaded)
        .expect("rewrite migrated tool panel placements");
    let rewritten =
        fs::read_to_string(temp.path().join("settings.toml")).expect("rewritten settings");
    assert!(rewritten.contains("[toolRail.panelPlacements]"));
    assert!(!rewritten.contains("panelPlacement ="));
}

#[test]
fn settings_toml_rejects_unknown_tool_rail_entry() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());
    fs::write(
        temp.path().join("settings.toml"),
        "schema_version = 1\n[toolRail]\norder = [\"unknown\"]\n",
    )
    .expect("write invalid tool rail settings");

    store
        .read_settings()
        .expect_err("unknown tool rail entry should fail closed");
}

#[test]
fn settings_toml_rejects_unknown_tool_panel_placement() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());
    fs::write(
        temp.path().join("settings.toml"),
        "schema_version = 1\n[toolRail.panelPlacements]\ncontext = \"detached\"\n",
    )
    .expect("write invalid tool panel placement");

    store
        .read_settings()
        .expect_err("unknown tool panel placement should fail closed");
}

#[test]
fn settings_toml_defaults_missing_command_block_rail_switch_to_enabled() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());
    let settings = AppSettings::default();

    store.write_settings(&settings).expect("write settings");
    let source = fs::read_to_string(temp.path().join("settings.toml")).expect("settings source");
    let source_without_switch = source.replace("showCommandBlockRail = true\n", "");
    fs::write(temp.path().join("settings.toml"), source_without_switch)
        .expect("write legacy settings");

    let loaded = store.read_settings().expect("read legacy settings");

    assert!(loaded.terminal.show_command_block_rail);
}

#[test]
fn settings_toml_keeps_explicit_auto_renderer_mode() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());
    let mut settings = AppSettings::default();
    settings.terminal.renderer_type = TerminalRendererType::Auto;

    store.write_settings(&settings).expect("write settings");
    let loaded = store.read_settings().expect("read settings");

    assert_eq!(loaded.terminal.renderer_type, TerminalRendererType::Auto);
}

#[test]
fn profile_toml_roundtrip_uses_one_file_per_profile() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());
    let mut env = HashMap::new();
    env.insert("RUST_LOG".to_owned(), "debug".to_owned());
    let profile = TerminalProfile {
        id: "profile-pwsh".to_owned(),
        name: "PowerShell".to_owned(),
        shell: "pwsh".to_owned(),
        args: vec!["-NoLogo".to_owned()],
        cwd: Some("C:/dev".to_owned()),
        env,
        is_default: true,
        sidebar_group_id: Some("group-local".to_owned()),
        sort_order: 10,
        created_at: "2026-06-24 08:00:00".to_owned(),
        updated_at: "2026-06-24 08:00:00".to_owned(),
    };

    store.write_profile(&profile).expect("write profile");
    let loaded = store.read_profile("profile-pwsh").expect("read profile");

    assert_eq!(loaded, profile);
    let source = fs::read_to_string(temp.path().join("profiles").join("profile-pwsh.toml"))
        .expect("profile source");
    assert!(source.contains("sidebar_group_id = \"group-local\""));
    assert!(temp
        .path()
        .join("profiles")
        .join("profile-pwsh.toml")
        .is_file());
}

#[test]
fn profile_toml_rejects_path_like_profile_ids() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());

    let error = store
        .read_profile("../default")
        .expect_err("path traversal profile id should fail");

    assert!(matches!(error, FileStoreError::InvalidPath(_)));
}

#[test]
fn settings_toml_rejects_wrong_schema_version() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());
    fs::write(
        temp.path().join("settings.toml"),
        "schema_version = 99\ntheme = \"dark\"\n",
    )
    .expect("write invalid schema");

    let error = store.read_settings().expect_err("wrong schema should fail");

    let diagnostics = parse_diagnostics(&error);
    assert_eq!(
        diagnostics[0].path.as_deref(),
        Some(Path::new("settings.toml"))
    );
    assert_eq!(diagnostics[0].line, 1);
    assert_eq!(diagnostics[0].column, 1);
    assert_eq!(diagnostics[0].key.as_deref(), Some("schema_version"));
    assert!(diagnostics[0]
        .recovery
        .as_deref()
        .is_some_and(|recovery| recovery.contains("schema_version = 1")));
}

#[test]
fn malformed_toml_reports_safe_path_line_column_and_key() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());
    fs::write(
        temp.path().join("settings.toml"),
        "schema_version = 1\nthemeMode = \"dark\"\nterminal = [\n",
    )
    .expect("write malformed settings");

    let error = store
        .read_settings()
        .expect_err("malformed TOML should fail");
    let diagnostics = parse_diagnostics(&error);

    assert_eq!(
        diagnostics[0].path.as_deref(),
        Some(Path::new("settings.toml"))
    );
    assert_eq!(diagnostics[0].line, 3);
    assert!(diagnostics[0].column >= 1);
    assert_eq!(diagnostics[0].key.as_deref(), Some("terminal"));
    assert!(diagnostics[0]
        .recovery
        .as_deref()
        .is_some_and(|recovery| recovery.contains("last-known-good")
            || recovery.contains("kerminal.config.validate")));
}

#[test]
fn remote_host_toml_does_not_persist_transient_secrets() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());
    let mut host = RemoteHost {
        id: "host-1".to_owned(),
        group_id: None,
        name: "prod".to_owned(),
        host: "prod.internal".to_owned(),
        port: 22,
        username: "deploy".to_owned(),
        protocol: Default::default(),
        auth_type: RemoteHostAuthType::Password,
        credential_ref: None,
        secret_ref: None,
        key_passphrase_ref: None,
        key_passphrase_secret: None,
        credential_secret: Some("target-secret".to_owned()),
        credential_status: Default::default(),
        tags: vec!["prod".to_owned()],
        ssh_options: SshOptions::default(),
        sort_order: 10,
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
    };
    host.ssh_options.jump_hosts.push(SshJumpHostOptions {
        name: "jump".to_owned(),
        host: "jump.internal".to_owned(),
        port: 22,
        username: "ops".to_owned(),
        auth_type: RemoteHostAuthType::Password,
        credential_ref: None,
        secret_ref: None,
        key_passphrase_ref: None,
        key_passphrase_secret: None,
        credential_secret: Some("jump-secret".to_owned()),
        credential_status: Default::default(),
    });

    store
        .apply_remote_host_change_set(None, &[host.clone()], &[])
        .expect("write host");

    let host_source =
        fs::read_to_string(temp.path().join("hosts/host-1.toml")).expect("host source");
    let loaded = store
        .remote_host_by_id("host-1")
        .expect("read host")
        .expect("host exists");

    assert!(!host_source.contains("target-secret"));
    assert!(!host_source.contains("jump-secret"));
    assert!(!host_source.contains("credential_secret"));
    assert!(host_source.contains("schema_version = 2"));
    assert!(host_source.contains("protocol = \"ssh\""));
    assert!(!temp.path().join("secrets/hosts/host-1.toml").exists());
    assert_eq!(loaded.credential_secret, None);
    assert_eq!(
        loaded
            .ssh_options
            .jump_hosts
            .first()
            .and_then(|jump| jump.credential_secret.as_ref()),
        None
    );
}

#[test]
fn remote_host_toml_tree_uses_runtime_ungrouped_group() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());
    let host = RemoteHost {
        id: "host-2".to_owned(),
        group_id: None,
        name: "standalone".to_owned(),
        host: "standalone.internal".to_owned(),
        port: 22,
        username: "deploy".to_owned(),
        protocol: Default::default(),
        auth_type: RemoteHostAuthType::Agent,
        credential_ref: None,
        secret_ref: None,
        key_passphrase_ref: None,
        key_passphrase_secret: None,
        credential_secret: None,
        credential_status: RemoteHostCredentialStatus::Agent,
        tags: Vec::new(),
        ssh_options: SshOptions::default(),
        sort_order: 10,
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
    };

    store
        .apply_remote_host_change_set(None, std::slice::from_ref(&host), &[])
        .expect("write host");

    let tree = store.list_remote_host_tree().expect("tree");

    assert_eq!(tree.len(), 1);
    assert_eq!(tree[0].id, "__ungrouped__");
    assert_eq!(tree[0].hosts, vec![host]);
    assert!(!temp.path().join("hosts/__ungrouped__.toml").exists());
}

#[test]
fn remote_host_toml_reads_legacy_production_and_drops_it_on_write() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());
    fs::create_dir_all(temp.path().join("hosts")).expect("hosts dir");
    fs::write(
        temp.path().join("hosts/host-legacy-production.toml"),
        r#"schema_version = 2
protocol = "ssh"
id = "host-legacy-production"
name = "AI added host"
host = "host.internal"
port = 22
username = "deploy"
auth_type = "agent"
production = true
sort_order = 10
created_at = "1"
updated_at = "1"
"#,
    )
    .expect("write host");

    let loaded = store
        .remote_host_by_id("host-legacy-production")
        .expect("read host")
        .expect("host exists");

    assert_eq!(loaded.protocol, RemoteHostProtocol::Ssh);
    store
        .apply_remote_host_change_set(None, std::slice::from_ref(&loaded), &[])
        .expect("write host without legacy classification");
    let rewritten = fs::read_to_string(temp.path().join("hosts/host-legacy-production.toml"))
        .expect("read rewritten host");
    assert!(!rewritten.contains("production ="));
}

#[test]
fn remote_host_toml_rejects_schema_v1_without_tag_inference() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());
    fs::create_dir_all(temp.path().join("hosts")).expect("hosts dir");
    fs::write(
        temp.path().join("hosts/legacy.toml"),
        r#"schema_version = 1
id = "legacy"
name = "legacy"
host = "legacy.internal"
port = 22
username = ""
auth_type = "agent"
tags = ["rdp"]
sort_order = 10
created_at = "1"
updated_at = "1"
"#,
    )
    .expect("write legacy host");

    let error = store
        .remote_host_by_id("legacy")
        .expect_err("schema v1 must fail closed");
    let diagnostics = parse_diagnostics(&error);
    assert!(diagnostics[0].message.contains("expected 2"));
    assert!(diagnostics[0]
        .recovery
        .as_deref()
        .is_some_and(|recovery| recovery.contains("one-time startup upgrader")
            && recovery.contains("explicit protocol")));
}

#[test]
fn remote_host_toml_v2_roundtrips_every_explicit_protocol_without_secrets() {
    for (index, protocol) in [
        RemoteHostProtocol::Ssh,
        RemoteHostProtocol::Sftp,
        RemoteHostProtocol::Rdp,
        RemoteHostProtocol::Telnet,
        RemoteHostProtocol::Serial,
    ]
    .into_iter()
    .enumerate()
    {
        let temp = tempdir().expect("temp dir");
        let store = ConfigFileStore::new(temp.path());
        let host = RemoteHost {
            id: "files-only".to_owned(),
            group_id: None,
            name: "files only".to_owned(),
            host: "files.internal".to_owned(),
            port: 22,
            username: "upload".to_owned(),
            protocol,
            auth_type: RemoteHostAuthType::Agent,
            credential_ref: None,
            secret_ref: None,
            key_passphrase_ref: None,
            key_passphrase_secret: None,
            credential_secret: None,
            credential_status: RemoteHostCredentialStatus::Agent,
            tags: vec!["files".to_owned()],
            ssh_options: SshOptions::default(),
            sort_order: 10,
            created_at: "1".to_owned(),
            updated_at: "1".to_owned(),
        };

        store
            .apply_remote_host_change_set(None, std::slice::from_ref(&host), &[])
            .expect("write host");
        let source = fs::read_to_string(temp.path().join("hosts/files-only.toml"))
            .expect("read host source");
        let loaded = store
            .remote_host_by_id("files-only")
            .expect("read host")
            .expect("host exists");

        assert!(source.contains("schema_version = 2"), "case {index}");
        assert!(source.contains(&format!(
            "protocol = \"{}\"",
            match protocol {
                RemoteHostProtocol::Ssh => "ssh",
                RemoteHostProtocol::Sftp => "sftp",
                RemoteHostProtocol::Rdp => "rdp",
                RemoteHostProtocol::Telnet => "telnet",
                RemoteHostProtocol::Serial => "serial",
            }
        )));
        assert!(!source.contains("credential_secret"));
        assert_eq!(loaded.protocol, protocol);
    }
}

#[test]
fn remote_host_toml_v2_requires_valid_explicit_protocol() {
    for protocol_line in ["", "protocol = \"ftp\"\n"] {
        let temp = tempdir().expect("temp dir");
        let store = ConfigFileStore::new(temp.path());
        fs::create_dir_all(temp.path().join("hosts")).expect("hosts dir");
        fs::write(
            temp.path().join("hosts/invalid-v2.toml"),
            format!(
                "schema_version = 2\n{protocol_line}id = \"invalid-v2\"\nname = \"invalid\"\nhost = \"invalid.internal\"\nport = 22\nusername = \"upload\"\nauth_type = \"agent\"\nsort_order = 10\ncreated_at = \"1\"\nupdated_at = \"1\"\n"
            ),
        )
        .expect("write invalid v2 host");

        let error = store
            .remote_host_by_id("invalid-v2")
            .expect_err("missing or invalid protocol must fail");
        assert_eq!(
            parse_diagnostics(&error)[0].path.as_deref(),
            Some(Path::new("hosts/invalid-v2.toml"))
        );
    }
}

#[test]
fn remote_host_toml_rejects_secret_fields_in_public_host_file() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());
    fs::create_dir_all(temp.path().join("hosts")).expect("hosts dir");
    fs::write(
        temp.path().join("hosts/host-3.toml"),
        "schema_version = 2\nprotocol = \"ssh\"\nid = \"host-3\"\ncredential_secret = \"leak\"\n",
    )
    .expect("write invalid host");

    let error = store
        .remote_host_by_id("host-3")
        .expect_err("secret in public host must fail");

    let diagnostics = parse_diagnostics(&error);
    assert_eq!(
        diagnostics[0].path.as_deref(),
        Some(Path::new("hosts/host-3.toml"))
    );
    assert_eq!(diagnostics[0].line, 4);
    assert_eq!(diagnostics[0].column, 1);
    assert_eq!(diagnostics[0].key.as_deref(), Some("credential_secret"));
    assert!(diagnostics[0]
        .recovery
        .as_deref()
        .is_some_and(
            |recovery| recovery.contains("encrypted vault") && recovery.contains("secret_ref")
        ));
}

#[test]
fn remote_host_toml_rejects_plaintext_password_and_key_fields() {
    for key in ["password", "inline_private_key", "key_passphrase"] {
        let temp = tempdir().expect("temp dir");
        let store = ConfigFileStore::new(temp.path());
        fs::create_dir_all(temp.path().join("hosts")).expect("hosts dir");
        fs::write(
            temp.path().join("hosts/host-secret.toml"),
            format!("schema_version = 1\nid = \"host-secret\"\n{key} = \"placeholder\"\n"),
        )
        .expect("write invalid host");

        let error = store
            .remote_host_by_id("host-secret")
            .expect_err("plaintext secret must fail");
        assert_eq!(parse_diagnostics(&error)[0].key.as_deref(), Some(key));
    }
}

#[test]
fn remote_host_toml_allows_key_passphrase_vault_reference() {
    let temp = tempdir().expect("temp dir");
    let store = ConfigFileStore::new(temp.path());
    fs::create_dir_all(temp.path().join("hosts")).expect("hosts dir");
    fs::write(
        temp.path().join("hosts/host-key-ref.toml"),
        concat!(
            "schema_version = 2\n",
            "protocol = \"ssh\"\n",
            "id = \"host-key-ref\"\n",
            "name = \"key host\"\n",
            "host = \"key.internal\"\n",
            "port = 22\n",
            "username = \"deploy\"\n",
            "auth_type = \"key\"\n",
            "credential_ref = \"/home/deploy/.ssh/id_ed25519\"\n",
            "key_passphrase_ref = \"credential:kerminal:ssh-host:host-key-ref:target:key-passphrase:v1\"\n",
            "tags = []\n",
            "sort_order = 10\n",
            "created_at = \"1\"\n",
            "updated_at = \"1\"\n",
        ),
    )
    .expect("write host with key passphrase ref");

    let host = store
        .remote_host_by_id("host-key-ref")
        .expect("vault reference must be allowed")
        .expect("host exists");

    assert_eq!(
        host.key_passphrase_ref.as_deref(),
        Some("credential:kerminal:ssh-host:host-key-ref:target:key-passphrase:v1")
    );
}

fn parse_diagnostics(
    error: &FileStoreError,
) -> &[kerminal_lib::storage::file_store::ParseDiagnostic] {
    let FileStoreError::TomlParse(parse_error) = error else {
        panic!("expected TOML parse error, got {error:?}");
    };
    parse_error.diagnostics()
}
