//! RootView 的终端建议运行态桥接；输入投影和候选校验仍由唯一 controller 持有。
//!
//! Root 只传递可信 context、明确输入和当前设置；本模块统一连接取消、查询、ghost 与 CAS 接受。
//!
//! @author kongweiguang

pub use crate::features::commands::inline_suggestions::{
    InlineSuggestionController, TerminalSuggestionAcceptance,
};
mod menu;
use kerminal_application::{
    AppResult,
    commands::{CommandsService, TerminalSuggestionBatch, TerminalSuggestionQuery},
    executor::ApplicationTaskExecutor,
    preferences::{
        TerminalCommandSuggestionMenuShortcut, TerminalCommandSuggestionPresentation,
        TerminalInlineSuggestionAcceptKey, TerminalInlineSuggestionSettings,
    },
    sessions::{
        TerminalCommandContext, TerminalCommandContextVersion, UiInputProjection, UiSessionBinding,
        UiTerminalInputRequest,
    },
};
use kerminal_domain::commands::{
    CommandSuggestionActivation, CommandSuggestionCandidate, SuggestionQueryMode,
};
use kerminal_terminal::input::{KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyEvent};
pub use menu::{
    TerminalCommandSuggestionMenu, TerminalCommandSuggestionMenuEvent,
    TerminalCommandSuggestionMenuKey, TerminalSuggestionMenuProjection,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 一次 provider 查询及其 UI 代次取消源；新输入或 context 失效时由 controller 取消。
pub struct TerminalSuggestionJob {
    query: TerminalSuggestionQuery,
    request_cancellation: CancellationToken,
}

/// 可安全绘制的 inline ghost；session 与 prompt 代次用于避免跨 pane 复用视觉状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InlineSuggestionGhost {
    pub binding: kerminal_application::sessions::UiSessionBinding,
    pub expected_context: TerminalCommandContextVersion,
    pub request_generation: u64,
    pub candidate_id: String,
    /// 仅包含当前 Unicode 光标之后可接受的普通文本，不含终端控制序列。
    pub text: String,
}

/// 接受候选的单个输入回执状态；下一事件只在前一事件成功并更新 context 后才可取出。
pub enum AcceptanceInputProgress {
    /// 当前事件已写入，调用方可以从 returned context 请求下一个事件。
    Next { event_index: usize },
    /// 全部事件成功；可选 job 是按最终输入发起的一次新查询。
    Complete {
        follow_up: Option<TerminalSuggestionJob>,
    },
    /// 绑定、代次、事件次序或终端状态失效，调用方必须停止接受队列。
    Invalidated,
}

/// 当前建议查询在 UI 中的可见状态；错误只暴露通用状态，不把 provider 错误细节带入菜单。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalSuggestionQueryState {
    /// 没有打开的查询或菜单。
    #[default]
    Closed,
    /// 可信输入已提交查询，尚未收到本代次结果。
    Loading,
    /// 查询成功且至少有一个可呈现候选。
    Ready,
    /// 查询成功但没有符合安全策略的候选。
    Empty,
    /// 当前代次查询失败，可由后续输入或再次打开菜单重试。
    Failed,
}

struct PendingAcceptance {
    acceptance: TerminalSuggestionAcceptance,
    expected_context: TerminalCommandContextVersion,
    next_event_index: usize,
    input_in_flight: bool,
}

/// 让 Root 通过同一 controller 管理 terminal 建议生命周期，不缓存第二份输入或候选状态。
#[derive(Default)]
pub struct TerminalCommandSuggestionBridge {
    controller: InlineSuggestionController,
    settings: Option<TerminalInlineSuggestionSettings>,
    pending_acceptance: Option<PendingAcceptance>,
    query_state: TerminalSuggestionQueryState,
}

impl TerminalCommandSuggestionBridge {
    /// 首帧没有可信偏好快照时保持关闭，避免设置尚未载入就向用户输入附加行为。
    pub fn new() -> Self {
        Self::default()
    }

    /// Settings 变化时取消旧查询；缺失或禁用设置立即清除 ghost 和可接受候选。
    pub fn set_settings(&mut self, settings: Option<TerminalInlineSuggestionSettings>) {
        let changed = self.settings != settings;
        self.settings = settings;
        if changed || !self.is_enabled() {
            self.clear();
        }
    }

    /// Session 切换、关闭或未可信 runtime context 会取消查询；输入投影状态由 controller 单独维护。
    ///
    /// @author kongweiguang
    pub fn observe_context(&mut self, context: Option<&TerminalCommandContext>) -> bool {
        if !self.is_enabled() {
            self.clear();
            return false;
        }
        let Some(context) = context else {
            self.clear();
            return false;
        };
        if let Some(pending) = self.pending_acceptance.as_ref() {
            let binding = binding_for_context(context);
            if pending.acceptance.binding != binding
                || pending.expected_context != context.version()
            {
                self.clear();
                return false;
            }
        }
        self.controller.observe_context(context)
    }

    /// 只有明确允许的呈现模式和最新可信输入能创建请求；可信 context 决定当前提示符是否可查询。
    ///
    /// @author kongweiguang
    pub fn request(
        &mut self,
        context: &TerminalCommandContext,
        input: String,
        cursor: usize,
        mode: SuggestionQueryMode,
    ) -> Option<TerminalSuggestionJob> {
        if self.pending_acceptance.is_some() {
            self.clear();
            return None;
        }
        if !self.mode_enabled(mode) {
            self.clear();
            return None;
        }
        let Some((query, request_cancellation)) =
            self.controller.update_input(context, input, cursor, mode)
        else {
            self.query_state = TerminalSuggestionQueryState::Closed;
            return None;
        };
        self.query_state = TerminalSuggestionQueryState::Loading;
        Some(TerminalSuggestionJob {
            query,
            request_cancellation,
        })
    }

    /// 在 UI 输入成功写入且收到新 context 后更新唯一投影；无法模拟的编辑由 controller 清除投影。
    ///
    /// @author kongweiguang
    pub fn tracked_input_written(
        &mut self,
        context: &TerminalCommandContext,
        event: &TerminalInputEvent,
        mode: SuggestionQueryMode,
    ) -> Option<TerminalSuggestionJob> {
        if self.pending_acceptance.is_some() {
            self.clear();
            return None;
        }
        if !self.mode_enabled(mode) {
            self.clear();
            return None;
        }
        let Some((query, request_cancellation)) =
            self.controller.apply_tracked_input(context, event, mode)
        else {
            self.query_state = TerminalSuggestionQueryState::Closed;
            return None;
        };
        self.query_state = TerminalSuggestionQueryState::Loading;
        Some(TerminalSuggestionJob {
            query,
            request_cancellation,
        })
    }

    /// 打开菜单时基于 controller 已跟踪的文本和 cursor 查询，防止 Root 再维护一份输入副本。
    pub fn request_current(
        &mut self,
        context: &TerminalCommandContext,
        mode: SuggestionQueryMode,
    ) -> Option<TerminalSuggestionJob> {
        if self.pending_acceptance.is_some() {
            return None;
        }
        if !self.observe_context(Some(context)) {
            return None;
        }
        let (input, cursor) = self.controller.input_projection()?;
        self.request(context, input.to_owned(), cursor, mode)
    }

    /// 按键触发和显式入口共用一个菜单查询，只有能从可信输入投影创建 job 时才拦截按键。
    pub fn request_menu_on_key(
        &mut self,
        event: TerminalKeyEvent,
        context: &TerminalCommandContext,
    ) -> Option<TerminalSuggestionJob> {
        let settings = self.settings.as_ref()?;
        if !self.mode_enabled(SuggestionQueryMode::Menu) || event.phase != KeyPhase::Press {
            return None;
        }
        let ctrl_space = settings.menu_shortcut == TerminalCommandSuggestionMenuShortcut::CtrlSpace
            && event.key == TerminalKey::Character(' ')
            && event.modifiers.control
            && !event.modifiers.shift
            && !event.modifiers.alt
            && !event.modifiers.super_key;
        let tab = settings.tab_opens_menu
            && event.key == TerminalKey::Tab
            && event.modifiers == Default::default();
        if !ctrl_space && !tab {
            return None;
        }
        self.request_current(context, SuggestionQueryMode::Menu)
    }

    /// 供 Root 在菜单快捷键或工具入口触发查询；候选始终来自当前 controller 投影。
    pub fn request_menu(
        &mut self,
        context: &TerminalCommandContext,
    ) -> Option<TerminalSuggestionJob> {
        self.request_current(context, SuggestionQueryMode::Menu)
    }

    /// 菜单只在有效菜单查询代次内绘制，Loading/Empty/Failed 由 query_state 单独表达。
    pub fn menu_is_open(&self) -> bool {
        self.controller
            .current_query()
            .is_some_and(|query| query.mode == SuggestionQueryMode::Menu)
            && self.query_state != TerminalSuggestionQueryState::Closed
    }

    /// 菜单读取 controller 的唯一候选列表，非 Ready 状态不暴露可能过期的行。
    pub fn menu_candidates(&self) -> &[CommandSuggestionCandidate] {
        if self.menu_is_open() && self.query_state == TerminalSuggestionQueryState::Ready {
            self.controller.candidates()
        } else {
            &[]
        }
    }

    /// 为 GPUI 菜单生成短期只读快照；候选仍以 controller 当前 batch 为唯一业务来源。
    pub fn menu_projection(&self) -> Option<TerminalSuggestionMenuProjection> {
        let query = self.controller.current_query()?;
        if !self.menu_is_open() {
            return None;
        }

        Some(TerminalSuggestionMenuProjection {
            request_generation: query.request_generation,
            state: self.query_state,
            partial_accept: self
                .settings
                .as_ref()
                .is_some_and(|settings| settings.partial_accept),
            candidates: Arc::from(self.menu_candidates().to_vec()),
        })
    }

    /// 关闭菜单时取消当前 provider 查询，但保留输入投影以便继续输入或重开菜单。
    pub fn dismiss_menu(&mut self) -> bool {
        if !self.menu_is_open() {
            return false;
        }
        self.controller.cancel_query();
        self.query_state = TerminalSuggestionQueryState::Closed;
        true
    }

    /// 只把当前请求的失败标为 Failed，迟到的旧请求不能覆盖新菜单或新输入状态。
    pub fn fail_request(&mut self, request_generation: u64) -> bool {
        if self.query_state != TerminalSuggestionQueryState::Loading
            || !self.is_request_current(request_generation)
        {
            return false;
        }
        self.query_state = TerminalSuggestionQueryState::Failed;
        true
    }

    /// 仅在 ApplicationTaskExecutor 的受管 runtime 中查询，并同时传播 UI 代次与应用退出取消。
    pub async fn run(
        job: TerminalSuggestionJob,
        executor: ApplicationTaskExecutor,
        commands: Arc<CommandsService>,
    ) -> AppResult<TerminalSuggestionBatch> {
        executor
            .run(move |task_cancellation| async move {
                commands
                    .terminal_suggestions(job.query, job.request_cancellation, task_cancellation)
                    .await
            })
            .await
    }

    /// 仅接受与 controller 当前 binding、context 和 request generation 匹配的 batch。
    pub fn apply_batch(&mut self, batch: TerminalSuggestionBatch) -> bool {
        if !self.is_enabled() || !self.controller.apply_batch(batch) {
            return false;
        }
        self.query_state = if self.controller.candidates().is_empty() {
            TerminalSuggestionQueryState::Empty
        } else {
            TerminalSuggestionQueryState::Ready
        };
        true
    }

    /// UI 用请求代次过滤异步错误，旧查询失败不应覆盖当前输入状态。
    pub fn is_request_current(&self, request_generation: u64) -> bool {
        self.controller
            .current_query()
            .is_some_and(|query| query.request_generation == request_generation)
    }

    /// 仅暴露 cursor 正好位于替换范围末尾的普通文本建议，避免重叠终端内容或危险动作伪装成 ghost。
    pub fn inline_ghost(&self) -> Option<InlineSuggestionGhost> {
        if !self.mode_enabled(SuggestionQueryMode::Inline) {
            return None;
        }
        let query = self.controller.current_query()?;
        if query.mode != SuggestionQueryMode::Inline {
            return None;
        }
        let candidate = self.controller.candidates().iter().find(|candidate| {
            candidate.activation == CommandSuggestionActivation::Insert
                && candidate
                    .allowed_presentations
                    .contains(&kerminal_domain::commands::SuggestionPresentation::Inline)
                && candidate.replacement_range.end == query.cursor
                && !candidate.suffix.is_empty()
        })?;
        Some(InlineSuggestionGhost {
            binding: query.binding.clone(),
            expected_context: query.expected_context,
            request_generation: query.request_generation,
            candidate_id: candidate.id.clone(),
            text: candidate.suffix.clone(),
        })
    }

    /// 配置为 RightArrow 时仅截获无修饰的首次按下，候选失效时按键仍交给 terminal。
    pub fn accept_on_key(
        &mut self,
        event: TerminalKeyEvent,
        context: &TerminalCommandContext,
    ) -> Option<TerminalSuggestionAcceptance> {
        let settings = self.settings.as_ref()?;
        let accept_key = settings.accept_key;
        let partial_accept = settings.partial_accept;
        if !self.mode_enabled(SuggestionQueryMode::Inline)
            || accept_key != TerminalInlineSuggestionAcceptKey::RightArrow
            || event.key != TerminalKey::Right
            || event.phase != KeyPhase::Press
            || event.modifiers != Default::default()
        {
            return None;
        }
        if !self.observe_context(Some(context)) {
            return None;
        }
        let ghost = self.inline_ghost()?;
        let acceptance = self
            .controller
            .accept(&ghost.candidate_id, partial_accept)?;
        self.begin_acceptance(acceptance.clone());
        Some(acceptance)
    }

    /// 菜单项接受仍经相同 controller 验证候选，调用方不得用候选 ID 重查或重新路由 session。
    pub fn accept_candidate(
        &mut self,
        candidate_id: &str,
        partial_accept: bool,
        context: &TerminalCommandContext,
    ) -> Option<TerminalSuggestionAcceptance> {
        if !self.observe_context(Some(context)) {
            return None;
        }
        let mode = self.controller.current_query()?.mode;
        if !self.mode_enabled(mode) {
            self.clear();
            return None;
        }
        let acceptance = self.controller.accept(candidate_id, partial_accept)?;
        self.begin_acceptance(acceptance.clone());
        Some(acceptance)
    }

    /// 菜单点击绑定查询代次，拒绝关闭、重开或切换输入后迟到的候选事件。
    pub fn accept_candidate_for_generation(
        &mut self,
        request_generation: u64,
        candidate_id: &str,
        partial_accept: bool,
        context: &TerminalCommandContext,
    ) -> Option<TerminalSuggestionAcceptance> {
        if !self.menu_is_open() || !self.is_request_current(request_generation) {
            return None;
        }
        self.accept_candidate(candidate_id, partial_accept, context)
    }

    /// 按当前 context、接受代次和事件序号生成下一笔 CAS 写入，拒绝跨 pane 或乱序重放。
    pub fn acceptance_input_request(
        &mut self,
        acceptance: &TerminalSuggestionAcceptance,
        event_index: usize,
        context: &TerminalCommandContext,
    ) -> Option<UiTerminalInputRequest> {
        let pending = self.pending_acceptance.as_ref()?;
        if pending.acceptance.request_generation != acceptance.request_generation
            || pending.acceptance.binding != acceptance.binding
            || pending.next_event_index != event_index
            || pending.input_in_flight
            || pending.expected_context != context.version()
            || binding_for_context(context) != acceptance.binding
            || !self
                .controller
                .projection_matches(&acceptance.binding, context.version())
            || context.availability
                != kerminal_application::sessions::TerminalCommandAvailability::Available
            || context.alternate_screen
            || !context.prompt_ready
        {
            return None;
        }
        let event = pending.acceptance.events.get(event_index)?.clone();
        self.pending_acceptance.as_mut()?.input_in_flight = true;
        Some(UiTerminalInputRequest {
            binding: acceptance.binding.clone(),
            expected_context: Some(context.version()),
            projection: UiInputProjection::Tracked,
            event,
        })
    }

    /// 只在同一接受计划的上一事件产生精确新版本时推进序号，失败会清掉剩余写入。
    pub fn acceptance_input_written(
        &mut self,
        acceptance: &TerminalSuggestionAcceptance,
        event_index: usize,
        context: &TerminalCommandContext,
        mode: SuggestionQueryMode,
    ) -> AcceptanceInputProgress {
        let valid_progress = self.pending_acceptance.as_ref().is_some_and(|pending| {
            let previous = pending.expected_context;
            pending.acceptance.request_generation == acceptance.request_generation
                && pending.acceptance.binding == acceptance.binding
                && pending.next_event_index == event_index
                && pending.input_in_flight
                && binding_for_context(context) == acceptance.binding
                && context.session_generation == previous.session_generation
                && context.prompt_epoch == previous.prompt_epoch
                && previous.input_generation.checked_add(1) == Some(context.input_generation)
                && context.availability
                    == kerminal_application::sessions::TerminalCommandAvailability::Available
                && context.prompt_ready
                && !context.alternate_screen
        });
        if !valid_progress || !self.mode_enabled(mode) {
            self.clear();
            return AcceptanceInputProgress::Invalidated;
        }
        let Some(event) = self
            .pending_acceptance
            .as_ref()
            .and_then(|pending| pending.acceptance.events.get(event_index))
            .cloned()
        else {
            self.clear();
            return AcceptanceInputProgress::Invalidated;
        };
        if self
            .controller
            .apply_tracked_input(context, &event, mode)
            .is_none()
        {
            self.clear();
            return AcceptanceInputProgress::Invalidated;
        }
        // 接受动作包含多个编辑事件；中间态不触发 provider 查询，避免每个退格都启动远端探测。
        self.controller.cancel_query();
        let Some(pending) = self.pending_acceptance.as_mut() else {
            self.clear();
            return AcceptanceInputProgress::Invalidated;
        };
        pending.expected_context = context.version();
        pending.next_event_index += 1;
        pending.input_in_flight = false;
        if pending.next_event_index < pending.acceptance.events.len() {
            return AcceptanceInputProgress::Next {
                event_index: pending.next_event_index,
            };
        }
        self.pending_acceptance = None;
        AcceptanceInputProgress::Complete {
            follow_up: self.request_current(context, mode),
        }
    }

    /// 队列拒收或服务写入失败时使本次接受及其余输入全部失效，避免重放未知结果。
    pub fn acceptance_input_failed(
        &mut self,
        acceptance: &TerminalSuggestionAcceptance,
        event_index: usize,
    ) {
        let matches_pending = self.pending_acceptance.as_ref().is_some_and(|pending| {
            pending.acceptance.request_generation == acceptance.request_generation
                && pending.next_event_index == event_index
                && pending.input_in_flight
        });
        if matches_pending {
            self.clear();
        }
    }

    /// 显式清除临时输入和候选；运行态 context 与 controller 投影共同决定后续是否能重建。
    ///
    /// @author kongweiguang
    pub fn clear(&mut self) {
        self.controller.clear();
        self.pending_acceptance = None;
        self.query_state = TerminalSuggestionQueryState::Closed;
    }

    /// 固定一次 controller 接受结果；无输入事件的动作（例如打开片段面板）不占用终端写入序列。
    fn begin_acceptance(&mut self, acceptance: TerminalSuggestionAcceptance) {
        self.query_state = TerminalSuggestionQueryState::Closed;
        self.pending_acceptance = (!acceptance.events.is_empty()).then_some(PendingAcceptance {
            expected_context: acceptance.expected_context,
            next_event_index: 0,
            input_in_flight: false,
            acceptance,
        });
    }

    /// 判断当前设置是否允许 inline 或用户主动打开的菜单查询。
    fn mode_enabled(&self, mode: SuggestionQueryMode) -> bool {
        let Some(settings) = self.settings.as_ref() else {
            return false;
        };
        if !settings.enabled {
            return false;
        }
        matches!(
            (settings.presentation, mode),
            (
                TerminalCommandSuggestionPresentation::Inline
                    | TerminalCommandSuggestionPresentation::InlineAndMenu,
                SuggestionQueryMode::Inline
            ) | (
                TerminalCommandSuggestionPresentation::InlineAndMenu,
                SuggestionQueryMode::Menu
            )
        )
    }

    /// 区分尚未加载设置与已启用设置，避免设置未载入时产生后台任务。
    fn is_enabled(&self) -> bool {
        self.settings.as_ref().is_some_and(|settings| {
            settings.enabled && settings.presentation != TerminalCommandSuggestionPresentation::Off
        })
    }
}

impl Drop for TerminalCommandSuggestionBridge {
    /// Root 退出时取消仍在 executor 中运行的建议查询，避免留存 provider 请求。
    fn drop(&mut self) {
        self.clear();
    }
}

/// 在 Root 边界复制可信运行态身份，避免依赖 commands feature 的私有投影实现。
fn binding_for_context(context: &TerminalCommandContext) -> UiSessionBinding {
    UiSessionBinding {
        session_id: context.session_id.clone(),
        owner: context.owner.clone(),
        session_generation: context.session_generation,
    }
}

impl TerminalSuggestionJob {
    /// 返回此 job 的单调请求代次，异步错误回到 Root 后用它过滤过期状态。
    pub const fn request_generation(&self) -> u64 {
        self.query.request_generation
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_command_suggestions.rs"]
mod tests;
