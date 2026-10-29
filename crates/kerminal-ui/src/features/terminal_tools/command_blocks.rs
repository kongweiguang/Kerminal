//! OSC 133 命令块卡片与有界转录投影；不改写终端网格或推测输出来源。
//!
//! @author kongweiguang

use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use gpui_kit::{
    ClickEvent, Context, EventEmitter, IntoElement, Render, Window, component::button::Button, div,
    prelude::*, px,
};
use kerminal_application::sessions::{
    CommandBlockId, CommandBlockState, CommandBlockText, CommandBlockTextUnknownReason,
    CommandBlockUnknownReason,
};
use kerminal_domain::identity::{AgentSessionId, SessionId};

use crate::theme::ThemeTokens;

#[path = "command_block_image.rs"]
pub(crate) mod image_export;

const MAX_VISIBLE_BLOCKS: usize = 128;
const MAX_COMMAND_BYTES: usize = 4096;
const MAX_OUTPUT_BYTES: usize = 12 * 1024;
const MAX_COPY_BYTES: usize = 256 * 1024;
const TRUNCATION_MARKER: &str = "\n… 输出已裁切 …";

/// 按来源命令块和目标 Agent 保存未知结果提示，不绑定会变化的输出 revision。
#[derive(Clone, Default)]
struct AgentSubmissionUnknowns {
    by_block: HashMap<CommandBlockId, HashSet<AgentSessionId>>,
}

impl AgentSubmissionUnknowns {
    /// 记录目标 Agent 的未知结果；源输出变化不能证明先前输入是否送达。
    fn mark(&mut self, block_id: CommandBlockId, agent_session_id: AgentSessionId) {
        self.by_block
            .entry(block_id)
            .or_default()
            .insert(agent_session_id);
    }

    /// 仅当来源快照驱逐区块时释放其未知提示。
    fn retain_live_blocks(&mut self, live_blocks: &HashSet<CommandBlockId>) {
        self.by_block
            .retain(|block_id, _| live_blocks.contains(block_id));
    }

    /// 来源会话或连接代次切换时丢弃旧视图投影。
    fn clear(&mut self) {
        self.by_block.clear();
    }

    /// 为卡片提供所有仍待核验的目标，不影响这些 Agent 以外的发送动作。
    fn agents_for(&self, block_id: CommandBlockId) -> Vec<AgentSessionId> {
        self.by_block
            .get(&block_id)
            .map(|agents| agents.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// 当前已知的输出文本只能来自独立 transcript 查询，绝不从屏幕行或命令块正文推导。
#[derive(Clone, Eq, PartialEq)]
pub enum CommandBlockContent {
    NotRequested,
    Loading,
    Available {
        text: String,
        capture_revision: Option<u64>,
        truncated: bool,
        complete: bool,
    },
    Incomplete {
        text: String,
        capture_revision: Option<u64>,
        reason: String,
        truncated: bool,
    },
    Unavailable(CommandBlockContentUnavailable),
}

/// 内容查询无可复制正文时给出的稳定原因；对外部错误仅保存已脱敏的 public message。
#[derive(Clone, Eq, PartialEq)]
pub enum CommandBlockContentUnavailable {
    NotCaptured,
    SensitiveOrUnsafe,
    WriteNotAcknowledged,
    Evicted,
    CaptureUnavailable,
    Incomplete(String),
    QueryFailed(String),
}

/// 命令块的真实元数据和独立输出转录，供卡片操作按稳定领域 ID 关联。
#[derive(Clone, Eq, PartialEq)]
pub struct CommandBlockProjection {
    pub id: CommandBlockId,
    pub command: CommandBlockText,
    pub state: CommandBlockState,
    pub content: CommandBlockContent,
}

/// 最新可安全预填的已提交命令块身份；Root 必须按其 block ID 读取 transcript。
///
/// @author kongweiguang
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct LatestSubmittedCommandBlock {
    pub(crate) id: CommandBlockId,
    pub(crate) session_generation: u64,
    pub(crate) capture_revision: Option<u64>,
    pub(crate) command: String,
}

impl fmt::Debug for LatestSubmittedCommandBlock {
    /// 诊断只保留命令块身份和 revision，不把终端命令正文写进格式化输出。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LatestSubmittedCommandBlock")
            .field("id", &self.id)
            .field("session_generation", &self.session_generation)
            .field("capture_revision", &self.capture_revision)
            .field("command", &"<redacted>")
            .finish()
    }
}

/// 用户在某个卡片上明确发起的操作；Root 会再次验证来源和资源代次。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandBlockAction {
    ToggleExpanded { expanded: bool },
    CopyText,
    CopyImage,
    SendToAgent,
}

/// 子控件发布的意图始终绑定创建它的会话和 OSC 命令块 ID。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandBlockEvent {
    pub session_id: SessionId,
    pub block_id: CommandBlockId,
    pub capture_revision: Option<u64>,
    pub action: CommandBlockAction,
}

/// 仅供命令块卡片显示的 UTF-8 byte range；range 会在渲染前校正字符边界。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextHighlightRange {
    pub start_byte: usize,
    pub end_byte: usize,
}

/// 一个经 UTF-8 边界校验的展示片段。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextSegment {
    pub text: String,
    pub highlighted: bool,
}

/// 命令卡片保留折叠状态，但只保存被当前有界快照引用的条目。
pub struct CommandBlockTimeline {
    session_id: Option<SessionId>,
    session_generation: Option<u64>,
    revision: u64,
    blocks: Vec<CommandBlockProjection>,
    expanded: HashSet<CommandBlockId>,
    notices: HashMap<CommandBlockId, String>,
    agent_submission_unknowns: AgentSubmissionUnknowns,
    snapshot_error: Option<String>,
}

impl EventEmitter<CommandBlockEvent> for CommandBlockTimeline {}

impl CommandBlockTimeline {
    /// 以空快照启动；没有已知 session 和 OSC 数据时不显示占位内容。
    pub fn new() -> Self {
        Self {
            session_id: None,
            session_generation: None,
            revision: 0,
            blocks: Vec::new(),
            expanded: HashSet::new(),
            notices: HashMap::new(),
            agent_submission_unknowns: AgentSubmissionUnknowns::default(),
            snapshot_error: None,
        }
    }

    /// session 或 OSC generation 改变时清除旧卡片，避免重连后复用命令块身份。
    pub fn set_session(&mut self, session_id: Option<SessionId>, cx: &mut Context<Self>) {
        if self.session_id == session_id {
            return;
        }
        self.session_id = session_id;
        self.session_generation = None;
        self.revision = 0;
        self.blocks.clear();
        self.expanded.clear();
        self.notices.clear();
        self.agent_submission_unknowns.clear();
        self.snapshot_error = None;
        cx.notify();
    }

    /// 仅接受本 session 的新快照，保留仍存在 block 的异步正文和用户折叠状态。
    pub fn set_blocks(
        &mut self,
        session_id: &SessionId,
        session_generation: u64,
        revision: u64,
        blocks: Vec<CommandBlockProjection>,
        cx: &mut Context<Self>,
    ) {
        if self.session_id.as_ref() != Some(session_id) {
            return;
        }
        if !should_accept_snapshot(
            self.session_generation,
            self.revision,
            session_generation,
            revision,
        ) {
            return;
        }

        let generation_changed = self.session_generation != Some(session_generation);
        if generation_changed {
            self.expanded.clear();
            self.notices.clear();
            self.agent_submission_unknowns.clear();
            self.blocks.clear();
        }
        let mut previous = self
            .blocks
            .drain(..)
            .map(|block| (block.id, block))
            .collect::<HashMap<_, _>>();
        let blocks = blocks
            .into_iter()
            .rev()
            .take(MAX_VISIBLE_BLOCKS)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|mut block| {
                if let Some(old) = previous.remove(&block.id)
                    && block.content == CommandBlockContent::NotRequested
                {
                    block.content = old.content;
                }
                block
            })
            .collect::<Vec<_>>();
        let live_ids = blocks.iter().map(|block| block.id).collect::<HashSet<_>>();
        self.expanded.retain(|block_id| live_ids.contains(block_id));
        self.notices
            .retain(|block_id, _| live_ids.contains(block_id));
        self.agent_submission_unknowns.retain_live_blocks(&live_ids);
        self.session_generation = Some(session_generation);
        self.revision = revision;
        self.blocks = blocks;
        self.snapshot_error = None;
        cx.notify();
    }

    /// 内容异步返回时同时核对 session generation 和仍保留的 block ID。
    pub fn set_content(
        &mut self,
        session_id: &SessionId,
        session_generation: u64,
        block_id: CommandBlockId,
        content: CommandBlockContent,
        cx: &mut Context<Self>,
    ) {
        if self.session_id.as_ref() != Some(session_id)
            || self.session_generation != Some(session_generation)
            || !self.blocks.iter().any(|block| block.id == block_id)
        {
            return;
        }
        if let Some(block) = self.blocks.iter_mut().find(|block| block.id == block_id) {
            block.content = content;
        }
        cx.notify();
    }

    /// 保存目标 Agent 的未知结果提示；新 transcript 和后续 ACK 都不能将其当作已核验。
    pub fn mark_agent_submission_unknown(
        &mut self,
        session_id: &SessionId,
        session_generation: u64,
        block_id: CommandBlockId,
        agent_session_id: AgentSessionId,
        cx: &mut Context<Self>,
    ) {
        if self.session_id.as_ref() != Some(session_id)
            || self.session_generation != Some(session_generation)
            || !self.blocks.iter().any(|block| block.id == block_id)
        {
            return;
        }
        self.agent_submission_unknowns
            .mark(block_id, agent_session_id);
        cx.notify();
    }

    /// 操作回执只写回仍在当前 generation 的卡片，不把旧失败显示到新 session。
    pub fn set_notice(
        &mut self,
        session_id: &SessionId,
        session_generation: u64,
        block_id: CommandBlockId,
        notice: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.session_id.as_ref() != Some(session_id)
            || self.session_generation != Some(session_generation)
            || !self.blocks.iter().any(|block| block.id == block_id)
        {
            return;
        }
        if let Some(notice) = notice {
            self.notices.insert(block_id, notice);
        } else {
            self.notices.remove(&block_id);
        }
        cx.notify();
    }

    /// 快照查询失败时保留可见诊断，避免空 rail 看起来像查询成功但无命令。
    pub fn set_snapshot_error(
        &mut self,
        session_id: &SessionId,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if self.session_id.as_ref() != Some(session_id) {
            return;
        }
        self.snapshot_error = Some(message);
        cx.notify();
    }

    /// Refresh 事件只重新读取已展开块的转录，避免将 session 全部历史正文载入 UI。
    pub fn expanded_block_ids(&self) -> Vec<CommandBlockId> {
        self.expanded.iter().copied().collect()
    }

    /// 返回此视图的权威 session/generation，供 Root 绑定异步任务 owner。
    pub fn binding(&self) -> Option<(&SessionId, u64)> {
        Some((self.session_id.as_ref()?, self.session_generation?))
    }

    /// 命令块列表为空时隐藏整个 rail，不创建伪执行记录。
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.snapshot_error.is_none()
    }

    /// 返回仍属于当前快照的区块，供 Root 在复制或异步动作前重新校验内容。
    pub(crate) fn block(&self, block_id: CommandBlockId) -> Option<&CommandBlockProjection> {
        self.blocks.iter().find(|block| block.id == block_id)
    }

    /// 明确校验卡片 ID 属于当前有界快照，避免过期按钮触发无意义的领域查询。
    pub(crate) fn contains_block(&self, block_id: CommandBlockId) -> bool {
        self.block(block_id).is_some()
    }

    /// 只将当前卡片已有的真实转录组成 payload，未读取或未知正文保持不可导出。
    pub(crate) fn export_text(&self, block_id: CommandBlockId) -> Option<String> {
        block_export_text(self.block(block_id)?)
    }

    /// 返回该卡片当前可导出的 transcript 代次，操作事件据此拒绝过期内容。
    pub(crate) fn export_revision(&self, block_id: CommandBlockId) -> Option<u64> {
        block_export_revision(self.block(block_id)?)
    }

    /// 读取当前 session/generation 内卡片的扩展状态，供 Root 只查询明确展开的转录。
    pub(crate) fn is_expanded(&self, block_id: CommandBlockId) -> bool {
        self.expanded.contains(&block_id)
    }

    /// 只暴露当前有界快照的稳定身份，让 Root 回收被驱逐块的异步请求。
    pub(crate) fn block_ids(&self) -> Vec<CommandBlockId> {
        self.blocks.iter().map(|block| block.id).collect()
    }

    /// 与 main 一样从最近已提交的非空块取值；最新块正文未知时不回退旧命令。
    ///
    /// @author kongweiguang
    pub(crate) fn latest_submitted_command_block(&self) -> Option<LatestSubmittedCommandBlock> {
        let block = self
            .blocks
            .iter()
            .rev()
            .find(|block| match &block.command {
                CommandBlockText::Known(command) => !command.trim().is_empty(),
                CommandBlockText::Unknown(_) => true,
            })?;
        let (_, session_generation) = self.binding()?;
        if block.id.session_generation != session_generation {
            return None;
        }
        let CommandBlockText::Known(command) = &block.command else {
            return None;
        };

        Some(LatestSubmittedCommandBlock {
            id: block.id,
            session_generation,
            capture_revision: self.export_revision(block.id),
            command: command.clone(),
        })
    }

    /// 用安全状态覆盖旧正文，避免内容查询失败时继续暴露过期输出。
    pub(crate) fn set_content_state(
        &mut self,
        session_id: &SessionId,
        session_generation: u64,
        block_id: CommandBlockId,
        content: CommandBlockContent,
        cx: &mut Context<Self>,
    ) {
        self.set_content(session_id, session_generation, block_id, content, cx);
    }

    /// 用户只在显式展开时读取输出，异步查询由 Root 负责取消和 owner 校验。
    fn toggle_expanded(&mut self, block_id: CommandBlockId, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        let expanded = if self.expanded.remove(&block_id) {
            false
        } else {
            self.expanded.insert(block_id);
            true
        };
        if expanded
            && let Some(block) = self.blocks.iter_mut().find(|block| block.id == block_id)
            && block.content == CommandBlockContent::NotRequested
        {
            block.content = CommandBlockContent::Loading;
        }
        self.emit_action(
            session_id,
            block_id,
            CommandBlockAction::ToggleExpanded { expanded },
            cx,
        );
        cx.notify();
    }

    /// 事件保持稳定 session 与 block 目标，Root 再核对真实 pane/Agent owner。
    fn emit_action(
        &self,
        session_id: SessionId,
        block_id: CommandBlockId,
        action: CommandBlockAction,
        cx: &mut Context<Self>,
    ) {
        cx.emit(CommandBlockEvent {
            session_id,
            block_id,
            capture_revision: self.export_revision(block_id),
            action,
        });
    }
}

/// 只有策略启用且存在真实命令块时才展示时间线，避免空面板或伪占位内容。
pub fn command_block_timeline_visible(rail_visible: bool, has_blocks: bool) -> bool {
    rail_visible && has_blocks
}

/// 只接受当前 OSC generation 的非倒退 revision；新 generation 可从较小 revision 重启。
pub(crate) fn should_accept_snapshot(
    current_generation: Option<u64>,
    current_revision: u64,
    incoming_generation: u64,
    incoming_revision: u64,
) -> bool {
    current_generation != Some(incoming_generation) || incoming_revision >= current_revision
}

impl Default for CommandBlockTimeline {
    /// 默认 timeline 必须为空，直到 Root 提供有来源的命令块投影。
    fn default() -> Self {
        Self::new()
    }
}

impl Render for CommandBlockTimeline {
    /// 显示独立折叠卡片和有来源的内容；状态与错误使用统一可读字号，窄卡片可换行，终端正文保持原有紧凑字号。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        let session_id = self.session_id.clone();
        let expanded = self.expanded.clone();
        let notices = self.notices.clone();
        let agent_submission_unknowns = self.agent_submission_unknowns.clone();
        let snapshot_error = self.snapshot_error.clone();
        let cards = self
            .blocks
            .iter()
            .rev()
            .map(|block| {
                let block_id = block.id;
                let is_expanded = expanded.contains(&block_id);
                let command = command_label(&block.command);
                let status = outcome_label(block.state);
                let status_color = match block.state {
                    CommandBlockState::Finished {
                        exit_code: Some(code),
                    } if code != 0 => theme.error,
                    CommandBlockState::Unknown(_) => theme.error,
                    _ => theme.muted,
                };
                let expand_label = if is_expanded {
                    crate::locale::text("收起输出", "Collapse output")
                } else {
                    crate::locale::text("展开输出", "Expand output")
                };
                let mut header = div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .min_w(px(0.))
                            .text_size(theme.text_xs)
                            .text_color(status_color)
                            .child(status),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new(format!("terminal-command-block-expand-{block_id:?}"))
                            .compact()
                            .label(expand_label)
                            .on_click(cx.listener(move |timeline, _: &ClickEvent, _, cx| {
                                timeline.toggle_expanded(block_id, cx);
                            })),
                    );
                let can_export = block_export_text(block).is_some();
                if can_export {
                    let copy_session = session_id.clone();
                    header = header.child(
                        Button::new(format!("terminal-command-block-copy-text-{block_id:?}"))
                            .compact()
                            .label(crate::locale::text("复制文本", "Copy text"))
                            .on_click(cx.listener(move |timeline, _: &ClickEvent, _, cx| {
                                if let Some(session_id) = copy_session.clone() {
                                    timeline.emit_action(
                                        session_id,
                                        block_id,
                                        CommandBlockAction::CopyText,
                                        cx,
                                    );
                                }
                            })),
                    );
                    let image_session = session_id.clone();
                    header = header.child(
                        Button::new(format!("terminal-command-block-copy-image-{block_id:?}"))
                            .compact()
                            .label(crate::locale::text("复制图片", "Copy image"))
                            .on_click(cx.listener(move |timeline, _: &ClickEvent, _, cx| {
                                if let Some(session_id) = image_session.clone() {
                                    timeline.emit_action(
                                        session_id,
                                        block_id,
                                        CommandBlockAction::CopyImage,
                                        cx,
                                    );
                                }
                            })),
                    );
                    let agent_session = session_id.clone();
                    header = header.child(
                        Button::new(format!("terminal-command-block-send-agent-{block_id:?}"))
                            .compact()
                            .label(crate::locale::text("发送到 Agent", "Send to Agent"))
                            .on_click(cx.listener(move |timeline, _: &ClickEvent, _, cx| {
                                if let Some(session_id) = agent_session.clone() {
                                    timeline.emit_action(
                                        session_id,
                                        block_id,
                                        CommandBlockAction::SendToAgent,
                                        cx,
                                    );
                                }
                            })),
                    );
                }

                let mut card = div()
                    .id(format!("terminal-command-block-{}", block_dom_id(block_id)))
                    .w_full()
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .rounded_md()
                    .bg(theme.panel)
                    .text_color(theme.foreground)
                    .child(header)
                    .child(div().text_color(theme.foreground).child(command));

                if is_expanded {
                    card = card.child(render_content(&block.content, &theme));
                }
                if let Some(notice) = notices.get(&block_id) {
                    card = card.child(
                        div()
                            .id(format!(
                                "terminal-command-block-notice-{}",
                                block_dom_id(block_id)
                            ))
                            .w_full()
                            .text_size(theme.text_xs)
                            .text_color(theme.error)
                            .child(notice.clone()),
                    );
                }
                for agent_session_id in agent_submission_unknowns.agents_for(block_id) {
                    card = card.child(
                        div()
                            .id(format!(
                                "terminal-command-block-agent-unknown-{}-{}",
                                block_dom_id(block_id),
                                agent_session_id
                            ))
                            .w_full()
                            .text_size(theme.text_xs)
                            .text_color(theme.error)
                            .child(crate::locale::text(
                                format!(
                                    "向 Agent {agent_session_id} 的上次提交结果仍未知；再次发送可能重复，请先检查该 Agent 终端。"
                                ),
                                format!(
                                    "The previous submission to Agent {agent_session_id} is still unknown; another send may duplicate it. Inspect that Agent terminal first."
                                ),
                            )),
                    );
                }
                card
            })
            .collect::<Vec<_>>();

        let mut timeline = div()
            .id("terminal-command-block-timeline")
            .w_full()
            .flex()
            .flex_col()
            .gap_1();
        if let Some(error) = snapshot_error {
            timeline = timeline.child(
                div()
                    .id("terminal-command-block-snapshot-error")
                    .w_full()
                    .p_2()
                    .rounded_md()
                    .text_color(theme.error)
                    .child(error),
            );
        }
        timeline.children(cards)
    }
}

/// 生成脱敏状态文本；未知命令正文不会进入元素树或 Debug 输出。
fn command_label(command: &CommandBlockText) -> String {
    match command {
        CommandBlockText::Known(command) => {
            format!("$ {}", truncate_at_boundary(command, MAX_COMMAND_BYTES))
        }
        CommandBlockText::Unknown(reason) => match reason {
            CommandBlockTextUnknownReason::NotCaptured => {
                crate::locale::text("命令正文未捕获", "Command text was not captured").to_string()
            }
            CommandBlockTextUnknownReason::SensitiveOrUnsafe => crate::locale::text(
                "命令正文因安全规则隐藏",
                "Command text is hidden by safety rules",
            )
            .to_string(),
            CommandBlockTextUnknownReason::WriteNotAcknowledged => {
                crate::locale::text("命令正文未确认写入", "Command text was not acknowledged")
                    .to_string()
            }
        },
    }
}

/// 退出码保留真实值；Unknown 的原因也显式展示，不把断线伪装成失败或成功。
fn outcome_label(state: CommandBlockState) -> String {
    match state {
        CommandBlockState::Running => crate::locale::text("执行中", "Running").to_string(),
        CommandBlockState::Finished { exit_code: Some(0) } => {
            crate::locale::text("成功 · 退出码 0", "Succeeded · exit code 0").to_string()
        }
        CommandBlockState::Finished {
            exit_code: Some(code),
        } => crate::locale::text(
            format!("失败 · 退出码 {code}"),
            format!("Failed · exit code {code}"),
        )
        .to_string(),
        CommandBlockState::Finished { exit_code: None } => {
            crate::locale::text("已结束 · 退出码未知", "Finished · exit code unknown").to_string()
        }
        CommandBlockState::Unknown(reason) => unknown_state_label(reason),
    }
}

/// 将状态未知原因映射成短提示，避免静默降级为普通运行状态。
fn unknown_state_label(reason: CommandBlockUnknownReason) -> String {
    match reason {
        CommandBlockUnknownReason::Superseded => {
            crate::locale::text("结果被新命令取代", "Superseded").to_string()
        }
        CommandBlockUnknownReason::Disconnected => {
            crate::locale::text("连接已断开", "Disconnected").to_string()
        }
        CommandBlockUnknownReason::ReaderFailed => {
            crate::locale::text("输出读取失败", "Output read failed").to_string()
        }
        CommandBlockUnknownReason::WriterFailed => {
            crate::locale::text("输入写入失败", "Input write failed").to_string()
        }
        CommandBlockUnknownReason::Closed => {
            crate::locale::text("会话已关闭", "Session closed").to_string()
        }
        CommandBlockUnknownReason::Reconnected => {
            crate::locale::text("会话已重连", "Session reconnected").to_string()
        }
    }
}

/// 仅拼接命令与已知/部分已知转录，敏感或被驱逐正文不能被复制或送给 Agent。
pub(crate) fn block_export_text(block: &CommandBlockProjection) -> Option<String> {
    let CommandBlockText::Known(command) = &block.command else {
        return None;
    };
    block_export_revision(block)?;
    let (output, status_suffix) = match &block.content {
        CommandBlockContent::Available {
            text,
            truncated,
            complete,
            ..
        } => (
            text.as_str(),
            if *truncated {
                crate::locale::text("\n… 转录已截断 …", "\n… transcript was truncated …")
                    .to_string()
            } else if !complete {
                crate::locale::text(
                    "\n… 输出仍在采集；当前内容可能不完整 …",
                    "\n… output is still being captured and may be incomplete …",
                )
                .to_string()
            } else {
                String::new()
            },
        ),
        CommandBlockContent::Incomplete {
            text,
            reason,
            truncated,
            ..
        } => (
            text.as_str(),
            format!(
                "\n\n… {}: {}{} …",
                crate::locale::text("输出不完整", "Output is incomplete"),
                reason,
                if *truncated {
                    crate::locale::text("；转录已截断", "; transcript was truncated")
                } else {
                    "".into()
                },
            ),
        ),
        CommandBlockContent::NotRequested
        | CommandBlockContent::Loading
        | CommandBlockContent::Unavailable(_) => return None,
    };
    let mut payload = format!("$ {command}\n{output}");
    if status_suffix.is_empty() {
        return Some(truncate_with_marker(&payload, MAX_COPY_BYTES));
    }
    payload = truncate_with_marker(&payload, MAX_COPY_BYTES.saturating_sub(status_suffix.len()));
    payload.push_str(&status_suffix);
    Some(payload)
}

/// 缺少应用层 capture revision 时，正文虽可显示但不能被复制或提交到 Agent。
pub(crate) fn block_export_revision(block: &CommandBlockProjection) -> Option<u64> {
    match &block.content {
        CommandBlockContent::Available {
            capture_revision: Some(revision),
            ..
        }
        | CommandBlockContent::Incomplete {
            capture_revision: Some(revision),
            ..
        } => Some(*revision),
        _ => None,
    }
}

/// 事件发出后若同一命令块已获得新 transcript revision，必须丢弃旧动作。
pub(crate) fn capture_revision_matches(expected: Option<u64>, current: Option<u64>) -> bool {
    expected.is_some() && expected == current
}

/// 限制复制和预览共用的文本预算，避免异常采集内容占满剪贴板或图像编码内存。
fn truncate_with_marker(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    if max_bytes == 0 {
        return String::new();
    }
    if max_bytes < TRUNCATION_MARKER.len() {
        let end = previous_char_boundary(TRUNCATION_MARKER, max_bytes);
        return TRUNCATION_MARKER[..end].to_owned();
    }
    let content_budget = max_bytes.saturating_sub(TRUNCATION_MARKER.len());
    let end = previous_char_boundary(text, content_budget.min(text.len()));
    format!("{}{}", &text[..end], TRUNCATION_MARKER)
}

/// 区分预览裁切和后端截断，且不对未知内容拼出占位正文。
fn render_content(content: &CommandBlockContent, theme: &ThemeTokens) -> gpui_kit::AnyElement {
    let (text, notice) = match content {
        CommandBlockContent::NotRequested => (
            crate::locale::text("展开后读取输出。", "Expand to load output.").to_string(),
            None,
        ),
        CommandBlockContent::Loading => (
            crate::locale::text("正在读取输出…", "Loading output…").to_string(),
            None,
        ),
        CommandBlockContent::Available {
            text,
            truncated,
            complete,
            ..
        } => {
            let locally_truncated = text.len() > MAX_OUTPUT_BYTES;
            let preview = truncate_at_boundary(text, MAX_OUTPUT_BYTES);
            let notice = if *truncated || locally_truncated {
                Some(crate::locale::text("输出已截断", "Output is truncated").to_string())
            } else if !complete {
                Some(
                    crate::locale::text("输出仍在采集", "Output is still being captured")
                        .to_string(),
                )
            } else {
                None
            };
            (preview, notice)
        }
        CommandBlockContent::Incomplete {
            text,
            reason,
            truncated,
            ..
        } => {
            let preview = truncate_at_boundary(text, MAX_OUTPUT_BYTES);
            let suffix = if *truncated {
                crate::locale::text(" · 已截断", " · truncated")
            } else {
                "".into()
            };
            (
                preview,
                Some(format!(
                    "{}{} · {}",
                    crate::locale::text("输出不完整", "Incomplete output"),
                    suffix,
                    reason
                )),
            )
        }
        CommandBlockContent::Unavailable(reason) => (content_unavailable_label(reason), None),
    };
    let mut output = div()
        .id("terminal-command-block-output")
        .w_full()
        .max_h(px(260.))
        .overflow_y_scroll()
        .p_2()
        .rounded_sm()
        .bg(theme.background)
        .text_size(px(11.))
        .text_color(theme.foreground)
        .child(text);
    if let Some(notice) = notice {
        output = output.child(
            div()
                .w_full()
                .text_size(theme.text_xs)
                .text_color(theme.muted)
                .child(notice),
        );
    }
    output.into_any_element()
}

/// 映射缺失、敏感或被驱逐的转录原因；不以空字符串冒充成功查询。
fn content_unavailable_label(reason: &CommandBlockContentUnavailable) -> String {
    match reason {
        CommandBlockContentUnavailable::NotCaptured => {
            crate::locale::text("输出未采集", "Output was not captured").to_string()
        }
        CommandBlockContentUnavailable::SensitiveOrUnsafe => crate::locale::text(
            "输出因安全规则不可用",
            "Output is unavailable due to safety rules",
        )
        .to_string(),
        CommandBlockContentUnavailable::WriteNotAcknowledged => crate::locale::text(
            "命令输入未确认，输出未关联",
            "Command input was not acknowledged; output is not associated",
        )
        .to_string(),
        CommandBlockContentUnavailable::Evicted => crate::locale::text(
            "输出已从有限缓冲中移除",
            "Output was evicted from the bounded buffer",
        )
        .to_string(),
        CommandBlockContentUnavailable::CaptureUnavailable => {
            crate::locale::text("输出转录当前不可用", "Output transcript is unavailable")
                .to_string()
        }
        CommandBlockContentUnavailable::Incomplete(reason) => crate::locale::text(
            format!("输出不完整 · {reason}"),
            format!("Output is incomplete · {reason}"),
        )
        .to_string(),
        CommandBlockContentUnavailable::QueryFailed(message) => message.clone(),
    }
}

/// 规范化不可信 byte range，保证分段永远不切开 UTF-8 字符且重叠命中只绘制一次。
pub fn normalized_text_segments(text: &str, ranges: &[TextHighlightRange]) -> Vec<TextSegment> {
    if text.is_empty() {
        return Vec::new();
    }

    let mut normalized = ranges
        .iter()
        .filter_map(|range| {
            if range.start_byte >= range.end_byte || range.start_byte >= text.len() {
                return None;
            }
            let start = previous_char_boundary(text, range.start_byte.min(text.len()));
            let end = next_char_boundary(text, range.end_byte.min(text.len()));
            (start < end).then_some(start..end)
        })
        .collect::<Vec<_>>();
    normalized.sort_by_key(|range| (range.start, range.end));

    let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
    for range in normalized {
        if let Some(previous) = merged.last_mut()
            && range.start <= previous.end
        {
            previous.end = previous.end.max(range.end);
            continue;
        }
        merged.push(range);
    }

    let mut segments = Vec::with_capacity(merged.len().saturating_mul(2).saturating_add(1));
    let mut cursor = 0;
    for range in merged {
        if cursor < range.start {
            segments.push(TextSegment {
                text: text[cursor..range.start].to_owned(),
                highlighted: false,
            });
        }
        segments.push(TextSegment {
            text: text[range.clone()].to_owned(),
            highlighted: true,
        });
        cursor = range.end;
    }
    if cursor < text.len() {
        segments.push(TextSegment {
            text: text[cursor..].to_owned(),
            highlighted: false,
        });
    }
    segments
}

/// UTF-8 安全裁剪展示文本，不改变真实输出或复制 payload。
fn truncate_at_boundary(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let end = previous_char_boundary(text, max_bytes);
    format!("{}…", &text[..end])
}

/// 向前定位字符边界，用于安全裁剪未对齐 UTF-8 偏移。
fn previous_char_boundary(text: &str, mut offset: usize) -> usize {
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// 向后定位字符边界，保证命中范围不会产生非法字符串切片。
fn next_char_boundary(text: &str, mut offset: usize) -> usize {
    while offset < text.len() && !text.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

/// 生成 DOM 稳定键，避免依赖 Debug 格式作为 UI 身份。
fn block_dom_id(block_id: CommandBlockId) -> String {
    format!(
        "{}-{}-{}",
        block_id.session_generation, block_id.prompt_epoch, block_id.ordinal
    )
}

#[cfg(test)]
#[path = "../../../tests/unit/terminal_command_blocks.rs"]
mod tests;
