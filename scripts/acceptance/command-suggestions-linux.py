#!/usr/bin/env python3
"""真实 Linux PTY/SSH 的 Shell 编辑快照验收，不操作当前桌面或正式数据。

@author kongweiguang
"""

from __future__ import annotations

import argparse
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import pwd
import re
import select
import shlex
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time
import urllib.parse


class AcceptanceFailure(RuntimeError):
    """只报告固定场景名，不把 Shell 缓冲正文或临时密钥写入诊断。"""


class PtyProcess:
    """每个测试拥有自己的控制终端和子进程，不借用桌面或用户终端。"""

    def __init__(self, command: list[str], environment: dict[str, str], cwd: Path):
        """fork 后直接 exec，避免用额外命令解释层改变被测 Shell 的交互语义。"""
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(cwd)
            os.execvpe(command[0], command, environment)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        self.buffer = bytearray()
        self.closed = False
        self.wait_status = None

    def send(self, value: bytes) -> None:
        """只在当前 PTY 写入指定字节；短写继续同一后缀而不重放已写入部分。"""
        offset = 0
        while offset < len(value):
            offset += os.write(self.fd, value[offset:])

    def drain(self, duration: float = 0.05) -> bytes:
        """以有限等待读取输出，并回答编辑器必要的终端查询，避免启动等待失真。"""
        deadline = time.monotonic() + duration
        collected = bytearray()
        while time.monotonic() < deadline:
            readable, _, _ = select.select([self.fd], [], [], min(0.05, max(0, deadline - time.monotonic())))
            if not readable:
                continue
            try:
                chunk = os.read(self.fd, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    break
                raise
            if not chunk:
                break
            collected.extend(chunk)
            if b"\x1b[6n" in chunk:
                self.send(b"\x1b[1;1R")
            if b"\x1b[c" in chunk:
                self.send(b"\x1b[?1;2c")
        self.buffer.extend(collected)
        if len(self.buffer) > 1024 * 1024:
            raise AcceptanceFailure("PTY 输出超过验收预算")
        return bytes(collected)

    def wait_for(self, predicate, description: str, timeout: float = 5.0):
        """只在有界时间内等真实协议状态，超时不向终端注入重试命令。"""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            result = predicate(bytes(self.buffer))
            if result is not None:
                return result
            self.drain(0.1)
        raise AcceptanceFailure(f"{description}: timeout")

    def close(self) -> bool:
        """清理仅针对本实例尚未 reap 的子进程组，PID 在回收前不会被系统复用。"""
        if self.closed:
            return self.wait_status is not None
        self.closed = True
        os.close(self.fd)
        for termination in (signal.SIGHUP, signal.SIGTERM, signal.SIGKILL):
            try:
                os.killpg(self.pid, termination)
            except ProcessLookupError:
                pass
            deadline = time.monotonic() + 1.0
            while time.monotonic() < deadline:
                result, status = os.waitpid(self.pid, os.WNOHANG)
                if result == self.pid:
                    self.wait_status = status
                    return True
                time.sleep(0.02)
        raise AcceptanceFailure("PTY 子进程未在清理预算内退出")


class PrivateSshServer:
    """固定 loopback 地址和新密钥，所有配置及 known_hosts 均在本次临时目录。"""

    def __init__(self, root: Path):
        """新配置不读取用户 SSH 配置；内置 SFTP 支持生产空输入路径建议，不依赖额外服务。"""
        if os.geteuid() != 0:
            raise AcceptanceFailure("隔离 sshd 验收要求当前 Linux fixture 用户为 root")
        self.root = root
        root.mkdir(mode=0o700)
        self.client_key = root / "client-key"
        self.server_key = root / "server-key"
        self.process = None
        self.log_stream = None
        for path in (self.client_key, self.server_key):
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(path)], check=True, timeout=10)
        with socket.socket() as reserved:
            reserved.bind(("127.0.0.1", 0))
            self.port = reserved.getsockname()[1]
        self.home = root / "home"
        self.home.mkdir(mode=0o700)
        configuration = root / "sshd_config"
        configuration.write_text("\n".join([
            f"Port {self.port}",
            "ListenAddress 127.0.0.1",
            f"HostKey {self.server_key}",
            f"PidFile {root / 'sshd.pid'}",
            f"AuthorizedKeysFile {self.client_key}.pub",
            "StrictModes yes",
            "PasswordAuthentication no",
            "KbdInteractiveAuthentication no",
            "UsePAM no",
            "PermitRootLogin prohibit-password",
            "AllowUsers root",
            "DisableForwarding yes",
            "PermitTTY yes",
            "Subsystem sftp internal-sftp",
            "LogLevel VERBOSE",
            f"SetEnv HOME={self.home} BASH_ENV=/dev/null ENV=/dev/null HISTFILE=/dev/null",
            "",
        ]), encoding="utf-8")
        subprocess.run(["/usr/sbin/sshd", "-t", "-f", str(configuration)], check=True, timeout=10)
        self.log_stream = (root / "sshd.log").open("wb")
        self.process = subprocess.Popen(["/usr/sbin/sshd", "-D", "-e", "-f", str(configuration)],
                                        stdin=subprocess.DEVNULL, stdout=self.log_stream,
                                        stderr=self.log_stream, start_new_session=True)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise AcceptanceFailure("隔离 sshd 启动失败")
            try:
                with socket.create_connection(("127.0.0.1", self.port), timeout=0.1):
                    return
            except OSError:
                time.sleep(0.05)
        self.close()
        raise AcceptanceFailure("隔离 sshd listener 启动超时")

    def command(self, target: list[str], environment: dict[str, str], cwd: Path) -> list[str]:
        """远端只 exec 固定测试启动命令，引用 argv 后不读取真实 ssh config 或 known_hosts。"""
        assignments = [f"{key}={value}" for key, value in environment.items()]
        remote = f"cd {shlex.quote(str(cwd))} && exec " + shlex.join(["env", "-i", *assignments, *target])
        return ["ssh", "-F", "/dev/null", "-tt", "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes",
                "-o", "StrictHostKeyChecking=accept-new", "-o", f"UserKnownHostsFile={self.root / 'known_hosts'}",
                "-o", "GlobalKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR", "-i", str(self.client_key),
                "-p", str(self.port), "root@127.0.0.1", remote]

    def close(self) -> bool:
        """客户端先关闭后再终止本次 listener；确认端口消失后才报告资源已释放。"""
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=3)
        if self.log_stream is not None:
            self.log_stream.close()
        try:
            with socket.create_connection(("127.0.0.1", self.port), timeout=0.1):
                raise AcceptanceFailure("本次 sshd 退出后私有端口仍有 listener")
        except ConnectionRefusedError:
            return True


def isolated_environment(root: Path) -> dict[str, str]:
    """明确构造测试子进程环境，避免 inherited config、历史和代理变量影响快照样本。"""
    directory = root / "home"
    directory.mkdir(mode=0o700, exist_ok=True)
    return {"PATH": "/usr/local/bin:/usr/bin:/bin", "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8",
            "TERM": "xterm-256color", "HOME": str(directory), "XDG_CONFIG_HOME": str(directory / ".config"),
            "XDG_DATA_HOME": str(directory / ".local/share"), "HISTFILE": "/dev/null", "BASH_ENV": "/dev/null",
            "ENV": "/dev/null", "USER": pwd.getpwuid(os.getuid()).pw_name}


def transport_self_test(root: Path) -> dict:
    """在实现协议测试前独立证明 PTY 与私有 SSH 能启动、往返并按所有权关闭。"""
    environment = isolated_environment(root)
    command = ["/bin/sh", "-c", "printf 'KERMINAL-TRANSPORT-READY'; read fixture_line; printf '%s' \"$fixture_line\""]
    result = {}
    server = None
    try:
        for transport in ("local", "ssh"):
            if transport == "ssh":
                server = PrivateSshServer(root / "ssh")
            target = command if server is None else server.command(command, environment, root)
            process = PtyProcess(target, environment, root)
            try:
                process.wait_for(lambda output: True if b"KERMINAL-TRANSPORT-READY" in output else None,
                                 f"{transport} transport startup")
                process.buffer.clear()
                process.send(b"probe-line\n")
                process.wait_for(lambda output: True if output.count(b"probe-line") >= 2 else None,
                                 f"{transport} transport round trip")
                result[transport] = "passed"
            except AcceptanceFailure as error:
                server_log = (server.root / "sshd.log").read_text() if server is not None else ""
                raise AcceptanceFailure(f"{error}; transport diagnostic={bytes(process.buffer[-1500:])!r}; daemon={server_log[-2000:]}") from None
            finally:
                process.close()
    finally:
        if server is not None:
            result["sshListenerClosed"] = server.close()
    return result


PROBE = b"\x1b[24;8~"
NONCE = "0123456789abcdef0123456789abcdef"
FRAME = re.compile(rb"\x1b\]6977;Kerminal;([^\x07]*?)\x07")


def editing_frames(output: bytes, kind: bytes) -> list[list[str]]:
    """只读取指定私有帧，正文保留在测试内存而不写入最终报告。"""
    result = []
    for match in FRAME.finditer(output):
        fields = match.group(1).split(b";")
        if len(fields) > 2 and fields[0] == b"1" and fields[1] == kind:
            result.append([urllib.parse.unquote(value.decode("ascii")) for value in fields])
    return result


def read_snapshot(process: PtyProcess) -> dict:
    """同时检查回调后的真实重绘边界，不能只验 S 帧而漏掉随后使上下文失效的新 A。"""
    process.buffer.clear()
    process.send(PROBE)
    fields = process.wait_for(lambda data: (editing_frames(data, b"S") or [None])[-1], "editing snapshot")
    if len(fields) != 10 or fields[2] != NONCE:
        raise AcceptanceFailure("快照字段或会话身份不匹配")
    if fields[3] in ("bash", "zsh", "fish"):
        process.wait_for(lambda data: True if b"\x1b]133;B\x07" in data else None,
                         f"{fields[3]} snapshot repaint")
        if any(marker in process.buffer for marker in
               (b"\x1b]133;A\x07", b"\x1b]133;C\x07", b"\x1b]133;D;")):
            raise AcceptanceFailure(f"{fields[3]} 只读快照重绘推进了提示符或命令生命周期")
    unit, position, text = fields[4], int(fields[5]), fields[8]
    if unit == "byte":
        try:
            cursor = len(text.encode("utf-8")[:position].decode("utf-8"))
        except UnicodeDecodeError:
            raise AcceptanceFailure(f"{fields[3]} byte cursor={position}, scalar length={len(text)}, bytes={len(text.encode('utf-8'))}") from None
    elif unit == "utf16":
        cursor = len(text.encode("utf-16-le")[:position * 2].decode("utf-16-le"))
    else:
        cursor = position
    return {"text": text, "cursor": cursor, "cwd": fields[7], "shell": fields[3]}


def read_oversized_refusal(process: PtyProcess, shell: str, version: str) -> None:
    """认证无正文拒绝帧并约束随后重绘仍属同一 prompt，不把回显或长输入加入验收输出。"""
    process.buffer.clear()
    process.send(PROBE)
    fields = process.wait_for(lambda data: (editing_frames(data, b"R") or [None])[-1],
                              "oversized refusal", timeout=1.5)
    if fields != ["1", "R", NONCE, shell, "input_too_long", version]:
        raise AcceptanceFailure(f"{shell}: invalid oversized refusal")
    if editing_frames(bytes(process.buffer), b"S"):
        raise AcceptanceFailure(f"{shell}: oversized input was sent as a snapshot")
    process.wait_for(lambda data: True if b"\x1b]133;B\x07" in data else None,
                     f"{shell} refusal repaint")
    if b"\x1b]133;A\x07" in process.buffer:
        raise AcceptanceFailure(f"{shell}: oversized refusal repaint changed the prompt")


def paste_in_chunks(process: PtyProcess, value: str) -> None:
    """分段喂入真实编辑器并消费回显，避免 PTY 输出堵塞造成测试假超时。"""
    process.send(b"\x1b[200~")
    encoded = value.encode("utf-8")
    for offset in range(0, len(encoded), 512):
        process.send(encoded[offset:offset + 512])
        process.drain(0.005)
    process.send(b"\x1b[201~")
    process.drain(0.1)


def shell_fixture(root: Path, shell: str, conflict: bool = False,
                  bash_keymap: str = "emacs-standard", bash_installation_failure: bool = False,
                  bash_prompt: str | None = None, bash_prompt_setup: str | None = None) -> tuple[list[str], dict[str, str]]:
    """从产品脚本准备隔离启动文件；按场景设置编辑模式与既有提示符状态，不继承用户配置。"""
    scripts = Path(__file__).resolve().parents[2] / "crates/infrastructure/src/shell_integration/scripts"
    source = {"bash": "bash.sh", "zsh": "zsh_hook.sh", "fish": "fish.fish"}[shell]
    content = (scripts / source).read_text(encoding="utf-8").replace("__KERMINAL_EDIT_NONCE__", NONCE)
    environment = isolated_environment(root)
    prefix = ("set -o vi\n" if bash_keymap.startswith("vi-") else "set -o emacs\n") if shell == "bash" else ""
    if bash_prompt is not None:
        prefix += f"PS1={shlex.quote(bash_prompt)}\n"
    if bash_prompt_setup is not None:
        prefix += bash_prompt_setup + "\n"
    if bash_installation_failure:
        prefix += """builtin() {
    if [[ $1 == bind && $2 == -m && $3 == vi-command && $4 == -x ]]; then return 1; fi
    command builtin "$@"
}
"""
    if conflict:
        prefix += {
            "bash": f"bind -m {bash_keymap} '\"\\e[24;8~\": backward-char'\n",
            "zsh": "bindkey -M emacs $'\\e[24;8~' backward-char\n",
            "fish": "bind --user \\e\\[24\\;8\\~ backward-char\n",
        }[shell]
    hook = root / (".zshrc" if shell == "zsh" else source)
    hook.write_text(prefix + content, encoding="utf-8")
    if shell == "bash":
        command = ["/bin/bash", "--noprofile", "--rcfile", str(hook), "-i"]
    elif shell == "zsh":
        environment["ZDOTDIR"] = str(root)
        command = ["/usr/bin/zsh", "-i"]
    else:
        command = ["/usr/bin/fish", "--interactive", "--init-command", f"source {shlex.quote(str(hook))}"]
    return command, environment


def verify_shell(root: Path, shell: str, server: PrivateSshServer | None, conflict: bool = False) -> dict:
    """覆盖编辑恢复及真实提示符/退出码边界，所有命令仅作用于本次临时目录。"""
    root.mkdir(mode=0o700)
    command, environment = shell_fixture(root, shell, conflict)
    target = command if server is None else server.command(command, environment, root)
    process = PtyProcess(target, environment, root)
    checks = []
    try:
        hello = process.wait_for(lambda data: (editing_frames(data, b"H") or [None])[-1], f"{shell} hello")
        if shell == "bash" and b"invalid keymap name" in process.buffer:
            raise AcceptanceFailure("Bash startup rejected an integration keymap")
        if hello[2] != NONCE or hello[3] != shell or hello[4] != ("0" if conflict else "1"):
            raise AcceptanceFailure(f"{shell}: capability negotiation mismatch")
        if conflict:
            return {"shell": shell, "transport": "ssh" if server else "local", "conflictPreserved": True}
        process.wait_for(lambda data: True if b"\x1b]133;B\x07" in data else None, f"{shell} prompt")
        initial = read_snapshot(process)
        if initial["text"] != "" or initial["cwd"] != str(root):
            raise AcceptanceFailure(f"{shell}: initial snapshot mismatch")
        checks.append("empty-prompt")
        value = "echo 中文😀e\u0301"
        process.send(value.encode())
        first = read_snapshot(process)
        if first["text"] != value or first["cursor"] != len(value):
            raise AcceptanceFailure(f"{shell}: Unicode buffer mismatch")
        process.send(b"\x1b[D")
        moved = read_snapshot(process)
        if moved["text"] != value or moved["cursor"] >= first["cursor"]:
            raise AcceptanceFailure(f"{shell}: cursor move mismatch")
        checks.append("unicode-and-cursor")
        process.send(b"\x03")
        process.drain(0.2)
        process.send(value.encode())
        if read_snapshot(process)["cursor"] != len(value):
            raise AcceptanceFailure(f"{shell}: end-of-line admission failed")
        process.send(b" appended")
        appended = read_snapshot(process)
        if appended["text"] != value + " appended":
            raise AcceptanceFailure(f"{shell}: suffix did not remain unexecuted")
        checks.append("suffix-without-execution")
        process.send(b"\x03")
        process.drain(0.2)
        process.buffer.clear()
        process.send(b"printf KERMINAL_HISTORY\r")
        process.wait_for(lambda data: True if b"\x1b]133;D;0" in data and b"\x1b]133;B" in data else None, f"{shell} history command completion")
        process.send(b"\x1b[A")
        process.drain(0.1)
        recalled = read_snapshot(process)
        if recalled["text"] != "printf KERMINAL_HISTORY":
            raise AcceptanceFailure(f"{shell}: native history recovery failed")
        checks.append("native-history")
        process.send(b"\x03")
        process.drain(0.2)
        (root / "kerminal_completion_target").touch()
        process.send(b"cat kerminal_completion_\t")
        process.drain(0.2)
        completed = read_snapshot(process)
        if not completed["text"].startswith("cat kerminal_completion_target"):
            raise AcceptanceFailure(f"{shell}: native Tab recovery failed")
        checks.append("native-tab")
        process.send(b"\x03")
        process.drain(0.2)
        folder = root / "space folder"
        folder.mkdir()
        process.send(("cd " + shlex.quote(str(folder)) + "\r").encode())
        process.drain(0.2)
        if read_snapshot(process)["cwd"] != str(folder):
            raise AcceptanceFailure(f"{shell}: changed cwd was not read back")
        checks.append("live-cwd")
        process.send(b"\x1b[200~echo pasted\x1b[201~")
        if read_snapshot(process)["text"] != "echo pasted":
            raise AcceptanceFailure(f"{shell}: bracketed paste recovery failed")
        checks.append("single-line-paste")
        verify_prompt_lifecycle(process, shell)
        checks.extend(["interrupt-new-prompt", "empty-enter-new-prompt", "failed-command-status",
                       "running-command-interrupt-status", "clear-screen-same-prompt"])
        return {"shell": shell, "version": hello[5], "transport": "ssh" if server else "local", "checks": checks}
    finally:
        process.close()


def verify_prompt_lifecycle(process: PtyProcess, shell: str) -> None:
    """区分编辑取消、执行取消与重绘，避免修复快照后误吞真正新 prompt 或改写退出码。"""
    for label, keys, expected_status in (("edit cancel", b"\x03", None),
                                         ("empty enter", b"\r", None),
                                         ("failed command", b"false\r", 1)):
        process.buffer.clear()
        process.send(keys)
        marker = None if expected_status is None else f"\x1b]133;D;{expected_status}\x07".encode()
        process.wait_for(lambda data: True if b"\x1b]133;A\x07" in data and b"\x1b]133;B\x07" in data
                         and (marker is None or marker in data) else None, f"{shell} {label}")
        process.drain(0.05)
        if read_snapshot(process)["text"] != "":
            raise AcceptanceFailure(f"{shell}: {label} retained an old input")
    process.buffer.clear()
    process.send(b"sleep 10\r")
    process.wait_for(lambda data: True if b"\x1b]133;C\x07" in data else None, f"{shell} running command")
    process.buffer.clear()
    process.send(b"\x03")
    process.wait_for(lambda data: True if b"\x1b]133;D;130\x07" in data
                     and b"\x1b]133;A\x07" in data and b"\x1b]133;B\x07" in data else None,
                     f"{shell} interrupted command status")
    process.drain(0.05)
    process.buffer.clear()
    process.send(b"\x0c")
    process.drain(0.2)
    if b"\x1b]133;A\x07" in process.buffer:
        raise AcceptanceFailure(f"{shell}: clear-screen repaint started a new prompt")
    if read_snapshot(process)["text"] != "":
        raise AcceptanceFailure(f"{shell}: clear-screen changed the buffer")


def verify_bash_keymap(root: Path, keymap: str, server: PrivateSshServer | None,
                      conflict: bool = False, wrapped_prompt: bool = False) -> dict:
    """在真实 emacs/vi 编辑器中验证探测、重绘和新 prompt；冲突分支核对原用户键位仍可查询。"""
    root.mkdir(mode=0o700)
    prompt = r"\[\e]133;A\a\]fixture> \[\e]133;B\a\]" if wrapped_prompt else None
    command, environment = shell_fixture(root, "bash", conflict, keymap, bash_prompt=prompt)
    target = command if server is None else server.command(command, environment, root)
    process = PtyProcess(target, environment, root)
    try:
        hello = process.wait_for(lambda data: (editing_frames(data, b"H") or [None])[-1], "Bash keymap hello")
        if b"invalid keymap name" in process.buffer or hello[4] != ("0" if conflict else "1"):
            raise AcceptanceFailure(f"Bash {keymap}: invalid capability negotiation")
        process.wait_for(lambda data: True if b"\x1b]133;B\x07" in data else None, "Bash keymap prompt")
        if conflict:
            process.buffer.clear()
            process.send((f"bind -m {keymap} -p\r").encode())
            process.wait_for(lambda data: True if b'"\\e[24;8~": backward-char' in data else None,
                             "user binding preserved")
            return {"shell": "bash", "keymap": keymap, "transport": "ssh" if server else "local",
                    "conflictPreserved": True}
        process.send(b"echo KERM")
        if keymap == "vi-command":
            process.send(b"\x1b")
            process.drain(0.15)
        snapshot = read_snapshot(process)
        if snapshot["text"] != "echo KERM":
            raise AcceptanceFailure(f"Bash {keymap}: probe changed the buffer")
        if keymap == "vi-command":
            process.send(b"a")
        process.send(b"X")
        if read_snapshot(process)["text"] != "echo KERMX":
            raise AcceptanceFailure(f"Bash {keymap}: append changed or executed the buffer")
        for action, keys in (("interrupt", b"\x03"), ("empty-enter", b"\r"),
                             ("command", b"printf KERM_BOUNDARY\r")):
            process.buffer.clear()
            process.send(keys)
            process.wait_for(lambda data: True if b"\x1b]133;A\x07" in data and b"\x1b]133;B\x07" in data else None,
                             f"Bash {action} new prompt")
            process.drain(0.05)
            if read_snapshot(process)["text"] != "":
                raise AcceptanceFailure(f"Bash {keymap}: {action} did not clear the real buffer")
        return {"shell": "bash", "keymap": keymap, "transport": "ssh" if server else "local",
                "wrappedPrompt": wrapped_prompt,
                "checks": ["snapshot-repaint-same-prompt", "append-without-execution",
                           "interrupt-new-prompt", "empty-enter-new-prompt", "command-new-prompt"]}
    finally:
        process.close()


def verify_long_input(root: Path, shell: str, server: PrivateSshServer | None) -> dict:
    """验证相同 Shell 在超限响应后仍能读取缩短的正文，不执行测试输入。"""
    root.mkdir(mode=0o700)
    command, environment = shell_fixture(root, shell)
    target = command if server is None else server.command(command, environment, root)
    process = PtyProcess(target, environment, root)
    checks = []
    try:
        hello = process.wait_for(lambda data: (editing_frames(data, b"H") or [None])[-1], f"{shell} hello")
        if hello[2] != NONCE or hello[3] != shell or hello[4] != "1":
            raise AcceptanceFailure(f"{shell}: capability negotiation mismatch")
        process.wait_for(lambda data: True if b"\x1b]133;B\x07" in data else None, f"{shell} prompt")
        cases = [("ascii-4000", "x" * 4000, False),
                 ("unicode-4000", "😀" * 4000, False),
                 ("ascii-4001", "x" * 4001, True),
                 ("unicode-4001", "😀" * 4001, True),
                 ("ascii-8001", "x" * 8001, True)]
        for label, value, rejected in cases:
            process.send(b"\x15")
            paste_in_chunks(process, value)
            if rejected:
                read_oversized_refusal(process, shell, hello[5])
                process.send(b"\x15")
                process.send(b"git sta")
                if read_snapshot(process)["text"] != "git sta":
                    raise AcceptanceFailure(f"{shell}: shorter input did not recover after {label}")
            else:
                snapshot = read_snapshot(process)
                if snapshot["text"] != value or snapshot["cursor"] != len(value):
                    raise AcceptanceFailure(f"{shell}: {label} snapshot mismatch")
            checks.append(label)
        return {"shell": shell, "transport": "ssh" if server else "local", "checks": checks}
    finally:
        process.close()


def verify_bash_installation_failure(root: Path) -> dict:
    """只在隔离 Shell 注入最后一个 map 的安装失败，证明前两项回滚且握手没有假报成功。"""
    root.mkdir(mode=0o700)
    command, environment = shell_fixture(root, "bash", bash_installation_failure=True)
    process = PtyProcess(command, environment, root)
    try:
        hello = process.wait_for(lambda data: (editing_frames(data, b"H") or [None])[-1], "failed install hello")
        if hello[4] != "0":
            raise AcceptanceFailure("partially installed Bash callback was advertised as available")
        process.wait_for(lambda data: True if b"\x1b]133;B\x07" in data else None, "failed install prompt")
        process.buffer.clear()
        command = "for map in emacs-standard vi-insert vi-command; do bind -m \"$map\" -X; done; printf 'ROLLBACK_%s_DONE' CLEAN\r"
        process.send(command.encode())
        process.wait_for(lambda data: True if b"ROLLBACK_CLEAN_DONE" in data else None, "rollback inspection")
        if b"__kerminal_edit_snapshot" in process.buffer:
            raise AcceptanceFailure("failed Bash installation left an active callback")
        return {"shell": "bash", "transport": "local", "partialInstallRolledBack": True}
    finally:
        process.close()


def verify_bash_prompt_declaration(root: Path, scenario: str) -> dict:
    """启动前固定声明和键位，拒绝不兼容状态时验证无副作用；可变数组仍需保留并实际执行。"""
    declarations = {
        "readonly-indexed": "declare -ar PROMPT_COMMAND=('true')",
        "readonly-indexed-exported": "declare -arx PROMPT_COMMAND=('true')",
        "readonly-indexed-conflict": "declare -ar PROMPT_COMMAND=('true')\nbind -m emacs-standard '\"\\e[24;8~\": backward-char'",
        "readonly-scalar": "declare -r PROMPT_COMMAND='true'",
        "associative": "declare -A PROMPT_COMMAND=([user]='true')",
        "associative-exported": "declare -Ax PROMPT_COMMAND=([user]='true')",
        "readonly-ps1": "declare -r PS1='KERMINAL_TEST_PROMPT> '\nPROMPT_COMMAND=true",
        "readonly-ps1-exported": "declare -rx PS1='KERMINAL_TEST_PROMPT> '\nPROMPT_COMMAND=true",
        "mutable-indexed": "__fixture_prompt_count=0\n__fixture_user_prompt() { ((__fixture_prompt_count+=1)); }\ndeclare -ax PROMPT_COMMAND=('__fixture_user_prompt')",
        "mutable-scalar-values": "PROMPT_COMMAND=\": 'declare -r' 'declare -A'\"\nPS1='KERMINAL_TEST_PROMPT> declare -r declare -A '",
    }
    if scenario not in declarations:
        raise AcceptanceFailure(f"unknown Bash prompt declaration scenario: {scenario}")
    root.mkdir(mode=0o700)
    setup = "unset PROMPT_COMMAND\n" + declarations[scenario] + """
__fixture_before_prompt=$(declare -p PROMPT_COMMAND)
__fixture_before_ps1_declaration=$(declare -p PS1)
__fixture_before_ps1=$PS1
__fixture_before_keymaps=$(for __fixture_map in emacs-standard vi-insert vi-command; do
    for __fixture_flag in -p -s -X; do builtin bind -m "$__fixture_map" "$__fixture_flag"; done
done)
"""
    command, environment = shell_fixture(root, "bash", bash_prompt="KERMINAL_TEST_PROMPT> ",
                                         bash_prompt_setup=setup)
    process = PtyProcess(command, environment, root)
    try:
        process.wait_for(lambda data: True if b"KERMINAL_TEST_PROMPT> " in data else None,
                         f"Bash {scenario} initial prompt")
        process.drain(0.1)
        hellos = editing_frames(process.buffer, b"H")
        if scenario in ("mutable-indexed", "mutable-scalar-values"):
            if len(hellos) != 1 or hellos[0][4] != "1":
                raise AcceptanceFailure(f"mutable Bash {scenario} did not enable editing")
            process.wait_for(lambda data: True if b"\x1b]133;B\x07" in data else None, "mutable prompt")
            if read_snapshot(process)["text"] != "":
                raise AcceptanceFailure(f"mutable Bash {scenario} changed the initial buffer")
            if scenario == "mutable-indexed":
                check = '[[ $__fixture_prompt_count -ge 1 && " ${PROMPT_COMMAND[*]} " == *" __fixture_user_prompt "* ]]'
            else:
                check = '[[ $__kerminal_user_prompt_command == *"declare -r"* && $PS1 == *"declare -r"* ]]'
        else:
            if any(hello[4] == "1" for hello in hellos):
                raise AcceptanceFailure(f"Bash {scenario} advertised unavailable integration as ready")
            if b"readonly variable" in process.buffer or b"invalid keymap name" in process.buffer:
                raise AcceptanceFailure(f"Bash {scenario} produced a startup error")
            check = '''[[ $(declare -p PROMPT_COMMAND) == "$__fixture_before_prompt" && $(declare -p PS1) == "$__fixture_before_ps1_declaration" && $PS1 == "$__fixture_before_ps1" && ${__kerminal_shell_integration_active-0} != 1 && -z $(trap -p DEBUG) && $(for __fixture_map in emacs-standard vi-insert vi-command; do for __fixture_flag in -p -s -X; do builtin bind -m "$__fixture_map" "$__fixture_flag"; done; done) == "$__fixture_before_keymaps" ]]'''
        process.buffer.clear()
        process.send((f"if {check}; then printf 'KERM_%s_%s\\n' DECLARATION PASS; else printf 'KERM_%s_%s\\n' DECLARATION FAIL; fi\r").encode())
        passed = b"KERM_DECLARATION_PASS"
        failed = b"KERM_DECLARATION_FAIL"
        process.wait_for(lambda data: True if passed in data or failed in data else None,
                         f"Bash {scenario} declaration preservation")
        if failed in process.buffer:
            raise AcceptanceFailure(f"Bash {scenario} changed the original prompt or key bindings")
        return {"shell": "bash", "transport": "local", "promptDeclaration": scenario,
                "originalStatePreserved": True}
    finally:
        process.close()


def main() -> int:
    """只在 Linux 中启动独立测试资源；报告完成前执行 finally 释放所有拥有的进程。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test-transports", action="store_true")
    parser.add_argument("--shells", action="store_true")
    parser.add_argument("--long-input", action="store_true")
    parser.add_argument("--bash-keymaps", action="store_true")
    parser.add_argument("--local-only", action="store_true")
    parser.add_argument("--output", type=Path)
    arguments = parser.parse_args()
    if not sys.platform.startswith("linux"):
        parser.error("此脚本仅在 Linux/WSL 运行")
    if sum((arguments.self_test_transports, arguments.shells, arguments.long_input, arguments.bash_keymaps)) != 1:
        parser.error("请选择一项 --shells、--long-input、--bash-keymaps 或 --self-test-transports")
    with tempfile.TemporaryDirectory(prefix="kerminal-command-suggestions-", dir="/run") as directory:
        root = Path(directory)
        if arguments.self_test_transports:
            result = transport_self_test(root)
        else:
            server = None
            result = {"cases": []}
            try:
                if not arguments.local_only:
                    server = PrivateSshServer(root / "ssh")
                if arguments.bash_keymaps:
                    for keymap in ("emacs-standard", "vi-insert", "vi-command"):
                        result["cases"].append(verify_bash_keymap(root / f"{keymap}-local", keymap, None))
                        result["cases"].append(verify_bash_keymap(root / f"{keymap}-conflict", keymap, None, True))
                        if server is not None:
                            result["cases"].append(verify_bash_keymap(root / f"{keymap}-ssh", keymap, server))
                    result["cases"].append(verify_bash_installation_failure(root / "bash-install-failure"))
                    for scenario in ("readonly-indexed", "readonly-indexed-exported", "readonly-indexed-conflict",
                                     "readonly-scalar", "associative", "associative-exported", "readonly-ps1",
                                     "readonly-ps1-exported", "mutable-indexed", "mutable-scalar-values"):
                        result["cases"].append(verify_bash_prompt_declaration(root / f"bash-{scenario}", scenario))
                    result["cases"].append(verify_bash_keymap(root / "bash-wrapped-local", "emacs-standard", None,
                                                            wrapped_prompt=True))
                    if server is not None:
                        result["cases"].append(verify_bash_keymap(root / "bash-wrapped-ssh", "emacs-standard", server,
                                                                wrapped_prompt=True))
                shells = () if arguments.bash_keymaps else ("bash", "zsh", "fish")
                for shell in shells:
                    if arguments.long_input:
                        result["cases"].append(verify_long_input(root / f"{shell}-local", shell, None))
                        if server is not None:
                            result["cases"].append(verify_long_input(root / f"{shell}-ssh", shell, server))
                    else:
                        result["cases"].append(verify_shell(root / f"{shell}-local", shell, None))
                        result["cases"].append(verify_shell(root / f"{shell}-conflict", shell, None, True))
                        if server is not None:
                            result["cases"].append(verify_shell(root / f"{shell}-ssh", shell, server))
            finally:
                if server is not None:
                    result["sshListenerClosed"] = server.close()
    report = json.dumps(result, ensure_ascii=False, indent=2)
    if arguments.output is not None:
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        arguments.output.write_text(report + "\n", encoding="utf-8")
    print(report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

