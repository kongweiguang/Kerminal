//! 终端的局部按键与平台文本边界，其余输入能力委托给 GPUI handler。
//!
//! @author kongweiguang

use std::ops::Range;

use gpui_kit::{
    App, Bounds, ClipboardItem, FocusHandle, InputHandler, KeyBinding, NoAction, Pixels, Point,
    TextInputConfiguration, UTF16Selection, Window,
};

pub(crate) const TERMINAL_INPUT_CONTEXT: &str = "KerminalTerminalInput";

/// 只覆盖真实聚焦画布的 Tab 导航，让中央和 Agent 终端都能先接收补全/TUI 按键。
/// 搜索框、工具按钮不挂此 context，继续使用 Kit 的正常焦点顺序。
///
/// @author kongweiguang
pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", NoAction, Some(TERMINAL_INPUT_CONTEXT)),
        KeyBinding::new("shift-tab", NoAction, Some(TERMINAL_INPUT_CONTEXT)),
    ]);
}

/// 焦点在搜索或按钮上时移除覆盖，避免终端祖先吞掉子控件正常的 Tab 导航。
///
/// @author kongweiguang
pub(crate) fn keyboard_context(focus: &FocusHandle, window: &Window) -> Option<&'static str> {
    focus.is_focused(window).then_some(TERMINAL_INPUT_CONTEXT)
}

/// 判断当前按键是否应跳过 IME 优先路由，让终端把 Option 作为 Meta 处理。
///
/// 将平台判断做成纯函数，使分支可在非 macOS 构建机上独立验证；生产调用仍以 GPUI
/// 窗口即时 modifier 为准，不依赖可能晚于原生 IME 查询的应用事件状态。
///
/// @author kongweiguang
fn should_bypass_ime_for_option_as_meta(
    is_macos: bool,
    meta_enabled: bool,
    option_pressed: bool,
) -> bool {
    is_macos && meta_enabled && option_pressed
}

/// 保留原输入处理器全部行为，只在 Option-as-Meta 条件成立时关闭 IME 优先路由。
///
/// `accepts_text_input` 与焦点状态继续由内层 handler 管理，避免按住 Option 时让 GPUI
/// 误认为文本输入失焦，从而取消正在进行的输入法组合。
pub(crate) struct TerminalImePreferenceHandler<H> {
    inner: H,
    mac_option_is_meta: bool,
}

impl<H> TerminalImePreferenceHandler<H> {
    /// 包装原 handler，并固定本 pane 当前生效的 Option-as-Meta 设置。
    ///
    /// @author kongweiguang
    pub(crate) const fn new(inner: H, mac_option_is_meta: bool) -> Self {
        Self {
            inner,
            mac_option_is_meta,
        }
    }

    /// 仅在设置开启且当前键确为 Option 时给平台返回绕过 IME 的覆盖值；否则继续委托原 handler。
    ///
    /// 将覆盖规则留在实际注册的 wrapper 上，让跨平台测试验证生产输入链使用的同一策略。
    ///
    /// @author kongweiguang
    fn ime_override_for_option(&self, is_macos: bool, option_pressed: bool) -> Option<bool> {
        should_bypass_ime_for_option_as_meta(is_macos, self.mac_option_is_meta, option_pressed)
            .then_some(false)
    }
}

impl<H: InputHandler> InputHandler for TerminalImePreferenceHandler<H> {
    /// 委托 UTF-16 选区查询，保持原 handler 的 IME 缓冲区语义。
    ///
    /// @author kongweiguang
    fn selected_text_range(
        &mut self,
        ignore_disabled_input: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<UTF16Selection> {
        self.inner
            .selected_text_range(ignore_disabled_input, window, cx)
    }

    /// 委托 marked range 查询，使系统组合态仍由原 handler 决定。
    ///
    /// @author kongweiguang
    fn marked_text_range(&mut self, window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        self.inner.marked_text_range(window, cx)
    }

    /// 委托组合缓冲区读取，避免包装层重建或截断 UTF-16 文本。
    ///
    /// @author kongweiguang
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<String> {
        self.inner
            .text_for_range(range_utf16, adjusted_range, window, cx)
    }

    /// 委托 IME commit，保持原有文本提交及重复按键去重逻辑。
    ///
    /// @author kongweiguang
    fn replace_text_in_range(
        &mut self,
        replacement_range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner
            .replace_text_in_range(replacement_range, text, window, cx);
    }

    /// 委托预编辑更新，让 composition buffer 和选区仍归原 handler 所有。
    ///
    /// @author kongweiguang
    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner.replace_and_mark_text_in_range(
            range_utf16,
            new_text,
            new_selected_range,
            window,
            cx,
        );
    }

    /// 委托取消预编辑，避免包装层留下过期的 marked text。
    ///
    /// @author kongweiguang
    fn unmark_text(&mut self, window: &mut Window, cx: &mut App) {
        self.inner.unmark_text(window, cx);
    }

    /// 委托平台剪贴板输入，保留终端原有 bracketed-paste 行为。
    ///
    /// @author kongweiguang
    fn paste(&mut self, item: ClipboardItem, window: &mut Window, cx: &mut App) {
        self.inner.paste(item, window, cx);
    }

    /// 委托候选窗位置查询，使 IME 候选仍定位在原终端光标附近。
    ///
    /// @author kongweiguang
    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        self.inner.bounds_for_range(range_utf16, window, cx)
    }

    /// 委托屏幕点到文本索引查询，保持 GPUI 原生输入定位策略。
    ///
    /// @author kongweiguang
    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<usize> {
        self.inner.character_index_for_point(point, window, cx)
    }

    /// 委托系统文本选区变化，不把 IME 选区映射成终端网格选择。
    ///
    /// @author kongweiguang
    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner.set_selected_text_range(range_utf16, window, cx);
    }

    /// 委托元素边界查询，保持 GPUI 对焦点文本区域的原有定位。
    ///
    /// @author kongweiguang
    fn element_bounds(&mut self, window: &mut Window, cx: &mut App) -> Option<Bounds<Pixels>> {
        self.inner.element_bounds(window, cx)
    }

    /// 委托 UTF-16 文本长度查询，保持输入法可见缓冲区的原有范围。
    ///
    /// @author kongweiguang
    fn text_length_utf16(&mut self, window: &mut Window, cx: &mut App) -> Option<usize> {
        self.inner.text_length_utf16(window, cx)
    }

    /// 委托按住字符键行为，不改变 macOS 长按字符弹层策略。
    ///
    /// @author kongweiguang
    fn apple_press_and_hold_enabled(&mut self) -> bool {
        self.inner.apple_press_and_hold_enabled()
    }

    /// 委托文本输入接受状态，避免 modifier 改变时切换平台 IME 焦点。
    ///
    /// @author kongweiguang
    fn accepts_text_input(&mut self, window: &mut Window, cx: &mut App) -> bool {
        self.inner.accepts_text_input(window, cx)
    }

    /// 委托输入法可编辑范围，保持平台文本辅助功能边界不变。
    ///
    /// @author kongweiguang
    fn text_input_editable_range(
        &mut self,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Range<usize>> {
        self.inner.text_input_editable_range(window, cx)
    }

    /// Option-as-Meta 时优先让 key handler 编码按键；其他输入源继续沿用原 IME 策略。
    ///
    /// 仅在 macOS Option-as-Meta 已启用且 GPUI 当前窗口报告 Alt 时覆盖为 false；读取窗口
    /// 的同步 modifier，避免依赖 Root 收到修饰键事件的时序。
    ///
    /// @author kongweiguang
    fn prefers_ime_for_printable_keys(&mut self, window: &mut Window, cx: &mut App) -> bool {
        if let Some(prefers_ime) =
            self.ime_override_for_option(cfg!(target_os = "macos"), window.modifiers().alt)
        {
            prefers_ime
        } else {
            self.inner.prefers_ime_for_printable_keys(window, cx)
        }
    }

    /// 委托平台文本辅助配置，保持自动更正、建议和输入动作设置不变。
    ///
    /// @author kongweiguang
    fn text_input_configuration(
        &mut self,
        window: &mut Window,
        cx: &mut App,
    ) -> TextInputConfiguration {
        self.inner.text_input_configuration(window, cx)
    }
}

#[cfg(test)]
#[path = "../tests/unit/terminal_input_handler.rs"]
mod tests;
