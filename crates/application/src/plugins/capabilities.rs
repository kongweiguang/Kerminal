//! MCP 运行态工具与内置启停单元的静态能力映射。
//!
//! MCP 目录可见性、`tools/call` 整族门禁和请求级目标门禁共用这一份表：
//! 若各工具适配器自行维护映射，禁用 SSH 后仍可能借其它工具族创建连接或传输。
//! 表中的空集合表示核心能力恒可用，请求级约束仍由调用方按真实目标追加。
//!
//! @author kongweiguang

use kerminal_domain::connections::RemoteProtocol;

use super::PluginRegistry;

/// 内置启停单元 ID 集中声明，避免字符串常量散落到每个工具适配器。
///
/// @author kongweiguang
pub const SSH_UNIT: &str = "kerminal.ssh";
/// SFTP 传输独立启停，避免关闭 SSH shell 时连带关闭纯文件主机。
///
/// @author kongweiguang
pub const SFTP_UNIT: &str = "kerminal.sftp";
/// Telnet 会话目标的启停归属。
///
/// @author kongweiguang
pub const TELNET_UNIT: &str = "kerminal.telnet";
/// 串口会话目标的启停归属。
///
/// @author kongweiguang
pub const SERIAL_UNIT: &str = "kerminal.serial";
/// 远程桌面目标的启停归属。
///
/// @author kongweiguang
pub const RDP_UNIT: &str = "kerminal.rdp";
/// Agent 生命周期与目标上下文工具的启停归属。
///
/// @author kongweiguang
pub const AGENTS_UNIT: &str = "kerminal.agents";
/// 片段、工作流与命令历史来源的启停归属。
///
/// @author kongweiguang
pub const COMMANDS_UNIT: &str = "kerminal.commands";
/// 端口转发编排的启停归属；实际转发仍要求目标 SSH 连接。
///
/// @author kongweiguang
pub const PORTS_UNIT: &str = "kerminal.ports";
/// 命令历史查询工具的启停归属。
///
/// @author kongweiguang
pub const LOGS_UNIT: &str = "kerminal.logs";

/// 工具整族依赖的启停单元；空表表示核心能力恒可用。
///
/// 整族禁用后工具必须从 `tools/list` 隐藏，已知道的调用返回 `plugin_disabled`，
/// 不能退化为 METHOD_NOT_FOUND，否则调用方无法区分“工具不存在”和“能力被关闭”。
///
/// @author kongweiguang
pub fn tool_units(tool: &str) -> &'static [&'static str] {
    match tool {
        "kerminal.agent.target_context" => &[AGENTS_UNIT],
        "ssh.command" => &[SSH_UNIT],
        "sftp.transfer.enqueue"
        | "sftp.transfer.list"
        | "sftp.transfer.cancel"
        | "sftp.transfer.retry" => &[SFTP_UNIT],
        "port_forward.create" | "port_forward.list" | "port_forward.close" => {
            &[PORTS_UNIT, SSH_UNIT]
        }
        "history.search" => &[LOGS_UNIT],
        _ => &[],
    }
}

/// 当前 MCP 目录实际暴露的 21 个工具；与冻结目录的契约测试据此发现两份名单漂移。
///
/// 名单仍是 `tool_units` 的唯一“已知工具”来源：对未知名称返回空表与“已知”必须分开，
/// 否则拼写错误会被当成无插件依赖的核心工具继续分派。
///
/// @author kongweiguang
pub const KNOWN_TOOLS: &[&str] = &[
    "kerminal.help",
    "kerminal.runtime_snapshot",
    "kerminal.config.validate",
    "terminal.create",
    "terminal.write",
    "terminal.snapshot",
    "terminal.resize",
    "terminal.list",
    "terminal.close",
    "kerminal.agent.target_context",
    "kerminal.host.upsert_with_credential",
    "kerminal.vault.encrypt_secret",
    "ssh.command",
    "sftp.transfer.enqueue",
    "sftp.transfer.list",
    "sftp.transfer.cancel",
    "sftp.transfer.retry",
    "port_forward.create",
    "port_forward.list",
    "port_forward.close",
    "history.search",
];

/// 暴露目录工具名单，供契约测试与诊断直接比较，不再复制第三份字面量。
///
/// @author kongweiguang
pub fn known_tools() -> &'static [&'static str] {
    KNOWN_TOOLS
}

/// 判断工具是否属于当前 MCP 目录的已知工具。
///
/// `tool_units` 对未知工具也返回空表，因此目录内核心工具与未知名称必须分开判定；
/// 调用方先确认“已知”，再依据启用视图决定是否返回 `plugin_disabled`。
///
/// @author kongweiguang
pub fn is_known_tool(tool: &str) -> bool {
    KNOWN_TOOLS.contains(&tool)
}

/// 判断工具整族是否可用；空表恒为 true，未知工具返回 false。
///
/// 未知工具必须返回 false，避免调用方把拼写错误当成全部启用；
/// 调用方仍需先用 `is_known_tool` 或 MCP 目录区分协议级未知与能力禁用。
///
/// @author kongweiguang
pub fn tool_enabled(registry: &PluginRegistry, tool: &str) -> bool {
    is_known_tool(tool)
        && tool_units(tool)
            .iter()
            .all(|unit| registry.is_enabled(unit))
}

/// 凭据加密的发现可见性候选族：其凭据种类只可能是 SSH 或 RDP。
///
/// 可用性按 any-of 判定，而不是把两个单元都写成硬依赖；否则关闭 SSH 后
/// 仅剩 RDP 时，仍能写入 RDP 凭据的工具会从目录消失。
const VAULT_ENCRYPT_VISIBILITY_UNITS: &[&str] = &[SSH_UNIT, RDP_UNIT];

/// 带凭据主机保存的发现可见性候选族：SSH、SFTP 与 RDP 主机都需要凭据。
///
/// 该工具创建的主机协议由请求或既有主机决定，目录层无法预知目标协议，
/// 因此任一族可用就保留入口，真正的拒绝留给请求级的协议门禁。
const HOST_UPSERT_VISIBILITY_UNITS: &[&str] = &[SSH_UNIT, SFTP_UNIT, RDP_UNIT];

/// 返回按“任一候选族启用即可见”判定的候选单元；普通工具返回 None。
///
/// @author kongweiguang
fn any_of_units(tool: &str) -> Option<&'static [&'static str]> {
    match tool {
        "kerminal.vault.encrypt_secret" => Some(VAULT_ENCRYPT_VISIBILITY_UNITS),
        "kerminal.host.upsert_with_credential" => Some(HOST_UPSERT_VISIBILITY_UNITS),
        _ => None,
    }
}

/// 判断工具是否应出现在当前目录（`tools/list`、帮助与实体清单同一视图）。
///
/// 普通工具要求 `tool_units` 中的单元全部启用；凭据工具按 any-of 判定，
/// 因为它们在目录层无法知道调用方接下来选择哪种协议或凭据种类。
/// 请求级门禁仍按真实目标拒绝，避免“目录可见”被误解成“任意协议都可执行”。
///
/// @author kongweiguang
pub fn tool_visible_for(registry: &PluginRegistry, tool: &str) -> bool {
    if !is_known_tool(tool) {
        return false;
    }
    match any_of_units(tool) {
        Some(units) => units.iter().any(|unit| registry.is_enabled(unit)),
        None => tool_enabled(registry, tool),
    }
}

/// 为不可见工具生成 `plugin_disabled` 所需的 required/disabled 单元视图。
///
/// 可见工具与未知工具返回 None：未知名称由协议层报 METHOD_NOT_FOUND，
/// 不能伪造一份空依赖让调用方误以为能力被关闭。any-of 工具在全部候选禁用时
/// 把候选同时作为 requiredUnits 与 disabledUnits，解释“任一族启用就可用”的语义。
///
/// @author kongweiguang
pub fn tool_visibility_gate(
    registry: &PluginRegistry,
    tool: &str,
) -> Option<(&'static [&'static str], Vec<&'static str>)> {
    if !is_known_tool(tool) {
        return None;
    }
    if let Some(candidates) = any_of_units(tool) {
        if candidates.iter().any(|unit| registry.is_enabled(unit)) {
            return None;
        }
        return Some((candidates, candidates.to_vec()));
    }
    let required_units = tool_units(tool);
    let disabled_units = required_units
        .iter()
        .copied()
        .filter(|unit| !registry.is_enabled(unit))
        .collect::<Vec<_>>();
    (!disabled_units.is_empty()).then_some((required_units, disabled_units))
}

/// 主机协议依赖的启停单元；本地及未知协议返回空表，保持本地能力不受远程插件影响。
///
/// @author kongweiguang
pub fn host_protocol_units(protocol: &str) -> &'static [&'static str] {
    match protocol.trim().to_ascii_lowercase().as_str() {
        "ssh" => &[SSH_UNIT],
        "sftp" => &[SFTP_UNIT],
        "telnet" => &[TELNET_UNIT],
        "serial" => &[SERIAL_UNIT],
        "rdp" => &[RDP_UNIT],
        _ => &[],
    }
}

/// 按协议判断远程目标是否可用；未知或本地协议不受远程插件限制。
///
/// @author kongweiguang
pub fn host_protocol_enabled(registry: &PluginRegistry, protocol: &str) -> bool {
    host_protocol_units(protocol)
        .iter()
        .all(|unit| registry.is_enabled(unit))
}

/// 凭据种类依赖的启停单元；跳跃主机仍沿用 SSH 凭据能力。
///
/// @author kongweiguang
pub fn credential_kind_units(kind: &str) -> &'static [&'static str] {
    match kind.trim().to_ascii_lowercase().as_str() {
        "ssh-host" | "jump-host" => &[SSH_UNIT],
        "rdp-host" => &[RDP_UNIT],
        _ => &[],
    }
}

/// 历史来源依赖的启停单元；片段与工作流由 commands 单元提供。
///
/// MCP wire 使用单数 `snippet`，同时兼容配置目录名 `snippets` 与 `workflows`，
/// 避免上层从文件名直接传值时绕过请求级门禁。
///
/// @author kongweiguang
pub fn history_source_units(source: &str) -> &'static [&'static str] {
    match source.trim().to_ascii_lowercase().as_str() {
        "snippet" | "snippets" | "workflows" => &[COMMANDS_UNIT],
        _ => &[],
    }
}

/// 把领域协议映射成与主机配置一致的 wire token，供门禁查询与诊断复用。
///
/// @author kongweiguang
pub fn remote_protocol_token(protocol: RemoteProtocol) -> &'static str {
    match protocol {
        RemoteProtocol::Ssh => "ssh",
        RemoteProtocol::Sftp => "sftp",
        RemoteProtocol::Telnet => "telnet",
        RemoteProtocol::Serial => "serial",
        RemoteProtocol::Rdp => "rdp",
    }
}
