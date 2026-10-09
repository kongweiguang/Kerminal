// @author kongweiguang
import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ExternalAgentId } from "../../../../src/lib/agentLauncherApi";
import { unregisterTestTerminalPaneSessions } from "../../support/terminalSessionRegistry.testSupport";
import { tools } from "../../../../src/features/workspace/workspaceData";
import { AgentLauncherToolContent } from "../../../../src/features/tool-panel/AgentLauncherToolContent";
import { ToolPanel } from "../../../../src/features/tool-panel/ToolPanel";
import { defaultAppSettings } from "../../../../src/features/settings/settingsModel";
import { launchAgent, workspaceStatus } from "./agentLauncherTestSupport";

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
          title:
            title ??
            (agentId === "claude"
              ? "Claude"
              : agentId === "pi"
                ? "PI Agent"
                : agentId === "custom"
                  ? "Custom"
                  : "Codex"),
          workspaceRoot: "C:/Users/me/.kerminal",
        },
      }),
    );
    apiMocks.prepareExternalAgentWorkspace.mockImplementation(
      async (request: {
        agentId: ExternalAgentId;
        agentSessionId: string;
        customCommand?: string;
      }) => {
        const command =
          request.customCommand ??
          (request.agentId === "pi"
            ? "pi --approve"
            : request.agentId);
        const title =
          request.agentId === "claude"
            ? "Claude"
            : request.agentId === "pi"
              ? "PI Agent"
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
  });

  afterEach(() => {
    vi.clearAllMocks();
  });

  it("removes the agent terminal when its owning workspace tab closes", async () => {
    const user = userEvent.setup();
    const tabA = terminalTab("tab-a");
    const tabB = terminalTab("tab-b");
    const { rerender } = renderAgentLauncher({
      activeTab: tabA,
      terminalTabs: [tabA, tabB],
    });

    await launchAgent(user, "Codex");
    await waitFor(() => {
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledWith({
        agentId: "codex",
        agentSessionId: "ags-codex",
        resumeProviderSession: false,
      });
    });
    expect(screen.getByTestId("agent-xterm")).toHaveAttribute(
      "data-cwd",
      "C:/Users/me/.kerminal/agents/sessions/ags-codex",
    );

    rerender(
      <AgentLauncherToolContent activeTab={tabB} terminalTabs={[tabB]} />,
    );

    await waitFor(() => {
      expect(screen.queryByTestId("agent-xterm")).not.toBeInTheDocument();
    });
  });

  it("sends a desktop notification when an enabled agent terminal finishes", async () => {
    const user = userEvent.setup();

    renderAgentLauncher({
      desktopNotifications: {
        backgroundOnly: true,
        enabled: true,
        importantOnly: false,
        minDurationMs: 10_000,
        throttleMs: 30_000,
      },
    });

    await launchAgent(user, "Codex");
    await waitFor(() => {
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledTimes(1);
    });

    const terminalCalls = terminalMocks.renderXtermPane.mock.calls;
    const terminalProps = terminalCalls[terminalCalls.length - 1]?.[0] as
      | {
          onSessionFinished?: (event: {
            durationMs: number;
            sessionId: string;
          }) => void;
        }
      | undefined;
    expect(terminalProps?.onSessionFinished).toEqual(expect.any(Function));

    act(() => {
      terminalProps?.onSessionFinished?.({
        durationMs: 12_500,
        sessionId: "term-agent-codex",
      });
    });

    expect(notificationMocks.sendDesktopNotification).toHaveBeenCalledWith(
      expect.objectContaining({
        event: {
          agentName: "Codex",
          durationMs: 12_500,
          exitCode: null,
          kind: "agent.process.finished",
          notificationKey: "agent.process.finished:ags-codex",
        },
        permissionPrompt: "important-event",
        settings: expect.objectContaining({ enabled: true }),
        visibility: "hidden",
      }),
    );
    expect(
      JSON.stringify(notificationMocks.sendDesktopNotification.mock.calls[0][0]),
    ).not.toContain("C:/Users/me/.kerminal");
    expect(
      JSON.stringify(notificationMocks.sendDesktopNotification.mock.calls[0][0]),
    ).not.toContain("KERMINAL_MCP_ENDPOINT");
    expect(
      JSON.stringify(notificationMocks.sendDesktopNotification.mock.calls[0][0]),
    ).not.toContain("-NoLogo");
  });

  it("renders from ToolPanel as the external agent launcher", async () => {
    render(
      <ToolPanel
        activeTab={terminalTab("tab-main")}
        activeTool="agentLauncher"
        onActiveToolChange={vi.fn()}
        tools={tools}
      />,
    );

    expect(await screen.findByRole("combobox", { name: "选择 Agent" })).toHaveAttribute(
      "aria-valuetext",
      "Codex",
    );
    expect(screen.getByRole("button", { name: "使用 Codex 进入" })).toBeVisible();
  });

  it("keeps a launched agent terminal while switching right-panel tools", async () => {
    const user = userEvent.setup();
    const { rerender } = render(
      <ToolPanel
        activeTab={terminalTab("tab-main")}
        activeTool="agentLauncher"
        onActiveToolChange={vi.fn()}
        tools={tools}
      />,
    );

    await launchAgent(user, "Codex");

    await waitFor(() => {
      expect(apiMocks.createAgentSession).toHaveBeenCalledTimes(1);
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledTimes(1);
    });
    expect(await screen.findByTestId("agent-xterm")).toHaveAttribute(
      "data-cwd",
      "C:/Users/me/.kerminal/agents/sessions/ags-codex",
    );

    rerender(
      <ToolPanel
        activeTab={terminalTab("tab-main")}
        activeTool="logs"
        onActiveToolChange={vi.fn()}
        tools={tools}
      />,
    );

    expect(await screen.findByTestId("logs-tool")).toBeInTheDocument();
    expect(screen.getByTestId("agent-xterm")).toHaveAttribute(
      "data-cwd",
      "C:/Users/me/.kerminal/agents/sessions/ags-codex",
    );

    rerender(
      <ToolPanel
        activeTab={terminalTab("tab-main")}
        activeTool="agentLauncher"
        onActiveToolChange={vi.fn()}
        tools={tools}
      />,
    );

    expect(screen.getByTestId("agent-xterm")).toHaveAttribute(
      "data-cwd",
      "C:/Users/me/.kerminal/agents/sessions/ags-codex",
    );
    expect(apiMocks.createAgentSession).toHaveBeenCalledTimes(1);
    expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledTimes(1);
  });

  it("launches a user supplied custom CLI command inside the right panel", async () => {
    const user = userEvent.setup();
    apiMocks.prepareExternalAgentWorkspace.mockImplementationOnce(async (request) => ({
      agentId: "custom",
      agentSessionId: request.agentSessionId,
      args: [
        "-NoLogo",
        "-NoProfile",
        "-NoExit",
        "-Command",
        "kimi --fast",
      ],
      cwd: "C:/Users/me/.kerminal/agents/sessions/ags-custom",
      message: "Custom workspace prepared.",
      shell: "pwsh.exe",
      title: "Custom Agent",
    }));

    renderAgentLauncher({
      settings: {
        ...defaultAppSettings,
        agentLauncher: {
          customAgents: [
            {
              command: "kimi --fast",
              id: "11111111-1111-4111-8111-111111111111",
              name: "Kimi",
            },
          ],
          selectedAgentKey: "builtin:codex",
        },
      },
    });

    await launchAgent(user, "Kimi");

    await waitFor(() => {
      expect(apiMocks.createAgentSession).toHaveBeenCalledWith({
        agentId: "custom",
        launcherKey: "custom:11111111-1111-4111-8111-111111111111",
        scope: { kind: "tab", tabId: "tab-main" },
        title: "Kimi",
      });
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledWith({
        agentId: "custom",
        agentSessionId: "ags-custom",
        customCommand: "kimi --fast",
        resumeProviderSession: false,
      });
    });
    expect(await screen.findByTestId("agent-xterm")).toHaveAttribute("data-shell", "pwsh.exe");
    expect(screen.getByTestId("agent-xterm")).toHaveAttribute(
      "data-args",
      "-NoLogo -NoProfile -NoExit -Command kimi --fast",
    );
    expect(screen.getByTestId("agent-terminal-command")).toHaveTextContent(
      "kimi --fast · C:/Users/me/.kerminal/agents/sessions/ags-custom",
    );
  });

  // 用户入口应显示原生 CLI 命令，旧 adapter 参数不再进入终端启动。
  it("launches PI as a native built-in Agent", async () => {
    const user = userEvent.setup();
    renderAgentLauncher();

    await launchAgent(user, "PI Agent");

    await waitFor(() => {
      expect(apiMocks.createAgentSession).toHaveBeenCalledWith({
        agentId: "pi",
        launcherKey: "builtin:pi",
        scope: { kind: "tab", tabId: "tab-main" },
        title: "PI Agent · 当前 Tab · 1 个终端 · tab-main",
      });
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledWith({
        agentId: "pi",
        agentSessionId: "ags-pi",
        resumeProviderSession: false,
      });
    });
    expect(await screen.findByTestId("agent-xterm")).toHaveTextContent(
      "PI Agent",
    );
    expect(screen.getByTestId("agent-terminal-command")).toHaveTextContent(
      "pi --approve",
    );
    expect(screen.getByTestId("agent-xterm")).toHaveAttribute(
      "data-args",
      "-NoLogo -NoProfile -NoExit -Command pi --approve",
    );
  });

  it("launches a saved Custom definition and snapshots its command", async () => {
    const user = userEvent.setup();
    const settings = {
      ...defaultAppSettings,
      agentLauncher: {
        customAgents: [
          {
            command: "custom-agent --profile local",
            id: "11111111-1111-4111-8111-111111111111",
            name: "Secret Agent",
          },
        ],
        selectedAgentKey: "builtin:codex",
      },
    };
    renderAgentLauncher({ settings });

    await launchAgent(user, "Secret Agent");

    await waitFor(() => {
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledWith({
        agentId: "custom",
        agentSessionId: "ags-custom",
        customCommand: "custom-agent --profile local",
        resumeProviderSession: false,
      });
    });
    expect(await screen.findByTestId("agent-xterm")).toBeInTheDocument();
    expect(apiMocks.createAgentSession).toHaveBeenCalledWith({
      agentId: "custom",
      launcherKey: "custom:11111111-1111-4111-8111-111111111111",
      scope: { kind: "tab", tabId: "tab-main" },
      title: "Secret Agent",
    });
    expect(apiMocks.updateAgentSession).toHaveBeenCalledWith(
      "ags-custom",
      expect.objectContaining({
        launch: expect.objectContaining({
          commandLabel: "custom-agent --profile local",
        }),
      }),
    );
  });

  it("creates a fresh dropdown Custom session from the edited definition", async () => {
    const user = userEvent.setup();
    const launcherKey = "custom:11111111-1111-4111-8111-111111111111";
    apiMocks.listAgentSessions.mockResolvedValue({
      diagnostics: [],
      sessions: [
        {
          session: {
            agentId: "custom",
            agentSessionId: "ags-old-pi",
            launcherKey,
            launch: {
              args: ["--old"],
              commandLabel: "pi --old",
              cwd: "C:/Users/me/.kerminal/agents/sessions/ags-old-pi",
              shell: "pi",
            },
            scope: { kind: "tab", tabId: "tab-main" },
            status: "active",
            title: "Old PI",
          },
        },
      ],
    });
    renderAgentLauncher({
      settings: {
        ...defaultAppSettings,
        agentLauncher: {
          customAgents: [
            {
              command: "pi --new",
              id: "11111111-1111-4111-8111-111111111111",
              name: "New PI",
            },
          ],
          selectedAgentKey: launcherKey,
        },
      },
    });

    await user.click(
      await screen.findByRole("button", { name: "使用 New PI 进入" }),
    );
    await user.click(await screen.findByRole("button", { name: "新会话" }));

    await waitFor(() => {
      expect(apiMocks.createAgentSession).toHaveBeenCalledWith({
        agentId: "custom",
        launcherKey,
        scope: { kind: "tab", tabId: "tab-main" },
        title: "New PI",
      });
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledWith({
        agentId: "custom",
        agentSessionId: "ags-custom",
        customCommand: "pi --new",
        resumeProviderSession: false,
      });
    });
  });

  it("creates a same-Agent session from a deleted Custom definition snapshot", async () => {
    const user = userEvent.setup();
    apiMocks.listAgentSessions.mockResolvedValue({
      diagnostics: [],
      sessions: [
        {
          session: {
            agentId: "custom",
            agentSessionId: "ags-deleted-pi",
            launcherKey: "custom:22222222-2222-4222-8222-222222222222",
            launch: {
              args: ["--deleted"],
              commandLabel: "pi --deleted",
              cwd: "C:/Users/me/.kerminal/agents/sessions/ags-deleted-pi",
              shell: "pi",
            },
            scope: { kind: "tab", tabId: "tab-main" },
            status: "active",
            title: "Deleted PI",
          },
        },
      ],
    });

    renderAgentLauncher();
    await user.click(
      await screen.findByRole("button", { name: "同 Agent 新会话" }),
    );

    await waitFor(() => {
      expect(apiMocks.createAgentSession).toHaveBeenCalledWith({
        agentId: "custom",
        launcherKey: "custom:22222222-2222-4222-8222-222222222222",
        scope: { kind: "tab", tabId: "tab-main" },
        title: "Deleted PI",
      });
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledWith({
        agentId: "custom",
        agentSessionId: "ags-custom",
        customCommand: "pi --deleted",
        resumeProviderSession: false,
      });
    });
  });

  it("keeps custom save disabled until the required fields are complete", async () => {
    const user = userEvent.setup();

    renderAgentLauncher();

    await user.click(screen.getByRole("combobox", { name: "选择 Agent" }));
    await user.click(screen.getByRole("button", { name: "添加自定义 Agent" }));

    expect(
      screen.getByRole("button", { name: "保存并选择" }),
    ).toBeDisabled();

    await user.keyboard("{Enter}");

    expect(apiMocks.prepareExternalAgentWorkspace).not.toHaveBeenCalled();
  });

  it("persists an edited Kerminal session title", async () => {
    const user = userEvent.setup();
    apiMocks.listAgentSessions.mockResolvedValue({
      diagnostics: [],
      sessions: [
        {
          session: {
            agentId: "codex",
            agentSessionId: "ags-title",
            launch: { args: [], cwd: "", shell: "codex" },
            scope: { kind: "tab", tabId: "tab-main" },
            status: "active",
            title: "旧标题",
          },
        },
      ],
    });

    renderAgentLauncher();

    await user.click(
      await screen.findByRole("button", { name: "重命名 旧标题" }),
    );
    const input = screen.getByRole("textbox", { name: "会话标题" });
    await user.clear(input);
    await user.type(input, "发布检查");
    await user.click(screen.getByRole("button", { name: "保存标题" }));

    await waitFor(() => {
      expect(apiMocks.updateAgentSession).toHaveBeenCalledWith("ags-title", {
        title: "发布检查",
      });
    });
  });

  it("keeps Agent runtime failures in collapsed technical details", async () => {
    const user = userEvent.setup();
    apiMocks.getExternalAgentWorkspaceStatus.mockRejectedValueOnce(
      new Error(
        'managed session failed at C:\\private\\agent.json with "token": "agent-secret"',
      ),
    );

    renderAgentLauncher();

    expect(await screen.findByText("无法读取 Agent 状态")).toBeVisible();
    expect(screen.getByText("请确认 Kerminal 服务可用后重试。")).toBeVisible();
    expect(screen.getByRole("button", { name: "重试" })).toBeVisible();
    const detail = screen.getByText(/managed session failed/);
    expect(detail.closest("details")).not.toHaveAttribute("open");
    expect(detail).not.toHaveTextContent("agent-secret");

    await user.click(screen.getByText("技术详情"));
    expect(detail.closest("details")).toHaveAttribute("open");
  });

});

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

function terminalTab(id: string) {
  return {
    id,
    layout: { paneId: `pane-${id}`, type: "pane" },
    machineId: "local",
    title: id,
  } as never;
}
