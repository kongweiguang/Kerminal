// @author kongweiguang
import { act, render, screen, waitFor } from "@testing-library/react";
import { defaultAppSettings } from "../../../../src/features/settings/settingsModel";
import { Profiler, type ProfilerOnRenderCallback } from "react";
import { describe, expect, it, vi } from "vitest";
import {
  mockElementBox,
  mocks,
} from "../../support/terminal/XtermPane.testSupport.tsx";
import { XtermPane } from "../../../../src/features/terminal/XtermPane";

describe("XtermPane inline suggestions", () => {
  it("renders a ghost command suggestion and accepts it with right arrow", async () => {
    mocks.api.listTerminalSuggestions.mockResolvedValue([
      {
        description: "历史命令，匹配当前目录",
        displayText: "git status --short",
        id: "history-1",
        provider: "history",
        replacementRange: { end: 3, start: 0 },
        replacementText: "git status --short",
        score: 0.9,
        sensitivity: "normal",
        sourceId: "history-1",
        suffix: " status --short",
      },
    ]);

    render(
      <XtermPane
        cwd="C:/dev/rust/kerminal"
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

    const terminal = mocks.terminalInstances[0];
    terminal.buffer.active.cursorX = 3;
    act(() => {
      terminal.onDataCallback?.("g");
      terminal.onDataCallback?.("i");
      terminal.onDataCallback?.("t");
    });

    await waitFor(() => {
      expect(mocks.api.listTerminalSuggestions).toHaveBeenCalledWith(
        expect.objectContaining({
          cwd: "C:/dev/rust/kerminal",
          input: "git",
          limit: 1,
          providers: ["history", "spec", "snippet"],
          target: "local",
        }),
      );
    });
    const ghostSuggestion = await screen.findByLabelText("终端命令灰色提示");
    expect(ghostSuggestion.textContent).toBe(" status --short");

    mocks.api.writeTerminal.mockClear();
    act(() => {
      terminal.triggerKeyData("\u001b[C");
    });

    expect(mocks.api.writeTerminal).toHaveBeenCalledTimes(1);
    expect(mocks.api.writeTerminal).toHaveBeenCalledWith(
      "session-1",
      " status --short",
    );
    expect(mocks.api.writeTerminal).not.toHaveBeenCalledWith(
      "session-1",
      "\u001b[C",
    );
    expect(mocks.api.recordTerminalSuggestionFeedback).toHaveBeenCalledWith(
      expect.objectContaining({
        action: "accepted",
        input: "git",
        provider: "history",
        replacementText: "git status --short",
        sourceId: "history-1",
        target: "local",
      }),
    );
    await waitFor(() => {
      expect(
        screen.queryByLabelText("终端命令灰色提示"),
      ).not.toBeInTheDocument();
    });
  });

  it("can finish a candidate after accepting its first segment without losing the remainder", async () => {
    mocks.api.listTerminalSuggestions.mockResolvedValue([{
      acceptBoundaries: [10], description: "分段历史候选", displayText: "git status --short",
      id: "partial-history", provider: "history", replacementRange: { start: 0, end: 3 },
      replacementText: "git status --short", score: 0.9, sensitivity: "normal",
      sourceId: "partial-history", suffix: " status --short",
    }]);
    render(<XtermPane focused paneId="pane-local" resolvedTheme="dark"
      terminalAppearance={{ ...defaultAppSettings.terminal, inlineSuggestion: {
        ...defaultAppSettings.terminal.inlineSuggestion, partialAccept: true,
      } }} title="本地 PowerShell" />);
    await screen.findByText("已连接");
    const terminal = mocks.terminalInstances[0];
    terminal.buffer.active.cursorX = 3;
    act(() => { for (const key of "git") terminal.triggerKeyData(key); });
    await screen.findByLabelText("终端命令灰色提示");
    mocks.api.writeTerminal.mockClear();
    act(() => {
      terminal.triggerCustomKeyEvent({ altKey: true, key: "ArrowRight", code: "ArrowRight", keyCode: 39 });
      terminal.triggerCustomKeyEvent({ key: "ArrowRight", code: "ArrowRight", keyCode: 39 });
      terminal.triggerKeyData("\r");
    });
    await waitFor(() => expect(mocks.api.writeTerminal.mock.calls).toEqual([
      ["session-1", " status"], ["session-1", " --short"], ["session-1", "\r"],
    ]));
    expect(mocks.api.recordCommandHistory).toHaveBeenCalledWith(expect.objectContaining({ command: "git status --short" }));
  });

  it("positions ghost suggestions from the xterm screen grid even on short rows", async () => {
    mocks.api.listTerminalSuggestions.mockResolvedValue([
      {
        description: "历史命令，匹配当前目录",
        displayText: "git status --short",
        id: "history-1",
        provider: "history",
        replacementRange: { end: 3, start: 0 },
        replacementText: "git status --short",
        score: 0.9,
        sensitivity: "normal",
        sourceId: "history-1",
        suffix: " status --short",
      },
    ]);

    render(
      <XtermPane
        cwd="C:/dev/rust/kerminal"
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

    const terminalContainer = screen.getByLabelText("本地 PowerShell xterm 终端");
    const terminalFrame = terminalContainer.parentElement;
    const xtermScreen = terminalContainer.querySelector(".xterm-screen");
    const rows = terminalContainer.querySelector(".xterm-rows");
    const firstRow = terminalContainer.querySelector(".xterm-rows > div");
    if (
      !(terminalContainer instanceof HTMLElement) ||
      !(terminalFrame instanceof HTMLElement) ||
      !(xtermScreen instanceof HTMLElement) ||
      !(rows instanceof HTMLElement) ||
      !(firstRow instanceof HTMLElement)
    ) {
      throw new Error("Mock xterm DOM was not mounted");
    }

    mockElementBox(terminalFrame, {
      height: 520,
      rectLeft: 100,
      rectTop: 80,
      width: 960,
    });
    mockElementBox(terminalContainer, {
      clientWidth: 900,
      offsetLeft: 20,
      offsetTop: 30,
    });
    mockElementBox(xtermScreen, {
      height: 480,
      rectLeft: 140,
      rectTop: 150,
      width: 800,
    });
    mockElementBox(rows, {
      height: 480,
      offsetLeft: 12,
      offsetTop: 6,
      rectLeft: 140,
      rectTop: 150,
      width: 800,
    });
    mockElementBox(firstRow, { height: 8, width: 32 });

    const terminal = mocks.terminalInstances[0];
    terminal.buffer.active.cursorX = 7;
    terminal.buffer.active.cursorY = 4;
    act(() => {
      terminal.onDataCallback?.("g");
      terminal.onDataCallback?.("i");
      terminal.onDataCallback?.("t");
    });

    const ghostSuggestion = await screen.findByLabelText("终端命令灰色提示");
    expect(ghostSuggestion).toHaveStyle({
      left: "96px",
      top: "134px",
    });
  });

  it("positions ghost suggestions using xterm cell cursor for wide characters", async () => {
    mocks.api.listTerminalSuggestions.mockResolvedValue([
      {
        description: "历史命令，匹配当前目录",
        displayText: "部署 --dry-run",
        id: "history-wide-1",
        provider: "history",
        replacementRange: { end: 2, start: 0 },
        replacementText: "部署 --dry-run",
        score: 0.9,
        sensitivity: "normal",
        sourceId: "history-wide-1",
        suffix: " --dry-run",
      },
    ]);

    render(
      <XtermPane
        cwd="C:/dev/rust/kerminal"
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

    const terminalContainer = screen.getByLabelText("本地 PowerShell xterm 终端");
    const terminalFrame = terminalContainer.parentElement;
    const xtermScreen = terminalContainer.querySelector(".xterm-screen");
    const rows = terminalContainer.querySelector(".xterm-rows");
    if (
      !(terminalContainer instanceof HTMLElement) ||
      !(terminalFrame instanceof HTMLElement) ||
      !(xtermScreen instanceof HTMLElement) ||
      !(rows instanceof HTMLElement)
    ) {
      throw new Error("Mock xterm DOM was not mounted");
    }

    mockElementBox(terminalFrame, {
      height: 520,
      rectLeft: 100,
      rectTop: 80,
      width: 960,
    });
    mockElementBox(terminalContainer, {
      clientWidth: 900,
      offsetLeft: 20,
      offsetTop: 30,
    });
    mockElementBox(xtermScreen, {
      height: 480,
      rectLeft: 140,
      rectTop: 150,
      width: 800,
    });
    mockElementBox(rows, {
      height: 480,
      offsetLeft: 12,
      offsetTop: 6,
      rectLeft: 140,
      rectTop: 150,
      width: 800,
    });

    const terminal = mocks.terminalInstances[0];
    terminal.buffer.active.cursorX = 4;
    terminal.buffer.active.cursorY = 0;
    act(() => {
      terminal.onDataCallback?.("部");
      terminal.onDataCallback?.("署");
    });

    await waitFor(() => {
      expect(mocks.api.listTerminalSuggestions).toHaveBeenCalledWith(
        expect.objectContaining({
          cursor: 2,
          input: "部署",
        }),
      );
    });

    const ghostSuggestion = await screen.findByLabelText("终端命令灰色提示");
    expect(ghostSuggestion).toHaveStyle({
      left: "72px",
      top: "70px",
    });
  });

  it("keeps stable ghost suggestion layout refreshes out of React commits", async () => {
    mocks.api.listTerminalSuggestions.mockResolvedValue([
      {
        description: "历史命令，匹配当前目录",
        displayText: "git status --short",
        id: "history-1",
        provider: "history",
        replacementRange: { end: 3, start: 0 },
        replacementText: "git status --short",
        score: 0.9,
        sensitivity: "normal",
        sourceId: "history-1",
        suffix: " status --short",
      },
    ]);
    const onRender = vi.fn<ProfilerOnRenderCallback>();

    render(
      <Profiler id="xterm-pane" onRender={onRender}>
        <XtermPane
          cwd="C:/dev/rust/kerminal"
          focused
          paneId="pane-local"
          resolvedTheme="dark"
          terminalAppearance={defaultAppSettings.terminal}
          title="本地 PowerShell"
        />
      </Profiler>,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    const terminal = mocks.terminalInstances[0];
    terminal.buffer.active.cursorX = 3;
    act(() => {
      terminal.onDataCallback?.("g");
      terminal.onDataCallback?.("i");
      terminal.onDataCallback?.("t");
    });

    await screen.findByLabelText("终端命令灰色提示");
    act(() => {
      terminal.onWriteParsedCallback?.();
    });
    onRender.mockClear();

    act(() => {
      for (let index = 0; index < 20; index += 1) {
        terminal.onWriteParsedCallback?.();
      }
    });

    expect(onRender).not.toHaveBeenCalled();
  });

  it("records dismissed feedback when submitting a different command with a visible ghost suggestion", async () => {
    mocks.api.listTerminalSuggestions.mockResolvedValue([
      {
        description: "历史命令，匹配当前目录",
        displayText: "git status --short",
        id: "history-1",
        provider: "history",
        replacementRange: { end: 3, start: 0 },
        replacementText: "git status --short",
        score: 0.9,
        sensitivity: "normal",
        sourceId: "history-1",
        suffix: " status --short",
      },
    ]);

    render(
      <XtermPane
        cwd="C:/dev/rust/kerminal"
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

    const terminal = mocks.terminalInstances[0];
    terminal.buffer.active.cursorX = 3;
    act(() => {
      terminal.onDataCallback?.("g");
      terminal.onDataCallback?.("i");
      terminal.onDataCallback?.("t");
    });

    await screen.findByLabelText("终端命令灰色提示");

    act(() => {
      terminal.onDataCallback?.("\r");
    });

    expect(mocks.api.recordTerminalSuggestionFeedback).toHaveBeenCalledWith(
      expect.objectContaining({
        action: "dismissed",
        input: "git",
        provider: "history",
        replacementText: "git status --short",
        sourceId: "history-1",
        target: "local",
      }),
    );
  });

  it("hides ghost suggestions, suppresses requests, and recovers cleanly after alternate buffer programs", async () => {
    mocks.api.listTerminalSuggestions.mockResolvedValue([
      {
        description: "历史命令，匹配当前目录",
        displayText: "git status --short",
        id: "history-1",
        provider: "history",
        replacementRange: { end: 3, start: 0 },
        replacementText: "git status --short",
        score: 0.9,
        sensitivity: "normal",
        sourceId: "history-1",
        suffix: " status --short",
      },
    ]);

    render(
      <XtermPane
        cwd="C:/dev/rust/kerminal"
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

    const terminal = mocks.terminalInstances[0];
    terminal.buffer.active.cursorX = 3;
    act(() => {
      terminal.onDataCallback?.("g");
      terminal.onDataCallback?.("i");
      terminal.onDataCallback?.("t");
    });

    expect(await screen.findByLabelText("终端命令灰色提示")).toBeInTheDocument();

    act(() => {
      terminal.buffer.active = terminal.buffer.alternate;
      terminal.onBufferChangeCallback?.();
    });

    await waitFor(() => {
      expect(
        screen.queryByLabelText("终端命令灰色提示"),
      ).not.toBeInTheDocument();
    });

    mocks.api.listTerminalSuggestions.mockClear();
    act(() => {
      terminal.onDataCallback?.("x");
    });

    expect(mocks.api.writeTerminal).toHaveBeenCalledWith("session-1", "x");
    expect(mocks.api.listTerminalSuggestions).not.toHaveBeenCalled();

    act(() => {
      terminal.buffer.active = terminal.buffer.normal;
      terminal.onBufferChangeCallback?.();
      terminal.buffer.active.cursorX = 2;
      terminal.onDataCallback?.("l");
      terminal.onDataCallback?.("s");
    });

    await waitFor(() => {
      expect(mocks.api.listTerminalSuggestions).toHaveBeenCalledWith(
        expect.objectContaining({
          input: "ls",
        }),
      );
    });
    expect(mocks.api.listTerminalSuggestions).not.toHaveBeenCalledWith(
      expect.objectContaining({
        input: expect.stringContaining("x"),
      }),
    );
  });

});
