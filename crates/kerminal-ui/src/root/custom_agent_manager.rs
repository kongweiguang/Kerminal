//! 自定义 Agent 的全窗口弹层与 Preferences 唯一 CAS 桥接。
//!
//! @author kongweiguang

use super::RootView;
use crate::{
    features::agents::{
        CustomAgentLauncherManager, CustomAgentManagerEvent, CustomAgentManagerWriteOutcome,
    },
    theme::ThemeTokens,
};
use gpui_kit::{
    AnyElement, AppContext as _, Context, IntoElement, Window, base::FocusTrapElement as _, div,
    prelude::*, px,
};

impl RootView {
    /// 提交和读回沿用管理器的版本链；只有已确认的自身写入推进版本，外部更新不能偷换基线。
    /// 打开时撤销终端焦点意图，防止迟到回调抢焦点。
    ///
    /// @author kongweiguang
    pub(super) fn open_custom_agent_manager(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.host_shutdown_requested || self.custom_agent_manager.is_some() {
            return;
        }
        let baseline = match self
            .preferences_panel
            .read(cx)
            .agent_launcher_settings_for_manager()
        {
            Ok(settings) => settings,
            Err(message) => {
                self.status = Some(message.into());
                self.open_preferences_modal(window, cx);
                return;
            }
        };
        self.preferences_panel.update(cx, |panel, _| {
            panel.release_agent_launcher_manager_after_close()
        });
        self.clear_pending_pane_focus();
        self.clear_keybinding_chord();
        self.pending_key_commits.clear();
        self.custom_agent_manager_return_focus = window.focused(cx);
        let manager = cx.new(|cx| {
            CustomAgentLauncherManager::new(baseline.settings.agent_launcher.clone(), window, cx)
        });
        let subscription =
            cx.subscribe_in(
                &manager,
                window,
                move |root, _, event, window, cx| match event {
                    CustomAgentManagerEvent::Save { settings, .. }
                    | CustomAgentManagerEvent::SaveKeepOpen { settings, .. } => {
                        let result = root.preferences_panel.update(cx, |panel, cx| {
                            panel.apply_agent_launcher_change(&baseline, settings.clone(), cx)
                        });
                        if let Err(message) = result {
                            root.apply_custom_agent_manager_outcome(
                                CustomAgentManagerWriteOutcome::Failed(message),
                                window,
                                cx,
                            );
                        }
                    }
                    CustomAgentManagerEvent::ReadBack => {
                        let result = root.preferences_panel.update(cx, |panel, cx| {
                            panel.read_back_agent_launcher_for_manager(&baseline, cx)
                        });
                        if let Err(message) = result {
                            root.apply_custom_agent_manager_outcome(
                                CustomAgentManagerWriteOutcome::ReadBackFailed(message),
                                window,
                                cx,
                            );
                        }
                    }
                    CustomAgentManagerEvent::OpenPreferences => {
                        root.open_preferences_modal(window, cx);
                        root.preferences_panel.update(cx, |panel, cx| {
                            panel.show_agent_launcher_settings(window, cx)
                        });
                    }
                    CustomAgentManagerEvent::Close => root.close_custom_agent_manager(window, cx),
                },
            );
        let focus = manager.read(cx).focus_handle(cx);
        self.custom_agent_manager = Some(manager);
        self.custom_agent_manager_subscription = Some(subscription);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// 管理器自己判断丢弃确认和忙状态，Root 的 Escape、遮罩及退出请求都不能绕过它。
    pub(super) fn request_close_custom_agent_manager(&mut self, cx: &mut Context<Self>) {
        if let Some(manager) = self.custom_agent_manager.clone() {
            manager.update(cx, |manager, cx| manager.request_close_from_root(cx));
        }
    }

    /// 确认保存刷新目录，删除后的确认保留管理器；失败与未知结果保留同一草稿等待核对。
    pub(super) fn apply_custom_agent_manager_outcome(
        &mut self,
        outcome: CustomAgentManagerWriteOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let confirmed = matches!(
            &outcome,
            CustomAgentManagerWriteOutcome::Confirmed(_)
                | CustomAgentManagerWriteOutcome::ConfirmedKeepOpen(_)
                | CustomAgentManagerWriteOutcome::ReadBack { .. }
        );
        let close = self.custom_agent_manager.clone().is_some_and(|manager| {
            manager.update(cx, |manager, cx| {
                let outcome = match outcome {
                    CustomAgentManagerWriteOutcome::Confirmed(settings)
                        if manager.pending_write_keeps_open() =>
                    {
                        CustomAgentManagerWriteOutcome::ConfirmedKeepOpen(settings)
                    }
                    other => other,
                };
                manager.apply_write_outcome(outcome, cx)
            })
        });
        if (confirmed || close)
            && let Some(panel) = self.agents_panel.clone()
        {
            panel.update(cx, |panel, cx| {
                panel.refresh_launcher_catalog_after_custom_settings_saved(cx)
            });
        }
        if close {
            self.close_custom_agent_manager(window, cx);
        }
    }

    /// 关闭后恢复仍在焦点树中的入口；旧焦点已消失时回到工作区，不按当前 Tab 重新绑定 Agent。
    pub(super) fn close_custom_agent_manager(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preferences_panel.update(cx, |panel, _| {
            panel.release_agent_launcher_manager_after_close()
        });
        self.custom_agent_manager = None;
        self.custom_agent_manager_subscription = None;
        if let Some(focus) = self
            .custom_agent_manager_return_focus
            .take()
            .filter(|focus| self.focus_handle.contains(focus, window))
        {
            window.focus(&focus, cx);
        } else {
            window.focus(&self.focus_handle, cx);
        }
        cx.notify();
    }
}

/// 采用 main 的 672px 宽、544px 高和弹框外观；小视口受安全边距约束，焦点仍由全窗陷阱管理。
///
/// @author kongweiguang
pub(super) fn render_overlay(
    root: &mut RootView,
    theme: ThemeTokens,
    window: &mut Window,
    cx: &mut Context<RootView>,
) -> Option<AnyElement> {
    if root.preferences_modal_open
        || root.connections_modal_open
        || root.root_confirmation_owner.is_some()
    {
        return None;
    }
    let manager = root.custom_agent_manager.clone()?;
    let focus = manager.read(cx).modal_focus_handle();
    let dialog_height = (window.viewport_size().height.as_f32() - 32.).clamp(0., 544.);
    Some(crate::ui_backdrop::modal_backdrop(
        div()
            .id("custom-agent-manager-overlay")
            .absolute()
            .inset_0()
            .occlude()
            .p(px(16.))
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.dialog_scrim)
            .on_click(cx.listener(|root, _, _, cx| root.request_close_custom_agent_manager(cx)))
            .child(crate::ui_backdrop::floating_surface(
                div()
                    .id("custom-agent-manager-surface")
                    .w_full()
                    .max_w(px(672.))
                    .h(px(dialog_height))
                    .max_h(px(dialog_height))
                    .min_w(px(0.))
                    .min_h(px(0.))
                    .occlude()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .rounded(theme.radius_dialog)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.floating_surface)
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(manager)
                    .focus_trap("custom-agent-manager-focus-trap", &focus),
                theme,
                theme.radius_dialog,
            ))
            .into_any_element(),
        theme,
    ))
}
