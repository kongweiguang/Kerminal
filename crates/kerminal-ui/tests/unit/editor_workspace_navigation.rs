//! 文件工作区后台恢复与显式激活的真实 GPUI 投影回归。
//!
//! @author kongweiguang

use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc};

use async_trait::async_trait;
use gpui_kit::{
    AppContext as _, Context, Entity, IntoElement as _, Render, Subscription, TestAppContext,
    VisualTestContext, Window,
};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor,
    files::{FileAccessProvider, FileBookmarkRepository, FileService, editor::FileEditorService},
};
use kerminal_domain::files::{
    BookmarkTarget, DeleteTargetKind, DirectoryListing, FileBookmark, FileEndpoint, FilePreview,
    TextDocument, WriteTextRequest, WriteTextResult,
};
use tokio::runtime::{Builder, Runtime};
use tokio_util::sync::CancellationToken;

use super::{
    EditorConfirmation, EditorWorkspaceProjection, FileEditorWorkspace, FileEditorWorkspaceEvent,
};
use crate::features::files::FileTargetNameLookup;

/// 监听真实 Entity 事件并读取当时投影，避免只断言内部字段而漏掉 Root 接收的变化。
struct ProjectionObserver {
    snapshots: Vec<EditorWorkspaceProjection>,
    _subscription: Subscription,
}

impl ProjectionObserver {
    /// 订阅与观察者同生共灭，测试每一步都先排空 GPUI 事件再检查快照。
    ///
    /// @author kongweiguang
    fn new(editor: &Entity<FileEditorWorkspace>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(editor, |observer, editor, event, cx| {
            if matches!(event, FileEditorWorkspaceEvent::WorkspaceProjectionChanged) {
                observer
                    .snapshots
                    .push(editor.read(cx).workspace_projection());
            }
        });
        Self {
            snapshots: Vec::new(),
            _subscription: subscription,
        }
    }
}

/// 固定两个互不相同的端点，测试不依赖路径实际存在。
///
/// @author kongweiguang
fn endpoint(name: &str) -> FileEndpoint {
    FileEndpoint::Local {
        path: PathBuf::from(format!("C:/navigation-fixture/{name}")),
    }
}

/// 挂载真实编辑工作区及其投影订阅；独立 Tokio runtime 保持读取任务待处理。
///
/// @author kongweiguang
fn fixture(
    app: &mut TestAppContext,
) -> (
    Runtime,
    Entity<FileEditorWorkspace>,
    Entity<ProjectionObserver>,
    &mut VisualTestContext,
) {
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let files = FileService::new(vec![Arc::new(NoIoProvider)], Arc::new(NoBookmarks))
        .expect("fixture file ports are registered");
    let service = FileEditorService::new(files);
    app.update(gpui_kit::init);
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
        let observer = cx.new(|cx| ProjectionObserver::new(&editor, cx));
        *mounted_out.borrow_mut() = Some((editor.clone(), observer));
        gpui_kit::base::Root::new(editor, window, cx)
    });
    let (editor, observer) = mounted.borrow_mut().take().expect("editor fixture mounted");
    (runtime, editor, observer, visual)
}

/// 根层只提供挂载位置，确认按钮和状态仍来自生产编辑器实体。
struct ConfirmationHost {
    editor: Entity<FileEditorWorkspace>,
}

impl Render for ConfirmationHost {
    /// 按当前窗口主题渲染生产确认内容，避免测试重建按钮或动作。
    /// @author kongweiguang
    fn render(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl gpui_kit::IntoElement {
        let theme = crate::theme::ThemeTokens::current(window, cx);
        self.editor
            .update(cx, |editor, cx| editor.render_root_confirmation(theme, cx))
            .unwrap_or_else(|| gpui_kit::div().into_any_element())
    }
}

/// 独立窗口只装配真实确认视图，文件端口保持只读且不推进后台 I/O。
/// @author kongweiguang
fn confirmation_fixture(
    app: &mut TestAppContext,
    confirmation: EditorConfirmation,
) -> (Runtime, Entity<FileEditorWorkspace>, &mut VisualTestContext) {
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let files = FileService::new(vec![Arc::new(NoIoProvider)], Arc::new(NoBookmarks))
        .expect("fixture file ports are registered");
    let service = FileEditorService::new(files);
    app.update(gpui_kit::init);
    let mounted = Rc::new(RefCell::new(None));
    let mounted_out = mounted.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let editor = cx.new(|cx| {
            let mut editor = FileEditorWorkspace::new(
                service,
                executor,
                FileTargetNameLookup::default(),
                window,
                cx,
            );
            editor.confirmation = Some(confirmation);
            editor
        });
        *mounted_out.borrow_mut() = Some(editor.clone());
        let host = cx.new(|_| ConfirmationHost { editor });
        gpui_kit::base::Root::new(host, window, cx)
    });
    let editor = mounted
        .borrow_mut()
        .take()
        .expect("confirmation fixture mounted");
    (runtime, editor, visual)
}

/// 关闭和退出确认按取消、丢弃、保存排列；点击取消只撤销待处理确认。
/// @author kongweiguang
#[gpui_kit::test]
fn unsaved_file_confirmation_orders_actions_and_cancel_preserves_the_request_boundary(
    app: &mut TestAppContext,
) {
    for quit in [false, true] {
        let confirmation = if quit {
            EditorConfirmation::Quit(endpoint("draft.txt"))
        } else {
            EditorConfirmation::Close(endpoint("draft.txt"))
        };
        let (_runtime, editor, visual) = confirmation_fixture(app, confirmation);
        visual.simulate_resize(gpui_kit::size(gpui_kit::px(1000.), gpui_kit::px(700.)));
        visual.run_until_parked();
        visual.update(|window, app| window.draw(app).clear(app));
        visual.update(|window, app| window.draw(app).clear(app));
        let cancel = visual
            .debug_bounds("file-editor-confirm-cancel")
            .expect("取消按钮应可测量");
        let discard = visual
            .debug_bounds(if quit {
                "file-editor-discard-continue-quit"
            } else {
                "file-editor-discard-close"
            })
            .expect("丢弃按钮应可测量");
        let save = visual
            .debug_bounds(if quit {
                "file-editor-save-continue-quit"
            } else {
                "file-editor-save-close"
            })
            .expect("保存按钮应可测量");
        assert!(cancel.right() <= discard.origin.x);
        assert!(discard.right() <= save.origin.x);
        visual.simulate_click(cancel.center(), Default::default());
        visual.run_until_parked();
        visual.update(|_, app| assert!(editor.read(app).confirmation.is_none()));
    }
}

/// 派发恢复意图并读取所有相应事件，不推进 Tokio 文件读取以覆盖 pending 状态。
///
/// @author kongweiguang
fn open(
    editor: &Entity<FileEditorWorkspace>,
    visual: &mut VisualTestContext,
    file: &FileEndpoint,
    activate: bool,
) {
    editor.update(visual, |workspace, cx| {
        workspace
            .open_workspace_file(file.clone(), "fixture".to_owned(), false, activate, cx)
            .expect("fixture editor tab is accepted");
    });
    visual.run_until_parked();
}

/// 后台装配 B 必须发布包含 B 的投影，但不能让 Root 把 B 误认成用户选择。
///
/// @author kongweiguang
#[gpui_kit::test]
fn background_file_open_preserves_active_projection_until_explicit_activation(
    app: &mut TestAppContext,
) {
    let (_runtime, editor, observer, visual) = fixture(app);
    let first = endpoint("a.txt");
    let second = endpoint("b.txt");
    open(&editor, visual, &first, true);
    let before = observer.read_with(visual, |observer, _| observer.snapshots.len());

    open(&editor, visual, &second, false);
    observer.read_with(visual, |observer, _| {
        let emitted = &observer.snapshots[before..];
        assert!(
            !emitted.is_empty(),
            "background tab must publish its projection"
        );
        assert!(
            emitted
                .iter()
                .all(|snapshot| snapshot.active_endpoint.as_ref() == Some(&first))
        );
        let latest = emitted.last().unwrap();
        assert_eq!(latest.tabs.len(), 2);
        assert!(latest.tabs.iter().all(|tab| tab.loading));
        assert!(
            latest
                .tabs
                .iter()
                .any(|tab| tab.endpoint == first && tab.active)
        );
        assert!(
            latest
                .tabs
                .iter()
                .any(|tab| tab.endpoint == second && !tab.active)
        );
    });

    let before_activate = observer.read_with(visual, |observer, _| observer.snapshots.len());
    open(&editor, visual, &second, true);
    observer.read_with(visual, |observer, _| {
        let emitted = &observer.snapshots[before_activate..];
        assert!(
            !emitted.is_empty(),
            "explicit activation must publish a projection"
        );
        assert_eq!(
            emitted.last().unwrap().active_endpoint.as_ref(),
            Some(&second)
        );
    });
}

/// 两个待恢复文件无论先装配哪一个，只有 descriptor 指定的活动文件能成为活动投影。
///
/// @author kongweiguang
#[gpui_kit::test]
fn pending_restore_order_does_not_select_the_background_file(app: &mut TestAppContext) {
    for background_first in [false, true] {
        let (_runtime, editor, observer, visual) = fixture(app);
        let first = endpoint("active.txt");
        let second = endpoint("background.txt");
        if background_first {
            open(&editor, visual, &second, false);
            open(&editor, visual, &first, true);
        } else {
            open(&editor, visual, &first, true);
            open(&editor, visual, &second, false);
        }
        observer.read_with(visual, |observer, _| {
            assert!(!observer.snapshots.is_empty());
            assert!(
                observer
                    .snapshots
                    .iter()
                    .all(|snapshot| { snapshot.active_endpoint.as_ref() != Some(&second) })
            );
            let latest = observer.snapshots.last().unwrap();
            assert_eq!(latest.active_endpoint.as_ref(), Some(&first));
            assert_eq!(latest.tabs.len(), 2);
            assert!(latest.tabs.iter().all(|tab| tab.loading));
        });
    }
}

/// 不访问磁盘或网络；本测试不推进 Tokio runtime，所有文件读取保持 pending。
struct NoIoProvider;

#[async_trait]
impl FileAccessProvider for NoIoProvider {
    /// 接受 fixture 端点，真正 IO 在测试中不应被执行。
    fn supports(&self, _: &FileEndpoint) -> bool {
        true
    }

    /// 未覆盖的目录操作明确拒绝。
    async fn list_directory(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
    ) -> AppResult<DirectoryListing> {
        Err(AppError::unsupported("navigation fixture"))
    }

    /// 未覆盖的预览操作明确拒绝。
    async fn preview_file(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: usize,
    ) -> AppResult<FilePreview> {
        Err(AppError::unsupported("navigation fixture"))
    }

    /// 即使 runtime 被推进，读取仍保持 pending，不会触碰实际文件。
    async fn read_text_file(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: usize,
    ) -> AppResult<TextDocument> {
        std::future::pending().await
    }

    /// 未覆盖的写操作明确拒绝。
    async fn write_text_file(
        &self,
        _: CancellationToken,
        _: WriteTextRequest,
    ) -> AppResult<WriteTextResult> {
        Err(AppError::unsupported("navigation fixture"))
    }

    /// 未覆盖的建目录操作明确拒绝。
    async fn create_directory(&self, _: CancellationToken, _: FileEndpoint) -> AppResult<()> {
        Err(AppError::unsupported("navigation fixture"))
    }

    /// 未覆盖的重命名操作明确拒绝。
    async fn rename(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: FileEndpoint,
    ) -> AppResult<()> {
        Err(AppError::unsupported("navigation fixture"))
    }

    /// 未覆盖的权限操作明确拒绝。
    async fn chmod(&self, _: CancellationToken, _: FileEndpoint, _: u32) -> AppResult<()> {
        Err(AppError::unsupported("navigation fixture"))
    }

    /// 未覆盖的删除操作明确拒绝。
    async fn delete(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: DeleteTargetKind,
    ) -> AppResult<()> {
        Err(AppError::unsupported("navigation fixture"))
    }
}

/// 本测试不使用书签，仓储只满足 FileService 构造约束。
struct NoBookmarks;

#[async_trait]
impl FileBookmarkRepository for NoBookmarks {
    /// 不读取用户书签。
    async fn list(&self, _: CancellationToken, _: BookmarkTarget) -> AppResult<Vec<FileBookmark>> {
        Ok(Vec::new())
    }

    /// 不允许 fixture 写入书签。
    async fn set(
        &self,
        _: CancellationToken,
        _: FileBookmark,
        _: bool,
    ) -> AppResult<Vec<FileBookmark>> {
        Err(AppError::unsupported("navigation fixture"))
    }
}
