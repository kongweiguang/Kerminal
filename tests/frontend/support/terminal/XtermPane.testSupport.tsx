// @author kongweiguang

import { afterEach, beforeEach, vi } from "vitest";
import { terminalSuggestionProbeScheduler } from "../../../../src/features/terminal/terminalSuggestionProbeScheduler";
import type { TerminalOutputEvent } from "../../../../src/lib/terminalApi";
const mocks = vi.hoisted(() => {
  const terminalInstances: MockTerminal[] = [];
  const fitInstances: MockFitAddon[] = [];
  const searchInstances: MockSearchAddon[] = [];
  const api = {
    closeExternalSshLaunch: vi.fn(),
    closeTerminal: vi.fn(),
    createSerialTerminalSession: vi.fn(),
    createSshTerminalSession: vi.fn(),
    createTelnetTerminalSession: vi.fn(),
    createTerminalSession: vi.fn(),
    getTerminalLogState: vi.fn(),
    listTerminalSessions: vi.fn(),
    listTerminalSuggestions: vi.fn(),
    recordCommandHistory: vi.fn(),
    recordTerminalSuggestionAuditEvent: vi.fn(),
    recordTerminalSuggestionFeedback: vi.fn(),
    registerTerminalSessionBinding: vi.fn(),
    refreshTerminalGitSuggestions: vi.fn(),
    refreshTerminalRemoteCommandSuggestions: vi.fn(),
    refreshTerminalRemoteHistorySuggestions: vi.fn(),
    refreshTerminalRemotePathSuggestions: vi.fn(),
    markTerminalSessionBindingDisconnected: vi.fn(),
    markTerminalSessionBindingReady: vi.fn(),
    closeTerminalSessionBinding: vi.fn(),
    readTerminalClipboardText: vi.fn(),
    requestSshAuthPrompt: vi.fn(),
    submitSshAuthPromptResponse: vi.fn(),
    writeDesktopClipboardText: vi.fn(),
    resizeTerminal: vi.fn(),
    startTerminalLog: vi.fn(),
    stopTerminalLog: vi.fn(),
    writeTerminal: vi.fn(),
  };
  let latestOutputHandler: ((event: TerminalOutputEvent) => void) | undefined;
  class MockTerminal {
    buffer: {
      active: {
        baseY: number;
        cursorX: number;
        cursorY: number;
        getLine: ReturnType<typeof vi.fn>;
        getNullCell: ReturnType<typeof vi.fn>;
        length: number;
        type: "normal" | "alternate";
        viewportY: number;
      };
      alternate: {
        baseY: number;
        cursorX: number;
        cursorY: number;
        getLine: ReturnType<typeof vi.fn>;
        getNullCell: ReturnType<typeof vi.fn>;
        length: number;
        type: "normal" | "alternate";
        viewportY: number;
      };
      normal: {
        baseY: number;
        cursorX: number;
        cursorY: number;
        getLine: ReturnType<typeof vi.fn>;
        getNullCell: ReturnType<typeof vi.fn>;
        length: number;
        type: "normal" | "alternate";
        viewportY: number;
      };
      onBufferChange: ReturnType<typeof vi.fn>;
    };
    cols = 80;
    clear = vi.fn();
    rows = 24;
    customKeyEventHandler:
      | ((event: KeyboardEvent) => boolean)
      | undefined;
    dispose = vi.fn();
    focus = vi.fn();
    getSelection = vi.fn(() => "selected output");
    loadAddon = vi.fn((addon: { activate?: (terminal: MockTerminal) => void }) => {
      addon.activate?.(this);
    });
    onDataCallback: ((data: string) => void) | undefined;
    onKeyCallback: ((event: { key: string; domEvent: KeyboardEvent }) => void) | undefined;
    modes = { bracketedPasteMode: false };
    onScrollCallback: ((viewportY: number) => void) | undefined;
    onSelectionChangeCallback: (() => void) | undefined;
    onWriteParsedCallback: (() => void) | undefined;
    open = vi.fn((container: HTMLElement) => {
      const screen = document.createElement("div");
      screen.className = "xterm-screen";
      const rows = document.createElement("div");
      rows.className = "xterm-rows";
      for (let index = 0; index < 30; index += 1) {
        const row = document.createElement("div");
        row.textContent = `row ${index}`;
        rows.append(row);
      }
      screen.append(rows);
      container.append(screen);
    });
    options: Record<string, unknown>;
    parser: {
      csiHandlers: Map<string, (params: Array<number | number[]>) => boolean>;
      escHandlers: Map<string, () => boolean>;
      oscHandlers: Map<number, (data: string) => boolean>;
      registerCsiHandler: ReturnType<typeof vi.fn>;
      registerEscHandler: ReturnType<typeof vi.fn>;
      registerOscHandler: ReturnType<typeof vi.fn>;
    };
    paste = vi.fn();
    refresh = vi.fn();
    /** 只注册 provider，不在组件测试中伪造 URL 解析。 */
    registerLinkProvider = vi.fn((_provider: unknown) => ({ dispose: vi.fn() }));
    private nextMarkerId = 1;
    selectAll = vi.fn();
    write = vi.fn((_data: string) => undefined);
    constructor(options: Record<string, unknown>) {
      this.options = { ignoreBracketedPasteMode: false, ...options };
      const csiHandlers = new Map<
        string,
        (params: Array<number | number[]>) => boolean
      >();
      const escHandlers = new Map<string, () => boolean>();
      const oscHandlers = new Map<number, (data: string) => boolean>();
      this.parser = {
        csiHandlers,
        escHandlers,
        oscHandlers,
        registerCsiHandler: vi.fn(
          (
            identifier: { final: string; prefix?: string },
            handler: (params: Array<number | number[]>) => boolean,
          ) => {
            const key = `${identifier.prefix ?? ""}${identifier.final}`;
            csiHandlers.set(key, handler);
            return {
              dispose: vi.fn(() => {
                csiHandlers.delete(key);
              }),
            };
          },
        ),
        registerEscHandler: vi.fn(
          (identifier: { final: string }, handler: () => boolean) => {
            escHandlers.set(identifier.final, handler);
            return {
              dispose: vi.fn(() => {
                escHandlers.delete(identifier.final);
              }),
            };
          },
        ),
        registerOscHandler: vi.fn(
          (identifier: number, handler: (data: string) => boolean) => {
            oscHandlers.set(identifier, handler);
            return {
              dispose: vi.fn(() => {
                oscHandlers.delete(identifier);
              }),
            };
          },
        ),
      };
      const normalBuffer = {
        baseY: 0,
        cursorX: 0,
        cursorY: 0,
        getLine: vi.fn(),
        getNullCell: vi.fn(),
        length: 30,
        type: "normal" as const,
        viewportY: 0,
      };
      const alternateBuffer = {
        baseY: 0,
        cursorX: 0,
        cursorY: 0,
        getLine: vi.fn(),
        getNullCell: vi.fn(),
        length: 30,
        type: "alternate" as const,
        viewportY: 0,
      };
      this.buffer = {
        active: normalBuffer,
        alternate: alternateBuffer,
        normal: normalBuffer,
        onBufferChange: vi.fn((callback: () => void) => {
          this.onBufferChangeCallback = callback;
          return { dispose: vi.fn() };
        }),
      };
      terminalInstances.push(this);
    }
    attachCustomKeyEventHandler(handler: (event: KeyboardEvent) => boolean) {
      this.customKeyEventHandler = handler;
    }
    /** 按真实 xterm 的同步 onKey→onData 次序发送完整编码，避免把协议回包当键盘。 */
    triggerKeyData(key: string) {
      this.onKeyCallback?.({ key, domEvent: new KeyboardEvent("keydown") });
      this.onDataCallback?.(key);
    }
    /** 公共事件订阅保留注册身份，旧 disposer 不影响后注册回调。 */
    onKey(callback: (event: { key: string; domEvent: KeyboardEvent }) => void) {
      this.onKeyCallback = callback;
      return { dispose: vi.fn(() => {
        if (this.onKeyCallback === callback) this.onKeyCallback = undefined;
      }) };
    }
    onData(callback: (data: string) => void) {
      this.onDataCallback = callback;
      return { dispose: vi.fn(() => {
        if (this.onDataCallback === callback) this.onDataCallback = undefined;
      }) };
    }
    onSelectionChange(callback: () => void) {
      this.onSelectionChangeCallback = callback;
      return { dispose: vi.fn() };
    }
    onBufferChangeCallback: (() => void) | undefined;
    onScroll(callback: (viewportY: number) => void) {
      this.onScrollCallback = callback;
      return { dispose: vi.fn() };
    }
    onWriteParsed(callback: () => void) {
      this.onWriteParsedCallback = callback;
      return { dispose: vi.fn() };
    }
    registerMarker(cursorYOffset = 0) {
      const activeBuffer = this.buffer.active;
      const line = Math.max(
        0,
        activeBuffer.baseY + activeBuffer.cursorY + cursorYOffset,
      );
      const disposeListeners = new Set<() => void>();
      const marker = {
        dispose: vi.fn(() => {
          if (marker.isDisposed) {
            return;
          }
          marker.isDisposed = true;
          marker.line = -1;
          for (const listener of disposeListeners) {
            listener();
          }
          disposeListeners.clear();
        }),
        id: this.nextMarkerId,
        isDisposed: false,
        line,
        onDispose: vi.fn((listener: () => void) => {
          if (marker.isDisposed) {
            listener();
            return { dispose: vi.fn() };
          }
          disposeListeners.add(listener);
          return {
            dispose: vi.fn(() => {
              disposeListeners.delete(listener);
            }),
          };
        }),
      };
      this.nextMarkerId += 1;
      this.buffer.active.length = Math.max(
        this.buffer.active.length,
        marker.line + 4,
      );
      return marker;
    }
    triggerEsc(final: string) {
      return this.parser.escHandlers.get(final)?.();
    }
    triggerCsi(
      final: string,
      params: Array<number | number[]> = [0],
      prefix = "",
    ) {
      return this.parser.csiHandlers.get(`${prefix}${final}`)?.(params);
    }
    triggerOsc(identifier: number, data: string) {
      return this.parser.oscHandlers.get(identifier)?.(data);
    }
    emitSelectionChange() {
      this.onSelectionChangeCallback?.();
    }
    triggerCustomKeyEvent(init: KeyboardEventInit) {
      const event = new KeyboardEvent("keydown", {
        bubbles: true,
        cancelable: true,
        ...init,
      });
      const result = this.customKeyEventHandler?.(event);
      return {
        defaultPrevented: event.defaultPrevented,
        result,
      };
    }
  }
  class MockFitAddon {
    private terminal: MockTerminal | null = null;
    activate(terminal: MockTerminal) {
      this.terminal = terminal;
    }
    dispose = vi.fn();
    fit = vi.fn(() => {
      const dimensions = this.proposeDimensions();
      if (this.terminal && dimensions) {
        this.terminal.cols = dimensions.cols;
        this.terminal.rows = dimensions.rows;
      }
    });
    proposeDimensions = vi.fn(() => ({ cols: 100, rows: 30 }));

    constructor() {
      fitInstances.push(this);
    }
  }

  class MockSearchAddon {
    clearDecorations = vi.fn();
    dispose = vi.fn();
    findNext = vi.fn(() => true);
    findPrevious = vi.fn(() => true);
    listener:
      | ((event: { resultCount: number; resultIndex: number }) => void)
      | undefined;
    onDidChangeResults = vi.fn(
      (
        listener: (event: { resultCount: number; resultIndex: number }) => void,
      ) => {
        this.listener = listener;
        return { dispose: vi.fn() };
      },
    );

    constructor() {
      searchInstances.push(this);
    }

    emitResults(resultCount: number, resultIndex: number) {
      this.listener?.({ resultCount, resultIndex });
    }
  }

  return {
    api,
    fitInstances,
    getLatestOutputHandler: () => latestOutputHandler,
    MockFitAddon,
    MockSearchAddon,
    MockTerminal,
    searchInstances,
    setLatestOutputHandler: (
      handler: ((event: TerminalOutputEvent) => void) | undefined,
    ) => {
      latestOutputHandler = handler;
    },
    terminalInstances,
  };
});

vi.mock("@xterm/xterm", () => ({
  Terminal: mocks.MockTerminal,
}));

vi.mock("@xterm/addon-fit", () => ({
  FitAddon: mocks.MockFitAddon,
}));

vi.mock("@xterm/addon-search", () => ({
  SearchAddon: mocks.MockSearchAddon,
}));

vi.mock("../../../../src/lib/terminalApi", () => ({
  closeTerminal: (...args: unknown[]) => mocks.api.closeTerminal(...args),
  createSerialTerminalSession: (...args: unknown[]) =>
    mocks.api.createSerialTerminalSession(...args),
  createSshTerminalSession: (...args: unknown[]) =>
    mocks.api.createSshTerminalSession(...args),
  createTelnetTerminalSession: (...args: unknown[]) =>
    mocks.api.createTelnetTerminalSession(...args),
  createTerminalSession: (...args: unknown[]) =>
    mocks.api.createTerminalSession(...args),
  getTerminalCommandError: (error: unknown) => {
    if (typeof error !== "object" || error === null) {
      return undefined;
    }
    return "terminalError" in error
      ? (error as { terminalError?: unknown }).terminalError
      : undefined;
  },
  getTerminalLogState: (...args: unknown[]) =>
    mocks.api.getTerminalLogState(...args),
  listTerminalSessions: (...args: unknown[]) =>
    mocks.api.listTerminalSessions(...args),
  readTerminalClipboardText: (...args: unknown[]) =>
    mocks.api.readTerminalClipboardText(...args),
  resizeTerminal: (...args: unknown[]) => mocks.api.resizeTerminal(...args),
  startTerminalLog: (...args: unknown[]) => mocks.api.startTerminalLog(...args),
  stopTerminalLog: (...args: unknown[]) => mocks.api.stopTerminalLog(...args),
  writeTerminal: (...args: unknown[]) => mocks.api.writeTerminal(...args),
}));

vi.mock("../../../../src/lib/externalLaunchApi", () => ({
  closeExternalSshLaunch: (...args: unknown[]) =>
    mocks.api.closeExternalSshLaunch(...args),
}));

vi.mock("../../../../src/features/ssh-auth/sshAuthPromptStore", () => ({
  requestSshAuthPrompt: (...args: unknown[]) =>
    mocks.api.requestSshAuthPrompt(...args),
}));

vi.mock("../../../../src/lib/sshAuthApi", () => ({
  submitSshAuthPromptResponse: (...args: unknown[]) =>
    mocks.api.submitSshAuthPromptResponse(...args),
}));

vi.mock("../../../../src/lib/commandHistoryApi", () => ({
  recordCommandHistory: (...args: unknown[]) =>
    mocks.api.recordCommandHistory(...args),
}));

vi.mock("../../../../src/lib/desktopClipboardApi", () => ({
  writeDesktopClipboardText: (...args: unknown[]) =>
    mocks.api.writeDesktopClipboardText(...args),
}));

vi.mock("../../../../src/lib/terminalSuggestionApi", () => ({
  listTerminalSuggestions: (...args: unknown[]) =>
    mocks.api.listTerminalSuggestions(...args),
  recordTerminalSuggestionFeedback: (...args: unknown[]) =>
    mocks.api.recordTerminalSuggestionFeedback(...args),
  recordTerminalSuggestionAuditEvent: (...args: unknown[]) =>
    mocks.api.recordTerminalSuggestionAuditEvent(...args),
  refreshTerminalGitSuggestions: (...args: unknown[]) =>
    mocks.api.refreshTerminalGitSuggestions(...args),
  refreshTerminalRemoteCommandSuggestions: (...args: unknown[]) =>
    mocks.api.refreshTerminalRemoteCommandSuggestions(...args),
  refreshTerminalRemoteHistorySuggestions: (...args: unknown[]) =>
    mocks.api.refreshTerminalRemoteHistorySuggestions(...args),
  refreshTerminalRemotePathSuggestions: (...args: unknown[]) =>
    mocks.api.refreshTerminalRemotePathSuggestions(...args),
}));

vi.mock("../../../../src/lib/paneSessionTraceApi", () => ({
  closeTerminalSessionBinding: (...args: unknown[]) =>
    mocks.api.closeTerminalSessionBinding(...args),
  markTerminalSessionBindingDisconnected: (...args: unknown[]) =>
    mocks.api.markTerminalSessionBindingDisconnected(...args),
  markTerminalSessionBindingReady: (...args: unknown[]) =>
    mocks.api.markTerminalSessionBindingReady(...args),
  registerTerminalSessionBinding: (...args: unknown[]) =>
    mocks.api.registerTerminalSessionBinding(...args),
}));

function installClipboardMock() {
  const clipboard = {
    readText: vi.fn().mockResolvedValue("echo pasted\r"),
    writeText: vi.fn().mockResolvedValue(undefined),
  };
  Object.defineProperty(navigator, "clipboard", {
    configurable: true,
    value: clipboard,
  });
  return clipboard;
}

function setTerminalBufferLines(
  terminal: (typeof mocks.terminalInstances)[number],
  lines: Record<number, string>,
  cursorY: number,
) {
  terminal.buffer.active.baseY = 0;
  terminal.buffer.active.cursorY = cursorY;
  terminal.buffer.active.length = Math.max(
    terminal.buffer.active.length,
    cursorY + 1,
    ...Object.keys(lines).map((line) => Number(line) + 1),
  );
  terminal.buffer.active.getLine.mockImplementation((line: number) => {
    const text = lines[line];
    if (typeof text !== "string") {
      return undefined;
    }
    return {
      translateToString: vi.fn(() => text),
    };
  });
}


beforeEach(() => {
  terminalSuggestionProbeScheduler.reset();
  mocks.terminalInstances.length = 0;
  mocks.fitInstances.length = 0;
  mocks.searchInstances.length = 0;
  mocks.setLatestOutputHandler(undefined);
  mocks.api.closeExternalSshLaunch.mockReset();
  mocks.api.createSerialTerminalSession.mockReset();
  mocks.api.createSshTerminalSession.mockReset();
  mocks.api.createTelnetTerminalSession.mockReset();
  mocks.api.createTerminalSession.mockReset();
  mocks.api.getTerminalLogState.mockReset();
  mocks.api.listTerminalSessions.mockReset();
  mocks.api.listTerminalSuggestions.mockReset();
  mocks.api.recordCommandHistory.mockReset();
  mocks.api.recordTerminalSuggestionAuditEvent.mockReset();
  mocks.api.recordTerminalSuggestionFeedback.mockReset();
  mocks.api.registerTerminalSessionBinding.mockReset();
  mocks.api.refreshTerminalGitSuggestions.mockReset();
  mocks.api.refreshTerminalRemoteCommandSuggestions.mockReset();
  mocks.api.refreshTerminalRemoteHistorySuggestions.mockReset();
  mocks.api.refreshTerminalRemotePathSuggestions.mockReset();
  mocks.api.markTerminalSessionBindingDisconnected.mockReset();
  mocks.api.markTerminalSessionBindingReady.mockReset();
  mocks.api.closeTerminalSessionBinding.mockReset();
  mocks.api.readTerminalClipboardText.mockReset();
  mocks.api.requestSshAuthPrompt.mockReset();
  mocks.api.submitSshAuthPromptResponse.mockReset();
  mocks.api.writeDesktopClipboardText.mockReset();
  mocks.api.writeTerminal.mockReset();
  mocks.terminalInstances.forEach((terminal) => terminal.write.mockReset());
  mocks.api.resizeTerminal.mockReset();
  mocks.api.closeTerminal.mockReset();
  mocks.api.startTerminalLog.mockReset();
  mocks.api.stopTerminalLog.mockReset();
  mocks.api.listTerminalSuggestions.mockResolvedValue([]);
  mocks.api.refreshTerminalGitSuggestions.mockResolvedValue({
    cachedAtUnixMs: 1760000000000,
    cwd: "/srv/app",
    entryCount: 0,
    hostId: "host-prod",
    ttlSeconds: 60,
  });
  mocks.api.refreshTerminalRemoteCommandSuggestions.mockResolvedValue({
    cachedAtUnixMs: 1760000000000,
    commandCount: 0,
    hostId: "host-prod",
    ttlSeconds: 300,
  });
  mocks.api.refreshTerminalRemoteHistorySuggestions.mockResolvedValue({
    cachedAtUnixMs: 1760000000000,
    commandCount: 0,
    hostId: "host-prod",
    ttlSeconds: 900,
  });
  mocks.api.refreshTerminalRemotePathSuggestions.mockResolvedValue({
    cachedAtUnixMs: 1760000000000,
    entryCount: 0,
    hostId: "host-prod",
    path: "/srv/app",
    ttlSeconds: 30,
  });
  mocks.api.createTerminalSession.mockImplementation(
    async (_request, onOutput) => {
      mocks.setLatestOutputHandler(
        onOutput as (event: TerminalOutputEvent) => void,
      );
      mocks.getLatestOutputHandler()?.({
        data: "hello from pty",
        kind: "data",
        sessionId: "session-1",
      });
      return {
        cols: 80,
        id: "session-1",
        rows: 24,
        shell: "powershell.exe",
        shellIntegration: { reason: "test default", status: "disabled" },
        status: "running",
      };
    },
  );
  mocks.api.createSshTerminalSession.mockImplementation(
    async (_request, onOutput) => {
      mocks.setLatestOutputHandler(
        onOutput as (event: TerminalOutputEvent) => void,
      );
      mocks.getLatestOutputHandler()?.({
        data: "hello from ssh",
        kind: "data",
        sessionId: "ssh-session-1",
      });
      return {
        cols: 80,
        id: "ssh-session-1",
        rows: 24,
        shell: "ssh",
        shellIntegration: { reason: "remote test default", status: "disabled" },
        status: "running",
      };
    },
  );
  mocks.api.createTelnetTerminalSession.mockImplementation(
    async (_request, onOutput) => {
      mocks.setLatestOutputHandler(
        onOutput as (event: TerminalOutputEvent) => void,
      );
      mocks.getLatestOutputHandler()?.({
        data: "hello from telnet",
        kind: "data",
        sessionId: "telnet-session-1",
      });
      return {
        cols: 80,
        id: "telnet-session-1",
        rows: 24,
        shell: "telnet",
        shellIntegration: { reason: "remote test default", status: "disabled" },
        status: "running",
      };
    },
  );
  mocks.api.createSerialTerminalSession.mockImplementation(
    async (_request, onOutput) => {
      mocks.setLatestOutputHandler(
        onOutput as (event: TerminalOutputEvent) => void,
      );
      mocks.getLatestOutputHandler()?.({
        data: "hello from serial",
        kind: "data",
        sessionId: "serial-session-1",
      });
      return {
        cols: 80,
        id: "serial-session-1",
        rows: 24,
        shell: "plink",
        shellIntegration: { reason: "remote test default", status: "disabled" },
        status: "running",
      };
    },
  );
  mocks.api.readTerminalClipboardText.mockResolvedValue("echo pasted\r");
  mocks.api.requestSshAuthPrompt.mockImplementation(async (options) => ({
    promptId: options.prompt.promptId,
    secretKind: options.prompt.secretKind,
  }));
  mocks.api.submitSshAuthPromptResponse.mockImplementation(async (request) => ({
    promptId: request.promptId,
    secretKind: request.secretKind,
  }));
  mocks.api.writeDesktopClipboardText.mockResolvedValue({ ok: true });
  mocks.api.writeTerminal.mockResolvedValue(undefined);
  mocks.api.resizeTerminal.mockResolvedValue(undefined);
  mocks.api.closeTerminal.mockResolvedValue(undefined);
  mocks.api.closeExternalSshLaunch.mockResolvedValue(undefined);
  mocks.api.getTerminalLogState.mockResolvedValue({
    active: false,
    bytesWritten: 0,
  });
  mocks.api.listTerminalSessions.mockResolvedValue([
    {
      cols: 80,
      id: "session-1",
      rows: 24,
      shell: "powershell.exe",
      shellIntegration: { reason: "test default", status: "disabled" },
      status: "running",
    },
    {
      cols: 80,
      id: "ssh-session-1",
      rows: 24,
      shell: "ssh",
      shellIntegration: { reason: "remote test default", status: "disabled" },
      status: "running",
    },
    {
      cols: 80,
      id: "telnet-session-1",
      rows: 24,
      shell: "telnet",
      shellIntegration: { reason: "remote test default", status: "disabled" },
      status: "running",
    },
    {
      cols: 80,
      id: "serial-session-1",
      rows: 24,
      shell: "plink",
      shellIntegration: { reason: "remote test default", status: "disabled" },
      status: "running",
    },
  ]);
  mocks.api.startTerminalLog.mockResolvedValue({
    active: true,
    bytesWritten: 128,
    path: "C:\\Users\\dev\\.kerminal\\logs\\sessions\\session-1.log",
    startedAt: "1760000000",
  });
  mocks.api.stopTerminalLog.mockResolvedValue({
    active: false,
    bytesWritten: 256,
    path: "C:\\Users\\dev\\.kerminal\\logs\\sessions\\session-1.log",
    startedAt: "1760000000",
  });
  mocks.api.recordCommandHistory.mockResolvedValue({
    entry: null,
    recorded: true,
    skipReason: null,
  });
  mocks.api.recordTerminalSuggestionFeedback.mockResolvedValue({
    recorded: true,
  });
  mocks.api.recordTerminalSuggestionAuditEvent.mockResolvedValue({
    eventId: "audit-1",
    recorded: true,
  });
  mocks.api.registerTerminalSessionBinding.mockResolvedValue(undefined);
  mocks.api.markTerminalSessionBindingDisconnected.mockResolvedValue(undefined);
  mocks.api.markTerminalSessionBindingReady.mockResolvedValue(undefined);
  mocks.api.closeTerminalSessionBinding.mockResolvedValue(undefined);
  installClipboardMock();
});

afterEach(() => {
  terminalSuggestionProbeScheduler.reset();
  vi.useRealTimers();
  vi.clearAllMocks();
});


export {
  installClipboardMock,
  mocks,
  setTerminalBufferLines,
};
