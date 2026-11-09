// @author kongweiguang

(() => {
  const native = globalThis.__kerminalWebSync;
  const encode = globalThis.__kerminalWebEncode;
  const decode = globalThis.__kerminalWebDecode;
  delete globalThis.__kerminalWebSync;
  delete globalThis.__kerminalWebEncode;
  delete globalThis.__kerminalWebDecode;

  /** 原生调用只交换 JSON，避免 QuickJS 对象跨线程或跨运行时使用。 */
  function call(request) { return JSON.parse(native(JSON.stringify(request))); }

  /** 二进制 API 只接受 ArrayBuffer 或视图，保留视图的偏移与长度。 */
  function bytesOf(value) {
    if (value instanceof ArrayBuffer) return new Uint8Array(value);
    if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
    throw new TypeError("expected an ArrayBuffer or view");
  }

  /** 原生编码有固定上限，避免大文件转码占满脚本执行时间。 */
  function toBase64(value) { return encode(bytesOf(value)); }

  /** 解码数组由运行时分配，仍计入脚本堆预算。 */
  function fromBase64(value) {
    if (typeof value !== "string") throw new TypeError("invalid base64");
    return decode(value);
  }

  /** 共享 UTF-8 解码实现，网络与文件内容不经过本机默认代码页。 */
  function decodeUtf8(value, fatal = false) {
    return call({ op: "decodeUtf8", data: toBase64(value), fatal });
  }

  class TextDecoder {
    /** 显式拒绝未实现的编码，不能悄悄按 UTF-8 解读其他代码页。 */
    constructor(label = "utf-8", options = {}) {
      if (!["utf-8", "utf8", "unicode-1-1-utf-8"].includes(String(label).toLowerCase())) throw new RangeError("unsupported encoding");
      this.encoding = "utf-8";
      this.fatal = Boolean(options.fatal);
      this.ignoreBOM = Boolean(options.ignoreBOM);
    }
    /** 独立解码拒绝流模式，避免截断的多字节字符被伪装成成功。 */
    decode(input = new Uint8Array(), options = {}) {
      if (options.stream) throw new TypeError("streaming decode is not supported");
      const text = decodeUtf8(input, this.fatal);
      return !this.ignoreBOM && text.startsWith("\ufeff") ? text.slice(1) : text;
    }
  }

  class URLSearchParams {
    /** 内部保留顺序与重复键，序列化由同版本 URL 实现执行。 */
    constructor(init = "") {
      this._change = null;
      if (init instanceof URLSearchParams) this._pairs = init._pairs.map((pair) => [...pair]);
      else if (typeof init === "string") this._pairs = call({ op: "query.parse", query: init });
      else if (init && typeof init[Symbol.iterator] === "function") this._pairs = Array.from(init, (pair) => {
        const values = Array.from(pair);
        if (values.length !== 2) throw new TypeError("URL parameter requires two values");
        return [String(values[0]), String(values[1])];
      });
      else if (init && typeof init === "object") this._pairs = Object.entries(init).map(([key, value]) => [key, String(value)]);
      else this._pairs = call({ op: "query.parse", query: String(init) });
    }
    /** URL 持有同一参数对象，修改查询字符串时同步其内容。 */
    _replace(search) { this._pairs = call({ op: "query.parse", query: search }); }
    /** 参数改变后通知所属 URL，独立参数对象无需回调。 */
    _updated() { if (this._change) this._change(this.toString()); }
    /** 重复键按加入顺序保留。 */
    append(name, value) { this._pairs.push([String(name), String(value)]); this._updated(); }
    /** 按可选值过滤，未给值时删除该键的全部条目。 */
    delete(name, value) { name = String(name); this._pairs = this._pairs.filter((pair) => pair[0] !== name || (value !== undefined && pair[1] !== String(value))); this._updated(); }
    /** 缺失值返回 null，与空字符串区分。 */
    get(name) { return this._pairs.find((pair) => pair[0] === String(name))?.[1] ?? null; }
    /** 所有重复值保持原始顺序。 */
    getAll(name) { return this._pairs.filter((pair) => pair[0] === String(name)).map((pair) => pair[1]); }
    /** 可按键或键值组合检查。 */
    has(name, value) { return this._pairs.some((pair) => pair[0] === String(name) && (value === undefined || pair[1] === String(value))); }
    /** 替换首项并移除后续同名项，不打乱其他键顺序。 */
    set(name, value) { name = String(name); value = String(value); const index = this._pairs.findIndex((pair) => pair[0] === name); if (index < 0) this._pairs.push([name, value]); else { this._pairs[index][1] = value; this._pairs = this._pairs.filter((pair, i) => pair[0] !== name || i === index); } this._updated(); }
    /** 稳定排序只比较键，重复键的相对顺序不变。 */
    sort() { this._pairs.sort((a, b) => a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0); this._updated(); }
    /** 参数数量包括重复键。 */
    get size() { return this._pairs.length; }
    /** 返回拷贝的条目，避免修改数组绕过 URL 同步。 */
    *entries() { for (const pair of this._pairs) yield [...pair]; }
    /** 迭代键时保留重复项。 */
    *keys() { for (const pair of this._pairs) yield pair[0]; }
    /** 迭代值时保留重复项。 */
    *values() { for (const pair of this._pairs) yield pair[1]; }
    /** 默认迭代协议与 entries 一致。 */
    [Symbol.iterator]() { return this.entries(); }
    /** 回调顺序遵循查询字符串顺序。 */
    forEach(callback, thisArg) { for (const [key, value] of this._pairs) callback.call(thisArg, value, key, this); }
    /** 表单编码使用 URL crate，统一处理空格与非 ASCII 字符。 */
    toString() { return call({ op: "query.format", pairs: this._pairs }); }
  }

  class URL {
    /** 基址解析和规范化交给原生 URL 解析器。 */
    constructor(input, base) {
      this._parts = call({ op: "url.parse", input: String(input), ...(base === undefined ? {} : { base: String(base) }) });
      this._params = new URLSearchParams(this._parts.search);
      this._params._change = (value) => { this._parts = call({ op: "url.set", href: this.href, field: "search", value }); };
    }
    /** 所有 setter 都刷新同一个 searchParams 对象。 */
    _set(field, value) { this._parts = call({ op: "url.set", href: this.href, field, value: String(value) }); this._params._replace(this._parts.search); }
    /** 查询参数对象保持身份稳定。 */
    get searchParams() { return this._params; }
    /** 字符串化返回规范化绝对地址。 */
    toString() { return this.href; }
    /** JSON 序列化与普通字符串化使用同一地址。 */
    toJSON() { return this.href; }
    /** 允许调用方无异常地预检地址。 */
    static canParse(input, base) { try { new URL(input, base); return true; } catch { return false; } }
  }
  // 属性统一代理同一个原生结果，防止某个 setter 绕过解析与规范化。
  for (const field of ["href", "protocol", "username", "password", "host", "hostname", "port", "pathname", "search", "hash"]) {
    Object.defineProperty(URL.prototype, field, { get() { return this._parts[field]; }, set(value) { this._set(field, value); }, enumerable: true });
  }
  Object.defineProperty(URL.prototype, "origin", { get() { return this._parts.origin; }, enumerable: true });

  /** 随机 UUID 由操作系统熵源生成，不能使用 Math.random。 */
  function randomUUID() { return call({ op: "uuid" }); }
  /** 摘要结果为独立 ArrayBuffer，未知算法明确拒绝。 */
  async function digest(algorithm, data) {
    const name = typeof algorithm === "string" ? algorithm : algorithm?.name;
    const result = fromBase64(call({ op: "digest", algorithm: String(name), data: toBase64(data) }));
    return result.buffer;
  }

  Object.defineProperty(globalThis, "__kerminalBinary", { value: Object.freeze({ bytesOf, toBase64, fromBase64, decodeUtf8 }), configurable: true });
  Object.defineProperty(globalThis, "TextDecoder", { value: TextDecoder, configurable: true, writable: true });
  Object.defineProperty(globalThis, "URL", { value: URL, configurable: true, writable: true });
  Object.defineProperty(globalThis, "URLSearchParams", { value: URLSearchParams, configurable: true, writable: true });
  Object.defineProperty(globalThis, "crypto", { value: Object.freeze({ randomUUID, subtle: Object.freeze({ digest }) }), configurable: true });
})();
