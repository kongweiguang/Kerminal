// @author kongweiguang
import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { useAgentLauncherAction } from "../../../../../src/features/tool-panel/agent-launcher/useAgentLauncherAction";

describe("useAgentLauncherAction", () => {
  it("同一渲染周期的重复点击只执行一次，结束后允许再次操作", async () => {
    const { result } = renderHook(() => useAgentLauncherAction());
    let finish!: () => void;
    const action = vi.fn(() => new Promise<void>((resolve) => { finish = resolve; }));
    let first!: Promise<void>;
    act(() => {
      first = result.current.runAction("codex", action);
      void result.current.runAction("codex", action);
    });
    expect(action).toHaveBeenCalledTimes(1);
    expect(result.current.actionState).toBe("codex");
    await act(async () => { finish(); await first; });
    const next = vi.fn().mockResolvedValue(undefined);
    await act(async () => { await result.current.runAction("codex", next); });
    expect(next).toHaveBeenCalledOnce();
    expect(result.current.actionState).toBeNull();
  });

  it("失败显示可恢复提示并释放锁，重试成功清除错误", async () => {
    const { result } = renderHook(() => useAgentLauncherAction());
    await act(async () => {
      await result.current.runAction("codex", async () => { throw new Error("offline"); });
    });
    expect(result.current.actionError?.title).toBe("Agent 操作未完成");
    expect(result.current.actionState).toBeNull();
    await act(async () => {
      await result.current.runAction("codex", async () => undefined);
    });
    expect(result.current.actionError).toBeNull();
  });
});
