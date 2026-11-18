"""只向隔离验收进程提供一次性 HTTP 目录与真实压缩包。"""
# @author kongweiguang

import argparse
import hashlib
import io
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import tarfile


def package(plugin_id: str) -> bytes:
    """使用完整格式的最小包，让后续下载仍经过生产解包、清单和摘要检查。"""
    manifest = json.dumps({
        "schema": 1, "id": plugin_id, "name": {"en": plugin_id},
        "description": {"en": "Isolated marketplace fixture"},
        "version": "1.0.0", "api": "^1.0", "publisher": "acme",
        "runtime": {"kind": "none"},
    }, separators=(",", ":")).encode()
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:gz") as archive:
        header = tarfile.TarInfo("kerminal-plugin.json")
        header.size = len(manifest)
        header.mode = 0o644
        archive.addfile(header, io.BytesIO(manifest))
    return output.getvalue()


def main() -> None:
    """监听随机 loopback 端口并发布 ready 收据；父脚本独占此进程的结束与回读。"""
    parser = argparse.ArgumentParser()
    parser.add_argument("--ready", type=Path, required=True)
    args = parser.parse_args()
    routes: dict[str, bytes] = {}

    class Handler(BaseHTTPRequestHandler):
        """只提供预先构造的路径，禁止目录映射与任意文件读取。"""

        def do_GET(self) -> None:
            """每次响应都明确大小和关闭边界，未知地址保留真实 404。"""
            body = routes.get(self.path)
            self.send_response(200 if body is not None else 404)
            self.send_header("Content-Length", str(len(body) if body else 0))
            self.send_header("Connection", "close")
            self.end_headers()
            if body:
                self.wfile.write(body)

        def log_message(self, _format: str, *_args: object) -> None:
            """夹具没有凭据，仍仅输出状态码与固定资源路径，避免默认反向DNS行为。"""
            print(f"fixture GET {self.path}", flush=True)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    address = f"http://127.0.0.1:{server.server_address[1]}"
    plugins = []
    for plugin_id in ["acme.market-one", "acme.market-two"]:
        body = package(plugin_id)
        path = f"/{plugin_id}.kplugin"
        routes[path] = body
        plugins.append({
            "id": plugin_id, "name": {"en": plugin_id},
            "description": {"en": "Isolated marketplace fixture"},
            "publisher": "acme", "categories": ["productivity"],
            "repository": "https://github.com/acme/fixture", "permissions": [],
            "versions": [{"version": "1.0.0", "api": "^1.0",
                          "published_at": "2026-01-01T00:00:00Z", "assets": [{
                              "platform": "any", "url": address + path,
                              "size": len(body), "sha256": hashlib.sha256(body).hexdigest(),
                          }]}],
        })
    # 自定义来源标识证明目录不再绑定固定轨道；只发布 JSON，不提供签名文件。
    raw = json.dumps({"schema": 1, "source_id": "fixture", "sequence": 1,
                      "plugins": plugins}, separators=(",", ":")).encode()
    routes["/catalog.json"] = raw
    ready_temporary = args.ready.with_suffix(".tmp")
    ready_temporary.write_text(json.dumps({"url": address + "/catalog.json",
                                          "catalogSha256": hashlib.sha256(raw).hexdigest()}), encoding="utf-8")
    ready_temporary.replace(args.ready)
    try:
        server.serve_forever(poll_interval=0.2)
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
