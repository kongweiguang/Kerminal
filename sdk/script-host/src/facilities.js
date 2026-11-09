// @author kongweiguang

(() => {
  const start = globalThis.__kerminalNativeStart;
  const cancel = globalThis.__kerminalNativeCancel;
  const binary = globalThis.__kerminalBinary;
  delete globalThis.__kerminalNativeStart;
  delete globalThis.__kerminalNativeCancel;
  delete globalThis.__kerminalBinary;
  const pending = new Map();

  /** 中止错误有独立名称，调用方无需依赖平台错误文字。 */
  function abortError() {
    const error = new Error("This operation was aborted");
    error.name = "AbortError";
    return error;
  }

  /** 结算和取消使用相同清理路径，避免已经完成的操作保留监听器。 */
  function take(id) {
    const entry = pending.get(id);
    if (entry) {
      pending.delete(id);
      entry.cleanup();
    }
    return entry;
  }

  /** 后台任务只交换 JSON；取消信号必须同步传给实际网络任务。 */
  function operate(request, signal) {
    if (signal != null && (typeof signal.addEventListener !== "function" || typeof signal.removeEventListener !== "function")) {
      return Promise.reject(new TypeError("invalid abort signal"));
    }
    if (signal?.aborted) return Promise.reject(signal.reason ?? abortError());
    return new Promise((resolve, reject) => {
      let id;
      /** 取消前先撤销 Promise 槽位，后台迟到的结果不得再次结算。 */
      function onAbort() {
        if (!take(id)) return;
        cancel(id);
        reject(signal.reason ?? abortError());
      }
      try {
        id = start(JSON.stringify(request));
        pending.set(id, { resolve, reject, cleanup: () => signal?.removeEventListener("abort", onAbort) });
        signal?.addEventListener("abort", onAbort, { once: true });
        if (signal?.aborted) onAbort();
      } catch (error) {
        if (id !== undefined) { take(id); cancel(id); }
        reject(error);
      }
    });
  }

  /** 原生线程结果必须在解释器线程处理，丢弃已经取消的回执。 */
  function complete(id, serialized) {
    const entry = take(id);
    if (!entry) return;
    const result = JSON.parse(serialized);
    if (result.ok) entry.resolve(result.value);
    else entry.reject(new TypeError(result.error));
  }

  /** 在 JS 边界提前拒绝非法名称，原生层仍会再次校验。 */
  function headerName(name) {
    name = String(name).toLowerCase();
    if (!/^[!#$%&'*+.^_`|~0-9a-z-]+$/.test(name)) throw new TypeError("invalid header name");
    return name;
  }

  /** 清除两端空白并拒绝换行，避免单个值注入额外头部。 */
  function headerValue(value) {
    value = String(value).trim();
    if (/[\r\n\0]/.test(value)) throw new TypeError("invalid header value");
    return value;
  }

  class Headers {
    /** 重复头保留为独立条目，交给原生 HTTP 客户端执行发送。 */
    constructor(init = undefined) {
      this._items = [];
      if (init instanceof Headers) this._items = init._items.map((item) => [...item]);
      else if (init != null && typeof init[Symbol.iterator] === "function") {
        for (const pair of init) {
          const values = Array.from(pair);
          if (values.length !== 2) throw new TypeError("invalid header pair");
          this.append(values[0], values[1]);
        }
      } else if (init != null && typeof init === "object") {
        for (const [name, value] of Object.entries(init)) this.append(name, value);
      } else if (init != null) throw new TypeError("invalid headers");
    }
    /** 保留重复项，读取时再按 HTTP 常见语义合并。 */
    append(name, value) { this._items.push([headerName(name), headerValue(value)]); }
    /** 删除所有同名条目，防止旧值留在请求中。 */
    delete(name) { name = headerName(name); this._items = this._items.filter((item) => item[0] !== name); }
    /** 缺失头返回 null，区分空字符串值。 */
    get(name) { name = headerName(name); const values = this._items.filter((item) => item[0] === name).map((item) => item[1]); return values.length ? values.join(", ") : null; }
    /** 名称比较不受调用方大小写影响。 */
    has(name) { return this.get(name) !== null; }
    /** 新值替换全部旧值，避免多余认证头。 */
    set(name, value) { this.delete(name); this.append(name, value); }
    /** 迭代聚合同名头，返回拷贝防止绕过校验。 */
    *entries() { for (const name of [...new Set(this._items.map((item) => item[0]))].sort()) yield [name, this.get(name)]; }
    /** 键和值的顺序跟 entries 保持一致。 */
    *keys() { for (const [name] of this.entries()) yield name; }
    /** 头值以聚合形式暴露给调用方。 */
    *values() { for (const [, value] of this.entries()) yield value; }
    /** 默认遍历等同 entries。 */
    [Symbol.iterator]() { return this.entries(); }
    /** 回调遵循值、名称、当前对象的顺序。 */
    forEach(callback, thisArg) { for (const [name, value] of this.entries()) callback.call(thisArg, value, name, this); }
  }

  const responseKey = {};
  class Response {
    /** 网络层已限制响应大小；独立构造尚未实现时明确拒绝。 */
    constructor(result, key) {
      if (key !== responseKey) throw new TypeError("Response construction is not supported");
      this.status = result.status;
      this.statusText = result.statusText;
      this.ok = result.status >= 200 && result.status < 300;
      this.url = result.url;
      this.redirected = result.redirected;
      this.headers = new Headers(result.headers);
      this.bodyUsed = false;
      this._body = result.body;
    }
    /** 对每种读取方式共用消费标记，避免重复读取冒充独立流。 */
    _consume() {
      if (this.bodyUsed) throw new TypeError("response body already used");
      this.bodyUsed = true;
      return binary.fromBase64(this._body);
    }
    /** 内容按 UTF-8 解码，不使用操作系统默认编码。 */
    async text() { return binary.decodeUtf8(this._consume()); }
    /** JSON 错误继续传给调用方，不能当成空对象。 */
    async json() { return JSON.parse(await this.text()); }
    /** 返回独立 ArrayBuffer，避免泄漏其他缓冲区。 */
    async arrayBuffer() { return this._consume().buffer; }
  }

  /** 不支持流和 Request 对象，显式拒绝无法兑现的 fetch 选项。 */
  async function fetch(input, options = {}) {
    if (typeof input !== "string" && !(input instanceof URL)) throw new TypeError("fetch requires a URL string");
    if (!options || typeof options !== "object") throw new TypeError("invalid fetch options");
    for (const key of Object.keys(options)) {
      if (!["method", "headers", "body", "redirect", "signal"].includes(key)) throw new TypeError(`unsupported fetch option: ${key}`);
    }
    const headers = new Headers(options.headers);
    let body = null;
    if (options.body != null) {
      const content = typeof options.body === "string" ? new TextEncoder().encode(options.body) : binary.bytesOf(options.body);
      if (typeof options.body === "string" && !headers.has("content-type")) headers.set("content-type", "text/plain;charset=UTF-8");
      body = binary.toBase64(content);
    }
    const result = await operate({ kind: "fetch", url: String(input), method: String(options.method ?? "GET").toUpperCase(),
      headers: headers._items, body, redirect: options.redirect ?? "follow" }, options.signal);
    return new Response(result, responseKey);
  }

  const storage = Object.freeze({
    /** 键值序列化在宿主侧执行，持久化边界不依赖 JS 对象引用。 */
    get(key) { return operate({ kind: "storage", op: "get", key }); },
    /** 不可序列化的顶层值明确失败，避免误写为 null。 */
    set(key, value) { if (value === undefined) return Promise.reject(new TypeError("invalid storage value")); return operate({ kind: "storage", op: "set", key, value }); },
    /** 删除返回是否存在，调用方可以区分幂等重试。 */
    delete(key) { return operate({ kind: "storage", op: "delete", key }); },
    /** 文件始终以字节返回，调用方自主选择解码方式。 */
    async readFile(path) { return binary.fromBase64(await operate({ kind: "storage", op: "readFile", path })); },
    /** 字符串按 UTF-8 写入，二进制视图保留偏移及长度。 */
    writeFile(path, data) { const value = typeof data === "string" ? new TextEncoder().encode(data) : binary.bytesOf(data); return operate({ kind: "storage", op: "writeFile", path, data: binary.toBase64(value) }); },
    /** 删除文件与删除键使用同样的幂等返回约定。 */
    removeFile(path) { return operate({ kind: "storage", op: "removeFile", path }); },
    /** 空路径表示文件根目录，结果由宿主排序。 */
    list(path = "") { return operate({ kind: "storage", op: "list", path }); },
  });
  Object.defineProperty(globalThis, "__kerminalNativeComplete", { value: complete });
  Object.defineProperty(globalThis, "__kerminalStorage", { value: storage, configurable: true });
  Object.defineProperty(globalThis, "Headers", { value: Headers, configurable: true, writable: true });
  Object.defineProperty(globalThis, "Response", { value: Response, configurable: true, writable: true });
  Object.defineProperty(globalThis, "fetch", { value: fetch, configurable: true, writable: true });
})();
