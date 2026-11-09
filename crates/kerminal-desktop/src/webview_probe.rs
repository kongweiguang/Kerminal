//! 系统 WebView 的隔离原生探针；页面资源随可执行文件冻结，不接触用户插件或配置。
//!
//! @author kongweiguang

use std::{
    cell::RefCell,
    error::Error,
    ffi::{OsStr, OsString},
    fs, io,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use gpui_kit::{
    AnyWindowHandle, AppContext as _, AsyncApp, Bounds, Context, DismissEvent, Entity, FocusHandle,
    Focusable as _, IntoElement, Render, Subscription, Window, WindowBounds, WindowOptions,
    component::{
        Theme, ThemeMode, TitleBar, WindowExt as _, button::Button, menu::PopupMenu,
        popover::Popover,
    },
    div,
    prelude::*,
    px, size,
};
use kerminal_ui::plugins::webview::{
    PluginWebView, PluginWebViewEvent, PluginWebViewResource, PluginWebViewResourceDocument,
    PluginWebViewSnapshot,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[cfg(windows)]
#[allow(dead_code)]
#[path = "right_rail_acceptance/isolation.rs"]
mod isolation;
#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "plugin_acceptance/x11_isolation.rs"]
mod isolation;
#[cfg(windows)]
#[path = "webview_probe/pixels.rs"]
mod pixels;
#[cfg(windows)]
#[path = "webview_probe/wgc.rs"]
mod wgc;

/// 独立动作只操作本探针窗口，避免向产品命令表注册临时验收命令。
#[derive(Clone, Debug, PartialEq, gpui_kit::Action)]
#[action(name = "kerminal.webviewProbe.openMenu", no_json, no_register)]
struct OpenMenu;
/// 菜单关闭后同一网页实体继续持有草稿。
#[derive(Clone, Debug, PartialEq, gpui_kit::Action)]
#[action(name = "kerminal.webviewProbe.closeMenu", no_json, no_register)]
struct CloseMenu;

struct Options {
    output: PathBuf,
    theme: String,
    headless: bool,
}

/// 只接管探针专用参数；未知参数明确失败，普通应用入口保持原样。
pub(crate) fn run_from_args<I>(arguments: I) -> Option<i32>
where
    I: IntoIterator<Item = OsString>,
{
    let args: Vec<_> = arguments.into_iter().collect();
    if args.first().map(OsString::as_os_str) != Some(OsStr::new("--webview-probe")) {
        return None;
    }
    Some(match parse_options(&args[1..]).and_then(run) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("WebView 探针失败：{error}");
            1
        }
    })
}

/// 输出目录默认唯一且只能新建，防止覆盖其他验收批次；无截图握手的模式仍运行真实 WebView。
fn parse_options(args: &[OsString]) -> Result<Options, Box<dyn Error>> {
    let mut output = None;
    let mut theme = None;
    let mut headless = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--output" && output.is_none() {
            output = Some(PathBuf::from(
                iter.next()
                    .ok_or_else(|| io::Error::other("缺少输出目录"))?,
            ));
        } else if arg == "--theme" && theme.is_none() {
            theme = Some(
                iter.next()
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| io::Error::other("缺少主题"))?
                    .to_owned(),
            );
        } else if arg == "--headless-check" && !headless {
            headless = true;
        } else {
            return Err(io::Error::other("无效或重复的 WebView 探针参数").into());
        }
    }
    let theme = theme.unwrap_or_else(|| "system".to_owned());
    if !matches!(theme.as_str(), "light" | "dark" | "system") {
        return Err(io::Error::other("主题只能是 light、dark 或 system").into());
    }
    let output = output.unwrap_or_else(|| {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "kerminal-webview-probe-{}-{stamp}",
            std::process::id()
        ))
    });
    if !output.is_absolute() || output.exists() {
        return Err(io::Error::other("输出目录必须是不存在的绝对路径").into());
    }
    Ok(Options {
        output,
        theme,
        headless,
    })
}

/// 各资源沿同一受限协议加载，模块导入和 CSS 生效均由页面真实回执验证。
fn document() -> kerminal_application::AppResult<PluginWebViewResourceDocument> {
    let files: [(&str, &str, &[u8]); 4] = [
        (
            "index.html",
            "text/html; charset=utf-8",
            include_bytes!("../../../tests/fixtures/plugins/webview-probe/index.html"),
        ),
        (
            "styles.css",
            "text/css; charset=utf-8",
            include_bytes!("../../../tests/fixtures/plugins/webview-probe/styles.css"),
        ),
        (
            "main.js",
            "text/javascript; charset=utf-8",
            include_bytes!("../../../tests/fixtures/plugins/webview-probe/main.js"),
        ),
        (
            "bridge.js",
            "text/javascript; charset=utf-8",
            include_bytes!("../../../tests/fixtures/plugins/webview-probe/bridge.js"),
        ),
    ];
    PluginWebViewResourceDocument::new(
        "acceptance.webview".into(),
        "probe-instance".into(),
        "index.html".into(),
        files.into_iter().map(|(path, content_type, bytes)| {
            (
                path.into(),
                PluginWebViewResource {
                    content_type,
                    bytes: Arc::from(bytes),
                },
            )
        }),
    )
}

struct ProbeView {
    webview: Option<Entity<PluginWebView>>,
    shown: bool,
    focus: FocusHandle,
    menu: Entity<PopupMenu>,
    menu_open: bool,
    _subscription: Option<Subscription>,
    _menu_subscription: Subscription,
}
impl ProbeView {
    /// 真实 Popover 的宿主状态由同一动作控制，避免用普通矩形伪装菜单。
    fn open_menu(&mut self, _: &OpenMenu, _: &mut Window, cx: &mut Context<Self>) {
        self.menu_open = true;
        cx.notify();
    }
    /// 关闭只改变宿主菜单，不创建新的 WebView 实例。
    fn close_menu(&mut self, _: &CloseMenu, _: &mut Window, cx: &mut Context<Self>) {
        self.menu_open = false;
        cx.notify();
    }
}
/// 饱和颜色供脚本识别真实 GPUI 浮层，不能只凭空白截图声称合成成功。
fn overlay_marker() -> impl IntoElement {
    div()
        .w(px(168.))
        .h(px(28.))
        .bg(gpui_kit::rgb(0xe63a91))
        .text_color(gpui_kit::rgb(0xffffff))
        .child("保留当前草稿")
}
impl Render for ProbeView {
    /// 持续挂载同一实体，隐藏期间仅撤销其布局与输入。
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let menu = self.menu.clone();
        div()
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::open_menu))
            .on_action(cx.listener(Self::close_menu))
            .size_full()
            .flex()
            .flex_col()
            .p_6()
            .gap_4()
            .child(
                div()
                    .h_8()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child("Kerminal · WebView 探针")
                    .child(
                        Popover::new("webview-probe-menu")
                            .appearance(false)
                            .trigger_on_mouse_down(false)
                            .open(self.menu_open)
                            .on_open_change(cx.listener(|this, open, _, cx| {
                                this.menu_open = *open;
                                cx.notify();
                            }))
                            .track_focus(&self.menu.focus_handle(cx))
                            .trigger(
                                Button::new("webview-probe-menu-trigger")
                                    .label("菜单")
                                    .on_click(|_, window, app| {
                                        window.dispatch_action(Box::new(OpenMenu), app)
                                    }),
                            )
                            .content(move |_, _, _| menu.clone()),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .overflow_hidden()
                    .when(self.shown, |node| node.children(self.webview.clone())),
            )
    }
}

/// Windows/Linux 必须先证明独立显示环境；macOS 探针仅允许显式隐藏窗口检查并如实记录未证明桌面隔离。
fn isolation_evidence(headless: bool) -> Result<Value, io::Error> {
    #[cfg(any(windows, target_os = "linux"))]
    {
        let _ = headless;
        let value = serde_json::to_value(isolation::capture()?).map_err(io::Error::other)?;
        #[cfg(windows)]
        if background_composition() {
            if value["uiThreadDesktopName"] != value["inputDesktopName"]
                || value["foregroundWindowPid"] == std::process::id()
            {
                return Err(io::Error::other("后台取证窗口的桌面或前台归属无效"));
            }
            let mut value = value;
            value["mode"] = json!("background-composition");
            return Ok(value);
        }
        if value["isolationProven"] != true {
            return Err(io::Error::other("WebView 探针需要已证明的隔离桌面"));
        }
        Ok(value)
    }
    #[cfg(target_os = "macos")]
    {
        if !headless {
            return Err(io::Error::other(
                "macOS 缺少隔离桌面，请使用 --headless-check",
            ));
        }
        Ok(json!({"isolationProven":false,"mode":"hidden-window"}))
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = headless;
        Err(io::Error::other("当前平台不支持 WebView 探针"))
    }
}

/// 窗口创建前完成隔离检查和专属配置，验收输出与用户现有配置没有共享路径。
fn run(options: Options) -> Result<(), Box<dyn Error>> {
    let isolation = isolation_evidence(options.headless)?;
    fs::create_dir(&options.output)?;
    let config = options.output.join("config");
    fs::create_dir(&config)?;
    // SAFETY: 在启动 GPUI 与任何由本探针创建的线程之前设置进程私有环境。
    unsafe {
        std::env::set_var("KERMINAL_CONFIG_ROOT", &config);
        std::env::set_var(
            "WEBVIEW2_USER_DATA_FOLDER",
            options.output.join("webview-profile"),
        );
    }
    let output_path = options.output.clone();
    let hash = format!("{:x}", Sha256::digest(fs::read(std::env::current_exe()?)?));
    let failure = Arc::new(Mutex::new(None::<String>));
    let saved = failure.clone();
    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(move |cx| {
        kerminal_ui::init(cx);
        let events = Rc::new(RefCell::new(Vec::<Value>::new()));
        let observed = events.clone();
        let fail_open = failure.clone();
        let requested_theme = options.theme.clone();
        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds { origin: gpui_kit::point(px(40.), px(40.)), size: size(px(1024.), px(720.)) })),
            show: !cfg!(target_os = "macos") && !background_composition(), focus: false, ..TitleBar::window_options()
        };
        let result = gpui_kit::open_window(window_options, cx, |window, cx| {
            let is_dark = match requested_theme.as_str() {
                "dark" => true,
                "light" => false,
                _ => matches!(window.appearance(), gpui_pre::WindowAppearance::Dark | gpui_pre::WindowAppearance::VibrantDark),
            };
            Theme::change(if is_dark { ThemeMode::Dark } else { ThemeMode::Light }, None, cx);
            let webview = match document().and_then(|doc| PluginWebView::create(doc, window, cx)) {
                Ok(view) => Some(view),
                Err(error) => { *fail_open.lock().unwrap_or_else(|error| error.into_inner()) = Some(error.to_string()); cx.quit(); None }
            };
            let subscription = webview.as_ref().map(|view| cx.subscribe(view, move |_, event: &PluginWebViewEvent, _| {
                match event {
                    PluginWebViewEvent::Message { body, .. } => if let Ok(value) = serde_json::from_str(body) { observed.borrow_mut().push(value); },
                    PluginWebViewEvent::Failed { message, .. } => observed.borrow_mut().push(json!({"failure":message})),
                    _ => {}
                }
            }));
            let menu = PopupMenu::build(window, cx, |menu, _, _| menu.menu_element(Box::new(CloseMenu), |_, _| overlay_marker()));
            let root = cx.new(|cx| {
                let menu_subscription = cx.subscribe(&menu, |this: &mut ProbeView, _, _: &DismissEvent, cx| { this.menu_open = false; cx.notify(); });
                ProbeView { webview, shown: true, focus: cx.focus_handle(), menu, menu_open: false, _subscription: subscription, _menu_subscription: menu_subscription }
            });
            let focus = root.read(cx).focus.clone();
            focus.focus(window, cx);
            root
        });
        let (window, root) = match result {
            Ok(value) => value,
            Err(error) => { *failure.lock().unwrap_or_else(|error| error.into_inner()) = Some(error.to_string()); cx.quit(); return; }
        };
        let Some(view) = root.read(cx).webview.clone() else { cx.quit(); return; };
        #[cfg(windows)]
        if background_composition() {
            let parent = view.read(cx).snapshot().native_parent_id;
            let scale = match window.update(cx, |_, window, _| window.scale_factor()) {
                Ok(scale) => scale,
                Err(error) => {
                    *failure.lock().unwrap_or_else(|error| error.into_inner()) = Some(error.to_string());
                    cx.quit();
                    return;
                }
            };
            if let Err(error) = show_background_window(parent, scale) {
                *failure.lock().unwrap_or_else(|error| error.into_inner()) = Some(error.to_string());
                cx.quit();
                return;
            }
        }
        cx.spawn(async move |cx| {
            let result = exercise(cx, window, &root, &view, &events, &options).await;
            let report = match result {
                Ok(evidence) => json!({"ok":true,"processId":std::process::id(),"candidateSha256":hash,"theme":options.theme,"nativeIsolation":isolation,"evidence":evidence,"nativeImeTested":false,"visualCaptureRequired":!options.headless}),
                Err(error) => { let message = error.to_string(); *failure.lock().unwrap_or_else(|error| error.into_inner()) = Some(message.clone()); json!({"ok":false,"processId":std::process::id(),"error":message}) }
            };
            let output = options.output.join("report.json");
            let write_result = cx.background_executor().spawn(async move { fs::write(output, serde_json::to_vec_pretty(&report).map_err(io::Error::other)?) }).await;
            if let Err(error) = write_result { *failure.lock().unwrap_or_else(|error| error.into_inner()) = Some(error.to_string()); }
            cx.update(|cx| cx.quit());
        }).detach();
    });
    let result = saved
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    match result {
        Some(error) => Err(io::Error::other(error).into()),
        None => {
            println!("WebView 探针报告：{}", output_path.display());
            Ok(())
        }
    }
}

/// 使用实际页面回执、真实菜单与对话框，分别验证尺寸、背板及恢复后的实例身份。
async fn exercise(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    root: &Entity<ProbeView>,
    view: &Entity<PluginWebView>,
    events: &Rc<RefCell<Vec<Value>>>,
    options: &Options,
) -> Result<Value, Box<dyn Error>> {
    wait(cx, window, view, events, |state, values| {
        state.visible && state.loaded && values.iter().any(|v| v["request_id"] == "ready")
    })
    .await?;
    let ready = receipt(events, "ready")?;
    if ready["background"] != "rgb(18, 52, 86)" {
        return Err(io::Error::other("冻结的样式资源未生效").into());
    }
    let before = snapshot(cx, view);
    checkpoint(cx, window, view, events, options, "ready", &ready).await?;
    view.update(cx, |view, _| {
        view.post_message(&json!({"operation":"draft","value":"保留中文草稿","request_id":"draft"}))
    })?;
    wait(cx, window, view, events, |_, values| {
        values.iter().any(|v| v["request_id"] == "draft")
    })
    .await?;
    window.update(cx, |_, window, app| {
        view.update(app, |view, cx| view.set_visible(false, window, cx))
    })??;
    root.update(cx, |root, cx| {
        root.shown = false;
        cx.notify();
    });
    wait(cx, window, view, events, |state, _| {
        !state.engine_visible && !state.backdrop_pending
    })
    .await?;
    root.update(cx, |root, cx| {
        root.shown = true;
        cx.notify();
    });
    window.update(cx, |_, window, app| {
        view.update(app, |view, cx| view.set_visible(true, window, cx))
    })??;
    wait(cx, window, view, events, restored).await?;
    let after_hide = inspect(cx, window, view, events, "after-hide").await?;
    if after_hide["boot"] != ready["boot"]
        || after_hide["draft"] != "保留中文草稿"
        || snapshot(cx, view).native_id != before.native_id
    {
        return Err(io::Error::other("恢复后网页实例或草稿发生变化").into());
    }
    window.update(cx, |_, window, _| window.resize(size(px(900.), px(650.))))?;
    wait(cx, window, view, events, |state, _| {
        state
            .bounds
            .zip(before.bounds)
            .is_some_and(|(new, old)| new.width < old.width)
    })
    .await?;
    let resized = settled_geometry(cx, window, view, events).await?;
    let after_resize = snapshot(cx, view);
    window.update(cx, |_, window, app| {
        window.dispatch_action(Box::new(OpenMenu), app)
    })?;
    wait(cx, window, view, events, backdrop_ready).await?;
    if !root.read_with(cx, |root, _| root.menu_open) {
        return Err(io::Error::other("PopupMenu 未打开").into());
    }
    let menu = snapshot(cx, view);
    checkpoint(cx, window, view, events, options, "menu", &after_hide).await?;
    window.update(cx, |_, window, app| {
        window.dispatch_action(Box::new(CloseMenu), app)
    })?;
    wait(cx, window, view, events, restored).await?;
    window.update(cx, |_, window, app| {
        window.open_dialog(app, |dialog, _, _| {
            dialog.title("WebView 浮层验收").child(overlay_marker())
        })
    })?;
    wait(cx, window, view, events, backdrop_ready).await?;
    let dialog = snapshot(cx, view);
    checkpoint(cx, window, view, events, options, "dialog", &after_hide).await?;
    window.update(cx, |_, window, app| window.close_dialog(app))?;
    wait(cx, window, view, events, restored).await?;
    for _ in 0..20 {
        if snapshot(cx, view).backdrop_slow_fallback {
            break;
        }
        window.update(cx, |_, window, app| {
            view.update(app, |view, cx| view.set_occluded(true, window, cx))
        })??;
        wait(cx, window, view, events, backdrop_ready).await?;
        window.update(cx, |_, window, app| {
            view.update(app, |view, cx| view.set_occluded(false, window, cx))
        })??;
        wait(cx, window, view, events, restored).await?;
    }
    let final_page = inspect(cx, window, view, events, "final").await?;
    let final_state = snapshot(cx, view);
    if final_page["boot"] != ready["boot"]
        || final_page["draft"] != "保留中文草稿"
        || final_state.native_id != before.native_id
    {
        return Err(io::Error::other("浮层恢复后网页实例或草稿发生变化").into());
    }
    let mut samples = final_state.backdrop_capture_ms.clone();
    if samples.len() < 20 {
        return Err(io::Error::other("有效原生截图样本不足 20 次").into());
    }
    samples.sort_unstable_by(f64::total_cmp);
    let p95_ms = samples[(samples.len() * 95).div_ceil(100) - 1];
    Ok(
        json!({"ready":ready,"before":before,"afterHide":after_hide,"resized":resized,"afterResize":after_resize,"menu":menu,"dialog":dialog,"finalPage":final_page,"finalState":final_state,"sameInstance":true,"programmaticDraftOnly":true,"captureSamples":samples.len(),"captureP95Ms":p95_ms}),
    )
}

/// 截图超限后的主题底色是有记录的降级，不能强迫降级状态继续生成背板。
fn backdrop_ready(state: &PluginWebViewSnapshot, _: &[Value]) -> bool {
    !state.visible
        && !state.backdrop_pending
        && (state.backdrop_visible || state.backdrop_slow_fallback)
}
/// 等待原生视图与截图任务均恢复，避免迟到的回执被误认为稳定状态。
fn restored(state: &PluginWebViewSnapshot, _: &[Value]) -> bool {
    state.visible && state.engine_visible && !state.backdrop_visible && !state.backdrop_pending
}
/// 页面读取使用单独请求编号，确保数据来自当次真实 JS 执行。
async fn inspect(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    view: &Entity<PluginWebView>,
    events: &Rc<RefCell<Vec<Value>>>,
    id: &str,
) -> Result<Value, Box<dyn Error>> {
    view.update(cx, |view, _| {
        view.post_message(&json!({"operation":"inspect","request_id":id}))
    })?;
    wait(cx, window, view, events, |_, values| {
        values.iter().any(|v| v["request_id"] == id)
    })
    .await?;
    Ok(receipt(events, id)?)
}
/// 多次读取引擎回执，直到实际 CSS 视口与当前物理布局和 DPI 一致。
async fn settled_geometry(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    view: &Entity<PluginWebView>,
    events: &Rc<RefCell<Vec<Value>>>,
) -> Result<Value, Box<dyn Error>> {
    for attempt in 0..40 {
        let page = inspect(cx, window, view, events, &format!("resized-{attempt}")).await?;
        let state = snapshot(cx, view);
        if let (Some(bounds), Some(width), Some(height), Some(dpr)) = (
            state.bounds,
            page["width"].as_f64(),
            page["height"].as_f64(),
            page["dpr"].as_f64(),
        ) {
            if (width * dpr - f64::from(bounds.width)).abs() <= 2.
                && (height * dpr - f64::from(bounds.height)).abs() <= 2.
            {
                return Ok(page);
            }
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
    Err(io::Error::other("网页实际视口未能与宿主布局和 DPI 对齐").into())
}

/// 定时刷新真实窗口并检查失败，等待有界且不借用用户桌面。
async fn wait(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    view: &Entity<PluginWebView>,
    events: &Rc<RefCell<Vec<Value>>>,
    predicate: impl Fn(&PluginWebViewSnapshot, &[Value]) -> bool,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        #[cfg(windows)]
        if background_composition() {
            ensure_background_window()?;
        }
        window.update(cx, |_, window, app| {
            window.refresh();
            let frame = window.draw(app);
            frame.clear(app);
        })?;
        let state = snapshot(cx, view);
        if let Some(error) = state.failure.as_ref() {
            return Err(io::Error::other(error.clone()).into());
        }
        if !state.backdrop_slow_fallback
            && let Some(error) = state.backdrop_failure.as_ref()
        {
            return Err(io::Error::other(error.clone()).into());
        }
        if predicate(&state, &events.borrow()) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待 WebView 超时：{state:?}")).into());
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
}
/// 报告直接读取生产实体快照，避免以宿主意图代替原生状态。
fn snapshot(cx: &AsyncApp, view: &Entity<PluginWebView>) -> PluginWebViewSnapshot {
    view.read_with(cx, |view, _| view.snapshot())
}
/// 回执按编号匹配，缺失时明确失败而不复用其他阶段结果。
fn receipt(events: &Rc<RefCell<Vec<Value>>>, id: &str) -> Result<Value, io::Error> {
    events
        .borrow()
        .iter()
        .rfind(|value| value["request_id"] == id)
        .cloned()
        .ok_or_else(|| io::Error::other("缺少网页回执"))
}
/// 仅验收脚本显式启用时使用输入桌面的后台合成；普通探针仍要求私有桌面。
fn background_composition() -> bool {
    cfg!(windows) && std::env::var_os("KERMINAL_WEBVIEW_BACKGROUND").is_some_and(|v| v == "1")
}

/// 每次等待及截图前检查前台不是候选进程，避免后台探针意外抢走用户输入。
#[cfg(windows)]
fn ensure_background_window() -> Result<(), io::Error> {
    let evidence = serde_json::to_value(isolation::capture()?).map_err(io::Error::other)?;
    if evidence["foregroundWindowPid"] == std::process::id() {
        return Err(io::Error::other("后台 WebView 探针意外获得前台焦点"));
    }
    Ok(())
}

/// 窗口创建时保持隐藏，设置 NOACTIVATE 后放到 Z 序底部再显示，避免抢占前台。
#[cfg(windows)]
fn show_background_window(parent: u64, scale: f32) -> Result<(), io::Error> {
    use windows_sys::Win32::{
        Foundation::{GetLastError, SetLastError},
        UI::WindowsAndMessaging::{
            GWL_EXSTYLE, GetWindowLongPtrW, GetWindowThreadProcessId, HWND_BOTTOM, SWP_NOACTIVATE,
            SWP_SHOWWINDOW, SetWindowLongPtrW, SetWindowPos, WS_EX_NOACTIVATE,
        },
    };
    let hwnd = parent as windows_sys::Win32::Foundation::HWND;
    let mut owner = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut owner) };
    if owner != std::process::id() {
        return Err(io::Error::other("后台窗口不属于候选进程"));
    }
    ensure_background_window()?;
    unsafe {
        SetLastError(0);
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if style == 0 && GetLastError() != 0 {
            return Err(io::Error::last_os_error());
        }
        SetLastError(0);
        if SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | WS_EX_NOACTIVATE as isize) == 0
            && GetLastError() != 0
        {
            return Err(io::Error::last_os_error());
        }
        if SetWindowPos(
            hwnd,
            HWND_BOTTOM,
            40,
            40,
            (1024.0 * scale).round() as i32,
            (720.0 * scale).round() as i32,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    ensure_background_window()
}
/// 外部截图与该进程的指定阶段握手；隐藏检查跳过外部合成截图并单独标记未覆盖。
async fn checkpoint(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    view: &Entity<PluginWebView>,
    events: &Rc<RefCell<Vec<Value>>>,
    options: &Options,
    phase: &str,
    page: &Value,
) -> Result<(), Box<dyn Error>> {
    if options.headless {
        return Ok(());
    }
    window.update(cx, |_, window, app| {
        window.refresh();
        let frame = window.draw(app);
        frame.clear(app);
    })?;
    let state = snapshot(cx, view);
    #[cfg(windows)]
    {
        // WGC 位于拥有目标窗口的进程内；CPU 读回在后台执行，外部脚本独立复核单帧像素。
        let native_parent = state.native_parent_id;
        let bmp = options.output.join(format!("{phase}.bmp"));
        let temp = options.output.join(format!("{phase}.wgc.tmp"));
        let result = options.output.join(format!("{phase}.wgc.json"));
        // 子窗口帧仅用于诊断系统是否单独合成网页，验收仍以父窗口同帧像素为准。
        if phase == "ready" {
            if let Ok(child) = state.native_id.parse::<u64>() {
                let child_bmp = options.output.join("ready.child.bmp");
                let child_json = options.output.join("ready.child.wgc.json");
                cx.background_executor()
                    .spawn(async move {
                        let evidence = wgc::capture(child, &child_bmp, false, false);
                        fs::write(
                            child_json,
                            serde_json::to_vec_pretty(&evidence).map_err(io::Error::other)?,
                        )
                    })
                    .await?;
            }
        }
        let overlay = phase != "ready";
        let modal = phase == "dialog";
        cx.background_executor()
            .spawn(async move {
                let evidence = wgc::capture(native_parent, &bmp, overlay, modal);
                fs::write(
                    &temp,
                    serde_json::to_vec_pretty(&evidence).map_err(io::Error::other)?,
                )?;
                fs::rename(temp, result)
            })
            .await?;
    }
    #[cfg(windows)]
    let foreground = if background_composition() {
        ensure_background_window()?;
        Some(serde_json::to_value(isolation::capture()?)?)
    } else {
        None::<Value>
    };
    #[cfg(not(windows))]
    let foreground = None::<Value>;
    let marker = json!({"processId":std::process::id(),"phase":phase,"snapshot":state,"page":page,"backgroundSafety":foreground});
    let bytes = serde_json::to_vec_pretty(&marker)?;
    let temp = options.output.join(format!("{phase}.marker.tmp"));
    let path = options.output.join(format!("{phase}.marker.json"));
    cx.background_executor()
        .spawn(async move {
            fs::write(&temp, bytes)?;
            fs::rename(temp, path)
        })
        .await?;
    let ack = options.output.join(format!("{phase}.ack.json"));
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let file = ack.clone();
        let accepted = cx
            .background_executor()
            .spawn(async move {
                fs::read(file)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            })
            .await
            .is_some_and(|value| {
                value["processId"] == std::process::id()
                    && value["phase"] == phase
                    && value["processed"] == true
            });
        if accepted {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("外部系统截图未确认").into());
        }
        wait(cx, window, view, events, |_, _| true).await?;
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
}
