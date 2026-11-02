//! 当前设置窗口独占的快捷键录制；确认前不修改持久化草稿。
//!
//! @author kongweiguang

use gpui_kit::{Context, FocusHandle, InputPreference, Keystroke, Subscription, Window};

use crate::keybindings::KeybindingAction;

use super::{PreferencesPanel, ShortcutBindingField, ShortcutBindingUpdate};

/// 一个专用焦点身份同时限定窗口、录制代次和拦截器生命周期。
///
/// @author kongweiguang
pub(in crate::features::preferences) struct ShortcutCapture {
    pub(in crate::features::preferences) action: String,
    pub(in crate::features::preferences) field: ShortcutBindingField,
    pub(in crate::features::preferences) focus: FocusHandle,
    pub(in crate::features::preferences) strokes: Vec<String>,
    pub(in crate::features::preferences) recording: bool,
    pub(in crate::features::preferences) error: Option<String>,
    return_focus: Option<FocusHandle>,
}

impl PreferencesPanel {
    /// 拦截器只接受当前窗口与本次焦点身份，避免录制误吞其他窗口或旧会话事件。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn start_shortcut_capture(
        &mut self,
        action: String,
        field: ShortcutBindingField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_controls_locked()
            || KeybindingAction::from_id(&action).is_none()
            || !self.settings.as_ref().is_some_and(|settings| {
                settings
                    .value()
                    .keybindings
                    .iter()
                    .any(|binding| binding.action == action && binding.editable)
            })
        {
            return;
        }

        self.cancel_shortcut_capture(cx);
        let return_focus = window.focused(cx);
        let focus = cx.focus_handle().tab_stop(true);
        let capture_focus = focus.clone();
        let focus_for_interceptor = focus.clone();
        let owner = cx.weak_entity();
        let target_window = window.window_handle();
        self.shortcut_capture = Some(ShortcutCapture {
            action,
            field,
            focus,
            strokes: Vec::new(),
            recording: true,
            error: None,
            return_focus,
        });
        let interceptor = cx.intercept_keystrokes(move |event, window, app| {
            if window.window_handle() != target_window
                || !focus_for_interceptor.contains_focused(window, app)
                || event.input_preference == InputPreference::CharacterInput
                || event.keystroke.is_ime_in_progress()
            {
                return;
            }
            let Some(panel) = owner.upgrade() else {
                return;
            };
            let active = panel
                .read(app)
                .shortcut_capture
                .as_ref()
                .is_some_and(|capture| capture.focus == focus_for_interceptor);
            if !active {
                return;
            }
            let focused = focus_for_interceptor.is_focused(window);
            let key = event.keystroke.key.to_ascii_lowercase();
            if !focused
                && (is_focus_navigation_stroke(&event.keystroke)
                    || (!event.keystroke.modifiers.modified()
                        && matches!(key.as_str(), "enter" | "space")))
            {
                return;
            }
            let consumed = if focused || (key == "escape" && !event.keystroke.modifiers.modified())
            {
                panel.update(app, |panel, cx| {
                    panel.capture_shortcut_keystroke(&event.keystroke, window, cx)
                })
            } else {
                true
            };
            if consumed {
                window.prevent_default();
                app.stop_propagation();
            }
        });
        let focus_for_blur = capture_focus.clone();
        let owner_for_blur = cx.weak_entity();
        let focus_subscription = window.on_focus_out(&capture_focus, cx, move |_, _, app| {
            if let Some(panel) = owner_for_blur.upgrade() {
                let active = panel
                    .read(app)
                    .shortcut_capture
                    .as_ref()
                    .is_some_and(|capture| capture.focus == focus_for_blur);
                if active {
                    panel.update(app, |panel, cx| panel.cancel_shortcut_capture(cx));
                }
            }
        });
        self.shortcut_capture_subscription =
            Some(Subscription::join(interceptor, focus_subscription));
        // FocusHandle 可在挂载前持有焦点；同步聚焦避免首个按键早于平台下一帧而漏录。
        capture_focus.focus(window, cx);
        cx.notify();
    }

    /// 取消时仅清理尚未提交的录制值，原设置与关联输入保持不变。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn cancel_shortcut_capture(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        let had_capture = self.shortcut_capture.take().is_some();
        let had_subscription = self.shortcut_capture_subscription.take().is_some();
        if had_capture || had_subscription {
            cx.notify();
        }
    }

    /// 已有前缀继续录制时重新聚焦专用控件，避免鼠标点击按钮改变事件归属。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn add_shortcut_stroke(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(capture) = self.shortcut_capture.as_mut() else {
            return;
        };
        capture.recording = true;
        capture.error = None;
        capture.focus.focus(window, cx);
        cx.notify();
    }

    /// 捕获阶段消费组合键；预览阶段仅 Enter 确认、Escape 取消，不转发应用动作。
    ///
    /// @author kongweiguang
    fn capture_shortcut_keystroke(
        &mut self,
        stroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = stroke.key.to_ascii_lowercase();
        if key == "escape" && !stroke.modifiers.modified() {
            self.finish_shortcut_capture(window, cx);
            return true;
        }
        let Some(capture) = self.shortcut_capture.as_mut() else {
            return false;
        };
        if !capture.recording {
            if is_focus_navigation_stroke(stroke) {
                return false;
            }
            if key == "enter" && !stroke.modifiers.modified() {
                self.confirm_shortcut_capture(window, cx);
            }
            return true;
        }
        if is_modifier_key(&key) {
            return true;
        }
        if is_terminal_reserved_stroke(stroke) {
            capture.error = Some(
                crate::locale::text(
                    "此按键已由终端复制、中断或滚动使用。",
                    "This key is reserved for terminal copy, interrupt, or scrolling.",
                )
                .to_string(),
            );
            cx.notify();
            return true;
        }
        if is_plain_character_stroke(stroke) {
            capture.error = Some(
                crate::locale::text(
                    "字符按键需要搭配 Ctrl、Alt 或系统键。",
                    "Use Ctrl, Alt, or the system key with a character.",
                )
                .to_string(),
            );
            cx.notify();
            return true;
        }
        capture.strokes.push(stroke.unparse());
        capture.recording = false;
        capture.error = None;
        cx.notify();
        true
    }

    /// 只在候选通过目标平台编译与冲突检查后预览并提交，失败保留录制草稿。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn confirm_shortcut_capture(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(capture) = self.shortcut_capture.as_ref() else {
            return;
        };
        if capture.recording || capture.strokes.is_empty() {
            return;
        }
        let action = capture.action.clone();
        let field = capture.field;
        let value = capture.strokes.join(" ");
        if let Err(message) =
            self.commit_shortcut_binding(&action, field, ShortcutBindingUpdate::Set(value), cx)
        {
            if let Some(capture) = self.shortcut_capture.as_mut() {
                capture.error = Some(message);
            }
            cx.notify();
            return;
        }
        self.finish_shortcut_capture(window, cx);
    }

    /// 退出录制后等按钮重新挂载；期间若出现离开确认，焦点交由确认层持有。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn finish_shortcut_capture(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let return_focus = self
            .shortcut_capture
            .as_ref()
            .and_then(|capture| capture.return_focus.clone());
        self.cancel_shortcut_capture(cx);
        if let Some(return_focus) = return_focus {
            let panel = cx.weak_entity();
            window.on_next_frame(move |window, cx| {
                if panel
                    .upgrade()
                    .is_some_and(|panel| !panel.read(cx).has_leave_confirmation())
                {
                    return_focus.focus(window, cx);
                }
            });
        }
    }
}

/// 纯修饰键没有可执行主键，保留录制状态等待下一次按键。
///
/// @author kongweiguang
fn is_modifier_key(key: &str) -> bool {
    matches!(
        key,
        "ctrl"
            | "control"
            | "shift"
            | "alt"
            | "cmd"
            | "win"
            | "super"
            | "fn"
            | "platform"
            | "function"
    )
}

/// 预览状态允许 Tab 和 Shift+Tab 在录制控件及其按钮间移动焦点。
///
/// @author kongweiguang
fn is_focus_navigation_stroke(stroke: &Keystroke) -> bool {
    let modifiers = stroke.modifiers;
    stroke.key.eq_ignore_ascii_case("tab")
        && !modifiers.control
        && !modifiers.alt
        && !modifiers.platform
        && !modifiers.function
}

/// 运行态会把纯字符及 Shift 字符留给文本输入，因此不能接受无法执行的绑定。
///
/// @author kongweiguang
fn is_plain_character_stroke(stroke: &Keystroke) -> bool {
    let modifiers = stroke.modifiers;
    !modifiers.control
        && !modifiers.alt
        && !modifiers.platform
        && !modifiers.function
        && stroke.key.chars().count() == 1
}

/// 终端复制、中断和只读历史滚动在配置 keymap 前处理，录制时显式拒绝这些组合。
///
/// @author kongweiguang
fn is_terminal_reserved_stroke(stroke: &Keystroke) -> bool {
    let modifiers = stroke.modifiers;
    if modifiers.alt {
        return false;
    }
    let key = stroke.key.to_ascii_lowercase();
    (key == "c" && (modifiers.platform || modifiers.control))
        || (matches!(key.as_str(), "pageup" | "pagedown")
            && modifiers.shift
            && !modifiers.control
            && !modifiers.platform)
        || (matches!(key.as_str(), "home" | "end")
            && modifiers.shift
            && modifiers.control
            && !modifiers.platform)
}

/// 与录制入口共用终端保留键规则，避免恢复默认或回退值提交运行时无法触发的组合。
///
/// @author kongweiguang
pub(super) fn shortcut_sequence_issue(value: &str) -> Option<String> {
    let normalized = crate::keybindings::normalize_legacy_binding(value);
    for part in normalized.split_whitespace() {
        let Ok(stroke) = Keystroke::parse(part) else {
            continue;
        };
        if is_modifier_key(&stroke.key.to_ascii_lowercase()) {
            return Some(
                crate::locale::text("请在修饰键后添加按键。", "Add a key after the modifiers.")
                    .to_string(),
            );
        }
        if is_terminal_reserved_stroke(&stroke) {
            return Some(
                crate::locale::text(
                    "此按键已由终端复制、中断或滚动使用。",
                    "This key is reserved for terminal copy, interrupt, or scrolling.",
                )
                .to_string(),
            );
        }
        if is_plain_character_stroke(&stroke) {
            return Some(
                crate::locale::text(
                    "字符按键需要搭配 Ctrl、Alt 或系统键。",
                    "Use Ctrl, Alt, or the system key with a character.",
                )
                .to_string(),
            );
        }
    }
    None
}

#[cfg(test)]
#[path = "../../../../tests/unit/shortcut_capture.rs"]
mod tests;
