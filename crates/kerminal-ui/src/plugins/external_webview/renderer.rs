//! 正式网页视图拥有独立页面代次；运行服务与网页之间仅交换受验 JSON。
//! @author kongweiguang

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, IntoElement, Render, Subscription,
    Task, Window,
    component::{
        ActiveTheme as _, Theme,
        button::{Button, ButtonVariants as _},
    },
    div,
    prelude::*,
};
use kerminal_application::{
    AppError, AppResult,
    executor::ApplicationTaskExecutor,
    plugin_ports::{
        PluginViewIdentity, PluginViewSavedState, PluginWebViewEventTarget, PreparedPlugin,
    },
};
use kerminal_domain::plugins::PluginRuntimeState;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{
    OpenWebViewCommandPalette, WebViewBridgeMessage, WebViewBridgeSession, browser_bootstrap,
};
use crate::plugins::{
    content::{ContentInstanceIdentity, ContentStateChanged},
    external::runtime::{
        AsyncViewRuntime, RuntimeEvent, RuntimeFuture, RuntimeRequest, RuntimeSubscription,
        WebViewEvents,
    },
    webview::{PluginWebView, PluginWebViewEvent, PluginWebViewResourceDocument},
};

mod lifecycle;
mod requests;

const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 页面信封解析后的请求独立携带取消令牌，避免长参数列表混淆身份与序列。
struct PageRequest {
    id: String,
    sequence: u64,
    method: String,
    params: Value,
    cancellation: CancellationToken,
}

/// 工厂传递冻结资源和共享执行器；窗口对象不进入后台任务。
pub(crate) struct WebViewRendererConfig {
    pub prepared: Arc<PreparedPlugin>,
    pub identity: PluginViewIdentity,
    pub scope: Value,
    pub saved_state: Option<PluginViewSavedState>,
    pub runtime: Arc<dyn AsyncViewRuntime>,
    pub executor: ApplicationTaskExecutor,
    pub locale: String,
    pub content_identity: Option<ContentInstanceIdentity>,
}

/// 保留宿主身份和 DOM，页面替换才推进桥接代次；进程恢复单独校验进程代次。
pub struct WebViewRenderer {
    prepared: Arc<PreparedPlugin>,
    identity: PluginViewIdentity,
    runtime: Arc<dyn AsyncViewRuntime>,
    executor: ApplicationTaskExecutor,
    scope: Value,
    saved_state: Option<PluginViewSavedState>,
    content_identity: Option<ContentInstanceIdentity>,
    owner: AnyWindowHandle,
    page: Option<Entity<PluginWebView>>,
    page_subscription: Option<Subscription>,
    event_watch: Option<Task<()>>,
    event_sequence: u64,
    _theme_subscription: Subscription,
    bridge: WebViewBridgeSession,
    page_epoch: u64,
    page_loaded: bool,
    page_open: Option<String>,
    page_ready: bool,
    pending_updates: Vec<Value>,
    last_heartbeat: Instant,
    process_generation: Option<u64>,
    retired_generation: Option<u64>,
    open_in_flight: bool,
    opened: bool,
    reopening: bool,
    cancellation: CancellationToken,
    closing: Arc<AtomicBool>,
    page_cancellation: CancellationToken,
    visible: bool,
    focused: bool,
    locale: String,
    failure: Option<String>,
    error: Option<String>,
    #[cfg(feature = "right-rail-acceptance")]
    acceptance_dom: Value,
}

impl WebViewRenderer {
    /// 先订阅运行态再打开实例；缺少原生运行时显示局部恢复界面而不阻断其它插件。
    pub(crate) fn mount(
        config: WebViewRendererConfig,
        window: &mut Window,
        cx: &mut App,
    ) -> AppResult<Entity<Self>> {
        let subscription = config.runtime.subscribe(&config.identity)?;
        let bridge = WebViewBridgeSession::new(config.identity.clone(), 1)?;
        Ok(cx.new(|cx| {
            let theme_subscription = cx
                .observe_global_in::<Theme>(window, |renderer: &mut Self, _, cx| {
                    renderer.send_environment(cx)
                });
            let mut renderer = Self {
                prepared: config.prepared,
                identity: config.identity,
                runtime: config.runtime,
                executor: config.executor,
                scope: config.scope,
                saved_state: config.saved_state,
                content_identity: config.content_identity,
                owner: window.window_handle(),
                page: None,
                page_subscription: None,
                event_watch: None,
                event_sequence: 0,
                _theme_subscription: theme_subscription,
                bridge,
                page_epoch: 1,
                page_loaded: false,
                page_open: None,
                page_ready: false,
                pending_updates: Vec::new(),
                last_heartbeat: Instant::now(),
                process_generation: None,
                retired_generation: None,
                open_in_flight: false,
                opened: false,
                reopening: false,
                cancellation: CancellationToken::new(),
                closing: Arc::new(AtomicBool::new(false)),
                page_cancellation: CancellationToken::new(),
                visible: false,
                focused: false,
                locale: config.locale,
                failure: None,
                error: None,
                #[cfg(feature = "right-rail-acceptance")]
                acceptance_dom: Value::Null,
            };
            renderer.create_page(window, cx);
            renderer.watch(subscription, window, cx);
            renderer.watch_health(window, cx);
            renderer.open(window, cx);
            renderer
        }))
    }

    /// 原生事件身份包含页面代次，旧子窗队列不能向替换后的网页投递消息。
    pub(crate) fn native_identity(&self) -> String {
        format!("{}:{}", self.identity.instance, self.page_epoch)
    }

    /// 使用校验完成的资源快照，不在窗口线程访问插件目录或尝试其他版本。
    fn create_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = &self.page
            && let Err(error) = page.update(cx, |page, cx| page.set_visible(false, window, cx))
        {
            self.failure = Some(error.public_message().into());
            cx.notify();
            return;
        }
        self.page_subscription = None;
        self.event_watch = None;
        self.event_sequence = 0;
        self.page = None;
        self.page_loaded = false;
        self.page_ready = false;
        self.page_open = None;
        self.pending_updates.clear();
        self.last_heartbeat = Instant::now();
        #[cfg(feature = "right-rail-acceptance")]
        {
            self.acceptance_dom = Value::Null;
        }
        let result = PluginWebViewResourceDocument::from_prepared(
            &self.prepared,
            &self.identity.view,
            self.native_identity(),
        )
        .and_then(|document| {
            Ok(document.with_initialization_script(browser_bootstrap(
                &self.identity,
                self.page_epoch,
                &self.locale,
                theme_name(cx),
            )?))
        })
        .and_then(|document| PluginWebView::create(document, window, cx));
        match result {
            Ok(page) => {
                self.page_subscription = Some(cx.subscribe_in(
                    &page,
                    window,
                    |renderer, _, event, window, cx| renderer.page_event(event, window, cx),
                ));
                self.page = Some(page);
                self.failure = None;
                self.sync_visibility(window, cx);
            }
            Err(error) => self.failure = Some(runtime_guidance(error.public_message())),
        }
        cx.notify();
    }

    /// 页面重载撤销所有旧请求，永不自动重放网页已发出的副作用。
    fn replace_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(next) = self
            .page_epoch
            .checked_add(1)
            .filter(|next| *next <= 9_007_199_254_740_991)
        else {
            self.failure = Some("页面重载代次已耗尽".into());
            cx.notify();
            return;
        };
        self.bridge.close();
        self.page_cancellation.cancel();
        self.page_cancellation = CancellationToken::new();
        self.page_epoch = next;
        match WebViewBridgeSession::new(self.identity.clone(), next) {
            Ok(bridge) => self.bridge = bridge,
            Err(error) => {
                self.failure = Some(error.public_message().into());
                return;
            }
        }
        self.create_page(window, cx);
    }

    /// 原生加载事件仅表示脚本可运行；仍须收到页面 view.open 回执才开放动作。
    fn page_event(
        &mut self,
        event: &PluginWebViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let expected = self.native_identity();
        match event {
            PluginWebViewEvent::Loaded { instance_id } if *instance_id == expected => {
                if self.page_loaded {
                    // F5 等引擎内重载也必须生成新桥接身份，不能复用浏览器重新开始的 p:1。
                    self.replace_page(window, cx);
                    return;
                }
                self.page_loaded = true;
                self.send_environment(cx);
                self.initialize_page(window, cx);
            }
            PluginWebViewEvent::Message { instance_id, body } if *instance_id == expected => {
                #[cfg(feature = "right-rail-acceptance")]
                if let Ok(frame) = serde_json::from_str::<Value>(body)
                    && frame.get("view_instance").and_then(Value::as_str)
                        == Some(self.identity.instance.as_str())
                    && frame.get("generation").and_then(Value::as_u64) == Some(self.page_epoch)
                    && let Some(dom) = frame.get("acceptance_dom")
                {
                    self.acceptance_dom = dom.clone();
                    cx.notify();
                    return;
                }
                match self.bridge.accept(body) {
                    Ok(WebViewBridgeMessage::Request {
                        id,
                        method,
                        params,
                        cancellation,
                        sequence,
                    }) => self.page_request(
                        PageRequest {
                            id,
                            sequence,
                            method,
                            params,
                            cancellation,
                        },
                        window,
                        cx,
                    ),
                    Ok(WebViewBridgeMessage::Response(response)) => {
                        if response.get("id").and_then(Value::as_str) == self.page_open.as_deref() {
                            self.page_open = None;
                            if response.get("error").is_some() {
                                self.failure = Some("页面初始化失败，请重新加载".into());
                            } else {
                                self.page_ready = true;
                                self.failure = None;
                                for patch in std::mem::take(&mut self.pending_updates) {
                                    let frame = self.bridge.notification(
                                        "view.update",
                                        json!({"instance":self.identity.instance,"patch":patch}),
                                    );
                                    self.post(&frame, cx);
                                }
                                self.send_visibility(window, cx);
                            }
                        }
                    }
                    Ok(WebViewBridgeMessage::CommandPalette) if self.visible => {
                        window.dispatch_action(Box::new(OpenWebViewCommandPalette), cx)
                    }
                    Ok(WebViewBridgeMessage::Heartbeat) => {
                        self.last_heartbeat = Instant::now();
                        return;
                    }
                    Ok(_) => {}
                    // 不把恶意、过期或重复信封的错误回送到同 ID 的合法请求。
                    Err(_) => {}
                }
            }
            PluginWebViewEvent::ExternalNavigation { instance_id, url }
                if *instance_id == expected =>
            {
                self.open_external(url.clone(), window, cx)
            }
            PluginWebViewEvent::Failed { instance_id, .. } if *instance_id == expected => {
                self.abort_page_requests(cx);
                self.page_ready = false;
                self.failure = Some("页面已停止响应，请重新加载".into());
            }
            _ => return,
        }
        self.sync_visibility(window, cx);
        cx.notify();
    }

    /// 回执发送失败立即撤销原生输入；只在错误变化时通知，避免主题重绘造成循环。
    fn post(&mut self, frame: &Value, cx: &mut Context<Self>) {
        if let Some(page) = &self.page
            && let Err(error) = page.read(cx).post_message(frame)
        {
            let message = error.public_message().to_owned();
            let changed = self.failure.as_ref() != Some(&message);
            self.failure = Some(message);
            self.page_ready = false;
            if changed {
                let owner = self.owner;
                let this = cx.weak_entity();
                cx.defer(move |cx| {
                    let _ = owner.update(cx, |_, window, cx| {
                        this.update(cx, |renderer, cx| renderer.sync_visibility(window, cx))
                    });
                });
                cx.notify();
            }
        }
    }

    /// 主题令牌来自 Kit 的同一语义主题；CSS 由受限初始化监听器投影。
    fn send_environment(&mut self, cx: &mut Context<Self>) {
        let theme = cx.theme();
        let tokens = json!({"--k-bg":theme.background.to_string(),"--k-fg":theme.foreground.to_string(),"--k-accent":theme.primary.to_string(),"--k-surface":theme.input.to_string(),"--k-border":theme.border.to_string(),"--k-button":theme.secondary.to_string(),"--k-button-fg":theme.secondary_foreground.to_string(),"--k-hover":theme.accent.to_string(),"--k-disabled-fg":theme.muted_foreground.to_string(),"--k-focus":theme.ring.to_string(),"--k-radius":format!("{}px",f32::from(theme.radius)),"--k-font":format!("{}px {}", f32::from(theme.font_size),theme.font_family)});
        let frame = self.bridge.notification(
            "theme.changed",
            json!({"theme":theme_name(cx),"tokens":tokens}),
        );
        self.post(&frame, cx);
        let frame = self.bridge.notification(
            "environment.changed",
            json!({"theme":theme_name(cx),"locale":self.locale}),
        );
        self.post(&frame, cx);
    }

    /// 显隐仅改变当前子窗，隐藏时保留 DOM、滚动和草稿；异步通知不阻塞导航。
    pub(crate) fn visibility_changed(
        &mut self,
        visible: bool,
        focused: bool,
        cx: &mut Context<Self>,
    ) {
        if visible && !self.visible {
            self.last_heartbeat = Instant::now();
        }
        self.visible = visible;
        self.focused = visible && focused;
        let owner = self.owner;
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = owner.update(cx, |_, window, cx| {
                this.update(cx, |renderer, cx| renderer.send_visibility(window, cx))
            });
        });
    }

    /// 原生显隐必须带所属窗口，恢复过程保留网页可见以避免重建或覆盖草稿。
    fn sync_visibility(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let visible = self.visible && self.failure.is_none();
        if let Some(page) = &self.page
            && let Err(error) = page.update(cx, |page, cx| page.set_visible(visible, window, cx))
        {
            self.failure = Some(error.public_message().into());
        }
    }

    /// 网页与进程接收相同显隐快照，不从当前焦点推断实例归属。
    fn send_visibility(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_visibility(window, cx);
        if self.page_loaded {
            let frame = self.bridge.notification("view.visibility", json!({"instance":self.identity.instance,"visible":self.visible,"focused":self.focused}));
            self.post(&frame, cx);
        }
        if self.process_generation.is_none() {
            return;
        }
        let runtime = self.runtime.clone();
        let executor = self.executor.clone();
        let request = RuntimeRequest {
            identity: self.identity.clone(),
            process_generation: self.process_generation,
            method: "view.visibility".into(),
            params: json!({"visible":self.visible,"focused":self.focused}),
            deadline: Instant::now() + OPEN_TIMEOUT,
            cancellation: self.cancellation.child_token(),
        };
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(request.cancellation.clone(), move |token| {
                    runtime.notify(RuntimeRequest {
                        cancellation: token,
                        ..request
                    })
                })
                .await;
            if let Err(error) = result {
                let _ = this.update(cx, |renderer, cx| {
                    renderer.error = Some(error.public_message().into());
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 保存只读取应用确认状态；启动失败时原有恢复数据仍可回存。
    pub(crate) fn cached_state(&self) -> AppResult<Option<PluginViewSavedState>> {
        match self.runtime.cached_state(&self.identity) {
            Ok(Some(state)) => Ok(Some(state)),
            Ok(None) => Ok(self.saved_state.clone()),
            Err(_) if self.saved_state.is_some() => Ok(self.saved_state.clone()),
            Err(error) => Err(error),
        }
    }

    /// 关闭请求沿用宿主绝对截止时间；在途打开必须先完成，不能先关闭后迟到登记。
    pub(crate) fn close(&self, deadline: Instant) -> RuntimeFuture<()> {
        if self.open_in_flight || self.reopening {
            return Box::pin(async {
                Err(AppError::conflict("插件视图仍在打开，请稍后关闭"))
            });
        }
        if !self.opened {
            return Box::pin(async { Ok(()) });
        }
        let request = RuntimeRequest {
            identity: self.identity.clone(),
            process_generation: self.process_generation,
            method: "view.close".into(),
            params: json!({}),
            deadline,
            cancellation: CancellationToken::new(),
        };
        let runtime = self.runtime.clone();
        Box::pin(async move { runtime.request(request).await.map(|_| ()) })
    }

    /// 关闭开始即停止接收新动作并取消旧动作，关闭失败后再开放显式重试。
    pub(crate) fn begin_close(
        &mut self,
        deadline: Instant,
        cx: &mut Context<Self>,
    ) -> RuntimeFuture<()> {
        let future = self.close(deadline);
        self.closing.store(true, Ordering::Release);
        self.page_cancellation.cancel();
        self.page_cancellation = CancellationToken::new();
        self.abort_page_requests(cx);
        let closing = self.closing.clone();
        Box::pin(async move {
            let result = future.await;
            if result.is_err() {
                closing.store(false, Ordering::Release);
            }
            result
        })
    }

    /// 失败与恢复先结算网页等待者，防止保留 DOM 后旧按钮永远无法重试。
    fn abort_page_requests(&mut self, cx: &mut Context<Self>) {
        self.event_watch = None;
        self.page_cancellation.cancel();
        self.page_cancellation = CancellationToken::new();
        for frame in self.bridge.abort_requests() {
            self.post(&frame, cx);
        }
    }

    /// 验收读取真实承载实例，不能用截图探针替代正式生命周期状态。
    #[cfg(feature = "right-rail-acceptance")]
    pub fn acceptance_snapshot(&self, cx: &App) -> Value {
        json!({"instance":self.identity.instance,"pageEpoch":self.page_epoch,"processGeneration":self.process_generation,"pageReady":self.page_ready,"opened":self.opened,"failure":self.failure,"error":self.error,"dom":self.acceptance_dom,"native":self.page.as_ref().map(|page| page.read(cx).snapshot())})
    }

    /// 探针局部 report 函数自动附加当前页面身份，回执仍经原生 IPC 和生产事件队列。
    #[cfg(feature = "right-rail-acceptance")]
    pub(crate) fn acceptance_evaluate(&self, script: &str, cx: &App) -> AppResult<()> {
        let identity = json!({"view_instance":self.identity.instance,"generation":self.page_epoch});
        let script = format!(
            "(()=>{{const report=(value)=>window.ipc.postMessage(JSON.stringify({{...{identity},acceptance_dom:value}}));{script}\n}})();"
        );
        self.page
            .as_ref()
            .ok_or_else(|| AppError::not_found("网页实例"))?
            .read(cx)
            .evaluate_acceptance_script(&script)
    }

    /// 手动重载复用生产恢复入口，不在验收中手工改写页面代次。
    #[cfg(feature = "right-rail-acceptance")]
    pub(crate) fn acceptance_reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_page(window, cx);
    }
}

impl Drop for WebViewRenderer {
    /// 撤下实体立即取消能力请求；原生页面由同一窗口线程拥有并随字段释放。
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.page_cancellation.cancel();
        self.bridge.close();
    }
}

impl Render for WebViewRenderer {
    /// 恢复条叠在网页上方，不在正常读取或局部刷新时插入占位行。
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let locale = gpui_kit::component::locale().to_string();
        if self.locale != locale {
            self.locale = locale;
            self.send_environment(cx);
        }
        let mut root = div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground);
        if let Some(message) = self.failure.as_ref().or(self.error.as_ref()) {
            root = root.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_3()
                    .text_sm()
                    .child(message.clone())
                    .child(
                        Button::new("webview-retry")
                            .label(if self.failure.is_some() {
                                "重新加载"
                            } else {
                                "重试"
                            })
                            .ghost()
                            .on_click(
                                cx.listener(|renderer, _, window, cx| renderer.retry(window, cx)),
                            ),
                    ),
            );
        }
        if let Some(page) = &self.page {
            root = root.child(div().flex_1().min_h_0().min_w_0().child(page.clone()));
        }
        root
    }
}

/// 平台安装指引随局部失败展示，避免普通视图承担网页运行时缺失的后果。
fn runtime_guidance(message: &str) -> String {
    if cfg!(windows) {
        format!("{message}。请确认已安装 Microsoft Edge WebView2 Runtime，再重新加载。")
    } else if cfg!(target_os = "linux") {
        format!("{message}。请确认 WebKitGTK 4.1 已安装，并使用 X11/XWayland 会话。")
    } else {
        format!("{message}。请重新加载页面；若仍失败，请重启应用。")
    }
}

/// 只传递 SDK 认识的固定主题名称，系统主题已由 Kit 解析为实际明暗。
fn theme_name(cx: &App) -> &'static str {
    if cx.theme().is_dark() {
        "dark"
    } else {
        "light"
    }
}

/// 运行服务错误映射到协议类别，保持取消、失效和参数错误的可辨识性。
fn rpc_error(error: AppError) -> (i32, String) {
    let code = match error {
        AppError::Cancelled { .. } => -32002,
        AppError::Conflict { .. } | AppError::NotFound { .. } => -32004,
        AppError::Invalid { .. } => -32602,
        AppError::Unsupported { .. } => -32601,
        _ => -32603,
    };
    (code, error.public_message().to_owned())
}
