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

/** 一次只发送一个保存动作，异步失败保留输入以便用户重试。 */
async function save(event: Event): Promise<void> {
  event.preventDefault();
  if (!bridge || busy) return;
  busy = true;
  button.disabled = true;
  const text = input.value;
  try {
    await bridge.request("view.action", { action: "save", data: { text } });
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
