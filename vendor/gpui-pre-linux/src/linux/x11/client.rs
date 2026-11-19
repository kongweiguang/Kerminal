use anyhow::{Context as _, anyhow};
use ashpd::WindowIdentifier;
use calloop::{
    LoopHandle, RegistrationToken,
    generic::{FdWrapper, Generic},
};
use collections::HashMap;
use core::str;
use gpui::{Capslock, GraphicalEnvironment};
use gpui_util::ResultExt as _;
use http_client::Url;
use log::Level;
use smallvec::SmallVec;
use std::{
    cell::{RefCell, RefMut},
    collections::{BTreeMap, HashSet, VecDeque},
    ops::Deref,
    path::PathBuf,
    rc::{Rc, Weak},
    time::{Duration, Instant},
};

use x11rb::{
    connection::{Connection, RequestConnection},
    cursor,
    errors::ConnectionError,
    protocol::randr::ConnectionExt as _,
    protocol::xinput::ConnectionExt,
    protocol::xkb::ConnectionExt as _,
    protocol::xproto::{
        AtomEnum, ChangeWindowAttributesAux, ClientMessageData, ClientMessageEvent,
        ConnectionExt as _, EventMask, Visibility,
    },
    protocol::{Event, dri3, randr, render, xinput, xkb, xproto},
    resource_manager::Database,
    wrapper::ConnectionExt as _,
    xcb_ffi::XCBConnection,
};
use xim::{AttributeName, Client, InputStyle, x11rb::X11rbClient};
use xkbc::x11::ffi::{XKB_X11_MIN_MAJOR_XKB_VERSION, XKB_X11_MIN_MINOR_XKB_VERSION};
use xkbcommon::xkb::{self as xkbc, STATE_LAYOUT_EFFECTIVE};

use super::{
    ButtonOrScroll, ScrollDirection, X11Display, X11WindowStatePtr, XcbAtoms, XimCallbackEvent,
    XimHandler, button_or_scroll_from_event_detail, check_reply,
    clipboard::{self, Clipboard},
    get_reply, get_valuator_axis_index, handle_connection_error, modifiers_from_state,
    pressed_button_from_mask, xcb_flush,
};

use crate::linux::{
    DEFAULT_CURSOR_ICON_NAME, capslock_from_xkb, cursor_style_to_icon_names, get_xkb_compose_state,
    is_within_click_distance, keystroke_from_xkb, keystroke_underlying_dead_key,
    log_cursor_icon_warning, modifiers_from_xkb, new_xkb_context, open_uri_internal,
    platform::{DOUBLE_CLICK_INTERVAL, SCROLL_LINES},
    reveal_path_internal,
    xdg_desktop_portal::{Event as XDPEvent, XDPEventSource},
};
use crate::linux::{LinuxCommon, LinuxKeyboardLayout, X11Window, modifiers_from_xinput_info};

use gpui::{
    AnyWindowHandle, Bounds, ClipboardItem, CursorStyle, FileDropEvent, FrameRequestSource,
    Keystroke, Modifiers, ModifiersChangedEvent, MouseButton, Pixels, PlatformDisplay,
    PlatformFrameSignal, PlatformInput, PlatformKeyboardLayout, PlatformWindow, Point,
    RequestFrameOptions, ScrollDelta, Size, TouchPhase, WindowButtonLayout, WindowParams,
    WindowVisibility, point, px,
};
use gpui_wgpu::{CompositorGpuHint, GpuContext};

/// Value for DeviceId parameters which selects all devices.
pub(crate) const XINPUT_ALL_DEVICES: xinput::DeviceId = 0;

/// Value for DeviceId parameters which selects all device groups. Events that
/// occur within the group are emitted by the group itself.
///
/// In XInput 2's interface, these are referred to as "master devices", but that
/// terminology is both archaic and unclear.
pub(crate) const XINPUT_ALL_DEVICE_GROUPS: xinput::DeviceId = 1;

const GPUI_X11_SCALE_FACTOR_ENV: &str = "GPUI_X11_SCALE_FACTOR";

pub(crate) struct WindowRef {
    window: X11WindowStatePtr,
    refresh_state: Option<RefreshState>,
    last_visibility: Visibility,
    is_mapped: bool,
}

impl WindowRef {
    pub fn handle(&self) -> AnyWindowHandle {
        self.window.state.borrow().handle
    }

    /// Whether the X server is presenting this window. Compositing window
    /// managers rarely report `FULLY_OBSCURED`, so under them this is the
    /// mapped state alone.
    fn visibility(&self) -> WindowVisibility {
        if self.is_mapped && !matches!(self.last_visibility, Visibility::FULLY_OBSCURED) {
            WindowVisibility::Visible
        } else {
            WindowVisibility::Hidden
        }
    }
}

impl Deref for WindowRef {
    type Target = X11WindowStatePtr;

    fn deref(&self) -> &Self::Target {
        &self.window
    }
}

enum RefreshState {
    Hidden {
        refresh_rate: Duration,
    },
    PeriodicRefresh {
        refresh_rate: Duration,
        event_loop_token: RegistrationToken,
    },
}

#[derive(Debug)]
#[non_exhaustive]
pub enum EventHandlerError {
    XCBConnectionError(ConnectionError),
    XIMClientError(xim::ClientError),
}

impl std::error::Error for EventHandlerError {}

impl std::fmt::Display for EventHandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EventHandlerError::XCBConnectionError(err) => err.fmt(f),
            EventHandlerError::XIMClientError(err) => err.fmt(f),
        }
    }
}

impl From<ConnectionError> for EventHandlerError {
    fn from(err: ConnectionError) -> Self {
        EventHandlerError::XCBConnectionError(err)
    }
}

impl From<xim::ClientError> for EventHandlerError {
    fn from(err: xim::ClientError) -> Self {
        EventHandlerError::XIMClientError(err)
    }
}

#[derive(Debug, Default)]
pub struct Xdnd {
    other_window: xproto::Window,
    drag_type: u32,
    retrieved: bool,
    position: Point<Pixels>,
}

#[derive(Debug)]
struct PointerDeviceState {
    horizontal: ScrollAxisState,
    vertical: ScrollAxisState,
}

#[derive(Debug, Default)]
struct ScrollAxisState {
    /// Valuator number for looking up this axis's scroll value.
    valuator_number: Option<u16>,
    /// Conversion factor from scroll units to lines.
    multiplier: f32,
    /// Last scroll value for calculating scroll delta.
    ///
    /// This gets set to `None` whenever it might be invalid - when devices change or when window focus changes.
    /// The logic errs on the side of invalidating this, since the consequence is just skipping the delta of one scroll event.
    /// The consequence of not invalidating it can be large invalid deltas, which are much more user visible.
    scroll_value: Option<f32>,
}

#[derive(Clone, Copy, Debug)]
struct SmoothScrollMotion {
    sourceid: xinput::DeviceId,
    time: xproto::Timestamp,
    delta: Point<f32>,
}

/// 判定一次 KeyRelease/KeyPress 是否是旧式 X11 自动重复的合成对。
///
/// Xorg/Xvfb 为每次重复生成的释放与按下共享键码、目标窗口和服务器时间戳；
/// 三者同时相等才成立，避免把不同窗口或不同时刻的真实点按误当成重复。该判定
/// 只作用于紧邻且已排队的一对事件，不跨事件或跨轮询批次推断。
fn is_synthetic_key_release_pair(
    release: &xproto::KeyReleaseEvent,
    press: &xproto::KeyPressEvent,
) -> bool {
    release.detail == press.detail && release.event == press.event && release.time == press.time
}

/// 一次轮询到的原始事件及其按键重复标记。
///
/// 重复判定必须发生在 XIM 过滤之前：XIM 可能吞掉 KeyPress，但物理按键的按下/
/// 抬起生命周期仍要在任何窗口和输入法状态下保持一致。
struct PendingX11Event {
    event: Event,
    key_press_is_repeat: bool,
}

impl From<Event> for PendingX11Event {
    fn from(event: Event) -> Self {
        Self {
            event,
            key_press_is_repeat: false,
        }
    }
}

/// 跟踪物理按键的按下/抬起生命周期，仅在 XKB 可检测自动重复模式下使用。
///
/// 该模式下服务器只发送连续的 KeyPress，同一键码仍在按下集合中即为重复。旧式
/// 服务器没有可靠状态可跨事件保留：合成释放一旦被丢弃，配对是否成立只能由轮询层
/// 的紧邻事件判定，因此这里不保存释放时间，避免已经下发 KeyUp 后把下一次
/// KeyPress 误标为 held。焦点丢失或窗口销毁可能永远收不到 KeyRelease，必须清空。
struct KeyRepeatTracker {
    /// 已收到 KeyPress、尚未收到 KeyRelease 的键码。
    pressed: HashSet<u8>,
}

impl KeyRepeatTracker {
    fn new() -> Self {
        Self {
            pressed: HashSet::new(),
        }
    }

    /// 记录物理抬键，使同一键码之后的 KeyPress 重新成为首次按下。
    fn release(&mut self, detail: u8) {
        self.pressed.remove(&detail);
    }

    /// 返回该 KeyPress 是否为仍处于按下状态的同一键码的自动重复。
    fn press(&mut self, detail: u8) -> bool {
        let is_repeat = self.pressed.contains(&detail);
        self.pressed.insert(detail);
        is_repeat
    }

    /// 焦点切换或窗口销毁后无法保证收到抬键事件，清空状态以免残留键码伪装成重复。
    fn reset(&mut self) {
        self.pressed.clear();
    }
}

/// 在途 KeyPress 的身份：XIM 协议原样保留目标窗口、键码和服务器时间戳。
///
/// 转发是异步的，多个 KeyPress 可能在首个回调前排队；以原始事件身份匹配，
/// 才能让乱序返回的回调各自认领自己的重复标记。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ForwardedKeyPressId {
    window: xproto::Window,
    detail: u8,
    time: xproto::Timestamp,
}

impl From<&xproto::KeyPressEvent> for ForwardedKeyPressId {
    fn from(event: &xproto::KeyPressEvent) -> Self {
        Self {
            window: event.event,
            detail: event.detail,
            time: event.time,
        }
    }
}

struct ForwardedKeyPress {
    id: ForwardedKeyPressId,
    is_repeat: bool,
    forwarded_at: Instant,
}

/// XIM 转发通常很快返回，但输入法吞掉事件时不会回调；超时后标记作废，
/// 避免陈旧标记套到之后恰好同键码、同时间戳的新按键上。
const FORWARDED_KEY_PRESS_TTL: Duration = Duration::from_secs(5);
/// 连续按键可在首个回调前排队；上限覆盖正常输入法往返，溢出按 FIFO 淘汰最旧项。
const FORWARDED_KEY_PRESS_LIMIT: usize = 32;

/// 保存尚未回调的 XIM 转发按键与其重复标记。
///
/// 队列按原始事件身份匹配而非假定 FIFO：XIM 服务器可能乱序返回，两个在途的
/// 重复按键也必须各自匹配。淘汰只依赖本地单调时间，与服务器时间戳无关。
#[derive(Default)]
struct ForwardedKeyPressQueue {
    entries: VecDeque<ForwardedKeyPress>,
}

impl ForwardedKeyPressQueue {
    fn new() -> Self {
        Self::default()
    }

    /// 记录一次转发；先淘汰过期项，再限制队列长度后追加。
    fn push(&mut self, event: &xproto::KeyPressEvent, is_repeat: bool, now: Instant) {
        self.evict_expired(now);
        self.entries.push_back(ForwardedKeyPress {
            id: event.into(),
            is_repeat,
            forwarded_at: now,
        });
        while self.entries.len() > FORWARDED_KEY_PRESS_LIMIT {
            self.entries.pop_front();
        }
    }

    /// 按身份取回并移除标记；找不到或已过期时返回 `None`，调用方按首次按下处理。
    fn take(&mut self, event: &xproto::KeyPressEvent, now: Instant) -> Option<bool> {
        self.evict_expired(now);
        let id = ForwardedKeyPressId::from(event);
        let index = self.entries.iter().position(|entry| entry.id == id)?;
        self.entries.remove(index).map(|entry| entry.is_repeat)
    }

    /// XIM 连接失效或焦点转移后，在途回调不会再按原语义到达，整体丢弃。
    fn clear(&mut self) {
        self.entries.clear();
    }

    /// 队首是最早转发的项；本地时间单调递增，因此从队首淘汰即覆盖全部过期项。
    fn evict_expired(&mut self, now: Instant) {
        while self
            .entries
            .front()
            .is_some_and(|entry| now.duration_since(entry.forwarded_at) > FORWARDED_KEY_PRESS_TTL)
        {
            self.entries.pop_front();
        }
    }
}

pub struct X11ClientState {
    /// A weak pointer to this state, for event-loop callbacks registered by its methods.
    this: X11ClientStatePtr,
    pub(crate) loop_handle: LoopHandle<'static, ()>,
    /// Long-lived sources this client registered on the loop, removed when its
    /// [`X11Connection`] drops.
    registrations: Vec<RegistrationToken>,
    /// The environment this connection was made in, without its activation token.
    graphical_environment: GraphicalEnvironment,

    pub(crate) last_click: Instant,
    pub(crate) last_mouse_button: Option<MouseButton>,
    pub(crate) last_location: Point<Pixels>,
    pub(crate) current_count: usize,
    pub(crate) pinch_scale: f32,

    pub(crate) gpu_context: GpuContext,
    pub(crate) compositor_gpu: Option<CompositorGpuHint>,

    pub(crate) scale_factor: f32,

    xkb_context: xkbc::Context,
    pub(crate) xcb_connection: Rc<XCBConnection>,
    xkb_device_id: i32,
    client_side_decorations_supported: bool,
    pub(crate) x_root_index: usize,
    pub(crate) resource_database: Database,
    pub(crate) atoms: XcbAtoms,
    pub(crate) windows: HashMap<xproto::Window, WindowRef>,
    pub(crate) mouse_focused_window: Option<xproto::Window>,
    pub(crate) keyboard_focused_window: Option<xproto::Window>,
    pub(crate) xkb: xkbc::State,
    keyboard_layout: LinuxKeyboardLayout,
    /// 服务器是否接受本连接的 XKB 可检测自动重复，决定重复按键的识别方式。
    detectable_auto_repeat: bool,
    /// 物理按键生命周期，用于把自动重复的 KeyPress 标记为 `is_held`。
    key_repeat_tracker: KeyRepeatTracker,
    /// 尚未回调的在途 XIM 转发按键及其重复标记。
    ///
    /// 转发是异步的，可能同时有多条在途事件；按原始事件身份保存，回调按身份
    /// 认领，避免后一个 KeyPress 覆盖前一个的 held 标记。
    forwarded_key_presses: ForwardedKeyPressQueue,
    pub(crate) ximc: Option<X11rbClient<Rc<XCBConnection>>>,
    pub(crate) xim_handler: Option<XimHandler>,
    pub modifiers: Modifiers,
    pub capslock: Capslock,
    // TODO: Can the other updates to `modifiers` be removed so that this is unnecessary?
    // capslock logic was done analog to modifiers
    pub last_modifiers_changed_event: Modifiers,
    pub last_capslock_changed_event: Capslock,

    pub(crate) compose_state: Option<xkbc::compose::State>,
    pub(crate) pre_edit_text: Option<String>,
    pub(crate) composing: bool,
    pub(crate) pre_key_char_down: Option<Keystroke>,
    pub(crate) cursor_handle: cursor::Handle,
    pub(crate) cursor_styles: HashMap<xproto::Window, CursorStyle>,
    pub(crate) cursor_cache: HashMap<CursorStyle, Option<xproto::Cursor>>,
    pub(crate) invisible_cursor_cache: Option<xproto::Cursor>,
    pub(crate) cursor_hidden_window: Option<xproto::Window>,

    pointer_device_states: BTreeMap<xinput::DeviceId, PointerDeviceState>,
    recent_smooth_scroll_motions: VecDeque<SmoothScrollMotion>,

    pub(crate) supports_xinput_gestures: bool,

    pub(crate) common: Rc<RefCell<LinuxCommon>>,
    pub(crate) clipboard: Clipboard,
    pub(crate) clipboard_item: Option<ClipboardItem>,
    pub(crate) xdnd_state: Xdnd,
}

#[derive(Clone)]
pub struct X11ClientStatePtr(pub Weak<RefCell<X11ClientState>>);

impl X11ClientStatePtr {
    pub fn get_client(&self) -> Option<X11Client> {
        self.0.upgrade().map(X11Client)
    }

    pub fn drop_window(&self, x_window: u32) {
        let Some(client) = self.get_client() else {
            return;
        };
        let mut state = client.0.borrow_mut();

        if let Some(window_ref) = state.windows.remove(&x_window)
            && let Some(RefreshState::PeriodicRefresh {
                event_loop_token, ..
            }) = window_ref.refresh_state
        {
            state.loop_handle.remove(event_loop_token);
        }
        if state.mouse_focused_window == Some(x_window) {
            state.mouse_focused_window = None;
        }
        if state.keyboard_focused_window == Some(x_window) {
            state.keyboard_focused_window = None;
            // 窗口销毁后收不到仍按住按键的 KeyRelease，清空以免残留键码被判为重复。
            state.key_repeat_tracker.reset();
            state.forwarded_key_presses.clear();
        }
        if state.cursor_hidden_window == Some(x_window) {
            state.cursor_hidden_window = None;
        }
        state.cursor_styles.remove(&x_window);
    }

    pub fn update_ime_position(&self, bounds: Bounds<Pixels>) {
        let Some(client) = self.get_client() else {
            return;
        };
        let mut state = client.0.borrow_mut();
        if state.composing || state.ximc.is_none() {
            return;
        }

        let Some(mut ximc) = state.ximc.take() else {
            log::error!("bug: xim connection not set");
            return;
        };
        let Some(xim_handler) = state.xim_handler.take() else {
            log::error!("bug: xim handler not set");
            state.ximc = Some(ximc);
            return;
        };
        let scaled_bounds = bounds.scale(state.scale_factor);
        let ic_attributes = ximc
            .build_ic_attributes()
            .push(
                xim::AttributeName::InputStyle,
                xim::InputStyle::PREEDIT_CALLBACKS,
            )
            .push(xim::AttributeName::ClientWindow, xim_handler.window)
            .push(xim::AttributeName::FocusWindow, xim_handler.window)
            .nested_list(xim::AttributeName::PreeditAttributes, |b| {
                b.push(
                    xim::AttributeName::SpotLocation,
                    xim::Point {
                        x: u32::from(scaled_bounds.origin.x + scaled_bounds.size.width) as i16,
                        y: u32::from(scaled_bounds.origin.y + scaled_bounds.size.height) as i16,
                    },
                );
            })
            .build();
        let _ = ximc
            .set_ic_values(xim_handler.im_id, xim_handler.ic_id, ic_attributes)
            .log_err();
        state.ximc = Some(ximc);
        state.xim_handler = Some(xim_handler);
    }
}

/// A connection to an X server.
///
/// Not `Clone`: dropping it removes the connection's event-loop sources and forgets its windows.
/// The X connection itself closes once the last [`X11Client`] handle is gone.
pub(crate) struct X11Connection(X11Client);

impl std::ops::Deref for X11Connection {
    type Target = X11Client;

    fn deref(&self) -> &X11Client {
        &self.0
    }
}

impl Drop for X11Connection {
    fn drop(&mut self) {
        let mut state = self.0.0.borrow_mut();
        state.common.borrow_mut().after_runnable = None;
        for token in std::mem::take(&mut state.registrations) {
            state.loop_handle.remove(token);
        }
        let refresh_tokens = state
            .windows
            .values_mut()
            .filter_map(|window| match window.refresh_state.take() {
                Some(RefreshState::PeriodicRefresh {
                    event_loop_token, ..
                }) => Some(event_loop_token),
                _ => None,
            })
            .collect::<Vec<_>>();
        for token in refresh_tokens {
            state.loop_handle.remove(token);
        }
        let windows = std::mem::take(&mut state.windows);
        drop(state);
        drop(windows);
    }
}

impl X11Connection {
    /// Connects to the X server `environment` names and registers its event sources on
    /// `handle`.
    ///
    /// Dropping the connection removes those sources, leaving the loop and `common` usable
    /// without X11.
    pub(crate) fn attach(
        handle: LoopHandle<'static, ()>,
        common: Rc<RefCell<LinuxCommon>>,
        environment: &GraphicalEnvironment,
    ) -> anyhow::Result<Self> {
        let display = environment
            .x11_display
            .as_ref()
            .filter(|display| !display.is_empty())
            .context("DISPLAY is not set")?
            .to_str()
            .context("DISPLAY is not valid UTF-8")?;
        let display_name =
            std::ffi::CString::new(display).context("X display name contains a NUL byte")?;
        let (xcb_connection, x_root_index) = XCBConnection::connect(Some(&display_name))
            .with_context(|| format!("failed to connect to X server {display:?}"))?;
        xcb_connection.prefetch_extension_information(xkb::X11_EXTENSION_NAME)?;
        xcb_connection.prefetch_extension_information(randr::X11_EXTENSION_NAME)?;
        xcb_connection.prefetch_extension_information(render::X11_EXTENSION_NAME)?;
        xcb_connection.prefetch_extension_information(xinput::X11_EXTENSION_NAME)?;

        // Announce to X server that XInput up to 2.4 is supported.
        // Version 2.4 is needed for gesture events (GesturePinchBegin/Update/End).
        // The server responds with the highest version it supports; if < 2.4,
        // we must not request gesture event masks in XISelectEvents.
        let xinput_version = get_reply(
            || "XInput XiQueryVersion failed",
            xcb_connection.xinput_xi_query_version(2, 4),
        )?;
        anyhow::ensure!(
            xinput_version.major_version >= 2,
            "XInput version >= 2 required."
        );
        let supports_xinput_gestures = xinput_version.major_version > 2
            || (xinput_version.major_version == 2 && xinput_version.minor_version >= 4);
        log::info!(
            "XInput version: {}.{}, gesture support: {}",
            xinput_version.major_version,
            xinput_version.minor_version,
            supports_xinput_gestures,
        );

        let pointer_device_states =
            current_pointer_device_states(&xcb_connection, &BTreeMap::new()).unwrap_or_default();

        let atoms = XcbAtoms::new(&xcb_connection)
            .context("Failed to get XCB atoms")?
            .reply()
            .context("Failed to get XCB atoms")?;

        let root = xcb_connection.setup().roots[0].root;
        let compositor_present = check_compositor_present(&xcb_connection, root);
        let gtk_frame_extents_supported =
            check_gtk_frame_extents_supported(&xcb_connection, &atoms, root);
        let client_side_decorations_supported = compositor_present && gtk_frame_extents_supported;
        log::info!(
            "x11: compositor present: {}, gtk_frame_extents_supported: {}",
            compositor_present,
            gtk_frame_extents_supported
        );

        let xkb = get_reply(
            || "Failed to initialize XKB extension",
            xcb_connection
                .xkb_use_extension(XKB_X11_MIN_MAJOR_XKB_VERSION, XKB_X11_MIN_MINOR_XKB_VERSION),
        )?;
        anyhow::ensure!(xkb.supported, "X server does not support XKB");

        // 启用本连接的 XKB 可检测自动重复后，服务器不再为每次重复伪造 KeyRelease，
        // 客户端可凭实际按下集合把重复 KeyPress 标记为 is_held。旧服务器或拒绝该
        // 标志时保留合成释放对兜底，避免把重复按键误当成新的快捷键触发。
        let detectable_auto_repeat = get_reply(
            || "Failed to enable XKB detectable auto-repeat",
            xcb_connection.xkb_per_client_flags(
                xkb::ID::USE_CORE_KBD.into(),
                xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT,
                xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT,
                xkb::BoolCtrl::default(),
                xkb::BoolCtrl::default(),
                xkb::BoolCtrl::default(),
            ),
        )
        .map(|reply| {
            reply
                .supported
                .contains(xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT)
                && reply
                    .value
                    .contains(xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT)
        })
        .log_err()
        .unwrap_or(false);
        log::info!("X11 detectable auto-repeat: {detectable_auto_repeat}");

        let events = xkb::EventType::STATE_NOTIFY
            | xkb::EventType::MAP_NOTIFY
            | xkb::EventType::NEW_KEYBOARD_NOTIFY;
        let map_notify_parts = xkb::MapPart::KEY_TYPES
            | xkb::MapPart::KEY_SYMS
            | xkb::MapPart::MODIFIER_MAP
            | xkb::MapPart::EXPLICIT_COMPONENTS
            | xkb::MapPart::KEY_ACTIONS
            | xkb::MapPart::KEY_BEHAVIORS
            | xkb::MapPart::VIRTUAL_MODS
            | xkb::MapPart::VIRTUAL_MOD_MAP;
        check_reply(
            || "Failed to select XKB events",
            xcb_connection.xkb_select_events(
                xkb::ID::USE_CORE_KBD.into(),
                0u8.into(),
                events,
                map_notify_parts,
                map_notify_parts,
                &xkb::SelectEventsAux::new(),
            ),
        )?;

        let xkb_context = new_xkb_context()?;
        let xkb_device_id = xkbc::x11::get_core_keyboard_device_id(&xcb_connection);
        let xkb_state = {
            let xkb_keymap = xkbc::x11::keymap_new_from_device(
                &xkb_context,
                &xcb_connection,
                xkb_device_id,
                xkbc::KEYMAP_COMPILE_NO_FLAGS,
            );
            xkbc::x11::state_new_from_device(&xkb_keymap, &xcb_connection, xkb_device_id)
        };
        let compose_state = get_xkb_compose_state(&xkb_context);
        let layout_idx = xkb_state.serialize_layout(STATE_LAYOUT_EFFECTIVE);
        let layout_name = xkb_state
            .get_keymap()
            .layout_get_name(layout_idx)
            .to_string();
        let keyboard_layout = LinuxKeyboardLayout::new(layout_name.into());

        let resource_database = x11rb::resource_manager::new_from_default(&xcb_connection)
            .context("Failed to create resource database")?;
        let scale_factor = get_scale_factor(&xcb_connection, &resource_database, x_root_index);
        let cursor_handle = cursor::Handle::new(&xcb_connection, x_root_index, &resource_database)
            .context("Failed to initialize cursor theme handler")?
            .reply()
            .context("Failed to initialize cursor theme handler")?;

        let clipboard = Clipboard::new(display).context("Failed to initialize clipboard")?;

        let screen = &xcb_connection.setup().roots[x_root_index];
        let compositor_gpu = detect_compositor_gpu(&xcb_connection, screen);

        let xcb_connection = Rc::new(xcb_connection);

        let ximc = X11rbClient::init(Rc::clone(&xcb_connection), x_root_index, None).ok();
        let xim_handler = if ximc.is_some() {
            Some(XimHandler::new())
        } else {
            None
        };

        let background_executor = common.borrow().background_executor.clone();
        let connection = X11Connection(X11Client(Rc::new_cyclic(|this| {
            RefCell::new(X11ClientState {
                this: X11ClientStatePtr(this.clone()),
                registrations: Vec::new(),
                graphical_environment: GraphicalEnvironment {
                    activation_token: None,
                    ..environment.clone()
                },
                modifiers: Modifiers::default(),
                capslock: Capslock::default(),
                last_modifiers_changed_event: Modifiers::default(),
                last_capslock_changed_event: Capslock::default(),
                loop_handle: handle.clone(),
                common,
                last_click: Instant::now(),
                last_mouse_button: None,
                last_location: Point::new(px(0.0), px(0.0)),
                current_count: 0,
                pinch_scale: 1.0,
                gpu_context: Rc::new(RefCell::new(None)),
                compositor_gpu,
                scale_factor,

                xkb_context,
                xcb_connection,
                xkb_device_id,
                client_side_decorations_supported,
                x_root_index,
                resource_database,
                atoms,
                windows: HashMap::default(),
                mouse_focused_window: None,
                keyboard_focused_window: None,
                xkb: xkb_state,
                keyboard_layout,
                detectable_auto_repeat,
                key_repeat_tracker: KeyRepeatTracker::new(),
                forwarded_key_presses: ForwardedKeyPressQueue::new(),
                ximc,
                xim_handler,

                compose_state,
                pre_edit_text: None,
                pre_key_char_down: None,
                composing: false,

                cursor_handle,
                cursor_styles: HashMap::default(),
                cursor_cache: HashMap::default(),
                cursor_hidden_window: None,
                invisible_cursor_cache: None,

                pointer_device_states,
                recent_smooth_scroll_motions: VecDeque::new(),

                supports_xinput_gestures,

                clipboard,
                clipboard_item: None,
                xdnd_state: Xdnd::default(),
            })
        })));
        // From here on, `Drop` removes whatever was registered if a later step fails.
        connection.register_sources(&handle, background_executor)?;
        let xcb_connection = connection.0.0.borrow().xcb_connection.clone();
        xcb_flush(&xcb_connection);
        Ok(connection)
    }
}

#[derive(Clone)]
pub(crate) struct X11Client(pub(crate) Rc<RefCell<X11ClientState>>);

impl X11Client {
    fn register_sources(
        &self,
        handle: &LoopHandle<'static, ()>,
        background_executor: gpui::BackgroundExecutor,
    ) -> anyhow::Result<()> {
        let this = self.0.borrow().this.clone();
        let xcb_connection = self.0.borrow().xcb_connection.clone();

        // Safety: Safe if xcb::Connection always returns a valid fd
        let fd = unsafe { FdWrapper::new(Rc::clone(&xcb_connection)) };
        let token = handle
            .insert_source(
                Generic::new_with_error::<EventHandlerError>(
                    fd,
                    calloop::Interest::READ,
                    calloop::Mode::Level,
                ),
                {
                    let this = this.clone();
                    let xcb_connection = xcb_connection.clone();
                    move |_readiness, _, _| {
                        if let Some(client) = this.get_client() {
                            client.process_x11_events(&xcb_connection)?;
                        }
                        Ok(calloop::PostAction::Continue)
                    }
                },
            )
            .map_err(|err| anyhow!("Failed to initialize X11 event source: {err:?}"))?;
        self.0.borrow_mut().registrations.push(token);

        let token = handle
            .insert_source(XDPEventSource::new(&background_executor), {
                let this = this.clone();
                move |event, _, _| {
                    let Some(client) = this.get_client() else {
                        return;
                    };
                    match event {
                        XDPEvent::WindowAppearance(appearance) => {
                            client.with_common(|common| common.appearance = appearance);
                            for window in client.0.borrow_mut().windows.values_mut() {
                                window.window.set_appearance(appearance);
                            }
                        }
                        XDPEvent::ButtonLayout(layout_str) => {
                            let layout = WindowButtonLayout::parse(&layout_str)
                                .log_err()
                                .unwrap_or_else(WindowButtonLayout::linux_default);
                            client.with_common(|common| common.button_layout = layout);
                            for window in client.0.borrow_mut().windows.values_mut() {
                                window.window.set_button_layout();
                            }
                        }
                        XDPEvent::CursorTheme(_) | XDPEvent::CursorSize(_) => {
                            // noop, X11 manages this for us.
                        }
                    }
                }
            })
            .map_err(|err| anyhow!("Failed to initialize XDP event source: {err:?}"))?;
        self.0.borrow_mut().registrations.push(token);

        // Runnables' requests can read events into xcb's queue, where they don't wake the loop.
        let common = self.0.borrow().common.clone();
        common.borrow_mut().after_runnable = Some(Rc::new(move || {
            if let Some(client) = this.get_client() {
                client.process_x11_events(&xcb_connection).log_err();
            }
        }));
        Ok(())
    }

    pub(crate) fn has_windows(&self) -> bool {
        !self.0.borrow().windows.is_empty()
    }

    pub(crate) fn graphical_environment(&self) -> GraphicalEnvironment {
        self.0.borrow().graphical_environment.clone()
    }

    /// 轮询并分发 X11 事件；按键重复在原始事件层判定后随事件一起下发。
    ///
    /// 重复判定依赖完整且未被 XIM 过滤的 KeyPress/KeyRelease 顺序，先在这里
    /// 标记再分发，避免终端快捷键在长按时被当成新的按下而重复触发。
    ///
    /// @author kongweiguang
    pub fn process_x11_events(
        &self,
        xcb_connection: &XCBConnection,
    ) -> Result<(), EventHandlerError> {
        let detectable_auto_repeat = self.0.borrow().detectable_auto_repeat;
        loop {
            let mut events: Vec<PendingX11Event> = Vec::new();
            let mut windows_to_refresh = HashSet::new();

            let mut last_key_release: Option<Event> = None;

            // event handlers for new keyboard / remapping refresh the state without using event
            // details, this deduplicates them.
            let mut last_keymap_change_event: Option<Event> = None;

            loop {
                match xcb_connection.poll_for_event() {
                    Ok(Some(event)) => {
                        match event {
                            Event::Expose(expose_event) => {
                                // Expose 不可能是合成重复对的一部分，排队的释放是真实抬键。
                                if let Some(last_key_release) = last_key_release.take() {
                                    events.push(last_key_release.into());
                                }
                                windows_to_refresh.insert(expose_event.window);
                            }
                            Event::KeyRelease(key_release) => {
                                if let Some(last_keymap_change_event) =
                                    last_keymap_change_event.take()
                                {
                                    if let Some(last_key_release) = last_key_release.take() {
                                        events.push(last_key_release.into());
                                    }
                                    events.push(last_keymap_change_event.into());
                                } else if let Some(last_key_release) = last_key_release.take() {
                                    // 两个 KeyRelease 之间没有 KeyPress，前一个不可能是自动
                                    // 重复的合成释放，必须补发，否则该键会一直停在按下状态。
                                    events.push(last_key_release.into());
                                }

                                // 物理抬键必须先于任何过滤被记录：可检测模式下它决定
                                // 后续 KeyPress 是否为重复，旧式合成对则由下面的紧邻
                                // 判定处理。
                                self.0
                                    .borrow_mut()
                                    .key_repeat_tracker
                                    .release(key_release.detail);
                                last_key_release = Some(Event::KeyRelease(key_release));
                            }
                            Event::KeyPress(key_press) => {
                                if let Some(last_keymap_change_event) =
                                    last_keymap_change_event.take()
                                {
                                    if let Some(last_key_release) = last_key_release.take() {
                                        events.push(last_key_release.into());
                                    }
                                    events.push(last_keymap_change_event.into());
                                }

                                // 旧式兜底只在释放与按下紧邻且键码、目标窗口、服务器
                                // 时间戳全部相同时认定合成对。释放一旦被其他事件或批次
                                // 边界隔开，就已经按真实 KeyUp 下发，不能再凭历史时间把它
                                // 之后的按下标成 held，否则 UI 的按键所有权会与 KeyUp 冲突。
                                let mut key_press_is_repeat = false;
                                if let Some(Event::KeyRelease(key_release)) =
                                    last_key_release.take()
                                {
                                    if !detectable_auto_repeat
                                        && is_synthetic_key_release_pair(&key_release, &key_press)
                                    {
                                        key_press_is_repeat = true;
                                    } else {
                                        events.push(Event::KeyRelease(key_release).into());
                                    }
                                }

                                if detectable_auto_repeat {
                                    key_press_is_repeat = self
                                        .0
                                        .borrow_mut()
                                        .key_repeat_tracker
                                        .press(key_press.detail);
                                }
                                events.push(PendingX11Event {
                                    event: Event::KeyPress(key_press),
                                    key_press_is_repeat,
                                });
                            }
                            Event::XkbNewKeyboardNotify(_) => {
                                if let Some(release_event) = last_key_release.take() {
                                    events.push(release_event.into());
                                }
                                // 新键盘设备可能收不到旧设备上仍按住按键的 KeyRelease，
                                // 旧设备在途的 XIM 转发也不再对应同一物理按键；在排队
                                // 键盘映射刷新前清空，避免残留键码或标记把新设备的首次
                                // 按下误判为重复（纯映射重绑定不改变物理按键状态）。
                                let mut state = self.0.borrow_mut();
                                state.key_repeat_tracker.reset();
                                state.forwarded_key_presses.clear();
                                drop(state);
                                last_keymap_change_event = Some(event);
                            }
                            Event::XkbMapNotify(_) => {
                                if let Some(release_event) = last_key_release.take() {
                                    events.push(release_event.into());
                                }
                                last_keymap_change_event = Some(event);
                            }
                            _ => {
                                if matches!(&event, Event::FocusOut(_)) {
                                    // 焦点离开后旧窗口可能收不到仍按住按键的 KeyRelease，
                                    // 残留键码会把之后的真实按键误判为重复；在途 XIM
                                    // 回调也已失去原焦点语义，一并丢弃。
                                    let mut state = self.0.borrow_mut();
                                    state.key_repeat_tracker.reset();
                                    state.forwarded_key_presses.clear();
                                }
                                if let Some(release_event) = last_key_release.take() {
                                    events.push(release_event.into());
                                }
                                events.push(event.into());
                            }
                        }
                    }
                    Ok(None) => {
                        break;
                    }
                    Err(err @ ConnectionError::IoError(..)) => {
                        return Err(EventHandlerError::from(err));
                    }
                    Err(err) => {
                        let err = handle_connection_error(err);
                        log::warn!("error while polling for X11 events: {err:?}");
                        break;
                    }
                }
            }

            if let Some(release_event) = last_key_release.take() {
                events.push(release_event.into());
            }
            if let Some(keymap_change_event) = last_keymap_change_event.take() {
                events.push(keymap_change_event.into());
            }

            if events.is_empty() && windows_to_refresh.is_empty() {
                break;
            }

            for PendingX11Event {
                event,
                key_press_is_repeat,
            } in events.into_iter()
            {
                let mut state = self.0.borrow_mut();
                if !state.has_xim() {
                    drop(state);
                    self.handle_event_with_repeat(event, key_press_is_repeat);
                    continue;
                }

                let Some((mut ximc, mut xim_handler)) = state.take_xim() else {
                    continue;
                };
                let xim_connected = xim_handler.connected;
                drop(state);

                let xim_filtered = ximc.filter_event(&event, &mut xim_handler);
                let xim_callback_event = xim_handler.last_callback_event.take();

                let mut state = self.0.borrow_mut();
                state.restore_xim(ximc, xim_handler);
                drop(state);

                if let Some(event) = xim_callback_event {
                    self.handle_xim_callback_event(event);
                }

                match xim_filtered {
                    Ok(handled) => {
                        if handled {
                            continue;
                        }
                        if xim_connected {
                            self.xim_handle_event(event, key_press_is_repeat);
                        } else {
                            self.handle_event_with_repeat(event, key_press_is_repeat);
                        }
                    }
                    Err(err) => {
                        // this might happen when xim server crashes on one of the events
                        // we do lose 1-2 keys when crash happens since there is no reliable way to get that info
                        // luckily, x11 sends us window not found error when xim server crashes upon further key press
                        // hence we fall back to handle_event
                        log::error!("XIMClientError: {}", err);
                        let mut state = self.0.borrow_mut();
                        state.take_xim();
                        // 连接已失效，在途转发不会再回调；保留标记只会污染之后的按键。
                        state.forwarded_key_presses.clear();
                        drop(state);
                        self.handle_event_with_repeat(event, key_press_is_repeat);
                    }
                }
            }

            for x_window in windows_to_refresh {
                let window = self
                    .0
                    .borrow()
                    .windows
                    .get(&x_window)
                    .and_then(|window| window.is_mapped.then(|| window.window.clone()));
                if let Some(window) = window {
                    window.refresh(RequestFrameOptions {
                        require_presentation: true,
                        force_render: false,
                        signal_at: None,
                        signal_source: FrameRequestSource::NativeCallback,
                    });
                }
            }
        }
        Ok(())
    }

    pub fn enable_ime(&self) {
        let mut state = self.0.borrow_mut();
        if !state.has_xim() {
            return;
        }

        let Some((mut ximc, xim_handler)) = state.take_xim() else {
            return;
        };
        let mut ic_attributes = ximc
            .build_ic_attributes()
            .push(AttributeName::InputStyle, InputStyle::PREEDIT_CALLBACKS)
            .push(AttributeName::ClientWindow, xim_handler.window)
            .push(AttributeName::FocusWindow, xim_handler.window);

        let window_id = state.keyboard_focused_window;
        drop(state);
        if let Some(window_id) = window_id {
            let Some(window) = self.get_window(window_id) else {
                log::error!("Failed to get window for IME positioning");
                let mut state = self.0.borrow_mut();
                state.ximc = Some(ximc);
                state.xim_handler = Some(xim_handler);
                return;
            };
            if let Some(scaled_area) = window.get_ime_area() {
                ic_attributes =
                    ic_attributes.nested_list(xim::AttributeName::PreeditAttributes, |b| {
                        b.push(
                            xim::AttributeName::SpotLocation,
                            xim::Point {
                                x: u32::from(scaled_area.origin.x + scaled_area.size.width) as i16,
                                y: u32::from(scaled_area.origin.y + scaled_area.size.height) as i16,
                            },
                        );
                    });
            }
        }
        ximc.create_ic(xim_handler.im_id, ic_attributes.build())
            .ok();
        let mut state = self.0.borrow_mut();
        state.restore_xim(ximc, xim_handler);
    }

    /// Focus 切换时清空 XIM 与 dead-key composition，避免旧预编辑跨 FocusId 延续。
    pub fn reset_ime(&self) {
        let mut state = self.0.borrow_mut();
        state.composing = false;
        state.pre_edit_text.take();
        if let Some(compose_state) = state.compose_state.as_mut() {
            compose_state.reset();
        }
        if let Some(mut ximc) = state.ximc.take() {
            if let Some(xim_handler) = state.xim_handler.as_ref() {
                ximc.reset_ic(xim_handler.im_id, xim_handler.ic_id).ok();
            } else {
                log::error!("bug: xim handler not set in reset_ime");
            }
            state.ximc = Some(ximc);
        }
    }

    pub(crate) fn get_window(&self, win: xproto::Window) -> Option<X11WindowStatePtr> {
        let state = self.0.borrow();
        state
            .windows
            .get(&win)
            .map(|window_reference| window_reference.window.clone())
    }

    /// 处理单个 X11 事件；滚轮轴的全局值在焦点/指针重新进入时先补基线，避免把首个增量当作初始化值丢弃。
    ///
    /// @author kongweiguang
    fn handle_event(&self, event: Event) -> Option<()> {
        self.handle_event_with_repeat(event, false)
    }

    /// 处理单个 X11 事件，并携带原始轮询层判定的按键重复标记。
    ///
    /// 重复标记只能由看到完整 KeyPress/KeyRelease 序列的轮询层给出；事件处理层
    /// 单独判断会漏掉被 XIM 过滤或在批次边界分开的重复，导致终端快捷键重复触发。
    ///
    /// @author kongweiguang
    fn handle_event_with_repeat(&self, event: Event, key_press_is_repeat: bool) -> Option<()> {
        match event {
            Event::UnmapNotify(event) => {
                let mut state = self.0.borrow_mut();
                if let Some(window_ref) = state.windows.get_mut(&event.window) {
                    window_ref.is_mapped = false;
                }
                handle_visibility_changed(state, event.window);
            }
            Event::MapNotify(event) => {
                let mut state = self.0.borrow_mut();
                if let Some(window_ref) = state.windows.get_mut(&event.window) {
                    window_ref.is_mapped = true;
                }
                handle_visibility_changed(state, event.window);
            }
            Event::VisibilityNotify(event) => {
                let mut state = self.0.borrow_mut();
                if let Some(window_ref) = state.windows.get_mut(&event.window) {
                    window_ref.last_visibility = event.state;
                }
                handle_visibility_changed(state, event.window);
            }
            Event::ClientMessage(event) => {
                let window = self.get_window(event.window)?;
                let [atom, arg1, arg2, arg3, arg4] = event.data.as_data32();
                let delete_window_atom = self.0.borrow().atoms.WM_DELETE_WINDOW;

                if atom == delete_window_atom {
                    if window.should_close() {
                        // window "x" button clicked by user
                        // Rest of the close logic is handled in drop_window()
                        window.close();
                    }
                    return Some(());
                }

                let mut state = self.0.borrow_mut();
                if atom == state.atoms._NET_WM_SYNC_REQUEST {
                    window.state.borrow_mut().last_sync_counter =
                        Some(x11rb::protocol::sync::Int64 {
                            lo: arg2,
                            hi: arg3 as i32,
                        })
                }

                if event.type_ == state.atoms.XdndEnter {
                    state.xdnd_state.other_window = atom;
                    if (arg1 & 0x1) == 0x1 {
                        state.xdnd_state.drag_type = xdnd_get_supported_atom(
                            &state.xcb_connection,
                            &state.atoms,
                            state.xdnd_state.other_window,
                        );
                    } else {
                        if let Some(atom) = [arg2, arg3, arg4]
                            .into_iter()
                            .find(|atom| xdnd_is_atom_supported(*atom, &state.atoms))
                        {
                            state.xdnd_state.drag_type = atom;
                        }
                    }
                } else if event.type_ == state.atoms.XdndLeave {
                    let position = state.xdnd_state.position;
                    drop(state);
                    window
                        .handle_input(PlatformInput::FileDrop(FileDropEvent::Pending { position }));
                    window.handle_input(PlatformInput::FileDrop(FileDropEvent::Exited {}));
                    self.0.borrow_mut().xdnd_state = Xdnd::default();
                } else if event.type_ == state.atoms.XdndPosition {
                    if let Ok(pos) = get_reply(
                        || "Failed to query pointer position",
                        state.xcb_connection.query_pointer(event.window),
                    ) {
                        state.xdnd_state.position =
                            Point::new(px(pos.win_x as f32), px(pos.win_y as f32));
                    }
                    if !state.xdnd_state.retrieved {
                        check_reply(
                            || "Failed to convert selection for drag and drop",
                            state.xcb_connection.convert_selection(
                                event.window,
                                state.atoms.XdndSelection,
                                state.xdnd_state.drag_type,
                                state.atoms.XDND_DATA,
                                arg3,
                            ),
                        )
                        .log_err();
                    }
                    xdnd_send_status(
                        &state.xcb_connection,
                        &state.atoms,
                        event.window,
                        state.xdnd_state.other_window,
                        arg4,
                    );
                    let position = state.xdnd_state.position;
                    drop(state);
                    window
                        .handle_input(PlatformInput::FileDrop(FileDropEvent::Pending { position }));
                } else if event.type_ == state.atoms.XdndDrop {
                    xdnd_send_finished(
                        &state.xcb_connection,
                        &state.atoms,
                        event.window,
                        state.xdnd_state.other_window,
                    );
                    let position = state.xdnd_state.position;
                    drop(state);
                    window
                        .handle_input(PlatformInput::FileDrop(FileDropEvent::Submit { position }));
                    self.0.borrow_mut().xdnd_state = Xdnd::default();
                }
            }
            Event::SelectionNotify(event) => {
                let window = self.get_window(event.requestor)?;
                let state = self.0.borrow_mut();
                let reply = get_reply(
                    || "Failed to get XDND_DATA",
                    state.xcb_connection.get_property(
                        false,
                        event.requestor,
                        state.atoms.XDND_DATA,
                        AtomEnum::ANY,
                        0,
                        1024,
                    ),
                )
                .log_err();
                let Some(reply) = reply else {
                    return Some(());
                };
                if let Ok(file_list) = str::from_utf8(&reply.value) {
                    let paths: SmallVec<[_; 2]> = file_list
                        .lines()
                        .filter_map(|path| Url::parse(path).log_err())
                        .filter_map(|url| match url.to_file_path() {
                            Ok(url) => Some(url),
                            Err(()) => {
                                log::error!("Failed turn {url:?} into a file path");
                                None
                            }
                        })
                        .collect();
                    let input = PlatformInput::FileDrop(FileDropEvent::Entered {
                        position: state.xdnd_state.position,
                        paths: gpui::ExternalPaths(paths),
                    });
                    drop(state);
                    window.handle_input(input);
                    self.0.borrow_mut().xdnd_state.retrieved = true;
                }
            }
            Event::ConfigureNotify(event) => {
                let bounds = Bounds {
                    origin: Point {
                        x: event.x.into(),
                        y: event.y.into(),
                    },
                    size: Size {
                        width: event.width.into(),
                        height: event.height.into(),
                    },
                };
                let window = self.get_window(event.window)?;
                window
                    .set_bounds(bounds)
                    .context("X11: Failed to set window bounds")
                    .log_err();
            }
            Event::PropertyNotify(event) => {
                let window = self.get_window(event.window)?;
                window
                    .property_notify(event)
                    .context("X11: Failed to handle property notify")
                    .log_err();
            }
            Event::FocusIn(event) => {
                let window = self.get_window(event.event)?;
                window.set_active(true);
                let mut state = self.0.borrow_mut();
                state.keyboard_focused_window = Some(event.event);
                if let Some(handler) = state.xim_handler.as_mut() {
                    handler.window = event.event;
                }
                refresh_pointer_device_states_if_needed(&mut state);
                drop(state);
                self.enable_ime();
            }
            Event::FocusOut(event) => {
                let window = self.get_window(event.event)?;
                window.set_active(false);
                let mut state = self.0.borrow_mut();
                // Set last scroll values to `None` so that a large delta isn't created if scrolling is done outside the window (the valuator is global)
                reset_all_pointer_device_scroll_positions(&mut state.pointer_device_states);
                state.recent_smooth_scroll_motions.clear();
                state.keyboard_focused_window = None;
                if let Some(compose_state) = state.compose_state.as_mut() {
                    compose_state.reset();
                }
                state.pre_edit_text.take();
                state.restore_cursor_after_hide();
                drop(state);
                self.reset_ime();
                window.handle_ime_delete();
            }
            Event::XkbNewKeyboardNotify(_) | Event::XkbMapNotify(_) => {
                let mut state = self.0.borrow_mut();
                let xkb_state = {
                    let xkb_keymap = xkbc::x11::keymap_new_from_device(
                        &state.xkb_context,
                        &state.xcb_connection,
                        state.xkb_device_id,
                        xkbc::KEYMAP_COMPILE_NO_FLAGS,
                    );
                    xkbc::x11::state_new_from_device(
                        &xkb_keymap,
                        &state.xcb_connection,
                        state.xkb_device_id,
                    )
                };
                state.xkb = xkb_state;
                drop(state);
                self.handle_keyboard_layout_change();
            }
            Event::XkbStateNotify(event) => {
                let mut state = self.0.borrow_mut();
                let old_layout = state.xkb.serialize_layout(STATE_LAYOUT_EFFECTIVE);
                let new_layout = u32::from(event.group);
                state.xkb.update_mask(
                    event.base_mods.into(),
                    event.latched_mods.into(),
                    event.locked_mods.into(),
                    event.base_group as u32,
                    event.latched_group as u32,
                    event.locked_group.into(),
                );
                let modifiers = modifiers_from_xkb(&state.xkb);
                let capslock = capslock_from_xkb(&state.xkb);
                if state.last_modifiers_changed_event == modifiers
                    && state.last_capslock_changed_event == capslock
                {
                    drop(state);
                } else {
                    let focused_window_id = state.keyboard_focused_window?;
                    state.modifiers = modifiers;
                    state.last_modifiers_changed_event = modifiers;
                    state.capslock = capslock;
                    state.last_capslock_changed_event = capslock;
                    drop(state);

                    let focused_window = self.get_window(focused_window_id)?;
                    focused_window.handle_input(PlatformInput::ModifiersChanged(
                        ModifiersChangedEvent {
                            modifiers,
                            capslock,
                        },
                    ));
                }

                if new_layout != old_layout {
                    self.handle_keyboard_layout_change();
                }
            }
            Event::KeyPress(event) => {
                let window = self.get_window(event.event)?;
                let mut state = self.0.borrow_mut();

                let modifiers = modifiers_from_state(event.state);
                state.modifiers = modifiers;
                state.pre_key_char_down.take();
                let key_event_state = xkb_state_for_key_event(&state.xkb, event.state);

                let keystroke = {
                    let code = event.detail.into();
                    let mut keystroke = keystroke_from_xkb(&key_event_state, modifiers, code);
                    let keysym = key_event_state.key_get_one_sym(code);

                    if keysym.is_modifier_key() {
                        return Some(());
                    }

                    if let Some(mut compose_state) = state.compose_state.take() {
                        compose_state.feed(keysym);
                        match compose_state.status() {
                            xkbc::Status::Composed => {
                                state.pre_edit_text.take();
                                keystroke.key_char = compose_state.utf8();
                                if let Some(keysym) = compose_state.keysym() {
                                    keystroke.key = xkbc::keysym_get_name(keysym);
                                }
                            }
                            xkbc::Status::Composing => {
                                keystroke.key_char = None;
                                state.pre_edit_text = compose_state
                                    .utf8()
                                    .or(keystroke_underlying_dead_key(keysym));
                                let pre_edit =
                                    state.pre_edit_text.clone().unwrap_or(String::default());
                                drop(state);
                                window.handle_ime_preedit(pre_edit);
                                state = self.0.borrow_mut();
                            }
                            xkbc::Status::Cancelled => {
                                let pre_edit = state.pre_edit_text.take();
                                drop(state);
                                if let Some(pre_edit) = pre_edit {
                                    window.handle_ime_commit(pre_edit);
                                }
                                if let Some(current_key) = keystroke_underlying_dead_key(keysym) {
                                    window.handle_ime_preedit(current_key);
                                }
                                state = self.0.borrow_mut();
                                compose_state.feed(keysym);
                            }
                            _ => {}
                        }
                        state.compose_state = Some(compose_state);
                    }
                    keystroke
                };
                drop(state);
                window.handle_input(PlatformInput::KeyDown(gpui::KeyDownEvent {
                    keystroke,
                    is_held: key_press_is_repeat,
                    prefer_character_input: false,
                }));
            }
            Event::KeyRelease(event) => {
                let window = self.get_window(event.event)?;
                let mut state = self.0.borrow_mut();

                let modifiers = modifiers_from_state(event.state);
                state.modifiers = modifiers;
                let key_event_state = xkb_state_for_key_event(&state.xkb, event.state);

                let keystroke = {
                    let code = event.detail.into();
                    let keystroke = keystroke_from_xkb(&key_event_state, modifiers, code);
                    let keysym = key_event_state.key_get_one_sym(code);

                    if keysym.is_modifier_key() {
                        return Some(());
                    }

                    keystroke
                };
                drop(state);
                window.handle_input(PlatformInput::KeyUp(gpui::KeyUpEvent { keystroke }));
            }
            Event::XinputButtonPress(event) => {
                let window = self.get_window(event.event)?;
                let mut state = self.0.borrow_mut();

                let modifiers = modifiers_from_xinput_info(event.mods);
                state.modifiers = modifiers;

                let position = point(
                    px(event.event_x as f32 / u16::MAX as f32 / state.scale_factor),
                    px(event.event_y as f32 / u16::MAX as f32 / state.scale_factor),
                );

                if state.composing && state.ximc.is_some() {
                    drop(state);
                    self.reset_ime();
                    window.handle_ime_unmark();
                    state = self.0.borrow_mut();
                } else if let Some(text) = state.pre_edit_text.take() {
                    if let Some(compose_state) = state.compose_state.as_mut() {
                        compose_state.reset();
                    }
                    drop(state);
                    window.handle_ime_commit(text);
                    state = self.0.borrow_mut();
                }
                match button_or_scroll_from_event_detail(event.detail) {
                    Some(ButtonOrScroll::Button(button)) => {
                        let click_elapsed = state.last_click.elapsed();
                        if click_elapsed < DOUBLE_CLICK_INTERVAL
                            && state
                                .last_mouse_button
                                .is_some_and(|prev_button| prev_button == button)
                            && is_within_click_distance(state.last_location, position)
                        {
                            state.current_count += 1;
                        } else {
                            state.current_count = 1;
                        }

                        state.last_click = Instant::now();
                        state.last_mouse_button = Some(button);
                        state.last_location = position;
                        let current_count = state.current_count;

                        drop(state);
                        window.handle_input(PlatformInput::MouseDown(gpui::MouseDownEvent {
                            button,
                            position,
                            modifiers,
                            click_count: current_count,
                            first_mouse: false,
                        }));
                    }
                    Some(ButtonOrScroll::Scroll(direction)) => {
                        let dispatch_emulated = !event
                            .flags
                            .contains(xinput::PointerEventFlags::POINTER_EMULATED)
                            || should_dispatch_emulated_scroll(
                                &direction,
                                event.sourceid,
                                event.time,
                                &state.recent_smooth_scroll_motions,
                            );
                        drop(state);
                        // Smooth motion normally precedes its emulated button at the same XI2 timestamp.
                        // Use the button only when no matching nonzero axis delta was already dispatched, closing query/re-entry races without double scrolling.
                        if dispatch_emulated {
                            let scroll_delta = match direction {
                                ScrollDirection::Up => Point::new(0.0, SCROLL_LINES),
                                ScrollDirection::Down => Point::new(0.0, -SCROLL_LINES),
                                ScrollDirection::Left => Point::new(SCROLL_LINES, 0.0),
                                ScrollDirection::Right => Point::new(-SCROLL_LINES, 0.0),
                            };
                            window.handle_input(PlatformInput::ScrollWheel(
                                make_scroll_wheel_event(position, scroll_delta, modifiers),
                            ));
                        }
                    }
                    None => {
                        log::error!("Unknown x11 button: {}", event.detail);
                    }
                }
            }
            Event::XinputButtonRelease(event) => {
                let window = self.get_window(event.event)?;
                let mut state = self.0.borrow_mut();
                let modifiers = modifiers_from_xinput_info(event.mods);
                state.modifiers = modifiers;

                let position = point(
                    px(event.event_x as f32 / u16::MAX as f32 / state.scale_factor),
                    px(event.event_y as f32 / u16::MAX as f32 / state.scale_factor),
                );
                match button_or_scroll_from_event_detail(event.detail) {
                    Some(ButtonOrScroll::Button(button)) => {
                        let click_count = state.current_count;
                        drop(state);
                        window.handle_input(PlatformInput::MouseUp(gpui::MouseUpEvent {
                            button,
                            position,
                            modifiers,
                            click_count,
                        }));
                    }
                    Some(ButtonOrScroll::Scroll(_)) => {}
                    None => {}
                }
            }
            Event::XinputMotion(event) => {
                let window = self.get_window(event.event)?;
                let mut state = self.0.borrow_mut();
                state.restore_cursor_after_hide();
                if window.is_blocked() {
                    // We want to set the cursor to the default arrow
                    // when the window is blocked
                    let style = CursorStyle::Arrow;

                    let current_style = state
                        .cursor_styles
                        .get(&window.x_window)
                        .unwrap_or(&CursorStyle::Arrow);
                    if *current_style != style
                        && let Some(cursor) = state.get_cursor_icon(style)
                    {
                        state.cursor_styles.insert(window.x_window, style);
                        check_reply(
                            || "Failed to set cursor style",
                            state.xcb_connection.change_window_attributes(
                                window.x_window,
                                &ChangeWindowAttributesAux {
                                    cursor: Some(cursor),
                                    ..Default::default()
                                },
                            ),
                        )
                        .log_err();
                        state.xcb_connection.flush().log_err();
                    };
                }
                let pressed_button = pressed_button_from_mask(event.button_mask[0]);
                let position = point(
                    px(event.event_x as f32 / u16::MAX as f32 / state.scale_factor),
                    px(event.event_y as f32 / u16::MAX as f32 / state.scale_factor),
                );
                let modifiers = modifiers_from_xinput_info(event.mods);
                state.modifiers = modifiers;
                drop(state);

                if event.valuator_mask[0] & 3 != 0 {
                    window.handle_input(PlatformInput::MouseMove(gpui::MouseMoveEvent {
                        position,
                        pressed_button,
                        modifiers,
                    }));
                }

                state = self.0.borrow_mut();
                if let Some(pointer) = state.pointer_device_states.get_mut(&event.sourceid) {
                    let scroll_delta = get_scroll_delta_and_update_state(pointer, &event);
                    if let Some(delta) = scroll_delta {
                        record_smooth_scroll_motion(
                            &mut state.recent_smooth_scroll_motions,
                            SmoothScrollMotion {
                                sourceid: event.sourceid,
                                time: event.time,
                                delta,
                            },
                        );
                    }
                    drop(state);
                    if let Some(scroll_delta) = scroll_delta {
                        window.handle_input(PlatformInput::ScrollWheel(make_scroll_wheel_event(
                            position,
                            scroll_delta,
                            modifiers,
                        )));
                    }
                }
            }
            Event::XinputEnter(event) if event.mode == xinput::NotifyMode::NORMAL => {
                let window = self.get_window(event.event)?;
                window.set_hovered(true);
                let mut state = self.0.borrow_mut();
                state.mouse_focused_window = Some(event.event);
                state.restore_cursor_after_hide();
                refresh_pointer_device_states_if_needed(&mut state);
            }
            Event::XinputLeave(event) if event.mode == xinput::NotifyMode::NORMAL => {
                let mut state = self.0.borrow_mut();

                // Set last scroll values to `None` so that a large delta isn't created if scrolling is done outside the window (the valuator is global)
                reset_all_pointer_device_scroll_positions(&mut state.pointer_device_states);
                state.recent_smooth_scroll_motions.clear();
                state.mouse_focused_window = None;
                let pressed_button = pressed_button_from_mask(event.buttons[0]);
                let position = point(
                    px(event.event_x as f32 / u16::MAX as f32 / state.scale_factor),
                    px(event.event_y as f32 / u16::MAX as f32 / state.scale_factor),
                );
                let modifiers = modifiers_from_xinput_info(event.mods);
                state.modifiers = modifiers;
                drop(state);

                let window = self.get_window(event.event)?;
                window.handle_input(PlatformInput::MouseExited(gpui::MouseExitEvent {
                    pressed_button,
                    position,
                    modifiers,
                }));
                window.set_hovered(false);
            }
            Event::XinputHierarchy(event) => {
                let mut state = self.0.borrow_mut();
                // Temporarily use `state.pointer_device_states` to only store pointers that still have valid scroll values.
                // Any change to a device invalidates its scroll values.
                for info in event.infos {
                    if is_pointer_device(info.type_) {
                        state.pointer_device_states.remove(&info.deviceid);
                    }
                }
                if let Some(pointer_device_states) = current_pointer_device_states(
                    &state.xcb_connection,
                    &state.pointer_device_states,
                ) {
                    state.pointer_device_states = pointer_device_states;
                }
            }
            Event::XinputDeviceChanged(event) => {
                let mut state = self.0.borrow_mut();
                if let Some(pointer) = state.pointer_device_states.get_mut(&event.sourceid) {
                    reset_pointer_device_scroll_positions(pointer);
                }
                state
                    .recent_smooth_scroll_motions
                    .retain(|motion| motion.sourceid != event.sourceid);
                refresh_pointer_device_states_if_needed(&mut state);
            }
            Event::XinputGesturePinchBegin(event) => {
                let window = self.get_window(event.event)?;
                let mut state = self.0.borrow_mut();
                state.pinch_scale = 1.0;
                let modifiers = modifiers_from_xinput_info(event.mods);
                state.modifiers = modifiers;
                let position = point(
                    px(event.event_x as f32 / u16::MAX as f32 / state.scale_factor),
                    px(event.event_y as f32 / u16::MAX as f32 / state.scale_factor),
                );
                drop(state);
                window.handle_input(PlatformInput::Pinch(gpui::PinchEvent {
                    position,
                    delta: 0.0,
                    modifiers,
                    phase: gpui::TouchPhase::Started,
                }));
            }
            Event::XinputGesturePinchUpdate(event) => {
                let window = self.get_window(event.event)?;
                let mut state = self.0.borrow_mut();
                let modifiers = modifiers_from_xinput_info(event.mods);
                state.modifiers = modifiers;
                let position = point(
                    px(event.event_x as f32 / u16::MAX as f32 / state.scale_factor),
                    px(event.event_y as f32 / u16::MAX as f32 / state.scale_factor),
                );
                // scale is in FP16.16 format: divide by 65536 to get the float value
                let new_absolute_scale = event.scale as f32 / 65536.0;
                let previous_scale = state.pinch_scale;
                let zoom_delta = new_absolute_scale - previous_scale;
                state.pinch_scale = new_absolute_scale;
                drop(state);
                window.handle_input(PlatformInput::Pinch(gpui::PinchEvent {
                    position,
                    delta: zoom_delta,
                    modifiers,
                    phase: gpui::TouchPhase::Moved,
                }));
            }
            Event::XinputGesturePinchEnd(event) => {
                let window = self.get_window(event.event)?;
                let mut state = self.0.borrow_mut();
                state.pinch_scale = 1.0;
                let modifiers = modifiers_from_xinput_info(event.mods);
                state.modifiers = modifiers;
                let position = point(
                    px(event.event_x as f32 / u16::MAX as f32 / state.scale_factor),
                    px(event.event_y as f32 / u16::MAX as f32 / state.scale_factor),
                );
                drop(state);
                window.handle_input(PlatformInput::Pinch(gpui::PinchEvent {
                    position,
                    delta: 0.0,
                    modifiers,
                    phase: gpui::TouchPhase::Ended,
                }));
            }
            _ => {}
        };

        Some(())
    }

    /// 处理 XIM 回调；转发回来的按键要恢复转发前记录的重复语义。
    ///
    /// @author kongweiguang
    fn handle_xim_callback_event(&self, event: XimCallbackEvent) {
        match event {
            XimCallbackEvent::XimXEvent(event) => {
                let key_press_is_repeat = self.take_forwarded_key_press_repeat(&event);
                self.handle_event_with_repeat(event, key_press_is_repeat);
            }
            XimCallbackEvent::XimCommitEvent(window, text) => {
                self.xim_handle_commit(window, text);
            }
            XimCallbackEvent::XimPreeditEvent(window, text) => {
                self.xim_handle_preedit(window, text);
            }
        };
    }

    /// 取回 XIM 原样转发回调所对应按键的重复标记。
    ///
    /// XIM 协议保留目标窗口、键码与时间戳，多条转发可能先后甚至乱序返回；按身份
    /// 从有界队列认领，匹配不到（被输入法消费或已过期）就按首次按下处理。
    fn take_forwarded_key_press_repeat(&self, event: &Event) -> bool {
        let Event::KeyPress(key_press) = event else {
            return false;
        };
        self.0
            .borrow_mut()
            .forwarded_key_presses
            .take(key_press, Instant::now())
            .unwrap_or(false)
    }

    /// 把原始按键转交 XIM，并在转发前记住 KeyPress 的重复标记。
    ///
    /// 转发是异步的，多个 KeyPress 可能同时处于在途状态；标记必须按原始事件身份
    /// 排队保存，否则后一个按键会覆盖前一个的 held 语义。
    ///
    /// @author kongweiguang
    fn xim_handle_event(&self, event: Event, key_press_is_repeat: bool) -> Option<()> {
        let is_key_press = matches!(&event, Event::KeyPress(_));
        match event {
            Event::KeyPress(event) | Event::KeyRelease(event) => {
                let mut state = self.0.borrow_mut();
                state.pre_key_char_down = Some(keystroke_from_xkb(
                    &state.xkb,
                    state.modifiers,
                    event.detail.into(),
                ));
                if is_key_press {
                    // 先入队再转发：XIM_FORWARD_EVENT 回调只带回事件本身，
                    // 没有这份标记，重复按键会在回调路径上退化成全新按下。
                    state
                        .forwarded_key_presses
                        .push(&event, key_press_is_repeat, Instant::now());
                }
                let (mut ximc, mut xim_handler) = state.take_xim()?;
                drop(state);
                xim_handler.window = event.event;
                ximc.forward_event(
                    xim_handler.im_id,
                    xim_handler.ic_id,
                    xim::ForwardEventFlag::empty(),
                    &event,
                )
                .context("X11: Failed to forward XIM event")
                .log_err();
                let mut state = self.0.borrow_mut();
                state.restore_xim(ximc, xim_handler);
                drop(state);
            }
            event => {
                self.handle_event(event);
            }
        }
        Some(())
    }

    fn xim_handle_commit(&self, window: xproto::Window, text: String) -> Option<()> {
        let Some(window) = self.get_window(window) else {
            log::error!("bug: Failed to get window for XIM commit");
            return None;
        };
        let mut state = self.0.borrow_mut();
        state.composing = false;
        drop(state);
        window.handle_ime_commit(text);
        Some(())
    }

    fn xim_handle_preedit(&self, window: xproto::Window, text: String) -> Option<()> {
        let Some(window) = self.get_window(window) else {
            log::error!("bug: Failed to get window for XIM preedit");
            return None;
        };

        let mut state = self.0.borrow_mut();
        let (mut ximc, xim_handler) = state.take_xim()?;
        state.composing = !text.is_empty();
        drop(state);
        window.handle_ime_preedit(text);

        if let Some(scaled_area) = window.get_ime_area() {
            let ic_attributes = ximc
                .build_ic_attributes()
                .push(
                    xim::AttributeName::InputStyle,
                    xim::InputStyle::PREEDIT_CALLBACKS,
                )
                .push(xim::AttributeName::ClientWindow, xim_handler.window)
                .push(xim::AttributeName::FocusWindow, xim_handler.window)
                .nested_list(xim::AttributeName::PreeditAttributes, |b| {
                    b.push(
                        xim::AttributeName::SpotLocation,
                        xim::Point {
                            x: u32::from(scaled_area.origin.x + scaled_area.size.width) as i16,
                            y: u32::from(scaled_area.origin.y + scaled_area.size.height) as i16,
                        },
                    );
                })
                .build();
            ximc.set_ic_values(xim_handler.im_id, xim_handler.ic_id, ic_attributes)
                .ok();
        }
        let mut state = self.0.borrow_mut();
        state.restore_xim(ximc, xim_handler);
        drop(state);
        Some(())
    }

    fn handle_keyboard_layout_change(&self) {
        let mut state = self.0.borrow_mut();
        let layout_idx = state.xkb.serialize_layout(STATE_LAYOUT_EFFECTIVE);
        let keymap = state.xkb.get_keymap();
        let layout_name = keymap.layout_get_name(layout_idx);
        if layout_name != state.keyboard_layout.name() {
            state.keyboard_layout = LinuxKeyboardLayout::new(layout_name.to_string().into());
            let callback = state
                .common
                .borrow_mut()
                .callbacks
                .keyboard_layout_change
                .take();
            if let Some(mut callback) = callback {
                drop(state);
                callback();
                state = self.0.borrow_mut();
                state.common.borrow_mut().callbacks.keyboard_layout_change = Some(callback);
            }
        }
    }
}

impl X11Client {
    pub(crate) fn compositor_name(&self) -> &'static str {
        "X11"
    }

    pub(crate) fn with_common<R>(&self, f: impl FnOnce(&mut LinuxCommon) -> R) -> R {
        let common = self.0.borrow().common.clone();
        f(&mut common.borrow_mut())
    }

    pub(crate) fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        let state = self.0.borrow();
        Box::new(state.keyboard_layout.clone())
    }

    pub(crate) fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        let state = self.0.borrow();
        let setup = state.xcb_connection.setup();
        setup
            .roots
            .iter()
            .enumerate()
            .filter_map(|(root_id, _)| {
                Some(Rc::new(
                    X11Display::new(&state.xcb_connection, state.scale_factor, root_id).ok()?,
                ) as Rc<dyn PlatformDisplay>)
            })
            .collect()
    }

    pub(crate) fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        let state = self.0.borrow();
        X11Display::new(
            &state.xcb_connection,
            state.scale_factor,
            state.x_root_index,
        )
        .log_err()
        .map(|display| Rc::new(display) as Rc<dyn PlatformDisplay>)
    }

    #[cfg(feature = "screen-capture")]
    pub(crate) fn is_screen_capture_supported(&self) -> bool {
        true
    }

    #[cfg(feature = "screen-capture")]
    pub(crate) fn screen_capture_sources(
        &self,
    ) -> futures::channel::oneshot::Receiver<anyhow::Result<Vec<Rc<dyn gpui::ScreenCaptureSource>>>>
    {
        let foreground_executor = self.0.borrow().common.borrow().foreground_executor.clone();
        gpui::scap_screen_capture::scap_screen_sources(&foreground_executor)
    }

    pub(crate) fn open_window(
        &self,
        handle: AnyWindowHandle,
        params: WindowParams,
    ) -> anyhow::Result<Box<dyn PlatformWindow>> {
        let mut state = self.0.borrow_mut();
        let parent_window = state
            .keyboard_focused_window
            .and_then(|focused_window| state.windows.get(&focused_window))
            .map(|w| w.window.clone());
        let x_window = state
            .xcb_connection
            .generate_id()
            .context("X11: Failed to generate window ID")?;

        let xcb_connection = state.xcb_connection.clone();
        let client_side_decorations_supported = state.client_side_decorations_supported;
        let x_root_index = state.x_root_index;
        let atoms = state.atoms;
        let scale_factor = state.scale_factor;
        let appearance = state.common.borrow().appearance;
        let compositor_gpu = state.compositor_gpu.take();
        let supports_xinput_gestures = state.supports_xinput_gestures;
        let is_bgr = state
            .resource_database
            .get_string("Xft.rgba", "Xft.Rgba")
            .is_some_and(|v| v.eq_ignore_ascii_case("bgr"));
        let window = X11Window::new(
            handle,
            X11ClientStatePtr(Rc::downgrade(&self.0)),
            state.common.borrow().foreground_executor.clone(),
            state.gpu_context.clone(),
            compositor_gpu,
            params,
            &xcb_connection,
            client_side_decorations_supported,
            x_root_index,
            x_window,
            &atoms,
            scale_factor,
            appearance,
            parent_window,
            supports_xinput_gestures,
            is_bgr,
        )?;
        check_reply(
            || "Failed to set XdndAware property",
            state.xcb_connection.change_property32(
                xproto::PropMode::REPLACE,
                x_window,
                state.atoms.XdndAware,
                state.atoms.XA_ATOM,
                &[5],
            ),
        )
        .log_err();
        xcb_flush(&state.xcb_connection);

        let window_ref = WindowRef {
            window: window.0.clone(),
            refresh_state: None,
            last_visibility: Visibility::UNOBSCURED,
            is_mapped: false,
        };

        state.windows.insert(x_window, window_ref);
        Ok(Box::new(window))
    }

    pub(crate) fn set_cursor_style(&self, style: CursorStyle) {
        let mut state = self.0.borrow_mut();
        let Some(focused_window) = state.mouse_focused_window else {
            return;
        };
        let current_style = state
            .cursor_styles
            .get(&focused_window)
            .unwrap_or(&CursorStyle::Arrow);

        let window = state
            .mouse_focused_window
            .and_then(|w| state.windows.get(&w));

        let should_change = *current_style != style
            && (window.is_none() || window.is_some_and(|w| !w.is_blocked()));

        if !should_change {
            return;
        }

        state.cursor_styles.insert(focused_window, style);

        // Don't clobber the invisible cursor; restore reads back from `cursor_styles`.
        if state.cursor_hidden_window == Some(focused_window) {
            return;
        }

        let Some(cursor) = state.get_cursor_icon(style) else {
            return;
        };

        check_reply(
            || "Failed to set cursor style",
            state.xcb_connection.change_window_attributes(
                focused_window,
                &ChangeWindowAttributesAux {
                    cursor: Some(cursor),
                    ..Default::default()
                },
            ),
        )
        .log_err();
        state.xcb_connection.flush().log_err();
    }

    pub(crate) fn hide_cursor_until_mouse_moves(&self) {
        self.0.borrow_mut().hide_cursor_until_mouse_moves();
    }

    pub(crate) fn is_cursor_visible(&self) -> bool {
        self.0.borrow().cursor_hidden_window.is_none()
    }

    pub(crate) fn open_uri(&self, uri: &str) {
        open_uri_internal(
            self.with_common(|c| c.background_executor.clone()),
            uri,
            None,
            self.graphical_environment(),
        );
    }

    pub(crate) fn reveal_path(&self, path: PathBuf) {
        reveal_path_internal(
            self.with_common(|c| c.background_executor.clone()),
            path,
            None,
            self.graphical_environment(),
        );
    }

    pub(crate) fn write_to_primary(&self, item: gpui::ClipboardItem) {
        let state = self.0.borrow_mut();
        state
            .clipboard
            .set_text(
                std::borrow::Cow::Owned(item.text().unwrap_or_default()),
                clipboard::ClipboardKind::Primary,
                clipboard::WaitConfig::None,
            )
            .context("X11 Failed to write to clipboard (primary)")
            .log_with_level(log::Level::Debug);
    }

    pub(crate) fn write_to_clipboard(&self, item: gpui::ClipboardItem) {
        let mut state = self.0.borrow_mut();
        state
            .clipboard
            .set_text(
                std::borrow::Cow::Owned(item.text().unwrap_or_default()),
                clipboard::ClipboardKind::Clipboard,
                clipboard::WaitConfig::None,
            )
            .context("X11: Failed to write to clipboard (clipboard)")
            .log_with_level(log::Level::Debug);
        state.clipboard_item.replace(item);
    }

    pub(crate) fn read_from_primary(&self) -> Option<gpui::ClipboardItem> {
        let state = self.0.borrow_mut();
        state
            .clipboard
            .get_any(clipboard::ClipboardKind::Primary)
            .context("X11: Failed to read from clipboard (primary)")
            .log_with_level(log::Level::Debug)
    }

    pub(crate) fn read_from_clipboard(&self) -> Option<gpui::ClipboardItem> {
        let state = self.0.borrow_mut();
        // if the last copy was from this app, return our cached item
        // which has metadata attached.
        if state
            .clipboard
            .is_owner(clipboard::ClipboardKind::Clipboard)
        {
            return state.clipboard_item.clone();
        }
        state
            .clipboard
            .get_any(clipboard::ClipboardKind::Clipboard)
            .context("X11: Failed to read from clipboard (clipboard)")
            .log_with_level(log::Level::Debug)
    }

    pub(crate) fn active_window(&self) -> Option<AnyWindowHandle> {
        let state = self.0.borrow();
        state.keyboard_focused_window.and_then(|focused_window| {
            state
                .windows
                .get(&focused_window)
                .map(|window| window.handle())
        })
    }

    pub(crate) fn window_stack(&self) -> Option<Vec<AnyWindowHandle>> {
        let state = self.0.borrow();
        let root = state.xcb_connection.setup().roots[state.x_root_index].root;

        let reply = state
            .xcb_connection
            .get_property(
                false,
                root,
                state.atoms._NET_CLIENT_LIST_STACKING,
                xproto::AtomEnum::WINDOW,
                0,
                u32::MAX,
            )
            .ok()?
            .reply()
            .ok()?;

        let window_ids = reply
            .value
            .chunks_exact(4)
            .filter_map(|chunk| chunk.try_into().ok().map(u32::from_ne_bytes))
            .collect::<Vec<xproto::Window>>();

        let mut handles = Vec::new();

        // We need to reverse, since _NET_CLIENT_LIST_STACKING has
        // a back-to-front order.
        // See: https://specifications.freedesktop.org/wm-spec/1.3/ar01s03.html
        for window_ref in window_ids
            .iter()
            .rev()
            .filter_map(|&win| state.windows.get(&win))
        {
            handles.push(window_ref.handle());
        }

        Some(handles)
    }

    pub(crate) fn window_identifier(
        &self,
    ) -> impl Future<Output = Option<WindowIdentifier>> + Send + 'static {
        let state = self.0.borrow();
        state
            .keyboard_focused_window
            .and_then(|focused_window| state.windows.get(&focused_window))
            .map(|window| window.window.x_window as u64)
            .map(|x_window| std::future::ready(Some(WindowIdentifier::from_xid(x_window))))
            .unwrap_or(std::future::ready(None))
    }
}

impl X11ClientState {
    fn has_xim(&self) -> bool {
        self.ximc.is_some() && self.xim_handler.is_some()
    }

    fn take_xim(&mut self) -> Option<(X11rbClient<Rc<XCBConnection>>, XimHandler)> {
        let ximc = self
            .ximc
            .take()
            .ok_or(anyhow!("bug: XIM connection not set"))
            .log_err()?;
        if let Some(xim_handler) = self.xim_handler.take() {
            Some((ximc, xim_handler))
        } else {
            self.ximc = Some(ximc);
            log::error!("bug: XIM handler not set");
            None
        }
    }

    fn restore_xim(&mut self, ximc: X11rbClient<Rc<XCBConnection>>, xim_handler: XimHandler) {
        self.ximc = Some(ximc);
        self.xim_handler = Some(xim_handler);
    }

    fn update_refresh_loop(&mut self, x_window: xproto::Window) {
        let Some(window_ref) = self.windows.get_mut(&x_window) else {
            return;
        };
        let is_visible = window_ref.visibility().is_visible();
        match (is_visible, window_ref.refresh_state.take()) {
            (false, refresh_state @ Some(RefreshState::Hidden { .. }))
            | (false, refresh_state @ None)
            | (true, refresh_state @ Some(RefreshState::PeriodicRefresh { .. })) => {
                window_ref.refresh_state = refresh_state;
            }
            (
                false,
                Some(RefreshState::PeriodicRefresh {
                    refresh_rate,
                    event_loop_token,
                }),
            ) => {
                self.loop_handle.remove(event_loop_token);
                window_ref.refresh_state = Some(RefreshState::Hidden { refresh_rate });
            }
            (true, Some(RefreshState::Hidden { refresh_rate })) => {
                let event_loop_token = self.start_refresh_loop(x_window, refresh_rate);
                let Some(window_ref) = self.windows.get_mut(&x_window) else {
                    return;
                };
                window_ref.refresh_state = Some(RefreshState::PeriodicRefresh {
                    refresh_rate,
                    event_loop_token,
                });
            }
            (true, None) => {
                let Some(screen_resources) = get_reply(
                    || "Failed to get screen resources",
                    self.xcb_connection
                        .randr_get_screen_resources_current(x_window),
                )
                .log_err() else {
                    return;
                };

                // Ideally this would be re-queried when the window changes screens, but there
                // doesn't seem to be an efficient / straightforward way to do this. Should also be
                // updated when screen configurations change.
                let mode_info = screen_resources.crtcs.iter().find_map(|crtc| {
                    let crtc_info = self
                        .xcb_connection
                        .randr_get_crtc_info(*crtc, x11rb::CURRENT_TIME)
                        .ok()?
                        .reply()
                        .ok()?;

                    screen_resources
                        .modes
                        .iter()
                        .find(|m| m.id == crtc_info.mode)
                });
                let refresh_rate = match mode_info {
                    Some(mode_info) => mode_refresh_rate(mode_info),
                    None => {
                        log::error!(
                            "Failed to get screen mode info from xrandr, \
                            defaulting to 60hz refresh rate."
                        );
                        Duration::from_micros(1_000_000 / 60)
                    }
                };

                let event_loop_token = self.start_refresh_loop(x_window, refresh_rate);
                let Some(window_ref) = self.windows.get_mut(&x_window) else {
                    return;
                };
                window_ref.refresh_state = Some(RefreshState::PeriodicRefresh {
                    refresh_rate,
                    event_loop_token,
                });
            }
        }
    }

    #[must_use]
    fn start_refresh_loop(
        &self,
        x_window: xproto::Window,
        refresh_rate: Duration,
    ) -> RegistrationToken {
        self.loop_handle
            .insert_source(calloop::timer::Timer::immediate(), {
                let this = self.this.clone();
                move |mut instant, (), _| {
                    let Some(client) = this.get_client() else {
                        return calloop::timer::TimeoutAction::Drop;
                    };
                    let xcb_connection = {
                        let mut state = client.0.borrow_mut();
                        let xcb_connection = state.xcb_connection.clone();
                        if let Some(window) = state.windows.get_mut(&x_window) {
                            let window = window.window.clone();
                            drop(state);
                            window.refresh(RequestFrameOptions {
                                require_presentation: false,
                                force_render: false,
                                signal_at: PlatformFrameSignal::capture(|| instant),
                                signal_source: FrameRequestSource::LocalSchedule,
                            });
                        }
                        xcb_connection
                    };
                    client.process_x11_events(&xcb_connection).log_err();

                    // Take into account that some frames have been skipped
                    let now = Instant::now();
                    while instant < now {
                        instant += refresh_rate;
                    }
                    calloop::timer::TimeoutAction::ToInstant(instant)
                }
            })
            .expect("Failed to initialize window refresh timer")
    }

    fn get_cursor_icon(&mut self, style: CursorStyle) -> Option<xproto::Cursor> {
        if let Some(cursor) = self.cursor_cache.get(&style) {
            return *cursor;
        }

        let result = 'outer: {
            let mut errors = String::new();
            let cursor_icon_names = cursor_style_to_icon_names(style);
            for cursor_icon_name in cursor_icon_names {
                match self
                    .cursor_handle
                    .load_cursor(&self.xcb_connection, cursor_icon_name)
                {
                    Ok(loaded_cursor) => {
                        if loaded_cursor != x11rb::NONE {
                            break 'outer Ok(loaded_cursor);
                        }
                    }
                    Err(err) => {
                        errors.push_str(&err.to_string());
                        errors.push('\n');
                    }
                }
            }
            if errors.is_empty() {
                Err(anyhow!(
                    "errors while loading cursor icons {:?}:\n{}",
                    cursor_icon_names,
                    errors
                ))
            } else {
                Err(anyhow!("did not find cursor icons {:?}", cursor_icon_names))
            }
        };

        let cursor = match result {
            Ok(cursor) => Some(cursor),
            Err(err) => {
                match self
                    .cursor_handle
                    .load_cursor(&self.xcb_connection, DEFAULT_CURSOR_ICON_NAME)
                {
                    Ok(default) => {
                        log_cursor_icon_warning(err.context(format!(
                            "X11: error loading cursor icon, falling back on default icon '{}'",
                            DEFAULT_CURSOR_ICON_NAME
                        )));
                        Some(default)
                    }
                    Err(default_err) => {
                        log_cursor_icon_warning(err.context(default_err).context(format!(
                            "X11: error loading default cursor fallback '{}'",
                            DEFAULT_CURSOR_ICON_NAME
                        )));
                        None
                    }
                }
            }
        };

        self.cursor_cache.insert(style, cursor);
        cursor
    }

    fn get_or_create_invisible_cursor(&mut self) -> Option<xproto::Cursor> {
        if let Some(cursor) = self.invisible_cursor_cache {
            return Some(cursor);
        }
        let cursor = create_invisible_cursor(&self.xcb_connection)
            .context("X11: error while creating invisible cursor")
            .log_err()?;
        self.invisible_cursor_cache = Some(cursor);
        Some(cursor)
    }

    fn hide_cursor_until_mouse_moves(&mut self) {
        if self.cursor_hidden_window.is_some() {
            return;
        }
        let Some(focused_window) = self.mouse_focused_window else {
            // No window to apply the per-window invisible cursor to.
            return;
        };
        let Some(invisible_cursor) = self.get_or_create_invisible_cursor() else {
            return;
        };
        check_reply(
            || "Failed to hide cursor",
            self.xcb_connection.change_window_attributes(
                focused_window,
                &ChangeWindowAttributesAux {
                    cursor: Some(invisible_cursor),
                    ..Default::default()
                },
            ),
        )
        .log_err();
        self.xcb_connection.flush().log_err();
        self.cursor_hidden_window = Some(focused_window);
    }

    fn restore_cursor_after_hide(&mut self) {
        let Some(hidden_window) = self.cursor_hidden_window.take() else {
            return;
        };
        let style = self
            .cursor_styles
            .get(&hidden_window)
            .copied()
            .unwrap_or(CursorStyle::Arrow);
        let Some(cursor) = self.get_cursor_icon(style) else {
            log::warn!(
                "X11: no cursor icon available to restore {:?} after hide; cursor may stay invisible",
                style
            );
            return;
        };
        check_reply(
            || "Failed to restore cursor style after hide",
            self.xcb_connection.change_window_attributes(
                hidden_window,
                &ChangeWindowAttributesAux {
                    cursor: Some(cursor),
                    ..Default::default()
                },
            ),
        )
        .log_err();
        self.xcb_connection.flush().log_err();
    }
}

// Adapted from:
// https://docs.rs/winit/0.29.11/src/winit/platform_impl/linux/x11/monitor.rs.html#103-111
pub fn mode_refresh_rate(mode: &randr::ModeInfo) -> Duration {
    if mode.dot_clock == 0 || mode.htotal == 0 || mode.vtotal == 0 {
        return Duration::from_millis(16);
    }

    let millihertz = mode.dot_clock as u64 * 1_000 / (mode.htotal as u64 * mode.vtotal as u64);
    let micros = 1_000_000_000 / millihertz;
    log::info!("Refreshing every {}ms", micros / 1_000);
    Duration::from_micros(micros)
}

/// Applies a mapped/obscured change to the refresh loop and reports the
/// resulting visibility to the window. Consumes the client borrow because the
/// visibility callback re-enters GPUI.
fn handle_visibility_changed(mut state: RefMut<'_, X11ClientState>, x_window: xproto::Window) {
    state.update_refresh_loop(x_window);
    let Some(window_ref) = state.windows.get(&x_window) else {
        return;
    };
    let visibility = window_ref.visibility();
    let window = window_ref.window.clone();
    drop(state);
    window.set_visibility(visibility);
}

fn fp3232_to_f32(value: xinput::Fp3232) -> f32 {
    value.integral as f32 + value.frac as f32 / u32::MAX as f32
}

fn detect_compositor_gpu(
    xcb_connection: &XCBConnection,
    screen: &xproto::Screen,
) -> Option<CompositorGpuHint> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    xcb_connection
        .extension_information(dri3::X11_EXTENSION_NAME)
        .ok()??;

    let reply = dri3::open(xcb_connection, screen.root, 0)
        .ok()?
        .reply()
        .ok()?;
    let fd = reply.device_fd;

    let path = format!("/proc/self/fd/{}", fd.as_raw_fd());
    let metadata = std::fs::metadata(&path).ok()?;

    crate::linux::compositor_gpu_hint_from_dev_t(metadata.rdev())
}

fn check_compositor_present(xcb_connection: &XCBConnection, root: xproto::Window) -> bool {
    // Method 1: Check for _NET_WM_CM_S{root}
    let atom_name = format!("_NET_WM_CM_S{}", root);
    let atom1 = get_reply(
        || format!("Failed to intern {atom_name}"),
        xcb_connection.intern_atom(false, atom_name.as_bytes()),
    );
    let method1 = match atom1.log_with_level(Level::Debug) {
        Some(reply) if reply.atom != x11rb::NONE => {
            let atom = reply.atom;
            get_reply(
                || format!("Failed to get {atom_name} owner"),
                xcb_connection.get_selection_owner(atom),
            )
            .map(|reply| reply.owner != 0)
            .log_with_level(Level::Debug)
            .unwrap_or(false)
        }
        _ => false,
    };

    // Method 2: Check for _NET_WM_CM_OWNER
    let atom_name = "_NET_WM_CM_OWNER";
    let atom2 = get_reply(
        || format!("Failed to intern {atom_name}"),
        xcb_connection.intern_atom(false, atom_name.as_bytes()),
    );
    let method2 = match atom2.log_with_level(Level::Debug) {
        Some(reply) if reply.atom != x11rb::NONE => {
            let atom = reply.atom;
            get_reply(
                || format!("Failed to get {atom_name}"),
                xcb_connection.get_property(false, root, atom, xproto::AtomEnum::WINDOW, 0, 1),
            )
            .map(|reply| reply.value_len > 0)
            .unwrap_or(false)
        }
        _ => return false,
    };

    // Method 3: Check for _NET_SUPPORTING_WM_CHECK
    let atom_name = "_NET_SUPPORTING_WM_CHECK";
    let atom3 = get_reply(
        || format!("Failed to intern {atom_name}"),
        xcb_connection.intern_atom(false, atom_name.as_bytes()),
    );
    let method3 = match atom3.log_with_level(Level::Debug) {
        Some(reply) if reply.atom != x11rb::NONE => {
            let atom = reply.atom;
            get_reply(
                || format!("Failed to get {atom_name}"),
                xcb_connection.get_property(false, root, atom, xproto::AtomEnum::WINDOW, 0, 1),
            )
            .map(|reply| reply.value_len > 0)
            .unwrap_or(false)
        }
        _ => return false,
    };

    log::debug!(
        "Compositor detection: _NET_WM_CM_S?={}, _NET_WM_CM_OWNER={}, _NET_SUPPORTING_WM_CHECK={}",
        method1,
        method2,
        method3
    );

    method1 || method2 || method3
}

fn check_gtk_frame_extents_supported(
    xcb_connection: &XCBConnection,
    atoms: &XcbAtoms,
    root: xproto::Window,
) -> bool {
    let Some(supported_atoms) = get_reply(
        || "Failed to get _NET_SUPPORTED",
        xcb_connection.get_property(
            false,
            root,
            atoms._NET_SUPPORTED,
            xproto::AtomEnum::ATOM,
            0,
            1024,
        ),
    )
    .log_with_level(Level::Debug) else {
        return false;
    };

    let supported_atom_ids: Vec<u32> = supported_atoms
        .value
        .chunks_exact(4)
        .filter_map(|chunk| chunk.try_into().ok().map(u32::from_ne_bytes))
        .collect();

    supported_atom_ids.contains(&atoms._GTK_FRAME_EXTENTS)
}

fn xdnd_is_atom_supported(atom: u32, atoms: &XcbAtoms) -> bool {
    atom == atoms.TEXT
        || atom == atoms.STRING
        || atom == atoms.UTF8_STRING
        || atom == atoms.TEXT_PLAIN
        || atom == atoms.TEXT_PLAIN_UTF8
        || atom == atoms.TextUriList
}

fn xdnd_get_supported_atom(
    xcb_connection: &XCBConnection,
    supported_atoms: &XcbAtoms,
    target: xproto::Window,
) -> u32 {
    if let Some(reply) = get_reply(
        || "Failed to get XDnD supported atoms",
        xcb_connection.get_property(
            false,
            target,
            supported_atoms.XdndTypeList,
            AtomEnum::ANY,
            0,
            1024,
        ),
    )
    .log_with_level(Level::Warn)
        && let Some(atoms) = reply.value32()
    {
        for atom in atoms {
            if xdnd_is_atom_supported(atom, supported_atoms) {
                return atom;
            }
        }
    }
    0
}

fn xdnd_send_finished(
    xcb_connection: &XCBConnection,
    atoms: &XcbAtoms,
    source: xproto::Window,
    target: xproto::Window,
) {
    let message = ClientMessageEvent {
        format: 32,
        window: target,
        type_: atoms.XdndFinished,
        data: ClientMessageData::from([source, 1, atoms.XdndActionCopy, 0, 0]),
        sequence: 0,
        response_type: xproto::CLIENT_MESSAGE_EVENT,
    };
    check_reply(
        || "Failed to send XDnD finished event",
        xcb_connection.send_event(false, target, EventMask::default(), message),
    )
    .log_err();
    xcb_connection.flush().log_err();
}

fn xdnd_send_status(
    xcb_connection: &XCBConnection,
    atoms: &XcbAtoms,
    source: xproto::Window,
    target: xproto::Window,
    action: u32,
) {
    let message = ClientMessageEvent {
        format: 32,
        window: target,
        type_: atoms.XdndStatus,
        data: ClientMessageData::from([source, 1, 0, 0, action]),
        sequence: 0,
        response_type: xproto::CLIENT_MESSAGE_EVENT,
    };
    check_reply(
        || "Failed to send XDnD status event",
        xcb_connection.send_event(false, target, EventMask::default(), message),
    )
    .log_err();
    xcb_connection.flush().log_err();
}

/// Recomputes scroll axes from XI2 device classes, preserving a valid event-stream baseline before using the server's current valuator value.
/// The valuator is global, so this seeds startup and focus re-entry without inventing zero; preserving an old baseline avoids rebasing over queued motion.
///
/// @author kongweiguang
fn current_pointer_device_states(
    xcb_connection: &XCBConnection,
    scroll_values_to_preserve: &BTreeMap<xinput::DeviceId, PointerDeviceState>,
) -> Option<BTreeMap<xinput::DeviceId, PointerDeviceState>> {
    let devices_query_result = get_reply(
        || "Failed to query XInput devices",
        xcb_connection.xinput_xi_query_device(XINPUT_ALL_DEVICES),
    )
    .log_err()?;

    let mut pointer_device_states = BTreeMap::new();
    pointer_device_states.extend(
        devices_query_result
            .infos
            .iter()
            .filter(|info| is_pointer_device(info.type_))
            .filter_map(|info| {
                let scroll_data = info
                    .classes
                    .iter()
                    .filter_map(|class| class.data.as_scroll())
                    .copied()
                    .rev()
                    .collect::<Vec<_>>();
                let old_state = scroll_values_to_preserve.get(&info.deviceid);
                let old_horizontal = old_state.map(|state| &state.horizontal);
                let old_vertical = old_state.map(|state| &state.vertical);
                let horizontal = scroll_data
                    .iter()
                    .find(|data| data.scroll_type == xinput::ScrollType::HORIZONTAL)
                    .map(|data| {
                        scroll_data_to_axis_state(
                            data,
                            old_horizontal,
                            current_valuator_value(&info.classes, data.number),
                        )
                    });
                let vertical = scroll_data
                    .iter()
                    .find(|data| data.scroll_type == xinput::ScrollType::VERTICAL)
                    .map(|data| {
                        scroll_data_to_axis_state(
                            data,
                            old_vertical,
                            current_valuator_value(&info.classes, data.number),
                        )
                    });
                if horizontal.is_none() && vertical.is_none() {
                    None
                } else {
                    Some((
                        info.deviceid,
                        PointerDeviceState {
                            horizontal: horizontal.unwrap_or_else(Default::default),
                            vertical: vertical.unwrap_or_else(Default::default),
                        },
                    ))
                }
            }),
    );
    if pointer_device_states.is_empty() {
        log::error!("Found no xinput mouse pointers.");
    }
    Some(pointer_device_states)
}

/// Finds the current valuator sample paired with one scroll class on the same XI2 device.
/// Matching by valuator number is required because valuator arrays can contain unrelated axes; absence stays unknown instead of becoming zero.
///
/// @author kongweiguang
fn current_valuator_value(
    classes: &[xinput::DeviceClass],
    valuator_number: u16,
) -> Option<xinput::Fp3232> {
    classes
        .iter()
        .filter_map(|class| class.data.as_valuator())
        .find(|valuator| valuator.number == valuator_number)
        .map(|valuator| valuator.value)
}

/// Rebuilds invalid scroll baselines after focus or pointer re-entry while leaving valid event baselines untouched.
/// FocusIn and XinputEnter may arrive together; only missing axes trigger a query, preventing the second event from rebasing over an already queued wheel.
///
/// @author kongweiguang
fn refresh_pointer_device_states_if_needed(state: &mut X11ClientState) {
    if !pointer_device_states_need_refresh(&state.pointer_device_states) {
        return;
    }

    if let Some(refreshed) =
        current_pointer_device_states(&state.xcb_connection, &state.pointer_device_states)
    {
        state.pointer_device_states = refreshed;
    }
}

/// Reports whether a query can seed an axis baseline that is currently absent.
/// Axes without a scroll valuator do not make the map incomplete; an empty map still needs a topology query.
///
/// @author kongweiguang
fn pointer_device_states_need_refresh(
    pointer_device_states: &BTreeMap<xinput::DeviceId, PointerDeviceState>,
) -> bool {
    pointer_device_states.is_empty()
        || pointer_device_states.values().any(|device| {
            [&device.horizontal, &device.vertical]
                .into_iter()
                .any(|axis| axis.valuator_number.is_some() && axis.scroll_value.is_none())
        })
}

/// Returns true if the device is a pointer device. Does not include pointer device groups.
fn is_pointer_device(type_: xinput::DeviceType) -> bool {
    type_ == xinput::DeviceType::SLAVE_POINTER
}

/// Combines a scroll-class conversion with its current valuator sample, preferring a still-valid event baseline over query time.
/// That priority keeps a device query from overwriting queued motion; a changed valuator number starts a new baseline.
///
/// @author kongweiguang
fn scroll_data_to_axis_state(
    data: &xinput::DeviceClassDataScroll,
    old_axis_state_with_valid_scroll_value: Option<&ScrollAxisState>,
    queried_valuator_value: Option<xinput::Fp3232>,
) -> ScrollAxisState {
    ScrollAxisState {
        valuator_number: Some(data.number),
        multiplier: SCROLL_LINES / fp3232_to_f32(data.increment),
        scroll_value: old_axis_state_with_valid_scroll_value
            .filter(|state| state.valuator_number == Some(data.number))
            .and_then(|state| state.scroll_value)
            .or_else(|| queried_valuator_value.map(fp3232_to_f32)),
    }
}

fn reset_all_pointer_device_scroll_positions(
    pointer_device_states: &mut BTreeMap<xinput::DeviceId, PointerDeviceState>,
) {
    pointer_device_states
        .iter_mut()
        .for_each(|(_, device_state)| reset_pointer_device_scroll_positions(device_state));
}

fn reset_pointer_device_scroll_positions(pointer: &mut PointerDeviceState) {
    pointer.horizontal.scroll_value = None;
    pointer.vertical.scroll_value = None;
}

/// Returns the scroll delta for a smooth scrolling motion event, or `None` if no scroll data is present.
fn get_scroll_delta_and_update_state(
    pointer: &mut PointerDeviceState,
    event: &xinput::MotionEvent,
) -> Option<Point<f32>> {
    let delta_x = get_axis_scroll_delta_and_update_state(event, &mut pointer.horizontal);
    let delta_y = get_axis_scroll_delta_and_update_state(event, &mut pointer.vertical);
    if delta_x.is_some() || delta_y.is_some() {
        Some(Point::new(delta_x.unwrap_or(0.0), delta_y.unwrap_or(0.0)))
    } else {
        None
    }
}

/// Retains a bounded set of dispatched motions so interleaved devices, axes, and ordinary pointer moves cannot erase pending emulation matches.
/// Zero deltas do not cover a button; the 64-entry cap bounds memory even when a device never emits legacy buttons.
///
/// @author kongweiguang
fn record_smooth_scroll_motion(
    recent_motions: &mut VecDeque<SmoothScrollMotion>,
    motion: SmoothScrollMotion,
) {
    if motion.delta.x == 0.0 && motion.delta.y == 0.0 {
        return;
    }
    if recent_motions.len() == 64 {
        recent_motions.pop_front();
    }
    recent_motions.push_back(motion);
}

/// Uses a legacy button only when no recent motion covered its source, timestamp, axis, and direction.
/// Each fallback button keeps its own notch: one XI2 motion can aggregate several legacy presses at the same timestamp.
///
/// @author kongweiguang
fn should_dispatch_emulated_scroll(
    direction: &ScrollDirection,
    sourceid: xinput::DeviceId,
    time: xproto::Timestamp,
    recent_motions: &VecDeque<SmoothScrollMotion>,
) -> bool {
    !recent_motions.iter().any(|motion| {
        motion.sourceid == sourceid
            && motion.time == time
            && match direction {
                ScrollDirection::Up => motion.delta.y > 0.0,
                ScrollDirection::Down => motion.delta.y < 0.0,
                ScrollDirection::Left => motion.delta.x > 0.0,
                ScrollDirection::Right => motion.delta.x < 0.0,
            }
    })
}

fn get_axis_scroll_delta_and_update_state(
    event: &xinput::MotionEvent,
    axis: &mut ScrollAxisState,
) -> Option<f32> {
    let axis_index = get_valuator_axis_index(&event.valuator_mask, axis.valuator_number?)?;
    if let Some(axis_value) = event.axisvalues.get(axis_index) {
        let new_scroll = fp3232_to_f32(*axis_value);
        let delta_scroll = axis
            .scroll_value
            .map(|old_scroll| (old_scroll - new_scroll) * axis.multiplier);
        axis.scroll_value = Some(new_scroll);
        delta_scroll
    } else {
        log::error!("Encountered invalid XInput valuator_mask, scrolling may not work properly.");
        None
    }
}

fn make_scroll_wheel_event(
    position: Point<Pixels>,
    scroll_delta: Point<f32>,
    modifiers: Modifiers,
) -> gpui::ScrollWheelEvent {
    // When shift is held down, vertical scrolling turns into horizontal scrolling.
    let delta = if modifiers.shift {
        Point {
            x: scroll_delta.y,
            y: 0.0,
        }
    } else {
        scroll_delta
    };
    gpui::ScrollWheelEvent {
        position,
        delta: ScrollDelta::Lines(delta),
        modifiers,
        touch_phase: TouchPhase::default(),
    }
}

fn create_invisible_cursor(
    connection: &XCBConnection,
) -> anyhow::Result<crate::linux::x11::client::xproto::Cursor> {
    let empty_pixmap = connection.generate_id()?;
    let root = connection.setup().roots[0].root;
    connection.create_pixmap(1, empty_pixmap, root, 1, 1)?;

    let cursor = connection.generate_id()?;
    connection.create_cursor(cursor, empty_pixmap, empty_pixmap, 0, 0, 0, 0, 0, 0, 0, 0)?;

    connection.free_pixmap(empty_pixmap)?;

    xcb_flush(connection);
    Ok(cursor)
}

enum DpiMode {
    Randr,
    Scale(f32),
    NotSet,
}

fn get_scale_factor(
    connection: &XCBConnection,
    resource_database: &Database,
    screen_index: usize,
) -> f32 {
    let env_dpi = std::env::var(GPUI_X11_SCALE_FACTOR_ENV)
        .ok()
        .map(|var| {
            if var.to_lowercase() == "randr" {
                DpiMode::Randr
            } else if let Ok(scale) = var.parse::<f32>() {
                if valid_scale_factor(scale) {
                    DpiMode::Scale(scale)
                } else {
                    panic!(
                        "`{}` must be a positive normal number or `randr`. Got `{}`",
                        GPUI_X11_SCALE_FACTOR_ENV, var
                    );
                }
            } else if var.is_empty() {
                DpiMode::NotSet
            } else {
                panic!(
                    "`{}` must be a positive number or `randr`. Got `{}`",
                    GPUI_X11_SCALE_FACTOR_ENV, var
                );
            }
        })
        .unwrap_or(DpiMode::NotSet);

    match env_dpi {
        DpiMode::Scale(scale) => {
            log::info!(
                "Using scale factor from {}: {}",
                GPUI_X11_SCALE_FACTOR_ENV,
                scale
            );
            return scale;
        }
        DpiMode::Randr => {
            if let Some(scale) = get_randr_scale_factor(connection, screen_index) {
                log::info!(
                    "Using RandR scale factor from {}=randr: {}",
                    GPUI_X11_SCALE_FACTOR_ENV,
                    scale
                );
                return scale;
            }
            log::warn!("Failed to calculate RandR scale factor, falling back to default");
            return 1.0;
        }
        DpiMode::NotSet => {}
    }

    // TODO: Use scale factor from XSettings here

    if let Some(dpi) = resource_database
        .get_value::<f32>("Xft.dpi", "Xft.dpi")
        .ok()
        .flatten()
    {
        let scale = dpi / 96.0; // base dpi
        log::info!("Using scale factor from Xft.dpi: {}", scale);
        return scale;
    }

    if let Some(scale) = get_randr_scale_factor(connection, screen_index) {
        log::info!("Using RandR scale factor: {}", scale);
        return scale;
    }

    log::info!("Using default scale factor: 1.0");
    1.0
}

fn get_randr_scale_factor(connection: &XCBConnection, screen_index: usize) -> Option<f32> {
    let root = connection.setup().roots.get(screen_index)?.root;

    let version_cookie = connection.randr_query_version(1, 6).ok()?;
    let version_reply = version_cookie.reply().ok()?;
    if version_reply.major_version < 1
        || (version_reply.major_version == 1 && version_reply.minor_version < 5)
    {
        return legacy_get_randr_scale_factor(connection, root); // for randr <1.5
    }

    let monitors_cookie = connection.randr_get_monitors(root, true).ok()?; // true for active only
    let monitors_reply = monitors_cookie.reply().ok()?;

    let mut fallback_scale: Option<f32> = None;
    for monitor in monitors_reply.monitors {
        if monitor.width_in_millimeters == 0 || monitor.height_in_millimeters == 0 {
            continue;
        }
        let scale_factor = get_dpi_factor(
            (monitor.width as u32, monitor.height as u32),
            (
                monitor.width_in_millimeters as u64,
                monitor.height_in_millimeters as u64,
            ),
        );
        if monitor.primary {
            return Some(scale_factor);
        } else if fallback_scale.is_none() {
            fallback_scale = Some(scale_factor);
        }
    }

    fallback_scale
}

fn legacy_get_randr_scale_factor(connection: &XCBConnection, root: u32) -> Option<f32> {
    let primary_cookie = connection.randr_get_output_primary(root).ok()?;
    let primary_reply = primary_cookie.reply().ok()?;
    let primary_output = primary_reply.output;

    let primary_output_cookie = connection
        .randr_get_output_info(primary_output, x11rb::CURRENT_TIME)
        .ok()?;
    let primary_output_info = primary_output_cookie.reply().ok()?;

    // try primary
    if primary_output_info.connection == randr::Connection::CONNECTED
        && primary_output_info.mm_width > 0
        && primary_output_info.mm_height > 0
        && primary_output_info.crtc != 0
    {
        let crtc_cookie = connection
            .randr_get_crtc_info(primary_output_info.crtc, x11rb::CURRENT_TIME)
            .ok()?;
        let crtc_info = crtc_cookie.reply().ok()?;

        if crtc_info.width > 0 && crtc_info.height > 0 {
            let scale_factor = get_dpi_factor(
                (crtc_info.width as u32, crtc_info.height as u32),
                (
                    primary_output_info.mm_width as u64,
                    primary_output_info.mm_height as u64,
                ),
            );
            return Some(scale_factor);
        }
    }

    // fallback: full scan
    let resources_cookie = connection.randr_get_screen_resources_current(root).ok()?;
    let screen_resources = resources_cookie.reply().ok()?;

    let mut crtc_cookies = Vec::with_capacity(screen_resources.crtcs.len());
    for &crtc in &screen_resources.crtcs {
        if let Ok(cookie) = connection.randr_get_crtc_info(crtc, x11rb::CURRENT_TIME) {
            crtc_cookies.push((crtc, cookie));
        }
    }

    let mut crtc_infos: HashMap<randr::Crtc, randr::GetCrtcInfoReply> = HashMap::default();
    let mut valid_outputs: HashSet<randr::Output> = HashSet::new();
    for (crtc, cookie) in crtc_cookies {
        if let Ok(reply) = cookie.reply()
            && reply.width > 0
            && reply.height > 0
            && !reply.outputs.is_empty()
        {
            crtc_infos.insert(crtc, reply.clone());
            valid_outputs.extend(&reply.outputs);
        }
    }

    if valid_outputs.is_empty() {
        return None;
    }

    let mut output_cookies = Vec::with_capacity(valid_outputs.len());
    for &output in &valid_outputs {
        if let Ok(cookie) = connection.randr_get_output_info(output, x11rb::CURRENT_TIME) {
            output_cookies.push((output, cookie));
        }
    }
    let mut output_infos: HashMap<randr::Output, randr::GetOutputInfoReply> = HashMap::default();
    for (output, cookie) in output_cookies {
        if let Ok(reply) = cookie.reply() {
            output_infos.insert(output, reply);
        }
    }

    let mut fallback_scale: Option<f32> = None;
    for crtc_info in crtc_infos.values() {
        for &output in &crtc_info.outputs {
            if let Some(output_info) = output_infos.get(&output) {
                if output_info.connection != randr::Connection::CONNECTED {
                    continue;
                }

                if output_info.mm_width == 0 || output_info.mm_height == 0 {
                    continue;
                }

                let scale_factor = get_dpi_factor(
                    (crtc_info.width as u32, crtc_info.height as u32),
                    (output_info.mm_width as u64, output_info.mm_height as u64),
                );

                if output != primary_output && fallback_scale.is_none() {
                    fallback_scale = Some(scale_factor);
                }
            }
        }
    }

    fallback_scale
}

fn get_dpi_factor((width_px, height_px): (u32, u32), (width_mm, height_mm): (u64, u64)) -> f32 {
    let ppmm = ((width_px as f64 * height_px as f64) / (width_mm as f64 * height_mm as f64)).sqrt(); // pixels per mm

    const MM_PER_INCH: f64 = 25.4;
    const BASE_DPI: f64 = 96.0;
    const QUANTIZE_STEP: f64 = 12.0; // e.g. 1.25 = 15/12, 1.5 = 18/12, 1.75 = 21/12, 2.0 = 24/12
    const MIN_SCALE: f64 = 1.0;
    const MAX_SCALE: f64 = 20.0;

    let dpi_factor =
        ((ppmm * (QUANTIZE_STEP * MM_PER_INCH / BASE_DPI)).round() / QUANTIZE_STEP).max(MIN_SCALE);

    let validated_factor = if dpi_factor <= MAX_SCALE {
        dpi_factor
    } else {
        MIN_SCALE
    };

    if valid_scale_factor(validated_factor as f32) {
        validated_factor as f32
    } else {
        log::warn!(
            "Calculated DPI factor {} is invalid, using 1.0",
            validated_factor
        );
        1.0
    }
}

#[inline]
fn valid_scale_factor(scale_factor: f32) -> bool {
    scale_factor.is_sign_positive() && scale_factor.is_normal()
}

#[inline]
fn xkb_state_for_key_event(xkb: &xkbc::State, event_state: xproto::KeyButMask) -> xkbc::State {
    let keymap = xkb.get_keymap();
    let mut key_event_state = xkbc::State::new(&keymap);

    let latched_modifiers = xkb.serialize_mods(xkbc::STATE_MODS_LATCHED);
    let locked_modifiers = xkb.serialize_mods(xkbc::STATE_MODS_LOCKED);
    let active_modifier_mask: xkbc::ModMask = u16::from(
        event_state
            & (xproto::KeyButMask::SHIFT
                | xproto::KeyButMask::LOCK
                | xproto::KeyButMask::CONTROL
                | xproto::KeyButMask::MOD1
                | xproto::KeyButMask::MOD2
                | xproto::KeyButMask::MOD3
                | xproto::KeyButMask::MOD4
                | xproto::KeyButMask::MOD5),
    )
    .into();
    let depressed_modifiers = active_modifier_mask & !(latched_modifiers | locked_modifiers);

    key_event_state.update_mask(
        depressed_modifiers,
        latched_modifiers,
        locked_modifiers,
        xkb.serialize_layout(xkbc::STATE_LAYOUT_DEPRESSED),
        xkb.serialize_layout(xkbc::STATE_LAYOUT_LATCHED),
        xkb.serialize_layout(xkbc::STATE_LAYOUT_LOCKED),
    );

    key_event_state
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a same-device XI2 valuator fixture so tests exercise number-based scroll-axis pairing.
    ///
    /// @author kongweiguang
    fn test_valuator_class(number: u16, value: i32) -> xinput::DeviceClass {
        xinput::DeviceClass {
            len: 0,
            sourceid: 7,
            data: xinput::DeviceClassData::Valuator(xinput::DeviceClassDataValuator {
                number,
                label: 0,
                min: xinput::Fp3232::default(),
                max: xinput::Fp3232::default(),
                value: xinput::Fp3232 {
                    integral: value,
                    frac: 0,
                },
                resolution: 0,
                mode: xinput::ValuatorMode::RELATIVE,
            }),
        }
    }

    /// Creates a fixed scroll class with a nonzero increment for axis-state tests.
    ///
    /// @author kongweiguang
    fn test_scroll_class(number: u16) -> xinput::DeviceClassDataScroll {
        xinput::DeviceClassDataScroll {
            number,
            scroll_type: xinput::ScrollType::VERTICAL,
            flags: xinput::ScrollFlags::default(),
            increment: xinput::Fp3232 {
                integral: 1,
                frac: 0,
            },
        }
    }

    /// Confirms a scroll class uses only its same-number valuator and leaves missing values unknown.
    ///
    /// @author kongweiguang
    #[test]
    fn current_scroll_valuator_matches_the_class_number() {
        let classes = [test_valuator_class(2, 14), test_valuator_class(9, 37)];

        let current = current_valuator_value(&classes, 9).expect("matching valuator");
        assert_eq!(fp3232_to_f32(current), 37.0);
        assert!(current_valuator_value(&classes, 4).is_none());
    }

    /// Confirms XIQueryDevice seeds a missing baseline but cannot overwrite a valid event-stream sample or invent zero.
    ///
    /// @author kongweiguang
    #[test]
    fn scroll_axis_query_seeds_only_missing_baseline() {
        let scroll = test_scroll_class(9);
        let queried = Some(xinput::Fp3232 {
            integral: 37,
            frac: 0,
        });
        let seeded = scroll_data_to_axis_state(&scroll, None, queried);
        assert_eq!(seeded.scroll_value, Some(37.0));

        let valid_old = ScrollAxisState {
            valuator_number: Some(9),
            multiplier: 1.0,
            scroll_value: Some(14.0),
        };
        let preserved = scroll_data_to_axis_state(&scroll, Some(&valid_old), queried);
        assert_eq!(preserved.scroll_value, Some(14.0));

        let changed_axis = ScrollAxisState {
            valuator_number: Some(2),
            ..valid_old
        };
        let replaced = scroll_data_to_axis_state(&scroll, Some(&changed_axis), queried);
        assert_eq!(replaced.scroll_value, Some(37.0));

        let unknown = scroll_data_to_axis_state(&scroll, None, None);
        assert_eq!(unknown.scroll_value, None);
    }

    /// Confirms a missing/rebased smooth delta falls back to its emulated button, while a matching same-time delta is emitted once.
    ///
    /// @author kongweiguang
    #[test]
    fn emulated_scroll_fallback_deduplicates_same_timestamp_motion() {
        let matching = SmoothScrollMotion {
            sourceid: 7,
            time: 101,
            delta: Point::new(0.0, 2.0),
        };
        let recent = VecDeque::from([matching]);

        assert!(!should_dispatch_emulated_scroll(
            &ScrollDirection::Up,
            7,
            101,
            &recent,
        ));
        assert!(should_dispatch_emulated_scroll(
            &ScrollDirection::Up,
            7,
            102,
            &recent,
        ));
        assert!(should_dispatch_emulated_scroll(
            &ScrollDirection::Up,
            8,
            101,
            &recent,
        ));
        assert!(should_dispatch_emulated_scroll(
            &ScrollDirection::Down,
            7,
            101,
            &recent,
        ));
        assert!(should_dispatch_emulated_scroll(
            &ScrollDirection::Left,
            7,
            101,
            &recent,
        ));
        assert!(should_dispatch_emulated_scroll(
            &ScrollDirection::Up,
            7,
            101,
            &VecDeque::new(),
        ));
    }

    /// Keeps matches across split axes and interleaved devices without consuming aggregated legacy notches or growing unbounded state.
    ///
    /// @author kongweiguang
    #[test]
    fn emulated_scroll_matches_interleaved_axes_and_bounds_history() {
        let mut recent = VecDeque::new();
        for (sourceid, delta) in [
            (7, Point::new(0.0, 6.0)),
            (8, Point::new(0.0, -3.0)),
            (7, Point::new(3.0, 0.0)),
            (7, Point::new(0.0, 0.0)),
        ] {
            record_smooth_scroll_motion(
                &mut recent,
                SmoothScrollMotion {
                    sourceid,
                    time: 101,
                    delta,
                },
            );
        }
        for _ in 0..2 {
            assert!(!should_dispatch_emulated_scroll(
                &ScrollDirection::Up,
                7,
                101,
                &recent
            ));
            assert!(!should_dispatch_emulated_scroll(
                &ScrollDirection::Left,
                7,
                101,
                &recent
            ));
        }
        assert!(!should_dispatch_emulated_scroll(
            &ScrollDirection::Down,
            8,
            101,
            &recent
        ));
        assert!(should_dispatch_emulated_scroll(
            &ScrollDirection::Down,
            7,
            101,
            &recent
        ));
        for time in 102..202 {
            record_smooth_scroll_motion(
                &mut recent,
                SmoothScrollMotion {
                    sourceid: 7,
                    time,
                    delta: Point::new(0.0, 3.0),
                },
            );
        }
        assert_eq!(recent.len(), 64);
        assert!(should_dispatch_emulated_scroll(
            &ScrollDirection::Up,
            7,
            101,
            &recent
        ));
    }

    /// Confirms FocusOut/Leave invalidation requests one re-entry query and valid axes do not trigger repeated queries.
    ///
    /// @author kongweiguang
    #[test]
    fn reset_scroll_positions_require_reentry_refresh() {
        let mut states = BTreeMap::from([(
            7,
            PointerDeviceState {
                horizontal: ScrollAxisState {
                    valuator_number: Some(2),
                    multiplier: 1.0,
                    scroll_value: Some(3.0),
                },
                vertical: ScrollAxisState::default(),
            },
        )]);

        assert!(!pointer_device_states_need_refresh(&states));
        reset_all_pointer_device_scroll_positions(&mut states);
        assert!(pointer_device_states_need_refresh(&states));
    }

    fn test_keymap(layouts: &str) -> xkbc::Keymap {
        test_keymap_with_variant(layouts, "")
    }

    fn test_keymap_with_variant(layouts: &str, variant: &str) -> xkbc::Keymap {
        // These fixtures compile layout names from local files, unlike server keymaps.
        let context = xkbc::Context::new(xkbc::CONTEXT_NO_FLAGS);
        assert!(!context.get_raw_ptr().is_null());
        xkbc::Keymap::new_from_names(
            &context,
            "",
            "pc105",
            layouts,
            variant,
            None,
            xkbc::COMPILE_NO_FLAGS,
        )
        .expect("test keymap should compile")
    }

    // Returns a state where the second layout is active via a temporary
    // mechanism (holding a key down or one-shot), not a permanent toggle.
    fn state_with_non_locked_layout(keymap: &xkbc::Keymap) -> xkbc::State {
        let mut depressed_layout_state = xkbc::State::new(keymap);
        depressed_layout_state.update_mask(0, 0, 0, 1, 0, 0);
        if depressed_layout_state.serialize_layout(STATE_LAYOUT_EFFECTIVE) == 1 {
            return depressed_layout_state;
        }

        let mut latched_layout_state = xkbc::State::new(keymap);
        latched_layout_state.update_mask(0, 0, 0, 0, 1, 0);
        if latched_layout_state.serialize_layout(STATE_LAYOUT_EFFECTIVE) == 1 {
            return latched_layout_state;
        }

        panic!("test keymap should support a non-locked secondary layout");
    }

    #[test]
    fn key_event_state_uses_event_modifiers_without_mutating_server_state() {
        let keymap = test_keymap("us");
        let server_state = xkbc::State::new(&keymap);
        // The "9" key on a US keyboard.
        let keycode = keymap
            .key_by_name("AE09")
            .expect("test key should exist in the keymap");

        // Simulate pressing Shift+9 (which should produce "(").
        let key_event_state = xkb_state_for_key_event(&server_state, xproto::KeyButMask::SHIFT);
        let keystroke = keystroke_from_xkb(
            &key_event_state,
            modifiers_from_state(xproto::KeyButMask::SHIFT),
            keycode,
        );

        // Assert Shift+9 produces "(" on US layout.
        assert_eq!(keystroke.key, "(");
        assert_eq!(keystroke.key_char.as_deref(), Some("("));
        // Assert the long-lived server state was not mutated by the key event.
        assert_eq!(server_state.key_get_utf8(keycode), "9");
    }

    #[test]
    fn key_event_state_ignores_pointer_button_bits() {
        let keymap = test_keymap("us");
        let server_state = xkbc::State::new(&keymap);
        // The "9" key on a US keyboard.
        let keycode = keymap
            .key_by_name("AE09")
            .expect("test key should exist in the keymap");

        // Simulate Shift held down.
        let shifted_state = xkb_state_for_key_event(&server_state, xproto::KeyButMask::SHIFT);
        // Simulate Shift held down while also clicking the left mouse button.
        let shifted_with_button_state = xkb_state_for_key_event(
            &server_state,
            xproto::KeyButMask::SHIFT | xproto::KeyButMask::BUTTON1,
        );

        // Assert the mouse button has no effect on modifier state.
        assert_eq!(
            shifted_with_button_state.serialize_mods(xkbc::STATE_MODS_EFFECTIVE),
            shifted_state.serialize_mods(xkbc::STATE_MODS_EFFECTIVE)
        );
        // Assert both cases produce the same character.
        assert_eq!(
            shifted_with_button_state.key_get_utf8(keycode),
            shifted_state.key_get_utf8(keycode)
        );
    }

    #[test]
    fn key_event_state_preserves_non_locked_layout_components() {
        // US + Russian dual-layout keyboard.
        let keymap = test_keymap("us,ru");
        // Simulate the Russian layout being active via a temporary layout
        // switch (holding a key), not a permanent toggle.
        let server_state = state_with_non_locked_layout(&keymap);
        // The "Q" key position, which produces a Cyrillic character in Russian layout.
        let keycode = keymap
            .key_by_name("AD01")
            .expect("test key should exist in the keymap");

        let expected_text = server_state.key_get_utf8(keycode);
        let key_event_state = xkb_state_for_key_event(&server_state, xproto::KeyButMask::default());

        // Assert the temporary layout switch is preserved.
        assert_eq!(
            key_event_state.serialize_layout(STATE_LAYOUT_EFFECTIVE),
            server_state.serialize_layout(STATE_LAYOUT_EFFECTIVE)
        );
        // Assert the key produces the same character as expected from the
        // Russian layout.
        assert_eq!(key_event_state.key_get_utf8(keycode), expected_text);
    }

    // https://github.com/zed-industries/zed/issues/14282
    #[test]
    fn capslock_toggle_produces_uppercase() {
        let keymap = test_keymap("us");
        let mut server_state = xkbc::State::new(&keymap);
        // The "A" key position on a US keyboard.
        let keycode = keymap
            .key_by_name("AC01")
            .expect("'a' key should exist in the keymap");

        // Simulate the user having toggled CapsLock on (it's now permanently
        // active until pressed again).
        let lock_mod = u16::from(xproto::KeyButMask::LOCK) as xkbc::ModMask;
        server_state.update_mask(0, 0, lock_mod, 0, 0, 0);

        // Simulate pressing the "a" key while CapsLock is on.
        let key_event_state = xkb_state_for_key_event(&server_state, xproto::KeyButMask::LOCK);

        // Assert CapsLock is treated as a toggle (locked), not as a held key
        // (depressed). This distinction matters because XKB only applies
        // capitalization when CapsLock is in the "locked" state.
        assert_eq!(
            key_event_state.serialize_mods(xkbc::STATE_MODS_LOCKED) & lock_mod,
            lock_mod,
        );
        // Assert typing "a" with CapsLock on produces "A".
        assert_eq!(key_event_state.key_get_utf8(keycode), "A");
    }

    // https://github.com/zed-industries/zed/issues/14282
    #[test]
    fn neo2_level3_via_capslock_produces_ellipsis() {
        // Neo 2 is a German keyboard layout that repurposes CapsLock as a
        // "level 3" modifier key for accessing additional characters.
        let keymap = test_keymap_with_variant("de", "neo");
        let server_state = xkbc::State::new(&keymap);
        // The key in the "Q" position, which produces "x" on Neo 2 base layer.
        let keycode = keymap
            .key_by_name("AD01")
            .expect("test key should exist in the keymap");

        // Simulate holding CapsLock, which in Neo 2 activates the "level 3"
        // layer (mapped to the Mod5 modifier internally).
        let key_event_state = xkb_state_for_key_event(&server_state, xproto::KeyButMask::MOD5);

        // Assert holding CapsLock + pressing the "x" key produces "..."
        // (ellipsis), which is the level 3 character on that key in Neo 2.
        assert_eq!(key_event_state.key_get_utf8(keycode), "\u{2026}");
    }

    // https://github.com/zed-industries/zed/issues/14282
    #[test]
    fn neo2_latched_mod5_preserved() {
        // Neo 2 also supports "latching" the level 3 modifier (via Caps+Tab),
        // which activates it for only the next keypress and then deactivates.
        let keymap = test_keymap_with_variant("de", "neo");
        let mut server_state = xkbc::State::new(&keymap);
        let keycode = keymap
            .key_by_name("AD01")
            .expect("test key should exist in the keymap");

        // Simulate the level 3 modifier being latched (one-shot active).
        let mod5 = u16::from(xproto::KeyButMask::MOD5) as xkbc::ModMask;
        server_state.update_mask(0, mod5, 0, 0, 0, 0);

        let key_event_state = xkb_state_for_key_event(&server_state, xproto::KeyButMask::MOD5);

        // Assert the modifier stays classified as "latched" (one-shot) rather
        // than being reclassified as "depressed" (held down). This matters
        // because latched modifiers auto-deactivate after one keypress.
        assert_eq!(
            key_event_state.serialize_mods(xkbc::STATE_MODS_LATCHED) & mod5,
            mod5,
        );
        // Assert the latched level 3 still produces the ellipsis character.
        assert_eq!(key_event_state.key_get_utf8(keycode), "\u{2026}");
    }

    // https://github.com/zed-industries/zed/pull/31193
    #[test]
    fn german_layout_correct_key_resolution() {
        // Standard German keyboard layout.
        let keymap = test_keymap("de");
        let server_state = xkbc::State::new(&keymap);
        // The "7" key on the number row.
        let keycode = keymap
            .key_by_name("AE07")
            .expect("'7' key should exist in the keymap");

        let key_event_state = xkb_state_for_key_event(&server_state, xproto::KeyButMask::default());

        // Assert pressing the "7" key on a German layout produces "7".
        assert_eq!(key_event_state.key_get_utf8(keycode), "7");
    }

    // https://github.com/zed-industries/zed/issues/26468
    // https://github.com/zed-industries/zed/issues/16667
    #[test]
    fn space_works_with_cyrillic_layout_active() {
        // US + Russian dual-layout keyboard.
        let keymap = test_keymap("us,ru");
        let mut server_state = xkbc::State::new(&keymap);
        let space = keymap
            .key_by_name("SPCE")
            .expect("space key should exist in the keymap");

        // Simulate the user having switched to the Russian layout
        // (e.g. via a keyboard shortcut like Super+Space).
        server_state.update_mask(0, 0, 0, 0, 0, 1);

        let key_event_state = xkb_state_for_key_event(&server_state, xproto::KeyButMask::default());

        // Assert the Russian layout is still active after constructing the
        // key event state (not accidentally reset to US).
        assert_eq!(key_event_state.serialize_layout(STATE_LAYOUT_EFFECTIVE), 1);
        // Assert pressing space while on the Russian layout still types a space.
        assert_eq!(key_event_state.key_get_utf8(space), " ");
    }

    // https://github.com/zed-industries/zed/issues/40678
    #[test]
    fn macro_shift_bracket_produces_brace() {
        let keymap = test_keymap("us");
        let server_state = xkbc::State::new(&keymap);
        // The "]" key on a US keyboard.
        let bracket = keymap
            .key_by_name("AD12")
            .expect("']' key should exist in the keymap");

        // Simulate a keyboard macro (e.g. from a ZMK/QMK firmware keyboard)
        // that sends Shift + "]" very rapidly. The modifier state notification
        // for Shift hasn't reached us yet, so the server state has no
        // modifiers. But the key event itself carries the correct Shift state.
        assert_eq!(server_state.serialize_mods(xkbc::STATE_MODS_EFFECTIVE), 0);
        let key_event_state = xkb_state_for_key_event(&server_state, xproto::KeyButMask::SHIFT);

        // Assert Shift+"]" produces "}" even when the Shift notification
        // arrived late.
        assert_eq!(key_event_state.key_get_utf8(bracket), "}");
    }

    // https://github.com/zed-industries/zed/issues/49329
    #[test]
    fn sequential_key_events_do_not_corrupt_state() {
        let keymap = test_keymap("us");
        let server_state = xkbc::State::new(&keymap);

        // Simulate typing "a s d" with spaces in between, all without any
        // modifier keys held.
        let keys: &[(&str, &str)] = &[
            ("AC01", "a"),
            ("SPCE", " "),
            ("AC02", "s"),
            ("SPCE", " "),
            ("AC03", "d"),
        ];

        for &(key_name, expected_utf8) in keys {
            let keycode = keymap
                .key_by_name(key_name)
                .expect("test key should exist in the keymap");

            let key_event_state =
                xkb_state_for_key_event(&server_state, xproto::KeyButMask::default());

            // Assert each key in the sequence produces the expected character
            // (no dropped or garbled input from state corruption).
            assert_eq!(
                key_event_state.key_get_utf8(keycode),
                expected_utf8,
                "key {key_name} should produce {expected_utf8:?}",
            );
        }

        // Assert the server state is completely untouched after processing
        // all key events.
        assert_eq!(server_state.serialize_mods(xkbc::STATE_MODS_EFFECTIVE), 0);
        assert_eq!(server_state.serialize_layout(STATE_LAYOUT_EFFECTIVE), 0);
    }

    // https://github.com/zed-industries/zed/issues/26468
    #[test]
    fn space_works_with_czech_layout_active() {
        // US + Czech dual-layout keyboard.
        let keymap = test_keymap("us,cz");
        let mut server_state = xkbc::State::new(&keymap);
        let space = keymap
            .key_by_name("SPCE")
            .expect("space key should exist in the keymap");

        // Simulate the user having switched to the Czech layout.
        server_state.update_mask(0, 0, 0, 0, 0, 1);

        let key_event_state = xkb_state_for_key_event(&server_state, xproto::KeyButMask::default());

        // Assert pressing space while on the Czech layout still types a space.
        assert_eq!(key_event_state.key_get_utf8(space), " ");
    }

    /// Confirms detectable auto-repeat marks every press of a still-down keycode as a
    /// repeat and stops once the real release arrives.
    ///
    /// @author kongweiguang
    #[test]
    fn key_repeat_tracker_marks_detectable_repeats() {
        let mut tracker = KeyRepeatTracker::new();

        assert!(!tracker.press(38));
        assert!(tracker.press(38));
        assert!(tracker.press(38));
        assert!(!tracker.press(39));

        tracker.release(38);
        assert!(!tracker.press(38));
    }

    /// Builds a minimal key event so pair checks can vary keycode, target window, and
    /// timestamp without depending on a live X server.
    ///
    /// @author kongweiguang
    fn test_key_event(
        detail: u8,
        time: xproto::Timestamp,
        event: xproto::Window,
    ) -> xproto::KeyPressEvent {
        xproto::KeyPressEvent {
            response_type: xproto::KEY_PRESS_EVENT,
            detail,
            sequence: 0,
            time,
            root: 1,
            event,
            child: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: xproto::KeyButMask::default(),
            same_screen: true,
        }
    }

    /// Confirms only an adjacent legacy pair with the same keycode, target window, and
    /// server timestamp is treated as X11 auto-repeat; anything else stays a real press.
    ///
    /// @author kongweiguang
    #[test]
    fn synthetic_release_pair_requires_same_key_target_and_timestamp() {
        let mut release = test_key_event(38, 100, 0x200000);
        release.response_type = xproto::KEY_RELEASE_EVENT;
        let press = test_key_event(38, 100, 0x200000);
        assert!(is_synthetic_key_release_pair(&release, &press));

        let mut different_key = release.clone();
        different_key.detail = 39;
        assert!(!is_synthetic_key_release_pair(&different_key, &press));

        let mut different_target = release.clone();
        different_target.event = 0x300000;
        assert!(!is_synthetic_key_release_pair(&different_target, &press));

        let mut different_time = release.clone();
        different_time.time = 101;
        assert!(!is_synthetic_key_release_pair(&different_time, &press));
    }

    /// Confirms queued XIM markers follow the original event identity even when callbacks
    /// arrive out of order, and that expired, cleared, or overflowed entries stay inactive.
    ///
    /// @author kongweiguang
    #[test]
    fn forwarded_key_press_queue_matches_by_identity_and_evicts() {
        let mut queue = ForwardedKeyPressQueue::new();
        let start = Instant::now();
        let held = test_key_event(38, 100, 0x200000);
        let fresh = test_key_event(38, 140, 0x200000);
        queue.push(&held, true, start);
        queue.push(&fresh, false, start);

        // 后转发的先回调：两个在途按键各自认领自己的标记。
        assert_eq!(
            queue.take(&fresh, start + Duration::from_millis(1)),
            Some(false)
        );
        assert_eq!(
            queue.take(&held, start + Duration::from_millis(1)),
            Some(true)
        );
        assert_eq!(queue.take(&held, start + Duration::from_millis(1)), None);

        queue.push(&held, true, start);
        assert_eq!(
            queue.take(
                &held,
                start + FORWARDED_KEY_PRESS_TTL + Duration::from_millis(1)
            ),
            None
        );

        queue.push(&held, true, start);
        queue.clear();
        assert_eq!(queue.take(&held, start), None);

        let oldest = test_key_event(10, 0, 0x200000);
        for index in 0..=FORWARDED_KEY_PRESS_LIMIT {
            queue.push(
                &test_key_event(10 + index as u8, index as u32, 0x200000),
                true,
                start,
            );
        }
        assert_eq!(queue.take(&oldest, start), None);
        assert_eq!(
            queue.take(
                &test_key_event(
                    10 + FORWARDED_KEY_PRESS_LIMIT as u8,
                    FORWARDED_KEY_PRESS_LIMIT as u32,
                    0x200000,
                ),
                start,
            ),
            Some(true)
        );
    }

    /// Confirms a focus transition drops held keys so their missing releases cannot turn
    /// the next press into a phantom repeat.
    ///
    /// @author kongweiguang
    #[test]
    fn key_repeat_tracker_reset_forgets_focus_lost_keys() {
        let mut tracker = KeyRepeatTracker::new();

        assert!(!tracker.press(38));
        assert!(tracker.press(38));
        tracker.reset();
        assert!(!tracker.press(38));
    }
}
