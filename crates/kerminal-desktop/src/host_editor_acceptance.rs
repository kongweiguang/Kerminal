//! Isolated native acceptance for the production host editor and probe lifecycle.
//!
//! @author kongweiguang

use std::{
    error::Error,
    ffi::OsStr,
    fs, io,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, AsyncApp, Entity, TitlebarOptions,
    WindowBackgroundAppearance, WindowOptions, component::TitleBar,
};
use gpui_pre::{
    App, Bounds, InputEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    WindowBounds, px, size,
};
use kerminal_application::{
    AppError, AppResult,
    connections::probe::{
        ConnectionProbeBackend, ConnectionProbeOutcome, ConnectionProbeService,
        ConnectionProbeTarget, ProbeVerificationLevel,
    },
    connections::{SerialPortDescriptor, SerialPortRepository, SerialPortService},
    preferences::{InterfaceLanguage, ThemeMode},
};
use kerminal_domain::connections::{RemoteProtocol, SshAuthentication};
use kerminal_ui::features::connections::host_editor_acceptance::HostEditorAcceptanceState;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{composition::DesktopComposition, runtime::ApplicationRuntime};

const ACTION_TIMEOUT: Duration = Duration::from_secs(8);
const LONG_PROBE_ERROR: &str = "连接测试未能完成：这是隔离验收使用的合成错误，不涉及真实服务器。请检查主机地址、端口、代理与跳板配置，并确认身份认证材料有效。错误详情应在独立弹层中换行显示，长文本不得挤压主机表单、测试按钮或保存按钮；取消、修改参数和重新测试后也不应残留旧结果。";
const DEFAULT_WIDTH: u32 = 1360;
const DEFAULT_HEIGHT: u32 = 900;
const MIN_WIDTH: u32 = 800;
const MAX_WIDTH: u32 = 3840;
const MIN_HEIGHT: u32 = 600;
const MAX_HEIGHT: u32 = 2160;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(12);
const REPORT_FILE: &str = "report.json";

const THEMES: [(ThemeMode, &str); 3] = [
    (ThemeMode::Light, "light"),
    (ThemeMode::Dark, "dark"),
    (ThemeMode::System, "system"),
];

const HOST_TYPES: [(Option<RemoteProtocol>, &str); 6] = [
    (Some(RemoteProtocol::Ssh), "ssh"),
    (Some(RemoteProtocol::Sftp), "sftp"),
    (None, "local"),
    (Some(RemoteProtocol::Rdp), "rdp"),
    (Some(RemoteProtocol::Telnet), "telnet"),
    (Some(RemoteProtocol::Serial), "serial"),
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
    probe_fixture: Arc<ControlledProbeFixture>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AcceptanceReport {
    schema_version: u32,
    locale: &'static str,
    capture_mode: &'static str,
    native_isolation: crate::right_rail_acceptance::isolation::DesktopIsolationEvidence,
    config_directory: String,
    probe_fixture: &'static str,
    probe_requests_started: usize,
    probe_requests_completed: usize,
    serial_fixture: &'static str,
    window_width: f32,
    window_height: f32,
    scale_factor: f32,
    captures: Vec<CaptureRecord>,
    probe_lifecycle: Vec<ProbeLifecycleRecord>,
    private_key_sources: Vec<PrivateKeySourceRecord>,
    long_error: LongErrorRecord,
    persistence: Vec<PersistenceRecord>,
    dense_scenes: Vec<DenseSceneRecord>,
    navigation_mode: &'static str,
    input_preparation: &'static str,
    layout_stability_scope: &'static str,
    window_geometry_stable: bool,
    layout_stable: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CaptureRecord {
    theme: &'static str,
    host_type: &'static str,
    section: &'static str,
    interaction_mode: &'static str,
    file: String,
    width: u32,
    height: u32,
    pixel_sha256: String,
    window_width: f32,
    window_height: f32,
    viewport_width: f32,
    viewport_height: f32,
    scale_factor: f32,
    editor_geometry: EditorGeometry,
}

#[derive(Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeometryRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[derive(Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct EditorGeometry {
    body: GeometryRect,
    status_lane: GeometryRect,
    save_button: GeometryRect,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistenceRecord {
    host_type: &'static str,
    saved_id: String,
    saved_by: &'static str,
    reopened_by: &'static str,
    readback_matches: bool,
    rename_only_readback_matches: bool,
    capture_file: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProbeLifecycleRecord {
    stage: &'static str,
    running: bool,
    outcome: Option<String>,
    cancelled: bool,
    error_present: bool,
    window_width: f32,
    window_height: f32,
    editor_geometry: EditorGeometry,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PrivateKeySourceRecord {
    source: &'static str,
    selected_by: &'static str,
    capture_file: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LongErrorRecord {
    requests_started: usize,
    message_preserved: bool,
    message_character_count: usize,
    details_popup_open: bool,
    capture_file: String,
    editor_geometry: EditorGeometry,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DenseSceneRecord {
    scene: &'static str,
    host_type: &'static str,
    native_interactions: Vec<String>,
    input_preparation: &'static str,
    capture_file: String,
    baseline_geometry: EditorGeometry,
    editor_geometry: EditorGeometry,
    footer_stable: bool,
    proxy_enabled: bool,
    jump_count: usize,
    second_jump_private_key: bool,
    rdp_windowed: bool,
    prepared_values_match: bool,
    scroll_offset_y: f32,
    second_jump_lower_focused: bool,
    second_jump_lower_visible: bool,
}

struct DenseSceneSetup {
    scene: &'static str,
    host_type: &'static str,
    baseline_geometry: EditorGeometry,
    native_interactions: Vec<String>,
    input_preparation: &'static str,
}

/// 在新配置根中装配生产主机面板，并替换探测和串口目录为可控 fixture。
///
/// @author kongweiguang
pub fn run() -> Result<(), Box<dyn Error>> {
    let options = parse_options(std::env::args_os().skip(2))?;
    let output_directory = match options.output_directory {
        Some(path) => create_requested_output_directory(&path)?,
        None => create_unique_output_directory(
            Path::new("target")
                .join("diagnostics")
                .join("host-editor-acceptance"),
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
    let probe_fixture = Arc::new(ControlledProbeFixture {
        requests: AtomicUsize::new(0),
        completed: AtomicUsize::new(0),
        released: AtomicUsize::new(0),
        gates: std::array::from_fn(|_| tokio::sync::Semaphore::new(0)),
    });
    let probe = Arc::new(ConnectionProbeService::new(probe_fixture.clone()));
    let serial_ports = Arc::new(SerialPortService::new(Arc::new(FixtureSerialPorts)));
    let context = AcceptanceContext {
        output_directory: output_directory.clone(),
        width: options.width.unwrap_or(DEFAULT_WIDTH),
        height: options.height.unwrap_or(DEFAULT_HEIGHT),
        probe_fixture: Arc::clone(&probe_fixture),
    };
    let ui_result = catch_unwind(AssertUnwindSafe(|| {
        run_window(services, probe, serial_ports, context)
    }))
    .unwrap_or_else(|_| Err(io::Error::other("host editor acceptance event loop panicked").into()));
    probe_fixture.release_remaining();
    let shutdown_result = runtime
        .block_on(composition.shutdown_until(tokio::time::Instant::now() + SHUTDOWN_TIMEOUT));
    runtime.shutdown();

    ui_result?;
    shutdown_result?;
    println!(
        "Host editor acceptance report: {}",
        output_directory.join(REPORT_FILE).display()
    );
    Ok(())
}

/// 创建输出目录前拒绝重复、格式错误和未知参数。
/// @author kongweiguang
fn parse_options(
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<AcceptanceOptions, io::Error> {
    let mut arguments = arguments.into_iter();
    let mut options = AcceptanceOptions::default();
    while let Some(flag) = arguments.next() {
        if flag == OsStr::new("--output-dir") {
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
        }
        let (slot, name, min, max) = if flag == OsStr::new("--width") {
            (&mut options.width, "--width", MIN_WIDTH, MAX_WIDTH)
        } else if flag == OsStr::new("--height") {
            (&mut options.height, "--height", MIN_HEIGHT, MAX_HEIGHT)
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "supported options: --host-editor-acceptance [--output-dir <new-dir>] [--width <logical-px>] [--height <logical-px>]",
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
        *slot = Some(parse_dimension(value, name, min, max)?);
    }
    Ok(options)
}

/// 限制逻辑窗口尺寸，使隔离截图使用可预测的表单视口。
/// @author kongweiguang
fn parse_dimension(
    value: std::ffi::OsString,
    name: &str,
    min: u32,
    max: u32,
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
    if !(min..=max).contains(&dimension) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} range is {min}..={max} logical pixels"),
        ));
    }
    Ok(dimension)
}

/// 只创建新的末级输出目录，保留既有验收报告和截图。
/// @author kongweiguang
fn create_requested_output_directory(path: &Path) -> Result<PathBuf, io::Error> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if let Some(parent) = absolute.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(&absolute)?;
    Ok(absolute)
}

/// 分配带时间戳的唯一目录，避免覆盖先前的验收结果。
/// @author kongweiguang
fn create_unique_output_directory(parent: PathBuf) -> Result<PathBuf, io::Error> {
    fs::create_dir_all(&parent)?;
    for _ in 0..100 {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis();
        let candidate = parent.join(format!("run-{timestamp}-{}", std::process::id()));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique host editor output directory",
    ))
}

/// 打开隐藏原生窗口，由生产编辑器负责主机表单与协议切换。
/// @author kongweiguang
fn run_window(
    services: kerminal_ui::AppServices,
    probe: Arc<ConnectionProbeService>,
    serial_ports: Arc<SerialPortService>,
    context: AcceptanceContext,
) -> Result<(), Box<dyn Error>> {
    let hosts = services
        .hosts
        .clone()
        .ok_or_else(|| io::Error::other("桌面服务未提供主机管理用例"))?;
    let remote = services
        .remote_connections
        .clone()
        .ok_or_else(|| io::Error::other("桌面服务未提供远程连接用例"))?;
    let rdp = services.rdp_launcher.clone();
    let executor = services.executor.clone();
    let profiles = services.profiles.clone();
    let failure = Arc::new(Mutex::new(None::<String>));
    let failure_for_app = Arc::clone(&failure);
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            kerminal_ui::init(cx);
            kerminal_ui::locale::apply_interface_language(InterfaceLanguage::ZhCn, "zh-CN");
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
                window_min_size: Some(size(px(MIN_WIDTH as f32), px(MIN_HEIGHT as f32))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Kerminal Host Editor Acceptance".into()),
                    ..TitleBar::title_bar_options()
                }),
                show: false,
                focus: false,
                ..TitleBar::window_options()
            };
            let result = gpui_kit::open_window(window_options, cx, |window, cx| {
                window.set_background_appearance(WindowBackgroundAppearance::Transparent);
                window.resize(size(px(context.width as f32), px(context.height as f32)));
                let state = cx.new(|cx| {
                    HostEditorAcceptanceState::new(
                        hosts,
                        remote,
                        rdp,
                        executor,
                        profiles,
                        Arc::clone(&probe),
                        Arc::clone(&serial_ports),
                        window,
                        cx,
                    )
                });
                let window_handle = window.window_handle();
                let capture_failure = Arc::clone(&failure_for_app);
                let capture_state = state.clone();
                let capture_context = AcceptanceContext {
                    output_directory: context.output_directory.clone(),
                    width: context.width,
                    height: context.height,
                    probe_fixture: Arc::clone(&context.probe_fixture),
                };
                cx.spawn(async move |cx| {
                    if let Err(error) =
                        capture_acceptance(cx, capture_state, window_handle, capture_context).await
                    {
                        set_failure(&capture_failure, error);
                    }
                    cx.update(|cx| cx.quit());
                })
                .detach();
                state
            });
            if let Err(error) = result {
                set_failure(&failure_for_app, error.to_string());
                cx.quit();
            }
        });
    match failure.lock() {
        Ok(mut failure) => match failure.take() {
            Some(error) => Err(io::Error::other(error).into()),
            None => Ok(()),
        },
        Err(_) => Err(io::Error::other("host editor acceptance failure state was poisoned").into()),
    }
}

/// 将请求的逻辑视口置于隔离桌面的主显示器中心。
/// @author kongweiguang
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

/// 先等待隐藏原生 GPU 目标就绪，再覆盖全部分类；逻辑窗口尺寸不能替代实际渲染目标。
/// 探测门闩明确控制旧结果的释放顺序，中文场景与保存均使用本轮隔离配置。
/// @author kongweiguang
async fn capture_acceptance(
    cx: &mut AsyncApp,
    state: Entity<HostEditorAcceptanceState>,
    window: AnyWindowHandle,
    context: AcceptanceContext,
) -> Result<(), String> {
    crate::settings_acceptance::prepare_capture_surface_for_title(
        cx,
        window,
        context.width,
        context.height,
        "Kerminal Host Editor Acceptance",
    )
    .await
    .map_err(|error| error.to_string())?;
    let mut captures = Vec::with_capacity(74);
    let mut reference_layout = None;
    for (theme, theme_name) in THEMES {
        apply_theme(cx, window, theme)?;
        for (protocol, host_type) in HOST_TYPES {
            let native_protocol = select_protocol(cx, state.clone(), window, protocol).await?;
            let sections = window
                .update(cx, |_, _, app| state.read(app).sections(app))
                .map_err(|error| error.to_string())?;
            if sections.is_empty() {
                return Err(format!("{host_type} editor has no production sections"));
            }
            for section in sections {
                let native_section =
                    select_section(cx, state.clone(), window, protocol, section).await?;
                let interaction_mode = if native_protocol && native_section {
                    "native-pointer"
                } else {
                    "production-method (compact navigation)"
                };
                let capture = save_frame(
                    cx,
                    window,
                    context
                        .output_directory
                        .join(format!("host-{theme_name}-{host_type}-{section}.png")),
                    theme_name,
                    host_type,
                    section,
                    interaction_mode,
                )?;
                reference_layout.get_or_insert((
                    capture.window_width,
                    capture.window_height,
                    capture.viewport_width,
                    capture.viewport_height,
                    capture.scale_factor,
                ));
                captures.push(capture);
            }
        }
    }

    select_protocol(cx, state.clone(), window, Some(RemoteProtocol::Ssh)).await?;
    select_section(
        cx,
        state.clone(),
        window,
        Some(RemoteProtocol::Ssh),
        "general",
    )
    .await?;
    let private_key_sources =
        capture_private_key_sources(cx, state.clone(), window, &context, &mut captures).await?;
    window
        .update(cx, |_, window, app| {
            state.update(app, |state, cx| {
                state.set_probe_target("fixture-old.invalid", window, cx)
            })
        })
        .map_err(|error| error.to_string())?
        .then_some(())
        .ok_or_else(|| "SSH acceptance editor did not become ready".to_owned())?;

    settle_frame(cx).await;
    let before = capture_probe_state(
        cx,
        state.clone(),
        window,
        context.output_directory.join("probe-before.png"),
        "before",
    )?;
    captures.push(before.0);
    let mut lifecycle = vec![before.1];
    require_click(cx, window, "connections-editor-probe")?;

    wait_for_probe(cx, state.clone(), window, |running, _| running).await?;
    wait_for_fixture(cx, &context.probe_fixture, 1, false).await?;
    let running = capture_probe_state(
        cx,
        state.clone(),
        window,
        context.output_directory.join("probe-running.png"),
        "running",
    )?;
    captures.push(running.0);
    lifecycle.push(running.1);

    require_click(cx, window, "connections-editor-probe")?;
    wait_for_probe(cx, state.clone(), window, |running, _| !running).await?;
    let cancelled = capture_probe_state(
        cx,
        state.clone(),
        window,
        context.output_directory.join("probe-cancelled.png"),
        "cancelled",
    )?;
    captures.push(cancelled.0);
    if cancelled.1.outcome.is_some() || cancelled.1.error_present || !cancelled.1.cancelled {
        return Err(
            "cancellation produced a success or failure outcome instead of neutral feedback"
                .to_owned(),
        );
    }
    lifecycle.push(cancelled.1);

    require_click(cx, window, "connections-editor-probe")?;
    wait_for_probe(cx, state.clone(), window, |running, _| running).await?;
    wait_for_fixture(cx, &context.probe_fixture, 2, false).await?;
    let retry_running = capture_probe_state(
        cx,
        state.clone(),
        window,
        context.output_directory.join("probe-retry-running.png"),
        "retry-running",
    )?;
    captures.push(retry_running.0);
    lifecycle.push(retry_running.1);

    window
        .update(cx, |_, window, app| {
            state.update(app, |state, cx| {
                state.change_probe_host("fixture-new.invalid", window, cx)
            })
        })
        .map_err(|error| error.to_string())?
        .then_some(())
        .ok_or_else(|| "SSH acceptance editor closed while changing its probe target".to_owned())?;
    wait_for_probe(cx, state.clone(), window, |running, outcome| {
        !running && outcome.is_none()
    })
    .await?;
    let changed = capture_probe_state(
        cx,
        state.clone(),
        window,
        context.output_directory.join("probe-input-changed.png"),
        "input-changed",
    )?;
    captures.push(changed.0);
    lifecycle.push(changed.1);

    require_click(cx, window, "connections-editor-probe")?;
    wait_for_probe(cx, state.clone(), window, |running, _| running).await?;
    wait_for_fixture(cx, &context.probe_fixture, 3, false).await?;
    context.probe_fixture.release(2)?;
    wait_for_probe(cx, state.clone(), window, |running, outcome| {
        !running
            && matches!(
                outcome,
                Some(ConnectionProbeOutcome::Verified(
                    ProbeVerificationLevel::SshAuthenticated
                ))
            )
    })
    .await?;
    let retry = capture_probe_state(
        cx,
        state.clone(),
        window,
        context.output_directory.join("probe-retry.png"),
        "retry",
    )?;
    captures.push(retry.0);
    lifecycle.push(retry.1);

    context.probe_fixture.release(0)?;
    context.probe_fixture.release(1)?;
    wait_for_fixture(cx, &context.probe_fixture, 3, true).await?;
    settle_frame(cx).await;
    let final_state = capture_probe_state(
        cx,
        state.clone(),
        window,
        context.output_directory.join("probe-late-response.png"),
        "late-response",
    )?;
    captures.push(final_state.0);
    if final_state.1.outcome.as_deref() != Some("ssh-authenticated") {
        return Err("late canceled probe replaced the successful retry outcome".to_owned());
    }
    lifecycle.push(final_state.1);

    let layout_stable = lifecycle
        .iter()
        .all(|record| record.editor_geometry == lifecycle[0].editor_geometry);
    if !layout_stable {
        return Err(
            "editor body, status lane or save button moved during probe lifecycle".to_owned(),
        );
    }

    let long_error = capture_long_error(cx, state.clone(), window, &context, &mut captures).await?;
    if long_error.editor_geometry != lifecycle[0].editor_geometry {
        return Err(
            "long error details moved the editor body, status lane or save button".to_owned(),
        );
    }
    let persistence =
        capture_persistence(cx, state.clone(), window, &context, &mut captures).await?;
    let dense_scenes = capture_dense_scenes(cx, state, window, &context, &mut captures).await?;

    let window_geometry_stable = captures.iter().all(|capture| {
        reference_layout.is_some_and(|reference| {
            (
                capture.window_width,
                capture.window_height,
                capture.viewport_width,
                capture.viewport_height,
                capture.scale_factor,
            ) == reference
        })
    });
    if !window_geometry_stable {
        return Err(
            "window or viewport dimensions changed during host editor acceptance".to_owned(),
        );
    }

    let (window_width, window_height, scale_factor) = window
        .update(cx, |_, window, _| {
            (
                window.bounds().size.width.as_f32(),
                window.bounds().size.height.as_f32(),
                window.scale_factor(),
            )
        })
        .map_err(|error| error.to_string())?;
    let native_isolation = window
        .update(cx, |_, _, _| {
            crate::right_rail_acceptance::isolation::capture()
        })
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    let report = AcceptanceReport {
        schema_version: 2,
        locale: "zh-CN",
        capture_mode: "hidden-native-window",
        native_isolation,
        config_directory: context
            .output_directory
            .join("config")
            .display()
            .to_string(),
        probe_fixture: "three independent gates; cancelled old requests released only after SSH retry succeeds; no network access",
        probe_requests_started: context.probe_fixture.requests.load(Ordering::SeqCst),
        probe_requests_completed: context.probe_fixture.completed.load(Ordering::SeqCst),
        serial_fixture: "two synthetic serial descriptors; no device access",
        window_width,
        window_height,
        scale_factor,
        captures,
        probe_lifecycle: lifecycle,
        private_key_sources,
        long_error,
        persistence,
        dense_scenes,
        navigation_mode: "native GPUI pointer events; compact menus use explicitly recorded production-method navigation",
        input_preparation: "InputState::set_value for setup; InputState::replace_all emits production Change invalidation; authentication and private-key source use production selection methods; does not establish OS typing, IME or Radio pointer behavior",
        layout_stability_scope: "same SSH general page before, during and after probes; actual body, status-lane and save-button bounds",
        window_geometry_stable,
        layout_stable,
    };
    let report_path = context.output_directory.join(REPORT_FILE);
    let report_bytes = serde_json::to_vec_pretty(&report).map_err(|error| error.to_string())?;
    fs::write(report_path, report_bytes).map_err(|error| error.to_string())?;
    Ok(())
}

/// 两种私钥来源由生产入口准备并分别截图；报告明确区分状态装配与真实 Radio 鼠标交互。
/// @author kongweiguang
async fn capture_private_key_sources(
    cx: &mut AsyncApp,
    state: Entity<HostEditorAcceptanceState>,
    window: AnyWindowHandle,
    context: &AcceptanceContext,
    captures: &mut Vec<CaptureRecord>,
) -> Result<Vec<PrivateKeySourceRecord>, String> {
    let mut sources = Vec::with_capacity(2);
    for (paste, source, section) in [
        (false, "file", "private-key-file"),
        (true, "paste", "private-key-paste"),
    ] {
        let selected = window
            .update(cx, |_, window, app| {
                state.update(app, |state, cx| {
                    state.prepare_private_key_source(paste, window, cx)
                })
            })
            .map_err(|error| error.to_string())?;
        if !selected {
            return Err(format!("private-key source {source} did not become active"));
        }
        settle_frame(cx).await;
        let capture = save_frame(
            cx,
            window,
            context
                .output_directory
                .join(format!("host-system-ssh-{section}.png")),
            "system",
            "ssh",
            section,
            "production-method input preparation; private-key source selection",
        )?;
        sources.push(PrivateKeySourceRecord {
            source,
            selected_by: "production authentication and source-selection methods",
            capture_file: capture.file.clone(),
        });
        captures.push(capture);
    }
    Ok(sources)
}

/// 长中文错误经应用探测和生产回调进入底栏，再真实点击详情，避免直接注入 UI 状态造图。
/// @author kongweiguang
async fn capture_long_error(
    cx: &mut AsyncApp,
    state: Entity<HostEditorAcceptanceState>,
    window: AnyWindowHandle,
    context: &AcceptanceContext,
    captures: &mut Vec<CaptureRecord>,
) -> Result<LongErrorRecord, String> {
    let fixture = Arc::new(LongErrorProbeFixture {
        requests: AtomicUsize::new(0),
    });
    let service = Arc::new(ConnectionProbeService::new(fixture.clone()));
    window
        .update(cx, |_, _, app| {
            state.update(app, |state, cx| state.replace_probe_service(service, cx));
        })
        .map_err(|error| error.to_string())?;
    require_click(cx, window, "connections-editor-probe")?;
    wait_for_probe_error(cx, state.clone(), window).await?;
    let (message_preserved, message_character_count) = window
        .update(cx, |_, _, app| {
            state.read(app).probe_error_content(LONG_PROBE_ERROR, app)
        })
        .map_err(|error| error.to_string())?;
    if !message_preserved {
        return Err(
            "production error callback did not preserve the long Chinese fixture".to_owned(),
        );
    }
    let already_open = window
        .update(cx, |_, _, app| {
            gpui_kit::base::GlobalState::is_in_deferred_context(app)
        })
        .map_err(|error| error.to_string())?;
    if already_open {
        return Err("another popup was already open before long-error details".to_owned());
    }
    require_click(cx, window, "connections-editor-probe-details")?;
    settle_frame(cx).await;
    let details_popup_open = window
        .update(cx, |_, window, app| {
            draw_acceptance_frame(window, app);
            gpui_kit::base::GlobalState::is_in_deferred_context(app)
        })
        .map_err(|error| error.to_string())?;
    if !details_popup_open {
        return Err("native details click did not open the long-error popover".to_owned());
    }
    let capture = save_frame(
        cx,
        window,
        context.output_directory.join("probe-long-error.png"),
        "system",
        "ssh-error",
        "long-error-details",
        "native-pointer; production error callback and details popover",
    )?;
    let record = LongErrorRecord {
        requests_started: fixture.requests.load(Ordering::SeqCst),
        message_preserved,
        message_character_count,
        details_popup_open,
        capture_file: capture.file.clone(),
        editor_geometry: capture.editor_geometry,
    };
    captures.push(capture);
    require_click(cx, window, "connections-editor-probe-details")?;
    settle_frame(cx).await;
    Ok(record)
}

/// 每类只保存公开合成值；真实按钮触发生产 CAS，目录读回和重开字段匹配后才形成成功证据。
/// @author kongweiguang
async fn capture_persistence(
    cx: &mut AsyncApp,
    state: Entity<HostEditorAcceptanceState>,
    window: AnyWindowHandle,
    context: &AcceptanceContext,
    captures: &mut Vec<CaptureRecord>,
) -> Result<Vec<PersistenceRecord>, String> {
    let mut records = Vec::with_capacity(HOST_TYPES.len());
    for (protocol, host_type) in HOST_TYPES {
        let name = format!("Acceptance saved {host_type}");
        let prepared = window
            .update(cx, |_, window, app| {
                state.update(app, |state, cx| {
                    state.prepare_save_fixture(protocol, &name, window, cx)
                })
            })
            .map_err(|error| error.to_string())?;
        if !prepared {
            return Err(format!(
                "{host_type} save fixture could not open a new draft"
            ));
        }
        let selector = if protocol.is_none() {
            "connections-local-editor-save"
        } else {
            "connections-editor-save"
        };
        let renamed = format!("{name} renamed");
        let mut saved_id = None;
        // 首次创建验证写入，第二次只改名称；两次都等待仓储刷新后重开，防止内存回执掩盖丢值。
        for (rename_only, current_name) in [(false, &name), (true, &renamed)] {
            if rename_only {
                let renamed = window
                    .update(cx, |_, window, app| {
                        state.update(app, |state, cx| {
                            state.rename_saved_fixture(current_name, window, cx)
                        })
                    })
                    .map_err(|error| error.to_string())?;
                if !renamed {
                    return Err(format!("{host_type} rename fixture editor missing"));
                }
            }
            settle_frame(cx).await;
            require_click(cx, window, selector)?;
            let deadline = Instant::now() + ACTION_TIMEOUT;
            let id = loop {
                let id = window
                    .update(cx, |_, _, app| {
                        state
                            .read(app)
                            .saved_fixture_id(protocol, current_name, app)
                    })
                    .map_err(|error| error.to_string())?;
                if let Some(id) = id {
                    break id;
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "{host_type} save did not close and reload its isolated configuration"
                    ));
                }
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
            };
            if saved_id.as_ref().is_some_and(|original| original != &id) {
                return Err(format!(
                    "{host_type} rename changed the saved host identity"
                ));
            }
            saved_id = Some(id);
            let reopened = window
                .update(cx, |_, window, app| {
                    state.update(app, |state, cx| {
                        state.reopen_saved_fixture(protocol, current_name, window, cx)
                    })
                })
                .map_err(|error| error.to_string())?;
            if !reopened {
                return Err(format!(
                    "{host_type} saved fixture was absent from the reloaded catalog"
                ));
            }
            let deadline = Instant::now() + ACTION_TIMEOUT;
            loop {
                let matches = window
                    .update(cx, |_, _, app| {
                        state
                            .read(app)
                            .saved_fixture_matches(protocol, current_name, app)
                    })
                    .map_err(|error| error.to_string())?;
                if matches {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "{host_type} reopened fields did not match their persisted fixture values"
                    ));
                }
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
            }
        }
        settle_frame(cx).await;
        let capture = save_frame(
            cx,
            window,
            context
                .output_directory
                .join(format!("saved-{host_type}.png")),
            "system",
            host_type,
            "saved-readback",
            "native save button; production reopen method",
        )?;
        records.push(PersistenceRecord {
            host_type,
            saved_id: saved_id.ok_or_else(|| "persistence loop did not save a host".to_owned())?,
            saved_by: "native GPUI pointer events on production Save button",
            reopened_by: "production edit method after repository catalog reload",
            readback_matches: true,
            rename_only_readback_matches: true,
            capture_file: capture.file.clone(),
        });
        captures.push(capture);
    }
    Ok(records)
}

/// 展开场景排在探测和持久化之后，不调用探测或保存；真实控件负责切换，公开样本只负责填值。
/// @author kongweiguang
async fn capture_dense_scenes(
    cx: &mut AsyncApp,
    state: Entity<HostEditorAcceptanceState>,
    window: AnyWindowHandle,
    context: &AcceptanceContext,
    captures: &mut Vec<CaptureRecord>,
) -> Result<Vec<DenseSceneRecord>, String> {
    let mut records = Vec::with_capacity(4);
    let proxy_baseline =
        prepare_dense_editor(cx, state.clone(), window, RemoteProtocol::Ssh, "proxy").await?;
    require_click(cx, window, "connections-proxy-toggle")?;
    prepare_dense_fields(cx, state.clone(), window, "proxy")?;
    settle_frame(cx).await;
    records.push(record_dense_scene(
        cx,
        state.clone(),
        window,
        context,
        captures,
        DenseSceneSetup {
            scene: "proxy-enabled",
            host_type: "ssh",
            baseline_geometry: proxy_baseline,
            native_interactions: vec!["connections-proxy-toggle".to_owned()],
            input_preparation: "new draft and public .invalid values through production methods and InputState::set_value; no password or network access",
        },
    )?);

    let jumps_baseline =
        prepare_dense_editor(cx, state.clone(), window, RemoteProtocol::Ssh, "jump-hosts").await?;
    require_click(cx, window, "connections-add-jump")?;
    require_click(cx, window, "connections-add-jump")?;
    prepare_dense_fields(cx, state.clone(), window, "jump-hosts")?;
    position_dense_jumps(cx, state.clone(), window, "second-auth").await?;
    let private_key_selector = window
        .update(cx, |_, _, app| {
            state.read(app).second_jump_private_key_selector(app)
        })
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "second jump did not exist after two native Add clicks".to_owned())?;
    require_click(cx, window, &private_key_selector)?;
    let jump_interactions = vec![
        "connections-add-jump".to_owned(),
        "connections-add-jump".to_owned(),
        private_key_selector,
    ];
    position_dense_jumps(cx, state.clone(), window, "top").await?;
    records.push(record_dense_scene(
        cx,
        state.clone(),
        window,
        context,
        captures,
        DenseSceneSetup {
            scene: "two-jumps-top",
            host_type: "ssh",
            baseline_geometry: jumps_baseline,
            native_interactions: jump_interactions.clone(),
            input_preparation: "public .invalid jump values and nonexistent key path through InputState::set_value; production ScrollHandle positioned at top; no file read, secret, probe or save",
        },
    )?);
    position_dense_jumps(cx, state.clone(), window, "lower").await?;
    records.push(record_dense_scene(
        cx,
        state.clone(),
        window,
        context,
        captures,
        DenseSceneSetup {
            scene: "two-jumps-lower",
            host_type: "ssh",
            baseline_geometry: jumps_baseline,
            native_interactions: jump_interactions,
            input_preparation: "same two-jump fixture; production FocusHandle focused second jump passphrase and ScrollHandle explicitly positioned at bottom; does not establish native Tab or automatic focus scrolling",
        },
    )?);

    let rdp_baseline =
        prepare_dense_editor(cx, state.clone(), window, RemoteProtocol::Rdp, "display").await?;
    require_radio_option(cx, window, "host-rdp-display-mode", 1)?;
    prepare_dense_fields(cx, state.clone(), window, "rdp-windowed")?;
    settle_frame(cx).await;
    records.push(record_dense_scene(
        cx,
        state,
        window,
        context,
        captures,
        DenseSceneSetup {
            scene: "rdp-windowed",
            host_type: "rdp",
            baseline_geometry: rdp_baseline,
            native_interactions: vec!["host-rdp-display-mode/1".to_owned()],
            input_preparation: "new draft and 1280x720 dimensions through production methods and InputState::set_value; no RDP launch, probe or save",
        },
    )?);
    Ok(records)
}

/// 新草稿与已保存样本分离，基线取自展开前的实际视口和底栏，避免把窗口不变冒充表单稳定。
/// 分类来自冻结的生产清单，保留静态身份以满足下一帧导航回调的生命周期。
/// @author kongweiguang
async fn prepare_dense_editor(
    cx: &mut AsyncApp,
    state: Entity<HostEditorAcceptanceState>,
    window: AnyWindowHandle,
    protocol: RemoteProtocol,
    section: &'static str,
) -> Result<EditorGeometry, String> {
    let prepared = window
        .update(cx, |_, window, app| {
            state.update(app, |state, cx| {
                state.prepare_save_fixture(Some(protocol), "展开布局验收", window, cx)
            })
        })
        .map_err(|error| error.to_string())?;
    if !prepared {
        return Err("could not open a fresh dense-layout fixture".to_owned());
    }
    select_section(cx, state, window, Some(protocol), section).await?;
    window
        .update(cx, |_, window, app| {
            draw_acceptance_frame(window, app);
            capture_editor_geometry(window, false)
        })
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

/// 样本准备失败即终止，不用直接改开关或补建跳板掩盖真实点击未生效。
/// @author kongweiguang
fn prepare_dense_fields(
    cx: &mut AsyncApp,
    state: Entity<HostEditorAcceptanceState>,
    window: AnyWindowHandle,
    scene: &str,
) -> Result<(), String> {
    let prepared = window
        .update(cx, |_, window, app| {
            state.update(app, |state, cx| {
                state.prepare_dense_fields(scene, window, cx)
            })
        })
        .map_err(|error| error.to_string())?;
    prepared
        .then_some(())
        .ok_or_else(|| format!("native controls did not prepare dense scene {scene}"))
}

/// 使用已标记的生产焦点与滚动准备入口；绘制后再检查下部输入实际可见，避免仅看偏移量。
/// @author kongweiguang
async fn position_dense_jumps(
    cx: &mut AsyncApp,
    state: Entity<HostEditorAcceptanceState>,
    window: AnyWindowHandle,
    position: &str,
) -> Result<(), String> {
    let positioned = window
        .update(cx, |_, window, app| {
            state.update(app, |state, cx| {
                state.position_dense_jumps(position, window, cx)
            })
        })
        .map_err(|error| error.to_string())?;
    if !positioned {
        return Err(format!("could not position dense jump view at {position}"));
    }
    settle_frame(cx).await;
    Ok(())
}

/// 记录截图对应的生产状态，并严格比较展开前后正文、状态栏与保存按钮的真实边界。
/// @author kongweiguang
fn record_dense_scene(
    cx: &mut AsyncApp,
    state: Entity<HostEditorAcceptanceState>,
    window: AnyWindowHandle,
    context: &AcceptanceContext,
    captures: &mut Vec<CaptureRecord>,
    setup: DenseSceneSetup,
) -> Result<DenseSceneRecord, String> {
    let section = match setup.scene {
        "proxy-enabled" => "dense-proxy-enabled",
        "two-jumps-top" => "dense-two-jumps-top",
        "two-jumps-lower" => "dense-two-jumps-lower",
        "rdp-windowed" => "dense-rdp-windowed",
        _ => return Err("unknown dense-layout capture scene".to_owned()),
    };
    let capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join(format!("host-system-{}-{section}.png", setup.host_type)),
        "system",
        setup.host_type,
        section,
        "native-pointer controls; production-method fixture and viewport preparation",
    )?;
    let actual = window
        .update(cx, |_, window, app| {
            state.read(app).dense_fixture_state(window, app)
        })
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "dense fixture editor closed before state readback".to_owned())?;
    let matches_scene = match setup.scene {
        "proxy-enabled" => actual.proxy_enabled,
        "two-jumps-top" => {
            actual.jump_count == 2
                && actual.second_jump_private_key
                && actual.scroll_offset_y == 0.0
        }
        "two-jumps-lower" => {
            actual.jump_count == 2
                && actual.second_jump_private_key
                && actual.second_jump_lower_focused
                && actual.second_jump_lower_visible
                && actual.scroll_offset_y < 0.0
        }
        "rdp-windowed" => actual.rdp_windowed,
        _ => false,
    };
    let footer_stable = setup.baseline_geometry == capture.editor_geometry;
    if !actual.prepared_values_match || !matches_scene || !footer_stable {
        return Err(format!(
            "dense scene {} did not preserve expected state or body/footer geometry: prepared={}, state={}, footer={}",
            setup.scene, actual.prepared_values_match, matches_scene, footer_stable
        ));
    }
    let record = DenseSceneRecord {
        scene: setup.scene,
        host_type: setup.host_type,
        native_interactions: setup.native_interactions,
        input_preparation: setup.input_preparation,
        capture_file: capture.file.clone(),
        baseline_geometry: setup.baseline_geometry,
        editor_geometry: capture.editor_geometry,
        footer_stable,
        proxy_enabled: actual.proxy_enabled,
        jump_count: actual.jump_count,
        second_jump_private_key: actual.second_jump_private_key,
        rdp_windowed: actual.rdp_windowed,
        prepared_values_match: actual.prepared_values_match,
        scroll_offset_y: actual.scroll_offset_y,
        second_jump_lower_focused: actual.second_jump_lower_focused,
        second_jump_lower_visible: actual.second_jump_lower_visible,
    };
    captures.push(capture);
    Ok(record)
}

/// 宽窗口点击实际协议按钮；折叠菜单的窄窗口明确记录生产方法导航，并核对活动编辑器。
/// @author kongweiguang
async fn select_protocol(
    cx: &mut AsyncApp,
    state: Entity<HostEditorAcceptanceState>,
    window: AnyWindowHandle,
    protocol: Option<RemoteProtocol>,
) -> Result<bool, String> {
    let native = click_if_present(cx, window, &format!("connections-protocol-{protocol:?}"))?;
    if !native {
        window
            .update(cx, |_, window, app| {
                state.update(app, |state, cx| match protocol {
                    Some(protocol) => state.select_protocol(protocol, window, cx),
                    None => state.select_local(window, cx),
                })
            })
            .map_err(|error| error.to_string())?;
    }
    settle_frame(cx).await;
    let observed = window
        .update(cx, |_, _, app| state.read(app).current_protocol(app))
        .map_err(|error| error.to_string())?;
    if observed != Some(protocol) {
        return Err(format!("protocol navigation did not activate {protocol:?}"));
    }
    Ok(native)
}

/// 分类从生产清单取得；点击后的状态必须一致，紧凑菜单采用可审计的方法入口回退。
/// @author kongweiguang
async fn select_section(
    cx: &mut AsyncApp,
    state: Entity<HostEditorAcceptanceState>,
    window: AnyWindowHandle,
    protocol: Option<RemoteProtocol>,
    section: &'static str,
) -> Result<bool, String> {
    let variant = match section {
        "general" => "Properties",
        "proxy" => "Proxy",
        "jump-hosts" => "JumpHosts",
        "terminal" | "serial" | "startup" => "Terminal",
        "network" => "Network",
        "files" => "Transfer",
        "display" => "Display",
        _ => return Err(format!("unmapped production section {section}")),
    };
    let prefix = if protocol.is_none() {
        "connections-local-editor-section"
    } else {
        "connections-editor-section"
    };
    let native = click_if_present(cx, window, &format!("{prefix}-{variant}"))?;
    if !native {
        let selected = window
            .update(cx, |_, window, app| {
                state.update(app, |state, cx| state.select_section(section, window, cx))
            })
            .map_err(|error| error.to_string())?;
        if !selected {
            return Err(format!("production editor rejected section {section}"));
        }
    }
    settle_frame(cx).await;
    let observed = window
        .update(cx, |_, _, app| state.read(app).current_section(app))
        .map_err(|error| error.to_string())?;
    if observed != Some(section) {
        return Err(format!("section navigation did not activate {section}"));
    }
    Ok(native)
}

/// 优先使用产品 selector，其他真实控件按同版本 Kit 的 ElementId 快照定位；统一派发窗口鼠标事件。
/// @author kongweiguang
fn click_if_present(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<bool, String> {
    window
        .update(cx, |_, window, app| {
            draw_acceptance_frame(window, app);
            let bounds = window.debug_selector_bounds(selector).or_else(|| {
                let id = gpui_pre::ElementId::Name(selector.to_owned().into());
                gpui_kit::base::test_support::find(window, &[], &id)
                    .filter(|element| element.visible())
                    .map(|element| element.bounds())
            });
            let Some(bounds) = bounds else {
                return Ok(false);
            };
            dispatch_native_click(window, app, bounds, selector)?;
            Ok(true)
        })
        .map_err(|error| error.to_string())?
}

/// 锁定版本的 RadioGroup 会重写子项为整数 ID，必须在真实分组作用域内定位，不能用构造时标签猜 ID。
/// @author kongweiguang
fn require_radio_option(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    group: &str,
    option: usize,
) -> Result<(), String> {
    window
        .update(cx, |_, window, app| {
            draw_acceptance_frame(window, app);
            let group_id = gpui_pre::ElementId::Name(group.to_owned().into());
            let scope = gpui_kit::base::test_support::scope(window, &[], &group_id);
            let bounds = gpui_kit::base::test_support::find(window, &scope, &option.into())
                .filter(|element| element.visible())
                .map(|element| element.bounds())
                .ok_or_else(|| format!("missing production Radio option {group}/{option}"))?;
            dispatch_native_click(window, app, bounds, &format!("{group}/{option}"))
        })
        .map_err(|error| error.to_string())?
}

/// 原生点击只接受完全位于视口内的已绘制目标，避免对屏外坐标误派发并掩盖交互遗漏。
/// @author kongweiguang
fn dispatch_native_click(
    window: &mut gpui_pre::Window,
    app: &mut App,
    bounds: Bounds<gpui_pre::Pixels>,
    description: &str,
) -> Result<(), String> {
    let viewport = window.visual_viewport_bounds();
    if bounds.size.width <= px(0.)
        || bounds.size.height <= px(0.)
        || bounds.left() < viewport.left()
        || bounds.right() > viewport.right()
        || bounds.top() < viewport.top()
        || bounds.bottom() > viewport.bottom()
    {
        return Err(format!(
            "rendered control {description} is outside the viewport"
        ));
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
    Ok(())
}

/// Probe 与 Save 不能用内部方法伪装点击成功，缺少真实按钮即终止本轮验收。
/// @author kongweiguang
fn require_click(cx: &mut AsyncApp, window: AnyWindowHandle, selector: &str) -> Result<(), String> {
    if click_if_present(cx, window, selector)? {
        Ok(())
    } else {
        Err(format!("missing production button {selector}"))
    }
}

/// 等待 fixture 确实收到或完成请求，避免“running”状态先于后台启动而造成迟到测试空转。
/// @author kongweiguang
async fn wait_for_fixture(
    cx: &mut AsyncApp,
    fixture: &ControlledProbeFixture,
    count: usize,
    completed: bool,
) -> Result<(), String> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let observed = if completed {
            fixture.completed.load(Ordering::SeqCst)
        } else {
            fixture.requests.load(Ordering::SeqCst)
        };
        if observed >= count {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "probe fixture timed out: completed={completed}, expected={count}, observed={observed}"
            ));
        }
        cx.background_executor()
            .timer(Duration::from_millis(25))
            .await;
    }
}

/// 从本帧已绘制的正文与底栏控件读取几何，不能以系统窗口不变代替内容没有被挤压。
/// @author kongweiguang
fn capture_editor_geometry(
    window: &gpui_pre::Window,
    local: bool,
) -> Result<EditorGeometry, io::Error> {
    let prefix = if local {
        "connections-local-editor"
    } else {
        "connections-editor"
    };
    let read = |suffix: &str| {
        let selector = format!("{prefix}-{suffix}");
        let bounds = window
            .debug_selector_bounds(&selector)
            .ok_or_else(|| io::Error::other(format!("missing editor geometry {selector}")))?;
        if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
            return Err(io::Error::other(format!(
                "empty editor geometry {selector}"
            )));
        }
        Ok(GeometryRect {
            x: bounds.origin.x.as_f32(),
            y: bounds.origin.y.as_f32(),
            width: bounds.size.width.as_f32(),
            height: bounds.size.height.as_f32(),
        })
    };
    Ok(EditorGeometry {
        body: read("viewport")?,
        status_lane: read("status-lane")?,
        save_button: read("save")?,
    })
}

/// 通过锁定版本的 Kit 主题 API 设置生产控件使用的外观。
/// @author kongweiguang
fn apply_theme(cx: &mut AsyncApp, window: AnyWindowHandle, mode: ThemeMode) -> Result<(), String> {
    window
        .update(cx, |_, window, app| match mode {
            ThemeMode::Light => gpui_kit::component::Theme::change(
                gpui_kit::component::ThemeMode::Light,
                Some(window),
                app,
            ),
            ThemeMode::Dark => gpui_kit::component::Theme::change(
                gpui_kit::component::ThemeMode::Dark,
                Some(window),
                app,
            ),
            ThemeMode::System => {
                gpui_kit::component::Theme::sync_system_appearance(Some(window), app)
            }
        })
        .map_err(|error| error.to_string())
}

/// 等待布局与异步状态稳定后再采样画面。
/// @author kongweiguang
async fn settle_frame(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(250))
        .await;
}

/// 在有界期限内等待指定探测状态，避免依赖固定延时猜测结果。
/// @author kongweiguang
async fn wait_for_probe(
    cx: &mut AsyncApp,
    state: Entity<HostEditorAcceptanceState>,
    window: AnyWindowHandle,
    predicate: impl Fn(bool, &Option<ConnectionProbeOutcome>) -> bool,
) -> Result<(), String> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let (running, outcome) = window
            .update(cx, |_, _, app| state.read(app).probe_state(app))
            .map_err(|error| error.to_string())?;
        if predicate(running, &outcome) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "probe state timed out: running={running}, outcome={outcome:?}"
            ));
        }
        cx.background_executor()
            .timer(Duration::from_millis(50))
            .await;
    }
}

/// 错误场景必须等到生产回调写入错误，不能把“无结果且未运行”的初始状态误判为失败。
/// @author kongweiguang
async fn wait_for_probe_error(
    cx: &mut AsyncApp,
    state: Entity<HostEditorAcceptanceState>,
    window: AnyWindowHandle,
) -> Result<(), String> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let ((running, outcome), (cancelled, error_present)) = window
            .update(cx, |_, _, app| {
                (
                    state.read(app).probe_state(app),
                    state.read(app).probe_feedback(app),
                )
            })
            .map_err(|error| error.to_string())?;
        if !running && outcome.is_none() && !cancelled && error_present {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "long-error callback timed out: running={running}, outcome={outcome:?}, cancelled={cancelled}, error_present={error_present}"
            ));
        }
        cx.background_executor()
            .timer(Duration::from_millis(25))
            .await;
    }
}

/// 使用同一原生渲染器截取指定生命周期状态和窗口几何。
/// @author kongweiguang
fn capture_probe_state(
    cx: &mut AsyncApp,
    state: Entity<HostEditorAcceptanceState>,
    window: AnyWindowHandle,
    path: PathBuf,
    stage: &'static str,
) -> Result<(CaptureRecord, ProbeLifecycleRecord), String> {
    let record = save_frame(
        cx,
        window,
        path,
        "system",
        "ssh-probe",
        "general",
        "native-pointer; input change through InputState",
    )?;
    let ((running, outcome), (cancelled, error_present)) = window
        .update(cx, |_, _, app| {
            (
                state.read(app).probe_state(app),
                state.read(app).probe_feedback(app),
            )
        })
        .map_err(|error| error.to_string())?;
    let lifecycle = ProbeLifecycleRecord {
        stage,
        running,
        outcome: outcome.as_ref().map(outcome_id),
        cancelled,
        error_present,
        window_width: record.window_width,
        window_height: record.window_height,
        editor_geometry: record.editor_geometry,
    };
    Ok((record, lifecycle))
}

/// 只序列化稳定的结果类别，不写入错误详情或主机输入。
/// @author kongweiguang
fn outcome_id(outcome: &ConnectionProbeOutcome) -> String {
    match outcome {
        ConnectionProbeOutcome::Verified(ProbeVerificationLevel::SshAuthenticated)
        | ConnectionProbeOutcome::SshAuthenticated { .. } => "ssh-authenticated".to_owned(),
        ConnectionProbeOutcome::Verified(ProbeVerificationLevel::SftpSubsystemReady)
        | ConnectionProbeOutcome::Verified(ProbeVerificationLevel::SftpDirectoryAccessible) => {
            "sftp-ready".to_owned()
        }
        ConnectionProbeOutcome::Verified(ProbeVerificationLevel::TelnetTransportConnected) => {
            "telnet-connected".to_owned()
        }
        ConnectionProbeOutcome::Verified(ProbeVerificationLevel::SerialDeviceOpened) => {
            "serial-opened".to_owned()
        }
        ConnectionProbeOutcome::InvalidInput { .. } => "invalid-input".to_owned(),
        ConnectionProbeOutcome::RdpConnectivity(_) => "rdp-connectivity".to_owned(),
    }
}

/// 隐藏 HWND 没有普通帧循环；显式交付下一帧回调，避免生产焦点交接滞留到后续截图。
/// @author kongweiguang
fn draw_acceptance_frame(window: &mut gpui_pre::Window, app: &mut App) {
    let frame = window.draw(app);
    frame.clear(app);
    if window.simulate_next_frame(app) > 0 {
        let frame = window.draw(app);
        frame.clear(app);
    }
}

/// 先绘制隐藏窗口并交付下一帧回调再读取像素，确保截图对应当前生产实体状态。
/// @author kongweiguang
fn save_frame(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    path: PathBuf,
    theme: &'static str,
    host_type: &'static str,
    section: &'static str,
    interaction_mode: &'static str,
) -> Result<CaptureRecord, String> {
    let (image, geometry, editor_geometry) = window
        .update(cx, |_, window, app| {
            crate::right_rail_acceptance::isolation::capture()
                .and_then(|evidence| {
                    evidence.validate_window_state(window.is_visible(), window.is_window_active())
                })
                .map_err(io::Error::other)?;
            draw_acceptance_frame(window, app);
            let image = window.render_to_image().map_err(io::Error::other)?;
            let bounds = window.bounds();
            let viewport = window.visual_viewport_bounds();
            let scale = window.scale_factor();
            if !image
                .as_raw()
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[3] != 0)
            {
                return Err(io::Error::other(
                    "native renderer returned a transparent frame",
                ));
            }
            let dimensions = (
                bounds.size.width.as_f32(),
                bounds.size.height.as_f32(),
                viewport.size.width.as_f32(),
                viewport.size.height.as_f32(),
                scale,
            );
            let editor_geometry = capture_editor_geometry(window, host_type == "local")?;
            Ok::<_, io::Error>((image, dimensions, editor_geometry))
        })
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    let (window_width, window_height, viewport_width, viewport_height, scale_factor) = geometry;
    let width = image.width();
    let height = image.height();
    let expected_width = viewport_width * scale_factor;
    let expected_height = viewport_height * scale_factor;
    if !scale_factor.is_finite()
        || scale_factor <= 0.0
        || width != expected_width.round() as u32
        || height != expected_height.round() as u32
    {
        return Err(format!(
            "native screenshot does not match viewport: expected={expected_width:.2}x{expected_height:.2}, observed={width}x{height}, scale={scale_factor}"
        ));
    }
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map_err(|error| error.to_string())?
            .join(path)
    };
    image.save(&path).map_err(|error| error.to_string())?;
    let file = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| "invalid screenshot filename".to_owned())?
        .to_owned();
    Ok(CaptureRecord {
        theme,
        host_type,
        section,
        interaction_mode,
        file,
        width,
        height,
        pixel_sha256: format!("{:x}", Sha256::digest(image.as_raw())),
        window_width,
        window_height,
        viewport_width,
        viewport_height,
        scale_factor,
        editor_geometry,
    })
}

/// 只记录首个失败，避免后续关闭错误掩盖原始原因。
/// @author kongweiguang
fn set_failure(failure: &Mutex<Option<String>>, message: String) {
    if let Ok(mut failure) = failure.lock()
        && failure.is_none()
    {
        *failure = Some(message);
    }
}

/// 三个请求由独立门闩控制，旧请求刻意忽略取消，以验证生产 generation 会拒绝异类迟到结果。
struct ControlledProbeFixture {
    requests: AtomicUsize,
    completed: AtomicUsize,
    released: AtomicUsize,
    gates: [tokio::sync::Semaphore; 3],
}

impl ControlledProbeFixture {
    /// 每个门闩只允许放行一次；旧请求只能在新 SSH 成功后由验收编排显式释放。
    /// @author kongweiguang
    fn release(&self, request: usize) -> Result<(), String> {
        let Some(gate) = self.gates.get(request) else {
            return Err(format!("unknown controlled probe request {request}"));
        };
        let mask = 1usize << request;
        if self.released.fetch_or(mask, Ordering::SeqCst) & mask != 0 {
            return Err(format!(
                "controlled probe request {request} was released twice"
            ));
        }
        gate.add_permits(1);
        Ok(())
    }

    /// 验收失败或窗口 panic 后也必须放行仍受 executor 管理的请求，避免门闩阻塞统一资源收尾。
    /// @author kongweiguang
    fn release_remaining(&self) {
        for (request, gate) in self.gates.iter().enumerate() {
            let mask = 1usize << request;
            if self.released.fetch_or(mask, Ordering::SeqCst) & mask == 0 {
                gate.add_permits(1);
            }
        }
    }
}

#[async_trait]
impl ConnectionProbeBackend for ControlledProbeFixture {
    /// 先登记真实请求，再等待专用门闩；不依赖机器负载猜测延迟，也不响应取消而绕开旧结果回调。
    /// @author kongweiguang
    async fn probe(
        &self,
        target: ConnectionProbeTarget,
        _cancellation: CancellationToken,
    ) -> AppResult<ConnectionProbeOutcome> {
        if target.profile.protocol != RemoteProtocol::Ssh
            || !matches!(
                target.profile.authentication,
                Some(SshAuthentication::Agent)
            )
        {
            return Err(AppError::invalid("验收探测必须使用无秘密的 SSH Agent 草稿"));
        }
        let request = self.requests.fetch_add(1, Ordering::SeqCst);
        let gate = self
            .gates
            .get(request)
            .ok_or_else(|| AppError::invalid("验收探测收到超过三个生命周期请求"))?;
        gate.acquire()
            .await
            .map_err(|_| AppError::infrastructure("验收探测门闩意外关闭"))?
            .forget();
        self.completed.fetch_add(1, Ordering::SeqCst);
        Ok(ConnectionProbeOutcome::Verified(if request < 2 {
            ProbeVerificationLevel::TelnetTransportConnected
        } else {
            ProbeVerificationLevel::SshAuthenticated
        }))
    }
}

/// 独立错误场景不计入三请求生命周期，长文本不包含真实端点、凭据或系统诊断。
struct LongErrorProbeFixture {
    requests: AtomicUsize,
}

#[async_trait]
impl ConnectionProbeBackend for LongErrorProbeFixture {
    /// 通过真实错误返回值进入生产错误处理，不能直接设置面板错误或弹层状态。
    /// @author kongweiguang
    async fn probe(
        &self,
        _target: ConnectionProbeTarget,
        _cancellation: CancellationToken,
    ) -> AppResult<ConnectionProbeOutcome> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        Err(AppError::infrastructure(LONG_PROBE_ERROR))
    }
}

/// 提供合成设备名称，使串口选择器无需连接硬件也可验收。
struct FixtureSerialPorts;

#[async_trait]
impl SerialPortRepository for FixtureSerialPorts {
    /// 返回合成设备项，不调用平台串口接口，也不打开设备路径。
    /// @author kongweiguang
    async fn list(&self, cancellation: CancellationToken) -> AppResult<Vec<SerialPortDescriptor>> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("验收串口目录读取已取消"));
        }
        Ok(vec![
            SerialPortDescriptor {
                name: "COM-ACCEPTANCE-01".to_owned(),
                manufacturer: Some("Synthetic".to_owned()),
                product: Some("Acceptance device".to_owned()),
                vendor_id: None,
                product_id: None,
            },
            SerialPortDescriptor {
                name: "/dev/ttyACM-acceptance".to_owned(),
                manufacturer: Some("Synthetic".to_owned()),
                product: Some("Acceptance device".to_owned()),
                vendor_id: None,
                product_id: None,
            },
        ])
    }
}
