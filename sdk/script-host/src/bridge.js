// @author kongweiguang

(() => {
  const emit = globalThis.__kerminalEmit;
  const schedule = globalThis.__kerminalSchedule;
  const cancelTimer = globalThis.__kerminalCancelTimer;
  const log = globalThis.__kerminalLog;
  const storage = globalThis.__kerminalStorage;
  const AbortController = globalThis.AbortController;
  delete globalThis.__kerminalEmit;
  delete globalThis.__kerminalSchedule;
  delete globalThis.__kerminalCancelTimer;
  delete globalThis.__kerminalLog;
  delete globalThis.__kerminalStorage;
  const handlers = new Map();
  const pending = new Map();
  const timers = new Map();
  let nextId = 0;
  const active = new Map();
  let environment = Object.freeze({ locale: "", theme: "" });

  /** 运行入口前注入宿主认可的环境，避免插件猜测主题与语言。 */
  function initializeEnvironment(serialized) {
    const params = JSON.parse(serialized);
    environment = Object.freeze({ locale: params.locale, theme: params.theme });
  }

  /** 回执与取消共用清理入口，避免监听器和请求槽位泄漏。 */
  function takePending(id) {
    const entry = pending.get(id);
    if (entry) {
      pending.delete(id);
      entry.cleanup();
    }
    return entry;
  }

  /** 限制未完成请求，并在取消时通知宿主停止可协作的工作。 */
  function request(method, params = {}, options = {}) {
    if (typeof method !== "string" || !method || !params || typeof params !== "object" || Array.isArray(params)) {
      return Promise.reject(new TypeError("invalid request"));
    }
    const signal = options?.signal;
    if (signal && (typeof signal.addEventListener !== "function" || typeof signal.removeEventListener !== "function")) {
      return Promise.reject(new TypeError("invalid abort signal"));
    }
    if (signal?.aborted) {
      return Promise.reject(Object.assign(new Error("cancelled"), { code: -32002 }));
    }
    if (pending.size >= 64) {
      return Promise.reject(Object.assign(new Error("overloaded"), { code: -32003 }));
    }
    const id = `p:${++nextId}`;
    return new Promise((resolve, reject) => {
      /** 只通知已发送的请求，失败时保留实际的通信错误。 */
      function onAbort() {
        const entry = takePending(id);
        if (!entry) return;
        try {
          if (entry.sent) emit(JSON.stringify({ jsonrpc: "2.0", method: "$/cancel", params: { id } }));
          entry.reject(Object.assign(new Error("cancelled"), { code: -32002 }));
        } catch (error) {
          entry.reject(error);
        }
      }
      const entry = { resolve, reject, sent: false, cleanup: () => signal?.removeEventListener("abort", onAbort) };
      pending.set(id, entry);
      try { signal?.addEventListener("abort", onAbort, { once: true }); }
      catch (error) { pending.delete(id); reject(error); return; }
      if (signal?.aborted) {
        onAbort();
        return;
      }
      try {
        entry.sent = true;
        emit(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
      } catch (error) {
        takePending(id)?.reject(error);
      }
    });
  }

  /** 一个方法只能注册一次，避免后注册的模块静默覆盖已有处理器。 */
  function onRequest(method, handler) {
    if (typeof method !== "string" || !method || typeof handler !== "function" || handlers.has(method)) {
      throw new TypeError("invalid or duplicate handler");
    }
    handlers.set(method, handler);
  }

  /** 可取消处理器与普通处理器共享注册表，避免同一方法被重复覆盖。 */
  function onCancellableRequest(method, handler) {
    onRequest(method, handler);
  }

  /** 把实例身份随补丁一起交给宿主，由宿主判断关闭和代次失效。 */
  function update(instance, patch, options) {
    return request("view.update", { instance, patch }, options);
  }

  /** 所有宿主能力都走双向 RPC，由宿主检查实例、权限和代次。 */
  function remoteCall(method) {
    return function callHost(params = {}) {
      return request(method, params);
    };
  }

  /** 控制台只写 stderr，不能把日志混入 stdout 协议帧。 */
  function writeLog(level, values) {
    const rendered = values.map((value) => {
      if (typeof value === "string") return value;
      try { return JSON.stringify(value) ?? String(value); }
      catch { return String(value); }
    });
    log(level, rendered.join(" "));
  }

  /** 保留常见控制台级别，便于宿主按等级收集日志。 */
  function consoleLog(...values) { writeLog("info", values); }
  /** 警告不进入 RPC 结果，避免把诊断文字误判为业务返回值。 */
  function consoleWarn(...values) { writeLog("warn", values); }
  /** 由宿主收集错误文本，插件仍需主动处理业务失败。 */
  function consoleError(...values) { writeLog("error", values); }

  /** 回调保留在当前 JS 运行时，原生队列只持有编号和到期时间。 */
  function registerTimer(callback, delay, repeat, args) {
    if (typeof callback !== "function") throw new TypeError("timer callback must be a function");
    const milliseconds = Math.trunc(Math.min(2147483647, Math.max(0, Number(delay) || 0)));
    const id = schedule(milliseconds, repeat);
    timers.set(id, { callback, repeat, args });
    return id;
  }

  /** 一次性计时器执行前移除，避免回调重入时看到过期编号。 */
  function fireTimer(id) {
    const timer = timers.get(id);
    if (!timer) return;
    if (!timer.repeat) timers.delete(id);
    timer.callback(...timer.args);
  }

  /** 键入参数由回调保存，确保单次和循环计时具有一致语义。 */
  function setTimeout(callback, delay = 0, ...args) {
    return registerTimer(callback, delay, false, args);
  }

  /** 循环计时使用同一编号，允许回调内取消自身。 */
  function setInterval(callback, delay = 0, ...args) {
    return registerTimer(callback, delay, true, args);
  }

  /** 同时删除 JS 回调和原生计时项，防止取消后仍触发。 */
  function clearTimer(id) {
    const timerId = Number(id) >>> 0;
    timers.delete(timerId);
    cancelTimer(timerId);
  }

  /** 回执保留远端错误码，调用者可以区分权限、取消和过载。 */
  function complete(message) {
    const entry = takePending(message.id);
    if (!entry) return;
    if (Object.hasOwn(message, "error")) {
      entry.reject(Object.assign(new Error(message.error?.message || "RPC error"), {
        code: message.error?.code,
        data: message.error?.data,
      }));
    } else {
      entry.resolve(message.result);
    }
  }

  /** 保留受协议约束的业务错误，其他异常统一当作内部失败处理。 */
  function responseError(error) {
    const code = error?.code;
    const permitted = Number.isInteger(code) && (code === -32002 || (code >= -32199 && code <= -32100));
    return { code: permitted ? code : -32603, message: String(error?.message || error) };
  }

  /** 异步处理器不会阻塞 stdin 读取，独立令牌防止旧请求抢占重用的 ID。 */
  function dispatch(serialized) {
    const message = JSON.parse(serialized);
    if (!Object.hasOwn(message, "method")) {
      complete(message);
      return;
    }
    if (message.method === "$/cancel" && typeof message.params?.id === "string") {
      const cancelled = message.params.id;
      const token = active.get(cancelled);
      if (active.delete(cancelled)) {
        token.controller.abort();
        emit(JSON.stringify({ jsonrpc: "2.0", id: cancelled, error: { code: -32002, message: "cancelled" } }));
      }
    }
    const handler = handlers.get(message.method);
    const hasId = Object.hasOwn(message, "id");
    if (!handler) {
      if (hasId) emit(JSON.stringify({ jsonrpc: "2.0", id: message.id, error: { code: -32601, message: "method not found" } }));
      return;
    }
    if (hasId && active.size >= 64) {
      emit(JSON.stringify({ jsonrpc: "2.0", id: message.id, error: { code: -32003, message: "overloaded" } }));
      return;
    }
    if (hasId && active.has(message.id)) {
      const prior = active.get(message.id);
      active.delete(message.id);
      prior.controller.abort();
      emit(JSON.stringify({ jsonrpc: "2.0", id: message.id, error: { code: -32600, message: "duplicate request id" } }));
      return;
    }
    const token = { controller: new AbortController() };
    if (hasId) active.set(message.id, token);
    let result;
    try {
      result = handler(message.params || {}, token.controller.signal);
    } catch (error) {
      if (!hasId) throw error;
      active.delete(message.id);
      emit(JSON.stringify({ jsonrpc: "2.0", id: message.id, error: responseError(error) }));
      return;
    }
    Promise.resolve(result).then(
      (result) => {
        if (hasId && active.get(message.id) === token) {
          active.delete(message.id);
          emit(JSON.stringify({ jsonrpc: "2.0", id: message.id, result: result === undefined ? null : result }));
        }
      },
      (error) => {
        if (hasId && active.get(message.id) === token) {
          active.delete(message.id);
          emit(JSON.stringify({ jsonrpc: "2.0", id: message.id, error: responseError(error) }));
        }
      },
    );
  }

  /** 原生层中断同步回调时撤销配额，迟到的异步回执不能再次返回。 */
  function abandon(id) {
    const token = active.get(id);
    active.delete(id);
    if (token) token.controller.abort();
  }

  /** QuickJS 无法定位中断的微任务所属请求；明确结束所有活动请求，避免永久悬挂。 */
  function interruptActive() {
    const entries = Array.from(active.entries());
    active.clear();
    for (const [id, token] of entries) {
      token.controller.abort();
      emit(JSON.stringify({ jsonrpc: "2.0", id, error: { code: -32603, message: "script execution interrupted" } }));
    }
  }

  Object.defineProperty(globalThis, "__kerminalDispatch", { value: dispatch });
  Object.defineProperty(globalThis, "__kerminalInit", { value: initializeEnvironment });
  Object.defineProperty(globalThis, "__kerminalAbandon", { value: abandon });
  Object.defineProperty(globalThis, "__kerminalInterruptActive", { value: interruptActive });
  Object.defineProperty(globalThis, "__kerminalTimer", { value: fireTimer });
  Object.defineProperties(globalThis, {
    setTimeout: { value: setTimeout },
    setInterval: { value: setInterval },
    clearTimeout: { value: clearTimer },
    clearInterval: { value: clearTimer },
    console: { value: Object.freeze({ log: consoleLog, info: consoleLog, warn: consoleWarn, error: consoleError }) },
  });
  Object.defineProperty(globalThis, "kerminal", {
    value: Object.freeze({
      onRequest,
      onCancellableRequest,
      request,
      storage,
      get environment() { return environment; },
      view: Object.freeze({ update }),
      ui: Object.freeze({ notify: remoteCall("ui.notify"), confirm: remoteCall("ui.confirm"),
        prompt: remoteCall("ui.prompt"), pickFile: remoteCall("ui.pickFile") }),
      settings: Object.freeze({ get: remoteCall("settings.get") }),
      state: Object.freeze({ get: remoteCall("state.get"), set: remoteCall("state.set") }),
      secrets: Object.freeze({ get: remoteCall("secrets.get"), set: remoteCall("secrets.set"),
        delete: remoteCall("secrets.delete") }),
      clipboard: Object.freeze({ read: remoteCall("clipboard.read"), write: remoteCall("clipboard.write") }),
      shell: Object.freeze({ openExternal: remoteCall("shell.openExternal") }),
      hosts: Object.freeze({ list: remoteCall("hosts.list") }),
      terminal: Object.freeze({ paste: remoteCall("terminal.paste") }),
      events: Object.freeze({ subscribe: remoteCall("events.subscribe") }),
    }),
  });
})();
