//! 当前终端单栏文件目标同步与浏览历史边界回归。
//!
//! @author kongweiguang

use std::{cell::RefCell, path::PathBuf, rc::Rc};

use super::{
    CompactTargetUpdateAction, FilePaneSide, NavigationHistory, SftpHostChoice, SftpHostRequested,
    TransferRefreshScope, TransferSubmissionPhase, can_transfer_to_other_side,
    compact_target_update_action, should_apply_shared_host_local_start,
};
use crate::features::files::action_test_support as support;
use gpui_kit::{
    AppContext as _, Focusable as _, KeyUpEvent, Keystroke, MouseButton, ScrollDelta,
    ScrollWheelEvent, Subscription, TestAppContext, VisualTestContext, point, px, size,
};
use kerminal_domain::{connections::RemoteProtocol, files::FileEndpoint, identity::HostId};

/// 测试保留事件订阅，验证真实输入动作交给 Root 的精确目标。
struct HostSelectionObserver {
    _subscription: Subscription,
}

/// 为状态测试构造经过同一领域校验的远端目录 endpoint。
///
/// @author kongweiguang
fn remote_endpoint(host_id: &HostId, path: &str) -> FileEndpoint {
    FileEndpoint::remote(host_id.clone(), path).expect("valid remote directory")
}

/// SSH 跟随保持显式 opt-in；本机切换目录或 Tab 始终保留用户浏览路径。
///
/// @author kongweiguang
#[test]
fn compact_file_target_updates_preserve_or_follow_only_the_intended_path() {
    let host_a = HostId::new();
    let host_b = HostId::new();
    let terminal_a = remote_endpoint(&host_a, "/home/kong");
    let terminal_a_cwd = remote_endpoint(&host_a, "/srv/app");
    let browser_a = remote_endpoint(&host_a, "/srv/app/config");
    let terminal_b = remote_endpoint(&host_b, "/home/deploy");
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let local_browser = FileEndpoint::Local {
        path: PathBuf::from("C:\\work\\docs"),
    };
    let local_next = FileEndpoint::Local {
        path: PathBuf::from("C:\\Users\\24052"),
    };

    assert_eq!(
        compact_target_update_action(Some(&terminal_a), &browser_a, Some(&terminal_a), false),
        CompactTargetUpdateAction::Unchanged
    );
    assert_eq!(
        compact_target_update_action(Some(&terminal_a), &browser_a, Some(&terminal_a), true),
        CompactTargetUpdateAction::Unchanged
    );
    assert_eq!(
        compact_target_update_action(Some(&terminal_a), &browser_a, Some(&terminal_a_cwd), false,),
        CompactTargetUpdateAction::PreservePath
    );
    assert_eq!(
        compact_target_update_action(Some(&terminal_a), &browser_a, Some(&terminal_a_cwd), true,),
        CompactTargetUpdateAction::FollowDirectory
    );
    assert_eq!(
        compact_target_update_action(Some(&terminal_a), &browser_a, Some(&terminal_b), false,),
        CompactTargetUpdateAction::BindTarget
    );
    assert_eq!(
        compact_target_update_action(None, &browser_a, Some(&terminal_a), false),
        CompactTargetUpdateAction::BindTarget
    );
    assert_eq!(
        compact_target_update_action(Some(&terminal_a), &browser_a, None, true),
        CompactTargetUpdateAction::Unavailable
    );
    assert_eq!(
        compact_target_update_action(Some(&terminal_a), &browser_a, Some(&local), true),
        CompactTargetUpdateAction::BindTarget
    );
    assert_eq!(
        compact_target_update_action(None, &local, Some(&local), false),
        CompactTargetUpdateAction::BindTarget
    );
    assert_eq!(
        compact_target_update_action(Some(&local), &local_browser, Some(&local_next), false),
        CompactTargetUpdateAction::PreservePath
    );
    assert_eq!(
        compact_target_update_action(Some(&local), &local_browser, Some(&local_next), true),
        CompactTargetUpdateAction::PreservePath
    );
    assert_eq!(
        compact_target_update_action(Some(&local), &local_browser, Some(&terminal_a), true),
        CompactTargetUpdateAction::BindTarget
    );
}

/// 只有真正切换目录才离开初始路径，重复打开当前目录和重置连接都恢复初始状态。
///
/// @author kongweiguang
#[test]
fn navigation_history_tracks_departure_from_initial_path() {
    let host_id = HostId::new();
    let a = remote_endpoint(&host_id, "/srv/a");
    let b = remote_endpoint(&host_id, "/srv/b");
    let mut history = NavigationHistory::new(a.clone());

    assert!(history.is_initial());
    history.push(a.clone());
    assert!(history.is_initial());
    history.push(b.clone());
    assert!(!history.is_initial());
    history.reset(b.clone());
    assert!(history.is_initial());
}

/// 保存主机的本地起始目录只覆盖共享工作台的首个未浏览本机目标。
///
/// @author kongweiguang
#[test]
fn shared_host_local_start_preserves_manual_or_locked_left_targets() {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\Users\\24052"),
    };
    let remote = remote_endpoint(&HostId::new(), "/home/deploy");

    assert!(should_apply_shared_host_local_start(
        true, true, &local, true, false
    ));
    assert!(!should_apply_shared_host_local_start(
        false, true, &local, true, false
    ));
    assert!(!should_apply_shared_host_local_start(
        true, false, &local, true, false
    ));
    assert!(!should_apply_shared_host_local_start(
        true, true, &local, false, false
    ));
    assert!(!should_apply_shared_host_local_start(
        true, true, &local, true, true
    ));
    assert!(!should_apply_shared_host_local_start(
        true, true, &remote, true, false
    ));
}

/// 两侧任一未选择时都不能把本机占位当作复制来源或目标。
///
/// @author kongweiguang
#[test]
fn shared_workbench_copy_requires_explicit_right_target() {
    assert!(!can_transfer_to_other_side(true, [false, true], 0));
    assert!(can_transfer_to_other_side(true, [false, false], 0));
    assert!(can_transfer_to_other_side(false, [false, true], 0));
    assert!(!can_transfer_to_other_side(true, [false, true], 1));
    assert!(!can_transfer_to_other_side(true, [true, false], 0));
    assert!(!can_transfer_to_other_side(true, [true, false], 1));
}

/// 同一主机可以多次添加，每个标签切回时恢复自己的目录、历史和选择。
///
/// @author kongweiguang
#[gpui_kit::test]
fn duplicate_host_tabs_keep_independent_browser_state(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let host = HostId::new();
    let first_path = remote_endpoint(&host, "/srv/first");
    let second_path = remote_endpoint(&host, "/srv/second");
    let entry = support::remote_entry(
        &first_path,
        "file.txt",
        kerminal_domain::files::FileEntryKind::File,
    );
    let folder = support::remote_entry(
        &first_path,
        "nested",
        kerminal_domain::files::FileEntryKind::Directory,
    );
    let fixture = support::entity_fixture(app, local, |_| {});
    let pane = fixture.pane.clone();
    let _runtime = fixture.runtime;

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_host_picker_enabled(true, cx);
            pane.request_sftp_host(FilePaneSide::Left, host.clone(), cx);
            let generation = pane.host_picker.target_generations[0];
            assert!(pane.complete_host_selection(
                FilePaneSide::Left,
                generation,
                first_path.clone(),
                None,
                cx
            ));
            let first_id = pane.host_picker.host_tabs.active[0].expect("first tab");
            let panel_generation = pane.panels[0].generation;
            support::seed_listing(
                pane,
                0,
                panel_generation,
                vec![entry.clone(), folder.clone()],
            );
            pane.panels[0].requested_endpoint = None;
            pane.panels[0].selection.select_only("file.txt");
            pane.panels[0].selected = Some(entry.clone());
            pane.panels[0]
                .tree
                .begin_root_request(&first_path, panel_generation);
            pane.panels[0].tree.set_root_listing(
                first_path.clone(),
                panel_generation,
                vec![entry.clone(), folder.clone()],
                true,
            );

            pane.request_sftp_host(FilePaneSide::Left, host.clone(), cx);
            let generation = pane.host_picker.target_generations[0];
            assert!(pane.complete_host_selection(
                FilePaneSide::Left,
                generation,
                second_path.clone(),
                None,
                cx
            ));
            let second_id = pane.host_picker.host_tabs.active[0].expect("second tab");
            assert_ne!(first_id, second_id);
            assert_eq!(pane.panels[0].endpoint, second_path);

            pane.activate_host_tab(FilePaneSide::Left, first_id, cx);
            let current_generation = pane.panels[0].generation;
            assert!(
                pane.panels[0]
                    .tree
                    .begin_directory_request(&folder.endpoint, current_generation)
                    .is_some(),
                "恢复的树必须接受当前面板代次"
            );
            pane.activate_host_tab(FilePaneSide::Left, second_id, cx);

            pane.invalidate_inactive_transfer_directories(&[TransferRefreshScope::Directory(
                first_path.clone(),
            )]);
            pane.activate_host_tab(FilePaneSide::Left, first_id, cx);
            assert_eq!(pane.panels[0].endpoint, first_path);
            assert!(pane.panels[0].loading, "提交变更后的非活动目录需要回读");
            assert!(pane.panels[0].selection.contains("file.txt"));
            assert_eq!(
                pane.panels[0]
                    .selected
                    .as_ref()
                    .map(|item| item.name.as_str()),
                Some("file.txt")
            );
            pane.close_host_tab(FilePaneSide::Left, first_id, cx);
            assert_eq!(pane.host_picker.host_tabs.active[0], Some(0));
            assert!(matches!(
                pane.panels[0].endpoint,
                FileEndpoint::Local { .. }
            ));
        });
    });
}

/// 同主机标签的提示使用各自当前浏览目录，切换标签后不会回退到初始路径。
///
/// @author kongweiguang
#[gpui_kit::test]
fn duplicate_host_tab_details_follow_their_browser_directories(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let host = HostId::new();
    let initial = remote_endpoint(&host, "/srv/first");
    let second = remote_endpoint(&host, "/srv/second");
    let folder = support::remote_entry(
        &initial,
        "nested",
        kerminal_domain::files::FileEntryKind::Directory,
    );
    let fixture = support::entity_fixture(app, local, |_| {});
    let pane = fixture.pane.clone();
    let _runtime = fixture.runtime;

    app.update(|app| {
        pane.update(app, |pane, cx| {
            pane.set_host_picker_enabled(true, cx);
            pane.add_host_tab(FilePaneSide::Left, initial, cx);
            let first_id = pane.host_picker.host_tabs.active[0].expect("first tab");
            pane.activate_entry(0, folder.clone(), cx);
            let tab = pane.host_picker.host_tabs.sides[0]
                .iter()
                .find(|tab| tab.id == first_id)
                .expect("first tab");
            assert_eq!(tab.display_endpoint(&pane.panels[0]), &folder.endpoint);

            pane.add_host_tab(FilePaneSide::Left, second.clone(), cx);
            let first = pane.host_picker.host_tabs.sides[0]
                .iter()
                .find(|tab| tab.id == first_id)
                .expect("inactive first tab");
            assert_eq!(first.display_endpoint(&pane.panels[0]), &folder.endpoint);
            let active = pane.host_picker.host_tabs.sides[0]
                .iter()
                .find(|tab| Some(tab.id) == pane.host_picker.host_tabs.active[0])
                .expect("second tab");
            assert_eq!(active.display_endpoint(&pane.panels[0]), &second);
        });
    });
}

/// 两侧预检代次相互独立；切回本机使旧回执失效，关闭右侧最后标签恢复空态。
///
/// @author kongweiguang
#[gpui_kit::test]
fn switching_hosts_rejects_stale_receipts_and_preserves_other_side(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let left_host = HostId::new();
    let right_host = HostId::new();
    let left = remote_endpoint(&left_host, "/left");
    let right = remote_endpoint(&right_host, "/right");
    let fixture = support::entity_fixture(app, local, |_| {});
    let pane = fixture.pane.clone();
    let _runtime = fixture.runtime;

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_host_picker_enabled(true, cx);
            assert!(pane.host_picker.target_unselected[1]);
            pane.request_sftp_host(FilePaneSide::Left, left_host.clone(), cx);
            let left_generation = pane.host_picker.target_generations[0];
            pane.request_sftp_host(FilePaneSide::Right, right_host.clone(), cx);
            let right_generation = pane.host_picker.target_generations[1];
            pane.request_local_host(FilePaneSide::Left, cx);
            assert!(!pane.host_selection_is_current(FilePaneSide::Left, left_generation));
            assert!(pane.host_selection_is_current(FilePaneSide::Right, right_generation));
            assert!(!pane.complete_host_selection(
                FilePaneSide::Left,
                left_generation,
                left,
                None,
                cx
            ));
            assert!(pane.complete_host_selection(
                FilePaneSide::Right,
                right_generation,
                right.clone(),
                None,
                cx
            ));
            assert_eq!(pane.panels[1].endpoint, right);
            let right_id = pane.host_picker.host_tabs.active[1].expect("right host tab");
            pane.close_host_tab(FilePaneSide::Right, right_id, cx);
            assert!(pane.host_picker.target_unselected[1]);
            assert_eq!(pane.host_picker.host_tabs.active[1], None);
            assert!(pane.panels[1].listing.is_none());
        });
    });
}

/// 发起主机选择时只撤销关联侧的准备阶段；原目标和已经开始入队的批次继续保留。
///
/// @author kongweiguang
#[gpui_kit::test]
fn host_selection_cancels_only_related_unsubmitted_transfer(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let host = HostId::new();
    let fixture = support::entity_fixture(app, local, |_| {});
    let pane = fixture.pane.clone();
    let _runtime = fixture.runtime;

    app.update(|app| {
        pane.update(app, |pane, cx| {
            pane.set_host_picker_enabled(true, cx);
            pane.request_local_host(FilePaneSide::Right, cx);
            for side in [FilePaneSide::Left, FilePaneSide::Right] {
                let index = side.index();
                let endpoint = pane.panels[index].endpoint.clone();
                let active = pane.host_picker.host_tabs.active[index];
                let mut related = [false; 2];
                related[index] = true;
                let mut unrelated = [false; 2];
                unrelated[1 - index] = true;
                for create in [false, true] {
                    for (phase, sides, should_cancel) in [
                        (TransferSubmissionPhase::Preparing, related, true),
                        (TransferSubmissionPhase::Preparing, unrelated, false),
                        (TransferSubmissionPhase::Enqueuing, [true, true], false),
                    ] {
                        let (generation, cancellation) = pane
                            .begin_transfer_preparation(sides, cx)
                            .expect("transfer preparation");
                        pane.transfer_submission.as_mut().expect("submission").phase = phase;
                        if create {
                            pane.request_create_sftp_host(side, cx);
                        } else {
                            pane.request_sftp_host(side, host.clone(), cx);
                        }
                        assert_eq!(cancellation.is_cancelled(), should_cancel);
                        assert_eq!(pane.transfer_submission_busy, !should_cancel);
                        assert_eq!(pane.panels[index].endpoint, endpoint);
                        assert_eq!(pane.host_picker.host_tabs.active[index], active);
                        assert!(pane.host_selection_is_current(
                            side,
                            pane.host_picker.target_generations[index]
                        ));
                        if !should_cancel {
                            assert!(pane.transfer_submission.as_ref().is_some_and(|submission| {
                                submission.generation == generation && submission.phase == phase
                            }));
                        }
                        pane.end_transfer_submission();
                        pane.cancel_pending_host_selection(side, cx);
                    }
                }
            }
        });
    });
}

/// 旧锁定值只校验恢复输入；显式右侧空态不会触发占位本机的目录读取。
///
/// @author kongweiguang
#[gpui_kit::test]
fn restoring_unselected_right_keeps_it_empty_and_unlocks_left(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let host = HostId::new();
    let left = remote_endpoint(&host, "/srv");
    let fixture = support::entity_fixture(app, local, |_| {});
    let pane = fixture.pane.clone();
    let _runtime = fixture.runtime;

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.open_workspace_transfer(left, None, Some(host), None, cx)
                .expect("restored transfer");
            pane.set_host_picker_enabled(true, cx);
            assert!(pane.host_picker.target_unselected[1]);
            assert!(!pane.panels[1].loading);
            assert!(pane.panels[1].listing.is_none());
            assert!(pane.locked_left_host_id.is_none());
            pane.request_local_host(FilePaneSide::Left, cx);
            assert!(matches!(
                pane.panels[0].endpoint,
                FileEndpoint::Local { .. }
            ));
        });
    });
}

/// 搜索输入保留点击语义；首次点击只读取一次，外点和 Escape 仍可关闭。
///
/// @author kongweiguang
#[gpui_kit::test]
fn host_search_input_pointer_and_escape_follow_the_visible_popover(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let (runtime, pane, _, visual) = support::window_fixture(app, local, |_| {});
    visual.update(|_, app| pane.update(app, |pane, cx| pane.set_host_picker_enabled(true, cx)));
    support::draw(visual);
    let trigger = visual
        .update(|window, _| window.debug_selector_bounds("files-sftp-host-picker-toggle-0"))
        .expect("left host search rendered")
        .center();
    let initial_generation = visual.update(|_, app| pane.read(app).host_picker.generation);
    click_host_search(visual, trigger);
    assert_eq!(
        visual.update(|_, app| pane.read(app).host_picker.generation),
        initial_generation + 1,
        "鼠标展开与输入框取得焦点不能重复重启主机目录读取"
    );
    assert_eq!(
        visual.update(|_, app| pane.read(app).host_picker.expanded_side),
        Some(FilePaneSide::Left)
    );
    click_host_search(visual, trigger);
    assert_eq!(
        visual.update(|_, app| pane.read(app).host_picker.expanded_side),
        Some(FilePaneSide::Left),
        "点击输入框内改变光标不能关闭结果"
    );
    click_host_search(visual, point(px(4.), px(4.)));
    assert_eq!(
        visual.update(|_, app| pane.read(app).host_picker.expanded_side),
        None
    );
    click_host_search(visual, trigger);
    assert_eq!(
        visual.update(|_, app| pane.read(app).host_picker.expanded_side),
        Some(FilePaneSide::Left),
        "输入框仍持有焦点时再次点击也可展开"
    );
    visual.simulate_keystrokes("escape");
    support::draw(visual);
    assert_eq!(
        visual.update(|_, app| pane.read(app).host_picker.expanded_side),
        None
    );
    let _runtime = runtime;
}

/// 工作区按实际可用宽度布局；缩小后两侧主机条和搜索框始终位于可见区域。
///
/// @author kongweiguang
#[gpui_kit::test]
fn transfer_workspace_keeps_both_hosts_visible_when_narrow(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let (runtime, pane, _, visual) = support::window_fixture(app, local, |_| {});
    visual.update(|_, app| {
        pane.update(app, |pane, cx| {
            pane.set_host_picker_enabled(true, cx);
            pane.request_local_host(FilePaneSide::Right, cx);
        })
    });
    for (width, stacked) in [(448., false), (240., true)] {
        visual.simulate_resize(size(px(width), px(720.)));
        support::draw(visual);
        support::draw(visual);
        let (left, right, left_search, right_search) = visual.update(|window, _| {
            (
                window
                    .debug_selector_bounds("files-host-strip-0")
                    .expect("left strip"),
                window
                    .debug_selector_bounds("files-host-strip-1")
                    .expect("right strip"),
                window
                    .debug_selector_bounds("files-sftp-host-picker-toggle-0")
                    .expect("left search"),
                window
                    .debug_selector_bounds("files-sftp-host-picker-toggle-1")
                    .expect("right search"),
            )
        });
        assert!(
            left.right() <= px(width) && right.right() <= px(width),
            "两侧不可超出窗口"
        );
        assert!(
            left_search.right() <= left.right() && right_search.right() <= right.right(),
            "搜索框不可裁出主机条"
        );
        if stacked {
            assert!(left.bottom() <= right.top(), "极窄时按上下布局");
        } else {
            assert!(left.right() <= right.left(), "足够宽时保持左右布局");
        }
    }
    let _runtime = runtime;
}

/// 长主机标签在两侧新增、缩放和键盘切换后仍完整露出，手动滚动不会被普通重绘抢回。
///
/// @author kongweiguang
#[gpui_kit::test]
fn host_tabs_reveal_active_and_keyboard_focus_in_narrow_workspace(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let host = HostId::new();
    let (runtime, pane, _, visual) = support::window_fixture(app, local, |_| {});
    visual.update(|window, _| window.activate_window());
    visual.simulate_resize(size(px(700.), px(720.)));
    let (left_ids, right_ids) = visual.update(|_, app| {
        pane.update(app, |pane, cx| {
            pane.set_host_picker_enabled(true, cx);
            pane.host_picker.hosts = vec![SftpHostChoice {
                id: host.clone(),
                name: "a very long saved host name for the transfer workspace".to_owned(),
                address: "127.0.0.1".to_owned(),
                username: "test".to_owned(),
                port: 22,
                protocol: RemoteProtocol::Ssh,
                group_id: None,
            }];
            let mut left_ids = Vec::new();
            let mut right_ids = Vec::new();
            for number in 0..4 {
                pane.add_host_tab(
                    FilePaneSide::Left,
                    remote_endpoint(&host, &format!("/left-{number}")),
                    cx,
                );
                left_ids.push(pane.host_picker.host_tabs.active[0].expect("left host tab"));
            }
            for number in 0..3 {
                pane.add_host_tab(
                    FilePaneSide::Right,
                    remote_endpoint(&host, &format!("/right-{number}")),
                    cx,
                );
                right_ids.push(pane.host_picker.host_tabs.active[1].expect("right host tab"));
            }
            (left_ids, right_ids)
        })
    });
    support::draw(visual);
    support::draw(visual);
    let left_active = *left_ids.last().expect("left active tab");
    let right_active = *right_ids.last().expect("right active tab");
    assert_host_tab_fully_visible(visual, 0, left_active);
    assert_host_tab_fully_visible(visual, 1, right_active);

    for width in [448., 240., 700.] {
        visual.simulate_resize(size(px(width), px(720.)));
        support::draw(visual);
        support::draw(visual);
        if width == 448. {
            for side in 0..2 {
                let strip = visual
                    .update(|window, _| {
                        window.debug_selector_bounds(&format!("files-host-strip-{side}"))
                    })
                    .expect("narrow host strip");
                assert!(
                    strip.size.width >= px(190.) && strip.size.width <= px(225.),
                    "测试必须覆盖约 200px 的双栏面板: {strip:?}"
                );
            }
        }
        assert_host_tab_fully_visible(visual, 0, left_active);
        assert_host_tab_fully_visible(visual, 1, right_active);
    }
    for scale in [1.25, 1.5, 2.0] {
        visual.simulate_scale_factor_change(scale);
        support::draw(visual);
        support::draw(visual);
        assert_host_tab_fully_visible(visual, 0, left_active);
        assert_host_tab_fully_visible(visual, 1, right_active);
    }

    let viewport = visual
        .update(|window, _| window.debug_selector_bounds("files-host-tabs-0"))
        .expect("left host strip");
    visual.simulate_mouse_move(viewport.center(), None, Default::default());
    visual.simulate_event(ScrollWheelEvent {
        position: viewport.center(),
        delta: ScrollDelta::Pixels(point(px(5_000.), px(0.))),
        ..Default::default()
    });
    support::draw(visual);
    let last = visual
        .update(|window, _| {
            window.debug_selector_bounds(&format!("files-host-tab-frame-0-{left_active}"))
        })
        .expect("last host tab");
    assert!(
        last.right() > viewport.right(),
        "手动滚动后不得强制回到活动标签"
    );

    visual.simulate_resize(size(px(448.), px(720.)));
    support::draw(visual);
    support::draw(visual);
    assert_host_tab_fully_visible(visual, 0, left_active);
    let selected_button = visual
        .update(|window, _| {
            window.debug_selector_bounds(&format!("files-host-tab-0-{left_active}"))
        })
        .expect("selected host button")
        .center();
    click_host_search(visual, selected_button);
    assert!(
        visual.update(|window, _| {
            let id = format!("files-host-tab-0-{left_active}").into();
            gpui_kit::base::test_support::find(window, &[], &id)
                .expect("selected host button")
                .focused()
                == Some(true)
        }),
        "点击标签后焦点应留在标签按钮"
    );
    visual.simulate_keystrokes("shift-tab");
    support::draw(visual);
    let previous = left_ids[left_ids.len() - 2];
    let (previous_focused, focused_controls) = visual.update(|window, _| {
        let id = format!("files-host-tab-close-0-{previous}").into();
        let previous_focused = gpui_kit::base::test_support::find(window, &[], &id)
            .expect("previous close button")
            .focused();
        let focused_controls = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|node| node.tracked_focus_id().is_some() && node.focused() == Some(true))
            .map(|node| format!("{node:?}"))
            .collect::<Vec<_>>();
        (previous_focused, focused_controls)
    });
    assert_eq!(
        previous_focused,
        Some(true),
        "Shift+Tab 应聚焦前一个关闭按钮，实际焦点: {focused_controls:?}"
    );
    assert_host_tab_fully_visible(visual, 0, previous);
    visual.simulate_resize(size(px(600.), px(720.)));
    support::draw(visual);
    support::draw(visual);
    assert_host_tab_fully_visible(visual, 0, previous);
    visual.simulate_keystrokes("enter");
    visual.simulate_event(KeyUpEvent {
        keystroke: Keystroke::parse("enter").expect("valid key"),
    });
    support::draw(visual);
    assert!(visual.update(|_, app| {
        let tabs = &pane.read(app).host_picker.host_tabs;
        tabs.active[0] == Some(left_active) && tabs.sides[0].iter().all(|tab| tab.id != previous)
    }));
    assert_host_tab_fully_visible(visual, 0, left_active);
    assert!(
        visual.update(|window, _| {
            let id = format!("files-host-tab-0-{left_active}").into();
            gpui_kit::base::test_support::find(window, &[], &id)
                .expect("remaining active tab")
                .focused()
                == Some(true)
        }),
        "键盘关闭后焦点应回到剩余活动标签"
    );

    visual.simulate_resize(size(px(448.), px(720.)));
    support::draw(visual);
    support::draw(visual);
    assert_host_tab_fully_visible(visual, 1, right_active);
    let close_position = visual.update(|window, _| {
        let id = format!("files-host-tab-close-1-{right_active}").into();
        gpui_kit::base::test_support::find(window, &[], &id)
            .expect("right close button")
            .bounds()
            .center()
    });
    click_host_search(visual, close_position);
    assert!(visual.update(|_, app| {
        let tabs = &pane.read(app).host_picker.host_tabs;
        tabs.active[1] == Some(right_ids[0])
            && tabs.sides[1].iter().all(|tab| tab.id != right_active)
    }));
    assert_host_tab_fully_visible(visual, 1, right_ids[0]);
    assert!(
        visual.update(|window, _| {
            let id = format!("files-host-tab-0-{left_active}").into();
            gpui_kit::base::test_support::find(window, &[], &id)
                .expect("previously focused left host")
                .focused()
                == Some(true)
        }),
        "关闭另一侧标签不能抢走已有焦点"
    );

    let remaining_right_button = visual
        .update(|window, _| {
            window.debug_selector_bounds(&format!("files-host-tab-1-{}", right_ids[0]))
        })
        .expect("remaining right host button")
        .center();
    click_host_search(visual, remaining_right_button);
    assert!(visual.update(|window, _| {
        let id = format!("files-host-tab-1-{}", right_ids[0]).into();
        gpui_kit::base::test_support::find(window, &[], &id)
            .expect("right active host button")
            .focused()
            == Some(true)
    }));
    let active_close_position = visual.update(|window, _| {
        let id = format!("files-host-tab-close-1-{}", right_ids[0]).into();
        gpui_kit::base::test_support::find(window, &[], &id)
            .expect("right active close button")
            .bounds()
            .center()
    });
    click_host_search(visual, active_close_position);
    assert!(
        visual.update(|window, app| {
            let pane = pane.read(app);
            let id = format!("files-host-tab-1-{}", right_ids[1]).into();
            pane.host_picker.host_tabs.active[1] == Some(right_ids[1])
                && gpui_kit::base::test_support::find(window, &[], &id)
                    .expect("fallback right host button")
                    .focused()
                    == Some(true)
        }),
        "鼠标关闭已聚焦标签后焦点应转到后继标签"
    );
    assert_host_tab_fully_visible(visual, 1, right_ids[1]);
    visual.simulate_keystrokes("tab");
    support::draw(visual);
    assert!(visual.update(|window, _| {
        let id = format!("files-host-tab-close-1-{}", right_ids[1]).into();
        gpui_kit::base::test_support::find(window, &[], &id)
            .expect("remaining right close button")
            .focused()
            == Some(true)
    }));
    visual.simulate_keystrokes("enter");
    visual.simulate_event(KeyUpEvent {
        keystroke: Keystroke::parse("enter").expect("valid key"),
    });
    support::draw(visual);
    assert!(
        visual.update(|window, app| {
            pane.read(app).host_picker.target_unselected[1]
                && pane.read(app).host_picker.search_inputs[1]
                    .as_ref()
                    .expect("right search input")
                    .read(app)
                    .focus_handle(app)
                    .is_focused(window)
        }),
        "关闭右侧最后标签后焦点应回到搜索框"
    );
    let _runtime = runtime;
}

/// 检查真实按钮与搜索框的边界和可见性，不以滚动偏移或外层包装代替控件布局。
///
/// @author kongweiguang
fn assert_host_tab_fully_visible(visual: &mut VisualTestContext, side: usize, tab_id: u64) {
    visual.update(|window, _| {
        let viewport = window
            .debug_selector_bounds(&format!("files-host-tabs-{side}"))
            .expect("host strip viewport");
        let tab = window
            .debug_selector_bounds(&format!("files-host-tab-frame-{side}-{tab_id}"))
            .expect("host tab frame");
        let close = window
            .debug_selector_bounds(&format!("files-host-tab-close-{side}-{tab_id}"))
            .expect("host close button");
        let search = window
            .debug_selector_bounds(&format!("files-sftp-host-picker-toggle-{side}"))
            .expect("host search");
        let activate_id = format!("files-host-tab-{side}-{tab_id}").into();
        let activate_button = gpui_kit::base::test_support::find(window, &[], &activate_id)
            .expect("host activation button");
        let close_id = format!("files-host-tab-close-{side}-{tab_id}").into();
        let close_button = gpui_kit::base::test_support::find(window, &[], &close_id)
            .expect("host close button");
        let activation_bounds = activate_button.bounds();
        let close_bounds = close_button.bounds();
        assert!(
            tab.left() + px(0.5) >= viewport.left() && tab.right() <= viewport.right() + px(0.5),
            "标签未完整显示: side={side}, tab={tab:?}, viewport={viewport:?}"
        );
        assert!(
            close.left() + px(0.5) >= viewport.left()
                && close.right() <= viewport.right() + px(0.5),
            "关闭按钮未完整显示: side={side}, close={close:?}, viewport={viewport:?}"
        );
        assert!(
            activate_button.visible()
                && close_button.visible()
                && activation_bounds.left() + px(0.5) >= viewport.left()
                && activation_bounds.right() <= close_bounds.left() + px(0.5)
                && close_bounds.right() <= viewport.right() + px(0.5)
                && activation_bounds.top() + px(0.5) >= viewport.top()
                && activation_bounds.bottom() <= viewport.bottom() + px(0.5)
                && close_bounds.top() + px(0.5) >= viewport.top()
                && close_bounds.bottom() <= viewport.bottom() + px(0.5),
            "标签按钮被裁切或重叠: side={side}, activation={activation_bounds:?}, close={close_bounds:?}, viewport={viewport:?}"
        );
        assert!(
            viewport.right() <= search.left() + px(0.5) && search.size.width > px(0.),
            "搜索框与标签滚动视口重叠: side={side}, search={search:?}, viewport={viewport:?}"
        );
    });
}

/// 两侧真实搜索框的方向键同步绘制高亮和主机请求，Escape 后焦点留在输入框。
///
/// @author kongweiguang
#[gpui_kit::test]
fn host_search_keyboard_selects_each_side_and_preserves_escape_focus(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let left_host = HostId::new();
    let right_host = HostId::new();
    let (runtime, pane, _, visual) = support::window_fixture(app, local, |_| {});
    let events = Rc::new(RefCell::new(Vec::<SftpHostRequested>::new()));
    let observer = visual.update(|_, app| {
        let observed_events = events.clone();
        let source = pane.clone();
        app.new(|cx| HostSelectionObserver {
            _subscription: cx.subscribe(&source, move |_, _, event: &SftpHostRequested, _| {
                observed_events.borrow_mut().push(event.clone());
            }),
        })
    });
    visual.update(|_, app| {
        pane.update(app, |pane, cx| {
            pane.set_host_picker_enabled(true, cx);
            pane.host_picker.hosts = vec![
                SftpHostChoice {
                    id: left_host.clone(),
                    name: "first".to_owned(),
                    address: "127.0.0.1".to_owned(),
                    username: "test".to_owned(),
                    port: 22,
                    protocol: RemoteProtocol::Ssh,
                    group_id: None,
                },
                SftpHostChoice {
                    id: right_host.clone(),
                    name: "second".to_owned(),
                    address: "127.0.0.2".to_owned(),
                    username: "test".to_owned(),
                    port: 22,
                    protocol: RemoteProtocol::Ssh,
                    group_id: None,
                },
            ];
        });
    });
    support::draw(visual);
    let search_position = |visual: &mut VisualTestContext, side| {
        visual
            .update(|window, _| {
                window.debug_selector_bounds(&format!("files-sftp-host-picker-toggle-{side}"))
            })
            .expect("host search rendered")
            .center()
    };
    let option_keys = ["local-device", left_host.as_str(), right_host.as_str()];
    let left_search = search_position(visual, 0);
    click_host_search(visual, left_search);
    assert_host_search_selection(visual, 0, &option_keys, "local-device");
    visual.simulate_keystrokes("down");
    support::draw(visual);
    assert_host_search_selection(visual, 0, &option_keys, left_host.as_str());
    visual.simulate_keystrokes("enter");
    support::draw(visual);
    let right_search = search_position(visual, 1);
    click_host_search(visual, right_search);
    assert_host_search_selection(visual, 1, &option_keys, "local-device");
    visual.simulate_keystrokes("down");
    support::draw(visual);
    assert_host_search_selection(visual, 1, &option_keys, left_host.as_str());
    visual.simulate_keystrokes("down");
    support::draw(visual);
    assert_host_search_selection(visual, 1, &option_keys, right_host.as_str());
    visual.simulate_keystrokes("enter");
    support::draw(visual);

    let captured = events.borrow();
    assert_eq!(captured.len(), 2);
    assert_eq!(
        (captured[0].side, &captured[0].host_id),
        (FilePaneSide::Left, &left_host)
    );
    assert_eq!(
        (captured[1].side, &captured[1].host_id),
        (FilePaneSide::Right, &right_host)
    );
    assert!(captured.iter().all(|event| event.generation > 0));
    assert!(visual.update(|_, app| captured.iter().all(|event| {
        pane.read(app)
            .host_selection_is_current(event.side, event.generation)
    })));
    drop(captured);

    click_host_search(visual, left_search);
    visual.simulate_keystrokes("escape");
    support::draw(visual);
    assert_eq!(
        visual.update(|_, app| pane.read(app).host_picker.expanded_side),
        None
    );
    assert!(visual.update(|window, app| {
        pane.read(app).host_picker.search_inputs[0]
            .as_ref()
            .expect("left input")
            .read(app)
            .focus_handle(app)
            .is_focused(window)
    }));
    let _observer = observer;
    let _runtime = runtime;
}

/// 已显示的目标在刷新期间不移动，方向键仍对应同一组过滤后的真实行。
///
/// @author kongweiguang
#[gpui_kit::test]
fn host_search_refresh_keeps_existing_rows_and_keyboard_selection_stable(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let host = HostId::new();
    let (runtime, pane, _, visual) = support::window_fixture(app, local, |_| {});
    visual.update(|_, app| {
        pane.update(app, |pane, cx| pane.set_host_picker_enabled(true, cx));
    });
    support::draw(visual);
    let search = visual
        .debug_bounds("files-sftp-host-picker-toggle-0")
        .expect("host search rendered")
        .center();
    click_host_search(visual, search);
    visual.update(|_, app| {
        pane.update(app, |pane, cx| {
            pane.host_picker.generation = pane.host_picker.generation.wrapping_add(1);
            pane.host_picker.loading = false;
            pane.host_picker.error = None;
            pane.host_picker.hosts = vec![SftpHostChoice {
                id: host.clone(),
                name: "existing host".to_owned(),
                address: "127.0.0.1".to_owned(),
                username: "test".to_owned(),
                port: 22,
                protocol: RemoteProtocol::Ssh,
                group_id: None,
            }];
            cx.notify();
        });
    });
    support::draw(visual);
    let row_id: gpui_kit::ElementId =
        format!("files-sftp-host-picker-list-0-option-{}", host.as_str()).into();
    let refresh_id: gpui_kit::ElementId = "files-sftp-host-picker-refresh-0".into();
    let (row_before, refresh_before) = visual.update(|window, _| {
        (
            gpui_kit::base::test_support::find(window, &[], &row_id)
                .expect("host row before refresh")
                .bounds(),
            gpui_kit::base::test_support::find(window, &[], &refresh_id)
                .expect("refresh before request")
                .bounds(),
        )
    });
    visual.update(|_, app| {
        pane.update(app, |pane, cx| {
            pane.host_picker.loading = true;
            cx.notify();
        });
    });
    support::draw(visual);
    visual.update(|window, _| {
        let row = gpui_kit::base::test_support::find(window, &[], &row_id)
            .expect("host row during refresh");
        let refresh = gpui_kit::base::test_support::find(window, &[], &refresh_id)
            .expect("refresh during request");
        assert_eq!(row.bounds(), row_before, "refresh moved the existing host");
        assert_eq!(refresh.bounds(), refresh_before, "refresh changed its slot");
        assert!(
            !refresh.supports_click(),
            "refresh must reject duplicate clicks"
        );
        assert!(matches!(
            refresh.label(),
            Some("正在刷新主机列表" | "Refreshing host list")
        ));
    });
    visual.simulate_keystrokes("down");
    support::draw(visual);
    assert_host_search_selection(visual, 0, &["local-device", host.as_str()], host.as_str());

    let _runtime = runtime;
}

/// 使用同一帧的原生选中语义检查高亮，避免把键盘内部状态误认为已经绘制。
///
/// @author kongweiguang
fn assert_host_search_selection(
    visual: &mut VisualTestContext,
    side: usize,
    option_keys: &[&str],
    expected: &str,
) {
    visual.update(|window, _| {
        for key in option_keys {
            let id = format!("files-sftp-host-picker-list-{side}-option-{key}").into();
            let row = gpui_kit::base::test_support::find(window, &[], &id)
                .unwrap_or_else(|| panic!("主机选择行未绘制: {side} / {key}"));
            assert_eq!(
                row.selected(),
                Some(*key == expected),
                "主机选择行的绘制选中状态不匹配: {side} / {key}"
            );
            if *key == expected {
                assert!(row.visible(), "选中的主机行必须可见: {side} / {key}");
            }
        }
    });
}

/// 派发真实鼠标事件并完成绘制，使弹层生命周期与生产窗口一致。
///
/// @author kongweiguang
fn click_host_search(visual: &mut VisualTestContext, position: gpui_kit::Point<gpui_kit::Pixels>) {
    visual.simulate_mouse_move(position, None, Default::default());
    visual.simulate_mouse_down(position, MouseButton::Left, Default::default());
    visual.simulate_mouse_up(position, MouseButton::Left, Default::default());
    support::draw(visual);
}
