// @author kongweiguang

/** 在等待宿主回执时保持双向请求有效，验证更新和动作关联。 */
async function handleAction(params) {
  const count = params.data.count + 1;
  const acknowledgement = await kerminal.view.update(params.instance, { count });
  return { patch: { count }, acknowledgement };
}

kerminal.onRequest("view.action", handleAction);

/** 清除单次与循环计时器后再返回结果，验证回调不会阻塞 RPC。 */
function exerciseTimers() {
  return new Promise((resolve, reject) => {
    const cancelled = setTimeout(() => reject(new Error("cancelled timer fired")), 1);
    clearTimeout(cancelled);
    let ticks = 0;
    const interval = setInterval(() => {
      ticks++;
      if (ticks === 3) {
        clearInterval(interval);
        setTimeout(() => resolve({ ticks }), 5);
      }
    }, 2);
  });
}

kerminal.onRequest("test.timers", exerciseTimers);

/** 宿主必须能中断死循环，不能依赖插件自行让出执行权。 */
function spinForever() {
  while (true) {}
}

/** 堆上限必须阻止单次分配绕过内存预算。 */
function exceedHeap() {
  return new ArrayBuffer(256 * 1024 * 1024);
}

kerminal.onRequest("test.spin", spinForever);
kerminal.onRequest("test.heap", exceedHeap);

/** 宿主拒绝授权时必须保留错误码，不能伪装成空成功。 */
async function rejectedHostCall() {
  try {
    await kerminal.clipboard.read({});
    return { unexpected: true };
  } catch (error) {
    return { code: error.code };
  }
}

kerminal.onRequest("test.rpc", rejectedHostCall);

const heldRequests = [];

/** 保持请求未完成，模拟随后取消和迟到结果。 */
function holdRequest() {
  return new Promise((resolve) => { heldRequests.push(resolve); });
}

/** 在取消后放行原任务，验证结果不会重新投递。 */
function releaseRequest() {
  heldRequests.shift()({ released: true });
  return {};
}

kerminal.onRequest("test.hold", holdRequest);
kerminal.onRequest("test.release", releaseRequest);

/** 插件业务码应交给调用方解释，而不是抹平成内部错误。 */
function rejectBusiness() {
  throw Object.assign(new Error("invalid note"), { code: -32101 });
}

kerminal.onRequest("test.business", rejectBusiness);

/** 同时覆盖代理项和目标缓冲区不足，防止截断 UTF-8 字符。 */
function encodeText() {
  const encoder = new TextEncoder();
  const destination = new Uint8Array(6);
  return {
    encoding: encoder.encoding,
    bytes: Array.from(encoder.encode("é中😀\ud800")),
    partial: encoder.encodeInto("A😀é", destination),
    destination: Array.from(destination),
  };
}

kerminal.onRequest("test.text", encodeText);

/** 后续微任务进入死循环时也要限制执行时间，并保留其他请求的机会。 */
function spinInPromise() {
  return Promise.resolve().then(spinForever);
}

/** 脱离请求的计时回调不能因为超时而令整个进程悄悄退出。 */
function spinInTimer() {
  setInterval(spinForever, 20);
  return {};
}

kerminal.onRequest("test.promise-spin", spinInPromise);
kerminal.onRequest("test.timer-spin", spinInTimer);

/** 动态模块与宿主进程对象不能从受限脚本中取得。 */
async function inspectSandbox() {
  let moduleRejected = false;
  try { await import("std"); } catch { moduleRejected = true; }
  return {
    require: typeof globalThis.require,
    process: typeof globalThis.process,
    fs: typeof globalThis.fs,
    moduleRejected,
  };
}

kerminal.onRequest("test.sandbox", inspectSandbox);

/** 所有便捷入口应复用双向 RPC，避免类型声明与运行时提供不同能力。 */
async function invokeHostApi(params) {
  const methods = {
    "ui.notify": kerminal.ui.notify,
    "ui.confirm": kerminal.ui.confirm,
    "ui.prompt": kerminal.ui.prompt,
    "ui.pickFile": kerminal.ui.pickFile,
    "settings.get": kerminal.settings.get,
    "state.get": kerminal.state.get,
    "state.set": kerminal.state.set,
    "secrets.get": kerminal.secrets.get,
    "secrets.set": kerminal.secrets.set,
    "secrets.delete": kerminal.secrets.delete,
    "clipboard.read": kerminal.clipboard.read,
    "clipboard.write": kerminal.clipboard.write,
    "shell.openExternal": kerminal.shell.openExternal,
    "hosts.list": kerminal.hosts.list,
    "terminal.paste": kerminal.terminal.paste,
    "events.subscribe": kerminal.events.subscribe,
  };
  return methods[params.method](params.payload);
}

kerminal.onRequest("test.host-api", invokeHostApi);

let cancellableSideEffects = 0;
let observedAbort = false;

/** 在取消时清除尚未执行的副作用，验证信号确实到达处理器。 */
function cancellableWork(_params, signal) {
  return new Promise((resolve) => {
    const timer = setTimeout(() => { cancellableSideEffects++; resolve({}); }, 100);
    signal.addEventListener("abort", () => {
      observedAbort = signal.aborted && signal.reason?.name === "AbortError";
      clearTimeout(timer);
      resolve({});
    }, { once: true });
  });
}

/** 等过原计时窗口后读取状态，避免迟到副作用被过早检查掩盖。 */
function cancellableStatus() {
  return new Promise((resolve) => setTimeout(() => resolve({ cancellableSideEffects, observedAbort }), 150));
}

/** 取消原因和监听器移除需要独立于 RPC 处理器也能使用。 */
function inspectAbortController() {
  const controller = new AbortController();
  let called = 0;
  const removed = () => { called += 100; };
  controller.signal.addEventListener("abort", removed);
  controller.signal.removeEventListener("abort", removed);
  controller.signal.addEventListener("abort", () => { called++; }, { once: true });
  controller.abort("reason");
  controller.abort("later");
  let thrown;
  try { controller.signal.throwIfAborted(); } catch (error) { thrown = error; }
  return { aborted: controller.signal.aborted, reason: controller.signal.reason, called, thrown };
}

kerminal.onCancellableRequest("test.cancellable", cancellableWork);
kerminal.onRequest("test.cancel-status", cancellableStatus);
kerminal.onRequest("test.abort", inspectAbortController);

/** 发出的宿主请求要收到取消通知，插件同时观察明确的取消错误。 */
async function cancelHostCall() {
  const controller = new AbortController();
  const result = kerminal.request("clipboard.read", {}, { signal: controller.signal });
  setTimeout(() => controller.abort(), 5);
  try { await result; return { unexpected: true }; }
  catch (error) { return { code: error.code }; }
}

/** 已取消的信号不得发出新请求，避免产生调用方以为未执行的副作用。 */
async function cancelBeforeHostCall() {
  const controller = new AbortController();
  controller.abort();
  try { await kerminal.request("clipboard.read", {}, { signal: controller.signal }); return { unexpected: true }; }
  catch (error) { return { code: error.code }; }
}

kerminal.onRequest("test.cancel-host-call", cancelHostCall);
kerminal.onRequest("test.cancel-before-host-call", cancelBeforeHostCall);

/** 测试通过同一公开入口触发存储操作，便于重启宿主验证磁盘结果。 */
async function exerciseStorage(params) {
  try {
    switch (params.op) {
      case "get": return { value: await kerminal.storage.get(params.key) };
      case "set": await kerminal.storage.set(params.key, params.value); return {};
      case "delete": return { deleted: await kerminal.storage.delete(params.key) };
      case "writeFile": await kerminal.storage.writeFile(params.path, params.text ?? new Uint8Array(params.bytes)); return {};
      case "readFile": return { bytes: Array.from(await kerminal.storage.readFile(params.path)) };
      case "removeFile": return { deleted: await kerminal.storage.removeFile(params.path) };
      case "list": return { names: await kerminal.storage.list(params.path) };
      default: throw new Error("unknown test operation");
    }
  } catch (error) { return { error: String(error.message) }; }
}

/** 每个 HTTP 测试报告真实响应以及消费约束，失败仍保留错误信息。 */
async function exerciseFetch(params, signal) {
  try {
    const options = { ...params.options };
    if (params.cancellable) options.signal = signal;
    if (params.preAborted) { const controller = new AbortController(); controller.abort(); options.signal = controller.signal; }
    const response = await fetch(params.url, options);
    const text = await response.text();
    let repeated = false;
    try { await response.text(); } catch { repeated = true; }
    return { status: response.status, ok: response.ok, url: response.url, redirected: response.redirected,
      contentType: response.headers.get("content-type"), text, bodyUsed: response.bodyUsed, repeated,
      responseType: response instanceof Response };
  } catch (error) { return { error: String(error.message), name: error.name }; }
}

/** URL、编码与摘要一起经过实际 QuickJS 边界，避免只检验原生辅助函数。 */
async function exerciseWeb() {
  const url = new URL("../a?x=1&x=2", "https://Example.COM/base/path");
  const params = url.searchParams;
  params.append("q", "中 文");
  url.hash = "part";
  const addresses = { href: url.href, values: params.getAll("x"), same: params === url.searchParams,
    origin: url.origin, ipv6: new URL("http://[::1]:8080/x").host,
    question: new URLSearchParams("??a=1").toString() };
  const hashes = {};
  for (const algorithm of ["SHA-256", "SHA-384", "SHA-512"]) {
    const digest = await crypto.subtle.digest(algorithm, new TextEncoder().encode("abc"));
    hashes[algorithm] = Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("");
  }
  let fatal = false;
  try { new TextDecoder("utf-8", { fatal: true }).decode(new Uint8Array([255])); } catch { fatal = true; }
  return { addresses, hashes, fatal, replacement: new TextDecoder().decode(new Uint8Array([255])),
    uuid: crypto.randomUUID(), uuid2: crypto.randomUUID() };
}

kerminal.onRequest("test.storage", exerciseStorage);
kerminal.onCancellableRequest("test.fetch", exerciseFetch);
kerminal.onRequest("test.web", exerciseWeb);

/** 四 MiB 边界的数据不经 RPC 返回，避免测试本身放大协议帧。 */
async function exerciseLargeStorage(params) {
  try {
    const bytes = new Uint8Array(params.size);
    bytes.fill(97);
    await kerminal.storage.writeFile("large.bin", bytes);
    const restored = await kerminal.storage.readFile("large.bin");
    return { length: restored.length, first: restored[0], last: restored[restored.length - 1] };
  } catch (error) { return { error: String(error.message) }; }
}

kerminal.onRequest("test.large-storage", exerciseLargeStorage);
