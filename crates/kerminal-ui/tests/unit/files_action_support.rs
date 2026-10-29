//! FilesPane 生命周期回归测试共用的无 IO 服务与窗口 fixture。
//!
//! @author kongweiguang

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor,
    files::{FileAccessProvider, FileBookmarkRepository, FileService},
    transfers::{
        TransferBackend, TransferExecutionContext, TransferExecutionError, TransferQueueOptions,
        TransferService,
    },
};
use kerminal_domain::files::{
    BookmarkTarget, DeleteTargetKind, DirectoryListing, FileBookmark, FileEndpoint, FileEntry,
    FileEntryKind, FilePreview, TextDocument, WriteTextRequest, WriteTextResult,
};
use tokio::runtime::{Builder, Runtime};
use tokio_util::sync::CancellationToken;

use crate::features::files::{FileTargetNameLookup, FilesPane};

/// 仅记录文件写操作，确认取消路径没有越过 UI 边界提交任何变更。
#[derive(Default)]
pub(crate) struct MutationCalls {
    pub(crate) create_directory: AtomicUsize,
    pub(crate) rename: AtomicUsize,
    pub(crate) chmod: AtomicUsize,
    pub(crate) delete: AtomicUsize,
}

/// 状态型 FilesPane 测试共用独立 runtime 和拒绝 IO 的 application ports。
pub(crate) struct FilesPaneFixture {
    pub(crate) runtime: Runtime,
    pub(crate) pane: Entity<FilesPane>,
    pub(crate) mutations: Arc<MutationCalls>,
}

/// 创建未启动后台 IO 的 FilesPane entity，测试通过原生产状态入口驱动生命周期。
pub(crate) fn entity_fixture(
    app: &mut TestAppContext,
    endpoint: FileEndpoint,
    seed: impl FnOnce(&mut FilesPane) + 'static,
) -> FilesPaneFixture {
    app.update(gpui_kit::init);
    let (runtime, files, transfers, executor, mutations) = services();
    let pane = app.update(|cx| {
        cx.new(|_| {
            let mut pane = FilesPane::new(
                files,
                transfers,
                executor,
                FileTargetNameLookup::default(),
                endpoint,
            );
            pane.initialized = true;
            seed(&mut pane);
            pane
        })
    });
    FilesPaneFixture {
        runtime,
        pane,
        mutations,
    }
}

/// 创建带真实 GPUI 窗口的 FilesPane，保留列表按键事件及其焦点路由。
pub(crate) fn window_fixture(
    app: &mut TestAppContext,
    endpoint: FileEndpoint,
    seed: impl FnOnce(&mut FilesPane) + 'static,
) -> (
    Runtime,
    Entity<FilesPane>,
    Arc<MutationCalls>,
    &mut VisualTestContext,
) {
    app.update(gpui_kit::init);
    let (runtime, files, transfers, executor, mutations) = services();
    let mounted = std::rc::Rc::new(std::cell::RefCell::new(None));
    let mounted_pane = mounted.clone();
    let files_for_view = files.clone();
    let transfers_for_view = transfers.clone();
    let executor_for_view = executor.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let pane = cx.new(|_| {
            let mut pane = FilesPane::new(
                files_for_view,
                transfers_for_view,
                executor_for_view,
                FileTargetNameLookup::default(),
                endpoint,
            );
            pane.initialized = true;
            seed(&mut pane);
            pane
        });
        *mounted_pane.borrow_mut() = Some(pane.clone());
        gpui_kit::base::Root::new(pane, window, cx)
    });
    let pane = mounted
        .borrow_mut()
        .take()
        .expect("FilesPane window fixture mounted");
    draw(visual);
    (runtime, pane, mutations, visual)
}

/// 绘制两帧以完成文件列表焦点与测试辅助节点注册。
pub(crate) fn draw(visual: &mut VisualTestContext) {
    visual.run_until_parked();
    for _ in 0..2 {
        visual.update(|window, app| window.draw(app).clear(app));
    }
}

/// 构造完整远端 endpoint，确保 fixture 不依赖宿主本地路径语法。
pub(crate) fn remote_endpoint(
    host: &kerminal_domain::identity::HostId,
    path: &str,
) -> FileEndpoint {
    FileEndpoint::remote(host.clone(), path).expect("valid test endpoint")
}

/// 按 parent endpoint 构造稳定的测试行，保留树和列表一致的真实路径身份。
pub(crate) fn remote_entry(parent: &FileEndpoint, name: &str, kind: FileEntryKind) -> FileEntry {
    let FileEndpoint::Remote { host_id, path } = parent else {
        panic!("test directory must be remote");
    };
    FileEntry {
        endpoint: FileEndpoint::Remote {
            host_id: host_id.clone(),
            path: path.join_name(name).expect("valid test item name"),
        },
        name: name.to_owned(),
        kind,
        size: None,
        modified: None,
        permissions: None,
        is_hidden: name.starts_with('.'),
        raw: None,
    }
}

/// 安装与指定代次匹配的目录快照，使失效测试能分离比较对象与结果。
pub(crate) fn seed_listing(
    pane: &mut FilesPane,
    side: usize,
    generation: u64,
    entries: Vec<FileEntry>,
) {
    let directory = pane.panels[side].endpoint.clone();
    pane.panels[side].generation = generation;
    pane.panels[side].loading = false;
    pane.panels[side].listing = Some(DirectoryListing { directory, entries });
}

/// 绘制边界测试使用严格隔离 runtime 和不执行传输/文件 IO 的 ports。
fn services() -> (
    Runtime,
    FileService,
    TransferService,
    ApplicationTaskExecutor,
    Arc<MutationCalls>,
) {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime can be created");
    let mutations = Arc::new(MutationCalls::default());
    let files = FileService::new(
        vec![Arc::new(StateOnlyFileProvider {
            mutations: mutations.clone(),
        })],
        Arc::new(EmptyBookmarks),
    )
    .expect("test file provider is registered");
    let transfers = TransferService::new(
        vec![Arc::new(DisabledTransferBackend)],
        runtime.handle().clone(),
        TransferQueueOptions::default(),
    )
    .expect("test transfer backend is registered");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    (runtime, files, transfers, executor, mutations)
}

/// 不支持任何真实传输，防止 UI 生命周期测试误触队列执行。
struct DisabledTransferBackend;

#[async_trait]
impl TransferBackend for DisabledTransferBackend {
    /// 测试不入队传输任务，后端声明没有可执行请求。
    fn supports(&self, _: &kerminal_domain::files::TransferRequest) -> bool {
        false
    }

    /// 任何意外调用都失败关闭，测试应先在可观察 UI 状态中发现它。
    async fn execute(
        &self,
        _: TransferExecutionContext,
    ) -> Result<kerminal_application::transfers::TransferExecutionResult, TransferExecutionError>
    {
        panic!("state-only FilesPane fixture must not execute transfers")
    }
}

/// 文件 provider 仅允许构造 pane；预览、目录和写操作都不访问实际文件系统。
struct StateOnlyFileProvider {
    mutations: Arc<MutationCalls>,
}

#[async_trait]
impl FileAccessProvider for StateOnlyFileProvider {
    /// 接受所有目标以保持 FilesPane 初始化可验证，所有 IO 方法都显式拒绝执行。
    fn supports(&self, _: &FileEndpoint) -> bool {
        true
    }

    /// 测试不执行目录读取，避免将 UI 状态测试变成文件系统测试。
    async fn list_directory(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
    ) -> AppResult<DirectoryListing> {
        Err(AppError::unsupported("state-only test provider"))
    }

    /// 测试不读取预览数据，防止服务边界外的 IO 影响结果。
    async fn preview_file(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: usize,
    ) -> AppResult<FilePreview> {
        Err(AppError::unsupported("state-only test provider"))
    }

    /// 测试不打开可编辑文本文件。
    async fn read_text_file(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: usize,
    ) -> AppResult<TextDocument> {
        Err(AppError::unsupported("state-only test provider"))
    }

    /// 测试不提交文本写入。
    async fn write_text_file(
        &self,
        _: CancellationToken,
        _: WriteTextRequest,
    ) -> AppResult<WriteTextResult> {
        Err(AppError::unsupported("state-only test provider"))
    }

    /// 记录目录创建提交，取消回归会断言计数仍为零。
    async fn create_directory(&self, _: CancellationToken, _: FileEndpoint) -> AppResult<()> {
        self.mutations
            .create_directory
            .fetch_add(1, Ordering::AcqRel);
        Err(AppError::unsupported("state-only test provider"))
    }

    /// 记录重命名提交，取消回归会断言计数仍为零。
    async fn rename(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: FileEndpoint,
    ) -> AppResult<()> {
        self.mutations.rename.fetch_add(1, Ordering::AcqRel);
        Err(AppError::unsupported("state-only test provider"))
    }

    /// 记录权限提交，取消回归会断言计数仍为零。
    async fn chmod(&self, _: CancellationToken, _: FileEndpoint, _: u32) -> AppResult<()> {
        self.mutations.chmod.fetch_add(1, Ordering::AcqRel);
        Err(AppError::unsupported("state-only test provider"))
    }

    /// 记录删除提交，取消回归会断言计数仍为零。
    async fn delete(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: DeleteTargetKind,
    ) -> AppResult<()> {
        self.mutations.delete.fetch_add(1, Ordering::AcqRel);
        Err(AppError::unsupported("state-only test provider"))
    }
}

/// FilesPane 生命周期测试不使用书签，repository 仍需满足 FileService 装配约束。
struct EmptyBookmarks;

#[async_trait]
impl FileBookmarkRepository for EmptyBookmarks {
    /// 返回空集合，不读取用户数据目录。
    async fn list(&self, _: CancellationToken, _: BookmarkTarget) -> AppResult<Vec<FileBookmark>> {
        Ok(Vec::new())
    }

    /// 测试不写入书签。
    async fn set(
        &self,
        _: CancellationToken,
        _: FileBookmark,
        _: bool,
    ) -> AppResult<Vec<FileBookmark>> {
        Err(AppError::unsupported("state-only test bookmarks"))
    }
}
