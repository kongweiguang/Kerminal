// @author kongweiguang
import { invoke, isTauri } from "@tauri-apps/api/core";
import { parseAgentCommandLine } from "./agentCommandLine";

export type ExternalAgentId = "codex" | "claude" | "pi" | "custom";

/** Agent 会话的右栏归属；Rust runtime 会将其有效终端能力统一投影为 global。 */
export type AgentSessionScope =
  | { kind: "tab"; tabId: string }
  | { kind: "global" };

export interface ExternalAgentStatus {
  id: ExternalAgentId;
  title: string;
  cliCommand: string;
  /** 保留 IPC 兼容字段；内置 provider 已原生支持 MCP，不再依赖额外 adapter。 */
  adapterAvailable: boolean;
  installed: boolean;
  configReady: boolean;
  configPath: string;
  statusDetail: string;
}

export interface ExternalAgentWorkspaceStatus {
  workspaceDir: string;
  mcpEndpoint: string;
  mcpServerRunning: boolean;
  agents: Record<ExternalAgentId, ExternalAgentStatus>;
  validator?: ExternalAgentValidatorStatus;
}

export interface PrepareExternalAgentWorkspaceRequest {
  agentId: ExternalAgentId;
  agentSessionId?: string;
  customCommand?: string;
  resumeProviderSession?: boolean;
  dryRun?: boolean;
  overwritePolicy?: ExternalAgentOverwritePolicy;
}

type ExternalAgentOverwritePolicy =
  | "backupAndReplaceInvalid"
  | "preserveUserContent";

export interface ExternalAgentLaunchSpec {
  agentId: ExternalAgentId;
  agentSessionId?: string;
  title: string;
  shell: string;
  args: string[];
  cwd: string;
  env?: Record<string, string>;
  status?: ExternalAgentSessionStatus;
  message: string;
  dryRun?: boolean;
  operations?: ExternalAgentFileOperation[];
  validator?: ExternalAgentValidatorStatus;
}

export type ExternalAgentSessionStatus =
  | "starting"
  | "running"
  | "stale"
  | "closed"
  | "error";

export type AgentSessionRecordStatus = "active" | "archived" | "stale";

interface ExternalAgentValidatorStatus {
  available: boolean;
  command: string;
  detail: string;
  status: string;
}

interface ExternalAgentFileOperation {
  path: string;
  action: "created" | "updated" | "unchanged";
  changed: boolean;
  dryRun: boolean;
  backupPath?: string;
  diff?: string;
  reason: string;
}

export type AgentTargetLiveStatus = "unbound" | "ready" | "stale" | "closed";

export interface AgentSessionTargetRequest {
  bindingId?: string;
  bindingGeneration?: number;
  paneId?: string;
  tabId?: string;
  targetTerminalSessionId?: string;
  targetRef?: string;
  targetKind?: string;
  cwd?: string;
  shell?: string;
  liveStatus?: AgentTargetLiveStatus;
  lastSeenAt?: string;
}

interface AgentSessionTargetRecord extends AgentSessionTargetRequest {
  binding_id?: string;
  binding_generation?: number;
  pane_id?: string;
  tab_id?: string;
  target_terminal_session_id?: string;
  target_ref?: string;
  target_kind?: string;
  live_status?: AgentTargetLiveStatus;
  last_seen_at?: string;
}

interface AgentSessionScopeRecord {
  kind?: "tab" | "global";
  tabId?: string;
  tab_id?: string;
}

export interface AgentSessionCreateRequest {
  agentId: ExternalAgentId;
  launcherKey?: string;
  scope: AgentSessionScope;
  title?: string;
  target?: AgentSessionTargetRequest;
  mcpEndpoint?: string;
}

export interface AgentSessionUpdateRequest {
  title?: string;
  launch?: AgentSessionLaunch;
}

interface AgentSessionLaunch {
  commandLabel: string;
  shell: string;
  args: string[];
  cwd: string;
}

interface AgentSessionRecordLaunch {
  commandLabel?: string;
  command_label?: string;
  shell: string;
  args: string[];
  cwd: string;
}

export interface AgentSessionRecord {
  session: {
    agentSessionId?: string;
    agent_session_id?: string;
    agentId?: ExternalAgentId;
    agent_id?: ExternalAgentId;
    launcherKey?: string;
    launcher_key?: string;
    title: string;
    sessionRoot?: string;
    session_root?: string;
    workspaceRoot?: string;
    workspace_root?: string;
    createdAt?: string;
    created_at?: string;
    updatedAt?: string;
    updated_at?: string;
    status?: AgentSessionRecordStatus;
    launch: AgentSessionRecordLaunch;
    scope?: AgentSessionScopeRecord | AgentSessionScope | null;
    target?: AgentSessionTargetRecord | null;
  };
}

export interface AgentSessionList {
  sessions: AgentSessionRecord[];
  diagnostics?: Array<{
    code: string;
    message: string;
    path?: string;
  }>;
}

export function getExternalAgentWorkspaceStatus(): Promise<ExternalAgentWorkspaceStatus> {
  if (!isTauri()) {
    return Promise.resolve(previewExternalAgentWorkspaceStatus());
  }

  return invoke<ExternalAgentWorkspaceStatus>(
    "get_external_agent_workspace_status",
  );
}

export function createAgentSession(
  request: AgentSessionCreateRequest,
): Promise<AgentSessionRecord> {
  if (!isTauri()) {
    return Promise.resolve(previewAgentSessionRecord(request));
  }

  return invoke<AgentSessionRecord>("agent_session_create", { request });
}

export function listAgentSessions(): Promise<AgentSessionList> {
  if (!isTauri()) {
    return Promise.resolve({ diagnostics: [], sessions: [] });
  }

  return invoke<AgentSessionList>("agent_session_list");
}

export function updateAgentSession(
  agentSessionId: string,
  request: AgentSessionUpdateRequest,
): Promise<AgentSessionRecord> {
  if (!isTauri()) {
    return Promise.resolve(
      previewUpdatedAgentSessionRecord(agentSessionId, request),
    );
  }

  return invoke<AgentSessionRecord>("agent_session_update", {
    agentSessionId,
    request: agentSessionUpdatePayload(request),
  });
}

/**
 * 通过专用 rebind command 更新 preferred target；后端会同时校验 live pane、
 * 刷新 runtime binding 并写入 target-binding/snapshot，避免普通 update 只改磁盘。
 */
export function rebindAgentSessionTarget(
  agentSessionId: string,
  target: AgentSessionTargetRequest,
): Promise<AgentSessionRecord> {
  if (!isTauri()) {
    return Promise.resolve(
      previewReboundAgentSessionRecord(agentSessionId, target),
    );
  }

  return invoke<AgentSessionRecord>("agent_session_rebind_target", {
    agentSessionId,
    target,
  });
}

/** Rust 的既有 AgentSessionLaunch 存储字段使用 snake_case；仅在 IPC 边界转换，保留前端模型的 camelCase。 */
function agentSessionUpdatePayload(request: AgentSessionUpdateRequest) {
  if (!request.launch) {
    return request;
  }

  const { commandLabel, ...launch } = request.launch;
  return {
    ...request,
    launch: {
      command_label: commandLabel,
      ...launch,
    },
  };
}

export function archiveAgentSession(
  agentSessionId: string,
): Promise<AgentSessionRecord> {
  if (!isTauri()) {
    return Promise.resolve(previewArchivedAgentSessionRecord(agentSessionId));
  }

  return invoke<AgentSessionRecord>("agent_session_archive", {
    agentSessionId,
  });
}

export function agentSessionRecordId(record: AgentSessionRecord): string {
  const id = record.session.agentSessionId ?? record.session.agent_session_id;
  if (!id?.trim()) {
    throw new Error("agent_session_create did not return an agent session id.");
  }
  return id;
}

export function agentSessionRecordAgentId(
  record: AgentSessionRecord,
): ExternalAgentId | undefined {
  return record.session.agentId ?? record.session.agent_id;
}

/** 兼容 IPC camelCase 与 session.toml snake_case，空 key 按旧会话处理。 */
export function agentSessionRecordLauncherKey(
  record: AgentSessionRecord,
): string | undefined {
  const launcherKey =
    record.session.launcherKey ?? record.session.launcher_key;
  return launcherKey?.trim() || undefined;
}

/**
 * Custom 恢复必须使用创建时的命令快照；优先使用完整 commandLabel，旧记录再退回
 * shell/args 的可读组合，不回查当前定义。
 */
export function agentSessionRecordLaunchCommand(
  record: AgentSessionRecord,
): string | undefined {
  const commandLabel =
    record.session.launch.commandLabel ?? record.session.launch.command_label;
  if (commandLabel?.trim()) {
    return commandLabel.trim();
  }
  const shell = record.session.launch.shell.trim();
  if (!shell) {
    return undefined;
  }
  return [shell, ...record.session.launch.args].join(" ").trim();
}

/**
 * 读取会话的右栏归属；旧 global 记录若保存了目标 Tab，迁移到该 Tab 以免同一
 * 历史助手在各 Tab 重复出现。运行态终端能力仍由 Rust 投影为全局权限。
 */
export function agentSessionRecordScope(
  record: AgentSessionRecord,
  fallback?: AgentSessionScope,
): AgentSessionScope {
  const scope = normalizeAgentSessionScope(record.session.scope);
  if (scope?.kind === "tab") {
    return scope;
  }

  const target = agentSessionRecordTarget(record);
  if (target?.liveStatus === "unbound") {
    return { kind: "global" };
  }
  const tabId = target?.tabId?.trim();
  if (tabId) {
    return { kind: "tab", tabId };
  }
  if (scope) {
    return scope;
  }
  return fallback ?? { kind: "global" };
}

export function agentSessionRecordTarget(
  record: AgentSessionRecord,
): AgentSessionTargetRequest | undefined {
  const target = record.session.target;
  if (!target) {
    return undefined;
  }
  return {
    bindingId: target.bindingId ?? target.binding_id,
    bindingGeneration: target.bindingGeneration ?? target.binding_generation,
    cwd: target.cwd,
    lastSeenAt: target.lastSeenAt ?? target.last_seen_at,
    liveStatus: target.liveStatus ?? target.live_status,
    paneId: target.paneId ?? target.pane_id,
    shell: target.shell,
    tabId: target.tabId ?? target.tab_id,
    targetKind: target.targetKind ?? target.target_kind,
    targetRef: target.targetRef ?? target.target_ref,
    targetTerminalSessionId:
      target.targetTerminalSessionId ?? target.target_terminal_session_id,
  };
}

export function agentSessionRecordStatus(
  record: AgentSessionRecord,
): AgentSessionRecordStatus {
  return record.session.status ?? "active";
}

export function prepareExternalAgentWorkspace(
  request: PrepareExternalAgentWorkspaceRequest,
): Promise<ExternalAgentLaunchSpec> {
  if (!isTauri()) {
    return Promise.resolve(previewExternalAgentLaunchSpec(request));
  }

  return invoke<ExternalAgentLaunchSpec>("prepare_external_agent_workspace", {
    request,
  });
}

/** 预览沿用原生 MCP 配置位置；CLI 是否安装仍由真实桌面运行态确认。 */
function previewExternalAgentWorkspaceStatus(): ExternalAgentWorkspaceStatus {
  const workspaceDir = "~/.kerminal";
  const endpoint = "http://127.0.0.1:37657/mcp";
  return {
    agents: {
      claude: {
        adapterAvailable: true,
        cliCommand: "claude",
        configPath: `${workspaceDir}/.mcp.json`,
        configReady: true,
        id: "claude",
        installed: false,
        statusDetail: "Claude CLI was not detected in browser preview.",
        title: "Claude",
      },
      codex: {
        adapterAvailable: true,
        cliCommand: "codex",
        configPath: `${workspaceDir}/.codex/config.toml`,
        configReady: true,
        id: "codex",
        installed: false,
        statusDetail: "Codex CLI was not detected in browser preview.",
        title: "Codex",
      },
      custom: {
        adapterAvailable: true,
        cliCommand: "",
        configPath: "",
        configReady: false,
        id: "custom",
        installed: false,
        statusDetail: "Custom Agent is not initialized by default.",
        title: "Custom",
      },
      pi: {
        adapterAvailable: true,
        cliCommand: "pi --approve",
        configPath: `${workspaceDir}/.pi/mcp.json`,
        configReady: true,
        id: "pi",
        installed: false,
        statusDetail:
          "PI Agent CLI was not detected in browser preview.",
        title: "PI Agent",
      },
    },
    mcpEndpoint: endpoint,
    mcpServerRunning: false,
    validator: {
      available: false,
      command: "Validator unavailable in browser preview",
      detail: "Open the Tauri app to resolve the local validator command.",
      status: "missing",
    },
    workspaceDir,
  };
}

function previewExternalAgentLaunchSpec({
  agentId,
  agentSessionId,
  customCommand,
  resumeProviderSession,
}: PrepareExternalAgentWorkspaceRequest): ExternalAgentLaunchSpec {
  const status = previewExternalAgentWorkspaceStatus();
  const agent = status.agents[agentId];
  const custom = agentId === "custom";
  const resumeCommand =
    resumeProviderSession && agentId === "codex"
      ? "codex resume --last"
      : resumeProviderSession && agentId === "claude"
        ? "claude --continue"
        : resumeProviderSession && agentId === "pi"
          ? `${agent.cliCommand} --continue`
          : null;
  const parsed = custom
    ? parseAgentCommandLine(customCommand ?? "")
    : parseAgentCommandLine(resumeCommand ?? agent.cliCommand);
  const sessionRoot = agentSessionId
    ? `${status.workspaceDir}/agents/sessions/${agentSessionId}`
    : status.workspaceDir;
  return {
    agentId,
    agentSessionId,
    args: parsed?.args ?? [],
    cwd: sessionRoot,
    env: agentSessionId
      ? {
          KERMINAL_AGENT_SESSION_ID: agentSessionId,
          KERMINAL_AGENT_SESSION_ROOT: sessionRoot,
          KERMINAL_MCP_ENDPOINT: `${status.mcpEndpoint}/agents/${agentSessionId}`,
          KERMINAL_WORKSPACE_ROOT: status.workspaceDir,
        }
      : undefined,
    shell: parsed?.shell ?? agent.cliCommand,
    title: agent.title,
    message: `${agent.title} preview launch prepared.`,
    dryRun: false,
    operations: [],
    validator: status.validator,
  };
}

/** 在非 Tauri 预览中保留请求 scope，确保 UI 测试与真实 IPC 使用同一权限模型。 */
function previewAgentSessionRecord(
  request: AgentSessionCreateRequest,
): AgentSessionRecord {
  const workspaceRoot = "~/.kerminal";
  const agentSessionId = `preview-${request.agentId}-${Date.now().toString(36)}`;
  const sessionRoot = `${workspaceRoot}/agents/sessions/${agentSessionId}`;
  const title =
    request.title ??
    (request.agentId === "custom"
      ? "Custom"
      : request.agentId === "claude"
        ? "Claude"
        : "Codex");
  return {
    session: {
      agentId: request.agentId,
      agentSessionId,
      launcherKey: request.launcherKey,
      launch: {
        args: [],
        commandLabel: request.agentId,
        cwd: sessionRoot,
        shell: request.agentId === "custom" ? "" : request.agentId,
      },
      scope: request.scope,
      sessionRoot,
      status: "active",
      target: request.target,
      title,
      workspaceRoot,
    },
  };
}

/** 兼容 Rust 返回的 camelCase/snake_case scope，拒绝缺失 tabId 的非法 tab 作用域。 */
function normalizeAgentSessionScope(
  value: AgentSessionRecord["session"]["scope"] | undefined,
): AgentSessionScope | undefined {
  if (!value || typeof value !== "object") {
    return undefined;
  }
  if (value.kind === "global") {
    return { kind: "global" };
  }
  if (value.kind !== "tab") {
    return undefined;
  }
  const candidate = value as AgentSessionScopeRecord;
  const tabId = candidate.tabId ?? candidate.tab_id;
  return tabId?.trim() ? { kind: "tab", tabId: tabId.trim() } : undefined;
}

function previewArchivedAgentSessionRecord(
  agentSessionId: string,
): AgentSessionRecord {
  const workspaceRoot = "~/.kerminal";
  const sessionRoot = `${workspaceRoot}/agents/sessions/${agentSessionId}`;
  return {
    session: {
      agentSessionId,
      launch: {
        args: [],
        commandLabel: "archived",
        cwd: sessionRoot,
        shell: "",
      },
      sessionRoot,
      status: "archived",
      title: "Archived Agent Session",
      workspaceRoot,
    },
  };
}

/** 非 Tauri 预览同步 update IPC 的标题/启动快照，保持原有恢复语义。 */
function previewUpdatedAgentSessionRecord(
  agentSessionId: string,
  request: AgentSessionUpdateRequest,
): AgentSessionRecord {
  const workspaceRoot = "~/.kerminal";
  const sessionRoot = `${workspaceRoot}/agents/sessions/${agentSessionId}`;
  return {
    session: {
      agentId: "custom",
      agentSessionId,
      launch: {
        args: [],
        commandLabel: "custom",
        cwd: sessionRoot,
        shell: "",
      },
      sessionRoot,
      status: "active",
      title: request.title ?? "Custom",
      workspaceRoot,
    },
  };
}

/** 非 Tauri 预览模拟 rebind 返回的持久 target，便于浏览器组件保持同一契约。 */
function previewReboundAgentSessionRecord(
  agentSessionId: string,
  target: AgentSessionTargetRequest,
): AgentSessionRecord {
  const workspaceRoot = "~/.kerminal";
  const sessionRoot = `${workspaceRoot}/agents/sessions/${agentSessionId}`;
  return {
    session: {
      agentId: "custom",
      agentSessionId,
      launch: {
        args: [],
        commandLabel: "custom",
        cwd: sessionRoot,
        shell: "",
      },
      sessionRoot,
      status: "active",
      target,
      title: "Custom",
      workspaceRoot,
    },
  };
}
