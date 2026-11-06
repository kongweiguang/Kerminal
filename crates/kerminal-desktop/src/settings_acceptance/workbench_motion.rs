//! 工作台侧栏短过渡的原生 present 采样，不将离屏绘制或 PTY 耗时称为帧延迟。
//!
//! @author kongweiguang

use super::*;

#[cfg(feature = "acceptance-metrics")]
#[path = "workbench_motion_intervals.rs"]
mod intervals;

const MOTION_ACCEPTANCE_ENV: &str = "KERMINAL_WORKBENCH_MOTION_ACCEPTANCE";

/// 仅在显式启用时运行耗时的动效专项，普通工作台验收不改变采样条件。
///
/// @author kongweiguang
pub(super) fn requested() -> bool {
    std::env::var_os(MOTION_ACCEPTANCE_ENV).as_deref() == Some(std::ffi::OsStr::new("1"))
}

#[cfg(feature = "acceptance-metrics")]
const TOGGLE_CYCLES: usize = 20;
#[cfg(feature = "acceptance-metrics")]
const WARMUP_TOGGLES: usize = 4;
#[cfg(feature = "acceptance-metrics")]
const MIN_ANIMATION_SAMPLES: u64 = 100;
#[cfg(feature = "acceptance-metrics")]
const SETTLE_DURATION: Duration = Duration::from_millis(240);
#[cfg(feature = "acceptance-metrics")]
const TARGET_OBSERVATION_INTERVAL: Duration = Duration::from_millis(8);

#[cfg(feature = "acceptance-metrics")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MotionReport {
    schema_version: u32,
    capture_mode: &'static str,
    measurement_boundary: &'static str,
    native_isolation: crate::right_rail_acceptance::isolation::DesktopIsolationEvidence,
    motion_window_handle: usize,
    native_window_visible: bool,
    native_focus_window_handle: Option<usize>,
    config_directory: String,
    window_visible: bool,
    window_active: bool,
    window_width: f32,
    window_height: f32,
    scale_factor: f32,
    ui_font_family: String,
    ui_font_size: f32,
    terminal_font_preference: String,
    terminal_font_size: u16,
    terminal_line_height: f64,
    refresh_interval_ns: Option<u64>,
    target_p95_ns: Option<u64>,
    warmup_toggles: usize,
    toggle_cycles: usize,
    completed_transitions: usize,
    initial_sidebar_visible: bool,
    final_sidebar_visible: bool,
    session_and_pane_preserved: bool,
    minimum_animation_samples: u64,
    status: &'static str,
    input_source: &'static str,
    gpui_input: kerminal_ui::acceptance_metrics::WindowInputMetrics,
    animation_intervals: intervals::MotionIntervalReport,
    frame: kerminal_ui::acceptance_metrics::WindowFrameMetrics,
}

/// 同时记录 HWND 的显示标志和 UI 线程焦点，避免仅用 GPUI 缓存状态推断原生窗口可见。
#[cfg(feature = "acceptance-metrics")]
struct MotionNativeWindowState {
    visible: bool,
    iconic: bool,
    focus_window_handle: Option<usize>,
    owner_process_id: Option<u32>,
    owner_thread_id: Option<u32>,
}

/// 复用生产按钮和真实平台帧回调；输入与帧共用预热后的区间，不把程序分发事件称为物理输入。
/// 自动采样不切换用户桌面，也不强行制造待测帧。
///
/// @author kongweiguang
#[cfg(feature = "acceptance-metrics")]
pub(super) async fn capture(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &WorkbenchContext,
    initial_session: WorkbenchState,
) -> Result<(), Box<dyn Error>> {
    use kerminal_ui::acceptance_metrics::{
        FrameMetricsEnvironment, RendererKind, capture_window_frame_metrics,
        summarize_input_metrics,
    };

    if context.width < 1280 {
        return Err(io::Error::other("motion acceptance requires a non-compact workbench").into());
    }
    let terminal_settings = load_persisted_settings(services).await?.settings.terminal;
    let motion_window = show_private_motion_window(cx, window).await?;
    let initial_sidebar_visible = wait_initial_sidebar(cx, window, motion_window).await?;
    let mut expected_visible = initial_sidebar_visible;
    for _ in 0..WARMUP_TOGGLES {
        toggle_sidebar(cx, window, motion_window)?;
        expected_visible = !expected_visible;
        cx.background_executor().timer(SETTLE_DURATION).await;
        if sidebar_visible(cx, window, motion_window)? != expected_visible {
            return Err(
                io::Error::other("sidebar warmup did not reach the requested state").into(),
            );
        }
    }

    let (baseline, input_baseline, refresh_interval_ns, mut timeline, baseline_environment) =
        window.update(cx, |_, window, app| {
            ensure_motion_window(window, motion_window)?;
            let origin = Instant::now();
            Ok::<_, io::Error>((
                window.frame_duration_snapshot(),
                window.input_latency_snapshot(),
                window
                    .refresh_interval(app)
                    .and_then(|interval| u64::try_from(interval.as_nanos()).ok()),
                intervals::MotionTimeline::new(window, app, origin),
                intervals::MotionEnvironment::capture(window, app),
            ))
        })??;

    let mut completed_transitions = 0;
    let mut toggle_observations = Vec::with_capacity(TOGGLE_CYCLES * 2);
    for _ in 0..TOGGLE_CYCLES * 2 {
        let (requested_at, before) = toggle_sidebar(cx, window, motion_window)?;
        expected_visible = !expected_visible;
        let mut observation =
            intervals::ToggleObservation::new(requested_at, expected_visible, before);
        let settle_deadline = Instant::now() + SETTLE_DURATION;
        loop {
            let now = Instant::now();
            let remaining = settle_deadline.saturating_duration_since(now);
            if remaining.is_zero() {
                break;
            }
            let observing = !observation.observed()
                && now.saturating_duration_since(requested_at) < intervals::MOTION_DURATION;
            let delay = if observing {
                remaining.min(TARGET_OBSERVATION_INTERVAL)
            } else {
                remaining
            };
            cx.background_executor().timer(delay).await;
            if observing {
                let observed = window.update(cx, |_, window, app| {
                    timeline.drain();
                    let visible = selector_bounds(window, "host-navigation-refresh").is_ok();
                    if visible == expected_visible && timeline.has_drawn_present_since(requested_at)
                    {
                        let environment = intervals::MotionEnvironment::capture(window, app);
                        Some((Instant::now(), environment))
                    } else {
                        None
                    }
                })?;
                if let Some((at, environment)) = observed {
                    observation.observe(at, environment);
                }
            }
        }
        let (visible, settled_at, settled_environment) =
            window.update(cx, |_, window, app| {
                ensure_motion_window(window, motion_window)?;
                timeline.drain();
                let visible = selector_bounds(window, "host-navigation-refresh").is_ok();
                let environment = intervals::MotionEnvironment::capture(window, app);
                Ok::<_, io::Error>((visible, Instant::now(), environment))
            })??;
        observation.settle(settled_at, settled_environment);
        if visible != expected_visible {
            return Err(io::Error::other(format!(
                "sidebar transition {} did not reach the requested state",
                completed_transitions + 1
            ))
            .into());
        }
        toggle_observations.push(observation);
        completed_transitions += 1;
    }

    let (
        frame,
        gpui_input,
        window_visible,
        window_active,
        window_width,
        window_height,
        scale_factor,
        ui_font_family,
        ui_font_size,
        native_isolation,
        native_window,
    ) = window.update(cx, |_, window, app| {
        let (native_isolation, native_window) = ensure_motion_window(window, motion_window)?;
        timeline.drain();
        let environment = FrameMetricsEnvironment::new(
            "isolated-native-desktop",
            "workbench-sidebar-motion",
            RendererKind::Unknown,
        );
        let frame = capture_window_frame_metrics(window, Some(&baseline), environment)
            .map_err(io::Error::other)?;
        let gpui_input =
            summarize_input_metrics(&window.input_latency_snapshot(), Some(&input_baseline))
                .map_err(io::Error::other)?;
        let bounds = window.bounds();
        Ok::<_, io::Error>((
            frame,
            gpui_input,
            window.is_visible(),
            window.is_window_active(),
            bounds.size.width.as_f32(),
            bounds.size.height.as_f32(),
            window.scale_factor(),
            gpui_kit::component::Theme::global(app)
                .font_family
                .to_string(),
            gpui_kit::component::Theme::global(app).font_size.as_f32(),
            native_isolation,
            native_window,
        ))
    })??;
    let final_sidebar_visible = sidebar_visible(cx, window, motion_window)?;
    let session_and_pane_preserved = window.update(cx, |_, window, app| {
        ensure_motion_window(window, motion_window)?;
        let root = root.read(app);
        let (tabs, panes, sessions, _) = root.workbench_acceptance_local_state();
        Ok::<_, io::Error>(
            initial_session.ready_session_ids == sessions
                && initial_session.local_tab_count == tabs
                && initial_session.local_pane_count == panes
                && initial_session.active_local_pane_id
                    == root.workbench_acceptance_active_local_pane_id(),
        )
    })??;
    let target_p95_ns = refresh_interval_ns.map(|interval| interval.saturating_add(interval / 2));
    let animation = &frame.gpui_animated_present_interval;
    let status = if !window_active || !session_and_pane_preserved {
        "state_changed"
    } else if animation.sample_count < MIN_ANIMATION_SAMPLES || animation.p95.is_none() {
        "insufficient_samples"
    } else if target_p95_ns.is_none() {
        "unknown_refresh_interval"
    } else if animation.p95 > target_p95_ns {
        "target_missed"
    } else {
        "passed"
    };
    let animation_intervals = timeline.finish(
        toggle_observations,
        baseline_environment,
        TOGGLE_CYCLES * 2,
        session_and_pane_preserved && final_sidebar_visible == initial_sidebar_visible,
        animation.sample_count,
        MIN_ANIMATION_SAMPLES,
        target_p95_ns,
    );
    let report = MotionReport {
        schema_version: 1,
        capture_mode: "private-visible-native-window",
        measurement_boundary: "gpui-platform-present-submission-not-screen-scanout",
        native_isolation,
        motion_window_handle: motion_window,
        native_window_visible: native_window.visible,
        native_focus_window_handle: native_window.focus_window_handle,
        config_directory: context.config_directory.to_string_lossy().into_owned(),
        window_visible,
        window_active,
        window_width,
        window_height,
        scale_factor,
        ui_font_family,
        ui_font_size,
        terminal_font_preference: terminal_settings.font_family,
        terminal_font_size: terminal_settings.font_size,
        terminal_line_height: terminal_settings.line_height,
        refresh_interval_ns,
        target_p95_ns,
        warmup_toggles: WARMUP_TOGGLES,
        toggle_cycles: TOGGLE_CYCLES,
        completed_transitions,
        initial_sidebar_visible,
        final_sidebar_visible,
        session_and_pane_preserved,
        minimum_animation_samples: MIN_ANIMATION_SAMPLES,
        status,
        input_source: "in-process-gpui-dispatched-pointer-events",
        gpui_input,
        animation_intervals,
        frame,
    };
    fs::write(
        context
            .output_directory
            .join("workbench-motion-report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    if status != "passed" || final_sidebar_visible != initial_sidebar_visible {
        return Err(io::Error::other(format!("workbench motion acceptance: {status}")).into());
    }
    Ok(())
}

/// 在已有生产帧中分发点击并记录其前置时间，保留真实动画起点的保守下界。
///
/// @author kongweiguang
#[cfg(feature = "acceptance-metrics")]
fn toggle_sidebar(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    motion_window: usize,
) -> Result<(Instant, intervals::MotionEnvironment), Box<dyn Error>> {
    Ok(handle.update(cx, |_, window, app| {
        ensure_motion_window(window, motion_window)?;
        let bounds = selector_bounds(window, "workbench-toggle-host-sidebar")?;
        let environment = intervals::MotionEnvironment::capture(window, app);
        let requested_at = Instant::now();
        click_bounds(window, app, bounds, "workbench-toggle-host-sidebar")?;
        Ok::<_, io::Error>((requested_at, environment))
    })??)
}

/// 从最后一个真实呈现的界面读取侧栏归属，并拒绝把失焦窗口的稀疏帧混入性能样本。
///
/// @author kongweiguang
#[cfg(feature = "acceptance-metrics")]
fn sidebar_visible(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    motion_window: usize,
) -> Result<bool, Box<dyn Error>> {
    Ok(handle.update(cx, |_, window, _| {
        ensure_motion_window(window, motion_window)?;
        Ok::<_, io::Error>(selector_bounds(window, "host-navigation-refresh").is_ok())
    })??)
}

/// 必须先观察到真实侧栏内的已注册按钮，再把其卸载视作关闭，避免未注册的容器 ID 始终返回假值。
///
/// @author kongweiguang
#[cfg(feature = "acceptance-metrics")]
async fn wait_initial_sidebar(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    motion_window: usize,
) -> Result<bool, Box<dyn Error>> {
    let deadline = Instant::now() + WORKBENCH_ACTION_TIMEOUT;
    loop {
        if sidebar_visible(cx, window, motion_window)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("motion sidebar refresh button never appeared").into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 仅对已证明归属的私有窗口执行真实显示，让 GPUI 通过系统可见性事件更新缓存。
/// 平台的 SetWindowPos 显示路径在此验收环境中未同步缓存，不能把原生标志冒充 GPUI 状态。
///
/// @author kongweiguang
#[cfg(all(windows, feature = "acceptance-metrics"))]
async fn show_private_motion_window(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<usize, Box<dyn Error>> {
    use windows_sys::Win32::{
        Foundation::{GetLastError, SetLastError},
        UI::{
            Input::KeyboardAndMouse::{SetActiveWindow, SetFocus},
            WindowsAndMessaging::{SW_SHOWNA, ShowWindow},
        },
    };

    let (hwnd, activation_attempt) = window.update(cx, |_, _, _| {
        let evidence = crate::right_rail_acceptance::isolation::capture()?;
        evidence.validate_motion_desktop()?;
        let hwnd = owned_motion_window(&evidence)?;
        let native_window = native_motion_window_state(hwnd);
        if native_window.visible || native_window.iconic {
            return Err(io::Error::other(
                "motion window was visible or minimized before explicit private-window show",
            ));
        }
        // SAFETY: HWND 已核对本进程、当前 UI 线程和非输入桌面；显示不会切换用户桌面或注入输入。
        let activation_attempt = unsafe {
            // ShowWindow 的返回值是之前的可见状态，实际显示结果由后续双重门禁核对。
            let previously_visible = ShowWindow(hwnd as _, SW_SHOWNA) != 0;
            SetLastError(0);
            let previous_active = SetActiveWindow(hwnd as _);
            let active_last_error = GetLastError();
            SetLastError(0);
            let previous_focus = SetFocus(hwnd as _);
            let focus_last_error = GetLastError();
            (
                previously_visible,
                (!previous_active.is_null()).then_some(previous_active as usize),
                active_last_error,
                (!previous_focus.is_null()).then_some(previous_focus as usize),
                focus_last_error,
            )
        };
        Ok::<_, io::Error>((hwnd, activation_attempt))
    })??;
    let deadline = Instant::now() + WORKBENCH_ACTION_TIMEOUT;
    loop {
        let (gpui_visible, gpui_active, native_window, evidence) =
            window.update(cx, |_, window, _| {
                let evidence = crate::right_rail_acceptance::isolation::capture()?;
                evidence.validate_motion_desktop()?;
                if owned_motion_window(&evidence)? != hwnd {
                    return Err(io::Error::other("motion window identity changed"));
                }
                let native_window = native_motion_window_state(hwnd);
                Ok::<_, io::Error>((
                    window.is_visible(),
                    window.is_window_active(),
                    native_window,
                    evidence,
                ))
            })??;
        if gpui_visible
            && gpui_active
            && evidence.get_active_window_handle == Some(hwnd)
            && native_window.visible
            && native_window.focus_window_handle == Some(hwnd)
        {
            return Ok(hwnd);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "private motion window did not become visible, active, and focused: \
                 hwnd={hwnd}, gpui_visible={gpui_visible}, gpui_active={gpui_active}, \
                 native_visible={}, native_iconic={}, native_focus={:?}, \
                 native_owner_process={:?}, native_owner_thread={:?}, \
                 activation_attempt=(previously_visible, previous_active, active_last_error, previous_focus, focus_last_error) \
                 {activation_attempt:?}, desktop={evidence:?}",
                native_window.visible,
                native_window.iconic,
                native_window.focus_window_handle,
                native_window.owner_process_id,
                native_window.owner_thread_id,
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 只查询 HWND 的显示、最小化、归属和调用线程焦点，区分原生状态与 GPUI 延迟更新。
///
/// @author kongweiguang
#[cfg(all(windows, feature = "acceptance-metrics"))]
fn native_motion_window_state(hwnd: usize) -> MotionNativeWindowState {
    use windows_sys::Win32::UI::{
        Input::KeyboardAndMouse::GetFocus,
        WindowsAndMessaging::{GetWindowThreadProcessId, IsIconic, IsWindowVisible},
    };

    // SAFETY: HWND 已由调用方核对归属；API 只读取状态并写入本地 PID 槽。
    let mut process_id = 0;
    let (visible, iconic, focus, thread_id) = unsafe {
        (
            IsWindowVisible(hwnd as _) != 0,
            IsIconic(hwnd as _) != 0,
            GetFocus(),
            GetWindowThreadProcessId(hwnd as _, &mut process_id),
        )
    };
    MotionNativeWindowState {
        visible,
        iconic,
        focus_window_handle: (!focus.is_null()).then_some(focus as usize),
        owner_process_id: (process_id != 0).then_some(process_id),
        owner_thread_id: (thread_id != 0).then_some(thread_id),
    }
}

/// 对固定标题反查 HWND，并逐次核对当前进程与 GPUI UI 线程，不能对其他窗口设置焦点。
///
/// @author kongweiguang
#[cfg(all(windows, feature = "acceptance-metrics"))]
fn owned_motion_window(
    evidence: &crate::right_rail_acceptance::isolation::DesktopIsolationEvidence,
) -> Result<usize, io::Error> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowThreadProcessId};

    let title: Vec<u16> = "Kerminal Workbench Acceptance"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: 标题以 NUL 结尾，Win32 只读取调用期间有效的字符串与 PID 输出指针。
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if hwnd.is_null() {
        return Err(io::Error::other("private motion window was not found"));
    }
    let mut process_id = 0;
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, &mut process_id) };
    if evidence.process_id != Some(process_id) || evidence.ui_thread_id != Some(thread_id) {
        return Err(io::Error::other(format!(
            "private motion window owner changed: hwnd={}, owner_process={process_id}, \
             owner_thread={thread_id}, desktop={evidence:?}",
            hwnd as usize,
        )));
    }
    Ok(hwnd as usize)
}

/// 每次输入和读帧都重新核对桌面、窗口归属与焦点，失去隔离后立即终止采样。
///
/// @author kongweiguang
#[cfg(all(windows, feature = "acceptance-metrics"))]
fn ensure_motion_window(
    window: &gpui_pre::Window,
    expected_hwnd: usize,
) -> Result<
    (
        crate::right_rail_acceptance::isolation::DesktopIsolationEvidence,
        MotionNativeWindowState,
    ),
    io::Error,
> {
    let evidence = crate::right_rail_acceptance::isolation::capture()?;
    if owned_motion_window(&evidence)? != expected_hwnd {
        return Err(io::Error::other("private motion window identity changed"));
    }
    evidence.validate_motion_window(
        window.is_visible(),
        window.is_window_active(),
        expected_hwnd,
    )?;
    let native_window = native_motion_window_state(expected_hwnd);
    if !native_window.visible || native_window.focus_window_handle != Some(expected_hwnd) {
        return Err(io::Error::other(
            "private motion window lost native visibility or UI-thread focus",
        ));
    }
    Ok((evidence, native_window))
}

/// 其它平台没有本专项所需的 Win32 私有桌面证据，不能把可见窗口误认为已隔离。
///
/// @author kongweiguang
#[cfg(all(not(windows), feature = "acceptance-metrics"))]
async fn show_private_motion_window(
    _cx: &mut AsyncApp,
    _window: AnyWindowHandle,
) -> Result<usize, Box<dyn Error>> {
    Err(io::Error::other("motion acceptance requires a Windows private desktop").into())
}

/// 非 Windows 平台保持失败关闭，防止缺失原生隔离实现时继续采集。
///
/// @author kongweiguang
#[cfg(all(not(windows), feature = "acceptance-metrics"))]
fn ensure_motion_window(
    _window: &gpui_pre::Window,
    _expected_hwnd: usize,
) -> Result<
    (
        crate::right_rail_acceptance::isolation::DesktopIsolationEvidence,
        MotionNativeWindowState,
    ),
    io::Error,
> {
    Err(io::Error::other(
        "motion acceptance requires a Windows private desktop",
    ))
}

/// 专项被请求但 profiler 未编译时明确失败，避免零样本被误记为通过。
///
/// @author kongweiguang
#[cfg(not(feature = "acceptance-metrics"))]
pub(super) async fn capture(
    _cx: &mut AsyncApp,
    _root: &Entity<RootView>,
    _window: AnyWindowHandle,
    _services: &kerminal_ui::AppServices,
    _context: &WorkbenchContext,
    _initial_session: WorkbenchState,
) -> Result<(), Box<dyn Error>> {
    Err(io::Error::other("motion acceptance requires the acceptance-metrics feature").into())
}
