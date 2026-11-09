//! 原生插件视图状态独立验证，不依赖窗口线程或真实插件进程。
//! @author kongweiguang

use serde_json::{Value, json};

use super::{BindingPath, ViewState, ViewStateError};

/// 测试路径使用生产解析器，避免测试自行接受比协议更宽的语法。
fn path(value: &str) -> BindingPath {
    BindingPath::parse(value).expect("test binding must be valid")
}

/// 页数据携带坐标，便于发现乱序结果覆盖或占位行混入。
fn numbered_rows(start: usize, end: usize) -> Vec<Value> {
    (start..end).map(|index| json!({ "id": index })).collect()
}

/// 绑定语法必须与协议的对象键约束一致，不接受表达式或数组索引语法。
#[test]
fn validates_binding_paths() {
    assert_eq!(path("$form.port").segments(), &["form", "port"]);
    for invalid in ["", "$", "a..b", ".a", "a.", "a.$b", "a[0]", "a b", "中文"] {
        assert_eq!(
            BindingPath::parse(invalid),
            Err(ViewStateError::InvalidPath),
            "{invalid}"
        );
    }
    assert_eq!(
        BindingPath::parse(&"a".repeat(257)),
        Err(ViewStateError::InvalidPath)
    );
}

/// 命中草稿只显示局部冲突；兄弟更新和远端删除仍按补丁语义生效。
#[test]
fn preserves_drafts_and_marks_only_touched_fields() {
    let mut state = ViewState::new(
        json!({"form": {"name": "old", "port": 22, "other": true}}),
        1,
    );
    let name = path("form.name");
    let port = path("form.port");
    state.edit(&name, json!("draft")).unwrap();
    state.edit(&port, json!(2222)).unwrap();
    state
        .apply_patch(&json!({"form": {"name": null, "other": false}}))
        .unwrap();
    assert_eq!(state.value(&name), Some(&json!("draft")));
    assert_eq!(state.remote_value(&name), None);
    assert_eq!(state.value(&port), Some(&json!(2222)));
    assert_eq!(state.data()["form"]["other"], json!(false));
    assert!(state.has_new_value(&name));
    assert!(!state.has_new_value(&port));
    state.accept_remote(&name).unwrap();
    assert_eq!(state.value(&name), None);
    assert!(!state.is_dirty(&name));
    assert!(state.is_dirty(&port));
}

/// 父级删除与非对象替换都不能吞掉嵌套草稿，接受远端由用户显式决定。
#[test]
fn retains_nested_drafts_across_ancestor_replacements() {
    let mut state = ViewState::new(json!({"form": {"name": "old", "port": 22}}), 1);
    let name = path("form.name");
    state.edit(&name, json!("draft")).unwrap();
    state.apply_patch(&json!({"form": null})).unwrap();
    assert_eq!(state.data(), &json!({"form": {"name": "draft"}}));
    assert_eq!(state.remote_value(&path("form")), None);
    assert!(state.has_new_value(&name));
    state.acknowledge_new_value(&name);
    state.apply_patch(&json!({"form": 7})).unwrap();
    assert!(state.has_new_value(&name));
    assert_eq!(state.value(&name), Some(&json!("draft")));
    state.acknowledge_new_value(&name);
    state.apply_patch(&json!({"form": {"other": 1}})).unwrap();
    assert!(state.has_new_value(&name));
    assert_eq!(
        state.data(),
        &json!({"form": {"name": "draft", "other": 1}})
    );
}

/// 接受被删除的字段后应回到远端结构，不能残留为保护草稿创建的空父对象。
#[test]
fn accepting_deleted_ancestor_removes_temporary_objects() {
    let mut state = ViewState::new(json!({"form": {"name": "old"}, "other": 1}), 1);
    let name = path("form.name");
    state.edit(&name, json!("draft")).unwrap();
    state.edit(&path("other"), json!(2)).unwrap();
    state.apply_patch(&json!({"form": null})).unwrap();
    state.accept_remote(&name).unwrap();
    assert_eq!(state.data(), &json!({"other": 2}));
    assert!(state.is_dirty(&path("other")));
}

/// 父子草稿恢复顺序保持子字段的最终值，确认和接受不会清除无关字段。
#[test]
fn handles_overlapping_drafts_and_explicit_confirmation() {
    let mut state = ViewState::new(json!({"form": {"name": "old", "port": 22}, "other": 1}), 1);
    let form = path("form");
    let name = path("form.name");
    let other = path("other");
    state
        .edit(&form, json!({"name": "first", "port": 22}))
        .unwrap();
    state.edit(&name, json!("second")).unwrap();
    state.edit(&other, json!(2)).unwrap();
    state
        .apply_patch(&json!({"form": {"name": "remote"}}))
        .unwrap();
    assert_eq!(state.value(&name), Some(&json!("second")));
    state.confirm(&form).unwrap();
    assert!(!state.is_dirty(&form));
    assert!(!state.is_dirty(&name));
    assert_eq!(state.remote_value(&name), Some(&json!("second")));
    assert!(state.is_dirty(&other));
    state.edit(&other, json!(1)).unwrap();
    assert!(!state.is_dirty(&other));
}

/// 非法路径和超大补丁都在写入前失败，不能留下半个对象或部分更新。
#[test]
fn rejects_invalid_writes_and_oversized_patches_atomically() {
    let original = json!({"form": {"port": 22}, "scalar": 7});
    let mut state = ViewState::new(original.clone(), 1);
    assert_eq!(
        state.edit(&path("scalar.port"), json!(8)),
        Err(ViewStateError::PathThroughNonObject)
    );
    assert_eq!(state.data(), &original);
    let large = json!({"form": {"port": 99}, "large": "a".repeat(1024 * 1024)});
    assert_eq!(
        state.apply_patch(&large),
        Err(ViewStateError::PatchTooLarge)
    );
    assert_eq!(state.data(), &original);
    assert_eq!(state.remote_value(&path("form.port")), Some(&json!(22)));
}

/// 每个按钮独立忙碌，进程重启和关闭后旧结果不能解除新动作。
#[test]
fn action_tickets_respect_busy_generation_and_close() {
    let mut state = ViewState::new(json!({}), 1);
    let old = state.begin_action("save").unwrap();
    assert_eq!(state.begin_action("save"), Err(ViewStateError::Busy));
    let other = state.begin_action("refresh").unwrap();
    state.finish_action(&other).unwrap();
    state.advance_generation(2).unwrap();
    let current = state.begin_action("save").unwrap();
    assert_eq!(state.finish_action(&old), Err(ViewStateError::Stale));
    assert!(state.is_action_busy("save"));
    state.finish_action(&current).unwrap();
    state.close();
    state.close();
    assert_eq!(state.begin_action("save"), Err(ViewStateError::Closed));
    assert_eq!(state.finish_action(&current), Err(ViewStateError::Closed));
}

/// 相邻范围合并，同时缓存与在途范围都阻止重复的行请求。
#[test]
fn merges_and_deduplicates_page_requests() {
    let mut state = ViewState::new(json!({}), 1);
    state.queue_page("list", 0..3).unwrap();
    state.queue_page("list", 3..6).unwrap();
    let first = state.take_page_requests("list").unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].range(), 0..6);
    state.queue_page("list", 1..5).unwrap();
    assert!(state.take_page_requests("list").unwrap().is_empty());
    state
        .finish_page(&first[0], 9, numbered_rows(0, 6))
        .unwrap();
    assert_eq!(
        state
            .page_rows("list")
            .iter()
            .map(|row| row.index())
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    state.queue_page("list", 4..8).unwrap();
    let second = state.take_page_requests("list").unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].range(), 6..8);
    assert_eq!(
        state.queue_page("list", 0..501),
        Err(ViewStateError::InvalidRange)
    );
    assert_eq!(
        state.queue_page("list", 3..3),
        Err(ViewStateError::InvalidRange)
    );
}

/// 不同页乱序返回时保留旧视图，较早请求不能把总数回滚到过期值。
#[test]
fn keeps_complete_rows_until_out_of_order_pages_are_ready() {
    let mut state = ViewState::new(json!({}), 1);
    state.queue_page("list", 0..3).unwrap();
    let initial = state.take_page_requests("list").unwrap().remove(0);
    state
        .finish_page(&initial, 20, numbered_rows(0, 3))
        .unwrap();
    state.queue_page("list", 1..5).unwrap();
    let older = state.take_page_requests("list").unwrap().remove(0);
    state.queue_page("list", 0..7).unwrap();
    let newer = state.take_page_requests("list").unwrap().remove(0);
    assert_eq!(older.range(), 3..5);
    assert_eq!(newer.range(), 5..7);
    state.finish_page(&newer, 20, numbered_rows(5, 7)).unwrap();
    assert_eq!(
        state
            .page_rows("list")
            .iter()
            .map(|row| row.index())
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    state.finish_page(&older, 100, numbered_rows(3, 5)).unwrap();
    assert_eq!(
        state
            .page_rows("list")
            .iter()
            .map(|row| row.index())
            .collect::<Vec<_>>(),
        (0..7).collect::<Vec<_>>()
    );
    assert_eq!(state.page_total("list"), Some(20));
}

/// 错误行数和请求失败释放在途标记，同时保留上次完整显示内容。
#[test]
fn invalid_and_failed_pages_can_be_retried_without_clearing_display() {
    let mut state = ViewState::new(json!({}), 1);
    state.queue_page("list", 0..2).unwrap();
    let initial = state.take_page_requests("list").unwrap().remove(0);
    state
        .finish_page(&initial, 10, numbered_rows(0, 2))
        .unwrap();
    state.queue_page("list", 2..4).unwrap();
    let invalid = state.take_page_requests("list").unwrap().remove(0);
    assert_eq!(
        state.finish_page(&invalid, 10, numbered_rows(2, 3)),
        Err(ViewStateError::InvalidPage)
    );
    assert_eq!(state.page_rows("list").len(), 2);
    state.queue_page("list", 2..4).unwrap();
    let failed = state.take_page_requests("list").unwrap().remove(0);
    state.fail_page(&failed).unwrap();
    assert_eq!(state.fail_page(&failed), Err(ViewStateError::Stale));
    assert_eq!(state.page_rows("list").len(), 2);
    state.queue_page("list", 2..4).unwrap();
    assert_eq!(state.take_page_requests("list").unwrap()[0].range(), 2..4);
}

/// 集合失效和进程重启均保留旧显示，但任何旧请求都不能重新写回。
#[test]
fn invalidation_generation_and_close_reject_stale_pages() {
    let mut state = ViewState::new(json!({"field": "old"}), 1);
    state.edit(&path("field"), json!("draft")).unwrap();
    state.queue_page("list", 0..2).unwrap();
    let initial = state.take_page_requests("list").unwrap().remove(0);
    state.finish_page(&initial, 5, numbered_rows(0, 2)).unwrap();
    state.queue_page("list", 2..4).unwrap();
    let stale = state.take_page_requests("list").unwrap().remove(0);
    state.invalidate_page("list").unwrap();
    assert_eq!(state.page_total("list"), Some(5));
    assert_eq!(state.page_rows("list").len(), 2);
    assert_eq!(
        state.finish_page(&stale, 5, numbered_rows(2, 4)),
        Err(ViewStateError::Stale)
    );
    let retry = state.take_page_requests("list").unwrap().remove(0);
    assert_eq!(retry.range(), 2..4);
    state.advance_generation(2).unwrap();
    assert_eq!(state.value(&path("field")), Some(&json!("draft")));
    assert_eq!(
        state.finish_page(&retry, 5, numbered_rows(2, 4)),
        Err(ViewStateError::Stale)
    );
    let restarted = state.take_page_requests("list").unwrap().remove(0);
    state.close();
    assert_eq!(
        state.finish_page(&restarted, 5, numbered_rows(2, 4)),
        Err(ViewStateError::Closed)
    );
}

/// 丢失事件后使用完整快照恢复时，旧字段必须消失，同时保留并标记有冲突的草稿。
#[test]
fn full_snapshot_replaces_remote_but_preserves_conflicting_drafts() {
    let mut state = ViewState::new(
        json!({"form": {"name": "old", "removed": 1}, "obsolete": true}),
        1,
    );
    state.edit(&path("form.name"), json!("draft")).unwrap();
    state
        .replace_remote(json!({"form": {"name": "remote", "fresh": 2}}))
        .unwrap();
    assert_eq!(
        state.data(),
        &json!({"form": {"name": "draft", "fresh": 2}})
    );
    assert_eq!(
        state.remote_value(&path("form.name")),
        Some(&json!("remote"))
    );
    assert!(state.has_new_value(&path("form.name")));
    state.accept_remote(&path("form.name")).unwrap();
    assert_eq!(
        state.data(),
        &json!({"form": {"name": "remote", "fresh": 2}})
    );
}

/// 远端确认值恰好等于当前草稿时，应自动解除冲突而不丢失其他字段的草稿。
#[test]
fn matching_remote_patch_confirms_only_matching_drafts() {
    let mut state = ViewState::new(json!({"name": "before", "port": 22}), 1);
    let name = path("name");
    let port = path("port");
    state.edit(&name, json!("edited")).unwrap();
    state.edit(&port, json!(2222)).unwrap();
    state.apply_patch(&json!({"name": "edited"})).unwrap();
    assert_eq!(state.value(&name), Some(&json!("edited")));
    assert!(!state.is_dirty(&name));
    assert!(!state.has_new_value(&name));
    assert!(state.is_dirty(&port));
}
