// @author kongweiguang
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ExternalAgentId, ExternalAgentWorkspaceStatus } from "../../../../src/lib/agentLauncherApi";
import { unregisterTestTerminalPaneSessions } from "../../support/terminalSessionRegistry.testSupport";
import { AgentLauncherToolContent } from "../../../../src/features/tool-panel/AgentLauncherToolContent";

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
  agentSessionRecordId: (record: { session: { agentSessionId?: string; agent_session_id?: string } }) =>
    record.session.agentSessionId ?? record.session.agent_session_id,
  agentSessionRecordLaunchCommand: (record: {
    session: { launch: { commandLabel?: string; command_label?: string; shell: string; args: string[] } };
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
    onAgentSignal?: (signal: {
      agent: "codex" | "claude" | "gemini";
      agentSessionId?: string;
      status: "working" | "attention" | "finished" | "exited";
      terminalSessionId: string;
    }) => void;
    onSessionFinished?: (event: { durationMs: number; sessionId: string }) => void;
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
vi.mock("../../../../src/features/logs/LogToolContent", () => ({
  LogToolContent: () => <div data-testid="logs-tool">Logs tool</div>,
}));

describe("AgentLauncherToolContent", () => {
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
    apiMocks.listAgentSessions.mockResolvedValue({
      diagnostics: [],
      sessions: [],
    });
    apiMocks.archiveAgentSession.mockResolvedValue({
      session: {
        agentSessionId: "ags-archived",
        launch: { args: [], cwd: "", shell: "" },
        status: "archived",
        title: "Archived",
      },
    });
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
    apiMocks.createAgentSession.mockImplementation(
      async ({
        agentId,
        launcherKey,
        scope,
        target,
        title,
      }: {
        agentId: string;
        launcherKey?: string;
        scope?: unknown;
        target?: unknown;
        title?: string;
      }) => ({
        session: {
          agentId,
          agentSessionId: `ags-${agentId}`,
          launcherKey,
          launch: {
            args: [],
            commandLabel: agentId,
            cwd: `C:/Users/me/.kerminal/agents/sessions/ags-${agentId}`,
            shell: agentId,
          },
          sessionRoot: `C:/Users/me/.kerminal/agents/sessions/ags-${agentId}`,
          scope,
          target,
          title: title ?? (agentId === "claude" ? "Claude" : agentId === "custom" ? "Custom" : "Codex"),
          workspaceRoot: "C:/Users/me/.kerminal",
        },
      }),
    );
    apiMocks.prepareExternalAgentWorkspace.mockImplementation(
      async (request: {
        agentId: ExternalAgentId;
        agentSessionId: string;
        customCommand?: string;
        resumeProviderSession?: boolean;
      }) => {
        const command =
          request.customCommand ??
          (request.resumeProviderSession && request.agentId === "claude"
            ? "claude --continue"
            : request.agentId);
        const title =
          request.agentId === "claude"
            ? "Claude"
            : request.agentId === "custom"
              ? "Custom"
              : "Codex";
        return {
          agentId: request.agentId,
          agentSessionId: request.agentSessionId,
          args: [
            "-NoLogo",
            "-NoProfile",
            "-NoExit",
            "-Command",
            command,
          ],
          cwd: `C:/Users/me/.kerminal/agents/sessions/${request.agentSessionId}`,
          env: {
            KERMINAL_AGENT_SESSION_ID: request.agentSessionId,
            KERMINAL_MCP_ENDPOINT: `http://127.0.0.1:37657/mcp/agents/${request.agentSessionId}`,
          },
          message: `${title} workspace prepared.`,
          shell: "pwsh.exe",
          title,
        };
      },
    );
    apiMocks.rebindAgentSessionTarget.mockImplementation(
      async (agentSessionId: string, target: unknown) => ({
        session: {
          agentId: "codex",
          agentSessionId,
          launch: { args: [], cwd: "", shell: "codex" },
          scope: { kind: "global" },
          status: "active",
          target,
          title: "Codex",
        },
      }),
    );
  });

  afterEach(() => {
    vi.clearAllMocks();
  });

  it("continues a persisted Claude agent session", async () => {
    const user = userEvent.setup();
    apiMocks.listAgentSessions.mockResolvedValue({
      diagnostics: [],
      sessions: [
        {
          session: {
            agentId: "claude",
            agentSessionId: "ags-restored-claude",
            launch: {
              args: [],
              commandLabel: "claude",
              cwd: "C:/Users/me/.kerminal/agents/sessions/ags-restored-claude",
              shell: "claude",
            },
            sessionRoot: "C:/Users/me/.kerminal/agents/sessions/ags-restored-claude",
            status: "active",
            scope: { kind: "tab", tabId: "tab-main" },
            title: "Claude",
            workspaceRoot: "C:/Users/me/.kerminal",
          },
        },
      ],
    });

    renderAgentLauncher();

    await launchAgent(user, "Claude");
    await user.click(await screen.findByRole("button", { name: "继续上次" }));

    await waitFor(() => {
      expect(apiMocks.createAgentSession).not.toHaveBeenCalled();
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledWith({
        agentId: "claude",
        agentSessionId: "ags-restored-claude",
        resumeProviderSession: true,
      });
    });
    expect(await screen.findByTestId("agent-xterm")).toHaveTextContent("Claude");
    expect(screen.getByTestId("agent-xterm")).toHaveAttribute(
      "data-args",
      "-NoLogo -NoProfile -NoExit -Command claude --dangerously-skip-permissions --continue",
    );
    expect(screen.getByTestId("agent-terminal-command")).toHaveTextContent(
      "claude --dangerously-skip-permissions --continue · C:/Users/me/.kerminal/agents/sessions/ags-restored-claude",
    );
  });

  it("creates a fresh provider session when the restore choice selects new session", async () => {
    const user = userEvent.setup();
    apiMocks.listAgentSessions.mockResolvedValue({
      diagnostics: [],
      sessions: [
        {
          session: {
            agentId: "codex",
            agentSessionId: "ags-restored-codex",
            launch: {
              args: [],
              commandLabel: "codex",
              cwd: "C:/Users/me/.kerminal/agents/sessions/ags-restored-codex",
              shell: "codex",
            },
            sessionRoot: "C:/Users/me/.kerminal/agents/sessions/ags-restored-codex",
            status: "active",
            scope: { kind: "tab", tabId: "tab-main" },
            title: "Codex",
            workspaceRoot: "C:/Users/me/.kerminal",
          },
        },
      ],
    });

    renderAgentLauncher();

    await launchAgent(user, "Codex");
    await user.click(await screen.findByRole("button", { name: "新会话" }));

    await waitFor(() => {
      expect(apiMocks.createAgentSession).toHaveBeenCalledWith({
        agentId: "codex",
        launcherKey: "builtin:codex",
        scope: { kind: "tab", tabId: "tab-main" },
        title: "Codex · 当前 Tab · 1 个终端 · tab-main",
      });
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledWith({
        agentId: "codex",
        agentSessionId: "ags-codex",
        resumeProviderSession: false,
      });
    });
    expect(await screen.findByTestId("agent-xterm")).toHaveAttribute(
      "data-cwd",
      "C:/Users/me/.kerminal/agents/sessions/ags-codex",
    );
  });

  it("shows stale persisted agent target as invalid after restore", async () => {
    const user = userEvent.setup();
    apiMocks.listAgentSessions.mockResolvedValue({
      diagnostics: [],
      sessions: [
        {
          session: {
            agentId: "codex",
            agentSessionId: "ags-stale-codex",
            launch: {
              args: [],
              commandLabel: "codex",
              cwd: "C:/Users/me/.kerminal/agents/sessions/ags-stale-codex",
              shell: "codex",
            },
            sessionRoot: "C:/Users/me/.kerminal/agents/sessions/ags-stale-codex",
            scope: { kind: "tab", tabId: "tab-old" },
            target: {
              cwd: "/srv/app",
              liveStatus: "stale",
              paneId: "pane-old",
              shell: "bash",
              tabId: "tab-old",
              targetKind: "ssh",
              targetRef: "ssh:prod-web",
              targetTerminalSessionId: "term-old",
            },
            title: "Codex",
            workspaceRoot: "C:/Users/me/.kerminal",
          },
        },
      ],
    });

    renderAgentLauncher({ activeTab: terminalTab("tab-old") });

    await launchAgent(user, "Codex");

    expect(await screen.findByTestId("agent-restore-target-chip")).toHaveTextContent(
      "已失效",
    );

    await user.click(screen.getByRole("button", { name: "继续上次" }));

    await waitFor(() => {
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledWith({
        agentId: "codex",
        agentSessionId: "ags-stale-codex",
        resumeProviderSession: true,
      });
    });
    expect(screen.queryByTestId("agent-target-chip")).not.toBeInTheDocument();
  });

  it("offers to continue or replace the active session after returning", async () => {
    const user = userEvent.setup();

    renderAgentLauncher();

    await launchAgent(user, "Codex");

    await waitFor(() => {
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledTimes(1);
    });
    expect(await screen.findByTestId("agent-xterm")).toHaveAttribute(
      "data-focused",
      "true",
    );

    await user.click(
      screen.getByRole("button", { name: "Back to agent launcher" }),
    );

    expect(
      await screen.findByRole("button", { name: "使用 Codex 进入" }),
    ).toBeInTheDocument();
    expect(screen.getByTestId("agent-xterm")).toBeInTheDocument();
    expect(screen.getByTestId("agent-xterm")).toHaveAttribute(
      "data-focused",
      "false",
    );

    await launchAgent(user, "Codex");

    expect(
      await screen.findByRole("button", { name: "继续上次" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "新会话" })).toBeInTheDocument();
    expect(screen.getByTestId("agent-xterm")).toHaveAttribute(
      "data-focused",
      "false",
    );

    await user.click(screen.getByRole("button", { name: "继续上次" }));

    expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledTimes(1);
    await waitFor(() => {
      expect(screen.getByTestId("agent-xterm")).toHaveAttribute(
        "data-focused",
        "true",
      );
    });

    await user.click(
      screen.getByRole("button", { name: "Back to agent launcher" }),
    );
    await launchAgent(user, "Codex");
    await user.click(await screen.findByRole("button", { name: "新会话" }));

    await waitFor(() => {
      expect(apiMocks.createAgentSession).toHaveBeenCalledTimes(2);
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledTimes(2);
    });
  });

  it("keeps Codex and Claude in independent right-panel terminal sessions", async () => {
    const user = userEvent.setup();
    apiMocks.prepareExternalAgentWorkspace.mockImplementation(
      async ({
        agentId,
        agentSessionId,
      }: {
        agentId: "codex" | "claude";
        agentSessionId: string;
      }) => ({
        agentId,
        agentSessionId,
        args: [
          "-NoLogo",
          "-NoProfile",
          "-NoExit",
          "-Command",
          agentId,
        ],
        cwd: `C:/Users/me/.kerminal/agents/sessions/${agentSessionId}`,
        message: `${agentId} workspace prepared.`,
        shell: "pwsh.exe",
        title: agentId === "codex" ? "Codex" : "Claude",
      }),
    );

    renderAgentLauncher();

    await launchAgent(user, "Codex");

    await waitFor(() => {
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledWith({
        agentId: "codex",
        agentSessionId: "ags-codex",
        resumeProviderSession: false,
      });
    });

    await user.click(
      screen.getByRole("button", { name: "Back to agent launcher" }),
    );

    await launchAgent(user, "Claude");

    await waitFor(() => {
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledWith({
        agentId: "claude",
        agentSessionId: "ags-claude",
        resumeProviderSession: false,
      });
    });
    expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledTimes(2);

    const terminals = screen.getAllByTestId("agent-xterm");
    expect(terminals).toHaveLength(2);
    expect(terminalByTitle("Codex")).toHaveAttribute("data-focused", "false");
    expect(terminalByTitle("Codex")).toHaveAttribute(
      "data-cwd",
      "C:/Users/me/.kerminal/agents/sessions/ags-codex",
    );
    expect(terminalByTitle("Claude")).toHaveAttribute("data-focused", "true");
    expect(terminalByTitle("Claude")).toHaveAttribute(
      "data-cwd",
      "C:/Users/me/.kerminal/agents/sessions/ags-claude",
    );

    await user.click(
      screen.getByRole("button", { name: "Back to agent launcher" }),
    );
    await launchAgent(user, "Codex");
    await user.click(
      await screen.findByRole("button", { name: "继续上次" }),
    );

    expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledTimes(2);
    await waitFor(() => {
      expect(terminalByTitle("Codex")).toHaveAttribute("data-focused", "true");
      expect(terminalByTitle("Claude")).toHaveAttribute(
        "data-focused",
        "false",
      );
    });
  });
});

function terminalByTitle(title: string): HTMLElement {
  const terminal = screen
    .getAllByTestId("agent-xterm")
    .find((current) => current.textContent === title);
  if (!terminal) {
    throw new Error(`Expected ${title} agent terminal to be rendered.`);
  }
  return terminal;
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

/** 用公开下拉交互切换内置 Agent，并从统一“进入”按钮启动。 */
async function launchAgent(
  user: ReturnType<typeof userEvent.setup>,
  name: "Codex" | "Claude",
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

function terminalTab(id: string) {
  return {
    id,
    layout: { paneId: `pane-${id}`, type: "pane" },
    machineId: "local",
    title: id,
  } as never;
}

/** 生命周期测试沿用原生 Pi 状态，不要求已经移除的 MCP adapter。 */
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
