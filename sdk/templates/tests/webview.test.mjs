// @author kongweiguang
import assert from "node:assert/strict";
import { access, readFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
import vm from "node:vm";
import test from "node:test";

/** 直接执行模板网页源码，防止模板能打包但保存与草稿保护流程不成立。 */
async function source(name) {
  return stripTypeScriptTypes(await readFile(new URL(`../webview/${name}.ts`, import.meta.url), "utf8"));
}

/** 只在测试中模拟“有后端/无后端”两种请求结果，不伪造真实 WebView 验收。 */
async function load({ backend }) {
  const elements = new Map();
  for (const id of ["#note", "#save", "#status", "#editor"]) {
    elements.set(id, { value: "", disabled: false, textContent: "", handlers: {}, addEventListener(event, handler) { this.handlers[event] = handler; } });
  }
  const handlers = new Map();
  const calls = [];
  let failing = false;
  let actionError;
  const bridge = {
    onRequest(method, handler) { handlers.set(method, handler); },
    async request(method, params) {
      calls.push({ method, params });
      if (method === "view.action") {
        if (actionError) throw actionError;
        // 声明式插件没有运行进程，宿主以 JSON-RPC -32601 表示方法不支持。
        if (!backend) throw Object.assign(new Error("method not supported"), { code: -32601 });
      }
      if (failing) throw new Error("offline");
      return {};
    },
  };
  vm.runInNewContext(await source("web"), { window: { kerminal: bridge }, document: { querySelector(id) { return elements.get(id); } } });
  return { elements, handlers, calls, setFailing(value) { failing = value; }, setActionError(value) { actionError = value; } };
}

/** 有后端时保存走实例动作，草稿保护与失败重试保持原流程。 */
test("webview page saves through the sidecar and protects drafts", async () => {
  const { elements, handlers, calls, setFailing } = await load({ backend: true });
  handlers.get("view.open")({ data: { text: "saved" } });
  const note = elements.get("#note");
  note.value = "my draft";
  note.handlers.input();
  handlers.get("view.update")({ patch: { text: "remote" } });
  assert.equal(note.value, "my draft");
  const submit = elements.get("#editor").handlers.submit;
  setFailing(true);
  await submit({ preventDefault() {} });
  assert.equal(note.value, "my draft");
  assert.equal(elements.get("#save").disabled, false);
  assert.match(elements.get("#status").textContent, /Try again/);
  setFailing(false);
  await submit({ preventDefault() {} });
  assert.deepEqual(JSON.parse(JSON.stringify(calls.at(-2))), { method: "view.action", params: { action: "save", data: { text: "my draft" } } });
  assert.deepEqual(JSON.parse(JSON.stringify(calls.at(-1))), { method: "ui.notify", params: { level: "info", message: "Note saved" } });
  assert.equal(elements.get("#status").textContent, "Saved");
  handlers.get("view.update")({ patch: { text: "accepted" } });
  assert.equal(note.value, "accepted");
});

/** 无后端时宿主返回 -32601，回退到 state.set，仍复用宿主提供的状态与通知调用。 */
test("webview page falls back to host state without a sidecar", async () => {
  const { elements, handlers, calls } = await load({ backend: false });
  handlers.get("view.open")({ state: { text: "restored" } });
  assert.equal(elements.get("#note").value, "restored");
  elements.get("#note").value = "draft";
  await elements.get("#editor").handlers.submit({ preventDefault() {} });
  assert.deepEqual(JSON.parse(JSON.stringify(calls.at(-2))), { method: "state.set", params: { state: { text: "draft" } } });
  assert.deepEqual(JSON.parse(JSON.stringify(calls.at(-1))), { method: "ui.notify", params: { level: "info", message: "Note saved" } });
});

/** 业务失败或崩溃必须冒泡到保存流程，不能静默回退成 state.set 的假成功。 */
test("webview page does not fall back for sidecar business failures", async () => {
  const { elements, calls, setActionError } = await load({ backend: true });
  const submit = elements.get("#editor").handlers.submit;
  for (const error of [
    Object.assign(new Error("plugin rejected the save"), { code: -32100 }),
    new Error("plugin process exited"),
  ]) {
    setActionError(error);
    await submit({ preventDefault() {} });
    assert.equal(calls.some((call) => call.method === "state.set"), false);
    assert.equal(calls.at(-1).method, "view.action");
    assert.match(elements.get("#status").textContent, /Try again/);
    assert.equal(elements.get("#save").disabled, false);
  }
});

/** 两种清单都不得再出现 Script 入口、权限门禁或 csp 声明；宿主调用不受清单限制。 */
test("templates expose none and sidecar without the removed script runtime", async () => {
  const none = JSON.parse(await readFile(new URL("../webview/manifest.json", import.meta.url), "utf8"));
  const sidecar = JSON.parse(await readFile(new URL("../webview/manifest.sidecar.json", import.meta.url), "utf8"));
  assert.deepEqual(none.runtime, { kind: "none" });
  assert.equal(sidecar.runtime.kind, "sidecar");
  assert.ok(sidecar.runtime.main["windows-x86_64"]);
  for (const manifest of [none, sidecar]) {
    assert.equal(manifest.runtime.kind === "script", false);
    assert.equal(Object.hasOwn(manifest, "permissions"), false);
    assert.equal(Object.hasOwn(manifest.views.page, "csp"), false);
    assert.equal(JSON.stringify(manifest).includes("network:"), false);
  }
  for (const path of ["../webview/main.rs", "../webview/Cargo.toml.in", "../native/main.rs", "../native/Cargo.toml.in"]) {
    await access(new URL(path, import.meta.url));
  }
  const backend = await readFile(new URL("../webview/main.rs", import.meta.url), "utf8");
  for (const method of ["view.open", "view.action", "view.snapshot", "view.close"]) {
    assert.match(backend, new RegExp(`"${method.replace(".", "\\.")}"`));
  }
  assert.doesNotMatch(await readFile(new URL("../webview/web.ts", import.meta.url), "utf8"), /storage|QuickJS/);
});