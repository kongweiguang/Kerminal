//! RootView 的终端建议运行态桥接；输入投影和候选校验仍由唯一 controller 持有。
//!
//! Root 只传递可信 context、明确输入和当前设置；本模块统一连接取消、查询、ghost 与 CAS 接受。
//!
//! @author kongweiguang

use crate::features::commands::inline_suggestions::appendable_suffix;
pub use crate::features::commands::inline_suggestions::{
    InlineSuggestionController, TerminalSuggestionAcceptance,
};
mod menu;
use kerminal_application::{
    AppResult,
    commands::{CommandsService, TerminalSuggestionProgress, TerminalSuggestionQuery},
    executor::ApplicationTaskExecutor,
    preferences::{
        TerminalCommandSuggestionMenuShortcut, TerminalCommandSuggestionPresentation,
        TerminalInlineSuggestionAcceptKey, TerminalInlineSuggestionSettings,
    },
    sessions::{
        ShellEditingAvailability, ShellEditingShell, ShellEditingSnapshot,
        ShellEditingUnavailableReason, TerminalCommandContext, TerminalCommandContextVersion,
        UiInputProjection, UiSessionBinding, UiTerminalInputRequest,
    },
};
use kerminal_domain::commands::{
    CommandHistoryTarget, CommandSuggestionActivation, CommandSuggestionCandidate,
    CommandSuggestionFeedbackAction, CommandSuggestionFeedbackRequest, SuggestionQueryMode,
    SuggestionSourceState,
};
use kerminal_domain::sessions::{SessionOwner, SessionTarget};
use kerminal_terminal::input::{KeyPhase, TerminalKey, TerminalKeyEvent};
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
    snapshot: Option<ShellEditingSnapshot>,
}

/// 一次尚未完成的只读 Shell 探测；代次用于拒绝切 pane 和继续输入后的迟到读回。
pub struct TerminalSuggestionSnapshotJob {
    pub binding: UiSessionBinding,
    pub request_generation: u64,
    pub cancellation: CancellationToken,
}

struct PendingSnapshot {
    binding: UiSessionBinding,
    generation: u64,
    mode: SuggestionQueryMode,
    cancellation: CancellationToken,
    previous_input: Option<DismissedSuggestionInput>,
}

/// 只保存关闭时已确认的最小身份；不使用会因无效按键而变化的输入代次。
#[derive(Clone, Eq, PartialEq)]
struct DismissedSuggestionInput {
    binding: UiSessionBinding,
    prompt_epoch: u64,
    input: String,
    cursor: usize,
}

impl DismissedSuggestionInput {
    /// 身份必须来自已校验的 Shell 读回，正文不进入 Debug 或诊断日志。
    fn from_snapshot(snapshot: &ShellEditingSnapshot) -> Self {
        Self {
            binding: snapshot.binding.clone(),
            prompt_epoch: snapshot.context.prompt_epoch,
            input: snapshot.input.clone(),
            cursor: snapshot.cursor,
        }
    }
}

enum DismissedSuggestion {
    Confirmed(DismissedSuggestionInput),
    PendingInput(UiSessionBinding),
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
    /// 全部事件成功；仅为存在的后续查询分配空间，避免放大常见回执。
    Complete {
        follow_up: Option<Box<TerminalSuggestionJob>>,
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
    /// 当前 Shell 不能响应探测；保留关闭入口和具体恢复指引，不承诺原样重试有效。
    Unavailable(ShellEditingUnavailableReason),
    /// 输入超限只影响这一行；由缩短后的真实输入触发恢复，不让用户反复提交同一超限探测。
    InputTooLong,
}

struct PendingAcceptance {
    acceptance: TerminalSuggestionAcceptance,
    expected_context: TerminalCommandContextVersion,
    next_event_index: usize,
    input_in_flight: bool,
}

/// 让 Root 通过同一 controller 管理建议生命周期；关闭标记只保存必要的短期输入身份。
#[derive(Default)]
pub struct TerminalCommandSuggestionBridge {
    controller: InlineSuggestionController,
    settings: Option<TerminalInlineSuggestionSettings>,
    pending_acceptance: Option<PendingAcceptance>,
    query_state: TerminalSuggestionQueryState,
    pending_snapshot: Option<PendingSnapshot>,
    next_snapshot_generation: u64,
    confirmed_snapshot: Option<ShellEditingSnapshot>,
    dismissed_suggestion: Option<DismissedSuggestion>,
    progress_final: bool,
    partial_failure: bool,
}

impl TerminalCommandSuggestionBridge {
    /// 原生验收失败仅暴露状态与长度，不输出用户输入或候选正文。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn acceptance_diagnostic(&self) -> String {
        format!(
            "state={:?},pending={},confirmed={},suspended={},query={:?},candidates={}",
            self.query_state,
            self.pending_snapshot.is_some(),
            self.confirmed_snapshot.is_some(),
            self.controller.is_suspended(),
            self.controller
                .current_query()
                .map(|q| (q.input.chars().count(), q.cursor, q.mode)),
            self.controller.candidates().len()
        )
    }

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
        if self.pending_snapshot.as_ref().is_some_and(|pending| {
            pending.binding == binding_for_context(context)
                && context.prompt_ready
                && !context.alternate_screen
        }) {
            return true;
        }
        if self.confirmed_snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.binding == binding_for_context(context)
                && snapshot.context == context.version()
                && context.prompt_ready
                && !context.alternate_screen
        }) {
            return true;
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
            snapshot: None,
        })
    }

    /// 手动菜单允许重开已关闭状态；仅保留仍可归因的上一份真实快照作为关闭基线。
    pub fn begin_snapshot(
        &mut self,
        binding: UiSessionBinding,
        mode: SuggestionQueryMode,
    ) -> Option<TerminalSuggestionSnapshotJob> {
        if self.pending_acceptance.is_some() || !self.mode_enabled(mode) {
            return None;
        }
        let previous_input = if mode != SuggestionQueryMode::Menu || self.controller.is_suspended()
        {
            None
        } else {
            self.confirmed_snapshot
                .as_ref()
                .filter(|snapshot| snapshot.binding == binding)
                .map(DismissedSuggestionInput::from_snapshot)
                .or_else(|| match self.dismissed_suggestion.as_ref() {
                    Some(DismissedSuggestion::Confirmed(input)) if input.binding == binding => {
                        Some(input.clone())
                    }
                    _ => None,
                })
        };
        if mode == SuggestionQueryMode::Menu {
            self.dismissed_suggestion = None;
        }
        self.cancel_pending_snapshot();
        self.controller.cancel_query();
        self.confirmed_snapshot = None;
        self.partial_failure = false;
        self.progress_final = false;
        self.next_snapshot_generation = self.next_snapshot_generation.wrapping_add(1).max(1);
        // 快照等待态与 controller 的候选代次分属不同命名空间，避免重开菜单时复用高亮。
        let generation = self.next_snapshot_generation | (1_u64 << 63);
        let cancellation = CancellationToken::new();
        self.pending_snapshot = Some(PendingSnapshot {
            binding: binding.clone(),
            generation,
            mode,
            cancellation: cancellation.clone(),
            previous_input,
        });
        self.query_state = TerminalSuggestionQueryState::Loading;
        Some(TerminalSuggestionSnapshotJob {
            binding,
            request_generation: generation,
            cancellation,
        })
    }

    /// 只接受仍在当前 pane 且没有被新输入撤销的探测结果。
    pub fn snapshot_is_current(&self, generation: u64, binding: &UiSessionBinding) -> bool {
        self.pending_snapshot.as_ref().is_some_and(|pending| {
            pending.generation == generation
                && &pending.binding == binding
                && !pending.cancellation.is_cancelled()
        })
    }

    /// 先校验真实读回，再按正文、光标及 prompt 抑制已关闭的自动提示。
    pub fn snapshot_received(
        &mut self,
        generation: u64,
        context: &TerminalCommandContext,
        snapshot: ShellEditingSnapshot,
    ) -> Option<TerminalSuggestionJob> {
        if !self.snapshot_is_current(generation, &snapshot.binding) {
            return None;
        }
        let mode = self.pending_snapshot.as_ref()?.mode;
        let Some((query, request_cancellation)) = self
            .controller
            .update_shell_snapshot(context, &snapshot, mode)
        else {
            self.query_state = TerminalSuggestionQueryState::Failed;
            return None;
        };
        let dismissed = mode == SuggestionQueryMode::Inline
            && match self.dismissed_suggestion.as_ref() {
                Some(DismissedSuggestion::Confirmed(previous)) => {
                    previous.binding == snapshot.binding
                        && previous.prompt_epoch == snapshot.context.prompt_epoch
                        && previous.input == snapshot.input
                        && previous.cursor == snapshot.cursor
                }
                Some(DismissedSuggestion::PendingInput(binding)) => binding == &snapshot.binding,
                None => false,
            };
        self.pending_snapshot = None;
        if dismissed {
            self.dismissed_suggestion = Some(DismissedSuggestion::Confirmed(
                DismissedSuggestionInput::from_snapshot(&snapshot),
            ));
            self.controller.cancel_query();
            self.confirmed_snapshot = None;
            self.query_state = TerminalSuggestionQueryState::Closed;
            return None;
        }
        self.dismissed_suggestion = None;
        self.confirmed_snapshot = Some(snapshot.clone());
        self.progress_final = false;
        self.partial_failure = false;
        self.query_state = TerminalSuggestionQueryState::Loading;
        Some(TerminalSuggestionJob {
            query,
            request_cancellation,
            snapshot: Some(snapshot),
        })
    }

    /// 仅主动菜单解释能力降级；未收到握手不等于来源查询失败，不能诱导无效重试。
    /// 此处不改变连接能力，稍后启动完成仍可重新请求；旧绑定的失败不能覆盖当前输入。
    pub fn snapshot_failed(
        &mut self,
        generation: u64,
        binding: &UiSessionBinding,
        availability: Option<ShellEditingAvailability>,
    ) -> bool {
        if !self.snapshot_is_current(generation, binding) {
            return false;
        }
        if self
            .pending_snapshot
            .as_ref()
            .is_some_and(|pending| pending.mode == SuggestionQueryMode::Menu)
        {
            self.query_state = match availability {
                Some(ShellEditingAvailability::Pending) => {
                    TerminalSuggestionQueryState::Unavailable(
                        ShellEditingUnavailableReason::IntegrationNotObserved,
                    )
                }
                Some(ShellEditingAvailability::Unavailable(reason)) => {
                    TerminalSuggestionQueryState::Unavailable(reason)
                }
                _ => TerminalSuggestionQueryState::Failed,
            };
        } else {
            self.pending_snapshot = None;
            self.query_state = TerminalSuggestionQueryState::Closed;
        }
        true
    }

    /// 超限响应已完成并排空本次探测，保留连接能力；只有主动菜单解释缩短输入即可恢复。
    pub fn snapshot_input_too_long(&mut self, generation: u64, binding: &UiSessionBinding) -> bool {
        if !self.snapshot_failed(generation, binding, None) {
            return false;
        }
        if self.menu_is_open() {
            self.query_state = TerminalSuggestionQueryState::InputTooLong;
        }
        true
    }

    /// 为自动刷新保留已打开菜单的交互模式，避免继续输入时菜单跳回行内模式。
    pub fn active_mode(&self) -> SuggestionQueryMode {
        if self.menu_is_open() {
            SuggestionQueryMode::Menu
        } else {
            SuggestionQueryMode::Inline
        }
    }

    /// 打开菜单时基于 controller 已跟踪的文本和 cursor 查询，防止 Root 再维护一份输入副本。
    pub fn request_current(
        &mut self,
        context: &TerminalCommandContext,
        mode: SuggestionQueryMode,
    ) -> Option<TerminalSuggestionJob> {
        if self.pending_acceptance.is_some() || self.controller.is_suspended() {
            return None;
        }
        if !self.observe_context(Some(context)) {
            return None;
        }
        let (input, cursor) = self.controller.input_projection()?;
        self.request(context, input.to_owned(), cursor, mode)
    }

    /// 识别菜单快捷键不依赖旧输入投影；按键被接管后再异步读取真实 Shell 行。
    pub fn is_menu_shortcut(&self, event: TerminalKeyEvent) -> bool {
        let Some(settings) = self.settings.as_ref() else {
            return false;
        };
        if !self.mode_enabled(SuggestionQueryMode::Menu) || event.phase != KeyPhase::Press {
            return false;
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
        let alt_slash = event.key == TerminalKey::Character('/')
            && event.modifiers.alt
            && !event.modifiers.control
            && !event.modifiers.shift
            && !event.modifiers.super_key;
        ctrl_space || alt_slash || tab
    }

    /// 菜单只在有效菜单查询代次内绘制，Loading/Empty/Failed 由 query_state 单独表达。
    pub fn menu_is_open(&self) -> bool {
        (self
            .pending_snapshot
            .as_ref()
            .is_some_and(|pending| pending.mode == SuggestionQueryMode::Menu)
            || self
                .controller
                .current_query()
                .is_some_and(|query| query.mode == SuggestionQueryMode::Menu))
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
        if let Some(pending) = self.pending_snapshot.as_ref()
            && pending.mode == SuggestionQueryMode::Menu
            && self.query_state != TerminalSuggestionQueryState::Closed
        {
            return Some(TerminalSuggestionMenuProjection {
                request_generation: pending.generation,
                state: self.query_state,
                partial_accept: false,
                candidates: Arc::from([]),
                selection_keys: Arc::from([]),
                partial_failure: false,
            });
        }
        let query = self.controller.current_query()?;
        if !self.menu_is_open() {
            return None;
        }

        Some(TerminalSuggestionMenuProjection {
            request_generation: query.request_generation,
            state: self.query_state,
            partial_accept: false,
            candidates: Arc::from(self.menu_candidates().to_vec()),
            partial_failure: self.partial_failure,
            selection_keys: self
                .menu_candidates()
                .iter()
                .map(|candidate| {
                    if candidate.activation == CommandSuggestionActivation::OpenSnippetPanel {
                        format!("snippet:{}", candidate.id)
                    } else {
                        format!(
                            "append:{}",
                            appendable_suffix(candidate, query).unwrap_or_default()
                        )
                    }
                })
                .collect::<Vec<_>>()
                .into(),
        })
    }

    /// 关闭菜单时保留关闭身份而非接受权限，手动重开仍会请求新的 Shell 快照。
    pub fn dismiss_menu(&mut self) -> bool {
        if !self.menu_is_open() {
            return false;
        }
        self.remember_dismissed_input();
        self.cancel_pending_snapshot();
        self.controller.cancel_query();
        self.confirmed_snapshot = None;
        self.query_state = TerminalSuggestionQueryState::Closed;
        true
    }

    /// 行内提示只在确实存在可绘制候选时关闭，普通 Escape 仍留给终端。
    pub fn dismiss_inline(&mut self) -> bool {
        if self.inline_ghost().is_none() {
            return false;
        }
        self.remember_dismissed_input();
        self.controller.cancel_query();
        self.confirmed_snapshot = None;
        self.query_state = TerminalSuggestionQueryState::Closed;
        true
    }

    /// 可见菜单代次与活动 provider 代次分离，使暂停或加载中的菜单仍能关闭。
    pub fn is_menu_projection_current(&self, request_generation: u64) -> bool {
        if !self.menu_is_open() {
            return false;
        }
        if let Some(pending) = self.pending_snapshot.as_ref()
            && pending.mode == SuggestionQueryMode::Menu
        {
            return pending.generation == request_generation;
        }
        self.controller.current_query().is_some_and(|query| {
            query.mode == SuggestionQueryMode::Menu
                && query.request_generation == request_generation
        })
    }

    /// 只把当前请求的失败标为 Failed，迟到的旧请求不能覆盖新菜单或新输入状态。
    pub fn fail_request(&mut self, request_generation: u64) -> bool {
        if !self.is_request_current(request_generation) || self.progress_final {
            return false;
        }
        self.progress_final = true;
        if self.controller.candidates().is_empty() {
            self.query_state = TerminalSuggestionQueryState::Failed;
        } else {
            self.partial_failure = true;
            self.query_state = TerminalSuggestionQueryState::Ready;
        }
        true
    }

    /// 仅在共享 executor 中查询；Shell 快照同时固定目录、Shell 身份和增量结果代次。
    pub async fn run(
        job: TerminalSuggestionJob,
        executor: ApplicationTaskExecutor,
        commands: Arc<CommandsService>,
        updates: Option<kerminal_application::commands::TerminalSuggestionProgressPublisher>,
    ) -> AppResult<TerminalSuggestionProgress> {
        executor
            .run(move |task_cancellation| async move {
                match job.snapshot {
                    Some(snapshot) => {
                        commands
                            .terminal_suggestions_from_snapshot(
                                snapshot,
                                job.query.request_generation,
                                job.query.mode,
                                job.request_cancellation,
                                task_cancellation,
                                updates,
                            )
                            .await
                    }
                    None => commands
                        .terminal_suggestions(
                            job.query,
                            job.request_cancellation,
                            task_cancellation,
                        )
                        .await
                        .map(|batch| TerminalSuggestionProgress {
                            batch,
                            snapshot_revision: 0,
                            sources: Vec::new(),
                        }),
                }
            })
            .await
    }

    /// 增量结果只能更新同一真实快照；最终结果提交后，排队的旧增量不得倒退界面。
    pub fn apply_progress(&mut self, progress: TerminalSuggestionProgress, is_final: bool) -> bool {
        if self.progress_final
            || self.confirmed_snapshot.as_ref().is_none_or(|snapshot| {
                snapshot.revision != progress.snapshot_revision
                    || snapshot.binding != progress.batch.binding
                    || snapshot.context != progress.batch.expected_context
            })
            || !self.controller.apply_batch(progress.batch)
        {
            return false;
        }
        let failed = progress
            .sources
            .iter()
            .any(|source| source.state == SuggestionSourceState::Failed);
        let pending = progress
            .sources
            .iter()
            .any(|source| source.state == SuggestionSourceState::Pending);
        self.partial_failure = failed;
        self.query_state = if !self.controller.candidates().is_empty() {
            TerminalSuggestionQueryState::Ready
        } else if pending && !is_final {
            TerminalSuggestionQueryState::Loading
        } else if failed {
            TerminalSuggestionQueryState::Failed
        } else {
            TerminalSuggestionQueryState::Empty
        };
        self.progress_final = is_final;
        true
    }

    /// 只把未取消的请求视为活动代次，已暂停菜单可以关闭但不能触发接受或重试。
    pub fn is_request_current(&self, request_generation: u64) -> bool {
        self.pending_snapshot.as_ref().is_some_and(|pending| {
            pending.generation == request_generation && !pending.cancellation.is_cancelled()
        }) || self.controller.is_request_active()
            && self
                .controller
                .current_query()
                .is_some_and(|query| query.request_generation == request_generation)
    }

    /// 输入入队立即撤销旧候选；未知关闭边界以首次排空后的快照为准，旧 ACK 不能撤销 Esc。
    /// 无法证明正文变化时保持关闭，之后的新快照或显式菜单才恢复候选。
    pub fn suspend_for_pending_input(&mut self) {
        let menu_open = self.menu_is_open();
        if let Some(pending) = self.pending_snapshot.as_ref() {
            pending.cancellation.cancel();
        }
        if !menu_open {
            self.pending_snapshot = None;
        }
        self.confirmed_snapshot = None;
        self.controller.suspend_query();
        self.query_state = if menu_open {
            TerminalSuggestionQueryState::Loading
        } else {
            TerminalSuggestionQueryState::Closed
        };
    }

    /// 未知原生预测交由渲染网格门禁判断；Shell 明确报告已绘制时始终让位。
    pub fn inline_ghost(&self) -> Option<InlineSuggestionGhost> {
        if !self.mode_enabled(SuggestionQueryMode::Inline) {
            return None;
        }
        if self
            .confirmed_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.shell_prediction_visible == Some(true))
        {
            return None;
        }
        let query = self.controller.current_query()?;
        if query.mode != SuggestionQueryMode::Inline {
            return None;
        }
        let (candidate, suffix) = self.controller.candidates().iter().find_map(|candidate| {
            appendable_suffix(candidate, query).map(|suffix| (candidate, suffix))
        })?;
        Some(InlineSuggestionGhost {
            binding: query.binding.clone(),
            expected_context: query.expected_context,
            request_generation: query.request_generation,
            candidate_id: candidate.id.clone(),
            text: suffix.to_owned(),
        })
    }

    /// 右方向键完整接受，显式启用时 Alt+右方向分段接受；其他按键继续交给 Shell。
    pub fn accept_on_key(
        &mut self,
        event: TerminalKeyEvent,
        context: &TerminalCommandContext,
    ) -> Option<TerminalSuggestionAcceptance> {
        let settings = self.settings.as_ref()?;
        let accept_key = settings.accept_key;
        let partial_accept = settings.partial_accept
            && event.modifiers.alt
            && !event.modifiers.control
            && !event.modifiers.shift
            && !event.modifiers.super_key;
        if !self.mode_enabled(SuggestionQueryMode::Inline)
            || accept_key != TerminalInlineSuggestionAcceptKey::RightArrow
            || event.key != TerminalKey::Right
            || event.phase != KeyPhase::Press
            || (event.modifiers != Default::default() && !partial_accept)
        {
            return None;
        }
        if !self.observe_context(Some(context)) {
            return None;
        }
        let ghost = self.inline_ghost()?;
        let mut acceptance = self
            .controller
            .accept(&ghost.candidate_id, partial_accept)?;
        acceptance.shell_snapshot = self.confirmed_snapshot.take();
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
        let mut acceptance = self.controller.accept(candidate_id, partial_accept)?;
        acceptance.shell_snapshot = self.confirmed_snapshot.take();
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
            || (acceptance.shell_snapshot.is_none()
                && context.availability
                    != kerminal_application::sessions::TerminalCommandAvailability::Available)
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

    /// 只在同一接受计划的上一事件产生精确新版本时推进序号，失败清掉剩余写入；后续查询按需装箱。
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
            follow_up: self.request_current(context, mode).map(Box::new),
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
        self.cancel_pending_snapshot();
        self.controller.clear();
        self.pending_acceptance = None;
        self.confirmed_snapshot = None;
        self.dismissed_suggestion = None;
        self.progress_final = false;
        self.partial_failure = false;
        self.query_state = TerminalSuggestionQueryState::Closed;
    }

    /// 接受只在服务回读与预期正文、prompt 和新修订一致时确认；任何不确定结果均停止。
    pub fn confirm_shell_acceptance(
        &mut self,
        acceptance: &TerminalSuggestionAcceptance,
        result: &ShellEditingSnapshot,
        context: &TerminalCommandContext,
    ) -> bool {
        let Some(before) = acceptance.shell_snapshot.as_ref() else {
            return false;
        };
        let valid = self.pending_acceptance.as_ref().is_some_and(|pending| {
            pending.input_in_flight
                && pending.acceptance.request_generation == acceptance.request_generation
                && pending.acceptance.binding == acceptance.binding
                && before.binding == result.binding
                && result.binding == binding_for_context(context)
                && result.context == context.version()
                && result.context.session_generation == before.context.session_generation
                && result.context.prompt_epoch == before.context.prompt_epoch
                && before.context.input_generation.checked_add(1)
                    == Some(result.context.input_generation)
                && result.revision > before.revision
                && result.input == acceptance.input_after
                && result.cursor == acceptance.cursor_after
                && result.cursor_at_end()
                && context.prompt_ready
                && !context.alternate_screen
        });
        self.pending_acceptance = None;
        self.controller.clear();
        self.confirmed_snapshot = None;
        self.query_state = TerminalSuggestionQueryState::Closed;
        valid
    }

    /// 取消探测只发协作取消，底层任务仍须自行收尾并释放 Shell 请求资源。
    fn cancel_pending_snapshot(&mut self) {
        if let Some(pending) = self.pending_snapshot.take() {
            pending.cancellation.cancel();
        }
    }

    /// 没有可靠正文时只等待已在途输入的首次读回，绝不把旧投影冒充当前 Shell 状态。
    fn remember_dismissed_input(&mut self) {
        let binding = self
            .pending_snapshot
            .as_ref()
            .map(|pending| pending.binding.clone())
            .or_else(|| {
                self.controller
                    .current_query()
                    .map(|query| query.binding.clone())
            });
        let dismissed = binding.and_then(|binding| {
            if self.controller.is_suspended() {
                return Some(DismissedSuggestion::PendingInput(binding));
            }
            self.confirmed_snapshot
                .as_ref()
                .filter(|snapshot| snapshot.binding == binding)
                .map(DismissedSuggestionInput::from_snapshot)
                .or_else(|| {
                    self.pending_snapshot
                        .as_ref()
                        .and_then(|pending| pending.previous_input.clone())
                })
                .map(DismissedSuggestion::Confirmed)
        });
        self.dismissed_suggestion = dismissed;
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

/// 仅从已确认的原始快照和固定目标构造反馈；敏感内容仍由 application 统一过滤。
pub(super) fn accepted_feedback_request(
    acceptance: &TerminalSuggestionAcceptance,
    target: &SessionTarget,
) -> Option<CommandSuggestionFeedbackRequest> {
    let snapshot = acceptance.shell_snapshot.as_ref()?;
    let SessionOwner::Ui { pane_id, .. } = &acceptance.binding.owner else {
        return None;
    };
    if snapshot.binding != acceptance.binding
        || snapshot.context != acceptance.expected_context
        || snapshot.input != acceptance.input_before
        || !snapshot.cursor_at_end()
        || acceptance.activation != CommandSuggestionActivation::Insert
        || acceptance
            .input_after
            .strip_prefix(&snapshot.input)
            .is_none_or(str::is_empty)
    {
        return None;
    }
    let (target, remote_host_id) = match target {
        SessionTarget::Local(_) => (CommandHistoryTarget::Local, None),
        SessionTarget::Ssh(target) => (
            CommandHistoryTarget::Ssh,
            Some(target.host_id.as_str().to_owned()),
        ),
        SessionTarget::Telnet(_) | SessionTarget::Serial(_) | SessionTarget::Container(_) => {
            return None;
        }
    };
    let shell = match snapshot.shell {
        ShellEditingShell::Bash => "bash",
        ShellEditingShell::Zsh => "zsh",
        ShellEditingShell::Fish => "fish",
        ShellEditingShell::PowerShell => "powershell",
    };
    Some(CommandSuggestionFeedbackRequest {
        action: CommandSuggestionFeedbackAction::Accepted,
        provider: acceptance.provider,
        replacement_text: acceptance.replacement_text.clone(),
        input: snapshot.input.clone(),
        target,
        source_id: acceptance.source_id.clone(),
        session_id: Some(acceptance.binding.session_id.as_str().to_owned()),
        pane_id: Some(pane_id.as_str().to_owned()),
        profile_id: None,
        remote_host_id,
        cwd: snapshot.cwd.clone(),
        shell: Some(shell.to_owned()),
    })
}

#[cfg(test)]
#[path = "../../tests/unit/root_command_suggestions.rs"]
mod tests;
