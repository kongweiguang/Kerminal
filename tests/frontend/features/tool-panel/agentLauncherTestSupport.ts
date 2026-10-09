// @author kongweiguang

import { screen } from "@testing-library/react";
import type { ExternalAgentWorkspaceStatus } from "../../../../src/lib/agentLauncherApi";

interface AgentLauncherTestUser {
  click(element: Element): Promise<unknown>;
}

/** 通过公开选择器和主“进入”按钮启动，避免测试耦合已移除的三宫格结构。 */
export async function launchAgent(
  user: AgentLauncherTestUser,
  name: string,
): Promise<void> {
  const selector = screen.getByRole("combobox", { name: "选择 Agent" });
  if (selector.getAttribute("aria-valuetext") !== name) {
    await user.click(selector);
    await user.click(
      await screen.findByRole("option", {
        name: new RegExp(`^${name}，`, "u"),
      }),
    );
  }
  await user.click(screen.getByRole("button", { name: `使用 ${name} 进入` }));
}

/** 共用状态契约，Pi 使用原生配置而非 adapter，避免测试继续认可旧启动路径。 */
export function workspaceStatus(): ExternalAgentWorkspaceStatus {
  return {
    agents: {
      claude: {
        adapterAvailable: true,
        cliCommand: "claude",
        configPath: "C:/Users/me/.kerminal/.mcp.json",
        configReady: false,
        id: "claude",
        installed: true,
        statusDetail: "Claude CLI detected. MCP config needs refresh.",
        title: "Claude",
      },
      codex: {
        adapterAvailable: true,
        cliCommand: "codex",
        configPath: "C:/Users/me/.kerminal/.codex/config.toml",
        configReady: true,
        id: "codex",
        installed: true,
        statusDetail: "Codex CLI detected.",
        title: "Codex",
      },
      custom: {
        adapterAvailable: true,
        cliCommand: "",
        configPath: "",
        configReady: false,
        id: "custom",
        installed: false,
        statusDetail: "Configure a custom agent command first.",
        title: "Custom",
      },
      pi: {
        adapterAvailable: true,
        cliCommand: "pi --approve",
        configPath: "C:/Users/me/.kerminal/.pi/mcp.json",
        configReady: true,
        id: "pi",
        installed: true,
        statusDetail: "PI Agent CLI detected. Native MCP config ready.",
        title: "PI Agent",
      },
    },
    mcpEndpoint: "http://127.0.0.1:37657/mcp",
    mcpServerRunning: true,
    workspaceDir: "C:/Users/me/.kerminal",
  };
}
