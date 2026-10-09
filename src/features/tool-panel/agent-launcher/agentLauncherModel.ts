// @author kongweiguang
import {
  agentSessionRecordAgentId,
  type AgentSessionRecord,
  type ExternalAgentId,
  type ExternalAgentLaunchSpec,
  type ExternalAgentStatus,
  type ExternalAgentWorkspaceStatus,
} from "../../../lib/agentLauncherApi";
import { parseAgentCommandLine } from "../../../lib/agentCommandLine";
const EXTERNAL_AGENT_IDS: ExternalAgentId[] = [
  "codex",
  "claude",
  "pi",
  "custom",
];
type AgentLauncherTone = "ready" | "warning" | "danger" | "muted";
export type AgentLaunchPermissionMode = "default" | "skipPermissions";
type AgentAvailabilityLabel = "可用" | "需安装" | "需设置";

/** 内置 provider 的标题在 session、历史和下拉之间保持稳定，避免 PI 被当作 Custom。 */
function agentTitle(agentId: ExternalAgentId): string {
  if (agentId === "claude") {
    return "Claude";
  }
  if (agentId === "pi") {
    return "PI Agent";
  }
  if (agentId === "custom") {
    return "Custom";
  }
  return "Codex";
}

export function buildAgentSessionTitle(
  agentId: ExternalAgentId,
  targetLabel: string,
) {
  const title = agentTitle(agentId);
  const normalizedTarget = targetLabel.trim();
  return !normalizedTarget || normalizedTarget === "未绑定"
    ? title
    : `${title} · ${normalizedTarget}`;
}

export interface McpStatusViewModel {
  label: string;
  detail: string;
  tone: AgentLauncherTone;
}

export interface AgentActionViewModel {
  agentId: ExternalAgentId;
  title: string;
  cliCommand: string;
  configPath: string;
  statusDetail: string;
  availabilityDetail: string;
  availabilityLabel: AgentAvailabilityLabel;
  installLabel: string;
  configLabel: string;
  actionLabel: string;
  disabled: boolean;
  disabledReason?: string;
  tone: AgentLauncherTone;
}

interface AgentActionOptions {
  mcpServerRunning: boolean;
  terminalLauncherAvailable: boolean;
}

export function buildAgentLauncherViewModel(
  status: ExternalAgentWorkspaceStatus,
  terminalLauncherAvailable: boolean,
): AgentActionViewModel[] {
  return EXTERNAL_AGENT_IDS.map((agentId) =>
    buildAgentActionViewModel(status.agents[agentId], {
      mcpServerRunning: status.mcpServerRunning,
      terminalLauncherAvailable,
    }),
  );
}

/** 原生 MCP 不需要额外 adapter，可用性只由 CLI 和受管配置决定。 */
export function buildAgentActionViewModel(
  agent: ExternalAgentStatus,
  options: AgentActionOptions,
): AgentActionViewModel {
  const disabledReason = resolveAgentDisabledReason(agent, options);
  const customAgent = agent.id === "custom";
  const commandConfigured = Boolean(agent.cliCommand.trim());
  const configReady = customAgent ? true : agent.configReady;
  const installed = agent.installed;
  const availability = resolveAgentAvailability(agent, disabledReason);
  const statusDetail =
    !customAgent && !options.mcpServerRunning
      ? "Kerminal MCP Server will be started before launch."
      : agent.statusDetail.trim() ||
        (customAgent
          ? "Runs your command directly."
          : installed
            ? `${agent.title} is ready.`
            : `${agent.title} CLI not on PATH.`);

  return {
    actionLabel: customAgent
      ? `Open ${agent.title}`
      : !options.mcpServerRunning
        ? `Start & Open ${agent.title}`
        : configReady
          ? `Open ${agent.title}`
          : "Prepare & Open",
    agentId: agent.id,
    availabilityDetail: availability.detail,
    availabilityLabel: availability.label,
    cliCommand: agent.cliCommand.trim() || "No command configured",
    configLabel: customAgent
      ? commandConfigured
        ? "Explicit command"
        : "Enter command"
      : configReady
        ? "Config ready"
        : "Config needs update",
    configPath: customAgent
      ? "User supplied CLI"
      : agent.configPath.trim() || "Config path not generated",
    disabled: Boolean(disabledReason),
    disabledReason,
    installLabel: installed ? "Installed" : "Missing CLI",
    statusDetail,
    title: agent.title,
    tone: resolveAgentTone(agent, disabledReason),
  };
}

export function getMcpStatusView(
  status: Pick<
    ExternalAgentWorkspaceStatus,
    "mcpEndpoint" | "mcpServerRunning"
  >,
): McpStatusViewModel {
  if (status.mcpServerRunning) {
    return {
      detail: status.mcpEndpoint || "Endpoint unavailable.",
      label: "Running",
      tone: "ready",
    };
  }

  return {
    detail: "Start MCP Server first.",
    label: "Stopped",
    tone: "danger",
  };
}

export function buildAgentConfigSnippet(
  status: Pick<ExternalAgentWorkspaceStatus, "mcpEndpoint">,
): string {
  const endpoint = status.mcpEndpoint || "http://127.0.0.1:37657/mcp";
  return [
    "# Codex: ~/.kerminal/.codex/config.toml",
    "[mcp_servers.kerminal]",
    `url = "${endpoint}"`,
    'default_tools_approval_mode = "prompt"',
    "tool_timeout_sec = 60",
    "enabled = true",
    "",
    "# Claude: ~/.kerminal/.mcp.json",
    JSON.stringify(
      {
        mcpServers: {
          kerminal: {
            timeout: 60000,
            type: "http",
            url: endpoint,
          },
        },
      },
      null,
      2,
    ),
  ].join("\n");
}

export function agentSupportsPermissionSkip(agentId: ExternalAgentId): boolean {
  return agentId === "codex" || agentId === "claude";
}

/**
 * 新建或恢复入口默认采用用户授予的完整权限；PI/Custom 没有可泛化的额外 flag，
 * 因而继续使用其既有命令快照，避免为不支持的 provider 拼接虚假参数。
 */
export function defaultAgentLaunchPermissionMode(
  agentId: ExternalAgentId,
): AgentLaunchPermissionMode {
  return agentSupportsPermissionSkip(agentId) ? "skipPermissions" : "default";
}

export function agentPermissionSkipFlag(
  agentId: ExternalAgentId,
): string | undefined {
  if (agentId === "codex") {
    return "--dangerously-bypass-approvals-and-sandbox";
  }
  if (agentId === "claude") {
    return "--dangerously-skip-permissions";
  }
  return undefined;
}

/** 从持久化的实际启动参数恢复权限模式，旧会话缺少该参数时安全降级为普通模式。 */
export function agentSessionRecordPermissionMode(
  record: AgentSessionRecord,
): AgentLaunchPermissionMode {
  const agentId = agentSessionRecordAgentId(record);
  const flag = agentId ? agentPermissionSkipFlag(agentId) : undefined;
  if (!flag) {
    return "default";
  }

  return agentSessionLaunchContainsArg(record.session.launch, flag)
    ? "skipPermissions"
    : "default";
}

export function applyAgentLaunchPermissionMode(
  spec: ExternalAgentLaunchSpec,
  permissionMode: AgentLaunchPermissionMode,
): ExternalAgentLaunchSpec {
  if (permissionMode !== "skipPermissions") {
    return spec;
  }

  const flag = agentPermissionSkipFlag(spec.agentId);
  if (!flag) {
    return spec;
  }

  const args = spec.args ?? [];
  if (launchSpecContainsArg(spec, flag)) {
    return spec;
  }

  const wrappedCommand = agentLaunchWrappedCommand(spec.shell, args);
  if (wrappedCommand?.command.trim()) {
    const nextArgs = [...args];
    nextArgs[wrappedCommand.argIndex] = insertGlobalCliArg(
      wrappedCommand.command,
      flag,
    );
    return { ...spec, args: nextArgs };
  }

  return { ...spec, args: [flag, ...args] };
}

/** Kerminal 已审查并生成受管会话 hooks，因此仅为受管 Codex 启动跳过重复信任确认。 */
export function applyManagedAgentLaunchTrust(
  spec: ExternalAgentLaunchSpec,
): ExternalAgentLaunchSpec {
  const flag = "--dangerously-bypass-hook-trust";
  if (spec.agentId !== "codex" || launchSpecContainsArg(spec, flag)) {
    return spec;
  }

  const args = spec.args ?? [];
  const wrappedCommand = agentLaunchWrappedCommand(spec.shell, args);
  if (wrappedCommand?.command.trim()) {
    const nextArgs = [...args];
    nextArgs[wrappedCommand.argIndex] = insertGlobalCliArg(
      wrappedCommand.command,
      flag,
    );
    return { ...spec, args: nextArgs };
  }

  return { ...spec, args: [flag, ...args] };
}

export function agentLaunchDisplayCommand(
  spec: ExternalAgentLaunchSpec,
): string {
  const args = spec.args ?? [];
  const wrappedCommand = agentLaunchWrappedCommand(spec.shell, args);
  if (wrappedCommand?.command.trim()) {
    return wrappedCommand.command.trim();
  }
  return [spec.shell, ...args].join(" ").trim();
}

function resolveAgentDisabledReason(
  agent: ExternalAgentStatus,
  options: AgentActionOptions,
): string | undefined {
  if (!options.terminalLauncherAvailable) {
    return "当前无法打开终端。";
  }
  if (agent.id === "custom") {
    return undefined;
  }
  if (!agent.cliCommand.trim()) {
    return "尚未配置启动命令。";
  }
  return undefined;
}

/** 配置可在进入时生成，因此只把缺失 CLI 映射为安装提示。 */
function resolveAgentAvailability(
  agent: ExternalAgentStatus,
  disabledReason: string | undefined,
): {
  detail: string;
  label: AgentAvailabilityLabel;
} {
  if (disabledReason) {
    return {
      detail: disabledReason,
      label: "需设置",
    };
  }
  if (agent.id === "custom") {
    return agent.cliCommand.trim()
      ? { detail: "可直接打开。", label: "可用" }
      : { detail: "输入自定义命令后打开。", label: "需设置" };
  }
  if (!agent.installed) {
    return {
      detail: `${agent.title} 尚未安装。`,
      label: "需安装",
    };
  }
  if (!agent.configReady) {
    return {
      detail: "需要先完成必要设置，打开时会自动准备。",
      label: "需设置",
    };
  }
  return {
    detail: "可直接打开。",
    label: "可用",
  };
}

/** CLI 或受管配置未就绪时保持警告色，不使用旧 adapter 字段阻断原生 MCP。 */
function resolveAgentTone(
  agent: ExternalAgentStatus,
  disabledReason: string | undefined,
): AgentLauncherTone {
  if (disabledReason) {
    return agent.installed ? "danger" : "warning";
  }
  if (agent.id === "custom") {
    return "ready";
  }
  if (!agent.installed) {
    return "warning";
  }
  if (!agent.configReady) {
    return "warning";
  }
  return "ready";
}

function launchSpecContainsArg(
  spec: ExternalAgentLaunchSpec,
  flag: string,
): boolean {
  if ((spec.args ?? []).some((arg) => arg === flag)) {
    return true;
  }
  const wrappedCommand = agentLaunchWrappedCommand(spec.shell, spec.args ?? []);
  return wrappedCommand
    ? commandLineContainsExactArg(wrappedCommand.command, flag)
    : false;
}

function agentSessionLaunchContainsArg(
  launch: AgentSessionRecord["session"]["launch"],
  flag: string,
): boolean {
  return [
    launch.commandLabel,
    launch.command_label,
    ...launch.args,
  ].some(
    (argument) =>
      argument === flag ||
      (argument ? commandLineContainsExactArg(argument, flag) : false),
  );
}

function commandLineContainsExactArg(command: string, flag: string): boolean {
  try {
    return parseAgentCommandLine(command).args.includes(flag);
  } catch {
    return false;
  }
}

interface AgentLaunchWrappedCommand {
  argIndex: number;
  command: string;
}

function agentLaunchWrappedCommand(
  shell: string,
  args: string[],
): AgentLaunchWrappedCommand | undefined {
  if (isWindowsCmdLaunch(shell, args) && args[3]?.trim()) {
    return { argIndex: 3, command: args[3] };
  }

  const commandFlagIndex = windowsPowerShellCommandFlagIndex(shell, args);
  const command = args[commandFlagIndex + 1];
  return commandFlagIndex >= 0 && command?.trim()
    ? { argIndex: commandFlagIndex + 1, command }
    : undefined;
}

function isWindowsCmdLaunch(shell: string, args: string[]): boolean {
  const lowerShell = shell.toLowerCase();
  return (
    lowerShell.endsWith("cmd.exe") &&
    args.length >= 4 &&
    args[0].toLowerCase() === "/d" &&
    args[1].toLowerCase() === "/s" &&
    args[2].toLowerCase() === "/k"
  );
}

function windowsPowerShellCommandFlagIndex(
  shell: string,
  args: string[],
): number {
  const lowerShell = shell.toLowerCase();
  if (
    !lowerShell.endsWith("pwsh.exe") &&
    !lowerShell.endsWith("powershell.exe")
  ) {
    return -1;
  }
  return args.findIndex((arg) => {
    const lowerArg = arg.toLowerCase();
    return lowerArg === "-command" || lowerArg === "-c";
  });
}

function insertGlobalCliArg(command: string, flag: string): string {
  const trimmed = command.trim();
  if (!trimmed) {
    return flag;
  }

  const firstWhitespace = trimmed.search(/\s/);
  if (firstWhitespace === -1) {
    return `${trimmed} ${flag}`;
  }

  return `${trimmed.slice(0, firstWhitespace)} ${flag}${trimmed.slice(
    firstWhitespace,
  )}`;
}
