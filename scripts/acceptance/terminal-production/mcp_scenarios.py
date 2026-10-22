#!/usr/bin/env python3
"""MCP 后台终端有限输出、回读和关闭场景。

@author kongweiguang
"""

from __future__ import annotations

import os
import re
import shutil
import time
from pathlib import Path
from typing import Any

from identity import AcceptanceFailure
from mcp_client import (
    EventWriter,
    McpClient,
    call_list,
    call_list_data,
    call_runtime_snapshot,
    measured_call,
    structured_data,
    terminal_ids,
)

def verify_output(
    client: McpClient,
    events: EventWriter,
    session_id: str,
    token: str,
    line_count: int,
    timeout_seconds: float,
    shell: str,
) -> dict[str, Any]:
    """按创建 session 时的显式 shell 生成并回读有限输出，避免依赖外层矩阵的隐式局部状态。"""

    shell_stem = Path(shell).stem.casefold()
    if os.name == "nt":
        if shell_stem not in {"pwsh", "powershell"}:
            raise AcceptanceFailure("Windows MCP 输出场景仅支持 PowerShell dialect")
        command = (
            f"for ($i = 1; $i -le {line_count}; $i++) "
            f"{{ Write-Output ('{token}-' + $i) }}; "
            f"Write-Output '{token}-END'\r"
        )
    else:
        if not Path(shell).is_absolute():
            raise AcceptanceFailure("Linux MCP 输出场景必须使用 POSIX shell 绝对路径")
        command = (
            f"i=1; while [ \"$i\" -le {line_count} ]; do "
            f"printf '{token}-%s\\n' \"$i\"; i=$((i+1)); done; "
            f"printf '{token}-END\\n'\r"
        )
    write_result, write_latency = measured_call(
        client,
        events,
        "terminal.write",
        {"sessionId": session_id, "data": command},
        "finite-output-write",
    )
    write_data = structured_data(write_result)
    expected_written = len(command.encode("utf-8"))
    if write_data.get("bytesWritten") != expected_written:
        raise AcceptanceFailure("terminal.write bytesWritten 与 UTF-8 输入字节数不一致")

    pattern = re.compile(rf"^{re.escape(token)}-(\d+)\s*$")
    deadline = time.monotonic() + timeout_seconds
    poll_count = 0
    last_result: dict[str, Any] | None = None
    last_data: dict[str, Any] | None = None
    snapshot_latency_ms: float | None = None
    expected = list(range(1, line_count + 1))
    while time.monotonic() < deadline:
        poll_count += 1
        client.verify_live_instance()
        started = time.perf_counter()
        result = client.call_tool(
            "terminal.snapshot", {"sessionId": session_id, "maxBytes": 1_048_576}
        )
        snapshot_latency_ms = (time.perf_counter() - started) * 1000
        data = structured_data(result)
        snapshot = data.get("snapshot")
        if not isinstance(snapshot, dict) or not isinstance(snapshot.get("data"), str):
            raise AcceptanceFailure("terminal.snapshot 缺少文本快照")
        text = snapshot["data"]
        seen: list[int] = []
        for line in text.splitlines():
            match = pattern.fullmatch(line.strip())
            if match:
                seen.append(int(match.group(1)))
        end_present = f"{token}-END" in text
        last_result, last_data = result, data
        if seen == expected and end_present:
            if snapshot.get("truncated") is not False:
                raise AcceptanceFailure("有限输出快照被截断，完整性无法确认")
            events.write(
                {
                    "kind": "mcpSnapshot",
                    "operation": "finite-output-readback",
                    "sessionId": session_id,
                    "marker": token,
                    "expectedLines": line_count,
                    "observedLines": len(seen),
                    "pollCount": poll_count,
                    "mcpCallResponseLatencyMs": snapshot_latency_ms,
                    "writeCallResponseLatencyMs": write_latency,
                    "rawResult": last_result,
                }
            )
            return {
                "expectedLines": line_count,
                "observedLines": len(seen),
                "pollCount": poll_count,
                "writeCallResponseLatencyMs": write_latency,
                "finalSnapshotCallResponseLatencyMs": snapshot_latency_ms,
                "capturedBytes": snapshot.get("capturedBytes"),
                "truncated": snapshot.get("truncated"),
                "rawSnapshot": last_data,
            }
        time.sleep(0.1)

    events.write(
        {
            "kind": "mcpSnapshotFailure",
            "operation": "finite-output-readback",
            "sessionId": session_id,
            "marker": token,
            "expectedLines": line_count,
            "pollCount": poll_count,
            "mcpCallResponseLatencyMs": snapshot_latency_ms,
            "rawResult": last_result,
            "error": "output-timeout-or-incomplete",
        }
    )
    observed = None if last_data is None else last_data.get("snapshot", {}).get("data")
    observed_count = 0
    if isinstance(observed, str):
        observed_count = sum(1 for line in observed.splitlines() if pattern.fullmatch(line.strip()))
    raise AcceptanceFailure(
        f"有限输出未完整回读：expected={line_count}, observed={observed_count}, polls={poll_count}"
    )


def create_session(
    client: McpClient,
    events: EventWriter,
    cwd: Path,
    shell: str,
) -> tuple[str, float]:
    """显式创建本轮专属后台 session 并校验 MCP contract 的 headless 标记。"""

    result, latency = measured_call(
        client,
        events,
        "terminal.create",
        {"target": "local", "shell": shell, "cwd": str(cwd), "cols": 120, "rows": 30},
        "create",
    )
    data = structured_data(result)
    session_id = data.get("sessionId")
    if not isinstance(session_id, str) or not session_id:
        raise AcceptanceFailure("terminal.create 未返回 sessionId")
    if data.get("headless") is not True:
        raise AcceptanceFailure("terminal.create 未确认 headless 会话")
    return session_id, latency


def close_session(client: McpClient, events: EventWriter, session_id: str) -> None:
    """每个 runner-owned session 只提交一次 close，随后由 terminal.list 读回回收事实。"""

    result, _ = measured_call(
        client,
        events,
        "terminal.close",
        {"sessionId": session_id},
        "close",
    )
    if structured_data(result).get("closed") is not True:
        raise AcceptanceFailure("terminal.close 未返回 closed=true")


def reconcile_unknown_creates(
    client: McpClient,
    events: EventWriter,
    scenario_root: Path,
    attempted_directories: set[Path],
    known_session_ids: set[str],
) -> tuple[set[str], dict[str, Any]]:
    """仅按本轮唯一 cwd 回读未知 create 结果，并显式关闭已确认的 runner-owned session。"""

    data = call_list_data(client, events, "create-outcome-readback")
    rows = data.get("sessions")
    if not isinstance(rows, list):
        raise AcceptanceFailure("terminal.list cleanup 回读缺少 sessions 数组")
    resolved_root = scenario_root.resolve()
    expected: set[str] = set()
    for path in attempted_directories:
        resolved = path.resolve()
        try:
            resolved.relative_to(resolved_root)
        except ValueError:
            raise AcceptanceFailure("runner-owned cwd 超出本轮临时目录") from None
        expected.add(os.path.normcase(str(resolved)))
    reconciled: set[str] = set()
    for row in rows:
        if not isinstance(row, dict):
            continue
        session_id = row.get("sessionId")
        cwd = row.get("cwd")
        if not isinstance(session_id, str) or not isinstance(cwd, str):
            continue
        if session_id in known_session_ids or os.path.normcase(str(Path(cwd).resolve())) not in expected:
            continue
        events.write(
            {
                "kind": "unknownCreateReconciledByReadback",
                "sessionId": session_id,
                "cwd": cwd,
            }
        )
        close_session(client, events, session_id)
        reconciled.add(session_id)
    if reconciled:
        data = call_list_data(client, events, "unknown-create-close-readback")
    return reconciled, data


def run_mcp_matrix(
    client: McpClient,
    events: EventWriter,
    output_root: Path,
    run_id: str,
    session_counts: list[int],
    integrity_lines: int,
    io_timeout: float,
    shell: str,
) -> dict[str, Any]:
    """逐个运行 1/10/50 后台 session 有限输出场景，并在每轮结束验证清理。"""

    initial = call_list(client, events, "preflight")
    initial_runtime = call_runtime_snapshot(client, events, "preflight")
    if initial or initial_runtime.get("terminalSessionCount") != 0 or initial_runtime.get("agentSessionCount") != 0:
        raise AcceptanceFailure(
            "隔离 endpoint 在验收前已有终端或 Agent session；拒绝与现存会话混跑"
        )
    scenarios: list[dict[str, Any]] = []
    for count in session_counts:
        scenario_root = output_root / "session-cwd" / f"{run_id}-{count}"
        scenario_root.mkdir(parents=True, exist_ok=False)
        sessions: list[str] = []
        attempted_directories: set[Path] = set()
        output_records: list[dict[str, Any]] = []
        create_latencies: list[float] = []
        close_errors: list[str] = []
        try:
            for index in range(count):
                cwd = scenario_root / f"session-{index + 1:03d}"
                cwd.mkdir()
                attempted_directories.add(cwd)
                session_id, latency = create_session(client, events, cwd, shell)
                if session_id in sessions:
                    raise AcceptanceFailure("terminal.create 重复返回 sessionId")
                sessions.append(session_id)
                create_latencies.append(latency)
            listed = call_list(client, events, f"sessions-{count}-created")
            if listed != set(sessions):
                raise AcceptanceFailure(
                    f"terminal.list 与本轮 session 集合不符：expected={count}, observed={len(listed)}"
                )
            for index, session_id in enumerate(sessions):
                marker = f"KTP_{run_id[:10]}_{count}_{index + 1:03d}"
                output_records.append(
                    verify_output(
                        client,
                        events,
                        session_id,
                        marker,
                        integrity_lines,
                        io_timeout,
                        shell,
                    )
                )
        except Exception as error:
            events.write(
                {
                    "kind": "scenarioFailure",
                    "scenario": f"sessions-{count}",
                    "errorType": type(error).__name__,
                    "error": str(error),
                }
            )
            raise
        finally:
            for session_id in reversed(sessions):
                try:
                    close_session(client, events, session_id)
                except Exception as error:
                    close_errors.append(f"{type(error).__name__}: {error}")
                    events.write(
                        {
                            "kind": "cleanupFailure",
                            "scenario": f"sessions-{count}",
                            "sessionId": session_id,
                            "errorType": type(error).__name__,
                            "error": str(error),
                        }
                    )
            _, remaining_data = reconcile_unknown_creates(
                client,
                events,
                scenario_root,
                attempted_directories,
                set(sessions),
            )
            remaining = terminal_ids(remaining_data)
            leaked = remaining.intersection(sessions)
            if leaked:
                raise AcceptanceFailure(
                    f"MCP close 后仍有 runner-owned session：{len(leaked)}"
                )
            if remaining:
                raise AcceptanceFailure("清理读回发现非 runner-owned session，拒绝继续场景")
            if close_errors:
                raise AcceptanceFailure(f"{len(close_errors)} 次 MCP close 未返回确认")
            shutil.rmtree(scenario_root)

        scenarios.append(
            {
                "scenario": f"sessions-{count}",
                "created": len(sessions),
                "finiteOutputSessionsVerified": len(output_records),
                "integrityLinesPerSession": integrity_lines,
                "meanCreateMcpCallResponseLatencyMs": (
                    sum(create_latencies) / len(create_latencies) if create_latencies else None
                ),
                "outputs": output_records,
                "cleanup": "terminal.list confirmed no sessions remain",
            }
        )
    final_ids = call_list(client, events, "final")
    final_runtime = call_runtime_snapshot(client, events, "final")
    if final_ids or final_runtime.get("terminalSessionCount") != 0:
        raise AcceptanceFailure("MCP 有限输出矩阵结束后仍检测到终端 session")
    return {
        "scenarios": scenarios,
        "finalSessionCount": len(final_ids),
        "finalRuntime": final_runtime,
    }


