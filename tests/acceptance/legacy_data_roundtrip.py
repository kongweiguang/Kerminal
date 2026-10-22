#!/usr/bin/env python3
"""为 v0.3.45 → GPUI → v0.3.45 的真实数据往返提供私有 WSL 验收流程。

脚本不会自动编排阶段或远程控制 Kerminal。只有显式执行 `launch` 子命令时，它才会
拉起调用者指定的程序，并在当前终端等待该程序退出；其余阶段只连接操作者手动开启的
127.0.0.1 Streamable HTTP MCP endpoint。旧版 MCP 必须由设置页启动/停止，窗口关闭会
隐藏到托盘，正常退出需选择托盘 Quit。`prepare` 只在隔离目录生成一次性 SSH host key；
各阶段用本机 SSH `true` 命令验证 vault 解密；只接受认证失败，若命令执行成功则验收失败。
探针仅连接私有 loopback sshd，不依赖公网或真实账号。

静态 vault 契约测试仍由 crates/infrastructure/tests/vault.rs 负责；本流程不复制
tests/fixtures/vault，而由 v0.3.45 实际创建文件，再由 GPUI 与旧版进程交替读取。

推荐顺序（每次只运行一个 Kerminal 实例；launch 所在终端会一直占用，阶段命令从另一终端运行）：
  1. prepare --root /tmp/kerminal-roundtrip-<id>
  2. launch --root <root> -- <旧版 AppRun>；人工在设置页启动 MCP，记录 UI 显示的 endpoint
  3. 在另一终端运行 old-write --root <root> --endpoint <旧版 UI endpoint>
  4. 人工在旧版设置页停止 MCP，再通过托盘 Quit 正常退出并等待 launch 返回
  5. launch --root <root> -- <新 GPUI Linux binary>；在新版 MCP 运行时面板手动点击 Start，记录实际 endpoint
  6. 在另一终端运行 new-roundtrip --root <root> --endpoint <新版实际 endpoint>
  7. 通过新版正常退出流程退出并等待 launch 返回；再次用旧版 AppRun 启动并人工开启 MCP
  8. 运行 old-return --root <root> --endpoint <旧版 UI endpoint>，随后按旧版正常流程退出
  9. 保留私有 root 供证据核对或手动删除

新版 MCP `terminal.write` 会在完整 writer ACK 且 shell integration admission 证明 prompt 已就绪等条件满足时，
把单行输入写入命令历史。`terminal.create` 没有提供确定性的 prompt-ready 握手；本脚本在创建后立即写入，
所以历史读回可能因 admission 尚未成立而失败。保留 `history.search` 强断言；该真实往返尚未实测，
超时应使当前阶段失败，不以数据库文件存在替代历史读写验证。

旧版来源：src-tauri/src/paths.rs 的 KERMINAL_CONFIG_ROOT；
src-tauri/src/services/mcp_streamable_http_server.rs 的 loopback /mcp endpoint；
src-tauri/src/app_tray.rs 与 src-tauri/src/lib.rs 的退出/托盘生命周期。
新版来源：crates/kerminal-desktop/src/config_root.rs、main.rs 与 composition.rs。
旧版必须使用官方 v0.3.45 Linux AppImage（基线 commit
`17c5d4be46172ca508f92546bfae5fe5ef477f11`，SHA-256
`0185f18d8361386580d0c1c9b1b1d7f4d469f94f701ff340ab258d5caac72d09`）；当前准备目录保留
AppImage 原件与 `squashfs-root/AppRun`，由操作者校验 artifact.json 后选择 AppRun 启动。

@author kongweiguang
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import secrets
import shlex
import signal
import shutil
import socket
import subprocess
import sys
import time
import uuid
from contextlib import contextmanager
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterator


REQUIRED_TOOLS = {
    "kerminal.config.validate",
    "kerminal.host.upsert_with_credential",
    "history.search",
    "terminal.create",
    "terminal.write",
    "terminal.close",
    "ssh.command",
}
PHASES = ("old-write", "new-roundtrip", "old-return")
TEST_USERNAME = "kerminal-roundtrip-no-such-user"
APP_ROOT_NAME = "kerminal"
STATE_FILE_NAME = "roundtrip-state.json"
SSH_DIRECTORY_NAME = "ssh-test"


from legacy_data_roundtrip_mcp import (
    AcceptanceError,
    McpClient,
    find_string_field,
    require_tool_success,
    tool_result_failed,
)


def private_root(raw_root: str) -> Path:
    """仅允许 `/tmp/kerminal-roundtrip-*` 作为数据根，避免误碰正式用户目录。"""
    root = Path(raw_root)
    if not root.is_absolute():
        raise AcceptanceError("验收 root 必须是绝对路径")
    if not root.name.startswith("kerminal-roundtrip-"):
        raise AcceptanceError("验收 root 目录名必须以 kerminal-roundtrip- 开头")
    if root.parent.resolve() != Path("/tmp"):
        raise AcceptanceError("验收 root 必须是 /tmp 的直接子目录")
    if root.exists() and root.is_symlink():
        raise AcceptanceError("验收 root 不能是符号链接")
    return root


def state_path(root: Path) -> Path:
    """将非敏感验收进度放在配置根目录旁，避免其进入应用数据契约。"""
    return root / STATE_FILE_NAME


def app_root(root: Path) -> Path:
    """返回两版应用共同使用的、由 `KERMINAL_CONFIG_ROOT` 显式指向的目录。"""
    return root / APP_ROOT_NAME


def write_private_file(path: Path, content: bytes) -> None:
    """用 owner-only 权限创建测试输入，避免验收材料被其他本机用户读取。"""
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, "wb") as stream:
        stream.write(content)
        stream.flush()
        os.fsync(stream.fileno())
    os.chmod(path, 0o600)


def save_state(root: Path, state: dict[str, Any]) -> None:
    """原子替换只含主机 id 与命令标记的状态文件，不保存测试密码或私钥。"""
    destination = state_path(root)
    staged = destination.with_suffix(".tmp")
    encoded = json.dumps(state, ensure_ascii=False, indent=2).encode("utf-8") + b"\n"
    write_private_file(staged, encoded)
    os.replace(staged, destination)


def load_state(root: Path) -> dict[str, Any]:
    """验证验收状态确实属于当前私有 root，拒绝空配置或串错运行目录。"""
    try:
        state = json.loads(state_path(root).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        raise AcceptanceError("找不到有效的私有验收状态；先运行 prepare") from None
    if not isinstance(state, dict) or state.get("schemaVersion") != 1:
        raise AcceptanceError("验收状态版本无效")
    if state.get("root") != str(root):
        raise AcceptanceError("验收状态与当前 root 不匹配")
    return state


def free_loopback_port() -> int:
    """选择临时 SSH 端口并立即释放探测 socket，server 启动时再验证端口可用。"""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def create_test_ssh_material(root: Path) -> dict[str, Any]:
    """生成仅供本次验收的 SSH host key、loopback 配置和 OpenSSH 信任行。"""
    ssh_directory = root / SSH_DIRECTORY_NAME
    ssh_directory.mkdir(mode=0o700)
    key_path = ssh_directory / "ssh_host_ed25519_key"
    ssh_keygen = shutil.which("ssh-keygen")
    sshd = shutil.which("sshd") or "/usr/sbin/sshd"
    if ssh_keygen is None or not Path(sshd).exists():
        raise AcceptanceError("WSL 需要已安装的 ssh-keygen 与 sshd；脚本不会自动安装")
    generated = subprocess.run(
        [ssh_keygen, "-q", "-t", "ed25519", "-N", "", "-f", str(key_path)],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    if generated.returncode != 0:
        raise AcceptanceError("无法生成私有验收 SSH host key")
    port = free_loopback_port()
    server_config = ssh_directory / "sshd_config"
    config_lines = [
        "AddressFamily inet",
        "ListenAddress 127.0.0.1",
        f"Port {port}",
        f"HostKey {key_path}",
        f"PidFile {ssh_directory / 'sshd.pid'}",
        "UsePAM no",
        "PasswordAuthentication yes",
        "KbdInteractiveAuthentication no",
        "PubkeyAuthentication no",
        "PermitEmptyPasswords no",
        "StrictModes no",
        "PermitRootLogin no",
        "AllowTcpForwarding no",
        "X11Forwarding no",
        "PermitTunnel no",
        "PrintMotd no",
        "PrintLastLog no",
        "UseDNS no",
        "LogLevel ERROR",
    ]
    write_private_file(server_config, ("\n".join(config_lines) + "\n").encode("utf-8"))
    checked = subprocess.run(
        [sshd, "-t", "-f", str(server_config)],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    if checked.returncode != 0:
        raise AcceptanceError("本机 sshd 不接受隔离测试配置；未启动服务")
    public_key = (key_path.with_suffix(key_path.suffix + ".pub")).read_text(encoding="utf-8").split()
    if len(public_key) < 2:
        raise AcceptanceError("ssh-keygen 未返回有效的临时 host public key")
    host_pattern = f"[127.0.0.1]:{port}"
    trust_line = f"{host_pattern} {public_key[0]} {public_key[1]} kerminal-acceptance\n"
    write_private_file(app_root(root) / "known_hosts", trust_line.encode("utf-8"))
    return {"port": port, "sshd": sshd, "config": str(server_config), "log": str(ssh_directory / "sshd.log")}


def ensure_test_username_is_absent() -> None:
    """拒绝复用本机现存账户，确保 SSH 凭据探针不会接触真实用户。"""
    getent = shutil.which("getent")
    if getent is None:
        raise AcceptanceError("需要 getent 确认合成 SSH 用户不存在；脚本不会跳过此检查")
    lookup = subprocess.run(
        [getent, "passwd", TEST_USERNAME],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    if lookup.returncode == 0:
        raise AcceptanceError("合成 SSH 用户名已存在于本机账户库；请更新测试常量后重建私有 root")
    if lookup.returncode != 2:
        raise AcceptanceError("无法确认合成 SSH 用户不存在；停止凭据探针")


def configure_launch_environment(root: Path) -> dict[str, str]:
    """为子进程隔离 HOME、XDG 数据与临时文件，同时继承 WSLg display/runtime socket。"""
    home = root / "home"
    temp = root / "tmp"
    for directory in (
        home,
        home / ".config",
        home / ".local" / "share",
        home / ".cache",
        home / ".local" / "state",
        temp,
        app_root(root),
    ):
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        os.chmod(directory, 0o700)
    environment = os.environ.copy()
    environment.update(
        {
            "HOME": str(home),
            "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_DATA_HOME": str(home / ".local" / "share"),
            "XDG_CACHE_HOME": str(home / ".cache"),
            "XDG_STATE_HOME": str(home / ".local" / "state"),
            "TMPDIR": str(temp),
            "KERMINAL_CONFIG_ROOT": str(app_root(root)),
        }
    )
    return environment


def prepare(root: Path) -> None:
    """建立全新私有 root 并提示两版 MCP 的显式启动入口，避免假定 listener 已自动运行。"""
    ensure_test_username_is_absent()
    if root.exists():
        raise AcceptanceError("prepare 需要一个尚不存在的私有 root")
    root.mkdir(mode=0o700)
    os.chmod(root, 0o700)
    environment = configure_launch_environment(root)
    ssh = create_test_ssh_material(root)
    state = {
        "schemaVersion": 1,
        "root": str(root),
        "createdAt": datetime.now(timezone.utc).isoformat(),
        "configRoot": environment["KERMINAL_CONFIG_ROOT"],
        "sshPort": ssh["port"],
        "hostId": None,
        "markers": {},
        "completedPhases": [],
        "sshServer": {"binary": ssh["sshd"], "config": ssh["config"], "log": ssh["log"]},
    }
    save_state(root, state)
    print(f"私有验收目录已准备：{root}")
    print(f"应用数据根：{app_root(root)}")
    print("仅设置了 loopback SSH host key；旧版/新版应用均尚未启动。")
    print("启动应用时必须执行：python3 tests/acceptance/legacy_data_roundtrip.py launch --root")
    print(f"  {shlex.quote(str(root))} -- <应用程序及参数>")
    print("旧版需在设置页手动启动 MCP；新版默认停止，需在 MCP 运行时面板点击 Start。")
    print("两版都使用 UI 显示的实际 endpoint；不要从新版启动日志推断 listener 已运行。")


def launch(root: Path, command: list[str]) -> None:
    """通过独立 D-Bus session 启动指定 Linux app，并只传入私有数据环境。"""
    if not command:
        raise AcceptanceError("launch 缺少应用程序路径")
    load_state(root)
    environment = configure_launch_environment(root)
    dbus = shutil.which("dbus-run-session")
    if dbus is None:
        raise AcceptanceError("WSL 缺少 dbus-run-session；脚本不会自动安装")
    print(f"显式启动指定程序：{command[0]}")
    print("本命令会占用当前终端直到应用退出；请在另一终端运行本阶段 MCP 子命令。")
    print("正常退出应用并等待本命令返回后，再启动下一个版本，避免单实例或文件锁交叠。")
    os.execvpe(dbus, [dbus, "--", *command], environment)


@contextmanager
def running_test_sshd(root: Path, state: dict[str, Any]) -> Iterator[None]:
    """仅在 loopback 临时端口启动测试 sshd，并在阶段结束时等待退出。"""
    server = state["sshServer"]
    log_path = Path(server["log"])
    with log_path.open("ab") as server_log:
        sshd_process = subprocess.Popen(
            [server["binary"], "-D", "-e", "-f", server["config"]],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=server_log,
            start_new_session=True,
        )
    try:
        deadline = time.monotonic() + 8.0
        while time.monotonic() < deadline:
            if sshd_process.poll() is not None:
                raise AcceptanceError("临时 SSH server 无法启动；日志留在私有验收目录")
            try:
                with socket.create_connection(("127.0.0.1", int(state["sshPort"])), timeout=0.2):
                    break
            except OSError:
                time.sleep(0.1)
        else:
            raise AcceptanceError("临时 SSH server 未在 8 秒内监听 loopback 端口")
        yield
    finally:
        if sshd_process.poll() is None:
            try:
                os.killpg(sshd_process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                sshd_process.wait(timeout=5.0)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(sshd_process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                sshd_process.wait(timeout=5.0)


def assert_required_tools(client: McpClient) -> None:
    """以 live `tools/list` 锁定每个阶段所需的旧新共有运行态工具。"""
    available = client.list_tools()
    missing = sorted(REQUIRED_TOOLS - available)
    if missing:
        raise AcceptanceError(f"当前进程缺少本验收所需工具：{', '.join(missing)}")


def upsert_test_host(client: McpClient, state: dict[str, Any]) -> str:
    """用每阶段新生成的密码走正式 host use case，秘密只存在于本次请求内。"""
    password = secrets.token_urlsafe(24)
    vault_file = app_root(Path(state["root"])) / "secrets" / "vault.toml"
    previous_vault_digest = file_digest(vault_file) if vault_file.is_file() else None
    arguments: dict[str, Any] = {
        "name": "Kerminal isolated roundtrip probe",
        "host": "127.0.0.1",
        "port": int(state["sshPort"]),
        "username": TEST_USERNAME,
        "password": password,
    }
    if isinstance(state.get("hostId"), str):
        arguments["id"] = state["hostId"]
    result = client.call_tool("kerminal.host.upsert_with_credential", arguments)
    require_tool_success("kerminal.host.upsert_with_credential", result)
    host_id = find_string_field(result, "hostId")
    if not host_id:
        raise AcceptanceError("host upsert 成功回执中没有 hostId")
    secret_ref = find_string_field(result, "secretRef")
    if not secret_ref:
        raise AcceptanceError("host upsert 成功回执中没有 secretRef")
    host_files = list((app_root(Path(state["root"])) / "hosts").glob("*.toml"))
    if not host_files:
        raise AcceptanceError("host upsert 没有在当前 KERMINAL_CONFIG_ROOT 生成 TOML")
    host_contents = [path.read_bytes() for path in host_files]
    if any(password.encode("utf-8") in content for content in host_contents):
        raise AcceptanceError("普通 host TOML 中出现了本次合成密码")
    if not any(secret_ref.encode("utf-8") in content for content in host_contents):
        raise AcceptanceError("host TOML 中未找到 vault secret_ref")
    current_vault_digest = file_digest(vault_file) if vault_file.is_file() else None
    if current_vault_digest is None or current_vault_digest == previous_vault_digest:
        raise AcceptanceError("host upsert 未更新 encrypted vault 文件")
    state["hostId"] = host_id
    return host_id


def validate_hosts(client: McpClient) -> None:
    """由应用自己的只读 config validator 读取当前 hosts TOML。"""
    result = client.call_tool("kerminal.config.validate", {"scope": "hosts"})
    require_tool_success("kerminal.config.validate", result)


def record_history_marker(client: McpClient, phase: str) -> str:
    """用后台 local PTY 写入无敏感标记，验证 SQLite 历史的真实工具读写路径。"""
    marker = f"KERMROUNDTRIP_{phase.replace('-', '_').upper()}_{uuid.uuid4().hex[:16]}"
    created = client.call_tool("terminal.create", {"target": "local", "cols": 80, "rows": 24})
    require_tool_success("terminal.create", created)
    session_id = find_string_field(created, "sessionId")
    if not session_id:
        raise AcceptanceError("terminal.create 成功回执中没有 sessionId")
    data = f"printf '%s\\n' {marker}\n"
    try:
        write_result = client.call_tool("terminal.write", {"sessionId": session_id, "data": data})
        require_tool_success("terminal.write", write_result)
        deadline = time.monotonic() + 10.0
        while time.monotonic() < deadline:
            search = client.call_tool("history.search", {"query": marker, "limit": 10})
            require_tool_success("history.search", search)
            if marker in json.dumps(search, ensure_ascii=False):
                return marker
            time.sleep(0.2)
        raise AcceptanceError("terminal.write 返回后，history.search 未读到本阶段标记")
    finally:
        close_result = client.call_tool("terminal.close", {"sessionId": session_id})
        require_tool_success("terminal.close", close_result)


def verify_history_markers(client: McpClient, markers: dict[str, str]) -> None:
    """逐条查询由前一应用版本写入的 SQLite 标记，而非直接检查文件存在。"""
    for phase, marker in markers.items():
        result = client.call_tool("history.search", {"query": marker, "limit": 10})
        require_tool_success("history.search", result)
        if marker not in json.dumps(result, ensure_ascii=False):
            raise AcceptanceError(f"当前版本未从命令历史读到 {phase} 阶段标记")


def verify_vault_decryption(client: McpClient, host_id: str) -> None:
    """要求本机 SSH 连接越过 vault 与 host-key 校验并明确落到认证拒绝阶段。"""
    ensure_test_username_is_absent()
    result = client.call_tool("ssh.command", {"hostId": host_id, "command": "true"})
    if not tool_result_failed(result):
        raise AcceptanceError("测试 SSH 用户意外认证成功；拒绝执行后续命令")
    response = json.dumps(result, ensure_ascii=False).lower()
    if any(token in response for token in ("vault", "decrypt", "secret_ref", "secret ref", "密钥库", "凭据缺失")):
        raise AcceptanceError("SSH 调用在 vault 读取/解密阶段失败，不能作为兼容证据")
    if any(token in response for token in ("known_hosts", "known hosts", "host key", "fingerprint", "主机密钥", "指纹")):
        raise AcceptanceError("SSH 调用未通过已预置的 host-key 校验，不能作为解密证据")
    if not any(token in response for token in ("auth", "permission denied", "invalid user", "拒绝", "认证")):
        raise AcceptanceError("SSH 失败没有明确归类为认证阶段；不能据此认定 vault 已解密")


def verify_app_outputs(root: Path) -> None:
    """确认三类持久化产物都落在私有 root，不解密或解析其内容。"""
    expected = (
        app_root(root) / "secrets" / "vault-key.toml",
        app_root(root) / "secrets" / "vault.toml",
        app_root(root) / "data" / "command.sqlite",
    )
    missing = [path.name for path in expected if not path.is_file()]
    if missing:
        raise AcceptanceError(f"应用尚未生成预期持久化文件：{', '.join(missing)}")


def file_digest(path: Path) -> bytes:
    """只计算密文文件摘要以验证写入变化，不把凭据或密文写入状态与诊断。"""
    return hashlib.sha256(path.read_bytes()).digest()


def run_phase(root: Path, endpoint: str, phase: str) -> None:
    """串行执行某个版本阶段，状态先读后写并保留前序版本生成的数据。"""
    if phase not in PHASES:
        raise AcceptanceError("未知验收阶段")
    state = load_state(root)
    completed = state.get("completedPhases", [])
    expected_prefix = {
        "old-write": [],
        "new-roundtrip": ["old-write"],
        "old-return": ["old-write", "new-roundtrip"],
    }[phase]
    if completed != expected_prefix:
        raise AcceptanceError(f"阶段顺序错误：{phase} 需要先完成 {expected_prefix}")
    client = McpClient(endpoint)
    client.initialize()
    assert_required_tools(client)
    markers: dict[str, str] = state.setdefault("markers", {})
    if phase != "old-write":
        verify_history_markers(client, markers)
        validate_hosts(client)
        with running_test_sshd(root, state):
            verify_vault_decryption(client, str(state["hostId"]))
            upsert_test_host(client, state)
            save_state(root, state)
            validate_hosts(client)
            verify_vault_decryption(client, str(state["hostId"]))
            markers[phase] = record_history_marker(client, phase)
            save_state(root, state)
    else:
        with running_test_sshd(root, state):
            host_id = upsert_test_host(client, state)
            save_state(root, state)
            validate_hosts(client)
            verify_vault_decryption(client, host_id)
            markers[phase] = record_history_marker(client, phase)
            save_state(root, state)
    verify_app_outputs(root)
    state["completedPhases"] = [*completed, phase]
    state["lastCompletedAt"] = datetime.now(timezone.utc).isoformat()
    save_state(root, state)
    print(f"{phase} 完成：配置校验、命令历史往返、vault 解密探针均通过。")
    print(f"数据仍位于私有目录：{app_root(root)}")


def parse_arguments() -> argparse.Namespace:
    """声明单 root、显式 endpoint 与应用启动子命令，避免隐式选中用户数据。"""
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="action", required=True)
    prepare_parser = subparsers.add_parser("prepare", help="创建一次性私有 root 和本机 SSH host key")
    prepare_parser.add_argument("--root", help="/tmp/kerminal-roundtrip-*；省略时自动生成")
    launch_parser = subparsers.add_parser("launch", help="用隔离 HOME/XDG 与独立 D-Bus session 启动指定应用")
    launch_parser.add_argument("--root", required=True)
    launch_parser.add_argument("command", nargs=argparse.REMAINDER, help="-- 后的 Linux app 路径和参数")
    for phase in PHASES:
        phase_parser = subparsers.add_parser(phase, help=f"执行 {phase} MCP 往返阶段")
        phase_parser.add_argument("--root", required=True)
        phase_parser.add_argument("--endpoint", required=True, help="本机 endpoint，例如 http://127.0.0.1:37657/mcp")
    return parser.parse_args()


def main() -> int:
    """运行一个显式子命令并把失败压缩为安全的静态诊断。"""
    arguments = parse_arguments()
    try:
        if arguments.action == "prepare":
            if arguments.root:
                root = private_root(arguments.root)
            else:
                root = Path(tempfile_mkdtemp())
            prepare(root)
        elif arguments.action == "launch":
            launch(private_root(arguments.root), [item for item in arguments.command if item != "--"])
        else:
            run_phase(private_root(arguments.root), arguments.endpoint, arguments.action)
    except AcceptanceError as error:
        print(f"验收未通过：{error}", file=sys.stderr)
        return 1
    except OSError as error:
        print(f"验收未通过：本地文件或进程操作失败（{type(error).__name__}）", file=sys.stderr)
        return 1
    return 0


def tempfile_mkdtemp() -> str:
    """在固定私有 parent 下生成不复用的目录名，目录本身由 `prepare` 以 0700 创建。"""
    root = Path("/tmp") / f"kerminal-roundtrip-{uuid.uuid4().hex[:12]}"
    return str(root)


if __name__ == "__main__":
    raise SystemExit(main())
