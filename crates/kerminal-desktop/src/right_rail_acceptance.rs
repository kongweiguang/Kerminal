//! 隐藏原生窗口的右栏验收截图入口。
//!
//! @author kongweiguang

mod functional;
mod functional_agent;
mod functional_compose;
mod functional_containers;
mod functional_forwarding;
mod functional_logs;
mod functional_snippets;
mod functional_tmux_attach;
mod functional_transfers;
pub(crate) mod isolation;
mod menus;
mod shell;
mod snippet_import;

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
    AnyWindowHandle, AppContext as _, AsyncApp, Context, Entity, TitlebarOptions,
    WindowBackgroundAppearance, WindowOptions, component::TitleBar,
};
use gpui_pre::{App, Bounds, WindowBounds, px, size};
use kerminal_application::preferences::{ThemeMode, settings::ToolRailToolId};
use kerminal_domain::{files::FileEndpoint, identity::HostId};
use kerminal_ui::RootView;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{composition::DesktopComposition, runtime::ApplicationRuntime};

const ACCEPTANCE_POLL: Duration = Duration::from_millis(150);
const FIXTURE_READY_TIMEOUT: Duration = Duration::from_secs(60);
const WINDOW_RESIZE_TIMEOUT: Duration = Duration::from_secs(10);
const PANEL_FRAME_TIMEOUT: Duration = Duration::from_secs(30);
const PANEL_FRAME_POLL: Duration = Duration::from_millis(100);
const PANEL_STABLE_FRAME_COUNT: usize = 2;
const PANEL_MAX_FRAME_PHASES: usize = 2;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(12);
const FIXTURE_MANIFEST: &str = ".local/right-rail-fixture/manifest.json";
const FIXTURE_HOST_ID: &str = "right-rail-fixture";
const MIN_WINDOW_WIDTH: u32 = 800;
const MAX_WINDOW_WIDTH: u32 = 3840;
const MIN_WINDOW_HEIGHT: u32 = 600;
const MAX_WINDOW_HEIGHT: u32 = 2160;

const RIGHT_RAIL_THEMES: [(ThemeMode, &str); 3] = [
    (ThemeMode::Light, "light"),
    (ThemeMode::Dark, "dark"),
    (ThemeMode::System, "system"),
];

const RIGHT_RAIL_TOOLS: [(ToolRailToolId, &str); 8] = [
    (ToolRailToolId::AgentLauncher, "agent-launcher"),
    (ToolRailToolId::Sftp, "sftp"),
    (ToolRailToolId::Containers, "containers"),
    (ToolRailToolId::Snippets, "snippets"),
    (ToolRailToolId::Tmux, "tmux"),
    (ToolRailToolId::Ports, "ports"),
    (ToolRailToolId::System, "system"),
    (ToolRailToolId::Logs, "logs"),
];

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AcceptanceReport {
    schema_version: u32,
    capture_mode: &'static str,
    capture_backend: &'static str,
    config_directory: String,
    fixture_config_source: String,
    fixture_host_id: String,
    fixture_host_key_policy: &'static str,
    window_size_unit: &'static str,
    window_size_source: &'static str,
    requested_window_width: Option<u32>,
    requested_window_height: Option<u32>,
    window_visible: bool,
    window_active: bool,
    native_isolation: isolation::DesktopIsolationEvidence,
    window_width: f32,
    window_height: f32,
    functional: Vec<functional::FunctionalRecord>,
    themes: Vec<ThemeCaptureRecord>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThemeCaptureRecord {
    mode: &'static str,
    window_appearance: String,
    baseline: CaptureRecord,
    panels: Vec<PanelCaptureRecord>,
    states: Vec<StateCaptureRecord>,
    dialogs: Vec<DialogCaptureRecord>,
    menus: Vec<menus::MenuCaptureRecord>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CaptureRecord {
    file: String,
    width: u32,
    height: u32,
    pixel_sha256: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PanelCaptureRecord {
    tool: &'static str,
    file: String,
    width: u32,
    height: u32,
    selected: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DialogCaptureRecord {
    surface: &'static str,
    capture: CaptureRecord,
    open: bool,
    dismissal: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StateCaptureRecord {
    surface: &'static str,
    capture: CaptureRecord,
    host_id: String,
    target_id: Option<String>,
    interaction: &'static str,
}

struct StateCaptureMetadata {
    theme_name: &'static str,
    surface: &'static str,
    host_id: String,
    target_id: Option<String>,
    interaction: &'static str,
}

#[derive(Default)]
struct AcceptanceOptions {
    output_directory: Option<PathBuf>,
    fixture_config_directory: Option<PathBuf>,
    window_width: Option<u32>,
    window_height: Option<u32>,
    functional_only: bool,
    capture_only: bool,
}

/// 跨主题截图共享的 fixture 绑定、输出位置和窗口尺寸快照。
struct RightRailCaptureContext {
    output_directory: PathBuf,
    fixture_config_source: PathBuf,
    fixture_host_id: HostId,
    remote_sftp_directory: String,
    local_files_root: PathBuf,
    fixture_container_name: String,
    container_target_port: u16,
    requested_window_width: Option<u32>,
    requested_window_height: Option<u32>,
    requested_bounds_size: Option<(f32, f32)>,
    functional_only: bool,
    capture_only: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureManifest {
    state: String,
    gpui_config_root: PathBuf,
    remote_sftp_directory: String,
    local_files_root: PathBuf,
    container_name: String,
    container_target_port: u16,
}

/// 只从隔离 fixture 读取配置并复制到新输出目录，用户配置不进入验收 Composition。
///
/// @author kongweiguang
pub(super) fn run() -> Result<(), Box<dyn Error>> {
    let options = requested_options()?;
    let requested_window_width = options.window_width;
    let requested_window_height = options.window_height;
    let functional_only = options.functional_only;
    let capture_only = options.capture_only;
    let fixture_manifest = read_fixture_manifest()?;
    let remote_sftp_directory = fixture_manifest.remote_sftp_directory.clone();
    let local_files_root = fixture_manifest.local_files_root.clone();
    let fixture_container_name = fixture_manifest.container_name.clone();
    let container_target_port = fixture_manifest.container_target_port;
    let fixture_config_source =
        resolve_fixture_config_directory(options.fixture_config_directory, &fixture_manifest)?;
    let output_base = options
        .output_directory
        .clone()
        .unwrap_or_else(|| Path::new("target").join("acceptance").join("right-rail"));
    reject_user_configuration_output_path(&output_base)?;
    reject_fixture_output_overlap(&fixture_config_source, &output_base)?;
    let output_directory = match options.output_directory {
        Some(path) => create_requested_output_directory(&path)?,
        None => create_unique_output_directory(output_base)?,
    };
    let config_directory = output_directory.join("config");
    fs::create_dir(&config_directory)?;
    copy_fixture_configuration(&fixture_config_source, &config_directory)?;
    let fixture_host_id = HostId::parse(FIXTURE_HOST_ID)?;

    let runtime = ApplicationRuntime::new()?;
    let mut composition = runtime.block_on(DesktopComposition::build(
        runtime.handle(),
        &config_directory,
    ))?;
    let ui_result = catch_unwind(AssertUnwindSafe(|| {
        run_hidden_window(
            composition.app_services(),
            RightRailCaptureContext {
                output_directory: output_directory.clone(),
                fixture_config_source,
                fixture_host_id,
                remote_sftp_directory,
                local_files_root,
                fixture_container_name,
                container_target_port,
                requested_window_width,
                requested_window_height,
                requested_bounds_size: None,
                functional_only,
                capture_only,
            },
        )
    }))
    .unwrap_or_else(|_| Err(io::Error::other("GPUI 右栏验收事件循环意外终止").into()));
    let shutdown_result = runtime
        .block_on(composition.shutdown_until(tokio::time::Instant::now() + SHUTDOWN_TIMEOUT));
    runtime.shutdown();

    ui_result?;
    shutdown_result?;
    println!("右栏验收截图和报告：{}", output_directory.display());
    Ok(())
}

/// 解析验收参数并拒绝重复或未知选项，防止把拼错的路径静默交给产品启动流程。
///
/// @author kongweiguang
fn requested_options() -> Result<AcceptanceOptions, io::Error> {
    let mut arguments = std::env::args_os().skip(2);
    let mut options = AcceptanceOptions::default();
    while let Some(flag) = arguments.next() {
        if flag == OsStr::new("--capture-only") {
            if options.capture_only || options.functional_only {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "验收子模式不能重复或混用",
                ));
            }
            options.capture_only = true;
            continue;
        }
        if flag == OsStr::new("--functional-only") {
            if options.functional_only || options.capture_only {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "验收子模式不能重复或混用",
                ));
            }
            options.functional_only = true;
            continue;
        }
        if flag == OsStr::new("--width") {
            if options.window_width.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--width 不能重复",
                ));
            }
            let Some(value) = arguments.next() else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--width 缺少逻辑像素值",
                ));
            };
            options.window_width = Some(parse_logical_dimension(
                value,
                "--width",
                MIN_WINDOW_WIDTH,
                MAX_WINDOW_WIDTH,
            )?);
            continue;
        }
        if flag == OsStr::new("--height") {
            if options.window_height.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--height 不能重复",
                ));
            }
            let Some(value) = arguments.next() else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--height 缺少逻辑像素值",
                ));
            };
            options.window_height = Some(parse_logical_dimension(
                value,
                "--height",
                MIN_WINDOW_HEIGHT,
                MAX_WINDOW_HEIGHT,
            )?);
            continue;
        }
        let slot = if flag == OsStr::new("--output-dir") {
            &mut options.output_directory
        } else if flag == OsStr::new("--fixture-config-dir") {
            &mut options.fixture_config_directory
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "仅支持 --right-rail-acceptance [--output-dir <新目录>] [--fixture-config-dir <隔离配置目录>] [--width <逻辑像素>] [--height <逻辑像素>] [--functional-only | --capture-only]",
            ));
        };
        if slot.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "验收选项不能重复",
            ));
        }
        let Some(path) = arguments.next() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} 缺少目录参数", flag.to_string_lossy()),
            ));
        };
        *slot = Some(PathBuf::from(path));
    }
    Ok(options)
}

/// Parses an integer logical-pixel dimension and rejects values that produce unusable or oversized windows.
///
/// @author kongweiguang
fn parse_logical_dimension(
    value: std::ffi::OsString,
    option: &str,
    minimum: u32,
    maximum: u32,
) -> Result<u32, io::Error> {
    let value = value.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{option} 必须是 UTF-8 整数逻辑像素值"),
        )
    })?;
    let dimension = value.parse::<u32>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{option} 必须是整数逻辑像素值"),
        )
    })?;
    if !(minimum..=maximum).contains(&dimension) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{option} 范围为 {minimum}..={maximum} 逻辑像素"),
        ));
    }
    Ok(dimension)
}

/// Resolves the supplied or validated manifest fixture root and rejects either real per-user configuration tree.
///
/// @author kongweiguang
fn resolve_fixture_config_directory(
    requested: Option<PathBuf>,
    manifest: &FixtureManifest,
) -> Result<PathBuf, Box<dyn Error>> {
    let requested = requested.unwrap_or_else(|| manifest.gpui_config_root.clone());
    let source = fs::canonicalize(requested)?;
    if !source.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fixture 配置路径必须是已存在的目录",
        )
        .into());
    }
    reject_user_configuration_directory(&source)?;
    if !source
        .join("hosts")
        .join(format!("{FIXTURE_HOST_ID}.toml"))
        .is_file()
        || !source.join("known_hosts").is_file()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fixture 配置目录缺少隔离主机定义或 known_hosts",
        )
        .into());
    }
    Ok(source)
}

/// 输出目录创建前校验隔离 SFTP 路径、容器名称和受控端口，缺失字段不得退回其它资源。
///
/// @author kongweiguang
fn read_fixture_manifest() -> Result<FixtureManifest, Box<dyn Error>> {
    let mut manifest: FixtureManifest = serde_json::from_slice(&fs::read(FIXTURE_MANIFEST)?)?;
    if manifest.state != "ready" {
        return Err(io::Error::other("隔离 SSH fixture 尚未就绪").into());
    }
    if manifest.container_target_port == 0 {
        return Err(io::Error::other("fixture HTTP 目标端口不能为零").into());
    }
    if !manifest.container_name.starts_with("kerminal-rr-")
        || !manifest
            .container_name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err(io::Error::other("fixture 容器名称不属于右栏隔离资源").into());
    }
    let endpoint = FileEndpoint::remote(
        HostId::parse(FIXTURE_HOST_ID)?,
        &manifest.remote_sftp_directory,
    )
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "fixture SFTP 目录路径格式无效"))?;
    let remote_path = endpoint
        .remote_path()
        .ok_or_else(|| io::Error::other("fixture SFTP 目标不是远端路径"))?;
    if !remote_path.as_str().starts_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fixture SFTP 目录必须是绝对 POSIX 路径",
        )
        .into());
    }
    manifest.remote_sftp_directory = remote_path.to_string();
    manifest.local_files_root = resolve_fixture_local_files_root(&manifest.local_files_root)?;
    Ok(manifest)
}

/// 规范化后只接受 manifest 同目录下的专用 local-files 目录，防止 fixture 输入指向用户文件。
///
/// @author kongweiguang
fn resolve_fixture_local_files_root(requested: &Path) -> Result<PathBuf, io::Error> {
    if !requested.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fixture 本地文件目录必须是绝对路径",
        ));
    }
    let fixture_root = Path::new(FIXTURE_MANIFEST)
        .parent()
        .ok_or_else(|| io::Error::other("fixture manifest 缺少父目录"))?;
    let expected = fs::canonicalize(fixture_root.join("local-files"))?;
    let root = fs::canonicalize(requested)?;
    if !root.is_dir() || root != expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "右栏验收本地文件只能使用隔离 fixture 的 local-files 目录",
        ));
    }
    Ok(root)
}

/// Refuses reading or copying the active GPUI data root and the legacy formal data root.
///
/// @author kongweiguang
fn reject_user_configuration_directory(source: &Path) -> Result<(), io::Error> {
    for user_root in user_configuration_roots()? {
        if path_starts_with(source, &user_root) || path_starts_with(&user_root, source) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "右栏验收拒绝使用或包含用户真实配置目录",
            ));
        }
    }
    Ok(())
}

/// Prevents output creation inside the active or default user data roots.
///
/// @author kongweiguang
fn reject_user_configuration_output_path(path: &Path) -> Result<(), io::Error> {
    let output = resolve_path_identity(path)?;
    for user_root in user_configuration_roots()? {
        if path_starts_with(&output, &user_root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "右栏验收输出目录不能位于用户真实配置目录内",
            ));
        }
    }
    Ok(())
}

/// Rejects output paths that could create files inside or over the fixture source tree.
///
/// @author kongweiguang
fn reject_fixture_output_overlap(source: &Path, output: &Path) -> Result<(), io::Error> {
    let output = resolve_path_identity(output)?;
    if path_starts_with(&output, source) || path_starts_with(source, &output) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "验收输出目录与 fixture 配置目录不能相互包含",
        ));
    }
    Ok(())
}

/// Returns canonical identities for the active config override and both product data roots.
///
/// @author kongweiguang
fn user_configuration_roots() -> Result<Vec<PathBuf>, io::Error> {
    let home = dirs::home_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "无法确认用户配置根目录身份，拒绝启动隔离验收",
        )
    })?;
    let mut roots = vec![
        resolve_path_identity(&home.join(".kerminal-gpui"))?,
        resolve_path_identity(&home.join(".kerminal"))?,
    ];
    if let Some(override_root) = std::env::var_os("KERMINAL_CONFIG_ROOT")
        && !override_root.is_empty()
    {
        roots.push(resolve_path_identity(Path::new(&override_root))?);
    }
    Ok(roots)
}

/// Resolves an existing ancestor before appending a possibly new suffix, so junction aliases compare safely.
///
/// @author kongweiguang
fn resolve_path_identity(path: &Path) -> Result<PathBuf, io::Error> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::new();
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = ancestor.file_name().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "无法解析配置路径身份")
                })?;
                suffix.push(name.to_os_string());
                ancestor = ancestor.parent().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "无法解析配置路径身份")
                })?;
            }
            Err(error) => return Err(error),
        }
    }
    let mut resolved = fs::canonicalize(ancestor)?;
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

/// Compares canonical path components with Windows' case-insensitive directory identity rules.
///
/// @author kongweiguang
fn path_starts_with(path: &Path, prefix: &Path) -> bool {
    #[cfg(windows)]
    {
        let mut path_components = path.components();
        prefix.components().all(|prefix_component| {
            path_components.next().is_some_and(|path_component| {
                let path_component = path_component.as_os_str();
                let prefix_component = prefix_component.as_os_str();
                path_component == prefix_component
                    || path_component
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&prefix_component.to_string_lossy())
            })
        })
    }
    #[cfg(not(windows))]
    {
        path.starts_with(prefix)
    }
}

/// Copies an isolated config tree into the newly created output directory without following links.
///
/// @author kongweiguang
fn copy_fixture_configuration(source: &Path, destination: &Path) -> Result<(), Box<dyn Error>> {
    let canonical_destination = fs::canonicalize(destination)?;
    if path_starts_with(source, &canonical_destination)
        || path_starts_with(&canonical_destination, source)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fixture 配置目录与验收输出目录不能相互包含",
        )
        .into());
    }
    copy_fixture_directory_contents(source, destination)?;
    Ok(())
}

/// Copies fixture files recursively but refuses symlinks and other special filesystem entries.
///
/// @author kongweiguang
fn copy_fixture_directory_contents(source: &Path, destination: &Path) -> Result<(), io::Error> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fixture 配置目录不能包含符号链接",
            ));
        }
        if file_type.is_dir() {
            fs::create_dir(&destination_path)?;
            copy_fixture_directory_contents(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &destination_path)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fixture 配置目录包含不支持的文件类型",
            ));
        }
    }
    Ok(())
}

/// 显式目录只创建最后一级，存在时拒绝覆盖用户已有验收证据。
///
/// @author kongweiguang
fn create_requested_output_directory(path: &Path) -> Result<PathBuf, io::Error> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(path)?;
    Ok(path.to_path_buf())
}

/// 用时间、进程号和原子目录创建组合生成唯一输出路径，不依赖随机数或覆写策略。
///
/// @author kongweiguang
fn create_unique_output_directory(base: PathBuf) -> Result<PathBuf, io::Error> {
    fs::create_dir_all(&base)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis();
    for attempt in 0..100_u32 {
        let candidate = base.join(format!("{timestamp}-{}-{attempt}", std::process::id()));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "无法为本次右栏验收分配唯一输出目录",
    ))
}

/// 使用生产 Composition 与真正的原生 GPUI renderer；窗口隐藏且不请求 OS 焦点，错误经共享回执返回。
///
/// @author kongweiguang
fn run_hidden_window(
    services: kerminal_ui::AppServices,
    capture: RightRailCaptureContext,
) -> Result<(), Box<dyn Error>> {
    let RightRailCaptureContext {
        output_directory,
        fixture_config_source,
        fixture_host_id,
        remote_sftp_directory,
        local_files_root,
        fixture_container_name,
        container_target_port,
        requested_window_width,
        requested_window_height,
        functional_only,
        capture_only,
        ..
    } = capture;
    let failure = Arc::new(Mutex::new(None::<String>));
    let failure_for_app = Arc::clone(&failure);
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            kerminal_ui::init(cx);
            let window_bounds =
                match acceptance_window_bounds(cx, requested_window_width, requested_window_height)
                {
                    Ok(bounds) => bounds,
                    Err(error) => {
                        set_failure(&failure_for_app, error.to_string());
                        cx.quit();
                        return;
                    }
                };
            let requested_bounds_size = match window_bounds {
                Some(WindowBounds::Windowed(bounds)) => Some(bounds.size),
                Some(WindowBounds::Maximized(_) | WindowBounds::Fullscreen(_)) | None => None,
            };
            let window_options = WindowOptions {
                window_background: WindowBackgroundAppearance::Transparent,
                window_bounds,
                titlebar: Some(TitlebarOptions {
                    title: Some("Kerminal Right Rail Acceptance".into()),
                    ..TitleBar::title_bar_options()
                }),
                show: false,
                focus: false,
                ..TitleBar::window_options()
            };
            let result = gpui_kit::open_window(window_options, cx, |window, cx| {
                window.set_background_appearance(WindowBackgroundAppearance::Transparent);
                if let Some(bounds_size) = requested_bounds_size {
                    // Win32 creates the hidden HWND with CW_USEDEFAULT; resize explicitly so its first client frame uses the requested logical size.
                    window.resize(bounds_size);
                }
                let root = cx.new(|cx| {
                    let mut root = RootView::new(services.clone(), window, cx);
                    root.enable_right_rail_acceptance_activity(cx);
                    root
                });
                let window_handle = window.window_handle();
                if let Err(error) = isolation::capture().and_then(|evidence| {
                    evidence.validate_window_state(window.is_visible(), window.is_window_active())
                }) {
                    set_failure(
                        &failure_for_app,
                        format!("隐藏原生窗口创建隔离检查失败：{error}"),
                    );
                    cx.quit();
                    return root;
                }
                let capture_root = root.clone();
                let capture_services = services.clone();
                let capture_failure = Arc::clone(&failure_for_app);
                cx.spawn(async move |cx| {
                    let result = capture_right_rail(
                        cx,
                        capture_root,
                        window_handle,
                        RightRailCaptureContext {
                            output_directory,
                            fixture_config_source,
                            fixture_host_id,
                            remote_sftp_directory,
                            local_files_root,
                            fixture_container_name,
                            container_target_port,
                            requested_window_width,
                            requested_window_height,
                            functional_only,
                            capture_only,
                            requested_bounds_size: requested_bounds_size
                                .map(|size| (size.width.as_f32(), size.height.as_f32())),
                        },
                        capture_services,
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
                set_failure(
                    &failure_for_app,
                    format!("无法打开隐藏原生验收窗口：{error}"),
                );
                cx.quit();
            }
        });
    let message = failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(message) = message {
        return Err(io::Error::other(message).into());
    }
    Ok(())
}

/// Preserves first-run bounds when size flags are absent and centers explicit logical dimensions on the primary display.
///
/// @author kongweiguang
fn acceptance_window_bounds(
    cx: &App,
    requested_width: Option<u32>,
    requested_height: Option<u32>,
) -> Result<Option<WindowBounds>, io::Error> {
    if requested_width.is_none() && requested_height.is_none() {
        return Ok(super::first_run_window_bounds(cx));
    }
    let default_bounds = super::first_run_window_bounds(cx).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "显式设置验收窗口尺寸时需要可用的主显示器",
        )
    })?;
    let WindowBounds::Windowed(default_bounds) = default_bounds else {
        return Err(io::Error::other("无法从首窗策略读取验收逻辑尺寸"));
    };
    let display = cx.primary_display().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "显式设置验收窗口尺寸时需要可用的主显示器",
        )
    })?;
    let width = requested_width
        .map(|width| width as f32)
        .unwrap_or_else(|| default_bounds.size.width.as_f32());
    let height = requested_height
        .map(|height| height as f32)
        .unwrap_or_else(|| default_bounds.size.height.as_f32());
    let bounds = Bounds::centered_at(
        display.visible_bounds().center(),
        size(px(width), px(height)),
    );
    Ok(Some(WindowBounds::Windowed(bounds)))
}

/// Waits for Root's normal settings and verified fixture-host connection before drawing any evidence.
///
/// @author kongweiguang
async fn wait_for_fixture_connection(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    fixture_host_id: &HostId,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + FIXTURE_READY_TIMEOUT;
    loop {
        let settings_ready = window.update(cx, |_, _, app| {
            root.update(app, |root, _| root.right_rail_acceptance_settings_ready())
        })?;
        if settings_ready {
            break;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("等待隔离验收设置加载超时").into());
        }
        cx.background_executor().timer(ACCEPTANCE_POLL).await;
    }

    loop {
        let request = window.update(cx, |_, _, app| {
            root.update(app, |root, root_cx| {
                root.request_right_rail_acceptance_fixture_connection(fixture_host_id, root_cx)
            })
        })?;
        match request {
            Ok(true) => break,
            Ok(false) => {}
            Err(message) => return Err(io::Error::other(message).into()),
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("隔离 fixture 主机未能加载到连接面板").into());
        }
        cx.background_executor().timer(ACCEPTANCE_POLL).await;
    }

    loop {
        let ready = window.update(cx, |_, _, app| {
            root.update(app, |root, _| {
                root.right_rail_acceptance_fixture_ready(fixture_host_id)
            })
        })?;
        match ready {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(message) => return Err(io::Error::other(message).into()),
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(
                "隔离 SSH fixture 未在时限内成为活动 pane，或尚无真实终端快照",
            )
            .into());
        }
        cx.background_executor().timer(ACCEPTANCE_POLL).await;
    }
}

/// Waits for Win32's asynchronous resize to reach the requested GPUI logical client size without showing or activating the window.
///
/// @author kongweiguang
async fn wait_for_window_size(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    requested_size: Option<(f32, f32)>,
) -> Result<(), Box<dyn Error>> {
    let Some((requested_width, requested_height)) = requested_size else {
        return Ok(());
    };
    let deadline = Instant::now() + WINDOW_RESIZE_TIMEOUT;
    loop {
        let (visible, active, bounds) = window.update(cx, |_, window, _| {
            (
                window.is_visible(),
                window.is_window_active(),
                window.bounds(),
            )
        })?;
        if visible || active {
            window.update(cx, |_, _, _| {
                isolation::capture()
                    .and_then(|evidence| evidence.validate_window_state(visible, active))
            })??;
        }
        let actual_width = bounds.size.width.as_f32();
        let actual_height = bounds.size.height.as_f32();
        if (actual_width - requested_width).abs() <= 1.0
            && (actual_height - requested_height).abs() <= 1.0
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "GPUI 窗口尺寸未生效：请求 {:.0}x{:.0} 逻辑像素，实测 {:.0}x{:.0}",
                requested_width, requested_height, actual_width, actual_height
            ))
            .into());
        }
        cx.background_executor().timer(ACCEPTANCE_POLL).await;
    }
}

/// 按生产 Root 动作捕获三种主题及右栏面板，保留请求尺寸与实测窗口尺寸的独立口径。
/// 受控文件准备沿用组合根的共享 services，避免验收另建执行器或绕开生产文件端口。
/// 工具操作后再验收自定义保存和 Tab 隔离，并恢复设置，避免布局草稿污染前面的主题对照。
/// 子模式允许在同一候选上独立重跑视觉或数据闭环；诊断环境开关只改变检查顺序，不跳过任何检查。
///
/// @author kongweiguang
async fn capture_right_rail(
    cx: &mut AsyncApp,
    root: Entity<RootView>,
    window: AnyWindowHandle,
    capture: RightRailCaptureContext,
    services: kerminal_ui::AppServices,
) -> Result<(), Box<dyn Error>> {
    let output_directory = &capture.output_directory;
    let fixture_config_source = &capture.fixture_config_source;
    let fixture_host_id = &capture.fixture_host_id;
    let requested_window_width = capture.requested_window_width;
    let requested_window_height = capture.requested_window_height;
    let requested_bounds_size = capture.requested_bounds_size;
    wait_for_window_size(cx, window, requested_bounds_size).await?;
    wait_for_fixture_connection(cx, &root, window, fixture_host_id).await?;
    let mut themes = Vec::with_capacity(RIGHT_RAIL_THEMES.len());
    if !capture.functional_only {
        for (mode, theme_name) in RIGHT_RAIL_THEMES {
            themes.push(capture_theme(cx, &root, window, &capture, mode, theme_name).await?);
        }
    }
    if !capture.functional_only
        && themes[0].baseline.pixel_sha256 == themes[1].baseline.pixel_sha256
    {
        return Err(io::Error::other("浅色和深色主题的基线截图完全相同").into());
    }
    let mut functional = Vec::new();
    if !capture.capture_only {
        let shell_first = std::env::var_os("KERMINAL_RIGHT_RAIL_SHELL_FIRST").is_some();
        if shell_first {
            shell::run(cx, &root, window, &capture, &mut functional).await?;
        }
        functional.extend(functional::run(cx, &root, window, &capture, &services).await?);
        if !shell_first {
            shell::run(cx, &root, window, &capture, &mut functional).await?;
        }
    }
    window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.close_right_rail_acceptance_panels(window, root_cx);
        });
    })?;

    let (window_visible, window_active, window_width, window_height, native_isolation) =
        window.update(cx, |_, window, _| {
            let bounds = window.bounds();
            Ok::<_, io::Error>((
                window.is_visible(),
                window.is_window_active(),
                bounds.size.width.as_f32(),
                bounds.size.height.as_f32(),
                isolation::capture()?,
            ))
        })??;
    if !window_width.is_finite()
        || !window_height.is_finite()
        || window_width <= 0.0
        || window_height <= 0.0
    {
        return Err(io::Error::other("验收窗口返回无效尺寸").into());
    }
    native_isolation.validate_window_state(window_visible, window_active)?;
    if let Some((requested_width, requested_height)) = requested_bounds_size
        && ((window_width - requested_width).abs() > 1.0
            || (window_height - requested_height).abs() > 1.0)
    {
        return Err(io::Error::other(format!(
            "右栏截图期间 GPUI 窗口尺寸偏离请求：请求 {:.0}x{:.0}，实测 {:.0}x{:.0}",
            requested_width, requested_height, window_width, window_height
        ))
        .into());
    }

    let report = AcceptanceReport {
        schema_version: 6,
        capture_mode: if capture.capture_only {
            "native_panels_menus_dialogs_and_target_bound_states_without_functional_writes"
        } else if capture.functional_only {
            "native_functional_actions_without_theme_capture"
        } else {
            "root_actions_native_keyboard_panels_dialogs_and_target_bound_states_without_os_input"
        },
        capture_backend: "native_window_render_to_image",
        config_directory: output_directory
            .join("config")
            .to_string_lossy()
            .into_owned(),
        fixture_config_source: fixture_config_source.to_string_lossy().into_owned(),
        fixture_host_id: FIXTURE_HOST_ID.to_owned(),
        fixture_host_key_policy: "existing_known_hosts_required_no_automatic_trust",
        window_size_unit: "gpui_logical_pixels",
        window_size_source: if requested_window_width.is_some() || requested_window_height.is_some()
        {
            "explicit_logical_dimensions_centered_on_primary_display"
        } else {
            "first_run_window_bounds"
        },
        requested_window_width,
        requested_window_height,
        window_visible,
        window_active,
        native_isolation,
        window_width,
        window_height,
        functional,
        themes,
    };
    let bytes = serde_json::to_vec_pretty(&report)?;
    fs::write(output_directory.join("report.json"), bytes)?;
    Ok(())
}

/// Opens the actual owning panel before its dialog so layout and target state match the visible production entry point.
/// Waiting for semantic readiness prevents loading states from being mistaken for a usable form.
/// Compact drawer ownership is checked after its first rendered layout rather than against the previous frame.
///
/// @author kongweiguang
async fn activate_dialog_tool(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    tool: ToolRailToolId,
    fixture_host_id: &HostId,
    tool_name: &'static str,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.activate_right_rail_tool_for_acceptance(tool, window, root_cx);
        })
    })?;
    let _ = frame_signature(cx, window)?;
    let selected = window.update(cx, |_, _, app| {
        root.read(app).right_rail_acceptance_active_tool() == Some(tool)
    })?;
    if !selected {
        return Err(io::Error::other(format!("弹框所属工具 {tool_name} 未能激活")).into());
    }
    wait_for_panel_readiness(cx, root, window, tool, fixture_host_id, tool_name).await
}

/// Resets the active panel, applies the requested Root theme, and records its baseline and tool states.
/// Pixel stability and deferred semantic focus are awaited independently; the driver never assigns focus to satisfy a gate.
/// Each dialog opens from its owning panel, retaining production layout and readiness constraints.
/// Visibility checks follow a rendered frame so compact layouts cannot mistake stale slot projections for the opened drawer.
///
/// @author kongweiguang
async fn capture_theme(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    mode: ThemeMode,
    theme_name: &'static str,
) -> Result<ThemeCaptureRecord, Box<dyn Error>> {
    let output_directory = &capture.output_directory;
    let fixture_host_id = &capture.fixture_host_id;
    let remote_sftp_directory = capture.remote_sftp_directory.as_str();
    let window_appearance = window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.close_right_rail_acceptance_panels(window, root_cx);
            root.set_right_rail_acceptance_theme(mode, root_cx);
        });
        format!("{:?}", window.appearance())
    })?;
    let baseline_file = format!("theme-{theme_name}-baseline.png");
    let baseline = wait_for_stable_frame(
        cx,
        window,
        output_directory.join(&baseline_file),
        None,
        "theme baseline",
    )
    .await?;
    let mut panels = Vec::with_capacity(RIGHT_RAIL_TOOLS.len());
    let mut menus = Vec::new();
    let mut tmux_collapsed_signatures = None;
    for (tool, tool_name) in RIGHT_RAIL_TOOLS {
        window.update(cx, |_, window, app| {
            root.update(app, |root, root_cx| {
                root.activate_right_rail_tool_for_acceptance(tool, window, root_cx);
            })
        })?;
        // Compact visibility belongs to the rendered projection, so assert it only after layout has mounted the drawer.
        let _ = frame_signature(cx, window)?;
        let selected = window.update(cx, |_, _, app| {
            root.read(app).right_rail_acceptance_active_tool() == Some(tool)
        })?;
        if !selected {
            return Err(io::Error::other(format!(
                "工具 {tool_name} 未通过 Root 激活动作成为当前面板"
            ))
            .into());
        }
        if tool == ToolRailToolId::Sftp {
            navigate_and_wait_for_sftp_directory(
                cx,
                root,
                window,
                fixture_host_id,
                remote_sftp_directory,
            )
            .await?;
        } else {
            wait_for_panel_readiness(cx, root, window, tool, fixture_host_id, tool_name).await?;
        }
        if tool == ToolRailToolId::Containers {
            // The tool's Entity is mounted lazily during render; reset navigation only after readiness.
            window
                .update(cx, |_, _, app| {
                    root.update(app, |root, root_cx| {
                        root.return_right_rail_acceptance_container_list(fixture_host_id, root_cx)
                    })
                })?
                .map_err(io::Error::other)?;
        }
        let file = format!("panel-{theme_name}-{tool_name}.png");
        let (capture, pixel_phases) = wait_for_stable_frame_with_candidates(
            cx,
            window,
            output_directory.join(&file),
            None,
            tool_name,
        )
        .await?;
        if capture.pixel_sha256 == baseline.pixel_sha256 {
            return Err(io::Error::other(format!(
                "{theme_name} 主题下工具 {tool_name} 截图与基线完全相同"
            ))
            .into());
        }
        panels.push(PanelCaptureRecord {
            tool: tool_name,
            file,
            width: capture.width,
            height: capture.height,
            selected,
        });
        if tool == ToolRailToolId::Tmux {
            tmux_collapsed_signatures = Some(pixel_phases);
        }
        // Only open and dismiss existing production menus; no item is selected and no fixture data is changed.
        let menu_specs: &[(&str, &str)] = match tool {
            ToolRailToolId::AgentLauncher => &[("agent-picker", "rr-agent-picker")],
            ToolRailToolId::Sftp => &[
                ("files-bookmarks", "rr-files-bookmarks-1"),
                ("files-view", "rr-files-view-menu"),
                ("files-upload", "rr-files-upload-menu"),
            ],
            ToolRailToolId::System => &[("system-interval", "rr-system-interval")],
            ToolRailToolId::Logs => &[("logs-source", "rr-logs-source")],
            ToolRailToolId::Containers => &[
                ("containers-runtime", "rr-containers-runtime"),
                ("containers-group", "rr-containers-group"),
            ],
            _ => &[],
        };
        for (surface, selector) in menu_specs {
            menus.push(
                menus::capture_menu(
                    cx,
                    window,
                    output_directory,
                    theme_name,
                    surface,
                    surface,
                    selector,
                )
                .await?,
            );
        }
    }

    // Capture the modal-owned snippet flows before target-bound state probes can fail independently.
    let mut snippet_dialogs = Vec::with_capacity(3);
    activate_dialog_tool(
        cx,
        root,
        window,
        ToolRailToolId::Snippets,
        fixture_host_id,
        "snippets",
    )
    .await?;
    let snippet_manager_open = window.update(cx, |_, _, app| {
        root.update(app, |root, root_cx| {
            root.open_right_rail_acceptance_snippet_manager(root_cx)
        })
    })?;
    if !snippet_manager_open {
        return Err(io::Error::other("片段库管理器生产入口未能打开").into());
    }
    let snippet_manager_file = format!("dialog-{theme_name}-snippet-library-manager.png");
    let snippet_manager_capture = wait_for_stable_frame(
        cx,
        window,
        output_directory.join(&snippet_manager_file),
        None,
        "snippet library manager",
    )
    .await?;
    if snippet_manager_capture.pixel_sha256 == baseline.pixel_sha256 {
        return Err(
            io::Error::other(format!("{theme_name} 主题下片段库管理器截图与基线完全相同")).into(),
        );
    }
    dismiss_dialog_with_native_keys(cx, window)?;
    wait_for_acceptance_dialog_closed(cx, root, window, "片段库管理器", |root, root_cx| {
        root.right_rail_acceptance_snippet_manager_closed(root_cx)
    })
    .await?;
    snippet_dialogs.push(DialogCaptureRecord {
        surface: "snippet-library-manager",
        capture: snippet_manager_capture,
        open: snippet_manager_open,
        dismissal: "native_tab_reverse_tab_escape_verified_closed",
    });

    let snippet_editor_open = window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.open_right_rail_acceptance_snippet_editor(window, root_cx)
        })
    })?;
    if !snippet_editor_open {
        return Err(io::Error::other("新建片段编辑器生产入口未能打开").into());
    }
    let snippet_editor_file = format!("dialog-{theme_name}-snippet-editor.png");
    let snippet_editor_capture = wait_for_stable_frame(
        cx,
        window,
        output_directory.join(&snippet_editor_file),
        None,
        "new snippet editor",
    )
    .await?;
    if snippet_editor_capture.pixel_sha256 == baseline.pixel_sha256 {
        return Err(io::Error::other(format!(
            "{theme_name} 主题下新建片段编辑器截图与基线完全相同"
        ))
        .into());
    }
    dismiss_dialog_with_native_keys(cx, window)?;
    wait_for_acceptance_dialog_closed(
        cx,
        root,
        window,
        "新建片段编辑器",
        |root, root_cx| root.right_rail_acceptance_snippet_editor_closed(root_cx),
    )
    .await?;
    snippet_dialogs.push(DialogCaptureRecord {
        surface: "new-snippet-editor",
        capture: snippet_editor_capture,
        open: snippet_editor_open,
        dismissal: "native_tab_reverse_tab_escape_verified_closed",
    });
    snippet_dialogs.push(snippet_import::capture(cx, root, window, capture, theme_name).await?);

    let mut states = Vec::with_capacity(5);
    activate_dialog_tool(
        cx,
        root,
        window,
        ToolRailToolId::System,
        fixture_host_id,
        "system",
    )
    .await?;
    for (page, surface) in [
        ("resources", "system-resources"),
        ("processes", "system-processes"),
    ] {
        window
            .update(cx, |_, _, app| {
                root.update(app, |root, root_cx| {
                    root.select_right_rail_acceptance_system_page(page, fixture_host_id, root_cx)
                })
            })?
            .map_err(io::Error::other)?;
        wait_for_acceptance_state(cx, root, window, surface, |root, root_cx| {
            root.right_rail_acceptance_system_page_ready(page, fixture_host_id, root_cx)
        })
        .await?;
        states.push(
            capture_state(
                cx,
                window,
                output_directory,
                &baseline.pixel_sha256,
                StateCaptureMetadata {
                    theme_name,
                    surface,
                    host_id: fixture_host_id.as_str().to_owned(),
                    target_id: None,
                    interaction: "acceptance_page_selection_and_production_snapshot",
                },
            )
            .await?,
        );
    }

    activate_dialog_tool(
        cx,
        root,
        window,
        ToolRailToolId::Containers,
        fixture_host_id,
        "containers",
    )
    .await?;
    let container_id = wait_for_acceptance_value(
        cx,
        root,
        window,
        "running container target",
        |root, root_cx| {
            root.right_rail_acceptance_container_target_id(
                fixture_host_id,
                &capture.fixture_container_name,
                root_cx,
            )
        },
    )
    .await?;
    for (page, surface) in [
        ("details", "container-details"),
        ("stats", "container-monitor"),
    ] {
        window
            .update(cx, |_, _, app| {
                root.update(app, |root, root_cx| {
                    root.open_right_rail_acceptance_container_page(
                        fixture_host_id,
                        &container_id,
                        page,
                        root_cx,
                    )
                })
            })?
            .map_err(io::Error::other)?;
        wait_for_acceptance_state(cx, root, window, surface, |root, root_cx| {
            root.right_rail_acceptance_container_page_ready(
                fixture_host_id,
                &container_id,
                page,
                root_cx,
            )
        })
        .await?;
        states.push(
            capture_state(
                cx,
                window,
                output_directory,
                &baseline.pixel_sha256,
                StateCaptureMetadata {
                    theme_name,
                    surface,
                    host_id: fixture_host_id.as_str().to_owned(),
                    target_id: Some(container_id.clone()),
                    interaction: "acceptance_navigation_and_production_container_read",
                },
            )
            .await?,
        );
        if page == "details" {
            menus.push(
                menus::capture_menu(
                    cx,
                    window,
                    output_directory,
                    theme_name,
                    "container-actions",
                    "container-actions",
                    &format!("rr-container-more-{container_id}"),
                )
                .await?,
            );
        }
    }

    activate_dialog_tool(
        cx,
        root,
        window,
        ToolRailToolId::Tmux,
        fixture_host_id,
        "tmux",
    )
    .await?;
    let session_id =
        wait_for_acceptance_value(cx, root, window, "fixture tmux session", |root, root_cx| {
            root.right_rail_acceptance_tmux_session_id(fixture_host_id, root_cx)
        })
        .await?;
    wait_for_acceptance_state(cx, root, window, "tmux session target", |root, root_cx| {
        root.right_rail_acceptance_tmux_session_ready(fixture_host_id, &session_id, root_cx)
    })
    .await?;
    let expanded = window.update(cx, |_, _, app| {
        root.update(app, |root, root_cx| {
            root.set_right_rail_acceptance_tmux_session_expanded(
                fixture_host_id,
                &session_id,
                true,
                root_cx,
            )
        })
    })?;
    if !expanded.map_err(io::Error::other)? {
        return Err(io::Error::other("tmux 生产折叠状态未进入展开态").into());
    }
    let tmux_expanded = wait_for_stable_frame(
        cx,
        window,
        output_directory.join(format!("state-{theme_name}-tmux-session-expanded.png")),
        None,
        "tmux expanded session",
    )
    .await?;
    let tmux_collapsed_signatures = tmux_collapsed_signatures
        .ok_or_else(|| io::Error::other("tmux 详情截图缺少同主题折叠面板像素候选"))?;
    if tmux_collapsed_signatures
        .iter()
        .any(|signature| signature == &tmux_expanded.pixel_sha256)
    {
        return Err(io::Error::other(format!(
            "{theme_name} 主题下 tmux session 点击后没有展开可见详情"
        ))
        .into());
    }
    wait_for_acceptance_state(
        cx,
        root,
        window,
        "tmux session target after expansion",
        |root, root_cx| {
            root.right_rail_acceptance_tmux_session_ready(fixture_host_id, &session_id, root_cx)
        },
    )
    .await?;
    states.push(StateCaptureRecord {
        surface: "tmux-session-expanded",
        capture: tmux_expanded,
        host_id: fixture_host_id.as_str().to_owned(),
        target_id: Some(session_id.clone()),
        interaction: "production_tmux_expansion_state_with_pixel_difference",
    });
    let collapsed = window.update(cx, |_, _, app| {
        root.update(app, |root, root_cx| {
            root.set_right_rail_acceptance_tmux_session_expanded(
                fixture_host_id,
                &session_id,
                false,
                root_cx,
            )
        })
    })?;
    if collapsed.map_err(io::Error::other)? {
        return Err(io::Error::other("tmux 生产折叠状态未恢复折叠").into());
    }
    wait_for_acceptance_state(
        cx,
        root,
        window,
        "tmux collapsed session target",
        |root, root_cx| {
            root.right_rail_acceptance_tmux_session_ready(fixture_host_id, &session_id, root_cx)
        },
    )
    .await?;
    wait_for_expected_frame_candidates(
        cx,
        window,
        &tmux_collapsed_signatures,
        "tmux collapsed session",
    )
    .await?;

    let opened = window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.open_right_rail_acceptance_customizer(window, root_cx)
        })
    })?;
    if !opened {
        return Err(io::Error::other("右栏生产 customizer 打开失败").into());
    }
    let dialog_file = format!("dialog-{theme_name}-tool-rail-customizer.png");
    let dialog_capture = wait_for_stable_frame(
        cx,
        window,
        output_directory.join(&dialog_file),
        None,
        "tool rail customizer",
    )
    .await?;
    if dialog_capture.pixel_sha256 == baseline.pixel_sha256 {
        return Err(
            io::Error::other(format!("{theme_name} 主题下 customizer 截图与基线完全相同")).into(),
        );
    }
    let closed = window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.close_right_rail_acceptance_customizer(window, root_cx)
        })
    })?;
    if !closed {
        return Err(io::Error::other("右栏 customizer 未能在截图后关闭").into());
    }

    let mut dialogs = vec![DialogCaptureRecord {
        surface: "tool-rail-customizer",
        capture: dialog_capture,
        open: opened,
        dismissal: "production_close_action",
    }];

    activate_dialog_tool(
        cx,
        root,
        window,
        ToolRailToolId::AgentLauncher,
        fixture_host_id,
        "agent-launcher",
    )
    .await?;
    let agent_manager_open = window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.open_right_rail_acceptance_agent_manager(window, root_cx)
        })
    })?;
    if !agent_manager_open {
        return Err(io::Error::other("Agent 管理器生产入口未能打开").into());
    }
    let agent_manager_file = format!("dialog-{theme_name}-agent-manager.png");
    let agent_manager_capture = wait_for_stable_frame(
        cx,
        window,
        output_directory.join(&agent_manager_file),
        None,
        "Agent manager",
    )
    .await?;
    if agent_manager_capture.pixel_sha256 == baseline.pixel_sha256 {
        return Err(io::Error::other(format!(
            "{theme_name} 主题下 Agent 管理器截图与基线完全相同"
        ))
        .into());
    }
    dismiss_dialog_with_native_keys(cx, window)?;
    wait_for_acceptance_dialog_closed(cx, root, window, "Agent 管理器", |root, _| {
        root.right_rail_acceptance_agent_manager_closed()
    })
    .await?;
    dialogs.push(DialogCaptureRecord {
        surface: "agent-manager",
        capture: agent_manager_capture,
        open: agent_manager_open,
        dismissal: "native_tab_reverse_tab_escape_verified_closed",
    });
    dialogs.extend(snippet_dialogs);

    activate_dialog_tool(
        cx,
        root,
        window,
        ToolRailToolId::Tmux,
        fixture_host_id,
        "tmux",
    )
    .await?;
    let tmux_create_open = window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.open_right_rail_acceptance_tmux_create(window, root_cx)
        })
    })?;
    if !tmux_create_open {
        return Err(io::Error::other("tmux 创建确认生产入口未能打开").into());
    }
    let tmux_create_file = format!("dialog-{theme_name}-tmux-create.png");
    let tmux_create_capture = wait_for_stable_frame(
        cx,
        window,
        output_directory.join(&tmux_create_file),
        None,
        "tmux create session",
    )
    .await?;
    if tmux_create_capture.pixel_sha256 == baseline.pixel_sha256 {
        return Err(io::Error::other(format!(
            "{theme_name} 主题下 tmux 创建弹框截图与基线完全相同"
        ))
        .into());
    }
    dismiss_dialog_with_native_keys(cx, window)?;
    wait_for_acceptance_dialog_closed(cx, root, window, "tmux 创建弹框", |root, root_cx| {
        root.right_rail_acceptance_tmux_create_closed(root_cx)
    })
    .await?;
    dialogs.push(DialogCaptureRecord {
        surface: "tmux-create-session",
        capture: tmux_create_capture,
        open: tmux_create_open,
        dismissal: "native_tab_reverse_tab_escape_verified_closed",
    });

    activate_dialog_tool(
        cx,
        root,
        window,
        ToolRailToolId::Ports,
        fixture_host_id,
        "ports",
    )
    .await?;
    let forward_add_open = window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.open_right_rail_acceptance_forward_add(window, root_cx)
        })
    })?;
    if !forward_add_open {
        return Err(io::Error::other("SSH 隧道新增表单生产入口未能打开").into());
    }
    let forward_add_file = format!("dialog-{theme_name}-forward-add.png");
    let forward_add_capture = wait_for_stable_frame(
        cx,
        window,
        output_directory.join(&forward_add_file),
        None,
        "forward add form",
    )
    .await?;
    if forward_add_capture.pixel_sha256 == baseline.pixel_sha256 {
        return Err(io::Error::other(format!(
            "{theme_name} 主题下 SSH 隧道新增表单截图与基线完全相同"
        ))
        .into());
    }
    dismiss_dialog_with_native_keys(cx, window)?;
    wait_for_acceptance_dialog_closed(
        cx,
        root,
        window,
        "SSH 隧道新增表单",
        |root, root_cx| root.right_rail_acceptance_forward_add_closed(root_cx),
    )
    .await?;
    dialogs.push(DialogCaptureRecord {
        surface: "forward-add",
        capture: forward_add_capture,
        open: forward_add_open,
        dismissal: "native_tab_reverse_tab_escape_verified_closed",
    });

    // SFTP readiness requires the fixture path; mount first, then navigate before checking the exact listing.
    window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.activate_right_rail_tool_for_acceptance(ToolRailToolId::Sftp, window, root_cx);
        })
    })?;
    navigate_and_wait_for_sftp_directory(cx, root, window, fixture_host_id, remote_sftp_directory)
        .await?;
    let file_create_open = window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.open_right_rail_acceptance_file_create_directory(window, root_cx)
        })
    })?;
    if !file_create_open {
        return Err(io::Error::other("文件新建目录生产入口未能打开全局弹框").into());
    }
    let file_create_file = format!("dialog-{theme_name}-file-create-directory.png");
    let file_create_capture = wait_for_stable_frame(
        cx,
        window,
        output_directory.join(&file_create_file),
        None,
        "file create directory",
    )
    .await?;
    if file_create_capture.pixel_sha256 == baseline.pixel_sha256 {
        return Err(io::Error::other(format!(
            "{theme_name} 主题下文件新建目录弹框截图与基线完全相同"
        ))
        .into());
    }
    functional::poll_root(
        cx,
        root,
        window,
        "文件弹框初始关闭按钮焦点",
        |root, window, cx| {
            if root.right_rail_acceptance_file_create_directory_closed(cx) {
                return Err("文件弹框在初始焦点就绪前已关闭".to_owned());
            }
            Ok(root
                .right_rail_acceptance_file_dialog_close_focused(window, cx)
                .then_some(()))
        },
    )
    .await?;
    dismiss_dialog_with_native_keys(cx, window)?;
    wait_for_acceptance_dialog_closed(
        cx,
        root,
        window,
        "文件新建目录弹框",
        |root, root_cx| root.right_rail_acceptance_file_create_directory_closed(root_cx),
    )
    .await?;
    dialogs.push(DialogCaptureRecord {
        surface: "file-create-directory",
        capture: file_create_capture,
        open: file_create_open,
        dismissal: "native_tab_reverse_tab_escape_verified_closed",
    });

    Ok(ThemeCaptureRecord {
        mode: theme_name,
        window_appearance,
        baseline,
        panels,
        states,
        dialogs,
        menus,
    })
}

/// Exercises Tab, reverse Tab and Escape through the real native window dispatch tree without moving the OS cursor or foreground focus.
/// The caller separately waits for the production overlay owner to be released, so an ignored Escape fails acceptance.
/// Raw key pairs avoid dispatch_keystroke's simulated IME text fallback for Tab.
///
/// @author kongweiguang
fn dismiss_dialog_with_native_keys(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    for key in ["tab", "shift-tab", "escape"] {
        let keystroke = gpui_kit::Keystroke::parse(key)?;
        window.update(cx, |_, window, app| {
            let contexts = window.context_stack();
            let result = window.dispatch_event(
                gpui_kit::PlatformInput::KeyDown(gpui_kit::KeyDownEvent {
                    keystroke: keystroke.clone(),
                    is_held: false,
                    prefer_character_input: false,
                }),
                app,
            );
            window.dispatch_event(
                gpui_kit::PlatformInput::KeyUp(gpui_kit::KeyUpEvent { keystroke }),
                app,
            );
            eprintln!(
                "acceptance keyboard key={key} handled={} contexts={contexts:?}",
                !result.propagate
            );
        })?;
        let _ = frame_signature(cx, window)?;
    }
    Ok(())
}

/// Draws and polls Root after close requests because feature and Root overlay events settle asynchronously.
///
/// @author kongweiguang
async fn wait_for_acceptance_dialog_closed<F>(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    surface: &str,
    is_closed: F,
) -> Result<(), Box<dyn Error>>
where
    F: Fn(&RootView, &Context<RootView>) -> bool,
{
    let deadline = Instant::now() + PANEL_FRAME_TIMEOUT;
    loop {
        let closed = window.update(cx, |_, _, app| {
            root.update(app, |root, root_cx| is_closed(root, root_cx))
        })?;
        if closed {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待 {surface} 关闭超时")).into());
        }
        let _ = frame_signature(cx, window)?;
        cx.background_executor().timer(ACCEPTANCE_POLL).await;
    }
}

/// 首帧先完成 SFTP 的生产懒装配，再导航 fixture 目录并等待精确 endpoint 的 listing 提交。
///
/// @author kongweiguang
async fn navigate_and_wait_for_sftp_directory(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    fixture_host_id: &HostId,
    remote_sftp_directory: &str,
) -> Result<(), Box<dyn Error>> {
    let _ = frame_signature(cx, window)?;
    let navigation = window.update(cx, |_, _, app| {
        root.update(app, |root, root_cx| {
            root.navigate_right_rail_acceptance_sftp_directory(
                fixture_host_id,
                remote_sftp_directory,
                root_cx,
            )
        })
    })?;
    navigation.map_err(io::Error::other)?;
    wait_for_acceptance_state(
        cx,
        root,
        window,
        "fixture SFTP directory",
        |root, root_cx| {
            root.right_rail_acceptance_sftp_directory_ready(
                fixture_host_id,
                remote_sftp_directory,
                root_cx,
            )
        },
    )
    .await
}

/// Polls production readiness for the selected fixture target before pixel stability can accept a panel.
///
/// @author kongweiguang
async fn wait_for_panel_readiness(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    tool: ToolRailToolId,
    fixture_host_id: &HostId,
    tool_name: &str,
) -> Result<(), Box<dyn Error>> {
    let _ = frame_signature(cx, window)?;
    let deadline = Instant::now() + PANEL_FRAME_TIMEOUT;
    loop {
        let readiness = window.update(cx, |_, _, app| {
            root.update(app, |root, root_cx| {
                if root.right_rail_acceptance_active_tool() != Some(tool) {
                    return Err(format!("{tool_name} 面板已不再是活动工具"));
                }
                root.right_rail_acceptance_panel_ready(tool, fixture_host_id, root_cx)
            })
        })?;
        match readiness {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(message) => {
                return Err(io::Error::other(format!(
                    "读取 {tool_name} 面板就绪状态失败：{message}"
                ))
                .into());
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待 {tool_name} 面板就绪超时")).into());
        }
        cx.background_executor().timer(ACCEPTANCE_POLL).await;
    }
}

/// Waits for a production-state predicate to become ready while propagating target mismatches as immediate failures.
///
/// @author kongweiguang
async fn wait_for_acceptance_state<F>(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    surface: &str,
    is_ready: F,
) -> Result<(), Box<dyn Error>>
where
    F: Fn(&RootView, &Context<RootView>) -> Result<bool, String>,
{
    let deadline = Instant::now() + PANEL_FRAME_TIMEOUT;
    loop {
        let result = window.update(cx, |_, _, app| {
            root.update(app, |root, root_cx| is_ready(root, root_cx))
        })?;
        match result {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(message) => {
                return Err(
                    io::Error::other(format!("读取 {surface} 验收状态失败：{message}")).into(),
                );
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待 {surface} 验收状态超时")).into());
        }
        let _ = frame_signature(cx, window)?;
        cx.background_executor().timer(ACCEPTANCE_POLL).await;
    }
}

/// Waits for an actual fixture identity to appear; empty service snapshots never get replaced with synthetic UI data.
///
/// @author kongweiguang
async fn wait_for_acceptance_value<T, F>(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    surface: &str,
    read_value: F,
) -> Result<T, Box<dyn Error>>
where
    F: Fn(&RootView, &Context<RootView>) -> Result<Option<T>, String>,
{
    let deadline = Instant::now() + FIXTURE_READY_TIMEOUT;
    loop {
        let result = window.update(cx, |_, _, app| {
            root.update(app, |root, root_cx| read_value(root, root_cx))
        })?;
        match result {
            Ok(Some(value)) => return Ok(value),
            Ok(None) => {}
            Err(message) => {
                return Err(
                    io::Error::other(format!("读取 {surface} 验收目标失败：{message}")).into(),
                );
            }
        }
        if Instant::now() >= deadline {
            return Err(
                io::Error::other(format!("隔离 fixture 未提供 {surface}，拒绝使用假目标")).into(),
            );
        }
        let _ = frame_signature(cx, window)?;
        cx.background_executor().timer(ACCEPTANCE_POLL).await;
    }
}

/// Captures a named right-rail state and rejects a byte-identical closed-window baseline.
///
/// @author kongweiguang
async fn capture_state(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    output_directory: &Path,
    baseline_signature: &str,
    metadata: StateCaptureMetadata,
) -> Result<StateCaptureRecord, Box<dyn Error>> {
    let capture = wait_for_stable_frame(
        cx,
        window,
        output_directory.join(format!(
            "state-{}-{}.png",
            metadata.theme_name, metadata.surface
        )),
        None,
        metadata.surface,
    )
    .await?;
    if capture.pixel_sha256 == baseline_signature {
        return Err(io::Error::other(format!(
            "{} 主题下 {} 与右栏关闭基线完全相同",
            metadata.theme_name, metadata.surface
        ))
        .into());
    }
    Ok(StateCaptureRecord {
        surface: metadata.surface,
        capture,
        host_id: metadata.host_id,
        target_id: metadata.target_id,
        interaction: metadata.interaction,
    })
}

/// Tracks repeated matches against a fixed, already-stable full-frame baseline.
///
/// @author kongweiguang
struct ExpectedFrameRestoration {
    signatures: Vec<String>,
    stable_samples: usize,
}

impl ExpectedFrameRestoration {
    /// Copies only previously accepted baseline phases; later observations cannot widen this set.
    ///
    /// @author kongweiguang
    fn new(signatures: &[String]) -> Self {
        Self {
            signatures: signatures.to_vec(),
            stable_samples: 0,
        }
    }

    /// Counts consecutive whole-frame matches and resets on every pixel signature outside the baseline.
    ///
    /// @author kongweiguang
    fn observe(&mut self, signature: &str) -> bool {
        if self.signatures.iter().any(|expected| expected == signature) {
            self.stable_samples += 1;
        } else {
            self.stable_samples = 0;
        }
        self.stable_samples >= PANEL_STABLE_FRAME_COUNT
    }
}

/// Waits for the exact prior whole-window content, allowing only phases proven stable in its baseline.
///
/// @author kongweiguang
async fn wait_for_expected_frame_candidates(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    expected: &[String],
    surface: &str,
) -> Result<(), Box<dyn Error>> {
    if expected.is_empty() {
        return Err(io::Error::other(format!("{surface} 缺少已验证的全窗像素基线")).into());
    }
    let deadline = Instant::now() + PANEL_FRAME_TIMEOUT;
    let mut restoration = ExpectedFrameRestoration::new(expected);
    loop {
        let signature = frame_signature(cx, window)?;
        if restoration.observe(&signature) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待 {surface} 恢复到已验证基线帧超时")).into());
        }
        cx.background_executor().timer(PANEL_FRAME_POLL).await;
    }
}

/// Tracks only the two known complete pixel phases caused by a focused input caret.
/// A third phase starts a fresh run, so unrelated animation or loading cannot be hidden.
///
/// @author kongweiguang
#[derive(Default)]
struct StableFrameCandidates {
    signatures: Vec<String>,
    stable_samples: usize,
}

impl StableFrameCandidates {
    /// The first frame establishes a baseline; recurring samples from either known phase advance stability.
    /// The second phase's first appearance is not recurrence, and a third phase replaces both candidates.
    ///
    /// @author kongweiguang
    fn observe(&mut self, signature: &str) -> bool {
        if self.signatures.is_empty() {
            self.signatures.push(signature.to_owned());
        } else if self.contains(signature) {
            self.stable_samples += 1;
        } else if self.signatures.len() < PANEL_MAX_FRAME_PHASES {
            self.signatures.push(signature.to_owned());
        } else {
            self.signatures.clear();
            self.signatures.push(signature.to_owned());
            self.stable_samples = 0;
        }
        self.is_stable()
    }

    /// Confirms the exact saved frame is one of the renderer phases that converged before capture.
    ///
    /// @author kongweiguang
    fn contains(&self, signature: &str) -> bool {
        self.signatures
            .iter()
            .any(|candidate| candidate == signature)
    }

    /// Allows a saved image only after convergence and only for one of the two observed pixel phases.
    ///
    /// @author kongweiguang
    fn accepts_saved_capture(&self, signature: &str) -> bool {
        self.is_stable() && self.contains(signature)
    }

    /// Requires two observations after the initial baseline, preserving the existing stability threshold.
    ///
    /// @author kongweiguang
    fn is_stable(&self) -> bool {
        self.stable_samples >= PANEL_STABLE_FRAME_COUNT
    }
}

/// Requires recurring complete renderer frames after business readiness, allowing the focused input's two cursor phases.
/// Every pixel must belong to those two candidates; unrelated animation or loading resets the run.
///
/// @author kongweiguang
async fn wait_for_stable_frame(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    path: PathBuf,
    initial_frame: Option<String>,
    surface: &str,
) -> Result<CaptureRecord, Box<dyn Error>> {
    let (capture, _) =
        wait_for_stable_frame_with_candidates(cx, window, path, initial_frame, surface).await?;
    Ok(capture)
}

/// Returns the exact phases that stabilized the saved frame so state restoration can match only that baseline.
///
/// @author kongweiguang
async fn wait_for_stable_frame_with_candidates(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    path: PathBuf,
    initial_frame: Option<String>,
    surface: &str,
) -> Result<(CaptureRecord, Vec<String>), Box<dyn Error>> {
    let deadline = Instant::now() + PANEL_FRAME_TIMEOUT;
    let mut first_panel_frame = None;
    let mut content_ready = initial_frame.is_none();
    let mut frame_candidates = StableFrameCandidates::default();

    loop {
        let frame = frame_signature(cx, window)?;
        if let Some(initial_frame) = initial_frame.as_ref() {
            match first_panel_frame.as_ref() {
                Some(first_panel_frame) if frame != *first_panel_frame => content_ready = true,
                None if frame != *initial_frame => first_panel_frame = Some(frame.clone()),
                _ => {}
            }
        }

        if content_ready && frame_candidates.observe(&frame) {
            let capture = save_frame(cx, window, path.clone())?;
            if frame_candidates.accepts_saved_capture(&capture.pixel_sha256) {
                let (logical_width, logical_height, scale_factor) =
                    window.update(cx, |_, window, _| {
                        let bounds = window.bounds();
                        (
                            bounds.size.width.as_f32(),
                            bounds.size.height.as_f32(),
                            window.scale_factor(),
                        )
                    })?;
                let expected_width = logical_width * scale_factor;
                let expected_height = logical_height * scale_factor;
                if (capture.width as f32 - expected_width).abs() > 1.0
                    || (capture.height as f32 - expected_height).abs() > 1.0
                {
                    return Err(io::Error::other(format!(
                            "{surface} renderer 尺寸与 GPUI 窗口不匹配：窗口 {:.0}x{:.0} 逻辑像素、scale {:.2}，截图 {}x{} 像素",
                            logical_width,
                            logical_height,
                            scale_factor,
                            capture.width,
                            capture.height
                        ))
                        .into());
                }
                return Ok((capture, frame_candidates.signatures));
            }
            frame_candidates.observe(&capture.pixel_sha256);
        }

        if Instant::now() >= deadline {
            let reason = if initial_frame.is_some() && first_panel_frame.is_none() {
                "面板未能渲染首屏"
            } else if !content_ready {
                "面板仍停留在初始内容"
            } else {
                "面板帧未能稳定"
            };
            save_frame(cx, window, path.with_extension("unstable-before.png"))?;
            cx.background_executor().timer(PANEL_FRAME_POLL).await;
            save_frame(cx, window, path.with_extension("unstable-after.png"))?;
            return Err(io::Error::other(format!("等待 {surface} 稳定超时（{reason}）")).into());
        }
        cx.background_executor().timer(PANEL_FRAME_POLL).await;
    }
}

#[cfg(test)]
#[path = "../tests/unit/right_rail_frame_stability.rs"]
mod frame_stability_tests;

/// 从原生 renderer 采样当前窗口像素摘要，稳定等待与最终截图使用同一帧证据来源。
/// 隐藏 HWND 没有可见帧循环，绘制后显式交付 Kit 的下一帧回调，令生产焦点转移与可见窗口一致。
/// 每次采样立即检查平台状态，意外激活时停止当前步骤，不等整批截图完成才发现问题。
///
/// @author kongweiguang
fn frame_signature(
    cx: &mut AsyncApp,
    window_handle: AnyWindowHandle,
) -> Result<String, Box<dyn Error>> {
    let signature = window_handle.update(cx, |_, window, app| {
        let visible = window.is_visible();
        let active = window.is_window_active();
        isolation::capture()?.validate_window_state(visible, active)?;
        let clear = window.draw(app);
        clear.clear(app);
        if window.simulate_next_frame(app) > 0 {
            let clear = window.draw(app);
            clear.clear(app);
        }
        let image = window.render_to_image().map_err(io::Error::other)?;
        if image.width() == 0 || image.height() == 0 {
            return Err(io::Error::other("原生 renderer 返回零尺寸截图"));
        }
        if !image
            .as_raw()
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] != 0)
        {
            return Err(io::Error::other("原生 renderer 返回全透明截图"));
        }
        Ok::<_, io::Error>(format!("{:x}", Sha256::digest(image.as_raw())))
    })??;
    Ok(signature)
}

/// 强制渲染并保存最终验收帧；零尺寸或全透明帧不作为成功证据。
///
/// @author kongweiguang
fn save_frame(
    cx: &mut AsyncApp,
    window_handle: AnyWindowHandle,
    path: PathBuf,
) -> Result<CaptureRecord, Box<dyn Error>> {
    let file = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "截图文件名无效"))?
        .to_owned();
    let capture = window_handle.update(cx, |_, window, app| {
        let clear = window.draw(app);
        clear.clear(app);
        let image = window.render_to_image().map_err(io::Error::other)?;
        let width = image.width();
        let height = image.height();
        if width == 0 || height == 0 {
            return Err(io::Error::other("原生 renderer 返回零尺寸截图"));
        }
        if !image
            .as_raw()
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] != 0)
        {
            return Err(io::Error::other("原生 renderer 返回全透明截图"));
        }
        let pixel_sha256 = format!("{:x}", Sha256::digest(image.as_raw()));
        image.save(&path).map_err(io::Error::other)?;
        Ok::<_, io::Error>((width, height, pixel_sha256))
    })??;
    Ok(CaptureRecord {
        file,
        width: capture.0,
        height: capture.1,
        pixel_sha256: capture.2,
    })
}

/// 异步 UI 任务只设置一次首要失败原因，保留最初能定位问题的回执。
///
/// @author kongweiguang
fn set_failure(failure: &Mutex<Option<String>>, message: String) {
    let mut slot = failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.is_none() {
        *slot = Some(message);
    }
}
