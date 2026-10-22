use std::{cell::Cell, rc::Rc, sync::atomic::Ordering};

use anyhow::Context as _;
use gpui_util::ResultExt;
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::SystemServices::*,
        UI::{
            Controls::*,
            HiDpi::*,
            Input::{Ime::*, KeyboardAndMouse::*},
            WindowsAndMessaging::*,
        },
    },
    core::PCWSTR,
};

use crate::*;
use gpui::*;

pub(crate) const WM_GPUI_CURSOR_STYLE_CHANGED: u32 = WM_USER + 1;
pub(crate) const WM_GPUI_CLOSE_ONE_WINDOW: u32 = WM_USER + 2;
pub(crate) const WM_GPUI_TASK_DISPATCHED_ON_MAIN_THREAD: u32 = WM_USER + 3;
pub(crate) const WM_GPUI_DOCK_MENU_ACTION: u32 = WM_USER + 4;
pub(crate) const WM_GPUI_FORCE_UPDATE_WINDOW: u32 = WM_USER + 5;
pub(crate) const WM_GPUI_KEYBOARD_LAYOUT_CHANGED: u32 = WM_USER + 6;
pub(crate) const WM_GPUI_GPU_DEVICE_LOST: u32 = WM_USER + 7;
pub(crate) const WM_GPUI_KEYDOWN: u32 = WM_USER + 8;
pub(crate) const WM_GPUI_DISPLAYS_CHANGED: u32 = WM_USER + 10;
pub(crate) const WM_GPUI_TRACK_SYSTEM_MENU: u32 = WM_USER + 11;

const SIZE_MOVE_LOOP_TIMER_ID: usize = 1;

/// 只在原生拖拽空白区打开系统菜单，避免禁用的最大化/最小化控件因复用 HTCAPTION 而误触。
///
/// @author kongweiguang
fn should_show_custom_caption_system_menu(
    hide_title_bar: bool,
    button: MouseButton,
    hit_test: usize,
    control_area: Option<WindowControlArea>,
) -> bool {
    hide_title_bar
        && button == MouseButton::Right
        && hit_test == HTCAPTION as usize
        && control_area == Some(WindowControlArea::Drag)
}

/// 自绘标题栏的 HTCAPTION 右键由本层收口，避免默认处理越过控件或遮罩打开菜单。
///
/// @author kongweiguang
fn is_custom_caption_right_click(
    hide_title_bar: bool,
    button: MouseButton,
    hit_test: usize,
) -> bool {
    hide_title_bar && button == MouseButton::Right && hit_test == HTCAPTION as usize
}

/// 只接管无系统标题栏窗口的鼠标菜单请求与 Alt+Space，避免改写普通系统标题栏行为。
///
/// @author kongweiguang
fn custom_caption_system_menu_command(
    hide_title_bar: bool,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<u32> {
    if !hide_title_bar {
        return None;
    }

    let command = wparam.0 as u32 & 0xFFF0;
    match command {
        SC_MOUSEMENU => Some(command),
        SC_KEYMENU if lparam.0 as u32 == VK_SPACE.0 as u32 => Some(command),
        _ => None,
    }
}

/// 鼠标菜单沿用原屏幕坐标，键盘菜单锚定窗口左上角以模拟标准系统菜单入口。
///
/// @author kongweiguang
fn system_menu_tracking_position(handle: HWND, command: u32, lparam: LPARAM) -> Option<LPARAM> {
    if command == SC_MOUSEMENU {
        return Some(lparam);
    }
    if command != SC_KEYMENU {
        return None;
    }

    let mut window_rect = RECT::default();
    if let Err(error) = unsafe { GetWindowRect(handle, &mut window_rect) } {
        log::error!("Failed to locate native system menu anchor: {error}");
        return None;
    }

    Some(system_menu_position_lparam(POINT {
        x: window_rect.left,
        y: window_rect.top,
    }))
}

/// 仅在现有原生诊断开关启用时输出固定阶段和命令数值，避免记录按键正文或增加常态日志。
///
/// @author kongweiguang
fn system_menu_diagnostics(stage: &'static str, command: u32) {
    if ime_diagnostics_enabled() {
        eprintln!("gpui_windows_system_menu stage={stage} command={command}");
    }
}

/// 平台层没有应用 tracing 依赖；隔离验收仅把固定阶段和数值写入 stderr，绝不格式化输入正文。
///
/// @author kongweiguang
fn ime_diagnostics_enabled() -> bool {
    std::env::var_os("KERMINAL_TERMINAL_RESIZE_DIAGNOSTICS").is_some_and(|value| value == "1")
}

/// Coordinates window draws on the UI thread. Owned by the platform and
/// shared with every window (like `WindowsPlatformState::cursor_visible`),
/// because the coordination is inherently cross-window: while window A is
/// drawing, a re-entrant paint request for window B must be deferred.
pub(crate) struct DrawCoordinator {
    /// Whether some window is currently inside `draw_window`. Win32 can
    /// re-enter the window procedure while a draw is in progress (e.g.
    /// cross-thread `SendMessage` dispatch during message pumping, or modal
    /// message loops entered by COM calls), and drawing re-entrantly would
    /// nest GPUI draws. Nested draws are wasted work whose output is
    /// immediately redrawn, so we defer them instead.
    drawing: Cell<bool>,
}

impl DrawCoordinator {
    pub(crate) fn new() -> Self {
        Self {
            drawing: Cell::new(false),
        }
    }

    /// Let frame wakeups defer while any window is drawing, avoiding nested Win32 paint dispatch.
    ///
    /// @author kongweiguang
    pub(crate) fn is_drawing(&self) -> bool {
        self.drawing.get()
    }

    fn try_begin_draw(&self) -> Option<DrawWindowGuard<'_>> {
        // This only covers the `draw_window` span (which extends past the GPUI
        // draw, through presentation and IME updates). Requests that arrive
        // re-entrantly during GPUI-initiated draws (e.g. key dispatch or
        // opening a window draws synchronously) are deferred by GPUI's
        // `on_request_frame` callback itself, which no-ops in that case.
        if self.drawing.get() {
            None
        } else {
            self.drawing.set(true);
            Some(DrawWindowGuard { coordinator: self })
        }
    }
}

struct DrawWindowGuard<'a> {
    coordinator: &'a DrawCoordinator,
}

impl Drop for DrawWindowGuard<'_> {
    fn drop(&mut self) {
        self.coordinator.drawing.set(false);
    }
}

impl WindowsWindowInner {
    /// 路由窗口消息，并在原生焦点离开时先收尾固定的 IME 目标。
    pub(crate) fn handle_msg(
        self: &Rc<Self>,
        handle: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let handled = match msg {
            // `DefWindowProc` answers `MA_NOACTIVATE` for a left click on `HTCAPTION`.
            // The activation is only triggered when `DefWindowProc` handles the following `WM_NCLBUTTONDOWN`.
            // The GPUI event is dispatched in between, so a click handler runs while `active_window` is still
            // whichever window was active before the click. If that handler consumes the
            // press, `DefWindowProc` never sees it, so the window is never activated at all.
            // So, let's eagerly activate the window.
            WM_MOUSEACTIVATE => Some(MA_ACTIVATE as isize),
            WM_ACTIVATE => self.handle_activate_msg(wparam),
            WM_KILLFOCUS => self.handle_kill_focus_msg(handle),
            WM_CREATE => self.handle_create_msg(handle),
            WM_MOVE => self.handle_move_msg(handle, lparam),
            WM_SIZE => self.handle_size_msg(wparam, lparam),
            WM_GETMINMAXINFO => self.handle_get_min_max_info_msg(lparam),
            WM_ENTERSIZEMOVE | WM_ENTERMENULOOP => self.handle_size_move_loop(handle),
            WM_EXITSIZEMOVE | WM_EXITMENULOOP => self.handle_size_move_loop_exit(handle),
            WM_TIMER => self.handle_timer_msg(handle, wparam),
            WM_NCCALCSIZE => self.handle_calc_client_size(handle, wparam, lparam),
            WM_DPICHANGED => self.handle_dpi_changed_msg(handle, wparam, lparam),
            WM_DISPLAYCHANGE => self.handle_display_change_msg(handle),
            WM_NCHITTEST => self.handle_hit_test_msg(handle, lparam),
            WM_PAINT => self.handle_paint_msg(handle),
            WM_CLOSE => self.handle_close_msg(),
            WM_DESTROY => self.handle_destroy_msg(handle),
            WM_MOUSEMOVE => self.handle_mouse_move_msg(handle, lparam, wparam),
            WM_MOUSELEAVE | WM_NCMOUSELEAVE => self.handle_mouse_leave_msg(),
            WM_NCMOUSEMOVE => self.handle_nc_mouse_move_msg(handle, lparam),
            // Treat double click as a second single click, since we track the double clicks ourselves.
            // If you don't interact with any elements, this will fall through to the windows default
            // behavior of toggling whether the window is maximized.
            WM_NCLBUTTONDBLCLK | WM_NCLBUTTONDOWN => {
                self.handle_nc_mouse_down_msg(handle, MouseButton::Left, wparam, lparam)
            }
            WM_NCRBUTTONDOWN => {
                self.handle_nc_mouse_down_msg(handle, MouseButton::Right, wparam, lparam)
            }
            WM_NCMBUTTONDOWN => {
                self.handle_nc_mouse_down_msg(handle, MouseButton::Middle, wparam, lparam)
            }
            WM_NCLBUTTONUP => {
                self.handle_nc_mouse_up_msg(handle, MouseButton::Left, wparam, lparam)
            }
            WM_NCRBUTTONUP => {
                self.handle_nc_mouse_up_msg(handle, MouseButton::Right, wparam, lparam)
            }
            WM_NCMBUTTONUP => {
                self.handle_nc_mouse_up_msg(handle, MouseButton::Middle, wparam, lparam)
            }
            WM_LBUTTONDOWN => self.handle_mouse_down_msg(handle, MouseButton::Left, lparam),
            WM_RBUTTONDOWN => self.handle_mouse_down_msg(handle, MouseButton::Right, lparam),
            WM_MBUTTONDOWN => self.handle_mouse_down_msg(handle, MouseButton::Middle, lparam),
            WM_XBUTTONDOWN => {
                self.handle_xbutton_msg(handle, wparam, lparam, Self::handle_mouse_down_msg)
            }
            WM_LBUTTONUP => self.handle_mouse_up_msg(handle, MouseButton::Left, lparam),
            WM_RBUTTONUP => self.handle_mouse_up_msg(handle, MouseButton::Right, lparam),
            WM_MBUTTONUP => self.handle_mouse_up_msg(handle, MouseButton::Middle, lparam),
            WM_XBUTTONUP => {
                self.handle_xbutton_msg(handle, wparam, lparam, Self::handle_mouse_up_msg)
            }
            WM_MOUSEWHEEL => self.handle_mouse_wheel_msg(handle, wparam, lparam),
            WM_MOUSEHWHEEL => self.handle_mouse_horizontal_wheel_msg(handle, wparam, lparam),
            WM_SYSCOMMAND => self.handle_system_command_msg(handle, wparam, lparam),
            WM_GPUI_TRACK_SYSTEM_MENU => self.handle_track_system_menu_msg(handle, wparam, lparam),
            WM_SYSKEYUP => self.handle_syskeyup_msg(wparam, lparam),
            WM_KEYUP => self.handle_keyup_msg(wparam, lparam),
            WM_GPUI_KEYDOWN => self.handle_keydown_msg(wparam, lparam),
            WM_CHAR => self.handle_char_msg(wparam),
            WM_IME_CHAR => self.handle_ime_char_msg(wparam),
            WM_IME_STARTCOMPOSITION => self.handle_ime_start_composition(handle),
            WM_IME_COMPOSITION => self.handle_ime_composition(handle, lparam),
            WM_IME_ENDCOMPOSITION => self.handle_ime_end_composition(),
            WM_SETCURSOR => self.handle_set_cursor(handle, lparam),
            WM_SETTINGCHANGE => self.handle_system_settings_changed(handle, wparam, lparam),
            WM_INPUTLANGCHANGE => self.handle_input_language_changed(),
            WM_SHOWWINDOW => self.handle_window_visibility_changed(handle, wparam),
            WM_GPUI_CURSOR_STYLE_CHANGED => self.handle_cursor_changed(lparam),
            WM_GPUI_FORCE_UPDATE_WINDOW => self.draw_window(handle, true),
            WM_GPUI_GPU_DEVICE_LOST => self.handle_device_lost(lparam),
            DM_POINTERHITTEST => self.handle_dm_pointer_hit_test(wparam),
            WM_GETOBJECT => self.handle_wm_getobject(wparam, lparam),
            _ => None,
        };
        if let Some(n) = handled {
            LRESULT(n)
        } else {
            unsafe { DefWindowProcW(handle, msg, wparam, lparam) }
        }
    }

    /// 等原始键鼠回调归还 App 后再排队菜单，避免模态菜单循环重入仍被借用的状态。
    ///
    /// @author kongweiguang
    fn handle_system_command_msg(
        &self,
        handle: HWND,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        if self.state.is_fullscreen() || self.state.closing.get() {
            return None;
        }

        let Some(command) = custom_caption_system_menu_command(self.hide_title_bar, wparam, lparam)
        else {
            return None;
        };
        let Some(position) = system_menu_tracking_position(handle, command, lparam) else {
            system_menu_diagnostics("request_failed", command);
            return Some(0);
        };

        system_menu_diagnostics("request", command);
        if let Err(error) = unsafe {
            PostMessageW(
                Some(handle),
                WM_GPUI_TRACK_SYSTEM_MENU,
                WPARAM(command as usize),
                position,
            )
        } {
            system_menu_diagnostics("queue_failed", command);
            log::error!("Failed to queue native system menu tracking: {error}");
        }

        // The selected command is posted back later, so this request must never fall through to a
        // default caption-menu path that may assume WS_CAPTION exists.
        Some(0)
    }

    /// 只在独立排队消息中进入 Win32 模态菜单；关闭或全屏使排队请求失效，选择异步回投系统命令。
    ///
    /// @author kongweiguang
    fn handle_track_system_menu_msg(
        &self,
        handle: HWND,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        if self.state.is_fullscreen() || self.state.closing.get() {
            return Some(0);
        }

        let source_command = wparam.0 as u32 & 0xFFF0;
        system_menu_diagnostics("tracking", source_command);
        let position = POINT {
            x: lparam.signed_loword() as i32,
            y: lparam.signed_hiword() as i32,
        };
        let selected_command = track_native_system_menu(
            handle,
            position,
            self.hide_title_bar,
            self.is_movable,
            self.is_resizable,
            self.is_minimizable,
        )
        .unwrap_or_default();
        system_menu_diagnostics("returned", selected_command);

        if selected_command != 0
            && let Err(error) = unsafe {
                PostMessageW(
                    Some(handle),
                    WM_SYSCOMMAND,
                    WPARAM(selected_command as usize),
                    LPARAM::default(),
                )
            }
        {
            log::error!("Failed to post selected native system command: {error}");
        }

        // A private queued message is consumed even when the window closed or no system menu remains.
        Some(0)
    }

    fn handle_move_msg(&self, handle: HWND, lparam: LPARAM) -> Option<isize> {
        let origin = logical_point(
            lparam.signed_loword() as f32,
            lparam.signed_hiword() as f32,
            self.state.scale_factor.get(),
        );
        self.state.origin.set(origin);
        let size = self.state.logical_size.get();
        let center_x = origin.x.as_f32() + size.width.as_f32() / 2.;
        let center_y = origin.y.as_f32() + size.height.as_f32() / 2.;
        let monitor_bounds = self.state.display.get().bounds();
        if center_x < monitor_bounds.left().as_f32()
            || center_x > monitor_bounds.right().as_f32()
            || center_y < monitor_bounds.top().as_f32()
            || center_y > monitor_bounds.bottom().as_f32()
        {
            // center of the window may have moved to another monitor
            let monitor = unsafe { MonitorFromWindow(handle, MONITOR_DEFAULTTONULL) };
            // minimize the window can trigger this event too, in this case,
            // monitor is invalid, we do nothing.
            if !monitor.is_invalid() && self.state.display.get().handle != monitor {
                // we will get the same monitor if we only have one
                self.set_display(WindowsDisplay::new(
                    WindowsDisplay::display_id_for_monitor(monitor),
                )?);
            }
        }
        if let Some(mut callback) = self.state.callbacks.moved.take() {
            callback();
            self.state.callbacks.moved.set(Some(callback));
        }
        Some(0)
    }

    fn handle_get_min_max_info_msg(&self, lparam: LPARAM) -> Option<isize> {
        let min_size = self.state.min_size?;
        let scale_factor = self.state.scale_factor.get();
        let boarder_offset = &self.state.border_offset;

        unsafe {
            let minmax_info = &mut *(lparam.0 as *mut MINMAXINFO);
            minmax_info.ptMinTrackSize.x = min_size.width.scale(scale_factor).as_f32() as i32
                + boarder_offset.width_offset.get();
            minmax_info.ptMinTrackSize.y = min_size.height.scale(scale_factor).as_f32() as i32
                + boarder_offset.height_offset.get();
        }
        Some(0)
    }

    fn handle_size_msg(self: &Rc<Self>, wparam: WPARAM, lparam: LPARAM) -> Option<isize> {
        // Minimizing and restoring both arrive as `WM_SIZE`; the deferred report
        // reads `IsIconic` at delivery, so one call covers both directions.
        self.report_visibility();

        // Don't resize the renderer when the window is minimized, but record that it was minimized so
        // that on restore the swap chain can be recreated via `update_drawable_size_even_if_unchanged`.
        if wparam.0 == SIZE_MINIMIZED as usize {
            self.state
                .restore_from_minimized
                .set(self.state.callbacks.request_frame.take());
            return Some(0);
        }

        let width = lparam.loword().max(1) as i32;
        let height = lparam.hiword().max(1) as i32;
        let new_size = size(DevicePixels(width), DevicePixels(height));

        let scale_factor = self.state.scale_factor.get();
        let mut should_resize_renderer = false;
        if let Some(restore_from_minimized) = self.state.restore_from_minimized.take() {
            self.state
                .callbacks
                .request_frame
                .set(Some(restore_from_minimized));
        } else {
            should_resize_renderer = true;
        }

        self.handle_size_change(new_size, scale_factor, should_resize_renderer);
        Some(0)
    }

    fn handle_size_change(
        &self,
        device_size: Size<DevicePixels>,
        scale_factor: f32,
        should_resize_renderer: bool,
    ) {
        let new_logical_size = device_size.to_pixels(scale_factor);

        self.state.logical_size.set(new_logical_size);
        if should_resize_renderer
            && let Err(e) = self.state.renderer.borrow_mut().resize(device_size)
        {
            log::error!("Failed to resize renderer, invalidating devices: {}", e);
            self.state
                .invalidate_devices
                .store(true, std::sync::atomic::Ordering::Release);
        }
        if let Some(mut callback) = self.state.callbacks.resize.take() {
            callback(new_logical_size, scale_factor);
            self.state.callbacks.resize.set(Some(callback));
        }
    }

    fn handle_size_move_loop(&self, handle: HWND) -> Option<isize> {
        unsafe {
            let ret = SetTimer(
                Some(handle),
                SIZE_MOVE_LOOP_TIMER_ID,
                USER_TIMER_MINIMUM,
                None,
            );
            if ret == 0 {
                log::error!(
                    "unable to create timer: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
        None
    }

    fn handle_size_move_loop_exit(&self, handle: HWND) -> Option<isize> {
        unsafe {
            KillTimer(Some(handle), SIZE_MOVE_LOOP_TIMER_ID).log_err();
        }
        None
    }

    fn handle_timer_msg(&self, handle: HWND, wparam: WPARAM) -> Option<isize> {
        if wparam.0 == SIZE_MOVE_LOOP_TIMER_ID {
            let mut runnables = self.main_receiver.clone().try_iter();
            while let Some(Ok(runnable)) = runnables.next() {
                WindowsDispatcher::execute_runnable(runnable);
            }
            self.handle_paint_msg(handle)
        } else {
            None
        }
    }

    fn handle_paint_msg(&self, handle: HWND) -> Option<isize> {
        self.draw_window(handle, false)
    }

    fn handle_close_msg(&self) -> Option<isize> {
        let mut callback = self.state.callbacks.should_close.take()?;
        let should_close = callback();
        self.state.callbacks.should_close.set(Some(callback));
        if should_close { None } else { Some(0) }
    }

    /// Close the frame wake gate before callbacks or HWND teardown can leave a stale request queued.
    ///
    /// @author kongweiguang
    fn handle_destroy_msg(&self, handle: HWND) -> Option<isize> {
        self.state.closing.set(true);
        let callback = { self.state.callbacks.close.take() };
        // Re-enable parent window if this was a modal dialog
        if let Some(parent_hwnd) = self.parent_hwnd {
            unsafe {
                let _ = EnableWindow(parent_hwnd, true);
                let _ = SetForegroundWindow(parent_hwnd);
            }
        }

        if let Some(callback) = callback {
            callback();
        }
        unsafe {
            PostMessageW(
                Some(self.platform_window_handle),
                WM_GPUI_CLOSE_ONE_WINDOW,
                WPARAM(self.validation_number),
                LPARAM(handle.0 as isize),
            )
            .log_err();
        }
        Some(0)
    }

    fn handle_mouse_move_msg(&self, handle: HWND, lparam: LPARAM, wparam: WPARAM) -> Option<isize> {
        self.start_tracking_mouse(handle, TME_LEAVE);
        self.restore_cursor_after_hide();

        let Some(mut func) = self.state.callbacks.input.take() else {
            return Some(1);
        };
        let scale_factor = self.state.scale_factor.get();

        let pressed_button = match MODIFIERKEYS_FLAGS(wparam.loword() as u32) {
            flags if flags.contains(MK_LBUTTON) => Some(MouseButton::Left),
            flags if flags.contains(MK_RBUTTON) => Some(MouseButton::Right),
            flags if flags.contains(MK_MBUTTON) => Some(MouseButton::Middle),
            flags if flags.contains(MK_XBUTTON1) => {
                Some(MouseButton::Navigate(NavigationDirection::Back))
            }
            flags if flags.contains(MK_XBUTTON2) => {
                Some(MouseButton::Navigate(NavigationDirection::Forward))
            }
            _ => None,
        };
        let x = lparam.signed_loword() as f32;
        let y = lparam.signed_hiword() as f32;
        let input = PlatformInput::MouseMove(MouseMoveEvent {
            position: logical_point(x, y, scale_factor),
            pressed_button,
            modifiers: current_modifiers(),
        });
        let handled = !func(input).propagate;
        self.state.callbacks.input.set(Some(func));

        if handled { Some(0) } else { Some(1) }
    }

    fn handle_mouse_leave_msg(&self) -> Option<isize> {
        self.state.hovered.set(false);
        // The next window's `WM_SETCURSOR` picks its own cursor, so we just clear
        // the flag for tight `is_cursor_visible()` semantics.
        self.state.cursor_visible.store(true, Ordering::Relaxed);
        if let Some(mut callback) = self.state.callbacks.hovered_status_change.take() {
            callback(false);
            self.state
                .callbacks
                .hovered_status_change
                .set(Some(callback));
        }

        Some(0)
    }

    fn handle_syskeyup_msg(&self, wparam: WPARAM, lparam: LPARAM) -> Option<isize> {
        let input = handle_key_event(wparam, lparam, &self.state, |keystroke, _| {
            PlatformInput::KeyUp(KeyUpEvent { keystroke })
        })?;
        let mut func = self.state.callbacks.input.take()?;

        func(input);
        self.state.callbacks.input.set(Some(func));

        // Always return 0 to indicate that the message was handled, so we could properly handle `ModifiersChanged` event.
        Some(0)
    }

    // It's a known bug that you can't trigger `ctrl-shift-0`. See:
    // https://superuser.com/questions/1455762/ctrl-shift-number-key-combination-has-stopped-working-for-a-few-numbers
    fn handle_keydown_msg(&self, wparam: WPARAM, lparam: LPARAM) -> Option<isize> {
        let Some(input) = handle_key_event(
            wparam,
            lparam,
            &self.state,
            |keystroke, prefer_character_input| {
                PlatformInput::KeyDown(KeyDownEvent {
                    keystroke,
                    is_held: lparam.0 & (0x1 << 30) > 0,
                    prefer_character_input,
                })
            },
        ) else {
            return Some(1);
        };

        let Some(mut func) = self.state.callbacks.input.take() else {
            return Some(1);
        };

        let handled = !func(input).propagate;

        self.state.callbacks.input.set(Some(func));

        if handled { Some(0) } else { Some(1) }
    }

    fn handle_keyup_msg(&self, wparam: WPARAM, lparam: LPARAM) -> Option<isize> {
        let Some(input) = handle_key_event(wparam, lparam, &self.state, |keystroke, _| {
            PlatformInput::KeyUp(KeyUpEvent { keystroke })
        }) else {
            return Some(1);
        };

        let Some(mut func) = self.state.callbacks.input.take() else {
            return Some(1);
        };

        let handled = !func(input).propagate;
        self.state.callbacks.input.set(Some(func));

        if handled { Some(0) } else { Some(1) }
    }

    fn handle_char_msg(&self, wparam: WPARAM) -> Option<isize> {
        let input = self.parse_char_message(wparam)?;
        self.with_input_handler(|input_handler| {
            input_handler.replace_text_in_range(None, &input);
        });

        Some(0)
    }

    fn handle_mouse_down_msg(
        &self,
        handle: HWND,
        button: MouseButton,
        lparam: LPARAM,
    ) -> Option<isize> {
        unsafe { SetCapture(handle) };

        let Some(mut func) = self.state.callbacks.input.take() else {
            return Some(1);
        };
        let x = lparam.signed_loword();
        let y = lparam.signed_hiword();
        let physical_point = point(DevicePixels(x as i32), DevicePixels(y as i32));
        let click_count = self.state.click_state.update(button, physical_point);
        let scale_factor = self.state.scale_factor.get();

        let input = PlatformInput::MouseDown(MouseDownEvent {
            button,
            position: logical_point(x as f32, y as f32, scale_factor),
            modifiers: current_modifiers(),
            click_count,
            first_mouse: false,
        });
        let handled = !func(input).propagate;
        self.state.callbacks.input.set(Some(func));

        if handled { Some(0) } else { Some(1) }
    }

    fn handle_mouse_up_msg(
        &self,
        _handle: HWND,
        button: MouseButton,
        lparam: LPARAM,
    ) -> Option<isize> {
        unsafe { ReleaseCapture().log_err() };

        let Some(mut func) = self.state.callbacks.input.take() else {
            return Some(1);
        };
        let x = lparam.signed_loword() as f32;
        let y = lparam.signed_hiword() as f32;
        let click_count = self.state.click_state.current_count.get();
        let scale_factor = self.state.scale_factor.get();

        let input = PlatformInput::MouseUp(MouseUpEvent {
            button,
            position: logical_point(x, y, scale_factor),
            modifiers: current_modifiers(),
            click_count,
        });
        let handled = !func(input).propagate;
        self.state.callbacks.input.set(Some(func));

        if handled { Some(0) } else { Some(1) }
    }

    fn handle_xbutton_msg(
        &self,
        handle: HWND,
        wparam: WPARAM,
        lparam: LPARAM,
        handler: impl Fn(&Self, HWND, MouseButton, LPARAM) -> Option<isize>,
    ) -> Option<isize> {
        let nav_dir = match wparam.hiword() {
            XBUTTON1 => NavigationDirection::Back,
            XBUTTON2 => NavigationDirection::Forward,
            _ => return Some(1),
        };
        handler(self, handle, MouseButton::Navigate(nav_dir), lparam)
    }

    fn handle_mouse_wheel_msg(
        &self,
        handle: HWND,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        let modifiers = current_modifiers();

        let Some(mut func) = self.state.callbacks.input.take() else {
            return Some(1);
        };
        let scale_factor = self.state.scale_factor.get();
        let wheel_scroll_amount = match modifiers.shift {
            true => self
                .system_settings()
                .mouse_wheel_settings
                .wheel_scroll_chars
                .get(),
            false => self
                .system_settings()
                .mouse_wheel_settings
                .wheel_scroll_lines
                .get(),
        };

        let wheel_distance =
            (wparam.signed_hiword() as f32 / WHEEL_DELTA as f32) * wheel_scroll_amount as f32;
        let mut cursor_point = POINT {
            x: lparam.signed_loword().into(),
            y: lparam.signed_hiword().into(),
        };
        unsafe { ScreenToClient(handle, &mut cursor_point).ok().log_err() };
        let input = PlatformInput::ScrollWheel(ScrollWheelEvent {
            position: logical_point(cursor_point.x as f32, cursor_point.y as f32, scale_factor),
            delta: ScrollDelta::Lines(match modifiers.shift {
                true => Point {
                    x: wheel_distance,
                    y: 0.0,
                },
                false => Point {
                    y: wheel_distance,
                    x: 0.0,
                },
            }),
            modifiers,
            touch_phase: TouchPhase::Moved,
        });
        let handled = !func(input).propagate;
        self.state.callbacks.input.set(Some(func));

        if handled { Some(0) } else { Some(1) }
    }

    fn handle_mouse_horizontal_wheel_msg(
        &self,
        handle: HWND,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        let Some(mut func) = self.state.callbacks.input.take() else {
            return Some(1);
        };
        let scale_factor = self.state.scale_factor.get();
        let wheel_scroll_chars = self
            .system_settings()
            .mouse_wheel_settings
            .wheel_scroll_chars
            .get();

        let wheel_distance =
            (-wparam.signed_hiword() as f32 / WHEEL_DELTA as f32) * wheel_scroll_chars as f32;
        let mut cursor_point = POINT {
            x: lparam.signed_loword().into(),
            y: lparam.signed_hiword().into(),
        };
        unsafe { ScreenToClient(handle, &mut cursor_point).ok().log_err() };
        let event = PlatformInput::ScrollWheel(ScrollWheelEvent {
            position: logical_point(cursor_point.x as f32, cursor_point.y as f32, scale_factor),
            delta: ScrollDelta::Lines(Point {
                x: wheel_distance,
                y: 0.0,
            }),
            modifiers: current_modifiers(),
            touch_phase: TouchPhase::Moved,
        });
        let handled = !func(event).propagate;
        self.state.callbacks.input.set(Some(func));

        if handled { Some(0) } else { Some(1) }
    }

    /// 捕获组合开始时的输入处理器及其 focus id；重复 START 保持原绑定，直到 END 释放。
    fn handle_ime_start_composition(&self, handle: HWND) -> Option<isize> {
        let started = self
            .state
            .ime_composition_handler
            .begin(&self.state.input_handler);
        if started {
            self.state.ime_pending_surrogate.set(None);
            self.state.ime_composition_focus_id.set(
                self.with_input_handler(|input_handler| input_handler.focus_id())
                    .flatten(),
            );
        }
        if let Some(caret_position) = self.retrieve_ime_caret_position() {
            self.update_ime_position(handle, caret_position);
        }
        // 开始消息仍交给 DefWindowProc 转发到系统 IME 窗口；固定槽只负责绑定后续文本回调。
        None
    }

    /// 先清理组合身份再释放固定目标，允许清理回调中安全建立下一次 START。
    fn handle_ime_end_composition(&self) -> Option<isize> {
        self.state.ime_composition_focus_id.set(None);
        self.state.ime_pending_surrogate.set(None);
        self.state
            .ime_composition_handler
            .end(PlatformInputHandler::unmark_text);
        None
    }

    /// 将提交送回组合开始时的处理器；opt-in 诊断只保留 handler 存在位与 UTF-16 长度，不扩大正文日志边界。
    fn handle_ime_char_msg(&self, wparam: WPARAM) -> Option<isize> {
        if !self.state.ime_composition_handler.has_handler() {
            if ime_diagnostics_enabled() {
                eprintln!("KERMINAL_IME_DIAGNOSTIC phase=100 handler=0");
            }
            self.state.ime_pending_surrogate.set(None);
            return Some(0);
        }
        let Some(input) = self.parse_ime_char_message(wparam) else {
            return Some(0);
        };
        if ime_diagnostics_enabled() {
            eprintln!(
                "KERMINAL_IME_DIAGNOSTIC phase=101 handler=1 utf16_units={}",
                input.encode_utf16().count()
            );
        }
        self.state
            .ime_composition_handler
            .with(|input_handler| input_handler.replace_text_in_range(None, &input));
        Some(0)
    }

    /// 查询固定组合处理器的光标位置，不能读取切换后的当前输入目标。
    fn retrieve_ime_caret_position(&self) -> Option<POINT> {
        self.with_ime_composition_handler(|input_handler| {
            let caret_range = input_handler.selected_text_range(false)?;
            let caret_position = input_handler.bounds_for_range(caret_range.range)?;
            let scale_factor = self.state.scale_factor.get();
            Some(POINT {
                x: (caret_position.origin.x.as_f32() * scale_factor) as i32,
                y: (caret_position.origin.y.as_f32() * scale_factor) as i32
                    + ((caret_position.size.height.as_f32() * scale_factor) as i32 / 2),
            })
        })
        .flatten()
    }

    pub(crate) fn update_ime_position(&self, handle: HWND, caret_position: POINT) {
        let Some(ctx) = ImeContext::get(handle) else {
            return;
        };
        unsafe {
            ImmSetCompositionWindow(
                *ctx,
                &COMPOSITIONFORM {
                    dwStyle: CFS_POINT,
                    ptCurrentPos: caret_position,
                    ..Default::default()
                },
            )
            .ok()
            .log_err();

            ImmSetCandidateWindow(
                *ctx,
                &CANDIDATEFORM {
                    dwStyle: CFS_CANDIDATEPOS,
                    ptCurrentPos: caret_position,
                    ..Default::default()
                },
            )
            .ok()
            .log_err();
        }
    }

    /// 帧回调结束后以已提交文本目标决定 IME 状态；没有目标时取消旧组合。
    fn update_ime_enabled(&self, handle: HWND) {
        let current_ime_enabled =
            self.with_input_handler(|input_handler| input_handler.query_accepts_text_input());
        if self.state.ime_composition_handler.is_active() && current_ime_enabled != Some(true) {
            // 帧回调完成后仍没有可接收文本的目标，才把空槽视为真实焦点变化。
            self.cancel_ime_composition(handle);
        }
        let ime_enabled = current_ime_enabled
            .or_else(|| {
                self.state
                    .ime_composition_handler
                    .with(|input_handler| input_handler.query_accepts_text_input())
            })
            .unwrap_or(false);
        if ime_enabled == self.state.ime_enabled.get() {
            return;
        }
        self.state.ime_enabled.set(ime_enabled);
        unsafe {
            if ime_enabled {
                ImmAssociateContextEx(handle, HIMC::default(), IACE_DEFAULT)
                    .ok()
                    .log_err();
            } else {
                ImmAssociateContextEx(handle, HIMC::default(), 0)
                    .ok()
                    .log_err();
            }
        }
    }

    /// 只接受由 START 锁定的目标；缺少 START/目标时消费消息，避免默认过程重投到新焦点。
    fn handle_ime_composition(&self, handle: HWND, lparam: LPARAM) -> Option<isize> {
        if !self.state.ime_composition_handler.has_handler() {
            return Some(0);
        }
        let Some(ctx) = ImeContext::get(handle) else {
            self.state
                .ime_composition_handler
                .cancel(PlatformInputHandler::unmark_text);
            self.state.ime_pending_surrogate.set(None);
            return Some(0);
        };
        self.handle_ime_composition_inner(*ctx, lparam)
    }

    /// 固定 handler 处理结果、预编辑和取消；无 GCS 标志按 Windows 契约清除，诊断不输出结果正文。
    fn handle_ime_composition_inner(&self, ctx: HIMC, lparam: LPARAM) -> Option<isize> {
        let lparam = lparam.0 as u32;
        if ime_diagnostics_enabled() {
            eprintln!(
                "KERMINAL_IME_DIAGNOSTIC phase=102 gcs_flags={lparam} handler={}",
                u8::from(self.state.ime_composition_handler.has_handler())
            );
        }
        if lparam == 0 {
            // Japanese IME may send this message with lparam = 0, which indicates that
            // there is no composition string.
            self.state
                .ime_composition_handler
                .cancel(PlatformInputHandler::unmark_text);
            self.state.ime_pending_surrogate.set(None);
            Some(0)
        } else {
            const GCS_FLAGS: u32 = GCS_COMPATTR.0
                | GCS_COMPCLAUSE.0
                | GCS_COMPREADSTR.0
                | GCS_COMPREADATTR.0
                | GCS_COMPREADCLAUSE.0
                | GCS_COMPSTR.0
                | GCS_CURSORPOS.0
                | GCS_DELTASTART.0
                | GCS_RESULTCLAUSE.0
                | GCS_RESULTREADCLAUSE.0
                | GCS_RESULTREADSTR.0
                | GCS_RESULTSTR.0;

            if ime_diagnostics_enabled() {
                eprintln!(
                    "KERMINAL_IME_DIAGNOSTIC phase=103 gcs_flags={} result={} preedit={} handler={}",
                    lparam & GCS_FLAGS,
                    u8::from(lparam & GCS_RESULTSTR.0 > 0),
                    u8::from(lparam & GCS_COMPSTR.0 > 0),
                    u8::from(self.state.ime_composition_handler.has_handler())
                );
            }

            if lparam & GCS_FLAGS == 0 {
                // Windows 将没有 GCS 标志的 composition 明确定义为取消，不能按 wParam 提交字符。
                self.state
                    .ime_composition_handler
                    .cancel(PlatformInputHandler::unmark_text);
                self.state.ime_pending_surrogate.set(None);
                return Some(0);
            }

            if lparam & GCS_RESULTSTR.0 > 0 {
                let Some(comp_result) = parse_ime_composition_string(ctx, GCS_RESULTSTR) else {
                    self.state
                        .ime_composition_handler
                        .cancel(PlatformInputHandler::unmark_text);
                    self.state.ime_pending_surrogate.set(None);
                    return Some(0);
                };
                if ime_diagnostics_enabled() {
                    eprintln!(
                        "KERMINAL_IME_DIAGNOSTIC phase=104 utf16_units={}",
                        comp_result.len()
                    );
                }
                let Some(()) = self.state.ime_composition_handler.with(|input_handler| {
                    input_handler
                        .replace_text_in_range(None, &String::from_utf16_lossy(&comp_result));
                }) else {
                    return Some(0);
                };
            }
            if lparam & GCS_COMPSTR.0 > 0 {
                let Some(comp_string) = parse_ime_composition_string(ctx, GCS_COMPSTR) else {
                    self.state
                        .ime_composition_handler
                        .cancel(PlatformInputHandler::unmark_text);
                    self.state.ime_pending_surrogate.set(None);
                    return Some(0);
                };
                if ime_diagnostics_enabled() {
                    eprintln!(
                        "KERMINAL_IME_DIAGNOSTIC phase=105 utf16_units={}",
                        comp_string.len()
                    );
                }
                let caret_pos =
                    (!comp_string.is_empty() && lparam & GCS_CURSORPOS.0 > 0).then(|| {
                        let cursor_pos = retrieve_composition_cursor_position(ctx);
                        let pos = if should_use_ime_cursor_position(ctx, cursor_pos) {
                            cursor_pos
                        } else {
                            comp_string.len()
                        };
                        pos..pos
                    });
                let Some(()) = self.state.ime_composition_handler.with(|input_handler| {
                    input_handler.replace_and_mark_text_in_range(
                        None,
                        &String::from_utf16_lossy(&comp_string),
                        caret_pos,
                    );
                }) else {
                    return Some(0);
                };
            }
            if lparam & (GCS_RESULTSTR.0 | GCS_COMPSTR.0) > 0 {
                return Some(0);
            }

            // Composition metadata outside COMPSTR/RESULTSTR is left to the default IME window.
            None
        }
    }

    fn handle_calc_client_size(
        &self,
        handle: HWND,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        if !self.hide_title_bar || self.state.is_fullscreen() || wparam.0 == 0 {
            return None;
        }

        unsafe {
            let params = lparam.0 as *mut NCCALCSIZE_PARAMS;
            let saved_top = (*params).rgrc[0].top;
            let result = DefWindowProcW(handle, WM_NCCALCSIZE, wparam, lparam);
            (*params).rgrc[0].top = saved_top;
            if self.state.is_maximized() {
                let dpi = GetDpiForWindow(handle);
                (*params).rgrc[0].top += get_frame_thicknessx(dpi);
            }
            Some(result.0 as isize)
        }
    }

    /// 窗口离开 active 状态时先取消组合，避免系统焦点切换提交到旧控件。
    fn handle_activate_msg(self: &Rc<Self>, wparam: WPARAM) -> Option<isize> {
        let activated = wparam.loword() > 0;

        if !activated {
            self.cancel_ime_composition(self.hwnd());
        }

        let events = self
            .state
            .a11y
            .try_borrow_mut()
            .ok()
            .and_then(|mut a11y| a11y.as_mut()?.adapter.update_window_focus_state(activated));
        if let Some(events) = events {
            events.raise();
        }

        let this = self.clone();

        if !activated {
            this.state.cursor_visible.store(true, Ordering::Relaxed);
        }

        // When the window is activated (gains focus), reset the modifier tracking state.
        // This fixes the issue where Alt-Tab away and back leaves stale modifier state
        // (especially the Alt key) because Windows doesn't always send key-up events to
        // windows that have lost focus.
        if activated {
            this.state.last_reported_modifiers.set(None);
            this.state.last_reported_capslock.set(None);

            if let Some(mut func) = this.state.callbacks.input.take() {
                let input = PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                    modifiers: current_modifiers(),
                    capslock: current_capslock(),
                });
                func(input);
                this.state.callbacks.input.set(Some(func));
            }
        }

        self.executor
            .spawn(async move {
                if let Some(mut func) = this.state.callbacks.active_status_change.take() {
                    func(activated);
                    this.state.callbacks.active_status_change.set(Some(func));
                }
            })
            .detach();

        None
    }

    /// 原生键盘焦点离开窗口时清理组合，避免系统继续向已失焦控件投递结果。
    fn handle_kill_focus_msg(&self, handle: HWND) -> Option<isize> {
        self.cancel_ime_composition(handle);
        None
    }

    /// 先失效并 unmark 固定目标，再在所有 handler 借用结束后请求 IME 取消组合。
    pub(crate) fn cancel_ime_composition(&self, handle: HWND) {
        self.state.ime_composition_focus_id.set(None);
        self.state.ime_pending_surrogate.set(None);
        if !self.state.ime_composition_handler.is_active() {
            return;
        }

        self.state
            .ime_composition_handler
            .end(PlatformInputHandler::unmark_text);

        // unmark 回调可能重入并开始新组合；旧取消请求不能伤及新 START。
        if self.state.ime_composition_handler.is_active() {
            return;
        }

        let Some(context) = ImeContext::get(handle) else {
            return;
        };
        unsafe {
            ImmNotifyIME(*context, NI_COMPOSITIONSTR, CPS_CANCEL, 0)
                .ok()
                .log_err();
        }
    }

    fn handle_wm_getobject(&self, wparam: WPARAM, lparam: LPARAM) -> Option<isize> {
        let result = {
            let mut a11y = self.state.a11y.borrow_mut();
            let a11y = a11y.as_mut()?;
            a11y.adapter.handle_wm_getobject(
                accesskit_windows::WPARAM(wparam.0),
                accesskit_windows::LPARAM(lparam.0),
                &mut a11y.activation_handler,
            )?
        };
        // The borrow above must be dropped before calling `.into()`, because
        // it calls `UiaReturnRawElementProvider` which may send a nested
        // `WM_GETOBJECT` back into this window procedure.
        let lresult: accesskit_windows::LRESULT = result.into();
        Some(lresult.0)
    }

    fn handle_create_msg(&self, handle: HWND) -> Option<isize> {
        if self.hide_title_bar {
            notify_frame_changed(handle);
            Some(0)
        } else {
            None
        }
    }

    fn handle_dpi_changed_msg(
        &self,
        handle: HWND,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        let new_dpi = wparam.loword() as f32;

        let is_maximized = self.state.is_maximized();
        let new_scale_factor = new_dpi / USER_DEFAULT_SCREEN_DPI as f32;
        self.state.scale_factor.set(new_scale_factor);
        self.state.border_offset.update(handle).log_err();

        self.state
            .direct_manipulation
            .set_scale_factor(new_scale_factor);

        if is_maximized {
            // Get the monitor and its work area at the new DPI
            let monitor = unsafe { MonitorFromWindow(handle, MONITOR_DEFAULTTONEAREST) };
            let mut monitor_info: MONITORINFO = unsafe { std::mem::zeroed() };
            monitor_info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if unsafe { GetMonitorInfoW(monitor, &mut monitor_info) }.as_bool() {
                let work_area = monitor_info.rcWork;
                let width = work_area.right - work_area.left;
                let height = work_area.bottom - work_area.top;

                // Update the window size to match the new monitor work area
                // This will trigger WM_SIZE which will handle the size change
                unsafe {
                    SetWindowPos(
                        handle,
                        None,
                        work_area.left,
                        work_area.top,
                        width,
                        height,
                        SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                    )
                    .context("unable to set maximized window position after dpi has changed")
                    .log_err();
                }

                // SetWindowPos may not send WM_SIZE for maximized windows in some cases,
                // so we manually update the size to ensure proper rendering
                let device_size = size(DevicePixels(width), DevicePixels(height));
                self.handle_size_change(device_size, new_scale_factor, true);
            }
        } else {
            // For non-maximized windows, use the suggested RECT from the system
            let rect = unsafe { &*(lparam.0 as *const RECT) };
            let width = rect.right - rect.left;
            let height = rect.bottom - rect.top;
            // this will emit `WM_SIZE` and `WM_MOVE` right here
            // even before this function returns
            // the new size is handled in `WM_SIZE`
            unsafe {
                SetWindowPos(
                    handle,
                    None,
                    rect.left,
                    rect.top,
                    width,
                    height,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
                .context("unable to set window position after dpi has changed")
                .log_err();
            }
        }

        Some(0)
    }

    fn handle_display_change_msg(&self, handle: HWND) -> Option<isize> {
        let new_monitor = unsafe { MonitorFromWindow(handle, MONITOR_DEFAULTTONULL) };
        if new_monitor.is_invalid() {
            log::error!("No monitor detected!");
            return None;
        }
        let new_display = WindowsDisplay::new(WindowsDisplay::display_id_for_monitor(new_monitor))?;
        self.set_display(new_display);
        Some(0)
    }

    fn set_display(&self, display: WindowsDisplay) {
        self.state.display.set(display);
        if let Some(mut callback) = self.state.callbacks.display_changed.take() {
            callback();
            self.state.callbacks.display_changed.set(Some(callback));
        }
    }

    /// 先读取 GPUI 控件区域再映射原生命中码，避免把不可用按钮误当标题空白。
    ///
    /// @author kongweiguang
    fn handle_hit_test_msg(&self, handle: HWND, lparam: LPARAM) -> Option<isize> {
        if self.state.is_fullscreen() {
            return None;
        }

        let drag_area = self
            .hit_test_window_control_area()
            .and_then(|area| match area {
                WindowControlArea::Drag if self.is_movable => Some(HTCAPTION as _),
                WindowControlArea::Drag => None,
                WindowControlArea::Close => Some(HTCLOSE as _),
                WindowControlArea::Max if self.is_resizable => Some(HTMAXBUTTON as _),
                WindowControlArea::Max if self.is_movable => Some(HTCAPTION as _),
                WindowControlArea::Max => Some(HTNOWHERE as _),
                WindowControlArea::Min if self.is_minimizable => Some(HTMINBUTTON as _),
                WindowControlArea::Min if self.is_movable => Some(HTCAPTION as _),
                WindowControlArea::Min => Some(HTNOWHERE as _),
            });

        if !self.hide_title_bar {
            // If the OS draws the title bar, we don't need to handle hit test messages.
            return drag_area;
        }

        let dpi = unsafe { GetDpiForWindow(handle) };
        // We do not use the OS title bar, so the default `DefWindowProcW` will only register a 1px edge for resizes
        // We need to calculate the frame thickness ourselves and do the hit test manually.
        let frame_y = get_frame_thicknessx(dpi);
        let frame_x = get_frame_thicknessy(dpi);
        let mut cursor_point = POINT {
            x: lparam.signed_loword().into(),
            y: lparam.signed_hiword().into(),
        };

        unsafe { ScreenToClient(handle, &mut cursor_point).ok().log_err() };
        if self.is_resizable
            && !self.state.is_maximized()
            && 0 <= cursor_point.y
            && cursor_point.y <= frame_y
        {
            // x-axis actually goes from -frame_x to 0
            return Some(if cursor_point.x <= 0 {
                HTTOPLEFT
            } else {
                let mut rect = Default::default();
                unsafe { GetWindowRect(handle, &mut rect) }.log_err();
                // right and bottom bounds of RECT are exclusive, thus `-1`
                let right = rect.right - rect.left - 1;
                // the bounds include the padding frames, so accommodate for both of them
                if right - 2 * frame_x <= cursor_point.x {
                    HTTOPRIGHT
                } else {
                    HTTOP
                }
            } as _);
        }

        drag_area
    }

    fn handle_nc_mouse_move_msg(&self, handle: HWND, lparam: LPARAM) -> Option<isize> {
        self.start_tracking_mouse(handle, TME_LEAVE | TME_NONCLIENT);
        self.restore_cursor_after_hide();

        let mut func = self.state.callbacks.input.take()?;
        let scale_factor = self.state.scale_factor.get();

        let mut cursor_point = POINT {
            x: lparam.signed_loword().into(),
            y: lparam.signed_hiword().into(),
        };
        unsafe { ScreenToClient(handle, &mut cursor_point).ok().log_err() };
        let input = PlatformInput::MouseMove(MouseMoveEvent {
            position: logical_point(cursor_point.x as f32, cursor_point.y as f32, scale_factor),
            pressed_button: None,
            modifiers: current_modifiers(),
        });
        let handled = !func(input).propagate;
        self.state.callbacks.input.set(Some(func));

        if handled { Some(0) } else { None }
    }

    fn handle_nc_mouse_down_msg(
        &self,
        handle: HWND,
        button: MouseButton,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        if let Some(mut func) = self.state.callbacks.input.take() {
            let scale_factor = self.state.scale_factor.get();
            let mut cursor_point = POINT {
                x: lparam.signed_loword().into(),
                y: lparam.signed_hiword().into(),
            };
            unsafe { ScreenToClient(handle, &mut cursor_point).ok().log_err() };
            let physical_point = point(DevicePixels(cursor_point.x), DevicePixels(cursor_point.y));
            let click_count = self.state.click_state.update(button, physical_point);

            let input = PlatformInput::MouseDown(MouseDownEvent {
                button,
                position: logical_point(cursor_point.x as f32, cursor_point.y as f32, scale_factor),
                modifiers: current_modifiers(),
                click_count,
                first_mouse: false,
            });
            let handled = !func(input).propagate;
            self.state.callbacks.input.set(Some(func));

            if handled {
                return Some(0);
            }
        } else {
        };

        // Since these are handled in handle_nc_mouse_up_msg we must prevent the default window proc
        if button == MouseButton::Left {
            match wparam.0 as u32 {
                HTMINBUTTON => self.state.nc_button_pressed.set(Some(HTMINBUTTON)),
                HTMAXBUTTON => self.state.nc_button_pressed.set(Some(HTMAXBUTTON)),
                HTCLOSE => self.state.nc_button_pressed.set(Some(HTCLOSE)),
                _ => return None,
            };
            Some(0)
        } else {
            None
        }
    }

    /// 先完成 GPUI 输入分发，再异步投递系统菜单，避免模态跟踪重入借用中的 App 状态。
    ///
    /// @author kongweiguang
    fn handle_nc_mouse_up_msg(
        &self,
        handle: HWND,
        button: MouseButton,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        let custom_caption_right_click = !self.state.is_fullscreen()
            && is_custom_caption_right_click(self.hide_title_bar, button, wparam.0);
        // Snapshot the pre-dispatch hit area so closing a modal/overlay on mouse-up cannot expose
        // the drag region underneath and accidentally open its system menu.
        let control_area = if custom_caption_right_click {
            self.hit_test_window_control_area()
        } else {
            None
        };

        let handled = if let Some(mut func) = self.state.callbacks.input.take() {
            let scale_factor = self.state.scale_factor.get();

            let mut cursor_point = POINT {
                x: lparam.signed_loword().into(),
                y: lparam.signed_hiword().into(),
            };
            unsafe { ScreenToClient(handle, &mut cursor_point).ok().log_err() };
            let input = PlatformInput::MouseUp(MouseUpEvent {
                button,
                position: logical_point(cursor_point.x as f32, cursor_point.y as f32, scale_factor),
                modifiers: current_modifiers(),
                click_count: 1,
            });
            let handled = !func(input).propagate;
            self.state.callbacks.input.set(Some(func));
            handled
        } else {
            false
        };

        if custom_caption_right_click {
            if should_show_custom_caption_system_menu(
                self.hide_title_bar,
                button,
                wparam.0,
                control_area,
            ) {
                // 事件回调结束后再让系统跟踪菜单，避免同步模态循环重入仍在借用的 App/RefCell。
                show_native_system_menu_at_screen_position(handle, lparam);
            }
            // HTCAPTION 也可能来自不可用的标题栏按钮；消费非空白区释放事件，阻止默认处理穿过控件或遮罩。
            return Some(0);
        }

        if handled {
            return Some(0);
        }

        let last_pressed = self.state.nc_button_pressed.take();
        if button == MouseButton::Left
            && let Some(last_pressed) = last_pressed
        {
            let handled = match (wparam.0 as u32, last_pressed) {
                (HTMINBUTTON, HTMINBUTTON) if self.is_minimizable => {
                    unsafe { ShowWindowAsync(handle, SW_MINIMIZE).ok().log_err() };
                    true
                }
                (HTMINBUTTON, HTMINBUTTON) => true,
                (HTMAXBUTTON, HTMAXBUTTON) if self.is_resizable => {
                    if self.state.is_maximized() {
                        unsafe { ShowWindowAsync(handle, SW_NORMAL).ok().log_err() };
                    } else {
                        unsafe { ShowWindowAsync(handle, SW_MAXIMIZE).ok().log_err() };
                    }
                    true
                }
                (HTMAXBUTTON, HTMAXBUTTON) => true,
                (HTCLOSE, HTCLOSE) => {
                    unsafe {
                        PostMessageW(Some(handle), WM_CLOSE, WPARAM::default(), LPARAM::default())
                            .log_err()
                    };
                    true
                }
                _ => false,
            };
            if handled {
                return Some(0);
            }
        }

        None
    }

    /// 读取当前标题栏命中区域并立即归还回调槽，确保遮罩和控件使用最新 GPUI 命中结果。
    ///
    /// @author kongweiguang
    fn hit_test_window_control_area(&self) -> Option<WindowControlArea> {
        let mut callback = self.state.callbacks.hit_test_window_control.take()?;
        let area = callback();
        self.state
            .callbacks
            .hit_test_window_control
            .set(Some(callback));
        area
    }

    fn handle_cursor_changed(&self, lparam: LPARAM) -> Option<isize> {
        let had_cursor = self.state.current_cursor.get().is_some();

        self.state.current_cursor.set(if lparam.0 == 0 {
            None
        } else {
            Some(HCURSOR(lparam.0 as _))
        });

        if had_cursor != self.state.current_cursor.get().is_some() {
            unsafe { SetCursor(self.state.current_cursor.get()) };
        }

        Some(0)
    }

    fn handle_set_cursor(&self, handle: HWND, lparam: LPARAM) -> Option<isize> {
        if unsafe { !IsWindowEnabled(handle).as_bool() }
            || matches!(
                lparam.loword() as u32,
                HTLEFT
                    | HTRIGHT
                    | HTTOP
                    | HTTOPLEFT
                    | HTTOPRIGHT
                    | HTBOTTOM
                    | HTBOTTOMLEFT
                    | HTBOTTOMRIGHT
            )
        {
            return None;
        }
        let cursor = if self.state.cursor_visible.load(Ordering::Relaxed) {
            self.state.current_cursor.get()
        } else {
            None
        };
        unsafe {
            SetCursor(cursor);
        };
        Some(0)
    }

    fn handle_system_settings_changed(
        &self,
        handle: HWND,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        if wparam.0 != 0 {
            self.state.click_state.system_update(wparam.0);
            self.state.border_offset.update(handle).log_err();
            // system settings may emit a window message which wants to take the refcell self.state, so drop it

            self.system_settings().update(wparam.0);
        } else {
            self.handle_system_theme_changed(handle, lparam)?;
        };

        Some(0)
    }

    fn handle_system_theme_changed(&self, handle: HWND, lparam: LPARAM) -> Option<isize> {
        // lParam is a pointer to a string that indicates the area containing the system parameter
        // that was changed.
        let parameter = PCWSTR::from_raw(lparam.0 as _);
        if unsafe { !parameter.is_null() && !parameter.is_empty() }
            && let Some(parameter_string) = unsafe { parameter.to_string() }.log_err()
        {
            log::info!("System settings changed: {}", parameter_string);
            if parameter_string.as_str() == "ImmersiveColorSet" {
                let new_appearance = system_appearance()
                    .context("unable to get system appearance when handling ImmersiveColorSet")
                    .log_err()?;

                if new_appearance != self.state.appearance.get() {
                    self.state.appearance.set(new_appearance);
                    let mut callback = self.state.callbacks.appearance_changed.take()?;

                    callback();
                    self.state.callbacks.appearance_changed.set(Some(callback));
                    configure_dwm_dark_mode(handle, new_appearance);
                }
            }
        }
        Some(0)
    }

    fn handle_input_language_changed(&self) -> Option<isize> {
        unsafe {
            PostMessageW(
                Some(self.platform_window_handle),
                WM_GPUI_KEYBOARD_LAYOUT_CHANGED,
                WPARAM(self.validation_number),
                LPARAM(0),
            )
            .log_err();
        }
        Some(0)
    }

    fn handle_window_visibility_changed(
        self: &Rc<Self>,
        handle: HWND,
        wparam: WPARAM,
    ) -> Option<isize> {
        self.report_visibility();
        if wparam.0 == 1 {
            self.draw_window(handle, false);
        }
        None
    }

    // The window procedure can run while GPUI is updating this window (e.g.
    // `ShowWindow` from an action handler), so deliver observers after that
    // update completes, as activation does. The state is read at delivery so
    // a burst of messages collapses to the final value.
    fn report_visibility(self: &Rc<Self>) {
        if self.state.last_visibility.get().is_none() {
            return;
        }
        let this = self.clone();
        self.executor
            .spawn(async move {
                let visibility = this.visibility();
                if this.state.last_visibility.get() == Some(visibility) {
                    return;
                }
                this.state.last_visibility.set(Some(visibility));
                if let Some(mut callback) = this.state.callbacks.visibility_change.take() {
                    callback(visibility);
                    this.state.callbacks.visibility_change.set(Some(callback));
                }
            })
            .detach();
    }

    fn handle_device_lost(&self, lparam: LPARAM) -> Option<isize> {
        let devices = lparam.0 as *const DirectXDevices;
        let devices = unsafe { &*devices };
        if let Err(err) = self
            .state
            .renderer
            .borrow_mut()
            .handle_device_lost(&devices)
        {
            panic!("Device lost: {err}");
        }
        // Make sure the first `draw_window` after recovery (whether it comes
        // from the forced WM_GPUI_FORCE_UPDATE_WINDOW or a stray WM_PAINT in
        // between) is treated as a forced render so it both clears
        // `skip_draws` and bypasses the view cache.
        self.state.force_render_pending.set(true);
        Some(0)
    }

    fn handle_dm_pointer_hit_test(&self, wparam: WPARAM) -> Option<isize> {
        self.state.direct_manipulation.on_pointer_hit_test(wparam);
        None
    }

    #[inline]
    fn draw_window(&self, handle: HWND, force_render: bool) -> Option<isize> {
        let Some(_guard) = self.state.draw_coordinator.try_begin_draw() else {
            log::debug!("deferring re-entrant draw of window {handle:?}");
            if force_render {
                self.state.force_render_pending.set(true);
            }
            // Validate the region so a nested message pump doesn't keep
            // re-dispatching WM_PAINT for the still-invalid region in a busy
            // loop until the in-progress draw unwinds. The vsync thread
            // re-invalidates every window on each vsync (see
            // `begin_vsync_thread`), so the deferred frame still gets drawn,
            // at most one vsync late.
            unsafe { ValidateRect(Some(handle), None).ok().log_err() };
            return Some(0);
        };
        let mut request_frame = self.state.callbacks.request_frame.take()?;

        self.state.direct_manipulation.update();

        let events = self.state.direct_manipulation.drain_events();
        if !events.is_empty() {
            if let Some(mut func) = self.state.callbacks.input.take() {
                for event in events {
                    func(event);
                }
                self.state.callbacks.input.set(Some(func));
            }
        }

        let force_render = force_render || self.state.force_render_pending.take();
        if force_render {
            // Re-enable drawing after a device loss recovery. The forced render
            // will rebuild the scene with fresh atlas textures.
            self.state.renderer.borrow_mut().mark_drawable();
        }
        let (signal_at, signal_source) = self.state.frame_signal.take().map_or(
            (None, FrameRequestSource::NativeCallback),
            |(at, source)| (Some(at), source),
        );
        request_frame(RequestFrameOptions {
            require_presentation: false,
            force_render,
            signal_at,
            signal_source,
        });

        self.state.callbacks.request_frame.set(Some(request_frame));
        self.update_ime_enabled(handle);
        unsafe { ValidateRect(Some(handle), None).ok().log_err() };

        Some(0)
    }

    #[inline]
    /// 普通 WM_CHAR 保持独立 surrogate 状态，不与 IME 组合字符相互配对。
    fn parse_char_message(&self, wparam: WPARAM) -> Option<String> {
        self.parse_char_message_with_pending(wparam, &self.state.pending_surrogate)
    }

    /// 使用指定 surrogate 状态解析 UTF-16 单元，区分普通文本与 IME 提交流。
    fn parse_char_message_with_pending(
        &self,
        wparam: WPARAM,
        pending_surrogate: &Cell<Option<u16>>,
    ) -> Option<String> {
        let code_point = wparam.loword();

        // https://www.unicode.org/versions/Unicode16.0.0/core-spec/chapter-3/#G2630
        match code_point {
            0xD800..=0xDBFF => {
                // High surrogate, wait for low surrogate
                pending_surrogate.set(Some(code_point));
                None
            }
            0xDC00..=0xDFFF => {
                if let Some(high_surrogate) = pending_surrogate.take() {
                    // Low surrogate, combine with pending high surrogate
                    String::from_utf16(&[high_surrogate, code_point]).ok()
                } else {
                    // Invalid low surrogate without a preceding high surrogate
                    log::warn!(
                        "Received low surrogate without a preceding high surrogate: {code_point:x}"
                    );
                    None
                }
            }
            _ => {
                pending_surrogate.set(None);
                char::from_u32(code_point as u32)
                    .filter(|c| !c.is_control())
                    .map(|c| c.to_string())
            }
        }
    }

    /// Clear the hidden flag and restore the cursor immediately
    fn restore_cursor_after_hide(&self) {
        if !self.state.cursor_visible.swap(true, Ordering::Relaxed) {
            unsafe {
                SetCursor(self.state.current_cursor.get());
            }
        }
    }

    fn start_tracking_mouse(&self, handle: HWND, flags: TRACKMOUSEEVENT_FLAGS) {
        if !self.state.hovered.get() {
            self.state.hovered.set(true);
            unsafe {
                TrackMouseEvent(&mut TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: flags,
                    hwndTrack: handle,
                    dwHoverTime: HOVER_DEFAULT,
                })
                .log_err()
            };
            if let Some(mut callback) = self.state.callbacks.hovered_status_change.take() {
                callback(true);
                self.state
                    .callbacks
                    .hovered_status_change
                    .set(Some(callback));
            }
        }
    }

    fn with_input_handler<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut PlatformInputHandler) -> R,
    {
        let mut input_handler = self.state.input_handler.take()?;
        let result = f(&mut input_handler);
        self.state.input_handler.set(Some(input_handler));
        Some(result)
    }

    /// 临时取出固定 IME handler 回调后放回原槽，不与普通 WM_CHAR 路径共用。
    fn with_ime_composition_handler<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut PlatformInputHandler) -> R,
    {
        self.state.ime_composition_handler.with(f)
    }

    /// 解析 IME 字符时使用独立 surrogate 槽，使组合取消不污染普通字符路径。
    fn parse_ime_char_message(&self, wparam: WPARAM) -> Option<String> {
        self.parse_char_message_with_pending(wparam, &self.state.ime_pending_surrogate)
    }
}

struct ImeContext {
    hwnd: HWND,
    himc: HIMC,
}

impl ImeContext {
    fn get(hwnd: HWND) -> Option<Self> {
        let himc = unsafe { ImmGetContext(hwnd) };
        if himc.is_invalid() {
            return None;
        }
        Some(Self { hwnd, himc })
    }
}

impl std::ops::Deref for ImeContext {
    type Target = HIMC;
    fn deref(&self) -> &HIMC {
        &self.himc
    }
}

impl Drop for ImeContext {
    fn drop(&mut self) {
        unsafe {
            ImmReleaseContext(self.hwnd, self.himc).ok().log_err();
        }
    }
}

fn handle_key_event<F>(
    wparam: WPARAM,
    lparam: LPARAM,
    state: &WindowsWindowState,
    f: F,
) -> Option<PlatformInput>
where
    F: FnOnce(Keystroke, bool) -> PlatformInput,
{
    let virtual_key = VIRTUAL_KEY(wparam.loword());
    let modifiers = current_modifiers();

    match virtual_key {
        VK_SHIFT | VK_CONTROL | VK_MENU | VK_LMENU | VK_RMENU | VK_LWIN | VK_RWIN => {
            if state
                .last_reported_modifiers
                .get()
                .is_some_and(|prev_modifiers| prev_modifiers == modifiers)
            {
                return None;
            }
            state.last_reported_modifiers.set(Some(modifiers));
            Some(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                modifiers,
                capslock: current_capslock(),
            }))
        }
        VK_PACKET => None,
        VK_CAPITAL => {
            let capslock = current_capslock();
            if state
                .last_reported_capslock
                .get()
                .is_some_and(|prev_capslock| prev_capslock == capslock)
            {
                return None;
            }
            state.last_reported_capslock.set(Some(capslock));
            Some(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                modifiers,
                capslock,
            }))
        }
        vkey => {
            let keystroke = parse_normal_key(vkey, lparam, modifiers)?;
            Some(f(keystroke.0, keystroke.1))
        }
    }
}

fn parse_immutable(vkey: VIRTUAL_KEY) -> Option<String> {
    Some(
        match vkey {
            VK_SPACE => "space",
            VK_BACK => "backspace",
            VK_RETURN => "enter",
            VK_TAB => "tab",
            VK_UP => "up",
            VK_DOWN => "down",
            VK_RIGHT => "right",
            VK_LEFT => "left",
            VK_HOME => "home",
            VK_END => "end",
            VK_PRIOR => "pageup",
            VK_NEXT => "pagedown",
            VK_BROWSER_BACK => "back",
            VK_BROWSER_FORWARD => "forward",
            VK_ESCAPE => "escape",
            VK_INSERT => "insert",
            VK_DELETE => "delete",
            VK_APPS => "menu",
            VK_F1 => "f1",
            VK_F2 => "f2",
            VK_F3 => "f3",
            VK_F4 => "f4",
            VK_F5 => "f5",
            VK_F6 => "f6",
            VK_F7 => "f7",
            VK_F8 => "f8",
            VK_F9 => "f9",
            VK_F10 => "f10",
            VK_F11 => "f11",
            VK_F12 => "f12",
            VK_F13 => "f13",
            VK_F14 => "f14",
            VK_F15 => "f15",
            VK_F16 => "f16",
            VK_F17 => "f17",
            VK_F18 => "f18",
            VK_F19 => "f19",
            VK_F20 => "f20",
            VK_F21 => "f21",
            VK_F22 => "f22",
            VK_F23 => "f23",
            VK_F24 => "f24",
            _ => return None,
        }
        .to_string(),
    )
}

fn parse_normal_key(
    vkey: VIRTUAL_KEY,
    lparam: LPARAM,
    mut modifiers: Modifiers,
) -> Option<(Keystroke, bool)> {
    let (key_char, prefer_character_input) = process_key(vkey, lparam.hiword());

    let key = parse_immutable(vkey).or_else(|| {
        let scan_code = lparam.hiword() & 0xFF;
        get_keystroke_key(vkey, scan_code as u32, &mut modifiers)
    })?;

    Some((
        Keystroke {
            modifiers,
            key,
            key_char,
        },
        prefer_character_input,
    ))
}

fn process_key(vkey: VIRTUAL_KEY, scan_code: u16) -> (Option<String>, bool) {
    let mut keyboard_state = [0u8; 256];
    unsafe {
        if GetKeyboardState(&mut keyboard_state).is_err() {
            return (None, false);
        }
    }

    let mut buffer_c = [0u16; 8];
    let result_c = unsafe {
        ToUnicode(
            vkey.0 as u32,
            scan_code as u32,
            Some(&keyboard_state),
            &mut buffer_c,
            0x4,
        )
    };

    if result_c == 0 {
        return (None, false);
    }

    let c = &buffer_c[..result_c.unsigned_abs() as usize];
    let key_char = String::from_utf16(c)
        .ok()
        .filter(|s| !s.is_empty() && !s.chars().next().unwrap().is_control());

    if result_c < 0 {
        return (key_char, true);
    }

    if key_char.is_none() {
        return (None, false);
    }

    // Workaround for some bug that makes the compiler think keyboard_state is still zeroed out
    let keyboard_state = std::hint::black_box(keyboard_state);
    let ctrl_down = (keyboard_state[VK_CONTROL.0 as usize] & 0x80) != 0;
    let alt_down = (keyboard_state[VK_MENU.0 as usize] & 0x80) != 0;
    let win_down = (keyboard_state[VK_LWIN.0 as usize] & 0x80) != 0
        || (keyboard_state[VK_RWIN.0 as usize] & 0x80) != 0;

    let has_modifiers = ctrl_down || alt_down || win_down;
    if !has_modifiers {
        return (key_char, false);
    }

    let mut state_no_modifiers = keyboard_state;
    state_no_modifiers[VK_CONTROL.0 as usize] = 0;
    state_no_modifiers[VK_LCONTROL.0 as usize] = 0;
    state_no_modifiers[VK_RCONTROL.0 as usize] = 0;
    state_no_modifiers[VK_MENU.0 as usize] = 0;
    state_no_modifiers[VK_LMENU.0 as usize] = 0;
    state_no_modifiers[VK_RMENU.0 as usize] = 0;
    state_no_modifiers[VK_LWIN.0 as usize] = 0;
    state_no_modifiers[VK_RWIN.0 as usize] = 0;

    let mut buffer_c_no_modifiers = [0u16; 8];
    let result_c_no_modifiers = unsafe {
        ToUnicode(
            vkey.0 as u32,
            scan_code as u32,
            Some(&state_no_modifiers),
            &mut buffer_c_no_modifiers,
            0x4,
        )
    };

    let c_no_modifiers = &buffer_c_no_modifiers[..result_c_no_modifiers.unsigned_abs() as usize];
    (
        key_char,
        result_c != result_c_no_modifiers || c != c_no_modifiers,
    )
}

fn parse_ime_composition_string(ctx: HIMC, comp_type: IME_COMPOSITION_STRING) -> Option<Vec<u16>> {
    unsafe {
        let string_len = ImmGetCompositionStringW(ctx, comp_type, None, 0);
        if string_len >= 0 {
            let mut buffer = vec![0u8; string_len as usize + 2];
            ImmGetCompositionStringW(
                ctx,
                comp_type,
                Some(buffer.as_mut_ptr() as _),
                string_len as _,
            );
            let wstring = std::slice::from_raw_parts::<u16>(
                buffer.as_mut_ptr().cast::<u16>(),
                string_len as usize / 2,
            );
            Some(wstring.to_vec())
        } else {
            None
        }
    }
}

#[inline]
fn retrieve_composition_cursor_position(ctx: HIMC) -> usize {
    unsafe { ImmGetCompositionStringW(ctx, GCS_CURSORPOS, None, 0) as usize }
}

fn should_use_ime_cursor_position(ctx: HIMC, cursor_pos: usize) -> bool {
    let attrs_size = unsafe { ImmGetCompositionStringW(ctx, GCS_COMPATTR, None, 0) } as usize;
    if attrs_size == 0 {
        return false;
    }

    let mut attrs = vec![0u8; attrs_size];
    let result = unsafe {
        ImmGetCompositionStringW(
            ctx,
            GCS_COMPATTR,
            Some(attrs.as_mut_ptr() as *mut _),
            attrs_size as u32,
        )
    };
    if result <= 0 {
        return false;
    }

    // Keep the cursor adjacent to the inserted text by only using the suggested position
    // if it's adjacent to unconverted text.
    let at_cursor_is_input = cursor_pos < attrs.len() && attrs[cursor_pos] == (ATTR_INPUT as u8);
    let before_cursor_is_input = cursor_pos > 0
        && (cursor_pos - 1) < attrs.len()
        && attrs[cursor_pos - 1] == (ATTR_INPUT as u8);

    at_cursor_is_input || before_cursor_is_input
}

#[inline]
fn is_virtual_key_pressed(vkey: VIRTUAL_KEY) -> bool {
    unsafe { GetKeyState(vkey.0 as i32) < 0 }
}

#[inline]
pub(crate) fn current_modifiers() -> Modifiers {
    Modifiers {
        control: is_virtual_key_pressed(VK_CONTROL),
        alt: is_virtual_key_pressed(VK_MENU),
        shift: is_virtual_key_pressed(VK_SHIFT),
        platform: is_virtual_key_pressed(VK_LWIN) || is_virtual_key_pressed(VK_RWIN),
        function: false,
    }
}

#[inline]
pub(crate) fn current_capslock() -> Capslock {
    let on = unsafe { GetKeyState(VK_CAPITAL.0 as i32) & 1 } > 0;
    Capslock { on }
}

// there is some additional non-visible space when talking about window
// borders on Windows:
// - SM_CXSIZEFRAME: The resize handle.
// - SM_CXPADDEDBORDER: Additional border space that isn't part of the resize handle.
fn get_frame_thicknessx(dpi: u32) -> i32 {
    let resize_frame_thickness = unsafe { GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi) };
    let padding_thickness = unsafe { GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi) };
    resize_frame_thickness + padding_thickness
}

fn get_frame_thicknessy(dpi: u32) -> i32 {
    let resize_frame_thickness = unsafe { GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi) };
    let padding_thickness = unsafe { GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi) };
    resize_frame_thickness + padding_thickness
}

fn notify_frame_changed(handle: HWND) {
    unsafe {
        SetWindowPos(
            handle,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED
                | SWP_NOACTIVATE
                | SWP_NOCOPYBITS
                | SWP_NOMOVE
                | SWP_NOOWNERZORDER
                | SWP_NOREPOSITION
                | SWP_NOSENDCHANGING
                | SWP_NOSIZE
                | SWP_NOZORDER,
        )
        .log_err();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        custom_caption_system_menu_command, is_custom_caption_right_click,
        should_show_custom_caption_system_menu,
    };
    use gpui::{MouseButton, WindowControlArea};
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_SPACE;
    use windows::Win32::UI::WindowsAndMessaging::{
        HTCAPTION, HTCLIENT, HTCLOSE, HTMAXBUTTON, HTMINBUTTON, SC_KEYMENU, SC_MOUSEMENU,
    };

    /// Keep right-click and Alt+Space system-menu requests scoped to hidden-caption windows.
    ///
    /// @author kongweiguang
    #[test]
    fn hidden_caption_system_menu_accepts_only_mouse_and_alt_space_commands() {
        assert_eq!(
            custom_caption_system_menu_command(
                true,
                WPARAM((SC_MOUSEMENU | 0x000F) as usize),
                LPARAM(0x0010_0020),
            ),
            Some(SC_MOUSEMENU)
        );
        assert_eq!(
            custom_caption_system_menu_command(
                true,
                WPARAM((SC_KEYMENU | 0x0007) as usize),
                LPARAM(VK_SPACE.0 as isize),
            ),
            Some(SC_KEYMENU)
        );
        assert_eq!(
            custom_caption_system_menu_command(
                true,
                WPARAM(SC_KEYMENU as usize),
                LPARAM(b'x' as isize),
            ),
            None
        );
        assert_eq!(
            custom_caption_system_menu_command(
                false,
                WPARAM(SC_MOUSEMENU as usize),
                LPARAM::default(),
            ),
            None
        );
    }

    /// 回归锁定菜单只能从明确标为 Drag 的空白区域触发。
    ///
    /// @author kongweiguang
    #[test]
    fn custom_caption_menu_requires_a_blank_drag_hitbox() {
        assert!(should_show_custom_caption_system_menu(
            true,
            MouseButton::Right,
            HTCAPTION as usize,
            Some(WindowControlArea::Drag),
        ));
        assert!(!should_show_custom_caption_system_menu(
            true,
            MouseButton::Right,
            HTCAPTION as usize,
            Some(WindowControlArea::Max),
        ));
        assert!(!should_show_custom_caption_system_menu(
            true,
            MouseButton::Right,
            HTCAPTION as usize,
            Some(WindowControlArea::Min),
        ));
        assert!(!should_show_custom_caption_system_menu(
            true,
            MouseButton::Right,
            HTCAPTION as usize,
            Some(WindowControlArea::Close),
        ));
        assert!(!should_show_custom_caption_system_menu(
            true,
            MouseButton::Right,
            HTCAPTION as usize,
            None,
        ));
        assert!(!should_show_custom_caption_system_menu(
            false,
            MouseButton::Right,
            HTCAPTION as usize,
            Some(WindowControlArea::Drag),
        ));
        assert!(!should_show_custom_caption_system_menu(
            true,
            MouseButton::Left,
            HTCAPTION as usize,
            Some(WindowControlArea::Drag),
        ));
        assert!(!should_show_custom_caption_system_menu(
            true,
            MouseButton::Right,
            HTCLIENT as usize,
            Some(WindowControlArea::Drag),
        ));
    }

    /// 回归锁定自绘标题栏只截获 HTCAPTION 右键，客户区和原生控件命中仍走原路径。
    ///
    /// @author kongweiguang
    #[test]
    fn custom_caption_right_click_gate_does_not_capture_controls_or_overlays() {
        assert!(is_custom_caption_right_click(
            true,
            MouseButton::Right,
            HTCAPTION as usize,
        ));
        assert!(!is_custom_caption_right_click(
            true,
            MouseButton::Right,
            HTCLIENT as usize,
        ));
        assert!(!is_custom_caption_right_click(
            true,
            MouseButton::Right,
            HTCLOSE as usize,
        ));
        assert!(!is_custom_caption_right_click(
            true,
            MouseButton::Right,
            HTMAXBUTTON as usize,
        ));
        assert!(!is_custom_caption_right_click(
            true,
            MouseButton::Right,
            HTMINBUTTON as usize,
        ));
        assert!(!is_custom_caption_right_click(
            true,
            MouseButton::Left,
            HTCAPTION as usize,
        ));
        assert!(!is_custom_caption_right_click(
            false,
            MouseButton::Right,
            HTCAPTION as usize,
        ));
    }
}
