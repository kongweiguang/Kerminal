// @author kongweiguang

import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it } from "vitest";
import type { TerminalOutputEvent } from "../../../../src/lib/terminalApi";
import { defaultAppSettings } from "../../../../src/features/settings/settingsModel";
import { requestAgentSend } from "../../../../src/features/agent-workflow/agentSendRequestStore";
import {
  dockerContainerTarget,
  dockerContainerTargetCapabilities,
} from "../../../../src/lib/targetModel";
import { useWorkspaceStore } from "../../../../src/features/workspace/workspaceStore";
import {
  remoteHostTree,
  testSshOptions,
} from "../../support/app/KerminalShell.testSupport.tsx";
import { KerminalShell } from "../../../../src/app/KerminalShell";
import {
  findExpandedSidebarMachine,
  mocks,
} from "./setup";

export function registerSessionAndToolTests() {
  it("starts without creating a local terminal when no workspace session is saved", async () => {
    render(<KerminalShell />);

    expect(
      await findExpandedSidebarMachine(/172\.16\.41\.60/),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "打开 Agent Launcher" }),
    ).toBeInTheDocument();
    expect(mocks.terminalApi.createTerminalSession).not.toHaveBeenCalled();
  });

  it("opens Agent Launcher for a terminal send request while the tool panel is collapsed", async () => {
    act(() => {
      useWorkspaceStore.getState().addTerminalTab({
        title: "Agent request target",
      });
    });
    const paneId = useWorkspaceStore.getState().terminalPanes[0]?.id;
    expect(paneId).toBeTruthy();
    render(<KerminalShell />);

    expect(
      await screen.findByRole("complementary", { name: "右侧工具栏" }),
    ).toHaveAttribute("aria-expanded", "false");

    act(() => {
      requestAgentSend({
        paneId: paneId!,
        source: "context",
      });
    });

    expect(
      await screen.findByRole("button", { name: "收起 Agent Launcher" }),
    ).toHaveAttribute("aria-pressed", "true");
    expect(
      screen.getByRole("complementary", { name: "工具面板" }),
    ).toHaveAttribute("aria-expanded", "true");

    fireEvent.click(
      screen.getByRole("button", { name: "收起 Agent Launcher" }),
    );
    await waitFor(() => {
      expect(
        screen.getByRole("complementary", { name: "右侧工具栏" }),
      ).toHaveAttribute("aria-expanded", "false");
    });
  });

  it("keeps different panel directions open together and replaces only the same direction", async () => {
    mocks.settingsApi.getSettings.mockResolvedValue({
      ...defaultAppSettings,
      toolRail: {
        ...defaultAppSettings.toolRail,
        panelPlacements: {
          ...defaultAppSettings.toolRail.panelPlacements,
          context: "left",
          logs: "attached",
          snippets: "center",
          system: "bottom",
        },
      },
    });
    const user = userEvent.setup();
    const { container } = render(<KerminalShell />);

    await waitFor(() => {
      expect(
        useWorkspaceStore.getState().settings.toolRail.panelPlacements.system,
      ).toBe("bottom");
    });
    for (const toolName of ["当前上下文", "命令历史", "系统", "片段"]) {
      await user.click(
        screen.getByRole("button", { name: `打开 ${toolName}` }),
      );
    }

    const visiblePanel = (placement: string) =>
      container.querySelector<HTMLElement>(
        `section[data-tool-panel-placement="${placement}"]:not([hidden])`,
      );
    await waitFor(() => {
      expect(visiblePanel("attached")).toBeVisible();
      expect(visiblePanel("left")).toBeVisible();
      expect(visiblePanel("bottom")).toBeVisible();
      expect(visiblePanel("center")).toBeVisible();
    });
    expect(screen.getByRole("button", { name: "收起 系统" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );

    await user.click(
      screen.getByRole("button", { name: "打开 Agent Launcher" }),
    );

    await waitFor(() => {
      expect(
        screen.getByRole("button", { name: "打开 命令历史" }),
      ).toHaveAttribute("aria-pressed", "false");
      expect(
        screen.getByRole("button", { name: "收起 Agent Launcher" }),
      ).toHaveAttribute("aria-pressed", "true");
      expect(visiblePanel("left")).toBeVisible();
      expect(visiblePanel("bottom")).toBeVisible();
      expect(visiblePanel("center")).toBeVisible();
    });

    await user.click(screen.getByRole("button", { name: "收起 系统" }));
    await waitFor(() => expect(visiblePanel("bottom")).toBeNull());
    expect(visiblePanel("attached")).toBeVisible();
    expect(visiblePanel("left")).toBeVisible();
    expect(visiblePanel("center")).toBeVisible();
  });

  it("reaps local orphan PTY sessions before restoring saved terminal tabs", async () => {
    const callOrder: string[] = [];
    let resolveReap: (() => void) | undefined;
    mocks.terminalApi.reapOrphanTerminalSessions.mockImplementation(
      () =>
        new Promise((resolve) => {
          callOrder.push("reap:start");
          resolveReap = () => {
            callOrder.push("reap:done");
            resolve({
              elapsedMs: 3,
              reapedCount: 1,
              sessionIds: ["old-local-session"],
            });
          };
        }),
    );
    mocks.terminalApi.createTerminalSession.mockImplementation(
      async (_request, onOutput: (event: TerminalOutputEvent) => void) => {
        callOrder.push("create:local");
        onOutput({
          data: "local ready",
          kind: "data",
          sessionId: "session-local",
        });
        return {
          cols: 80,
          id: "session-local",
          rows: 24,
          shell: "test-shell",
          status: "running",
        };
      },
    );
    mocks.workspaceSessionApi.loadWorkspaceSessionFile.mockResolvedValue({
      activeTabId: "tab-local-restore",
      focusedPaneId: "pane-local-restore",
      selectedMachineId: "machine-local-restore",
      sidebarMachines: [],
      terminalPanes: [
        {
          id: "pane-local-restore",
          lines: [],
          machineId: "machine-local-restore",
          mode: "local",
          prompt: "PS>",
          shell: "test-shell",
          status: "online",
          title: "恢复本地会话",
        },
      ],
      terminalTabs: [
        {
          id: "tab-local-restore",
          layout: { type: "pane", paneId: "pane-local-restore" },
          machineId: "machine-local-restore",
          title: "恢复本地会话",
        },
      ],
    });

    render(<KerminalShell />);

    await waitFor(() => {
      expect(
        mocks.terminalApi.reapOrphanTerminalSessions,
      ).toHaveBeenCalledTimes(1);
    });
    expect(mocks.terminalApi.createTerminalSession).not.toHaveBeenCalled();

    await act(async () => {
      resolveReap?.();
      await Promise.resolve();
    });

    await waitFor(() => {
      expect(mocks.terminalApi.createTerminalSession).toHaveBeenCalledTimes(1);
    });
    expect(callOrder).toEqual(["reap:start", "reap:done", "create:local"]);
  });

  it("continues workspace restore when local orphan PTY reaping fails", async () => {
    mocks.terminalApi.reapOrphanTerminalSessions.mockRejectedValue(
      new Error("reaper unavailable"),
    );
    mocks.workspaceSessionApi.loadWorkspaceSessionFile.mockResolvedValue({
      activeTabId: "tab-local-reap-failed",
      focusedPaneId: "pane-local-reap-failed",
      selectedMachineId: "machine-local-reap-failed",
      sidebarMachines: [],
      terminalPanes: [
        {
          id: "pane-local-reap-failed",
          lines: [],
          machineId: "machine-local-reap-failed",
          mode: "local",
          prompt: "PS>",
          shell: "test-shell",
          status: "online",
          title: "reaper 失败后恢复",
        },
      ],
      terminalTabs: [
        {
          id: "tab-local-reap-failed",
          layout: { type: "pane", paneId: "pane-local-reap-failed" },
          machineId: "machine-local-reap-failed",
          title: "reaper 失败后恢复",
        },
      ],
    });

    render(<KerminalShell />);

    await waitFor(() => {
      expect(mocks.terminalApi.createTerminalSession).toHaveBeenCalledWith(
        expect.objectContaining({
          cols: 80,
          rows: 24,
          shell: "test-shell",
        }),
        expect.any(Function),
      );
    });
  });

  it("keeps host SFTP and cross-host transfer entries on separate surfaces", async () => {
    const user = userEvent.setup();
    render(<KerminalShell />);

    const hostButton = await findExpandedSidebarMachine(/172\.16\.41\.60/);

    fireEvent.contextMenu(hostButton);
    await user.click(screen.getByRole("menuitem", { name: "打开 SFTP" }));

    expect(await screen.findByLabelText("SFTP 工具内容")).toBeInTheDocument();
    expect(screen.queryByLabelText("SFTP 传输工作台")).not.toBeInTheDocument();

    fireEvent.contextMenu(hostButton);
    await user.click(screen.getByRole("menuitem", { name: "新建传输 Tab" }));

    expect(await screen.findByLabelText("SFTP 传输工作台")).toHaveTextContent(
      "right:db980b17-2ed0-44e5-b72a-6ecadf788439 locked:none",
    );
    expect(
      screen.getByRole("complementary", { name: "右侧工具栏" }),
    ).toHaveAttribute("aria-expanded", "false");
  });

  it("creates a new SSH host from the SFTP transfer workbench and returns it to the requested side", async () => {
    const user = userEvent.setup();
    const createdHost = {
      authType: "agent" as const,
      createdAt: "test",
      groupId: "30fbc381-2884-4b75-9f88-0e28f31ca8b0",
      host: "10.0.0.9",
      id: "host-created-from-transfer",
      name: "transfer-dev",
      port: 22,
      protocol: "sftp",
      sshOptions: testSshOptions,
      sortOrder: 30,
      tags: [],
      updatedAt: "test",
      username: "deploy",
    };
    mocks.remoteHostApi.createRemoteHost.mockResolvedValue(createdHost);
    mocks.remoteHostApi.listRemoteHostTree
      .mockResolvedValueOnce(remoteHostTree)
      .mockResolvedValueOnce([
        {
          ...remoteHostTree[0],
          hosts: [...remoteHostTree[0].hosts, createdHost],
        },
      ]);

    render(<KerminalShell />);

    const hostButton = await findExpandedSidebarMachine(/172\.16\.41\.60/);
    fireEvent.contextMenu(hostButton);
    await user.click(screen.getByRole("menuitem", { name: "新建传输 Tab" }));
    expect(await screen.findByLabelText("SFTP 传输工作台")).toHaveTextContent(
      "right:db980b17-2ed0-44e5-b72a-6ecadf788439",
    );

    await user.click(
      screen.getByRole("button", { name: "从右侧新建 SSH 主机" }),
    );

    expect(
      await screen.findByRole("dialog", { name: "新建主机" }),
    ).toBeInTheDocument();
    await user.type(screen.getByLabelText("名称"), "transfer-dev");
    await user.type(screen.getByLabelText("主机"), "10.0.0.9");
    await user.type(screen.getByLabelText("用户名"), "deploy");
    await user.click(screen.getByRole("combobox", { name: "认证方式" }));
    await user.click(screen.getByRole("option", { name: /SSH Agent/ }));
    await user.click(screen.getByRole("button", { name: "确认" }));

    await waitFor(() => {
      expect(mocks.remoteHostApi.createRemoteHost).toHaveBeenCalledWith(
        expect.objectContaining({
          host: "10.0.0.9",
          name: "transfer-dev",
          protocol: "sftp",
          username: "deploy",
        }),
      );
      expect(screen.getByLabelText("SFTP 传输工作台")).toHaveTextContent(
        "created:tab-sftp-transfer-1:right:host-created-from-transfer",
      );
    });
  });

  it("flushes the workspace session before the page is hidden", async () => {
    mocks.workspaceSessionApi.loadWorkspaceSessionFile.mockResolvedValue({
      activeTabId: "",
      focusedPaneId: "",
      removedSidebarMachineIds: [],
      selectedMachineId: "",
      sidebarMachines: [],
      terminalPanes: [],
      terminalTabGroupPreferences: {},
      terminalTabs: [],
    });
    render(<KerminalShell />);

    await waitFor(() => {
      expect(
        mocks.workspaceSessionApi.saveWorkspaceSessionFile,
      ).toHaveBeenCalled();
    });
    mocks.workspaceSessionApi.saveWorkspaceSessionFile.mockClear();

    fireEvent(window, new Event("pagehide"));

    await waitFor(() => {
      expect(
        mocks.workspaceSessionApi.saveWorkspaceSessionFile,
      ).toHaveBeenCalledWith(
        expect.objectContaining({
          terminalPanes: [],
          terminalTabs: [],
        }),
      );
    });
  });

  it("keeps shell chrome stable when terminal output history changes", async () => {
    mocks.terminalApi.createTerminalSession.mockResolvedValue({
      cols: 80,
      id: "session-local-output-history",
      rows: 24,
      shell: "test-shell",
      status: "running",
    });

    render(<KerminalShell />);

    expect(
      await findExpandedSidebarMachine(/172\.16\.41\.60/),
    ).toBeInTheDocument();
    await waitFor(() => {
      expect(mocks.settingsApi.getSettings).toHaveBeenCalled();
    });

    act(() => {
      useWorkspaceStore.getState().addTerminalTab();
    });
    await waitFor(() => {
      expect(mocks.terminalApi.createTerminalSession).toHaveBeenCalled();
    });

    const paneId = useWorkspaceStore.getState().terminalPanes[0]?.id;
    if (!paneId) {
      throw new Error("Expected a local terminal pane to be created.");
    }
    expect(paneId).toBe("pane-local-1");
    const chromeRenderCountAfterOpen = mocks.appTitleBar.renderCount;

    act(() => {
      useWorkspaceStore
        .getState()
        .updatePaneOutputHistory(paneId, "latest shell-isolated output");
    });

    expect(
      useWorkspaceStore
        .getState()
        .terminalPanes.find((pane) => pane.id === paneId)?.outputHistory,
    ).toBe("latest shell-isolated output");
    expect(mocks.appTitleBar.renderCount).toBe(chromeRenderCountAfterOpen);

    fireEvent(window, new Event("pagehide"));
    await waitFor(() => {
      const calls =
        mocks.workspaceSessionApi.saveWorkspaceSessionFile.mock.calls;
      const savedSession = calls[calls.length - 1]?.[0];
      expect(savedSession.terminalPanes).toEqual(
        expect.arrayContaining([
          expect.objectContaining({
            id: paneId,
            outputHistory: "latest shell-isolated output",
          }),
        ]),
      );
    });
  });

  it("does not list Docker containers when an SSH host is selected", async () => {
    const user = userEvent.setup();

    render(<KerminalShell />);

    const hostButton = await findExpandedSidebarMachine(/172\.16\.41\.60/);
    await user.click(hostButton);

    expect(mocks.dockerApi.listDockerContainers).not.toHaveBeenCalled();
  });

  /** 主机菜单应在对应 SSH Tab 打开右栏容器工具，而不再切换左栏视图。 */
  it("opens SSH host containers in the right tool panel from the context menu", async () => {
    const user = userEvent.setup();
    const hostId = "db980b17-2ed0-44e5-b72a-6ecadf788439";
    const runtime = "docker" as const;
    const api = {
      capabilities: dockerContainerTargetCapabilities,
      hostId,
      id: "c0ffee1234567890",
      image: "kerminal/api:latest",
      name: "api",
      ports: [],
      runtime,
      shortId: "c0ffee123456",
      state: "running" as const,
      status: "running" as const,
      statusText: "Up 12 minutes",
      target: dockerContainerTarget({
        containerId: "c0ffee1234567890",
        containerName: "api",
        hostId,
        runtime,
      }),
    };
    mocks.dockerApi.listDockerContainers.mockResolvedValue([api]);

    render(<KerminalShell />);

    const hostButton = await findExpandedSidebarMachine(/172\.16\.41\.60/);
    fireEvent.contextMenu(hostButton);
    await user.click(screen.getByRole("menuitem", { name: "容器" }));

    const sidebar = screen.getByRole("complementary", { name: "主机侧边栏" });
    expect(within(sidebar).queryByRole("button", { name: "容器" })).not.toBeInTheDocument();

    await waitFor(() => {
      const state = useWorkspaceStore.getState();
      expect(state.terminalTabs.some((tab) => tab.machineId === hostId)).toBe(true);
      expect(state.activeTabId).toBe(
        state.terminalTabs.find((tab) => tab.machineId === hostId)?.id,
      );
      expect(state.openToolsByTabId[state.activeTabId]).toContain("containers");
    });

    const selectedContainerTool = await screen.findByRole("button", {
      name: "收起 容器",
    });
    expect(selectedContainerTool).toHaveAttribute("aria-pressed", "true");
    const containerPanel = await screen.findByTestId("host-containers-tool-content");
    expect(containerPanel).toHaveTextContent("Docker");
    expect(
      await within(containerPanel).findByRole(
        "button",
        { name: "进入容器 api" },
        { timeout: 5000 },
      ),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("dialog", { name: /容器/ }),
    ).not.toBeInTheDocument();
    expect(mocks.dockerApi.listDockerContainers).toHaveBeenCalledWith({
      hostId,
      includeStopped: true,
      runtime,
    });
  });

  it("runs IDEA-style settings and terminal shortcuts", async () => {
    render(<KerminalShell />);

    await waitFor(() => {
      expect(mocks.settingsApi.getSettings).toHaveBeenCalled();
    });

    fireEvent.keyDown(window, {
      altKey: true,
      ctrlKey: true,
      key: "s",
    });
    expect(
      await screen.findByRole("dialog", { name: "设置" }),
    ).toBeInTheDocument();

    fireEvent.keyDown(window, {
      ctrlKey: true,
      key: "t",
      shiftKey: true,
    });

    await waitFor(() => {
      expect(mocks.terminalApi.createTerminalSession).toHaveBeenCalled();
    });
  });

  it("does not show settings in the compact right tool rail", async () => {
    const originalInnerWidth = window.innerWidth;
    Object.defineProperty(window, "innerWidth", {
      configurable: true,
      value: 390,
    });
    fireEvent(window, new Event("resize"));

    try {
      render(<KerminalShell />);

      const toolPanel = await screen.findByRole("complementary", {
        name: "右侧工具栏",
      });
      expect(
        screen.queryByRole("complementary", { name: "主机侧边栏" }),
      ).not.toBeInTheDocument();

      expect(
        within(toolPanel).queryByRole("button", { name: "打开 设置" }),
      ).not.toBeInTheDocument();
      expect(toolPanel).toHaveAttribute("aria-expanded", "false");
    } finally {
      Object.defineProperty(window, "innerWidth", {
        configurable: true,
        value: originalInnerWidth,
      });
      fireEvent(window, new Event("resize"));
    }
  });

  it("opens and closes the tool drawer at the 899px compact breakpoint", async () => {
    const originalInnerWidth = window.innerWidth;
    Object.defineProperty(window, "innerWidth", {
      configurable: true,
      value: 899,
    });
    fireEvent(window, new Event("resize"));

    try {
      render(<KerminalShell />);

      const collapsedPanel = await screen.findByRole("complementary", {
        name: "右侧工具栏",
      });
      fireEvent.click(
        within(collapsedPanel).getByRole("button", {
          name: "打开 当前上下文",
        }),
      );

      const drawer = await screen.findByRole("dialog", {
        name: "紧凑工具面板",
      });
      expect(drawer).toBeVisible();
      expect(
        within(drawer).getByRole("complementary", { name: "工具面板" }),
      ).toHaveAttribute("aria-expanded", "true");

      fireEvent.keyDown(window, { key: "Escape" });

      await waitFor(() => {
        expect(
          screen.queryByRole("dialog", { name: "紧凑工具面板" }),
        ).not.toBeInTheDocument();
      });
      expect(
        screen.getByRole("complementary", { name: "右侧工具栏" }),
      ).toHaveAttribute("aria-expanded", "false");
    } finally {
      Object.defineProperty(window, "innerWidth", {
        configurable: true,
        value: originalInnerWidth,
      });
      fireEvent(window, new Event("resize"));
    }
  });

  it("uses saved custom keybindings at runtime", async () => {
    mocks.settingsApi.getSettings.mockResolvedValue({
      ...defaultAppSettings,
      keybindings: defaultAppSettings.keybindings.map((keybinding) =>
        keybinding.action === "settings.open"
          ? {
              ...keybinding,
              binding: "Ctrl+Alt+,",
              windowsBinding: "Ctrl+Alt+,",
            }
          : keybinding,
      ),
      themeMode: "light",
    });

    render(<KerminalShell />);

    await waitFor(() => {
      expect(document.documentElement).toHaveAttribute("data-theme", "light");
    });
    fireEvent.keyDown(window, {
      altKey: true,
      ctrlKey: true,
      key: ",",
    });

    expect(
      await screen.findByRole("dialog", { name: "设置" }),
    ).toBeInTheDocument();
  });

}
