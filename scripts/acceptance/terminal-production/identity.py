#!/usr/bin/env python3
"""校验隔离实例记录，并核对 MCP loopback 端口的真实进程 owner。

@author kongweiguang
"""

from __future__ import annotations

import base64
import datetime as dt
import hashlib
import json
import math
import os
import re
import shutil
import socket
import stat
import subprocess
import sys
from dataclasses import dataclass
from decimal import Decimal
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit


PROTECTED_MCP_PORT = 37657
LINUX_RUN_ROOT_PATTERN = re.compile(
    r"kerminal-gpui-terminal-production-20261020-[A-Za-z0-9]{8}\Z"
)


class AcceptanceFailure(RuntimeError):
    """报告不含请求正文和进程命令行的稳定验收错误。"""


@dataclass(frozen=True)
class InstanceRecord:
    """由启动协调器提供的隔离实例身份声明。"""

    pid: int
    executable_path: Path
    started_at_utc: dt.datetime
    endpoint: str
    config_root: Path
    host: str
    port: int
    start_ticks: int | None


def repository_root() -> Path:
    """从固定脚本位置取仓库根，避免依赖调用者当前目录或猜测工作区。"""

    return Path(__file__).resolve().parents[3]


def utc_now() -> str:
    """统一用带 UTC 时区的 ISO 时间标记报告和逐项事件。"""

    return dt.datetime.now(dt.timezone.utc).isoformat(timespec="milliseconds")


def sha256_file(path: Path) -> str:
    """分块计算可执行文件摘要，避免将大型桌面二进制整体读入内存。"""

    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def read_instance_record(path: Path) -> InstanceRecord:
    """按平台验证隔离根，Linux 仅放行本任务记录绑定的 WSL ext4 runRoot。

    Windows 继续限制在 worktree diagnostics；Linux 例外同时要求固定运行目录、
    launch.json 位置和私有目录权限，避免把任意缓存目录或正式配置当作验收目标。

    @author kongweiguang
    """

    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise AcceptanceFailure(f"实例记录不可读或 JSON 无效（{type(error).__name__}）") from None
    if not isinstance(raw, dict):
        raise AcceptanceFailure("实例记录必须是 JSON object")
    required = {"pid", "executablePath", "startedAtUtc", "mcpEndpoint", "configRoot"}
    if not required.issubset(raw):
        raise AcceptanceFailure("实例记录缺少 pid、executablePath、startedAtUtc、mcpEndpoint 或 configRoot")
    pid = raw["pid"]
    if not isinstance(pid, int) or isinstance(pid, bool) or pid <= 0:
        raise AcceptanceFailure("实例记录 pid 必须是正整数")
    start_ticks = raw.get("startTicks")
    if start_ticks is not None and (
        not isinstance(start_ticks, int) or isinstance(start_ticks, bool) or start_ticks <= 0
    ):
        raise AcceptanceFailure("实例记录 startTicks 必须是正整数")
    try:
        started = dt.datetime.fromisoformat(str(raw["startedAtUtc"]).replace("Z", "+00:00"))
    except ValueError:
        raise AcceptanceFailure("实例记录 startedAtUtc 必须是 ISO 8601 时间") from None
    if started.tzinfo is None:
        raise AcceptanceFailure("实例记录 startedAtUtc 必须包含时区")

    endpoint = raw["mcpEndpoint"]
    if not isinstance(endpoint, str):
        raise AcceptanceFailure("实例记录 mcpEndpoint 必须是字符串")
    parsed = urlsplit(endpoint)
    if (
        parsed.scheme != "http"
        or parsed.hostname != "127.0.0.1"
        or parsed.username is not None
        or parsed.password is not None
        or parsed.path != "/mcp"
        or parsed.query
        or parsed.fragment
    ):
        raise AcceptanceFailure("MCP endpoint 必须精确使用 http://127.0.0.1:<port>/mcp")
    try:
        port = parsed.port
    except ValueError:
        raise AcceptanceFailure("MCP endpoint 端口无效") from None
    if port is None or port <= 0 or port > 65535:
        raise AcceptanceFailure("MCP endpoint 必须包含有效的非零端口")
    if port == PROTECTED_MCP_PORT:
        raise AcceptanceFailure("拒绝连接正式 MCP 端口 37657")

    executable_value = raw["executablePath"]
    config_value = raw["configRoot"]
    if not isinstance(executable_value, str) or not isinstance(config_value, str):
        raise AcceptanceFailure("executablePath 和 configRoot 必须是绝对路径字符串")
    executable_input = Path(executable_value)
    config_root_input = Path(config_value)
    if not executable_input.is_absolute() or not config_root_input.is_absolute():
        raise AcceptanceFailure("executablePath 和 configRoot 必须是绝对路径")
    try:
        record_path = path.resolve(strict=True)
        executable = executable_input.resolve(strict=True)
        config_root = config_root_input.resolve(strict=True)
    except OSError as error:
        raise AcceptanceFailure(f"实例记录、二进制或隔离配置根不存在（{type(error).__name__}）") from None
    if not executable.is_file() or not config_root.is_dir():
        raise AcceptanceFailure("实例二进制必须是文件，configRoot 必须是已存在目录")
    protected_root = (Path.home() / ".kerminal").resolve()
    if is_same_or_child(config_root, protected_root):
        raise AcceptanceFailure("拒绝把用户正式 ~/.kerminal 当作验收 configRoot")
    if sys.platform.startswith("linux"):
        validate_linux_run_layout(record_path, raw, executable, config_root)
    else:
        task_config_root = (
            repository_root() / "target" / "diagnostics" / "terminal-production-20261020"
        ).resolve(strict=True)
        if config_root == task_config_root or not is_same_or_child(config_root, task_config_root):
            raise AcceptanceFailure(
                "configRoot 必须是本 worktree target/diagnostics/terminal-production-20261020 下的隔离子目录"
            )
    return InstanceRecord(
        pid, executable, started, endpoint, config_root, parsed.hostname, port, start_ticks
    )


def validate_linux_run_layout(
    record_path: Path,
    raw: dict[str, Any],
    executable: Path,
    config_root: Path,
) -> None:
    """只接受 launcher 记录绑定的唯一 ext4 runRoot，并验证 owner 与写权限边界。

    固定 home/.cache 前缀、随机 run 名、worktree launch.json 位置和 config/bin
    相对关系共同构成允许条件；父目录可读但不可被组或其他用户写，运行子树仅 owner 可访问。

    @author kongweiguang
    """

    if raw.get("schemaVersion") != 1 or raw.get("owner") != "terminal-production-20261020":
        raise AcceptanceFailure("Linux 隔离实例必须来自本任务 schemaVersion 1 launcher 记录")
    run_root_value = raw.get("runRoot")
    if not isinstance(run_root_value, str):
        raise AcceptanceFailure("Linux launcher 记录必须包含 runRoot")
    run_root_input = Path(run_root_value)
    if not run_root_input.is_absolute() or not LINUX_RUN_ROOT_PATTERN.fullmatch(run_root_input.name):
        raise AcceptanceFailure("Linux runRoot 必须使用本任务唯一隔离目录名")

    try:
        home_input = Path.home()
        home = home_input.resolve(strict=True)
        cache_input = home / ".cache"
        cache_root = cache_input.resolve(strict=True)
        run_root = run_root_input.resolve(strict=True)
        diagnostics_runs = (
            repository_root() / "target" / "diagnostics" / "terminal-production-20261020" / "runs"
        )
        diagnostics_runs_resolved = diagnostics_runs.resolve(strict=True)
    except OSError as error:
        raise AcceptanceFailure(f"Linux 隔离目录或 worktree 记录目录不可读（{type(error).__name__}）") from None

    if home_input != home or cache_input != cache_root or run_root_input != run_root:
        raise AcceptanceFailure("Linux 隔离路径不得经过符号链接或非规范路径")
    if run_root.parent != cache_root:
        raise AcceptanceFailure("Linux runRoot 必须是当前用户 home/.cache 下的本任务隔离目录")
    if str(run_root_input) != run_root_value:
        raise AcceptanceFailure("Linux launcher runRoot 必须使用规范绝对路径")

    expected_record = diagnostics_runs_resolved / run_root.name / "launch.json"
    if diagnostics_runs != diagnostics_runs_resolved or record_path != expected_record:
        raise AcceptanceFailure("Linux launch.json 必须位于本 worktree 对应 runRoot 的 diagnostics 记录目录")
    expected_config = run_root / "config"
    expected_executable = run_root / "bin" / "kerminal"
    if raw.get("configRoot") != str(expected_config) or config_root != expected_config:
        raise AcceptanceFailure("Linux configRoot 必须精确对应 launcher runRoot/config")
    if raw.get("executablePath") != str(expected_executable) or executable != expected_executable:
        raise AcceptanceFailure("Linux executablePath 必须精确对应 launcher runRoot/bin/kerminal")

    uid = os.getuid()
    _require_linux_path_security(home, uid, directory=True, private=False)
    _require_linux_path_security(cache_root, uid, directory=True, private=False)
    _require_linux_path_security(run_root, uid, directory=True, private=True)
    _require_linux_path_security(expected_config, uid, directory=True, private=True)
    _require_linux_path_security(run_root / "bin", uid, directory=True, private=True)
    _require_linux_path_security(expected_executable, uid, directory=False, private=False)
    if not os.access(expected_executable, os.X_OK):
        raise AcceptanceFailure("Linux runRoot 中的候选二进制必须可执行")


def _require_linux_path_security(
    path: Path,
    uid: int,
    *,
    directory: bool,
    private: bool,
) -> None:
    """拒绝符号链接、跨 owner 路径和可被其他用户改写的 Linux 隔离节点。

    launcher 的 ext4 runRoot/config/bin 使用 0700；home/cache 可按常见权限读取，
    但所有节点都必须由当前用户持有且禁止组或其他用户写入。

    @author kongweiguang
    """

    try:
        metadata = path.lstat()
    except OSError as error:
        raise AcceptanceFailure(f"Linux 隔离路径不可读取（{type(error).__name__}）") from None
    expected_kind = stat.S_ISDIR(metadata.st_mode) if directory else stat.S_ISREG(metadata.st_mode)
    if stat.S_ISLNK(metadata.st_mode) or not expected_kind:
        raise AcceptanceFailure("Linux 隔离路径类型无效或包含符号链接")
    if metadata.st_uid != uid:
        raise AcceptanceFailure("Linux 隔离路径必须由当前 launcher 用户持有")
    mode = stat.S_IMODE(metadata.st_mode)
    if mode & 0o022 or (private and mode & 0o077):
        raise AcceptanceFailure("Linux 隔离路径权限过宽")


def is_same_or_child(path: Path, root: Path) -> bool:
    """比较规范化路径边界，避免前缀相似路径绕过正式配置根保护。"""

    try:
        path.relative_to(root)
        return True
    except ValueError:
        return False


def parse_iso_utc(value: str) -> dt.datetime:
    """解析带时区时间并统一成 UTC，裁去 Python 无法表示的亚微秒尾数。

    PowerShell round-trip 格式可带 7 位 100ns 小数，而 datetime 的精度为微秒；
    截断多余尾数保留可表示的创建时间，避免 Unicode 回退路径在解析阶段失败。

    @author kongweiguang
    """

    normalized = value.replace("Z", "+00:00")
    normalized = re.sub(
        r"(?<=\.\d{6})\d+(?=(?:[+-]\d{2}:\d{2})?$)",
        "",
        normalized,
        count=1,
    )
    parsed = dt.datetime.fromisoformat(normalized)
    if parsed.tzinfo is None:
        raise AcceptanceFailure("时间记录缺少时区")
    return parsed.astimezone(dt.timezone.utc)


def process_identity(pid: int, port: int) -> dict[str, Any]:
    """只读取监听 PID、可执行路径和创建时间，不采集进程命令行或环境变量。"""

    if os.name == "nt":
        return windows_process_identity(pid, port)
    return linux_process_identity(pid, port)


def windows_process_identity(pid: int, port: int) -> dict[str, Any]:
    """优先用可选 psutil 读取身份，能力不足时保留既有 PowerShell 查询。

    矩阵重复核验会频繁检查同一类 loopback listener；psutil 避免每次启动 PowerShell，
    但只有能确认唯一 owner 且两次新 Process 读取到相同创建时间时才采用快路径。
    缺依赖、权限受限或信息不完整时回退到保留 ASCII envelope 的 PowerShell 实现。

    @author kongweiguang
    """

    try:
        import psutil
    except ImportError:
        return _windows_process_identity_powershell(pid, port)

    fast_identity = _windows_process_identity_psutil(pid, port, psutil)
    if fast_identity is not None:
        return fast_identity
    return _windows_process_identity_powershell(pid, port)


def _windows_process_identity_psutil(
    pid: int,
    port: int,
    psutil_module: Any,
) -> dict[str, Any] | None:
    """只在可完整证明 loopback owner 与 PID 代次稳定时返回 psutil 结果。

    系统连接表中无法归属 PID 的匹配 socket 会触发慢速权威回退；已观察到其他
    owner 或同 PID 创建时间变化则是确定冲突，直接拒绝而不能让回退掩盖竞态。

    @author kongweiguang
    """

    try:
        before_process = psutil_module.Process(pid)
        created_before = float(before_process.create_time())
        executable_path = before_process.exe()
        connections = psutil_module.net_connections(kind="inet4")
        owners: set[int] = set()
        matching_connections = 0
        unknown_owner = False
        for connection in connections:
            if (
                connection.family != socket.AF_INET
                or connection.type != socket.SOCK_STREAM
                or connection.status != psutil_module.CONN_LISTEN
            ):
                continue
            local_address = connection.laddr
            if hasattr(local_address, "ip"):
                address = local_address.ip
                local_port = local_address.port
            else:
                address, local_port = local_address[0], local_address[1]
            if address != "127.0.0.1" or local_port != port:
                continue
            matching_connections += 1
            owner = connection.pid
            if owner is None:
                unknown_owner = True
            else:
                owners.add(int(owner))

        # Construct a new Process: create_time is cached per Process instance by psutil.
        after_process = psutil_module.Process(pid)
        created_after = float(after_process.create_time())
        if (
            not math.isfinite(created_before)
            or not math.isfinite(created_after)
            or created_before != created_after
            or not after_process.is_running()
        ):
            raise AcceptanceFailure("Windows PID 在读取期间退出或被复用")
        if unknown_owner:
            return None
        if matching_connections == 0:
            # Empty/incomplete tables are not proof that the port has no listener.
            return None
        if owners != {pid}:
            raise AcceptanceFailure(f"endpoint listener owner 与预期 PID 不一致：observed={sorted(owners)}")
        if not isinstance(executable_path, str) or not executable_path:
            return None

        epoch_microseconds = int(Decimal(str(created_after)) * 1_000_000)
        started = dt.datetime(1970, 1, 1, tzinfo=dt.timezone.utc) + dt.timedelta(
            microseconds=epoch_microseconds
        )
        started_at_utc = started.isoformat(timespec="microseconds")
        return {
            "listeners": [pid],
            "pid": pid,
            "executablePath": executable_path,
            "startedAtUtc": started_at_utc,
        }
    except AcceptanceFailure:
        raise
    except (
        psutil_module.Error,
        OSError,
        AttributeError,
        IndexError,
        NotImplementedError,
        OverflowError,
        TypeError,
        ValueError,
    ):
        return None


def _windows_process_identity_powershell(pid: int, port: int) -> dict[str, Any]:
    """用 PowerShell 取得完整 TCP owner，并以 ASCII envelope 保持 Unicode 路径准确。

    Python 在 Windows 上可能采用系统 ANSI 编码解码子进程文本；在 PowerShell 中先将
    JSON 编为 UTF-8 Base64，stdout 仅含 ASCII。该查询也是 psutil 缺失或数据不完整时的
    既有权威后备路径。

    @author kongweiguang
    """

    powershell = shutil.which("pwsh") or shutil.which("powershell.exe")
    if not powershell:
        raise AcceptanceFailure("需要 PowerShell 7 或 Windows PowerShell 5.1 读取 TCP owner")
    # PID 与端口均已严格转成整数；固定脚本不拼接任何路径、命令行或用户输入。
    command = (
        "$ErrorActionPreference='Stop'; "
        f"$pidValue={pid}; $portValue={port}; "
        "$listeners=@(Get-NetTCPConnection -State Listen -LocalPort $portValue "
        "-ErrorAction Stop | Where-Object { $_.LocalAddress -eq '127.0.0.1' } "
        "| Select-Object -ExpandProperty OwningProcess -Unique); "
        "$proc=Get-CimInstance Win32_Process -Filter ('ProcessId = ' + $pidValue); "
        "if ($null -eq $proc) { throw 'process-missing' }; "
        "$started=$proc.CreationDate.ToUniversalTime().ToString('o'); "
        "$json=[ordered]@{listeners=$listeners; pid=[int]$proc.ProcessId; "
        "path=$proc.ExecutablePath; startedAtUtc=$started} | ConvertTo-Json -Compress; "
        "[Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($json))"
    )
    completed = subprocess.run(
        [powershell, "-NoProfile", "-NonInteractive", "-Command", command],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        timeout=15,
        check=False,
    )
    if completed.returncode != 0:
        raise AcceptanceFailure("无法读取隔离 endpoint TCP owner 或进程身份")
    try:
        payload = base64.b64decode(completed.stdout.strip(), validate=True).decode("utf-8")
        result = json.loads(payload)
    except (ValueError, UnicodeDecodeError, json.JSONDecodeError):
        raise AcceptanceFailure("Windows 进程身份查询返回格式无效") from None
    listeners = result.get("listeners")
    if not isinstance(listeners, list):
        listeners = [] if listeners is None else [listeners]
    started_at_utc = result.get("startedAtUtc")
    if isinstance(started_at_utc, str):
        started_at_utc = parse_iso_utc(started_at_utc).isoformat(timespec="microseconds")
    return {
        "listeners": sorted({int(value) for value in listeners}),
        "pid": int(result["pid"]),
        "executablePath": result.get("path"),
        "startedAtUtc": started_at_utc,
    }


def linux_process_identity(pid: int, port: int) -> dict[str, Any]:
    """从 procfs 将 loopback 监听 inode 映射到唯一 PID，并读出 exe 与启动 tick。"""

    proc_root = Path("/proc")
    stat_path = proc_root / str(pid) / "stat"
    try:
        stat_text = stat_path.read_text(encoding="ascii")
        right_paren = stat_text.rfind(")")
        fields = stat_text[right_paren + 2 :].split()
        start_ticks = int(fields[19])
        exe_path = (proc_root / str(pid) / "exe").resolve(strict=True)
        boot_time = next(
            int(line.split()[1])
            for line in (proc_root / "stat").read_text(encoding="ascii").splitlines()
            if line.startswith("btime ")
        )
        ticks_per_second = os.sysconf("SC_CLK_TCK")
        started = dt.datetime.fromtimestamp(
            boot_time + start_ticks / ticks_per_second, tz=dt.timezone.utc
        )
    except (OSError, StopIteration, ValueError, IndexError):
        raise AcceptanceFailure("无法读取 Linux endpoint 进程身份") from None

    try:
        tcp_rows = (proc_root / "net" / "tcp").read_text(encoding="ascii").splitlines()[1:]
    except OSError:
        raise AcceptanceFailure("无法读取 Linux IPv4 TCP listener 表") from None
    inodes: set[str] = set()
    port_hex = f"{port:04X}"
    for row in tcp_rows:
        columns = row.split()
        if len(columns) < 10:
            continue
        local_address, state, inode = columns[1], columns[3], columns[9]
        if local_address.upper() == f"0100007F:{port_hex}" and state == "0A":
            inodes.add(inode)
    owners: set[int] = set()
    for process_dir in proc_root.iterdir():
        if not process_dir.name.isdecimal():
            continue
        fd_dir = process_dir / "fd"
        try:
            for descriptor in fd_dir.iterdir():
                try:
                    link = os.readlink(descriptor)
                except OSError:
                    continue
                match = re.fullmatch(r"socket:\[(\d+)\]", link)
                if match and match.group(1) in inodes:
                    owners.add(int(process_dir.name))
                    break
        except OSError:
            continue
    return {
        "listeners": sorted(owners),
        "pid": pid,
        "startTicks": start_ticks,
        "executablePath": str(exe_path),
        "startedAtUtc": started.isoformat(timespec="milliseconds"),
    }


def verify_instance(record: InstanceRecord, hash_binary: bool = True) -> dict[str, Any]:
    """核对 live TCP owner、PID 代次和可执行路径，并保留 UTC 差值供诊断。

    Linux 的 `/proc` startTicks 是 PID 生存代次的精确身份；boot-time 推导的 UTC
    会受系统校时影响，因此 ticks 一致时仅跳过 UTC 拒绝阈值，仍把差值写入报告。
    Windows 或记录缺少 ticks 时继续使用原五秒时间保护。

    @author kongweiguang
    """

    live = process_identity(record.pid, record.port)
    if live["listeners"] != [record.pid]:
        raise AcceptanceFailure(
            f"endpoint listener owner 与预期 PID 不一致：observed={live['listeners']}"
        )
    if live["pid"] != record.pid:
        raise AcceptanceFailure("实例 PID 与 live process PID 不一致")
    if record.start_ticks is not None and live.get("startTicks") != record.start_ticks:
        raise AcceptanceFailure("live process 启动 tick 与隔离启动记录不一致")
    try:
        live_path = Path(str(live["executablePath"])).resolve(strict=True)
    except OSError:
        raise AcceptanceFailure("live process 可执行路径不可读") from None
    if os.path.normcase(str(live_path)) != os.path.normcase(str(record.executable_path)):
        raise AcceptanceFailure("live process 可执行路径与隔离启动记录不一致")
    live_started = parse_iso_utc(str(live["startedAtUtc"]))
    start_delta = abs((live_started - record.started_at_utc.astimezone(dt.timezone.utc)).total_seconds())
    linux_ticks_match = (
        sys.platform.startswith("linux")
        and record.start_ticks is not None
        and live.get("startTicks") == record.start_ticks
    )
    if start_delta > 5 and not linux_ticks_match:
        raise AcceptanceFailure("live process 启动时间与隔离启动记录相差超过 5 秒")
    return {
        "pid": record.pid,
        "listeners": live["listeners"],
        "executablePath": str(live_path),
        "executableSha256": sha256_file(live_path) if hash_binary else None,
        "startedAtUtc": live_started.isoformat(timespec="milliseconds"),
        "recordStartedAtUtc": record.started_at_utc.astimezone(dt.timezone.utc).isoformat(
            timespec="milliseconds"
        ),
        "startTicks": live.get("startTicks"),
        "startTimeDeltaSeconds": start_delta,
    }


