// @author kongweiguang
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  createTerminalInputSyncController,
  type TerminalInputSyncBinding,
  type TerminalInputSyncController,
  type TerminalInputSyncEndpoint,
  type TerminalInputSyncInput,
} from "../../../../src/features/terminal/terminalInputSync";

const writeTerminalMock = vi.hoisted(() =>
  vi.fn<(sessionId: string, data: string) => Promise<void>>(),
);

vi.mock("../../../../src/lib/terminalApi", () => ({
  writeTerminal: writeTerminalMock,
}));

/** 来源提交只依赖 pane 身份与输入模型，不耦合注册辅助函数的返回类型。 */
interface InputModelPane {
  paneId: string;
  state: { model: string[] };
}

/** 可控 PTY 回执用于明确区分已接受输入、实际发送与迟到失败。 */
function deferredWrite() {
  let resolve!: () => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<void>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

/** 测试 endpoint 只持有当前 binding 和输入模型，模拟接收端自己的粘贴模式。 */
function registerPane(
  controller: TerminalInputSyncController,
  paneId: string,
  overrides: Partial<TerminalInputSyncBinding> = {},
) {
  const state: {
    binding: TerminalInputSyncBinding | null;
    bracketedPaste: boolean;
    model: string[];
  } = {
    binding: {
      sessionId: `session-${paneId}`,
      connectionGeneration: 1,
      tabId: "tab-1",
      ...overrides,
    },
    bracketedPaste: false,
    model: [],
  };
  const endpoint = {
    readBinding: vi.fn(() => state.binding),
    prepareMirroredInput: vi.fn((input: TerminalInputSyncInput) => {
      if (input.pasteText === undefined) {
        return input.data;
      }
      return state.bracketedPaste
        ? `\x1b[200~${input.pasteText}\x1b[201~`
        : input.pasteText;
    }),
    observeMirroredInput: vi.fn((data: string) => {
      state.model.push(data);
    }),
    onWriteError: vi.fn(),
  } satisfies TerminalInputSyncEndpoint;
  const dispose = controller.register(paneId, endpoint);
  return { paneId, state, endpoint, dispose };
}

/** 每个 pane 使用唯一 session，另设跨标签和无 tab 的独立来源。 */
function createHarness() {
  const controller = createTerminalInputSyncController();
  const a = registerPane(controller, "a");
  const b = registerPane(controller, "b");
  const c = registerPane(controller, "c");
  const otherTab = registerPane(controller, "other", { tabId: "tab-2" });
  const agent = registerPane(controller, "agent", { tabId: undefined });
  controller.configure("tab-1", ["a", "b"]);
  return { controller, a, b, c, otherTab, agent };
}

/** 与生产调用约定一致：来源模型在分发前更新一次，控制器只观察镜像。 */
function commitInput(
  controller: TerminalInputSyncController,
  pane: InputModelPane,
  data: string,
): Promise<void> {
  pane.state.model.push(data);
  return controller.writeUserInput(pane.paneId, { data });
}

describe("terminalInputSync", () => {
  beforeEach(() => {
    writeTerminalMock.mockReset();
    writeTerminalMock.mockResolvedValue(undefined);
  });

  it("starts disabled and keeps snapshots stable until their public state changes", async () => {
    const controller = createTerminalInputSyncController();
    const listener = vi.fn();
    const unsubscribe = controller.subscribe(listener);
    const initial = controller.getSnapshot();

    expect(initial).toEqual({
      enabled: false,
      disabledReason: "请至少选择两个终端分屏",
      notice: null,
    });
    controller.refresh();
    controller.configure(null, []);
    controller.setEnabled(true);
    expect(controller.getSnapshot()).toBe(initial);
    expect(listener).not.toHaveBeenCalled();

    registerPane(controller, "a");
    registerPane(controller, "b");
    controller.configure("tab-1", ["a", "b"]);
    const eligible = controller.getSnapshot();
    expect(eligible).not.toBe(initial);
    expect(listener).toHaveBeenCalledTimes(1);
    controller.setEnabled(true);
    const enabled = controller.getSnapshot();
    expect(listener).toHaveBeenCalledTimes(2);

    controller.refresh();
    controller.setEnabled(true);
    controller.configure("tab-1", ["b", "a", "a"]);
    await controller.writeUserInput("a", { data: " " });
    expect(controller.getSnapshot()).toBe(enabled);
    expect(listener).toHaveBeenCalledTimes(2);

    unsubscribe();
    controller.setEnabled(false);
    expect(listener).toHaveBeenCalledTimes(2);
  });

  it.each([{ selected: [] }, { selected: ["a"] }, { selected: ["a", "a"] }])(
    "does not enable fewer than two distinct selected panes: $selected",
    ({ selected }) => {
      const { controller } = createHarness();
      controller.configure("tab-1", selected);
      controller.setEnabled(true);
      expect(controller.getSnapshot()).toEqual({
        enabled: false,
        disabledReason: "请至少选择两个终端分屏",
        notice: null,
      });
    },
  );

  it("requires every selected endpoint and a valid binding in the configured tab", () => {
    const controller = createTerminalInputSyncController();
    registerPane(controller, "a");
    controller.configure("tab-1", ["a", "b"]);
    controller.setEnabled(true);
    expect(controller.getSnapshot().disabledReason).toBe("所选分屏尚未全部连接");

    const b = registerPane(controller, "b", { tabId: "tab-2" });
    controller.setEnabled(true);
    expect(controller.getSnapshot().enabled).toBe(false);
    b.state.binding = null;
    controller.refresh();
    expect(controller.getSnapshot().disabledReason).toBe("所选分屏尚未全部连接");

    b.state.binding = {
      sessionId: "session-b",
      connectionGeneration: 1,
      tabId: "tab-1",
    };
    controller.refresh();
    expect(controller.getSnapshot()).toEqual({
      enabled: false,
      disabledReason: null,
      notice: null,
    });
    controller.setEnabled(true);
    expect(controller.getSnapshot().enabled).toBe(true);
  });

  it("writes locally while disabled, including outside configured scope or without a tabId", async () => {
    const { controller, a, otherTab, agent } = createHarness();
    await commitInput(controller, a, "a");
    await commitInput(controller, otherTab, "other");
    await commitInput(controller, agent, "agent");
    controller.configure(null, []);
    await controller.writeUserInput("c", { data: "after owner unmount" });

    expect(writeTerminalMock.mock.calls).toEqual([
      ["session-a", "a"],
      ["session-other", "other"],
      ["session-agent", "agent"],
      ["session-c", "after owner unmount"],
    ]);
    expect(a.endpoint.observeMirroredInput).not.toHaveBeenCalled();
  });

  it("isolates the selected A/B group from C, other tabs, and agent input", async () => {
    const { controller, a, b, c, otherTab, agent } = createHarness();
    controller.setEnabled(true);
    await commitInput(controller, a, "a");
    await commitInput(controller, b, "b");
    await commitInput(controller, c, "c");
    await commitInput(controller, otherTab, "other");
    await commitInput(controller, agent, "agent");

    expect(a.state.model).toEqual(["a", "b"]);
    expect(b.state.model).toEqual(["a", "b"]);
    expect(c.state.model).toEqual(["c"]);
    expect(otherTab.state.model).toEqual(["other"]);
    expect(agent.state.model).toEqual(["agent"]);
    expect(writeTerminalMock.mock.calls.filter(([id]) => id === "session-a"))
      .toEqual([["session-a", "a"], ["session-a", "b"]]);
    expect(writeTerminalMock.mock.calls.filter(([id]) => id === "session-b"))
      .toEqual([["session-b", "a"], ["session-b", "b"]]);
    expect(c.endpoint.observeMirroredInput).not.toHaveBeenCalled();
    expect(otherTab.endpoint.observeMirroredInput).not.toHaveBeenCalled();
    expect(agent.endpoint.observeMirroredInput).not.toHaveBeenCalled();
    expect(controller.getSnapshot().enabled).toBe(true);
  });

  it("does not trim spaces or controls, and ignores only data with no content or paste body", async () => {
    const { controller, a, b } = createHarness();
    controller.setEnabled(true);
    await controller.writeUserInput("a", { data: "" });
    await controller.writeUserInput("a", { data: "", pasteText: "" });
    await controller.writeUserInput("missing", { data: "ignored" });
    expect(writeTerminalMock).not.toHaveBeenCalled();
    expect(b.endpoint.observeMirroredInput).not.toHaveBeenCalled();

    const data = " \t\r\n\b\x7f\x1b[A\x03\x04\x0c";
    await commitInput(controller, a, data);
    expect(writeTerminalMock).toHaveBeenCalledWith("session-a", data);
    expect(writeTerminalMock).toHaveBeenCalledWith("session-b", data);
    expect(a.state.model).toEqual([data]);
    expect(b.state.model).toEqual([data]);
  });

  it("prepares every mirror and observes models synchronously before the first PTY write", async () => {
    const { controller, a, b, c } = createHarness();
    const order: string[] = [];
    controller.configure("tab-1", ["a", "b", "c"]);
    controller.setEnabled(true);
    b.endpoint.prepareMirroredInput.mockImplementation((input) => {
      order.push("prepare-b");
      return input.data;
    });
    c.endpoint.prepareMirroredInput.mockImplementation((input) => {
      order.push("prepare-c");
      return input.data;
    });
    b.endpoint.observeMirroredInput.mockImplementation((data) => {
      order.push("observe-b");
      b.state.model.push(data);
    });
    c.endpoint.observeMirroredInput.mockImplementation((data) => {
      order.push("observe-c");
      c.state.model.push(data);
    });
    writeTerminalMock.mockImplementation(async (sessionId) => {
      order.push(`write-${sessionId}`);
    });

    const completion = commitInput(controller, a, "x");
    expect(order).toEqual([
      "prepare-b", "prepare-c", "observe-b", "observe-c",
      "write-session-a", "write-session-b", "write-session-c",
    ]);
    expect(a.state.model).toEqual(["x"]);
    expect(b.state.model).toEqual(["x"]);
    expect(c.state.model).toEqual(["x"]);
    await completion;
  });

  it("uses each receiver's paste encoding without double wrapping the source data", async () => {
    const { controller, a, b, c } = createHarness();
    b.state.bracketedPaste = true;
    controller.configure("tab-1", ["a", "b", "c"]);
    controller.setEnabled(true);
    const pasteText = "中文😀\rnext\tline";
    const sourceData = `\x1b[200~${pasteText}\x1b[201~`;

    await controller.writeUserInput("a", { data: sourceData, pasteText });
    expect(writeTerminalMock.mock.calls).toEqual([
      ["session-a", sourceData],
      ["session-b", sourceData],
      ["session-c", pasteText],
    ]);
    expect(a.endpoint.prepareMirroredInput).not.toHaveBeenCalled();
    expect(b.state.model).toEqual([sourceData]);
    expect(c.state.model).toEqual([pasteText]);

    await controller.writeUserInput("b", { data: "", pasteText: "body" });
    expect(writeTerminalMock).toHaveBeenCalledWith("session-b", "");
    expect(writeTerminalMock).toHaveBeenCalledWith("session-a", "body");
  });

  it("keeps alternating A/B input and models in the same per-pane FIFO despite a slow A", async () => {
    const { controller, a, b } = createHarness();
    const firstA = deferredWrite();
    writeTerminalMock.mockImplementation((sessionId, data) =>
      sessionId === "session-a" && data === "ec"
        ? firstA.promise
        : Promise.resolve(),
    );
    controller.setEnabled(true);
    const first = commitInput(controller, a, "ec");
    const second = commitInput(controller, b, "ho");
    const third = commitInput(controller, a, "\r");

    expect(a.state.model).toEqual(["ec", "ho", "\r"]);
    expect(b.state.model).toEqual(["ec", "ho", "\r"]);
    await second;
    expect(writeTerminalMock.mock.calls.filter(([id]) => id === "session-a"))
      .toEqual([["session-a", "ec"]]);
    firstA.resolve();
    await Promise.all([first, second, third]);
    await vi.waitFor(() => {
      expect(writeTerminalMock.mock.calls.filter(([id]) => id === "session-b"))
        .toEqual([["session-b", "ec"], ["session-b", "ho"], ["session-b", "\r"]]);
    });
    expect(writeTerminalMock.mock.calls.filter(([id]) => id === "session-a"))
      .toEqual([["session-a", "ec"], ["session-a", "ho"], ["session-a", "\r"]]);
  });

  it("does not await a slow mirror before completing or accepting further source input", async () => {
    const { controller } = createHarness();
    const slowB = deferredWrite();
    writeTerminalMock.mockImplementation((sessionId, data) =>
      sessionId === "session-b" && data === "x" ? slowB.promise : Promise.resolve(),
    );
    controller.setEnabled(true);
    await controller.writeUserInput("a", { data: "x" });
    await controller.writeUserInput("a", { data: "y" });
    expect(writeTerminalMock.mock.calls).toEqual([
      ["session-a", "x"], ["session-b", "x"], ["session-a", "y"],
    ]);
    slowB.resolve();
    await vi.waitFor(() => {
      expect(writeTerminalMock).toHaveBeenCalledWith("session-b", "y");
    });
  });

  it("cancels queued mirrors on manual disable but preserves queued local source writes", async () => {
    const { controller } = createHarness();
    const slowA = deferredWrite();
    const slowB = deferredWrite();
    writeTerminalMock.mockImplementation((sessionId, data) => {
      if (data !== "x") {
        return Promise.resolve();
      }
      return sessionId === "session-a" ? slowA.promise : slowB.promise;
    });
    controller.setEnabled(true);
    const first = controller.writeUserInput("a", { data: "x" });
    const localA = controller.writeUserInput("a", { data: "a" });
    const localB = controller.writeUserInput("b", { data: "b" });
    controller.setEnabled(false);
    slowA.resolve();
    slowB.resolve();
    await Promise.all([first, localA, localB]);

    expect(writeTerminalMock.mock.calls.filter(([id]) => id === "session-a"))
      .toEqual([["session-a", "x"], ["session-a", "a"]]);
    expect(writeTerminalMock.mock.calls.filter(([id]) => id === "session-b"))
      .toEqual([["session-b", "x"], ["session-b", "b"]]);
    expect(controller.getSnapshot()).toEqual({
      enabled: false, disabledReason: null, notice: null,
    });
  });

  it("catches group rejection, invalidates remaining mirrors, and preserves local queued input", async () => {
    const { controller, b } = createHarness();
    const slowA = deferredWrite();
    const failingB = deferredWrite();
    writeTerminalMock.mockImplementation((sessionId, data) => {
      if (data !== "x") {
        return Promise.resolve();
      }
      return sessionId === "session-a" ? slowA.promise : failingB.promise;
    });
    controller.setEnabled(true);
    const first = controller.writeUserInput("a", { data: "x" });
    const localA = controller.writeUserInput("a", { data: "a" });
    const localB = controller.writeUserInput("b", { data: "b" });
    failingB.reject(new Error("PTY failed"));
    await localB;
    expect(controller.getSnapshot()).toEqual({
      enabled: false,
      disabledReason: null,
      notice: {
        level: "error",
        message: "同步已关闭：部分输入可能未送达，请检查各分屏后重新开启。",
      },
    });
    expect(b.endpoint.onWriteError).toHaveBeenCalledTimes(1);
    slowA.resolve();
    await Promise.all([first, localA]);
    expect(writeTerminalMock.mock.calls.filter(([id]) => id === "session-a"))
      .toEqual([["session-a", "x"], ["session-a", "a"]]);
    expect(writeTerminalMock.mock.calls.filter(([id]) => id === "session-b"))
      .toEqual([["session-b", "x"], ["session-b", "b"]]);
  });

  it("does not close A/B or publish a group error when unselected C fails", async () => {
    const { controller, c } = createHarness();
    writeTerminalMock.mockImplementation((sessionId) =>
      sessionId === "session-c"
        ? Promise.reject(new Error("local failure"))
        : Promise.resolve(),
    );
    controller.setEnabled(true);
    const enabled = controller.getSnapshot();
    await expect(controller.writeUserInput("c", { data: "c" })).resolves.toBeUndefined();
    expect(c.endpoint.onWriteError).toHaveBeenCalledTimes(1);
    expect(controller.getSnapshot()).toBe(enabled);
    await controller.writeUserInput("a", { data: "still synchronized" });
    expect(writeTerminalMock).toHaveBeenCalledWith("session-b", "still synchronized");
  });

  it.each(["prepare", "observe", "write"])(
    "catches synchronous %s failures without dropping a valid source write",
    async (stage) => {
      const { controller, b } = createHarness();
      const fail = () => { throw new Error("synchronous failure"); };
      if (stage === "prepare") {
        b.endpoint.prepareMirroredInput.mockImplementation(fail);
      } else if (stage === "observe") {
        b.endpoint.observeMirroredInput.mockImplementation(fail);
      } else {
        writeTerminalMock.mockImplementation((sessionId) => {
          if (sessionId === "session-b") {
            throw new Error("synchronous PTY failure");
          }
          return Promise.resolve();
        });
      }
      controller.setEnabled(true);
      await expect(controller.writeUserInput("a", { data: "x" })).resolves.toBeUndefined();
      expect(writeTerminalMock).toHaveBeenCalledWith("session-a", "x");
      expect(controller.getSnapshot().enabled).toBe(false);
      expect(controller.getSnapshot().notice?.level).toBe("error");
      expect(b.endpoint.onWriteError).toHaveBeenCalledTimes(1);
    },
  );

  it("catches binding reads and error callbacks, without unhandled promise rejection", async () => {
    const { controller, b } = createHarness();
    const unhandled = vi.fn();
    window.addEventListener("unhandledrejection", unhandled);
    try {
      b.endpoint.readBinding.mockImplementationOnce(() => {
        throw new Error("unavailable binding");
      });
      controller.refresh();
      expect(controller.getSnapshot().disabledReason).toBe("所选分屏尚未全部连接");
      controller.setEnabled(true);
      b.endpoint.onWriteError.mockImplementation(() => {
        throw new Error("notice callback failed");
      });
      writeTerminalMock.mockImplementation((sessionId) =>
        sessionId === "session-b"
          ? Promise.reject(new Error("async PTY rejection"))
          : Promise.resolve(),
      );
      const first = controller.writeUserInput("a", { data: "x" });
      const last = controller.writeUserInput("b", { data: "queued local" });
      await expect(Promise.all([first, last])).resolves.toEqual([undefined, undefined]);
      expect(b.endpoint.onWriteError).toHaveBeenCalledTimes(1);
      expect(unhandled).not.toHaveBeenCalled();
      expect(controller.getSnapshot().enabled).toBe(false);
      expect(controller.getSnapshot().notice?.level).toBe("error");
    } finally {
      window.removeEventListener("unhandledrejection", unhandled);
    }
  });

  it("closes only for membership changes, not reordered or duplicate IDs", () => {
    const { controller } = createHarness();
    controller.setEnabled(true);
    const enabled = controller.getSnapshot();
    controller.configure("tab-1", ["b", "a", "b"]);
    expect(controller.getSnapshot()).toBe(enabled);
    controller.configure("tab-1", ["a", "b", "c"]);
    expect(controller.getSnapshot()).toEqual({
      enabled: false,
      disabledReason: null,
      notice: { level: "status", message: "目标已变更，同步已关闭。" },
    });
    controller.setEnabled(true);
    controller.configure("tab-1", []);
    expect(controller.getSnapshot().disabledReason).toBe("请至少选择两个终端分屏");
    expect(controller.getSnapshot().notice?.message).toBe("目标已变更，同步已关闭。");
  });

  it("clears old notices when switching tabs or clearing the owner scope", async () => {
    const { controller } = createHarness();
    controller.setEnabled(true);
    controller.stopForPane("a");
    controller.configure("tab-2", ["other"]);
    expect(controller.getSnapshot()).toEqual({
      enabled: false, disabledReason: "请至少选择两个终端分屏", notice: null,
    });
    controller.configure("tab-1", ["a", "b"]);
    controller.setEnabled(true);
    controller.stopForPane("b");
    controller.configure(null, []);
    expect(controller.getSnapshot().notice).toBeNull();
    await controller.writeUserInput("a", { data: "local after scope clear" });
    expect(writeTerminalMock.mock.calls).toEqual([["session-a", "local after scope clear"]]);
  });

  it("stops only an enabled group containing the pane receiving directed input", () => {
    const { controller } = createHarness();
    controller.stopForPane("a");
    expect(controller.getSnapshot().notice).toBeNull();
    controller.setEnabled(true);
    const enabled = controller.getSnapshot();
    controller.stopForPane("c");
    expect(controller.getSnapshot()).toBe(enabled);
    controller.stopForPane("b");
    expect(controller.getSnapshot()).toEqual({
      enabled: false,
      disabledReason: null,
      notice: { level: "status", message: "检测到定向输入，同步已关闭。" },
    });
  });

  it("invalidates immediately on unregister and does not let stale nested disposers remove replacements", async () => {
    const { controller, a } = createHarness();
    const secondDispose = controller.register("a", a.endpoint);
    const latestDispose = controller.register("a", a.endpoint);
    controller.setEnabled(true);
    const enabled = controller.getSnapshot();
    secondDispose();
    a.dispose();
    secondDispose();
    expect(controller.getSnapshot()).toBe(enabled);
    await controller.writeUserInput("b", { data: "x" });
    expect(writeTerminalMock).toHaveBeenCalledWith("session-a", "x");
    latestDispose();
    expect(controller.getSnapshot()).toEqual({
      enabled: false,
      disabledReason: "所选分屏尚未全部连接",
      notice: {
        level: "status",
        message: "分屏连接已变化，同步已关闭；请确认各端输入后重新开启。",
      },
    });
  });

  it("requires manual re-enable after endpoint replacement, even with the same binding values", () => {
    const { controller, b } = createHarness();
    controller.setEnabled(true);
    const replacement = registerPane(controller, "b");
    expect(controller.getSnapshot().enabled).toBe(false);
    expect(controller.getSnapshot().disabledReason).toBeNull();
    expect(controller.getSnapshot().notice?.message)
      .toBe("分屏连接已变化，同步已关闭；请确认各端输入后重新开启。");
    b.dispose();
    controller.refresh();
    expect(controller.getSnapshot().enabled).toBe(false);
    controller.setEnabled(true);
    expect(controller.getSnapshot().enabled).toBe(true);
    replacement.dispose();
    expect(controller.getSnapshot().enabled).toBe(false);
  });

  it.each(["disconnect", "generation", "session", "tab"])(
    "revalidates %s changes at enqueue without waiting for refresh notifications",
    async (change) => {
      const { controller, b } = createHarness();
      controller.setEnabled(true);
      if (change === "disconnect") {
        b.state.binding = null;
      } else if (change === "generation") {
        b.state.binding!.connectionGeneration += 1;
      } else if (change === "session") {
        b.state.binding!.sessionId = "session-b-new";
      } else {
        b.state.binding!.tabId = "tab-2";
      }
      await controller.writeUserInput("a", { data: "local" });
      expect(writeTerminalMock.mock.calls).toEqual([["session-a", "local"]]);
      expect(controller.getSnapshot().enabled).toBe(false);
      expect(controller.getSnapshot().notice?.message)
        .toBe("分屏连接已变化，同步已关闭；请确认各端输入后重新开启。");
      b.state.binding = { sessionId: "session-b", connectionGeneration: 2, tabId: "tab-1" };
      controller.refresh();
      expect(controller.getSnapshot().enabled).toBe(false);
      expect(controller.getSnapshot().disabledReason).toBeNull();
    },
  );

  it("revalidates the whole group at dequeue even when only another member changed", async () => {
    const { controller, b } = createHarness();
    const slowA = deferredWrite();
    writeTerminalMock.mockImplementation((sessionId, data) =>
      sessionId === "session-a" && data === "first" ? slowA.promise : Promise.resolve(),
    );
    controller.setEnabled(true);
    const first = controller.writeUserInput("a", { data: "first" });
    const second = controller.writeUserInput("b", { data: "second" });
    await second;
    b.state.binding = null;
    slowA.resolve();
    await first;
    await vi.waitFor(() => expect(controller.getSnapshot().enabled).toBe(false));
    expect(writeTerminalMock.mock.calls.filter(([id]) => id === "session-a"))
      .toEqual([["session-a", "first"]]);
  });

  it("never writes queued old mirrors or local source tasks into a reconnected session", async () => {
    const { controller, b } = createHarness();
    const oldB = deferredWrite();
    writeTerminalMock.mockImplementation((sessionId, data) =>
      sessionId === "session-b" && data === "first" ? oldB.promise : Promise.resolve(),
    );
    controller.setEnabled(true);
    await controller.writeUserInput("a", { data: "first" });
    await controller.writeUserInput("a", { data: "old mirror" });
    const oldLocal = controller.writeUserInput("b", { data: "old local" });
    b.state.binding = { sessionId: "session-b-new", connectionGeneration: 2, tabId: "tab-1" };
    controller.refresh();
    controller.setEnabled(true);
    await controller.writeUserInput("a", { data: "new input" });
    oldB.resolve();
    await oldLocal;
    await vi.waitFor(() => {
      expect(writeTerminalMock).toHaveBeenCalledWith("session-b-new", "new input");
    });
    expect(writeTerminalMock.mock.calls.filter(([id]) => id.startsWith("session-b")))
      .toEqual([["session-b", "first"], ["session-b-new", "new input"]]);
    expect(controller.getSnapshot().enabled).toBe(true);
  });

  it.each(["resolve", "reject"] as const)(
    "ignores late old-group %s after re-enabling the same endpoints",
    async (settlement) => {
      const { controller, a, b } = createHarness();
      const oldB = deferredWrite();
      writeTerminalMock.mockImplementation((sessionId, data) =>
        sessionId === "session-b" && data === "old" ? oldB.promise : Promise.resolve(),
      );
      controller.setEnabled(true);
      await controller.writeUserInput("a", { data: "old" });
      controller.setEnabled(false);
      controller.setEnabled(true);
      const reopened = controller.getSnapshot();
      const newLocal = controller.writeUserInput("b", { data: "new" });
      if (settlement === "reject") {
        oldB.reject(new Error("late old-group error"));
      } else {
        oldB.resolve();
      }
      await newLocal;
      expect(controller.getSnapshot()).toBe(reopened);
      expect(a.endpoint.onWriteError).not.toHaveBeenCalled();
      expect(b.endpoint.onWriteError).not.toHaveBeenCalled();
      expect(writeTerminalMock).toHaveBeenCalledWith("session-b", "new");
    },
  );

  it("does not notify an old or replacement runtime when its old write rejects late", async () => {
    const { controller, b } = createHarness();
    const oldB = deferredWrite();
    writeTerminalMock.mockImplementation((sessionId, data) =>
      sessionId === "session-b" && data === "old" ? oldB.promise : Promise.resolve(),
    );
    controller.setEnabled(true);
    await controller.writeUserInput("a", { data: "old" });
    const replacement = registerPane(controller, "b", {
      sessionId: "replacement-session-b", connectionGeneration: 2,
    });
    controller.setEnabled(true);
    const reopened = controller.getSnapshot();
    const replacementWrite = controller.writeUserInput("b", { data: "replacement" });
    oldB.reject(new Error("late disposed-runtime error"));
    await replacementWrite;
    expect(controller.getSnapshot()).toBe(reopened);
    expect(b.endpoint.onWriteError).not.toHaveBeenCalled();
    expect(replacement.endpoint.onWriteError).not.toHaveBeenCalled();
    expect(writeTerminalMock).toHaveBeenCalledWith("replacement-session-b", "replacement");
  });

  it("does not notify a replacement endpoint for an older independent local failure", async () => {
    const { controller, c } = createHarness();
    const oldC = deferredWrite();
    writeTerminalMock.mockReturnValueOnce(oldC.promise);
    const oldWrite = controller.writeUserInput("c", { data: "local" });
    const replacement = registerPane(controller, "c");
    oldC.reject(new Error("old local error"));
    await oldWrite;
    expect(c.endpoint.onWriteError).not.toHaveBeenCalled();
    expect(replacement.endpoint.onWriteError).not.toHaveBeenCalled();
    expect(controller.getSnapshot().notice).toBeNull();
  });

  it.each(["members", "tab", "owner"])(
    "invalidates queued mirrors on %s changes without replaying them after re-enable",
    async (change) => {
      const { controller } = createHarness();
      const slowB = deferredWrite();
      writeTerminalMock.mockImplementation((sessionId, data) =>
        sessionId === "session-b" && data === "first" ? slowB.promise : Promise.resolve(),
      );
      controller.setEnabled(true);
      await controller.writeUserInput("a", { data: "first" });
      await controller.writeUserInput("a", { data: "discarded mirror" });
      if (change === "members") {
        controller.configure("tab-1", ["a", "b", "c"]);
      } else if (change === "tab") {
        controller.configure("tab-2", ["other"]);
      } else {
        controller.configure(null, []);
      }
      controller.configure("tab-1", ["a", "b"]);
      controller.setEnabled(true);
      const reopened = controller.getSnapshot();
      const currentLocal = controller.writeUserInput("b", { data: "current local" });
      slowB.resolve();
      await currentLocal;
      expect(writeTerminalMock.mock.calls.filter(([id]) => id === "session-b"))
        .toEqual([["session-b", "first"], ["session-b", "current local"]]);
      expect(controller.getSnapshot()).toBe(reopened);
    },
  );

  it("catches a synchronous source failure before starting mirrors and still accepts later local input", async () => {
    const { controller, a, b } = createHarness();
    writeTerminalMock.mockImplementationOnce(() => {
      throw new Error("source write failed synchronously");
    });
    controller.setEnabled(true);
    const failed = commitInput(controller, a, "failed");
    expect(controller.getSnapshot().notice?.level).toBe("error");
    expect(a.state.model).toEqual(["failed"]);
    expect(b.state.model).toEqual(["failed"]);
    await expect(failed).resolves.toBeUndefined();
    expect(a.endpoint.onWriteError).toHaveBeenCalledTimes(1);
    expect(b.endpoint.onWriteError).not.toHaveBeenCalled();
    await controller.writeUserInput("a", { data: "local after failure" });
    expect(writeTerminalMock.mock.calls).toEqual([
      ["session-a", "failed"], ["session-a", "local after failure"],
    ]);
  });
});
