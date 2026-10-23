//! 持久本地主机复用现有 ProfileService；表单只提交 ProfileDraft，不自建存储或终端引擎。
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
    local_shells::LocalShellCandidate,
    preferences::{ProfileDraft, ProfileRevision, VersionedProfile},
};
use kerminal_domain::{
    preferences::{ConfigRevision, ConfigurationMode, TerminalProfile},
    sessions::LocalSessionTarget,
};
use tokio_util::sync::CancellationToken;

use super::{
    form::create_input,
    panel::{
        ConnectionsEditorKind, ConnectionsEvent, ConnectionsPanel, HostFilePickerTarget,
        SavedSelectionTarget, saved_selection_event,
    },
};

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
    SetDefault {
        profile_id: String,
    },
}

/// 完整列表读回后的默认项事实；stable ID 用来避免以显示名称误认目标。
#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalHostDefaultReadback {
    default_profile_ids: Vec<String>,
    target_present: bool,
    target_is_default: bool,
}

/// 按整个本地主机列表构造默认项 CAS 输入，防止局部快照遗漏需要清除的旧默认标记。
fn local_host_profile_revisions(rows: &[VersionedProfile]) -> Vec<ProfileRevision> {
    rows.iter()
        .map(|row| ProfileRevision {
            profile_id: row.profile.id.clone(),
            revision: row.revision.clone(),
        })
        .collect()
}

/// 只允许从当前完整列表选择目标，并始终按稳定 Profile ID 匹配。
fn local_host_default_target<'a>(
    rows: &'a [VersionedProfile],
    profile_id: &str,
) -> Option<&'a VersionedProfile> {
    rows.iter().find(|row| row.profile.id == profile_id)
}

/// 从显式列表读回提取目标与全部默认 ID，未知结果不会被转换成“原请求成功”。
fn local_host_default_readback(
    rows: &[VersionedProfile],
    target_id: &str,
) -> LocalHostDefaultReadback {
    let default_profile_ids = rows
        .iter()
        .filter(|row| row.profile.is_default)
        .map(|row| row.profile.id.clone())
        .collect();
    let target = local_host_default_target(rows, target_id);
    LocalHostDefaultReadback {
        default_profile_ids,
        target_present: target.is_some(),
        target_is_default: target.is_some_and(|row| row.profile.is_default),
    }
}

/// 唯一默认项的不变量只有在显式读回不再观察到多个默认标记时才允许解除写入封锁。
fn local_host_default_readback_can_release(readback: &LocalHostDefaultReadback) -> bool {
    readback.default_profile_ids.len() <= 1
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

/// 只映射应用层已知 Profile 校验；复合错误按相同约束定位 cwd/argv，剩余落到环境变量。
/// @author kongweiguang
pub(super) fn local_host_validation_field(
    message: &str,
    arguments_text: &str,
    working_directory: &str,
) -> Option<LocalHostValidationField> {
    match message {
        "profile 名称不能为空或过长" => Some(LocalHostValidationField::Name),
        "profile shell 路径不能为空或过长" => Some(LocalHostValidationField::Shell),
        "profile 参数或环境变量超出限制" => {
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

/// 本地主机状态集中持有 ProfileService 回执、稳定 ID 快照与写入未知门禁。
pub(super) struct LocalHostState {
    pub(super) service: Option<ProfileService>,
    pub(super) rows: Vec<VersionedProfile>,
    pub(super) selected_id: Option<String>,
    pub(super) selected: Option<VersionedProfile>,
    pub(super) editor: Option<LocalHostEditor>,
    pub(super) shell_candidates: Vec<LocalShellCandidate>,
    pub(super) shell_loading: bool,
    pub(super) shell_error: Option<String>,
    pub(super) editor_load_intent: Option<LocalHostEditorLoadIntent>,
    pub(super) editor_load_pending: bool,
    pub(super) editor_load_error: Option<String>,
    pub(super) loading: bool,
    pub(super) saving: bool,
    pub(super) save_outcome_unknown: bool,
    pub(super) pending_delete: Option<VersionedProfile>,
    pub(super) status: Option<String>,
    pub(super) delete_checking: bool,
    pub(super) readback_in_progress: bool,
    pub(super) unknown_readback: Option<VersionedProfile>,
    pub(super) unknown_target_missing: bool,
    pub(super) pending_action: Option<LocalHostPendingAction>,
    unknown_mutation: Option<LocalHostUnknownMutation>,
    generation: u64,
    editor_load_generation: u64,
    editor_generation: u64,
    shell_discovery_generation: u64,
    shell_discovery_cancellation: Option<CancellationToken>,
    default_target_generation: u64,
    pub(super) default_target_request: Option<LocalHostDefaultRequest>,
}

impl Default for LocalHostState {
    /// 构造不触碰磁盘的空状态，服务注入后再由应用 executor 执行首次读取。
    fn default() -> Self {
        Self {
            service: None,
            rows: Vec::new(),
            selected_id: None,
            selected: None,
            editor: None,
            shell_candidates: Vec::new(),
            shell_loading: false,
            shell_error: None,
            editor_load_intent: None,
            editor_load_pending: false,
            editor_load_error: None,
            loading: false,
            saving: false,
            save_outcome_unknown: false,
            pending_delete: None,
            status: None,
            delete_checking: false,
            readback_in_progress: false,
            unknown_readback: None,
            unknown_target_missing: false,
            pending_action: None,
            unknown_mutation: None,
            generation: 0,
            editor_load_generation: 0,
            editor_generation: 0,
            shell_discovery_generation: 0,
            shell_discovery_cancellation: None,
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

    /// 仅对当前本地编辑器发起可取消发现；代次与 profile ID 一起拒绝已关闭表单的迟到回执。
    /// @author kongweiguang
    pub(super) fn discover_local_shells(&mut self, cx: &mut Context<Self>) {
        let Some(service) = self.local_hosts.service.clone() else {
            self.local_hosts.shell_error = Some(
                crate::locale::text(
                    "本地主机服务尚未就绪，可直接填写 Shell 路径。",
                    "The local host service is not ready. You can enter a Shell path manually.",
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
        self.cancel_local_shell_discovery();
        self.local_hosts.shell_discovery_generation =
            self.local_hosts.shell_discovery_generation.wrapping_add(1);
        let discovery_generation = self.local_hosts.shell_discovery_generation;
        let editor_generation = self.local_hosts.editor_generation;
        let cancellation = CancellationToken::new();
        self.local_hosts.shell_discovery_cancellation = Some(cancellation.clone());
        self.local_hosts.shell_loading = true;
        self.local_hosts.shell_error = None;
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { service.discover_local_shells(cancellation).await })
                .await;
            let _ = this.update_in(cx, |panel, _window, cx| {
                if panel.local_hosts.shell_discovery_generation != discovery_generation
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
                panel.local_hosts.shell_loading = false;
                panel.local_hosts.shell_discovery_cancellation = None;
                match result {
                    Ok(candidates) => {
                        panel.local_hosts.shell_candidates = candidates;
                        panel.local_hosts.shell_error = None;
                    }
                    Err(error) => {
                        panel.local_hosts.shell_candidates.clear();
                        panel.local_hosts.shell_error = Some(super::panel::localized_error(&error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 候选按稳定 ID 解析；无法把系统参数无损转成表单文本时不改动已有手动输入。
    /// @author kongweiguang
    pub(super) fn select_local_shell(
        &mut self,
        candidate_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(candidate) = self
            .local_hosts
            .shell_candidates
            .iter()
            .find(|candidate| candidate.id() == candidate_id)
            .cloned()
        else {
            self.local_hosts.shell_error = Some(crate::locale::text(
                "Shell 候选已失效，请重新扫描或手动填写路径。",
                "The Shell candidate is no longer available. Scan again or enter a path manually.",
            ).to_string());
            cx.notify();
            return;
        };
        let converted = candidate
            .program()
            .to_str()
            .map(str::to_owned)
            .and_then(|program| {
                candidate
                    .args()
                    .iter()
                    .map(|arg| arg.to_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
                    .map(|args| (program, args))
            });
        let Some((program, args)) = converted else {
            self.local_hosts.shell_error = Some(crate::locale::text(
                "该 Shell 路径或参数无法用文本表示；原输入已保留，请手动填写。",
                "This Shell path or its arguments cannot be represented as text. Your input was preserved; enter it manually.",
            ).to_string());
            cx.notify();
            return;
        };
        let Some(editor) = self.local_hosts.editor.as_mut() else {
            return;
        };
        editor
            .shell
            .update(cx, |state, cx| state.set_value(program, window, cx));
        editor
            .args
            .update(cx, |state, cx| state.set_value(args.join("\n"), window, cx));
        self.local_hosts.shell_error = None;
        self.refresh_local_host_dirty(cx);
    }

    /// 显式恢复默认终端 Profile 会覆盖点击时已有的启动设置，但仍保护点击后新增的输入。
    /// @author kongweiguang
    pub(super) fn use_default_local_shell(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.local_hosts.editor.as_ref() else {
            return;
        };
        let expected = read_editor_baseline(editor, cx);
        self.resolve_default_local_target(Some(expected), cx);
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

    /// 自动填充与显式切换共用同一请求；保留意图供协议返回重试，代次阻止旧回执重放。
    /// @author kongweiguang
    fn resolve_default_local_target(
        &mut self,
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
            explicit_override: explicit_override.clone(),
        });
        self.local_hosts.default_target_generation =
            self.local_hosts.default_target_generation.wrapping_add(1);
        let default_generation = self.local_hosts.default_target_generation;
        let editor_generation = self.local_hosts.editor_generation;
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { service.default_local_session_target().await })
                .await;
            let _ = this.update_in(cx, move |panel, window, cx| {
                if panel.local_hosts.default_target_generation != default_generation
                    || panel.local_hosts.editor_generation != editor_generation
                    || panel.local_hosts.editor.as_ref().map(|editor| editor.profile_id.as_str())
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
                                let projection = if let Some(expected) = explicit_override.as_ref() {
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
                                    editor.shell.update(cx, |state, cx| state.set_value(value, window, cx));
                                }
                                if let Some(value) = projection.cwd {
                                    editor.cwd.update(cx, |state, cx| state.set_value(value, window, cx));
                                }
                                if let Some(value) = projection.args {
                                    editor.args.update(cx, |state, cx| state.set_value(value, window, cx));
                                }
                                if let Some(value) = projection.env {
                                    editor.env.update(cx, |state, cx| state.set_value(value, window, cx));
                                }
                                editor.baseline = projection.baseline;
                                editor.dirty = editor.is_dirty(cx);
                            }
                            panel.status = Some(if explicit_override.is_some() {
                                crate::locale::text("已应用默认终端 Profile；请求后的修改保持不变。", "The default terminal Profile was applied; edits made after the request were preserved.").to_string()
                            } else {
                                crate::locale::text("系统默认目标已就绪；仅补全期间未修改的字段。", "System defaults are ready; only fields unchanged during lookup were filled.").to_string()
                            });
                        }
                        Err(message) => panel.status = Some(message),
                    },
                    Err(error) => panel.status = Some(super::panel::localized_error(&error)),
                }
                panel.local_hosts.status = panel.status.clone();
                cx.notify();
            });
        }).detach();
        self.status = Some(
            crate::locale::text(
                "正在读取默认终端 Profile…",
                "Loading the default terminal Profile…",
            )
            .to_string(),
        );
        cx.notify();
    }

    /// 关闭或替换表单会同时丢弃默认目标意图，避免以后打开另一表单时重放旧请求。
    /// @author kongweiguang
    pub(super) fn cancel_local_host_background_requests(&mut self) {
        self.local_hosts.shell_discovery_generation =
            self.local_hosts.shell_discovery_generation.wrapping_add(1);
        if let Some(cancellation) = self.local_hosts.shell_discovery_cancellation.take() {
            cancellation.cancel();
        }
        self.local_hosts.shell_loading = false;
        self.local_hosts.default_target_generation =
            self.local_hosts.default_target_generation.wrapping_add(1);
        self.local_hosts.default_target_request = None;
    }

    /// 仅恢复切协议留下的在途意图；已完成、失败或被关闭的请求不会反复覆盖初值。
    /// @author kongweiguang
    pub(super) fn resume_local_host_default_target(&mut self, cx: &mut Context<Self>) {
        if let Some(request) = self.local_hosts.default_target_request.take() {
            self.resolve_default_local_target(request.explicit_override, cx);
        }
    }

    /// 新扫描取代旧扫描并保留跨线程可观察的取消令牌。
    /// @author kongweiguang
    fn cancel_local_shell_discovery(&mut self) {
        self.local_hosts.shell_discovery_generation =
            self.local_hosts.shell_discovery_generation.wrapping_add(1);
        if let Some(cancellation) = self.local_hosts.shell_discovery_cancellation.take() {
            cancellation.cancel();
        }
        self.local_hosts.shell_loading = false;
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

    /// 仅对当前完整列表中的稳定 ID 执行 ProfileService 的原子默认项切换，并保留未知回执所有权。
    pub(super) fn request_set_default_local_host(
        &mut self,
        profile_id: String,
        cx: &mut Context<Self>,
    ) {
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            self.status = Some(crate::locale::text(
                "当前配置处于只读恢复状态，不能设置默认本地主机。",
                "The configuration is in read-only recovery mode; the default local host cannot be changed.",
            ).to_string());
            cx.notify();
            return;
        }
        if self.local_hosts.mutation_blocked()
            || self.local_hosts.loading
            || self.local_hosts.pending_delete.is_some()
        {
            self.status = Some(crate::locale::text(
                "本地主机配置正在读取、保存或等待核对，暂不能设置默认项。",
                "Local host configuration is loading, saving, or awaiting reconciliation; the default cannot be changed yet.",
            ).to_string());
            cx.notify();
            return;
        }
        let dirty_editor = self.local_host_is_dirty(cx)
            || self
                .editor
                .as_ref()
                .is_some_and(|editor| editor.is_dirty(cx));
        if dirty_editor {
            self.status = Some(crate::locale::text(
                "请先保存或放弃未完成的连接编辑，再设置默认本地主机。",
                "Save or discard the unfinished connection edits before changing the default local host.",
            ).to_string());
            cx.notify();
            return;
        }
        let Some(target) = local_host_default_target(&self.local_hosts.rows, &profile_id) else {
            self.status = Some(
                crate::locale::text(
                    "列表中已找不到这个本地主机，请刷新后重试。",
                    "This local host is no longer in the list. Refresh and try again.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let current_default_count = self
            .local_hosts
            .rows
            .iter()
            .filter(|row| row.profile.is_default)
            .count();
        if target.profile.is_default && current_default_count == 1 {
            self.status = Some(
                crate::locale::text(
                    "这个本地主机已经是唯一的默认项。",
                    "This local host is already the only default.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let Some(service) = self.local_hosts.service.clone() else {
            self.status = Some(
                crate::locale::text(
                    "本地主机服务尚未就绪，请稍后重试。",
                    "The local host service is not ready yet. Try again shortly.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let expected = local_host_profile_revisions(&self.local_hosts.rows);
        let executor = self.executor.clone();
        let request_id = profile_id.clone();
        self.local_hosts.saving = true;
        self.local_hosts.status = Some(
            crate::locale::text("正在更新默认本地主机…", "Updating the default local host…")
                .to_string(),
        );
        self.status = self.local_hosts.status.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result =
                executor
                    .run(move |_| async move {
                        service.set_default_profile(&request_id, expected).await
                    })
                    .await;
            let _ = this.update_in(cx, move |panel, _window, cx| {
                panel.finish_local_host_set_default(profile_id, result, cx);
            });
        })
        .detach();
    }

    /// 依据真实操作回执区分成功、普通失败与未知结果，成功后刷新完整列表和选中快照。
    fn finish_local_host_set_default(
        &mut self,
        profile_id: String,
        result: Result<VersionedProfile, AppError>,
        cx: &mut Context<Self>,
    ) {
        self.local_hosts.saving = false;
        match result {
            Ok(saved) if saved.profile.id == profile_id => {
                self.local_hosts.selected_id = Some(saved.profile.id.clone());
                self.local_hosts.selected = Some(saved);
                self.status = Some(
                    crate::locale::text("默认本地主机已更新。", "Default local host updated.")
                        .to_string(),
                );
                self.reload_local_hosts(cx);
            }
            Ok(_) => {
                self.local_hosts.save_outcome_unknown = true;
                self.local_hosts.unknown_mutation =
                    Some(LocalHostUnknownMutation::SetDefault { profile_id });
                self.local_hosts.unknown_readback = None;
                self.local_hosts.unknown_target_missing = false;
                self.status = Some(crate::locale::text(
                    "默认项回执与请求目标不一致；结果需显式读回核对，不会自动重放。",
                    "The default update receipt did not match the requested target. Read the list back to reconcile; the request will not be replayed automatically.",
                ).to_string());
            }
            Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                self.local_hosts.save_outcome_unknown = true;
                self.local_hosts.unknown_mutation =
                    Some(LocalHostUnknownMutation::SetDefault { profile_id });
                self.local_hosts.unknown_readback = None;
                self.local_hosts.unknown_target_missing = false;
                self.status = Some(crate::locale::text(
                    "设置默认项的结果未知。请显式核对当前默认状态；不会自动重放。",
                    "The default change result is unknown. Explicitly verify the current default state; it will not be replayed automatically.",
                ).to_string());
            }
            Err(error) => {
                self.status = Some(super::panel::localized_error(&error));
                if error.public_code() == PublicErrorCode::Conflict {
                    self.reload_local_hosts(cx);
                }
            }
        }
        self.local_hosts.status = self.status.clone();
        cx.notify();
    }

    /// 未知结果必须由用户显式核对；普通 watcher 刷新不会擅自解除门禁。
    pub(super) fn review_local_host_unknown(&mut self, cx: &mut Context<Self>) {
        if !self.local_hosts.save_outcome_unknown || self.local_hosts.operation_in_progress() {
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
        if matches!(
            self.local_hosts.unknown_mutation.as_ref(),
            Some(LocalHostUnknownMutation::SetDefault { .. })
        ) {
            self.status = Some(crate::locale::text(
                "设为默认只核对当前状态，不应用读回快照，也不会重放原操作。",
                "Setting a default only reconciles the current state. The readback is not applied and the original operation is not replayed.",
            ).to_string());
            self.local_hosts.status = self.status.clone();
            cx.notify();
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
                self.local_hosts.selected = Some(snapshot.clone());
                self.clear_local_host_unknown();
                self.discover_local_shells(cx);
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
            LocalHostUnknownMutation::SetDefault { .. } => {
                self.status = Some(crate::locale::text(
                    "设为默认没有草稿可继续；请先核对当前状态，再明确发起新的操作。",
                    "A default change has no draft to continue. Reconcile the current state first, then explicitly start a new operation.",
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

    /// 保存前检查配置恢复模式、组有效性及未知结果门禁，再把完整草稿提交给唯一 ProfileService。
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
        let Some(service) = self.local_hosts.service.clone() else {
            self.status = Some(
                crate::locale::text(
                    "本地主机服务尚未就绪。",
                    "The local host service is not ready yet.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let executor = self.executor.clone();
        let id = profile_id.clone();
        self.local_hosts.saving = true;
        self.local_hosts.status =
            Some(crate::locale::text("正在保存本地主机…", "Saving local host…").to_string());
        self.status = self.local_hosts.status.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { service.save_profile(draft, revision).await })
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
        result: Result<VersionedProfile, AppError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.local_hosts.saving = false;
        match result {
            Ok(saved) => {
                let saved_profile_id = saved.profile.id.clone();
                self.local_hosts.selected_id = Some(saved_profile_id.clone());
                self.local_hosts.selected = Some(saved.clone());
                upsert_profile(&mut self.local_hosts.rows, saved);
                cx.emit(saved_selection_event(SavedSelectionTarget::LocalProfile(
                    saved_profile_id,
                )));
                self.local_hosts.pending_delete = None;
                self.close_unified_editor(window, cx);
                let success_status =
                    crate::locale::text("本地主机已保存。", "Local host saved.").to_string();
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
                if error.public_code() == PublicErrorCode::OutcomeUnknown {
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
        self.local_hosts.shell_candidates.clear();
        self.local_hosts.shell_error = None;
        self.local_hosts.shell_loading = false;
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
        let Some(service) = self.local_hosts.service.clone() else {
            return;
        };
        let executor = self.executor.clone();
        let generation = self.local_hosts.generation.wrapping_add(1);
        self.local_hosts.generation = generation;
        self.local_hosts.delete_checking = true;
        self.local_hosts.pending_delete = None;
        self.status = Some(
            crate::locale::text("正在核对删除目标…", "Checking the delete target…").to_string(),
        );
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { service.get_profile(&profile_id).await })
                .await;
            let _ = this.update_in(cx, |panel, _window, cx| {
                if panel.local_hosts.generation != generation {
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

    /// 关闭根确认时只清除确认快照，不触及 ProfileService。
    pub(crate) fn cancel_delete_local_host(&mut self, cx: &mut Context<Self>) {
        if !self.local_hosts.mutation_blocked() {
            self.local_hosts.pending_delete = None;
            cx.notify();
        }
    }

    /// 根确认后才按确认快照中的精确 ID/revision 删除，重复确认和未知结果都受门禁约束。
    pub(crate) fn confirm_delete_local_host(&mut self, cx: &mut Context<Self>) {
        if self.local_hosts.mutation_blocked()
            || self.recovery.mode == ConfigurationMode::ReadOnlyRecovery
        {
            return;
        }
        let Some(pending) = self.local_hosts.pending_delete.clone() else {
            return;
        };
        let profile_id = pending.profile.id.clone();
        let revision = pending.revision.clone();
        let Some(service) = self.local_hosts.service.clone() else {
            return;
        };
        let executor = self.executor.clone();
        self.local_hosts.saving = true;
        self.status =
            Some(crate::locale::text("正在删除本地主机…", "Deleting local host…").to_string());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let expected_revision = revision.clone();
            let id = profile_id.clone();
            let result = executor
                .run(move |_| async move { service.delete_profile(&id, expected_revision).await })
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
        result: Result<bool, AppError>,
        cx: &mut Context<Self>,
    ) {
        self.local_hosts.saving = false;
        match result {
            Ok(true) => {
                self.local_hosts.pending_delete = None;
                self.local_hosts
                    .rows
                    .retain(|row| row.profile.id != profile_id);
                if self.local_hosts.selected_id.as_deref() == Some(profile_id.as_str()) {
                    self.local_hosts.selected_id = None;
                    self.local_hosts.selected = None;
                }
                self.status = Some(
                    crate::locale::text("本地主机已删除。", "Local host deleted.").to_string(),
                );
                self.reload_local_hosts(cx);
            }
            Ok(false) => {
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

    /// 连接项异步加载统一走 executor；世代号阻止慢结果覆盖较新的选择或配置刷新。
    fn load_local_profiles(&mut self, explicit_readback: bool, cx: &mut Context<Self>) {
        let Some(service) = self.local_hosts.service.clone() else {
            return;
        };
        let Some(generation) = self.local_hosts.begin_list_load(explicit_readback) else {
            return;
        };
        let executor = self.executor.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { service.list_profiles().await })
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

    /// 读回只更新 profile 列表；脏编辑仍保留，未知写入必须经过单独的用户恢复动作。
    fn finish_local_profiles_load(
        &mut self,
        result: Result<Vec<VersionedProfile>, AppError>,
        explicit_readback: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.local_hosts.loading = false;
        self.local_hosts.readback_in_progress = false;
        match result {
            Ok(rows) => {
                self.local_hosts.rows = rows;
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
                    self.reconcile_local_host_readback(cx);
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
                    self.local_hosts.save_outcome_unknown = true;
                }
            }
        }
        self.local_hosts.status = self.status.clone();
        cx.notify();
    }

    /// 显式读回后按稳定 ID 和原 revision 判断保存/删除当前事实，不把读回伪装成成功回执。
    fn reconcile_local_host_readback(&mut self, cx: &mut Context<Self>) {
        let Some(operation) = self.local_hosts.unknown_mutation.clone() else {
            return;
        };
        match operation {
            LocalHostUnknownMutation::Save { profile_id } => {
                let current = self
                    .local_hosts
                    .rows
                    .iter()
                    .find(|row| row.profile.id == profile_id)
                    .cloned();
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
            LocalHostUnknownMutation::SetDefault { profile_id } => {
                let readback = local_host_default_readback(&self.local_hosts.rows, &profile_id);
                self.local_hosts.unknown_readback =
                    local_host_default_target(&self.local_hosts.rows, &profile_id).cloned();
                self.local_hosts.unknown_target_missing = !readback.target_present;
                let default_count = readback.default_profile_ids.len();
                self.status = Some(if default_count > 1 {
                    crate::locale::text(
                        format!(
                            "已核对当前默认状态，未重放原操作。发现 {default_count} 个默认本地主机，请修复配置后再次读回；写入仍封锁。"
                        ),
                        format!(
                            "The current default state was verified; the original operation was not replayed. Found {default_count} default local hosts. Repair the configuration and read it back again; writes remain blocked."
                        ),
                    )
                    .to_string()
                } else if default_count == 0 {
                    let target_state = if readback.target_present {
                        "目标仍存在，但当前没有默认本地主机。"
                    } else {
                        "目标已不存在，且当前没有默认本地主机。"
                    };
                    crate::locale::text(
                        format!("已核对当前默认状态，未重放原操作。{target_state}"),
                        if readback.target_present {
                            "The current default state was verified; the original operation was not replayed. The target still exists, but no local host is currently default.".to_owned()
                        } else {
                            "The current default state was verified; the original operation was not replayed. The target no longer exists, and no local host is currently default.".to_owned()
                        },
                    )
                    .to_string()
                } else if readback.target_is_default {
                    crate::locale::text(
                        "已核对当前默认状态，未重放原操作。目标目前是唯一默认本地主机。",
                        "The current default state was verified; the original operation was not replayed. The target is currently the only default local host.",
                    )
                    .to_string()
                } else {
                    let default_id = &readback.default_profile_ids[0];
                    let default_name = self
                        .local_hosts
                        .rows
                        .iter()
                        .find(|row| row.profile.id.as_str() == default_id)
                        .map(|row| row.profile.name.as_str())
                        .unwrap_or(default_id.as_str());
                    let target_state = if readback.target_present {
                        "目标仍存在。"
                    } else {
                        "目标已不存在。"
                    };
                    crate::locale::text(
                        format!(
                            "已核对当前默认状态，未重放原操作。{target_state}当前默认项为“{default_name}”。"
                        ),
                        format!(
                            "The current default state was verified; the original operation was not replayed. {}The current default is “{default_name}”.",
                            if readback.target_present { "The target still exists. " } else { "The target no longer exists. " }
                        ),
                    )
                    .to_string()
                });
                if local_host_default_readback_can_release(&readback) {
                    self.clear_local_host_unknown();
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
                self.start_default_local_target_resolution(cx);
                self.discover_local_shells(cx);
                self.status = Some(crate::locale::text("填写本地主机配置后保存；不会自动设为默认主机。", "Configure the local host and save it; it will not become the default host automatically.").to_string());
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
        self.resolve_default_local_target(None, cx);
    }

    /// 基于稳定 Profile ID 读取最新快照；失败时保留现有编辑器，成功后才清理旧环境变量并替换。
    /// 编辑/复制/冲突重读不得用导航列表旧 revision 写回，也不能因读取失败丢失当前草稿。
    /// @author kongweiguang
    pub(super) fn load_local_profile_editor(
        &mut self,
        profile_id: String,
        mode: LocalHostEditorMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(service) = self.local_hosts.service.clone() else {
            return;
        };
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
                .run(move |_| async move { service.get_profile(&requested_id).await })
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
                        panel.local_hosts.selected = Some(snapshot.clone());
                        upsert_profile(&mut panel.local_hosts.rows, snapshot);
                        panel.discover_local_shells(cx);
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

    /// 按稳定 ID 重新读取详情，选择过程不写配置且不会将 profile ID 塞入临时 Local 目标。
    fn load_local_host_selection(&mut self, profile_id: Option<String>, cx: &mut Context<Self>) {
        let Some(profile_id) = profile_id else {
            self.local_hosts.selected_id = None;
            self.local_hosts.selected = None;
            self.local_hosts.pending_delete = None;
            cx.notify();
            return;
        };
        let Some(service) = self.local_hosts.service.clone() else {
            return;
        };
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
                .run(move |_| async move { service.get_profile(&profile_id).await })
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
        self.local_hosts.shell_candidates.clear();
        self.local_hosts.shell_error = None;
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
