// @author kongweiguang

import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
  type CSSProperties,
  type MouseEvent as ReactMouseEvent,
} from "react";
import {
  analyzeBroadcastCommand,
  canBroadcastCommand,
  type BroadcastCommandAnalysis,
} from "./broadcastCommandPolicy";
import { collectPaneIds } from "../workspace/contracts/index";
import {
  isTerminalSessionTab,
  type TerminalTab,
} from "../workspace/contracts/index";
import { resolveWorkspaceTabCloseDecision } from "../workspace/contracts/index";
import { TerminalBroadcastBar } from "./TerminalBroadcastBar";
import { TerminalTabBar } from "./TerminalTabBar";
import { terminalChromeRuntimeStore } from "./terminalChromeRuntimeStore";
import { terminalInputSync } from "./terminalInputSync";
import {
  resolveTerminalTabPresentation,
  type TerminalTabPresentation,
} from "./terminalTabPresentationModel";
import { TerminalWorkspaceContent } from "./TerminalWorkspaceContent";
import { useTerminalBroadcastTargets } from "./useTerminalBroadcastTargets";
import { useTerminalTabOverview } from "./TerminalWorkspace.tabOverview";
import {
  resolveTerminalTabStatus,
  sameTerminalTabGroupSnapshot,
} from "./terminalTabWorkspaceModel";
import {
  buildTerminalTabGroups,
  clampContextMenuPosition,
  type TerminalTabGroup,
  type TerminalTabContextMenu,
  type TerminalTabContextMenuPayload,
} from "./terminalTabChrome";
import { TerminalWorkspaceTabOverlays } from "./TerminalWorkspaceTabOverlays";
import type { TerminalWorkspaceProps } from "./TerminalWorkspace.types";
export type {
  BroadcastCommandRequest,
  BroadcastCommandResult,
} from "./TerminalWorkspace.types";

const EMPTY_PANE_CHROME_SNAPSHOTS: ReturnType<
  typeof terminalChromeRuntimeStore.getSnapshots
> = Object.freeze([]);
/**
 * 组合标签、广播栏和活动终端内容；左右 inset 只施加到标签栏下方，既让导航横跨
 * 停靠面板上方，也避免面板覆盖实际终端与广播交互区。
 */
export function TerminalWorkspace({
  activeTabId,
  backgroundImageVisible = false,
  broadcastDraft,
  contentLeftInset = 0,
  contentRightInset = 0,
  focusedPaneId,
  interfaceDensity = "comfortable",
  leftTitleBarInset = 0,
  machineGroups = [],
  onBroadcastCommand,
  onBroadcastDraftChange,
  onClosePane,
  onCloseTabs,
  onCreateTerminal,
  onFocusPane,
  onOpenAgentTool,
  onOpenConnection,
  onOpenSavedTerminal,
  onMovePane,
  onPaneConnectionStateChange,
  onPaneCurrentCwdChange,
  onPaneOutputHistoryChange,
  onSplitLayoutSizesChange,
  onOpenLogs,
  onRevealWorkspaceFileInSftp,
  onRenameTab,
  onUpdateTabGroupPreference,
  onCreateTerminalTabGroup,
  onUpdateTerminalTabGroup,
  onSetTerminalTabGroupCollapsed,
  onMoveTerminalTab,
  onMoveTerminalTabGroup,
  onRemoveTerminalTabFromGroup,
  onUngroupTerminalTabGroup,
  rightTitleBarInset = 112,
  resolvePaneLines,
  resolvePaneOutputHistory,
  renderCustomTab,
  onSelectTab,
  onSplitPane,
  panes,
  profiles = [],
  resolvedTheme,
  splitDropIndicator,
  tabs,
  terminalTabGroups,
  tabGroupPreferences = {},
  terminalAppearance,
  workspaceFileDirtyState = {},
}: TerminalWorkspaceProps) {
  const activeTab = tabs.find((tab) => tab.id === activeTabId) ?? tabs[0];
  const explicitTabGroups = terminalTabGroups !== undefined;
  const tabGroups = useMemo(
    () =>
      buildTerminalTabGroups(
        tabs,
        explicitTabGroups ? terminalTabGroups : tabGroupPreferences,
        {
          machineGroups,
          mode: explicitTabGroups ? "explicit" : "legacy",
          panes,
        },
      ),
    [
      explicitTabGroups,
      machineGroups,
      panes,
      tabGroupPreferences,
      tabs,
      terminalTabGroups,
    ],
  );
  const [collapsedTabGroupIds, setCollapsedTabGroupIds] = useState<Set<string>>(
    () =>
      new Set(
        terminalTabGroups
          ? Object.entries(terminalTabGroups)
              .filter(([, definition]) => definition.collapsed)
              .map(([groupId]) => groupId)
          : [],
      ),
  );
  const [contextMenu, setContextMenu] = useState<TerminalTabContextMenu | null>(
    null,
  );
  const [editingTabGroup, setEditingTabGroup] =
    useState<TerminalTabGroup | null>(null);
  const [creatingGroupForTabId, setCreatingGroupForTabId] = useState<
    string | null
  >(null);
  const [renamingTab, setRenamingTab] = useState<TerminalTab | null>(null);
  const contextMenuRef = useRef<HTMLDivElement>(null);
  const contextMenuTriggerRef = useRef<HTMLElement | null>(null);
  const panesById = useMemo(
    () => new Map(panes.map((pane) => [pane.id, pane])),
    [panes],
  );
  const tabStatusById = useMemo(
    () =>
      new Map(
        tabs.map((tab) => [tab.id, resolveTerminalTabStatus(tab, panesById)]),
      ),
    [panesById, tabs],
  );
  const paneChromeSnapshots = useSyncExternalStore(
    terminalChromeRuntimeStore.subscribeAll,
    terminalChromeRuntimeStore.getSnapshots,
    () => EMPTY_PANE_CHROME_SNAPSHOTS,
  );
  const tabPresentationById = useMemo(() => {
    const snapshotsByPaneId = new Map(
      paneChromeSnapshots.map((snapshot) => [snapshot.paneId, snapshot]),
    );
    return new Map<string, TerminalTabPresentation>(
      tabs.map((tab) => {
        if (!isTerminalSessionTab(tab)) {
          return [tab.id, resolveTerminalTabPresentation([])];
        }
        const paneSnapshots = collectPaneIds(tab.layout)
          .map((paneId) => snapshotsByPaneId.get(paneId))
          .filter((snapshot) => snapshot !== undefined);
        return [tab.id, resolveTerminalTabPresentation(paneSnapshots)];
      }),
    );
  }, [paneChromeSnapshots, tabs]);
  const activePaneIds = useMemo(
    () =>
      activeTab && isTerminalSessionTab(activeTab)
        ? collectPaneIds(activeTab.layout)
        : [],
    [activeTab],
  );
  const hasActiveSplit = activePaneIds.length > 1;
  const {
    broadcastTargets,
    broadcastTargetMode,
    broadcastTargetOptions,
    handleBroadcastTargetModeChange,
    handleToggleCustomTarget,
    selectedTargetPaneIds,
  } = useTerminalBroadcastTargets({
    activePaneIds,
    focusedPaneId,
    panesById,
  });
  const inputSync = useSyncExternalStore(
    terminalInputSync.subscribe,
    terminalInputSync.getSnapshot,
    terminalInputSync.getSnapshot,
  );
  const inputSyncTabId = activeTab && isTerminalSessionTab(activeTab) ? activeTab.id : null;
  useLayoutEffect(() => {
    terminalInputSync.configure(inputSyncTabId, selectedTargetPaneIds);
    terminalInputSync.refresh();
  }, [inputSyncTabId, selectedTargetPaneIds, paneChromeSnapshots]);
  // 只有 owner 卸载才清空范围；布局更新的 cleanup 不能关闭未变化的成员集合。
  useLayoutEffect(() => () => terminalInputSync.configure(null, []), []);
  const broadcastAnalysis = useMemo(
    () => analyzeBroadcastCommand(broadcastDraft, broadcastTargets),
    [broadcastDraft, broadcastTargets],
  );
  const [broadcastStatus, setBroadcastStatus] = useState<string | null>(null);
  const [broadcastError, setBroadcastError] = useState<string | null>(null);
  const [pendingCloseTabIds, setPendingCloseTabIds] = useState<string[] | null>(
    null,
  );
  const [pendingDirtyCloseTabIds, setPendingDirtyCloseTabIds] = useState<
    string[] | null
  >(null);
  const [sendingBroadcast, setSendingBroadcast] = useState(false);
  const contextTab =
    contextMenu?.type === "tab"
      ? tabs.find((tab) => tab.id === contextMenu.tabId)
      : undefined;
  const contextTabGroup =
    contextMenu?.type === "group"
      ? tabGroups.find((group) => group.id === contextMenu.groupId)
      : contextTab
        ? tabGroups.find((group) =>
            group.tabs.some((tab) => tab.id === contextTab.id),
          )
        : undefined;
  const compactDensity = interfaceDensity === "compact";
  const spaciousDensity = interfaceDensity === "spacious";
  const tabBarHeightClass = compactDensity
    ? "h-9"
    : spaciousDensity
      ? "h-10"
      : "h-9";
  const toolbarPaddingClass = compactDensity
    ? "px-2 py-1.5"
    : spaciousDensity
      ? "px-4 py-3"
      : "px-3 py-2";
  const workspacePaddingClass = compactDensity
    ? "p-1.5"
    : spaciousDensity
      ? "p-3"
      : "p-2";
  const terminalInset = compactDensity ? 6 : spaciousDensity ? 12 : 8;
  const contentInsetStyle =
    contentLeftInset > 0 || contentRightInset > 0
      ? ({
          marginLeft: contentLeftInset,
          marginRight: contentRightInset,
        } satisfies CSSProperties)
      : undefined;
  const tabBarStyle =
    leftTitleBarInset > 0
      ? ({ paddingLeft: leftTitleBarInset } satisfies CSSProperties)
      : undefined;
  const {
    handleTabListWheel,
    selectTabFromOverview,
    shouldShowTabOverview,
    tabListRef,
    tabOverviewButtonRef,
    tabOverviewMenuRef,
    tabOverviewOpen,
    tabOverviewPosition,
    toggleTabOverview,
  } = useTerminalTabOverview({
    collapsedTabGroupIds,
    onSelectTab,
    tabCount: tabs.length,
    tabGroups,
  });

  useEffect(() => {
    setCollapsedTabGroupIds((current) => {
      const validIds = new Set(
        tabGroups.filter((group) => group.grouped).map((group) => group.id),
      );
      // 显式组的折叠状态由 store 定义权威维护；不能只做并集，否则恢复到
      // collapsed=false 时旧的本地 optimistic 状态会把组错误地重新折叠。
      const next = terminalTabGroups
        ? new Set(
            Object.entries(terminalTabGroups)
              .filter(
                ([groupId, definition]) =>
                  definition.collapsed && validIds.has(groupId),
              )
              .map(([groupId]) => groupId),
          )
        : new Set([...current].filter((id) => validIds.has(id)));
      if (
        next.size === current.size &&
        [...next].every((groupId) => current.has(groupId))
      ) {
        return current;
      }
      return next;
    });
  }, [tabGroups, terminalTabGroups]);

  useEffect(() => {
    if (hasActiveSplit) {
      return;
    }

    setBroadcastStatus(null);
    setBroadcastError(null);
  }, [hasActiveSplit]);

  /**
   * 菜单或其后续 Dialog 关闭时优先回到原触发按钮；若该 Tab 已随动作关闭，
   * 下一帧改为聚焦新的活动 Tab，避免焦点落到被移除的 portal 节点。
   */
  const restoreContextMenuFocus = useCallback(() => {
    window.requestAnimationFrame(() => {
      const trigger = contextMenuTriggerRef.current;
      if (trigger?.isConnected) {
        trigger.focus();
        return;
      }
      const activeTabButton = Array.from(
        document.querySelectorAll<HTMLElement>("[data-terminal-tab-id]"),
      )
        .find((element) => element.dataset.terminalTabId === activeTabId)
        ?.querySelector<HTMLElement>("button");
      activeTabButton?.focus();
    });
  }, [activeTabId]);

  useEffect(() => {
    if (!contextMenu) {
      return undefined;
    }

    const close = () => setContextMenu(null);
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        close();
        restoreContextMenuFocus();
      }
    };
    window.addEventListener("click", close);
    window.addEventListener("keydown", closeOnEscape);
    window.addEventListener("resize", close);
    return () => {
      window.removeEventListener("click", close);
      window.removeEventListener("keydown", closeOnEscape);
      window.removeEventListener("resize", close);
    };
  }, [contextMenu, restoreContextMenuFocus]);

  useEffect(() => {
    if (!editingTabGroup) {
      return;
    }

    const nextGroup = tabGroups.find(
      (group) => group.id === editingTabGroup.id,
    );
    if (!nextGroup) {
      setEditingTabGroup(null);
      return;
    }

    if (!sameTerminalTabGroupSnapshot(nextGroup, editingTabGroup)) {
      setEditingTabGroup(nextGroup);
    }
  }, [editingTabGroup, tabGroups]);

  useEffect(() => {
    if (!renamingTab) {
      return;
    }

    if (!tabs.some((tab) => tab.id === renamingTab.id)) {
      setRenamingTab(null);
    }
  }, [renamingTab, tabs]);

  useLayoutEffect(() => {
    if (!contextMenu) {
      return;
    }

    const menuElement = contextMenuRef.current;
    if (!menuElement) {
      return;
    }

    menuElement
      .querySelector<HTMLButtonElement>('[role="menuitem"]:not([disabled])')
      ?.focus();

    const rect = menuElement.getBoundingClientRect();
    const nextPosition = clampContextMenuPosition(
      contextMenu.x,
      contextMenu.y,
      rect.width,
      rect.height,
    );
    if (nextPosition.x === contextMenu.x && nextPosition.y === contextMenu.y) {
      return;
    }
    setContextMenu((current) =>
      current === contextMenu ? { ...current, ...nextPosition } : current,
    );
  }, [contextMenu]);

  const executeBroadcast = useCallback(
    async (analysis: BroadcastCommandAnalysis) => {
      if (!canBroadcastCommand(analysis)) {
        setBroadcastError(
          analysis.command
            ? "当前 tab 没有可发送的真实终端分屏。"
            : "请输入要发送的命令。",
        );
        return;
      }

      setSendingBroadcast(true);
      setBroadcastError(null);
      try {
        const result = await onBroadcastCommand({
          command: analysis.command,
          data: analysis.data,
          targetPaneIds: analysis.targets.map((target) => target.paneId),
        });
        const skipped =
          result.missingPaneIds.length > 0
            ? `，${result.missingPaneIds.length} 个分屏尚未连接`
            : "";
        setBroadcastStatus(
          `已发送到 ${result.sentPaneIds.length} 个分屏${skipped}。`,
        );
        if (result.sentPaneIds.length > 0) {
          onBroadcastDraftChange("");
        }
      } catch (error) {
        setBroadcastError(
          error instanceof Error ? error.message : String(error),
        );
      } finally {
        setSendingBroadcast(false);
      }
    },
    [onBroadcastCommand, onBroadcastDraftChange],
  );

  const requestBroadcast = useCallback(() => {
    setBroadcastStatus(null);
    setBroadcastError(null);
    if (!canBroadcastCommand(broadcastAnalysis)) {
      void executeBroadcast(broadcastAnalysis);
      return;
    }
    void executeBroadcast(broadcastAnalysis);
  }, [broadcastAnalysis, executeBroadcast]);

  const handleDraftChange = useCallback(
    (draft: string) => {
      setBroadcastStatus(null);
      setBroadcastError(null);
      onBroadcastDraftChange(draft);
    },
    [onBroadcastDraftChange],
  );
  const toggleTabGroup = useCallback(
    (groupId: string) => {
      setCollapsedTabGroupIds((current) => {
        const next = new Set(current);
        const collapsed = !next.has(groupId);
        if (!collapsed) {
          next.delete(groupId);
        } else {
          next.add(groupId);
        }
        onSetTerminalTabGroupCollapsed?.(groupId, collapsed);
        return next;
      });
    },
    [onSetTerminalTabGroupCollapsed],
  );
  const openContextMenu = useCallback(
    (event: ReactMouseEvent, menu: TerminalTabContextMenuPayload) => {
      event.preventDefault();
      event.stopPropagation();
      const eventTarget = event.target as HTMLElement;
      const currentTarget = event.currentTarget as HTMLElement;
      // Tab 的 contextmenu 绑定在视觉外壳，实际可聚焦 activator 是其内层按钮；
      // 组头则直接以 button 触发，因此优先从原始事件目标解析真实按钮。
      contextMenuTriggerRef.current =
        eventTarget.closest<HTMLElement>("button") ??
        (currentTarget.matches("button")
          ? currentTarget
          : currentTarget.querySelector<HTMLElement>("button"));
      const position = clampContextMenuPosition(
        event.clientX,
        event.clientY,
        0,
        0,
      );
      setContextMenu({ ...menu, ...position });
    },
    [],
  );
  const moveTabWithinGroup = useCallback(
    (tabId: string, direction: "before" | "after") => {
      const group = tabGroups.find((candidate) =>
        candidate.tabs.some((tab) => tab.id === tabId),
      );
      if (!group || !onMoveTerminalTab) return;
      const index = group.tabs.findIndex((tab) => tab.id === tabId);
      const target = group.tabs[index + (direction === "before" ? -1 : 1)];
      if (!target) return;
      onMoveTerminalTab({
        position: direction,
        tabId,
        targetGroupId: group.id,
        targetTabId: target.id,
      });
    },
    [onMoveTerminalTab, tabGroups],
  );
  const moveTabToGroup = useCallback(
    (tabId: string, groupId: string) => {
      const targetGroup = tabGroups.find((group) => group.id === groupId);
      if (!targetGroup || !onMoveTerminalTab) return;
      const lastTab = targetGroup.tabs[targetGroup.tabs.length - 1];
      onMoveTerminalTab({
        position: "after",
        tabId,
        targetGroupId: groupId,
        ...(lastTab ? { targetTabId: lastTab.id } : {}),
      });
    },
    [onMoveTerminalTab, tabGroups],
  );
  const moveGroup = useCallback(
    (groupId: string, direction: "before" | "after") => {
      if (!onMoveTerminalTabGroup) return;
      const index = tabGroups.findIndex((group) => group.id === groupId);
      const target = tabGroups[index + (direction === "before" ? -1 : 1)];
      if (!target) return;
      const targetIndex = target.grouped
        ? undefined
        : tabs.findIndex((tab) => tab.id === target.tabs[0]?.id) +
          (direction === "after" ? 1 : 0);
      onMoveTerminalTabGroup({
        groupId,
        position: direction,
        ...(target.grouped
          ? { targetGroupId: target.id }
          : { targetIndex: Math.max(0, targetIndex ?? 0) }),
      });
    },
    [onMoveTerminalTabGroup, tabGroups, tabs],
  );
  const runMenuAction = useCallback(
    (action?: () => void) => {
      setContextMenu(null);
      action?.();
      restoreContextMenuFocus();
    },
    [restoreContextMenuFocus],
  );
  const requestCloseTabs = useCallback(
    (tabIds: string[], confirmedDirtyFiles = false) => {
      const decision = resolveWorkspaceTabCloseDecision({
        confirmTerminalClose: terminalAppearance.confirmCloseTab,
        confirmedDirtyFiles,
        tabIds,
        tabs,
        workspaceFileDirtyState,
      });
      if (decision.kind === "confirmDirtyFiles") {
        setPendingDirtyCloseTabIds(decision.tabIds);
        return;
      }
      if (decision.kind === "confirmTerminalTabs") {
        setPendingCloseTabIds(decision.tabIds);
        return;
      }
      onCloseTabs(decision.tabIds);
    },
    [
      onCloseTabs,
      tabs,
      terminalAppearance.confirmCloseTab,
      workspaceFileDirtyState,
    ],
  );
  const confirmCloseTabs = useCallback(() => {
    if (!pendingCloseTabIds) {
      return;
    }
    onCloseTabs(pendingCloseTabIds);
    setPendingCloseTabIds(null);
  }, [onCloseTabs, pendingCloseTabIds]);
  const confirmDirtyFileCloseTabs = useCallback(() => {
    if (!pendingDirtyCloseTabIds) {
      return;
    }
    requestCloseTabs(pendingDirtyCloseTabIds, true);
    setPendingDirtyCloseTabIds(null);
  }, [pendingDirtyCloseTabIds, requestCloseTabs]);
  return (
    <main
      aria-label="终端工作区"
      className="kerminal-workspace-surface flex h-full w-full min-w-0 flex-col overflow-hidden"
      data-density={interfaceDensity}
    >
      <TerminalTabBar
        activeTabId={activeTabId}
        collapsedGroupIds={collapsedTabGroupIds}
        heightClassName={tabBarHeightClass}
        machineGroups={machineGroups}
        onCreateTerminal={onCreateTerminal}
        onOpenConnection={onOpenConnection}
        onOpenContextMenu={openContextMenu}
        onOpenSavedTerminal={onOpenSavedTerminal}
        onMoveTerminalTab={onMoveTerminalTab}
        onMoveTerminalTabGroup={onMoveTerminalTabGroup}
        onRequestCloseTab={(tabId) => requestCloseTabs([tabId])}
        onSelectTab={onSelectTab}
        onToggleGroup={toggleTabGroup}
        onToggleOverview={toggleTabOverview}
        onWheel={handleTabListWheel}
        overviewButtonRef={tabOverviewButtonRef}
        overviewOpen={tabOverviewOpen}
        profiles={profiles}
        rightTitleBarInset={rightTitleBarInset}
        shouldShowOverview={shouldShowTabOverview}
        style={tabBarStyle}
        tabGroups={tabGroups}
        terminalTabGroups={terminalTabGroups}
        tabListRef={tabListRef}
        tabPresentationById={tabPresentationById}
        tabs={tabs}
        tabStatusById={tabStatusById}
        terminalAppearance={terminalAppearance}
        workspaceFileDirtyState={workspaceFileDirtyState}
      />
      <TerminalWorkspaceTabOverlays
        activeTabId={activeTabId}
        collapsedGroupIds={collapsedTabGroupIds}
        contextMenu={contextMenu}
        contextMenuRef={contextMenuRef}
        contextTab={contextTab}
        contextTabGroup={contextTabGroup}
        creatingGroupForTabId={creatingGroupForTabId}
        editingTabGroup={editingTabGroup}
        explicitTabGroups={explicitTabGroups}
        onCloseContextDialog={() => {
          setRenamingTab(null);
          setEditingTabGroup(null);
          setCreatingGroupForTabId(null);
          restoreContextMenuFocus();
        }}
        onCloseTabs={requestCloseTabs}
        onConfirmCloseTabs={confirmCloseTabs}
        onConfirmDirtyCloseTabs={confirmDirtyFileCloseTabs}
        onCreateGroup={(tabId, definition) => {
          onCreateTerminalTabGroup?.(tabId, definition);
          setCreatingGroupForTabId(null);
        }}
        onDismissCloseTabs={() => setPendingCloseTabIds(null)}
        onDismissDirtyCloseTabs={() => setPendingDirtyCloseTabIds(null)}
        onMoveGroup={onMoveTerminalTabGroup ? moveGroup : undefined}
        onMoveTabToGroup={onMoveTerminalTab ? moveTabToGroup : undefined}
        onMoveTabWithinGroup={
          onMoveTerminalTab ? moveTabWithinGroup : undefined
        }
        onRemoveTabFromGroup={onRemoveTerminalTabFromGroup}
        onRequestCreateGroup={
          onCreateTerminalTabGroup ? setCreatingGroupForTabId : undefined
        }
        onRequestEditGroup={
          onUpdateTerminalTabGroup || onUpdateTabGroupPreference
            ? setEditingTabGroup
            : undefined
        }
        onRequestRenameTab={setRenamingTab}
        onRenameTab={onRenameTab}
        onRevealWorkspaceFileInSftp={onRevealWorkspaceFileInSftp}
        onSaveGroup={(groupId, preference) =>
          onUpdateTerminalTabGroup
            ? onUpdateTerminalTabGroup(groupId, {
                color: preference.color ?? undefined,
                title: preference.title ?? undefined,
              })
            : onUpdateTabGroupPreference?.(groupId, preference)
        }
        onSelectTab={onSelectTab}
        onSelectTabFromOverview={selectTabFromOverview}
        onToggleGroup={toggleTabGroup}
        onUngroup={onUngroupTerminalTabGroup}
        overviewMenuRef={tabOverviewMenuRef}
        overviewOpen={tabOverviewOpen}
        overviewPosition={tabOverviewPosition}
        pendingCloseTabIds={pendingCloseTabIds}
        pendingDirtyCloseTabIds={pendingDirtyCloseTabIds}
        renamingTab={renamingTab}
        runMenuAction={runMenuAction}
        tabGroups={tabGroups}
        tabPresentationById={tabPresentationById}
        tabs={tabs}
        tabStatusById={tabStatusById}
        terminalAppearance={terminalAppearance}
        workspaceFileDirtyState={workspaceFileDirtyState}
      />
      {hasActiveSplit ? (
        <TerminalBroadcastBar
          analysis={broadcastAnalysis}
          draft={broadcastDraft}
          error={broadcastError}
          focusedPaneId={focusedPaneId}
          inputSync={inputSync}
          onToggleInputSync={() => terminalInputSync.setEnabled(!inputSync.enabled)}
          onDraftChange={handleDraftChange}
          onRequestBroadcast={requestBroadcast}
          onTargetModeChange={handleBroadcastTargetModeChange}
          onToggleCustomTarget={handleToggleCustomTarget}
          selectedTargetPaneIds={selectedTargetPaneIds}
          sending={sendingBroadcast}
          status={broadcastStatus}
          style={contentInsetStyle}
          targetMode={broadcastTargetMode}
          targetOptions={broadcastTargetOptions}
          toolbarPaddingClass={toolbarPaddingClass}
        />
      ) : null}

      <TerminalWorkspaceContent
        activeTab={activeTab}
        backgroundImageVisible={backgroundImageVisible}
        contentInsetStyle={contentInsetStyle}
        focusedPaneId={focusedPaneId}
        machineGroups={machineGroups}
        onClosePane={onClosePane}
        onCreateTerminal={onCreateTerminal}
        onFocusPane={onFocusPane}
        onOpenAgentTool={onOpenAgentTool}
        onOpenConnection={onOpenConnection}
        onOpenLogs={onOpenLogs}
        onMovePane={onMovePane}
        onPaneConnectionStateChange={onPaneConnectionStateChange}
        onPaneCurrentCwdChange={onPaneCurrentCwdChange}
        onPaneOutputHistoryChange={onPaneOutputHistoryChange}
        onSplitLayoutSizesChange={onSplitLayoutSizesChange}
        onSplitPane={onSplitPane}
        panesById={panesById}
        resolvePaneLines={resolvePaneLines}
        resolvePaneOutputHistory={resolvePaneOutputHistory}
        renderCustomTab={renderCustomTab}
        resolvedTheme={resolvedTheme}
        splitDropIndicator={splitDropIndicator}
        tabs={tabs}
        terminalAppearance={terminalAppearance}
        terminalInset={terminalInset}
        workspacePaddingClass={workspacePaddingClass}
      />
    </main>
  );
}
