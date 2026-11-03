//! 终端模板与持久本地主机分别使用独立用例；表单仅保存用户确认的主机快照。
//!
//! @author kongweiguang

use std::collections::BTreeMap;

use gpui_kit::{
    AppContext as _, Context, Entity, Focusable, ScrollHandle, SharedString, Subscription, Window,
    component::{
        input::{InputEvent, InputState, TextareaState},
        searchable_list::SearchableListItem,
        select::{SelectEvent, SelectState},
    },
};
use kerminal_application::{
    AppError, ProfileService, PublicErrorCode,
    connections::management::{
        CredentialCleanupStatus, HostDeletionReceipt, LocalHostDraft, LocalHostSaveRecoveryToken,
        ManagedLocalHost, RecoverableLocalHostSaveOutcome,
    },
    preferences::{ProfileDraft, VersionedProfile},
};
use kerminal_domain::{
    identity::HostId,
    preferences::{ConfigRevision, ConfigurationMode, TerminalProfile},
    sessions::LocalSessionTarget,
};

use super::{
    form::create_input,
    panel::{
        ConnectionsEditorKind, ConnectionsEvent, ConnectionsPanel, HostFilePickerTarget,
        SavedSelectionTarget, saved_selection_event,
    },
};

/// 将独立本地主机映射到旧表单的私有显示模型；列表调用方传空环境，只有编辑时才解密。
/// @author kongweiguang
pub(super) fn local_host_projection(
    host: ManagedLocalHost,
    env: BTreeMap<String, String>,
) -> VersionedProfile {
    VersionedProfile {
        profile: TerminalProfile {
            id: host.id.as_str().to_owned(),
            name: host.name,
            shell: host.shell,
            args: host.args,
            cwd: host.cwd,
            env,
            is_default: false,
            sidebar_group_id: host.group_id,
            sort_order: host.sort_order,
            created_at: host.created_at,
            updated_at: host.updated_at,
        },
        revision: host.revision,
    }
}

/// 校验 UI 携带的稳定标识后才访问主机仓储，不以默认模板替代无效目标。
/// @author kongweiguang
fn local_host_id(value: &str) -> Result<HostId, AppError> {
    HostId::parse(value).map_err(|_| AppError::invalid("本地主机标识无效"))
}

/// 在提交边界转换表单快照，确保主机与模板使用不同的持久化用例。
/// @author kongweiguang
fn local_host_draft(value: ProfileDraft) -> Result<LocalHostDraft, AppError> {
    Ok(LocalHostDraft {
        id: value.id.as_deref().map(local_host_id).transpose()?,
        group_id: value.sidebar_group_id,
        name: value.name,
        shell: value.shell,
        args: value.args,
        cwd: value.cwd,
        env: value.env,
        sort_order: value.sort_order,
    })
}

/// 本地主机表单模式；复制使用独立 ID 与空 revision，不继承默认项状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LocalHostEditorMode {
    Create,
    Edit,
    Duplicate,
}

/// 本地编辑器仅用两类导航区承载连接身份与终端启动参数，缩窄布局可折叠到选择器。
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum LocalHostEditorSection {
    #[default]
    Properties,
    Terminal,
}

/// 本地 Profile 校验只聚焦已有控件，并按编辑器分类切换到实际承载字段的页面。
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LocalHostValidationField {
    Name,
    Group,
    Shell,
    WorkingDirectory,
    Arguments,
    Environment,
}

impl LocalHostValidationField {
    /// 身份字段留在属性页，启动参数与环境变量转到终端页以让错误能被直接看到。
    /// @author kongweiguang
    fn section(self) -> LocalHostEditorSection {
        match self {
            Self::Name | Self::Group | Self::Shell | Self::WorkingDirectory => {
                LocalHostEditorSection::Properties
            }
            Self::Arguments | Self::Environment => LocalHostEditorSection::Terminal,
        }
    }
}

/// 编辑/复制异步读取的精确意图；失败时保留以供显式重试，关闭时按代次失效。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct LocalHostEditorLoadIntent {
    pub(super) profile_id: String,
    pub(super) mode: LocalHostEditorMode,
    pub(super) generation: u64,
}

/// 根确认弹层关闭脏表单后继续用户原先请求的动作，避免确认后要求重复点击。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum LocalHostPendingAction {
    Create(Option<String>),
    Edit(String),
    Duplicate(String),
    Select(Option<String>),
    Delete(String),
}

/// 持久操作未知时保留目标身份，显式读回后才能解除写入门禁。
#[derive(Clone, Debug, Eq, PartialEq)]
enum LocalHostUnknownMutation {
    Save {
        profile_id: String,
    },
    Delete {
        profile_id: String,
        revision: ConfigRevision,
    },
}

/// 统一提供“未分组、现存分组、已失效分组”选择项，保留旧 group ID 供用户明确调整。
#[derive(Clone)]
pub(super) struct LocalHostGroupOption {
    pub(super) id: Option<String>,
    name: LocalHostGroupName,
}

#[derive(Clone)]
enum LocalHostGroupName {
    Ungrouped,
    Missing(String),
    Named(SharedString),
}

impl SearchableListItem for LocalHostGroupOption {
    type Value = Option<String>;

    /// 下拉搜索展示本地化组名，但业务值始终是稳定 group ID 而非显示名称。
    fn title(&self) -> SharedString {
        match &self.name {
            LocalHostGroupName::Ungrouped => crate::locale::text("未分组", "Ungrouped"),
            LocalHostGroupName::Missing(id) => crate::locale::text(
                format!("已删除分组（{id}）"),
                format!("Missing group ({id})"),
            ),
            LocalHostGroupName::Named(name) => name.clone(),
        }
    }

    /// Option ID 同时表达未分组，不引入可能与真实分组冲突的哨兵值。
    fn value(&self) -> &Self::Value {
        &self.id
    }
}

/// 为 Kit 下拉保留未分组与失效组提示，同时不自动把旧配置改成别的组。
fn local_host_group_options(
    groups: &[kerminal_application::connections::management::HostGroup],
    selected: Option<&str>,
) -> Vec<LocalHostGroupOption> {
    let mut options = Vec::with_capacity(groups.len() + 2);
    options.push(LocalHostGroupOption {
        id: None,
        name: LocalHostGroupName::Ungrouped,
    });
    options.extend(groups.iter().map(|group| LocalHostGroupOption {
        id: Some(group.id.clone()),
        name: LocalHostGroupName::Named(SharedString::from(group.name.clone())),
    }));
    if let Some(id) = selected.filter(|id| !groups.iter().any(|group| group.id == *id)) {
        options.push(LocalHostGroupOption {
            id: Some(id.to_owned()),
            name: LocalHostGroupName::Missing(id.to_owned()),
        });
    }
    options
}

/// 持久本地主机编辑器复用 Kit 控件；revision 与 typed baseline 保护外部并发修改。
pub(super) struct LocalHostEditor {
    pub(super) profile_id: String,
    pub(super) mode: LocalHostEditorMode,
    pub(super) section: LocalHostEditorSection,
    pub(super) scroll_handle: ScrollHandle,
    pub(super) revision: Option<ConfigRevision>,
    pub(super) name: Entity<InputState>,
    pub(super) group_select: Entity<SelectState<Vec<LocalHostGroupOption>>>,
    pub(super) shell: Entity<InputState>,
    pub(super) cwd: Entity<InputState>,
    pub(super) args: Entity<TextareaState>,
    pub(super) env: Entity<TextareaState>,
    pub(super) group_id: Option<String>,
    pub(super) dirty: bool,
    pub(super) validation_issue: Option<String>,
    validation_field: Option<LocalHostValidationField>,
    pub(super) external_conflict: bool,
    baseline: LocalHostInputBaseline,
    sort_order: i64,
    _group_subscription: Subscription,
    _input_subscriptions: Vec<Subscription>,
}

/// 保留原始输入快照，使无效 env 文本也能正确参与 dirty/关闭确认判断。
#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalHostInputBaseline {
    name: String,
    shell: String,
    cwd: String,
    args: String,
    env: String,
    group_id: Option<String>,
}

/// 仅保留被协议切换中断的默认目标意图；显式应用仍以发起时快照保护后续输入。
pub(super) struct LocalHostDefaultRequest {
    profile_id: Option<String>,
    explicit_override: Option<LocalHostInputBaseline>,
}

/// 默认本地目标的可编辑字段；不包含名称或分组，避免系统探测改写用户身份信息。
#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalHostDefaultValues {
    shell: String,
    cwd: String,
    args: String,
    env: String,
}

/// 逐字段标识仍可由系统默认值填入的字段；用户已编辑的输入保持原样。
#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalHostDefaultProjection {
    shell: Option<String>,
    cwd: Option<String>,
    args: Option<String>,
    env: Option<String>,
    baseline: LocalHostInputBaseline,
}

/// 只投影仍等于打开时基线的字段，迟到的默认目标回执不能覆盖用户编辑。
fn project_untouched_local_defaults(
    baseline: &LocalHostInputBaseline,
    current: &LocalHostInputBaseline,
    defaults: LocalHostDefaultValues,
) -> LocalHostDefaultProjection {
    let mut next_baseline = baseline.clone();
    let shell = (current.shell == baseline.shell).then_some(defaults.shell);
    let cwd = (current.cwd == baseline.cwd).then_some(defaults.cwd);
    let args = (current.args == baseline.args).then_some(defaults.args);
    let env = (current.env == baseline.env).then_some(defaults.env);
    if let Some(value) = &shell {
        next_baseline.shell = value.clone();
    }
    if let Some(value) = &cwd {
        next_baseline.cwd = value.clone();
    }
    if let Some(value) = &args {
        next_baseline.args = value.clone();
    }
    if let Some(value) = &env {
        next_baseline.env = value.clone();
    }
    LocalHostDefaultProjection {
        shell,
        cwd,
        args,
        env,
        baseline: next_baseline,
    }
}

/// 用户主动恢复 Profile 时可覆盖点击时已有值，但异步期间新输入始终优先。
/// @author kongweiguang
fn project_explicit_local_defaults(
    expected: &LocalHostInputBaseline,
    current: &LocalHostInputBaseline,
    defaults: LocalHostDefaultValues,
    baseline: &LocalHostInputBaseline,
) -> LocalHostDefaultProjection {
    LocalHostDefaultProjection {
        shell: (current.shell == expected.shell).then_some(defaults.shell),
        cwd: (current.cwd == expected.cwd).then_some(defaults.cwd),
        args: (current.args == expected.args).then_some(defaults.args),
        env: (current.env == expected.env).then_some(defaults.env),
        baseline: baseline.clone(),
    }
}

impl LocalHostEditor {
    /// 从当前输入和 group 选择计算 dirty；不解析或丢弃暂时无效的用户文本。
    fn is_dirty(&self, cx: &gpui_kit::App) -> bool {
        self.name.read(cx).value().as_ref() != self.baseline.name
            || self.shell.read(cx).value().as_ref() != self.baseline.shell
            || self.cwd.read(cx).value().as_ref() != self.baseline.cwd
            || self.args.read(cx).value().as_ref() != self.baseline.args
            || self.env.read(cx).value().as_ref() != self.baseline.env
            || self.group_id != self.baseline.group_id
    }

    /// 新建与复制即使输入仍是初始值也需创建；只有已有版本且原始输入未改时可跳过保存。
    fn needs_save(&self, cx: &gpui_kit::App) -> bool {
        local_host_needs_save(self.revision.as_ref(), self.is_dirty(cx))
    }

    /// 仅在提交边界把多行编辑文本转成应用 ProfileDraft；ProfileService 仍是最终校验者。
    fn draft(&self, cx: &Context<ConnectionsPanel>) -> Result<ProfileDraft, String> {
        let args_text = self.args.read(cx).value().to_string();
        let env_text = self.env.read(cx).value().to_string();
        Ok(ProfileDraft {
            id: Some(self.profile_id.clone()),
            name: self.name.read(cx).value().to_string(),
            shell: self.shell.read(cx).value().to_string(),
            args: parse_profile_arguments(&args_text),
            cwd: optional_path(self.cwd.read(cx).value().as_ref()),
            env: parse_profile_environment(&env_text)?,
            sidebar_group_id: self.group_id.clone(),
            sort_order: self.sort_order,
        })
    }
}

/// 把错误定位到真实的本地 Profile 控件；无对应输入的未知错误由调用方仅显示状态消息。
/// @author kongweiguang
fn local_host_focus_handle(
    editor: &LocalHostEditor,
    field: LocalHostValidationField,
    cx: &gpui_kit::App,
) -> gpui_kit::FocusHandle {
    match field {
        LocalHostValidationField::Name => editor.name.read(cx).focus_handle(cx),
        LocalHostValidationField::Group => editor.group_select.read(cx).focus_handle(cx),
        LocalHostValidationField::Shell => editor.shell.read(cx).focus_handle(cx),
        LocalHostValidationField::WorkingDirectory => editor.cwd.read(cx).focus_handle(cx),
        LocalHostValidationField::Arguments => editor.args.read(cx).focus_handle(cx),
        LocalHostValidationField::Environment => editor.env.read(cx).focus_handle(cx),
    }
}

/// 将“是否需要写入”集中为生产逻辑和纯回归共同使用的最小判定。
fn local_host_needs_save(revision: Option<&ConfigRevision>, raw_dirty: bool) -> bool {
    revision.is_none() || raw_dirty
}

/// 将应用层主机校验映射到实际字段，复合错误按相同 cwd/argv 约束定位。
/// @author kongweiguang
pub(super) fn local_host_validation_field(
    message: &str,
    arguments_text: &str,
    working_directory: &str,
) -> Option<LocalHostValidationField> {
    match message {
        "profile 名称不能为空或过长" | "本地主机名称不能为空或过长" => {
            Some(LocalHostValidationField::Name)
        }
        "profile shell 路径不能为空或过长" | "本地主机 shell 路径不能为空或过长" => {
            Some(LocalHostValidationField::Shell)
        }
        "本地主机环境变量超出限制" => Some(LocalHostValidationField::Environment),
        "profile 参数或环境变量超出限制" | "本地主机参数或工作目录超出限制" =>
        {
            if working_directory.len() > 4096 || working_directory.contains('\0') {
                return Some(LocalHostValidationField::WorkingDirectory);
            }
            let arguments_exceed_limit = parse_profile_arguments(arguments_text)
                .iter()
                .any(|argument| argument.len() > 16 * 1024 || argument.contains('\0'));
            Some(if arguments_exceed_limit {
                LocalHostValidationField::Arguments
            } else {
                LocalHostValidationField::Environment
            })
        }
        _ => None,
    }
}

/// 本地主机和模板各持一份投影，写入未知与凭据清理由同一门禁管理。
pub(super) struct LocalHostState {
    pub(super) service: Option<ProfileService>,
    /// 模板只供启动参数选择，本地主机的 CRUD 始终使用 rows 和主机服务。
    pub(super) templates: Vec<VersionedProfile>,
    pub(super) rows: Vec<VersionedProfile>,
    pub(super) selected_id: Option<String>,
    pub(super) selected: Option<VersionedProfile>,
    pub(super) editor: Option<LocalHostEditor>,
    pub(super) editor_load_intent: Option<LocalHostEditorLoadIntent>,
    pub(super) editor_load_pending: bool,
    pub(super) editor_load_error: Option<String>,
    pub(super) loading: bool,
    pub(super) saving: bool,
    pub(super) save_outcome_unknown: bool,
    pub(super) cleanup_pending: bool,
    startup_recovery_checked: bool,
    pub(super) pending_delete: Option<VersionedProfile>,
    pub(super) status: Option<String>,
    pub(super) delete_checking: bool,
    pub(super) readback_in_progress: bool,
    pub(super) unknown_readback: Option<VersionedProfile>,
    pub(super) unknown_target_missing: bool,
    pub(super) pending_action: Option<LocalHostPendingAction>,
    unknown_mutation: Option<LocalHostUnknownMutation>,
    unknown_save_recovery: Option<LocalHostSaveRecoveryToken>,
    generation: u64,
    delete_check_generation: u64,
    editor_load_generation: u64,
    editor_generation: u64,
    default_target_generation: u64,
    pub(super) default_target_request: Option<LocalHostDefaultRequest>,
}

/// 列表快照与一次性核对结果共同返回，避免读回秘密进入普通列表投影。
struct LocalHostCatalogLoad {
    rows: Vec<VersionedProfile>,
    templates: Vec<VersionedProfile>,
    save_readback: Option<Option<VersionedProfile>>,
    startup_recovery: Option<Result<CredentialCleanupStatus, AppError>>,
}

impl Default for LocalHostState {
    /// 构造不触碰磁盘的空状态，服务注入后再由应用 executor 执行首次读取。
    fn default() -> Self {
        Self {
            service: None,
            templates: Vec::new(),
            rows: Vec::new(),
            selected_id: None,
            selected: None,
            editor: None,
            editor_load_intent: None,
            editor_load_pending: false,
            editor_load_error: None,
            loading: false,
            saving: false,
            save_outcome_unknown: false,
            cleanup_pending: false,
            startup_recovery_checked: false,
            pending_delete: None,
            status: None,
            delete_checking: false,
            readback_in_progress: false,
            unknown_readback: None,
            unknown_target_missing: false,
            pending_action: None,
            unknown_mutation: None,
            unknown_save_recovery: None,
            generation: 0,
            delete_check_generation: 0,
            editor_load_generation: 0,
            editor_generation: 0,
            default_target_generation: 0,
            default_target_request: None,
        }
    }
}

impl LocalHostState {
    /// 面板构造后注入既有服务；状态本身不创建仓储或独立文件路径。
    pub(super) fn set_service(&mut self, service: ProfileService) {
        self.service = Some(service);
    }

    /// 任何仍未核对的持久化结果都禁止再次写入，避免把未知提交重放成重复操作。
    pub(super) fn mutation_blocked(&self) -> bool {
        self.saving
            || self.save_outcome_unknown
            || self.cleanup_pending
            || self.delete_checking
            || self.readback_in_progress
            || self.editor_load_intent.is_some()
    }

    /// 读回可在未知结果期间启动，但任何并发写入、读回或删除目标核对都互斥。
    fn operation_in_progress(&self) -> bool {
        self.saving || self.delete_checking || self.readback_in_progress || self.editor_load_pending
    }

    /// 显式核对独占本轮列表读取；旧快照先失效，watcher 不能打断它或把失败伪装成已有结果。
    /// @author kongweiguang
    fn begin_list_load(&mut self, explicit_readback: bool) -> Option<u64> {
        if self.saving || self.readback_in_progress {
            return None;
        }
        self.generation = self.generation.wrapping_add(1);
        self.loading = true;
        self.readback_in_progress = explicit_readback;
        if explicit_readback {
            self.unknown_readback = None;
            self.unknown_target_missing = false;
        }
        Some(self.generation)
    }

    /// 仅让仍持有同一 profile、模式和代次的活动请求接收读取回执。
    fn accepts_editor_load(
        &self,
        profile_id: &str,
        mode: LocalHostEditorMode,
        generation: u64,
    ) -> bool {
        self.editor_load_pending
            && self.editor_load_intent.as_ref().is_some_and(|intent| {
                intent.profile_id == profile_id
                    && intent.mode == mode
                    && intent.generation == generation
            })
    }

    /// 关闭或取消读取时使旧回执失效；不触碰保存未知结果及其核对所有权。
    pub(super) fn cancel_editor_load(&mut self) {
        self.editor_load_generation = self.editor_load_generation.wrapping_add(1);
        self.editor_load_pending = false;
        self.editor_load_intent = None;
        self.editor_load_error = None;
    }

    /// 仅允许统一表单关闭撤销纯读取意图；写入未知、删除核对或读回应继续保留所有权。
    /// @author kongweiguang
    pub(super) fn can_cancel_editor_load(&self) -> bool {
        self.editor_load_intent.is_some()
            && !self.saving
            && !self.save_outcome_unknown
            && !self.delete_checking
            && !self.readback_in_progress
    }
}

impl ConnectionsPanel {
    /// 分区切换保留草稿并交接可见字段焦点，避免隐藏输入使 Esc 与 Tab 失去表单路径。
    /// @author kongweiguang
    pub(super) fn select_local_host_section(
        &mut self,
        section: LocalHostEditorSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = self.local_hosts.editor.as_mut()
            && editor.section != section
        {
            editor.scroll_handle.set_offset(Default::default());
            editor.section = section;
            self.focus_editor_primary_input(window, cx);
            cx.notify();
        }
    }

    /// 从同一 ProfileService 按 ID 读取最新启动字段，异步期间的新输入仍由逐字段门禁保护。
    ///
    /// @author kongweiguang
    pub(super) fn use_local_profile(&mut self, profile_id: String, cx: &mut Context<Self>) {
        let Some(editor) = self.local_hosts.editor.as_ref() else {
            return;
        };
        let expected = read_editor_baseline(editor, cx);
        self.resolve_local_profile_target(Some(profile_id), Some(expected), cx);
    }

    /// 只有启动字段完整匹配且唯一时才把输入标为某个 Profile，避免同名程序的不同参数被误认。
    ///
    /// @author kongweiguang
    pub(super) fn selected_local_profile_id(&self, cx: &Context<Self>) -> Option<String> {
        let editor = self.local_hosts.editor.as_ref()?;
        let current = read_editor_baseline(editor, cx);
        let mut matches = self.local_hosts.templates.iter().filter(|row| {
            row.profile.shell == current.shell
                && row.profile.cwd.as_deref().unwrap_or_default() == current.cwd
                && row.profile.args.join("\n") == current.args
                && format_environment(&row.profile.env) == current.env
        });
        let first = matches.next()?;
        matches.next().is_none().then(|| first.profile.id.clone())
    }

    /// 文件对话框回执只更新仍活动的可编辑本地主机输入；无效路径保留原草稿并给出可读错误。
    /// @author kongweiguang
    pub(super) fn set_local_host_picker_path(
        &mut self,
        target: HostFilePickerTarget,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(editor) = self.local_hosts.editor.as_mut() else {
            return false;
        };
        let input = match target {
            HostFilePickerTarget::LocalShell => &editor.shell,
            HostFilePickerTarget::LocalWorkingDirectory => &editor.cwd,
            _ => return false,
        };
        input.update(cx, |state, cx| state.set_value(path.to_owned(), window, cx));
        self.refresh_local_host_dirty(cx);
        true
    }

    /// 自动填充与显式选择共用异步回执门禁；成功由表单值反馈，避免状态提示挤动字段。
    /// @author kongweiguang
    fn resolve_local_profile_target(
        &mut self,
        profile_id: Option<String>,
        explicit_override: Option<LocalHostInputBaseline>,
        cx: &mut Context<Self>,
    ) {
        let Some(service) = self.local_hosts.service.clone() else {
            self.status = Some(
                crate::locale::text(
                    "默认终端 Profile 服务尚未就绪，可继续手动填写。",
                    "The default terminal Profile service is not ready. You can continue manually.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let Some(editor_id) = self
            .local_hosts
            .editor
            .as_ref()
            .map(|editor| editor.profile_id.clone())
        else {
            return;
        };
        self.local_hosts.default_target_request = Some(LocalHostDefaultRequest {
            profile_id: profile_id.clone(),
            explicit_override: explicit_override.clone(),
        });
        self.local_hosts.default_target_generation =
            self.local_hosts.default_target_generation.wrapping_add(1);
        let default_generation = self.local_hosts.default_target_generation;
        let editor_generation = self.local_hosts.editor_generation;
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move {
                    match profile_id {
                        Some(id) => service
                            .local_session_target(&id)
                            .await?
                            .ok_or_else(|| AppError::not_found("Profile 已不存在，请重新读取")),
                        None => service.default_local_session_target().await,
                    }
                })
                .await;
            let _ = this.update_in(cx, move |panel, window, cx| {
                if panel.local_hosts.default_target_generation != default_generation
                    || panel.local_hosts.editor_generation != editor_generation
                    || panel
                        .local_hosts
                        .editor
                        .as_ref()
                        .map(|editor| editor.profile_id.as_str())
                        != Some(editor_id.as_str())
                    || panel.active_editor_kind != Some(ConnectionsEditorKind::Local)
                {
                    return;
                }
                panel.local_hosts.default_target_request = None;
                match result {
                    Ok(target) => match local_target_to_profile_fields(target) {
                        Ok((shell, args, cwd, env)) => {
                            if let Some(editor) = panel.local_hosts.editor.as_mut() {
                                let current = read_editor_baseline(editor, cx);
                                let projection = if let Some(expected) = explicit_override.as_ref()
                                {
                                    project_explicit_local_defaults(
                                        expected,
                                        &current,
                                        LocalHostDefaultValues {
                                            shell,
                                            cwd: cwd.unwrap_or_default(),
                                            args: args.join("\n"),
                                            env: format_environment(&env),
                                        },
                                        &editor.baseline,
                                    )
                                } else {
                                    project_untouched_local_defaults(
                                        &editor.baseline,
                                        &current,
                                        LocalHostDefaultValues {
                                            shell,
                                            cwd: cwd.unwrap_or_default(),
                                            args: args.join("\n"),
                                            env: format_environment(&env),
                                        },
                                    )
                                };
                                if let Some(value) = projection.shell {
                                    editor
                                        .shell
                                        .update(cx, |state, cx| state.set_value(value, window, cx));
                                }
                                if let Some(value) = projection.cwd {
                                    editor
                                        .cwd
                                        .update(cx, |state, cx| state.set_value(value, window, cx));
                                }
                                if let Some(value) = projection.args {
                                    editor
                                        .args
                                        .update(cx, |state, cx| state.set_value(value, window, cx));
                                }
                                if let Some(value) = projection.env {
                                    editor
                                        .env
                                        .update(cx, |state, cx| state.set_value(value, window, cx));
                                }
                                editor.baseline = projection.baseline;
                                editor.dirty = editor.is_dirty(cx);
                            }
                            panel.status = None;
                        }
                        Err(message) => panel.status = Some(message),
                    },
                    Err(error) => panel.status = Some(super::panel::localized_error(&error)),
                }
                panel.local_hosts.status = panel.status.clone();
                cx.notify();
            });
        })
        .detach();
    }

    /// 关闭或替换表单会同时丢弃默认目标意图，避免以后打开另一表单时重放旧请求。
    /// @author kongweiguang
    pub(super) fn cancel_local_host_background_requests(&mut self) {
        self.local_hosts.default_target_generation =
            self.local_hosts.default_target_generation.wrapping_add(1);
        self.local_hosts.default_target_request = None;
    }

    /// 仅恢复切协议留下的在途意图；已完成、失败或被关闭的请求不会反复覆盖初值。
    /// @author kongweiguang
    pub(super) fn resume_local_host_default_target(&mut self, cx: &mut Context<Self>) {
        if let Some(request) = self.local_hosts.default_target_request.take() {
            self.resolve_local_profile_target(request.profile_id, request.explicit_override, cx);
        }
    }

    /// 新建分组返回时刷新分组选项并选择刚创建的稳定 ID，保留当前本地主机表单基线。
    /// @author kongweiguang
    pub(super) fn select_local_host_group_after_inline_create(
        &mut self,
        group_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let options = local_host_group_options(&self.groups, Some(&group_id));
        let Some(editor) = self.local_hosts.editor.as_mut() else {
            return;
        };
        editor.group_select.update(cx, |select, cx| {
            select.set_items(options, window, cx);
            select.set_selected_value(&Some(group_id.clone()), window, cx);
            select.focus(window, cx);
        });
        editor.group_id = Some(group_id);
        editor.validation_issue = None;
        editor.validation_field = None;
        self.refresh_local_host_dirty(cx);
    }

    /// 列表刷新由 watcher 或显式刷新调用；dirty editor 保留原输入并只标记外部冲突。
    pub(crate) fn reload_local_hosts(&mut self, cx: &mut Context<Self>) {
        self.load_local_profiles(false, cx);
    }

    /// 未知结果必须由用户显式核对；普通 watcher 刷新不会擅自解除门禁。
    pub(super) fn review_local_host_unknown(&mut self, cx: &mut Context<Self>) {
        if (!self.local_hosts.save_outcome_unknown && !self.local_hosts.cleanup_pending)
            || self.local_hosts.operation_in_progress()
        {
            return;
        }
        self.load_local_profiles(true, cx);
    }

    /// 只重试当前失败的读取意图，不把普通列表刷新或未知写入核对隐式变成编辑请求。
    pub(super) fn retry_local_host_editor_load(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.local_hosts.editor_load_pending
            || self.local_hosts.saving
            || self.local_hosts.save_outcome_unknown
            || self.local_hosts.delete_checking
            || self.local_hosts.readback_in_progress
        {
            return;
        }
        let Some(intent) = self.local_hosts.editor_load_intent.clone() else {
            return;
        };
        self.load_local_profile_editor(intent.profile_id, intent.mode, window, cx);
    }

    /// 使用已读回的配置替代当前草稿；该动作只改 UI 状态，不再次写入文件。
    /// @author kongweiguang
    pub(super) fn use_local_host_readback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.local_hosts.save_outcome_unknown || self.local_hosts.operation_in_progress() {
            return;
        }
        let Some(snapshot) = self.local_hosts.unknown_readback.clone() else {
            self.status = Some(
                crate::locale::text(
                    "尚无可应用的本地主机读回结果。",
                    "There is no local host readback result to apply yet.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        match self.local_hosts.unknown_mutation.clone() {
            Some(LocalHostUnknownMutation::Save { profile_id })
                if snapshot.profile.id == profile_id =>
            {
                cx.emit(saved_selection_event(SavedSelectionTarget::LocalProfile(
                    snapshot.profile.id.clone(),
                )));
                self.local_hosts.editor = Some(self.make_local_host_editor(
                    profile_to_draft(&snapshot.profile),
                    Some(snapshot.revision.clone()),
                    LocalHostEditorMode::Edit,
                    window,
                    cx,
                ));
                self.local_hosts.selected_id = Some(snapshot.profile.id.clone());
                let mut visible_snapshot = snapshot.clone();
                visible_snapshot.profile.env.clear();
                self.local_hosts.selected = Some(visible_snapshot);
                self.clear_local_host_unknown();
                self.status = Some(crate::locale::text(
                    "已采用当前磁盘版本；未重放上次保存。",
                    "The current stored version is now loaded; the previous save was not replayed.",
                ).to_string());
            }
            Some(LocalHostUnknownMutation::Delete { profile_id, .. })
                if snapshot.profile.id == profile_id =>
            {
                self.local_hosts.selected_id = Some(profile_id.clone());
                self.local_hosts.selected = Some(snapshot.clone());
                self.local_hosts.pending_delete = None;
                self.clear_local_host_unknown();
                self.status = Some(crate::locale::text(
                    "本地主机仍存在；如需删除，请重新发起并确认。",
                    "The local host still exists. Start and confirm a new delete request if needed.",
                ).to_string());
            }
            _ => {
                self.status = Some(
                    crate::locale::text(
                        "读回目标与待核对操作不匹配，仍保持写入封锁。",
                        "The readback does not match the pending operation; writes remain blocked.",
                    )
                    .to_string(),
                );
            }
        }
        cx.notify();
    }

    /// 用户在读回后显式选择保留草稿；把下一次 CAS 绑定到读回 revision，而不是重放旧请求。
    pub(super) fn continue_local_host_with_draft(&mut self, cx: &mut Context<Self>) {
        if !self.local_hosts.save_outcome_unknown || self.local_hosts.operation_in_progress() {
            return;
        }
        let Some(operation) = self.local_hosts.unknown_mutation.clone() else {
            return;
        };
        match operation {
            LocalHostUnknownMutation::Save { profile_id } => {
                let latest = self
                    .local_hosts
                    .unknown_readback
                    .as_ref()
                    .filter(|row| row.profile.id == profile_id)
                    .cloned();
                let missing = self.local_hosts.unknown_target_missing;
                let Some(editor) = self.local_hosts.editor.as_mut() else {
                    return;
                };
                if let Some(latest) = latest {
                    editor.revision = Some(latest.revision);
                } else if missing {
                    editor.revision = None;
                } else {
                    self.status = Some(crate::locale::text(
                        "请先读回本地主机，确认当前版本后再继续。",
                        "Read the local host back and confirm its current version before continuing.",
                    ).to_string());
                    cx.notify();
                    return;
                }
                self.clear_local_host_unknown();
                self.status = Some(crate::locale::text(
                    "读回已完成；草稿保留。再次点击保存会创建新的 CAS 写入。",
                    "Readback is complete and the draft is preserved. Saving again will start a new CAS write.",
                ).to_string());
            }
            LocalHostUnknownMutation::Delete { .. } => {
                self.status = Some(crate::locale::text(
                    "删除操作不能从旧确认自动重试；请先采用读回版本，再重新发起删除。",
                    "A delete confirmation is never retried. Apply the readback, then start a new delete request.",
                ).to_string());
                cx.notify();
                return;
            }
        }
        cx.notify();
    }

    /// 创建、编辑或复制前尊重共享 editor gate；脏输入由根确认弹层处理后再继续动作。
    pub(crate) fn begin_create_local_host(
        &mut self,
        group_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_local_host_action(LocalHostPendingAction::Create(group_id), window, cx);
    }

    /// 按稳定 Profile ID 重新读取最新 revision，再创建受 CAS 保护的编辑器。
    pub(crate) fn begin_edit_local_host(
        &mut self,
        profile_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_local_host_action(LocalHostPendingAction::Edit(profile_id), window, cx);
    }

    /// 复制只继承可见配置字段，生成新 ID 且不继承默认标记或原 revision。
    pub(crate) fn begin_duplicate_local_host(
        &mut self,
        profile_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_local_host_action(LocalHostPendingAction::Duplicate(profile_id), window, cx);
    }

    /// 选择状态只绑定真实 Profile ID；远程保存待核对时保持统一编辑器可见，避免隐藏恢复入口。
    /// @author kongweiguang
    pub(crate) fn select_local_host(
        &mut self,
        profile_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_host_editor_replacement(cx) {
            return;
        }
        if self.local_hosts.mutation_blocked() {
            self.status = Some(crate::locale::text(
                "本地主机操作正在保存或等待核对，暂不能切换。",
                "A local host operation is saving or awaiting reconciliation; switching is unavailable.",
            ).to_string());
            cx.notify();
            return;
        }
        if self.local_hosts.editor.is_some() && self.local_host_is_dirty(cx) {
            self.local_hosts.pending_action = Some(LocalHostPendingAction::Select(profile_id));
            self.begin_editor_discard_confirmation(window, cx);
            self.status = Some(
                crate::locale::text(
                    "本地主机尚未保存；确认放弃后再切换。",
                    "The local host has unsaved changes. Confirm discard before switching.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        self.cancel_local_host_background_requests();
        self.local_hosts.editor = None;
        self.active_editor_kind = None;
        self.load_local_host_selection(profile_id, cx);
    }

    /// 从导航详情连接时仅发出 Profile ID，由 Root 经 ProfileService 解析原生 LocalSessionTarget。
    pub(super) fn request_connect_local_host(
        &mut self,
        profile_id: String,
        cx: &mut Context<Self>,
    ) {
        if self.local_hosts.save_outcome_unknown {
            self.status = Some(
                crate::locale::text(
                    "本地主机写入结果尚未核对，暂不能从该条目启动。",
                    "The local host write is not reconciled yet, so it cannot be launched now.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if !self
            .local_hosts
            .rows
            .iter()
            .any(|row| row.profile.id == profile_id)
        {
            self.status = Some(
                crate::locale::text(
                    "本地主机已不在当前列表，请刷新后重试。",
                    "The local host is no longer in the current list. Refresh and try again.",
                )
                .to_string(),
            );
            self.reload_local_hosts(cx);
            return;
        }
        cx.emit(ConnectionsEvent::ConnectLocalProfileRequested { profile_id });
    }

    /// 选择属性/终端分区并在下一帧聚焦同一编辑器的错误输入，避免关闭后的迟到焦点回调。
    /// @author kongweiguang
    fn set_local_host_validation(
        &mut self,
        message: String,
        field: Option<LocalHostValidationField>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.local_hosts.editor.as_mut() else {
            self.status = Some(message);
            cx.notify();
            return;
        };
        editor.validation_issue = Some(message.clone());
        editor.validation_field = field;
        if let Some(field) = field {
            editor.section = field.section();
        }
        let focus = field.map(|field| local_host_focus_handle(editor, field, cx));
        let editor_name_id = editor.name.entity_id();
        let generation = self.local_hosts.editor_generation;
        self.status = Some(message.clone());
        self.local_hosts.status = self.status.clone();
        if let Some(focus) = focus {
            let panel_entity = cx.entity().clone();
            window.on_next_frame(move |window, app| {
                panel_entity.update(app, |panel, cx| {
                    if panel.pending_editor_discard || panel.presentation.shows_group_editor() {
                        return;
                    }
                    let Some(editor) = panel.local_hosts.editor.as_ref() else {
                        return;
                    };
                    if panel.local_hosts.editor_generation != generation
                        || editor.name.entity_id() != editor_name_id
                        || editor.validation_issue.as_deref() != Some(message.as_str())
                        || editor.validation_field != field
                    {
                        return;
                    }
                    window.focus(&focus, cx);
                });
            });
        }
        cx.notify();
    }

    /// 保存前检查恢复模式和组 revision；主机与加密环境作为同一可恢复操作提交。
    /// @author kongweiguang
    pub(super) fn save_local_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.refuse_conflict_reload_mutation(cx) {
            return;
        }
        if self.saving || self.save_outcome_unknown {
            self.status = Some(crate::locale::text(
                "远程主机保存仍在处理或结果待核对，暂不能提交本地主机。",
                "A remote host save is active or unresolved, so a local host cannot be saved yet.",
            ).to_string());
            cx.notify();
            return;
        }
        if self.local_hosts.mutation_blocked() {
            self.status = Some(
                crate::locale::text(
                    "本地主机操作尚未完成或结果未知；请等待或先读回核对。",
                    "A local host operation is active or unresolved. Wait or read it back first.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            self.status = Some(
                crate::locale::text(
                    "配置处于只读恢复模式，暂不能保存本地主机。",
                    "Configuration is read-only during recovery; local hosts cannot be saved.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let Some(editor) = self.local_hosts.editor.as_ref() else {
            return;
        };
        if editor.group_id.as_deref().is_some_and(|id| {
            self.group_revision.is_none() || !self.groups.iter().any(|group| group.id == id)
        }) {
            self.set_local_host_validation(
                crate::locale::text(
                    "所选分组无法核实；请刷新分组或选择未分组。",
                    "The selected group could not be verified. Refresh groups or choose Ungrouped.",
                )
                .to_string(),
                Some(LocalHostValidationField::Group),
                window,
                cx,
            );
            return;
        }
        let (draft, revision, profile_id) =
            match self.local_hosts.editor.as_ref().unwrap().draft(cx) {
                Ok(draft) => {
                    let editor = self.local_hosts.editor.as_ref().unwrap();
                    (draft, editor.revision.clone(), editor.profile_id.clone())
                }
                Err(message) => {
                    self.set_local_host_validation(
                        message,
                        Some(LocalHostValidationField::Environment),
                        window,
                        cx,
                    );
                    return;
                }
            };
        if self
            .local_hosts
            .editor
            .as_ref()
            .is_some_and(|editor| !editor.needs_save(cx))
        {
            self.status = Some(
                crate::locale::text("没有待保存的修改。", "There are no changes to save.")
                    .to_string(),
            );
            cx.notify();
            return;
        }
        let draft = match local_host_draft(draft) {
            Ok(draft) => draft,
            Err(error) => {
                self.status = Some(super::panel::localized_error(&error));
                cx.notify();
                return;
            }
        };
        let service = self.hosts.clone();
        let executor = self.executor.clone();
        let id = profile_id.clone();
        self.local_hosts.saving = true;
        self.local_hosts.status =
            Some(crate::locale::text("正在保存本地主机…", "Saving local host…").to_string());
        self.status = self.local_hosts.status.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move {
                    service
                        .save_local_host_recoverable(draft, revision, cancel)
                        .await
                })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.finish_local_host_save(id, result, window, cx)
            });
        })
        .detach();
    }

    /// 保存仅在真实回执后建立新基线；冲突保留草稿，未知结果禁止盲目重放。
    /// 保存成功才把服务器快照设为新基线；冲突和未知结果必须继续保留用户草稿。
    /// @author kongweiguang
    fn finish_local_host_save(
        &mut self,
        profile_id: String,
        result: Result<RecoverableLocalHostSaveOutcome, AppError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.local_hosts.saving = false;
        let (result, unknown_receipt) = match result {
            Ok(RecoverableLocalHostSaveOutcome::Saved {
                host,
                credential_cleanup,
            }) => (Ok((host, credential_cleanup)), false),
            Ok(RecoverableLocalHostSaveOutcome::OutcomeUnknown { error, recovery }) => {
                self.local_hosts.unknown_save_recovery = Some(recovery);
                (Err(error), true)
            }
            Err(error) => (Err(error), false),
        };
        match result {
            Ok((host, credential_cleanup)) => {
                let saved = local_host_projection(host, BTreeMap::new());
                let saved_profile_id = saved.profile.id.clone();
                self.local_hosts.selected_id = Some(saved_profile_id.clone());
                self.local_hosts.selected = Some(saved.clone());
                upsert_profile(&mut self.local_hosts.rows, saved);
                cx.emit(saved_selection_event(SavedSelectionTarget::LocalProfile(
                    saved_profile_id,
                )));
                self.local_hosts.pending_delete = None;
                self.close_unified_editor(window, cx);
                self.local_hosts.cleanup_pending =
                    matches!(credential_cleanup, CredentialCleanupStatus::Deferred { .. });
                let success_status = match credential_cleanup {
                    CredentialCleanupStatus::Complete => {
                        crate::locale::text("本地主机已保存。", "Local host saved.").to_string()
                    }
                    CredentialCleanupStatus::Deferred { remaining } => crate::locale::text(
                        format!("本地主机已保存；{remaining} 个旧凭据待清理。"),
                        format!("Local host saved; {remaining} old credentials await cleanup."),
                    )
                    .to_string(),
                };
                self.local_hosts.status = Some(success_status.clone());
                self.status = Some(success_status);
                self.reload_local_hosts(cx);
            }
            Err(error) => {
                if let Some(editor) = self.local_hosts.editor.as_mut() {
                    editor.external_conflict = error.public_code() == PublicErrorCode::Conflict;
                    editor.validation_issue = None;
                    editor.validation_field = None;
                }
                if unknown_receipt || error.public_code() == PublicErrorCode::OutcomeUnknown {
                    self.local_hosts.save_outcome_unknown = true;
                    self.local_hosts.unknown_mutation =
                        Some(LocalHostUnknownMutation::Save { profile_id });
                    self.local_hosts.unknown_readback = None;
                    self.local_hosts.unknown_target_missing = false;
                    self.local_hosts.status = Some(crate::locale::text(
                        "保存结果未知；草稿已保留。请先读回核对，不要直接重试。",
                        "The save result is unknown and the draft is preserved. Read it back before any retry.",
                    ).to_string());
                } else {
                    let message = super::panel::localized_error(&error);
                    let arguments_text = self
                        .local_hosts
                        .editor
                        .as_ref()
                        .map(|editor| editor.args.read(cx).value().to_string())
                        .unwrap_or_default();
                    let field = if error.public_code() == PublicErrorCode::Invalid {
                        let working_directory = self
                            .local_hosts
                            .editor
                            .as_ref()
                            .map(|editor| editor.cwd.read(cx).value().to_string())
                            .unwrap_or_default();
                        local_host_validation_field(
                            error.public_message(),
                            &arguments_text,
                            &working_directory,
                        )
                    } else {
                        None
                    };
                    if let Some(field) = field {
                        self.set_local_host_validation(message, Some(field), window, cx);
                    } else {
                        self.local_hosts.status = Some(message);
                    }
                    if error.public_code() == PublicErrorCode::Conflict {
                        self.reload_local_hosts(cx);
                    }
                }
                if self.status.as_ref() != self.local_hosts.status.as_ref() {
                    self.status = self.local_hosts.status.clone();
                }
            }
        }
        cx.notify();
    }

    /// 根确认后的唯一丢弃入口；若原动作被 dirty gate 暂停，则安全地继续该动作。
    /// @author kongweiguang
    pub(super) fn discard_local_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.local_hosts.mutation_blocked() {
            return;
        }
        self.cancel_local_host_background_requests();
        self.local_hosts.editor = None;
        self.pending_editor_discard = false;
        self.active_editor_kind = None;
        let action = self.local_hosts.pending_action.take();
        self.local_hosts.status = None;
        self.status = None;
        cx.notify();
        if let Some(action) = action {
            self.execute_local_host_action(action, window, cx);
        }
    }

    /// “继续编辑”只撤销待执行意图，保留输入与当前编辑器，避免以后误触发旧动作。
    pub(super) fn keep_local_host_editor(&mut self) {
        self.local_hosts.pending_action = None;
    }

    /// 判断当前确认是否在等待本地列表导航；该动作确认后仅丢弃本地表单并继续导航。
    /// @author kongweiguang
    pub(super) fn has_pending_local_host_action(&self) -> bool {
        self.local_hosts.pending_action.is_some()
    }

    /// 统一表单关闭时取消本地读取与默认发现，并丢弃本地字段实体及其潜在环境秘密。
    /// 先推进代次再清空字段，迟到订阅与异步回执不能重新打开或覆盖已关闭的表单。
    /// @author kongweiguang
    pub(super) fn clear_local_host_editor_for_unified_close(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_local_host_background_requests();
        self.local_hosts.cancel_editor_load();
        self.local_hosts.editor_generation = self.local_hosts.editor_generation.wrapping_add(1);
        if let Some(editor) = self.local_hosts.editor.as_ref() {
            editor.env.update(cx, |state, cx| {
                state.set_value(String::new(), window, cx);
            });
        }
        self.local_hosts.editor = None;
        self.local_hosts.pending_action = None;
        self.local_hosts.status = None;
    }

    /// 检查 editor 仅用于导航关闭确认，读取原始输入而不触发校验或持久化。
    pub(crate) fn local_host_is_dirty(&self, cx: &gpui_kit::App) -> bool {
        self.local_hosts
            .editor
            .as_ref()
            .is_some_and(|editor| editor.is_dirty(cx))
    }

    /// 先取最新 versioned row 再打开删除确认，确认目标显示名与 revision 必须来自同一快照。
    /// 删除核对独占自身代次，避免目录刷新和此前列表读取互相作废而遗留忙碌状态。
    /// @author kongweiguang
    pub(crate) fn request_delete_local_host(
        &mut self,
        profile_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.local_hosts.mutation_blocked()
            || self.recovery.mode == ConfigurationMode::ReadOnlyRecovery
        {
            self.status = Some(
                crate::locale::text(
                    "当前配置状态不允许删除本地主机。",
                    "The current configuration state does not allow deleting this local host.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if self.local_host_is_dirty(cx) {
            self.local_hosts.pending_action = Some(LocalHostPendingAction::Delete(profile_id));
            self.begin_editor_discard_confirmation(window, cx);
            self.status = Some(
                crate::locale::text(
                    "请先处理本地编辑草稿，再确认删除。",
                    "Resolve the local editor draft before confirming deletion.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let service = self.hosts.clone();
        let executor = self.executor.clone();
        let generation = self.local_hosts.delete_check_generation.wrapping_add(1);
        self.local_hosts.delete_check_generation = generation;
        self.local_hosts.delete_checking = true;
        self.local_hosts.pending_delete = None;
        self.status = Some(
            crate::locale::text("正在核对删除目标…", "Checking the delete target…").to_string(),
        );
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move {
                    let id = local_host_id(&profile_id)?;
                    Ok(service
                        .load_local_host(&id)
                        .await?
                        .map(|host| local_host_projection(host, BTreeMap::new())))
                })
                .await;
            let _ = this.update_in(cx, |panel, _window, cx| {
                if !panel.local_hosts.delete_checking
                    || panel.local_hosts.delete_check_generation != generation
                {
                    return;
                }
                panel.local_hosts.delete_checking = false;
                match result {
                    Ok(Some(snapshot)) => {
                        panel.local_hosts.pending_delete = Some(snapshot);
                        panel.status = None;
                    }
                    Ok(None) => {
                        panel.status = Some(
                            crate::locale::text(
                                "本地主机已不存在，列表正在刷新。",
                                "The local host no longer exists. Refreshing the list.",
                            )
                            .to_string(),
                        );
                        panel.reload_local_hosts(cx);
                    }
                    Err(error) => panel.status = Some(super::panel::localized_error(&error)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 取消只撤销只读意图；凭据待清理时仍可关闭确认以显示恢复入口，写入未知则保留上下文。
    /// @author kongweiguang
    pub(crate) fn cancel_delete_local_host(&mut self, cx: &mut Context<Self>) {
        if self.local_hosts.delete_checking {
            self.local_hosts.delete_check_generation =
                self.local_hosts.delete_check_generation.wrapping_add(1);
            self.local_hosts.delete_checking = false;
            self.local_hosts.pending_delete = None;
            self.status = None;
            cx.notify();
            return;
        }
        if !self.local_hosts.saving && !self.local_hosts.save_outcome_unknown {
            self.local_hosts.pending_delete = None;
            cx.notify();
        }
    }

    /// 根确认后才按确认快照中的精确 ID/revision 删除；列表读取期间与按钮状态一致地拒绝提交。
    /// @author kongweiguang
    pub(crate) fn confirm_delete_local_host(&mut self, cx: &mut Context<Self>) {
        if self.local_hosts.mutation_blocked()
            || self.local_hosts.loading
            || self.recovery.mode == ConfigurationMode::ReadOnlyRecovery
        {
            return;
        }
        let Some(pending) = self.local_hosts.pending_delete.clone() else {
            return;
        };
        let profile_id = pending.profile.id.clone();
        let revision = pending.revision.clone();
        let service = self.hosts.clone();
        let executor = self.executor.clone();
        self.local_hosts.saving = true;
        self.status =
            Some(crate::locale::text("正在删除本地主机…", "Deleting local host…").to_string());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let expected_revision = revision.clone();
            let id = profile_id.clone();
            let result = executor
                .run(move |_| async move {
                    let id = local_host_id(&id)?;
                    service.delete_local_host(&id, &expected_revision).await
                })
                .await;
            let _ = this.update_in(cx, |panel, _window, cx| {
                panel.finish_local_host_delete(profile_id, revision, result, cx)
            });
        })
        .detach();
    }

    /// 删除回执区分成功、CAS 冲突和未知结果；只有成功读回才解除未知删除门禁。
    fn finish_local_host_delete(
        &mut self,
        profile_id: String,
        revision: ConfigRevision,
        result: Result<HostDeletionReceipt, AppError>,
        cx: &mut Context<Self>,
    ) {
        self.local_hosts.saving = false;
        match result {
            Ok(receipt) => {
                self.local_hosts.pending_delete = None;
                self.local_hosts
                    .rows
                    .retain(|row| row.profile.id != profile_id);
                if self.local_hosts.selected_id.as_deref() == Some(profile_id.as_str()) {
                    self.local_hosts.selected_id = None;
                    self.local_hosts.selected = None;
                }
                self.local_hosts.cleanup_pending = matches!(
                    receipt.credential_cleanup,
                    CredentialCleanupStatus::Deferred { .. }
                );
                self.status = Some(match receipt.credential_cleanup {
                    CredentialCleanupStatus::Complete => {
                        crate::locale::text("本地主机已删除。", "Local host deleted.").to_string()
                    }
                    CredentialCleanupStatus::Deferred { remaining } => crate::locale::text(
                        format!("本地主机已删除；{remaining} 个旧凭据待清理。"),
                        format!("Local host deleted; {remaining} old credentials await cleanup."),
                    )
                    .to_string(),
                });
                self.reload_local_hosts(cx);
            }
            Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                self.local_hosts.save_outcome_unknown = true;
                self.local_hosts.unknown_mutation = Some(LocalHostUnknownMutation::Delete {
                    profile_id,
                    revision,
                });
                self.local_hosts.unknown_readback = None;
                self.local_hosts.unknown_target_missing = false;
                self.status = Some(
                    crate::locale::text(
                        "删除结果未知；请读回列表核对，不要重复确认。",
                        "The delete result is unknown. Read the list back; do not confirm again.",
                    )
                    .to_string(),
                );
            }
            Err(error) if error.public_code() == PublicErrorCode::NotFound => {
                self.local_hosts.pending_delete = None;
                self.status = Some(
                    crate::locale::text(
                        "本地主机已不存在，列表正在刷新。",
                        "The local host no longer exists. Refreshing the list.",
                    )
                    .to_string(),
                );
                self.reload_local_hosts(cx);
            }
            Err(error) => {
                self.local_hosts.pending_delete = None;
                self.status = Some(super::panel::localized_error(&error));
                if error.public_code() == PublicErrorCode::Conflict {
                    self.reload_local_hosts(cx);
                }
            }
        }
        self.local_hosts.status = self.status.clone();
        cx.notify();
    }

    /// 分别读取主机快照和模板候选；主机列表不读取环境秘密，世代号阻止旧结果覆盖。
    fn load_local_profiles(&mut self, explicit_readback: bool, cx: &mut Context<Self>) {
        let Some(service) = self.local_hosts.service.clone() else {
            return;
        };
        let Some(generation) = self.local_hosts.begin_list_load(explicit_readback) else {
            return;
        };
        let executor = self.executor.clone();
        let hosts = self.hosts.clone();
        let operation = explicit_readback
            .then(|| self.local_hosts.unknown_mutation.clone())
            .flatten();
        let startup_recovery = !self.local_hosts.startup_recovery_checked && !explicit_readback;
        let cleanup_pending = explicit_readback && self.local_hosts.cleanup_pending;
        let recovery = explicit_readback
            .then(|| self.local_hosts.unknown_save_recovery.clone())
            .flatten();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move {
                    let startup_recovery = if startup_recovery {
                        Some(hosts.recover_pending_local_host_save().await)
                    } else {
                        None
                    };
                    let save_readback = match operation {
                        Some(LocalHostUnknownMutation::Save { profile_id }) => {
                            let id = local_host_id(&profile_id)?;
                            let current = hosts.load_local_host(&id).await?;
                            let current = match current {
                                Some(host) => {
                                    let env = hosts.read_local_environment(&host, &cancel).await?;
                                    Some(local_host_projection(host, env))
                                }
                                None => None,
                            };
                            let cleanup = match recovery {
                                Some(recovery) => {
                                    hosts.recover_unknown_local_host_save(&recovery).await?
                                }
                                None => hosts.recover_pending_local_host_save().await?,
                            };
                            if matches!(cleanup, CredentialCleanupStatus::Deferred { .. }) {
                                return Err(AppError::infrastructure(
                                    "本地主机恢复凭据清理尚未完成，请重新核对",
                                ));
                            }
                            Some(current)
                        }
                        Some(LocalHostUnknownMutation::Delete { profile_id, .. }) => {
                            let id = local_host_id(&profile_id)?;
                            let _ = hosts.load_local_host(&id).await?;
                            if matches!(
                                hosts.recover_pending_local_host_save().await?,
                                CredentialCleanupStatus::Deferred { .. }
                            ) {
                                return Err(AppError::infrastructure(
                                    "本地主机恢复凭据清理尚未完成，请重新核对",
                                ));
                            }
                            None
                        }
                        None if cleanup_pending => {
                            if matches!(
                                hosts.recover_pending_local_host_save().await?,
                                CredentialCleanupStatus::Deferred { .. }
                            ) {
                                return Err(AppError::infrastructure(
                                    "本地主机恢复凭据清理尚未完成，请重新核对",
                                ));
                            }
                            None
                        }
                        None => None,
                    };
                    let rows = hosts
                        .list_local_hosts()
                        .await?
                        .into_iter()
                        .map(|host| local_host_projection(host, BTreeMap::new()))
                        .collect();
                    let templates = service.list_profiles().await?;
                    Ok(LocalHostCatalogLoad {
                        rows,
                        templates,
                        save_readback,
                        startup_recovery,
                    })
                })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                if panel.local_hosts.generation != generation {
                    return;
                }
                panel.finish_local_profiles_load(result, explicit_readback, window, cx);
            });
        })
        .detach();
    }

    /// 原子替换两个目录投影，脏编辑保留原值，未知写入仍需显式核对。
    fn finish_local_profiles_load(
        &mut self,
        result: Result<LocalHostCatalogLoad, AppError>,
        explicit_readback: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.local_hosts.loading = false;
        self.local_hosts.readback_in_progress = false;
        match result {
            Ok(LocalHostCatalogLoad {
                rows,
                templates,
                save_readback,
                startup_recovery,
            }) => {
                self.local_hosts.rows = rows;
                self.local_hosts.templates = templates;
                if let Some(recovery) = startup_recovery {
                    self.local_hosts.startup_recovery_checked = true;
                    match recovery {
                        Ok(CredentialCleanupStatus::Complete) => {
                            self.local_hosts.cleanup_pending = false;
                        }
                        Ok(CredentialCleanupStatus::Deferred { .. }) => {
                            self.local_hosts.cleanup_pending = true;
                            self.status = Some(
                                crate::locale::text(
                                    "本地主机旧凭据尚待清理，请点击核对后重试。",
                                    "Local host credentials still need cleanup. Review and retry.",
                                )
                                .to_string(),
                            );
                        }
                        Err(error) => {
                            self.local_hosts.cleanup_pending = true;
                            self.status = Some(super::panel::localized_error(&error));
                        }
                    }
                }
                self.local_hosts.selected = self
                    .local_hosts
                    .selected_id
                    .as_deref()
                    .and_then(|id| {
                        self.local_hosts
                            .rows
                            .iter()
                            .find(|row| row.profile.id == id)
                    })
                    .cloned();
                if let Some(editor) = self.local_hosts.editor.as_mut()
                    && let Some(current) = self
                        .local_hosts
                        .rows
                        .iter()
                        .find(|row| row.profile.id == editor.profile_id)
                {
                    editor.external_conflict = Some(&current.revision) != editor.revision.as_ref();
                }
                if explicit_readback {
                    if save_readback.is_some() {
                        self.local_hosts.unknown_save_recovery = None;
                    }
                    if self.local_hosts.cleanup_pending && !self.local_hosts.save_outcome_unknown {
                        self.local_hosts.cleanup_pending = false;
                        self.status = Some(
                            crate::locale::text(
                                "本地主机凭据清理已完成。",
                                "Local host credential cleanup is complete.",
                            )
                            .to_string(),
                        );
                    } else {
                        self.reconcile_local_host_readback(save_readback, cx);
                    }
                } else if self
                    .local_hosts
                    .editor
                    .as_ref()
                    .is_some_and(|editor| editor.external_conflict)
                {
                    self.status = Some(crate::locale::text(
                        "本地主机已被外部修改；草稿已保留，保存前请先读取并核对。",
                        "The local host changed externally. The draft is preserved; read and reconcile before saving.",
                    ).to_string());
                }
            }
            Err(error) => {
                self.status = Some(super::panel::localized_error(&error));
                if explicit_readback {
                    if !self.local_hosts.cleanup_pending {
                        self.local_hosts.save_outcome_unknown = true;
                    }
                } else if !self.local_hosts.startup_recovery_checked {
                    self.local_hosts.cleanup_pending = true;
                }
            }
        }
        self.local_hosts.status = self.status.clone();
        cx.notify();
    }

    /// 显式读回后按稳定 ID 和原 revision 判断保存/删除当前事实，不把读回伪装成成功回执。
    fn reconcile_local_host_readback(
        &mut self,
        save_readback: Option<Option<VersionedProfile>>,
        cx: &mut Context<Self>,
    ) {
        let Some(operation) = self.local_hosts.unknown_mutation.clone() else {
            return;
        };
        match operation {
            LocalHostUnknownMutation::Save { profile_id } => {
                let Some(current) = save_readback else {
                    self.status = Some(
                        crate::locale::text(
                            "恢复读回未完成，请重新核对。",
                            "Recovery readback is incomplete; review it again.",
                        )
                        .to_string(),
                    );
                    cx.notify();
                    return;
                };
                if current
                    .as_ref()
                    .is_some_and(|row| row.profile.id != profile_id)
                {
                    self.status = Some(
                        crate::locale::text(
                            "恢复读回目标不匹配，写入保持暂停。",
                            "The recovery target does not match. Writes remain paused.",
                        )
                        .to_string(),
                    );
                    cx.notify();
                    return;
                }
                self.local_hosts.unknown_readback = current.clone();
                self.local_hosts.unknown_target_missing = current.is_none();
                self.status = Some(match current {
                    Some(_) => crate::locale::text(
                        "已读回目标本地主机；请选择采用当前版本，或明确保留草稿后重新比较保存。",
                        "The local host was read back. Choose the current version or explicitly keep the draft for a fresh compare-and-save.",
                    ).to_string(),
                    None => crate::locale::text(
                        "读回列表中未找到目标 ID；草稿仍保留，需明确选择后续操作。",
                        "The target ID was not found in the readback list. The draft is preserved; choose the next action explicitly.",
                    ).to_string(),
                });
            }
            LocalHostUnknownMutation::Delete {
                profile_id,
                revision,
            } => {
                let current = self
                    .local_hosts
                    .rows
                    .iter()
                    .find(|row| row.profile.id == profile_id)
                    .cloned();
                match current {
                    None => {
                        self.local_hosts.pending_delete = None;
                        self.local_hosts.unknown_readback = None;
                        self.local_hosts.unknown_target_missing = true;
                        self.clear_local_host_unknown();
                        if self.local_hosts.selected_id.as_deref() == Some(profile_id.as_str()) {
                            self.local_hosts.selected_id = None;
                            self.local_hosts.selected = None;
                        }
                        self.status = Some(crate::locale::text(
                            "读回确认本地主机已不存在；删除结果已核实。",
                            "Readback confirms the local host is absent; the delete result is reconciled.",
                        ).to_string());
                    }
                    Some(snapshot) if snapshot.revision == revision => {
                        self.local_hosts.pending_delete = None;
                        self.local_hosts.unknown_readback = Some(snapshot);
                        self.local_hosts.unknown_target_missing = false;
                        self.clear_local_host_unknown();
                        self.status = Some(crate::locale::text(
                            "读回确认本地主机仍存在；如需删除，请重新发起并确认。",
                            "Readback confirms the local host still exists. Start and confirm a new delete request if needed.",
                        ).to_string());
                    }
                    Some(snapshot) => {
                        self.local_hosts.pending_delete = None;
                        self.local_hosts.selected_id = Some(profile_id);
                        self.local_hosts.selected = Some(snapshot.clone());
                        self.local_hosts.unknown_readback = Some(snapshot);
                        self.local_hosts.unknown_target_missing = false;
                        self.clear_local_host_unknown();
                        self.status = Some(crate::locale::text(
                            "读回发现本地主机版本已变化；旧删除未重放，请重新核对目标。",
                            "Readback found a changed local host version. The old delete was not replayed; review the target again.",
                        ).to_string());
                    }
                }
            }
        }
        self.local_hosts.status = self.status.clone();
        cx.notify();
    }

    /// 清空未知 operation marker 只由显式读回后的选择或已确认的实际状态调用。
    fn clear_local_host_unknown(&mut self) {
        self.local_hosts.save_outcome_unknown = false;
        self.local_hosts.unknown_mutation = None;
        self.local_hosts.unknown_readback = None;
        self.local_hosts.unknown_target_missing = false;
        self.local_hosts.unknown_save_recovery = None;
    }

    /// 统一 dirty gate；确认后继续原始操作，取消则保留草稿和当前 Profile ID；远程恢复期间不切换表单。
    /// @author kongweiguang
    fn dispatch_local_host_action(
        &mut self,
        action: LocalHostPendingAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_host_editor_replacement(cx) {
            return;
        }
        if self.local_hosts.mutation_blocked() {
            self.status = Some(crate::locale::text(
                "本地主机正在保存或等待核对，当前操作暂不可用。",
                "The local host is saving or awaiting reconciliation; this action is unavailable.",
            ).to_string());
            cx.notify();
            return;
        }
        if self.local_host_is_dirty(cx) {
            self.local_hosts.pending_action = Some(action);
            self.begin_editor_discard_confirmation(window, cx);
            self.status = Some(crate::locale::text(
                "本地主机有未保存修改；确认放弃后继续刚才的操作。",
                "The local host has unsaved changes. Confirm discard to continue the requested action.",
            ).to_string());
            cx.notify();
            return;
        }
        self.cancel_local_host_background_requests();
        self.local_hosts.editor = None;
        self.active_editor_kind = None;
        self.execute_local_host_action(action, window, cx);
    }

    /// 根据用户显式意图创建、重读、选择或请求删除，不把导航动作变成隐式写入。
    /// @author kongweiguang
    fn execute_local_host_action(
        &mut self,
        action: LocalHostPendingAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reload_local_hosts(cx);
        match action {
            LocalHostPendingAction::Create(group_id) => {
                if group_id
                    .as_ref()
                    .is_some_and(|id| !self.groups.iter().any(|group| &group.id == id))
                {
                    self.status = Some(crate::locale::text(
                        "目标分组已不存在，请刷新分组后再新建本地主机。",
                        "The target group no longer exists. Refresh groups before creating the local host.",
                    ).to_string());
                    cx.notify();
                    return;
                }
                let sort_order = self
                    .local_hosts
                    .rows
                    .iter()
                    .map(|row| row.profile.sort_order)
                    .max()
                    .map_or(0, |order| order.saturating_add(1));
                let id = uuid::Uuid::new_v4().to_string();
                let draft = ProfileDraft {
                    id: Some(id),
                    name: crate::locale::text("本地终端", "Local terminal").to_string(),
                    shell: String::new(),
                    args: Vec::new(),
                    cwd: None,
                    env: BTreeMap::new(),
                    sidebar_group_id: group_id,
                    sort_order,
                };
                self.local_hosts.editor = Some(self.make_local_host_editor(
                    draft,
                    None,
                    LocalHostEditorMode::Create,
                    window,
                    cx,
                ));
                self.active_editor_kind = Some(ConnectionsEditorKind::Local);
                self.status = None;
                self.start_default_local_target_resolution(cx);
            }
            LocalHostPendingAction::Edit(profile_id) => {
                self.load_local_profile_editor(profile_id, LocalHostEditorMode::Edit, window, cx)
            }
            LocalHostPendingAction::Duplicate(profile_id) => self.load_local_profile_editor(
                profile_id,
                LocalHostEditorMode::Duplicate,
                window,
                cx,
            ),
            LocalHostPendingAction::Select(profile_id) => {
                self.load_local_host_selection(profile_id, cx)
            }
            LocalHostPendingAction::Delete(profile_id) => {
                self.request_delete_local_host(profile_id, window, cx)
            }
        }
        cx.notify();
    }

    /// 读取 ProfileService 的平台/默认 Profile 目标并预填 LocalSessionTarget，不猜 shell 路径。
    /// 默认目标由 ProfileService 唯一解释，异步回执只补填仍符合当前意图的字段。
    /// @author kongweiguang
    fn start_default_local_target_resolution(&mut self, cx: &mut Context<Self>) {
        self.resolve_local_profile_target(None, None, cx);
    }

    /// 按本地主机 ID 重新读取 revision 和加密环境；任何读取失败均保留原编辑器。
    /// 编辑/复制/冲突重读不得用导航列表旧 revision 写回，也不能因读取失败丢失当前草稿。
    /// @author kongweiguang
    pub(super) fn load_local_profile_editor(
        &mut self,
        profile_id: String,
        mode: LocalHostEditorMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let service = self.hosts.clone();
        self.local_hosts.editor_load_generation =
            self.local_hosts.editor_load_generation.wrapping_add(1);
        let editor_load_generation = self.local_hosts.editor_load_generation;
        self.local_hosts.editor_load_pending = true;
        self.local_hosts.editor_load_error = None;
        self.local_hosts.editor_load_intent = Some(LocalHostEditorLoadIntent {
            profile_id: profile_id.clone(),
            mode,
            generation: editor_load_generation,
        });
        self.active_editor_kind = Some(ConnectionsEditorKind::Local);
        let executor = self.executor.clone();
        self.status =
            Some(crate::locale::text("正在读取本地主机…", "Loading local host…").to_string());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let requested_id = profile_id.clone();
            let result = executor
                .run(move |cancel| async move {
                    let id = local_host_id(&requested_id)?;
                    let Some(host) = service.load_local_host(&id).await? else {
                        return Ok(None);
                    };
                    let environment = service.read_local_environment(&host, &cancel).await?;
                    Ok(Some(local_host_projection(host, environment)))
                })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                if !panel
                    .local_hosts
                    .accepts_editor_load(&profile_id, mode, editor_load_generation)
                    || panel.active_editor_kind != Some(ConnectionsEditorKind::Local)
                {
                    return;
                }
                panel.local_hosts.editor_load_pending = false;
                if panel.conflict_reload_in_progress.as_ref()
                    == Some(&super::panel::ConflictReloadTarget::Local(profile_id.clone()))
                {
                    panel.conflict_reload_in_progress = None;
                }
                match result {
                    Ok(Some(snapshot)) => {
                        if snapshot.profile.id != profile_id {
                            let message = crate::locale::text(
                                "读取返回的 Profile ID 与请求不一致；原表单已保留，请重试。",
                                "The read returned a different Profile ID. The original form was preserved; retry the reload.",
                            ).to_string();
                            panel.local_hosts.editor_load_error = Some(message.clone());
                            panel.status = Some(message.clone());
                            panel.local_hosts.status = Some(message);
                            cx.notify();
                            return;
                        }
                        panel.local_hosts.editor_load_intent = None;
                        panel.local_hosts.editor_load_error = None;
                        let mut draft = profile_to_draft(&snapshot.profile);
                        let revision = if mode == LocalHostEditorMode::Edit {
                            Some(snapshot.revision.clone())
                        } else {
                            draft.id = Some(uuid::Uuid::new_v4().to_string());
                            draft.name = crate::locale::text(
                                format!("{} 副本", snapshot.profile.name),
                                format!("{} copy", snapshot.profile.name),
                            )
                            .to_string();
                            draft.sort_order = panel
                                .local_hosts
                                .rows
                                .iter()
                                .map(|row| row.profile.sort_order)
                                .max()
                                .map_or(0, |order| order.saturating_add(1));
                            None
                        };
                        let editor =
                            panel.make_local_host_editor(draft, revision, mode, window, cx);
                        if let Some(previous_editor) = panel.local_hosts.editor.as_ref() {
                            previous_editor.env.update(cx, |state, cx| {
                                state.set_value(String::new(), window, cx);
                            });
                        }
                        panel.invalidate_host_file_picker();
                        panel.local_hosts.editor = Some(editor);
                        panel.active_editor_kind = Some(ConnectionsEditorKind::Local);
                        panel.local_hosts.selected_id = Some(snapshot.profile.id.clone());
                        let mut visible_snapshot = snapshot;
                        visible_snapshot.profile.env.clear();
                        panel.local_hosts.selected = Some(visible_snapshot.clone());
                        upsert_profile(&mut panel.local_hosts.rows, visible_snapshot);
                        panel.status = None;
                    }
                    Ok(None) => {
                        let error = crate::locale::text(
                            "本地主机已不存在，列表正在刷新。",
                            "The local host no longer exists. Refreshing the list.",
                        )
                        .to_string();
                        panel.local_hosts.editor_load_error = Some(error.clone());
                        panel.status = Some(error);
                    }
                    Err(error) => {
                        let message = super::panel::localized_error(&error);
                        panel.local_hosts.editor_load_error = Some(message.clone());
                        panel.status = Some(message);
                    }
                }
                panel.local_hosts.status = panel.status.clone();
                cx.notify();
            });
        })
        .detach();
    }

    /// 按稳定本地主机 ID 读取非秘密详情，不把模板 ID 解释成左栏主机。
    fn load_local_host_selection(&mut self, profile_id: Option<String>, cx: &mut Context<Self>) {
        let Some(profile_id) = profile_id else {
            self.local_hosts.selected_id = None;
            self.local_hosts.selected = None;
            self.local_hosts.pending_delete = None;
            cx.notify();
            return;
        };
        let service = self.hosts.clone();
        self.local_hosts.generation = self.local_hosts.generation.wrapping_add(1);
        let generation = self.local_hosts.generation;
        let executor = self.executor.clone();
        self.local_hosts.loading = true;
        self.local_hosts.selected_id = Some(profile_id.clone());
        self.local_hosts.selected = None;
        self.local_hosts.pending_delete = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move {
                    let id = local_host_id(&profile_id)?;
                    Ok(service
                        .load_local_host(&id)
                        .await?
                        .map(|host| local_host_projection(host, BTreeMap::new())))
                })
                .await;
            let _ = this.update_in(cx, |panel, _window, cx| {
                if panel.local_hosts.generation != generation {
                    return;
                }
                panel.local_hosts.loading = false;
                match result {
                    Ok(Some(snapshot)) => {
                        panel.local_hosts.selected_id = Some(snapshot.profile.id.clone());
                        panel.local_hosts.selected = Some(snapshot.clone());
                        upsert_profile(&mut panel.local_hosts.rows, snapshot);
                        panel.status = None;
                    }
                    Ok(None) => {
                        panel.local_hosts.selected_id = None;
                        panel.local_hosts.selected = None;
                        panel.status = Some(
                            crate::locale::text(
                                "本地主机已不存在，列表正在刷新。",
                                "The local host no longer exists. Refreshing the list.",
                            )
                            .to_string(),
                        );
                        panel.reload_local_hosts(cx);
                    }
                    Err(error) => panel.status = Some(super::panel::localized_error(&error)),
                }
                panel.local_hosts.status = panel.status.clone();
                cx.notify();
            });
        })
        .detach();
    }

    /// 根据 ProfileDraft 创建 Kit 输入实体并绑定 group 事件；输入订阅不执行 IO。
    /// 控件实体拥有未提交值和焦点；每次替换编辑器前撤销它的后台请求。
    /// @author kongweiguang
    fn make_local_host_editor(
        &mut self,
        draft: ProfileDraft,
        revision: Option<ConfigRevision>,
        mode: LocalHostEditorMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> LocalHostEditor {
        self.cancel_local_host_background_requests();
        self.local_hosts.editor_generation = self.local_hosts.editor_generation.wrapping_add(1);
        let generation = self.local_hosts.editor_generation;
        let profile_id = draft
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let draft = ProfileDraft {
            id: Some(profile_id.clone()),
            ..draft
        };
        let baseline = LocalHostInputBaseline {
            name: draft.name.clone(),
            shell: draft.shell.clone(),
            cwd: draft.cwd.clone().unwrap_or_default(),
            args: draft.args.join("\n"),
            env: format_environment(&draft.env),
            group_id: draft.sidebar_group_id.clone(),
        };
        let name = create_input(
            window,
            cx,
            crate::locale::text("本地主机名称", "Local host name"),
            &draft.name,
            false,
        );
        let shell = create_input(
            window,
            cx,
            crate::locale::text("Shell 程序路径", "Shell program path"),
            &draft.shell,
            false,
        );
        let cwd = create_input(
            window,
            cx,
            crate::locale::text(
                "默认工作目录（可选）",
                "Default working directory (optional)",
            ),
            draft.cwd.as_deref().unwrap_or_default(),
            false,
        );
        let args = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(crate::locale::text("每行一个参数", "One argument per line"))
                .auto_grow(2, 5)
        });
        args.update(cx, |state, cx| {
            state.set_value(baseline.args.clone(), window, cx)
        });
        let env = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(crate::locale::text(
                    "每行一个 KEY=VALUE",
                    "One KEY=VALUE per line",
                ))
                .auto_grow(2, 5)
        });
        env.update(cx, |state, cx| {
            state.set_value(baseline.env.clone(), window, cx)
        });
        let group_select = cx.new(|cx| {
            SelectState::new(
                local_host_group_options(&self.groups, draft.sidebar_group_id.as_deref()),
                None,
                window,
                cx,
            )
        });
        group_select.update(cx, |state, cx| {
            state.set_selected_value(&draft.sidebar_group_id, window, cx)
        });
        let group_subscription = cx.subscribe(
            &group_select,
            move |panel, _, event: &SelectEvent<Vec<LocalHostGroupOption>>, cx| {
                if let SelectEvent::Confirm(Some(group_id)) = event
                    && panel.local_hosts.editor_generation == generation
                {
                    if let Some(editor) = panel.local_hosts.editor.as_mut() {
                        editor.group_id = group_id.clone();
                        editor.validation_issue = None;
                        editor.validation_field = None;
                    }
                    panel.refresh_local_host_dirty(cx);
                }
            },
        );
        let input_subscriptions = vec![
            subscribe_local_host_input(&name, generation, LocalHostInputField::Name, cx),
            subscribe_local_host_input(&shell, generation, LocalHostInputField::Shell, cx),
            subscribe_local_host_input(&cwd, generation, LocalHostInputField::Cwd, cx),
            subscribe_local_host_textarea(&args, generation, LocalHostInputField::Args, cx),
            subscribe_local_host_textarea(&env, generation, LocalHostInputField::Env, cx),
        ];
        LocalHostEditor {
            profile_id,
            mode,
            section: LocalHostEditorSection::Properties,
            scroll_handle: ScrollHandle::new(),
            revision,
            name,
            group_select,
            shell,
            cwd,
            args,
            env,
            group_id: draft.sidebar_group_id,
            dirty: false,
            validation_issue: None,
            validation_field: None,
            external_conflict: false,
            baseline,
            sort_order: draft.sort_order,
            _group_subscription: group_subscription,
            _input_subscriptions: input_subscriptions,
        }
    }

    /// 输入事件只刷新 dirty 和字段提示，不从控件订阅中启动配置写入。
    /// @author kongweiguang
    fn refresh_local_host_dirty(&mut self, cx: &mut Context<Self>) {
        let dirty = self
            .local_hosts
            .editor
            .as_ref()
            .is_some_and(|editor| editor.is_dirty(cx));
        if let Some(editor) = self.local_hosts.editor.as_mut() {
            editor.dirty = dirty;
            editor.validation_issue = None;
            editor.validation_field = None;
        }
        cx.notify();
    }
}

#[derive(Clone, Copy)]
enum LocalHostInputField {
    Name,
    Shell,
    Cwd,
    Args,
    Env,
}

/// 单行字段只触发草稿 dirty 计算；保存逻辑始终从当前 Entity 构造完整 typed DTO。
fn subscribe_local_host_input(
    input: &Entity<InputState>,
    generation: u64,
    _field: LocalHostInputField,
    cx: &mut Context<ConnectionsPanel>,
) -> Subscription {
    cx.subscribe(input, move |panel, _, event: &InputEvent, cx| {
        if matches!(event, InputEvent::Change)
            && panel.local_hosts.editor_generation == generation
            && !panel.local_hosts.mutation_blocked()
        {
            panel.refresh_local_host_dirty(cx);
        }
    })
}

/// 多行参数和环境变量保留原始换行，空行解释集中在纯 parser 而非 UI 文本控件。
fn subscribe_local_host_textarea(
    input: &Entity<TextareaState>,
    generation: u64,
    _field: LocalHostInputField,
    cx: &mut Context<ConnectionsPanel>,
) -> Subscription {
    cx.subscribe(input, move |panel, _, event: &InputEvent, cx| {
        if matches!(event, InputEvent::Change)
            && panel.local_hosts.editor_generation == generation
            && !panel.local_hosts.mutation_blocked()
        {
            panel.refresh_local_host_dirty(cx);
        }
    })
}

/// 将已保存 Profile 拆成表单字段；default flag 由 ProfileService/CAS 独立保留，不进入草稿。
fn profile_to_draft(profile: &TerminalProfile) -> ProfileDraft {
    ProfileDraft {
        id: Some(profile.id.clone()),
        name: profile.name.clone(),
        shell: profile.shell.clone(),
        args: profile.args.clone(),
        cwd: profile.cwd.clone(),
        env: profile.env.clone(),
        sidebar_group_id: profile.sidebar_group_id.clone(),
        sort_order: profile.sort_order,
    }
}

/// 按现有 Profile UI 语义逐行保存参数；空编辑区表示零参数，显式空行仍是空参数。
fn parse_profile_arguments(source: &str) -> Vec<String> {
    if source.is_empty() {
        Vec::new()
    } else {
        source.split('\n').map(str::to_owned).collect()
    }
}

/// 环境变量沿用既有表单约定：空行忽略、首个等号分割、重复键与非法键要求用户修正。
fn parse_profile_environment(source: &str) -> Result<BTreeMap<String, String>, String> {
    let mut values = BTreeMap::new();
    for (index, raw_line) in source.lines().enumerate() {
        if raw_line.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = raw_line.split_once('=') else {
            return Err(crate::locale::text(
                format!("第 {} 行需要使用 KEY=VALUE 格式。", index + 1),
                format!("Line {} must use KEY=VALUE format.", index + 1),
            )
            .to_string());
        };
        let key = key.trim();
        if key.is_empty() || key.contains('\0') || value.contains('\0') {
            return Err(crate::locale::text(
                format!("第 {} 行包含无效环境变量名或不可表示字符。", index + 1),
                format!("Line {} contains an invalid environment variable name or unsupported character.", index + 1),
            ).to_string());
        }
        if values.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(crate::locale::text(
                format!("第 {} 行重复定义环境变量“{}”。", index + 1, key),
                format!("Line {} repeats environment variable “{}”.", index + 1, key),
            )
            .to_string());
        }
    }
    Ok(values)
}

/// 空白工作目录只表示继承/不覆盖，不保存用户无意义的空路径。
fn optional_path(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_owned())
}

/// 保持环境变量格式稳定，供 Profile 编辑 baseline 与默认目标预填共用。
fn format_environment(values: &BTreeMap<String, String>) -> String {
    values
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 读取当前输入作为基线，仅在 ProfileService 明确返回有效默认目标后调用。
fn read_editor_baseline(
    editor: &LocalHostEditor,
    cx: &Context<ConnectionsPanel>,
) -> LocalHostInputBaseline {
    LocalHostInputBaseline {
        name: editor.name.read(cx).value().to_string(),
        shell: editor.shell.read(cx).value().to_string(),
        cwd: editor.cwd.read(cx).value().to_string(),
        args: editor.args.read(cx).value().to_string(),
        env: editor.env.read(cx).value().to_string(),
        group_id: editor.group_id.clone(),
    }
}

/// 按 Profile 文件字段次序命名类型，避免默认目标转换接口重复展开复合元组。
type LocalHostProfileFields = (
    String,
    Vec<String>,
    Option<String>,
    BTreeMap<String, String>,
);

/// 将平台目标转换为可文件化的 UTF-8 Profile 字段；不可表达的 OS 字符串不做 lossy 转换。
fn local_target_to_profile_fields(
    target: LocalSessionTarget,
) -> Result<LocalHostProfileFields, String> {
    let shell = target.program.into_string().map_err(|_| crate::locale::text(
        "系统默认 Shell 包含不可表示字符，请手动填写程序路径。",
        "The default shell contains characters that cannot be represented. Enter its path manually.",
    ).to_string())?;
    let args = target
        .args
        .into_iter()
        .map(|arg| {
            arg.into_string().map_err(|_| {
                crate::locale::text(
        "默认终端参数包含不可表示字符，请手动填写。",
        "Default terminal arguments contain unrepresentable characters. Enter them manually.",
    ).to_string()
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let cwd = target
        .working_directory
        .map(|path| {
            path.into_os_string().into_string().map_err(|_| {
                crate::locale::text(
        "默认工作目录包含不可表示字符，请手动填写。",
        "The default working directory contains unrepresentable characters. Enter it manually.",
    ).to_string()
            })
        })
        .transpose()?;
    let mut env = BTreeMap::new();
    for (key, value) in target.environment {
        let key = key.into_string().map_err(|_| {
            crate::locale::text(
                "默认环境变量名包含不可表示字符。",
                "A default environment variable name contains unrepresentable characters.",
            )
            .to_string()
        })?;
        let value = value.into_string().map_err(|_| {
            crate::locale::text(
                "默认环境变量值包含不可表示字符。",
                "A default environment variable value contains unrepresentable characters.",
            )
            .to_string()
        })?;
        env.insert(key, value);
    }
    Ok((shell, args, cwd, env))
}

/// 新 Profile 插入/保存回执更新现有顺序，不以列表展示或旧快照冒充磁盘保存成功。
fn upsert_profile(rows: &mut Vec<VersionedProfile>, row: VersionedProfile) {
    if let Some(existing) = rows
        .iter_mut()
        .find(|existing| existing.profile.id == row.profile.id)
    {
        *existing = row;
    } else {
        rows.push(row);
    }
    rows.sort_by(|left, right| {
        left.profile
            .sort_order
            .cmp(&right.profile.sort_order)
            .then_with(|| {
                left.profile
                    .name
                    .to_lowercase()
                    .cmp(&right.profile.name.to_lowercase())
            })
            .then_with(|| left.profile.id.cmp(&right.profile.id))
    });
}

#[cfg(test)]
#[path = "../../../tests/unit/connections_local_hosts.rs"]
mod tests;
