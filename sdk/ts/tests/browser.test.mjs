// @author kongweiguang

import assert from "node:assert/strict";
import { test } from "node:test";
import { BridgeError, createBrowserBridge } from "../dist/browser.js";

class FakeTransport {
  sent = [];
  listener;
  error;
  unsubscribeError;

  /** 记录完整信封，以检查每次发送都固定了实例与代次。 */
  send(envelope) {
    if (this.error) throw this.error;
    this.sent.push(structuredClone(envelope));
  }

  /** 解绑后仍可保存旧回调，模拟真实浏览器中的迟到消息。 */
  subscribe(listener) {
    this.listener = listener;
    return () => {
      this.listener = undefined;
      if (this.unsubscribeError) throw this.unsubscribeError;
    };
  }

  /** 由测试显式控制入站次序，避免依赖实际窗口或定时器。 */
  deliver(message, viewInstance = "tab-1", generation = 7) {
    this.listener?.({ view_instance: viewInstance, generation, message });
  }
}

/** 验证协议错误码，避免把普通异常误判为预期的生命周期结果。 */
function hasCode(code) {
  return (error) => error instanceof BridgeError && error.code === code;
}

/** 浏览器与 QuickJS 的业务错误必须同义，且不能接受插件伪造的宿主协议错误。 */
test("handler errors preserve business codes and reject reserved codes", async () => {
  const transport = new FakeTransport();
  const bridge = createBrowserBridge(transport, { view_instance: "tab-1", generation: 7 });
  bridge.onRequest("fail", ({ code }) => { throw { code, message: "invalid note" }; });
  for (const [index, code] of [-32101, -32199, -32002, -32602, -32100.5].entries()) {
    transport.deliver({ jsonrpc: "2.0", id: `h:${index}`, method: "fail", params: { code } });
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(transport.sent.at(-1).message.error.code, index < 3 ? code : -32603);
    assert.equal(transport.sent.at(-1).message.error.message, "invalid note");
  }
  bridge.close();
});

test("responses and updates stay bound to their original instance and generation", async () => {
  const transport = new FakeTransport();
  const identity = { view_instance: "tab-1", generation: 7 };
  const bridge = createBrowserBridge(transport, identity);
  identity.generation = 8;
  const reply = bridge.request("settings.get");
  assert.deepEqual(transport.sent[0], {
    view_instance: "tab-1", generation: 7,
    message: { jsonrpc: "2.0", id: "p:1", method: "settings.get", params: {} },
  });
  let settled = false;
  reply.finally(() => { settled = true; });
  transport.deliver({ jsonrpc: "2.0", id: "p:1", result: "wrong instance" }, "tab-2", 7);
  transport.deliver({ jsonrpc: "2.0", id: "p:1", result: "wrong generation" }, "tab-1", 8);
  await Promise.resolve();
  assert.equal(settled, false);
  transport.deliver({ jsonrpc: "2.0", id: "p:1", result: { theme: "dark" } });
  assert.deepEqual(await reply, { theme: "dark" });

  await assert.rejects(bridge.view.update("tab-2", { value: 1 }), hasCode(-32004));
  await assert.rejects(bridge.request("view.update", { instance: "tab-2", patch: {} }), hasCode(-32004));
  const update = bridge.view.update("tab-1", { value: 2 });
  assert.equal(transport.sent.at(-1).message.params.instance, "tab-1");
  transport.deliver({ jsonrpc: "2.0", id: "p:2", result: null });
  assert.equal(await update, null);
  bridge.close();
});

test("abort rejects once, sends cancellation, and ignores the late response", async () => {
  const transport = new FakeTransport();
  const bridge = createBrowserBridge(transport, { view_instance: "tab-1", generation: 7 });
  const controller = new AbortController();
  const pending = bridge.request("ui.confirm", { message: "Continue?" }, { signal: controller.signal });
  controller.abort();
  await assert.rejects(pending, hasCode(-32002));
  assert.deepEqual(transport.sent[1].message, { jsonrpc: "2.0", method: "$/cancel", params: { id: "p:1" } });
  transport.deliver({ jsonrpc: "2.0", id: "p:1", result: true });
  await assert.rejects(bridge.request("ui.confirm", {}, { signal: controller.signal }), hasCode(-32002));
  assert.equal(transport.sent.length, 2);
  bridge.close();
});

test("closing releases requests and rejects stale results even through an old callback", async () => {
  const transport = new FakeTransport();
  const bridge = createBrowserBridge(transport, { view_instance: "tab-1", generation: 7 });
  const oldListener = transport.listener;
  const pending = bridge.request("state.get");
  const rejected = assert.rejects(pending, hasCode(-32004));
  bridge.close();
  await rejected;
  assert.equal(transport.listener, undefined);
  assert.equal(transport.sent.at(-1).message.method, "$/cancel");
  oldListener({ view_instance: "tab-1", generation: 7, message: { jsonrpc: "2.0", id: "p:1", result: 1 } });
  await assert.rejects(bridge.request("state.get"), hasCode(-32004));
  bridge.close();
});

test("host cancellation aborts a running handler and suppresses its late result", async () => {
  const transport = new FakeTransport();
  const bridge = createBrowserBridge(transport, { view_instance: "tab-1", generation: 7 });
  let signal;
  let finish;
  bridge.onCancellableRequest("search", (_, handlerSignal) => {
    signal = handlerSignal;
    return new Promise((resolve) => { finish = resolve; });
  });
  transport.deliver({ jsonrpc: "2.0", id: "h:1", method: "search", params: {} });
  await Promise.resolve();
  assert.equal(signal.aborted, false);
  transport.deliver({ jsonrpc: "2.0", method: "$/cancel", params: { id: "h:1" } });
  assert.equal(signal.aborted, true);
  finish({ rows: [] });
  await Promise.resolve();
  await Promise.resolve();
  assert.deepEqual(transport.sent, []);
  bridge.close();
});

test("backpressure, send failure and malformed responses release their slots", async () => {
  const transport = new FakeTransport();
  const bridge = createBrowserBridge(transport, { view_instance: "tab-1", generation: 7 });
  transport.error = new Error("transport failed");
  await assert.rejects(bridge.request("state.get"), /transport failed/);
  transport.error = undefined;
  const malformed = bridge.request("state.get");
  transport.deliver({ jsonrpc: "2.0", id: "p:2", result: null, error: { code: -1, message: "bad" } });
  await assert.rejects(malformed, hasCode(-32600));
  const waiting = Array.from({ length: 64 }, () => bridge.request("state.get"));
  const settled = Promise.allSettled(waiting);
  await assert.rejects(bridge.request("state.get"), hasCode(-32003));
  bridge.close();
  assert.equal((await settled).filter((result) => result.status === "rejected").length, 64);
});

test("cleanup continues when the transport cannot unsubscribe", async () => {
  const transport = new FakeTransport();
  const bridge = createBrowserBridge(transport, { view_instance: "tab-1", generation: 7 });
  const pending = bridge.request("state.get");
  const rejected = assert.rejects(pending, hasCode(-32004));
  transport.unsubscribeError = new Error("unsubscribe failed");
  assert.throws(() => bridge.close(), /unsubscribe failed/);
  await rejected;
  assert.equal(transport.sent.at(-1).message.method, "$/cancel");
});
