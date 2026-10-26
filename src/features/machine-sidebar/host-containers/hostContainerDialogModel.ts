/**
 * Host-scoped Docker container dialog view model.
 *
 * @author kongweiguang
 */

import type {
  DockerContainerStatus,
  DockerContainerSummary,
} from "../../../lib/dockerApi";
import {
  buildComposeProjectViews,
  readContainerComposeConfigPaths,
  readContainerComposeProject,
  readContainerComposeService,
  type ComposeProjectContainerSummary,
} from "./composeProjectModel";

export type HostContainerGroupMode = "compose" | "status" | "flat";
export type HostContainerLifecycleAction =
  | "start"
  | "stop"
  | "restart"
  | "remove";
export type HostContainerInspectorTab = "details" | "stats";
export type HostContainerSelection =
  | { kind: "container"; containerId: string }
  | { kind: "project"; projectId: string }
  | null;

export type HostContainerMetadata = ComposeProjectContainerSummary;

export interface HostContainerDialogViewOptions {
  groupMode: HostContainerGroupMode;
  query: string;
}

export interface HostContainerGroupView {
  id: string;
  containers: HostContainerMetadata[];
  runningCount: number;
  title: string;
  totalCount: number;
}

export interface HostContainerDialogViewModel {
  containers: HostContainerMetadata[];
  emptySearch: boolean;
  groups: HostContainerGroupView[];
  runningCount: number;
  selectedContainer?: HostContainerMetadata;
  stoppedCount: number;
  totalCount: number;
}

export interface HostContainerLifecycleDialogCopy {
  confirmLabel: string;
  description: string;
  helperText: string;
  inputLabel?: string;
  placeholder?: string;
  title: string;
  variant: "danger" | "primary";
}

const statusSortOrder: Record<DockerContainerStatus, number> = {
  running: 0,
  restarting: 1,
  paused: 2,
  created: 3,
  exited: 4,
  dead: 5,
  unknown: 6,
};

const statusLabels: Record<DockerContainerStatus, string> = {
  created: "已创建",
  dead: "异常",
  exited: "已停止",
  paused: "已暂停",
  restarting: "重启中",
  running: "运行中",
  unknown: "未知",
};

export function buildHostContainerDialogViewModel(
  containers: DockerContainerSummary[],
  options: HostContainerDialogViewOptions,
  selectedContainerId?: string,
): HostContainerDialogViewModel {
  const normalizedQuery = normalizeSearch(options.query);
  const source = containers.map(
    (container) => container as HostContainerMetadata,
  );
  const filtered = sortHostContainers(
    normalizedQuery
      ? source.filter((container) =>
          containerMatchesQuery(container, normalizedQuery),
        )
      : source,
  );
  const groups = buildGroups(filtered, options.groupMode);
  const selectedContainer =
    filtered.find((container) => container.id === selectedContainerId) ??
    filtered[0];

  return {
    containers: filtered,
    emptySearch: source.length > 0 && filtered.length === 0,
    groups,
    runningCount: source.filter((container) => container.status === "running")
      .length,
    selectedContainer,
    stoppedCount: source.filter((container) => container.status !== "running")
      .length,
    totalCount: source.length,
  };
}

export function resolveHostContainerSelection(
  containers: DockerContainerSummary[],
  groupMode: HostContainerGroupMode,
  current: HostContainerSelection,
  initialContainerId?: string,
): HostContainerSelection {
  const hasContainer = (containerId: string) =>
    containers.some((container) => container.id === containerId);

  if (initialContainerId && hasContainer(initialContainerId)) {
    return { kind: "container", containerId: initialContainerId };
  }
  if (current?.kind === "container" && hasContainer(current.containerId)) {
    return current;
  }

  if (groupMode === "compose") {
    const composeView = buildComposeProjectViews(containers);
    if (
      current?.kind === "project" &&
      composeView.projects.some((project) => project.id === current.projectId)
    ) {
      return current;
    }
    if (composeView.projects[0]) {
      return { kind: "project", projectId: composeView.projects[0].id };
    }
  }

  return containers[0]
    ? { kind: "container", containerId: containers[0].id }
    : null;
}

export function containerProjectName(container: HostContainerMetadata) {
  return readContainerComposeProject(container);
}

export function containerComposeService(container: HostContainerMetadata) {
  return readContainerComposeService(container);
}

export function hostContainerStatusLabel(status: DockerContainerStatus) {
  return statusLabels[status] ?? statusLabels.unknown;
}

export function hostContainerStatusTone(status: DockerContainerStatus) {
  if (status === "running") {
    return "running";
  }
  if (status === "paused" || status === "restarting" || status === "created") {
    return "attention";
  }
  if (status === "dead") {
    return "danger";
  }
  return "muted";
}

export function canEnterHostContainer(container: DockerContainerSummary) {
  return container.status === "running";
}

export function canRunHostContainerLifecycleAction(
  container: DockerContainerSummary,
  action: HostContainerLifecycleAction,
) {
  switch (action) {
    case "start":
      return (
        container.status !== "running" && container.status !== "restarting"
      );
    case "stop":
      return (
        container.status === "running" ||
        container.status === "paused" ||
        container.status === "restarting"
      );
    case "restart":
      return container.status === "running" || container.status === "paused";
    case "remove":
      return (
        container.status !== "running" && container.status !== "restarting"
      );
    default:
      return false;
  }
}

export function hostContainerLifecycleDisabledReason(
  container: DockerContainerSummary,
  action: HostContainerLifecycleAction,
) {
  if (canRunHostContainerLifecycleAction(container, action)) {
    return undefined;
  }
  switch (action) {
    case "start":
      return "容器已在运行或正在重启";
    case "stop":
      return "只有运行中、暂停或重启中的容器可以停止";
    case "restart":
      return "只有运行中或暂停的容器可以重启";
    case "remove":
      return "先停止容器再删除";
    default:
      return "当前状态不可执行";
  }
}

/** Lifecycle guidance stays independent of the confirmation surface so dialogs remain reusable across sidebar and tool panel entry points. */
export function hostContainerLifecycleDialogCopy(
  action: HostContainerLifecycleAction,
  container: DockerContainerSummary,
): HostContainerLifecycleDialogCopy {
  switch (action) {
    case "stop":
      return {
        confirmLabel: "停止容器",
        description: `停止 ${container.name} 会中断其中正在运行的进程。`,
        helperText: "停止后可重新启动容器。",
        title: "停止容器",
        variant: "danger" as const,
      };
    case "restart":
      return {
        confirmLabel: "重启容器",
        description: `重启 ${container.name} 会先停止再启动容器。`,
        helperText: "适合服务卡住或需要重新加载配置时使用。",
        title: "重启容器",
        variant: "primary" as const,
      };
    case "remove":
      return {
        confirmLabel: "删除容器",
        description: `删除 ${container.name} 会移除该容器对象，容器内未持久化数据可能丢失。`,
        helperText: "请输入容器名以确认删除。",
        inputLabel: "容器名",
        placeholder: container.name,
        title: "删除容器",
        variant: "danger" as const,
      };
    case "start":
    default:
      return {
        confirmLabel: "启动容器",
        description: `启动 ${container.name}。`,
        helperText: "启动完成后可以进入容器终端。",
        title: "启动容器",
        variant: "primary" as const,
      };
  }
}

function buildGroups(
  containers: HostContainerMetadata[],
  groupMode: HostContainerGroupMode,
): HostContainerGroupView[] {
  if (groupMode === "flat") {
    return [groupView("all", "全部容器", containers)].filter(
      (group) => group.totalCount > 0,
    );
  }

  const groups = new Map<string, HostContainerMetadata[]>();
  for (const container of containers) {
    const key =
      groupMode === "status"
        ? hostContainerStatusLabel(container.status)
        : containerProjectName(container) || "独立容器";
    groups.set(key, [...(groups.get(key) ?? []), container]);
  }

  return [...groups.entries()]
    .map(([title, groupContainers]) =>
      groupView(groupIdForTitle(groupMode, title), title, groupContainers),
    )
    .sort(compareGroups);
}

function groupView(
  id: string,
  title: string,
  containers: HostContainerMetadata[],
): HostContainerGroupView {
  return {
    containers,
    id,
    runningCount: containers.filter(
      (container) => container.status === "running",
    ).length,
    title,
    totalCount: containers.length,
  };
}

function compareGroups(
  left: HostContainerGroupView,
  right: HostContainerGroupView,
) {
  const leftStandalone = left.title === "独立容器";
  const rightStandalone = right.title === "独立容器";
  if (leftStandalone !== rightStandalone) {
    return leftStandalone ? 1 : -1;
  }
  return (
    right.runningCount - left.runningCount ||
    left.title.localeCompare(right.title)
  );
}

function groupIdForTitle(mode: HostContainerGroupMode, title: string) {
  return `${mode}:${title.toLowerCase().replace(/\s+/g, "-")}`;
}

function sortHostContainers(containers: HostContainerMetadata[]) {
  return [...containers].sort(
    (left, right) =>
      statusSortOrder[left.status] - statusSortOrder[right.status] ||
      containerProjectName(left).localeCompare(containerProjectName(right)) ||
      left.name.localeCompare(right.name),
  );
}

function containerMatchesQuery(
  container: HostContainerMetadata,
  normalizedQuery: string,
) {
  return [
    container.name,
    container.image,
    container.shortId,
    container.id,
    container.statusText,
    container.state,
    container.runtime,
    containerProjectName(container),
    containerComposeService(container),
    ...readContainerComposeConfigPaths(container),
    ...container.ports,
  ]
    .filter(Boolean)
    .join(" ")
    .toLowerCase()
    .includes(normalizedQuery);
}

function normalizeSearch(query: string) {
  return query.trim().toLowerCase();
}
