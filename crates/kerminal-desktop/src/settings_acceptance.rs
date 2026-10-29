//! Hidden native-window acceptance for the production Settings modal and its controls.
//!
//! @author kongweiguang

use std::{
    error::Error,
    ffi::OsStr,
    fs, io,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use gpui_kit::{
    AnyWindowHandle, AppContext as _, AsyncApp, Entity, FocusHandle, InputEvent, Keystroke,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ScrollDelta, ScrollWheelEvent,
    TitlebarOptions, TouchPhase, WindowBackgroundAppearance, WindowOptions, component::TitleBar,
    point,
};
use gpui_pre::{App, Bounds, ElementId, WindowBounds, px, size};
use kerminal_application::preferences::{
    InterfaceDensity, TerminalKeywordHighlightMatchMode, TerminalKeywordHighlightRule, ThemeMode,
};
use kerminal_ui::{RootView, SettingsAcceptanceState};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{composition::DesktopComposition, runtime::ApplicationRuntime};

#[path = "settings_acceptance/app_theme.rs"]
mod app_theme;
#[path = "settings_acceptance/mcp.rs"]
mod mcp;
#[path = "settings_acceptance/profiles.rs"]
mod profiles;
#[path = "settings_acceptance/sync.rs"]
mod sync;
#[path = "settings_acceptance/workbench.rs"]
mod workbench;

const READY_TIMEOUT: Duration = Duration::from_secs(45);
const ACTION_TIMEOUT: Duration = Duration::from_secs(20);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(12);
const WINDOW_RESIZE_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(120);
const DEFAULT_WIDTH: u32 = 1440;
const DEFAULT_HEIGHT: u32 = 900;
const MIN_WIDTH: u32 = 800;
const MAX_WIDTH: u32 = 3840;
const MIN_HEIGHT: u32 = 600;
const MAX_HEIGHT: u32 = 2160;
const REPORT_FILE: &str = "report.json";

const SETTINGS_PAGES: [(&str, &str); 12] = [
    ("appearance", "appearance"),
    ("terminal", "terminal"),
    ("profiles", "profiles"),
    ("keywords", "keywords"),
    ("suggestions", "suggestions"),
    ("sftp", "sftp"),
    ("external-launch", "external-launch"),
    ("mcp", "mcp"),
    ("sync", "sync"),
    ("desktop", "desktop"),
    ("shortcuts", "shortcuts"),
    ("about", "about"),
];

const THEMES: [(ThemeMode, &str, &str); 3] = [
    (ThemeMode::Light, "light", "theme-light"),
    (ThemeMode::Dark, "dark", "theme-dark"),
    (ThemeMode::System, "system", "theme-system"),
];

#[derive(Default)]
struct AcceptanceOptions {
    output_directory: Option<PathBuf>,
    width: Option<u32>,
    height: Option<u32>,
}

struct AcceptanceContext {
    output_directory: PathBuf,
    width: u32,
    height: u32,
    sync_fixture: sync::SyncFixture,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AcceptanceReport {
    schema_version: u32,
    capture_mode: &'static str,
    capture_backend: &'static str,
    native_isolation: crate::right_rail_acceptance::isolation::DesktopIsolationEvidence,
    config_directory: String,
    window_visible: bool,
    window_active: bool,
    window_width: f32,
    window_height: f32,
    settings_modal_generation: u64,
    language_escape_check: LanguageEscapeCheck,
    themes: Vec<ThemeCapture>,
    disclosure_checks: Vec<SettingCheck>,
    density_checks: Vec<SettingCheck>,
    keyword_rule_check: SettingCheck,
    keyword_lifecycle_checks: Vec<KeywordLifecycleCheck>,
    sync_checks: Vec<SettingCheck>,
    mcp_checks: Vec<SettingCheck>,
    profile_checks: Vec<SettingCheck>,
    screenshots: Vec<CaptureRecord>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThemeCapture {
    requested_mode: &'static str,
    persisted_mode: &'static str,
    window_appearance: String,
    effective_theme: String,
    pages: Vec<PageCapture>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PageCapture {
    page_id: &'static str,
    navigation_selector: String,
    observed_page_id: &'static str,
    settings_ready: bool,
    capture: CaptureRecord,
    bottom_capture: CaptureRecord,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct CaptureRecord {
    file: String,
    width: u32,
    height: u32,
    pixel_sha256: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingCheck {
    action: &'static str,
    passed: bool,
    expected: String,
    observed: String,
    capture: Option<CaptureRecord>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KeywordLifecycleCheck {
    action: &'static str,
    passed: bool,
    expected: String,
    observed: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LanguageEscapeCheck {
    action: &'static str,
    passed: bool,
    expected_language: String,
    observed_language: String,
    first_escape_closed_select: bool,
    first_escape_preserved_modal: bool,
    first_escape_preserved_language: bool,
    second_escape_closed_modal: bool,
    focus_returned_to_trigger: bool,
    modal_reopened: bool,
    dropdown_open_capture: CaptureRecord,
    dropdown_closed_capture: CaptureRecord,
}

/// Detects only the dedicated command so ordinary desktop startup stays on its production path.
pub fn requested() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == OsStr::new("--settings-acceptance"))
}

/// 专项入口优先分派到隔离的主题验收；其它情况下继续运行现有完整设置验收及资源收尾。
///
/// @author kongweiguang
pub fn run() -> Result<(), Box<dyn Error>> {
    if app_theme::requested() {
        return app_theme::run();
    }
    if workbench::requested() {
        return workbench::run();
    }
    let options = requested_options()?;
    let output_directory = match options.output_directory {
        Some(path) => create_requested_output_directory(&path)?,
        None => create_unique_output_directory(
            Path::new("target")
                .join("diagnostics")
                .join("settings-parity"),
        )?,
    };
    let config_directory = output_directory.join("config");
    fs::create_dir(&config_directory)?;
    let runtime = ApplicationRuntime::new()?;
    let mut composition = match runtime.block_on(DesktopComposition::build(
        runtime.handle(),
        &config_directory,
    )) {
        Ok(composition) => composition,
        Err(error) => {
            runtime.shutdown();
            return Err(error.into());
        }
    };
    let services = composition.app_services();
    let ui_result = match runtime.block_on(sync::prepare(&services, &output_directory)) {
        Ok(sync_fixture) => {
            let context = AcceptanceContext {
                output_directory: output_directory.clone(),
                width: options.width.unwrap_or(DEFAULT_WIDTH),
                height: options.height.unwrap_or(DEFAULT_HEIGHT),
                sync_fixture,
            };
            catch_unwind(AssertUnwindSafe(|| run_hidden_window(services, context))).unwrap_or_else(
                |_| Err(io::Error::other("settings acceptance event loop panicked").into()),
            )
        }
        Err(error) => Err(error),
    };
    let shutdown_result = runtime
        .block_on(composition.shutdown_until(tokio::time::Instant::now() + SHUTDOWN_TIMEOUT));
    runtime.shutdown();

    ui_result?;
    shutdown_result?;
    println!(
        "Settings parity report: {}",
        output_directory.join(REPORT_FILE).display()
    );
    Ok(())
}

/// Rejects duplicate, malformed, and unknown options before creating an isolated output root.
fn requested_options() -> Result<AcceptanceOptions, io::Error> {
    parse_options(std::env::args_os().skip(2))
}

/// Parses an explicit argument stream so boundary cases remain testable without mutating process arguments.
fn parse_options(
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<AcceptanceOptions, io::Error> {
    let mut arguments = arguments.into_iter();
    let mut options = AcceptanceOptions::default();
    while let Some(flag) = arguments.next() {
        let (slot, name, minimum, maximum) = if flag == OsStr::new("--output-dir") {
            if options.output_directory.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--output-dir repeated",
                ));
            }
            let Some(path) = arguments.next() else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--output-dir requires a path",
                ));
            };
            options.output_directory = Some(PathBuf::from(path));
            continue;
        } else if flag == OsStr::new("--width") {
            (&mut options.width, "--width", MIN_WIDTH, MAX_WIDTH)
        } else if flag == OsStr::new("--height") {
            (&mut options.height, "--height", MIN_HEIGHT, MAX_HEIGHT)
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "supported options: --settings-acceptance [--output-dir <new-dir>] [--width <logical-px>] [--height <logical-px>]",
            ));
        };
        if slot.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} repeated"),
            ));
        }
        let Some(value) = arguments.next() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} requires a logical-pixel value"),
            ));
        };
        *slot = Some(parse_dimension(value, name, minimum, maximum)?);
    }
    Ok(options)
}

/// Parses a bounded logical dimension to keep acceptance windows useful on common displays.
fn parse_dimension(
    value: std::ffi::OsString,
    name: &str,
    minimum: u32,
    maximum: u32,
) -> Result<u32, io::Error> {
    let value = value.to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("{name} must be UTF-8"))
    })?;
    let dimension = value.parse::<u32>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be an integer"),
        )
    })?;
    if !(minimum..=maximum).contains(&dimension) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} range is {minimum}..={maximum} logical pixels"),
        ));
    }
    Ok(dimension)
}

/// Creates only a new leaf directory, preserving all prior screenshots and reports.
fn create_requested_output_directory(path: &Path) -> Result<PathBuf, io::Error> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    fs::create_dir_all(absolute.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "output directory has no parent",
        )
    })?)?;
    fs::create_dir(&absolute)?;
    Ok(absolute)
}

/// Allocates a timestamped leaf under the dedicated diagnostics root without replacing evidence.
fn create_unique_output_directory(base: PathBuf) -> Result<PathBuf, io::Error> {
    fs::create_dir_all(&base)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    for suffix in 0..100_u32 {
        let name = if suffix == 0 {
            format!("run-{now}-{}", std::process::id())
        } else {
            format!("run-{now}-{}-{suffix}", std::process::id())
        };
        let path = base.join(name);
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique settings parity output directory",
    ))
}

/// 打开固定验收尺寸的隐藏窗口；最小尺寸与请求一致，避免隔离桌面的屏幕跟踪限制夹紧截图。
/// 窗口不显示或切换桌面，绘制和回读结束后统一退出。
fn run_hidden_window(
    services: kerminal_ui::AppServices,
    context: AcceptanceContext,
) -> Result<(), Box<dyn Error>> {
    let failure = Arc::new(Mutex::new(None::<String>));
    let failure_for_app = Arc::clone(&failure);
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            kerminal_ui::init(cx);
            let bounds = match acceptance_bounds(cx, context.width, context.height) {
                Ok(bounds) => bounds,
                Err(error) => {
                    set_failure(&failure_for_app, error.to_string());
                    cx.quit();
                    return;
                }
            };
            let window_options = WindowOptions {
                window_background: WindowBackgroundAppearance::Transparent,
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(context.width as f32), px(context.height as f32))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Kerminal Settings Parity Acceptance".into()),
                    ..TitleBar::title_bar_options()
                }),
                show: false,
                focus: false,
                ..TitleBar::window_options()
            };
            let result = gpui_kit::open_window(window_options, cx, |window, cx| {
                window.set_background_appearance(WindowBackgroundAppearance::Transparent);
                window.resize(size(px(context.width as f32), px(context.height as f32)));
                let root = cx.new(|cx| RootView::new(services.clone(), window, cx));
                if let Err(error) =
                    crate::right_rail_acceptance::isolation::capture().and_then(|evidence| {
                        evidence
                            .validate_window_state(window.is_visible(), window.is_window_active())
                    })
                {
                    set_failure(
                        &failure_for_app,
                        format!("settings acceptance window isolation failed: {error}"),
                    );
                    cx.quit();
                    return root;
                }
                let window_handle = window.window_handle();
                let capture_root = root.clone();
                let capture_services = services.clone();
                let capture_failure = Arc::clone(&failure_for_app);
                let capture_context = AcceptanceContext {
                    output_directory: context.output_directory.clone(),
                    width: context.width,
                    height: context.height,
                    sync_fixture: context.sync_fixture.clone(),
                };
                cx.spawn(async move |cx| {
                    let result = capture_settings(
                        cx,
                        capture_root,
                        window_handle,
                        capture_services,
                        capture_context,
                    )
                    .await;
                    if let Err(error) = result {
                        set_failure(&capture_failure, error.to_string());
                    }
                    cx.update(|cx| cx.quit());
                })
                .detach();
                root
            });
            if let Err(error) = result {
                set_failure(&failure_for_app, error.to_string());
                cx.quit();
            }
        });
    if let Some(message) = failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        return Err(io::Error::other(message).into());
    }
    Ok(())
}

/// Centers explicit logical bounds so hidden captures do not depend on the user's saved window layout.
fn acceptance_bounds(
    cx: &App,
    width: u32,
    height: u32,
) -> Result<Bounds<gpui_pre::Pixels>, io::Error> {
    let display = cx.primary_display().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "no primary display is available")
    })?;
    Ok(Bounds::centered_at(
        display.visible_bounds().center(),
        size(px(width as f32), px(height as f32)),
    ))
}

/// 首次 settings 加载可能异步恢复旧窗口几何；重设尺寸后等待 Win32 异步调整确认，避免以默认 bounds 截图。
///
/// @author kongweiguang
async fn resize_acceptance_window(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    width: u32,
    height: u32,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        window.resize(size(px(width as f32), px(height as f32)));
        window.refresh();
        draw_acceptance_frame(window, app);
        Ok::<_, io::Error>(())
    })??;

    let expected_width = width as f32;
    let expected_height = height as f32;
    let deadline = Instant::now() + WINDOW_RESIZE_TIMEOUT;
    loop {
        let (visible, active, actual_width, actual_height, viewport_width, viewport_height, scale) =
            window.update(cx, |_, window, _| {
                let bounds = window.bounds();
                let viewport = window.visual_viewport_bounds();
                (
                    window.is_visible(),
                    window.is_window_active(),
                    bounds.size.width.as_f32(),
                    bounds.size.height.as_f32(),
                    viewport.size.width.as_f32(),
                    viewport.size.height.as_f32(),
                    window.scale_factor(),
                )
            })?;
        if visible || active {
            window.update(cx, |_, window, _| ensure_hidden_window(window))??;
        }
        if actual_width == expected_width && actual_height == expected_height {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "settings window resize timed out: requested={width}x{height}, platform_client={actual_width:.1}x{actual_height:.1} logical ({:.0}x{:.0} device px at scale {scale:.3}), viewport={viewport_width:.1}x{viewport_height:.1} logical",
                actual_width * scale,
                actual_height * scale,
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 在隔离生产 Root 上使用真实快捷键和滚轮；先保存 Profile，使三主题截图覆盖真实记录。
/// 紧凑窗口中入口及字段必须先进入可见区域。
///
/// @author kongweiguang
async fn capture_settings(
    cx: &mut AsyncApp,
    root: Entity<RootView>,
    window: AnyWindowHandle,
    services: kerminal_ui::AppServices,
    context: AcceptanceContext,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + READY_TIMEOUT;
    let initial = wait_for_state(cx, &root, window, deadline, |state| {
        state.settings_ready() && state.draft_ready()
    })
    .await?;
    if initial.modal_open() {
        return Err(io::Error::other(
            "settings modal was already open before the acceptance click",
        )
        .into());
    }
    resize_acceptance_window(cx, window, context.width, context.height).await?;
    let mut themes = Vec::with_capacity(THEMES.len());
    let mut screenshots = Vec::with_capacity(THEMES.len() * SETTINGS_PAGES.len() * 2 + 10);
    press_key(cx, window, "ctrl-alt-s")?;
    let _opened = wait_for_state(cx, &root, window, deadline, |state| {
        state.modal_open() && state.settings_ready() && state.draft_ready()
    })
    .await?;
    screenshots.push(save_frame(
        cx,
        window,
        context.output_directory.join("settings-opened.png"),
    )?);
    let opened = wait_for_state(
        cx,
        &root,
        window,
        Instant::now() + ACTION_TIMEOUT,
        |state| state.modal_open() && state.settings_ready() && state.draft_ready(),
    )
    .await?;
    let modal_generation = opened.modal_generation();
    let profile_checks =
        profiles::verify(cx, &root, window, &services, &context, &mut screenshots).await?;
    for (theme, name, selector) in THEMES {
        ensure_page(cx, &root, window, "appearance", "appearance").await?;
        reveal_settings_selector(cx, &root, window, "appearance", selector).await?;
        click_selector(cx, window, selector)?;
        let theme_stage = format!("theme selector {selector} should select {name}");
        let stable_draft = wait_for_state_with_stage(
            cx,
            &root,
            window,
            Instant::now() + ACTION_TIMEOUT,
            &theme_stage,
            |state| {
                state.modal_open()
                    && state.draft_theme_mode() == Some(name)
                    && !state.draft_dirty()
                    && state.draft_ready()
            },
        )
        .await?;
        let persisted = load_persisted_settings(&services).await?;
        let persisted_mode = theme_mode_id(persisted.settings.theme_mode);
        if persisted_mode != name {
            return Err(io::Error::other(format!(
                "theme save readback mismatch: requested={name}, persisted={persisted_mode}"
            ))
            .into());
        }
        let (window_appearance, effective_theme) = window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            Ok::<_, io::Error>((
                format!("{:?}", window.appearance()),
                if gpui_kit::component::Theme::global(app).is_dark() {
                    "dark"
                } else {
                    "light"
                }
                .to_owned(),
            ))
        })??;
        let expected_appearance = match theme {
            ThemeMode::Dark => Some("dark"),
            ThemeMode::Light => Some("light"),
            ThemeMode::System => None,
        };
        if expected_appearance.is_some_and(|expected| effective_theme != expected) {
            return Err(io::Error::other(format!(
                "component theme did not follow saved theme {name}: {effective_theme}"
            ))
            .into());
        }
        let mut pages = Vec::with_capacity(SETTINGS_PAGES.len());
        for (page_id, _) in SETTINGS_PAGES {
            let state = ensure_page(cx, &root, window, page_id, page_id).await?;
            if !state.settings_ready() || !state.draft_ready() {
                return Err(io::Error::other(format!(
                    "settings page {page_id} rendered before its draft was ready"
                ))
                .into());
            }
            scroll_settings_content(cx, &root, window, page_id, 5_000.)?;
            let file_name = format!("settings-{name}-{page_id}.png");
            let capture = save_frame(cx, window, context.output_directory.join(&file_name))?;
            screenshots.push(capture.clone());
            scroll_settings_content(cx, &root, window, page_id, -5_000.)?;
            let bottom_file_name = format!("settings-{name}-{page_id}-bottom.png");
            let bottom_capture =
                save_frame(cx, window, context.output_directory.join(&bottom_file_name))?;
            screenshots.push(bottom_capture.clone());
            pages.push(PageCapture {
                page_id,
                navigation_selector: format!("preferences-nav-{page_id}"),
                observed_page_id: state.page_id(),
                settings_ready: state.settings_ready(),
                capture,
                bottom_capture,
            });
        }
        themes.push(ThemeCapture {
            requested_mode: name,
            persisted_mode,
            window_appearance,
            effective_theme,
            pages,
        });
        let _ = stable_draft;
    }

    verify_capture_dimensions(cx, window, context.width, context.height, &screenshots)?;
    let language_escape_check =
        verify_language_select_escape(cx, &root, window, &context, &mut screenshots).await?;
    let disclosure_checks =
        capture_expanded_disclosures(cx, &root, window, &context, &mut screenshots).await?;
    let mcp_checks = mcp::verify(cx, &root, window, &services, &context, &mut screenshots).await?;

    let mut density_checks = Vec::with_capacity(2);
    ensure_page(cx, &root, window, "appearance", "appearance").await?;
    for (density, selector) in [
        (InterfaceDensity::Spacious, "density-spacious"),
        (InterfaceDensity::Compact, "density-compact"),
    ] {
        let expected = density_id(density);
        reveal_settings_selector(cx, &root, window, "appearance", selector).await?;
        click_selector(cx, window, selector)?;
        wait_for_state(
            cx,
            &root,
            window,
            Instant::now() + ACTION_TIMEOUT,
            |state| {
                state.modal_open()
                    && state.draft_density() == Some(expected)
                    && !state.draft_dirty()
                    && state.draft_ready()
            },
        )
        .await?;
        let persisted = load_persisted_settings(&services).await?;
        let persisted_density = density_id(persisted.settings.interface_density);
        if persisted_density != expected {
            return Err(io::Error::other(format!(
                "density save readback mismatch: requested={expected}, persisted={persisted_density}"
            ))
            .into());
        }
        let file_name = format!("settings-density-{expected}.png");
        let capture = save_frame(cx, window, context.output_directory.join(file_name))?;
        screenshots.push(capture.clone());
        density_checks.push(SettingCheck {
            action: "click-and-readback-density",
            passed: persisted_density == expected,
            expected: expected.to_owned(),
            observed: persisted_density.to_owned(),
            capture: Some(capture),
        });
    }

    let (keyword_rule_check, keyword_lifecycle_checks) =
        add_keyword_rule(cx, &root, window, &services, &context, &mut screenshots).await?;
    let sync_checks =
        sync::verify(cx, &root, window, &services, &context, &mut screenshots).await?;
    let final_state = root_state(cx, &root, window)?;
    if !final_state.modal_open() {
        return Err(
            io::Error::other("settings modal closed before final lifecycle readback").into(),
        );
    }
    click_selector(cx, window, "preferences-close")?;
    wait_for_state(
        cx,
        &root,
        window,
        Instant::now() + ACTION_TIMEOUT,
        |state| !state.modal_open(),
    )
    .await?;
    let closed_state = root_state(cx, &root, window)?;
    if closed_state.modal_open() {
        return Err(io::Error::other(
            "settings modal did not close through its production control",
        )
        .into());
    }
    verify_capture_dimensions(cx, window, context.width, context.height, &screenshots)?;

    let (visible, active, bounds, native_isolation) = window.update(cx, |_, window, _| {
        let visible = window.is_visible();
        let active = window.is_window_active();
        let native_isolation = crate::right_rail_acceptance::isolation::capture()?;
        native_isolation.validate_window_state(visible, active)?;
        Ok::<_, io::Error>((visible, active, window.bounds(), native_isolation))
    })??;
    let report = AcceptanceReport {
        schema_version: 4,
        capture_mode: "hidden_native_window",
        capture_backend: "gpui_window_render_to_image",
        native_isolation,
        config_directory: context
            .output_directory
            .join("config")
            .display()
            .to_string(),
        window_visible: visible,
        window_active: active,
        window_width: bounds.size.width.as_f32(),
        window_height: bounds.size.height.as_f32(),
        settings_modal_generation: modal_generation,
        language_escape_check,
        themes,
        disclosure_checks,
        density_checks,
        keyword_rule_check,
        keyword_lifecycle_checks,
        sync_checks,
        mcp_checks,
        profile_checks,
        screenshots,
    };
    let report_path = context.output_directory.join(REPORT_FILE);
    fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    Ok(())
}

/// 配置仓储使用 Tokio 阻塞池，验收回读也必须经过生产执行器，避免 GPUI 线程缺少 reactor。
///
/// @author kongweiguang
async fn load_persisted_settings(
    services: &kerminal_ui::AppServices,
) -> kerminal_application::AppResult<kerminal_application::preferences::VersionedSettings> {
    let preferences = services.preferences.clone();
    services
        .executor
        .run(move |_| async move { preferences.load_settings().await })
        .await
}

/// 在生产 Tokio 执行器内创建超时计时器；GPUI future 所在线程没有 Tokio reactor。
/// 撤销窗口使用剩余时限，避免配置回读耗尽用户实际可用的 15 秒撤销时间。
///
/// @author kongweiguang
async fn load_persisted_settings_with_timeout(
    services: &kerminal_ui::AppServices,
    timeout: Duration,
) -> kerminal_application::AppResult<kerminal_application::preferences::VersionedSettings> {
    let preferences = services.preferences.clone();
    services
        .executor
        .run(move |_| async move {
            tokio::time::timeout(timeout, preferences.load_settings())
                .await
                .map_err(|source| {
                    kerminal_application::AppError::infrastructure_with_source(
                        "Settings acceptance readback timed out",
                        source,
                    )
                })?
        })
        .await
}
/// 通过生产弹层栈和真实焦点句柄区分 Select 关闭与设置弹框关闭。
///
/// @author kongweiguang
async fn verify_language_select_escape(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
) -> Result<LanguageEscapeCheck, Box<dyn Error>> {
    ensure_page(cx, root, window, "appearance", "appearance").await?;
    let before = wait_for_state_with_stage(
        cx,
        root,
        window,
        Instant::now() + ACTION_TIMEOUT,
        "language Select draft and selected value synchronization",
        |state| {
            state.modal_open()
                && state.page_id() == "appearance"
                && state.draft_interface_language().is_some()
                && state.draft_interface_language() == state.selected_interface_language()
        },
    )
    .await?;
    let expected_language = before
        .draft_interface_language()
        .ok_or_else(|| io::Error::other("appearance page has no interface-language draft"))?
        .to_owned();
    let select_focus = before.language_select_focus().clone();
    let return_focus = before
        .modal_return_focus()
        .cloned()
        .ok_or_else(|| io::Error::other("settings modal did not remember its opening focus"))?;
    if deferred_popover_open(cx, window)? {
        return Err(io::Error::other(
            "a deferred popup was already open before the language Select check",
        )
        .into());
    }

    reveal_settings_selector(
        cx,
        root,
        window,
        "appearance",
        "settings-interface-language-select",
    )
    .await?;
    click_selector(cx, window, "settings-interface-language-select")?;
    wait_for_deferred_context(cx, window, true, Instant::now() + ACTION_TIMEOUT).await?;
    let dropdown_open_capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-language-select-open.png"),
    )?;
    screenshots.push(dropdown_open_capture.clone());

    press_key(cx, window, "escape")?;
    wait_for_deferred_context(cx, window, false, Instant::now() + ACTION_TIMEOUT).await?;
    let first_escape_state =
        wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
            state.modal_open()
                && state.page_id() == "appearance"
                && state.draft_interface_language() == Some(expected_language.as_str())
                && state.selected_interface_language() == Some(expected_language.as_str())
        })
        .await?;
    wait_for_window_focus(cx, window, &select_focus, Instant::now() + ACTION_TIMEOUT).await?;
    let first_escape_closed_select = !deferred_popover_open(cx, window)?;
    if !first_escape_closed_select {
        return Err(io::Error::other("first Escape left the language Select popup open").into());
    }
    let dropdown_closed_capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-language-select-dismissed.png"),
    )?;
    screenshots.push(dropdown_closed_capture.clone());

    press_key(cx, window, "escape")?;
    let closed = wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        !state.modal_open()
    })
    .await?;
    wait_for_window_focus(cx, window, &return_focus, Instant::now() + ACTION_TIMEOUT).await?;
    let focus_returned_to_trigger = window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        Ok::<_, io::Error>(window.focused(app).as_ref() == Some(&return_focus))
    })??;
    if !focus_returned_to_trigger {
        return Err(io::Error::other("modal close did not restore focus to its trigger").into());
    }

    press_key(cx, window, "ctrl-alt-s")?;
    let reopened = wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open()
            && state.modal_generation() > closed.modal_generation()
            && state.settings_ready()
            && state.draft_ready()
    })
    .await?;
    if reopened.draft_interface_language() != Some(expected_language.as_str())
        || reopened.selected_interface_language() != Some(expected_language.as_str())
    {
        return Err(
            io::Error::other("reopened settings changed the committed interface language").into(),
        );
    }

    Ok(LanguageEscapeCheck {
        action: "escape-select-then-modal-and-restore-focus",
        passed: true,
        expected_language: expected_language.clone(),
        observed_language: first_escape_state
            .selected_interface_language()
            .unwrap_or_default()
            .to_owned(),
        first_escape_closed_select,
        first_escape_preserved_modal: first_escape_state.modal_open(),
        first_escape_preserved_language: first_escape_state
            .draft_interface_language()
            .is_some_and(|language| language == expected_language)
            && first_escape_state
                .selected_interface_language()
                .is_some_and(|language| language == expected_language),
        second_escape_closed_modal: !closed.modal_open(),
        focus_returned_to_trigger,
        modal_reopened: reopened.modal_open(),
        dropdown_open_capture,
        dropdown_closed_capture,
    })
}

/// Captures one expanded rendering per real disclosure and restores each local toggle after capture.
async fn capture_expanded_disclosures(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
) -> Result<Vec<SettingCheck>, Box<dyn Error>> {
    let disclosures = [
        (
            "terminal",
            "terminal-renderer-disclosure",
            "terminal-renderer",
        ),
        ("sftp", "preferences-sftp-advanced", "sftp-advanced"),
        (
            "external-launch",
            "external-launch-compatibility-toggle",
            "external-launch-compatibility",
        ),
        ("mcp", "mcp-connection-info-toggle", "mcp-connection-info"),
        (
            "desktop",
            "desktop-notifications-advanced-toggle",
            "desktop-notifications-advanced",
        ),
    ];
    let mut checks = Vec::with_capacity(disclosures.len());
    for (index, (page_id, selector, capture_id)) in disclosures.into_iter().enumerate() {
        ensure_page(cx, root, window, page_id, page_id).await?;
        let before = root_state(cx, root, window)?;
        if before.expanded_disclosures()[index] {
            return Err(io::Error::other(format!(
                "disclosure {capture_id} was unexpectedly expanded before its capture"
            ))
            .into());
        }

        reveal_settings_selector(cx, root, window, page_id, selector).await?;
        click_selector(cx, window, selector)?;
        wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
            state.modal_open() && state.expanded_disclosures()[index]
        })
        .await?;
        scroll_settings_content(cx, root, window, page_id, -240.)?;
        let capture = save_frame(
            cx,
            window,
            context
                .output_directory
                .join(format!("settings-disclosure-system-{capture_id}.png")),
        )?;
        screenshots.push(capture.clone());

        reveal_settings_selector(cx, root, window, page_id, selector).await?;
        click_selector(cx, window, selector)?;
        wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
            state.modal_open() && !state.expanded_disclosures()[index]
        })
        .await?;
        let expected = format!("{capture_id}:expanded");
        checks.push(SettingCheck {
            action: "capture-expanded-disclosure",
            passed: true,
            expected: expected.clone(),
            observed: expected,
            capture: Some(capture),
        });
    }
    Ok(checks)
}

/// 长页面仅用真实滚轮定位；控件完整进入生产正文裁切区域后才允许点击。
///
/// @author kongweiguang
async fn reveal_settings_selector(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    page_id: &str,
    selector: &str,
) -> Result<(), Box<dyn Error>> {
    scroll_settings_content(cx, root, window, page_id, 5_000.)?;
    for _ in 0..16 {
        let visible = window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            draw_acceptance_frame(window, app);
            let viewport = root.update(app, |root, cx| root.settings_acceptance_content_bounds(cx));
            Ok::<_, io::Error>(
                selector_bounds(window, selector)
                    .is_ok_and(|bounds| bounds_inside_viewport(bounds, viewport)),
            )
        })??;
        if visible {
            return Ok(());
        }
        scroll_settings_content(cx, root, window, page_id, -280.)?;
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
    Err(io::Error::other(format!(
        "settings control {selector:?} could not be revealed on page {page_id:?}"
    ))
    .into())
}

/// Checks the shared popup registry so the Escape probe proves the Select closed its real overlay.
fn deferred_popover_open(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<bool, Box<dyn Error>> {
    Ok(window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        Ok::<_, io::Error>(gpui_kit::base::GlobalState::is_in_deferred_context(app))
    })??)
}

/// Waits for the Select's registered popup to open or close before dispatching the next Escape.
async fn wait_for_deferred_context(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    expected_open: bool,
    deadline: Instant,
) -> Result<(), Box<dyn Error>> {
    loop {
        if deferred_popover_open(cx, window)? == expected_open {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "deferred Select popup did not reach open={expected_open}"
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 每次轮询驱动隐藏窗口的下一帧回调，再比较原始 GPUI 焦点句柄身份。
///
/// @author kongweiguang
async fn wait_for_window_focus(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    expected: &FocusHandle,
    deadline: Instant,
) -> Result<(), Box<dyn Error>> {
    loop {
        let focused = window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            window.refresh();
            draw_acceptance_frame(window, app);
            Ok::<_, io::Error>(window.focused(app))
        })??;
        if focused.as_ref() == Some(expected) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(
                io::Error::other("window focus did not return to the saved trigger").into(),
            );
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 比较关键词规则的持久化验收字段，避免把界面回显误作保存证据。
///
/// @author kongweiguang
fn acceptance_keyword_rule_matches(rule: &TerminalKeywordHighlightRule, enabled: bool) -> bool {
    rule.pattern == "settings-parity-keyword"
        && rule.note == "isolated acceptance"
        && rule.enabled == enabled
        && rule.match_mode == TerminalKeywordHighlightMatchMode::WholeWord
        && rule
            .custom_colors
            .as_ref()
            .is_some_and(|colors| colors.light.foreground.as_deref() == Some("#2F80ED"))
}

/// 用真实控件和滚轮验证窄窗规则生命周期；字段回显与持久化回读分别断言。
///
/// @author kongweiguang
async fn add_keyword_rule(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
) -> Result<(SettingCheck, Vec<KeywordLifecycleCheck>), Box<dyn Error>> {
    ensure_page(cx, root, window, "keywords", "keywords").await?;
    let before = load_persisted_settings(services).await?;
    let before_count = before.settings.terminal.keyword_highlights.rules.len();
    if before_count >= 64 {
        return Err(io::Error::other(
            "isolated keyword acceptance settings already reached the 64-rule limit",
        )
        .into());
    }
    reveal_settings_selector(cx, root, window, "keywords", "keyword-new").await?;
    click_selector(cx, window, "keyword-new")?;
    let new_rule = wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open()
            && state.keyword_editor_open()
            && state.keyword_editor_is_new()
            && state.keyword_editor_pattern() == Some("")
            && state.keyword_editor_note() == Some("")
            && !state.keyword_save_pending()
    })
    .await?;
    let new_rule_id = new_rule
        .keyword_editor_rule_id()
        .ok_or_else(|| io::Error::other("new keyword editor did not expose its stable rule ID"))?
        .to_owned();

    reveal_settings_selector(cx, root, window, "keywords", "keyword-pattern-field").await?;
    click_and_type(
        cx,
        window,
        "keyword-pattern-field",
        "settings-parity-keyword",
    )?;
    reveal_settings_selector(cx, root, window, "keywords", "keyword-note-field").await?;
    click_and_type(cx, window, "keyword-note-field", "isolated acceptance")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open()
            && state.keyword_editor_open()
            && state.keyword_editor_is_new()
            && state.keyword_editor_rule_id() == Some(new_rule_id.as_str())
            && state.keyword_editor_pattern() == Some("settings-parity-keyword")
            && state.keyword_editor_note() == Some("isolated acceptance")
            && !state.keyword_save_pending()
            && !state.draft_dirty()
            && state.draft_ready()
    })
    .await?;

    let before_save = load_persisted_settings(services).await?;
    let before_save_rules = &before_save.settings.terminal.keyword_highlights.rules;
    if before_save_rules.len() != before_count
        || before_save_rules.iter().any(|rule| rule.id == new_rule_id)
    {
        return Err(io::Error::other(
            "new keyword draft reached persisted settings before Save was clicked",
        )
        .into());
    }

    reveal_settings_selector(cx, root, window, "keywords", "keyword-style-Custom").await?;
    click_selector(cx, window, "keyword-style-Custom")?;
    reveal_settings_selector(cx, root, window, "keywords", "keyword-mode-WholeWord").await?;
    click_selector(cx, window, "keyword-mode-WholeWord")?;
    reveal_settings_selector(
        cx,
        root,
        window,
        "keywords",
        "keyword-color-field-light-foreground",
    )
    .await?;
    click_and_type(
        cx,
        window,
        "keyword-color-field-light-foreground",
        "#2F80ED",
    )?;
    reveal_settings_selector(
        cx,
        root,
        window,
        "keywords",
        "keyword-color-picker-light-foreground",
    )
    .await?;
    screenshots.push(save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-keyword-colors.png"),
    )?);
    click_selector(cx, window, "keyword-color-picker-light-foreground")?;
    wait_for_deferred_context(cx, window, true, Instant::now() + ACTION_TIMEOUT).await?;
    screenshots.push(save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-keyword-color-picker.png"),
    )?);
    press_key(cx, window, "escape")?;
    wait_for_deferred_context(cx, window, false, Instant::now() + ACTION_TIMEOUT).await?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open() && state.keyword_editor_open() && state.keyword_editor_is_new()
    })
    .await?;
    reveal_settings_selector(cx, root, window, "keywords", "keyword-save").await?;
    click_selector(cx, window, "keyword-save")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open()
            && state.keyword_editor_open()
            && !state.keyword_editor_is_new()
            && state.keyword_editor_rule_id() == Some(new_rule_id.as_str())
            && state.keyword_editor_pattern() == Some("settings-parity-keyword")
            && state.keyword_editor_note() == Some("isolated acceptance")
            && state.draft_keyword_rule_count() == Some(before_count + 1)
            && !state.keyword_save_pending()
            && !state.draft_dirty()
            && state.draft_ready()
    })
    .await?;

    let persisted_after_save = load_persisted_settings(services).await?;
    let saved_rule = persisted_after_save
        .settings
        .terminal
        .keyword_highlights
        .rules
        .iter()
        .find(|rule| rule.id == new_rule_id);
    let saved_color =
        saved_rule.and_then(|rule| rule.custom_colors.as_ref()?.light.foreground.as_deref());
    let saved_mode = saved_rule.map(|rule| rule.match_mode);
    let saved_fields_match =
        saved_rule.is_some_and(|rule| acceptance_keyword_rule_matches(rule, true));
    if persisted_after_save
        .settings
        .terminal
        .keyword_highlights
        .rules
        .len()
        != before_count + 1
        || !saved_fields_match
    {
        return Err(io::Error::other(format!(
            "keyword save readback mismatch: expected WholeWord and #2F80ED on rule {new_rule_id}"
        ))
        .into());
    }

    let saved_mode_id = match saved_mode {
        Some(TerminalKeywordHighlightMatchMode::Literal) => "Literal",
        Some(TerminalKeywordHighlightMatchMode::WholeWord) => "WholeWord",
        Some(TerminalKeywordHighlightMatchMode::Regex) => "Regex",
        None => "missing",
    };
    let saved_color_id = saved_color.unwrap_or("missing");
    let saved_fields = format!("{saved_mode_id}|{saved_color_id}|enabled=true");
    let mut lifecycle_checks = vec![KeywordLifecycleCheck {
        action: "save-whole-word-and-custom-hex-readback",
        passed: true,
        expected: "WholeWord|#2F80ED|enabled=true".to_owned(),
        observed: saved_fields,
        elapsed_ms: None,
    }];

    let capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-keyword-saved.png"),
    )?;
    screenshots.push(capture.clone());

    let saved_rule_selector = format!("keyword-rule-{new_rule_id}");
    reveal_settings_selector(cx, root, window, "keywords", &saved_rule_selector).await?;
    click_selector(cx, window, &saved_rule_selector)?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open()
            && state.keyword_editor_open()
            && !state.keyword_editor_is_new()
            && state.keyword_editor_rule_id() == Some(new_rule_id.as_str())
            && state.keyword_editor_pattern() == Some("settings-parity-keyword")
    })
    .await?;
    reveal_settings_selector(cx, root, window, "keywords", "keyword-pattern-field").await?;
    click_and_type(
        cx,
        window,
        "keyword-pattern-field",
        "settings-parity-keyword-cancelled",
    )?;
    reveal_settings_selector(cx, root, window, "keywords", "keyword-note-field").await?;
    click_and_type(cx, window, "keyword-note-field", "discard this local edit")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open()
            && state.keyword_editor_pattern() == Some("settings-parity-keyword-cancelled")
            && state.keyword_editor_note() == Some("discard this local edit")
            && !state.keyword_save_pending()
    })
    .await?;

    let before_cancel = load_persisted_settings(services).await?;
    let before_cancel_rule = before_cancel
        .settings
        .terminal
        .keyword_highlights
        .rules
        .iter()
        .find(|rule| rule.id == new_rule_id);
    if !before_cancel_rule.is_some_and(|rule| acceptance_keyword_rule_matches(rule, true)) {
        return Err(io::Error::other(format!(
            "keyword editor changed persisted settings before Cancel for rule {new_rule_id}"
        ))
        .into());
    }

    reveal_settings_selector(cx, root, window, "keywords", "keyword-cancel").await?;
    click_selector(cx, window, "keyword-cancel")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open()
            && state.keyword_editor_open()
            && !state.keyword_editor_is_new()
            && state.keyword_editor_rule_id() == Some(new_rule_id.as_str())
            && state.keyword_editor_pattern() == Some("settings-parity-keyword")
            && state.keyword_editor_note() == Some("isolated acceptance")
            && !state.keyword_save_pending()
            && !state.draft_dirty()
            && state.draft_ready()
    })
    .await?;

    let persisted_after_cancel = load_persisted_settings(services).await?;
    let final_rule = persisted_after_cancel
        .settings
        .terminal
        .keyword_highlights
        .rules
        .iter()
        .find(|rule| rule.id == new_rule_id)
        .ok_or_else(|| io::Error::other(format!("saved keyword rule {new_rule_id} disappeared")))?;
    if persisted_after_cancel
        .settings
        .terminal
        .keyword_highlights
        .rules
        .len()
        != before_count + 1
        || !acceptance_keyword_rule_matches(final_rule, true)
    {
        return Err(io::Error::other(format!(
            "keyword cancel readback mismatch: saved rule {new_rule_id} did not retain its original fields"
        ))
        .into());
    }

    let enabled_selector = format!("keyword-rule-enabled-{new_rule_id}");
    reveal_settings_selector(cx, root, window, "keywords", &enabled_selector).await?;
    click_selector(cx, window, &enabled_selector)?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open()
            && state.page_id() == "keywords"
            && state.keyword_editor_open()
            && state.keyword_editor_rule_id() == Some(new_rule_id.as_str())
            && state.draft_keyword_rule_count() == Some(before_count + 1)
            && !state.draft_dirty()
            && state.draft_ready()
    })
    .await?;
    let disabled_settings = load_persisted_settings(services).await?;
    let disabled_rule = disabled_settings
        .settings
        .terminal
        .keyword_highlights
        .rules
        .iter()
        .find(|rule| rule.id == new_rule_id)
        .ok_or_else(|| {
            io::Error::other(format!("disabled keyword rule {new_rule_id} disappeared"))
        })?;
    if !acceptance_keyword_rule_matches(disabled_rule, false) {
        return Err(io::Error::other(format!(
            "keyword disable readback mismatch for rule {new_rule_id}"
        ))
        .into());
    }
    lifecycle_checks.push(KeywordLifecycleCheck {
        action: "disable-keyword-rule-readback",
        passed: true,
        expected: "enabled=false".to_owned(),
        observed: "enabled=false".to_owned(),
        elapsed_ms: None,
    });

    reveal_settings_selector(cx, root, window, "keywords", &enabled_selector).await?;
    click_selector(cx, window, &enabled_selector)?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open()
            && state.page_id() == "keywords"
            && state.keyword_editor_open()
            && state.keyword_editor_rule_id() == Some(new_rule_id.as_str())
            && state.draft_keyword_rule_count() == Some(before_count + 1)
            && !state.draft_dirty()
            && state.draft_ready()
    })
    .await?;
    let enabled_settings = load_persisted_settings(services).await?;
    let enabled_rule = enabled_settings
        .settings
        .terminal
        .keyword_highlights
        .rules
        .iter()
        .find(|rule| rule.id == new_rule_id)
        .ok_or_else(|| {
            io::Error::other(format!("enabled keyword rule {new_rule_id} disappeared"))
        })?;
    if !acceptance_keyword_rule_matches(enabled_rule, true) {
        return Err(io::Error::other(format!(
            "keyword enable readback mismatch for rule {new_rule_id}"
        ))
        .into());
    }
    lifecycle_checks.push(KeywordLifecycleCheck {
        action: "enable-keyword-rule-readback",
        passed: true,
        expected: "enabled=true".to_owned(),
        observed: "enabled=true".to_owned(),
        elapsed_ms: None,
    });

    let before_delete_rules = enabled_settings
        .settings
        .terminal
        .keyword_highlights
        .rules
        .clone();
    if before_delete_rules.len() != before_count + 1
        || !before_delete_rules
            .iter()
            .any(|rule| rule.id == new_rule_id && acceptance_keyword_rule_matches(rule, true))
    {
        return Err(io::Error::other(format!(
            "keyword delete baseline did not contain the saved rule {new_rule_id}"
        ))
        .into());
    }

    reveal_settings_selector(cx, root, window, "keywords", "keyword-delete").await?;
    let undo_started_at = Instant::now();
    let undo_deadline = undo_started_at + Duration::from_secs(14);
    click_selector(cx, window, "keyword-delete")?;
    wait_for_state(cx, root, window, undo_deadline, |state| {
        state.modal_open()
            && state.page_id() == "keywords"
            && state.draft_keyword_rule_count() == Some(before_count)
            && !state.draft_dirty()
            && state.draft_ready()
    })
    .await?;

    let remaining = undo_deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "keyword delete readback exceeded the 14-second undo budget",
        )
        .into());
    }
    let deleted_settings = load_persisted_settings_with_timeout(services, remaining).await?;
    let deleted_rules = &deleted_settings.settings.terminal.keyword_highlights.rules;
    let expected_remaining_order = before_delete_rules
        .iter()
        .filter(|rule| rule.id != new_rule_id)
        .map(|rule| rule.id.as_str())
        .collect::<Vec<_>>()
        .join("|");
    let observed_remaining_order = deleted_rules
        .iter()
        .map(|rule| rule.id.as_str())
        .collect::<Vec<_>>()
        .join("|");
    if deleted_rules.len() != before_delete_rules.len() - 1
        || deleted_rules.iter().any(|rule| rule.id == new_rule_id)
        || observed_remaining_order != expected_remaining_order
    {
        return Err(io::Error::other(format!(
            "keyword delete readback mismatch for rule {new_rule_id}"
        ))
        .into());
    }
    lifecycle_checks.push(KeywordLifecycleCheck {
        action: "delete-keyword-rule-readback",
        passed: true,
        expected: format!("removed={new_rule_id}|remaining={expected_remaining_order}"),
        observed: format!("removed={new_rule_id}|remaining={observed_remaining_order}"),
        elapsed_ms: None,
    });

    if Instant::now() >= undo_deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "keyword delete undo control was not activated within the acceptance budget",
        )
        .into());
    }
    reveal_settings_selector(cx, root, window, "keywords", "keyword-undo-delete").await?;
    click_selector(cx, window, "keyword-undo-delete")?;
    wait_for_state(cx, root, window, undo_deadline, |state| {
        state.modal_open()
            && state.page_id() == "keywords"
            && state.keyword_editor_open()
            && state.keyword_editor_rule_id() == Some(new_rule_id.as_str())
            && state.keyword_editor_pattern() == Some("settings-parity-keyword")
            && state.keyword_editor_note() == Some("isolated acceptance")
            && state.draft_keyword_rule_count() == Some(before_count + 1)
            && !state.draft_dirty()
            && state.draft_ready()
    })
    .await?;
    let remaining = undo_deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "keyword undo readback exceeded the 14-second acceptance budget",
        )
        .into());
    }
    let restored_settings = load_persisted_settings_with_timeout(services, remaining).await?;
    let restored_rules = &restored_settings.settings.terminal.keyword_highlights.rules;
    if restored_rules != &before_delete_rules {
        return Err(io::Error::other(format!(
            "keyword undo readback did not restore rule identity, fields, and order for {new_rule_id}"
        ))
        .into());
    }
    let undo_elapsed_ms = undo_started_at.elapsed().as_millis() as u64;
    if undo_elapsed_ms >= 15_000 {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("keyword undo completed outside the 15-second window: {undo_elapsed_ms}ms"),
        )
        .into());
    }
    let restored_order = restored_rules
        .iter()
        .map(|rule| rule.id.as_str())
        .collect::<Vec<_>>()
        .join("|");
    let expected_order = before_delete_rules
        .iter()
        .map(|rule| rule.id.as_str())
        .collect::<Vec<_>>()
        .join("|");
    let lifecycle_expected = format!("restored={new_rule_id}|order={expected_order}");
    let lifecycle_observed = format!("restored={new_rule_id}|order={restored_order}");
    lifecycle_checks.push(KeywordLifecycleCheck {
        action: "undo-keyword-deletion-within-15-seconds",
        passed: true,
        expected: lifecycle_expected,
        observed: lifecycle_observed,
        elapsed_ms: Some(undo_elapsed_ms),
    });

    Ok((
        SettingCheck {
            action: "save-keyword-and-cancel-unsaved-edit",
            passed: true,
            expected: "settings-parity-keyword|isolated acceptance".to_owned(),
            observed: format!("{}|{}", final_rule.pattern, final_rule.note),
            capture: Some(capture),
        },
        lifecycle_checks,
    ))
}

/// Selects a production category and waits for protocol reads so screenshots retain settled runtime state.
async fn ensure_page(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    page_id: &str,
    expected_section: &str,
) -> Result<SettingsAcceptanceState, Box<dyn Error>> {
    let current = root_state(cx, root, window)?;
    if !current.modal_open() {
        return Err(io::Error::other("settings modal was unexpectedly closed").into());
    }
    if current.page_id() != expected_section {
        let selector = format!("preferences-nav-{page_id}");
        reveal_navigation_selector(cx, window, &selector).await?;
        click_selector(cx, window, &selector)?;
    }
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open()
            && state.page_id() == expected_section
            && (page_id != "external-launch" || state.protocol_settled())
            && (page_id != "sync" || state.sync_ready())
    })
    .await
}

/// 紧凑窗口会裁切侧栏下方项目；按真实裁切边界滚动，不能仅凭窗口内坐标判断可点击。
///
/// @author kongweiguang
async fn reveal_navigation_selector(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<(), Box<dyn Error>> {
    for _ in 0..16 {
        let revealed = window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            draw_acceptance_frame(window, app);
            let viewport = selector_bounds(window, "preferences-navigation-viewport")?;
            let id: ElementId = selector.to_owned().into();
            let target = gpui_kit::base::test_support::find(window, &[], &id)
                .ok_or_else(|| io::Error::other(format!("missing navigation {selector}")))?
                .bounds();
            if bounds_inside_viewport(target, viewport) {
                return Ok::<_, io::Error>(true);
            }
            let delta = if target.origin.y < viewport.origin.y {
                180.
            } else {
                -180.
            };
            let event = ScrollWheelEvent {
                position: viewport.center(),
                delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
                touch_phase: TouchPhase::Moved,
                ..ScrollWheelEvent::default()
            };
            let _ = window.dispatch_event(event.to_platform_input(), app);
            Ok(false)
        })??;
        if revealed {
            return Ok(());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
    Err(io::Error::other(format!(
        "navigation {selector} did not enter its scroll viewport"
    ))
    .into())
}

/// 等待受限状态并在超时时输出安全运行态，便于区分未保存、主题投影和终端快照问题。
///
/// @author kongweiguang
async fn wait_for_state<F>(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    deadline: Instant,
    mut condition: F,
) -> Result<SettingsAcceptanceState, Box<dyn Error>>
where
    F: FnMut(&SettingsAcceptanceState) -> bool,
{
    loop {
        let state = root_state(cx, root, window)?;
        if condition(&state) {
            return Ok(state);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "settings state wait timed out: page={}, modal={}, ready={}, loading={}, draft_ready={}, dirty={}, theme={:?}, app_draft={:?}, app_applied={:?}, kit_matches={}, terminal_generation={}, rendered_terminal={}, sessions={}, density={:?}, rules={:?}, language={:?}, selected_language={:?}, protocol_settled={}, sync_ready={}, sync_busy={}, sync_outcome={:?}, sync_error_present={}",
                state.page_id(),
                state.modal_open(),
                state.settings_ready(),
                state.settings_loading(),
                state.draft_ready(),
                state.draft_dirty(),
                state.draft_theme_mode(),
                state.draft_app_color_scheme(),
                state.applied_app_color_scheme(),
                state.kit_theme_matches_app_color(),
                state.terminal_palette_update_generation(),
                state.rendered_terminal_palette().is_some(),
                state.ready_local_session_ids().len(),
                state.draft_density(),
                state.draft_keyword_rule_count(),
                state.draft_interface_language(),
                state.selected_interface_language(),
                state.protocol_settled(),
                state.sync_ready(),
                state.sync_busy(),
                state.sync_result().map(|result| result.outcome),
                state.sync_error().is_some(),
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 为有明确目标的状态等待附加阶段，避免超时只能从最终 Root 快照猜测失败步骤。
///
/// @author kongweiguang
async fn wait_for_state_with_stage<F>(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    deadline: Instant,
    stage: &str,
    condition: F,
) -> Result<SettingsAcceptanceState, Box<dyn Error>>
where
    F: FnMut(&SettingsAcceptanceState) -> bool,
{
    match wait_for_state(cx, root, window, deadline, condition).await {
        Ok(state) => Ok(state),
        Err(error) => Err(io::Error::other(format!("{stage}: {error}")).into()),
    }
}

/// 先驱动隐藏窗口绘制生产 Root 帧，再读取状态，保证 render 派生选择值已经同步。
///
/// @author kongweiguang
fn root_state(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
) -> Result<SettingsAcceptanceState, Box<dyn Error>> {
    Ok(window.update(cx, |_, window, app| {
        window.refresh();
        draw_acceptance_frame(window, app);
        root.update(app, |root, cx| root.settings_acceptance_state(cx))
    })?)
}

/// 在隐藏验收窗口中显式执行 GPUI 下一帧回调，并在回调更新状态时补绘一帧。
///
/// @author kongweiguang
fn draw_acceptance_frame(window: &mut gpui_pre::Window, app: &mut App) {
    let frame = window.draw(app);
    frame.clear(app);
    if window.simulate_next_frame(app) > 0 {
        let frame = window.draw(app);
        frame.clear(app);
    }
}

/// 校验 Root 的逻辑尺寸符合请求，并确认每张 PNG 使用当前显示缩放对应的栅格尺寸。
///
/// @author kongweiguang
fn verify_capture_dimensions(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    width: u32,
    height: u32,
    captures: &[CaptureRecord],
) -> Result<(), Box<dyn Error>> {
    let (logical_width, logical_height, scale_factor) = window.update(cx, |_, window, _| {
        (
            window.bounds().size.width.as_f32(),
            window.bounds().size.height.as_f32(),
            window.scale_factor(),
        )
    })?;
    if logical_width != width as f32 || logical_height != height as f32 {
        return Err(io::Error::other(format!(
            "settings window logical size mismatch: requested={}x{}, observed={logical_width:.1}x{logical_height:.1}",
            width, height
        ))
        .into());
    }
    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return Err(io::Error::other(format!(
            "settings window scale factor is invalid: {scale_factor}"
        ))
        .into());
    }
    let expected_width = (width as f32 * scale_factor).round() as u32;
    let expected_height = (height as f32 * scale_factor).round() as u32;
    if let Some(capture) = captures
        .iter()
        .find(|capture| capture.width != expected_width || capture.height != expected_height)
    {
        return Err(io::Error::other(format!(
            "settings screenshot size mismatch: file={}, expected={}x{} at scale {scale_factor:.3}, observed={}x{}",
            capture.file, expected_width, expected_height, capture.width, capture.height
        ))
        .into());
    }
    Ok(())
}

/// Resolves Kit controls from the current observation frame, using debug selectors only for plain wrappers.
fn click_selector(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        window.refresh();
        let frame = window.draw(app);
        frame.clear(app);
        let bounds = selector_bounds(window, selector)?;
        if !bounds_inside_viewport(bounds, window.visual_viewport_bounds()) {
            return Err(io::Error::other(format!(
                "selector {selector:?} is outside the visible viewport"
            )));
        }
        let position = bounds.center();
        for event in [
            MouseMoveEvent {
                position,
                ..MouseMoveEvent::default()
            }
            .to_platform_input(),
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..MouseDownEvent::default()
            }
            .to_platform_input(),
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..MouseUpEvent::default()
            }
            .to_platform_input(),
        ] {
            let _ = window.dispatch_event(event, app);
        }
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// Uses an observed child fully inside the production body clip so wheel events cannot hit the header or sidebar.
fn scroll_settings_content(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    page_id: &str,
    delta: f32,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        window.refresh();
        let frame = window.draw(app);
        frame.clear(app);
        let viewport = root.update(app, |root, cx| root.settings_acceptance_content_bounds(cx));
        let position = observed_settings_scroll_position(window, page_id, viewport)?;
        let event = ScrollWheelEvent {
            position,
            delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
            touch_phase: TouchPhase::Moved,
            ..ScrollWheelEvent::default()
        };
        let _ = window.dispatch_event(event.to_platform_input(), app);
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// Resolves Kit observation first; debug selectors are reserved for plain layout wrappers with no native control ID.
fn selector_bounds(
    window: &gpui_pre::Window,
    selector: &str,
) -> Result<Bounds<gpui_pre::Pixels>, io::Error> {
    let selector_id: ElementId = selector.to_owned().into();
    let input_id: ElementId = "input".into();
    let element = gpui_kit::base::test_support::find(window, &[], &selector_id).or_else(|| {
        gpui_kit::base::test_support::find(window, std::slice::from_ref(&selector_id), &input_id)
    });
    if let Some(element) = element {
        if !element.visible() {
            return Err(io::Error::other(format!(
                "observed ElementId {selector:?} is not visible"
            )));
        }
        return Ok(element.bounds());
    }
    window
        .debug_selector_bounds(selector)
        .ok_or_else(|| {
            let registered = gpui_kit::base::test_support::registered_paths(window);
            io::Error::other(format!(
                "missing observed ElementId or debug selector {selector:?}; registered paths: {registered}"
            ))
        })
}

/// Maps each settings section to the real scroll container already present in its production page tree.
fn settings_scroll_root(page_id: &str) -> Option<&'static str> {
    match page_id {
        "appearance" => Some("preferences-appearance"),
        "terminal" => Some("preferences-terminal"),
        "profiles" => Some("preferences-profile-form-scroll"),
        "keywords" => Some("settings-keyword-highlights-panel"),
        "suggestions" => Some("settings-suggestions-panel"),
        "sftp" => Some("preferences-sftp"),
        "external-launch" => Some("preferences-external-launch"),
        "mcp" => Some("preferences-mcp-settings"),
        "sync" => Some("preferences-configuration-sync"),
        "desktop" => Some("settings-desktop-page"),
        "shortcuts" => Some("settings-shortcuts-page"),
        "about" => Some("settings-about-page"),
        _ => None,
    }
}

/// Chooses a child within the body clip, excluding nested JSON scrolling so the page itself consumes the wheel.
fn observed_settings_scroll_position(
    window: &gpui_pre::Window,
    page_id: &str,
    viewport: Bounds<gpui_pre::Pixels>,
) -> Result<gpui_pre::Point<gpui_pre::Pixels>, io::Error> {
    let root_name = settings_scroll_root(page_id)
        .ok_or_else(|| io::Error::other(format!("unknown settings scroll page {page_id:?}")))?;
    let root_id: gpui_pre::ElementId = root_name.into();
    let nested_scroll_root = match page_id {
        "mcp" => Some(gpui_pre::ElementId::from("mcp-json-preview")),
        _ => None,
    };
    let viewport_center = viewport.center();
    let mut nearest: Option<(f32, gpui_pre::Point<gpui_pre::Pixels>)> = None;

    for element in gpui_kit::base::test_support::snapshots(window) {
        if !element.visible()
            || !element.path().contains(&root_id)
            || nested_scroll_root
                .as_ref()
                .is_some_and(|nested_id| element.path().contains(nested_id))
        {
            continue;
        }
        let bounds = element.bounds();
        if !bounds_inside_viewport(bounds, viewport) {
            continue;
        }
        let center = bounds.center();
        let delta_x = center.x.as_f32() - viewport_center.x.as_f32();
        let delta_y = center.y.as_f32() - viewport_center.y.as_f32();
        let distance_squared = delta_x * delta_x + delta_y * delta_y;
        if nearest
            .as_ref()
            .is_none_or(|(nearest_distance, _)| distance_squared < *nearest_distance)
        {
            nearest = Some((distance_squared, center));
        }
    }

    nearest.map(|(_, position)| position).ok_or_else(|| {
        let registered = gpui_kit::base::test_support::registered_paths(window);
        io::Error::other(format!(
            "settings page {page_id:?} has no visible observed control for scrolling; registered paths: {registered}"
        ))
    })
}

/// 点击后先绘制焦点交接帧，再通过真实按键清空和输入；隐藏窗口没有系统自动补帧，
/// 不显式绘制会把文字送给上一帧的输入处理器，造成原生验收伪失败。
///
/// @author kongweiguang
fn click_and_type(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
    value: &str,
) -> Result<(), Box<dyn Error>> {
    click_selector(cx, window, selector)?;
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        window.refresh();
        draw_acceptance_frame(window, app);
        Ok::<_, io::Error>(())
    })??;
    press_key(cx, window, "secondary-a")?;
    press_key(cx, window, "backspace")?;
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        for character in value.chars() {
            let key = character.to_string();
            let keystroke = Keystroke {
                modifiers: gpui_pre::Modifiers::default(),
                key: key.clone(),
                key_char: Some(key),
            };
            window.dispatch_keystroke(keystroke, app);
        }
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// Dispatches a named GPUI keystroke to the focused production control.
fn press_key(cx: &mut AsyncApp, window: AnyWindowHandle, key: &str) -> Result<(), Box<dyn Error>> {
    let keystroke = Keystroke::parse(key)
        .map_err(|error| io::Error::other(format!("invalid acceptance key {key}: {error}")))?;
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        window.dispatch_keystroke(keystroke, app);
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// Draws hidden-window next-frame callbacks before screenshotting, since no visible compositor frame loop runs.
fn save_frame(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    path: PathBuf,
) -> Result<CaptureRecord, Box<dyn Error>> {
    let file = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid screenshot filename"))?
        .to_owned();
    let (width, height, pixel_sha256) = window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        let image = window.render_to_image().map_err(io::Error::other)?;
        let width = image.width();
        let height = image.height();
        if width == 0 || height == 0 {
            return Err(io::Error::other(
                "native renderer returned a zero-sized image",
            ));
        }
        if !image
            .as_raw()
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] != 0)
        {
            return Err(io::Error::other(
                "native renderer returned a fully transparent image",
            ));
        }
        let hash = format!("{:x}", Sha256::digest(image.as_raw()));
        image.save(&path).map_err(io::Error::other)?;
        Ok::<_, io::Error>((width, height, hash))
    })??;
    Ok(CaptureRecord {
        file,
        width,
        height,
        pixel_sha256,
    })
}

/// Confirms test clicks target rendered controls within the visible client viewport.
fn bounds_inside_viewport(
    bounds: Bounds<gpui_pre::Pixels>,
    viewport: Bounds<gpui_pre::Pixels>,
) -> bool {
    !bounds.is_empty()
        && bounds.origin.x >= viewport.origin.x
        && bounds.origin.y >= viewport.origin.y
        && bounds.origin.x + bounds.size.width <= viewport.origin.x + viewport.size.width
        && bounds.origin.y + bounds.size.height <= viewport.origin.y + viewport.size.height
}

/// Reuses the native isolation proof: visible windows always fail, while active windows pass only on a proven private desktop.
fn ensure_hidden_window(window: &gpui_pre::Window) -> Result<(), io::Error> {
    let evidence = crate::right_rail_acceptance::isolation::capture()?;
    ensure_hidden_state(&evidence, window.is_visible(), window.is_window_active())
}

/// Routes the native probe and deterministic tests through one isolation policy.
fn ensure_hidden_state(
    evidence: &crate::right_rail_acceptance::isolation::DesktopIsolationEvidence,
    visible: bool,
    active: bool,
) -> Result<(), io::Error> {
    evidence.validate_window_state(visible, active)
}

/// Maps the tested enum to the same lowercase value written by settings.
fn density_id(value: InterfaceDensity) -> &'static str {
    match value {
        InterfaceDensity::Compact => "compact",
        InterfaceDensity::Comfortable => "comfortable",
        InterfaceDensity::Spacious => "spacious",
    }
}

/// Maps theme modes to the stable lower-case settings representation used by readback checks.
fn theme_mode_id(value: ThemeMode) -> &'static str {
    match value {
        ThemeMode::System => "system",
        ThemeMode::Light => "light",
        ThemeMode::Dark => "dark",
    }
}

/// Stores only the first failure so shutdown errors cannot hide the interaction that failed.
fn set_failure(failure: &Arc<Mutex<Option<String>>>, message: String) {
    let mut failure = failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if failure.is_none() {
        *failure = Some(message);
    }
}

#[cfg(test)]
#[path = "../tests/unit/settings_acceptance_args.rs"]
mod args_tests;
