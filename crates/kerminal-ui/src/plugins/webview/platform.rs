//! @author kongweiguang

//! 仅桥接现有父窗口；Linux XCB/Xlib 共享 XID，不建立独立用户窗口。

use std::sync::mpsc::SyncSender;
use std::time::Duration;

use gpui_kit::Window;
use kerminal_application::{AppError, AppResult};
use wry::raw_window_handle::{HasWindowHandle, RawWindowHandle};

use super::{PluginWebViewBounds, PluginWebViewEvent, PluginWebViewSource};

#[cfg(target_os = "linux")]
mod linux_preview;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod preview;

pub(super) struct PreviewCapture {
    pub data: PreviewData,
    pub api_duration: Duration,
}

pub(super) enum PreviewData {
    // Linux 使用 Cairo 像素，其他平台的编码字节由各自异步截图回调提供。
    #[cfg_attr(not(windows), allow(dead_code))]
    Png(Vec<u8>),
    #[cfg(target_os = "macos")]
    Tiff(Vec<u8>),
    #[cfg(target_os = "linux")]
    Cairo {
        data: gtk::cairo::ImageSurfaceDataOwned,
        width: i32,
        height: i32,
        stride: i32,
        format: gtk::cairo::Format,
    },
}

/// Windows 直接持有 Wry 子窗；Linux 使用有真实 frame clock 的 GtkPlug 嵌入现有 X11 父窗。
pub(super) struct NativeWebView {
    webview: wry::WebView,
    #[cfg(windows)]
    parent_position: ParentPosition,
    #[cfg(target_os = "linux")]
    plug: gtk::Plug,
    #[cfg(target_os = "macos")]
    mac: macos::Surface,
}

impl std::ops::Deref for NativeWebView {
    type Target = wry::WebView;
    /// 消息与引擎接口仍复用 Wry，平台适配只覆盖原生子窗的几何和显隐。
    fn deref(&self) -> &Self::Target {
        &self.webview
    }
}

impl NativeWebView {
    /// 截图仅作为宿主浮层的静态背板；异步失败不污染网页生命周期，也不泵事件等待回调。
    pub(super) fn capture_preview(
        &self,
        callback: impl FnOnce(AppResult<PreviewCapture>) + 'static,
    ) -> AppResult<()> {
        #[cfg(windows)]
        {
            preview::capture(&self.webview, callback)
        }
        #[cfg(target_os = "linux")]
        {
            linux_preview::capture(&self.webview, callback)
        }
        #[cfg(target_os = "macos")]
        {
            macos::capture(&self.webview, callback)
        }
        #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
        {
            let _ = callback;
            Err(AppError::unsupported(
                "当前平台尚不支持原生 WebView 背板截图",
            ))
        }
    }

    /// GtkPlug 保留真实 frame clock；Windows 子容器移动还须通知引擎，单独更新 Bounds 不足够。
    pub(super) fn set_bounds(&mut self, bounds: wry::Rect) -> AppResult<()> {
        #[cfg(windows)]
        {
            self.webview
                .set_bounds(bounds)
                .map_err(|_| AppError::infrastructure("WebView 子窗口布局失败"))?;
            self.synchronize_parent_position()
        }
        #[cfg(target_os = "linux")]
        {
            use gtk::prelude::*;
            use wry::WebViewExtUnix as _;
            let scale = f64::from(self.plug.scale_factor().max(1));
            let position = bounds.position.to_logical::<i32>(scale);
            let size = bounds.size.to_logical::<i32>(scale);
            // GtkFixed 的下一轮布局会重读 request；只 size_allocate 会被初始 1x1 请求覆盖。
            self.plug
                .set_size_request(size.width.max(1), size.height.max(1));
            let view = self.webview.webview();
            view.set_size_request(size.width.max(1), size.height.max(1));
            if let Some(parent) = view.parent() {
                parent.set_size_request(size.width.max(1), size.height.max(1));
            }
            if let Some(window) = self.plug.window() {
                window.move_resize(
                    position.x,
                    position.y,
                    size.width.max(1),
                    size.height.max(1),
                );
            }
            self.plug.size_allocate(&gtk::Allocation::new(
                0,
                0,
                size.width.max(1),
                size.height.max(1),
            ));
            self.webview
                .set_bounds(wry::Rect {
                    position: wry::dpi::LogicalPosition::new(0, 0).into(),
                    size: bounds.size,
                })
                .map_err(|_| AppError::infrastructure("WebView 子窗口布局失败"))
        }
        #[cfg(target_os = "macos")]
        {
            self.mac.set_bounds(&self.webview, bounds)
        }
        #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
        {
            let _ = bounds;
            Err(AppError::unsupported("当前平台尚不支持内嵌 WebView 布局"))
        }
    }

    /// 只读本实例专用父 HWND 的实际屏幕原点；祖先移动和容器移动共用去重，纯尺寸变化不通知。
    pub(super) fn synchronize_parent_position(&mut self) -> AppResult<()> {
        #[cfg(windows)]
        {
            use wry::WebViewExtWindows as _;
            let origin = parent_client_origin(&self.webview)?;
            let webview = &self.webview;
            self.parent_position.synchronize(origin, || {
                // SAFETY: 此方法只在创建 WebView 的 GPUI UI 线程及其窗口生命周期内调用。
                unsafe { webview.controller().NotifyParentWindowPositionChanged() }
                    .map_err(|_| AppError::infrastructure("WebView 无法同步父窗口位置"))
            })?;
        }
        #[cfg(target_os = "macos")]
        self.mac.parent_resized(&self.webview)?;
        Ok(())
    }

    /// 计数只记录引擎成功接收的移动通知，供原生门禁区分位置同步与 DOM 几何回执。
    pub(super) fn parent_position_notifications(&self) -> u64 {
        #[cfg(windows)]
        {
            self.parent_position.notifications
        }
        #[cfg(not(windows))]
        {
            0
        }
    }

    /// 整个 Plug 一同撤下，隐藏网页本身还不足以撤销 GTK 原生子窗口的输入区域。
    pub(super) fn set_visible(&self, visible: bool) -> wry::Result<()> {
        #[cfg(target_os = "linux")]
        {
            use gtk::prelude::*;
            if visible {
                self.plug.show_all();
                // GtkPlug 只发 XEMBED_MAPPED 请求；GPUI 不是 GtkSocket，由宿主映射本实例子窗。
                if let Some(window) = self.plug.window() {
                    window.show_unraised();
                }
            } else {
                self.plug.hide();
            }
        }
        self.webview.set_visible(visible)
    }
}

#[cfg(windows)]
struct ParentPosition {
    origin: (i32, i32),
    notifications: u64,
}

#[cfg(windows)]
impl ParentPosition {
    /// 失败不能提前提交位置缓存，否则相同位置的恢复尝试会被误当作重复通知跳过。
    fn synchronize(
        &mut self,
        origin: (i32, i32),
        notify: impl FnOnce() -> AppResult<()>,
    ) -> AppResult<()> {
        if self.origin != origin {
            notify()?;
            self.origin = origin;
            self.notifications = self.notifications.saturating_add(1);
        }
        Ok(())
    }
}

/// 原点取自 WebView2 controller 的专用容器，包含 GPUI 祖先移动和子容器自身的布局位移。
#[cfg(windows)]
fn parent_client_origin(webview: &wry::WebView) -> AppResult<(i32, i32)> {
    use windows_sys::Win32::{Foundation::POINT, Graphics::Gdi::ClientToScreen};
    use wry::WebViewExtWindows as _;
    let mut parent = Default::default();
    let mut origin = POINT::default();
    // SAFETY: controller 由本 UI 线程持有，容器句柄仅在仍存活的 WebView 借用期间读取。
    unsafe {
        webview
            .controller()
            .ParentWindow(&mut parent)
            .map_err(|_| AppError::infrastructure("WebView 父窗口不可用"))?;
        if ClientToScreen(parent.0, &mut origin) == 0 {
            return Err(AppError::infrastructure("WebView 父窗口位置不可用"));
        }
    }
    Ok((origin.x, origin.y))
}

impl Drop for NativeWebView {
    /// GtkPlug 的原生容器由本适配器拥有，不能遗留在 GTK 的全局顶层列表中。
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            use gtk::prelude::*;
            // SAFETY: 这里只销毁本实例创建并拥有的 Plug，Wry 随后释放其已销毁的 WebKit widget 引用。
            unsafe {
                self.plug.destroy();
            }
        }
    }
}

/// 隐藏前只交还本 WebView 持有的焦点；其他窗口或同窗另一个控件的输入不能被抢走。
pub(super) fn return_focus_to_parent(webview: &wry::WebView, parent_id: u64) -> AppResult<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::{
            Input::KeyboardAndMouse::GetFocus, WindowsAndMessaging::IsChild,
        };
        use wry::WebViewExtWindows as _;
        let mut child = Default::default();
        // SAFETY: controller 与焦点 HWND 均在拥有该 WebView 的 UI 线程读取。
        unsafe {
            webview
                .controller()
                .ParentWindow(&mut child)
                .map_err(|_| AppError::infrastructure("WebView 焦点窗口不可用"))?;
            let focus = GetFocus();
            if !focus.is_null() && (focus == child.0 || IsChild(child.0, focus) != 0) {
                webview
                    .focus_parent()
                    .map_err(|_| AppError::infrastructure("WebView 无法将输入焦点交还宿主"))?;
            }
        }
        let _ = parent_id;
    }
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::WidgetExt as _;
        use wry::WebViewExtUnix as _;
        if webview.webview().has_focus() {
            let xlib = x11_dl::xlib::Xlib::open()
                .map_err(|_| AppError::infrastructure("X11 焦点接口不可用"))?;
            // SAFETY: 同一 DISPLAY 只连接当前窗口所属 X server；parent_id 来自仍存活的 GPUI 窗口借用。
            unsafe {
                let display = (xlib.XOpenDisplay)(std::ptr::null());
                if display.is_null() {
                    return Err(AppError::infrastructure("X11 焦点连接不可用"));
                }
                (xlib.XSetInputFocus)(
                    display,
                    parent_id as _,
                    x11_dl::xlib::RevertToParent,
                    x11_dl::xlib::CurrentTime,
                );
                (xlib.XSync)(display, 0);
                (xlib.XCloseDisplay)(display);
            }
        }
    }
    #[cfg(target_os = "macos")]
    macos::return_focus_to_parent(webview)?;
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    let _ = webview;
    #[cfg(not(any(windows, target_os = "linux")))]
    let _ = parent_id;
    Ok(())
}

/// 空裁剪区域不能撤销已有鼠标捕获；只禁用本 WebView 容器，让引擎继续截图而不接收宿主浮层输入。
pub(super) fn set_interactive(webview: &wry::WebView, interactive: bool) -> AppResult<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::{
            Input::KeyboardAndMouse::{EnableWindow, GetCapture, IsWindowEnabled, ReleaseCapture},
            WindowsAndMessaging::IsChild,
        };
        use wry::WebViewExtWindows as _;
        let mut container = Default::default();
        // SAFETY: controller 与输入状态只在所属 UI 线程访问，HWND 来自仍存活的专用 WebView 容器。
        unsafe {
            webview
                .controller()
                .ParentWindow(&mut container)
                .map_err(|_| AppError::infrastructure("WebView 输入窗口不可用"))?;
            if container.0.is_null() {
                return Err(AppError::infrastructure("WebView 输入窗口不可用"));
            }
            let capture_released = if !interactive {
                let capture = GetCapture();
                if !capture.is_null()
                    && (capture == container.0 || IsChild(container.0, capture) != 0)
                {
                    ReleaseCapture() != 0
                } else {
                    true
                }
            } else {
                true
            };
            // EnableWindow 返回旧状态而非成功标志；即使释放捕获失败也先撤销输入，再由宿主真正隐藏降级。
            EnableWindow(container.0, i32::from(interactive));
            if (IsWindowEnabled(container.0) != 0) != interactive {
                return Err(AppError::infrastructure("WebView 无法更新原生输入状态"));
            }
            if !capture_released {
                return Err(AppError::infrastructure("WebView 无法释放原生鼠标捕获"));
            }
        }
    }
    #[cfg(not(windows))]
    {
        // 其他平台仍由整个原生视图的真实隐藏撤销输入，不套用未验证的 Windows 裁剪路径。
        let _ = (webview, interactive);
    }
    Ok(())
}

/// 原生验收只暴露所属父窗标识，不允许页面自行选择或操纵其他窗口。
pub(super) fn parent_id(window: &Window) -> AppResult<u64> {
    match HasWindowHandle::window_handle(window)
        .map_err(|_| AppError::unsupported("当前窗口无法提供原生句柄"))?
        .as_raw()
    {
        RawWindowHandle::Win32(handle) => Ok(handle.hwnd.get() as u64),
        RawWindowHandle::Xcb(handle) => Ok(handle.window.get().into()),
        // 用 From 而不是 as 转换：c_ulong 在 64 位 Unix 上本身就是 u64，as 会被 clippy 判为多余转换，而 32 位仍需提升位宽，From 同时覆盖两种位宽。
        RawWindowHandle::Xlib(handle) => Ok(u64::from(handle.window)),
        RawWindowHandle::AppKit(handle) => Ok(handle.ns_view.as_ptr() as usize as u64),
        _ => Err(AppError::unsupported("当前窗口系统不支持内嵌 WebView")),
    }
}

/// 原生 region 只裁剪像素和输入；完整网页视口不因 GPUI 的滚动/遮挡区域而重新排版。
pub(super) fn set_clip(
    webview: &NativeWebView,
    clip: Option<PluginWebViewBounds>,
) -> AppResult<()> {
    let clip = clip.unwrap_or(PluginWebViewBounds {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    });
    #[cfg(windows)]
    {
        use windows_sys::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, SetWindowRgn};
        use wry::WebViewExtWindows as _;
        let mut parent = Default::default();
        // SAFETY: controller 是本 UI 线程拥有的存活 WebView；成功返回的 HWND 属于其专用子窗。
        unsafe { webview.controller().ParentWindow(&mut parent) }
            .map_err(|_| AppError::infrastructure("WebView 子窗口句柄不可用"))?;
        // SAFETY: region 的坐标已来自有界布局；成功后系统取得 region，失败时由本函数释放。
        unsafe {
            let region = CreateRectRgn(
                clip.x,
                clip.y,
                clip.x.saturating_add(clip.width as i32),
                clip.y.saturating_add(clip.height as i32),
            );
            if region.is_null() {
                return Err(AppError::infrastructure("WebView 裁剪区域创建失败"));
            }
            if SetWindowRgn(parent.0, region, 1) == 0 {
                DeleteObject(region);
                return Err(AppError::infrastructure("WebView 裁剪区域应用失败"));
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::*;
        use wry::WebViewExtUnix as _;
        let view = webview.webview();
        let top = view
            .toplevel()
            .and_then(|widget| widget.downcast::<gtk::Window>().ok())
            .and_then(|window| window.window())
            .ok_or_else(|| AppError::infrastructure("WebView X11 子窗口尚未建立"))?;
        let scale = top.scale_factor().max(1);
        let region = gtk::cairo::Region::create_rectangle(&gtk::cairo::RectangleInt::new(
            clip.x / scale,
            clip.y / scale,
            (clip.width as i32) / scale,
            (clip.height as i32) / scale,
        ));
        top.shape_combine_region(Some(&region), 0, 0);
        top.input_shape_combine_region(&region, 0, 0);
    }
    #[cfg(target_os = "macos")]
    webview.mac.set_clip(&webview.webview, clip)?;
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    let _ = (webview, clip);
    Ok(())
}

/// 文档来源不改变平台门禁；先拒绝不支持的窗口系统，避免 Wry 对未知句柄或 GTK panic。
pub(super) fn create(
    document: PluginWebViewSource,
    sender: SyncSender<PluginWebViewEvent>,
    window: &Window,
) -> AppResult<NativeWebView> {
    let handle = HasWindowHandle::window_handle(window)
        .map_err(|_| AppError::unsupported("当前窗口无法提供原生句柄"))?;
    let raw = handle.as_raw();
    #[cfg(target_os = "windows")]
    if !matches!(raw, RawWindowHandle::Win32(_)) {
        return Err(AppError::unsupported("Windows WebView 需要 Win32 父窗口"));
    }
    #[cfg(target_os = "linux")]
    {
        if !matches!(raw, RawWindowHandle::Xcb(_) | RawWindowHandle::Xlib(_)) {
            return Err(AppError::unsupported(
                "当前内嵌 WebView 只支持 Linux X11，暂不支持 Wayland",
            ));
        }
        gtk::init().map_err(|_| AppError::infrastructure("系统 GTK 引擎初始化失败"))?;
        use gtk::prelude::ObjectExt as _;
        if !gtk::gdk::Display::default()
            .is_some_and(|display| display.type_().name() == "GdkX11Display")
        {
            return Err(AppError::unsupported(
                "GTK 显示必须与 GPUI 的 X11 父窗口一致",
            ));
        }
    }
    #[cfg(target_os = "macos")]
    if !matches!(raw, RawWindowHandle::AppKit(_)) {
        return Err(AppError::unsupported("macOS WebView 需要 AppKit 父视图"));
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        let _ = (raw, document, sender);
        return Err(AppError::unsupported("当前平台尚未通过内嵌 WebView 验收"));
    }
    #[cfg(target_os = "windows")]
    {
        let webview = super::document::builder(document, sender)
            .with_bounds(wry::Rect {
                position: wry::dpi::PhysicalPosition::new(0, 0).into(),
                size: wry::dpi::PhysicalSize::new(1, 1).into(),
            })
            .build_as_child(window)
            .map_err(|error| {
                tracing::warn!(error = %error, "embedded plugin webview creation failed");
                AppError::infrastructure("无法创建系统内嵌 WebView，请检查系统引擎是否已安装")
            })?;
        let parent_position = ParentPosition {
            origin: parent_client_origin(&webview)?,
            notifications: 0,
        };
        Ok(NativeWebView {
            webview,
            parent_position,
        })
    }
    #[cfg(target_os = "macos")]
    {
        let webview = super::document::builder(document, sender)
            .with_bounds(wry::Rect {
                position: wry::dpi::PhysicalPosition::new(0, 0).into(),
                size: wry::dpi::PhysicalSize::new(1, 1).into(),
            })
            .build_as_child(window)
            .map_err(|_| AppError::infrastructure("无法创建系统 WKWebView"))?;
        let mac = macos::Surface::new(&webview)?;
        Ok(NativeWebView { webview, mac })
    }
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::*;
        use wry::WebViewBuilderExtUnix as _;
        let parent = parent_id(window)?;
        let plug = gtk::Plug::new(parent as _);
        plug.set_decorated(false);
        plug.set_accept_focus(true);
        let fixed = gtk::Fixed::new();
        plug.add(&fixed);
        plug.realize();
        // GtkPlug 必须已经是真实 X11 子窗；异常时不能退回单独顶层窗口冒充内嵌。
        if !plug.is_embedded() {
            unsafe {
                plug.destroy();
            }
            return Err(AppError::unsupported(
                "GTK 未能将 WebView 容器嵌入 GPUI 父窗口",
            ));
        }
        let result = super::document::builder(document, sender)
            .with_bounds(wry::Rect {
                position: wry::dpi::LogicalPosition::new(0, 0).into(),
                size: wry::dpi::LogicalSize::new(1, 1).into(),
            })
            .build_gtk(&fixed);
        match result {
            Ok(webview) => Ok(NativeWebView { webview, plug }),
            Err(error) => {
                unsafe {
                    plug.destroy();
                }
                tracing::warn!(error = %error, "embedded GTK WebView creation failed");
                Err(AppError::infrastructure("无法创建 GTK 内嵌 WebView"))
            }
        }
    }
}

/// 有界推进 GTK 原生事件以处理渲染和桥接；不能启动第二条主循环或阻塞 GPUI。
pub(super) fn pump_events() {
    #[cfg(target_os = "linux")]
    for _ in 0..64 {
        if !gtk::events_pending() {
            break;
        }
        gtk::main_iteration_do(false);
    }
}

#[cfg(all(test, windows))]
#[path = "../../../tests/unit/plugin_webview_parent_position.rs"]
mod parent_position_tests;
