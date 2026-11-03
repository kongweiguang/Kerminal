# @author kongweiguang
"""在私有 HOME 和 PTY 中执行 Windows 生成的生产启动命令，不输出会话正文。"""

import errno
import os
from pathlib import Path
import pty
import select
import signal
import sys
import tempfile
import time


# 等待具体协议边界后才发送输入；失败只报告阶段，避免泄露环境或命令正文。
# @author kongweiguang
def wait_for(fd, output, predicate):
    deadline = time.monotonic() + 15
    while not predicate(output):
        if len(output) > 256 * 1024:
            raise RuntimeError("PTY output exceeded its limit")
        if time.monotonic() >= deadline:
            raise RuntimeError("PTY protocol boundary timed out")
        if select.select([fd], [], [], 0.1)[0]:
            try:
                chunk = os.read(fd, 4096)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                chunk = b""
            if not chunk:
                raise RuntimeError("PTY closed before the expected boundary")
            output.extend(chunk)


# pty.fork 为子进程建立独立会话，finally 只终止本次拥有的进程组。
# @author kongweiguang
def probe(shell, command):
    with tempfile.TemporaryDirectory(prefix="kerminal-startup-") as root:
        home = Path(root, "home")
        private_tmp = Path(root, "tmp")
        home.mkdir()
        private_tmp.mkdir()
        pid, fd = pty.fork()
        if pid == 0:
            os.environ.update(HOME=str(home), ZDOTDIR=str(home),
                              XDG_CONFIG_HOME=str(home), TMPDIR=str(private_tmp),
                              TERM="xterm-256color")
            os.execvp(shell, [shell, "-c", command])
        reaped = False
        output = bytearray()
        try:
            wait_for(fd, output, lambda data: b"\x1b]133;A\x07" in data
                     and b"\x1b]133;B\x07" in data)
            if b"command not found" in output or b"syntax error" in output:
                raise RuntimeError("shell emitted a startup error")
            # 输出拼接的标识，避免将输入回显误认为命令确已执行。
            os.write(fd, b"printf 'KERM_%s_%s\\n' STARTUP OK\r")
            wait_for(fd, output, lambda data: b"KERM_STARTUP_OK\r\n" in data
                     and b"\x1b]133;D;0\x07" in data
                     and data.count(b"\x1b]133;B\x07") >= 2)
            os.write(fd, b"exit 23\r")
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                waited, status = os.waitpid(pid, os.WNOHANG)
                if waited:
                    reaped = True
                    if os.waitstatus_to_exitcode(status) != 23:
                        raise RuntimeError("startup wrapper changed the exit code")
                    break
                # 持续排空 PTY，不能让退出提示阻塞子进程。
                if select.select([fd], [], [], 0.05)[0]:
                    try:
                        os.read(fd, 4096)
                    except OSError as error:
                        if error.errno != errno.EIO:
                            raise
            if not reaped:
                raise RuntimeError("shell exit timed out")
            if any(private_tmp.glob("kerminal.*")):
                raise RuntimeError("startup left its temporary scripts behind")
        finally:
            if not reaped:
                os.killpg(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
            os.close(fd)


probe(sys.argv[1], sys.argv[2])
print("startup, command, exit and cleanup passed")
