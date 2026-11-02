//! Inline/menu 建议的短暂输入投影与安全接受计划。
//!
//! 可信 Shell 编辑快照是实际输入的权威来源；绝不从终端回显重建命令行。
//!
//! @author kongweiguang

use std::fmt;

use kerminal_application::{
    commands::{TerminalSuggestionBatch, TerminalSuggestionQuery},
    sessions::{
        SHELL_EDITING_MAX_INPUT_CHARACTERS, SHELL_EDITING_MAX_SUFFIX_BYTES, ShellEditingSnapshot,
        TerminalCommandAvailability, TerminalCommandContext, TerminalCommandContextVersion,
        UiSessionBinding,
    },
};
use kerminal_domain::{
    commands::{
        CommandSuggestionActivation, CommandSuggestionCandidate, CommandSuggestionSensitivity,
        SuggestionPresentation, SuggestionQueryMode,
    },
    sessions::SessionOwner,
};
use kerminal_terminal::input::{InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey};
use tokio_util::sync::CancellationToken;
use unicode_segmentation::UnicodeSegmentation;

const MAX_INPUT_CHARACTERS: usize = SHELL_EDITING_MAX_INPUT_CHARACTERS;
const MAX_INPUT_BYTES: usize = 32_000;
const MAX_CANDIDATES: usize = 20;

/// 接受结果固定原始 UI owner 和终端版本；调用方逐个写入时必须沿用返回的新版本。
#[derive(Clone, Eq, PartialEq)]
pub struct TerminalSuggestionAcceptance {
    pub binding: UiSessionBinding,
    pub expected_context: TerminalCommandContextVersion,
    pub request_generation: u64,
    pub candidate_id: String,
    pub provider: kerminal_domain::commands::SuggestionProviderKind,
    pub source_id: Option<String>,
    pub activation: CommandSuggestionActivation,
    pub input_before: String,
    pub input_after: String,
    pub cursor_after: usize,
    pub replacement_text: String,
    pub events: Vec<TerminalInputEvent>,
    /// 写入必须使用产生该候选的真实快照，缺失时 UI 不得退回旧的投影写入路径。
    pub shell_snapshot: Option<ShellEditingSnapshot>,
}

impl fmt::Debug for TerminalSuggestionAcceptance {
    /// 接受计划可能包含未提交凭据；诊断只展示身份与长度，不打印正文或候选标识。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TerminalSuggestionAcceptance")
            .field("binding", &self.binding)
            .field("expected_context", &self.expected_context)
            .field("request_generation", &self.request_generation)
            .field("provider", &self.provider)
            .field("activation", &self.activation)
            .field("input_before", &"[redacted]")
            .field("input_after", &"[redacted]")
            .field("replacement_text", &"[redacted]")
            .field("event_count", &self.events.len())
            .field("shell_snapshot", &self.shell_snapshot)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Projection {
    binding: UiSessionBinding,
    version: TerminalCommandContextVersion,
    input: String,
    cursor: usize,
    mode: SuggestionQueryMode,
}

enum ProjectionInputUpdate {
    Updated(String, usize),
    NoChange,
    Invalid,
}

/// 只公开完整匹配当前 session/prompt/input 版本的候选，迟到查询不能覆盖新输入。
#[derive(Default)]
pub struct InlineSuggestionController {
    projection: Option<Projection>,
    query: Option<TerminalSuggestionQuery>,
    batch: Option<TerminalSuggestionBatch>,
    query_cancellation: Option<CancellationToken>,
    /// 输入已经入队但尚无最终 ACK 时，旧查询可用于保留菜单状态，却不能再次接受结果。
    suspended: bool,
    next_request_generation: u64,
}

impl InlineSuggestionController {
    /// 创建空控制器，只有新鲜的可信 runtime context 才能开启输入投影。
    pub fn new() -> Self {
        Self::default()
    }

    /// 同步 OSC、owner 和 generation 状态；Raw 输入、切换会话或全屏程序会清除候选。
    pub fn observe_context(&mut self, context: &TerminalCommandContext) -> bool {
        if context.availability != TerminalCommandAvailability::Available
            || context.alternate_screen
            || !context.prompt_ready
            || !matches!(&context.owner, SessionOwner::Ui { .. })
        {
            self.clear();
            return false;
        }

        let binding = binding_for(context);
        let version = context.version();
        let Some(current) = self.projection.as_ref() else {
            if !context.prompt_line_empty {
                self.clear();
                return false;
            }
            self.projection = Some(empty_projection(binding, version));
            return true;
        };
        if current.binding == binding && current.version == version {
            return true;
        }

        let same_prompt = current.binding == binding
            && current.version.session_generation == version.session_generation
            && current.version.prompt_epoch == version.prompt_epoch;
        let next_tracked_input =
            current.version.input_generation.checked_add(1) == Some(version.input_generation);
        self.cancel_query();
        if same_prompt && next_tracked_input {
            if let Some(current) = self.projection.as_mut() {
                current.version = version;
            }
        } else {
            self.projection = Some(empty_projection(binding, version));
        }
        true
    }

    /// 按当前 GPUI 明确维护的文本和 Unicode 光标创建请求，并取消旧代次的高成本查询。
    pub fn update_input(
        &mut self,
        context: &TerminalCommandContext,
        input: String,
        cursor: usize,
        mode: SuggestionQueryMode,
    ) -> Option<(TerminalSuggestionQuery, CancellationToken)> {
        if !self.observe_context(context) || !valid_input(&input, cursor) {
            self.clear();
            return None;
        }
        self.make_query(input, cursor, mode)
    }

    /// 精确匹配的 Shell 读回可以恢复未知行，但不能改变旧命令生命周期 context 的含义。
    pub fn update_shell_snapshot(
        &mut self,
        context: &TerminalCommandContext,
        snapshot: &ShellEditingSnapshot,
        mode: SuggestionQueryMode,
    ) -> Option<(TerminalSuggestionQuery, CancellationToken)> {
        if snapshot.binding != binding_for(context)
            || snapshot.context != context.version()
            || !matches!(&context.owner, SessionOwner::Ui { .. })
            || !context.prompt_ready
            || context.alternate_screen
            || !valid_input(&snapshot.input, snapshot.cursor)
        {
            self.clear();
            return None;
        }
        self.projection = Some(Projection {
            binding: snapshot.binding.clone(),
            version: snapshot.context,
            input: snapshot.input.clone(),
            cursor: snapshot.cursor,
            mode,
        });
        self.make_query(snapshot.input.clone(), snapshot.cursor, mode)
    }

    /// 只有已校验的输入可分配查询代次；统一入口确保旧 provider 令牌先被撤销。
    fn make_query(
        &mut self,
        input: String,
        cursor: usize,
        mode: SuggestionQueryMode,
    ) -> Option<(TerminalSuggestionQuery, CancellationToken)> {
        self.cancel_query();
        self.next_request_generation = self.next_request_generation.wrapping_add(1).max(1);

        let projection = self.projection.as_mut()?;
        projection.input = input.clone();
        projection.cursor = cursor;
        projection.mode = mode;
        let query = TerminalSuggestionQuery {
            binding: projection.binding.clone(),
            expected_context: projection.version,
            request_generation: self.next_request_generation,
            input,
            cursor,
            mode,
        };
        let cancellation = CancellationToken::new();
        self.query = Some(query.clone());
        self.batch = None;
        self.query_cancellation = Some(cancellation.clone());
        Some((query, cancellation))
    }

    /// 按一次已成功写入的 UI 事件更新唯一输入投影；不从终端回显推导文本或光标。
    pub fn apply_tracked_input(
        &mut self,
        context: &TerminalCommandContext,
        event: &TerminalInputEvent,
        mode: SuggestionQueryMode,
    ) -> Option<(TerminalSuggestionQuery, CancellationToken)> {
        let previous_version = self
            .projection
            .as_ref()
            .map(|projection| projection.version);
        if !self.observe_context(context) {
            return None;
        }
        let version_advanced = previous_version.is_some_and(|version| version != context.version());
        let projection = self.projection.as_ref()?;
        let input_before = projection.input.clone();
        let cursor_before = projection.cursor;
        let (input, cursor) = match edit_projection(&input_before, cursor_before, event) {
            ProjectionInputUpdate::Updated(input, cursor) => (input, cursor),
            ProjectionInputUpdate::NoChange if version_advanced => {
                return self.update_input(context, input_before, cursor_before, mode);
            }
            ProjectionInputUpdate::NoChange => return None,
            ProjectionInputUpdate::Invalid => {
                self.clear();
                return None;
            }
        };
        self.update_input(context, input, cursor, mode)
    }

    /// 给 Root 的菜单快捷键复用已跟踪文本与 Unicode cursor，不再维护第二个输入缓冲。
    pub fn input_projection(&self) -> Option<(&str, usize)> {
        self.projection
            .as_ref()
            .map(|projection| (projection.input.as_str(), projection.cursor))
    }

    /// 确认建议接受仍绑定到 controller 当前投影，拒绝切换 pane 后重放旧事件。
    pub fn projection_matches(
        &self,
        binding: &UiSessionBinding,
        version: TerminalCommandContextVersion,
    ) -> bool {
        self.projection.as_ref().is_some_and(|projection| {
            &projection.binding == binding && projection.version == version
        })
    }

    /// 仅接受与活动查询和最新 session/prompt/input 版本完全一致的 provider 结果。
    pub fn apply_batch(&mut self, mut batch: TerminalSuggestionBatch) -> bool {
        if !self.is_request_active() {
            return false;
        }
        let Some(query) = self.query.as_ref() else {
            return false;
        };
        let matches = batch.binding == query.binding
            && batch.expected_context == query.expected_context
            && batch.request_generation == query.request_generation
            && self.projection.as_ref().is_some_and(|projection| {
                projection.binding == query.binding && projection.version == query.expected_context
            });
        if !matches {
            return false;
        }
        let presentation = presentation_for(query.mode);
        batch.candidates.retain(|candidate| {
            candidate.sensitivity != CommandSuggestionSensitivity::Sensitive
                && (candidate.sensitivity != CommandSuggestionSensitivity::Dangerous
                    || query.mode == SuggestionQueryMode::Menu)
                && candidate.allowed_presentations.contains(&presentation)
                && valid_candidate(candidate, &query.input)
                && (candidate.activation == CommandSuggestionActivation::OpenSnippetPanel
                    && query.mode == SuggestionQueryMode::Menu
                    || appendable_suffix(candidate, query).is_some())
        });
        batch.candidates.truncate(MAX_CANDIDATES);
        self.batch = Some(batch);
        true
    }

    /// 只借用当前有效 batch 的候选；下一次输入、context 变化或清除会立即使该列表失效。
    pub fn candidates(&self) -> &[CommandSuggestionCandidate] {
        self.batch
            .as_ref()
            .map_or(&[], |batch| batch.candidates.as_slice())
    }

    /// 暴露当前请求投影供 Root 生成 ghost 和过滤异步错误，不复制第二份 query 状态。
    ///
    /// @author kongweiguang
    pub fn current_query(&self) -> Option<&TerminalSuggestionQuery> {
        self.query.as_ref()
    }

    /// 将选中候选转成有界终端输入计划；不直接写终端，避免绕过 SessionService 的版本检查。
    pub fn accept(
        &mut self,
        candidate_id: &str,
        partial_accept: bool,
    ) -> Option<TerminalSuggestionAcceptance> {
        if !self.is_request_active() {
            return None;
        }
        let query = self.query.as_ref()?;
        let projection = self.projection.as_ref()?;
        let batch = self.batch.as_ref()?;
        if projection.binding != query.binding
            || projection.version != query.expected_context
            || batch.binding != query.binding
            || batch.expected_context != query.expected_context
            || batch.request_generation != query.request_generation
        {
            self.clear();
            return None;
        }
        let candidate = batch
            .candidates
            .iter()
            .find(|candidate| candidate.id == candidate_id)?;
        let replacement_end = accepted_replacement_end(candidate, query, partial_accept);
        let replacement_text = candidate
            .replacement_text
            .chars()
            .take(replacement_end)
            .collect::<String>();
        let (input_after, cursor_after, events) =
            build_edit_plan(query, candidate, replacement_text.as_str())?;
        let acceptance = TerminalSuggestionAcceptance {
            binding: query.binding.clone(),
            expected_context: query.expected_context,
            request_generation: query.request_generation,
            candidate_id: candidate.id.clone(),
            provider: candidate.provider,
            source_id: candidate.source_id.clone(),
            activation: candidate.activation,
            input_before: query.input.clone(),
            input_after,
            cursor_after,
            replacement_text,
            events,
            shell_snapshot: None,
        };
        self.batch = None;
        self.cancel_query();
        Some(acceptance)
    }

    /// 失焦、未知编辑、Tab 切换或部分写失败时同步取消请求并清除所有可接受状态。
    pub fn clear(&mut self) {
        self.cancel_query();
        self.projection = None;
        self.query = None;
        self.batch = None;
    }

    /// 只取消当前 provider 查询并移除旧候选，保留可信输入投影供 UI 反馈错误后重试。
    pub fn cancel_query(&mut self) {
        if let Some(cancellation) = self.query_cancellation.take() {
            cancellation.cancel();
        }
        self.query = None;
        self.batch = None;
        self.suspended = false;
    }

    /// 输入入队后立即撤销当前结果，但保留菜单模式，直到同一 FIFO 的最新回执重建查询。
    pub fn suspend_query(&mut self) {
        if let Some(cancellation) = self.query_cancellation.take() {
            cancellation.cancel();
        }
        self.batch = None;
        self.suspended = true;
    }

    /// 被撤销的代次不能接收迟到结果，也不能用旧输入重新打开查询。
    pub fn is_request_active(&self) -> bool {
        !self.suspended
            && self.query.is_some()
            && self
                .query_cancellation
                .as_ref()
                .is_some_and(|cancellation| !cancellation.is_cancelled())
    }

    /// 菜单快捷键在输入尚未回执时必须等待，避免用旧投影新建合法查询。
    pub fn is_suspended(&self) -> bool {
        self.suspended
    }
}

/// 从 runtime context 提取可写入的精确 owner binding，不允许从 active Tab 重新路由。
fn binding_for(context: &TerminalCommandContext) -> UiSessionBinding {
    UiSessionBinding {
        session_id: context.session_id.clone(),
        owner: context.owner.clone(),
        session_generation: context.session_generation,
    }
}

/// 限制文本长度、光标位置和控制字符，保证 Unicode scalar replacement range 有界。
fn valid_input(input: &str, cursor: usize) -> bool {
    let characters = input.chars().count();
    characters <= MAX_INPUT_CHARACTERS
        && input.len() <= MAX_INPUT_BYTES
        && cursor <= characters
        && !input.chars().any(char::is_control)
}

/// 过滤无效替换区间和控制文本，避免 provider 结果变成换行、转义或提交按键。
fn valid_candidate(candidate: &CommandSuggestionCandidate, input: &str) -> bool {
    let input_length = input.chars().count();
    let replacement_length = candidate.replacement_text.chars().count();
    candidate.replacement_range.start <= candidate.replacement_range.end
        && candidate.replacement_range.end <= input_length
        && candidate.score.is_finite()
        && safe_candidate_text(&candidate.id)
        && safe_candidate_text(&candidate.display_text)
        && safe_candidate_text(&candidate.suffix)
        && replacement_length <= MAX_INPUT_CHARACTERS
        && candidate.replacement_text.len() <= MAX_INPUT_BYTES
        && !candidate.replacement_text.chars().any(char::is_control)
        && candidate
            .description
            .as_deref()
            .is_none_or(safe_candidate_text)
        && candidate
            .source_explanation
            .as_deref()
            .is_none_or(safe_candidate_text)
        && candidate
            .merged_source_explanations
            .iter()
            .all(|value| safe_candidate_text(value))
}

/// 控制 provider 返回的可见文本尺寸，并拒绝终端控制字符污染 GPUI 标签。
fn safe_candidate_text(value: &str) -> bool {
    value.chars().count() <= MAX_INPUT_CHARACTERS
        && value.len() <= MAX_INPUT_BYTES
        && !value.chars().any(char::is_control)
}

/// 查询模式决定展示位置；危险候选只会通过 domain allowlist 留在主动菜单。
fn presentation_for(mode: SuggestionQueryMode) -> SuggestionPresentation {
    match mode {
        SuggestionQueryMode::Inline => SuggestionPresentation::Inline,
        SuggestionQueryMode::Menu => SuggestionPresentation::Menu,
    }
}

/// 只展示原文严格匹配且 writer 可追加的后缀；Unicode 字符数和 UTF-8 字节数分别校验。
pub(crate) fn appendable_suffix<'a>(
    candidate: &'a CommandSuggestionCandidate,
    query: &TerminalSuggestionQuery,
) -> Option<&'a str> {
    if candidate.activation != CommandSuggestionActivation::Insert
        || query.cursor != query.input.chars().count()
        || candidate.replacement_range.end != query.cursor
        || candidate.replacement_range.start > query.cursor
    {
        return None;
    }
    let typed = query
        .input
        .chars()
        .skip(candidate.replacement_range.start)
        .collect::<String>();
    candidate
        .replacement_text
        .strip_prefix(&typed)
        .filter(|suffix| {
            !suffix.is_empty()
                && suffix.len() <= SHELL_EDITING_MAX_SUFFIX_BYTES
                && query.cursor + suffix.chars().count() <= SHELL_EDITING_MAX_INPUT_CHARACTERS
        })
}

/// 按候选声明的 Unicode scalar 边界分段，但不得把组合字符截断在字素内部。
fn accepted_replacement_end(
    candidate: &CommandSuggestionCandidate,
    query: &TerminalSuggestionQuery,
    partial_accept: bool,
) -> usize {
    let total = candidate.replacement_text.chars().count();
    if !partial_accept || query.mode != SuggestionQueryMode::Inline {
        return total;
    }
    let typed_length = candidate
        .replacement_range
        .end
        .saturating_sub(candidate.replacement_range.start);
    candidate
        .accept_boundaries
        .iter()
        .copied()
        .filter(|boundary| {
            *boundary > typed_length
                && *boundary < total
                && candidate
                    .replacement_text
                    .grapheme_indices(true)
                    .any(|(offset, _)| {
                        candidate.replacement_text[..offset].chars().count() == *boundary
                    })
                && !ends_with_unpaired_escape(&candidate.replacement_text, *boundary)
        })
        .min()
        .unwrap_or(total)
}

/// Shell 路径中的反斜杠和 PowerShell 反引号可能转义后续字符，不能把它们单独追加。
fn ends_with_unpaired_escape(text: &str, boundary: usize) -> bool {
    let prefix = text.chars().take(boundary).collect::<Vec<_>>();
    let Some(&last) = prefix.last() else {
        return false;
    };
    matches!(last, '\\' | '`')
        && prefix
            .iter()
            .rev()
            .take_while(|character| **character == last)
            .count()
            % 2
            == 1
}

/// 仅生成一个后缀粘贴事件，不移动光标、不删除内容，也绝不隐式执行命令。
fn build_edit_plan(
    query: &TerminalSuggestionQuery,
    candidate: &CommandSuggestionCandidate,
    replacement_text: &str,
) -> Option<(String, usize, Vec<TerminalInputEvent>)> {
    let start = candidate.replacement_range.start;
    let end = candidate.replacement_range.end;
    if start > end || !valid_input(&query.input, query.cursor) {
        return None;
    }
    if candidate.activation == CommandSuggestionActivation::OpenSnippetPanel {
        return Some((query.input.clone(), query.cursor, Vec::new()));
    }
    appendable_suffix(candidate, query)?;
    let typed = query.input.chars().skip(start).collect::<String>();
    let suffix = replacement_text
        .strip_prefix(&typed)
        .filter(|text| !text.is_empty())?;
    let mut updated = query.input.clone();
    updated.push_str(suffix);
    let cursor_after = query.cursor + suffix.chars().count();
    if !valid_input(&updated, cursor_after) {
        return None;
    }
    Some((
        updated,
        cursor_after,
        vec![TerminalInputEvent::Paste(suffix.to_owned())],
    ))
}

/// 通过一次可信 context 初始化空行投影；会话目标仍由 binding 独立持有。
fn empty_projection(
    binding: UiSessionBinding,
    version: TerminalCommandContextVersion,
) -> Projection {
    Projection {
        binding,
        version,
        input: String::new(),
        cursor: 0,
        mode: SuggestionQueryMode::Inline,
    }
}

/// 只模拟可以从已提交的 UI 事件精确还原的文本插入、光标移动和删除操作。
fn edit_projection(
    input: &str,
    cursor: usize,
    event: &TerminalInputEvent,
) -> ProjectionInputUpdate {
    match event {
        TerminalInputEvent::Text(text) | TerminalInputEvent::Paste(text) => {
            insert_projection_text(input, cursor, text)
        }
        TerminalInputEvent::KeyWithText { event, text, .. }
            if event.phase != KeyPhase::Release
                && !event.modifiers.control
                && !event.modifiers.alt
                && !event.modifiers.super_key =>
        {
            insert_projection_text(input, cursor, text)
        }
        TerminalInputEvent::KeyWithText { event, .. } if event.phase == KeyPhase::Release => {
            ProjectionInputUpdate::NoChange
        }
        TerminalInputEvent::Key(key) if key.phase == KeyPhase::Release => {
            ProjectionInputUpdate::NoChange
        }
        TerminalInputEvent::Key(key) if key.modifiers != InputModifiers::default() => {
            ProjectionInputUpdate::Invalid
        }
        TerminalInputEvent::Key(key) => {
            let mut characters = input.chars().collect::<Vec<_>>();
            if cursor > characters.len() {
                return ProjectionInputUpdate::Invalid;
            }
            let next_cursor = match key.key {
                // Character 没有附带已提交文本，无法证明它与后续 IME/文本事件一一对应。
                TerminalKey::Character(_) => return ProjectionInputUpdate::Invalid,
                TerminalKey::Backspace => {
                    if cursor > 0 {
                        characters.remove(cursor - 1);
                    }
                    cursor.saturating_sub(1)
                }
                TerminalKey::Delete => {
                    if cursor < characters.len() {
                        characters.remove(cursor);
                    }
                    cursor
                }
                TerminalKey::Left => cursor.saturating_sub(1),
                TerminalKey::Right => cursor.saturating_add(1).min(characters.len()),
                TerminalKey::Home => 0,
                TerminalKey::End => characters.len(),
                TerminalKey::Enter
                | TerminalKey::Tab
                | TerminalKey::Escape
                | TerminalKey::BackTab
                | TerminalKey::Up
                | TerminalKey::Down
                | TerminalKey::Insert
                | TerminalKey::PageUp
                | TerminalKey::PageDown
                | TerminalKey::Function(_) => return ProjectionInputUpdate::Invalid,
            };
            let updated = characters.into_iter().collect::<String>();
            if valid_input(&updated, next_cursor) {
                ProjectionInputUpdate::Updated(updated, next_cursor)
            } else {
                ProjectionInputUpdate::Invalid
            }
        }
        TerminalInputEvent::KeyWithText { .. }
        | TerminalInputEvent::Mouse(_)
        | TerminalInputEvent::Focus(_) => ProjectionInputUpdate::Invalid,
    }
}

/// 在 Unicode scalar cursor 插入明确文本；超长或控制字符立即使投影失效。
fn insert_projection_text(input: &str, cursor: usize, text: &str) -> ProjectionInputUpdate {
    if text.is_empty() {
        return ProjectionInputUpdate::NoChange;
    }
    let mut characters = input.chars().collect::<Vec<_>>();
    if cursor > characters.len() || text.chars().any(char::is_control) {
        return ProjectionInputUpdate::Invalid;
    }
    let inserted_characters = text.chars().count();
    characters.splice(cursor..cursor, text.chars());
    let updated = characters.into_iter().collect::<String>();
    let next_cursor = cursor.saturating_add(inserted_characters);
    if valid_input(&updated, next_cursor) {
        ProjectionInputUpdate::Updated(updated, next_cursor)
    } else {
        ProjectionInputUpdate::Invalid
    }
}
