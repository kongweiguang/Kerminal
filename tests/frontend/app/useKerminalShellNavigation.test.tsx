// @author kongweiguang

import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useKerminalShellNavigation } from "../../../src/app/useKerminalShellNavigation";
import type { HostContainerRevealRequest } from "../../../src/features/tool-panel/toolPanelContextModel";
import type { Machine, MachineGroup, TerminalPane, TerminalTab } from "../../../src/features/workspace/types";
import { useWorkspaceStore } from "../../../src/features/workspace/workspaceStore";
import {
  apiContainer,
  resetWorkspaceStore,
} from "../support/workspace/workspaceStore.testSupport";

const host: Machine = {
  description: "root@lab.internal:22",
  host: "lab.internal",
  id: apiContainer.hostId,
  kind: "ssh",
  name: "实验室",
  port: 22,
  status: "online",
  tags: ["ssh"],
  username: "root",
};

const containerMachine: Machine = {
  containerId: apiContainer.id,
  containerName: apiContainer.name,
  description: "容器",
  id: "container-machine",
  kind: "dockerContainer",
  name: apiContainer.name,
  parentMachineId: apiContainer.hostId,
  runtime: "podman",
  status: "online",
  tags: ["container"],
};

const machineGroups: MachineGroup[] = [
  {
    id: "remote-group",
    title: "远程主机",
    machines: [host, containerMachine],
  },
];

const sshPane = (id: string, machineId: string): TerminalPane => ({
  id,
  lines: [],
  machineId,
  mode: "ssh",
  prompt: "$",
  remoteHostId: machineId,
  status: "online",
  title: machineId,
});

const terminalTab = (
  id: string,
  paneId: string,
  machineId: string,
): TerminalTab => ({
  id,
  layout: { paneId, type: "pane" },
  machineId,
  title: machineId,
});

describe("useKerminalShellNavigation", () => {
  beforeEach(() => {
    resetWorkspaceStore();
    useWorkspaceStore.setState({ machineGroups });
  });

  afterEach(() => {
    resetWorkspaceStore();
  });

  it("keeps SFTP routing on the selected host", () => {
    const options = createOptions();
    const { result } = renderHook(() => useKerminalShellNavigation(options));

    act(() => result.current.openSftpForMachine("host-1"));
    expect(options.selectMachine).toHaveBeenCalledWith("host-1");
    expect(options.openTool).toHaveBeenCalledWith("sftp");

    act(() => result.current.openSftpTransferWorkbench("host-2"));
    expect(options.openSftpTransferTab).toHaveBeenCalledWith({
      rightHostId: "host-2",
    });
    expect(options.closeTool).toHaveBeenCalledWith("sftp");
  });

  it("reuses and focuses an existing SSH pane before revealing the panel", () => {
    useWorkspaceStore.setState({
      activeTabId: "tab-other",
      focusedPaneId: "pane-other",
      settings: {
        ...useWorkspaceStore.getState().settings,
        toolRail: {
          ...useWorkspaceStore.getState().settings.toolRail,
          bottom: ["logs"],
          panelPlacements: {
            ...useWorkspaceStore.getState().settings.toolRail.panelPlacements,
            agentLauncher: "left",
            logs: "bottom",
          },
        },
      },
      terminalPanes: [sshPane("pane-target", host.id), sshPane("pane-other", "host-other")],
      terminalTabs: [
        terminalTab("tab-other", "pane-other", "host-other"),
        {
          id: "tab-target",
          layout: {
            children: [
              { paneId: "pane-other-target", type: "pane" },
              { paneId: "pane-target", type: "pane" },
            ],
            direction: "horizontal",
            id: "split-1",
            type: "split",
          },
          machineId: host.id,
          title: host.name,
        },
      ],
      activeToolByTabId: { "tab-other": "system", "tab-target": "agentLauncher" },
      openToolsByTabId: {
        "tab-other": ["system"],
        "tab-target": ["agentLauncher", "logs"],
      },
      openTools: ["system"],
    });
    useWorkspaceStore.setState((state) => ({
      terminalPanes: [
        ...state.terminalPanes,
        sshPane("pane-other-target", "host-other"),
      ],
    }));
    const options = createOptions();
    const { result } = renderHook(() => useKerminalShellNavigation(options));

    act(() => result.current.openHostContainersPanel(host.id));

    expect(useWorkspaceStore.getState()).toMatchObject({
      activeTabId: "tab-target",
      focusedPaneId: "pane-target",
      openToolsByTabId: {
        "tab-other": ["system"],
        "tab-target": ["agentLauncher", "logs", "containers"],
      },
    });
    expect(options.onRevealHostContainers).toHaveBeenCalledWith({
      hostId: host.id,
      tabId: "tab-target",
    });
  });

  it("creates a host terminal when no ordinary SSH tab is open", () => {
    useWorkspaceStore.setState({ machineGroups, terminalTabs: [], terminalPanes: [] });
    const options = createOptions();
    const { result } = renderHook(() => useKerminalShellNavigation(options));

    act(() => result.current.openHostContainersPanel(host.id));

    const state = useWorkspaceStore.getState();
    expect(state.terminalTabs).toHaveLength(1);
    expect(state.terminalTabs[0].machineId).toBe(host.id);
    expect(state.openToolsByTabId[state.activeTabId]).toContain("containers");
    expect(options.onRevealHostContainers).toHaveBeenCalledWith({
      hostId: host.id,
      tabId: state.activeTabId,
    });
  });

  it("routes a pinned Podman container to its host and preserves its runtime", () => {
    useWorkspaceStore.setState({
      activeTabId: "host-tab",
      focusedPaneId: "host-pane",
      terminalPanes: [sshPane("host-pane", host.id)],
      terminalTabs: [terminalTab("host-tab", "host-pane", host.id)],
    });
    const options = createOptions();
    const { result } = renderHook(() => useKerminalShellNavigation(options));

    act(() => result.current.openContainerDetails(containerMachine.id));

    expect(options.onRevealHostContainers).toHaveBeenCalledWith({
      containerId: apiContainer.id,
      hostId: host.id,
      runtime: "podman",
      tabId: "host-tab",
    } satisfies HostContainerRevealRequest);
    expect(useWorkspaceStore.getState().activeTabId).toBe("host-tab");
  });

  it("ignores invalid host and incomplete container bindings", () => {
    const options = createOptions({ machineGroups: [] });
    const { result } = renderHook(() => useKerminalShellNavigation(options));

    act(() => result.current.openHostContainersPanel(host.id));
    act(() => result.current.openContainerDetails("missing"));

    expect(options.onRevealHostContainers).not.toHaveBeenCalled();
    expect(useWorkspaceStore.getState().terminalTabs).toHaveLength(0);
  });

  it("opens container logs through the host command terminal without closing tools", () => {
    const options = createOptions();
    const { result } = renderHook(() => useKerminalShellNavigation(options));

    act(() =>
      result.current.openHostContainerLogs({
        ...apiContainer,
        id: "container with space",
        runtime: "podman",
      }),
    );

    expect(options.openSshCommandTerminal).toHaveBeenCalledWith(host.id, {
      remoteCommand: "podman logs -f --tail 200 'container with space'",
      title: `${apiContainer.name} logs`,
    });
    expect(options.closeTool).not.toHaveBeenCalled();
  });

  it("opens an entered container terminal without collapsing the active tool", () => {
    const options = createOptions();
    const { result } = renderHook(() => useKerminalShellNavigation(options));

    act(() => result.current.enterHostContainer(apiContainer));

    expect(options.openDockerContainerTerminal).toHaveBeenCalledWith(apiContainer);
    expect(options.closeTool).not.toHaveBeenCalled();
  });
});

/** 每个用例由真实 workspace action 驱动，验证目标和右栏状态的实际联动。 */
function createOptions(overrides: Record<string, unknown> = {}) {
  return {
    machineGroups,
    openDockerContainerTerminal: vi.fn(),
    openSftpTransferTab: vi.fn(),
    openSshCommandTerminal: vi.fn(),
    selectMachine: vi.fn(),
    closeTool: vi.fn(),
    openTool: vi.fn(),
    onRevealHostContainers: vi.fn(),
    ...overrides,
  } as Parameters<typeof useKerminalShellNavigation>[0];
}
