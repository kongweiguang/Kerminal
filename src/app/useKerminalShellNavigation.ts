// @author kongweiguang

import { useCallback } from "react";
import type { HostContainerRevealRequest } from "../features/tool-panel/toolPanelContextModel";
import { openToolPanel } from "../features/tool-panel";
import type { MachineGroup, TerminalLayoutNode, ToolId } from "../features/workspace/types";
import { isTerminalSessionTab } from "../features/workspace/types";
import {
  findMachine,
  useWorkspaceStore,
  type WorkspaceState,
} from "../features/workspace/workspaceStore";
import type { ContainerRuntime } from "../lib/targetModel";
import type { DockerContainerSummary } from "../lib/dockerApi";
import { shellQuote } from "./KerminalShell.contextWorkspaceShellHelpers";

interface UseKerminalShellNavigationOptions {
  machineGroups: MachineGroup[];
  openDockerContainerTerminal: WorkspaceState["openDockerContainerTerminal"];
  openSftpTransferTab: WorkspaceState["openSftpTransferTab"];
  openSshCommandTerminal: WorkspaceState["openSshCommandTerminal"];
  selectMachine: WorkspaceState["selectMachine"];
  closeTool: (toolId: ToolId) => void;
  openTool: (toolId: ToolId) => void;
  onRevealHostContainers: (request: HostContainerRevealRequest) => void;
}

/**
 * 统一 Shell 的主机、SFTP 与容器导航；容器入口在 store 更新后再打开，避免
 * 使用旧 Tab 闭包覆盖新建或聚焦 Tab 的工具状态。
 */
export function useKerminalShellNavigation({
  machineGroups,
  openDockerContainerTerminal,
  openSftpTransferTab,
  openSshCommandTerminal,
  selectMachine,
  closeTool,
  openTool,
  onRevealHostContainers,
}: UseKerminalShellNavigationOptions) {
  /** SFTP host 工具沿用所选 Machine，避免改变原有目标语义。 */
  const openSftpForMachine = useCallback(
    (machineId: string) => {
      selectMachine(machineId);
      openTool("sftp");
    },
    [openTool, selectMachine],
  );

  /** 传输工作台属于独立 Tab，关闭重复的内嵌 SFTP 面板入口。 */
  const openSftpTransferWorkbench = useCallback(
    (machineId?: string) => {
      openSftpTransferTab(machineId ? { rightHostId: machineId } : undefined);
      closeTool("sftp");
    },
    [closeTool, openSftpTransferTab],
  );

  /** 显式指定宿主机时聚焦其 SSH pane，并在最新活动 Tab 打开容器面板。 */
  const openHostContainersPanel = useCallback(
    (machineId: string, initialContainerId?: string, runtime?: ContainerRuntime) => {
      const host = findMachine(machineGroups, machineId);
      if (host?.kind !== "ssh") {
        return;
      }

      const workspace = useWorkspaceStore.getState();
      const existing = findHostTerminal(workspace, host.id);
      if (existing) {
        workspace.selectTab(existing.tabId);
        workspace.focusPane(existing.paneId);
      } else {
        workspace.openSshTerminal(host.id);
      }

      const latest = useWorkspaceStore.getState();
      if (!latest.activeTabId) {
        return;
      }
      latest.setOpenTools(
        openToolPanel(
          latest.openTools,
          "containers",
          latest.settings.toolRail,
          typeof window !== "undefined" && window.innerWidth < 900,
        ),
        "containers",
      );

      const ready = useWorkspaceStore.getState();
      if (!ready.activeTabId) {
        return;
      }
      onRevealHostContainers({
        hostId: host.id,
        tabId: ready.activeTabId,
        ...(initialContainerId ? { containerId: initialContainerId } : {}),
        ...(runtime ? { runtime } : {}),
      });
    },
    [machineGroups, onRevealHostContainers],
  );

  /** 固定容器只解析到明确的 SSH 父主机，并携带运行时与容器身份。 */
  const openContainerDetails = useCallback(
    (machineId: string) => {
      const machine = findMachine(machineGroups, machineId);
      if (
        machine?.kind !== "dockerContainer" ||
        !machine.parentMachineId ||
        !machine.containerId
      ) {
        return;
      }
      openHostContainersPanel(
        machine.parentMachineId,
        machine.containerId,
        machine.runtime,
      );
    },
    [machineGroups, openHostContainersPanel],
  );

  /** 进入终端沿用 workspace tab transition，保留其各自的右栏状态。 */
  const enterHostContainer = useCallback(
    (container: DockerContainerSummary) => {
      openDockerContainerTerminal(container);
    },
    [openDockerContainerTerminal],
  );

  /** 日志创建绑定原容器宿主机的命令 Tab，不收起其他工具。 */
  const openHostContainerLogs = useCallback(
    (container: DockerContainerSummary) => {
      const runtimeBin = container.runtime === "podman" ? "podman" : "docker";
      openSshCommandTerminal(container.hostId, {
        remoteCommand: `${runtimeBin} logs -f --tail 200 ${shellQuote(container.id)}`,
        title: `${container.name} logs`,
      });
    },
    [openSshCommandTerminal],
  );

  return {
    enterHostContainer,
    openContainerDetails,
    openHostContainerLogs,
    openHostContainersPanel,
    openSftpForMachine,
    openSftpTransferWorkbench,
  };
}

/** 查找目标 SSH 主机的普通 pane，避免重用仍在运行的日志命令 Tab。 */
function findHostTerminal(
  state: WorkspaceState,
  hostId: string,
): { tabId: string; paneId: string } | undefined {
  const tabs = [...state.terminalTabs];
  tabs.sort(
    (left, right) =>
      Number(right.id === state.activeTabId) - Number(left.id === state.activeTabId),
  );
  for (const tab of tabs) {
    if (!isTerminalSessionTab(tab)) {
      continue;
    }
    for (const paneId of paneIds(tab.layout)) {
      const pane = state.terminalPanes.find((candidate) => candidate.id === paneId);
      if (
        pane?.mode === "ssh" &&
        !pane.remoteCommand &&
        (pane.remoteHostId ?? pane.machineId) === hostId
      ) {
        return { tabId: tab.id, paneId };
      }
    }
  }
  return undefined;
}

/** 保持分屏布局的遍历顺序，令复用目标稳定且可预测。 */
function paneIds(node: TerminalLayoutNode): string[] {
  if (node.type === "pane") {
    return [node.paneId];
  }
  return node.children.flatMap(paneIds);
}
