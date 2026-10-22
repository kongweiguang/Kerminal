//! typed 偏好快照的即时表单校验，不解析 TOML 或访问持久化端口。
//!
//! @author kongweiguang

use std::collections::BTreeMap;

use crate::keybindings::{
    CompiledKeybindings, KeybindingDiagnosticKind, KeybindingPlatform as RuntimeKeybindingPlatform,
};
use kerminal_application::preferences::{KeybindingPreference, ProfileDraft, SettingsSnapshot};

/// 快捷键平台选项；Linux 复用旧配置中的 Windows/Linux 字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardPlatform {
    /// Windows 使用 Windows 主绑定和可选备用绑定。
    Windows,
    /// Linux 独立读取 linux_binding，并对旧条目使用历史回退值。
    Linux,
    /// macOS 使用 mac_binding。
    Mac,
}

/// 可在控件旁展示的偏好校验问题。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsIssue {
    /// 问题类别供 UI 选择字段和呈现语气。
    pub kind: SettingsIssueKind,
    /// 涉及的 action 标识；字段错误时为空。
    pub actions: Vec<String>,
    /// 面向当前界面语言的简短原因。
    pub message: String,
}

impl SettingsIssue {
    /// 未知 action 是兼容性提示，不能阻止保存其它设置或丢弃未来版本字段。
    ///
    /// @author kongweiguang
    pub const fn blocks_save(&self) -> bool {
        !matches!(self.kind, SettingsIssueKind::UnknownKeybindingAction)
    }
}

/// 只保留用户需要修正的语义分类，不把仓储错误带进表单状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsIssueKind {
    /// 字体名为空或超出现行文件契约限制。
    InvalidFontFamily,
    /// 字号不在当前允许范围。
    InvalidFontSize,
    /// 背景图路径超过文件契约限制。
    InvalidBackgroundPath,
    /// 背景材质透明度超出可交互范围。
    InvalidWindowOpacity,
    /// 终端行高不在当前允许范围。
    InvalidLineHeight,
    /// scrollback 超出运行时内存边界。
    InvalidScrollback,
    /// 快捷键语法无法表示单一键和修饰键组合。
    InvalidKeybinding,
    /// 当前平台下两个可同时生效动作占用了同一组合。
    KeybindingCollision,
    /// 当前版本不执行该 action，但保留原配置以兼容未来版本。
    UnknownKeybindingAction,
    /// 两条配置使用重复 action ID，保存无法无歧义映射。
    DuplicateAction,
    /// 关键词规则字段违反长度、唯一性或主题颜色约束。
    InvalidKeywordRule,
}

/// Profile 校验问题；保持表单字段定位与应用层配置错误分离。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileIssue {
    /// 问题类型供 UI 将焦点移到对应字段。
    pub kind: ProfileIssueKind,
    /// 面向当前界面语言的简短原因。
    pub message: String,
}

/// Profile 输入的最小结构约束。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileIssueKind {
    /// profile 名称不能为空或过长。
    InvalidName,
    /// shell 不能为空或过长。
    InvalidShell,
    /// 参数、工作目录或环境变量包含不可表示的输入。
    InvalidProcessValue,
    /// 环境变量条数或字段长度超出应用用例约束。
    InvalidEnvironmentSize,
}

/// 设置页草稿直接保存完整 application snapshot，未展示字段也会随 CAS 原样回写。
pub type SettingsDraft = SettingsSnapshot;

/// 快捷键表单直接编辑应用投影，不复制 action 元数据和扩展字段语义。
pub type KeybindingDraft = KeybindingPreference;

/// 校验完整 typed snapshot；快捷键使用运行时同一 GPUI parser，保留未知 action 警告。
///
/// @author kongweiguang
pub fn validate_settings(draft: &SettingsDraft, platform: KeyboardPlatform) -> Vec<SettingsIssue> {
    let mut issues = Vec::new();
    let font_family = draft.terminal.font_family.trim();
    if font_family.is_empty() || font_family.len() > 160 {
        issues.push(SettingsIssue {
            kind: SettingsIssueKind::InvalidFontFamily,
            actions: Vec::new(),
            message: validation_text(
                "终端字体不能为空，且长度不能超过 160 字节。",
                "Terminal font family is required and must be at most 160 bytes.",
            ),
        });
    }
    if !(10..=24).contains(&draft.terminal.font_size) {
        issues.push(SettingsIssue {
            kind: SettingsIssueKind::InvalidFontSize,
            actions: Vec::new(),
            message: validation_text(
                "终端字号需要在 10 到 24 之间。",
                "Terminal font size must be between 10 and 24.",
            ),
        });
    }
    if draft.background_image_path.trim().len() > 1024 {
        issues.push(SettingsIssue {
            kind: SettingsIssueKind::InvalidBackgroundPath,
            actions: Vec::new(),
            message: validation_text(
                "背景图路径不能超过 1024 个字符。",
                "Background image path must be at most 1,024 characters.",
            ),
        });
    }
    if !(35..=100).contains(&draft.window_opacity) {
        issues.push(SettingsIssue {
            kind: SettingsIssueKind::InvalidWindowOpacity,
            actions: Vec::new(),
            message: validation_text(
                "背景材质透明度需要在 35 到 100 之间。",
                "Window material opacity must be between 35 and 100.",
            ),
        });
    }
    if !(1.0..=1.8).contains(&draft.terminal.line_height) {
        issues.push(SettingsIssue {
            kind: SettingsIssueKind::InvalidLineHeight,
            actions: Vec::new(),
            message: validation_text(
                "终端行高需要在 1.0 到 1.8 之间。",
                "Terminal line height must be between 1.0 and 1.8.",
            ),
        });
    }
    if !(1_000..=50_000).contains(&draft.terminal.scrollback) {
        issues.push(SettingsIssue {
            kind: SettingsIssueKind::InvalidScrollback,
            actions: Vec::new(),
            message: validation_text(
                "滚屏缓冲需要在 1000 到 50000 行之间。",
                "Scrollback must be between 1,000 and 50,000 lines.",
            ),
        });
    }
    let rules = &draft.terminal.keyword_highlights.rules;
    if rules.len() > 64 {
        issues.push(keyword_issue(
            "关键词高亮规则最多保存 64 条。",
            "At most 64 keyword highlight rules can be saved.",
        ));
    }
    let mut keyword_ids = BTreeMap::new();
    for rule in rules {
        if rule.id.trim().is_empty() || keyword_ids.insert(&rule.id, ()).is_some() {
            issues.push(keyword_issue(
                "关键词规则 ID 不能为空且不能重复。",
                "Keyword rule IDs must be non-empty and unique.",
            ));
        }
        if rule.pattern.trim().is_empty() || rule.pattern.chars().count() > 256 {
            issues.push(keyword_issue(
                "关键词不能为空，且最多 256 个字符。",
                "Keyword pattern is required and must be at most 256 characters.",
            ));
        }
        if rule.note.chars().count() > 160 {
            issues.push(keyword_issue(
                "关键词备注最多 160 个字符。",
                "Keyword note must be at most 160 characters.",
            ));
        }
        if let Some(colors) = &rule.custom_colors {
            let valid = [&colors.light, &colors.dark].into_iter().all(|pair| {
                [&pair.foreground, &pair.background]
                    .into_iter()
                    .flatten()
                    .all(|color| is_hex_color(color))
            });
            if !valid {
                issues.push(keyword_issue(
                    "关键词颜色必须使用 #RRGGBB 格式。",
                    "Keyword colors must use #RRGGBB format.",
                ));
            }
        }
        if rule.style == kerminal_application::preferences::TerminalKeywordHighlightStyle::Custom
            && rule.custom_colors.as_ref().is_none_or(|colors| {
                !has_keyword_color(&colors.light) || !has_keyword_color(&colors.dark)
            })
        {
            issues.push(keyword_issue(
                "自定义关键词颜色需要为浅色和深色主题各设置一种颜色。",
                "Custom keyword colors require at least one color for each of the light and dark themes.",
            ));
        }
    }

    let mut seen_actions = BTreeMap::<&str, usize>::new();
    for keybinding in &draft.keybindings {
        *seen_actions.entry(&keybinding.action).or_default() += 1;
        if keybinding.binding.len() > 128
            || keybinding.windows_binding.len() > 128
            || keybinding.mac_binding.len() > 128
            || keybinding
                .linux_binding
                .as_ref()
                .is_some_and(|binding| binding.len() > 128)
            || keybinding
                .windows_alternate_binding
                .as_ref()
                .is_some_and(|binding| binding.len() > 128)
        {
            issues.push(SettingsIssue {
                kind: SettingsIssueKind::InvalidKeybinding,
                actions: vec![keybinding.action.clone()],
                message: validation_text(
                    &format!("动作 {} 的快捷键长度超出限制。", keybinding.action),
                    &format!(
                        "Shortcut for action '{}' exceeds the length limit.",
                        keybinding.action
                    ),
                )
                .to_string(),
            });
        }
    }
    for (action, count) in seen_actions {
        if count > 1 {
            issues.push(SettingsIssue {
                kind: SettingsIssueKind::DuplicateAction,
                actions: vec![action.to_string()],
                message: validation_text(
                    "快捷键列表中存在重复动作标识。",
                    "The shortcut list contains duplicate action IDs.",
                ),
            });
        }
    }
    let runtime_platform = match platform {
        KeyboardPlatform::Windows => RuntimeKeybindingPlatform::Windows,
        KeyboardPlatform::Linux => RuntimeKeybindingPlatform::Linux,
        KeyboardPlatform::Mac => RuntimeKeybindingPlatform::Mac,
    };
    let compiled = CompiledKeybindings::compile(&draft.keybindings, runtime_platform);
    for diagnostic in compiled.diagnostics() {
        let (kind, zh_cn, en_us) = match diagnostic.kind {
            KeybindingDiagnosticKind::UnknownAction => (
                SettingsIssueKind::UnknownKeybindingAction,
                format!(
                    "未知快捷键动作 {}（{}）已保留，不会执行。",
                    diagnostic.action, diagnostic.binding
                ),
                format!(
                    "Unknown shortcut action '{}' ({}) is preserved but will not run.",
                    diagnostic.action, diagnostic.binding
                ),
            ),
            KeybindingDiagnosticKind::InvalidBinding => (
                SettingsIssueKind::InvalidKeybinding,
                format!(
                    "动作 {} 的绑定 {} 无效：{}",
                    diagnostic.action, diagnostic.binding, diagnostic.message
                ),
                format!(
                    "Binding '{}' for action '{}' is invalid: {}",
                    diagnostic.binding, diagnostic.action, diagnostic.message
                ),
            ),
            KeybindingDiagnosticKind::Conflict => (
                SettingsIssueKind::KeybindingCollision,
                format!(
                    "动作 {} 的绑定 {} 与 {} 冲突。",
                    diagnostic.action,
                    diagnostic.binding,
                    diagnostic.conflicts_with.as_deref().unwrap_or("未知动作")
                ),
                format!(
                    "Binding '{}' for action '{}' conflicts with '{}'.",
                    diagnostic.binding,
                    diagnostic.action,
                    diagnostic
                        .conflicts_with
                        .as_deref()
                        .unwrap_or("unknown action")
                ),
            ),
        };
        let mut actions = vec![diagnostic.action.clone()];
        if let Some(conflict) = &diagnostic.conflicts_with {
            actions.push(conflict.clone());
        }
        issues.push(SettingsIssue {
            kind,
            actions,
            message: validation_text(&zh_cn, &en_us),
        });
    }
    issues
}

/// 关键词规则校验只在表单层解释可即时反馈的结构约束，复杂正则由 application 校验。
fn keyword_issue(zh_cn: &str, en_us: &str) -> SettingsIssue {
    SettingsIssue {
        kind: SettingsIssueKind::InvalidKeywordRule,
        actions: Vec::new(),
        message: validation_text(zh_cn, en_us),
    }
}

/// 先判断 RGB 文本格式，具体主题 palette 转换留在终端渲染边界。
fn is_hex_color(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// 自定义主题至少需要一种有效覆盖颜色。
fn has_keyword_color(
    pair: &kerminal_application::preferences::TerminalKeywordHighlightColorPair,
) -> bool {
    pair.foreground
        .as_ref()
        .is_some_and(|value| is_hex_color(value))
        || pair
            .background
            .as_ref()
            .is_some_and(|value| is_hex_color(value))
}

/// 校验 Profile 的启动输入边界；真实路径存在性仍由启动用例反馈。
pub fn validate_profile(draft: &ProfileDraft) -> Vec<ProfileIssue> {
    let mut issues = Vec::new();
    if draft.name.trim().is_empty() || draft.name.len() > 256 {
        issues.push(ProfileIssue {
            kind: ProfileIssueKind::InvalidName,
            message: validation_text(
                "Profile 名称不能为空，且长度不能超过 256 字节。",
                "Profile name is required and must be at most 256 bytes.",
            ),
        });
    }
    if draft.shell.trim().is_empty() || draft.shell.len() > 1024 {
        issues.push(ProfileIssue {
            kind: ProfileIssueKind::InvalidShell,
            message: validation_text(
                "Shell 路径不能为空，且长度不能超过 1024 字节。",
                "Shell path is required and must be at most 1,024 bytes.",
            ),
        });
    }
    let process_value_invalid = draft.shell.contains('\0')
        || draft.cwd.as_deref().is_some_and(|cwd| cwd.contains('\0'))
        || draft.args.iter().any(|argument| argument.contains('\0'))
        || draft.env.iter().any(|(name, value)| {
            name.is_empty() || name.contains('=') || name.contains('\0') || value.contains('\0')
        });
    if process_value_invalid {
        issues.push(ProfileIssue {
            kind: ProfileIssueKind::InvalidProcessValue,
            message: validation_text("命令、参数、工作目录或环境变量包含不可表示的字符。", "Command, arguments, working directory, or environment variables contain unsupported characters."),
        });
    }
    if draft.cwd.as_ref().is_some_and(|cwd| cwd.len() > 4096)
        || draft.args.iter().any(|argument| argument.len() > 16 * 1024)
        || draft.env.len() > 256
        || draft
            .env
            .iter()
            .any(|(key, value)| key.len() > 1024 || value.len() > 16 * 1024)
    {
        issues.push(ProfileIssue {
            kind: ProfileIssueKind::InvalidEnvironmentSize,
            message: validation_text(
                "Profile 参数或环境变量超出应用用例限制。",
                "Profile arguments or environment variables exceed application limits.",
            ),
        });
    }
    issues
}

/// 表单自有校验消息随全局语言切换，配置值与 action ID 保持原样。
fn validation_text(zh_cn: &str, en_us: &str) -> String {
    crate::locale::text(zh_cn, en_us).to_string()
}
