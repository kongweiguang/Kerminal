// @author kongweiguang

import { ui } from "../src/builders.js";
import type { JsonValue, Kerminal, KerminalEnvironment, Permission, PluginManifest, RemoteCall, RpcResponse, ViewFetchParams } from "../src/index.js";

const node = ui.stack({ children: [ui.button({ action: "save", label: "Save", id: "save", "x-test": true })] });
const iconButton = ui.button({ action: "save", icon: "save" });
const rpcList = ui.list({ source: "rpc", id: "items", key: "id", title: "name" });
const inlineList = ui.list({ items: "$items", key: "id", title: "name" });
const rpcListWithNull = ui.list({ source: "rpc", id: "items", items: null, key: "id", title: "name" });
const inlineListWithNull = ui.list({ source: null, items: "$items", key: "id", title: "name" });
const page: ViewFetchParams = {
  instance: "tab-1", node: "rows", range: [0, 20], parent_key: "parent",
  sort: { column: "name", direction: "asc" },
};
const response: RpcResponse = { jsonrpc: "2.0", id: "p:1", result: null };
const permissions: Permission[] = ["ui.notify", "network:api.example.com"];
const manifest: PluginManifest = {
  schema: 1, id: "acme.example", publisher: "acme", version: "1.0.0", api: "^1.0",
  name: { en: "Example", "zh-CN": "示例" }, description: { en: "Example plugin" }, runtime: { kind: "none" },
};

kerminal.onRequest("view.fetch", (params) => ({ total: params.range[1], rows: [] }));
kerminal.onRequest("view.visibility", (params) => { void params.focused; });
kerminal.view.update("tab-1", { ready: true });
const environment: KerminalEnvironment = kerminal.environment;
const controller = new AbortController();
const requestResult: Promise<JsonValue> = kerminal.request("custom.request", {}, { signal: controller.signal });
const updateResult: Promise<JsonValue> = kerminal.view.update("tab-1", { ready: true }, { signal: controller.signal });
// 已知方法保留各自的参数与结果类型，同时提供协作取消信号。
kerminal.onCancellableRequest("view.fetch", (params, signal) => {
  signal.throwIfAborted();
  signal.addEventListener("abort", () => { void signal.reason; }, { once: true });
  // 同一信号能传给后续宿主请求、视图更新与网络调用，贯通协作取消。
  void kerminal.request("custom.request", {}, { signal });
  void kerminal.view.update("tab-1", {}, { signal });
  void fetch("https://example.com", { signal });
  return { total: params.range[1], rows: [] };
});
// 扩展方法只承诺 JSON 参数，不会误用已知协议方法的结果约束。
kerminal.onCancellableRequest("acme.custom", (params, signal) => ({ cancelled: signal.aborted, value: params.value }));
const storedValue: Promise<JsonValue> = kerminal.storage.get("key");
const stored: Promise<null> = kerminal.storage.set("key", { nested: [1, null, "value"] });
const deleted: Promise<boolean> = kerminal.storage.delete("key");
const fileBytes: Promise<Uint8Array> = kerminal.storage.readFile("notes.txt");
const wroteText: Promise<null> = kerminal.storage.writeFile("notes.txt", "hello");
const wroteBuffer: Promise<null> = kerminal.storage.writeFile("notes.bin", new ArrayBuffer(2));
const wroteView: Promise<null> = kerminal.storage.writeFile("notes.bin", new Uint8Array([1, 2]));
const removed: Promise<boolean> = kerminal.storage.removeFile("notes.txt");
const files: Promise<string[]> = kerminal.storage.list();
const nestedFiles: Promise<string[]> = kerminal.storage.list("nested");
const bridge: Kerminal = window.kerminal;
if (bridge.storage) void bridge.storage.get("key");
const notify: RemoteCall = kerminal.ui.notify;
const hostCalls: Promise<unknown>[] = [
  notify({ level: "info", message: "ready" }),
  kerminal.ui.confirm(), kerminal.ui.prompt(), kerminal.ui.pickFile(),
  kerminal.settings.get(), kerminal.state.get(), kerminal.state.set({ value: 1 }),
  kerminal.secrets.get(), kerminal.secrets.set(), kerminal.secrets.delete(),
  kerminal.clipboard.read(), kerminal.clipboard.write(), kerminal.shell.openExternal(),
  kerminal.hosts.list(), kerminal.terminal.paste(), kerminal.events.subscribe(),
];

// @ts-expect-error 动作是按钮的必填字段。
ui.button({ label: "Save" });
// @ts-expect-error 按钮至少提供可访问的名称或图标。
ui.button({ action: "save" });
// @ts-expect-error null 不能满足名称或图标的要求。
ui.button({ action: "save", label: null, icon: null });
// @ts-expect-error 分栏至少需要两个子节点。
ui.split({ children: [ui.text({ text: "Only one" })] });
// @ts-expect-error 表格需要至少一个列定义。
ui.table({ items: "$items", key: "id", columns: [] });
// @ts-expect-error RPC 集合必须有节点 ID，以关联分页响应。
ui.list({ source: "rpc", key: "id", title: "name" });
// @ts-expect-error RPC 数据源与非空内联绑定互斥。
ui.list({ source: "rpc", id: "items", items: "$items", key: "id", title: "name" });
// @ts-expect-error 视图不接受颜色或其他任意样式。
ui.text({ text: "bad", color: "red" });
// @ts-expect-error 语义间距由协议枚举约束。
ui.stack({ children: [], gap: "huge" });
// @ts-expect-error 只有 x- 前缀的扩展字段有效。
ui.text({ text: "bad", unknown: true });
// @ts-expect-error 分页区间必须恰好两个元素。
const badRange: ViewFetchParams = { instance: "tab-1", node: "rows", range: [0] };
// @ts-expect-error 排序方向来自协议枚举，不能传入其他值。
const badSort: ViewFetchParams = { instance: "tab-1", node: "rows", range: [0, 20], sort: { column: "name", direction: "ascending" } };
// @ts-expect-error 成功结果和错误不可同时出现。
const badResponse: RpcResponse = { jsonrpc: "2.0", result: null, error: { code: -1, message: "bad" } };
// @ts-expect-error error 字段存在时必须是协议错误对象，不能为 null。
const nullError: RpcResponse = { jsonrpc: "2.0", id: null, error: null };
// @ts-expect-error 不属于协议权限枚举的值不能静默通过。
const badPermission: Permission = "unrestricted";
// @ts-expect-error 本地化映射必须包含英语，其他语言可按需添加。
const badManifest: PluginManifest = { ...manifest, name: { "zh-CN": "示例" } };
// @ts-expect-error 已知 RPC 的处理结果必须遵循对应协议类型。
kerminal.onRequest("view.fetch", () => ({ rows: [] }));
// @ts-expect-error 已知可取消 RPC 的结果仍必须符合协议。
kerminal.onCancellableRequest("view.fetch", () => ({ rows: [] }));
// @ts-expect-error 处理器收到的是 AbortSignal，而非任意值。
kerminal.onCancellableRequest("view.fetch", (_params, _signal: number) => ({ total: 0, rows: [] }));
// @ts-expect-error 请求选项必须提供真正的取消信号。
kerminal.request("custom.request", {}, { signal: "cancel" });
// @ts-expect-error 视图更新沿用同一信号类型。
kerminal.view.update("tab-1", {}, { signal: 1 });
// @ts-expect-error 本地存储的键必须是字符串。
kerminal.storage.get(1);
// @ts-expect-error 存储值必须可表示为 JSON。
kerminal.storage.set("key", () => 1);
// @ts-expect-error 文件内容必须是字符串、缓冲区或缓冲区视图。
kerminal.storage.writeFile("file", 123);
// @ts-expect-error 网页桥的存储可选，使用前必须检查能力存在。
window.kerminal.storage.get("key");
// @ts-expect-error 宿主环境是只读快照，不由插件就地修改。
kerminal.environment.theme = "dark";
// @ts-expect-error 宿主能力只接受 JSON 对象参数。
kerminal.ui.notify("ready");

void [node, iconButton, rpcList, inlineList, rpcListWithNull, inlineListWithNull, page, response, permissions, manifest, environment, hostCalls, requestResult, updateResult, storedValue, stored, deleted, fileBytes, wroteText, wroteBuffer, wroteView, removed, files, nestedFiles, badRange, badSort, badResponse, nullError, badPermission, badManifest];
