//! 焦点上下文分类与 chord 焦点身份追踪。
//!
//! @author kongweiguang

use gpui_kit::{KeyContext, KeyDownEvent};

use super::keybindings::KeybindingFocusScope;

const TEXT_INPUT_CONTEXTS: [&str; 3] = ["Input", "TextInput", "KerminalTextInput"];

/// 按 GPUI 当前焦点路径分类快捷键作用域；任何文本控件优先于其工作区/终端祖先。
///
/// Kit 输入框使用通用 `Input` context，不能只依赖 Kerminal 自定义标记；将输入检查放在
/// Terminal 之前，避免左栏搜索和右侧 Agent 输入误触发继承自祖先的工作区快捷键。
pub(super) fn classify_focus_scope(
    contexts: &[KeyContext],
    modal_active: bool,
) -> KeybindingFocusScope {
    if modal_active || context_present(contexts, &["Modal", "Menu", "Popover", "PopupMenu"]) {
        return KeybindingFocusScope::Modal;
    }
    if is_text_input_context(contexts) {
        return KeybindingFocusScope::Editor;
    }
    if context_present(contexts, &["KerminalTerminal"]) {
        return KeybindingFocusScope::Terminal;
    }
    if context_present(contexts, &["KerminalWorkspace"]) {
        return KeybindingFocusScope::Workspace;
    }
    KeybindingFocusScope::App
}

/// 判断当前焦点路径是否落在系统或应用的文本编辑控件中。
pub(super) fn is_text_input_context(contexts: &[KeyContext]) -> bool {
    context_present(contexts, &TEXT_INPUT_CONTEXTS)
}

/// 模态只阻断底层动作，不吞焦点文本框依赖的原生字符；修饰快捷键仍由模态处理。
/// AltGr/死键沿用平台的字符优先事实，不以 ASCII 白名单猜测键盘布局。
pub(super) fn modal_key_uses_platform_text(contexts: &[KeyContext], event: &KeyDownEvent) -> bool {
    if !is_text_input_context(contexts) {
        return false;
    }
    if event.prefer_character_input {
        return true;
    }
    let modifiers = event.keystroke.modifiers;
    !modifiers.control
        && !modifiers.platform
        && !modifiers.alt
        && !modifiers.function
        && event
            .keystroke
            .key_char
            .as_ref()
            .is_some_and(|text| !text.is_empty() && !text.chars().any(char::is_control))
}

/// 活动预编辑期间把 pane 按键留给平台 IME，避免菜单、快捷键或 PTY 抢走候选键。
pub(super) fn pane_key_is_owned_by_ime(ime_composing: bool) -> bool {
    ime_composing
}

/// 组合文本尚未提交时隐藏候选 UI，防止旧候选看起来仍可接受。
pub(super) fn suggestion_overlays_visible(
    ime_composing: bool,
    menu_matches_binding: bool,
    has_inline_ghost: bool,
) -> bool {
    !ime_composing && (menu_matches_binding || has_inline_ghost)
}

/// 在整条焦点路径搜索一个或多个 context 名称，保留祖先 scope 供后续优先级判定。
fn context_present(contexts: &[KeyContext], names: &[&str]) -> bool {
    contexts
        .iter()
        .any(|context| names.iter().any(|name| context.contains(name)))
}

/// 跟踪当前 GPUI 焦点；chord 只在同一个具体控件获得两次按键时保持有效。
pub(super) struct KeybindingFocusTracker<T> {
    focused: Option<T>,
    initialized: bool,
}

impl<T> Default for KeybindingFocusTracker<T> {
    /// 初始焦点尚未观察，首个事件将建立明确边界并清掉可能残留的 chord。
    fn default() -> Self {
        Self {
            focused: None,
            initialized: false,
        }
    }
}

impl<T: PartialEq> KeybindingFocusTracker<T> {
    /// 保存当前焦点并返回其是否不同于上次观察值；首次观察也视为边界。
    pub(super) fn observe(&mut self, focused: Option<T>) -> bool {
        let changed = !self.initialized || self.focused != focused;
        self.focused = focused;
        self.initialized = true;
        changed
    }

    /// 明确的 Tab 或中心面板切换会让旧焦点身份失效，即使 GPUI 尚未报告新焦点。
    pub(super) fn reset(&mut self) {
        self.focused = None;
        self.initialized = false;
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_keybinding_focus.rs"]
mod tests;
