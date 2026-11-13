// Generated from sdk/ts/src/browser.ts with TypeScript 5.9.3; keep the SDK and this injected copy in sync.
// @author kongweiguang
class BridgeError extends Error {
    code;
    data;
    /** 保留协议错误码，让调用方能区分取消、过载和过期实例。 */
    constructor(code, message, data) {
        super(message);
        this.name = "BridgeError";
        this.code = code;
        this.data = data;
    }
}
/** 固定视图实例与代次，关闭或重载后的迟到消息不能落入另一个页面。 */
function createBrowserBridge(transport, identity, environment = { locale: "", theme: "" }) {
    if (!identity.view_instance || !Number.isSafeInteger(identity.generation) || identity.generation < 0) {
        throw new TypeError("invalid bridge identity");
    }
    const viewInstance = identity.view_instance;
    const generation = identity.generation;
    const initialEnvironment = Object.freeze({ locale: environment.locale, theme: environment.theme });
    const pending = new Map();
    const active = new Map();
    const handlers = new Map();
    const lifecycle = new AbortController();
    let nextId = 0;
    let closed = false;
    /** 每一帧独立附带身份，避免异步调用追随后来选中的视图。 */
    function send(message) {
        transport.send({ view_instance: viewInstance, generation, message });
    }
    /** 撤销监听和请求槽位必须同时完成，防止迟到回执再次结算。 */
    function takePending(id) {
        const entry = pending.get(id);
        if (entry) {
            pending.delete(id);
            entry.cleanup();
        }
        return entry;
    }
    /** 取消只发送协议通知，不自动重放可能有副作用的请求。 */
    function cancelRequest(id) {
        send({ jsonrpc: "2.0", method: "$/cancel", params: { id } });
    }
    /** 共享注册表按方法名去重，重复注册不能静默覆盖已生效的处理器。 */
    function register(method, handler) {
        if (closed || typeof method !== "string" || !method || typeof handler !== "function" || handlers.has(method)) {
            throw new TypeError("invalid or duplicate handler");
        }
        handlers.set(method, handler);
    }
    /** 无回执通知也保留错误出口，插件异常不能伪装成处理成功。 */
    function report(error) {
        if (transport.onError)
            transport.onError(error);
        else
            console.error(error);
    }
    /** 只允许宿主定义的业务错误范围，防止页面伪造取消或过载等协议错误。 */
    function responseError(error) {
        const record = error && typeof error === "object" ? error : undefined;
        const code = record?.code;
        const permitted = typeof code === "number" && Number.isInteger(code)
            && (code === -32002 || (code >= -32199 && code <= -32100));
        return { code: permitted ? code : -32603, message: String(record?.message || error) };
    }
    /** 响应只匹配本代仍在途的请求，并拒绝缺失或冲突的结果。 */
    function receiveResponse(message) {
        if (typeof message.id !== "string" || !message.id.startsWith("p:"))
            return;
        const entry = takePending(message.id);
        if (!entry)
            return;
        const hasResult = Object.hasOwn(message, "result");
        const hasError = Object.hasOwn(message, "error");
        if (hasResult === hasError) {
            entry.reject(new BridgeError(-32600, "invalid RPC response"));
        }
        else if (hasError) {
            const error = message.error;
            if (!error || typeof error !== "object" || Array.isArray(error)
                || typeof error.code !== "number"
                || typeof error.message !== "string") {
                entry.reject(new BridgeError(-32600, "invalid RPC error"));
            }
            else {
                const rpcError = error;
                entry.reject(new BridgeError(rpcError.code, rpcError.message, rpcError.data));
            }
        }
        else {
            entry.resolve(message.result);
        }
    }
    /** 取消入站动作时中止对应信号，已经失效的异步结果不会发回宿主。 */
    function receiveRequest(message) {
        if (typeof message.method !== "string" || !message.method)
            return;
        const hasId = Object.hasOwn(message, "id");
        const id = message.id;
        if (hasId && (typeof id !== "string" || !id.startsWith("h:")))
            return;
        const params = message.params ?? {};
        if (!params || typeof params !== "object" || Array.isArray(params)) {
            if (typeof id === "string")
                send({ jsonrpc: "2.0", id, error: { code: -32602, message: "invalid params" } });
            return;
        }
        if (message.method === "$/cancel" && !hasId) {
            const target = params.id;
            if (typeof target === "string") {
                const controller = active.get(target);
                active.delete(target);
                controller?.abort();
            }
            return;
        }
        const handler = handlers.get(message.method);
        if (!handler) {
            if (typeof id === "string")
                send({ jsonrpc: "2.0", id, error: { code: -32601, message: "method not found" } });
            return;
        }
        if (typeof id !== "string") {
            Promise.resolve().then(() => {
                if (!closed)
                    return handler(params, lifecycle.signal);
            }).catch(report);
            return;
        }
        if (active.has(id) || active.size >= 64) {
            send({ jsonrpc: "2.0", id, error: { code: active.has(id) ? -32600 : -32003, message: "request unavailable" } });
            return;
        }
        const controller = new AbortController();
        active.set(id, controller);
        Promise.resolve().then(() => {
            if (!closed && active.get(id) === controller)
                return handler(params, controller.signal);
        }).then((result) => {
            if (!closed && active.get(id) === controller) {
                active.delete(id);
                send({ jsonrpc: "2.0", id, result: (result === undefined ? null : result) });
            }
        }, (error) => {
            if (!closed && active.get(id) === controller) {
                active.delete(id);
                send({ jsonrpc: "2.0", id, error: responseError(error) });
            }
        }).catch(report);
    }
    /** 外层身份先于任何 RPC 分派检查；错误页面的消息不会消费当前请求。 */
    function receive(envelope) {
        if (closed || !envelope || envelope.view_instance !== viewInstance || envelope.generation !== generation)
            return;
        const message = envelope.message;
        if (!message || typeof message !== "object" || Array.isArray(message))
            return;
        const record = message;
        if (record.jsonrpc !== "2.0")
            return;
        if (Object.hasOwn(record, "method"))
            receiveRequest(record);
        else
            receiveResponse(record);
    }
    const unsubscribe = transport.subscribe(receive);
    /** 最多保留 64 个请求；取消、发送失败和正常回执都释放同一槽位。 */
    function request(method, params = {}, options = {}) {
        if (closed)
            return Promise.reject(new BridgeError(-32004, "stale view"));
        if (typeof method !== "string" || !method || !params || typeof params !== "object" || Array.isArray(params)) {
            return Promise.reject(new TypeError("invalid request"));
        }
        if (method === "view.update" && params.instance !== viewInstance) {
            return Promise.reject(new BridgeError(-32004, "view instance mismatch"));
        }
        if (options.signal?.aborted)
            return Promise.reject(new BridgeError(-32002, "cancelled"));
        if (pending.size >= 64 || nextId >= Number.MAX_SAFE_INTEGER) {
            return Promise.reject(new BridgeError(-32003, "overloaded"));
        }
        const id = `p:${++nextId}`;
        return new Promise((resolve, reject) => {
            /** 已发出的请求才需要通知宿主；发送失败必须反馈给调用者。 */
            function onAbort() {
                const entry = takePending(id);
                if (!entry)
                    return;
                try {
                    if (entry.sent)
                        cancelRequest(id);
                    entry.reject(new BridgeError(-32002, "cancelled"));
                }
                catch (error) {
                    entry.reject(error);
                }
            }
            const entry = {
                resolve: (value) => resolve(value),
                reject,
                cleanup: () => options.signal?.removeEventListener("abort", onAbort),
                sent: false,
            };
            pending.set(id, entry);
            options.signal?.addEventListener("abort", onAbort, { once: true });
            if (options.signal?.aborted) {
                onAbort();
                return;
            }
            try {
                entry.sent = true;
                const message = { jsonrpc: "2.0", id, method, params };
                send(message);
            }
            catch (error) {
                takePending(id)?.reject(error);
            }
        });
    }
    /** 关闭先使本代失效，再撤销订阅和所有等待中的操作。 */
    function close() {
        if (closed)
            return;
        closed = true;
        let failure;
        try {
            unsubscribe();
        }
        catch (error) {
            failure = error;
        }
        lifecycle.abort();
        for (const controller of active.values())
            controller.abort();
        active.clear();
        handlers.clear();
        for (const id of pending.keys()) {
            const entry = takePending(id);
            if (!entry)
                continue;
            try {
                if (entry.sent)
                    cancelRequest(id);
            }
            catch (error) {
                failure ??= error;
            }
            entry.reject(new BridgeError(-32004, "stale view"));
        }
        if (failure !== undefined)
            throw failure;
    }
    /** 每个便捷入口复用同一个请求门禁，避免绕过视图身份与关闭清理。 */
    function remoteCall(method) {
        return (params = {}) => request(method, params);
    }
    return {
        environment: initialEnvironment,
        onRequest: register,
        onCancellableRequest: register,
        request,
        view: Object.freeze({
            /** 只允许更新当前 WebView 实例，关闭或重载后的迟到请求不能写入其他视图。 */
            update: (instance, patch, options) => request("view.update", { instance, patch }, options),
        }),
        ui: Object.freeze({
            notify: remoteCall("ui.notify"), confirm: remoteCall("ui.confirm"),
            prompt: remoteCall("ui.prompt"), pickFile: remoteCall("ui.pickFile"),
        }),
        settings: Object.freeze({ get: remoteCall("settings.get") }),
        state: Object.freeze({ get: remoteCall("state.get"), set: remoteCall("state.set") }),
        secrets: Object.freeze({
            get: remoteCall("secrets.get"), set: remoteCall("secrets.set"), delete: remoteCall("secrets.delete"),
        }),
        clipboard: Object.freeze({ read: remoteCall("clipboard.read"), write: remoteCall("clipboard.write") }),
        shell: Object.freeze({ openExternal: remoteCall("shell.openExternal") }),
        hosts: Object.freeze({ list: remoteCall("hosts.list") }),
        terminal: Object.freeze({ paste: remoteCall("terminal.paste") }),
        events: Object.freeze({ subscribe: remoteCall("events.subscribe") }),
        close,
    };
}
