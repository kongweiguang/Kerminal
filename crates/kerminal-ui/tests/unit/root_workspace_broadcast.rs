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
        FrozenBroadcastRequest, PendingSyncValidation, WorkspaceBroadcastResult,
        WorkspaceBroadcastTabState, finish_workspace_broadcast_error,
        reconcile_workspace_broadcast_tab_state, result_all_targets_sent, result_delivery_details,
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

/// 构造最小逐目标回执，验证部分成功与全目标成功的清稿边界。
///
/// @author kongweiguang
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

/// 反馈语义随当前消息更新，成功提示不能继承之前错误的颜色。
///
/// @author kongweiguang
#[test]
fn status_error_tone_tracks_latest_feedback() {
    let mut state = WorkspaceBroadcastTabState::default();

    state.set_status("目标已变化", true);
    assert!(state.status_is_error);
    assert_eq!(state.status.as_deref(), Some("目标已变化"));

    state.set_status("同步已开启", false);
    assert!(!state.status_is_error);
    assert_eq!(state.status.as_deref(), Some("同步已开启"));

    state.clear_status();
    assert!(!state.status_is_error);
    assert_eq!(state.status, None);
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

/// pane 移除会清掉失效自选 ID；由单 pane 回到多 pane 时重新展开工具栏。
///
/// @author kongweiguang
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

    reconcile_workspace_broadcast_tab_state(&mut state, &[first.clone(), added.clone()]);
    assert_eq!(state.custom_panes, BTreeSet::from([first.clone()]));
    assert!(!state.custom_panes.contains(&added));
    assert!(!state.bar_visible);

    reconcile_workspace_broadcast_tab_state(&mut state, std::slice::from_ref(&first));
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

/// 部分、重复或外来回执都不能伪装为完整成功并清空用户草稿。
///
/// @author kongweiguang
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
        delivery(first.clone(), true, true, false),
        delivery(second.clone(), true, true, false),
    ];
    let duplicate = vec![
        delivery(first.clone(), true, true, false),
        delivery(first.clone(), true, true, false),
    ];
    let foreign = vec![
        delivery(first, true, true, false),
        delivery(second, true, true, false),
        delivery(pane("foreign"), true, true, false),
    ];

    assert!(!result_all_targets_sent(&targets, &partial));
    assert!(result_all_targets_sent(&targets, &complete));
    assert!(!result_all_targets_sent(&targets, &duplicate));
    assert!(!result_all_targets_sent(&targets, &foreign));
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

/// 原始粘贴在超限或含 NUL 时仍可编辑，修正后错误反馈随之清除。
///
/// @author kongweiguang
#[test]
fn invalid_draft_is_preserved_and_can_be_corrected() {
    let mut state = WorkspaceBroadcastTabState::default();
    let oversized = "中".repeat(5462);
    state.replace_draft(oversized.clone(), None);
    assert_eq!(state.draft, oversized);
    assert!(state.status_is_error);
    assert!(
        state
            .status
            .as_deref()
            .is_some_and(|text| text.contains("16 KiB"))
    );
    state.replace_draft("echo\0unsafe".to_owned(), None);
    assert_eq!(state.draft, "echo\0unsafe");
    assert!(state.status_is_error);
    state.replace_draft("echo safe".to_owned(), None);
    assert!(!state.status_is_error);
    assert!(state.status.is_none());
}

/// 状态消息和草稿修正保留旧回执；只有新发送开始才清除上次结果。
///
/// @author kongweiguang
#[test]
fn previous_result_survives_status_and_draft_changes_until_new_send() {
    let mut state = WorkspaceBroadcastTabState::default();
    let result = WorkspaceBroadcastResult {
        request: FrozenBroadcastRequest {
            tab_id: TabId::new(),
            command: "echo old".to_owned(),
            draft_snapshot: "echo old".to_owned(),
            targets: vec![],
            ui_bindings: vec![],
            api_bindings: vec![],
        },
        deliveries: vec![],
    };
    state.last_result = Some(result.clone());
    state.set_status("同步按键已开启。", false);
    assert_eq!(
        state
            .last_result
            .as_ref()
            .map(|value| value.request.command.as_str()),
        Some("echo old")
    );
    state.replace_draft("echo new".to_owned(), None);
    assert_eq!(
        state
            .last_result
            .as_ref()
            .map(|value| value.request.command.as_str()),
        Some("echo old")
    );
    state.clear_status();
    assert!(state.last_result.is_some());
    state.begin_command(7, "正在校验所选终端…");
    assert!(state.last_result.is_none());
    assert_eq!(state.command_request_id, Some(7));
    assert_eq!(state.status.as_deref(), Some("正在校验所选终端…"));
}

/// 稀疏回执按发送快照列出旧目标；切换当前自选 pane 不会改写历史明细。
///
/// @author kongweiguang
#[test]
fn sparse_receipts_keep_frozen_targets_after_selection_changes() {
    let first = pane("frozen-first");
    let missing = pane("frozen-missing");
    let request = FrozenBroadcastRequest {
        tab_id: TabId::new(),
        command: "echo hello".to_owned(),
        draft_snapshot: "echo hello".to_owned(),
        targets: vec![
            BroadcastTarget {
                pane_id: first.clone(),
                title: "frozen first".to_owned(),
                target: CommandHistoryTarget::Local,
            },
            BroadcastTarget {
                pane_id: missing,
                title: "frozen missing".to_owned(),
                target: CommandHistoryTarget::Ssh,
            },
        ],
        ui_bindings: vec![],
        api_bindings: vec![],
    };
    let result = WorkspaceBroadcastResult {
        request,
        deliveries: vec![delivery(first, true, true, false)],
    };
    let mut state = WorkspaceBroadcastTabState::default();
    state.mode = WorkspaceBroadcastTargetMode::Custom;
    state.custom_panes.insert(pane("current-selection"));
    state.last_result = Some(result);

    let result = state
        .last_result
        .as_ref()
        .expect("frozen result remains stored");
    let summary = result.summary().expect("missing receipt must be reported");
    let details = result_delivery_details(result);
    assert!(summary.contains("1/2"));
    assert!(
        summary.contains("回执未确认 1")
            || summary.contains("1 delivery receipts were unconfirmed")
    );
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].title, "frozen missing");
    assert!(matches!(
        details[0].state.as_str(),
        "结果未确认" | "Outcome unconfirmed"
    ));
    assert!(
        !details
            .iter()
            .any(|detail| detail.title == "current-selection")
    );
}

/// 外层发送错误保留全部冻结目标为未知；编辑和切换选择不重发，下一次发送才清旧结果。
///
/// @author kongweiguang
#[test]
fn outer_broadcast_error_survives_edit_and_target_change_until_next_send() {
    let first = pane("outer-first");
    let second = pane("outer-second");
    let request = FrozenBroadcastRequest {
        tab_id: TabId::new(),
        command: "echo uncertain".to_owned(),
        draft_snapshot: "echo uncertain".to_owned(),
        targets: vec![
            BroadcastTarget {
                pane_id: first,
                title: "outer first".to_owned(),
                target: CommandHistoryTarget::Local,
            },
            BroadcastTarget {
                pane_id: second,
                title: "outer second".to_owned(),
                target: CommandHistoryTarget::Ssh,
            },
        ],
        ui_bindings: vec![],
        api_bindings: vec![],
    };
    let mut state = WorkspaceBroadcastTabState::default();
    finish_workspace_broadcast_error(&mut state, request, "发送结果无法确认");
    assert!(state.status_is_error);
    assert_eq!(state.status.as_deref(), Some("发送结果无法确认"));

    state.mode = WorkspaceBroadcastTargetMode::Custom;
    state.custom_panes.insert(pane("current-selection"));
    state.replace_draft("echo next".to_owned(), None);
    assert!(!state.status_is_error);
    let result = state
        .last_result
        .as_ref()
        .expect("outer error receipt is retained");
    let summary = result
        .summary()
        .expect("empty receipts are unknown outcomes");
    let details = result_delivery_details(result);
    assert!(
        summary.contains("回执未确认 2")
            || summary.contains("2 delivery receipts were unconfirmed")
    );
    assert_eq!(details.len(), 2);
    assert!(
        details
            .iter()
            .all(|detail| matches!(detail.state.as_str(), "结果未确认" | "Outcome unconfirmed"))
    );
    assert!(
        !details
            .iter()
            .any(|detail| detail.title == "current-selection")
    );

    state.begin_command(9, "正在发送…");
    assert!(state.last_result.is_none());
}

/// 完整成功静默收敛；部分回执保留可读摘要，避免把未知写入误报为成功。
///
/// @author kongweiguang
#[test]
fn result_summary_is_silent_for_complete_success_and_counts_partial_delivery() {
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
    let request = FrozenBroadcastRequest {
        tab_id: TabId::new(),
        command: "echo hello".to_owned(),
        draft_snapshot: "echo hello".to_owned(),
        targets: targets.clone(),
        ui_bindings: vec![],
        api_bindings: vec![],
    };
    let success = WorkspaceBroadcastResult {
        request: request.clone(),
        deliveries: vec![
            delivery(first.clone(), true, true, false),
            delivery(second.clone(), true, true, false),
        ],
    };
    assert_eq!(success.summary(), None);
    assert!(result_delivery_details(&success).is_empty());

    let duplicate = WorkspaceBroadcastResult {
        request: request.clone(),
        deliveries: vec![
            delivery(first.clone(), true, true, false),
            delivery(first.clone(), true, true, false),
            delivery(second.clone(), true, true, false),
        ],
    };
    let duplicate_summary = duplicate
        .summary()
        .expect("duplicate receipt must not be reported as complete success");
    assert!(
        duplicate_summary.contains("回执未确认 1")
            || duplicate_summary.contains("1 delivery receipts were unconfirmed")
    );
    assert!(matches!(
        result_delivery_details(&duplicate)[0].state.as_str(),
        "结果未确认" | "Outcome unconfirmed"
    ));

    let foreign = WorkspaceBroadcastResult {
        request: request.clone(),
        deliveries: vec![
            delivery(first.clone(), true, true, false),
            delivery(second.clone(), true, true, false),
            delivery(pane("foreign"), true, true, false),
        ],
    };
    let foreign_summary = foreign
        .summary()
        .expect("foreign receipt must prevent silent success");
    assert!(
        foreign_summary.contains("回执未确认 1")
            || foreign_summary.contains("1 delivery receipts were unconfirmed")
    );

    let partial = WorkspaceBroadcastResult {
        request,
        deliveries: vec![
            delivery(first, true, true, false),
            delivery(second, true, false, false),
        ],
    };
    let summary = partial
        .summary()
        .expect("partial delivery must be reported");
    assert!(summary.contains("1/2"));

    let mut history_unconfirmed = success;
    history_unconfirmed.deliveries[0].history_error_code =
        Some(kerminal_application::PublicErrorCode::Infrastructure);
    assert!(history_unconfirmed.summary().is_some());
}

/// 校验取消之后即使异步结果不再回写，当前 Tab 也不能永久停留在校验中。
///
/// @author kongweiguang
#[test]
fn cancelled_sync_validation_finishes_progress_feedback() {
    let token = tokio_util::sync::CancellationToken::new();
    let mut state = WorkspaceBroadcastTabState::default();
    state.set_status("正在校验同步目标…", false);
    state.replace_draft("echo edited".to_owned(), Some("正在校验同步目标…"));
    assert_eq!(state.status.as_deref(), Some("正在校验同步目标…"));
    state.replace_draft("echo\0invalid".to_owned(), Some("正在校验同步目标…"));
    assert!(state.status_is_error);
    state.replace_draft("echo corrected".to_owned(), Some("正在校验同步目标…"));
    assert_eq!(state.status.as_deref(), Some("正在校验同步目标…"));
    PendingSyncValidation {
        generation: 1,
        tab_id: TabId::new(),
        bindings: vec![],
        cancellation: token.clone(),
    }
    .cancel(Some(&mut state));
    assert!(token.is_cancelled());
    assert!(!state.sync_enabled);
    assert!(!state.status_is_error);
    assert_eq!(state.status.as_deref(), Some("同步按键已关闭。"));
}
