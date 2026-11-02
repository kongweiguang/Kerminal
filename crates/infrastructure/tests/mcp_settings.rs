//! MCP 设置和服务专用 Vault 令牌的隔离回归。
//!
//! @author kongweiguang

use kerminal_application::{
    mcp_runtime::{MCP_TOKEN_REFERENCE, McpTokenStore},
    preferences::{PreferencesService, settings::McpBindMode},
};
use kerminal_infrastructure::{
    configuration::ConfigurationStore, storage::StorageCoordinator, vault::VaultStore,
};
use std::{fs, sync::Arc};

/// 令牌只在显式创建后存在；缺失引用不自动换发，密文文件不含令牌正文。
#[tokio::test]
async fn mcp_vault_token_is_stable_and_encrypted() {
    let root = tempfile::tempdir().unwrap();
    let vault = VaultStore::open(StorageCoordinator::new(root.path().to_owned()));
    assert!(
        vault
            .load_or_create(Some(MCP_TOKEN_REFERENCE))
            .await
            .is_err()
    );
    assert!(!root.path().join("secrets/vault.toml").exists());
    let first = vault.load_or_create(None).await.unwrap();
    assert_eq!(first.reference(), MCP_TOKEN_REFERENCE);
    let second = vault
        .load_or_create(Some(MCP_TOKEN_REFERENCE))
        .await
        .unwrap();
    assert_eq!(first.secret(), second.secret());
    let bytes = fs::read_to_string(root.path().join("secrets/vault.toml")).unwrap();
    assert!(!bytes.contains(first.secret()));
    assert!(!format!("{first:?}").contains(first.secret()));
}

/// 原文注释与未知字段在 MCP CAS 更新后仍保留，旧浅深字段只在读取投影标记待归并。
#[tokio::test]
async fn settings_preserve_extensions_and_identify_legacy_terminal_theme() {
    let root = tempfile::tempdir().unwrap();
    let source = "schema_version = 1\n[terminal]\nlightColorScheme = \"kerminal\"\ndarkColorScheme = \"kerminal\"\n[mcp]\n# user note\nbindMode = \"loopback\"\nport = 37657\ncustom = \"keep\"\n";
    fs::write(root.path().join("settings.toml"), source).unwrap();
    let store = ConfigurationStore::initialize(root.path()).unwrap();
    let preferences = PreferencesService::new(Arc::new(store));
    let loaded = preferences.load_settings().await.unwrap();
    assert!(loaded.settings.terminal.legacy_color_scheme_missing);
    assert_eq!(
        fs::read_to_string(root.path().join("settings.toml")).unwrap(),
        source
    );
    let mut next = loaded.settings;
    next.terminal.resolve_legacy_terminal_scheme(true);
    next.mcp.bind_mode = McpBindMode::Lan;
    next.mcp.port = 41017;
    next.mcp.token_ref = Some(MCP_TOKEN_REFERENCE.to_owned());
    let saved = preferences
        .save_settings(next, loaded.revision)
        .await
        .unwrap();
    assert_eq!(saved.settings.mcp.port, 41017);
    let result = fs::read_to_string(root.path().join("settings.toml")).unwrap();
    assert!(result.contains("# user note"));
    assert!(result.contains("custom = \"keep\""));
    assert!(result.contains("service:kerminal:mcp-token:v1"));
    assert!(!saved.settings.terminal.legacy_color_scheme_missing);
}

/// 后台只调整 MCP 时没有窗口外观上下文，必须保留旧浅深主题而不是写入默认主题。
#[tokio::test]
async fn unrelated_mcp_save_keeps_unresolved_legacy_terminal_themes() {
    let root = tempfile::tempdir().unwrap();
    let source = "schema_version = 1\nthemeMode = \"system\"\n[terminal]\nlightColorScheme = \"github\"\ndarkColorScheme = \"tokyoNight\"\n";
    fs::write(root.path().join("settings.toml"), source).unwrap();
    let store = ConfigurationStore::initialize(root.path()).unwrap();
    let preferences = PreferencesService::new(Arc::new(store));
    let loaded = preferences.load_settings().await.unwrap();
    assert!(loaded.settings.terminal.legacy_color_scheme_missing);
    let mut next = loaded.settings;
    next.mcp.port = 41018;
    preferences
        .save_settings(next, loaded.revision)
        .await
        .unwrap();
    let persisted: toml::Value =
        toml::from_str(&fs::read_to_string(root.path().join("settings.toml")).unwrap()).unwrap();
    let terminal = persisted.get("terminal").unwrap().as_table().unwrap();
    assert!(!terminal.contains_key("colorScheme"));
    assert_eq!(terminal["lightColorScheme"].as_str(), Some("github"));
    assert_eq!(terminal["darkColorScheme"].as_str(), Some("tokyoNight"));
    assert!(
        preferences
            .load_settings()
            .await
            .unwrap()
            .settings
            .terminal
            .legacy_color_scheme_missing
    );
}
