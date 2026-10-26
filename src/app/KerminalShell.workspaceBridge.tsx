// @author kongweiguang

import { useCallback, useMemo, useSyncExternalStore } from "react";
import { MachineSidebar } from "../features/machine-sidebar/MachineSidebar";
import type { MachineSidebarProps } from "../features/machine-sidebar/MachineSidebar.shared";
import type {
  SftpTransferCreatedHostTarget,
  SftpTransferCreateHostRequest,
} from "../features/sftp/SftpTransferWorkbench";
import type {
  InterfaceDensity,
  ResolvedTheme,
  TerminalAppearance,
  AppSettings,
} from "../features/settings/settingsModel";
import { LazySftpTransferWorkbench as SftpTransferWorkbench } from "../features/sftp/LazySftpTransferWorkbench";
import { LazyWorkspaceFileTabSurface as WorkspaceFileTabSurface } from "../features/workspace/LazyWorkspaceFileTabSurface";
import {
  type BroadcastCommandRequest,
  type BroadcastCommandResult,
  TerminalWorkspace,
} from "../features/terminal/TerminalWorkspace";
import type { TerminalSplitDropIndicator } from "../features/terminal/TerminalSplitDropOverlay";
import type { ConnectionState } from "../features/terminal/XtermPane.helpers";
import { ToolPanel, type ContainerToolActions } from "../features/tool-panel/ToolPanel";
import type { HostContainerRevealRequest } from "../features/tool-panel/toolPanelContextModel";
import type {
  MachineGroup,
  MachineStatus,
  TerminalSplitDirection,
  ToolId,
} from "../features/workspace/types";
import {
  isSftpTransferWorkspaceTab,
  isWorkspaceFileTab,
} from "../features/workspace/types";
import {
  tools,
  useWorkspaceStore,
  type AddTerminalTabOptions,
  type OpenWorkspaceFileTabOptions,
  type TmuxAttachPlacement,
} from "../features/workspace/workspaceStore";
import type { SettingsSectionId } from "../features/settings/SettingsToolContent";
import type { TmuxAttachLaunch } from "../lib/tmuxApi";
import {
  buildOpenMachineIdsSnapshot,
  buildTerminalWorkspaceSnapshot,
  buildToolPanelWorkspaceContext,
  buildToolPanelWorkspaceSnapshot,
  parseOpenMachineIdsSnapshot,
  parseTerminalWorkspaceSnapshot,
} from "./KerminalShell.workspaceSelectors";

type WorkspaceTerminalSurfaceProps = {
  backgroundImageVisible: boolean;
  contentLeftInset: number;
  contentRightInset: number;
  createdSftpHostTarget?: SftpTransferCreatedHostTarget;
  desktopNotifications: AppSettings["desktopNotifications"];
  interfaceDensity: InterfaceDensity;
  machineGroups: MachineGroup[];
  onBroadcastCommand: (
    request: BroadcastCommandRequest,
  ) => Promise<BroadcastCommandResult>;
  onCreateSftpHost?: (request: SftpTransferCreateHostRequest) => void;
  onCloseConfirmedTabs: (tabIds: string[]) => void;
  onOpenAgentTool: () => void;
  onOpenConnection: () => void;
  onOpenLogs: () => void;
  leftTitleBarInset: number;
  rightTitleBarInset: number;
  resolvedTheme: ResolvedTheme;
  splitDropIndicator?: TerminalSplitDropIndicator | null;
  terminalAppearance: TerminalAppearance;
};

type MachineSidebarStoreBridgeProps = Omit<
  MachineSidebarProps,
  "openMachineIds"
>;

interface ToolPanelStoreBridgeProps {
  containerActions?: ContainerToolActions;
  containerRevealRequest?: HostContainerRevealRequest;
  activeTool: ToolId | null;
  activeTools?: readonly ToolId[];
  defaultRemoteGroupId?: string;
  defaultRemoteHostId?: string;
  machineGroups: MachineGroup[];
  onActiveToolChange: (toolId: ToolId) => void;
  onOpenTool?: (toolId: ToolId) => void;
  onCreateTerminal?: (options?: AddTerminalTabOptions) => void;
  onFocusTab?: (tabId: string) => void;
  onOpenToolRailCustomization?: () => void;
  onOpenSettingsSection?: (sectionId: SettingsSectionId) => void;
  onOpenSshTerminal?: (hostId: string) => void;
  onOpenWorkspaceFileTab?: (options: OpenWorkspaceFileTabOptions) => void;
  onOpenTmuxTerminal?: (
    launch: TmuxAttachLaunch,
    placement?: TmuxAttachPlacement,
  ) => void;
  onRemoteHostCreated?: () => void | Promise<void>;
  onConfirmedSettingsChange?: (
    settings: AppSettings,
  ) => Promise<AppSettings>;
  onSettingsChange?: (settings: AppSettings) => void;
  onSplitPane?: (direction: TerminalSplitDirection) => void;
  resolvedTheme: ResolvedTheme;
  settings: AppSettings;
  showRail?: boolean;
  visible?: boolean;
  snippetConfigRevision?: number;
  terminalAppearance: TerminalAppearance;
  workflowConfigRevision?: number;
}

const subscribeToWorkspaceStore = (onStoreChange: () => void) =>
  useWorkspaceStore.subscribe(onStoreChange);

const getOpenMachineIdsSnapshot = () =>
  buildOpenMachineIdsSnapshot(useWorkspaceStore.getState());

const getToolPanelWorkspaceSnapshot = () =>
  buildToolPanelWorkspaceSnapshot(useWorkspaceStore.getState());

const getTerminalWorkspaceSnapshot = () =>
  buildTerminalWorkspaceSnapshot(useWorkspaceStore.getState());

function paneStatusForConnectionState(state: ConnectionState): MachineStatus {
  if (state === "connected") {
    return "online";
  }
  if (state === "connecting" || state === "error") {
    return "warning";
  }
  return "offline";
}

/**
 * 将 Shell 设置与 workspace store 快照接入终端工作区；左右停靠宽度只作用于
 * 导航栏下方内容，避免展示层读取全局布局或让工具面板挤占终端导航。
 */
export function WorkspaceTerminalSurface({
  backgroundImageVisible,
  contentLeftInset,
  contentRightInset,
  createdSftpHostTarget,
  desktopNotifications,
  interfaceDensity,
  leftTitleBarInset,
  machineGroups,
  onBroadcastCommand,
  onCreateSftpHost,
  onCloseConfirmedTabs,
  onOpenAgentTool,
  onOpenConnection,
  onOpenLogs,
  rightTitleBarInset,
  resolvedTheme,
  splitDropIndicator,
  terminalAppearance,
}: WorkspaceTerminalSurfaceProps) {
  const openWorkspaceFileTab = useWorkspaceStore(
    (state) => state.openWorkspaceFileTab,
  );
  const terminalWorkspaceSnapshot = useSyncExternalStore(
    subscribeToWorkspaceStore,
    getTerminalWorkspaceSnapshot,
    getTerminalWorkspaceSnapshot,
  );
  const terminalWorkspace = useMemo(
    () => parseTerminalWorkspaceSnapshot(terminalWorkspaceSnapshot),
    [terminalWorkspaceSnapshot],
  );
  const closePane = useWorkspaceStore((state) => state.closePane);
  const setWorkspaceFileTabDirty = useWorkspaceStore(
    (state) => state.setWorkspaceFileTabDirty,
  );
  const revealWorkspaceFileInSftp = useWorkspaceStore(
    (state) => state.revealWorkspaceFileInSftp,
  );
  const addTerminalTab = useWorkspaceStore((state) => state.addTerminalTab);
  const openContainerTerminal = useWorkspaceStore(
    (state) => state.openContainerTerminal,
  );
  const openSerialTerminal = useWorkspaceStore(
    (state) => state.openSerialTerminal,
  );
  const openSshTerminal = useWorkspaceStore((state) => state.openSshTerminal);
  const openTelnetTerminal = useWorkspaceStore(
    (state) => state.openTelnetTerminal,
  );
  const profiles = useWorkspaceStore((state) => state.profiles);
  /**
   * Tab 栏入口始终创建 workspace 范围终端；显式 Profile 只替换运行参数来源，
   * 不允许这个快捷入口反向生成或选中左栏本地 Machine。
   */
  const createWorkspaceTerminal = useCallback(
    (profileId?: string) =>
      addTerminalTab({
        localMachineScope: "workspace",
        ...(profileId ? { profileId } : {}),
      }),
    [addTerminalTab],
  );
  /**
   * 右键菜单只负责按已保存 Machine 类型分派到现有 action；凭据解析、复用和
   * runtime 回收继续由各协议 slice 负责，避免 TabBar 成为第二套连接实现。
   */
  const openSavedTerminal = useCallback(
    (machineId: string) => {
      const machine = machineGroups
        .flatMap((group) => group.machines)
        .find((candidate) => candidate.id === machineId);
      if (machine?.kind === "ssh") {
        openSshTerminal(machineId);
      } else if (machine?.kind === "telnet") {
        openTelnetTerminal(machineId);
      } else if (machine?.kind === "serial") {
        openSerialTerminal(machineId);
      } else if (machine?.kind === "dockerContainer") {
        openContainerTerminal(machineId);
      }
    },
    [
      machineGroups,
      openContainerTerminal,
      openSerialTerminal,
      openSshTerminal,
      openTelnetTerminal,
    ],
  );
  const focusPane = useWorkspaceStore((state) => state.focusPane);
  const moveTerminalPane = useWorkspaceStore((state) => state.moveTerminalPane);
  const renameTerminalTab = useWorkspaceStore(
    (state) => state.renameTerminalTab,
  );
  const selectTab = useWorkspaceStore((state) => state.selectTab);
  const setBroadcastDraft = useWorkspaceStore(
    (state) => state.setBroadcastDraft,
  );
  const splitFocusedPane = useWorkspaceStore((state) => state.splitFocusedPane);
  const createTerminalTabGroup = useWorkspaceStore(
    (state) => state.createTerminalTabGroup,
  );
  const updateTerminalTabGroup = useWorkspaceStore(
    (state) => state.updateTerminalTabGroup,
  );
  const setTerminalTabGroupCollapsed = useWorkspaceStore(
    (state) => state.setTerminalTabGroupCollapsed,
  );
  const moveTerminalTab = useWorkspaceStore((state) => state.moveTerminalTab);
  const moveTerminalTabGroup = useWorkspaceStore(
    (state) => state.moveTerminalTabGroup,
  );
  const removeTerminalTabFromGroup = useWorkspaceStore(
    (state) => state.removeTerminalTabFromGroup,
  );
  const ungroupTerminalTabGroup = useWorkspaceStore(
    (state) => state.ungroupTerminalTabGroup,
  );
  const updatePaneCurrentCwd = useWorkspaceStore(
    (state) => state.updatePaneCurrentCwd,
  );
  const updatePaneOutputHistory = useWorkspaceStore(
    (state) => state.updatePaneOutputHistory,
  );
  const updatePaneStatus = useWorkspaceStore((state) => state.updatePaneStatus);
  const updateTerminalSplitLayoutSizes = useWorkspaceStore(
    (state) => state.updateTerminalSplitLayoutSizes,
  );
  const resolvePaneLines = useCallback((paneId: string) => {
    return (
      useWorkspaceStore
        .getState()
        .terminalPanes.find((pane) => pane.id === paneId)?.lines ?? []
    );
  }, []);
  const resolvePaneOutputHistory = useCallback((paneId: string) => {
    return useWorkspaceStore
      .getState()
      .terminalPanes.find((pane) => pane.id === paneId)?.outputHistory;
  }, []);

  return (
    <TerminalWorkspace
      activeTabId={terminalWorkspace.activeTabId}
      backgroundImageVisible={backgroundImageVisible}
      broadcastDraft={terminalWorkspace.broadcastDraft}
      contentLeftInset={contentLeftInset}
      contentRightInset={contentRightInset}
      focusedPaneId={terminalWorkspace.focusedPaneId}
      interfaceDensity={interfaceDensity}
      machineGroups={machineGroups}
      onBroadcastCommand={onBroadcastCommand}
      onBroadcastDraftChange={setBroadcastDraft}
      onClosePane={closePane}
      onCloseTabs={onCloseConfirmedTabs}
      onCreateTerminal={createWorkspaceTerminal}
      onFocusPane={focusPane}
      onOpenAgentTool={onOpenAgentTool}
      onOpenConnection={onOpenConnection}
      onOpenSavedTerminal={openSavedTerminal}
      onMovePane={moveTerminalPane}
      onMoveTerminalTab={moveTerminalTab}
      onMoveTerminalTabGroup={moveTerminalTabGroup}
      onPaneConnectionStateChange={(paneId, state) =>
        updatePaneStatus(paneId, paneStatusForConnectionState(state))
      }
      onPaneCurrentCwdChange={updatePaneCurrentCwd}
      onPaneOutputHistoryChange={updatePaneOutputHistory}
      onSplitLayoutSizesChange={updateTerminalSplitLayoutSizes}
      onOpenLogs={onOpenLogs}
      onRevealWorkspaceFileInSftp={revealWorkspaceFileInSftp}
      onRenameTab={renameTerminalTab}
      onCreateTerminalTabGroup={createTerminalTabGroup}
      onUpdateTerminalTabGroup={updateTerminalTabGroup}
      onSetTerminalTabGroupCollapsed={setTerminalTabGroupCollapsed}
      onRemoveTerminalTabFromGroup={removeTerminalTabFromGroup}
      onUngroupTerminalTabGroup={ungroupTerminalTabGroup}
      leftTitleBarInset={leftTitleBarInset}
      rightTitleBarInset={rightTitleBarInset}
      resolvePaneLines={resolvePaneLines}
      resolvePaneOutputHistory={resolvePaneOutputHistory}
      renderCustomTab={(tab, active) =>
        isSftpTransferWorkspaceTab(tab) ? (
          <SftpTransferWorkbench
            active={active}
            createdHostTarget={createdSftpHostTarget}
            desktopNotifications={desktopNotifications}
            groups={machineGroups}
            externalLaunchId={tab.externalLaunchId}
            initialRightHostId={tab.rightHostId}
            initialRightPath={tab.initialRightPath}
            initialRightSelection={tab.initialRightSelection}
            interfaceDensity={interfaceDensity}
            lockedLeftHostId={tab.lockedLeftHostId}
            onCreateSshHost={onCreateSftpHost}
            onOpenWorkspaceFileTab={openWorkspaceFileTab}
            workspaceTabId={tab.id}
          />
        ) : isWorkspaceFileTab(tab) ? (
          <WorkspaceFileTabSurface
            active={active}
            onDirtyChange={(dirty) => setWorkspaceFileTabDirty(tab.id, dirty)}
            tab={tab}
            terminalAppearance={terminalAppearance}
          />
        ) : null
      }
      onSelectTab={selectTab}
      onSplitPane={splitFocusedPane}
      panes={terminalWorkspace.terminalPanes}
      profiles={profiles}
      resolvedTheme={resolvedTheme}
      splitDropIndicator={splitDropIndicator}
      tabs={terminalWorkspace.terminalTabs}
      terminalTabGroups={terminalWorkspace.terminalTabGroups}
      tabGroupPreferences={terminalWorkspace.terminalTabGroupPreferences}
      terminalAppearance={terminalAppearance}
      workspaceFileDirtyState={terminalWorkspace.workspaceFileDirtyState}
    />
  );
}

export function MachineSidebarStoreBridge(
  props: MachineSidebarStoreBridgeProps,
) {
  const openMachineIdsSnapshot = useSyncExternalStore(
    subscribeToWorkspaceStore,
    getOpenMachineIdsSnapshot,
    getOpenMachineIdsSnapshot,
  );
  const openMachineIds = useMemo(
    () => parseOpenMachineIdsSnapshot(openMachineIdsSnapshot),
    [openMachineIdsSnapshot],
  );

  return <MachineSidebar {...props} openMachineIds={openMachineIds} />;
}

/** 目标快照和主机目录在同一宿主解析，容器 pane 无需注册为左栏快捷项也能找到宿主机。 */
export function ToolPanelStoreBridge({
  machineGroups,
  ...props
}: ToolPanelStoreBridgeProps) {
  const toolPanelWorkspaceSnapshot = useSyncExternalStore(
    subscribeToWorkspaceStore,
    getToolPanelWorkspaceSnapshot,
    getToolPanelWorkspaceSnapshot,
  );
  const workspaceContext = useMemo(
    () => {
      void toolPanelWorkspaceSnapshot;
      return buildToolPanelWorkspaceContext(
        useWorkspaceStore.getState(),
        machineGroups,
      );
    },
    [machineGroups, toolPanelWorkspaceSnapshot],
  );
  const closePane = useWorkspaceStore((state) => state.closePane);
  const openTmuxAttachTerminal = useWorkspaceStore(
    (state) => state.openTmuxAttachTerminal,
  );
  const openWorkspaceFileTab = useWorkspaceStore(
    (state) => state.openWorkspaceFileTab,
  );
  const workspaceFileDirtyState = useWorkspaceStore(
    (state) => state.workspaceFileDirtyState,
  );

  return (
    <ToolPanel
      {...props}
      machineGroups={machineGroups}
      activeMachine={workspaceContext.activeMachine}
      activeTab={workspaceContext.activeTab}
      focusedPane={workspaceContext.focusedPane}
      selectedMachine={workspaceContext.selectedMachine}
      workspaceContext={workspaceContext.projection}
      onClosePane={closePane}
      onOpenWorkspaceFileTab={openWorkspaceFileTab}
      onOpenTmuxTerminal={openTmuxAttachTerminal}
      terminalPanes={workspaceContext.terminalPanes}
      terminalTabs={workspaceContext.terminalTabs}
      sftpRevealRequest={workspaceContext.sftpRevealRequest}
      workspaceFileDirtyState={workspaceFileDirtyState}
      tools={tools}
    />
  );
}
