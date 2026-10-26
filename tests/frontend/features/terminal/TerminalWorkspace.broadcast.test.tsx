// @author kongweiguang

import { useState, type ReactNode } from "react";
import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { TerminalPane, TerminalTab } from "../../../../src/features/workspace/types";
import { TerminalWorkspace } from "../../../../src/features/terminal/TerminalWorkspace";
import { terminalInputSync } from "../../../../src/features/terminal/terminalInputSync";
import {
  baseTerminalPane,
  batchPanes,
  batchTabs,
  mixedSplitPanes,
  mixedSplitTabs,
  previewOnlyPanes,
  previewOnlyTabs,
  workspaceProps,
} from "../../support/terminal/TerminalWorkspace.testSupport.ts";

vi.mock("../../../../src/features/terminal/XtermPane", () => ({
  XtermPane: ({ title }: { title: string }) => (
    <div aria-label={`${title} xterm 终端`}>本地终端测试替身</div>
  ),
}));

vi.mock("../../../../src/components/ui/resizable", () => ({
  ResizableHandle: ({ "aria-label": ariaLabel }: { "aria-label"?: string }) => (
    <div aria-label={ariaLabel} role="separator" />
  ),
  ResizablePanel: ({ children }: { children: ReactNode }) => <div>{children}</div>,
  ResizablePanelGroup: ({ children }: { children: ReactNode }) => <div>{children}</div>,
}));

const syncDisposers: Array<() => void> = [];

/** 工具栏测试只替代连接资格；输入模型及真实 xterm 分别由集成用例覆盖。 */
function connectBroadcastTargets(failReceiver = false) {
  for (const pane of batchPanes) {
    syncDisposers.push(terminalInputSync.register(pane.id, {
      readBinding: () => ({ sessionId: `sync-${pane.id}`, connectionGeneration: 1, tabId: "tab-batch" }),
      prepareMirroredInput: (input) => {
        if (failReceiver && pane.id === "pane-batch-ssh") throw new Error("encoding failed");
        return input.data;
      },
      observeMirroredInput: () => {},
      onWriteError: () => {},
    }));
  }
}

afterEach(() => {
  act(() => {
    terminalInputSync.configure(null, []);
    for (const dispose of syncDisposers.splice(0)) dispose();
  });
});

describe("TerminalWorkspace broadcast command", () => {
  it("updates the broadcast command draft", async () => {
    const user = userEvent.setup();
    const onBroadcastDraftChange = vi.fn();

    function ControlledWorkspace() {
      const [broadcastDraft, setBroadcastDraft] = useState("");

      return (
        <TerminalWorkspace
          {...workspaceProps({
            activeTabId: "tab-mixed-split",
            broadcastDraft,
            focusedPaneId: "pane-split-local",
            onBroadcastDraftChange: (draft) => {
              setBroadcastDraft(draft);
              onBroadcastDraftChange(draft);
            },
            panes: mixedSplitPanes,
            tabs: mixedSplitTabs,
          })}
        />
      );
    }

    render(<ControlledWorkspace />);

    await user.type(screen.getByLabelText("批量命令"), "uptime");

    expect(onBroadcastDraftChange).toHaveBeenLastCalledWith("uptime");
  });

  it("sends a safe command to the active local pane without confirmation", async () => {
    const user = userEvent.setup();
    const onBroadcastCommand = vi.fn().mockResolvedValue({
      missingPaneIds: [],
      sentPaneIds: ["pane-split-local"],
    });

    function ControlledWorkspace() {
      const [broadcastDraft, setBroadcastDraft] = useState("");

      return (
        <TerminalWorkspace
          {...workspaceProps({
            activeTabId: "tab-mixed-split",
            broadcastDraft,
            focusedPaneId: "pane-split-local",
            onBroadcastCommand,
            onBroadcastDraftChange: setBroadcastDraft,
            panes: mixedSplitPanes,
            tabs: mixedSplitTabs,
          })}
        />
      );
    }

    render(<ControlledWorkspace />);

    await user.type(screen.getByLabelText("批量命令"), "uptime");
    await user.click(screen.getByRole("button", { name: "发送到 1 个目标" }));

    expect(onBroadcastCommand).toHaveBeenCalledWith({
      command: "uptime",
      data: "uptime\r",
      targetPaneIds: ["pane-split-local"],
    });
    expect(screen.getByRole("status")).toHaveTextContent("已发送到 1 个分屏");
    expect(screen.getByLabelText("批量命令")).toHaveValue("");
  });

  it("sends to multiple or SSH panes without confirmation", async () => {
    const user = userEvent.setup();
    const onBroadcastCommand = vi.fn().mockResolvedValue({
      missingPaneIds: ["pane-batch-ssh"],
      sentPaneIds: ["pane-batch-local"],
    });
    const onBroadcastDraftChange = vi.fn();

    function ControlledWorkspace() {
      const [broadcastDraft, setBroadcastDraft] = useState("");

      return (
        <TerminalWorkspace
          {...workspaceProps({
            activeTabId: "tab-batch",
            broadcastDraft,
            focusedPaneId: "pane-batch-local",
            onBroadcastCommand,
            onBroadcastDraftChange: (draft) => {
              setBroadcastDraft(draft);
              onBroadcastDraftChange(draft);
            },
            panes: batchPanes,
            tabs: batchTabs,
          })}
        />
      );
    }

    render(<ControlledWorkspace />);

    await user.click(screen.getByLabelText("批量命令"));
    expect(screen.getByLabelText("批量命令")).toHaveFocus();
    await user.keyboard("systemctl status nginx");
    expect(onBroadcastDraftChange).toHaveBeenCalled();
    expect(screen.getByLabelText("批量命令")).toHaveValue("systemctl status nginx");
    expect(screen.getByRole("button", { name: "发送到 2 个目标" })).toBeEnabled();
    await user.click(screen.getByRole("button", { name: "发送到 2 个目标" }));

    expect(onBroadcastCommand).toHaveBeenCalledWith({
      command: "systemctl status nginx",
      data: "systemctl status nginx\r",
      targetPaneIds: ["pane-batch-local", "pane-batch-ssh"],
    });
    expect(screen.getByRole("status")).toHaveTextContent("1 个分屏尚未连接");
  });

  it("can send only to the focused pane from a split tab", async () => {
    const user = userEvent.setup();
    const onBroadcastCommand = vi.fn().mockResolvedValue({
      missingPaneIds: [],
      sentPaneIds: ["pane-batch-ssh"],
    });

    function ControlledWorkspace() {
      const [broadcastDraft, setBroadcastDraft] = useState("");

      return (
        <TerminalWorkspace
          {...workspaceProps({
            activeTabId: "tab-batch",
            broadcastDraft,
            focusedPaneId: "pane-batch-ssh",
            onBroadcastCommand,
            onBroadcastDraftChange: setBroadcastDraft,
            panes: batchPanes,
            tabs: batchTabs,
          })}
        />
      );
    }

    render(<ControlledWorkspace />);

    await user.type(screen.getByLabelText("批量命令"), "hostname");
    await user.click(
      screen.getByRole("button", { name: "发送目标：全部分屏 · 2" }),
    );
    await user.click(
      within(screen.getByRole("menu", { name: "发送目标选择" })).getByRole(
        "button",
        { name: /当前分屏/ },
      ),
    );
    await user.click(screen.getByRole("button", { name: "发送到 1 个目标" }));

    expect(onBroadcastCommand).toHaveBeenCalledWith({
      command: "hostname",
      data: "hostname\r",
      targetPaneIds: ["pane-batch-ssh"],
    });
  });

  it("can send to a custom subset of split panes", async () => {
    const user = userEvent.setup();
    const onBroadcastCommand = vi.fn().mockResolvedValue({
      missingPaneIds: [],
      sentPaneIds: ["pane-batch-local"],
    });

    function ControlledWorkspace() {
      const [broadcastDraft, setBroadcastDraft] = useState("");

      return (
        <TerminalWorkspace
          {...workspaceProps({
            activeTabId: "tab-batch",
            broadcastDraft,
            focusedPaneId: "pane-batch-local",
            onBroadcastCommand,
            onBroadcastDraftChange: setBroadcastDraft,
            panes: batchPanes,
            tabs: batchTabs,
          })}
        />
      );
    }

    render(<ControlledWorkspace />);

    await user.type(screen.getByLabelText("批量命令"), "date");
    await user.click(
      screen.getByRole("button", { name: "发送目标：全部分屏 · 2" }),
    );
    const targetMenu = screen.getByRole("menu", { name: "发送目标选择" });
    await user.click(within(targetMenu).getByRole("button", { name: /自定义/ }));
    await user.click(
      within(targetMenu).getByRole("checkbox", { name: /SSH 批量/ }),
    );
    await user.click(screen.getByRole("button", { name: "发送到 1 个目标" }));

    expect(onBroadcastCommand).toHaveBeenCalledWith({
      command: "date",
      data: "date\r",
      targetPaneIds: ["pane-batch-local"],
    });
  });

  it("keeps an empty custom selection instead of sending to every pane", async () => {
    const user = userEvent.setup();
    const onBroadcastCommand = vi.fn();
    render(
      <TerminalWorkspace
        {...workspaceProps({
          activeTabId: "tab-batch",
          broadcastDraft: "hostname",
          focusedPaneId: "pane-batch-local",
          onBroadcastCommand,
          panes: batchPanes,
          tabs: batchTabs,
        })}
      />,
    );
    await user.click(screen.getByRole("button", { name: "发送目标：全部分屏 · 2" }));
    const menu = screen.getByRole("menu", { name: "发送目标选择" });
    await user.click(within(menu).getByRole("button", { name: /自定义/ }));
    for (const checkbox of within(menu).getAllByRole("checkbox")) {
      await user.click(checkbox);
    }
    expect(screen.getByRole("button", { name: "发送目标：自定义 · 0" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "没有可发送目标" })).toBeDisabled();
    expect(screen.getByRole("switch", { name: "同步终端输入" })).toBeDisabled();
    expect(screen.getByRole("switch", { name: "同步终端输入" }).parentElement).toHaveAttribute("title", "请至少选择两个终端分屏");
    expect(onBroadcastCommand).not.toHaveBeenCalled();
  });

  it("hides broadcast controls after switching from a split tab to a single pane tab", async () => {
    const user = userEvent.setup();
    const onBroadcastCommand = vi.fn().mockResolvedValue({
      missingPaneIds: [],
      sentPaneIds: ["pane-local"],
    });

    function ControlledWorkspace() {
      const [activeTabId, setActiveTabId] = useState("tab-batch");
      const [broadcastDraft, setBroadcastDraft] = useState("");
      const focusedPaneId =
        activeTabId === "tab-batch" ? "pane-batch-local" : "pane-local";

      return (
        <>
          <button onClick={() => setActiveTabId("tab-local")} type="button">
            切换到本地
          </button>
          <TerminalWorkspace
            {...workspaceProps({
              activeTabId,
              broadcastDraft,
              focusedPaneId,
              onBroadcastCommand,
              onBroadcastDraftChange: setBroadcastDraft,
              panes: [...batchPanes, baseTerminalPane],
              tabs: [...batchTabs, {
                ...baseTerminalPane,
                id: "tab-local",
                layout: { paneId: "pane-local", type: "pane" as const },
                machineId: "local-powershell",
                title: "本地 PowerShell",
              } as TerminalTab],
            })}
          />
        </>
      );
    }

    render(<ControlledWorkspace />);

    await user.click(
      screen.getByRole("button", { name: "发送目标：全部分屏 · 2" }),
    );
    await user.click(
      within(screen.getByRole("menu", { name: "发送目标选择" })).getByRole(
        "button",
        { name: /自定义/ },
      ),
    );
    expect(
      screen.getByRole("button", { name: "发送目标：自定义 · 2" }),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "切换到本地" }));

    expect(screen.queryByLabelText("批量命令")).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /发送目标/ }),
    ).not.toBeInTheDocument();
    expect(onBroadcastCommand).not.toHaveBeenCalled();
  });

  it("includes telnet and serial panes in broadcast targets without confirmation", async () => {
    const user = userEvent.setup();
    const onBroadcastCommand = vi.fn().mockResolvedValue({
      missingPaneIds: [],
      sentPaneIds: ["pane-broadcast-telnet", "pane-broadcast-serial"],
    });
    const panes: TerminalPane[] = [
      {
        ...baseTerminalPane,
        id: "pane-broadcast-telnet",
        machineId: "telnet-host",
        mode: "telnet",
        title: "Telnet 设备",
      },
      {
        ...baseTerminalPane,
        id: "pane-broadcast-serial",
        machineId: "serial-device",
        mode: "serial",
        title: "Serial 设备",
      },
      {
        ...baseTerminalPane,
        id: "pane-broadcast-preview",
        machineId: "preview-helper",
        mode: "preview",
        title: "预览辅助",
      },
    ];
    const tabs: TerminalTab[] = [
      {
        id: "tab-device-broadcast",
        layout: {
          type: "split",
          id: "split-device-broadcast",
          direction: "horizontal",
          children: [
            { type: "pane", paneId: "pane-broadcast-telnet" },
            { type: "pane", paneId: "pane-broadcast-serial" },
            { type: "pane", paneId: "pane-broadcast-preview" },
          ],
        },
        machineId: "telnet-host",
        title: "设备批量",
      },
    ];

    function ControlledWorkspace() {
      const [broadcastDraft, setBroadcastDraft] = useState("");

      return (
        <TerminalWorkspace
          {...workspaceProps({
            activeTabId: "tab-device-broadcast",
            broadcastDraft,
            focusedPaneId: "pane-broadcast-telnet",
            onBroadcastCommand,
            onBroadcastDraftChange: setBroadcastDraft,
            panes,
            tabs,
          })}
        />
      );
    }

    render(<ControlledWorkspace />);

    await user.type(screen.getByLabelText("批量命令"), "show version");
    await user.click(screen.getByRole("button", { name: "发送到 2 个目标" }));

    expect(onBroadcastCommand).toHaveBeenCalledWith({
      command: "show version",
      data: "show version\r",
      targetPaneIds: ["pane-broadcast-telnet", "pane-broadcast-serial"],
    });
  });

  it("disables broadcast when the active tab has no real terminal panes", async () => {
    const user = userEvent.setup();
    const onBroadcastCommand = vi.fn();

    function ControlledWorkspace() {
      const [broadcastDraft, setBroadcastDraft] = useState("");

      return (
        <TerminalWorkspace
          {...workspaceProps({
            activeTabId: "tab-preview-only",
            broadcastDraft,
            focusedPaneId: "pane-preview-a",
            onBroadcastCommand,
            onBroadcastDraftChange: setBroadcastDraft,
            panes: previewOnlyPanes,
            tabs: previewOnlyTabs,
          })}
        />
      );
    }

    render(<ControlledWorkspace />);

    await user.type(screen.getByLabelText("批量命令"), "uptime");

    expect(screen.getByRole("button", { name: "没有可发送目标" })).toBeDisabled();
    expect(onBroadcastCommand).not.toHaveBeenCalled();
  });
  it("keeps pointer focus, supports keyboard activation and stops rather than expanding a focused target", async () => {
    const user = userEvent.setup();
    connectBroadcastTargets();
    const props = workspaceProps({ activeTabId: "tab-batch", focusedPaneId: "pane-batch-local", panes: batchPanes, tabs: batchTabs });
    const { rerender } = render(<TerminalWorkspace {...props} />);
    const toggle = screen.getByRole("switch", { name: "同步终端输入" });
    expect(toggle).toHaveAttribute("aria-checked", "false");
    await user.click(screen.getByLabelText("批量命令"));
    await user.click(toggle);
    expect(screen.getByLabelText("批量命令")).toHaveFocus();
    expect(toggle).toHaveAttribute("aria-checked", "true");
    rerender(<TerminalWorkspace {...props} focusedPaneId="pane-batch-ssh" />);
    expect(toggle).toHaveAttribute("aria-checked", "true");
    await user.tab();
    expect(toggle).toHaveFocus();
    await user.keyboard(" ");
    expect(toggle).toHaveAttribute("aria-checked", "false");
    expect(toggle).toHaveFocus();
    await user.keyboard("{Enter}");
    expect(toggle).toHaveAttribute("aria-checked", "true");
    await user.click(screen.getByRole("button", { name: "发送目标：全部分屏 · 2" }));
    await user.click(within(screen.getByRole("menu", { name: "发送目标选择" })).getByRole("button", { name: /当前分屏/ }));
    expect(toggle).toBeDisabled();
    expect(toggle).toHaveAttribute("aria-checked", "false");
    expect(toggle.parentElement).toHaveAttribute("title", "请至少选择两个终端分屏");
    expect(screen.getByRole("status")).toHaveTextContent("目标已变更，同步已关闭。");
  });

  it("shows send and synchronization failures together instead of masking either error", async () => {
    const user = userEvent.setup();
    connectBroadcastTargets(true);
    render(<TerminalWorkspace {...workspaceProps({
      activeTabId: "tab-batch", focusedPaneId: "pane-batch-local", panes: batchPanes, tabs: batchTabs,
      broadcastDraft: "hostname", onBroadcastCommand: vi.fn().mockRejectedValue(new Error("send failure")),
    })} />);
    await user.click(screen.getByRole("button", { name: "发送到 2 个目标" }));
    await user.click(screen.getByRole("switch", { name: "同步终端输入" }));
    await act(async () => { await terminalInputSync.writeUserInput("pane-batch-local", { data: "x" }); });
    const alert = screen.getByRole("alert");
    expect(alert).toHaveTextContent("send failure");
    expect(alert).toHaveTextContent("同步已关闭：部分输入可能未送达，请检查各分屏后重新开启。");
    expect(screen.getByRole("switch", { name: "同步终端输入" })).toHaveAttribute("aria-checked", "false");
  });
});
