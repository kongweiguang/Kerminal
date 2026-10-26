// @author kongweiguang

import { expect, vi, type Mock } from "vitest";
import type { Terminal as XtermTerminal } from "@xterm/xterm";
import { createTerminalInputModelState, type TerminalInputModelState } from "../../../../src/features/terminal/terminalInputModel";
import type { createTerminalOutputWriter } from "../../../../src/features/terminal/terminalOutputWriter";
import { createTerminalShellIntegrationState } from "../../../../src/features/terminal/terminalShellIntegrationModel";
import * as terminalApi from "../../../../src/lib/terminalApi";
import { registerXtermPaneRuntimeEvents } from "../../../../src/features/terminal/XtermPane.runtime.events";
import { terminalInputSync } from "../../../../src/features/terminal/terminalInputSync";
import { terminalChromeRuntimeStore } from "../../../../src/features/terminal/terminalChromeRuntimeStore";
import { registerTerminalPaneSession, unregisterTerminalPaneSession } from "../../../../src/features/terminal/terminalSessionRegistry";

/** @author kongweiguang 等待实际解析事件而非假定写入已同步完成。 */
export function waitForNextParsedWrite(
  terminal: Awaited<ReturnType<typeof createRealXtermHarness>>["terminal"],
) {
  return new Promise<void>((resolve) => {
    const subscription: { disposable?: { dispose: () => void } } = {};
    subscription.disposable = terminal.onWriteParsed(() => {
      subscription.disposable?.dispose();
      resolve();
    });
  });
}

/** @author kongweiguang 有界等待 writer 的在途写入与队列同时清空。 */
export async function waitForWriterIdle(
  writer: ReturnType<typeof createTerminalOutputWriter>,
) {
  for (let index = 0; index < 100; index += 1) {
    if (!writer.stats().inFlight && writer.pendingLength() === 0) {
      return;
    }
    await new Promise((resolve) => window.setTimeout(resolve, 0));
  }
  throw new Error("terminal writer did not become idle");
}

/**
 * @author kongweiguang
 * 使用项目锁定的真实 xterm 包而非 mock；proposed API 只由显式测试开启，确保其它
 * 兼容用例仍覆盖默认安全门禁，关键词装饰用例则验证生产所需能力确实可注册。
 */
export async function createRealXtermHarness(options?: {
  allowProposedApi?: boolean;
  runtimePaneId?: string;
  rows?: number;
  scrollback?: number;
  theme?: { background: string; foreground: string };
}) {
  installBrowserApiStubs();

  const { Terminal: XtermTerminal } = await import("@xterm/xterm");
  const container = document.createElement("div");
  document.body.appendChild(container);
  const terminal = new XtermTerminal({
    ...(options?.allowProposedApi ? { allowProposedApi: true } : {}),
    cols: 80,
    rows: options?.rows ?? 24,
    scrollback: options?.scrollback ?? 1000,
    theme: options?.theme,
  });
  const data: string[] = [];
  terminal.onData((entry) => data.push(entry));
  terminal.open(container);
  const runtime = options?.runtimePaneId
    ? installRealInputRuntime(terminal, container, options.runtimePaneId)
    : undefined;

  return {
    container,
    runtime,
    data,
    dispose() {
      runtime?.dispose();
      terminal.dispose();
      container.remove();
    },
    terminal,
    write(entry: string) {
      return new Promise<void>((resolve) => terminal.write(entry, resolve));
    },
  };
}

/** @author kongweiguang 仅固定布局数据，保留真实 xterm 事件逻辑。 */
export function stubElementRect(
  element: HTMLElement,
  rect: Pick<DOMRect, "height" | "left" | "top" | "width">,
) {
  element.style.paddingLeft = "0px";
  element.style.paddingTop = "0px";
  element.getBoundingClientRect = () =>
    ({
      bottom: rect.top + rect.height,
      height: rect.height,
      left: rect.left,
      right: rect.left + rect.width,
      top: rect.top,
      width: rect.width,
      x: rect.left,
      y: rect.top,
      toJSON: () => ({}),
    }) as DOMRect;
}

/** @author kongweiguang 补齐 jsdom 缺少的绘制与尺寸 API。 */
function installBrowserApiStubs() {
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    value: () => ({
      addEventListener: vi.fn(),
      addListener: vi.fn(),
      dispatchEvent: vi.fn(() => false),
      matches: false,
      media: "",
      onchange: null,
      removeEventListener: vi.fn(),
      removeListener: vi.fn(),
    }),
  });
  Object.defineProperty(window, "ResizeObserver", {
    configurable: true,
    value: class ResizeObserver {
      disconnect() {}
      observe() {}
      unobserve() {}
    },
  });
  Object.defineProperty(globalThis, "ResizeObserver", {
    configurable: true,
    value: window.ResizeObserver,
  });

  vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue(
    createCanvasContextStub() as unknown as CanvasRenderingContext2D,
  );
  vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockReturnValue(320);
  vi.spyOn(HTMLElement.prototype, "offsetHeight", "get").mockReturnValue(16);
}

/** @author kongweiguang 绘图调用保持惰性，不替代终端解析与输入。 */
function createCanvasContextStub() {
  return {
    beginPath: vi.fn(),
    clearRect: vi.fn(),
    clip: vi.fn(),
    closePath: vi.fn(),
    createLinearGradient: vi.fn(() => ({ addColorStop: vi.fn() })),
    drawImage: vi.fn(),
    fillRect: vi.fn(),
    fillText: vi.fn(),
    getImageData: vi.fn(() => ({ data: new Uint8ClampedArray(4) })),
    measureText: vi.fn(() => ({ width: 10 })),
    putImageData: vi.fn(),
    rect: vi.fn(),
    restore: vi.fn(),
    save: vi.fn(),
    scale: vi.fn(),
    setTransform: vi.fn(),
    strokeRect: vi.fn(),
    strokeText: vi.fn(),
    translate: vi.fn(),
  };
}

/** @author kongweiguang Real xterm owns event production; only business services are inert. */
function installRealInputRuntime(
  terminal: XtermTerminal,
  container: HTMLElement,
  paneId: string,
) {
  type Params = Parameters<typeof registerXtermPaneRuntimeEvents>[0];
  const inputModelRef = { current: createTerminalInputModelState() };
  const sessionIdRef = { current: `${paneId}-session` as string | null };
  const registerCommandBlock = vi.fn();
  const auth = { active: false };
  let shellState = createTerminalShellIntegrationState();
  const removeChrome = terminalChromeRuntimeStore.register(paneId, { connectionState: "connected" });
  registerTerminalPaneSession(paneId, sessionIdRef.current!, { tabId: "real-input-tab" });
  const events = registerXtermPaneRuntimeEvents({
    terminal, container, paneId, compositionTarget: terminal.textarea!,
    sessionIdRef, inputModelRef, inputBufferRef: { current: "" },
    currentCwdRef: { current: undefined }, assistEnabled: false,
    inputCompatibilityMode: "shell",
    activityRuntimeRef: { current: null },
    terminalRuntimeLifecycleControllerRef: { current: null },
    ghostSuggestionRef: { current: null },
    shellIntegrationCommandBlockProtocolRef: { current: false },
    terminalAppearanceRef: { current: { selectionCopy: false } as Params["terminalAppearanceRef"]["current"] },
    terminalInlineSshAuthPrompt: {
      handleInput: () => auth.active,
      isActive: () => auth.active,
    } as unknown as Params["terminalInlineSshAuthPrompt"],
    commandBlockRuntime: {
      registerCommandBlock,
      hasOpenCommandBlock: () => false,
      setInteractiveTuiActive: vi.fn(),
      scheduleCommandBlockViewSync: vi.fn(),
      clearCommandBlockViewSyncFrame: vi.fn(),
      syncCommandBlockRuntimeContext: vi.fn(),
      closeCurrentCommandBlock: vi.fn(),
    } as unknown as Params["commandBlockRuntime"],
    ghostSuggestions: {
      acceptGhostSuggestion: () => false,
      isCommittingSuggestion: () => false,
      handleRuntimeKeyEvent: () => false,
      clearGhostSuggestion: vi.fn(), scheduleGhostSuggestion: vi.fn(),
      refreshGhostSuggestionLayout: vi.fn(), setLifecycle: vi.fn(),
    } as unknown as Params["ghostSuggestions"],
    searchAddon: { onDidChangeResults: () => ({ dispose() {} }) } as unknown as Params["searchAddon"],
    setSearchResults: vi.fn(), syncCommandBlockViews: vi.fn(),
    readShellIntegrationState: () => shellState,
    writeShellIntegrationState: (state) => { shellState = state; },
    onInputWriteError: vi.fn(),
  });
  return {
    auth, inputModelRef, registerCommandBlock, sessionIdRef,
    dispose() {
      events.dispose();
      unregisterTerminalPaneSession(paneId);
      removeChrome();
    },
  };
}

/** @author kongweiguang Drain controller FIFOs without assuming PTY packet boundaries. */
export async function settleInputQueues() {
  for (let index = 0; index < 40; index += 1) await Promise.resolve();
}

/** @author kongweiguang Send browser keyboard events through xterm's public textarea. */
export function realKey(terminal: XtermTerminal, key: string, keyCode: number, extra: KeyboardEventInit = {}) {
  terminal.textarea!.dispatchEvent(new KeyboardEvent("keydown", {
    bubbles: true, cancelable: true, key, keyCode, which: keyCode, ...extra,
  }));
}

/** @author kongweiguang Clipboard data is supplied by the browser, never an onData echo. */
export function realPaste(terminal: XtermTerminal, text: string) {
  const event = new Event("paste", { bubbles: true, cancelable: true });
  Object.defineProperty(event, "clipboardData", { value: { getData: () => text } });
  terminal.textarea!.dispatchEvent(event);
  expect(event.defaultPrevented).toBe(true);
}

/** @author kongweiguang Public resources retained by a real terminal test. */
interface RealInputHarness {
  terminal: XtermTerminal;
  container: HTMLElement;
  data: string[];
  write(data: string): Promise<void>;
  dispose(): void;
  runtime?: {
    auth: { active: boolean };
    inputModelRef: { current: TerminalInputModelState };
    sessionIdRef: { current: string | null };
    registerCommandBlock: Mock;
    dispose(): void;
  };
}

/** @author kongweiguang Isolate registry, chrome, singleton scope and PTY boundary per pair. */
export async function withRealInputPair(action: (
  a: RealInputHarness,
  b: RealInputHarness,
  bytes: (paneId: string) => string,
) => Promise<void>) {
  const writes = vi.spyOn(terminalApi, "writeTerminal").mockResolvedValue(undefined);
  const a = await createRealXtermHarness({ runtimePaneId: "real-a" });
  const b = await createRealXtermHarness({ runtimePaneId: "real-b" });
  try {
    terminalInputSync.configure("real-input-tab", ["real-a", "real-b"]);
    terminalInputSync.setEnabled(true);
    expect(terminalInputSync.getSnapshot().enabled).toBe(true);
    await action(a, b, (paneId) => writes.mock.calls
      .filter(([sessionId]) => sessionId === `${paneId}-session`)
      .map(([, data]) => data).join(""));
  } finally {
    terminalInputSync.configure(null, []);
    a.dispose(); b.dispose();
    vi.useRealTimers();
    writes.mockRestore();
    vi.restoreAllMocks();
  }
}
