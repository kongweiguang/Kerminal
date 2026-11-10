//! 扩展中心作为普通内容实例接入宿主，页面操作由共享服务和退出门禁管理。
//! @author kongweiguang

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use gpui_kit::{
    AnyView, App, AppContext as _, Entity, WeakEntity, Window,
    component::{
        WindowExt as _,
        button::{Button, ButtonVariants as _},
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor, extensions::ExtensionsService,
};

use crate::{
    features::extensions::ExtensionsPage,
    plugins::content::{
        ContentCloseFuture, ContentFactory, ContentInstance, ContentKind, ContentSavedState,
        ContentScope,
    },
};

/// 稳定内容标识与插件声明相同，便于工作台的普通内容导航复用。
pub const EXTENSIONS_CONTENT_ID: &str = "extensions";

/// 实体句柄只用于宿主订阅事件；真正的资源 owner 始终是内容实例。
pub struct ExtensionsContentFactory {
    service: Arc<ExtensionsService>,
    executor: ApplicationTaskExecutor,
    host_api: String,
    page: Mutex<Option<WeakEntity<ExtensionsPage>>>,
    restart_pending: AtomicBool,
}

impl ExtensionsContentFactory {
    /// 桌面组合根注入同一服务和 API 版本，避免页面与安装器采用不同兼容规则。
    pub fn new(
        service: Arc<ExtensionsService>,
        executor: ApplicationTaskExecutor,
        host_api: String,
    ) -> Self {
        Self {
            service,
            executor,
            host_api,
            page: Mutex::new(None),
            restart_pending: AtomicBool::new(false),
        }
    }

    /// 宿主在打开页面后取得实体以订阅重启事件；毒化锁显式报错避免丢失订阅。
    pub fn page(&self) -> AppResult<Option<Entity<ExtensionsPage>>> {
        self.page
            .lock()
            .map(|page| page.as_ref().and_then(WeakEntity::upgrade))
            .map_err(|_| AppError::conflict("扩展页面状态不可用"))
    }
}

impl ContentFactory for ExtensionsContentFactory {
    /// 页面身份必须与注册清单匹配，不能作为可持久化标签恢复。
    fn content_id(&self) -> &str {
        EXTENSIONS_CONTENT_ID
    }
    /// 扩展中心只允许作为中央页面挂载。
    fn supports(&self, kind: ContentKind) -> bool {
        kind == ContentKind::Page
    }
    /// 每个窗口独立创建页面和输入焦点，服务仍由组合根共享。
    fn create(
        &self,
        scope: &ContentScope,
        state: Option<&ContentSavedState>,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Box<dyn ContentInstance>> {
        if !matches!(scope, ContentScope::Page(id) if id == EXTENSIONS_CONTENT_ID)
            || state.is_some()
        {
            return Err(AppError::invalid("扩展页面作用域无效"));
        }
        let view = cx.new(|cx| {
            ExtensionsPage::new(
                self.service.clone(),
                self.executor.clone(),
                self.host_api.clone(),
                window,
                cx,
            )
        });
        *self
            .page
            .lock()
            .map_err(|_| AppError::conflict("扩展页面状态不可用"))? = Some(view.downgrade());
        Ok(Box::new(ExtensionsContent { view }))
    }
}

struct ExtensionsContent {
    view: Entity<ExtensionsPage>,
}

impl ContentInstance for ExtensionsContent {
    /// 复用同一个视图实体以保留筛选和焦点。
    fn view(&self) -> AnyView {
        self.view.clone().into()
    }
    /// 此页面不参与工作区标签持久化。
    fn snapshot(&self, _cx: &App) -> AppResult<ContentSavedState> {
        Err(AppError::unsupported("扩展页面无需保存标签状态"))
    }
    /// 导航成功后再加载缓存，避免绘制时启动网络任务。
    fn activate(&mut self, _window: &mut Window, cx: &mut App) -> AppResult<()> {
        self.view.update(cx, |page, cx| page.activate(cx));
        Ok(())
    }
    /// 写入结束前拒绝关闭，确保提交结果与回读不会因页面销毁而丢失。
    fn begin_close(&mut self, _deadline: Instant, cx: &mut App) -> AppResult<ContentCloseFuture> {
        if self.view.read(cx).is_busy() {
            return Err(AppError::conflict("扩展操作尚未结束，请稍后重试关闭"));
        }
        Ok(Box::pin(async { Ok(()) }))
    }
}

impl super::RootView {
    /// 只订阅当前内容实体，重新打开页面时旧订阅随句柄替换而释放。
    pub(super) fn subscribe_extensions(
        &mut self,
        window: &mut Window,
        cx: &mut gpui_kit::Context<Self>,
    ) -> AppResult<()> {
        let page = self
            .extensions_factory
            .as_ref()
            .ok_or_else(|| AppError::not_found("扩展服务不可用"))?
            .page()?
            .ok_or_else(|| AppError::not_found("扩展页面不可用"))?;
        self.extensions_subscription =
            Some(
                cx.subscribe_in(&page, window, |root, _, event, window, cx| match event {
                    crate::features::extensions::ExtensionsEvent::RestartRequested => {
                        root.begin_extensions_restart(window, cx)
                    }
                }),
            );
        Ok(())
    }

    /// 同一窗口只允许一条重启确认链，避免慢会话快照与重复点击相互覆盖。
    fn begin_extensions_restart(&mut self, window: &mut Window, cx: &mut gpui_kit::Context<Self>) {
        let Some(factory) = self.extensions_factory.clone() else {
            return;
        };
        if factory.restart_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        self.check_extensions_restart_sessions(factory, None, window, cx);
    }

    /// 查询共享会话表，确认后再次核对身份集合；会话变化必须重新确认。
    fn check_extensions_restart_sessions(
        &mut self,
        factory: Arc<ExtensionsContentFactory>,
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
                    factory.restart_pending.store(false, Ordering::Release);
                    return;
                }
                match result {
                    Err(error) => {
                        factory.restart_pending.store(false, Ordering::Release);
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
                            factory.restart_pending.store(false, Ordering::Release);
                            root.request_quit(super::QuitIntent::RestartForExtensions, cx);
                        } else {
                            root.confirm_extensions_restart(factory.clone(), current, window, cx);
                        }
                    }
                }
            });
        })
        .detach();
    }

    /// 会话数来自共享 session registry；SSH 是终端总数的子集而不是额外相加。
    fn confirm_extensions_restart(
        &mut self,
        factory: Arc<ExtensionsContentFactory>,
        activity: RestartActivity,
        window: &mut Window,
        cx: &mut gpui_kit::Context<Self>,
    ) {
        let total = activity.sessions.len();
        let ssh = activity.sessions.iter().filter(|(_, ssh)| *ssh).count();
        let routes = activity.remote.as_ref().map_or(0, |value| value.routes);
        let entity = cx.entity().downgrade();
        let cancel_factory = factory.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm_entity = entity.clone();
            let confirm_factory = factory.clone();
            let confirm_sessions = activity.clone();
            let keyboard_entity = entity.clone();
            let keyboard_factory = factory.clone();
            let keyboard_sessions = activity.clone();
            let cancel_button_factory = cancel_factory.clone();
            let keyboard_cancel_factory = cancel_factory.clone();
            dialog.centered().width(px(460.)).close_button(false).overlay_closable(false)
                .title(crate::locale::text("重启 Kerminal", "Restart Kerminal"))
                .child(div().child(crate::locale::text(
                    format!("当前有 {total} 个活动终端会话，其中 {ssh} 个 SSH 终端；另有 {routes} 条 SSH 连接路由（可能包含这些终端）。重启将关闭它们。"),
                    format!("There are {total} active terminal sessions, including {ssh} SSH terminals, and {routes} SSH connection routes (which may include those terminals). Restarting will close them."),
                )))
                .footer(div().flex().justify_end().gap_2()
                    .child(Button::new("extensions-restart-cancel").label(crate::locale::text("取消", "Cancel"))
                        .on_click(move |_, window, app| {
                            cancel_button_factory.restart_pending.store(false, Ordering::Release);
                            window.close_dialog(app);
                        }))
                    .child(Button::new("extensions-restart-confirm").primary().label(crate::locale::text("重启", "Restart"))
                        .on_click(move |_, window, app| {
                            window.close_dialog(app);
                            if let Some(entity) = confirm_entity.upgrade() {
                                entity.update(app, |root, cx| root.check_extensions_restart_sessions(confirm_factory.clone(), Some(confirm_sessions.clone()), window, cx));
                            } else { confirm_factory.restart_pending.store(false, Ordering::Release); }
                        })))
                .on_ok(move |_, window, app| {
                    if let Some(entity) = keyboard_entity.upgrade() {
                        entity.update(app, |root, cx| root.check_extensions_restart_sessions(keyboard_factory.clone(), Some(keyboard_sessions.clone()), window, cx));
                    } else { keyboard_factory.restart_pending.store(false, Ordering::Release); }
                    true
                })
                .on_cancel(move |_, _, _| { keyboard_cancel_factory.restart_pending.store(false, Ordering::Release); true })
        });
    }

    /// 验收使用生产内容宿主打开页面，保留服务和导航的真实生命周期。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn extensions_acceptance_open(
        &mut self,
        window: &mut Window,
        cx: &mut gpui_kit::Context<Self>,
    ) -> AppResult<()> {
        self.open_plugin_page(EXTENSIONS_CONTENT_ID, window, cx)
    }

    /// 验收将隔离测试包视为文件选择结果，保留生产预检与确认门禁。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn extensions_acceptance_inspect_file(
        &mut self,
        path: std::path::PathBuf,
        cx: &mut gpui_kit::Context<Self>,
    ) -> AppResult<()> {
        let page = self
            .extensions_factory
            .as_ref()
            .ok_or_else(|| AppError::not_found("扩展服务不可用"))?
            .page()?
            .ok_or_else(|| AppError::not_found("扩展页面不可用"))?;
        page.update(cx, |page, cx| page.acceptance_inspect_file(path, cx))
    }

    /// 未打开页面时明确返回 None，不能把未初始化状态当成空目录。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn extensions_acceptance_snapshot(
        &self,
        cx: &App,
    ) -> AppResult<Option<crate::features::extensions::ExtensionsAcceptanceSnapshot>> {
        let Some(factory) = &self.extensions_factory else {
            return Ok(None);
        };
        Ok(factory
            .page()?
            .map(|page| page.read(cx).acceptance_snapshot()))
    }
}

/// 终端身份和 SSH 路由代次共同核对，避免同数量连接被替换后复用旧确认。
#[derive(Clone, Debug, Eq, PartialEq)]
struct RestartActivity {
    sessions: Vec<(String, bool)>,
    remote: Option<kerminal_application::connections::RemoteConnectionActivity>,
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
