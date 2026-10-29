//! 真实连接表单初始化、协议缓存与关闭保护的 GPUI 上下文回归。
//!
//! @author kongweiguang

use std::{
    cell::RefCell,
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::{Arc, Mutex},
};

use gpui_kit::{AppContext as _, Context, Entity, TestAppContext, VisualTestContext, Window};
use kerminal_application::{
    AppResult, ApplicationTaskExecutor, ProfileService,
    connections::{
        ConnectionIntent, HostKeyChallenge, HostKeyPeer, HostKeyRotationReceipt,
        HostKeyRotationRequest, HostKeyTrustReceipt, HostKeyTrustRequest, RemoteConnectionLease,
        RemoteConnectionService,
        management::{
            HostAuthenticationMode, HostDocumentRevision, HostDraft, HostGroup,
            HostGroupDeletionReceipt, HostGroupDraft, HostGroupSnapshot, HostManagementRepository,
            HostManagementService, HostRouteSettings, HostSftpOptions, HostTerminalOptions,
            ManagedHost, RdpDisplayOptions,
        },
    },
    preferences::{
        DefaultLocalShellProvider, ProfileDraft, ProfileRepository, ProfileRevision,
        VersionedProfile,
    },
};
use kerminal_domain::{
    connections::{HostEndpoint, RemoteProtocol},
    identity::HostId,
    preferences::{ConfigRevision, ConfigurationMode, ConfigurationRecoveryState, TerminalProfile},
    sessions::LocalSessionTarget,
};
use tokio::{
    runtime::{Builder, Runtime},
    sync::Semaphore,
};
use tokio_util::sync::CancellationToken;

use super::{ConnectionsEditorKind, ConnectionsPanel, HostEditorSection};

type PortFuture<'a, T> = Pin<Box<dyn Future<Output = AppResult<T>> + Send + 'a>>;

/// 目录仅提供内存快照；任何写入都使测试立即失败，防止关闭回归偷偷走保存路径。
struct ReadOnlyHostDirectory {
    rows: Vec<ManagedHost>,
    groups: Vec<HostGroup>,
}

impl HostManagementRepository for ReadOnlyHostDirectory {
    /// 测试没有保存授权，因此写门禁被访问本身就是错误。
    /// @author kongweiguang
    fn ensure_host_writable<'a, 'f>(&'a self) -> PortFuture<'f, ()>
    where
        'a: 'f,
        Self: 'f,
    {
        panic!("关闭表单不应请求主机写门禁")
    }

    /// 生产构造器会自动 reload，返回固定内存目录以保留这条真实路径。
    /// @author kongweiguang
    fn list_hosts<'a, 'f>(&'a self) -> PortFuture<'f, Vec<ManagedHost>>
    where
        'a: 'f,
        Self: 'f,
    {
        Box::pin(async { Ok(self.rows.clone()) })
    }

    /// 编辑已有主机仍只读取测试快照，不解析真实配置根。
    /// @author kongweiguang
    fn load_host<'a, 'b, 'f>(&'a self, id: &'b HostId) -> PortFuture<'f, Option<ManagedHost>>
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        Box::pin(async move { Ok(self.rows.iter().find(|host| &host.id == id).cloned()) })
    }

    /// 禁止用保存将表单重置为干净状态，避免测试掩盖关闭判断缺陷。
    /// @author kongweiguang
    fn save_host<'a, 'f>(
        &'a self,
        _: HostId,
        _: HostDraft,
        _: Option<ConfigRevision>,
    ) -> PortFuture<'f, ManagedHost>
    where
        'a: 'f,
        Self: 'f,
    {
        panic!("关闭表单不应保存主机")
    }

    /// 测试只丢弃未保存草稿，不允许触发主机删除。
    /// @author kongweiguang
    fn delete_host<'a, 'b, 'c, 'f>(
        &'a self,
        _: &'b HostId,
        _: &'c ConfigRevision,
    ) -> PortFuture<'f, ManagedHost>
    where
        'a: 'f,
        'b: 'f,
        'c: 'f,
        Self: 'f,
    {
        panic!("关闭表单不应删除主机")
    }

    /// 预选分组来自同一生产 reload 快照，避免直接拼造 HostEditor 的基线。
    /// @author kongweiguang
    fn load_groups<'a, 'f>(&'a self) -> PortFuture<'f, HostGroupSnapshot>
    where
        'a: 'f,
        Self: 'f,
    {
        Box::pin(async {
            Ok(HostGroupSnapshot {
                groups: self.groups.clone(),
                revision: None,
            })
        })
    }

    /// 分组预选不应写回 groups.toml。
    /// @author kongweiguang
    fn save_group<'a, 'f>(
        &'a self,
        _: HostGroupDraft,
        _: Option<ConfigRevision>,
    ) -> PortFuture<'f, HostGroupSnapshot>
    where
        'a: 'f,
        Self: 'f,
    {
        panic!("关闭表单不应保存分组")
    }

    /// 关闭主机草稿不能连带执行目录事务。
    /// @author kongweiguang
    fn delete_group_and_reassign<'a, 'f>(
        &'a self,
        _: String,
        _: Option<String>,
        _: ConfigRevision,
        _: Vec<HostDocumentRevision>,
    ) -> PortFuture<'f, HostGroupDeletionReceipt>
    where
        'a: 'f,
        Self: 'f,
    {
        panic!("关闭表单不应删除分组")
    }
}

struct NoRemoteConnections {
    preflight_fails: bool,
}

impl RemoteConnectionService for NoRemoteConnections {
    /// 本回归只操作表单，任何连接请求均代表越过测试边界。
    /// @author kongweiguang
    fn connect<'a, 'f>(
        &'a self,
        _: HostId,
        _: ConnectionIntent,
        _: CancellationToken,
    ) -> PortFuture<'f, RemoteConnectionLease>
    where
        'a: 'f,
        Self: 'f,
    {
        panic!("表单关闭回归不得建立远程连接")
    }

    /// 表单初始化与取消不需要读取服务器指纹；仅失败反馈回归允许一次确定的离线失败。
    /// @author kongweiguang
    fn inspect_host_key<'a, 'f>(
        &'a self,
        _: HostId,
        _: HostKeyPeer,
        _: CancellationToken,
    ) -> PortFuture<'f, HostKeyChallenge>
    where
        'a: 'f,
        Self: 'f,
    {
        if self.preflight_fails {
            return Box::pin(async {
                Err(kerminal_application::AppError::infrastructure(
                    "SSH TCP connection failed",
                ))
            });
        }
        panic!("表单关闭回归不得探测主机密钥")
    }

    /// 丢弃确认不是信任服务器的授权。
    /// @author kongweiguang
    fn trust_host_key<'a, 'f>(
        &'a self,
        _: HostKeyTrustRequest,
        _: CancellationToken,
    ) -> PortFuture<'f, HostKeyTrustReceipt>
    where
        'a: 'f,
        Self: 'f,
    {
        panic!("表单关闭回归不得信任主机密钥")
    }

    /// 测试完全隔离 known_hosts，密钥轮换不得被间接触发。
    /// @author kongweiguang
    fn replace_host_key<'a, 'f>(
        &'a self,
        _: HostKeyRotationRequest,
        _: CancellationToken,
    ) -> PortFuture<'f, HostKeyRotationReceipt>
    where
        'a: 'f,
        Self: 'f,
    {
        panic!("表单关闭回归不得轮换主机密钥")
    }
}

/// 仅延迟默认 Profile 的读取回执，测试可控制异步填充值与用户编辑之间的先后顺序。
struct DelayedProfileDirectory {
    release_default: Arc<Semaphore>,
    selected: Option<(Arc<Mutex<VersionedProfile>>, Arc<Semaphore>)>,
}

impl ProfileRepository for DelayedProfileDirectory {
    /// 信号量释放前保留真实请求在途，避免用手动调用 UI 回执代替生产异步链路。
    /// @author kongweiguang
    fn list_profiles<'a, 'f>(&'a self) -> PortFuture<'f, Vec<TerminalProfile>>
    where
        'a: 'f,
        Self: 'f,
    {
        Box::pin(async {
            self.release_default
                .acquire()
                .await
                .expect("测试回执门禁保持打开")
                .forget();
            Ok(vec![default_profile()])
        })
    }

    /// 可选提供同一内存 Profile，列表和按 ID 读取共享真实测试仓储。
    /// @author kongweiguang
    fn list_versioned_profiles<'a, 'f>(&'a self) -> PortFuture<'f, Vec<VersionedProfile>>
    where
        'a: 'f,
        Self: 'f,
    {
        Box::pin(async {
            Ok(self
                .selected
                .as_ref()
                .map(|(profile, _)| vec![profile.lock().unwrap().clone()])
                .unwrap_or_default())
        })
    }

    /// 显式选择 Profile 时等待测试释放，再读取仓储最新值以验证迟到回执门禁。
    /// @author kongweiguang
    fn get_profile<'a, 'b, 'f>(&'a self, id: &'b str) -> PortFuture<'f, Option<VersionedProfile>>
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        Box::pin(async move {
            let Some((profile, release)) = &self.selected else {
                panic!("未选择 Profile 时不得读取")
            };
            release.acquire().await.expect("Profile gate").forget();
            let current = profile.lock().unwrap().clone();
            Ok((current.profile.id == id).then_some(current))
        })
    }

    /// 回执和关闭验证均不得触发持久化，防止隐式保存掩盖脏态。
    /// @author kongweiguang
    fn save_profile<'a, 'f>(
        &'a self,
        _: ProfileDraft,
        _: Option<ConfigRevision>,
    ) -> PortFuture<'f, VersionedProfile>
    where
        'a: 'f,
        Self: 'f,
    {
        panic!("本地表单关闭回归不得保存 Profile")
    }

    /// 丢弃未保存草稿不能删除磁盘配置。
    /// @author kongweiguang
    fn delete_profile<'a, 'b, 'f>(&'a self, _: &'b str, _: ConfigRevision) -> PortFuture<'f, bool>
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        panic!("本地表单关闭回归不得删除 Profile")
    }

    /// 默认值预填只读默认项，不具有切换默认配置的授权。
    /// @author kongweiguang
    fn set_default_profile<'a, 'b, 'f>(
        &'a self,
        _: &'b str,
        _: Vec<ProfileRevision>,
    ) -> PortFuture<'f, VersionedProfile>
    where
        'a: 'f,
        'b: 'f,
        Self: 'f,
    {
        panic!("本地表单关闭回归不得切换默认 Profile")
    }
}

struct NoPlatformShellFallback;

impl DefaultLocalShellProvider for NoPlatformShellFallback {
    /// 内存默认 Profile 已包含完整目标，测试不得回退读取 OS 环境。
    /// @author kongweiguang
    fn resolve_default(&self) -> AppResult<LocalSessionTarget> {
        panic!("默认 Profile 回归不得探测系统 Shell")
    }
}

/// 返回虚构路径和非秘密环境值，仅验证回显，不执行目标程序或读取目录。
/// @author kongweiguang
fn default_profile() -> TerminalProfile {
    TerminalProfile {
        id: "fixture-default".into(),
        name: "默认终端".into(),
        shell: "C:\\fixture\\pwsh.exe".into(),
        args: vec!["-NoLogo".into()],
        cwd: Some("C:\\fixture".into()),
        env: BTreeMap::from([("FIXTURE".into(), "profile".into())]),
        is_default: true,
        sidebar_group_id: None,
        sort_order: 0,
        created_at: "1".into(),
        updated_at: "1".into(),
    }
}

/// 使用完整应用服务解释默认项，信号量只替换仓储的完成时机，不模拟 UI 投影。
/// @author kongweiguang
fn delayed_profiles() -> (ProfileService, Arc<Semaphore>) {
    let release_default = Arc::new(Semaphore::new(0));
    let service = ProfileService::new(
        Arc::new(DelayedProfileDirectory {
            release_default: release_default.clone(),
            selected: None,
        }),
        Arc::new(NoPlatformShellFallback),
    );
    (service, release_default)
}

/// 使用独立当前线程 runtime 驱动 ctor 的真实 reload，不读用户目录、不启动 OS 窗口。
/// @author kongweiguang
fn fixture(
    app: &mut TestAppContext,
    rows: Vec<ManagedHost>,
    groups: Vec<HostGroup>,
) -> (Runtime, Entity<ConnectionsPanel>, &mut VisualTestContext) {
    fixture_with_profiles(app, rows, groups, None)
}

/// 可选注入本地 ProfileService，远程与本地回归仍共用同一生产面板及隔离 runtime。
/// @author kongweiguang
fn fixture_with_profiles(
    app: &mut TestAppContext,
    rows: Vec<ManagedHost>,
    groups: Vec<HostGroup>,
    profiles: Option<ProfileService>,
) -> (Runtime, Entity<ConnectionsPanel>, &mut VisualTestContext) {
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let hosts = HostManagementService::new(Arc::new(ReadOnlyHostDirectory { rows, groups }));
    app.update(|app| {
        crate::init(app);
        app.set_reduce_motion(true);
    });
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let panel = cx.new(|cx| {
            let panel = ConnectionsPanel::new(
                hosts,
                Arc::new(NoRemoteConnections {
                    preflight_fails: false,
                }),
                None,
                executor,
                ConfigurationRecoveryState {
                    mode: ConfigurationMode::ReadWrite,
                    issues: Vec::new(),
                },
                window,
                cx,
            );
            if let Some(profiles) = profiles {
                panel.with_profiles(profiles, cx)
            } else {
                panel
            }
        });
        *mounted.borrow_mut() = Some(panel.clone());
        gpui_kit::base::Root::new(panel, window, cx)
    });
    let panel = slot.borrow_mut().take().expect("生产连接面板已挂载");
    pump_runtime(&runtime, visual);
    visual.update(|_, app| {
        assert!(!panel.read(app).loading, "生产目录 reload 必须完成");
        assert!(
            !panel.read(app).local_hosts.loading,
            "本地目录 reload 必须完成"
        );
        assert!(panel.read(app).status.is_none());
    });
    (runtime, panel, visual)
}

/// 交替排空 GPUI 和 Tokio 即时任务；只有测试显式调用时默认回执才有机会进入 UI。
/// @author kongweiguang
fn pump_runtime(runtime: &Runtime, visual: &mut VisualTestContext) {
    for _ in 0..3 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
    draw(visual);
}

/// 首次输入控件的默认值和订阅可能延后应用；完成两帧再判断关闭结果。
/// @author kongweiguang
fn draw(visual: &mut VisualTestContext) {
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
    visual.update(|window, app| window.draw(app).clear(app));
    visual.run_until_parked();
}

/// 所有动作都修改生产面板实体，避免用复制的 is_dirty 逻辑制造测试结果。
/// @author kongweiguang
fn update_panel(
    panel: &Entity<ConnectionsPanel>,
    visual: &mut VisualTestContext,
    action: impl FnOnce(&mut ConnectionsPanel, &mut Window, &mut Context<ConnectionsPanel>),
) {
    visual.update(|window, app| panel.update(app, |panel, cx| action(panel, window, cx)));
    draw(visual);
}

/// 通过真实关闭 owner 断言整个统一编辑器已清理，而不只断言底层比较函数。
/// @author kongweiguang
fn close_clean_editor(panel: &Entity<ConnectionsPanel>, visual: &mut VisualTestContext) {
    update_panel(panel, visual, |panel, window, cx| {
        panel.cancel_editor(window, cx)
    });
    visual.update(|_, app| {
        let panel = panel.read(app);
        assert!(panel.editor.is_none(), "未修改表单应直接关闭");
        assert!(panel.local_hosts.editor.is_none(), "本地草稿应同步关闭");
        assert!(!panel.pending_editor_discard, "未修改表单不应弹丢弃确认");
        assert!(!panel.editor_open());
        assert_eq!(panel.active_editor_kind, None);
    });
}

/// SFTP 的 SSH 副本必须是新的干净草稿，复用 endpoint 而不修改源主机或触发保存。
/// @author kongweiguang
#[gpui_kit::test]
fn duplicate_sftp_as_ssh_preserves_endpoint_without_modifying_source(app: &mut TestAppContext) {
    let mut source = existing_host();
    source.protocol = RemoteProtocol::Sftp;
    let id = source.id.clone();
    let (_runtime, panel, visual) = fixture(app, vec![source.clone()], Vec::new());
    update_panel(&panel, visual, |panel, window, cx| {
        panel.duplicate_saved_host_as_ssh(&id, window, cx);
    });
    visual.update(|_, app| {
        let panel = panel.read(app);
        let editor = panel.editor.as_ref().expect("SSH copy editor");
        assert_eq!(editor.protocol, RemoteProtocol::Ssh);
        assert!(
            editor.original.is_none(),
            "the copy must not own source credentials or revision"
        );
        assert_eq!(
            editor.inputs.host.read(app).value().as_str(),
            source.endpoint.host
        );
        assert_eq!(
            editor.inputs.port.read(app).value().as_str(),
            source.endpoint.port.to_string()
        );
        assert_eq!(panel.rows[0].protocol, RemoteProtocol::Sftp);
    });
    close_clean_editor(&panel, visual);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.duplicate_saved_host(&id, window, cx);
    });
    visual.update(|_, app| {
        assert_eq!(
            panel.read(app).editor.as_ref().unwrap().protocol,
            RemoteProtocol::Sftp
        )
    });
    close_clean_editor(&panel, visual);
}

/// 五种新建协议在真实输入控件完成初始化后均应可直接关闭。
/// @author kongweiguang
#[gpui_kit::test]
fn untouched_remote_editors_close_without_discard_after_frames(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    for protocol in [
        RemoteProtocol::Ssh,
        RemoteProtocol::Sftp,
        RemoteProtocol::Telnet,
        RemoteProtocol::Rdp,
        RemoteProtocol::Serial,
    ] {
        update_panel(&panel, visual, |panel, window, cx| {
            panel.create_host_with_protocol(protocol, window, cx);
        });
        visual.update(|_, app| {
            assert_eq!(panel.read(app).editor.as_ref().unwrap().protocol, protocol)
        });
        close_clean_editor(&panel, visual);
    }
}

/// 远程协议切换、分区导航与分组入口预选都属于初值或视图状态，不是用户字段修改。
/// @author kongweiguang
#[gpui_kit::test]
fn untouched_protocol_navigation_and_group_preselection_stay_clean(app: &mut TestAppContext) {
    let group = HostGroup {
        id: "fixture-group".into(),
        name: "测试分组".into(),
        sort_order: 0,
        created_at: "1".into(),
        updated_at: "1".into(),
    };
    let (_runtime, panel, visual) = fixture(app, Vec::new(), vec![group]);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.create_host_in_group("fixture-group", window, cx);
    });
    for protocol in [
        RemoteProtocol::Sftp,
        RemoteProtocol::Rdp,
        RemoteProtocol::Serial,
        RemoteProtocol::Telnet,
        RemoteProtocol::Ssh,
    ] {
        update_panel(&panel, visual, |panel, window, cx| {
            panel.activate_remote_protocol(protocol, window, cx);
        });
        for section in HostEditorSection::for_protocol(protocol) {
            update_panel(&panel, visual, |panel, window, cx| {
                panel.select_editor_section(*section, window, cx)
            });
        }
    }
    visual.update(|_, app| {
        assert_eq!(
            panel
                .read(app)
                .editor
                .as_ref()
                .unwrap()
                .selected_group
                .as_deref(),
            Some("fixture-group")
        );
    });
    close_clean_editor(&panel, visual);
}

/// 名称与标签跨协议共用同一控件；恢复原值后不能留下旧协议的离开时脏标记。
/// @author kongweiguang
#[gpui_kit::test]
fn shared_inputs_reverted_after_protocol_switch_close_without_discard(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    for change_tags in [false, true] {
        update_panel(&panel, visual, |panel, window, cx| {
            panel.begin_create_host(window, cx);
            let inputs = &panel.editor.as_ref().unwrap().inputs;
            let input = if change_tags {
                &inputs.tags
            } else {
                &inputs.name
            };
            input.update(cx, |input, cx| input.replace_all("临时值", window, cx));
        });
        update_panel(&panel, visual, |panel, window, cx| {
            panel.activate_remote_protocol(RemoteProtocol::Rdp, window, cx);
        });
        update_panel(&panel, visual, |panel, window, cx| {
            let inputs = &panel.editor.as_ref().unwrap().inputs;
            let input = if change_tags {
                &inputs.tags
            } else {
                &inputs.name
            };
            input.update(cx, |input, cx| input.replace_all("", window, cx));
        });
        close_clean_editor(&panel, visual);
    }
}

/// 认证、分组与收藏恢复默认值属于可撤销编辑，比较应反映最终值而非交互次数。
/// @author kongweiguang
#[gpui_kit::test]
fn reverted_authentication_group_and_favorite_close_without_discard(app: &mut TestAppContext) {
    let group = HostGroup {
        id: "fixture-group".into(),
        name: "测试分组".into(),
        sort_order: 0,
        created_at: "1".into(),
        updated_at: "1".into(),
    };
    let (_runtime, panel, visual) = fixture(app, Vec::new(), vec![group]);
    for field in 0..3 {
        update_panel(&panel, visual, |panel, window, cx| {
            panel.begin_create_host(window, cx)
        });
        update_panel(&panel, visual, |panel, window, cx| match field {
            0 => panel.set_authentication_mode(HostAuthenticationMode::Agent, window, cx),
            1 => panel.select_editor_group(Some("fixture-group".into()), cx),
            _ => panel.toggle_favorite(cx),
        });
        update_panel(&panel, visual, |panel, window, cx| match field {
            0 => panel.set_authentication_mode(HostAuthenticationMode::Password, window, cx),
            1 => panel.select_editor_group(None, cx),
            _ => panel.toggle_favorite(cx),
        });
        close_clean_editor(&panel, visual);
    }
}

/// 既有配置同时含高级值与多行备注，保证回显后的基线来自真实输入而不是空表单特判。
/// @author kongweiguang
fn existing_host() -> ManagedHost {
    ManagedHost {
        id: HostId::new(),
        group_id: None,
        name: "已保存主机".into(),
        note: Some("第一行\n第二行".into()),
        protocol: RemoteProtocol::Ssh,
        endpoint: HostEndpoint {
            host: "fixture.example.test".into(),
            port: 2222,
            username: "tester".into(),
        },
        authentication: None,
        authentication_mode: HostAuthenticationMode::Agent,
        rdp_password: None,
        route: HostRouteSettings::default(),
        serial: None,
        tags: vec!["fixture".into()],
        is_favorite: true,
        sort_order: 10,
        connect_timeout_ms: 12_000,
        keepalive_interval_ms: Some(30_000),
        terminal_options: HostTerminalOptions::default(),
        sftp_options: HostSftpOptions::default(),
        rdp_display: RdpDisplayOptions::default(),
        created_at: "1".into(),
        updated_at: "2".into(),
        revision: ConfigRevision::from_sha256("fixture-revision".into()),
    }
}

/// 既有配置不改动和字段改回原值，都必须关闭且不显示丢弃确认。
/// @author kongweiguang
#[gpui_kit::test]
fn saved_host_and_reverted_input_close_without_discard(app: &mut TestAppContext) {
    let host = existing_host();
    let id = host.id.clone();
    let (_runtime, panel, visual) = fixture(app, vec![host], Vec::new());
    update_panel(&panel, visual, |panel, window, cx| {
        panel.edit_host(&id, window, cx)
    });
    close_clean_editor(&panel, visual);
    update_panel(&panel, visual, |panel, window, cx| {
        panel.edit_host(&id, window, cx)
    });
    for value in ["临时修改", "已保存主机"] {
        update_panel(&panel, visual, |panel, window, cx| {
            panel
                .editor
                .as_ref()
                .unwrap()
                .inputs
                .name
                .update(cx, |input, cx| {
                    input.replace_all(value, window, cx);
                });
        });
    }
    close_clean_editor(&panel, visual);
}

/// 隐藏协议的真改动仍须拦截关闭；取消确认保留实体与秘密，确认后清空全部缓存秘密。
/// @author kongweiguang
#[gpui_kit::test]
fn hidden_dirty_protocol_requires_discard_and_preserves_cancelled_draft(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_host(window, cx);
        let inputs = &panel.editor.as_ref().unwrap().inputs;
        inputs
            .name
            .update(cx, |input, cx| input.replace_all("未保存 SSH", window, cx));
        inputs.secret.update(cx, |input, cx| {
            input.replace_all("fixture-secret", window, cx)
        });
    });
    let (name, secret) = visual.update(|_, app| {
        let editor = panel.read(app).editor.as_ref().unwrap();
        (editor.inputs.name.clone(), editor.inputs.secret.clone())
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.activate_remote_protocol(RemoteProtocol::Rdp, window, cx);
        panel.cancel_editor(window, cx);
    });
    visual.update(|_, app| {
        assert!(panel.read(app).pending_editor_discard);
        assert!(panel.read(app).editor.is_some());
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.keep_editor_open(window, cx)
    });
    visual.update(|_, app| {
        assert!(!panel.read(app).pending_editor_discard);
        assert_eq!(
            panel.read(app).active_editor_kind,
            Some(ConnectionsEditorKind::Remote)
        );
        assert_eq!(name.read(app).value().as_str(), "未保存 SSH");
        assert!(!secret.read(app).value().is_empty());
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.cancel_editor(window, cx)
    });
    visual.update(|_, app| assert!(panel.read(app).pending_editor_discard));
    update_panel(&panel, visual, |panel, window, cx| {
        panel.confirm_discard_editor(window, cx)
    });
    visual.update(|_, app| {
        assert!(panel.read(app).editor.is_none());
        assert!(!panel.read(app).pending_editor_discard);
        assert!(!panel.read(app).editor_open());
        assert!(
            secret.read(app).value().is_empty(),
            "隐藏协议秘密应在确认丢弃后清理"
        );
    });
}

/// 未填写本地草稿可立即关闭；跨 Local/Remote 导航保留同一草稿且不制造修改。
/// @author kongweiguang
#[gpui_kit::test]
fn untouched_local_and_local_remote_roundtrip_close_without_discard(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_local_host(None, window, cx);
    });
    close_clean_editor(&panel, visual);

    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_local_host(None, window, cx);
    });
    let profile_id = visual.update(|_, app| {
        panel
            .read(app)
            .local_hosts
            .editor
            .as_ref()
            .unwrap()
            .profile_id
            .clone()
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.activate_remote_protocol(RemoteProtocol::Sftp, window, cx);
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.activate_local_host_editor(window, cx);
    });
    visual.update(|_, app| {
        let panel = panel.read(app);
        assert_eq!(panel.active_editor_kind, Some(ConnectionsEditorKind::Local));
        assert_eq!(
            panel.local_hosts.editor.as_ref().unwrap().profile_id,
            profile_id
        );
        assert!(!panel.local_host_is_dirty(app));
    });
    close_clean_editor(&panel, visual);
}

/// 名称恢复打开时的文本后即恢复干净；一次 Change 事件不能永久标脏。
/// @author kongweiguang
#[gpui_kit::test]
fn reverted_local_name_closes_without_discard(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_local_host(None, window, cx);
    });
    let original_name = visual.update(|_, app| {
        panel
            .read(app)
            .local_hosts
            .editor
            .as_ref()
            .unwrap()
            .name
            .read(app)
            .value()
            .to_string()
    });
    for value in ["临时本地主机".to_owned(), original_name] {
        update_panel(&panel, visual, |panel, window, cx| {
            panel
                .local_hosts
                .editor
                .as_ref()
                .unwrap()
                .name
                .update(cx, |input, cx| {
                    input.replace_all(value, window, cx);
                });
        });
    }
    close_clean_editor(&panel, visual);
}

/// 本地真实修改需要退出确认，取消保留环境输入，确认才清空潜在秘密并销毁草稿。
/// @author kongweiguang
#[gpui_kit::test]
fn dirty_local_discard_keeps_then_clears_environment(app: &mut TestAppContext) {
    let (_runtime, panel, visual) = fixture(app, Vec::new(), Vec::new());
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_local_host(None, window, cx);
        panel
            .local_hosts
            .editor
            .as_ref()
            .unwrap()
            .env
            .update(cx, |input, cx| {
                input.replace_all("FIXTURE_ENV=manual", window, cx);
            });
    });
    let env = visual.update(|_, app| {
        panel
            .read(app)
            .local_hosts
            .editor
            .as_ref()
            .unwrap()
            .env
            .clone()
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.cancel_editor(window, cx)
    });
    visual.update(|_, app| {
        assert!(panel.read(app).pending_editor_discard);
        assert!(panel.read(app).local_hosts.editor.is_some());
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.keep_editor_open(window, cx)
    });
    visual.update(|_, app| {
        assert!(!panel.read(app).pending_editor_discard);
        assert!(panel.read(app).local_host_is_dirty(app));
        assert!(!env.read(app).value().is_empty());
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.cancel_editor(window, cx)
    });
    visual.update(|_, app| assert!(panel.read(app).pending_editor_discard));
    update_panel(&panel, visual, |panel, window, cx| {
        panel.confirm_discard_editor(window, cx)
    });
    visual.update(|_, app| {
        let panel = panel.read(app);
        assert!(panel.local_hosts.editor.is_none());
        assert!(panel.local_hosts.pending_action.is_none());
        assert!(!panel.pending_editor_discard);
        assert!(!panel.editor_open());
        assert!(env.read(app).value().is_empty());
    });
}

/// 默认 Profile 自动预填属于草稿初值；回执前可关闭，迟到回执失效，回执后仍保持干净。
/// @author kongweiguang
#[gpui_kit::test]
fn local_default_receipt_keeps_pristine_and_cannot_reopen_closed_editor(app: &mut TestAppContext) {
    let (profiles, release) = delayed_profiles();
    let (runtime, panel, visual) =
        fixture_with_profiles(app, Vec::new(), Vec::new(), Some(profiles));
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_local_host(None, window, cx);
    });
    pump_runtime(&runtime, visual);
    visual.update(|_, app| {
        let panel = panel.read(app);
        assert!(
            panel
                .local_hosts
                .editor
                .as_ref()
                .unwrap()
                .shell
                .read(app)
                .value()
                .is_empty()
        );
        assert!(!panel.local_host_is_dirty(app));
    });
    close_clean_editor(&panel, visual);
    release.add_permits(1);
    pump_runtime(&runtime, visual);
    visual.update(|_, app| {
        assert!(
            panel.read(app).local_hosts.editor.is_none(),
            "迟到回执不得重新打开编辑器"
        );
        assert!(!panel.read(app).editor_open());
    });

    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_local_host(None, window, cx);
    });
    release.add_permits(1);
    pump_runtime(&runtime, visual);
    visual.update(|_, app| {
        let panel = panel.read(app);
        let editor = panel.local_hosts.editor.as_ref().unwrap();
        assert_eq!(
            editor.shell.read(app).value().as_str(),
            default_profile().shell
        );
        assert_eq!(editor.cwd.read(app).value().as_str(), "C:\\fixture");
        assert_eq!(editor.args.read(app).value().as_str(), "-NoLogo");
        assert!(!editor.env.read(app).value().is_empty());
        assert!(
            !panel.local_host_is_dirty(app),
            "默认回执必须同步更新干净基线"
        );
    });
    close_clean_editor(&panel, visual);
}

/// 默认目标回执只能填充未改字段；请求期间编辑的名称、Shell、参数和环境必须保留。
/// @author kongweiguang
#[gpui_kit::test]
fn late_local_defaults_preserve_user_inputs_and_dirty_close_guard(app: &mut TestAppContext) {
    let (profiles, release) = delayed_profiles();
    let (runtime, panel, visual) =
        fixture_with_profiles(app, Vec::new(), Vec::new(), Some(profiles));
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_local_host(None, window, cx);
    });
    pump_runtime(&runtime, visual);
    update_panel(&panel, visual, |panel, window, cx| {
        let editor = panel.local_hosts.editor.as_ref().unwrap();
        editor
            .name
            .update(cx, |input, cx| input.replace_all("手动名称", window, cx));
        editor.shell.update(cx, |input, cx| {
            input.replace_all("C:\\manual\\shell.exe", window, cx)
        });
        editor
            .args
            .update(cx, |input, cx| input.replace_all("--manual", window, cx));
        editor.env.update(cx, |input, cx| {
            input.replace_all("FIXTURE_ENV=manual", window, cx)
        });
    });
    release.add_permits(1);
    pump_runtime(&runtime, visual);
    visual.update(|_, app| {
        let panel = panel.read(app);
        let editor = panel.local_hosts.editor.as_ref().unwrap();
        assert_eq!(editor.name.read(app).value().as_str(), "手动名称");
        assert_eq!(
            editor.shell.read(app).value().as_str(),
            "C:\\manual\\shell.exe"
        );
        assert_eq!(editor.args.read(app).value().as_str(), "--manual");
        assert!(editor.env.read(app).value().as_str() == "FIXTURE_ENV=manual");
        assert_eq!(editor.cwd.read(app).value().as_str(), "C:\\fixture");
        assert!(panel.local_host_is_dirty(app));
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.cancel_editor(window, cx)
    });
    visual.update(|_, app| {
        assert!(panel.read(app).pending_editor_discard);
        assert!(panel.read(app).local_hosts.editor.is_some());
    });
    update_panel(&panel, visual, |panel, window, cx| {
        panel.confirm_discard_editor(window, cx)
    });
    visual.update(|_, app| {
        assert!(panel.read(app).local_hosts.editor.is_none());
        assert!(!panel.read(app).pending_editor_discard);
        assert!(!panel.read(app).editor_open());
    });
}

/// 本地主机菜单读取共享 Profile；确认时重新按 ID 读取，并保留请求期间用户继续修改的字段。
///
/// @author kongweiguang
#[gpui_kit::test]
fn local_host_picker_applies_latest_shared_profile_without_losing_edits(app: &mut TestAppContext) {
    let release_default = Arc::new(Semaphore::new(0));
    let release_selected = Arc::new(Semaphore::new(0));
    let mut profile = default_profile();
    profile.id = "stored-profile".into();
    profile.name = "Stored Profile".into();
    profile.is_default = false;
    let row = Arc::new(Mutex::new(VersionedProfile {
        profile,
        revision: ConfigRevision::from_sha256("stored-revision".into()),
    }));
    let profiles = ProfileService::new(
        Arc::new(DelayedProfileDirectory {
            release_default: release_default.clone(),
            selected: Some((row.clone(), release_selected.clone())),
        }),
        Arc::new(NoPlatformShellFallback),
    );
    let (runtime, panel, visual) =
        fixture_with_profiles(app, Vec::new(), Vec::new(), Some(profiles));
    update_panel(&panel, visual, |panel, window, cx| {
        panel.begin_create_local_host(None, window, cx)
    });
    pump_runtime(&runtime, visual);
    let picker = visual.update(|window, _| {
        gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .find(|node| {
                node.path()
                    .iter()
                    .any(|id| id == &gpui_kit::ElementId::from("connections-local-shell-picker"))
            })
            .expect("local Shell picker")
            .bounds()
            .center()
    });
    visual.simulate_click(picker, Default::default());
    draw(visual);
    let item = visual.update(|window, _| {
        gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .find(|node| {
                node.visible()
                    && node.role() == Some(gpui_kit::Role::MenuItem)
                    && node.label() == Some("Stored Profile")
            })
            .expect("saved Profile appears in Shell menu")
            .bounds()
            .center()
    });
    visual.simulate_click(item, Default::default());
    draw(visual);
    row.lock().unwrap().profile.shell = "C:\\updated\\pwsh.exe".into();
    update_panel(&panel, visual, |panel, window, cx| {
        panel
            .local_hosts
            .editor
            .as_ref()
            .unwrap()
            .cwd
            .update(cx, |input, cx| {
                input.replace_all("C:\\user-change", window, cx)
            });
    });
    release_selected.add_permits(1);
    pump_runtime(&runtime, visual);
    visual.update(|_, app| {
        let editor = panel.read(app).local_hosts.editor.as_ref().unwrap();
        assert_eq!(
            editor.shell.read(app).value().as_str(),
            "C:\\updated\\pwsh.exe"
        );
        assert_eq!(editor.cwd.read(app).value().as_str(), "C:\\user-change");
        assert_eq!(editor.args.read(app).value().as_str(), "-NoLogo");
        assert_eq!(editor.env.read(app).value().as_str(), "FIXTURE=profile");
    });
    release_default.add_permits(1);
    pump_runtime(&runtime, visual);
    visual.update(|_, app| {
        assert_eq!(
            panel
                .read(app)
                .local_hosts
                .editor
                .as_ref()
                .unwrap()
                .shell
                .read(app)
                .value()
                .as_str(),
            "C:\\updated\\pwsh.exe"
        )
    });
}

#[path = "connections_editor_interactions.rs"]
mod interaction_tests;

/// 容器续接意图复用正式连接取消边界；取消后的预检不探测网络，也不能留下可接受的旧 ID。
/// @author kongweiguang
#[gpui_kit::test]
fn sidebar_container_preflight_cancel_releases_intent(app: &mut TestAppContext) {
    let host = existing_host();
    let (runtime, panel, visual) = fixture(app, vec![host.clone()], Vec::new());
    update_panel(&panel, visual, |panel, _, cx| {
        assert!(panel.request_connect_host_for_container(&host.id, "container-a".into(), cx));
        assert!(panel.sidebar_container_intent_is_current("container-a"));
        panel.cancel_pending_trust(cx);
        assert!(!panel.sidebar_container_intent_is_current("container-a"));
        assert!(!panel.request_connect_host_for_container(&HostId::new(), "missing".into(), cx));
    });
    pump_runtime(&runtime, visual);
    visual.update(|_, app| {
        let panel = panel.read(app);
        assert!(!panel.sidebar_container_intent_is_current("container-a"));
        assert!(panel.pending_trust.is_none());
    });
}

#[path = "connections_probe_lifecycle.rs"]
mod probe_lifecycle_tests;

#[path = "connections_editor_scroll.rs"]
mod scroll_tests;

/// 后台预检失败保留原请求的本地化恢复指引；取消与后续请求清理旧反馈。
/// @author kongweiguang
#[gpui_kit::test]
fn sidebar_container_preflight_failure_is_bound_to_its_request(app: &mut TestAppContext) {
    let host = existing_host();
    let (runtime, panel, visual) = fixture(app, vec![host.clone()], Vec::new());
    update_panel(&panel, visual, |panel, _, cx| {
        panel.remote = Arc::new(NoRemoteConnections {
            preflight_fails: true,
        });
        assert!(panel.request_connect_host_for_container(&host.id, "failed-container".into(), cx));
    });
    pump_runtime(&runtime, visual);
    update_panel(&panel, visual, |panel, _, cx| {
        assert!(!panel.sidebar_container_intent_is_current("failed-container"));
        assert_eq!(
            panel.sidebar_container_failure("failed-container"),
            panel.status.as_deref()
        );
        assert_eq!(
            panel.sidebar_container_failure("failed-container"),
            Some(crate::locale::text(
                "无法连接 SSH 主机，请检查网络、地址和端口后重试。",
                "Cannot connect to the SSH host. Check the network, address and port, then retry."
            ).as_ref())
        );
        assert!(panel.sidebar_container_failure("other-container").is_none());
        panel.cancel_pending_trust(cx);
        assert!(
            panel
                .sidebar_container_failure("failed-container")
                .is_none()
        );
    });
}
