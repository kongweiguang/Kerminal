//! Option-as-Meta 对输入法按键优先级的纯策略契约。
//!
//! @author kongweiguang

use super::TerminalImePreferenceHandler;

use gpui_kit::component::FocusTrapElement as _;
use gpui_kit::{
    AppContext as _, Context, Entity, FocusHandle, IntoElement, Render, TestAppContext, Window,
    div, prelude::*,
};

struct TerminalTabHarness {
    terminal: FocusHandle,
    control: FocusHandle,
    drawer_focus: Option<FocusHandle>,
    keys: Vec<String>,
}

impl Render for TerminalTabHarness {
    /// 复用生产按键上下文与 Kit Root，同时覆盖紧凑抽屉的焦点陷阱，避免窄窗再次吞掉 Tab。
    ///
    /// @author kongweiguang
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = div().size_full().child(
            div()
                .id("terminal")
                .track_focus(&self.terminal)
                .tab_index(0)
                .when_some(
                    super::keyboard_context(&self.terminal, window),
                    |canvas, context| canvas.key_context(context),
                )
                .on_key_down(cx.listener(|this, event: &gpui_kit::KeyDownEvent, _, cx| {
                    this.keys.push(event.keystroke.key.clone());
                    cx.stop_propagation();
                }))
                .child("terminal")
                .child(
                    div()
                        .id("control")
                        .track_focus(&self.control)
                        .tab_index(0)
                        .child("control"),
                ),
        );
        if let Some(focus) = self.drawer_focus.as_ref() {
            content
                .focus_trap("test-tool-drawer", focus)
                .into_any_element()
        } else {
            content.into_any_element()
        }
    }
}

/// Tab/Shift+Tab 必须到达画布且保持焦点；子控件聚焦后同一按键恢复 Kit 导航。
///
/// @author kongweiguang
#[gpui_kit::test]
fn tab_stays_in_focused_terminal_but_controls_keep_navigation(cx: &mut TestAppContext) {
    assert_terminal_tab_routing(cx, false);
}

/// 同一个输入上下文进入紧凑抽屉后仍优先交给 PTY，外部按钮继续支持可访问的键盘导航。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_drawer_focus_trap_does_not_consume_terminal_tab(cx: &mut TestAppContext) {
    assert_terminal_tab_routing(cx, true);
}

/// 两种宿主共用完整分派断言，不以直接调用 key handler 绕过 Kit 的全局 Tab 动作。
///
/// @author kongweiguang
fn assert_terminal_tab_routing(cx: &mut TestAppContext, in_drawer: bool) {
    cx.update(crate::init);
    let returned = std::rc::Rc::new(std::cell::RefCell::new(None));
    let output = returned.clone();
    let (_, visual) = cx.add_window_view(move |window, cx| {
        let view = cx.new(|cx| {
            let terminal = cx.focus_handle().tab_stop(true);
            cx.on_focus(&terminal, window, |_, _, cx| cx.notify())
                .detach();
            cx.on_blur(&terminal, window, |_, _, cx| cx.notify())
                .detach();
            TerminalTabHarness {
                terminal,
                control: cx.focus_handle().tab_stop(true),
                drawer_focus: in_drawer.then(|| cx.focus_handle()),
                keys: Vec::new(),
            }
        });
        *output.borrow_mut() = Some(view.clone());
        gpui_kit::base::Root::new(view, window, cx)
    });
    let view: Entity<TerminalTabHarness> = returned.borrow_mut().take().unwrap();
    visual.update(|window, app| {
        let focus = view.read(app).terminal.clone();
        window.focus(&focus, app);
        window.draw(app).clear(app);
    });
    for key in ["tab", "shift-tab"] {
        visual.simulate_keystrokes(key);
        visual.update(|window, app| assert!(view.read(app).terminal.is_focused(window)));
    }
    visual.update(|window, app| {
        assert_eq!(view.read(app).keys, ["tab", "tab"]);
        let focus = view.read(app).control.clone();
        window.focus(&focus, app);
        window.draw(app).clear(app);
    });
    visual.simulate_keystrokes("shift-tab");
    visual.update(|window, app| {
        assert!(
            view.read(app).terminal.is_focused(window),
            "control_focused={} keys={:?}",
            view.read(app).control.is_focused(window),
            view.read(app).keys
        );
        assert_eq!(
            view.read(app).keys.len(),
            2,
            "control Tab must not enter the terminal"
        );
    });
}

/// 只有实际输入包装器在 macOS 设置开启且按下 Option 时覆盖 IME 优先路由，其它情形仍委托原 handler。
///
/// @author kongweiguang
#[test]
fn registered_wrapper_overrides_ime_only_for_enabled_macos_option() {
    let enabled = TerminalImePreferenceHandler::new((), true);
    assert_eq!(enabled.ime_override_for_option(true, true), Some(false));
    assert_eq!(enabled.ime_override_for_option(true, false), None);
    assert_eq!(enabled.ime_override_for_option(false, true), None);

    let disabled = TerminalImePreferenceHandler::new((), false);
    assert_eq!(disabled.ime_override_for_option(true, true), None);
}
