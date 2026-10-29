//! 文件列表右键菜单对单项、多选和过期目录快照的回归。
//!
//! @author kongweiguang

use gpui_kit::TestAppContext;
use kerminal_domain::{files::FileEntryKind, identity::HostId};

use super::*;
use crate::features::files::{action_test_support as support, selection::SelectionGesture};

/// 右键已选单项保留当前多选，右键未选项则只选中捕获的行。
///
/// @author kongweiguang
#[gpui_kit::test]
fn context_entry_preserves_selected_rows_and_replaces_with_unselected_target(
    app: &mut TestAppContext,
) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv");
    let first = support::remote_entry(&directory, "first", FileEntryKind::Directory);
    let second = support::remote_entry(&directory, "second", FileEntryKind::Directory);
    let third = support::remote_entry(&directory, "third", FileEntryKind::Directory);
    let ordered_names = vec![first.name.clone(), second.name.clone(), third.name.clone()];
    let seed_ordered_names = ordered_names.clone();
    let seed_first = first.clone();
    let seed_second = second.clone();
    let seed_third = third.clone();
    let fixture = support::entity_fixture(app, directory.clone(), move |pane| {
        support::seed_listing(
            pane,
            1,
            9,
            vec![seed_first.clone(), seed_second.clone(), seed_third],
        );
        pane.panels[1].selection.select_only("first");
        pane.panels[1]
            .selection
            .apply("second", &seed_ordered_names, SelectionGesture::Toggle);
        pane.panels[1].selected = Some(seed_first);
    });
    let _runtime = fixture.runtime;
    let pane = fixture.pane.clone();

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            assert!(select_context_entry(
                pane,
                1,
                first.clone(),
                &ordered_names,
                9,
                &directory,
                cx,
            ));
            assert_eq!(
                pane.selected_entries(1)
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .collect::<Vec<_>>(),
                ["first", "second"]
            );

            assert!(select_context_entry(
                pane,
                1,
                third.clone(),
                &ordered_names,
                9,
                &directory,
                cx,
            ));
            assert_eq!(
                pane.selected_entries(1)
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .collect::<Vec<_>>(),
                ["third"]
            );
        });
    });
}

/// 批量菜单只接受创建时完整匹配的 endpoint 集合，并拒绝迟到目录动作。
///
/// @author kongweiguang
#[gpui_kit::test]
fn context_selection_rejects_changed_selection_and_stale_snapshot(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv");
    let first = support::remote_entry(&directory, "first", FileEntryKind::Directory);
    let second = support::remote_entry(&directory, "second", FileEntryKind::Directory);
    let other = support::remote_entry(&directory, "other", FileEntryKind::Directory);
    let ordered_names = vec!["first".to_owned(), "second".to_owned(), "other".to_owned()];
    let seed_first = first.clone();
    let seed_second = second.clone();
    let seed_other = other.clone();
    let fixture = support::entity_fixture(app, directory.clone(), move |pane| {
        support::seed_listing(
            pane,
            1,
            4,
            vec![seed_first.clone(), seed_second.clone(), seed_other],
        );
        pane.panels[1].selection.select_only("first");
        pane.panels[1]
            .selection
            .apply("second", &ordered_names, SelectionGesture::Toggle);
    });
    let _runtime = fixture.runtime;
    let pane = fixture.pane.clone();

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            assert!(select_context_selection(
                pane,
                1,
                &[first.clone(), second.clone()],
                4,
                &directory,
                cx,
            ));
            pane.panels[1].selection.select_only("other");
            assert!(!select_context_selection(
                pane,
                1,
                &[first.clone(), second.clone()],
                4,
                &directory,
                cx,
            ));
            assert!(!select_context_selection(
                pane,
                1,
                &[other],
                3,
                &directory,
                cx,
            ));
        });
    });
}
