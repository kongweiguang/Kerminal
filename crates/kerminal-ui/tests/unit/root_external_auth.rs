//! 外部认证回退只对可交互的精确失败开放。
//!
//! @author kongweiguang

// 共用此处的隔离 Root 服务验证退出订阅，避免另一套真实会话或磁盘依赖。
#[path = "root_preferences_quit.rs"]
mod preferences_quit;

use super::*;

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor,
    connections::{
        ConnectionIntent, HostKeyChallenge, HostKeyPeer, HostKeyRotationReceipt,
        HostKeyRotationRequest, HostKeyTrustReceipt, HostKeyTrustRequest, RemoteConnectionLease,
        RemoteConnectionService,
        management::{
            HostAuthenticationMode, HostDocumentRevision, HostDraft, HostGroupDeletionReceipt,
            HostGroupDraft, HostGroupSnapshot, HostManagementRepository, HostManagementService,
            HostRouteSettings, HostSftpOptions, HostTerminalOptions, ManagedHost,
            RdpDisplayOptions,
        },
    },
    platform::external_launch::{
        ExternalLaunchIntent, ExternalLaunchService, ExternalTargetClassification,
        ExternalTargetIdentity, ExternalTargetLease, ExternalTargetLeaseOwner,
        ExternalTemporaryTargetRegistry, ExternalTemporaryTargetSpec, SavedExternalTargetLookup,
    },
    preferences::{
        ConfigurationRepository, DefaultLocalShellProvider, PreferencesService, ProfileDraft,
        ProfileRepository, ProfileRevision, ProfileService, SettingsRepository, SettingsSnapshot,
        ToolRailPanelPlacement, VersionedProfile, VersionedSettings,
        settings::ExternalLaunchSettings,
    },
    sessions::{
        CreateSessionRequest, PlainTextSnapshot, PlainTextSnapshotRequest, RenderSnapshot,
        RenderSnapshotRequest, ScrollRequest, SessionDeadline, SessionFilter,
        SessionRuntimeMetadata, SessionService, SessionSubscription, ShellCommandOutcome,
        ShellCommandRequest, TerminalCommandContext, TerminalInputEvent, TerminalPalette,
        TerminalSearchRequest, TerminalSearchResult, TerminalSelection, UiCommandLineRequest,
        UiSessionBinding,
    },
    workspace::{
        WorkspaceMachineStatus, WorkspacePaneMode, WorkspaceService, WorkspaceSessionLoad,
        WorkspaceSessionPane, WorkspaceSessionSaveOutcome, WorkspaceSessionTab, WorkspaceTargetRef,
    },
};
use kerminal_domain::{
    connections::{HostEndpoint, RemoteProtocol},
    files::FileEndpoint,
    identity::{SessionId, TabId},
    preferences::{
        ConfigDocument, ConfigDocumentPath, ConfigRevision, ConfigurationMode,
        ConfigurationRecoveryState, TerminalProfile,
    },
    sessions::{
        LocalSessionTarget, SessionInfo, SessionOwner, SshInitialCommand, SshSessionTarget,
        TerminalSize,
    },
};
use tokio::runtime::{Builder, Runtime};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::features::connections::ConnectionsEvent;
use crate::features::files::{
    FilePaneSide, FileTargetNameLookup, FilesPane, action_test_support as file_support,
};
use crate::features::workspace_controller::WorkspaceController;
use crate::root::AppServices;
use crate::root::{
    connections::{
        FileSftpOrigin, FileSftpSelectionRequest, PendingSftpHostCreation, handle_connections_event,
    },
    workspace_files::{FilesWorkspaceSelection, WorkspaceFileRuntime, sftp_transfer_tab_for_host},
};

/// 测试只接收内存请求；任何未预期的端口调用都不能访问本机或网络。
///
/// @author kongweiguang
fn unavailable<T>() -> AppResult<T> {
    Err(AppError::unsupported("external auth test fixture"))
}

struct EmptyConfiguration;

#[async_trait]
impl SettingsRepository for EmptyConfiguration {
    /// 使用默认快照隔离用户设置。
    async fn load_settings(&self) -> AppResult<VersionedSettings> {
        Ok(VersionedSettings {
            settings: SettingsSnapshot::default(),
            revision: None,
        })
    }
    /// 重试不应写设置。
    async fn save_settings(
        &self,
        _: SettingsSnapshot,
        _: Option<ConfigRevision>,
    ) -> AppResult<VersionedSettings> {
        unavailable()
    }
}

#[async_trait]
impl ConfigurationRepository for EmptyConfiguration {
    /// 不依赖真实配置根的恢复状态。
    fn recovery_state(&self) -> ConfigurationRecoveryState {
        ConfigurationRecoveryState {
            mode: ConfigurationMode::ReadWrite,
            issues: Vec::new(),
        }
    }
    /// 重试不读取配置文档。
    async fn read(&self, _: ConfigDocumentPath) -> AppResult<Option<ConfigDocument>> {
        unavailable()
    }
    /// 重试不写配置文档。
    async fn write(
        &self,
        _: ConfigDocumentPath,
        _: String,
        _: Option<ConfigRevision>,
    ) -> AppResult<ConfigRevision> {
        unavailable()
    }
}

#[async_trait]
impl ProfileRepository for EmptyConfiguration {
    /// 避免读取本机终端配置。
    async fn list_profiles(&self) -> AppResult<Vec<TerminalProfile>> {
        Ok(Vec::new())
    }
    /// 两个目录入口保持一致。
    async fn list_versioned_profiles(&self) -> AppResult<Vec<VersionedProfile>> {
        Ok(Vec::new())
    }
    /// 不允许解析磁盘 Profile。
    async fn get_profile(&self, _: &str) -> AppResult<Option<VersionedProfile>> {
        Ok(None)
    }
    /// 密码重试不保存 Profile。
    async fn save_profile(
        &self,
        _: ProfileDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<VersionedProfile> {
        unavailable()
    }
    /// 密码重试不删除 Profile。
    async fn delete_profile(&self, _: &str, _: ConfigRevision) -> AppResult<bool> {
        unavailable()
    }
    /// 密码重试不改变默认 Profile。
    async fn set_default_profile(
        &self,
        _: &str,
        _: Vec<ProfileRevision>,
    ) -> AppResult<VersionedProfile> {
        unavailable()
    }
}

impl DefaultLocalShellProvider for EmptyConfiguration {
    /// 不因渲染 Root 而探测本机 shell。
    fn resolve_default(&self) -> AppResult<LocalSessionTarget> {
        unavailable()
    }
}

#[derive(Default)]
struct EmptyHosts {
    saved: Vec<ManagedHost>,
}

#[async_trait]
impl HostManagementRepository for EmptyHosts {
    /// 未授权的配置写入必须失败。
    async fn ensure_host_writable(&self) -> AppResult<()> {
        unavailable()
    }
    /// 构造器只读取测试提供的主机快照，不访问用户配置目录。
    async fn list_hosts(&self) -> AppResult<Vec<ManagedHost>> {
        Ok(self.saved.clone())
    }
    /// 名称投影与列表使用同一测试快照，临时目标不会被推断为保存主机。
    async fn load_host(&self, id: &HostId) -> AppResult<Option<ManagedHost>> {
        Ok(self.saved.iter().find(|host| &host.id == id).cloned())
    }
    /// 重试不保存主机。
    async fn save_host(
        &self,
        _: HostId,
        _: HostDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<ManagedHost> {
        unavailable()
    }
    /// 重试不删除主机。
    async fn delete_host(&self, _: &HostId, _: &ConfigRevision) -> AppResult<ManagedHost> {
        unavailable()
    }
    /// 空目录不引用用户的分组。
    async fn load_groups(&self) -> AppResult<HostGroupSnapshot> {
        Ok(HostGroupSnapshot::default())
    }
    /// 重试不保存分组。
    async fn save_group(
        &self,
        _: HostGroupDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<HostGroupSnapshot> {
        unavailable()
    }
    /// 重试不删除分组。
    async fn delete_group_and_reassign(
        &self,
        _: String,
        _: Option<String>,
        _: ConfigRevision,
        _: Vec<HostDocumentRevision>,
    ) -> AppResult<HostGroupDeletionReceipt> {
        unavailable()
    }
}

struct NoRemote;

#[async_trait]
impl RemoteConnectionService for NoRemote {
    /// 本测试不能建立真实连接。
    async fn connect(
        &self,
        _: HostId,
        _: ConnectionIntent,
        _: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        unavailable()
    }
    /// 不访问网络探测主机密钥。
    async fn inspect_host_key(
        &self,
        _: HostId,
        _: HostKeyPeer,
        _: CancellationToken,
    ) -> AppResult<HostKeyChallenge> {
        unavailable()
    }
    /// 不写 known_hosts。
    async fn trust_host_key(
        &self,
        _: HostKeyTrustRequest,
        _: CancellationToken,
    ) -> AppResult<HostKeyTrustReceipt> {
        unavailable()
    }
    /// 不更换真实密钥。
    async fn replace_host_key(
        &self,
        _: HostKeyRotationRequest,
        _: CancellationToken,
    ) -> AppResult<HostKeyRotationReceipt> {
        unavailable()
    }
}

#[derive(Default)]
struct TemporaryTargets {
    retains: Mutex<usize>,
    releases: Mutex<Vec<HostId>>,
}

impl ExternalTargetLeaseOwner for TemporaryTargets {
    /// 捕获独立所有权的获取，验证失效回执不会取得新资源。
    fn retain_scope(&self, _: &HostId) -> AppResult<()> {
        *self.retains.lock().unwrap() += 1;
        Ok(())
    }
    /// 精确记录最后的 scope 释放，不触及用户配置或凭据。
    fn release_scope(&self, host_id: &HostId) {
        self.releases.lock().unwrap().push(host_id.clone());
    }
}

#[async_trait]
impl ExternalTemporaryTargetRegistry for TemporaryTargets {
    /// 测试直接建立 lease，不允许额外注册目标。
    async fn register(&self, _: ExternalTemporaryTargetSpec) -> AppResult<ExternalTargetLease> {
        unavailable()
    }
    /// 本测试不提交真实口令。
    async fn set_password(&self, _: &HostId, _: Zeroizing<String>) -> AppResult<()> {
        unavailable()
    }
    /// 不持有真实凭据或外部资源。
    async fn clear_all(&self) -> AppResult<()> {
        Ok(())
    }
}

#[async_trait]
impl SavedExternalTargetLookup for TemporaryTargets {
    /// 本测试不查询持久主机，也不推测其凭据。
    async fn find_exact(&self, _: &ExternalTargetIdentity) -> AppResult<Option<HostId>> {
        Ok(None)
    }
}

#[derive(Default)]
struct CapturingSessions {
    creates: Mutex<Vec<CreateSessionRequest>>,
}

#[async_trait]
impl SessionService for CapturingSessions {
    /// 任何非可取消创建都违反测试的生命周期边界。
    async fn create(&self, _: CreateSessionRequest) -> AppResult<SessionInfo> {
        unavailable()
    }
    /// 捕获生产请求后模拟认证失败，不启动 PTY 或联网。
    async fn create_cancellable(
        &self,
        request: CreateSessionRequest,
        _: CancellationToken,
    ) -> AppResult<SessionInfo> {
        self.creates.lock().unwrap().push(request);
        Err(AppError::authentication_failed())
    }
    /// 禁止查询真实会话。
    async fn list(&self, _: SessionFilter) -> AppResult<Vec<SessionInfo>> {
        unavailable()
    }
    /// 无会话资源可查询。
    async fn runtime_metadata(&self, _: SessionId) -> AppResult<SessionRuntimeMetadata> {
        unavailable()
    }
    /// 不向真实终端写入。
    async fn write(&self, _: SessionId, _: Vec<u8>) -> AppResult<()> {
        unavailable()
    }
    /// 不发送真实输入。
    async fn write_input(&self, _: SessionId, _: TerminalInputEvent) -> AppResult<()> {
        unavailable()
    }
    /// 不向任何 UI owner 发送命令。
    async fn submit_ui_command_line(
        &self,
        _: UiCommandLineRequest,
    ) -> AppResult<TerminalCommandContext> {
        unavailable()
    }
    /// 不调整真实终端尺寸。
    async fn resize(&self, _: SessionId, _: TerminalSize) -> AppResult<()> {
        unavailable()
    }
    /// 不读取真实终端快照。
    async fn render_snapshot(
        &self,
        _: SessionId,
        _: RenderSnapshotRequest,
    ) -> AppResult<RenderSnapshot> {
        unavailable()
    }
    /// 不修改真实终端调色板。
    async fn set_palette(&self, _: SessionId, _: TerminalPalette) -> AppResult<()> {
        unavailable()
    }
    /// 不修改真实终端历史容量。
    async fn set_scrollback_limit(&self, _: SessionId, _: usize) -> AppResult<usize> {
        unavailable()
    }
    /// 不读取真实终端文本。
    async fn plaintext_snapshot(
        &self,
        _: SessionId,
        _: PlainTextSnapshotRequest,
    ) -> AppResult<PlainTextSnapshot> {
        unavailable()
    }
    /// 不滚动真实终端。
    async fn scroll(&self, _: SessionId, _: ScrollRequest) -> AppResult<()> {
        unavailable()
    }
    /// 不对可见 UI 会话执行滚动。
    async fn scroll_ui(&self, _: UiSessionBinding, _: ScrollRequest) -> AppResult<()> {
        unavailable()
    }
    /// 不修改真实终端选择。
    async fn set_selection(&self, _: SessionId, _: Option<TerminalSelection>) -> AppResult<()> {
        unavailable()
    }
    /// 不读取真实终端选择。
    async fn selected_text(&self, _: SessionId) -> AppResult<Option<String>> {
        unavailable()
    }
    /// 不搜索真实终端历史。
    async fn search(
        &self,
        _: SessionId,
        _: TerminalSearchRequest,
    ) -> AppResult<TerminalSearchResult> {
        unavailable()
    }
    /// 初次重试必须创建原 pane，不应调用已有会话的重连。
    async fn reconnect_cancellable(
        &self,
        _: SessionId,
        _: CancellationToken,
    ) -> AppResult<SessionInfo> {
        unavailable()
    }
    /// 没有已建立的 session 可关闭。
    async fn close(&self, _: SessionId) -> AppResult<()> {
        unavailable()
    }
    /// 没有已建立的 session 可按时限关闭。
    async fn close_until(&self, _: SessionId, _: SessionDeadline) -> AppResult<()> {
        unavailable()
    }
    /// 不操作进程外的会话。
    async fn close_all(&self) -> AppResult<()> {
        unavailable()
    }
    /// 不订阅真实终端。
    async fn subscribe(&self, _: SessionId) -> AppResult<SessionSubscription> {
        unavailable()
    }
    /// 不执行远端命令。
    async fn execute_shell_command(
        &self,
        _: ShellCommandRequest,
        _: CancellationToken,
    ) -> AppResult<ShellCommandOutcome> {
        unavailable()
    }
}

/// 在真实 Root 中装配隔离服务，保留生产密码模态和会话投递路径。
///
/// @author kongweiguang
fn fixture(
    app: &mut TestAppContext,
) -> (
    Runtime,
    Entity<RootView>,
    Arc<CapturingSessions>,
    Arc<TemporaryTargets>,
    &mut VisualTestContext,
) {
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let sessions = Arc::new(CapturingSessions::default());
    let temporary = Arc::new(TemporaryTargets::default());
    let config = Arc::new(EmptyConfiguration);
    let services = AppServices::new(
        WorkspaceService::new(),
        sessions.clone(),
        ApplicationTaskExecutor::new(runtime.handle().clone()),
        PreferencesService::new(config.clone()),
        ProfileService::new(config.clone(), config),
    )
    .with_connections(
        HostManagementService::new(Arc::new(EmptyHosts::default())),
        Arc::new(NoRemote),
        None,
    )
    .with_external_launch(ExternalLaunchService::new(
        temporary.clone(),
        temporary.clone(),
        ExternalLaunchSettings::default(),
    ));
    app.update(|app| {
        crate::init(app);
        app.set_reduce_motion(true);
    });
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let root = cx.new(|cx| RootView::new(services, window, cx));
        *mounted.borrow_mut() = Some(root.clone());
        gpui_kit::base::Root::new(root, window, cx)
    });
    let root = slot.borrow_mut().take().expect("Root fixture is mounted");
    (runtime, root, sessions, temporary, visual)
}

/// 交替推进生产 GPUI 与 Tokio 回执，避免仅断言同步投递。
///
/// @author kongweiguang
fn settle(runtime: &Runtime, visual: &mut VisualTestContext) {
    for _ in 0..5 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
}

/// 所有目标均为内存临时身份，命令参数含空格以检验 argv 不被重新拼接。
///
/// @author kongweiguang
fn routed_target(temporary: Arc<TemporaryTargets>, id: &str) -> ExternalRoutedTarget {
    ExternalRoutedTarget {
        lease: ExternalTargetLease::new_registered(HostId::parse(id).unwrap(), temporary).unwrap(),
        identity: ExternalTargetIdentity {
            protocol: RemoteProtocol::Ssh,
            endpoint: HostEndpoint {
                host: "example.invalid".into(),
                port: 22,
                username: "tester".into(),
            },
            jump_hosts: Vec::new(),
        },
        classification: ExternalTargetClassification::RestrictedUnknown,
        intent: ExternalLaunchIntent::SshTerminal {
            remote_command: vec!["printf".into(), "two words".into()],
        },
        display_name: None,
        authentication_required: false,
        password_retry_available: true,
        open_sftp_after_connect: false,
    }
}

/// 保存一个失败的原 pane、目标与代次，避免用当前焦点或新 Tab 推测重试位置。
///
/// @author kongweiguang
fn failed_pane(
    root: &mut RootView,
    target: &ExternalRoutedTarget,
) -> (TabId, PaneId, u64, SessionTarget) {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let host_id = target.host_id().clone();
    let session_target = SessionTarget::Ssh(SshSessionTarget {
        host_id: host_id.clone(),
        working_directory: None,
        command: Some(SshInitialCommand::Process {
            program: "printf".into(),
            args: vec!["two words".into()],
        }),
    });
    root.workspace_runtime
        .register_pending_pane(
            tab_id.clone(),
            WorkspaceSessionPane {
                id: pane_id.clone(),
                title: "external".into(),
                machine_id: host_id.as_str().into(),
                mode: WorkspacePaneMode::Ssh,
                target: Some(WorkspaceTargetRef::Ssh {
                    host_id: host_id.clone(),
                }),
                remote_host_id: Some(host_id),
                container_id: None,
                profile_id: None,
                shell: None,
                args: None,
                cwd: None,
                current_cwd: None,
                env: None,
                remote_command: None,
                tmux_binding: None,
                prompt: String::new(),
                status: WorkspaceMachineStatus::Offline,
                latency_ms: None,
                local_machine_scope: None,
                output_history: None,
            },
        )
        .unwrap();
    root.workspace_runtime
        .set_restore_target(&pane_id, session_target.clone())
        .unwrap();
    root.workspace_runtime
        .set_resource_owner(&pane_id, Some(target.lease.clone()), true)
        .unwrap();
    root.workspace_runtime
        .mark_restore_failed(&pane_id)
        .unwrap();
    let (generation, _) = root.begin_workspace_restore(&pane_id);
    root.external_ssh_attempts.insert(
        pane_id.clone(),
        ExternalSshAttempt {
            target: target.clone(),
            generation,
        },
    );
    (tab_id, pane_id, generation, session_target)
}

/// 保证关闭、清理挂起与网络失败不会在迟到回执中打开密码提示。
///
/// @author kongweiguang
#[test]
fn password_retry_requires_authentication_failure_and_live_reclaimed_target() {
    assert!(should_offer_password_retry(
        PublicErrorCode::AuthenticationFailed,
        false,
        true,
        false
    ));
    assert!(!should_offer_password_retry(
        PublicErrorCode::Timeout,
        false,
        true,
        false
    ));
    assert!(!should_offer_password_retry(
        PublicErrorCode::Cancelled,
        false,
        true,
        false
    ));
    assert!(!should_offer_password_retry(
        PublicErrorCode::AuthenticationFailed,
        true,
        true,
        false
    ));
    assert!(!should_offer_password_retry(
        PublicErrorCode::AuthenticationFailed,
        false,
        false,
        false
    ));
    assert!(!should_offer_password_retry(
        PublicErrorCode::AuthenticationFailed,
        false,
        true,
        true
    ));
}

/// 只有当前认证失败打开真实密码提示，成功回执必须保留原 pane 的 owner、目标和 argv。
///
/// @author kongweiguang
#[gpui_kit::test]
fn authentication_failure_offers_password_and_retries_original_pane(app: &mut TestAppContext) {
    let (runtime, root, sessions, temporary, visual) = fixture(app);
    settle(&runtime, visual);
    let target = routed_target(temporary.clone(), "external:auth-retry");
    let other = routed_target(temporary.clone(), "external:other");
    let (tab_id, pane_id, session_target) = visual.update(|window, app| {
        root.update(app, |root, cx| {
            let (tab_id, pane_id, generation, session_target) = failed_pane(root, &target);
            root.offer_external_password_retry(
                &pane_id,
                generation + 1,
                PublicErrorCode::AuthenticationFailed,
                false,
                cx,
            );
            assert!(root.external_ssh_attempts.contains_key(&pane_id));
            assert!(root.pending_external_password_retry.is_none());
            assert!(
                !root
                    .connections_panel
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .root_confirmation_active()
            );

            root.offer_external_password_retry(
                &pane_id,
                generation,
                PublicErrorCode::Timeout,
                false,
                cx,
            );
            assert!(!root.external_ssh_attempts.contains_key(&pane_id));
            assert!(root.pending_external_password_retry.is_none());
            assert!(
                !root
                    .connections_panel
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .root_confirmation_active()
            );

            root.external_ssh_attempts.insert(
                pane_id.clone(),
                ExternalSshAttempt {
                    target: target.clone(),
                    generation,
                },
            );
            root.offer_external_password_retry(
                &pane_id,
                generation,
                PublicErrorCode::AuthenticationFailed,
                false,
                cx,
            );
            assert!(
                root.connections_panel
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .root_confirmation_active()
            );
            assert!(root.pending_external_password_retry.is_some());
            assert_eq!(
                root.resume_external_password_retry(&other, window, cx),
                None
            );
            assert!(root.pending_external_password_retry.is_some());
            assert_eq!(
                root.resume_external_password_retry(&target, window, cx),
                Some(true)
            );
            assert!(root.pending_external_password_retry.is_none());
            assert_eq!(root.workspace_runtime.panes().count(), 1);
            (tab_id, pane_id, session_target)
        })
    });
    settle(&runtime, visual);
    let creates = sessions.creates.lock().unwrap();
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].owner, SessionOwner::Ui { tab_id, pane_id });
    assert_eq!(creates[0].target, session_target);
    assert_eq!(*temporary.retains.lock().unwrap(), 1);
}

/// 已提交的密码再次认证失败时重新提示，但取消和关闭必须释放目标且不能自动重连。
///
/// @author kongweiguang
#[gpui_kit::test]
fn failed_password_retry_prompts_again_and_cancel_releases_target(app: &mut TestAppContext) {
    let (runtime, root, sessions, temporary, visual) = fixture(app);
    settle(&runtime, visual);
    let target = routed_target(temporary.clone(), "external:password-retry-again");
    let host_id = target.host_id().clone();
    let (tab_id, pane_id, session_target) = visual.update(|window, app| {
        root.update(app, |root, cx| {
            let (tab_id, pane_id, generation, session_target) = failed_pane(root, &target);
            // 票据代表原 Agent 尝试已获得密码回退资格，并且用户已提交口令、通过预检。
            root.external_ssh_attempts.remove(&pane_id);
            root.pending_external_password_retry = Some(ExternalPasswordRetryTicket {
                host_id: host_id.clone(),
                pane_id: pane_id.clone(),
                generation,
            });
            let mut submitted = target.clone();
            submitted.password_retry_available = false;
            submitted.authentication_required = true;
            assert_eq!(
                root.resume_external_password_retry(&submitted, window, cx),
                Some(true)
            );
            let attempt = root
                .external_ssh_attempts
                .get(&pane_id)
                .expect("explicit retry is tracked");
            assert_ne!(attempt.generation, generation);
            assert!(attempt.target.password_retry_available);
            (tab_id, pane_id, session_target)
        })
    });
    settle(&runtime, visual);
    visual.update(|_, cx| {
        let root = root.read(cx);
        let ticket = root
            .pending_external_password_retry
            .as_ref()
            .expect("failed password prompts again");
        assert_eq!(ticket.host_id, host_id);
        assert_eq!(ticket.pane_id, pane_id);
        assert!(
            root.connections_panel
                .as_ref()
                .unwrap()
                .read(cx)
                .root_confirmation_active()
        );
        assert!(!root.external_ssh_attempts.contains_key(&pane_id));
        assert_eq!(root.workspace_runtime.panes().count(), 1);
    });
    settle(&runtime, visual);
    {
        let creates = sessions.creates.lock().unwrap();
        assert_eq!(
            creates.len(),
            1,
            "a failed password must not trigger another connection"
        );
        assert_eq!(
            creates[0].owner,
            SessionOwner::Ui {
                tab_id,
                pane_id: pane_id.clone()
            }
        );
        assert_eq!(creates[0].target, session_target);
    }
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let panel = root.connections_panel.as_ref().unwrap().clone();
            panel.update(cx, |panel, cx| panel.cancel_root_confirmation(window, cx));
        });
    });
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            assert!(root.pending_external_password_retry.is_none());
            assert_eq!(
                root.resume_external_password_retry(&target, window, cx),
                None
            );
            root.cancel_workspace_restore(&pane_id);
            root.workspace_runtime.remove_empty_pane(&pane_id).unwrap();
        });
    });
    drop(target);
    settle(&runtime, visual);
    assert_eq!(sessions.creates.lock().unwrap().len(), 1);
    assert_eq!(*temporary.retains.lock().unwrap(), 1);
    assert_eq!(
        temporary
            .releases
            .lock()
            .unwrap()
            .iter()
            .filter(|id| **id == host_id)
            .count(),
        2
    );
}

/// 关闭、换代和退出后迟到的口令回执只能消费 ticket，不能获取新 lease 或创建会话。
///
/// @author kongweiguang
#[gpui_kit::test]
fn stale_password_receipts_do_not_retry_or_create_targets(app: &mut TestAppContext) {
    let (runtime, root, sessions, temporary, visual) = fixture(app);
    settle(&runtime, visual);
    let target = routed_target(temporary.clone(), "external:stale-retry");
    for case in 0..3 {
        visual.update(|window, app| {
            root.update(app, |root, cx| {
                let (_, pane_id, generation, _) = failed_pane(root, &target);
                root.offer_external_password_retry(
                    &pane_id,
                    generation,
                    PublicErrorCode::AuthenticationFailed,
                    false,
                    cx,
                );
                assert!(
                    root.connections_panel
                        .as_ref()
                        .unwrap()
                        .read(cx)
                        .root_confirmation_active()
                );
                match case {
                    0 => {
                        root.begin_workspace_restore(&pane_id);
                    }
                    1 => {
                        root.cancel_workspace_restore(&pane_id);
                        root.workspace_runtime.remove_empty_pane(&pane_id).unwrap();
                    }
                    _ => {
                        root.host_shutdown_requested = true;
                    }
                }
                let pane_count = root.workspace_runtime.panes().count();
                assert_eq!(
                    root.resume_external_password_retry(&target, window, cx),
                    Some(true)
                );
                assert!(root.pending_external_password_retry.is_none());
                assert_eq!(root.workspace_runtime.panes().count(), pane_count);
                assert!(root.status.is_some());
                assert_eq!(*temporary.retains.lock().unwrap(), 0);
                root.host_shutdown_requested = false;
                if root.workspace_runtime.pane(&pane_id).is_some() {
                    root.cancel_workspace_restore(&pane_id);
                    root.workspace_runtime.remove_empty_pane(&pane_id).unwrap();
                }
                let panel = root.connections_panel.as_ref().unwrap().clone();
                panel.update(cx, |panel, cx| panel.cancel_root_confirmation(window, cx));
            });
        });
    }
    settle(&runtime, visual);
    assert!(sessions.creates.lock().unwrap().is_empty());
}

/// 第二次认证失败必须等待首个提示关闭，且只为原失败 pane 接管下一次密码输入。
///
/// @author kongweiguang
#[gpui_kit::test]
fn simultaneous_authentication_failures_are_prompted_in_order(app: &mut TestAppContext) {
    let (runtime, root, sessions, temporary, visual) = fixture(app);
    settle(&runtime, visual);
    let first = routed_target(temporary.clone(), "external:queue-first");
    let second = routed_target(temporary.clone(), "external:queue-second");
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let (_, first_pane, first_generation, _) = failed_pane(root, &first);
            let (_, second_pane, second_generation, _) = failed_pane(root, &second);
            root.offer_external_password_retry(
                &first_pane,
                first_generation,
                PublicErrorCode::AuthenticationFailed,
                false,
                cx,
            );
            root.offer_external_password_retry(
                &second_pane,
                second_generation,
                PublicErrorCode::AuthenticationFailed,
                false,
                cx,
            );
            assert_eq!(
                root.pending_external_password_retry
                    .as_ref()
                    .unwrap()
                    .host_id,
                *first.host_id()
            );
            assert_eq!(root.pending_external_password_offers.len(), 1);
            assert_eq!(
                root.pending_external_password_offers.front(),
                Some(&(second_pane.clone(), second_generation))
            );
            assert!(root.external_ssh_attempts.contains_key(&second_pane));

            root.resume_queued_external_password_offer(cx);
            assert_eq!(
                root.pending_external_password_retry
                    .as_ref()
                    .unwrap()
                    .host_id,
                *first.host_id()
            );
            assert_eq!(root.pending_external_password_offers.len(), 1);
            let panel = root.connections_panel.as_ref().unwrap().clone();
            panel.update(cx, |panel, cx| panel.cancel_root_confirmation(window, cx));
            root.resume_queued_external_password_offer(cx);

            let ticket = root
                .pending_external_password_retry
                .as_ref()
                .expect("second prompt accepted");
            assert_eq!(ticket.host_id, *second.host_id());
            assert_eq!(ticket.pane_id, second_pane);
            assert_eq!(ticket.generation, second_generation);
            assert!(panel.read(cx).root_confirmation_active());
            assert!(root.pending_external_password_offers.is_empty());
            assert!(!root.external_ssh_attempts.contains_key(&second_pane));
        });
    });
    settle(&runtime, visual);
    assert!(sessions.creates.lock().unwrap().is_empty());
}

/// 排队期间关闭原 pane 要立即释放其临时目标，首个提示结束后不得重新打开已关闭目标。
///
/// @author kongweiguang
#[gpui_kit::test]
fn closing_queued_pane_releases_target_and_skips_its_prompt(app: &mut TestAppContext) {
    let (runtime, root, sessions, temporary, visual) = fixture(app);
    settle(&runtime, visual);
    let first = routed_target(temporary.clone(), "external:close-first");
    let second = routed_target(temporary.clone(), "external:close-queued");
    let second_host = second.host_id().clone();
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let (_, first_pane, first_generation, _) = failed_pane(root, &first);
            let (_, second_pane, second_generation, _) = failed_pane(root, &second);
            root.offer_external_password_retry(
                &first_pane,
                first_generation,
                PublicErrorCode::AuthenticationFailed,
                false,
                cx,
            );
            root.offer_external_password_retry(
                &second_pane,
                second_generation,
                PublicErrorCode::AuthenticationFailed,
                false,
                cx,
            );
            assert_eq!(root.pending_external_password_offers.len(), 1);
            drop(second);
            assert!(!temporary.releases.lock().unwrap().contains(&second_host));

            root.cancel_workspace_restore(&second_pane);
            root.workspace_runtime
                .remove_empty_pane(&second_pane)
                .unwrap();
            assert!(root.pending_external_password_offers.is_empty());
            assert!(!root.external_ssh_attempts.contains_key(&second_pane));
            assert_eq!(
                temporary
                    .releases
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|id| **id == second_host)
                    .count(),
                1
            );

            let panel = root.connections_panel.as_ref().unwrap().clone();
            panel.update(cx, |panel, cx| panel.cancel_root_confirmation(window, cx));
            root.resume_queued_external_password_offer(cx);
            assert!(!panel.read(cx).root_confirmation_active());
            assert_eq!(root.workspace_runtime.panes().count(), 1);
            assert_eq!(*temporary.retains.lock().unwrap(), 0);
        });
    });
    settle(&runtime, visual);
    assert!(sessions.creates.lock().unwrap().is_empty());
}

/// 被新的恢复代次取代的队列项不能再弹提示，也不能阻塞后续仍有效的请求。
///
/// @author kongweiguang
#[gpui_kit::test]
fn queued_offer_skips_replaced_generation_and_continues(app: &mut TestAppContext) {
    let (runtime, root, sessions, temporary, visual) = fixture(app);
    settle(&runtime, visual);
    let first = routed_target(temporary.clone(), "external:stale-queue-first");
    let stale = routed_target(temporary.clone(), "external:stale-queue-old");
    let next = routed_target(temporary.clone(), "external:stale-queue-next");
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let (_, first_pane, first_generation, _) = failed_pane(root, &first);
            let (_, stale_pane, stale_generation, _) = failed_pane(root, &stale);
            let (_, next_pane, next_generation, _) = failed_pane(root, &next);
            for (pane, generation) in [
                (&first_pane, first_generation),
                (&stale_pane, stale_generation),
                (&next_pane, next_generation),
            ] {
                root.offer_external_password_retry(
                    pane,
                    generation,
                    PublicErrorCode::AuthenticationFailed,
                    false,
                    cx,
                );
            }
            assert_eq!(root.pending_external_password_offers.len(), 2);
            root.begin_workspace_restore(&stale_pane);
            let panel = root.connections_panel.as_ref().unwrap().clone();
            panel.update(cx, |panel, cx| panel.cancel_root_confirmation(window, cx));
            root.resume_queued_external_password_offer(cx);

            let ticket = root
                .pending_external_password_retry
                .as_ref()
                .expect("next live prompt accepted");
            assert_eq!(ticket.host_id, *next.host_id());
            assert_eq!(ticket.pane_id, next_pane);
            assert_eq!(ticket.generation, next_generation);
            assert!(root.pending_external_password_offers.is_empty());
            assert!(!root.external_ssh_attempts.contains_key(&stale_pane));
            assert!(panel.read(cx).root_confirmation_active());
            assert_eq!(*temporary.retains.lock().unwrap(), 0);
        });
    });
    settle(&runtime, visual);
    assert!(sessions.creates.lock().unwrap().is_empty());
}

/// 用拒绝 IO 的文件端口挂载真实 Root 桥接，预检通道由哨兵占用以稳定观察左右排队。
///
/// @author kongweiguang
fn file_queue_fixture(
    app: &mut TestAppContext,
) -> (
    Runtime,
    Runtime,
    Entity<RootView>,
    Entity<FilesPane>,
    &mut VisualTestContext,
) {
    let (file_runtime, files, transfers, executor, _) = file_support::services();
    let (runtime, root, _, _, visual) = fixture(app);
    let pane_slot = Rc::new(RefCell::new(None));
    let pane_slot_for_mount = pane_slot.clone();
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let initial = FileEndpoint::Local {
                path: "C:/fixture".into(),
            };
            root.services.files = Some(files.clone());
            root.services.transfers = Some(transfers.clone());
            let pane = cx.new(|_| {
                FilesPane::new(
                    files,
                    transfers,
                    executor,
                    FileTargetNameLookup::default(),
                    initial,
                )
            });
            pane.update(cx, |pane, cx| pane.set_host_picker_enabled(true, cx));
            root.files_pane = Some(pane.clone());
            root.center_panel = CenterPanel::Files;
            root.files_workspace_selection = FilesWorkspaceSelection::Workbench;
            root.sftp_workbench_tab_open = true;
            root._files_pane_subscriptions =
                Some(RootView::subscribe_files_pane_events(&pane, window, cx));
            root.active_file_sftp_selection = Some(FileSftpSelectionRequest {
                request_id: "fixture-busy".to_owned(),
                host_id: HostId::new(),
                origin: FileSftpOrigin {
                    source: FilesWorkspaceSelection::Workbench,
                    pane_id: 0,
                    side: FilePaneSide::Left,
                    generation: 0,
                    navigation_generation: root.surface_navigation_generation,
                },
            });
            *pane_slot_for_mount.borrow_mut() = Some(pane);
        });
    });
    let pane = pane_slot.borrow_mut().take().expect("files pane attached");
    (runtime, file_runtime, root, pane, visual)
}

/// 隔离的真实 Root 使用本机端点和拒绝 IO 的服务，保证文件导航测试不访问用户文件。
///
/// @author kongweiguang
fn compact_editor_fixture(
    app: &mut TestAppContext,
) -> (
    Runtime,
    Runtime,
    Entity<RootView>,
    Entity<FilesPane>,
    FileEndpoint,
    kerminal_domain::files::FileEntry,
    &mut VisualTestContext,
) {
    use crate::features::editor::FileEditorWorkspace;
    use gpui_kit::{px, size};
    use kerminal_application::{files::editor::FileEditorService, preferences::ToolRailToolId};
    use kerminal_domain::files::{FileEntry, FileEntryKind};

    let (runtime, file_runtime, root, pane, visual) = file_queue_fixture(app);
    let directory = FileEndpoint::Local {
        path: "C:/fixture".into(),
    };
    let initial = FileEndpoint::Local {
        path: "C:/fixture/initial.txt".into(),
    };
    let entry = FileEntry {
        endpoint: FileEndpoint::Local {
            path: "C:/fixture/next.txt".into(),
        },
        name: "next.txt".to_owned(),
        kind: FileEntryKind::File,
        size: None,
        modified: None,
        permissions: None,
        is_hidden: false,
        raw: None,
    };
    visual.simulate_resize(size(px(800.), px(700.)));
    visual.update(|window, _| window.activate_window());
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.active_file_sftp_selection = None;
            root.services.initial_file_endpoint = Some(directory.clone());
            let service = FileEditorService::new(root.services.files.clone().unwrap());
            let executor = root.services.executor.clone();
            root.file_editor_workspace = Some(cx.new(|cx| {
                FileEditorWorkspace::new(
                    service,
                    executor,
                    FileTargetNameLookup::default(),
                    window,
                    cx,
                )
            }));
            let controller = WorkspaceController::from_missing().unwrap();
            root.workspace_files = WorkspaceFileRuntime::from_snapshot(controller.snapshot());
            root.workspace_controller = Some(controller);
            assert!(root.open_file_endpoint_with_lease(initial, None, window, cx));
            root.reconcile_tab_tool_contents(window, cx);
            root.files_pane = None;
            root.sftp_panel = Some(pane.clone());
            root._sftp_panel_subscriptions = root._files_pane_subscriptions.take();
            root.sftp_workbench_tab_open = false;
        });
        pane.update(app, |pane, cx| {
            pane.set_host_picker_enabled(false, cx);
            pane.set_compact_layout(true, cx);
            file_support::seed_compact_listing(pane, directory.clone(), vec![entry.clone()]);
        });
        window.draw(app).clear(app);
        root.update(app, |root, cx| {
            root.show_compact_tool_panel(ToolRailToolId::Sftp, window, cx);
        });
        window.draw(app).clear(app);
    });
    (runtime, file_runtime, root, pane, directory, entry, visual)
}

/// 从真实组件的可访问快照取命中位置，确保回归经过生产按钮与 Root 订阅。
///
/// @author kongweiguang
fn click_root_button(visual: &mut VisualTestContext, selector: &str) {
    let id: gpui_kit::ElementId = selector.to_owned().into();
    let target = visual.update(|window, app| {
        window.draw(app).clear(app);
        gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|node| node.visible() && node.path().contains(&id))
            .min_by_key(|node| node.path().len())
            .unwrap_or_else(|| panic!("button {selector} is not visible"))
            .bounds()
            .center()
    });
    visual.simulate_click(target, Default::default());
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
}

/// 点击真实标签的正文命中区域，避免把关闭按钮或直接发事件当作导航验收。
///
/// @author kongweiguang
fn click_root_tab(visual: &mut VisualTestContext, tab_id: &TabId) {
    let selector = format!("workspace-tab-hitbox-{}", tab_id.as_str());
    let bounds = visual.update(|window, app| {
        window.draw(app).clear(app);
        window
            .debug_selector_bounds(&selector)
            .expect("workspace tab hitbox")
    });
    visual.simulate_click(bounds.center(), Default::default());
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
}

/// 共享工作台与右栏必须持有不同实体，避免测试通过重复挂载掩盖真实宿主切换。
///
/// @author kongweiguang
fn attach_fixture_shared_files(root: &Entity<RootView>, visual: &mut VisualTestContext) {
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let pane = RootView::new_files_panel(&root.services, cx).expect("shared files pane");
            pane.update(cx, |pane, cx| pane.set_host_picker_enabled(true, cx));
            root._files_pane_subscriptions =
                Some(RootView::subscribe_files_pane_events(&pane, window, cx));
            root.files_pane = Some(pane);
        });
    });
}

/// 复用带文件抽屉的真实 Root，注册独立传输标签但不预先激活其工具栏状态。
///
/// @author kongweiguang
fn compact_transfer_fixture(
    app: &mut TestAppContext,
    files_available: bool,
) -> (
    Runtime,
    Runtime,
    Entity<RootView>,
    TabId,
    TabId,
    &mut VisualTestContext,
) {
    let (runtime, file_runtime, root, _, _, _, visual) = compact_editor_fixture(app);
    attach_fixture_shared_files(&root, visual);
    let transfer_id = TabId::new();
    let editor_id = visual.update(|_, app| {
        root.update(app, |root, cx| {
            if !files_available {
                root.services.files = None;
            }
            let host_id = HostId::new();
            root.services.hosts = Some(HostManagementService::new(Arc::new(EmptyHosts {
                saved: vec![saved_sftp_host(host_id.clone())],
            })));
            let tab = sftp_transfer_tab_for_host(
                transfer_id.clone(),
                host_id,
                None,
                &kerminal_domain::files::RemotePath::parse("/fixture").unwrap(),
                "Transfer",
            );
            assert!(
                root.workspace_files
                    .register_pending_sftp_transfer_tab(tab.clone())
            );
            let controller = root.workspace_controller.as_mut().unwrap();
            let mut snapshot = controller.snapshot().clone();
            let editor_id = snapshot.active_tab_id.clone().unwrap();
            snapshot.terminal_tabs.push(tab);
            controller.synchronize_snapshot(snapshot).unwrap();
            root.refresh_workspace_views(cx);
            editor_id
        })
    });
    (runtime, file_runtime, root, editor_id, transfer_id, visual)
}

/// 通过真实命中与键盘输入验证中央面板拥有搜索，而非只检查控件的绘制 bounds。
///
/// @author kongweiguang
fn assert_transfer_host_search_receives_input(visual: &mut VisualTestContext, value: &str) {
    let bounds = visual
        .debug_bounds("files-sftp-host-picker-toggle-0")
        .expect("central left host search is mounted");
    visual.simulate_mouse_move(bounds.center(), None, Default::default());
    visual.simulate_click(bounds.center(), Default::default());
    visual.update(|window, app| window.draw(app).clear(app));
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input(value);
    visual.run_until_parked();
    visual.update(|window, app| {
        window.draw(app).clear(app);
        let id: gpui_kit::ElementId = "files-sftp-host-picker-input-0".into();
        let values = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|node| node.path().contains(&id))
            .filter_map(|node| node.value().map(str::to_owned))
            .collect::<Vec<_>>();
        assert!(
            values.iter().any(|actual| actual == value),
            "values: {values:?}"
        );
    });
}

/// 首次进入传输标签时继承的文件抽屉不能遮住中央主机搜索，目录结果未到也可交互。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_transfer_tab_inherited_drawer_does_not_block_host_search(app: &mut TestAppContext) {
    let (_runtime, _file_runtime, root, _, transfer_id, visual) =
        compact_transfer_fixture(app, true);
    click_root_button(visual, "workbench-toggle-host-sidebar");
    click_root_button(visual, "host-navigation-sftp-workbench");
    assert!(root.read_with(visual, |root, _| !root.compact_tool_drawer_open));
    click_root_tab(visual, &transfer_id);
    visual.update(|_, app| {
        let root = root.read(app);
        assert_eq!(root.center_panel, CenterPanel::Files);
        assert_eq!(
            root.files_workspace_selection,
            FilesWorkspaceSelection::TransferTab(transfer_id.clone())
        );
        assert!(root.workspace_files.files_pane(&transfer_id).is_some());
    });
    assert_transfer_host_search_receives_input(visual, "fixture");
    assert!(root.read_with(visual, |root, _| !root.compact_tool_drawer_open));
}

/// 已访问标签恢复自身的打开顺序时同样要让中央面板可命中，手动打开工具仍然有效。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_transfer_tab_cached_drawer_does_not_block_host_search(app: &mut TestAppContext) {
    let (_runtime, _file_runtime, root, editor_id, transfer_id, visual) =
        compact_transfer_fixture(app, true);
    click_root_tab(visual, &transfer_id);
    if !root.read_with(visual, |root, _| root.compact_tool_drawer_open) {
        click_root_button(visual, "workbench-tool-sftp");
    }
    assert!(root.read_with(visual, |root, _| root.compact_tool_drawer_open));
    click_root_tab(visual, &editor_id);
    click_root_tab(visual, &transfer_id);
    assert_transfer_host_search_receives_input(visual, "cached");
    assert!(root.read_with(visual, |root, _| !root.compact_tool_drawer_open));
    click_root_button(visual, "workbench-tool-sftp");
    assert!(root.read_with(visual, |root, _| root.compact_tool_drawer_open));
}

/// 面板已受理即显示双端，即使隔离 provider 随后拒绝目录读取也保留可操作的搜索与重试入口。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_transfer_tab_directory_failure_keeps_center_accessible(app: &mut TestAppContext) {
    let (runtime, _file_runtime, root, _, transfer_id, visual) =
        compact_transfer_fixture(app, true);
    click_root_tab(visual, &transfer_id);
    visual.update(|_, app| {
        let root = root.read(app);
        assert!(matches!(
            root.workspace_files.restore_state(&transfer_id),
            Some(crate::root::workspace_files::WorkspaceFileRestoreState::Ready)
        ));
        assert!(root.workspace_files.files_pane(&transfer_id).is_some());
        assert!(!root.compact_tool_drawer_open);
    });
    settle(&runtime, visual);
    visual.update(|window, app| window.draw(app).clear(app));
    assert_transfer_host_search_receives_input(visual, "retry");
    assert!(root.read_with(visual, |root, _| !root.compact_tool_drawer_open));
    #[cfg(feature = "right-rail-acceptance")]
    let pane = root
        .read_with(visual, |root, _| {
            root.workspace_files.files_pane(&transfer_id)
        })
        .unwrap();
    #[cfg(feature = "right-rail-acceptance")]
    assert!(visual.update(|_, app| {
        pane.read(app)
            .transfer_workspace_acceptance_endpoint(0)
            .unwrap()
            .2
            .is_some()
    }));
    visual.simulate_keystrokes("escape");
    visual.update(|window, app| window.draw(app).clear(app));
    click_root_button(visual, "files-refresh-0");
    #[cfg(feature = "right-rail-acceptance")]
    assert!(visual.update(|_, app| {
        pane.read(app)
            .transfer_workspace_acceptance_endpoint(0)
            .unwrap()
            .2
            .is_none()
    }));
}

/// 文件服务缺失导致面板未受理时，不能借导航动作收起原来的工具抽屉。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_transfer_tab_unavailable_keeps_drawer(app: &mut TestAppContext) {
    let (_runtime, _file_runtime, root, _, transfer_id, visual) =
        compact_transfer_fixture(app, false);
    click_root_tab(visual, &transfer_id);
    visual.update(|_, app| {
        let root = root.read(app);
        assert!(matches!(
            root.workspace_files.restore_state(&transfer_id),
            Some(crate::root::workspace_files::WorkspaceFileRestoreState::Failed { .. })
        ));
        assert!(root.workspace_files.files_pane(&transfer_id).is_none());
        assert!(root.compact_tool_drawer_open);
    });
}

/// 宽窗激活独立传输标签继续保留原 Dock 状态，避免把窄窗遮罩语义写入停靠布局。
///
/// @author kongweiguang
#[gpui_kit::test]
fn wide_transfer_tab_keeps_docked_tool(app: &mut TestAppContext) {
    use gpui_kit::{px, size};
    use kerminal_application::preferences::ToolRailToolId;

    let (_runtime, _file_runtime, root, _, transfer_id, visual) =
        compact_transfer_fixture(app, true);
    visual.simulate_resize(size(px(1500.), px(700.)));
    let was_collapsed = visual.update(|window, app| {
        window.draw(app).clear(app);
        root.read(app).right_dock_collapsed
    });
    click_root_tab(visual, &transfer_id);
    visual.update(|_, app| {
        root.update(app, |root, _| {
            assert!(!root.tool_rail_rendered_compact.0);
            assert_eq!(root.right_dock_collapsed, was_collapsed);
            let settings = root.current_tool_rail_settings();
            assert_eq!(
                root.tool_rail_state.project(&settings).panels.attached,
                Some(ToolRailToolId::Sftp)
            );
        });
    });
}

/// 上层设置模态持有焦点时，迟到的标签激活不能卸载原抽屉或移动模态焦点。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_transfer_tab_modal_does_not_take_drawer_focus(app: &mut TestAppContext) {
    use crate::features::workspace_tabs::WorkspaceTabEvent;

    let (_runtime, _file_runtime, root, _, transfer_id, visual) =
        compact_transfer_fixture(app, true);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.open_preferences_modal(window, cx);
            let event = WorkspaceTabEvent::Activate(transfer_id.clone());
            assert!(root.handle_workspace_tab_event(&event, window, cx));
            root.activate_workspace_tab_surface(&transfer_id, true, window, cx);
        });
        window.draw(app).clear(app);
        let root = root.read(app);
        assert!(root.preferences_modal_open);
        assert!(root.compact_tool_drawer_open);
        assert!(
            root.preferences_modal_focus_handle
                .contains_focused(window, app)
        );
    });
}

/// 关闭回执等后台导航不应因恢复旧工具抽屉而夺走已经属于正文的焦点。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_transfer_tab_background_activation_does_not_steal_focus(app: &mut TestAppContext) {
    use crate::features::workspace_tabs::WorkspaceTabEvent;

    let (_runtime, _file_runtime, root, _, transfer_id, visual) =
        compact_transfer_fixture(app, true);
    click_root_button(visual, "workbench-toggle-host-sidebar");
    click_root_button(visual, "host-navigation-sftp-workbench");
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            window.focus(&root.focus_handle, cx);
            let event = WorkspaceTabEvent::Activate(transfer_id.clone());
            assert!(root.handle_workspace_tab_event(&event, window, cx));
            root.activate_workspace_tab_surface(&transfer_id, false, window, cx);
        });
        window.draw(app).clear(app);
        let root = root.read(app);
        assert!(root.focus_handle.is_focused(window));
        assert!(!root.compact_tool_drawer_open);
        assert_eq!(root.center_panel, CenterPanel::Files);
    });
}

/// 顶部入口必须让中央传输台在下一帧保持可见，旧标签的 SFTP 投影不能夺回焦点。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_host_sftp_workbench_reveals_center_and_can_reopen_tool(app: &mut TestAppContext) {
    let (_runtime, _file_runtime, root, _, _, _, visual) = compact_editor_fixture(app);
    attach_fixture_shared_files(&root, visual);
    assert!(root.read_with(visual, |root, _| root.compact_tool_drawer_open));
    click_root_button(visual, "workbench-toggle-host-sidebar");
    visual.update(|_, app| {
        let root = root.read(app);
        assert!(root.compact_host_drawer_open);
        assert!(!root.compact_tool_drawer_open);
    });
    click_root_button(visual, "host-navigation-sftp-workbench");
    visual.update(|window, app| {
        let root = root.read(app);
        assert!(root.shared_sftp_workbench_is_selected());
        assert!(!root.compact_host_drawer_open);
        assert!(!root.compact_tool_drawer_open, "old SFTP drawer returned");
        assert!(
            !root
                .compact_drawer_focus_handle
                .contains_focused(window, app)
        );
        assert!(root.focus_handle.is_focused(window));
    });
    click_root_button(visual, "workbench-tool-sftp");
    assert!(root.read_with(visual, |root, _| root.compact_tool_drawer_open));
}

/// 已存在的共享标签和顶部主机入口必须使用同一个抽屉交接；标签已有焦点时不能重新抢回工作区根。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_existing_sftp_workbench_tab_reveals_center(app: &mut TestAppContext) {
    let (_runtime, _file_runtime, root, _, _, _, visual) = compact_editor_fixture(app);
    attach_fixture_shared_files(&root, visual);
    let editor_tab = root.read_with(visual, |root, _| {
        root.workspace_controller
            .as_ref()
            .unwrap()
            .snapshot()
            .active_tab_id
            .clone()
            .unwrap()
    });
    click_root_button(visual, "workbench-toggle-host-sidebar");
    click_root_button(visual, "host-navigation-sftp-workbench");
    let workbench_tab = root.read_with(visual, |root, _| root.sftp_workbench_tab_id.clone());
    click_root_tab(visual, &editor_tab);
    assert!(root.read_with(visual, |root, _| root.center_panel == CenterPanel::Editor));
    click_root_button(visual, "workbench-tool-sftp");
    assert!(root.read_with(visual, |root, _| root.compact_tool_drawer_open));

    click_root_tab(visual, &workbench_tab);
    visual.update(|window, app| {
        let root = root.read(app);
        assert!(root.shared_sftp_workbench_is_selected());
        assert!(!root.compact_host_drawer_open);
        assert!(!root.compact_tool_drawer_open);
        assert!(window.focused(app).is_some());
        assert!(!root.focus_handle.is_focused(window));
        assert!(
            !root
                .compact_drawer_focus_handle
                .contains_focused(window, app)
        );
    });
    click_root_button(visual, "workbench-tool-sftp");
    assert!(root.read_with(visual, |root, _| root.compact_tool_drawer_open));
}

/// 没有活动标签时仍使用真实 Root 的空标签投影，覆盖 controller 缺失与已创建两种窗口状态。
///
/// @author kongweiguang
fn assert_compact_host_workbench_without_active_tab(app: &mut TestAppContext, controller: bool) {
    use gpui_kit::{px, size};
    use kerminal_application::preferences::ToolRailToolId;

    let (_runtime, _file_runtime, root, _, visual) = file_queue_fixture(app);
    visual.simulate_resize(size(px(800.), px(700.)));
    visual.update(|window, _| window.activate_window());
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.active_file_sftp_selection = None;
            root.services.initial_file_endpoint = Some(FileEndpoint::Local {
                path: "C:/fixture".into(),
            });
            root.center_panel = CenterPanel::Terminal;
            root.sftp_workbench_tab_open = false;
            if controller {
                let workspace = WorkspaceController::from_missing().unwrap();
                root.workspace_files = WorkspaceFileRuntime::from_snapshot(workspace.snapshot());
                root.workspace_controller = Some(workspace);
            } else {
                root.workspace_controller = None;
            }
            let pane = RootView::new_files_panel(&root.services, cx).expect("SFTP pane");
            pane.update(cx, |pane, cx| pane.set_compact_layout(true, cx));
            root._sftp_panel_subscriptions =
                Some(RootView::subscribe_files_pane_events(&pane, window, cx));
            root.sftp_panel = Some(pane);
            root.show_compact_tool_panel(ToolRailToolId::Sftp, window, cx);
        });
        window.draw(app).clear(app);
    });
    assert!(root.read_with(visual, |root, _| root.compact_tool_drawer_open));
    click_root_button(visual, "workbench-toggle-host-sidebar");
    assert!(root.read_with(visual, |root, _| root.compact_host_drawer_open));
    click_root_button(visual, "host-navigation-sftp-workbench");
    visual.update(|window, app| {
        let root = root.read(app);
        assert!(root.shared_sftp_workbench_is_selected());
        assert!(!root.compact_host_drawer_open);
        assert!(!root.compact_tool_drawer_open);
        assert!(root.focus_handle.is_focused(window));
        assert!(
            root.workspace_controller
                .as_ref()
                .is_none_or(|controller| { controller.snapshot().active_tab_id.is_none() })
        );
    });
    click_root_button(visual, "workbench-tool-sftp");
    assert!(root.read_with(visual, |root, _| root.compact_tool_drawer_open));
    let workbench_tab = root.read_with(visual, |root, _| root.sftp_workbench_tab_id.clone());
    click_root_tab(visual, &workbench_tab);
    visual.update(|window, app| {
        let root = root.read(app);
        assert!(root.shared_sftp_workbench_is_selected());
        assert!(!root.compact_tool_drawer_open);
        assert!(
            !root
                .compact_drawer_focus_handle
                .contains_focused(window, app)
        );
    });
    click_root_button(visual, "workbench-tool-sftp");
    assert!(root.read_with(visual, |root, _| root.compact_tool_drawer_open));
}

/// 旧工作台标签仍可见但服务缺失时，激活失败必须保留当前工具抽屉。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_sftp_workbench_tab_unavailable_keeps_tool_drawer(app: &mut TestAppContext) {
    let (_runtime, _file_runtime, root, _, _, _, visual) = compact_editor_fixture(app);
    let workbench_tab = visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.sftp_workbench_tab_open = true;
            root.refresh_workspace_views(cx);
        });
        window.draw(app).clear(app);
        root.read(app).sftp_workbench_tab_id.clone()
    });
    click_root_tab(visual, &workbench_tab);
    visual.update(|_, app| {
        let root = root.read(app);
        assert_eq!(root.center_panel, CenterPanel::Editor);
        assert!(root.status.is_some());
        assert!(root.compact_tool_drawer_open);
        assert!(!root.compact_host_drawer_open);
    });
}

/// 没有 controller 的空工作区也必须能显式关闭旧遮罩并重新打开工具。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_host_sftp_workbench_without_controller(app: &mut TestAppContext) {
    assert_compact_host_workbench_without_active_tab(app, false);
}

/// 已创建但没有活动标签的 controller 与无 controller 使用相同的空标签语义。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_host_sftp_workbench_with_empty_controller(app: &mut TestAppContext) {
    assert_compact_host_workbench_without_active_tab(app, true);
}

/// 文件服务缺失时实际按钮给出错误并保留主机抽屉，避免失败后丢失原操作位置。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_host_sftp_workbench_unavailable_keeps_host_drawer(app: &mut TestAppContext) {
    let (_runtime, _file_runtime, root, _, _, _, visual) = compact_editor_fixture(app);
    click_root_button(visual, "workbench-toggle-host-sidebar");
    click_root_button(visual, "host-navigation-sftp-workbench");
    visual.update(|window, app| {
        let root = root.read(app);
        assert_eq!(root.center_panel, CenterPanel::Editor);
        assert!(root.status.is_some());
        assert!(root.compact_host_drawer_open);
        assert!(!root.compact_tool_drawer_open);
        assert!(
            root.compact_drawer_focus_handle
                .contains_focused(window, app)
        );
    });
}

/// 模态打开后的迟到入口不能收起原抽屉或抢走模态焦点。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_host_sftp_workbench_modal_rejection_keeps_host_drawer(app: &mut TestAppContext) {
    use crate::features::connections::HostNavigationEvent;

    let (_runtime, _file_runtime, root, _, _, _, visual) = compact_editor_fixture(app);
    attach_fixture_shared_files(&root, visual);
    click_root_button(visual, "workbench-toggle-host-sidebar");
    visual.update(|window, app| {
        root.update(app, |root, cx| root.open_preferences_modal(window, cx));
        let navigation = root.read(app).host_navigation.clone().unwrap();
        navigation.update(app, |_, cx| {
            cx.emit(HostNavigationEvent::OpenSftpWorkbenchRequested);
        });
    });
    visual.run_until_parked();
    visual.update(|window, app| {
        window.draw(app).clear(app);
        let root = root.read(app);
        assert_eq!(root.center_panel, CenterPanel::Editor);
        assert!(root.preferences_modal_open);
        assert!(root.compact_host_drawer_open);
        assert!(!root.compact_tool_drawer_open);
        assert!(
            root.preferences_modal_focus_handle
                .contains_focused(window, app)
        );
    });
}

/// 宽窗从固定主机侧栏进入共享传输台时继续保留既有的 SFTP 停靠与折叠状态。
///
/// @author kongweiguang
#[gpui_kit::test]
fn wide_host_sftp_workbench_preserves_docked_tool(app: &mut TestAppContext) {
    use gpui_kit::{px, size};
    use kerminal_application::preferences::ToolRailToolId;

    let (_runtime, _file_runtime, root, _, _, _, visual) = compact_editor_fixture(app);
    attach_fixture_shared_files(&root, visual);
    visual.simulate_resize(size(px(1500.), px(700.)));
    let was_collapsed = visual.update(|window, app| {
        root.update(app, |root, _| root.host_sidebar_collapsed = false);
        window.draw(app).clear(app);
        root.read(app).right_dock_collapsed
    });
    click_root_button(visual, "host-navigation-sftp-workbench");
    visual.update(|_, app| {
        root.update(app, |root, _| {
            assert!(root.shared_sftp_workbench_is_selected());
            assert!(!root.tool_rail_rendered_compact.0);
            assert_eq!(root.right_dock_collapsed, was_collapsed);
            let settings = root.current_tool_rail_settings();
            assert_eq!(
                root.tool_rail_state.project(&settings).panels.attached,
                Some(ToolRailToolId::Sftp)
            );
        });
    });
}

/// 列表打开和菜单复用都以实际目标标签收起窄窗遮罩，手动打开后不会重新出现旧遮罩。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_sftp_file_navigation_reveals_editor_and_can_reopen(app: &mut TestAppContext) {
    use gpui_kit::{Modifiers, MouseButton};
    use kerminal_application::preferences::ToolRailToolId;

    let (_runtime, _file_runtime, root, source, directory, entry, visual) =
        compact_editor_fixture(app);
    let initial_tab = root.read_with(visual, |root, _| {
        root.workspace_controller
            .as_ref()
            .unwrap()
            .snapshot()
            .active_tab_id
            .clone()
            .unwrap()
    });
    let row = visual
        .debug_bounds("files-row-1-next.txt")
        .expect("file row");
    visual.simulate_mouse_move(row.center(), None, Modifiers::none());
    visual.simulate_click(row.center(), Modifiers::none());
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
    let target_tab = visual.update(|window, app| {
        let root = root.read(app);
        assert_eq!(root.center_panel, CenterPanel::Editor);
        assert!(!root.compact_tool_drawer_open);
        assert!(
            !root
                .compact_drawer_focus_handle
                .contains_focused(window, app)
        );
        let controller = root.workspace_controller.as_ref().expect("controller");
        let id = controller
            .snapshot()
            .active_tab_id
            .clone()
            .expect("file tab");
        assert_eq!(
            root.workspace_files.file_endpoint(&id),
            Some(entry.endpoint.clone())
        );
        assert_eq!(controller.snapshot().terminal_tabs.len(), 2);
        id
    });
    visual.update(|_, app| {
        source.update(app, |pane, cx| {
            file_support::request_fixture_open(
                pane,
                FileEndpoint::Local {
                    path: "C:/fixture/stale.txt".into(),
                },
                cx,
            );
        });
    });
    visual.run_until_parked();
    visual.update(|window, app| {
        window.draw(app).clear(app);
        let root = root.read(app);
        let snapshot = root.workspace_controller.as_ref().unwrap().snapshot();
        assert_eq!(snapshot.active_tab_id.as_ref(), Some(&target_tab));
        assert_eq!(snapshot.terminal_tabs.len(), 2);
        assert!(!root.compact_tool_drawer_open);
    });
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.show_compact_tool_panel(ToolRailToolId::Sftp, window, cx);
        });
        window.draw(app).clear(app);
    });
    assert!(root.read_with(visual, |root, _| root.compact_tool_drawer_open));
    let target_pane = root
        .read_with(visual, |root, _| root.sftp_panel.clone())
        .expect("target tab owns its SFTP pane");
    visual.update(|window, app| {
        target_pane.update(app, |pane, cx| {
            file_support::seed_compact_listing(pane, directory.clone(), vec![entry.clone()]);
            cx.notify();
        });
        window.draw(app).clear(app);
    });
    let row = visual
        .debug_bounds("files-row-1-next.txt")
        .expect("reopened file row");
    visual.simulate_mouse_move(row.center(), None, Modifiers::none());
    visual.simulate_mouse_down(row.center(), MouseButton::Right, Modifiers::none());
    visual.simulate_mouse_up(row.center(), MouseButton::Right, Modifiers::none());
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
    assert!(visual.debug_bounds("rr-popup-menu").is_some());
    visual.simulate_keystrokes("down enter");
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
    visual.update(|window, app| {
        let root = root.read(app);
        let snapshot = root.workspace_controller.as_ref().unwrap().snapshot();
        assert_eq!(snapshot.active_tab_id.as_ref(), Some(&target_tab));
        assert_eq!(snapshot.terminal_tabs.len(), 2);
        assert_eq!(root.center_panel, CenterPanel::Editor);
        assert!(!root.compact_tool_drawer_open);
        assert!(
            !root
                .compact_drawer_focus_handle
                .contains_focused(window, app)
        );
    });
    assert!(visual.debug_bounds("rr-popup-menu").is_none());

    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.show_compact_tool_panel(ToolRailToolId::Sftp, window, cx);
        });
        let mut initial_entry = entry.clone();
        initial_entry.name = "initial.txt".to_owned();
        initial_entry.endpoint = FileEndpoint::Local {
            path: "C:/fixture/initial.txt".into(),
        };
        target_pane.update(app, |pane, cx| {
            file_support::seed_compact_listing(pane, directory, vec![initial_entry]);
            cx.notify();
        });
        window.draw(app).clear(app);
    });
    let row = visual
        .debug_bounds("files-row-1-initial.txt")
        .expect("existing file row");
    visual.simulate_mouse_move(row.center(), None, Modifiers::none());
    visual.simulate_click(row.center(), Modifiers::none());
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
    visual.update(|_, app| {
        let root = root.read(app);
        let snapshot = root.workspace_controller.as_ref().unwrap().snapshot();
        assert_eq!(snapshot.active_tab_id.as_ref(), Some(&initial_tab));
        assert_eq!(snapshot.terminal_tabs.len(), 2);
        assert!(!root.compact_tool_drawer_open);
    });
}

/// 编辑服务缺失时导航未受理，原抽屉、来源和目录必须继续可用。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_sftp_failed_file_navigation_keeps_drawer(app: &mut TestAppContext) {
    use gpui_kit::Modifiers;

    let (_runtime, _file_runtime, root, _, _, _, visual) = compact_editor_fixture(app);
    visual.update(|_, app| {
        root.update(app, |root, _| root.file_editor_workspace = None);
    });
    let row = visual
        .debug_bounds("files-row-1-next.txt")
        .expect("file row");
    visual.simulate_mouse_move(row.center(), None, Modifiers::none());
    visual.simulate_click(row.center(), Modifiers::none());
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
    visual.update(|_, app| {
        let root = root.read(app);
        assert!(root.compact_tool_drawer_open);
        assert!(root.status.is_some());
        assert_eq!(
            root.workspace_controller
                .as_ref()
                .unwrap()
                .snapshot()
                .terminal_tabs
                .len(),
            1
        );
    });
}

/// 文件夹进入仍是原文件面板内的浏览动作，不应收起当前抽屉。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_sftp_directory_navigation_keeps_drawer(app: &mut TestAppContext) {
    use gpui_kit::Modifiers;
    use kerminal_domain::files::FileEntryKind;

    let (_runtime, _file_runtime, root, pane, directory, mut entry, visual) =
        compact_editor_fixture(app);
    entry.name = "folder".to_owned();
    entry.endpoint = FileEndpoint::Local {
        path: "C:/fixture/folder".into(),
    };
    entry.kind = FileEntryKind::Directory;
    visual.update(|window, app| {
        pane.update(app, |pane, cx| {
            file_support::seed_compact_listing(pane, directory, vec![entry]);
            cx.notify();
        });
        window.draw(app).clear(app);
    });
    let row = visual
        .debug_bounds("files-row-1-folder")
        .expect("directory row");
    visual.simulate_mouse_move(row.center(), None, Modifiers::none());
    visual.simulate_click(row.center(), Modifiers::none());
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
    visual.update(|_, app| {
        let root = root.read(app);
        assert!(root.compact_tool_drawer_open);
        assert_eq!(
            root.workspace_controller
                .as_ref()
                .unwrap()
                .snapshot()
                .terminal_tabs
                .len(),
            1
        );
    });
}

/// 正常宽窗里的文件右栏继续停靠；打开文件不改变目标标签的宽窗工具布局。
///
/// @author kongweiguang
#[gpui_kit::test]
fn wide_sftp_file_navigation_keeps_the_docked_tool(app: &mut TestAppContext) {
    use gpui_kit::{Modifiers, px, size};
    use kerminal_application::preferences::ToolRailToolId;

    let (_runtime, _file_runtime, root, _, _, entry, visual) = compact_editor_fixture(app);
    visual.simulate_resize(size(px(1500.), px(700.)));
    visual.update(|window, app| window.draw(app).clear(app));
    let row = visual
        .debug_bounds("files-row-1-next.txt")
        .expect("wide file row");
    visual.simulate_mouse_move(row.center(), None, Modifiers::none());
    visual.simulate_click(row.center(), Modifiers::none());
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
    visual.update(|_, app| {
        root.update(app, |root, _| {
            assert!(!root.tool_rail_rendered_compact.0);
            assert_eq!(root.center_panel, CenterPanel::Editor);
            let settings = root.current_tool_rail_settings();
            assert_eq!(
                root.tool_rail_state.project(&settings).panels.attached,
                Some(ToolRailToolId::Sftp)
            );
            let active = root
                .workspace_controller
                .as_ref()
                .unwrap()
                .snapshot()
                .active_tab_id
                .as_ref()
                .unwrap();
            assert_eq!(
                root.workspace_files.file_endpoint(active),
                Some(entry.endpoint)
            );
        })
    });
}

/// 真实 Root 的窄窗文件菜单必须先取得焦点，第一次 Escape 关闭菜单，第二次才关闭抽屉。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_sftp_context_menu_escape_preserves_drawer_until_second_press(app: &mut TestAppContext) {
    use gpui_kit::{Modifiers, MouseButton, px, size};
    use kerminal_application::preferences::ToolRailToolId;
    use kerminal_domain::files::FileEntryKind;

    let (runtime, _file_runtime, root, pane, visual) = file_queue_fixture(app);
    let endpoint = file_support::remote_endpoint(&HostId::new(), "/fixture");
    let entry = file_support::remote_entry(&endpoint, "fixture.txt", FileEntryKind::File);
    visual.simulate_resize(size(px(800.), px(700.)));
    visual.update(|window, _| window.activate_window());
    visual.update(|window, app| {
        pane.update(app, |pane, cx| {
            pane.set_host_picker_enabled(false, cx);
            pane.set_compact_layout(true, cx);
            file_support::seed_compact_listing(pane, endpoint, vec![entry]);
        });
        root.update(app, |root, cx| {
            root.center_panel = CenterPanel::Terminal;
            root.sftp_workbench_tab_open = false;
            root.tab_tool_contents_tab_id = Some(TabId::new());
            root.sftp_panel = Some(pane.clone());
            root.show_compact_tool_panel(ToolRailToolId::Sftp, window, cx);
        });
    });
    settle(&runtime, visual);
    visual.update(|window, app| window.draw(app).clear(app));
    visual.update(|window, app| {
        let root = root.read(app);
        assert_eq!(
            root.tool_rail_rendered_compact,
            (true, Some(ToolRailToolId::Sftp))
        );
        assert!(root.compact_tool_drawer_open);
        assert!(root.compact_drawer_focus_handle.is_focused(window));
    });
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.show_compact_tool_panel(ToolRailToolId::System, window, cx);
        });
        window.draw(app).clear(app);
        let root = root.read(app);
        assert_eq!(
            root.tool_rail_rendered_compact,
            (true, Some(ToolRailToolId::System))
        );
        assert!(root.compact_drawer_focus_handle.is_focused(window));
    });
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.show_compact_tool_panel(ToolRailToolId::Sftp, window, cx);
        });
        window.draw(app).clear(app);
        let root = root.read(app);
        assert_eq!(
            root.tool_rail_rendered_compact,
            (true, Some(ToolRailToolId::Sftp))
        );
        assert!(root.compact_drawer_focus_handle.is_focused(window));
    });
    let row = visual
        .debug_bounds("files-row-1-fixture.txt")
        .expect("fixture file row is rendered in the SFTP drawer");
    visual.simulate_mouse_move(row.center(), None, Modifiers::none());
    visual.simulate_mouse_down(row.center(), MouseButton::Right, Modifiers::none());
    visual.simulate_mouse_up(row.center(), MouseButton::Right, Modifiers::none());
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
    let (drawer_open, contexts, focused) = visual.update(|window, app| {
        (
            root.read(app).compact_tool_drawer_open,
            window.context_stack(),
            window.focused(app),
        )
    });
    assert!(drawer_open, "right-click must not close the drawer");
    assert!(
        visual.debug_bounds("rr-popup-menu").is_some(),
        "menu must be visible"
    );
    assert!(
        contexts.iter().any(|context| context.contains("PopupMenu")),
        "the visible menu must own focus before Escape; focused={focused:?}, contexts={contexts:?}"
    );

    visual.simulate_keystrokes("escape");
    visual.update(|window, app| window.draw(app).clear(app));
    assert!(visual.debug_bounds("rr-popup-menu").is_none());
    assert!(root.read_with(visual, |root, _| root.compact_tool_drawer_open));
    visual.simulate_keystrokes("escape");
    visual.update(|window, app| window.draw(app).clear(app));
    assert!(!root.read_with(visual, |root, _| root.compact_tool_drawer_open));

    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.show_compact_tool_panel(ToolRailToolId::Sftp, window, cx);
        });
        window.draw(app).clear(app);
    });
    visual.simulate_resize(size(px(1500.), px(700.)));
    visual.update(|window, app| window.draw(app).clear(app));
    visual.update(|window, app| {
        let root = root.read(app);
        assert!(!root.tool_rail_rendered_compact.0);
        assert!(!root.compact_tool_drawer_open);
        assert!(!root.compact_drawer_focus_handle.is_focused(window));
    });
    visual.simulate_resize(size(px(800.), px(700.)));
    visual.update(|window, app| window.draw(app).clear(app));
    visual.update(|window, app| {
        let root = root.read(app);
        assert_eq!(
            root.tool_rail_rendered_compact,
            (true, Some(ToolRailToolId::Sftp))
        );
        assert!(root.compact_tool_drawer_open);
        assert!(root.compact_drawer_focus_handle.is_focused(window));
    });
}

/// 关闭只阻挡受理中的目标；批量关闭先整体检查，其他标签仍可独立关闭。
///
/// @author kongweiguang
#[gpui_kit::test]
fn transfer_enqueue_blocks_only_targeted_close_and_batch(app: &mut TestAppContext) {
    use crate::features::workspace_tabs::{WorkspaceCloseBatchIntent, WorkspaceTabEvent};

    let (_runtime, _file_runtime, root, shared, visual) = file_queue_fixture(app);
    let first_id = TabId::new();
    let second_id = TabId::new();
    let host_id = HostId::new();
    let pane_slot = Rc::new(RefCell::new(None));
    let pane_slot_for_mount = pane_slot.clone();
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.active_file_sftp_selection = None;
            root.services.hosts = Some(HostManagementService::new(Arc::new(EmptyHosts {
                saved: vec![saved_sftp_host(host_id.clone())],
            })));
            let make_tab = |id| {
                sftp_transfer_tab_for_host(
                    id,
                    host_id.clone(),
                    None,
                    &kerminal_domain::files::RemotePath::parse("/fixture").unwrap(),
                    "Transfer",
                )
            };
            let mut controller = WorkspaceController::from_missing().unwrap();
            let mut snapshot = controller.snapshot().clone();
            snapshot.active_tab_id = Some(first_id.clone());
            snapshot.terminal_tabs = vec![make_tab(first_id.clone()), make_tab(second_id.clone())];
            controller.synchronize_snapshot(snapshot).unwrap();
            root.workspace_files = WorkspaceFileRuntime::from_snapshot(controller.snapshot());
            root.workspace_controller = Some(controller);
            root.ensure_workspace_file_tab(&first_id, window, cx);
            root.ensure_workspace_file_tab(&second_id, window, cx);
            *pane_slot_for_mount.borrow_mut() = root.workspace_files.files_pane(&first_id);
        });
    });
    let first = pane_slot.borrow_mut().take().expect("first transfer pane");
    visual.update(|window, app| {
        first.update(app, file_support::begin_enqueue_barrier);
        root.update(app, |root, cx| {
            let workbench_id = root.sftp_workbench_tab_id.clone();
            let batch = WorkspaceCloseBatchIntent::ToRight {
                anchor_tab_id: TabId::new(),
                tab_ids: vec![first_id.clone(), second_id.clone(), workbench_id.clone()],
            };
            root.handle_workspace_tab_event(&WorkspaceTabEvent::CloseBatch(batch), window, cx);
            assert!(root.sftp_workbench_tab_open);
            assert_eq!(
                root.workspace_controller
                    .as_ref()
                    .unwrap()
                    .snapshot()
                    .terminal_tabs
                    .len(),
                2
            );

            root.handle_workspace_tab_event(
                &WorkspaceTabEvent::Close(first_id.clone()),
                window,
                cx,
            );
            assert!(root.workspace_files.files_pane(&first_id).is_some());
            root.handle_workspace_tab_event(
                &WorkspaceTabEvent::Close(second_id.clone()),
                window,
                cx,
            );
            assert!(root.workspace_files.files_pane(&second_id).is_none());
            assert!(root.workspace_files.files_pane(&first_id).is_some());
        });
    });
    visual.update(|window, app| {
        first.update(app, |pane, _| file_support::finish_enqueue_barrier(pane));
        root.update(app, |root, cx| {
            root.handle_workspace_tab_event(
                &WorkspaceTabEvent::Close(first_id.clone()),
                window,
                cx,
            );
            assert!(root.workspace_files.files_pane(&first_id).is_none());
        });
        shared.update(app, file_support::begin_enqueue_barrier);
        root.update(app, |root, cx| {
            root.close_shared_sftp_workbench(window, cx);
            assert!(root.sftp_workbench_tab_open);
        });
        shared.update(app, |pane, _| file_support::finish_enqueue_barrier(pane));
        root.update(app, |root, cx| {
            root.close_shared_sftp_workbench(window, cx);
            assert!(!root.sftp_workbench_tab_open);
        });
    });
}

/// 只构造内存里的 SFTP 主机，确保目录刷新不会把本测试目标当作已删除配置。
///
/// @author kongweiguang
fn saved_sftp_host(id: HostId) -> ManagedHost {
    ManagedHost {
        name: id.as_str().to_owned(),
        id,
        group_id: None,
        note: None,
        protocol: RemoteProtocol::Ssh,
        endpoint: HostEndpoint {
            host: "fixture.invalid".to_owned(),
            port: 22,
            username: "fixture".to_owned(),
        },
        authentication: None,
        authentication_mode: HostAuthenticationMode::Agent,
        rdp_password: None,
        route: HostRouteSettings::default(),
        serial: None,
        tags: Vec::new(),
        is_favorite: false,
        sort_order: 0,
        connect_timeout_ms: 30_000,
        keepalive_interval_ms: None,
        terminal_options: HostTerminalOptions::default(),
        sftp_options: HostSftpOptions::default(),
        rdp_display: RdpDisplayOptions::default(),
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
        revision: ConfigRevision::from_sha256("fixture".to_owned()),
    }
}

/// 两侧真实事件各自排队；同侧新意图替换旧项，取消首项后继续原来的另一侧。
///
/// @author kongweiguang
#[gpui_kit::test]
fn file_selection_queue_keeps_other_side_when_replaced_or_cancelled(app: &mut TestAppContext) {
    let (runtime, _file_runtime, root, pane, visual) = file_queue_fixture(app);
    let old_left = HostId::new();
    let right = HostId::new();
    let new_left = HostId::new();
    visual.update(|_, app| {
        pane.update(app, |pane, cx| {
            file_support::request_saved_host(pane, FilePaneSide::Left, old_left.clone(), cx)
        });
        pane.update(app, |pane, cx| {
            file_support::request_saved_host(pane, FilePaneSide::Right, right.clone(), cx)
        });
    });
    settle(&runtime, visual);
    visual.update(|_, app| {
        root.update(app, |root, _| {
            assert_eq!(root.pending_file_sftp_selections.len(), 2);
        });
        pane.update(app, |pane, cx| {
            file_support::request_saved_host(pane, FilePaneSide::Left, new_left.clone(), cx)
        });
    });
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let hosts = root
                .pending_file_sftp_selections
                .iter()
                .map(|request| request.host_id.clone())
                .collect::<Vec<_>>();
            assert_eq!(hosts, vec![right.clone(), new_left.clone()]);
            root.active_file_sftp_selection = root.pending_file_sftp_selections.pop_front();
            let request = root
                .active_file_sftp_selection
                .as_ref()
                .expect("first request")
                .clone();
            handle_connections_event(
                root,
                &ConnectionsEvent::SftpSelectionFinished {
                    request_id: request.request_id,
                    message: None,
                    interrupted: false,
                },
                window,
                cx,
            );
            assert_eq!(
                root.active_file_sftp_selection
                    .as_ref()
                    .map(|request| &request.host_id),
                Some(&new_left)
            );
        });
    });
    settle(&runtime, visual);
    visual.update(|_, app| {
        root.update(app, |root, _| {
            assert!(root.active_file_sftp_selection.is_none());
            assert!(root.pending_file_sftp_selections.is_empty());
        })
    });
}

/// 用户离开中央文件页后，旧主机预检即使回报成功也不能改写原侧或抢回页面。
///
/// @author kongweiguang
#[gpui_kit::test]
fn late_file_selection_receipt_after_navigation_does_not_rebind(app: &mut TestAppContext) {
    let (runtime, _file_runtime, root, pane, visual) = file_queue_fixture(app);
    let host_id = HostId::new();
    visual.update(|_, app| {
        pane.update(app, |pane, cx| {
            file_support::request_saved_host(pane, FilePaneSide::Left, host_id.clone(), cx)
        })
    });
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let before = file_support::panel_endpoint(pane.read(cx), FilePaneSide::Left);
            root.active_file_sftp_selection = root.pending_file_sftp_selections.pop_front();
            let request = root
                .active_file_sftp_selection
                .as_ref()
                .expect("pending selection")
                .clone();
            root.set_center_panel(CenterPanel::Terminal, window, cx);
            handle_connections_event(
                root,
                &ConnectionsEvent::SftpSelectionReady {
                    request_id: request.request_id,
                    endpoint: FileEndpoint::remote(host_id, "/after").expect("remote target"),
                    initial_local_directory: None,
                },
                window,
                cx,
            );
            assert_eq!(root.center_panel, CenterPanel::Terminal);
            assert_eq!(
                file_support::panel_endpoint(pane.read(cx), FilePaneSide::Left),
                before
            );
            assert!(root.active_file_sftp_selection.is_none());
        })
    });
}

/// 新建主机的保存回执在模态真实关闭后才可回投，且继续使用原侧和原代次。
///
/// @author kongweiguang
#[gpui_kit::test]
fn file_host_creation_waits_for_modal_close_and_preserves_original_side(app: &mut TestAppContext) {
    let (runtime, _file_runtime, root, pane, visual) = file_queue_fixture(app);
    let host_id = HostId::new();
    visual.update(|_, app| {
        pane.update(app, |pane, cx| {
            file_support::request_saved_host(pane, FilePaneSide::Right, host_id.clone(), cx)
        })
    });
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let request = root
                .pending_file_sftp_selections
                .pop_front()
                .expect("source request");
            root.active_file_sftp_selection = None;
            root.pending_sftp_host_creation = Some(PendingSftpHostCreation {
                request_id: "creation".to_owned(),
                workbench_tab_id: root.sftp_workbench_tab_id.clone(),
                navigation_generation: root.surface_navigation_generation,
                saved_host: Some((host_id.clone(), true)),
                origin: Some(request.origin.clone()),
            });
            root.connections_modal_open = true;
            root.resume_sftp_host_creation(window, cx);
            assert!(root.pending_sftp_host_creation.is_some());
            assert!(root.active_file_sftp_selection.is_none());
            root.connections_modal_open = false;
            root.resume_sftp_host_creation(window, cx);
            assert!(root.pending_sftp_host_creation.is_none());
            let resumed = root
                .active_file_sftp_selection
                .as_ref()
                .expect("selection resumed");
            assert_eq!(resumed.host_id, host_id);
            assert_eq!(resumed.origin.side, FilePaneSide::Right);
            assert_eq!(resumed.origin.generation, request.origin.generation);
        })
    });
}

/// CAS 冲突期间保留最新目录，明确解除冲突后同步；关闭标签后待写内容不能重新创建标签。
///
/// @author kongweiguang
#[gpui_kit::test]
fn transfer_endpoint_changes_wait_for_cas_and_cannot_revive_closed_tab(app: &mut TestAppContext) {
    let (runtime, _file_runtime, root, _, visual) = file_queue_fixture(app);
    let tab_id = TabId::new();
    let original_host = HostId::new();
    let new_host = HostId::new();
    let pane_slot = Rc::new(RefCell::new(None));
    let pane_slot_for_mount = pane_slot.clone();
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.active_file_sftp_selection = None;
            root.services.hosts = Some(HostManagementService::new(Arc::new(EmptyHosts {
                saved: vec![
                    saved_sftp_host(original_host.clone()),
                    saved_sftp_host(new_host.clone()),
                ],
            })));
            let tab = sftp_transfer_tab_for_host(
                tab_id.clone(),
                original_host.clone(),
                None,
                &kerminal_domain::files::RemotePath::parse("/old").expect("valid path"),
                "Transfer",
            );
            let mut controller = WorkspaceController::from_missing().expect("workspace controller");
            let mut snapshot = controller.snapshot().clone();
            snapshot.active_tab_id = Some(tab_id.clone());
            snapshot.terminal_tabs.push(tab);
            controller
                .synchronize_snapshot(snapshot)
                .expect("transfer descriptor");
            root.workspace_files = WorkspaceFileRuntime::from_snapshot(controller.snapshot());
            root.workspace_controller = Some(controller);
            root.center_panel = CenterPanel::Files;
            root.files_workspace_selection = FilesWorkspaceSelection::TransferTab(tab_id.clone());
            root.ensure_workspace_file_tab(&tab_id, window, cx);
            *pane_slot_for_mount.borrow_mut() = root.workspace_files.files_pane(&tab_id);
            let controller = root.workspace_controller.as_mut().expect("controller");
            let request = controller
                .begin_save()
                .expect("begin save")
                .expect("dirty workspace");
            controller
                .finish_save(
                    &request.request_id,
                    WorkspaceSessionSaveOutcome::Conflict {
                        latest: WorkspaceSessionLoad::Missing,
                    },
                )
                .expect("conflict receipt");
        })
    });
    let pane = pane_slot
        .borrow_mut()
        .take()
        .expect("transfer pane restored");
    let left = FileEndpoint::remote(new_host.clone(), "/new-left").expect("remote target");
    visual.update(|_, app| {
        pane.update(app, |pane, cx| {
            file_support::bind_host_target(pane, FilePaneSide::Left, left.clone(), cx);
            file_support::close_active_host_tab(pane, FilePaneSide::Right, cx);
        })
    });
    settle(&runtime, visual);
    visual.update(|_, app| root.update(app, |root, cx| {
        assert_eq!(
            root.workspace_files.pending_transfer_endpoints(&tab_id),
            Some((left.clone(), None)),
            "actual panel endpoints: left={:?}, right={:?}; root status={:?}",
            file_support::panel_endpoint(pane.read(cx), FilePaneSide::Left),
            file_support::panel_endpoint(pane.read(cx), FilePaneSide::Right),
            root.status,
        );
        let controller = root.workspace_controller.as_mut().expect("controller");
        assert!(matches!(controller.snapshot().terminal_tabs.first(), Some(WorkspaceSessionTab::SftpTransfer {
            left_host_id: None, right_host_id: Some(host), ..
        }) if host == &original_host));
        controller.confirm_overwrite_latest().expect("user resolves conflict");
        root.refresh_workspace_views(cx);
        let controller = root.workspace_controller.as_mut().expect("controller");
        assert!(matches!(controller.snapshot().terminal_tabs.first(), Some(WorkspaceSessionTab::SftpTransfer {
            left_host_id: Some(host), right_host_id: None, initial_right_path: None, ..
        }) if host == &new_host), "snapshot={:?}", controller.snapshot());
        let request = controller.begin_save().expect("begin save").expect("dirty workspace");
        controller.finish_save(&request.request_id, WorkspaceSessionSaveOutcome::Conflict {
            latest: WorkspaceSessionLoad::Missing,
        }).expect("conflict receipt");
    }));
    visual.update(|_, app| {
        pane.update(app, |pane, cx| {
            file_support::bind_host_target(
                pane,
                FilePaneSide::Right,
                FileEndpoint::Local {
                    path: "C:/new-right".into(),
                },
                cx,
            );
        })
    });
    settle(&runtime, visual);
    let closing_host = HostId::new();
    visual.update(|_, app| {
        root.update(app, |root, _| {
            root.active_file_sftp_selection = Some(FileSftpSelectionRequest {
                request_id: "fixture-busy-close".to_owned(),
                host_id: HostId::new(),
                origin: FileSftpOrigin {
                    source: FilesWorkspaceSelection::Workbench,
                    pane_id: 0,
                    side: FilePaneSide::Left,
                    generation: 0,
                    navigation_generation: root.surface_navigation_generation,
                },
            });
        });
        pane.update(app, |pane, cx| {
            file_support::request_saved_host(pane, FilePaneSide::Left, closing_host.clone(), cx)
        });
    });
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let request = root
                .pending_file_sftp_selections
                .pop_front()
                .expect("closing tab request");
            assert_eq!(request.host_id, closing_host);
            root.active_file_sftp_selection = Some(request.clone());
            let controller = root.workspace_controller.as_mut().expect("controller");
            controller
                .confirm_overwrite_latest()
                .expect("user resolves conflict");
            let mut snapshot = controller.snapshot().clone();
            snapshot.terminal_tabs.retain(|tab| tab.id() != &tab_id);
            snapshot.active_tab_id = None;
            controller
                .synchronize_snapshot(snapshot)
                .expect("close descriptor");
            root.cancel_file_sftp_selections_for_tab(&tab_id, cx);
            root.remove_workspace_file_tab_after_close(&tab_id);
            root.refresh_workspace_views(cx);
            assert!(
                !root
                    .workspace_controller
                    .as_ref()
                    .expect("controller")
                    .snapshot()
                    .terminal_tabs
                    .iter()
                    .any(|tab| tab.id() == &tab_id)
            );
            assert!(
                !pane
                    .read(cx)
                    .host_selection_is_current(request.origin.side, request.origin.generation)
            );
            handle_connections_event(
                root,
                &ConnectionsEvent::SftpSelectionReady {
                    request_id: request.request_id,
                    endpoint: FileEndpoint::remote(closing_host, "/late").expect("remote target"),
                    initial_local_directory: None,
                },
                window,
                cx,
            );
            assert!(root.active_file_sftp_selection.is_none());
            assert!(
                !root
                    .workspace_controller
                    .as_ref()
                    .expect("controller")
                    .snapshot()
                    .terminal_tabs
                    .iter()
                    .any(|tab| tab.id() == &tab_id)
            );
        })
    });
    settle(&runtime, visual);
}

/// 真实挂载紧凑文件栏并断言文件行占满宿主正文；三种非右栏方向共享同一验证。
///
/// @author kongweiguang
fn assert_compact_files_placement_keeps_rows(
    app: &mut TestAppContext,
    placement: ToolRailPanelPlacement,
    minimum_list_height: f32,
) {
    use gpui_kit::{px, size};
    use kerminal_application::preferences::ToolRailToolId;

    let (runtime, _file_runtime, root, _pane, _directory, _entry, visual) =
        compact_editor_fixture(app);
    visual.simulate_resize(size(px(1440.), px(900.)));
    file_support::draw(visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let mut settings = SettingsSnapshot::default();
            settings
                .tool_rail
                .panel_placements
                .insert(ToolRailToolId::Sftp, placement);
            root.settings_snapshot = Some(settings);
            root.remap_tool_rail_settings(cx);
        });
        window.draw(app).clear(app);
    });
    file_support::draw(visual);

    let list = visual
        .debug_bounds("files-browser-list-1")
        .unwrap_or_else(|| panic!("{placement:?} did not mount the file list"));
    assert!(
        list.size.height > px(minimum_list_height),
        "{placement:?} collapsed the file browser: {list:?}"
    );
    let row = visual
        .debug_bounds("files-row-1-next.txt")
        .unwrap_or_else(|| panic!("{placement:?} did not lay out file rows"));
    assert!(
        row.origin.y >= list.origin.y && row.bottom() <= list.bottom(),
        "file row escaped the list in {placement:?}: row={row:?}, list={list:?}"
    );
    let transfer_header = visual
        .debug_bounds("rr-files-transfer-history")
        .expect("transfer header stays mounted");
    assert!(
        transfer_header.origin.y >= list.bottom(),
        "transfer header must follow the file list in {placement:?}: header={transfer_header:?}, list={list:?}"
    );
    let _runtime = runtime;
}

/// 左侧停靠必须与右栏使用同一满高宿主，不能把文件正文压成横向工具栏高度。
///
/// @author kongweiguang
#[gpui_kit::test]
fn left_tool_panel_host_preserves_compact_files_browser_height(app: &mut TestAppContext) {
    assert_compact_files_placement_keeps_rows(app, ToolRailPanelPlacement::Left, 240.);
}

/// 底部停靠按可读下限分配高度，保留文件行和传输条，不让工具栏吃掉正文。
///
/// @author kongweiguang
#[gpui_kit::test]
fn bottom_tool_panel_host_preserves_compact_files_browser_height(app: &mut TestAppContext) {
    assert_compact_files_placement_keeps_rows(app, ToolRailPanelPlacement::Bottom, 100.);
}

/// 浮窗宿主由外层几何提供确定高度，文件正文仍要占满除工具栏外的剩余空间。
///
/// @author kongweiguang
#[gpui_kit::test]
fn floating_tool_panel_host_preserves_compact_files_browser_height(app: &mut TestAppContext) {
    assert_compact_files_placement_keeps_rows(app, ToolRailPanelPlacement::Center, 240.);
}
