#!/usr/bin/env python3
"""为 Windows 隐藏窗口验收提供一次性 loopback SSH Bash 服务。

@author kongweiguang
"""

from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time


def load_fixture_class():
    """复用协议验收的 sshd 约束和关闭校验，避免另造连接实现。"""
    source = Path(__file__).with_name("command-suggestions-linux.py")
    spec = importlib.util.spec_from_file_location("suggestions_linux_fixture", source)
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load the isolated SSH fixture")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.PrivateSshServer


def write_json(path: Path, payload: dict) -> None:
    """先完整写入本轮临时文件再发布，防止 Windows 进程读取半份就绪凭据。"""
    temporary = path.with_suffix(".tmp")
    with temporary.open("x", encoding="utf-8") as stream:
        json.dump(payload, stream)
    os.replace(temporary, path)


def run(root: Path, timeout: int) -> None:
    """仅写入本轮目录；关闭 listener 后保留不含私钥的认证日志，再释放 Linux 密钥目录。"""
    allowed = (Path(__file__).resolve().parents[2] /
               "target/diagnostics/command-suggestions-ssh-fixtures").resolve(strict=True)
    root = root.resolve(strict=True)
    if root == allowed or allowed not in root.parents or timeout < 30 or timeout > 900:
        raise RuntimeError("fixture directory or timeout is outside the permitted boundary")
    if any(root.iterdir()):
        # The caller's redirected log files may be opened before this process starts.
        if any(item.name not in {"stdout.log", "stderr.log"} for item in root.iterdir()):
            raise RuntimeError("fixture directory is not empty")
    fixture_class = load_fixture_class()
    # StrictModes 会检查 authorized_keys 的父目录；使用 root 拥有的 /run，不放宽认证检查。
    with tempfile.TemporaryDirectory(prefix="kerminal-suggestions-native-ssh-", dir="/run") as temporary:
        server = fixture_class(Path(temporary) / "ssh")
        closed = False
        try:
            shutil.copyfile(server.client_key, root / "client-key")
            public_key = (server.root / "server-key.pub").read_text(encoding="utf-8").split()
            if len(public_key) < 2 or public_key[0] != "ssh-ed25519":
                raise RuntimeError("unexpected fixture host key")
            (root / "known_hosts").write_text(
                f"[127.0.0.1]:{server.port} {public_key[0]} {public_key[1]}\n", encoding="utf-8"
            )
            write_json(root / "ready.json", {"port": server.port, "username": "root"})
            deadline = time.monotonic() + timeout
            while not (root / "stop.fixture").exists() and time.monotonic() < deadline:
                if server.process.poll() is not None:
                    raise RuntimeError("fixture SSH listener exited prematurely")
                time.sleep(0.1)
        finally:
            closed = server.close()
            shutil.copyfile(server.root / "sshd.log", root / "server-sshd.log")
            write_json(root / "closed.json", {"listenerClosed": closed})
        if not closed:
            raise RuntimeError("fixture listener closure was not verified")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("usage: fixture ROOT TIMEOUT_SECONDS")
    run(Path(sys.argv[1]), int(sys.argv[2]))
