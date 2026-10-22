//! 终端外观、输入建议与关键词高亮的持久化设置契约。
//!
//! @author kongweiguang

use std::collections::HashSet;

use regex::Regex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AppError, AppResult};

/// 终端配色 preset；GPUI 渲染器负责把逻辑 preset 映射到当前主题。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TerminalColorScheme {
    #[default]
    Kerminal,
    TokyoNight,
    Solarized,
    Github,
}

/// 终端光标形态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TerminalCursorStyle {
    #[default]
    Block,
    Bar,
    Underline,
}

/// 终端普通文本字重。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TerminalFontWeight {
    #[default]
    Normal,
    Medium,
    Bold,
}

/// 终端右键的明确动作。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TerminalRightClickBehavior {
    None,
    Paste,
    #[default]
    Menu,
}

/// inline suggestion 接受键。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TerminalInlineSuggestionAcceptKey {
    Disabled,
    #[default]
    RightArrow,
}

/// 命令候选的呈现模式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TerminalCommandSuggestionPresentation {
    Off,
    Inline,
    #[default]
    InlineAndMenu,
}

/// 命令建议菜单触发键。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TerminalCommandSuggestionMenuShortcut {
    #[default]
    CtrlSpace,
}

/// 远端建议缓存刷新策略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TerminalCommandSuggestionRemoteRefresh {
    Off,
    #[default]
    Safe,
}

/// inline suggestion 的可独立启停来源。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct TerminalInlineSuggestionProviderSettings {
    pub history: bool,
    pub remote_path: bool,
    pub remote_command: bool,
    pub git: bool,
    pub spec: bool,
}

impl Default for TerminalInlineSuggestionProviderSettings {
    /// 缺字段时维持旧版启用的五类 provider。
    fn default() -> Self {
        Self {
            history: true,
            remote_path: true,
            remote_command: true,
            git: true,
            spec: true,
        }
    }
}

/// 终端命令 inline 与菜单建议的行为参数。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct TerminalInlineSuggestionSettings {
    pub enabled: bool,
    pub accept_key: TerminalInlineSuggestionAcceptKey,
    pub presentation: TerminalCommandSuggestionPresentation,
    pub menu_shortcut: TerminalCommandSuggestionMenuShortcut,
    pub tab_opens_menu: bool,
    pub partial_accept: bool,
    pub remote_refresh: TerminalCommandSuggestionRemoteRefresh,
    pub providers: TerminalInlineSuggestionProviderSettings,
    pub remote_probe_enabled: bool,
    pub audit_retention_days: u32,
    pub feedback_retention_days: u32,
}

impl Default for TerminalInlineSuggestionSettings {
    /// 保持旧版右方向接受、inline+menu 和远端安全刷新默认值。
    fn default() -> Self {
        Self {
            enabled: true,
            accept_key: TerminalInlineSuggestionAcceptKey::RightArrow,
            presentation: TerminalCommandSuggestionPresentation::InlineAndMenu,
            menu_shortcut: TerminalCommandSuggestionMenuShortcut::CtrlSpace,
            tab_opens_menu: false,
            partial_accept: true,
            remote_refresh: TerminalCommandSuggestionRemoteRefresh::Safe,
            providers: TerminalInlineSuggestionProviderSettings::default(),
            remote_probe_enabled: true,
            audit_retention_days: 30,
            feedback_retention_days: 365,
        }
    }
}

impl TerminalInlineSuggestionSettings {
    /// 沿用旧版建议开关的互斥规则，并限制本地诊断保留期避免无界增长。
    pub(super) fn normalized(mut self) -> Self {
        self.audit_retention_days = self.audit_retention_days.clamp(1, 3650);
        self.feedback_retention_days = self.feedback_retention_days.clamp(1, 3650);
        if !self.enabled {
            self.presentation = TerminalCommandSuggestionPresentation::Off;
        }
        if self.presentation == TerminalCommandSuggestionPresentation::Off {
            self.enabled = false;
        }
        if !self.remote_probe_enabled {
            self.remote_refresh = TerminalCommandSuggestionRemoteRefresh::Off;
        }
        if self.remote_refresh == TerminalCommandSuggestionRemoteRefresh::Off {
            self.remote_probe_enabled = false;
        }
        self
    }
}

/// 关键词的匹配模式。
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TerminalKeywordHighlightMatchMode {
    #[default]
    Literal,
    WholeWord,
    Regex,
}

/// 关键词高亮预置色。
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TerminalKeywordHighlightStyle {
    Red,
    Orange,
    #[default]
    Yellow,
    Green,
    Cyan,
    Blue,
    Purple,
    Pink,
    Custom,
}

/// 单主题下可选的自定义前景和背景颜色。
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct TerminalKeywordHighlightColorPair {
    pub foreground: Option<String>,
    pub background: Option<String>,
}

/// 浅色和深色主题分别使用的自定义高亮颜色。
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct TerminalKeywordHighlightCustomColors {
    pub light: TerminalKeywordHighlightColorPair,
    pub dark: TerminalKeywordHighlightColorPair,
}

/// 单条关键词规则；数组顺序决定重叠匹配时的优先级。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct TerminalKeywordHighlightRule {
    pub id: String,
    pub enabled: bool,
    pub pattern: String,
    pub match_mode: TerminalKeywordHighlightMatchMode,
    pub case_sensitive: bool,
    pub note: String,
    pub style: TerminalKeywordHighlightStyle,
    pub custom_colors: Option<TerminalKeywordHighlightCustomColors>,
}

impl Default for TerminalKeywordHighlightRule {
    /// 只为 TOML 反序列化提供字段缺省；无 pattern 的规则仍会被 validated 拒绝。
    fn default() -> Self {
        Self {
            id: String::new(),
            enabled: true,
            pattern: String::new(),
            match_mode: TerminalKeywordHighlightMatchMode::Literal,
            case_sensitive: false,
            note: String::new(),
            style: TerminalKeywordHighlightStyle::Yellow,
            custom_colors: None,
        }
    }
}

impl TerminalKeywordHighlightRule {
    /// 在设置用例边界创建带稳定 UUID 的新规则，避免 UI 自行定义持久身份。
    pub fn new(pattern: impl Into<String>, note: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            enabled: true,
            pattern: pattern.into(),
            match_mode: TerminalKeywordHighlightMatchMode::Literal,
            case_sensitive: false,
            note: note.into(),
            style: TerminalKeywordHighlightStyle::Yellow,
            custom_colors: None,
        }
    }
}

/// 全局终端关键词高亮和有序规则集合。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct TerminalKeywordHighlightSettings {
    pub enabled: bool,
    pub rules: Vec<TerminalKeywordHighlightRule>,
}

impl Default for TerminalKeywordHighlightSettings {
    /// 高亮默认可用，但不额外预置会改变现有终端文本颜色的规则。
    fn default() -> Self {
        Self {
            enabled: true,
            rules: Vec::new(),
        }
    }
}

impl TerminalKeywordHighlightSettings {
    /// 验证规则身份、长度、regex 运行时边界与主题色，拒绝零宽和回溯特性。
    pub(super) fn validated(mut self) -> AppResult<Self> {
        if self.rules.len() > 64 {
            return Err(AppError::invalid("关键词高亮规则不能超过 64 条"));
        }
        let mut ids = HashSet::with_capacity(self.rules.len());
        for rule in &mut self.rules {
            rule.id = rule.id.trim().to_owned();
            rule.pattern = rule.pattern.trim().to_owned();
            rule.note = rule.note.trim().to_owned();
            if rule.id.is_empty() || !ids.insert(rule.id.clone()) {
                return Err(AppError::invalid("关键词规则 ID 不能为空且不能重复"));
            }
            if rule.pattern.is_empty() || rule.pattern.chars().count() > 256 {
                return Err(AppError::invalid("关键词内容不能为空或超过 256 个字符"));
            }
            if rule.note.chars().count() > 160 {
                return Err(AppError::invalid("关键词备注不能超过 160 个字符"));
            }
            if rule.match_mode == TerminalKeywordHighlightMatchMode::Regex {
                validate_keyword_regex(&rule.pattern)?;
            }
            if let Some(colors) = rule.custom_colors.as_mut() {
                normalize_color_pair(&mut colors.light)?;
                normalize_color_pair(&mut colors.dark)?;
            }
            if rule.style == TerminalKeywordHighlightStyle::Custom
                && rule.custom_colors.as_ref().is_none_or(|colors| {
                    !has_custom_color(&colors.light) || !has_custom_color(&colors.dark)
                })
            {
                return Err(AppError::invalid(
                    "自定义关键词颜色需要分别配置浅色和深色主题",
                ));
            }
        }
        Ok(self)
    }
}

/// 终端关键视觉、输入行为与显示开关，rendererType 刻意不建模以保留旧文件字段但不暴露失效选项。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct TerminalSettings {
    pub auto_reconnect: bool,
    pub color_scheme: TerminalColorScheme,
    pub light_color_scheme: TerminalColorScheme,
    pub dark_color_scheme: TerminalColorScheme,
    pub font_family: String,
    pub font_size: u16,
    pub font_weight: TerminalFontWeight,
    pub mac_option_is_meta: bool,
    pub line_height: f64,
    pub cursor_style: TerminalCursorStyle,
    pub cursor_blink: bool,
    pub right_click_behavior: TerminalRightClickBehavior,
    pub selection_copy: bool,
    pub show_command_block_rail: bool,
    pub show_tab_numbers: bool,
    pub confirm_close_tab: bool,
    pub inline_suggestion: TerminalInlineSuggestionSettings,
    pub keyword_highlights: TerminalKeywordHighlightSettings,
    pub scrollback: u32,
}

impl Default for TerminalSettings {
    /// 与现行 settings.toml 的终端默认值一致，避免初次读取改变用户体验。
    fn default() -> Self {
        Self {
            auto_reconnect: true,
            color_scheme: TerminalColorScheme::Kerminal,
            light_color_scheme: TerminalColorScheme::Kerminal,
            dark_color_scheme: TerminalColorScheme::Kerminal,
            font_family: r#""JetBrains Mono", "SF Mono", "Cascadia Code", Consolas, monospace"#
                .to_owned(),
            font_size: 15,
            font_weight: TerminalFontWeight::Normal,
            mac_option_is_meta: false,
            line_height: 1.35,
            cursor_style: TerminalCursorStyle::Block,
            cursor_blink: true,
            right_click_behavior: TerminalRightClickBehavior::Menu,
            selection_copy: false,
            show_command_block_rail: true,
            show_tab_numbers: false,
            confirm_close_tab: true,
            inline_suggestion: TerminalInlineSuggestionSettings::default(),
            keyword_highlights: TerminalKeywordHighlightSettings::default(),
            scrollback: 5000,
        }
    }
}

impl TerminalSettings {
    /// 校验高亮规则并收敛建议数据保留期，避免 UI 与运行态读到不同范围。
    pub(super) fn validated(mut self) -> AppResult<Self> {
        self.inline_suggestion = self.inline_suggestion.normalized();
        self.keyword_highlights = self.keyword_highlights.validated()?;
        Ok(self)
    }
}

/// 拒绝高成本回溯特征与零宽匹配，终端输出路径只接受有限时间 regex 子集。
fn validate_keyword_regex(pattern: &str) -> AppResult<()> {
    if pattern.contains("(?=")
        || pattern.contains("(?!")
        || pattern.contains("(?<=")
        || pattern.contains("(?<!")
        || pattern.contains("\\1")
        || pattern.contains("\\2")
        || pattern.contains("\\k")
    {
        return Err(AppError::invalid("关键词正则不支持查找断言或回溯引用"));
    }
    let regex = Regex::new(pattern).map_err(|_| AppError::invalid("关键词正则格式无效"))?;
    const PROBES: [&str; 6] = ["", "a", "0", " ", "中", "ERROR"];
    if PROBES.iter().any(|probe| {
        regex
            .find_iter(probe)
            .any(|matched| matched.start() == matched.end())
    }) {
        return Err(AppError::invalid("关键词正则不能匹配空文本"));
    }
    Ok(())
}

/// 规范化主题颜色并拒绝无法映射成 GPUI 颜色的配置。
fn normalize_color_pair(pair: &mut TerminalKeywordHighlightColorPair) -> AppResult<()> {
    normalize_hex_color(&mut pair.foreground)?;
    normalize_hex_color(&mut pair.background)
}

/// 保证非空颜色符合 #RRGGBB，空白值被当作未覆盖颜色。
fn normalize_hex_color(color: &mut Option<String>) -> AppResult<()> {
    let Some(value) = color.as_mut() else {
        return Ok(());
    };
    *value = value.trim().to_ascii_uppercase();
    if value.is_empty() {
        *color = None;
        return Ok(());
    }
    if value.len() != 7
        || !value.starts_with('#')
        || !value[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(AppError::invalid("关键词颜色必须使用 #RRGGBB"));
    }
    Ok(())
}

/// 自定义主题只有提供前景色或背景色时才有可见效果。
fn has_custom_color(pair: &TerminalKeywordHighlightColorPair) -> bool {
    pair.foreground.is_some() || pair.background.is_some()
}
