//! 文件树右键菜单的已选行保留、单行替换和批量快照校验。
//!
//! @author kongweiguang

use gpui_kit::TestAppContext;
use kerminal_domain::{files::FileEntryKind, identity::HostId};

use super::*;
use crate::features::files::{action_test_support as support, selection::SelectionGesture};

/// 树行菜单保留已选行上的多选，未选行右键时改为目标单选。
///
/// @author kongweiguang
#[gpui_kit::test]
fn tree_context_row_preserves_selected_rows_and_replaces_unselected_target(
    app: &mut TestAppContext,
) {
    let host = HostId::new();
    let root = support::remote_endpoint(&host, "/workspace");
    let first = support::remote_entry(&root, "first", FileEntryKind::Directory);
    let second = support::remote_entry(&root, "second", FileEntryKind::Directory);
    let third = support::remote_entry(&root, "third", FileEntryKind::Directory);
    let root_for_seed = root.clone();
    let seed_first = first.clone();
    let seed_second = second.clone();
    let seed_third = third.clone();
    let fixture = support::entity_fixture(app, root.clone(), move |pane| {
        pane.panels[1].generation = 6;
        pane.panels[1].tree.begin_root_request(&root_for_seed, 6);
        pane.panels[1].tree.set_root_listing(
            root_for_seed.clone(),
            6,
            vec![seed_first.clone(), seed_second.clone(), seed_third],
            true,
        );
        let visible = pane.panels[1].tree.visible_endpoints(true);
        pane.panels[1].tree.apply_selection(
            &seed_first.endpoint,
            &visible,
            SelectionGesture::Replace,
        );
        pane.panels[1].tree.apply_selection(
            &seed_second.endpoint,
            &visible,
            SelectionGesture::Toggle,
        );
    });
    let _runtime = fixture.runtime;
    let pane = fixture.pane.clone();

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            assert!(select_tree_row(pane, 1, &first, 6, &root, cx));
            assert_eq!(
                pane.panels[1]
                    .tree
                    .selected_entries(true)
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .collect::<Vec<_>>(),
                ["first", "second"]
            );

            assert!(select_tree_row(pane, 1, &third, 6, &root, cx));
            assert_eq!(
                pane.panels[1]
                    .tree
                    .selected_entries(true)
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .collect::<Vec<_>>(),
                ["third"]
            );
        });
    });
}

/// 树批量菜单要求 endpoint 顺序一致，并对旧面板代次 fail closed。
///
/// @author kongweiguang
#[gpui_kit::test]
fn tree_context_selection_rejects_reordered_or_stale_targets(app: &mut TestAppContext) {
    let host = HostId::new();
    let root = support::remote_endpoint(&host, "/workspace");
    let first = support::remote_entry(&root, "first", FileEntryKind::Directory);
    let second = support::remote_entry(&root, "second", FileEntryKind::Directory);
    let root_for_seed = root.clone();
    let seed_first = first.clone();
    let seed_second = second.clone();
    let fixture = support::entity_fixture(app, root.clone(), move |pane| {
        pane.panels[1].generation = 10;
        pane.panels[1].tree.begin_root_request(&root_for_seed, 10);
        pane.panels[1].tree.set_root_listing(
            root_for_seed.clone(),
            10,
            vec![seed_first.clone(), seed_second.clone()],
            true,
        );
        let visible = pane.panels[1].tree.visible_endpoints(true);
        pane.panels[1].tree.apply_selection(
            &seed_first.endpoint,
            &visible,
            SelectionGesture::Replace,
        );
        pane.panels[1].tree.apply_selection(
            &seed_second.endpoint,
            &visible,
            SelectionGesture::Toggle,
        );
    });
    let _runtime = fixture.runtime;
    let pane = fixture.pane.clone();

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            assert!(select_tree_selection(
                pane,
                1,
                &[first.clone(), second.clone()],
                10,
                &root,
                true,
                cx,
            ));
            assert!(!select_tree_selection(
                pane,
                1,
                &[second.clone(), first.clone()],
                10,
                &root,
                true,
                cx,
            ));
            assert!(!select_tree_selection(
                pane,
                1,
                &[first, second],
                9,
                &root,
                true,
                cx,
            ));
        });
    });
}
