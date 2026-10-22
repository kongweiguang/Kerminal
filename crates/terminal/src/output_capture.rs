//! OSC 133 命令边界内的有界文本转录；不试图重建终端网格或原始 stdout。
//!
//! @author kongweiguang

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use vte::{Parser, Perform};

use crate::shell_integration::ShellIntegrationMarker;

/// 保留命令块的数量上限；被移出的 ID 仍会以 `Evicted` 明确反馈。
///
/// @author kongweiguang
pub const TRANSCRIPT_BLOCK_LIMIT: usize = 128;

/// 单块保留的净化文本字节上限，避免一条命令长期占用过多内存。
///
/// @author kongweiguang
pub const TRANSCRIPT_BLOCK_BYTES_LIMIT: usize = 256 * 1024;

/// 同一引擎内所有已保留转录的净化文本总预算。
///
/// @author kongweiguang
pub const TRANSCRIPT_SESSION_BYTES_LIMIT: usize = 2 * 1024 * 1024;

/// 单块 parser 接受的原始 PTY 输入上限；额外余量容纳 OSC 133 结束标记。
///
/// @author kongweiguang
const TRANSCRIPT_INPUT_BYTES_LIMIT: usize = TRANSCRIPT_BLOCK_BYTES_LIMIT + 512;

/// 当前 TerminalEngine 生命周期内不透明的转录句柄。
///
/// 应用层应将它与 session generation 和自己的 CommandBlockId 关联，不能单独持久化或跨重连代次复用。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TranscriptCaptureId(u64);

impl TranscriptCaptureId {
    /// 暴露只用于同一终端生命周期内关联事件的序号，不将它定义为跨 session 的业务 ID。
    ///
    /// @author kongweiguang
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// 转录采集结束但没有完整观察到 OSC 133 D 的原因。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TranscriptIncompleteReason {
    Superseded,
    Closed,
    Disconnected,
    Reconnected,
}

/// 转录当前的运行态；文本始终只是去控制序列后的字节流投影。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalTranscriptState {
    Capturing,
    Complete { exit_code: Option<i32> },
    Incomplete(TranscriptIncompleteReason),
}

/// 查询结果因为当前引擎不再持有对应 ID 而不可用。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TranscriptUnavailableReason {
    Evicted,
    UnknownCapture,
}

impl fmt::Display for TranscriptUnavailableReason {
    /// 返回稳定且不含终端内容的查询失败说明。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Evicted => formatter.write_str("终端转录已超出保留预算"),
            Self::UnknownCapture => formatter.write_str("终端转录 ID 不属于当前引擎"),
        }
    }
}

impl std::error::Error for TranscriptUnavailableReason {}

/// 一次有界的转录快照；`truncated` 同时反映采集截断或本次读取预算截断。
///
/// `captured_bytes` 是引擎仍保留的净化 UTF-8 文本长度，`returned_bytes` 是本次返回长度。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalTranscriptSnapshot {
    pub capture_id: TranscriptCaptureId,
    pub revision: u64,
    pub state: TerminalTranscriptState,
    pub text: Arc<str>,
    pub captured_bytes: usize,
    pub returned_bytes: usize,
    pub truncated: bool,
}

/// 同一 PTY read 中按 wire 顺序报告的 shell 边界和转录变更。
///
/// C 与其匹配的 D 携带同一个 capture ID；旧 `shell_integration` 字段只是 marker 兼容投影。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalOutputEvent {
    ShellMarker {
        marker: ShellIntegrationMarker,
        capture_id: Option<TranscriptCaptureId>,
    },
    TranscriptChanged {
        capture_id: TranscriptCaptureId,
        revision: u64,
    },
    TranscriptEvicted {
        capture_id: TranscriptCaptureId,
    },
}

/// 已缓存的一条转录；仅一个活动记录额外持有流式 VTE parser。
struct TranscriptRecord {
    id: TranscriptCaptureId,
    revision: u64,
    state: TerminalTranscriptState,
    text: String,
    input_bytes: usize,
    truncated: bool,
}

/// 单个活动 capture 的增量 UTF-8/ANSI parser 状态。
struct ActiveTranscript {
    id: TranscriptCaptureId,
    parser: Parser,
    pending_carriage_return: bool,
}

/// 有界的命令输出文本缓存；保留前缀并明确截断，保证复制结果稳定可重复。
pub(crate) struct OutputCapture {
    records: VecDeque<TranscriptRecord>,
    active: Option<ActiveTranscript>,
    next_id: u64,
    retained_text_bytes: usize,
}

impl Default for OutputCapture {
    /// 以空记录表开始，所有容量由固定预算约束而非 PTY 输出规模决定。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            records: VecDeque::new(),
            active: None,
            next_id: 1,
            retained_text_bytes: 0,
        }
    }
}

impl OutputCapture {
    /// 在真实 C 标记处开启新 capture；若旧命令仍未收到 D，则保留其部分文本并标记 superseded。
    ///
    /// @author kongweiguang
    pub(crate) fn start(&mut self) -> (TranscriptCaptureId, Vec<TerminalOutputEvent>) {
        let mut events = Vec::new();
        events.extend(self.invalidate_active(TranscriptIncompleteReason::Superseded));

        while self.records.len() >= TRANSCRIPT_BLOCK_LIMIT {
            let Some(id) = self.evict_oldest_completed() else {
                break;
            };
            events.push(TerminalOutputEvent::TranscriptEvicted { capture_id: id });
        }

        let id = TranscriptCaptureId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.records.push_back(TranscriptRecord {
            id,
            revision: 0,
            state: TerminalTranscriptState::Capturing,
            text: String::new(),
            input_bytes: 0,
            truncated: false,
        });
        self.active = Some(ActiveTranscript {
            id,
            parser: Parser::new(),
            pending_carriage_return: false,
        });
        (id, events)
    }

    /// 返回当前尚未收到 D 的 capture ID，供边界 marker 事件关联同一命令块。
    ///
    /// @author kongweiguang
    pub(crate) fn active_id(&self) -> Option<TranscriptCaptureId> {
        self.active.as_ref().map(|active| active.id)
    }

    /// 仅将活动 C..D 片段送入 VTE tokenizer，并在 raw/text/session 预算内追加可打印文本。
    ///
    /// @author kongweiguang
    pub(crate) fn advance(&mut self, bytes: &[u8]) -> Vec<TerminalOutputEvent> {
        let Some(active_id) = self.active.as_ref().map(|active| active.id) else {
            return Vec::new();
        };
        let Some((input_remaining, text_remaining)) = self
            .records
            .iter_mut()
            .find(|record| record.id == active_id)
            .map(|record| {
                let input_remaining =
                    TRANSCRIPT_INPUT_BYTES_LIMIT.saturating_sub(record.input_bytes);
                let text_remaining = TRANSCRIPT_BLOCK_BYTES_LIMIT.saturating_sub(record.text.len());
                (input_remaining, text_remaining)
            })
        else {
            self.active = None;
            return Vec::new();
        };

        let input_length = bytes.len().min(input_remaining);
        let accepted = &bytes[..input_length];
        if let Some(record) = self
            .records
            .iter_mut()
            .find(|record| record.id == active_id)
        {
            record.input_bytes = record.input_bytes.saturating_add(input_length);
        }

        let active = match self.active.as_mut() {
            Some(active) => active,
            None => return Vec::new(),
        };
        let mut collector = PlainTextCollector::new(text_remaining, active.pending_carriage_return);
        active.parser.advance(&mut collector, accepted);
        active.pending_carriage_return = collector.pending_carriage_return;

        let parsed_text = collector.text;
        let collector_truncated = collector.truncated;
        let mut events = self.append_text(active_id, &parsed_text);
        if (input_length < bytes.len() || collector_truncated)
            && let Some(event) = self.mark_truncated(active_id)
        {
            events.push(event);
        }
        events
    }

    /// 在观察到完整 D 后封口当前转录；退出码仍由 shell integration 原样提供。
    ///
    /// @author kongweiguang
    pub(crate) fn finish(&mut self, exit_code: Option<i32>) -> Vec<TerminalOutputEvent> {
        let Some(active) = self.active.take() else {
            return Vec::new();
        };
        let mut events =
            self.flush_pending_carriage_return(active.id, active.pending_carriage_return);
        if let Some(record) = self
            .records
            .iter_mut()
            .find(|record| record.id == active.id)
        {
            record.state = TerminalTranscriptState::Complete { exit_code };
            record.revision = record.revision.wrapping_add(1);
            events.push(TerminalOutputEvent::TranscriptChanged {
                capture_id: active.id,
                revision: record.revision,
            });
        }
        events
    }

    /// EOF、关闭或重连时将未完成命令转成明确不完整状态，不把部分文本伪装成成功输出。
    ///
    /// @author kongweiguang
    pub(crate) fn invalidate_active(
        &mut self,
        reason: TranscriptIncompleteReason,
    ) -> Vec<TerminalOutputEvent> {
        let Some(active) = self.active.take() else {
            return Vec::new();
        };
        let mut events =
            self.flush_pending_carriage_return(active.id, active.pending_carriage_return);
        let Some(record) = self
            .records
            .iter_mut()
            .find(|record| record.id == active.id)
        else {
            return events;
        };
        record.state = TerminalTranscriptState::Incomplete(reason);
        record.revision = record.revision.wrapping_add(1);
        events.push(TerminalOutputEvent::TranscriptChanged {
            capture_id: active.id,
            revision: record.revision,
        });
        events
    }

    /// 读取一条转录的 UTF-8 安全前缀；超出保留期与未知 ID 使用不同错误，不返回含糊空文本。
    ///
    /// @author kongweiguang
    pub(crate) fn snapshot(
        &self,
        id: TranscriptCaptureId,
        max_bytes: usize,
    ) -> Result<TerminalTranscriptSnapshot, TranscriptUnavailableReason> {
        let record = self.records.iter().find(|record| record.id == id).ok_or(
            if id.0 != 0 && id.0 < self.next_id {
                TranscriptUnavailableReason::Evicted
            } else {
                TranscriptUnavailableReason::UnknownCapture
            },
        )?;
        let mut returned_bytes = record.text.len().min(max_bytes);
        while !record.text.is_char_boundary(returned_bytes) {
            returned_bytes = returned_bytes.saturating_sub(1);
        }
        Ok(TerminalTranscriptSnapshot {
            capture_id: id,
            revision: record.revision,
            state: record.state,
            text: Arc::from(&record.text[..returned_bytes]),
            captured_bytes: record.text.len(),
            returned_bytes,
            truncated: record.truncated || returned_bytes < record.text.len(),
        })
    }

    /// 把回车、换行与制表符投影为可复制文本，并按字符边界执行块/会话容量裁剪。
    ///
    /// @author kongweiguang
    fn append_text(&mut self, id: TranscriptCaptureId, text: &str) -> Vec<TerminalOutputEvent> {
        if text.is_empty() {
            return Vec::new();
        }
        let mut events = Vec::new();
        while self.retained_text_bytes.saturating_add(text.len()) > TRANSCRIPT_SESSION_BYTES_LIMIT {
            let Some(evicted) = self.evict_oldest_completed_except(id) else {
                break;
            };
            events.push(TerminalOutputEvent::TranscriptEvicted {
                capture_id: evicted,
            });
        }

        let session_remaining =
            TRANSCRIPT_SESSION_BYTES_LIMIT.saturating_sub(self.retained_text_bytes);
        let accepted_limit = text.len().min(session_remaining);
        let mut accepted_length = accepted_limit;
        while !text.is_char_boundary(accepted_length) {
            accepted_length = accepted_length.saturating_sub(1);
        }

        if let Some(record) = self.records.iter_mut().find(|record| record.id == id) {
            let block_remaining = TRANSCRIPT_BLOCK_BYTES_LIMIT.saturating_sub(record.text.len());
            let mut block_length = accepted_length.min(block_remaining);
            while !text.is_char_boundary(block_length) {
                block_length = block_length.saturating_sub(1);
            }
            if block_length > 0 {
                record.text.push_str(&text[..block_length]);
                self.retained_text_bytes = self.retained_text_bytes.saturating_add(block_length);
                record.revision = record.revision.wrapping_add(1);
                events.push(TerminalOutputEvent::TranscriptChanged {
                    capture_id: id,
                    revision: record.revision,
                });
            }
            if block_length < text.len() && !record.truncated {
                record.truncated = true;
                record.revision = record.revision.wrapping_add(1);
                events.push(TerminalOutputEvent::TranscriptChanged {
                    capture_id: id,
                    revision: record.revision,
                });
            }
        }
        events
    }

    /// 将 chunk 末尾未配对的 CR 变成一个换行，避免完成或中断快照丢失最后一行。
    ///
    /// @author kongweiguang
    fn flush_pending_carriage_return(
        &mut self,
        id: TranscriptCaptureId,
        pending: bool,
    ) -> Vec<TerminalOutputEvent> {
        if !pending {
            return Vec::new();
        }
        self.append_text(id, "\n")
    }

    /// 记录因为 raw 输入或文本预算触顶而发生的截断，避免把前缀误报为完整转录。
    ///
    /// @author kongweiguang
    fn mark_truncated(&mut self, id: TranscriptCaptureId) -> Option<TerminalOutputEvent> {
        let record = self.records.iter_mut().find(|record| record.id == id)?;
        if record.truncated {
            return None;
        }
        record.truncated = true;
        record.revision = record.revision.wrapping_add(1);
        Some(TerminalOutputEvent::TranscriptChanged {
            capture_id: id,
            revision: record.revision,
        })
    }

    /// 仅移出已完成/不完整的最旧记录；活动命令不会被预算整理动作移除。
    ///
    /// @author kongweiguang
    fn evict_oldest_completed(&mut self) -> Option<TranscriptCaptureId> {
        self.evict_oldest_completed_except(TranscriptCaptureId(0))
    }

    /// 为追加文本保留活动 ID，其余最旧完成记录可被总预算或块数预算驱逐。
    ///
    /// @author kongweiguang
    fn evict_oldest_completed_except(
        &mut self,
        protected_id: TranscriptCaptureId,
    ) -> Option<TranscriptCaptureId> {
        let index = self.records.iter().position(|record| {
            record.id != protected_id && !matches!(record.state, TerminalTranscriptState::Capturing)
        })?;
        let record = self.records.remove(index)?;
        self.retained_text_bytes = self.retained_text_bytes.saturating_sub(record.text.len());
        Some(record.id)
    }
}

/// 只收集 VTE parser 已判定为 printable 的字符，并保留 parser 状态跨 PTY chunk。
struct PlainTextCollector {
    text: String,
    remaining_bytes: usize,
    truncated: bool,
    pending_carriage_return: bool,
}

impl PlainTextCollector {
    /// 按活动记录剩余预算构造输出 sink，避免 tokenizer 产生无界文本临时值。
    ///
    /// @author kongweiguang
    fn new(remaining_bytes: usize, pending_carriage_return: bool) -> Self {
        Self {
            text: String::new(),
            remaining_bytes,
            truncated: false,
            pending_carriage_return,
        }
    }

    /// 追加完整 Unicode scalar；不拆分 UTF-8 码点，超限后仅记录截断状态。
    ///
    /// @author kongweiguang
    fn push_char(&mut self, character: char) {
        let bytes = character.len_utf8();
        if bytes > self.remaining_bytes {
            self.truncated = true;
            return;
        }
        self.text.push(character);
        self.remaining_bytes -= bytes;
    }

    /// 在后续可打印字符不是 LF 时将先前 CR 映射为换行，保留文本复制的行边界。
    ///
    /// @author kongweiguang
    fn flush_carriage_return(&mut self) {
        if self.pending_carriage_return {
            self.push_char('\n');
            self.pending_carriage_return = false;
        }
    }
}

impl Perform for PlainTextCollector {
    /// 保存 tokenizer 确认的普通 Unicode 文本，不模拟光标覆盖和屏幕擦除。
    ///
    /// @author kongweiguang
    fn print(&mut self, character: char) {
        if self.pending_carriage_return {
            if character == '\n' {
                self.push_char('\n');
                self.pending_carriage_return = false;
                return;
            }
            self.flush_carriage_return();
        }
        if character == '\r' {
            self.pending_carriage_return = true;
        } else {
            self.push_char(character);
        }
    }

    /// 仅保留文本需要的换行和制表符，其余控制功能不会伪装成可见输出。
    ///
    /// @author kongweiguang
    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => {
                self.pending_carriage_return = true;
            }
            b'\n' => {
                self.pending_carriage_return = false;
                self.push_char('\n');
            }
            b'\t' => {
                self.flush_carriage_return();
                self.push_char('\t');
            }
            _ => self.flush_carriage_return(),
        }
    }
}
