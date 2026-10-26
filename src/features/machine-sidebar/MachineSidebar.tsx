// @author kongweiguang

import {
  ChevronsDownUp,
  ChevronsUpDown,
  Plus,
} from "lucide-react";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
} from "react";
import { createPortal } from "react-dom";
import { Button } from "../../components/ui/button";
import type { Machine } from "../workspace/contracts/index";
import {
  POINTER_DRAG_THRESHOLD_PX,
  SIDEBAR_GROUP_DROP_TARGET_ATTRIBUTE,
  type MachineDragPreview,
  type MachineSidebarProps,
  type PointerMachineDrag,
  type SidebarContextMenu,
  type SidebarContextMenuPayload,
} from "./MachineSidebar.shared";
import { MachineSidebarContextMenuPortal } from "./MachineSidebarContextMenuPortal";
import { MachineSidebarHostList } from "./MachineSidebarHostList";
import { MachineSidebarHeader } from "./MachineSidebarHeader";
import { MachineSidebarSettingsButtons } from "./MachineSidebarSettingsButtons";
import {
  CollapsedHostPopover,
  CollapsedMachineSidebar,
  MachineDragPreviewCard,
  clampContextMenuPosition,
  isPinnedMachineGroup,
} from "./MachineSidebar.parts";
import { buildVisibleMachineGroups } from "./machineSidebarVisibilityModel";

export type { ConnectionOpenOptions } from "./MachineSidebar.shared";

const sidebarFooterClassName =
  "kerminal-sidebar-footer flex items-center justify-between border-t border-[var(--border-subtle)]";

function stringSetsEqual(left: Set<string>, right: Set<string>) {
  if (left.size !== right.size) {
    return false;
  }

  for (const value of left) {
    if (!right.has(value)) {
      return false;
    }
  }

  return true;
}

/** 主机树是唯一左栏视图；状态和快捷入口仍委托既有 workspace 与菜单回调。 */
export function MachineSidebar({
  collapsed = false,
  collapsedGroupIds: controlledCollapsedGroupIds,
  groups,
  openMachineIds = [],
  rdpOpeningMachineIds = [],
  onAddConnection,
  onAddGroup,
  onAddMachine,
  onCollapsedGroupIdsChange,
  onDeleteGroup,
  onDeleteMachine,
  onDuplicateMachine,
  onEditGroup,
  onEditMachine,
  onExternalMachineDrag,
  onExternalMachineDragEnd,
  onExternalMachineDrop,
  onMoveMachine,
  onSearchChange,
  onOpenContainerDetails,
  onOpenHostContainers,
  onOpenLocalTerminal,
  onOpenContainerTerminal,
  onOpenRdpConnection,
  onOpenSettings,
  onOpenUpdates,
  onOpenSftp,
  onOpenSshTerminal,
  onOpenSftpTransferWorkbench,
  onOpenTransferWorkbench,
  onOpenTelnetTerminal,
  onOpenSerialTerminal,
  onPinGroup,
  onSelectMachine,
  search,
  selectedMachineId,
  settingsSelected = false,
}: MachineSidebarProps) {
  const [uncontrolledCollapsedGroupIds, setUncontrolledCollapsedGroupIds] =
    useState<Set<string>>(() => new Set());
  const [contextMenu, setContextMenu] = useState<SidebarContextMenu | null>(
    null,
  );
  const [dragOverGroupId, setDragOverGroupId] = useState<string | null>(null);
  const [dragPreview, setDragPreview] = useState<MachineDragPreview | null>(
    null,
  );
  const [draggingMachineId, setDraggingMachineId] = useState<string | null>(null);
  const [collapsedHostPopoverOpen, setCollapsedHostPopoverOpen] = useState(false);
  const contextMenuRef = useRef<HTMLDivElement>(null);
  const collapsedHostButtonRef = useRef<HTMLButtonElement>(null);
  const collapsedHostPopoverRef = useRef<HTMLDivElement>(null);
  const pointerDragRef = useRef<PointerMachineDrag | null>(null);
  const pointerDragCleanupRef = useRef<(() => void) | null>(null);
  const suppressNextClickRef = useRef(false);
  const collapsedGroupIds = useMemo(
    () =>
      controlledCollapsedGroupIds
        ? new Set(controlledCollapsedGroupIds)
        : uncontrolledCollapsedGroupIds,
    [controlledCollapsedGroupIds, uncontrolledCollapsedGroupIds],
  );
  const updateCollapsedGroupIds = useCallback(
    (updater: (current: Set<string>) => Set<string>) => {
      const current = new Set(
        controlledCollapsedGroupIds ?? uncontrolledCollapsedGroupIds,
      );
      const next = updater(current);
      if (stringSetsEqual(current, next)) {
        return;
      }

      if (!controlledCollapsedGroupIds) {
        setUncontrolledCollapsedGroupIds(next);
      }
      onCollapsedGroupIdsChange?.([...next].sort());
    },
    [
      controlledCollapsedGroupIds,
      onCollapsedGroupIdsChange,
      uncontrolledCollapsedGroupIds,
    ],
  );
  const openMachineIdSet = useMemo(
    () => new Set(openMachineIds),
    [openMachineIds],
  );
  const rdpOpeningMachineIdSet = useMemo(
    () => new Set(rdpOpeningMachineIds),
    [rdpOpeningMachineIds],
  );
  const normalizedSearch = search.trim().toLowerCase();
  const hasSearch = normalizedSearch.length > 0;
  const visibleGroups = useMemo(
    () => buildVisibleMachineGroups(groups, normalizedSearch),
    [groups, normalizedSearch],
  );
  const machineCount = useMemo(
    () => groups.reduce((total, group) => total + group.machines.length, 0),
    [groups],
  );
  const allGroupsCollapsed = useMemo(
    () =>
      groups.length > 0 && groups.every((group) => collapsedGroupIds.has(group.id)),
    [collapsedGroupIds, groups],
  );
  const groupToggleLabel = allGroupsCollapsed
    ? "展开所有分组"
    : "折叠所有分组";
  const GroupToggleIcon = allGroupsCollapsed ? ChevronsUpDown : ChevronsDownUp;
  const contextGroup =
    contextMenu?.type === "group" || contextMenu?.type === "machine"
      ? groups.find((group) => group.id === contextMenu.groupId)
      : undefined;
  const contextMachine =
    contextMenu?.type === "machine"
      ? contextGroup?.machines.find(
          (machine) => machine.id === contextMenu.machineId,
        )
      : undefined;
  const contextGroupPinned = isPinnedMachineGroup(contextGroup);
  const dragTargetGroup = dragOverGroupId
    ? groups.find((group) => group.id === dragOverGroupId)
    : undefined;
  useLayoutEffect(() => {
    if (groups.length === 0) {
      return;
    }

    const nextKnownGroupIds = new Set(groups.map((group) => group.id));

    updateCollapsedGroupIds((current) => {
      const next = new Set<string>();

      for (const groupId of current) {
        if (nextKnownGroupIds.has(groupId)) {
          next.add(groupId);
        }
      }

      if (next.size !== current.size) {
        return next;
      }

      for (const groupId of next) {
        if (!current.has(groupId)) {
          return next;
        }
      }

      return current;
    });
  }, [groups, updateCollapsedGroupIds]);

  useEffect(() => {
    if (!contextMenu) {
      return undefined;
    }

    const close = () => setContextMenu(null);
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setContextMenu(null);
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
  }, [contextMenu]);

  useEffect(() => {
    if (!collapsed) {
      setCollapsedHostPopoverOpen(false);
    }
  }, [collapsed]);

  useEffect(() => {
    if (!collapsedHostPopoverOpen) {
      return undefined;
    }

    const closeOnOutsideClick = (event: MouseEvent) => {
      const target = event.target;
      if (!(target instanceof Node)) {
        return;
      }
      if (
        collapsedHostPopoverRef.current?.contains(target) ||
        collapsedHostButtonRef.current?.contains(target)
      ) {
        return;
      }
      setCollapsedHostPopoverOpen(false);
    };
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setCollapsedHostPopoverOpen(false);
      }
    };
    const closeOnResize = () => setCollapsedHostPopoverOpen(false);

    window.addEventListener("click", closeOnOutsideClick);
    window.addEventListener("keydown", closeOnEscape);
    window.addEventListener("resize", closeOnResize);
    return () => {
      window.removeEventListener("click", closeOnOutsideClick);
      window.removeEventListener("keydown", closeOnEscape);
      window.removeEventListener("resize", closeOnResize);
    };
  }, [collapsedHostPopoverOpen]);

  useLayoutEffect(() => {
    if (!contextMenu) {
      return;
    }

    const menuElement = contextMenuRef.current;
    if (!menuElement) {
      return;
    }

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

  const runMenuAction = (action?: () => void) => {
    setContextMenu(null);
    action?.();
  };

  const openContextMenu = (
    event: ReactMouseEvent,
    menu: SidebarContextMenuPayload,
  ) => {
    event.preventDefault();
    event.stopPropagation();
    const position = clampContextMenuPosition(event.clientX, event.clientY, 0, 0);
    const menuState = {
      ...menu,
      ...position,
    };
    setContextMenu(menuState);
  };

  const toggleGroup = (groupId: string) => {
    updateCollapsedGroupIds((current) => {
      const next = new Set(current);
      if (next.has(groupId)) {
        next.delete(groupId);
      } else {
        next.add(groupId);
      }
      return next;
    });
  };

  const toggleAllGroups = () => {
    updateCollapsedGroupIds((current) => {
      if (groups.length === 0) {
        return current;
      }
      if (groups.every((group) => current.has(group.id))) {
        return new Set();
      }
      return new Set(groups.map((group) => group.id));
    });
  };

  const openMachineSession = (machine: Machine) => {
    if (
      machine.kind === "rdp" &&
      rdpOpeningMachineIdSet.has(machine.id)
    ) {
      return;
    }
    onSelectMachine(machine.id);
    if (machine.kind === "sftp") {
      onOpenSftpTransferWorkbench?.(machine.id);
      return;
    }
    if (machine.kind === "ssh") {
      onOpenSshTerminal?.(machine.id);
      return;
    }
    if (machine.kind === "telnet") {
      onOpenTelnetTerminal?.(machine.id);
      return;
    }
    if (machine.kind === "serial") {
      onOpenSerialTerminal?.(machine.id);
      return;
    }
    if (machine.kind === "dockerContainer") {
      onOpenContainerTerminal?.(machine.id);
      return;
    }
    if (machine.kind === "rdp") {
      onOpenRdpConnection?.(machine.id);
      return;
    }
    if (machine.kind === "local") {
      onOpenLocalTerminal?.(machine.id);
    }
  };

  const selectMachine = (machine: Machine) => {
    if (machine.kind === "dockerContainer") {
      openMachineSession(machine);
      return;
    }
    onSelectMachine(machine.id);
  };

  /** 主机菜单把目标交给 Shell 统一解析，避免侧栏维护容器目标状态。 */
  const openHostContainersInSidebar = (machineId: string) => {
    onSelectMachine(machineId);
    onOpenHostContainers?.(machineId);
  };

  /** 固定容器详情导航由 Shell 结合其宿主机与运行时完成。 */
  const openContainerDetailsInSidebar = (machineId: string) => {
    onOpenContainerDetails?.(machineId);
  };

  const cleanupPointerDrag = (notifyExternal = false) => {
    const shouldNotifyExternal = notifyExternal && pointerDragRef.current?.active;
    pointerDragCleanupRef.current?.();
    pointerDragCleanupRef.current = null;
    pointerDragRef.current = null;
    setDragPreview(null);
    setDraggingMachineId(null);
    setDragOverGroupId(null);
    if (shouldNotifyExternal) {
      onExternalMachineDragEnd?.();
    }
  };

  const groupIdFromPoint = (clientX: number, clientY: number) => {
    const element = document.elementFromPoint(clientX, clientY);
    if (!(element instanceof HTMLElement)) {
      return null;
    }
    return (
      element.closest<HTMLElement>(`[${SIDEBAR_GROUP_DROP_TARGET_ATTRIBUTE}]`)
        ?.dataset.machineSidebarGroupId ?? null
    );
  };

  const startPointerMachineDrag = (
    event: ReactPointerEvent<HTMLButtonElement>,
    machine: Machine,
  ) => {
    if (
      (!onMoveMachine && !onExternalMachineDrag && !onExternalMachineDrop) ||
      rdpOpeningMachineIdSet.has(machine.id) ||
      event.button !== 0
    ) {
      return;
    }

    cleanupPointerDrag(true);
    pointerDragRef.current = {
      active: false,
      machineId: machine.id,
      pointerId: event.pointerId,
      startX: event.clientX,
      startY: event.clientY,
    };

    const movePointerMachineDrag = (moveEvent: PointerEvent) => {
      const drag = pointerDragRef.current;
      if (!drag || drag.pointerId !== moveEvent.pointerId) {
        return;
      }

      const deltaX = moveEvent.clientX - drag.startX;
      const deltaY = moveEvent.clientY - drag.startY;
      if (!drag.active) {
        const distance = Math.hypot(deltaX, deltaY);
        if (distance < POINTER_DRAG_THRESHOLD_PX) {
          return;
        }
        pointerDragRef.current = { ...drag, active: true };
        suppressNextClickRef.current = true;
        setDraggingMachineId(drag.machineId);
      }

      moveEvent.preventDefault();
      const nextDragOverGroupId = groupIdFromPoint(
        moveEvent.clientX,
        moveEvent.clientY,
      );
      const externalDragFeedback = onExternalMachineDrag?.({
        clientX: moveEvent.clientX,
        clientY: moveEvent.clientY,
        machine,
      });
      setDragOverGroupId(nextDragOverGroupId);
      setDragPreview({
        externalTargetHint: externalDragFeedback?.hint,
        machine,
        x: moveEvent.clientX,
        y: moveEvent.clientY,
      });
    };

    const finishPointerMachineDrag = (upEvent: PointerEvent) => {
      const drag = pointerDragRef.current;
      if (!drag || drag.pointerId !== upEvent.pointerId) {
        return;
      }

      const targetGroupId = drag.active
        ? groupIdFromPoint(upEvent.clientX, upEvent.clientY)
        : null;
      const machineId = drag.machineId;
      const shouldMove = Boolean(drag.active && targetGroupId);
      const externalDropConsumed =
        drag.active &&
        Boolean(
          onExternalMachineDrop?.({
            clientX: upEvent.clientX,
            clientY: upEvent.clientY,
            machine,
          }),
        );
      if (drag.active) {
        suppressNextClickRef.current = true;
        window.setTimeout(() => {
          suppressNextClickRef.current = false;
        }, 0);
      }

      cleanupPointerDrag(true);
      if (externalDropConsumed) {
        upEvent.preventDefault();
        return;
      }
      if (shouldMove && targetGroupId && onMoveMachine) {
        upEvent.preventDefault();
        onMoveMachine(machineId, targetGroupId);
      }
    };

    const cancelPointerMachineDrag = (cancelEvent: PointerEvent) => {
      const drag = pointerDragRef.current;
      if (!drag || drag.pointerId !== cancelEvent.pointerId) {
        return;
      }
      cleanupPointerDrag(true);
    };

    window.addEventListener("pointermove", movePointerMachineDrag);
    window.addEventListener("pointerup", finishPointerMachineDrag);
    window.addEventListener("pointercancel", cancelPointerMachineDrag);
    pointerDragCleanupRef.current = () => {
      window.removeEventListener("pointermove", movePointerMachineDrag);
      window.removeEventListener("pointerup", finishPointerMachineDrag);
      window.removeEventListener("pointercancel", cancelPointerMachineDrag);
    };
  };

  const handleMachineClick = (machine: Machine) => {
    if (suppressNextClickRef.current) {
      suppressNextClickRef.current = false;
      return;
    }
    selectMachine(machine);
  };

  useEffect(
    () => () => {
      pointerDragCleanupRef.current?.();
    },
    [],
  );

  const toggleCollapsedHostPopover = (event: ReactMouseEvent<HTMLButtonElement>) => {
    event.stopPropagation();
    setCollapsedHostPopoverOpen((open) => !open);
  };

  const contextMenuElement = (
    <MachineSidebarContextMenuPortal
      contextGroup={contextGroup}
      contextGroupPinned={contextGroupPinned}
      contextMachine={contextMachine}
      contextMenu={contextMenu}
      menuRef={contextMenuRef}
      onAddConnection={onAddConnection}
      onAddGroup={onAddGroup}
      onAddMachine={onAddMachine}
      onDeleteGroup={onDeleteGroup}
      onDeleteMachine={onDeleteMachine}
      onDuplicateMachine={onDuplicateMachine}
      onEditGroup={onEditGroup}
      onEditMachine={onEditMachine}
      onOpenContainerDetails={openContainerDetailsInSidebar}
      onOpenHostContainers={openHostContainersInSidebar}
      onOpenLocalTerminal={onOpenLocalTerminal}
      onOpenContainerTerminal={onOpenContainerTerminal}
      onOpenRdpConnection={onOpenRdpConnection}
      onOpenSftp={onOpenSftp}
      onOpenSshTerminal={onOpenSshTerminal}
      onOpenSftpTransferWorkbench={onOpenSftpTransferWorkbench}
      onOpenTelnetTerminal={onOpenTelnetTerminal}
      onOpenSerialTerminal={onOpenSerialTerminal}
      onPinGroup={onPinGroup}
      rdpOpeningMachineIdSet={rdpOpeningMachineIdSet}
      runMenuAction={runMenuAction}
    />
  );

  const collapsedHostPopoverElement = (
    <CollapsedHostPopover
      allGroupsCollapsed={allGroupsCollapsed}
      collapsedGroupIds={collapsedGroupIds}
      dragOverGroupId={dragOverGroupId}
      draggingMachineId={draggingMachineId}
      forceGroupsExpanded={hasSearch}
      groupCount={groups.length}
      groupToggleIcon={<GroupToggleIcon className="h-4 w-4" />}
      groupToggleLabel={groupToggleLabel}
      handleMachineClick={handleMachineClick}
      machineCount={machineCount}
      onMoveMachine={onMoveMachine}
      onSelectMachine={onSelectMachine}
      open={collapsedHostPopoverOpen}
      openContextMenu={openContextMenu}
      openMachineIdSet={openMachineIdSet}
      openMachineSession={openMachineSession}
      popoverRef={collapsedHostPopoverRef}
      rdpOpeningMachineIdSet={rdpOpeningMachineIdSet}
      selectedMachineId={selectedMachineId}
      startPointerMachineDrag={startPointerMachineDrag}
      toggleAllGroups={toggleAllGroups}
      toggleGroup={toggleGroup}
      visibleGroups={visibleGroups}
    />
  );

  const dragPreviewElement =
    dragPreview && typeof document !== "undefined"
      ? createPortal(
          <MachineDragPreviewCard
            externalTargetHint={dragPreview.externalTargetHint}
            machine={dragPreview.machine}
            targetGroupTitle={dragTargetGroup?.title}
            x={dragPreview.x}
            y={dragPreview.y}
          />,
          document.body,
        )
      : null;

  if (collapsed) {
    return (
      <CollapsedMachineSidebar
        collapsedHostButtonRef={collapsedHostButtonRef}
        collapsedHostPopoverElement={collapsedHostPopoverElement}
        collapsedHostPopoverOpen={collapsedHostPopoverOpen}
        contextMenuElement={contextMenuElement}
        dragPreviewElement={dragPreviewElement}
        onAddConnection={onAddConnection}
        onOpenSettings={onOpenSettings}
        onOpenUpdates={onOpenUpdates}
        onOpenTransferWorkbench={onOpenTransferWorkbench}
        openContextMenu={openContextMenu}
        openMachineIdSet={openMachineIdSet}
        settingsSelected={settingsSelected}
        toggleCollapsedHostPopover={toggleCollapsedHostPopover}
      />
    );
  }

  return (
    <aside
      aria-label="主机侧边栏"
      className="kerminal-material-nav kerminal-shell-sidebar relative flex h-full w-full min-w-[220px] flex-col border-r"
      onContextMenu={(event) => openContextMenu(event, { type: "root" })}
    >
      <MachineSidebarHeader
        allGroupsCollapsed={allGroupsCollapsed}
        groupCount={groups.length}
        groupToggleLabel={groupToggleLabel}
        onOpenTransferWorkbench={onOpenTransferWorkbench}
        onSearchChange={onSearchChange}
        onToggleAllGroups={toggleAllGroups}
        search={search}
      />

      <MachineSidebarHostList
        collapsedGroupIds={collapsedGroupIds}
        dragOverGroupId={dragOverGroupId}
        draggingMachineId={draggingMachineId}
        groups={groups}
        hasSearch={hasSearch}
        onAddConnection={onAddConnection}
        onMachineClick={handleMachineClick}
        onMoveMachine={onMoveMachine}
        onOpenContextMenu={openContextMenu}
        onOpenMachineSession={openMachineSession}
        onSelectMachine={onSelectMachine}
        onStartPointerDrag={startPointerMachineDrag}
        onToggleGroup={toggleGroup}
        openMachineIdSet={openMachineIdSet}
        rdpOpeningMachineIdSet={rdpOpeningMachineIdSet}
        selectedMachineId={selectedMachineId}
        visibleGroups={visibleGroups}
      />

      <div className={sidebarFooterClassName}>
        <div className="flex items-center gap-1">
          <MachineSidebarSettingsButtons
            onOpenSettings={onOpenSettings}
            onOpenUpdates={onOpenUpdates}
            settingsSelected={settingsSelected}
          />
        </div>
        {groups.length > 0 ? (
          <Button
            aria-label="添加连接"
            className="h-8 w-8 rounded-lg"
            disabled={!onAddConnection}
            onClick={() => onAddConnection?.({ mode: "ssh" })}
            size="icon"
            title="添加连接"
            variant="ghost"
          >
            <Plus className="h-4 w-4" />
          </Button>
        ) : null}
      </div>

      {contextMenuElement}
      {dragPreviewElement}
    </aside>
  );
}

