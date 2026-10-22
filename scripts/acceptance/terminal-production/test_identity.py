#!/usr/bin/env python3
# @author kongweiguang
"""覆盖 terminal-production launcher 身份与隔离根的窄回归。"""

from __future__ import annotations

import base64
import datetime as dt
import json
import os
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import identity
import run


class RunnerProvenanceTests(unittest.TestCase):
    """用临时 Cargo dep-info 固定精确来源判定，不触碰实际 runner 或构建缓存。"""

    def setUp(self) -> None:
        """建立包含空格的隔离 worktree，覆盖 Windows dep-info 的路径转义。

        @author kongweiguang
        """
        self.temporary_root = tempfile.TemporaryDirectory(prefix="terminal provenance ")
        self.container = Path(self.temporary_root.name)
        self.root = self.container / "workspace with spaces"
        self.runner_dir = self.root / "target" / "release" / "examples"
        self.runner_dir.mkdir(parents=True)
        self.runner = self.runner_dir / "session-acceptance.exe"
        self.runner.write_bytes(b"fixture runner; never executed")
        self.source = self.root / "tests" / "acceptance" / "session_runtime.rs"
        self.source.parent.mkdir(parents=True)
        self.source.write_text("// isolated source fixture\n", encoding="utf-8")
        (self.root / "crates" / "infrastructure").mkdir(parents=True)
        self.dep_info = self.runner.with_suffix(".d")

    def tearDown(self) -> None:
        """释放仅由本测试创建的临时 source、runner 与 dep-info。

        @author kongweiguang
        """
        self.temporary_root.cleanup()

    def _write_dep_info(self, dependency: Path) -> None:
        """按 Cargo/Makefile 形式编码空格，同时保留 Windows 目录反斜杠。

        @author kongweiguang
        """
        target_text = str(self.runner).replace(" ", r"\ ")
        dependency_text = str(dependency).replace(" ", r"\ ")
        self.dep_info.write_text(
            f"{target_text}: {dependency_text}\n", encoding="utf-8"
        )

    def test_accepts_equivalent_parent_segments_and_escaped_spaces(self) -> None:
        """等价 `..` 依赖必须规范化到精确源码，且转义空格与反斜杠均保真。

        @author kongweiguang
        """
        equivalent = (
            self.root
            / "crates"
            / "infrastructure"
            / ".."
            / ".."
            / "tests"
            / "acceptance"
            / "session_runtime.rs"
        )
        self._write_dep_info(equivalent)

        result = run.verify_runner_provenance(self.root, self.runner)

        self.assertEqual(Path(result["workspaceSourcePath"]), self.source.resolve())
        self.assertTrue(result["workspaceSourceReferenced"])

    def test_rejects_same_basename_from_another_worktree_path(self) -> None:
        """相同文件名不能替代当前 worktree 中要求的精确源码路径。

        @author kongweiguang
        """
        other_source = self.root / "other" / "session_runtime.rs"
        other_source.parent.mkdir(parents=True)
        other_source.write_text("// wrong source fixture\n", encoding="utf-8")
        self._write_dep_info(other_source)

        with self.assertRaisesRegex(identity.AcceptanceFailure, "同名 acceptance 源"):
            run.verify_runner_provenance(self.root, self.runner)

    def test_rejects_dependency_that_escapes_worktree(self) -> None:
        """通过多级 `..` 逃到 worktree 外的同名源码必须被严格规范化后拒绝。

        @author kongweiguang
        """
        escaped_source = self.container / "outside" / "session_runtime.rs"
        escaped_source.parent.mkdir(parents=True)
        escaped_source.write_text("// outside source fixture\n", encoding="utf-8")
        escaped_reference = (
            self.root
            / "crates"
            / "infrastructure"
            / ".."
            / ".."
            / ".."
            / "outside"
            / "session_runtime.rs"
        )
        self._write_dep_info(escaped_reference)

        with self.assertRaisesRegex(identity.AcceptanceFailure, "同名 acceptance 源"):
            run.verify_runner_provenance(self.root, self.runner)


class WindowsProcessIdentityTests(unittest.TestCase):
    """固定 PowerShell 与 Windows Python 之间的 Unicode 路径编码契约。"""

    def _psutil_fixture(
        self,
        *,
        path: str,
        create_times: tuple[float, float],
        listener_owners: tuple[int | None, ...],
    ) -> types.SimpleNamespace:
        """用确定的进程与 socket 元数据覆盖平台 API，避免触碰真实进程表。

        @author kongweiguang
        """
        before_process = types.SimpleNamespace(
            create_time=Mock(return_value=create_times[0]),
            exe=Mock(return_value=path),
        )
        after_process = types.SimpleNamespace(
            create_time=Mock(return_value=create_times[1]),
            is_running=Mock(return_value=True),
        )
        connections = [
            types.SimpleNamespace(
                family=socket.AF_INET,
                type=socket.SOCK_STREAM,
                status="LISTEN",
                laddr=("127.0.0.1", 37659),
                pid=owner,
            )
            for owner in listener_owners
        ]
        return types.SimpleNamespace(
            Process=Mock(side_effect=[before_process, after_process]),
            net_connections=Mock(return_value=connections),
            CONN_LISTEN="LISTEN",
            Error=type("PsutilError", (Exception,), {}),
        )

    def test_decodes_unicode_path_from_ascii_envelope(self) -> None:
        """模拟 psutil 不可用且 Python 默认 GBK，确保后备路径准确恢复中文路径。

        @author kongweiguang
        """
        expected_path = r"C:\kong\personal-projects\Kerminal\代码\kerminal-gpui\target\kerminal.exe"
        payload = json.dumps(
            {
                "listeners": [53852],
                "pid": 53852,
                "path": expected_path,
                "startedAtUtc": "2026-10-20T11:45:00.3938000Z",
            },
            ensure_ascii=False,
            separators=(",", ":"),
        ).encode("utf-8")
        stdout = base64.b64encode(payload).decode("ascii") + "\r\n"
        completed = subprocess.CompletedProcess(
            args=["pwsh"], returncode=0, stdout=stdout, stderr=""
        )

        with (
            patch.dict(sys.modules, {"psutil": None}),
            patch.object(identity.shutil, "which", return_value="pwsh"),
            patch.object(identity.subprocess, "run", return_value=completed) as run,
        ):
            result = identity.windows_process_identity(53852, 37659)

        self.assertEqual(result["executablePath"], expected_path)
        self.assertIn("ToBase64String", run.call_args.args[0][-1])

    def test_psutil_fast_path_preserves_unicode_path_and_exact_owner(self) -> None:
        """有完整 owner 与稳定 PID 代次时走快路径，Unicode exe 路径不经 shell 编码。

        @author kongweiguang
        """
        expected_path = r"C:\kong\个人项目\Kerminal\候选\kerminal.exe"
        fake_psutil = self._psutil_fixture(
            path=expected_path,
            create_times=(1792497481.1289563, 1792497481.1289563),
            listener_owners=(115248,),
        )

        with (
            patch.dict(sys.modules, {"psutil": fake_psutil}),
            patch.object(identity.subprocess, "run", side_effect=AssertionError("unexpected fallback")),
        ):
            result = identity.windows_process_identity(115248, 37659)

        self.assertEqual(result["listeners"], [115248])
        self.assertEqual(result["pid"], 115248)
        self.assertEqual(result["executablePath"], expected_path)
        self.assertEqual(result["startedAtUtc"], "2026-10-20T11:58:01.128956+00:00")
        self.assertEqual(fake_psutil.Process.call_count, 2)

    def test_psutil_fast_path_rejects_non_owner_without_fallback(self) -> None:
        """已观测到另一 PID 占用目标 loopback 端口时，不能改用后备结果掩盖冲突。

        @author kongweiguang
        """
        fake_psutil = self._psutil_fixture(
            path=r"C:\kerminal.exe",
            create_times=(1792497481.0, 1792497481.0),
            listener_owners=(115247,),
        )

        with (
            patch.dict(sys.modules, {"psutil": fake_psutil}),
            patch.object(identity.subprocess, "run", side_effect=AssertionError("unexpected fallback")),
        ):
            with self.assertRaisesRegex(identity.AcceptanceFailure, "owner 与预期 PID"):
                identity.windows_process_identity(115248, 37659)

    def test_psutil_fast_path_rejects_pid_reuse_during_identity_read(self) -> None:
        """快照前后 create_time 不同意味着 PID 代次改变，必须立即拒绝。

        @author kongweiguang
        """
        fake_psutil = self._psutil_fixture(
            path=r"C:\kerminal.exe",
            create_times=(1792497481.0, 1792497482.0),
            listener_owners=(115248,),
        )

        with (
            patch.dict(sys.modules, {"psutil": fake_psutil}),
            patch.object(identity.subprocess, "run", side_effect=AssertionError("unexpected fallback")),
        ):
            with self.assertRaisesRegex(identity.AcceptanceFailure, "退出或被复用"):
                identity.windows_process_identity(115248, 37659)


class LinuxRunLayoutTests(unittest.TestCase):
    """用临时 ext4 目录核验 WSL Linux launcher 的隔离边界，不启动候选进程。"""

    def setUp(self) -> None:
        """把虚拟实例放在当前 WSL 用户自己的 cache 下，匹配 launcher 的真实目录布局。

        @author kongweiguang
        """
        if not sys.platform.startswith("linux"):
            self.skipTest("Linux runRoot 只在 WSL/Linux Python 中验证")
        self.cache_root = Path.home() / ".cache"
        if not self.cache_root.is_dir():
            self.skipTest("当前用户没有已存在的 ~/.cache")
        self.run_root = self.cache_root / (
            "kerminal-gpui-terminal-production-20261020-" + secrets.token_hex(4)
        )
        self.run_root.mkdir(mode=0o700)
        self.run_id = self.run_root.name
        self.config_root = self.run_root / "config"
        self.bin_root = self.run_root / "bin"
        self.config_root.mkdir(mode=0o700)
        self.bin_root.mkdir(mode=0o700)
        os.chmod(self.config_root, 0o700)
        os.chmod(self.bin_root, 0o700)
        self.executable = self.bin_root / "kerminal"
        self.executable.write_bytes(b"fixture executable; not launched")
        os.chmod(self.executable, 0o755)

        self.temporary_repo = tempfile.TemporaryDirectory(prefix="terminal-production-identity-")
        self.repo_root = Path(self.temporary_repo.name)
        self.record_dir = (
            self.repo_root
            / "target"
            / "diagnostics"
            / "terminal-production-20261020"
            / "runs"
            / self.run_id
        )
        self.record_dir.mkdir(parents=True)
        self.record_path = self.record_dir / "launch.json"
        self.record = {
            "schemaVersion": 1,
            "owner": "terminal-production-20261020",
            "status": "active",
            "pid": 12345,
            "startTicks": 67890,
            "executablePath": str(self.executable),
            "startedAtUtc": "2026-10-20T00:00:00Z",
            "mcpEndpoint": "http://127.0.0.1:37658/mcp",
            "configRoot": str(self.config_root),
            "runRoot": str(self.run_root),
        }
        self._write_record()
        self.repository_root_patch = patch.object(identity, "repository_root", return_value=self.repo_root)
        self.repository_root_patch.start()

    def tearDown(self) -> None:
        """只清理本测试创建的临时 runRoot 和临时记录目录。"""
        if hasattr(self, "repository_root_patch"):
            self.repository_root_patch.stop()
        if hasattr(self, "temporary_repo"):
            self.temporary_repo.cleanup()
        if hasattr(self, "run_root"):
            shutil.rmtree(self.run_root, ignore_errors=False)

    def _write_record(self) -> None:
        """每次变体都覆盖临时 launch.json，避免测试间复用陈旧身份字段。"""
        self.record_path.write_text(json.dumps(self.record), encoding="utf-8")

    def _verification_inputs(
        self,
        *,
        record_ticks: int | None,
        live_ticks: int | None,
        drift_seconds: int,
    ) -> tuple[identity.InstanceRecord, dict[str, object]]:
        """构造合成 live 身份，隔离验证时钟策略，不依赖真实 PID 或 procfs。"""
        recorded_start = dt.datetime(2026, 10, 20, tzinfo=dt.timezone.utc)
        live_start = recorded_start + dt.timedelta(seconds=drift_seconds)
        record = identity.InstanceRecord(
            pid=12345,
            executable_path=self.executable.resolve(strict=True),
            started_at_utc=recorded_start,
            endpoint="http://127.0.0.1:37658/mcp",
            config_root=self.config_root,
            host="127.0.0.1",
            port=37658,
            start_ticks=record_ticks,
        )
        live = {
            "listeners": [12345],
            "pid": 12345,
            "startTicks": live_ticks,
            "executablePath": str(self.executable.resolve(strict=True)),
            "startedAtUtc": live_start.isoformat(),
        }
        return record, live

    def test_accepts_exact_ext4_run_root_record(self) -> None:
        """只要记录、目录关系及 owner/权限吻合，就允许 WSL ext4 隔离根进入后续 live 校验。"""
        record = identity.read_instance_record(self.record_path.resolve(strict=True))

        self.assertEqual(record.config_root, self.config_root)
        self.assertEqual(record.executable_path, self.executable)
        self.assertEqual(record.port, 37658)
        self.assertEqual(record.start_ticks, 67890)

    def test_rejects_config_outside_recorded_run_root(self) -> None:
        """不能因目录同处 ~/.cache 就接受与 launch.json runRoot 无关的配置。"""
        unrelated_config = self.run_root / "other-config"
        unrelated_config.mkdir(mode=0o700)
        self.record["configRoot"] = str(unrelated_config)
        self._write_record()

        with self.assertRaisesRegex(identity.AcceptanceFailure, "runRoot/config"):
            identity.read_instance_record(self.record_path.resolve(strict=True))

    def test_rejects_arbitrary_cache_directory_name(self) -> None:
        """普通 ~/.cache 子目录不属于本任务 launcher 创建的隔离根。"""
        self.record["runRoot"] = str(self.cache_root / "arbitrary-cache-entry")
        self._write_record()

        with self.assertRaisesRegex(identity.AcceptanceFailure, "唯一隔离目录名"):
            identity.read_instance_record(self.record_path.resolve(strict=True))

    def test_rejects_group_or_other_writable_run_root(self) -> None:
        """runRoot 必须保持 launcher 的 owner-only 权限，避免身份声明被其他用户替换。"""
        os.chmod(self.run_root, 0o777)

        with self.assertRaisesRegex(identity.AcceptanceFailure, "权限过宽"):
            identity.read_instance_record(self.record_path.resolve(strict=True))

    def test_rejects_protected_mcp_port_before_path_admission(self) -> None:
        """默认正式 MCP 端口保护对 Linux 新路径仍先于任何网络身份核验生效。"""
        self.record["mcpEndpoint"] = "http://127.0.0.1:37657/mcp"
        self._write_record()

        with self.assertRaisesRegex(identity.AcceptanceFailure, "正式 MCP 端口 37657"):
            identity.read_instance_record(self.record_path.resolve(strict=True))

    def test_accepts_matching_linux_ticks_when_wall_clock_drifts(self) -> None:
        """精确 PID 代次相同即可跨过 boot-time 校时差，UTC 差值仍完整留在报告中。"""
        record, live = self._verification_inputs(
            record_ticks=67890,
            live_ticks=67890,
            drift_seconds=12,
        )
        with patch.object(identity, "process_identity", return_value=live):
            verified = identity.verify_instance(record, hash_binary=False)

        self.assertEqual(verified["startTicks"], 67890)
        self.assertEqual(verified["startTimeDeltaSeconds"], 12.0)

    def test_rejects_mismatched_linux_ticks_even_when_clock_matches(self) -> None:
        """wall clock 接近不能掩盖 PID 复用或启动代次不一致。"""
        record, live = self._verification_inputs(
            record_ticks=67890,
            live_ticks=67891,
            drift_seconds=1,
        )
        with patch.object(identity, "process_identity", return_value=live):
            with self.assertRaisesRegex(identity.AcceptanceFailure, "启动 tick"):
                identity.verify_instance(record, hash_binary=False)

    def test_rejects_wall_clock_drift_when_record_has_no_ticks(self) -> None:
        """没有可比较的 launcher ticks 时仍由原五秒 UTC 阈值兜底。"""
        record, live = self._verification_inputs(
            record_ticks=None,
            live_ticks=None,
            drift_seconds=12,
        )
        with patch.object(identity, "process_identity", return_value=live):
            with self.assertRaisesRegex(identity.AcceptanceFailure, "超过 5 秒"):
                identity.verify_instance(record, hash_binary=False)


if __name__ == "__main__":
    unittest.main()
