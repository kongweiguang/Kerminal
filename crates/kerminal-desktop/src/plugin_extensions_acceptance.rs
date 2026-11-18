//! 扩展市场通过真实服务、HTTP 目录与安装确认界面完成隔离原生验收。
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
    AnyWindowHandle, AppContext as _, AsyncApp, Entity, Keystroke, WindowOptions,
    component::TitleBar,
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
    // 验收只要求目录 URL：目录按 JSON 加载，不再需要公钥或签名文件。
    // 来源保存场景结束后还要用同一地址核对隔离配置与运行源，先在这里固定下来。
    let expected_source = std::env::var("KERMINAL_PLUGIN_CATALOG_URL")
        .map_err(|_| io::Error::other("缺少一次性目录配置"))?;
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
    // 来源保存的最终核对必须在组合根关闭前完成：设置从隔离磁盘真实回读，
    // 运行源从正式扩展服务读取，两者都必须等于启动器注入的夹具地址。
    // 这里只证明落盘值与运行源一致，不把 UI 内部回读次数当成证据。
    let source_check = (|| -> Result<(String, String), Box<dyn Error>> {
        let runtime_url = services
            .extensions
            .as_ref()
            .map(|service| service.source_url())
            .ok_or_else(|| io::Error::other("缺少正式扩展服务"))?;
        let config_url = runtime
            .block_on(services.preferences.load_settings())
            .map_err(|error| -> Box<dyn Error> { error.into() })?
            .settings
            .plugin_catalog_url;
        validate_source_urls(&config_url, &runtime_url, &expected_source)?;
        Ok((config_url, runtime_url))
    })();
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
    // 来源场景的配置与运行源核对独立于 UI 快照；两者不一致时整轮验收失败，
    // 不能只把页面看起来正常当成保存闭环。
    let (config_url, runtime_url) = source_check?;
    report["source"]["configUrl"] = json!(config_url);
    report["source"]["runtimeUrl"] = json!(runtime_url);
    report["source"]["expectedUrl"] = json!(expected_source);
    report["source"]["configMatchesRuntime"] = json!(true);
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

/// 来源保存的最终一致性只比较配置回读、运行源与启动器注入地址。
///
/// 三者任一为空或不同都说明保存、回读或切换没有形成闭环；单独成函数让单元测试
/// 覆盖缺失、错配和成功路径，而不依赖真实窗口。
/// @author kongweiguang
fn validate_source_urls(
    config_url: &str,
    runtime_url: &str,
    expected_url: &str,
) -> Result<(), io::Error> {
    if config_url.is_empty() || runtime_url.is_empty() || expected_url.is_empty() {
        return Err(io::Error::other("来源地址为空，无法证明保存闭环"));
    }
    if config_url != runtime_url || runtime_url != expected_url {
        return Err(io::Error::other(format!(
            "来源保存未形成闭环：config={config_url}, runtime={runtime_url}, expected={expected_url}"
        )));
    }
    Ok(())
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

/// 把启动器主题字符串映射为运行时主题。
///
/// 选项解析阶段已拒绝未知值；这里集中映射，供初次打开页面和来源保存后重新应用
/// 共用，避免两处分支漂移导致某次回执后主题悄悄退回默认值。
/// @author kongweiguang
fn acceptance_theme(theme: &str) -> ThemeMode {
    match theme {
        "light" => ThemeMode::Light,
        "dark" => ThemeMode::Dark,
        _ => ThemeMode::System,
    }
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
            root.set_right_rail_acceptance_theme(acceptance_theme(&options.theme), cx);
            root.extensions_acceptance_open(window, cx)
        })
    })??;
    let market = wait(cx, root, window, "JSON 目录", |value| {
        value["page"]["catalog_entries"] == 2
            && value["page"]["busy"] == false
            && value["page"]["offline"] == false
    })
    .await?;
    capture_frame(cx, window, &options.output.join("catalog.png")).await?;
    // 初始目录就绪后真实点击页头来源入口：运行态来源已被验收环境覆盖为夹具地址，
    // 而隔离设置仍为空；保持回填地址不变点击保存，才能让设置面板真实走一次
    // 保存→load_settings 回读→切换运行源，而不是直接调用 set_source_url。
    let source_url = std::env::var("KERMINAL_PLUGIN_CATALOG_URL")
        .map_err(|_| io::Error::other("缺少一次性目录配置"))?;
    let source_before = wait(cx, root, window, "来源入口就绪", |value| {
        value["page"]["source_url"].as_str() == Some(source_url.as_str())
            && value["page"]["source_dialog_open"] == false
            && value["page"]["source_saving"] == false
            && value["page"]
                .get("source_error")
                .is_some_and(Value::is_null)
    })
    .await?;
    // 页头的商店来源与文件安装统一收进更多菜单；验收仍走真实点击与键盘选择，
    // 不再依赖已被移除的独立来源按钮。菜单打开后第一个可选项就是“商店来源…”，
    // Down 选中第一项、Enter 确认，与应用内键盘操作路径一致。
    wait_selector(cx, window, "extensions-more").await?;
    click_selector(cx, window, "extensions-more", false)?;
    wait_selector(cx, window, "popup-menu").await?;
    for key in ["down", "enter"] {
        window.update(cx, |_, window, app| {
            window.dispatch_keystroke(Keystroke::parse(key).map_err(io::Error::other)?, app);
            Ok::<_, io::Error>(())
        })??;
    }
    let source_dialog = wait(cx, root, window, "商店来源弹层", |value| {
        value["page"]["source_dialog_open"] == true
            && value["page"]["source_saving"] == false
            && value["page"]
                .get("source_error")
                .is_some_and(Value::is_null)
    })
    .await?;
    // 页面在延迟打开前就置位 source_dialog_open；等真实保存按钮进入渲染树后
    // 再截图，避免把尚未绘制的空弹层当成证据。
    wait_selector(cx, window, "extensions-source-save").await?;
    capture_frame(cx, window, &options.output.join("source-dialog.png")).await?;
    click_selector(cx, window, "extensions-source-save", false)?;
    let source_saved = wait(
        cx,
        root,
        window,
        "来源保存回读与目录重载",
        |value| {
            value["page"]["source_dialog_open"] == false
                && value["page"]["source_saving"] == false
                && value["page"]
                    .get("source_error")
                    .is_some_and(Value::is_null)
                && value["page"]["source_url"].as_str() == Some(source_url.as_str())
                && value["page"]["catalog_entries"] == 2
                && value["page"]["offline"] == false
                && value["page"]["busy"] == false
        },
    )
    .await?;
    // 来源保存会触发设置面板回读并应用持久化主题，覆盖本轮内存中的验收主题；
    // 保存闭环确认后重新应用，确保后续安装、统一列表和窄窗截图仍属于所选主题，
    // 而不是把默认深色误当成 light 主题证据。
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.set_right_rail_acceptance_theme(acceptance_theme(&options.theme), cx)
        })
    })?;
    wait_selector(cx, window, "extension-row-acme.market-one").await?;
    // 行本身不抢焦点；点击第一行把焦点留在列表，再用真实键盘序列覆盖 Home/End/方向键。
    click_selector(cx, window, "extension-row-acme.market-one", false)?;
    for key in ["end", "home", "down"] {
        window.update(cx, |_, window, app| {
            window.dispatch_keystroke(Keystroke::parse(key).map_err(io::Error::other)?, app);
            Ok::<_, io::Error>(())
        })??;
    }
    let selection = wait(cx, root, window, "键盘选择第二条目录项", |value| {
        value["page"]["selected"] == "acme.market-two"
    })
    .await?;
    capture_frame(cx, window, &options.output.join("selection.png")).await?;
    wait_selector(cx, window, "extension-action-acme.market-two").await?;
    click_selector(cx, window, "extension-action-acme.market-two", false)?;
    let catalog_confirmation = wait(cx, root, window, "目录安装确认", |value| {
        value["page"]["confirmation_kind"] == "install"
            && value["page"]["confirmation_id"] == "acme.market-two"
    })
    .await?;
    capture_frame(cx, window, &options.output.join("catalog-permissions.png")).await?;
    wait_selector(cx, window, "extensions-confirm").await?;
    click_selector(cx, window, "extensions-confirm", false)?;
    let catalog_installed = wait(cx, root, window, "目录下载与提交", |value| {
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
    // 宽窗五个标准阶段已在 1200x800 留证；再真实缩放窗口留下单栏列表、行点击详情
    // 和 Escape 返回列表的关键状态。返回按钮只在窄布局渲染，选择器存在才能证明
    // 页面确实降级为单栏。200% DPI 需要切换系统缩放，当前私有桌面会话无法可靠模拟，
    // 报告如实标为未覆盖。
    // 隐藏窗口只调用 resize 不会更新原生绘制表面；复用设置验收的隐藏窗口门禁，
    // 真实改变尺寸、投递 WM_PAINT 并等待 GPU 目标同步后再读取页面宽度。
    crate::settings_acceptance::prepare_capture_surface_for_title(cx, window, 700, 800, TITLE)
        .await?;
    // 宽窗键盘选择把 back_to_list 置为真，缩放后先看到列表；等实测宽度降下来再点击。
    let narrow_list = wait(cx, root, window, "窄窗列表", |value| {
        value["page"]["back_to_list"] == true
            && value["page"]["pending"] == 2
            && builtin_collapsed(value) == Some(true)
            && value["page"]["content_width"]
                .as_f64()
                .is_some_and(|width| width < 700.0)
    })
    .await?;
    wait_selector(cx, window, "extension-row-acme.market-one").await?;
    capture_frame(cx, window, &options.output.join("narrow-list.png")).await?;
    click_selector(cx, window, "extension-row-acme.market-one", false)?;
    let narrow_detail = wait(cx, root, window, "窄窗行点击详情", |value| {
        value["page"]["back_to_list"] == false && value["page"]["selected"] == "acme.market-one"
    })
    .await?;
    wait_selector(cx, window, "extension-back-to-list").await?;
    capture_frame(cx, window, &options.output.join("narrow-detail.png")).await?;
    // 焦点已由行点击交给详情；真实 Escape 必须回到列表而不是关闭设置页。
    window.update(cx, |_, window, app| {
        window.dispatch_keystroke(Keystroke::parse("escape").map_err(io::Error::other)?, app);
        Ok::<_, io::Error>(())
    })??;
    let narrow_returned = wait(cx, root, window, "窄窗 Escape 返回列表", |value| {
        value["page"]["back_to_list"] == true
    })
    .await?;
    wait_selector(cx, window, "extension-row-acme.market-one").await?;
    capture_frame(cx, window, &options.output.join("narrow-list-returned.png")).await?;
    // 恢复脚本约定的 1200x800，最终 viewport 与五个标准阶段保持同一尺寸。
    crate::settings_acceptance::prepare_capture_surface_for_title(cx, window, 1200, 800, TITLE)
        .await?;
    wait(cx, root, window, "恢复宽窗", |value| {
        value["page"]["back_to_list"] == true
            && value["page"]["pending"] == 2
            && builtin_collapsed(value) == Some(true)
            && value["page"]["content_width"]
                .as_f64()
                // 1200 窗口的正文宽度约 903（导航与留白占其余部分），
                // 只要明显高于 700 的窄窗阈值即可证明已恢复双栏。
                .is_some_and(|width| width > 800.0)
    })
    .await?;
    // 统一列表没有已安装 Tab：安装提交后直接等待真实分组快照。内置组默认收起，
    // 待重启身份只出现在 pending 组，目录组按稳定身份去重；不能用返回态或
    // pending 数量代替分组事实。
    let unified = wait(cx, root, window, "统一扩展列表", |value| {
        value["page"]["pending"] == 2
            && value["page"]["busy"] == false
            && value["page"]["confirmation_visible"] == false
            && value["page"]["pending_restart"] == true
            && value["page"]["back_to_list"] == true
            && builtin_collapsed(value) == Some(true)
            && group_count(value, "installed").is_none()
            && group_count(value, "pending") == Some(2)
            && group_count(value, "market") == Some(1)
            && value["page"]["update_candidates"] == 0
    })
    .await?;
    wait_selector(cx, window, "extensions-group-builtin").await?;
    for selector in [
        "extension-row-acme.market-two",
        "extension-row-acme.file-search",
        "extension-row-acme.market-one",
    ] {
        wait_selector(cx, window, selector).await?;
    }
    capture_frame(cx, window, &options.output.join("unified-list.png")).await?;
    // 内置组标题本身是真实展开/收起入口；快照中的 collapsed 与渲染同源，
    // 展开后还必须出现真实内置行，避免只改标题箭头。
    click_selector(cx, window, "extensions-group-builtin", false)?;
    let builtin_expanded = wait(cx, root, window, "内置组展开", |value| {
        builtin_collapsed(value) == Some(false)
            && group_count(value, "builtin").is_some()
            && value["page"]["pending"] == 2
    })
    .await?;
    wait_selector(cx, window, "extension-row-builtin-kerminal.core").await?;
    capture_frame(cx, window, &options.output.join("builtin-expanded.png")).await?;
    click_selector(cx, window, "extensions-group-builtin", false)?;
    let builtin_collapsed_again = wait(cx, root, window, "内置组收起", |value| {
        builtin_collapsed(value) == Some(true) && value["page"]["pending"] == 2
    })
    .await?;
    capture_frame(cx, window, &options.output.join("builtin-collapsed.png")).await?;
    let (viewport, scale) =
        window.update(cx, |_, window, _| viewport_evidence(window, [1200, 800]))??;
    // 当前夹具没有已安装外部记录，也没有可更新版本；报告只记录真实候选数，
    // 批量确认/执行没有在本轮原生流程中出现，不能据此宣称批量更新已通过。
    let installed_group_observed = group_count(&unified, "installed").is_some();
    let update_candidates = unified["page"]["update_candidates"]
        .as_u64()
        .ok_or_else(|| io::Error::other("统一列表缺少批量更新候选数"))?;
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| root.request_quit(QuitIntent::Exit, cx))
    })?;
    wait(cx, root, window, "生产退出", |value| {
        value["quitReady"] == true
    })
    .await?;
    Ok(
        json!({"schemaVersion":1,"mode":ACCEPTANCE_MODE,"processId":std::process::id(),"theme":options.theme,"desktopName":options.desktop,"nativeIsolation":isolation,"viewport":viewport,"scaleFactor":scale,"frameEvidence":frame_evidence(),"market":market,"source":{"entry":"real header button opens the window dialog; unchanged backfilled fixture URL is saved through the settings CAS and load_settings read-back","before":source_before,"dialog":source_dialog,"saved":source_saved,"screenshot":"source-dialog.png","networkLog":"fixture catalog JSON only; no minisig request"},"selection":selection,"catalogConfirmation":catalog_confirmation,"catalogInstalled":catalog_installed,"confirmation":confirmation,"installed":installed,"unifiedList":{"collapsed":unified,"expanded":builtin_expanded,"collapsedAgain":builtin_collapsed_again,"screenshots":["unified-list.png","builtin-expanded.png","builtin-collapsed.png"],"entry":"real unified list; builtin group header click toggles expansion","installedGroupObserved":installed_group_observed,"updateCandidates":update_candidates,"batchUpdateNativeCovered":false,"batchUpdateNotCoveredReason":"fixture has no installed updatable records; update-all was not exercised"},"narrow":{"viewport":[700,800],"list":narrow_list,"detail":narrow_detail,"returned":narrow_returned,"screenshots":["narrow-list.png","narrow-detail.png","narrow-list-returned.png"],"dpi200PercentTested":false},"inputMode":"synthetic GPUI pointer dispatch to focus the extension list plus real Home/End/arrow keystrokes; production file-picker result entry","nativeImeTested":false,"dpiSwitchTested":false,"sleepResumeTested":false}),
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

/// 统一列表的组计数直接读取页面快照；缺失或类型不符返回 None，让等待继续到超时并打印真实快照。
fn group_count(snapshot: &Value, kind: &str) -> Option<u64> {
    snapshot["page"]["groups"]
        .as_array()?
        .iter()
        .find(|group| group["kind"].as_str() == Some(kind))?
        .get("count")?
        .as_u64()
}

/// 内置组折叠事实来自与渲染同源的快照；不能用返回列表或待重启数量推断。
fn builtin_collapsed(snapshot: &Value) -> Option<bool> {
    snapshot["page"]["groups"]
        .as_array()?
        .iter()
        .find(|group| group["kind"].as_str() == Some("builtin"))?
        .get("collapsed")?
        .as_bool()
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
                value["page"] = serde_json::to_value(root.extensions_acceptance_snapshot(cx))
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
