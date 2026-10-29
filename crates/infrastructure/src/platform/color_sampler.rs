//! On-demand Windows desktop color sampling for the settings color picker.
//!
//! @author kongweiguang

use std::time::Duration;

#[cfg(windows)]
use std::{
    io,
    sync::atomic::{AtomicI32, AtomicU8, Ordering},
    time::Instant,
};

use async_trait::async_trait;
#[cfg(windows)]
use kerminal_application::preferences::color_sampler::{ColorSampleCancellation, SampledColor};
use kerminal_application::{
    AppError, AppResult,
    preferences::color_sampler::{ColorSampleOutcome, ScreenColorSampler},
};
use tokio_util::sync::CancellationToken;

/// Bounds a sampling request so a lost UI cancellation cannot leave a worker polling indefinitely.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(windows)]
const POLL_INTERVAL: Duration = Duration::from_millis(16);

#[cfg(windows)]
const HOOK_RELEASE_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[cfg(windows)]
const MAGNIFIER_WINDOW_SIZE: i32 = 184;

#[cfg(windows)]
const MAGNIFIER_MINIMUM_SIZE: i32 = 80;

#[cfg(windows)]
const MAGNIFIER_GAP: i32 = 24;

#[cfg(windows)]
const MAGNIFIER_SAMPLE_RADIUS: i32 = 7;

#[cfg(windows)]
const MAGNIFIER_IMAGE_ORIGIN: i32 = 12;

#[cfg(windows)]
static COLOR_SAMPLER_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(windows)]
static MOUSE_CAPTURE_PHASE: AtomicU8 = AtomicU8::new(MouseCapturePhase::Inactive as u8);

#[cfg(windows)]
static CAPTURED_CURSOR_X: AtomicI32 = AtomicI32::new(0);

#[cfg(windows)]
static CAPTURED_CURSOR_Y: AtomicI32 = AtomicI32::new(0);

#[cfg(windows)]
static MAGNIFIER_CURSOR_X: AtomicI32 = AtomicI32::new(0);

#[cfg(windows)]
static MAGNIFIER_CURSOR_Y: AtomicI32 = AtomicI32::new(0);

#[cfg(windows)]
static MAGNIFIER_CURRENT_SIZE: AtomicI32 = AtomicI32::new(0);

/// Platform adapter for explicit, on-demand desktop color sampling.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlatformScreenColorSampler;

#[async_trait]
impl ScreenColorSampler for PlatformScreenColorSampler {
    /// Keeps Win32 polling off the async executor and caps abandoned requests at thirty seconds.
    ///
    /// @author kongweiguang
    async fn sample(
        &self,
        timeout: Duration,
        cancellation: CancellationToken,
    ) -> AppResult<ColorSampleOutcome> {
        let timeout = bounded_sampling_timeout(timeout);

        #[cfg(windows)]
        {
            let started = tokio::time::Instant::now();
            let sampling_guard = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    return Ok(ColorSampleOutcome::Cancelled(
                        ColorSampleCancellation::RequestCancelled,
                    ));
                }
                _ = tokio::time::sleep(timeout) => {
                    return Ok(ColorSampleOutcome::Cancelled(ColorSampleCancellation::Timeout));
                }
                guard = COLOR_SAMPLER_LOCK.lock() => guard,
            };
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Ok(ColorSampleOutcome::Cancelled(
                    ColorSampleCancellation::Timeout,
                ));
            }
            let outcome = tokio::task::spawn_blocking(move || {
                let _sampling_guard = sampling_guard;
                sample_screen_color_blocking(remaining, cancellation)
            })
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("屏幕取色后台任务失败", source)
            })?;
            outcome.map_err(|source| AppError::infrastructure_with_source("屏幕取色失败", source))
        }

        #[cfg(not(windows))]
        {
            let _ = (timeout, cancellation);
            Err(AppError::unsupported("屏幕取色当前仅支持 Windows"))
        }
    }
}

/// Applies the sampler-wide maximum without changing shorter caller deadlines.
fn bounded_sampling_timeout(timeout: Duration) -> Duration {
    timeout.min(DEFAULT_TIMEOUT)
}

#[cfg(windows)]
/// Polls only the current input desktop and requires the trigger click to be released first.
///
/// @author kongweiguang
fn sample_screen_color_blocking(
    timeout: Duration,
    cancellation: CancellationToken,
) -> io::Result<ColorSampleOutcome> {
    use std::thread;

    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_ESCAPE, VK_LBUTTON,
    };

    if cancellation.is_cancelled() {
        return Ok(ColorSampleOutcome::Cancelled(
            ColorSampleCancellation::RequestCancelled,
        ));
    }
    let deadline = Instant::now() + timeout;
    if timeout.is_zero() {
        return Ok(ColorSampleOutcome::Cancelled(
            ColorSampleCancellation::Timeout,
        ));
    }
    ensure_input_desktop_matches_thread()?;

    MOUSE_CAPTURE_PHASE.store(
        MouseCapturePhase::AwaitingTriggerRelease as u8,
        Ordering::SeqCst,
    );
    let _phase_reset = MouseCapturePhaseReset;
    let class = MagnifierClass::register()?;
    let mut window = MagnifierWindow::create(&class)?;
    let _hook = LowLevelMouseHook::install(class.module)?;
    let left_button = i32::from(VK_LBUTTON);
    let escape_key = i32::from(VK_ESCAPE);
    if !key_is_down(unsafe { GetAsyncKeyState(left_button) }) {
        let _ = MOUSE_CAPTURE_PHASE.compare_exchange(
            MouseCapturePhase::AwaitingTriggerRelease as u8,
            MouseCapturePhase::Armed as u8,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    window.update(cursor_position()?)?;
    while Instant::now() < deadline {
        pump_thread_messages()?;
        if current_mouse_capture_phase() == MouseCapturePhase::Selected {
            ensure_input_desktop_matches_thread()?;
            window.hide()?;
            let point = windows_sys::Win32::Foundation::POINT {
                x: CAPTURED_CURSOR_X.load(Ordering::SeqCst),
                y: CAPTURED_CURSOR_Y.load(Ordering::SeqCst),
            };
            return ScreenDc::acquire()?
                .pixel(point)
                .map(ColorSampleOutcome::Selected);
        }
        if cancellation.is_cancelled() {
            return Ok(ColorSampleOutcome::Cancelled(
                ColorSampleCancellation::RequestCancelled,
            ));
        }
        if key_is_down(unsafe { GetAsyncKeyState(escape_key) }) {
            return Ok(ColorSampleOutcome::Cancelled(
                ColorSampleCancellation::Escape,
            ));
        }
        ensure_input_desktop_matches_thread()?;
        window.update(cursor_position()?)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(POLL_INTERVAL.min(remaining));
    }

    Ok(ColorSampleOutcome::Cancelled(
        ColorSampleCancellation::Timeout,
    ))
}

#[cfg(any(windows, test))]
/// Mouse phases ensure the eyedropper's initiating release passes through and only one later click is swallowed.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MouseCapturePhase {
    Inactive = 0,
    AwaitingTriggerRelease = 1,
    Armed = 2,
    AwaitingSelectionRelease = 3,
    Selected = 4,
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MouseCaptureInput {
    LeftButtonDown,
    LeftButtonUp,
    Other,
}

#[cfg(any(windows, test))]
/// Returns the next phase and whether this specific left-button event must be consumed.
fn transition_mouse_capture_phase(
    phase: MouseCapturePhase,
    input: MouseCaptureInput,
) -> (MouseCapturePhase, bool) {
    match (phase, input) {
        (MouseCapturePhase::AwaitingTriggerRelease, MouseCaptureInput::LeftButtonUp) => {
            (MouseCapturePhase::Armed, false)
        }
        (MouseCapturePhase::Armed, MouseCaptureInput::LeftButtonDown) => {
            (MouseCapturePhase::AwaitingSelectionRelease, true)
        }
        (MouseCapturePhase::AwaitingSelectionRelease, MouseCaptureInput::LeftButtonUp) => {
            (MouseCapturePhase::Selected, true)
        }
        _ => (phase, false),
    }
}

#[cfg(windows)]
/// Decodes the only mutable global phase while the active request owns the sampler lock.
fn current_mouse_capture_phase() -> MouseCapturePhase {
    match MOUSE_CAPTURE_PHASE.load(Ordering::SeqCst) {
        1 => MouseCapturePhase::AwaitingTriggerRelease,
        2 => MouseCapturePhase::Armed,
        3 => MouseCapturePhase::AwaitingSelectionRelease,
        4 => MouseCapturePhase::Selected,
        _ => MouseCapturePhase::Inactive,
    }
}

#[cfg(windows)]
/// Applies a left-button transition atomically and records the selected screen coordinate only once.
fn handle_hook_mouse_message(message: u32, point: windows_sys::Win32::Foundation::POINT) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{WM_LBUTTONDOWN, WM_LBUTTONUP};

    let input = match message {
        WM_LBUTTONDOWN => MouseCaptureInput::LeftButtonDown,
        WM_LBUTTONUP => MouseCaptureInput::LeftButtonUp,
        _ => MouseCaptureInput::Other,
    };
    loop {
        let current = MOUSE_CAPTURE_PHASE.load(Ordering::SeqCst);
        let phase = match current {
            1 => MouseCapturePhase::AwaitingTriggerRelease,
            2 => MouseCapturePhase::Armed,
            3 => MouseCapturePhase::AwaitingSelectionRelease,
            4 => MouseCapturePhase::Selected,
            _ => MouseCapturePhase::Inactive,
        };
        let (next, consume) = transition_mouse_capture_phase(phase, input);
        if next == phase {
            return false;
        }
        if MOUSE_CAPTURE_PHASE
            .compare_exchange(current, next as u8, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            if phase == MouseCapturePhase::Armed && input == MouseCaptureInput::LeftButtonDown {
                CAPTURED_CURSOR_X.store(point.x, Ordering::SeqCst);
                CAPTURED_CURSOR_Y.store(point.y, Ordering::SeqCst);
            }
            return consume;
        }
    }
}

#[cfg(windows)]
/// RAII cleanup resets process-local state after the hook and magnifier have been released.
struct MouseCapturePhaseReset;

#[cfg(windows)]
impl Drop for MouseCapturePhaseReset {
    /// Prevents stale capture state from affecting a later request after any exit path.
    fn drop(&mut self) {
        MOUSE_CAPTURE_PHASE.store(MouseCapturePhase::Inactive as u8, Ordering::SeqCst);
    }
}

#[cfg(windows)]
/// Owns the temporary low-level hook and drains a swallowed button-up before uninstalling it.
struct LowLevelMouseHook(windows_sys::Win32::UI::WindowsAndMessaging::HHOOK);

#[cfg(windows)]
impl LowLevelMouseHook {
    /// Installs a desktop-wide hook whose callback only handles left-button down and up.
    fn install(module: windows_sys::Win32::Foundation::HINSTANCE) -> io::Result<Self> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowsHookExW, WH_MOUSE_LL};

        let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(low_level_mouse_proc), module, 0) };
        if hook.is_null() {
            return Err(last_windows_error("SetWindowsHookExW failed"));
        }
        Ok(Self(hook))
    }
}

#[cfg(windows)]
impl Drop for LowLevelMouseHook {
    /// Keeps the hook alive briefly for a matching release, then always unhooks it.
    fn drop(&mut self) {
        use std::thread;

        use windows_sys::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx;

        let deadline = Instant::now() + HOOK_RELEASE_DRAIN_TIMEOUT;
        while current_mouse_capture_phase() == MouseCapturePhase::AwaitingSelectionRelease
            && Instant::now() < deadline
        {
            let _ = pump_thread_messages();
            thread::sleep(POLL_INTERVAL);
        }
        unsafe {
            UnhookWindowsHookEx(self.0);
        }
    }
}

#[cfg(windows)]
/// Consumes only the selected left-click pair; every other input event continues through the hook chain.
unsafe extern "system" fn low_level_mouse_proc(
    code: i32,
    message: windows_sys::Win32::Foundation::WPARAM,
    data: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, MSLLHOOKSTRUCT, WM_LBUTTONDOWN, WM_LBUTTONUP,
    };

    if code >= 0 && (message as u32 == WM_LBUTTONDOWN || message as u32 == WM_LBUTTONUP) {
        let event = unsafe { &*(data as *const MSLLHOOKSTRUCT) };
        if handle_hook_mouse_message(message as u32, event.pt) {
            return 1;
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, message, data) }
}

#[cfg(windows)]
/// Pumps the hook-owning worker thread so Windows can deliver low-level hook callbacks.
fn pump_thread_messages() -> io::Result<()> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage, WM_QUIT,
    };

    let mut message = MSG::default();
    while unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
        if message.message == WM_QUIT {
            return Err(io::Error::other(
                "color sampler message loop received WM_QUIT",
            ));
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}

#[cfg(windows)]
/// Registers a per-request window class so no magnifier class remains globally resident.
struct MagnifierClass {
    module: windows_sys::Win32::Foundation::HINSTANCE,
    name: Vec<u16>,
}

#[cfg(windows)]
impl MagnifierClass {
    /// Registers a no-background class used only after an explicit color-sampling request begins.
    fn register() -> io::Result<Self> {
        use std::mem::size_of;

        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CS_HREDRAW, CS_VREDRAW, RegisterClassExW, WNDCLASSEXW,
        };

        let module = unsafe { GetModuleHandleW(std::ptr::null()) };
        if module.is_null() {
            return Err(last_windows_error("GetModuleHandleW failed"));
        }
        let name = wide_null("Kerminal.SettingsColorSamplerMagnifier");
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(magnifier_window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: module,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: name.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        if unsafe { RegisterClassExW(&class) } == 0 {
            return Err(last_windows_error("RegisterClassExW failed"));
        }
        Ok(Self { module, name })
    }
}

#[cfg(windows)]
impl Drop for MagnifierClass {
    /// Unregisters the temporary class after the associated window is destroyed.
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::UnregisterClassW(
                self.name.as_ptr(),
                self.module,
            );
        }
    }
}

#[cfg(windows)]
/// Owns the non-activating magnifier window for one explicit sampling request.
struct MagnifierWindow {
    hwnd: windows_sys::Win32::Foundation::HWND,
    position: Option<(i32, i32, i32)>,
}

#[cfg(windows)]
impl MagnifierWindow {
    /// Creates a hidden click-through tool window on the current input desktop.
    fn create(class: &MagnifierClass) -> io::Result<Self> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
            WS_EX_TRANSPARENT, WS_POPUP,
        };

        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST
                    | WS_EX_TRANSPARENT,
                class.name.as_ptr(),
                std::ptr::null(),
                WS_POPUP,
                0,
                0,
                MAGNIFIER_WINDOW_SIZE,
                MAGNIFIER_WINDOW_SIZE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                class.module,
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err(last_windows_error("CreateWindowExW failed"));
        }
        let window = Self {
            hwnd,
            position: None,
        };
        window.set_layered_opaque()?;
        Ok(window)
    }

    /// Keeps the click-through layered window opaque enough to read magnified pixel colors.
    fn set_layered_opaque(&self) -> io::Result<()> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{LWA_ALPHA, SetLayeredWindowAttributes};

        if unsafe { SetLayeredWindowAttributes(self.hwnd, 0, u8::MAX, LWA_ALPHA) } == 0 {
            return Err(last_windows_error("SetLayeredWindowAttributes failed"));
        }
        Ok(())
    }

    /// Shows without activation, positions away from the cursor, and refreshes the zoomed desktop crop.
    fn update(&mut self, cursor: windows_sys::Win32::Foundation::POINT) -> io::Result<()> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_SHOWWINDOW, SetWindowPos,
        };

        MAGNIFIER_CURSOR_X.store(cursor.x, Ordering::SeqCst);
        MAGNIFIER_CURSOR_Y.store(cursor.y, Ordering::SeqCst);
        let Some((x, y, size)) = magnifier_placement(cursor) else {
            MAGNIFIER_CURRENT_SIZE.store(0, Ordering::SeqCst);
            if self.position.is_some() {
                self.hide()?;
                self.position = None;
            }
            return Ok(());
        };
        MAGNIFIER_CURRENT_SIZE.store(size, Ordering::SeqCst);
        if self.position != Some((x, y, size)) {
            if unsafe {
                SetWindowPos(
                    self.hwnd,
                    HWND_TOPMOST,
                    x,
                    y,
                    size,
                    size,
                    SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
                )
            } == 0
            {
                return Err(last_windows_error("SetWindowPos failed"));
            }
            self.position = Some((x, y, size));
        }
        let dc = unsafe { windows_sys::Win32::Graphics::Gdi::GetDC(self.hwnd) };
        if dc.is_null() {
            return Err(last_windows_error("GetDC for magnifier failed"));
        }
        let target = WindowDc {
            hwnd: self.hwnd,
            dc,
        };
        draw_magnifier_contents(target.dc, cursor, size)
    }

    /// Hides the lens before reading the selected screen pixel to avoid capturing its own content.
    fn hide(&self) -> io::Result<()> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE,
            SWP_NOZORDER, SetWindowPos,
        };

        if unsafe {
            SetWindowPos(
                self.hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_HIDEWINDOW
                    | SWP_NOACTIVATE
                    | SWP_NOMOVE
                    | SWP_NOSIZE
                    | SWP_NOZORDER
                    | SWP_NOOWNERZORDER,
            )
        } == 0
        {
            return Err(last_windows_error("SetWindowPos hide failed"));
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for MagnifierWindow {
    /// Destroys the per-request overlay on its creating worker thread.
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(self.hwnd);
        }
    }
}

#[cfg(windows)]
/// Selects the largest safe quadrant so the zoom source cannot include the magnifier itself.
fn magnifier_placement(cursor: windows_sys::Win32::Foundation::POINT) -> Option<(i32, i32, i32)> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let right = left.saturating_add(unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) });
    let bottom = top.saturating_add(unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) });
    let safe_gap = MAGNIFIER_GAP + MAGNIFIER_SAMPLE_RADIUS + 1;
    let right_space = right
        .saturating_sub(cursor.x.saturating_add(safe_gap))
        .max(0);
    let left_space = cursor
        .x
        .saturating_sub(safe_gap)
        .saturating_sub(left)
        .max(0);
    let below_space = bottom
        .saturating_sub(cursor.y.saturating_add(safe_gap))
        .max(0);
    let above_space = cursor.y.saturating_sub(safe_gap).saturating_sub(top).max(0);

    let mut best = None;
    for (x, width, right_of_cursor) in [
        (cursor.x.saturating_add(safe_gap), right_space, true),
        (cursor.x.saturating_sub(safe_gap), left_space, false),
    ] {
        for (y, height, below_cursor) in [
            (cursor.y.saturating_add(safe_gap), below_space, true),
            (cursor.y.saturating_sub(safe_gap), above_space, false),
        ] {
            let size = width.min(height).min(MAGNIFIER_WINDOW_SIZE);
            if size < MAGNIFIER_MINIMUM_SIZE {
                continue;
            }
            let x = if right_of_cursor {
                x
            } else {
                x.saturating_sub(size)
            };
            let y = if below_cursor {
                y
            } else {
                y.saturating_sub(size)
            };
            if best.is_none_or(|(_, _, best_size)| size > best_size) {
                best = Some((x, y, size));
            }
        }
    }
    best
}

#[cfg(windows)]
/// Draws a nearest-neighbor desktop crop and a centered marker around the sample pixel.
fn draw_magnifier_contents(
    target: windows_sys::Win32::Graphics::Gdi::HDC,
    cursor: windows_sys::Win32::Foundation::POINT,
    window_size: i32,
) -> io::Result<()> {
    use windows_sys::Win32::Graphics::Gdi::{
        COLORONCOLOR, CreatePen, LineTo, MoveToEx, PS_SOLID, SRCCOPY, SetStretchBltMode, StretchBlt,
    };

    let source = ScreenDc::acquire()?;
    let image_size = window_size - MAGNIFIER_IMAGE_ORIGIN * 2;
    if unsafe { SetStretchBltMode(target, COLORONCOLOR) } == 0
        || unsafe {
            StretchBlt(
                target,
                MAGNIFIER_IMAGE_ORIGIN,
                MAGNIFIER_IMAGE_ORIGIN,
                image_size,
                image_size,
                source.0,
                cursor.x - MAGNIFIER_SAMPLE_RADIUS,
                cursor.y - MAGNIFIER_SAMPLE_RADIUS,
                MAGNIFIER_SAMPLE_RADIUS * 2 + 1,
                MAGNIFIER_SAMPLE_RADIUS * 2 + 1,
                SRCCOPY,
            )
        } == 0
    {
        return Err(last_windows_error("StretchBlt magnifier capture failed"));
    }

    let grid_pen = unsafe { CreatePen(PS_SOLID, 1, 0x00606060) };
    if grid_pen.is_null() {
        return Err(last_windows_error("CreatePen for magnifier grid failed"));
    }
    let grid = SelectedPen::select(target, grid_pen)?;
    for index in 1..(MAGNIFIER_SAMPLE_RADIUS * 2 + 1) {
        let offset =
            MAGNIFIER_IMAGE_ORIGIN + index * image_size / (MAGNIFIER_SAMPLE_RADIUS * 2 + 1);
        unsafe {
            MoveToEx(target, offset, MAGNIFIER_IMAGE_ORIGIN, std::ptr::null_mut());
            LineTo(target, offset, MAGNIFIER_IMAGE_ORIGIN + image_size);
            MoveToEx(target, MAGNIFIER_IMAGE_ORIGIN, offset, std::ptr::null_mut());
            LineTo(target, MAGNIFIER_IMAGE_ORIGIN + image_size, offset);
        }
    }
    drop(grid);

    let center_start = MAGNIFIER_IMAGE_ORIGIN
        + MAGNIFIER_SAMPLE_RADIUS * image_size / (MAGNIFIER_SAMPLE_RADIUS * 2 + 1);
    let center_end = MAGNIFIER_IMAGE_ORIGIN
        + (MAGNIFIER_SAMPLE_RADIUS + 1) * image_size / (MAGNIFIER_SAMPLE_RADIUS * 2 + 1);
    let marker_pen = unsafe { CreatePen(PS_SOLID, 2, 0x00FFFFFF) };
    if marker_pen.is_null() {
        return Err(last_windows_error("CreatePen for magnifier marker failed"));
    }
    let marker = SelectedPen::select(target, marker_pen)?;
    unsafe {
        MoveToEx(target, center_start, center_start, std::ptr::null_mut());
        LineTo(target, center_end, center_start);
        LineTo(target, center_end, center_end);
        LineTo(target, center_start, center_end);
        LineTo(target, center_start, center_start);
    }
    drop(marker);
    Ok(())
}

#[cfg(windows)]
/// Restores the previous GDI selection and deletes the request-local drawing pen.
struct SelectedPen {
    dc: windows_sys::Win32::Graphics::Gdi::HDC,
    pen: windows_sys::Win32::Graphics::Gdi::HPEN,
    previous: windows_sys::Win32::Graphics::Gdi::HGDIOBJ,
}

#[cfg(windows)]
impl SelectedPen {
    /// Selects a new pen and rejects GDI's invalid-object sentinel before drawing.
    fn select(
        dc: windows_sys::Win32::Graphics::Gdi::HDC,
        pen: windows_sys::Win32::Graphics::Gdi::HPEN,
    ) -> io::Result<Self> {
        use windows_sys::Win32::Graphics::Gdi::SelectObject;

        let previous = unsafe { SelectObject(dc, pen) };
        if previous.is_null() || previous == -1isize as _ {
            unsafe {
                windows_sys::Win32::Graphics::Gdi::DeleteObject(pen);
            }
            return Err(last_windows_error("SelectObject for magnifier pen failed"));
        }
        Ok(Self { dc, pen, previous })
    }
}

#[cfg(windows)]
impl Drop for SelectedPen {
    /// Restores GDI state before releasing the temporary pen.
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Graphics::Gdi::SelectObject(self.dc, self.previous);
            windows_sys::Win32::Graphics::Gdi::DeleteObject(self.pen);
        }
    }
}

#[cfg(windows)]
/// Owns a window DC acquired from the magnifier.
struct WindowDc {
    hwnd: windows_sys::Win32::Foundation::HWND,
    dc: windows_sys::Win32::Graphics::Gdi::HDC,
}

#[cfg(windows)]
impl Drop for WindowDc {
    /// Releases the window DC on the same worker thread that acquired it.
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Graphics::Gdi::ReleaseDC(self.hwnd, self.dc);
        }
    }
}

#[cfg(windows)]
/// Handles paint and hit-test messages without activating the magnifier or taking pointer input.
unsafe extern "system" fn magnifier_window_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    message: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, HTTRANSPARENT, MA_NOACTIVATE, WM_ERASEBKGND, WM_MOUSEACTIVATE,
        WM_NCHITTEST, WM_PAINT,
    };

    match message {
        WM_ERASEBKGND => 1,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
        WM_NCHITTEST => HTTRANSPARENT as isize,
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let dc = unsafe { BeginPaint(hwnd, &mut paint) };
            if !dc.is_null() {
                let point = windows_sys::Win32::Foundation::POINT {
                    x: MAGNIFIER_CURSOR_X.load(Ordering::SeqCst),
                    y: MAGNIFIER_CURSOR_Y.load(Ordering::SeqCst),
                };
                let size = MAGNIFIER_CURRENT_SIZE.load(Ordering::SeqCst);
                if size >= MAGNIFIER_MINIMUM_SIZE {
                    let _ = draw_magnifier_contents(dc, point, size);
                }
            }
            unsafe {
                EndPaint(hwnd, &paint);
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

#[cfg(windows)]
/// Converts a static window-class name to the terminated encoding required by Win32.
fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
/// Checks the high-order key-state bit; the low-order "pressed since last query" bit is unreliable.
///
/// @author kongweiguang
fn key_is_down(state: i16) -> bool {
    (state as u16 & 0x8000) != 0
}

#[cfg(windows)]
/// Refuses sampling when this worker belongs to a hidden/private desktop instead of the input desktop.
///
/// @author kongweiguang
fn ensure_input_desktop_matches_thread() -> io::Result<()> {
    use windows_sys::Win32::System::{StationsAndDesktops::*, Threading::GetCurrentThreadId};

    let thread_desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
    if thread_desktop.is_null() {
        return Err(last_windows_error("GetThreadDesktop failed"));
    }

    let input_desktop = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
    if input_desktop.is_null() {
        return Err(last_windows_error("OpenInputDesktop failed"));
    }
    let input_desktop = InputDesktopGuard(input_desktop);
    let thread_name = user_object_name(thread_desktop)?;
    let input_name = user_object_name(input_desktop.0)?;
    if !thread_name.eq_ignore_ascii_case(&input_name) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "screen sampling is unavailable outside the current input desktop",
        ));
    }
    Ok(())
}

#[cfg(windows)]
/// Reads one bounded Win32 user-object name for desktop identity comparison.
///
/// @author kongweiguang
fn user_object_name(handle: windows_sys::Win32::Foundation::HANDLE) -> io::Result<String> {
    use std::{mem::size_of, ptr};

    use windows_sys::Win32::System::StationsAndDesktops::{GetUserObjectInformationW, UOI_NAME};

    const MAX_NAME_BYTES: u32 = 4096;
    let mut required_bytes = 0u32;
    unsafe {
        GetUserObjectInformationW(handle, UOI_NAME, ptr::null_mut(), 0, &mut required_bytes);
    }
    if required_bytes == 0 || required_bytes > MAX_NAME_BYTES {
        return Err(last_windows_error(
            "GetUserObjectInformationW returned an invalid name length",
        ));
    }
    let unit_count = usize::try_from(required_bytes / size_of::<u16>() as u32)
        .map_err(|_| io::Error::other("desktop name length conversion failed"))?;
    if !required_bytes.is_multiple_of(size_of::<u16>() as u32) || unit_count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "desktop name length is not valid UTF-16 data",
        ));
    }

    let mut buffer = vec![0u16; unit_count];
    if unsafe {
        GetUserObjectInformationW(
            handle,
            UOI_NAME,
            buffer.as_mut_ptr().cast(),
            required_bytes,
            &mut required_bytes,
        )
    } == 0
    {
        return Err(last_windows_error("GetUserObjectInformationW failed"));
    }
    let end = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16(&buffer[..end]).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "desktop name is not valid UTF-16",
        )
    })
}

#[cfg(windows)]
/// Captures the current cursor position in virtual-desktop coordinates.
///
/// @author kongweiguang
fn cursor_position() -> io::Result<windows_sys::Win32::Foundation::POINT> {
    use windows_sys::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};

    let mut point = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut point) } == 0 {
        return Err(last_windows_error("GetCursorPos failed"));
    }
    Ok(point)
}

#[cfg(windows)]
/// Owns a short-lived input-desktop handle and always closes it after identity checks.
struct InputDesktopGuard(windows_sys::Win32::System::StationsAndDesktops::HDESK);

#[cfg(windows)]
impl Drop for InputDesktopGuard {
    /// Releases the handle returned by OpenInputDesktop on every exit path.
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::StationsAndDesktops::CloseDesktop(self.0);
        }
    }
}

#[cfg(windows)]
/// Owns the screen DC only while one selected pixel is read.
struct ScreenDc(windows_sys::Win32::Graphics::Gdi::HDC);

#[cfg(windows)]
impl ScreenDc {
    /// Acquires the current desktop's DC without creating or activating a window.
    ///
    /// @author kongweiguang
    fn acquire() -> io::Result<Self> {
        use windows_sys::Win32::Graphics::Gdi::GetDC;

        let dc = unsafe { GetDC(std::ptr::null_mut()) };
        if dc.is_null() {
            return Err(last_windows_error("GetDC failed"));
        }
        Ok(Self(dc))
    }

    /// Reads one `COLORREF` and converts Win32's 0x00BBGGRR layout into RGB channels.
    ///
    /// @author kongweiguang
    fn pixel(&self, point: windows_sys::Win32::Foundation::POINT) -> io::Result<SampledColor> {
        use windows_sys::Win32::Graphics::Gdi::{CLR_INVALID, GetPixel};

        let color = unsafe { GetPixel(self.0, point.x, point.y) };
        if color == CLR_INVALID {
            return Err(last_windows_error("GetPixel returned CLR_INVALID"));
        }
        Ok(SampledColor::new(
            color as u8,
            (color >> 8) as u8,
            (color >> 16) as u8,
        ))
    }
}

#[cfg(windows)]
impl Drop for ScreenDc {
    /// Releases the GDI screen DC on the same worker thread that acquired it.
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Graphics::Gdi::ReleaseDC(std::ptr::null_mut(), self.0);
        }
    }
}

#[cfg(windows)]
/// Keeps the failing Win32 operation in the diagnostic while retaining the OS error description.
///
/// @author kongweiguang
fn last_windows_error(operation: &str) -> io::Error {
    io::Error::other(format!("{operation}: {}", io::Error::last_os_error()))
}

#[cfg(test)]
#[path = "../../tests/unit/platform_color_sampler.rs"]
mod tests;
