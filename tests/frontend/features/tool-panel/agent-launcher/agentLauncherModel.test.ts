// @author kongweiguang
import { describe, expect, it } from "vitest";
import type {
  AgentSessionRecord,
  ExternalAgentLaunchSpec,
  ExternalAgentStatus,
  ExternalAgentWorkspaceStatus,
} from "../../../../../src/lib/agentLauncherApi";
import {
  agentLaunchDisplayCommand,
  agentPermissionSkipFlag,
  agentSessionRecordPermissionMode,
  agentSupportsPermissionSkip,
  applyAgentLaunchPermissionMode,
  applyManagedAgentLaunchTrust,
  buildAgentActionViewModel,
  buildAgentConfigSnippet,
  buildAgentLauncherViewModel,
  getMcpStatusView,
} from "../../../../../src/features/tool-panel/agent-launcher/agentLauncherModel";
import { parseAgentCommandLine } from "../../../../../src/lib/agentCommandLine";

const readyCodex: ExternalAgentStatus = {
  adapterAvailable: true,
  cliCommand: "codex",
  configPath: "C:/Users/me/.kerminal/.codex/config.toml",
  configReady: true,
  id: "codex",
  installed: true,
  statusDetail: "Codex CLI detected.",
  title: "Codex",
};

describe("agentLauncherModel", () => {
  it("describes installed agents with ready config", () => {
    const view = buildAgentActionViewModel(readyCodex, {
      mcpServerRunning: true,
      terminalLauncherAvailable: true,
    });

    expect(view.installLabel).toBe("Installed");
    expect(view.configLabel).toBe("Config ready");
    expect(view.actionLabel).toBe("Open Codex");
    expect(view.availabilityLabel).toBe("可用");
    expect(view.availabilityDetail).toBe("可直接打开。");
    expect(view.disabled).toBe(false);
    expect(view.statusDetail).toBe("Codex CLI detected.");
    expect(view.tone).toBe("ready");
  });

  it("keeps missing CLIs launchable so the terminal owns startup feedback", () => {
    const view = buildAgentActionViewModel(
      {
        ...readyCodex,
        cliCommand: "claude",
        configReady: false,
        id: "claude",
        installed: false,
        statusDetail: "",
        title: "Claude",
      },
      {
        mcpServerRunning: true,
        terminalLauncherAvailable: true,
      },
    );

    expect(view.installLabel).toBe("Missing CLI");
    expect(view.configLabel).toBe("Config needs update");
    expect(view.actionLabel).toBe("Prepare & Open");
    expect(view.availabilityLabel).toBe("需安装");
    expect(view.availabilityDetail).toBe("Claude 尚未安装。");
    expect(view.disabled).toBe(false);
    expect(view.disabledReason).toBeUndefined();
    expect(view.tone).toBe("warning");
  });

  it("keeps config repair launchable when the provider is installed", () => {
    const view = buildAgentActionViewModel(
      {
        ...readyCodex,
        configReady: false,
        statusDetail: "",
      },
      {
        mcpServerRunning: true,
        terminalLauncherAvailable: true,
      },
    );

    expect(view.actionLabel).toBe("Prepare & Open");
    expect(view.availabilityLabel).toBe("需设置");
    expect(view.availabilityDetail).toContain("必要设置");
    expect(view.disabled).toBe(false);
    expect(view.configLabel).toBe("Config needs update");
  });

  it("treats Custom as an explicit command without default config files", () => {
    const view = buildAgentActionViewModel(
      {
        ...readyCodex,
        cliCommand: "",
        configPath: "",
        configReady: false,
        id: "custom",
        installed: false,
        statusDetail: "",
        title: "Custom",
      },
      {
        mcpServerRunning: true,
        terminalLauncherAvailable: true,
      },
    );

    expect(view.configLabel).toBe("Enter command");
    expect(view.configPath).toBe("User supplied CLI");
    expect(view.availabilityLabel).toBe("需设置");
    expect(view.availabilityDetail).toBe("输入自定义命令后打开。");
    expect(view.disabledReason).toBeUndefined();
    expect(view.disabled).toBe(false);
  });

  it("keeps installed Codex and Claude launchable when MCP is stopped", () => {
    const view = buildAgentActionViewModel(readyCodex, {
      mcpServerRunning: false,
      terminalLauncherAvailable: true,
    });

    expect(view.actionLabel).toBe("Start & Open Codex");
    expect(view.availabilityLabel).toBe("可用");
    expect(view.disabled).toBe(false);
    expect(view.disabledReason).toBeUndefined();
    expect(view.statusDetail).toBe(
      "Kerminal MCP Server will be started before launch.",
    );
    expect(view.tone).toBe("ready");
  });

  it("keeps diagnostic fields without leaking them into short availability copy", () => {
    const view = buildAgentActionViewModel(
      {
        ...readyCodex,
        configPath: "C:/internal/workspace/.codex/config.toml",
        statusDetail:
          "command=codex endpoint=http://127.0.0.1:37657/mcp workspaceId=ws-42",
      },
      {
        mcpServerRunning: true,
        terminalLauncherAvailable: true,
      },
    );

    expect(view.configPath).toContain(".codex/config.toml");
    expect(view.statusDetail).toContain("endpoint=");
    expect(`${view.availabilityLabel} ${view.availabilityDetail}`).toBe(
      "可用 可直接打开。",
    );
    expect(`${view.availabilityLabel} ${view.availabilityDetail}`).not.toMatch(
      /command=|endpoint=|workspaceId|config\.toml/i,
    );
  });

  it("orders Codex, Claude, and Custom cards from workspace status", () => {
    const status: ExternalAgentWorkspaceStatus = {
      agents: {
        claude: {
          ...readyCodex,
          cliCommand: "claude",
          id: "claude",
          title: "Claude",
        },
        codex: readyCodex,
        custom: {
          ...readyCodex,
          cliCommand: "custom-agent",
          id: "custom",
          title: "Custom",
        },
        pi: {
          ...readyCodex,
          cliCommand: "pi --approve",
          configPath: "C:/Users/me/.kerminal/.pi/mcp.json",
          id: "pi",
          title: "PI Agent",
        },
      },
      mcpEndpoint: "http://127.0.0.1:37657/mcp",
      mcpServerRunning: true,
      workspaceDir: "C:/Users/me/.kerminal",
    };

    expect(
      buildAgentLauncherViewModel(status, true).map((view) => view.agentId),
    ).toEqual(["codex", "claude", "pi", "custom"]);
  });

  // 旧后端的 adapter 字段为 false 时也不能误导原生 MCP 用户安装插件。
  it("uses native PI MCP without requiring the legacy adapter", () => {
    const view = buildAgentActionViewModel(
      {
        ...readyCodex,
        adapterAvailable: false,
        cliCommand: "pi --approve",
        configPath: "C:/Users/me/.kerminal/.pi/mcp.json",
        id: "pi",
        title: "PI Agent",
      },
      {
        mcpServerRunning: true,
        terminalLauncherAvailable: true,
      },
    );

    expect(view.availabilityLabel).toBe("可用");
    expect(view.availabilityDetail).toBe("可直接打开。");
    expect(view.installLabel).toBe("Installed");
    expect(view.configPath).toBe("C:/Users/me/.kerminal/.pi/mcp.json");
    expect(view.tone).toBe("ready");
  });

  it("builds MCP status and copyable config snippets from endpoint", () => {
    expect(
      getMcpStatusView({
        mcpEndpoint: "http://127.0.0.1:37657/mcp",
        mcpServerRunning: true,
      }),
    ).toMatchObject({
      label: "Running",
      tone: "ready",
    });
    expect(
      buildAgentConfigSnippet({
        mcpEndpoint: "http://127.0.0.1:37657/mcp",
      }),
    ).toContain('url = "http://127.0.0.1:37657/mcp"');
  });

  it("applies provider permission skip flags without changing default launches", () => {
    const codexSpec: ExternalAgentLaunchSpec = {
      agentId: "codex",
      agentSessionId: "ags-codex",
      args: [
        "-NoLogo",
        "-NoProfile",
        "-NoExit",
        "-Command",
        "codex resume --last",
      ],
      cwd: "C:/Users/me/.kerminal/agents/sessions/ags-codex",
      message: "Codex workspace prepared.",
      shell: "pwsh.exe",
      title: "Codex",
    };
    const cmdCodexSpec: ExternalAgentLaunchSpec = {
      ...codexSpec,
      args: ["/d", "/s", "/k", "codex resume --last"],
      shell: "cmd.exe",
    };
    const claudeSpec: ExternalAgentLaunchSpec = {
      agentId: "claude",
      agentSessionId: "ags-claude",
      args: ["--permission-mode", "default"],
      cwd: "/home/me/.kerminal/agents/sessions/ags-claude",
      message: "Claude workspace prepared.",
      shell: "claude",
      title: "Claude",
    };

    expect(agentSupportsPermissionSkip("codex")).toBe(true);
    expect(agentSupportsPermissionSkip("claude")).toBe(true);
    expect(agentSupportsPermissionSkip("pi")).toBe(false);
    expect(agentSupportsPermissionSkip("custom")).toBe(false);
    expect(agentPermissionSkipFlag("pi")).toBeUndefined();
    expect(agentPermissionSkipFlag("custom")).toBeUndefined();
    expect(applyAgentLaunchPermissionMode(codexSpec, "default")).toBe(
      codexSpec,
    );
    expect(
      applyAgentLaunchPermissionMode(codexSpec, "skipPermissions").args,
    ).toEqual([
      "-NoLogo",
      "-NoProfile",
      "-NoExit",
      "-Command",
      "codex --dangerously-bypass-approvals-and-sandbox resume --last",
    ]);
    expect(agentLaunchDisplayCommand(codexSpec)).toBe("codex resume --last");
    expect(
      applyAgentLaunchPermissionMode(cmdCodexSpec, "skipPermissions").args,
    ).toEqual([
      "/d",
      "/s",
      "/k",
      "codex --dangerously-bypass-approvals-and-sandbox resume --last",
    ]);
    expect(
      applyAgentLaunchPermissionMode(claudeSpec, "skipPermissions").args,
    ).toEqual([
      "--dangerously-skip-permissions",
      "--permission-mode",
      "default",
    ]);
  });

  it("restores skip permissions only from an exact persisted provider flag", () => {
    const persistedCodex: AgentSessionRecord = {
      session: {
        agentId: "codex",
        agentSessionId: "ags-codex",
        launch: {
          args: [
            "-NoLogo",
            "-Command",
            "codex --dangerously-bypass-approvals-and-sandbox resume --last",
          ],
          commandLabel:
            "codex --dangerously-bypass-approvals-and-sandbox resume --last",
          cwd: "C:/Users/me/.kerminal/agents/sessions/ags-codex",
          shell: "pwsh.exe",
        },
        title: "Codex",
      },
    };

    expect(agentSessionRecordPermissionMode(persistedCodex)).toBe(
      "skipPermissions",
    );
    expect(
      agentSessionRecordPermissionMode({
        session: {
          ...persistedCodex.session,
          launch: {
            ...persistedCodex.session.launch,
            args: [
              "-Command",
              "codex --dangerously-bypass-approvals-and-sandboxed resume --last",
            ],
            commandLabel:
              "codex --dangerously-bypass-approvals-and-sandboxed resume --last",
          },
        },
      }),
    ).toBe("default");
    expect(
      agentSessionRecordPermissionMode({
        session: {
          ...persistedCodex.session,
          agentId: "custom",
        },
      }),
    ).toBe("default");
  });

  it("bypasses hook trust only for Kerminal-managed Codex launches", () => {
    const directSpec: ExternalAgentLaunchSpec = {
      agentId: "codex",
      agentSessionId: "ags-codex",
      args: ["resume", "--last"],
      cwd: "C:/Users/me/.kerminal/agents/sessions/ags-codex",
      message: "Codex workspace prepared.",
      shell: "codex",
      title: "Codex",
    };
    const wrappedSpec: ExternalAgentLaunchSpec = {
      ...directSpec,
      args: [
        "-NoLogo",
        "-NoProfile",
        "-NoExit",
        "-Command",
        "codex resume --last",
      ],
      shell: "pwsh.exe",
    };
    const cmdSpec: ExternalAgentLaunchSpec = {
      ...directSpec,
      args: ["/d", "/s", "/k", "codex resume --last"],
      shell: "cmd.exe",
    };
    const claudeSpec: ExternalAgentLaunchSpec = {
      ...directSpec,
      agentId: "claude",
      shell: "claude",
      title: "Claude",
    };

    expect(applyManagedAgentLaunchTrust(directSpec).args).toEqual([
      "--dangerously-bypass-hook-trust",
      "resume",
      "--last",
    ]);
    expect(
      agentLaunchDisplayCommand(applyManagedAgentLaunchTrust(wrappedSpec)),
    ).toBe("codex --dangerously-bypass-hook-trust resume --last");
    expect(
      agentLaunchDisplayCommand(applyManagedAgentLaunchTrust(cmdSpec)),
    ).toBe("codex --dangerously-bypass-hook-trust resume --last");
    expect(applyManagedAgentLaunchTrust(claudeSpec)).toBe(claudeSpec);
    expect(
      applyManagedAgentLaunchTrust(applyManagedAgentLaunchTrust(directSpec))
        .args,
    ).toEqual(["--dangerously-bypass-hook-trust", "resume", "--last"]);
  });

  it("parses custom agent command lines into shell and args", () => {
    expect(parseAgentCommandLine('qwen --model "qwen max"')).toEqual({
      args: ["--model", "qwen max"],
      shell: "qwen",
    });
    expect(parseAgentCommandLine("C:\\Tools\\kimi.exe --fast")).toEqual({
      args: ["--fast"],
      shell: "C:\\Tools\\kimi.exe",
    });
    expect(
      parseAgentCommandLine('"C:\\Program Files\\Kimi\\kimi.exe" --fast'),
    ).toEqual({
      args: ["--fast"],
      shell: "C:\\Program Files\\Kimi\\kimi.exe",
    });
    expect(() => parseAgentCommandLine("   ")).toThrow(
      "Enter a command to launch a custom agent.",
    );
  });
});
