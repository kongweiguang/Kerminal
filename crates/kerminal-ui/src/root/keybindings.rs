//! Root 快捷键上下文投影；按焦点边界复用统一 GPUI keymap resolver。
//!
//! @author kongweiguang

use std::time::{Duration, Instant};

use gpui_kit::{KeyContext, Keystroke};
use kerminal_application::preferences::KeybindingPreference;

use crate::keybindings::{
    CompiledKeybindings, KeybindingAction, KeybindingContext, KeybindingPlatform,
};

const KEY_SEQUENCE_TIMEOUT: Duration = Duration::from_millis(1_000);

/// Root 当前焦点的快捷键作用范围；编辑器和弹层由自己的控件处理按键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeybindingFocusScope {
    /// 应用级面板或导航焦点。
    App,
    /// 工作区焦点，可执行工作区级快捷键。
    Workspace,
    /// 终端画布焦点，可执行终端及上层快捷键。
    Terminal,
    /// 原生编辑器焦点，避免应用快捷键截获编辑和输入法按键。
    Editor,
    /// 编辑器内仅解析退出和打开设置动作，其他全局动作不进入该作用域。
    EditorGlobal,
    /// 模态弹层焦点，完全交由弹层自身的键位语义处理。
    Modal,
}

/// 一次 Root 快捷键解析结果；只有 Action 和 Pending 会拦截当前按键。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum KeybindingDispatch {
    /// 当前按键不属于快捷键，必须继续交给终端或焦点控件。
    Unhandled,
    /// 当前按键是有效 chord 前缀，暂时等待后续 stroke。
    Pending,
    /// 找到已知的业务动作，由 Root 调用对应的原生用例入口。
    Action(KeybindingAction),
}

/// 保存当前设置编译出的不可变 keymap 与短暂 chord 前缀。
pub(super) struct RootKeybindingRuntime {
    compiled: CompiledKeybindings,
    editor_global_runtime: Option<Box<RootKeybindingRuntime>>,
    pending_strokes: Vec<Keystroke>,
    pending_scope: Option<KeybindingFocusScope>,
    pending_since: Option<Instant>,
}

impl Default for RootKeybindingRuntime {
    /// 启动时先安装空 keymap，等待 PreferencesService 的首份快照后再启用用户绑定。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            compiled: CompiledKeybindings::compile(&[], KeybindingPlatform::current()),
            editor_global_runtime: None,
            pending_strokes: Vec::new(),
            pending_scope: None,
            pending_since: None,
        }
    }
}

impl RootKeybindingRuntime {
    /// Settings 变化时整体替换解析快照，并为文本编辑焦点单独编译最小全局动作白名单。
    ///
    /// @author kongweiguang
    pub(super) fn update(&mut self, preferences: &[KeybindingPreference]) {
        self.compiled = CompiledKeybindings::compile(preferences, KeybindingPlatform::current());
        let editor_preferences = preferences
            .iter()
            .filter(|preference| {
                [
                    KeybindingAction::ApplicationQuit,
                    KeybindingAction::SettingsOpen,
                ]
                .iter()
                .any(|action| preference.action == action.id())
            })
            .cloned()
            .collect::<Vec<_>>();
        let editor_global_runtime = Self {
            compiled: CompiledKeybindings::compile(
                &editor_preferences,
                KeybindingPlatform::current(),
            ),
            ..Self::default()
        };
        self.editor_global_runtime = Some(Box::new(editor_global_runtime));
        self.clear_pending();
    }

    /// 用 GPUI keymap 解析焦点明确的按键；IME 组合状态始终由真实预编辑状态门禁处理。
    /// Tab/Enter 的原生事件没有可打印字符，交由 GPUI 的非打印键分类处理，不伪造文本标记。
    /// @author kongweiguang
    pub(super) fn dispatch(
        &mut self,
        keystroke: &Keystroke,
        scope: KeybindingFocusScope,
        ime_composing: bool,
        text_input_focused: bool,
        repeated: bool,
    ) -> KeybindingDispatch {
        let Some(contexts) = context_stack(scope) else {
            self.clear_pending();
            return KeybindingDispatch::Unhandled;
        };
        if ime_composing
            || text_input_focused
            || repeated
            || keystroke.is_ime_in_progress()
            || is_plain_text_stroke(keystroke)
        {
            self.clear_pending();
            return KeybindingDispatch::Unhandled;
        }

        let now = Instant::now();
        let sequence_expired = self
            .pending_since
            .is_some_and(|started| now.duration_since(started) > KEY_SEQUENCE_TIMEOUT);
        if sequence_expired || self.pending_scope.is_some_and(|pending| pending != scope) {
            self.clear_pending();
        }

        let mut sequence = self.pending_strokes.clone();
        sequence.push(keystroke.clone());
        let resolution = self.compiled.resolve(
            &sequence,
            &contexts,
            ime_composing,
            text_input_focused,
            repeated,
        );
        if let Some(action) = resolution.action {
            self.clear_pending();
            return KeybindingDispatch::Action(action);
        }
        if resolution.pending {
            self.pending_strokes = sequence;
            self.pending_scope = Some(scope);
            self.pending_since = Some(now);
            return KeybindingDispatch::Pending;
        }

        // 上一 stroke 已作为 chord 前缀被消费；失败的当前 stroke 仍可独立匹配或进入终端。
        if !self.pending_strokes.is_empty() {
            self.clear_pending();
            let resolution = self.compiled.resolve(
                std::slice::from_ref(keystroke),
                &contexts,
                ime_composing,
                text_input_focused,
                repeated,
            );
            if let Some(action) = resolution.action {
                return KeybindingDispatch::Action(action);
            }
            if resolution.pending {
                self.pending_strokes.push(keystroke.clone());
                self.pending_scope = Some(scope);
                self.pending_since = Some(now);
                return KeybindingDispatch::Pending;
            }
        } else {
            self.clear_pending();
        }
        KeybindingDispatch::Unhandled
    }

    /// 在文本编辑焦点中只解析允许的两个全局动作，并沿用统一 chord、IME、字符输入和重复键门禁。
    ///
    /// @author kongweiguang
    pub(super) fn dispatch_editor_shortcut(
        &mut self,
        keystroke: &Keystroke,
        ime_composing: bool,
        prefer_character_input: bool,
        repeated: bool,
    ) -> KeybindingDispatch {
        let Some(runtime) = self.editor_global_runtime.as_mut() else {
            return KeybindingDispatch::Unhandled;
        };
        if prefer_character_input {
            runtime.clear_pending();
            return KeybindingDispatch::Unhandled;
        }
        runtime.dispatch(
            keystroke,
            KeybindingFocusScope::EditorGlobal,
            ime_composing,
            false,
            repeated,
        )
    }

    /// 焦点或设置上下文改变时清除两份解析器的 chord，避免跨焦点拼接按键。
    ///
    /// @author kongweiguang
    pub(super) fn clear_pending(&mut self) {
        self.pending_strokes.clear();
        self.pending_scope = None;
        self.pending_since = None;
        if let Some(editor_global_runtime) = self.editor_global_runtime.as_mut() {
            editor_global_runtime.clear_pending();
        }
    }
}

/// 纯 Shift 仍会产生文本，因此不能让单字符快捷键截获大小写输入或启动不可见 chord。
fn is_plain_text_stroke(keystroke: &Keystroke) -> bool {
    let modifiers = &keystroke.modifiers;
    !modifiers.control
        && !modifiers.alt
        && !modifiers.platform
        && !modifiers.function
        && keystroke.key.chars().count() == 1
}

/// 将 Root 识别出的焦点映射为与已编译快捷键 predicate 一致的 GPUI context stack。
///
/// `None` 表示当前按键必须留给模态控件处理；编辑器则保留上下文供观察，同时以
/// `KerminalTextInput` 明确屏蔽 Kerminal 全局快捷键。实际 IME、重复按键与 Keystroke
/// 解析仍由 `CompiledKeybindings::resolve` 统一判断，避免出现第二套解析规则。
///
/// @author kongweiguang
pub(super) fn context_stack(scope: KeybindingFocusScope) -> Option<Vec<KeyContext>> {
    let mut app = KeyContext::new_with_defaults();
    app.add(KeybindingContext::App.as_gpui_identifier());

    match scope {
        KeybindingFocusScope::App => Some(vec![app]),
        KeybindingFocusScope::Workspace => {
            let mut workspace = KeyContext::new_with_defaults();
            workspace.add(KeybindingContext::Workspace.as_gpui_identifier());
            Some(vec![app, workspace])
        }
        KeybindingFocusScope::Terminal => {
            let mut workspace = KeyContext::new_with_defaults();
            workspace.add(KeybindingContext::Workspace.as_gpui_identifier());
            let mut terminal = KeyContext::new_with_defaults();
            terminal.add(KeybindingContext::Terminal.as_gpui_identifier());
            Some(vec![app, workspace, terminal])
        }
        KeybindingFocusScope::Editor => {
            let mut workspace = KeyContext::new_with_defaults();
            workspace.add(KeybindingContext::Workspace.as_gpui_identifier());
            let mut editor = KeyContext::new_with_defaults();
            editor.add(KeybindingContext::TextInput.as_gpui_identifier());
            Some(vec![app, workspace, editor])
        }
        KeybindingFocusScope::EditorGlobal => Some(vec![app]),
        KeybindingFocusScope::Modal => None,
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_keybindings.rs"]
mod tests;
