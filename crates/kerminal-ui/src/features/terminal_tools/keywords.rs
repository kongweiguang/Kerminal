//! 将已验证的关键词设置映射到终端快照的真实 cell 区间。
//!
//! @author kongweiguang

use std::cell::RefCell;
use std::ops::Range;
use std::sync::Arc;

use kerminal_application::preferences::{
    TerminalKeywordHighlightMatchMode, TerminalKeywordHighlightRule,
    TerminalKeywordHighlightSettings, TerminalKeywordHighlightStyle,
};
use kerminal_domain::identity::SessionId;
use kerminal_terminal::snapshot::{
    RenderSnapshot, TerminalPoint, TerminalRgb, TerminalRow, TerminalTextCellSpan,
};
use regex::{Regex, RegexBuilder};

const MAX_KEYWORD_OVERLAYS: usize = 1_000;
const MAX_LOGICAL_LINE_BYTES: usize = 64 * 1024;
const REGEX_SIZE_LIMIT_BYTES: usize = 1024 * 1024;

/// 关键词覆盖层的已解析样式；Canvas 不猜测设置语义或当前主题颜色。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeywordCellStyle {
    pub foreground: Option<TerminalRgb>,
    pub background: Option<TerminalRgb>,
}

/// 一条关键词命中可跨越多个带样式 run 或软换行终端行。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeywordCellMatch {
    pub range: Range<TerminalPoint>,
    pub style: KeywordCellStyle,
}

#[derive(Clone)]
struct CompiledKeywordRule {
    regex: Regex,
    whole_word: bool,
    style: KeywordCellStyle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CacheKey {
    session_id: SessionId,
    revision: u64,
    scrollback_offset: usize,
    columns: u16,
    row_count: usize,
    first_line: Option<i64>,
    last_line: Option<i64>,
    rule_generation: u64,
}

struct CachedProjection {
    key: CacheKey,
    matches: Arc<[KeywordCellMatch]>,
}

struct FailedCompilation {
    settings: TerminalKeywordHighlightSettings,
    dark_theme: bool,
    message: String,
}

#[derive(Default)]
struct LogicalLine {
    text: String,
    mappings: Vec<CellMapping>,
}

#[derive(Clone)]
struct CellMapping {
    bytes: Range<usize>,
    line: i64,
    columns: Range<u16>,
}

/// 预编译并缓存规则映射，避免每帧为可见终端文本重复构造正则或重算列宽。
pub struct KeywordOverlayProjector {
    settings: Option<TerminalKeywordHighlightSettings>,
    dark_theme: bool,
    rules: Arc<[CompiledKeywordRule]>,
    generation: u64,
    cache: RefCell<Option<CachedProjection>>,
    failed_compilation: Option<FailedCompilation>,
}

impl Default for KeywordOverlayProjector {
    /// 初始状态不预置关键词，等待 Root 注入已加载的用户设置。
    fn default() -> Self {
        Self {
            settings: None,
            dark_theme: true,
            rules: Arc::from([]),
            generation: 0,
            cache: RefCell::new(None),
            failed_compilation: None,
        }
    }
}

impl KeywordOverlayProjector {
    /// 只在配置或主题变化时重编译；重复失败输入复用错误并保留上一套有效投影。
    pub fn update(
        &mut self,
        settings: TerminalKeywordHighlightSettings,
        dark_theme: bool,
    ) -> Result<bool, String> {
        if self.settings.as_ref() == Some(&settings) && self.dark_theme == dark_theme {
            return Ok(false);
        }

        if let Some(failed) = self.failed_compilation.as_ref()
            && failed.settings == settings
            && failed.dark_theme == dark_theme
        {
            return Err(failed.message.clone());
        }

        let compiled = match compile_rules(&settings, dark_theme) {
            Ok(compiled) => compiled,
            Err(message) => {
                self.failed_compilation = Some(FailedCompilation {
                    settings,
                    dark_theme,
                    message: message.clone(),
                });
                return Err(message);
            }
        };
        self.settings = Some(settings);
        self.dark_theme = dark_theme;
        self.rules = Arc::from(compiled);
        self.generation = self.generation.wrapping_add(1);
        *self.cache.borrow_mut() = None;
        self.failed_compilation = None;
        Ok(true)
    }

    /// 只投影当前可见快照；缓存键同时包含会话、引擎 revision 与视口，避免跨 Tab 复用坐标。
    pub fn project(&self, snapshot: &RenderSnapshot) -> Arc<[KeywordCellMatch]> {
        if self.rules.is_empty() || snapshot.size.columns() == 0 {
            return Arc::from([]);
        }
        let key = projection_key(snapshot, self.generation);
        if let Some(cached) = self.cache.borrow().as_ref()
            && cached.key == key
        {
            return cached.matches.clone();
        }

        let matches = project_visible_rows(&self.rules, snapshot.size.columns(), &snapshot.rows);
        *self.cache.borrow_mut() = Some(CachedProjection {
            key,
            matches: matches.clone(),
        });
        matches
    }
}

/// 预编译受限 regex，并将旧版预置色或当前明暗主题的自定义颜色解析为 RGB。
fn compile_rules(
    settings: &TerminalKeywordHighlightSettings,
    dark_theme: bool,
) -> Result<Vec<CompiledKeywordRule>, String> {
    if !settings.enabled {
        return Ok(Vec::new());
    }

    settings
        .rules
        .iter()
        .filter(|rule| rule.enabled)
        .map(|rule| compile_rule(rule, dark_theme))
        .collect()
}

/// 按规则模式编译表达式并绑定主题色；任何规则无效时不提交半套配置。
fn compile_rule(
    rule: &TerminalKeywordHighlightRule,
    dark_theme: bool,
) -> Result<CompiledKeywordRule, String> {
    let source = match rule.match_mode {
        TerminalKeywordHighlightMatchMode::Regex => rule.pattern.clone(),
        TerminalKeywordHighlightMatchMode::Literal
        | TerminalKeywordHighlightMatchMode::WholeWord => regex::escape(&rule.pattern),
    };
    let regex = RegexBuilder::new(&source)
        .case_insensitive(!rule.case_sensitive)
        .size_limit(REGEX_SIZE_LIMIT_BYTES)
        .dfa_size_limit(REGEX_SIZE_LIMIT_BYTES)
        .build()
        .map_err(|_| "终端关键词规则无效".to_owned())?;

    Ok(CompiledKeywordRule {
        regex,
        whole_word: rule.match_mode == TerminalKeywordHighlightMatchMode::WholeWord,
        style: keyword_style(rule, dark_theme),
    })
}

/// 为可见行建立有界逻辑文本，软换行相连而硬换行互相隔离。
fn project_visible_rows(
    rules: &[CompiledKeywordRule],
    columns: u16,
    rows: &[TerminalRow],
) -> Arc<[KeywordCellMatch]> {
    let mut logical = LogicalLine::default();
    let mut matches = Vec::new();

    for row in rows {
        append_row(&mut logical, row, columns);
        if !row.wrapped || logical.text.len() >= MAX_LOGICAL_LINE_BYTES {
            project_logical_line(rules, &logical, &mut matches);
            logical = LogicalLine::default();
            if matches.len() >= MAX_KEYWORD_OVERLAYS {
                break;
            }
        }
    }
    if !logical.text.is_empty() && matches.len() < MAX_KEYWORD_OVERLAYS {
        project_logical_line(rules, &logical, &mut matches);
    }
    Arc::from(matches)
}

/// 把 run 和引擎提供的 UTF-8 到 cell 映射拼成逻辑行，不通过字符数推算宽字列。
fn append_row(logical: &mut LogicalLine, row: &TerminalRow, columns: u16) {
    let mut next_column = 0_u16;
    for run in row.runs.iter() {
        let start_column = run.start_column.min(columns);
        if start_column > next_column {
            append_cell_gap(logical, row.absolute_line, next_column..start_column);
        }
        if logical.text.len() >= MAX_LOGICAL_LINE_BYTES {
            return;
        }
        let text_start = logical.text.len();
        let remaining = MAX_LOGICAL_LINE_BYTES.saturating_sub(text_start);
        let run_end = previous_char_boundary(&run.text, run.text.len().min(remaining));
        logical.text.push_str(&run.text[..run_end]);
        for span in run.cell_spans.iter() {
            append_span_mapping(
                &mut logical.mappings,
                text_start,
                row.absolute_line,
                span,
                run_end,
                columns,
            );
        }
        next_column = run.end_column.max(next_column).min(columns);
        if run_end < run.text.len() {
            return;
        }
    }
}

/// 为 run 间未占用的空 cell 建立逐列映射，保证含空格的规则也能准确着色。
fn append_cell_gap(logical: &mut LogicalLine, line: i64, columns: Range<u16>) {
    for column in columns {
        if logical.text.len() >= MAX_LOGICAL_LINE_BYTES {
            return;
        }
        let start = logical.text.len();
        logical.text.push(' ');
        logical.mappings.push(CellMapping {
            bytes: start..start + 1,
            line,
            columns: column..column.saturating_add(1),
        });
    }
}

/// 接纳引擎给出的有效 grapheme 映射，裁掉视口外列并拒绝不合法 UTF-8 范围。
fn append_span_mapping(
    mappings: &mut Vec<CellMapping>,
    text_start: usize,
    line: i64,
    span: &TerminalTextCellSpan,
    text_len: usize,
    columns: u16,
) {
    if span.byte_range.start >= span.byte_range.end
        || span.byte_range.end > text_len
        || span.column_range.start >= span.column_range.end
    {
        return;
    }
    let start_column = span.column_range.start.min(columns);
    let end_column = span.column_range.end.min(columns);
    if start_column >= end_column {
        return;
    }
    mappings.push(CellMapping {
        bytes: text_start + span.byte_range.start..text_start + span.byte_range.end,
        line,
        columns: start_column..end_column,
    });
}

/// 逐条规则应用首条优先策略，并将命中的字节范围还原为一个或多个行内 cell overlay。
fn project_logical_line(
    rules: &[CompiledKeywordRule],
    logical: &LogicalLine,
    output: &mut Vec<KeywordCellMatch>,
) {
    if logical.text.is_empty() || output.len() >= MAX_KEYWORD_OVERLAYS {
        return;
    }

    let mut claimed: Vec<Range<usize>> = Vec::new();
    for rule in rules {
        for matched in rule.regex.find_iter(&logical.text) {
            if matched.start() == matched.end()
                || rule.whole_word && !whole_word_match(&logical.text, matched.range())
                || claimed
                    .iter()
                    .any(|existing| ranges_overlap(existing, &matched.range()))
            {
                continue;
            }
            let ranges = mapped_cell_ranges(&logical.mappings, matched.range());
            if ranges.is_empty() {
                continue;
            }
            claimed.push(matched.range());
            output.extend(ranges.into_iter().map(|range| KeywordCellMatch {
                range,
                style: rule.style,
            }));
            if output.len() >= MAX_KEYWORD_OVERLAYS {
                return;
            }
        }
    }
}

/// 在相邻 Unicode 字母、数字或下划线中拒绝部分词命中，和用户配置语义保持一致。
fn whole_word_match(text: &str, matched: Range<usize>) -> bool {
    let is_word = |character: char| character.is_alphanumeric() || character == '_';
    let before = text[..matched.start].chars().next_back();
    let after = text[matched.end..].chars().next();
    !before.is_some_and(is_word) && !after.is_some_and(is_word)
}

/// 检查两段 UTF-8 半开区间是否重叠，用于维持配置数组中的首条规则优先级。
fn ranges_overlap(first: &Range<usize>, second: &Range<usize>) -> bool {
    first.start < second.end && second.start < first.end
}

/// 将一次命中拆成按终端行排序的区间，支持颜色 run 边界和 Alacritty 软换行。
fn mapped_cell_ranges(
    mappings: &[CellMapping],
    matched: Range<usize>,
) -> Vec<Range<TerminalPoint>> {
    let mut ranges: Vec<Range<TerminalPoint>> = Vec::new();
    for mapping in mappings
        .iter()
        .filter(|mapping| mapping.bytes.start < matched.end && matched.start < mapping.bytes.end)
    {
        let start = TerminalPoint {
            line: mapping.line,
            column: mapping.columns.start,
        };
        let end = TerminalPoint {
            line: mapping.line,
            column: mapping.columns.end,
        };
        if let Some(previous) = ranges.last_mut()
            && previous.end.line == mapping.line
            && mapping.columns.start <= previous.end.column
        {
            previous.end.column = previous.end.column.max(mapping.columns.end);
        } else {
            ranges.push(start..end);
        }
    }
    ranges
}

/// 解析预置色和对应主题下的自定义十六进制颜色，缺失自定义色安全回退到黄色预置。
fn keyword_style(rule: &TerminalKeywordHighlightRule, dark_theme: bool) -> KeywordCellStyle {
    use TerminalKeywordHighlightStyle as Style;

    if rule.style == Style::Custom
        && let Some(colors) = rule.custom_colors.as_ref()
    {
        let pair = if dark_theme {
            &colors.dark
        } else {
            &colors.light
        };
        return KeywordCellStyle {
            foreground: pair.foreground.as_deref().and_then(parse_hex_color),
            background: pair.background.as_deref().and_then(parse_hex_color),
        };
    }

    let (foreground, background) = match rule.style {
        Style::Red => (
            rgb_value(0xfca5a5, 0x991b1b, dark_theme),
            rgb_value(0x450a0a, 0xfee2e2, dark_theme),
        ),
        Style::Orange => (
            rgb_value(0xfdba74, 0x9a3412, dark_theme),
            rgb_value(0x431407, 0xffedd5, dark_theme),
        ),
        Style::Yellow | Style::Custom => (
            rgb_value(0xfde047, 0x854d0e, dark_theme),
            rgb_value(0x422006, 0xfef9c3, dark_theme),
        ),
        Style::Green => (
            rgb_value(0x86efac, 0x166534, dark_theme),
            rgb_value(0x052e16, 0xdcfce7, dark_theme),
        ),
        Style::Cyan => (
            rgb_value(0x67e8f9, 0x155e75, dark_theme),
            rgb_value(0x083344, 0xcffafe, dark_theme),
        ),
        Style::Blue => (
            rgb_value(0x93c5fd, 0x1e40af, dark_theme),
            rgb_value(0x172554, 0xdbeafe, dark_theme),
        ),
        Style::Purple => (
            rgb_value(0xd8b4fe, 0x6b21a8, dark_theme),
            rgb_value(0x3b0764, 0xf3e8ff, dark_theme),
        ),
        Style::Pink => (
            rgb_value(0xf9a8d4, 0x9d174d, dark_theme),
            rgb_value(0x500724, 0xfce7f3, dark_theme),
        ),
    };
    KeywordCellStyle {
        foreground: Some(TerminalRgb::new(
            ((foreground >> 16) & 0xff) as u8,
            ((foreground >> 8) & 0xff) as u8,
            (foreground & 0xff) as u8,
        )),
        background: Some(TerminalRgb::new(
            ((background >> 16) & 0xff) as u8,
            ((background >> 8) & 0xff) as u8,
            (background & 0xff) as u8,
        )),
    }
}

/// 从 `#RRGGBB` 配置值读取真实颜色；应用设置已校验格式，防御性解析避免 UI 崩溃。
fn parse_hex_color(value: &str) -> Option<TerminalRgb> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let rgb = u32::from_str_radix(hex, 16).ok()?;
    Some(TerminalRgb::new(
        ((rgb >> 16) & 0xff) as u8,
        ((rgb >> 8) & 0xff) as u8,
        (rgb & 0xff) as u8,
    ))
}

/// 依据主题选取预置颜色的深浅色 RGB 编码。
fn rgb_value(dark: u32, light: u32, dark_theme: bool) -> u32 {
    if dark_theme { dark } else { light }
}

/// 将截断位置向前退到合法 UTF-8 边界，避免超长终端行令 String::truncate panic。
fn previous_char_boundary(text: &str, mut offset: usize) -> usize {
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// 用快照可见身份构成缓存键，滚屏、resize、输出和规则更新都会自然失效旧命中。
fn projection_key(snapshot: &RenderSnapshot, rule_generation: u64) -> CacheKey {
    CacheKey {
        session_id: snapshot.session_id.clone(),
        revision: snapshot.revision,
        scrollback_offset: snapshot.scrollback_offset,
        columns: snapshot.size.columns(),
        row_count: snapshot.rows.len(),
        first_line: snapshot.rows.first().map(|row| row.absolute_line),
        last_line: snapshot.rows.last().map(|row| row.absolute_line),
        rule_generation,
    }
}
