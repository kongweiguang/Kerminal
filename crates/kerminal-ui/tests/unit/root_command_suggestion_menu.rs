//! 命令建议菜单的选择状态回归测试。
//!
//! @author kongweiguang

use kerminal_domain::commands::{
    CommandSuggestionActivation, CommandSuggestionCandidate, CommandSuggestionReplacementRange,
    CommandSuggestionSensitivity, SuggestionPresentation, SuggestionProviderKind,
};

use super::MenuSelection;

/// 构造只有菜单所需信息的候选，确保选择逻辑不依赖 provider 正文或业务副作用。
fn candidate(id: &str) -> CommandSuggestionCandidate {
    CommandSuggestionCandidate {
        id: id.to_owned(),
        provider: SuggestionProviderKind::History,
        activation: CommandSuggestionActivation::Insert,
        display_text: id.to_owned(),
        replacement_text: id.to_owned(),
        replacement_range: CommandSuggestionReplacementRange { start: 0, end: 0 },
        suffix: String::new(),
        score: 1.0,
        sensitivity: CommandSuggestionSensitivity::Normal,
        description: None,
        source_explanation: None,
        merged_source_explanations: Vec::new(),
        source_id: None,
        allowed_presentations: vec![SuggestionPresentation::Menu],
        accept_boundaries: Vec::new(),
        context_key: None,
    }
}

/// 同一请求更新候选时保留仍有效的高亮 ID，避免异步排序变化误选另一命令。
#[test]
fn same_generation_preserves_selected_candidate_by_id() {
    let mut selection = MenuSelection::default();
    selection.reconcile(7, &[candidate("first"), candidate("second")]);
    assert!(selection.select(7, 1));

    selection.reconcile(7, &[candidate("second"), candidate("first")]);

    assert_eq!(selection.selected_id(), Some("second"));
}

/// 新查询代次重置旧高亮，防止菜单快速重开时以旧索引接受不同输入的候选。
#[test]
fn new_generation_resets_highlight_to_first_candidate() {
    let mut selection = MenuSelection::default();
    selection.reconcile(7, &[candidate("first"), candidate("second")]);
    assert!(selection.select(7, 1));

    selection.reconcile(8, &[candidate("new-first"), candidate("new-second")]);

    assert_eq!(selection.selected_id(), Some("new-first"));
}

/// 键盘导航在列表首尾环绕，空结果时不创建虚假的可选项。
#[test]
fn navigation_wraps_and_empty_candidates_remain_unselected() {
    let mut selection = MenuSelection::default();
    selection.reconcile(7, &[candidate("first"), candidate("second")]);

    assert!(selection.move_by(-1));
    assert_eq!(selection.selected_id(), Some("second"));
    assert!(selection.move_by(1));
    assert_eq!(selection.selected_id(), Some("first"));

    selection.reconcile(8, &[]);
    assert!(!selection.move_by(1));
    assert_eq!(selection.selected_id(), None);
}
