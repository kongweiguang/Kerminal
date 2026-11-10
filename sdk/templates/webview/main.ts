// @author kongweiguang

declare const kerminal: { onRequest(method: string, handler: (params: any) => unknown): void };

const sessions = new Map<string, string>();

/** 状态按实例保存，多个标签同时打开时不会互相覆盖。 */
function openView(params: { instance: string; state?: { text?: unknown } }): { data: { text: string } } {
  const text = typeof params.state?.text === "string" ? params.state.text.slice(0, 10000) : "";
  sessions.set(params.instance, text);
  return { data: { text } };
}

/** 网页只能保存自己的实例，超限输入不会扩大恢复状态。 */
function action(params: { instance: string; action: string; data?: { text?: unknown } }): { patch: { text: string } } {
  if (!sessions.has(params.instance) || params.action !== "save" || typeof params.data?.text !== "string") {
    throw new Error("Invalid or stale save action");
  }
  const text = params.data.text.slice(0, 10000);
  sessions.set(params.instance, text);
  return { patch: { text } };
}

/** 只持久化插件已经接受的内容，浏览器未保存草稿仍由当前页面持有。 */
function snapshot(params: { instance: string }): { state_version: number; state: { text: string } } {
  const text = sessions.get(params.instance);
  if (text === undefined) throw new Error("Stale instance");
  return { state_version: 1, state: { text } };
}

/** 先取得最终快照再释放实例，关闭后的动作会被拒绝。 */
function closeView(params: { instance: string }): ReturnType<typeof snapshot> {
  const state = snapshot(params);
  sessions.delete(params.instance);
  return state;
}

kerminal.onRequest("view.open", openView);
kerminal.onRequest("view.action", action);
kerminal.onRequest("view.snapshot", snapshot);
kerminal.onRequest("view.close", closeView);
