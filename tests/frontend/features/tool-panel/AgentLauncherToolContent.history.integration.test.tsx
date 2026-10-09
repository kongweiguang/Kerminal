// @author kongweiguang
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type {
  AgentSessionRecord,
  ExternalAgentId,
  ExternalAgentWorkspaceStatus,
} from "../../../../src/lib/agentLauncherApi";
import { defaultAppSettings } from "../../../../src/features/settings/settingsModel";
import { AgentLauncherToolContent } from "../../../../src/features/tool-panel/AgentLauncherToolContent";
import { unregisterTestTerminalPaneSessions } from "../../support/terminalSessionRegistry.testSupport";

const apiMocks = vi.hoisted(() => ({
  archiveAgentSession: vi.fn(),
  createAgentSession: vi.fn(),
  getExternalAgentWorkspaceStatus: vi.fn(),
  listAgentSessions: vi.fn(),
  prepareExternalAgentWorkspace: vi.fn(),
  rebindAgentSessionTarget: vi.fn(),
  updateAgentSession: vi.fn(),
}));

const terminalMocks = vi.hoisted(() => ({
  renderXtermPane: vi.fn(),
}));

const notificationMocks = vi.hoisted(() => ({
  currentDesktopNotificationVisibility: vi.fn(),
  sendDesktopNotification: vi.fn(),
}));

vi.mock("../../../../src/lib/agentLauncherApi", () => ({
  archiveAgentSession: (...args: unknown[]) =>
    apiMocks.archiveAgentSession(...args),
  agentSessionRecordAgentId: (record: {
    session: { agentId?: string; agent_id?: string };
  }) => record.session.agentId ?? record.session.agent_id,
  agentSessionRecordId: (record: {
    session: { agentSessionId?: string; agent_session_id?: string };
  }) => record.session.agentSessionId ?? record.session.agent_session_id,
  agentSessionRecordLaunchCommand: (record: {
    session: {
      launch: {
        args: string[];
        commandLabel?: string;
        command_label?: string;
        shell: string;
      };
    };
  }) =>
    record.session.launch.commandLabel ??
    record.session.launch.command_label ??
    [record.session.launch.shell, ...record.session.launch.args].join(" ").trim(),
  agentSessionRecordLauncherKey: (record: {
    session: { launcherKey?: string; launcher_key?: string };
  }) => record.session.launcherKey ?? record.session.launcher_key,
  agentSessionRecordStatus: (record: { session: { status?: string } }) =>
    record.session.status ?? "active",
  agentSessionRecordTarget: (record: { session: { target?: unknown } }) =>
    record.session.target,
  createAgentSession: (...args: unknown[]) =>
    apiMocks.createAgentSession(...args),
  getExternalAgentWorkspaceStatus: (...args: unknown[]) =>
    apiMocks.getExternalAgentWorkspaceStatus(...args),
  listAgentSessions: (...args: unknown[]) =>
    apiMocks.listAgentSessions(...args),
  prepareExternalAgentWorkspace: (...args: unknown[]) =>
    apiMocks.prepareExternalAgentWorkspace(...args),
  rebindAgentSessionTarget: (...args: unknown[]) =>
    apiMocks.rebindAgentSessionTarget(...args),
  updateAgentSession: (...args: unknown[]) =>
    apiMocks.updateAgentSession(...args),
}));

vi.mock("../../../../src/lib/fileDialogApi", () => ({
  openLocalDirectory: vi.fn(),
}));

vi.mock("../../../../src/lib/desktopNotificationApi", () => ({
  currentDesktopNotificationVisibility: () =>
    notificationMocks.currentDesktopNotificationVisibility(),
  sendDesktopNotification: (...args: unknown[]) =>
    notificationMocks.sendDesktopNotification(...args),
}));

vi.mock("../../../../src/features/terminal/XtermPane", () => ({
  XtermPane: (props: {
    args?: string[];
    cwd?: string;
    focused?: boolean;
    inputCompatibilityMode?: string;
    paneId?: string;
    shell?: string;
    shellAssistEnabled?: boolean;
    startupMessage?: string;
    title: string;
    transientStartupMessage?: boolean;
  }) => {
    terminalMocks.renderXtermPane(props);
    return (
      <div
        data-args={(props.args ?? []).join(" ")}
        data-cwd={props.cwd}
        data-focused={String(props.focused)}
        data-input-compatibility-mode={props.inputCompatibilityMode}
        data-pane-id={props.paneId}
        data-shell={props.shell}
        data-shell-assist-enabled={String(props.shellAssistEnabled)}
        data-startup-message={props.startupMessage}
        data-testid="agent-xterm"
        data-transient-startup-message={String(props.transientStartupMessage)}
      >
        {props.title}
      </div>
    );
  },
}));

describe("AgentLauncherToolContent history refresh", () => {
  beforeEach(() => {
    apiMocks.archiveAgentSession.mockReset();
    apiMocks.createAgentSession.mockReset();
    apiMocks.getExternalAgentWorkspaceStatus.mockReset();
    apiMocks.listAgentSessions.mockReset();
    apiMocks.prepareExternalAgentWorkspace.mockReset();
    apiMocks.rebindAgentSessionTarget.mockReset();
    apiMocks.updateAgentSession.mockReset();
    terminalMocks.renderXtermPane.mockClear();
    notificationMocks.currentDesktopNotificationVisibility.mockReset();
    notificationMocks.sendDesktopNotification.mockReset();
    notificationMocks.currentDesktopNotificationVisibility.mockReturnValue(
      "hidden",
    );
    notificationMocks.sendDesktopNotification.mockResolvedValue({
      reason: "will-send",
      requestedPermission: false,
      sent: true,
    });
    unregisterTestTerminalPaneSessions();
    apiMocks.getExternalAgentWorkspaceStatus.mockResolvedValue(workspaceStatus());
    apiMocks.listAgentSessions.mockResolvedValue({ diagnostics: [], sessions: [] });
    apiMocks.updateAgentSession.mockImplementation(
      async (agentSessionId: string, request: { title?: string }) => ({
        session: {
          agentId: "codex",
          agentSessionId,
          launch: { args: [], cwd: "", shell: "codex" },
          status: "active",
          title: request.title ?? "Codex",
        },
      }),
    );
    apiMocks.prepareExternalAgentWorkspace.mockImplementation(
      async (request: {
        agentId: ExternalAgentId;
        agentSessionId: string;
        customCommand?: string;
      }) => ({
        agentId: request.agentId,
        agentSessionId: request.agentSessionId,
        args: [
          "-NoLogo",
          "-NoProfile",
          "-NoExit",
          "-Command",
          request.customCommand ?? request.agentId,
        ],
        cwd: `C:/Users/me/.kerminal/agents/sessions/${request.agentSessionId}`,
        env: {
          KERMINAL_AGENT_SESSION_ID: request.agentSessionId,
          KERMINAL_MCP_ENDPOINT: `http://127.0.0.1:37657/mcp/agents/${request.agentSessionId}`,
        },
        message: "Agent workspace prepared.",
        shell: "pwsh.exe",
        title: request.agentId === "custom" ? "Custom" : "Codex",
      }),
    );
  });

  afterEach(() => {
    vi.clearAllMocks();
  });

  it("refreshes workflow history once after every successful Custom launch", async () => {
    const user = userEvent.setup();
    const persistedSessions: AgentSessionRecord[] = [
      {
        session: {
          agentId: "custom",
          agentSessionId: "ags-initial-pi",
          launcherKey: "custom:33333333-3333-4333-8333-333333333333",
          launch: {
            args: [],
            commandLabel: "pi --initial",
            cwd: "C:/Users/me/.kerminal/agents/sessions/ags-initial-pi",
            shell: "pi",
          },
          scope: { kind: "tab", tabId: "tab-main" },
          status: "active",
          title: "Initial PI",
        },
      },
    ];
    apiMocks.listAgentSessions.mockImplementation(async () => ({
      diagnostics: [],
      sessions: [...persistedSessions],
    }));
    let nextSessionIndex = 0;
    apiMocks.createAgentSession.mockImplementation(
      async ({
        agentId,
        launcherKey,
        scope,
        title,
      }: {
        agentId: ExternalAgentId;
        launcherKey?: string;
        scope?: AgentSessionRecord["session"]["scope"];
        title?: string;
      }) => {
        nextSessionIndex += 1;
        const agentSessionId = `ags-custom-${nextSessionIndex}`;
        const record: AgentSessionRecord = {
          session: {
            agentId,
            agentSessionId,
            launcherKey,
            launch: {
              args: [],
              commandLabel: agentId,
              cwd: `C:/Users/me/.kerminal/agents/sessions/${agentSessionId}`,
              shell: agentId,
            },
            scope,
            status: "active",
            title: title ?? "Custom",
          },
        };
        persistedSessions.push(record);
        return record;
      },
    );
    renderAgentLauncher({
      settings: {
        ...defaultAppSettings,
        agentLauncher: {
          customAgents: [
            {
              command: "pi --alpha",
              id: "11111111-1111-4111-8111-111111111111",
              name: "Agent Alpha",
            },
            {
              command: "pi --beta",
              id: "22222222-2222-4222-8222-222222222222",
              name: "Agent Beta",
            },
          ],
          selectedAgentKey: "custom:11111111-1111-4111-8111-111111111111",
        },
      },
    });

    expect(
      await screen.findByRole("button", { name: "当前目标 1" }),
    ).toBeInTheDocument();
    await waitFor(() => {
      expect(apiMocks.listAgentSessions).toHaveBeenCalledTimes(2);
    });

    await launchAgent(user, "Agent Alpha");
    await user.click(
      await screen.findByRole("button", { name: "Back to agent launcher" }),
    );
    expect(
      await screen.findByRole("button", { name: "当前目标 2" }),
    ).toBeInTheDocument();
    expect(apiMocks.listAgentSessions).toHaveBeenCalledTimes(5);

    await launchAgent(user, "Agent Beta");
    await user.click(
      await screen.findByRole("button", { name: "Back to agent launcher" }),
    );
    expect(
      await screen.findByRole("button", { name: "当前目标 3" }),
    ).toBeInTheDocument();
    expect(apiMocks.listAgentSessions).toHaveBeenCalledTimes(8);
  });
});

/** 用用户可见选择器切换条目，再从分裂按钮主动作启动。 */
async function launchAgent(
  user: ReturnType<typeof userEvent.setup>,
  name: "Agent Alpha" | "Agent Beta",
) {
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

/** 以稳定本地 Tab fixture 验证历史计数，避免 pane 目标差异干扰会话语义。 */
function terminalTab(id: string) {
  return {
    id,
    layout: { paneId: `pane-${id}`, type: "pane" },
    machineId: "local",
    title: id,
  } as never;
}

/** 历史恢复复用原生 Pi 状态，防止旧 adapter fixture 掩盖启动配置回归。 */
function workspaceStatus(): ExternalAgentWorkspaceStatus {
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

function renderAgentLauncher(
  props: Partial<Parameters<typeof AgentLauncherToolContent>[0]> = {},
) {
  return render(
    <AgentLauncherToolContent
      activeTab={terminalTab("tab-main")}
      onConfirmedSettingsChange={async (nextSettings) => nextSettings}
      {...props}
    />,
  );
}
