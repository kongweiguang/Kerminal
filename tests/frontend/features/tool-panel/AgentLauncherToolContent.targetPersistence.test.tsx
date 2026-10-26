// @author kongweiguang

import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ExternalAgentId } from "../../../../src/lib/agentLauncherApi";
import { registerTerminalPaneSession } from "../../../../src/features/terminal/terminalSessionRegistry";
import { AgentLauncherToolContent } from "../../../../src/features/tool-panel/AgentLauncherToolContent";
import { unregisterTestTerminalPaneSessions } from "../../support/terminalSessionRegistry.testSupport";
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
  listAgentSessions: (...args: unknown[]) => apiMocks.listAgentSessions(...args),
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

vi.mock("../../../../src/features/logs/LogToolContent", () => ({
  LogToolContent: () => <div data-testid="logs-tool">Logs tool</div>,
}));

describe("AgentLauncherToolContent preferred target persistence", () => {
  beforeEach(() => {
    apiMocks.archiveAgentSession.mockReset();
    apiMocks.createAgentSession.mockReset();
    apiMocks.getExternalAgentWorkspaceStatus.mockReset();
    apiMocks.listAgentSessions.mockReset();
    apiMocks.prepareExternalAgentWorkspace.mockReset();
    apiMocks.rebindAgentSessionTarget.mockReset();
    apiMocks.updateAgentSession.mockReset();
    terminalMocks.renderXtermPane.mockClear();
    unregisterTestTerminalPaneSessions();
    apiMocks.getExternalAgentWorkspaceStatus.mockResolvedValue(workspaceStatus());
    apiMocks.listAgentSessions.mockResolvedValue({ diagnostics: [], sessions: [] });
    let nextSessionIndex = 0;
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
      }) => {
        nextSessionIndex += 1;
        const agentSessionId = `ags-${agentId}-${nextSessionIndex}`;
        return {
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
          target,
          title: title ?? "Codex",
          },
        };
      },
    );
    apiMocks.prepareExternalAgentWorkspace.mockImplementation(
      async (request: {
        agentId: ExternalAgentId;
        agentSessionId: string;
      }) => ({
        agentId: request.agentId,
        agentSessionId: request.agentSessionId,
        args: ["-NoLogo", "-NoProfile", "-NoExit", "-Command", request.agentId],
        cwd: `C:/Users/me/.kerminal/agents/sessions/${request.agentSessionId}`,
        message: "Agent workspace prepared.",
        shell: "pwsh.exe",
        title: request.agentId === "claude" ? "Claude" : "Codex",
      }),
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

  it("keeps independent assistants for Tab A and B, and rebinds only after explicit continue", async () => {
    const user = userEvent.setup();
    const tabA = terminalTab("tab-a");
    const tabB = terminalTab("tab-b");
    const paneA = terminalPane("pane-a");
    const paneB = terminalPane("pane-b");
    registerTerminalPaneSession("pane-a", "term-a", {
      cwd: "/srv/a",
      shell: "bash",
      tabId: "tab-a",
      target: "local",
      targetRef: "local:tab-a",
    });
    registerTerminalPaneSession("pane-b", "term-b", {
      cwd: "/srv/b",
      shell: "bash",
      tabId: "tab-b",
      target: "local",
      targetRef: "local:tab-b",
    });

    const { rerender } = renderAgentLauncher({
      activeTab: tabA,
      focusedPane: paneA,
    });
    await launchAgent(user, "Codex");

    await waitFor(() => {
      expect(apiMocks.createAgentSession).toHaveBeenCalledWith(
        expect.objectContaining({
          scope: { kind: "tab", tabId: "tab-a" },
          target: expect.objectContaining({
            cwd: "/srv/a",
            paneId: "pane-a",
            tabId: "tab-a",
            targetRef: "local:tab-a",
            targetTerminalSessionId: "term-a",
          }),
          title: expect.stringContaining("当前 Tab"),
        }),
      );
    });
    expect(terminalByCwd("C:/Users/me/.kerminal/agents/sessions/ags-codex-1"))
      .toHaveAttribute("data-focused", "true");

    rerender(
      <AgentLauncherToolContent activeTab={tabB} focusedPane={paneB} />,
    );
    expect(apiMocks.rebindAgentSessionTarget).not.toHaveBeenCalled();

    await launchAgent(user, "Codex");

    await waitFor(() => {
      expect(apiMocks.createAgentSession).toHaveBeenNthCalledWith(
        2,
        expect.objectContaining({
          scope: { kind: "tab", tabId: "tab-b" },
          target: expect.objectContaining({
            cwd: "/srv/b",
            paneId: "pane-b",
            tabId: "tab-b",
            targetRef: "local:tab-b",
            targetTerminalSessionId: "term-b",
          }),
          title: expect.stringContaining("当前 Tab"),
        }),
      );
    });
    expect(apiMocks.rebindAgentSessionTarget).not.toHaveBeenCalled();
    expect(terminalByCwd("C:/Users/me/.kerminal/agents/sessions/ags-codex-1"))
      .toHaveAttribute("data-focused", "false");
    expect(terminalByCwd("C:/Users/me/.kerminal/agents/sessions/ags-codex-2"))
      .toHaveAttribute("data-focused", "true");

    rerender(
      <AgentLauncherToolContent activeTab={tabA} focusedPane={paneA} />,
    );
    expect(apiMocks.rebindAgentSessionTarget).not.toHaveBeenCalled();
    expect(terminalByCwd("C:/Users/me/.kerminal/agents/sessions/ags-codex-1"))
      .toHaveAttribute("data-focused", "true");
    expect(terminalByCwd("C:/Users/me/.kerminal/agents/sessions/ags-codex-2"))
      .toHaveAttribute("data-focused", "false");

    await user.click(
      screen.getByRole("button", { name: "Back to agent launcher" }),
    );
    await launchAgent(user, "Codex");

    await waitFor(() => {
      expect(apiMocks.rebindAgentSessionTarget).toHaveBeenCalledWith(
        "ags-codex-1",
        expect.objectContaining({
          cwd: "/srv/a",
          paneId: "pane-a",
          tabId: "tab-a",
          targetRef: "local:tab-a",
          targetTerminalSessionId: "term-a",
        }),
      );
    });
    expect(apiMocks.rebindAgentSessionTarget).toHaveBeenCalledTimes(1);
    expect(terminalByCwd("C:/Users/me/.kerminal/agents/sessions/ags-codex-1"))
      .toHaveAttribute("data-focused", "true");
  });
});

/** 使用最小稳定结构构造 tab，避免目标持久化测试依赖完整工作区 store。 */
function terminalTab(id: string) {
  return {
    id,
    layout: { paneId: `pane-${id}`, type: "pane" },
    machineId: "local",
    title: id,
  } as never;
}

/** 为目标绑定回归构造 pane 元数据，真实 session 信息由 registry 提供。 */
function terminalPane(id: string) {
  return {
    cwd: "/srv/fallback",
    id,
    mode: "local",
    shell: "bash",
    status: "connected",
    title: id,
  } as never;
}

/** 从测试可见的 cwd 定位单个 Agent TUI，断言切换时没有隐式销毁或重建。 */
function terminalByCwd(cwd: string): HTMLElement {
  const terminal = screen
    .getAllByTestId("agent-xterm")
    .find((current) => current.getAttribute("data-cwd") === cwd);
  if (!terminal) {
    throw new Error(`Expected agent terminal with cwd ${cwd} to be rendered.`);
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
