//! 文件宿主的 Root 导航与编辑器回执回归；文件端口全部为内存实现。
//!
//! @author kongweiguang

use super::*;

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use gpui_kit::{TestAppContext, VisualTestContext};
use kerminal_application::{
    connections::management::{
        HostAuthenticationMode, HostDocumentRevision, HostDraft, HostGroupDeletionReceipt,
        HostGroupDraft, HostGroupSnapshot, HostManagementRepository, HostManagementService,
        HostRouteSettings, ManagedHost,
    },
    files::{FileAccessProvider, FileBookmarkRepository, FileService, editor::FileEditorService},
    plugins::{PluginId, PluginRegistry, PluginRegistryBuilder},
    workspace::WorkspaceSessionSnapshot,
};
use kerminal_domain::connections::{HostEndpoint, RemoteProtocol};
use kerminal_domain::files::{
    BookmarkTarget, DeleteTargetKind, DirectoryListing, FileBookmark, FilePreview, FileRevision,
    LineEnding, TextDocument, WriteTextRequest, WriteTextResult,
};
use kerminal_domain::preferences::ConfigRevision;
use tokio::{runtime::Runtime, sync::Notify};

use crate::features::workspace_controller::{WorkspaceController, WorkspaceRestoreFailure};
use crate::root::AppServices;

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
    fixture_with(app, |services| services)
}

/// 在隔离 Root 装配上叠加测试指定的冻结服务，避免每个门禁用例复制整套假端口。
fn fixture_with(
    app: &mut TestAppContext,
    configure: impl FnOnce(AppServices) -> AppServices,
) -> (Runtime, Entity<RootView>, &mut VisualTestContext) {
    let (runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, |mut services| {
            let files = FileService::new(vec![Arc::new(MemoryFiles)], Arc::new(NoBookmarks))
                .expect("memory file service");
            services.file_editor = Some(FileEditorService::new(files));
            configure(services)
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

/// 冻结启用视图只关闭指定内置单元，其余保持生产默认；用于验证协议与容器/tmux 门禁。
fn gated_plugins(disabled: &[&str]) -> Arc<PluginRegistry> {
    let mut builder = PluginRegistryBuilder::new();
    builder.register_builtins().expect("valid builtins");
    let states = kerminal_application::plugins::builtin_unit_ids()
        .iter()
        .map(|id| {
            (
                PluginId::new(*id).expect("valid builtin id"),
                !disabled.contains(id),
            )
        })
        .collect::<BTreeMap<_, _>>();
    Arc::new(
        builder
            .freeze_with_builtin_states(&states)
            .expect("valid gated registry"),
    )
}

/// 权威确认回执的三种测试结局：配置中缺失、持久协议与读取失败。
#[derive(Clone, Copy)]
enum FixtureHostLookup {
    Absent,
    Persisted(RemoteProtocol),
    Failed,
}

/// 只实现读取路径的主机仓储；写操作显式失败，避免夹具触碰真实配置目录。
struct FixtureHosts {
    control: FixtureHostControl,
}

/// 夹具读取句柄：可在恢复完成后切换后续权威读取的结局，并可选挂起请求。
///
/// 文件目标名称解析也会走同一个主机服务；因此断言不能只数总读取次数，而是用
/// “切换为失败后目录仍不得创建”反证激活确实重新确认，而不是复用旧结论。
#[derive(Clone)]
struct FixtureHostControl {
    lookup: Arc<std::sync::Mutex<FixtureHostLookup>>,
    loads: Arc<AtomicUsize>,
    notify: Option<Arc<Notify>>,
    blocked: Arc<AtomicBool>,
}

impl FixtureHostControl {
    /// 只改变后续读取的返回，不触碰已完成的恢复状态。
    fn set_lookup(&self, lookup: FixtureHostLookup) {
        *self.lookup.lock().expect("fixture host lookup") = lookup;
    }

    /// 已发起的权威读取次数；用于确认竞态用例确实制造了在途窗口。
    fn loads(&self) -> usize {
        self.loads.load(Ordering::SeqCst)
    }

    /// 控制后续读取是否挂起；测试用它把确认稳定停在回执之前。
    fn set_blocked(&self, blocked: bool) {
        self.blocked.store(blocked, Ordering::SeqCst);
    }

    /// 放行已挂起的读取，并让后续读取按当前结局直接完成。
    fn release(&self) {
        self.blocked.store(false, Ordering::SeqCst);
        if let Some(notify) = &self.notify {
            notify.notify_waiters();
        }
    }
}

#[async_trait]
impl HostManagementRepository for FixtureHosts {
    /// 夹具不提供配置写权限。
    async fn ensure_host_writable(&self) -> AppResult<()> {
        Err(AppError::unsupported(
            "fixture host repository is read only",
        ))
    }

    /// 导航目录保持为空，确保测试只观察 `load_host` 的权威协议确认。
    async fn list_hosts(&self) -> AppResult<Vec<ManagedHost>> {
        Ok(Vec::new())
    }

    /// 按测试结局返回持久协议、缺失身份或读取失败；失败不能被当成缺失放行。
    ///
    /// 先计数再等待门闩，测试可以据此确认请求已发出且仍未被回执消费；结局在等待
    /// 之后读取，允许测试在在途期间切换后续请求的返回。
    async fn load_host(&self, host_id: &HostId) -> AppResult<Option<ManagedHost>> {
        self.control.loads.fetch_add(1, Ordering::SeqCst);
        if self.control.blocked.load(Ordering::SeqCst)
            && let Some(notify) = &self.control.notify
        {
            notify.notified().await;
        }
        let lookup = *self.control.lookup.lock().expect("fixture host lookup");
        match lookup {
            FixtureHostLookup::Absent => Ok(None),
            FixtureHostLookup::Failed => Err(AppError::infrastructure("fixture host read failure")),
            FixtureHostLookup::Persisted(protocol) => {
                Ok(Some(fixture_managed_host(host_id.clone(), protocol)))
            }
        }
    }

    /// 夹具不保存主机。
    async fn save_host(
        &self,
        _: HostId,
        _: HostDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<ManagedHost> {
        Err(AppError::unsupported(
            "fixture host repository is read only",
        ))
    }

    /// 夹具不删除主机。
    async fn delete_host(&self, _: &HostId, _: &ConfigRevision) -> AppResult<ManagedHost> {
        Err(AppError::unsupported(
            "fixture host repository is read only",
        ))
    }

    /// 空分组让导航保持“未同步”而不是伪造持久主机条目。
    async fn load_groups(&self) -> AppResult<HostGroupSnapshot> {
        Ok(HostGroupSnapshot::default())
    }

    /// 夹具不保存分组。
    async fn save_group(
        &self,
        _: HostGroupDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<HostGroupSnapshot> {
        Err(AppError::unsupported(
            "fixture host repository is read only",
        ))
    }

    /// 夹具不删除分组。
    async fn delete_group_and_reassign(
        &self,
        _: String,
        _: Option<String>,
        _: ConfigRevision,
        _: Vec<HostDocumentRevision>,
    ) -> AppResult<HostGroupDeletionReceipt> {
        Err(AppError::unsupported(
            "fixture host repository is read only",
        ))
    }
}

/// 构造最小持久主机身份；字段只服务协议判定，不携带凭据或真实端点。
fn fixture_managed_host(id: HostId, protocol: RemoteProtocol) -> ManagedHost {
    ManagedHost {
        id,
        group_id: None,
        name: "fixture host".to_owned(),
        note: None,
        protocol,
        endpoint: HostEndpoint {
            host: "example.invalid".to_owned(),
            port: 22,
            username: "tester".to_owned(),
        },
        authentication: None,
        authentication_mode: HostAuthenticationMode::Agent,
        rdp_password: None,
        route: HostRouteSettings::default(),
        serial: None,
        tags: Vec::new(),
        sort_order: 0,
        connect_timeout_ms: 30_000,
        keepalive_interval_ms: None,
        terminal_options: Default::default(),
        sftp_options: Default::default(),
        rdp_display: Default::default(),
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
        revision: ConfigRevision::from_sha256("fixture-revision".to_owned()),
    }
}

/// 注入同一冻结启用视图的主机服务，保证导航与恢复共享协议门禁。
fn host_service(lookup: FixtureHostLookup, disabled: &[&str]) -> HostManagementService {
    observed_host_service(lookup, disabled, None).0
}

/// 可观察或可切换读取结局的主机服务；`notify` 只用于需要挂起权威读取的关闭竞态用例。
fn observed_host_service(
    lookup: FixtureHostLookup,
    disabled: &[&str],
    notify: Option<Arc<Notify>>,
) -> (HostManagementService, FixtureHostControl) {
    let control = FixtureHostControl {
        lookup: Arc::new(std::sync::Mutex::new(lookup)),
        loads: Arc::new(AtomicUsize::new(0)),
        blocked: Arc::new(AtomicBool::new(notify.is_some())),
        notify,
    };
    let repository = FixtureHosts {
        control: control.clone(),
    };
    (
        HostManagementService::new(Arc::new(repository)).with_plugins(gated_plugins(disabled)),
        control,
    )
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

/// 只改控制器已提交的活动标签，模拟用户切走/切回；不触发额外的恢复入口。
fn activate_committed_tab(root: &Entity<RootView>, visual: &mut VisualTestContext, tab: &TabId) {
    visual.update(|_window, app| {
        root.update(app, |root, _cx| {
            let controller = root.workspace_controller.as_mut().expect("controller");
            let mut snapshot = controller.snapshot().clone();
            snapshot.active_tab_id = Some(tab.clone());
            controller
                .synchronize_snapshot(snapshot)
                .expect("activate committed tab");
        });
    });
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

/// 中间标签只由生产文档按钮命中切换；宿主不提供新建目录或定位入口，标签带保持极简。
/// @author kongweiguang
#[gpui_kit::test]
fn production_document_tab_click_keeps_the_same_owner(app: &mut TestAppContext) {
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
    assert!(visual.debug_bounds("host-file-new-directory").is_none());
    assert!(visual.debug_bounds("host-file-locate-file").is_none());
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
                // 内部关闭回归直接构造第二个目录项，验证目录与文档共用同一资源门禁。
                root.update_host_file_state(&owner, |state| {
                    let path = state.items.first().map(|item| item.path().to_owned());
                    let directory = TabId::new();
                    state.items.push(FileWorkspaceItem::Directory {
                        id: directory.clone(),
                        path: path.unwrap_or_default(),
                    });
                    state.active_item_id = Some(directory);
                    Ok(())
                })
                .expect("add another directory item");
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

/// 文件工作区把文件上下文行（宿主描述与保存）放在文档标签带上方；目录不进入标签带，
/// 切到目录时标签带仍停在同一行，不会上下跳动。
#[gpui_kit::test]
fn host_file_workspace_lists_editor_toolbar_above_document_tabs(app: &mut TestAppContext) {
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
    let document = state
        .items
        .iter()
        .find_map(|item| match item {
            FileWorkspaceItem::Document { id, .. } => Some(id.clone()),
            FileWorkspaceItem::Directory { .. } => None,
        })
        .expect("document item");
    let strip = visual
        .debug_bounds("host-file-tab-strip")
        .expect("document tab strip");
    let save = visual
        .debug_bounds("rr-file-editor-save")
        .expect("editor save button");
    assert!(
        save.origin.y < strip.origin.y,
        "the host description and save row stay above the document tab strip"
    );
    let document_selector: &'static str =
        Box::leak(format!("host-file-item-{}", document.as_str()).into_boxed_str());
    assert!(
        visual.debug_bounds(document_selector).is_some(),
        "opened document stays visible in the tab strip"
    );
    let directory_selector: &'static str =
        Box::leak(format!("host-file-item-{}", directory.as_str()).into_boxed_str());
    assert!(
        visual.debug_bounds(directory_selector).is_none(),
        "SFTP directory must not appear as a tab inside the file workspace"
    );
    // 目录活动时仍保留文档标签带，上下文行换成宿主+目录说明；位置必须与文档态一致，
    // 否则用户在目录与文档之间切换时标签带会上下跳动。
    let strip_top = strip.origin.y;
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.activate_host_file_item(&owner, &directory, window, cx);
        });
    });
    settle(&runtime, visual);
    for _ in 0..2 {
        visual.update(|window, app| window.draw(app).clear(app));
    }
    let directory_strip = visual
        .debug_bounds("host-file-tab-strip")
        .expect("tab strip remains visible with a directory active");
    let directory_context = visual
        .debug_bounds("host-file-directory-context")
        .expect("directory context row");
    assert_eq!(
        directory_strip.origin.y, strip_top,
        "switching to a directory keeps the document tab strip on the same row"
    );
    assert!(
        directory_context.origin.y < directory_strip.origin.y,
        "the directory context row stays above the document tab strip"
    );
    assert!(visual.debug_bounds("host-file-new-directory").is_none());
    assert!(visual.debug_bounds("host-file-locate-file").is_none());
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
    assert!(
        visual.debug_bounds("rr-file-editor-tab-0").is_none(),
        "the host must not draw a second editor tab strip below its toolbar"
    );
}

/// 文件工作区目录门禁：SFTP/SSH 按导航快照收紧，快照缺失且 SSH 禁用时无法确认协议必须拒绝；
/// 容器目录还要求 SSH 与 containers 单元，本机目录不门禁。
///
/// @author kongweiguang
#[test]
fn host_file_workspace_restore_gate_rejects_disabled_or_unconfirmed_protocols() {
    use super::restore::{FileWorkspaceTargetGate, file_workspace_target_gate};

    let host_id = HostId::parse("host-a").expect("fixture host");
    let ssh_target = WorkspaceTargetRef::Ssh {
        host_id: host_id.clone(),
    };
    let local_target = WorkspaceTargetRef::Local { profile_id: None };
    let container_target = WorkspaceTargetRef::DockerContainer {
        host_id,
        container_id: "container-a".to_owned(),
        runtime: kerminal_application::workspace::WorkspaceContainerRuntime::Docker,
        container_name: None,
        user: None,
        workdir: None,
    };

    assert_eq!(
        file_workspace_target_gate(&ssh_target, Some(RemoteProtocol::Ssh), |_| false, false),
        FileWorkspaceTargetGate::ProtocolDisabled(RemoteProtocol::Sftp)
    );
    assert_eq!(
        file_workspace_target_gate(
            &ssh_target,
            Some(RemoteProtocol::Ssh),
            |protocol| protocol != RemoteProtocol::Ssh,
            false,
        ),
        FileWorkspaceTargetGate::ProtocolDisabled(RemoteProtocol::Ssh)
    );
    // 快照明确为纯 SFTP 主机时只按 SFTP 判定，SSH 单元禁用不影响该目标。
    assert_eq!(
        file_workspace_target_gate(
            &ssh_target,
            Some(RemoteProtocol::Sftp),
            |protocol| protocol != RemoteProtocol::Ssh,
            false,
        ),
        FileWorkspaceTargetGate::Allowed
    );
    // 快照缺失时若 SSH 仍启用，仍按 SFTP 单条件放行；SSH 也禁用则无法确认协议。
    assert_eq!(
        file_workspace_target_gate(&ssh_target, None, |_| true, false),
        FileWorkspaceTargetGate::Allowed
    );
    assert_eq!(
        file_workspace_target_gate(
            &ssh_target,
            None,
            |protocol| protocol != RemoteProtocol::Ssh,
            false,
        ),
        FileWorkspaceTargetGate::ProtocolUnconfirmed
    );
    assert_eq!(
        file_workspace_target_gate(&local_target, Some(RemoteProtocol::Ssh), |_| false, false),
        FileWorkspaceTargetGate::Allowed
    );

    assert_eq!(
        file_workspace_target_gate(&container_target, None, |_| false, true),
        FileWorkspaceTargetGate::ProtocolDisabled(RemoteProtocol::Ssh)
    );
    assert_eq!(
        file_workspace_target_gate(&container_target, None, |_| true, false),
        FileWorkspaceTargetGate::ContainersDisabled
    );
    assert_eq!(
        file_workspace_target_gate(&container_target, None, |_| true, true),
        FileWorkspaceTargetGate::Allowed
    );

    let (failure, error) = FileWorkspaceTargetGate::ProtocolUnconfirmed
        .rejection()
        .expect("unconfirmed protocol is rejected");
    assert_eq!(
        failure,
        WorkspaceRestoreFailure::UnavailableTarget(WorkspaceTargetUnavailableReason::UnknownKind)
    );
    assert!(error.public_message().contains("主机目录尚未同步"));
    assert!(
        FileWorkspaceTargetGate::ProtocolUnconfirmed
            .rejection()
            .expect("rejection")
            .1
            .public_message()
            .contains("已保留材料")
    );
    let containers = FileWorkspaceTargetGate::ContainersDisabled
        .rejection()
        .expect("disabled containers are rejected");
    assert!(containers.1.public_message().contains("原标签和状态仍保留"));
}

/// 快照缺失时，`load_host` 的权威结论必须区分纯 SFTP/external、持久 SSH 与读取失败；
/// SFTP 禁用仍是终态拒绝，失败结论永远不能放行远端 I/O。
///
/// @author kongweiguang
#[test]
fn host_file_workspace_confirmation_gate_preserves_protocol_dependencies() {
    use super::restore::{
        FileWorkspaceTargetGate, HostProtocolLookup, file_workspace_confirmed_target_gate,
    };

    let host_id = HostId::parse("host-a").expect("fixture host");
    let ssh_target = WorkspaceTargetRef::Ssh {
        host_id: host_id.clone(),
    };
    let local_target = WorkspaceTargetRef::Local { profile_id: None };
    let container_target = WorkspaceTargetRef::DockerContainer {
        host_id,
        container_id: "container-a".to_owned(),
        runtime: kerminal_application::workspace::WorkspaceContainerRuntime::Docker,
        container_name: None,
        user: None,
        workdir: None,
    };
    let ssh_disabled = |protocol| protocol != RemoteProtocol::Ssh;

    // 快照缺失 + SFTP 启用 + 权威结论不是持久主机：external/临时目标应放行。
    assert_eq!(
        file_workspace_confirmed_target_gate(
            &ssh_target,
            HostProtocolLookup::Unpersisted,
            ssh_disabled,
            false,
        ),
        Some(FileWorkspaceTargetGate::Allowed)
    );
    // 持久 SSH 主机在 SSH 禁用时仍是协议禁用，不能因确认路径重新放行。
    assert_eq!(
        file_workspace_confirmed_target_gate(
            &ssh_target,
            HostProtocolLookup::Persisted(RemoteProtocol::Ssh),
            ssh_disabled,
            false,
        ),
        Some(FileWorkspaceTargetGate::ProtocolDisabled(
            RemoteProtocol::Ssh
        ))
    );
    // 持久纯 SFTP 主机只要求 SFTP，SSH 单元禁用不影响恢复。
    assert_eq!(
        file_workspace_confirmed_target_gate(
            &ssh_target,
            HostProtocolLookup::Persisted(RemoteProtocol::Sftp),
            ssh_disabled,
            false,
        ),
        Some(FileWorkspaceTargetGate::Allowed)
    );
    // SFTP 本身禁用时，无论确认结果如何都不能放行。
    assert_eq!(
        file_workspace_confirmed_target_gate(
            &ssh_target,
            HostProtocolLookup::Unpersisted,
            |_| false,
            true,
        ),
        Some(FileWorkspaceTargetGate::ProtocolDisabled(
            RemoteProtocol::Sftp
        ))
    );
    // 读取失败没有权威结论，必须保持不可放行，不能降级成“非持久主机”。
    assert_eq!(
        file_workspace_confirmed_target_gate(
            &ssh_target,
            HostProtocolLookup::Failed,
            |_| true,
            true,
        ),
        None
    );
    // 非 SSH 目标不受这条确认路径改变语义。
    assert_eq!(
        file_workspace_confirmed_target_gate(
            &local_target,
            HostProtocolLookup::Unpersisted,
            |_| false,
            false,
        ),
        Some(FileWorkspaceTargetGate::Allowed)
    );
    assert_eq!(
        file_workspace_confirmed_target_gate(
            &container_target,
            HostProtocolLookup::Persisted(RemoteProtocol::Sftp),
            |_| true,
            false,
        ),
        Some(FileWorkspaceTargetGate::ContainersDisabled)
    );
    assert_eq!(
        file_workspace_confirmed_target_gate(
            &container_target,
            HostProtocolLookup::Unpersisted,
            ssh_disabled,
            true,
        ),
        Some(FileWorkspaceTargetGate::ProtocolDisabled(
            RemoteProtocol::Ssh
        ))
    );
}

/// 快照缺失且 SSH 禁用时，权威读取确认目标不在持久配置中（external/临时 lease）应放行；
/// 后续目录激活必须重新读取并完成装配，不能因同一快照缺失而误拒或复用旧结论。
///
/// @author kongweiguang
#[gpui_kit::test]
fn unpersisted_target_restores_after_authoritative_confirmation(app: &mut TestAppContext) {
    let mut directory_runtime = None;
    let mut control = None;
    let (runtime, root, visual) = fixture_with(app, |mut services| {
        let (hosts, host_control) = observed_host_service(
            FixtureHostLookup::Absent,
            &["kerminal.ssh"],
            Some(Arc::new(Notify::new())),
        );
        control = Some(host_control);
        services.hosts = Some(hosts);
        // 目录装配还要求文件与传输端口；测试用拒绝真实 I/O 的内存端口，只为验证门禁放行。
        let (directory_rt, files, transfers, _, _) =
            crate::features::files::action_test_support::services();
        directory_runtime = Some(directory_rt);
        services.files = Some(files);
        services.transfers = Some(transfers);
        services
    });
    let control = control.expect("host read control");
    let _directory_runtime = directory_runtime.expect("directory service runtime");
    let endpoint = remote("unsynced-host", "/etc/config.txt");
    let owner = open(&root, visual, endpoint);
    settle(&runtime, visual);

    // 权威读取默认挂起，恢复必须停在 Pending，并已发出至少一次真实 load_host。
    assert!(
        control.loads() >= 1,
        "restore must issue an authoritative host read"
    );
    root.read_with(visual, |root, _| {
        assert!(matches!(
            root.workspace_files.restore_state(&owner),
            Some(WorkspaceFileRestoreState::Pending)
        ));
        assert!(root.workspace_files.directories.is_empty());
        assert!(root.workspace_files.documents.is_empty());
    });

    control.release();
    settle(&runtime, visual);
    root.read_with(visual, |root, _| {
        assert!(matches!(
            root.workspace_files.restore_state(&owner),
            Some(WorkspaceFileRestoreState::Ready)
        ));
        assert!(
            !root.workspace_files.documents.is_empty(),
            "confirmed target must restore its document"
        );
    });
    let loads_after_restore = control.loads();

    // Ready 后的目录激活发生在导航仍未同步时；必须重新读取，而不是复用恢复结论。
    let directory = root.read_with(visual, |root, _| {
        owner_state(root, &owner)
            .items
            .iter()
            .find(|item| matches!(item, FileWorkspaceItem::Directory { .. }))
            .expect("directory item")
            .id()
            .clone()
    });
    control.set_blocked(true);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.activate_host_file_item(&owner, &directory, window, cx);
            root.activate_host_file_workspace(&owner, window, cx);
        });
    });
    settle(&runtime, visual);
    assert!(
        control.loads() > loads_after_restore,
        "directory activation must re-read the host configuration"
    );
    root.read_with(visual, |root, _| {
        assert!(
            root.workspace_files.directories.is_empty(),
            "directory pane must wait for the fresh authoritative conclusion"
        );
        assert!(matches!(
            root.workspace_files.restore_state(&owner),
            Some(WorkspaceFileRestoreState::Ready)
        ));
    });

    control.release();
    settle(&runtime, visual);
    root.read_with(visual, |root, _| {
        assert!(
            root.workspace_files
                .directories
                .contains_key(&(owner.clone(), directory)),
            "activation must keep the confirmed target usable: {:?}",
            root.status
        );
    });
}

/// 恢复完成后的目录激活必须重新取得权威结论，不能复用恢复阶段取得的放行结论。
///
/// 在恢复完成后把夹具结局切换为读取失败：如果激活复用旧结论，目录会被直接创建；
/// 正确实现会重新读取、保持 Ready，只提示目录失败而不发起远端访问。
///
/// @author kongweiguang
#[gpui_kit::test]
fn directory_activation_reconfirms_instead_of_reusing_restore_conclusion(app: &mut TestAppContext) {
    let mut directory_runtime = None;
    let mut control = None;
    let (runtime, root, visual) = fixture_with(app, |mut services| {
        let (hosts, host_control) =
            observed_host_service(FixtureHostLookup::Absent, &["kerminal.ssh"], None);
        control = Some(host_control);
        services.hosts = Some(hosts);
        // 目录装配需要真实的文件与传输端口；内存实现拒绝任何远端 I/O。
        let (directory_rt, files, transfers, _, _) =
            crate::features::files::action_test_support::services();
        directory_runtime = Some(directory_rt);
        services.files = Some(files);
        services.transfers = Some(transfers);
        services
    });
    let control = control.expect("host read control");
    let _directory_runtime = directory_runtime.expect("directory service runtime");
    let owner = open(&root, visual, remote("reconfirm-host", "/etc/config.txt"));
    settle(&runtime, visual);
    let directory = root.read_with(visual, |root, _| {
        assert!(matches!(
            root.workspace_files.restore_state(&owner),
            Some(WorkspaceFileRestoreState::Ready)
        ));
        assert!(
            root.workspace_files.directories.is_empty(),
            "restoring documents must not create a directory pane"
        );
        owner_state(root, &owner)
            .items
            .iter()
            .find(|item| matches!(item, FileWorkspaceItem::Directory { .. }))
            .expect("directory item")
            .id()
            .clone()
    });

    control.set_lookup(FixtureHostLookup::Failed);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.activate_host_file_item(&owner, &directory, window, cx);
            root.activate_host_file_workspace(&owner, window, cx);
        });
    });
    settle(&runtime, visual);

    root.read_with(visual, |root, _| {
        assert!(
            !root
                .workspace_files
                .directories
                .contains_key(&(owner.clone(), directory)),
            "directory activation must re-confirm instead of reusing the restore conclusion"
        );
        assert!(
            matches!(
                root.workspace_files.restore_state(&owner),
                Some(WorkspaceFileRestoreState::Ready)
            ),
            "directory confirmation failure must keep the workspace Ready"
        );
        assert!(
            !root.workspace_files.documents.is_empty(),
            "directory confirmation failure must retain restored documents"
        );
        assert!(
            root.status.as_ref().is_some_and(|status| {
                status.contains("主机配置读取失败")
                    || status.contains("Reading the host configuration failed")
            }),
            "directory confirmation failure must be visible: {:?}",
            root.status
        );
    });
}

/// 权威确认结果为持久 SSH 主机时，SSH 禁用仍沿用既有协议禁用失败，不创建远端实体。
///
/// @author kongweiguang
#[gpui_kit::test]
fn persisted_ssh_target_stays_rejected_after_authoritative_confirmation(app: &mut TestAppContext) {
    let (runtime, root, visual) = fixture_with(app, |mut services| {
        services.hosts = Some(host_service(
            FixtureHostLookup::Persisted(RemoteProtocol::Ssh),
            &["kerminal.ssh"],
        ));
        services
    });
    let owner = open(
        &root,
        visual,
        remote("persisted-ssh-host", "/etc/config.txt"),
    );
    settle(&runtime, visual);

    root.read_with(visual, |root, _| {
        let Some(WorkspaceFileRestoreState::Failed {
            message, failure, ..
        }) = root.workspace_files.restore_state(&owner)
        else {
            panic!("expected protocol-disabled failure");
        };
        assert!(message.contains("SSH 协议已禁用"), "{message}");
        assert_eq!(
            *failure,
            WorkspaceRestoreFailure::UnavailableTarget(
                WorkspaceTargetUnavailableReason::UnknownKind
            )
        );
        assert!(root.workspace_files.directories.is_empty());
        assert!(root.workspace_files.documents.is_empty());
    });
}

/// 主机配置读取失败必须保留恢复材料并显示失败，不能伪装成“非持久主机”去创建 pane；
/// 失败页上的激活也不能重新发起远端 I/O 或覆盖原故障文案。
///
/// @author kongweiguang
#[gpui_kit::test]
fn host_read_failure_keeps_failure_page_and_skips_remote_io(app: &mut TestAppContext) {
    let (runtime, root, visual) = fixture_with(app, |mut services| {
        services.hosts = Some(host_service(FixtureHostLookup::Failed, &["kerminal.ssh"]));
        services
    });
    let owner = open(&root, visual, remote("unreadable-host", "/etc/config.txt"));
    settle(&runtime, visual);

    let (failure_message, failure_state) = root.read_with(visual, |root, _| {
        let Some(WorkspaceFileRestoreState::Failed {
            message, failure, ..
        }) = root.workspace_files.restore_state(&owner)
        else {
            panic!("expected host read failure");
        };
        // 文案随全局语言选择切换；断言两种受支持语言中的稳定前缀。
        assert!(
            message.contains("主机配置读取失败")
                || message.contains("Reading the host configuration failed"),
            "{message}"
        );
        assert!(root.workspace_files.directories.is_empty());
        assert!(root.workspace_files.documents.is_empty());
        (message.clone(), *failure)
    });
    root.update(visual, |root, _| {
        root.center_panel = CenterPanel::Files;
    });

    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.activate_host_file_workspace(&owner, window, cx);
        });
    });

    root.read_with(visual, |root, _cx| {
        assert_eq!(
            root.center_panel,
            CenterPanel::Files,
            "activation must not switch the center panel before the restore is Ready"
        );
        assert!(
            root.workspace_files.directories.is_empty(),
            "activation must not create a remote directory pane after a failed restore"
        );
        assert!(
            root.workspace_files.documents.is_empty(),
            "activation must not open documents after a failed restore"
        );
        match root.workspace_files.restore_state(&owner) {
            Some(WorkspaceFileRestoreState::Failed { message, failure }) => {
                assert_eq!(message, &failure_message);
                assert_eq!(*failure, failure_state);
            }
            other => panic!("the failure page must keep its original message, got {other:?}"),
        }
    });
}

/// 权威读取尚未返回时关闭 Tab，回执必须在应用前被丢弃，不能复活宿主或创建远端实体。
///
/// 在途表没有测试专用观察入口（src 中的 test-only 项会触发架构门禁），因此用行为
/// 断言回执无效；条目移除由回执入口的无条件消费与 UI 回调失败路径的结构保证。
///
/// @author kongweiguang
#[gpui_kit::test]
fn closing_owner_while_confirmation_in_flight_discards_result(app: &mut TestAppContext) {
    let mut control = None;
    let mut release_load = None;
    let (runtime, root, visual) = fixture_with(app, |mut services| {
        let notify = Arc::new(Notify::new());
        let (hosts, host_control) = observed_host_service(
            FixtureHostLookup::Persisted(RemoteProtocol::Sftp),
            &["kerminal.ssh"],
            Some(notify.clone()),
        );
        control = Some(host_control);
        release_load = Some(notify);
        services.hosts = Some(hosts);
        services
    });
    let control = control.expect("host read control");
    let release_load = release_load.expect("host read gate");
    let owner = open(&root, visual, remote("pending-host", "/etc/config.txt"));
    settle(&runtime, visual);
    assert_eq!(
        control.loads(),
        1,
        "confirmation must be in flight before the owner is closed"
    );
    root.read_with(visual, |root, _| {
        assert!(matches!(
            root.workspace_files.restore_state(&owner),
            Some(WorkspaceFileRestoreState::Pending)
        ));
    });

    visual.update(|_window, app| {
        root.update(app, |root, cx| {
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
            root.center_panel = CenterPanel::Terminal;
            cx.notify();
        });
    });

    release_load.notify_one();
    settle(&runtime, visual);

    root.read_with(visual, |root, _| {
        assert!(
            !root.workspace_files.tabs.contains_key(&owner),
            "late confirmation must not recreate the closed owner"
        );
        assert!(
            root.workspace_files.directories.is_empty(),
            "late confirmation must not create a directory pane"
        );
        assert!(
            root.workspace_files.documents.is_empty(),
            "late confirmation must not open documents"
        );
        assert_eq!(
            root.center_panel,
            CenterPanel::Terminal,
            "discarded confirmation must not switch the center panel"
        );
        assert_eq!(
            control.loads(),
            1,
            "discarded confirmation must not trigger another read"
        );
    });
}

/// 目录确认在途时用户切到其它标签，回执不得激活非前台工作区或抢占共享编辑器。
///
/// 回执若仍按旧逻辑应用，会清掉共享编辑器当前活动文档、为非活动 Tab 建 FilesPane
/// 并把中心页切成 Editor。切回原标签再次点击目录时必须重新确认并成功装配。
///
/// @author kongweiguang
#[gpui_kit::test]
fn inactive_tab_discards_directory_confirmation_without_touching_editor(app: &mut TestAppContext) {
    let mut directory_runtime = None;
    let mut control = None;
    let (runtime, root, visual) = fixture_with(app, |mut services| {
        let (hosts, host_control) = observed_host_service(
            FixtureHostLookup::Absent,
            &["kerminal.ssh"],
            Some(Arc::new(Notify::new())),
        );
        control = Some(host_control);
        services.hosts = Some(hosts);
        let (directory_rt, files, transfers, _, _) =
            crate::features::files::action_test_support::services();
        directory_runtime = Some(directory_rt);
        services.files = Some(files);
        services.transfers = Some(transfers);
        services
    });
    let control = control.expect("host read control");
    let _directory_runtime = directory_runtime.expect("directory service runtime");
    control.set_blocked(false);
    let owner = open(&root, visual, remote("active-tab-host", "/srv/a.txt"));
    settle(&runtime, visual);
    let other = open(&root, visual, remote("other-tab-host", "/srv/b.txt"));
    settle(&runtime, visual);

    // 让目标 Tab 重新成为已提交的活动标签，模拟用户停留在文件目录页。
    activate_committed_tab(&root, visual, &owner);
    let directory = root.read_with(visual, |root, _| {
        owner_state(root, &owner)
            .items
            .iter()
            .find(|item| matches!(item, FileWorkspaceItem::Directory { .. }))
            .expect("directory item")
            .id()
            .clone()
    });

    // 挂起目录确认，再切到另一个已提交 Tab。
    let loads_before_confirmation = control.loads();
    control.set_blocked(true);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.activate_host_file_item(&owner, &directory, window, cx);
            root.activate_host_file_workspace(&owner, window, cx);
        });
    });
    settle(&runtime, visual);
    // 只用“没有 pane”无法区分“根本未发起确认”和“确认被正确丢弃”；权威读取已计数
    // 说明请求确实发出，而阻塞门闩保证它在切换 Tab 时仍在途。
    assert!(
        control.loads() > loads_before_confirmation,
        "the directory activation must issue an authoritative host read before it can be discarded"
    );
    root.read_with(visual, |root, _| {
        assert!(root.workspace_files.directories.is_empty());
    });

    activate_committed_tab(&root, visual, &other);
    let active_doc_before = root.read_with(visual, |root, cx| {
        root.file_editor_workspace
            .as_ref()
            .expect("editor")
            .read(cx)
            .workspace_projection()
            .active_endpoint
    });
    // 原断言取释放前的中心页再比较，旧缺陷若在释放后把中心页切成 Editor 但仍与
    // 释放前相等就会漏检；这里显式置于文件页，使“迟到的目录回执抢占中心页”能被
    // 后续的唯一断言独立发现。
    root.update(visual, |root, _| {
        root.center_panel = CenterPanel::Files;
    });

    control.release();
    settle(&runtime, visual);

    root.read_with(visual, |root, cx| {
        assert!(
            !root
                .workspace_files
                .directories
                .contains_key(&(owner.clone(), directory.clone())),
            "confirmation for an inactive tab must not create a pane"
        );
        assert_eq!(
            root.center_panel,
            CenterPanel::Files,
            "discarded directory confirmation must not steal the center panel"
        );
        let active_doc_after = root
            .file_editor_workspace
            .as_ref()
            .expect("editor")
            .read(cx)
            .workspace_projection()
            .active_endpoint;
        assert_eq!(
            active_doc_after, active_doc_before,
            "discarded directory confirmation must not clear the visible document"
        );
        assert!(matches!(
            root.workspace_files.restore_state(&owner),
            Some(WorkspaceFileRestoreState::Ready)
        ));
        assert_eq!(
            root.workspace_controller
                .as_ref()
                .expect("controller")
                .snapshot()
                .active_tab_id
                .as_ref(),
            Some(&other)
        );
    });

    // 切回原标签再点目录：这是一次新的权威确认，成功后应正常建 pane。
    activate_committed_tab(&root, visual, &owner);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.activate_host_file_item(&owner, &directory, window, cx);
            root.activate_host_file_workspace(&owner, window, cx);
        });
    });
    settle(&runtime, visual);
    root.read_with(visual, |root, _| {
        assert!(
            root.workspace_files
                .directories
                .contains_key(&(owner.clone(), directory.clone())),
            "re-activating the owner must reconfirm and assemble the directory: {:?}",
            root.status
        );
    });
}

/// 读取失败没有权威结论；内部 helper 不能回退到快照门禁而放行远端目录。
///
/// @author kongweiguang
#[gpui_kit::test]
fn failed_authoritative_lookup_never_falls_back_to_snapshot(app: &mut TestAppContext) {
    use super::restore::{FileWorkspaceTargetGate, HostProtocolLookup};

    let (_runtime, root, visual) = fixture(app);
    let target = WorkspaceTargetRef::Ssh {
        host_id: HostId::parse("gate-host").expect("fixture host"),
    };
    root.update(visual, |root, cx| {
        assert_eq!(
            root.file_workspace_gate_with_protocol(&target, Some(HostProtocolLookup::Failed), cx),
            FileWorkspaceTargetGate::ProtocolUnconfirmed
        );
    });
}

/// 容器能力禁用的展示文案必须按界面语言选择，不能在英文界面显示静态中文。
///
/// 该测试直接调用纯辅助而不改全局 locale，避免影响并行用例。
///
/// @author kongweiguang
#[test]
fn container_capability_disabled_message_is_localized() {
    use super::restore::container_capability_disabled_message;
    use crate::locale::UiLanguage;

    let zh = container_capability_disabled_message(UiLanguage::ZhCn);
    assert!(zh.contains("容器能力已禁用"), "unexpected zh text: {zh}");

    let en = container_capability_disabled_message(UiLanguage::EnUs);
    assert!(en.contains("disabled"), "unexpected en text: {en}");
    assert!(
        !en.chars().any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch)),
        "English text must not contain CJK characters: {en}"
    );
}
