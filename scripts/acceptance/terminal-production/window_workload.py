#!/usr/bin/env python3
"""为 Windows release 窗口验收准备有界后台本地终端负载，不采集 GPUI 帧指标。

@author kongweiguang
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
import re
import sys
import time
import uuid
from pathlib import Path
from typing import Any

from identity import (
    AcceptanceFailure,
    InstanceRecord,
    is_same_or_child,
    read_instance_record,
    repository_root,
    sha256_file,
    utc_now,
    verify_instance,
)
from mcp_client import EventWriter, McpClient, REQUIRED_TOOLS, structured_data, terminal_ids


DIAGNOSTICS_ROOT = Path("target/diagnostics/terminal-production-20261020")
MCP_SHELL = DIAGNOSTICS_ROOT / "bin" / "pwsh.exe"
RUN_ROOT_PATTERN = re.compile(r"windows-[a-z0-9-]+-\d{8}-\d{6}-\d{3}")
TARGET_COUNTS = (1, 10, 50)
MAX_VISIBLE = 4
LINES_PER_SECOND = 16
OUTPUT_SETUP_MARGIN_SECS = 300
MAX_OUTPUT_LINES = 2_000_000
MAX_DURATION_SECS = 7_200
SAMPLE_INTERVAL_SECS = 10
OUTPUT_READINESS_TIMEOUT_SECS = 15.0
OUTPUT_READINESS_POLL_INTERVAL_SECS = 0.25


def parse_session_ids(value: str, option: str) -> set[str]:
    """要求显式列出当前 session，避免漏传时脚本擅自把现有会话当成可操作对象。

    @author kongweiguang
    """

    if value.strip().casefold() == "none":
        return set()
    ids = [part.strip() for part in value.split(",")]
    if not ids or any(not item for item in ids):
        raise AcceptanceFailure(f"{option} 必须是逗号分隔 ID，或显式写 none")
    if any(len(item) > 128 or re.fullmatch(r"[A-Za-z0-9_.:-]+", item) is None for item in ids):
        raise AcceptanceFailure(f"{option} 含控制字符或无效 session ID")
    if len(set(ids)) != len(ids):
        raise AcceptanceFailure(f"{option} 不得重复 session ID")
    return set(ids)


def parse_arguments() -> argparse.Namespace:
    """把目标总数、当前会话清单和时长作为显式输入，创建会话前不推测用户意图。

    @author kongweiguang
    """

    parser = argparse.ArgumentParser(
        description="准备隔离 Kerminal release 窗口验收的有限后台终端负载。"
    )
    parser.add_argument("--instance-record", required=True, type=Path)
    parser.add_argument("--target-total-count", required=True, type=int, choices=TARGET_COUNTS)
    parser.add_argument("--expected-existing-session-ids", required=True)
    parser.add_argument("--expected-visible-session-ids", required=True)
    parser.add_argument("--duration-secs", required=True, type=int)
    parser.add_argument("--no-reserve-fresh-ui-session", action="store_true")
    parser.add_argument("--request-timeout-secs", type=float, default=30.0)
    return parser.parse_args()


def validate_options(
    args: argparse.Namespace,
) -> tuple[set[str], set[str], int, bool, Path]:
    """验证 pane 上限和输出总量，确保本脚本的合成输出在运行前就有明确上界。

    @author kongweiguang
    """

    existing = parse_session_ids(args.expected_existing_session_ids, "--expected-existing-session-ids")
    visible = parse_session_ids(args.expected_visible_session_ids, "--expected-visible-session-ids")
    if not visible.issubset(existing):
        raise AcceptanceFailure("visible IDs 必须属于 root 明确提供的 existing IDs")
    reserve_fresh = not args.no_reserve_fresh_ui_session
    if len(visible) > MAX_VISIBLE or (reserve_fresh and len(visible) >= MAX_VISIBLE):
        raise AcceptanceFailure("预留新前台 pane 后可见 session 总数不能超过 4")
    if args.duration_secs < 1 or args.duration_secs > MAX_DURATION_SECS:
        raise AcceptanceFailure(f"--duration-secs 必须在 1..={MAX_DURATION_SECS}")
    if args.request_timeout_secs < 1 or args.request_timeout_secs > 300:
        raise AcceptanceFailure("--request-timeout-secs 必须在 1..=300")

    background_count = args.target_total_count - len(existing) - int(reserve_fresh)
    if background_count < 0:
        raise AcceptanceFailure("目标总数不足以容纳现存 session 和预留 pane；脚本不会关闭现存会话")
    planned_lines = background_count * (args.duration_secs + OUTPUT_SETUP_MARGIN_SECS) * LINES_PER_SECOND
    if planned_lines > MAX_OUTPUT_LINES:
        raise AcceptanceFailure(f"有限输出上界 {planned_lines} 行超过 {MAX_OUTPUT_LINES} 行")

    evidence_root = (repository_root() / "target/acceptance/terminal-production").resolve()
    run_id = uuid.uuid4().hex
    return existing, visible, background_count, reserve_fresh, evidence_root / f"window-workload-{run_id}"


def validate_windows_instance_layout(
    record_path: Path, record: InstanceRecord
) -> tuple[Path, Path]:
    """核对 Windows launcher 的唯一 run 目录、独立 configRoot、release 二进制和 NoProfile shim。

    @author kongweiguang
    """

    if os.name != "nt":
        raise AcceptanceFailure("window_workload.py 仅支持 Windows 原生窗口验收")
    root = repository_root().resolve(strict=True)
    diagnostics_root = (root / DIAGNOSTICS_ROOT).resolve(strict=True)
    record_path = record_path.resolve(strict=True)
    run_root = record_path.parent.resolve(strict=True)
    if record_path.name != "instance.json" or not is_same_or_child(run_root, diagnostics_root):
        raise AcceptanceFailure("instance record 必须位于本 worktree Windows diagnostics run 目录")
    if run_root == diagnostics_root or RUN_ROOT_PATTERN.fullmatch(run_root.name) is None:
        raise AcceptanceFailure("拒绝复用 diagnostics 根目录或非唯一 launcher run 目录")
    if record.config_root.parent != run_root or record.config_root.name != "config":
        raise AcceptanceFailure("configRoot 必须是当前 run 目录下的 config 子目录")
    expected_binary = (run_root / "bin" / "kerminal.exe").resolve(strict=True)
    if os.path.normcase(str(record.executable_path)) != os.path.normcase(str(expected_binary)):
        raise AcceptanceFailure("实例二进制必须来自当前 run 的 bin/kerminal.exe")
    if not (record.config_root / "profiles" / "native-pwsh.toml").is_file():
        raise AcceptanceFailure("隔离 configRoot 缺少 native-pwsh Profile fixture")
    shell = (root / MCP_SHELL).resolve(strict=True)
    if not shell.is_file():
        raise AcceptanceFailure("本 worktree 的 NoProfile pwsh MCP shim 不存在")
    return run_root, shell


def record_event(events: EventWriter, kind: str, **fields: Any) -> None:
    """为阶段事件附加 monotonic 时间，供 root 与 native-metrics 时间线交叉核对。

    @author kongweiguang
    """

    events.write({"kind": kind, "monotonicNs": time.perf_counter_ns(), **fields})


def safe_request(tool: str, arguments: dict[str, Any]) -> dict[str, Any]:
    """只保存合成写入长度和哈希，不把命令正文写进 workload 证据。

    @author kongweiguang
    """

    if tool != "terminal.write" or not isinstance(arguments.get("data"), str):
        return arguments
    data = arguments["data"].encode("utf-8")
    return {
        "sessionId": arguments.get("sessionId"),
        "synthetic": True,
        "dataBytes": len(data),
        "dataSha256": hashlib.sha256(data).hexdigest(),
    }


def redacted_list_result(result: dict[str, Any]) -> dict[str, Any]:
    """保留 terminal.list 的 ID 结构并去掉现存会话的 cwd 等展示字段。

    @author kongweiguang
    """

    redacted = copy.deepcopy(result)
    structured = redacted.get("structuredContent")
    data = structured.get("data") if isinstance(structured, dict) else None
    rows = data.get("sessions") if isinstance(data, dict) else None
    if isinstance(rows, list):
        data["sessions"] = [
            {"sessionId": row["sessionId"]}
            for row in rows
            if isinstance(row, dict) and isinstance(row.get("sessionId"), str)
        ]
    if "content" in redacted:
        redacted["content"] = [{"type": "text", "text": "<terminal.list details redacted>"}]
    return redacted


class RecordingMcpClient(McpClient):
    """记录 terminal API 往返，同时继承 McpClient 的固定 loopback endpoint 与协议校验。

    @author kongweiguang
    """

    def __init__(self, record: InstanceRecord, timeout: float, events: EventWriter) -> None:
        super().__init__(record, timeout)
        self.events = events

    def call_tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        """每次调用前复核 live process，并记录原始回执或错误；不自动重试未知写入结果。

        @author kongweiguang
        """

        request = safe_request(name, arguments)
        try:
            self.verify_live_instance()
        except Exception as error:
            record_event(
                self.events,
                "identityGuardRejected",
                tool=name,
                request=request,
                errorType=type(error).__name__,
                error=str(error),
            )
            raise
        started_ns = time.perf_counter_ns()
        started_utc = utc_now()
        try:
            result = super().call_tool(name, arguments)
        except Exception as error:
            record_event(
                self.events,
                "mcpApiError",
                tool=name,
                request=request,
                startedAtUtc=started_utc,
                startedMonotonicNs=started_ns,
                elapsedMs=(time.perf_counter_ns() - started_ns) / 1_000_000,
                errorType=type(error).__name__,
                error=str(error),
            )
            raise
        raw_result = redacted_list_result(result) if name == "terminal.list" else result
        record_event(
            self.events,
            "mcpApiResponse",
            tool=name,
            request=request,
            startedAtUtc=started_utc,
            startedMonotonicNs=started_ns,
            elapsedMs=(time.perf_counter_ns() - started_ns) / 1_000_000,
            rawResponse=raw_result,
            rawResponseRedacted=name == "terminal.list",
        )
        try:
            structured_data(result)
        except Exception as error:
            record_event(
                self.events,
                "mcpToolError",
                tool=name,
                errorType=type(error).__name__,
                error=str(error),
            )
            raise
        return result


def read_session_list(
    client: RecordingMcpClient, operation: str
) -> tuple[set[str], list[dict[str, Any]]]:
    """只用 session IDs 校验数量；列表中的展示字段不会进入事件日志。

    @author kongweiguang
    """

    data = structured_data(client.call_tool("terminal.list", {}))
    ids = terminal_ids(data)
    rows_value = data.get("sessions")
    if not isinstance(rows_value, list):
        raise AcceptanceFailure("terminal.list 缺少 sessions 数组")
    rows = [row for row in rows_value if isinstance(row, dict)]
    record_event(
        client.events,
        "terminalCountObserved",
        operation=operation,
        terminalSessionCount=len(ids),
        sessionIds=sorted(ids),
    )
    return ids, rows


def read_runtime(
    client: RecordingMcpClient, operation: str
) -> dict[str, Any]:
    """记录 phase 边界 runtime count，以独立事实校验 terminal.list 的会话集合。

    @author kongweiguang
    """

    data = structured_data(client.call_tool("kerminal.runtime_snapshot", {}))
    runtime = data.get("runtime")
    if not isinstance(runtime, dict):
        raise AcceptanceFailure(f"{operation} runtime_snapshot 缺少 runtime object")
    return runtime


def validate_runtime(runtime: dict[str, Any], expected_count: int, operation: str) -> None:
    """拒绝缺失或不匹配的全局计数，也拒绝专用实例出现 Agent session。

    @author kongweiguang
    """

    terminal_count = runtime.get("terminalSessionCount")
    agent_count = runtime.get("agentSessionCount")
    if (
        not isinstance(terminal_count, int)
        or isinstance(terminal_count, bool)
        or terminal_count != expected_count
        or not isinstance(agent_count, int)
        or isinstance(agent_count, bool)
        or agent_count != 0
    ):
        raise AcceptanceFailure(
            f"{operation} runtime count 不符：terminal={terminal_count}, "
            f"expected={expected_count}, agent={agent_count}"
        )


def finite_output_command(marker: str, seconds: int) -> str:
    """使用 run 唯一 marker 和已校验整数生成每秒 16 行、到期自动结束的 PowerShell 命令。

    @author kongweiguang
    """

    return (
        "$ErrorActionPreference='Stop'; "
        f"for ($second = 1; $second -le {seconds}; $second++) {{ "
        "$batchTimer = [System.Diagnostics.Stopwatch]::StartNew(); "
        f"for ($line = 1; $line -le {LINES_PER_SECOND}; $line++) {{ "
        f"Write-Output ('{marker}_' + $second + '_' + $line) }}; "
        "$remaining = 1000 - $batchTimer.ElapsedMilliseconds; "
        "if ($remaining -gt 0) { Start-Sleep -Milliseconds $remaining } }; "
        f"Write-Output '{marker}_DONE'\r"
    )


def verify_owned_output(
    client: RecordingMcpClient,
    session_id: str,
    marker: str,
    timeout_seconds: float = OUTPUT_READINESS_TIMEOUT_SECS,
    poll_interval_seconds: float = OUTPUT_READINESS_POLL_INTERVAL_SECS,
) -> bool:
    """以单调 deadline 轮询真实只读快照；write ACK 不代表 shell 已输出 marker。

    每轮只重试 terminal.snapshot，不重发 terminal.write；间隔 sleep 只限速，
    ready 必须来自快照中完整匹配的合成输出行。每个 HTTP 读取的超时也受剩余
    deadline 限制，避免最后一次轮询把总等待无界拉长。

    @author kongweiguang
    """

    if timeout_seconds <= 0 or poll_interval_seconds <= 0:
        raise AcceptanceFailure("输出 readiness timeout 和 poll interval 必须为正数")
    started_at = time.perf_counter()
    deadline = started_at + timeout_seconds
    request_timeout = client.timeout
    pattern = re.compile(rf"^{re.escape(marker)}_\d+_\d+$")
    attempt = 0

    while True:
        remaining = deadline - time.perf_counter()
        if remaining <= 0:
            return False
        attempt += 1
        client.timeout = min(request_timeout, remaining)
        try:
            data = structured_data(
                client.call_tool(
                    "terminal.snapshot", {"sessionId": session_id, "maxBytes": 262_144}
                )
            )
        finally:
            client.timeout = request_timeout

        completed_at = time.perf_counter()
        snapshot = data.get("snapshot")
        text = snapshot.get("data") if isinstance(snapshot, dict) else None
        if not isinstance(text, str):
            raise AcceptanceFailure("owned terminal.snapshot 缺少 data 文本")
        count = sum(1 for line in text.splitlines() if pattern.fullmatch(line.strip()))
        within_deadline = completed_at <= deadline
        elapsed_ms = (completed_at - started_at) * 1_000
        record_event(
            client.events,
            "backgroundOutputVerified",
            sessionId=session_id,
            markerPrefix=marker,
            attempt=attempt,
            checkedAtUtc=utc_now(),
            elapsedMs=elapsed_ms,
            observedSyntheticLineCount=count,
            withinDeadline=within_deadline,
        )
        if not within_deadline:
            return False
        if count > 0:
            return True

        remaining = deadline - time.perf_counter()
        if remaining <= 0:
            return False
        time.sleep(min(poll_interval_seconds, remaining))


def reconcile_unknown_creates(
    client: RecordingMcpClient,
    attempted_cwds: dict[str, str],
    existing_ids: set[str],
    owned_ids: list[str],
) -> None:
    """只按本轮唯一 cwd 回读未知 create；不会从其他 session 中猜测关闭目标。

    @author kongweiguang
    """

    if not attempted_cwds:
        return
    _, rows = read_session_list(client, "cleanup-create-outcome-readback")
    expected_paths = {
        os.path.normcase(str(Path(path).resolve())): marker
        for path, marker in attempted_cwds.items()
    }
    matches: dict[str, list[str]] = {path: [] for path in expected_paths}
    for row in rows:
        session_id, cwd = row.get("sessionId"), row.get("cwd")
        if isinstance(session_id, str) and isinstance(cwd, str):
            normalized = os.path.normcase(str(Path(cwd).resolve()))
            if normalized in matches:
                matches[normalized].append(session_id)
    for path, ids in matches.items():
        if len(ids) > 1:
            raise AcceptanceFailure("同一 runner-owned cwd 对应多个 session，拒绝猜测关闭")
        if ids and ids[0] not in existing_ids and ids[0] not in owned_ids:
            owned_ids.append(ids[0])
            record_event(
                client.events,
                "unknownCreateReconciledByUniqueCwd",
                sessionId=ids[0],
                cwd=path,
                markerPrefix=expected_paths[path],
            )
        elif ids and ids[0] in existing_ids:
            raise AcceptanceFailure("未知 create readback 命中 root 声明的现存 ID，拒绝关闭")


def observed_unowned_ids(
    actual: set[str], fixed: set[str], target_count: int, reserve_fresh: bool, operation: str
) -> set[str]:
    """要求原有和自有会话持续存在，并最多容忍 root 预留的一个新前台 session。

    @author kongweiguang
    """

    missing = fixed - actual
    if missing:
        raise AcceptanceFailure(f"{operation} 缺少已确认 session：{sorted(missing)}")
    additions = actual - fixed
    if len(additions) > int(reserve_fresh) or len(actual) > target_count:
        raise AcceptanceFailure(f"{operation} 出现未预期的 terminal session：{sorted(additions)}")
    return additions


def main() -> int:
    """准备稳定后台规模并输出精确时间线；结束时只关闭本脚本创建的 session IDs。

    @author kongweiguang
    """

    args = parse_arguments()
    existing_ids, visible_ids, background_count, reserve_fresh, output_dir = validate_options(args)
    run_id = output_dir.name.removeprefix("window-workload-")
    output_dir.mkdir(parents=True, exist_ok=False)
    events = EventWriter(output_dir / "events.jsonl")
    report: dict[str, Any] = {
        "schemaVersion": 1,
        "runId": run_id,
        "startedAtUtc": utc_now(),
        "status": "running",
        "scope": "background headless local PTY load; not GPUI frame or OS presentation timing",
        "limitations": [
            "Foreground echo and GPUI frame percentiles must be read from native-metrics.json.",
            "Existing sessions are never written to or closed by this script.",
        ],
        "targetTotalSessionCount": args.target_total_count,
        "durationSeconds": args.duration_secs,
        "expectedExistingSessionIds": sorted(existing_ids),
        "expectedVisibleSessionIds": sorted(visible_ids),
        "reservedFreshUiSessionCount": int(reserve_fresh),
        "backgroundSessionTarget": background_count,
        "syntheticLinesPerSecondPerBackgroundSession": LINES_PER_SECOND,
        "ownedSessionIds": [],
        "failures": [],
        "cleanup": {"status": "not-started"},
        "sourceSha256": {
            name: sha256_file(Path(__file__).resolve().with_name(name))
            for name in ("window_workload.py", "identity.py", "mcp_client.py")
        },
    }

    client: RecordingMcpClient | None = None
    record: InstanceRecord | None = None
    owned_ids: list[str] = []
    attempted_cwds: dict[str, str] = {}
    inventory_verified = False
    observed_additions: set[str] = set()
    close_uncertain: list[str] = []
    cleanup_errors: list[str] = []
    report_write_failed = False
    try:
        record_path = args.instance_record.expanduser().resolve(strict=True)
        record = read_instance_record(record_path)
        run_root, shell = validate_windows_instance_layout(record_path, record)
        identity = verify_instance(record)
        report["instance"] = {
            **identity,
            "mcpEndpoint": record.endpoint,
            "mcpPort": record.port,
            "configRoot": str(record.config_root),
            "launcherRunRoot": str(run_root),
            "instanceRecord": str(record_path),
            "mcpShellShim": str(shell),
        }
        record_event(events, "identityVerified", identity=report["instance"])
        client = RecordingMcpClient(record, args.request_timeout_secs, events)
        client.verify_live_instance()
        try:
            initialize = client.initialize()
            client.verify_live_instance()
            tools = client.list_tools()
            client.verify_live_instance()
        except Exception as error:
            record_event(events, "mcpHandshakeError", errorType=type(error).__name__, error=str(error))
            raise
        missing_tools = sorted(REQUIRED_TOOLS - tools)
        record_event(
            events,
            "mcpHandshakeResponse",
            protocolVersion=initialize.get("protocolVersion"),
            serverInfo=initialize.get("serverInfo"),
            availableTools=sorted(tools),
            missingTools=missing_tools,
        )
        if missing_tools:
            raise AcceptanceFailure(f"isolated MCP 缺少 terminal tools：{', '.join(missing_tools)}")

        current_ids, _ = read_session_list(client, "preflight-current-session-inventory")
        if current_ids != existing_ids:
            raise AcceptanceFailure(
                f"terminal.list 与 root 提供的 current IDs 不符：expected={sorted(existing_ids)}, "
                f"observed={sorted(current_ids)}"
            )
        validate_runtime(read_runtime(client, "preflight"), len(current_ids), "preflight")
        inventory_verified = True

        cwd_root = output_dir / "session-cwd"
        if background_count:
            cwd_root.mkdir(parents=False, exist_ok=False)
        markers: dict[str, str] = {}
        for index in range(background_count):
            cwd = cwd_root / f"session-{index + 1:03d}"
            cwd.mkdir(parents=False, exist_ok=False)
            marker = f"KWW_{run_id}_{index + 1:03d}"
            attempted_cwds[str(cwd.resolve())] = marker
            data = structured_data(
                client.call_tool(
                    "terminal.create",
                    {
                        "target": "local",
                        "shell": str(shell),
                        "cwd": str(cwd.resolve()),
                        "cols": 120,
                        "rows": 30,
                    },
                )
            )
            session_id = data.get("sessionId")
            if not isinstance(session_id, str) or not session_id:
                raise AcceptanceFailure("terminal.create 未返回有效 sessionId")
            if session_id in existing_ids or session_id in owned_ids:
                raise AcceptanceFailure("terminal.create 返回重复或现存 ID，拒绝将其用于 cleanup")
            owned_ids.append(session_id)
            report["ownedSessionIds"] = list(owned_ids)
            markers[session_id] = marker
            record_event(
                events,
                "ownedSessionCreated",
                sessionId=session_id,
                cwd=str(cwd.resolve()),
                headless=data.get("headless"),
                ownedSessionCount=len(owned_ids),
            )
            if data.get("headless") is not True:
                raise AcceptanceFailure("terminal.create 未确认新 session 为 headless")

        if owned_ids:
            output_seconds = args.duration_secs + OUTPUT_SETUP_MARGIN_SECS
            write_started_ns = time.perf_counter_ns()
            for session_id in owned_ids:
                if session_id in existing_ids:
                    raise AcceptanceFailure("写入门禁拒绝对 existing session 注入数据")
                command = finite_output_command(markers[session_id], output_seconds)
                data = structured_data(
                    client.call_tool(
                        "terminal.write", {"sessionId": session_id, "data": command}
                    )
                )
                expected_bytes = len(command.encode("utf-8"))
                if data.get("bytesWritten") != expected_bytes:
                    raise AcceptanceFailure("owned terminal.write bytesWritten 与命令字节数不一致")
            if (time.perf_counter_ns() - write_started_ns) / 1_000_000_000 > OUTPUT_SETUP_MARGIN_SECS:
                raise AcceptanceFailure("有限输出命令入队超过 setup margin")
            for session_id in owned_ids:
                if not verify_owned_output(client, session_id, markers[session_id]):
                    raise AcceptanceFailure(f"owned session {session_id} 在有界 readiness deadline 内未回读到合成输出")

        fixed_ids = existing_ids | set(owned_ids)
        stable_ids, _ = read_session_list(client, "background-count-stabilized")
        if stable_ids != fixed_ids:
            raise AcceptanceFailure(
                f"背景 session 数未稳定：expected={sorted(fixed_ids)}, observed={sorted(stable_ids)}"
            )
        validate_runtime(read_runtime(client, "background-count-stabilized"), len(fixed_ids), "stabilized")
        ready_ns = time.perf_counter_ns()
        ready_utc = utc_now()
        record_event(
            events,
            "workloadReady",
            stableStartedAtUtc=ready_utc,
            stableStartedMonotonicNs=ready_ns,
            stableBackgroundSessionCount=len(fixed_ids),
            targetTotalSessionCount=args.target_total_count,
            reservedFreshUiSessionCount=int(reserve_fresh),
            ownedSessionIds=list(owned_ids),
        )
        print(
            f"WINDOW_WORKLOAD_READY runId={run_id} evidence={output_dir} "
            f"stableSessions={len(fixed_ids)} target={args.target_total_count}",
            flush=True,
        )

        deadline = time.perf_counter() + args.duration_secs
        next_sample = time.perf_counter() + SAMPLE_INTERVAL_SECS
        sample_index = 0
        while time.perf_counter() < deadline:
            time.sleep(min(max(0.0, next_sample - time.perf_counter()), deadline - time.perf_counter()))
            if time.perf_counter() >= next_sample or time.perf_counter() >= deadline:
                sample_index += 1
                ids, _ = read_session_list(client, f"stable-window-poll-{sample_index:04d}")
                additions = observed_unowned_ids(
                    ids, fixed_ids, args.target_total_count, reserve_fresh, "window-poll"
                )
                observed_additions.update(additions)
                record_event(
                    events,
                    "workloadCountSample",
                    pollIndex=sample_index,
                    terminalSessionCount=len(ids),
                    unownedSessionIds=sorted(additions),
                )
                next_sample += SAMPLE_INTERVAL_SECS

        end_ids, _ = read_session_list(client, "measurement-window-end")
        additions = observed_unowned_ids(
            end_ids, fixed_ids, args.target_total_count, reserve_fresh, "measurement-window-end"
        )
        observed_additions.update(additions)
        if reserve_fresh and len(additions) != 1:
            raise AcceptanceFailure("测量期内未观察到 root 预留的新 UI session")
        if len(end_ids) != args.target_total_count:
            raise AcceptanceFailure("测量结束 terminal.list 总数与目标不符")
        validate_runtime(read_runtime(client, "measurement-window-end"), len(end_ids), "window-end")
        report["measurementTimeline"] = {
            "stableStartedAtUtc": ready_utc,
            "stableStartedMonotonicNs": ready_ns,
            "finishedAtUtc": utc_now(),
            "finishedMonotonicNs": time.perf_counter_ns(),
            "durationSeconds": args.duration_secs,
            "countPollIntervalSeconds": SAMPLE_INTERVAL_SECS,
            "countSamples": sample_index,
            "endSessionIds": sorted(end_ids),
            "observedUnownedSessionIds": sorted(observed_additions),
        }
        report["status"] = "passed"
    except KeyboardInterrupt:
        report["status"] = "failed"
        report["failures"].append({"type": "KeyboardInterrupt", "message": "运行被中断"})
        record_event(events, "runInterrupted")
    except Exception as error:
        report["status"] = "failed"
        report["failures"].append({"type": type(error).__name__, "message": str(error)})
        record_event(events, "phaseFailure", errorType=type(error).__name__, error=str(error))
    finally:
        if client is not None and record is not None and inventory_verified:
            try:
                reconcile_unknown_creates(client, attempted_cwds, existing_ids, owned_ids)
            except Exception as error:
                cleanup_errors.append(f"create-readback:{type(error).__name__}:{error}")
            close_attempted: set[str] = set()
            for session_id in reversed(owned_ids):
                if session_id in existing_ids or session_id in close_attempted:
                    cleanup_errors.append("owned-id-overlapped-existing-or-duplicate")
                    continue
                close_attempted.add(session_id)
                try:
                    data = structured_data(
                        client.call_tool("terminal.close", {"sessionId": session_id})
                    )
                    closed = data.get("closed") is True
                    record_event(events, "ownedSessionCloseResult", sessionId=session_id, closed=closed)
                    if not closed:
                        close_uncertain.append(session_id)
                except Exception as error:
                    close_uncertain.append(session_id)
                    record_event(
                        events,
                        "ownedSessionCloseError",
                        sessionId=session_id,
                        errorType=type(error).__name__,
                        error=str(error),
                    )
            try:
                final_ids, _ = read_session_list(client, "owned-cleanup-readback")
                runtime = read_runtime(client, "owned-cleanup-readback")
                owned_remaining = sorted(set(owned_ids) & final_ids)
                missing_originals = sorted(existing_ids - final_ids)
                if owned_remaining:
                    cleanup_errors.append("owned-sessions-remain:" + ",".join(owned_remaining))
                if missing_originals:
                    cleanup_errors.append("root-declared-sessions-missing:" + ",".join(missing_originals))
                if runtime.get("terminalSessionCount") != len(final_ids):
                    cleanup_errors.append("runtime-terminal-count-mismatch")
                if runtime.get("agentSessionCount") != 0:
                    cleanup_errors.append("unexpected-agent-session-present")
                report["cleanup"] = {
                    "status": "confirmed" if not cleanup_errors else "unconfirmed",
                    "ownedSessionIds": list(owned_ids),
                    "ownedSessionIdsRemaining": owned_remaining,
                    "closeOutcomeUncertainIds": close_uncertain,
                    "rootDeclaredSessionIdsPreserved": sorted(existing_ids & final_ids),
                    "unownedSessionIdsPreserved": sorted(final_ids - existing_ids - set(owned_ids)),
                    "finalSessionCount": len(final_ids),
                    "finalRuntime": runtime,
                    "errors": cleanup_errors,
                }
                if cleanup_errors:
                    report["status"] = "failed"
            except Exception as error:
                cleanup_errors.append(f"readback:{type(error).__name__}:{error}")
                report["cleanup"] = {
                    "status": "unconfirmed",
                    "ownedSessionIds": list(owned_ids),
                    "closeOutcomeUncertainIds": close_uncertain,
                    "errors": cleanup_errors,
                }
                report["status"] = "failed"
        else:
            report["cleanup"] = {
                "status": "no-owned-sessions-or-inventory-not-verified",
                "ownedSessionIds": list(owned_ids),
                "errors": cleanup_errors,
            }

        report["ownedSessionIds"] = list(owned_ids)
        report["finishedAtUtc"] = utc_now()
        record_event(
            events,
            "runFinished",
            status=report["status"],
            cleanupStatus=report["cleanup"]["status"],
        )
        try:
            events.close()
            (output_dir / "report.json").write_text(
                json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
            )
        except Exception as error:
            print(f"无法保存 workload 报告：{type(error).__name__}", file=sys.stderr)
            report_write_failed = True

    if report_write_failed:
        return 2
    print(f"状态：{report['status']}；证据：{output_dir}")
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except AcceptanceFailure as error:
        print(f"workload 参数或隔离边界无效：{error}", file=sys.stderr)
        raise SystemExit(2) from None
