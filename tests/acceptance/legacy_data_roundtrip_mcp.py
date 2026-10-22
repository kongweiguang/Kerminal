"""旧新数据往返脚本专用的最小 Streamable HTTP MCP client。

该模块只接受 loopback endpoint，响应错误不回显请求正文，避免测试密码进入日志。

@author kongweiguang
"""

from __future__ import annotations

import http.client
import json
from typing import Any, Iterator
from urllib.parse import urlsplit


MCP_PROTOCOL_VERSION = "2025-03-26"


class AcceptanceError(RuntimeError):
    """给验收调用者的静态错误，不携带 MCP 请求正文或秘密。"""


class McpClient:
    """面向本机 RMCP Streamable HTTP server 的最小状态化客户端。"""

    def __init__(self, endpoint: str, timeout_seconds: float = 20.0) -> None:
        """只接受回环地址和 `/mcp` 路径，防止测试秘密意外发往远程服务。"""
        parsed = urlsplit(endpoint)
        if parsed.scheme != "http" or parsed.hostname not in {"127.0.0.1", "localhost"}:
            raise AcceptanceError("MCP endpoint 必须是本机 HTTP 回环地址")
        if parsed.path != "/mcp" or parsed.query or parsed.fragment:
            raise AcceptanceError("MCP endpoint 路径必须是 /mcp，且不能带 query 或 fragment")
        try:
            port = parsed.port
        except ValueError:
            raise AcceptanceError("MCP endpoint 端口无效") from None
        if port is None:
            raise AcceptanceError("MCP endpoint 必须显式包含端口")
        self._host = "127.0.0.1" if parsed.hostname == "localhost" else parsed.hostname
        self._port = port
        self._path = parsed.path
        self._timeout = timeout_seconds
        self._session_id: str | None = None
        self._protocol_version = MCP_PROTOCOL_VERSION
        self._next_request_id = 1

    def initialize(self) -> dict[str, Any]:
        """先完成 MCP initialize 与 initialized 通知，保留服务端返回的 session id。"""
        response = self._rpc(
            "initialize",
            {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "kerminal-legacy-roundtrip", "version": "1.0.0"},
            },
            remember_session=True,
            is_initialize=True,
        )
        version = response.get("protocolVersion")
        if isinstance(version, str):
            self._protocol_version = version
        self._post({"jsonrpc": "2.0", "method": "notifications/initialized"})
        return response

    def list_tools(self) -> set[str]:
        """读取当前进程实际暴露的 tool id，避免依赖静态 catalog 推断运行态能力。"""
        result = self._rpc("tools/list", {})
        tools = result.get("tools")
        if not isinstance(tools, list):
            raise AcceptanceError("MCP tools/list 响应格式无效")
        return {
            item["name"]
            for item in tools
            if isinstance(item, dict) and isinstance(item.get("name"), str)
        }

    def call_tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        """调用一个 MCP tool 并保留结构化回执供字段断言，不打印整个响应。"""
        return self._rpc("tools/call", {"name": name, "arguments": arguments})

    def _rpc(
        self,
        method: str,
        params: dict[str, Any],
        remember_session: bool = False,
        is_initialize: bool = False,
    ) -> dict[str, Any]:
        """为每次 JSON-RPC 请求分配单调 id，并将协议错误转换为静态脚本错误。"""
        request_id = self._next_request_id
        self._next_request_id += 1
        message = self._post(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params,
            },
            remember_session=remember_session,
            include_protocol_version=not is_initialize,
        )
        if message.get("id") != request_id:
            raise AcceptanceError("MCP 响应 id 与请求不匹配")
        if "error" in message:
            raise AcceptanceError(f"MCP {method} 请求被拒绝；响应正文已省略")
        result = message.get("result")
        if not isinstance(result, dict):
            raise AcceptanceError(f"MCP {method} 响应缺少 result")
        return result

    def _post(
        self,
        message: dict[str, Any],
        remember_session: bool = False,
        include_protocol_version: bool = True,
    ) -> dict[str, Any]:
        """按 Streamable HTTP 发送 JSON-RPC；网络边界固定为 localhost。"""
        connection = http.client.HTTPConnection(self._host, self._port, timeout=self._timeout)
        headers = {
            "Accept": "application/json, text/event-stream",
            "Content-Type": "application/json",
        }
        if include_protocol_version:
            headers["MCP-Protocol-Version"] = self._protocol_version
        if self._session_id is not None:
            headers["Mcp-Session-Id"] = self._session_id
        body = json.dumps(message, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        try:
            connection.request("POST", self._path, body=body, headers=headers)
            response = connection.getresponse()
            response_body = response.read()
            session_id = response.getheader("Mcp-Session-Id")
            if remember_session and session_id:
                self._session_id = session_id
            if response.status not in {200, 202}:
                raise AcceptanceError(f"MCP endpoint 返回 HTTP {response.status}")
            if not response_body:
                return {}
            content_type = response.getheader("Content-Type", "").lower()
            if "text/event-stream" in content_type:
                payloads = [
                    line[5:].strip()
                    for line in response_body.decode("utf-8", errors="replace").splitlines()
                    if line.startswith("data:")
                ]
                for payload in reversed(payloads):
                    try:
                        decoded = json.loads(payload)
                    except json.JSONDecodeError:
                        continue
                    if isinstance(decoded, dict):
                        return decoded
                raise AcceptanceError("MCP SSE 响应中没有 JSON-RPC 消息")
            decoded = json.loads(response_body.decode("utf-8"))
            if not isinstance(decoded, dict):
                raise AcceptanceError("MCP HTTP 响应不是 JSON-RPC object")
            return decoded
        except (OSError, TimeoutError, http.client.HTTPException, json.JSONDecodeError) as error:
            raise AcceptanceError(f"无法完成本机 MCP 请求（{type(error).__name__}）") from None
        finally:
            connection.close()


def json_tree(value: Any) -> Iterator[Any]:
    """递归遍历结构化内容，并尝试解析 RMCP 文本 block 中的 JSON。"""
    yield value
    if isinstance(value, dict):
        for child in value.values():
            yield from json_tree(child)
    elif isinstance(value, list):
        for child in value:
            yield from json_tree(child)
    elif isinstance(value, str) and value.lstrip().startswith(("{", "[")):
        try:
            decoded = json.loads(value)
        except json.JSONDecodeError:
            return
        yield from json_tree(decoded)


def find_string_field(value: Any, field: str) -> str | None:
    """从新旧 RMCP 的结构化或 text-encoded 回执中提取稳定字符串字段。"""
    for item in json_tree(value):
        if isinstance(item, dict) and isinstance(item.get(field), str):
            return item[field]
    return None


def tool_result_failed(result: dict[str, Any]) -> bool:
    """兼容 MCP 标准 isError 与旧版结构化 status 字段判断工具是否失败。"""
    for item in json_tree(result):
        if not isinstance(item, dict):
            continue
        if item.get("isError") is True:
            return True
        if isinstance(item.get("status"), str) and item["status"].lower() in {"failed", "error"}:
            return True
    return False


def require_tool_success(name: str, result: dict[str, Any]) -> None:
    """在阶段边界拒绝失败回执，错误消息不拼接可能含输入的 MCP 正文。"""
    if tool_result_failed(result):
        raise AcceptanceError(f"MCP tool {name} 返回失败；敏感响应正文已省略")
