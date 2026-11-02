//! 跨入口复用文件时验证工作区、运行态和真实编辑器的只读事务。
//!
//! @author kongweiguang

use std::{cell::RefCell, collections::BTreeMap, rc::Rc, sync::Arc};

use async_trait::async_trait;
use gpui_kit::{TestAppContext, VisualTestContext};
use kerminal_application::{
    ApplicationTaskExecutor,
    files::{FileAccessProvider, FileBookmarkRepository, FileService, editor::FileEditorService},
    workspace::{WorkspaceFileSource, WorkspaceSessionDocument, WorkspaceSessionRevision},
};
use kerminal_domain::files::{
    BookmarkTarget, DeleteTargetKind, DirectoryListing, FileBookmark, FilePreview, FileRevision,
    LineEnding, TextDocument, WriteTextRequest, WriteTextResult,
};
use tokio::runtime::{Builder, Runtime};

use super::*;
use crate::features::{files::FileTargetNameLookup, workspace_tabs::WorkspaceTabEvent};

/// 真实编辑器只使用隔离的内存文件端口，保存保持在途以验证权限不能越过写入回执。
fn editor_fixture(
    app: &mut TestAppContext,
    filesystem_readonly: bool,
) -> (Runtime, Entity<FileEditorWorkspace>, &mut VisualTestContext) {
    app.update(gpui_kit::init);
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let files = FileService::new(
        vec![Arc::new(MemoryFile {
            readonly: filesystem_readonly,
        })],
        Arc::new(NoBookmarks),
    )
    .unwrap();
    let service = FileEditorService::new(files);
    let mounted = Rc::new(RefCell::new(None));
    let mounted_out = mounted.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let editor = cx.new(|cx| {
            FileEditorWorkspace::new(
                service,
                executor,
                FileTargetNameLookup::default(),
                window,
                cx,
            )
        });
        *mounted_out.borrow_mut() = Some(editor.clone());
        gpui_kit::base::Root::new(editor, window, cx)
    });
    let editor = mounted.borrow_mut().take().expect("editor mounted");
    (runtime, editor, visual)
}

/// 固定 SSH 主机和绝对路径，使测试覆盖 SFTP 与 Compose 的真实身份重合。
fn fixture_endpoint() -> FileEndpoint {
    FileEndpoint::Remote {
        host_id: HostId::parse("host-a").unwrap(),
        path: RemotePath::parse("/srv/compose.yaml").unwrap(),
    }
}

/// 描述来源可以不同，但只读策略与主机路径必须保留在同一个稳定 TabId 上。
fn fixture_workspace(
    source: WorkspaceFileSource,
    access: WorkspaceFileAccess,
) -> (WorkspaceController, WorkspaceFileRuntime, TabId) {
    let tab_id = TabId::new();
    let host_id = HostId::parse("host-a").unwrap();
    let tab = WorkspaceSessionTab::WorkspaceFile {
        id: tab_id.clone(),
        tab_group_id: None,
        title: "compose.yaml".to_owned(),
        machine_id: host_id.as_str().to_owned(),
        target: WorkspaceTargetRef::Ssh { host_id },
        path: "/srv/compose.yaml".to_owned(),
        root_path: None,
        access,
        source,
    };
    let snapshot = WorkspaceSessionSnapshot {
        pinned_containers: Vec::new(),
        active_tab_id: Some(tab_id.clone()),
        focused_pane_id: None,
        selected_machine_id: Some("host-a".to_owned()),
        terminal_tabs: vec![tab],
        terminal_panes: Vec::new(),
        tab_groups: BTreeMap::new(),
    };
    let mut document = WorkspaceSessionDocument::empty_v3();
    document.update_known_fields(&snapshot).unwrap();
    let controller =
        WorkspaceController::from_loaded(document, WorkspaceSessionRevision::new("fixture"))
            .unwrap();
    let files = WorkspaceFileRuntime::from_snapshot(&snapshot);
    (controller, files, tab_id)
}

/// 交替推进 GPUI 与独立 Tokio runtime，等待真实文件编辑器消费内存读取回执。
fn open_editor(
    runtime: &Runtime,
    editor: &Entity<FileEditorWorkspace>,
    visual: &mut VisualTestContext,
    endpoint: &FileEndpoint,
    readonly: bool,
) {
    editor.update(visual, |workspace, cx| {
        workspace
            .open_workspace_file(
                endpoint.clone(),
                "compose.yaml".to_owned(),
                readonly,
                true,
                cx,
            )
            .unwrap();
    });
    for _ in 0..32 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
        visual.run_until_parked();
        if editor.read_with(visual, |workspace, _| {
            workspace
                .workspace_projection()
                .tabs
                .iter()
                .any(|tab| tab.endpoint == *endpoint && !tab.loading)
        }) {
            return;
        }
    }
    panic!("editor did not consume the in-memory read result");
}

/// 通过真实编辑组件输入产生脏稿，避免测试只伪造一个 dirty 标志。
fn type_draft(editor: &Entity<FileEditorWorkspace>, visual: &mut VisualTestContext) {
    visual.update(|window, app| {
        assert!(editor.update(app, |workspace, cx| {
            workspace.focus_active_editor(window, cx)
        }));
        window.draw(app).clear(app);
    });
    visual.simulate_keystrokes("x");
    visual.run_until_parked();
    assert!(editor.read_with(visual, |workspace, _| workspace.has_unsaved_changes()));
}

/// 普通文件先打开时收紧同一 Tab，之后普通入口再激活也不能放宽只读。
#[gpui_kit::test]
fn editable_sftp_then_compose_reuses_one_readonly_tab(app: &mut TestAppContext) {
    let (runtime, editor, visual) = editor_fixture(app, false);
    let endpoint = fixture_endpoint();
    let (mut controller, mut files, id) =
        fixture_workspace(WorkspaceFileSource::Sftp, WorkspaceFileAccess::Editable);
    open_editor(&runtime, &editor, visual, &endpoint, false);
    files.set_result(&id, Ok(Some(endpoint.clone())));
    visual.update(|_, cx| {
        assert_eq!(files.tab_for_endpoint(&endpoint), Some(id.clone()));
        assert!(
            files
                .restrict_file_to_readonly(&mut controller, &editor, &id, &endpoint, cx)
                .unwrap()
        );
        assert_eq!(controller.snapshot().terminal_tabs.len(), 1);
        assert_eq!(files.tab_for_endpoint(&endpoint), Some(id.clone()));
        assert!(files.workspace_file_requires_readonly(&endpoint));
        assert!(matches!(
            &controller.snapshot().terminal_tabs[0],
            WorkspaceSessionTab::WorkspaceFile {
                id: actual,
                source: WorkspaceFileSource::Sftp,
                access: WorkspaceFileAccess::Readonly,
                ..
            } if actual == &id
        ));
        assert_eq!(files.tabs[&id].tab, controller.snapshot().terminal_tabs[0]);
        assert_eq!(
            editor
                .read(cx)
                .workspace_file_readonly_before_restriction(&endpoint, cx)
                .unwrap(),
            Some(true)
        );
        editor.update(cx, |workspace, cx| {
            workspace
                .open_workspace_file(
                    endpoint.clone(),
                    "compose.yaml".to_owned(),
                    files.workspace_file_requires_readonly(&endpoint),
                    true,
                    cx,
                )
                .unwrap();
        });
        assert!(editor.read(cx).workspace_projection().tabs[0].readonly);
    });
}

/// Compose 先打开时普通文件入口复用原 Tab，文件系统权限与工作区只读策略仍共同生效。
#[gpui_kit::test]
fn readonly_compose_then_sftp_reuses_its_original_tab(app: &mut TestAppContext) {
    let (runtime, editor, visual) = editor_fixture(app, false);
    let endpoint = fixture_endpoint();
    let (mut controller, mut files, id) = fixture_workspace(
        WorkspaceFileSource::ComposeYaml,
        WorkspaceFileAccess::Readonly,
    );
    open_editor(&runtime, &editor, visual, &endpoint, true);
    files.set_result(&id, Ok(Some(endpoint.clone())));
    visual.update(|_, cx| {
        assert_eq!(files.tab_for_endpoint(&endpoint), Some(id.clone()));
        assert!(
            !files
                .restrict_file_to_readonly(&mut controller, &editor, &id, &endpoint, cx)
                .unwrap()
        );
        editor.update(cx, |workspace, cx| {
            workspace
                .open_workspace_file(
                    endpoint.clone(),
                    "compose.yaml".to_owned(),
                    files.workspace_file_requires_readonly(&endpoint),
                    true,
                    cx,
                )
                .unwrap();
        });
        assert_eq!(controller.snapshot().terminal_tabs.len(), 1);
        assert_eq!(files.tab_for_endpoint(&endpoint), Some(id));
        assert!(editor.read(cx).workspace_projection().tabs[0].readonly);
    });
}

/// 脏正文及写入中的文件均拒绝收紧，原标签、工作区和编辑权限保持可恢复。
#[gpui_kit::test]
fn dirty_and_saving_file_reject_readonly_restriction(app: &mut TestAppContext) {
    let (runtime, editor, visual) = editor_fixture(app, false);
    let endpoint = fixture_endpoint();
    let (mut controller, mut files, id) =
        fixture_workspace(WorkspaceFileSource::Sftp, WorkspaceFileAccess::Editable);
    open_editor(&runtime, &editor, visual, &endpoint, false);
    files.set_result(&id, Ok(Some(endpoint.clone())));
    type_draft(&editor, visual);
    let original = controller.snapshot().clone();
    let projection = editor.read_with(visual, |workspace, _| workspace.workspace_projection());
    visual.update(|_, cx| {
        let error = files
            .restrict_file_to_readonly(&mut controller, &editor, &id, &endpoint, cx)
            .unwrap_err();
        assert!(error.public_message().contains("未保存"));
        assert_eq!(controller.snapshot(), &original);
        assert_eq!(files.tabs[&id].tab, original.terminal_tabs[0]);
        assert_eq!(editor.read(cx).workspace_projection(), projection);
        assert!(!files.workspace_file_requires_readonly(&endpoint));
    });

    visual.simulate_keystrokes("ctrl-s");
    visual.run_until_parked();
    visual.update(|_, cx| {
        let error = files
            .restrict_file_to_readonly(&mut controller, &editor, &id, &endpoint, cx)
            .unwrap_err();
        assert!(error.public_message().contains("正在保存"));
        assert_eq!(controller.snapshot(), &original);
        assert!(!files.workspace_file_requires_readonly(&endpoint));
        assert!(editor.read(cx).workspace_projection().tabs[0].dirty);
    });
}

/// controller 占有资源门禁时不得修改编辑器或运行态，解除门禁后同一标签可重新收紧。
#[gpui_kit::test]
fn blocked_controller_keeps_the_editor_policy_and_allows_retry(app: &mut TestAppContext) {
    let (runtime, editor, visual) = editor_fixture(app, false);
    let endpoint = fixture_endpoint();
    let (mut controller, mut files, id) =
        fixture_workspace(WorkspaceFileSource::Sftp, WorkspaceFileAccess::Editable);
    open_editor(&runtime, &editor, visual, &endpoint, false);
    files.set_result(&id, Ok(Some(endpoint.clone())));
    let original = controller.snapshot().clone();
    controller
        .apply_tab_event(WorkspaceTabEvent::Close(id.clone()))
        .unwrap();
    visual.update(|_, cx| {
        assert!(
            files
                .restrict_file_to_readonly(&mut controller, &editor, &id, &endpoint, cx)
                .is_err()
        );
        assert_eq!(controller.snapshot(), &original);
        assert_eq!(files.tabs[&id].tab, original.terminal_tabs[0]);
        assert_eq!(
            editor
                .read(cx)
                .workspace_file_readonly_before_restriction(&endpoint, cx)
                .unwrap(),
            Some(false)
        );
        assert!(!editor.read(cx).workspace_projection().tabs[0].readonly);
        assert!(controller.discard_pending_resource_operation());
        assert!(
            files
                .restrict_file_to_readonly(&mut controller, &editor, &id, &endpoint, cx)
                .unwrap()
        );
        assert!(editor.read(cx).workspace_projection().tabs[0].readonly);
    });
}

/// 文件系统只读不会被误记为工作区策略，controller 拒绝时两种权限都保持原值。
#[gpui_kit::test]
fn filesystem_readonly_remains_distinct_from_workspace_policy(app: &mut TestAppContext) {
    let (runtime, editor, visual) = editor_fixture(app, true);
    let endpoint = fixture_endpoint();
    let (mut controller, mut files, id) =
        fixture_workspace(WorkspaceFileSource::Sftp, WorkspaceFileAccess::Editable);
    open_editor(&runtime, &editor, visual, &endpoint, false);
    files.set_result(&id, Ok(Some(endpoint.clone())));
    controller
        .apply_tab_event(WorkspaceTabEvent::Close(id.clone()))
        .unwrap();
    visual.update(|_, cx| {
        assert!(editor.read(cx).workspace_projection().tabs[0].readonly);
        assert_eq!(
            editor
                .read(cx)
                .workspace_file_readonly_before_restriction(&endpoint, cx)
                .unwrap(),
            Some(false)
        );
        assert!(
            files
                .restrict_file_to_readonly(&mut controller, &editor, &id, &endpoint, cx)
                .is_err()
        );
        assert!(editor.read(cx).workspace_projection().tabs[0].readonly);
        assert_eq!(
            editor
                .read(cx)
                .workspace_file_readonly_before_restriction(&endpoint, cx)
                .unwrap(),
            Some(false)
        );
        assert!(!files.workspace_file_requires_readonly(&endpoint));
    });
}

/// 未装配或已失败的持久描述也先复用并收紧，真正恢复时再创建同一编辑缓冲。
#[gpui_kit::test]
fn pending_and_failed_descriptors_keep_identity_during_restriction(app: &mut TestAppContext) {
    let (_runtime, editor, visual) = editor_fixture(app, false);
    let endpoint = fixture_endpoint();
    for failed in [false, true] {
        let (mut controller, mut files, id) =
            fixture_workspace(WorkspaceFileSource::Sftp, WorkspaceFileAccess::Editable);
        if failed {
            files.set_result(
                &id,
                Err((
                    WorkspaceRestoreFailure::ResourceFailure,
                    "fixture failure".into(),
                )),
            );
        }
        visual.update(|_, cx| {
            assert_eq!(files.tab_for_endpoint(&endpoint), Some(id.clone()));
            assert!(
                files
                    .restrict_file_to_readonly(&mut controller, &editor, &id, &endpoint, cx)
                    .unwrap()
            );
            assert_eq!(controller.snapshot().terminal_tabs.len(), 1);
            assert_eq!(files.tab_for_endpoint(&endpoint), Some(id.clone()));
            assert!(files.workspace_file_requires_readonly(&endpoint));
            assert_eq!(files.tabs[&id].tab, controller.snapshot().terminal_tabs[0]);
            assert!(editor.read(cx).workspace_projection().tabs.is_empty());
            assert_eq!(
                matches!(
                    files.restore_state(&id),
                    Some(WorkspaceFileRestoreState::Failed { .. })
                ),
                failed
            );
        });
    }
}

/// 内存文件只允许读取；保存永远等待以稳定模拟尚未收到写入回执的状态。
struct MemoryFile {
    readonly: bool,
}

#[async_trait]
impl FileAccessProvider for MemoryFile {
    /// 测试端点由调用方固定，不查询真实文件系统。
    fn supports(&self, _: &FileEndpoint) -> bool {
        true
    }

    /// 未使用目录操作明确失败。
    async fn list_directory(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
    ) -> AppResult<DirectoryListing> {
        Err(AppError::unsupported("fixture"))
    }

    /// 未使用预览操作明确失败。
    async fn preview_file(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: usize,
    ) -> AppResult<FilePreview> {
        Err(AppError::unsupported("fixture"))
    }

    /// 同一正文与 revision 支持验证真实脏稿，不触碰磁盘或网络。
    async fn read_text_file(
        &self,
        _: CancellationToken,
        endpoint: FileEndpoint,
        _: usize,
    ) -> AppResult<TextDocument> {
        Ok(TextDocument {
            endpoint,
            content: "services: {}\n".to_owned(),
            line_ending: LineEnding::Lf,
            revision: FileRevision {
                size: 13,
                modified: None,
                permissions: None,
                content_digest: None,
            },
            readonly: self.readonly,
            truncated: false,
        })
    }

    /// 不返回伪造写入结果，直到测试结束才由 runtime 取消。
    async fn write_text_file(
        &self,
        _: CancellationToken,
        _: WriteTextRequest,
    ) -> AppResult<WriteTextResult> {
        std::future::pending().await
    }

    /// 测试不使用目录写入。
    async fn create_directory(&self, _: CancellationToken, _: FileEndpoint) -> AppResult<()> {
        Err(AppError::unsupported("fixture"))
    }
    /// 测试不使用重命名。
    async fn rename(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: FileEndpoint,
    ) -> AppResult<()> {
        Err(AppError::unsupported("fixture"))
    }
    /// 测试不使用权限写入。
    async fn chmod(&self, _: CancellationToken, _: FileEndpoint, _: u32) -> AppResult<()> {
        Err(AppError::unsupported("fixture"))
    }
    /// 测试不使用删除。
    async fn delete(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: DeleteTargetKind,
    ) -> AppResult<()> {
        Err(AppError::unsupported("fixture"))
    }
}

/// 书签与此事务无关，适配器不读取或写入用户资料。
struct NoBookmarks;

#[async_trait]
impl FileBookmarkRepository for NoBookmarks {
    /// 不读取用户书签。
    async fn list(&self, _: CancellationToken, _: BookmarkTarget) -> AppResult<Vec<FileBookmark>> {
        Ok(Vec::new())
    }
    /// 禁止 fixture 写入书签。
    async fn set(
        &self,
        _: CancellationToken,
        _: FileBookmark,
        _: bool,
    ) -> AppResult<Vec<FileBookmark>> {
        Err(AppError::unsupported("fixture"))
    }
}
