//! 右栏 SFTP 目录数据的局部入场与目标身份回归。
//!
//! @author kongweiguang

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use async_trait::async_trait;
use gpui_kit::{AppContext as _, Entity, TestAppContext};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor, files::FileAccessProvider,
    sessions::UiSessionBinding,
};
use kerminal_domain::{
    files::{
        DeleteTargetKind, DirectoryListing, FileEndpoint, FileEntry, FilePreview, TextDocument,
        WriteTextRequest, WriteTextResult,
    },
    identity::{HostId, PaneId, SessionId, TabId},
    sessions::SessionOwner,
};
use tokio::{runtime::Runtime, sync::Notify};
use tokio_util::sync::CancellationToken;

use super::{FilesPane, action_test_support as support};

impl FilesPane {
    /// Root 可见性回归只读取真实 activity，不暴露可变状态或新增生产入口。
    /// 方法放在外置测试模块，`src` 不再保留测试专属方法。
    ///
    /// @author kongweiguang
    pub(crate) fn is_active_for_test(&self) -> bool {
        self.active
    }

    /// 真实右栏回归需要确定性的“系统允许动效”前提，避免测试依赖运行机器的桌面设置。
    /// 注入方法放在外置测试模块，`src` 只保留常驻字段与 `cfg!(test)` 读取分支。
    ///
    /// @author kongweiguang
    fn set_motion_allowed_for_test(&mut self, allowed: bool) {
        self.motion_allowed_for_test = Some(allowed);
    }
}

/// 可门控的目录读取 provider：指定端点挂起直到显式放行，让手动刷新真正在途。
struct GatedListingProvider {
    gated_endpoint: FileEndpoint,
    listing: MutexOptionListing,
    gate: Arc<Notify>,
    started: AtomicUsize,
}

type MutexOptionListing = std::sync::Mutex<Option<DirectoryListing>>;

impl GatedListingProvider {
    /// 以目标端点与放行闸门构造 provider；读取命中门控端点时挂起等待放行。
    fn new(
        gated_endpoint: FileEndpoint,
        listing: Option<DirectoryListing>,
    ) -> (Arc<Self>, Arc<Notify>) {
        let gate = Arc::new(Notify::new());
        (
            Arc::new(Self {
                gated_endpoint,
                listing: std::sync::Mutex::new(listing),
                gate: gate.clone(),
                started: AtomicUsize::new(0),
            }),
            gate,
        )
    }

    /// 读取启动次数，断言跳过重读时不产生新 provider 调用。
    fn start_count(&self) -> usize {
        self.started.load(Ordering::Acquire)
    }
}

#[async_trait]
impl FileAccessProvider for GatedListingProvider {
    /// 接受全部端点，保证 FilesPane 构造与初始化不被拒。
    fn supports(&self, _: &FileEndpoint) -> bool {
        true
    }

    /// 命中门控端点时挂起到放行；其它端点无内容直接失败，不产生真实 IO。
    async fn list_directory(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
    ) -> AppResult<DirectoryListing> {
        self.started.fetch_add(1, Ordering::AcqRel);
        if endpoint != self.gated_endpoint {
            return Err(AppError::unsupported("gated provider: other endpoint"));
        }
        tokio::select! {
            _ = self.gate.notified() => {}
            _ = cancellation.cancelled() => {
                return Err(AppError::cancelled("gated provider cancelled"));
            }
        }
        self.listing
            .lock()
            .expect("listing mutex")
            .clone()
            .ok_or_else(|| AppError::unsupported("gated provider: no listing"))
    }

    /// 测试不读取预览数据。
    async fn preview_file(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: usize,
    ) -> AppResult<FilePreview> {
        Err(AppError::unsupported("gated provider: no preview"))
    }

    /// 测试不读取可编辑文本。
    async fn read_text_file(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: usize,
    ) -> AppResult<TextDocument> {
        Err(AppError::unsupported("gated provider: no text"))
    }

    /// 测试不提交文本写入。
    async fn write_text_file(
        &self,
        _: CancellationToken,
        _: WriteTextRequest,
    ) -> AppResult<WriteTextResult> {
        Err(AppError::unsupported("gated provider: no write"))
    }

    /// 测试不创建目录。
    async fn create_directory(&self, _: CancellationToken, _: FileEndpoint) -> AppResult<()> {
        Err(AppError::unsupported("gated provider: no create"))
    }

    /// 测试不重命名。
    async fn rename(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: FileEndpoint,
    ) -> AppResult<()> {
        Err(AppError::unsupported("gated provider: no rename"))
    }

    /// 测试不改权限。
    async fn chmod(&self, _: CancellationToken, _: FileEndpoint, _: u32) -> AppResult<()> {
        Err(AppError::unsupported("gated provider: no chmod"))
    }

    /// 测试不删除。
    async fn delete(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: DeleteTargetKind,
    ) -> AppResult<()> {
        Err(AppError::unsupported("gated provider: no delete"))
    }
}

/// 延迟 provider 专用书签仓储，保持 FileService 装配约束且不产生持久副作用。
struct NoBookmarks;

#[async_trait]
impl kerminal_application::files::FileBookmarkRepository for NoBookmarks {
    /// 测试不读取书签。
    async fn list(
        &self,
        _: CancellationToken,
        _: kerminal_domain::files::BookmarkTarget,
    ) -> AppResult<Vec<kerminal_domain::files::FileBookmark>> {
        Ok(Vec::new())
    }

    /// 测试不写入书签。
    async fn set(
        &self,
        _: CancellationToken,
        _: kerminal_domain::files::FileBookmark,
        _: bool,
    ) -> AppResult<Vec<kerminal_domain::files::FileBookmark>> {
        Err(AppError::unsupported("gated provider: no bookmarks"))
    }
}

/// 延迟 provider 专用传输后端，声明不可执行以防 UI 生命周期测试误触队列。
struct NoTransfers;

#[async_trait]
impl kerminal_application::transfers::TransferBackend for NoTransfers {
    /// 测试不入队传输任务。
    fn supports(&self, _: &kerminal_domain::files::TransferRequest) -> bool {
        false
    }

    /// 任何意外调用都 panic，测试应先在可观察 UI 状态中发现它。
    async fn execute(
        &self,
        _: kerminal_application::transfers::TransferExecutionContext,
    ) -> Result<
        kerminal_application::transfers::TransferExecutionResult,
        kerminal_application::transfers::TransferExecutionError,
    > {
        panic!("gated entrance fixture must not execute transfers")
    }
}

/// 用门控 provider 组装真实 FilesPane：先建立内存 listing 基线，再由用例控制读取节奏。
///
/// @author kongweiguang
fn gated_pane_fixture(
    app: &mut TestAppContext,
    gated_endpoint: FileEndpoint,
    listing: Option<DirectoryListing>,
) -> (
    Runtime,
    Entity<FilesPane>,
    Arc<GatedListingProvider>,
    Arc<Notify>,
) {
    app.update(gpui_kit::init);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let (provider, gate) = GatedListingProvider::new(gated_endpoint.clone(), listing);
    let files = kerminal_application::files::FileService::new(
        vec![provider.clone()],
        Arc::new(NoBookmarks),
    )
    .expect("gated file service");
    let transfers = kerminal_application::transfers::TransferService::new(
        vec![Arc::new(NoTransfers)],
        runtime.handle().clone(),
        kerminal_application::transfers::TransferQueueOptions::default(),
    )
    .expect("transfer service");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let pane = app.update(|cx| {
        cx.new(|_| {
            let mut pane = FilesPane::new(
                files,
                transfers,
                executor,
                crate::features::files::FileTargetNameLookup::default(),
                gated_endpoint.clone(),
            );
            pane.initialized = true;
            pane
        })
    });
    (runtime, pane, provider, gate)
}

/// 绑定样本只需稳定身份；目标动画判断不使用会话名称，因此每秒可独立构造。
///
/// @author kongweiguang
fn binding() -> UiSessionBinding {
    UiSessionBinding {
        session_id: SessionId::new(),
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
        session_generation: 1,
    }
}

/// 同一 Tab 下的不同分屏绑定：复用 TabId，只改变 pane 与 session 身份，
/// 避免把“标签切换”误当成真实的同 Tab 分屏切换。
///
/// @author kongweiguang
fn pane_binding(tab_id: &TabId, session: &str, pane: &str) -> UiSessionBinding {
    UiSessionBinding {
        session_id: SessionId::parse(session).expect("valid session identity"),
        owner: SessionOwner::Ui {
            tab_id: tab_id.clone(),
            pane_id: PaneId::parse(pane).expect("valid pane identity"),
        },
        session_generation: 1,
    }
}

/// 分屏测试的夹具前置条件：共享 TabId、不同 PaneId 和 SessionId 才符合生产分屏切换。
///
/// @author kongweiguang
fn assert_same_tab_different_panes(a: &UiSessionBinding, b: &UiSessionBinding) {
    let (
        SessionOwner::Ui {
            tab_id: tab_a,
            pane_id: pane_a,
        },
        SessionOwner::Ui {
            tab_id: tab_b,
            pane_id: pane_b,
        },
    ) = (&a.owner, &b.owner)
    else {
        panic!("pane switch fixtures must be UI-owned sessions");
    };
    assert_eq!(tab_a, tab_b, "pane switch must stay inside one Tab");
    assert_ne!(pane_a, pane_b, "pane switch must target different panes");
    assert_ne!(
        a.session_id, b.session_id,
        "different panes must carry different terminal sessions"
    );
}

/// 只把内存 listing 交给真实 FilesPane 读取链，不引入磁盘、网络或 SFTP 租约。
///
/// @author kongweiguang
fn seed_listing(
    fixture: &support::FilesPaneFixture,
    endpoint: &FileEndpoint,
    entries: Vec<FileEntry>,
) {
    *fixture
        .mutations
        .directory_listing
        .lock()
        .expect("listing mutex") = Some(DirectoryListing {
        directory: endpoint.clone(),
        entries,
    });
}

/// 与生产同一条异步完成链：交替推进 GPUI 和独立 Tokio runtime，直到 listing 发布。
///
/// @author kongweiguang
fn wait_for_listing(
    app: &mut TestAppContext,
    runtime: &Runtime,
    pane: &Entity<FilesPane>,
    endpoint: &FileEndpoint,
) {
    for _ in 0..4_000 {
        app.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
        let loaded = pane.read_with(app, |pane, _| {
            pane.panels[1]
                .listing
                .as_ref()
                .is_some_and(|listing| &listing.directory == endpoint)
                && !pane.panels[1].loading
        });
        if loaded {
            return;
        }
    }
    panic!("directory listing was not published: {endpoint:?}");
}

/// 失败必须停在真实错误态，便于断言等待中的入场没有被失败结果消费。
///
/// @author kongweiguang
fn wait_for_error(app: &mut TestAppContext, runtime: &Runtime, pane: &Entity<FilesPane>) {
    for _ in 0..4_000 {
        app.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
        let failed = pane.read_with(app, |pane, _| {
            !pane.panels[1].loading && pane.panels[1].directory_error.is_some()
        });
        if failed {
            return;
        }
    }
    panic!("directory error was not published");
}

/// 首次读取只建立基线；真正切换宿主后的首个成功结果消费一次入场，普通刷新和同主机跟随不重放。
/// 系统减少动效是否允许播放由既有 `TargetDataEntrance` 契约覆盖，本测试不绑定本机视觉偏好。
///
/// @author kongweiguang
#[gpui_kit::test]
fn new_host_first_success_consumes_entrance_without_refresh_or_follow_replay(
    app: &mut TestAppContext,
) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let fixture = support::entity_fixture(app, local, |_| {});
    let runtime = &fixture.runtime;
    let pane = fixture.pane.clone();
    let host_a = HostId::new();
    let host_b = HostId::new();
    let a = support::remote_endpoint(&host_a, "/a");
    let b = support::remote_endpoint(&host_b, "/b");
    let b_other = support::remote_endpoint(&host_b, "/b/other");
    let b_follow = support::remote_endpoint(&host_b, "/b/follow");
    let bind = binding();

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_compact_layout(true, cx);
            pane.set_active(true, cx);
        })
    });

    seed_listing(&fixture, &a, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(a.clone()), None, Some(bind.clone()), cx);
        })
    });
    wait_for_listing(app, runtime, &pane, &a);
    assert_eq!(
        fixture
            .mutations
            .list_directory
            .load(std::sync::atomic::Ordering::Acquire),
        1,
        "establishing the baseline must perform exactly one directory read"
    );
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(!pane.data_entrance.awaiting_result());
            assert_eq!(
                pane.data_entrance.opacity(Instant::now()),
                1.0,
                "initial target establishment must not animate"
            );
        })
    });

    seed_listing(&fixture, &b, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(b.clone()), None, Some(bind.clone()), cx);
            assert!(
                pane.data_entrance.awaiting_result(),
                "a new SSH host must arm exactly one entrance before its first result"
            );
        })
    });
    wait_for_listing(app, runtime, &pane, &b);
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(
                !pane.data_entrance.awaiting_result(),
                "a new SSH host's first success must consume the entrance"
            );
        })
    });

    // 同一目录普通刷新只替换结果，不重新武装或重播入场。
    app.update(|cx| {
        pane.update(cx, |pane, cx| pane.load_panel(1, cx));
    });
    wait_for_listing(app, runtime, &pane, &b);
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(!pane.data_entrance.awaiting_result());
        })
    });

    // 同一 pane 上报新 cwd 但关闭目录跟随时保留手动路径，不重读也不触发入场。
    let reads_before_preserve = fixture
        .mutations
        .list_directory
        .load(std::sync::atomic::Ordering::Acquire);
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(b_other), None, Some(bind.clone()), cx);
        })
    });
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(!pane.data_entrance.awaiting_result());
        })
    });
    assert_eq!(
        fixture
            .mutations
            .list_directory
            .load(std::sync::atomic::Ordering::Acquire),
        reads_before_preserve,
        "preserving a manual path must not issue another directory read"
    );

    // 显式跟随目录会读取同一 pane 的新 cwd，但 binding 未变，仍不重播。
    seed_listing(&fixture, &b_follow, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_follow_terminal_directory(true, cx);
            pane.set_current_terminal_target(Some(b_follow.clone()), None, Some(bind), cx);
        })
    });
    wait_for_listing(app, runtime, &pane, &b_follow);
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(!pane.data_entrance.awaiting_result());
        })
    });
}

/// 同一 SSH 宿主内切换分屏时，binding 与终端目录同时变化才会登记一次新目录入场；
/// 切到同一 cwd 的另一个分屏仍不播放，避免把仅身份变化误当成数据变化。
///
/// @author kongweiguang
#[gpui_kit::test]
fn pane_switch_changed_directory_arms_once_without_same_path_replay(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let fixture = support::entity_fixture(app, local, |_| {});
    let runtime = &fixture.runtime;
    let pane = fixture.pane.clone();
    let host = HostId::new();
    let a = support::remote_endpoint(&host, "/a");
    let b = support::remote_endpoint(&host, "/b");
    let tab_id = TabId::new();
    let binding_a = pane_binding(&tab_id, "session-pane-a", "pane-a");
    let binding_b = pane_binding(&tab_id, "session-pane-b", "pane-b");
    assert_same_tab_different_panes(&binding_a, &binding_b);

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_compact_layout(true, cx);
            pane.set_active(true, cx);
        })
    });
    seed_listing(&fixture, &a, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(a.clone()), None, Some(binding_a.clone()), cx);
        })
    });
    wait_for_listing(app, runtime, &pane, &a);
    app.update(|cx| pane.update(cx, |pane, cx| pane.set_follow_terminal_directory(true, cx)));

    // 切到同宿主不同 cwd 的另一个分屏：真实发起新目录读取，登记并消费一次入场。
    seed_listing(&fixture, &b, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(b.clone()), None, Some(binding_b.clone()), cx);
            assert!(
                pane.data_entrance.awaiting_result(),
                "a pane switch with a different cwd must arm one entrance"
            );
        })
    });
    wait_for_listing(app, runtime, &pane, &b);
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(
                !pane.data_entrance.awaiting_result(),
                "the new pane directory's first success must consume the entrance"
            );
        })
    });

    // 切回原分屏也是真实目录变化，重新登记一次；迟到的旧结果不能借用新代次。
    seed_listing(&fixture, &a, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(a.clone()), None, Some(binding_a), cx);
            assert!(pane.data_entrance.awaiting_result());
        })
    });
    wait_for_listing(app, runtime, &pane, &a);
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(!pane.data_entrance.awaiting_result());
        })
    });

    // 切到同一 cwd 的另一个分屏只改变身份，目录数据未变，不登记入场。
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(a.clone()), None, Some(binding_b), cx);
            assert!(
                !pane.data_entrance.awaiting_result(),
                "a pane switch at the same cwd must not replay the data entrance"
            );
        })
    });
    wait_for_listing(app, runtime, &pane, &a);
}

/// 分屏切换登记入场后，同一 pane 的普通 cwd 跟随会撤销这份等待；
/// 否则普通跟随结果会借用旧目标代次播放，违反“普通跟随不动画”的边界。
///
/// @author kongweiguang
#[gpui_kit::test]
fn same_pane_follow_cancels_pending_pane_switch_entrance(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let fixture = support::entity_fixture(app, local, |_| {});
    let runtime = &fixture.runtime;
    let pane = fixture.pane.clone();
    let host = HostId::new();
    let a = support::remote_endpoint(&host, "/a");
    let b = support::remote_endpoint(&host, "/b");
    let c = support::remote_endpoint(&host, "/c");
    let tab_id = TabId::new();
    let binding_a = pane_binding(&tab_id, "session-follow-a", "pane-follow-a");
    let binding_b = pane_binding(&tab_id, "session-follow-b", "pane-follow-b");
    assert_same_tab_different_panes(&binding_a, &binding_b);

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_compact_layout(true, cx);
            pane.set_active(true, cx);
        })
    });
    seed_listing(&fixture, &a, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(a.clone()), None, Some(binding_a), cx);
        })
    });
    wait_for_listing(app, runtime, &pane, &a);
    app.update(|cx| pane.update(cx, |pane, cx| pane.set_follow_terminal_directory(true, cx)));

    // 切到另一个分屏并进入等待，但先不推进它的读取。
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(b.clone()), None, Some(binding_b.clone()), cx);
            assert!(pane.data_entrance.awaiting_result());
        })
    });

    // 同一分屏的 cwd 跟随是普通读取，必须撤销上一分屏留下的等待。
    seed_listing(&fixture, &c, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(c.clone()), None, Some(binding_b), cx);
            assert!(
                !pane.data_entrance.awaiting_result(),
                "a normal same-pane cwd follow must cancel the pending pane-switch entrance"
            );
        })
    });
    wait_for_listing(app, runtime, &pane, &c);
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(!pane.data_entrance.awaiting_result());
            assert_eq!(pane.data_entrance.opacity(Instant::now()), 1.0);
        })
    });
}

/// 隐藏期间完成的当前目标结果静默消费入场；重显后的普通刷新不能补播。
///
/// @author kongweiguang
#[gpui_kit::test]
fn hidden_success_consumes_entrance_without_replay_on_reshow(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let fixture = support::entity_fixture(app, local, |_| {});
    let runtime = &fixture.runtime;
    let pane = fixture.pane.clone();
    let host_a = HostId::new();
    let host_b = HostId::new();
    let a = support::remote_endpoint(&host_a, "/a");
    let b = support::remote_endpoint(&host_b, "/b");
    let bind = binding();

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_compact_layout(true, cx);
            pane.set_active(true, cx);
        })
    });
    seed_listing(&fixture, &a, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(a.clone()), None, Some(bind.clone()), cx);
        })
    });
    wait_for_listing(app, runtime, &pane, &a);

    app.update(|cx| pane.update(cx, |pane, cx| pane.set_active(false, cx)));
    seed_listing(&fixture, &b, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(b.clone()), None, Some(bind), cx);
            assert!(
                pane.data_entrance.awaiting_result(),
                "hidden target change still keeps the entrance waiting for its own result"
            );
        })
    });
    wait_for_listing(app, runtime, &pane, &b);
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(!pane.active);
            assert!(!pane.data_entrance.awaiting_result());
            assert_eq!(
                pane.data_entrance.opacity(Instant::now()),
                1.0,
                "hidden success must consume the entrance without starting a frame loop"
            );
        })
    });

    app.update(|cx| pane.update(cx, |pane, cx| pane.set_active(true, cx)));
    app.update(|cx| pane.update(cx, |pane, cx| pane.load_panel(1, cx)));
    wait_for_listing(app, runtime, &pane, &b);
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(pane.active);
            assert_eq!(
                pane.data_entrance.opacity(Instant::now()),
                1.0,
                "reshow refresh must not replay data already consumed while hidden"
            );
        })
    });
}

/// 失败不消费等待；只有重试后的真实成功才开始入场，并保持同一目标身份。
///
/// @author kongweiguang
#[gpui_kit::test]
fn failure_keeps_entrance_armed_for_retry_success(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let fixture = support::entity_fixture(app, local, |_| {});
    let runtime = &fixture.runtime;
    let pane = fixture.pane.clone();
    let host_a = HostId::new();
    let host_b = HostId::new();
    let a = support::remote_endpoint(&host_a, "/a");
    let b = support::remote_endpoint(&host_b, "/b");
    let bind = binding();

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_compact_layout(true, cx);
            pane.set_active(true, cx);
        })
    });
    seed_listing(&fixture, &a, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(a.clone()), None, Some(bind.clone()), cx);
        })
    });
    wait_for_listing(app, runtime, &pane, &a);

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(b.clone()), None, Some(bind), cx);
            assert!(pane.data_entrance.awaiting_result());
        })
    });
    wait_for_error(app, runtime, &pane);
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(
                pane.data_entrance.awaiting_result(),
                "a failed read must not consume the target's entrance"
            );
            assert_eq!(pane.data_entrance.opacity(Instant::now()), 1.0);
        })
    });

    seed_listing(&fixture, &b, Vec::new());
    app.update(|cx| pane.update(cx, |pane, cx| pane.retry_panel_load(1, cx)));
    wait_for_listing(app, runtime, &pane, &b);
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(
                !pane.data_entrance.awaiting_result(),
                "retry success must consume the still-current target entrance"
            );
        })
    });
}

/// 旧代次结果和无目标状态都不能借用新目标的入场机会。
///
/// @author kongweiguang
#[gpui_kit::test]
fn stale_epoch_and_unavailable_target_do_not_start_entrance(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let fixture = support::entity_fixture(app, local, |_| {});
    let runtime = &fixture.runtime;
    let pane = fixture.pane.clone();
    let host_a = HostId::new();
    let host_b = HostId::new();
    let a = support::remote_endpoint(&host_a, "/a");
    let b = support::remote_endpoint(&host_b, "/b");
    let bind = binding();

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_compact_layout(true, cx);
            pane.set_active(true, cx);
        })
    });
    seed_listing(&fixture, &a, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(a.clone()), None, Some(bind.clone()), cx);
        })
    });
    wait_for_listing(app, runtime, &pane, &a);

    let stale_epoch = app.update(|cx| pane.update(cx, |pane, _| pane.data_entrance.epoch()));
    seed_listing(&fixture, &b, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(b.clone()), None, Some(bind), cx);
            assert!(pane.data_entrance.awaiting_result());
        })
    });
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(!pane.data_entrance.ready(stale_epoch, Instant::now(), true));
            assert!(pane.data_entrance.awaiting_result());
            assert_eq!(pane.data_entrance.opacity(Instant::now()), 1.0);
        })
    });

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(None, None, None, cx);
        })
    });
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(!pane.data_entrance.awaiting_result());
            assert_eq!(pane.data_entrance.opacity(Instant::now()), 1.0);
        })
    });
}

/// 分屏切到右栏已显示的同一目录时不重读也不入场：数据没有变化，重复读取
/// 和一次无意义的淡化都违背“只有真实新目录数据才播放”的契约。
///
/// @author kongweiguang
#[gpui_kit::test]
fn pane_switch_to_displayed_directory_skips_reread_and_entrance(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let fixture = support::entity_fixture(app, local, |_| {});
    let runtime = &fixture.runtime;
    let pane = fixture.pane.clone();
    let host = HostId::new();
    let manual = support::remote_endpoint(&host, "/manual");
    let tab_id = TabId::new();
    let binding_a = pane_binding(&tab_id, "session-display-a", "pane-display-a");
    let binding_b = pane_binding(&tab_id, "session-display-b", "pane-display-b");
    assert_same_tab_different_panes(&binding_a, &binding_b);

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_compact_layout(true, cx);
            pane.set_active(true, cx);
        })
    });
    seed_listing(&fixture, &manual, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(manual.clone()), None, Some(binding_a), cx);
        })
    });
    wait_for_listing(app, runtime, &pane, &manual);
    let reads_after_display = fixture
        .mutations
        .list_directory
        .load(std::sync::atomic::Ordering::Acquire);

    // 另一个 pane 的 cwd 与右栏已显示目录一致：身份切换成立，但数据未变。
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_follow_terminal_directory(true, cx);
            pane.set_current_terminal_target(Some(manual.clone()), None, Some(binding_b), cx);
            assert!(
                !pane.data_entrance.awaiting_result(),
                "switching to an already displayed directory must not arm an entrance"
            );
            assert_eq!(pane.data_entrance.opacity(Instant::now()), 1.0);
        })
    });
    assert_eq!(
        fixture
            .mutations
            .list_directory
            .load(std::sync::atomic::Ordering::Acquire),
        reads_after_display,
        "the displayed directory must not be read again on a same-path pane switch"
    );
    assert_eq!(
        pane.read_with(app, |pane, _| pane.panels[1].endpoint.clone()),
        manual
    );
}

/// 手动下钻不消费分屏切换留下的等待：武装只属于它自己那次目标读取，
/// 其它读取要么等价延续，要么立即撤销，任何无关成功结果都不能开始动画。
///
/// @author kongweiguang
#[gpui_kit::test]
fn manual_drill_cancels_armed_entrance_before_its_own_success(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let fixture = support::entity_fixture(app, local, |_| {});
    let runtime = &fixture.runtime;
    let pane = fixture.pane.clone();
    let host = HostId::new();
    let a = support::remote_endpoint(&host, "/a");
    let b = support::remote_endpoint(&host, "/b");
    let manual = support::remote_endpoint(&host, "/manual");
    let tab_id = TabId::new();
    let binding_a = pane_binding(&tab_id, "session-drill-a", "pane-drill-a");
    let binding_b = pane_binding(&tab_id, "session-drill-b", "pane-drill-b");
    assert_same_tab_different_panes(&binding_a, &binding_b);

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_compact_layout(true, cx);
            pane.set_active(true, cx);
            pane.set_motion_allowed_for_test(true);
        })
    });
    seed_listing(&fixture, &a, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(a.clone()), None, Some(binding_a), cx);
        })
    });
    wait_for_listing(app, runtime, &pane, &a);
    app.update(|cx| pane.update(cx, |pane, cx| pane.set_follow_terminal_directory(true, cx)));

    // 切到 B 的 cwd 并武装，但先不推进它的读取；用户此时手动下钻到别的目录。
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(b.clone()), None, Some(binding_b.clone()), cx);
            assert!(pane.data_entrance.awaiting_result());
        })
    });
    seed_listing(&fixture, &manual, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.open_endpoint_on_side(1, manual.clone(), cx);
            assert!(
                !pane.data_entrance.awaiting_result(),
                "a manual drill must disarm the pending pane-switch entrance"
            );
        })
    });
    wait_for_listing(app, runtime, &pane, &manual);
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert_eq!(
                pane.data_entrance.opacity(Instant::now()),
                1.0,
                "the manual drill's own success must not animate"
            );
        })
    });

    // 之后再手动导航回原分屏目标 B：这只是普通读取，成功也不得开始动画。
    seed_listing(&fixture, &b, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.open_endpoint_on_side(1, b.clone(), cx);
        })
    });
    wait_for_listing(app, runtime, &pane, &b);
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(!pane.data_entrance.awaiting_result());
            assert_eq!(pane.data_entrance.opacity(Instant::now()), 1.0);
        })
    });
}

/// 分屏切换武装的读取失败后，关闭跟随必须立即撤销等待：取消资格已被失败回执清空，
/// 取消函数不能因此跳过 disarm，让之后的普通刷新借用旧目标武装补播。
///
/// @author kongweiguang
#[gpui_kit::test]
fn follow_failure_then_disable_disarms_before_plain_refresh(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let fixture = support::entity_fixture(app, local, |_| {});
    let runtime = &fixture.runtime;
    let pane = fixture.pane.clone();
    let host = HostId::new();
    let a = support::remote_endpoint(&host, "/a");
    let b = support::remote_endpoint(&host, "/b");
    let tab_id = TabId::new();
    let binding_a = pane_binding(&tab_id, "session-fail-a", "pane-fail-a");
    let binding_b = pane_binding(&tab_id, "session-fail-b", "pane-fail-b");
    assert_same_tab_different_panes(&binding_a, &binding_b);

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_compact_layout(true, cx);
            pane.set_active(true, cx);
            pane.set_motion_allowed_for_test(true);
        })
    });
    seed_listing(&fixture, &a, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(a.clone()), None, Some(binding_a), cx);
        })
    });
    wait_for_listing(app, runtime, &pane, &a);
    app.update(|cx| pane.update(cx, |pane, cx| pane.set_follow_terminal_directory(true, cx)));

    // 分屏切换武装，读取真实失败；失败回执会清空跟随取消资格。
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(b.clone()), None, Some(binding_b), cx);
            assert!(pane.data_entrance.awaiting_result());
        })
    });
    wait_for_error(app, runtime, &pane);
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(
                pane.data_entrance.awaiting_result(),
                "a failed follow read must not consume the entrance"
            );
            assert!(pane.terminal_directory_follow_generation.is_none());
        })
    });

    // 关闭跟随走不到取消分支，但等待必须仍然被撤销。
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_follow_terminal_directory(false, cx);
            assert!(
                !pane.data_entrance.awaiting_result(),
                "disabling follow must disarm even after the request receipt already cleared it"
            );
        })
    });

    // 之后的普通刷新成功只能呈现数据，不得播放入场。
    seed_listing(&fixture, &b, Vec::new());
    app.update(|cx| pane.update(cx, |pane, cx| pane.retry_panel_load(1, cx)));
    wait_for_listing(app, runtime, &pane, &b);
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert_eq!(
                pane.data_entrance.opacity(Instant::now()),
                1.0,
                "a plain refresh after disarm must not replay the entrance"
            );
        })
    });
}

/// 树形视图与列表共用同一数据入场：目标切换的首次成功在树模式同样收敛透明度，
/// 而不是动画照跑、屏幕无变化的空转帧。
///
/// @author kongweiguang
#[gpui_kit::test]
fn tree_mode_entrance_fades_data_layer_too(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let fixture = support::entity_fixture(app, local, |_| {});
    let runtime = &fixture.runtime;
    let pane = fixture.pane.clone();
    let host_a = HostId::new();
    let host_b = HostId::new();
    let a = support::remote_endpoint(&host_a, "/a");
    let b = support::remote_endpoint(&host_b, "/b");
    let bind = binding();

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_compact_layout(true, cx);
            pane.set_active(true, cx);
            pane.set_motion_allowed_for_test(true);
        })
    });
    seed_listing(&fixture, &a, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(a.clone()), None, Some(bind.clone()), cx);
        })
    });
    wait_for_listing(app, runtime, &pane, &a);
    app.update(|cx| pane.update(cx, |pane, cx| pane.toggle_tree_view(cx)));

    // 切换宿主并武装；树模式的首次成功同样消费入场并真正启动一次动画，
    // 而不是动画照跑、屏幕无变化的空转帧。
    seed_listing(&fixture, &b, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(b.clone()), None, Some(bind), cx);
            assert!(pane.data_entrance.awaiting_result());
        })
    });
    wait_for_listing(app, runtime, &pane, &b);
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(
                pane.started_entrances_for_test == 1,
                "tree mode must play the same entrance as the list mode, got {}",
                pane.started_entrances_for_test
            );
        })
    });
}

/// 门控 provider 的有界放行推进：交替泵 GPUI 与 Tokio，直到谓词成立或超时。
///
/// @author kongweiguang
fn pump_until(
    app: &mut TestAppContext,
    runtime: &Runtime,
    pane: &Entity<FilesPane>,
    until: impl Fn(&FilesPane) -> bool,
    what: &str,
) {
    for _ in 0..4_000 {
        app.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
        if pane.read_with(app, |pane, _| until(pane)) {
            return;
        }
    }
    panic!("timed out waiting for: {what}");
}

/// 已显示目录的手动刷新在途时，切到目标仍为该目录的另一 pane，再关闭跟随：
/// 关闭只取消自动跟随，绝不能取消用户手动发起的刷新；放行后手动请求正常完成。
/// 跟随取消资格只属于 follow_terminal_directory_to 发起的读取，不得凭空登记。
///
/// @author kongweiguang
#[gpui_kit::test]
fn disabling_follow_never_cancels_in_flight_manual_same_directory_refresh(
    app: &mut TestAppContext,
) {
    let host = HostId::new();
    let dir = support::remote_endpoint(&host, "/shared");
    let (runtime, pane, provider, gate) = gated_pane_fixture(
        app,
        dir.clone(),
        Some(DirectoryListing {
            directory: dir.clone(),
            entries: Vec::new(),
        }),
    );
    let tab_id = TabId::new();
    let binding_a = pane_binding(&tab_id, "session-manual-a", "pane-manual-a");
    let binding_b = pane_binding(&tab_id, "session-manual-b", "pane-manual-b");
    assert_same_tab_different_panes(&binding_a, &binding_b);

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_compact_layout(true, cx);
            pane.set_active(true, cx);
        })
    });
    // 首绑建立 /shared 基线；provider 放行后 listing 发布。
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(dir.clone()), None, Some(binding_a), cx);
        })
    });
    gate.notify_one();
    pump_until(
        app,
        &runtime,
        &pane,
        |pane| {
            pane.panels[1]
                .listing
                .as_ref()
                .is_some_and(|listing| listing.directory == dir)
                && !pane.panels[1].loading
        },
        "initial /shared listing",
    );
    let reads_after_baseline = provider.start_count();

    // 用户手动发起同目录刷新并让它真正在途（挂起在 provider）。
    app.update(|cx| pane.update(cx, |pane, cx| pane.load_panel(1, cx)));
    pump_until(
        app,
        &runtime,
        &pane,
        |pane| pane.panels[1].loading && pane.panels[1].requested_endpoint.as_ref() == Some(&dir),
        "manual refresh in flight",
    );
    assert_eq!(
        provider.start_count(),
        reads_after_baseline + 1,
        "the manual refresh must issue exactly one new read"
    );

    // 切到另一 pane，目标仍是已显示的 /shared：跳过重读，不产生新读取。
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_follow_terminal_directory(true, cx);
            pane.set_current_terminal_target(Some(dir.clone()), None, Some(binding_b), cx);
            assert!(
                !pane.data_entrance.awaiting_result(),
                "switching to the displayed directory must not arm an entrance"
            );
        })
    });
    assert_eq!(
        provider.start_count(),
        reads_after_baseline + 1,
        "the displayed directory must not be read again on a same-path pane switch"
    );

    // 关闭跟随不得取消用户的手动刷新：在途请求保持，token 未取消。
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_follow_terminal_directory(false, cx);
            assert!(
                pane.panels[1].loading,
                "disabling follow must not cancel the user's in-flight manual refresh"
            );
            assert_eq!(
                pane.panels[1].requested_endpoint.as_ref(),
                Some(&dir),
                "the manual refresh keeps its request identity"
            );
        })
    });

    // 放行后手动刷新真实完成，listing 发布；入场全程未武装，无动画。
    gate.notify_one();
    pump_until(
        app,
        &runtime,
        &pane,
        |pane| {
            !pane.panels[1].loading
                && pane.panels[1]
                    .listing
                    .as_ref()
                    .is_some_and(|listing| listing.directory == dir)
        },
        "manual refresh completion after follow disabled",
    );
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(!pane.data_entrance.awaiting_result());
            assert_eq!(pane.data_entrance.opacity(Instant::now()), 1.0);
            assert_eq!(provider.start_count(), reads_after_baseline + 1);
        })
    });
}

/// 同目录读取由跟随发起时，关闭跟随仍按原规则取消它：登记归属不被同目录
/// 手动分支破坏，关闭后的迟到回执不能改变目录或错误状态。
///
/// @author kongweiguang
#[gpui_kit::test]
fn disabling_follow_still_cancels_follow_initiated_same_directory_read(app: &mut TestAppContext) {
    let host = HostId::new();
    let a = support::remote_endpoint(&host, "/a");
    let b = support::remote_endpoint(&host, "/b");
    let (runtime, pane, provider, gate) = gated_pane_fixture(
        app,
        a.clone(),
        Some(DirectoryListing {
            directory: a.clone(),
            entries: Vec::new(),
        }),
    );
    let tab_id = TabId::new();
    let binding_a = pane_binding(&tab_id, "session-follow-a", "pane-follow-a");
    let binding_b = pane_binding(&tab_id, "session-follow-b", "pane-follow-b");
    assert_same_tab_different_panes(&binding_a, &binding_b);

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_compact_layout(true, cx);
            pane.set_active(true, cx);
        })
    });
    // 首绑 /a 基线（真实 BindTarget 读取，放行后完成）。
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(a.clone()), None, Some(binding_a), cx);
        })
    });
    gate.notify_one();
    pump_until(
        app,
        &runtime,
        &pane,
        |pane| {
            pane.panels[1]
                .listing
                .as_ref()
                .is_some_and(|listing| listing.directory == a)
                && !pane.panels[1].loading
        },
        "initial /a listing",
    );

    // 开启跟随（候选目录即已提交目录，不重读），切到 cwd=/b 的分屏并让跟随读取在途。
    app.update(|cx| pane.update(cx, |pane, cx| pane.set_follow_terminal_directory(true, cx)));
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(b.clone()), None, Some(binding_b), cx);
            assert!(pane.data_entrance.awaiting_result());
        })
    });
    pump_until(
        app,
        &runtime,
        &pane,
        |pane| pane.panels[1].loading && pane.panels[1].requested_endpoint.as_ref() == Some(&b),
        "follow read to /b in flight",
    );
    let token = pane.read_with(app, |pane, _| {
        pane.panels[1]
            .root_request_cancellation
            .as_ref()
            .expect("in-flight read owns a token")
            .cancellation
            .clone()
    });
    assert!(!token.is_cancelled());

    // 关闭跟随取消该读取；token 真实到达 provider，请求身份被清除。
    app.update(|cx| pane.update(cx, |pane, cx| pane.set_follow_terminal_directory(false, cx)));
    assert!(
        token.is_cancelled(),
        "disabling follow must cancel the follow-initiated read"
    );
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(!pane.panels[1].loading);
            assert!(pane.terminal_directory_follow_generation.is_none());
            assert_eq!(pane.panels[1].endpoint, a, "committed listing is preserved");
            assert!(
                !pane.data_entrance.awaiting_result(),
                "disarm accompanies the cancellation"
            );
        })
    });
    pump_until(
        app,
        &runtime,
        &pane,
        |_pane| provider.start_count() > 1,
        "cancelled provider call observed",
    );
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert_eq!(
                pane.panels[1]
                    .listing
                    .as_ref()
                    .map(|listing| listing.directory.clone()),
                Some(a.clone()),
                "late cancelled receipt must not change the directory"
            );
        })
    });
}

/// 中央文件工作区不播放右栏入场，即使发生了真实目录切换和成功读取。
///
/// @author kongweiguang
#[gpui_kit::test]
fn central_file_workspace_never_arms_compact_entrance(app: &mut TestAppContext) {
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:\\work"),
    };
    let fixture = support::entity_fixture(app, local, |pane| {
        pane.file_workspace_layout = true;
        pane.compact_layout = false;
    });
    let runtime = &fixture.runtime;
    let pane = fixture.pane.clone();
    let host_a = HostId::new();
    let host_b = HostId::new();
    let a = support::remote_endpoint(&host_a, "/a");
    let b = support::remote_endpoint(&host_b, "/b");
    let bind = binding();

    app.update(|cx| pane.update(cx, |pane, cx| pane.set_active(true, cx)));
    seed_listing(&fixture, &a, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(a.clone()), None, Some(bind.clone()), cx);
        })
    });
    wait_for_listing(app, runtime, &pane, &a);
    seed_listing(&fixture, &b, Vec::new());
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(b.clone()), None, Some(bind), cx);
        })
    });
    wait_for_listing(app, runtime, &pane, &b);
    app.update(|cx| {
        pane.update(cx, |pane, _| {
            assert!(!pane.data_entrance.awaiting_result());
            assert_eq!(pane.data_entrance.opacity(Instant::now()), 1.0);
        })
    });
}
