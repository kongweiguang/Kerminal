//! 原生草稿恢复必须与插件状态隔离，并拒绝未知或越界恢复材料。
//! @author kongweiguang

use super::*;
use serde_json::json;

/// 插件可以保存任意 JSON，宿主不应假定其结构等于 view.data 或绑定路径。
#[test]
fn envelope_roundtrip_preserves_opaque_state_and_separate_drafts() {
    let plugin = PluginViewSavedState {
        state_version: 7,
        state: json!(["opaque", {"payload": 42}]),
    };
    let draft = NativeDraft {
        path: "note.text".into(),
        value: json!("unsubmitted"),
    };
    let saved = encode("editor", Some(plugin.clone()), vec![draft])
        .unwrap()
        .unwrap();
    assert_eq!(saved.state_version, 0);
    validate_native_content_state(&saved.state, "editor", 7).unwrap();
    assert!(validate_native_content_state(&saved.state, "editor", 8).is_err());
    let (restored, drafts) = decode(Some(saved), "editor").unwrap();
    assert_eq!(restored, Some(plugin));
    assert_eq!(drafts[0].path, "note.text");
    assert_eq!(drafts[0].value, json!("unsubmitted"));
}

/// 合法插件状态即便长得像宿主信封也必须原样转交，不按 JSON 键猜测格式。
#[test]
fn nonzero_plugin_state_never_decodes_as_host_envelope() {
    let state = PluginViewSavedState {
        state_version: 1,
        state: json!({"format": FORMAT, "schema": 99, "drafts": []}),
    };
    let (actual, drafts) = decode(Some(state.clone()), "editor").unwrap();
    assert_eq!(actual, Some(state));
    assert!(drafts.is_empty());
}

/// 两份未触发插件动作的标签草稿独立往返，恢复后远端更新仍只能改变基线和冲突标记。
#[test]
fn two_tab_drafts_survive_restart_before_first_remote_frame() {
    use crate::plugins::external::state::ViewState;
    let path = BindingPath::parse("text").unwrap();
    for text in ["draft-tab-one", "draft-tab-two"] {
        let mut first = ViewState::new(json!({"text":"default"}), 1);
        first.edit(&path, json!(text)).unwrap();
        let drafts = first
            .draft_values()
            .into_iter()
            .map(|(path, value)| NativeDraft {
                path: path.segments().join("."),
                value,
            })
            .collect();
        let saved = encode("editor", None, drafts).unwrap().unwrap();
        let (plugin, recovered) = decode(Some(saved), "editor").unwrap();
        assert_eq!(plugin, None);
        // 当前包的默认值恰好等于恢复草稿，也不能在首个远端快照前清掉脏标记。
        let mut next = ViewState::new(json!({"text":text}), 1);
        next.restore_draft(&path, recovered[0].value.clone())
            .unwrap();
        assert!(next.is_dirty(&path));
        next.replace_remote(json!({"text":"server"})).unwrap();
        assert_eq!(next.value(&path), Some(&json!(text)));
        assert_eq!(next.remote_value(&path), Some(&json!("server")));
        assert!(next.has_new_value(&path));
        next.apply_patch(&json!({"text":"new-server"})).unwrap();
        assert_eq!(next.value(&path), Some(&json!(text)));
    }
}

/// 无关本地编辑会执行全局等值清理，但静态默认值相同的恢复草稿仍须抵挡后续服务端值。
#[test]
fn unrelated_edit_cannot_confirm_restored_default_value() {
    use crate::plugins::external::state::ViewState;
    let path = BindingPath::parse("text").unwrap();
    let other = BindingPath::parse("other").unwrap();
    let mut state = ViewState::new(json!({"text":"draft", "other":"old"}), 1);
    state.restore_draft(&path, json!("draft")).unwrap();
    state.edit(&other, json!("edited")).unwrap();
    assert!(state.is_dirty(&path));
    state.apply_patch(&json!({"text":"server"})).unwrap();
    assert_eq!(state.value(&path), Some(&json!("draft")));
    assert_eq!(state.remote_value(&path), Some(&json!("server")));
    assert!(state.is_dirty(&path));
    assert!(state.has_new_value(&path));
}

/// 无关远端补丁同样不能确认恢复路径；目标首次更新后应保留草稿并展示冲突。
#[test]
fn unrelated_patch_cannot_confirm_restored_default_value() {
    use crate::plugins::external::state::ViewState;
    let path = BindingPath::parse("text").unwrap();
    let mut state = ViewState::new(json!({"text":"draft", "other":"old"}), 1);
    state.restore_draft(&path, json!("draft")).unwrap();
    state.apply_patch(&json!({"other":"updated"})).unwrap();
    assert!(state.is_dirty(&path));
    state.apply_patch(&json!({"text":"server"})).unwrap();
    assert_eq!(state.value(&path), Some(&json!("draft")));
    assert_eq!(state.remote_value(&path), Some(&json!("server")));
    assert!(state.is_dirty(&path));
    assert!(state.has_new_value(&path));
}

/// 目标权威等值补丁、完整快照及显式确认都应结束保护，避免恢复标记永久变脏。
#[test]
fn authoritative_or_explicit_confirmation_releases_recovered_draft() {
    use crate::plugins::external::state::ViewState;
    let path = BindingPath::parse("text").unwrap();
    for mode in 0..4 {
        let mut state = ViewState::new(json!({"text":"draft"}), 1);
        state.restore_draft(&path, json!("draft")).unwrap();
        match mode {
            0 => state.apply_patch(&json!({"text":"draft"})).unwrap(),
            1 => state.replace_remote(json!({"text":"draft"})).unwrap(),
            2 => state.confirm(&path).unwrap(),
            _ => state.accept_remote(&path).unwrap(),
        }
        assert!(!state.is_dirty(&path));
        state.apply_patch(&json!({"text":"server"})).unwrap();
        assert_eq!(state.value(&path), Some(&json!("server")));
    }
}

/// 当前控件约束决定草稿可否恢复，秘密父子别名和无效数字都不能成为落盘数据。
#[test]
fn current_view_policy_excludes_secrets_invalid_numbers_and_unknown_paths() {
    let node: NativeNode = serde_json::from_value(json!({"type":"stack","children":[
        {"type":"textarea","bind":"$text"},
        {"type":"input","bind":"credentials"},
        {"type":"secret","bind":"credentials.password"},
        {"type":"input","bind":"credentials.password"},
        {"type":"number","bind":"count","min":0,"max":10,"step":2},
        {"type":"select","bind":"choice","options":[{"label":"A","value":"a"}]},
        {"type":"checkbox","bind":"enabled"}
    ]}))
    .unwrap();
    let mut policy = DraftPolicy::default();
    policy.add_node(&node);
    assert!(policy.permits("text", &json!("draft")));
    assert!(policy.permits("enabled", &json!(true)));
    assert!(policy.permits("count", &json!(4)));
    assert!(policy.permits("choice", &json!("a")));
    for (path, value) in [
        ("credentials", json!("raw-secret")),
        ("credentials.password", json!("raw-secret")),
        ("text", json!({"password":"raw-secret"})),
        ("count", json!("-")),
        ("count", json!(11)),
        ("count", json!(3)),
        ("choice", json!("missing")),
        ("unknown", json!("raw")),
    ] {
        assert!(!policy.permits(path, &value), "{path}");
    }
}

/// 未知版本、视图串用、重复路径及大小越界全部失败，不能部分恢复后覆盖旧材料。
#[test]
fn malformed_or_future_envelopes_fail_closed() {
    let valid = encode(
        "editor",
        None,
        vec![NativeDraft {
            path: "text".into(),
            value: json!("draft"),
        }],
    )
    .unwrap()
    .unwrap();
    for (key, value) in [
        ("schema", json!(2)),
        ("view", json!("other")),
        ("format", json!("other")),
        ("drafts", json!([{"path":"a..b","value":"bad"}])),
        ("drafts", json!([{"path":"$text","value":"bad"}])),
        (
            "drafts",
            json!([{"path":"text","value":"a"},{"path":"text","value":"b"}]),
        ),
        ("drafts", json!([{"path":"text","value":{"nested":"bad"}}])),
        ("plugin_state", json!({"state_version":0,"state":{}})),
    ] {
        let mut broken = valid.clone();
        broken.state[key] = value;
        assert!(decode(Some(broken), "editor").is_err(), "{key}");
    }
    assert!(
        encode(
            "editor",
            None,
            vec![NativeDraft {
                path: "text".into(),
                value: json!("x".repeat(MAX_RECOVERY_BYTES))
            }]
        )
        .is_err()
    );
    assert_eq!(encode("editor", None, Vec::new()).unwrap(), None);
}
