//! 命令历史、片段、工作流与建议面板的 GPUI 状态和用例协调。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, sync::Arc};

use gpui_kit::{
    AppContext as _, Context, Entity, FocusHandle, SharedString, Subscription, Window,
    component::{
        input::{InputEvent, InputState, TextareaState},
        searchable_list::SearchableListItem,
        select::{SelectEvent, SelectState},
    },
};
use kerminal_application::{
    AppError, ApplicationTaskExecutor,
    commands::{
        BroadcastExecutionResult, BroadcastSessionBinding, CommandExecutionReceipt,
        CommandsService, SnippetCatalogCursor, SnippetCatalogSection, SnippetExecutionService,
        SnippetTransferFileService, VersionedCommandDefinition, WorkflowExecution,
        WorkflowExecutionControl, WorkflowExecutionRequest, WorkflowProgressEvent,
        WorkflowProgressPublisher, WorkflowRunProgress, WorkflowStepExecution,
    },
    configuration_directory::ConfigurationDirectoryService,
    preferences::{ConfigurationValidationService, ThemeMode},
    sessions::UiSessionBinding,
};
use kerminal_domain::{
    commands::{
        BroadcastAnalysis, BroadcastTarget, CommandHistoryEntry, CommandHistoryTarget,
        CommandSuggestionCandidate, CommandSuggestionRequest, CommandWorkflow, CommandWorkflowStep,
        SnippetCatalogItem, SnippetCatalogOrigin, SnippetContextBinding, SnippetContextBindingKind,
        SnippetDefaultAction, SnippetDefinition, SnippetPreferenceKey, SnippetRisk, SnippetScope,
        SnippetVariable, SnippetVariableKind, SnippetVariableRenderStrategy,
        SuggestionProviderKind, SuggestionQueryMode, WorkflowListRequest, WorkflowRun,
        WorkflowRunState, WorkflowStepState,
    },
    identity::{PaneId, SessionId},
    sessions::{SessionInfo, SessionOwner, SessionTarget},
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroize;

pub mod inline_suggestions;

#[path = "commands/configuration_watch.rs"]
mod configuration_watch;
#[path = "commands/history_query.rs"]
mod history_query;
#[path = "commands/logs.rs"]
mod logs;
#[path = "commands/render.rs"]
mod render;
#[path = "commands/send_state.rs"]
mod send_state;
#[path = "commands/snippet_applicability.rs"]
mod snippet_applicability;
#[path = "commands/snippet_browser.rs"]
mod snippet_browser;
#[path = "commands/snippet_fill.rs"]
mod snippet_fill;
#[path = "commands/snippet_run.rs"]
mod snippet_run;
#[path = "commands/snippet_transfer_ui.rs"]
mod snippet_transfer_ui;
#[path = "commands/workflow_editor.rs"]
mod workflow_editor;

use send_state::{SendOperationKind, SendOperationState, SendOperationToken};

pub use logs::{LogsTarget, LogsView};

/// 面板按业务任务切换内容；切换仅改变视图，不销毁共享终端会话。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandsSection {
    History,
    Snippets,
    Workflows,
}

/// 工作流命令回执未知时保留核对所需的原始运行身份；人工确认只解除新运行门禁，不改写结果。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WorkflowUnknownReview {
    pub(super) run_id: String,
    pub(super) target: String,
    pub(super) step_title: String,
    acknowledged: bool,
}

impl WorkflowUnknownReview {
    /// 未核对前禁止启动新一轮；确认后仍保留摘要，避免把 Unknown 当成成功或删除证据。
    ///
    /// @author kongweiguang
    pub(super) fn blocks_new_run(&self) -> bool {
        !self.acknowledged
    }

    /// 用户明确确认已检查原终端后只解除门禁，不推断执行成功或失败。
    ///
    /// @author kongweiguang
    fn acknowledge(&mut self) -> bool {
        if self.acknowledged {
            return false;
        }
        self.acknowledged = true;
        true
    }
}

/// 片段变量只保留当前面板草稿；敏感输入使用 GPUI Kit password content type。
pub(super) struct SnippetVariableInput {
    pub id: u64,
    pub variable: SnippetVariable,
    pub name: Entity<InputState>,
    pub label: Entity<InputState>,
    pub description: Entity<InputState>,
    pub default_value: Entity<InputState>,
    pub suggestions: Entity<InputState>,
    pub validation: Entity<InputState>,
    pub input: Entity<InputState>,
    pub kind_select: Entity<SelectState<Vec<SnippetVariableKindOption>>>,
    pub render_strategy_select: Entity<SelectState<Vec<SnippetVariableRenderStrategyOption>>>,
    _kind_subscription: Subscription,
    _render_strategy_subscription: Subscription,
}

/// 变量类型下拉保存稳定领域枚举，显示文字仅用于本地化搜索与选择。
///
/// @author kongweiguang
#[derive(Clone)]
pub(super) struct SnippetVariableKindOption {
    kind: SnippetVariableKind,
    label: SharedString,
}

impl SearchableListItem for SnippetVariableKindOption {
    type Value = SnippetVariableKind;

    /// 标题单独表达类型含义，保证下拉搜索与辅助技术读到的都是可理解文案。
    ///
    /// @author kongweiguang
    fn title(&self) -> SharedString {
        self.label.clone()
    }

    /// 选择结果使用领域类型，不从本地化标签反解析业务值。
    ///
    /// @author kongweiguang
    fn value(&self) -> &Self::Value {
        &self.kind
    }
}

/// 渲染策略菜单只包含当前类型可安全使用的选项。
///
/// @author kongweiguang
#[derive(Clone)]
pub(super) struct SnippetVariableRenderStrategyOption {
    strategy: SnippetVariableRenderStrategy,
    label: SharedString,
}

impl SearchableListItem for SnippetVariableRenderStrategyOption {
    type Value = SnippetVariableRenderStrategy;

    /// 用明确策略名称解释插值安全边界，不显示难以区分的缩写。
    ///
    /// @author kongweiguang
    fn title(&self) -> SharedString {
        self.label.clone()
    }

    /// 选择结果直接携带领域策略枚举。
    ///
    /// @author kongweiguang
    fn value(&self) -> &Self::Value {
        &self.strategy
    }
}

/// 绑定项单独保存目标输入，编辑类型时保留所有其它绑定和未知元数据。
pub(super) struct SnippetContextBindingInput {
    pub binding: SnippetContextBinding,
    pub target_id: Entity<InputState>,
}

/// 统一收集已知片段字段，保存与 dirty 检测共享同一规范化规则。
#[derive(Clone, PartialEq, Eq)]
pub(super) struct SnippetDraftValues {
    pub title: String,
    pub command: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub sort_order: i64,
    pub scope: SnippetScope,
    pub category: Option<String>,
    pub risk: SnippetRisk,
    pub default_action: SnippetDefaultAction,
    pub variables: Vec<SnippetVariable>,
    pub context_bindings: Vec<SnippetContextBinding>,
}

/// 片段 Fill 回执必须仍属于点击时的目标、定义、编辑器和草稿快照。
///
/// @author kongweiguang
#[derive(Clone, PartialEq, Eq)]
struct SnippetFillGuard {
    request_generation: u64,
    session_view_generation: u64,
    session_id: Option<SessionId>,
    binding: Option<UiSessionBinding>,
    section: CommandsSection,
    snippet_generation: u64,
    editor_generation: u64,
    editor_open: bool,
    selected_sensitive: bool,
    command_draft: String,
    definition_draft: SnippetDraftValues,
    variable_values: BTreeMap<String, String>,
}

impl Drop for SnippetFillGuard {
    /// 异步 UI 核对也会持有 Secret 变量副本；释放时与应用执行计划一样清除展开值。
    fn drop(&mut self) {
        self.command_draft.zeroize();
        self.definition_draft.command.zeroize();
        for value in self.variable_values.values_mut() {
            value.zeroize();
        }
    }
}

impl SnippetFillGuard {
    /// 目标或编辑器身份改变时丢弃回执，避免迟到操作接管新的工作区状态。
    ///
    /// @author kongweiguang
    fn same_request_context(&self, current: &Self) -> bool {
        self.request_generation == current.request_generation
            && self.session_view_generation == current.session_view_generation
            && self.session_id == current.session_id
            && self.binding == current.binding
            && self.section == current.section
            && self.snippet_generation == current.snippet_generation
            && self.editor_generation == current.editor_generation
    }

    /// 用户在等待期间编辑命令、变量定义或变量值时保留新输入，不让旧预览覆盖它。
    ///
    /// @author kongweiguang
    fn drafts_unchanged(&self, current: &Self) -> bool {
        self.command_draft == current.command_draft
            && self.selected_sensitive == current.selected_sensitive
            && self.definition_draft == current.definition_draft
            && self.variable_values == current.variable_values
    }
}

/// 编辑器切换请求暂存为 typed intent，保存/丢弃确认完成前不改选中定义。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum EditorTransition {
    New,
    Open(String),
    OpenKey(SnippetPreferenceKey),
    PreviewKey(SnippetPreferenceKey),
    Delete,
}

/// 根级片段弹层只投影可见性与 Fill 后焦点意图；草稿和 revision 仍由 CommandsView 唯一持有。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnippetEditorVisibilityChanged {
    pub open: bool,
    /// Fill 成功关闭弹层后，Root 应聚焦历史命令草稿而不是恢复到已隐藏的变量输入框。
    pub focus_command_draft: bool,
}

/// 片段成功写入后的焦点意图票据；Root 仍须核对活动目标和点击时焦点。
///
/// @author kongweiguang
#[derive(Clone, Debug)]
pub(crate) struct SnippetTerminalFillCompleted {
    pub(crate) binding: UiSessionBinding,
    pub(crate) snippet_fill_generation: u64,
    pub(crate) session_view_generation: u64,
    pub(crate) expected_focus: Option<FocusHandle>,
    pub(crate) snippet_editor_generation: Option<u64>,
}

/// 活动 UI pane 的名称只作为展示快照，并随不可变 binding 一起失效。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommandsTargetPresentation {
    pub binding: UiSessionBinding,
    pub label: String,
}

impl CommandsTargetPresentation {
    /// 目标代次变化时隐藏旧名称，避免把已重连 pane 的显示身份误贴到新 session。
    ///
    /// @author kongweiguang
    pub(super) fn label_for_binding<'a>(
        &'a self,
        binding: Option<&UiSessionBinding>,
    ) -> Option<&'a str> {
        (binding == Some(&self.binding)).then_some(self.label.as_str())
    }
}

/// 一次命令提交的不可变 UI 所有权；迟到回执必须同时核对目标、代次与操作令牌。
struct CommandSubmissionSnapshot {
    session_id: SessionId,
    binding: UiSessionBinding,
    session_generation: u64,
    operation_token: SendOperationToken,
    command: String,
}

/// 工作流每一步保留独立输入实体，避免把多行 shell 源码编码进 UI 迷你语法。
pub(super) struct WorkflowStepEditor {
    pub definition: CommandWorkflowStep,
    pub title: Entity<InputState>,
    pub command: Entity<TextareaState>,
}

/// 独立的命令工作区面板，由 RootView 持有并在活动 session 变化时更新目标。
pub struct CommandsView {
    pub(super) commands: Arc<CommandsService>,
    pub(super) executor: ApplicationTaskExecutor,
    /// 文件目录/validator/导入文件都由 composition root 装配，UI 不直接访问路径或磁盘。
    pub(super) configuration_validation: Option<ConfigurationValidationService>,
    pub(super) configuration_directory: Option<ConfigurationDirectoryService>,
    pub(super) snippet_transfer_files: Option<SnippetTransferFileService>,
    /// Shell 执行与目标绑定在应用用例内完成，面板不直接编排 SessionService。
    pub(super) snippet_execution: Option<Arc<SnippetExecutionService>>,
    pub(super) session_id: Option<SessionId>,
    pub(super) session_info: Option<SessionInfo>,
    pub(super) theme_mode: ThemeMode,
    /// 宿主槽位决定是否将编辑器改为列表与详情两级导航。
    pub(super) compact_layout: bool,
    /// 当前紧凑面板是否处于片段或工作流详情页；定义草稿始终留在原实体。
    pub(super) compact_detail: Option<CommandsSection>,
    pub(super) section: CommandsSection,
    pub(super) history_query: Entity<InputState>,
    pub(super) command_input: Entity<InputState>,
    pub(super) snippet_title: Entity<InputState>,
    pub(super) snippet_query: Entity<InputState>,
    /// TabList 的独立键盘焦点，避免搜索输入与列表方向键处理互相争用。
    pub(super) snippet_catalog_focus: FocusHandle,
    pub(super) snippet_query_subscription: Option<Subscription>,
    pub(super) snippet_command: Entity<TextareaState>,
    pub(super) workflow_title: Entity<InputState>,
    pub(super) snippet_description: Entity<InputState>,
    pub(super) snippet_tags: Entity<InputState>,
    pub(super) snippet_category: Entity<InputState>,
    pub(super) snippet_sort_order: Entity<InputState>,
    pub(super) snippet_scope: SnippetScope,
    pub(super) snippet_risk: SnippetRisk,
    pub(super) snippet_default_action: SnippetDefaultAction,
    pub(super) snippet_context_bindings: Vec<SnippetContextBindingInput>,
    pub(super) snippet_advanced_open: bool,
    pub(super) workflow_steps: Vec<WorkflowStepEditor>,
    pub(super) history: Vec<CommandHistoryEntry>,
    pub(super) snippet_catalog_section: SnippetCatalogSection,
    pub(super) snippet_page_items: Vec<SnippetCatalogItem>,
    pub(super) snippet_total_count: usize,
    pub(super) snippet_next_cursor: Option<SnippetCatalogCursor>,
    /// 最近一次完整成功页所属的筛选条件；保留旧行时由此识别它们是过期投影。
    pub(super) snippet_page_loaded_section: Option<SnippetCatalogSection>,
    pub(super) snippet_page_loaded_query: Option<String>,
    /// 当前目录读取失败时单独呈现，避免把暂不可用误显示为真正空目录。
    pub(super) snippet_page_error: Option<String>,
    /// continuation 失败时保留精确游标；重试只查询该页，不改写前面的成功页。
    pub(super) snippet_page_retry_cursor: Option<SnippetCatalogCursor>,
    pub(super) snippet_page_loading: bool,
    pub(super) snippet_page_generation: u64,
    pub(super) snippet_query_generation: u64,
    pub(super) snippet_page_cancellation: Option<CancellationToken>,
    /// 列表原位预览只保存精确目录键和加载反馈，片段定义仍由 selected_snippet 唯一持有。
    snippet_inline_preview: snippet_browser::SnippetInlinePreview,
    /// 只保留最近一次显式复制请求，避免较慢的旧结果覆盖用户后续剪贴板选择。
    pub(super) snippet_copy_generation: u64,
    pub(super) snippet_copy_cancellation: Option<CancellationToken>,
    /// 只追踪当前选中定义与可信 binding 的适用性结果，避免列表渲染触发逐项 IO。
    snippet_applicability: snippet_applicability::SelectedSnippetApplicability,
    /// 片段创建结果未知时保留原 ID 与正文，后续只能精确读回，不能生成新 ID 重试。
    pending_builtin_copy: Option<snippet_browser::PendingBuiltinSnippetCopy>,
    snippet_import_review: Option<snippet_transfer_ui::SnippetImportReview>,
    snippet_run: snippet_run::SnippetRunUiState,
    pub(super) snippet_transfer_busy: bool,
    pub(super) snippet_transfer_generation: u64,
    pub(super) snippet_transfer_cancellation: Option<CancellationToken>,
    pub(super) snippet_library_manager_open: bool,
    pub(super) snippet_validation_running: bool,
    pub(super) snippet_validation_report:
        Option<kerminal_application::preferences::ConfigurationValidationReport>,
    pub(super) snippet_directory_request_pending: bool,
    pub(super) snippet_manager_generation: u64,
    pub(super) snippet_manager_cancellation: Option<CancellationToken>,
    pub(super) workflows: Vec<CommandWorkflow>,
    pub(super) ui_sessions: Vec<SessionInfo>,
    pub(super) suggestions: Vec<CommandSuggestionCandidate>,
    pub(super) selected_snippet: Option<VersionedCommandDefinition<SnippetDefinition>>,
    pub(super) selected_snippet_sensitive: bool,
    /// 编辑器可见状态独立于草稿；关闭根级弹层不丢弃定义、revision 或变量值。
    pub(super) snippet_editor_open: bool,
    /// 关闭后重开同一片段也属于新的 modal 请求，迟到 Fill 不得把它再次关闭。
    pub(super) snippet_editor_generation: u64,
    pub(super) selected_workflow: Option<VersionedCommandDefinition<CommandWorkflow>>,
    pub(super) snippet_variables: Vec<SnippetVariableInput>,
    /// 输入控件事件以单调 ID 定位变量；删除前项后迟到选择不会改到相邻变量。
    snippet_variable_id: u64,
    pub(super) snippet_is_new: bool,
    pub(super) workflow_is_new: bool,
    pub(super) snippet_saving: bool,
    pub(super) workflow_saving: bool,
    pub(super) workflow_loading: bool,
    pub(super) workflow_deleting: bool,
    pub(super) snippet_generation: u64,
    /// 只接受最近一次 Fill 回执；并行点击不会让较早请求覆盖较新的操作。
    pub(super) snippet_fill_generation: u64,
    snippet_terminal_fill: snippet_fill::TerminalFillState,
    pub(super) workflow_editor_generation: u64,
    pub(super) workflow_delete_generation: u64,
    pub(super) pending_snippet_transition: Option<EditorTransition>,
    pub(super) pending_workflow_transition: Option<EditorTransition>,
    pub(super) confirm_snippet_delete: bool,
    pub(super) confirm_workflow_delete: bool,
    pub(super) workflow_run: Option<WorkflowRun>,
    /// 整轮工作流固定使用的 UI binding；重连后的新 generation 不能接管旧运行。
    pub(super) workflow_binding: Option<UiSessionBinding>,
    /// 执行开始时的目标快照，切换当前 Tab 时仍能展示审批对应的真实目标。
    pub(super) workflow_session_info: Option<SessionInfo>,
    pub(super) workflow_attempts: Vec<WorkflowStepExecution>,
    pub(super) workflow_control: Option<WorkflowExecutionControl>,
    pub(super) workflow_running: bool,
    /// 当前运行的有界最新快照；run id 与 generation 双重门禁隔离迟到回执。
    pub(super) workflow_progress: Option<WorkflowRunProgress>,
    /// 最近一次未知运行的核对摘要跨新运行保留；只有明确核对后才允许新运行。
    pub(super) workflow_unknown_review: Option<WorkflowUnknownReview>,
    pub(super) pending_broadcast:
        Option<(String, Vec<BroadcastTarget>, Vec<BroadcastSessionBinding>)>,
    pub(super) broadcast_control: Option<CancellationToken>,
    pub(super) record_command: bool,
    pub(super) confirm_clear_history: bool,
    pub(super) configuration_conflict: Option<String>,
    pub(super) loading: bool,
    /// 命令与广播共用唯一 busy token，过期回执不能解除新操作的锁定。
    send_operation: SendOperationState,
    /// 目标 generation 改变时通知用例停止排队；writer 已接纳后必须等真实回执收敛。
    pub(super) command_send_cancellation: Option<CancellationToken>,
    pub(super) status: Option<String>,
    pub(super) session_view_generation: u64,
    /// Root 仅在真实 UI pane/session binding 更新时刷新此快照。
    pub(super) ui_session_binding: Option<UiSessionBinding>,
    /// 与精确 binding 同代的目标展示名；焦点/目标更新后必须重新投影。
    pub(super) target_presentation: Option<CommandsTargetPresentation>,
    pub(super) history_generation: u64,
    pub(super) panel_generation: u64,
    pub(super) suggestion_generation: u64,
    pub(super) suggestion_cancellation: Option<CancellationToken>,
    pub(super) workflow_generation: u64,
}

type PanelLoad = (
    Option<Result<SessionInfo, AppError>>,
    Result<Vec<CommandHistoryEntry>, AppError>,
    Result<Vec<CommandWorkflow>, AppError>,
    Result<Vec<SessionInfo>, AppError>,
);

impl CommandsView {
    /// 使用窗口上下文创建稳定输入实体；布局重绘不会丢失搜索、命令和定义草稿。
    pub fn new(
        commands: Arc<CommandsService>,
        executor: ApplicationTaskExecutor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let configuration_subscription = commands.subscribe_configuration_changes();
        let history_query = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text(
                "搜索命令历史",
                "Search command history",
            ))
        });
        let command_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::locale::text("输入命令…", "Enter a command…"))
        });
        let snippet_title = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text("片段名称", "Snippet name"))
        });
        let snippet_query = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text(
                "搜索名称、命令、标签…",
                "Search titles, commands, and tags…",
            ))
        });
        let snippet_catalog_focus = cx.focus_handle().tab_stop(true);
        let snippet_command = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(crate::locale::text(
                    "命令模板；变量写作 {{name}}",
                    "Command template; variables use {{name}}",
                ))
                .auto_grow(3, 8)
        });
        let workflow_title = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::locale::text("工作流名称", "Workflow name"))
        });
        let snippet_description = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text("说明", "Description"))
        });
        let snippet_tags = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text(
                "标签，逗号分隔",
                "Comma-separated tags",
            ))
        });
        let snippet_category = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text("分类", "Category"))
        });
        let snippet_sort_order = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text("排序", "Sort order"))
        });
        let mut view = Self {
            commands,
            executor,
            configuration_validation: None,
            configuration_directory: None,
            snippet_transfer_files: None,
            snippet_execution: None,
            session_id: None,
            session_info: None,
            theme_mode: ThemeMode::System,
            compact_layout: false,
            compact_detail: None,
            section: CommandsSection::History,
            history_query,
            command_input,
            snippet_title,
            snippet_query,
            snippet_catalog_focus,
            snippet_query_subscription: None,
            snippet_command,
            workflow_title,
            snippet_description,
            snippet_tags,
            snippet_category,
            snippet_sort_order,
            snippet_scope: SnippetScope::Any,
            snippet_risk: SnippetRisk::Unknown,
            snippet_default_action: SnippetDefaultAction::Insert,
            snippet_context_bindings: Vec::new(),
            snippet_advanced_open: false,
            workflow_steps: Vec::new(),
            history: Vec::new(),
            snippet_catalog_section: SnippetCatalogSection::Common,
            snippet_page_items: Vec::new(),
            snippet_total_count: 0,
            snippet_next_cursor: None,
            snippet_page_loaded_section: None,
            snippet_page_loaded_query: None,
            snippet_page_error: None,
            snippet_page_retry_cursor: None,
            snippet_page_loading: false,
            snippet_page_generation: 0,
            snippet_query_generation: 0,
            snippet_page_cancellation: None,
            snippet_inline_preview: snippet_browser::SnippetInlinePreview::Collapsed,
            snippet_copy_generation: 0,
            snippet_copy_cancellation: None,
            snippet_applicability: snippet_applicability::SelectedSnippetApplicability::default(),
            pending_builtin_copy: None,
            snippet_import_review: None,
            snippet_run: snippet_run::SnippetRunUiState::default(),
            snippet_transfer_busy: false,
            snippet_transfer_generation: 0,
            snippet_transfer_cancellation: None,
            snippet_library_manager_open: false,
            snippet_validation_running: false,
            snippet_validation_report: None,
            snippet_directory_request_pending: false,
            snippet_manager_generation: 0,
            snippet_manager_cancellation: None,
            workflows: Vec::new(),
            ui_sessions: Vec::new(),
            suggestions: Vec::new(),
            selected_snippet: None,
            selected_snippet_sensitive: false,
            snippet_editor_open: false,
            snippet_editor_generation: 0,
            selected_workflow: None,
            snippet_variables: Vec::new(),
            snippet_variable_id: 0,
            snippet_is_new: false,
            workflow_is_new: false,
            snippet_saving: false,
            workflow_saving: false,
            workflow_loading: false,
            workflow_deleting: false,
            snippet_generation: 0,
            snippet_fill_generation: 0,
            snippet_terminal_fill: Default::default(),
            workflow_editor_generation: 0,
            workflow_delete_generation: 0,
            pending_snippet_transition: None,
            pending_workflow_transition: None,
            confirm_snippet_delete: false,
            confirm_workflow_delete: false,
            workflow_run: None,
            workflow_binding: None,
            workflow_session_info: None,
            workflow_attempts: Vec::new(),
            workflow_control: None,
            workflow_running: false,
            workflow_progress: None,
            workflow_unknown_review: None,
            pending_broadcast: None,
            broadcast_control: None,
            record_command: true,
            confirm_clear_history: false,
            loading: false,
            send_operation: SendOperationState::default(),
            command_send_cancellation: None,
            status: None,
            configuration_conflict: None,
            session_view_generation: 0,
            ui_session_binding: None,
            target_presentation: None,
            history_generation: 0,
            panel_generation: 0,
            suggestion_generation: 0,
            suggestion_cancellation: None,
            workflow_generation: 0,
        };
        let query_entity = view.snippet_query.clone();
        view.snippet_query_subscription = Some(cx.subscribe(
            &query_entity,
            |view, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let query = input.read(cx).value().to_string();
                    view.schedule_snippet_query(query, cx);
                }
            },
        ));
        view.load_snippet_page(None, cx);
        if let Some(subscription) = configuration_subscription {
            view.watch_configuration_changes(subscription, window, cx);
        }
        view
    }

    /// 注入 composition root 共享的只读 validator，不在面板复制配置解析规则。
    pub fn with_configuration_validation(
        mut self,
        service: ConfigurationValidationService,
    ) -> Self {
        self.configuration_validation = Some(service);
        self
    }

    /// 注入受限配置目录入口；操作系统只会收到用户明确的目录打开请求。
    pub fn with_configuration_directory(mut self, service: ConfigurationDirectoryService) -> Self {
        self.configuration_directory = Some(service);
        self
    }

    /// 注入应用层片段文件服务，保留对话框路径校验、大小上限和未知提交回读语义。
    pub fn with_snippet_transfer_files(mut self, service: SnippetTransferFileService) -> Self {
        self.snippet_transfer_files = Some(service);
        self
    }

    /// 注入共享终端会话上的片段执行用例，避免 UI 直接渲染后写入另一个 generation。
    pub fn with_snippet_execution_service(mut self, service: SnippetExecutionService) -> Self {
        self.snippet_execution = Some(Arc::new(service));
        self
    }

    /// 仅刷新外观令牌；主题切换不会清空当前输入、筛选或活动工作流。
    pub fn set_theme_mode(&mut self, mode: ThemeMode, cx: &mut Context<Self>) {
        self.theme_mode = mode;
        cx.notify();
    }

    /// 语言切换只改已创建输入实体的占位文字，保留搜索、命令、定义草稿与输入焦点。
    ///
    /// @author kongweiguang
    pub fn refresh_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.history_query.update(cx, |input, cx| {
            input.set_placeholder(
                crate::locale::text("搜索命令历史", "Search command history"),
                window,
                cx,
            )
        });
        self.command_input.update(cx, |input, cx| {
            input.set_placeholder(
                crate::locale::text("输入命令…", "Enter a command…"),
                window,
                cx,
            )
        });
        self.snippet_title.update(cx, |input, cx| {
            input.set_placeholder(crate::locale::text("片段名称", "Snippet name"), window, cx)
        });
        self.snippet_query.update(cx, |input, cx| {
            input.set_placeholder(
                crate::locale::text(
                    "搜索名称、命令、标签…",
                    "Search titles, commands, and tags…",
                ),
                window,
                cx,
            )
        });
        self.snippet_command.update(cx, |input, cx| {
            input.set_placeholder(
                crate::locale::text(
                    "命令模板；变量写作 {{name}}",
                    "Command template; variables use {{name}}",
                ),
                window,
                cx,
            )
        });
        self.workflow_title.update(cx, |input, cx| {
            input.set_placeholder(
                crate::locale::text("工作流名称", "Workflow name"),
                window,
                cx,
            )
        });
        self.snippet_description.update(cx, |input, cx| {
            input.set_placeholder(crate::locale::text("说明", "Description"), window, cx)
        });
        self.snippet_tags.update(cx, |input, cx| {
            input.set_placeholder(
                crate::locale::text("标签，逗号分隔", "Comma-separated tags"),
                window,
                cx,
            )
        });
        self.snippet_category.update(cx, |input, cx| {
            input.set_placeholder(crate::locale::text("分类", "Category"), window, cx)
        });
        self.snippet_sort_order.update(cx, |input, cx| {
            input.set_placeholder(crate::locale::text("排序", "Sort order"), window, cx)
        });

        for variable in &self.snippet_variables {
            variable.description.update(cx, |input, cx| {
                input.set_placeholder(
                    crate::locale::text("描述这个变量的用途", "Describe what this variable is for"),
                    window,
                    cx,
                )
            });
            variable.suggestions.update(cx, |input, cx| {
                input.set_placeholder(
                    crate::locale::text(
                        "例如：dev, staging, prod",
                        "For example: dev, staging, prod",
                    ),
                    window,
                    cx,
                )
            });
            variable.validation.update(cx, |input, cx| {
                input.set_placeholder(
                    crate::locale::text(
                        "只支持安全子集；保存时校验",
                        "Safe subset only; checked on save",
                    ),
                    window,
                    cx,
                )
            });
            variable.input.update(cx, |input, cx| {
                input.set_placeholder(
                    crate::locale::text("运行时变量值", "Runtime value"),
                    window,
                    cx,
                )
            });
        }

        for step in &self.workflow_steps {
            step.command.update(cx, |input, cx| {
                input.set_placeholder(
                    crate::locale::text(
                        "完整 shell 脚本；保留换行、引号和 heredoc",
                        "Full shell script; preserves newlines, quotes, and heredocs",
                    ),
                    window,
                    cx,
                )
            });
        }
        cx.notify();
    }

    /// 宿主按面板槽位设置紧凑布局；值未变化时不触发无效重绘。
    ///
    /// @author kongweiguang
    pub fn set_compact_layout(&mut self, compact: bool, cx: &mut Context<Self>) {
        if self.compact_layout == compact {
            return;
        }
        self.compact_layout = compact;
        cx.notify();
    }

    /// 编辑入口只切换可见层级，不创建新草稿或重载定义。
    ///
    /// @author kongweiguang
    pub(super) fn show_compact_detail(&mut self, section: CommandsSection, cx: &mut Context<Self>) {
        if !matches!(
            section,
            CommandsSection::Snippets | CommandsSection::Workflows
        ) || self.compact_detail == Some(section)
        {
            return;
        }
        self.compact_detail = Some(section);
        cx.notify();
    }

    /// 返回目录只隐藏编辑器，保留选中 revision、脏草稿和运行状态以便继续处理。
    ///
    /// @author kongweiguang
    pub(super) fn back_to_compact_list(&mut self, cx: &mut Context<Self>) {
        let transition_blocked = match self.compact_detail {
            Some(CommandsSection::Snippets) => {
                self.snippet_saving
                    || self.confirm_snippet_delete
                    || self.pending_snippet_transition.is_some()
            }
            Some(CommandsSection::Workflows) => {
                self.workflow_saving
                    || self.workflow_loading
                    || self.workflow_deleting
                    || self.workflow_run_blocks_editor()
                    || self.confirm_workflow_delete
                    || self.pending_workflow_transition.is_some()
            }
            Some(CommandsSection::History) | None => false,
        };
        if transition_blocked {
            self.status = Some(command_message(
                "请先完成当前编辑确认，再返回目录",
                "Finish the pending editor decision before returning to the list",
            ));
            cx.notify();
            return;
        }
        if self.compact_detail == Some(CommandsSection::Snippets) {
            self.set_snippet_editor_open(false, cx);
        }
        if self.compact_detail.take().is_some() {
            cx.notify();
        }
    }

    /// 通过单一切换点保留各 section 的草稿与版本快照，并回到新 section 的目录层。
    ///
    /// @author kongweiguang
    pub fn select_section(&mut self, section: CommandsSection, cx: &mut Context<Self>) {
        if self.section == section {
            return;
        }
        if (self.snippet_editor_open && section != CommandsSection::Snippets)
            || (self.section == CommandsSection::Snippets
                && section != CommandsSection::Snippets
                && self.snippet_run.blocks_navigation())
        {
            self.status = Some(command_message(
                if self.snippet_editor_open {
                    "请先关闭片段编辑器，再切换工作区"
                } else {
                    "请先完成或核对片段运行，再切换工作区"
                },
                if self.snippet_editor_open {
                    "Close the snippet editor before switching workspaces"
                } else {
                    "Finish or review the snippet run before switching workspaces"
                },
            ));
            cx.notify();
            return;
        }
        self.section = section;
        self.compact_detail = None;
        cx.notify();
    }

    /// 为工具快捷键提供稳定入口；片段编辑状态仍由同一个 CommandsView Entity 持有。
    pub fn select_snippets(&mut self, cx: &mut Context<Self>) {
        self.select_section(CommandsSection::Snippets, cx);
    }

    /// 工作流导航复用同一 CommandsView，保留原编辑草稿、revision 与运行确认状态。
    pub fn select_workflows(&mut self, cx: &mut Context<Self>) {
        self.select_section(CommandsSection::Workflows, cx);
    }

    /// Quick Open 按来源精确确认片段身份；同 ID 多来源时拒绝猜测并保留当前编辑器。
    ///
    /// @author kongweiguang
    pub fn open_snippet_by_id(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.select_snippets(cx);
        if self.section != CommandsSection::Snippets {
            return;
        }
        if self.snippet_saving || self.confirm_snippet_delete {
            self.status = Some(command_message(
                "片段编辑操作尚未完成，当前无法跳转",
                "Finish the current snippet edit operation before navigating",
            ));
            cx.notify();
            return;
        }

        self.snippet_generation = self.snippet_generation.wrapping_add(1).max(1);
        let generation = self.snippet_generation;
        let commands = self.commands.clone();
        let executor = self.executor.clone();
        self.status = Some(command_message(
            "正在确认片段仍然存在…",
            "Checking that the snippet still exists…",
        ));
        cx.spawn_in(window, async move |this, cx| {
            let result = executor
                .run(move |cancel| async move {
                    let builtin_key = SnippetPreferenceKey {
                        origin: SnippetCatalogOrigin::Builtin,
                        snippet_id: id.clone(),
                    };
                    let user_key = SnippetPreferenceKey {
                        origin: SnippetCatalogOrigin::User,
                        snippet_id: id.clone(),
                    };
                    let builtin = commands.get_snippet_catalog_item(builtin_key.clone()).await?;
                    if cancel.is_cancelled() {
                        return Err(AppError::cancelled(command_error_context(
                            "片段目录查询已取消",
                            "Snippet catalog lookup was cancelled",
                        )));
                    }
                    let user = commands.get_snippet_catalog_item(user_key.clone()).await?;
                    Ok(match (builtin, user) {
                        (Some(_), Some(_)) => Err(AppError::conflict(command_error_context(
                            "片段 ID 同时存在于多个来源，请从目录中选择来源",
                            "The snippet ID exists in multiple sources. Choose its source from the catalog",
                        ))),
                        (Some(_), None) => Ok(builtin_key),
                        (None, Some(_)) => Ok(user_key),
                        (None, None) => Err(AppError::not_found(command_error_context(
                            "该片段已不存在，请重新搜索",
                            "That snippet no longer exists; search again",
                        ))),
                    })
                })
                .await;
            let _ = cx.update(|window, app| {
                let _ = this.update(app, |view, cx| {
                    if view.snippet_generation != generation {
                        return;
                    }
                    match result {
                        Ok(Ok(key)) => view.open_snippet_key(key, window, cx),
                        Ok(Err(error)) | Err(error) => {
                            view.status = Some(error.public_message().to_owned());
                            cx.notify();
                        }
                    }
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// Quick Open 工作流入口继续复用 versioned loader 和保存/丢弃/取消切换决策。
    pub fn open_workflow_by_id(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.select_workflows(cx);
        self.open_workflow(id, window, cx);
    }

    /// 切换活动终端时取消旧发送并失效其 token；迟到回执不得释放新目标的忙状态。
    ///
    /// @author kongweiguang
    pub fn set_session(&mut self, session_id: Option<SessionId>, cx: &mut Context<Self>) {
        if self.session_id == session_id {
            return;
        }
        self.cancel_snippet_run(cx);
        snippet_browser::cancel_snippet_copy(self);
        self.clear_suggestions();
        let run_is_in_flight = self.workflow_running;
        let run_was_waiting = self
            .workflow_run
            .as_ref()
            .is_some_and(|run| run.state() == WorkflowRunState::AwaitingConfirmation);
        let active_send_kind = self.send_operation.active.map(|token| token.kind);
        let command_was_sending = active_send_kind == Some(SendOperationKind::Command);
        let broadcast_was_sending = active_send_kind == Some(SendOperationKind::Broadcast);
        if let Some(control) = self.workflow_control.as_ref() {
            control.cancel();
        }
        if let Some(control) = self.broadcast_control.take() {
            control.cancel();
        }
        self.cancel_terminal_fill();
        if let Some(cancellation) = self.command_send_cancellation.take() {
            cancellation.cancel();
        }
        self.send_operation.invalidate();
        self.session_view_generation = self.session_view_generation.wrapping_add(1).max(1);
        self.history_generation = self.history_generation.wrapping_add(1).max(1);
        if !run_is_in_flight {
            self.workflow_generation = self.workflow_generation.wrapping_add(1);
            if let Some(run) = self.workflow_run.as_mut()
                && run.state() == WorkflowRunState::AwaitingConfirmation
            {
                let _ = run.cancel();
                self.workflow_progress = None;
            }
            self.workflow_control = None;
        }
        self.session_id = session_id;
        self.ui_session_binding = None;
        snippet_applicability::refresh_selected(self, cx);
        self.target_presentation = None;
        self.session_info = None;
        self.history.clear();
        self.pending_broadcast = None;
        self.confirm_clear_history = false;
        self.status = if run_was_waiting {
            Some(command_message(
                "终端目标已切换，等待确认的工作流已取消",
                "Terminal target changed; the workflow awaiting confirmation was cancelled",
            ))
        } else if command_was_sending {
            Some(command_message(
                "终端目标已切换；旧命令结果可能未知，草稿已保留，请核对旧终端后再决定是否重发",
                "Terminal target changed; the old command result may be unknown. The draft was kept; check the old terminal before deciding whether to resend",
            ))
        } else if broadcast_was_sending {
            Some(command_message(
                "终端目标已切换，旧广播已请求取消；当前写入回执收敛后停止剩余目标",
                "Terminal target changed; the old broadcast was asked to stop after its current write receipt",
            ))
        } else if run_is_in_flight {
            Some(command_message(
                "正在取消旧终端上的工作流；等待终端回执后可编辑",
                "Cancelling the workflow on the previous terminal; editing resumes after its receipt",
            ))
        } else {
            None
        };
        self.reload_panel(cx);
    }

    /// Root 传入当前 pane binding；generation 变化时清名称并取消旧执行，但保留旧运行直到回执收敛。
    ///
    /// @author kongweiguang
    pub fn set_session_binding(
        &mut self,
        binding: Option<UiSessionBinding>,
        cx: &mut Context<Self>,
    ) {
        let binding = binding.filter(|binding| {
            self.session_id.as_ref() == Some(&binding.session_id)
                && matches!(&binding.owner, SessionOwner::Ui { .. })
        });
        if self.ui_session_binding == binding {
            return;
        }
        let command_was_sending = self
            .send_operation
            .active
            .is_some_and(|token| token.kind == SendOperationKind::Command);
        self.cancel_terminal_fill();
        if let Some(cancellation) = self.command_send_cancellation.take() {
            cancellation.cancel();
        }
        self.target_presentation = None;
        self.cancel_snippet_run(cx);
        snippet_browser::cancel_snippet_copy(self);
        self.clear_suggestions();
        self.ui_session_binding = binding;
        snippet_applicability::refresh_selected(self, cx);
        if self.workflow_running {
            if let Some(control) = self.workflow_control.as_ref() {
                control.cancel();
            }
        } else if let Some(run) = self
            .workflow_run
            .as_mut()
            .filter(|run| run.state() == WorkflowRunState::AwaitingConfirmation)
        {
            let _ = run.cancel();
            self.workflow_control = None;
            self.workflow_progress = None;
            self.status = Some(command_message(
                "目标会话已变化，待确认步骤已取消",
                "The target session changed; the waiting step was cancelled",
            ));
        }
        if command_was_sending {
            self.status = Some(command_message(
                "终端目标已变化，正在收敛旧命令写入结果；草稿会保留",
                "The terminal target changed; waiting for the old write result. The draft will be kept",
            ));
        }
        cx.notify();
    }

    /// 仅接受与当前 UI session generation 完全相同的展示投影，不把标签当成目标身份。
    ///
    /// @author kongweiguang
    pub(crate) fn set_target_presentation(
        &mut self,
        presentation: Option<CommandsTargetPresentation>,
        cx: &mut Context<Self>,
    ) {
        let presentation = presentation
            .filter(|presentation| self.ui_session_binding.as_ref() == Some(&presentation.binding));
        if self.target_presentation == presentation {
            return;
        }
        self.target_presentation = presentation;
        cx.notify();
    }

    /// 只向 Commands renderer 暴露仍与活动 generation 一致的目标显示名。
    ///
    /// @author kongweiguang
    pub(super) fn session_target_label(&self) -> Option<&str> {
        self.target_presentation
            .as_ref()?
            .label_for_binding(self.ui_session_binding.as_ref())
    }

    /// 在应用 runtime 并行读取当前会话、历史、文件化目录与可见分屏，不阻塞 GPUI 线程。
    ///
    /// @author kongweiguang
    pub(super) fn reload_panel(&mut self, cx: &mut Context<Self>) {
        self.refresh_snippet_page(cx);
        self.panel_generation = self.panel_generation.wrapping_add(1).max(1);
        let generation = self.panel_generation;
        let service = self.commands.clone();
        let executor = self.executor.clone();
        let session_id = self.session_id.clone();
        let query = self
            .history_query
            .update(cx, |input, _| input.value().to_string());
        self.loading = true;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    let session = match session_id.clone() {
                        Some(id) => Some(service.session_info(id).await),
                        None => None,
                    };
                    let request = history_query::HistoryScope::session(session_id.clone())
                        .list_request(query, None, 100);
                    let history = service.list_history(request).await;
                    let workflows = service.list_workflows(WorkflowListRequest::default()).await;
                    let ui_sessions = service.list_ui_sessions().await;
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled(command_error_context(
                            "命令面板读取已取消",
                            "Command panel loading was cancelled",
                        )));
                    }
                    Ok((session, history, workflows, ui_sessions))
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                view.finish_panel_load(generation, result, cx)
            });
        })
        .detach();
        cx.notify();
    }

    /// 只接受当前 generation 的数据，旧 session 或旧搜索的迟到结果不会覆盖新面板。
    fn finish_panel_load(
        &mut self,
        generation: u64,
        result: Result<PanelLoad, AppError>,
        cx: &mut Context<Self>,
    ) {
        if self.panel_generation != generation {
            return;
        }
        self.loading = false;
        match result {
            Ok((session, history, workflows, ui_sessions)) => {
                let mut errors = Vec::new();
                if let Some(session) = session {
                    match session {
                        Ok(session) => self.session_info = Some(session),
                        Err(error) => errors.push(error.public_message()),
                    }
                }
                match history {
                    Ok(history) => self.history = history,
                    Err(error) => errors.push(error.public_message()),
                }
                match workflows {
                    Ok(workflows) => self.workflows = workflows,
                    Err(error) => errors.push(error.public_message()),
                }
                match ui_sessions {
                    Ok(sessions) => self.ui_sessions = sessions,
                    Err(error) => errors.push(error.public_message()),
                }
                self.status = if errors.is_empty() {
                    self.configuration_conflict.clone()
                } else {
                    Some(errors.join("；"))
                };
            }
            Err(error) => self.status = Some(error.public_message().to_owned()),
        }
        cx.notify();
    }

    /// 用显式会话筛选重新读取历史；清理或删除后不会重新显示旧列表快照。
    ///
    /// @author kongweiguang
    pub(super) fn reload_history(&mut self, cx: &mut Context<Self>) {
        self.history_generation = self.history_generation.wrapping_add(1).max(1);
        let generation = self.history_generation;
        let service = self.commands.clone();
        let executor = self.executor.clone();
        let session_id = self.session_id.clone();
        let query = self
            .history_query
            .update(cx, |input, _| input.value().to_string());
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    let request = history_query::HistoryScope::session(session_id)
                        .list_request(query, None, 100);
                    let result = service.list_history(request).await?;
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled(command_error_context(
                            "命令历史查询已取消",
                            "History search was cancelled",
                        )));
                    }
                    Ok(result)
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.history_generation != generation {
                    return;
                }
                match result {
                    Ok(history) => {
                        view.history = history;
                        view.status = None;
                    }
                    Err(error) => view.status = Some(error.public_message().to_owned()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 历史项点击只把原文恢复到草稿，不会自动提交或改变当前活动会话。
    pub(super) fn use_history_command(
        &mut self,
        command: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_suggestions();
        self.command_input
            .update(cx, |input, cx| input.set_value(command, window, cx));
        self.record_command = true;
        self.select_section(CommandsSection::History, cx);
        self.status = Some(command_message(
            "历史命令已填入草稿，请检查后提交",
            "History command copied to the draft; review it before sending",
        ));
        cx.notify();
    }

    /// 删除单条历史后按当前筛选重新读取，界面不保留已删除的旧快照。
    pub(super) fn delete_history_entry(&mut self, entry_id: String, cx: &mut Context<Self>) {
        let service = self.commands.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move {
                    if cancel.is_cancelled() {
                        return Err(AppError::cancelled(command_error_context(
                            "命令历史删除已取消",
                            "History deletion was cancelled",
                        )));
                    }
                    service.delete_history(entry_id).await
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(true) => {
                        view.status =
                            Some(command_message("历史记录已删除", "History entry deleted"));
                        view.reload_history(cx);
                    }
                    Ok(false) => {
                        view.status = Some(command_message(
                            "历史记录已不存在",
                            "History entry no longer exists",
                        ));
                        view.reload_history(cx);
                    }
                    Err(error) => view.status = Some(error.public_message().to_owned()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 清理前由界面二次确认，并仅按当前明确 session 范围删除历史。
    ///
    /// @author kongweiguang
    pub(super) fn clear_history(&mut self, cx: &mut Context<Self>) {
        let service = self.commands.clone();
        let executor = self.executor.clone();
        let session_id = self.session_id.clone();
        self.confirm_clear_history = false;
        self.status = Some(command_message("正在清理历史…", "Clearing history…"));
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancel| async move {
                    if cancel.is_cancelled() {
                        return Err(AppError::cancelled(command_error_context(
                            "命令历史清理已取消",
                            "History cleanup was cancelled",
                        )));
                    }
                    let request = history_query::HistoryScope::session(session_id).clear_request();
                    service.clear_history(request).await
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(count) => {
                        view.status = Some(command_message(
                            format!("已清理 {count} 条历史记录"),
                            format!("Cleared {count} history entries"),
                        ));
                        view.reload_history(cx);
                    }
                    Err(error) => view.status = Some(error.public_message().to_owned()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 发送命令必须带当前 pane generation；回执只证明写入，不代表 shell 已完成。
    ///
    /// @author kongweiguang
    pub(super) fn send_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.send_operation.active.is_some() {
            return;
        }
        let Some(session_id) = self.session_id.clone() else {
            self.status = Some(command_message(
                "请先选择一个终端会话",
                "Select a terminal session first",
            ));
            cx.notify();
            return;
        };
        let Some(binding) = self
            .ui_session_binding
            .clone()
            .filter(|binding| binding.session_id == session_id)
        else {
            self.status = Some(command_message(
                "当前终端尚未建立可信目标绑定，未发送命令",
                "The terminal has no verified target binding; the command was not sent",
            ));
            cx.notify();
            return;
        };
        let command = self
            .command_input
            .update(cx, |input, _| input.value().to_string());
        if command.trim().is_empty() {
            self.status = Some(command_message("命令不能为空", "Command cannot be empty"));
            cx.notify();
            return;
        }
        self.clear_suggestions();
        let Some(send_operation_token) = self.send_operation.begin(SendOperationKind::Command)
        else {
            return;
        };
        self.status = Some(command_message("正在发送…", "Sending…"));
        let session_generation = self.session_view_generation;
        let submitted_command = command.clone();
        let callback_session_id = session_id;
        let callback_binding = binding.clone();
        let service = self.commands.clone();
        let executor = self.executor.clone();
        let record = self.record_command;
        let command_send_cancellation = CancellationToken::new();
        self.command_send_cancellation = Some(command_send_cancellation.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = executor
                .run_cancellable(command_send_cancellation, move |cancellation| async move {
                    service
                        .send_ui_command(binding, command, record, cancellation)
                        .await
                })
                .await;
            let _ = cx.update(|window, app| {
                let _ = this.update(app, |view, cx| {
                    view.finish_send(
                        CommandSubmissionSnapshot {
                            session_id: callback_session_id,
                            binding: callback_binding,
                            session_generation,
                            operation_token: send_operation_token,
                            command: submitted_command,
                        },
                        result,
                        window,
                        cx,
                    );
                });
            });
        })
        .detach();
    }

    /// 按提交快照核对完整所有权后才消费回执；历史故障不反转写入事实，目标变化不清除新草稿。
    ///
    /// @author kongweiguang
    fn finish_send(
        &mut self,
        submitted: CommandSubmissionSnapshot,
        result: Result<CommandExecutionReceipt, AppError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let CommandSubmissionSnapshot {
            session_id: submitted_session_id,
            binding: submitted_binding,
            session_generation,
            operation_token: send_operation_token,
            command: submitted_command,
        } = submitted;
        if !self.send_operation.finish(send_operation_token)
            || self.session_view_generation != session_generation
            || self.session_id.as_ref() != Some(&submitted_session_id)
        {
            return;
        }
        self.command_send_cancellation = None;
        if self.ui_session_binding.as_ref() != Some(&submitted_binding) {
            self.status = Some(match result {
                Ok(receipt) if receipt.sent => command_message(
                    "命令已提交到切换前的终端；草稿已保留，退出状态未跟踪",
                    "Command submitted to the previous terminal; the draft was kept and exit status is untracked",
                ),
                Ok(_) => command_message(
                    "切换前的终端未接受命令；草稿已保留",
                    "The previous terminal did not accept the command; the draft was kept",
                ),
                Err(error)
                    if error.public_code()
                        == kerminal_application::PublicErrorCode::OutcomeUnknown =>
                {
                    command_message(
                        "目标已变化，旧终端的写入结果未确认；草稿已保留，请检查后再决定是否重发",
                        "The target changed and the old terminal write is unconfirmed; the draft was kept. Check the old terminal before deciding whether to resend",
                    )
                }
                Err(_) => command_message(
                    "切换前的终端未接受命令；草稿已保留，未自动重发",
                    "The previous terminal rejected the command; the draft was kept and not resent",
                ),
            });
            cx.notify();
            return;
        }
        match result {
            Ok(receipt) if receipt.sent => {
                let draft_unchanged = self.command_input.update(cx, |input, _| {
                    input.value().as_ref() == submitted_command.as_str()
                });
                if draft_unchanged {
                    self.command_input
                        .update(cx, |input, cx| input.set_value("", window, cx));
                }
                self.status = Some(if let Some(reason) = receipt.history_skip_reason {
                    let reason = match reason.as_str() {
                        "命令包含疑似凭据内容，已跳过历史记录" => {
                            command_message(
                                "命令包含疑似凭据内容，已跳过历史记录",
                                "History skipped because the command may contain credentials",
                            )
                        }
                        _ => reason,
                    };
                    command_message(
                        format!("命令已提交；{reason}。终端退出状态未跟踪"),
                        format!("Command submitted; {reason}. Exit status is not tracked"),
                    )
                } else if receipt.history_error_code.is_some() {
                    command_message(
                        "命令已提交，但历史写入失败。终端退出状态未跟踪",
                        "Command submitted, but history could not be saved. Exit status is not tracked",
                    )
                } else {
                    command_message(
                        if draft_unchanged {
                            "命令已提交；终端退出状态未跟踪"
                        } else {
                            "命令已提交；已保留发送期间新编辑的草稿"
                        },
                        if draft_unchanged {
                            "Command submitted; exit status is not tracked"
                        } else {
                            "Command submitted; the draft edited while sending was preserved"
                        },
                    )
                });
                self.reload_history(cx);
            }
            Ok(_) => {
                self.status = Some(command_message(
                    "终端没有接受命令输入",
                    "Terminal did not accept command input",
                ))
            }
            Err(error) => self.status = Some(error.public_message().to_owned()),
        }
        cx.notify();
    }

    /// 生成当前文本的历史和片段建议；危险或敏感候选由应用层策略过滤展示位置。
    pub(super) fn load_suggestions(&mut self, cx: &mut Context<Self>) {
        self.clear_suggestions();
        let Some(session_id) = self.session_id.clone() else {
            self.status = Some(command_message(
                "建议需要明确的活动终端会话",
                "Suggestions need an explicit active terminal session",
            ));
            cx.notify();
            return;
        };
        let input = self
            .command_input
            .update(cx, |input, _| input.value().to_string());
        let generation = self.suggestion_generation;
        let request_input = input.clone();
        let service_session_id = session_id.clone();
        let callback_session_id = session_id.clone();
        let cancellation = CancellationToken::new();
        self.suggestion_cancellation = Some(cancellation.clone());
        let service = self.commands.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |cancellation| async move {
                    let info = service.session_info(service_session_id).await?;
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled(command_error_context(
                            "命令建议查询已取消",
                            "Command suggestions were cancelled",
                        )));
                    }
                    let request = suggestion_request(&info, input, generation);
                    let suggestions = service.suggestions(request, cancellation.clone()).await?;
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled(command_error_context(
                            "命令建议查询已取消",
                            "Command suggestions were cancelled",
                        )));
                    }
                    Ok(suggestions)
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.suggestion_generation != generation
                    || view.session_id.as_ref() != Some(&callback_session_id)
                {
                    return;
                }
                view.suggestion_cancellation = None;
                let draft_matches = view.command_input.update(cx, |input, _| {
                    input.value().as_ref() == request_input.as_str()
                });
                if !draft_matches {
                    return;
                }
                match result {
                    Ok(items) => {
                        view.suggestions = items;
                        view.status = None;
                    }
                    Err(error) => view.status = Some(error.public_message().to_owned()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 清除建议时同时取消 service 查询并推进代次，防止迟到结果重新填充旧目标候选。
    fn clear_suggestions(&mut self) {
        cancel_owned_suggestion_request(&mut self.suggestion_cancellation);
        self.suggestion_generation = self.suggestion_generation.wrapping_add(1).max(1);
        self.suggestions.clear();
    }

    /// 捕获 Fill 请求仍需匹配的目标、定义、弹层和输入值，供异步回执拒绝过期结果。
    ///
    /// @author kongweiguang
    fn snippet_fill_guard(
        &mut self,
        request_generation: u64,
        definition_draft: SnippetDraftValues,
        cx: &mut Context<Self>,
    ) -> SnippetFillGuard {
        SnippetFillGuard {
            request_generation,
            session_view_generation: self.session_view_generation,
            session_id: self.session_id.clone(),
            binding: self.ui_session_binding.clone(),
            section: self.section,
            snippet_generation: self.snippet_generation,
            editor_generation: self.snippet_editor_generation,
            editor_open: self.snippet_editor_open,
            selected_sensitive: self.selected_snippet_sensitive
                || self.snippet_variables.iter().any(|field| {
                    field.variable.sensitive || field.variable.kind == SnippetVariableKind::Secret
                }),
            command_draft: self
                .command_input
                .update(cx, |input, _| input.value().to_string()),
            definition_draft,
            variable_values: self
                .snippet_variables
                .iter()
                .map(|field| {
                    (
                        field.name.update(cx, |input, _| input.value().to_string()),
                        field.input.update(cx, |input, _| input.value().to_string()),
                    )
                })
                .collect(),
        }
    }

    /// 右栏写入精确绑定终端且不回车；宽工作台保留命令编辑草稿，敏感内容关闭历史记录。
    /// 成功仅隐藏同一弹层并切到历史，保留定义/变量草稿和既有运行保护。
    ///
    /// @author kongweiguang
    pub(super) fn fill_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.compact_layout {
            self.fill_snippet_terminal(window, cx);
            return;
        }
        if self.snippet_run.blocks_navigation() {
            self.status = Some(command_message(
                "请先完成或核对片段运行，再填入命令草稿",
                "Finish or review the snippet run before filling a command draft",
            ));
            cx.notify();
            return;
        }
        if self.snippet_saving
            || self.confirm_snippet_delete
            || self.pending_snippet_transition.is_some()
        {
            self.status = Some(command_message(
                "请先完成当前片段保存、删除或切换确认",
                "Finish the pending snippet save, deletion, or transition first",
            ));
            cx.notify();
            return;
        }
        let Some(snippet) = self.selected_snippet.clone() else {
            self.status = Some(command_message(
                "请先选择一个用户片段",
                "Select a user snippet first",
            ));
            cx.notify();
            return;
        };
        let Some(session_id) = self.session_id.clone() else {
            self.status = Some(command_message(
                "命令片段需要明确的活动终端会话",
                "A snippet needs an explicit active terminal session",
            ));
            cx.notify();
            return;
        };
        let definition = snippet.definition.clone();
        let user_definition = snippet.revision.is_some();
        let snippet_id = definition.id.clone();
        let definition_draft = match self.collect_snippet_draft_values(cx) {
            Ok(draft) => draft,
            Err(message) => {
                self.status = Some(message);
                cx.notify();
                return;
            }
        };
        if !configuration_watch::snippet_definition_draft_matches_saved(
            &definition,
            &definition_draft,
        ) {
            self.status = Some(command_message(
                "请先保存片段定义，再填入命令草稿；运行变量会保留",
                "Save the snippet definition before filling; runtime variable values are preserved",
            ));
            cx.notify();
            return;
        }
        self.snippet_fill_generation = self.snippet_fill_generation.wrapping_add(1).max(1);
        let request_guard =
            self.snippet_fill_guard(self.snippet_fill_generation, definition_draft, cx);
        let selected_sensitive = request_guard.selected_sensitive;
        let values = request_guard.variable_values.clone();
        let service = self.commands.clone();
        let executor = self.executor.clone();
        let callback_guard = request_guard;
        cx.spawn_in(window, async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    let rendered = if user_definition {
                        service
                            .render_snippet_for_session(snippet_id, session_id, values)
                            .await?
                    } else {
                        service
                            .render_definition_for_session(definition, session_id, values)
                            .await?
                    };
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled(command_error_context(
                            "命令片段渲染已取消",
                            "Snippet rendering was cancelled",
                        )));
                    }
                    let mut rendered = rendered;
                    rendered.sensitive |= selected_sensitive;
                    Ok(rendered)
                })
                .await;
            let _ = cx.update(|window, app| {
                let _ = this.update(app, |view, cx| {
                    if view.snippet_fill_generation != callback_guard.request_generation {
                        return;
                    }
                    let mut current_guard = view.snippet_fill_guard(
                        callback_guard.request_generation,
                        callback_guard.definition_draft.clone(),
                        cx,
                    );
                    if !callback_guard.same_request_context(&current_guard) {
                        return;
                    }
                    current_guard.definition_draft =
                        match view.collect_snippet_draft_values(cx) {
                            Ok(draft) => draft,
                            Err(_) => {
                                view.status = Some(command_message(
                                    "填入期间片段定义已变化，已保留当前编辑；请检查后重新填入",
                                    "The snippet definition changed while filling; your edits were preserved. Review and fill again",
                                ));
                                cx.notify();
                                return;
                            }
                        };
                    if !callback_guard.drafts_unchanged(&current_guard) {
                        view.status = Some(command_message(
                            "填入期间命令或变量草稿已修改，已保留当前输入，请重新填入",
                            "The command or variable draft changed while filling; it was preserved. Fill again to use the new values",
                        ));
                        cx.notify();
                        return;
                    }
                    if view.snippet_run.blocks_navigation()
                        || view.snippet_saving
                        || view.confirm_snippet_delete
                        || view.pending_snippet_transition.is_some()
                    {
                        return;
                    }
                    match result {
                        Ok(rendered) => {
                            if callback_guard.editor_open {
                                view.close_snippet_editor_for_fill(cx);
                                if view.snippet_editor_open {
                                    return;
                                }
                            }
                            view.clear_suggestions();
                            view.command_input.update(cx, |input, cx| {
                                input.set_value(rendered.command, window, cx)
                            });
                            if rendered.sensitive {
                                view.record_command = false;
                                view.status = Some(command_message(
                                    "敏感片段已填入命令框，本次发送不会写入历史",
                                    "Sensitive snippet copied to the command field; this send will not be recorded",
                                ));
                            } else {
                                view.record_command = true;
                                view.status = Some(command_message(
                                    "片段已填入命令框，请检查后提交",
                                    "Snippet copied to the command field; review it before sending",
                                ));
                            }
                            view.select_section(CommandsSection::History, cx);
                            if !callback_guard.editor_open {
                                let focus_at_schedule = window.focused(cx);
                                let commands_view = cx.entity();
                                window.on_next_frame(move |window, app| {
                                    if window.has_active_prompt()
                                        || window.focused(app) != focus_at_schedule
                                    {
                                        return;
                                    }
                                    commands_view.update(app, |view, cx| {
                                        view.focus_command_draft(window, cx);
                                    });
                                });
                            }
                            cx.notify();
                        }
                        Err(error) => {
                            view.status = Some(error.public_message().to_owned());
                            cx.notify();
                        }
                    }
                });
            });
        })
        .detach();
    }

    /// 兼容旧调用方的 ID 入口；来源必须重新精确查询，不能从当前分页猜测。
    ///
    /// @author kongweiguang
    pub(super) fn open_snippet(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.open_snippet_by_id(id, window, cx);
    }

    /// 新增最小安全文本变量；敏感值仅留在运行输入，不能变成定义默认值或建议。
    ///
    /// @author kongweiguang
    pub(super) fn add_snippet_variable(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut index = self.snippet_variables.len() + 1;
        let existing = self
            .snippet_variables
            .iter()
            .map(|field| field.name.update(cx, |input, _| input.value().to_string()))
            .collect::<std::collections::HashSet<_>>();
        while existing.contains(&format!("variable_{index}")) {
            index += 1;
        }
        let name = format!("variable_{index}");
        let label = command_message(format!("变量 {index}"), format!("Variable {index}"));
        let variable_id = self.allocate_snippet_variable_id();
        self.snippet_variables.push(new_snippet_variable_input(
            variable_id,
            SnippetVariable {
                name: name.clone(),
                label: label.clone(),
                description: String::new(),
                kind: SnippetVariableKind::Text,
                required: false,
                default_value: None,
                suggestions: Vec::new(),
                validation: None,
                render_strategy: SnippetVariableRenderStrategy::ShellArg,
                sensitive: false,
            },
            window,
            cx,
        ));
        cx.notify();
    }

    /// 变量下拉回调以稳定身份定位条目；列表删除或重建不会把旧事件转给邻项。
    ///
    /// @author kongweiguang
    fn allocate_snippet_variable_id(&mut self) -> u64 {
        self.snippet_variable_id = self.snippet_variable_id.wrapping_add(1).max(1);
        self.snippet_variable_id
    }

    /// 删除变量会同步移除其输入状态，未保存草稿不会遗留隐藏的敏感内容。
    pub(super) fn remove_snippet_variable(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.snippet_variables.len() {
            self.snippet_variables.remove(index);
            cx.notify();
        }
    }

    /// 在普通文本与 Secret 之间显式切换；转为敏感后立即清掉可持久化默认项。
    ///
    /// @author kongweiguang
    pub(super) fn toggle_snippet_variable_sensitive(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((variable_id, next_kind)) = self.snippet_variables.get(index).map(|field| {
            (
                field.id,
                if field.variable.kind == SnippetVariableKind::Secret || field.variable.sensitive {
                    SnippetVariableKind::Text
                } else {
                    SnippetVariableKind::Secret
                },
            )
        }) else {
            return;
        };
        let kind_select = self.snippet_variables[index].kind_select.clone();
        self.set_snippet_variable_kind(variable_id, next_kind, window, cx);
        kind_select.update(cx, |select, cx| {
            select.set_selected_value(&next_kind, window, cx);
        });
    }

    /// 必填状态是定义元数据，切换后重新校验缺失值而不自动填入终端。
    pub(super) fn toggle_snippet_variable_required(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(field) = self.snippet_variables.get_mut(index) else {
            return;
        };
        field.variable.required = !field.variable.required;
        cx.notify();
    }

    /// 类型改变只更新当前草稿；Secret 清除持久化默认项、建议和旧敏感运行值。
    ///
    /// @author kongweiguang
    pub(super) fn set_snippet_variable_kind(
        &mut self,
        variable_id: u64,
        kind: SnippetVariableKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self
            .snippet_variables
            .iter()
            .position(|field| field.id == variable_id)
        else {
            return;
        };
        let (strategy_select, strategy, was_sensitive, runtime_input) = {
            let field = &mut self.snippet_variables[index];
            let was_sensitive =
                field.variable.sensitive || field.variable.kind == SnippetVariableKind::Secret;
            field.variable = transition_snippet_variable_kind(field.variable.clone(), kind);
            (
                field.render_strategy_select.clone(),
                field.variable.render_strategy,
                was_sensitive,
                field.input.clone(),
            )
        };
        if kind == SnippetVariableKind::Secret {
            let field = &mut self.snippet_variables[index];
            field
                .default_value
                .update(cx, |input, cx| input.set_value("", window, cx));
            field
                .suggestions
                .update(cx, |input, cx| input.set_value("", window, cx));
        } else if was_sensitive {
            runtime_input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        strategy_select.update(cx, |select, cx| {
            select.set_items(snippet_variable_render_strategy_options(kind), window, cx);
            select.set_selected_value(&strategy, window, cx);
        });
        cx.notify();
    }

    /// 防御性验证下拉事件，不能让不支持的类型保存为原始命令插值。
    ///
    /// @author kongweiguang
    pub(super) fn set_snippet_variable_render_strategy(
        &mut self,
        variable_id: u64,
        strategy: SnippetVariableRenderStrategy,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self
            .snippet_variables
            .iter()
            .position(|field| field.id == variable_id)
        else {
            return;
        };
        let field = &mut self.snippet_variables[index];
        if !supports_validated_raw(field.variable.kind)
            && strategy == SnippetVariableRenderStrategy::ValidatedRaw
        {
            return;
        }
        field.variable.render_strategy = strategy;
        cx.notify();
    }

    /// 高级元数据使用同一版本化定义持久化，不让内置片段进入用户写路径。
    pub(super) fn toggle_snippet_advanced(&mut self, cx: &mut Context<Self>) {
        self.snippet_advanced_open = !self.snippet_advanced_open;
        cx.notify();
    }

    /// 上下文绑定增加一个可编辑项；非全局项保存前必须提供真实目标 ID。
    pub(super) fn add_snippet_context_binding(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.snippet_context_bindings
            .push(new_context_binding_input(
                SnippetContextBinding {
                    kind: SnippetContextBindingKind::Workspace,
                    target_id: Some(String::new()),
                },
                window,
                cx,
            ));
        cx.notify();
    }

    /// 删除绑定项不触及其它绑定；允许零绑定表示目录默认范围。
    pub(super) fn remove_snippet_context_binding(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.snippet_context_bindings.len() {
            self.snippet_context_bindings.remove(index);
            cx.notify();
        }
    }

    /// 循环上下文类型时保留目标文本，切到 Global 才清掉无意义的 targetId。
    pub(super) fn cycle_snippet_context_binding(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(binding) = self.snippet_context_bindings.get_mut(index) else {
            return;
        };
        binding.binding.kind = match binding.binding.kind {
            SnippetContextBindingKind::Global => SnippetContextBindingKind::Workspace,
            SnippetContextBindingKind::Workspace => SnippetContextBindingKind::Host,
            SnippetContextBindingKind::Host => SnippetContextBindingKind::HostGroup,
            SnippetContextBindingKind::HostGroup => SnippetContextBindingKind::Global,
        };
        if binding.binding.kind == SnippetContextBindingKind::Global {
            binding.binding.target_id = None;
        }
        cx.notify();
    }

    /// 统一规范化表单里的定义字段，保证 dirty 判断与版本化保存针对同一值比较。
    pub(super) fn collect_snippet_draft_values(
        &self,
        cx: &mut Context<Self>,
    ) -> Result<SnippetDraftValues, String> {
        let title = self
            .snippet_title
            .update(cx, |input, _| input.value().to_string())
            .trim()
            .to_owned();
        let command = self
            .snippet_command
            .update(cx, |input, _| input.text().to_string())
            .trim()
            .to_owned();
        let description = self
            .snippet_description
            .update(cx, |input, _| input.value().to_string());
        let description = (!description.trim().is_empty()).then(|| description.trim().to_owned());
        let tags: Vec<String> = self
            .snippet_tags
            .update(cx, |input, _| input.value().to_string())
            .split([',', '，', '\n'])
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
            .map(str::to_owned)
            .fold(Vec::new(), |mut tags, tag| {
                if !tags.contains(&tag) {
                    tags.push(tag);
                }
                tags
            });
        let sort_order_text = self
            .snippet_sort_order
            .update(cx, |input, _| input.value().to_string());
        let sort_order = sort_order_text
            .trim()
            .parse::<i64>()
            .map_err(|_| command_message("排序必须是整数", "Sort order must be an integer"))?;
        if title.is_empty() {
            return Err(command_message(
                "片段标题不能为空",
                "Snippet title cannot be empty",
            ));
        }
        if command.is_empty() {
            return Err(command_message(
                "命令模板不能为空",
                "Snippet command cannot be empty",
            ));
        }
        if title.chars().count() > 80 || command.chars().count() > 8_000 {
            return Err(command_message(
                "标题最多 80 个字符，命令模板最多 8,000 个字符",
                "Title is limited to 80 characters and command template to 8,000",
            ));
        }
        if tags.len() > 12 || tags.iter().any(|tag| tag.chars().count() > 32) {
            return Err(command_message(
                "最多 12 个标签，每个标签最多 32 个字符",
                "Use at most 12 tags, each limited to 32 characters",
            ));
        }
        let variables = self
            .snippet_variables
            .iter()
            .map(|field| {
                let mut variable = field.variable.clone();
                variable.name = field
                    .name
                    .update(cx, |input, _| input.value().trim().to_owned());
                variable.label = field
                    .label
                    .update(cx, |input, _| input.value().trim().to_owned());
                variable.description = field
                    .description
                    .update(cx, |input, _| input.value().to_string());
                let default_value = field
                    .default_value
                    .update(cx, |input, _| input.value().to_string());
                variable.default_value = (!default_value.is_empty()).then_some(default_value);
                variable.suggestions = field
                    .suggestions
                    .update(cx, |input, _| input.value().to_string())
                    .split([',', '，', '\n'])
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect();
                let validation = field
                    .validation
                    .update(cx, |input, _| input.value().to_string());
                variable.validation =
                    (!validation.trim().is_empty()).then(|| validation.trim().to_owned());
                if variable.sensitive || variable.kind == SnippetVariableKind::Secret {
                    variable.sensitive = true;
                    variable.kind = SnippetVariableKind::Secret;
                    variable.default_value = None;
                    variable.suggestions.clear();
                }
                variable
            })
            .collect();
        let mut context_bindings = Vec::with_capacity(self.snippet_context_bindings.len());
        for item in &self.snippet_context_bindings {
            let mut binding = item.binding.clone();
            let target_id = item
                .target_id
                .update(cx, |input, _| input.value().trim().to_owned());
            binding.target_id = if binding.kind == SnippetContextBindingKind::Global {
                None
            } else if target_id.is_empty() {
                return Err(command_message(
                    "工作区、主机和主机组绑定都需要目标 ID",
                    "Workspace, host, and host-group bindings need a target ID",
                ));
            } else {
                Some(target_id)
            };
            context_bindings.push(binding);
        }
        let category = self
            .snippet_category
            .update(cx, |input, _| input.value().trim().to_owned());
        Ok(SnippetDraftValues {
            title,
            command,
            description,
            tags,
            sort_order,
            scope: self.snippet_scope,
            category: (!category.is_empty()).then_some(category),
            risk: self.snippet_risk,
            default_action: self.snippet_default_action,
            variables,
            context_bindings,
        })
    }

    /// 请求创建空片段并进入详情；脏稿门禁重新显示编辑器，避免隐藏弹层后看不到保留决策。
    ///
    /// @author kongweiguang
    pub(super) fn new_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.snippet_saving || self.confirm_snippet_delete {
            return;
        }
        snippet_browser::cancel_snippet_copy(self);
        if self.snippet_is_new && self.selected_snippet.is_none() {
            self.set_snippet_editor_open(true, cx);
            self.show_compact_detail(CommandsSection::Snippets, cx);
            return;
        }
        self.show_compact_detail(CommandsSection::Snippets, cx);
        if self.snippet_draft_is_dirty(cx) {
            self.pending_snippet_transition = Some(EditorTransition::New);
            self.set_snippet_editor_open(true, cx);
            self.status = Some(command_message(
                "片段草稿尚未保存，请选择保存、丢弃或取消切换",
                "The snippet draft has unsaved changes. Save, discard, or cancel the switch",
            ));
            cx.notify();
            return;
        }
        self.install_new_snippet(window, cx);
    }

    /// 只有过渡决策已确认后才重置状态，避免把用户输入当作可丢弃的临时视图。
    fn install_new_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.snippet_generation = self.snippet_generation.wrapping_add(1).max(1);
        self.snippet_inline_preview = snippet_browser::SnippetInlinePreview::Collapsed;
        snippet_applicability::clear(self);
        self.selected_snippet = None;
        self.snippet_advanced_open = false;
        self.configuration_conflict = None;
        self.selected_snippet_sensitive = false;
        self.snippet_variables.clear();
        self.snippet_context_bindings.clear();
        self.snippet_context_bindings
            .push(new_context_binding_input(
                SnippetContextBinding {
                    kind: SnippetContextBindingKind::Global,
                    target_id: None,
                },
                window,
                cx,
            ));
        self.snippet_is_new = true;
        self.set_snippet_editor_open(true, cx);
        self.snippet_scope = SnippetScope::Any;
        self.snippet_risk = SnippetRisk::Unknown;
        self.snippet_default_action = SnippetDefaultAction::Insert;
        self.snippet_description
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.snippet_tags
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.snippet_category
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.snippet_sort_order
            .update(cx, |input, cx| input.set_value("0", window, cx));
        self.snippet_title
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.snippet_command
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.status = Some(command_message(
            "新建片段：命令变量使用 {{name}} 语法",
            "New snippet: reference variables with {{name}}",
        ));
        cx.notify();
    }

    /// 继续暂存的片段切换；删除动作在恢复已保存 revision 后转为第二次明确确认。
    pub(super) fn discard_snippet_transition(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(transition) = self.pending_snippet_transition.take() else {
            return;
        };
        match transition {
            EditorTransition::New => self.install_new_snippet(window, cx),
            EditorTransition::Open(id) => self.open_snippet_by_id(id, window, cx),
            EditorTransition::OpenKey(key) => self.load_snippet_key(key, true, false, window, cx),
            EditorTransition::PreviewKey(key) => {
                self.set_snippet_editor_open(false, cx);
                self.snippet_inline_preview =
                    snippet_browser::SnippetInlinePreview::Loading(key.clone());
                self.load_snippet_key(key, false, true, window, cx);
            }
            EditorTransition::Delete => {
                if let Some(saved) = self.selected_snippet.clone() {
                    self.install_snippet(saved, window, cx);
                    self.confirm_snippet_delete = true;
                    self.status = Some(command_message(
                        "草稿已丢弃；请再次确认删除保存的片段",
                        "Draft discarded. Confirm deletion of the saved snippet",
                    ));
                }
                cx.notify();
            }
        }
    }

    /// 取消过渡时只清除 intent，当前定义、revision 和运行时变量输入均保留。
    pub(super) fn cancel_snippet_transition(&mut self, cx: &mut Context<Self>) {
        self.pending_snippet_transition = None;
        self.status = None;
        cx.notify();
    }

    /// 把保存/丢弃之后的意图继续执行，删除必须再经过独立确认按钮。
    fn apply_snippet_transition(
        &mut self,
        transition: EditorTransition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match transition {
            EditorTransition::New => self.install_new_snippet(window, cx),
            EditorTransition::Open(id) => self.open_snippet_by_id(id, window, cx),
            EditorTransition::OpenKey(key) => self.load_snippet_key(key, true, false, window, cx),
            EditorTransition::PreviewKey(key) => {
                self.set_snippet_editor_open(false, cx);
                self.snippet_inline_preview =
                    snippet_browser::SnippetInlinePreview::Loading(key.clone());
                self.load_snippet_key(key, false, true, window, cx);
            }
            EditorTransition::Delete => {
                self.confirm_snippet_delete = true;
                self.status = Some(command_message(
                    "保存完成；再次确认后删除片段",
                    "Saved. Confirm once more to delete the snippet",
                ));
                cx.notify();
            }
        }
    }

    /// 按打开时的 revision 保存片段；冲突结果保留本地草稿并提示重新加载。
    pub(super) fn save_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.snippet_saving {
            return;
        }
        let values = match self.collect_snippet_draft_values(cx) {
            Ok(values) => values,
            Err(message) => {
                self.status = Some(message);
                cx.notify();
                return;
            }
        };
        let existing = self.selected_snippet.clone();
        if existing
            .as_ref()
            .is_some_and(|saved| saved.revision.is_none())
        {
            self.status = Some(command_message(
                "内置片段只读，不能保存覆盖",
                "Built-in snippets are read-only",
            ));
            cx.notify();
            return;
        }
        let (definition, revision) = match existing {
            Some(current) => {
                let mut definition = current.definition;
                definition.title = values.title;
                definition.command = values.command;
                definition.description = values.description;
                definition.tags = values.tags;
                definition.sort_order = values.sort_order;
                definition.scope = values.scope;
                definition.category = values.category;
                definition.risk = values.risk;
                definition.default_action = values.default_action;
                definition.variables = values.variables;
                definition.context_bindings = values.context_bindings;
                (definition, current.revision)
            }
            None => (
                SnippetDefinition {
                    id: self.commands.new_definition_id(),
                    title: values.title,
                    command: values.command,
                    description: values.description,
                    tags: values.tags,
                    sort_order: values.sort_order,
                    scope: values.scope,
                    category: values.category,
                    risk: values.risk,
                    default_action: values.default_action,
                    variables: values.variables,
                    context_bindings: values.context_bindings,
                    derived_from: None,
                },
                None,
            ),
        };
        let service = self.commands.clone();
        let executor = self.executor.clone();
        self.snippet_generation = self.snippet_generation.wrapping_add(1).max(1);
        let generation = self.snippet_generation;
        self.snippet_saving = true;
        self.status = Some(command_message("正在保存片段…", "Saving snippet…"));
        cx.spawn_in(window, async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    let saved = service.save_snippet_versioned(definition, revision).await?;
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled(command_error_context(
                            "命令片段保存已取消",
                            "Snippet save was cancelled",
                        )));
                    }
                    Ok(saved)
                })
                .await;
            let _ = cx.update(|window, app| {
                let _ = this.update(app, |view, cx| {
                    view.finish_snippet_save(generation, result, window, cx);
                });
            });
        })
        .detach();
    }

    /// 接受已保存定义并回显版本化草稿，同时重建变量输入而不写回默认值。
    fn finish_snippet_save(
        &mut self,
        generation: u64,
        result: Result<VersionedCommandDefinition<SnippetDefinition>, AppError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.snippet_generation != generation {
            return;
        }
        self.snippet_saving = false;
        match result {
            Ok(saved) => {
                let definition = saved.definition.clone();
                self.install_snippet(saved, window, cx);
                self.snippet_is_new = false;
                self.status = Some(command_message(
                    format!("已保存片段：{}", definition.title),
                    format!("Saved snippet: {}", definition.title),
                ));
                self.reload_panel(cx);
                if let Some(transition) = self.pending_snippet_transition.take() {
                    self.apply_snippet_transition(transition, window, cx);
                }
            }
            Err(error) => self.status = Some(error.public_message().to_owned()),
        }
        cx.notify();
    }

    /// 删除前再次使用读取时 revision 做 CAS；内置项没有版本化用户定义，不能进入删除路径。
    pub(super) fn delete_snippet(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.snippet_saving || self.confirm_snippet_delete {
            return;
        }
        if self.snippet_draft_is_dirty(cx) {
            self.pending_snippet_transition = Some(EditorTransition::Delete);
            self.status = Some(command_message(
                "片段草稿尚未保存，请先选择保存、丢弃或取消删除",
                "The snippet draft has unsaved changes. Save, discard, or cancel deletion",
            ));
            cx.notify();
            return;
        }
        self.confirm_snippet_delete = true;
        self.status = Some(command_message(
            "请确认删除此片段；删除使用当前 revision，不会删除内置内容",
            "Confirm deletion. The current revision is checked and built-in content is never removed",
        ));
        cx.notify();
    }

    /// 只有用户在确认条再次选择删除后才调用 CAS 删除端口。
    pub(super) fn confirm_delete_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.confirm_snippet_delete || self.snippet_saving {
            return;
        }
        let Some(current) = self.selected_snippet.clone() else {
            self.status = Some(command_message(
                "请选择可编辑的用户片段",
                "Select an editable user snippet",
            ));
            cx.notify();
            return;
        };
        let Some(revision) = current.revision else {
            self.status = Some(command_message(
                "片段缺少 revision，已阻止不安全删除",
                "Snippet has no revision; unsafe deletion was blocked",
            ));
            cx.notify();
            return;
        };
        let service = self.commands.clone();
        let executor = self.executor.clone();
        let id = current.definition.id;
        let generation = self.snippet_generation.wrapping_add(1).max(1);
        self.snippet_generation = generation;
        self.snippet_saving = true;
        cx.spawn_in(window, async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    let removed = service.delete_snippet_versioned(id, revision).await?;
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled(command_error_context(
                            "命令片段删除已取消",
                            "Snippet deletion was cancelled",
                        )));
                    }
                    Ok(removed)
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.snippet_generation != generation {
                    return;
                }
                view.snippet_saving = false;
                view.confirm_snippet_delete = false;
                match result {
                    Ok(true) => {
                        snippet_applicability::clear(view);
                        view.snippet_inline_preview =
                            snippet_browser::SnippetInlinePreview::Collapsed;
                        view.selected_snippet = None;
                        view.snippet_variables.clear();
                        view.snippet_context_bindings.clear();
                        view.snippet_is_new = false;
                        view.status = Some(command_message("片段已删除", "Snippet deleted"));
                        view.reload_panel(cx);
                    }
                    Ok(false) => {
                        view.status =
                            Some(command_message("片段已不存在", "Snippet no longer exists"))
                    }
                    Err(error) => view.status = Some(error.public_message().to_owned()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 用户取消二次删除确认时保留表单与原 revision，之后可以继续保存。
    pub(super) fn cancel_snippet_delete(&mut self, cx: &mut Context<Self>) {
        self.confirm_snippet_delete = false;
        self.status = None;
        cx.notify();
    }

    /// 只运行与编辑草稿一致的保存快照；未知回执必须先由用户核对原终端，不能被新一轮覆盖。
    /// @author kongweiguang
    pub(super) fn run_workflow(&mut self, cx: &mut Context<Self>) {
        if self
            .workflow_unknown_review
            .as_ref()
            .is_some_and(WorkflowUnknownReview::blocks_new_run)
        {
            self.status = Some(command_message(
                "请先检查未知结果对应的原终端，并明确结束本次核对",
                "Inspect the original terminal for the unknown result and explicitly finish its review first",
            ));
            cx.notify();
            return;
        }
        let Some(binding) = self.ui_session_binding.clone() else {
            self.status = Some(command_message(
                "当前终端尚无已确认的 UI 会话绑定，请重新选择目标终端",
                "The terminal has no verified UI session binding; select the target terminal again",
            ));
            cx.notify();
            return;
        };
        if self.session_id.as_ref() != Some(&binding.session_id) {
            self.status = Some(command_message(
                "工作流目标绑定已过期，请重新选择终端",
                "Workflow target binding is stale; select the terminal again",
            ));
            cx.notify();
            return;
        }
        if self.workflow_editor_locked() {
            self.status = Some(command_message(
                "当前工作流仍在运行或等待确认，请先完成当前运行",
                "The current workflow is running or awaiting confirmation; finish it first",
            ));
            cx.notify();
            return;
        }
        if !self.workflow_draft_matches_saved(cx) {
            self.status = Some(command_message(
                "工作流定义有未保存修改，请先保存再运行",
                "Workflow has unsaved changes; save it before running",
            ));
            cx.notify();
            return;
        }
        let Some(workflow) = self
            .selected_workflow
            .as_ref()
            .map(|item| item.definition.clone())
        else {
            self.status = Some(command_message(
                "请先打开一个工作流",
                "Open a workflow first",
            ));
            cx.notify();
            return;
        };
        let run = match self.commands.start_workflow_run(&workflow) {
            Ok(run) => run,
            Err(error) => {
                self.status = Some(error.public_message().to_owned());
                cx.notify();
                return;
            }
        };
        self.workflow_attempts.clear();
        self.workflow_progress = None;
        self.workflow_run = Some(run);
        self.workflow_binding = Some(binding);
        self.workflow_session_info = self.session_info.clone();
        self.workflow_control = Some(WorkflowExecutionControl::new());
        self.execute_workflow_run(false, cx);
    }

    /// 用户检查未知结果对应的原终端后才解除新运行门禁；不改写旧回执或启动终端命令。
    ///
    /// @author kongweiguang
    pub(super) fn acknowledge_workflow_unknown_review(&mut self, cx: &mut Context<Self>) {
        let acknowledged = self
            .workflow_unknown_review
            .as_mut()
            .is_some_and(WorkflowUnknownReview::acknowledge);
        if acknowledged {
            self.status = Some(command_message(
                "已核对原终端；未知结果仍保留为未知，现在可以启动新的运行",
                "The original terminal was reviewed; the prior result remains unknown and a new run is now allowed",
            ));
            cx.notify();
        }
    }

    /// 只有同一 UI binding 的状态机待确认步骤才允许继续，目标代次变化时只取消旧运行。
    pub(super) fn confirm_workflow_step(&mut self, cx: &mut Context<Self>) {
        if self
            .workflow_run
            .as_ref()
            .is_none_or(|run| run.state() != WorkflowRunState::AwaitingConfirmation)
        {
            self.status = Some(command_message(
                "当前工作流没有等待确认的步骤",
                "No workflow step is waiting for confirmation",
            ));
            cx.notify();
            return;
        }
        if !self.workflow_binding_is_current() {
            self.cancel_workflow(cx);
            self.status = Some(command_message(
                "审批目标已切换，已取消旧工作流；请在当前终端重新启动",
                "The approved target changed. The old workflow was cancelled; start it again on the current terminal",
            ));
            cx.notify();
            return;
        }
        self.execute_workflow_run(true, cx);
    }

    /// 仅当前 UI binding 下的明确失败可由用户重试；未知结果和正在编辑的定义不自动重放。
    /// @author kongweiguang
    pub(super) fn retry_workflow_step(&mut self, cx: &mut Context<Self>) {
        if self.workflow_editor_locked() {
            self.status = Some(command_message(
                "请先完成当前工作流编辑操作",
                "Finish the current workflow editor operation first",
            ));
            cx.notify();
            return;
        }
        if !self.workflow_binding_is_current() {
            self.status = Some(command_message(
                "工作流目标已变化，请在当前终端重新启动，而不要重试旧目标",
                "The workflow target changed; start it again on the current terminal instead of retrying the old target",
            ));
            cx.notify();
            return;
        }
        let Some(mut run) = self.workflow_run.clone() else {
            self.status = Some(command_message(
                "当前没有可重试的工作流运行",
                "There is no workflow step available to retry",
            ));
            cx.notify();
            return;
        };
        if let Err(error) = self.commands.retry_workflow_step(&mut run) {
            self.status = Some(error.public_message().to_owned());
            cx.notify();
            return;
        }
        self.workflow_run = Some(run);
        self.workflow_control = Some(WorkflowExecutionControl::new());
        self.execute_workflow_run(true, cx);
    }

    /// 运行中取消等待真实 receipt；待确认没有活动 lease，清除旧投影后才显示本地取消状态。
    /// @author kongweiguang
    pub(super) fn cancel_workflow(&mut self, cx: &mut Context<Self>) {
        if self.workflow_running {
            let Some(control) = self.workflow_control.as_ref() else {
                return;
            };
            control.cancel();
            self.status = Some(command_message(
                "正在取消当前步骤并确认终端结果…",
                "Cancelling the current step and waiting for the terminal result…",
            ));
            cx.notify();
            return;
        }
        if self.workflow_saving
            || self.workflow_loading
            || self.workflow_deleting
            || self.pending_workflow_transition.is_some()
            || self.confirm_workflow_delete
        {
            self.status = Some(command_message(
                "工作流编辑操作尚未结束，未启动终端命令",
                "A workflow editor operation is still active; no terminal command was started",
            ));
            cx.notify();
            return;
        }

        let Some(run) = self
            .workflow_run
            .as_mut()
            .filter(|run| run.state() == WorkflowRunState::AwaitingConfirmation)
        else {
            return;
        };
        match run.cancel() {
            Ok(()) => {
                self.workflow_control = None;
                self.workflow_progress = None;
                self.status = Some(command_message(
                    "等待确认的工作流步骤已取消",
                    "The workflow step awaiting confirmation was cancelled",
                ));
            }
            Err(error) => {
                self.status = Some(format!(
                    "{}: {error:?}",
                    command_message("无法取消工作流", "Could not cancel workflow",)
                ))
            }
        }
        cx.notify();
    }

    /// 以 tracked executor 执行整轮用例；丢弃 GPUI waiter 会向 executor 传播取消 token。
    /// 执行目标以捕获的 UI binding 为准；应用订阅包装负责合并进度和真实回执，UI 不依赖 Tokio。
    /// @author kongweiguang
    fn execute_workflow_run(&mut self, confirmed: bool, cx: &mut Context<Self>) {
        let (Some(binding), Some(saved), Some(run)) = (
            self.workflow_binding.clone(),
            self.selected_workflow.clone(),
            self.workflow_run.clone(),
        ) else {
            self.status = Some(command_message(
                "工作流需要明确的活动终端和已保存定义",
                "A workflow needs an explicit active terminal and a saved definition",
            ));
            cx.notify();
            return;
        };
        if self.session_id.as_ref() != Some(&binding.session_id)
            || self.ui_session_binding.as_ref() != Some(&binding)
        {
            self.status = Some(command_message(
                "工作流目标绑定已过期，未发送命令",
                "The workflow target binding expired; no command was sent",
            ));
            if let Some(run) = self.workflow_run.as_mut()
                && run.state() == WorkflowRunState::AwaitingConfirmation
            {
                let _ = run.cancel();
                self.workflow_progress = None;
            }
            self.workflow_control = None;
            cx.notify();
            return;
        }
        if self.workflow_running {
            self.status = Some(command_message(
                "当前工作流仍在执行",
                "A workflow is already running",
            ));
            cx.notify();
            return;
        }
        let control = self
            .workflow_control
            .clone()
            .filter(|control| !control.is_cancelled())
            .unwrap_or_default();
        self.workflow_control = Some(control.clone());
        self.workflow_generation = self.workflow_generation.wrapping_add(1).max(1);
        let generation = self.workflow_generation;
        let service = self.commands.clone();
        let executor = self.executor.clone();
        let run_id = run.id().to_owned();
        let initial_progress = self
            .workflow_progress
            .clone()
            .filter(|progress| progress.run_id == run_id);
        let (progress_sender, mut progress_receiver) =
            WorkflowProgressPublisher::channel(initial_progress);
        self.workflow_running = true;
        self.status = Some(command_message("正在执行工作流…", "Running workflow…"));
        cx.spawn(async move |this, cx| {
            let execution = executor.run(move |cancel| async move {
                service
                    .execute_workflow_with_progress(
                        WorkflowExecutionRequest {
                            workflow: saved.definition,
                            expected_ui_binding: binding,
                            run,
                            confirmed_current_step: confirmed,
                        },
                        control,
                        cancel,
                        progress_sender,
                    )
                    .await
            });
            let mut execution = std::pin::pin!(execution);
            loop {
                match progress_receiver.next(execution.as_mut()).await {
                    WorkflowProgressEvent::Progress(progress) => {
                        let _ = this.update(cx, |view, cx| {
                            view.apply_workflow_progress(generation, &run_id, progress, cx)
                        });
                    }
                    WorkflowProgressEvent::Finished { result, progress } => {
                        if let Some(progress) = progress {
                            let _ = this.update(cx, |view, cx| {
                                view.apply_workflow_progress(generation, &run_id, progress, cx)
                            });
                        }
                        let _ = this.update(cx, |view, cx| {
                            view.finish_workflow_run(generation, &run_id, result, cx)
                        });
                        break;
                    }
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// 只接受当前运行的最新完整快照；旧 generation 或旧 run 的迟到更新不能覆盖新操作。
    /// @author kongweiguang
    fn apply_workflow_progress(
        &mut self,
        generation: u64,
        expected_run_id: &str,
        progress: WorkflowRunProgress,
        cx: &mut Context<Self>,
    ) {
        if generation != self.workflow_generation
            || progress.run_id != expected_run_id
            || self.workflow_run.as_ref().is_none_or(|run| {
                self.workflow_binding.as_ref().is_none_or(|binding| {
                    !workflow_progress_matches_run(
                        &progress,
                        run,
                        &binding.session_id,
                        self.workflow_progress.as_ref(),
                    )
                })
            })
            || self
                .selected_workflow
                .as_ref()
                .is_none_or(|workflow| workflow.definition.id != progress.workflow_id)
        {
            return;
        }
        let cancellation_pending = self.workflow_running
            && self
                .workflow_control
                .as_ref()
                .is_some_and(WorkflowExecutionControl::is_cancelled);
        self.status = Some(workflow_progress_display_status(
            &progress,
            cancellation_pending,
        ));
        self.workflow_progress = Some(progress);
        cx.notify();
    }

    /// 只接受当前执行代次和 run_id 的 receipt；未知回执单独留存身份摘要并要求原终端核对。
    ///
    /// @author kongweiguang
    fn finish_workflow_run(
        &mut self,
        generation: u64,
        expected_run_id: &str,
        result: Result<WorkflowExecution, AppError>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.workflow_generation
            || self
                .workflow_run
                .as_ref()
                .is_none_or(|run| run.id() != expected_run_id)
        {
            return;
        }
        self.workflow_running = false;
        match result {
            Ok(mut execution) => {
                if let Some(review) = workflow_unknown_review_snapshot(
                    &execution.run,
                    self.workflow_progress.as_ref(),
                    self.workflow_session_info.as_ref(),
                ) {
                    self.workflow_unknown_review = Some(review);
                }
                let target_changed_while_waiting = execution.run.state()
                    == WorkflowRunState::AwaitingConfirmation
                    && !self.workflow_binding_is_current();
                let cancelled_while_waiting = execution.run.state()
                    == WorkflowRunState::AwaitingConfirmation
                    && self
                        .workflow_control
                        .as_ref()
                        .is_some_and(WorkflowExecutionControl::is_cancelled);
                let progress_status = self
                    .workflow_progress
                    .as_ref()
                    .filter(|progress| progress.run_id == execution.run.id())
                    .map(workflow_progress_status);
                if target_changed_while_waiting || cancelled_while_waiting {
                    let _ = execution.run.cancel();
                    self.workflow_progress = None;
                }
                let state = execution.run.state();
                self.workflow_run = Some(execution.run);
                self.workflow_attempts.extend(execution.steps);
                self.status = Some(if target_changed_while_waiting {
                    command_message(
                        "目标终端已切换，待确认步骤已取消",
                        "The terminal target changed; the step awaiting confirmation was cancelled",
                    )
                } else if cancelled_while_waiting {
                    command_message(
                        "等待确认的工作流步骤已取消",
                        "The workflow step awaiting confirmation was cancelled",
                    )
                } else if let Some(progress_status) = progress_status {
                    progress_status
                } else {
                    match state {
                        WorkflowRunState::Ready => {
                            command_message("工作流已就绪", "Workflow is ready")
                        }
                        WorkflowRunState::Running => {
                            command_message("工作流步骤已返回", "Workflow step returned")
                        }
                        WorkflowRunState::AwaitingConfirmation => command_message(
                            "下一步需要显式确认",
                            "The next step needs confirmation",
                        ),
                        WorkflowRunState::Completed => {
                            command_message("工作流已完成", "Workflow completed")
                        }
                        WorkflowRunState::Failed => command_message(
                            "工作流失败；未知结果不能自动重试",
                            "Workflow failed; an unknown result cannot be retried automatically",
                        ),
                        WorkflowRunState::Cancelled => command_message(
                            "工作流已取消，保留已完成步骤",
                            "Workflow cancelled; completed steps were retained",
                        ),
                    }
                });
                if !matches!(state, WorkflowRunState::AwaitingConfirmation) {
                    self.workflow_control = None;
                }
                self.reload_history(cx);
            }
            Err(error) => {
                self.workflow_control = None;
                let last_progress = self.workflow_progress.take();
                if let Some(run) = self.workflow_run.as_mut()
                    && run.state() == WorkflowRunState::Running
                {
                    let _ = run.mark_current_step_unknown();
                }
                if let Some(run) = self.workflow_run.as_ref()
                    && let Some(review) = workflow_unknown_review_snapshot(
                        run,
                        last_progress.as_ref(),
                        self.workflow_session_info.as_ref(),
                    )
                {
                    self.workflow_unknown_review = Some(review);
                }
                self.status = Some(error.public_message().to_owned());
            }
        }
        cx.notify();
    }

    /// 根据明确目标列表生成风险预览；多分屏、远程或破坏性广播在执行前显示确认理由。
    ///
    /// @author kongweiguang
    pub(super) fn prepare_broadcast(&mut self, cx: &mut Context<Self>) {
        if self.send_operation.active.is_some() {
            self.status = Some(command_message(
                "当前已有命令发送，广播预览暂不可用",
                "A command send is already active; broadcast preview is unavailable",
            ));
            cx.notify();
            return;
        }
        let command = self
            .command_input
            .update(cx, |input, _| input.value().to_string());
        let targets = self
            .ui_sessions
            .iter()
            .filter_map(broadcast_target)
            .collect::<Vec<_>>();
        let bindings = self
            .ui_sessions
            .iter()
            .filter_map(broadcast_binding)
            .collect::<Vec<_>>();
        let analysis = BroadcastAnalysis::analyze(&command, targets.clone());
        if !analysis.is_sendable() || targets.len() != bindings.len() {
            self.status = Some(command_message(
                "没有可广播的运行中分屏，或命令为空",
                "No running panes are available to broadcast to, or the command is empty",
            ));
            cx.notify();
            return;
        }
        self.pending_broadcast = Some((command, targets, bindings));
        if analysis.requires_confirmation {
            self.status = Some(command_message(
                format!("广播需要确认：{}", analysis.reasons.join("；")),
                format!(
                    "Broadcast needs confirmation: {}",
                    analysis.reasons.join("; ")
                ),
            ));
        } else {
            self.execute_broadcast(true, cx);
        }
        cx.notify();
    }

    /// 对风险预览中的精确 session 集合广播；目标再次验证，用户取消后保留部分结果。
    ///
    /// @author kongweiguang
    pub(super) fn execute_broadcast(&mut self, confirmed: bool, cx: &mut Context<Self>) {
        if self.send_operation.active.is_some() {
            self.status = Some(command_message(
                "当前已有发送操作，广播尚未开始；确认内容仍保留",
                "Another send is active; the broadcast has not started and its confirmation is preserved",
            ));
            cx.notify();
            return;
        }
        let Some((command, targets, bindings)) = self.pending_broadcast.take() else {
            return;
        };
        let Some(send_operation_token) = self.send_operation.begin(SendOperationKind::Broadcast)
        else {
            self.pending_broadcast = Some((command, targets, bindings));
            return;
        };
        let service = self.commands.clone();
        let executor = self.executor.clone();
        let user_cancellation = CancellationToken::new();
        self.broadcast_control = Some(user_cancellation.clone());
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |shutdown| async move {
                    service
                        .broadcast_with_cancellation(
                            command,
                            targets,
                            bindings,
                            confirmed,
                            user_cancellation,
                            shutdown,
                        )
                        .await
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if !view.send_operation.finish(send_operation_token) {
                    return;
                }
                view.broadcast_control = None;
                match result {
                    Ok(result) => view.finish_broadcast(result, cx),
                    Err(error) => view.status = Some(error.public_message().to_owned()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 取消只停止当前广播任务及尚未处理的目标，调用方等待用例返回后再清除状态。
    pub(super) fn cancel_broadcast(&mut self, cx: &mut Context<Self>) {
        let Some(control) = self.broadcast_control.as_ref() else {
            return;
        };
        control.cancel();
        self.status = Some(command_message(
            "正在停止广播；当前终端写入返回后将取消剩余目标",
            "Stopping broadcast; remaining targets will be cancelled after the current terminal write returns",
        ));
        cx.notify();
    }

    /// 按目标逐项总结广播结果，不把部分失败包装为全量成功。
    fn finish_broadcast(&mut self, result: BroadcastExecutionResult, cx: &mut Context<Self>) {
        let delivered = result.deliveries.iter().filter(|item| item.sent).count();
        let cancelled = result
            .deliveries
            .iter()
            .filter(|item| item.cancelled)
            .count();
        let failed = result
            .deliveries
            .iter()
            .filter(|item| item.attempted && !item.sent && !item.cancelled)
            .count();
        let history_failures = result
            .deliveries
            .iter()
            .filter(|item| item.history_error_code.is_some())
            .count();
        self.status = Some(command_message(
            format!(
                "广播结束：{delivered} 个已发送，{failed} 个失败，{cancelled} 个已取消；{history_failures} 条历史未写入，退出状态未跟踪"
            ),
            format!(
                "Broadcast finished: {delivered} sent, {failed} failed, {cancelled} cancelled; {history_failures} history entries were not saved, exit status is not tracked"
            ),
        ));
        self.broadcast_control = None;
        self.pending_broadcast = None;
        self.reload_history(cx);
    }

    /// 构建版本化片段表单，并将变量默认值只用于当前草稿输入状态。
    ///
    /// @author kongweiguang
    pub(super) fn install_snippet(
        &mut self,
        saved: VersionedCommandDefinition<SnippetDefinition>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let installed_key = SnippetPreferenceKey {
            origin: if saved.revision.is_some() {
                SnippetCatalogOrigin::User
            } else {
                SnippetCatalogOrigin::Builtin
            },
            snippet_id: saved.definition.id.clone(),
        };
        self.snippet_inline_preview
            .reconcile_loaded_key(&installed_key);
        let identity_changed = match self.selected_snippet.as_ref() {
            Some(current) => {
                current.definition.id != saved.definition.id
                    || current.revision.is_some() != saved.revision.is_some()
            }
            None => true,
        };
        if identity_changed {
            snippet_browser::cancel_snippet_copy(self);
            self.snippet_advanced_open = false;
        }
        let definition = &saved.definition;
        self.snippet_title.update(cx, |input, cx| {
            input.set_value(definition.title.clone(), window, cx)
        });
        self.snippet_command.update(cx, |input, cx| {
            input.set_value(definition.command.clone(), window, cx)
        });
        self.snippet_description.update(cx, |input, cx| {
            input.set_value(
                definition.description.clone().unwrap_or_default(),
                window,
                cx,
            )
        });
        self.snippet_tags.update(cx, |input, cx| {
            input.set_value(definition.tags.join(", "), window, cx)
        });
        self.snippet_category.update(cx, |input, cx| {
            input.set_value(definition.category.clone().unwrap_or_default(), window, cx)
        });
        self.snippet_sort_order.update(cx, |input, cx| {
            input.set_value(definition.sort_order.to_string(), window, cx)
        });
        self.snippet_scope = definition.scope;
        self.snippet_risk = definition.risk;
        self.snippet_default_action = definition.default_action;
        self.snippet_context_bindings = definition
            .context_bindings
            .iter()
            .cloned()
            .map(|binding| new_context_binding_input(binding, window, cx))
            .collect();
        if self.snippet_context_bindings.is_empty() {
            self.snippet_context_bindings
                .push(new_context_binding_input(
                    SnippetContextBinding {
                        kind: SnippetContextBindingKind::Global,
                        target_id: None,
                    },
                    window,
                    cx,
                ));
        }
        let mut next_variable_id = self.snippet_variable_id;
        self.snippet_variables = definition
            .variables
            .iter()
            .cloned()
            .map(|variable| {
                next_variable_id = next_variable_id.wrapping_add(1).max(1);
                new_snippet_variable_input(next_variable_id, variable, window, cx)
            })
            .collect();
        self.snippet_variable_id = next_variable_id;
        self.selected_snippet_sensitive = definition
            .variables
            .iter()
            .any(|variable| variable.sensitive || variable.kind == SnippetVariableKind::Secret);
        self.selected_snippet = Some(saved);
        snippet_applicability::refresh_selected(self, cx);
        self.snippet_is_new = false;
        self.configuration_conflict = None;
        self.status = None;
        cx.notify();
    }
}

/// 在命令面板中把状态消息统一映射到当前 Kit locale，错误安全文案仍由应用层维护。
pub(super) fn command_message(
    zh_cn: impl Into<SharedString>,
    en_us: impl Into<SharedString>,
) -> String {
    crate::locale::text(zh_cn, en_us).to_string()
}

/// 同一运行只接纳精确目标和递增 revision，避免迟到快照覆盖新运行或另一个终端。
/// @author kongweiguang
fn workflow_progress_matches_run(
    progress: &WorkflowRunProgress,
    run: &WorkflowRun,
    session_id: &SessionId,
    current: Option<&WorkflowRunProgress>,
) -> bool {
    progress.run_id == run.id()
        && progress.workflow_id == run.workflow_id()
        && &progress.session_id == session_id
        && current.is_none_or(|current| {
            current.run_id != progress.run_id || current.revision < progress.revision
        })
}

/// 仅从当前 run 的 Unknown 步骤快照提取核对摘要；迟到或异 run 投影不能借来目标信息。
///
/// @author kongweiguang
fn workflow_unknown_review_snapshot(
    run: &WorkflowRun,
    progress: Option<&WorkflowRunProgress>,
    captured_session: Option<&SessionInfo>,
) -> Option<WorkflowUnknownReview> {
    let unknown_step = run
        .current_result()
        .filter(|step| step.state == WorkflowStepState::Unknown)?;
    let progress = progress.filter(|progress| {
        progress.run_id == run.id() && progress.workflow_id == run.workflow_id()
    });
    let step_title = progress
        .and_then(|progress| {
            progress
                .current_step_index
                .and_then(|index| progress.steps.get(index))
                .filter(|step| step.step_id == unknown_step.step_id)
                .map(|step| step.title.clone())
        })
        .unwrap_or_else(|| unknown_step.step_id.clone());
    let target = progress
        .map_or_else(
            || {
                captured_session.map(|session| {
                    workflow_unknown_target_label(
                        &session.target,
                        &session.session_id,
                        Some(session),
                    )
                })
            },
            |progress| {
                Some(workflow_unknown_target_label(
                    &progress.target,
                    &progress.session_id,
                    captured_session,
                ))
            },
        )
        .unwrap_or_else(|| {
            crate::locale::text("目标终端身份不可用", "Terminal target identity unavailable")
                .to_string()
        });
    Some(WorkflowUnknownReview {
        run_id: run.id().to_owned(),
        target,
        step_title,
        acknowledged: false,
    })
}

/// 只显示安全的协议/会话摘要，不序列化目标 Debug 或运行配置中的环境与凭据字段。
///
/// @author kongweiguang
fn workflow_unknown_target_label(
    target: &SessionTarget,
    session_id: &SessionId,
    captured_session: Option<&SessionInfo>,
) -> String {
    let target_label = match target {
        SessionTarget::Local(_) => crate::locale::text("本地终端", "Local terminal").to_string(),
        SessionTarget::Ssh(target) => crate::locale::text(
            format!("SSH 主机 {}", target.host_id),
            format!("SSH host {}", target.host_id),
        )
        .to_string(),
        SessionTarget::Telnet(_) => {
            crate::locale::text("Telnet 终端", "Telnet terminal").to_string()
        }
        SessionTarget::Serial(_) => crate::locale::text("串口终端", "Serial terminal").to_string(),
        SessionTarget::Container(_) => {
            crate::locale::text("容器终端", "Container terminal").to_string()
        }
    };
    captured_session
        .filter(|session| &session.session_id == session_id && &session.target == target)
        .map_or(target_label.clone(), |session| {
            format!("{target_label} · {}", session_title(session))
        })
}

/// 取消请求不能覆盖最终退出事实，但等待原命令回执时不能被 Running 快照抹成正常执行。
/// @author kongweiguang
fn workflow_progress_display_status(
    progress: &WorkflowRunProgress,
    cancellation_pending: bool,
) -> String {
    if cancellation_pending
        && matches!(
            progress.state,
            WorkflowRunState::Ready
                | WorkflowRunState::Running
                | WorkflowRunState::AwaitingConfirmation
        )
    {
        return command_message(
            "正在取消当前步骤并确认终端结果…",
            "Cancelling the current step and waiting for the terminal result…",
        );
    }
    workflow_progress_status(progress)
}

/// 把逐步快照投影成可行动的当前状态，特别区分未知回执与可安全重试的明确失败。
/// @author kongweiguang
fn workflow_progress_status(progress: &WorkflowRunProgress) -> String {
    let current = progress
        .current_step_index
        .and_then(|index| progress.steps.get(index).map(|step| (index, step)));
    match (progress.state, current) {
        (WorkflowRunState::Ready, _) => command_message("工作流已就绪", "Workflow is ready"),
        (WorkflowRunState::Running, Some((index, step)))
            if step.state == WorkflowStepState::Pending =>
        {
            command_message(
                format!(
                    "准备执行第 {}/{} 步：{}",
                    index + 1,
                    progress.steps.len(),
                    step.title
                ),
                format!(
                    "Preparing step {} of {}: {}",
                    index + 1,
                    progress.steps.len(),
                    step.title
                ),
            )
        }
        (WorkflowRunState::Running, Some((index, step))) => command_message(
            format!(
                "正在执行第 {}/{} 步：{}",
                index + 1,
                progress.steps.len(),
                step.title
            ),
            format!(
                "Running step {} of {}: {}",
                index + 1,
                progress.steps.len(),
                step.title
            ),
        ),
        (WorkflowRunState::Running, None) => {
            command_message("工作流正在执行", "Workflow is running")
        }
        (WorkflowRunState::AwaitingConfirmation, Some((index, step))) => command_message(
            format!(
                "等待确认第 {}/{} 步：{}",
                index + 1,
                progress.steps.len(),
                step.title
            ),
            format!(
                "Waiting to confirm step {} of {}: {}",
                index + 1,
                progress.steps.len(),
                step.title
            ),
        ),
        (WorkflowRunState::AwaitingConfirmation, None) => {
            command_message("等待确认当前步骤", "Waiting for confirmation")
        }
        (WorkflowRunState::Completed, _) => command_message("工作流已完成", "Workflow completed"),
        (WorkflowRunState::Failed, Some((_, step))) if step.state == WorkflowStepState::Unknown => {
            command_message(
                "当前步骤结果未知，请核对终端；不会自动重试",
                "The step result is unknown. Check the terminal; it will not be retried automatically",
            )
        }
        (WorkflowRunState::Failed, Some((index, step)))
            if step.state == WorkflowStepState::NotStarted =>
        {
            command_message(
                format!(
                    "第 {} 步未发送，可检查后显式重试：{}",
                    index + 1,
                    step.title
                ),
                format!(
                    "Step {} was not sent; review it before retrying: {}",
                    index + 1,
                    step.title
                ),
            )
        }
        (WorkflowRunState::Failed, Some((index, step))) => command_message(
            format!(
                "第 {} 步失败：{}，退出码 {:?}",
                index + 1,
                step.title,
                step.exit_code
            ),
            format!(
                "Step {} failed: {}; exit code {:?}",
                index + 1,
                step.title,
                step.exit_code
            ),
        ),
        (WorkflowRunState::Failed, None) => command_message("工作流失败", "Workflow failed"),
        (WorkflowRunState::Cancelled, Some((_, step)))
            if step.state == WorkflowStepState::Unknown =>
        {
            command_message(
                "工作流已取消；当前步骤结果未知，请先核对终端",
                "Workflow cancelled; check the terminal because the current step result is unknown",
            )
        }
        (WorkflowRunState::Cancelled, _) => command_message(
            "工作流已取消，已完成步骤已保留",
            "Workflow cancelled; completed steps were retained",
        ),
    }
}

/// 应用错误只接受静态安全文案，因此在构造时按 Kit locale 选择静态字面量。
pub(super) fn command_error_context(zh_cn: &'static str, en_us: &'static str) -> &'static str {
    match crate::locale::current_language() {
        crate::locale::UiLanguage::ZhCn => zh_cn,
        crate::locale::UiLanguage::EnUs => en_us,
    }
}

/// 原样保留派生身份；来源名称和链接只是展示信息，混入会导致填入的精确版本校验误拒绝。
///
/// @author kongweiguang
fn snippet_definition_from_catalog(item: SnippetCatalogItem) -> SnippetDefinition {
    SnippetDefinition {
        id: item.id,
        title: item.title,
        command: item.template,
        description: (!item.description.is_empty()).then_some(item.description),
        tags: item.tags,
        sort_order: item.sort_order,
        scope: item.scope,
        category: Some(item.category),
        risk: item.risk,
        default_action: item.default_action,
        variables: item.variables,
        context_bindings: item.context_bindings,
        derived_from: item.derived_from,
    }
}

/// 只允许领域校验认可的类型使用原始插值，避免编辑器提供无效的 shell 写入策略。
///
/// @author kongweiguang
fn supports_validated_raw(kind: SnippetVariableKind) -> bool {
    matches!(
        kind,
        SnippetVariableKind::Port
            | SnippetVariableKind::Integer
            | SnippetVariableKind::Host
            | SnippetVariableKind::Service
            | SnippetVariableKind::Container
            | SnippetVariableKind::Enum
    )
}

/// 类型变更先规范化领域值，Secret 绝不保留可持久化默认项或建议。
///
/// @author kongweiguang
fn transition_snippet_variable_kind(
    mut variable: SnippetVariable,
    kind: SnippetVariableKind,
) -> SnippetVariable {
    variable.kind = kind;
    variable.sensitive = kind == SnippetVariableKind::Secret;
    if variable.sensitive {
        variable.default_value = None;
        variable.suggestions.clear();
    }
    if variable.render_strategy == SnippetVariableRenderStrategy::ValidatedRaw
        && !supports_validated_raw(kind)
    {
        variable.render_strategy = SnippetVariableRenderStrategy::ShellArg;
    }
    variable
}

/// 下拉每次按固定顺序提供完整领域变量类型，避免隐藏旧版可配置能力。
///
/// @author kongweiguang
fn snippet_variable_kind_options() -> Vec<SnippetVariableKindOption> {
    [
        (SnippetVariableKind::Text, "文本", "Text"),
        (SnippetVariableKind::Path, "路径", "Path"),
        (SnippetVariableKind::Port, "端口", "Port"),
        (SnippetVariableKind::Integer, "整数", "Integer"),
        (SnippetVariableKind::Host, "主机名", "Host"),
        (SnippetVariableKind::Url, "网址", "URL"),
        (SnippetVariableKind::Service, "服务名", "Service"),
        (SnippetVariableKind::Container, "容器名", "Container"),
        (SnippetVariableKind::Enum, "枚举", "Enum"),
        (SnippetVariableKind::Secret, "敏感信息", "Secret"),
        (SnippetVariableKind::Raw, "原始文本", "Raw text"),
    ]
    .into_iter()
    .map(|(kind, zh_cn, en_us)| SnippetVariableKindOption {
        kind,
        label: crate::locale::text(zh_cn, en_us),
    })
    .collect()
}

/// 原始插值只对领域允许且经运行时严格验证的类型开放。
///
/// @author kongweiguang
fn snippet_variable_render_strategy_options(
    kind: SnippetVariableKind,
) -> Vec<SnippetVariableRenderStrategyOption> {
    let strategies = if supports_validated_raw(kind) {
        vec![
            SnippetVariableRenderStrategy::ShellArg,
            SnippetVariableRenderStrategy::ValidatedRaw,
            SnippetVariableRenderStrategy::Literal,
        ]
    } else {
        vec![
            SnippetVariableRenderStrategy::ShellArg,
            SnippetVariableRenderStrategy::Literal,
        ]
    };
    strategies
        .into_iter()
        .map(|strategy| SnippetVariableRenderStrategyOption {
            strategy,
            label: crate::locale::text(
                match strategy {
                    SnippetVariableRenderStrategy::ShellArg => "Shell 参数转义",
                    SnippetVariableRenderStrategy::ValidatedRaw => "验证后原样插入",
                    SnippetVariableRenderStrategy::Literal => "模板字面量",
                },
                match strategy {
                    SnippetVariableRenderStrategy::ShellArg => "Shell argument quoting",
                    SnippetVariableRenderStrategy::ValidatedRaw => "Validated raw insertion",
                    SnippetVariableRenderStrategy::Literal => "Template literal",
                },
            ),
        })
        .collect()
}

/// 创建类型、策略选择器和字段输入；提示属于 InputState，Secret 默认值不会进入运行输入。
///
/// @author kongweiguang
fn new_snippet_variable_input(
    id: u64,
    variable: SnippetVariable,
    window: &mut Window,
    cx: &mut Context<CommandsView>,
) -> SnippetVariableInput {
    let current_kind = if variable.sensitive || variable.kind == SnippetVariableKind::Secret {
        SnippetVariableKind::Secret
    } else {
        variable.kind
    };
    let variable = transition_snippet_variable_kind(variable, current_kind);
    let kind_select =
        cx.new(|cx| SelectState::new(snippet_variable_kind_options(), None, window, cx));
    kind_select.update(cx, |select, cx| {
        select.set_selected_value(&current_kind, window, cx);
    });
    let render_strategy = variable.render_strategy;
    let render_strategy_select = cx.new(|cx| {
        SelectState::new(
            snippet_variable_render_strategy_options(current_kind),
            None,
            window,
            cx,
        )
    });
    render_strategy_select.update(cx, |select, cx| {
        select.set_selected_value(&render_strategy, window, cx);
    });

    let kind_subscription = cx.subscribe_in(
        &kind_select,
        window,
        move |view, _, event: &SelectEvent<Vec<SnippetVariableKindOption>>, window, cx| {
            if let SelectEvent::Confirm(Some(kind)) = event {
                view.set_snippet_variable_kind(id, *kind, window, cx);
            }
        },
    );
    let render_strategy_subscription = cx.subscribe_in(
        &render_strategy_select,
        window,
        move |view,
              _,
              event: &SelectEvent<Vec<SnippetVariableRenderStrategyOption>>,
              _window,
              cx| {
            if let SelectEvent::Confirm(Some(strategy)) = event {
                view.set_snippet_variable_render_strategy(id, *strategy, cx);
            }
        },
    );

    let initial = variable.default_value.clone().unwrap_or_default();
    let name = cx.new(|cx| InputState::new(window, cx).default_value(variable.name.clone()));
    let label = cx.new(|cx| InputState::new(window, cx).default_value(variable.label.clone()));
    let description = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(crate::locale::text(
                "描述这个变量的用途",
                "Describe what this variable is for",
            ))
            .default_value(variable.description.clone())
    });
    let default_value = cx.new(|cx| {
        InputState::new(window, cx)
            .default_value(variable.default_value.clone().unwrap_or_default())
    });
    let suggestions = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(crate::locale::text(
                "例如：dev, staging, prod",
                "For example: dev, staging, prod",
            ))
            .default_value(variable.suggestions.join(", "))
    });
    let validation = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(crate::locale::text(
                "只支持安全子集；保存时校验",
                "Safe subset only; checked on save",
            ))
            .default_value(variable.validation.clone().unwrap_or_default())
    });
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .default_value(initial)
            .placeholder(crate::locale::text("运行时变量值", "Runtime value"))
    });
    SnippetVariableInput {
        id,
        variable,
        name,
        label,
        description,
        default_value,
        suggestions,
        validation,
        input,
        kind_select,
        render_strategy_select,
        _kind_subscription: kind_subscription,
        _render_strategy_subscription: render_strategy_subscription,
    }
}

/// 每个上下文绑定保留独立 target 输入，编辑某一项时不压缩其它用户选择。
fn new_context_binding_input(
    binding: SnippetContextBinding,
    window: &mut Window,
    cx: &mut Context<CommandsView>,
) -> SnippetContextBindingInput {
    let target = binding.target_id.clone().unwrap_or_default();
    SnippetContextBindingInput {
        binding,
        target_id: cx.new(|cx| InputState::new(window, cx).default_value(target)),
    }
}

/// 将 UI 文本与会话目标转换成严格限定的建议请求，当前目录和 session 均来自 runtime。
fn suggestion_request(
    session: &SessionInfo,
    input: String,
    generation: u64,
) -> CommandSuggestionRequest {
    CommandSuggestionRequest {
        cursor: input.chars().count(),
        input,
        target: history_target_for_target(&session.target),
        session_id: Some(session.session_id.to_string()),
        pane_id: session_pane(session).map(|pane| pane.to_string()),
        profile_id: None,
        remote_host_id: session_host(&session.target).map(ToString::to_string),
        cwd: session_cwd(&session.target),
        shell: session_shell(&session.target),
        providers: Some(vec![
            SuggestionProviderKind::History,
            SuggestionProviderKind::Snippet,
        ]),
        limit: Some(8),
        mode: SuggestionQueryMode::Menu,
        generation: Some(generation),
        context_key: Some(session.session_id.to_string()),
    }
}

/// 命令历史过滤协议与真实 SessionTarget 一一对应，容器按旧 wire 值 dockerContainer 映射。
fn history_target_for_target(target: &SessionTarget) -> CommandHistoryTarget {
    match target {
        SessionTarget::Local(_) => CommandHistoryTarget::Local,
        SessionTarget::Ssh(_) => CommandHistoryTarget::Ssh,
        SessionTarget::Telnet(_) => CommandHistoryTarget::Telnet,
        SessionTarget::Serial(_) => CommandHistoryTarget::Serial,
        SessionTarget::Container(_) => CommandHistoryTarget::Container,
    }
}

/// 仅 UI 所有会话能得到 pane 目标，后台 session 不伪造可见广播身份。
fn session_pane(session: &SessionInfo) -> Option<&PaneId> {
    match &session.owner {
        SessionOwner::Ui { pane_id, .. } => Some(pane_id),
        SessionOwner::Agent { .. } | SessionOwner::Mcp | SessionOwner::Service { .. } => None,
    }
}

/// 查询历史和建议时只携带 SSH 或远程容器的 host 引用。
fn session_host(target: &SessionTarget) -> Option<&kerminal_domain::identity::HostId> {
    match target {
        SessionTarget::Ssh(target) => Some(&target.host_id),
        SessionTarget::Container(target) => target.host_id.as_ref(),
        SessionTarget::Local(_) | SessionTarget::Telnet(_) | SessionTarget::Serial(_) => None,
    }
}

/// 当前配置过的工作目录用于建议过滤；未知 cwd 保持为空而不读取终端文本猜路径。
fn session_cwd(target: &SessionTarget) -> Option<String> {
    match target {
        SessionTarget::Local(target) => target.working_directory.as_ref(),
        SessionTarget::Ssh(target) => target.working_directory.as_ref(),
        SessionTarget::Telnet(_) | SessionTarget::Serial(_) | SessionTarget::Container(_) => None,
    }
    .map(|path| path.to_string_lossy().into_owned())
}

/// 本地程序路径只投影文件名到建议上下文，SSH shell 名称没有可靠 runtime 数据时省略。
fn session_shell(target: &SessionTarget) -> Option<String> {
    let SessionTarget::Local(target) = target else {
        return None;
    };
    Some(
        std::path::Path::new(target.program.as_os_str())
            .file_name()
            .unwrap_or(target.program.as_os_str())
            .to_string_lossy()
            .into_owned(),
    )
}

/// 从运行中分屏构造风险预览目标；pane title 只作为 UI 标签，不参与路由。
fn broadcast_target(session: &SessionInfo) -> Option<BroadcastTarget> {
    let pane_id = session_pane(session)?.clone();
    Some(BroadcastTarget {
        pane_id,
        title: session_title(session),
        target: history_target_for_target(&session.target),
    })
}

/// 广播绑定携带 session 与 pane 双重身份，Application 会在写入前再次核对归属。
fn broadcast_binding(session: &SessionInfo) -> Option<BroadcastSessionBinding> {
    Some(BroadcastSessionBinding {
        pane_id: session_pane(session)?.clone(),
        session_id: session.session_id.clone(),
        target: history_target_for_target(&session.target),
    })
}

/// UI 广播列表以 pane 稳定标识显示，不读取终端标题中的路径或命令文本。
fn session_title(session: &SessionInfo) -> String {
    match &session.owner {
        SessionOwner::Ui { pane_id, .. } => command_message(
            format!(
                "分屏 {}",
                pane_id.as_str().chars().take(8).collect::<String>()
            ),
            format!(
                "Pane {}",
                pane_id.as_str().chars().take(8).collect::<String>()
            ),
        ),
        SessionOwner::Agent { .. } | SessionOwner::Mcp | SessionOwner::Service { .. } => {
            command_message("后台会话", "Background session")
        }
    }
}

impl Drop for CommandsView {
    /// Entity 释放时取消面板自有请求；executor 继续跟踪写入直至确定或未知回执。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        cancel_owned_suggestion_request(&mut self.suggestion_cancellation);
        snippet_browser::cancel_snippet_page_request(&mut self.snippet_page_cancellation);
        snippet_browser::cancel_snippet_copy(self);
        snippet_applicability::clear(self);
        if let Some(cancellation) = self.snippet_transfer_cancellation.take() {
            cancellation.cancel();
        }
        if let Some(cancellation) = self.snippet_manager_cancellation.take() {
            cancellation.cancel();
        }
        self.cancel_terminal_fill();
        if let Some(cancellation) = self.command_send_cancellation.take() {
            cancellation.cancel();
        }
    }
}

/// 从 view 所有权槽取出并取消 token，确保下一代查询不会被旧取消源误伤。
fn cancel_owned_suggestion_request(cancellation: &mut Option<CancellationToken>) {
    if let Some(cancellation) = cancellation.take() {
        cancellation.cancel();
    }
}

#[cfg(test)]
#[path = "../../tests/unit/commands_snippet_variable_policy.rs"]
mod snippet_variable_policy_tests;

#[cfg(test)]
#[path = "../../tests/unit/commands_suggestion_cancellation.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/unit/commands_target_presentation.rs"]
mod target_presentation_tests;

#[cfg(test)]
#[path = "../../tests/unit/commands_snippet_fill.rs"]
mod snippet_fill_tests;

#[cfg(test)]
#[path = "../../tests/unit/commands_send_operation.rs"]
mod send_operation_tests;

#[cfg(test)]
#[path = "../../tests/unit/commands_workflow_progress.rs"]
mod workflow_progress_tests;
