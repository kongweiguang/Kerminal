// @author kongweiguang

import { useRef, useState, type ReactNode } from "react";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { TerminalPane, TerminalTab } from "../../../../src/features/workspace/types";
import { mocks } from "../../support/terminal/XtermPane.testSupport.tsx";
import { TerminalWorkspace } from "../../../../src/features/terminal/TerminalWorkspace";
import {
  baseTerminalPane,
  workspaceProps,
} from "../../support/terminal/TerminalWorkspace.testSupport.ts";

import type { Terminal as XtermTerminal } from "@xterm/xterm";
import type { TerminalOutputEvent } from "../../../../src/lib/terminalApi";
import { terminalInputSync } from "../../../../src/features/terminal/terminalInputSync";
import { getTerminalPaneSessionRecord } from "../../../../src/features/terminal/terminalSessionRegistry";
import { useXtermPanePromptBridge } from "../../../../src/features/terminal/XtermPane.promptSourceRegistry";
import type { TerminalCommandBlock } from "../../../../src/features/terminal/terminalCommandBlocks";
const resizableMockState = vi.hoisted(() => ({
  groups: [] as Array<{
    defaultLayout?: Record<string, number>;
    id?: string;
    onLayoutChanged?: (layout: Record<string, number>) => void;
  }>,
}));

vi.mock("../../../../src/components/ui/resizable", () => ({
  ResizableHandle: ({ "aria-label": ariaLabel }: { "aria-label"?: string }) => (
    <div aria-label={ariaLabel} role="separator" />
  ),
  ResizablePanel: ({ children }: { children: ReactNode }) => (
    <div>{children}</div>
  ),
  ResizablePanelGroup: ({
    children,
    defaultLayout,
    id,
    onLayoutChanged,
  }: {
    children: ReactNode;
    defaultLayout?: Record<string, number>;
    id?: string;
    onLayoutChanged?: (layout: Record<string, number>) => void;
  }) => (
    resizableMockState.groups.push({ defaultLayout, id, onLayoutChanged }),
    <div>{children}</div>
  ),
}));

describe("TerminalWorkspace manual input synchronization", () => {
  afterEach(() => {
    act(() => terminalInputSync.configure(null, []));
  });

  it("stops immediately on read error even while the session registry still retains the binding", async () => {
    const runtime = await renderInputSyncWorkspace();
    act(() => {
      runtime.failRead(1);
      expect(getTerminalPaneSessionRecord(runtime.panes[1].id)?.sessionId).toBe(runtime.sessions[1]);
      expect(terminalInputSync.getSnapshot().enabled).toBe(false);
      runtime.terminals[0].triggerKeyData("local after error");
      runtime.terminals[1].triggerKeyData("must not reach failed PTY");
    });
    await expectPaneInput(runtime, 0, ["local after error"]);
    expect(runtime.writes(1)).toEqual([]);
    expect(terminalInputSync.getSnapshot().disabledReason).toBe("所选分屏尚未全部连接");
  });

  it("merges alternating pane input into the same command, histories and command blocks without involving excluded runtimes", async () => {
    const runtime = await renderInputSyncWorkspace();
    const [a, b, c, hidden] = runtime.terminals;
    act(() => {
      a.triggerKeyData("echo sync-");
      b.triggerKeyData("joined");
      b.triggerKeyData("\r");
    });

    await expectPaneInput(runtime, 0, ["echo sync-", "joined", "\r"]);
    await expectPaneInput(runtime, 1, ["echo sync-", "joined", "\r"]);
    expect(runtime.writes(2)).toEqual([]);
    expect(runtime.writes(3)).toEqual([]);
    expect(mocks.api.recordCommandHistory.mock.calls.map(([entry]) => entry)).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ command: "echo sync-joined", paneId: runtime.panes[0].id, sessionId: runtime.sessions[0], source: "broadcast", target: "local", cwd: runtime.panes[0].cwd }),
        expect.objectContaining({ command: "echo sync-joined", paneId: runtime.panes[1].id, sessionId: runtime.sessions[1], source: "user", target: "local", cwd: runtime.panes[1].cwd }),
      ]),
    );
    expect(mocks.api.recordCommandHistory).toHaveBeenCalledTimes(2);
    await expectSyncedCommandBlocks(runtime, "echo sync-joined");

    act(() => {
      c.triggerKeyData("echo C-only\r");
      hidden.triggerKeyData("echo hidden-only\r");
    });
    await expectPaneInput(runtime, 2, ["echo C-only\r"]);
    await expectPaneInput(runtime, 3, ["echo hidden-only\r"]);
    expect(runtime.writes(0)).toEqual(["echo sync-", "joined", "\r"]);
    expect(runtime.writes(1)).toEqual(["echo sync-", "joined", "\r"]);
    expect(mocks.api.recordCommandHistory).toHaveBeenCalledTimes(4);
    expect(terminalInputSync.getSnapshot().enabled).toBe(true);
  });

  it("mirrors the source ghost's accepted text instead of accepting a different receiver candidate", async () => {
    mocks.api.listTerminalSuggestions.mockImplementation(async (request) => {
      const source = request.cwd === "C:/sync/a";
      const paneId = source ? "pane-sync-a" : "pane-sync-b";
      const suffix = source ? " status --short" : " reset --hard";
      return [{
        description: "本分屏历史候选",
        displayText: `git${suffix}`,
        id: `ghost-${paneId}`,
        provider: "history",
        replacementRange: { end: 3, start: 0 },
        replacementText: `git${suffix}`,
        score: 0.9,
        sensitivity: "normal",
        sourceId: `history-${paneId}`,
        suffix,
      }];
    });
    const runtime = await renderInputSyncWorkspace(1);
    runtime.terminals[0].buffer.active.cursorX = 3;
    runtime.terminals[1].buffer.active.cursorX = 3;
    act(() => { for (const key of "git") runtime.terminals[1].triggerKeyData(key); });
    await waitFor(() => {
      expect(within(runtime.host(1)).getByLabelText("终端命令灰色提示").textContent).toBe(" reset --hard");
    });
    // 非焦点分屏会隐藏灰字；先让 B 真实取得独立候选，再切回 A 请求源候选。
    act(() => runtime.focus(0));
    act(() => {
      runtime.terminals[0].triggerKeyData("\u007f");
      runtime.terminals[0].triggerKeyData("t");
    });
    await waitFor(() => {
      expect(within(runtime.host(0)).getByLabelText("终端命令灰色提示").textContent).toBe(" status --short");
    });

    act(() => runtime.terminals[0].triggerKeyData("\u001b[C"));
    await expectPaneInput(runtime, 0, ["g", "i", "t", "\u007f", "t", " status --short"]);
    await expectPaneInput(runtime, 1, ["g", "i", "t", "\u007f", "t", " status --short"]);
    act(() => runtime.terminals[0].triggerKeyData("\r"));
    await expectPaneInput(runtime, 0, ["g", "i", "t", "\u007f", "t", " status --short", "\r"]);
    await expectPaneInput(runtime, 1, ["g", "i", "t", "\u007f", "t", " status --short", "\r"]);
    expect(mocks.api.recordCommandHistory.mock.calls.map(([entry]) => entry)).toEqual([
      expect.objectContaining({ command: "git status --short", paneId: runtime.panes[0].id, source: "user" }),
      expect.objectContaining({ command: "git status --short", paneId: runtime.panes[1].id, source: "broadcast" }),
    ]);
    expect(mocks.api.recordTerminalSuggestionFeedback).toHaveBeenCalledWith(
      expect.objectContaining({ action: "accepted", replacementText: "git status --short", sourceId: "history-pane-sync-a" }),
    );
    expect(mocks.api.recordTerminalSuggestionFeedback).not.toHaveBeenCalledWith(
      expect.objectContaining({ action: "accepted", sourceId: "history-pane-sync-b" }),
    );
    await expectSyncedCommandBlocks(runtime, "git status --short");
    expect(runtime.writes(2)).toEqual([]);
    expect(runtime.writes(3)).toEqual([]);
  });

  it("synchronizes the actual compatibility override bytes and keeps unselected C local", async () => {
    const runtime = await renderInputSyncWorkspace();
    const [a, b, c] = runtime.terminals;
    act(() => {
      expect(a.triggerCustomKeyEvent({ code: "Enter", key: "Enter", keyCode: 13, shiftKey: true })).toEqual({ defaultPrevented: true, result: false });
      b.triggerCustomKeyEvent({ altKey: true, code: "KeyV", key: "v", keyCode: 86 });
      fireEvent.keyDown(screen.getByLabelText("同步 B xterm 终端"), { code: "KeyV", ctrlKey: true, key: "V", keyCode: 86, shiftKey: true });
      a.triggerKeyData("\u001b[D");
      a.triggerKeyData("\u0003");
      a.triggerKeyData("\u0004");
      a.triggerKeyData("\u000c");
      c.triggerCustomKeyEvent({ altKey: true, code: "KeyV", key: "v", keyCode: 86 });
    });
    const expected = ["\n", "\u001bv", "\u0016", "\u001b[D", "\u0003", "\u0004", "\u000c"];
    await expectPaneInput(runtime, 0, expected);
    await expectPaneInput(runtime, 1, expected);
    await expectPaneInput(runtime, 2, ["\u001bv"]);
    expect(runtime.writes(3)).toEqual([]);
    expect(a.paste).not.toHaveBeenCalled();
    expect(b.paste).not.toHaveBeenCalled();
    expect(mocks.api.readTerminalClipboardText).not.toHaveBeenCalled();
    expect(terminalInputSync.getSnapshot().enabled).toBe(true);
  });

  it("keeps directed paste and submit behind already accepted local input", async () => {
    const runtime = await renderInputSyncWorkspace();
    const a = runtime.terminals[0];
    let release!: () => void;
    const pending = new Promise<void>((resolve) => { release = resolve; });
    mocks.api.writeTerminal.mockImplementation((sessionId, data) =>
      sessionId === runtime.sessions[0] && data === "first " ? pending : Promise.resolve());
    a.paste.mockImplementation((text: string) => { a.onDataCallback?.(text); });
    act(() => {
      a.triggerKeyData("first ");
      a.triggerKeyData("second ");
    });
    render(<DirectedWorkspaceInput terminal={a as unknown as XtermTerminal} paneId={runtime.panes[0].id} sessionId={runtime.sessions[0]} />);
    await act(async () => { release(); });
    await expectPaneInput(runtime, 0, ["first ", "second ", "echo directed", "\r"]);
    expect(terminalInputSync.getSnapshot().enabled).toBe(false);
  });

  it("closes the selected group before consuming directed inputRequest paste and leaves later DEL local", async () => {
    const runtime = await renderInputSyncWorkspace();
    const a = runtime.terminals[0];
    // 仅回送 onData 以覆盖生产 bridge/model 集成，不模拟真实 xterm 的 paste/IME 行为。
    a.paste.mockImplementation((text: string) => {
      expect(terminalInputSync.getSnapshot().enabled).toBe(false);
      a.onDataCallback?.(text);
    });
    render(<DirectedWorkspaceInput terminal={a as unknown as XtermTerminal} paneId={runtime.panes[0].id} sessionId={runtime.sessions[0]} />);

    await expectPaneInput(runtime, 0, ["echo directed", "\r"]);
    expect(a.paste).toHaveBeenCalledExactlyOnceWith("echo directed");
    expect(terminalInputSync.getSnapshot()).toMatchObject({
      enabled: false,
      notice: { level: "status", message: "检测到定向输入，同步已关闭。" },
    });
    act(() => a.triggerKeyData("\u007f"));
    await expectPaneInput(runtime, 0, ["echo directed", "\r", "\u007f"]);
    expect(runtime.writes(1)).toEqual([]);
    expect(runtime.writes(2)).toEqual([]);
    expect(runtime.writes(3)).toEqual([]);
  });
});

interface InputSyncWorkspaceRuntime {
  failRead(index: number): void;
  focus(index: number): void;
  host(index: number): HTMLElement;
  panes: TerminalPane[];
  sessions: string[];
  terminals: (typeof mocks.terminalInstances)[number][];
  writes(index: number): string[];
}

/** @author kongweiguang 使用真实 Workspace/runtime，并以各 pane 独立 PTY 输出通道避免共享 session 掩盖错路由。 */
async function renderInputSyncWorkspace(initialFocusedIndex = 0): Promise<InputSyncWorkspaceRuntime> {
  const panes: TerminalPane[] = ["a", "b", "c", "hidden"].map((name) => ({
    ...baseTerminalPane,
    cwd: `C:/sync/${name}`,
    id: `pane-sync-${name}`,
    title: `同步 ${name === "hidden" ? "隐藏" : name.toUpperCase()}`,
  }));
  const outputs = new Map<string, (event: TerminalOutputEvent) => void>();
  mocks.api.createTerminalSession.mockImplementation(async (request, onOutput) => {
    const sessionId = `session-${request.cwd?.split("/").at(-1)}`;
    outputs.set(sessionId, onOutput);
    return {
      cols: request.cols,
      id: sessionId,
      rows: request.rows,
      shell: "powershell.exe",
      shellIntegration: { reason: "sync integration regression", status: "disabled" },
      status: "running",
    };
  });
  const tabs: TerminalTab[] = [{
    id: "tab-sync",
    title: "同步测试",
    machineId: "local-powershell",
    layout: { children: panes.slice(0, 3).map((pane) => ({ paneId: pane.id, type: "pane" as const })), direction: "horizontal", id: "split-sync", type: "split" },
  }, { id: "tab-sync-hidden", title: "隐藏测试", machineId: "local-powershell", layout: { paneId: panes[3].id, type: "pane" } }];
  let focus: (index: number) => void = () => { throw new Error("Workspace 尚未挂载"); };
  /** @author kongweiguang 将焦点状态保留在真实 Workspace 内。 */
  function InputSyncWorkspace() {
    const [focusedPaneId, setFocusedPaneId] = useState(panes[initialFocusedIndex].id);
    focus = (index) => setFocusedPaneId(panes[index].id);
    return <TerminalWorkspace {...workspaceProps({ activeTabId: tabs[0].id, focusedPaneId, onFocusPane: setFocusedPaneId, panes, tabs })} />;
  }
  render(<InputSyncWorkspace />);
  await waitFor(() => {
    expect(outputs.size).toBe(4);
    panes.forEach((pane) => expect(getTerminalPaneSessionRecord(pane.id)?.sessionId).toBe(`session-${pane.id.replace("pane-sync-", "")}`));
  });
  const host = (index: number) => document.querySelector<HTMLElement>(`[data-terminal-pane-runtime-host="${panes[index].id}"]`)!;
  const terminals = panes.map((_, index) => mocks.terminalInstances.find((terminal) => {
    const container = terminal.open.mock.calls[0]?.[0];
    return container && host(index).contains(container);
  })!);
  expect(terminals.every(Boolean)).toBe(true);
  const sessions = panes.map((pane) => getTerminalPaneSessionRecord(pane.id)!.sessionId);
  act(() => {
    sessions.forEach((sessionId) => outputs.get(sessionId)!({ data: "PS> ", kind: "data", sessionId }));
  });
  fireEvent.click(screen.getByRole("button", { name: "发送目标：全部分屏 · 3" }));
  fireEvent.click(within(screen.getByRole("menu", { name: "发送目标选择" })).getByRole("checkbox", { name: /同步 C/ }));
  fireEvent.keyDown(window, { key: "Escape" });
  fireEvent.click(screen.getByRole("switch", { name: "同步终端输入" }));
  expect(terminalInputSync.getSnapshot().enabled).toBe(true);
  mocks.api.writeTerminal.mockClear();
  return {
    focus, host, panes, sessions, terminals,
    failRead: (index: number) => outputs.get(sessions[index])!({ data: "read failure", kind: "error", sessionId: sessions[index] }),
    writes: (index: number) => mocks.api.writeTerminal.mock.calls.filter(([sessionId]) => sessionId === sessions[index]).map(([, data]) => data),
  };
}

/** @author kongweiguang 逐 PTY 断言完整序列，保留分包与来源单写约束。 */
async function expectPaneInput(runtime: InputSyncWorkspaceRuntime, index: number, data: string[]) {
  await waitFor(() => expect(runtime.writes(index)).toEqual(data));
}

/** @author kongweiguang 命令块必须来自实际输入模型，且隔离 pane 不得获得同名命令块。 */
async function expectSyncedCommandBlocks(runtime: InputSyncWorkspaceRuntime, command: string) {
  await waitFor(() => {
    for (const index of [0, 1]) {
      expect(within(runtime.host(index)).getAllByLabelText(`折叠命令块 ${command}`)).toHaveLength(1);
    }
  });
  for (const index of [2, 3]) {
    expect(within(runtime.host(index)).queryByLabelText(`折叠命令块 ${command}`)).toBeNull();
  }
}

/** @author kongweiguang Workspace 不公开程序输入 props；直接调用其 XtermPane 使用的生产 bridge，指向已有 runtime。 */
function DirectedWorkspaceInput({ terminal, paneId, sessionId }: { terminal: XtermTerminal; paneId: string; sessionId: string }) {
  const commandBlocksRef = useRef<TerminalCommandBlock[]>([]);
  const terminalRef = useRef<XtermTerminal | null>(terminal);
  const sessionIdRef = useRef<string | null>(sessionId);
  useXtermPanePromptBridge({
    commandBlocksRef,
    connectionState: "connected",
    inputRequest: { id: "sync-directed-input", submit: true, text: "echo directed" },
    paneId,
    sessionIdRef,
    terminalRef,
  });
  return null;
}
