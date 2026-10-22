//! Inline/menu 建议的短暂输入投影与安全接受计划。
//!
//! 控制器只维护 GPUI 已明确观察到的输入，不从终端回显、快照或 readline 状态反推命令行。
//!
//! @author kongweiguang

use kerminal_application::{
    commands::{TerminalSuggestionBatch, TerminalSuggestionQuery},
    sessions::{
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
use kerminal_terminal::input::{
    InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyEvent,
};
use tokio_util::sync::CancellationToken;

const MAX_INPUT_CHARACTERS: usize = 4_000;
const MAX_INPUT_BYTES: usize = 32_000;
const MAX_ACCEPT_KEY_EVENTS: usize = 1_024;

/// 接受结果固定原始 UI owner 和终端版本；调用方逐个写入时必须沿用返回的新版本。
#[derive(Clone, Debug, Eq, PartialEq)]
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
        });
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

/// 只按候选声明的下一个 Unicode scalar boundary 执行部分接受。
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
        .filter(|boundary| *boundary > typed_length && *boundary < total)
        .min()
        .unwrap_or(total)
}

/// 用显式光标键/退格和有界 paste 实现 range edit，避免 Kitty 全键模式丢弃合成 Text。
fn build_edit_plan(
    query: &TerminalSuggestionQuery,
    candidate: &CommandSuggestionCandidate,
    replacement_text: &str,
) -> Option<(String, usize, Vec<TerminalInputEvent>)> {
    let start = candidate.replacement_range.start;
    let end = candidate.replacement_range.end;
    let current = query.input.chars().collect::<Vec<_>>();
    if start > end || end > current.len() || !valid_input(&query.input, query.cursor) {
        return None;
    }
    if candidate.activation == CommandSuggestionActivation::OpenSnippetPanel {
        return Some((query.input.clone(), query.cursor, Vec::new()));
    }

    let movement_count = query.cursor.abs_diff(end);
    let delete_count = end - start;
    let text_count = usize::from(!replacement_text.is_empty());
    if movement_count
        .saturating_add(delete_count)
        .saturating_add(text_count)
        > MAX_ACCEPT_KEY_EVENTS
    {
        return None;
    }

    let mut events = Vec::with_capacity(movement_count + delete_count + text_count);
    let movement_key = if query.cursor > end {
        TerminalKey::Left
    } else {
        TerminalKey::Right
    };
    events.extend((0..movement_count).map(|_| key_event(movement_key)));
    events.extend((0..delete_count).map(|_| key_event(TerminalKey::Backspace)));
    if !replacement_text.is_empty() {
        events.push(TerminalInputEvent::Paste(replacement_text.to_owned()));
    }

    let mut updated = String::with_capacity(query.input.len() + replacement_text.len());
    updated.extend(current[..start].iter().copied());
    updated.push_str(replacement_text);
    updated.extend(current[end..].iter().copied());
    let cursor_after = start + replacement_text.chars().count();
    if !valid_input(&updated, cursor_after) {
        return None;
    }
    Some((updated, cursor_after, events))
}

/// 统一使用无修饰的按下事件，避免平台按键状态污染 shell 的 replacement 操作。
fn key_event(key: TerminalKey) -> TerminalInputEvent {
    TerminalInputEvent::Key(TerminalKeyEvent {
        key,
        phase: KeyPhase::Press,
        modifiers: InputModifiers::default(),
    })
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
