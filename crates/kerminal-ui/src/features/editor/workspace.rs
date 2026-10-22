//! 文件编辑工作区的状态、异步用例协调和关闭保护。
//!
//! @author kongweiguang

use std::sync::Arc;

use super::{EditorQuitGate, evaluate_editor_quit_gate};
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Context, Entity, EventEmitter, KeyDownEvent, ScrollHandle,
    Subscription, Task, Window,
    component::input::{EditorState, InputEvent, InputState, Paste, TextDecorationCollection},
};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor, files::editor::FileEditorService,
};
use kerminal_domain::files::editor::{
    CloseDecision, EditorPhase, EditorTransitionError, FileEditorSession, SaveResolution,
};
use kerminal_domain::files::{ContainerRuntime, FileEndpoint};
use tokio_util::sync::CancellationToken;

use super::search::{EditorSearchResults, SearchActivity, SearchDecorationStyles};
use super::search_model::EditorSearchOptions;
use crate::features::files::{FileTargetNameLookup, FileTargetNameState};
use crate::theme::ThemeTokens;

/// 查找面板显式区分关闭、仅查找和查找加替换，快捷键不会误展开替换区。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum SearchPanelMode {
    #[default]
    Closed,
    Find,
    Replace,
}

/// 工作区状态区分普通反馈与可随最后一个文件关闭而清除的保存成功提示。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum EditorStatus {
    Message(String),
    SaveCompleted,
}

impl SearchPanelMode {
    /// Ctrl/Cmd+F 无条件选择紧凑查找状态，不继承上次展开的替换行。
    pub(super) fn show_find(self) -> Self {
        Self::Find
    }

    /// Ctrl/Cmd+H 显式选择替换状态，使键盘入口与工具栏切换一致。
    pub(super) fn show_replace(self) -> Self {
        Self::Replace
    }

    /// 关闭入口只改变面板状态，调用方随后统一取消查询并清空装饰。
    pub(super) fn close(self) -> Self {
        Self::Closed
    }

    /// 工具栏只在打开时进入替换模式，再次点击仅折叠替换行而保留查询。
    pub(super) fn toggle_replace(self) -> Self {
        match self {
            Self::Closed | Self::Find => Self::Replace,
            Self::Replace => Self::Find,
        }
    }

    /// 搜索请求回执在面板任一可见模式下仍有效，只有关闭才使代次失效。
    pub(super) fn is_visible(self) -> bool {
        self != Self::Closed
    }

    /// 替换输入和按钮仅属于展开模式，查找模式保持紧凑单行布局。
    pub(super) fn shows_replace(self) -> bool {
        self == Self::Replace
    }
}

/// 工作区完成退出前脏文件决策后发送此事件，由应用根继续关闭窗口。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileEditorWorkspaceEvent {
    QuitReady,
    QuitCancelled,
    /// 已打开文件的展示投影变化时通知 Root 读取带单调版本的最新快照。
    WorkspaceProjectionChanged,
    WorkspaceTabCloseResolved {
        operation_id: String,
        endpoint: FileEndpoint,
        closed: bool,
    },
}

/// 给文件面板使用的带版本只读快照，不暴露 GPUI Entity 或正文缓冲。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EditorWorkspaceProjection {
    pub(crate) revision: u64,
    pub(crate) tabs: Vec<EditorTabProjection>,
    pub(crate) active_endpoint: Option<FileEndpoint>,
}

/// 一个已打开文件的轻量投影；endpoint 既是身份键，也是 Root 激活时重新校验的依据。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EditorTabProjection {
    pub(crate) endpoint: FileEndpoint,
    pub(crate) title: String,
    pub(crate) dirty: bool,
    pub(crate) readonly: bool,
    pub(crate) loading: bool,
    pub(crate) active: bool,
}

/// 单个文件 tab 的渲染和业务状态；GPUI Rope 独占可编辑正文，领域聚合只持有保存基线。
pub(super) struct EditorTab {
    pub(super) endpoint: FileEndpoint,
    pub(super) title: String,
    pub(super) session: Option<FileEditorSession>,
    pub(super) editor: Option<Entity<EditorState>>,
    pub(super) loading: bool,
    pub(super) load_generation: u64,
    /// 以文档编辑代次隔离后台搜索与替换，迟到任务不能修改后续内容。
    pub(super) document_generation: u64,
    /// 由标签持有读取任务；删除标签时丢弃任务句柄以触发协作取消。
    pub(super) load_task: Option<Task<()>>,
    /// 关闭意图只绑定用户确认后启动的那一代读回，普通重试不能继承它。
    pub(super) close_after_readback: ReadbackCloseIntent,
    pub(super) dirty: bool,
    /// Workspace 文件策略只能收紧编辑权限，不能覆盖文件系统自身的只读状态。
    pub(super) workspace_readonly: bool,
    pub(super) error: Option<String>,
    pub(super) editor_subscription: Option<Subscription>,
    /// 每个文档拥有独立的命中装饰集合，标签切换不会污染另一份缓冲。
    pub(super) search_decorations: Option<TextDecorationCollection>,
    /// 保存请求及回执绑定到文件和代次，切换活动标签不会改投取消或完成结果。
    pub(super) save_operation: Option<PendingEditorSave>,
}

/// UI 保存取消只通知同一个受监督任务，不拥有提交 future 或伪造终态。
pub(super) struct PendingEditorSave {
    endpoint: FileEndpoint,
    generation: u64,
    cancellation: CancellationToken,
    cancel_requested: bool,
}

impl PendingEditorSave {
    /// 保存意图在领域状态进入 Saving 后建立，确保文件身份和异步结果所有者一致。
    pub(super) fn new(
        endpoint: FileEndpoint,
        generation: u64,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            endpoint,
            generation,
            cancellation,
            cancel_requested: false,
        }
    }

    /// 只接受相同文件与代次的真实保存收据，拒绝旧任务触碰新缓冲。
    pub(super) fn matches(&self, endpoint: &FileEndpoint, generation: u64) -> bool {
        self.endpoint == *endpoint && self.generation == generation
    }

    /// 重复点击取消不改变已发出的请求，也不释放仍在等待的结果所有者。
    pub(super) fn request_cancel(&mut self) -> bool {
        if self.cancel_requested {
            return false;
        }
        self.cancel_requested = true;
        self.cancellation.cancel();
        true
    }

    /// 供按钮呈现“等待回执”状态，不把取消请求当作保存操作已完成。
    pub(super) fn cancel_requested(&self) -> bool {
        self.cancel_requested
    }
}

/// 保存结果未知后的关闭意图状态，避免失败或取消后被后续普通加载误消费。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ReadbackCloseIntent {
    #[default]
    None,
    AwaitingConfirmation,
    Reading {
        generation: u64,
    },
}

impl ReadbackCloseIntent {
    /// 记录用户明确要求读回成功后关闭，但等到确认重载才绑定具体代次。
    pub(super) fn arm(&mut self) {
        *self = Self::AwaitingConfirmation;
    }

    /// 将关闭动作绑定到经确认启动的读取，阻止其它 endpoint 或旧响应完成该动作。
    pub(super) fn bind_to_load(&mut self, generation: u64) -> bool {
        if *self != Self::AwaitingConfirmation {
            return false;
        }
        *self = Self::Reading { generation };
        true
    }

    /// 只有绑定代次的成功读回才关闭标签；该代次失败时清除意图并保留编辑器。
    pub(super) fn finish_load(&mut self, generation: u64, succeeded: bool) -> bool {
        if *self != (Self::Reading { generation }) {
            return false;
        }
        *self = Self::None;
        succeeded
    }

    /// 只撤销被用户取消的那次读回，不能清除另一个代次等待中的关闭确认。
    pub(super) fn cancel_load(&mut self, generation: u64) -> bool {
        self.finish_load(generation, false)
    }

    /// 取消尚未开始的确认或清理已终结的读回意图。
    pub(super) fn clear(&mut self) {
        *self = Self::None;
    }

    /// 查询是否仍存在关闭确认或已确认读回。
    pub(super) fn is_pending(self) -> bool {
        self != Self::None
    }
}

/// Root 发起的文件 tab 关闭收据按 operation、endpoint 和读取代次绑定。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PendingWorkspaceClose {
    pub(super) operation_id: String,
    pub(super) endpoint: FileEndpoint,
    pub(super) load_generation: Option<u64>,
}

impl PendingWorkspaceClose {
    /// 只为原始文件身份和代次生成回执，忽略已过期读取或其它标签的结束事件。
    pub(super) fn resolution(
        &self,
        endpoint: &FileEndpoint,
        generation: Option<u64>,
        closed: bool,
    ) -> Option<FileEditorWorkspaceEvent> {
        if &self.endpoint != endpoint || self.load_generation != generation {
            return None;
        }
        Some(FileEditorWorkspaceEvent::WorkspaceTabCloseResolved {
            operation_id: self.operation_id.clone(),
            endpoint: self.endpoint.clone(),
            closed,
        })
    }
}

/// 脏文件在关闭、远端冲突覆盖和显式重载前均要求用户选择。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum EditorConfirmation {
    Close(FileEndpoint),
    Quit(FileEndpoint),
    Overwrite {
        endpoint: FileEndpoint,
        close_after_success: bool,
        quit_after_success: bool,
    },
    Reload {
        endpoint: FileEndpoint,
        continue_quit: bool,
        close_after_readback: bool,
    },
}

impl EditorTab {
    /// 由文件系统权限和工作区访问策略合成有效权限，保存与编辑控件共用此判定。
    pub(super) fn is_readonly(&self) -> bool {
        self.session.as_ref().is_some_and(|session| {
            effective_readonly(session.is_readonly(), self.workspace_readonly)
        })
    }
}

/// 一个原生编辑区承载多个独立文件 tab，并通过现有 FileService 读写本机、SSH 与容器文件。
pub struct FileEditorWorkspace {
    pub(super) service: FileEditorService,
    pub(super) executor: ApplicationTaskExecutor,
    /// 所有 editor tab 共用名称快照，文件路径仍只通过 endpoint 作为业务身份。
    pub(super) target_name_lookup: FileTargetNameLookup,
    pub(super) target_names: FileTargetNameState,
    pub(super) window_handle: AnyWindowHandle,
    pub(super) tabs: Vec<EditorTab>,
    pub(super) active_endpoint: Option<FileEndpoint>,
    /// 投影只在可见内容变化时递增，迟到的 FilesPane 请求不能覆盖较新的推送快照。
    pub(super) projection_revision: u64,
    /// 最近一次推送给 Root 的轻量投影，用于避免按键等无关变化触发重复广播。
    last_published_projection: EditorWorkspaceProjection,
    /// GPUI 依据直接子项边界滚动标签行；滚动位置属于视图状态，不写入工作区数据。
    pub(super) tabs_scroll_handle: ScrollHandle,
    /// 最近一帧布局测得的标签行宽度，用于只在收窄时重显当前标签。
    pub(super) last_tabs_viewport_width: Option<f32>,
    /// 最近一次自动确保可见的活动标签，普通重绘不重复抢占手动滚动位置。
    pub(super) last_tabs_auto_revealed: Option<(FileEndpoint, usize)>,
    pub(super) find_input: Entity<InputState>,
    pub(super) replace_input: Entity<InputState>,
    _find_subscription: Subscription,
    _replace_subscription: Subscription,
    pub(super) confirmation: Option<EditorConfirmation>,
    pub(super) quit_flow: bool,
    pub(super) search_panel_mode: SearchPanelMode,
    /// 搜索设置在工作区内共享，文档标签只承载各自的匹配投影。
    pub(super) search_query: String,
    pub(super) search_options: EditorSearchOptions,
    /// 缓存当前主题配对色，异步搜索回执不依赖 Window 也能生成正确装饰。
    pub(super) search_decoration_styles: SearchDecorationStyles,
    pub(super) search_results: Option<Arc<EditorSearchResults>>,
    pub(super) search_current_range: Option<std::ops::Range<usize>>,
    pub(super) search_match_index: Option<usize>,
    pub(super) search_generation: u64,
    pub(super) search_operation_generation: u64,
    pub(super) search_activity: SearchActivity,
    pub(super) search_operation_cancellation: Option<CancellationToken>,
    pub(super) search_operation_task: Option<Task<()>>,
    pub(super) search_error: Option<String>,
    pub(super) next_load_generation: u64,
    pub(super) next_save_generation: u64,
    pub(super) status: Option<EditorStatus>,
    /// Root 的单一资源关闭操作在编辑器确认完成前保持挂起，避免先移除 workspace tab。
    pub(super) pending_workspace_close: Option<PendingWorkspaceClose>,
}

impl FileEditorWorkspace {
    /// 复用桌面注入的文件服务，并让标签滚动状态留在工作区视图，避免引入第二份标签索引状态。
    ///
    /// @author kongweiguang
    pub(crate) fn new(
        service: FileEditorService,
        executor: ApplicationTaskExecutor,
        target_name_lookup: FileTargetNameLookup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search_decoration_styles =
            SearchDecorationStyles::from_theme(ThemeTokens::current(window, cx));
        let find_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text("查找文本", "Find text"))
        });
        let replace_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::locale::text("替换为", "Replace with"))
        });
        let find_subscription = cx.subscribe(&find_input, |workspace, input, event, cx| {
            if matches!(event, InputEvent::Change) {
                workspace.sync_search_query(input.read(cx).value().to_string(), cx);
            }
        });
        let replace_subscription = cx.subscribe(&replace_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });

        Self {
            service,
            executor,
            target_name_lookup,
            target_names: FileTargetNameState::default(),
            window_handle: window.window_handle(),
            tabs: Vec::new(),
            active_endpoint: None,
            projection_revision: 0,
            last_published_projection: EditorWorkspaceProjection {
                revision: 0,
                tabs: Vec::new(),
                active_endpoint: None,
            },
            tabs_scroll_handle: ScrollHandle::new(),
            last_tabs_viewport_width: None,
            last_tabs_auto_revealed: None,
            find_input,
            replace_input,
            _find_subscription: find_subscription,
            _replace_subscription: replace_subscription,
            confirmation: None,
            quit_flow: false,
            search_panel_mode: SearchPanelMode::Closed,
            search_query: String::new(),
            search_options: EditorSearchOptions::default(),
            search_decoration_styles,
            search_results: None,
            search_current_range: None,
            search_match_index: None,
            search_generation: 0,
            search_operation_generation: 0,
            search_activity: SearchActivity::Idle,
            search_operation_cancellation: None,
            search_operation_task: None,
            search_error: None,
            next_load_generation: 1,
            next_save_generation: 1,
            status: None,
            pending_workspace_close: None,
        }
    }

    /// 切换语言时只改已创建搜索框的提示，保留查询文本、焦点和当前编辑文档。
    ///
    /// @author kongweiguang
    pub fn refresh_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find_input.update(cx, |input, cx| {
            input.set_placeholder(crate::locale::text("查找文本", "Find text"), window, cx)
        });
        self.replace_input.update(cx, |input, cx| {
            input.set_placeholder(crate::locale::text("替换为", "Replace with"), window, cx)
        });
        cx.notify();
    }

    /// 只为已打开标签的唯一目标查询名称；标签关闭或重开时由 key+generation 丢弃旧回执。
    ///
    /// @author kongweiguang
    fn sync_target_name_requests(&mut self, cx: &mut Context<Self>) {
        let endpoints = self
            .tabs
            .iter()
            .map(|tab| tab.endpoint.clone())
            .collect::<Vec<_>>();
        let requests = self
            .target_names
            .synchronize(endpoints, &self.target_name_lookup);
        for request in requests {
            let lookup = self.target_name_lookup.clone();
            let executor = self.executor.clone();
            let key = request.key;
            let query_key = key.clone();
            let generation = request.generation;
            cx.spawn(async move |this, cx| {
                let result = executor
                    .run_cancellable(
                        request.cancellation,
                        move |operation_cancellation| async move {
                            lookup.resolve(query_key, operation_cancellation).await
                        },
                    )
                    .await;
                let _ = this.update(cx, |workspace, cx| {
                    if workspace.target_names.complete(&key, generation, result) {
                        cx.notify();
                    }
                });
            })
            .detach();
        }
    }

    /// 保存操作使用单调变化的本地代次，迟到收据不能改写重开后的同路径文件标签。
    pub(super) fn reserve_save_generation(&mut self) -> u64 {
        let generation = self.next_save_generation;
        self.next_save_generation = self.next_save_generation.wrapping_add(1).max(1);
        generation
    }

    /// 同一路径复用已有 tab；活动项与加载状态改变时发布带版本投影，迟到的 Files 查询仍由其请求代次隔离。
    ///
    /// @author kongweiguang
    pub fn open_file(&mut self, endpoint: FileEndpoint, cx: &mut Context<Self>) {
        if let Some(index) = self.tabs.iter().position(|tab| tab.endpoint == endpoint) {
            self.active_endpoint = Some(endpoint.clone());
            self.publish_workspace_projection_if_changed(cx);
            self.sync_target_name_requests(cx);
            let should_retry = self.tabs[index].session.is_none() && !self.tabs[index].loading;
            if should_retry {
                self.start_load(endpoint, false, cx);
            } else {
                let query = self.current_search_query(cx);
                self.sync_search_query(query, cx);
                cx.notify();
            }
            return;
        }

        self.clear_search_projection(cx);
        let generation = self.reserve_load_generation();
        let title = endpoint_title(&endpoint);
        self.tabs.push(EditorTab {
            endpoint: endpoint.clone(),
            title,
            session: None,
            editor: None,
            loading: true,
            load_generation: generation,
            document_generation: 0,
            load_task: None,
            close_after_readback: ReadbackCloseIntent::None,
            dirty: false,
            workspace_readonly: false,
            error: None,
            editor_subscription: None,
            search_decorations: None,
            save_operation: None,
        });
        self.active_endpoint = Some(endpoint.clone());
        self.sync_target_name_requests(cx);
        self.spawn_load(endpoint, generation, cx);
        self.publish_workspace_projection_if_changed(cx);
        cx.notify();
    }

    /// Workspace v3 文件按持久标题和权限打开；只读别名不能解锁共享缓冲，且标题/权限变化需同步 Files 快照。
    ///
    /// @author kongweiguang
    pub fn open_workspace_file(
        &mut self,
        endpoint: FileEndpoint,
        title: String,
        readonly: bool,
        cx: &mut Context<Self>,
    ) -> AppResult<()> {
        if !self.tabs.iter().any(|tab| tab.endpoint == endpoint) {
            self.open_file(endpoint.clone(), cx);
        }

        let Some(index) = self.tabs.iter().position(|tab| tab.endpoint == endpoint) else {
            return Err(AppError::not_found("工作区文件标签未创建"));
        };
        let tab = &mut self.tabs[index];
        if readonly && !tab.workspace_readonly && tab.dirty {
            return Err(AppError::conflict(
                "文件已有未保存修改，不能在保留修改的同时切为工作区只读状态",
            ));
        }

        tab.workspace_readonly = readonly;
        if !title.trim().is_empty() {
            tab.title = title;
        }
        self.active_endpoint = Some(endpoint);
        if let (Some(editor), Some(session)) = (&tab.editor, &tab.session) {
            let readonly = effective_readonly(session.is_readonly(), tab.workspace_readonly);
            editor.update(cx, |state, cx| state.set_readonly(readonly, cx));
        }
        self.publish_workspace_projection_if_changed(cx);
        cx.notify();
        Ok(())
    }

    /// 多个 Workspace Tab 可映射同一文档；只在汇总权限实际变化后同步只读投影。
    ///
    /// @author kongweiguang
    pub fn set_workspace_file_readonly(
        &mut self,
        endpoint: &FileEndpoint,
        readonly: bool,
        cx: &mut Context<Self>,
    ) -> AppResult<()> {
        let Some(tab) = self.tabs.iter_mut().find(|tab| &tab.endpoint == endpoint) else {
            return Err(AppError::not_found("工作区文件标签不存在"));
        };
        if readonly && !tab.workspace_readonly && tab.dirty {
            return Err(AppError::conflict(
                "文件已有未保存修改，不能在保留修改的同时切为工作区只读状态",
            ));
        }
        tab.workspace_readonly = readonly;
        if let (Some(editor), Some(session)) = (&tab.editor, &tab.session) {
            let effective_readonly = effective_readonly(session.is_readonly(), readonly);
            editor.update(cx, |state, cx| state.set_readonly(effective_readonly, cx));
        }
        self.publish_workspace_projection_if_changed(cx);
        cx.notify();
        Ok(())
    }

    /// 关闭确认和异步读回按 endpoint/代次关联，且普通提示保持独立于保存成功标记。
    ///
    /// @author kongweiguang
    pub fn request_close(&mut self, endpoint: FileEndpoint, cx: &mut Context<Self>) {
        let load_generation = self
            .tabs
            .iter()
            .find(|tab| tab.endpoint == endpoint)
            .map(|tab| tab.load_generation);
        if self
            .confirmation
            .as_ref()
            .is_some_and(|confirmation| confirmation_endpoint(confirmation) != &endpoint)
        {
            self.status = Some(EditorStatus::Message(
                crate::locale::text(
                    "请先完成当前文件的关闭确认。",
                    "Resolve the current file confirmation before closing another file.",
                )
                .to_string(),
            ));
            self.resolve_workspace_close(&endpoint, load_generation, false, cx);
            cx.notify();
            return;
        }
        let Some(index) = self.tabs.iter().position(|tab| tab.endpoint == endpoint) else {
            self.resolve_workspace_close(&endpoint, None, true, cx);
            return;
        };
        let (tab_generation, loading_existing_session, outcome_unknown) = {
            let tab = &self.tabs[index];
            (
                tab.load_generation,
                tab.loading && tab.session.is_some(),
                tab.session
                    .as_ref()
                    .is_some_and(|session| session.phase() == EditorPhase::OutcomeUnknown),
            )
        };
        if loading_existing_session {
            self.status = Some(EditorStatus::Message(
                crate::locale::text(
                    "正在重新读取文件，完成后再关闭",
                    "The file is being reloaded. Close it after the read finishes.",
                )
                .to_string(),
            ));
            self.resolve_workspace_close(&endpoint, Some(tab_generation), false, cx);
            cx.notify();
            return;
        }
        if outcome_unknown {
            self.tabs[index].close_after_readback.arm();
            self.confirmation = Some(EditorConfirmation::Reload {
                endpoint,
                continue_quit: false,
                close_after_readback: true,
            });
            self.status = None;
            cx.notify();
            return;
        }

        let decision = self
            .tabs
            .iter()
            .find(|tab| tab.endpoint == endpoint)
            .and_then(|tab| {
                Some(
                    tab.session.as_ref()?.close_decision(
                        &tab.editor
                            .as_ref()?
                            .update(cx, |state, _| state.text().to_string()),
                    ),
                )
            });

        match decision {
            None | Some(CloseDecision::CloseNow) => self.remove_tab(&endpoint, cx),
            Some(CloseDecision::ConfirmUnsavedChanges) => {
                self.confirmation = Some(EditorConfirmation::Close(endpoint));
            }
            Some(CloseDecision::WaitForSave) => {
                self.status = Some(EditorStatus::Message(
                    crate::locale::text(
                        "保存完成前无法关闭此文件",
                        "Wait for the save to finish before closing this file.",
                    )
                    .to_string(),
                ));
                self.resolve_workspace_close(&endpoint, Some(tab_generation), false, cx);
            }
        }
        cx.notify();
    }

    /// 保留 Root operation owner，并把当前读取代次作为关闭回执的匹配边界。
    pub fn request_workspace_close(
        &mut self,
        endpoint: FileEndpoint,
        operation_id: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(pending_close) = &self.pending_workspace_close {
            let duplicate =
                pending_close.operation_id == operation_id && pending_close.endpoint == endpoint;
            if !duplicate {
                cx.emit(FileEditorWorkspaceEvent::WorkspaceTabCloseResolved {
                    operation_id,
                    endpoint,
                    closed: false,
                });
            }
            return;
        }
        self.pending_workspace_close = Some(PendingWorkspaceClose {
            operation_id,
            endpoint: endpoint.clone(),
            load_generation: self
                .tabs
                .iter()
                .find(|tab| tab.endpoint == endpoint)
                .map(|tab| tab.load_generation),
        });
        self.request_close(endpoint, cx);
    }

    /// 仅匹配原 endpoint 和 load generation 后回报结果，Root 才可提交同一关闭操作。
    pub(super) fn resolve_workspace_close(
        &mut self,
        endpoint: &FileEndpoint,
        generation: Option<u64>,
        closed: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(event) = take_workspace_close_resolution(
            &mut self.pending_workspace_close,
            endpoint,
            generation,
            closed,
        ) else {
            return;
        };
        cx.emit(event);
    }

    /// 退出先以未关闭收据结清原 workspace close owner，避免确认换到另一文件后丢失原始回执。
    /// 只撤销关闭意图并保留正文、脏状态和在途任务，再沿既有退出门禁逐文件处理。
    ///
    /// @author kongweiguang
    pub fn request_quit(&mut self, cx: &mut Context<Self>) -> bool {
        if let Some(event) = take_workspace_close_cancellation(&mut self.pending_workspace_close) {
            if let FileEditorWorkspaceEvent::WorkspaceTabCloseResolved { endpoint, .. } = &event {
                if self
                    .confirmation
                    .as_ref()
                    .is_some_and(|confirmation| confirmation_endpoint(confirmation) == endpoint)
                {
                    self.confirmation = None;
                }
                if let Some(tab) = self.tabs.iter_mut().find(|tab| &tab.endpoint == endpoint) {
                    tab.close_after_readback.clear();
                }
            }
            cx.emit(event);
        }
        if self.quit_gate() == EditorQuitGate::Ready {
            self.clear_search_projection(cx);
            self.tabs.clear();
            self.active_endpoint = None;
            self.publish_workspace_projection_if_changed(cx);
            self.confirmation = None;
            self.quit_flow = false;
            cx.notify();
            return true;
        }
        self.quit_flow = true;
        self.advance_quit(cx);
        cx.notify();
        false
    }

    /// 查询未保存标签供应用根阻断窗口退出；加载和保存状态由 quit 流程另行等待。
    pub fn has_unsaved_changes(&self) -> bool {
        self.tabs.iter().any(|tab| tab.dirty)
    }

    /// 按顺序等待加载、保存和未知提交读回，再对脏内容逐文件征求决定。
    ///
    /// @author kongweiguang
    pub(super) fn advance_quit(&mut self, cx: &mut Context<Self>) {
        match self.quit_gate() {
            EditorQuitGate::WaitingForLoad => {
                self.status = Some(EditorStatus::Message(
                    crate::locale::text(
                        "正在读取文件，完成后继续退出",
                        "Reading files. The app will quit when loading finishes.",
                    )
                    .to_string(),
                ));
                return;
            }
            EditorQuitGate::WaitingForSave => {
                self.status = Some(EditorStatus::Message(
                    crate::locale::text(
                        "正在保存文件，完成后继续退出",
                        "Saving files. The app will quit when saving finishes.",
                    )
                    .to_string(),
                ));
                return;
            }
            EditorQuitGate::ReadbackRequired => {
                if let Some(endpoint) = self
                    .tabs
                    .iter()
                    .find(|tab| {
                        tab.session
                            .as_ref()
                            .is_some_and(|session| session.phase() == EditorPhase::OutcomeUnknown)
                    })
                    .map(|tab| tab.endpoint.clone())
                {
                    self.confirmation = Some(EditorConfirmation::Reload {
                        endpoint,
                        continue_quit: true,
                        close_after_readback: false,
                    });
                    self.status = None;
                    cx.notify();
                    return;
                }
            }
            EditorQuitGate::ConfirmUnsaved => {
                if let Some(endpoint) = self
                    .tabs
                    .iter()
                    .find(|tab| tab.dirty)
                    .map(|tab| tab.endpoint.clone())
                {
                    self.confirmation = Some(EditorConfirmation::Quit(endpoint));
                    self.status = None;
                    cx.notify();
                    return;
                }
            }
            EditorQuitGate::Ready => {}
        }

        self.clear_search_projection(cx);
        self.tabs.clear();
        self.active_endpoint = None;
        self.publish_workspace_projection_if_changed(cx);
        self.confirmation = None;
        self.quit_flow = false;
        cx.emit(FileEditorWorkspaceEvent::QuitReady);
        cx.notify();
    }

    /// 取消前保存被取消代次、先换代再丢任务，并为同一关闭 owner 回报失败收据。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_load(&mut self, endpoint: FileEndpoint, cx: &mut Context<Self>) {
        let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.endpoint == endpoint && tab.loading)
        else {
            return;
        };
        let cancelled_generation = self.tabs[index].load_generation;
        let generation = self.reserve_load_generation();
        let tab = &mut self.tabs[index];

        let cancelled_task = tab.load_task.take();
        tab.load_generation = generation;
        tab.loading = false;
        tab.close_after_readback.cancel_load(cancelled_generation);
        let message = crate::locale::text(
            "文件读取已取消；当前内容没有被替换。",
            "File loading was cancelled. The current contents were kept.",
        )
        .to_string();
        tab.error = Some(message.clone());
        if let (Some(editor), Some(session)) = (&tab.editor, &tab.session) {
            let readonly = effective_readonly(session.is_readonly(), tab.workspace_readonly);
            editor.update(cx, |state, cx| state.set_readonly(readonly, cx));
        }
        drop(cancelled_task);

        self.resolve_workspace_close(&endpoint, Some(cancelled_generation), false, cx);

        if self.active_endpoint.as_ref() == Some(&endpoint) && self.search_visible() {
            let query = self.current_search_query(cx);
            self.sync_search_query(query, cx);
        }

        self.status = Some(EditorStatus::Message(message));
        if self.quit_flow {
            self.quit_flow = false;
            self.confirmation = None;
            cx.emit(FileEditorWorkspaceEvent::QuitCancelled);
        }
        self.publish_workspace_projection_if_changed(cx);
        cx.notify();
    }

    /// 删除指定 endpoint 并仅完成其当前代次的关闭 owner；无文件时仅清理语义标记为成功的保存提示。
    ///
    /// @author kongweiguang
    pub(super) fn remove_tab(&mut self, endpoint: &FileEndpoint, cx: &mut Context<Self>) {
        let was_active = self.active_endpoint.as_ref() == Some(endpoint);
        if was_active {
            self.clear_search_projection(cx);
        }
        let load_generation = self
            .tabs
            .iter()
            .find(|tab| &tab.endpoint == endpoint)
            .map(|tab| tab.load_generation);
        self.tabs.retain(|tab| &tab.endpoint != endpoint);
        if self.active_endpoint.as_ref() == Some(endpoint) {
            self.active_endpoint = self.tabs.last().map(|tab| tab.endpoint.clone());
        }
        self.sync_target_name_requests(cx);
        if self
            .confirmation
            .as_ref()
            .is_some_and(|confirmation| confirmation_endpoint(confirmation) == endpoint)
        {
            self.confirmation = None;
        }
        self.resolve_workspace_close(endpoint, load_generation, true, cx);
        if was_active && self.tabs.is_empty() {
            // 搜索输入仍保存在同一实体中，之后打开文件可继续使用；这里只关闭没有目标的控件。
            self.search_panel_mode = SearchPanelMode::Closed;
            if matches!(self.status.as_ref(), Some(EditorStatus::SaveCompleted)) {
                self.status = None;
            }
        } else if was_active && self.search_visible() {
            let query = self.current_search_query(cx);
            self.sync_search_query(query, cx);
        }
        self.publish_workspace_projection_if_changed(cx);
    }

    /// 汇总异步任务和脏状态，确保关闭快路与等待流程使用同一优先级。
    fn quit_gate(&self) -> EditorQuitGate {
        evaluate_editor_quit_gate(
            self.tabs.iter().any(|tab| tab.loading),
            self.tabs.iter().any(|tab| {
                tab.session
                    .as_ref()
                    .is_some_and(|session| session.phase() == EditorPhase::Saving)
            }),
            self.tabs.iter().any(|tab| {
                tab.session
                    .as_ref()
                    .is_some_and(|session| session.phase() == EditorPhase::OutcomeUnknown)
            }),
            self.has_unsaved_changes(),
        )
    }

    /// 返回当前 tab 的只读引用，供保存、搜索和渲染集中使用同一活动端点判定。
    pub(super) fn active_tab(&self) -> Option<&EditorTab> {
        let endpoint = self.active_endpoint.as_ref()?;
        self.tabs.iter().find(|tab| &tab.endpoint == endpoint)
    }

    /// 工具隐藏后只聚焦已加载的活动正文，保留光标与选择，不借焦点恢复重新打开或读取文件。
    ///
    /// @author kongweiguang
    pub(crate) fn focus_active_editor(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(editor) = self.active_tab().and_then(|tab| tab.editor.as_ref()) else {
            return false;
        };
        editor.update(cx, |state, cx| state.focus(window, cx));
        true
    }

    /// 返回当前查找输入实体供 render 子模块构造 GPUI Kit Input。
    pub(super) fn find_input(&self) -> &Entity<InputState> {
        &self.find_input
    }

    /// 返回当前替换输入实体供 render 子模块构造 GPUI Kit Input。
    pub(super) fn replace_input(&self) -> &Entity<InputState> {
        &self.replace_input
    }

    /// 返回标签集合供 render 子模块一次性绘制，业务状态仍由工作区持有。
    pub(super) fn tabs(&self) -> &[EditorTab] {
        &self.tabs
    }

    /// 投影真实打开文件身份与状态，供 Root 同步到 Files；不复制正文或编辑器句柄。
    ///
    /// @author kongweiguang
    pub(crate) fn workspace_projection(&self) -> EditorWorkspaceProjection {
        let active_endpoint = self.active_endpoint.clone();
        let tabs = self
            .tabs
            .iter()
            .map(|tab| EditorTabProjection {
                endpoint: tab.endpoint.clone(),
                title: tab.title.clone(),
                dirty: tab.dirty,
                readonly: tab.is_readonly(),
                loading: tab.loading,
                active: active_endpoint.as_ref() == Some(&tab.endpoint),
            })
            .collect();

        EditorWorkspaceProjection {
            revision: self.projection_revision,
            tabs,
            active_endpoint,
        }
    }

    /// 只在文件身份、活动项、只读、加载或脏状态改变时推送；搜索与光标更新不广播。
    ///
    /// @author kongweiguang
    pub(super) fn publish_workspace_projection_if_changed(&mut self, cx: &mut Context<Self>) {
        let current = self.workspace_projection();
        if !workspace_projection_content_changed(&self.last_published_projection, &current) {
            return;
        }
        self.projection_revision = self.projection_revision.saturating_add(1);
        self.last_published_projection = self.workspace_projection();
        cx.emit(FileEditorWorkspaceEvent::WorkspaceProjectionChanged);
    }

    /// 返回当前 endpoint 供 render 子模块确定活动 tab。
    pub(super) fn active_endpoint(&self) -> Option<&FileEndpoint> {
        self.active_endpoint.as_ref()
    }

    /// 根层只读取待确认状态，正文、保存和退出意图仍由同一编辑器 Entity 持有。
    /// @author kongweiguang
    pub(crate) fn root_confirmation_active(&self) -> bool {
        self.confirmation.is_some()
    }

    /// 根层 Escape 或遮罩只撤销原待确认意图，复用退出与 workspace close 的既有收据处理。
    /// @author kongweiguang
    pub(crate) fn cancel_root_confirmation(&mut self, cx: &mut Context<Self>) {
        self.cancel_confirmation(cx);
    }

    /// 返回确认状态的不可变快照，避免渲染层直接改变业务状态。
    pub(super) fn confirmation(&self) -> Option<&EditorConfirmation> {
        self.confirmation.as_ref()
    }

    /// 仅在渲染时本地化保存成功标记，普通错误和警告继续使用写入时的用户语言文案。
    ///
    /// @author kongweiguang
    pub(super) fn status(&self) -> Option<String> {
        match self.status.as_ref()? {
            EditorStatus::Message(message) => Some(message.clone()),
            EditorStatus::SaveCompleted => {
                Some(crate::locale::text("保存完成", "Saved").to_string())
            }
        }
    }

    /// 切换活动标签后重建该文档搜索范围，并同步活动项快照。
    ///
    /// @author kongweiguang
    pub(super) fn activate(&mut self, endpoint: FileEndpoint, cx: &mut Context<Self>) {
        self.active_endpoint = Some(endpoint);
        let query = self.current_search_query(cx);
        self.sync_search_query(query, cx);
        self.publish_workspace_projection_if_changed(cx);
        cx.notify();
    }

    /// 只激活仍存在的文件标签；Root 的迟到行点击不能隐式重新打开或重读已关闭文件。
    ///
    /// @author kongweiguang
    pub(crate) fn activate_open_file(
        &mut self,
        endpoint: FileEndpoint,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.tabs.iter().any(|tab| tab.endpoint == endpoint) {
            return false;
        }
        self.activate(endpoint, cx);
        true
    }

    /// 工具栏切换替换区后明确聚焦查找框；无已加载编辑器时不创建无目标搜索状态。
    ///
    /// @author kongweiguang
    pub(super) fn toggle_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.active_tab().is_some_and(|tab| tab.editor.is_some()) {
            return;
        }
        let opening = !self.search_panel_mode.is_visible();
        self.search_panel_mode = self.search_panel_mode.toggle_replace();
        if opening {
            let query = self.current_search_query(cx);
            self.sync_search_query(query, cx);
        }
        self.find_input
            .update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// Ctrl/Cmd+F 只对已加载编辑器打开查找行；没有目标时保留查询且不改变焦点。
    ///
    /// @author kongweiguang
    pub(super) fn show_find_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.active_tab().is_some_and(|tab| tab.editor.is_some()) {
            return;
        }
        let opening = !self.search_panel_mode.is_visible();
        self.search_panel_mode = self.search_panel_mode.show_find();
        if opening {
            let query = self.current_search_query(cx);
            self.sync_search_query(query, cx);
        }
        self.find_input
            .update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// Ctrl/Cmd+H 只对已加载编辑器展开替换栏；没有目标时不把焦点交给隐藏输入。
    ///
    /// @author kongweiguang
    pub(super) fn show_replace_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.active_tab().is_some_and(|tab| tab.editor.is_some()) {
            return;
        }
        let opening = !self.search_panel_mode.is_visible();
        self.search_panel_mode = self.search_panel_mode.show_replace();
        if opening {
            let query = self.current_search_query(cx);
            self.sync_search_query(query, cx);
        }
        self.replace_input
            .update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// 关闭查找时使后台查询代次失效并清理范围，避免关闭后迟到结果重新显示。
    pub(super) fn close_search(&mut self, cx: &mut Context<Self>) {
        if !self.search_panel_mode.is_visible() {
            return;
        }
        self.search_panel_mode = self.search_panel_mode.close();
        self.clear_search_projection(cx);
        cx.notify();
    }

    /// 面板显隐状态供 render 子模块决定是否展示查找替换控件。
    pub(super) fn search_visible(&self) -> bool {
        self.search_panel_mode.is_visible()
    }

    /// 替换输入行只在显式展开或 Ctrl/Cmd+H 时呈现。
    pub(super) fn replace_visible(&self) -> bool {
        self.search_panel_mode.shows_replace()
    }

    /// 共享加载、权限和编辑器初始化门禁，避免按钮态与替换处理器判断不一致。
    pub(super) fn can_replace_active(&self) -> bool {
        self.active_tab()
            .is_some_and(|tab| can_replace(tab.loading, tab.is_readonly(), tab.editor.is_some()))
    }

    /// 替换按钮只在加载完成、可写且存在有效命中时启用。
    pub(super) fn can_replace_match(&self) -> bool {
        self.can_replace_active()
            && self.search_error.is_none()
            && self.search_activity == SearchActivity::Idle
            && self.search_current_range.is_some()
            && self
                .search_results
                .as_ref()
                .is_some_and(|results| results.total_matches() > 0)
    }

    /// 待确认时先拦截正文快捷键；其它情况下 Shift+Insert 复用 Paste，保留 IME 输入路径。
    ///
    /// @author kongweiguang
    pub(super) fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let modifiers = event.keystroke.modifiers;
        let key = event.keystroke.key.to_ascii_lowercase();
        // 挂载根焦点 trap 前也不能执行正文快捷键；确认本身仅接管 Escape。
        if self.confirmation.is_some() {
            if key == "escape" {
                self.cancel_confirmation(cx);
            }
            cx.stop_propagation();
            return;
        }
        if is_shift_insert_paste(event) {
            let can_paste = self.active_tab().is_some_and(|tab| {
                !tab.loading
                    && !tab.is_readonly()
                    && tab.session.as_ref().is_some_and(|session| {
                        !matches!(
                            session.phase(),
                            EditorPhase::Conflict | EditorPhase::OutcomeUnknown
                        )
                    })
                    && tab.editor.is_some()
            });
            if can_paste {
                window.dispatch_action(Box::new(Paste), cx);
            }
            cx.stop_propagation();
        } else if modifiers.secondary() {
            match key.as_str() {
                "s" => {
                    self.save_active(SaveResolution::CompareRevision, false, false, cx);
                    cx.stop_propagation();
                }
                "f" => {
                    self.show_find_search(window, cx);
                    cx.stop_propagation();
                }
                "h" => {
                    self.show_replace_search(window, cx);
                    cx.stop_propagation();
                }
                _ => {}
            }
        } else if key == "escape" && self.search_visible() {
            self.close_search(cx);
            cx.stop_propagation();
        }
    }

    /// 收到当前确认框的保存选项后按预期 revision 保存，成功才关闭并推进整体退出流程。
    pub(super) fn confirm_save_and_close(
        &mut self,
        endpoint: FileEndpoint,
        quit: bool,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.iter().any(|tab| {
            tab.endpoint == endpoint
                && tab
                    .session
                    .as_ref()
                    .is_some_and(|session| session.phase() == EditorPhase::Conflict)
        }) {
            self.confirmation = Some(EditorConfirmation::Overwrite {
                endpoint,
                close_after_success: true,
                quit_after_success: quit,
            });
            cx.notify();
            return;
        }
        self.confirmation = None;
        self.active_endpoint = Some(endpoint.clone());
        self.publish_workspace_projection_if_changed(cx);
        self.save_endpoint(endpoint, SaveResolution::CompareRevision, true, quit, cx);
    }

    /// 用户明确选择放弃编辑时清除该 tab，再按需处理剩余退出目标。
    pub(super) fn confirm_discard_and_close(
        &mut self,
        endpoint: FileEndpoint,
        quit: bool,
        cx: &mut Context<Self>,
    ) {
        self.confirmation = None;
        self.remove_tab(&endpoint, cx);
        if quit || self.quit_flow {
            self.advance_quit(cx);
        }
        cx.notify();
    }

    /// 继续退出的覆盖动作必须先经过单独确认，不会把普通保存按钮解释为覆盖授权。
    pub(super) fn confirm_overwrite(
        &mut self,
        endpoint: FileEndpoint,
        close_after_success: bool,
        quit_after_success: bool,
        cx: &mut Context<Self>,
    ) {
        self.confirmation = None;
        self.active_endpoint = Some(endpoint.clone());
        self.publish_workspace_projection_if_changed(cx);
        self.save_endpoint(
            endpoint,
            SaveResolution::OverwriteAfterConflict,
            close_after_success,
            quit_after_success,
            cx,
        );
    }

    /// 只有用户确认的读回才能绑定关闭 generation，普通重试不会继承该意图。
    pub(super) fn confirm_reload(
        &mut self,
        endpoint: FileEndpoint,
        continue_quit: bool,
        close_after_readback: bool,
        cx: &mut Context<Self>,
    ) {
        self.confirmation = None;
        if continue_quit {
            self.quit_flow = true;
        }
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.endpoint == endpoint) {
            if close_after_readback {
                tab.close_after_readback.arm();
            } else {
                tab.close_after_readback.clear();
            }
        }
        self.active_endpoint = Some(endpoint.clone());
        self.start_load(endpoint, close_after_readback, cx);
    }

    /// 重载确认只呈现当前文档尚未终结的读回关闭意图，失败后的旧意图不会复活。
    pub(super) fn request_reload(&mut self, endpoint: FileEndpoint, cx: &mut Context<Self>) {
        let close_after_readback = self
            .tabs
            .iter()
            .find(|tab| tab.endpoint == endpoint)
            .is_some_and(|tab| tab.close_after_readback.is_pending());
        self.confirmation = Some(EditorConfirmation::Reload {
            endpoint,
            continue_quit: self.quit_flow,
            close_after_readback,
        });
        cx.notify();
    }

    /// 标签菜单重载只向既有确认流提交请求；显式代次用于拒绝内部 editor tab 的迟到右键动作。
    /// None 仅供 Root 已核验的持久 WorkspaceFile 目标；不自动保存或重试未知写入。
    /// 任一标签的在途保存都可能在回执中请求关闭/覆盖确认，不能让新重载占用同一确认槽。
    /// 只有具备真实缓冲与 Ready 基线的干净标签才跳过“丢弃修改”确认，其余情况保留既有确认。
    ///
    /// @author kongweiguang
    pub(crate) fn request_user_reload(
        &mut self,
        endpoint: FileEndpoint,
        expected_generation: Option<u64>,
        cx: &mut Context<Self>,
    ) -> bool {
        let tab_state = self
            .tabs
            .iter()
            .find(|tab| tab.endpoint == endpoint)
            .map(|tab| {
                (
                    tab.load_generation,
                    tab.loading,
                    tab.dirty,
                    tab.editor.is_some(),
                    tab.session.as_ref().map(|session| session.phase()),
                    tab.close_after_readback.is_pending(),
                )
            });
        let Some((generation, tab_loading, tab_dirty, has_editor, phase, close_readback_pending)) =
            tab_state
        else {
            self.status = Some(EditorStatus::Message(
                crate::locale::text(
                    "文件标签已关闭；请重新打开后重试。",
                    "The file tab is closed. Reopen it and try again.",
                )
                .to_string(),
            ));
            cx.notify();
            return false;
        };
        if expected_generation
            .is_some_and(|expected| !editor_tab_generation_matches(Some(generation), expected))
        {
            self.status = Some(EditorStatus::Message(
                crate::locale::text(
                    "文件标签已重新读取或重开；请从当前标签重试。",
                    "The file tab was reloaded or reopened. Retry from the current tab.",
                )
                .to_string(),
            ));
            cx.notify();
            return false;
        }
        if !user_reload_request_is_available(
            self.confirmation.is_some(),
            self.pending_workspace_close.is_some() || self.quit_flow,
            tab_loading,
            self.tabs
                .iter()
                .map(|tab| tab.session.as_ref().map(|session| session.phase())),
        ) {
            self.status = Some(EditorStatus::Message(
                crate::locale::text(
                    "编辑器正忙或已有确认未完成；请先完成当前操作。",
                    "The editor is busy or another confirmation is pending. Finish it first.",
                )
                .to_string(),
            ));
            cx.notify();
            return false;
        }
        if clean_tab_can_reload_without_confirmation(
            phase,
            tab_dirty,
            has_editor,
            tab_loading,
            close_readback_pending,
        ) {
            // 复用确认后的只读读回执行路径；前面的代次及全部 owner/pending guard 仍然有效。
            self.confirm_reload(endpoint, false, false, cx);
        } else {
            // 脏、冲突、未知或不完整会话仍由原模态明确确认，不写入、不自动重试。
            self.request_reload(endpoint, cx);
        }
        true
    }

    /// 只复制菜单打开时那一代文件标签的 endpoint 路径，防止重开同路径后迟到动作误指新标签。
    ///
    /// @author kongweiguang
    pub(super) fn copy_file_path_for_generation(
        &mut self,
        endpoint: FileEndpoint,
        generation: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        let current_generation = self
            .tabs
            .iter()
            .find(|tab| tab.endpoint == endpoint)
            .map(|tab| tab.load_generation);
        if !editor_tab_generation_matches(current_generation, generation) {
            self.status = Some(EditorStatus::Message(
                crate::locale::text(
                    "文件标签已变化；请从当前标签重新打开菜单。",
                    "The file tab changed. Reopen the menu on the current tab.",
                )
                .to_string(),
            ));
            cx.notify();
            return false;
        }
        let Some(path) = editor_file_full_path(&endpoint) else {
            self.status = Some(EditorStatus::Message(
                crate::locale::text(
                    "此本地路径无法无损转换为剪贴板文本。",
                    "This local path cannot be represented losslessly as clipboard text.",
                )
                .to_string(),
            ));
            cx.notify();
            return false;
        };
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(path));
        self.status = Some(EditorStatus::Message(
            crate::locale::text("已复制文件完整路径。", "Copied the full file path.").to_string(),
        ));
        cx.notify();
        true
    }

    /// 取消确认时清除对应读回意图并只终结相同 endpoint/代次的 Root close owner。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_confirmation(&mut self, cx: &mut Context<Self>) {
        let cancelled_readback_close = match self.confirmation.as_ref() {
            Some(EditorConfirmation::Reload {
                endpoint,
                close_after_readback: true,
                ..
            }) => Some(endpoint.clone()),
            _ => None,
        };
        let cancelled_quit = self.quit_flow
            || matches!(
                self.confirmation,
                Some(EditorConfirmation::Quit(_))
                    | Some(EditorConfirmation::Overwrite {
                        quit_after_success: true,
                        ..
                    })
            );
        if cancelled_quit {
            self.quit_flow = false;
            cx.emit(FileEditorWorkspaceEvent::QuitCancelled);
        }
        if let Some(endpoint) = cancelled_readback_close
            && let Some(tab) = self.tabs.iter_mut().find(|tab| tab.endpoint == endpoint)
        {
            tab.close_after_readback.clear();
        }
        let cancelled_workspace_close =
            self.pending_workspace_close
                .as_ref()
                .is_some_and(|pending_close| {
                    self.confirmation.as_ref().is_some_and(|confirmation| {
                        confirmation_endpoint(confirmation) == &pending_close.endpoint
                    })
                });
        if cancelled_workspace_close
            && let Some(pending_close) = self.pending_workspace_close.as_ref()
        {
            let endpoint = pending_close.endpoint.clone();
            let generation = pending_close.load_generation;
            self.resolve_workspace_close(&endpoint, generation, false, cx);
        }
        self.confirmation = None;
        cx.notify();
    }

    /// 冲突后的覆盖必须展示远端内容将被替换的确认提示。
    pub(super) fn request_overwrite(&mut self, endpoint: FileEndpoint, cx: &mut Context<Self>) {
        self.confirmation = Some(EditorConfirmation::Overwrite {
            endpoint,
            close_after_success: false,
            quit_after_success: false,
        });
        cx.notify();
    }
}

/// 只接管纯 Shift+Insert，AltGr/IME 与带其它修饰键的组合仍由 GPUI 默认路由处理。
///
/// @author kongweiguang
fn is_shift_insert_paste(event: &KeyDownEvent) -> bool {
    let modifiers = event.keystroke.modifiers;
    !event.prefer_character_input
        && event.keystroke.key.eq_ignore_ascii_case("insert")
        && modifiers.shift
        && !modifiers.control
        && !modifiers.alt
        && !modifiers.platform
        && !modifiers.function
}

#[cfg(test)]
#[path = "../../../tests/unit/editor_shift_insert.rs"]
mod shift_insert_tests;

impl EventEmitter<FileEditorWorkspaceEvent> for FileEditorWorkspace {}

/// 文件系统权限与 workspace 只读意图只能相交，避免工作区权限放宽 provider 限制。
pub(super) fn effective_readonly(file_readonly: bool, workspace_readonly: bool) -> bool {
    file_readonly || workspace_readonly
}

/// 只有读取已完成、正文可编辑且 GPUI 编辑器存在时才允许替换文本。
pub(super) fn can_replace(loading: bool, readonly: bool, has_editor: bool) -> bool {
    !loading && !readonly && has_editor
}

/// 只展示目标路径而不暴露连接配置或凭据，容器文件显示稳定的运行时产品名称。
///
/// @author kongweiguang
pub(super) fn endpoint_label(endpoint: &FileEndpoint) -> String {
    match endpoint {
        FileEndpoint::Local { path } => format!(
            "{} · {}",
            crate::locale::text("本机", "Local"),
            path.display()
        ),
        FileEndpoint::Remote { path, .. } => format!("SSH · {}", path.as_str()),
        FileEndpoint::Container {
            runtime,
            container_id,
            path,
            ..
        } => format!(
            "{} · {container_id} · {}",
            container_runtime_label(*runtime),
            path.as_str()
        ),
    }
}

/// 将容器运行时枚举映射为产品名称，避免编辑器文案依赖调试格式。
///
/// @author kongweiguang
pub(super) fn container_runtime_label(runtime: ContainerRuntime) -> String {
    match runtime {
        ContainerRuntime::Docker => crate::locale::text("Docker", "Docker").to_string(),
        ContainerRuntime::Podman => crate::locale::text("Podman", "Podman").to_string(),
    }
}

/// 标签标题优先使用文件名，避免长的本地或远端绝对路径挤占工作区。
pub(super) fn endpoint_title(endpoint: &FileEndpoint) -> String {
    let name = match endpoint {
        FileEndpoint::Local { path } => path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
        FileEndpoint::Remote { path, .. } | FileEndpoint::Container { path, .. } => path
            .as_str()
            .rsplit('/')
            .find(|part| !part.is_empty())
            .map(str::to_owned),
    };
    name.unwrap_or_else(|| endpoint_label(endpoint))
}

/// 把领域迁移拒绝映射到简洁操作反馈，不向用户展示 Rust Debug 或协议底层文本。
pub(super) fn transition_message(error: EditorTransitionError) -> String {
    match error {
        EditorTransitionError::ReadOnly => crate::locale::text(
            "此文件为只读或内容不完整，不能保存",
            "This file is read-only or incomplete and cannot be saved.",
        )
        .to_string(),
        EditorTransitionError::ContentTooLarge => crate::locale::text(
            "文件超过 10 MiB 编辑上限，请使用其他工具修改",
            "The file exceeds the 10 MiB editing limit. Use another tool to modify it.",
        )
        .to_string(),
        EditorTransitionError::SaveInProgress => crate::locale::text(
            "文件正在保存，请等待完成",
            "This file is being saved. Wait for the save to finish.",
        )
        .to_string(),
        EditorTransitionError::NothingToSave => crate::locale::text(
            "文件没有未保存的修改",
            "There are no unsaved changes to save.",
        )
        .to_string(),
        EditorTransitionError::ConflictResolutionRequired => crate::locale::text(
            "文件已发生变化，请重载或确认覆盖",
            "The file has changed. Reload it or explicitly confirm an overwrite.",
        )
        .to_string(),
        EditorTransitionError::OutcomeUnknownRequiresReload => crate::locale::text(
            "上次保存结果尚未确认，请重新读取文件后再继续",
            "The previous save result is unknown. Reload the file before continuing.",
        )
        .to_string(),
        EditorTransitionError::OverwriteRequiresConflict => crate::locale::text(
            "只有 revision 冲突后才能显式覆盖",
            "Explicit overwrite is available only after a revision conflict.",
        )
        .to_string(),
        EditorTransitionError::MissingPendingSave => crate::locale::text(
            "保存结果与当前文件状态不匹配，请重新加载",
            "The save result does not match the current file state. Reload the file.",
        )
        .to_string(),
        EditorTransitionError::ReloadWhileSaving => crate::locale::text(
            "保存完成前不能重新加载文件",
            "Wait for the save to finish before reloading this file.",
        )
        .to_string(),
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/editor_workspace_access.rs"]
mod tests;

#[cfg(test)]
#[path = "../../../tests/unit/editor_workspace_close_readback.rs"]
mod readback_close_tests;

#[cfg(test)]
#[path = "../../../tests/unit/editor_search_panel.rs"]
mod search_panel_tests;

#[cfg(test)]
#[path = "../../../tests/unit/editor_workspace_projection.rs"]
mod projection_tests;

/// 返回确认框绑定的文件身份，关闭事件可在 tab 已移除后清理过期对话框。
fn confirmation_endpoint(confirmation: &EditorConfirmation) -> &FileEndpoint {
    match confirmation {
        EditorConfirmation::Close(endpoint)
        | EditorConfirmation::Quit(endpoint)
        | EditorConfirmation::Reload { endpoint, .. } => endpoint,
        EditorConfirmation::Overwrite { endpoint, .. } => endpoint,
    }
}

/// 右键重载只创建确认，不得覆盖其他确认、保存、读回关闭或尚未完成的加载。
///
/// @author kongweiguang
fn user_reload_request_is_available(
    confirmation_pending: bool,
    close_or_quit_pending: bool,
    tab_loading: bool,
    tab_phases: impl IntoIterator<Item = Option<EditorPhase>>,
) -> bool {
    !confirmation_pending
        && !close_or_quit_pending
        && !tab_loading
        && !tab_phases
            .into_iter()
            .any(|phase| phase == Some(EditorPhase::Saving))
}

/// 只有有编辑缓冲、未加载且无未决关闭读回的 Ready 干净标签可免丢弃确认；phase 本身不代表 Clean。
///
/// @author kongweiguang
fn clean_tab_can_reload_without_confirmation(
    phase: Option<EditorPhase>,
    dirty: bool,
    has_editor: bool,
    loading: bool,
    close_readback_pending: bool,
) -> bool {
    phase == Some(EditorPhase::Ready) && !dirty && has_editor && !loading && !close_readback_pending
}

/// 只接受右键打开时捕获的同一读取代次，防止同路径标签重开后的迟到菜单动作落到新文档。
///
/// @author kongweiguang
fn editor_tab_generation_matches(current_generation: Option<u64>, requested: u64) -> bool {
    current_generation == Some(requested)
}

/// 本地非 UTF-8 路径拒绝有损复制；远端与容器仅复制 endpoint 中的实际文件路径。
///
/// @author kongweiguang
fn editor_file_full_path(endpoint: &FileEndpoint) -> Option<String> {
    match endpoint {
        FileEndpoint::Local { path } => path.to_str().map(str::to_owned),
        FileEndpoint::Remote { path, .. } | FileEndpoint::Container { path, .. } => {
            Some(path.as_str().to_owned())
        }
    }
}

/// 只比较 Files 面板展示的标签快照，不让版本号自身造成重复投影事件。
///
/// @author kongweiguang
fn workspace_projection_content_changed(
    previous: &EditorWorkspaceProjection,
    current: &EditorWorkspaceProjection,
) -> bool {
    previous.tabs != current.tabs || previous.active_endpoint != current.active_endpoint
}

/// 退出抢占关闭确认时按原 owner 而非当前确认文件回报未关闭，只消费一次且不宣称文档已关闭。
/// 原 endpoint 与读取代次仍通过共用回执校验，不因 Quit(A) 覆盖 Close(B) 而遗失 B 的操作身份。
///
/// @author kongweiguang
fn take_workspace_close_cancellation(
    pending_close: &mut Option<PendingWorkspaceClose>,
) -> Option<FileEditorWorkspaceEvent> {
    let owner = pending_close.as_ref()?;
    let endpoint = owner.endpoint.clone();
    let generation = owner.load_generation;
    take_workspace_close_resolution(pending_close, &endpoint, generation, false)
}

/// 只消费与 endpoint/代次完全匹配的一次回执，迟到事件不会终结新的关闭意图。
fn take_workspace_close_resolution(
    pending_close: &mut Option<PendingWorkspaceClose>,
    endpoint: &FileEndpoint,
    generation: Option<u64>,
    closed: bool,
) -> Option<FileEditorWorkspaceEvent> {
    let event = pending_close
        .as_ref()?
        .resolution(endpoint, generation, closed)?;
    *pending_close = None;
    Some(event)
}
