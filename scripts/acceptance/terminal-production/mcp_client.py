#!/usr/bin/env python3
"""隔离 MCP Streamable HTTP client 与结构化回执记录。

@author kongweiguang
"""

from __future__ import annotations

import http.client
import json
import time
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

from identity import AcceptanceFailure, InstanceRecord, verify_instance, utc_now

MCP_PROTOCOL_VERSION = "2025-03-26"
MAX_TOOL_LIST_PAGES = 100
REQUIRED_TOOLS = {
    "terminal.create", "terminal.write", "terminal.snapshot", "terminal.close",
    "terminal.list", "kerminal.runtime_snapshot",
}

class McpClient:
    """仅向已验证的本机 endpoint 发送现有 terminal runtime 工具请求。"""

    def __init__(self, record: InstanceRecord, timeout_seconds: float) -> None:
        """保留 Streamable HTTP session header，禁止重定向到其他目标。"""

        parsed = urlsplit(record.endpoint)
        self.record = record
        self.host = parsed.hostname or "127.0.0.1"
        self.port = parsed.port or 0
        self.path = parsed.path
        self.timeout = timeout_seconds
        self.session_id: str | None = None
        self.protocol_version = MCP_PROTOCOL_VERSION
        self.next_request_id = 1

    def initialize(self) -> dict[str, Any]:
        """完成 MCP initialize/initialized 往返，并使用服务端实际协议版本。"""

        result = self.rpc(
            "initialize",
            {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "kerminal-terminal-production-acceptance", "version": "1.0"},
            },
            remember_session=True,
            omit_protocol_header=True,
        )
        version = result.get("protocolVersion")
        if isinstance(version, str):
            self.protocol_version = version
        self.post({"jsonrpc": "2.0", "method": "notifications/initialized"})
        return result

    def list_tools(self) -> set[str]:
        """跟进 tools/list 游标并限制页数，避免分页遗漏或服务端游标死循环。"""

        names: set[str] = set()
        seen_cursors: set[str] = set()
        cursor: str | None = None
        for _ in range(MAX_TOOL_LIST_PAGES):
            params = {} if cursor is None else {"cursor": cursor}
            result = self.rpc("tools/list", params)
            tools = result.get("tools")
            if not isinstance(tools, list):
                raise AcceptanceFailure("tools/list 响应缺少工具数组")
            names.update(
                item["name"]
                for item in tools
                if isinstance(item, dict) and isinstance(item.get("name"), str)
            )
            next_cursor = result.get("nextCursor")
            if next_cursor is None:
                return names
            if not isinstance(next_cursor, str) or not next_cursor:
                raise AcceptanceFailure("tools/list nextCursor 必须是非空字符串")
            if next_cursor in seen_cursors:
                raise AcceptanceFailure("tools/list 返回重复 nextCursor，拒绝无限分页")
            seen_cursors.add(next_cursor)
            cursor = next_cursor
        raise AcceptanceFailure(f"tools/list 超过 {MAX_TOOL_LIST_PAGES} 页，拒绝继续读取")

    def call_tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        """只调用脚本固定的运行态工具，并返回脱敏契约中的结构化响应。"""

        return self.rpc("tools/call", {"name": name, "arguments": arguments})

    def verify_live_instance(self) -> None:
        """每次外部写入前复核 port owner，防止 app 退出后误连重用端口的进程。"""

        verify_instance(self.record, hash_binary=False)

    def rpc(
        self,
        method: str,
        params: dict[str, Any],
        remember_session: bool = False,
        omit_protocol_header: bool = False,
    ) -> dict[str, Any]:
        """验证单调 JSON-RPC id 和响应形状，不把响应正文拼入传输错误。"""

        request_id = self.next_request_id
        self.next_request_id += 1
        message = self.post(
            {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params},
            remember_session=remember_session,
            omit_protocol_header=omit_protocol_header,
        )
        if message is None or message.get("id") != request_id:
            raise AcceptanceFailure(f"MCP {method} 响应 id 不匹配")
        if "error" in message:
            error = message.get("error")
            code = error.get("code") if isinstance(error, dict) else "unknown"
            raise AcceptanceFailure(f"MCP {method} 协议请求失败（code={code}）")
        result = message.get("result")
        if not isinstance(result, dict):
            raise AcceptanceFailure(f"MCP {method} 响应缺少 result")
        return result

    def post(
        self,
        message: dict[str, Any],
        remember_session: bool = False,
        omit_protocol_header: bool = False,
    ) -> dict[str, Any] | None:
        """为每个 HTTP 请求创建短连接，只接收已校验 endpoint 的 JSON 或 SSE。"""

        connection = http.client.HTTPConnection(self.host, self.port, timeout=self.timeout)
        headers = {"Accept": "application/json, text/event-stream", "Content-Type": "application/json"}
        if not omit_protocol_header:
            headers["MCP-Protocol-Version"] = self.protocol_version
        if self.session_id is not None:
            headers["Mcp-Session-Id"] = self.session_id
        body = json.dumps(message, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        try:
            connection.request("POST", self.path, body=body, headers=headers)
            response = connection.getresponse()
            response_body = response.read()
            session_id = response.getheader("Mcp-Session-Id")
            if remember_session and session_id:
                self.session_id = session_id
            if response.status not in {200, 202}:
                raise AcceptanceFailure(f"MCP endpoint 返回 HTTP {response.status}")
            if not response_body:
                return None
            content_type = response.getheader("Content-Type", "").lower()
            if "text/event-stream" in content_type:
                for line in reversed(response_body.decode("utf-8", errors="replace").splitlines()):
                    if not line.startswith("data:"):
                        continue
                    try:
                        candidate = json.loads(line[5:].strip())
                    except json.JSONDecodeError:
                        continue
                    if isinstance(candidate, dict):
                        return candidate
                raise AcceptanceFailure("MCP SSE 响应没有 JSON-RPC 消息")
            try:
                candidate = json.loads(response_body.decode("utf-8"))
            except (UnicodeError, json.JSONDecodeError):
                raise AcceptanceFailure("MCP HTTP 响应不是有效 JSON") from None
            if not isinstance(candidate, dict):
                raise AcceptanceFailure("MCP HTTP 响应不是 JSON object")
            return candidate
        except (OSError, TimeoutError, http.client.HTTPException) as error:
            raise AcceptanceFailure(f"无法完成隔离 MCP 请求（{type(error).__name__}）") from None
        finally:
            connection.close()


def structured_data(result: dict[str, Any]) -> dict[str, Any]:
    """从当前 Kerminal structuredContent 契约提取 data，并拒绝工具错误回执。"""

    if result.get("isError") is True:
        content = result.get("structuredContent")
        kind = content.get("errorKind", "unknown") if isinstance(content, dict) else "unknown"
        raise AcceptanceFailure(f"MCP tool 返回错误（errorKind={kind}）")
    structured = result.get("structuredContent")
    if not isinstance(structured, dict):
        raise AcceptanceFailure("MCP tool 缺少 structuredContent")
    if isinstance(structured.get("errorKind"), str):
        raise AcceptanceFailure(f"MCP tool 返回错误（errorKind={structured['errorKind']}）")
    data = structured.get("data")
    if not isinstance(data, dict):
        raise AcceptanceFailure("MCP tool structuredContent.data 不是 object")
    return data


class EventWriter:
    """逐项落盘 JSONL 原始回执和异常，使中断后仍能定位已完成阶段。"""

    def __init__(self, path: Path) -> None:
        """打开本次唯一的 append-only 事件文件，避免覆盖旧验收证据。"""

        self.path = path
        self.stream = path.open("x", encoding="utf-8", newline="\n")

    def write(self, event: dict[str, Any]) -> None:
        """每条记录立即 flush，保留长稳场景中断前的最后一个已写事件。"""

        event.setdefault("atUtc", utc_now())
        self.stream.write(json.dumps(event, ensure_ascii=False, separators=(",", ":")) + "\n")
        self.stream.flush()

    def close(self) -> None:
        """确保最终 JSONL 事件已关闭并可供后续审阅。"""

        self.stream.close()


def measured_call(
    client: McpClient,
    events: EventWriter,
    tool: str,
    arguments: dict[str, Any],
    operation: str,
) -> tuple[dict[str, Any], float]:
    """记录纯 MCP 请求往返时间和原始结构化回执，不称为 UI 输入或帧延迟。"""

    client.verify_live_instance()
    started = time.perf_counter()
    result = client.call_tool(tool, arguments)
    elapsed_ms = (time.perf_counter() - started) * 1000
    events.write(
        {
            "kind": "mcpTool",
            "tool": tool,
            "operation": operation,
            "mcpCallResponseLatencyMs": elapsed_ms,
            "rawResult": result,
        }
    )
    structured_data(result)
    return result, elapsed_ms


def terminal_ids(data: dict[str, Any]) -> set[str]:
    """提取 terminal.list 的稳定 session IDs，忽略展示字段避免依赖整份 DTO。"""

    sessions = data.get("sessions")
    if not isinstance(sessions, list):
        raise AcceptanceFailure("terminal.list 缺少 sessions 数组")
    values = {item.get("sessionId") for item in sessions if isinstance(item, dict)}
    if any(not isinstance(value, str) for value in values):
        raise AcceptanceFailure("terminal.list 返回了无效 sessionId")
    return values


def call_list_data(client: McpClient, events: EventWriter, label: str) -> dict[str, Any]:
    """读取真实终端列表并只记录会话身份集合，避免泄漏 session cwd 等无关字段。"""

    client.verify_live_instance()
    started = time.perf_counter()
    result = client.call_tool("terminal.list", {})
    elapsed_ms = (time.perf_counter() - started) * 1000
    data = structured_data(result)
    ids = terminal_ids(data)
    events.write(
        {
            "kind": "mcpList",
            "operation": label,
            "mcpCallResponseLatencyMs": elapsed_ms,
            "sessionCount": len(ids),
            "sessionIds": sorted(ids),
            "rawResult": result,
        }
    )
    return data


def call_list(client: McpClient, events: EventWriter, label: str) -> set[str]:
    """返回 terminal.list session ID 集合，调用方不必重新解释列表 DTO。"""

    return terminal_ids(call_list_data(client, events, label))


def call_runtime_snapshot(
    client: McpClient, events: EventWriter, label: str
) -> dict[str, Any]:
    """核对专用实例的全局终端与 Agent 会话数，防止在有状态服务上混跑。"""

    client.verify_live_instance()
    started = time.perf_counter()
    result = client.call_tool("kerminal.runtime_snapshot", {})
    elapsed_ms = (time.perf_counter() - started) * 1000
    data = structured_data(result)
    runtime = data.get("runtime")
    if not isinstance(runtime, dict):
        raise AcceptanceFailure("kerminal.runtime_snapshot 缺少 runtime 状态")
    events.write(
        {
            "kind": "mcpRuntimeSnapshot",
            "operation": label,
            "mcpCallResponseLatencyMs": elapsed_ms,
            "runtime": runtime,
            "rawResult": result,
        }
    )
    return runtime


