//! 扩展市场通过真实服务、签名目录与安装确认界面完成隔离原生验收。
//! @author kongweiguang

use crate::{
    bootstrap,
    composition::DesktopComposition,
    plugin_acceptance::{
        capture_frame, checked_isolation, click_selector, draw, viewport_evidence, wait_selector,
    },
    plugins::{arguments::PluginStartupOptions, startup::StartupPlugins},
    runtime::ApplicationRuntime,
};
use gpui_kit::{
    AnyWindowHandle, AppContext as _, AsyncApp, Entity, WindowOptions, component::TitleBar,
};
use gpui_pre::{Bounds, TitlebarOptions, WindowBounds, px, size};
use kerminal_application::preferences::ThemeMode;
use kerminal_domain::plugins::PluginStoreState;
use kerminal_ui::{AppServices, QuitIntent, RootView, RootViewEvent};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    error::Error,
    ffi::OsString,
    fmt, fs, io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const TITLE: &str = "Kerminal Extensions Acceptance";
const WAIT: Duration = Duration::from_secs(45);
const POLL: Duration = Duration::from_millis(100);
const ACCEPTANCE_MODE: &str = "private-desktop-composition";
const FRAME_VALIDATION: &str = "png-structure-crc-and-dimensions";
// 全新隔离根只会提交这两项；缺少、重复或出现额外 pending 都说明安装回执不能作为本次证据。
const EXPECTED_PENDING: [&str; 2] = ["acme.market-two", "acme.file-search"];

#[derive(Clone)]
struct Options {
    output: PathBuf,
    package: PathBuf,
    theme: String,
    /// 启动器为本轮创建的唯一私有桌面；报告与隔离快照必须与它完全一致，不能用前缀或空值猜测。
    desktop: String,
}

/// 信任覆盖只能与新隔离根一起使用，实际安装仍由组合根的 ExtensionsService 执行。
pub(super) fn run() -> Result<(), Box<dyn Error>> {
    let options = options(std::env::args_os().skip(2))?;
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
    if std::env::var("KERMINAL_PLUGIN_CATALOG_URL").is_err()
        || std::env::var("KERMINAL_PLUGIN_CATALOG_PUBKEY").is_err()
    {
        return Err(io::Error::other("缺少一次性签名目录配置").into());
    }
    // SAFETY: 当前入口尚未创建运行时或 UI 线程，仅修改候选进程自己的数据根。
    unsafe {
        std::env::set_var("KERMINAL_CONFIG_ROOT", &config);
    }
    let candidate = format!("{:x}", Sha256::digest(fs::read(std::env::current_exe()?)?));
    let package = format!("{:x}", Sha256::digest(fs::read(&options.package)?));
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
            dev_plugin: None,
        },
    )?);
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
    // 与正式入口一致，先装配真实扩展服务和工厂，再克隆窗口依赖。
    let installation = runtime.block_on(composition.install_external_plugins(startup));
    let services = composition.app_services();
    let ui: Result<_, Box<dyn Error>> = match installation {
        Ok(()) => run_window(services.clone(), &options),
        Err(error) => Err(error.into()),
    };
    // 权威状态必须在关闭组合根前读取；pending 集合的严格核对也在同一阶段完成。
    let pending = match &services.extensions {
        Some(service) => runtime
            .block_on(service.state())
            .map_err(|error| -> Box<dyn Error> { error.into() })
            .and_then(|state| {
                validate_pending_install(&state, &EXPECTED_PENDING)
                    .map_err(|error| -> Box<dyn Error> { error.into() })
            }),
        None => Err(io::Error::other("缺少正式扩展服务").into()),
    };
    let deadline = ui
        .as_ref()
        .map(|(_, deadline)| *deadline)
        .unwrap_or_else(|_| Instant::now() + Duration::from_secs(12));
    let shutdown = runtime
        .block_on(composition.shutdown_until(deadline.into()))
        .map_err(|error| -> Box<dyn Error> { error.into() });
    drop(primary);
    // ApplicationRuntime::shutdown 没有返回值，无法观察运行时是否在超时前排空；报告只声明组合根资源。
    runtime.shutdown();
    // ui 的成功值同时包含生产退出截止时间；截止时间已在关闭阶段使用，报告只取快照。
    let ((mut report, _quit_deadline), pending_ids) =
        combine_acceptance_results(ui, shutdown, pending)?;
    report["candidateSha256"] = json!(candidate);
    report["localPackageSha256"] = json!(package);
    report["pendingCountReadBack"] = json!(pending_ids.len());
    report["pendingPluginIds"] = json!(pending_ids);
    // 只有真实收到生产退出截止时间并有界面成功回执时才可达此处。
    report["quitReceipt"] = json!(true);
    report["resourcesShutdownComplete"] = json!(true);
    report["runtimeShutdownObserved"] = json!(false);
    report["shutdownScope"] = json!("composition-resources-only");
    report["isolationScope"] = json!("candidate-win32-thread-snapshot");
    fs::write(
        options.output.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}

/// 缺失、重复或额外的 pending 都不能通过；只返回排序后的权威身份，避免数量相等掩盖替换。
fn validate_pending_install(
    state: &PluginStoreState,
    expected: &[&str],
) -> Result<Vec<String>, io::Error> {
    let mut actual = state
        .pending
        .iter()
        .map(|change| change.plugin_id().as_str().to_owned())
        .collect::<Vec<_>>();
    actual.sort_unstable();
    let mut expected = expected
        .iter()
        .map(|id| (*id).to_owned())
        .collect::<Vec<_>>();
    expected.sort_unstable();
    if actual != expected {
        return Err(io::Error::other(format!(
            "安装回执没有权威 pending 集合：expected={expected:?}, actual={actual:?}"
        )));
    }
    Ok(actual)
}

#[derive(Debug)]
struct AcceptanceFailures {
    failures: Vec<Box<dyn Error>>,
}

impl fmt::Display for AcceptanceFailures {
    /// 同时暴露界面、资源关闭和状态回读的失败，避免任一阶段覆盖其余真实原因。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let messages = self
            .failures
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        write!(formatter, "扩展验收存在多项失败：{}", messages.join("；"))
    }
}

impl Error for AcceptanceFailures {
    /// 主错误链保留首个失败，其余失败仍由显示文本完整列出。
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.failures.first().map(|error| error.as_ref())
    }
}

/// 三个阶段全部结束后统一决议；不能因为界面失败就跳过关闭错误或权威状态错误的保留。
fn combine_acceptance_results<T>(
    ui: Result<T, Box<dyn Error>>,
    shutdown: Result<(), Box<dyn Error>>,
    pending: Result<Vec<String>, Box<dyn Error>>,
) -> Result<(T, Vec<String>), Box<dyn Error>> {
    let mut failures = Vec::new();
    let ui = match ui {
        Ok(value) => Some(value),
        Err(error) => {
            failures.push(error);
            None
        }
    };
    let shutdown = match shutdown {
        Ok(()) => Some(()),
        Err(error) => {
            failures.push(error);
            None
        }
    };
    let pending = match pending {
        Ok(value) => Some(value),
        Err(error) => {
            failures.push(error);
            None
        }
    };
    match (ui, shutdown, pending) {
        (Some(ui), Some(()), Some(pending)) => Ok((ui, pending)),
        _ => Err(Box::new(AcceptanceFailures { failures })),
    }
}

/// 输出根不能复用，包路径必须存在；主题与选项拼写错误均直接失败。
fn options(arguments: impl IntoIterator<Item = OsString>) -> Result<Options, io::Error> {
    let mut arguments = arguments.into_iter();
    let mut values = std::collections::BTreeMap::new();
    while let Some(key) = arguments.next() {
        let key = key
            .into_string()
            .map_err(|_| io::Error::other("选项编码无效"))?;
        if !["--output-dir", "--local-package", "--theme"].contains(&key.as_str()) {
            return Err(io::Error::other("未知扩展验收选项"));
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
            .ok_or_else(|| io::Error::other("缺少输出目录"))?,
    );
    let package = PathBuf::from(
        values
            .remove("--local-package")
            .ok_or_else(|| io::Error::other("缺少本地包"))?,
    )
    .canonicalize()?;
    let theme = values
        .remove("--theme")
        .map(|value| {
            value
                .into_string()
                .map_err(|_| io::Error::other("主题编码无效"))
        })
        .transpose()?
        .unwrap_or_else(|| "system".into());
    let desktop = private_desktop_name()?;
    if !output.is_absolute()
        || output.exists()
        || !package.is_file()
        || !["light", "dark", "system"].contains(&theme.as_str())
    {
        return Err(io::Error::other("扩展验收目录、包或主题无效"));
    }
    Ok(Options {
        output,
        package,
        theme,
        desktop,
    })
}

/// 本轮桌面名由启动器注入；只接受固定前缀和 32 位小写十六进制，缺失或别名都拒绝继续。
#[cfg(windows)]
fn private_desktop_name() -> Result<String, io::Error> {
    let name = std::env::var("KERMINAL_PLUGIN_EXTENSIONS_DESKTOP")
        .map_err(|_| io::Error::other("缺少本轮私有桌面身份"))?;
    let valid = name
        .strip_prefix("KerminalRail-settings-")
        .is_some_and(|suffix| {
            suffix.len() == 32
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
    if !valid {
        return Err(io::Error::other("本轮私有桌面身份无效"));
    }
    Ok(name)
}

/// 非 Windows 不提供私有桌面启动器；保留编译路径但不虚构桌面证据。
#[cfg(not(windows))]
fn private_desktop_name() -> Result<String, io::Error> {
    Ok(String::new())
}

/// 普通 Root 窗口在私有桌面绘制；退出以生产 QuitReady 截止时间为唯一成功门禁。
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
            let completed = outcome.clone();
            let closing = deadline.clone();
            let options = options.clone();
            let result = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: gpui_kit::point(px(40.), px(40.)),
                        size: size(px(1200.), px(800.)),
                    })),
                    show: false,
                    focus: false,
                    titlebar: Some(TitlebarOptions {
                        title: Some(TITLE.into()),
                        ..TitleBar::title_bar_options()
                    }),
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
                        if result.is_err() {
                            let _ = capture_frame(cx, window, &options.output.join("failure.png"))
                                .await;
                        }
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
        .ok_or_else(|| io::Error::other("扩展验收没有回执"))?
        .map_err(io::Error::other)?;
    let deadline = retained_deadline
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .ok_or_else(|| io::Error::other("缺少生产退出回执"))?;
    Ok((report, deadline))
}

/// 列表选择通过键盘焦点与Enter，安装通过真实预检、确认按钮和权威状态回读。
async fn exercise(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    options: &Options,
) -> Result<Value, Box<dyn Error>> {
    let isolation = window.update(cx, |_, window, _| checked_isolation(window))??;
    // 候选内的 Win32 采样还必须命中启动器指定的唯一桌面，而不是仅满足通用前缀。
    #[cfg(windows)]
    validate_private_desktop(&isolation, &options.desktop)?;
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.enable_right_rail_acceptance_activity(cx)
        })
    })?;
    wait(cx, root, window, "工作区", |value| {
        value["workspaceReady"] == true
    })
    .await?;
    #[cfg(windows)]
    crate::settings_acceptance::prepare_capture_surface_for_title(cx, window, 1200, 800, TITLE)
        .await?;
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.set_right_rail_acceptance_theme(
                match options.theme.as_str() {
                    "light" => ThemeMode::Light,
                    "dark" => ThemeMode::Dark,
                    _ => ThemeMode::System,
                },
                cx,
            );
            root.extensions_acceptance_open(window, cx)
        })
    })??;
    let market = wait(cx, root, window, "验签目录", |value| {
        value["page"]["catalog_entries"] == 2
            && value["page"]["busy"] == false
            && value["page"]["offline"] == false
    })
    .await?;
    capture_frame(cx, window, &options.output.join("catalog.png")).await?;
    wait_selector(cx, window, "extension-select-acme.market-one").await?;
    // Button 明确在鼠标按下时阻止抢焦点，Tab 顺序无法从任意行开始；此处用真实指针分发选择第二项，
    // 键盘路径由 P3 输入与 P7 原生动作覆盖，不在本页把不可达的 Tab 序列伪装成通过。
    click_selector(cx, window, "extension-select-acme.market-one", false)?;
    click_selector(cx, window, "extension-select-acme.market-two", false)?;
    let selection = wait(cx, root, window, "选择第二条目录项", |value| {
        value["page"]["selected"] == "acme.market-two"
    })
    .await?;
    capture_frame(cx, window, &options.output.join("selection.png")).await?;
    wait_selector(cx, window, "extension-action-acme.market-two").await?;
    click_selector(cx, window, "extension-action-acme.market-two", false)?;
    let catalog_confirmation = wait(cx, root, window, "签名目录安装确认", |value| {
        value["page"]["confirmation_kind"] == "install"
            && value["page"]["confirmation_id"] == "acme.market-two"
    })
    .await?;
    capture_frame(cx, window, &options.output.join("catalog-permissions.png")).await?;
    wait_selector(cx, window, "extensions-confirm").await?;
    click_selector(cx, window, "extensions-confirm", false)?;
    let catalog_installed = wait(cx, root, window, "签名目录下载与提交", |value| {
        value["page"]["pending"] == 1
            && value["page"]["busy"] == false
            && value["page"]["confirmation_visible"] == false
            && value["page"]["pending_restart"] == true
    })
    .await?;
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.extensions_acceptance_inspect_file(options.package.clone(), cx)
        })
    })??;
    let confirmation = wait(cx, root, window, "真实包权限确认", |value| {
        value["page"]["confirmation_visible"] == true
            && value["page"]["busy"] == false
            && value["page"]["confirmation_kind"] == "file"
            && value["page"]["confirmation_id"] == "acme.file-search"
            && value["page"]["confirmation_permissions"]
                .as_array()
                .is_some_and(|permissions| {
                    permissions
                        .iter()
                        .any(|permission| permission == "full-access")
                })
    })
    .await?;
    capture_frame(cx, window, &options.output.join("permissions.png")).await?;
    wait_selector(cx, window, "extensions-confirm").await?;
    click_selector(cx, window, "extensions-confirm", false)?;
    let installed = wait(cx, root, window, "已提交待重启安装", |value| {
        value["page"]["pending"] == 2
            && value["page"]["busy"] == false
            && value["page"]["confirmation_visible"] == false
            && value["page"]["pending_restart"] == true
    })
    .await?;
    capture_frame(cx, window, &options.output.join("pending-restart.png")).await?;
    let (viewport, scale) =
        window.update(cx, |_, window, _| viewport_evidence(window, [1200, 800]))??;
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| root.request_quit(QuitIntent::Exit, cx))
    })?;
    wait(cx, root, window, "生产退出", |value| {
        value["quitReady"] == true
    })
    .await?;
    Ok(
        json!({"schemaVersion":1,"mode":ACCEPTANCE_MODE,"processId":std::process::id(),"theme":options.theme,"desktopName":options.desktop,"nativeIsolation":isolation,"viewport":viewport,"scaleFactor":scale,"frameEvidence":frame_evidence(),"market":market,"selection":selection,"catalogConfirmation":catalog_confirmation,"catalogInstalled":catalog_installed,"confirmation":confirmation,"installed":installed,"inputMode":"synthetic GPUI pointer dispatch for extension rows; keyboard row selection is not reachable because buttons prevent focus on mouse down; production file-picker result entry","nativeImeTested":false,"dpiSwitchTested":false,"sleepResumeTested":false}),
    )
}

/// 验收线程的桌面、进程和输入桌面必须同时匹配本轮身份；任一字段缺失或类型不符都不能继续。
#[cfg(windows)]
fn validate_private_desktop(isolation: &Value, expected: &str) -> Result<(), io::Error> {
    let process_id = isolation.get("processId").and_then(Value::as_u64);
    let ui_thread_id = isolation.get("uiThreadId").and_then(Value::as_u64);
    let desktop = isolation
        .get("uiThreadDesktopName")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let input = isolation
        .get("inputDesktopName")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if isolation.get("platformSupported").and_then(Value::as_bool) != Some(true)
        || isolation.get("isolationProven").and_then(Value::as_bool) != Some(true)
        || process_id != Some(u64::from(std::process::id()))
        || ui_thread_id.is_none_or(|id| id == 0)
        || desktop != expected
        || input.trim().is_empty()
        || expected.eq_ignore_ascii_case(input.trim())
    {
        return Err(io::Error::other(format!(
            "扩展验收未证明本轮私有桌面隔离：{isolation}"
        )));
    }
    Ok(())
}

/// 截图只证明图片结构、CRC 和尺寸；没有同帧特征像素检查时不得宣称主题或系统合成已观察。
fn frame_evidence() -> Value {
    json!({
        "source": "gpui-render-to-image",
        "validation": FRAME_VALIDATION,
        "featurePixelsChecked": false,
        "systemComposition": "not-observed",
    })
}

/// 快照只读取正式页面，所有等待推进绘制并持续校验私有桌面身份。
async fn wait(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    stage: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Result<Value, Box<dyn Error>> {
    let deadline = Instant::now() + WAIT;
    loop {
        let snapshot = window.update(cx, |_, window, app| {
            checked_isolation(window)?;
            draw(window, app);
            root.update(app, |root, cx| {
                let mut value = root.plugin_acceptance_snapshot(cx);
                value["page"] = serde_json::to_value(root.extensions_acceptance_snapshot(cx)?)
                    .map_err(io::Error::other)?;
                value["quitReady"] = json!(
                    root.transfer_workspace_acceptance_quit_ready()
                        .map_err(io::Error::other)?
                );
                Ok::<_, Box<dyn Error>>(value)
            })
        })??;
        if predicate(&snapshot) {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待{stage}超时：{snapshot}")).into());
        }
        cx.background_executor().timer(POLL).await;
    }
}

#[cfg(test)]
#[path = "../tests/support/plugin_extensions_acceptance_unit.rs"]
mod tests;
