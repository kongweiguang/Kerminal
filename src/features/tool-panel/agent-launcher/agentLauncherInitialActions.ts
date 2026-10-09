// @author kongweiguang

import type { AgentActionViewModel } from "./agentLauncherModel";

/** Agent 状态 API 返回前使用的稳定占位动作，不承担运行态可用性判断。 */
export const initialAgentActions: AgentActionViewModel[] = [
  {
    actionLabel: "Open Codex",
    agentId: "codex",
    availabilityDetail: "正在检查 Codex 状态。",
    availabilityLabel: "需设置",
    cliCommand: "codex",
    configLabel: "Workspace",
    configPath: "~/.kerminal/.codex/config.toml",
    disabled: false,
    installLabel: "Launch",
    statusDetail: "Open Codex in the Kerminal workspace.",
    title: "Codex",
    tone: "muted",
  },
  {
    actionLabel: "Open Claude",
    agentId: "claude",
    availabilityDetail: "正在检查 Claude 状态。",
    availabilityLabel: "需设置",
    cliCommand: "claude",
    configLabel: "Workspace",
    configPath: "~/.kerminal/.mcp.json",
    disabled: false,
    installLabel: "Launch",
    statusDetail: "Open Claude in the Kerminal workspace.",
    title: "Claude",
    tone: "muted",
  },
  {
    actionLabel: "Open PI Agent",
    agentId: "pi",
    availabilityDetail: "正在检查 PI Agent 与原生 MCP 配置。",
    availabilityLabel: "需设置",
    cliCommand: "pi --approve",
    configLabel: "Workspace",
    configPath: "~/.kerminal/.pi/mcp.json",
    disabled: false,
    installLabel: "Launch",
    statusDetail: "Open PI Agent in the Kerminal workspace.",
    title: "PI Agent",
    tone: "muted",
  },
  {
    actionLabel: "Open Custom Agent",
    agentId: "custom",
    availabilityDetail: "输入自定义命令后打开。",
    availabilityLabel: "需设置",
    cliCommand: "User supplied CLI",
    configLabel: "Explicit command",
    configPath: "~/.kerminal",
    disabled: false,
    installLabel: "Launch",
    statusDetail:
      "Enter a custom CLI command to run in the Kerminal workspace.",
    title: "Custom",
    tone: "muted",
  },
];
