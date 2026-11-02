use super::*;

/// 为选择模型测试构造稳定的可见名称顺序，不依赖文件系统排序状态。
///
/// @author kongweiguang
fn names(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

/// 同时检查私有集合基数与生产行命中结果，确保未列出的选择也会被测试发现。
///
/// @author kongweiguang
fn assert_selected(selection: &FileSelection, candidates: &[String], expected: &[&str]) {
    assert_eq!(selection.names.len(), expected.len(), "selected item count");
    for name in candidates {
        assert_eq!(
            selection.contains(name),
            expected.contains(&name.as_str()),
            "selection membership for {name:?}"
        );
    }
}

/// 范围选择以可见排序作为边界，反向选择仍包含锚点与目标两端。
#[test]
fn shift_selection_includes_both_endpoints_in_visual_order() {
    let visible = names(&["a", "b", "c", "d", "e"]);
    let mut selection = FileSelection::default();
    selection.apply("d", &visible, SelectionGesture::Replace);
    selection.apply("b", &visible, SelectionGesture::Extend);

    assert_selected(&selection, &visible, &["b", "c", "d"]);
}

/// Ctrl/Command 切换不会破坏其他选择，Shift 增量范围也保留范围外项目。
#[test]
fn additive_selection_preserves_existing_items() {
    let visible = names(&["a", "b", "c", "d"]);
    let mut selection = FileSelection::default();
    selection.apply("a", &visible, SelectionGesture::Replace);
    selection.apply("d", &visible, SelectionGesture::Toggle);
    selection.apply("b", &visible, SelectionGesture::AddRange);

    assert_selected(&selection, &visible, &["a", "b", "c", "d"]);
}

/// 全选只接收当前视图投影的行；清除后不遗留范围锚点或隐式选择。
#[test]
fn select_all_and_clear_are_scoped_to_visible_rows() {
    let visible = names(&["alpha", "beta"]);
    let mut selection = FileSelection::default();
    selection.select_all(&visible);

    let candidates = names(&["alpha", "beta", ".hidden"]);
    assert_selected(&selection, &candidates, &["alpha", "beta"]);

    selection.clear();
    assert_selected(&selection, &candidates, &[]);
    assert!(selection.names.is_empty());
    selection.apply("beta", &visible, SelectionGesture::Extend);
    assert_selected(&selection, &candidates, &["beta"]);
}

/// 隐藏过滤或目录刷新移除不可见名称，旧锚点不会跨集合继续扩展范围。
#[test]
fn pruning_invalidates_stale_names_and_range_anchor() {
    let original = names(&["a", ".hidden", "b"]);
    let mut selection = FileSelection::default();
    selection.apply(".hidden", &original, SelectionGesture::Replace);
    let visible = names(&["a", "b"]);
    selection.retain_visible(&visible);
    selection.apply("b", &visible, SelectionGesture::Extend);

    assert_selected(&selection, &original, &["b"]);
}

/// 键盘活动项在首尾钳制；Shift 扩展始终沿用最初 Replace 项作为范围锚点。
///
/// @author kongweiguang
#[test]
fn keyboard_cursor_clamps_and_shift_range_keeps_anchor() {
    let directory = kerminal_domain::files::FileEndpoint::remote(
        kerminal_domain::identity::HostId::new(),
        "/srv",
    )
    .expect("remote directory endpoint is valid");
    let visible = names(&["alpha", "beta", "gamma"]);
    let mut focus = super::super::FileListKeyboardFocus::default();
    let mut selection = FileSelection::default();

    let alpha = focus
        .move_active(&directory, &visible, super::super::FileListMovement::Next)
        .expect("non-empty list has a first item");
    assert_eq!(alpha, 0);
    selection.apply(&visible[alpha], &visible, SelectionGesture::Replace);

    let beta = focus
        .move_active(&directory, &visible, super::super::FileListMovement::Next)
        .expect("second item exists");
    selection.apply(&visible[beta], &visible, SelectionGesture::Extend);
    let gamma = focus
        .move_active(&directory, &visible, super::super::FileListMovement::Next)
        .expect("third item exists");
    selection.apply(&visible[gamma], &visible, SelectionGesture::Extend);
    assert_selected(&selection, &visible, &["alpha", "beta", "gamma"]);

    assert_eq!(
        focus.move_active(&directory, &visible, super::super::FileListMovement::Next,),
        Some(2),
        "Down at the final row must not wrap"
    );
    let beta = focus
        .move_active(
            &directory,
            &visible,
            super::super::FileListMovement::Previous,
        )
        .expect("previous row exists");
    selection.apply(&visible[beta], &visible, SelectionGesture::Extend);
    assert_selected(&selection, &visible, &["alpha", "beta"]);
    assert_eq!(focus.active_name(&directory, &visible), Some("beta"));
}

/// 翻页按可视行数留一行重叠，反向移动可回到相同活动项且两端不会环绕。
///
/// @author kongweiguang
#[test]
fn keyboard_page_movement_overlaps_one_row_and_clamps_at_boundaries() {
    let directory = kerminal_domain::files::FileEndpoint::remote(
        kerminal_domain::identity::HostId::new(),
        "/srv",
    )
    .expect("remote directory endpoint is valid");
    let visible = (0..12)
        .map(|index| format!("item-{index}"))
        .collect::<Vec<_>>();
    let mut focus = super::super::FileListKeyboardFocus::default();

    assert_eq!(
        focus.move_active(&directory, &visible, super::super::FileListMovement::First),
        Some(0)
    );
    assert_eq!(
        focus.move_active(
            &directory,
            &visible,
            super::super::FileListMovement::PageNext { visible_rows: 0 },
        ),
        Some(1),
        "a viewport shorter than one row still advances",
    );
    assert_eq!(
        focus.move_active(
            &directory,
            &visible,
            super::super::FileListMovement::PagePrevious { visible_rows: 1 },
        ),
        Some(0),
    );
    let page_down = || super::super::FileListMovement::PageNext { visible_rows: 5 };
    let page_up = || super::super::FileListMovement::PagePrevious { visible_rows: 5 };

    assert_eq!(
        focus.move_active(&directory, &visible, page_down()),
        Some(4)
    );
    assert_eq!(
        focus.move_active(&directory, &visible, page_down()),
        Some(8)
    );
    assert_eq!(focus.move_active(&directory, &visible, page_up()), Some(4));
    assert_eq!(focus.move_active(&directory, &visible, page_up()), Some(0));
    assert_eq!(focus.move_active(&directory, &visible, page_up()), Some(0));
    assert_eq!(
        focus.move_active(&directory, &visible, page_down()),
        Some(4)
    );

    assert_eq!(
        focus.move_active(&directory, &visible, super::super::FileListMovement::Last),
        Some(11)
    );
    assert_eq!(
        focus.move_active(&directory, &visible, page_down()),
        Some(11)
    );
    assert_eq!(focus.move_active(&directory, &visible, page_up()), Some(7));
}

/// Shift 翻页沿用列表原锚点，并在 PageUp 反向移动后重算到当前活动行的范围。
///
/// @author kongweiguang
#[test]
fn shift_page_movement_preserves_range_anchor_in_both_directions() {
    let directory = kerminal_domain::files::FileEndpoint::remote(
        kerminal_domain::identity::HostId::new(),
        "/srv",
    )
    .expect("remote directory endpoint is valid");
    let visible = names(&["a", "b", "c", "d", "e", "f", "g", "h"]);
    let mut focus = super::super::FileListKeyboardFocus::default();
    let mut selection = FileSelection::default();

    let first = focus
        .move_active(&directory, &visible, super::super::FileListMovement::First)
        .expect("non-empty list has a first item");
    selection.apply(&visible[first], &visible, SelectionGesture::Replace);

    let page_down = super::super::FileListMovement::PageNext { visible_rows: 4 };
    let third_page_row = focus
        .move_active(&directory, &visible, page_down)
        .expect("page movement has a target");
    selection.apply(&visible[third_page_row], &visible, SelectionGesture::Extend);
    assert_eq!(third_page_row, 3);

    let sixth = focus
        .move_active(&directory, &visible, page_down)
        .expect("second page movement has a target");
    selection.apply(&visible[sixth], &visible, SelectionGesture::Extend);
    assert_eq!(sixth, 6);

    let third_page_row = focus
        .move_active(
            &directory,
            &visible,
            super::super::FileListMovement::PagePrevious { visible_rows: 4 },
        )
        .expect("reverse page movement has a target");
    selection.apply(&visible[third_page_row], &visible, SelectionGesture::Extend);

    assert_selected(&selection, &visible, &["a", "b", "c", "d"]);
    assert_eq!(focus.active_name(&directory, &visible), Some("d"));
}

/// 路径或主机变化时相同 basename 不继承旧活动索引；刷新移除活动项也会清理身份。
///
/// @author kongweiguang
#[test]
fn keyboard_cursor_rebinds_to_directory_and_target_identity() {
    use kerminal_domain::{files::FileEndpoint, identity::HostId};

    let first_host = HostId::new();
    let original = FileEndpoint::remote(first_host.clone(), "/srv").expect("valid endpoint");
    let child_directory = FileEndpoint::remote(first_host, "/srv/child").expect("valid endpoint");
    let other_host = FileEndpoint::remote(HostId::new(), "/srv/child").expect("valid endpoint");
    let visible = names(&["first", "old"]);
    let mut focus = super::super::FileListKeyboardFocus::default();
    focus.set_active(&original, "old", &visible);
    assert_eq!(focus.active_name(&original, &visible), Some("old"));

    focus.retain_visible(&child_directory, &visible);
    assert_eq!(focus.active_name(&child_directory, &visible), None);
    assert_eq!(
        focus.move_active(
            &child_directory,
            &visible,
            super::super::FileListMovement::Next,
        ),
        Some(0),
        "same basename in a new path must not inherit the old cursor"
    );

    focus.set_active(&child_directory, "old", &visible);
    focus.retain_visible(&other_host, &visible);
    assert_eq!(focus.active_name(&other_host, &visible), None);
    assert_eq!(
        focus.move_active(&other_host, &visible, super::super::FileListMovement::Next,),
        Some(0),
        "same path on another host must begin at the first visible row"
    );

    let refreshed = names(&["first", "remaining"]);
    focus.set_active(&other_host, "remaining", &refreshed);
    focus.retain_visible(&other_host, &visible);
    assert_eq!(focus.active_name(&other_host, &visible), None);
}
