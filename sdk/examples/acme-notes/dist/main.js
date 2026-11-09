// @author kongweiguang
// 集成依赖：脚本宿主需提供受 network:example.com 限制的 fetch；宿主快照需带当前脏字段。

                                                                                                      

                                                     
                                              
const host = kerminal                                                    ;
const sessions = new Map                 ();
const EXAMPLE_VERSION = "1.0.0";
const messages = {
  en: { available: "example.com is reachable", unavailable: "Could not reach example.com", saved: "Note saved", copied: "Note copied" },
  zh: { available: "example.com 可访问", unavailable: "无法访问 example.com", saved: "笔记已保存", copied: "笔记已复制" },
};

/** 文案在运行时依据握手语言选择，非中文语言统一回退英文。 */
function text(key                          )         {
  return messages[host.environment.locale.toLowerCase().startsWith("zh") ? "zh" : "en"][key];
}

/** 未知状态只作为空对象处理，避免恢复旧数据时触发类型异常。 */
function object(value                       )                            {
  return value && typeof value === "object" && !Array.isArray(value) ? value : {};
}

/** 限制单条内容，避免工作区状态或插件状态超过协议上限。 */
function noteText(value                       )         {
  return typeof value === "string" ? value.slice(0, 32_000) : "";
}

/** 只在编辑器打开时读取已保存内容；宿主带回的实例草稿优先。 */
async function openView(params                )                                               {
  const saved = object(await host.request("state.get", {}));
  const savedState = object(saved.state ?? saved);
  const restored = object(params.state);
  const value = typeof restored.text === "string" ? noteText(restored.text) : noteText(savedState.text);
  sessions.set(params.instance, { view: params.view, text: value });
  if (params.view === "notes.panel") {
    const reply = object(await host.request("settings.get", {}));
    const settings = object(reply.settings ?? reply);
    return { data: { saved: noteText(savedState.text), showPreview: settings.showPreview !== false } };
  }
  return { data: { text: value } };
}

/** 网络访问固定在清单白名单内；失败保留业务状态并返回可读文案。 */
async function checkService()                  {
  try {
    const response = await fetch("https://example.com/", { method: "HEAD", redirect: "error" });
    return text(response.ok ? "available" : "unavailable");
  } catch {
    return text("unavailable");
  }
}

/** 保存只使用发起动作时的实例快照，不覆盖其他编辑器的未保存输入。 */
async function act(params                  )                                                                 {
  const session = sessions.get(params.instance);
  if (!session) throw new Error("stale instance");
  const data = object(params.data);
  if (typeof data.text === "string") session.text = noteText(data.text);
  if (params.action === "save") {
    await host.request("state.set", { state: { text: session.text } });
    for (const [instance, target] of sessions) {
      if (target.view === "notes.panel") await host.view.update(instance, { saved: session.text });
    }
    return { toast: text("saved") };
  }
  if (params.action === "check") return { toast: await checkService() };
  if (params.action === "copy" && ["1.1.0"].includes(EXAMPLE_VERSION)) {
    await host.request("clipboard.write", { text: session.text });
    return { toast: text("copied") };
  }
  throw new Error("unknown action");
}

/** 设置变动只影响侧栏预览，不改写编辑器草稿。 */
async function settingsChanged(params                         )                {
  const settings = object(params.settings);
  for (const [instance, session] of sessions) {
    if (session.view === "notes.panel") await host.view.update(instance, { showPreview: settings.showPreview !== false });
  }
}

/** 快照优先使用宿主附带的当前数据；旧宿主只能恢复最近一次动作。 */
function snapshot(params                                        )                                                     {
  const session = sessions.get(params.instance);
  if (!session) throw new Error("stale instance");
  const data = object(params.data);
  if (typeof data.text === "string") session.text = noteText(data.text);
  return { state_version: 1, state: { text: session.text } };
}

/** 关闭时先取得快照再清理实例，不能让其他标签的状态串入当前标签。 */
function closeView(params                                        )                                                     {
  const state = snapshot(params);
  sessions.delete(params.instance);
  return state;
}

/** 命令复用同一网络检查，结果由宿主通知而不是更改当前编辑草稿。 */
async function runCommand()                                 {
  await host.request("ui.notify", { level: "info", message: await checkService() });
  return {};
}

host.onRequest("view.open", openView);
host.onRequest("view.action", act);
host.onRequest("view.snapshot", snapshot);
host.onRequest("view.close", closeView);
host.onRequest("command.run", runCommand);
host.onRequest("settings.changed", settingsChanged);
