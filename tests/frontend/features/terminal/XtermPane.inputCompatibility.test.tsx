// @author kongweiguang
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { defaultAppSettings } from "../../../../src/features/settings/settingsModel";
import { mocks } from "../../support/terminal/XtermPane.testSupport.tsx";
import { XtermPane } from "../../../../src/features/terminal/XtermPane";

describe("XtermPane input compatibility", () => {
  it("reports a local input rejection without retrying the accepted bytes", async () => {
    render(
      <XtermPane focused paneId="pane-shell" resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal} title="PowerShell" />,
    );
    await screen.findByText("已连接");
    mocks.api.writeTerminal.mockRejectedValueOnce(new Error("PTY closed"));
    act(() => { mocks.terminalInstances[0].triggerKeyData("echo accepted\r"); });
    expect(await screen.findByText("终端输入发送失败，请检查连接。")).toBeInTheDocument();
    expect(mocks.api.writeTerminal.mock.calls).toEqual([["session-1", "echo accepted\r"]]);
  });

  it("maps Agent TUI Shift+Enter to LF without xterm default CR", async () => {
    render(
      <XtermPane
        focused
        inputCompatibilityMode="agentTui"
        paneId="pane-agent-codex"
        resolvedTheme="dark"
        shellAssistEnabled={false}
        terminalAppearance={defaultAppSettings.terminal}
        title="Codex"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    const result = mocks.terminalInstances[0].triggerCustomKeyEvent({
      code: "Enter",
      key: "Enter",
      keyCode: 13,
      shiftKey: true,
    });

    expect(result).toEqual({ defaultPrevented: true, result: false });
    expect(mocks.api.writeTerminal).toHaveBeenCalledWith("session-1", "\n");
    expect(mocks.api.writeTerminal).not.toHaveBeenCalledWith("session-1", "\r");
  });

  it("does not intercept Agent TUI Shift+Enter during IME composition", async () => {
    render(
      <XtermPane
        focused
        inputCompatibilityMode="agentTui"
        paneId="pane-agent-codex"
        resolvedTheme="dark"
        shellAssistEnabled={false}
        terminalAppearance={defaultAppSettings.terminal}
        title="Codex"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    const result = mocks.terminalInstances[0].triggerCustomKeyEvent({
      code: "Enter",
      isComposing: true,
      key: "Enter",
      keyCode: 13,
      shiftKey: true,
    });

    expect(result).toEqual({ defaultPrevented: false, result: true });
    expect(mocks.api.writeTerminal).not.toHaveBeenCalledWith(
      "session-1",
      "\n",
    );
  });

  it("maps shell Shift+Enter to LF for terminal apps started inside shells", async () => {
    render(
      <XtermPane
        focused
        paneId="pane-shell"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="PowerShell"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    const result = mocks.terminalInstances[0].triggerCustomKeyEvent({
      code: "Enter",
      key: "Enter",
      keyCode: 13,
      shiftKey: true,
    });

    expect(result).toEqual({ defaultPrevented: true, result: false });
    expect(mocks.api.writeTerminal).toHaveBeenCalledWith("session-1", "\n");
    expect(mocks.api.writeTerminal).not.toHaveBeenCalledWith("session-1", "\r");
  });

  it("captures real DOM Shift+Enter before xterm can turn it into CR", async () => {
    render(
      <XtermPane
        focused
        paneId="pane-shell"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="PowerShell"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });
    mocks.api.writeTerminal.mockClear();

    const event = new KeyboardEvent("keydown", {
      bubbles: true,
      cancelable: true,
      code: "Enter",
      key: "Enter",
      keyCode: 13,
      shiftKey: true,
    });
    screen.getByLabelText("PowerShell xterm 终端").dispatchEvent(event);

    expect(event.defaultPrevented).toBe(true);
    expect(mocks.api.writeTerminal).toHaveBeenCalledTimes(1);
    expect(mocks.api.writeTerminal).toHaveBeenCalledWith("session-1", "\n");
    expect(mocks.api.writeTerminal).not.toHaveBeenCalledWith("session-1", "\r");
  });

  it("lets shell paste shortcuts reach terminal apps instead of Kerminal text paste", async () => {
    render(
      <XtermPane
        focused
        paneId="pane-shell"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="PowerShell"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    const terminal = mocks.terminalInstances[0];
    const ctrlV = terminal.triggerCustomKeyEvent({
      code: "KeyV",
      ctrlKey: true,
      key: "v",
      keyCode: 86,
    });
    const ctrlShiftV = terminal.triggerCustomKeyEvent({
      code: "KeyV",
      ctrlKey: true,
      key: "V",
      keyCode: 86,
      shiftKey: true,
    });
    const shiftInsert = terminal.triggerCustomKeyEvent({
      code: "Insert",
      key: "Insert",
      keyCode: 45,
      shiftKey: true,
    });

    expect(ctrlV).toEqual({ defaultPrevented: false, result: true });
    expect(ctrlShiftV).toEqual({ defaultPrevented: false, result: true });
    expect(shiftInsert).toEqual({ defaultPrevented: false, result: true });
    expect(mocks.api.writeTerminal).not.toHaveBeenCalledWith("session-1", "\x16");
    expect(terminal.paste).not.toHaveBeenCalled();
    expect(mocks.api.readTerminalClipboardText).not.toHaveBeenCalled();
  });

  it("lets Agent TUI paste shortcuts reach Codex and Claude instead of Kerminal text paste", async () => {
    render(
      <XtermPane
        focused
        inputCompatibilityMode="agentTui"
        paneId="pane-agent-codex"
        resolvedTheme="dark"
        shellAssistEnabled={false}
        terminalAppearance={defaultAppSettings.terminal}
        title="Codex"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    const terminal = mocks.terminalInstances[0];
    const ctrlV = terminal.triggerCustomKeyEvent({
      code: "KeyV",
      ctrlKey: true,
      key: "v",
      keyCode: 86,
    });
    const ctrlShiftV = terminal.triggerCustomKeyEvent({
      code: "KeyV",
      ctrlKey: true,
      key: "V",
      keyCode: 86,
      shiftKey: true,
    });
    const shiftInsert = terminal.triggerCustomKeyEvent({
      code: "Insert",
      key: "Insert",
      keyCode: 45,
      shiftKey: true,
    });

    expect(ctrlV).toEqual({ defaultPrevented: false, result: true });
    expect(ctrlShiftV).toEqual({ defaultPrevented: false, result: true });
    expect(shiftInsert).toEqual({ defaultPrevented: false, result: true });
    expect(mocks.api.writeTerminal).not.toHaveBeenCalledWith("session-1", "\x16");
    expect(terminal.paste).not.toHaveBeenCalled();
    expect(mocks.api.readTerminalClipboardText).not.toHaveBeenCalled();
  });

  it("forwards large xterm input payloads to the backend as one write", async () => {
    render(
      <XtermPane
        focused
        inputCompatibilityMode="agentTui"
        paneId="pane-agent-large-paste"
        resolvedTheme="dark"
        shellAssistEnabled={false}
        terminalAppearance={defaultAppSettings.terminal}
        title="Codex"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });
    mocks.api.writeTerminal.mockClear();

    const payload = "p".repeat(8 * 1024);
    act(() => {
      mocks.terminalInstances[0].onDataCallback?.(payload);
    });

    expect(mocks.api.writeTerminal).toHaveBeenCalledTimes(1);
    expect(mocks.api.writeTerminal).toHaveBeenCalledWith("session-1", payload);
  });

  it("captures real DOM Ctrl+Shift+V as one PTY paste signal without browser paste duplication", async () => {
    render(
      <XtermPane
        focused
        paneId="pane-agent-codex"
        resolvedTheme="dark"
        shellAssistEnabled={false}
        terminalAppearance={defaultAppSettings.terminal}
        title="Codex"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });
    mocks.api.writeTerminal.mockClear();

    const event = new KeyboardEvent("keydown", {
      bubbles: true,
      cancelable: true,
      code: "KeyV",
      ctrlKey: true,
      key: "v",
      keyCode: 86,
      shiftKey: true,
    });
    const terminalContainer = screen.getByLabelText("Codex xterm 终端");
    terminalContainer.dispatchEvent(event);

    expect(event.defaultPrevented).toBe(true);
    expect(mocks.api.writeTerminal).toHaveBeenCalledTimes(1);
    expect(mocks.api.writeTerminal).toHaveBeenCalledWith("session-1", "\x16");

    const pasteEvent = new Event("paste", {
      bubbles: true,
      cancelable: true,
    });
    terminalContainer.dispatchEvent(pasteEvent);

    expect(pasteEvent.defaultPrevented).toBe(true);
    expect(mocks.api.writeTerminal).toHaveBeenCalledTimes(1);
    expect(mocks.terminalInstances[0].paste).not.toHaveBeenCalled();
    expect(mocks.api.readTerminalClipboardText).not.toHaveBeenCalled();

    const unrelatedPasteEvent = new Event("paste", {
      bubbles: true,
      cancelable: true,
    });
    terminalContainer.dispatchEvent(unrelatedPasteEvent);

    expect(unrelatedPasteEvent.defaultPrevented).toBe(false);
  });

  it("maps Claude image paste Alt+V to ESC v", async () => {
    render(
      <XtermPane
        focused
        inputCompatibilityMode="agentTui"
        paneId="pane-agent-claude"
        resolvedTheme="dark"
        shellAssistEnabled={false}
        terminalAppearance={defaultAppSettings.terminal}
        title="Claude"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    const result = mocks.terminalInstances[0].triggerCustomKeyEvent({
      altKey: true,
      code: "KeyV",
      key: "v",
      keyCode: 86,
    });

    expect(result).toEqual({ defaultPrevented: true, result: false });
    expect(mocks.api.writeTerminal).toHaveBeenCalledWith("session-1", "\x1bv");
    expect(mocks.api.readTerminalClipboardText).not.toHaveBeenCalled();
  });

  it("reports fitted terminal dimensions to callers", async () => {
    const onTerminalDimensionsChange = vi.fn();

    render(
      <XtermPane
        focused
        onTerminalDimensionsChange={onTerminalDimensionsChange}
        paneId="pane-agent-codex"
        resolvedTheme="dark"
        shellAssistEnabled={false}
        terminalAppearance={defaultAppSettings.terminal}
        title="Codex"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    await waitFor(() => {
      expect(onTerminalDimensionsChange).toHaveBeenCalledWith({
        cols: 100,
        rows: 30,
      });
    });
    expect(mocks.api.resizeTerminal).not.toHaveBeenCalledWith("session-1", {
      cols: 100,
      rows: 30,
    });
  });

  it("focuses the terminal from focus requests and terminal-area pointer input", async () => {
    const { rerender } = render(
      <XtermPane
        focused={false}
        focusRequestToken={0}
        paneId="pane-agent-codex"
        resolvedTheme="dark"
        shellAssistEnabled={false}
        terminalAppearance={defaultAppSettings.terminal}
        title="Codex"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });
    const terminal = mocks.terminalInstances[0];
    terminal.focus.mockClear();

    rerender(
      <XtermPane
        focused={false}
        focusRequestToken={1}
        paneId="pane-agent-codex"
        resolvedTheme="dark"
        shellAssistEnabled={false}
        terminalAppearance={defaultAppSettings.terminal}
        title="Codex"
      />,
    );

    expect(terminal.focus).toHaveBeenCalledTimes(1);
    terminal.focus.mockClear();

    fireEvent.pointerDown(screen.getByLabelText("Codex xterm 终端"));

    expect(terminal.focus).toHaveBeenCalledTimes(1);
  });

  it("pastes external Agent composer requests through xterm and optionally submits", async () => {
    const { rerender } = render(
      <XtermPane
        focused
        inputRequest={null}
        paneId="pane-agent-codex"
        resolvedTheme="dark"
        shellAssistEnabled={false}
        terminalAppearance={defaultAppSettings.terminal}
        title="Codex"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });
    const terminal = mocks.terminalInstances[0];
    terminal.paste.mockClear();
    terminal.focus.mockClear();
    mocks.api.writeTerminal.mockClear();

    rerender(
      <XtermPane
        focused
        inputRequest={{ id: "paste-1", text: "line1\nline2" }}
        paneId="pane-agent-codex"
        resolvedTheme="dark"
        shellAssistEnabled={false}
        terminalAppearance={defaultAppSettings.terminal}
        title="Codex"
      />,
    );

    expect(terminal.paste).toHaveBeenCalledWith("line1\nline2");
    expect(mocks.api.writeTerminal).not.toHaveBeenCalledWith("session-1", "\r");
    expect(terminal.focus).toHaveBeenCalledTimes(1);

    rerender(
      <XtermPane
        focused
        inputRequest={{ id: "submit-1", submit: true, text: "run tests" }}
        paneId="pane-agent-codex"
        resolvedTheme="dark"
        shellAssistEnabled={false}
        terminalAppearance={defaultAppSettings.terminal}
        title="Codex"
      />,
    );

    expect(terminal.paste).toHaveBeenLastCalledWith("run tests");
    expect(mocks.api.writeTerminal).toHaveBeenCalledWith("session-1", "\r");
  });

  it("writes the kitty keyboard protocol enable sequence for Agent TUI mode", async () => {
    render(
      <XtermPane
        focused
        inputCompatibilityMode="agentTui"
        paneId="pane-agent-codex-kitty"
        resolvedTheme="dark"
        shellAssistEnabled={false}
        terminalAppearance={defaultAppSettings.terminal}
        title="Codex"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    const terminal = mocks.terminalInstances[0];
    expect(terminal.write).toHaveBeenCalledWith("\x1b[>1u");
    expect(
      (terminal.options as { modifyOtherKeys?: number }).modifyOtherKeys,
    ).toBe(2);
  });

  it("does not write the kitty keyboard protocol enable sequence for shell mode", async () => {
    render(
      <XtermPane
        focused
        paneId="pane-shell-kitty"
        resolvedTheme="dark"
        terminalAppearance={defaultAppSettings.terminal}
        title="PowerShell"
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("已连接")).toBeInTheDocument();
    });

    const terminal = mocks.terminalInstances[0];
    expect(terminal.write).not.toHaveBeenCalledWith("\x1b[>1u");
    expect(
      (terminal.options as { modifyOtherKeys?: number }).modifyOtherKeys,
    ).toBe(0);
  });
});
