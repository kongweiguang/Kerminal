//! 文件草稿关闭确认必须先于 Tab 插件工具的不可逆关闭。
//!
//! @author kongweiguang

use super::*;
use crate::features::workspace_tabs::WorkspaceTabEvent;
use gpui_kit::{AnyView, App, Context, IntoElement, Render, Window, div};
use kerminal_application::{
    files::{FileAccessProvider, FileBookmarkRepository, FileService, editor::FileEditorService},
    plugins::{PluginDescriptor, PluginId, PluginRegistryBuilder, ToolDescriptor, ToolId},
    preferences::ToolRailToolId,
};
use kerminal_domain::files::{
    BookmarkTarget, DeleteTargetKind, DirectoryListing, FileBookmark, FilePreview, FileRevision,
    LineEnding, TextDocument, WriteTextRequest, WriteTextResult,
};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

use crate::plugins::tools::{
    ToolCloseFuture, ToolContext, ToolFactory, ToolInstance, ToolPhase, ToolPresentation, ToolScope,
};

struct MemoryFile;

#[async_trait]
impl FileAccessProvider for MemoryFile {
    /// 仅接受当前回归的隔离端点，意外目标不能触及实际文件。
    fn supports(&self, endpoint: &FileEndpoint) -> bool {
        endpoint == &file_endpoint()
    }

    /// 本回归只打开文本，不用虚假的目录快照影响 Root 的关闭选择。
    async fn list_directory(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
    ) -> AppResult<DirectoryListing> {
        unavailable()
    }

    /// 预览不属于关闭场景，误调用时明确失败。
    async fn preview_file(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: usize,
    ) -> AppResult<FilePreview> {
        unavailable()
    }

    /// 返回固定可写文档，脏状态必须由真实编辑器输入和订阅产生。
    async fn read_text_file(
        &self,
        _: CancellationToken,
        endpoint: FileEndpoint,
        _: usize,
    ) -> AppResult<TextDocument> {
        Ok(TextDocument {
            endpoint,
            content: "original\n".to_owned(),
            line_ending: LineEnding::Lf,
            revision: FileRevision {
                size: 9,
                modified: None,
                permissions: None,
                content_digest: Some("fixture-original".to_owned()),
            },
            readonly: false,
            truncated: false,
        })
    }

    /// 本回归选择取消和丢弃，任何保存都表明用户决策被错误执行。
    async fn write_text_file(
        &self,
        _: CancellationToken,
        _: WriteTextRequest,
    ) -> AppResult<WriteTextResult> {
        unavailable()
    }

    /// 不为关闭回归实现额外的文件写入副作用。
    async fn create_directory(&self, _: CancellationToken, _: FileEndpoint) -> AppResult<()> {
        unavailable()
    }

    /// 不允许关闭流程通过重命名改变文件身份。
    async fn rename(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: FileEndpoint,
    ) -> AppResult<()> {
        unavailable()
    }

    /// 权限修改不属于该场景，误调用时保持显式失败。
    async fn chmod(&self, _: CancellationToken, _: FileEndpoint, _: u32) -> AppResult<()> {
        unavailable()
    }

    /// 丢弃编辑草稿不能被误实现为删除底层文件。
    async fn delete(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: DeleteTargetKind,
    ) -> AppResult<()> {
        unavailable()
    }
}

struct NoBookmarks;

#[async_trait]
impl FileBookmarkRepository for NoBookmarks {
    /// 内存 fixture 不读取用户收藏。
    async fn list(&self, _: CancellationToken, _: BookmarkTarget) -> AppResult<Vec<FileBookmark>> {
        Ok(Vec::new())
    }

    /// 收藏写入不属于关闭行为，误调用时明确失败。
    async fn set(
        &self,
        _: CancellationToken,
        _: FileBookmark,
        _: bool,
    ) -> AppResult<Vec<FileBookmark>> {
        unavailable()
    }
}

struct ToolView;

impl Render for ToolView {
    /// 工具实体只提供独立身份，关闭次序通过真实实例回调记录。
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

struct CloseProbe {
    view: Entity<ToolView>,
    closes: Arc<AtomicUsize>,
}

impl ToolInstance for CloseProbe {
    /// 返回原实体，取消不能借重建视图掩盖提前关闭。
    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    /// 同步记录关闭开始，异步成功仍须由宿主接收后才能提交删除。
    fn begin_close(&mut self, _: Instant, _: &mut App) -> AppResult<ToolCloseFuture> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(async { Ok(()) }))
    }
}

struct ProbeFactory(Arc<AtomicUsize>);

impl ToolFactory for ProbeFactory {
    /// 固定身份使目录注册和关闭回执指向同一贡献。
    fn tool_id(&self) -> &str {
        "fixture.file-close-tool"
    }

    /// 图标与显示名称不参与实例作用域判断。
    fn presentation(&self) -> ToolPresentation {
        ToolPresentation::new(
            self.tool_id(),
            "fixture",
            gpui_kit::assets::IconName::FileText,
        )
    }

    /// 必须使用标签作用域，才能检测取消后原 Tab owner 是否仍然可用。
    fn scope(&self) -> ToolScope {
        ToolScope::Tab
    }

    /// 工厂只建立视图和计数句柄，不在 fixture 中模拟宿主关闭状态机。
    fn create(
        &self,
        _: &ToolContext,
        _: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ToolInstance>> {
        Ok(Box::new(CloseProbe {
            view: cx.new(|_| ToolView),
            closes: self.0.clone(),
        }))
    }
}

/// 固定端点仅供内存提供方处理，不依赖实际路径存在。
fn file_endpoint() -> FileEndpoint {
    FileEndpoint::Local {
        path: "C:/plugin-close-fixture/dirty.txt".into(),
    }
}

/// 从 Root 的正规事件入口发起关闭，不直接调用 editor guard 或工具收尾辅助函数。
fn request_close(root: &Entity<RootView>, tab_id: &TabId, visual: &mut VisualTestContext) {
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.handle_workspace_tab_event(&WorkspaceTabEvent::Close(tab_id.clone()), window, cx);
        });
    });
    visual.run_until_parked();
    file_support::draw(visual);
}

/// 真实输入产生 dirty 后，取消保留原工具和 owner；再次确认才允许启动关闭并移除标签。
#[gpui_kit::test]
fn cancelling_dirty_file_close_keeps_tab_tool_open_until_confirmed(app: &mut TestAppContext) {
    let files = FileService::new(vec![Arc::new(MemoryFile)], Arc::new(NoBookmarks))
        .expect("memory file ports");
    let (runtime, root, _, _, visual) = fixture_with_configuration_and_services(
        app,
        Arc::new(EmptyConfiguration),
        move |mut services| {
            services.file_editor = Some(FileEditorService::new(files.clone()));
            services.files = Some(files);
            services
        },
    );
    visual.update(|window, app| {
        window.activate_window();
        root.update(app, |root, cx| {
            let controller = WorkspaceController::from_missing().unwrap();
            root.workspace_files = WorkspaceFileRuntime::from_snapshot(controller.snapshot());
            root.workspace_controller = Some(controller);
            assert!(root.open_file_endpoint_with_lease(file_endpoint(), None, window, cx));
        });
    });
    settle(&runtime, visual);
    let (editor, tab_id) = root.read_with(visual, |root, cx| {
        let editor = root
            .file_editor_workspace
            .clone()
            .expect("production editor");
        let projection = editor.read(cx).workspace_projection();
        assert_eq!(projection.tabs.len(), 1);
        assert!(
            !projection.tabs[0].loading,
            "the document must be loaded before editing"
        );
        let tab_id = root
            .workspace_controller
            .as_ref()
            .unwrap()
            .snapshot()
            .active_tab_id
            .clone()
            .expect("file tab");
        (editor, tab_id)
    });
    visual.update(|window, cx| {
        assert!(editor.update(cx, |editor, cx| editor.focus_active_editor(window, cx)));
        window.draw(cx).clear(cx);
    });
    visual.simulate_input("unsaved draft");
    visual.run_until_parked();
    assert!(editor.read_with(visual, |editor, _| editor.has_unsaved_changes()));

    let closes = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(ProbeFactory(closes.clone()));
    let provider = PluginId::new("fixture.file-close-provider").unwrap();
    let mut builder = PluginRegistryBuilder::new();
    builder.register_provider(PluginDescriptor::new(provider.clone(), "Fixture", true));
    builder.register_tool(ToolDescriptor::new(
        ToolId::new(factory.tool_id()).unwrap(),
        provider,
        "Close probe",
        None,
    ));
    let registry = builder.freeze().unwrap();
    let tool = ToolRailToolId::from_id(factory.tool_id()).unwrap();
    let context = ToolContext::new(Some(tab_id.clone()), None);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.plugin_tools.register(&registry, factory).unwrap();
            root.plugin_tools
                .ensure(tool, &context, window, cx)
                .unwrap();
        });
    });

    request_close(&root, &tab_id, visual);
    assert!(editor.read_with(visual, |editor, _| editor.root_confirmation_active()));
    assert_eq!(closes.load(Ordering::SeqCst), 0);
    assert_eq!(
        root.read_with(visual, |root, _| root.plugin_tools.phase(tool, &context)),
        Some(ToolPhase::Open)
    );
    click_root_button(visual, "file-editor-confirm-cancel");
    settle(&runtime, visual);
    assert!(editor.read_with(visual, |editor, _| editor.has_unsaved_changes()));
    root.read_with(visual, |root, _| {
        assert_eq!(
            root.plugin_tools.phase(tool, &context),
            Some(ToolPhase::Open)
        );
        let controller = root.workspace_controller.as_ref().unwrap();
        assert!(controller.pending_resource_operation().is_none());
        assert!(
            controller
                .snapshot()
                .terminal_tabs
                .iter()
                .any(|tab| tab.id() == &tab_id)
        );
        assert!(!root.plugin_tab_close_deadlines.contains_key(&tab_id));
    });
    assert_eq!(closes.load(Ordering::SeqCst), 0);

    request_close(&root, &tab_id, visual);
    assert!(editor.read_with(visual, |editor, _| editor.root_confirmation_active()));
    assert_eq!(closes.load(Ordering::SeqCst), 0);
    click_root_button(visual, "file-editor-discard-close");
    assert_eq!(closes.load(Ordering::SeqCst), 1);
    assert_eq!(
        root.read_with(visual, |root, _| root.plugin_tools.phase(tool, &context)),
        Some(ToolPhase::Closing)
    );
    settle(&runtime, visual);
    root.read_with(visual, |root, _| {
        assert!(root.plugin_tools.phase(tool, &context).is_none());
        let controller = root.workspace_controller.as_ref().unwrap();
        assert!(controller.pending_resource_operation().is_none());
        assert!(
            !controller
                .snapshot()
                .terminal_tabs
                .iter()
                .any(|tab| tab.id() == &tab_id)
        );
    });
    assert_eq!(closes.load(Ordering::SeqCst), 1);
}
