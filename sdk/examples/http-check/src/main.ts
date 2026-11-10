// @author kongweiguang
// 集成依赖：官方脚本插件启动来源、受 network 限制的 fetch 和 AbortController 由宿主提供。
import type { HostOperationParams, JsonValue, Kerminal, ViewActionParams, ViewOpenParams } from "../../../ts/src/index.js";

type ResultState = { url: string; message: string; ok: boolean };
type Environment = { locale: string; theme: string };
const host = kerminal as Kerminal & { readonly environment: Environment };
const sessions = new Map<string, ResultState>();
const messages = {
  en: { ok: "HTTP status", failed: "Could not reach this endpoint", invalid: "Only https://example.com URLs are allowed" },
  zh: { ok: "HTTP 状态码", failed: "无法访问此服务", invalid: "仅支持 https://example.com 地址" },
};

/** 未提供当前语言时回退英文，避免把异常对象泄漏到界面。 */
function text(key: keyof typeof messages.en): string {
  return messages[host.environment.locale.toLowerCase().startsWith("zh") ? "zh" : "en"][key];
}

/** 只访问明确声明的域名；同时拒绝凭据、端口和混淆主机名。 */
function endpoint(config: JsonValue): string {
  if (!config || typeof config !== "object" || Array.isArray(config) || typeof config.url !== "string") {
    throw new Error(text("invalid"));
  }
  const raw = config.url;
  if (!/^https:\/\/example\.com(?:\/[^#]*)?$/.test(raw)) throw new Error(text("invalid"));
  return raw;
}

/** 每次检查都有有限等待时间，不会把网络失败伪装成成功。 */
async function probe(url: string): Promise<ResultState> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 10_000);
  try {
    const response = await fetch(url, { method: "GET", signal: controller.signal, redirect: "error" });
    return { url, ok: response.ok, message: `${text("ok")}: ${response.status}` };
  } catch {
    return { url, ok: false, message: text("failed") };
  } finally {
    clearTimeout(timer);
  }
}

/** 测试连接仅使用配置中的允许地址，不记录传入的密钥。 */
async function testHost(params: HostOperationParams): Promise<{ ok: boolean; message: string }> {
  try {
    const result = await probe(endpoint(params.config));
    return { ok: result.ok, message: result.message };
  } catch {
    return { ok: false, message: text("invalid") };
  }
}

/** 打开前取得实际结果，标签状态包含原地址与检查结果供重启恢复。 */
async function openHost(params: HostOperationParams): Promise<{ tab: string; state: ResultState }> {
  return { tab: "http-result", state: await probe(endpoint(params.config)) };
}

/** 恢复缺损状态时只给默认地址，避免使用未验证的任意网络目标。 */
function openView(params: ViewOpenParams): { data: ResultState } {
  const saved = params.state && typeof params.state === "object" && !Array.isArray(params.state) ? params.state : {};
  let url = "https://example.com/";
  if (typeof saved.url === "string" && /^https:\/\/example\.com(?:\/[^#]*)?$/.test(saved.url)) url = saved.url;
  const state = { url, message: typeof saved.message === "string" ? saved.message : "", ok: saved.ok === true };
  sessions.set(params.instance, state);
  return { data: state };
}

/** 重查使用该实例自己的地址，切换标签不会改变正在处理的目标。 */
async function action(params: ViewActionParams): Promise<{ patch: ResultState }> {
  const current = sessions.get(params.instance);
  if (!current || params.action !== "refresh") throw new Error("stale or unknown action");
  const result = await probe(current.url);
  if (sessions.get(params.instance) !== current) throw new Error("stale instance");
  sessions.set(params.instance, result);
  return { patch: result };
}

/** 状态快照携带当前结果，不混用其他实例的检查记录。 */
function snapshot(params: { instance: string }): { state_version: number; state: ResultState } {
  const state = sessions.get(params.instance);
  if (!state) throw new Error("stale instance");
  return { state_version: 1, state };
}

/** 关闭后释放实例，迟到网络回执不能重新写入。 */
function closeView(params: { instance: string }): { state_version: number; state: ResultState } {
  const state = snapshot(params);
  sessions.delete(params.instance);
  return state;
}

host.onRequest("host.test", testHost);
host.onRequest("host.open", openHost);
host.onRequest("view.open", openView);
host.onRequest("view.action", action);
host.onRequest("view.snapshot", snapshot);
host.onRequest("view.close", closeView);
