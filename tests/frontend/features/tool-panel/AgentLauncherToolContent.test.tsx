// @author kongweiguang
import {
  act,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ExternalAgentId } from "../../../../src/lib/agentLauncherApi";
import { registerTerminalPaneSession } from "../../../../src/features/terminal/terminalSessionRegistry";
import { unregisterTestTerminalPaneSessions } from "../../support/terminalSessionRegistry.testSupport";
import { AgentLauncherToolContent } from "../../../../src/features/tool-panel/AgentLauncherToolContent";
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
  agentSessionRecordId: (record: {
    session: { agentSessionId?: string; agent_session_id?: string };
  }) => record.session.agentSessionId ?? record.session.agent_session_id,
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
    onSessionFinished?: (event: {
      durationMs: number;
      sessionId: string;
    }) => void;
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
    apiMocks.getExternalAgentWorkspaceStatus.mockResolvedValue(
      workspaceStatus(),
    );
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
      async ({ agentId, launcherKey, scope, target, title }: { agentId: string; launcherKey?: string; scope?: unknown; target?: unknown; title?: string }) => ({
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
          title: title ??
            agentId === "claude"
              ? "Claude"
              : agentId === "custom"
                ? "Custom"
                : "Codex",
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
        const command = request.customCommand ?? request.agentId;
        const title =
          request.agentId === "claude"
            ? "Claude"
            : request.agentId === "custom"
              ? "Custom"
              : "Codex";
        return {
          agentId: request.agentId,
          agentSessionId: request.agentSessionId,
          args: ["-NoLogo", "-NoProfile", "-NoExit", "-Command", command],
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

  it("starts as a compact selector with one full-permission enter button", async () => {
    const user = userEvent.setup();
    const { container } = renderAgentLauncher();

    expect(await screen.findByRole("combobox", { name: "选择 Agent" })).toHaveAttribute(
      "aria-valuetext",
      "Codex",
    );
    expect(screen.getByRole("button", { name: "使用 Codex 进入" })).toBeVisible();
    await user.click(screen.getByRole("combobox", { name: "选择 Agent" }));
    expect(screen.getByRole("option", { name: /^Codex，/u })).toBeVisible();
    expect(screen.getByRole("option", { name: /^Claude，/u })).toBeVisible();
    expect(screen.getByRole("button", { name: "添加自定义 Agent" })).toBeVisible();
    await user.keyboard("{Escape}");
    expect(screen.getByRole("combobox", { name: "选择 Agent" })).toHaveTextContent(
      "可用",
    );
    expect(screen.getByTestId("agent-current-target")).toHaveTextContent(
      "当前目标 · 当前 Tab · 1 个终端 · tab-main",
    );
    expect(
      screen.queryByRole("heading", { name: "新建对话" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("textbox", { name: "Custom agent command" }),
    ).not.toBeInTheDocument();
    expect(container.textContent).not.toMatch(
      /config\.toml|\.mcp\.json|127\.0\.0\.1:37657|workspaceDir|pane-|session-/i,
    );
    for (const button of screen.getAllByRole("button")) {
      expect(button.getAttribute("title") ?? "").not.toMatch(
        /CLI detected|MCP config|config\.toml|\.mcp\.json|127\.0\.0\.1:37657/i,
      );
    }

    const commandHint = screen.getByLabelText("启动命令");
    expect(commandHint).toHaveAttribute("title", "启动命令：codex");
    expect(commandHint.tagName).not.toBe("BUTTON");
  });

  it("keeps the selector usable while workspace status is loading", () => {
    apiMocks.getExternalAgentWorkspaceStatus.mockReturnValueOnce(
      new Promise(() => {}),
    );

    renderAgentLauncher();

    expect(screen.getByRole("combobox", { name: "选择 Agent" })).toHaveAttribute(
      "aria-valuetext",
      "Codex",
    );
    expect(screen.getByRole("button", { name: "使用 Codex 进入" })).toBeVisible();
    expect(
      screen.queryByRole("textbox", { name: "Custom agent command" }),
    ).not.toBeInTheDocument();
  });

  it("opens Codex inside the right-panel terminal from the prepared launch spec", async () => {
    const user = userEvent.setup();

    renderAgentLauncher();

    await screen.findByRole("button", { name: "使用 Codex 进入" });
    await launchAgent(user, "Codex");

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
      "data-shell",
      "pwsh.exe",
    );
    expect(screen.getByTestId("agent-xterm")).toHaveAttribute(
      "data-cwd",
      "C:/Users/me/.kerminal/agents/sessions/ags-codex",
    );
    expect(screen.getByTestId("agent-terminal-command")).toHaveTextContent(
      "codex --dangerously-bypass-approvals-and-sandbox --dangerously-bypass-hook-trust · C:/Users/me/.kerminal/agents/sessions/ags-codex",
    );
    expect(terminalMocks.renderXtermPane).toHaveBeenLastCalledWith(
      expect.objectContaining({
        args: [
          "-NoLogo",
          "-NoProfile",
          "-NoExit",
          "-Command",
          "codex --dangerously-bypass-approvals-and-sandbox --dangerously-bypass-hook-trust",
        ],
        cwd: "C:/Users/me/.kerminal/agents/sessions/ags-codex",
        focused: true,
        inputCompatibilityMode: "agentTui",
        shell: "pwsh.exe",
        shellAssistEnabled: false,
        startupMessage: "加载 Codex...\r\n",
        title: "Codex",
        transientStartupMessage: true,
      }),
    );
  });

  it("opens Codex without an active terminal tab as an unbound agent session", async () => {
    const user = userEvent.setup();

    renderAgentLauncher({ activeTab: undefined });

    await launchAgent(user, "Codex");

    await waitFor(() => {
      expect(apiMocks.createAgentSession).toHaveBeenCalledWith({
        agentId: "codex",
        launcherKey: "builtin:codex",
        scope: { kind: "global" },
        title: "Codex · 整个 Kerminal",
      });
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledWith({
        agentId: "codex",
        agentSessionId: "ags-codex",
        resumeProviderSession: false,
      });
    });
    expect(await screen.findByTestId("agent-xterm")).toHaveAttribute(
      "data-shell",
      "pwsh.exe",
    );
    expect(
      screen.queryByText("Open a terminal tab before launching an agent."),
    ).not.toBeInTheDocument();
  });

  it("shows typed agent signal status without restoring the composer UI", async () => {
    const user = userEvent.setup();

    renderAgentLauncher();

    await launchAgent(user, "Codex");

    await waitFor(() => {
      expect(terminalMocks.renderXtermPane).toHaveBeenCalled();
    });
    const latestProps =
      terminalMocks.renderXtermPane.mock.calls[
        terminalMocks.renderXtermPane.mock.calls.length - 1
      ]?.[0];

    act(() => {
      latestProps.onAgentSignal?.({
        agent: "codex",
        agentSessionId: "ags-codex",
        status: "attention",
        terminalSessionId: "terminal-codex",
      });
    });

    expect(
      await screen.findByTestId("agent-terminal-signal"),
    ).toHaveTextContent("需处理");
    expect(screen.queryByRole("textbox")).not.toBeInTheDocument();
    expect(screen.queryByText("队列")).not.toBeInTheDocument();
  });

  it("opens Codex with full permissions from the primary enter button", async () => {
    const user = userEvent.setup();

    renderAgentLauncher();

    await user.click(
      await screen.findByRole("button", { name: "使用 Codex 进入" }),
    );

    await waitFor(() => {
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledWith({
        agentId: "codex",
        agentSessionId: "ags-codex",
        resumeProviderSession: false,
      });
    });
    expect(await screen.findByTestId("agent-xterm")).toHaveAttribute(
      "data-args",
      "-NoLogo -NoProfile -NoExit -Command codex --dangerously-bypass-approvals-and-sandbox --dangerously-bypass-hook-trust",
    );
    expect(screen.getByTestId("agent-terminal-command")).toHaveTextContent(
      "codex --dangerously-bypass-approvals-and-sandbox --dangerously-bypass-hook-trust · C:/Users/me/.kerminal/agents/sessions/ags-codex",
    );
    expect(apiMocks.updateAgentSession).toHaveBeenCalledWith("ags-codex", {
      launch: {
        args: [
          "-NoLogo",
          "-NoProfile",
          "-NoExit",
          "-Command",
          "codex --dangerously-bypass-approvals-and-sandbox --dangerously-bypass-hook-trust",
        ],
        commandLabel:
          "codex --dangerously-bypass-approvals-and-sandbox --dangerously-bypass-hook-trust",
        cwd: "C:/Users/me/.kerminal/agents/sessions/ags-codex",
        shell: "pwsh.exe",
      },
    });
    expect(
      screen.queryByRole("button", { name: "打开 Agent 启动选项" }),
    ).not.toBeInTheDocument();
  });

  it("continues a persisted yolo conversation with its saved permission mode", async () => {
    const user = userEvent.setup();
    apiMocks.listAgentSessions.mockResolvedValue({
      diagnostics: [],
      sessions: [
        {
          session: {
            agentId: "codex",
            agentSessionId: "ags-yolo-codex",
            launch: {
              args: [
                "-NoLogo",
                "-NoProfile",
                "-NoExit",
                "-Command",
                "codex --dangerously-bypass-approvals-and-sandbox --dangerously-bypass-hook-trust resume --last",
              ],
              commandLabel:
                "codex --dangerously-bypass-approvals-and-sandbox --dangerously-bypass-hook-trust resume --last",
              cwd: "C:/Users/me/.kerminal/agents/sessions/ags-yolo-codex",
              shell: "pwsh.exe",
            },
            status: "active",
            scope: { kind: "tab", tabId: "tab-main" },
            title: "Codex",
          },
        },
      ],
    });

    renderAgentLauncher();

    await user.click(
      await screen.findByRole("button", { name: "继续对话 Codex" }),
    );

    await waitFor(() => {
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledWith({
        agentId: "codex",
        agentSessionId: "ags-yolo-codex",
        resumeProviderSession: true,
      });
    });
    expect(await screen.findByTestId("agent-xterm")).toHaveAttribute(
      "data-args",
      "-NoLogo -NoProfile -NoExit -Command codex --dangerously-bypass-approvals-and-sandbox --dangerously-bypass-hook-trust",
    );
  });

  it("default entry action directly continues a persisted full-permission session", async () => {
    const user = userEvent.setup();
    apiMocks.listAgentSessions.mockResolvedValue({
      diagnostics: [],
      sessions: [
        {
          session: {
            agentId: "codex",
            agentSessionId: "ags-yolo-codex",
            launch: {
              args: [
                "-Command",
                "codex --dangerously-bypass-approvals-and-sandbox resume --last",
              ],
              commandLabel:
                "codex --dangerously-bypass-approvals-and-sandbox resume --last",
              cwd: "C:/Users/me/.kerminal/agents/sessions/ags-yolo-codex",
              shell: "pwsh.exe",
            },
            status: "active",
            scope: { kind: "tab", tabId: "tab-main" },
            title: "Codex yolo",
          },
        },
      ],
    });

    renderAgentLauncher();
    await user.click(
      await screen.findByRole("button", { name: "使用 Codex 进入" }),
    );

    await waitFor(() => {
      expect(apiMocks.createAgentSession).not.toHaveBeenCalled();
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledWith({
        agentId: "codex",
        agentSessionId: "ags-yolo-codex",
        resumeProviderSession: true,
      });
    });
  });

  it("continues a workflow session from the current target", async () => {
    const user = userEvent.setup();
    apiMocks.listAgentSessions.mockResolvedValue({
      diagnostics: [],
      sessions: [
        {
          session: {
            agentId: "codex",
            agentSessionId: "ags-global-source",
            launch: {
              args: [],
              commandLabel: "codex",
              cwd: "C:/Users/me/.kerminal/agents/sessions/ags-global-source",
              shell: "codex",
            },
            scope: { kind: "global" },
            status: "active",
            title: "Global Codex",
          },
        },
      ],
    });

    renderAgentLauncher();
    await user.click(await screen.findByRole("button", { name: /全部/u }));
    await user.click(
      await screen.findByRole("button", { name: "继续对话 Global Codex" }),
    );

    await waitFor(() => {
      expect(apiMocks.createAgentSession).not.toHaveBeenCalled();
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledWith({
        agentId: "codex",
        agentSessionId: "ags-global-source",
        resumeProviderSession: true,
      });
    });
  });

  it("opens Claude with full permissions from the primary enter button", async () => {
    const user = userEvent.setup();

    renderAgentLauncher();

    const selector = await screen.findByRole("combobox", { name: "选择 Agent" });
    await user.click(selector);
    await user.click(screen.getByRole("option", { name: /^Claude，/u }));
    await user.click(
      await screen.findByRole("button", { name: "使用 Claude 进入" }),
    );

    await waitFor(() => {
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledWith({
        agentId: "claude",
        agentSessionId: "ags-claude",
        resumeProviderSession: false,
      });
    });
    expect(await screen.findByTestId("agent-xterm")).toHaveAttribute(
      "data-args",
      "-NoLogo -NoProfile -NoExit -Command claude --dangerously-skip-permissions",
    );
    expect(screen.getByTestId("agent-terminal-command")).toHaveTextContent(
      "claude --dangerously-skip-permissions · C:/Users/me/.kerminal/agents/sessions/ags-claude",
    );
  });

  it("binds a new agent session to the focused terminal pane", async () => {
    const user = userEvent.setup();
    registerTerminalPaneSession("pane-prod", "term-prod", {
      cwd: "/srv/app",
      remoteHostId: "prod-web",
      shell: "bash",
      tabId: "tab-main",
      target: "ssh",
      targetRef: "ssh:prod-web",
    });

    render(
      <AgentLauncherToolContent
        activeTab={{ id: "tab-main" } as never}
        focusedPane={
          {
            currentCwd: "/srv/app",
            cwd: "/srv/fallback",
            id: "pane-prod",
            mode: "ssh",
            shell: "bash",
            title: "prod web",
          } as never
        }
      />,
    );

    expect(await screen.findByTestId("agent-current-target")).toHaveTextContent(
      "当前目标 · 当前 Tab · 1 个终端",
    );
    await launchAgent(user, "Codex");

    await waitFor(() => {
      expect(apiMocks.createAgentSession).toHaveBeenCalledWith({
        agentId: "codex",
        launcherKey: "builtin:codex",
        scope: { kind: "tab", tabId: "tab-main" },
        target: {
          cwd: "/srv/app",
          liveStatus: "ready",
          paneId: "pane-prod",
          shell: "bash",
          tabId: "tab-main",
          targetKind: "ssh",
          targetRef: "ssh:prod-web",
          targetTerminalSessionId: "term-prod",
        },
        title: "Codex · 当前 Tab · 1 个终端",
      });
    });
    expect(screen.queryByTestId("agent-target-chip")).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Rebind agent target" }),
    ).not.toBeInTheDocument();
  });

  it("keeps the primary Codex entry in the current tab while retaining the focused pane target", async () => {
    const user = userEvent.setup();
    registerTerminalPaneSession("pane-prod", "term-prod", {
      cwd: "/srv/app",
      remoteHostId: "prod-web",
      shell: "bash",
      tabId: "tab-main",
      target: "ssh",
      targetRef: "ssh:prod-web",
    });

    render(
      <AgentLauncherToolContent
        activeTab={{ id: "tab-main" } as never}
        focusedPane={
          {
            currentCwd: "/srv/app",
            cwd: "/srv/fallback",
            id: "pane-prod",
            mode: "ssh",
            shell: "bash",
          } as never
        }
      />,
    );

    await user.click(
      await screen.findByRole("button", { name: "使用 Codex 进入" }),
    );

    await waitFor(() => {
      expect(apiMocks.createAgentSession).toHaveBeenCalledWith({
        agentId: "codex",
        launcherKey: "builtin:codex",
        scope: { kind: "tab", tabId: "tab-main" },
        target: {
          cwd: "/srv/app",
          liveStatus: "ready",
          paneId: "pane-prod",
          shell: "bash",
          tabId: "tab-main",
          targetKind: "ssh",
          targetRef: "ssh:prod-web",
          targetTerminalSessionId: "term-prod",
        },
        title: "Codex · 当前 Tab · 1 个终端",
      });
    });
    expect(await screen.findByTestId("agent-xterm")).toHaveTextContent("Codex");
  });

  it("continues a persisted Codex agent session without creating a new one", async () => {
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
            sessionRoot:
              "C:/Users/me/.kerminal/agents/sessions/ags-restored-codex",
            scope: { kind: "tab", tabId: "tab-main" },
            status: "active",
            title: "Codex",
            workspaceRoot: "C:/Users/me/.kerminal",
          },
        },
      ],
    });

    renderAgentLauncher();

    await launchAgent(user, "Codex");

    await waitFor(() => {
      expect(apiMocks.createAgentSession).not.toHaveBeenCalled();
      expect(apiMocks.prepareExternalAgentWorkspace).toHaveBeenCalledWith({
        agentId: "codex",
        agentSessionId: "ags-restored-codex",
        resumeProviderSession: true,
      });
    });
    expect(await screen.findByTestId("agent-xterm")).toHaveAttribute(
      "data-cwd",
      "C:/Users/me/.kerminal/agents/sessions/ags-restored-codex",
    );
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
