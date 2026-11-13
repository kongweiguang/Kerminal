//! 随应用编译的内置贡献声明，与现有配置键和 UI 入口保持稳定映射。
//!
//! @author kongweiguang

use super::{
    BUILTIN_PROVIDER_ID, CORE_PLUGIN_ID, ContentDescriptor, ContentTypeId, HostTypeDescriptor,
    HostTypeId, PluginDescriptor, PluginId, PluginRegistryBuilder, PluginRegistryError,
    ToolDescriptor, ToolId,
};

/// 内置启停单元的固定顺序与可关闭性；顺序稳定便于测试和后续扩展页枚举。
///
/// @author kongweiguang
const UNITS: &[(&str, &str, bool)] = &[
    (CORE_PLUGIN_ID, "Core", false),
    ("kerminal.ssh", "SSH", true),
    ("kerminal.sftp", "SFTP", true),
    ("kerminal.telnet", "Telnet", true),
    ("kerminal.serial", "Serial", true),
    ("kerminal.rdp", "RDP", true),
    ("kerminal.agents", "Agents", true),
    ("kerminal.commands", "Commands", true),
    ("kerminal.tmux", "tmux", true),
    ("kerminal.containers", "Containers", true),
    ("kerminal.ports", "Port forwarding", true),
    ("kerminal.system", "System", true),
    ("kerminal.logs", "Logs", true),
];

/// 这些声明只描述已有实现；服务、codec 和 GPUI 工厂分别由组合根装配。
/// 贡献 ID 与 `kerminal` provider 保持原值以兼容主机文件，启停归属改由 owner 表达。
///
/// @author kongweiguang
pub(super) fn register(builder: &mut PluginRegistryBuilder) -> Result<(), PluginRegistryError> {
    // 兼容 provider 不作为插件注册：它不是启停单元，也不是用户可见的插件条目，
    // 仅作为内置贡献的来源键继续服务于主机 codec 与持久格式查询。
    let provider = PluginId::new(BUILTIN_PROVIDER_ID)?;

    for (id, name, can_disable) in UNITS {
        builder.register_provider(PluginDescriptor::builtin(
            PluginId::new(*id)?,
            *name,
            *can_disable,
        ));
    }

    for (id, name, owner) in [
        ("terminal", "Terminal", CORE_PLUGIN_ID),
        ("files", "Files", CORE_PLUGIN_ID),
        ("editor", "Editor", CORE_PLUGIN_ID),
        ("preferences", "Preferences", CORE_PLUGIN_ID),
        ("commands", "Commands", "kerminal.commands"),
        ("connections", "Connections", CORE_PLUGIN_ID),
        ("agents", "Agents", "kerminal.agents"),
        ("sftpTransfer", "SFTP transfer", "kerminal.sftp"),
        ("workspaceFile", "Workspace file", CORE_PLUGIN_ID),
    ] {
        builder.register_content_type(
            ContentDescriptor::new(ContentTypeId::new(id)?, provider.clone(), name)
                .owned_by(PluginId::new(owner)?),
        );
    }

    for (kind, name, content, owner) in [
        ("local", "Local", Some("terminal"), CORE_PLUGIN_ID),
        ("ssh", "SSH", Some("terminal"), "kerminal.ssh"),
        ("sftp", "SFTP", Some("files"), "kerminal.sftp"),
        ("telnet", "Telnet", Some("terminal"), "kerminal.telnet"),
        ("serial", "Serial", Some("terminal"), "kerminal.serial"),
        ("rdp", "RDP", None, "kerminal.rdp"),
    ] {
        builder.register_host_type(
            HostTypeDescriptor::new(
                HostTypeId::new(format!("host.{kind}"))?,
                provider.clone(),
                kind,
                name,
                content.map(ContentTypeId::new).transpose()?,
            )
            .owned_by(PluginId::new(owner)?),
        );
    }

    for (id, name, owner) in [
        ("agentLauncher", "Agent", "kerminal.agents"),
        ("sftp", "SFTP", "kerminal.sftp"),
        ("snippets", "Snippets", "kerminal.commands"),
        ("tmux", "tmux", "kerminal.tmux"),
        ("ports", "Port forwarding", "kerminal.ports"),
        ("system", "System", "kerminal.system"),
        ("logs", "Logs", "kerminal.logs"),
        ("containers", "Containers", "kerminal.containers"),
        ("context", "Context", CORE_PLUGIN_ID),
    ] {
        builder.register_tool(
            ToolDescriptor::new(ToolId::new(id)?, provider.clone(), name, None)
                .owned_by(PluginId::new(owner)?),
        );
    }
    Ok(())
}
