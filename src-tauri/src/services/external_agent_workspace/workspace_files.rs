//! External agent workspace file preparation methods.
//!
//! @author kongweiguang

use std::{io::ErrorKind, path::PathBuf};

use serde_json::{json, Map, Value};

use super::*;
use crate::{
    error::{AppError, AppResult},
    models::agent_session::{
        AgentSessionId, AgentTargetBindingContext, AgentTerminalSnapshotContext,
        AGENT_SESSION_SCHEMA_VERSION,
    },
    services::agent_session_file_store::AgentSessionFileStore,
};

impl ExternalAgentWorkspaceService {
    /// 写入所有 provider 共用的 session 文件；`.mcp.json` 放在此层以保证 Custom CLI
    /// 也能按工作目录自动发现同一 scoped endpoint。
    pub(super) fn prepare_agent_session_common_files(
        &self,
        context: &AgentSessionWorkspaceContext,
        include_claude_file: bool,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<Vec<ExternalAgentFileOperation>> {
        let mut operations = Vec::with_capacity(if include_claude_file { 6 } else { 5 });
        operations.push(self.ensure_agent_session_instructions(context, options)?);
        if include_claude_file {
            operations.push(self.ensure_agent_session_claude_instructions(context, options)?);
        }
        operations.push(self.ensure_agent_session_mcp_json(context, options)?);
        operations.push(self.ensure_agent_session_mcp_endpoint(context, options)?);
        operations.push(self.ensure_agent_session_target_binding(context, options)?);
        operations.push(self.ensure_agent_session_terminal_snapshot(context, options)?);
        Ok(operations)
    }

    /// 只写内置 provider 专属配置，避免 Custom session 误生成 Codex 目录。
    pub(super) fn prepare_agent_session_provider_files(
        &self,
        context: &AgentSessionWorkspaceContext,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<Vec<ExternalAgentFileOperation>> {
        Ok(vec![
            self.ensure_agent_session_codex_config(context, options)?
        ])
    }

    /// 生成内置右栏 Agent 会话规则，将既有 global scope、targetBinding 绑定和可恢复的终端操作写入受管工作区。
    ///
    /// 规则文件必须描述运行态授权边界而不是缓存某一个 pane；MCP 服务端会在每次
    /// snapshot/write 调用中校验显式 sessionId 的 scope membership。
    pub(super) fn ensure_agent_session_instructions(
        &self,
        context: &AgentSessionWorkspaceContext,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<ExternalAgentFileOperation> {
        let agent_title = match context.agent_id.as_str() {
            "codex" => "Codex",
            "claude" => "Claude",
            "pi" => "PI Agent",
            "custom" => "Custom Agent",
            _ => "External Agent",
        };
        let agents_body = format!(
            r#"{MANAGED_BLOCK_START}
# Kerminal Agent Session

- You are the built-in Kerminal right-panel Agent for this session workspace, launched from the Kerminal right panel.
- Agent provider: `{}`.
- Agent title: `{}`.
- Kerminal agent session id: `{}`.
- Kerminal workspace root: `{}`.
- This session root: `{}`.
- Session-scoped MCP endpoint: `{}`.
- This is a Kerminal runtime workspace, not a source-code repository.
- Read `context/mcp-endpoint.json`, `context/target-binding.json`, and `context/terminal-snapshot.json` before runtime work. They contain the scoped endpoint, the session scope with its live terminal membership, and the most recent bounded terminal output snapshots.
- Kerminal MCP is tools-only; use it for live runtime actions, not file-backed configuration CRUD.
- Operate Kerminal through MCP when the task needs the live app: terminal sessions, SSH commands, SFTP files, tmux sessions, containers, port forwarding, server info, command history, diagnostics, runtime snapshot, or authorized credential saving.
- Start by calling `kerminal.app_guide` when you need the product/UI structure map; call `kerminal.capabilities` when you need the current tool map, file-first configuration boundary, or deliberately absent tool families; call `kerminal.tool_help` with `toolId`, `family`, or `query` when you need exact schemas, examples, and safety annotations; call `kerminal.config_guide` when you need the generated configuration rules through MCP; call `kerminal.operation_guide` with an intent such as `terminal`, `session-terminal`, `ssh-command`, `config`, `sftp`, `tmux`, `container`, `port-forward`, `server-info`, `history`, `credentials`, or `diagnostics` when you need a concrete tool sequence; call `kerminal.runtime_snapshot` when you need the current running terminals, Agent sessions, port forwards, and next actions.
- MCP host policy owns confirmation, approval, permissions, hooks, and audit and may choose when to request them. Kerminal exposes tools and validates arguments; it does not add a second per-command prompt or pending/confirm queue.
- This is the built-in right-panel Agent exception to the external-MCP background default. Its existing global/tab scope and permissions are unchanged; use the session's `targetBinding` as the bound terminal target, and do not silently change that binding.
- Every external Agent session uses global terminal scope across all Kerminal tabs. The session `targetBinding` is a binding preference, not a replacement for scope authorization; other user terminals remain operable only through the existing explicit scope rules.
- For ordinary commands and interactive work, call `terminal.snapshot` for the current `targetBinding`, then `terminal.write` through the visible PTY; the command and output stay visible in the user's left PTY. The current target path does not require an extra confirmation or a new scope selection.
- If `targetBinding` is missing/stale or the user names another terminal, call `kerminal.agent.current_session`, `kerminal.agent.target_context`, and `terminal.list` to refresh and choose an explicit `sessionId`; do not ask the user to reopen an already available terminal or create a new binding.
- Pass `agentSessionId` from `context/mcp-endpoint.json` to the Agent scope tools; use an explicit returned `sessionId` only when selecting another terminal instead of the current `targetBinding`.
- Use `terminal.list` only when selecting another global user terminal or recovering stale target context; its returned session ids are authoritative. The right-panel Agent TUI is excluded from the user-terminal set.
- If a selected terminal is actually disconnected, call `terminal.reconnect` with its `paneId`, wait for the acknowledgement, then call `terminal.list` and `terminal.snapshot` again. A disconnect does not invalidate global scope, so continue the task after reconnecting it.
- If no live PTY exists, use `terminal.create` with `target = "local"` or `target = "ssh"` plus a saved `hostId` when needed, then use `terminal.snapshot`/`terminal.write`/`terminal.close` on its returned `sessionId`. Close every headless session you create with `terminal.close` when the task completes, including after an error when cleanup is possible. `shell` applies only to local targets; SSH uses the saved host's login shell, and `agentSessionId` is optional correlation metadata rather than a scope restriction. Use `ssh.command` or `ssh.command_on_resolved_host` only when a suitable PTY cannot be created or the user explicitly requests a background structured result; their stdout/stderr do not appear in the left terminal.
- Useful runtime tool families: `terminal.*` including `terminal.reconnect`, `ssh.command`, `ssh.command_on_resolved_host`, `sftp.*`, `tmux.*`, `container.*` including `container.files.*` (`container.files.list`, `container.files.preview`, `container.files.write_text`, `container.files.upload`, `container.files.download`, `container.files.create_directory`, `container.files.rename`, `container.files.chmod`, `container.files.delete`), `port_forward.*`, `server_info.snapshot`, `history.search`, `diagnostics.*`, `kerminal.app_guide`, `kerminal.config_guide`, `kerminal.capabilities`, `kerminal.tool_help`, `kerminal.operation_guide`, `kerminal.runtime_snapshot`, `kerminal.host.upsert_with_credential`, and `kerminal.vault.encrypt_secret`.
- For SFTP copies, call `sftp.transfer.enqueue` only to receive a transfer id and track it through `sftp.transfer.list`. No total-duration limit applies; keep the default `idleTimeoutSeconds` for large files unless the user has a reason to change its 30-3600 second no-confirmed-progress threshold. Queue wait does not count, a five-second pause only shows waiting, and safe single-file upload/download automatically reconnects once after idle timeout under the same task id. The final file may remain absent until commit. For a terminal snapshot with `retryable=true`, use `sftp.transfer.retry`; `resumable=true` means confirmed partial data may be used after validation, and `resumable=false` starts fresh. Retry inherits the original endpoints, conflict policy, and adopted timeout, returns one accepted idempotent successor id, and must be followed by `sftp.transfer.list` until completion. Use `sftp.transfer.cancel` only when the user requests it, then query until a terminal state; an already committed file remains succeeded. The 60-second MCP guard protects short calls only, never the queued transfer. Do not call retired synchronous `sftp.upload*` or `sftp.download*` tools.
- If SFTP returns `failureKind=commitUnknown`, the final rename may have succeeded without an acknowledgement. That task is not retryable; inspect the destination and content before any new write.
- File-backed Kerminal configuration is file-first: edit files under the workspace root directly, including `settings.toml`, `profiles/*.toml`, `hosts/groups.toml`, `hosts/*.toml`, `snippets/*.toml`, and `workflows/*.toml`.
- Before editing Kerminal configuration files, read `{}` from the workspace root or call `kerminal.config_guide` for the same generated rules. It documents file purposes, relationships, fields, examples, forbidden edits, and validation.
- After editing Kerminal configuration files, call MCP tool `{CONFIG_VALIDATOR_TOOL_ID}` with `scope = "all"` or the narrowest matching scope. If MCP validation is unavailable, manually check the guide and say validation was manual only.
- If Kerminal is running, valid file-backed config edits auto-refresh the UI and show a concise `cfg: ...` notice; invalid TOML keeps last-known-good. This feedback does not replace validation.
- Do not expect MCP config CRUD for settings, profiles, hosts, snippets, workflows, UI choreography, history writes, or approval/audit queues.
- Do not edit `data/command.sqlite` directly; use command history lookup tools when command history is needed.
- Do not read or edit `secrets/` unless the user explicitly asks for credential work; when authorized, follow `kerminal-config.md` and use the UI save flow, `kerminal.host.upsert_with_credential`, or `kerminal.vault.encrypt_secret` so ordinary host files only keep `secret_ref` / `key_passphrase_ref`; never write `password`, `credential_secret`, or `inline_private_key` into ordinary config files.
{MANAGED_BLOCK_END}
"#,
            context.agent_id,
            agent_title,
            context.agent_session_id,
            workspace_display_path(&self.workspace_dir, &self.workspace_dir),
            workspace_display_path(&self.workspace_dir, &context.session_root),
            context.mcp_endpoint,
            CONFIG_REFERENCE_FILE_NAME
        );
        patch_managed_block(
            &context.session_root.join("AGENTS.md"),
            &agents_body,
            "Update agent session instructions.",
            options,
        )
    }

    /// 为内置右栏 Claude 会话生成与 AGENTS.md 一致的 scope-aware 规则，避免 provider 模板
    /// 重新引入单终端绑定门禁或把断线误报为会话失效。
    pub(super) fn ensure_agent_session_claude_instructions(
        &self,
        context: &AgentSessionWorkspaceContext,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<ExternalAgentFileOperation> {
        let claude_body = format!(
            r#"{MANAGED_BLOCK_START}
@AGENTS.md

## Kerminal Claude Session

- Follow `AGENTS.md` for this Kerminal agent session.
- This is the built-in right-panel Agent session workspace. Its existing `targetBinding` binding and scope/permissions remain in force; the global external-MCP background default does not replace this session binding.
- This is a Kerminal runtime workspace, not a source-code repository.
- Kerminal MCP is tools-only; use it for live runtime actions, not file-backed configuration CRUD.
- MCP host policy owns confirmation, approval, permissions, hooks, and audit and may choose when to request them; Kerminal does not add a second per-command prompt or pending/confirm queue.
- Call `kerminal.app_guide` when you need the product/UI structure map; call `kerminal.capabilities` when you need the current runtime tool map or config/tool boundary; call `kerminal.tool_help` with `toolId`, `family`, or `query` when you need exact schemas, examples, and safety annotations; call `kerminal.config_guide` when you need the generated configuration rules through MCP; call `kerminal.operation_guide` with an intent such as `session-terminal`, `ssh-command`, `config`, `sftp`, `tmux`, `container`, `port-forward`, `server-info`, `history`, `credentials`, or `diagnostics` when you need a concrete tool sequence; call `kerminal.runtime_snapshot` when you need the current running terminals, Agent sessions, port forwards, and next actions.
- Every external Agent session uses global terminal scope across all Kerminal tabs. Use `targetBinding` from the session context as the bound current terminal; it does not restrict other user terminals under the existing scope rules.
- For ordinary commands and interactive work, use `terminal.snapshot` on the current `targetBinding`, then `terminal.write` through the visible PTY so input and output remain visible in the user's left PTY. Do not add a confirmation or scope-switch step when that target is available.
- Read `context/mcp-endpoint.json`, `context/target-binding.json`, and `context/terminal-snapshot.json`; when targetBinding is missing/stale or another terminal is requested, use the session-scoped endpoint for `kerminal.agent.current_session`, `kerminal.agent.target_context`, and `terminal.list` to choose an explicit sessionId.
- Pass `agentSessionId` to `kerminal.agent.current_session` and `kerminal.agent.target_context`; terminal snapshot/write calls use the current targetBinding by default and an explicit sessionId only for another global user terminal.
- Use `terminal.list` when another terminal is needed or target context is stale; membership is checked by the server for explicit selections.
- Use runtime tool families from `AGENTS.md`, including `terminal.*` and `terminal.reconnect`, `ssh.command`, `ssh.command_on_resolved_host`, `sftp.*`, `tmux.*`, `container.*` including `container.files.*` (`container.files.list`, `container.files.preview`, `container.files.write_text`, `container.files.upload`, `container.files.download`, `container.files.create_directory`, `container.files.rename`, `container.files.chmod`, `container.files.delete`), `port_forward.*`, `server_info.snapshot`, `history.search`, `diagnostics.*`, `kerminal.app_guide`, `kerminal.config_guide`, `kerminal.capabilities`, `kerminal.tool_help`, `kerminal.operation_guide`, `kerminal.runtime_snapshot`, and credential helpers.
- For SFTP copies, call `sftp.transfer.enqueue` only to receive a transfer id and track it through `sftp.transfer.list`. No total-duration limit applies; keep the default `idleTimeoutSeconds` for large files unless the user has a reason to change its 30-3600 second no-confirmed-progress threshold. Queue wait does not count, a five-second pause only shows waiting, and safe single-file upload/download automatically reconnects once after idle timeout under the same task id. The final file may remain absent until commit. For a terminal snapshot with `retryable=true`, use `sftp.transfer.retry`; `resumable=true` means confirmed partial data may be used after validation, and `resumable=false` starts fresh. Retry inherits the original endpoints, conflict policy, and adopted timeout, returns one accepted idempotent successor id, and must be followed by `sftp.transfer.list` until completion. Use `sftp.transfer.cancel` only when the user requests it, then query until a terminal state; an already committed file remains succeeded. The 60-second MCP guard protects short calls only, never the queued transfer. Do not call retired synchronous `sftp.upload*` or `sftp.download*` tools.
- If SFTP returns `failureKind=commitUnknown`, the final rename may have succeeded without an acknowledgement. That task is not retryable; inspect the destination and content before any new write.
- Prefer direct file edits in the Kerminal workspace root for `settings.toml`, `profiles/*.toml`, `hosts/*.toml`, `snippets/*.toml`, and `workflows/*.toml`.
- Before editing Kerminal configuration files, read `{}` from the workspace root or call `kerminal.config_guide` for the same generated rules.
- After editing Kerminal configuration files, call MCP tool `{CONFIG_VALIDATOR_TOOL_ID}` with `scope = "all"` or the narrowest matching scope. If MCP validation is unavailable, manually check the guide and say validation was manual only.
- If Kerminal is running, valid file-backed config edits auto-refresh the UI and show a concise `cfg: ...` notice; invalid TOML keeps last-known-good. This feedback does not replace validation.
- Use the session-scoped MCP endpoint `{}`.
- If a selected member is actually disconnected, use `terminal.reconnect` with its `paneId`, wait for acknowledgement, and refresh the scope; a disconnected terminal is a recoverable runtime condition and does not require the user to reopen it.
- If no live PTY exists, use `terminal.create` with `target = "local"` or `target = "ssh"` plus a saved `hostId` when needed, then use `terminal.snapshot`/`terminal.write`/`terminal.close` on its returned `sessionId`. Close every headless session you create with `terminal.close` when the task completes, including after an error when cleanup is possible. `shell` applies only to local targets; SSH uses the saved host's login shell, and `agentSessionId` is optional correlation metadata rather than a scope restriction. Use `ssh.command` or `ssh.command_on_resolved_host` only when a suitable PTY cannot be created or the user explicitly requests a background structured result; their stdout/stderr do not appear in the left terminal.
- Do not expect MCP tools for config CRUD or UI choreography such as `settings.*`, `profile.*`, `remote_host.*`, `snippet.*`, `workflow.*`, or `workspace.*`; `terminal.create` creates a headless PTY and `terminal.reconnect` recovers an existing pane.
- Do not read or edit `secrets/` unless the user explicitly asks for credential work; when authorized, follow `kerminal-config.md` and use the UI save flow, `kerminal.host.upsert_with_credential`, or `kerminal.vault.encrypt_secret` so ordinary host files only keep `secret_ref` / `key_passphrase_ref`; never write `password`, `credential_secret`, or `inline_private_key` into ordinary config files.
{MANAGED_BLOCK_END}
"#,
            CONFIG_REFERENCE_FILE_NAME, context.mcp_endpoint
        );
        patch_managed_block(
            &context.session_root.join("CLAUDE.md"),
            &claude_body,
            "Update Claude agent session instructions.",
            options,
        )
    }

    pub(super) fn ensure_agent_session_codex_config(
        &self,
        context: &AgentSessionWorkspaceContext,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<ExternalAgentFileOperation> {
        let path = context.session_root.join(".codex").join("config.toml");
        let snippet = format!(
            r#"[mcp_servers.kerminal]
url = "{}"
default_tools_approval_mode = "prompt"
tool_timeout_sec = 60
enabled = true
"#,
            context.mcp_endpoint
        );
        let current = read_optional_string(&path)?;
        let current_content = current.as_deref().unwrap_or_default();
        let next = replace_toml_table(current_content, "[mcp_servers.kerminal]", &snippet);
        let current_snippet = extract_toml_table(current_content, "[mcp_servers.kerminal]");
        apply_text_plan(
            WorkspaceTextPlan {
                path,
                next,
                current,
                current_snippet,
                next_snippet: snippet,
                reason: "Update session Codex MCP server table.".to_owned(),
            },
            options,
        )
    }

    /// 为共用标准 `.mcp.json` 的 Agent 写入 session endpoint；Pi 另写原生项目配置。
    pub(super) fn ensure_agent_session_mcp_json(
        &self,
        context: &AgentSessionWorkspaceContext,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<ExternalAgentFileOperation> {
        ensure_mcp_json_server(
            context.session_root.join(".mcp.json"),
            context.mcp_endpoint.as_str(),
            60_000,
            "Update session MCP server entry.",
            options,
        )
    }

    /// 为 Pi session 写 cwd-scoped 原生配置，保证它能自行发现会话专属 endpoint。
    pub(super) fn ensure_agent_session_pi_mcp_json(
        &self,
        context: &AgentSessionWorkspaceContext,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<ExternalAgentFileOperation> {
        ensure_mcp_json_server(
            context.session_root.join(".pi").join("mcp.json"),
            context.mcp_endpoint.as_str(),
            60,
            "Update Pi session MCP server entry.",
            options,
        )
    }

    pub(super) fn ensure_agent_session_mcp_endpoint(
        &self,
        context: &AgentSessionWorkspaceContext,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<ExternalAgentFileOperation> {
        let env = self.agent_session_env(context);
        let next = serde_json::to_string_pretty(&json!({
            "schemaVersion": 1,
            "agentSessionId": context.agent_session_id.as_str(),
            "agentId": context.agent_id.as_str(),
            "workspaceRoot": path_to_string(&self.workspace_dir),
            "agentSessionRoot": path_to_string(&context.session_root),
            "endpoint": context.mcp_endpoint.as_str(),
            "transport": "streamable-http",
            "toolsOnly": true,
            "generatedAt": current_unix_timestamp_string(),
            "env": env
        }))?;
        let path = context
            .session_root
            .join("context")
            .join("mcp-endpoint.json");
        let current = read_optional_string(&path)?;
        apply_text_plan(
            WorkspaceTextPlan {
                path,
                next: format!("{next}\n"),
                current,
                current_snippet: None,
                next_snippet: next,
                reason: "Update session MCP endpoint context.".to_owned(),
            },
            options,
        )
    }

    pub(super) fn ensure_agent_session_target_binding(
        &self,
        context: &AgentSessionWorkspaceContext,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<ExternalAgentFileOperation> {
        let target_context = self.seed_agent_session_target_binding_context(context)?;
        let next = serde_json::to_string_pretty(&target_context)?;
        let path = context
            .session_root
            .join("context")
            .join("target-binding.json");
        let current = read_optional_string(&path)?;
        apply_text_plan(
            WorkspaceTextPlan {
                path,
                next: format!("{next}\n"),
                current,
                current_snippet: None,
                next_snippet: next,
                reason: "Update session target binding context.".to_owned(),
            },
            options,
        )
    }

    pub(super) fn ensure_agent_session_terminal_snapshot(
        &self,
        context: &AgentSessionWorkspaceContext,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<ExternalAgentFileOperation> {
        let agent_session_id = AgentSessionId::new(context.agent_session_id.clone())?;
        let snapshot_context = AgentTerminalSnapshotContext {
            schema_version: AGENT_SESSION_SCHEMA_VERSION,
            agent_session_id,
            target_terminal_session_id: None,
            captured_bytes: 0,
            max_bytes: AGENT_SESSION_TERMINAL_SNAPSHOT_BYTES,
            truncated: false,
            redacted: false,
            output: String::new(),
            generated_at: current_unix_timestamp_string(),
        };
        let next = serde_json::to_string_pretty(&snapshot_context)?;
        let path = context
            .session_root
            .join("context")
            .join("terminal-snapshot.json");
        let current = read_optional_string(&path)?;
        apply_text_plan(
            WorkspaceTextPlan {
                path,
                next: format!("{next}\n"),
                current,
                current_snippet: None,
                next_snippet: next,
                reason: "Update session terminal snapshot context.".to_owned(),
            },
            options,
        )
    }

    pub(super) fn seed_agent_session_target_binding_context(
        &self,
        context: &AgentSessionWorkspaceContext,
    ) -> AppResult<AgentTargetBindingContext> {
        let agent_session_id = AgentSessionId::new(context.agent_session_id.clone())?;
        let generated_at = current_unix_timestamp_string();
        let store = AgentSessionFileStore::new(&self.workspace_dir);
        match store.read_session(&agent_session_id) {
            Ok(session) => Ok(AgentTargetBindingContext::from_session_target(
                &session,
                generated_at,
            )),
            Err(AppError::Io(error)) if error.kind() == ErrorKind::NotFound => Ok(
                unbound_agent_target_binding_context(agent_session_id, generated_at),
            ),
            Err(error) => Err(error),
        }
    }

    /// 汇总内置 provider 状态；共享 DTO 的 adapter 字段仅为旧调用方兼容保留。
    pub(super) fn agent_status(
        &self,
        id: &str,
        title: &str,
        command: &str,
        config_path: PathBuf,
    ) -> ExternalAgentStatus {
        let installed = executable_on_path(command);
        let config_ready = match id {
            "codex" => {
                codex_config_ready(&config_path)
                    && self.agents_file_path().is_file()
                    && self.config_reference_path().is_file()
            }
            "claude" => {
                mcp_json_config_ready(&config_path)
                    && self.agents_file_path().is_file()
                    && self.config_reference_path().is_file()
                    && self.claude_instructions_path().is_file()
            }
            _ => false,
        };
        let status_detail = match (installed, config_ready) {
            (true, true) => "Ready".to_owned(),
            (true, false) => "CLI installed; workspace files need regeneration".to_owned(),
            (false, true) => "Workspace files ready; CLI not found in PATH".to_owned(),
            (false, false) => "CLI not found in PATH; workspace files need regeneration".to_owned(),
        };

        ExternalAgentStatus {
            id: id.to_owned(),
            title: title.to_owned(),
            cli_command: command.to_owned(),
            installed,
            adapter_available: true,
            config_ready,
            config_path: path_to_string(&config_path),
            status_detail,
        }
    }

    /// 报告 Pi CLI 与原生项目配置；旧 adapter 字段恒为 true，不参与可用性判断。
    pub(super) fn pi_agent_status(&self) -> ExternalAgentStatus {
        let config_path = self.pi_config_path();
        let installed = executable_on_path("pi");
        let config_ready = mcp_json_config_ready(&config_path)
            && self.agents_file_path().is_file()
            && self.config_reference_path().is_file();
        let status_detail = if !installed {
            "PI CLI not found in PATH"
        } else if !config_ready {
            "PI CLI installed; workspace files need regeneration"
        } else {
            "Ready"
        };

        ExternalAgentStatus {
            id: "pi".to_owned(),
            title: "PI Agent".to_owned(),
            cli_command: PI_AGENT_LAUNCH_COMMAND.to_owned(),
            installed,
            adapter_available: true,
            config_ready,
            config_path: path_to_string(&config_path),
            status_detail: status_detail.to_owned(),
        }
    }

    /// Custom 命令按定义动态探测，因此 workspace 级状态不假定 CLI 或 adapter 存在。
    pub(super) fn custom_agent_status(&self) -> ExternalAgentStatus {
        ExternalAgentStatus {
            id: "custom".to_owned(),
            title: "Custom Agent".to_owned(),
            cli_command: String::new(),
            installed: false,
            adapter_available: false,
            config_ready: false,
            config_path: path_to_string(&self.workspace_dir),
            status_detail: "Enter a custom CLI command to launch it in this workspace".to_owned(),
        }
    }

    pub(super) fn prepare_codex_files(
        &self,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<Vec<ExternalAgentFileOperation>> {
        Ok(vec![
            self.ensure_shared_instructions(options)?,
            self.ensure_config_reference(options)?,
            self.ensure_codex_config(options)?,
        ])
    }

    pub(super) fn prepare_claude_files(
        &self,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<Vec<ExternalAgentFileOperation>> {
        let mut operations = Vec::with_capacity(4);
        operations.push(self.ensure_shared_instructions(options)?);
        operations.push(self.ensure_config_reference(options)?);
        operations.extend(self.ensure_claude_files(options)?);
        Ok(operations)
    }

    /// 为 Pi 准备共享说明和 Pi 原生 `.pi/mcp.json`，不触碰 Claude 的根 `.mcp.json`。
    pub(super) fn prepare_pi_files(
        &self,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<Vec<ExternalAgentFileOperation>> {
        Ok(vec![
            self.ensure_shared_instructions(options)?,
            self.ensure_config_reference(options)?,
            self.ensure_pi_mcp_json(options)?,
        ])
    }

    pub(super) fn validator_status(&self) -> ExternalAgentValidatorStatus {
        let available = self.mcp_server_running;
        ExternalAgentValidatorStatus {
            available,
            command: format!("MCP tool: {CONFIG_VALIDATOR_TOOL_ID} {{\"scope\":\"all\"}}"),
            detail: if available {
                "Call this read-only MCP tool after editing settings.toml, profiles, hosts, snippets, or workflows.".to_owned()
            } else {
                "Start the Kerminal MCP Server to use runtime config validation; otherwise manually check kerminal-config.md.".to_owned()
            },
            status: if available {
                "available"
            } else {
                "mcp-unavailable"
            }
            .to_owned(),
        }
    }

    /// 生成全局 Kerminal 工作区规则；无 session 时仍以显式 scope member 操作运行态，
    /// 同时保留文件优先配置和凭据隔离边界。
    pub(super) fn ensure_shared_instructions(
        &self,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<ExternalAgentFileOperation> {
        let validator_line = format!(
            "- After changing Kerminal configuration files, call MCP tool `{CONFIG_VALIDATOR_TOOL_ID}` with `scope = \"all\"` or the narrowest matching scope, then fix any diagnostics before reporting success. If the tool is unavailable, manually check `{CONFIG_REFERENCE_FILE_NAME}` and say validation was manual only."
        );
        let agents_body = format!(
            r#"{MANAGED_BLOCK_START}
# Kerminal External Agent Workspace

- Treat this directory as the Kerminal runtime workspace, not a source-code repository.
- Your job is to operate Kerminal for the user through the Kerminal MCP server and edit file-backed configuration only when the user asks for configuration changes.
- Operate Kerminal through MCP for live app features: terminal sessions, SSH commands, SFTP files, tmux sessions, containers, port forwarding, server info, command history, diagnostics, runtime snapshot, and authorized credential saving. MCP endpoint: `{}`.
- Call MCP tool `kerminal.app_guide` when you need the product/UI structure map; call `kerminal.capabilities` when you need the current Kerminal tool map, recommended first calls, file-first config boundary, or deliberately absent tool families; call `kerminal.tool_help` with `toolId`, `family`, or `query` when you need exact schemas, examples, and safety annotations; call `kerminal.config_guide` when you need the generated configuration rules through MCP; call `kerminal.operation_guide` with an intent such as `terminal`, `session-terminal`, `ssh-command`, `config`, `sftp`, `tmux`, `container`, `port-forward`, `server-info`, `history`, `credentials`, or `diagnostics` when you need a concrete tool sequence; call `kerminal.runtime_snapshot` when you need the current running terminals, Agent sessions, port forwards, and next actions.
- MCP host policy owns confirmation, approval, permissions, hooks, and audit and may choose when to request them. Kerminal exposes tools and validates arguments; it does not add a second per-command prompt or pending/confirm queue.
- Useful MCP tool families include `terminal.*` including `terminal.reconnect`, `ssh.command`, `ssh.command_on_resolved_host`, `sftp.*`, `tmux.*`, `container.*` including `container.files.*` (`container.files.list`, `container.files.preview`, `container.files.write_text`, `container.files.upload`, `container.files.download`, `container.files.create_directory`, `container.files.rename`, `container.files.chmod`, `container.files.delete`), `port_forward.*`, `server_info.snapshot`, `history.search`, `diagnostics.*`, `kerminal.app_guide`, `kerminal.config_guide`, `kerminal.capabilities`, `kerminal.tool_help`, `kerminal.operation_guide`, `kerminal.runtime_snapshot`, credential helpers (`kerminal.host.upsert_with_credential`, `kerminal.vault.encrypt_secret`), and, in session workspaces, `kerminal.agent.*`.
- For SFTP file copies, use `sftp.transfer.enqueue` only to obtain a transfer id, then `sftp.transfer.list` to track it. A five-second pause shows waiting without failure; safe single-file uploads/downloads automatically reconnect once after an idle timeout under the same id. If a terminal snapshot reports `retryable=true`, call `sftp.transfer.retry` with the original id; `resumable=true` may use retained confirmed partial data after validation, while `resumable=false` starts fresh. Retry returns an accepted idempotent successor, not a completed file, so continue listing the successor until a terminal status. Use `sftp.transfer.cancel` only for an explicit stop, then query until terminal. The 60-second MCP call timeout covers these short calls, not the background transfer. `idleTimeoutSeconds` defaults to 180 seconds and protects only consecutive missing confirmed progress; do not increase it just because a file is large. A long transfer with progress has no total-duration limit, and the final file may remain absent until commit. Do not call retired synchronous `sftp.upload*` or `sftp.download*` tools.
- If SFTP returns `failureKind=commitUnknown`, the final rename may have succeeded without an acknowledgement. That task is not retryable; inspect the destination and content before any new write.
- If this workspace has an Agent session under `agents/sessions/<id>/`, prefer launching from that session directory so `AGENTS.md`, `context/mcp-endpoint.json`, `context/target-binding.json`, `context/terminal-snapshot.json`, and the session-scoped endpoint describe the current scope. The nested session rules identify the built-in right-panel Agent and retain its existing `targetBinding` binding; follow them there instead of applying the global external-MCP default to the bound target. The session cwd is intentionally isolated under `agents/sessions/<id>/`; its logical Kerminal workspace remains the parent `~/.kerminal` root.
- Every external Agent session uses global scope across all Kerminal tabs. `targetBinding` identifies a preferred user terminal only; it is not an access restriction and does not by itself authorize visible-Tab interaction. The right-panel Agent TUI is never a user-terminal member.
- External MCP calls default to background execution. Do not open, switch, or create a UI Tab unless the user explicitly asks to operate the current or a specified Tab.
- For non-interactive SSH commands, use `ssh.command` or `ssh.command_on_resolved_host` first; their stdout/stderr return to the Agent and do not appear in a visible terminal.
- For a persistent shell, interactive input, or a local shell, use `terminal.create` with `target = "local"` or `target = "ssh"` plus a saved `hostId` when needed, then use its returned `sessionId` explicitly with `terminal.snapshot`, `terminal.write`, and `terminal.close`. `terminal.create` is headless and does not open a Tab; close every headless session you create with `terminal.close` when the task completes, including after an error when cleanup is possible. An interactive shell does not imply UI-Tab interaction (`shell` is local-only; SSH uses the saved host login shell; `agentSessionId` is correlation metadata only).
- Only when the user explicitly asks to operate the current or a specified UI Tab, call `terminal.list` to discover and confirm the requested visible terminal, then select its explicit `sessionId`; when a managed Agent session context exists, refresh it with `kerminal.agent.current_session`/`kerminal.agent.target_context` as needed. A background failure must not fall back to a visible Tab; a missing or stale requested target must be reported for reconfirmation, not replaced with another Tab.
- If a selected member is actually disconnected after the user explicitly selected that visible terminal, call `terminal.reconnect` with its `paneId`, wait for acknowledgement, and refresh the list. A disconnect does not invalidate global scope.
- Use Kerminal MCP for saved credentials and remote access. Do not read `secrets/` to get passwords, private keys, or key passphrases.
- Before editing Kerminal configuration files, read `{CONFIG_REFERENCE_FILE_NAME}` or call `kerminal.config_guide` for the same generated rules. It documents file purposes, relationships, fields, examples, forbidden edits, and validation.
- Prefer direct file edits for file-backed Kerminal configuration; use MCP for runtime operation rather than config CRUD.
- Editable config files by default: `settings.toml`, `profiles/*.toml`, `hosts/groups.toml`, `hosts/*.toml`, `snippets/*.toml`, and `workflows/*.toml`.
- When Kerminal is running, valid file-backed config edits auto-refresh the UI and show a short `cfg: ...` notice; invalid TOML keeps last-known-good. Still validate with `{CONFIG_VALIDATOR_TOOL_ID}` before reporting success.
- Do not use Kerminal MCP tools for settings, profile, host, snippet, or workflow CRUD when the same change can be made by editing the files above.
- Do not expect MCP tools for config CRUD or UI choreography such as `settings.*`, `profile.*`, `remote_host.*`, `snippet.*`, `workflow.*`, `workspace.*`, `terminal.resolve_current`, `workspace.focus_tab`, or history write/delete/clear operations. `terminal.create` creates a headless PTY without UI choreography, while `terminal.reconnect` recovers an existing pane.
- Kerminal-owned runtime areas: `data/`, `logs/`, `cache/`, `temp/`, and `exports/`; prefer MCP tools over direct edits there.
- Do not edit `data/command.sqlite` directly; use `history.search` when command history is needed.
- Secret scope: do not read or edit `secrets/` unless the user explicitly asks for credential work.
- When credential work is authorized, follow `kerminal-config.md` and use the UI save flow, `kerminal.host.upsert_with_credential`, or `kerminal.vault.encrypt_secret` so host files only keep `secret_ref` / `key_passphrase_ref`; never write `password =`, `credential_secret`, or `inline_private_key` into ordinary config files.
- Never store API keys, tokens, passwords, or private keys in ordinary config files.
- Keep edits small and targeted; do not reformat all TOML or remove comments outside the requested change.
{}
{MANAGED_BLOCK_END}
"#,
            self.mcp_endpoint, validator_line
        );
        patch_managed_block(
            &self.agents_file_path(),
            &agents_body,
            "Update shared external agent instructions.",
            options,
        )
    }

    pub(super) fn ensure_config_reference(
        &self,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<ExternalAgentFileOperation> {
        let path = self.config_reference_path();
        let current = read_optional_string(&path)?;
        apply_text_plan(
            WorkspaceTextPlan {
                path,
                next: CONFIG_REFERENCE_BODY.to_owned(),
                current: current.clone(),
                current_snippet: current,
                next_snippet: CONFIG_REFERENCE_BODY.to_owned(),
                reason: "Update Kerminal configuration guide for external agents.".to_owned(),
            },
            options,
        )
    }

    pub(super) fn ensure_codex_config(
        &self,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<ExternalAgentFileOperation> {
        let path = self.codex_config_path();
        let snippet = format!(
            r#"[mcp_servers.kerminal]
url = "{}"
default_tools_approval_mode = "prompt"
tool_timeout_sec = 60
enabled = true
"#,
            self.mcp_endpoint
        );
        let current = read_optional_string(&path)?;
        let current_content = current.as_deref().unwrap_or_default();
        let next = replace_toml_table(current_content, "[mcp_servers.kerminal]", &snippet);
        let current_snippet = extract_toml_table(current_content, "[mcp_servers.kerminal]");
        apply_text_plan(
            WorkspaceTextPlan {
                path,
                next,
                current,
                current_snippet,
                next_snippet: snippet,
                reason: "Update Codex MCP server table.".to_owned(),
            },
            options,
        )
    }

    pub(super) fn ensure_claude_files(
        &self,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<Vec<ExternalAgentFileOperation>> {
        let claude_body = format!(
            r#"{MANAGED_BLOCK_START}
@AGENTS.md

## Claude Code

- Treat this directory as the Kerminal runtime workspace, not a source-code repository.
- Follow `AGENTS.md` first.
- Operate Kerminal through MCP for live terminal, SSH/SFTP, tmux, container, port forwarding, server info, history, diagnostics, runtime snapshot, and authorized credential saving work: `{}`.
- Call MCP tool `kerminal.app_guide` when you need the product/UI structure map; call `kerminal.capabilities` when you need the current Kerminal tool map or config/tool boundary; call `kerminal.tool_help` with `toolId`, `family`, or `query` when you need exact schemas, examples, and safety annotations; call `kerminal.config_guide` when you need the generated configuration rules through MCP; call `kerminal.operation_guide` with an intent such as `terminal`, `session-terminal`, `ssh-command`, `config`, `sftp`, `tmux`, `container`, `port-forward`, `server-info`, `history`, `credentials`, or `diagnostics` when you need a concrete tool sequence; call `kerminal.runtime_snapshot` when you need the current running terminals, Agent sessions, port forwards, and next actions.
- Useful runtime tool families include `terminal.*`, `kerminal.agent.*`, `ssh.command`, `ssh.command_on_resolved_host`, `sftp.*`, `tmux.*`, `container.*` including `container.files.*` (`container.files.list`, `container.files.preview`, `container.files.write_text`, `container.files.upload`, `container.files.download`, `container.files.create_directory`, `container.files.rename`, `container.files.chmod`, `container.files.delete`), `port_forward.*`, `server_info.snapshot`, `history.search`, `diagnostics.*`, `kerminal.app_guide`, `kerminal.config_guide`, `kerminal.capabilities`, `kerminal.tool_help`, `kerminal.operation_guide`, `kerminal.runtime_snapshot`, `kerminal.host.upsert_with_credential`, and `kerminal.vault.encrypt_secret`.
- For SFTP copies, call `sftp.transfer.enqueue` only to receive a transfer id and track it through `sftp.transfer.list`. No total-duration limit applies; keep the default `idleTimeoutSeconds` for large files unless the user has a reason to change its 30-3600 second no-confirmed-progress threshold. Queue wait does not count, a five-second pause only shows waiting, and safe single-file upload/download automatically reconnects once after idle timeout under the same task id. The final file may remain absent until commit. For a terminal snapshot with `retryable=true`, use `sftp.transfer.retry`; `resumable=true` means confirmed partial data may be used after validation, and `resumable=false` starts fresh. Retry inherits the original endpoints, conflict policy, and adopted timeout, returns one accepted idempotent successor id, and must be followed by `sftp.transfer.list` until completion. Use `sftp.transfer.cancel` only when the user requests it, then query until a terminal state; an already committed file remains succeeded. The 60-second MCP guard protects short calls only, never the queued transfer. Do not call retired synchronous `sftp.upload*` or `sftp.download*` tools.
- If SFTP returns `failureKind=commitUnknown`, the final rename may have succeeded without an acknowledgement. That task is not retryable; inspect the destination and content before any new write.
- MCP host policy owns confirmation, approval, permissions, hooks, and audit and may choose when to request them; Kerminal does not add a second per-command prompt or pending/confirm queue.
- Every external Agent session uses global scope across all Kerminal tabs. `targetBinding` identifies a preferred user terminal only; it is not an access restriction and does not by itself authorize visible-Tab interaction.
- External MCP calls default to background execution. Do not open, switch, or create a UI Tab unless the user explicitly asks to operate the current or a specified Tab.
- In a session workspace, read `context/mcp-endpoint.json`, `context/target-binding.json`, and `context/terminal-snapshot.json`. A nested session workspace belongs to the built-in right-panel Agent; follow its session-local rules when present rather than applying the external-MCP default to its bound target.
- For non-interactive SSH commands, use `ssh.command` or `ssh.command_on_resolved_host` first; their stdout/stderr return to the Agent and do not appear in a visible terminal. For a persistent shell, interactive input, or a local shell, use headless `terminal.create`, then pass its returned `sessionId` explicitly to `terminal.snapshot`/`terminal.write`/`terminal.close`; close every headless session you create with `terminal.close` when the task completes, including after an error when cleanup is possible. An interactive shell does not imply UI-Tab interaction.
- Only when the user explicitly asks to operate the current or a specified UI Tab, call `terminal.list` to discover and confirm the requested visible terminal, then pass its explicit `sessionId` to `terminal.snapshot`/`terminal.write`; when a managed Agent session context exists, refresh it with `kerminal.agent.current_session`/`kerminal.agent.target_context` as needed. A background failure must not fall back to a visible Tab; a missing or stale requested target must be reported for reconfirmation, not replaced with another Tab.
- Before editing Kerminal configuration files, read `{CONFIG_REFERENCE_FILE_NAME}` or call `kerminal.config_guide` for the same generated rules.
- After editing Kerminal configuration files, call MCP tool `{CONFIG_VALIDATOR_TOOL_ID}` with `scope = "all"` or the narrowest matching scope. If MCP validation is unavailable, manually check `{CONFIG_REFERENCE_FILE_NAME}` and say validation was manual only.
- Prefer direct file edits for file-backed Kerminal configuration; use the Kerminal MCP server only for runtime actions that require the live app, an existing terminal session, saved connection credentials, SSH/SFTP, tmux, containers, port forwarding, server info, history, diagnostics, runtime snapshot, or authorized credential saving: `{}`.
- Do not use Kerminal MCP tools for settings, profile, host, snippet, or workflow CRUD when direct file edits can express the change.
- Do not expect MCP tools for config CRUD or UI choreography such as `settings.*`, `profile.*`, `remote_host.*`, `snippet.*`, `workflow.*`, or `workspace.*`; `terminal.create` creates a headless PTY and `terminal.reconnect` recovers an existing pane.
- If a selected member is disconnected, call `terminal.reconnect` with `paneId`, wait for acknowledgement, and repeat `terminal.list`; a disconnect is recoverable and does not change global scope.
- Do not edit `data/command.sqlite` directly; use `history.search` when command history is needed.
- Do not edit `secrets/` unless the user explicitly asks; when authorized, follow `kerminal-config.md` and use the UI save flow, `kerminal.host.upsert_with_credential`, or `kerminal.vault.encrypt_secret` so ordinary host files only keep `secret_ref` / `key_passphrase_ref`; never write `password`, `credential_secret`, or `inline_private_key` into ordinary config files.
{MANAGED_BLOCK_END}
"#,
            self.mcp_endpoint, self.mcp_endpoint
        );
        Ok(vec![
            patch_managed_block(
                &self.claude_instructions_path(),
                &claude_body,
                "Update Claude workspace instructions.",
                options,
            )?,
            self.ensure_claude_mcp_json(options)?,
        ])
    }

    /// 保留 Claude 使用的根 `.mcp.json` 入口，与 Pi 的 `.pi/mcp.json` 相互独立。
    pub(super) fn ensure_claude_mcp_json(
        &self,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<ExternalAgentFileOperation> {
        ensure_mcp_json_server(
            self.claude_config_path(),
            self.mcp_endpoint.as_str(),
            60_000,
            "Update Claude project MCP server entry.",
            options,
        )
    }

    /// Pi reads trusted project MCP settings from `.pi/mcp.json`; 60 is expressed in seconds.
    pub(super) fn ensure_pi_mcp_json(
        &self,
        options: &WorkspaceWriteOptions,
    ) -> AppResult<ExternalAgentFileOperation> {
        ensure_mcp_json_server(
            self.pi_config_path(),
            self.mcp_endpoint.as_str(),
            60,
            "Update Pi project MCP server entry.",
            options,
        )
    }

    pub(super) fn agents_file_path(&self) -> PathBuf {
        self.workspace_dir.join("AGENTS.md")
    }

    pub(super) fn claude_instructions_path(&self) -> PathBuf {
        self.workspace_dir.join("CLAUDE.md")
    }

    pub(super) fn config_reference_path(&self) -> PathBuf {
        self.workspace_dir.join(CONFIG_REFERENCE_FILE_NAME)
    }

    pub(super) fn codex_config_path(&self) -> PathBuf {
        self.workspace_dir.join(".codex").join("config.toml")
    }

    pub(super) fn claude_config_path(&self) -> PathBuf {
        self.workspace_dir.join(".mcp.json")
    }

    /// 返回 Pi 原生项目 MCP 配置路径；目录布局与其实际启动 cwd 保持一致。
    pub(super) fn pi_config_path(&self) -> PathBuf {
        self.workspace_dir.join(".pi").join("mcp.json")
    }
}

/// 更新配置中唯一由 Kerminal 管理的 server，同时保留其他 server 与顶层用户内容。
///
/// 统一复用解析、diff、备份和 dry-run 流程；timeout 数值沿用客户端 schema 的单位，
/// Pi 使用秒，现有 `.mcp.json` provider 继续使用毫秒。
fn ensure_mcp_json_server(
    path: PathBuf,
    endpoint: &str,
    timeout_value: u64,
    reason: &str,
    options: &WorkspaceWriteOptions,
) -> AppResult<ExternalAgentFileOperation> {
    let current = read_optional_string(&path)?;
    let mut root = match parse_mcp_json(&path, current.as_deref(), options)? {
        Some(root) => root,
        None => json!({}),
    };
    let previous_server = root
        .pointer("/mcpServers/kerminal")
        .map(|value| serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()));
    let object = root.as_object_mut().ok_or_else(|| {
        AppError::InvalidInput(format!(
            "{} must be a JSON object. Use overwritePolicy=backupAndReplaceInvalid to repair it.",
            path_to_string(&path)
        ))
    })?;
    let servers = object
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    if !servers.is_object() {
        match options.overwrite_policy {
            ExternalAgentOverwritePolicy::BackupAndReplaceInvalid => {
                *servers = Value::Object(Map::new());
            }
            ExternalAgentOverwritePolicy::PreserveUserContent => {
                return Err(AppError::InvalidInput(format!(
                    "{}.mcpServers must be a JSON object. Use overwritePolicy=backupAndReplaceInvalid to repair it.",
                    path_to_string(&path)
                )));
            }
        }
    }
    let servers_object = servers.as_object_mut().expect("mcpServers object");
    let next_server = json!({
        "type": "http",
        "url": endpoint,
        "timeout": timeout_value
    });
    servers_object.insert("kerminal".to_owned(), next_server.clone());
    let next = serde_json::to_string_pretty(&root)?;
    apply_text_plan(
        WorkspaceTextPlan {
            path,
            next: format!("{next}\n"),
            current,
            current_snippet: previous_server,
            next_snippet: serde_json::to_string_pretty(&next_server)?,
            reason: reason.to_owned(),
        },
        options,
    )
}
