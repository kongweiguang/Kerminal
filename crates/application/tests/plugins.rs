//! 插件目录的纯数据契约；运行时及原生视图由各自的测试覆盖。
//!
//! @author kongweiguang

use kerminal_application::plugins::{
    ContentDescriptor, ContentTypeId, HostTypeDescriptor, HostTypeId, PluginDescriptor, PluginId,
    PluginRegistry, PluginRegistryBuilder, PluginRegistryError, ToolDescriptor, ToolId,
};

/// 使用同一测试插件构造三类贡献，避免共享产品实现或启用生产测试入口。
///
/// @author kongweiguang
fn fixture() -> (
    PluginDescriptor,
    HostTypeDescriptor,
    ContentDescriptor,
    ToolDescriptor,
) {
    let provider = PluginDescriptor::new(
        PluginId::new("test.provider").expect("valid provider id"),
        "Test provider",
        true,
    );
    let content = ContentDescriptor::new(
        ContentTypeId::new("test.content").expect("valid content id"),
        provider.id.clone(),
        "Content",
    );
    let host = HostTypeDescriptor::new(
        HostTypeId::new("test.host").expect("valid host id"),
        provider.id.clone(),
        "test.host",
        "Host",
        Some(content.id.clone()),
    );
    let tool = ToolDescriptor::new(
        ToolId::new("test.tool").expect("valid tool id"),
        provider.id.clone(),
        "Tool",
        Some(content.id.clone()),
    );
    (provider, host, content, tool)
}

/// 贡献可先于提供方声明，冻结后按稳定身份查询三类引用。
///
/// @author kongweiguang
#[test]
fn freezes_cross_contribution_test_plugin() {
    let (provider, host, content, tool) = fixture();
    let mut builder = PluginRegistryBuilder::new();
    builder
        .register_builtins()
        .expect("valid builtin declarations");
    builder.register_host_type(host.clone());
    builder.register_tool(tool.clone());
    builder.register_content_type(content.clone());
    builder.register_provider(provider.clone());
    let registry = builder.freeze().expect("valid registry");
    assert_eq!(registry.provider("test.provider"), Some(&provider));
    assert_eq!(registry.host_type("test.host"), Some(&host));
    assert_eq!(
        registry.host_type_for_configuration("test.provider", "test.host"),
        Some(&host)
    );
    assert!(
        registry
            .host_type_for_configuration("other.provider", "test.host")
            .is_none()
    );
    assert_eq!(registry.content_type("test.content"), Some(&content));
    assert_eq!(registry.tool("test.tool"), Some(&tool));
    assert!(registry.tool("missing").is_none());
    assert!(
        registry
            .host_type_for_configuration("kerminal", "ssh")
            .is_some()
    );
}

/// 身份在所有贡献类型和提供方之间全局唯一，避免跨目录歧义。
///
/// @author kongweiguang
#[test]
fn rejects_duplicate_ids_across_contribution_types() {
    let (provider, host, _, mut tool) = fixture();
    tool.id = ToolId::new(host.id.as_str()).expect("valid duplicate id");
    let mut builder = PluginRegistryBuilder::new();
    builder.register_provider(provider);
    builder.register_host_type(host);
    builder.register_tool(tool);
    assert!(
        matches!(builder.freeze(), Err(PluginRegistryError::DuplicateId(id)) if id == "test.host")
    );
}

/// 提供方与贡献共用同一身份空间，防止来源身份与视图身份发生歧义。
///
/// @author kongweiguang
#[test]
fn rejects_provider_id_reused_by_contribution() {
    let (provider, _, mut content, _) = fixture();
    content.id = ContentTypeId::new(provider.id.as_str()).expect("valid duplicate id");
    let mut builder = PluginRegistryBuilder::new();
    builder.register_provider(provider);
    builder.register_content_type(content);
    assert!(
        matches!(builder.freeze(), Err(PluginRegistryError::DuplicateId(id)) if id == "test.provider")
    );
}

/// 即便贡献 ID 不同，两个主机也不能覆盖同一个磁盘配置类型。
///
/// @author kongweiguang
#[test]
fn rejects_duplicate_host_configuration_types() {
    let (provider, mut host, _, _) = fixture();
    host.content_type = None;
    let mut other = host.clone();
    other.id = HostTypeId::new("test.other-host").expect("valid host id");
    let mut builder = PluginRegistryBuilder::new();
    builder.register_provider(provider);
    builder.register_host_type(host);
    builder.register_host_type(other);
    assert!(
        matches!(builder.freeze(), Err(PluginRegistryError::DuplicateId(id)) if id == "test.host")
    );
}

/// 冻结前集中检查来源，避免孤立贡献进入配置扫描或界面装配。
///
/// @author kongweiguang
#[test]
fn rejects_missing_provider() {
    let (_, _, mut content, _) = fixture();
    content.provider = PluginId::new("test.missing").expect("valid provider id");
    let mut builder = PluginRegistryBuilder::new();
    builder.register_content_type(content);
    assert!(
        matches!(builder.freeze(), Err(PluginRegistryError::MissingProvider { contribution, provider }) if contribution == "test.content" && provider == "test.missing")
    );
}

/// 不存在的内容引用必须显式失败，不能把主机或工具替换为其它内容。
///
/// @author kongweiguang
#[test]
fn rejects_missing_content_reference() {
    let (provider, host, _, _) = fixture();
    let mut builder = PluginRegistryBuilder::new();
    builder.register_provider(provider);
    builder.register_host_type(host);
    assert!(
        matches!(builder.freeze(), Err(PluginRegistryError::MissingContent { contribution, content }) if contribution == "test.host" && content == "test.content")
    );
}

/// 拒绝不能稳定作为持久键的身份，保证所有类型采用一致校验。
///
/// @author kongweiguang
#[test]
fn rejects_invalid_ids_and_empty_names() {
    for value in [
        "",
        " bad",
        "bad ",
        ".bad",
        "vendor/part",
        "/part",
        "vendor/",
        "vendor.plugin//part",
        "vendor.plugin/part/extra",
        "主机",
    ] {
        assert!(PluginId::new(value).is_err(), "provider: {value}");
        assert!(HostTypeId::new(value).is_err(), "host: {value}");
        assert!(ContentTypeId::new(value).is_err(), "content: {value}");
        assert!(ToolId::new(value).is_err(), "tool: {value}");
    }
    for value in ["vendor.plugin/panel", "vendor.plugin-/panel"] {
        assert_eq!(
            HostTypeId::new(value).expect("namespaced host").as_str(),
            value
        );
        assert_eq!(ToolId::new(value).expect("namespaced tool").as_str(), value);
    }
    let (mut provider, _, _, _) = fixture();
    provider.name = " ".into();
    let mut builder = PluginRegistryBuilder::new();
    builder.register_provider(provider);
    assert!(
        matches!(builder.freeze(), Err(PluginRegistryError::EmptyName(id)) if id == "test.provider")
    );
}

/// 生产内置目录覆盖现有主机键及工具键，空目录适用于无插件的测试构造。
///
/// @author kongweiguang
#[test]
fn builtin_registry_covers_existing_configuration_keys() {
    let registry = PluginRegistry::builtin().expect("valid builtins");
    assert!(
        registry
            .providers()
            .iter()
            .all(|provider| provider.default_enabled)
    );
    for kind in ["local", "ssh", "sftp", "telnet", "serial", "rdp"] {
        assert!(
            registry
                .host_type_for_configuration("kerminal", kind)
                .is_some()
        );
    }
    for id in [
        "agentLauncher",
        "sftp",
        "snippets",
        "tmux",
        "ports",
        "system",
        "logs",
        "containers",
        "context",
    ] {
        assert!(registry.tool(id).is_some());
    }
    assert!(PluginRegistry::default().providers().is_empty());
}
