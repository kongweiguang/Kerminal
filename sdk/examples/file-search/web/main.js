// @author kongweiguang
                                                                
                                                         

                                       
                            
const bridge = window.kerminal                                                          ;
const messages = {
  en: { title: "File search", root: "Folder", query: "File name contains", search: "Search", cancel: "Cancel", searching: "Searching", done: "Search complete", incomplete: "Some files may be missing because the search reached its limit or could not read a folder", cancelled: "Search cancelled", failed: "Search failed. Check the folder and try again.", unavailable: "The plugin connection is unavailable" },
  zh: { title: "文件搜索", root: "文件夹", query: "文件名包含", search: "搜索", cancel: "取消", searching: "正在搜索", done: "搜索完成", incomplete: "搜索达到上限或有文件夹无法读取，部分文件可能未显示", cancelled: "搜索已取消", failed: "搜索失败，请检查文件夹后重试。", unavailable: "插件连接不可用" },
};
let locale = bridge?.environment?.locale || navigator.language;
let active                             ;
let draftTimer                                           ;
const root = document.querySelector                  ("#root") ;
const query = document.querySelector                  ("#query") ;
const searchButton = document.querySelector                   ("#search") ;
const cancelButton = document.querySelector                   ("#cancel") ;
const status = document.querySelector             ("#status") ;
const results = document.querySelector                  ("#results") ;

/** 显示语言优先跟随宿主，未提供时使用浏览器语言并回退英文。 */
function text(key                          )         {
  return messages[locale.toLowerCase().startsWith("zh") ? "zh" : "en"][key];
}

/** 控件名称和窗口标题统一更新，语言变化不重置输入或结果。 */
function translate()       {
  document.documentElement.lang = locale;
  document.title = text("title");
  document.querySelector("#heading") .textContent = text("title");
  document.querySelector("#root-label") .textContent = text("root");
  document.querySelector("#query-label") .textContent = text("query");
  searchButton.textContent = text("search");
  cancelButton.textContent = text("cancel");
}

/** 进度只占用原有状态区域，取消与开始按钮的尺寸保持稳定。 */
function setBusy(busy         )       {
  searchButton.disabled = busy;
  cancelButton.disabled = !busy;
  root.disabled = busy;
  query.disabled = busy;
  if (busy) status.textContent = text("searching");
}

/** 结果均通过 textContent 写入，文件名不能成为 HTML 或脚本。 */
function renderRows(rows       )       {
  const fragment = document.createDocumentFragment();
  for (const row of rows) {
    const item = document.createElement("li");
    item.textContent = row.path;
    fragment.append(item);
  }
  results.replaceChildren(fragment);
}

/** 只应用协议允许的数据字段，失效或格式错误的补丁不会改变视图。 */
function applyPatch(value                       )       {
  if (!value || typeof value !== "object" || Array.isArray(value)) return;
  if (typeof value.root === "string" && document.activeElement !== root) root.value = value.root;
  if (typeof value.query === "string" && document.activeElement !== query) query.value = value.query;
  if (Array.isArray(value.results)) {
    const rows = value.results.filter((row)             =>
      !!row && typeof row === "object" && !Array.isArray(row) && typeof row.path === "string");
    renderRows(rows);
  }
  if (typeof value.busy === "boolean") setBusy(value.busy);
}

/** 草稿以实例动作为边界；输入时不清空旧结果，也不触发文件扫描。 */
function queueDraft()       {
  if (!bridge) return;
  if (draftTimer !== undefined) clearTimeout(draftTimer);
  draftTimer = setTimeout(() => {
    bridge.request("view.action", { action: "draft", data: { root: root.value, query: query.value } })
      .catch(() => { status.textContent = text("unavailable"); });
  }, 250);
}

/** 操作绑定创建时的控制器，旧搜索结束不能重置新搜索的按钮状态。 */
async function search(event       )                {
  event.preventDefault();
  if (!bridge || active || !root.value.trim() || !query.value.trim()) return;
  if (draftTimer !== undefined) clearTimeout(draftTimer);
  const controller = new AbortController();
  active = controller;
  setBusy(true);
  try {
    const result = await bridge.request("view.action", {
      action: "search", data: { root: root.value, query: query.value },
    }, { signal: controller.signal });
    if (result && typeof result === "object" && !Array.isArray(result)) {
      applyPatch(result.patch);
      const patch = result.patch;
      const incomplete = patch && typeof patch === "object" && !Array.isArray(patch)
        && (patch.limited === true || (typeof patch.skipped === "number" && patch.skipped > 0));
      status.textContent = text(incomplete ? "incomplete" : "done");
    }
  } catch (error) {
    status.textContent = controller.signal.aborted ? text("cancelled") : text("failed");
  } finally {
    if (active === controller) { active = undefined; setBusy(false); }
  }
}

translate();
if (bridge) {
  bridge.onRequest("view.update", (params) => { applyPatch(params.patch); return {}; });
  bridge.onRequest("view.open", (params) => {
    const opening = params                                        ;
    applyPatch(opening.data ?? opening.state);
    return {};
  });
  bridge.onRequest("environment.changed", (params) => {
    if (typeof params.locale === "string") { locale = params.locale; translate(); }
    return {};
  });
  document.querySelector("#search-form") .addEventListener("submit", search);
  root.addEventListener("input", queueDraft);
  query.addEventListener("input", queueDraft);
  cancelButton.addEventListener("click", () => active?.abort());
} else {
  searchButton.disabled = true;
  status.textContent = text("unavailable");
}
