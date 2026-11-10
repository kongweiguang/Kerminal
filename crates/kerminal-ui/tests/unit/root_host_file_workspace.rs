//! 文件宿主的 Root 导航与编辑器回执回归；文件端口全部为内存实现。
//!
//! @author kongweiguang

use super::*;

use std::sync::Arc;

use async_trait::async_trait;
use gpui_kit::{TestAppContext, VisualTestContext};
use kerminal_application::{
    files::{FileAccessProvider, FileBookmarkRepository, FileService, editor::FileEditorService},
    workspace::WorkspaceSessionSnapshot,
};
use kerminal_domain::files::{
    BookmarkTarget, DeleteTargetKind, DirectoryListing, FileBookmark, FilePreview, FileRevision,
    LineEnding, TextDocument, WriteTextRequest, WriteTextResult,
};
use tokio::runtime::Runtime;

use crate::features::workspace_controller::{WorkspaceController, WorkspaceRestoreFailure};

/// 所有端点只读内存正文，跨主机测试不会访问磁盘、网络或真实用户根。
struct MemoryFiles;

#[async_trait]
impl FileAccessProvider for MemoryFiles {
    /// 完整端点身份由生产服务保留，fixture 不以协议差异拒绝读取。
    fn supports(&self, _: &FileEndpoint) -> bool {
        true
    }

    /// 本组不模拟目录内容，防止把导航状态测试误称为目录 I/O 验收。
    async fn list_directory(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
    ) -> AppResult<DirectoryListing> {
        Err(AppError::unsupported(
            "memory fixture does not list directories",
        ))
    }

    /// 不经预览入口读取正文，保持测试只覆盖生产编辑用例。
    async fn preview_file(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: usize,
    ) -> AppResult<FilePreview> {
        Err(AppError::unsupported("memory fixture does not preview"))
    }

    /// 同一正文与 revision 允许真实编辑器计算 dirty，不伪造 UI 标志。
    async fn read_text_file(
        &self,
        _: CancellationToken,
        endpoint: FileEndpoint,
        _: usize,
    ) -> AppResult<TextDocument> {
        Ok(TextDocument {
            endpoint,
            content: "original\n".into(),
            line_ending: LineEnding::Lf,
            revision: FileRevision {
                size: 9,
                modified: None,
                permissions: None,
                content_digest: None,
            },
            readonly: false,
            truncated: false,
        })
    }

    /// 保存不是本次导航测试目标，明确拒绝而不伪造写入成功。
    async fn write_text_file(
        &self,
        _: CancellationToken,
        _: WriteTextRequest,
    ) -> AppResult<WriteTextResult> {
        Err(AppError::unsupported("memory fixture does not write"))
    }

    /// 禁止 fixture 意外创建真实目录。
    async fn create_directory(&self, _: CancellationToken, _: FileEndpoint) -> AppResult<()> {
        Err(AppError::unsupported("memory fixture does not mutate"))
    }

    /// 禁止 fixture 意外重命名真实文件。
    async fn rename(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: FileEndpoint,
    ) -> AppResult<()> {
        Err(AppError::unsupported("memory fixture does not mutate"))
    }

    /// 禁止 fixture 意外修改真实权限。
    async fn chmod(&self, _: CancellationToken, _: FileEndpoint, _: u32) -> AppResult<()> {
        Err(AppError::unsupported("memory fixture does not mutate"))
    }

    /// 禁止 fixture 意外删除真实文件。
    async fn delete(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: DeleteTargetKind,
    ) -> AppResult<()> {
        Err(AppError::unsupported("memory fixture does not mutate"))
    }
}

/// 书签不属于宿主导航，使用无持久副作用的端口。
struct NoBookmarks;

#[async_trait]
impl FileBookmarkRepository for NoBookmarks {
    /// 不读取真实书签，以免 fixture 引入用户状态。
    async fn list(&self, _: CancellationToken, _: BookmarkTarget) -> AppResult<Vec<FileBookmark>> {
        Ok(Vec::new())
    }

    /// 不允许导航测试顺带写入书签。
    async fn set(
        &self,
        _: CancellationToken,
        _: FileBookmark,
        _: bool,
    ) -> AppResult<Vec<FileBookmark>> {
        Err(AppError::unsupported(
            "memory fixture does not change bookmarks",
        ))
    }
}

/// 交替推进两个真实执行器，确保断言覆盖异步回执而不依赖固定休眠。
fn settle(runtime: &Runtime, visual: &mut VisualTestContext) {
    for _ in 0..32 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
    visual.run_until_parked();
}

/// 复用既有 Root 假服务，只注入内存编辑器并显式安装空工作区，不启动普通持久化服务。
fn fixture(app: &mut TestAppContext) -> (Runtime, Entity<RootView>, &mut VisualTestContext) {
    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, |mut services| {
            let files = FileService::new(vec![Arc::new(MemoryFiles)], Arc::new(NoBookmarks))
                .expect("memory file service");
            services.file_editor = Some(FileEditorService::new(files));
            services
        });
    settle(&runtime, visual);
    root.update(visual, |root, _| {
        root.workspace_controller =
            Some(WorkspaceController::from_missing().expect("empty workspace"));
        root.workspace_files = WorkspaceFileRuntime::from_snapshot(
            root.workspace_controller
                .as_ref()
                .expect("controller")
                .snapshot(),
        );
        root.workspace_load_pending = false;
        root.center_panel = CenterPanel::Terminal;
    });
    (runtime, root, visual)
}

/// 端点仅包含不可连接的测试身份，文件路径变化不得改变同一主机宿主。
fn remote(host: &str, path: &str) -> FileEndpoint {
    FileEndpoint::Remote {
        host_id: HostId::parse(host).expect("fixture host"),
        path: RemotePath::parse(path).expect("fixture path"),
    }
}

/// 普通打开调用生产入口，而不是在测试里手动拼接顶层 FileWorkspace。
fn open(root: &Entity<RootView>, visual: &mut VisualTestContext, endpoint: FileEndpoint) -> TabId {
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            assert!(root.open_file_in_host_workspace(endpoint, None, window, cx));
            root.workspace_controller
                .as_ref()
                .expect("controller")
                .snapshot()
                .active_tab_id
                .clone()
                .expect("active owner")
        })
    })
}

/// 只从 controller 读取权威状态，避免测试误把运行态副本当作已提交导航。
fn owner_state(root: &RootView, owner: &TabId) -> FileWorkspaceState {
    let tab = root
        .workspace_controller
        .as_ref()
        .expect("controller")
        .snapshot()
        .terminal_tabs
        .iter()
        .find(|tab| tab.id() == owner)
        .expect("owner survives");
    let WorkspaceSessionTab::FileWorkspace { state, .. } = tab else {
        panic!("expected file workspace");
    };
    state.clone()
}

/// 通过生产宿主实际按钮命中切换文档及新建目录，不用直接调用事件处理器替代点击证据。
/// @author kongweiguang
#[gpui_kit::test]
fn production_tab_and_new_directory_buttons_keep_the_same_owner(app: &mut TestAppContext) {
    let (runtime, root, visual) = fixture(app);
    let first = remote("click-host", "/etc/first.conf");
    let owner = open(&root, visual, first.clone());
    open(&root, visual, remote("click-host", "/srv/second.conf"));
    settle(&runtime, visual);
    let state = root.read_with(visual, |root, _| owner_state(root, &owner));
    let first_id = state
        .items
        .iter()
        .find(|item| item.path() == "/etc/first.conf")
        .expect("first document")
        .id()
        .clone();
    visual.simulate_resize(gpui_kit::size(gpui_kit::px(1200.), gpui_kit::px(800.)));
    for _ in 0..2 {
        visual.update(|window, app| window.draw(app).clear(app));
    }
    // 锁定版 debug_bounds 要求静态选择器；仅测试进程保留这一条动态身份字符串。
    let selector = Box::leak(format!("host-file-item-{}", first_id.as_str()).into_boxed_str());
    let bounds = visual
        .debug_bounds(selector)
        .expect("production document button");
    visual.simulate_mouse_move(bounds.center(), None, Default::default());
    visual.simulate_mouse_down(
        bounds.center(),
        gpui_kit::MouseButton::Left,
        Default::default(),
    );
    visual.simulate_mouse_up(
        bounds.center(),
        gpui_kit::MouseButton::Left,
        Default::default(),
    );
    settle(&runtime, visual);
    assert_eq!(
        root.read_with(visual, |root, _| root.workspace_files.file_endpoint(&owner)),
        Some(first)
    );
    for _ in 0..2 {
        visual.update(|window, app| window.draw(app).clear(app));
    }
    let bounds = visual
        .debug_bounds("host-file-new-directory")
        .expect("production new directory button");
    visual.simulate_mouse_down(
        bounds.center(),
        gpui_kit::MouseButton::Left,
        Default::default(),
    );
    visual.simulate_mouse_up(
        bounds.center(),
        gpui_kit::MouseButton::Left,
        Default::default(),
    );
    settle(&runtime, visual);
    let state = root.read_with(visual, |root, _| owner_state(root, &owner));
    assert_eq!(
        state
            .items
            .iter()
            .filter(|item| matches!(item, FileWorkspaceItem::Directory { .. }))
            .count(),
        2
    );
    assert_eq!(
        root.read_with(visual, |root, _| root
            .workspace_controller
            .as_ref()
            .unwrap()
            .snapshot()
            .terminal_tabs
            .len()),
        1
    );
}

/// 外层文档右键复用原编辑器菜单，复制冻结端点路径而不是宿主或另一份活动文档。
/// @author kongweiguang
#[gpui_kit::test]
fn production_document_menu_copies_the_captured_path(app: &mut TestAppContext) {
    let (runtime, root, visual) = fixture(app);
    let owner = open(&root, visual, remote("menu-host", "/etc/first.conf"));
    settle(&runtime, visual);
    let item = root.read_with(visual, |root, _| {
        owner_state(root, &owner)
            .items
            .iter()
            .find(|item| matches!(item, FileWorkspaceItem::Document { .. }))
            .unwrap()
            .id()
            .clone()
    });
    visual.simulate_resize(gpui_kit::size(gpui_kit::px(1200.), gpui_kit::px(800.)));
    for _ in 0..2 {
        visual.update(|window, app| window.draw(app).clear(app));
    }
    let bounds = visual
        .update(|window, _| window.debug_selector_bounds(&format!("host-file-item-{item}")))
        .expect("document button");
    visual.simulate_mouse_move(bounds.center(), None, Default::default());
    visual.simulate_mouse_down(
        bounds.center(),
        gpui_kit::MouseButton::Right,
        Default::default(),
    );
    visual.simulate_mouse_up(
        bounds.center(),
        gpui_kit::MouseButton::Right,
        Default::default(),
    );
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
    assert!(visual.debug_bounds("rr-popup-menu").is_some());
    visual.simulate_keystrokes("down enter");
    visual.run_until_parked();
    let clipboard = visual.update(|_, app| app.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(clipboard.as_deref(), Some("/etc/first.conf"));
    root.read_with(visual, |root, _| {
        assert_eq!(
            root.workspace_controller
                .as_ref()
                .unwrap()
                .snapshot()
                .terminal_tabs
                .len(),
            1
        )
    });
}

/// 通过生产取消按钮验证读取反馈只替换工具栏槽位，避免绕过点击接线而漏掉正文跳动。
/// @author kongweiguang
#[gpui_kit::test]
fn reload_feedback_does_not_displace_the_document(app: &mut TestAppContext) {
    let (runtime, root, visual) = fixture(app);
    let endpoint = remote("layout-host", "/etc/config.txt");
    open(&root, visual, endpoint.clone());
    settle(&runtime, visual);
    visual.simulate_resize(gpui_kit::size(gpui_kit::px(1200.), gpui_kit::px(800.)));
    for _ in 0..2 {
        visual.update(|window, app| window.draw(app).clear(app));
    }
    let before = visual
        .debug_bounds("file-editor-document")
        .expect("loaded document")
        .origin
        .y;
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let editor = root.file_editor_workspace.clone().unwrap();
            assert!(editor.update(cx, |editor, cx| editor.request_user_reload(
                endpoint.clone(),
                None,
                cx
            )));
            assert!(
                editor
                    .read(cx)
                    .workspace_projection()
                    .tabs
                    .iter()
                    .any(|tab| tab.endpoint == endpoint && tab.loading)
            );
        });
        window.draw(app).clear(app);
        assert_eq!(
            window
                .debug_selector_bounds("file-editor-document")
                .unwrap()
                .origin
                .y,
            before
        );
    });
    let cancel = visual
        .debug_bounds("file-editor-cancel-load")
        .expect("production cancel read button");
    visual.simulate_click(cancel.center(), Default::default());
    root.read_with(visual, |root, cx| {
        assert!(
            root.file_editor_workspace
                .as_ref()
                .expect("editor workspace")
                .read(cx)
                .workspace_projection()
                .tabs
                .iter()
                .any(|tab| tab.endpoint == endpoint && !tab.loading)
        );
    });
    settle(&runtime, visual);
    for _ in 0..2 {
        visual.update(|window, app| window.draw(app).clear(app));
    }
    assert_eq!(
        visual
            .debug_bounds("file-editor-document")
            .unwrap()
            .origin
            .y,
        before
    );
}

/// 保存未知与其它资源操作必须在释放正文前拦截内部关闭；门禁解除后原目标可继续关闭而不遗留幽灵标签。
/// @author kongweiguang
#[gpui_kit::test]
fn blocked_internal_close_preserves_buffers_and_recovers_after_the_gate(app: &mut TestAppContext) {
    use kerminal_application::workspace::{
        WorkspaceSessionLoad, WorkspaceSessionRevision, WorkspaceSessionSaveOutcome,
    };

    for unknown_save in [true, false] {
        let (runtime, root, visual) = fixture(app);
        let endpoint = remote("close-gate-host", "/etc/config.txt");
        let owner = open(&root, visual, endpoint.clone());
        settle(&runtime, visual);
        visual.update(|window, app| {
            root.update(app, |root, cx| {
                root.new_host_directory_tab(&owner, window, cx);
                let before = owner_state(root, &owner);
                let document = before
                    .items
                    .iter()
                    .find(|item| matches!(item, FileWorkspaceItem::Document { .. }))
                    .unwrap()
                    .id()
                    .clone();
                let directory = before.active_item_id.clone().unwrap();
                let controller = root.workspace_controller.as_mut().unwrap();
                let request = if unknown_save {
                    let request = controller.begin_save().unwrap().unwrap();
                    controller
                        .finish_save(
                            &request.request_id,
                            WorkspaceSessionSaveOutcome::OutcomeUnknown,
                        )
                        .unwrap();
                    Some(request)
                } else {
                    controller
                        .apply_tab_event(crate::features::workspace_tabs::WorkspaceTabEvent::Close(
                            owner.clone(),
                        ))
                        .unwrap();
                    None
                };
                root.close_host_file_item(&owner, &document, window, cx);
                root.close_host_file_item(&owner, &directory, window, cx);
                assert_eq!(owner_state(root, &owner), before);
                assert!(
                    root.file_editor_workspace
                        .as_ref()
                        .unwrap()
                        .read(cx)
                        .workspace_projection()
                        .tabs
                        .iter()
                        .any(|tab| tab.endpoint == endpoint)
                );
                let controller = root.workspace_controller.as_mut().unwrap();
                if let Some(request) = request {
                    controller
                        .resolve_unknown_readback(WorkspaceSessionLoad::Loaded {
                            document: request.candidate_document,
                            revision: WorkspaceSessionRevision::new("confirmed-gate-fixture"),
                        })
                        .unwrap();
                } else {
                    assert!(controller.discard_pending_resource_operation());
                }
                root.close_host_file_item(&owner, &document, window, cx);
                root.close_host_file_item(&owner, &directory, window, cx);
            });
        });
        settle(&runtime, visual);
        root.read_with(visual, |root, cx| {
            let state = owner_state(root, &owner);
            assert_eq!(state.items.len(), 1);
            assert!(matches!(
                state.active_item(),
                Some(FileWorkspaceItem::Directory { .. })
            ));
            assert!(
                !root
                    .file_editor_workspace
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .workspace_projection()
                    .tabs
                    .iter()
                    .any(|tab| tab.endpoint == endpoint)
            );
        });
    }
}

/// 恢复中断可能留下没有编辑器登记的文档；缺少服务或无法解析端点时仍应只提交描述删除。
/// @author kongweiguang
#[gpui_kit::test]
fn interrupted_unregistered_document_can_close_without_an_editor_tab(app: &mut TestAppContext) {
    for (missing_editor, invalid_source) in [(false, false), (true, false), (false, true)] {
        let (_runtime, root, visual) = fixture(app);
        let endpoint = remote("orphan-host", "/srv/orphan.txt");
        visual.update(|window, app| {
            root.update(app, |root, cx| {
                let (descriptor, transient) =
                    crate::root::workspace_file_navigation::file_navigation_descriptor(
                        &endpoint, None,
                    )
                    .expect("document descriptor");
                let owner = root
                    .open_host_document_descriptor(
                        descriptor,
                        endpoint.clone(),
                        None,
                        transient,
                        cx,
                    )
                    .expect("descriptor committed before restore");
                let document = owner_state(root, &owner).active_item_id.unwrap();
                if invalid_source {
                    root.update_host_file_state(&owner, |state| {
                        let FileWorkspaceItem::Document { source, .. } = state
                            .items
                            .iter_mut()
                            .find(|entry| entry.id() == &document)
                            .expect("document")
                        else {
                            panic!("expected document");
                        };
                        *source = WorkspaceFileSource::Container;
                        Ok(())
                    })
                    .expect("simulate a source that cannot restore against this host");
                    assert!(
                        root.workspace_files
                            .host_document_endpoint(&owner, &document)
                            .is_none()
                    );
                }
                root.workspace_files.set_result(
                    &owner,
                    Err((
                        WorkspaceRestoreFailure::ResourceFailure,
                        "interrupted".into(),
                    )),
                );
                assert!(
                    !root
                        .workspace_files
                        .documents
                        .contains_key(&(owner.clone(), document.clone()))
                );
                if missing_editor {
                    root.file_editor_workspace = None;
                } else {
                    assert!(
                        root.file_editor_workspace
                            .as_ref()
                            .unwrap()
                            .read(cx)
                            .workspace_projection()
                            .tabs
                            .is_empty()
                    );
                }
                root.close_host_file_item(&owner, &document, window, cx);
                let state = owner_state(root, &owner);
                assert_eq!(state.items.len(), 1);
                assert!(matches!(
                    state.active_item(),
                    Some(FileWorkspaceItem::Directory { .. })
                ));
                let WorkspaceSessionTab::FileWorkspace {
                    state: runtime_state,
                    ..
                } = &root.workspace_files.tabs.get(&owner).unwrap().tab
                else {
                    panic!("runtime must remain a file workspace");
                };
                assert_eq!(runtime_state, &state);
                assert!(
                    !root
                        .workspace_files
                        .documents
                        .contains_key(&(owner, document))
                );
            });
        });
    }
}

/// 跨 owner 的同端点脏稿不能被未装配描述误当作自己的缓冲；两类 controller 门禁解除后才提交关闭。
/// @author kongweiguang
#[gpui_kit::test]
fn unregistered_close_preserves_shared_dirty_buffer_and_obeys_gates(app: &mut TestAppContext) {
    use kerminal_application::workspace::{
        WorkspaceSessionLoad, WorkspaceSessionRevision, WorkspaceSessionSaveOutcome,
    };

    for unknown_save in [true, false] {
        let (runtime, root, visual) = fixture(app);
        let endpoint = remote("shared-host", "/srv/shared.txt");
        let original_owner = open(&root, visual, endpoint.clone());
        settle(&runtime, visual);
        visual.update(|window, app| {
            root.update(app, |root, cx| {
                let editor = root.file_editor_workspace.clone().unwrap();
                assert!(editor.update(cx, |editor, cx| editor.focus_active_editor(window, cx)));
            });
            window.draw(app).clear(app);
        });
        visual.simulate_keystrokes("x");
        settle(&runtime, visual);
        visual.update(|window, app| {
            root.update(app, |root, cx| {
                let editor = root.file_editor_workspace.clone().unwrap();
                assert!(
                    editor
                        .read(cx)
                        .workspace_projection()
                        .tabs
                        .iter()
                        .any(|tab| tab.endpoint == endpoint && tab.dirty)
                );
                let original_state = owner_state(root, &original_owner);
                let original_document = original_state
                    .items
                    .iter()
                    .find(|entry| matches!(entry, FileWorkspaceItem::Document { .. }))
                    .unwrap()
                    .id()
                    .clone();
                assert!(
                    root.workspace_files
                        .documents
                        .contains_key(&(original_owner.clone(), original_document))
                );

                // 模拟恢复快照中另一 owner 的文档尚未装配，但已有 owner 正在引用同一共享缓冲。
                let mut snapshot = root
                    .workspace_controller
                    .as_ref()
                    .unwrap()
                    .snapshot()
                    .clone();
                let mut restored = snapshot
                    .terminal_tabs
                    .iter()
                    .find(|tab| tab.id() == &original_owner)
                    .unwrap()
                    .clone();
                let orphan_owner = TabId::new();
                let orphan_document = TabId::new();
                let WorkspaceSessionTab::FileWorkspace { id, state, .. } = &mut restored else {
                    panic!("expected file workspace");
                };
                *id = orphan_owner.clone();
                for entry in &mut state.items {
                    match entry {
                        FileWorkspaceItem::Directory { id, .. } => *id = TabId::new(),
                        FileWorkspaceItem::Document { id, .. } => *id = orphan_document.clone(),
                    }
                }
                state.active_item_id = Some(orphan_document.clone());
                snapshot.terminal_tabs.push(restored.clone());
                root.workspace_controller
                    .as_mut()
                    .unwrap()
                    .synchronize_snapshot(snapshot)
                    .unwrap();
                root.workspace_files.install_host_descriptor(
                    restored,
                    Some(endpoint.clone()),
                    None,
                    false,
                );
                assert!(
                    !root
                        .workspace_files
                        .documents
                        .contains_key(&(orphan_owner.clone(), orphan_document.clone()))
                );
                let before = owner_state(root, &orphan_owner);
                let controller = root.workspace_controller.as_mut().unwrap();
                let request = if unknown_save {
                    let request = controller.begin_save().unwrap().unwrap();
                    controller
                        .finish_save(
                            &request.request_id,
                            WorkspaceSessionSaveOutcome::OutcomeUnknown,
                        )
                        .unwrap();
                    Some(request)
                } else {
                    controller
                        .apply_tab_event(crate::features::workspace_tabs::WorkspaceTabEvent::Close(
                            orphan_owner.clone(),
                        ))
                        .unwrap();
                    None
                };
                root.close_host_file_item(&orphan_owner, &orphan_document, window, cx);
                assert_eq!(owner_state(root, &orphan_owner), before);
                assert!(!editor.read(cx).root_confirmation_active());
                let controller = root.workspace_controller.as_mut().unwrap();
                if let Some(request) = request {
                    controller
                        .resolve_unknown_readback(WorkspaceSessionLoad::Loaded {
                            document: request.candidate_document,
                            revision: WorkspaceSessionRevision::new("confirmed-orphan-fixture"),
                        })
                        .unwrap();
                } else {
                    assert!(controller.discard_pending_resource_operation());
                }
                root.close_host_file_item(&orphan_owner, &orphan_document, window, cx);
                assert!(matches!(
                    owner_state(root, &orphan_owner).active_item(),
                    Some(FileWorkspaceItem::Directory { .. })
                ));
                assert_eq!(owner_state(root, &original_owner), original_state);
                let projection = editor.read(cx).workspace_projection();
                assert!(
                    projection
                        .tabs
                        .iter()
                        .any(|tab| tab.endpoint == endpoint && tab.dirty)
                );
                assert_eq!(projection.active_endpoint.as_ref(), Some(&endpoint));
                assert!(!editor.read(cx).root_confirmation_active());
                assert_eq!(
                    root.workspace_controller
                        .as_ref()
                        .unwrap()
                        .snapshot()
                        .active_tab_id
                        .as_ref(),
                    Some(&original_owner)
                );
                assert_eq!(root.center_panel, CenterPanel::Editor);
            });
        });
    }
}

/// 已登记文档不能走孤儿快路，真实脏稿关闭仍需等待编辑器的显式授权。
/// @author kongweiguang
#[gpui_kit::test]
fn registered_dirty_document_still_requests_close_confirmation(app: &mut TestAppContext) {
    let (runtime, root, visual) = fixture(app);
    let endpoint = remote("registered-host", "/srv/dirty.txt");
    let owner = open(&root, visual, endpoint.clone());
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            assert!(
                root.file_editor_workspace
                    .clone()
                    .unwrap()
                    .update(cx, |editor, cx| editor.focus_active_editor(window, cx))
            );
        });
        window.draw(app).clear(app);
    });
    visual.simulate_keystrokes("x");
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let before = owner_state(root, &owner);
            let document = before.active_item_id.clone().unwrap();
            assert!(
                root.workspace_files
                    .documents
                    .contains_key(&(owner.clone(), document.clone()))
            );
            root.close_host_file_item(&owner, &document, window, cx);
            let editor = root.file_editor_workspace.as_ref().unwrap().read(cx);
            assert!(editor.root_confirmation_active());
            assert!(
                editor
                    .workspace_projection()
                    .tabs
                    .iter()
                    .any(|tab| tab.endpoint == endpoint && tab.dirty)
            );
            assert_eq!(owner_state(root, &owner), before);
        });
    });
}

/// 编辑器已经结束关闭但宿主登记尚未收到投影时，也必须经 controller 事务清除残余描述。
/// @author kongweiguang
#[gpui_kit::test]
fn closed_editor_with_stale_document_registration_can_finish_cleanup(app: &mut TestAppContext) {
    let (runtime, root, visual) = fixture(app);
    let endpoint = remote("stale-registration-host", "/srv/config.txt");
    let owner = open(&root, visual, endpoint.clone());
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let document = owner_state(root, &owner).active_item_id.unwrap();
            let key = (owner.clone(), document.clone());
            let editor = root.file_editor_workspace.clone().unwrap();
            assert!(root.workspace_files.documents.contains_key(&key));
            editor.update(cx, |editor, cx| editor.request_close(endpoint.clone(), cx));
            assert!(
                !editor
                    .read(cx)
                    .workspace_projection()
                    .tabs
                    .iter()
                    .any(|tab| tab.endpoint == endpoint)
            );
            assert!(root.workspace_files.documents.contains_key(&key));
            root.close_host_file_item(&owner, &document, window, cx);
            assert!(matches!(
                owner_state(root, &owner).active_item(),
                Some(FileWorkspaceItem::Directory { .. })
            ));
            assert!(!root.workspace_files.documents.contains_key(&key));
        });
    });
}

/// 登记身份与权威描述不一致时不能关闭任何共享缓冲，恢复一致后仍沿正常授权路径处理。
/// @author kongweiguang
#[gpui_kit::test]
fn inconsistent_registered_document_is_retained_until_identity_recovers(app: &mut TestAppContext) {
    let (runtime, root, visual) = fixture(app);
    let endpoint = remote("identity-host", "/srv/config.txt");
    let owner = open(&root, visual, endpoint.clone());
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let before = owner_state(root, &owner);
            let document = before.active_item_id.clone().unwrap();
            let runtime = root.workspace_files.tabs.get_mut(&owner).unwrap();
            let WorkspaceSessionTab::FileWorkspace { state, .. } = &mut runtime.tab else {
                panic!("expected file workspace");
            };
            let FileWorkspaceItem::Document { path, .. } = state
                .items
                .iter_mut()
                .find(|entry| entry.id() == &document)
                .unwrap()
            else {
                panic!("expected document");
            };
            *path = "/srv/other.txt".into();
            root.close_host_file_item(&owner, &document, window, cx);
            assert_eq!(owner_state(root, &owner), before);
            assert!(
                root.workspace_files
                    .documents
                    .contains_key(&(owner.clone(), document.clone()))
            );
            assert!(
                root.file_editor_workspace
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .workspace_projection()
                    .tabs
                    .iter()
                    .any(|tab| tab.endpoint == endpoint)
            );
            let authoritative = root
                .workspace_controller
                .as_ref()
                .unwrap()
                .snapshot()
                .terminal_tabs
                .iter()
                .find(|tab| tab.id() == &owner)
                .unwrap()
                .clone();
            root.workspace_files
                .install_host_descriptor(authoritative.clone(), None, None, false);
            let runtime = root.workspace_files.tabs.get_mut(&owner).unwrap();
            let WorkspaceSessionTab::FileWorkspace { target, .. } = &mut runtime.tab else {
                panic!("expected file workspace");
            };
            *target = WorkspaceTargetRef::Ssh {
                host_id: HostId::parse("other-identity-host").unwrap(),
            };
            // runtime anchor 仍为旧端点，不能只比较路径和缓存 endpoint 而忽略权威宿主身份。
            assert_eq!(
                root.workspace_files
                    .host_document_endpoint(&owner, &document),
                Some(endpoint.clone())
            );
            root.close_host_file_item(&owner, &document, window, cx);
            assert_eq!(owner_state(root, &owner), before);
            assert!(
                root.file_editor_workspace
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .workspace_projection()
                    .tabs
                    .iter()
                    .any(|tab| tab.endpoint == endpoint)
            );
            root.workspace_files
                .install_host_descriptor(authoritative, None, None, false);
            root.close_host_file_item(&owner, &document, window, cx);
        });
    });
    settle(&runtime, visual);
    root.read_with(visual, |root, cx| {
        assert!(matches!(
            owner_state(root, &owner).active_item(),
            Some(FileWorkspaceItem::Directory { .. })
        ));
        assert!(
            !root
                .file_editor_workspace
                .as_ref()
                .unwrap()
                .read(cx)
                .workspace_projection()
                .tabs
                .iter()
                .any(|tab| tab.endpoint == endpoint)
        );
    });
}

/// 最后目录关闭失败后原选择器仍占有准备闩锁，完成选择器后才能重新发起上传。
/// @author kongweiguang
#[gpui_kit::test]
fn last_directory_rejection_preserves_upload_preparation(app: &mut TestAppContext) {
    use crate::features::files::{FileTargetNameLookup, action_test_support as support};

    let (runtime, root, visual) = fixture(app);
    let (files_runtime, files, transfers, executor, _) = support::services();
    let endpoint = remote("preparing-host", "/srv");
    let owner = TabId::new();
    let directory = TabId::new();
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let snapshot = WorkspaceSessionSnapshot {
                pinned_containers: Vec::new(),
                active_tab_id: Some(owner.clone()),
                focused_pane_id: None,
                selected_machine_id: Some("preparing-host".into()),
                terminal_tabs: vec![WorkspaceSessionTab::FileWorkspace {
                    id: owner.clone(),
                    tab_group_id: None,
                    title: "Preparing host".into(),
                    machine_id: "preparing-host".into(),
                    target: WorkspaceTargetRef::Ssh {
                        host_id: HostId::parse("preparing-host").unwrap(),
                    },
                    state: FileWorkspaceState {
                        items: vec![FileWorkspaceItem::Directory {
                            id: directory.clone(),
                            path: "/srv".into(),
                        }],
                        active_item_id: Some(directory.clone()),
                        source_tab_ids: Vec::new(),
                    },
                }],
                terminal_panes: Vec::new(),
                tab_groups: Default::default(),
            };
            let mut controller = WorkspaceController::from_missing().unwrap();
            controller.synchronize_snapshot(snapshot).unwrap();
            root.workspace_files = WorkspaceFileRuntime::from_snapshot(controller.snapshot());
            root.workspace_controller = Some(controller);
            root.workspace_files
                .set_result(&owner, Ok(Some(endpoint.clone())));
            root.workspace_load_pending = false;
            let pane = cx.new(|_| {
                FilesPane::new(
                    files,
                    transfers,
                    executor,
                    FileTargetNameLookup::default(),
                    endpoint.clone(),
                )
            });
            pane.update(cx, |pane, cx| {
                pane.configure_file_workspace(
                    endpoint.clone(),
                    None,
                    vec![TransferOrigin::WorkspaceFile(owner.clone())],
                    cx,
                )
                .unwrap();
                support::seed_compact_listing(pane, endpoint.clone(), Vec::new());
            });
            let subscriptions = RootView::subscribe_files_pane_events(&pane, window, cx);
            root.workspace_files
                .directories
                .insert((owner.clone(), directory.clone()), (pane, subscriptions));
            root.activate_host_file_workspace(&owner, window, cx);
        });
    });
    visual.simulate_resize(gpui_kit::size(gpui_kit::px(1200.), gpui_kit::px(800.)));
    for _ in 0..2 {
        visual.update(|window, app| window.draw(app).clear(app));
    }
    let upload = visual
        .debug_bounds("rr-files-upload-menu")
        .expect("directory upload menu");
    visual.simulate_click(upload.center(), Default::default());
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
    assert!(visual.debug_bounds("rr-popup-menu").is_some());
    visual.simulate_keystrokes("down enter");
    assert!(
        visual.did_prompt_for_paths(),
        "upload must be waiting for the real picker"
    );
    settle(&files_runtime, visual);

    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let before = owner_state(root, &owner);
            assert!(
                !root
                    .workspace_files
                    .directories
                    .get(&(owner.clone(), directory.clone()))
                    .unwrap()
                    .0
                    .read(cx)
                    .file_workspace_directory_close_blocked()
            );
            root.close_host_file_item(&owner, &directory, window, cx);
            assert_eq!(owner_state(root, &owner), before);
            assert!(
                root.workspace_files
                    .directories
                    .contains_key(&(owner.clone(), directory.clone()))
            );
            // 即使迟到关闭事件仍指向一个目录实体，也不能在确认 item 不存在前取消它的准备。
            let stale = TabId::new();
            let pane = root
                .workspace_files
                .directories
                .remove(&(owner.clone(), directory.clone()))
                .unwrap();
            root.workspace_files
                .directories
                .insert((owner.clone(), stale.clone()), pane);
            root.close_host_file_item(&owner, &stale, window, cx);
            assert_eq!(owner_state(root, &owner), before);
            let pane = root
                .workspace_files
                .directories
                .remove(&(owner.clone(), stale))
                .expect("stale close must retain the directory entity");
            root.workspace_files
                .directories
                .insert((owner.clone(), directory.clone()), pane);
        });
    });
    for _ in 0..2 {
        visual.update(|window, app| window.draw(app).clear(app));
    }
    // 若错误地取消准备，第二次上传会打开另一个选择器；原选择器收到回执后仍会留下它。
    let upload = visual.debug_bounds("rr-files-upload-menu").unwrap();
    visual.simulate_click(upload.center(), Default::default());
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
    if visual.debug_bounds("rr-popup-menu").is_some() {
        visual.simulate_keystrokes("down enter");
    }
    visual.simulate_path_prompt_response(|_| None);
    assert!(
        !visual.did_prompt_for_paths(),
        "failed close must not release the original submission"
    );
    settle(&files_runtime, visual);
    settle(&runtime, visual);
    for _ in 0..2 {
        visual.update(|window, app| window.draw(app).clear(app));
    }
    let upload = visual.debug_bounds("rr-files-upload-menu").unwrap();
    visual.simulate_click(upload.center(), Default::default());
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
    assert!(
        visual.debug_bounds("rr-popup-menu").is_some(),
        "the picker receipt must permit a retry"
    );
    visual.simulate_keystrokes("down enter");
    assert!(visual.did_prompt_for_paths());
    let replacement = TabId::new();
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.update_host_file_state(&owner, |state| {
                state.items.push(FileWorkspaceItem::Directory {
                    id: replacement.clone(),
                    path: "/srv".into(),
                });
                Ok(())
            })
            .unwrap();
            root.close_host_file_item(&owner, &directory, window, cx);
            let state = owner_state(root, &owner);
            assert_eq!(state.items.len(), 1);
            assert_eq!(state.active_item_id.as_ref(), Some(&replacement));
            assert!(
                !root
                    .workspace_files
                    .directories
                    .contains_key(&(owner.clone(), directory.clone()))
            );
        });
    });
    visual.simulate_path_prompt_response(|_| None);
    settle(&files_runtime, visual);
    root.read_with(visual, |root, _| {
        assert_eq!(
            owner_state(root, &owner).active_item_id.as_ref(),
            Some(&replacement)
        );
        assert!(
            !root
                .workspace_files
                .directories
                .contains_key(&(owner.clone(), directory.clone()))
        );
    });
}

/// 新目录加号在没有文件 UI 服务时也先提交独立身份，同路径起步不能退化为目标去重或新顶层。
/// @author kongweiguang
#[gpui_kit::test]
fn new_directory_intent_adds_independent_item_without_another_owner(app: &mut TestAppContext) {
    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, |services| services);
    settle(&runtime, visual);
    let owner = TabId::new();
    let original = TabId::new();
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            assert!(root.services.files.is_none());
            assert!(root.services.transfers.is_none());
            assert!(root.file_editor_workspace.is_none());
            let snapshot = WorkspaceSessionSnapshot {
                pinned_containers: Vec::new(),
                active_tab_id: Some(owner.clone()),
                focused_pane_id: None,
                selected_machine_id: Some("host-a".into()),
                terminal_tabs: vec![WorkspaceSessionTab::FileWorkspace {
                    id: owner.clone(),
                    tab_group_id: None,
                    title: "Host files".into(),
                    machine_id: "host-a".into(),
                    target: WorkspaceTargetRef::Ssh {
                        host_id: HostId::parse("host-a").expect("fixture host"),
                    },
                    state: FileWorkspaceState {
                        items: vec![FileWorkspaceItem::Directory {
                            id: original.clone(),
                            path: "/srv".into(),
                        }],
                        active_item_id: Some(original.clone()),
                        source_tab_ids: Vec::new(),
                    },
                }],
                terminal_panes: Vec::new(),
                tab_groups: Default::default(),
            };
            let mut controller = WorkspaceController::from_missing().expect("controller");
            controller
                .synchronize_snapshot(snapshot)
                .expect("seed directory owner");
            root.workspace_files = WorkspaceFileRuntime::from_snapshot(controller.snapshot());
            root.workspace_controller = Some(controller);
            root.workspace_load_pending = false;

            root.new_host_directory_tab(&owner, window, cx);
            let state = owner_state(root, &owner);
            assert_eq!(state.items.len(), 2);
            assert_eq!(state.items[0].id(), &original);
            assert_ne!(state.items[1].id(), &original);
            assert_eq!(state.items[0].path(), "/srv");
            assert_eq!(state.items[1].path(), "/srv");
            assert_eq!(state.active_item_id.as_ref(), Some(state.items[1].id()));
            let snapshot = root
                .workspace_controller
                .as_ref()
                .expect("controller")
                .snapshot();
            assert_eq!(snapshot.terminal_tabs.len(), 1);
            assert_eq!(snapshot.terminal_tabs[0].id(), &owner);
            assert_eq!(snapshot.active_tab_id.as_ref(), Some(&owner));
            assert!(snapshot.terminal_panes.is_empty());
        });
    });
}

/// 普通打开跨目录文件仍进入同一主机宿主，保留首目录且不伪造终端 pane。
#[gpui_kit::test]
fn ordinary_files_across_paths_share_one_top_level_owner(app: &mut TestAppContext) {
    let (runtime, root, visual) = fixture(app);
    let first = open(&root, visual, remote("host-a", "/srv/a.txt"));
    let second = open(&root, visual, remote("host-a", "/opt/b.txt"));
    assert_eq!(first, second);
    settle(&runtime, visual);
    root.read_with(visual, |root, _| {
        let snapshot = root
            .workspace_controller
            .as_ref()
            .expect("controller")
            .snapshot();
        assert_eq!(snapshot.terminal_tabs.len(), 1);
        assert!(snapshot.terminal_panes.is_empty());
        let state = owner_state(root, &first);
        assert_eq!(state.items.len(), 3);
        assert_eq!(state.active_path(), Some("/opt/b.txt"));
        assert!(
            matches!(&state.items[0], FileWorkspaceItem::Directory { path, .. } if path == "/srv")
        );
    });
}

/// 未装配或恢复失败的文档也参与去重，连点不能追加别名文档或第二个主机页。
#[gpui_kit::test]
fn repeated_pending_or_failed_file_retains_internal_identity(app: &mut TestAppContext) {
    let (_runtime, root, visual) = fixture(app);
    let endpoint = remote("host-a", "/srv/a.txt");
    for failed in [false, true] {
        visual.update(|_, app| {
            root.update(app, |root, cx| {
                let (descriptor, transient) =
                    crate::root::workspace_file_navigation::file_navigation_descriptor(
                        &endpoint, None,
                    )
                    .expect("descriptor");
                let owner = root
                    .open_host_document_descriptor(
                        descriptor,
                        endpoint.clone(),
                        None,
                        transient,
                        cx,
                    )
                    .expect("first descriptor");
                let before = owner_state(root, &owner);
                if failed {
                    root.workspace_files.set_result(
                        &owner,
                        Err((
                            WorkspaceRestoreFailure::ResourceFailure,
                            "read failed".into(),
                        )),
                    );
                }
                let (descriptor, transient) =
                    crate::root::workspace_file_navigation::file_navigation_descriptor(
                        &endpoint, None,
                    )
                    .expect("second descriptor");
                let repeated = root
                    .open_host_document_descriptor(
                        descriptor,
                        endpoint.clone(),
                        None,
                        transient,
                        cx,
                    )
                    .expect("repeat descriptor");
                assert_eq!(repeated, owner);
                assert_eq!(owner_state(root, &owner), before);
                assert_eq!(
                    root.workspace_controller
                        .as_ref()
                        .expect("controller")
                        .snapshot()
                        .terminal_tabs
                        .len(),
                    1
                );
                assert_eq!(
                    root.workspace_files.tab_for_endpoint(&endpoint),
                    Some(owner)
                );
            })
        });
    }
}

/// 本机、不同 HostId、容器 ID 与 runtime 都是文件系统身份边界，同名路径不能串宿主。
#[gpui_kit::test]
fn local_hosts_and_container_filesystems_remain_isolated(app: &mut TestAppContext) {
    let (_runtime, root, visual) = fixture(app);
    let mut endpoints = vec![
        FileEndpoint::Local {
            path: std::env::temp_dir().join("kerminal-memory-file.txt"),
        },
        remote("host-a", "/srv/a.txt"),
        remote("host-b", "/srv/a.txt"),
    ];
    for (runtime, container_id) in [
        (ContainerRuntime::Docker, "container-a"),
        (ContainerRuntime::Docker, "container-b"),
        (ContainerRuntime::Podman, "container-a"),
    ] {
        endpoints.push(FileEndpoint::Container {
            host_id: Some(HostId::parse("host-a").expect("host")),
            runtime,
            container_id: container_id.into(),
            path: RemotePath::parse_container("/srv/a.txt").expect("container path"),
        });
    }
    endpoints.push(FileEndpoint::Container {
        host_id: None,
        runtime: ContainerRuntime::Docker,
        container_id: "local-container".into(),
        path: RemotePath::parse_container("/srv/a.txt").expect("local container path"),
    });
    let ids = endpoints
        .iter()
        .cloned()
        .map(|endpoint| open(&root, visual, endpoint))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), endpoints.len());
    root.read_with(visual, |root, _| {
        assert_eq!(
            root.workspace_controller
                .as_ref()
                .expect("controller")
                .snapshot()
                .terminal_tabs
                .len(),
            endpoints.len()
        );
        for endpoint in endpoints {
            assert!(
                ids.contains(
                    &root
                        .workspace_files
                        .host_workspace_for_endpoint(&endpoint)
                        .expect("exact filesystem owner")
                )
            );
        }
        let target = root
            .context_workspace_projection()
            .active_target
            .expect("active local container");
        assert_eq!(
            target.kind,
            crate::features::context::ContextConnectionKind::Container
        );
        assert_eq!(target.container_id.as_deref(), Some("local-container"));
        assert!(target.host_id.is_none());
        let active = root
            .workspace_controller
            .as_ref()
            .unwrap()
            .snapshot()
            .active_tab_id
            .as_ref()
            .unwrap();
        assert!(
            root.workspace_files.transient_tab_ids().contains(active),
            "本机容器不能按本机文件恢复"
        );
    });
}

/// 内部切换不关宿主，干净文档收到真实关闭投影后回到保留目录。
#[gpui_kit::test]
fn closing_last_document_returns_to_directory_without_closing_owner(app: &mut TestAppContext) {
    let (runtime, root, visual) = fixture(app);
    let owner = open(&root, visual, remote("host-a", "/srv/a.txt"));
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let state = owner_state(root, &owner);
            let directory = state
                .items
                .iter()
                .find(|item| matches!(item, FileWorkspaceItem::Directory { .. }))
                .expect("directory")
                .id()
                .clone();
            let document = state
                .items
                .iter()
                .find(|item| matches!(item, FileWorkspaceItem::Document { .. }))
                .expect("document")
                .id()
                .clone();
            root.activate_host_file_item(&owner, &directory, window, cx);
            assert_eq!(owner_state(root, &owner).active_item_id, Some(directory));
            assert!(root.workspace_files.host_active_document(&owner).is_none());
            root.activate_host_file_item(&owner, &document, window, cx);
            assert!(root.workspace_files.host_active_document(&owner).is_some());
            root.close_host_file_item(&owner, &document, window, cx);
        })
    });
    settle(&runtime, visual);
    root.read_with(visual, |root, _| {
        let state = owner_state(root, &owner);
        assert_eq!(state.items.len(), 1);
        assert!(matches!(
            state.active_item(),
            Some(FileWorkspaceItem::Directory { .. })
        ));
        assert_eq!(
            root.workspace_controller
                .as_ref()
                .expect("controller")
                .snapshot()
                .active_tab_id
                .as_ref(),
            Some(&owner)
        );
        assert!(root.workspace_files.host_active_document(&owner).is_none());
        assert_eq!(root.center_panel, CenterPanel::Editor);
    });
}

/// 隐藏的内部文档仍让宿主显示 dirty，切到目录不能把未保存正文藏成干净状态。
#[gpui_kit::test]
fn inactive_dirty_document_marks_the_owner_title(app: &mut TestAppContext) {
    let (runtime, root, visual) = fixture(app);
    let owner = open(&root, visual, remote("host-a", "/srv/a.txt"));
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let editor = root.file_editor_workspace.clone().expect("editor");
            assert!(editor.update(cx, |workspace, cx| {
                workspace.focus_active_editor(window, cx)
            }));
        });
        window.draw(app).clear(app);
    });
    visual.simulate_keystrokes("x");
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            assert!(
                root.file_editor_workspace
                    .as_ref()
                    .expect("editor")
                    .read(cx)
                    .has_unsaved_changes()
            );
            let directory = owner_state(root, &owner)
                .items
                .iter()
                .find(|item| matches!(item, FileWorkspaceItem::Directory { .. }))
                .expect("directory")
                .id()
                .clone();
            root.activate_host_file_item(&owner, &directory, window, cx);
            let editor_projection = root
                .file_editor_workspace
                .as_ref()
                .expect("editor")
                .read(cx)
                .workspace_projection();
            assert!(
                editor_projection.active_endpoint.is_none(),
                "目录页面不能让保存动作命中隐藏文档"
            );
            assert!(
                editor_projection.tabs.iter().any(|tab| tab.dirty),
                "撤销活动引用不能丢弃脏缓冲"
            );
            let controller = root.workspace_controller.as_ref().expect("controller");
            let persisted_title = controller.snapshot().terminal_tabs[0].title().to_owned();
            let mut tabs = controller.tab_projections();
            root.project_file_dirty_titles(&mut tabs, cx);
            assert_eq!(tabs.len(), 1);
            assert_eq!(tabs[0].title.as_ref(), format!("{persisted_title} •"));
            assert_eq!(
                root.workspace_controller
                    .as_ref()
                    .expect("controller")
                    .snapshot()
                    .terminal_tabs[0]
                    .title(),
                persisted_title
            );
        })
    });
}

/// 关闭后旧恢复结果、编辑器投影和内部导航请求都不得复活已移除的 owner。
#[gpui_kit::test]
fn late_results_after_owner_removal_cannot_recreate_navigation(app: &mut TestAppContext) {
    let (runtime, root, visual) = fixture(app);
    let endpoint = remote("host-a", "/srv/a.txt");
    let owner = open(&root, visual, endpoint.clone());
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let projection = root
                .file_editor_workspace
                .as_ref()
                .expect("editor")
                .read(cx)
                .workspace_projection();
            let snapshot = WorkspaceSessionSnapshot {
                pinned_containers: Vec::new(),
                active_tab_id: None,
                focused_pane_id: None,
                selected_machine_id: None,
                terminal_tabs: Vec::new(),
                terminal_panes: Vec::new(),
                tab_groups: Default::default(),
            };
            root.workspace_controller
                .as_mut()
                .expect("controller")
                .synchronize_snapshot(snapshot)
                .expect("remove committed owner");
            root.remove_workspace_file_tab_after_close(&owner);
            root.workspace_files
                .set_result(&owner, Ok(Some(endpoint.clone())));
            root.sync_host_editor_projection(&projection, window, cx);
            assert!(root.update_host_file_state(&owner, |_| Ok(())).is_err());
            assert!(
                root.workspace_controller
                    .as_ref()
                    .expect("controller")
                    .snapshot()
                    .terminal_tabs
                    .is_empty()
            );
            assert!(
                root.workspace_files
                    .host_workspace_for_endpoint(&endpoint)
                    .is_none()
            );
            assert!(
                root.workspace_files
                    .host_document_endpoints(&owner)
                    .is_empty()
            );
        })
    });
}

/// 宿主目录页必须是单栏文件视图，内部标签带始终保留新建与定位入口；不能退回双端 SFTP 工作台。
#[gpui_kit::test]
fn host_directory_renders_single_pane_with_host_tab_actions(app: &mut TestAppContext) {
    let (runtime, root, visual) = fixture(app);
    let owner = open(&root, visual, remote("layout-host", "/etc/first.conf"));
    settle(&runtime, visual);
    visual.simulate_resize(gpui_kit::size(gpui_kit::px(1200.), gpui_kit::px(800.)));
    for _ in 0..2 {
        visual.update(|window, app| window.draw(app).clear(app));
    }
    let state = root.read_with(visual, |root, _| owner_state(root, &owner));
    let directory = state
        .items
        .iter()
        .find_map(|item| match item {
            FileWorkspaceItem::Directory { id, .. } => Some(id.clone()),
            FileWorkspaceItem::Document { .. } => None,
        })
        .expect("parent directory item");
    let selector: &'static str =
        Box::leak(format!("host-file-item-{}", directory.as_str()).into_boxed_str());
    let bounds = visual.debug_bounds(selector).expect("directory tab button");
    visual.simulate_click(bounds.center(), Default::default());
    settle(&runtime, visual);
    for _ in 0..2 {
        visual.update(|window, app| window.draw(app).clear(app));
    }
    assert!(
        visual.debug_bounds("host-file-tab-strip").is_some(),
        "host tab strip must stay visible"
    );
    assert!(visual.debug_bounds("host-file-new-directory").is_some());
    assert!(visual.debug_bounds("host-file-locate-file").is_some());
    let active = root.read_with(visual, |root, _| owner_state(root, &owner).active_item_id);
    assert_eq!(active.as_ref(), Some(&directory));
}

/// 顶层活动标签是文件工作区时，残留的文件面板选择不能把中央切回双端 SFTP 工作台。
#[gpui_kit::test]
fn stale_files_panel_selection_cannot_replace_the_active_file_workspace(app: &mut TestAppContext) {
    let (runtime, root, visual) = fixture(app);
    let _owner = open(&root, visual, remote("stale-panel-host", "/etc/first.conf"));
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.center_panel = CenterPanel::Files;
            cx.notify();
        });
        window.draw(app).clear(app);
    });
    for _ in 0..2 {
        visual.update(|window, app| window.draw(app).clear(app));
    }
    assert!(
        visual.debug_bounds("host-file-tab-strip").is_some(),
        "the active file workspace must keep its own view"
    );
    assert!(
        visual.debug_bounds("rr-files-browser-1").is_none(),
        "the shared SFTP workbench must not replace the active file workspace"
    );
}
