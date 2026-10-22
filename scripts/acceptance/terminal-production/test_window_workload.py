"""Regression checks for asynchronous terminal-output readiness and deadline handling.

@author kongweiguang
"""

from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPT_DIR = Path(__file__).resolve().parent
SCRIPT_PATH = SCRIPT_DIR / "window_workload.py"
SPEC = importlib.util.spec_from_file_location("window_workload_under_test", SCRIPT_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("cannot load workload script")
workload = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = workload
SPEC.loader.exec_module(workload)


class FakeClock:
    """替换单调时钟与间隔限速，使 deadline 测试不等待真实时间。

    @author kongweiguang
    """

    def __init__(self) -> None:
        """从零开始记录测试时间，以便精确驱动 readiness 期限。

        @author kongweiguang
        """

        self.now = 0.0

    def perf_counter(self) -> float:
        """返回仅由测试 sleep 和模拟请求推进的时钟值。

        @author kongweiguang
        """

        return self.now

    def sleep(self, seconds: float) -> None:
        """把限速间隔推进到逻辑时钟，不引入真实等待。

        @author kongweiguang
        """

        self.now += seconds


class FakeEvents:
    """仅保留 readiness 事件，避免在单测中创建运行态证据文件。

    @author kongweiguang
    """

    def __init__(self) -> None:
        """初始化内存事件缓冲区。

        @author kongweiguang
        """

        self.rows: list[dict[str, object]] = []

    def write(self, row: dict[str, object]) -> None:
        """记录脚本实际生成的事件字段，供断言调用次数和期限结果。

        @author kongweiguang
        """

        self.rows.append(row)


class FakeClient:
    """模拟只读快照回执并暴露 timeout 恢复与禁止重发写入的检查点。

    @author kongweiguang
    """

    def __init__(self, snapshots: list[str], clock: FakeClock | None = None) -> None:
        """按调用顺序返回合成快照，不实现任何创建、写入或关闭接口。

        @author kongweiguang
        """

        self.snapshots = list(snapshots)
        self.clock = clock
        self.timeout = 5.0
        self.timeouts: list[float] = []
        self.calls: list[str] = []

    def call_tool(self, name: str, arguments: dict[str, object]) -> dict[str, object]:
        """只允许模拟 snapshot；每次请求都返回既定内存文本。

        @author kongweiguang
        """

        self.calls.append(name)
        self.timeouts.append(self.timeout)
        if name != "terminal.snapshot":
            raise AssertionError("readiness probe must not reissue terminal.write")
        if self.clock is not None and getattr(self, "advance_on_call", 0.0):
            self.clock.now += self.advance_on_call
        text = self.snapshots.pop(0) if self.snapshots else "prompt"
        return {"structuredContent": {"data": {"snapshot": {"data": text}}}}


class ReadinessPollingTests(unittest.TestCase):
    """验证只读重采样、期限边界和请求超时恢复。"""

    def test_delayed_marker_is_observed_without_reissuing_write(self) -> None:
        """第二次快照读到完整 marker 后才报告 ready，且每轮仅请求 snapshot。

        @author kongweiguang
        """

        clock = FakeClock()
        client = FakeClient(["prompt", "prompt\nMARKER_JOB_1_1\n"], clock)
        events = FakeEvents()
        client.events = events
        with patch.object(workload.time, "perf_counter", clock.perf_counter), patch.object(
            workload.time, "sleep", clock.sleep
        ):
            ready = workload.verify_owned_output(
                client, "synthetic-session", "MARKER_JOB", timeout_seconds=1.0,
                poll_interval_seconds=0.25
            )
        self.assertTrue(ready)
        self.assertEqual(client.calls, ["terminal.snapshot", "terminal.snapshot"])
        self.assertEqual(client.timeouts, [1.0, 0.75])
        self.assertEqual(client.timeout, 5.0)
        self.assertEqual([row["attempt"] for row in events.rows], [1, 2])
        self.assertEqual(events.rows[-1]["observedSyntheticLineCount"], 1)

    def test_empty_snapshots_stop_at_deadline(self) -> None:
        """缺少 marker 时只读取期限内的快照，并按期限返回失败。

        @author kongweiguang
        """

        clock = FakeClock()
        client = FakeClient(["prompt"] * 8, clock)
        client.events = FakeEvents()
        with patch.object(workload.time, "perf_counter", clock.perf_counter), patch.object(
            workload.time, "sleep", clock.sleep
        ):
            ready = workload.verify_owned_output(
                client, "synthetic-session", "MARKER_JOB", timeout_seconds=0.5,
                poll_interval_seconds=0.25
            )
        self.assertFalse(ready)
        self.assertEqual(client.calls, ["terminal.snapshot", "terminal.snapshot"])
        self.assertEqual(client.timeout, 5.0)
        self.assertEqual(clock.now, 0.5)
        self.assertEqual(len(client.events.rows), 2)

    def test_marker_returned_after_deadline_does_not_pass(self) -> None:
        """请求超过期限后即便返回 marker，也不能把晚到结果记作 readiness。

        @author kongweiguang
        """

        clock = FakeClock()
        client = FakeClient(["MARKER_JOB_1_1"], clock)
        client.events = FakeEvents()
        client.advance_on_call = 1.1
        with patch.object(workload.time, "perf_counter", clock.perf_counter), patch.object(
            workload.time, "sleep", clock.sleep
        ):
            ready = workload.verify_owned_output(
                client, "synthetic-session", "MARKER_JOB", timeout_seconds=1.0,
                poll_interval_seconds=0.25
            )
        self.assertFalse(ready)
        self.assertEqual(client.calls, ["terminal.snapshot"])
        self.assertFalse(client.events.rows[0]["withinDeadline"])
        self.assertEqual(client.timeout, 5.0)


if __name__ == "__main__":
    unittest.main()
