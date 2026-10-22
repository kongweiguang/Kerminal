//! 标签投影的编号、分组目标和新组名称校验。
//!
//! @author kongweiguang

use super::{
    GroupTitleError, PendingTabReveal, TabDropPlacement, TabRevealReason,
    WorkspaceCloseBatchIntent, WorkspaceFileTabAction, WorkspaceNameEditKeyAction,
    WorkspaceNameEditTarget, WorkspaceTabEvent, WorkspaceTabGroupProjection, WorkspaceTabKind,
    WorkspaceTabProjection, WorkspaceTabTrackSection, WorkspaceTabs,
    append_shared_sftp_workbench_projection, capture_workspace_name_edit_key, group_color_actions,
    group_drop_before_id, invalidate_tab_reveal_for_group_change, minimal_active_tab_reveal_offset,
    next_tab_drag_scroll_offset, partition_shared_sftp_workbench_close_batch,
    reconcile_active_tab_reveal_projection, should_enable_ungrouped_drop_target,
    should_show_workspace_group_header, tab_drop_event, tab_drop_placement,
    take_workspace_name_edit_key_action, trailing_tab_drop_event, ungrouped_tab_drop_event,
    validate_group_title, workspace_file_context_action_events, workspace_name_edit_event,
    workspace_tab_event_targets, workspace_tab_track_width_budget,
};
use gpui_kit::{Bounds, SharedString, point, px, size};
use kerminal_domain::identity::TabId;
use std::cell::Cell;

/// 实际 viewport builder 必须可压缩并独立裁切，固定入口不能由宽轨道撑开或越界遮挡。
/// 此处只验证布局投影；左折叠按钮命中与右端菜单焦点仍需 Root 原生验收。
///
/// @author kongweiguang
#[test]
fn workspace_tab_viewport_shrinks_and_clips_without_navigation_slots() {
    use gpui_kit::{Overflow, Styled};

    let mut viewport = super::render::workspace_tabs_scroll_viewport();
    let style = viewport.style();

    assert_eq!(style.flex_grow, Some(1.));
    assert_eq!(style.flex_shrink, Some(1.));
    assert_eq!(style.min_size.width, Some(px(0.).into()));
    assert_eq!(style.overflow.x, Some(Overflow::Scroll));
    assert_eq!(style.overflow.y, Some(Overflow::Hidden));
}

/// 下拉只在标签与尾随 + 实际溢出时出现；已出现时回补自身槽位后即可退出。
///
/// @author kongweiguang
#[test]
fn tab_picker_visibility_tracks_overflow_without_self_locking() {
    let should_show = super::render::workspace_tab_picker_should_show;

    assert!(!should_show(px(0.), false));
    assert!(should_show(px(1.), false));
    assert!(should_show(px(33.), true));
    assert!(!should_show(px(32.), true));
    assert!(!should_show(px(0.), true));
}

/// 总列表不按滚动或组折叠过滤，完整长标题、活动标记和稳定 ID 与标签显示顺序保持一致。
/// 菜单快照独立于后续投影排序，避免点击旧菜单时用新索引激活错误标签。
///
/// @author kongweiguang
#[test]
fn tab_picker_keeps_collapsed_and_ungrouped_tabs_with_stable_identities() {
    let mut hidden = tab("tab-hidden", "完整而未省略的长标签标题", Some("group-b"));
    hidden.active = true;
    let mut tabs = WorkspaceTabs::new(vec![
        tab("tab-loose", "Loose", None),
        hidden,
        tab("tab-first", "First", Some("group-a")),
    ]);
    let mut collapsed = group("group-b", "Collapsed", 1);
    collapsed.collapsed = true;
    tabs.groups = vec![collapsed, group("group-a", "First group", 0)];
    tabs.show_tab_numbers = true;

    let captured = tabs.tab_picker_entries();
    assert_eq!(
        captured,
        vec![
            (
                TabId::parse("tab-first").unwrap(),
                SharedString::from("3  First"),
                false,
            ),
            (
                TabId::parse("tab-hidden").unwrap(),
                SharedString::from("2  完整而未省略的长标签标题"),
                true,
            ),
            (
                TabId::parse("tab-loose").unwrap(),
                SharedString::from("1  Loose"),
                false,
            ),
        ]
    );

    tabs.projection.swap(0, 1);
    assert_eq!(captured[1].0.as_str(), "tab-hidden");
    assert_eq!(tabs.tab_picker_entries()[1].0, captured[1].0);
    assert_eq!(captured[1].1.as_ref(), "2  完整而未省略的长标签标题");
    assert_eq!(
        tabs.tab_picker_entries()[1].1.as_ref(),
        "1  完整而未省略的长标签标题"
    );
}

/// 文件右键事件捕获原 TabId；其他标签类型不生成会误操作文件的动作。
///
/// @author kongweiguang
#[test]
fn workspace_file_context_actions_keep_stable_target_and_reject_other_kinds() {
    let tab_id = TabId::parse("file-tab-1").expect("valid file tab id");
    assert_eq!(
        workspace_file_context_action_events(&WorkspaceTabKind::WorkspaceFile, &tab_id),
        vec![
            WorkspaceTabEvent::FileAction {
                tab_id: tab_id.clone(),
                action: WorkspaceFileTabAction::CopyFullPath,
            },
            WorkspaceTabEvent::FileAction {
                tab_id: tab_id.clone(),
                action: WorkspaceFileTabAction::Reload,
            },
        ]
    );
    assert!(workspace_file_context_action_events(&WorkspaceTabKind::Terminal, &tab_id).is_empty());
}

/// 共享 SFTP 临时标签只在实际选中时独占 active；隐藏时保留 controller 的持久 active 投影。
///
/// @author kongweiguang
#[test]
fn shared_sftp_workbench_projection_has_one_active_owner() {
    let workbench_id = TabId::parse("sftp-workbench").expect("valid tab ID");
    let mut persistent = tab("persistent-transfer", "Transfer", None);
    persistent.active = true;
    let mut active_projection = vec![persistent.clone()];

    assert!(append_shared_sftp_workbench_projection(
        &mut active_projection,
        &workbench_id,
        true,
    ));
    assert_eq!(active_projection.iter().filter(|tab| tab.active).count(), 1);
    let workbench_projection = active_projection
        .iter()
        .find(|tab| tab.id == workbench_id)
        .expect("workbench projection exists");
    assert!(workbench_projection.active);
    assert_eq!(workbench_projection.kind, WorkspaceTabKind::SftpWorkbench);
    assert!(workbench_projection.group_id.is_none());

    let mut hidden_projection = vec![persistent];
    assert!(append_shared_sftp_workbench_projection(
        &mut hidden_projection,
        &workbench_id,
        false,
    ));
    assert_eq!(hidden_projection.iter().filter(|tab| tab.active).count(), 1);
    let hidden_workbench = hidden_projection
        .iter()
        .find(|tab| tab.kind == WorkspaceTabKind::SftpWorkbench)
        .expect("hidden workbench projection exists");
    assert!(!hidden_workbench.active);
}

/// 批量关闭先从持久 reducer 意图剔除共享临时标签，且拒绝把它伪装成锚点或分组成员。
///
/// @author kongweiguang
#[test]
fn shared_sftp_workbench_close_batch_filters_only_the_temporary_target() {
    let keep = TabId::parse("keep-tab").expect("valid tab ID");
    let target = TabId::parse("target-tab").expect("valid tab ID");
    let workbench = TabId::parse("sftp-workbench").expect("valid tab ID");
    let intent = WorkspaceCloseBatchIntent::Others {
        keep_tab_id: keep.clone(),
        tab_ids: vec![target.clone(), workbench.clone()],
    };

    let (includes_workbench, persistent_intent) =
        partition_shared_sftp_workbench_close_batch(&intent, &workbench)
            .expect("workbench can be a close target");
    assert!(includes_workbench);
    assert_eq!(
        persistent_intent,
        WorkspaceCloseBatchIntent::Others {
            keep_tab_id: keep.clone(),
            tab_ids: vec![target.clone()],
        }
    );
    assert!(!workspace_tab_event_targets(
        &WorkspaceTabEvent::CloseBatch(persistent_intent.clone()),
        &workbench,
    ));
    assert!(workspace_tab_event_targets(
        &WorkspaceTabEvent::MoveToUngroupedEnd(workbench.clone()),
        &workbench,
    ));

    let only_workbench = WorkspaceCloseBatchIntent::Others {
        keep_tab_id: keep.clone(),
        tab_ids: vec![workbench.clone()],
    };
    let (includes_workbench, empty_persistent_intent) =
        partition_shared_sftp_workbench_close_batch(&only_workbench, &workbench)
            .expect("workbench can be the only close target");
    assert!(includes_workbench);
    assert!(empty_persistent_intent.tab_ids().is_empty());

    let invalid_anchor = WorkspaceCloseBatchIntent::ToRight {
        anchor_tab_id: workbench.clone(),
        tab_ids: vec![target],
    };
    assert!(partition_shared_sftp_workbench_close_batch(&invalid_anchor, &workbench).is_none());
    assert!(workspace_tab_event_targets(
        &WorkspaceTabEvent::CloseBatch(invalid_anchor),
        &workbench,
    ));
}

/// 实测 bounds 只移动遮挡距离；已可见标签不动，过宽标签以左边缘对齐。
///
/// @author kongweiguang
#[test]
fn active_tab_reveal_offset_is_minimal_and_handles_tabs_wider_than_viewport() {
    let viewport = Bounds {
        origin: point(px(100.), px(0.)),
        size: size(px(400.), px(36.)),
    };
    let current = point(px(-200.), px(0.));
    let visible = Bounds {
        origin: point(px(140.), px(0.)),
        size: size(px(180.), px(36.)),
    };
    assert_eq!(
        minimal_active_tab_reveal_offset(current, viewport, visible),
        current
    );

    let clipped_right = Bounds {
        origin: point(px(450.), px(0.)),
        size: size(px(180.), px(36.)),
    };
    assert_eq!(
        minimal_active_tab_reveal_offset(current, viewport, clipped_right),
        point(px(-330.), px(0.))
    );

    let clipped_left = Bounds {
        origin: point(px(40.), px(0.)),
        size: size(px(180.), px(36.)),
    };
    assert_eq!(
        minimal_active_tab_reveal_offset(current, viewport, clipped_left),
        point(px(-140.), px(0.))
    );

    let wider_than_viewport = Bounds {
        origin: point(px(60.), px(0.)),
        size: size(px(500.), px(36.)),
    };
    assert_eq!(
        minimal_active_tab_reveal_offset(current, viewport, wider_than_viewport),
        point(px(-160.), px(0.))
    );
}

/// 由固定 ID 构造只读标签，测试只覆盖排序投影，不需要创建 GPUI 窗口。
fn tab(id: &str, title: &str, group_id: Option<&str>) -> WorkspaceTabProjection {
    WorkspaceTabProjection {
        id: TabId::parse(id).expect("valid tab ID"),
        title: SharedString::from(title.to_owned()),
        group_id: group_id.map(str::to_owned),
        active: false,
        kind: WorkspaceTabKind::WorkspaceFile,
        layout: None,
    }
}

/// 同名检查用显示标题而不是内部 ID，避免旧版任意组 ID 泄漏到交互语义。
fn group(id: &str, title: &str, order: usize) -> WorkspaceTabGroupProjection {
    WorkspaceTabGroupProjection {
        id: id.to_owned(),
        title: SharedString::from(title.to_owned()),
        color: None,
        collapsed: false,
        order,
    }
}

/// 同一 active ID 的排序变化会丢弃上一帧测量，但不把旧 bounds delta 用到新顺序。
///
/// @author kongweiguang
#[test]
fn active_tab_reorder_invalidates_stale_measurement_without_new_reveal() {
    let mut active = tab("tab-active", "Active", None);
    active.active = true;
    let other = tab("tab-other", "Other", None);
    let current_projection = vec![active.clone(), other.clone()];
    let next_projection = vec![other, active];
    let captured = PendingTabReveal {
        tab_id: TabId::parse("tab-active").expect("valid tab ID"),
        generation: 4,
        expected_offset: point(px(-120.), px(0.)),
        reason: TabRevealReason::ActiveSelection,
    };
    let mut generation = captured.generation;
    let mut pending = Some(captured.clone());

    reconcile_active_tab_reveal_projection(
        &current_projection,
        &next_projection,
        &mut generation,
        &mut pending,
        point(px(-120.), px(0.)),
    );

    assert_eq!(generation, captured.generation + 1);
    assert!(pending.is_none());
}

/// 同一 active ID 的分组/折叠布局变化仅使旧测量失效，不凭布局变化发起自动滚动。
///
/// @author kongweiguang
#[test]
fn group_projection_change_invalidates_stale_measurement_without_new_reveal() {
    let current_groups = vec![group("group-a", "A", 0)];
    let mut collapsed_group = group("group-a", "A", 0);
    collapsed_group.collapsed = true;
    let next_groups = vec![collapsed_group];
    let captured = PendingTabReveal {
        tab_id: TabId::parse("tab-active").expect("valid tab ID"),
        generation: 9,
        expected_offset: point(px(-220.), px(0.)),
        reason: TabRevealReason::ActiveSelection,
    };
    let mut generation = captured.generation;
    let mut pending = Some(captured.clone());

    invalidate_tab_reveal_for_group_change(
        &current_groups,
        &next_groups,
        &mut generation,
        &mut pending,
    );

    assert_eq!(generation, captured.generation + 1);
    assert!(pending.is_none());
}

/// 相同组快照不会使待测量请求作废，重复 refresh 不应制造可见滚动跳变。
///
/// @author kongweiguang
#[test]
fn unchanged_group_projection_preserves_pending_reveal() {
    let groups = vec![group("group-a", "A", 0)];
    let captured = PendingTabReveal {
        tab_id: TabId::parse("tab-active").expect("valid tab ID"),
        generation: 5,
        expected_offset: point(px(-90.), px(0.)),
        reason: TabRevealReason::ActiveSelection,
    };
    let mut generation = captured.generation;
    let mut pending = Some(captured.clone());

    invalidate_tab_reveal_for_group_change(&groups, &groups, &mut generation, &mut pending);

    assert_eq!(generation, captured.generation);
    let current = pending.expect("unchanged groups preserve request");
    assert_eq!(current.tab_id, captured.tab_id);
    assert_eq!(current.generation, captured.generation);
    assert_eq!(current.expected_offset, captured.expected_offset);
}

/// 无变化的重复快照保留同一请求，避免普通 refresh 吃掉尚待下一帧处理的 reveal。
///
/// @author kongweiguang
#[test]
fn unchanged_tab_projection_preserves_pending_reveal() {
    let mut active = tab("tab-active", "Active", None);
    active.active = true;
    let projection = vec![active];
    let captured = PendingTabReveal {
        tab_id: TabId::parse("tab-active").expect("valid tab ID"),
        generation: 3,
        expected_offset: point(px(-80.), px(0.)),
        reason: TabRevealReason::ActiveSelection,
    };
    let mut generation = captured.generation;
    let mut pending = Some(captured.clone());

    reconcile_active_tab_reveal_projection(
        &projection,
        &projection,
        &mut generation,
        &mut pending,
        point(px(-80.), px(0.)),
    );

    assert_eq!(generation, captured.generation);
    let current = pending.expect("unchanged projection preserves request");
    assert_eq!(current.tab_id, captured.tab_id);
    assert_eq!(current.generation, captured.generation);
    assert_eq!(current.expected_offset, captured.expected_offset);
}

/// 标题更新不改变稳定顺序或可见性；保留键盘请求但换代，令下一帧重测新投影边界。
///
/// @author kongweiguang
#[test]
fn metadata_projection_change_retags_pending_keyboard_reveal() {
    let mut active = tab("tab-active", "Active", None);
    active.active = true;
    let focused = tab("tab-focused", "Focused", None);
    let current_projection = vec![active, focused];
    let mut next_projection = current_projection.clone();
    next_projection[1].title = "Renamed focused terminal".into();
    let captured = PendingTabReveal {
        tab_id: TabId::parse("tab-focused").expect("valid tab ID"),
        generation: 8,
        expected_offset: point(px(-140.), px(0.)),
        reason: TabRevealReason::KeyboardFocus,
    };
    let mut generation = captured.generation;
    let mut pending = Some(captured.clone());

    reconcile_active_tab_reveal_projection(
        &current_projection,
        &next_projection,
        &mut generation,
        &mut pending,
        point(px(-140.), px(0.)),
    );

    let request = pending.expect("metadata refresh keeps the valid keyboard target");
    assert_eq!(generation, captured.generation + 1);
    assert_eq!(request.tab_id, captured.tab_id);
    assert_eq!(request.generation, generation);
    assert_eq!(request.expected_offset, captured.expected_offset);
    assert_eq!(request.reason, TabRevealReason::KeyboardFocus);
}

/// active ID 变化才建立新请求，且捕获当时偏移供手动滚动 stale gate 使用。
///
/// @author kongweiguang
#[test]
fn active_identity_change_creates_one_reveal_for_new_tab() {
    let mut previous = tab("tab-old", "Old", None);
    previous.active = true;
    let mut next = tab("tab-new", "New", None);
    next.active = true;
    let current_projection = vec![previous];
    let next_projection = vec![next];
    let expected_offset = point(px(-340.), px(0.));
    let mut generation = 6;
    let mut pending = None;

    reconcile_active_tab_reveal_projection(
        &current_projection,
        &next_projection,
        &mut generation,
        &mut pending,
        expected_offset,
    );

    let request = pending.expect("new active identity schedules reveal");
    assert_eq!(generation, 7);
    assert_eq!(
        request.tab_id,
        TabId::parse("tab-new").expect("valid tab ID")
    );
    assert_eq!(request.generation, generation);
    assert_eq!(request.expected_offset, expected_offset);
}

/// 调色菜单的九个选择必须始终落到同一稳定组 ID，默认项只清除颜色而不丢失目标。
///
/// @author kongweiguang
#[test]
fn group_color_actions_keep_target_and_complete_palette() {
    let colors = group_color_actions("group-a")
        .into_iter()
        .map(|(_, _, event)| match event {
            WorkspaceTabEvent::SetGroupColor { group_id, color } => {
                assert_eq!(group_id, "group-a");
                color
            }
            other => panic!("unexpected group color action: {other:?}"),
        })
        .collect::<Vec<_>>();

    assert_eq!(
        colors,
        vec![
            None,
            Some(kerminal_application::workspace::WorkspaceTabGroupColor::Blue),
            Some(kerminal_application::workspace::WorkspaceTabGroupColor::Pink),
            Some(kerminal_application::workspace::WorkspaceTabGroupColor::Purple),
            Some(kerminal_application::workspace::WorkspaceTabGroupColor::Mint),
            Some(kerminal_application::workspace::WorkspaceTabGroupColor::Amber),
            Some(kerminal_application::workspace::WorkspaceTabGroupColor::Teal),
            Some(kerminal_application::workspace::WorkspaceTabGroupColor::Orange),
            Some(kerminal_application::workspace::WorkspaceTabGroupColor::Gray),
        ]
    );
}

/// 无命名组时不额外显示未分组行；有命名组时保留非空区与拖动期间的空 drop target。
///
/// @author kongweiguang
#[test]
fn workspace_group_header_visibility_hides_only_idle_empty_ungrouped() {
    assert!(should_show_workspace_group_header(
        Some("group-a"),
        true,
        false,
        false
    ));
    assert!(should_show_workspace_group_header(None, true, true, false));
    assert!(should_show_workspace_group_header(None, true, false, true));
    assert!(!should_show_workspace_group_header(
        None, true, false, false
    ));
    assert!(!should_show_workspace_group_header(
        None, false, true, false
    ));
    assert!(!should_show_workspace_group_header(
        None, false, false, true
    ));
    assert!(!should_show_workspace_group_header(
        None, false, false, false
    ));
}

/// 未分组目标只在有组且存在活动拖拽时临时扩张，并将稳定 TabId 映射为清组意图。
///
/// @author kongweiguang
#[test]
fn ungrouped_drop_target_requires_groups_and_active_drag() {
    assert!(!should_enable_ungrouped_drop_target(false, false));
    assert!(!should_enable_ungrouped_drop_target(false, true));
    assert!(!should_enable_ungrouped_drop_target(true, false));
    assert!(should_enable_ungrouped_drop_target(true, true));

    let tab_id = TabId::parse("tab-source").expect("valid tab ID");
    assert_eq!(
        ungrouped_tab_drop_event(tab_id.clone()),
        WorkspaceTabEvent::MoveToGroup {
            tab_id,
            group_id: None,
        }
    );
}

/// 尾部投放只发源标签末尾意图，避免合成工作台成为被拒绝的持久化落点。
///
/// @author kongweiguang
#[test]
fn trailing_drop_appends_atomically_and_ignores_stale_or_already_last_sources() {
    let source_id = TabId::parse("tab-a").expect("valid tab ID");
    let grouped_projection = vec![tab("tab-a", "A", Some("group-a")), tab("tab-b", "B", None)];
    assert_eq!(
        trailing_tab_drop_event(&source_id, &grouped_projection),
        Some(WorkspaceTabEvent::MoveToUngroupedEnd(source_id.clone()))
    );

    let ungrouped_projection = vec![tab("tab-a", "A", None), tab("tab-b", "B", None)];
    assert_eq!(
        trailing_tab_drop_event(&source_id, &ungrouped_projection),
        Some(WorkspaceTabEvent::MoveToUngroupedEnd(source_id.clone()))
    );
    assert_eq!(
        trailing_tab_drop_event(
            &TabId::parse("tab-b").expect("valid tab ID"),
            &ungrouped_projection,
        ),
        None,
        "already-last source has no visible reorder to commit"
    );

    let workbench_id = TabId::parse("sftp-workbench").expect("valid tab ID");
    let mut projection_with_workbench = vec![tab("tab-a", "A", None)];
    assert!(append_shared_sftp_workbench_projection(
        &mut projection_with_workbench,
        &workbench_id,
        false,
    ));
    let tail_event = trailing_tab_drop_event(&source_id, &projection_with_workbench)
        .expect("persistent tab can be dropped after the synthetic workbench projection");
    assert_eq!(
        tail_event,
        WorkspaceTabEvent::MoveToUngroupedEnd(source_id.clone())
    );
    assert!(
        !workspace_tab_event_targets(&tail_event, &workbench_id),
        "the synthetic workbench must not become a persistent drop target"
    );

    assert_eq!(
        trailing_tab_drop_event(
            &TabId::parse("tab-stale").expect("valid tab ID"),
            &ungrouped_projection,
        ),
        None,
        "stale drag identity is ignored before it reaches the reducer"
    );
}

/// 多组、多固定宽标签时滚动轨道仍将 72px 末尾 drop 区计入宽度，避免目标落在不可滚动溢出中。
///
/// @author kongweiguang
#[test]
fn wide_workspace_track_budget_keeps_trailing_ungrouped_target_scrollable() {
    let visible_sections = [
        WorkspaceTabTrackSection {
            header_text_units: Some(5),
            collapsed_header: false,
            visible_tab_count: 3,
            is_ungrouped_drop_target: false,
        },
        WorkspaceTabTrackSection {
            header_text_units: Some(4),
            collapsed_header: false,
            visible_tab_count: 2,
            is_ungrouped_drop_target: false,
        },
        WorkspaceTabTrackSection {
            header_text_units: Some(3),
            collapsed_header: false,
            visible_tab_count: 0,
            is_ungrouped_drop_target: true,
        },
    ];
    let mut without_target = visible_sections;
    without_target[2].is_ungrouped_drop_target = false;

    let content_width = workspace_tab_track_width_budget(&without_target);
    let track_width = workspace_tab_track_width_budget(&visible_sections);

    assert!(content_width > 1_000.);
    assert_eq!(track_width, 1_431.);
    assert_eq!(track_width, content_width + 72.);
}

/// 普通态多组固定宽标签也使用完整轨道预算，避免内容溢出未进入 ScrollHandle 的滚动范围。
///
/// @author kongweiguang
#[test]
fn wide_workspace_track_budget_keeps_normal_tabs_scrollable_without_drop_target() {
    let sections = [
        WorkspaceTabTrackSection {
            header_text_units: Some(5),
            collapsed_header: false,
            visible_tab_count: 3,
            is_ungrouped_drop_target: false,
        },
        WorkspaceTabTrackSection {
            header_text_units: Some(4),
            collapsed_header: false,
            visible_tab_count: 2,
            is_ungrouped_drop_target: false,
        },
    ];

    let track_width = workspace_tab_track_width_budget(&sections);

    assert_eq!(track_width, 1_235.);
}

/// 序号跟随全局投影索引，不因标签组或滚动窗口变化而重排。
#[test]
fn tab_numbers_follow_global_projection_order() {
    let mut tabs = WorkspaceTabs::new(vec![
        tab("tab-a", "A", Some("group-b")),
        tab("tab-b", "B", None),
        tab("tab-c", "C", Some("group-b")),
    ]);
    tabs.show_tab_numbers = true;

    assert_eq!(tabs.display_title(0).to_string(), "1  A");
    assert_eq!(tabs.display_title(1).to_string(), "2  B");
    assert_eq!(tabs.display_title(2).to_string(), "3  C");
}

/// 投影保留空组与空未分组数据；标题可见性另由活动拖拽策略决定。
///
/// @author kongweiguang
#[test]
fn grouped_projection_keeps_empty_groups_and_ungrouped_target() {
    let mut tabs = WorkspaceTabs::new(vec![tab("tab-a", "A", Some("group-b"))]);
    tabs.groups = vec![
        group("group-b", "Operations", 0),
        group("group-empty", "Empty", 1),
    ];

    assert_eq!(
        tabs.grouped_indices(),
        vec![
            (Some("group-b".to_owned()), vec![0]),
            (Some("group-empty".to_owned()), vec![]),
            (None, vec![]),
        ]
    );
}

/// 仅提交规范化后的名称；空白、控制字符和大小写无关重复组都在发事件前拒绝。
#[test]
fn new_group_name_is_trimmed_and_validated() {
    let groups = vec![group("group-a", "DevOps", 0)];

    assert_eq!(
        validate_group_title("  远程主机组  ", &groups, None),
        Ok("远程主机组".to_owned())
    );
    assert_eq!(
        validate_group_title("   ", &groups, None),
        Err(GroupTitleError::Empty)
    );
    assert_eq!(
        validate_group_title("bad\nname", &groups, None),
        Err(GroupTitleError::ControlCharacter)
    );
    assert_eq!(
        validate_group_title("devops", &groups, None),
        Err(GroupTitleError::Duplicate)
    );
    assert_eq!(
        validate_group_title("DevOps", &groups, Some("group-a")),
        Ok("DevOps".to_owned()),
        "renaming a group without changing its name must not collide with itself"
    );
}

/// 改名事件保留打开菜单时捕获的 TabId，不会依据后来变化的活动标签重新定位。
///
/// @author kongweiguang
#[test]
fn rename_event_keeps_the_captured_tab_identity() {
    let captured_tab = TabId::parse("tab-a").unwrap();
    let event = workspace_name_edit_event(
        WorkspaceNameEditTarget::RenameTab {
            tab_id: captured_tab.clone(),
        },
        "Renamed tab".to_owned(),
    );

    assert_eq!(
        event,
        WorkspaceTabEvent::RenameTab {
            tab_id: captured_tab,
            title: "Renamed tab".to_owned(),
        }
    );
}

/// 拖放事件同时保留源和目标 TabId，使 reducer 不必依赖可能过期的投影索引。
///
/// @author kongweiguang
#[test]
fn drop_event_keeps_both_captured_tab_identities() {
    let source_tab_id = TabId::parse("tab-source").unwrap();
    let target_tab_id = TabId::parse("tab-target").unwrap();

    assert_eq!(
        tab_drop_event(
            source_tab_id.clone(),
            target_tab_id.clone(),
            TabDropPlacement::After,
        ),
        WorkspaceTabEvent::DropOnTab {
            tab_id: source_tab_id,
            target_tab_id,
            placement: TabDropPlacement::After,
        }
    );
}

/// 拖放命中按目标实际左右半区决定，边界归入后半区以保持单一、可重复落点。
///
/// @author kongweiguang
#[test]
fn tab_drop_placement_uses_target_half() {
    let target_bounds = Bounds {
        origin: point(px(100.), px(0.)),
        size: size(px(180.), px(36.)),
    };

    assert_eq!(
        tab_drop_placement(point(px(189.), px(18.)), target_bounds),
        TabDropPlacement::Before
    );
    assert_eq!(
        tab_drop_placement(point(px(190.), px(18.)), target_bounds),
        TabDropPlacement::After
    );
}

/// 组头按左右半区换算成 controller 的 before-group 契约，尾组后半区提交追加。
///
/// @author kongweiguang
#[test]
fn group_drop_placement_handles_before_after_append_and_stale_target() {
    let ordered = vec![
        "group-a".to_owned(),
        "group-b".to_owned(),
        "group-c".to_owned(),
    ];

    assert_eq!(
        group_drop_before_id(&ordered, "group-b", TabDropPlacement::Before),
        Some(Some("group-b".to_owned()))
    );
    assert_eq!(
        group_drop_before_id(&ordered, "group-b", TabDropPlacement::After),
        Some(Some("group-c".to_owned()))
    );
    assert_eq!(
        group_drop_before_id(&ordered, "group-c", TabDropPlacement::After),
        Some(None)
    );
    assert_eq!(
        group_drop_before_id(&ordered, "missing", TabDropPlacement::After),
        None
    );
}

/// 标签方向键循环移动焦点，Home/End 到可见集合两端；未知键不消费或改写焦点。
///
/// @author kongweiguang
#[test]
fn workspace_tab_keyboard_order_wraps_and_respects_boundaries() {
    let ids = [
        TabId::parse("tab-a").unwrap(),
        TabId::parse("tab-b").unwrap(),
        TabId::parse("tab-c").unwrap(),
    ];

    assert_eq!(
        super::workspace_tab_keyboard_target_index(&ids, &ids[0], "ArrowLeft"),
        Some(2)
    );
    assert_eq!(
        super::workspace_tab_keyboard_target_index(&ids, &ids[2], "right"),
        Some(0)
    );
    assert_eq!(
        super::workspace_tab_keyboard_target_index(&ids, &ids[1], "Home"),
        Some(0)
    );
    assert_eq!(
        super::workspace_tab_keyboard_target_index(&ids, &ids[1], "End"),
        Some(2)
    );
    assert_eq!(
        super::workspace_tab_keyboard_target_index(&ids, &ids[1], "Escape"),
        None
    );
    assert_eq!(
        super::workspace_tab_keyboard_target_index(&[], &ids[0], "right"),
        None
    );
}

/// 拖动只在边缘推进滚动并遵守两端上限，鼠标离开边缘后不会留下滚动动作。
///
/// @author kongweiguang
#[test]
fn tab_drag_edge_scroll_is_clamped_and_stops_outside_edge_zones() {
    let viewport = Bounds {
        origin: point(px(100.), px(0.)),
        size: size(px(300.), px(36.)),
    };

    let left = next_tab_drag_scroll_offset(
        point(px(102.), px(18.)),
        viewport,
        point(px(-40.), px(0.)),
        point(px(120.), px(0.)),
    )
    .expect("left edge moves toward the start");
    assert!(left.x > px(-40.));
    assert_eq!(
        next_tab_drag_scroll_offset(
            point(px(105.), px(18.)),
            viewport,
            point(px(0.), px(0.)),
            point(px(120.), px(0.)),
        ),
        None,
        "the start bound prevents positive offsets"
    );

    let right = next_tab_drag_scroll_offset(
        point(px(398.), px(18.)),
        viewport,
        point(px(-100.), px(0.)),
        point(px(120.), px(0.)),
    )
    .expect("right edge moves toward the end");
    assert!(right.x < px(-100.));
    assert_eq!(
        next_tab_drag_scroll_offset(
            point(px(250.), px(18.)),
            viewport,
            point(px(-40.), px(0.)),
            point(px(120.), px(0.)),
        ),
        None
    );
    assert_eq!(
        next_tab_drag_scroll_offset(
            point(px(398.), px(18.)),
            viewport,
            point(px(-120.), px(0.)),
            point(px(120.), px(0.)),
        ),
        None,
        "the end bound prevents overscroll"
    );
    assert_eq!(
        next_tab_drag_scroll_offset(
            point(px(102.), px(40.)),
            viewport,
            point(px(-40.), px(0.)),
            point(px(120.), px(0.)),
        ),
        None,
        "horizontal proximity outside the viewport must not keep scrolling"
    );
}

/// 竖线只保留在未激活、未悬停的相邻标签间，激活或悬停不留下短色线。
///
/// @author kongweiguang
#[test]
fn tab_separator_is_hidden_next_to_active_or_hovered_tabs() {
    let mut left = tab("tab-left", "Left", None);
    let right = tab("tab-right", "Right", None);
    let separator_visible = super::render::should_draw_tab_separator;

    assert!(separator_visible(&left, &right, None));
    assert!(!separator_visible(&left, &right, Some(&left.id)));
    left.active = true;
    assert!(!separator_visible(&left, &right, None));
}

/// 输入先处理 IME 键并清除 marked range，父级快捷键仍须使用按键前快照而不取消编辑。
///
/// @author kongweiguang
#[test]
fn ime_key_uses_pre_action_composition_snapshot() {
    for key in ["Escape", "Enter"] {
        let snapshot = Cell::new(None);
        let live_composing = Cell::new(true);

        capture_workspace_name_edit_key(&snapshot, 7, key, true, live_composing.get());
        // InputState 会在事件冒泡前处理组合键；之后读取实时状态已不能代表按键时状态。
        live_composing.set(false);

        assert!(!live_composing.get());
        assert_eq!(
            take_workspace_name_edit_key_action(&snapshot, 7, key),
            None,
            "IME {key} must remain an input action after the marked range is cleared"
        );
        assert!(
            snapshot.get().is_none(),
            "a key decision consumes its snapshot"
        );
    }
}

/// 快捷键快照只在输入聚焦时建立，并且不能跨重命名代次保存或取消另一标签。
///
/// @author kongweiguang
#[test]
fn rename_key_snapshot_requires_focus_and_current_generation() {
    let snapshot = Cell::new(None);
    capture_workspace_name_edit_key(&snapshot, 3, "Enter", false, false);
    assert!(
        snapshot.get().is_none(),
        "unfocused input must not capture a shortcut"
    );

    capture_workspace_name_edit_key(&snapshot, 3, "Enter", true, false);
    assert_eq!(
        take_workspace_name_edit_key_action(&snapshot, 4, "Enter"),
        None,
        "an old edit generation must not submit the current editor"
    );

    capture_workspace_name_edit_key(&snapshot, 4, "Enter", true, false);
    assert_eq!(
        take_workspace_name_edit_key_action(&snapshot, 4, "Enter"),
        Some(WorkspaceNameEditKeyAction::Submit)
    );
}

/// 批量关闭只复制点击瞬间的分组/顺序目标，后续新标签不会扩展原操作范围。
#[test]
fn close_intents_capture_ordered_tab_ids_at_click_time() {
    let mut tabs = WorkspaceTabs::new(vec![
        tab("tab-a", "A", Some("group-a")),
        tab("tab-b", "B", Some("group-a")),
        tab("tab-c", "C", Some("group-b")),
        tab("tab-d", "D", None),
    ]);
    tabs.groups = vec![group("group-b", "B", 1), group("group-a", "A", 0)];

    let group_intent = tabs.close_group_intent("group-a").unwrap();
    let right_intent = tabs
        .close_right_intent(&TabId::parse("tab-a").unwrap())
        .unwrap();
    let others_intent = tabs
        .close_others_intent(&TabId::parse("tab-b").unwrap())
        .unwrap();

    assert_eq!(
        group_intent.tab_ids(),
        &[
            TabId::parse("tab-a").unwrap(),
            TabId::parse("tab-b").unwrap()
        ]
    );
    assert_eq!(
        right_intent.tab_ids(),
        &[
            TabId::parse("tab-b").unwrap(),
            TabId::parse("tab-c").unwrap(),
            TabId::parse("tab-d").unwrap()
        ]
    );
    assert_eq!(
        others_intent.tab_ids(),
        &[
            TabId::parse("tab-a").unwrap(),
            TabId::parse("tab-c").unwrap(),
            TabId::parse("tab-d").unwrap()
        ]
    );

    tabs.projection.push(tab("tab-e", "E", Some("group-a")));
    assert_eq!(
        group_intent.tab_ids(),
        &[
            TabId::parse("tab-a").unwrap(),
            TabId::parse("tab-b").unwrap()
        ],
        "the owned intent remains unchanged if the projection refreshes before confirmation"
    );
}

/// 新菜单动作分别固定“保留整个组”与“保留组内锚点”，目标快照不会随投影刷新扩展。
///
/// @author kongweiguang
#[test]
fn group_scoped_close_intents_capture_keep_members_and_exact_targets() {
    let mut tabs = WorkspaceTabs::new(vec![
        tab("tab-a", "A", Some("group-a")),
        tab("tab-b", "B", Some("group-a")),
        tab("tab-c", "C", Some("group-b")),
        tab("tab-d", "D", None),
    ]);
    tabs.groups = vec![group("group-a", "A", 0), group("group-b", "B", 1)];

    let close_group_others = tabs
        .close_group_others_intent(&TabId::parse("tab-b").unwrap())
        .unwrap();
    let close_outside = tabs.close_outside_group_intent("group-a").unwrap();

    assert_eq!(
        close_group_others,
        super::WorkspaceCloseBatchIntent::GroupOthers {
            group_id: "group-a".to_owned(),
            keep_tab_id: TabId::parse("tab-b").unwrap(),
            tab_ids: vec![TabId::parse("tab-a").unwrap()],
        }
    );
    assert_eq!(
        close_outside,
        super::WorkspaceCloseBatchIntent::OutsideGroup {
            group_id: "group-a".to_owned(),
            keep_tab_ids: vec![
                TabId::parse("tab-a").unwrap(),
                TabId::parse("tab-b").unwrap(),
            ],
            tab_ids: vec![
                TabId::parse("tab-c").unwrap(),
                TabId::parse("tab-d").unwrap(),
            ],
        }
    );

    tabs.projection.push(tab("tab-e", "E", Some("group-a")));
    assert_eq!(
        close_outside.tab_ids(),
        &[
            TabId::parse("tab-c").unwrap(),
            TabId::parse("tab-d").unwrap()
        ],
        "the menu intent owns its exact click-time close list"
    );
}

/// 用真实标签类型构造确认目标，避免把未参与关闭的文件或传输标签算入说明。
///
/// @author kongweiguang
fn close_target(id: &str, kind: WorkspaceTabKind) -> WorkspaceTabProjection {
    let mut projection = tab(id, id, None);
    projection.kind = kind;
    projection
}

/// 单终端说明不引入文件或内部资源规则，两种语言都保留关闭失败时的可恢复结果。
///
/// @author kongweiguang
#[test]
fn close_confirmation_single_terminal_uses_short_description() {
    let tabs = WorkspaceTabs::new(vec![close_target("terminal-a", WorkspaceTabKind::Terminal)]);
    let tab_ids = vec![TabId::parse("terminal-a").unwrap()];

    assert_eq!(
        tabs.close_confirmation_description(&tab_ids, 1),
        (
            "将关闭此终端标签。关闭失败时，标签会保留。".to_owned(),
            "This terminal tab will close. If closing fails, the tab will stay open.".to_owned(),
        )
    );
}

/// 纯终端批量确认使用捕获数量与自然复数，不展示无关文件保护说明。
///
/// @author kongweiguang
#[test]
fn close_confirmation_terminal_batch_uses_actual_count() {
    let tabs = WorkspaceTabs::new(vec![
        close_target("terminal-a", WorkspaceTabKind::Terminal),
        close_target("terminal-b", WorkspaceTabKind::Terminal),
    ]);
    let tab_ids = vec![
        TabId::parse("terminal-a").unwrap(),
        TabId::parse("terminal-b").unwrap(),
    ];

    assert_eq!(
        tabs.close_confirmation_description(&tab_ids, 2),
        (
            "将关闭 2 个终端标签。关闭失败的标签会保留。".to_owned(),
            "This will close 2 terminal tabs. Tabs that fail to close will stay open.".to_owned(),
        )
    );
}

/// 混合关闭只统计捕获的真实文件，保留其它标签的总数，并覆盖双语单复数与未保存提示。
///
/// @author kongweiguang
#[test]
fn close_confirmation_mixed_targets_count_only_selected_files() {
    let tabs = WorkspaceTabs::new(vec![
        close_target("terminal-a", WorkspaceTabKind::Terminal),
        close_target("terminal-b", WorkspaceTabKind::Terminal),
        close_target("file-a", WorkspaceTabKind::WorkspaceFile),
        close_target("file-b", WorkspaceTabKind::WorkspaceFile),
        close_target("transfer", WorkspaceTabKind::SftpTransfer),
        close_target("future", WorkspaceTabKind::Unsupported("future".into())),
    ]);

    for (ids, terminal_count, zh_description, en_description) in [
        (
            vec!["terminal-a", "file-a", "transfer", "future"],
            1,
            "将关闭 4 个标签，包含 1 个终端和 1 个文件。未保存的文件会另行确认。终端关闭失败时，标签会保留。",
            "This will close 4 tabs, including 1 terminal and 1 file. You'll be prompted about unsaved files. Terminal tabs that fail to close will stay open.",
        ),
        (
            vec![
                "terminal-a",
                "terminal-b",
                "file-a",
                "file-b",
                "transfer",
                "future",
            ],
            2,
            "将关闭 6 个标签，包含 2 个终端和 2 个文件。未保存的文件会另行确认。终端关闭失败时，标签会保留。",
            "This will close 6 tabs, including 2 terminals and 2 files. You'll be prompted about unsaved files. Terminal tabs that fail to close will stay open.",
        ),
    ] {
        let tab_ids = ids
            .into_iter()
            .map(|id| TabId::parse(id).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            tabs.close_confirmation_description(&tab_ids, terminal_count),
            (zh_description.to_owned(), en_description.to_owned())
        );
    }
}

/// 传输和未知标签不触发文件说明，未选中的真实文件也不扩展本次确认范围。
///
/// @author kongweiguang
#[test]
fn close_confirmation_non_file_targets_omit_unsaved_file_notice() {
    let tabs = WorkspaceTabs::new(vec![
        close_target("terminal-a", WorkspaceTabKind::Terminal),
        close_target("transfer", WorkspaceTabKind::SftpTransfer),
        close_target("future", WorkspaceTabKind::Unsupported("future".into())),
        close_target("unselected-file", WorkspaceTabKind::WorkspaceFile),
    ]);
    let tab_ids = ["terminal-a", "transfer", "future"]
        .into_iter()
        .map(|id| TabId::parse(id).unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        tabs.close_confirmation_description(&tab_ids, 1),
        (
            "将关闭 3 个标签，其中 1 个终端。终端关闭失败时，标签会保留。".to_owned(),
            "This will close 3 tabs, including 1 terminal. Terminal tabs that fail to close will stay open."
                .to_owned(),
        )
    );
}

/// 共享工作台关闭说明明确只收起视图，避免用户误以为已有传输任务会被终止。
///
/// @author kongweiguang
#[test]
fn close_confirmation_for_shared_workbench_preserves_running_transfers() {
    let tabs = WorkspaceTabs::new(vec![close_target(
        "sftp-workbench",
        WorkspaceTabKind::SftpWorkbench,
    )]);
    let tab_ids = vec![TabId::parse("sftp-workbench").expect("valid tab ID")];

    assert_eq!(
        tabs.close_confirmation_description(&tab_ids, 0),
        (
            "将关闭共享 SFTP 工作台视图，已有传输任务会继续运行。".to_owned(),
            "This closes the shared SFTP workbench view; existing transfers keep running."
                .to_owned(),
        )
    );
}

/// Exercise the production alert callbacks through Base's real Confirm/Cancel actions;
/// callback return values alone cannot detect a second pop or delayed focus restoration.
///
/// @author kongweiguang
mod close_confirmation_gpui {
    use super::*;
    use crate::features::workspace_tabs::WorkspaceTabCloseConfirmationFocusPhase;
    use gpui::component::{
        WindowExt as _,
        dialog::{Cancel, Confirm},
    };
    use gpui::{
        AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
        ParentElement as _, Render, Styled as _, Subscription, TestAppContext, VisualTestContext,
        Window, div,
    };
    use gpui_kit as gpui;
    use std::{cell::RefCell, rc::Rc, time::Duration};

    struct ConfirmationHarness {
        tabs: Entity<WorkspaceTabs>,
        tab_id: TabId,
        before: FocusHandle,
        later: FocusHandle,
        phases: Rc<RefCell<Vec<WorkspaceTabCloseConfirmationFocusPhase>>>,
        events: Rc<RefCell<Vec<WorkspaceTabEvent>>>,
        _event_subscription: Subscription,
    }

    impl ConfirmationHarness {
        /// Keep both focus nodes mounted while observing the real tab event emitter;
        /// the fixture intentionally does not emulate Root's asynchronous close reducer.
        /// 夹具显式保留单标签顺序，避免关闭确认测试构造出与投影不一致的焦点集合。
        ///
        /// @author kongweiguang
        fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
            let tab_id = TabId::new();
            let tabs = cx.new(|_| {
                WorkspaceTabs::new(vec![WorkspaceTabProjection {
                    id: tab_id.clone(),
                    title: "Terminal".into(),
                    group_id: None,
                    active: true,
                    kind: WorkspaceTabKind::Terminal,
                    layout: None,
                }])
            });
            let phases = Rc::new(RefCell::new(Vec::new()));
            let recorded_phases = phases.clone();
            tabs.update(cx, |tabs, _| {
                tabs.close_confirmation_focus_hook = Some(Rc::new(move |phase, _, _, _, _| {
                    recorded_phases.borrow_mut().push(phase);
                }));
            });
            let events = Rc::new(RefCell::new(Vec::new()));
            let recorded_events = events.clone();
            let event_subscription =
                cx.subscribe(&tabs, move |_, _, event: &WorkspaceTabEvent, _| {
                    recorded_events.borrow_mut().push(event.clone());
                });
            let before = cx.focus_handle();
            before.focus(window, cx);
            Self {
                tabs,
                tab_id: tab_id.clone(),
                before,
                later: cx.focus_handle(),
                phases,
                events,
                _event_subscription: event_subscription,
            }
        }
    }

    impl Render for ConfirmationHarness {
        /// A real dispatch tree is necessary: the test must prove that the mounted
        /// dialog consumes Confirm, rather than call the on_ok closure directly.
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(div().size_0().track_focus(&self.before))
                .child(div().size_0().track_focus(&self.later))
        }
    }

    /// Mount the same Kit Root/plugin combination used by gpui_kit::open_window;
    /// reduced entrance motion does not disable Kit's separate 250ms restore task.
    fn fixture(cx: &mut TestAppContext) -> (Entity<ConfirmationHarness>, &mut VisualTestContext) {
        cx.update(|app| {
            gpui::init(app);
            app.set_reduce_motion(true);
        });
        let slot = Rc::new(RefCell::new(None));
        let returned = slot.clone();
        let (_, visual) = cx.add_window_view(move |window, cx| {
            let harness = cx.new(|cx| ConfirmationHarness::new(window, cx));
            *returned.borrow_mut() = Some(harness.clone());
            gpui::base::Root::new(harness, window, cx)
        });
        let harness = slot.borrow_mut().take().expect("mounted harness");
        draw_dialog(visual);
        (harness, visual)
    }

    /// Render pending layer changes without advancing the executor's virtual clock,
    /// preserving the race between the immediate handoff and Kit's restore timer.
    fn draw_dialog(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear(app));
        cx.update(|window, app| window.draw(app).clear(app));
    }

    /// Open through WorkspaceTabs so the test covers its actual lifecycle hooks
    /// and event callback, including its choice to own or defer dialog dismissal.
    fn open_confirmation(harness: &Entity<ConfirmationHarness>, cx: &mut VisualTestContext) {
        cx.update(|window, app| {
            harness.update(app, |harness, cx| {
                let tab_id = harness.tab_id.clone();
                harness
                    .tabs
                    .update(cx, |tabs, cx| tabs.request_close_tab(tab_id, window, cx));
            });
            assert!(window.has_active_dialog(app));
        });
        draw_dialog(cx);
    }

    /// The formerly failing path restores a newer Root/pane focus at 250ms;
    /// asserting immediately and after that deadline distinguishes both states.
    #[gpui::test]
    fn confirm_emits_once_and_keeps_later_focus_after_animation(cx: &mut TestAppContext) {
        let (harness, cx) = fixture(cx);
        open_confirmation(&harness, cx);
        cx.dispatch_action(Confirm { secondary: false });
        cx.run_until_parked();
        cx.update(|window, app| {
            assert!(!window.has_active_dialog(app));
            let later = harness.read_with(app, |harness, _| {
                assert_eq!(
                    *harness.phases.borrow(),
                    vec![
                        WorkspaceTabCloseConfirmationFocusPhase::Requested,
                        WorkspaceTabCloseConfirmationFocusPhase::Confirmed
                    ],
                );
                assert_eq!(
                    *harness.events.borrow(),
                    vec![WorkspaceTabEvent::Close(harness.tab_id.clone())]
                );
                assert_eq!(window.focused(app).as_ref(), Some(&harness.before));
                harness.later.clone()
            });
            window.focus(&later, app);
        });
        cx.executor().advance_clock(Duration::from_millis(300));
        cx.run_until_parked();
        cx.update(|window, app| {
            harness.read_with(app, |harness, _| {
                assert_eq!(window.focused(app).as_ref(), Some(&harness.later));
                assert_eq!(harness.events.borrow().len(), 1);
            });
        });
    }

    /// Cancellation must retain Kit's ordinary synchronous Cancel path and never
    /// publish the resource-close event that only confirmation authorizes.
    #[gpui::test]
    fn cancel_restores_original_focus_without_close_event(cx: &mut TestAppContext) {
        let (harness, cx) = fixture(cx);
        open_confirmation(&harness, cx);
        cx.dispatch_action(Cancel);
        cx.run_until_parked();
        cx.update(|window, app| {
            assert!(!window.has_active_dialog(app));
            harness.read_with(app, |harness, _| {
                assert_eq!(
                    *harness.phases.borrow(),
                    vec![
                        WorkspaceTabCloseConfirmationFocusPhase::Requested,
                        WorkspaceTabCloseConfirmationFocusPhase::Cancelled
                    ],
                );
                assert!(harness.events.borrow().is_empty());
                assert_eq!(window.focused(app).as_ref(), Some(&harness.before));
            });
        });
    }

    /// Returning true after an explicit close would pop the underlying dialog;
    /// its survival proves the confirmation callback owns exactly one dismissal.
    #[gpui::test]
    fn confirm_pops_only_its_own_nested_dialog(cx: &mut TestAppContext) {
        let (harness, cx) = fixture(cx);
        cx.update(|window, app| {
            window.open_dialog(app, |dialog, _, _| dialog.title("Underlying dialog"));
        });
        draw_dialog(cx);
        let underlying_focus =
            cx.update(|window, app| window.focused(app).expect("underlying focus"));
        open_confirmation(&harness, cx);
        cx.dispatch_action(Confirm { secondary: false });
        cx.run_until_parked();
        cx.update(|window, app| {
            assert!(
                window.has_active_dialog(app),
                "the underlying dialog must not be popped"
            );
            assert_eq!(window.focused(app).as_ref(), Some(&underlying_focus));
            harness.read_with(app, |harness, _| {
                assert_eq!(harness.events.borrow().len(), 1)
            });
            window.close_dialog(app);
            assert!(
                !window.has_active_dialog(app),
                "only the underlying dialog remained"
            );
            harness.read_with(app, |harness, _| {
                assert_eq!(window.focused(app).as_ref(), Some(&harness.before));
            });
        });
    }
}

/// Render the production tab chrome and dispatch a real hit inside the measured tab box.
///
/// @author kongweiguang
mod workspace_tabs_gpui_chrome {
    use super::*;
    use gpui::{
        AppContext as _, Context, Entity, IntoElement, Modifiers, ParentElement as _, Render,
        Styled as _, Subscription, TestAppContext, VisualTestContext, Window, div, point, size,
    };
    use gpui_kit as gpui;
    use std::{cell::RefCell, rc::Rc};

    struct ChromeHarness {
        tabs: Entity<WorkspaceTabs>,
        events: Rc<RefCell<Vec<WorkspaceTabEvent>>>,
        tab_ids: Vec<TabId>,
        _event_subscription: Subscription,
    }

    impl ChromeHarness {
        /// Build multiple stable test tabs so keyboard navigation exercises real horizontal overflow.
        ///
        /// @author kongweiguang
        fn with_tab_count(cx: &mut Context<Self>, count: usize) -> Self {
            let tab_ids = (0..count)
                .map(|index| {
                    let id = match index {
                        0 => "tab-chrome-first".to_owned(),
                        1 => "tab-chrome-second".to_owned(),
                        _ => format!("tab-chrome-overflow-{index}"),
                    };
                    TabId::parse(&id).expect("valid test tab ID")
                })
                .collect::<Vec<_>>();
            let projection = tab_ids
                .iter()
                .enumerate()
                .map(|(index, id)| WorkspaceTabProjection {
                    id: id.clone(),
                    title: match index {
                        0 => "Selected terminal".into(),
                        1 => "Target terminal".into(),
                        _ => format!("Overflow terminal {index}").into(),
                    },
                    group_id: None,
                    active: index == 0,
                    kind: WorkspaceTabKind::Terminal,
                    layout: None,
                })
                .collect();
            let tabs = cx.new(|_| WorkspaceTabs::new(projection));
            let events = Rc::new(RefCell::new(Vec::new()));
            let recorded_events = events.clone();
            let event_subscription =
                cx.subscribe(&tabs, move |_, _, event: &WorkspaceTabEvent, _| {
                    recorded_events.borrow_mut().push(event.clone());
                });
            Self {
                tabs,
                events,
                tab_ids,
                _event_subscription: event_subscription,
            }
        }
    }

    impl Render for ChromeHarness {
        /// The actual WorkspaceTabs entity is mounted so element hitboxes and click bubbling are exercised.
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.tabs.clone())
        }
    }

    /// Mount a real GPUI dispatch tree with the ordinary two-tab projection.
    ///
    /// @author kongweiguang
    fn fixture(cx: &mut TestAppContext) -> (Entity<ChromeHarness>, &mut VisualTestContext) {
        fixture_with_tab_count(cx, 2)
    }

    /// Mount the production tab view with a caller-selected count for real overflow and focus tests.
    ///
    /// @author kongweiguang
    fn fixture_with_tab_count(
        cx: &mut TestAppContext,
        count: usize,
    ) -> (Entity<ChromeHarness>, &mut VisualTestContext) {
        cx.update(|app| {
            gpui::init(app);
            app.set_reduce_motion(true);
        });
        let slot = Rc::new(RefCell::new(None));
        let returned = slot.clone();
        let (_, visual) = cx.add_window_view(move |window, cx| {
            let harness = cx.new(|cx| ChromeHarness::with_tab_count(cx, count));
            *returned.borrow_mut() = Some(harness.clone());
            gpui::base::Root::new(harness, window, cx)
        });
        let harness = slot.borrow_mut().take().expect("mounted chrome harness");
        draw(visual);
        (harness, visual)
    }

    /// Draw after dispatch so next-frame real bounds and the scheduled minimal reveal settle together.
    /// 测试窗口没有平台帧循环，因此只显式交付两次实际绘制后排队的 GPUI 帧回调，并在两帧间排空任务；固定上限避免用无界补帧掩盖生产侧漏调度。
    ///
    /// @author kongweiguang
    fn draw(visual: &mut VisualTestContext) {
        visual.run_until_parked();
        visual.update(|window, app| {
            window.draw(app).clear(app);
            window.simulate_next_frame(app);
        });
        visual.run_until_parked();
        visual.update(|window, app| {
            window.draw(app).clear(app);
            window.simulate_next_frame(app);
        });
        visual.run_until_parked();
    }

    /// Home/End move real GPUI focus to clipped tabs and reveal them from their measured bounds, even when Home targets the already-focused tab.
    /// Modified chords remain unhandled so Alt+Space reaches the window system.
    /// 固定 12 个测试标签对应静态 selector，满足 GPUI 测试查询的静态生命周期约束。
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn keyboard_home_end_reveal_focused_tabs_inside_overflow_viewport(cx: &mut TestAppContext) {
        let (harness, visual) = fixture_with_tab_count(cx, 12);
        visual.simulate_resize(size(px(480.), px(160.)));
        draw(visual);

        let (first_tab_id, last_tab_id) = visual.update(|_, app| {
            harness.read_with(app, |harness, _| {
                (
                    harness.tab_ids.first().expect("first tab").clone(),
                    harness.tab_ids.last().expect("last tab").clone(),
                )
            })
        });
        const FIRST_SELECTOR: &str = "workspace-tab-hitbox-tab-chrome-first";
        const LAST_SELECTOR: &str = "workspace-tab-hitbox-tab-chrome-overflow-11";
        let viewport_before = visual.update(|_, app| {
            harness.read_with(app, |harness, app| {
                harness
                    .tabs
                    .read_with(app, |tabs, _| tabs.scroll_handle.bounds())
            })
        });
        let first_before = visual
            .debug_bounds(FIRST_SELECTOR)
            .expect("first tab is measured");
        let last_before = visual
            .debug_bounds(LAST_SELECTOR)
            .expect("last tab is measured before scrolling");
        assert!(
            last_before.right() > viewport_before.right(),
            "fixture must begin with the final tab clipped by real horizontal overflow"
        );

        visual.simulate_click(
            point(
                first_before.origin.x + first_before.size.width / 2.,
                first_before.origin.y + first_before.size.height / 2.,
            ),
            Modifiers::default(),
        );
        draw(visual);
        visual.update(|_, app| {
            harness.update(app, |harness, _| harness.events.borrow_mut().clear());
        });

        visual.simulate_keystrokes("end");
        draw(visual);
        let viewport_at_end = visual.update(|_, app| {
            harness.read_with(app, |harness, app| {
                harness
                    .tabs
                    .read_with(app, |tabs, _| tabs.scroll_handle.bounds())
            })
        });
        let last_at_end = visual
            .debug_bounds(LAST_SELECTOR)
            .expect("last tab remains mounted after End");
        let end_reveal_diagnostics = visual.update(|window, app| {
            harness.read_with(app, |harness, app| {
                harness.tabs.read_with(app, |tabs, _| {
                    (
                        tabs.scroll_handle.offset(),
                        tabs.scroll_handle.max_offset(),
                        tabs.roving_focus_tab_id.clone(),
                        tabs.tab_focus_handles
                            .get(&last_tab_id)
                            .is_some_and(|focus| focus.is_focused(window)),
                        tabs.pending_tab_reveal.as_ref().map(|pending| {
                            (
                                pending.tab_id.clone(),
                                pending.generation,
                                pending.expected_offset,
                                pending.reason,
                            )
                        }),
                    )
                })
            })
        });
        assert!(
            end_reveal_diagnostics.4.is_none(),
            "the scheduled End reveal must be consumed by a delivered GPUI frame: target={last_tab_id:?}, pending={:?}",
            end_reveal_diagnostics.4
        );
        assert!(
            last_at_end.left() >= viewport_at_end.left()
                && last_at_end.right() <= viewport_at_end.right(),
            "End must reveal the focused tab completely within the viewport: target={last_tab_id:?}, measured={last_at_end:?}, viewport={viewport_at_end:?}, offset={:?}, max_offset={:?}, roving_focus={:?}, target_focused={}, pending={:?}",
            end_reveal_diagnostics.0,
            end_reveal_diagnostics.1,
            end_reveal_diagnostics.2,
            end_reveal_diagnostics.3,
            end_reveal_diagnostics.4
        );
        let end_focus_is_current = visual.update(|window, app| {
            harness.read_with(app, |harness, app| {
                harness.tabs.read_with(app, |tabs, _| {
                    tabs.roving_focus_tab_id.as_ref() == Some(&last_tab_id)
                        && tabs
                            .tab_focus_handles
                            .get(&last_tab_id)
                            .is_some_and(|focus| focus.is_focused(window))
                })
            })
        });
        assert!(end_focus_is_current, "End must focus the final visible tab");
        let events_after_end = visual
            .update(|_, app| harness.read_with(app, |harness, _| harness.events.borrow().clone()));
        assert!(
            events_after_end.is_empty(),
            "roving focus does not activate the tab"
        );

        visual.simulate_keystrokes("home");
        draw(visual);
        let viewport_at_home = visual.update(|_, app| {
            harness.read_with(app, |harness, app| {
                harness
                    .tabs
                    .read_with(app, |tabs, _| tabs.scroll_handle.bounds())
            })
        });
        let first_at_home = visual
            .debug_bounds(FIRST_SELECTOR)
            .expect("first tab remains mounted after Home");
        assert!(
            first_at_home.left() >= viewport_at_home.left()
                && first_at_home.right() <= viewport_at_home.right(),
            "Home must reveal the focused tab completely within the viewport"
        );
        let home_focus_is_current = visual.update(|window, app| {
            harness.read_with(app, |harness, app| {
                harness.tabs.read_with(app, |tabs, _| {
                    tabs.roving_focus_tab_id.as_ref() == Some(&first_tab_id)
                        && tabs
                            .tab_focus_handles
                            .get(&first_tab_id)
                            .is_some_and(|focus| focus.is_focused(window))
                })
            })
        });
        assert!(
            home_focus_is_current,
            "Home must focus the first visible tab"
        );

        visual.update(|_, app| {
            let tabs = harness.read(app).tabs.clone();
            tabs.update(app, |tabs, _| {
                let max_offset = tabs.scroll_handle.max_offset();
                tabs.scroll_handle
                    .set_offset(gpui::point(-max_offset.x, px(0.)));
            });
        });
        draw(visual);
        let viewport_after_manual_scroll = visual.update(|_, app| {
            harness.read_with(app, |harness, app| {
                harness
                    .tabs
                    .read_with(app, |tabs, _| tabs.scroll_handle.bounds())
            })
        });
        let first_after_manual_scroll = visual
            .debug_bounds(FIRST_SELECTOR)
            .expect("focused first tab remains mounted after manual scroll");
        assert!(
            first_after_manual_scroll.left() < viewport_after_manual_scroll.left(),
            "manual scrolling must move the already-focused first tab out of view"
        );

        visual.simulate_keystrokes("home");
        draw(visual);
        let viewport_after_same_tab_home = visual.update(|_, app| {
            harness.read_with(app, |harness, app| {
                harness
                    .tabs
                    .read_with(app, |tabs, _| tabs.scroll_handle.bounds())
            })
        });
        let first_after_same_tab_home = visual
            .debug_bounds(FIRST_SELECTOR)
            .expect("focused first tab remains mounted after repeated Home");
        assert!(
            first_after_same_tab_home.left() >= viewport_after_same_tab_home.left()
                && first_after_same_tab_home.right() <= viewport_after_same_tab_home.right(),
            "Home must reveal a target that is already focused after manual scrolling"
        );
        let same_tab_home_kept_focus = visual.update(|window, app| {
            harness.read_with(app, |harness, app| {
                harness.tabs.read_with(app, |tabs, _| {
                    tabs.roving_focus_tab_id.as_ref() == Some(&first_tab_id)
                        && tabs
                            .tab_focus_handles
                            .get(&first_tab_id)
                            .is_some_and(|focus| focus.is_focused(window))
                })
            })
        });
        assert!(
            same_tab_home_kept_focus,
            "Home should retain the first tab focus"
        );

        visual.simulate_keystrokes("shift-end alt-space");
        draw(visual);
        let focus_after_modified_keys = visual.update(|window, app| {
            harness.read_with(app, |harness, app| {
                let roving_focus_is_first = harness.tabs.read_with(app, |tabs, _| {
                    tabs.roving_focus_tab_id.as_ref() == Some(&first_tab_id)
                });
                let events_are_empty = harness.events.borrow().is_empty();
                roving_focus_is_first
                    && events_are_empty
                    && harness.tabs.read_with(app, |tabs, _| {
                        tabs.tab_focus_handles
                            .get(&first_tab_id)
                            .is_some_and(|focus| focus.is_focused(window))
                    })
            })
        });
        assert!(
            focus_after_modified_keys,
            "Shift navigation and Alt+Space must not be consumed as local tab actions"
        );
    }

    /// A click at the measured label center activates that TabId; the inset active face does not expand the label or overlap sibling hitboxes.
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn tab_chrome_hitbox_activates_target_without_expanding_active_body(cx: &mut TestAppContext) {
        let (harness, visual) = fixture(cx);
        let active_box = visual
            .debug_bounds("workspace-tab-hitbox-tab-chrome-first")
            .expect("active tab outer hitbox is measured");
        let active_body = visual
            .debug_bounds("workspace-tab-active-body-tab-chrome-first")
            .expect("active tab body is measured");
        let target_box = visual
            .debug_bounds("workspace-tab-hitbox-tab-chrome-second")
            .expect("target tab hitbox is measured");
        let active_close = visual
            .debug_bounds("workspace-tab-close-tab-chrome-first")
            .expect("active close button has a stable hitbox");
        let target_close = visual
            .debug_bounds("workspace-tab-close-tab-chrome-second")
            .expect("inactive close button has a stable hitbox");
        let create_button = visual
            .debug_bounds("workspace-tabs-create-terminal")
            .expect("new tab button is measured");
        let trailing_drag_space = visual
            .debug_bounds("workspace-tabs-trailing-drag-space")
            .expect("unused viewport space has a dedicated hitbox");

        assert_eq!(active_box.size.width.as_f32(), 180.);
        assert!(
            (7.0..=10.0).contains(&(active_body.origin.x - active_box.origin.x).as_f32()),
            "the active face must remain inset within the label hitbox"
        );
        assert!(
            (160.0..=166.0).contains(&active_body.size.width.as_f32()),
            "the active face must be inset without shrinking the 180px tab hitbox"
        );
        let active_close_trailing_inset = active_box.origin.x + active_box.size.width
            - active_close.origin.x
            - active_close.size.width;
        let inactive_close_trailing_inset = target_box.origin.x + target_box.size.width
            - target_close.origin.x
            - target_close.size.width;
        assert!(
            (active_close_trailing_inset - inactive_close_trailing_inset)
                .as_f32()
                .abs()
                < 0.5,
            "active state must not shift the stable close-button target"
        );
        assert!(trailing_drag_space.size.width > px(0.));
        assert!(
            trailing_drag_space.origin.x >= create_button.origin.x + create_button.size.width,
            "the native drag area must start after the independent create button"
        );

        let click_position = point(
            target_box.origin.x + target_box.size.width / 2.,
            target_box.origin.y + target_box.size.height / 2.,
        );
        visual.simulate_click(click_position, Modifiers::default());
        visual.run_until_parked();

        visual.update(|_, app| {
            harness.read_with(app, |harness, _| {
                assert_eq!(
                    *harness.events.borrow(),
                    vec![WorkspaceTabEvent::Activate(
                        TabId::parse("tab-chrome-second").unwrap()
                    )]
                );
            });
        });
    }
}
