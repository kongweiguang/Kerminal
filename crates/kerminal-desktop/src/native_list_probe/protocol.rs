//! P0.4 夹具进程的最小 JSON-RPC 处理，不依赖图形或插件运行时。
//!
//! @author kongweiguang

use serde_json::{Value, json};

const ROW_COUNT: usize = 100_000;
const MAX_PAGE_ROWS: usize = 512;

/// 严格限制方法和半开区间，使错误请求不能伪装成有效的大集合页面。
pub fn handle(request: &Value) -> Result<Value, &'static str> {
    if request["jsonrpc"] != "2.0" || request["method"] != "view.fetch" {
        return Err("unsupported JSON-RPC request");
    }
    let id = request["id"].as_u64().ok_or("invalid request id")?;
    let parameters = &request["params"];
    if parameters["view"] != "native-probe"
        || parameters["instance"] != "native-probe-instance"
        || parameters["node"] != "rows"
    {
        return Err("invalid view identity");
    }
    let range = parameters["range"].as_array().ok_or("invalid range")?;
    if range.len() != 2 {
        return Err("invalid range");
    }
    let start = usize::try_from(range[0].as_u64().ok_or("invalid range start")?)
        .map_err(|_| "invalid range start")?;
    let end = usize::try_from(range[1].as_u64().ok_or("invalid range end")?)
        .map_err(|_| "invalid range end")?;
    if start >= end || end > ROW_COUNT || end - start > MAX_PAGE_ROWS {
        return Err("range outside bounded collection");
    }
    let rows = (start..end)
        .map(|index| {
            json!({
                "index": index,
                "title": format!("Item {index:06}"),
                "subtitle": format!("/probe/record/{index:06}"),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {"range": [start, end], "rows": rows},
    }))
}
