//! 防止原生验收将后台标签或仅更新投影的值误判为真实输入成功。
//! @author kongweiguang

use super::*;

/// 测试构造的是观察协议样本，不替代原生验收使用的生产实体。
fn note_snapshot(model: &str, input: &str) -> Value {
    json!({
        "activeTabId":"active",
        "tabs":[{
            "tabId":"active", "contentId":CONTENT,
            "renderer":{
                "status":"Ready", "data":{"text":model},
                "fields":[{"binding":"text","kind":"textarea","value":input}]
            }
        }]
    })
}

/// 必须由真实输入实体确认模型变化，控件失步时不能让后续保存步骤继续。
#[test]
fn text_wait_rejects_stale_input_and_unready_renderer() {
    assert_eq!(active_text(&note_snapshot("new", "old")), None);
    assert_eq!(active_text(&note_snapshot("new", "new")), Some("new"));
    let mut snapshot = note_snapshot("new", "new");
    snapshot["tabs"][0]["renderer"]["status"] = json!("Opening");
    assert_eq!(active_text(&snapshot), None);
}

/// 隐藏标签中恰好匹配的值不能满足当前可见编辑器的等待条件。
#[test]
fn text_wait_rejects_background_or_different_content() {
    let mut snapshot = note_snapshot("new", "new");
    snapshot["activeTabId"] = json!("different");
    assert_eq!(active_text(&snapshot), None);
    snapshot["activeTabId"] = json!("active");
    snapshot["tabs"][0]["contentId"] = json!("other/editor");
    assert_eq!(active_text(&snapshot), None);
}

/// CLI 的隔离根与阶段必须明确，重复选项或相对路径不能静默选择最后一个值。
#[test]
fn options_reject_ambiguous_or_relative_output() {
    let parse = |values: &[&str]| parse_options(values.iter().map(OsString::from));
    assert!(parse(&["--phase", "prepare", "--phase", "verify"]).is_err());
    assert!(
        parse(&[
            "--phase",
            "prepare",
            "--output-dir",
            "relative",
            "--dev-plugin",
            "relative"
        ])
        .is_err()
    );
    assert!(parse(&["--unknown", "value"]).is_err());
    assert!(dimension(Some(OsString::from("799")), 1440, 800, 3840).is_err());
}
