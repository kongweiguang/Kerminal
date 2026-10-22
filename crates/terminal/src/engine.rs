//! Alacritty VT state 与稳定快照之间的纯 Rust 适配层。
//!
//! @author kongweiguang

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::{Flags, LineLength};
use alacritty_terminal::term::{
    ClipboardType, Config, ResizeStrategy as TermResizeStrategy, Term, TermDamage, TermMode,
};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb};
use kerminal_domain::identity::SessionId;
use kerminal_domain::sessions::TerminalSize;
use regex::RegexBuilder;

use crate::cursor_style::CursorStyleTracker;
use crate::output_capture::{
    OutputCapture, TerminalOutputEvent, TerminalTranscriptSnapshot, TranscriptCaptureId,
    TranscriptIncompleteReason, TranscriptUnavailableReason,
};
use crate::shell_integration::{Osc133Scanner, ShellIntegrationMarker};
use crate::snapshot::{
    OutputRevision, PlainTextSnapshot, PlainTextSnapshotRequest, RenderSnapshot,
    RenderSnapshotRequest, SelectionViewport, TerminalAttributes, TerminalColor, TerminalCursor,
    TerminalCursorCell, TerminalCursorShape, TerminalHyperlink, TerminalModes, TerminalPalette,
    TerminalPoint, TerminalRgb, TerminalRow, TerminalRowWitness, TerminalRun,
    TerminalSearchLogicalLineWitness, TerminalSearchMatch, TerminalSearchProjection,
    TerminalSearchRequest, TerminalSearchResult, TerminalSearchRowWitness, TerminalSelection,
    TerminalSelectionKind, TerminalStyle, TerminalTextCellSpan, UnderlineStyle,
};

/// 根据真实 transport 选择终端扩行契约；默认行为继续兼容既有构造器。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalResizePolicy {
    /// 使用 Alacritty 的通用历史拉入行为。
    #[default]
    Generic,
    /// 仅用于 Windows 本地 ConPTY，扩行时保留历史与旧行坐标。
    ConPty,
}

/// 网格与 scrollback 的硬上限；输入来自 MCP 或远端时也不允许无限分配。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalLimits {
    pub max_columns: u16,
    pub max_rows: u16,
    pub max_cells: usize,
    pub max_scrollback_lines: usize,
}

impl Default for TerminalLimits {
    /// 采用明确的列、行和历史单元格预算，使异常 resize 不会触发巨型网格分配。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            max_columns: 1000,
            max_rows: 500,
            max_cells: 2_000_000,
            max_scrollback_lines: 10_000,
        }
    }
}

/// 终端网格超限、搜索正则无效或取消时的纯领域错误。
///
/// @author kongweiguang
#[derive(Debug)]
pub enum TerminalEngineError {
    Capacity {
        max_columns: u16,
        max_rows: u16,
        max_cells: usize,
    },
    InvalidSearch(regex::Error),
    SearchCancelled,
}

impl std::fmt::Display for TerminalEngineError {
    /// 输出稳定且不包含终端缓冲内容的错误描述，供应用层转换为 AppError。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Capacity {
                max_columns,
                max_rows,
                max_cells,
            } => write!(
                formatter,
                "终端尺寸超过允许容量（最大 {max_columns} 列、{max_rows} 行、{max_cells} 个单元格）"
            ),
            Self::InvalidSearch(_) => formatter.write_str("终端搜索表达式无效"),
            Self::SearchCancelled => formatter.write_str("终端搜索已取消"),
        }
    }
}

impl std::error::Error for TerminalEngineError {
    /// 保留正则编译失败来源以便内部诊断，但公开消息保持稳定。
    ///
    /// @author kongweiguang
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidSearch(error) => Some(error),
            Self::Capacity { .. } | Self::SearchCancelled => None,
        }
    }
}

/// Parser 回调的最小线程安全事件队列；PTY query 回复不会在 UI 通知队列中丢失。
#[derive(Clone, Default)]
struct EventProxy(Arc<Mutex<Vec<Event>>>);

impl EventListener for EventProxy {
    /// 把同步 parser 回调暂存到短临界区，稍后由 runtime 顺序写回 transport。
    ///
    /// @author kongweiguang
    fn send_event(&self, event: Event) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(event);
    }
}

const SEARCH_PROJECTION_MAX_LOGICAL_LINES: usize = 512;
const SEARCH_PROJECTION_MAX_ROWS: usize = 1024;
const SEARCH_PROJECTION_MAX_TEXT_BYTES: usize = 512 * 1024;
const SEARCH_PROJECTION_MAX_CELL_SPANS: usize = 128 * 1024;
const SEARCH_PROJECTION_MAX_MATCHES: usize = 1024;
const SEARCH_PROJECTION_MAX_LINE_ROWS: usize = 64;
const SEARCH_PROJECTION_MAX_LINE_TEXT_BYTES: usize = 32 * 1024;
const SEARCH_PROJECTION_MAX_LINE_CELL_SPANS: usize = 8192;

/// 一条由多个 Alacritty 物理行组成的逻辑行及其搜索文本到 cell 的映射。
///
/// 行证据只在该逻辑行确有命中时从同一网格读取，避免未命中历史行产生第二份文本。
///
/// @author kongweiguang
#[derive(Default)]
struct SearchLogicalLine {
    text: String,
    byte_cells: Vec<(TerminalPoint, u16)>,
}

impl SearchLogicalLine {
    /// 同步追加 matcher 文本及其 UTF-8 字节到真实终端 cell 的映射。
    ///
    /// @author kongweiguang
    fn push_char(&mut self, character: char, point: TerminalPoint, width: u16) {
        let previous_len = self.text.len();
        self.text.push(character);
        self.byte_cells.resize(
            self.byte_cells.len() + self.text.len() - previous_len,
            (point, width),
        );
    }

    /// 将非空正则字节范围投影到首尾 cell，终点保持跨物理行的 end-exclusive 坐标。
    ///
    /// @author kongweiguang
    fn project_range(&self, range: Range<usize>, columns: u16) -> Option<Range<TerminalPoint>> {
        let start = self.byte_cells.get(range.start)?.0;
        let (last, width) = *self.byte_cells.get(range.end.checked_sub(1)?)?;
        Some(
            start..TerminalPoint {
                line: last.line,
                column: last.column.saturating_add(width).min(columns),
            },
        )
    }
}

/// 聚合所有已提交逻辑行的硬预算；完整搜索仍受请求的 max_matches 控制。
///
/// @author kongweiguang
#[derive(Default)]
struct SearchProjectionBudget {
    rows: usize,
    text_bytes: usize,
    cell_spans: usize,
    matches: usize,
}

impl SearchProjectionBudget {
    /// 预算耗尽后停止重建后续命中行证据，但继续完成搜索结果本身。
    ///
    /// @author kongweiguang
    fn has_capacity(&self) -> bool {
        self.rows < SEARCH_PROJECTION_MAX_ROWS
            && self.text_bytes < SEARCH_PROJECTION_MAX_TEXT_BYTES
            && self.cell_spans < SEARCH_PROJECTION_MAX_CELL_SPANS
            && self.matches < SEARCH_PROJECTION_MAX_MATCHES
    }
}

/// 仅保留命中逻辑行的共享证据，并同时限制行数、文本、span 和命中引用总量。
///
/// @author kongweiguang
fn append_search_projection_line(
    rows: Vec<TerminalSearchRowWitness>,
    first_match_index: usize,
    matches: &[TerminalSearchMatch],
    budget: &mut SearchProjectionBudget,
) -> Option<TerminalSearchLogicalLineWitness> {
    let hit_count = matches.len().saturating_sub(first_match_index);
    let (last, preceding) = rows.split_last()?;
    let text_bytes = rows.iter().fold(0usize, |total, row| {
        total.saturating_add(row.witness.text.len())
    });
    let cell_spans = rows.iter().fold(0usize, |total, row| {
        total.saturating_add(row.witness.cell_spans.len())
    });
    if hit_count == 0
        || last.witness.wrapped
        || preceding.iter().any(|row| !row.witness.wrapped)
        || rows.len() > SEARCH_PROJECTION_MAX_LINE_ROWS
        || text_bytes > SEARCH_PROJECTION_MAX_LINE_TEXT_BYTES
        || cell_spans > SEARCH_PROJECTION_MAX_LINE_CELL_SPANS
        || budget.rows.saturating_add(rows.len()) > SEARCH_PROJECTION_MAX_ROWS
        || budget.text_bytes.saturating_add(text_bytes) > SEARCH_PROJECTION_MAX_TEXT_BYTES
        || budget.cell_spans.saturating_add(cell_spans) > SEARCH_PROJECTION_MAX_CELL_SPANS
        || budget.matches >= SEARCH_PROJECTION_MAX_MATCHES
    {
        return None;
    }

    let retained_matches = hit_count.min(SEARCH_PROJECTION_MAX_MATCHES - budget.matches);
    let line_matches = matches[first_match_index..first_match_index + retained_matches]
        .iter()
        .cloned()
        .enumerate()
        .map(|(offset, matched)| (first_match_index + offset, matched))
        .collect::<Vec<_>>();
    budget.rows += rows.len();
    budget.text_bytes += text_bytes;
    budget.cell_spans += cell_spans;
    budget.matches += retained_matches;
    Some(TerminalSearchLogicalLineWitness {
        rows: rows.into(),
        matches: line_matches.into(),
    })
}

/// 把一个完整逻辑行的命中映射为终端坐标，并只在确有额外命中时标记截断。
///
/// @author kongweiguang
fn collect_logical_line_matches(
    logical_line: &SearchLogicalLine,
    regex: &regex::Regex,
    max_matches: usize,
    columns: u16,
    matches: &mut Vec<TerminalSearchMatch>,
    truncated: &mut bool,
) {
    for found in regex.find_iter(&logical_line.text) {
        if found.start() == found.end() {
            continue;
        }
        if matches.len() == max_matches {
            *truncated = true;
            break;
        }
        if let Some(range) = logical_line.project_range(found.start()..found.end(), columns) {
            matches.push(TerminalSearchMatch { range });
        }
    }
}

/// Alacritty 需要的轻量尺寸实现；像素尺寸归 renderer/PTy，不进入仿真网格。
#[derive(Clone, Copy)]
struct GridDimensions {
    columns: usize,
    rows: usize,
}

impl Dimensions for GridDimensions {
    /// 把可见屏幕作为仿真器的 total lines，历史由 Alacritty scrollback 单独管理。
    ///
    /// @author kongweiguang
    fn total_lines(&self) -> usize {
        self.rows
    }

    /// 返回当前可见行数，避免尺寸由外部可变 renderer 状态决定。
    ///
    /// @author kongweiguang
    fn screen_lines(&self) -> usize {
        self.rows
    }

    /// 返回当前列数，保证 Parser 写入与 PTY resize 使用同一个网格维度。
    ///
    /// @author kongweiguang
    fn columns(&self) -> usize {
        self.columns
    }
}

/// Alacritty terminal state 与其持久 VT parser；不持有 GPUI、PTY 或网络对象。
///
/// @author kongweiguang
pub struct TerminalEngine {
    session_id: SessionId,
    term: Term<EventProxy>,
    parser: Processor,
    event_proxy: EventProxy,
    limits: TerminalLimits,
    size: TerminalSize,
    scrollback_limit: usize,
    resize_policy: TerminalResizePolicy,
    revision: u64,
    /// 最近一次可能改变 absolute grid 行坐标的全局 revision；viewport 与 selection 变更不推进此水位。
    last_grid_revision: OutputRevision,
    /// 当前坐标空间代次；与 Term 的代次分离，以便同 session 重连后仍能拒绝旧快照。
    grid_epoch: u64,
    /// 上次同步的 Alacritty 坐标空间代次，用于合并一次 VT chunk 内的多次拓扑变化。
    observed_term_grid_epoch: u64,
    damage_history: VecDeque<(u64, Vec<u16>)>,
    /// 缓存当前 viewport 各行已合并的 runs；快照仍返回完整行表，但干净行复用 Arc。
    render_row_cache: Vec<Option<CachedRenderRow>>,
    /// 备用屏与主屏可能复用相同行号，切换时必须避免命中另一屏的行缓存。
    alternate_screen: bool,
    palette: TerminalPalette,
    title: Option<Arc<str>>,
    current_working_directory: Option<Arc<str>>,
    selection: Option<TerminalSelection>,
    /// 记录本引擎见过的最大 gesture id；手势 id 由 UI 全局递增，禁止旧事件倒退重建 anchor。
    selection_gesture_id: Option<u64>,
    /// resize 或普通选择操作会结束当前 id，但保留它作为迟到 UI 事件的拒绝水位。
    selection_gesture_ended: bool,
    /// 记录该拖选曾否形成有效范围，以区分尚未移动的零长度选择与已被历史裁掉的选择。
    selection_gesture_had_range: bool,
    /// 记录 MouseDown 空锚点的坐标水位，避免 Alacritty 在历史满后把已裁掉的锚点夹到新行。
    selection_gesture_empty_anchor: Option<EmptySelectionGestureAnchor>,
    shell_integration_scanner: Osc133Scanner,
    output_capture: OutputCapture,
    cursor_style_tracker: CursorStyleTracker,
}

/// 空手势锚点在建立时的行坐标水位；全屏滚动可映射，拓扑变化与历史裁剪则拒绝续接。
///
/// @author kongweiguang
#[derive(Clone, Copy)]
struct EmptySelectionGestureAnchor {
    line: i64,
    scroll_origin: u64,
    grid_epoch: u64,
}

/// Parser 输入后需要由应用 runtime 写回 PTY 的响应与 UI 可观察事件。
///
/// @author kongweiguang
#[derive(Default)]
pub struct TerminalOutput {
    pub pty_writes: Vec<Vec<u8>>,
    pub title_changed: Option<Option<Arc<str>>>,
    pub bell_count: usize,
    pub close_requested: bool,
    /// 需宿主策略层处理后再回复 parser 的操作，不在引擎内访问剪贴板或窗口系统。
    pub host_events: Vec<TerminalHostEvent>,
    /// 仅包含本次输入字节中真实解析到的 OSC 133 标记。
    pub shell_integration: Vec<ShellIntegrationMarker>,
    /// Shell marker 与文本转录变化按输入字节顺序排列；adapter 应只消费此有序事件流。
    pub events: Vec<TerminalOutputEvent>,
}

/// VT parser 请求的宿主操作；保留 formatter 让 adapter 生成准确 OSC/CSI 响应。
///
/// @author kongweiguang
pub enum TerminalHostEvent {
    ClipboardStore {
        kind: ClipboardType,
        text: String,
    },
    ClipboardLoad {
        kind: ClipboardType,
        format: Arc<dyn Fn(&str) -> String + Send + Sync>,
    },
    ColorRequest {
        index: usize,
        format: Arc<dyn Fn(Rgb) -> String + Send + Sync>,
    },
    TextAreaSizeRequest {
        format: Arc<dyn Fn(WindowSize) -> String + Send + Sync>,
    },
}

/// 可变行草稿把 cells 合并成 runs 后仅在整行完成时分配 Arc 快照。
struct RowBuilder {
    absolute_line: i64,
    wrapped: bool,
    runs: Vec<RunBuilder>,
}

/// 保留单行网格映射和已冻结 runs，使快照创建跳过未变化行的字符串与 span 分配。
#[derive(Clone)]
struct CachedRenderRow {
    absolute_line: i64,
    wrapped: bool,
    runs: Arc<[TerminalRun]>,
}

/// 一行内同 style 与 link 的连续可绘制 cell 段。
struct RunBuilder {
    start_column: u16,
    end_column: u16,
    text: String,
    cell_spans: Vec<crate::snapshot::TerminalTextCellSpan>,
    style: TerminalStyle,
    hyperlink: Option<TerminalHyperlink>,
}

impl TerminalEngine {
    /// 在启动 PTY 前校验列行和 scrollback 预算，防止不可信 resize 先触发系统资源分配。
    ///
    /// @author kongweiguang
    pub fn validate_size(
        size: TerminalSize,
        limits: TerminalLimits,
    ) -> Result<(), TerminalEngineError> {
        checked_scrollback(size, limits).map(|_| ())
    }

    /// 校验容量后创建 generic 网格，维持所有既有调用的默认 row-growth 语义。
    ///
    /// @author kongweiguang
    pub fn new(
        session_id: SessionId,
        size: TerminalSize,
        limits: TerminalLimits,
    ) -> Result<Self, TerminalEngineError> {
        Self::new_with_resize_policy(session_id, size, limits, TerminalResizePolicy::Generic)
    }

    /// 按已验证的 transport 能力创建网格，不在纯终端层猜测 OS 或目标类型。
    ///
    /// @author kongweiguang
    pub fn new_with_resize_policy(
        session_id: SessionId,
        size: TerminalSize,
        limits: TerminalLimits,
        resize_policy: TerminalResizePolicy,
    ) -> Result<Self, TerminalEngineError> {
        let scrollback = checked_scrollback(size, limits)?;
        let event_proxy = EventProxy::default();
        let dimensions = GridDimensions {
            columns: usize::from(size.columns()),
            rows: usize::from(size.rows()),
        };
        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        let mut term = Term::new(config, &dimensions, event_proxy.clone());
        term.reset_damage();
        let observed_term_grid_epoch = term.grid_epoch();
        Ok(Self {
            session_id,
            term,
            parser: Processor::new(),
            event_proxy,
            limits,
            size,
            scrollback_limit: scrollback,
            resize_policy,
            revision: 0,
            last_grid_revision: 0,
            grid_epoch: 0,
            observed_term_grid_epoch,
            damage_history: VecDeque::from([(0, (0..size.rows()).collect())]),
            render_row_cache: (0..size.rows()).map(|_| None).collect(),
            alternate_screen: false,
            palette: TerminalPalette::default(),
            title: None,
            current_working_directory: None,
            selection: None,
            selection_gesture_id: None,
            selection_gesture_ended: false,
            selection_gesture_had_range: false,
            selection_gesture_empty_anchor: None,
            shell_integration_scanner: Osc133Scanner::default(),
            output_capture: OutputCapture::default(),
            cursor_style_tracker: CursorStyleTracker::default(),
        })
    }

    /// 同 PTY 的 OSC7 只更新目录元数据，不能认证发送者是 shell 或扩大文件访问的 host 权限。
    /// 任意非空 VT chunk 都可能移动行坐标，因此推进 grid watermark；零字节观察不使坐标失效。
    /// 不按 chunk 末态过滤备用屏，避免真实 prompt 与屏幕切换同块时丢目录；OSC133 仍保留精确 offset。
    ///
    /// @author kongweiguang
    pub fn process_output(&mut self, bytes: &[u8]) -> TerminalOutput {
        let scanned_markers = self.shell_integration_scanner.advance_with_offsets(bytes);
        self.cursor_style_tracker.advance(bytes);
        self.parser.advance(&mut self.term, bytes);
        self.sync_term_grid_epoch();
        let alternate_screen = self
            .term
            .renderable_content()
            .mode
            .contains(TermMode::ALT_SCREEN);
        if alternate_screen != self.alternate_screen {
            self.invalidate_all_render_rows();
            self.alternate_screen = alternate_screen;
        }
        if let Some(directory) = self
            .shell_integration_scanner
            .take_current_working_directory()
        {
            self.current_working_directory = Some(directory.into());
        }
        self.revision = self.revision.wrapping_add(1);
        if !bytes.is_empty() {
            // 任意非空 VT 输入都可能换行、滚屏或切换网格，保守地使旧行坐标失效。
            self.last_grid_revision = self.revision;
        }
        self.collect_damage();
        let mut output = TerminalOutput::default();
        self.capture_output_segments(bytes, &scanned_markers, &mut output);
        let events = std::mem::take(
            &mut *self
                .event_proxy
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for event in events {
            match event {
                Event::PtyWrite(text) => output.pty_writes.push(text.into_bytes()),
                Event::Title(title) => {
                    let title: Arc<str> = title.into();
                    self.title = Some(title.clone());
                    output.title_changed = Some(Some(title));
                }
                Event::ResetTitle => {
                    self.title = None;
                    output.title_changed = Some(None);
                }
                Event::Bell => output.bell_count += 1,
                Event::Exit => output.close_requested = true,
                Event::ChildExit(_)
                | Event::Wakeup
                | Event::MouseCursorDirty
                | Event::CursorBlinkingChange => {}
                Event::ClipboardStore(kind, text) => output
                    .host_events
                    .push(TerminalHostEvent::ClipboardStore { kind, text }),
                Event::ClipboardLoad(kind, format) => {
                    output.pty_writes.push(format("").into_bytes());
                    output
                        .host_events
                        .push(TerminalHostEvent::ClipboardLoad { kind, format });
                }
                Event::ColorRequest(index, format) => {
                    let color = self.effective_palette().named_index(index);
                    if let Some(color) = color {
                        output.pty_writes.push(
                            format(Rgb {
                                r: color.red,
                                g: color.green,
                                b: color.blue,
                            })
                            .into_bytes(),
                        );
                    }
                    output
                        .host_events
                        .push(TerminalHostEvent::ColorRequest { index, format });
                }
                Event::TextAreaSizeRequest(format) => {
                    output.pty_writes.push(
                        format(WindowSize {
                            num_lines: self.size.rows(),
                            num_cols: self.size.columns(),
                            cell_width: 0,
                            cell_height: 0,
                        })
                        .into_bytes(),
                    );
                    output
                        .host_events
                        .push(TerminalHostEvent::TextAreaSizeRequest { format });
                }
            }
        }
        output
    }

    /// 让 transcript capture 与 OSC marker 共用同一 wire 顺序，避免同块 C/D 的 block-ID 错配。
    ///
    /// @author kongweiguang
    fn capture_output_segments(
        &mut self,
        bytes: &[u8],
        markers: &[crate::shell_integration::ScannedShellMarker],
        output: &mut TerminalOutput,
    ) {
        let mut offset = 0usize;
        for scanned in markers {
            let end_offset = scanned.end_offset.min(bytes.len()).max(offset);
            if end_offset > offset {
                output
                    .events
                    .extend(self.output_capture.advance(&bytes[offset..end_offset]));
            }

            let marker = scanned.marker;
            output.shell_integration.push(marker);
            let capture_id = match marker {
                ShellIntegrationMarker::CommandStart => {
                    let (id, events) = self.output_capture.start();
                    output.events.extend(events);
                    Some(id)
                }
                ShellIntegrationMarker::CommandFinished { exit_code } => {
                    let id = self.output_capture.active_id();
                    output.events.extend(self.output_capture.finish(exit_code));
                    id
                }
                ShellIntegrationMarker::PromptStart | ShellIntegrationMarker::PromptEnd => None,
            };
            output
                .events
                .push(TerminalOutputEvent::ShellMarker { marker, capture_id });
            offset = end_offset;
        }

        if offset < bytes.len() {
            output
                .events
                .extend(self.output_capture.advance(&bytes[offset..]));
        }
    }

    /// 查询当前 engine 保留的 command transcript；字节上限在 UTF-8 边界处裁剪。
    ///
    /// @author kongweiguang
    pub fn transcript_snapshot(
        &self,
        capture_id: TranscriptCaptureId,
        max_bytes: usize,
    ) -> Result<TerminalTranscriptSnapshot, TranscriptUnavailableReason> {
        self.output_capture.snapshot(capture_id, max_bytes)
    }

    /// session EOF、显式 close 或 disconnect 时终止活动转录，不伪造 OSC 133 D。
    ///
    /// @author kongweiguang
    pub fn invalidate_active_transcript(
        &mut self,
        reason: TranscriptIncompleteReason,
    ) -> Vec<TerminalOutputEvent> {
        self.output_capture.invalidate_active(reason)
    }

    /// 校验 cell budget 后按 transport 契约 resize；结束拖选并推进 grid watermark 以拒绝旧快照坐标。
    ///
    /// @author kongweiguang
    pub fn resize(&mut self, size: TerminalSize) -> Result<(), TerminalEngineError> {
        let maximum_scrollback = checked_scrollback(size, self.limits)?;
        let scrollback = self.scrollback_limit.min(maximum_scrollback);
        let dimensions = GridDimensions {
            columns: usize::from(size.columns()),
            rows: usize::from(size.rows()),
        };
        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        self.term.set_options(config);
        let strategy = match self.resize_policy {
            TerminalResizePolicy::Generic => TermResizeStrategy::Generic,
            TerminalResizePolicy::ConPty => TermResizeStrategy::ConPty,
        };
        self.term.resize_with_strategy(dimensions, strategy);
        self.sync_term_grid_epoch();
        self.selection_gesture_ended = self.selection_gesture_id.is_some();
        self.selection_gesture_had_range = false;
        self.selection_gesture_empty_anchor = None;
        self.size = size;
        self.scrollback_limit = scrollback;
        self.render_row_cache = (0..size.rows()).map(|_| None).collect();
        self.revision = self.revision.wrapping_add(1);
        self.last_grid_revision = self.revision;
        self.record_damage((0..size.rows()).collect());
        self.term.reset_damage();
        Ok(())
    }

    /// 在同一 session 原位重连时重置 VT 状态并推进坐标水位，既有 view/watch 可继续观察新代输出。
    ///
    /// @author kongweiguang
    pub fn reset_for_reconnect(&mut self) -> Result<(), TerminalEngineError> {
        let next_revision = self.revision.wrapping_add(1);
        let next_grid_epoch = self
            .grid_epoch
            .checked_add(1)
            .expect("terminal grid epoch exhausted");
        let palette = self.palette;
        let mut output_capture = std::mem::take(&mut self.output_capture);
        output_capture.invalidate_active(TranscriptIncompleteReason::Reconnected);
        let mut fresh = Self::new_with_resize_policy(
            self.session_id.clone(),
            self.size,
            self.limits,
            self.resize_policy,
        )?;
        fresh.set_scrollback_limit(self.scrollback_limit);
        fresh.palette = palette;
        fresh.output_capture = output_capture;
        fresh.revision = next_revision;
        fresh.last_grid_revision = next_revision;
        fresh.grid_epoch = next_grid_epoch;
        fresh.observed_term_grid_epoch = fresh.term.grid_epoch();
        fresh.damage_history = VecDeque::from([(next_revision, (0..self.size.rows()).collect())]);
        *self = fresh;
        Ok(())
    }

    /// 更新 Alacritty 的 viewport offset；行槽映射会变，因此清除缓存但保留 PTY 会话。
    ///
    /// @author kongweiguang
    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
        self.invalidate_all_render_rows();
        self.revision = self.revision.wrapping_add(1);
        self.record_damage((0..self.size.rows()).collect());
        self.collect_damage();
    }

    /// 按绝对 scrollback offset 跳转，并把写 viewport 的权限留给显式滚动命令。
    ///
    /// offset 按 Alacritty 当前活动 grid 的 history 上限裁剪；相同位置不制造多余 revision，
    /// 这样只读快照不会在 UI 刷新时覆盖滚轮或跳转结果。
    ///
    /// @author kongweiguang
    pub fn scroll_to_offset(&mut self, offset: usize) {
        let current_offset = self.term.grid().display_offset();
        let target_offset = offset.min(self.term.grid().history_size());
        if target_offset == current_offset {
            return;
        }

        let distance = target_offset
            .abs_diff(current_offset)
            .min(i32::MAX as usize) as i32;
        self.term
            .scroll_display(Scroll::Delta(if target_offset > current_offset {
                distance
            } else {
                -distance
            }));
        self.invalidate_all_render_rows();
        self.revision = self.revision.wrapping_add(1);
        self.record_damage((0..self.size.rows()).collect());
        self.collect_damage();
    }

    /// 用仿真器选择模型保存 typed 坐标，保证复制按 wrap 与宽字符规则生成；普通点击会替换旧选择并结束当前 token。
    ///
    /// @author kongweiguang
    pub fn set_selection(&mut self, selection: Option<TerminalSelection>) {
        if self.selection_gesture_id.is_some() {
            self.selection_gesture_ended = true;
        }
        self.selection_gesture_had_range = false;
        self.selection_gesture_empty_anchor = None;
        self.selection = selection;
        self.term.selection = self.selection.map(|selection| {
            let kind = match selection.kind {
                TerminalSelectionKind::Character => SelectionType::Simple,
                TerminalSelectionKind::Line => SelectionType::Lines,
                TerminalSelectionKind::Block => SelectionType::Block,
                TerminalSelectionKind::Word => SelectionType::Semantic,
            };
            let start = point(selection.start);
            let end = point(selection.end);
            let mut result = Selection::new(kind, start, Side::Left);
            result.update(end, Side::Right);
            result
        });
        self.revision = self.revision.wrapping_add(1);
        self.record_damage((0..self.size.rows()).collect());
    }

    /// 兼容旧调用方的拖选入口；没有快照版本时保留既有的坐标契约。
    ///
    /// @author kongweiguang
    pub fn update_selection_gesture(
        &mut self,
        gesture_id: u64,
        selection: TerminalSelection,
    ) -> bool {
        self.update_selection_gesture_at_revision(gesture_id, selection, None)
    }

    /// 将 Canvas 快照点按 Alacritty 的累计全屏滚动量映射到当前网格，并用精确行证据拒绝错选新内容。
    ///
    /// 尺寸或坐标代次变化代表无法保持的拓扑；裁剪越界和行内容变化也拒绝，不把旧点夹到另一行。
    /// 新手势只支持同一快照行上的起始范围；UI 的 MouseDown 以同点 anchor 开始，后续 Move 可跨行。
    ///
    /// @author kongweiguang
    pub fn update_selection_gesture_in_viewport(
        &mut self,
        gesture_id: u64,
        selection: TerminalSelection,
        viewport: SelectionViewport,
    ) -> bool {
        let Some(starts_new_gesture) = self.selection_gesture_start_state(gesture_id) else {
            return false;
        };
        if viewport.size != self.size || viewport.grid_epoch != self.grid_epoch {
            if starts_new_gesture {
                // 消耗过期坐标下的新 token，防止迟到 Move 在刷新后重建错误 anchor。
                self.tombstone_selection_gesture(gesture_id);
            }
            return false;
        }

        let Some(end) = self.current_point_for_viewport(selection.end, &viewport) else {
            if starts_new_gesture {
                self.tombstone_selection_gesture(gesture_id);
            }
            return false;
        };
        let start = if starts_new_gesture {
            // 当前 viewport 只携带 endpoint 行证据；MouseDown 的起止点必须位于同一原始快照行。
            if selection.start.line != selection.end.line {
                self.tombstone_selection_gesture(gesture_id);
                return false;
            }
            let Some(start) = self.current_point_for_viewport(selection.start, &viewport) else {
                self.tombstone_selection_gesture(gesture_id);
                return false;
            };
            start
        } else {
            // Move/End 的 start 仍是首次快照坐标；活动 anchor 已由 Alacritty 随输出移动。
            selection.start
        };
        let mapped = TerminalSelection {
            start,
            end,
            kind: selection.kind,
        };
        self.update_selection_gesture_at_revision(gesture_id, mapped, None)
    }

    /// 仅在创建新 anchor 时校验快照是否跨过网格坐标变更水位；选择和 viewport 更新不使绝对坐标失效。
    ///
    /// 新的全局递增 token 建立一次选择，同 token 后续坐标只推进原 Alacritty anchor；过期起点会留下
    /// ended tombstone，避免同 token 的迟到 Move 绕过校验。字符拖选的同点起始仍保留空 anchor。
    ///
    /// @author kongweiguang
    pub fn update_selection_gesture_at_revision(
        &mut self,
        gesture_id: u64,
        selection: TerminalSelection,
        expected_revision: Option<OutputRevision>,
    ) -> bool {
        let Some(starts_new_gesture) = self.selection_gesture_start_state(gesture_id) else {
            return false;
        };
        if starts_new_gesture {
            if expected_revision
                .is_some_and(|expected| !self.is_grid_coordinate_revision_valid(expected))
            {
                // UI 坐标已来自旧网格；保留已确认选区，并消耗该递增 token，拒绝其迟到 Move。
                self.tombstone_selection_gesture(gesture_id);
                return false;
            }

            // 普通 setter 会结束上一个 token；下面显式安装递增的新 id 作为当前手势。
            self.set_selection(Some(selection));
            if selection.kind == TerminalSelectionKind::Character
                && selection.start == selection.end
            {
                // MouseDown 需要先在 Alacritty 建立 SideLeft anchor，输出在首个 Move 前滚屏时它才能随行旋转。
                // 普通点击会替换旧选择；不更新 end 的 SideRight，避免同点 press 被解释成已选中一个字符。
                self.term.selection = Some(Selection::new(
                    SelectionType::Simple,
                    point(selection.start),
                    Side::Left,
                ));
                self.selection_gesture_had_range = false;
                let grid = self.term.grid();
                self.selection_gesture_empty_anchor = Some(EmptySelectionGestureAnchor {
                    line: selection.start.line,
                    scroll_origin: grid.scroll_origin(),
                    grid_epoch: self.grid_epoch,
                });
            }
            self.selection_gesture_id = Some(gesture_id);
            self.selection_gesture_ended = false;
            if selection.kind != TerminalSelectionKind::Character
                || selection.start != selection.end
            {
                self.selection_gesture_had_range = self
                    .term
                    .selection
                    .as_ref()
                    .and_then(|current| current.to_range(&self.term))
                    .is_some();
            }
            return true;
        }

        let effective_range_exists = self
            .term
            .selection
            .as_ref()
            .and_then(|current| current.to_range(&self.term))
            .is_some();
        if self
            .selection_gesture_empty_anchor
            .is_some_and(|anchor| !self.empty_selection_anchor_is_valid(anchor))
        {
            // 空锚点没有 effective range 可供现有裁剪门禁识别；失效时结束 token，禁止 Alacritty clamp 到新行。
            self.term.selection = None;
            self.selection = None;
            self.tombstone_selection_gesture(gesture_id);
            return false;
        }
        if self.term.selection.is_none()
            || (self.selection_gesture_had_range && !effective_range_exists)
        {
            // Alacritty 已因 trim 清除选择，保留当前 token 作为 tombstone，后续 move 不能重建旧 anchor。
            self.term.selection = None;
            self.selection = None;
            self.selection_gesture_had_range = true;
            self.selection_gesture_ended = true;
            self.selection_gesture_empty_anchor = None;
            return false;
        }

        let Some(current) = self.selection else {
            self.term.selection = None;
            self.selection_gesture_had_range = true;
            self.selection_gesture_ended = true;
            self.selection_gesture_empty_anchor = None;
            return false;
        };
        let Some(active) = self.term.selection.as_mut() else {
            return false;
        };

        active.update(point(selection.end), Side::Right);
        self.selection = Some(TerminalSelection {
            start: current.start,
            end: selection.end,
            kind: current.kind,
        });
        self.selection_gesture_had_range |= self
            .term
            .selection
            .as_ref()
            .and_then(|current| current.to_range(&self.term))
            .is_some();
        self.revision = self.revision.wrapping_add(1);
        self.record_damage((0..self.size.rows()).collect());
        true
    }

    /// 用两代 full-screen scroll origin 之差映射行，再要求当前行仍与快照 witness 完全一致。
    ///
    /// 不使用 history_size 推断滚动，因为历史满后它会饱和；映射越出当前 Grid 即视为已裁剪。
    ///
    /// @author kongweiguang
    fn current_point_for_viewport(
        &self,
        point: TerminalPoint,
        viewport: &SelectionViewport,
    ) -> Option<TerminalPoint> {
        let snapshot_row = i128::from(point.line) + viewport.scrollback_offset as i128;
        if snapshot_row < 0 || snapshot_row >= i128::from(viewport.size.rows()) {
            return None;
        }
        if point.column >= viewport.size.columns() {
            return None;
        }
        let witness = viewport.endpoint_row_witness.as_deref()?;
        let current_line = i128::from(point.line) + i128::from(viewport.scroll_origin)
            - i128::from(self.term.grid().scroll_origin());
        if current_line < i128::from(i32::MIN) || current_line > i128::from(i32::MAX) {
            return None;
        }
        let line = Line(current_line as i32);
        let grid = self.term.grid();
        if line < grid.topmost_line() || line > grid.bottommost_line() {
            return None;
        }
        if self.witness_for_grid_row(line).as_deref() != Some(witness) {
            return None;
        }
        Some(TerminalPoint {
            line: i64::from(line.0),
            column: point.column,
        })
    }

    /// 选择重基准和搜索共用渲染的行构造路径，避免分别计算字形、列宽和软换行。
    ///
    /// @author kongweiguang
    fn witness_for_grid_row(&self, line: Line) -> Option<Arc<TerminalRowWitness>> {
        let grid = self.term.grid();
        if line < grid.topmost_line() || line > grid.bottommost_line() {
            return None;
        }

        let row = &grid[line];
        let mut wrapped = false;
        let mut runs = Vec::new();
        let columns = usize::from(self.size.columns());
        for column in 0..columns {
            let column_u16 = column.min(usize::from(u16::MAX)) as u16;
            let cell = &row[Column(column)];
            if column + 1 == columns && cell.flags.contains(Flags::WRAPLINE) {
                wrapped = true;
            }
            let hyperlink = cell.hyperlink().map(|link| TerminalHyperlink {
                id: Arc::from(link.id()),
                uri: Arc::from(link.uri()),
            });
            push_cell_run(&mut runs, column_u16, cell, cell_style(cell), hyperlink);
        }
        let runs = freeze_runs(runs);
        Some(Arc::new(TerminalRowWitness::from_runs(wrapped, &runs)))
    }

    /// 仅为命中逻辑行按 byte-cell 首尾补读完整物理行证据，复用与渲染相同的 cell 几何。
    ///
    /// 单行有严格行数、文本和 span 上限；未命中逻辑行不会创建额外行文本或坐标副本。
    ///
    /// @author kongweiguang
    fn witnesses_for_search_logical_line(
        &self,
        logical_line: &SearchLogicalLine,
    ) -> Option<Vec<TerminalSearchRowWitness>> {
        let (first, _) = logical_line.byte_cells.first()?;
        let (last, _) = logical_line.byte_cells.last()?;
        let row_count = usize::try_from(last.line.checked_sub(first.line)?.checked_add(1)?).ok()?;
        if row_count == 0 || row_count > SEARCH_PROJECTION_MAX_LINE_ROWS {
            return None;
        }

        let mut rows = Vec::with_capacity(row_count);
        let mut text_bytes = 0usize;
        let mut cell_spans = 0usize;
        for absolute_line in first.line..=last.line {
            let line = Line(i32::try_from(absolute_line).ok()?);
            let witness = self.witness_for_grid_row(line)?;
            text_bytes = text_bytes.saturating_add(witness.text.len());
            cell_spans = cell_spans.saturating_add(witness.cell_spans.len());
            if text_bytes > SEARCH_PROJECTION_MAX_LINE_TEXT_BYTES
                || cell_spans > SEARCH_PROJECTION_MAX_LINE_CELL_SPANS
            {
                return None;
            }
            rows.push(TerminalSearchRowWitness {
                absolute_line,
                witness,
            });
        }
        Some(rows)
    }

    /// 合并 Term 坐标 epoch；一次 parser.advance 的多次内部操作只需使旧坐标失效一次。
    ///
    /// @author kongweiguang
    fn sync_term_grid_epoch(&mut self) {
        let term_epoch = self.term.grid_epoch();
        if term_epoch != self.observed_term_grid_epoch {
            self.grid_epoch = self
                .grid_epoch
                .checked_add(1)
                .expect("terminal grid epoch exhausted");
            self.observed_term_grid_epoch = term_epoch;
        }
    }

    /// 识别可开始的新单调 gesture token，并统一拒绝旧 token 与已结束 token 的迟到事件。
    ///
    /// @author kongweiguang
    fn selection_gesture_start_state(&self, gesture_id: u64) -> Option<bool> {
        match self.selection_gesture_id {
            Some(last_id) if gesture_id < last_id => None,
            Some(last_id) if gesture_id == last_id => {
                (!self.selection_gesture_ended).then_some(false)
            }
            Some(_) | None => Some(true),
        }
    }

    /// 记录被尺寸或快照 revision 拒绝的新 token，避免其迟到 Move 绕过起点校验。
    ///
    /// @author kongweiguang
    fn tombstone_selection_gesture(&mut self, gesture_id: u64) {
        self.selection_gesture_id = Some(gesture_id);
        self.selection_gesture_ended = true;
        self.selection_gesture_had_range = false;
        self.selection_gesture_empty_anchor = None;
    }

    /// 验证尚未形成范围的 MouseDown 锚点仍位于同一网格代次与可读历史内。
    ///
    /// 全屏上滚通过累计 origin 平移旧行；历史容量饱和后若锚点越过 topmost，必须拒绝首个 Move，
    /// 因为 Alacritty 的 range 投影会把过期起点 clamp 到当前最老行。普通 append 不改变 epoch，
    /// 因此仍可保留有效锚点并继续拖选。
    ///
    /// @author kongweiguang
    fn empty_selection_anchor_is_valid(&self, anchor: EmptySelectionGestureAnchor) -> bool {
        if anchor.grid_epoch != self.grid_epoch {
            return false;
        }
        let grid = self.term.grid();
        let line = i128::from(anchor.line) + i128::from(anchor.scroll_origin)
            - i128::from(grid.scroll_origin());
        line >= i128::from(grid.topmost_line().0) && line <= i128::from(grid.bottommost_line().0)
    }

    /// 判断快照 revision 到当前状态之间是否没有网格坐标重排，允许中间仅发生 selection 或 viewport 变化。
    ///
    /// 使用 wrapping 距离比较，和全局 revision 的回绕语义一致；有效区间两端都包含在内。
    ///
    /// @author kongweiguang
    fn is_grid_coordinate_revision_valid(&self, revision: OutputRevision) -> bool {
        let interval = self.revision.wrapping_sub(self.last_grid_revision);
        let offset = revision.wrapping_sub(self.last_grid_revision);
        offset <= interval
    }

    /// 依据 Alacritty 当前活动 grid 的真实 scrollback 与屏幕边界提交全选，不从 UI 快照反推坐标。
    ///
    /// 空且无历史的网格清除选择；alternate screen 只覆盖其活动网格，不把 primary 历史带入。
    /// 端点使用 bottommost_line 而非 total_lines 推算，避免落到最后一行之后的无效网格行。
    ///
    /// @author kongweiguang
    pub fn select_all(&mut self) {
        let grid = self.term.grid();
        let top = grid.topmost_line();
        let bottom = grid.bottommost_line();
        let has_scrollback = grid.history_size() > 0;
        let has_visible_content =
            (0..self.size.rows()).any(|line| grid[Line(i32::from(line))].line_length().0 > 0);

        if !has_scrollback && !has_visible_content {
            self.set_selection(None);
            return;
        }

        self.set_selection(Some(TerminalSelection {
            start: TerminalPoint {
                line: i64::from(top.0),
                column: 0,
            },
            end: TerminalPoint {
                line: i64::from(bottom.0),
                column: self.size.columns().saturating_sub(1),
            },
            kind: TerminalSelectionKind::Character,
        }));
    }

    /// 返回已由 Alacritty 根据终端宽字符及软换行规则提取的选择文本。
    ///
    /// @author kongweiguang
    pub fn selected_text(&self) -> Option<String> {
        self.term.selection_to_string()
    }

    /// 按当前引擎 viewport 生成行表；快照只读，避免刷新把显式滚动结果写回旧 offset。
    ///
    /// @author kongweiguang
    pub fn render_snapshot(&mut self, request: RenderSnapshotRequest) -> RenderSnapshot {
        let row_count = usize::from(self.size.rows());
        let mut builders: Vec<Option<RowBuilder>> =
            std::iter::repeat_with(|| None).take(row_count).collect();
        let renderable = self.term.renderable_content();
        let cursor_point = renderable.cursor.point;
        let mut cursor_cell = None;
        let mut previous_cell: Option<(i32, usize, &alacritty_terminal::term::cell::Cell)> = None;
        let grid = self.term.grid();
        let offset = grid.display_offset() as i64;
        for indexed in grid.display_iter() {
            let row_index = indexed.point.line.0 as i64 + offset;
            if row_index < 0 || row_index >= row_count as i64 {
                continue;
            }
            let row_index = row_index as usize;
            let cell = indexed.cell;
            let grid_line = indexed.point.line.0;
            let grid_column = indexed.point.column.0;
            let column = grid_column as u16;
            let absolute_line = row_index as i64 - offset;
            let cached = self
                .render_row_cache
                .get(row_index)
                .and_then(Option::as_ref)
                .filter(|row| row.absolute_line == absolute_line);
            if cached.is_none() {
                let row = builders[row_index].get_or_insert_with(|| RowBuilder {
                    absolute_line,
                    wrapped: false,
                    runs: Vec::new(),
                });
                if cell.flags.contains(Flags::WRAPLINE) && column + 1 == self.size.columns() {
                    row.wrapped = true;
                }
                let style = cell_style(cell);
                let hyperlink = cell.hyperlink().map(|link| TerminalHyperlink {
                    id: Arc::from(link.id()),
                    uri: Arc::from(link.uri()),
                });
                push_cell_run(&mut row.runs, column, cell, style, hyperlink);
            }
            if indexed.point == cursor_point {
                cursor_cell = if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    previous_cell
                        .filter(|(line, previous_column, previous)| {
                            *line == grid_line
                                && previous_column.saturating_add(1) == grid_column
                                && previous.flags.contains(Flags::WIDE_CHAR)
                        })
                        .map(|(_, previous_column, previous)| {
                            cursor_cell_from_cell(previous, previous_column as u16)
                        })
                        .or_else(|| Some(cursor_cell_from_cell(cell, column)))
                } else {
                    Some(cursor_cell_from_cell(cell, column))
                };
            }
            previous_cell = Some((grid_line, grid_column, cell));
        }
        let mut rows = Vec::with_capacity(row_count);
        for (index, builder_slot) in builders.iter_mut().enumerate() {
            if let Some(builder) = builder_slot.take() {
                let absolute_line = builder.absolute_line;
                let wrapped = builder.wrapped;
                let runs = freeze_runs(builder.runs);
                self.render_row_cache[index] = Some(CachedRenderRow {
                    absolute_line,
                    wrapped,
                    runs,
                });
            }
            let cached = self.render_row_cache[index]
                .as_ref()
                .cloned()
                .unwrap_or_else(|| CachedRenderRow {
                    absolute_line: index as i64 - offset,
                    wrapped: false,
                    runs: Arc::from([]),
                });
            rows.push(TerminalRow {
                viewport_row: index as u16,
                absolute_line: cached.absolute_line,
                wrapped: cached.wrapped,
                runs: cached.runs,
            });
        }
        let cursor_row = renderable.cursor.point.line.0 as i64 + renderable.display_offset as i64;
        let cursor_row_visible = cursor_row >= 0 && cursor_row < i64::from(self.size.rows());
        let cursor = TerminalCursor {
            row: cursor_row.clamp(0, i64::from(self.size.rows().saturating_sub(1))) as u16,
            column: renderable
                .cursor
                .point
                .column
                .0
                .min(usize::from(self.size.columns().saturating_sub(1))) as u16,
            shape: match renderable.cursor.shape {
                CursorShape::Block | CursorShape::HollowBlock => TerminalCursorShape::Block,
                CursorShape::Underline => TerminalCursorShape::Underline,
                CursorShape::Beam => TerminalCursorShape::Beam,
                CursorShape::Hidden => TerminalCursorShape::Block,
            },
            style_from_terminal: self.cursor_style_tracker.is_terminal_styled(),
            blinking: self.term.cursor_style().blinking,
            visible: renderable.cursor.shape != CursorShape::Hidden && cursor_row_visible,
        };
        let cursor_cell = (renderable.cursor.shape != CursorShape::Hidden && cursor_row_visible)
            .then_some(cursor_cell)
            .flatten();
        let modes = snapshot_modes(renderable.mode);
        let selection = self.selection.and_then(|stored| {
            renderable.selection.map(|range| TerminalSelection {
                start: TerminalPoint {
                    line: i64::from(range.start.line.0),
                    column: range.start.column.0.min(usize::from(u16::MAX)) as u16,
                },
                end: TerminalPoint {
                    line: i64::from(range.end.line.0),
                    column: range.end.column.0.min(usize::from(u16::MAX)) as u16,
                },
                kind: if range.is_block {
                    TerminalSelectionKind::Block
                } else {
                    stored.kind
                },
            })
        });
        let history_size = grid.history_size();
        RenderSnapshot {
            session_id: self.session_id.clone(),
            revision: self.revision,
            size: self.size,
            scrollback_offset: grid.display_offset(),
            history_size,
            scroll_origin: grid.scroll_origin(),
            grid_epoch: self.grid_epoch,
            title: self.title.clone(),
            current_working_directory: self.current_working_directory.clone(),
            cursor,
            cursor_cell,
            modes,
            palette: self.effective_palette(),
            rows: rows.into(),
            dirty_rows: self.dirty_since(request.since_revision).into(),
            selection,
            search_matches: Arc::from([]),
        }
    }

    /// 返回独立纯文本窗口，先限制行数再限制 UTF-8 字节并重算实际元数据。
    ///
    /// @author kongweiguang
    pub fn plaintext_snapshot(&self, request: PlainTextSnapshotRequest) -> PlainTextSnapshot {
        let grid = self.term.grid();
        let total_lines = grid.total_lines();
        let visible_rows = usize::from(self.size.rows());
        let available = if request.include_scrollback {
            total_lines
        } else {
            visible_rows
        };
        let line_limit = request.max_lines.min(available);
        let start = available.saturating_sub(line_limit);
        let first_line = if request.include_scrollback {
            i64::from(grid.topmost_line().0) + start as i64
        } else {
            -(grid.display_offset() as i64) + start as i64
        };
        let byte_limit = request.max_bytes.unwrap_or(usize::MAX);
        let mut lines = Vec::with_capacity(line_limit);
        for line_offset in start..available {
            let absolute = if request.include_scrollback {
                grid.topmost_line() + (line_offset as i32)
            } else {
                Line(-(grid.display_offset() as i32) + line_offset as i32)
            };
            let mut line = String::new();
            for column in 0..self.size.columns() {
                let cell = &grid[absolute][Column(column as usize)];
                if !cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    line.push(cell.c);
                    if let Some(extra) = cell.zerowidth() {
                        line.extend(extra);
                    }
                }
            }
            trim_line(&mut line);
            lines.push((line_offset, line));
        }
        let mut tail = Vec::new();
        let mut used_bytes = 0usize;
        let mut byte_truncated = false;
        for (line_offset, line) in lines.into_iter().rev() {
            let separator = usize::from(!tail.is_empty());
            let remaining = byte_limit.saturating_sub(used_bytes.saturating_add(separator));
            if line.len() > remaining {
                byte_truncated = true;
                if tail.is_empty() && remaining > 0 {
                    tail.push((line_offset, utf8_suffix(&line, remaining).to_owned()));
                }
                break;
            }
            used_bytes = used_bytes
                .saturating_add(separator)
                .saturating_add(line.len());
            tail.push((line_offset, line));
        }
        let returned_lines = tail.len();
        tail.reverse();
        let actual_first_line = tail
            .first()
            .map(|(line_offset, _)| {
                first_line.saturating_add(line_offset.saturating_sub(start) as i64)
            })
            .unwrap_or(first_line);
        let text = tail
            .into_iter()
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n");
        PlainTextSnapshot {
            session_id: self.session_id.clone(),
            first_line: actual_first_line,
            line_count: returned_lines,
            total_scrollback_lines: grid.history_size(),
            truncated: byte_truncated
                || returned_lines < line_limit
                || (request.include_scrollback && line_limit < available),
            text,
        }
    }

    /// 按 Alacritty 的 WRAPLINE 合并 bounded scrollback 逻辑行，再把命中还原为物理 cell 坐标。
    ///
    /// @author kongweiguang
    pub fn search(
        &self,
        request: &TerminalSearchRequest,
    ) -> Result<TerminalSearchResult, TerminalEngineError> {
        self.search_cancellable(request, || false)
    }

    /// 在有界 cell 扫描点检查取消信号；取消时丢弃中间命中，不伪装为完整搜索结果。
    ///
    /// 该核心层只依赖无副作用 predicate，调用方可桥接 CancellationToken 而不把 async runtime
    /// 带入 VT engine；每个物理行及每 256 个 cell 检查一次，限制长历史扫描的取消延迟。
    ///
    /// @author kongweiguang
    pub fn search_cancellable(
        &self,
        request: &TerminalSearchRequest,
        mut is_cancelled: impl FnMut() -> bool,
    ) -> Result<TerminalSearchResult, TerminalEngineError> {
        if is_cancelled() {
            return Err(TerminalEngineError::SearchCancelled);
        }
        let regex = if request.regex {
            RegexBuilder::new(&request.query)
                .case_insensitive(!request.case_sensitive)
                .build()
        } else {
            RegexBuilder::new(&regex::escape(&request.query))
                .case_insensitive(!request.case_sensitive)
                .build()
        }
        .map_err(TerminalEngineError::InvalidSearch)?;
        let grid = self.term.grid();
        let search_scroll_origin = grid.scroll_origin();
        let search_history_size = grid.history_size();
        if request.max_matches == 0 {
            return Ok(TerminalSearchResult {
                matches: Arc::from([]),
                selected_match: None,
                truncated: false,
                projection: Some(Arc::new(TerminalSearchProjection {
                    session_id: self.session_id.clone(),
                    revision: self.revision,
                    size: self.size,
                    grid_epoch: self.grid_epoch,
                    scroll_origin: search_scroll_origin,
                    history_size: search_history_size,
                    logical_lines: Arc::from([]),
                })),
            });
        }
        let mut matches = Vec::new();
        let mut projection_lines = Vec::new();
        let mut projection_budget = SearchProjectionBudget::default();
        let total = grid.total_lines();
        let top = grid.topmost_line();
        let mut truncated = false;
        let columns = self.size.columns();
        let mut logical_line = SearchLogicalLine::default();
        for line_index in 0..total {
            if is_cancelled() {
                return Err(TerminalEngineError::SearchCancelled);
            }
            let line = top + line_index as i32;
            for column in 0..columns {
                if usize::from(column) % 256 == 0 && is_cancelled() {
                    return Err(TerminalEngineError::SearchCancelled);
                }
                let cell = &grid[line][Column(column as usize)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                    || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                let width = if cell.flags.contains(Flags::WIDE_CHAR) {
                    2
                } else {
                    1
                };
                let point = TerminalPoint {
                    line: i64::from(line.0),
                    column,
                };
                logical_line.push_char(cell.c, point, width);
                for zero in cell.zerowidth().unwrap_or(&[]) {
                    logical_line.push_char(*zero, point, width);
                }
            }
            let wrapped = grid[line][Column(usize::from(columns.saturating_sub(1)))]
                .flags
                .contains(Flags::WRAPLINE);
            if !wrapped {
                if is_cancelled() {
                    return Err(TerminalEngineError::SearchCancelled);
                }
                let first_match_index = matches.len();
                collect_logical_line_matches(
                    &logical_line,
                    &regex,
                    request.max_matches,
                    columns,
                    &mut matches,
                    &mut truncated,
                );
                if matches.len() > first_match_index
                    && projection_lines.len() < SEARCH_PROJECTION_MAX_LOGICAL_LINES
                    && projection_budget.has_capacity()
                    && let Some(rows) = self.witnesses_for_search_logical_line(&logical_line)
                    && let Some(witness) = append_search_projection_line(
                        rows,
                        first_match_index,
                        &matches,
                        &mut projection_budget,
                    )
                {
                    projection_lines.push(witness);
                }
                if is_cancelled() {
                    return Err(TerminalEngineError::SearchCancelled);
                }
                logical_line = SearchLogicalLine::default();
            }
            if truncated {
                break;
            }
        }
        if !truncated && !logical_line.text.is_empty() {
            if is_cancelled() {
                return Err(TerminalEngineError::SearchCancelled);
            }
            let first_match_index = matches.len();
            collect_logical_line_matches(
                &logical_line,
                &regex,
                request.max_matches,
                columns,
                &mut matches,
                &mut truncated,
            );
            if matches.len() > first_match_index
                && projection_lines.len() < SEARCH_PROJECTION_MAX_LOGICAL_LINES
                && projection_budget.has_capacity()
                && let Some(rows) = self.witnesses_for_search_logical_line(&logical_line)
                && let Some(witness) = append_search_projection_line(
                    rows,
                    first_match_index,
                    &matches,
                    &mut projection_budget,
                )
            {
                projection_lines.push(witness);
            }
            if is_cancelled() {
                return Err(TerminalEngineError::SearchCancelled);
            }
        }
        if request.backwards {
            matches.reverse();
            let match_count = matches.len();
            for line in &mut projection_lines {
                line.matches = line
                    .matches
                    .iter()
                    .map(|(match_index, matched)| (match_count - 1 - match_index, matched.clone()))
                    .collect::<Vec<_>>()
                    .into();
            }
        }
        let selected_match = (!matches.is_empty()).then_some(0);
        Ok(TerminalSearchResult {
            matches: matches.into(),
            selected_match,
            truncated,
            projection: Some(Arc::new(TerminalSearchProjection {
                session_id: self.session_id.clone(),
                revision: self.revision,
                size: self.size,
                grid_epoch: self.grid_epoch,
                scroll_origin: search_scroll_origin,
                history_size: search_history_size,
                logical_lines: projection_lines.into(),
            })),
        })
    }

    /// 返回当前 revision，供应用 watch 合并终端更新。
    ///
    /// @author kongweiguang
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// 返回已验证尺寸，供 PTY 与仿真器保持一致。
    ///
    /// @author kongweiguang
    pub const fn size(&self) -> TerminalSize {
        self.size
    }

    /// 只读取当前可见 cursor 坐标；诊断不得调用会改变 viewport/cache/revision 的 render_snapshot。
    ///
    /// @author kongweiguang
    pub fn diagnostic_cursor_position(&self) -> (u16, u16) {
        let renderable = self.term.renderable_content();
        let cursor_point = renderable.cursor.point;
        let row = (cursor_point.line.0 as i64 + renderable.display_offset as i64)
            .clamp(0, i64::from(self.size.rows().saturating_sub(1))) as u16;
        let column = cursor_point
            .column
            .0
            .min(usize::from(self.size.columns().saturating_sub(1))) as u16;
        (row, column)
    }

    /// 暴露仿真器协商后的键鼠模式，让应用编码输入时不重复解析协议状态。
    ///
    /// @author kongweiguang
    pub fn input_modes(&self) -> TerminalModes {
        snapshot_modes(self.term.renderable_content().mode)
    }

    /// 更新同一份绘制与 OSC 查询颜色源；相同色盘同步保持 revision/damage 稳定，避免重复主题通知作废版本状态。
    ///
    /// @author kongweiguang
    pub fn set_palette(&mut self, palette: TerminalPalette) {
        if self.palette == palette {
            return;
        }

        self.palette = palette;
        self.revision = self.revision.wrapping_add(1);
        self.record_damage((0..self.size.rows()).collect());
    }

    /// 按偏好设置调整 Alacritty 历史行数；实际裁剪会推进 grid watermark，清除重排缓存并刷新视口。
    ///
    /// @author kongweiguang
    pub fn set_scrollback_limit(&mut self, requested: usize) -> usize {
        let maximum = checked_scrollback(self.size, self.limits).unwrap_or_default();
        let actual = requested.min(maximum);
        if actual == self.scrollback_limit {
            return actual;
        }
        let config = Config {
            scrolling_history: actual,
            ..Config::default()
        };
        self.term.set_options(config);
        self.sync_term_grid_epoch();
        self.scrollback_limit = actual;
        self.invalidate_all_render_rows();
        self.revision = self.revision.wrapping_add(1);
        self.last_grid_revision = self.revision;
        self.record_damage((0..self.size.rows()).collect());
        self.term.reset_damage();
        actual
    }

    /// 从用户主题 base palette 与 Alacritty 当前 OSC override 构造快照色盘，使绘制、查询和 reset 一致。
    ///
    /// @author kongweiguang
    fn effective_palette(&self) -> TerminalPalette {
        let mut palette = self.palette;
        // Alacritty 将颜色存入私有定长数组，只公开索引访问；按其公开 COUNT 常量逐项读取。
        for index in 0..alacritty_terminal::term::color::COUNT {
            if let Some(color) = self.term.colors()[index] {
                palette.apply_named_override(index, TerminalRgb::new(color.r, color.g, color.b));
            }
        }
        palette
    }

    /// 汇总 Alacritty 自带 damage iterator；snapshot 的读取不会消费其它视图的 dirty 状态。
    ///
    /// @author kongweiguang
    fn collect_damage(&mut self) {
        let rows = usize::from(self.size.rows());
        let (full_damage, dirty_rows): (bool, Vec<u16>) = match self.term.damage() {
            TermDamage::Full => (true, (0..self.size.rows()).collect()),
            TermDamage::Partial(damage) => (
                false,
                damage
                    .filter_map(|line| (line.line < rows).then_some(line.line as u16))
                    .collect(),
            ),
        };
        if full_damage {
            self.invalidate_all_render_rows();
        } else {
            self.invalidate_render_rows(&dirty_rows);
        }
        self.record_damage(dirty_rows);
        self.term.reset_damage();
    }

    /// 使 Alacritty 给出的当前 viewport 行失效；空操作会保留其它行 Arc 以减少分配。
    ///
    /// @author kongweiguang
    fn invalidate_render_rows(&mut self, rows: &[u16]) {
        for row in rows {
            if let Some(cached) = self.render_row_cache.get_mut(usize::from(*row)) {
                *cached = None;
            }
        }
    }

    /// 清除所有 viewport 行缓存，用于滚动、尺寸和屏幕身份变化导致的整屏重映射。
    ///
    /// @author kongweiguang
    fn invalidate_all_render_rows(&mut self) {
        self.render_row_cache.fill(None);
    }

    /// 累积多个输出批次的行损伤，避免 watch 合并更新时漏掉之前的变化。
    ///
    /// @author kongweiguang
    fn record_damage(&mut self, mut rows: Vec<u16>) {
        rows.sort_unstable();
        rows.dedup();
        if rows.is_empty() {
            return;
        }
        self.damage_history.push_back((self.revision, rows));
        while self.damage_history.len() > 256 {
            self.damage_history.pop_front();
        }
    }

    /// 汇总请求 revision 之后的 dirty rows；历史被裁掉时退回全屏保证正确性。
    ///
    /// @author kongweiguang
    fn dirty_since(&self, since_revision: Option<u64>) -> Vec<u16> {
        let Some(since_revision) = since_revision else {
            return (0..self.size.rows()).collect();
        };
        if since_revision >= self.revision {
            return Vec::new();
        }
        let Some((oldest_revision, _)) = self.damage_history.front() else {
            return (0..self.size.rows()).collect();
        };
        if since_revision.saturating_add(1) < *oldest_revision {
            return (0..self.size.rows()).collect();
        }
        let mut rows = Vec::new();
        for (revision, changed) in &self.damage_history {
            if *revision > since_revision {
                rows.extend_from_slice(changed);
            }
        }
        rows.sort_unstable();
        rows.dedup();
        rows
    }
}

/// 把每種尺寸限制先統一校驗，並將 scrollback 裁到 cell budget 允許的最大值。
///
/// @author kongweiguang
fn checked_scrollback(
    size: TerminalSize,
    limits: TerminalLimits,
) -> Result<usize, TerminalEngineError> {
    let columns = usize::from(size.columns());
    let rows = usize::from(size.rows());
    if size.columns() > limits.max_columns
        || size.rows() > limits.max_rows
        || columns.saturating_mul(rows) > limits.max_cells
    {
        return Err(TerminalEngineError::Capacity {
            max_columns: limits.max_columns,
            max_rows: limits.max_rows,
            max_cells: limits.max_cells,
        });
    }
    let history_budget = limits
        .max_cells
        .saturating_sub(columns.saturating_mul(rows))
        / columns;
    Ok(limits.max_scrollback_lines.min(history_budget))
}

/// 将领域的逻辑点转换成 Alacritty 有界整数网格点。
///
/// @author kongweiguang
fn point(point: TerminalPoint) -> Point {
    Point::new(
        Line(point.line.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32),
        Column(usize::from(point.column)),
    )
}

/// 从 UTF-8 边界保留文本尾部，使 MCP 字节预算不会生成无效 UTF-8。
///
/// @author kongweiguang
fn utf8_suffix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut start = value.len() - max_bytes;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

/// 保留默认色、特殊前景与调色索引的独立语义，避免 UI 颜色映射混淆终端协议值。
///
/// @author kongweiguang
fn terminal_color(color: Color) -> TerminalColor {
    match color {
        Color::Named(NamedColor::Foreground) => TerminalColor::DefaultForeground,
        Color::Named(NamedColor::Background) => TerminalColor::DefaultBackground,
        Color::Named(NamedColor::Cursor) => TerminalColor::Cursor,
        Color::Named(NamedColor::BrightForeground) => TerminalColor::BrightForeground,
        Color::Named(NamedColor::DimForeground) => TerminalColor::DimForeground,
        Color::Named(named) => {
            let index = named as u16;
            if (0..=15).contains(&index) {
                TerminalColor::Ansi(index as u8)
            } else if (259..=266).contains(&index) {
                TerminalColor::DimAnsi((index - 259) as u8)
            } else {
                TerminalColor::DefaultForeground
            }
        }
        Color::Indexed(index) => TerminalColor::Indexed(index),
        Color::Spec(rgb) => TerminalColor::Rgb {
            red: rgb.r,
            green: rgb.g,
            blue: rgb.b,
        },
    }
}

/// 将终端 cell 的颜色与显示属性转换为可比较的 run style。
///
/// @author kongweiguang
fn cell_style(cell: &alacritty_terminal::term::cell::Cell) -> TerminalStyle {
    let flags = cell.flags;
    let underline = if flags.contains(Flags::UNDERCURL) {
        UnderlineStyle::Curly
    } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
        UnderlineStyle::Double
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        UnderlineStyle::Dotted
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        UnderlineStyle::Dashed
    } else if flags.contains(Flags::UNDERLINE) {
        UnderlineStyle::Single
    } else {
        UnderlineStyle::None
    };
    TerminalStyle {
        foreground: terminal_color(cell.fg),
        background: terminal_color(cell.bg),
        underline_color: cell.underline_color().map(terminal_color),
        attributes: TerminalAttributes {
            bold: flags.contains(Flags::BOLD),
            dim: flags.contains(Flags::DIM),
            italic: flags.contains(Flags::ITALIC),
            underline,
            inverse: flags.contains(Flags::INVERSE),
            hidden: flags.contains(Flags::HIDDEN),
            strikethrough: flags.contains(Flags::STRIKEOUT),
        },
    }
}

/// 从 VT cell 读取其已写入的 grapheme、样式与 Alacritty 宽字符标记供光标反色绘制。
///
/// @author kongweiguang
fn cursor_cell_from_cell(
    cell: &alacritty_terminal::term::cell::Cell,
    start_column: u16,
) -> TerminalCursorCell {
    let spacer = cell.flags.contains(Flags::WIDE_CHAR_SPACER);
    let mut text = String::new();
    text.push(if spacer { ' ' } else { cell.c });
    if !spacer && let Some(zerowidth) = cell.zerowidth() {
        text.extend(zerowidth);
    }
    TerminalCursorCell {
        text: text.into(),
        columns: if cell.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        },
        start_column,
        style: cell_style(cell),
    }
}

/// 將相邻且同样式的 cell 合并为一段，宽字符 spacer 仅扩展列宽不重复绘字。
///
/// @author kongweiguang
fn push_cell_run(
    runs: &mut Vec<RunBuilder>,
    column: u16,
    cell: &alacritty_terminal::term::cell::Cell,
    style: TerminalStyle,
    hyperlink: Option<TerminalHyperlink>,
) {
    let spacer = cell.flags.contains(Flags::WIDE_CHAR_SPACER);
    if let Some(last) = runs.last_mut().filter(|last| {
        last.style == style && last.hyperlink == hyperlink && last.end_column == column
    }) {
        last.end_column = column.saturating_add(1);
        let start_byte = last.text.len();
        if !spacer || cell.zerowidth().is_some() {
            if !spacer {
                last.text.push(cell.c);
            }
            if let Some(extra) = cell.zerowidth() {
                last.text.extend(extra);
            }
        }
        if last.text.len() > start_byte {
            let column_width = if !spacer && cell.flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };
            let end_byte = last.text.len();
            last.cell_spans.push(TerminalTextCellSpan {
                byte_range: start_byte..end_byte,
                column_range: column..column.saturating_add(column_width),
            });
        }
    } else {
        let mut text = String::new();
        if !spacer {
            text.push(cell.c);
        }
        if let Some(extra) = cell.zerowidth() {
            text.extend(extra);
        }
        let cell_spans = if text.is_empty() {
            Vec::new()
        } else {
            let column_width = if !spacer && cell.flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };
            vec![TerminalTextCellSpan {
                byte_range: 0..text.len(),
                column_range: column..column.saturating_add(column_width),
            }]
        };
        runs.push(RunBuilder {
            start_column: column,
            end_column: column.saturating_add(1),
            text,
            cell_spans,
            style,
            hyperlink,
        });
    }
}

/// Freeze mutable row builders into the same immutable run representation for rendering and witnesses.
///
/// @author kongweiguang
fn freeze_runs(runs: Vec<RunBuilder>) -> Arc<[TerminalRun]> {
    runs.into_iter()
        .map(|run| TerminalRun {
            start_column: run.start_column,
            end_column: run.end_column,
            text: run.text.into(),
            cell_spans: run.cell_spans.into(),
            style: run.style,
            hyperlink: run.hyperlink,
        })
        .collect::<Vec<_>>()
        .into()
}

/// 从 Alacritty mode bitset 暴露键盘、鼠标与屏幕状态给 UI 输入层。
///
/// @author kongweiguang
fn snapshot_modes(mode: TermMode) -> TerminalModes {
    let mouse_tracking = if mode.contains(TermMode::MOUSE_MOTION) {
        crate::snapshot::MouseTrackingMode::AnyMotion
    } else if mode.contains(TermMode::MOUSE_DRAG) {
        crate::snapshot::MouseTrackingMode::ButtonMotion
    } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
        crate::snapshot::MouseTrackingMode::Click
    } else {
        crate::snapshot::MouseTrackingMode::None
    };
    let mut kitty = 0;
    if mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
        kitty |= 1;
    }
    if mode.contains(TermMode::REPORT_EVENT_TYPES) {
        kitty |= 2;
    }
    if mode.contains(TermMode::REPORT_ALTERNATE_KEYS) {
        kitty |= 4;
    }
    if mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) {
        kitty |= 8;
    }
    if mode.contains(TermMode::REPORT_ASSOCIATED_TEXT) {
        kitty |= 16;
    }
    TerminalModes {
        alternate_screen: mode.contains(TermMode::ALT_SCREEN),
        application_cursor: mode.contains(TermMode::APP_CURSOR),
        alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
        bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
        mouse_tracking,
        sgr_mouse: mode.contains(TermMode::SGR_MOUSE),
        focus_reporting: mode.contains(TermMode::FOCUS_IN_OUT),
        kitty_keyboard_flags: kitty,
    }
}

/// 移除右侧填充空格；保留内部空格供 MCP 与搜索使用。
///
/// @author kongweiguang
fn trim_line(line: &mut String) {
    while line.ends_with(' ') {
        line.pop();
    }
}
