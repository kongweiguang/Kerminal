// @author kongweiguang

type Data = Record<string, unknown>;
type Bridge = {
  request(method: string, params: Data): Promise<unknown>;
  onRequest(method: string, handler: (params: Data) => unknown): void;
};
const bridge = (window as Window & { kerminal?: Bridge }).kerminal;
const input = document.querySelector<HTMLTextAreaElement>("#note")!;
const button = document.querySelector<HTMLButtonElement>("#save")!;
const statusLabel = document.querySelector<HTMLElement>("#status")!;
let busy = false;
let dirty = false;

/** 宿主恢复不能覆盖网页中的未保存输入，保存确认后才释放草稿保护。 */
function apply(value: unknown): void {
  if (!dirty && value && typeof value === "object" && "text" in value && typeof value.text === "string") {
    input.value = value.text;
  }
}

/** 只有声明式插件明确返回“方法不支持”时才回退；业务失败、崩溃和拒绝必须原样上抛。 */
function methodUnsupported(error: unknown): boolean {
  return !!error && typeof error === "object" && (error as { code?: unknown }).code === -32601;
}

/** 有后端时优先让 Sidecar 保存并更新实例快照；仅在方法确实缺失时回退到宿主状态 API。 */
async function persist(text: string): Promise<void> {
  try {
    await bridge!.request("view.action", { action: "save", data: { text } });
  } catch (error) {
    if (!methodUnsupported(error)) throw error;
    // 声明式插件没有进程处理 view.action；state.set 仍是宿主提供的调用。
    await bridge!.request("state.set", { state: { text } });
  }
}

/** 保存优先复用 Sidecar，缺失后端时仍能持久化，页面不是只能纯展示。 */
async function save(event: Event): Promise<void> {
  event.preventDefault();
  if (!bridge || busy) return;
  busy = true;
  button.disabled = true;
  const text = input.value;
  try {
    await persist(text);
    // 通知是宿主提供的调用；无后端页面同样可用。
    await bridge.request("ui.notify", { level: "info", message: "Note saved" });
    dirty = input.value !== text;
    statusLabel.textContent = "Saved";
  } catch {
    statusLabel.textContent = "Could not save. Try again.";
  } finally {
    busy = false;
    button.disabled = false;
  }
}

/** 实例生命周期由宿主注入的桥接分发，页面不创建第二条 RPC 连接。 */
function connect(): void {
  if (!bridge) {
    button.disabled = true;
    statusLabel.textContent = "Plugin connection unavailable";
    return;
  }
  bridge.onRequest("view.open", (params) => { apply(params.data ?? params.state); return {}; });
  bridge.onRequest("view.update", (params) => { apply(params.patch); return {}; });
  input.addEventListener("input", () => { dirty = true; });
  document.querySelector("#editor")!.addEventListener("submit", save);
}

connect();