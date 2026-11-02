//! 持久本地主机字段投影与输入解析的无窗口回归。
//!
//! @author kongweiguang

use super::{
    LocalHostDefaultValues, LocalHostEditorLoadIntent, LocalHostEditorMode, LocalHostInputBaseline,
    LocalHostState, LocalHostUnknownMutation, LocalHostValidationField, local_host_draft,
    local_host_needs_save, local_host_projection, local_host_validation_field,
    parse_profile_arguments, parse_profile_environment, profile_to_draft,
    project_explicit_local_defaults, project_untouched_local_defaults,
};
use async_trait::async_trait;
use gpui_kit::{
    AppContext as _, Entity, KeyDownEvent, Keystroke, TestAppContext, VisualTestContext, point, px,
    size,
};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor, ProfileService,
    connections::{
        ConnectionIntent, HostKeyChallenge, HostKeyPeer, HostKeyRotationReceipt,
        HostKeyRotationRequest, HostKeyTrustReceipt, HostKeyTrustRequest, RemoteConnectionLease,
        RemoteConnectionService,
        management::{
            HostDocumentRevision, HostDraft, HostGroupDeletionReceipt, HostGroupDraft,
            HostGroupSnapshot, HostManagementRepository, HostManagementService,
            LocalHostSaveRecoveryToken, ManagedHost, ManagedLocalHost,
        },
    },
    preferences::{
        DefaultLocalShellProvider, ProfileDraft, ProfileRepository, ProfileRevision,
        VersionedProfile,
    },
};
use kerminal_domain::identity::HostId;
use kerminal_domain::preferences::{
    ConfigRevision, ConfigurationMode, ConfigurationRecoveryState, TerminalProfile,
};
use kerminal_domain::sessions::LocalSessionTarget;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    runtime::{Builder, Runtime},
    sync::Semaphore,
};
use tokio_util::sync::CancellationToken;

use super::super::panel::ConnectionsPanel;

/// 列表与编辑器必须保留主机自己的身份与配置，只有编辑器读回才携带环境值。
/// @author kongweiguang
#[test]
fn local_host_projection_keeps_host_identity_and_explicit_environment() {
    let host = ManagedLocalHost {
        id: HostId::parse("local-host").expect("测试 ID 有效"),
        group_id: Some("team".to_owned()),
        name: "PowerShell".to_owned(),
        shell: "pwsh.exe".to_owned(),
        args: vec!["-NoLogo".to_owned()],
        cwd: Some("C:\\work".to_owned()),
        environment_ref: None,
        sort_order: 3,
        created_at: "created".to_owned(),
        updated_at: "updated".to_owned(),
        revision: ConfigRevision::from_sha256("revision".to_owned()),
        migration_origin: None,
    };
    let list_row = local_host_projection(host.clone(), BTreeMap::new());
    assert_eq!(list_row.profile.id, "local-host");
    assert_eq!(list_row.profile.sidebar_group_id.as_deref(), Some("team"));
    assert!(!list_row.profile.is_default);
    assert!(list_row.profile.env.is_empty());

    let environment = BTreeMap::from([("PASSWORD".to_owned(), "secret".to_owned())]);
    let editor_row = local_host_projection(host, environment.clone());
    let draft = local_host_draft(profile_to_draft(&editor_row.profile)).expect("草稿可保存");
    assert_eq!(draft.id.as_ref().map(HostId::as_str), Some("local-host"));
    assert_eq!(draft.group_id.as_deref(), Some("team"));
    assert_eq!(draft.env, environment);
    assert_eq!(draft.args, vec!["-NoLogo"]);
}

/// 给每个本地主机字段构造一致的原始输入快照，验证默认值异步到达时的保护规则。
fn baseline(name: &str, shell: &str, cwd: &str, args: &str, env: &str) -> LocalHostInputBaseline {
    LocalHostInputBaseline {
        name: name.to_owned(),
        shell: shell.to_owned(),
        cwd: cwd.to_owned(),
        args: args.to_owned(),
        env: env.to_owned(),
        group_id: None,
    }
}

#[test]
/// 默认目标晚于用户输入返回时，只填补仍未修改的对应字段，不污染名称基线。
fn late_default_result_only_projects_fields_unchanged_since_editor_opened() {
    let original = baseline("本地主机", "", "", "", "");
    let current = baseline("用户改过的名称", "", "", "--login\n--noprofile", "K=custom");
    let defaults = LocalHostDefaultValues {
        shell: "pwsh.exe".to_owned(),
        cwd: "C:\\work".to_owned(),
        args: "-NoLogo\n-NoProfile".to_owned(),
        env: "K=default\nTERM=xterm-256color".to_owned(),
    };

    let projection = project_untouched_local_defaults(&original, &current, defaults);

    assert_eq!(projection.shell.as_deref(), Some("pwsh.exe"));
    assert_eq!(projection.cwd.as_deref(), Some("C:\\work"));
    assert_eq!(projection.args, None);
    assert_eq!(projection.env, None);
    assert_eq!(projection.baseline.name, original.name);
    assert_eq!(projection.baseline.shell, "pwsh.exe");
    assert_eq!(projection.baseline.cwd, "C:\\work");
    assert_eq!(projection.baseline.args, original.args);
    assert_eq!(projection.baseline.env, original.env);
    assert_ne!(current.name, original.name);
}

#[test]
/// 未触碰的字段均可按平台默认值填充，保持多行参数及环境变量的原始语义。
fn untouched_default_fields_are_all_projected() {
    let original = baseline("Local", "", "", "", "");
    let projection = project_untouched_local_defaults(
        &original,
        &original,
        LocalHostDefaultValues {
            shell: "bash".to_owned(),
            cwd: "/workspace".to_owned(),
            args: "-l\n-i".to_owned(),
            env: "TERM=xterm".to_owned(),
        },
    );

    assert_eq!(projection.shell.as_deref(), Some("bash"));
    assert_eq!(projection.cwd.as_deref(), Some("/workspace"));
    assert_eq!(projection.args.as_deref(), Some("-l\n-i"));
    assert_eq!(projection.env.as_deref(), Some("TERM=xterm"));
}

/// 显式恢复覆盖请求时已有字段，但不覆盖请求发出后用户继续编辑的值。
/// @author kongweiguang
#[test]
fn explicit_default_request_overrides_prior_values_and_preserves_later_edits() {
    let initial = baseline("Local", "old-shell", "old-cwd", "old-args", "OLD=1");
    let expected = baseline(
        "Local",
        "custom-shell",
        "custom-cwd",
        "custom-args",
        "CUSTOM=1",
    );
    let current = baseline(
        "Local",
        "custom-shell",
        "newer-cwd",
        "custom-args",
        "CUSTOM=1",
    );
    let defaults = LocalHostDefaultValues {
        shell: "pwsh.exe".to_owned(),
        cwd: "C:\\work".to_owned(),
        args: "-NoLogo".to_owned(),
        env: "TERM=xterm".to_owned(),
    };

    let projection = project_explicit_local_defaults(&expected, &current, defaults, &initial);

    assert_eq!(projection.shell.as_deref(), Some("pwsh.exe"));
    assert_eq!(projection.cwd, None);
    assert_eq!(projection.args.as_deref(), Some("-NoLogo"));
    assert_eq!(projection.env.as_deref(), Some("TERM=xterm"));
    assert_eq!(projection.baseline, initial);
}

#[test]
/// 空参数编辑区表达零参数，显式空行仍代表一个空参数以保持现有表单兼容性。
fn argument_lines_preserve_explicit_empty_arguments() {
    assert_eq!(parse_profile_arguments(""), Vec::<String>::new());
    assert_eq!(
        parse_profile_arguments("--flag\n\nvalue"),
        vec!["--flag".to_owned(), "".to_owned(), "value".to_owned()]
    );
}

#[test]
/// 环境变量以首个等号拆分，值可继续包含等号，空行忽略且重复键拒绝。
fn environment_lines_split_once_and_reject_duplicate_keys() {
    let values = parse_profile_environment("TOKEN=a=b=c\n\nTERM=xterm").unwrap();
    assert_eq!(values.get("TOKEN").map(String::as_str), Some("a=b=c"));
    assert_eq!(values.get("TERM").map(String::as_str), Some("xterm"));
    assert!(parse_profile_environment("MISSING_SEPARATOR").is_err());
    assert!(parse_profile_environment("DUP=first\nDUP=second").is_err());
}

/// Profile 校验只聚焦已知字段，复合参数限制按原始启动参数决定聚焦终端页的哪一项。
/// @author kongweiguang
#[test]
fn local_profile_validation_targets_properties_or_terminal_fields() {
    assert_eq!(
        local_host_validation_field("profile 名称不能为空或过长", "", ""),
        Some(LocalHostValidationField::Name),
    );
    assert_eq!(
        local_host_validation_field("profile shell 路径不能为空或过长", "", ""),
        Some(LocalHostValidationField::Shell),
    );
    assert_eq!(
        local_host_validation_field("profile 参数或环境变量超出限制", "-NoLogo", ""),
        Some(LocalHostValidationField::Environment),
    );
    assert_eq!(
        local_host_validation_field(
            "profile 参数或环境变量超出限制",
            &"x".repeat(16 * 1024 + 1),
            "",
        ),
        Some(LocalHostValidationField::Arguments),
    );
    assert_eq!(
        local_host_validation_field("profile 参数或环境变量超出限制", "", &"x".repeat(4097)),
        Some(LocalHostValidationField::WorkingDirectory),
    );
    assert_eq!(
        local_host_validation_field("unmapped profile error", "", ""),
        None,
    );
}

#[test]
/// 创建与复制没有 revision，因此即使 pristine 也必须保存；干净编辑则不应制造无效写入。
fn create_and_duplicate_need_save_without_raw_edits_but_clean_edit_does_not() {
    let revision = ConfigRevision::from_sha256("revision".to_owned());

    assert!(local_host_needs_save(None, false));
    assert!(local_host_needs_save(None, true));
    assert!(!local_host_needs_save(Some(&revision), false));
    assert!(local_host_needs_save(Some(&revision), true));
}

/// 主机新校验文案要跳到真实字段，不能只在表单底部显示笼统失败。
/// @author kongweiguang
#[test]
fn local_host_validation_focuses_the_failed_field() {
    assert_eq!(
        local_host_validation_field("本地主机名称不能为空或过长", "", ""),
        Some(LocalHostValidationField::Name),
    );
    assert_eq!(
        local_host_validation_field("本地主机 shell 路径不能为空或过长", "", ""),
        Some(LocalHostValidationField::Shell),
    );
    assert_eq!(
        local_host_validation_field("本地主机环境变量超出限制", "", ""),
        Some(LocalHostValidationField::Environment),
    );
    assert_eq!(
        local_host_validation_field("本地主机参数或工作目录超出限制", "", &"x".repeat(4097)),
        Some(LocalHostValidationField::WorkingDirectory),
    );
}

#[test]
/// 保存、未知结果、待清理凭据、删除核对及读回期间统一封锁变更入口。
fn mutation_gate_blocks_each_in_flight_or_unresolved_operation() {
    let mut state = LocalHostState::default();
    assert!(!state.mutation_blocked());

    state.saving = true;
    assert!(state.mutation_blocked());
    state.saving = false;

    state.save_outcome_unknown = true;
    assert!(state.mutation_blocked());
    state.save_outcome_unknown = false;

    state.cleanup_pending = true;
    assert!(state.mutation_blocked());
    state.cleanup_pending = false;

    state.delete_checking = true;
    assert!(state.mutation_blocked());
    state.delete_checking = false;

    state.readback_in_progress = true;
    assert!(state.mutation_blocked());
}

#[test]
/// 关闭正在加载的本地主机编辑器会撤销唯一有效意图，迟到的 get_profile 回执不能重开表单。
fn cancelled_editor_load_rejects_its_late_readback() {
    let mut state = LocalHostState {
        editor_load_generation: 7,
        editor_load_pending: true,
        editor_load_intent: Some(LocalHostEditorLoadIntent {
            profile_id: "host-a".to_owned(),
            mode: LocalHostEditorMode::Edit,
            generation: 7,
        }),
        ..LocalHostState::default()
    };
    assert!(state.accepts_editor_load("host-a", LocalHostEditorMode::Edit, 7));
    assert!(state.can_cancel_editor_load());
    assert!(state.mutation_blocked());

    state.cancel_editor_load();

    assert!(!state.editor_load_pending);
    assert!(state.editor_load_intent.is_none());
    assert!(!state.accepts_editor_load("host-a", LocalHostEditorMode::Edit, 7));
    assert!(!state.can_cancel_editor_load());
    assert!(!state.mutation_blocked());

    let owner = LocalHostUnknownMutation::Save {
        profile_id: "host-b".to_owned(),
    };
    state.save_outcome_unknown = true;
    state.unknown_mutation = Some(owner.clone());
    state.cancel_editor_load();
    assert!(state.save_outcome_unknown);
    assert_eq!(state.unknown_mutation, Some(owner));
}

#[test]
/// 核对读取必须从空快照开始；普通列表刷新不能抢走其代次或解除未知写入所有权。
/// @author kongweiguang
fn explicit_readback_invalidates_old_result_and_rejects_watcher_refresh() {
    let mut state = LocalHostState::default();
    let owner = LocalHostUnknownMutation::Save {
        profile_id: "host-a".to_owned(),
    };
    state.save_outcome_unknown = true;
    state.unknown_mutation = Some(owner.clone());
    state.unknown_target_missing = true;

    let generation = state.begin_list_load(true).expect("首次核对可启动");
    assert!(!state.unknown_target_missing);
    assert!(state.unknown_readback.is_none());
    assert!(state.loading);
    assert!(state.readback_in_progress);
    assert_eq!(state.begin_list_load(false), None);
    assert_eq!(state.begin_list_load(true), None);
    assert_eq!(state.generation, generation);
    assert!(state.save_outcome_unknown);
    assert_eq!(state.unknown_mutation, Some(owner));
}

/// 只延迟显式选中的一次读取，其他目录读取仍经生产用例独立完成。
struct DelayedLocalHosts {
    host: Mutex<Option<ManagedLocalHost>>,
    read_gate: Mutex<Option<Arc<Semaphore>>>,
    list_gate: Mutex<Option<Arc<Semaphore>>>,
    reads_started: AtomicUsize,
    lists_started: AtomicUsize,
    deleted_revisions: Mutex<Vec<ConfigRevision>>,
    recovery: Mutex<Option<LocalHostSaveRecoveryToken>>,
    recoveries_started: AtomicUsize,
    recoveries_finished: AtomicUsize,
}

#[async_trait]
impl HostManagementRepository for DelayedLocalHosts {
    /// 测试只允许内存仓储完成本地主机确认删除。
    async fn ensure_host_writable(&self) -> AppResult<()> {
        Ok(())
    }
    /// 远程目录保持空，避免本地回归启动外部连接。
    async fn list_hosts(&self) -> AppResult<Vec<ManagedHost>> {
        Ok(Vec::new())
    }
    /// 本测试不读取远程主机。
    async fn load_host(&self, _: &HostId) -> AppResult<Option<ManagedHost>> {
        Ok(None)
    }
    /// 先冻结一次目录快照，再等测试释放，以重现正在读取时发起删除。
    async fn list_local_hosts(&self) -> AppResult<Vec<ManagedLocalHost>> {
        let snapshot = self.host.lock().unwrap().clone().into_iter().collect();
        let gate = self.list_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            self.lists_started.fetch_add(1, Ordering::SeqCst);
            gate.acquire().await.unwrap().forget();
        }
        Ok(snapshot)
    }
    /// 在信号量释放前保留读取时的快照，便于验证取消后迟到回执不能覆盖新意图。
    async fn load_local_host(&self, id: &HostId) -> AppResult<Option<ManagedLocalHost>> {
        let snapshot = self
            .host
            .lock()
            .unwrap()
            .clone()
            .filter(|host| &host.id == id);
        let gate = self.read_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            self.reads_started.fetch_add(1, Ordering::SeqCst);
            gate.acquire().await.unwrap().forget();
        }
        Ok(snapshot)
    }
    /// 只有登记同一目标和 revision 的恢复记录后才能删除，模拟真实 CAS 次序。
    async fn delete_local_host(
        &self,
        id: &HostId,
        revision: &ConfigRevision,
    ) -> AppResult<ManagedLocalHost> {
        let recovery = self.recovery.lock().unwrap();
        if !recovery.as_ref().is_some_and(|token| {
            token.host_id() == id && token.baseline_revision() == Some(revision)
        }) {
            return Err(AppError::conflict("test recovery target mismatch"));
        }
        drop(recovery);
        let mut host = self.host.lock().unwrap();
        let current = host
            .as_ref()
            .filter(|host| &host.id == id)
            .ok_or_else(|| AppError::not_found("test host missing"))?;
        if &current.revision != revision {
            return Err(AppError::conflict("test revision changed"));
        }
        self.deleted_revisions
            .lock()
            .unwrap()
            .push(revision.clone());
        Ok(host.take().unwrap())
    }
    /// 删除前登记恢复身份，以检查测试替身遵循真实仓储的写入顺序。
    async fn begin_local_host_recovery(&self, token: &LocalHostSaveRecoveryToken) -> AppResult<()> {
        let mut recovery = self.recovery.lock().unwrap();
        if recovery.is_some() {
            return Err(AppError::conflict("test recovery already pending"));
        }
        *recovery = Some(token.clone());
        self.recoveries_started.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    /// 启动检查读取同一份恢复记录，避免把未清理的删除伪装成完成。
    async fn pending_local_host_recovery(&self) -> AppResult<Option<LocalHostSaveRecoveryToken>> {
        Ok(self.recovery.lock().unwrap().clone())
    }
    /// 只清理当前操作的恢复记录，重复清理不存在的记录保持幂等。
    async fn finish_local_host_recovery(
        &self,
        token: &LocalHostSaveRecoveryToken,
    ) -> AppResult<()> {
        let mut recovery = self.recovery.lock().unwrap();
        match recovery.as_ref() {
            Some(current) if current != token => {
                Err(AppError::conflict("test recovery identity mismatch"))
            }
            Some(_) => {
                *recovery = None;
                self.recoveries_finished.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            None => Ok(()),
        }
    }
    /// 本回归不保存远程主机。
    async fn save_host(
        &self,
        _: HostId,
        _: HostDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<ManagedHost> {
        Err(AppError::unsupported("test"))
    }
    /// 本回归不删除远程主机。
    async fn delete_host(&self, _: &HostId, _: &ConfigRevision) -> AppResult<ManagedHost> {
        Err(AppError::unsupported("test"))
    }
    /// 返回空分组快照以完成面板真实初始化。
    async fn load_groups(&self) -> AppResult<HostGroupSnapshot> {
        Ok(HostGroupSnapshot {
            groups: Vec::new(),
            revision: None,
        })
    }
    /// 本回归不保存分组。
    async fn save_group(
        &self,
        _: HostGroupDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<HostGroupSnapshot> {
        Err(AppError::unsupported("test"))
    }
    /// 本回归不修改分组事务。
    async fn delete_group_and_reassign(
        &self,
        _: String,
        _: Option<String>,
        _: ConfigRevision,
        _: Vec<HostDocumentRevision>,
    ) -> AppResult<HostGroupDeletionReceipt> {
        Err(AppError::unsupported("test"))
    }
}

struct EmptyLocalProfiles;

#[async_trait]
impl ProfileRepository for EmptyLocalProfiles {
    /// 列表刷新正常读取空模板目录，与主机快照分开。
    async fn list_profiles(&self) -> AppResult<Vec<TerminalProfile>> {
        Ok(Vec::new())
    }
    /// 没有测试模板版本。
    async fn list_versioned_profiles(&self) -> AppResult<Vec<VersionedProfile>> {
        Ok(Vec::new())
    }
    /// 没有测试模板详情。
    async fn get_profile(&self, _: &str) -> AppResult<Option<VersionedProfile>> {
        Ok(None)
    }
    /// 本测试不保存模板。
    async fn save_profile(
        &self,
        _: ProfileDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<VersionedProfile> {
        Err(AppError::unsupported("test"))
    }
    /// 本测试不删除模板。
    async fn delete_profile(&self, _: &str, _: ConfigRevision) -> AppResult<bool> {
        Err(AppError::unsupported("test"))
    }
    /// 本测试不变更默认模板。
    async fn set_default_profile(
        &self,
        _: &str,
        _: Vec<ProfileRevision>,
    ) -> AppResult<VersionedProfile> {
        Err(AppError::unsupported("test"))
    }
}

struct NoLocalShell;

impl DefaultLocalShellProvider for NoLocalShell {
    /// 本回归不解析系统 Shell，确保没有外部环境依赖。
    fn resolve_default(&self) -> AppResult<LocalSessionTarget> {
        Err(AppError::unsupported("test"))
    }
}

struct NoRemoteService;

#[async_trait]
impl RemoteConnectionService for NoRemoteService {
    /// 本地主机删除回归不建立网络连接。
    async fn connect(
        &self,
        _: HostId,
        _: ConnectionIntent,
        _: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        Err(AppError::unsupported("test"))
    }
    /// 不探测远端密钥。
    async fn inspect_host_key(
        &self,
        _: HostId,
        _: HostKeyPeer,
        _: CancellationToken,
    ) -> AppResult<HostKeyChallenge> {
        Err(AppError::unsupported("test"))
    }
    /// 不写入远端密钥信任。
    async fn trust_host_key(
        &self,
        _: HostKeyTrustRequest,
        _: CancellationToken,
    ) -> AppResult<HostKeyTrustReceipt> {
        Err(AppError::unsupported("test"))
    }
    /// 不轮换远端密钥。
    async fn replace_host_key(
        &self,
        _: HostKeyRotationRequest,
        _: CancellationToken,
    ) -> AppResult<HostKeyRotationReceipt> {
        Err(AppError::unsupported("test"))
    }
}

/// 隔离仓储使用固定本地主机，不涉及用户配置、环境秘密或 OS 进程。
fn delayed_local_host_repository() -> Arc<DelayedLocalHosts> {
    Arc::new(DelayedLocalHosts {
        host: Mutex::new(Some(ManagedLocalHost {
            id: HostId::parse("local-delete-test").unwrap(),
            group_id: None,
            name: "Original".into(),
            shell: "test-shell".into(),
            args: Vec::new(),
            cwd: None,
            environment_ref: None,
            sort_order: 0,
            created_at: "created".into(),
            updated_at: "updated".into(),
            revision: ConfigRevision::from_sha256("1".repeat(64)),
            migration_origin: None,
        })),
        read_gate: Mutex::new(None),
        list_gate: Mutex::new(None),
        reads_started: AtomicUsize::new(0),
        lists_started: AtomicUsize::new(0),
        deleted_revisions: Mutex::new(Vec::new()),
        recovery: Mutex::new(None),
        recoveries_started: AtomicUsize::new(0),
        recoveries_finished: AtomicUsize::new(0),
    })
}

/// 真实面板、共享执行器和受控仓储组成无原生窗口的异步竞态测试。
fn local_delete_fixture(
    app: &mut TestAppContext,
    repository: Arc<DelayedLocalHosts>,
) -> (Runtime, Entity<ConnectionsPanel>, &mut VisualTestContext) {
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let hosts = HostManagementService::new(repository);
    let profiles = ProfileService::new(Arc::new(EmptyLocalProfiles), Arc::new(NoLocalShell));
    app.update(crate::init);
    let mounted = Rc::new(RefCell::new(None));
    let slot = mounted.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let panel = cx.new(|cx| {
            ConnectionsPanel::new(
                hosts,
                Arc::new(NoRemoteService),
                None,
                executor,
                ConfigurationRecoveryState {
                    mode: ConfigurationMode::ReadWrite,
                    issues: Vec::new(),
                },
                window,
                cx,
            )
            .with_profiles(profiles, cx)
        });
        *slot.borrow_mut() = Some(panel.clone());
        gpui_kit::base::Root::new(panel, window, cx)
    });
    let panel = mounted.borrow_mut().take().unwrap();
    pump_local_delete(&runtime, visual);
    (runtime, panel, visual)
}

/// 推进 Tokio 仓储回执与 GPUI 实体更新，显式受控的读取仍保持在途。
fn pump_local_delete(runtime: &Runtime, visual: &mut VisualTestContext) {
    for _ in 0..12 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
    visual.run_until_parked();
}

/// 根据生产按钮的真实 AccessKit Click 能力核对加载门禁，而不是推断可点击状态。
fn local_delete_confirm_clickable(visual: &mut VisualTestContext) -> bool {
    visual.update(|window, app| {
        window.draw(app).clear(app);
        let id = gpui_kit::ElementId::from("connections-delete-local-host-confirm");
        let button =
            gpui_kit::base::test_support::find(window, &[], &id).expect("删除确认按钮必须已绘制");
        assert_eq!(button.role(), Some(gpui_kit::Role::Button));
        button.supports_click()
    })
}

/// watcher 刷新不得使删除核对失效；取消不写入，再确认时必须使用最新 revision。
/// @author kongweiguang
#[gpui_kit::test]
fn local_delete_check_survives_watcher_reload_and_confirms_latest_revision(
    app: &mut TestAppContext,
) {
    let repository = delayed_local_host_repository();
    let (runtime, panel, visual) = local_delete_fixture(app, repository.clone());
    let latest_revision = ConfigRevision::from_sha256("2".repeat(64));
    {
        let mut host = repository.host.lock().unwrap();
        let host = host.as_mut().unwrap();
        host.name = "Current".into();
        host.revision = latest_revision.clone();
    }
    let gate = Arc::new(Semaphore::new(0));
    *repository.read_gate.lock().unwrap() = Some(gate.clone());
    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.request_delete_local_host("local-delete-test".into(), window, cx);
        })
    });
    pump_local_delete(&runtime, visual);
    assert_eq!(repository.reads_started.load(Ordering::SeqCst), 1);
    assert!(visual.update(|_, app| panel.read(app).local_hosts.delete_checking));

    // 配置 watcher 和手动刷新共用此生产入口，读取本身仍由真实 application service 执行。
    visual.update(|_, app| panel.update(app, |panel, cx| panel.reload_local_hosts(cx)));
    pump_local_delete(&runtime, visual);
    visual.update(|_, app| {
        let state = &panel.read(app).local_hosts;
        assert!(state.delete_checking);
        assert!(!state.loading);
        assert_eq!(state.rows[0].revision, latest_revision);
    });

    gate.add_permits(1);
    pump_local_delete(&runtime, visual);
    visual.update(|_, app| {
        let state = &panel.read(app).local_hosts;
        assert!(!state.delete_checking);
        let pending = state.pending_delete.as_ref().unwrap();
        assert_eq!(pending.profile.name, "Current");
        assert_eq!(pending.revision, latest_revision);
    });
    visual.update(|_, app| panel.update(app, |panel, cx| panel.cancel_delete_local_host(cx)));
    assert!(repository.deleted_revisions.lock().unwrap().is_empty());
    assert!(visual.update(|_, app| panel.read(app).local_hosts.pending_delete.is_none()));

    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.request_delete_local_host("local-delete-test".into(), window, cx);
        })
    });
    pump_local_delete(&runtime, visual);
    assert_eq!(
        visual.update(|_, app| panel
            .read(app)
            .local_hosts
            .pending_delete
            .as_ref()
            .map(|host| host.revision.clone())),
        Some(latest_revision.clone())
    );
    visual.update(|_, app| {
        let state = &panel.read(app).local_hosts;
        assert!(!state.loading, "列表读取未结束，不能确认删除");
        assert!(!state.mutation_blocked(), "删除提交仍被操作门禁阻止");
    });
    assert!(local_delete_confirm_clickable(visual));
    visual.update(|_, app| panel.update(app, |panel, cx| panel.confirm_delete_local_host(cx)));
    pump_local_delete(&runtime, visual);
    assert_eq!(
        *repository.deleted_revisions.lock().unwrap(),
        vec![latest_revision]
    );
    assert!(repository.host.lock().unwrap().is_none());
    assert_eq!(repository.recoveries_started.load(Ordering::SeqCst), 1);
    assert_eq!(repository.recoveries_finished.load(Ordering::SeqCst), 1);
    assert!(repository.recovery.lock().unwrap().is_none());
    visual.update(|_, app| {
        let state = &panel.read(app).local_hosts;
        assert!(!state.loading && !state.delete_checking && !state.saving);
        assert!(!state.cleanup_pending);
        assert!(state.pending_delete.is_none());
        assert!(state.rows.is_empty());
        assert!(!state.mutation_blocked());
    });
}

/// 删除核对不能使已在途的目录回执过期；两个读取各自结束后都必须解除忙碌状态。
/// @author kongweiguang
#[gpui_kit::test]
fn local_delete_check_preserves_inflight_list_completion(app: &mut TestAppContext) {
    let repository = delayed_local_host_repository();
    let (runtime, panel, visual) = local_delete_fixture(app, repository.clone());
    let gate = Arc::new(Semaphore::new(0));
    *repository.list_gate.lock().unwrap() = Some(gate.clone());
    visual.update(|_, app| panel.update(app, |panel, cx| panel.reload_local_hosts(cx)));
    pump_local_delete(&runtime, visual);
    assert_eq!(repository.lists_started.load(Ordering::SeqCst), 1);
    assert!(visual.update(|_, app| panel.read(app).local_hosts.loading));

    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.request_delete_local_host("local-delete-test".into(), window, cx);
        })
    });
    pump_local_delete(&runtime, visual);
    visual.update(|_, app| {
        let state = &panel.read(app).local_hosts;
        assert!(!state.delete_checking);
        assert!(state.pending_delete.is_some());
        assert!(state.loading);
    });
    assert!(!local_delete_confirm_clickable(visual));
    visual.update(|_, app| panel.update(app, |panel, cx| panel.confirm_delete_local_host(cx)));
    pump_local_delete(&runtime, visual);
    visual.update(|_, app| {
        let state = &panel.read(app).local_hosts;
        assert!(state.loading && !state.saving);
        assert!(state.pending_delete.is_some());
    });
    assert!(repository.deleted_revisions.lock().unwrap().is_empty());
    gate.add_permits(1);
    pump_local_delete(&runtime, visual);
    visual.update(|_, app| {
        let state = &panel.read(app).local_hosts;
        assert!(!state.loading && !state.delete_checking);
        assert!(state.pending_delete.is_some());
    });
    assert!(local_delete_confirm_clickable(visual));
    visual.update(|_, app| panel.update(app, |panel, cx| panel.cancel_delete_local_host(cx)));
    assert!(!visual.update(|_, app| panel.read(app).local_hosts.mutation_blocked()));
    assert!(repository.deleted_revisions.lock().unwrap().is_empty());
}

/// 清理阻塞不能让确认框遮住恢复入口；写入中和未知回执仍保留确认上下文。
/// @author kongweiguang
#[gpui_kit::test]
fn local_delete_confirmation_allows_cancel_when_cleanup_is_pending(app: &mut TestAppContext) {
    let repository = delayed_local_host_repository();
    let (runtime, panel, visual) = local_delete_fixture(app, repository.clone());
    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.request_delete_local_host("local-delete-test".into(), window, cx);
        })
    });
    pump_local_delete(&runtime, visual);
    assert!(visual.update(|_, app| panel.read(app).local_hosts.pending_delete.is_some()));

    visual.update(|_, app| {
        panel.update(app, |panel, cx| {
            panel.local_hosts.saving = true;
            panel.cancel_delete_local_host(cx);
            assert!(panel.local_hosts.pending_delete.is_some());
            panel.local_hosts.saving = false;
            panel.local_hosts.save_outcome_unknown = true;
            panel.cancel_delete_local_host(cx);
            assert!(panel.local_hosts.pending_delete.is_some());
            panel.local_hosts.save_outcome_unknown = false;
            panel.local_hosts.cleanup_pending = true;
            cx.notify();
        });
    });
    assert!(!local_delete_confirm_clickable(visual));
    visual.update(|window, app| {
        window.draw(app).clear(app);
        let id = gpui_kit::ElementId::from("connections-delete-local-host-cancel");
        let cancel = gpui_kit::base::test_support::find(window, &[], &id).unwrap();
        assert!(cancel.supports_click());
    });
    visual.update(|_, app| panel.update(app, |panel, cx| panel.confirm_delete_local_host(cx)));
    assert!(repository.deleted_revisions.lock().unwrap().is_empty());
    visual.update(|_, app| panel.update(app, |panel, cx| panel.cancel_delete_local_host(cx)));
    visual.update(|window, app| {
        let state = &panel.read(app).local_hosts;
        assert!(state.pending_delete.is_none());
        assert!(state.cleanup_pending);
        window.draw(app).clear(app);
        let id = gpui_kit::ElementId::from("connections-review-local-host-unknown-banner");
        let review = gpui_kit::base::test_support::find(window, &[], &id)
            .expect("关闭确认后必须显示清理恢复入口");
        assert!(review.supports_click());
    });
}

/// 删除确认必须以限定宽度的居中弹层出现。
/// 确认卡一旦回到面板内容流，就会把空状态工作区撑成横跨窗口的大弹框。
///
/// @author kongweiguang
#[gpui_kit::test]
fn local_delete_confirmation_is_a_bounded_centered_overlay(app: &mut TestAppContext) {
    let repository = delayed_local_host_repository();
    let (runtime, panel, visual) = local_delete_fixture(app, repository);
    visual.simulate_resize(size(px(1280.), px(800.)));
    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.request_delete_local_host("local-delete-test".into(), window, cx);
        })
    });
    pump_local_delete(&runtime, visual);
    assert!(visual.update(|_, app| panel.read(app).local_hosts.pending_delete.is_some()));
    visual.update(|window, app| {
        window.draw(app).clear(app);
    });

    let overlay = visual
        .debug_bounds("connections-delete-local-host-overlay-bounds")
        .expect("删除确认遮罩必须覆盖面板");
    let card = visual
        .debug_bounds("rr-connections-delete-local-host-card")
        .expect("删除确认卡必须已绘制");
    assert!(overlay.size.width >= px(1279.), "遮罩应覆盖整个面板宽度");
    assert!(overlay.size.height >= px(799.), "遮罩应覆盖整个面板高度");
    assert!(
        card.size.width <= px(520.),
        "确认卡宽度必须受限，实际 {}",
        card.size.width
    );
    assert!(
        card.size.height < overlay.size.height * 0.5,
        "确认卡不应占满面板高度"
    );
    assert!(
        (card.center().x - overlay.center().x).abs() <= px(1.),
        "确认卡必须水平居中"
    );
    assert!(
        (card.center().y - overlay.center().y).abs() <= px(1.),
        "确认卡必须垂直居中"
    );
}

/// 点击遮罩等同取消：删除确认不因误触背景而丢失，也不在取消后继续滞留。
///
/// @author kongweiguang
#[gpui_kit::test]
fn local_delete_confirmation_dismisses_from_scrim_click(app: &mut TestAppContext) {
    let repository = delayed_local_host_repository();
    let (runtime, panel, visual) = local_delete_fixture(app, repository.clone());
    visual.simulate_resize(size(px(1280.), px(800.)));
    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.request_delete_local_host("local-delete-test".into(), window, cx);
        })
    });
    pump_local_delete(&runtime, visual);
    assert!(visual.update(|_, app| panel.read(app).local_hosts.pending_delete.is_some()));
    visual.update(|window, app| {
        window.draw(app).clear(app);
    });
    let overlay = visual
        .debug_bounds("connections-delete-local-host-overlay-bounds")
        .expect("删除确认遮罩必须覆盖面板");
    visual.simulate_click(
        point(overlay.origin.x + px(6.), overlay.origin.y + px(6.)),
        Default::default(),
    );
    visual.update(|window, app| {
        window.draw(app).clear(app);
    });
    assert!(
        visual.update(|_, app| panel.read(app).local_hosts.pending_delete.is_none()),
        "点击遮罩必须取消删除确认"
    );
    assert!(repository.deleted_revisions.lock().unwrap().is_empty());
}

/// Escape 与遮罩点击共用同一取消入口；确认中的删除不会被按键穿透提交。
///
/// @author kongweiguang
#[gpui_kit::test]
fn escape_cancels_pending_delete_confirmation(app: &mut TestAppContext) {
    let repository = delayed_local_host_repository();
    let (runtime, panel, visual) = local_delete_fixture(app, repository.clone());
    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.request_delete_local_host("local-delete-test".into(), window, cx);
        })
    });
    pump_local_delete(&runtime, visual);
    assert!(visual.update(|_, app| panel.read(app).local_hosts.pending_delete.is_some()));
    visual.update(|window, app| {
        let event = KeyDownEvent {
            keystroke: Keystroke {
                modifiers: Default::default(),
                key: "escape".to_owned(),
                key_char: None,
            },
            is_held: false,
            prefer_character_input: false,
        };
        panel.update(app, |panel, cx| {
            panel.handle_confirmation_escape(&event, window, cx)
        });
    });
    assert!(
        !visual.update(|_, app| panel.read(app).has_pending_deletion_confirmation()),
        "Escape 必须取消最上层删除确认"
    );
    assert!(repository.deleted_revisions.lock().unwrap().is_empty());
}

/// 取消在途只读核对立即释放门禁；旧回执不能覆盖之后重新发起的确认。
/// @author kongweiguang
#[gpui_kit::test]
fn cancelled_local_delete_check_rejects_late_receipt(app: &mut TestAppContext) {
    let repository = delayed_local_host_repository();
    let (runtime, panel, visual) = local_delete_fixture(app, repository.clone());
    let gate = Arc::new(Semaphore::new(0));
    *repository.read_gate.lock().unwrap() = Some(gate.clone());
    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.request_delete_local_host("local-delete-test".into(), window, cx);
        })
    });
    pump_local_delete(&runtime, visual);
    assert_eq!(repository.reads_started.load(Ordering::SeqCst), 1);
    visual.update(|_, app| panel.update(app, |panel, cx| panel.cancel_delete_local_host(cx)));
    assert!(!visual.update(|_, app| panel.read(app).local_hosts.mutation_blocked()));

    let latest = ConfigRevision::from_sha256("2".repeat(64));
    repository.host.lock().unwrap().as_mut().unwrap().revision = latest.clone();
    visual.update(|window, app| {
        panel.update(app, |panel, cx| {
            panel.request_delete_local_host("local-delete-test".into(), window, cx);
        })
    });
    pump_local_delete(&runtime, visual);
    gate.add_permits(1);
    pump_local_delete(&runtime, visual);
    visual.update(|_, app| {
        let state = &panel.read(app).local_hosts;
        assert!(!state.delete_checking && !state.loading);
        assert_eq!(state.pending_delete.as_ref().unwrap().revision, latest);
    });
    visual.update(|_, app| panel.update(app, |panel, cx| panel.cancel_delete_local_host(cx)));
    assert!(repository.deleted_revisions.lock().unwrap().is_empty());
}
