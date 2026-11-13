//! 隔离原生窗口中的右栏分屏切换验收。
//!
//! 只通过生产控件切换 pane 焦点和分屏目标，再用命令历史的精确条目 ID 证明
//! 右栏数据跟随当前 pane；固定区域的几何和像素哈希独立记录，不用一次截图代替过渡证据。
//!
//! @author kongweiguang

use std::{
    collections::HashSet,
    error::Error,
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use gpui_kit::{
    AnyWindowHandle, AsyncApp, Entity, Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent,
};
use gpui_pre::{Bounds, Pixels, px};
use kerminal_application::preferences::{ProfileDraft, ToolRailToolId};
use kerminal_domain::{
    commands::{CommandHistoryListRequest, CommandHistorySource, CommandHistoryTarget},
    identity::{PaneId, SessionId},
};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::*;

const RIGHT_RAIL_SPLIT_SWITCH_ENV: &str = "KERMINAL_RIGHT_RAIL_SPLIT_SWITCH_ACCEPTANCE";
pub(super) const REPORT_FILE: &str = "right-rail-split-switch-report.json";
pub(super) const CLEANUP_FILE: &str = "right-rail-split-switch-cleanup.json";
const FIRST_SAMPLE_COVERAGE: &str = "single-draw-after-click-dispatch-in-same-window-update; renderer-first-present-and-first-gpui-draw-not-proven";
// 计时起点在实际 click_bounds 之前，不包含点击前的 selector 查找/预绘制；首样本与过渡帧
// 都从同一 click 时刻起算，但各自在截图、哈希和落盘完成后读取 elapsed。
const TIMING_ORIGIN: &str = "Instant::now immediately before synthetic click dispatch, after pre-click selector lookup and viewport validation; first-sample duration includes click dispatch, one explicit draw/clear, render_to_image, region hashing, PNG save, and row checks; transition/settled durations use the same click origin and include each frame's settle wait, capture work, and final copy";
const TIMING_PRECISION: &str = "microseconds";
const ROW_EVIDENCE_SCOPE: &str =
    "rendered-logs-page-selectors; not-a-full-history-or-backing-store-enumeration";
const INTERMEDIATE_CLAIM: &str =
    "target-data-visible-old-data-absent-intermediate-sample-not-animation";
// workbench_motion 的 show_private_motion_window 只编译在 acceptance-metrics 特性下，
// 且按左栏侧栏动效窗口的标题/焦点语义校验；右栏当前构建不含该特性、也不允许把隐藏
// 采样改成可见窗口，因此这里不借用它，继续明确标注 normal-window present 未验证。
const TARGET_DATA_BOUNDARY: &str = "hidden-window-manual-draw; window is hidden and may be OS-inactive; its active state is reported separately and is not a normal-window present proof; intermediate target-data samples only; normal-window present, opacity, running state, and 140ms fade are not instrumented or proven; hash difference is not an animation measurement";
const FADE_LIMITATION: &str = "target-data intermediate samples only; normal-window present, opacity, running state, and 140ms fade duration are not instrumented or proven; hash difference is not an animation measurement";
const NO_INTERMEDIATE_LIMITATION: &str = "no transition sample showed the active target pane with the new row visible/present and the old row absent; normal-window present, opacity, running state, and 140ms fade duration are also not instrumented or proven";
const OBSERVATION_BOUNDARY: &str = "hidden-window; window is hidden and may be OS-inactive; acceptance-active override enabled; manual draw_acceptance_frame/window.draw+clear; window active state is reported separately and is not a normal-window present proof; target-data intermediate samples only; normal-window present, opacity, running state, and 140ms fade are not proven; KnownFolder paths are not redirected and the process is not a file sandbox";
const STATUS_SCOPE: &str = "right-rail-target-data-switch-and-environment; not-animation-not-window-present-not-file-sandbox";
const PIXEL_MIN_DISTINCT_RGB: u32 = 8;
const PIXEL_MIN_LUMA_RANGE: u8 = 8;
const LOGS_LIST_MIN_DISTINCT_RGB: u32 = 4;
const LOGS_LIST_MIN_LUMA_RANGE: u8 = 4;
const ENVIRONMENT_PROBE_SCOPE: &str = "outer-isolated-pwsh-probe-only; child shell inherits trusted modules and is not separately probed";
const PSREADLINE_HISTORY_SCOPE: &str = "outer-isolated-pwsh-probe-plus-run-root-env-mapping; child shell actual path is inferred, not probed";
const KNOWN_FOLDER_GUARD: &str = "checkpoints only: initial active tool must be None or Logs and final active tool must be Logs; transient activation and other components' KnownFolder access are not proven";
const TRUSTED_MODULE_COMMANDS_SCOPE: &str = "outer-isolated-pwsh-probe-only; child shell inherits trusted modules but is not separately probed";
const PROFILE_B_ID: &str = "acceptance-right-rail-split-b";
const PROFILE_B_NAME: &str = "Acceptance Right Rail Split B";
const PROFILE_B_CWD_DIR: &str = "right-rail-split-b-cwd";
const SPLIT_TARGET_B_KEY: &str = "local:acceptance-right-rail-split-b";
const MARKER_A: &str = "echo KERMINAL_RIGHT_RAIL_SPLIT_A";
const MARKER_B: &str = "echo KERMINAL_RIGHT_RAIL_SPLIT_B";
const ROW_A: &str = "a";
const ROW_B: &str = "b";
const SWITCH_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_TRANSITION_FRAMES: usize = 24;
const GEOMETRY_TOLERANCE: f32 = 0.6;
const FIXED_REGIONS: [(&str, &str); 5] = [
    ("navigationRail", "workbench-tool-rail"),
    ("workspaceTabs", "workspace-tabs-view"),
    ("rightRailCard", "rr-logs-card"),
    ("rightRailToolbar", "rr-logs-source"),
    ("logsList", "rr-logs-list"),
];

/// 环境隔离证据只覆盖子进程环境变量白名单；Windows KnownFolder API 不保证服从这些值，
/// 因此 known_folder_api_covered 固定为 false，避免把路径模拟说成系统级全局隔离。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EnvironmentIsolationEvidence {
    scope: &'static str,
    validated_before_composition: bool,
    home: String,
    appdata: String,
    localappdata: String,
    temp: String,
    ps_module_path: String,
    psreadline_history_expected_path: String,
    psreadline_history_scope: &'static str,
    psreadline_history_child_actual_verified: bool,
    module_probe_verified: bool,
    module_probe_scope: &'static str,
    config_root: String,
    program_data: String,
    public: String,
    known_folder_api_covered: bool,
}

/// KnownFolder API 不受进程环境变量重定向；这里只记录两个边界检查点，不声称
/// 捕获期间从未短暂激活其他工具，也不声称其他组件没有访问 KnownFolder 路径。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KnownFolderEvidence {
    redirected: bool,
    content_access_guard: &'static str,
    initial_active_tool_checked: bool,
    final_active_tool_is_logs: bool,
    transient_activation_verified: bool,
    psreadline_history_expected_path: String,
    psreadline_history_expected_under_isolated_appdata: bool,
    psreadline_history_child_actual_verified: bool,
    trusted_modules: String,
    trusted_module_commands_verified: bool,
    trusted_module_commands_scope: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RightRailSplitSwitchReport {
    schema_version: u32,
    capture_mode: &'static str,
    capture_backend: &'static str,
    input_source: &'static str,
    native_isolation: crate::right_rail_acceptance::isolation::DesktopIsolationEvidence,
    config_directory: String,
    temp_directory: String,
    debug_build: bool,
    environment_isolation: EnvironmentIsolationEvidence,
    known_folder_evidence: KnownFolderEvidence,
    local_profile_shell: String,
    scale_factor: f32,
    window_visible: bool,
    window_active: bool,
    window_width: f32,
    window_height: f32,
    row_evidence_scope: &'static str,
    system_appearance_verified: bool,
    fixed_regions: Vec<FixedRegionSpec>,
    profile_a: ProfileEvidence,
    profile_b: ProfileEvidence,
    profile_markers_distinct: bool,
    pane_sessions_distinct: bool,
    workspace_preserved: bool,
    themes: Vec<ThemeSwitchEvidence>,
    target_data_observation: TargetDataObservation,
    status_scope: &'static str,
    window_hidden_verified: bool,
    acceptance_active_override: bool,
    manual_draw_used: bool,
    normal_window_present_verified: bool,
    observation_boundary: &'static str,
    status: &'static str,
}

/// 中间态只证明目标数据在切换过程中被真实渲染，不证明透明度、正常窗口 present 或 140ms 淡入；
/// 这些未插桩项固定报告为 false，避免把“数据已经可见”夸大成“看到动画”。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TargetDataObservation {
    observed: bool,
    observed_directions: usize,
    total_directions: usize,
    claim: &'static str,
    opacity_observed: bool,
    fade_duration_verified: bool,
    window_present_verified: bool,
    measurement_boundary: &'static str,
    limitation: &'static str,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FixedRegionSpec {
    key: &'static str,
    selector: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileEvidence {
    profile_id: String,
    profile_name: String,
    split_target_key: Option<&'static str>,
    pane_id: String,
    session_id: String,
    marker_command: &'static str,
    history_entry_id: String,
    history_profile_id: Option<String>,
    cwd: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThemeSwitchEvidence {
    theme: &'static str,
    effective_theme: String,
    theme_claim: &'static str,
    system_follow_verified: bool,
    a_baseline: FrameEvidence,
    a_to_b: TransitionEvidence,
    b_to_a: TransitionEvidence,
    fixed_geometry_stable: bool,
    target_hashes_distinct: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FrameEvidence {
    requested_focus_pane_id: String,
    active_pane_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sample_elapsed_ms: Option<u64>,
    // 毫秒会被整除截断为 0；微秒字段保留真实测量并用于脚本的顺序校验。
    #[serde(skip_serializing_if = "Option::is_none")]
    sample_elapsed_micros: Option<u64>,
    a_marker_row_visible_in_current_page: bool,
    b_marker_row_visible_in_current_page: bool,
    a_marker_row_present_in_current_page: bool,
    b_marker_row_present_in_current_page: bool,
    // 统一门禁面积与日志列表的像素内容；全透明、纯黑或均匀空白帧不能只靠哈希不同冒充有效采样。
    pixel_content: Vec<PixelContentEvidence>,
    regions: Vec<RegionEvidence>,
    capture: CaptureRecord,
}

/// 逐像素统计只看采样区域的颜色数量和亮度跨度，不解释具体颜色语义；
/// 阈值只用于拒绝全透明、纯黑或均匀空白，不把“内容存在”扩展成“目标数据正确”。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PixelContentEvidence {
    key: &'static str,
    physical_bounds: PhysicalBoundsEvidence,
    sampled_pixels: u64,
    distinct_rgb_colors: u32,
    luma_min: u8,
    luma_max: u8,
    fully_transparent: bool,
    required_min_distinct_rgb_colors: u32,
    required_min_luma_range: u8,
    content_verified: bool,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct PhysicalBoundsEvidence {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

#[derive(Clone, Copy)]
struct PhysicalRect {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegionEvidence {
    key: &'static str,
    selector: &'static str,
    bounds: BoundsEvidence,
    pixel_sha256: String,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct BoundsEvidence {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TransitionEvidence {
    from_pane_id: String,
    to_pane_id: String,
    expected_row: &'static str,
    forbidden_row: &'static str,
    expected_row_visible_in_current_page: bool,
    forbidden_row_absent_from_current_page: bool,
    first_sample: FrameEvidence,
    first_sample_coverage: &'static str,
    transition_frames: Vec<FrameEvidence>,
    settled_frame: FrameEvidence,
    elapsed_ms: u64,
    elapsed_micros: u64,
    timing_origin: &'static str,
    timing_precision: &'static str,
    intermediate_state_observed: bool,
    intermediate_frame_count: usize,
    first_intermediate_elapsed_ms: Option<u64>,
    // 毫秒同样会被截断；中间帧的时间顺序以微秒为准，毫秒只保留为可读信息。
    first_intermediate_elapsed_micros: Option<u64>,
    intermediate_claim: &'static str,
    opacity_observed: bool,
    fade_duration_verified: bool,
    limitation: &'static str,
    target_hash_changed: bool,
    fixed_geometry_stable: bool,
    data_transition_passed: bool,
}

/// 一次渲染同时携带观察所需的行状态和可选落盘记录，避免 observe 与 capture
/// 各自重复绘制并让“点击后首张采样”被等待循环提前消费。
struct LogsSnapshot {
    active_pane_id: Option<String>,
    a_marker_row_visible_in_current_page: bool,
    b_marker_row_visible_in_current_page: bool,
    a_marker_row_present_in_current_page: bool,
    b_marker_row_present_in_current_page: bool,
    pixel_content: Vec<PixelContentEvidence>,
    regions: Vec<RegionEvidence>,
    capture: Option<CaptureRecord>,
}

struct LogsObservation {
    active_pane_id: Option<String>,
    expected_row_visible: bool,
    expected_row_present: bool,
    forbidden_row_visible: bool,
    forbidden_row_present: bool,
    regions: Vec<RegionEvidence>,
}

struct HistoryEvidence {
    entry_id: String,
    profile_id: Option<String>,
}

/// 一次 A/B 切换所需的输入、身份和固定区域参考集中在一个请求里，
/// 避免长参数列表让方向、行目标和来源帧在调用点错位。
struct SwitchRequest<'a> {
    output_directory: &'a Path,
    theme_name: &'a str,
    direction: &'static str,
    from_pane: &'a str,
    to_pane: &'a str,
    expected_row: &'static str,
    entry_a: &'a str,
    entry_b: &'a str,
    source_frame: &'a FrameEvidence,
    reference: &'a [RegionEvidence],
}

impl BoundsEvidence {
    /// 固定区域只保存逻辑坐标，像素哈希另存，便于脚本用同一容差核对不同缩放。
    ///
    /// @author kongweiguang
    fn from_bounds(bounds: Bounds<Pixels>) -> Self {
        Self {
            x: bounds.origin.x.as_f32(),
            y: bounds.origin.y.as_f32(),
            width: bounds.size.width.as_f32(),
            height: bounds.size.height.as_f32(),
        }
    }
}

/// 专项入口必须由私有桌面试跑器显式开启，普通工作台验收不承担额外的分屏和命令输入。
///
/// @author kongweiguang
pub(super) fn requested() -> bool {
    std::env::var_os(RIGHT_RAIL_SPLIT_SWITCH_ENV).as_deref() == Some(OsStr::new("1"))
}

/// kerminal 的 `--output-dir` 入口要求末级 run root 尚不存在，因此父进程不能预建
/// HOME/TEMP 等子目录。这里在子进程创建 run root 后补建白名单目录，确保环境校验
/// 看到的是真实可写目录，而不是只在环境变量里声明但文件系统并不存在的路径。
///
/// @author kongweiguang
pub(super) fn prepare_isolated_environment(output_directory: &Path) -> Result<(), Box<dyn Error>> {
    for relative in [
        "home/AppData/Roaming",
        "home/AppData/Roaming/Microsoft/Windows/PowerShell/PSReadLine",
        "home/AppData/Local",
        "home/AppData/Local/Microsoft/Windows/PowerShell",
        "temp",
        "program-data",
        "public",
    ] {
        fs::create_dir_all(output_directory.join(relative)).map_err(|error| {
            io::Error::other(format!(
                "right-rail isolated directory {relative} cannot be created: {error}"
            ))
        })?;
    }
    Ok(())
}

/// 子进程环境由 PowerShell 以白名单方式重建；这里在 DesktopComposition 和 PTY 启动前
/// 逐项核对 HOME、KnownFolder 模拟目录、TEMP、PowerShell 模块目录、配置根和 GIT 隔离值。
/// 只校验环境变量不能证明 Windows KnownFolder API 也服从这些值，因此不声称全局元数据隔离。
///
/// @author kongweiguang
pub(super) fn validate_isolated_environment(
    output_directory: &Path,
    config_directory: &Path,
) -> Result<EnvironmentIsolationEvidence, Box<dyn Error>> {
    if !cfg!(windows) {
        return Err(io::Error::other(
            "right-rail split acceptance environment isolation is only defined on Windows",
        )
        .into());
    }
    let home = output_directory.join("home");
    let appdata = home.join("AppData").join("Roaming");
    let localappdata = home.join("AppData").join("Local");
    let temp = output_directory.join("temp");
    let program_data = output_directory.join("program-data");
    let public = output_directory.join("public");
    // 模块目录不能指向空 run-local 目录：shell integration 依赖 pwsh 自带的
    // PSReadLine/Utility/Management。这里只接受父进程探针给出的单一可信 PSHOME\Modules。
    let trusted_modules = PathBuf::from(required_environment_string(
        "KERMINAL_RIGHT_RAIL_TRUSTED_MODULES",
    )?);
    let psreadline_history = PathBuf::from(required_environment_string(
        "KERMINAL_RIGHT_RAIL_PSREADLINE_HISTORY",
    )?);
    for (label, directory) in [
        ("home", home.as_path()),
        ("APPDATA", appdata.as_path()),
        ("LOCALAPPDATA", localappdata.as_path()),
        ("TEMP", temp.as_path()),
        ("PSModulePath", trusted_modules.as_path()),
        ("ProgramData", program_data.as_path()),
        ("PUBLIC", public.as_path()),
        ("config", config_directory),
    ] {
        let metadata = fs::metadata(directory).map_err(|error| {
            io::Error::other(format!("{label} directory is unavailable: {error}"))
        })?;
        if !metadata.is_dir() {
            return Err(io::Error::other(format!("{label} is not a directory")).into());
        }
    }
    for module in [
        "PSReadLine",
        "Microsoft.PowerShell.Utility",
        "Microsoft.PowerShell.Management",
    ] {
        if !trusted_modules.join(module).is_dir() {
            return Err(io::Error::other(format!(
                "trusted PowerShell module directory is missing {module}"
            ))
            .into());
        }
    }

    for (name, expected) in [
        ("USERPROFILE", home.as_path()),
        ("HOME", home.as_path()),
        ("APPDATA", appdata.as_path()),
        ("LOCALAPPDATA", localappdata.as_path()),
        ("TEMP", temp.as_path()),
        ("TMP", temp.as_path()),
        ("TMPDIR", temp.as_path()),
        ("KERMINAL_CONFIG_ROOT", config_directory),
        ("ProgramData", program_data.as_path()),
        ("PUBLIC", public.as_path()),
    ] {
        require_same_environment_path(name, expected)?;
    }
    // PSModulePath 必须是单一可信目录，不能追加用户模块路径或 run-local 空目录。
    let ps_module_path = required_environment_string("PSModulePath")?;
    if ps_module_path.contains(';') {
        return Err(io::Error::other(
            "right-rail PSModulePath must contain exactly one trusted pwsh module directory",
        )
        .into());
    }
    require_same_environment_path("PSModulePath", &trusted_modules)?;
    let expected_history = appdata
        .join("Microsoft")
        .join("Windows")
        .join("PowerShell")
        .join("PSReadLine")
        .join("ConsoleHost_history.txt");
    if normalize_lexical_path(&psreadline_history) != normalize_lexical_path(&expected_history) {
        return Err(
            io::Error::other("PSReadLine history path escaped the isolated APPDATA root").into(),
        );
    }
    if required_environment_string("KERMINAL_RIGHT_RAIL_MODULE_PROBE_VERIFIED")? != "1" {
        return Err(io::Error::other(
            "right-rail trusted PowerShell module probe was not verified",
        )
        .into());
    }

    let home_drive = required_environment_string("HOMEDRIVE")?;
    let home_path = required_environment_string("HOMEPATH")?;
    if !home_drive.ends_with(':') || !home_path.starts_with('\\') {
        return Err(io::Error::other("HOMEDRIVE/HOMEPATH are not in Windows drive form").into());
    }
    let combined_home = PathBuf::from(format!(
        "{}{}",
        home_drive.trim_end_matches('\\'),
        home_path
    ));
    if !same_path(&combined_home, &home) {
        return Err(
            io::Error::other("HOMEDRIVE/HOMEPATH do not resolve to the isolated HOME").into(),
        );
    }

    for (name, expected) in [
        ("USERNAME", "acceptance"),
        ("USERDOMAIN", "KERMINAL_ACCEPTANCE"),
        ("COMPUTERNAME", "KERMINAL-ACCEPTANCE"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_SYSTEM", "NUL"),
        ("GIT_ALLOW_PROTOCOL", "file"),
        ("GIT_TERMINAL_PROMPT", "0"),
    ] {
        let actual = required_environment_string(name)?;
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(io::Error::other(format!(
                "right-rail isolated environment value {name} is not {expected}"
            ))
            .into());
        }
    }

    // Profile Shell 是白名单中唯一允许位于 run root 之外的可执行文件；它必须由
    // 私有桌面试跑器给出绝对路径并真实存在，否则子进程可能在隔离环境中找不到 shell
    // 或回退到其他实现，后续 pane 行为就不再可归因于本轮验收。
    let acceptance_shell = required_environment_string("KERMINAL_WORKBENCH_ACCEPTANCE_POWERSHELL")?;
    let acceptance_shell = Path::new(&acceptance_shell);
    if !acceptance_shell.is_absolute() || !acceptance_shell.is_file() {
        return Err(io::Error::other(
            "right-rail acceptance PowerShell must be an existing absolute file",
        )
        .into());
    }

    let expected_git_config = config_directory
        .join(".acceptance-sync")
        .join("global.gitconfig");
    let actual_git_config = required_environment_string("GIT_CONFIG_GLOBAL")?;
    if normalize_lexical_path(Path::new(&actual_git_config))
        != normalize_lexical_path(&expected_git_config)
    {
        return Err(
            io::Error::other("GIT_CONFIG_GLOBAL escaped the isolated configuration root").into(),
        );
    }

    let system_root = PathBuf::from(required_environment_string("SystemRoot")?);
    let system_root = system_root.canonicalize().map_err(|error| {
        io::Error::other(format!("SystemRoot cannot be canonicalized: {error}"))
    })?;
    // WINDIR 是 Windows 程序常用的另一处系统目录别名；与 SystemRoot 不一致时，
    // 白名单虽然只放行系统 PATH，子进程仍可能从另一套目录解析组件，因此直接拒绝。
    let windows_dir = PathBuf::from(required_environment_string("WINDIR")?);
    if !same_path(&windows_dir, &system_root) {
        return Err(io::Error::other("WINDIR does not resolve to SystemRoot").into());
    }
    let path_value = required_environment_string("PATH")?;
    let mut path_entries = 0_usize;
    for entry in path_value.split(';') {
        if entry.trim().is_empty() {
            return Err(io::Error::other("isolated PATH contains an empty entry").into());
        }
        let canonical = Path::new(entry).canonicalize().map_err(|error| {
            io::Error::other(format!(
                "isolated PATH entry cannot be canonicalized: {error}"
            ))
        })?;
        if !canonical.starts_with(&system_root) {
            return Err(io::Error::other(format!(
                "isolated PATH entry escaped SystemRoot: {entry}"
            ))
            .into());
        }
        path_entries += 1;
    }
    if path_entries == 0 {
        return Err(io::Error::other("isolated PATH is empty").into());
    }
    Ok(EnvironmentIsolationEvidence {
        // 原生应用子进程是严格白名单；外层 pwsh 只重写受控值，其余继承变量未清除，
        // 因此 scope 不能写成整条链路的完全隔离。
        scope: "native-child-process-environment-whitelist; outer-pwsh-rewrites-controlled-values-but-does-not-scrub-inherited-variables; known-folder-api-not-covered",
        validated_before_composition: true,
        home: home.to_string_lossy().into_owned(),
        appdata: appdata.to_string_lossy().into_owned(),
        localappdata: localappdata.to_string_lossy().into_owned(),
        temp: temp.to_string_lossy().into_owned(),
        ps_module_path: trusted_modules.to_string_lossy().into_owned(),
        // 外层探针只证明 pwsh 版本、PSModulePath 和父进程历史路径；子 shell 的真实历史
        // 文件由环境映射推断，当前没有在子 shell 内回读，因此不能把期望路径当作实测值。
        psreadline_history_expected_path: psreadline_history.to_string_lossy().into_owned(),
        psreadline_history_scope: PSREADLINE_HISTORY_SCOPE,
        psreadline_history_child_actual_verified: false,
        module_probe_verified: true,
        module_probe_scope: ENVIRONMENT_PROBE_SCOPE,
        config_root: config_directory.to_string_lossy().into_owned(),
        program_data: program_data.to_string_lossy().into_owned(),
        public: public.to_string_lossy().into_owned(),
        known_folder_api_covered: false,
    })
}

/// 环境变量必须指向调用方给定的独占目录，而不是依赖字符串前缀或用户默认目录。
///
/// @author kongweiguang
fn require_same_environment_path(name: &str, expected: &Path) -> Result<(), Box<dyn Error>> {
    let actual = std::env::var_os(name).ok_or_else(|| {
        io::Error::other(format!("right-rail isolated environment is missing {name}"))
    })?;
    if !same_path(&PathBuf::from(actual), expected) {
        return Err(io::Error::other(format!(
            "right-rail isolated environment {name} does not point to its run-local directory"
        ))
        .into());
    }
    Ok(())
}

/// 读取必填环境变量并把空值视为失败，避免空白值被当成已隔离。
///
/// @author kongweiguang
fn required_environment_string(name: &str) -> Result<String, Box<dyn Error>> {
    let value = std::env::var_os(name).ok_or_else(|| {
        io::Error::other(format!("right-rail isolated environment is missing {name}"))
    })?;
    let value = value.to_string_lossy();
    if value.trim().is_empty() {
        return Err(
            io::Error::other(format!("right-rail isolated environment {name} is empty")).into(),
        );
    }
    Ok(value.into_owned())
}

/// 路径已存在时用 canonicalize 消除 `..` 和大小写差异；失败时按不可信处理。
///
/// @author kongweiguang
fn same_path(actual: &Path, expected: &Path) -> bool {
    match (actual.canonicalize(), expected.canonicalize()) {
        (Ok(actual), Ok(expected)) => {
            normalize_lexical_path(&actual) == normalize_lexical_path(&expected)
        }
        _ => false,
    }
}

/// 词法归一化只用于比较脚本写入的绝对路径，不替代实际文件系统隔离证明。
///
/// @author kongweiguang
fn normalize_lexical_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

/// 第二个本地 Profile 必须在默认 Profile 的 revision 列表生成前写入，
/// 否则设置默认项时会漏掉它并使分屏目标目录与持久化修订不一致。
/// 给 B 单独设置隔离配置根内的启动目录，运行时可从 pane 的 OSC7 目录回读，
/// 从而区分“确实使用了 B Profile”和静默回退到默认 Profile。
///
/// @author kongweiguang
pub(super) async fn prepare_profile(
    services: &kerminal_ui::AppServices,
    config_directory: &Path,
) -> Result<(), Box<dyn Error>> {
    if std::env::var_os(super::WORKBENCH_POWERSHELL_ENV).is_none() {
        return Err(io::Error::other(
            "right-rail split acceptance requires an explicit PowerShell path",
        )
        .into());
    }
    let working_directory = config_directory.join(PROFILE_B_CWD_DIR);
    fs::create_dir_all(&working_directory)?;
    let working_directory = working_directory.to_string_lossy().into_owned();
    let profile_service = services.profiles.clone();
    services
        .executor
        .clone()
        .run(move |_| async move {
            profile_service
                .save_profile(
                    ProfileDraft {
                        id: Some(PROFILE_B_ID.to_owned()),
                        name: PROFILE_B_NAME.to_owned(),
                        shell: acceptance_profile_shell(),
                        args: vec!["-NoLogo".to_owned(), "-NoProfile".to_owned()],
                        cwd: Some(working_directory),
                        env: Default::default(),
                        sidebar_group_id: None,
                        sort_order: 1,
                    },
                    None,
                )
                .await
        })
        .await?;
    Ok(())
}

/// 两个本地 Profile 只在隔离配置根内存在；命令历史以 pane/session 精确区分，
/// 右栏切换证据不依赖真实主机、凭据或用户当前窗口。
///
/// @author kongweiguang
pub(super) async fn capture(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &WorkbenchContext,
    first_session: WorkbenchState,
) -> Result<(), Box<dyn Error>> {
    if std::env::var_os(super::WORKBENCH_POWERSHELL_ENV).is_none() {
        return Err(io::Error::other(
            "right-rail split acceptance requires an explicit PowerShell path",
        )
        .into());
    }
    split_directory::prepare_capture_surface(cx, window, context.width, context.height).await?;
    let output_directory = if context.output_directory.is_absolute() {
        context.output_directory.clone()
    } else {
        std::env::current_dir()?.join(&context.output_directory)
    };

    let initial_panes = wait_for_panes(cx, root, window, "initial local prompt", |panes| {
        panes.len() == 1
            && Some(panes[0].0.as_str()) == first_session.active_local_pane_id.as_deref()
            && panes[0].2.is_some()
    })
    .await?;
    let pane_a = initial_panes[0].0.clone();
    let session_a = initial_panes[0].1.clone();
    let cwd_a = initial_panes[0].2.clone();

    // 初始布局必须没有打开工具，或只打开 Logs；任何 Files/SFTP 挂载都可能消费
    // KnownFolder 返回的真实用户目录，必须在首次数据采集前拒绝。
    let initial_tool = window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        window.refresh();
        draw_acceptance_frame(window, app);
        Ok::<_, io::Error>(root.read(app).right_rail_acceptance_active_tool())
    })??;
    if !matches!(initial_tool, None | Some(ToolRailToolId::Logs)) {
        return Err(io::Error::other(format!(
            "right-rail KnownFolder guard failed before capture: unexpected active tool {initial_tool:?}"
        ))
        .into());
    }

    activate_logs(cx, root, window).await?;
    focus_terminal_pane(cx, root, window, &pane_a, "source pane A focus").await?;
    type_terminal_command(cx, window, MARKER_A)?;
    let history_a = wait_for_history_entry(
        cx,
        services,
        &pane_a,
        &session_a,
        MARKER_A,
        "pane A history",
    )
    .await?;

    split_to_profile_b(cx, window, &pane_a).await?;
    let split_panes = wait_for_panes(cx, root, window, "profile B split readiness", |panes| {
        panes.len() == 2
            && panes
                .iter()
                .any(|pane| pane.0 == pane_a && pane.1 == session_a)
            && panes
                .iter()
                .any(|pane| pane.0 != pane_a && pane.1 != session_a && pane.2.is_some())
    })
    .await?;
    let split_pane = split_panes
        .into_iter()
        .find(|pane| pane.0 != pane_a)
        .ok_or_else(|| io::Error::other("right-rail split pane was not observed"))?;
    let pane_b = split_pane.0;
    let session_b = split_pane.1;
    let cwd_b = split_pane.2;
    let expected_b_cwd = context.config_directory.join(PROFILE_B_CWD_DIR);
    if !directory_matches(cwd_b.as_deref(), &expected_b_cwd) {
        return Err(io::Error::other(format!(
            "right-rail split profile B did not start in its configured directory: observed={cwd_b:?}, expected={}",
            expected_b_cwd.display()
        ))
        .into());
    }

    focus_terminal_pane(cx, root, window, &pane_b, "profile B pane focus").await?;
    type_terminal_command(cx, window, MARKER_B)?;
    let history_b = wait_for_history_entry(
        cx,
        services,
        &pane_b,
        &session_b,
        MARKER_B,
        "pane B history",
    )
    .await?;

    let mut themes = Vec::new();
    for (theme, name, selector) in THEMES {
        let effective_theme =
            split_directory::change_theme(cx, root, window, services, theme, name, selector)
                .await?;
        activate_logs(cx, root, window).await?;
        focus_terminal_pane(cx, root, window, &pane_a, "theme baseline pane A focus").await?;
        wait_for_logs_data(
            cx,
            root,
            window,
            &pane_a,
            &history_a.entry_id,
            &history_b.entry_id,
            "theme baseline A history",
        )
        .await?;
        let a_baseline = capture_logs_frame(
            cx,
            root,
            window,
            output_directory.join(format!("right-rail-{name}-a-baseline.png")),
            &pane_a,
            true,
            Instant::now(),
            &history_a.entry_id,
            &history_b.entry_id,
        )?;
        validate_frame_geometry(&a_baseline.regions)?;

        let a_to_b = capture_switch(
            cx,
            root,
            window,
            SwitchRequest {
                output_directory: &output_directory,
                theme_name: name,
                direction: "a-to-b",
                from_pane: &pane_a,
                to_pane: &pane_b,
                expected_row: ROW_B,
                entry_a: &history_a.entry_id,
                entry_b: &history_b.entry_id,
                source_frame: &a_baseline,
                reference: &a_baseline.regions,
            },
        )
        .await?;
        let b_to_a = capture_switch(
            cx,
            root,
            window,
            SwitchRequest {
                output_directory: &output_directory,
                theme_name: name,
                direction: "b-to-a",
                from_pane: &pane_b,
                to_pane: &pane_a,
                expected_row: ROW_A,
                entry_a: &history_a.entry_id,
                entry_b: &history_b.entry_id,
                source_frame: &a_to_b.settled_frame,
                reference: &a_baseline.regions,
            },
        )
        .await?;

        let target_hashes_distinct = a_to_b.target_hash_changed && b_to_a.target_hash_changed;
        if !target_hashes_distinct {
            return Err(io::Error::other(format!(
                "right-rail split theme {name} did not expose distinct target data in both directions"
            ))
            .into());
        }
        themes.push(ThemeSwitchEvidence {
            theme: name,
            effective_theme,
            // system 只证明设置模式已保存并实际渲染出浅/深色盘，不读取 OS 外观，
            // 因此不能把 system 命名当成“跟随系统已通过”。
            theme_claim: if name == "system" {
                "mode-only"
            } else {
                "palette-verified"
            },
            system_follow_verified: false,
            a_baseline,
            a_to_b,
            b_to_a,
            fixed_geometry_stable: true,
            target_hashes_distinct,
        });
    }

    let final_panes = wait_for_panes(cx, root, window, "final split workspace", |panes| {
        panes.len() == 2
            && panes
                .iter()
                .any(|pane| pane.0 == pane_a && pane.1 == session_a)
            && panes
                .iter()
                .any(|pane| pane.0 == pane_b && pane.1 == session_b)
            && panes.iter().all(|pane| pane.2.is_some())
    })
    .await?;
    let final_active =
        wait_for_active_pane(cx, root, window, &pane_a, "final pane A focus").await?;
    if final_active != pane_a {
        return Err(io::Error::other("right-rail split did not return focus to pane A").into());
    }
    let (failed_pane_ids, ready_session_ids) = window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        let (_, _, ready_session_ids, failed_pane_ids) =
            root.read(app).workbench_acceptance_local_state();
        Ok::<_, io::Error>((failed_pane_ids, ready_session_ids))
    })??;
    let workspace_preserved = failed_pane_ids.is_empty()
        && ready_session_ids.len() == 2
        && final_panes.iter().all(|pane| pane.2.is_some());

    let (window_visible, window_active, window_width, window_height, scale_factor) = window
        .update(cx, |_, window, _| {
            ensure_hidden_window(window)?;
            Ok::<_, io::Error>((
                window.is_visible(),
                window.is_window_active(),
                window.bounds().size.width.as_f32(),
                window.bounds().size.height.as_f32(),
                window.scale_factor(),
            ))
        })??;
    let native_isolation = crate::right_rail_acceptance::isolation::capture()?;
    native_isolation.validate_window_state(window_visible, window_active)?;
    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return Err(io::Error::other("right-rail split scale factor is invalid").into());
    }

    let profile_a = ProfileEvidence {
        profile_id: super::WORKBENCH_PROFILE_ID.to_owned(),
        profile_name: super::WORKBENCH_PROFILE_NAME.to_owned(),
        split_target_key: None,
        pane_id: pane_a.clone(),
        session_id: session_a.clone(),
        marker_command: MARKER_A,
        history_entry_id: history_a.entry_id,
        history_profile_id: history_a.profile_id,
        cwd: cwd_a,
    };
    let profile_b = ProfileEvidence {
        profile_id: PROFILE_B_ID.to_owned(),
        profile_name: PROFILE_B_NAME.to_owned(),
        split_target_key: Some(SPLIT_TARGET_B_KEY),
        pane_id: pane_b.clone(),
        session_id: session_b.clone(),
        marker_command: MARKER_B,
        history_entry_id: history_b.entry_id,
        history_profile_id: history_b.profile_id,
        cwd: cwd_b,
    };
    let profile_markers_distinct = profile_a.marker_command != profile_b.marker_command;
    let pane_sessions_distinct = profile_a.pane_id != profile_b.pane_id
        && profile_a.session_id != profile_b.session_id
        && profile_a.history_entry_id != profile_b.history_entry_id;
    if !workspace_preserved || !profile_markers_distinct || !pane_sessions_distinct {
        return Err(io::Error::other(
            "right-rail split workspace or profile evidence did not remain distinct",
        )
        .into());
    }

    let total_directions = themes.len() * 2;
    let observed_directions = themes
        .iter()
        .map(|theme| {
            usize::from(theme.a_to_b.intermediate_state_observed)
                + usize::from(theme.b_to_a.intermediate_state_observed)
        })
        .sum::<usize>();
    let target_data_observed = observed_directions == total_directions;
    let target_data_observation = TargetDataObservation {
        observed: target_data_observed,
        observed_directions,
        total_directions,
        claim: if target_data_observed {
            INTERMEDIATE_CLAIM
        } else {
            "not-observed"
        },
        opacity_observed: false,
        fade_duration_verified: false,
        window_present_verified: false,
        measurement_boundary: TARGET_DATA_BOUNDARY,
        limitation: if target_data_observed {
            FADE_LIMITATION
        } else {
            NO_INTERMEDIATE_LIMITATION
        },
    };
    let status = if target_data_observed {
        "passed"
    } else {
        "insufficient-target-data-intermediate-evidence"
    };

    // KnownFolder API 不会被 HOME/APPDATA 重定向；这里只在初始与最终两个检查点
    // 拒绝 Logs 之外的工具成为活动工具，不能证明期间没有短暂激活，也不是文件沙箱。
    let final_active_tool_is_logs = window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        window.refresh();
        draw_acceptance_frame(window, app);
        Ok::<_, io::Error>(
            root.read(app).right_rail_acceptance_active_tool() == Some(ToolRailToolId::Logs),
        )
    })??;
    if !final_active_tool_is_logs {
        return Err(io::Error::other(
            "right-rail KnownFolder checkpoint failed: Logs is not the final active tool",
        )
        .into());
    }

    let environment_isolation = context
        .right_rail_environment
        .as_ref()
        .ok_or_else(|| {
            io::Error::other(
                "right-rail environment isolation was not validated before composition",
            )
        })?
        .clone();
    let known_folder_evidence = KnownFolderEvidence {
        redirected: false,
        content_access_guard: KNOWN_FOLDER_GUARD,
        // 能走到报告构造点说明 capture 开头的初始检查点已通过；这里只记录两个
        // 检查点事实，不把期间从未短暂激活写成已验证结论。
        initial_active_tool_checked: true,
        final_active_tool_is_logs,
        transient_activation_verified: false,
        psreadline_history_expected_under_isolated_appdata: Path::new(
            &environment_isolation.psreadline_history_expected_path,
        )
        .starts_with(Path::new(&environment_isolation.appdata)),
        psreadline_history_expected_path: environment_isolation
            .psreadline_history_expected_path
            .clone(),
        psreadline_history_child_actual_verified: false,
        trusted_modules: environment_isolation.ps_module_path.clone(),
        trusted_module_commands_verified: environment_isolation.module_probe_verified,
        trusted_module_commands_scope: TRUSTED_MODULE_COMMANDS_SCOPE,
    };
    let report = RightRailSplitSwitchReport {
        schema_version: 1,
        capture_mode: "hidden-native-window",
        capture_backend: "gpui-root-pane-focus-split-and-logs-readback",
        input_source: "gpui-key-and-mouse-events-on-private-desktop",
        native_isolation,
        config_directory: context.config_directory.to_string_lossy().into_owned(),
        temp_directory: std::env::temp_dir().to_string_lossy().into_owned(),
        debug_build: cfg!(debug_assertions),
        environment_isolation,
        known_folder_evidence,
        local_profile_shell: acceptance_profile_shell(),
        scale_factor,
        window_visible,
        window_active,
        window_width,
        window_height,
        row_evidence_scope: ROW_EVIDENCE_SCOPE,
        system_appearance_verified: false,
        fixed_regions: FIXED_REGIONS
            .into_iter()
            .map(|(key, selector)| FixedRegionSpec { key, selector })
            .collect(),
        profile_a,
        profile_b,
        profile_markers_distinct,
        pane_sessions_distinct,
        workspace_preserved,
        themes,
        target_data_observation,
        status_scope: STATUS_SCOPE,
        window_hidden_verified: !window_visible,
        acceptance_active_override: true,
        manual_draw_used: true,
        normal_window_present_verified: false,
        observation_boundary: OBSERVATION_BOUNDARY,
        status,
    };
    fs::write(
        output_directory.join(REPORT_FILE),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}

/// 隐藏窗口中的点击必须先绘制再解析真实控件边界，右击沿用同一路径以打开生产分屏目标选择器。
///
/// @author kongweiguang
fn right_click_selector(
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
                button: MouseButton::Right,
                position,
                click_count: 1,
                ..MouseDownEvent::default()
            }
            .to_platform_input(),
            MouseUpEvent {
                button: MouseButton::Right,
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

/// 生产分屏按钮右键只选择目标，不直接复用默认 Profile；显式点击 B 的稳定 option key，
/// 才能让后续新 pane、session 与 A 的独立历史形成可核对的分屏目标。
///
/// @author kongweiguang
async fn split_to_profile_b(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    source_pane: &str,
) -> Result<(), Box<dyn Error>> {
    right_click_selector(
        cx,
        window,
        &format!("pane-split-horizontal-after-{source_pane}-trigger"),
    )?;
    let option = format!("split-target-picker-options-option-{SPLIT_TARGET_B_KEY}");
    wait_for_selector(cx, window, &option, "split target profile B option").await?;
    click_selector(cx, window, &option)?;
    Ok(())
}

/// 终端 Canvas 的调试选择器来自真实生产布局，点击中心既激活 pane 也把键盘焦点交给该终端，
/// 不依赖标题栏文本或未导出的 pane 几何。
///
/// @author kongweiguang
async fn focus_terminal_pane(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    pane_id: &str,
    stage: &str,
) -> Result<(), Box<dyn Error>> {
    click_selector(cx, window, &format!("workspace-terminal-canvas-{pane_id}"))?;
    let active = wait_for_active_pane(cx, root, window, pane_id, stage).await?;
    if active != pane_id {
        return Err(io::Error::other(format!(
            "right-rail split focus mismatch during {stage}: expected={pane_id}, observed={active}"
        ))
        .into());
    }
    Ok(())
}

/// 只有活动 pane 仍是目标时才允许进入下一步，避免右侧日志已经跟随到另一个分屏。
///
/// @author kongweiguang
async fn wait_for_active_pane(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    pane_id: &str,
    stage: &str,
) -> Result<String, Box<dyn Error>> {
    let deadline = Instant::now() + WORKBENCH_ACTION_TIMEOUT;
    loop {
        let active = window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            window.refresh();
            draw_acceptance_frame(window, app);
            Ok::<_, io::Error>(root.read(app).workbench_acceptance_active_local_pane_id())
        })??;
        if active.as_deref() == Some(pane_id) {
            return Ok(active.unwrap_or_default());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "right-rail split timed out during {stage}: expected_pane={pane_id}, active={active:?}"
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 右栏必须由生产工具入口打开并保持活动；激活前开启验收活动门禁，
/// 隐藏窗口中的日志读取才能像可见窗口一样推进。
///
/// @author kongweiguang
async fn activate_logs(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        root.update(app, |root, cx| {
            root.enable_right_rail_acceptance_activity(cx);
            root.activate_right_rail_tool_for_acceptance(ToolRailToolId::Logs, window, cx);
        });
        window.refresh();
        draw_acceptance_frame(window, app);
        Ok::<_, io::Error>(())
    })??;
    let deadline = Instant::now() + WORKBENCH_ACTION_TIMEOUT;
    loop {
        let active = window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            window.refresh();
            draw_acceptance_frame(window, app);
            Ok::<_, io::Error>(root.read(app).right_rail_acceptance_active_tool())
        })??;
        if active == Some(ToolRailToolId::Logs) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "right-rail logs panel did not become active: {active:?}"
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 命令经真实键盘事件进入当前 pane 的终端输入处理器；字符被拒绝时立即失败，
/// 不把未送达的按键当成历史写入成功。
///
/// @author kongweiguang
fn type_terminal_command(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    command: &str,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        window.refresh();
        draw_acceptance_frame(window, app);
        for character in command.chars() {
            let key = character.to_string();
            let keystroke = Keystroke {
                modifiers: Default::default(),
                key: key.clone(),
                key_char: Some(key),
            };
            if !window.dispatch_keystroke(keystroke, app) {
                return Err(io::Error::other(format!(
                    "terminal rejected right-rail marker character {character:?}"
                )));
            }
        }
        draw_acceptance_frame(window, app);
        let enter = Keystroke::parse("enter")
            .map_err(|error| io::Error::other(format!("invalid enter keystroke: {error}")))?;
        if !window.dispatch_keystroke(enter, app) {
            return Err(io::Error::other(
                "terminal rejected the right-rail marker Enter key",
            ));
        }
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// 历史仓储的 profile_id 当前不由 UI 提交填充，因此以 pane、session 和精确命令正文
/// 三重匹配作为条目身份，避免把相近文本或其它 pane 的旧记录当成本轮证据。
///
/// @author kongweiguang
async fn wait_for_history_entry(
    cx: &mut AsyncApp,
    services: &kerminal_ui::AppServices,
    pane_id: &str,
    session_id: &str,
    command: &'static str,
    stage: &str,
) -> Result<HistoryEvidence, Box<dyn Error>> {
    let pane = PaneId::parse(pane_id)?;
    let session = SessionId::parse(session_id)?;
    let deadline = Instant::now() + WORKBENCH_ACTION_TIMEOUT;
    loop {
        let commands = services
            .commands
            .clone()
            .ok_or_else(|| io::Error::other("command history service is unavailable"))?;
        let request = CommandHistoryListRequest {
            query: Some(command.to_owned()),
            source: Some(CommandHistorySource::User),
            target: Some(CommandHistoryTarget::Local),
            pane_id: Some(pane.clone()),
            session_id: Some(session.clone()),
            limit: Some(50),
            ..Default::default()
        };
        let entries = services
            .executor
            .clone()
            .run(move |_| async move { commands.list_history(request).await })
            .await?;
        let matching = entries
            .iter()
            .filter(|entry| {
                entry.command == command
                    && entry.pane_id.as_ref() == Some(&pane)
                    && entry.session_id.as_ref() == Some(&session)
            })
            .collect::<Vec<_>>();
        if matching.len() == 1 {
            return Ok(HistoryEvidence {
                entry_id: matching[0].id.clone(),
                profile_id: matching[0].profile_id.clone(),
            });
        }
        if matching.len() > 1 {
            return Err(io::Error::other(format!(
                "right-rail split found ambiguous history entries during {stage}"
            ))
            .into());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "right-rail split timed out waiting for history during {stage}: pane={pane_id}, session={session_id}, command={command:?}"
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 分屏就绪必须同时具备两个 Ready local pane 和各自的 OSC7 目录，
/// 否则命令可能送入尚未启动的 PowerShell，历史证据会被伪失败掩盖。
///
/// @author kongweiguang
async fn wait_for_panes<F>(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    stage: &str,
    mut condition: F,
) -> Result<Vec<(String, String, Option<String>)>, Box<dyn Error>>
where
    F: FnMut(&[(String, String, Option<String>)]) -> bool,
{
    let deadline = Instant::now() + WORKBENCH_ACTION_TIMEOUT;
    loop {
        let panes = window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            window.refresh();
            draw_acceptance_frame(window, app);
            Ok::<_, io::Error>(root.read(app).workbench_acceptance_local_panes())
        })??;
        if condition(&panes) {
            return Ok(panes);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "right-rail split timed out during {stage}: panes={panes:?}"
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 基线帧必须先看到目标行、看不到另一 pane 的行，并连续两次保持相同几何与数据哈希；
/// 这样后续帧的差异来自真实的 pane 切换，而不是主题或面板入场动画尚未完成。
///
/// @author kongweiguang
async fn wait_for_logs_data(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    pane_id: &str,
    expected_entry: &str,
    forbidden_entry: &str,
    stage: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + WORKBENCH_ACTION_TIMEOUT;
    let mut previous: Option<LogsObservation> = None;
    loop {
        let observation = observe_logs(cx, root, window, expected_entry, forbidden_entry)?;
        let stable = previous.as_ref().is_some_and(|previous| {
            previous.active_pane_id.as_deref() == Some(pane_id)
                && previous.expected_row_visible
                && previous.expected_row_present
                && !previous.forbidden_row_visible
                && !previous.forbidden_row_present
                && observation.active_pane_id.as_deref() == Some(pane_id)
                && observation.expected_row_visible
                && observation.expected_row_present
                && !observation.forbidden_row_visible
                && !observation.forbidden_row_present
                && geometry_matches(&previous.regions, &observation.regions)
                && region_sha256(&previous.regions, "navigationRail")
                    .ok()
                    .zip(region_sha256(&observation.regions, "navigationRail").ok())
                    .is_some_and(|(previous, current)| previous == current)
                && region_sha256(&previous.regions, "logsList")
                    .ok()
                    .zip(region_sha256(&observation.regions, "logsList").ok())
                    .is_some_and(|(previous, current)| previous == current)
        });
        if stable {
            return Ok(());
        }
        previous = Some(observation);
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "right-rail split timed out waiting for stable logs data during {stage}"
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 观察与截图共用同一绘制和视口校验路径；可选保存让首张点击后采样和稳定轮询
/// 使用完全相同的像素来源，避免两套绘制顺序把“首张采样”推迟到等待循环之后。
///
/// @author kongweiguang
fn render_logs_snapshot(
    window: &mut gpui_pre::Window,
    app: &mut gpui_pre::App,
    root: &Entity<RootView>,
    entry_a: &str,
    entry_b: &str,
    save_path: Option<PathBuf>,
    single_draw: bool,
) -> Result<LogsSnapshot, io::Error> {
    ensure_hidden_window(window)?;
    window.refresh();
    if single_draw {
        // 首张点击后采样只允许一次 draw/clear；共享路径的 simulate_next_frame 可能再绘制一帧，
        // 那会把采样推后，因此这里单独收窄，避免把第二次 draw 冒充同一点击派发的首张采样。
        let frame = window.draw(app);
        frame.clear(app);
    } else {
        draw_acceptance_frame(window, app);
    }
    let image = window.render_to_image().map_err(io::Error::other)?;
    let width = image.width();
    let height = image.height();
    let viewport = window.viewport_size();
    let scale = window.scale_factor();
    let expected_width = viewport.width.as_f32() * scale;
    let expected_height = viewport.height.as_f32() * scale;
    if !scale.is_finite()
        || scale <= 0.0
        || !expected_width.is_finite()
        || !expected_height.is_finite()
        || expected_width < 1.0
        || expected_height < 1.0
        || width != expected_width.round() as u32
        || height != expected_height.round() as u32
    {
        return Err(io::Error::other(format!(
            "right-rail screenshot does not match viewport: expected={expected_width:.3}x{expected_height:.3}, observed={width}x{height}, scale={scale}"
        )));
    }
    if !image
        .as_raw()
        .as_chunks::<4>()
        .0
        .iter()
        .any(|pixel| pixel[3] != 0)
    {
        return Err(io::Error::other(
            "right-rail renderer returned a fully transparent image",
        ));
    }
    let list_bounds = selector_bounds(window, "rr-logs-list")?;
    let regions = collect_regions(image.as_raw(), width, height, window, scale)?;
    // 全透明帧已经在上方拒绝；这里再对整帧和日志列表做颜色/亮度门禁，
    // 防止纯黑或均匀空白渲染被当成“目标数据已经出现”的有效采样。
    let pixel_content = vec![
        pixel_content_evidence(
            image.as_raw(),
            width,
            height,
            PhysicalRect {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            },
            "frame",
            PIXEL_MIN_DISTINCT_RGB,
            PIXEL_MIN_LUMA_RANGE,
        )?,
        pixel_content_evidence(
            image.as_raw(),
            width,
            height,
            physical_rect(width, height, list_bounds, scale, "rr-logs-list")?,
            "logsList",
            LOGS_LIST_MIN_DISTINCT_RGB,
            LOGS_LIST_MIN_LUMA_RANGE,
        )?,
    ];
    let capture = if let Some(path) = save_path {
        let file = path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid screenshot filename")
            })?
            .to_owned();
        let pixel_sha256 = format!("{:x}", Sha256::digest(image.as_raw()));
        image.save(&path).map_err(io::Error::other)?;
        Some(CaptureRecord {
            file,
            width,
            height,
            pixel_sha256,
        })
    } else {
        None
    };
    Ok(LogsSnapshot {
        active_pane_id: root.read(app).workbench_acceptance_active_local_pane_id(),
        a_marker_row_visible_in_current_page: row_visible(window, entry_a, list_bounds),
        b_marker_row_visible_in_current_page: row_visible(window, entry_b, list_bounds),
        a_marker_row_present_in_current_page: row_present(window, entry_a),
        b_marker_row_present_in_current_page: row_present(window, entry_b),
        pixel_content,
        regions,
        capture,
    })
}

/// 只观察不截图的轮询仍走真实绘制和固定区域哈希，用极小额外产物判断动画是否落位。
///
/// @author kongweiguang
fn observe_logs(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    expected_entry: &str,
    forbidden_entry: &str,
) -> Result<LogsObservation, Box<dyn Error>> {
    let snapshot = window.update(cx, |_, window, app| {
        render_logs_snapshot(
            window,
            app,
            root,
            expected_entry,
            forbidden_entry,
            None,
            false,
        )
    })??;
    require_pixel_content(&snapshot.pixel_content)?;
    Ok(LogsObservation {
        active_pane_id: snapshot.active_pane_id,
        expected_row_visible: snapshot.a_marker_row_visible_in_current_page,
        expected_row_present: snapshot.a_marker_row_present_in_current_page,
        forbidden_row_visible: snapshot.b_marker_row_visible_in_current_page,
        forbidden_row_present: snapshot.b_marker_row_present_in_current_page,
        regions: snapshot.regions,
    })
}

/// 一次截图同时保存整帧像素摘要、五个固定区域的逻辑边界与区域摘要；
/// 两个 marker 行的存在性和视口可见性分别记录，后续可区分数据混入与单纯滚动裁切。
/// 首张采样允许活动 pane 仍是来源或在同一事件内已切到目标，稳定帧才强制目标焦点。
///
/// @author kongweiguang
fn capture_logs_frame(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    path: PathBuf,
    requested_focus_pane_id: &str,
    require_focus: bool,
    started: Instant,
    entry_a: &str,
    entry_b: &str,
) -> Result<FrameEvidence, Box<dyn Error>> {
    let snapshot = window.update(cx, |_, window, app| {
        render_logs_snapshot(window, app, root, entry_a, entry_b, Some(path), false)
    })??;
    let capture = snapshot
        .capture
        .ok_or_else(|| io::Error::other("right-rail capture did not produce a frame record"))?;
    require_pixel_content(&snapshot.pixel_content)?;
    let active_pane_id = snapshot.active_pane_id.unwrap_or_default();
    if require_focus && active_pane_id != requested_focus_pane_id {
        return Err(io::Error::other(format!(
            "right-rail frame focus mismatch: expected={requested_focus_pane_id}, active={active_pane_id}"
        ))
        .into());
    }
    // 计时在截图、哈希、像素门禁和焦点检查全部完成后取一次，毫秒与微秒来自同一个
    // Duration，避免两次 elapsed 让两个字段描述不同区间。
    let sample_elapsed = started.elapsed();
    Ok(FrameEvidence {
        requested_focus_pane_id: requested_focus_pane_id.to_owned(),
        active_pane_id,
        sample_elapsed_ms: Some(sample_elapsed.as_millis().min(u64::MAX as u128) as u64),
        sample_elapsed_micros: Some(sample_elapsed.as_micros().min(u64::MAX as u128) as u64),
        a_marker_row_visible_in_current_page: snapshot.a_marker_row_visible_in_current_page,
        b_marker_row_visible_in_current_page: snapshot.b_marker_row_visible_in_current_page,
        a_marker_row_present_in_current_page: snapshot.a_marker_row_present_in_current_page,
        b_marker_row_present_in_current_page: snapshot.b_marker_row_present_in_current_page,
        pixel_content: snapshot.pixel_content,
        regions: snapshot.regions,
        capture,
    })
}

/// 切换证据把“点击派发后同一次 update 内的首张采样”和“聚焦生效后的过渡采样”分开。
/// 首张采样只绘制一次，但不是 renderer 内部首个 present，也不是可证明的 first GPUI draw；
/// 报告显式标记该覆盖边界。过渡帧还必须同时证明活动目标 pane、新目标行可见/存在且旧行不存在，
/// 才允许声明观察到目标数据中间态；透明度、运行态和 140ms 淡入仍明确未证明。
///
/// @author kongweiguang
async fn capture_switch(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    request: SwitchRequest<'_>,
) -> Result<TransitionEvidence, Box<dyn Error>> {
    let SwitchRequest {
        output_directory,
        theme_name,
        direction,
        from_pane,
        to_pane,
        expected_row,
        entry_a,
        entry_b,
        source_frame,
        reference,
    } = request;
    let first_path = output_directory.join(format!(
        "right-rail-{theme_name}-{direction}-first-sample.png"
    ));
    // 计时起点移入首样本函数，紧贴 click 派发；调用前的路径拼接和文件名字符串
    // 构造不属于点击后测量区间。
    let (first_sample, started) =
        click_and_capture_first_sample(cx, root, window, first_path, to_pane, entry_a, entry_b)?;
    if !geometry_matches(reference, &first_sample.regions) {
        return Err(io::Error::other(format!(
            "right-rail split {theme_name} {direction} first sample changed fixed geometry"
        ))
        .into());
    }
    let active = wait_for_active_pane(
        cx,
        root,
        window,
        to_pane,
        &format!("right-rail switch {theme_name} {direction} focus"),
    )
    .await?;
    if active != to_pane {
        return Err(io::Error::other(format!(
            "right-rail split {theme_name} {direction} did not focus the target pane: expected={to_pane}, observed={active}"
        ))
        .into());
    }

    let mut transition_frames = Vec::new();
    let mut previous_list_hash: Option<String> = None;
    let mut index = 0_usize;
    loop {
        if index >= MAX_TRANSITION_FRAMES || started.elapsed() >= SWITCH_TIMEOUT {
            return Err(io::Error::other(format!(
                "right-rail split {theme_name} {direction} did not settle: samples={index}, elapsed_ms={}",
                started.elapsed().as_millis()
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
        let path = output_directory.join(format!(
            "right-rail-{theme_name}-{direction}-transition-{index:02}.png"
        ));
        let frame = capture_logs_frame(
            cx, root, window, path, to_pane, true, started, entry_a, entry_b,
        )?;
        if !geometry_matches(reference, &frame.regions) {
            return Err(io::Error::other(format!(
                "right-rail split {theme_name} {direction} changed fixed geometry during transition"
            ))
            .into());
        }
        let expected_visible = frame_row_visible(&frame, expected_row);
        let expected_present = frame_row_present(&frame, expected_row);
        let forbidden_visible = frame_row_visible(&frame, forbidden_row(expected_row));
        let forbidden_present = frame_row_present(&frame, forbidden_row(expected_row));
        transition_frames.push(frame.clone());
        if expected_visible && expected_present && !forbidden_visible && !forbidden_present {
            let list_hash = region_sha256(&frame.regions, "logsList")?.to_owned();
            if previous_list_hash.as_deref() == Some(list_hash.as_str()) {
                transition_frames.pop();
                let settled_path = output_directory
                    .join(format!("right-rail-{theme_name}-{direction}-settled.png"));
                let settled_file = settled_path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "invalid settled screenshot filename",
                        )
                    })?
                    .to_owned();
                fs::copy(output_directory.join(&frame.capture.file), &settled_path)?;
                let total_elapsed = started.elapsed();
                let total_elapsed_ms = total_elapsed.as_millis().min(u64::MAX as u128) as u64;
                let total_elapsed_micros = total_elapsed.as_micros().min(u64::MAX as u128) as u64;
                let mut settled_frame = frame;
                settled_frame.capture.file = settled_file;
                settled_frame.sample_elapsed_ms = Some(total_elapsed_ms);
                settled_frame.sample_elapsed_micros = Some(total_elapsed_micros);
                let source_hash = region_sha256(&source_frame.regions, "logsList")?;
                let settled_hash = region_sha256(&settled_frame.regions, "logsList")?;
                if source_hash == settled_hash {
                    return Err(io::Error::other(format!(
                        "right-rail split {theme_name} {direction} did not change logs target data"
                    ))
                    .into());
                }
                let first_hash = region_sha256(&first_sample.regions, "logsList")?;
                let mut intermediate_frame_count = 0_usize;
                let mut first_intermediate_elapsed_ms = None;
                let mut first_intermediate_elapsed_micros = None;
                for sample in &transition_frames {
                    let sample_hash = region_sha256(&sample.regions, "logsList")?;
                    // 空白加载态或布局变化也可能改变图像；只有活动目标 pane 已生效，
                    // 且新目标行可见/存在、旧目标行不存在时，才把它算作数据中间态。
                    if sample.active_pane_id == to_pane
                        && frame_row_visible(sample, expected_row)
                        && frame_row_present(sample, expected_row)
                        && !frame_row_visible(sample, forbidden_row(expected_row))
                        && !frame_row_present(sample, forbidden_row(expected_row))
                        && sample_hash != first_hash
                        && sample_hash != settled_hash
                    {
                        intermediate_frame_count += 1;
                        first_intermediate_elapsed_ms =
                            first_intermediate_elapsed_ms.or(sample.sample_elapsed_ms);
                        first_intermediate_elapsed_micros =
                            first_intermediate_elapsed_micros.or(sample.sample_elapsed_micros);
                    }
                }
                let intermediate_state_observed = intermediate_frame_count > 0;
                if transition_frames.is_empty() {
                    return Err(io::Error::other(format!(
                        "right-rail split {theme_name} {direction} has no transition frame"
                    ))
                    .into());
                }
                let intermediate_claim = if intermediate_state_observed {
                    INTERMEDIATE_CLAIM
                } else {
                    "not-observed"
                };
                return Ok(TransitionEvidence {
                    from_pane_id: from_pane.to_owned(),
                    to_pane_id: to_pane.to_owned(),
                    expected_row,
                    forbidden_row: forbidden_row(expected_row),
                    expected_row_visible_in_current_page: true,
                    forbidden_row_absent_from_current_page: true,
                    first_sample,
                    first_sample_coverage: FIRST_SAMPLE_COVERAGE,
                    transition_frames,
                    settled_frame,
                    elapsed_ms: total_elapsed_ms,
                    elapsed_micros: total_elapsed_micros,
                    timing_origin: TIMING_ORIGIN,
                    timing_precision: TIMING_PRECISION,
                    intermediate_state_observed,
                    intermediate_frame_count,
                    first_intermediate_elapsed_ms,
                    first_intermediate_elapsed_micros,
                    intermediate_claim,
                    opacity_observed: false,
                    fade_duration_verified: false,
                    limitation: if intermediate_state_observed {
                        FADE_LIMITATION
                    } else {
                        NO_INTERMEDIATE_LIMITATION
                    },
                    target_hash_changed: source_hash != settled_hash,
                    fixed_geometry_stable: true,
                    data_transition_passed: true,
                });
            }
            previous_list_hash = Some(list_hash);
        } else {
            previous_list_hash = None;
        }
        index += 1;
    }
}

/// 点击目标终端后不进入任何等待循环，在同一次 update 内、点击派发后只绘制一次并采样。
/// 活动 pane 可能仍是来源或已切换到目标；requestedFocusPaneId 只是请求记录，
/// 真实焦点只由 activePaneId 和随后的 wait_for_active_pane 证明。
///
/// @author kongweiguang
fn click_and_capture_first_sample(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    path: PathBuf,
    to_pane: &str,
    entry_a: &str,
    entry_b: &str,
) -> Result<(FrameEvidence, Instant), Box<dyn Error>> {
    let selector = format!("workspace-terminal-canvas-{to_pane}");
    let frame = window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        window.refresh();
        // 点击前先绘制一次，确保 selector 边界来自真实布局；这次绘制发生在派发前，
        // 不计入点击后的首张采样。
        draw_acceptance_frame(window, app);
        let bounds = selector_bounds(window, &selector)?;
        if !bounds_inside_viewport(bounds, window.visual_viewport_bounds()) {
            return Err(io::Error::other(format!(
                "selector {selector:?} is outside the visible viewport"
            )));
        }
        // 起点紧贴派发：selector 查找、视口校验和点击前绘制都属于准备阶段，
        // 不能算进“点击后首张采样”的耗时。
        let started = Instant::now();
        click_bounds(window, app, bounds, &selector)?;
        let snapshot = render_logs_snapshot(window, app, root, entry_a, entry_b, Some(path), true)?;
        require_pixel_content(&snapshot.pixel_content)?;
        // 绘制、截图、区域哈希、PNG 保存和行检查全部完成后只取一次 Duration，
        // 毫秒与微秒字段因此描述同一区间。
        let sample_elapsed = started.elapsed();
        let capture = snapshot.capture.ok_or_else(|| {
            io::Error::other("right-rail first sample did not produce a frame record")
        })?;
        Ok::<_, io::Error>((
            FrameEvidence {
                requested_focus_pane_id: to_pane.to_owned(),
                active_pane_id: snapshot.active_pane_id.unwrap_or_default(),
                sample_elapsed_ms: Some(
                    sample_elapsed.as_millis().min(u64::MAX as u128) as u64,
                ),
                sample_elapsed_micros: Some(
                    sample_elapsed.as_micros().min(u64::MAX as u128) as u64,
                ),
                a_marker_row_visible_in_current_page: snapshot
                    .a_marker_row_visible_in_current_page,
                b_marker_row_visible_in_current_page: snapshot
                    .b_marker_row_visible_in_current_page,
                a_marker_row_present_in_current_page: snapshot
                    .a_marker_row_present_in_current_page,
                b_marker_row_present_in_current_page: snapshot
                    .b_marker_row_present_in_current_page,
                pixel_content: snapshot.pixel_content,
                regions: snapshot.regions,
                capture,
            },
            started,
        ))
    })??;
    Ok(frame)
}

/// OSC7 与配置写入可能使用不同分隔符大小写；归一化后比较，避免把同一条隔离路径判成 B 启动失败。
///
/// @author kongweiguang
fn directory_matches(observed: Option<&str>, expected: &Path) -> bool {
    let expected = expected.to_string_lossy().replace('\\', "/");
    observed.is_some_and(|path| {
        path.replace('\\', "/")
            .trim_end_matches('/')
            .eq_ignore_ascii_case(expected.trim_end_matches('/'))
    })
}

/// 方向决定目标和禁止出现的 pane；两个方向共用同一实现，避免 A/B 规则分叉。
///
/// @author kongweiguang
fn forbidden_row(expected_row: &str) -> &'static str {
    if expected_row == ROW_A { ROW_B } else { ROW_A }
}

/// 只读取已保存帧在当前渲染日志页中的两个精确历史条目可见性，不做文本模糊匹配。
///
/// @author kongweiguang
fn frame_row_visible(frame: &FrameEvidence, row: &str) -> bool {
    if row == ROW_A {
        frame.a_marker_row_visible_in_current_page
    } else if row == ROW_B {
        frame.b_marker_row_visible_in_current_page
    } else {
        false
    }
}

/// 只读取当前日志页布局中是否存在该精确行；它不能证明完整历史仓储中没有旧数据，
/// 因此报告字段和错误文案都限制为“current page”，避免把分页/虚拟化结果说成全局缺失。
///
/// @author kongweiguang
fn frame_row_present(frame: &FrameEvidence, row: &str) -> bool {
    if row == ROW_A {
        frame.a_marker_row_present_in_current_page
    } else if row == ROW_B {
        frame.b_marker_row_present_in_current_page
    } else {
        false
    }
}

/// 固定区域必须完全落在日志列表和物理视口内；仅存在布局边界但被滚动裁切的
/// 行不能当作目标数据已经显示。
///
/// @author kongweiguang
fn row_visible(window: &gpui_pre::Window, entry_id: &str, list_bounds: Bounds<Pixels>) -> bool {
    selector_bounds(window, &format!("rr-logs-row-{entry_id}"))
        .ok()
        .is_some_and(|bounds| {
            bounds_within(bounds, list_bounds)
                && bounds_inside_viewport(bounds, window.visual_viewport_bounds())
        })
}

/// 行是否进入当前日志页的布局；只用它证明当前页没有混入另一 pane 的条目，
/// 不把它扩展成完整历史仓储或所有分页都缺失该行。
///
/// @author kongweiguang
fn row_present(window: &gpui_pre::Window, entry_id: &str) -> bool {
    selector_bounds(window, &format!("rr-logs-row-{entry_id}")).is_ok()
}

/// 目标行必须同时落在日志列表和可视视口内，不能只凭控件存在就判定数据已经显示。
///
/// @author kongweiguang
fn bounds_within(inner: Bounds<Pixels>, outer: Bounds<Pixels>) -> bool {
    let tolerance = px(GEOMETRY_TOLERANCE);
    !inner.is_empty()
        && !outer.is_empty()
        && inner.origin.x + tolerance >= outer.origin.x
        && inner.origin.y + tolerance >= outer.origin.y
        && inner.origin.x + inner.size.width <= outer.origin.x + outer.size.width + tolerance
        && inner.origin.y + inner.size.height <= outer.origin.y + outer.size.height + tolerance
}

/// 固定区域按逻辑坐标采集，再映射到当前缩放对应的物理像素并逐行哈希；
/// 区域为空或越界时直接失败，避免以零像素摘要冒充固定区域证据。
///
/// @author kongweiguang
fn collect_regions(
    raw: &[u8],
    width: u32,
    height: u32,
    window: &gpui_pre::Window,
    scale: f32,
) -> Result<Vec<RegionEvidence>, io::Error> {
    let mut regions = Vec::with_capacity(FIXED_REGIONS.len());
    for (key, selector) in FIXED_REGIONS {
        let bounds = selector_bounds(window, selector)?;
        if bounds.is_empty() {
            return Err(io::Error::other(format!(
                "right-rail fixed region {selector:?} has empty bounds"
            )));
        }
        let pixel_sha256 = region_pixel_sha256(raw, width, height, bounds, scale, selector)?;
        regions.push(RegionEvidence {
            key,
            selector,
            bounds: BoundsEvidence::from_bounds(bounds),
            pixel_sha256,
        });
    }
    Ok(regions)
}

/// 每次采样都要求整帧和日志列表通过内容门禁；只在最终报告里检查会让
/// 纯黑/均匀空白中间帧继续参与哈希比较，从而伪造“数据已切换”的采样序列。
///
/// @author kongweiguang
fn require_pixel_content(content: &[PixelContentEvidence]) -> Result<(), io::Error> {
    for evidence in content {
        if !evidence.content_verified {
            return Err(io::Error::other(format!(
                "right-rail pixel content gate failed for {:?}: sampled={}, distinct_rgb={} (need {}), luma_range={} (need {}), fully_transparent={}",
                evidence.key,
                evidence.sampled_pixels,
                evidence.distinct_rgb_colors,
                evidence.required_min_distinct_rgb_colors,
                evidence.luma_max.saturating_sub(evidence.luma_min),
                evidence.required_min_luma_range,
                evidence.fully_transparent
            )));
        }
    }
    Ok(())
}

/// 逻辑边界到物理像素的映射只保留一份实现：区域哈希和像素内容门禁必须
/// 采样同一块像素，否则“区域变化”和“内容有效”可能指向不同截图位置。
///
/// @author kongweiguang
fn physical_rect(
    width: u32,
    height: u32,
    bounds: Bounds<Pixels>,
    scale: f32,
    selector: &str,
) -> Result<PhysicalRect, io::Error> {
    if !scale.is_finite() || scale <= 0.0 {
        return Err(io::Error::other(format!(
            "right-rail fixed region {selector:?} has invalid scale {scale}"
        )));
    }
    let left_f = (bounds.origin.x.as_f32() * scale).floor().max(0.0);
    let top_f = (bounds.origin.y.as_f32() * scale).floor().max(0.0);
    let right_f = ((bounds.origin.x.as_f32() + bounds.size.width.as_f32()) * scale).ceil();
    let bottom_f = ((bounds.origin.y.as_f32() + bounds.size.height.as_f32()) * scale).ceil();
    if !left_f.is_finite() || !top_f.is_finite() || !right_f.is_finite() || !bottom_f.is_finite() {
        return Err(io::Error::other(format!(
            "right-rail fixed region {selector:?} has non-finite pixel bounds"
        )));
    }
    let left = left_f.min(width as f32) as u32;
    let top = top_f.min(height as f32) as u32;
    let right = (right_f as i64).clamp(left as i64, width as i64) as u32;
    let bottom = (bottom_f as i64).clamp(top as i64, height as i64) as u32;
    if right <= left || bottom <= top {
        return Err(io::Error::other(format!(
            "right-rail fixed region {selector:?} has no physical pixels"
        )));
    }
    Ok(PhysicalRect {
        left,
        top,
        right,
        bottom,
    })
}

/// 统计采样区域的透明像素、不同 RGB 数和亮度跨度；颜色语义不参与判断，
/// 只用最低门槛排除纯黑、全透明和均匀空白，避免把“有像素”当成“有目标数据”。
///
/// @author kongweiguang
fn pixel_content_evidence(
    raw: &[u8],
    width: u32,
    height: u32,
    rect: PhysicalRect,
    key: &'static str,
    required_min_distinct_rgb_colors: u32,
    required_min_luma_range: u8,
) -> Result<PixelContentEvidence, io::Error> {
    let stride = width as usize * 4;
    if raw.len() < stride * height as usize {
        return Err(io::Error::other(
            "right-rail rendered image buffer is shorter than its dimensions",
        ));
    }
    let mut colors = HashSet::new();
    let mut luma_min = u8::MAX;
    let mut luma_max = u8::MIN;
    let mut sampled_pixels = 0_u64;
    let mut fully_transparent = true;
    for row in rect.top..rect.bottom {
        let start = row as usize * stride + rect.left as usize * 4;
        let end = row as usize * stride + rect.right as usize * 4;
        for pixel in raw[start..end].as_chunks::<4>().0 {
            sampled_pixels += 1;
            if pixel[3] == 0 {
                continue;
            }
            // 只对不透明像素计算颜色和亮度，避免透明背景的 RGB 残留伪造内容跨度。
            fully_transparent = false;
            // 门禁只需要“至少 N 种颜色”；达到阈值后停止记录，避免整帧数百万种颜色
            // 在每次轮询时撑大集合。
            if (colors.len() as u32) < required_min_distinct_rgb_colors {
                colors.insert(
                    u32::from(pixel[0]) << 16 | u32::from(pixel[1]) << 8 | u32::from(pixel[2]),
                );
            }
            let luma = ((u32::from(pixel[0]) * 299
                + u32::from(pixel[1]) * 587
                + u32::from(pixel[2]) * 114)
                / 1000) as u8;
            luma_min = luma_min.min(luma);
            luma_max = luma_max.max(luma);
        }
    }
    if sampled_pixels == 0 {
        return Err(io::Error::other(format!(
            "right-rail pixel content region {key:?} sampled no pixels"
        )));
    }
    let distinct_rgb_colors = colors.len().min(u32::MAX as usize) as u32;
    let luma_range = luma_max.saturating_sub(luma_min);
    let content_verified = !fully_transparent
        && distinct_rgb_colors >= required_min_distinct_rgb_colors
        && luma_range >= required_min_luma_range;
    Ok(PixelContentEvidence {
        key,
        physical_bounds: PhysicalBoundsEvidence {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        },
        sampled_pixels,
        distinct_rgb_colors,
        luma_min,
        luma_max,
        fully_transparent,
        required_min_distinct_rgb_colors,
        required_min_luma_range,
        content_verified,
    })
}

/// 区域哈希直接读取渲染缓冲区的原始 RGBA，避免 PNG 编码差异掩盖像素变化；
/// 逐行只取区域列宽，既不复制整帧也不把相邻区域的像素算进摘要。
///
/// @author kongweiguang
fn region_pixel_sha256(
    raw: &[u8],
    width: u32,
    height: u32,
    bounds: Bounds<Pixels>,
    scale: f32,
    selector: &str,
) -> Result<String, io::Error> {
    let rect = physical_rect(width, height, bounds, scale, selector)?;
    let stride = width as usize * 4;
    if raw.len() < stride * height as usize {
        return Err(io::Error::other(
            "right-rail rendered image buffer is shorter than its dimensions",
        ));
    }
    let mut hasher = Sha256::new();
    for row in rect.top..rect.bottom {
        let start = row as usize * stride + rect.left as usize * 4;
        let end = row as usize * stride + rect.right as usize * 4;
        hasher.update(&raw[start..end]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 固定区域先按顺序核对 key 和 selector，再要求所有逻辑边界有限且非空；
/// 后续帧只允许在 0.6 逻辑像素内变化，主题和文字内容的变化不在此处放宽。
///
/// @author kongweiguang
fn validate_frame_geometry(regions: &[RegionEvidence]) -> Result<(), Box<dyn Error>> {
    if regions.len() != FIXED_REGIONS.len() {
        return Err(io::Error::other(format!(
            "right-rail frame has {} fixed regions, expected {}",
            regions.len(),
            FIXED_REGIONS.len()
        ))
        .into());
    }
    for (region, (key, selector)) in regions.iter().zip(FIXED_REGIONS) {
        if region.key != key || region.selector != selector {
            return Err(io::Error::other(format!(
                "right-rail fixed region order mismatch: expected={key}/{selector}, observed={}/{}",
                region.key, region.selector
            ))
            .into());
        }
        let bounds = region.bounds;
        if !bounds.x.is_finite()
            || !bounds.y.is_finite()
            || !bounds.width.is_finite()
            || !bounds.height.is_finite()
            || bounds.width <= 0.0
            || bounds.height <= 0.0
        {
            return Err(io::Error::other(format!(
                "right-rail fixed region {key} has invalid bounds"
            ))
            .into());
        }
    }
    Ok(())
}

/// 只允许固定区域几何在容差内变化；目标文字、悬停和日志内容不参与几何一致性判断。
///
/// @author kongweiguang
fn geometry_matches(reference: &[RegionEvidence], candidate: &[RegionEvidence]) -> bool {
    reference.len() == candidate.len()
        && reference.iter().zip(candidate).all(|(left, right)| {
            left.key == right.key
                && left.selector == right.selector
                && (left.bounds.x - right.bounds.x).abs() <= GEOMETRY_TOLERANCE
                && (left.bounds.y - right.bounds.y).abs() <= GEOMETRY_TOLERANCE
                && (left.bounds.width - right.bounds.width).abs() <= GEOMETRY_TOLERANCE
                && (left.bounds.height - right.bounds.height).abs() <= GEOMETRY_TOLERANCE
        })
}

/// 固定 key 必须唯一，缺失时返回类型化错误，避免把“没有区域”误判为内容稳定。
///
/// @author kongweiguang
fn region_sha256<'a>(regions: &'a [RegionEvidence], key: &str) -> Result<&'a str, io::Error> {
    regions
        .iter()
        .find(|region| region.key == key)
        .map(|region| region.pixel_sha256.as_str())
        .ok_or_else(|| io::Error::other(format!("right-rail fixed region {key} is missing")))
}
