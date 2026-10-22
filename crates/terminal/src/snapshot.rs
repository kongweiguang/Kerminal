//! 供桌面 renderer 与 MCP 分别消费的终端快照模型。
//!
//! @author kongweiguang

use std::ops::Range;
use std::sync::Arc;

use kerminal_domain::identity::SessionId;
use kerminal_domain::sessions::TerminalSize;

/// 引擎与 UI 快照共享的状态版本；手势起点用它拒绝已经失效的网格坐标。
pub type OutputRevision = u64;

/// 终端颜色保留调色板语义，避免 UI 把默认前景色固化为 RGB。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TerminalColor {
    DefaultForeground,
    DefaultBackground,
    Cursor,
    Ansi(u8),
    DimAnsi(u8),
    BrightForeground,
    DimForeground,
    Indexed(u8),
    Rgb { red: u8, green: u8, blue: u8 },
}

/// 引擎与 renderer 共用的精确颜色值，避免 ANSI 和 OSC 查询各维护一套常量。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct TerminalRgb {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

/// 语义颜色和 xterm 256 色索引的唯一解析表；UI 只负责转换到 GPUI 颜色类型。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalPalette {
    pub foreground: TerminalRgb,
    pub background: TerminalRgb,
    pub cursor: TerminalRgb,
    pub ansi: [TerminalRgb; 16],
    pub dim_ansi: [TerminalRgb; 8],
    pub bright_foreground: TerminalRgb,
    pub dim_foreground: TerminalRgb,
    /// OSC 动态覆盖的 xterm 256 索引；None 时回退到 ANSI/xterm 算法基色。
    indexed_overrides: [Option<TerminalRgb>; 256],
}

impl Default for TerminalPalette {
    /// 提供与现有 Kerminal 终端样式兼容的默认前景、背景和 xterm ANSI 调色板。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self::with_defaults(
            TerminalRgb::new(0xd0, 0xd0, 0xd0),
            TerminalRgb::new(0x1c, 0x1c, 0x1c),
            TerminalRgb::new(0xd0, 0xd0, 0xd0),
        )
    }
}

impl TerminalRgb {
    /// 构造跨平台 RGB 值，不将 GPUI 类型引入纯终端模型。
    ///
    /// @author kongweiguang
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }
}

impl TerminalPalette {
    /// 用窗口主题传入的默认色生成同一 ANSI/Dim/Indexed 解析表供绘制和 OSC 回答共享。
    ///
    /// @author kongweiguang
    pub fn with_defaults(
        foreground: TerminalRgb,
        background: TerminalRgb,
        cursor: TerminalRgb,
    ) -> Self {
        let ansi = [
            TerminalRgb::new(0x00, 0x00, 0x00),
            TerminalRgb::new(0xcd, 0x00, 0x00),
            TerminalRgb::new(0x00, 0xcd, 0x00),
            TerminalRgb::new(0xcd, 0xcd, 0x00),
            TerminalRgb::new(0x00, 0x00, 0xee),
            TerminalRgb::new(0xcd, 0x00, 0xcd),
            TerminalRgb::new(0x00, 0xcd, 0xcd),
            TerminalRgb::new(0xe5, 0xe5, 0xe5),
            TerminalRgb::new(0x7f, 0x7f, 0x7f),
            TerminalRgb::new(0xff, 0x00, 0x00),
            TerminalRgb::new(0x00, 0xff, 0x00),
            TerminalRgb::new(0xff, 0xff, 0x00),
            TerminalRgb::new(0x5c, 0x5c, 0xff),
            TerminalRgb::new(0xff, 0x00, 0xff),
            TerminalRgb::new(0x00, 0xff, 0xff),
            TerminalRgb::new(0xff, 0xff, 0xff),
        ];
        Self::from_ansi(foreground, background, cursor, ansi)
    }

    /// 由 renderer 或偏好设置提供完整 ANSI 色组，再统一派生 dim 与语义前景色供 OSC/绘制共用。
    ///
    /// @author kongweiguang
    pub fn from_ansi(
        foreground: TerminalRgb,
        background: TerminalRgb,
        cursor: TerminalRgb,
        ansi: [TerminalRgb; 16],
    ) -> Self {
        let dim_ansi = std::array::from_fn(|index| dim_rgb(ansi[index]));
        let bright_foreground = ansi[15];
        let dim_foreground = blend_rgb(foreground, background, 2, 3);
        Self {
            foreground,
            background,
            cursor,
            ansi,
            dim_ansi,
            bright_foreground,
            dim_foreground,
            indexed_overrides: [None; 256],
        }
    }

    /// 将 engine style 的语义颜色解析为真实 RGB，UI 不需要了解 Alacritty enum 序号。
    ///
    /// @author kongweiguang
    pub fn resolve(&self, color: TerminalColor) -> TerminalRgb {
        match color {
            TerminalColor::DefaultForeground => self.foreground,
            TerminalColor::DefaultBackground => self.background,
            TerminalColor::Cursor => self.cursor,
            TerminalColor::Ansi(index) => self.ansi[usize::from(index.min(15))],
            TerminalColor::DimAnsi(index) => self.dim_ansi[usize::from(index.min(7))],
            TerminalColor::BrightForeground => self.bright_foreground,
            TerminalColor::DimForeground => self.dim_foreground,
            TerminalColor::Indexed(index) => self.indexed(index),
            TerminalColor::Rgb { red, green, blue } => TerminalRgb::new(red, green, blue),
        }
    }

    /// 在临时有效色盘上覆盖 OSC 编号；engine 每次快照都从用户 base palette 重算，reset 自动恢复主题色。
    ///
    /// @author kongweiguang
    pub(crate) fn apply_named_override(&mut self, index: usize, color: TerminalRgb) -> bool {
        match index {
            0..=15 => self.ansi[index] = color,
            16..=255 => self.indexed_overrides[index] = Some(color),
            256 => self.foreground = color,
            257 => self.background = color,
            258 => self.cursor = color,
            259..=266 => self.dim_ansi[index - 259] = color,
            267 => self.bright_foreground = color,
            268 => self.dim_foreground = color,
            _ => return false,
        }
        true
    }

    /// 解析 OSC 10/11/12 等 Alacritty NamedColor 查询索引，回用当前同一套语义色。
    ///
    /// @author kongweiguang
    pub fn named_index(&self, index: usize) -> Option<TerminalRgb> {
        match index {
            0..=15 => Some(self.ansi[index]),
            16..=255 => Some(self.indexed(index as u8)),
            256 => Some(self.foreground),
            257 => Some(self.background),
            258 => Some(self.cursor),
            259..=266 => Some(self.dim_ansi[index - 259]),
            267 => Some(self.bright_foreground),
            268 => Some(self.dim_foreground),
            _ => None,
        }
    }

    /// 按 xterm 256 色标准解析 ANSI、6×6×6 色立方和灰阶区间。
    ///
    /// @author kongweiguang
    fn indexed(&self, index: u8) -> TerminalRgb {
        let value = usize::from(index);
        if let Some(color) = self.indexed_overrides[value] {
            return color;
        }
        match value {
            0..=15 => self.ansi[value],
            16..=231 => {
                let value = value - 16;
                let channel = |component: usize| {
                    if component == 0 {
                        0
                    } else {
                        (component * 40 + 55) as u8
                    }
                };
                TerminalRgb::new(
                    channel(value / 36),
                    channel((value / 6) % 6),
                    channel(value % 6),
                )
            }
            _ => {
                let gray = 8 + ((value - 232) * 10) as u8;
                TerminalRgb::new(gray, gray, gray)
            }
        }
    }
}

/// 依照终端 dim 语义压低 RGB 强度，不依赖渲染器的透明度合成行为。
///
/// @author kongweiguang
fn dim_rgb(color: TerminalRgb) -> TerminalRgb {
    TerminalRgb::new(
        ((u16::from(color.red) * 2) / 3) as u8,
        ((u16::from(color.green) * 2) / 3) as u8,
        ((u16::from(color.blue) * 2) / 3) as u8,
    )
}

/// 将前景按比例混合到背景上，保证 DimForeground 有一个稳定的实际颜色。
///
/// @author kongweiguang
fn blend_rgb(
    foreground: TerminalRgb,
    background: TerminalRgb,
    numerator: u16,
    denominator: u16,
) -> TerminalRgb {
    let blend = |front: u8, back: u8| {
        ((u16::from(front) * numerator + u16::from(back) * (denominator - numerator)) / denominator)
            as u8
    };
    TerminalRgb::new(
        blend(foreground.red, background.red),
        blend(foreground.green, background.green),
        blend(foreground.blue, background.blue),
    )
}

/// 终端字符样式；使用显式字段跨越 Alacritty 与 GPUI 边界。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct TerminalAttributes {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: UnderlineStyle,
    pub inverse: bool,
    pub hidden: bool,
    pub strikethrough: bool,
}

/// 终端可表达的下划线样式。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// 按可绘制的相同属性合并相邻单元格，供 GPUI 一次 shape 多个字形。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct TerminalStyle {
    pub foreground: TerminalColor,
    pub background: TerminalColor,
    pub underline_color: Option<TerminalColor>,
    pub attributes: TerminalAttributes,
}

/// OSC 8 超链接标记；URI 由终端内容提供，打开动作仍由 UI 安全策略决定。
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TerminalHyperlink {
    pub id: Arc<str>,
    pub uri: Arc<str>,
}

/// 一行中连续、同样式的可见文本区间。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalRun {
    pub start_column: u16,
    pub end_column: u16,
    pub text: Arc<str>,
    /// 把每段 UTF-8 文本精确映射到 Alacritty cell 列，供搜索高亮等功能使用。
    pub cell_spans: Arc<[TerminalTextCellSpan]>,
    pub style: TerminalStyle,
    pub hyperlink: Option<TerminalHyperlink>,
}

/// run 文本中的一个 UTF-8 范围及其真实终端列范围，两个 Range 均为半开区间。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalTextCellSpan {
    /// 相对所属 `TerminalRun::text` 的字节边界，保证落在 UTF-8 字符边界。
    pub byte_range: Range<usize>,
    /// 所属行中的 cell 列边界；宽字字符映射两个列，组合字符沿用基字符列。
    pub column_range: Range<u16>,
}

/// 已合并样式 run 的可见终端行。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalRow {
    pub viewport_row: u16,
    pub absolute_line: i64,
    pub wrapped: bool,
    pub runs: Arc<[TerminalRun]>,
}

/// 终端行的精确文字坐标证据；省略样式和超链接，避免仅视觉变化使旧手势失效。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalRowWitness {
    pub wrapped: bool,
    pub text: Arc<str>,
    pub cell_spans: Arc<[TerminalTextCellSpan]>,
}

impl TerminalRow {
    /// 从当前行已缓存的 runs 生成共享证据，只为正在操作的端点分配，不复制 scrollback。
    ///
    /// @author kongweiguang
    pub fn witness(&self) -> Arc<TerminalRowWitness> {
        Arc::new(TerminalRowWitness::from_runs(self.wrapped, &self.runs))
    }
}

impl TerminalRowWitness {
    /// 将样式分段的行 runs 归一成连续文本和绝对 cell span，作为稳定坐标校验依据。
    ///
    /// 拼接时重定位每个 byte range，保留 Alacritty 实际列宽与软换行信息；不纳入 style/link，
    /// 因此主题变化和 run 合并边界不会改变同一行的坐标证据。
    ///
    /// @author kongweiguang
    pub fn from_runs(wrapped: bool, runs: &[TerminalRun]) -> Self {
        let text_len = runs.iter().map(|run| run.text.len()).sum();
        let span_len = runs.iter().map(|run| run.cell_spans.len()).sum();
        let mut text = String::with_capacity(text_len);
        let mut cell_spans = Vec::with_capacity(span_len);
        for run in runs {
            let byte_offset = text.len();
            text.push_str(&run.text);
            cell_spans.extend(run.cell_spans.iter().map(|span| TerminalTextCellSpan {
                byte_range: span.byte_range.start + byte_offset..span.byte_range.end + byte_offset,
                column_range: span.column_range.clone(),
            }));
        }
        Self {
            wrapped,
            text: Arc::from(text),
            cell_spans: cell_spans.into(),
        }
    }
}

/// 终端光标形状，visible 为 false 时仍保留位置但不绘制。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalCursorShape {
    Block,
    Underline,
    Beam,
}

/// 光标 viewport 坐标。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalCursor {
    pub row: u16,
    pub column: u16,
    pub shape: TerminalCursorShape,
    /// 自上次 RIS 或 DECSTR 以来是否实际观察到 DECSCUSR；false 时 UI 可用用户默认形状。
    pub style_from_terminal: bool,
    /// 当前 DECSCUSR 样式是否要求闪烁；UI 可再与用户偏好相与，不猜协议状态。
    pub blinking: bool,
    pub visible: bool,
}

/// 光标所在的终端字符及其真实列宽，供 renderer 按原样反色而不估算 Unicode 宽度。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalCursorCell {
    /// 已包含组合字符的 grapheme 文本；空白单元格显式存为一个空格。
    pub text: Arc<str>,
    /// 字符在终端网格中占用的列数，由 Alacritty cell flags 决定。
    pub columns: u8,
    /// 宽字符尾部 spacer 将此列指向真实 glyph 的起始列。
    pub start_column: u16,
    pub style: TerminalStyle,
}

/// 仿真器当前启用的输入协议状态，GPUI 不需要重新解析控制字节。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalModes {
    pub alternate_screen: bool,
    pub application_cursor: bool,
    pub alternate_scroll: bool,
    pub bracketed_paste: bool,
    pub mouse_tracking: MouseTrackingMode,
    pub sgr_mouse: bool,
    pub focus_reporting: bool,
    /// Kitty protocol progressive-enhancement flags；各 bit 与协议规范一致。
    pub kitty_keyboard_flags: u8,
}

/// DEC 鼠标跟踪方式；分离模式才能拒绝不受支持的 move/drag 事件。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MouseTrackingMode {
    #[default]
    None,
    Click,
    ButtonMotion,
    AnyMotion,
}

/// 终端网格中的稳定逻辑坐标；line 为含 scrollback 的绝对行号。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct TerminalPoint {
    pub line: i64,
    pub column: u16,
}

/// 选择区域类型。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalSelectionKind {
    Character,
    Line,
    Block,
    Word,
}

/// 终端文本选择的逻辑端点。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSelection {
    pub start: TerminalPoint,
    pub end: TerminalPoint,
    pub kind: TerminalSelectionKind,
}

/// 描述终端点坐标所属快照的网格与历史偏移，供异步手势在 engine 锁内映射到当前视口。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectionViewport {
    pub size: TerminalSize,
    pub scrollback_offset: usize,
    /// 当前活动 grid 的累计全屏上滚行数；history 满时仍递增。
    pub scroll_origin: u64,
    /// resize、屏幕切换、部分滚动或历史重置后的坐标代次。
    pub grid_epoch: u64,
    /// endpoint 所属快照行的精确文字/宽度/wrap 证据；缺失时引擎拒绝猜测坐标。
    pub endpoint_row_witness: Option<Arc<TerminalRowWitness>>,
}

/// 搜索命中在终端网格中的坐标范围。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalSearchMatch {
    pub range: Range<TerminalPoint>,
}

/// 快照读取引擎当前视口；显式滚动命令负责改变 viewport，不复制完整 scrollback。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderSnapshotRequest {
    /// 仅返回该 UI view 自上次提交后的 dirty rows；None 表示首次绘制全视口。
    pub since_revision: Option<OutputRevision>,
}

impl Default for RenderSnapshotRequest {
    /// 默认从当前 screen 初次绘制，调用方随后用返回 revision 做增量读取。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            since_revision: None,
        }
    }
}

/// RenderSnapshot 是 engine 到 UI 的 typed view model，不包含 GPU/GPUI 对象。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderSnapshot {
    pub session_id: SessionId,
    pub revision: OutputRevision,
    pub size: TerminalSize,
    pub scrollback_offset: usize,
    /// 当前活动 grid 可滚动历史的总行数，同时也是 viewport offset 的最大有效值。
    pub history_size: usize,
    /// 当前活动 grid 的累计全屏上滚行数；与历史容量无关，供旧快照点映射。
    pub scroll_origin: u64,
    /// 坐标拓扑变化水位；跨 resize、备用屏切换或不可平移的历史操作后递增。
    pub grid_epoch: u64,
    pub title: Option<Arc<str>>,
    /// 同一 engine 最近收到的合法 OSC7 绝对路径；无通知或重连后为 None，不能当作路由主机。
    pub current_working_directory: Option<Arc<str>>,
    pub cursor: TerminalCursor,
    pub cursor_cell: Option<TerminalCursorCell>,
    pub modes: TerminalModes,
    pub palette: TerminalPalette,
    pub rows: Arc<[TerminalRow]>,
    pub dirty_rows: Arc<[u16]>,
    pub selection: Option<TerminalSelection>,
    pub search_matches: Arc<[TerminalSearchMatch]>,
}

/// 请求明确指定文本快照窗口与上限，MCP 无需推断 render rows 的解码方式。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlainTextSnapshotRequest {
    pub include_scrollback: bool,
    pub max_lines: usize,
    /// 可选 UTF-8 输出字节上限；限制 MCP 大响应内存，不切断终端底层数据流。
    pub max_bytes: Option<usize>,
}

impl Default for PlainTextSnapshotRequest {
    /// 以屏幕文本作为安全默认值，MCP 必须显式选择是否包含 scrollback。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            include_scrollback: false,
            max_lines: 2000,
            max_bytes: Some(1_048_576),
        }
    }
}

/// MCP/自动化消费的纯文本结果，明确给出窗口和是否截断。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlainTextSnapshot {
    pub session_id: SessionId,
    pub text: String,
    pub first_line: i64,
    pub line_count: usize,
    pub total_scrollback_lines: usize,
    pub truncated: bool,
}

/// 终端搜索选项。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalSearchRequest {
    pub query: String,
    pub case_sensitive: bool,
    pub regex: bool,
    pub backwards: bool,
    pub max_matches: usize,
}

/// 搜索结果及当前选中的命中位置。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalSearchResult {
    pub matches: Arc<[TerminalSearchMatch]>,
    pub selected_match: Option<usize>,
    pub truncated: bool,
    /// 仅供桌面视图把已提交命中投影到后续快照；MCP 适配不得暴露此内部证据。
    #[doc(hidden)]
    pub projection: Option<Arc<TerminalSearchProjection>>,
}

/// 搜索时冻结的网格水位及有界逻辑行证据；字段留在 crate 内避免形成外部坐标契约。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalSearchProjection {
    pub(crate) session_id: SessionId,
    pub(crate) revision: OutputRevision,
    pub(crate) size: TerminalSize,
    pub(crate) grid_epoch: u64,
    pub(crate) scroll_origin: u64,
    pub(crate) history_size: usize,
    pub(crate) logical_lines: Arc<[TerminalSearchLogicalLineWitness]>,
}

/// 一条已命中的完整逻辑行及其原结果序号；多命中共享行证据以限制 scrollback 内存。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalSearchLogicalLineWitness {
    pub(crate) rows: Arc<[TerminalSearchRowWitness]>,
    pub(crate) matches: Arc<[(usize, TerminalSearchMatch)]>,
}

/// 搜索时某个物理网格行的绝对坐标与字形、列宽、软换行证据。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalSearchRowWitness {
    pub(crate) absolute_line: i64,
    pub(crate) witness: Arc<TerminalRowWitness>,
}

impl TerminalSearchResult {
    /// 将已提交命中安全投影到当前 viewport，并保留原索引供 UI 继续标识选中项。
    ///
    /// 同代快照直接使用搜索坐标；后续快照按 scroll origin 平移，且要求整条闭合逻辑行的
    /// 每个物理行仍可见并与搜索时的文字、cell span 和 wrap 证据一致。越过 trim、resize、
    /// grid epoch 或证据预算的命中会被隐藏，等待用户显式重新搜索，避免猜测或重扫历史。
    ///
    /// @author kongweiguang
    pub fn project_visible_matches(
        &self,
        snapshot: &RenderSnapshot,
    ) -> Option<Arc<[(usize, TerminalSearchMatch)]>> {
        let projection = self.projection.as_deref()?;
        if snapshot.session_id != projection.session_id
            || snapshot.size != projection.size
            || snapshot.grid_epoch != projection.grid_epoch
        {
            return None;
        }

        if snapshot.revision == projection.revision {
            if snapshot.scroll_origin != projection.scroll_origin
                || snapshot.history_size != projection.history_size
            {
                return None;
            }
            let visible = self
                .matches
                .iter()
                .enumerate()
                .filter(|(_, matched)| search_range_intersects_snapshot(matched, snapshot))
                .map(|(index, matched)| (index, matched.clone()))
                .collect::<Vec<_>>();
            return Some(visible.into());
        }
        if snapshot.revision <= projection.revision {
            return None;
        }

        let mut projected = Vec::new();
        for logical_line in projection.logical_lines.iter() {
            if !search_logical_line_is_closed(logical_line)
                || !search_logical_line_matches_snapshot(logical_line, projection, snapshot)
            {
                continue;
            }
            for (match_index, matched) in logical_line.matches.iter() {
                if self.matches.get(*match_index) != Some(matched) {
                    continue;
                }
                let Some((first, last)) = logical_line.rows.first().zip(logical_line.rows.last())
                else {
                    continue;
                };
                if matched.range.start.line < first.absolute_line
                    || matched.range.end.line > last.absolute_line
                    || matched.range.start.column >= projection.size.columns()
                    || matched.range.end.column > projection.size.columns()
                {
                    continue;
                }
                let Some(range) = map_search_range(matched, projection.scroll_origin, snapshot)
                else {
                    continue;
                };
                projected.push((*match_index, TerminalSearchMatch { range }));
            }
        }
        projected.sort_by_key(|(match_index, _)| *match_index);
        Some(projected.into())
    }
}

/// 只接受搜索时已经闭合的逻辑行；尾部 WRAPLINE 仍可能在后续输出中接入新上下文。
///
/// @author kongweiguang
fn search_logical_line_is_closed(line: &TerminalSearchLogicalLineWitness) -> bool {
    let Some((last, preceding)) = line.rows.split_last() else {
        return false;
    };
    !last.witness.wrapped && preceding.iter().all(|row| row.witness.wrapped)
}

/// 在 viewport 的绝对行坐标中判断半开终端范围是否至少有一个物理行可见。
///
/// @author kongweiguang
fn search_range_intersects_snapshot(
    matched: &TerminalSearchMatch,
    snapshot: &RenderSnapshot,
) -> bool {
    snapshot
        .rows
        .first()
        .zip(snapshot.rows.last())
        .is_some_and(|(first, last)| {
            matched.range.end.line >= first.absolute_line
                && matched.range.start.line <= last.absolute_line
        })
}

/// 把完整逻辑行映射到新 scroll origin，并拒绝缺行、裁剪、重排或任何 cell 证据变化。
///
/// @author kongweiguang
fn search_logical_line_matches_snapshot(
    line: &TerminalSearchLogicalLineWitness,
    projection: &TerminalSearchProjection,
    snapshot: &RenderSnapshot,
) -> bool {
    let mut previous_line = None;
    for source in line.rows.iter() {
        let source_history_start = -(projection.history_size as i128);
        if i128::from(source.absolute_line) < source_history_start
            || i128::from(source.absolute_line) >= i128::from(projection.size.rows())
        {
            return false;
        }
        let Some(mapped_line) = map_search_line(
            source.absolute_line,
            projection.scroll_origin,
            snapshot.scroll_origin,
        ) else {
            return false;
        };
        let history_start = -(snapshot.history_size as i128);
        if i128::from(mapped_line) < history_start
            || i128::from(mapped_line) >= i128::from(snapshot.size.rows())
            || previous_line.is_some_and(|previous: i64| mapped_line != previous.saturating_add(1))
        {
            return false;
        }
        let Some(current) = snapshot_row_for_absolute_line(snapshot, mapped_line) else {
            return false;
        };
        if current.witness().as_ref() != source.witness.as_ref() {
            return false;
        }
        previous_line = Some(mapped_line);
    }
    true
}

/// 通过 snapshot 的 offset 直接定位行，避免每次输出都在线性扫描 viewport 中查找证据。
///
/// @author kongweiguang
fn snapshot_row_for_absolute_line(
    snapshot: &RenderSnapshot,
    absolute_line: i64,
) -> Option<&TerminalRow> {
    let viewport_row = i128::from(absolute_line) + snapshot.scrollback_offset as i128;
    let viewport_row = usize::try_from(viewport_row).ok()?;
    snapshot
        .rows
        .get(viewport_row)
        .filter(|row| row.absolute_line == absolute_line)
}

/// 将搜索命中行按选择 anchor 的累计 scroll-origin 公式映射，且不把被裁掉的坐标夹回网格。
///
/// @author kongweiguang
fn map_search_range(
    matched: &TerminalSearchMatch,
    source_scroll_origin: u64,
    snapshot: &RenderSnapshot,
) -> Option<std::ops::Range<TerminalPoint>> {
    let map_point = |point: TerminalPoint| {
        let line = i128::from(point.line) + i128::from(source_scroll_origin)
            - i128::from(snapshot.scroll_origin);
        i64::try_from(line).ok().map(|line| TerminalPoint {
            line,
            column: point.column,
        })
    };
    let start = map_point(matched.range.start)?;
    let end = map_point(matched.range.end)?;
    let history_start = -(snapshot.history_size as i128);
    let (first_visible, last_visible) = snapshot.rows.first().zip(snapshot.rows.last())?;
    if i128::from(start.line) < history_start
        || i128::from(start.line) >= i128::from(snapshot.size.rows())
        || i128::from(end.line) < history_start
        || i128::from(end.line) >= i128::from(snapshot.size.rows())
        || end.line < first_visible.absolute_line
        || start.line > last_visible.absolute_line
    {
        return None;
    }
    Some(start..end)
}

/// 将任意行按 selection anchor 已验证的 scroll-origin 差值映射到目标网格坐标。
///
/// @author kongweiguang
fn map_search_line(line: i64, source_scroll_origin: u64, target_scroll_origin: u64) -> Option<i64> {
    let mapped =
        i128::from(line) + i128::from(source_scroll_origin) - i128::from(target_scroll_origin);
    i64::try_from(mapped).ok()
}
