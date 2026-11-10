//! 扩展页由设置弹框承载，这里只保留会话重启确认与隔离验收委托。
//! @author kongweiguang

use gpui_kit::{
    Window,
    component::{
        WindowExt as _,
        button::{Button, ButtonVariants as _},
    },
    div,
    prelude::*,
    px,
};
#[cfg(feature = "right-rail-acceptance")]
use kerminal_application::{AppError, AppResult};

/// 终端身份和 SSH 路由代次共同核对，避免同数量连接被替换后复用旧确认。
#[derive(Clone, Debug, Eq, PartialEq)]
struct RestartActivity {
    sessions: Vec<(String, bool)>,
    remote: Option<kerminal_application::connections::RemoteConnectionActivity>,
}

impl super::RootView {
    /// 同一窗口只允许一条重启确认链，避免慢会话快照与重复点击相互覆盖。
    ///
    /// 扩展页已在设置弹框内，重启请求只结束进程，不改变页面宿主。
    /// @author kongweiguang
    pub(super) fn begin_extensions_restart(
        &mut self,
        window: &mut Window,
        cx: &mut gpui_kit::Context<Self>,
    ) {
        if self.extensions_restart_pending {
            return;
        }
        self.extensions_restart_pending = true;
        self.check_extensions_restart_sessions(None, window, cx);
    }

    /// 查询共享会话表，确认后再次核对身份集合；会话变化必须重新确认。
    ///
    /// @author kongweiguang
    fn check_extensions_restart_sessions(
        &mut self,
        approved: Option<RestartActivity>,
        window: &mut Window,
        cx: &mut gpui_kit::Context<Self>,
    ) {
        let executor = self.services.executor.clone();
        let sessions = self.services.sessions.clone();
        let remote = self.services.remote_connections.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = executor
                .run(move |_| async move {
                    let sessions = sessions
                        .list(kerminal_application::sessions::SessionFilter::default())
                        .await?;
                    let remote = match remote {
                        Some(remote) => Some(remote.activity()?),
                        None => None,
                    };
                    Ok(RestartActivity {
                        sessions: active_session_ids(&sessions),
                        remote,
                    })
                })
                .await;
            let _ = this.update_in(cx, |root, window, cx| {
                if root.host_shutdown_requested || root.pending_quit_intent.is_some() {
                    root.extensions_restart_pending = false;
                    return;
                }
                match result {
                    Err(error) => {
                        root.extensions_restart_pending = false;
                        root.status = Some(error.public_message().to_owned().into());
                        cx.notify();
                    }
                    Ok(current) => {
                        if (current.sessions.is_empty()
                            && current
                                .remote
                                .as_ref()
                                .is_none_or(|value| value.routes == 0))
                            || approved.as_ref() == Some(&current)
                        {
                            root.extensions_restart_pending = false;
                            root.request_quit(super::QuitIntent::RestartForExtensions, cx);
                        } else {
                            root.confirm_extensions_restart(current, window, cx);
                        }
                    }
                }
            });
        })
        .detach();
    }

    /// 会话数来自共享 session registry；SSH 是终端总数的子集而不是额外相加。
    ///
    /// 确认链不持有页面实体，设置弹框关闭后迟到的确认也必须能清除去重标记。
    /// @author kongweiguang
    fn confirm_extensions_restart(
        &mut self,
        activity: RestartActivity,
        window: &mut Window,
        cx: &mut gpui_kit::Context<Self>,
    ) {
        let total = activity.sessions.len();
        let ssh = activity.sessions.iter().filter(|(_, ssh)| *ssh).count();
        let routes = activity.remote.as_ref().map_or(0, |value| value.routes);
        let entity = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            // 对话框构建器是 Fn，按钮回调各自取得弱引用和会话快照的新克隆。
            let cancel_entity = entity.clone();
            let confirm_entity = entity.clone();
            let confirm_sessions = activity.clone();
            let keyboard_entity = entity.clone();
            let keyboard_sessions = activity.clone();
            let keyboard_cancel_entity = entity.clone();
            dialog.centered().width(px(460.)).close_button(false).overlay_closable(false)
                .title(crate::locale::text("重启 Kerminal", "Restart Kerminal"))
                .child(div().child(crate::locale::text(
                    format!("当前有 {total} 个活动终端会话，其中 {ssh} 个 SSH 终端；另有 {routes} 条 SSH 连接路由（可能包含这些终端）。重启将关闭它们。"),
                    format!("There are {total} active terminal sessions, including {ssh} SSH terminals, and {routes} SSH connection routes (which may include those terminals). Restarting will close them."),
                )))
                .footer(div().flex().justify_end().gap_2()
                    .child(Button::new("extensions-restart-cancel").label(crate::locale::text("取消", "Cancel"))
                        .on_click(move |_, window, app| {
                            if let Some(entity) = cancel_entity.upgrade() {
                                entity.update(app, |root, _| root.extensions_restart_pending = false);
                            }
                            window.close_dialog(app);
                        }))
                    .child(Button::new("extensions-restart-confirm").primary().label(crate::locale::text("重启", "Restart"))
                        .on_click(move |_, window, app| {
                            window.close_dialog(app);
                            if let Some(entity) = confirm_entity.upgrade() {
                                entity.update(app, |root, cx| root.check_extensions_restart_sessions(Some(confirm_sessions.clone()), window, cx));
                            }
                        })))
                .on_ok(move |_, window, app| {
                    if let Some(entity) = keyboard_entity.upgrade() {
                        entity.update(app, |root, cx| root.check_extensions_restart_sessions(Some(keyboard_sessions.clone()), window, cx));
                    }
                    true
                })
                .on_cancel(move |_, _, app| {
                    if let Some(entity) = keyboard_cancel_entity.upgrade() {
                        entity.update(app, |root, _| root.extensions_restart_pending = false);
                    }
                    true
                })
        });
    }

    /// 验收使用真实设置弹框定位扩展分类，保留服务和导航的真实生命周期。
    ///
    /// @author kongweiguang
    #[cfg(feature = "right-rail-acceptance")]
    pub fn extensions_acceptance_open(
        &mut self,
        window: &mut Window,
        cx: &mut gpui_kit::Context<Self>,
    ) -> AppResult<()> {
        self.open_preferences_modal(window, cx);
        if !self.preferences_modal_open {
            return Err(AppError::conflict("设置窗口未能打开"));
        }
        self.preferences_panel
            .update(cx, |panel, cx| panel.open_extensions_section(window, cx));
        Ok(())
    }

    /// 验收将隔离测试包视为文件选择结果，保留生产预检与确认门禁。
    ///
    /// @author kongweiguang
    #[cfg(feature = "right-rail-acceptance")]
    pub fn extensions_acceptance_inspect_file(
        &mut self,
        path: std::path::PathBuf,
        cx: &mut gpui_kit::Context<Self>,
    ) -> AppResult<()> {
        self.preferences_panel.update(cx, |panel, cx| {
            panel.extensions_acceptance_inspect_file(path, cx)
        })
    }

    /// 页面不存在时明确返回 None，不能把未进入扩展分类当成空目录。
    ///
    /// @author kongweiguang
    #[cfg(feature = "right-rail-acceptance")]
    pub fn extensions_acceptance_snapshot(
        &self,
        cx: &gpui_kit::App,
    ) -> AppResult<Option<crate::features::extensions::ExtensionsAcceptanceSnapshot>> {
        Ok(self
            .preferences_panel
            .read(cx)
            .extensions_acceptance_snapshot(cx))
    }
}

/// 只计入可能仍占有会话资源的生命周期，并排序使重复查询不依赖注册表顺序。
fn active_session_ids(sessions: &[kerminal_domain::sessions::SessionInfo]) -> Vec<(String, bool)> {
    use kerminal_domain::sessions::{SessionLifecycle, SessionTarget};
    let mut active = sessions
        .iter()
        .filter(|session| {
            matches!(
                session.lifecycle,
                SessionLifecycle::Starting | SessionLifecycle::Running | SessionLifecycle::Closing
            )
        })
        .map(|session| {
            (
                session.session_id.to_string(),
                matches!(&session.target, SessionTarget::Ssh(_)),
            )
        })
        .collect::<Vec<_>>();
    active.sort_unstable();
    active
}

#[cfg(test)]
#[path = "../../tests/unit/root_extensions.rs"]
mod tests;
