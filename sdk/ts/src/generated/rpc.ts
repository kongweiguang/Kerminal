// 由 sdk/protocol/schema 生成；请运行 npm run generate 更新。
// @author kongweiguang

import type { JsonValue } from "../json.js";

export type RpcMessage = (RpcRequest | RpcNotification | RpcResponse);
export type CancelParams = { "id": string; };
export type CommandContext = { "host"?: JsonValue; "tab"?: JsonValue; };
export type CommandRunParams = { "command": string; "context": CommandContext; };
export type EventParams = { "name": string; "payload": JsonValue; };
export type HostOpenResult = { "state": JsonValue; "tab": string; };
export type HostOperationParams = { "config": JsonValue; "host_type": string; "secrets": JsonValue; };
export type HostTestResult = { "message"?: string | null; "ok": boolean; };
export type InitializeParams = { "data_dir": string; "generation": number; "host_api": string; "locale": string; "plugin": PluginIdentity; "protocol": number; "theme": string; };
export type InitializeResult = { "plugin": PluginIdentity; "protocol": number; };
export type PluginIdentity = { "id": string; "version": string; };
export type ProgressParams = { "message"?: string | null; "percent"?: number | null; "token": string; };
export type RpcError = { "code": number; "data"?: JsonValue; "message": string; };
export type RpcNotification = { "jsonrpc": "2.0"; "method": string; "params"?: (Record<string, JsonValue> | Array<JsonValue>); };
export type RpcRequest = { "id": string; "jsonrpc": "2.0"; "method": string; "params"?: (Record<string, JsonValue> | Array<JsonValue>); };
export type RpcResponse = { "error"?: RpcError; "id": (string | null); "jsonrpc": "2.0"; "result"?: JsonValue; } & ({ "id"?: string; "result": JsonValue; "error"?: never; } | { "error": RpcError; "result"?: never; });
export type ViewActionParams = { "action": string; "data": JsonValue; "instance": string; "item_key"?: string | null; "view"?: string | null; };
export type ViewActionResult = { "patch"?: JsonValue; "toast"?: JsonValue; };
export type ViewFetchParams = { "instance": string; "node": string; "parent_key"?: string | null; "range": [number, number]; "sort"?: (ViewFetchSort | null); "view"?: string | null; };
export type ViewFetchResult = { "rows": Array<JsonValue>; "total": number; };
export type ViewFetchSort = { "column": string; "direction": ViewSortDirection; };
export type ViewInstanceParams = { "instance": string; };
export type ViewOpenParams = { "instance": string; "scope": JsonValue; "state"?: JsonValue; "view": string; };
export type ViewOpenResult = { "data"?: JsonValue; };
export type ViewSortDirection = "asc" | "desc";
export type ViewStateResult = { "state"?: JsonValue; "state_version"?: number | null; };
export type ViewUpdateParams = { "instance": string; "patch": JsonValue; };
export type ViewVisibilityParams = { "focused": boolean; "instance": string; "visible": boolean; };
