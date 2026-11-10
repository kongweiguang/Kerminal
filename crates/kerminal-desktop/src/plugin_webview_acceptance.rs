//! 正式 File Search 工厂、网页桥接和 Sidecar 的后台原生验收。
//! @author kongweiguang

use crate::{
    bootstrap,
    composition::DesktopComposition,
    plugin_acceptance::{draw, viewport_evidence},
    plugins::{arguments::PluginStartupOptions, startup::StartupPlugins},
    runtime::ApplicationRuntime,
};
use gpui_kit::{
    AnyWindowHandle, AppContext as _, AsyncApp, Entity, WindowOptions, component::TitleBar,
};
use gpui_pre::{Bounds, WindowBounds, px, size};
use kerminal_application::{external_contributions::PreparedPlugin, preferences::ThemeMode};
use kerminal_plugin_protocol::{ManifestPolicy, Platform, PluginRuntime, ViewDefinition};
use kerminal_ui::{AppServices, QuitIntent, RootView, RootViewEvent};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    error::Error,
    ffi::OsString,
    fmt, fs,
    io::{self, Read as _},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[cfg(windows)]
#[allow(dead_code)]
#[path = "right_rail_acceptance/isolation.rs"]
mod isolation;
#[cfg(windows)]
#[path = "webview_probe/pixels.rs"]
mod pixels;
#[cfg(windows)]
#[path = "webview_probe/wgc.rs"]
mod wgc;

const CONTENT: &str = "kerminal.file-search/search-tab";
const WAIT: Duration = Duration::from_secs(45);
const POLL: Duration = Duration::from_millis(70);

thread_local! {
    // 验收在同一 UI 线程顺序执行；保存最近一次已观察状态，不为失败诊断引入磁盘 I/O。
    static LAST_SNAPSHOT: RefCell<Option<Value>> = const { RefCell::new(None) };
    // 后续阶段必须与首次 UI 线程、输入桌面和进程基线一致，不能只比较各次瞬时值。
    static ISOLATION_BASELINE: RefCell<Option<Value>> = const { RefCell::new(None) };
}

#[derive(Clone)]
struct Options {
    output: PathBuf,
    plugin: PathBuf,
    theme: String,
    desktop: String,
    candidate_hash: String,
}

/// 只在全新隔离根装配正式服务；可执行文件、资源和 Sidecar 摘要一起固定到报告。
pub(super) fn run() -> Result<(), Box<dyn Error>> {
    let options = options(std::env::args_os().skip(2))?;
    let hash = digest(&fs::read(std::env::current_exe()?)?);
    if hash != options.candidate_hash {
        return Err(io::Error::other("网页验收可执行文件摘要与启动器不一致").into());
    }
    fs::create_dir(&options.output)?;
    let config = options.output.join("config");
    fs::create_dir(&config)?;
    // 验收根使用当前格式；避免空根首次迁移在隔离环境产生 journal 竞态。
    fs::write(
        config.join(".kerminal-format.toml"),
        b"data_format = 4
migration_contract = 4
",
    )?;
    // SAFETY: 本入口尚未建立运行时或 UI 线程，只设置候选进程自己的隔离目录。
    unsafe {
        std::env::set_var("KERMINAL_CONFIG_ROOT", &config);
        std::env::set_var(
            "WEBVIEW2_USER_DATA_FOLDER",
            options.output.join("webview-profile"),
        );
    }
    let fixture = options.output.join("search-fixture");
    fs::create_dir(&fixture)?;
    for directory in 0..2_000 {
        let path = fixture.join(format!("folder-{directory:04}"));
        fs::create_dir(&path)?;
        for file in 0..10 {
            fs::write(path.join(format!("item-{file:02}.txt")), b"acceptance")?;
        }
    }
    for file in 0..120 {
        fs::write(fixture.join(format!("needle-{file:03}.txt")), b"acceptance")?;
    }
    let runtime = ApplicationRuntime::new()?;
    let primary =
        match crate::acquire_single_instance(&runtime, &crate::single_instance_id(&config)?)? {
            single_application::SingleApplication::PrimaryInstance(primary) => primary,
            _ => return Err(io::Error::other("验收根已有运行实例").into()),
        };
    let startup = Arc::new(StartupPlugins::load(
        &config,
        PluginStartupOptions {
            safe_mode: false,
            dev_plugin: Some(options.plugin.clone()),
        },
    )?);
    let prepared = startup
        .enabled
        .iter()
        .find(|plugin| plugin.id.as_str() == "kerminal.file-search")
        .ok_or_else(|| io::Error::other("未加载真实 File Search 包"))?;
    if startup.enabled.len() != 1 {
        return Err(io::Error::other("网页验收只允许目标 File Search 包").into());
    }
    let (mut identity, _sidecar_guard) = plugin_identity(prepared)?;
    identity["processId"] = json!(std::process::id());
    identity["candidateSha256"] = json!(hash);
    identity["theme"] = json!(options.theme);
    fs::write(
        options.output.join("plugin-identity.json"),
        serde_json::to_vec_pretty(&identity)?,
    )?;
    let catalogs = startup.catalogs("zh-CN")?;
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
    // 安装失败也经过同一有界收尾，避免监督器在返回错误时留下已启动的插件子进程。
    let (ui, ui_deadline): (Result<Value, Box<dyn Error>>, Option<Instant>) =
        match runtime.block_on(composition.install_external_plugins(startup)) {
            Ok(()) => run_window(composition.app_services(), &options, &fixture),
            Err(error) => (Err(error.into()), None),
        };
    // 即使视觉证据未证明，也沿用已发出的生产退出截止时间；只有根本没有退出回执时才使用兜底时限。
    let deadline = ui_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(12));
    let shutdown = runtime.block_on(composition.shutdown_until(deadline.into()));
    drop(primary);
    runtime.shutdown();
    // 退出回执缺失本身必须是失败，但仍先完成同一截止时间的资源关闭并与关闭错误聚合。
    let ui = require_quit_receipt(ui, ui_deadline);
    let mut report = combine_ui_shutdown(ui, shutdown)?;
    report["candidateSha256"] = json!(hash);
    report["pluginSha256"] = identity["pluginSha256"].clone();
    report["sidecarSha256"] = identity["sidecarSha256"].clone();
    report["sidecarMain"] = identity["sidecarMain"].clone();
    report["sidecarPlatform"] = identity["sidecarPlatform"].clone();
    report["sidecarRuntimeBinding"] = json!("not-proven");
    report["quitReceipt"] = json!(true);
    // shutdown_until 只证明组合根资源完成收尾；ApplicationRuntime::shutdown 的超时排空结果不可观察。
    report["resourcesShutdownComplete"] = json!(true);
    report["runtimeShutdownObserved"] = json!(false);
    report["shutdownScope"] = json!("composition-resources-only");
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
            return Err(io::Error::other("未知网页插件验收选项"));
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
    let desktop = std::env::var("KERMINAL_PLUGIN_WEBVIEW_DESKTOP")
        .map_err(|_| io::Error::other("缺少本轮私有桌面身份"))?;
    let candidate_hash = std::env::var("KERMINAL_PLUGIN_WEBVIEW_CANDIDATE_SHA256")
        .map_err(|_| io::Error::other("缺少本轮候选摘要"))?;
    let valid_hex = |value: &str, length: usize| {
        value.len() == length
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    if !output.is_absolute()
        || output.exists()
        || !["light", "dark", "system"].contains(&theme.as_str())
        || !desktop
            .strip_prefix("KerminalRail-settings-plugin-webview-")
            .is_some_and(|suffix| valid_hex(suffix, 32))
        || !valid_hex(&candidate_hash, 64)
    {
        return Err(io::Error::other("验收根、主题或本轮身份无效"));
    }
    Ok(Options {
        output,
        plugin,
        theme,
        desktop,
        candidate_hash,
    })
}

/// 摘要固定真实文件字节，文件名或版本不能代替候选身份。
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// 与生产进程装配使用相同平台映射，缺失或非 Sidecar 不得回退到猜测的文件名。
fn sidecar_entry(runtime: &PluginRuntime, platform: Platform) -> Result<&str, io::Error> {
    match runtime {
        PluginRuntime::Sidecar { main, .. } => main
            .get(&platform)
            .map(String::as_str)
            .ok_or_else(|| io::Error::other("目标平台未声明 Sidecar 入口")),
        _ => Err(io::Error::other("文件搜索验收要求 Sidecar 运行时")),
    }
}

/// 与基础设施资源快照使用同一扩展名集合；清单资源缺失时验收必须失败而不是缩小检查。
fn is_webview_resource_path(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, extension)| {
        matches!(
            extension.to_ascii_lowercase().as_str(),
            "html"
                | "htm"
                | "js"
                | "mjs"
                | "css"
                | "json"
                | "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
                | "ico"
                | "svg"
                | "woff"
                | "woff2"
                | "ttf"
                | "otf"
        )
    })
}

/// 原生视图图标引用藏在节点值中；验收只收集冻结清单里真实存在的路径。
fn collect_frozen_icons(
    node: &Value,
    files: &BTreeMap<String, kerminal_plugin_protocol::PackageFile>,
    paths: &mut BTreeSet<String>,
) {
    match node {
        Value::Object(map) => {
            if let Some(icon) = map.get("icon").and_then(Value::as_str)
                && files.contains_key(icon)
            {
                paths.insert(icon.to_owned());
            }
            for child in map.values() {
                collect_frozen_icons(child, files, paths);
            }
        }
        Value::Array(array) => {
            for child in array {
                collect_frozen_icons(child, files, paths);
            }
        }
        _ => {}
    }
}

/// 从冻结包清单推导 UI 资源完整集合，防止候选把不完整的 `prepared.resources` 当作全部资源。
fn expected_frozen_resources(prepared: &PreparedPlugin) -> BTreeSet<String> {
    let files = &prepared.package.files;
    let manifest = &prepared.package.manifest;
    let mut paths = BTreeSet::new();
    if let Some(icon) = &manifest.icon
        && files.contains_key(icon)
    {
        paths.insert(icon.clone());
    }
    for tool in &manifest.contributes.tools {
        if let Some(icon) = &tool.icon
            && files.contains_key(icon)
        {
            paths.insert(icon.clone());
        }
    }
    for view in prepared.native_views.values() {
        collect_frozen_icons(view.as_ref(), files, &mut paths);
    }
    if manifest
        .views
        .values()
        .any(|view| matches!(view, ViewDefinition::Webview { .. }))
    {
        paths.extend(
            files
                .keys()
                .filter(|path| is_webview_resource_path(path.as_str()))
                .cloned(),
        );
    }
    paths
}

/// 身份摘要必须使用启动阶段冻结的资源字节；磁盘回读只作为脚本的时点一致性复核。
fn plugin_identity(prepared: &PreparedPlugin) -> Result<(Value, fs::File), Box<dyn Error>> {
    let platform = crate::plugins::startup::current_platform()?;
    let main = sidecar_entry(&prepared.package.manifest.runtime, platform)?;
    let expected_size = prepared
        .package
        .files
        .get(main)
        .ok_or_else(|| io::Error::other("Sidecar 入口不在已校验的包内"))?
        .size;
    let path = prepared.directory.join(main);
    if !path
        .canonicalize()?
        .starts_with(prepared.directory.canonicalize()?)
    {
        return Err(io::Error::other("Sidecar 入口越过插件目录").into());
    }
    #[allow(unused_mut)]
    let mut open = fs::OpenOptions::new();
    open.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // 与运行时同用该路径；拒绝写入和删除共享，避免先哈希后替换入口文件。
        open.share_mode(0x0000_0001).custom_flags(0x0020_0000);
    }
    let mut sidecar = open.open(path)?;
    let metadata = sidecar.metadata()?;
    if !metadata.is_file() || metadata.len() != expected_size {
        return Err(io::Error::other("Sidecar 文件类型或大小与冻结清单不一致").into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x0000_0400 != 0 {
            return Err(io::Error::other("Sidecar 入口不能为重解析点").into());
        }
    }
    let mut sidecar_bytes = Vec::new();
    sidecar.read_to_end(&mut sidecar_bytes)?;
    if sidecar_bytes.len() as u64 != expected_size {
        return Err(io::Error::other("Sidecar 文件读取长度发生变化").into());
    }
    let manifest_name = kerminal_plugin_protocol::MANIFEST_FILE_NAME;
    let manifest_file = prepared
        .package
        .files
        .get(manifest_name)
        .ok_or_else(|| io::Error::other("冻结清单不在已校验包清点内"))?;
    // WebView 宿主会把包内 json 一并冻结，因此清单原文必须从 `prepared.resources` 取，
    // 不能在启动冻结后再次读取磁盘，否则 permissions/views 变化会被误称为实际运行清单。
    let manifest_bytes = prepared
        .resources
        .get(manifest_name)
        .ok_or_else(|| io::Error::other("WebView 冻结资源缺少清单原文"))?;
    if manifest_bytes.len() as u64 != manifest_file.size {
        return Err(io::Error::other("冻结清单资源大小与包清点不一致").into());
    }
    let manifest_policy = if prepared.id.as_str().starts_with("kerminal.") {
        ManifestPolicy::Official
    } else {
        ManifestPolicy::ThirdParty
    };
    let frozen_manifest =
        kerminal_plugin_protocol::parse_manifest(manifest_bytes.as_ref(), manifest_policy)
            .map_err(|source| io::Error::other(format!("冻结清单无法重新解析：{source}")))?;
    if frozen_manifest != prepared.package.manifest {
        return Err(io::Error::other("冻结清单资源与已解析清单语义不一致").into());
    }
    let expected_resources = expected_frozen_resources(prepared);
    let actual_resources = prepared.resources.keys().cloned().collect::<BTreeSet<_>>();
    if actual_resources != expected_resources {
        return Err(io::Error::other("冻结 UI 资源集合与包清单推导结果不一致").into());
    }
    let manifest_hash = Sha256::digest(manifest_bytes.as_ref());
    let mut plugin_hash = Sha256::new();
    plugin_hash.update(b"kerminal-webview-plugin-v1\0");
    plugin_hash.update(&manifest_hash[..]);
    let mut resources = Vec::new();
    for (path, bytes) in &prepared.resources {
        let resource_hash = Sha256::digest(bytes.as_ref());
        plugin_hash.update((path.len() as u64).to_le_bytes());
        plugin_hash.update(path.as_bytes());
        plugin_hash.update((bytes.len() as u64).to_le_bytes());
        plugin_hash.update(&resource_hash[..]);
        resources
            .push(json!({"path":path,"size":bytes.len(),"sha256":format!("{resource_hash:x}")}));
    }
    let package_files = prepared
        .package
        .files
        .values()
        .map(|file| json!({"path":&file.path,"size":file.size}))
        .collect::<Vec<_>>();
    Ok((
        json!({
            "schemaVersion": 1,
            "algorithm": "kerminal-webview-plugin-v1",
            "pluginId": prepared.id.as_str(),
            "pluginVersion": prepared.version.to_string(),
            "manifestSha256": format!("{manifest_hash:x}"),
            "manifestSource": "prepared.resources",
            "manifestSemanticsVerified": true,
            "pluginSha256": format!("{:x}", plugin_hash.finalize()),
            "resourceSetComplete": true,
            "resourceSetDerivation": "package-files+manifest",
            "sidecarPlatform": platform,
            "sidecarMain": main,
            "sidecarSha256": digest(&sidecar_bytes),
            "sidecarEvidence": "package-file-only",
            "sidecarProcessBound": false,
            "packageFiles": package_files,
            "resources": resources,
        }),
        sidecar,
    ))
}

#[derive(Debug)]
struct UiAndShutdownFailure {
    ui: Box<dyn Error>,
    shutdown: kerminal_application::AppError,
}

impl fmt::Display for UiAndShutdownFailure {
    /// 同时暴露交互失败和关闭失败，避免前者掩盖尚未回收的资源。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "界面或安装失败：{}；资源关闭也失败：{}",
            self.ui, self.shutdown
        )
    }
}

impl Error for UiAndShutdownFailure {
    /// 主错误链保留原始交互错误，关闭错误仍作为类型化字段保留。
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.ui.as_ref())
    }
}

/// 两个阶段均结束后再决议结果，单独或同时失败都不能写出关闭成功回执。
fn combine_ui_shutdown<T>(
    ui: Result<T, Box<dyn Error>>,
    shutdown: kerminal_application::AppResult<()>,
) -> Result<T, Box<dyn Error>> {
    match (ui, shutdown) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(Box::new(error)),
        (Err(ui), Err(shutdown)) => Err(Box::new(UiAndShutdownFailure { ui, shutdown })),
    }
}

#[derive(Debug)]
struct MissingQuitReady;

impl fmt::Display for MissingQuitReady {
    /// 退出截止时间是资源关闭的共同起点；缺失回执不能继续冒充成功路径。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("未收到生产退出截止时间回执，无法确认资源关闭使用了生产截止时间")
    }
}

impl Error for MissingQuitReady {}

#[derive(Debug)]
struct UiAndQuitReceiptFailure {
    ui: Box<dyn Error>,
}

impl fmt::Display for UiAndQuitReceiptFailure {
    /// 同时保留网页验收错误和缺失退出回执，避免一个真实失败被另一个失败覆盖。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "网页验收失败：{}；同时未收到生产退出截止时间回执",
            self.ui
        )
    }
}

impl Error for UiAndQuitReceiptFailure {
    /// 主错误链保留原始网页验收错误，退出回执缺失仍由显示文本明确暴露。
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.ui.as_ref())
    }
}

/// 退出回执与界面结果分开决议；即使关闭已经完成，缺失回执也不能写出成功报告。
fn require_quit_receipt<T>(
    ui: Result<T, Box<dyn Error>>,
    deadline: Option<Instant>,
) -> Result<T, Box<dyn Error>> {
    match (ui, deadline) {
        (Ok(value), Some(_)) => Ok(value),
        (Ok(_), None) => Err(Box::new(MissingQuitReady)),
        (Err(error), Some(_)) => Err(error),
        (Err(ui), None) => Err(Box::new(UiAndQuitReceiptFailure { ui })),
    }
}

/// 后台窗口沿普通 Root 装配；失败结果与已收到或缺失的退出截止时间分别返回，由组合根决议并收尾。
fn run_window(
    services: AppServices,
    options: &Options,
    fixture: &Path,
) -> (Result<Value, Box<dyn Error>>, Option<Instant>) {
    let options = options.clone();
    let fixture = fixture.to_path_buf();
    let outcome = Arc::new(Mutex::new(None::<Result<Value, String>>));
    let retained = outcome.clone();
    let deadline = Arc::new(Mutex::new(None::<Instant>));
    let retained_deadline = deadline.clone();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            // 在创建任何窗口之前检查 UI 线程，默认桌面或无证明时不能继续装配。
            if let Err(error) = background_safety("创建窗口前") {
                *outcome.lock().unwrap_or_else(|error| error.into_inner()) =
                    Some(Err(error.to_string()));
                cx.quit();
                return;
            }
            kerminal_ui::init(cx);
            let options = options.clone();
            let fixture = fixture.clone();
            let completed = outcome.clone();
            let closing = deadline.clone();
            let result = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: gpui_kit::point(px(40.), px(40.)),
                        size: size(px(1024.), px(720.)),
                    })),
                    show: false,
                    focus: false,
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
                        let result = exercise(cx, &owned, window, &options, &fixture).await;
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
    let result = retained
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .ok_or_else(|| Box::<dyn Error>::from(io::Error::other("网页验收未返回")))
        .and_then(|result| result.map_err(|error| Box::<dyn Error>::from(io::Error::other(error))));
    let deadline = retained_deadline
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    (result, deadline)
}

/// DOM 交互触发示例的真实监听器；观察流式更新、取消、页面事件、遮挡恢复、代次及关闭。
async fn exercise(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    options: &Options,
    fixture: &Path,
) -> Result<Value, Box<dyn Error>> {
    LAST_SNAPSHOT.with(|last| *last.borrow_mut() = None);
    isolation_checkpoint(cx, options, "start").await?;
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.enable_right_rail_acceptance_activity(cx)
        })
    })?;
    wait(cx, root, window, "工作区", |v| {
        v["workspaceReady"] == true && v["settingsReady"] == true && v["registeredContent"] == true
    })
    .await?;
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
            root.plugin_webview_acceptance_open(window, cx)
        })
    })??;
    let mounted = wait(cx, root, window, "真实网页承载", |v| {
        renderer(v)["native"]["native_parent_id"]
            .as_u64()
            .is_some_and(|id| id != 0)
    })
    .await?;
    let parent = renderer(&mounted)["native"]["native_parent_id"]
        .as_u64()
        .ok_or_else(|| io::Error::other("缺少网页父窗"))?;
    #[cfg(windows)]
    let scale = window.update(cx, |_, window, _| window.scale_factor())?;
    #[cfg(windows)]
    show_background(parent, scale)?;
    let ready = wait(cx, root, window, "网页与Sidecar握手", |v| {
        renderer(v)["pageReady"] == true
            && renderer(v)["opened"] == true
            && renderer(v)["native"]["visible"] == true
    })
    .await?;
    let instance = renderer(&ready)["instance"].clone();
    let native = renderer(&ready)["native"]["native_id"].clone();
    let setup = r#"window.__kaTrace=[];new MutationObserver(()=>window.__kaTrace.push({rows:document.querySelectorAll('#results li').length,busy:!document.querySelector('#cancel').disabled})).observe(document.querySelector('#results'),{childList:true});const proof=document.createElement('div');proof.id='acceptance-proof';proof.style.cssText='position:fixed;right:4px;bottom:4px;width:160px;height:64px;background:rgb(18,52,86);pointer-events:none';const chip=document.createElement('div');chip.style.cssText='width:40px;height:40px;background:rgb(37,207,120)';proof.append(chip);document.body.append(proof);"#;
    evaluate(cx, root, window, setup)?;
    let fixture_json = serde_json::to_string(&fixture.to_string_lossy())?;
    evaluate(
        cx,
        root,
        window,
        &format!(
            "const r=document.querySelector('#root'),q=document.querySelector('#query');r.value={fixture_json};q.value='needle';r.dispatchEvent(new Event('input',{{bubbles:true}}));q.dispatchEvent(new Event('input',{{bubbles:true}}));document.querySelector('#search').click();"
        ),
    )?;
    let searched = dom_wait(cx, root, window, "流式文件结果", |dom| {
        dom["rows"].as_array().is_some_and(|rows| rows.len() == 120)
            && dom["busy"] == false
            && dom["trace"].as_array().is_some_and(|trace| {
                trace.iter().any(|event| {
                    event["busy"] == true
                        && event["rows"]
                            .as_u64()
                            .is_some_and(|rows| rows > 0 && rows < 120)
                })
            })
    })
    .await?;
    if !matches!(
        capture(cx, window, parent, options, "search").await?,
        FrameProof::SearchMarker
    ) {
        return Err(io::Error::other("网页标记阶段未完成独立校验").into());
    }
    evaluate(
        cx,
        root,
        window,
        "document.querySelector('#query').value='no-such-needle-for-cancel';document.querySelector('#search').click();setTimeout(()=>document.querySelector('#cancel').click(),5);",
    )?;
    let cancelled = dom_wait(cx, root, window, "真实桥接取消", |dom| {
        dom["busy"] == false
            && dom["status"]
                .as_str()
                .is_some_and(|status| status.contains("取消") || status.contains("cancelled"))
    })
    .await?;
    evaluate(
        cx,
        root,
        window,
        "window.__kaEvents=[];window.kerminal.onRequest('event',params=>{window.__kaEvents.push(params);});window.kerminal.events.subscribe({names:['tab.changed']}).then(()=>window.__kaSubscribed=true,error=>window.__kaSubscribeError=String(error));",
    )?;
    dom_wait(cx, root, window, "页面事件订阅回执", |dom| {
        dom["subscribed"] == true
    })
    .await?;
    let second = window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.plugin_webview_acceptance_open(window, cx)
        })
    })??;
    wait(cx, root, window, "第二网页实例", |value| {
        value["activeTabId"] == second.as_str()
            && renderer(value)["pageReady"] == true
            && renderer(value)["opened"] == true
    })
    .await?;
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.plugin_acceptance_tab_action(tab.clone(), false, window, cx)
        })
    })?;
    wait(cx, root, window, "返回原网页实例", |value| {
        value["activeTabId"] == tab.as_str() && renderer(value)["instance"] == instance
    })
    .await?;
    let page_events = dom_wait(cx, root, window, "隐藏页面真实标签事件", |dom| {
        dom["events"].as_array().is_some_and(|events| {
            events.iter().any(|event| {
                event["name"] == "tab.changed" && event["payload"]["tab"] == second.as_str()
            })
        })
    })
    .await?;
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.plugin_acceptance_tab_action(second.clone(), true, window, cx)
        })
    })?;
    // 内容关闭是异步资源回收；通知阶段必须落在唯一剩余的真实标签上，不能把 Closing 代次写进报告。
    wait(cx, root, window, "第二网页实例关闭", |value| {
        value["tabs"]
            .as_array()
            .is_some_and(|tabs| tabs.iter().all(|item| item["tabId"] != second.as_str()))
            && value["activeTabId"] == tab.as_str()
    })
    .await?;
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.plugin_webview_acceptance_show_notification(window, cx)
        })
    })?;
    let notified = wait(cx, root, window, "正式通知遮挡", |value| {
        renderer(value)["native"]["visible"] == false
            && renderer(value)["native"]["backdrop_pending"] == false
    })
    .await?;
    match capture(cx, window, parent, options, "notification").await? {
        FrameProof::OccludedState => {}
        FrameProof::SearchMarker => return Err(io::Error::other("通知误用网页标记判定").into()),
    }
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.plugin_webview_acceptance_clear_notification(window, cx)
        })
    })?;
    let notification_restored = wait(cx, root, window, "通知卸载后恢复", |value| {
        renderer(value)["native"]["visible"] == true
            && renderer(value)["instance"] == instance
            && renderer(value)["native"]["native_id"] == native
    })
    .await?;
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.plugin_webview_acceptance_open_preferences(window, cx)
        })
    })?;
    let occluded = wait(cx, root, window, "真实设置遮挡", |v| {
        v["preferencesOpen"] == true
            && renderer(v)["native"]["visible"] == false
            && renderer(v)["native"]["backdrop_pending"] == false
    })
    .await?;
    match capture(cx, window, parent, options, "dialog").await? {
        FrameProof::OccludedState => {}
        FrameProof::SearchMarker => return Err(io::Error::other("设置误用网页标记判定").into()),
    }
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.plugin_webview_acceptance_close_preferences(cx)
        })
    })?;
    let restored = wait(cx, root, window, "同实例恢复", |v| {
        v["preferencesOpen"] == false
            && renderer(v)["native"]["visible"] == true
            && renderer(v)["instance"] == instance
            && renderer(v)["native"]["native_id"] == native
    })
    .await?;
    let draft = dom_wait(cx, root, window, "遮挡后草稿", |dom| {
        dom["query"] == "no-such-needle-for-cancel"
            && dom["root"] == fixture.to_string_lossy().as_ref()
    })
    .await?;
    let old_epoch = renderer(&restored)["pageEpoch"]
        .as_u64()
        .ok_or_else(|| io::Error::other("缺少页面代次"))?;
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.plugin_webview_acceptance_reload(window, cx)
        })
    })??;
    let reloaded = wait(cx, root, window, "新页面桥接", |v| {
        renderer(v)["pageEpoch"]
            .as_u64()
            .is_some_and(|epoch| epoch > old_epoch)
            && renderer(v)["pageReady"] == true
            && renderer(v)["instance"] == instance
    })
    .await?;
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.plugin_acceptance_tab_action(tab.clone(), true, window, cx)
        })
    })?;
    let closed = wait(cx, root, window, "关闭正式内容实例", |v| {
        v["tabs"].as_array().is_some_and(|tabs| tabs.is_empty())
            && v["activeTabId"].is_null()
            && v["centralContent"] == false
    })
    .await?;
    let (viewport, scale) =
        window.update(cx, |_, window, _| viewport_evidence(window, [1024, 720]))??;
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| root.request_quit(QuitIntent::Exit, cx))
    })?;
    wait(cx, root, window, "生产退出", |v| v["quitReady"] == true).await?;
    isolation_checkpoint(cx, options, "complete").await?;
    Ok(
        json!({"schemaVersion":1,"processId":std::process::id(),"theme":options.theme,"mode":"private-desktop-composition","nativeIsolation":background_safety("最终报告")?,"viewport":viewport,"scaleFactor":scale,"ready":ready,"searched":searched,"cancelled":cancelled,"pageEvents":page_events,"notificationOccluded":notified,"notificationRestored":notification_restored,"occluded":occluded,"restored":restored,"draftAfterOcclusion":draft,"reloaded":reloaded,"closed":closed,"occlusionEvidence":"production-root-state-and-gpui-offscreen","systemComposition":"not-observed","offscreenFrames":["search.png","notification.png","dialog.png"],"nativeImeTested":false,"dpiSwitchTested":false,"sleepResumeTested":false,"inputMode":"DOM events through production page listeners and SDK bridge"}),
    )
}

/// 活动标签身份来自工作区，隐藏实例不能满足当前页面的等待。
fn renderer(snapshot: &Value) -> &Value {
    snapshot["tabs"]
        .as_array()
        .and_then(|tabs| {
            tabs.iter()
                .find(|tab| tab["tabId"] == snapshot["activeTabId"] && tab["contentId"] == CONTENT)
        })
        .map(|tab| &tab["renderer"])
        .unwrap_or(&Value::Null)
}

/// 每轮推进真实绘制并检查前台归属，超时保留末次状态而不伪造成功。
async fn wait(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    stage: &str,
    mut predicate: impl FnMut(&Value) -> bool,
) -> Result<Value, Box<dyn Error>> {
    let deadline = Instant::now() + WAIT;
    loop {
        background_safety(stage)?;
        let snapshot = window.update(cx, |_, window, app| {
            draw(window, app);
            root.update(app, |root, cx| {
                let mut value = root.plugin_webview_acceptance_snapshot(window, cx);
                value["quitReady"] = json!(
                    root.transfer_workspace_acceptance_quit_ready()
                        .map_err(io::Error::other)?
                );
                Ok::<_, io::Error>(value)
            })
        })??;
        LAST_SNAPSHOT.with(|last| *last.borrow_mut() = Some(snapshot.clone()));
        if predicate(&snapshot) {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待{stage}超时：{snapshot}")).into());
        }
        cx.background_executor().timer(POLL).await;
    }
}

/// 注入只定位当前页面，report回执仍经拥有该页面代次的真实原生IPC返回。
fn evaluate(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    script: &str,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.plugin_webview_acceptance_evaluate(script, cx)
        })
    })??;
    Ok(())
}

/// 唯一序列绑定每次DOM读取，不能用旧的成功快照越过后续动作。
async fn dom_wait(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    stage: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Result<Value, Box<dyn Error>> {
    let deadline = Instant::now() + WAIT;
    let mut sequence = 0u64;
    loop {
        sequence += 1;
        let token = format!("{stage}:{sequence}");
        let script = format!(
            "report({{token:{},root:document.querySelector('#root').value,query:document.querySelector('#query').value,status:document.querySelector('#status').textContent,busy:!document.querySelector('#cancel').disabled,rows:[...document.querySelectorAll('#results li')].map(x=>x.textContent),trace:window.__kaTrace||[],subscribed:window.__kaSubscribed||false,subscribeError:window.__kaSubscribeError||null,events:window.__kaEvents||[]}});",
            serde_json::to_string(&token)?
        );
        evaluate(cx, root, window, &script)?;
        let snapshot = wait(cx, root, window, stage, |v| {
            renderer(v)["dom"]["token"] == token
        })
        .await?;
        let dom = renderer(&snapshot)["dom"].clone();
        if predicate(&dom) {
            return Ok(dom);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待{stage}超时：{dom}")).into());
        }
        cx.background_executor().timer(POLL).await;
    }
}

/// 统计 GPUI 调用平台绘制的尝试；Windows 可能跳过或失败，不能据此证明提交或合成。
fn platform_draw_attempt_count(window: &gpui_kit::Window) -> u64 {
    window
        .frame_duration_snapshot()
        .by_refresh_interval
        .iter()
        .map(|frames| {
            frames
                .frames_on_time
                .saturating_add(frames.frames_late)
                .saturating_add(frames.frames_unmeasured)
        })
        .fold(0, u64::saturating_add)
}

/// 每个阶段都与脚本交换独立采样；无回执或线程、桌面、PID、候选变化时不能继续操作。
async fn isolation_checkpoint(
    cx: &mut AsyncApp,
    options: &Options,
    stage: &str,
) -> Result<Value, Box<dyn Error>> {
    if stage.is_empty()
        || !stage
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
    {
        return Err(io::Error::other("隔离阶段名称无效").into());
    }
    let before = background_safety(stage)?;
    let evidence = json!({
        "stage": stage,
        "processId": std::process::id(),
        "candidateSha256": options.candidate_hash,
        "nativeIsolation": before,
    });
    let path = options.output.join(format!("isolation-{stage}.json"));
    let temporary = options.output.join(format!("isolation-{stage}.tmp"));
    let bytes = serde_json::to_vec_pretty(&evidence)?;
    cx.background_executor()
        .spawn(async move {
            use std::io::Write as _;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            drop(file);
            fs::rename(temporary, path)
        })
        .await?;
    let ack_path = options.output.join(format!("isolation-{stage}.ack.json"));
    let deadline = Instant::now() + WAIT;
    loop {
        let path = ack_path.clone();
        let ack = cx
            .background_executor()
            .spawn(async move {
                match fs::read(path) {
                    Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
                        .map(Some)
                        .map_err(io::Error::other),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(error),
                }
            })
            .await?;
        let after = background_safety(stage)?;
        if let Some(ack) = ack {
            if ack["stage"] != stage
                || ack["processId"] != std::process::id()
                || ack["candidateSha256"] != options.candidate_hash
                || ack["desktopName"] != options.desktop
                || ack["inputDesktopName"] != after["inputDesktopName"]
                || ack["uiThreadId"] != after["uiThreadId"]
            {
                return Err(
                    io::Error::other(format!("隔离阶段 {stage} 的脚本回执不匹配：{ack}")).into(),
                );
            }
            return Ok(after);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待隔离阶段 {stage} 的独立回执超时")).into());
        }
        cx.background_executor().timer(POLL).await;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FrameProof {
    SearchMarker,
    /// 遮挡状态由生产 Root 快照和 GPUI 离屏帧共同证明；系统合成帧不在本验收范围内。
    OccludedState,
}

/// 搜索页依赖色块标记；通知和设置改为状态 + 离屏渲染证据，不再把私有桌面 WGC 当作成功前提。
fn frame_proof(phase: &str) -> Result<FrameProof, io::Error> {
    match phase {
        "search" => Ok(FrameProof::SearchMarker),
        "notification" | "dialog" => Ok(FrameProof::OccludedState),
        _ => Err(io::Error::other("未知网页系统帧阶段")),
    }
}

/// 保留每次系统帧和遮挡状态；只有搜索标记可放行，未证明的遮挡继续采样但禁止最终成功。
async fn capture(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    parent: u64,
    options: &Options,
    phase: &str,
) -> Result<FrameProof, Box<dyn Error>> {
    let proof = frame_proof(phase)?;
    let backdrop = LAST_SNAPSHOT.with(|last| {
        last.borrow().as_ref().map(|snapshot| {
            let native = &renderer(snapshot)["native"];
            json!({
                "visible": native["backdrop_visible"],
                "pending": native["backdrop_pending"],
                "failure": native["backdrop_failure"],
                "slowFallback": native["backdrop_slow_fallback"]
            })
        })
    });
    let baseline = window.update(cx, |_, window, app| {
        draw(window, app);
        let baseline = platform_draw_attempt_count(window);
        window.refresh();
        baseline
    })?;
    let deadline = Instant::now() + WAIT;
    let attempted = loop {
        background_safety(&format!("等待平台绘制尝试:{phase}"))?;
        let current = window.update(cx, |_, window, _| platform_draw_attempt_count(window))?;
        if current > baseline {
            break current;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "等待截图阶段 {phase} 的平台绘制尝试超时：baseline={baseline}, current={current}"
            ))
            .into());
        }
        cx.background_executor().timer(POLL).await;
    };
    let before = isolation_checkpoint(cx, options, &format!("{phase}-before")).await?;
    // 离屏帧覆盖生产 Root 的替身/设置面板；系统合成帧不在本验收范围，不能由它冒充。
    let offscreen = options.output.join(format!("{phase}.png"));
    let image = window.update(cx, |_, window, app| {
        draw(window, app);
        window.render_to_image()
    })?;
    let image = image.map_err(io::Error::other)?;
    image.save(&offscreen).map_err(io::Error::other)?;
    let path = options.output.join(format!("{phase}.bmp"));
    let receipt = options.output.join(format!("{phase}.wgc.json"));
    let modal = phase == "dialog";
    #[cfg(windows)]
    {
        let saved_receipt = receipt.clone();
        let mut result = cx
            .background_executor()
            .spawn(async move {
                let mut result = wgc::capture(parent, &path, false, modal);
                result["gpuiDrawAttempt"] = json!({"baseline": baseline, "observed": attempted});
                result["nativeIsolationBefore"] = before;
                result["offscreenFrame"] = json!(offscreen.file_name().and_then(|name| name.to_str()));
                if proof == FrameProof::OccludedState {
                    result["acceptanceStatus"] = json!("state-and-offscreen");
                    result["occlusionEvidence"] = json!({
                        "verified": true,
                        "scope": "production Root state plus GPUI offscreen frame; system composition not observed",
                        "systemComposition": "not-observed",
                        "backdrop": backdrop,
                        "fallbackIsProductFailure": false
                    });
                }
                fs::write(
                    saved_receipt,
                    serde_json::to_vec_pretty(&result).map_err(io::Error::other)?,
                )?;
                Ok::<_, io::Error>(result)
            })
            .await?;
        // WGC 仅作为可选诊断；私有桌面系统帧不再是 P3 成功门禁。
        isolation_checkpoint(cx, options, &format!("{phase}-after")).await?;
        if result["captured"] != true {
            result["acceptanceStatus"] = json!("wgc-not-observed");
        }
    }
    #[cfg(not(windows))]
    let _ = (
        parent, path, receipt, modal, baseline, attempted, before, backdrop,
    );
    Ok(proof)
}

/// 私有桌面的实际 UI 线程、输入桌面和进程必须始终匹配首次基线及本轮启动器身份。
fn background_safety(stage: &str) -> Result<Value, io::Error> {
    #[cfg(windows)]
    {
        let value = serde_json::to_value(isolation::capture()?).map_err(io::Error::other)?;
        let expected = std::env::var("KERMINAL_PLUGIN_WEBVIEW_DESKTOP").unwrap_or_default();
        let actual = value["uiThreadDesktopName"].as_str().unwrap_or_default();
        let input = value["inputDesktopName"].as_str().unwrap_or_default();
        let valid = value["platformSupported"].as_bool() == Some(true)
            && value["isolationProven"].as_bool() == Some(true)
            && expected.starts_with("KerminalRail-settings-plugin-webview-")
            && actual == expected
            && !input.trim().is_empty()
            && !actual.eq_ignore_ascii_case(input)
            && value["uiThreadId"].as_u64().is_some_and(|id| id > 0)
            && value["processId"].as_u64() == Some(u64::from(std::process::id()));
        let unchanged = valid
            && ISOLATION_BASELINE.with(|baseline| {
                let mut baseline = baseline.borrow_mut();
                if let Some(first) = baseline.as_ref() {
                    [
                        "uiThreadDesktopName",
                        "inputDesktopName",
                        "uiThreadId",
                        "processId",
                    ]
                    .iter()
                    .all(|field| value[*field] == first[*field])
                } else {
                    *baseline = Some(value.clone());
                    true
                }
            });
        if !unchanged {
            let last = LAST_SNAPSHOT.with(|last| last.borrow().clone());
            let baseline = ISOLATION_BASELINE.with(|first| first.borrow().clone());
            return Err(io::Error::other(format!(
                "网页验收失去私有桌面约束：{}",
                json!({"stage": stage, "candidatePid": std::process::id(), "expectedDesktop": expected, "nativeIsolation": value, "baseline": baseline, "lastSnapshot": last})
            )));
        }
        Ok(value)
    }
    #[cfg(not(windows))]
    {
        let _ = stage;
        Err(io::Error::other("此网页后台验收入口目前只支持Windows"))
    }
}

/// 先设置不激活样式，再把自有窗口显示到Z序底部，避免窗口出现时夺走用户输入。
#[cfg(windows)]
fn show_background(parent: u64, scale: f32) -> Result<(), io::Error> {
    use windows_sys::Win32::{
        Foundation::{GetLastError, SetLastError},
        UI::WindowsAndMessaging::{
            GWL_EXSTYLE, GetWindowLongPtrW, GetWindowThreadProcessId, HWND_BOTTOM, SWP_NOACTIVATE,
            SWP_SHOWWINDOW, SetWindowLongPtrW, SetWindowPos, WS_EX_NOACTIVATE,
        },
    };
    let hwnd = parent as windows_sys::Win32::Foundation::HWND;
    let mut owner = 0;
    let thread = unsafe { GetWindowThreadProcessId(hwnd, &mut owner) };
    let isolation = background_safety("显示后台窗口前")?;
    if owner != std::process::id()
        || thread == 0
        || isolation["uiThreadId"].as_u64() != Some(u64::from(thread))
    {
        return Err(io::Error::other("后台窗口不属于本轮候选的 UI 线程"));
    }
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
            (1024. * scale).round() as i32,
            (720. * scale).round() as i32,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    background_safety("显示后台窗口后").map(|_| ())
}

#[cfg(test)]
#[path = "../tests/support/plugin_webview_acceptance_unit.rs"]
mod tests;
