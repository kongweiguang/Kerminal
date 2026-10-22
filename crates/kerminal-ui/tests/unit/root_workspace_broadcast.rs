//! 群发 per-Tab 选择、异步回执和取消边界。
//!
//! @author kongweiguang

use std::collections::BTreeSet;

use kerminal_application::commands::BroadcastDeliveryResult;
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    commands::{BroadcastTarget, CommandHistoryTarget},
    identity::{PaneId, SessionId, TabId},
    sessions::SessionOwner,
};

use crate::{
    features::workspace_broadcast::WorkspaceBroadcastTargetMode,
    root::workspace_broadcast::{
        WorkspaceBroadcastTabState, invalidate_snippet_picker, is_current_snippet_request,
        is_safe_broadcast_retry, reconcile_workspace_broadcast_tab_state, result_all_targets_sent,
        select_workspace_broadcast_panes, should_cancel_active_workspace_command,
        should_cancel_pending_sync_validation, should_clear_workspace_broadcast_draft,
        workspace_broadcast_bindings_match_selection,
    },
};

/// 使用可读的稳定标识让纯状态测试直接比较布局中的 pane。
fn pane(id: &str) -> PaneId {
    PaneId::parse(id).expect("test pane id must be valid")
}

/// 构造具有 pane owner 与 generation 身份的输入绑定，覆盖目标集合的精确比较。
///
/// @author kongweiguang
fn binding(tab_id: &TabId, pane_id: &PaneId, generation: u64) -> UiSessionBinding {
    UiSessionBinding {
        session_id: SessionId::new(),
        owner: SessionOwner::Ui {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        },
        session_generation: generation,
    }
}

/// 构造最小逐目标回执，以便验证未知和取消结果的安全重试边界。
fn delivery(
    pane_id: PaneId,
    attempted: bool,
    sent: bool,
    cancelled: bool,
) -> BroadcastDeliveryResult {
    BroadcastDeliveryResult {
        pane_id,
        session_id: SessionId::new(),
        attempted,
        sent,
        cancelled,
        error_code: None,
        history_recorded: None,
        history_skip_reason: None,
        history_error_code: None,
    }
}

/// 三种模式共享布局顺序；Current 只跟随当前焦点，Custom 不隐式吸收新 pane。
#[test]
fn target_modes_keep_their_distinct_selection_rules() {
    let first = pane("first");
    let second = pane("second");
    let added = pane("added");
    let layout = vec![first.clone(), second.clone(), added.clone()];
    let custom = BTreeSet::from([first.clone(), added.clone()]);
    let default_state = WorkspaceBroadcastTabState::default();

    assert_eq!(default_state.mode, WorkspaceBroadcastTargetMode::All);
    assert!(!default_state.sync_enabled);

    assert_eq!(
        select_workspace_broadcast_panes(
            WorkspaceBroadcastTargetMode::All,
            &custom,
            &layout,
            Some(&second),
        ),
        layout
    );
    assert_eq!(
        select_workspace_broadcast_panes(
            WorkspaceBroadcastTargetMode::Current,
            &custom,
            &layout,
            Some(&second),
        ),
        vec![second]
    );
    assert_eq!(
        select_workspace_broadcast_panes(
            WorkspaceBroadcastTargetMode::Custom,
            &custom,
            &layout,
            None,
        ),
        vec![first, added]
    );
}

/// 未确认过的启动校验在切 Tab 或收起栏后失效，当前可见上下文仍可继续。
#[test]
fn pending_sync_validation_requires_same_active_tab_and_visible_bar() {
    let first = TabId::parse("first-tab").expect("valid test tab");
    let second = TabId::parse("second-tab").expect("valid test tab");

    assert!(!should_cancel_pending_sync_validation(
        Some(&first),
        &first,
        true,
        true,
    ));
    assert!(should_cancel_pending_sync_validation(
        Some(&second),
        &first,
        true,
        true,
    ));
    assert!(should_cancel_pending_sync_validation(
        Some(&first),
        &first,
        false,
        true,
    ));
    assert!(should_cancel_pending_sync_validation(
        Some(&first),
        &first,
        true,
        false,
    ));
}

/// All 新增可输入 pane 或 owner/generation 改变后，旧路由不得继续保持开启。
///
/// @author kongweiguang
#[test]
fn sync_binding_set_must_match_current_selection_exactly() {
    let tab = TabId::parse("broadcast-tab").expect("valid test tab");
    let first = pane("first");
    let second = pane("second");
    let added = pane("added");
    let first_binding = binding(&tab, &first, 1);
    let second_binding = binding(&tab, &second, 2);
    let expanded = binding(&tab, &added, 3);

    assert!(workspace_broadcast_bindings_match_selection(
        &[first_binding.clone(), second_binding.clone()],
        &[second_binding.clone(), first_binding.clone()],
    ));
    assert!(!workspace_broadcast_bindings_match_selection(
        &[first_binding.clone(), second_binding.clone()],
        &[first_binding.clone(), second_binding.clone(), expanded],
    ));
    assert!(!workspace_broadcast_bindings_match_selection(
        &[first_binding.clone(), second_binding.clone()],
        std::slice::from_ref(&first_binding),
    ));
    assert!(!workspace_broadcast_bindings_match_selection(
        &[first_binding.clone(), second_binding.clone()],
        &[first_binding, binding(&tab, &second, 4)],
    ));
}

/// pane 移除会清掉失效自选 ID；单 pane 隐藏弹层，下一次由单到多会重新展开工具栏。
#[test]
fn pane_lifecycle_prunes_custom_targets_and_reopens_after_single_pane() {
    let first = pane("first");
    let closed = pane("closed");
    let added = pane("added");
    let mut state = WorkspaceBroadcastTabState::default();
    state.mode = WorkspaceBroadcastTargetMode::Custom;
    state.custom_panes = BTreeSet::from([first.clone(), closed]);
    state.previous_view_pane_count = 2;
    state.bar_visible = false;
    state.snippets_open = true;
    state.snippet_generation = 9;

    reconcile_workspace_broadcast_tab_state(&mut state, &[first.clone(), added.clone()]);
    assert_eq!(state.custom_panes, BTreeSet::from([first.clone()]));
    assert!(!state.custom_panes.contains(&added));
    assert!(!state.bar_visible);

    reconcile_workspace_broadcast_tab_state(&mut state, std::slice::from_ref(&first));
    assert!(!state.snippets_open);
    assert_eq!(state.snippet_generation, 10);
    assert!(!state.bar_visible);

    reconcile_workspace_broadcast_tab_state(&mut state, &[first.clone(), added]);
    assert!(state.bar_visible);
    assert_eq!(
        select_workspace_broadcast_panes(
            state.mode,
            &state.custom_panes,
            &[first.clone(), pane("next")],
            None,
        ),
        vec![first]
    );
}

/// 弹层关闭通过代次使正在运行的目录/渲染回执失效，防止它重开或覆盖当前草稿。
#[test]
fn closing_snippet_picker_invalidates_pending_request_generation() {
    let mut state = WorkspaceBroadcastTabState::default();
    state.snippets_open = true;
    state.snippet_generation = 41;
    assert!(is_current_snippet_request(&state, 41));

    invalidate_snippet_picker(&mut state);

    assert!(!state.snippets_open);
    assert_eq!(state.snippet_generation, 42);
    assert!(!is_current_snippet_request(&state, 41));
    assert!(!is_current_snippet_request(&state, 42));
}

/// 切 Tab、隐藏工具栏或精确绑定变化都会停止后续目标；稳定场景继续保留冻结请求。
#[test]
fn active_command_cancels_when_tab_bar_or_binding_changes() {
    let first = TabId::parse("first-tab").expect("valid test tab");
    let second = TabId::parse("second-tab").expect("valid test tab");

    assert!(!should_cancel_active_workspace_command(
        Some(&first),
        &first,
        true,
        true
    ));
    assert!(should_cancel_active_workspace_command(
        Some(&second),
        &first,
        true,
        true
    ));
    assert!(should_cancel_active_workspace_command(
        Some(&first),
        &first,
        false,
        true
    ));
    assert!(should_cancel_active_workspace_command(
        Some(&first),
        &first,
        true,
        false
    ));
}

/// 重试只接受未尝试的明确取消；失败、未知和已送达目标都不能再次写入。
#[test]
fn safe_retry_excludes_sent_and_attempted_unknown_deliveries() {
    assert!(is_safe_broadcast_retry(&delivery(
        pane("cancelled"),
        false,
        false,
        true,
    )));
    assert!(!is_safe_broadcast_retry(&delivery(
        pane("sent"),
        true,
        true,
        false,
    )));
    assert!(!is_safe_broadcast_retry(&delivery(
        pane("unknown"),
        true,
        false,
        false,
    )));
    assert!(!is_safe_broadcast_retry(&delivery(
        pane("failed"),
        true,
        false,
        true,
    )));
}

/// 部分失败保留输入；即使旧发送全成功，迟到回执也不能清掉用户正在编辑的新草稿。
#[test]
fn draft_clears_only_after_all_targets_sent_and_snapshot_still_matches() {
    let first = pane("first");
    let second = pane("second");
    let targets = vec![
        BroadcastTarget {
            pane_id: first.clone(),
            title: "first".to_owned(),
            target: CommandHistoryTarget::Local,
        },
        BroadcastTarget {
            pane_id: second.clone(),
            title: "second".to_owned(),
            target: CommandHistoryTarget::Ssh,
        },
    ];
    let partial = vec![delivery(first.clone(), true, true, false)];
    let complete = vec![
        delivery(first, true, true, false),
        delivery(second, true, true, false),
    ];

    assert!(!result_all_targets_sent(&targets, &partial));
    assert!(result_all_targets_sent(&targets, &complete));
    assert!(!should_clear_workspace_broadcast_draft(
        false,
        "old command",
        "old command",
    ));
    assert!(!should_clear_workspace_broadcast_draft(
        true,
        "new command",
        "old command",
    ));
    assert!(should_clear_workspace_broadcast_draft(
        true,
        "old command",
        "old command",
    ));
}
