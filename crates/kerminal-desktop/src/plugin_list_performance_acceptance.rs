//! 正式原生RPC列表十万行采样，保留帧时间线和业务行可见证据。
//! @author kongweiguang

use crate::{
    bootstrap,
    composition::DesktopComposition,
    plugins::{arguments::PluginStartupOptions, startup::StartupPlugins},
    runtime::ApplicationRuntime,
};
use gpui_kit::{
    AnyWindowHandle, AppContext as _, AsyncApp, Entity, WindowOptions,
    component::{ActiveTheme as _, TitleBar},
    profiler::{self, FrameEvent, FrameTimingCollector},
};
use gpui_pre::{Bounds, WindowBounds, px, size};
use kerminal_application::preferences::ThemeMode;
use kerminal_domain::identity::TabId;
use kerminal_ui::{
    AppServices, QuitIntent, RootView, RootViewEvent,
    acceptance_metrics::{FrameMetricsEnvironment, RendererKind, capture_window_frame_metrics},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    error::Error,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[cfg(windows)]
#[allow(dead_code)]
#[path = "right_rail_acceptance/isolation.rs"]
mod isolation;

const WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(16);
const TOTAL_ROWS: usize = 100_000;
const CONTINUOUS_TARGETS: usize = 500;
/// 与 PowerShell 门禁共用同一帧样本下限，单帧或少量帧不能证明列表在采样窗内持续呈现。
const MIN_FRAME_SAMPLES: usize = 100;
/// 采样窗前后的每一半都必须有独立帧证据，不能用集中在某一瞬间的帧冒充整窗活动。
const MIN_FRAME_SAMPLES_PER_HALF: usize = MIN_FRAME_SAMPLES / 2;
/// 即使视口很小，观测跨度也不能低于八行；避免两行报告绕过前向页证明。
const MIN_OBSERVED_SPAN: usize = 8;
/// 连续阶段只接受真实前向：观测起点不得回到请求前范围，也无须超过目标太多；
/// 渲染超采样在起点和跨度上各保留两行的可解释余量，超过即视为范围漂移而非同一页前进。
const CONTINUOUS_RANGE_TOLERANCE: usize = 2;
/// 逐条观测与报告共用同一输入来源文本，PS 门禁可拒绝把物理输入或其它滚动通道写进证据。
const INPUT_MODE: &str = "production VirtualListScrollHandle.scroll_to_item";
/// 正式 RPC 列表身份固定到具体外部插件、视图和 Sidecar 运行时，宿主 fixture 不能靠相同快照形状冒充。
const LIST_CONTENT_ID: &str = "test.list-performance/records";
const LIST_PLUGIN_ID: &str = "test.list-performance";
const LIST_VIEW: &str = "records";
const LIST_RUNTIME: &str = "sidecar";
const HOST_LIST_CONTENT_ID: &str = "acceptance.host-list";
const HOST_LIST_PLUGIN_ID: &str = "fixture.host-list";
/// 副本不可变策略同时写入报告与 PS 门禁；只读位与摘要重算共同覆盖原地写和同尺寸换入。
const IMMUTABLE_COPY_MODE: &str = "all acceptance-copy files set read-only before startup; full-tree digest recomputed after sampling";
/// 摘要规则同时写进报告并逐字校验，跨语言重算不能各自漂移到不同拼接方式。
const PLUGIN_DIGEST_RULE: &str = "sha256 over all regular files under plugin root, sorted by ordinal UTF-8 path bytes, each file contributing uint64-le path length, path bytes, uint64-le content length, content bytes";
/// 预暖末目标停在 20_000；采样首目标必须由实际可视范围重新推导，不能与它原地重合。
const WARMUP_TARGETS: [usize; 2] = [50_000, 20_000];
/// 15 个首中末目标显式避开相邻重复，避免把原地滚动算作一次往返。
const ROUNDTRIP_TARGETS: [usize; 15] = [
    0, 49_999, 99_999, 49_999, 0, 49_999, 0, 99_999, 0, 49_999, 0, 99_999, 49_999, 0, 49_999,
];

/// 可视范围来自renderer最近一帧实际构造的范围，包含其绘制超采样，不代表物理显示边界。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VisibleRange {
    start: usize,
    end: usize,
}

impl VisibleRange {
    /// 只按半开区间判断，目标恰在 end 时属于视口之外的真实前进位置。
    fn contains(self, index: usize) -> bool {
        index >= self.start && index < self.end
    }
}

/// 一次列表快照解析出的强类型证据；字符串、数字和布尔字段都在解析处失败关闭。
#[derive(Clone, Debug, PartialEq, Eq)]
struct ListEvidence {
    instance: String,
    generation: u64,
    window_visible: bool,
    total: usize,
    range: VisibleRange,
    rows: Vec<(usize, String)>,
    fetch_requests: u64,
    fetch_successes: u64,
    fetch_errors: u64,
}

/// 列表尚未就绪与已经可验证必须区分，避免把Opening或缺失集合当成成功样本。
#[derive(Debug)]
enum ListSnapshot {
    Waiting,
    Ready(ListEvidence),
}

/// 插件实例身份和进程代次共同固定采样归属；任一变化都说明发生了重建或重启。
#[derive(Clone, Debug, PartialEq, Eq)]
struct ExpectedInstance {
    instance: String,
    generation: u64,
}

#[derive(Clone)]
struct Options {
    output: PathBuf,
    plugin: PathBuf,
    theme: String,
}

/// 独立根和正式插件扫描保持与产品相同组合；结束始终等待正常关闭截止时间。
pub(super) fn run() -> Result<(), Box<dyn Error>> {
    let mut options = options(std::env::args_os().skip(2))?;
    fs::create_dir(&options.output)?;
    // 宿主必须从输出根内的验收副本读取插件：原 fixture 之后被改动或替换都不会影响本轮
    // 实际执行的字节，PS 也能对报告声明的同一目录独立重算摘要。
    let source_plugin = options.plugin.clone();
    let plugin_copy = options.output.join("plugin");
    copy_plugin_tree(&source_plugin, &plugin_copy)?;
    // 副本文件在启动前统一设为只读；宿主只能读取，运行期间任何写入都会失败，
    // 再配合采样结束摘要重算覆盖同尺寸换入换出的替换场景。
    mark_copy_read_only(&plugin_copy)?;
    options.plugin = plugin_copy.canonicalize()?;
    let config = options.output.join("config");
    fs::create_dir(&config)?;
    // SAFETY: 本入口尚未创建线程，只设置当前候选的独立数据根。
    unsafe {
        std::env::set_var("KERMINAL_CONFIG_ROOT", &config);
    }
    let candidate_hash = digest(&fs::read(std::env::current_exe()?)?);
    let runtime = ApplicationRuntime::new()?;
    let primary =
        match crate::acquire_single_instance(&runtime, &crate::single_instance_id(&config)?)? {
            single_application::SingleApplication::PrimaryInstance(primary) => primary,
            _ => return Err(io::Error::other("验收根已有实例").into()),
        };
    let startup = Arc::new(StartupPlugins::load(
        &config,
        PluginStartupOptions {
            safe_mode: false,
            dev_plugin: Some(options.plugin.clone()),
        },
    )?);
    if !startup
        .enabled
        .iter()
        .any(|plugin| plugin.id.as_str() == "test.list-performance")
    {
        return Err(io::Error::other("十万行fixture未加载").into());
    }
    // 候选身份必须绑定实际执行的全部包文件：Sidecar 二进制、native 视图和清单本身都在内，
    // 只摘要 manifest/resources 会让替换 Sidecar 却复用旧摘要的候选混过验收。
    let plugin_hash = plugin_tree_digest(&options.plugin)?;
    let catalogs = startup.catalogs("en")?;
    bootstrap::run_after_migration(
        &bootstrap::migration_service(&config, None, catalogs.1.clone()),
        false,
        |_| (),
    )?;
    let mut composition = runtime.block_on(DesktopComposition::build_with_plugin_catalogs(
        runtime.handle(),
        &config,
        None,
        catalogs.0,
        catalogs.1,
    ))?;
    let ui: Result<_, Box<dyn Error>> =
        match runtime.block_on(composition.install_external_plugins(startup)) {
            Ok(()) => run_window(composition.app_services(), &options),
            Err(error) => Err(error.into()),
        };
    let deadline = ui
        .as_ref()
        .map(|(_, deadline)| *deadline)
        .unwrap_or_else(|_| Instant::now() + Duration::from_secs(12));
    let shutdown = runtime.block_on(composition.shutdown_until(deadline.into()));
    drop(primary);
    runtime.shutdown();
    let (mut report, _) = ui?;
    shutdown?;
    // 副本在采样窗内必须保持逐字节不变；PS 会从同一路径重算摘要，任何中途替换都在两处暴露。
    if plugin_tree_digest(&options.plugin)? != plugin_hash {
        return Err(io::Error::other("插件验收副本在采样期间发生变化").into());
    }
    report["candidateSha256"] = json!(candidate_hash);
    report["pluginSha256"] = json!(plugin_hash);
    report["pluginDigestRule"] = json!(PLUGIN_DIGEST_RULE);
    report["pluginSourcePath"] = json!(source_plugin.to_string_lossy().into_owned());
    report["pluginCopyPath"] = json!(options.plugin.to_string_lossy().into_owned());
    report["pluginCopyStable"] = json!(true);
    report["immutableMode"] = json!(IMMUTABLE_COPY_MODE);
    report["shutdownComplete"] = json!(true);
    fs::write(
        options.output.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}

/// 参数必须明确绑定候选插件和全新根，不回退用户数据目录或忽略未知选项。
fn options(arguments: impl IntoIterator<Item = OsString>) -> Result<Options, io::Error> {
    let mut values = std::collections::BTreeMap::new();
    let mut arguments = arguments.into_iter();
    while let Some(key) = arguments.next() {
        let key = key
            .into_string()
            .map_err(|_| io::Error::other("参数编码无效"))?;
        if !["--output-dir", "--dev-plugin", "--theme"].contains(&key.as_str()) {
            return Err(io::Error::other("未知原生列表验收选项"));
        }
        let value = arguments
            .next()
            .ok_or_else(|| io::Error::other("验收选项缺少值"))?;
        if values.insert(key, value).is_some() {
            return Err(io::Error::other("验收选项重复"));
        }
    }
    let output = PathBuf::from(
        values
            .remove("--output-dir")
            .ok_or_else(|| io::Error::other("缺少输出根"))?,
    );
    let plugin = PathBuf::from(
        values
            .remove("--dev-plugin")
            .ok_or_else(|| io::Error::other("缺少插件目录"))?,
    )
    .canonicalize()?;
    let theme = values
        .remove("--theme")
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(|| "system".into());
    if !output.is_absolute()
        || output.exists()
        || !["light", "dark", "system"].contains(&theme.as_str())
    {
        return Err(io::Error::other("验收根或主题无效"));
    }
    Ok(Options {
        output,
        plugin,
        theme,
    })
}

/// 摘要固定真实文件字节，文件名或版本不能代替候选身份。
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// 包内全部普通文件按路径字节序参与摘要；路径与内容各自长度前缀，
/// 使文件名和内容边界不会因简单拼接产生歧义，也让 PowerShell 能独立重算出同一结果。
fn plugin_content_digest(files: &std::collections::BTreeMap<String, Vec<u8>>) -> String {
    let mut hash = Sha256::new();
    for (path, bytes) in files {
        hash.update((path.len() as u64).to_le_bytes());
        hash.update(path.as_bytes());
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    format!("{:x}", hash.finalize())
}

/// 从验收副本递归读取全部普通文件并按共享规则摘要；PS 对同一目录使用相同路径字节序和
/// 长度前缀，因此任何额外注入、二进制替换或视图修改都会让两侧摘要同时变化。
fn plugin_tree_digest(directory: &Path) -> Result<String, io::Error> {
    let mut files = BTreeMap::new();
    collect_plugin_files(directory, directory, &mut files)?;
    Ok(plugin_content_digest(&files))
}

/// 递归枚举普通文件并拒绝链接；相对路径统一使用 `/`，与 PowerShell 重算规则逐字一致。
fn collect_plugin_files(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), io::Error> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| io::Error::other(format!("读取插件目录失败：{error}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io::Error::other(format!("枚举插件目录失败：{error}")))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| io::Error::other(format!("读取插件项属性失败：{error}")))?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::other(format!(
                "插件副本包含链接，不能生成可重算摘要：{}",
                path.display()
            )));
        }
        if metadata.is_dir() {
            collect_plugin_files(root, &path, files)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| io::Error::other("插件文件路径不在副本根内"))?
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let bytes = fs::read(&path)
                .map_err(|error| io::Error::other(format!("读取插件文件失败：{error}")))?;
            files.insert(relative, bytes);
        } else {
            return Err(io::Error::other(format!(
                "插件副本包含非普通文件：{}",
                path.display()
            )));
        }
    }
    Ok(())
}

/// 逐项复制插件包并拒绝链接；验收副本必须只包含本轮输出根内的普通文件，
/// 否则宿主仍可能读取连接指向的外部字节，摘要也无法代表实际执行内容。
fn copy_plugin_tree(source: &Path, target: &Path) -> Result<(), io::Error> {
    fs::create_dir(target)?;
    copy_plugin_directory(source, target)
}

/// 递归把副本普通文件设为只读；目录保持可遍历，宿主只读路径不受影响。
/// 只读位阻止原地覆盖，采样结束的全文件摘要再拦截删除后同尺寸换入。
fn mark_copy_read_only(directory: &Path) -> Result<(), io::Error> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| io::Error::other(format!("读取插件副本目录失败：{error}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io::Error::other(format!("枚举插件副本目录失败：{error}")))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| io::Error::other(format!("读取插件副本项属性失败：{error}")))?;
        if metadata.is_dir() {
            mark_copy_read_only(&path)?;
        } else if metadata.is_file() {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&path, permissions)
                .map_err(|error| io::Error::other(format!("设置插件副本只读失败：{error}")))?;
        }
    }
    Ok(())
}

/// 目录递归保持普通文件与目录语义；排序只让失败诊断稳定，不参与摘要计算。
fn copy_plugin_directory(source: &Path, target: &Path) -> Result<(), io::Error> {
    let mut entries = fs::read_dir(source)
        .map_err(|error| io::Error::other(format!("读取插件目录失败：{error}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io::Error::other(format!("枚举插件目录失败：{error}")))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let from = entry.path();
        let to = target.join(entry.file_name());
        let metadata = fs::symlink_metadata(&from)
            .map_err(|error| io::Error::other(format!("读取插件项属性失败：{error}")))?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::other(format!(
                "插件包包含链接，不能复制为验收副本：{}",
                from.display()
            )));
        }
        if metadata.is_dir() {
            fs::create_dir(&to)?;
            copy_plugin_directory(&from, &to)?;
        } else if metadata.is_file() {
            let _ = fs::copy(&from, &to).map_err(|error| {
                io::Error::other(format!("复制插件文件{}失败：{error}", from.display()))
            })?;
        } else {
            return Err(io::Error::other(format!(
                "插件包包含非普通文件：{}",
                from.display()
            )));
        }
    }
    Ok(())
}

/// 私有桌面的真实可见窗口沿普通 Root 装配；退出必须收到生产关闭回执。
fn run_window(
    services: AppServices,
    options: &Options,
) -> Result<(Value, Instant), Box<dyn Error>> {
    let options = options.clone();
    let outcome = Arc::new(Mutex::new(None::<Result<Value, String>>));
    let retained = outcome.clone();
    let deadline = Arc::new(Mutex::new(None::<Instant>));
    let retained_deadline = deadline.clone();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            kerminal_ui::init(cx);
            let options = options.clone();
            let completed = outcome.clone();
            let closing = deadline.clone();
            let result = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: gpui_kit::point(px(40.), px(40.)),
                        size: size(px(1024.), px(720.)),
                    })),
                    show: true,
                    focus: true,
                    ..TitleBar::window_options()
                },
                cx,
                move |window, cx| {
                    let root = cx.new(|cx| RootView::new(services, window, cx));
                    cx.subscribe(&root, move |_, event: &RootViewEvent, _| {
                        if let RootViewEvent::QuitReady { deadline, .. } = event {
                            *closing.lock().unwrap_or_else(|error| error.into_inner()) =
                                Some(*deadline);
                        }
                    })
                    .detach();
                    let owned = root.clone();
                    let window = window.window_handle();
                    cx.spawn(async move |cx| {
                        let result = exercise(cx, &owned, window, &options).await;
                        *completed.lock().unwrap_or_else(|error| error.into_inner()) =
                            Some(result.map_err(|error| error.to_string()));
                        cx.update(|cx| cx.quit());
                    })
                    .detach();
                    root
                },
            );
            if let Err(error) = result {
                *outcome.lock().unwrap_or_else(|error| error.into_inner()) =
                    Some(Err(error.to_string()));
                cx.quit();
            }
        });
    let report = retained
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .ok_or_else(|| io::Error::other("原生列表验收未返回"))?
        .map_err(io::Error::other)?;
    let deadline = retained_deadline
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .ok_or_else(|| io::Error::other("缺少生产退出回执"))?;
    Ok((report, deadline))
}

/// 临时采样状态必须在异常返回时也恢复，避免影响同进程其他验收。
struct TraceRestore(bool);
impl Drop for TraceRestore {
    /// 恢复原始开关，不把验收采样遗留为应用默认设置。
    fn drop(&mut self) {
        profiler::set_trace_enabled(self.0);
    }
}

/// 按可视页采样后由单一生产退出事务完成内容收尾，避免额外异步关闭与工作区保存竞争。
/// 轮询只证明业务行构造已可观察，不能当作显示延迟。
async fn exercise(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    options: &Options,
) -> Result<Value, Box<dyn Error>> {
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.enable_right_rail_acceptance_activity(cx)
        })
    })?;
    wait_workspace(cx, root, window).await?;
    // 标签身份是整轮来源绑定的锚点：之后每次快照都要求它仍是活动外部内容标签，
    // 不能靠活动作用域滚动到哪个列表就采哪个列表。
    let tab = window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.set_right_rail_acceptance_theme(
                match options.theme.as_str() {
                    "light" => ThemeMode::Light,
                    "dark" => ThemeMode::Dark,
                    _ => ThemeMode::System,
                },
                cx,
            );
            root.plugin_list_performance_open(window, cx)
        })
    })??;
    // 首屏先建立实例和代次基线；之后每个目标都必须由同一实例构造，不能用替身范围补位。
    let (first, first_evidence) = wait_target(cx, root, window, &tab, 0, None, None).await?;
    let source = inspect_source(&first, &tab)?
        .ok_or_else(|| io::Error::other("首屏就绪后仍缺少来源证据"))?;
    let expected = ExpectedInstance {
        instance: first_evidence.instance.clone(),
        generation: first_evidence.generation,
    };
    // 首屏范围是整条证据链的唯一锚点：之后每一步的请求前范围必须等于上一步实际观测范围，
    // 这样 PS 侧能逐条重放，无法通过删改中间观测伪造跳步或往返。
    let mut previous_visible = first_evidence.range;
    // 先完成远离 0 的预暖，再让采样首目标由实际可视范围推导，避免预暖和首个样本原地重合。
    let mut warmup_targets = Vec::with_capacity(WARMUP_TARGETS.len());
    let mut warmup_observations = Vec::with_capacity(WARMUP_TARGETS.len());
    for (step, index) in WARMUP_TARGETS.into_iter().enumerate() {
        let baseline = current_range(cx, root, window, &tab)?;
        require_contiguous_baseline(baseline, previous_visible)?;
        validate_request_target(baseline, index, TOTAL_ROWS)?;
        request_scroll(cx, root, window, index)?;
        let (snapshot, observed) = wait_target(
            cx,
            root,
            window,
            &tab,
            index,
            Some(baseline),
            Some(&expected),
        )
        .await?;
        warmup_observations.push(warmup_observation(
            step,
            index,
            baseline,
            observed.range,
            &snapshot,
        ));
        previous_visible = observed.range;
        warmup_targets.push(index);
    }
    let sampling_started_unix_ms = unix_ms()?;
    // 采样窗起点读取累计分页计数；结束后再读同一字段，成功的净增长证明目标行由本轮新页构造。
    let fetch_start = current_evidence(cx, root, window, &tab)?;
    let origin = Instant::now();
    let old_trace = profiler::trace_enabled();
    let _trace = TraceRestore(old_trace);
    profiler::set_trace_enabled(true);
    let mut collector = FrameTimingCollector::new();
    let isolation = isolation_evidence()?;
    let (frame_baseline, window_id, metadata) = window.update(cx, |_, window, app| {
        let theme = app.theme();
        let viewport = window.viewport_size();
        (
            window.frame_duration_snapshot(),
            window.window_handle().window_id(),
            // visible只在采样起点读取一次；它是OS窗口可见性，列表的content可见性由每帧快照单独记录。
            json!({"viewport":[viewport.width.as_f32(),viewport.height.as_f32()],"scaleFactor":window.scale_factor(),"fontFamily":theme.font_family.to_string(),"fontSize":theme.font_size.as_f32(),"darkMode":theme.is_dark(),"themeScope":"darkMode read from the live GPUI Theme at sampling start; system mode is not forced to a specific appearance","debugAssertions":cfg!(debug_assertions),"targetOs":std::env::consts::OS,"targetArch":std::env::consts::ARCH,"visible":window.is_visible(),"visibleScope":"OS window visibility sampled once at sampling start; list.visible is renderer content visibility, not OS window visibility","visibleSampledAt":"sampling-start","active":window.is_window_active(),"activeScope":"OS window active state sampled once at sampling start in a private desktop; not proof the window remained focused or received user input","nativeIsolation":isolation}),
        )
    })?;
    let minimum_observed_span = minimum_observed_span(&metadata)?;
    let mut raw = Vec::new();
    let mut observations = Vec::new();
    let mut previous_target = warmup_targets
        .last()
        .copied()
        .ok_or_else(|| io::Error::other("缺少预暖目标"))?;
    // 连续阶段把实际可视跨度作为步距，目标至少领先一整页；极高视口下无法继续前进时显式失败。
    for step in 0..CONTINUOUS_TARGETS {
        let baseline = current_range(cx, root, window, &tab)?;
        require_contiguous_baseline(baseline, previous_visible)?;
        let target = continuous_target(baseline, Some(previous_target), TOTAL_ROWS)?;
        validate_request_target(baseline, target, TOTAL_ROWS)?;
        let requested = Instant::now();
        request_scroll(cx, root, window, target)?;
        let (snapshot, observed) = wait_target(
            cx,
            root,
            window,
            &tab,
            target,
            Some(baseline),
            Some(&expected),
        )
        .await?;
        // 连续阶段必须真实前向：仅“范围发生变化”仍可能回到第 0 行或跳到无关区域，
        // 因此这里同时约束起点、目标邻近度和跨度，与 PS 对同一条观测做逐项重放。
        validate_continuous_forward(baseline, target, observed.range, minimum_observed_span)?;
        drain_frames(&mut collector, window_id, origin, &mut raw);
        observations.push(observation(
            "continuous",
            step,
            target,
            baseline,
            observed.range,
            requested,
            origin,
            &snapshot,
        ));
        previous_visible = observed.range;
        previous_target = target;
    }
    // 15 个首中末目标按显式序列验证；相邻目标不同，且每个目标都必须落在请求后的新范围。
    if ROUNDTRIP_TARGETS.windows(2).any(|pair| pair[0] == pair[1])
        || ROUNDTRIP_TARGETS[0] == previous_target
    {
        return Err(io::Error::other("往返目标序列存在相邻重复").into());
    }
    for (step, target) in ROUNDTRIP_TARGETS.into_iter().enumerate() {
        let baseline = current_range(cx, root, window, &tab)?;
        require_contiguous_baseline(baseline, previous_visible)?;
        validate_request_target(baseline, target, TOTAL_ROWS)?;
        let requested = Instant::now();
        request_scroll(cx, root, window, target)?;
        let (snapshot, observed) = wait_target(
            cx,
            root,
            window,
            &tab,
            target,
            Some(baseline),
            Some(&expected),
        )
        .await?;
        // 往返阶段同样要求完整视口跨度，不能以两行残片证明目标页。
        validate_observed_span(observed.range, minimum_observed_span)?;
        drain_frames(&mut collector, window_id, origin, &mut raw);
        observations.push(observation(
            "roundtrip",
            step,
            target,
            baseline,
            observed.range,
            requested,
            origin,
            &snapshot,
        ));
        previous_visible = observed.range;
    }
    // 等待已排队的原生呈现收尾；不主动draw制造性能样本。时间窗在收尾帧之后闭合，
    // 这样 raw-frames 中每条记录都落在报告声明的采样窗内，PS 不会把收尾帧误判为窗外数据。
    cx.background_executor()
        .timer(Duration::from_millis(120))
        .await;
    drain_frames(&mut collector, window_id, origin, &mut raw);
    let sampling_finished_unix_ms = unix_ms()?;
    let sampling_elapsed_ms = u64::try_from(origin.elapsed().as_millis())
        .map_err(|_| io::Error::other("列表采样时长超出u64"))?;
    if sampling_finished_unix_ms < sampling_started_unix_ms
        || sampling_finished_unix_ms
            .saturating_sub(sampling_started_unix_ms)
            .abs_diff(sampling_elapsed_ms)
            > 10
    {
        return Err(io::Error::other("列表采样墙钟与单调时钟不一致").into());
    }
    // 成功页计数必须在采样窗内净增长；仅读取缓存或重复旧行不能作为本轮目标行证据。
    let fetch_end = current_evidence(cx, root, window, &tab)?;
    if fetch_end.fetch_successes <= fetch_start.fetch_successes || fetch_end.fetch_errors > 0 {
        return Err(io::Error::other(format!(
            "列表采样窗内未产生新的成功分页：起点{}次成功/{}次错误，终点{}次成功/{}次错误",
            fetch_start.fetch_successes,
            fetch_start.fetch_errors,
            fetch_end.fetch_successes,
            fetch_end.fetch_errors
        ))
        .into());
    }
    let metrics = window.update(cx, |_, window, _| {
        capture_window_frame_metrics(
            window,
            Some(&frame_baseline),
            FrameMetricsEnvironment::new(
                "private-desktop",
                "native-rpc-list-100000",
                RendererKind::Unknown,
            ),
        )
        .map_err(io::Error::other)
    })??;
    let half_limit_ns = sampling_elapsed_ms.saturating_mul(1_000_000) / 2;
    let count_frames = |kind: &str, second_half: bool| {
        raw.iter()
            .filter(|event| {
                event["kind"] == kind
                    && event["startNs"]
                        .as_u64()
                        .is_some_and(|start| (start >= half_limit_ns) == second_half)
            })
            .count()
    };
    let draw = count_frames("draw", false) + count_frames("draw", true);
    let present = count_frames("present", false) + count_frames("present", true);
    if draw < MIN_FRAME_SAMPLES || present < MIN_FRAME_SAMPLES {
        return Err(io::Error::other(format!(
            "真实帧样本不足：draw {draw}，present {present}，下限 {MIN_FRAME_SAMPLES}"
        ))
        .into());
    }
    // 前后两半各自都需要达到下限的一半，防止帧集中在采样窗某一瞬间。
    for (kind, first, second) in [
        (
            "draw",
            count_frames("draw", false),
            count_frames("draw", true),
        ),
        (
            "present",
            count_frames("present", false),
            count_frames("present", true),
        ),
    ] {
        if first < MIN_FRAME_SAMPLES_PER_HALF || second < MIN_FRAME_SAMPLES_PER_HALF {
            return Err(io::Error::other(format!(
                "采样窗前后两半{kind}帧不足：前半{first}，后半{second}，每半下限{MIN_FRAME_SAMPLES_PER_HALF}"
            ))
            .into());
        }
    }
    fs::write(
        options.output.join("raw-frames.json"),
        serde_json::to_vec_pretty(&raw)?,
    )?;
    fs::write(
        options.output.join("row-observations.json"),
        serde_json::to_vec_pretty(&observations)?,
    )?;
    // 由同一生产退出事务关闭内容并保存工作区，不能先发异步关标签再立即退出形成 Busy 竞态。
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| root.request_quit(QuitIntent::Exit, cx))
    })?;
    let deadline = Instant::now() + WAIT;
    loop {
        let quit = window.update(cx, |_, _, app| {
            root.read(app)
                .transfer_workspace_acceptance_quit_ready()
                .map_err(io::Error::other)
        })??;
        if quit {
            break;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("生产关闭超时").into());
        }
        cx.background_executor().timer(POLL).await;
    }
    Ok(
        json!({"schemaVersion":3,"processId":std::process::id(),"theme":options.theme,"mode":"private-desktop-native-list","totalRows":TOTAL_ROWS,"source":source,"minimumObservedSpan":minimum_observed_span,"fetchWindow":json!({"start":{"requests":fetch_start.fetch_requests,"successes":fetch_start.fetch_successes,"errors":fetch_start.fetch_errors},"end":{"requests":fetch_end.fetch_requests,"successes":fetch_end.fetch_successes,"errors":fetch_end.fetch_errors}}),"samplingStartedUnixMs":sampling_started_unix_ms,"samplingFinishedUnixMs":sampling_finished_unix_ms,"samplingElapsedMs":sampling_elapsed_ms,"continuousTargets":CONTINUOUS_TARGETS,"roundtripTargets":ROUNDTRIP_TARGETS.len(),"warmupTargets":warmup_targets,"warmupObservations":warmup_observations,"continuousStepRule":"index = baselineVisibleEnd + (baselineVisibleEnd - baselineVisibleStart)","roundtripSequence":ROUNDTRIP_TARGETS.to_vec(),"firstScreen":first,"metadata":metadata,"frameMetrics":metrics,"rawFrames":"raw-frames.json","rowObservations":"row-observations.json","observationMeaning":"Each target row was constructed by the production native renderer inside a visible range that changed after the scroll request; OS window visibility was sampled once at sampling start and list.visible is renderer content visibility, not OS window visibility; this is not a frame count, system compositing proof, physical display timing, or input-to-display latency.","inputMode":INPUT_MODE,"physicalInputTested":false,"rendererBackend":"unknown; hardware inventory is recorded separately"}),
    )
}

/// 观测只把请求前范围、请求后范围和原始实体快照并列保存；不从样本推导帧率或物理显示时延。
fn observation(
    phase: &str,
    step: usize,
    index: usize,
    baseline: VisibleRange,
    visible: VisibleRange,
    requested: Instant,
    origin: Instant,
    snapshot: &Value,
) -> Value {
    json!({
        "phase": phase,
        "step": step,
        "index": index,
        "baselineVisibleStart": baseline.start,
        "baselineVisibleEnd": baseline.end,
        "visibleStart": visible.start,
        "visibleEnd": visible.end,
        "requestedNs": ns(requested.saturating_duration_since(origin)),
        "observedNs": ns(Instant::now().saturating_duration_since(origin)),
        "inputMode": INPUT_MODE,
        "list": snapshot["list"].clone()
    })
}

/// 预暖同样留下可重放的请求前/请求后范围；只有确实观测到目标行，预暖才不能被报告口头声明代替。
fn warmup_observation(
    step: usize,
    index: usize,
    baseline: VisibleRange,
    visible: VisibleRange,
    snapshot: &Value,
) -> Value {
    json!({
        "phase": "warmup",
        "step": step,
        "index": index,
        "baselineVisibleStart": baseline.start,
        "baselineVisibleEnd": baseline.end,
        "visibleStart": visible.start,
        "visibleEnd": visible.end,
        "inputMode": INPUT_MODE,
        "list": snapshot["list"].clone()
    })
}

/// 请求前范围必须等于上一观测到的范围；不连续说明中间存在未记录滚动，证据链无法重放。
fn require_contiguous_baseline(
    baseline: VisibleRange,
    previous_visible: VisibleRange,
) -> Result<(), io::Error> {
    if baseline != previous_visible {
        return Err(io::Error::other(format!(
            "请求前可视范围{}..{}与上一观测范围{}..{}不连续",
            baseline.start, baseline.end, previous_visible.start, previous_visible.end
        )));
    }
    Ok(())
}

/// 请求前范围必须来自已就绪列表；未就绪立即显式失败，不能默认为空视口后继续采样。
fn current_range(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    tab: &TabId,
) -> Result<VisibleRange, Box<dyn Error>> {
    Ok(current_evidence(cx, root, window, tab)?.range)
}

/// 读取同一标签当前就绪证据；采样窗起点/终点和请求前范围共用这条路径，
/// 避免两处各自解析导致计数或实例绑定漂移。
fn current_evidence(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    tab: &TabId,
) -> Result<ListEvidence, Box<dyn Error>> {
    let snapshot = window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.plugin_list_performance_snapshot_for_tab(tab, cx)
        })
    })?;
    inspect_source(&snapshot, tab)?;
    match inspect_list(&snapshot)? {
        ListSnapshot::Ready(evidence) => {
            if evidence.total != TOTAL_ROWS {
                return Err(io::Error::other(format!(
                    "列表总行数{}与验收目标{TOTAL_ROWS}不符",
                    evidence.total
                ))
                .into());
            }
            Ok(evidence)
        }
        ListSnapshot::Waiting => {
            Err(io::Error::other(format!("列表在滚动请求前未就绪：{snapshot}")).into())
        }
    }
}

/// 工作区与设置就绪只作为打开前提；列表内容仍由wait_target逐帧验证，不能相互代替。
async fn wait_workspace(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + WAIT;
    loop {
        let isolation = isolation_evidence()?;
        if isolation["isolationProven"] != true {
            return Err(io::Error::other("原生列表验收不在独立桌面").into());
        }
        let snapshot = window.update(cx, |_, _, app| {
            root.update(app, |root, cx| root.plugin_list_performance_snapshot(cx))
        })?;
        if snapshot["workspaceReady"] == true && snapshot["settingsReady"] == true {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待工作区或设置就绪超时：{snapshot}")).into());
        }
        cx.background_executor().timer(POLL).await;
    }
}

/// 只有目标在请求前范围之外、落入请求后新范围、并对应真实构造业务键时才完成；
/// 旧帧继续保持等待，空动作最终超时，类型错误由快照解析立即失败关闭。
async fn wait_target(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    tab: &TabId,
    target: usize,
    baseline: Option<VisibleRange>,
    expected: Option<&ExpectedInstance>,
) -> Result<(Value, ListEvidence), Box<dyn Error>> {
    let deadline = Instant::now() + WAIT;
    loop {
        let isolation = isolation_evidence()?;
        if isolation["isolationProven"] != true {
            return Err(io::Error::other("原生列表验收不在独立桌面").into());
        }
        let snapshot = window.update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.plugin_list_performance_snapshot_for_tab(tab, cx)
            })
        })?;
        // 来源绑定先于列表解析：活动标签被切走或内容提供方不符时立即失败，
        // 不能等到超时后把“采不到目标行”当作唯一诊断。
        inspect_source(&snapshot, tab)?;
        match inspect_list(&snapshot)? {
            ListSnapshot::Waiting => {}
            ListSnapshot::Ready(evidence) => {
                if evidence.total != TOTAL_ROWS {
                    return Err(io::Error::other(format!(
                        "列表总行数{}与验收目标{TOTAL_ROWS}不符",
                        evidence.total
                    ))
                    .into());
                }
                if let Some(expected) = expected
                    && !same_instance(expected, &evidence)
                {
                    return Err(io::Error::other(format!(
                        "列表实例或代次发生变化：期望{}@{}，实际{}@{}",
                        expected.instance,
                        expected.generation,
                        evidence.instance,
                        evidence.generation
                    ))
                    .into());
                }
                let ready = evidence.window_visible
                    && (match baseline {
                        Some(baseline) => target_observed(&evidence, target, baseline),
                        None => {
                            evidence.range.contains(target)
                                && evidence.rows.iter().any(|(index, key)| {
                                    *index == target && key == &format!("record-{target}")
                                })
                        }
                    });
                if ready {
                    return Ok((snapshot, evidence));
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "等待目标业务行{target}在请求后新可视范围内构造超时：{snapshot}"
            ))
            .into());
        }
        cx.background_executor().timer(POLL).await;
    }
}

/// 所有滚动都走生产验收入口；错误保持原类型返回，不能把无效索引伪装成一次空滚动。
fn request_scroll(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    index: usize,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.plugin_list_performance_scroll(index, cx)
        })
    })??;
    Ok(())
}

/// 连续步距使用实际可视跨度：目标至少领先当前范围一整页，极大视口无法继续时明确失败。
fn continuous_target(
    baseline: VisibleRange,
    previous_target: Option<usize>,
    total: usize,
) -> Result<usize, io::Error> {
    let span = baseline
        .end
        .checked_sub(baseline.start)
        .filter(|span| *span > 0)
        .ok_or_else(|| io::Error::other("连续递进需要非空可视范围"))?;
    let target = baseline
        .end
        .checked_add(span)
        .ok_or_else(|| io::Error::other("连续递进目标索引溢出"))?;
    if target >= total {
        return Err(io::Error::other(format!(
            "连续递进目标{target}超出{total}行上限；当前可视跨度{span}行，无法产生可验证的下一页目标"
        )));
    }
    if previous_target.is_some_and(|previous| target <= previous) {
        return Err(io::Error::other(format!(
            "连续递进目标{target}未超过上一目标{previous_target:?}"
        )));
    }
    Ok(target)
}

/// 请求前就拒绝已可见或越界目标，避免把原地滚动或无效索引当作真实位移。
fn validate_request_target(
    baseline: VisibleRange,
    target: usize,
    total: usize,
) -> Result<(), io::Error> {
    if target >= total {
        return Err(io::Error::other(format!("目标{target}超出{total}行列表")));
    }
    if baseline.contains(target) {
        return Err(io::Error::other(format!(
            "目标{target}已在请求前可视范围{}..{}内，无法证明真实位移",
            baseline.start, baseline.end
        )));
    }
    Ok(())
}

/// 同时要求新范围、目标包含关系和真实业务键，三者缺一都不能证明目标行曾被构造。
fn target_observed(evidence: &ListEvidence, target: usize, baseline: VisibleRange) -> bool {
    let expected_key = format!("record-{target}");
    evidence.range != baseline
        && !baseline.contains(target)
        && evidence.range.contains(target)
        && evidence
            .rows
            .iter()
            .any(|(index, key)| *index == target && key == &expected_key)
}

/// 实例与代次必须完全一致；只比较其中一项会把插件重建后的新实例误当成同一轮采样。
fn same_instance(expected: &ExpectedInstance, evidence: &ListEvidence) -> bool {
    expected.instance == evidence.instance && expected.generation == evidence.generation
}

/// 连续阶段的范围必须保持前向且跨度稳定：起点不得回到请求前范围或更早，
/// 允许目标前两行的渲染超采样；跨度低于视口合理下限的一半或差超过两行都失败。
fn validate_continuous_forward(
    baseline: VisibleRange,
    _target: usize,
    visible: VisibleRange,
    minimum_span: usize,
) -> Result<(), io::Error> {
    if visible.start < baseline.end {
        return Err(io::Error::other(format!(
            "连续观测范围{}..{}未越过请求前范围{}..{}，属于回退而非前向",
            visible.start, visible.end, baseline.start, baseline.end
        )));
    }
    // 虚拟列表把目标放到新视口起点之后的一段是正常渲染行为，不以目标靠近起点作为前向判据。
    validate_observed_span(visible, minimum_span)?;
    let baseline_span = baseline.end.saturating_sub(baseline.start);
    let visible_span = visible.end.saturating_sub(visible.start);
    if baseline_span.abs_diff(visible_span) > CONTINUOUS_RANGE_TOLERANCE {
        return Err(io::Error::other(format!(
            "连续观测跨度{visible_span}与请求前跨度{baseline_span}相差超过{CONTINUOUS_RANGE_TOLERANCE}行"
        )));
    }
    Ok(())
}

/// 视口高度除以按字体两倍半行高估算的可见行数，取一半作为观测跨度合理下限；
/// 再与八行绝对下限取大值，防止两行范围被当作完整一页。
fn minimum_observed_span(metadata: &Value) -> Result<usize, io::Error> {
    let height = metadata
        .get("viewport")
        .and_then(|viewport| viewport.get(1))
        .and_then(Value::as_f64)
        .filter(|height| height.is_finite() && *height > 0.0)
        .ok_or_else(|| io::Error::other("列表视口高度无效"))?;
    let font_size = metadata
        .get("fontSize")
        .and_then(Value::as_f64)
        .filter(|font| font.is_finite() && *font > 0.0)
        .ok_or_else(|| io::Error::other("列表字体大小无效"))?;
    let ideal_rows = (height / (font_size * 2.5)).floor().clamp(0.0, 1_000_000.0) as usize;
    let half = ideal_rows / 2;
    Ok(MIN_OBSERVED_SPAN.max(half))
}

/// 单次观测跨度不得低于本轮视口下限；连续和往返阶段共用同一判定。
fn validate_observed_span(visible: VisibleRange, minimum_span: usize) -> Result<(), io::Error> {
    let span = visible.end.saturating_sub(visible.start);
    if span < minimum_span {
        return Err(io::Error::other(format!(
            "观测跨度{span}小于本轮视口合理下限{minimum_span}行"
        )));
    }
    Ok(())
}

/// 来源快照先于列表解析：活动标签、内容注册表和 renderer 身份共同绑定同一个外部插件，
/// 宿主 fixture 或其它插件列表即使拥有相同形状也不能进入证据链。列表尚未出现时返回 None，
/// 由既有等待逻辑处理，而不是把未就绪误报成来源伪造。
fn inspect_source(snapshot: &Value, expected_tab: &TabId) -> Result<Option<Value>, io::Error> {
    if let Some(error) = snapshot.get("bindingError").and_then(Value::as_str) {
        return Err(io::Error::other(format!("列表标签来源绑定失败：{error}")));
    }
    let content = snapshot
        .get("content")
        .filter(|value| value.is_object())
        .ok_or_else(|| io::Error::other("列表快照缺少content来源对象"))?;
    let tab = required_str(content, "tabId", "列表来源")?;
    if tab != expected_tab.as_str() {
        return Err(io::Error::other(format!(
            "列表快照标签{tab}与本轮打开标签{}不符",
            expected_tab.as_str()
        )));
    }
    let active_tab = required_str(content, "activeTabId", "列表来源")?;
    if active_tab != expected_tab.as_str() {
        return Err(io::Error::other(format!(
            "活动标签{active_tab}不是本轮外部内容标签{}",
            expected_tab.as_str()
        )));
    }
    if !required_bool(content, "activeBinding", "列表来源")? {
        return Err(io::Error::other("活动内容作用域未绑定本轮外部内容标签"));
    }
    let content_id = required_str(content, "id", "列表来源")?;
    if content_id == HOST_LIST_CONTENT_ID {
        return Err(io::Error::other("宿主列表fixture不能作为外部插件列表来源"));
    }
    if content_id != LIST_CONTENT_ID {
        return Err(io::Error::other(format!(
            "列表内容{content_id}不是外部插件列表{LIST_CONTENT_ID}"
        )));
    }
    let provider = required_str(content, "provider", "列表来源")?;
    if provider == HOST_LIST_PLUGIN_ID {
        return Err(io::Error::other("宿主列表fixture不能作为外部插件来源"));
    }
    if provider != LIST_PLUGIN_ID {
        return Err(io::Error::other(format!(
            "列表提供方{provider}不是外部插件{LIST_PLUGIN_ID}"
        )));
    }
    let Some(list) = snapshot.get("list") else {
        return Ok(None);
    };
    if list.is_null() {
        return Ok(None);
    }
    if !list.is_object() {
        return Err(io::Error::other("列表快照list字段不是JSON对象"));
    }
    let plugin = required_str(list, "plugin", "列表快照")?;
    let view = required_str(list, "view", "列表快照")?;
    let runtime = required_str(list, "runtime", "列表快照")?;
    if plugin != LIST_PLUGIN_ID || plugin != provider {
        return Err(io::Error::other(format!(
            "列表renderer插件{plugin}与内容提供方{provider}或目标插件{LIST_PLUGIN_ID}不符"
        )));
    }
    if view != LIST_VIEW {
        return Err(io::Error::other(format!(
            "列表renderer视图{view}不是目标视图{LIST_VIEW}"
        )));
    }
    if runtime != LIST_RUNTIME {
        return Err(io::Error::other(format!(
            "列表renderer运行时{runtime}不是外部 Sidecar 运行时{LIST_RUNTIME}"
        )));
    }
    if !required_bool(list, "external", "列表快照")?
        || !required_bool(content, "externalPlugin", "列表来源")?
    {
        return Err(io::Error::other("列表来源未证明来自外部插件"));
    }
    Ok(Some(json!({
        "tabId": tab,
        "activeTabId": active_tab,
        "contentId": content_id,
        "plugin": plugin,
        "view": view,
        "runtime": runtime,
        "externalPlugin": true,
        "activeBindingVerified": true,
    })))
}

/// 快照按强类型逐字段解析；缺失字段表示尚未就绪，类型错误则立即失败关闭。
fn inspect_list(snapshot: &Value) -> Result<ListSnapshot, io::Error> {
    let Some(list) = snapshot.get("list") else {
        return Ok(ListSnapshot::Waiting);
    };
    if list.is_null() {
        return Ok(ListSnapshot::Waiting);
    }
    if !list.is_object() {
        return Err(io::Error::other("列表快照list字段不是JSON对象"));
    }
    let status = required_str(list, "status", "列表快照")?;
    // renderer以Debug格式输出 Failed("原因")，必须按前缀识别并把原因带进错误，
    // 否则真实失败会被误报成未知状态而丢失诊断。
    if status.starts_with("Failed") {
        return Err(io::Error::other(format!(
            "列表renderer进入失败状态：{status}"
        )));
    }
    match status {
        "Ready" => {}
        "Opening" | "Recovering" => return Ok(ListSnapshot::Waiting),
        other => return Err(io::Error::other(format!("未知列表renderer状态：{other}"))),
    }
    // Ready 只描述打开流程；显隐通知或其它异步错误仍可能留在实体上，
    // 有错误就必须连同原文失败，不能继续把列表当作成功样本。
    if let Some(error) = list.get("error").filter(|value| !value.is_null()) {
        let message = error
            .as_str()
            .filter(|text| !text.is_empty())
            .ok_or_else(|| io::Error::other("列表renderer错误字段类型无效"))?;
        return Err(io::Error::other(format!("列表renderer错误：{message}")));
    }
    let instance = required_str(list, "instance", "列表快照")?.to_owned();
    let generation = required_u64(list, "generation", "列表快照")?;
    let window_visible = required_bool(list, "visible", "列表快照")?;
    let Some(collection) = list.get("collection") else {
        return Ok(ListSnapshot::Waiting);
    };
    if collection.is_null() {
        return Ok(ListSnapshot::Waiting);
    }
    if !collection.is_object() {
        return Err(io::Error::other("列表collection字段不是JSON对象"));
    }
    // 分页错误必须进入快照门禁；字符串原文保留，缺失或非字符串/null以外类型都失败关闭。
    let Some(collection_error) = collection.get("error") else {
        return Err(io::Error::other("列表集合error字段缺失"));
    };
    if !collection_error.is_null() {
        let message = collection_error
            .as_str()
            .filter(|text| !text.is_empty())
            .ok_or_else(|| io::Error::other("列表集合error字段类型无效"))?;
        return Err(io::Error::other(format!("列表集合分页错误：{message}")));
    }
    let total = usize::try_from(required_u64(collection, "total", "列表集合")?)
        .map_err(|_| io::Error::other("列表总行数超出平台范围"))?;
    let fetch_requests = required_u64(collection, "fetch_requests", "列表集合")?;
    let fetch_successes = required_u64(collection, "fetch_successes", "列表集合")?;
    let fetch_errors = required_u64(collection, "fetch_errors", "列表集合")?;
    // 成功页会清除可见错误文本，累计错误计数不会；任何一次失败都必须保留诊断并停止采样。
    if fetch_errors > 0 {
        return Err(io::Error::other(format!(
            "列表集合累计{fetch_errors}次分页错误"
        )));
    }
    if fetch_successes > fetch_requests {
        return Err(io::Error::other(format!(
            "列表集合成功{fetch_successes}次多于请求{fetch_requests}次"
        )));
    }
    let start = usize::try_from(required_u64(collection, "visible_start", "列表集合")?)
        .map_err(|_| io::Error::other("可视起始索引超出平台范围"))?;
    let end = usize::try_from(required_u64(collection, "visible_end", "列表集合")?)
        .map_err(|_| io::Error::other("可视结束索引超出平台范围"))?;
    let rows = collection
        .get("visible_rows")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other("列表集合visible_rows缺失或类型错误"))?;
    let mut rendered = Vec::with_capacity(rows.len());
    for row in rows {
        if !row.is_object() {
            return Err(io::Error::other("可视行业务键不是JSON对象"));
        }
        let index = usize::try_from(required_u64(row, "index", "可视行")?)
            .map_err(|_| io::Error::other("可视行索引超出平台范围"))?;
        let key = required_str(row, "key", "可视行")?.to_owned();
        rendered.push((index, key));
    }
    // 首屏 Ready 早于分页返回时集合仍是 total=0、0..40 的占位形状；
    // 它不是错误，也不能作为目标证据，必须继续等待真实总数和首批业务行出现。
    if total == 0 && rendered.is_empty() {
        return Ok(ListSnapshot::Waiting);
    }
    if total == 0 {
        return Err(io::Error::other("列表总行数为零却已构造可视业务行"));
    }
    if start >= end {
        return Ok(ListSnapshot::Waiting);
    }
    if end > total {
        return Err(io::Error::other(format!(
            "可视范围{start}..{end}超出列表总行数{total}"
        )));
    }
    Ok(ListSnapshot::Ready(ListEvidence {
        instance,
        generation,
        window_visible,
        total,
        range: VisibleRange { start, end },
        rows: rendered,
        fetch_requests,
        fetch_successes,
        fetch_errors,
    }))
}

/// 字段缺失或类型不符都返回错误；PowerShell侧会得到非零退出和stderr，而不是静默零值。
fn required_u64(value: &Value, key: &str, context: &str) -> Result<u64, io::Error> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| io::Error::other(format!("{context}字段{key}缺失或类型错误")))
}

/// 只接受非空字符串；空实例或空业务键不能作为身份证据。
fn required_str<'a>(value: &'a Value, key: &str, context: &str) -> Result<&'a str, io::Error> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| io::Error::other(format!("{context}字段{key}缺失或类型错误")))
}

/// 布尔字段必须保持JSON布尔类型，字符串"true"不能冒充可见状态。
fn required_bool(value: &Value, key: &str, context: &str) -> Result<bool, io::Error> {
    value
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| io::Error::other(format!("{context}字段{key}缺失或类型错误")))
}

/// 采样窗两端各取一次系统墙钟，仅与单调时钟长度交叉核对；不据此推导物理显示时刻。
fn unix_ms() -> Result<u64, io::Error> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| io::Error::other(format!("系统时钟早于Unix纪元：{error}")))?
        .as_millis();
    u64::try_from(millis).map_err(|_| io::Error::other("Unix毫秒超出u64"))
}

/// 原始事件只保留当前窗口，时戳使用同一个单调时钟原点，不映射成物理显示时刻。
fn drain_frames(
    collector: &mut FrameTimingCollector,
    window_id: gpui_kit::WindowId,
    origin: Instant,
    raw: &mut Vec<Value>,
) {
    for event in collector.collect_unseen() {
        match event {
            FrameEvent::Draw(timing) if timing.window_id==window_id => raw.push(json!({"kind":"draw","startNs":ns(timing.draw_start.saturating_duration_since(origin)),"endNs":ns(timing.draw_end.saturating_duration_since(origin)),"durationNs":ns(timing.draw_duration()),"dirtyAtNs":timing.dirty_at.map(|at|ns(at.saturating_duration_since(origin))),"signalAtNs":timing.signal_at.map(|at|ns(at.saturating_duration_since(origin))),"invalidations":timing.invalidations,"refreshIntervalNs":timing.refresh_interval.map(ns),"requestLayoutNs":ns(timing.phases.request_layout),"prepaintNs":ns(timing.phases.prepaint),"paintNs":ns(timing.phases.paint)})),
            FrameEvent::Present(timing) if timing.window_id==window_id => raw.push(json!({"kind":"present","startNs":ns(timing.present_start.saturating_duration_since(origin)),"endNs":ns(timing.present_end.saturating_duration_since(origin)),"durationNs":ns(timing.present_duration()),"animationIntervalNs":timing.animation_interval.map(ns)})),
            _=>{}
        }
    }
}

/// 固定单位且显式饱和，避免平台usize宽度改变原始数据格式。
fn ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// 私有桌面证据来自Win32真实线程归属；其他平台不伪造通过。
fn isolation_evidence() -> Result<Value, io::Error> {
    #[cfg(windows)]
    {
        serde_json::to_value(isolation::capture()?).map_err(io::Error::other)
    }
    #[cfg(not(windows))]
    {
        Err(io::Error::other("当前原生列表采样启动器只支持Windows"))
    }
}

#[cfg(test)]
#[path = "../tests/support/plugin_list_performance_acceptance_unit.rs"]
mod tests;
