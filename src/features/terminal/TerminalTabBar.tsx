// @author kongweiguang

import { ChevronDown } from "lucide-react";
import {
  PointerSensor,
  useSensor,
  useSensors,
  type DragEndEvent,
  type DragOverEvent,
  type DragStartEvent,
} from "@dnd-kit/core";
import { horizontalListSortingStrategy } from "@dnd-kit/sortable";
import { useCallback, useMemo, useRef, useState } from "react";
import type {
  CSSProperties,
  MouseEvent as ReactMouseEvent,
  RefObject,
  WheelEvent as ReactWheelEvent,
} from "react";
import { cn } from "../../lib/cn";
import type { TerminalProfile } from "../../lib/profileApi";
import type { TerminalAppearance } from "../settings/contracts/index";
import type {
  MachineGroup,
  MachineStatus,
  TerminalTab,
  TerminalTabGroups,
  WorkspaceFileDirtyState,
} from "../workspace/contracts/index";
import {
  TerminalCreateButton,
  TerminalCreatePanel,
  type TerminalCreatePanelPosition,
} from "./TerminalCreateMenu";
import {
  buildTerminalCreateHostOptions,
  buildTerminalCreateProfileOptions,
} from "./terminalCreateMenuModel";
import {
  TerminalTabButton,
  TerminalTabGroupHeader,
  type TerminalTabContextMenuPayload,
  type TerminalTabGroup,
} from "./terminalTabChrome";
import {
  resolveTerminalTabGroupPresentation,
  resolveTerminalTabPresentation,
  type TerminalTabPresentation,
} from "./terminalTabPresentationModel";
import type {
  TerminalTabGroupMoveRequest,
  TerminalTabMoveRequest,
} from "../workspace/contracts/index";
import {
  resolveTerminalTabDragCommand,
  type TerminalTabDragSource,
  type TerminalTabDragTarget,
} from "./terminalTabDragModel";
import {
  DraggableGroupItem,
  DraggableTabItem,
  DropGap,
  MaybeDndContext,
  MaybeDragOverlay,
  MaybeSortableContext,
  resolveDropPosition,
  terminalTabCollisionDetection,
} from "./TerminalTabBar.dnd";

interface TerminalTabBarProps {
  activeTabId: string;
  collapsedGroupIds: ReadonlySet<string>;
  heightClassName: string;
  machineGroups: readonly MachineGroup[];
  onOpenContextMenu: (
    event: ReactMouseEvent,
    payload: TerminalTabContextMenuPayload,
  ) => void;
  onCreateTerminal?: (profileId?: string) => void;
  onOpenConnection?: () => void;
  onOpenSavedTerminal?: (machineId: string) => void;
  onMoveTerminalTab?: (request: TerminalTabMoveRequest) => void;
  onMoveTerminalTabGroup?: (request: TerminalTabGroupMoveRequest) => void;
  onRequestCloseTab: (tabId: string) => void;
  onSelectTab: (tabId: string) => void;
  onToggleGroup: (groupId: string) => void;
  onToggleOverview: (event: ReactMouseEvent) => void;
  onWheel: (event: ReactWheelEvent<HTMLDivElement>) => void;
  overviewButtonRef: RefObject<HTMLButtonElement | null>;
  overviewOpen: boolean;
  profiles: readonly TerminalProfile[];
  rightTitleBarInset: number;
  shouldShowOverview: boolean;
  style?: CSSProperties;
  tabGroups: TerminalTabGroup[];
  terminalTabGroups?: TerminalTabGroups;
  tabListRef: RefObject<HTMLDivElement | null>;
  tabPresentationById: ReadonlyMap<string, TerminalTabPresentation>;
  tabs: TerminalTab[];
  tabStatusById: ReadonlyMap<string, MachineStatus>;
  terminalAppearance: TerminalAppearance;
  workspaceFileDirtyState: WorkspaceFileDirtyState;
}

/** 终端标签栏只负责展示与命令转发；顶层间距由 DropGap 提供，避免重复留白。 */
export function TerminalTabBar({
  activeTabId,
  collapsedGroupIds,
  heightClassName,
  machineGroups,
  onCreateTerminal,
  onOpenConnection,
  onOpenContextMenu,
  onOpenSavedTerminal,
  onMoveTerminalTab,
  onMoveTerminalTabGroup,
  onRequestCloseTab,
  onSelectTab,
  onToggleGroup,
  onToggleOverview,
  onWheel,
  overviewButtonRef,
  overviewOpen,
  profiles,
  rightTitleBarInset,
  shouldShowOverview,
  style,
  tabGroups,
  tabListRef,
  tabPresentationById,
  tabs,
  tabStatusById,
  terminalAppearance,
  workspaceFileDirtyState,
}: TerminalTabBarProps) {
  const createButtonRef = useRef<HTMLButtonElement>(null);
  const [createPanelPosition, setCreatePanelPosition] =
    useState<TerminalCreatePanelPosition | null>(null);
  const [draggingLabel, setDraggingLabel] = useState<string | null>(null);
  const [dragAnnouncement, setDragAnnouncement] = useState("");
  // 单个未分组 Tab 没有可执行的排序动作，避免为旧兼容渲染路径注入
  // dnd-kit 的全局 role=status live region；出现第二个 Tab 或显式组后再启用完整拖拽上下文。
  const dragEnabled =
    tabs.length > 1 || tabGroups.some((group) => group.grouped);
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 6 } }),
  );
  const hostCreateOptions = useMemo(
    () => buildTerminalCreateHostOptions(machineGroups),
    [machineGroups],
  );
  const profileCreateOptions = useMemo(
    () => buildTerminalCreateProfileOptions(profiles),
    [profiles],
  );
  const canOpenCreatePanel = Boolean(
    profileCreateOptions.length > 0 ||
      hostCreateOptions.length > 0 ||
      onOpenConnection,
  );
  /** 关闭由 TabBar 持有的面板，使按钮迁移时不会绑定到失效组件状态。 */
  const closeCreatePanel = useCallback(() => {
    setCreatePanelPosition(null);
  }, []);
  /** 记录拖拽开始；PointerSensor 自己负责拦截 pointerup 后的即时 click。 */
  const handleDragStart = useCallback((event: DragStartEvent) => {
    const data = event.active.data.current as { label?: string } | undefined;
    setDraggingLabel(data?.label ?? "标签");
  }, []);
  /** 取消拖拽只清理浮层，不能额外吞掉下一次普通点击。 */
  const handleDragCancel = useCallback(() => {
    setDraggingLabel(null);
  }, []);
  /**
   * 在 drop 前同步宣告候选目标，让键盘/辅助技术用户知道当前会移入哪组或哪一位；
   * 这里只更新 aria-live，不提前改 store，取消或过期 drop 仍保持纯 no-op。
   */
  const handleDragOver = useCallback((event: DragOverEvent) => {
    const activeData = event.active.data.current as
      | { kind?: "tab" | "group"; tabId?: string; groupId?: string }
      | undefined;
    const overData = event.over?.data.current as
      | {
          kind?: "tab" | "group" | "gap";
          label?: string;
          tabId?: string;
          groupId?: string;
          index?: number;
        }
      | undefined;
    if (!activeData?.kind || !overData?.kind) return;
    const position = resolveDropPosition(event);
    if (activeData.kind === "tab" && activeData.tabId) {
      if (overData.kind === "gap") {
        setDragAnnouncement(
          `移出分组，移动到第 ${(overData.index ?? 0) + 1} 位`,
        );
      } else if (overData.kind === "group" && overData.groupId) {
        setDragAnnouncement(`移入标签组 ${overData.label ?? overData.groupId}`);
      } else if (overData.kind === "tab" && overData.tabId) {
        setDragAnnouncement(
          `移动到标签 ${overData.label ?? overData.tabId} ${position === "before" ? "前" : "后"}`,
        );
      }
    } else if (activeData.kind === "group" && activeData.groupId) {
      if (overData.kind === "gap") {
        setDragAnnouncement(`移动标签组到第 ${(overData.index ?? 0) + 1} 位`);
      } else if (overData.kind === "group" && overData.groupId) {
        setDragAnnouncement(
          `移动标签组到 ${overData.label ?? overData.groupId} ${position === "before" ? "前" : "后"}`,
        );
      } else if (overData.kind === "tab" && overData.tabId) {
        setDragAnnouncement(
          `移动标签组到 ${overData.label ?? overData.tabId} ${position === "before" ? "前" : "后"}`,
        );
      }
    }
  }, []);
  /** 组拖到成员 Tab 时仍按顶层块解析，不能把整组误并入目标组。 */
  const handleDragEnd = useCallback(
    (event: DragEndEvent) => {
      setDraggingLabel(null);
      const activeData = event.active.data.current as
        | (TerminalTabDragSource & { label?: string })
        | undefined;
      const overData = event.over?.data.current as
        | (TerminalTabDragTarget & { label?: string })
        | undefined;
      if (!activeData?.kind || !overData?.kind) return;
      const position = resolveDropPosition(event);
      const command = resolveTerminalTabDragCommand({
        active: activeData,
        over: overData,
        position,
        tabGroups: tabGroups.map((group) => ({
          grouped: group.grouped,
          id: group.id,
          tabs: group.tabs,
        })),
        tabs,
      });
      if (!command) return;
      if ("tabId" in command) {
        onMoveTerminalTab?.(command);
      } else {
        onMoveTerminalTabGroup?.(command);
      }
      if (overData.kind === "gap") {
        setDragAnnouncement(
          `${activeData.kind === "group" ? "移动标签组" : "移出分组，移动标签"}到第 ${(overData.index ?? 0) + 1} 位`,
        );
      } else if (overData.kind === "group") {
        setDragAnnouncement(`移入标签组 ${overData.label ?? overData.groupId}`);
      } else {
        setDragAnnouncement(
          `移动到标签 ${overData.label ?? overData.tabId} ${position === "before" ? "前" : "后"}`,
        );
      }
    },
    [onMoveTerminalTab, onMoveTerminalTabGroup, tabGroups, tabs],
  );
  /** 键盘排序按当前层级解析：组内 Tab 只换组内位置，未分组 Tab 在顶层移动。 */
  const handleTabListKeyDown = useCallback(
    (event: React.KeyboardEvent<HTMLDivElement>) => {
      if (!event.ctrlKey || !event.shiftKey) return;
      if (event.key !== "PageUp" && event.key !== "PageDown") return;
      const target = event.target as HTMLElement;
      const tabId = target.closest<HTMLElement>("[data-terminal-tab-id]")
        ?.dataset.terminalTabId;
      const groupId = target.closest<HTMLElement>(
        "[data-terminal-tab-group-id]",
      )?.dataset.terminalTabGroupId;
      const direction = event.key === "PageUp" ? "before" : "after";
      if (tabId) {
        const group = tabGroups.find((candidate) =>
          candidate.tabs.some((tab) => tab.id === tabId),
        );
        if (group?.grouped) {
          const index = group.tabs.findIndex((tab) => tab.id === tabId);
          const sibling = group.tabs[index + (direction === "before" ? -1 : 1)];
          if (sibling) {
            event.preventDefault();
            onMoveTerminalTab?.({
              position: direction,
              tabId,
              targetGroupId: group.id,
              targetTabId: sibling.id,
            });
          }
        } else if (group) {
          const groupIndex = tabGroups.findIndex(
            (candidate) => candidate.id === group.id,
          );
          const siblingGroup =
            tabGroups[groupIndex + (direction === "before" ? -1 : 1)];
          const siblingTab = siblingGroup?.tabs[0];
          if (siblingTab) {
            event.preventDefault();
            if (siblingGroup.grouped) {
              const siblingStartIndex = tabs.findIndex(
                (tab) => tab.id === siblingTab.id,
              );
              onMoveTerminalTab?.({
                tabId,
                targetGroupId: null,
                targetIndex:
                  siblingStartIndex +
                  (direction === "after" ? siblingGroup.tabs.length : 0),
              });
            } else {
              onMoveTerminalTab?.({
                position: direction,
                tabId,
                targetGroupId: null,
                targetTabId: siblingTab.id,
              });
            }
          }
        }
      } else if (groupId) {
        const index = tabGroups.findIndex((group) => group.id === groupId);
        const sibling = tabGroups[index + (direction === "before" ? -1 : 1)];
        if (sibling) {
          event.preventDefault();
          onMoveTerminalTabGroup?.({
            groupId,
            position: direction,
            ...(sibling.grouped
              ? { targetGroupId: sibling.id }
              : {
                  targetIndex:
                    tabs.findIndex((tab) => tab.id === sibling.tabs[0]?.id) +
                    (direction === "after" ? 1 : 0),
                }),
          });
        }
      }
    },
    [onMoveTerminalTab, onMoveTerminalTabGroup, tabGroups, tabs],
  );

  return (
    <>
      <MaybeDndContext
        collisionDetection={terminalTabCollisionDetection}
        enabled={dragEnabled}
        onDragCancel={handleDragCancel}
        onDragEnd={handleDragEnd}
        onDragOver={handleDragOver}
        onDragStart={handleDragStart}
        sensors={sensors}
      >
        <MaybeSortableContext
          enabled={dragEnabled}
          items={tabGroups.flatMap((group) =>
            group.grouped
              ? [`group:${group.id}`]
              : group.tabs.map((tab) => `tab:${tab.id}`),
          )}
          strategy={horizontalListSortingStrategy}
        >
          <div
            className={cn(
              "kerminal-material-nav relative z-20 flex items-center border-b border-[var(--border-subtle)] shadow-[inset_0_-1px_0_var(--border-subtle)]",
              heightClassName,
            )}
            style={{ ...style, paddingRight: rightTitleBarInset }}
          >
            <div className="flex min-w-0 flex-1 items-center self-stretch">
              <div
                aria-label="终端标签栏"
                className="scrollbar-none flex min-w-0 max-w-full flex-[0_1_auto] items-center overflow-x-auto overflow-y-hidden overscroll-x-contain"
                onWheel={onWheel}
                onKeyDown={handleTabListKeyDown}
                ref={tabListRef}
              >
                <DropGap disabled={!dragEnabled} index={0} />
                {tabGroups.map((group) => {
                  const collapsed = collapsedGroupIds.has(group.id);
                  const groupActive = group.tabs.some(
                    (tab) => tab.id === activeTabId,
                  );
                  const groupPresentation = resolveTerminalTabGroupPresentation(
                    group.tabs.map(
                      (tab) =>
                        tabPresentationById.get(tab.id) ??
                        resolveTerminalTabPresentation([]),
                    ),
                    !collapsed,
                  );
                  const groupStartIndex = Math.max(
                    0,
                    tabs.findIndex((tab) => tab.id === group.tabs[0]?.id),
                  );
                  if (!group.grouped) {
                    return group.tabs.flatMap((tab) => [
                      <DraggableTabItem
                        disabled={!dragEnabled}
                        groupId={undefined}
                        key={tab.id}
                        label={tab.title}
                        tabId={tab.id}
                      >
                        {(dragProps) => (
                          <TerminalTabButton
                            {...dragProps}
                            active={tab.id === activeTabId}
                            identityAccent={group.identityAccent}
                            onCloseTab={onRequestCloseTab}
                            onContextMenu={(event) =>
                              onOpenContextMenu(event, {
                                tabId: tab.id,
                                type: "tab",
                              })
                            }
                            onSelectTab={onSelectTab}
                            presentation={tabPresentationById.get(tab.id)}
                            showClose
                            status={tabStatusById.get(tab.id)}
                            tab={tab}
                            tabNumber={
                              terminalAppearance.showTabNumbers
                                ? tabs.findIndex(
                                    (candidate) => candidate.id === tab.id,
                                  ) + 1
                                : undefined
                            }
                            workspaceFileDirty={workspaceFileDirtyState[tab.id]}
                          />
                        )}
                      </DraggableTabItem>,
                      <DropGap
                        disabled={!dragEnabled}
                        index={
                          tabs.findIndex(
                            (candidate) => candidate.id === tab.id,
                          ) + 1
                        }
                        key={`gap:tab:${tab.id}`}
                      />,
                    ]);
                  }

                  return [
                    <DraggableGroupItem
                      disabled={!dragEnabled}
                      groupId={group.id}
                      key={group.id}
                      label={group.title}
                    >
                      {(dragProps) => (
                        <div className="relative flex h-9 shrink-0 items-center gap-1 rounded-lg border border-[var(--border-subtle)] bg-[var(--surface-solid)] px-1 shadow-sm shadow-black/5 dark:shadow-black/20">
                          <TerminalTabGroupHeader
                            {...dragProps}
                            active={collapsed && groupActive}
                            collapsed={collapsed}
                            group={group}
                            onContextMenu={(event) =>
                              onOpenContextMenu(event, {
                                groupId: group.id,
                                type: "group",
                              })
                            }
                            onToggle={() => onToggleGroup(group.id)}
                            presentation={groupPresentation}
                          />
                          {!collapsed ? (
                            <MaybeSortableContext
                              enabled={dragEnabled}
                              items={group.tabs.map((tab) => `tab:${tab.id}`)}
                              strategy={horizontalListSortingStrategy}
                            >
                              {group.tabs.map((tab) => (
                                <DraggableTabItem
                                  disabled={!dragEnabled}
                                  groupId={group.id}
                                  key={tab.id}
                                  label={tab.title}
                                  tabId={tab.id}
                                >
                                  {(tabDragProps) => (
                                    <TerminalTabButton
                                      {...tabDragProps}
                                      active={tab.id === activeTabId}
                                      compact
                                      onCloseTab={onRequestCloseTab}
                                      onContextMenu={(event) =>
                                        onOpenContextMenu(event, {
                                          tabId: tab.id,
                                          type: "tab",
                                        })
                                      }
                                      onSelectTab={onSelectTab}
                                      presentation={tabPresentationById.get(
                                        tab.id,
                                      )}
                                      showClose
                                      status={tabStatusById.get(tab.id)}
                                      tab={tab}
                                      tabNumber={
                                        terminalAppearance.showTabNumbers
                                          ? tabs.findIndex(
                                              (candidate) =>
                                                candidate.id === tab.id,
                                            ) + 1
                                          : undefined
                                      }
                                      workspaceFileDirty={
                                        workspaceFileDirtyState[tab.id]
                                      }
                                    />
                                  )}
                                </DraggableTabItem>
                              ))}
                            </MaybeSortableContext>
                          ) : null}
                        </div>
                      )}
                    </DraggableGroupItem>,
                    <DropGap
                      disabled={!dragEnabled}
                      index={groupStartIndex + group.tabs.length}
                      key={`gap:${group.id}`}
                    />,
                  ];
                })}
                {!shouldShowOverview ? (
                  <TerminalCreateButton
                    buttonRef={createButtonRef}
                    canOpenPanel={canOpenCreatePanel}
                    onCreateDefault={onCreateTerminal}
                    onRequestOpen={setCreatePanelPosition}
                    panelOpen={Boolean(createPanelPosition)}
                    placement="inline"
                  />
                ) : null}
              </div>
              <div
                aria-hidden="true"
                className="h-full min-w-3 flex-1"
                data-tauri-drag-region
              />
            </div>
            {shouldShowOverview ? (
              <div
                className="relative z-20 flex shrink-0 items-center gap-1 pl-1"
                data-terminal-tab-actions
              >
                <button
                  aria-expanded={overviewOpen}
                  aria-label="查看所有标签"
                  className={cn(
                    "kerminal-focus-ring kerminal-pressable kerminal-muted-surface flex h-8 w-8 shrink-0 items-center justify-center rounded-[var(--radius-control)] border border-[var(--border-subtle)] text-[var(--text-secondary)] shadow-sm shadow-black/10 hover:bg-[var(--surface-hover)] hover:text-[var(--text-primary)] dark:shadow-black/30",
                    overviewOpen &&
                      "border-sky-500/30 bg-[var(--surface-selected)] text-sky-700 dark:text-sky-100",
                  )}
                  onClick={onToggleOverview}
                  ref={overviewButtonRef}
                  title="查看所有标签"
                  type="button"
                >
                  <ChevronDown className="h-4 w-4" />
                </button>
                <TerminalCreateButton
                  buttonRef={createButtonRef}
                  canOpenPanel={canOpenCreatePanel}
                  onCreateDefault={onCreateTerminal}
                  onRequestOpen={setCreatePanelPosition}
                  panelOpen={Boolean(createPanelPosition)}
                  placement="fixed"
                />
              </div>
            ) : null}
          </div>
        </MaybeSortableContext>
        <div aria-live="polite" className="sr-only">
          {dragAnnouncement}
        </div>
        <MaybeDragOverlay enabled={dragEnabled} draggingLabel={draggingLabel} />
      </MaybeDndContext>
      {createPanelPosition ? (
        <TerminalCreatePanel
          hostOptions={hostCreateOptions}
          onClose={closeCreatePanel}
          onCreateProfile={onCreateTerminal}
          onOpenConnection={onOpenConnection}
          onOpenHost={onOpenSavedTerminal}
          position={createPanelPosition}
          profileOptions={profileCreateOptions}
          triggerRef={createButtonRef}
        />
      ) : null}
    </>
  );
}
