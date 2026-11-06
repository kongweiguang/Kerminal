#!/usr/bin/env python3
"""为 SFTP 跟随验收创建仅属于本轮的 SSH 服务与目录。

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


def load_server():
    """沿用既有 loopback sshd 和主机密钥约束，避免改变系统 SSH 服务。"""
    source = Path(__file__).with_name("command-suggestions-linux.py")
    spec = importlib.util.spec_from_file_location("sftp_follow_linux_fixture", source)
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load the isolated SSH fixture")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.PrivateSshServer


def write_json(path: Path, payload: dict) -> None:
    """只原子发布完整回执，防止 Windows 进程读取到半份凭据元数据。"""
    temporary = path.with_suffix(".tmp")
    with temporary.open("x", encoding="utf-8") as stream:
        json.dump(payload, stream, ensure_ascii=False)
    os.replace(temporary, path)


def run(root: Path, timeout: int) -> None:
    """目录和密钥仅存于本次 fixture；退出后分别确认 listener 与远端目录回收。"""
    allowed = (Path(__file__).resolve().parents[2] /
               "target/diagnostics/sftp-follow-ssh-fixtures").resolve(strict=True)
    root = root.resolve(strict=True)
    if root == allowed or allowed not in root.parents or not 30 <= timeout <= 900:
        raise RuntimeError("fixture directory or timeout is outside the permitted boundary")
    if any(item.name not in {"fixture-stdout.log", "fixture-stderr.log"} for item in root.iterdir()):
        raise RuntimeError("fixture directory is not empty")
    server_class = load_server()
    remote_root = None
    closed = False
    try:
        with tempfile.TemporaryDirectory(prefix="kerminal-sftp-follow-", dir="/run") as temporary:
            remote_root = Path(temporary)
            server = None
            try:
                server = server_class(remote_root / "ssh")
                directory = remote_root / "follow"
                directory.mkdir(mode=0o700)
                (directory / "rr-ui-base").mkdir()
                for leaf, marker in (("first", "rr-ui-first"), ("目录 空间", "rr-ui-unicode"),
                                     ("last", "rr-ui-last")):
                    target = directory / leaf
                    target.mkdir()
                    (target / marker).mkdir()
                shutil.copyfile(server.client_key, root / "client-key")
                public_key = (server.root / "server-key.pub").read_text(encoding="utf-8").split()
                if len(public_key) < 2 or public_key[0] != "ssh-ed25519":
                    raise RuntimeError("unexpected fixture host key")
                (root / "known_hosts").write_text(
                    f"[127.0.0.1]:{server.port} {public_key[0]} {public_key[1]}\n",
                    encoding="utf-8",
                )
                write_json(root / "ready.json", {
                    "port": server.port, "username": "root", "remoteDirectory": str(directory)
                })
                deadline = time.monotonic() + timeout
                while not (root / "stop.fixture").exists() and time.monotonic() < deadline:
                    if server.process.poll() is not None:
                        raise RuntimeError("fixture SSH listener exited prematurely")
                    time.sleep(0.1)
            finally:
                if server is not None:
                    closed = server.close()
                    shutil.copyfile(server.root / "sshd.log", root / "server-sshd.log")
    finally:
        write_json(root / "closed.json", {
            "listenerClosed": closed,
            "remoteDirectoryRemoved": remote_root is not None and not remote_root.exists(),
        })
    if not closed:
        raise RuntimeError("fixture listener closure was not verified")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("usage: fixture ROOT TIMEOUT_SECONDS")
    run(Path(sys.argv[1]), int(sys.argv[2]))
