// @author kongweiguang

import {
  dockerContainerTarget,
  localTarget,
  serialTarget,
  sshTarget,
  targetStableId,
  telnetTarget,
  type ContainerRuntime,
  type RemoteTargetRef,
} from "../../lib/targetModel";
import type {
  Machine,
  TerminalPane,
  TerminalTab,
  ToolId,
} from "../workspace/contracts/index";
import { isWorkspaceFileTab } from "../workspace/contracts/index";
import type { WorkspaceContextRevision } from "../workspace/context";

/** 右栏能力拥有的上下文层级；读写动作只能使用对应层级生成的 binding。 */
export type ToolPanelBindingScope =
  | "global"
  | "workspace"
  | "tab"
  | "pane"
  | "target"
  | "host";

type ToolPanelBindingSource =
  | "global"
  | "workspace"
  | "focusedPane"
  | "activeTab"
  | "activeTarget"
  | "selectedMachine"
  | "none";

export interface ToolPanelContextInput {
  machineGroups?: readonly { machines: readonly Machine[] }[];
  activeMachine?: Machine;
  activeTab?: TerminalTab;
  focusedPane?: TerminalPane;
  selectedMachine?: Machine;
  workspaceRevision?: WorkspaceContextRevision;
}

/** 显式导航只在指定 Tab 和宿主机消费，避免切 Tab 后沿用另一台主机的详情定位。 */
export interface HostContainerRevealRequest {
  tabId: string;
  hostId: string;
  containerId?: string;
  runtime?: ContainerRuntime;
  requestId?: number;
}

/**
 * 单个右栏能力的不可变上下文投影。
 *
 * `bindingKey` 表示交互对象，`resourceKey` 表示可复用读取资源。相同主机的
 * 两个 pane 可以共享主机数据，但 pane 级动作仍使用不同 binding，避免跨分屏。
 */
export interface ToolPanelBinding {
  activeTab?: TerminalTab;
  bindingKey: string;
  focusedPane?: TerminalPane;
  machine?: Machine;
  resourceKey?: string;
  scope: ToolPanelBindingScope;
  source: ToolPanelBindingSource;
  target?: RemoteTargetRef;
  toolId: ToolId;
}

/** 全量覆盖 ToolId，新增右栏能力时必须先选择所有者层级。 */
export const toolPanelBindingScopes = {
  agentLauncher: "tab",
  containers: "host",
  context: "workspace",
  logs: "pane",
  ports: "host",
  settings: "global",
  sftp: "target",
  snippets: "pane",
  system: "target",
  tmux: "target",
} as const satisfies Record<ToolId, ToolPanelBindingScope>;

/** 按能力语义解析唯一 binding；不会越过当前 pane/tab 回退到另一台侧栏主机。 */
export function resolveToolPanelBinding(
  toolId: ToolId,
  context: ToolPanelContextInput,
): ToolPanelBinding {
  const scope = toolPanelBindingScopes[toolId];
  if (scope === "global") {
    return binding(toolId, scope, "global", `global:${toolId}`);
  }
  if (scope === "workspace") {
    const revision = context.workspaceRevision ?? 0;
    return binding(
      toolId,
      scope,
      "workspace",
      `workspace:${revision}`,
      undefined,
      context.activeTab,
      context.focusedPane,
    );
  }
  if (scope === "tab") {
    const resourceKey = context.activeTab
      ? `tab:${context.activeTab.id}`
      : "tab:unbound";
    return binding(
      toolId,
      scope,
      context.activeTab ? "activeTab" : "none",
      resourceKey,
      undefined,
      context.activeTab,
      context.focusedPane,
      context.activeMachine,
    );
  }
  if (scope === "pane") {
    const resourceKey = context.focusedPane
      ? `pane:${context.focusedPane.id}`
      : "pane:unbound";
    const target = targetFromPane(context.focusedPane);
    return binding(
      toolId,
      scope,
      context.focusedPane ? "focusedPane" : "none",
      resourceKey,
      target,
      context.activeTab,
      context.focusedPane,
      context.focusedPane ? context.activeMachine : undefined,
    );
  }

  // 系统监控描述当前工作区运行目标；没有 Tab 时不能继续刷新侧栏残留主机。
  const subject =
    toolId === "containers"
      ? containerHostSubject(context)
      : toolId === "system" &&
          !context.focusedPane &&
          !context.activeTab &&
          !context.activeMachine
        ? { source: "none" as const }
        : activeTargetSubject(context);
  const resourceKey =
    scope === "host"
      ? hostResourceKey(subject.machine, subject.target)
      : subject.target
        ? targetStableId(subject.target)
        : undefined;
  const unavailableKey = `${scope}:unbound:${subject.source}`;
  return binding(
    toolId,
    scope,
    subject.source,
    resourceKey ?? unavailableKey,
    subject.target,
    context.activeTab,
    context.focusedPane,
    subject.machine,
  );
}

interface ActiveTargetSubject {
  machine?: Machine;
  source: ToolPanelBindingSource;
  target?: RemoteTargetRef;
}

/** 容器工具始终操作当前目标的 SSH 宿主；缺失引用或不支持的协议不能借用侧栏选择。 */
function containerHostSubject(context: ToolPanelContextInput): ActiveTargetSubject {
  if (!context.focusedPane && !context.activeTab && !context.activeMachine) {
    return { source: "none" };
  }
  const subject = activeTargetSubject(context);
  const target = subject.target;
  const hostId =
    target?.kind === "ssh" || target?.kind === "dockerContainer"
      ? target.hostId
      : undefined;
  if (!hostId || subject.machine?.kind === "sftp") {
    return { source: subject.source };
  }
  const host =
    subject.machine?.kind === "ssh" && subject.machine.id === hostId
      ? subject.machine
      : context.machineGroups
          ?.flatMap((group) => group.machines)
          .find((machine) => machine.id === hostId && machine.kind === "ssh");
  return {
    source: subject.source,
    ...(host ? { machine: host, target: sshTarget(host.id) } : {}),
  };
}

function activeTargetSubject(
  context: ToolPanelContextInput,
): ActiveTargetSubject {
  // 聚焦 pane 是终端动作的最高优先级；即使目标不支持当前能力，也不能
  // 静默回退到侧栏选择的另一台主机。
  if (context.focusedPane) {
    return {
      machine: context.activeMachine,
      source: "focusedPane",
      target: targetFromPane(context.focusedPane),
    };
  }
  if (context.activeTab) {
    return {
      machine: context.activeMachine,
      source: "activeTab",
      target: isWorkspaceFileTab(context.activeTab)
        ? context.activeTab.target
        : targetFromMachine(context.activeMachine),
    };
  }
  if (context.activeMachine) {
    return {
      machine: context.activeMachine,
      source: "activeTarget",
      target: targetFromMachine(context.activeMachine),
    };
  }
  if (context.selectedMachine) {
    return {
      machine: context.selectedMachine,
      source: "selectedMachine",
      target: targetFromMachine(context.selectedMachine),
    };
  }
  return { source: "none" };
}

function binding(
  toolId: ToolId,
  scope: ToolPanelBindingScope,
  source: ToolPanelBindingSource,
  resourceKey: string,
  target?: RemoteTargetRef,
  activeTab?: TerminalTab,
  focusedPane?: TerminalPane,
  machine?: Machine,
): ToolPanelBinding {
  const targetKey = target ? targetStableId(target) : "no-target";
  return {
    ...(activeTab ? { activeTab } : {}),
    bindingKey: [
      toolId,
      scope,
      source,
      activeTab?.id ?? "no-tab",
      focusedPane?.id ?? "no-pane",
      targetKey,
      resourceKey,
    ].join("|"),
    ...(focusedPane ? { focusedPane } : {}),
    ...(machine ? { machine } : {}),
    resourceKey,
    scope,
    source,
    ...(target ? { target } : {}),
    toolId,
  };
}

function hostResourceKey(
  machine: Machine | undefined,
  target: RemoteTargetRef | undefined,
) {
  if (machine?.kind === "ssh") {
    return `host:${machine.id}`;
  }
  if (target?.kind === "ssh") {
    return `host:${target.hostId}`;
  }
  return undefined;
}

function targetFromPane(pane: TerminalPane | undefined) {
  if (!pane) {
    return undefined;
  }
  if (pane.target) {
    return pane.target;
  }
  if (pane.mode === "local") {
    return localTarget(pane.profileId);
  }
  if (pane.mode === "ssh") {
    return sshTarget(pane.remoteHostId ?? pane.machineId);
  }
  if (pane.mode === "telnet") {
    return telnetTarget(pane.remoteHostId ?? pane.machineId);
  }
  if (pane.mode === "serial") {
    return serialTarget(pane.remoteHostId ?? pane.machineId);
  }
  if (pane.mode === "container" && pane.remoteHostId && pane.containerId) {
    return dockerContainerTarget({
      containerId: pane.containerId,
      hostId: pane.remoteHostId,
      workdir: pane.currentCwd ?? pane.cwd,
    });
  }
  return undefined;
}

function targetFromMachine(machine: Machine | undefined) {
  if (!machine) {
    return undefined;
  }
  if (machine.target) {
    return machine.target;
  }
  if (machine.kind === "local") {
    return localTarget(machine.profileId);
  }
  if (machine.kind === "ssh") {
    return sshTarget(machine.id);
  }
  if (machine.kind === "telnet") {
    return telnetTarget(machine.id);
  }
  if (machine.kind === "serial") {
    return serialTarget(machine.id);
  }
  if (
    machine.kind === "dockerContainer" &&
    machine.parentMachineId &&
    machine.containerId
  ) {
    return dockerContainerTarget({
      containerId: machine.containerId,
      containerName: machine.containerName,
      hostId: machine.parentMachineId,
      runtime: machine.runtime,
      user: machine.user,
      workdir: machine.workdir,
    });
  }
  return undefined;
}
