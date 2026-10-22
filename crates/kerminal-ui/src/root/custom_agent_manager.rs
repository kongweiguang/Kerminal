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
    /// 提交和读回共用打开时固定的 Settings 版本；立即撤销终端焦点意图，防止迟到回调抢焦点。
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
                    CustomAgentManagerEvent::Save { settings, .. } => {
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

    /// 仅确认回执或相同快照读回可关闭；失败与未知结果保留同一 Entity 和输入内容。
    pub(super) fn apply_custom_agent_manager_outcome(
        &mut self,
        outcome: CustomAgentManagerWriteOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let close = self.custom_agent_manager.clone().is_some_and(|manager| {
            manager.update(cx, |manager, cx| manager.apply_write_outcome(outcome, cx))
        });
        if close {
            if let Some(panel) = self.agents_panel.clone() {
                panel.update(cx, |panel, cx| {
                    panel.refresh_launcher_catalog_after_custom_settings_saved(cx)
                });
            }
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

/// 采用旧版 672px 宽、最多 544px 高的编辑器尺寸；小视口受安全边距约束，焦点仍由全窗陷阱管理。
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
    Some(
        div()
            .id("custom-agent-manager-overlay")
            .absolute()
            .inset_0()
            .occlude()
            .p(px(16.))
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.background.alpha(0.62))
            .on_click(cx.listener(|root, _, _, cx| root.request_close_custom_agent_manager(cx)))
            .child(
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
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.panel)
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(manager)
                    .focus_trap("custom-agent-manager-focus-trap", &focus),
            )
            .into_any_element(),
    )
}
