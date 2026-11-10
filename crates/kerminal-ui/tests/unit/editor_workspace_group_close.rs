//! 使用真实共享编辑器和事件订阅核对组关闭原子性，不访问用户数据或执行文件 I/O。
//!
//! @author kongweiguang

use super::*;
use kerminal_domain::files::{FileRevision, LineEnding, editor::FileEditorSession};

/// 所有收据使用可辨认 revision，最终核对不能只凭正文相同判断权限仍有效。
///
/// @author kongweiguang
fn revision(value: &str) -> FileRevision {
    FileRevision {
        size: 4,
        modified: Some(value.to_owned()),
        permissions: None,
        content_digest: None,
    }
}

/// 只注入已读回的领域契约，生产 finish_load 负责创建真实编辑器及正文订阅。
///
/// @author kongweiguang
fn loaded(
    editor: &Entity<FileEditorWorkspace>,
    visual: &mut VisualTestContext,
    file: &FileEndpoint,
) {
    open(editor, visual, file, true);
    visual.update(|window, app| {
        editor.update(app, |workspace, cx| {
            let generation = workspace
                .tabs
                .iter()
                .find(|tab| &tab.endpoint == file)
                .unwrap()
                .load_generation;
            workspace.finish_load(
                file.clone(),
                generation,
                Ok(FileEditorSession::open(TextDocument {
                    endpoint: file.clone(),
                    content: "base".to_owned(),
                    line_ending: LineEnding::Lf,
                    revision: revision("initial"),
                    readonly: false,
                    truncated: false,
                })),
                window,
                cx,
            );
        })
    });
    visual.run_until_parked();
}

/// 真实正文与脏状态一起变更，测试亦可在事件排空前提交后续动作以覆盖迟到 InputEvent。
///
/// @author kongweiguang
fn edit(
    editor: &Entity<FileEditorWorkspace>,
    visual: &mut VisualTestContext,
    file: &FileEndpoint,
    text: &str,
) {
    visual.update(|window, app| {
        editor.update(app, |workspace, cx| {
            let state = workspace
                .tabs
                .iter()
                .find(|tab| &tab.endpoint == file)
                .unwrap()
                .editor
                .clone()
                .unwrap();
            state.update(cx, |state, cx| state.set_value(text, window, cx));
            workspace.update_dirty(file, text.to_owned(), cx);
        })
    });
    visual.run_until_parked();
}

/// 仅检查 operation 回执，投影通知不能冒充组关闭结果，也不应冒出逐文件成功事件。
///
/// @author kongweiguang
fn resolutions(
    observer: &Entity<ProjectionObserver>,
    visual: &mut VisualTestContext,
) -> Vec<(String, bool)> {
    visual.run_until_parked();
    observer.read_with(visual, |observer, _| {
        observer
            .events
            .iter()
            .filter_map(|event| match event {
                FileEditorWorkspaceEvent::WorkspaceGroupCloseResolved {
                    operation_id,
                    closed,
                } => Some((operation_id.clone(), *closed)),
                _ => None,
            })
            .collect()
    })
}

/// 准备回执只表示授权完成；正文释放必须等 Root 的工具关闭与工作区提交都成功。
///
/// @author kongweiguang
fn prepared(observer: &Entity<ProjectionObserver>, visual: &mut VisualTestContext) -> Vec<String> {
    visual.run_until_parked();
    observer.read_with(visual, |observer, _| {
        observer
            .events
            .iter()
            .filter_map(|event| match event {
                FileEditorWorkspaceEvent::WorkspaceGroupClosePrepared { operation_id } => {
                    Some(operation_id.clone())
                }
                _ => None,
            })
            .collect()
    })
}

/// 重复与不存在端点不扩大关闭范围；成功事件出现时整组正文已一次性移除。
///
/// @author kongweiguang
#[gpui_kit::test]
fn clean_group_closes_once_without_singleton_receipts(app: &mut TestAppContext) {
    let (_runtime, editor, observer, visual) = fixture(app);
    let a = endpoint("a.txt");
    let b = endpoint("b.txt");
    let keep = endpoint("keep.txt");
    for file in [&a, &b, &keep] {
        loaded(&editor, visual, file);
    }
    editor.update(visual, |workspace, cx| {
        workspace.request_workspace_group_close(
            "group".into(),
            vec![a.clone(), b, a, endpoint("absent.txt")],
            cx,
        )
    });
    assert_eq!(prepared(&observer, visual), vec!["group".to_owned()]);
    assert_eq!(resolutions(&observer, visual), Vec::new());
    assert!(editor.update(visual, |workspace, cx| {
        assert_eq!(workspace.tabs.len(), 3);
        workspace.release_workspace_group_close("group", cx)
    }));
    assert_eq!(resolutions(&observer, visual), vec![("group".into(), true)]);
    editor.read_with(visual, |workspace, _| {
        assert_eq!(workspace.tabs.len(), 1);
        assert_eq!(workspace.tabs[0].endpoint, keep);
    });
    observer.read_with(visual, |observer, _| {
        assert!(!observer.events.iter().any(|event| matches!(
            event,
            FileEditorWorkspaceEvent::WorkspaceTabCloseResolved { .. }
        )))
    });
}

/// 第一项丢弃仅形成票据，第二项取消时第一份正文和 dirty 均未丢失。
///
/// @author kongweiguang
#[gpui_kit::test]
fn discard_first_cancel_second_preserves_every_buffer(app: &mut TestAppContext) {
    let (_runtime, editor, observer, visual) = fixture(app);
    let a = endpoint("a.txt");
    let b = endpoint("b.txt");
    for file in [&a, &b] {
        loaded(&editor, visual, file);
        edit(&editor, visual, file, "draft");
    }
    editor.update(visual, |workspace, cx| {
        workspace.request_workspace_group_close("cancel".into(), vec![a.clone(), b.clone()], cx);
        workspace.confirm_discard_and_close(a.clone(), false, cx);
        assert_eq!(workspace.tabs.len(), 2);
        assert_eq!(
            workspace.confirmation,
            Some(EditorConfirmation::Close(b.clone()))
        );
        workspace.cancel_confirmation(cx);
    });
    assert_eq!(
        resolutions(&observer, visual),
        vec![("cancel".into(), false)]
    );
    editor.read_with(visual, |workspace, app| {
        assert_eq!(workspace.tabs.len(), 2);
        assert!(workspace.tabs.iter().all(|tab| tab.dirty));
        assert_eq!(
            workspace.tabs[0]
                .editor
                .as_ref()
                .unwrap()
                .read(app)
                .text()
                .to_string(),
            "draft"
        );
    });
}

/// 已批准文件的新输入或仅 revision/读取代次变化都会使最终整组提交失败，不先移除其它成员。
///
/// @author kongweiguang
#[gpui_kit::test]
fn approved_snapshot_changes_abort_atomic_commit(app: &mut TestAppContext) {
    for change in ["text", "revision", "generation"] {
        let (_runtime, editor, observer, visual) = fixture(app);
        let a = endpoint("a.txt");
        let b = endpoint("b.txt");
        for file in [&a, &b] {
            loaded(&editor, visual, file);
            edit(&editor, visual, file, "draft");
        }
        editor.update(visual, |workspace, cx| {
            workspace.request_workspace_group_close(
                "changed".into(),
                vec![a.clone(), b.clone()],
                cx,
            );
            workspace.confirm_discard_and_close(a.clone(), false, cx);
        });
        if change == "text" {
            edit(&editor, visual, &a, "newer");
        }
        editor.update(visual, |workspace, cx| {
            let tab = workspace
                .tabs
                .iter_mut()
                .find(|tab| tab.endpoint == a)
                .unwrap();
            if change == "generation" {
                tab.load_generation += 1;
            }
            if change == "revision" {
                let session = tab.session.as_mut().unwrap();
                session
                    .begin_save(
                        "draft",
                        kerminal_domain::files::editor::SaveResolution::CompareRevision,
                    )
                    .unwrap();
                session
                    .save_succeeded(WriteTextResult {
                        bytes_written: 5,
                        revision: revision("changed"),
                    })
                    .unwrap();
                tab.dirty = false;
            }
            workspace.confirm_discard_and_close(b.clone(), false, cx);
            assert_eq!(workspace.tabs.len(), 2);
        });
        assert_eq!(
            resolutions(&observer, visual),
            vec![("changed".into(), false)]
        );
    }
}

/// 保存第一份后取消第二份不回滚明确保存事实；保存过程中继续输入仍须再次确认。
///
/// @author kongweiguang
#[gpui_kit::test]
fn save_receipt_preserves_new_edits_and_cancel_does_not_rollback_saved_revision(
    app: &mut TestAppContext,
) {
    for edit_while_saving in [false, true] {
        let (_runtime, editor, observer, visual) = fixture(app);
        let a = endpoint("a.txt");
        let b = endpoint("b.txt");
        for file in [&a, &b] {
            loaded(&editor, visual, file);
            edit(&editor, visual, file, "draft");
        }
        editor.update(visual, |workspace, cx| {
            workspace.request_workspace_group_close("save".into(), vec![a.clone(), b.clone()], cx);
            workspace.confirm_save_and_close(a.clone(), false, cx);
        });
        if edit_while_saving {
            edit(&editor, visual, &a, "newer");
        }
        editor.update(visual, |workspace, cx| {
            let generation = workspace.tabs[0]
                .save_operation
                .as_ref()
                .unwrap()
                .generation;
            workspace.finish_save(
                a.clone(),
                generation,
                Ok(WriteTextResult {
                    bytes_written: 5,
                    revision: revision("saved"),
                }),
                true,
                false,
                cx,
            );
            assert_eq!(workspace.tabs.len(), 2);
            assert_eq!(
                workspace.confirmation,
                Some(EditorConfirmation::Close(if edit_while_saving {
                    a.clone()
                } else {
                    b.clone()
                }))
            );
            workspace.cancel_confirmation(cx);
        });
        assert_eq!(resolutions(&observer, visual), vec![("save".into(), false)]);
        editor.read_with(visual, |workspace, _| {
            assert_eq!(workspace.tabs.len(), 2);
            assert_eq!(workspace.tabs[0].dirty, edit_while_saving);
            assert_eq!(
                workspace.tabs[0].session.as_ref().unwrap().revision(),
                &revision("saved")
            );
        });
    }
}

/// 单文件抢占和退出都必须结清组 owner，旧保存回执不能随后删除原正文。
///
/// @author kongweiguang
#[gpui_kit::test]
fn singleton_or_quit_revokes_group_and_late_save_close_authorization(app: &mut TestAppContext) {
    for quit in [false, true] {
        let (_runtime, editor, observer, visual) = fixture(app);
        let a = endpoint("a.txt");
        let b = endpoint("b.txt");
        for file in [&a, &b] {
            loaded(&editor, visual, file);
            edit(&editor, visual, file, "draft");
        }
        editor.update(visual, |workspace, cx| {
            workspace.request_workspace_group_close("old".into(), vec![a.clone(), b.clone()], cx);
            workspace.confirm_save_and_close(a.clone(), false, cx);
            let generation = workspace.tabs[0]
                .save_operation
                .as_ref()
                .unwrap()
                .generation;
            if quit {
                assert!(!workspace.request_quit(cx));
            } else {
                workspace.request_close(b.clone(), cx);
            }
            workspace.finish_save(
                a.clone(),
                generation,
                Ok(WriteTextResult {
                    bytes_written: 5,
                    revision: revision("saved"),
                }),
                true,
                false,
                cx,
            );
            assert_eq!(workspace.tabs.len(), 2);
            assert!(workspace.pending_workspace_group_close.is_none());
        });
        assert_eq!(resolutions(&observer, visual), vec![("old".into(), false)]);
    }
}

/// 失败或取消读回必须撤销原组，随后普通重试成功不能继承旧关闭授权。
///
/// @author kongweiguang
#[gpui_kit::test]
fn unknown_readback_failure_and_cancellation_keep_group_and_retry_open(app: &mut TestAppContext) {
    for cancel in [false, true] {
        let (_runtime, editor, observer, visual) = fixture(app);
        let a = endpoint("a.txt");
        let b = endpoint("b.txt");
        for file in [&a, &b] {
            loaded(&editor, visual, file);
        }
        editor.update(visual, |workspace, cx| {
            let session = workspace.tabs[0].session.as_mut().unwrap();
            session
                .begin_save(
                    "uncertain",
                    kerminal_domain::files::editor::SaveResolution::CompareRevision,
                )
                .unwrap();
            session.save_outcome_unknown().unwrap();
            workspace.request_workspace_group_close("readback".into(), vec![a.clone(), b], cx);
            workspace.confirm_reload(a.clone(), false, true, cx);
            if cancel {
                workspace.cancel_load(a.clone(), cx);
            }
        });
        if !cancel {
            visual.update(|window, app| {
                editor.update(app, |workspace, cx| {
                    let generation = workspace.tabs[0].load_generation;
                    workspace.finish_load(
                        a.clone(),
                        generation,
                        Err(AppError::unsupported("fixture readback failed")),
                        window,
                        cx,
                    );
                })
            });
        }
        assert_eq!(
            resolutions(&observer, visual),
            vec![("readback".into(), false)]
        );
        editor.update(visual, |workspace, cx| {
            workspace.start_load(a.clone(), false, cx)
        });
        visual.update(|window, app| {
            editor.update(app, |workspace, cx| {
                let generation = workspace.tabs[0].load_generation;
                workspace.finish_load(
                    a.clone(),
                    generation,
                    Ok(FileEditorSession::open(TextDocument {
                        endpoint: a,
                        content: "readback".into(),
                        line_ending: LineEnding::Lf,
                        revision: revision("readback"),
                        readonly: false,
                        truncated: false,
                    })),
                    window,
                    cx,
                );
                assert_eq!(workspace.tabs.len(), 2);
            })
        });
    }
}

/// 普通写失败终结整组；冲突仍复用二次覆盖确认，取消时全部正文保留。
///
/// @author kongweiguang
#[gpui_kit::test]
fn save_failure_and_conflict_keep_all_group_buffers(app: &mut TestAppContext) {
    for conflict in [false, true] {
        let (_runtime, editor, observer, visual) = fixture(app);
        let a = endpoint("a.txt");
        let b = endpoint("b.txt");
        for file in [&a, &b] {
            loaded(&editor, visual, file);
            edit(&editor, visual, file, "draft");
        }
        editor.update(visual, |workspace, cx| {
            workspace.request_workspace_group_close("failed".into(), vec![a.clone(), b], cx);
            workspace.confirm_save_and_close(a.clone(), false, cx);
            let generation = workspace.tabs[0]
                .save_operation
                .as_ref()
                .unwrap()
                .generation;
            let error = if conflict {
                AppError::conflict("fixture conflict")
            } else {
                AppError::unsupported("fixture failure")
            };
            workspace.finish_save(a, generation, Err(error), true, false, cx);
            if conflict {
                assert!(matches!(
                    workspace.confirmation,
                    Some(EditorConfirmation::Overwrite { .. })
                ));
                workspace.cancel_confirmation(cx);
            }
            assert_eq!(workspace.tabs.len(), 2);
            assert!(workspace.tabs.iter().all(|tab| tab.dirty));
        });
        assert_eq!(
            resolutions(&observer, visual),
            vec![("failed".into(), false)]
        );
    }
}

/// 未知提交仅接受当前 endpoint 和确认读回代次；旧成功不能推进，正确读回可完成整组。
///
/// @author kongweiguang
#[gpui_kit::test]
fn unknown_readback_success_requires_exact_current_generation(app: &mut TestAppContext) {
    let (_runtime, editor, observer, visual) = fixture(app);
    let a = endpoint("a.txt");
    let b = endpoint("b.txt");
    for file in [&a, &b] {
        loaded(&editor, visual, file);
    }
    editor.update(visual, |workspace, cx| {
        let session = workspace.tabs[0].session.as_mut().unwrap();
        session
            .begin_save(
                "uncertain",
                kerminal_domain::files::editor::SaveResolution::CompareRevision,
            )
            .unwrap();
        session.save_outcome_unknown().unwrap();
        workspace.request_workspace_group_close("known".into(), vec![a.clone(), b], cx);
        workspace.confirm_reload(a.clone(), false, true, cx);
        let generation = workspace.tabs[0].load_generation;
        workspace.resolve_workspace_close(&a, Some(generation - 1), false, cx);
        assert!(workspace.pending_workspace_group_close.is_some());
    });
    visual.update(|window, app| {
        editor.update(app, |workspace, cx| {
            let generation = workspace.tabs[0].load_generation;
            workspace.finish_load(
                a.clone(),
                generation - 1,
                Ok(FileEditorSession::open(TextDocument {
                    endpoint: a.clone(),
                    content: "stale".into(),
                    line_ending: LineEnding::Lf,
                    revision: revision("stale"),
                    readonly: false,
                    truncated: false,
                })),
                window,
                cx,
            );
            assert_eq!(workspace.tabs.len(), 2);
            workspace.finish_load(
                a.clone(),
                generation,
                Ok(FileEditorSession::open(TextDocument {
                    endpoint: a,
                    content: "confirmed".into(),
                    line_ending: LineEnding::Lf,
                    revision: revision("confirmed"),
                    readonly: false,
                    truncated: false,
                })),
                window,
                cx,
            );
            assert_eq!(workspace.tabs.len(), 2);
            assert!(workspace.has_prepared_workspace_group_close("known"));
            assert!(workspace.release_workspace_group_close("known", cx));
            assert!(workspace.tabs.is_empty());
        })
    });
    assert_eq!(resolutions(&observer, visual), vec![("known".into(), true)]);
}

/// 默认独立宿主保留原文件带，嵌入只隐藏导航，不改变正文、工具条和搜索状态。
///
/// @author kongweiguang
#[gpui_kit::test]
fn embedded_navigation_defaults_off_and_keeps_shared_buffer(app: &mut TestAppContext) {
    let (_runtime, editor, _observer, visual) = fixture(app);
    let a = endpoint("a.txt");
    loaded(&editor, visual, &a);
    editor.update(visual, |workspace, cx| {
        assert!(!workspace.embedded_navigation);
        let original = workspace.tabs[0].editor.clone().unwrap();
        workspace.set_embedded_navigation(true, cx);
        assert!(workspace.embedded_navigation);
        assert_eq!(workspace.tabs[0].editor.as_ref(), Some(&original));
        workspace.set_embedded_navigation(false, cx);
        assert!(!workspace.embedded_navigation);
    });
}
