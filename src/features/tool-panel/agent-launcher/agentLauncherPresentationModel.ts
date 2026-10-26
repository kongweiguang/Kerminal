// @author kongweiguang

import type {
  AgentSessionScope,
  ExternalAgentId,
} from "../../../lib/agentLauncherApi";
import { redactSensitiveTechnicalDetail } from "../../../lib/userFacingMessage";
import {
  BUILTIN_CLAUDE_AGENT_KEY,
  BUILTIN_CODEX_AGENT_KEY,
  BUILTIN_PI_AGENT_KEY,
  customAgentLauncherKey,
  type CustomAgentDefinition,
} from "../../settings/contracts/index";
import {
  isTerminalSessionTab,
  type TerminalPane,
  type TerminalTab,
} from "../../workspace/contracts/index";
import {
  customAgentExecutableName,
  type AgentLauncherDescriptor,
} from "./agentLauncherSettingsModel";
import type { AgentActionViewModel } from "./agentLauncherModel";
import type { AgentSelectorOption } from "./AgentSelector";
import type { AgentSessionSelection } from "./agentSessionRestoreModel";
import {
  buildAgentSessionTarget,
  formatCurrentAgentTargetLabel,
  formatTargetChipLabel,
} from "./agentSessionTargetModel";
import { agentSessionScopeFromId } from "./agentTabSessionModel";

export type AgentLaunchSnapshot = Omit<AgentLauncherDescriptor, "launcherKey"> & {
  launcherKey?: string;
};

/** 列表保留短命令，悬停提供脱敏的完整配置命令，避免长参数挤压窄栏。 */
export function buildAgentSelectorOptions(
  agentActions: readonly AgentActionViewModel[],
  customAgents: readonly CustomAgentDefinition[],
): AgentSelectorOption[] {
  return [
    ...agentActions
      .filter((agent) => agent.agentId !== "custom")
      .map((agent) => ({
        agentId: agent.agentId,
        commandLabel: agent.cliCommand,
        launchCommand: redactSensitiveTechnicalDetail(agent.cliCommand),
        disabled: agent.disabled,
        disabledReason: agent.disabledReason,
        key: builtinAgentLauncherKey(agent.agentId) ?? BUILTIN_CODEX_AGENT_KEY,
        name: agent.title,
        statusDetail: agent.availabilityDetail,
        statusLabel: agent.availabilityLabel,
        tone:
          agent.tone === "danger" ? ("warning" as const) : agent.tone,
      })),
    ...customAgents.map((agent) => ({
      agentId: "custom" as const,
      commandLabel: customAgentExecutableName(agent.command),
      launchCommand: redactSensitiveTechnicalDetail(agent.command),
      disabled: false,
      key: customAgentLauncherKey(agent.id),
      name: agent.name,
      statusLabel: "已保存",
      tone: "ready" as const,
    })),
  ];
}

/** 将当前 scope 一次投影成标题、兼容 target 和显式权限范围，避免三处条件漂移。 */
export function buildAgentTargetPresentation(options: {
  activeAgentScope: AgentSessionScope;
  activeAgentViewScopeId: string;
  activeTab?: TerminalTab;
  effectiveFocusedPane?: TerminalPane;
  globalAgentScopeId: string;
  terminalPanes?: TerminalPane[];
}) {
  const global = options.activeAgentViewScopeId === options.globalAgentScopeId;
  const preferredTarget = buildAgentSessionTarget(
    options.effectiveFocusedPane,
    options.activeTab,
  );
  return {
    currentAgentScope: global
      ? ({ kind: "global" } as const)
      : agentSessionScopeFromId(options.activeAgentViewScopeId),
    currentAgentTarget: preferredTarget,
    currentAgentTargetLabel: global
      ? formatGlobalAgentTargetLabel(
          preferredTarget,
          options.effectiveFocusedPane,
          options.activeTab,
          options.terminalPanes,
        )
      : formatCurrentAgentTargetLabel(
          options.effectiveFocusedPane,
          options.activeTab,
          options.terminalPanes,
        ),
  };
}

/** 全局权限仍显示当前聚焦终端作为首选，避免用户误以为 target 被清空。 */
function formatGlobalAgentTargetLabel(
  preferredTarget: ReturnType<typeof buildAgentSessionTarget>,
  focusedPane?: TerminalPane,
  activeTab?: TerminalTab,
  terminalPanes?: readonly TerminalPane[],
): string {
  const preferredLabel =
    focusedPane?.title?.trim() ||
    activeTab?.title?.trim() ||
    (preferredTarget
      ? formatTargetChipLabel(preferredTarget)
      : isTerminalSessionTab(activeTab)
        ? formatCurrentAgentTargetLabel(focusedPane, activeTab, terminalPanes)
        : "无首选终端");
  return preferredLabel === "整个 Kerminal"
    ? preferredLabel
    : `整个 Kerminal · 首选 ${preferredLabel}`;
}

/** 历史恢复始终从 session 快照构造启动描述，不读取可能已编辑或删除的定义。 */
export function launcherSnapshotFromSelection(
  agentId: ExternalAgentId,
  selection: AgentSessionSelection,
): AgentLaunchSnapshot {
  return {
    agentId,
    customCommand: selection.customCommand,
    launcherKey:
      selection.launcherKey ??
      builtinAgentLauncherKey(agentId),
    title:
      selection.title ||
      (agentId === "codex"
        ? "Codex"
        : agentId === "claude"
          ? "Claude"
          : agentId === "pi"
            ? "PI Agent"
            : "Custom"),
  };
}

/** 只为原生内置 provider 返回稳定 key；Custom 必须保留其定义 UUID。 */
function builtinAgentLauncherKey(
  agentId: ExternalAgentId,
): string | undefined {
  if (agentId === "codex") {
    return BUILTIN_CODEX_AGENT_KEY;
  }
  if (agentId === "claude") {
    return BUILTIN_CLAUDE_AGENT_KEY;
  }
  return agentId === "pi" ? BUILTIN_PI_AGENT_KEY : undefined;
}
