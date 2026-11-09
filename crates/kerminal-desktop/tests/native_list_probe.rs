//! RPC 夹具边界测试，不打开桌面窗口。
//!
//! @author kongweiguang

#[path = "../src/native_list_probe/protocol.rs"]
mod protocol;

use serde_json::{Value, json};

/// 请求固定视图身份，避免测试通过错误的协议路径。
fn request(start: usize, end: usize) -> Value {
    json!({"jsonrpc":"2.0", "id":7, "method":"view.fetch", "params": {
        "view":"native-probe", "instance":"native-probe-instance", "node":"rows", "range":[start,end]
    }})
}

/// 末页仍使用半开区间，不能越过十万行或预生成整份数据。
#[test]
fn fetches_only_the_requested_final_range() {
    let response = protocol::handle(&request(99_998, 100_000)).unwrap();
    assert_eq!(response["id"], 7);
    assert_eq!(response["result"]["range"], json!([99_998, 100_000]));
    assert_eq!(response["result"]["rows"].as_array().unwrap().len(), 2);
    assert_eq!(response["result"]["rows"][1]["index"], 99_999);
}

/// 空、过宽、越界和目标身份错误都不得产生成功回执。
#[test]
fn rejects_out_of_bounds_and_wrong_view() {
    assert!(protocol::handle(&request(2, 2)).is_err());
    assert!(protocol::handle(&request(0, 513)).is_err());
    assert!(protocol::handle(&request(99_999, 100_001)).is_err());
    let mut wrong = request(0, 2);
    wrong["params"]["node"] = json!("other");
    assert!(protocol::handle(&wrong).is_err());
}
