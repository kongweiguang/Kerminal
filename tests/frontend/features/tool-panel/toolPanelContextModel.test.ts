// @author kongweiguang

import { describe, expect, it } from "vitest";
import {
  resolveToolPanelBinding,
  toolPanelBindingScopes,
} from "../../../../src/features/tool-panel/toolPanelContextModel";
import type {
  Machine,
  TerminalPane,
  TerminalTab,
} from "../../../../src/features/workspace/types";

const hostA: Machine = {
  description: "Host A",
  id: "host-a",
  kind: "ssh",
  name: "Host A",
  status: "online",
  tags: [],
};
const hostB: Machine = {
  description: "Host B",
  id: "host-b",
  kind: "ssh",
  name: "Host B",
  status: "online",
  tags: [],
};
const paneA: TerminalPane = {
  id: "pane-a",
  lines: [],
  machineId: hostA.id,
  mode: "ssh",
  prompt: "$",
  remoteHostId: hostA.id,
  status: "online",
  title: "Host A",
};
const tabA: TerminalTab = {
  id: "tab-a",
  layout: { paneId: paneA.id, type: "pane" },
  machineId: hostA.id,
  title: "Host A",
};

describe("toolPanelContextModel", () => {
  it("容器工具从当前容器 pane 解析宿主机，而不借用侧栏选择", () => {
    const binding = resolveToolPanelBinding("containers", {
      activeMachine: { ...hostA, id: "podman:host-a:api", kind: "dockerContainer", parentMachineId: hostA.id },
      activeTab: tabA,
      focusedPane: { ...paneA, mode: "container", target: { kind: "dockerContainer", hostId: hostA.id, containerId: "api", runtime: "podman" } },
      machineGroups: [{ machines: [hostA, hostB] }],
      selectedMachine: hostB,
    });
    expect(binding.machine).toBe(hostA);
    expect(binding.resourceKey).toBe("host:host-a");
    expect(binding.target).toEqual({ kind: "ssh", hostId: hostA.id });
  });

  it("容器宿主机不存在时关闭绑定，不回退其它主机", () => {
    const binding = resolveToolPanelBinding("containers", {
      activeMachine: { ...hostA, kind: "dockerContainer" },
      focusedPane: { ...paneA, mode: "container", target: { kind: "dockerContainer", hostId: "deleted-host", containerId: "api" } },
      selectedMachine: hostB,
      machineGroups: [{ machines: [hostB] }],
    });
    expect(binding.machine).toBeUndefined();
    expect(binding.target).toBeUndefined();
    expect(binding.resourceKey).toContain("unbound");
  });

  it("无活动目标或当前为本地、SFTP-only 时不发出容器主机绑定", () => {
    expect(resolveToolPanelBinding("containers", { selectedMachine: hostA }).machine).toBeUndefined();
    for (const machine of [{ ...hostA, kind: "local" as const }, { ...hostA, kind: "sftp" as const, target: { kind: "ssh" as const, hostId: hostA.id } }]) {
      expect(resolveToolPanelBinding("containers", { activeMachine: machine, selectedMachine: hostB }).machine).toBeUndefined();
    }
  });

  it("分屏切换使用新的聚焦 pane，忽略侧栏选择", () => {
    const first = resolveToolPanelBinding("containers", {
      activeMachine: hostA,
      activeTab: tabA,
      focusedPane: paneA,
      selectedMachine: hostB,
    });
    const second = resolveToolPanelBinding("containers", {
      activeMachine: hostB,
      activeTab: tabA,
      focusedPane: {
        ...paneA,
        id: "pane-b",
        machineId: hostB.id,
        remoteHostId: hostB.id,
      },
      selectedMachine: hostA,
    });
    expect(first.machine).toBe(hostA);
    expect(second.machine).toBe(hostB);
    expect(second.bindingKey).not.toBe(first.bindingKey);
  });

  it("SSH 文件标签通过自身目标解析容器宿主机", () => {
    const binding = resolveToolPanelBinding("containers", {
      activeTab: {
        access: "readonly",
        id: "compose-file",
        kind: "workspaceFile",
        machineId: hostA.id,
        path: "/srv/compose.yaml",
        source: "composeYaml",
        target: { hostId: hostA.id, kind: "ssh" },
        title: "compose.yaml",
      },
      machineGroups: [{ machines: [hostA, hostB] }],
      selectedMachine: hostB,
    });
    expect(binding.machine).toBe(hostA);
    expect(binding.source).toBe("activeTab");
  });

  it("为全部右栏能力声明唯一所有者层级", () => {
    expect(toolPanelBindingScopes).toEqual({
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
    });
  });

  it("当前 pane 优先于侧栏选择，避免把 tmux 请求发往另一台主机", () => {
    const binding = resolveToolPanelBinding("tmux", {
      activeMachine: hostA,
      activeTab: tabA,
      focusedPane: paneA,
      selectedMachine: hostB,
    });

    expect(binding).toMatchObject({
      machine: hostA,
      resourceKey: "ssh:host-a",
      source: "focusedPane",
      target: { hostId: "host-a", kind: "ssh" },
    });
    expect(binding.bindingKey).toContain("pane-a");
    expect(binding.bindingKey).not.toContain("host-b");
  });

  it("系统监控在没有活动 tab 时不绑定侧栏残留主机", () => {
    const binding = resolveToolPanelBinding("system", {
      selectedMachine: hostB,
    });

    expect(binding.source).toBe("none");
    expect(binding.machine).toBeUndefined();
    expect(binding.target).toBeUndefined();
    expect(binding.resourceKey).toBe("target:unbound:none");
  });

  it("pane 级能力不会回退到无关侧栏主机", () => {
    const binding = resolveToolPanelBinding("logs", {
      selectedMachine: hostB,
    });

    expect(binding.source).toBe("none");
    expect(binding.machine).toBeUndefined();
    expect(binding.resourceKey).toBe("pane:unbound");
  });

  it("workspace file tab 使用 tab 自带 target，而不是侧栏选择", () => {
    const workspaceFileTab: TerminalTab = {
      access: "readonly",
      id: "file-a",
      kind: "workspaceFile",
      machineId: hostA.id,
      path: "/srv/a/app.log",
      source: "sftp",
      target: { hostId: hostA.id, kind: "ssh" },
      title: "app.log",
    };
    const binding = resolveToolPanelBinding("sftp", {
      activeMachine: hostA,
      activeTab: workspaceFileTab,
      selectedMachine: hostB,
    });

    expect(binding.source).toBe("activeTab");
    expect(binding.resourceKey).toBe("ssh:host-a");
  });

  it("端口能力不会从当前容器静默跳到侧栏 SSH 主机", () => {
    const container: Machine = {
      containerId: "container-a",
      description: "api",
      id: "docker:host-a:container-a",
      kind: "dockerContainer",
      name: "api",
      parentMachineId: hostA.id,
      runtime: "docker",
      status: "online",
      tags: [],
    };
    const containerPane: TerminalPane = {
      containerId: "container-a",
      id: "pane-container",
      lines: [],
      machineId: container.id,
      mode: "container",
      prompt: "#",
      remoteHostId: hostA.id,
      status: "online",
      target: {
        containerId: "container-a",
        hostId: hostA.id,
        kind: "dockerContainer",
      },
      title: "api",
    };
    const binding = resolveToolPanelBinding("ports", {
      activeMachine: container,
      activeTab: tabA,
      focusedPane: containerPane,
      selectedMachine: hostB,
    });

    expect(binding.machine?.kind).toBe("dockerContainer");
    expect(binding.source).toBe("focusedPane");
    expect(binding.resourceKey).toContain("host:unbound");
  });

  it("workspace revision 和 pane 身份分别驱动对应 binding 变化", () => {
    const workspaceOne = resolveToolPanelBinding("context", {
      workspaceRevision: 1,
    });
    const workspaceTwo = resolveToolPanelBinding("context", {
      workspaceRevision: 2,
    });
    const paneOne = resolveToolPanelBinding("snippets", {
      activeMachine: hostA,
      activeTab: tabA,
      focusedPane: paneA,
    });
    const paneTwo = resolveToolPanelBinding("snippets", {
      activeMachine: hostA,
      activeTab: tabA,
      focusedPane: { ...paneA, id: "pane-a-2" },
    });

    expect(workspaceOne.bindingKey).not.toBe(workspaceTwo.bindingKey);
    expect(paneOne.bindingKey).not.toBe(paneTwo.bindingKey);
  });
});
