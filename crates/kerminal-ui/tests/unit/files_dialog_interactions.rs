//! 文件弹框通过生产按钮验证实体借用、提交防重和失败后继续操作。
//!
//! @author kongweiguang

use std::{cell::RefCell, rc::Rc, sync::Arc};

use gpui_kit::{
    AppContext as _, Context, Entity, IntoElement, MouseButton, ParentElement as _, Render,
    Styled as _, Subscription, TestAppContext, VisualTestContext, Window, div, px, size,
};
use kerminal_domain::{files::FileEntryKind, identity::HostId};
use tokio::runtime::Runtime;

use super::{FileTargetNameLookup, FilesPane, action_test_support as support};

/// 冲突计划只引用隔离端点，保留真实批次闩锁和目录代次，让策略按钮经过正常受理边界。
fn seed_conflict(pane: &mut FilesPane, cx: &mut Context<FilesPane>) {
    use super::{
        BatchEnqueueLabels, ConflictPolicy, PendingTransferConflict, TransferBatchPlan,
        TransferConflict, TransferConflictKind, TransferEnqueueItem, TransferKind, TransferRequest,
        TransferTargetSnapshot,
    };

    let directory = pane.panels[1].endpoint.clone();
    support::seed_listing(pane, 1, 7, Vec::new());
    assert!(pane.begin_transfer_submission(cx));
    pane.pending_transfer_conflict = Some(PendingTransferConflict {
        plan: TransferBatchPlan {
            items: vec![TransferEnqueueItem {
                name: "report.txt".into(),
                request: TransferRequest {
                    source: support::remote_endpoint(&HostId::new(), "/source/report.txt"),
                    destination: support::remote_entry(
                        &directory,
                        "report.txt",
                        FileEntryKind::File,
                    )
                    .endpoint,
                    kind: TransferKind::File,
                    conflict_policy: ConflictPolicy::Rename,
                    idle_timeout_seconds: None,
                },
                external_target_lease: None,
            }],
            total: 1,
            included_by_selected_directories: 0,
            issues: Vec::new(),
            labels: BatchEnqueueLabels {
                action_zh: "测试传输已入队",
                action_en: "Test transfer queued",
            },
            target: TransferTargetSnapshot {
                side: 1,
                endpoint: directory,
                generation: 7,
            },
        },
        conflicts: vec![TransferConflict {
            item_index: 0,
            kind: TransferConflictKind::Existing,
        }],
    });
    cx.notify();
}

struct DialogHarness {
    pane: Entity<FilesPane>,
    _observe: Subscription,
}

impl Render for DialogHarness {
    /// 使用生产弹框内容和真实按钮；宿主只提供窗口，避免直接调用 owner 掩盖事件重入。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dialog = self
            .pane
            .update(cx, |pane, cx| pane.render_global_dialog(window, cx));
        div().size_full().children(dialog)
    }
}

/// 所有文件端口拒绝真实 IO；窗口保留生产 Input 和 Button 的事件订阅与实体生命周期。
fn fixture(
    app: &mut TestAppContext,
) -> (
    Runtime,
    Entity<FilesPane>,
    Arc<support::MutationCalls>,
    &mut VisualTestContext,
) {
    app.update(crate::init);
    let (runtime, files, transfers, executor, mutations) = support::services();
    let endpoint = support::remote_endpoint(&HostId::new(), "/srv/dialogs");
    let mounted = Rc::new(RefCell::new(None));
    let slot = mounted.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let pane = cx.new(|_| {
            let mut pane = FilesPane::new(
                files,
                transfers,
                executor,
                FileTargetNameLookup::default(),
                endpoint,
            );
            pane.initialized = true;
            pane.compact_layout = true;
            pane.active_side = 1;
            pane
        });
        *slot.borrow_mut() = Some(pane.clone());
        let harness = cx.new(|cx| DialogHarness {
            _observe: cx.observe(&pane, |_, _, cx| cx.notify()),
            pane,
        });
        gpui_kit::base::Root::new(harness, window, cx)
    });
    visual.simulate_resize(size(px(800.), px(600.)));
    let pane = mounted.borrow_mut().take().expect("dialog pane mounted");
    support::draw(visual);
    (runtime, pane, mutations, visual)
}

/// 通过 Kit 观察到的真实按钮 bounds 派发鼠标，断言完整命中范围可见后再点击。
fn click(visual: &mut VisualTestContext, id: &str) {
    support::draw(visual);
    let bounds = visual.update(|window, _| {
        gpui_kit::base::test_support::find(window, &[], &id.to_owned().into())
            .filter(|button| button.visible())
            .expect("production dialog button is visible")
            .bounds()
    });
    let position = bounds.center();
    visual.simulate_mouse_move(position, None, Default::default());
    visual.simulate_mouse_down(position, MouseButton::Left, Default::default());
    visual.simulate_mouse_up(position, MouseButton::Left, Default::default());
    support::draw(visual);
}

/// 显式推进隔离 runtime，让“等待中”和“回执已收尾”成为两个可控制阶段。
fn settle(runtime: &Runtime, visual: &mut VisualTestContext) {
    for _ in 0..8 {
        support::draw(visual);
        runtime.block_on(tokio::task::yield_now());
    }
    support::draw(visual);
}

/// 取消必须从生产按钮释放当前表单，且不能递归借用同一 FilesPane 或提交文件操作。
#[gpui_kit::test]
fn cancel_button_closes_each_file_form_without_submitting(app: &mut TestAppContext) {
    use std::sync::atomic::Ordering;

    let (runtime, pane, mutations, visual) = fixture(app);
    for kind in ["create", "rename", "permissions", "delete"] {
        visual.update(|window, cx| {
            pane.update(cx, |pane, cx| {
                let directory = pane.panels[1].endpoint.clone();
                let mut entry =
                    support::remote_entry(&directory, "report.txt", FileEntryKind::File);
                entry.permissions = Some(0o644);
                pane.panels[1].selected = Some(entry.clone());
                match kind {
                    "create" => pane.prepare_create_directory(window, cx),
                    "rename" => pane.prepare_rename(window, cx),
                    "permissions" => pane.prepare_permissions(1, window, cx),
                    "delete" => {
                        pane.delete_confirmation = Some(vec![entry]);
                        cx.notify();
                    }
                    _ => unreachable!(),
                }
                assert!(pane.pending_global_dialog());
            });
        });
        click(visual, "files-global-dialog-cancel");
        visual.update(|_, cx| assert!(!pane.read(cx).pending_global_dialog(), "{kind}"));
    }
    runtime.block_on(tokio::task::yield_now());
    for count in [
        &mutations.create_directory,
        &mutations.rename,
        &mutations.chmod,
        &mutations.delete,
    ] {
        assert_eq!(count.load(Ordering::Acquire), 0);
    }
}

/// 真正点击确认并连续重击，在端口失败后保留名称草稿，允许修正重试和取消。
#[gpui_kit::test]
fn name_confirm_buttons_reject_reentry_and_keep_failed_drafts(app: &mut TestAppContext) {
    use std::sync::atomic::Ordering;

    let (runtime, pane, mutations, visual) = fixture(app);
    for rename in [false, true] {
        let draft = if rename {
            "revised.txt"
        } else {
            "/srv/dialogs/new"
        };
        visual.update(|window, cx| {
            pane.update(cx, |pane, cx| {
                if rename {
                    let entry = support::remote_entry(
                        &pane.panels[1].endpoint,
                        "report.txt",
                        FileEntryKind::File,
                    );
                    pane.panels[1].selected = Some(entry);
                    pane.prepare_rename(window, cx);
                } else {
                    pane.prepare_create_directory(window, cx);
                }
                pane.name_input
                    .as_ref()
                    .expect("name input")
                    .update(cx, |input, cx| {
                        input.set_value(draft, window, cx);
                    });
            });
        });
        click(visual, "files-global-dialog-confirm");
        click(visual, "files-global-dialog-confirm");
        visual.update(|_, cx| {
            pane.update(cx, |pane, cx| {
                assert!(pane.name_action_busy);
                pane.load_panel(1, cx);
            });
        });
        settle(&runtime, visual);
        let count = if rename {
            &mutations.rename
        } else {
            &mutations.create_directory
        };
        assert_eq!(count.load(Ordering::Acquire), 1);
        visual.update(|_, cx| {
            let pane = pane.read(cx);
            assert!(!pane.name_action_busy);
            assert!(pane.status.is_some());
            assert!(pane.pending_global_dialog());
            assert_eq!(
                pane.name_input
                    .as_ref()
                    .expect("kept input")
                    .read(cx)
                    .value(),
                draft
            );
        });
        click(visual, "files-global-dialog-confirm");
        settle(&runtime, visual);
        assert_eq!(count.load(Ordering::Acquire), 2);
        click(visual, "files-global-dialog-cancel");
        visual.update(|_, cx| assert!(!pane.read(cx).pending_global_dialog()));
    }
}

/// 删除确认必须从生产按钮受理一次请求；失败仍展示可理解的结果，不能在回调内实体重入。
#[gpui_kit::test]
fn delete_confirm_button_submits_the_frozen_selection_once(app: &mut TestAppContext) {
    use std::sync::atomic::Ordering;

    let (runtime, pane, mutations, visual) = fixture(app);
    visual.update(|_, cx| {
        pane.update(cx, |pane, cx| {
            let entry =
                support::remote_entry(&pane.panels[1].endpoint, "report.txt", FileEntryKind::File);
            *mutations.directory_listing.lock().expect("listing fixture") =
                Some(kerminal_domain::files::DirectoryListing {
                    directory: pane.panels[1].endpoint.clone(),
                    entries: vec![entry.clone()],
                });
            pane.panels[1].selected = Some(entry.clone());
            pane.delete_confirmation = Some(vec![entry]);
            cx.notify();
        });
    });
    click(visual, "files-global-dialog-confirm");
    visual.update(|_, cx| assert!(!pane.read(cx).pending_global_dialog()));
    visual.update(|_, cx| {
        pane.update(cx, |pane, cx| {
            pane.delete_selected(cx);
            assert!(
                !pane.pending_global_dialog(),
                "an accepted deletion must reject resubmission"
            );
            pane.load_panel(1, cx);
        });
    });
    settle(&runtime, visual);
    assert_eq!(mutations.delete.load(Ordering::Acquire), 1);
    visual.update(|_, cx| {
        let pane = pane.read(cx);
        assert!(!pane.delete_action_busy);
        assert!(
            pane.panels[1].listing.is_some(),
            "successful refresh is independent of delete failure"
        );
        assert!(
            pane.status
                .as_deref()
                .is_some_and(|status| status.contains("report.txt"))
        );
    });
}

/// 删除发出后保留用户的新导航代次；旧回执不能重读提交目录而废弃正在读取的新目录。
#[gpui_kit::test]
fn late_delete_receipt_preserves_pending_navigation(app: &mut TestAppContext) {
    let (runtime, pane, mutations, visual) = fixture(app);
    visual.update(|_, cx| {
        pane.update(cx, |pane, cx| {
            let entry =
                support::remote_entry(&pane.panels[1].endpoint, "report.txt", FileEntryKind::File);
            pane.panels[1].selected = Some(entry.clone());
            pane.delete_confirmation = Some(vec![entry]);
            cx.notify();
        });
    });
    click(visual, "files-global-dialog-confirm");
    let navigation_generation = visual.update(|_, cx| {
        pane.update(cx, |pane, cx| {
            let next = support::remote_endpoint(&HostId::new(), "/srv/next");
            *mutations.directory_listing.lock().expect("next listing") =
                Some(kerminal_domain::files::DirectoryListing {
                    directory: next.clone(),
                    entries: Vec::new(),
                });
            pane.open_endpoint_on_side(1, next, cx);
            pane.status = Some("new target feedback".into());
            pane.panels[1].generation
        })
    });
    settle(&runtime, visual);
    visual.update(|_, cx| {
        let pane = pane.read(cx);
        assert!(!pane.delete_action_busy);
        assert_eq!(pane.panels[1].generation, navigation_generation);
        assert_eq!(pane.status.as_deref(), Some("new target feedback"));
    });
}

/// 名称操作在目录导航后撤销旧展示但保留受理中的所有权，迟到失败不能覆盖新目录。
#[gpui_kit::test]
fn late_name_receipts_preserve_new_navigation_and_release_busy(app: &mut TestAppContext) {
    let (runtime, pane, mutations, visual) = fixture(app);
    for rename in [false, true] {
        visual.update(|window, cx| {
            pane.update(cx, |pane, cx| {
                pane.panels[1].selected = Some(support::remote_entry(
                    &pane.panels[1].endpoint,
                    "report.txt",
                    FileEntryKind::File,
                ));
                if rename {
                    pane.prepare_rename(window, cx);
                } else {
                    pane.prepare_create_directory(window, cx);
                }
            });
        });
        click(visual, "files-global-dialog-confirm");
        let navigation_generation = visual.update(|_, cx| {
            pane.update(cx, |pane, cx| {
                let next = support::remote_endpoint(&HostId::new(), "/srv/next");
                *mutations.directory_listing.lock().expect("next listing") =
                    Some(kerminal_domain::files::DirectoryListing {
                        directory: next.clone(),
                        entries: Vec::new(),
                    });
                pane.open_endpoint_on_side(1, next, cx);
                assert!(
                    pane.name_action.is_none(),
                    "navigation dismisses the old form"
                );
                assert!(
                    pane.name_action_busy,
                    "navigation must not release submitted work"
                );
                pane.status = Some("new target feedback".into());
                pane.panels[1].generation
            })
        });
        settle(&runtime, visual);
        visual.update(|_, cx| {
            let pane = pane.read(cx);
            assert!(!pane.name_action_busy);
            assert_eq!(pane.panels[1].generation, navigation_generation);
            assert_eq!(pane.status.as_deref(), Some("new target feedback"));
        });
    }
}

/// 每种策略和取消都点击真实按钮，防止某一个闭包仍重入 owner；取消不受理任何队列任务。
#[gpui_kit::test]
fn transfer_conflict_buttons_release_the_dialog_without_entity_reentry(app: &mut TestAppContext) {
    let (runtime, pane, _, visual) = fixture(app);
    for action in ["cancel", "overwrite", "rename", "skip"] {
        visual.update(|_, cx| pane.update(cx, seed_conflict));
        click(visual, &format!("files-transfer-conflict-{action}"));
        visual.update(|_, cx| {
            let pane = pane.read(cx);
            assert!(pane.pending_transfer_conflict.is_none(), "{action}");
            assert_eq!(pane.transfer_submission_busy, action != "cancel");
            assert_eq!(pane.queue.conflict_policy, super::ConflictPolicy::Rename);
        });
        settle(&runtime, visual);
        visual.update(|_, cx| assert!(!pane.read(cx).transfer_submission_busy, "{action}"));
    }
}

/// 目标失效只撤销展示；旧创建/重命名回执必须释放独占状态，且不能覆盖新目标的错误。
#[gpui_kit::test]
fn lost_target_name_receipts_release_busy_before_accepting_a_new_form(app: &mut TestAppContext) {
    let (runtime, pane, _, visual) = fixture(app);
    for rename in [false, true] {
        visual.update(|window, cx| {
            pane.update(cx, |pane, cx| {
                pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
                if rename {
                    pane.panels[1].selected = Some(support::remote_entry(
                        &pane.panels[1].endpoint,
                        "report.txt",
                        FileEntryKind::File,
                    ));
                    pane.prepare_rename(window, cx);
                } else {
                    pane.prepare_create_directory(window, cx);
                }
            });
        });
        click(visual, "files-global-dialog-confirm");
        visual.update(|window, cx| {
            pane.update(cx, |pane, cx| {
                assert!(pane.name_action_busy);
                pane.set_current_terminal_target(None, None, None, cx);
                assert!(pane.name_action.is_none());
                assert!(
                    pane.name_action_busy,
                    "submitted work still owns its busy state"
                );
                pane.prepare_create_directory(window, cx);
                pane.prepare_rename(window, cx);
                pane.panels[1].selected = Some(support::remote_entry(
                    &pane.panels[1].endpoint,
                    "next.txt",
                    FileEntryKind::File,
                ));
                pane.prepare_permissions(1, window, cx);
                assert!(
                    !pane.pending_global_dialog(),
                    "new form must wait for the old receipt"
                );
                pane.status = Some("new target feedback".into());
            });
        });
        settle(&runtime, visual);
        visual.update(|window, cx| {
            pane.update(cx, |pane, cx| {
                assert!(!pane.name_action_busy);
                assert_eq!(pane.status.as_deref(), Some("new target feedback"));
                pane.prepare_create_directory(window, cx);
                assert!(
                    pane.pending_global_dialog(),
                    "new work is usable after the old receipt"
                );
            });
        });
        click(visual, "files-global-dialog-cancel");
    }
}

/// 权限确认走真实按钮，离开目标后的迟到失败只收尾原请求，不改写当前选择或状态。
#[gpui_kit::test]
fn stale_permission_receipt_keeps_new_target_state_and_releases_busy(app: &mut TestAppContext) {
    use std::sync::atomic::Ordering;

    let (runtime, pane, mutations, visual) = fixture(app);
    visual.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
            let mut entry =
                support::remote_entry(&pane.panels[1].endpoint, "report.txt", FileEntryKind::File);
            entry.permissions = Some(0o644);
            pane.panels[1].selected = Some(entry);
            pane.prepare_permissions(1, window, cx);
        });
    });
    click(visual, "files-global-dialog-confirm");
    click(visual, "files-global-dialog-confirm");
    let next = visual.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            assert!(pane.permissions.busy);
            pane.set_current_terminal_target(None, None, None, cx);
            assert!(!pane.pending_global_dialog());
            let next =
                support::remote_entry(&pane.panels[1].endpoint, "next.txt", FileEntryKind::File);
            pane.panels[1].selected = Some(next.clone());
            pane.status = Some("new target feedback".into());
            pane.prepare_create_directory(window, cx);
            assert!(
                pane.name_action.is_none(),
                "permission request keeps exclusive ownership"
            );
            next
        })
    });
    settle(&runtime, visual);
    assert_eq!(mutations.chmod.load(Ordering::Acquire), 1);
    visual.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            assert!(!pane.permissions.busy);
            assert_eq!(pane.status.as_deref(), Some("new target feedback"));
            assert_eq!(
                pane.panels[1]
                    .selected
                    .as_ref()
                    .map(|entry| &entry.endpoint),
                Some(&next.endpoint)
            );
            assert!(pane.panels[1].pending_selection.is_none());
            pane.prepare_create_directory(window, cx);
            assert!(pane.pending_global_dialog());
        });
    });
}

/// 没有旧清单时目录错误只在列表内联卡片出现一次：底栏不复述，重试落在卡片上，
/// 而且 TCP 失败不能被误当成主机密钥问题弹出信任按钮。
///
/// @author kongweiguang
#[gpui_kit::test]
fn directory_error_renders_single_inline_failure_with_retry(app: &mut TestAppContext) {
    let host = HostId::new();
    let endpoint = support::remote_endpoint(&host, "/srv/unreachable");
    let failed = endpoint.clone();
    let bound = endpoint.clone();
    let (runtime, pane, _mutations, visual) = support::window_fixture(app, endpoint, move |pane| {
        pane.compact_layout = true;
        pane.active_side = 1;
        pane.current_terminal_endpoint = Some(bound);
        pane.panels[1].listing = None;
        pane.panels[1].failed_directory = Some(failed);
        pane.panels[1].directory_error = Some(super::localized_error(
            &kerminal_application::AppError::infrastructure("SSH TCP connection failed"),
        ));
    });
    visual.simulate_resize(size(px(800.), px(600.)));
    settle(&runtime, visual);
    visual.update(|window, _| {
        let visible = |id: &str| {
            gpui_kit::base::test_support::find(window, &[], &id.to_owned().into())
                .filter(|element| element.visible())
                .is_some()
        };
        assert!(
            visible("files-directory-error-retry-1"),
            "the inline error card keeps the retry entry"
        );
        assert!(
            !visible("files-trust-host-key"),
            "a TCP failure must not offer host-key trust"
        );
        assert!(
            !visible("files-retry-directory"),
            "the bottom bar must not repeat the same directory error"
        );
    });
    visual.update(|_, cx| {
        pane.update(cx, |pane, _| {
            let message = pane.panels[1]
                .directory_error
                .as_deref()
                .expect("directory failure detail");
            assert_ne!(
                message, "SSH TCP connection failed",
                "the raw transport context must be localized for users"
            );
            assert!(message.contains("SSH"), "detail keeps the failing subject");
        });
    });
}

/// 树视图根目录读取失败时沿用列表内联错误卡，错误和唯一重试入口与列表一致。
///
/// @author kongweiguang
#[gpui_kit::test]
fn tree_view_reuses_inline_directory_error_and_retry(app: &mut TestAppContext) {
    let host = HostId::new();
    let endpoint = support::remote_endpoint(&host, "/srv/unreachable");
    let failed = endpoint.clone();
    let bound = endpoint.clone();
    let (runtime, _pane, _mutations, visual) =
        support::window_fixture(app, endpoint, move |pane| {
            pane.compact_layout = true;
            pane.active_side = 1;
            pane.view_mode = super::view_mode::FilesViewMode::Tree;
            pane.current_terminal_endpoint = Some(bound);
            pane.panels[1].listing = None;
            pane.panels[1].failed_directory = Some(failed);
            pane.panels[1].directory_error =
                Some("无法连接 SSH 主机，请检查网络、地址和端口后重试。".into());
        });
    visual.simulate_resize(size(px(800.), px(600.)));
    settle(&runtime, visual);
    visual.update(|window, _| {
        let visible = |id: &str| {
            gpui_kit::base::test_support::find(window, &[], &id.to_owned().into())
                .filter(|element| element.visible())
                .is_some()
        };
        assert!(
            visible("files-directory-error-retry-1"),
            "tree mode shows the same inline retry as the list"
        );
        assert!(
            !visible("files-retry-directory"),
            "the bottom bar must not repeat the same directory error"
        );
    });
}

/// 同任务摘要与恢复记录共用一张卡片；通过列表高度对照防止额外渲染孤儿卡。
///
/// @author kongweiguang
#[gpui_kit::test]
fn linked_recovery_shares_the_summary_card_without_maintenance_buttons(app: &mut TestAppContext) {
    let (summary, recovery) = transfer_card_fixture();
    let linked_id = summary.id.clone();
    let (runtime, pane, _mutations, visual) = support::window_fixture(
        app,
        support::remote_endpoint(&HostId::new(), "/srv"),
        move |pane| {
            pane.compact_layout = true;
            pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
            pane.queue_watch_started = true;
            pane.transfer_rows = vec![summary];
            pane.transfer_recovery_rows = vec![recovery];
        },
    );
    visual.simulate_resize(size(px(800.), px(1000.)));
    settle(&runtime, visual);
    let linked_list = visual.update(|window, _| {
        let card = window
            .debug_selector_bounds(&format!("rr-files-transfer-{linked_id}"))
            .expect("the linked summary card is rendered");
        let list = window
            .debug_selector_bounds("rr-files-transfer-list")
            .expect("the expanded transfer list is rendered");
        assert!(
            list.size.height >= card.size.height && card.size.height > px(0.),
            "the summary card must fit in the list: card={card:?}, list={list:?}"
        );
        for prefix in [
            "recovery-inspect",
            "recovery-locate",
            "recovery-archive",
            "recovery-staging",
        ] {
            let id = format!("{prefix}-{linked_id}");
            assert!(
                gpui_kit::base::test_support::find(window, &[], &id.clone().into()).is_none(),
                "unexpected maintenance button: {id}"
            );
        }
        list
    });
    visual.update(|_, cx| {
        pane.update(cx, |pane, cx| {
            pane.transfer_recovery_rows.clear();
            cx.notify();
        });
    });
    support::draw(visual);
    visual.update(|window, _| {
        let summary_only = window
            .debug_selector_bounds("rr-files-transfer-list")
            .expect("the summary-only list is rendered");
        assert_eq!(
            linked_list.size.height, summary_only.size.height,
            "linking a recovery record must not create another card"
        );
    });
}

/// 孤儿恢复记录仍需形成非空列表；移除该记录后列表消失，证明卡片并非空占位。
///
/// @author kongweiguang
#[gpui_kit::test]
fn orphan_recovery_keeps_its_own_card(app: &mut TestAppContext) {
    let (_summary, mut recovery) = transfer_card_fixture();
    recovery.id = kerminal_domain::identity::TransferId::new();
    let orphan_id = recovery.id.clone();
    let (runtime, pane, _mutations, visual) = support::window_fixture(
        app,
        support::remote_endpoint(&HostId::new(), "/srv"),
        move |pane| {
            pane.compact_layout = true;
            pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
            pane.queue_watch_started = true;
            pane.transfer_recovery_rows = vec![recovery];
        },
    );
    visual.simulate_resize(size(px(800.), px(600.)));
    settle(&runtime, visual);
    visual.update(|window, _| {
        let list = window
            .debug_selector_bounds("rr-files-transfer-list")
            .expect("the orphan recovery list is rendered");
        assert!(
            list.size.height > px(0.),
            "the orphan recovery card occupies visible list space"
        );
        for prefix in [
            "recovery-inspect",
            "recovery-locate",
            "recovery-archive",
            "recovery-staging",
        ] {
            let id = format!("{prefix}-{orphan_id}");
            assert!(
                gpui_kit::base::test_support::find(window, &[], &id.clone().into()).is_none(),
                "unexpected maintenance button: {id}"
            );
        }
    });
    visual.update(|_, cx| {
        pane.update(cx, |pane, cx| {
            pane.transfer_recovery_rows.clear();
            cx.notify();
        });
    });
    support::draw(visual);
    visual.update(|window, _| {
        assert!(
            window
                .debug_selector_bounds("rr-files-transfer-list")
                .is_none(),
            "without the orphan record there is no recovery list"
        );
    });
}

/// 复用真实领域摘要并绑定测试面板来源，避免手工 DTO 绕过生产的归属规则。
///
/// @author kongweiguang
fn transfer_card_fixture() -> (
    kerminal_domain::files::TransferSummary,
    kerminal_application::transfers::TransferRecoveryRecord,
) {
    use kerminal_domain::files::{
        ConflictPolicy, FileEndpoint, TransferKind, TransferRequest, TransferStatus, TransferTask,
    };

    let request = TransferRequest {
        source: FileEndpoint::Local {
            path: std::path::PathBuf::from("report.zip"),
        },
        destination: FileEndpoint::Local {
            path: std::path::PathBuf::from(".kerminal-transfer/report.zip"),
        },
        kind: TransferKind::File,
        conflict_policy: ConflictPolicy::Rename,
        idle_timeout_seconds: None,
    };
    let mut summary = TransferTask::queued(kerminal_domain::identity::TransferId::new(), request)
        .expect("fixture endpoints are distinct")
        .summary();
    summary.status = TransferStatus::Failed;
    summary.failure_kind = Some(kerminal_domain::files::TransferFailureKind::CommitUnknown);
    let recovery = kerminal_application::transfers::TransferRecoveryRecord {
        id: summary.id.clone(),
        origin: Some(kerminal_application::transfers::TransferOrigin::SharedWorkbench),
        drag_staging: false,
        request: summary.request.clone(),
        actual_destination: None,
        checkpoint: None,
        staging_ownership: Vec::new(),
        cleanup_pending: true,
        commit_unknown: true,
        inspection: kerminal_application::transfers::TransferRecoveryInspection::Insufficient,
        archived: false,
    };
    (summary, recovery)
}

/// 列表与树保留旧清单时，读取失败仍有真实重试入口；重试必须命中失败目录且不抹掉独立业务错误。
///
/// @author kongweiguang
#[gpui_kit::test]
fn directory_retry_keeps_failed_target_and_independent_feedback(app: &mut TestAppContext) {
    use kerminal_domain::files::DirectoryListing;

    for tree in [false, true] {
        let host = HostId::new();
        let original = support::remote_endpoint(&host, "/srv/original");
        let target = support::remote_endpoint(&host, "/srv/retry");
        let (runtime, pane, mutations, visual) =
            support::window_fixture(app, original.clone(), move |pane| {
                pane.queue_watch_started = true;
                pane.active_side = 1;
                if tree {
                    pane.view_mode = super::view_mode::FilesViewMode::Tree;
                }
                support::seed_listing(pane, 1, 7, Vec::new());
            });
        visual.simulate_resize(size(px(800.), px(600.)));
        visual.update(|_, cx| {
            pane.update(cx, |pane, cx| {
                pane.open_endpoint_on_side(1, target.clone(), cx);
            });
        });
        settle(&runtime, visual);
        visual.update(|_, cx| {
            pane.update(cx, |pane, cx| {
                assert_eq!(pane.panels[1].endpoint, original);
                assert_eq!(pane.panels[1].failed_directory.as_ref(), Some(&target));
                assert!(pane.visible_operation_status().is_some());
                pane.status = Some("independent mutation failure".into());
                cx.notify();
            });
        });
        *mutations.directory_listing.lock().expect("listing fixture") = Some(DirectoryListing {
            directory: target.clone(),
            entries: Vec::new(),
        });
        click(visual, "files-retry-directory");
        settle(&runtime, visual);
        visual.update(|_, cx| {
            let pane = pane.read(cx);
            assert_eq!(pane.panels[1].endpoint, target);
            assert!(pane.panels[1].directory_error.is_none());
            assert!(pane.panels[1].failed_directory.is_none());
            assert_eq!(pane.status.as_deref(), Some("independent mutation failure"));
        });
    }
}

/// 双击进入再返回父目录即使路径相同也已是新导航；旧写入只释放 busy，不能复活表单或覆盖新反馈。
///
/// @author kongweiguang
#[gpui_kit::test]
fn returning_to_same_path_does_not_revive_previous_mutation(app: &mut TestAppContext) {
    let (runtime, pane, mutations, visual) = fixture(app);
    visual.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            pane.prepare_create_directory(window, cx);
            pane.name_input
                .as_ref()
                .expect("name input")
                .update(cx, |input, cx| {
                    input.set_value("new", window, cx);
                });
        });
    });
    click(visual, "files-global-dialog-confirm");
    visual.update(|_, cx| {
        pane.update(cx, |pane, cx| {
            assert!(pane.name_action_busy);
            let original = pane.panels[1].endpoint.clone();
            let child = support::remote_entry(&original, "child", FileEntryKind::Directory);
            pane.activate_entry(1, child, cx);
            pane.navigate_parent(1, cx);
            assert_eq!(pane.panels[1].endpoint, original);
            assert!(!pane.pending_global_dialog());
            *mutations.directory_listing.lock().expect("listing fixture") =
                Some(kerminal_domain::files::DirectoryListing {
                    directory: original,
                    entries: Vec::new(),
                });
            pane.status = Some("returned navigation feedback".into());
        });
    });
    settle(&runtime, visual);
    visual.update(|_, cx| {
        let pane = pane.read(cx);
        assert!(!pane.name_action_busy);
        assert!(!pane.pending_global_dialog());
        assert_eq!(pane.status.as_deref(), Some("returned navigation feedback"));
    });
}
