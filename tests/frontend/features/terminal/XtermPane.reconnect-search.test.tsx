// @author kongweiguang
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { defaultAppSettings } from "../../../../src/features/settings/settingsModel";
import type { TerminalOutputEvent } from "../../../../src/lib/terminalApi";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { installClipboardMock, mocks } from "../../support/terminal/XtermPane.testSupport.tsx";
import { XtermPane } from "../../../../src/features/terminal/XtermPane";
import { consumeAgentSendRequest, getAgentSendRequestSnapshot } from "../../../../src/features/agent-workflow/agentSendRequestStore";

describe("XtermPane context menu search and logging", () => {
  beforeEach(() => {
    const pendingRequest = getAgentSendRequestSnapshot().request;
    if (pendingRequest) consumeAgentSendRequest(pendingRequest.id);
  });

  it("automatically reconnects after a session closes when enabled", async () => {
    let sequence = 0;
    mocks.api.createTerminalSession.mockImplementation(
      async (_request, onOutput) => {
        sequence += 1;
        const sessionId = `session-${sequence}`;
        mocks.setLatestOutputHandler(
          onOutput as (event: TerminalOutputEvent) => void,
        );
        return {
          cols: 80,
          id: sessionId,
          rows: 24,
          shell: "powershell.exe",
          shellIntegration: { reason: "test default", status: "disabled" },
          status: "running",
        };
      },
    );

    try {
      render(
        <XtermPane
          focused
          paneId="pane-local"
          resolvedTheme="dark"
          terminalAppearance={defaultAppSettings.terminal}
          title="本地 PowerShell"
        />,
      );

      await waitFor(() => {
        expect(screen.getByText("已连接")).toBeInTheDocument();
      });
      mocks.api.markTerminalSessionBindingDisconnected.mockClear();
      mocks.api.markTerminalSessionBindingReady.mockClear();
      mocks.api.registerTerminalSessionBinding.mockClear();

      vi.useFakeTimers();
      act(() => {
        mocks.getLatestOutputHandler()?.({
          data: "",
          kind: "closed",
          sessionId: "session-1",
        });
      });

      expect(screen.getByText("已结束")).toBeInTheDocument();
      expect(mocks.api.markTerminalSessionBindingDisconnected).toHaveBeenCalledWith(
        expect.objectContaining({
          paneId: "pane-local",
          sessionId: "session-1",
        }),
      );

      await act(async () => {
        await vi.advanceTimersByTimeAsync(3000);
        await Promise.resolve();
      });

      expect(mocks.api.createTerminalSession).toHaveBeenCalledTimes(2);
      expect(mocks.api.registerTerminalSessionBinding).toHaveBeenCalledWith(
        expect.objectContaining({
          paneId: "pane-local",
          sessionId: "session-2",
        }),
      );
      expect(mocks.api.markTerminalSessionBindingReady).toHaveBeenCalledWith(
        expect.objectContaining({
          paneId: "pane-local",
          sessionId: "session-2",
        }),
      );
    } finally {
      vi.useRealTimers();
    }
  });

  it("pastes clipboard text and exposes local terminal actions from the context menu", async () => {
    const user = userEvent.setup();
    const clipboard = installClipboardMock();
    const onOpenLogs = vi.fn();
    const onSplitPane = vi.fn();

    render(
      <XtermPane
        focused
        onOpenLogs={onOpenLogs}
        onSplitPane={onSplitPane}
        paneId="pane-local"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="本地 PowerShell"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    fireEvent.contextMenu(screen.getByLabelText("本地 PowerShell xterm 终端"));
    expect(
      screen.queryByRole("menuitem", { name: "打开设置" }),
    ).not.toBeInTheDocument();
    await user.click(screen.getByRole("menuitem", { name: /粘贴/ }));
    expect(mocks.api.readTerminalClipboardText).toHaveBeenCalled();
    expect(mocks.terminalInstances[0].paste).toHaveBeenCalledWith(
      "echo pasted\r",
    );
    expect(clipboard.readText).not.toHaveBeenCalled();

    fireEvent.contextMenu(screen.getByLabelText("本地 PowerShell xterm 终端"));
    await user.click(screen.getByRole("menuitem", { name: "全选" }));
    expect(mocks.terminalInstances[0].selectAll).toHaveBeenCalled();

    fireEvent.contextMenu(screen.getByLabelText("本地 PowerShell xterm 终端"));
    await user.click(screen.getByRole("menuitem", { name: "清屏" }));
    expect(mocks.terminalInstances[0].clear).not.toHaveBeenCalled();
    expect(mocks.api.writeTerminal).toHaveBeenCalledWith("session-1", "\x0c");
    expect(mocks.terminalInstances[0].write).not.toHaveBeenCalledWith(
      "\x1b[H\x1b[2J\x1b[3J",
      expect.any(Function),
    );

    fireEvent.contextMenu(screen.getByLabelText("本地 PowerShell xterm 终端"));
    await user.click(screen.getByRole("menuitem", { name: /搜索/ }));
    expect(screen.getByRole("form", { name: "终端搜索" })).toBeInTheDocument();

    fireEvent.contextMenu(screen.getByLabelText("本地 PowerShell xterm 终端"));
    expect(
      screen.queryByRole("menuitem", { name: "开始记录日志" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("menuitem", { name: "停止记录日志" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("menuitem", { name: "打开日志" }),
    ).not.toBeInTheDocument();
    expect(onOpenLogs).not.toHaveBeenCalled();

    fireEvent.contextMenu(screen.getByLabelText("本地 PowerShell xterm 终端"));
    expect(
      screen.queryByRole("menuitem", { name: "新建本地终端" }),
    ).not.toBeInTheDocument();

    fireEvent.contextMenu(screen.getByLabelText("本地 PowerShell xterm 终端"));
    await user.click(screen.getByRole("menuitem", { name: "左右分屏" }));
    fireEvent.contextMenu(screen.getByLabelText("本地 PowerShell xterm 终端"));
    await user.click(screen.getByRole("menuitem", { name: "上下分屏" }));
    expect(onSplitPane).toHaveBeenNthCalledWith(1, "horizontal");
    expect(onSplitPane).toHaveBeenNthCalledWith(2, "vertical");
  });

  it("searches the current terminal buffer with xterm search addon", async () => {
    const user = userEvent.setup();

    render(
      <XtermPane
        focused
        paneId="pane-local"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="本地 PowerShell"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    fireEvent.contextMenu(screen.getByLabelText("本地 PowerShell xterm 终端"));
    await user.click(screen.getByRole("menuitem", { name: /搜索/ }));
    await user.type(screen.getByLabelText("搜索终端缓冲区"), "hello");
    await user.click(screen.getByRole("button", { name: "下一个匹配" }));
    mocks.searchInstances[0].emitResults(2, 0);

    expect(mocks.searchInstances[0].findNext).toHaveBeenCalledWith(
      "hello",
      expect.objectContaining({ caseSensitive: false }),
    );
    expect(await screen.findByText("1/2")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "上一个匹配" }));
    expect(mocks.searchInstances[0].findPrevious).toHaveBeenCalledWith(
      "hello",
      expect.objectContaining({ caseSensitive: false }),
    );

    await user.click(screen.getByRole("button", { name: "区分大小写" }));
    await user.click(screen.getByRole("button", { name: "下一个匹配" }));
    expect(mocks.searchInstances[0].findNext).toHaveBeenLastCalledWith(
      "hello",
      expect.objectContaining({ caseSensitive: true }),
    );
  });

  it("clears search decorations when search closes or query is empty", async () => {
    const user = userEvent.setup();

    render(
      <XtermPane
        focused
        paneId="pane-local"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="本地 PowerShell"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    fireEvent.contextMenu(screen.getByLabelText("本地 PowerShell xterm 终端"));
    await user.click(screen.getByRole("menuitem", { name: /搜索/ }));
    const input = screen.getByLabelText("搜索终端缓冲区");
    await user.type(input, "hello");
    await user.clear(input);
    expect(mocks.searchInstances[0].clearDecorations).toHaveBeenCalled();

    await user.click(screen.getByRole("button", { name: "关闭搜索" }));
    expect(mocks.searchInstances[0].clearDecorations).toHaveBeenCalledTimes(2);
    expect(mocks.terminalInstances[0].focus).toHaveBeenCalled();
  });

  it("does not expose terminal session logging from the context menu", async () => {
    render(
      <XtermPane
        focused
        paneId="pane-local"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="本地 PowerShell"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    fireEvent.contextMenu(screen.getByLabelText("本地 PowerShell xterm 终端"));
    expect(
      screen.queryByRole("menuitem", { name: "开始记录日志" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("menuitem", { name: "停止记录日志" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("menuitem", { name: "打开日志" }),
    ).not.toBeInTheDocument();
    expect(mocks.api.startTerminalLog).not.toHaveBeenCalled();
    expect(mocks.api.stopTerminalLog).not.toHaveBeenCalled();
    expect(screen.queryByLabelText("终端日志记录状态")).not.toBeInTheDocument();
  });

  it("disconnects and reconnects a local terminal session from the context menu", async () => {
    const user = userEvent.setup();
    let sequence = 0;
    mocks.api.createTerminalSession.mockImplementation(
      async (_request, onOutput) => {
        sequence += 1;
        const sessionId = `session-${sequence}`;
        mocks.setLatestOutputHandler(
          onOutput as (event: TerminalOutputEvent) => void,
        );
        mocks.getLatestOutputHandler()?.({
          data: `hello from ${sessionId}`,
          kind: "data",
          sessionId,
        });
        return {
          cols: 80,
          id: sessionId,
          rows: 24,
          shell: "powershell.exe",
          shellIntegration: { reason: "test default", status: "disabled" },
          status: "running",
        };
      },
    );

    render(
      <XtermPane
        cwd="C:/dev/rust/kerminal"
        focused
        paneId="pane-local"
        resolvedTheme="dark"
        shell="powershell.exe"
        terminalAppearance={defaultAppSettings.terminal}
        title="本地 PowerShell"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });
    mocks.api.markTerminalSessionBindingDisconnected.mockClear();
    mocks.api.markTerminalSessionBindingReady.mockClear();
    mocks.api.registerTerminalSessionBinding.mockClear();

    fireEvent.contextMenu(screen.getByLabelText("本地 PowerShell xterm 终端"));
    await user.click(screen.getByRole("menuitem", { name: "断开连接" }));

    await waitFor(() => {
      expect(mocks.api.closeTerminal).toHaveBeenCalledWith("session-1");
    });
    expect(await screen.findByText("已断开")).toBeInTheDocument();
    expect(mocks.terminalInstances[0].write).toHaveBeenCalledWith(
      expect.stringContaining("连接已断开"),
    );
    expect(mocks.api.markTerminalSessionBindingDisconnected).toHaveBeenCalledWith(
      expect.objectContaining({
        paneId: "pane-local",
        sessionId: "session-1",
      }),
    );

    fireEvent.contextMenu(screen.getByLabelText("本地 PowerShell xterm 终端"));
    await user.click(screen.getByRole("menuitem", { name: "重新连接" }));

    await waitFor(() => {
      expect(mocks.api.createTerminalSession).toHaveBeenCalledTimes(2);
    });
    expect(mocks.api.createTerminalSession).toHaveBeenLastCalledWith(
      {
        cols: 100,
        cwd: "C:/dev/rust/kerminal",
        rows: 30,
        shell: "powershell.exe",
      },
      expect.any(Function),
    );
    expect(await screen.findByText("已连接")).toBeInTheDocument();
    expect(mocks.api.registerTerminalSessionBinding).toHaveBeenCalledWith(
      expect.objectContaining({
        paneId: "pane-local",
        sessionId: "session-2",
      }),
    );
    expect(mocks.api.markTerminalSessionBindingReady).toHaveBeenCalledWith(
      expect.objectContaining({
        paneId: "pane-local",
        sessionId: "session-2",
      }),
    );
    expect(mocks.api.resizeTerminal).not.toHaveBeenCalledWith("session-2", {
      cols: 100,
      rows: 30,
    });

    mocks.terminalInstances[0].onDataCallback?.("pwd\r");
    expect(mocks.api.writeTerminal).toHaveBeenLastCalledWith(
      "session-2",
      "pwd\r",
    );
  });

  it("reconnects an SSH terminal with the original host id", async () => {
    const user = userEvent.setup();
    let sequence = 0;
    mocks.api.createSshTerminalSession.mockImplementation(
      async (_request, onOutput) => {
        sequence += 1;
        const sessionId = `ssh-session-${sequence}`;
        mocks.setLatestOutputHandler(
          onOutput as (event: TerminalOutputEvent) => void,
        );
        mocks.getLatestOutputHandler()?.({
          data: `hello from ${sessionId}`,
          kind: "data",
          sessionId,
        });
        return {
          cols: 80,
          id: sessionId,
          rows: 24,
          shell: "ssh",
          shellIntegration: { reason: "remote test default", status: "disabled" },
          status: "running",
        };
      },
    );

    render(
      <XtermPane
        focused
        paneId="pane-ssh"
        remoteHostId="host-lab"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="lab server"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    fireEvent.contextMenu(screen.getByLabelText("lab server xterm 终端"));
    await user.click(screen.getByRole("menuitem", { name: "重新连接" }));

    await waitFor(() => {
      expect(mocks.api.createSshTerminalSession).toHaveBeenCalledTimes(2);
    });
    expect(mocks.api.closeTerminal).toHaveBeenCalledWith("ssh-session-1");
    expect(mocks.api.createSshTerminalSession).toHaveBeenLastCalledWith(
      { cols: 100, hostId: "host-lab", rows: 30 },
      expect.any(Function),
    );
    expect(await screen.findByText("已连接")).toBeInTheDocument();
  });

  it("discards clipboard text resolved after the pane disconnects", async () => {
    const user = userEvent.setup();
    let resolveClipboard!: (text: string) => void;
    mocks.api.readTerminalClipboardText.mockReturnValue(new Promise<string>((resolve) => {
      resolveClipboard = resolve;
    }));
    render(
      <XtermPane focused paneId="pane-local" resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal} title="本地 PowerShell" />,
    );
    await screen.findByText("已连接");
    const surface = screen.getByLabelText("本地 PowerShell xterm 终端");
    fireEvent.contextMenu(surface);
    await user.click(screen.getByRole("menuitem", { name: /粘贴/ }));
    fireEvent.contextMenu(surface);
    await user.click(screen.getByRole("menuitem", { name: "断开连接" }));
    await screen.findByText("已断开");
    await act(async () => { resolveClipboard("must not reach old session"); });
    expect(mocks.terminalInstances[0].paste).not.toHaveBeenCalled();
    expect(mocks.api.writeTerminal).not.toHaveBeenCalled();
  });

  it.each(["menu", "paste"] as const)("reports clipboard rejection for %s paste", async (behavior) => {
    const user = userEvent.setup();
    mocks.api.readTerminalClipboardText.mockRejectedValue(new Error("clipboard denied"));
    render(
      <XtermPane focused paneId="pane-local" resolvedTheme="dark"
        terminalAppearance={{ ...defaultAppSettings.terminal, rightClickBehavior: behavior }}
        title="本地 PowerShell" />,
    );
    await screen.findByText("已连接");
    fireEvent.contextMenu(screen.getByLabelText("本地 PowerShell xterm 终端"));
    if (behavior === "menu") {
      await user.click(screen.getByRole("menuitem", { name: /粘贴/ }));
    }
    expect(await screen.findByText("粘贴失败，请检查剪贴板权限或终端连接。")).toBeInTheDocument();
    expect(mocks.terminalInstances[0].paste).not.toHaveBeenCalled();
    expect(mocks.api.writeTerminal).not.toHaveBeenCalled();
  });

  it("disables copy in the context menu when there is no selection", async () => {
    mocks.terminalInstances.length = 0;
    render(
      <XtermPane
        focused
        paneId="pane-local"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="本地 PowerShell"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });
    mocks.terminalInstances[0].getSelection.mockReturnValue("");

    fireEvent.contextMenu(screen.getByLabelText("本地 PowerShell xterm 终端"));

    expect(screen.getByRole("menuitem", { name: "复制Ctrl+C" })).toBeDisabled();
  });

});