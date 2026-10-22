//! 仅从已确认的可见 UI 输入 ACK 维护有界命令行投影。
//!
//! 该状态不读取终端回显、不进入持久化，也不替 shell 推断未知编辑行为。
//!
//! @author kongweiguang

use std::collections::{HashMap, VecDeque};

use kerminal_application::sessions::{
    TerminalCommandAvailability, TerminalCommandContext, TerminalCommandContextVersion,
    TerminalCommandUnavailableReason, UiInputProjection, UiSessionBinding, UiTerminalInputRequest,
    command_blocks::{MAX_COMMAND_BLOCK_TEXT_BYTES, is_plain_command_enter},
};
use kerminal_domain::identity::SessionId;
use kerminal_domain::sessions::SessionOwner;
use kerminal_terminal::input::{
    InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyEvent,
};

const MAX_TRACKED_SESSIONS: usize = 256;
const MAX_PENDING_INPUTS_PER_SESSION: usize = 128;

/// 与一次 FIFO 输入绑定的无正文票据；line generation 变化后，迟到 ACK 自动失效。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CommandLineInputTicket {
    binding: UiSessionBinding,
    sequence: u64,
    line_generation: u64,
    kind: TicketKind,
}

/// Enter 票据与普通编辑票据分开，避免提交流程在 ACK 前清除待捕获正文。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TicketKind {
    Edit,
    Submission,
}

/// 已验证的命令正文和提交上下文；故意不实现 Debug，避免正文进入诊断日志。
#[derive(Clone, Eq, PartialEq)]
pub(super) struct CommandLineSubmission {
    pub(super) command: String,
    pub(super) expected_context: TerminalCommandContextVersion,
}

/// 每个固定 UI session 只保存当前 prompt 的短暂输入投影及有限的 ACK 票据。
#[derive(Default)]
pub(super) struct CommandLineProjection {
    sessions: HashMap<SessionId, SessionLineProjection>,
    next_ticket_sequence: u64,
}

struct SessionLineProjection {
    binding: UiSessionBinding,
    context: TerminalCommandContextVersion,
    known_line: Option<LineBuffer>,
    must_wait_for_prompt: bool,
    line_generation: u64,
    pending: VecDeque<PendingTicket>,
}

struct PendingTicket {
    sequence: u64,
    line_generation: u64,
    kind: TicketKind,
}

struct LineBuffer {
    text: String,
    /// UTF-8 字节偏移；所有编辑操作都在字符边界上移动。
    cursor: usize,
}

enum EditEffect {
    Insert(String),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Noop,
}

impl CommandLineProjection {
    /// 只有完整 UI 绑定和可信 shell context 同时匹配时，才允许空 prompt 建立已知基线。
    pub(super) fn observe_context(
        &mut self,
        binding: &UiSessionBinding,
        context: &TerminalCommandContext,
    ) {
        if !context_matches_binding(binding, context) {
            self.invalidate_binding(binding);
            return;
        }

        if !self.sessions.contains_key(&binding.session_id)
            && self.sessions.len() >= MAX_TRACKED_SESSIONS
        {
            return;
        }

        let Some(state) = self.sessions.get_mut(&binding.session_id) else {
            self.sessions.insert(
                binding.session_id.clone(),
                SessionLineProjection::from_context(binding.clone(), context),
            );
            return;
        };

        if state.binding != *binding {
            *state = SessionLineProjection::from_context(binding.clone(), context);
            return;
        }

        if context.prompt_epoch < state.context.prompt_epoch
            || (context.prompt_epoch == state.context.prompt_epoch
                && context.input_generation < state.context.input_generation)
        {
            return;
        }

        if context.prompt_epoch != state.context.prompt_epoch {
            state.reset_for_context(context);
            return;
        }

        if context.input_generation != state.context.input_generation || !context_is_usable(context)
        {
            state.invalidate(context.version(), context_requires_new_prompt(context));
            if !state.must_wait_for_prompt && context_can_seed_empty(context) {
                state.known_line = Some(LineBuffer::empty());
            }
            return;
        }

        if state.known_line.is_none()
            && !state.must_wait_for_prompt
            && context_can_seed_empty(context)
        {
            state.known_line = Some(LineBuffer::empty());
        }
    }

    /// 仅当同一 binding/context 的 writer ACK 已使投影精确落在指定 ASCII 行时返回真；探针据此避免盲目回退用户输入。
    ///
    /// @author kongweiguang
    #[cfg(feature = "acceptance-metrics")]
    pub(super) fn confirms_exact_line(
        &self,
        binding: &UiSessionBinding,
        expected_text: &str,
        expected_cursor: usize,
        expected_context: TerminalCommandContextVersion,
    ) -> bool {
        self.sessions.get(&binding.session_id).is_some_and(|state| {
            state.binding == *binding
                && state.context == expected_context
                && !state.must_wait_for_prompt
                && state.pending.is_empty()
                && state.known_line.as_ref().is_some_and(|line| {
                    line.text == expected_text && line.cursor == expected_cursor
                })
        })
    }

    /// 已知行上的 committed UI Text 作为精确 FIFO edit；无法建模的输入立即失效，Enter 正文仍在出队后读取。
    /// 未知行可暂存 tracked edit 票据，但只有严格连续的 ACK 才能初始化，不能把票据本身当正文可信度。
    ///
    /// @author kongweiguang
    pub(super) fn register_input(
        &mut self,
        request: &UiTerminalInputRequest,
    ) -> Option<CommandLineInputTicket> {
        if !matches!(request.binding.owner, SessionOwner::Ui { .. }) {
            return None;
        }
        let committed_text_on_known_line = request.projection == UiInputProjection::Invalidates
            && matches!(&request.event, TerminalInputEvent::Text(_))
            && self
                .sessions
                .get(&request.binding.session_id)
                .is_some_and(|state| {
                    state.binding == request.binding
                        && state.known_line.is_some()
                        && !state.must_wait_for_prompt
                });
        let tracked =
            request.projection == UiInputProjection::Tracked || committed_text_on_known_line;
        let kind = if is_plain_command_enter(&request.event) {
            TicketKind::Submission
        } else if tracked {
            TicketKind::Edit
        } else {
            self.invalidate_binding(&request.binding);
            return None;
        };

        let state = self.sessions.get_mut(&request.binding.session_id)?;
        if state.binding != request.binding {
            return None;
        }
        if state.pending.len() >= MAX_PENDING_INPUTS_PER_SESSION {
            state.invalidate(state.context, true);
            return None;
        }

        self.next_ticket_sequence = self.next_ticket_sequence.wrapping_add(1).max(1);
        let ticket = CommandLineInputTicket {
            binding: request.binding.clone(),
            sequence: self.next_ticket_sequence,
            line_generation: state.line_generation,
            kind,
        };
        state.pending.push_back(PendingTicket {
            sequence: ticket.sequence,
            line_generation: ticket.line_generation,
            kind,
        });
        Some(ticket)
    }

    /// 只有刚签发且仍在队列中的精确已知行编辑票据，才能把 committed Text 标为可跟踪。
    ///
    /// @author kongweiguang
    pub(super) fn promote_registered_committed_text(
        &self,
        request: &mut UiTerminalInputRequest,
        ticket: Option<&CommandLineInputTicket>,
    ) {
        if request.projection != UiInputProjection::Invalidates
            || !matches!(&request.event, TerminalInputEvent::Text(_))
        {
            return;
        }
        let (Some(ticket), Some(state)) = (ticket, self.sessions.get(&request.binding.session_id))
        else {
            return;
        };
        let ticket_is_live_known_line_edit = ticket.kind == TicketKind::Edit
            && ticket_matches_request(ticket, request)
            && ticket_matches_state(ticket, state)
            && state.known_line.is_some()
            && !state.must_wait_for_prompt
            && state.pending.iter().any(|pending| {
                pending.sequence == ticket.sequence
                    && pending.line_generation == ticket.line_generation
                    && pending.kind == TicketKind::Edit
            });
        if ticket_is_live_known_line_edit {
            request.projection = UiInputProjection::Tracked;
        }
    }

    /// FIFO ACK 才会应用输入；不支持或无法证明的事件立即失效，避免正文与真实 shell 行分叉。
    pub(super) fn apply_ack(
        &mut self,
        ticket: &CommandLineInputTicket,
        request: &UiTerminalInputRequest,
        context: &TerminalCommandContext,
    ) {
        let Some(state) = self.sessions.get_mut(&ticket.binding.session_id) else {
            return;
        };
        if !ticket_matches_state(ticket, state) || !ticket_matches_request(ticket, request) {
            return;
        }
        if !state.consume_ticket(ticket) {
            state.invalidate(state.context, true);
            return;
        }
        if ticket.kind == TicketKind::Submission {
            state.finish_submission(context);
            return;
        }
        if !context_matches_binding(&ticket.binding, context) || !context_is_usable(context) {
            state.invalidate(context.version(), true);
            return;
        }

        let Some(effect) = edit_effect(&request.event) else {
            state.invalidate(context.version(), true);
            return;
        };
        if !state.apply_edit_ack(effect, context) {
            state.invalidate(context.version(), true);
        }
    }

    /// 仅失效尚待处理的当前票据；重复迟到回执无副作用，失败提交不会伪装成 shell 已收到 Enter。
    pub(super) fn apply_failure(&mut self, ticket: &CommandLineInputTicket) {
        let Some(state) = self.sessions.get_mut(&ticket.binding.session_id) else {
            return;
        };
        if !ticket_matches_state(ticket, state) {
            return;
        }
        if !state.pending.iter().any(|pending| {
            pending.sequence == ticket.sequence
                && pending.line_generation == ticket.line_generation
                && pending.kind == ticket.kind
        }) {
            // 迟到的重复失败回执不应使后续 prompt 的有效状态再次失效。
            return;
        }
        if !state.consume_ticket(ticket) {
            state.invalidate(state.context, true);
            return;
        }
        state.invalidate(state.context, true);
    }

    /// Enter 出队后，只有完整 binding、FIFO 头票据及精确 ACK context 都匹配时才返回正文。
    pub(super) fn prepare_submission(
        &self,
        ticket: &CommandLineInputTicket,
        request: &UiTerminalInputRequest,
        context: &TerminalCommandContext,
    ) -> Option<CommandLineSubmission> {
        let state = self.sessions.get(&ticket.binding.session_id)?;
        let line = state.known_line.as_ref()?;
        if ticket.kind != TicketKind::Submission
            || !ticket_matches_state(ticket, state)
            || !ticket_matches_request(ticket, request)
            || !is_plain_command_enter(&request.event)
            || state.pending.front().is_none_or(|pending| {
                pending.sequence != ticket.sequence
                    || pending.line_generation != ticket.line_generation
                    || pending.kind != TicketKind::Submission
            })
            || !context_matches_binding(&ticket.binding, context)
            || !context_is_usable(context)
            || context.version() != state.context
            || state.must_wait_for_prompt
            || line.text.is_empty()
            || line.text.len() > MAX_COMMAND_BLOCK_TEXT_BYTES
            || line.text.chars().any(char::is_control)
        {
            return None;
        }

        Some(CommandLineSubmission {
            command: line.text.clone(),
            expected_context: context.version(),
        })
    }

    /// 提交成功表示 Enter 已获 writer ACK；旧行随即失效，后续输入票据不得落入新 prompt。
    pub(super) fn apply_submission_ack(
        &mut self,
        ticket: &CommandLineInputTicket,
        request: &UiTerminalInputRequest,
        context: &TerminalCommandContext,
    ) {
        let Some(state) = self.sessions.get_mut(&ticket.binding.session_id) else {
            return;
        };
        if ticket.kind != TicketKind::Submission
            || !ticket_matches_state(ticket, state)
            || !ticket_matches_request(ticket, request)
            || !is_plain_command_enter(&request.event)
            || !state.consume_ticket(ticket)
        {
            if ticket_matches_state(ticket, state) {
                state.invalidate(state.context, true);
            }
            return;
        }
        state.finish_submission(context);
    }

    /// 未跟踪输入、广播、重连或外部编辑使当前行失去可信度，必须等新的 OSC prompt 再恢复。
    pub(super) fn invalidate_binding(&mut self, binding: &UiSessionBinding) {
        if let Some(state) = self.sessions.get_mut(&binding.session_id)
            && state.binding == *binding
        {
            state.invalidate(state.context, true);
        }
    }
}

impl SessionLineProjection {
    /// 初始状态只从服务明确报告的空 prompt 建立，已有未知正文不会被重置为空字符串。
    fn from_context(binding: UiSessionBinding, context: &TerminalCommandContext) -> Self {
        let known_line = context_can_seed_empty(context).then(LineBuffer::empty);
        Self {
            binding,
            context: context.version(),
            known_line,
            must_wait_for_prompt: context_requires_new_prompt(context),
            line_generation: 1,
            pending: VecDeque::new(),
        }
    }

    /// 新 OSC prompt epoch 是唯一能越过旧行失效屏障的事件。
    fn reset_for_context(&mut self, context: &TerminalCommandContext) {
        self.line_generation = self.line_generation.wrapping_add(1).max(1);
        self.pending.clear();
        self.context = context.version();
        self.known_line = context_can_seed_empty(context).then(LineBuffer::empty);
        self.must_wait_for_prompt = context_requires_new_prompt(context);
    }

    /// 丢弃同 generation 下所有待回执并记录失效基线，防止同 prompt 的空标记错误复活旧行。
    fn invalidate(&mut self, context: TerminalCommandContextVersion, wait: bool) {
        self.line_generation = self.line_generation.wrapping_add(1).max(1);
        self.pending.clear();
        self.known_line = None;
        self.must_wait_for_prompt = wait;
        self.context = context;
    }

    /// 只接受队首且 generation 未变化的票据，其他次序一律视为投影失配。
    fn consume_ticket(&mut self, ticket: &CommandLineInputTicket) -> bool {
        if ticket.binding != self.binding || ticket.line_generation != self.line_generation {
            return false;
        }
        let Some(front) = self.pending.front() else {
            return false;
        };
        if front.sequence != ticket.sequence || front.line_generation != ticket.line_generation {
            return false;
        }
        self.pending.pop_front();
        true
    }

    /// 将普通编辑与真实 context ACK 配对；同一 FIFO 中跨到新 prompt 的输入仍保留后续票据。
    fn apply_edit_ack(&mut self, effect: EditEffect, context: &TerminalCommandContext) -> bool {
        if context.session_generation != self.binding.session_generation {
            return false;
        }
        if context.prompt_epoch < self.context.prompt_epoch
            || context.input_generation < self.context.input_generation
        {
            return false;
        }
        let input_delta = context
            .input_generation
            .checked_sub(self.context.input_generation);
        let mut line = self.known_line.take();

        if context.prompt_epoch != self.context.prompt_epoch {
            let fresh_prompt = context.prompt_epoch > self.context.prompt_epoch;
            let first_input_in_prompt = fresh_prompt
                && input_delta == Some(1)
                && context.prompt_ready
                && context.availability == TerminalCommandAvailability::Available
                && !context.alternate_screen;
            if first_input_in_prompt || context_can_seed_empty(context) {
                line = Some(LineBuffer::empty());
                self.must_wait_for_prompt = false;
            } else {
                self.context = context.version();
                self.pending.clear();
                self.line_generation = self.line_generation.wrapping_add(1).max(1);
                self.must_wait_for_prompt = context_requires_new_prompt(context);
                return false;
            }
        } else if input_delta.is_none_or(|delta| delta > 1) {
            return false;
        }

        let Some(mut line) = line else {
            return false;
        };
        if input_delta == Some(0) && !matches!(effect, EditEffect::Noop) {
            return false;
        }
        if input_delta != Some(0) && input_delta != Some(1) {
            return false;
        }
        if !line.apply(effect) || line.text.len() > MAX_COMMAND_BLOCK_TEXT_BYTES {
            return false;
        }

        self.known_line = Some(line);
        self.context = context.version();
        self.must_wait_for_prompt = false;
        true
    }

    /// 已提交 Enter 后保留真实 context，但旧命令正文与后续预先排队输入都不再可信。
    fn finish_submission(&mut self, context: &TerminalCommandContext) {
        self.pending.clear();
        self.line_generation = self.line_generation.wrapping_add(1).max(1);
        if context.session_id == self.binding.session_id
            && context.owner == self.binding.owner
            && context.session_generation == self.binding.session_generation
            && context.prompt_epoch >= self.context.prompt_epoch
            && context.input_generation >= self.context.input_generation
        {
            self.context = context.version();
            self.known_line = context_can_seed_empty(context).then(LineBuffer::empty);
            self.must_wait_for_prompt = context_requires_new_prompt(context);
        } else {
            self.known_line = None;
            self.must_wait_for_prompt = true;
        }
    }
}

impl LineBuffer {
    /// 构造服务已确认的新 prompt 空行，游标以 UTF-8 字节偏移保存并始终落在字符边界。
    fn empty() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
        }
    }

    /// 只跟踪可证明的编辑；非 ASCII 行的 grapheme 导航/删除语义随 shell 而异，故保守转 Unknown。
    fn apply(&mut self, effect: EditEffect) -> bool {
        if !self.text.is_ascii()
            && matches!(
                &effect,
                EditEffect::Backspace
                    | EditEffect::Delete
                    | EditEffect::Left
                    | EditEffect::Right
                    | EditEffect::Home
                    | EditEffect::End
            )
        {
            return false;
        }

        match effect {
            EditEffect::Insert(text) => {
                if text.chars().any(char::is_control)
                    || self.text.len().saturating_add(text.len()) > MAX_COMMAND_BLOCK_TEXT_BYTES
                {
                    return false;
                }
                self.text.insert_str(self.cursor, &text);
                self.cursor += text.len();
            }
            EditEffect::Backspace => {
                if let Some(start) = self.text[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map(|p| p.0)
                {
                    self.text.drain(start..self.cursor);
                    self.cursor = start;
                }
            }
            EditEffect::Delete => {
                if let Some(character) = self.text[self.cursor..].chars().next() {
                    self.text
                        .drain(self.cursor..self.cursor.saturating_add(character.len_utf8()));
                }
            }
            EditEffect::Left => {
                if let Some(start) = self.text[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map(|p| p.0)
                {
                    self.cursor = start;
                }
            }
            EditEffect::Right => {
                if let Some(character) = self.text[self.cursor..].chars().next() {
                    self.cursor = self.cursor.saturating_add(character.len_utf8());
                }
            }
            EditEffect::Home => self.cursor = 0,
            EditEffect::End => self.cursor = self.text.len(),
            EditEffect::Noop => {}
        }
        self.cursor <= self.text.len() && self.text.is_char_boundary(self.cursor)
    }
}

/// 将一次 input ACK 仅应用于它创建时的精确 binding 和 line generation。
fn ticket_matches_state(ticket: &CommandLineInputTicket, state: &SessionLineProjection) -> bool {
    ticket.binding == state.binding && ticket.line_generation == state.line_generation
}

/// 队列搬运不得把另一个 pane 的请求配给当前 ticket。
fn ticket_matches_request(
    ticket: &CommandLineInputTicket,
    request: &UiTerminalInputRequest,
) -> bool {
    ticket.binding == request.binding
}

/// UI 输入 owner 与终端 service context 同代后，才允许使用该 context 推进正文。
fn context_matches_binding(binding: &UiSessionBinding, context: &TerminalCommandContext) -> bool {
    matches!(binding.owner, SessionOwner::Ui { .. })
        && context.session_id == binding.session_id
        && context.owner == binding.owner
        && context.session_generation == binding.session_generation
}

/// shell prompt 可用且不在备用屏幕时，输入 ACK 才能作为可靠编辑证据。
fn context_is_usable(context: &TerminalCommandContext) -> bool {
    context.availability == TerminalCommandAvailability::Available
        && context.prompt_ready
        && !context.alternate_screen
}

/// 只有服务明确报告空行且 prompt 已就绪时，空字符串才是可信初始投影。
fn context_can_seed_empty(context: &TerminalCommandContext) -> bool {
    context_is_usable(context) && context.prompt_line_empty
}

/// 只有已观察到空 prompt 的短暂 PromptNotReady 可在同一 epoch 恢复，其它未知状态等新 epoch。
fn context_requires_new_prompt(context: &TerminalCommandContext) -> bool {
    if context_can_seed_empty(context) {
        return false;
    }
    if context.alternate_screen || context.prompt_epoch == 0 {
        return true;
    }
    // PromptNotReady 也表示 OSC A 已开始新 prompt、尚未收到 B；不提前建空行，等待同 epoch 的可用 context。
    if context.availability
        == TerminalCommandAvailability::Unavailable(
            TerminalCommandUnavailableReason::PromptNotReady,
        )
    {
        return false;
    }
    if !context.prompt_line_empty {
        return true;
    }
    match context.availability {
        TerminalCommandAvailability::Unavailable(_) => true,
        TerminalCommandAvailability::Available => !context.prompt_ready,
    }
}

/// 只接受明确可建模的单行编辑键；历史、修饰键、粘贴和未知事件不会猜测其副作用。
fn edit_effect(event: &TerminalInputEvent) -> Option<EditEffect> {
    match event {
        TerminalInputEvent::Text(text) if !text.chars().any(char::is_control) => {
            Some(EditEffect::Insert(text.clone()))
        }
        TerminalInputEvent::Key(event) => key_effect(*event),
        TerminalInputEvent::KeyWithText { event, text, .. } => {
            if event.phase == KeyPhase::Release {
                return Some(EditEffect::Noop);
            }
            if matches!(event.key, TerminalKey::Character(_)) {
                if event.modifiers.control || event.modifiers.alt || event.modifiers.super_key {
                    return None;
                }
                return (!text.is_empty() && !text.chars().any(char::is_control))
                    .then(|| EditEffect::Insert(text.clone()));
            }
            if !text.is_empty() {
                return None;
            }
            key_effect(*event)
        }
        TerminalInputEvent::Paste(_)
        | TerminalInputEvent::Mouse(_)
        | TerminalInputEvent::Focus(_) => None,
        TerminalInputEvent::Text(_) => None,
    }
}

/// 将标准 readline 光标编辑按单次字符操作投影，其他按键保持未知以避免 shell 绑定差异。
fn key_effect(event: TerminalKeyEvent) -> Option<EditEffect> {
    if event.phase == KeyPhase::Release {
        return Some(EditEffect::Noop);
    }
    if event.modifiers != InputModifiers::default() {
        return None;
    }
    match event.key {
        TerminalKey::Backspace => Some(EditEffect::Backspace),
        TerminalKey::Delete => Some(EditEffect::Delete),
        TerminalKey::Left => Some(EditEffect::Left),
        TerminalKey::Right => Some(EditEffect::Right),
        TerminalKey::Home => Some(EditEffect::Home),
        TerminalKey::End => Some(EditEffect::End),
        _ => None,
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_command_line_projection.rs"]
mod tests;
