//! 窗口级光标时钟；终端协议是否允许闪烁仍由 Canvas 判定。
//!
//! @author kongweiguang

use std::time::Duration;

use gpui_kit::{Context, Window};

use super::RootView;

impl RootView {
    /// 时钟只持有弱实体且由 Root 保存任务所有权，窗口销毁或宿主收尾即可结束。
    /// 失焦和禁用只恢复可见相位，不持续重绘后台窗口；焦点回到任一真实 pane 后再推进相位。
    ///
    /// @author kongweiguang
    pub(super) fn start_cursor_blink_clock(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.cursor_blink_task.is_some() {
            return;
        }
        self.cursor_blink_task = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let keep_running = cx
                    .update(|window, app| {
                        this.update(app, |root, cx| {
                            if root.host_shutdown_requested {
                                return false;
                            }
                            let terminal_focused = root.focus_handle.is_focused(window)
                                || root.pane_input_handlers.values().any(|registration| {
                                    registration
                                        .entity
                                        .read(cx)
                                        .focus_handle()
                                        .is_focused(window)
                                });
                            let next_visible = if root.cursor_blink_enabled
                                && window.is_window_active()
                                && terminal_focused
                            {
                                !root.cursor_blink_phase_visible
                            } else {
                                true
                            };
                            if root.cursor_blink_phase_visible != next_visible {
                                root.cursor_blink_phase_visible = next_visible;
                                cx.notify();
                            }
                            true
                        })
                    })
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or(false);
                if !keep_running {
                    break;
                }
            }
        }));
    }
}
