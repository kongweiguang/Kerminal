//! MCP 自发现、配置与任务导航工具的只读实现。
//!
//! @author kongweiguang

use std::collections::BTreeSet;

use kerminal_application::{AppError, preferences::CONFIG_GUIDE, sessions::SessionFilter};
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
    "pending/confirm/approval/audit queues",
];

const CONTAINER_TEXT_WRITE_GUIDANCE: &str = "编辑现有容器文件时先调用 container.files.preview，并将返回的 revision 原样传入 container.files.write_text.expectedRevision；只要 expectedRevision 存在就执行比较并交换，目标缺失或 revision 不匹配都应重新读取处理。无 revision 时，create=true 表示仅新建，overwriteOnConflict=true 表示显式覆盖，两者同时为 true 表示显式 upsert；两者均为 false 会拒绝。改名不会覆盖已存在的目标。提交结果未知时先读回目标，再决定后续操作。";

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
        "kerminal.capabilities" => capabilities(arguments),
        "kerminal.app_guide" => app_guide(arguments),
        "kerminal.config_guide" => config_guide(arguments),
        "kerminal.tool_help" => tool_help(arguments),
        "kerminal.operation_guide" => operation_guide(arguments),
        "kerminal.runtime_snapshot" => runtime_snapshot(services, arguments, cancellation).await,
        _ => failure(AppError::invalid("Unknown discovery tool")),
    }
}

/// 从 catalog 注解形成能力地图，保持 MCP host 审批责任和当前真实工具集合可发现。
fn capabilities(arguments: Map<String, Value>) -> CallToolResult {
    if !arguments.is_empty() {
        return failure(AppError::invalid("tool arguments"));
    }
    let catalog = tool_catalog_entries();
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
        .filter(|family| *family != "discovery")
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

    success(
        format!("Kerminal MCP 能力指南已读取：当前暴露 {exposed_count} 个 tools。"),
        json!({
            "schemaVersion": 1,
            "purpose": "帮助外部 Agent 发现 Kerminal 运行态工具、配置边界与 host 审批职责。",
            "recommendedFirstCalls": [
                "kerminal.app_guide", "kerminal.config_guide", "kerminal.capabilities",
                "kerminal.tool_help", "kerminal.operation_guide", "kerminal.runtime_snapshot",
                "ssh.command", "terminal.create", "kerminal.config.validate"
            ],
            "sessionWorkspace": {
                "readFirst": ["AGENTS.md", "CLAUDE.md", "context/mcp-endpoint.json",
                    "context/target-binding.json", "context/terminal-snapshot.json", "kerminal-config.md"],
                "refreshTools": ["kerminal.agent.current_session", "kerminal.agent.target_context",
                    "terminal.list", "terminal.snapshot"],
                "scopeRule": "Agent 会话具有全局运行态能力；targetBinding 只是内置 Agent 首选终端，不是访问权限边界。",
                "terminalWriteRule": "外部 MCP 默认后台执行。持久或交互式 shell 使用 terminal.create 并保存其返回的 sessionId；目标失效时报错，不替换为其他 Tab。",
                "terminalExecutionPolicy": {
                    "scope": "global",
                    "preferredTarget": "targetBinding",
                    "externalMcpDefault": "非交互 SSH 使用 ssh.command；持久、交互或本地 shell 使用 terminal.create 与明确 sessionId。",
                    "visiblePtyFirst": "仅内置 Agent 的目标上下文流程使用 targetBinding 作为首选目标。",
                    "otherTerminals": "读取 terminal.list 不会切换 UI Tab；指定目标失效时不回退。",
                    "headlessFallback": "创建后台 PTY 后，使用其明确 sessionId 执行 snapshot、write 和 close。",
                    "backgroundFallback": "后台失败不自动改为向可见终端写入。",
                    "uiTabInteraction": "仅用户明确要求操作可见 Tab 时才选择已确认的 sessionId。",
                    "reconnect": "仅对明确选定的断开 pane 重连，不自动换 pane。"
                }
            },
            "runtimeToolFamilies": families,
            "fileFirstConfiguration": {
                "editableFiles": editable_files(),
                "manualGuide": "kerminal-config.md",
                "manualGuideTool": "kerminal.config_guide",
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
/// 容器导航固定在右栏并只绑定活动 pane；Context UI 入口移除不影响 Agent 目标绑定、context 文件或后台工具调用。
/// MCP 容器工具继续使用调用方显式目标，导航说明不改变其参数和执行语义。
///
/// @author kongweiguang
fn app_guide(arguments: Map<String, Value>) -> CallToolResult {
    if !arguments.is_empty() {
        return failure(AppError::invalid("tool arguments"));
    }
    let areas = vec![
        json!({
            "id":"hosts",
            "location":"left",
            "purpose":"左侧主机导航用于浏览已保存的主机与连接；SSH 工具可在后台直接调用，不要求先打开导航。",
            "toolFamilies":["ssh","credentials"]
        }),
        json!({
            "id":"terminal-workspace",
            "location":"center",
            "purpose":"中央工作区承载可见终端 Tab、分屏、文件编辑和完整文件工作区；后台终端和 SSH 调用不要求创建 UI Tab。",
            "toolFamilies":["terminal"]
        }),
        json!({
            "id":"agent-and-tools",
            "location":"right",
            "purpose":"右侧提供 Agent 与文件、运维工具，按需打开；容器工具固定在右侧并按当前活动 pane 显示宿主管理或容器只读信息。其它工具的顺序与位置可通过工具栏右键菜单调整。发现与诊断工具描述运行态能力，不要求打开面板。",
            "toolFamilies":["agent-session","sftp","container","tmux","port-forward","server-info","discovery","diagnostics"]
        }),
        json!({
            "id":"bottom-tools",
            "location":"right",
            "purpose":"命令历史默认从右栏底部图标按需打开；bottom-tools 为兼容保留的区域 ID，不表示终端底部工具区。后台查询不要求打开面板。",
            "toolFamilies":["history"]
        }),
        json!({
            "id":"configuration-workspace",
            "location":"workspace",
            "purpose":"配置工作区通过文件编辑与 validator 检查；连接凭据由主机连接界面的加密 vault 用例保存。",
            "toolFamilies":["config"]
        }),
    ];
    let tool_ids = tool_catalog_entries()
        .iter()
        .filter_map(|tool| tool["toolId"].as_str().map(ToOwned::to_owned))
        .collect::<Vec<_>>();
    success(
        "Kerminal 应用导航指南已读取：返回工作区区域、运行态工具和文件化配置边界。".to_owned(),
        json!({
            "schemaVersion": 1,
            "purpose": "帮助外部 Agent 将 Kerminal 的工作区区域映射到对应运行态工具族。",
            "areas": areas,
            "mcpToolIds": tool_ids,
            "sessionWorkspace": {
                "readFirst": ["AGENTS.md", "CLAUDE.md", "context/mcp-endpoint.json", "context/target-binding.json", "context/terminal-snapshot.json", "kerminal-config.md"],
                "refreshTools": ["kerminal.agent.current_session", "kerminal.agent.target_context"],
                "scope": "Agent endpoint 对应一个 live Agent session；普通 Tab 切换不会更新首选目标。"
            },
            "containerPanel": {
                "uiToolId": "containers",
                "location": "right",
                "fixedPosition": true,
                "defaultAfter": "sftp",
                "binding": "当前活动 pane 的已核实目标",
                "hostMode": "本机或 SSH pane 管理对应宿主的容器列表。",
                "containerMode": "容器 pane 只读当前容器的概览、资源和日志；不提供生命周期、Compose、exec 或文件操作。",
                "unavailable": "无有效终端、目标身份不一致、Telnet 或串口时显示原因，不回退到左栏选中的主机。"
            },
            "configuration": {"guide":"kerminal.config_guide", "validator":"kerminal.config.validate", "editableFiles":editable_files()},
            "safetyBoundaries": {
                "approval": "MCP host 管理审批、权限、hook 与审计，Kerminal 不添加第二层确认。",
                "background": "外部 MCP 操作不要求先打开终端 Tab。",
                "targetBinding": "首选目标失效时显式报告，不回退到另一个目标。",
                "configuration": "settings/profile/host/snippet/workflow 使用文件编辑与 validator，不通过 MCP CRUD。",
                "secrets": "不读取 vault 文件；普通配置仅保存凭据引用。"
            }
        }),
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

/// 根据 toolId、family 或 query 查找冻结 schema；SFTP 细分操作由调用方按需通过 family 查询。
///
/// @author kongweiguang
fn tool_help(arguments: Map<String, Value>) -> CallToolResult {
    let args = match crate::terminal_tools::parse_args::<ToolHelpArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let catalog = tool_catalog_entries();
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
                "Use kerminal.operation_guide when a task needs a sequence of calls.",
                "Use tools/list for the protocol-level catalog.",
                "For configuration, read kerminal.config_guide, edit files, then validate.",
                "For a specific SFTP operation, set family to sftp in kerminal.tool_help to inspect that tool's schema and safety notes.",
                "If a write result is outcome unknown because its acknowledgement was lost, inspect the affected resource before any retry."
            ],
            "safetyBoundaries":{
                "readOnly":"kerminal.tool_help never invokes the referenced tool.",
                "hostPolicy":"The MCP host owns confirmations, permissions, hooks and audit.",
                "fileFirstConfiguration":"Configuration CRUD tools are intentionally absent.",
                "terminalExecution":"Use background tools by default; explicit sessionId selects a session, and invalid targets are never substituted.",
                "outcomeUnknown":"A lost acknowledgement after a possible write does not prove that the operation failed. Read back state before retrying.",
                "containerTextWrite":CONTAINER_TEXT_WRITE_GUIDANCE
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

/// 将任务意图归一成当前可用工具序列，并为容器编辑提供 revision 优先的写入流程。
fn operation_guide(arguments: Map<String, Value>) -> CallToolResult {
    let args = match crate::terminal_tools::parse_args::<OperationGuideArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let requested_intent = non_empty(args.intent).unwrap_or_else(|| "overview".to_owned());
    let normalized = requested_intent
        .trim()
        .to_ascii_lowercase()
        .replace('_', "-");
    let intent = normalize_intent(&normalized);
    let note = (intent != normalized)
        .then(|| format!("Unsupported intent `{requested_intent}` was normalized to `{intent}`."));
    let calls = intent_calls(intent);
    let catalog = tool_catalog_entries();
    let available = calls
        .iter()
        .filter(|id| catalog.iter().any(|tool| tool["toolId"] == **id))
        .copied()
        .collect::<Vec<_>>();
    let missing = calls
        .iter()
        .filter(|id| !available.contains(id))
        .copied()
        .collect::<Vec<_>>();
    let workflow = workflow_steps(intent, &available);
    let refs = catalog
        .iter()
        .filter(|tool| available.contains(&tool["toolId"].as_str().unwrap_or_default()))
        .cloned()
        .collect::<Vec<_>>();
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
            "fileFirstConfiguration":{"readBeforeEditing":"kerminal-config.md or kerminal.config_guide","guideTool":"kerminal.config_guide","editableFiles":editable_files(),"validator":"kerminal.config.validate"},
            "safetyBoundaries":{
                "hostPolicy":"The MCP host owns confirmation, approval, permissions, hooks, and audit.",
                "terminalWrite":"External MCP defaults to background. Use returned sessionId explicitly; do not substitute a stale target.",
                "outcomeUnknown":"If the acknowledgement to a write is lost, read back the destination before deciding whether to retry.",
                "containerTextWrite":if intent == "container" { CONTAINER_TEXT_WRITE_GUIDANCE } else { "See kerminal.tool_help for container.files.write_text guidance." },
                "secrets":"Never expose vault content or plaintext credentials."
            },
            "containerTextWriteWorkflow":if intent == "container" { json!({
                "readTool":"container.files.preview",
                "writeTool":"container.files.write_text",
                "revisionSource":"将预览成功响应 structuredContent.data.revision 的完整对象复制到 expectedRevision。",
                "writeModes":{
                    "compareAndSwap":"提供 expectedRevision 后始终执行比较并交换，优先于 create 与 overwriteOnConflict。",
                    "createOnly":{"create":true,"overwriteOnConflict":false},
                    "explicitOverwrite":{"create":false,"overwriteOnConflict":true},
                    "explicitUpsert":{"create":true,"overwriteOnConflict":true},
                    "implicitWrite":{"create":false,"overwriteOnConflict":false,"result":"没有 expectedRevision 时拒绝"}
                },
                "conflict":"冲突后重新读取目标并处理差异，再决定是否写入。",
                "unknownOutcome":"回执未知时先读回目标，确认状态后再决定是否重试。"
            }) } else { Value::Null },
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
    let families = tool_catalog_entries();
    let data = json!({
        "schemaVersion":1,
        "runtime":{"terminalSessionCount":terminal_sessions.len(),"agentSessionCount":agent_sessions.len(),"portForwardCount":forwards.len()},
        "terminalSessions":terminal_summaries,
        "agentSessions":agent_sessions,
        "portForwards":forwards,
        "toolFamilies":families.iter().filter_map(|tool| tool["family"].as_str()).collect::<BTreeSet<_>>(),
        "diagnostics":diagnostics
    });
    success(
        "Kerminal 运行态快照已读取；所有 session 状态来自实际 application services。".to_owned(),
        data,
        Vec::new(),
    )
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

/// 匹配旧帮助族别名，且始终按冻结 catalog 顺序返回结果。
fn family_matches(requested: &str, family: &str) -> bool {
    let requested = requested.trim().to_ascii_lowercase().replace('_', "-");
    match requested.as_str() {
        "guide" | "guides" => family == "discovery",
        "agent" | "session" => family == "agent-session",
        "ssh-command" => family == "ssh",
        "docker" | "podman" => family == "container",
        "port" | "port-forwarding" => family == "port-forward",
        "server" => family == "server-info",
        "diagnostic" => family == "diagnostics",
        "credential" | "vault" | "secret" => family == "credentials",
        _ => requested == family,
    }
}

/// 返回当前工具族对应的已知故意缺席能力，避免把不存在的 CRUD 描述成可调用工具。
fn matching_absent_families(filter: &str) -> Vec<&'static str> {
    let normalized = filter.trim().to_ascii_lowercase().replace('_', "-");
    ABSENT_TOOL_FAMILIES
        .iter()
        .copied()
        .filter(|family| {
            let family = family.to_ascii_lowercase();
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
        "session-terminal" => vec![
            "kerminal.agent.current_session",
            "kerminal.agent.target_context",
            "terminal.resolve_agent_target",
            "terminal.snapshot",
        ],
        "ssh-command" => vec![
            "kerminal.runtime_snapshot",
            "ssh.command",
            "ssh.command_on_resolved_host",
        ],
        "sftp" => vec![
            "sftp.list",
            "sftp.transfer.enqueue",
            "sftp.transfer.list",
            "sftp.transfer.retry",
            "sftp.transfer.cancel",
        ],
        "tmux" => vec![
            "kerminal.runtime_snapshot",
            "tmux.probe",
            "tmux.list_sessions",
            "tmux.list_windows",
            "tmux.list_panes",
            "tmux.capture_pane",
        ],
        "container" => vec![
            "kerminal.runtime_snapshot",
            "container.list",
            "container.inspect",
            "container.logs.tail",
            "container.stats",
            "container.start",
            "container.stop",
            "container.restart",
            "container.remove",
            "container.files.list",
            "container.files.preview",
            "container.files.write_text",
            "container.files.create_directory",
            "container.files.rename",
            "container.files.chmod",
            "container.files.delete",
            "container.files.upload",
            "container.files.download",
            "sftp.transfer.list",
            "sftp.transfer.retry",
            "sftp.transfer.cancel",
        ],
        "port-forward" => vec![
            "kerminal.runtime_snapshot",
            "port_forward.list",
            "port_forward.create",
            "port_forward.close",
        ],
        "server-info" => vec!["kerminal.runtime_snapshot", "server_info.snapshot"],
        "history" => vec!["history.search"],
        "config" => vec!["kerminal.config_guide", "kerminal.config.validate"],
        "credentials" => vec![
            "kerminal.tool_help",
            "kerminal.host.upsert_with_credential",
            "kerminal.vault.encrypt_secret",
        ],
        "diagnostics" => vec![
            "diagnostics.runtime_health",
            "diagnostics.create_bundle",
            "kerminal.runtime_snapshot",
        ],
        _ => vec![
            "kerminal.app_guide",
            "kerminal.capabilities",
            "kerminal.runtime_snapshot",
            "kerminal.tool_help",
        ],
    }
}

/// 为工具序列生成简洁工作步骤，并在缺少真实可用工具时不伪称成功路径存在。
fn workflow_steps(intent: &str, tool_ids: &[&str]) -> Vec<Value> {
    tool_ids.iter().enumerate().map(|(index, tool_id)| json!({
        "id":format!("step-{}", index + 1),
        "toolId":tool_id,
        "description":step_description(intent, tool_id),
        "requiredArguments":required_arguments(tool_id),
        "safety":"Use only the explicitly selected target. If a write outcome is unknown, inspect state before retrying."
    })).collect()
}

/// 根据工具语义选择任务步骤说明，不从自由文本拼接命令或目标参数。
fn step_description(intent: &str, tool_id: &str) -> &'static str {
    match tool_id {
        "terminal.create" => "创建一个无需 UI Tab 的后台终端并记录返回 sessionId。",
        "terminal.write" => "只向显式 sessionId 写入已授权的终端输入。",
        "terminal.snapshot" => "读取已选 session 的有界文本快照。",
        "terminal.close" => "完成任务后关闭同一 sessionId 创建的后台终端。",
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
        "container.files.upload" | "container.files.download" => {
            "只受理容器文件传输并返回 transferId，accepted=true 且 completed=false；随后用 sftp.transfer.list 查询直到终态。"
        }
        "container.files.rename" => "在同一容器内改名；目标已存在时不会覆盖。",
        "container.files.chmod" => "修改显式容器路径的八进制权限。",
        "container.files.delete" => "删除显式选择的容器文件或目录；目录删除具有破坏性。",
        "container.files.create_directory" => "在显式容器路径创建目录。",
        "container.remove" => "移除显式选择的容器；force 只在明确传 true 时生效。",
        "container.files.write_text" => {
            "先读取目标 revision；无 revision 时显式选择新建或覆盖策略。"
        }
        "kerminal.config.validate" => "检查文件化配置并处理返回的错误或警告。",
        "history.search" => "只读查询与当前任务相关的历史命令。",
        "diagnostics.create_bundle" => "生成应用管理目录下的本地脱敏诊断包。",
        _ if intent == "overview" => "先确认当前运行态和工具能力，再继续具体操作。",
        _ => "调用对应的只读或运行态工具，并核对目标与返回状态。",
    }
}

/// 从冻结 JSON Schema 读取 required names，帮助调用方与 tool/list 保持同一参数定义。
fn required_arguments(tool_id: &str) -> Vec<String> {
    let catalog = tool_catalog_entries();
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
        "container" => "容器",
        "diagnostics" => "诊断与发现",
        "history" => "命令历史",
        "portForward" => "端口转发",
        "serverInfo" => "服务器信息",
        "sftp" => "文件传输",
        "ssh" => "SSH 命令",
        "terminal" => "终端",
        "tmux" => "tmux",
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
