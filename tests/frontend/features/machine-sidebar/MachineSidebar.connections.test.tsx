import { fireEvent, render, screen } from "@testing-library/react";
// @author kongweiguang

import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MachineSidebar } from "../../../../src/features/machine-sidebar/MachineSidebar";
import { containerSidebarGroups, localSidebarGroups, rdpSidebarGroups, remoteSidebarGroups } from "../../support/machine-sidebar/MachineSidebar.testSupport";

describe("MachineSidebar", () => {
  afterEach(() => {
    document.documentElement.classList.remove("dark");
  });

  it("opens an RDP connection when a saved RDP host is double-clicked", () => {
    const onOpenRdpConnection = vi.fn();
    const onOpenSshTerminal = vi.fn();
    const onSelectMachine = vi.fn();

    render(
      <MachineSidebar
        groups={rdpSidebarGroups}
        onOpenRdpConnection={onOpenRdpConnection}
        onOpenSshTerminal={onOpenSshTerminal}
        onSearchChange={vi.fn()}
        onSelectMachine={onSelectMachine}
        search=""
        selectedMachineId="rdp-office"
      />,
    );

    fireEvent.doubleClick(screen.getByRole("button", { name: /office-rdp/i }));

    expect(onSelectMachine).toHaveBeenCalledWith("rdp-office");
    expect(onOpenRdpConnection).toHaveBeenCalledWith("rdp-office");
    expect(onOpenSshTerminal).not.toHaveBeenCalled();
  });

  it("shows RDP opening feedback and disables the repeated context-menu action", () => {
    const onOpenRdpConnection = vi.fn();

    render(
      <MachineSidebar
        groups={rdpSidebarGroups}
        onOpenRdpConnection={onOpenRdpConnection}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        rdpOpeningMachineIds={["rdp-office"]}
        search=""
        selectedMachineId="rdp-office"
      />,
    );

    const hostButton = screen.getByRole("button", { name: /office-rdp/i });
    expect(hostButton).toHaveAttribute("aria-busy", "true");
    expect(hostButton).toHaveTextContent("正在打开远程桌面...");

    fireEvent.doubleClick(hostButton);
    expect(onOpenRdpConnection).not.toHaveBeenCalled();

    fireEvent.contextMenu(hostButton);
    expect(
      screen.getByRole("menuitem", { name: "正在打开 RDP..." }),
    ).toBeDisabled();
  });

  it("shows RDP opening feedback in the collapsed host list", async () => {
    const user = userEvent.setup();

    render(
      <MachineSidebar
        collapsed
        groups={rdpSidebarGroups}
        onOpenRdpConnection={vi.fn()}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        rdpOpeningMachineIds={["rdp-office"]}
        search=""
        selectedMachineId="rdp-office"
      />,
    );

    await user.click(screen.getByRole("button", { name: "打开主机列表" }));

    const hostButton = screen.getByRole("button", { name: /office-rdp/i });
    expect(hostButton).toHaveAttribute("aria-busy", "true");
    expect(hostButton).toHaveTextContent("正在打开远程桌面...");
  });

  it("opens RDP machine actions from the right-click menu", async () => {
    const user = userEvent.setup();
    const onDeleteMachine = vi.fn();
    const onDuplicateMachine = vi.fn();
    const onEditMachine = vi.fn();
    const onOpenRdpConnection = vi.fn();

    render(
      <MachineSidebar
        groups={rdpSidebarGroups}
        onDeleteMachine={onDeleteMachine}
        onDuplicateMachine={onDuplicateMachine}
        onEditMachine={onEditMachine}
        onOpenRdpConnection={onOpenRdpConnection}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId="rdp-office"
      />,
    );

    fireEvent.contextMenu(screen.getByRole("button", { name: /office-rdp/i }));
    await user.click(screen.getByRole("menuitem", { name: "打开 RDP 连接" }));

    expect(onOpenRdpConnection).toHaveBeenCalledWith("rdp-office");

    fireEvent.contextMenu(screen.getByRole("button", { name: /office-rdp/i }));
    await user.click(screen.getByRole("menuitem", { name: "编辑连接配置" }));

    expect(onEditMachine).toHaveBeenCalledWith("rdp-office");

    fireEvent.contextMenu(screen.getByRole("button", { name: /office-rdp/i }));
    await user.click(screen.getByRole("menuitem", { name: "复制主机" }));

    expect(onDuplicateMachine).toHaveBeenCalledWith("rdp-office");

    fireEvent.contextMenu(screen.getByRole("button", { name: /office-rdp/i }));
    await user.click(screen.getByRole("menuitem", { name: "删除连接" }));

    expect(onDeleteMachine).toHaveBeenCalledWith("rdp-office");
  });

  it("opens an existing local machine when it is double-clicked", () => {
    const onOpenLocalTerminal = vi.fn();
    const onSelectMachine = vi.fn();

    render(
      <MachineSidebar
        groups={localSidebarGroups}
        onOpenLocalTerminal={onOpenLocalTerminal}
        onSearchChange={vi.fn()}
        onSelectMachine={onSelectMachine}
        search=""
        selectedMachineId="local-powershell"
      />,
    );

    fireEvent.doubleClick(screen.getByRole("button", { name: /PowerShell/i }));

    expect(onSelectMachine).toHaveBeenCalledWith("local-powershell");
    expect(onOpenLocalTerminal).toHaveBeenCalledWith("local-powershell");
  });

  it("opens a Docker container when it is clicked", async () => {
    const user = userEvent.setup();
    const onOpenContainerTerminal = vi.fn();
    const onSelectMachine = vi.fn();

    render(
      <MachineSidebar
        groups={containerSidebarGroups}
        onOpenContainerTerminal={onOpenContainerTerminal}
        onSearchChange={vi.fn()}
        onSelectMachine={onSelectMachine}
        search=""
        selectedMachineId="ubuntu-dev"
      />,
    );

    await user.click(screen.getByRole("button", { name: /api/i }));

    expect(onSelectMachine).toHaveBeenCalledWith(
      "docker:ubuntu-dev:c0ffee1234567890",
    );
    expect(onOpenContainerTerminal).toHaveBeenCalledWith(
      "docker:ubuntu-dev:c0ffee1234567890",
    );
  });

  it("does not offer duplicate for discovered Docker container cards", () => {
    render(
      <MachineSidebar
        groups={containerSidebarGroups}
        onDuplicateMachine={vi.fn()}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId="docker:ubuntu-dev:c0ffee1234567890"
      />,
    );

    fireEvent.contextMenu(screen.getByRole("button", { name: /api/i }));

    expect(
      screen.queryByRole("menuitem", { name: "复制主机" }),
    ).not.toBeInTheDocument();
  });

  it("offers Docker SFTP without the cross-host transfer tab action", () => {
    render(
      <MachineSidebar
        groups={containerSidebarGroups}
        onOpenContainerDetails={vi.fn()}
        onOpenSftp={vi.fn()}
        onOpenSftpTransferWorkbench={vi.fn()}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId="docker:ubuntu-dev:c0ffee1234567890"
      />,
    );

    fireEvent.contextMenu(screen.getByRole("button", { name: /api/i }));

    expect(screen.getByRole("menuitem", { name: "详情" })).toBeInTheDocument();
    expect(screen.getByRole("menuitem", { name: "打开 SFTP" })).toBeInTheDocument();
    expect(screen.queryByRole("menuitem", { name: "容器" })).not.toBeInTheDocument();
    expect(
      screen.queryByRole("menuitem", { name: "新建传输 Tab" }),
    ).not.toBeInTheDocument();
  });

  it("opens Docker container details from the right-click menu", async () => {
    const user = userEvent.setup();
    const onOpenContainerDetails = vi.fn();

    render(
      <MachineSidebar
        groups={containerSidebarGroups}
        onOpenContainerDetails={onOpenContainerDetails}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId="docker:ubuntu-dev:c0ffee1234567890"
      />,
    );

    fireEvent.contextMenu(screen.getByRole("button", { name: /api/i }));
    await user.click(screen.getByRole("menuitem", { name: "详情" }));

    expect(onOpenContainerDetails).toHaveBeenCalledWith(
      "docker:ubuntu-dev:c0ffee1234567890",
    );
  });

  it("opens local machine actions without creating a duplicate local session", async () => {
    const user = userEvent.setup();
    const onAddMachine = vi.fn();
    const onDeleteMachine = vi.fn();
    const onDuplicateMachine = vi.fn();
    const onEditMachine = vi.fn();
    const onOpenLocalTerminal = vi.fn();

    render(
      <MachineSidebar
        groups={localSidebarGroups}
        onAddMachine={onAddMachine}
        onDeleteMachine={onDeleteMachine}
        onDuplicateMachine={onDuplicateMachine}
        onEditMachine={onEditMachine}
        onOpenLocalTerminal={onOpenLocalTerminal}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId="local-powershell"
      />,
    );

    fireEvent.contextMenu(screen.getByRole("button", { name: /PowerShell/i }));
    await user.click(screen.getByRole("menuitem", { name: "打开本地会话" }));

    expect(onOpenLocalTerminal).toHaveBeenCalledWith("local-powershell");

    fireEvent.contextMenu(screen.getByRole("button", { name: /PowerShell/i }));
    await user.click(screen.getByRole("menuitem", { name: "编辑连接配置" }));

    expect(onEditMachine).toHaveBeenCalledWith("local-powershell");

    fireEvent.contextMenu(screen.getByRole("button", { name: /PowerShell/i }));
    await user.click(screen.getByRole("menuitem", { name: "复制主机" }));

    expect(onDuplicateMachine).toHaveBeenCalledWith("local-powershell");

    fireEvent.contextMenu(screen.getByRole("button", { name: /PowerShell/i }));
    await user.click(screen.getByRole("menuitem", { name: "添加同组连接" }));

    expect(onAddMachine).toHaveBeenCalledWith("__ungrouped__");

    fireEvent.contextMenu(screen.getByRole("button", { name: /PowerShell/i }));
    await user.click(screen.getByRole("menuitem", { name: "删除连接" }));

    expect(onDeleteMachine).toHaveBeenCalledWith("local-powershell");
  });

  it("lights a saved host status dot when it has an open session", () => {
    render(
      <MachineSidebar
        groups={remoteSidebarGroups}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        openMachineIds={["ubuntu-dev"]}
        search=""
        selectedMachineId="ubuntu-dev"
      />,
    );

    const hostButton = screen.getByRole("button", { name: /ubuntu-dev/i });
    expect(hostButton.querySelector(".bg-emerald-400")).not.toBeNull();
    expect(hostButton.querySelector(".bg-zinc-500")).toBeNull();
  });

  it("does not render the deprecated local terminal header button", () => {
    render(
      <MachineSidebar
        groups={localSidebarGroups}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId="local-powershell"
      />,
    );

    expect(
      screen.queryByRole("button", { name: "本地终端" }),
    ).not.toBeInTheDocument();
  });

  it("shows an empty state when search removes every machine", () => {
    render(
      <MachineSidebar
        groups={localSidebarGroups}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search="no-such-machine"
        selectedMachineId="local-powershell"
      />,
    );

    expect(screen.getByText("没有结果")).toBeInTheDocument();
  });

  it("keeps empty remote groups visible before search is applied", () => {
    render(
      <MachineSidebar
        groups={[
          {
            id: "empty-group",
            machines: [],
            title: "云服务器",
          },
        ]}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId=""
      />,
    );

    expect(screen.getByText("云服务器")).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "添加 SSH 连接" }),
    ).not.toBeInTheDocument();
  });

  it("keeps the search toolbar while connections are empty", async () => {
    const user = userEvent.setup();
    const onAddConnection = vi.fn();

    render(
      <MachineSidebar
        groups={[]}
        onAddConnection={onAddConnection}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId=""
      />,
    );

    expect(screen.getByLabelText("搜索主机")).toBeDisabled();
    expect(screen.getByText("暂无连接")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "添加连接" }));

    expect(onAddConnection).toHaveBeenCalledWith({ mode: "ssh" });
    expect(screen.getAllByRole("button", { name: "添加连接" })).toHaveLength(1);
  });

});
