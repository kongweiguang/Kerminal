// @author kongweiguang

import type { HostContainersToolContentProps } from "./HostContainersToolContent";
import type { Machine, MachineGroup } from "../workspace/contracts/index";

export type MachineSidebarViewMode = "hosts" | "containers";

export type ConnectionOpenOptions = {
  groupId?: string;
  mode?: "local" | "rdp" | "ssh" | "sftp" | "telnet" | "serial";
};

export interface MachineSidebarProps {
  activeView?: MachineSidebarViewMode;
  collapsed?: boolean;
  collapsedGroupIds?: readonly string[];
  containerHostId?: string | null;
  containerInitialContainerId?: string;
  groups: MachineGroup[];
  openMachineIds?: readonly string[];
  rdpOpeningMachineIds?: readonly string[];
  selectedMachineId: string;
  search: string;
  settingsSelected?: boolean;
  onActiveViewChange?: (view: MachineSidebarViewMode) => void;
  onSearchChange: (query: string) => void;
  onAddConnection?: (options?: ConnectionOpenOptions) => void;
  onAddGroup?: () => void;
  onAddMachine?: (groupId?: string) => void;
  onCollapsedGroupIdsChange?: (groupIds: string[]) => void;
  onDeleteGroup?: (groupId: string) => void;
  onDeleteMachine?: (machineId: string) => void;
  onDuplicateMachine?: (machineId: string) => void;
  onEditGroup?: (groupId: string) => void;
  onEditMachine?: (machineId: string) => void;
  onMoveMachine?: (machineId: string, groupId: string) => void;
  onExternalMachineDrag?: (
    event: MachineSidebarMachineDragEvent,
  ) => MachineSidebarExternalDragFeedback | void;
  onExternalMachineDragEnd?: () => void;
  onExternalMachineDrop?: (event: MachineSidebarMachineDragEvent) => boolean;
  onOpenHostContainers?: (machineId: string) => void;
  onOpenContainerDetails?: (machineId: string) => void;
  onOpenLocalTerminal?: (machineId: string) => void;
  onOpenContainerTerminal?: (machineId: string) => void;
  onOpenRdpConnection?: (machineId: string) => void;
  onOpenSettings?: () => void;
  onOpenUpdates?: () => void;
  onOpenSftp?: (machineId: string) => void;
  onOpenSshTerminal?: (machineId: string) => void;
  onOpenSftpTransferWorkbench?: (machineId: string) => void;
  onOpenTransferWorkbench?: () => void;
  onOpenTelnetTerminal?: (machineId: string) => void;
  onOpenSerialTerminal?: (machineId: string) => void;
  onPinGroup?: (groupId: string, pinned?: boolean) => void;
  onSelectMachine: (machineId: string) => void;
  onContainerHostChange?: (hostId: string) => void;
  onEnterContainer?: HostContainersToolContentProps["onEnterContainer"];
  onFetchContainerStats?: HostContainersToolContentProps["onFetchContainerStats"];
  onInspectContainer?: HostContainersToolContentProps["onInspectContainer"];
  onLifecycleContainer?: HostContainersToolContentProps["onLifecycleContainer"];
  onListDockerContainers?: HostContainersToolContentProps["onListDockerContainers"];
  onOpenContainerLogs?: HostContainersToolContentProps["onOpenContainerLogs"];
  onOpenWorkspaceFileTab?: HostContainersToolContentProps["onOpenWorkspaceFileTab"];
  onPinContainer?: HostContainersToolContentProps["onPinContainer"];
}

export type SidebarContextMenu =
  | {
      type: "group";
      groupId: string;
      x: number;
      y: number;
    }
  | {
      type: "machine";
      groupId: string;
      machineId: string;
      x: number;
      y: number;
    }
  | {
      type: "root";
      x: number;
      y: number;
    };

export type SidebarContextMenuPayload =
  | {
      type: "group";
      groupId: string;
    }
  | {
      type: "machine";
      groupId: string;
      machineId: string;
    }
  | {
      type: "root";
    };

export const statusClasses = {
  online: "bg-emerald-400",
  offline: "bg-zinc-500",
  warning: "bg-amber-400",
};
export const CONTEXT_MENU_MARGIN = 8;
export const POINTER_DRAG_THRESHOLD_PX = 6;
export const SIDEBAR_GROUP_DROP_TARGET_ATTRIBUTE = "data-machine-sidebar-group-id";

export type PointerMachineDrag = {
  active: boolean;
  machineId: string;
  pointerId: number;
  startX: number;
  startY: number;
};

export type MachineDragPreview = {
  machine: Machine;
  externalTargetHint?: string;
  x: number;
  y: number;
};

export type MachineSidebarMachineDragEvent = {
  clientX: number;
  clientY: number;
  machine: Machine;
};

export type MachineSidebarExternalDragFeedback = {
  hint?: string;
};
