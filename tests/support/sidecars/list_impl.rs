// 十万行原生列表夹具的 Sidecar 业务实现。
// @author kongweiguang
//
// 分页始终按宿主请求范围即时生成，不预分配整表；这样验收看到的是正式 RPC
// 往返成本，而不是夹具提前持有十万行掩盖了列表加载路径。

use std::{
    collections::HashSet,
    sync::{Mutex, OnceLock},
};

use serde_json::{Value, json};

pub(super) const PLUGIN_ID: &str = "test.list-performance";
pub(super) const PLUGIN_VERSION: &str = match option_env!("KERMINAL_PLUGIN_VERSION") {
    Some(version) => version,
    None => "1.0.0",
};
const TOTAL_ROWS: usize = 100_000;
const MAX_RANGE: usize = 500;

static INSTANCES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// 只接受协议声明的方法；未知方法明确失败，避免静态视图与宿主参数被静默忽略。
pub(super) fn dispatch(_ctx: &Ctx, method: &str, params: Value) -> RpcResult {
    match method {
        "view.open" => open(&params),
        "view.fetch" => fetch(&params),
        "view.snapshot" => snapshot(&params),
        "view.close" => close(&params),
        _ => Err((-32601, "method not found".into())),
    }
}

/// 列表夹具不响应通知；保留入口使通用协议无需特殊分支。
pub(super) fn on_notification(_ctx: &Ctx, _method: &str, _params: Value) -> Result<(), RpcError> {
    Ok(())
}

/// 列表夹具没有后台线程，退出时无需额外回收。
pub(super) fn on_shutdown(_shared: &Shared) {}

/// 打开只为真实实例登记身份，初始数据保持空对象由 renderer 自行请求分页。
fn open(params: &Value) -> RpcResult {
    let instance = string_param(params, "instance")?;
    instances()
        .lock()
        .map_err(|_| internal("instances lock unavailable"))?
        .insert(instance);
    Ok(json!({"data":{}}))
}

/// 分页严格匹配正式 `view.fetch` 范围，单次最多五百行并截断到总行数。
fn fetch(params: &Value) -> RpcResult {
    let instance = string_param(params, "instance")?;
    if !instances()
        .lock()
        .map_err(|_| internal("instances lock unavailable"))?
        .contains(&instance)
    {
        return Err((-32004, "stale list instance".into()));
    }
    if params.get("node").and_then(Value::as_str) != Some("records") {
        return Err((-32602, "node required".into()));
    }
    let range = params
        .get("range")
        .and_then(Value::as_array)
        .ok_or((-32602, "range required".into()))?;
    if range.len() != 2 {
        return Err((-32602, "invalid range".into()));
    }
    let start = range[0]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or((-32602, "invalid range".into()))?;
    let end = range[1]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or((-32602, "invalid range".into()))?;
    if end <= start || end - start > MAX_RANGE {
        return Err((-32602, "invalid range".into()));
    }
    let mut rows = Vec::new();
    for index in start..end.min(TOTAL_ROWS) {
        rows.push(json!({"id":format!("record-{index}"),"name":format!("Record {index}")}));
    }
    Ok(json!({"total":TOTAL_ROWS,"rows":rows}))
}

/// 快照只保存固定夹具版本；分页缓存归正式 renderer 管理，插件不持有滚动状态。
fn snapshot(params: &Value) -> RpcResult {
    let instance = string_param(params, "instance")?;
    if !instances()
        .lock()
        .map_err(|_| internal("instances lock unavailable"))?
        .contains(&instance)
    {
        return Err((-32004, "stale list instance".into()));
    }
    Ok(json!({"state_version":1,"state":{}}))
}

/// 关闭沿正式协议撤销实例；重复关闭返回 stale 而不是伪成功。
fn close(params: &Value) -> RpcResult {
    let state = snapshot(params)?;
    let instance = string_param(params, "instance")?;
    instances()
        .lock()
        .map_err(|_| internal("instances lock unavailable"))?
        .remove(&instance);
    Ok(state)
}

fn instances() -> &'static Mutex<HashSet<String>> {
    INSTANCES.get_or_init(|| Mutex::new(HashSet::new()))
}