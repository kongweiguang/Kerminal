// @author kongweiguang

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { createContext, runInContext } from "node:vm";
import { BridgeError, createBrowserBridge } from "../dist/browser.js";

const methods = [
  ["ui", "notify"], ["ui", "confirm"], ["ui", "prompt"], ["ui", "pickFile"],
  ["settings", "get"], ["state", "get"], ["state", "set"],
  ["secrets", "get"], ["secrets", "set"], ["secrets", "delete"],
  ["clipboard", "read"], ["clipboard", "write"], ["shell", "openExternal"],
  ["hosts", "list"], ["terminal", "paste"], ["events", "subscribe"],
];

/** 加载真实设施和桥接源码，使脚本与网页的公开能力差异能够被检测。 */
function scriptHost() {
  const sent = [];
  const operations = [];
  const context = createContext({
    AbortController, TextEncoder,
    __kerminalEmit: (message) => sent.push(JSON.parse(message)),
    __kerminalSchedule: () => 1,
    __kerminalCancelTimer: () => {},
    __kerminalLog: () => {},
    /** 原生请求只记录在队列中，由测试显式触发对应回执。 */
    __kerminalNativeStart: (message) => { operations.push(JSON.parse(message)); return operations.length; },
    /** 存储映射测试不需要真实后台任务，但设施仍须取得取消入口。 */
    __kerminalNativeCancel: () => {},
    __kerminalBinary: {
      /** 按视图自身的偏移和长度读取，避免测试把无关字节算进存储内容。 */
      bytesOf(value) {
        if (value instanceof ArrayBuffer) return new Uint8Array(value);
        if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
        throw new TypeError("expected an ArrayBuffer or view");
      },
      /** 测试代替原生转码，实际存储读写由宿主测试覆盖。 */
      toBase64(value) { return Buffer.from(value.buffer, value.byteOffset, value.byteLength).toString("base64"); },
      /** 返回字节数组，保持脚本存储对调用方暴露的类型。 */
      fromBase64(value) { return Uint8Array.from(Buffer.from(value, "base64")); },
    },
  });
  for (const file of ["facilities.js", "bridge.js"]) {
    const source = readFileSync(new URL(`../../script-host/src/${file}`, import.meta.url), "utf8");
    runInContext(source, context);
  }
  return { context, sent, operations, bridge: context.kerminal };
}

/** 模拟视图所属传输，保留实例和代次供每一条请求检查。 */
function browserHost(environment) {
  const sent = [];
  let listener;
  const bridge = createBrowserBridge({
    send: (envelope) => sent.push(structuredClone(envelope)),
    subscribe: (callback) => { listener = callback; return () => { listener = undefined; }; },
  }, { view_instance: "tab-1", generation: 7 }, environment);
  return {
    sent, bridge,
    deliver: (message) => listener?.({ view_instance: "tab-1", generation: 7, message }),
  };
}

/** 以公开对象形状核对方法清单，新增或遗漏宿主能力都会使测试失败。 */
function publicMethods(bridge) {
  return Object.keys(bridge)
    .filter((namespace) => !["environment", "view"].includes(namespace) && typeof bridge[namespace] === "object")
    .flatMap((namespace) => Object.keys(bridge[namespace]).map((method) => `${namespace}.${method}`))
    .sort();
}

/** 逐项对照真实脚本入口和浏览器封装，验证方法名、默认参数及结果透传。 */
test("browser facade matches the real script host wrappers and RPC results", async () => {
  const script = scriptHost();
  const initialEnvironment = { locale: "zh-CN", theme: "dark" };
  const browser = browserHost(initialEnvironment);
  assert.deepEqual(JSON.parse(JSON.stringify(script.bridge.environment)), { locale: "", theme: "" });
  script.context.__kerminalInit(JSON.stringify(initialEnvironment));
  initialEnvironment.theme = "light";
  assert.deepEqual(JSON.parse(JSON.stringify(script.bridge.environment)), { locale: "zh-CN", theme: "dark" });
  assert.deepEqual(browser.bridge.environment, { locale: "zh-CN", theme: "dark" });
  assert.equal(Object.isFrozen(browser.bridge.environment), true);
  const expected = methods.map(([namespace, method]) => `${namespace}.${method}`).sort();
  const scriptStorage = ["get", "set", "delete", "readFile", "writeFile", "removeFile", "list"]
    .map((method) => `storage.${method}`);
  assert.deepEqual(publicMethods(script.bridge), [...expected, ...scriptStorage].sort());
  assert.deepEqual(publicMethods(browser.bridge), expected);
  assert.equal(browser.bridge.storage, undefined);

  for (const [index, [namespace, method]] of methods.entries()) {
    const params = index % 2 === 0 ? { sample: index, nested: { ready: true } } : undefined;
    const scriptResult = script.bridge[namespace][method](params);
    const browserResult = browser.bridge[namespace][method](params);
    const request = {
      jsonrpc: "2.0", id: `p:${index + 1}`, method: `${namespace}.${method}`, params: params ?? {},
    };
    assert.deepEqual(script.sent.at(-1), request);
    assert.deepEqual(browser.sent.at(-1), { view_instance: "tab-1", generation: 7, message: request });
    const response = { jsonrpc: "2.0", id: request.id, result: { accepted: index } };
    script.context.__kerminalDispatch(JSON.stringify(response));
    browser.deliver(response);
    assert.deepEqual(JSON.parse(JSON.stringify(await scriptResult)), response.result);
    assert.deepEqual(await browserResult, response.result);
  }
  browser.bridge.close();
});

/** 核对真实设施的请求和返回值，避免类型声明偏离脚本宿主协议。 */
test("script storage maps all operations and preserves result types", async () => {
  const script = scriptHost();
  const storage = script.bridge.storage;
  /** 原生层在完成后回送 JSON，测试同时检查请求映射与公开返回值。 */
  async function verify(promise, request, nativeValue, expected) {
    assert.deepEqual(script.operations.at(-1), request);
    script.context.__kerminalNativeComplete(script.operations.length, JSON.stringify({ ok: true, value: nativeValue }));
    const result = await promise;
    if (expected instanceof Uint8Array) {
      assert.ok(result instanceof Uint8Array);
      assert.deepEqual([...result], [...expected]);
    } else {
      assert.deepEqual(JSON.parse(JSON.stringify(result)), expected);
    }
  }
  await verify(storage.get("theme"), { kind: "storage", op: "get", key: "theme" }, { dark: true }, { dark: true });
  await verify(storage.get("missing"), { kind: "storage", op: "get", key: "missing" }, null, null);
  await verify(storage.set("theme", { dark: true }), { kind: "storage", op: "set", key: "theme", value: { dark: true } }, null, null);
  await verify(storage.delete("theme"), { kind: "storage", op: "delete", key: "theme" }, true, true);
  await verify(storage.readFile("notes.txt"), { kind: "storage", op: "readFile", path: "notes.txt" }, "aGVsbG8=", new Uint8Array([104, 101, 108, 108, 111]));
  await verify(storage.writeFile("notes.txt", "hello"), { kind: "storage", op: "writeFile", path: "notes.txt", data: "aGVsbG8=" }, null, null);
  await verify(storage.writeFile("notes.bin", new Uint8Array([0, 1, 2, 3]).subarray(1, 3)), { kind: "storage", op: "writeFile", path: "notes.bin", data: "AQI=" }, null, null);
  await verify(storage.removeFile("notes.txt"), { kind: "storage", op: "removeFile", path: "notes.txt" }, false, false);
  await verify(storage.list(), { kind: "storage", op: "list", path: "" }, ["notes.bin"], ["notes.bin"]);
  await verify(storage.list("nested"), { kind: "storage", op: "list", path: "nested" }, [], []);
  await assert.rejects(storage.set("bad", undefined), { name: "TypeError" });
});

/** 覆盖错误与关闭时的在途请求，避免便捷入口绕过生命周期门禁。 */
test("facade methods propagate host errors and reject after the browser closes", async () => {
  const script = scriptHost();
  const browser = browserHost();
  assert.deepEqual(browser.bridge.environment, { locale: "", theme: "" });
  const scriptResult = script.bridge.ui.confirm();
  const browserResult = browser.bridge.ui.confirm();
  const response = { jsonrpc: "2.0", id: "p:1", error: { code: -32100, message: "denied" } };
  script.context.__kerminalDispatch(JSON.stringify(response));
  browser.deliver(response);
  await assert.rejects(scriptResult, (error) => error.code === -32100 && error.message === "denied");
  await assert.rejects(browserResult, (error) => error instanceof BridgeError && error.code === -32100);
  const pending = browser.bridge.state.set({ value: 1 });
  const cancelled = assert.rejects(pending, (error) => error instanceof BridgeError && error.code === -32004);
  browser.bridge.close();
  await cancelled;
  assert.deepEqual(browser.sent.at(-1).message, { jsonrpc: "2.0", method: "$/cancel", params: { id: "p:2" } });
  await assert.rejects(browser.bridge.ui.confirm(), (error) => error instanceof BridgeError && error.code === -32004);
});
