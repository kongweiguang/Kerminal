//! 片段新建入口的空白草稿基线回归。
//!
//! @author kongweiguang

use kerminal_domain::commands::{
    SnippetContextBinding, SnippetContextBindingKind, SnippetDefaultAction, SnippetRisk,
    SnippetScope,
};

use super::SnippetDraftSnapshot;

/// 用与宽工作台和 compact install_new_snippet 一致的字段创建表单快照。
///
/// @author kongweiguang
fn blank_snapshot(is_new: bool, compact_layout: bool) -> SnippetDraftSnapshot {
    SnippetDraftSnapshot {
        title: String::new(),
        command: String::new(),
        description: String::new(),
        tags: String::new(),
        category: if is_new && compact_layout {
            "custom".to_owned()
        } else {
            String::new()
        },
        sort_order: match (is_new, compact_layout) {
            (true, true) => "10",
            (true, false) => "0",
            (false, _) => "",
        }
        .to_owned(),
        scope: SnippetScope::Any,
        risk: if is_new && compact_layout {
            SnippetRisk::Change
        } else {
            SnippetRisk::Unknown
        },
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
    assert!(blank_snapshot(false, false).matches_empty_baseline(false, false));
    assert!(blank_snapshot(true, false).matches_empty_baseline(true, false));
    assert!(blank_snapshot(true, true).matches_empty_baseline(true, true));
}

/// 内容或无效排序输入都偏离空白基线，后续仍由原 dirty 校验保护草稿。
///
/// @author kongweiguang
#[test]
fn edited_or_invalid_new_snippet_fields_leave_clean_baseline() {
    let mut edited = blank_snapshot(true, true);
    edited.command = "echo ready".to_owned();
    assert!(!edited.matches_empty_baseline(true, true));

    let mut edited_metadata = blank_snapshot(true, true);
    edited_metadata.category = "operations".to_owned();
    assert!(!edited_metadata.matches_empty_baseline(true, true));

    let mut edited_risk = blank_snapshot(true, true);
    edited_risk.risk = SnippetRisk::Unknown;
    assert!(!edited_risk.matches_empty_baseline(true, true));

    let mut edited_sort_order = blank_snapshot(true, true);
    edited_sort_order.sort_order = "11".to_owned();
    assert!(!edited_sort_order.matches_empty_baseline(true, true));

    let mut invalid_sort_order = blank_snapshot(false, false);
    invalid_sort_order.sort_order = "later".to_owned();
    assert!(!invalid_sort_order.matches_empty_baseline(false, false));
}
