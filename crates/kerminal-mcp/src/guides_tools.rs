//! MCP 自发现、配置与任务导航工具的只读实现。
//!
//! @author kongweiguang

use std::collections::BTreeSet;

use kerminal_application::{
    AppError, plugins::capabilities, preferences::CONFIG_GUIDE, sessions::SessionFilter,
};
use kerminal_domain::sessions::SessionInfo;
use rmcp::model::CallToolResult;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    McpServices,
    protocol::tool_catalog_entries,
    terminal_tools::{failure, session_summary, success},
};

const ABSENT_TOOL_FAMILIES: &[&str] = &[
    "settings.*",
    "external_launch.*",
    "profile.*",
    "remote_host.*",
    "snippet.*",
    "workflow.*",
    "workspace.*",
    "terminal.resolve_current",
    "history.record",
    "history.delete",
    "history.clear",
    "sftp.list",
    "sftp.preview",
    "sftp.rename",
    "sftp.delete",
    "sftp.create_directory",
    "sftp.chmod",
    "container.*",
    "tmux.*",
    "server_info.snapshot",
    "terminal.log.*",
    "terminal.reconnect",
    "diagnostics.*",
    "pending/confirm/approval/audit queues",
];

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ToolHelpArgs {
    tool_id: Option<String>,
    family: Option<String>,
    query: Option<String>,
    include_schemas: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OperationGuideArgs {
    intent: Option<String>,
    goal: Option<String>,
}

/// 按固定 catalog 分派只读发现用例；业务状态只从已装配的 application service 获取。
pub(crate) async fn dispatch(
    tool_id: &str,
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("MCP discovery request was cancelled"));
    }
    match tool_id {
        "kerminal.help" => help(services, arguments),
        "kerminal.runtime_snapshot" => runtime_snapshot(services, arguments, cancellation).await,
        _ => failure(AppError::invalid("Unknown discovery tool")),
    }
}

/// 一个只读入口按主题返回已有帮助；参数仍由对应主题 DTO 严格校验，不能触发执行。
fn help(services: &McpServices, mut arguments: Map<String, Value>) -> CallToolResult {
    let inferred = if ["toolId", "family", "query", "includeSchemas"]
        .iter()
        .any(|key| arguments.contains_key(*key))
    {
        "tools"
    } else if arguments.contains_key("intent") || arguments.contains_key("goal") {
        "operation"
    } else {
        "overview"
    };
    let topic = arguments.remove("topic").unwrap_or_else(|| json!(inferred));
    match topic.as_str() {
        Some("overview") => capabilities(services, arguments),
        Some("app") => app_guide(services, arguments),
        Some("config") => config_guide(arguments),
        Some("tools") => tool_help(services, arguments),
        Some("operation") => operation_guide(services, arguments),
        _ => failure(AppError::invalid("Unknown Kerminal help topic")),
    }
}

/// 未注入 registry 的离线装配按全部启用处理，与协议层目录判定保持同一缺省语义。
///
/// @author kongweiguang
fn unit_enabled(services: &McpServices, unit: &str) -> bool {
    services
        .plugins
        .as_deref()
        .is_none_or(|registry| registry.is_enabled(unit))
}

/// 帮助中的工具族可用性按整族判定；credentials 只要 SSH 或 RDP 任一可用就保留，
/// 避免仅关闭 SSH 时把仍可用于 RDP 的凭据入口一起隐藏。
///
/// @author kongweiguang
fn family_enabled(services: &McpServices, family: &str) -> bool {
    match family {
        "ssh" => unit_enabled(services, capabilities::SSH_UNIT),
        "credentials" => {
            unit_enabled(services, capabilities::SSH_UNIT)
                || unit_enabled(services, capabilities::RDP_UNIT)
        }
        "agent-session" => unit_enabled(services, capabilities::AGENTS_UNIT),
        "sftp" => unit_enabled(services, capabilities::SFTP_UNIT),
        "port-forward" => {
            unit_enabled(services, capabilities::PORTS_UNIT)
                && unit_enabled(services, capabilities::SSH_UNIT)
        }
        "history" => unit_enabled(services, capabilities::LOGS_UNIT),
        "containers" => unit_enabled(services, "kerminal.containers"),
        // terminal、config 和 discovery 属于核心能力，不随内置单元关闭。
        _ => true,
    }
}

/// 帮助推荐与目录共用同一可见性判定，避免凭据工具在任一候选协议启用时
/// 于 `tools/list` 可见、却在操作指南里被整族规则误删。
///
/// @author kongweiguang
fn guide_tool_enabled(services: &McpServices, tool_id: &str) -> bool {
    services
        .plugins
        .as_deref()
        .is_none_or(|registry| capabilities::tool_visible_for(registry, tool_id))
}

/// 意图依赖的启停单元在请求前统一核对；单独检查目录可见性无法表达
/// “container 还必须打开容器族”这类跨族约束。
///
/// @author kongweiguang
fn intent_enabled(services: &McpServices, intent: &str) -> bool {
    match intent {
        "ssh-command" | "tmux" | "server-info" => unit_enabled(services, capabilities::SSH_UNIT),
        "container" => {
            unit_enabled(services, capabilities::SSH_UNIT)
                && unit_enabled(services, "kerminal.containers")
        }
        "sftp" => unit_enabled(services, capabilities::SFTP_UNIT),
        "port-forward" => {
            unit_enabled(services, capabilities::PORTS_UNIT)
                && unit_enabled(services, capabilities::SSH_UNIT)
        }
        "history" => unit_enabled(services, capabilities::LOGS_UNIT),
        // 带凭据主机保存支持 SFTP 主机，因此凭据意图按任一候选协议可用判定；
        // 具体协议拒绝仍由请求级门禁完成，不能用显示族的 SSH/RDP 口径提前隐藏。
        "credentials" => {
            unit_enabled(services, capabilities::SSH_UNIT)
                || unit_enabled(services, capabilities::SFTP_UNIT)
                || unit_enabled(services, capabilities::RDP_UNIT)
        }
        "session-terminal" => unit_enabled(services, capabilities::AGENTS_UNIT),
        _ => true,
    }
}

/// 意图的主工具至少有一个仍可用时才返回工作流；仅剩核心辅助工具时
/// 回到 overview，避免用空壳步骤冒充请求的能力仍存在。
///
/// @author kongweiguang
fn intent_has_primary_tool(intent: &str, available: &[&str]) -> bool {
    let has = |predicate: fn(&str) -> bool| available.iter().copied().any(predicate);
    match intent {
        "terminal" => has(|id| id.starts_with("terminal.")),
        "session-terminal" => available.contains(&"kerminal.agent.target_context"),
        "ssh-command" | "tmux" | "container" | "server-info" => available.contains(&"ssh.command"),
        "sftp" => has(|id| id.starts_with("sftp.transfer.")),
        "port-forward" => has(|id| id.starts_with("port_forward.")),
        "history" => available.contains(&"history.search"),
        "credentials" => available.iter().any(|id| {
            matches!(
                *id,
                "kerminal.host.upsert_with_credential" | "kerminal.vault.encrypt_secret"
            )
        }),
        "config" => available.contains(&"kerminal.config.validate"),
        _ => true,
    }
}

/// 概览从当前 endpoint 的目录选择首呼，右栏助手先读取绑定，外部调用方保留独立执行入口。
///
/// @author kongweiguang
fn capabilities(services: &McpServices, arguments: Map<String, Value>) -> CallToolResult {
    if !arguments.is_empty() {
        return failure(AppError::invalid("tool arguments"));
    }
    let catalog = tool_catalog_entries(services);
    let exposed_count = catalog.len();
    let write_ids = catalog
        .iter()
        .filter(|tool| {
            !tool["annotations"]["readOnlyHint"]
                .as_bool()
                .unwrap_or(false)
                && !tool["annotations"]["destructiveHint"]
                    .as_bool()
                    .unwrap_or(false)
        })
        .filter_map(|tool| tool["toolId"].as_str())
        .collect::<Vec<_>>();
    let destructive_ids = catalog
        .iter()
        .filter(|tool| {
            tool["annotations"]["destructiveHint"]
                .as_bool()
                .unwrap_or(false)
        })
        .filter_map(|tool| tool["toolId"].as_str())
        .collect::<Vec<_>>();
    let families = catalog
        .iter()
        .filter_map(|tool| tool["family"].as_str())
        .filter(|family| *family != "discovery" && family_enabled(services, family))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|family| {
            json!({
                "id": family,
                "toolIds": catalog.iter()
                    .filter(|tool| tool["family"] == family)
                    .filter_map(|tool| tool["toolId"].as_str())
                    .collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();
    let mut recommended_first_calls = vec!["kerminal.help", "kerminal.runtime_snapshot"];
    if catalog
        .iter()
        .any(|tool| tool["toolId"] == "kerminal.agent.target_context")
    {
        recommended_first_calls.insert(0, "kerminal.agent.target_context");
    } else if unit_enabled(services, capabilities::SSH_UNIT) {
        recommended_first_calls.push("ssh.command");
    }
    recommended_first_calls.push("terminal.create");
    // SSH 关闭后策略文案不再把 ssh.command 描述为可用手段。
    let external_mcp_default = if unit_enabled(services, capabilities::SSH_UNIT) {
        "非交互 SSH 使用 ssh.command；持久、交互或本地 shell 使用 terminal.create 与明确 sessionId。"
    } else {
        "持久、交互或本地 shell 使用 terminal.create 与明确 sessionId。"
    };

    success(
        format!("Kerminal MCP 能力指南已读取：当前暴露 {exposed_count} 个 tools。"),
        json!({
            "schemaVersion": 1,
            "purpose": "帮助外部 Agent 发现 Kerminal 运行态工具、配置边界与 host 审批职责。",
            "recommendedFirstCalls": recommended_first_calls,
            "sessionWorkspace": {
                "readFirst": ["AGENTS.md", "CLAUDE.md", "context/mcp-endpoint.json",
                    "context/target-binding.json", "context/terminal-snapshot.json", "kerminal-config.md"],
                "refreshTools": refresh_tools(services),
                "scopeRule": "Agent 会话具有全局运行态能力；targetBinding 只是内置 Agent 首选终端，不是访问权限边界。",
                "terminalWriteRule": "外部 MCP 默认后台执行。持久或交互式 shell 使用 terminal.create 并保存其返回的 sessionId；目标失效时报错，不替换为其他 Tab。",
                "terminalExecutionPolicy": {
                    "scope": "global",
                    "preferredTarget": "targetBinding",
                    "externalMcpDefault": external_mcp_default,
                    "visiblePtyFirst": "内置 Agent 先通过 target_context 的 nextHints 确认执行方式。设置开启时，绑定主机的命令通过 terminal.write 写入同一终端。设置关闭时，可使用后台 SSH 或显式创建后台 PTY。",
                    "otherTerminals": "读取 terminal.list 不会切换 UI Tab；指定目标失效时不回退。",
                    "headlessFallback": "创建后台 PTY 后，使用其明确 sessionId 执行 snapshot、write 和 close。",
                    "backgroundFallback": "后台失败不自动改为向可见终端写入。",
                    "uiTabInteraction": "仅用户明确要求操作可见 Tab 时才选择已确认的 sessionId。",
                    "reconnect": "断开的 UI pane 由界面操作重连，MCP 不提供重连入口。"
                }
            },
            "runtimeToolFamilies": families,
            "helpTopics": ["overview", "app", "config", "tools", "operation"],
            "fileFirstConfiguration": {
                "editableFiles": editable_files(),
                "manualGuide": "kerminal-config.md",
                "manualGuideTool": "kerminal.help",
                "manualGuideArguments": {"topic":"config"},
                "validator": "kerminal.config.validate",
                "secretBoundary": "普通主机 TOML 只保存 vault 引用；密钥通过受管 vault 用例保存。",
                "autoRefresh": "运行时配置由应用装配层观察文件变化；修改后仍应运行 validator。"
            },
            "deliberatelyAbsentToolFamilies": ABSENT_TOOL_FAMILIES,
            "hostPolicy": {
                "approvalOwner": "MCP host 决定确认、权限、hook 与审计策略。",
                "kerminalRole": "Kerminal 校验参数、仅绑定 loopback 服务并执行运行态用例。"
            },
            "toolCounts": {"exposed": exposed_count, "write": write_ids.len(), "destructive": destructive_ids.len()},
            "writeToolIds": write_ids,
            "destructiveToolIds": destructive_ids
        }),
        catalog
            .iter()
            .map(|tool| {
                json!({
                    "type": "mcpTool",
                    "id": tool["toolId"],
                    "category": tool["category"],
                    "categoryLabel": category_label(tool["category"].as_str().unwrap_or_default()),
                    "readOnly": tool["annotations"]["readOnlyHint"],
                    "destructive": tool["annotations"]["destructiveHint"],
                    "openWorld": tool["annotations"]["openWorldHint"]
                })
            })
            .collect(),
    )
}

/// 导航只映射现有 UI 入口；保留区域 ID 与工具族，避免界面收敛破坏外部发现契约。
/// 容器导航绑定经过校验的 SSH 宿主；Context UI 入口移除不影响 Agent 目标绑定、context 文件或后台工具调用。
/// UI 能力与 MCP 目录分别描述，避免把容器和 tmux 面板误写成可调用工具族。
///
/// @author kongweiguang
fn app_guide(services: &McpServices, arguments: Map<String, Value>) -> CallToolResult {
    if !arguments.is_empty() {
        return failure(AppError::invalid("tool arguments"));
    }
    let ssh_enabled = unit_enabled(services, capabilities::SSH_UNIT);
    let credentials_enabled = ssh_enabled || unit_enabled(services, capabilities::RDP_UNIT);
    let agents_enabled = unit_enabled(services, capabilities::AGENTS_UNIT);
    let sftp_enabled = unit_enabled(services, capabilities::SFTP_UNIT);
    let ports_enabled = unit_enabled(services, capabilities::PORTS_UNIT) && ssh_enabled;
    let history_enabled = unit_enabled(services, capabilities::LOGS_UNIT);
    let containers_enabled = unit_enabled(services, "kerminal.containers");
    let mut areas = Vec::new();
    // 区域标题只在仍有可用工具族时出现；全部族都禁用时省略区域本身，
    // 避免留下空标题或引用已关闭能力的说明。
    let host_families = [
        ssh_enabled.then_some("ssh"),
        credentials_enabled.then_some("credentials"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if !host_families.is_empty() {
        areas.push(json!({
            "id":"hosts",
            "location":"left",
            "purpose": if ssh_enabled {
                "左侧主机导航用于浏览已保存的主机与连接；SSH 工具可在后台直接调用，不要求先打开导航。"
            } else {
                "左侧主机导航用于浏览已保存的主机与连接；连接工具可在后台直接调用，不要求先打开导航。"
            },
            "toolFamilies":host_families
        }));
    }
    areas.push(json!({
        "id":"terminal-workspace",
        "location":"center",
        // SSH 单元关闭后不再把 SSH 调用描述成工作区能力，避免帮助文案指向已禁用入口。
        "purpose": if ssh_enabled {
            "中央工作区承载可见终端 Tab、分屏、文件编辑和完整文件工作区；后台终端和 SSH 调用不要求创建 UI Tab。"
        } else {
            "中央工作区承载可见终端 Tab、分屏、文件编辑和完整文件工作区；后台终端调用不要求创建 UI Tab。"
        },
        "toolFamilies":["terminal"]
    }));
    let right_families = [
        agents_enabled.then_some("agent-session"),
        sftp_enabled.then_some("sftp"),
        ssh_enabled.then_some("ssh"),
        ports_enabled.then_some("port-forward"),
        Some("discovery"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let right_base = if agents_enabled || sftp_enabled || ssh_enabled || ports_enabled {
        "右侧提供 Agent 与文件、运维工具，按需打开；"
    } else {
        "右侧提供运行态工具入口，按需打开；"
    };
    let mut right_purpose = right_base.to_owned();
    if containers_enabled && ssh_enabled {
        right_purpose.push_str(
            "容器和 tmux 面板属于 UI 能力。通过 MCP 操作 SSH 主机上的容器、tmux 和服务器信息时使用 ssh.command。",
        );
    } else if containers_enabled {
        right_purpose.push_str("容器面板属于 UI 能力。");
    } else if ssh_enabled {
        right_purpose.push_str("通过 MCP 操作 SSH 主机上的服务器信息时使用 ssh.command。");
    }
    right_purpose.push_str("工具的顺序与内容打开位置可通过工具栏右键菜单调整。");
    areas.push(json!({
        "id":"agent-and-tools",
        "location":"right",
        "purpose":right_purpose,
        "toolFamilies":right_families
    }));
    if history_enabled {
        areas.push(json!({
            "id":"bottom-tools",
            "location":"right",
            "purpose":"命令历史默认从右栏底部图标按需打开；bottom-tools 为兼容保留的区域 ID，不表示终端底部工具区。后台查询不要求打开面板。",
            "toolFamilies":["history"]
        }));
    }
    areas.push(json!({
        "id":"configuration-workspace",
        "location":"workspace",
        "purpose": if credentials_enabled {
            "配置工作区通过文件编辑与 validator 检查；连接凭据由主机连接界面的加密 vault 用例保存。"
        } else {
            "配置工作区通过文件编辑与 validator 检查。"
        },
        "toolFamilies":["config"]
    }));
    // 与 tools/list 使用同一可见集合；显示族（如 credentials）可能因展示口径隐藏，
    // 但任何仍可调用的工具都不能从 mcpToolIds 中消失。
    let tool_ids = tool_catalog_entries(services)
        .iter()
        .filter_map(|tool| tool["toolId"].as_str().map(ToOwned::to_owned))
        .collect::<Vec<_>>();
    let mut payload = json!({
        "schemaVersion": 1,
        "purpose": "帮助外部 Agent 将 Kerminal 的工作区区域映射到对应运行态工具族。",
        "areas": areas,
        "mcpToolIds": tool_ids,
        "sessionWorkspace": {
            "readFirst": ["AGENTS.md", "CLAUDE.md", "context/mcp-endpoint.json", "context/target-binding.json", "context/terminal-snapshot.json", "kerminal-config.md"],
            "refreshTools": refresh_tools(services),
            "scope": "Agent endpoint 对应一个 live Agent session；普通 Tab 切换不会更新首选目标。"
        },
        "configuration": {"guide":"kerminal.help", "guideArguments":{"topic":"config"}, "validator":"kerminal.config.validate", "editableFiles":editable_files()},
        "safetyBoundaries": {
            "approval": "MCP host 管理审批、权限、hook 与审计，Kerminal 不添加第二层确认。",
            "background": "外部 MCP 操作不要求先打开终端 Tab。",
            "targetBinding": "首选目标失效时显式报告，不回退到另一个目标。",
            "configuration": "settings/profile/host/snippet/workflow 使用文件编辑与 validator，不通过 MCP CRUD。",
            "secrets": "不读取 vault 文件；普通配置仅保存凭据引用。"
        }
    });
    // 容器面板绑定经过 SSH 宿主校验；SSH 单元关闭后不再返回该说明，
    // 避免帮助把不可用的宿主绑定继续描述成可执行能力。
    if containers_enabled && ssh_enabled {
        payload["containerPanel"] = json!({
            "uiToolId": "containers",
            "location": "right",
            "fixedPosition": false,
            "defaultAfter": "sftp",
            "customization": "在工具栏右键菜单中调整顺序、显示状态和内容打开位置。",
            "binding": "当前活动 SSH pane 或文件 Tab 的已核实宿主",
            "hostMode": "SSH pane 管理对应宿主的容器列表；本机 pane 不提供宿主容器管理。",
            "containerMode": "容器 pane 解析创建时的 SSH 宿主，仍显示该宿主的容器工具；文件 Tab 保留真实宿主身份。",
            "unavailable": "无有效目标、目标身份不一致、本机、Telnet 或串口时显示原因，不回退到左栏选中的主机。"
        });
    }
    success(
        "Kerminal 应用导航指南已读取：返回工作区区域、运行态工具和文件化配置边界。".to_owned(),
        payload,
        Vec::new(),
    )
}

/// 从 application 唯一配置指南常量返回规则正文，避免模板和 MCP 文档产生两份事实源。
fn config_guide(arguments: Map<String, Value>) -> CallToolResult {
    if !arguments.is_empty() {
        return failure(AppError::invalid("tool arguments"));
    }
    success(
        format!(
            "Kerminal 配置指南已读取：返回同源的 {} 行规则正文。",
            CONFIG_GUIDE.lines().count()
        ),
        json!({
            "schemaVersion": 1,
            "guideFile": "kerminal-config.md",
            "lineCount": CONFIG_GUIDE.lines().count(),
            "purpose": "Expose the same file-backed configuration rules generated in the Kerminal workspace. This tool is read-only and does not perform config CRUD.",
            "markdown": CONFIG_GUIDE,
            "editableFiles": editable_files(),
            "protectedPaths": ["data/command.sqlite", "secrets/vault.toml", "secrets/vault-key.toml", "logs/", "cache/", "temp/"],
            "validator": "kerminal.config.validate",
            "secretBoundary": "Do not read or edit vault files. Store credentials through authorized encrypted-vault application tools; ordinary host configuration stores secret_ref only.",
            "mcpCrudBoundary": "Configuration is file-first. Edit supported files directly and validate; settings/profile/host/snippet/workflow CRUD tools are intentionally absent.",
            "deliberatelyAbsentToolFamilies": ABSENT_TOOL_FAMILIES,
            "nextActions": [
                "Read the guide before editing file-backed configuration.",
                "Run kerminal.config.validate with the narrowest matching scope after edits.",
                "Use vault application tools for credential material; never place plaintext secrets in TOML."
            ]
        }),
        vec![
            json!({"type":"configGuide","path":"kerminal-config.md","validator":"kerminal.config.validate"}),
        ],
    )
}

/// 根据筛选条件查询当前目录的 schema，不将已移除工具描述为可调用。
///
/// @author kongweiguang
fn tool_help(services: &McpServices, arguments: Map<String, Value>) -> CallToolResult {
    let args = match crate::terminal_tools::parse_args::<ToolHelpArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let catalog = tool_catalog_entries(services);
    let requested_id = non_empty(args.tool_id);
    let requested_family = non_empty(args.family);
    let requested_query = non_empty(args.query);
    let include_schemas = args.include_schemas.unwrap_or(true);
    let (match_mode, selected) = if let Some(id) = requested_id.as_deref() {
        (
            "toolId",
            catalog
                .iter()
                .filter(|tool| tool["toolId"] == id)
                .collect::<Vec<_>>(),
        )
    } else if let Some(family) = requested_family.as_deref() {
        (
            "family",
            catalog
                .iter()
                .filter(|tool| family_matches(family, tool["family"].as_str().unwrap_or_default()))
                .collect::<Vec<_>>(),
        )
    } else if let Some(query) = requested_query.as_deref() {
        let query = query.to_lowercase();
        (
            "query",
            catalog
                .iter()
                .filter(|tool| {
                    [
                        tool["toolId"].as_str(),
                        tool["title"].as_str(),
                        tool["description"].as_str(),
                        tool["category"].as_str(),
                    ]
                    .into_iter()
                    .flatten()
                    .any(|value| value.to_lowercase().contains(&query))
                })
                .collect::<Vec<_>>(),
        )
    } else {
        (
            "default",
            catalog
                .iter()
                .filter(|tool| tool["family"] == "discovery")
                .collect::<Vec<_>>(),
        )
    };
    let matched_ids = selected
        .iter()
        .filter_map(|tool| tool["toolId"].as_str())
        .collect::<Vec<_>>();
    let tool_reference = selected
        .iter()
        .map(|tool| {
            let mut reference = (**tool).clone();
            reference["exampleArguments"] = example_arguments(&reference["inputSchema"]);
            // SSH 的目标条件是联合约束；仅生成 schema.required 会遗漏可执行请求必需的目标。
            if reference["toolId"] == "ssh.command" {
                reference["exampleArguments"]["hostId"] = json!("<saved-host-id>");
                reference["exampleArguments"]["command"] = json!("pwd");
            }
            if !include_schemas && let Some(object) = reference.as_object_mut() {
                object.remove("inputSchema");
            }
            reference
        })
        .collect::<Vec<_>>();
    let missing_ids = requested_id
        .as_deref()
        .filter(|_| selected.is_empty())
        .into_iter()
        .collect::<Vec<_>>();
    let absent = requested_id
        .as_deref()
        .or(requested_family.as_deref())
        .or(requested_query.as_deref())
        .map(matching_absent_families)
        .unwrap_or_default();
    let summary = format!(
        "Kerminal 工具帮助已读取：matchMode={match_mode}，匹配 {} 个当前工具。",
        selected.len()
    );
    success(
        summary,
        json!({
            "schemaVersion":1,
            "purpose":"Return schema-backed help for currently exposed Kerminal MCP tools without executing them.",
            "filters":{"toolId":requested_id,"family":requested_family,"query":requested_query,"includeSchemas":include_schemas},
            "matchMode":match_mode,
            "matchedToolCount":selected.len(),
            "toolReference":tool_reference,
            "availableToolIds":matched_ids,
            "missingToolIds":missing_ids,
            "absentToolMatches":absent,
            "deliberatelyAbsentToolFamilies":ABSENT_TOOL_FAMILIES,
            "fallbacks":[
                "Use kerminal.help with topic=operation when a task needs a sequence of calls.",
                "Use tools/list for the protocol-level catalog.",
                "For configuration, read kerminal.help with topic=config, edit files, then validate.",
                "For file transfer, inspect the sftp.transfer tools; server-side file commands use ssh.command.",
                "If a write result is outcome unknown because its acknowledgement was lost, inspect the affected resource before any retry."
            ],
            "safetyBoundaries":{
                "readOnly":"kerminal.help never invokes the referenced tool.",
                "hostPolicy":"The MCP host owns confirmations, permissions, hooks and audit.",
                "fileFirstConfiguration":"Configuration CRUD tools are intentionally absent.",
                "terminalExecution":"Use background tools by default; explicit sessionId selects a session, and invalid targets are never substituted.",
                "outcomeUnknown":"A lost acknowledgement after a possible write does not prove that the operation failed. Read back state before retrying.",
                "serverOperations":"Use ssh.command for server-side container, tmux and file commands; the SFTP queue accepts local and remote endpoints."
            },
            "nextActions":[
                "Check the selected tool's schema and safety annotations before calling it.",
                "When a write returns outcome unknown, read back state before retrying.",
                "Host-side approval remains the sole approval policy."
            ]
        }),
        matched_ids
            .iter()
            .map(|id| json!({"type":"mcpTool","id":id}))
            .collect(),
    )
}

/// 将任务意图归一成可用工具序列，右栏助手先查询终端路由，避免把 SSH 当作固定路径。
///
/// @author kongweiguang
fn operation_guide(services: &McpServices, arguments: Map<String, Value>) -> CallToolResult {
    let args = match crate::terminal_tools::parse_args::<OperationGuideArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let requested_intent = non_empty(args.intent).unwrap_or_else(|| "overview".to_owned());
    let normalized = requested_intent
        .trim()
        .to_ascii_lowercase()
        .replace('_', "-");
    let requested = normalize_intent(&normalized);
    let mut note = (requested != normalized).then(|| {
        format!("Unsupported intent `{requested_intent}` was normalized to `{requested}`.")
    });
    let catalog = tool_catalog_entries(services);
    let calls = intent_calls(requested);
    let mut available = calls
        .iter()
        .filter(|id| catalog.iter().any(|tool| tool["toolId"] == **id))
        .filter(|id| guide_tool_enabled(services, id))
        .copied()
        .collect::<Vec<_>>();
    // 意图依赖的单元被关闭、或主工具全部不可用时，回落到 overview 并说明原因；
    // 不返回只有辅助工具的空壳流程，也不把禁用工具名写进 missing 列表。
    let mut intent = requested;
    let intent_enabled_now = intent_enabled(services, requested);
    // 会话入口只在 Agent endpoint 暴露；全局 endpoint 缺少主工具是范围问题，
    // 不能误报成“能力被禁用”，否则调用方会去扩展页徒劳启用插件。
    let endpoint_scope_excludes_primary =
        requested == "session-terminal" && intent_enabled_now && services.agent_scope.is_none();
    if !intent_enabled_now || !intent_has_primary_tool(requested, &available) {
        intent = "overview";
        note = Some(if endpoint_scope_excludes_primary {
            format!(
                "Intent `{requested_intent}` is unavailable on the global endpoint because kerminal.agent.target_context is only exposed to Agent session endpoints."
            )
        } else {
            format!(
                "Intent `{requested_intent}` is unavailable because its capabilities are disabled."
            )
        });
        available = intent_calls(intent)
            .iter()
            .filter(|id| catalog.iter().any(|tool| tool["toolId"] == **id))
            .filter(|id| guide_tool_enabled(services, id))
            .copied()
            .collect();
    }
    let resolve_agent_route = available.contains(&"ssh.command")
        && catalog
            .iter()
            .any(|tool| tool["toolId"] == "kerminal.agent.target_context");
    if resolve_agent_route {
        available.insert(0, "kerminal.agent.target_context");
    }
    let calls = intent_calls(intent);
    let missing = calls
        .iter()
        .filter(|id| !available.contains(id))
        // 被插件关闭的候选不是“目录缺席”，保留名称会形成悬空推荐。
        .filter(|id| guide_tool_enabled(services, id))
        .copied()
        .collect::<Vec<_>>();
    let workflow = workflow_steps(intent, &available, &catalog);
    let refs = catalog
        .iter()
        .filter(|tool| available.contains(&tool["toolId"].as_str().unwrap_or_default()))
        .cloned()
        .collect::<Vec<_>>();
    // 安全边界中的替代路径同样不能推荐已关闭的 SSH；本地终端说明始终保留。
    let server_operations = if resolve_agent_route {
        "On an Agent endpoint, follow the live target context hints. In visible mode use terminal.write for the bound host. Use ssh.command in background mode or on an explicitly requested different host."
    } else if unit_enabled(services, capabilities::SSH_UNIT) {
        "Use ssh.command for server-side container, tmux, system and file commands. Local shell tasks use terminal.create with an explicit sessionId."
    } else {
        "Local shell tasks use terminal.create with an explicit sessionId."
    };
    success(
        format!(
            "Kerminal 操作指南已读取：intent={intent}；返回 {} 个步骤。",
            workflow.len()
        ),
        json!({
            "schemaVersion":1,
            "intent":intent,
            "requestedIntent":requested_intent,
            "goal":args.goal,
            "note":note,
            "purpose":"为运行态 MCP 调用提供顺序和关键边界；host 仍负责审批。",
            "recommendedFirstCalls":available.iter().take(2).collect::<Vec<_>>(),
            "workflow":workflow,
            "toolReference":refs,
            "requiredContextFiles":["AGENTS.md","CLAUDE.md","context/mcp-endpoint.json","context/target-binding.json","context/terminal-snapshot.json","kerminal-config.md"],
            "fileFirstConfiguration":{"readBeforeEditing":"kerminal-config.md or kerminal.help(topic=config)","guideTool":"kerminal.help","guideArguments":{"topic":"config"},"editableFiles":editable_files(),"validator":"kerminal.config.validate"},
            "safetyBoundaries":{
                "hostPolicy":"The MCP host owns confirmation, approval, permissions, hooks, and audit.",
                "terminalWrite":"External MCP defaults to background. Use returned sessionId explicitly; do not substitute a stale target.",
                "outcomeUnknown":"If the acknowledgement to a write is lost, read back the destination before deciding whether to retry.",
                "serverOperations":server_operations,
                "secrets":"Never expose vault content or plaintext credentials."
            },
            "availableReferencedToolIds":available,
            "missingReferencedToolIds":missing,
            "deliberatelyAbsentToolFamilies":ABSENT_TOOL_FAMILIES,
            "stopConditions":["The explicitly selected target is unavailable.","The task requires a deliberately absent configuration CRUD tool.","A write outcome is unknown; inspect state before retrying."]
        }),
        available
            .iter()
            .map(|id| json!({"type":"mcpTool","id":id}))
            .collect(),
    )
}

/// 汇总 session 与 Agent / port forward 的实际应用快照；失败子系统用脱敏诊断单独标出。
async fn runtime_snapshot(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if !arguments.is_empty() {
        return failure(AppError::invalid("tool arguments"));
    }
    let mut diagnostics = Vec::new();
    let terminal_sessions = match services.sessions.list(SessionFilter::default()).await {
        Ok(sessions) if !cancellation.is_cancelled() => sessions,
        Ok(_) => return failure(AppError::cancelled("Runtime snapshot was cancelled")),
        Err(_) => {
            diagnostics.push(json!({"source":"terminal.list","code":"terminalSessionsUnavailable","status":"degraded","severity":"warning"}));
            Vec::new()
        }
    };
    let mut terminal_summaries = Vec::with_capacity(terminal_sessions.len());
    for session in &terminal_sessions {
        if cancellation.is_cancelled() {
            return failure(AppError::cancelled("Runtime snapshot was cancelled"));
        }
        terminal_summaries.push(session_summary(services, session).await);
    }
    let agent_sessions = if let Some(agents) = services.agents.as_deref() {
        match agents.list(cancellation.child_token()).await {
            Ok(sessions) => sessions.iter().map(|view| json!({
                "agentSessionId":view.session.agent_session_id.as_str(),
                "provider":view.session.agent_id.as_str(),
                "status":agent_status(view.session.status),
                "terminalStatus":view.terminal.as_ref().map(|session| session_status(session))
            })).collect::<Vec<_>>(),
            Err(_) => {
                diagnostics.push(json!({"source":"agent.sessions","code":"agentSessionsUnavailable","status":"degraded","severity":"warning"}));
                Vec::new()
            }
        }
    } else {
        diagnostics.push(json!({"source":"agent.sessions","code":"agentCapabilityUnavailable","status":"degraded","severity":"warning"}));
        Vec::new()
    };
    let forwards = if let Some(operations) = services.operations.as_ref() {
        operations
            .port_forwards
            .list()
            .await
            .into_iter()
            .map(|item| {
                json!({
                    "id": item.forward.id,
                    "status": format!("{:?}", item.forward.state).to_ascii_lowercase(),
                    "failureCode": item.failure_code.map(|code| code.as_str())
                })
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let families = tool_catalog_entries(services);
    let data = json!({
        "schemaVersion":1,
        "runtime":{"terminalSessionCount":terminal_sessions.len(),"agentSessionCount":agent_sessions.len(),"portForwardCount":forwards.len()},
        "terminalSessions":terminal_summaries,
        "agentSessions":agent_sessions,
        "portForwards":forwards,
        "toolFamilies":families.iter()
            .filter_map(|tool| tool["family"].as_str())
            .filter(|family| family_enabled(services, family))
            .collect::<BTreeSet<_>>(),
        "diagnostics":diagnostics
    });
    success(
        "Kerminal 运行态快照已读取；所有 session 状态来自实际 application services。".to_owned(),
        data,
        Vec::new(),
    )
}

/// 普通 endpoint 不推荐会话专用工具，避免帮助再次引导客户端调用隐藏入口。
/// Agent 单元关闭后，会话 endpoint 也回落到核心终端读取，不推荐已禁用的目标上下文。
fn refresh_tools(services: &McpServices) -> Vec<&'static str> {
    if services.agent_scope.is_some() && unit_enabled(services, capabilities::AGENTS_UNIT) {
        vec!["kerminal.agent.target_context"]
    } else {
        vec!["terminal.list", "terminal.snapshot"]
    }
}

/// 为每个开放的配置文件类别返回统一路径，禁止 discovery 工具创造新的配置读写通道。
fn editable_files() -> [&'static str; 6] {
    [
        "settings.toml",
        "profiles/*.toml",
        "hosts/groups.toml",
        "hosts/*.toml",
        "snippets/*.toml",
        "workflows/*.toml",
    ]
}

/// 给 schema 中的必填字段生成非可执行占位参数，避免帮助工具把示例误当真实目标。
fn example_arguments(schema: &Value) -> Value {
    let mut example = Map::new();
    if let Some(required) = schema["required"].as_array() {
        for field in required.iter().filter_map(Value::as_str) {
            if let Some(field_schema) = schema["properties"].get(field) {
                example.insert(field.to_owned(), example_value(field, field_schema));
            }
        }
    }
    Value::Object(example)
}

/// 根据 JSON Schema 类型填入不会泄露数据的提示值，敏感字段永远使用占位符。
fn example_value(name: &str, schema: &Value) -> Value {
    if is_sensitive_field(name) {
        return Value::String("<redacted>".to_owned());
    }
    if let Some(values) = schema["enum"].as_array() {
        return values.first().cloned().unwrap_or(Value::Null);
    }
    match schema["type"].as_str() {
        Some("string") => Value::String(format!("<{}>", name)),
        Some("integer") | Some("number") => json!(1),
        Some("boolean") => Value::Bool(false),
        Some("array") => Value::Array(Vec::new()),
        Some("object") => json!({}),
        _ => Value::Null,
    }
}

/// 识别凭据字段，防止从 schema 生成器把敏感信息误当作普通输入示例。
fn is_sensitive_field(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "password",
        "secret",
        "token",
        "privatekey",
        "passphrase",
        "plaintext",
    ]
    .iter()
    .any(|marker| name.contains(marker))
}

/// 匹配仍受支持的帮助族别名，且始终按当前 catalog 顺序返回结果。
fn family_matches(requested: &str, family: &str) -> bool {
    let requested = requested.trim().to_ascii_lowercase().replace('_', "-");
    match requested.as_str() {
        "guide" | "guides" => family == "discovery",
        "agent" | "session" => family == "agent-session",
        "ssh-command" => family == "ssh",
        "port" | "port-forwarding" => family == "port-forward",
        "credential" | "vault" | "secret" => family == "credentials",
        _ => requested == family,
    }
}

/// 将用户筛选与已移除族名统一归一化，避免下划线或连字符差异掩盖缺席能力。
fn matching_absent_families(filter: &str) -> Vec<&'static str> {
    let normalized = filter.trim().to_ascii_lowercase().replace('_', "-");
    ABSENT_TOOL_FAMILIES
        .iter()
        .copied()
        .filter(|family| {
            let family = family.to_ascii_lowercase().replace('_', "-");
            normalized.contains(family.trim_end_matches('*').trim_end_matches('.'))
                || family.contains(&normalized)
        })
        .collect()
}

/// 归一化已支持的 intent；不支持的值回落到 overview 并在响应中保留原值和 note。
fn normalize_intent(intent: &str) -> &'static str {
    match intent {
        "terminal" | "session-terminal" | "ssh-command" | "sftp" | "tmux" | "container"
        | "port-forward" | "server-info" | "history" | "config" | "credentials" | "diagnostics" => {
            match intent {
                "terminal" => "terminal",
                "session-terminal" => "session-terminal",
                "ssh-command" => "ssh-command",
                "sftp" => "sftp",
                "tmux" => "tmux",
                "container" => "container",
                "port-forward" => "port-forward",
                "server-info" => "server-info",
                "history" => "history",
                "config" => "config",
                "credentials" => "credentials",
                _ => "diagnostics",
            }
        }
        _ => "overview",
    }
}

/// 按 intent 提供一组真实 catalog ids；响应会再按当前 catalog 过滤，避免过期引用。
fn intent_calls(intent: &str) -> Vec<&'static str> {
    match intent {
        "terminal" => vec![
            "terminal.create",
            "terminal.snapshot",
            "terminal.write",
            "terminal.close",
        ],
        "session-terminal" => vec!["kerminal.agent.target_context", "terminal.snapshot"],
        "ssh-command" => vec!["kerminal.runtime_snapshot", "ssh.command"],
        "sftp" => vec![
            "sftp.transfer.enqueue",
            "sftp.transfer.list",
            "sftp.transfer.retry",
            "sftp.transfer.cancel",
        ],
        "tmux" | "container" | "server-info" => vec!["ssh.command"],
        "port-forward" => vec![
            "kerminal.runtime_snapshot",
            "port_forward.list",
            "port_forward.create",
            "port_forward.close",
        ],
        "history" => vec!["history.search"],
        "config" => vec!["kerminal.help", "kerminal.config.validate"],
        "credentials" => vec![
            "kerminal.help",
            "kerminal.host.upsert_with_credential",
            "kerminal.vault.encrypt_secret",
        ],
        "diagnostics" => vec!["kerminal.runtime_snapshot"],
        _ => vec!["kerminal.help", "kerminal.runtime_snapshot"],
    }
}

/// 为工具序列生成简洁工作步骤，并在缺少真实可用工具时不伪称成功路径存在。
fn workflow_steps(intent: &str, tool_ids: &[&str], catalog: &[Value]) -> Vec<Value> {
    tool_ids.iter().enumerate().map(|(index, tool_id)| json!({
        "id":format!("step-{}", index + 1),
        "toolId":tool_id,
        "description":step_description(intent, tool_id),
        "requiredArguments":required_arguments(tool_id, catalog),
        "safety":"Use only the explicitly selected target. If a write outcome is unknown, inspect state before retrying."
    })).collect()
}

/// 只提供固定步骤说明，右栏路由由 live 提示决定，避免自由文本干预目标选择。
///
/// @author kongweiguang
fn step_description(intent: &str, tool_id: &str) -> &'static str {
    match tool_id {
        "terminal.create" => "创建一个无需 UI Tab 的后台终端并记录返回 sessionId。",
        "terminal.write" => "只向显式 sessionId 写入已授权的终端输入。",
        "terminal.snapshot" => "读取已选 session 的有界文本快照。",
        "terminal.close" => "完成任务后关闭同一 sessionId 创建的后台终端。",
        "kerminal.agent.target_context" => "核对绑定目标并按 nextHints 选择执行路径。",
        "ssh.command" if intent == "container" => {
            "通过保存的 SSH 目标执行容器运行时命令；文件写入结果未知时先回读。"
        }
        "ssh.command" if intent == "tmux" => "通过保存的 SSH 目标执行 tmux 命令。",
        "ssh.command" if intent == "server-info" => "通过保存的 SSH 目标执行服务器信息查询命令。",
        "ssh.command" => "通过保存的 SSH 目标执行一次非交互命令。",
        "sftp.transfer.enqueue" => "提交传输任务；请求受理不代表文件已传输完成。",
        "sftp.transfer.list" => {
            "按 transferId 查询直到终态；failureKind=commitUnknown 时先检查目标，不要自动重试。"
        }
        "sftp.transfer.retry" => {
            "仅在查询明确返回 retryable=true 时重试，并继续查询返回的后继 transferId。"
        }
        "sftp.transfer.cancel" => {
            "取消请求受理不代表任务已停止；继续查询同一 transferId 直到终态。"
        }
        "kerminal.config.validate" => "检查文件化配置并处理返回的错误或警告。",
        "history.search" => "只读查询与当前任务相关的历史命令。",
        _ if intent == "overview" => "先确认当前运行态和工具能力，再继续具体操作。",
        _ => "调用对应的只读或运行态工具，并核对目标与返回状态。",
    }
}

/// 从本次请求的同一目录快照读取必填项，避免流程与帮助使用不同的暴露集合。
fn required_arguments(tool_id: &str, catalog: &[Value]) -> Vec<String> {
    catalog
        .iter()
        .find(|tool| tool["toolId"] == tool_id)
        .and_then(|tool| tool["inputSchema"]["required"].as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// 生成各类应用领域可读类别名，保留 catalog 的原始 category 字段。
fn category_label(category: &str) -> &'static str {
    match category {
        "diagnostics" => "诊断与发现",
        "history" => "命令历史",
        "portForward" => "端口转发",
        "sftp" => "文件传输",
        "ssh" => "SSH 命令",
        "terminal" => "终端",
        _ => "运行态工具",
    }
}

/// 去掉空字符串查询项；优先级由 toolHelp handler 固定为 toolId、family、query。
fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// 以 MCP wire 字段返回 Agent 生命周期枚举，避免领域状态直接序列化。
fn agent_status(status: kerminal_domain::agents::AgentSessionStatus) -> &'static str {
    match status {
        kerminal_domain::agents::AgentSessionStatus::Active => "active",
        kerminal_domain::agents::AgentSessionStatus::Archived => "archived",
        kerminal_domain::agents::AgentSessionStatus::Stale => "stale",
    }
}

/// 以 MCP wire 字段返回 terminal lifecycle，不暴露 PTY runtime 内部细节。
fn session_status(session: &SessionInfo) -> &'static str {
    use kerminal_domain::sessions::SessionLifecycle;
    match session.lifecycle {
        SessionLifecycle::Starting => "starting",
        SessionLifecycle::Running => "running",
        SessionLifecycle::Closing => "closing",
        SessionLifecycle::Exited => "exited",
        SessionLifecycle::Closed => "closed",
        SessionLifecycle::Failed => "failed",
    }
}
