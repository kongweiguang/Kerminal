//! 片段新建入口的空白草稿基线回归。
//!
//! @author kongweiguang

use kerminal_domain::commands::{
    SnippetContextBinding, SnippetContextBindingKind, SnippetDefaultAction, SnippetRisk,
    SnippetScope,
};

use super::SnippetDraftSnapshot;

/// 用与 CommandsView 构造和 install_new_snippet 一致的字段创建表单快照。
///
/// @author kongweiguang
fn blank_snapshot(is_new: bool) -> SnippetDraftSnapshot {
    SnippetDraftSnapshot {
        title: String::new(),
        command: String::new(),
        description: String::new(),
        tags: String::new(),
        category: String::new(),
        sort_order: if is_new { "0" } else { "" }.to_owned(),
        scope: SnippetScope::Any,
        risk: SnippetRisk::Unknown,
        default_action: SnippetDefaultAction::Insert,
        variable_count: 0,
        context_bindings: if is_new {
            vec![(
                SnippetContextBinding {
                    kind: SnippetContextBindingKind::Global,
                    target_id: None,
                },
                String::new(),
            )]
        } else {
            Vec::new()
        },
    }
}

/// 首次打开的空目录表单和点击新建后安装的默认表单都不应触发离开确认。
///
/// @author kongweiguang
#[test]
fn empty_catalog_and_new_snippet_defaults_match_clean_baseline() {
    assert!(blank_snapshot(false).matches_empty_baseline(false));
    assert!(blank_snapshot(true).matches_empty_baseline(true));
}

/// 内容或无效排序输入都偏离空白基线，后续仍由原 dirty 校验保护草稿。
///
/// @author kongweiguang
#[test]
fn edited_or_invalid_new_snippet_fields_leave_clean_baseline() {
    let mut edited = blank_snapshot(true);
    edited.command = "echo ready".to_owned();
    assert!(!edited.matches_empty_baseline(true));

    let mut invalid_sort_order = blank_snapshot(false);
    invalid_sort_order.sort_order = "later".to_owned();
    assert!(!invalid_sort_order.matches_empty_baseline(false));
}
