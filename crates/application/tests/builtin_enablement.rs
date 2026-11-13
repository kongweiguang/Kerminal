//! 内置插件拆分后的启停视图与归属契约；不启动桌面或运行服务。
//!
//! @author kongweiguang

use std::collections::{BTreeMap, BTreeSet};

use kerminal_application::plugins::{
    CORE_PLUGIN_ID, ContentDescriptor, ContentTypeId, HostTypeDescriptor, HostTypeId,
    PluginDescriptor, PluginId, PluginRegistry, PluginRegistryBuilder, PluginRegistryError,
    PluginSource, ToolDescriptor, ToolId, builtin_unit_ids, is_builtin_unit, is_reserved_plugin_id,
};

/// 用显式状态表冻结生产内置声明，聚焦启用视图而不是装配细节。
///
/// @author kongweiguang
fn freeze_states(states: &[(&str, bool)]) -> Result<PluginRegistry, PluginRegistryError> {
    let mut builder = PluginRegistryBuilder::new();
    builder.register_builtins().expect("valid builtins");
    let states = states
        .iter()
        .map(|(id, enabled)| (PluginId::new(*id).expect("valid unit id"), *enabled))
        .collect::<BTreeMap<_, _>>();
    builder.freeze_with_builtin_states(&states)
}

/// 构造一个启停状态由缺省值决定的外部插件，验证声明保留与可用性分离。
///
/// @author kongweiguang
fn external_registry(default_enabled: bool) -> PluginRegistry {
    let provider = PluginDescriptor::new(
        PluginId::new("test.provider").expect("valid provider"),
        "Test provider",
        default_enabled,
    );
    let content = ContentDescriptor::new(
        ContentTypeId::new("test.content").expect("valid content"),
        provider.id.clone(),
        "Content",
    );
    let host = HostTypeDescriptor::new(
        HostTypeId::new("test.host").expect("valid host"),
        provider.id.clone(),
        "test.host",
        "Host",
        Some(content.id.clone()),
    );
    let tool = ToolDescriptor::new(
        ToolId::new("test.tool").expect("valid tool"),
        provider.id.clone(),
        "Tool",
        Some(content.id.clone()),
    );
    let mut builder = PluginRegistryBuilder::new();
    builder.register_builtins().expect("valid builtins");
    builder.register_provider(provider);
    builder.register_host_type(host);
    builder.register_content_type(content);
    builder.register_tool(tool);
    builder.freeze().expect("valid registry")
}

/// 13 个内置单元身份唯一、顺序固定，全部贡献保留原 ID 且 owner 落在单元内。
///
/// @author kongweiguang
#[test]
fn builtin_units_are_unique_and_own_all_contributions() {
    let units = builtin_unit_ids();
    assert_eq!(units.len(), 13);
    assert_eq!(units.iter().copied().collect::<BTreeSet<_>>().len(), 13);
    assert_eq!(units[0], CORE_PLUGIN_ID);
    assert!(is_builtin_unit(CORE_PLUGIN_ID));
    assert!(!is_builtin_unit("kerminal"));
    // 安装器与静态快照共用保留判定：兼容 provider 和内置单元都不能被外部身份占用。
    assert!(is_reserved_plugin_id("kerminal"));
    assert!(is_reserved_plugin_id(CORE_PLUGIN_ID));
    assert!(!is_reserved_plugin_id("acme.notes"));

    let registry = PluginRegistry::builtin().expect("valid builtins");
    // 兼容 provider 只服务于内置贡献的来源键，不作为用户可见插件出现。
    assert!(registry.plugin("kerminal").is_none());
    assert!(!registry.is_enabled("kerminal"));

    assert_eq!(registry.providers().len(), 13);
    let declared = registry
        .providers()
        .iter()
        .map(|provider| provider.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(declared.as_slice(), units);
    for provider in registry.providers() {
        assert_eq!(provider.source, PluginSource::Builtin);
        assert!(provider.default_enabled);
        assert_eq!(provider.can_disable, provider.id.as_str() != CORE_PLUGIN_ID);
        assert!(registry.is_enabled(provider.id.as_str()));
    }

    let expected_content = [
        "terminal",
        "files",
        "editor",
        "preferences",
        "commands",
        "connections",
        "agents",
        "sftpTransfer",
        "workspaceFile",
    ];
    let expected_hosts = ["local", "ssh", "sftp", "telnet", "serial", "rdp"];
    let expected_tools = [
        "agentLauncher",
        "sftp",
        "snippets",
        "tmux",
        "ports",
        "system",
        "logs",
        "containers",
        "context",
    ];

    for content in registry.content_types() {
        assert_eq!(content.provider.as_str(), "kerminal");
        assert!(is_builtin_unit(content.owner.as_str()));
        assert!(registry.plugin(content.owner.as_str()).is_some());
        assert_eq!(
            registry.owner_of_content_type(content.id.as_str()),
            Some(&content.owner)
        );
        assert!(registry.content_type_enabled(content.id.as_str()));
    }
    for host in registry.host_types() {
        assert_eq!(host.provider.as_str(), "kerminal");
        assert!(is_builtin_unit(host.owner.as_str()));
        assert_eq!(
            registry.owner_of_host_type(host.id.as_str()),
            Some(&host.owner)
        );
        assert!(registry.host_type_enabled(&host.configuration_type));
        assert!(registry.host_type_enabled_for("kerminal", &host.configuration_type));
    }
    for tool in registry.tools() {
        assert_eq!(tool.provider.as_str(), "kerminal");
        assert!(is_builtin_unit(tool.owner.as_str()));
        assert_eq!(registry.owner_of_tool(tool.id.as_str()), Some(&tool.owner));
        assert!(registry.tool_enabled(tool.id.as_str()));
    }

    let contents = registry
        .content_types()
        .iter()
        .map(|entry| entry.id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(contents, expected_content.into_iter().collect());
    let hosts = registry
        .host_types()
        .iter()
        .map(|entry| entry.configuration_type.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(hosts, expected_hosts.into_iter().collect());
    let tools = registry
        .tools()
        .iter()
        .map(|entry| entry.id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(tools, expected_tools.into_iter().collect());

    assert_eq!(
        registry
            .owner_of_content_type("sftpTransfer")
            .map(PluginId::as_str),
        Some("kerminal.sftp")
    );
    assert_eq!(
        registry.owner_of_tool("context").map(PluginId::as_str),
        Some(CORE_PLUGIN_ID)
    );
    assert_eq!(
        registry
            .owner_of_host_type("host.ssh")
            .map(PluginId::as_str),
        Some("kerminal.ssh")
    );
}

/// 关闭 SSH 只影响 SSH 入口；主机声明仍保留，已有配置可以继续解析为不可用。
///
/// @author kongweiguang
#[test]
fn disabling_ssh_hides_only_ssh_entries() {
    let registry = freeze_states(&[("kerminal.ssh", false)]).expect("valid states");
    assert!(!registry.is_enabled("kerminal.ssh"));
    assert!(!registry.host_type_enabled("ssh"));
    assert!(!registry.host_type_enabled_for("kerminal", "ssh"));
    assert!(
        registry
            .host_type_for_configuration("kerminal", "ssh")
            .is_some()
    );
    assert!(registry.host_type_enabled("local"));
    assert!(registry.host_type_enabled("telnet"));
    assert!(registry.tool_enabled("context"));
}

/// SFTP 单元同时拥有主机、内容与工具，三者在禁用时一起不可用。
///
/// @author kongweiguang
#[test]
fn disabling_sftp_hides_host_content_and_tool_together() {
    let registry = freeze_states(&[("kerminal.sftp", false)]).expect("valid states");
    assert!(!registry.content_type_enabled("sftpTransfer"));
    assert!(!registry.tool_enabled("sftp"));
    assert!(!registry.host_type_enabled("sftp"));
    assert!(
        registry
            .host_type_for_configuration("kerminal", "sftp")
            .is_some()
    );
    assert!(registry.content_type_enabled("terminal"));
    assert!(registry.tool_enabled("tmux"));
    assert!(registry.host_type_enabled("ssh"));
}

/// Agents 单元的内容与工具共享同一开关，不影响 Commands 等相邻单元。
///
/// @author kongweiguang
#[test]
fn disabling_agents_hides_content_and_launcher() {
    let registry = freeze_states(&[("kerminal.agents", false)]).expect("valid states");
    assert!(!registry.content_type_enabled("agents"));
    assert!(!registry.tool_enabled("agentLauncher"));
    assert!(registry.content_type_enabled("commands"));
    assert!(registry.tool_enabled("snippets"));
}

/// 核心单元拒绝关闭，未知 key 拒绝写入，空状态表等价于全部启用的冻结结果。
///
/// @author kongweiguang
#[test]
fn rejects_unknown_or_immutable_builtin_states() {
    assert!(matches!(
        freeze_states(&[(CORE_PLUGIN_ID, false)]),
        Err(PluginRegistryError::ImmutablePlugin(id)) if id == CORE_PLUGIN_ID
    ));
    assert!(matches!(
        freeze_states(&[("kerminal.unknown", false)]),
        Err(PluginRegistryError::UnknownPlugin(id)) if id == "kerminal.unknown"
    ));
    // 不是内置单元的外部插件身份也不能写入内置状态表。
    assert!(matches!(
        freeze_states(&[("acme.notes", true)]),
        Err(PluginRegistryError::UnknownPlugin(id)) if id == "acme.notes"
    ));

    let all = PluginRegistry::builtin().expect("valid builtins");
    let explicit = freeze_states(&[]).expect("empty states");
    for unit in builtin_unit_ids() {
        assert!(all.is_enabled(unit));
        assert!(explicit.is_enabled(unit));
    }
    for content in all.content_types() {
        assert_eq!(
            explicit.content_type_enabled(content.id.as_str()),
            all.content_type_enabled(content.id.as_str())
        );
    }
    for tool in all.tools() {
        assert_eq!(
            explicit.tool_enabled(tool.id.as_str()),
            all.tool_enabled(tool.id.as_str())
        );
    }
    for host in all.host_types() {
        assert_eq!(
            explicit.host_type_enabled(&host.configuration_type),
            all.host_type_enabled(&host.configuration_type)
        );
    }
    // 不含前导作用域的未知身份不会被误判为可用。
    assert!(!explicit.is_enabled("missing"));
    assert!(!explicit.content_type_enabled("missing"));
    assert!(!explicit.tool_enabled("missing"));
    assert!(!explicit.host_type_enabled("missing"));
    assert!(!explicit.host_type_enabled_for("kerminal", "missing"));
    assert!(explicit.owner_of_content_type("missing").is_none());
    assert!(explicit.owner_of_tool("missing").is_none());
    assert!(explicit.owner_of_host_type("missing").is_none());
}

/// 外部插件缺省停用仍完整注册声明，启用只改变可用视图，不重建身份。
///
/// @author kongweiguang
#[test]
fn external_provider_enablement_follows_default_without_dropping_declarations() {
    let disabled = external_registry(false);
    assert_eq!(
        disabled.plugin("test.provider").map(|entry| entry.source),
        Some(PluginSource::External)
    );
    assert!(!disabled.is_enabled("test.provider"));
    assert!(!disabled.content_type_enabled("test.content"));
    assert!(disabled.content_type("test.content").is_some());
    assert!(!disabled.tool_enabled("test.tool"));
    assert!(disabled.tool("test.tool").is_some());
    assert!(!disabled.host_type_enabled("test.host"));
    assert!(
        disabled
            .host_type_for_configuration("test.provider", "test.host")
            .is_some()
    );
    assert!(disabled.is_enabled(CORE_PLUGIN_ID));
    assert!(disabled.tool_enabled("context"));

    let enabled = external_registry(true);
    assert!(enabled.is_enabled("test.provider"));
    assert!(enabled.content_type_enabled("test.content"));
    assert!(enabled.tool_enabled("test.tool"));
    assert!(enabled.host_type_enabled("test.host"));
    assert!(enabled.host_type_enabled_for("test.provider", "test.host"));
}
