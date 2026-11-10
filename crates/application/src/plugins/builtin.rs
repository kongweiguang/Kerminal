//! 随应用编译的内置贡献声明，与现有配置键和 UI 入口保持稳定映射。
//!
//! @author kongweiguang

use super::{
    ContentDescriptor, ContentTypeId, HostTypeDescriptor, HostTypeId, PluginDescriptor, PluginId,
    PluginRegistryBuilder, PluginRegistryError, ToolDescriptor, ToolId,
};

/// 这些声明只描述已有实现；服务、codec 和 GPUI 工厂分别由组合根装配。
///
/// @author kongweiguang
pub(super) fn register(builder: &mut PluginRegistryBuilder) -> Result<(), PluginRegistryError> {
    let provider = PluginId::new("kerminal")?;
    builder.register_provider(PluginDescriptor {
        id: provider.clone(),
        name: "Kerminal".into(),
        default_enabled: true,
    });

    for (id, name) in [
        ("terminal", "Terminal"),
        ("files", "Files"),
        ("editor", "Editor"),
        ("preferences", "Preferences"),
        ("commands", "Commands"),
        ("connections", "Connections"),
        ("agents", "Agents"),
        ("sftpTransfer", "SFTP transfer"),
        ("workspaceFile", "Workspace file"),
    ] {
        builder.register_content_type(ContentDescriptor {
            id: ContentTypeId::new(id)?,
            provider: provider.clone(),
            name: name.into(),
        });
    }

    for (kind, name, content) in [
        ("local", "Local", Some("terminal")),
        ("ssh", "SSH", Some("terminal")),
        ("sftp", "SFTP", Some("files")),
        ("telnet", "Telnet", Some("terminal")),
        ("serial", "Serial", Some("terminal")),
        ("rdp", "RDP", None),
    ] {
        builder.register_host_type(HostTypeDescriptor {
            id: HostTypeId::new(format!("host.{kind}"))?,
            provider: provider.clone(),
            configuration_type: kind.into(),
            name: name.into(),
            content_type: content.map(ContentTypeId::new).transpose()?,
        });
    }

    for (id, name) in [
        ("agentLauncher", "Agent"),
        ("sftp", "SFTP"),
        ("snippets", "Snippets"),
        ("tmux", "tmux"),
        ("ports", "Port forwarding"),
        ("system", "System"),
        ("logs", "Logs"),
        ("containers", "Containers"),
        ("context", "Context"),
    ] {
        builder.register_tool(ToolDescriptor {
            id: ToolId::new(id)?,
            provider: provider.clone(),
            name: name.into(),
            content_type: None,
        });
    }
    Ok(())
}
