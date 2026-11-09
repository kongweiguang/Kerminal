// @author kongweiguang

(() => {
  /** Web 字符串转换不能把 Symbol 悄悄当成可编码的文本。 */
  function text(value) {
    if (typeof value === "symbol") throw new TypeError("cannot encode a Symbol");
    return String(value);
  }

  /** 孤立代理项按 USVString 规则替换，避免写出无效 UTF-8。 */
  function scalar(character) {
    const value = character.codePointAt(0);
    return value >= 0xd800 && value <= 0xdfff ? 0xfffd : value;
  }

  /** 先计算长度，使 encode 恰好分配所需空间。 */
  function width(value) {
    return value < 0x80 ? 1 : value < 0x800 ? 2 : value < 0x10000 ? 3 : 4;
  }

  /** 只写完整字符，encodeInto 不会把多字节序列截在目标末尾。 */
  function writeScalar(value, destination, offset, size) {
    if (size === 1) destination[offset] = value;
    else if (size === 2) {
      destination[offset] = 0xc0 | (value >> 6);
      destination[offset + 1] = 0x80 | (value & 0x3f);
    } else if (size === 3) {
      destination[offset] = 0xe0 | (value >> 12);
      destination[offset + 1] = 0x80 | ((value >> 6) & 0x3f);
      destination[offset + 2] = 0x80 | (value & 0x3f);
    } else {
      destination[offset] = 0xf0 | (value >> 18);
      destination[offset + 1] = 0x80 | ((value >> 12) & 0x3f);
      destination[offset + 2] = 0x80 | ((value >> 6) & 0x3f);
      destination[offset + 3] = 0x80 | (value & 0x3f);
    }
  }

  class TextEncoder {
    /** TextEncoder 只支持 UTF-8，标签与浏览器一致。 */
    get encoding() { return "utf-8"; }

    /** 保留 UTF-16 读取计数和 UTF-8 写入计数，支持调用者分段编码。 */
    encodeInto(source, destination) {
      if (arguments.length < 2 || !(destination instanceof Uint8Array)) {
        throw new TypeError("encodeInto requires a string and Uint8Array");
      }
      let read = 0;
      let written = 0;
      for (const character of text(source)) {
        const value = scalar(character);
        const size = width(value);
        if (written + size > destination.length) break;
        writeScalar(value, destination, written, size);
        read += character.length;
        written += size;
      }
      return { read, written };
    }

    /** 分配精确长度后复用 encodeInto，避免维护第二套编码规则。 */
    encode(source = "") {
      const value = text(source);
      let size = 0;
      for (const character of value) size += width(scalar(character));
      const destination = new Uint8Array(size);
      this.encodeInto(value, destination);
      return destination;
    }
  }

  const signalKey = {};
  const signalStates = new WeakMap();
  const controllerSignals = new WeakMap();

  /** 中止使用独立错误名称，调用方可以识别取消而不依赖错误消息。 */
  function abortError() {
    const error = new Error("This operation was aborted");
    error.name = "AbortError";
    return error;
  }

  /** 中止状态在通知监听器前固定，重复中止不会重复触发副作用。 */
  function abortSignal(signal, reason) {
    const state = signalStates.get(signal);
    if (state.aborted) return;
    state.aborted = true;
    state.reason = reason === undefined ? abortError() : reason;
    const event = Object.freeze({ type: "abort", target: signal, currentTarget: signal });
    const listeners = [...state.listeners];
    for (const entry of listeners) {
      if (!state.listeners.includes(entry)) continue;
      if (entry.once) state.listeners.splice(state.listeners.indexOf(entry), 1);
      try {
        if (typeof entry.callback === "function") entry.callback.call(signal, event);
        else entry.callback.handleEvent(event);
      } catch (error) {
        console.error("abort listener failed", error);
      }
    }
    if (state.onabort) {
      try { state.onabort.call(signal, event); }
      catch (error) { console.error("abort listener failed", error); }
    }
  }

  class AbortSignal {
    /** 信号只能通过控制器创建，避免无状态对象伪装成有效取消来源。 */
    constructor(key) {
      if (key !== signalKey) throw new TypeError("Illegal constructor");
      signalStates.set(this, { aborted: false, reason: undefined, listeners: [], onabort: null });
    }

    /** 状态只由所属控制器更改，插件读取后不会影响其他监听者。 */
    get aborted() { return signalStates.get(this).aborted; }
    /** 保留调用方传入的原因，以便异步工作准确转交。 */
    get reason() { return signalStates.get(this).reason; }
    /** 单属性监听器与显式监听器共用同一中止事件。 */
    get onabort() { return signalStates.get(this).onabort; }
    /** 非函数值不当作回调，避免无效赋值中断取消流程。 */
    set onabort(callback) { signalStates.get(this).onabort = typeof callback === "function" ? callback : null; }

    /** 支持一次性监听，异步操作完成后也可显式解除绑定。 */
    addEventListener(type, callback, options = {}) {
      if (type !== "abort" || callback == null) return;
      if (typeof callback !== "function" && typeof callback.handleEvent !== "function") return;
      const capture = typeof options === "boolean" ? options : Boolean(options?.capture);
      const listeners = signalStates.get(this).listeners;
      if (!listeners.some((entry) => entry.callback === callback && entry.capture === capture)) {
        listeners.push({ callback, capture, once: Boolean(options?.once) });
      }
    }

    /** 按回调和 capture 撤销监听，避免任务结束后保留闭包。 */
    removeEventListener(type, callback, options = {}) {
      if (type !== "abort") return;
      const capture = typeof options === "boolean" ? options : Boolean(options?.capture);
      const listeners = signalStates.get(this).listeners;
      const index = listeners.findIndex((entry) => entry.callback === callback && entry.capture === capture);
      if (index >= 0) listeners.splice(index, 1);
    }

    /** 调用者可在开始副作用前显式检查已发生的取消。 */
    throwIfAborted() {
      if (this.aborted) throw this.reason;
    }
  }

  class AbortController {
    /** 每个控制器只拥有自己的信号，取消不能泄漏到其他请求。 */
    constructor() { controllerSignals.set(this, new AbortSignal(signalKey)); }
    /** 信号对象在控制器生命周期内保持稳定。 */
    get signal() { return controllerSignals.get(this); }
    /** 真实通知监听者停止工作，不把取消简化为丢弃回执。 */
    abort(reason) { abortSignal(controllerSignals.get(this), reason); }
  }

  Object.defineProperty(globalThis, "TextEncoder", { value: TextEncoder, configurable: true, writable: true });
  Object.defineProperty(globalThis, "AbortController", { value: AbortController, configurable: true, writable: true });
  Object.defineProperty(globalThis, "AbortSignal", { value: AbortSignal, configurable: true, writable: true });
})();
