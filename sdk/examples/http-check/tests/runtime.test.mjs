// @author kongweiguang
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
import test from "node:test";

/** 在受控 fetch 下运行实际分发脚本，检查权限边界和状态恢复。 */
async function load(fetch) {
  const handlers = new Map();
  const host = { environment: { locale: "zh-CN", theme: "light" }, onRequest(method, handler) { handlers.set(method, handler); } };
  const script = new vm.Script(await readFile(fileURLToPath(new URL("../dist/main.js", import.meta.url)), "utf8"));
  script.runInNewContext({ kerminal: host, fetch, AbortController, setTimeout, clearTimeout });
  return handlers;
}

/** 主机测试和打开共享同一真实响应；非成功状态不会伪装成可用。 */
test("host test, result tab and refresh", async () => {
  const urls = [];
  const handlers = await load(async (url) => { urls.push(url); return { ok: false, status: 503 }; });
  const params = { host_type: "http-endpoint", config: { url: "https://example.com/health" }, secrets: {} };
  const tested = await handlers.get("host.test")(params);
  assert.equal(tested.ok, false);
  assert.match(tested.message, /503/);
  const opened = await handlers.get("host.open")(params);
  assert.equal(opened.tab, "http-result");
  const restored = handlers.get("view.open")({ instance: "one", view: "http.result", scope: {}, state: opened.state });
  assert.equal(restored.data.url, params.config.url);
  const refreshed = await handlers.get("view.action")({ instance: "one", action: "refresh", data: {} });
  assert.equal(refreshed.patch.ok, false);
  assert.equal(handlers.get("view.close")({ instance: "one" }).state.message, refreshed.patch.message);
  assert.deepEqual(urls, [params.config.url, params.config.url, params.config.url]);
});

/** 域名、端口和凭据校验早于 fetch，绕过表单直接调用也无法扩大权限。 */
test("rejects unapproved endpoints before network access", async () => {
  let count = 0;
  const handlers = await load(async () => { count++; return { ok: true, status: 200 }; });
  for (const url of ["https://example.com.evil/", "https://user@example.com/", "https://example.com:443/", "http://example.com/"]) {
    const result = await handlers.get("host.test")({ host_type: "http-endpoint", config: { url }, secrets: {} });
    assert.equal(result.ok, false);
  }
  assert.equal(count, 0);
});

/** 关闭中的请求即使迟到也不能更新已关闭的实例。 */
test("ignores a response after its tab closes", async () => {
  let resolveFetch;
  const pending = new Promise((resolve) => { resolveFetch = resolve; });
  const handlers = await load(() => pending);
  handlers.get("view.open")({ instance: "one", view: "http.result", scope: {} });
  const result = handlers.get("view.action")({ instance: "one", action: "refresh", data: {} });
  handlers.get("view.close")({ instance: "one" });
  resolveFetch({ ok: true, status: 200 });
  await assert.rejects(result, /stale instance/);
});
