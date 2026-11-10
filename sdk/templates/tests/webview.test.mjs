// @author kongweiguang
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
import vm from "node:vm";
import test from "node:test";

/** 直接执行模板源码，防止模板能打包但示例桥接和状态流程仍不成立。 */
async function source(name) {
  return stripTypeScriptTypes(await readFile(new URL(`../webview/${name}.ts`, import.meta.url), "utf8"));
}

/** 两个实例保存和恢复相互隔离，关闭后旧实例不能继续写入。 */
test("webview runtime saves independent snapshots", async () => {
  const handlers = new Map();
  vm.runInNewContext(await source("main"), { kerminal: { onRequest(method, handler) { handlers.set(method, handler); } } });
  const open = handlers.get("view.open");
  const action = handlers.get("view.action");
  const snapshot = handlers.get("view.snapshot");
  open({ instance: "a", state: { text: "draft a" } });
  open({ instance: "b", state: { text: "draft b" } });
  action({ instance: "a", action: "save", data: { text: "saved a" } });
  assert.equal(snapshot({ instance: "b" }).state.text, "draft b");
  const saved = handlers.get("view.close")({ instance: "a" });
  assert.equal(saved.state.text, "saved a");
  assert.throws(() => action({ instance: "a", action: "save", data: { text: "late" } }));
  assert.equal(open({ instance: "restored", state: saved.state }).data.text, "saved a");
});

/** DOM 桩只覆盖桥接业务，不把此结果当成真实 WebView 的视觉验收。 */
test("webview browser protects drafts and retries failures", async () => {
  const elements = new Map();
  for (const id of ["#note", "#save", "#status", "#editor"]) {
    elements.set(id, { value: "", disabled: false, textContent: "", handlers: {}, addEventListener(event, handler) { this.handlers[event] = handler; } });
  }
  const handlers = new Map();
  const calls = [];
  let failing = false;
  const bridge = {
    onRequest(method, handler) { handlers.set(method, handler); },
    async request(method, params) {
      calls.push({ method, params });
      if (failing) throw new Error("offline");
      return { patch: { text: params.data.text } };
    },
  };
  vm.runInNewContext(await source("web"), { window: { kerminal: bridge }, document: { querySelector(id) { return elements.get(id); } } });
  handlers.get("view.open")({ data: { text: "saved" } });
  const note = elements.get("#note");
  note.value = "my draft";
  note.handlers.input();
  handlers.get("view.update")({ patch: { text: "remote" } });
  assert.equal(note.value, "my draft");
  const submit = elements.get("#editor").handlers.submit;
  failing = true;
  await submit({ preventDefault() {} });
  assert.equal(note.value, "my draft");
  assert.equal(elements.get("#save").disabled, false);
  assert.match(elements.get("#status").textContent, /Try again/);
  failing = false;
  await submit({ preventDefault() {} });
  assert.equal(calls.at(-1).params.data.text, "my draft");
  assert.equal(elements.get("#status").textContent, "Saved");
  handlers.get("view.update")({ patch: { text: "accepted" } });
  assert.equal(note.value, "accepted");
});
