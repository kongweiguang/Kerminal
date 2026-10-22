//! 将持久化快捷键编译为 GPUI keymap，并提供可热替换的 typed dispatch 快照。
//!
//! @author kongweiguang

use std::rc::Rc;

use gpui_kit::{
    DummyKeyboardMapper, KeyBinding, KeyBindingContextPredicate, KeyContext, Keymap, Keystroke,
};
use kerminal_application::preferences::{KeybindingPreference, KeybindingScope};

/// GPUI 视图树中快捷键需要识别的上下文标记。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeybindingContext {
    /// 应用根上下文，承载全局快捷键。
    App,
    /// 工作区上下文，承载终端 Tab 和布局快捷键。
    Workspace,
    /// 终端上下文，承载当前终端或分屏快捷键。
    Terminal,
    /// 可编辑文本控件上下文，用于屏蔽应用快捷键。
    TextInput,
}

impl KeybindingContext {
    /// 返回 GPUI context predicate 使用的稳定标识符。
    pub const fn as_gpui_identifier(self) -> &'static str {
        match self {
            Self::App => "KerminalApp",
            Self::Workspace => "KerminalWorkspace",
            Self::Terminal => "KerminalTerminal",
            Self::TextInput => "KerminalTextInput",
        }
    }
}

/// Kerminal 当前支持的内置快捷键动作；未知 action 只保留在配置 DTO 并产生诊断。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeybindingAction {
    /// 打开设置面板。
    SettingsOpen,
    /// 打开快捷键设置页。
    SettingsKeybindings,
    /// 聚焦终端工作区。
    TerminalFocus,
    /// 搜索当前终端的可见输出与 scrollback。
    TerminalSearch,
    /// 将系统剪贴板文本经固定终端会话的粘贴通道输入。
    TerminalPaste,
    /// 新建终端 Tab。
    TerminalNewTab,
    /// 关闭当前终端 Tab。
    TerminalCloseTab,
    /// 关闭当前终端 Pane。
    TerminalClosePane,
    /// 创建左右分屏。
    TerminalSplitHorizontal,
    /// 创建上下分屏。
    TerminalSplitVertical,
    /// 切换到工作区上一个可激活 Tab，保留既有配置动作 ID。
    TerminalPreviousTab,
    /// 切换到工作区下一个可激活 Tab，保留既有配置动作 ID。
    TerminalNextTab,
    /// 打开 Agent Launcher。
    ToolAgentLauncher,
    /// 打开系统信息工具。
    ToolSystem,
    /// 打开 SFTP 工具。
    ToolSftp,
    /// 打开端口转发工具。
    ToolPorts,
    /// 打开脚本片段工具。
    ToolSnippets,
    /// 打开日志工具。
    ToolLogs,
    /// 按当前工作区状态查找标签、文件、主机和命令。
    WorkspaceQuickOpen,
    /// 打开可搜索的内置动作面板。
    WorkspaceCommandPalette,
    /// 经 Root 的关闭门禁请求退出应用。
    ApplicationQuit,
}

impl KeybindingAction {
    /// 返回由 Root 实际分发的完整 action 集，供命令面板复用而不维护第二份动作目录。
    pub const ALL: [Self; 21] = [
        Self::SettingsOpen,
        Self::SettingsKeybindings,
        Self::TerminalFocus,
        Self::TerminalSearch,
        Self::TerminalPaste,
        Self::TerminalNewTab,
        Self::TerminalCloseTab,
        Self::TerminalClosePane,
        Self::TerminalSplitHorizontal,
        Self::TerminalSplitVertical,
        Self::TerminalPreviousTab,
        Self::TerminalNextTab,
        Self::ToolAgentLauncher,
        Self::ToolSystem,
        Self::ToolSftp,
        Self::ToolPorts,
        Self::ToolSnippets,
        Self::ToolLogs,
        Self::WorkspaceQuickOpen,
        Self::WorkspaceCommandPalette,
        Self::ApplicationQuit,
    ];

    /// 将冻结的配置 action ID 映射为可安全分发的内置动作。
    ///
    /// @author kongweiguang
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "application.quit" => Some(Self::ApplicationQuit),
            "settings.open" => Some(Self::SettingsOpen),
            "settings.keybindings" => Some(Self::SettingsKeybindings),
            "terminal.focus" => Some(Self::TerminalFocus),
            "terminal.search" => Some(Self::TerminalSearch),
            "terminal.paste" => Some(Self::TerminalPaste),
            "terminal.newTab" => Some(Self::TerminalNewTab),
            "terminal.closeTab" => Some(Self::TerminalCloseTab),
            "terminal.closePane" => Some(Self::TerminalClosePane),
            "terminal.splitHorizontal" => Some(Self::TerminalSplitHorizontal),
            "terminal.splitVertical" => Some(Self::TerminalSplitVertical),
            "terminal.previousTab" => Some(Self::TerminalPreviousTab),
            "terminal.nextTab" => Some(Self::TerminalNextTab),
            "tool.agentLauncher" => Some(Self::ToolAgentLauncher),
            "tool.system" => Some(Self::ToolSystem),
            "tool.sftp" => Some(Self::ToolSftp),
            "tool.ports" => Some(Self::ToolPorts),
            "tool.snippets" => Some(Self::ToolSnippets),
            "tool.logs" => Some(Self::ToolLogs),
            "workspace.quickOpen" => Some(Self::WorkspaceQuickOpen),
            "workspace.commandPalette" => Some(Self::WorkspaceCommandPalette),
            _ => None,
        }
    }

    /// 返回与 v0.3.45 配置文件一致的 action ID。
    ///
    /// @author kongweiguang
    pub const fn id(self) -> &'static str {
        match self {
            Self::ApplicationQuit => "application.quit",
            Self::SettingsOpen => "settings.open",
            Self::SettingsKeybindings => "settings.keybindings",
            Self::TerminalFocus => "terminal.focus",
            Self::TerminalSearch => "terminal.search",
            Self::TerminalPaste => "terminal.paste",
            Self::TerminalNewTab => "terminal.newTab",
            Self::TerminalCloseTab => "terminal.closeTab",
            Self::TerminalClosePane => "terminal.closePane",
            Self::TerminalSplitHorizontal => "terminal.splitHorizontal",
            Self::TerminalSplitVertical => "terminal.splitVertical",
            Self::TerminalPreviousTab => "terminal.previousTab",
            Self::TerminalNextTab => "terminal.nextTab",
            Self::ToolAgentLauncher => "tool.agentLauncher",
            Self::ToolSystem => "tool.system",
            Self::ToolSftp => "tool.sftp",
            Self::ToolPorts => "tool.ports",
            Self::ToolSnippets => "tool.snippets",
            Self::ToolLogs => "tool.logs",
            Self::WorkspaceQuickOpen => "workspace.quickOpen",
            Self::WorkspaceCommandPalette => "workspace.commandPalette",
        }
    }
}

/// 平台字段明确区分三类桌面，同时保留旧配置未提供 Linux 字段时的回退规则。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeybindingPlatform {
    /// Windows 专用绑定字段。
    Windows,
    /// Linux 专用绑定字段；旧条目仍回退到 Windows 字段。
    Linux,
    /// macOS 专用覆盖字段。
    Mac,
}

impl KeybindingPlatform {
    /// 根据编译目标选择有效平台字段，旧配置兼容由 `effective_binding` 集中处理。
    ///
    /// @author kongweiguang
    pub const fn current() -> Self {
        #[cfg(target_os = "windows")]
        {
            Self::Windows
        }
        #[cfg(target_os = "macos")]
        {
            Self::Mac
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            Self::Linux
        }
    }
}

/// 快捷键加载过程中的可见诊断类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeybindingDiagnosticKind {
    /// 配置包含当前应用不执行的未知 action。
    UnknownAction,
    /// GPUI parser 拒绝了配置中的按键表达式。
    InvalidBinding,
    /// 相同按键序列在重叠作用范围中重复出现。
    Conflict,
}

/// 单条配置绑定的安全诊断，不包含键盘事件正文或其它敏感数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingDiagnostic {
    /// 诊断类别。
    pub kind: KeybindingDiagnosticKind,
    /// 配置里的 action ID，未知 ID 原样保留以便显示。
    pub action: String,
    /// 经平台选择后的绑定文本。
    pub binding: String,
    /// 冲突诊断指向的另一 action ID。
    pub conflicts_with: Option<String>,
    /// 面向设置 UI 的简短说明。
    pub message: String,
}

/// GPUI 命中的 Kerminal 动作及是否仍有更长按键序列可以匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeybindingResolution {
    /// 当前输入应派发的动作；无匹配、文本焦点或 IME 输入时为空。
    pub action: Option<KeybindingAction>,
    /// 当前按键是有效组合序列的前缀，GPUI 仍需等待后续按键。
    pub pending: bool,
}

/// 统一 GPUI action marker；具体业务动作以不可变 typed enum 字段交给 Root 分发。
#[derive(Debug, Clone, PartialEq, gpui_kit::Action)]
#[action(name = "kerminal.shortcut", no_json, no_register)]
pub struct KerminalShortcutAction {
    /// 已通过内置 action ID 白名单解析的业务动作。
    pub action: KeybindingAction,
}

/// 一次完整解析后的 keymap；UI 可整体替换快照以实现设置热更新。
pub struct CompiledKeybindings {
    keymap: Keymap,
    diagnostics: Vec<KeybindingDiagnostic>,
}

impl CompiledKeybindings {
    /// 使用 GPUI parser 构造平台快照；Windows alternate 与主绑定共享 action 和冲突校验。
    ///
    /// @author kongweiguang
    pub fn compile(preferences: &[KeybindingPreference], platform: KeybindingPlatform) -> Self {
        let mut bindings = Vec::new();
        let mut diagnostics = Vec::new();
        let mut parsed: Vec<(String, String, KeybindingScope, String)> = Vec::new();

        for preference in preferences {
            let selected_bindings = effective_bindings(preference, platform);
            let Some(action) = KeybindingAction::from_id(&preference.action) else {
                diagnostics.push(KeybindingDiagnostic {
                    kind: KeybindingDiagnosticKind::UnknownAction,
                    action: preference.action.clone(),
                    binding: selected_bindings.join(" / "),
                    conflicts_with: None,
                    message: "未知快捷键动作已保留，但不会执行。".to_owned(),
                });
                continue;
            };

            for selected in selected_bindings {
                if selected.trim().is_empty() {
                    continue;
                }

                let normalized = normalize_legacy_binding(selected);
                let predicate =
                    match KeyBindingContextPredicate::parse(context_predicate(preference.scope)) {
                        Ok(predicate) => Rc::new(predicate),
                        Err(error) => {
                            diagnostics.push(KeybindingDiagnostic {
                                kind: KeybindingDiagnosticKind::InvalidBinding,
                                action: preference.action.clone(),
                                binding: selected.to_owned(),
                                conflicts_with: None,
                                message: format!("快捷键作用范围无效：{error}"),
                            });
                            continue;
                        }
                    };
                let binding = match KeyBinding::load(
                    &normalized,
                    Box::new(KerminalShortcutAction { action }),
                    Some(predicate),
                    false,
                    None,
                    &DummyKeyboardMapper,
                ) {
                    Ok(binding) => binding,
                    Err(error) => {
                        diagnostics.push(KeybindingDiagnostic {
                            kind: KeybindingDiagnosticKind::InvalidBinding,
                            action: preference.action.clone(),
                            binding: selected.to_owned(),
                            conflicts_with: None,
                            message: format!("快捷键格式无法解析：{error}"),
                        });
                        continue;
                    }
                };
                let canonical = binding
                    .keystrokes()
                    .iter()
                    .map(|stroke| stroke.unparse())
                    .collect::<Vec<_>>()
                    .join(" ");

                if parsed
                    .iter()
                    .any(|(previous_action, _, _, previous_sequence)| {
                        previous_action == &preference.action && previous_sequence == &canonical
                    })
                {
                    continue;
                }

                for (previous_action, previous_binding, previous_scope, previous_sequence) in
                    &parsed
                {
                    if previous_sequence == &canonical
                        && scopes_overlap(*previous_scope, preference.scope)
                    {
                        diagnostics.push(KeybindingDiagnostic {
                            kind: KeybindingDiagnosticKind::Conflict,
                            action: preference.action.clone(),
                            binding: selected.to_owned(),
                            conflicts_with: Some(previous_action.clone()),
                            message: format!(
                                "与动作 {previous_action} 的绑定 {previous_binding} 重叠；GPUI 按上下文深度及声明顺序决胜。"
                            ),
                        });
                    }
                }

                parsed.push((
                    preference.action.clone(),
                    selected.to_owned(),
                    preference.scope,
                    canonical,
                ));
                bindings.push(binding);
            }
        }

        Self {
            keymap: Keymap::new(bindings),
            diagnostics,
        }
    }

    /// 返回当前快照的未知动作、解析失败和冲突诊断。
    pub fn diagnostics(&self) -> &[KeybindingDiagnostic] {
        &self.diagnostics
    }

    /// 通过 GPUI Keymap 解析当前输入；焦点、重复和 IME 门禁优先于动作匹配。
    pub fn resolve(
        &self,
        input: &[Keystroke],
        context_stack: &[KeyContext],
        ime_composing: bool,
        text_input_focused: bool,
        repeated: bool,
    ) -> KeybindingResolution {
        if ime_composing
            || text_input_focused
            || repeated
            || context_stack
                .iter()
                .any(|context| context.contains(KeybindingContext::TextInput.as_gpui_identifier()))
            || input.iter().any(Keystroke::is_ime_in_progress)
        {
            return KeybindingResolution::default();
        }

        let (matched, pending) = self.keymap.bindings_for_input(input, context_stack);
        let action = matched.first().and_then(|binding| {
            binding
                .action()
                .as_any()
                .downcast_ref::<KerminalShortcutAction>()
                .map(|shortcut| shortcut.action)
        });

        KeybindingResolution { action, pending }
    }
}

/// 选择主绑定及同 action 的 Windows 备用绑定；旧 Linux 行缺字段时仍沿用 Windows 回退。
///
/// @author kongweiguang
fn effective_bindings(
    preference: &KeybindingPreference,
    platform: KeybindingPlatform,
) -> Vec<&str> {
    let primary = effective_binding(preference, platform);
    let mut bindings = vec![primary];
    if platform == KeybindingPlatform::Windows
        && let Some(alternate) = preference
            .windows_alternate_binding
            .as_deref()
            .filter(|binding| !binding.trim().is_empty())
    {
        bindings.push(alternate);
    }
    bindings
}

/// 保持旧 Windows/macOS 空 override 回退规则；Linux 的 Some 空串是显式关闭值。
///
/// @author kongweiguang
fn effective_binding(preference: &KeybindingPreference, platform: KeybindingPlatform) -> &str {
    let override_binding = match platform {
        KeybindingPlatform::Windows => &preference.windows_binding,
        KeybindingPlatform::Linux => {
            if let Some(linux_binding) = preference.linux_binding.as_deref() {
                return linux_binding;
            }
            &preference.windows_binding
        }
        KeybindingPlatform::Mac => &preference.mac_binding,
    };
    if override_binding.trim().is_empty() {
        &preference.binding
    } else {
        override_binding
    }
}

/// 把旧版加号分隔符与修饰键别名转换成 GPUI 格式，再交 GPUI parser 校验完整按键语法。
fn normalize_legacy_binding(binding: &str) -> String {
    binding
        .split_whitespace()
        .map(|stroke| {
            stroke
                .split('+')
                .map(str::trim)
                .filter(|component| !component.is_empty())
                .map(normalize_component)
                .collect::<Vec<_>>()
                .join("-")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 仅规范历史 modifier/key 命名差异，不自行解析 GPUI 按键或组合语法。
fn normalize_component(component: &str) -> String {
    match component.to_ascii_lowercase().as_str() {
        "control" => "ctrl".to_owned(),
        "option" => "alt".to_owned(),
        "command" | "meta" => "cmd".to_owned(),
        "windows" => "win".to_owned(),
        key if key.starts_with("arrow") => key.trim_start_matches("arrow").to_owned(),
        key if key.starts_with("digit") && key.len() == 6 => key[5..].to_owned(),
        key if key.starts_with("key") && key.len() == 4 => key[3..].to_owned(),
        key => key.to_owned(),
    }
}

/// 把旧版三种作用范围映射到有明确层级的 GPUI context predicate。
fn context_predicate(scope: KeybindingScope) -> &'static str {
    match scope {
        KeybindingScope::Global => "KerminalApp && !KerminalTextInput",
        KeybindingScope::Workspace => "(KerminalApp > KerminalWorkspace) && !KerminalTextInput",
        KeybindingScope::Terminal => {
            "(KerminalApp > KerminalWorkspace > KerminalTerminal) && !KerminalTextInput"
        }
    }
}

/// 判断两个作用域是否可能同时出现在同一 GPUI context stack。
fn scopes_overlap(left: KeybindingScope, right: KeybindingScope) -> bool {
    matches!(
        (left, right),
        (KeybindingScope::Global, _)
            | (_, KeybindingScope::Global)
            | (KeybindingScope::Workspace, KeybindingScope::Workspace)
            | (KeybindingScope::Workspace, KeybindingScope::Terminal)
            | (KeybindingScope::Terminal, KeybindingScope::Workspace)
            | (KeybindingScope::Terminal, KeybindingScope::Terminal)
    )
}
