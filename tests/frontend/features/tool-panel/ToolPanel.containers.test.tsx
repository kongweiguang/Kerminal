// @author kongweiguang

import "../../support/tool-panel/ToolPanel.testSupport";
import { render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { HostContainersToolContentProps } from "../../../../src/features/machine-sidebar/HostContainersToolContent";
import { ToolPanel } from "../../../../src/features/tool-panel/ToolPanel";
import { tools } from "../../../../src/features/workspace/workspaceData";
import { focusedSshPane, secondarySshMachine, sshMachine, sshTerminalTab } from "../../support/tool-panel/ToolPanel.testSupport";

const probe = vi.hoisted(() => ({ props: undefined as HostContainersToolContentProps | undefined }));

vi.mock("../../../../src/features/machine-sidebar/HostContainersToolContent", () => ({
  /** 此处只替换容器 I/O 视图，保留真实 ToolPanel 的懒加载、绑定和可见性接线。 */
  HostContainersToolContent: (props: HostContainersToolContentProps) => {
    probe.props = props;
    return <div data-testid="container-content-probe" />;
  },
}));

describe("ToolPanel containers", () => {
  it("绑定聚焦终端而非左栏选择，并透传操作", async () => {
    const onEnterContainer = vi.fn();
    render(<ToolPanel activeTool="containers" activeMachine={sshMachine} activeTab={sshTerminalTab} focusedPane={focusedSshPane}
      selectedMachine={secondarySshMachine} containerActions={{ onEnterContainer }} onActiveToolChange={vi.fn()} tools={tools} />);
    await screen.findByTestId("container-content-probe");
    expect(probe.props?.selectedMachine).toBe(sshMachine);
    expect(probe.props?.active).toBe(true);
    expect(probe.props?.onEnterContainer).toBe(onEnterContainer);
    expect(screen.getByRole("button", { name: "收起 容器" })).toHaveAttribute("aria-pressed", "true");
  });

  it("容器终端解析宿主机并传递 Podman 运行时", async () => {
    render(<ToolPanel activeTool="containers" activeTab={sshTerminalTab}
      activeMachine={{ ...sshMachine, id: "podman:host:api", kind: "dockerContainer", parentMachineId: sshMachine.id, runtime: "podman" }}
      focusedPane={{ ...focusedSshPane, mode: "container", target: { kind: "dockerContainer", hostId: sshMachine.id, containerId: "api", runtime: "podman" } }}
      machineGroups={[{ id: "hosts", title: "Hosts", machines: [sshMachine, secondarySshMachine] }]}
      selectedMachine={secondarySshMachine} onActiveToolChange={vi.fn()} tools={tools} />);
    await screen.findByTestId("container-content-probe");
    expect(probe.props?.selectedMachine).toBe(sshMachine);
    expect(probe.props?.initialRuntime).toBe("podman");
  });

  it("定位请求仅用于匹配 Tab 和主机，收起后通知内容停止读取", async () => {
    const props = { activeTool: "containers" as const, activeMachine: sshMachine, activeTab: sshTerminalTab, focusedPane: focusedSshPane, onActiveToolChange: vi.fn(), tools };
    const view = render(<ToolPanel {...props} containerRevealRequest={{ tabId: "other-tab", hostId: sshMachine.id, containerId: "wrong", runtime: "podman", requestId: 1 }} />);
    await screen.findByTestId("container-content-probe");
    expect(probe.props?.initialContainerId).toBeUndefined();
    view.rerender(<ToolPanel {...props} visible={false} containerRevealRequest={{ tabId: sshTerminalTab.id, hostId: sshMachine.id, containerId: "api", runtime: "podman", requestId: 2 }} />);
    await waitFor(() => expect(probe.props?.active).toBe(false));
    expect(probe.props?.initialContainerId).toBe("api");
    expect(probe.props?.initialRuntime).toBe("podman");
    expect(probe.props?.initialRequestId).toBe(2);
  });
});
