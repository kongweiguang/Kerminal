#!/usr/bin/env python3
"""组合隔离 MCP 回读与既有 SessionService PTY 稳定性 runner。

@author kongweiguang
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import shutil
import socket
import subprocess
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
from mcp_client import EventWriter, McpClient, REQUIRED_TOOLS
from mcp_scenarios import call_list, call_runtime_snapshot, run_mcp_matrix

MAX_OUTPUT_LINES = 1_500
WINDOWS_MCP_SHELL = Path("target/diagnostics/terminal-production-20261020/bin/pwsh.exe")

def collect_host_environment(root: Path) -> dict[str, Any]:
    """记录可复现的主机和源码身份，不泄漏环境变量、用户名或命令行。"""

    facts: dict[str, Any] = {
        "os": platform.platform(),
        "osName": platform.system(),
        "osRelease": platform.release(),
        "architecture": platform.machine(),
        "processor": platform.processor() or None,
        "cpuCount": os.cpu_count(),
        "python": platform.python_version(),
        "hostname": socket.gethostname(),
    }
    facts["gitRevision"] = git_text(root, ["rev-parse", "HEAD"])
    facts["worktreeDirty"] = git_dirty(root)
    return facts


def git_text(root: Path, arguments: list[str]) -> str | None:
    """读取 Git 的单个身份值，不将工作区文件列表或源码内容写入报告。"""

    try:
        result = subprocess.run(
            ["git", "-C", str(root), *arguments],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=10,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    return result.stdout.strip() if result.returncode == 0 else None


def git_dirty(root: Path) -> bool | None:
    """只记录 worktree 是否有差异，不把其他执行者路径或差异明细复制进报告。"""

    status = git_text(root, ["status", "--porcelain", "--untracked-files=normal"])
    return None if status is None else bool(status)


def _parse_cargo_dep_info(dep_text: str) -> tuple[list[str], list[str]]:
    """按 Makefile 规则解析 Cargo dep-info，保留 Windows 路径分隔符并解码转义空格。

    Cargo 的 Windows dep-info 使用反斜杠作为路径分隔符，同时用反斜杠转义空白；
    因此不能全局替换反斜杠或按原始文本做子串匹配。未知转义和残缺规则 fail closed。

    @author kongweiguang
    """
    logical_text = re.sub(r"\\\r?\n[ \t]*", " ", dep_text)
    targets: list[str] = []
    dependencies: list[str] = []

    def find_separator(line: str) -> int | None:
        """只识别规则分隔冒号，不把 Windows drive colon 当成规则边界。

        @author kongweiguang
        """
        index = 0
        while index < len(line):
            char = line[index]
            if char == "\\" and index + 1 < len(line):
                following = line[index + 1]
                if following.isspace() or following in {"#", ":"}:
                    index += 2
                    continue
            if char == ":" and (index + 1 == len(line) or line[index + 1].isspace()):
                return index
            index += 1
        return None

    def tokenize(segment: str) -> list[str]:
        """解码 Makefile 转义空白与注释符，普通反斜杠保持为路径分隔符。

        @author kongweiguang
        """
        tokens: list[str] = []
        current: list[str] = []
        index = 0
        while index < len(segment):
            char = segment[index]
            if char.isspace():
                if current:
                    tokens.append("".join(current))
                    current.clear()
                index += 1
                continue
            if char == "#":
                break
            if char == "\\" and index + 1 < len(segment):
                following = segment[index + 1]
                if following.isspace() or following in {"#", ":"}:
                    current.append(following)
                    index += 2
                    continue
            if char == "$":
                if index + 1 >= len(segment) or segment[index + 1] != "$":
                    raise AcceptanceFailure("runner Cargo dep-info 含未知 Makefile 变量，拒绝核验来源")
                current.append("$")
                index += 2
                continue
            current.append(char)
            index += 1
        if current:
            tokens.append("".join(current))
        return tokens

    for line in logical_text.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        separator = find_separator(line)
        if separator is None:
            raise AcceptanceFailure("runner Cargo dep-info 规则格式无效，拒绝核验来源")
        rule_targets = tokenize(line[:separator])
        rule_dependencies = tokenize(line[separator + 1 :])
        if not rule_targets:
            raise AcceptanceFailure("runner Cargo dep-info 缺少规则目标，拒绝核验来源")
        targets.extend(rule_targets)
        dependencies.extend(rule_dependencies)
    if not targets or not dependencies:
        raise AcceptanceFailure("runner Cargo dep-info 缺少依赖项，拒绝核验来源")
    return targets, dependencies


def _canonical_dep_info_path(path_text: str, workspace: Path) -> Path | None:
    """将 dep-info 路径按当前平台规则解析为真实路径，拒绝靠文本相似度认领源码。

    相对路径只相对 worktree 根解析；严格解析会跟随 junction/symlink，使逃逸 checkout
    即使使用等价的 `..` 文本也无法伪装成本 worktree 源码。

    @author kongweiguang
    """
    candidate = Path(path_text)
    if not candidate.is_absolute():
        candidate = workspace / candidate
    try:
        return candidate.resolve(strict=True)
    except (OSError, RuntimeError):
        return None


def _has_path_component(path_text: str, component: str) -> bool:
    """跨 Windows 与 POSIX 分隔符检查完整路径段，避免把 baseline 子串漏过。

    @author kongweiguang
    """
    return component.casefold() in {
        part.casefold() for part in re.split(r"[\\/]+", path_text) if part
    }


def verify_runner_provenance(root: Path, runner_executable: Path) -> dict[str, Any]:
    """只接受精确解析到本 worktree 源文件的 runner，阻止 Cargo 跨 checkout 复用缓存。

    dep-info 的转义文本不能作为路径证据；必须逐依赖项规范化并与期望源码绝对路径
    精确比较。遇到 baseline 或同名异路径依赖立即拒绝，以保持来源门禁 fail closed。

    @author kongweiguang
    """

    workspace = root.resolve(strict=True)
    try:
        runner = runner_executable.resolve(strict=True)
    except OSError:
        raise AcceptanceFailure(f"预构建 runner 不存在：{runner_executable}") from None
    try:
        expected_runner_dir = (
            workspace / "target" / "release" / "examples"
        ).resolve(strict=True)
        dep_info = runner.with_suffix(".d").resolve(strict=True)
    except OSError:
        raise AcceptanceFailure("runner 或相邻 Cargo dep-info 不存在，拒绝使用无法核验来源的二进制") from None
    if not is_same_or_child(runner, expected_runner_dir) or not runner.is_file():
        raise AcceptanceFailure("预构建 runner 不在当前 worktree 的 target/release/examples 下")
    if not is_same_or_child(dep_info, expected_runner_dir) or not dep_info.is_file():
        raise AcceptanceFailure("runner Cargo dep-info 不在当前 worktree 的 target/release/examples 下")

    try:
        dep_text = dep_info.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise AcceptanceFailure(
            f"runner Cargo dep-info 缺失或不可读，拒绝使用无法核验来源的二进制（{type(error).__name__}）"
        ) from None
    targets, dependencies = _parse_cargo_dep_info(dep_text)
    if any(_has_path_component(item, "baseline") for item in (*targets, *dependencies)):
        raise AcceptanceFailure("runner Cargo dep-info 引用了 baseline checkout，拒绝启动此候选")

    try:
        source = (workspace / "tests" / "acceptance" / "session_runtime.rs").resolve(
            strict=True
        )
    except OSError:
        raise AcceptanceFailure("当前 worktree 的 PTY acceptance 源文件不存在") from None
    expected_source = os.path.normcase(str(source))
    matching_source_seen = False
    expected_name = source.name.casefold()
    for dependency in dependencies:
        if Path(dependency).name.casefold() != expected_name:
            continue
        resolved_dependency = _canonical_dep_info_path(dependency, workspace)
        if resolved_dependency is None:
            raise AcceptanceFailure("runner Cargo dep-info 的同名 acceptance 源无法解析，拒绝启动此候选")
        if _has_path_component(str(resolved_dependency), "baseline"):
            raise AcceptanceFailure("runner Cargo dep-info 引用了 baseline checkout，拒绝启动此候选")
        if os.path.normcase(str(resolved_dependency)) != expected_source:
            raise AcceptanceFailure("runner Cargo dep-info 引用了 worktree 外的同名 acceptance 源，拒绝启动此候选")
        matching_source_seen = True
    if not matching_source_seen:
        raise AcceptanceFailure(
            "runner Cargo dep-info 未引用当前 worktree 的 tests/acceptance/session_runtime.rs"
        )
    return {
        "runnerPath": str(runner),
        "runnerSha256": sha256_file(runner),
        "depInfoPath": str(dep_info.resolve(strict=True)),
        "depInfoSha256": sha256_file(dep_info),
        "workspaceSourcePath": str(source),
        "workspaceSourceReferenced": True,
        "baselinePathReferenced": False,
    }


def validate_options(
    args: argparse.Namespace,
) -> tuple[list[int], str, str | None]:
    """先校验规模与平台 shell 边界；MCP 用 NoProfile shim，PTY runner 单独用真实解释器。"""

    try:
        counts = [int(part.strip()) for part in args.session_counts.split(",")]
    except ValueError:
        raise AcceptanceFailure("--session-counts 格式应为 1,10,50") from None
    if counts != [1, 10, 50]:
        raise AcceptanceFailure("生产验收规模固定为 --session-counts 1,10,50")
    if args.churn_cycles < 1 or args.churn_cycles > 100_000:
        raise AcceptanceFailure("--churn-cycles 必须在 1..=100000")
    if args.mixed_duration_secs < 0 or args.mixed_duration_secs > 172_800:
        raise AcceptanceFailure("--mixed-duration-secs 必须在 0..=172800")
    if args.mixed_sessions < 1 or args.mixed_sessions > 256:
        raise AcceptanceFailure("--mixed-sessions 必须在 1..=256")
    if args.sample_interval_ms < 50 or args.sample_interval_ms > 60_000:
        raise AcceptanceFailure("--sample-interval-ms 必须在 50..=60000")
    if args.integrity_lines < 1 or args.integrity_lines > MAX_OUTPUT_LINES:
        raise AcceptanceFailure(f"--integrity-lines 必须在 1..={MAX_OUTPUT_LINES}")
    if args.io_timeout_secs < 1 or args.io_timeout_secs > 300:
        raise AcceptanceFailure("--io-timeout-secs 必须在 1..=300")
    if args.request_timeout_secs < 1 or args.request_timeout_secs > 300:
        raise AcceptanceFailure("--request-timeout-secs 必须在 1..=300")
    if os.name == "nt":
        shim_path = (repository_root() / WINDOWS_MCP_SHELL).resolve()
        shell_path = Path(args.shell).expanduser() if args.shell else shim_path
        if not shell_path.is_absolute():
            raise AcceptanceFailure("Windows MCP --shell 必须是隔离 pwsh shim 的绝对路径")
        try:
            shell_path = shell_path.resolve(strict=True)
        except OSError:
            raise AcceptanceFailure(
                f"Windows MCP NoProfile shim 缺失；请先编译 {shim_path} 并通过 --shell 指定"
            ) from None
        if os.path.normcase(str(shell_path)) != os.path.normcase(str(shim_path)):
            raise AcceptanceFailure(
                f"Windows MCP --shell 只能使用本任务 NoProfile shim：{shim_path}"
            )
        if not shell_path.is_file():
            raise AcceptanceFailure("Windows MCP NoProfile shim 路径不是文件")
        mcp_shell = str(shell_path)
    else:
        mcp_shell = args.shell or "/bin/bash"
        shell_path = Path(mcp_shell)
        if not shell_path.is_absolute() or not shell_path.is_file() or not os.access(shell_path, os.X_OK):
            raise AcceptanceFailure("Linux MCP --shell 必须是存在且可执行的 POSIX 绝对路径")

    runtime_shell: str | None = None
    if not args.skip_session_runtime:
        if args.runtime_shell:
            runtime_path = Path(args.runtime_shell).expanduser()
        elif os.name == "nt":
            resolved = shutil.which("pwsh.exe")
            if not resolved:
                raise AcceptanceFailure("PTY runner 需要 PATH 中可解析的真实 pwsh.exe")
            runtime_path = Path(resolved)
        else:
            runtime_path = Path(shutil.which("bash") or "/bin/bash")
        if not runtime_path.is_absolute() or not runtime_path.is_file():
            raise AcceptanceFailure("--runtime-shell 必须是存在的解释器绝对路径")
        if os.name == "nt":
            if runtime_path.stem.casefold() not in {"pwsh", "powershell"}:
                raise AcceptanceFailure("Windows PTY runner 仅支持 PowerShell 解释器")
            if os.path.normcase(str(runtime_path.resolve())) == os.path.normcase(mcp_shell):
                raise AcceptanceFailure("PTY runner 必须使用真实 PowerShell，不能使用 MCP shim")
        elif not os.access(runtime_path, os.X_OK):
            raise AcceptanceFailure("Linux --runtime-shell 必须可执行并使用 POSIX dialect")
        runtime_shell = str(runtime_path.resolve())
    return counts, mcp_shell, runtime_shell


def parse_arguments() -> argparse.Namespace:
    """提供可配置时长与 shell 参数，同时将默认值设为完整两小时验收。"""

    parser = argparse.ArgumentParser(
        description="运行隔离 Kerminal GPUI 后台终端 production acceptance harness。"
    )
    parser.add_argument("--instance-record", required=True, type=Path)
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--session-counts", default="1,10,50")
    parser.add_argument("--integrity-lines", type=int, default=128)
    parser.add_argument("--churn-cycles", type=int, default=100)
    parser.add_argument("--mixed-duration-secs", type=int, default=7200)
    parser.add_argument("--mixed-sessions", type=int, default=10)
    parser.add_argument("--sample-interval-ms", type=int, default=1000)
    parser.add_argument("--throughput-lines", type=int, default=128)
    parser.add_argument("--io-timeout-secs", type=float, default=30.0)
    parser.add_argument("--request-timeout-secs", type=float, default=30.0)
    parser.add_argument("--shell", help="MCP 创建终端使用的 shell；Windows 必须是本任务 NoProfile shim")
    parser.add_argument("--runtime-shell", help="既有 PTY runner 使用的真实 shell；Windows 默认解析 PATH 中的 pwsh.exe")
    parser.add_argument("--skip-session-runtime", action="store_true")
    return parser.parse_args()


def default_output_dir(root: Path, run_id: str) -> Path:
    """将报告置于仓库忽略的 acceptance 证据目录并按 run id 隔离。"""

    return root / "target" / "acceptance" / "terminal-production" / run_id


def run_session_runtime(
    root: Path,
    output_dir: Path,
    events: EventWriter,
    session_counts: list[int],
    args: argparse.Namespace,
    runtime_shell: str,
) -> dict[str, Any]:
    """直接运行预构建 PTY example，避免 cargo 子进程及运行时隐式编译。"""

    runner_name = "session-acceptance.exe" if os.name == "nt" else "session-acceptance"
    runner_executable = root / "target" / "release" / "examples" / runner_name
    if not runner_executable.is_file() or (
        os.name != "nt" and not os.access(runner_executable, os.X_OK)
    ):
        raise AcceptanceFailure(
            "预构建 session-acceptance runner 缺失；请先运行 "
            "cargo build --locked --release -p kerminal-infrastructure --example session-acceptance"
        )
    try:
        provenance = verify_runner_provenance(root, runner_executable)
    except AcceptanceFailure as error:
        events.write(
            {
                "kind": "runnerProvenanceRejected",
                "runnerPath": str(runner_executable),
                "error": str(error),
            }
        )
        raise

    runner_dir = output_dir / "session-runtime"
    runner_dir.mkdir(parents=True, exist_ok=False)
    command = [
        str(runner_executable),
        "--session-counts",
        ",".join(str(value) for value in session_counts),
        "--churn-cycles",
        str(args.churn_cycles),
        "--mixed-duration-secs",
        str(args.mixed_duration_secs),
        "--mixed-sessions",
        str(args.mixed_sessions),
        "--sample-interval-ms",
        str(args.sample_interval_ms),
        "--throughput-lines",
        str(args.throughput_lines),
        "--shell-program",
        runtime_shell,
        "--output-dir",
        str(runner_dir),
    ]
    env = os.environ.copy()
    started = time.perf_counter()
    log_path = output_dir / "session-runtime.log"
    with log_path.open("x", encoding="utf-8", newline="\n") as log:
        try:
            completed = subprocess.run(
                command,
                cwd=root,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=log,
                stderr=subprocess.STDOUT,
                timeout=args.mixed_duration_secs + 3600,
                check=False,
            )
            exit_code = completed.returncode
        except subprocess.TimeoutExpired:
            exit_code = None
            events.write(
                {
                    "kind": "runnerFailure",
                    "error": "session-runtime-timeout",
                    "timeoutSeconds": args.mixed_duration_secs + 3600,
                }
            )
    reports = sorted(runner_dir.glob("session-acceptance-*.json"))
    runner_report = None
    if reports:
        try:
            runner_report = json.loads(reports[-1].read_text(encoding="utf-8"))
        except (OSError, UnicodeError, json.JSONDecodeError):
            runner_report = None
    summary = {
        "command": command,
        "provenance": provenance,
        "exitCode": exit_code,
        "elapsedSeconds": time.perf_counter() - started,
        "logPath": str(log_path),
        "reportPaths": [str(path) for path in reports],
        "report": runner_report,
        "status": "passed" if exit_code == 0 and isinstance(runner_report, dict) else "failed",
    }
    events.write(
        {
            "kind": "sessionRuntimeRunner",
            "provenance": provenance,
            "exitCode": exit_code,
            "elapsedSeconds": summary["elapsedSeconds"],
            "reportPaths": summary["reportPaths"],
            "runnerStatus": runner_report.get("status") if isinstance(runner_report, dict) else None,
        }
    )
    if summary["status"] != "passed":
        raise AcceptanceFailure("既有 session-runtime runner 未生成成功报告；查看 session-runtime.log")
    return summary


def main() -> int:
    """以 fail-closed 顺序执行身份检查、MCP smoke 和现有长稳 runner 并保存总报告。"""

    args = parse_arguments()
    root = repository_root()
    run_id = uuid.uuid4().hex
    output_dir = (args.output_dir or default_output_dir(root, run_id)).expanduser().resolve()
    evidence_root = (root / "target" / "acceptance" / "terminal-production").resolve()
    if not is_same_or_child(output_dir, evidence_root) or output_dir == evidence_root:
        raise AcceptanceFailure("--output-dir 必须位于仓库 target/acceptance/terminal-production 下")
    output_dir.mkdir(parents=True, exist_ok=False)
    events = EventWriter(output_dir / "events.jsonl")
    record: InstanceRecord | None = None
    option_record = {
        key: str(value) if isinstance(value, Path) else value
        for key, value in vars(args).items()
    }
    report: dict[str, Any] = {
        "schemaVersion": 1,
        "runId": run_id,
        "startedAtUtc": utc_now(),
        "status": "running",
        "scope": "isolated GPUI app MCP + existing SessionService PTY acceptance",
        "limitations": [
            "MCP and PTY response timings are backend measurements, not GPUI UI frame latency.",
            "Physical keyboard-to-presentation, IME, mouse selection, scroll, visual quality, and native TUI interaction are not measured by this harness.",
            "The isolated configRoot comes from the launcher instance record; this script does not read the target process environment block.",
        ],
        "options": option_record,
        "host": collect_host_environment(root),
        "sourceAndBinary": {
            "harnessModuleSha256": {
                module: sha256_file(Path(__file__).with_name(module))
                for module in ["run.py", "identity.py", "mcp_client.py", "mcp_scenarios.py"]
            }
        },
        "instance": None,
        "mcp": None,
        "sessionRuntime": None,
        "failures": [],
    }
    client: McpClient | None = None
    cleanup_status = "not-started"
    report_write_failed = False
    try:
        record = read_instance_record(args.instance_record.expanduser().resolve(strict=True))
        counts, mcp_shell, runtime_shell = validate_options(args)
        identity = verify_instance(record)
        report["instance"] = {
            **identity,
            "mcpEndpoint": record.endpoint,
            "mcpPort": record.port,
            "configRoot": str(record.config_root),
            "configRootSource": str(args.instance_record.resolve()),
        }
        report["options"].update(
            {
                "sessionCounts": counts,
                "mcpShell": mcp_shell,
                "runtimeShell": runtime_shell,
                "churnCycles": args.churn_cycles,
                "mixedDurationSeconds": args.mixed_duration_secs,
                "mixedSessions": args.mixed_sessions,
                "sampleIntervalMs": args.sample_interval_ms,
                "throughputLines": args.throughput_lines,
                "integrityLines": args.integrity_lines,
            }
        )
        events.write({"kind": "identityVerified", "instance": report["instance"]})

        client = McpClient(record, args.request_timeout_secs)
        client.verify_live_instance()
        initialize = client.initialize()
        client.verify_live_instance()
        tools = client.list_tools()
        missing = sorted(REQUIRED_TOOLS - tools)
        events.write(
            {
                "kind": "mcpInitialized",
                "protocolVersion": initialize.get("protocolVersion"),
                "serverInfo": initialize.get("serverInfo"),
                "requiredTools": sorted(REQUIRED_TOOLS),
                "missingTools": missing,
            }
        )
        if missing:
            raise AcceptanceFailure(f"隔离 MCP 运行时缺少 terminal 工具：{', '.join(missing)}")

        mcp_result = run_mcp_matrix(
            client,
            events,
            output_dir,
            run_id,
            counts,
            args.integrity_lines,
            args.io_timeout_secs,
            mcp_shell,
        )
        report["mcp"] = mcp_result
        cleanup_status = "MCP session matrix cleanup confirmed"

        if args.skip_session_runtime:
            report["sessionRuntime"] = {"status": "skipped-by-explicit-option"}
            report["status"] = "partial"
        else:
            report["sessionRuntime"] = run_session_runtime(
                root,
                output_dir,
                events,
                counts,
                args,
                runtime_shell or "",
            )
            report["status"] = "passed"
    except KeyboardInterrupt:
        report["failures"].append({"type": "KeyboardInterrupt", "message": "运行被中断"})
        report["status"] = "failed"
    except Exception as error:
        report["failures"].append(
            {"type": type(error).__name__, "message": str(error)}
        )
        events.write(
            {"kind": "runFailure", "errorType": type(error).__name__, "error": str(error)}
        )
        report["status"] = "failed"
    finally:
        if client is not None and record is not None:
            try:
                # 读取收尾状态；未知 close 不会重发，前序阶段已对已知 session 做 close。
                final_identity = verify_instance(record)
                if final_identity["executableSha256"] != report["instance"]["executableSha256"]:
                    raise AcceptanceFailure("隔离 app 二进制摘要在运行期间发生变化")
                report["instanceFinal"] = final_identity
                final_ids = call_list(client, events, "final-cleanup-readback")
                final_runtime = call_runtime_snapshot(client, events, "final-cleanup-readback")
                report["finalTerminalSessionIds"] = sorted(final_ids)
                report["finalRuntime"] = final_runtime
                if final_ids or final_runtime.get("terminalSessionCount") != 0:
                    cleanup_status = "unconfirmed: terminal sessions remain"
                    report["status"] = "failed"
                elif cleanup_status == "not-started":
                    cleanup_status = "no sessions created"
                else:
                    cleanup_status = "terminal.list confirmed no session remains"
            except Exception as error:
                cleanup_status = f"unconfirmed: {type(error).__name__}"
                report["failures"].append(
                    {"type": type(error).__name__, "message": f"最终 cleanup readback 失败：{error}"}
                )
                report["status"] = "failed"
        report["resourceCleanup"] = cleanup_status
        report["finishedAtUtc"] = utc_now()
        try:
            events.write(
                {
                    "kind": "runFinished",
                    "status": report["status"],
                    "resourceCleanup": cleanup_status,
                }
            )
            events.close()
            report_path = output_dir / "report.json"
            report_path.write_text(
                json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
            )
        except Exception as error:
            print(f"无法保存完整验收报告：{type(error).__name__}", file=sys.stderr)
            report_write_failed = True
    if report_write_failed:
        return 2
    print(f"状态：{report['status']}；报告：{output_dir / 'report.json'}")
    return 0 if report["status"] in {"passed", "partial"} else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except AcceptanceFailure as error:
        print(f"验收参数或隔离边界无效：{error}", file=sys.stderr)
        raise SystemExit(2) from None
