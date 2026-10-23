// @author kongweiguang
import { useCallback, useMemo, useRef, useState } from "react";
import type { MachineSidebarViewMode } from "../features/machine-sidebar/MachineSidebar.shared";
import { resolveThemeMode } from "../features/settings/settingsModel";
import type {
  ToolRailPanelPlacement,
  ToolRailSettings,
} from "../features/tool-panel";
import { writeBroadcastCommand } from "../features/terminal/terminalSessionRegistry";
import { useWorkspaceStore } from "../features/workspace/workspaceStore";
import { resolveDesktopPlatform } from "../lib/desktopPlatform";

import {
  createRemoteHostGroup,
  updateRemoteHost,
} from "../lib/remoteHostApi";
import { useDocumentTheme } from "../lib/useDocumentTheme";
import { useAppUpdateCheck } from "./useAppUpdateCheck";
import { useTauriWindowFrameState } from "../lib/useTauriWindowFrameState";
import { resolveWindowChromeModel } from "../lib/windowChromeModel";
import {
  htmlLanguage,
  useSystemThemePreference,
  useViewportSize,
} from "./KerminalShell.helpers";
import { useKerminalShellRemoteActions } from "./useKerminalShellRemoteActions";
import { useKerminalShellBackgroundStyle } from "./useKerminalShellBackgroundStyle";
import { useKerminalShellCommands } from "./useKerminalShellCommands";
import { useKerminalShellContainerActions } from "./useKerminalShellContainerActions";
import { useKerminalShellNavigation } from "./useKerminalShellNavigation";
import { useKerminalShellConfigRefresh } from "./useKerminalShellConfigRefresh";
import { useKerminalShellPanelResize } from "./useKerminalShellPanelResize";
import { useKerminalShellSettings } from "./useKerminalShellSettings";
import { useKerminalShellSftpHostCreate } from "./useKerminalShellSftpHostCreate";
import { useKerminalShellTabClose } from "./useKerminalShellTabClose";
import { KerminalShellLayout } from "./KerminalShell.layout";
import {
  useKerminalShellRemoteTargetModel,
  useKerminalShellViewModel,
} from "./kerminalShellViewModel";
import { useKerminalShellStartupSync } from "./useKerminalShellStartupSync";
import { useKerminalShellSnippetBridge } from "./useKerminalShellSnippetBridge";
import { useKerminalShellTerminalDrop } from "./useKerminalShellTerminalDrop";
import { useKerminalShellToolPanels } from "./useKerminalShellToolPanels";
import { archiveAgentSessionsForClosedTabs } from "./agentSessionTabCloseCleanup";

/** 主窗口统一承载 rail 编辑与后台更新检查，使设置页挂载次数不影响全局运行状态。 */
export function KerminalShell() {
  const activeTabId = useWorkspaceStore((state) => state.activeTabId);
  const activeTool = useWorkspaceStore((state) => state.activeTool);
  const openTools = useWorkspaceStore((state) => state.openTools);
  const focusedPaneId = useWorkspaceStore((state) => state.focusedPaneId);
  const machineGroups = useWorkspaceStore((state) => state.machineGroups);
  const machineSearch = useWorkspaceStore((state) => state.machineSearch);
  const selectedMachineId = useWorkspaceStore(
    (state) => state.selectedMachineId,
  );
  const addDockerContainer = useWorkspaceStore(
    (state) => state.addDockerContainer,
  );
  const addLocalProfileMachine = useWorkspaceStore(
    (state) => state.addLocalProfileMachine,
  );
  const focusPane = useWorkspaceStore((state) => state.focusPane);
  const addTerminalTab = useWorkspaceStore((state) => state.addTerminalTab);
  const closePane = useWorkspaceStore((state) => state.closePane);
  const closeTerminalTab = useWorkspaceStore((state) => state.closeTerminalTab);
  const openLocalTerminal = useWorkspaceStore(
    (state) => state.openLocalTerminal,
  );
  const openContainerTerminal = useWorkspaceStore(
    (state) => state.openContainerTerminal,
  );
  const openDockerContainerTerminal = useWorkspaceStore(
    (state) => state.openDockerContainerTerminal,
  );
  const openSshTerminal = useWorkspaceStore((state) => state.openSshTerminal);
  const openSshCommandTerminal = useWorkspaceStore(
    (state) => state.openSshCommandTerminal,
  );
  const openTelnetTerminal = useWorkspaceStore(
    (state) => state.openTelnetTerminal,
  );
  const openSerialTerminal = useWorkspaceStore(
    (state) => state.openSerialTerminal,
  );
  const openSftpTransferTab = useWorkspaceStore(
    (state) => state.openSftpTransferTab,
  );
  const openWorkspaceFileTab = useWorkspaceStore(
    (state) => state.openWorkspaceFileTab,
  );
  const removeSidebarMachine = useWorkspaceStore(
    (state) => state.removeSidebarMachine,
  );
  const renameMachineGroup = useWorkspaceStore(
    (state) => state.renameMachineGroup,
  );
  const selectMachine = useWorkspaceStore((state) => state.selectMachine);
  const selectTab = useWorkspaceStore((state) => state.selectTab);
  const setOpenTools = useWorkspaceStore((state) => state.setOpenTools);
  const setMachineSearch = useWorkspaceStore((state) => state.setMachineSearch);
  const setProfiles = useWorkspaceStore((state) => state.setProfiles);
  const setRemoteHostTree = useWorkspaceStore(
    (state) => state.setRemoteHostTree,
  );
  const splitFocusedPane = useWorkspaceStore((state) => state.splitFocusedPane);
  const updateLocalMachine = useWorkspaceStore(
    (state) => state.updateLocalMachine,
  );
  const moveSidebarMachine = useWorkspaceStore(
    (state) => state.moveSidebarMachine,
  );
  const pinMachineGroup = useWorkspaceStore((state) => state.pinMachineGroup);
  const terminalTabs = useWorkspaceStore((state) => state.terminalTabs);
  const workspaceFileDirtyState = useWorkspaceStore(
    (state) => state.workspaceFileDirtyState,
  );
  const profiles = useWorkspaceStore((state) => state.profiles);
  const activeProfileId = useWorkspaceStore((state) => state.activeProfileId);
  const settings = useWorkspaceStore((state) => state.settings);
  useAppUpdateCheck(settings.desktopNotifications);
  const setSettings = useWorkspaceStore((state) => state.setSettings);
  const [toolRailCustomizationOpen, setToolRailCustomizationOpen] =
    useState(false);
  const viewportSize = useViewportSize();
  const [shellNoticeVisible, setShellNoticeVisible] = useState(false);
  const [workspaceSessionPersistenceNotice, setWorkspaceSessionPersistenceNotice] =
    useState<string | null>(null);
  const [machineSidebarView, setMachineSidebarView] =
    useState<MachineSidebarViewMode>("hosts");
  const [hostContainersHostId, setHostContainersHostId] = useState<
    string | null
  >(null);
  const [
    hostContainersInitialContainerId,
    setHostContainersInitialContainerId,
  ] = useState<string>();
  const workspaceFrameRef = useRef<HTMLDivElement>(null);
  const {
    handleSettingsChange,
    handleConfirmedSettingsChange,
    handleSettingsDialogChange,
    handleSettingsDialogClose,
    openSettingsTool,
    settingsDialogDirtyRef,
    settingsDialogOpen,
    settingsDialogOpenRef,
    settingsInitialSectionId,
    settingsLoadError,
    settingsSaveError,
    settingsSaveState,
    settingsSaveStateRef,
  } = useKerminalShellSettings({ setSettings });
  /** 工具栏编辑器是全局偏好入口，状态放在 Shell 以跨桌面/紧凑 rail 保持一致。 */
  const openToolRailCustomization = useCallback(() => {
    setToolRailCustomizationOpen(true);
  }, []);
  /** 关闭只撤销弹窗状态，不触碰当前 tab 的活动工具或 Agent 绑定。 */
  const closeToolRailCustomization = useCallback(() => {
    setToolRailCustomizationOpen(false);
  }, []);
  /** 只把 rail patch 合并进最新全局 settings，保存失败时由弹窗保留草稿重试。 */
  const saveToolRailSettings = useCallback(
    async (toolRail: ToolRailSettings) => {
      const latestSettings = useWorkspaceStore.getState().settings;
      await handleConfirmedSettingsChange({ ...latestSettings, toolRail });
    },
    [handleConfirmedSettingsChange],
  );
  const systemPrefersDark = useSystemThemePreference();
  const resolvedTheme = resolveThemeMode(settings.themeMode, systemPrefersDark);
  const {
    closeAllTools,
    closeTool,
    openPanels: openToolPanels,
    openTool,
    openTools: normalizedOpenTools,
    toggleTool,
  } = useKerminalShellToolPanels({
    activeTool,
    compactShell: viewportSize.width < 900,
    openTools,
    setOpenTools,
    settings: settings.toolRail,
  });
  const openToolPlacements = useMemo(
    () => Object.keys(openToolPanels) as ToolRailPanelPlacement[],
    [openToolPanels],
  );
  const desktopPlatform = resolveDesktopPlatform();
  const windowFrameState = useTauriWindowFrameState();
  const windowChrome = resolveWindowChromeModel({
    frameState: windowFrameState,
    platform: desktopPlatform,
  });
  useDocumentTheme({
    density: settings.interfaceDensity,
    desktopPlatform,
    language: settings.appearance.interfaceLanguage,
    lang: htmlLanguage(settings.appearance.interfaceLanguage),
    theme: resolvedTheme,
    windowFrame: windowFrameState,
  });
  const {
    beginPanelResize,
    collapsedMachineGroupIds,
    compactShell,
    effectiveBottomToolPanelOpen,
    effectiveLeftPanelCollapsed,
    effectiveLeftToolPanelOpen,
    effectiveRightPanelOpen,
    gridTemplateColumns,
    gridTemplateRows,
    handleCollapsedMachineGroupIdsChange,
    handleWorkspaceShellLayoutRestored,
    leftPanelCollapsed,
    leftWorkspaceInset,
    resizeWithKeyboard,
    rightWorkspaceInset,
    setLeftPanelCollapsed,
    workspaceShellLayout,
  } = useKerminalShellPanelResize({
    openToolPlacements,
    viewportHeight: viewportSize.height,
    viewportWidth: viewportSize.width,
    workspaceFrameRef,
  });
  const workspaceBackgroundStyle = useKerminalShellBackgroundStyle({
    resolvedTheme,
    settings,
  });
  const handleTabsClosed = useCallback((tabIds: string[]) => {
    void archiveAgentSessionsForClosedTabs(tabIds).catch((error) => {
      console.error("Failed to archive Agent sessions for closed tabs", error);
    });
  }, []);
  const { defaultRemoteGroupId, defaultRemoteHostId } =
    useKerminalShellRemoteTargetModel(machineGroups);
  const {
    handleExternalMachineDrag,
    handleExternalMachineDragEnd,
    handleExternalMachineDrop,
    terminalSplitDropIndicator,
  } = useKerminalShellTerminalDrop({
    activeTabId,
    focusedPaneId,
    splitFocusedPane,
    terminalTabs,
  });
  const {
    cancelDirtyFileTabs,
    cancelTerminalTabs,
    closeConfirmedTabs,
    confirmDirtyFileTabs,
    confirmTerminalTabs,
    dirtyFileTabCount,
    pendingDirtyFileTabCount,
    pendingTerminalTabCount,
    requestCloseTab,
  } = useKerminalShellTabClose({
    closeTerminalTab,
    confirmTerminalClose: settings.terminal.confirmCloseTab,
    onTabsClosed: handleTabsClosed,
    removeSidebarMachine,
    terminalTabs,
    workspaceFileDirtyState,
  });
  const { activateTool, openLogsTool } = useKerminalShellCommands({
    activeTabId,
    addTerminalTab,
    closeAllTools,
    closePane,
    closeTerminalTab: requestCloseTab,
    focusPane,
    focusedPaneId,
    keybindings: settings.keybindings,
    openSettingsTool,
    openTool,
    selectTab,
    splitFocusedPane,
    terminalTabs,
    toggleTool,
  });
  useKerminalShellSnippetBridge({ focusPane, openTool });
  const {
    enterHostContainer,
    openContainerDetails,
    openHostContainerLogs,
    openHostContainersSidebar,
    openSftpForMachine,
    openSftpTransferWorkbench,
    selectHostContainersHost,
  } = useKerminalShellNavigation({
    activeTool,
    machineGroups,
    openDockerContainerTerminal,
    openSftpTransferTab,
    openSshCommandTerminal,
    selectMachine,
    closeTool,
    openTool,
    setHostContainersHostId,
    setHostContainersInitialContainerId,
    setMachineSidebarView,
  });
  const {
    closeConnectionDialog,
    closeRemoteGroupDialog,
    confirmDelete,
    deleteError,
    deleteSaving,
    editingLocalMachine,
    editingRemoteGroup,
    editingRemoteHost,
    handleCreateLocalProfile,
    handleCreateRemoteHost,
    handleDuplicateMachine,
    handleMoveMachineToGroup,
    handlePinMachineGroup,
    handleRemoteGroupSaved,
    handleRemoteGroupUpdate,
    handleRemoteHostCreated,
    handleUpdateLocalProfile,
    openConnectionDialog,
    openRemoteGroupDialog,
    openSavedRdpMachine,
    pendingDelete,
    profileLoadError,
    refreshProfiles,
    refreshRemoteHostTree,
    rdpOpeningMachineIds,
    remoteGroupDialogOpen,
    remoteHostDefaultGroupId,
    remoteHostDefaultMode,
    remoteHostDialogOpen,
    remoteHostLoadError,
    requestDeleteGroup,
    requestDeleteMachine,
    resolveTargetGroupId,
    setDeleteError,
    setPendingDelete,
    setProfileLoadError,
  } = useKerminalShellRemoteActions({
    activeProfileId,
    addLocalProfileMachine,
    addTerminalTab,
    defaultRemoteGroupId,
    machineGroups,
    moveSidebarMachine,
    pinMachineGroup,
    profiles,
    removeSidebarMachine,
    renameMachineGroup,
    selectMachine,
    setProfiles,
    setRemoteHostTree,
    updateLocalMachine,
  });
  const {
    configCatalogRevisions,
    configNotice,
    configRefreshCoordinator,
    setConfigNotice,
  } = useKerminalShellConfigRefresh({
    machineGroups,
    profiles,
    refreshProfiles,
    refreshRemoteHostTree,
    setSettings,
    settings,
    settingsDialogDirtyRef,
    settingsDialogOpenRef,
    settingsSaveStateRef,
  });
  const {
    fetchContainerStats,
    inspectContainer,
    listDockerContainers: loadDockerContainers,
    pinHostContainer,
    runHostContainerLifecycleAction,
  } = useKerminalShellContainerActions({
    addDockerContainer,
    defaultRemoteGroupId,
    machineGroups,
    resolveTargetGroupId,
  });
  const {
    connectionConfigConflict,
    leftTitleBarInset,
    remoteGroupConfigConflict,
    rightTitleBarInset,
    rightToolRailTitleBarFillWidth,
    shellNoticeMessage,
  } = useKerminalShellViewModel({
    activeTool,
    compactShell,
    editingLocalMachine,
    editingRemoteGroup,
    editingRemoteHost,
    effectiveLeftPanelCollapsed,
    interfaceDensity: settings.interfaceDensity,
    machineGroups,
    profileLoadError,
    remoteHostLoadError,
    settingsLoadError,
    toolPanelDocked: effectiveRightPanelOpen,
    windowChrome,
    workspaceSessionPersistenceNotice,
  });
  const {
    createdSftpHostTarget,
    handleConnectionDialogClose,
    handleConnectionDialogCreated,
    openSftpTransferHostCreateDialog,
  } = useKerminalShellSftpHostCreate({
    closeConnectionDialog,
    handleRemoteHostCreated,
    openConnectionDialog,
  });
  useKerminalShellStartupSync({
    configRefreshCoordinator, handleWorkspaceShellLayoutRestored,
    refreshRemoteHostTree, settingsDialogDirtyRef, settingsSaveState,
    setProfileLoadError, setProfiles, setShellNoticeVisible,
    setWorkspaceSessionPersistenceNotice,
    shellNoticeMessage, workspaceShellLayout,
  });

  return (
    <KerminalShellLayout
      activeTool={activeTool}
      activeTools={normalizedOpenTools}
      compactShell={compactShell}
      contextWorkspaceProps={{
        onOpenSettings: openSettingsTool,
        onOpenTool: openTool,
      }}
      deleteDialogProps={{
        deleteError, deleting: deleteSaving, pendingDelete,
        onConfirm: () => void confirmDelete(),
        onClose: () => {
          if (!deleteSaving) {
            setPendingDelete(null);
            setDeleteError(null);
          }
        },
      }}
      frame={{
        backgroundStyle: workspaceBackgroundStyle, density: settings.interfaceDensity,
        desktopPlatform, gridTemplateColumns, gridTemplateRows,
        lang: htmlLanguage(settings.appearance.interfaceLanguage),
        language: settings.appearance.interfaceLanguage, resolvedTheme,
        windowFrameState, workspaceFrameRef,
      }}
      leftSeparatorProps={{
        className: "kerminal-shell-separator col-[2/3] row-[2/5]",
        hidden: effectiveLeftPanelCollapsed, label: "调整主机侧边栏宽度",
        onKeyDown: (event) => resizeWithKeyboard("left", event),
        onPointerDown: (event) => beginPanelResize("left", event),
      }}
      leftToolSeparatorProps={{
        className: "kerminal-shell-separator relative z-20",
        hidden: !effectiveLeftToolPanelOpen,
        label: "调整左侧工具面板宽度",
        onKeyDown: (event) => resizeWithKeyboard("leftTools", event),
        onPointerDown: (event) => beginPanelResize("leftTools", event),
        style: { gridColumn: "4 / 5", gridRow: "2 / 5" },
      }}
      bottomSeparatorProps={{
        className: "kerminal-shell-separator relative z-20",
        hidden: !effectiveBottomToolPanelOpen,
        label: "调整底部工具面板高度",
        onKeyDown: (event) => resizeWithKeyboard("bottomTools", event),
        onPointerDown: (event) => beginPanelResize("bottomTools", event),
        orientation: "horizontal",
        style: { gridColumn: "5 / 6", gridRow: "3 / 4" },
      }}
      machineSidebarProps={effectiveLeftPanelCollapsed ? null : {
        activeView: machineSidebarView, collapsed: false, collapsedGroupIds: collapsedMachineGroupIds,
        containerHostId: hostContainersHostId, containerInitialContainerId: hostContainersInitialContainerId,
        groups: machineGroups, onActiveViewChange: setMachineSidebarView,
        onAddConnection: openConnectionDialog, onAddGroup: openRemoteGroupDialog,
        onAddMachine: (groupId) => openConnectionDialog({ groupId, mode: "ssh" }),
        onCollapsedGroupIdsChange: handleCollapsedMachineGroupIdsChange,
        onContainerHostChange: selectHostContainersHost, onDeleteGroup: requestDeleteGroup,
        onDeleteMachine: requestDeleteMachine,
        onDuplicateMachine: (machineId) => void handleDuplicateMachine(machineId),
        onEditGroup: openRemoteGroupDialog, onEditMachine: (hostId) => openConnectionDialog({ hostId }),
        onEnterContainer: enterHostContainer, onExternalMachineDrag: handleExternalMachineDrag,
        onExternalMachineDragEnd: handleExternalMachineDragEnd,
        onExternalMachineDrop: handleExternalMachineDrop,
        onFetchContainerStats: fetchContainerStats, onInspectContainer: inspectContainer,
        onLifecycleContainer: runHostContainerLifecycleAction, onListDockerContainers: loadDockerContainers,
        onMoveMachine: (machineId, groupId) => void handleMoveMachineToGroup(machineId, groupId),
        onOpenContainerDetails: openContainerDetails,
        onOpenContainerLogs: openHostContainerLogs, onOpenContainerTerminal: openContainerTerminal,
        onOpenHostContainers: openHostContainersSidebar, onOpenLocalTerminal: openLocalTerminal,
        onOpenRdpConnection: openSavedRdpMachine, onOpenSerialTerminal: openSerialTerminal,
        onOpenSettings: openSettingsTool, onOpenSftp: openSftpForMachine,
        onOpenUpdates: () => openSettingsTool("settings-about"),
        onOpenSftpTransferWorkbench: openSftpTransferWorkbench,
        onOpenSshTerminal: openSshTerminal, onOpenTelnetTerminal: openTelnetTerminal,
        onOpenTransferWorkbench: openSftpTransferWorkbench,
        onOpenWorkspaceFileTab: openWorkspaceFileTab,
        onPinContainer: pinHostContainer,
        onPinGroup: (groupId, pinned) => void handlePinMachineGroup(groupId, pinned),
        onSearchChange: setMachineSearch, onSelectMachine: selectMachine,
        rdpOpeningMachineIds, search: machineSearch, selectedMachineId,
        settingsSelected: settingsDialogOpen,
      }}
      noticesProps={{
        configNotice, shellNoticeMessage, shellNoticeVisible,
        onConfigNoticeDismiss: () => setConfigNotice(null),
        onShellNoticeDismiss: () => setShellNoticeVisible(false),
      }}
      onActiveToolChange={activateTool}
      onOpenTool={openTool}
      onOpenToolRailCustomization={openToolRailCustomization}
      onCloseToolPanel={closeTool}
      openToolPanels={openToolPanels}
      remoteGroupDialogProps={remoteGroupDialogOpen ? {
        externalConfigConflict: remoteGroupConfigConflict?.message, group: editingRemoteGroup,
        onClose: closeRemoteGroupDialog, onCreateGroup: createRemoteHostGroup,
        onCreated: handleRemoteGroupSaved, onUpdateGroup: handleRemoteGroupUpdate,
        open: remoteGroupDialogOpen,
      } : null}
      remoteHostDialogProps={remoteHostDialogOpen ? {
        defaultGroupId: remoteHostDefaultGroupId ?? defaultRemoteGroupId,
        defaultMode: remoteHostDefaultMode, editingHost: editingRemoteHost,
        editingLocalMachine, externalConfigConflict: connectionConfigConflict?.message,
        groups: machineGroups, onClose: handleConnectionDialogClose,
        onCreateGroup: createRemoteHostGroup, onCreateHost: handleCreateRemoteHost,
        onCreateLocal: handleCreateLocalProfile, onCreated: handleConnectionDialogCreated,
        onGroupCreated: handleRemoteGroupSaved, onUpdateHost: updateRemoteHost,
        onUpdateLocal: handleUpdateLocalProfile, open: remoteHostDialogOpen,
      } : null}
      rightSeparatorProps={{
        className: "kerminal-shell-separator relative z-20",
        hidden: !effectiveRightPanelOpen,
        label: "调整工具面板宽度",
        onKeyDown: (event) => resizeWithKeyboard("rightTools", event),
        onPointerDown: (event) => beginPanelResize("rightTools", event),
        style: {
          gridColumn: "6 / 7",
          gridRow: "2 / 5",
        },
      }}
      settingsDialogProps={settingsDialogOpen ? {
        initialSectionId: settingsInitialSectionId, onClose: handleSettingsDialogClose,
        onConfirmedSettingsChange: handleConfirmedSettingsChange,
        onSettingsChange: handleSettingsDialogChange, open: settingsDialogOpen,
        saveError: settingsSaveError, saveState: settingsSaveState, settings,
      } : null}
      toolRailCustomizationProps={{
        onClose: closeToolRailCustomization,
        onSave: saveToolRailSettings,
        open: toolRailCustomizationOpen,
        settings: settings.toolRail,
      }}
      shellWindowChromeProps={{
        desktopPlatform, leftPanelCollapsed,
        onLeftPanelCollapsedChange: setLeftPanelCollapsed, resolvedTheme,
        rightToolRailTitleBarFillWidth, windowFrameState,
      }}
      tabsConfirmationProps={{
        onClose: cancelTerminalTabs,
        onConfirm: confirmTerminalTabs, tabCount: pendingTerminalTabCount,
      }}
      toolPanelProps={{
        activeTool, defaultRemoteGroupId, defaultRemoteHostId, machineGroups,
        onActiveToolChange: activateTool, onCreateTerminal: addTerminalTab,
        onFocusTab: selectTab, onOpenSettingsSection: openSettingsTool,
        onOpenToolRailCustomization: openToolRailCustomization,
        onOpenTool: openTool,
        onOpenSshTerminal: openSshTerminal, onRemoteHostCreated: refreshRemoteHostTree,
        onConfirmedSettingsChange: handleConfirmedSettingsChange,
        onSettingsChange: handleSettingsChange, onSplitPane: splitFocusedPane,
        resolvedTheme, settings, snippetConfigRevision: configCatalogRevisions.snippets,
        terminalAppearance: settings.terminal,
        workflowConfigRevision: configCatalogRevisions.workflows,
      }}
      workspaceFileConfirmationProps={{
        dirtyTabCount: dirtyFileTabCount,
        onClose: cancelDirtyFileTabs,
        onConfirm: confirmDirtyFileTabs,
        tabCount: pendingDirtyFileTabCount,
      }}
      workspaceTerminalProps={{
        backgroundImageVisible:
          settings.appearance.backgroundEnabled &&
          settings.appearance.backgroundOpacity > 0 &&
          settings.appearance.backgroundImagePath.trim().length > 0,
        contentLeftInset: leftWorkspaceInset,
        contentRightInset: rightWorkspaceInset, createdSftpHostTarget,
        desktopNotifications: settings.desktopNotifications,
        interfaceDensity: settings.interfaceDensity, leftTitleBarInset,
        machineGroups, onBroadcastCommand: writeBroadcastCommand,
        onCreateSftpHost: openSftpTransferHostCreateDialog,
        onCloseConfirmedTabs: closeConfirmedTabs,
        onOpenAgentTool: () => openTool("agentLauncher"),
        onOpenConnection: () => openConnectionDialog({ mode: "ssh" }),
        onOpenLogs: openLogsTool, rightTitleBarInset,
        resolvedTheme, splitDropIndicator: terminalSplitDropIndicator,
        terminalAppearance: settings.terminal,
      }}
    />
  );
}
