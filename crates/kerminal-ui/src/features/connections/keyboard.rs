//! 主机表单的快捷保存门禁；在输入组件消费按键前读取 IME 状态。
//!
//! @author kongweiguang

use gpui_kit::{
    Context, KeyDownEvent, Window,
    component::{WindowExt as _, input::AnyInputState},
};

use super::panel::{ConnectionsEditorKind, ConnectionsPanel};

impl ConnectionsPanel {
    /// Escape 先关闭最上层删除确认，且只调用既有取消入口，写入门禁和未知回执仍由原逻辑裁决。
    /// 面板内按钮持有焦点时也必须在这一层截断按键，否则会穿透到终端或全局快捷键。
    ///
    /// @author kongweiguang
    pub(super) fn handle_confirmation_escape(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !event.keystroke.key.eq_ignore_ascii_case("escape")
            || event.keystroke.modifiers.modified()
            || event.is_held
        {
            return;
        }
        if !self.has_pending_deletion_confirmation() {
            return;
        }
        window.prevent_default();
        cx.stop_propagation();
        self.cancel_pending_deletion_confirmation(cx);
    }

    /// Ctrl/Cmd+S 只提交当前单行编辑任务；多行、IME 和顶层分组弹层拥有自己的输入语义。
    /// 捕获阶段读取组合态，避免输入组件先提交候选词后把同一按键误判为保存。
    /// @author kongweiguang
    pub(super) fn handle_editor_save_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.editor_open()
            || !event.keystroke.key.eq_ignore_ascii_case("s")
            || !event.keystroke.modifiers.secondary()
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.shift
        {
            return;
        }
        let (multiline, composing) = match window.focused_input(cx) {
            Some(AnyInputState::Input(input)) => (false, input.read(cx).is_composing()),
            Some(AnyInputState::Textarea(_)) | Some(AnyInputState::Editor(_)) => (true, false),
            Some(AnyInputState::Otp(_)) => (false, true),
            None => (false, false),
        };
        window.prevent_default();
        cx.stop_propagation();
        if !shortcut_may_save(
            event.is_held,
            multiline,
            composing,
            self.presentation.shows_group_editor() || self.pending_editor_discard,
        ) {
            return;
        }
        match self.active_editor_kind {
            Some(ConnectionsEditorKind::Local) => self.save_local_host(window, cx),
            Some(ConnectionsEditorKind::Remote) => self.save_editor(window, cx),
            None => {}
        }
    }
}

/// 按住重复、组合输入和嵌套任务均不能重复或提前提交底层草稿。
/// @author kongweiguang
fn shortcut_may_save(held: bool, multiline: bool, composing: bool, nested: bool) -> bool {
    !held && !multiline && !composing && !nested
}

#[cfg(test)]
#[path = "../../../tests/unit/connections_keyboard.rs"]
mod tests;
