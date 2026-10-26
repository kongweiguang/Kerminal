// @author kongweiguang

import { useState } from "react";
import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MachineSidebar } from "../../../../src/features/machine-sidebar/MachineSidebar";
import { localSidebarGroups, remoteSidebarGroups } from "../../support/machine-sidebar/MachineSidebar.testSupport";

describe("MachineSidebar", () => {
  afterEach(() => {
    document.documentElement.classList.remove("dark");
  });

  it("renders grouped machines and highlights the selected machine", () => {
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
      screen.getByRole("complementary", { name: "主机侧边栏" }),
    ).toBeInTheDocument();
    expect(screen.getByText("默认分组")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /PowerShell/i }),
    ).toHaveAttribute("aria-pressed", "true");
    expect(screen.queryByText("虚拟机")).not.toBeInTheDocument();
  });

  it("keeps host search and common controls in one row without view tabs", () => {
    render(
      <MachineSidebar
        groups={remoteSidebarGroups}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId="ubuntu-dev"
      />,
    );

    const search = screen.getByRole("textbox", { name: "搜索主机" });
    const header = search.closest("[data-tauri-drag-region]");
    expect(header).toHaveClass("items-center");
    expect(header).toContainElement(
      screen.getByRole("button", { name: "打开 SFTP 传输工作台" }),
    );
    expect(header).toContainElement(
      screen.getByRole("button", { name: "折叠所有分组" }),
    );
    expect(screen.queryByRole("button", { name: "主机" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "容器" })).not.toBeInTheDocument();
  });

  it("shows local machines as unopened until a terminal session is open", () => {
    const { rerender } = render(
      <MachineSidebar
        groups={localSidebarGroups}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId="local-powershell"
      />,
    );

    expect(screen.getByTitle("未打开会话")).toBeInTheDocument();

    rerender(
      <MachineSidebar
        groups={localSidebarGroups}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        openMachineIds={["local-powershell"]}
        search=""
        selectedMachineId="local-powershell"
      />,
    );

    expect(screen.getByTitle("已打开会话")).toBeInTheDocument();
  });

  it("collapses and expands all groups from the header action", async () => {
    const user = userEvent.setup();

    render(
      <MachineSidebar
        groups={remoteSidebarGroups}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId="ubuntu-dev"
      />,
    );

    expect(screen.getByRole("button", { name: /PowerShell/i })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /ubuntu-dev/i })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "折叠所有分组" }));

    expect(
      screen.queryByRole("button", { name: /PowerShell/i }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /ubuntu-dev/i }),
    ).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "展开所有分组" }));

    expect(screen.getByRole("button", { name: /PowerShell/i })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /ubuntu-dev/i })).toBeInTheDocument();
  });

  it("marks the expanded sidebar header as a native drag region", () => {
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
      screen.getByRole("textbox", { name: "搜索主机" }).closest("[data-tauri-drag-region]"),
    ).toHaveAttribute("data-tauri-drag-region");
    expect(
      screen.getByLabelText("搜索主机").closest("label"),
    ).not.toHaveAttribute("data-tauri-drag-region");
  });

  it("filters machines through the search input", async () => {
    const user = userEvent.setup();
    const onSearchChange = vi.fn();

    function ControlledSidebar() {
      const [search, setSearch] = useState("");

      return (
        <MachineSidebar
          groups={localSidebarGroups}
          onSearchChange={(query) => {
            setSearch(query);
            onSearchChange(query);
          }}
          onSelectMachine={vi.fn()}
          search={search}
          selectedMachineId="local-powershell"
        />
      );
    }

    render(<ControlledSidebar />);

    await user.type(screen.getByLabelText("搜索主机"), "Power");

    expect(onSearchChange).toHaveBeenLastCalledWith("Power");
  });

  it("calls selection handler when a machine is chosen", async () => {
    const user = userEvent.setup();
    const onSelectMachine = vi.fn();

    render(
      <MachineSidebar
        groups={localSidebarGroups}
        onSearchChange={vi.fn()}
        onSelectMachine={onSelectMachine}
        search=""
        selectedMachineId="local-powershell"
      />,
    );

    await user.click(screen.getByRole("button", { name: /PowerShell/i }));

    expect(onSelectMachine).toHaveBeenCalledWith("local-powershell");
  });

  it("dispatches every local machine double click to open a terminal", async () => {
    const user = userEvent.setup();
    const onOpenLocalTerminal = vi.fn();
    const onSelectMachine = vi.fn();

    render(
      <MachineSidebar
        groups={localSidebarGroups}
        onOpenLocalTerminal={onOpenLocalTerminal}
        onSearchChange={vi.fn()}
        onSelectMachine={onSelectMachine}
        search=""
        selectedMachineId=""
      />,
    );

    const localMachine = screen.getByRole("button", { name: /PowerShell/i });
    await user.dblClick(localMachine);
    await user.dblClick(localMachine);

    expect(onSelectMachine).toHaveBeenCalledWith("local-powershell");
    expect(onOpenLocalTerminal).toHaveBeenCalledTimes(2);
    expect(onOpenLocalTerminal).toHaveBeenNthCalledWith(1, "local-powershell");
    expect(onOpenLocalTerminal).toHaveBeenNthCalledWith(2, "local-powershell");
  });

  it("opens an SFTP transfer tab on SFTP host double click", async () => {
    const user = userEvent.setup();
    const onOpenSftpTransferWorkbench = vi.fn();
    const onSelectMachine = vi.fn();

    render(
      <MachineSidebar
        groups={[
          {
            id: "files",
            machines: [
              {
                authType: "agent",
                description: "SFTP · upload@files.internal:22",
                host: "files.internal",
                id: "sftp-1",
                kind: "sftp",
                name: "files-only",
                port: 22,
                status: "offline",
                tags: ["files"],
                target: { hostId: "sftp-1", kind: "ssh" },
                username: "upload",
              },
            ],
            title: "文件主机",
          },
        ]}
        onOpenSftpTransferWorkbench={onOpenSftpTransferWorkbench}
        onSearchChange={vi.fn()}
        onSelectMachine={onSelectMachine}
        search=""
        selectedMachineId=""
      />,
    );

    await user.dblClick(screen.getByRole("button", { name: /files-only/i }));

    expect(onSelectMachine).toHaveBeenCalledWith("sftp-1");
    expect(onOpenSftpTransferWorkbench).toHaveBeenCalledWith("sftp-1");
  });

  it("offers SFTP-only file tools and a separate transfer tab action", async () => {
    const user = userEvent.setup();
    const onOpenSftp = vi.fn();
    const onOpenSftpTransferWorkbench = vi.fn();
    const groups = [
      {
        id: "files",
        machines: [
          {
            authType: "agent" as const,
            description: "SFTP · upload@files.internal:22",
            host: "files.internal",
            id: "sftp-1",
            kind: "sftp" as const,
            name: "files-only",
            port: 22,
            status: "offline" as const,
            tags: ["files"],
            target: { hostId: "sftp-1", kind: "ssh" as const },
            username: "upload",
          },
        ],
        title: "文件主机",
      },
    ];

    render(
      <MachineSidebar
        groups={groups}
        onOpenSftp={onOpenSftp}
        onOpenSftpTransferWorkbench={onOpenSftpTransferWorkbench}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId="sftp-1"
      />,
    );

    fireEvent.contextMenu(screen.getByRole("button", { name: /files-only/i }));
    await user.click(screen.getByRole("menuitem", { name: "打开文件传输" }));
    expect(onOpenSftp).toHaveBeenCalledWith("sftp-1");

    fireEvent.contextMenu(screen.getByRole("button", { name: /files-only/i }));
    await user.click(screen.getByRole("menuitem", { name: "新建传输 Tab" }));
    expect(onOpenSftpTransferWorkbench).toHaveBeenCalledWith("sftp-1");
    expect(screen.queryByRole("menuitem", { name: /终端|容器/ })).not.toBeInTheDocument();
  });

  it("opens settings from the lower-left control", async () => {
    const user = userEvent.setup();
    const onOpenSettings = vi.fn();

    render(
      <MachineSidebar
        groups={localSidebarGroups}
        onOpenSettings={onOpenSettings}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId="local-powershell"
        settingsSelected
      />,
    );

    const settingsButton = screen.getByRole("button", { name: "打开设置" });
    expect(settingsButton).toHaveAttribute("aria-pressed", "true");

    await user.click(settingsButton);

    expect(onOpenSettings).toHaveBeenCalled();
  });

  it("opens the unified connection dialog from the lower-left add action", async () => {
    const user = userEvent.setup();
    const onAddConnection = vi.fn();

    render(
      <MachineSidebar
        groups={localSidebarGroups}
        onAddConnection={onAddConnection}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId="local-powershell"
      />,
    );

    await user.click(screen.getByRole("button", { name: "添加连接" }));

    expect(onAddConnection).toHaveBeenCalledWith({ mode: "ssh" });
    expect(
      screen.queryByRole("menu", { name: "添加终端或主机" }),
    ).not.toBeInTheDocument();
  });

  it("collapses all machines behind one host entry and opens them in a flyout", async () => {
    const user = userEvent.setup();

    render(
      <MachineSidebar
        collapsed
        groups={remoteSidebarGroups}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId="ubuntu-dev"
      />,
    );

    expect(
      screen.getByRole("button", { name: "打开主机列表" }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /PowerShell/i }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /ubuntu-dev/i }),
    ).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "打开主机列表" }));

    expect(screen.getByRole("dialog", { name: "主机列表" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /PowerShell/i })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /ubuntu-dev/i })).toBeInTheDocument();

    await user.type(screen.getByLabelText("搜索主机列表"), "ubuntu");

    expect(screen.getByRole("button", { name: /ubuntu-dev/i })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /PowerShell/i })).toBeNull();
  });

  it("opens the SFTP transfer workbench from expanded and collapsed sidebar actions", async () => {
    const user = userEvent.setup();
    const onOpenTransferWorkbench = vi.fn();
    const { rerender } = render(
      <MachineSidebar
        groups={remoteSidebarGroups}
        onOpenTransferWorkbench={onOpenTransferWorkbench}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId="ubuntu-dev"
      />,
    );

    await user.click(
      screen.getByRole("button", { name: "打开 SFTP 传输工作台" }),
    );

    expect(onOpenTransferWorkbench).toHaveBeenCalledTimes(1);

    rerender(
      <MachineSidebar
        collapsed
        groups={remoteSidebarGroups}
        onOpenTransferWorkbench={onOpenTransferWorkbench}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId="ubuntu-dev"
      />,
    );

    await user.click(
      screen.getByRole("button", { name: "打开 SFTP 传输工作台" }),
    );

    expect(onOpenTransferWorkbench).toHaveBeenCalledTimes(2);
  });

  it("keeps settings out of the root context menu", () => {
    render(
      <MachineSidebar
        groups={localSidebarGroups}
        onOpenSettings={vi.fn()}
        onSearchChange={vi.fn()}
        onSelectMachine={vi.fn()}
        search=""
        selectedMachineId="local-powershell"
      />,
    );

    fireEvent.contextMenu(
      screen.getByRole("complementary", { name: "主机侧边栏" }),
    );

    expect(
      screen.queryByRole("menuitem", { name: "打开设置" }),
    ).not.toBeInTheDocument();
  });

  it("keeps the right-click menu close to the pointer near the viewport bottom", () => {
    const originalGetBoundingClientRect =
      HTMLElement.prototype.getBoundingClientRect;
    const originalInnerWidth = window.innerWidth;
    const originalInnerHeight = window.innerHeight;

    Object.defineProperty(window, "innerWidth", {
      configurable: true,
      value: 360,
      writable: true,
    });
    Object.defineProperty(window, "innerHeight", {
      configurable: true,
      value: 300,
      writable: true,
    });
    HTMLElement.prototype.getBoundingClientRect = function () {
      if (this.getAttribute("role") === "menu") {
        return {
          bottom: 76,
          height: 76,
          left: 0,
          right: 224,
          top: 0,
          width: 224,
          x: 0,
          y: 0,
          toJSON: () => ({}),
        } as DOMRect;
      }

      return originalGetBoundingClientRect.call(this);
    };

    try {
      render(
        <MachineSidebar
          groups={localSidebarGroups}
          onSearchChange={vi.fn()}
          onSelectMachine={vi.fn()}
          search=""
          selectedMachineId="local-powershell"
        />,
      );

      fireEvent.contextMenu(
        screen.getByRole("complementary", { name: "主机侧边栏" }),
        {
          clientX: 100,
          clientY: 260,
        },
      );

      const menu = screen.getByRole("menu", { name: "主机操作菜单" });

      expect(menu.style.left).toBe("100px");
      expect(menu.style.top).toBe("216px");
    } finally {
      HTMLElement.prototype.getBoundingClientRect =
        originalGetBoundingClientRect;
      Object.defineProperty(window, "innerWidth", {
        configurable: true,
        value: originalInnerWidth,
        writable: true,
      });
      Object.defineProperty(window, "innerHeight", {
        configurable: true,
        value: originalInnerHeight,
        writable: true,
      });
    }
  });


});
