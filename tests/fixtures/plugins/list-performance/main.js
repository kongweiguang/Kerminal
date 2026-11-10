// @author kongweiguang
const instances = new Set();
const total = 100000;
/** 真实脚本实例由宿主身份登记，不预分配十万行，避免fixture生成掩盖分页成本。 */
function open(params) { instances.add(params.instance); return { data: {} }; }
/** 严格匹配正式view.fetch范围，每次仅生成请求区间的稳定业务键。 */
function fetchPage(params) {
  if (!instances.has(params.instance) || params.node !== "records") throw new Error("stale list instance");
  const [start, end] = params.range;
  if (!Number.isInteger(start) || !Number.isInteger(end) || start < 0 || end <= start || end - start > 500) throw new Error("invalid range");
  const rows = [];
  for (let index = start; index < Math.min(end, total); index++) rows.push({ id: `record-${index}`, name: `Record ${index}` });
  return { total, rows };
}
/** 快照仅保存固定fixture版本；分页缓存始终归正式renderer管理。 */
function snapshot(params) { if (!instances.has(params.instance)) throw new Error("stale instance"); return { state_version: 1, state: {} }; }
/** 关闭沿正式协议撤销实例，不持有额外线程或计时器。 */
function close(params) { const value = snapshot(params); instances.delete(params.instance); return value; }
kerminal.onRequest("view.open", open);
kerminal.onRequest("view.fetch", fetchPage);
kerminal.onRequest("view.snapshot", snapshot);
kerminal.onRequest("view.close", close);
