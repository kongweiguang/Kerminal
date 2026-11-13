//! 插件主机类型表单的隔离原生验收：受监督 Sidecar 插件、生产面板表单、host.test/save/open 与结果 Tab。
//! 入口只由显式环境变量触发；正常启动和发行构建不注册菜单或可发现命令。
//!
//! @author kongweiguang

use std::{
    error::Error,
    fs, io,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use gpui_kit::{
    AnyWindowHandle, AppContext as _, AsyncApp, Entity, TitlebarOptions,
    WindowBackgroundAppearance, WindowOptions, component::TitleBar,
};
use gpui_pre::{Bounds, WindowBounds, px, size};
use kerminal_application::preferences::ThemeMode;
use kerminal_domain::identity::HostId;
use kerminal_plugin_protocol::{Platform, PluginRuntime};
use kerminal_ui::{AppServices, QuitIntent, RootView, RootViewEvent};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    bootstrap,
    composition::DesktopComposition,
    plugin_acceptance::{capture_frame, checked_isolation, draw},
    plugins::{arguments::PluginStartupOptions, startup::StartupPlugins},
    runtime::ApplicationRuntime,
};

const PLUGIN_ID: &str = "kerminal.http-check";
const HOST_TYPE: &str = "kerminal.http-check/http-endpoint";
const RESULT_CONTENT: &str = "kerminal.http-check/http-result";
const TITLE: &str = "Kerminal Plugin Host Form Acceptance";
const WAIT: Duration = Duration::from_secs(45);
const TEST_WAIT: Duration = Duration::from_secs(35);
const POLL: Duration = Duration::from_millis(120);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(12);
const NAME: &str = "HTTP 验收主机";
const URL: &str = "https://example.com/";

/// 环境变量由隔离启动器设置，避免把路径或主题拼进可被无关进程读取的命令行。
#[derive(Clone)]
struct Options {
    output: PathBuf,
    plugin: PathBuf,
    theme: ThemeMode,
    theme_name: &'static str,
}

struct UiResult {
    report: Value,
    deadline: Instant,
}

/// 只接受显式绝对路径和三主题之一；输出根必须为空并位于调用方指定位置。
pub(super) fn run() -> Result<(), Box<dyn Error>> {
    let options = options_from_env()?;
    fs::create_dir(&options.output)?;
    let config_directory = options.output.join("config");
    fs::create_dir(&config_directory)?;
    // 与 P3/P5 验收一致：预先写入当前格式，避免空根首次迁移和验收日志产生 journal 竞态。
    fs::write(
        config_directory.join(".kerminal-format.toml"),
        b"data_format = 4\nmigration_contract = 4\n",
    )?;
    let output = options.output.canonicalize()?;
    let config = config_directory.canonicalize()?;
    if !config.starts_with(&output) {
        return Err(io::Error::other("验收配置根越界").into());
    }
    let candidate_hash = digest(&fs::read(std::env::current_exe()?)?);
    let runtime = ApplicationRuntime::new()?;
    let primary =
        match crate::acquire_single_instance(&runtime, &crate::single_instance_id(&config)?)? {
            single_application::SingleApplication::PrimaryInstance(primary) => primary,
            single_application::SingleApplication::SecondaryInstance(_) => {
                return Err(io::Error::other("隔离配置根已有运行进程").into());
            }
        };
    let startup = Arc::new(StartupPlugins::load(
        &config,
        PluginStartupOptions {
            safe_mode: false,
            dev_plugin: Some(options.plugin.canonicalize()?),
        },
    )?);
    if startup.enabled.len() != 1 || startup.enabled[0].id.as_str() != PLUGIN_ID {
        return Err(io::Error::other("主机表单验收只允许 http-check 一个开发插件").into());
    }
    let prepared = startup.enabled[0].clone();
    let plugin_hash = plugin_digest(&prepared)?;
    let catalogs = startup.catalogs("zh-CN")?;
    let migration = bootstrap::migration_service(&config, None, catalogs.1.clone());
    bootstrap::run_after_migration(&migration, false, |_| ())
        .inspect_err(report_migration_io_error)?;
    let mut composition = runtime.block_on(DesktopComposition::build_with_plugin_catalogs(
        runtime.handle(),
        &config,
        None,
        catalogs.0,
        catalogs.1,
    ))?;
    let install = runtime.block_on(composition.install_external_plugins(startup.clone()));
    if let Err(error) = install {
        let _ = runtime
            .block_on(composition.shutdown_until(tokio::time::Instant::now() + SHUTDOWN_TIMEOUT));
        drop(primary);
        runtime.shutdown();
        return Err(error.into());
    }
    let services = composition.app_services();
    let ui = catch_unwind(AssertUnwindSafe(|| {
        run_window(services.clone(), startup, &options, &output)
    }))
    .unwrap_or_else(|_| Err(io::Error::other("主机表单验收事件循环异常").into()));
    let shutdown_deadline = ui
        .as_ref()
        .map(|result| result.deadline)
        .unwrap_or_else(|_| Instant::now() + SHUTDOWN_TIMEOUT);
    let shutdown = runtime.block_on(composition.shutdown_until(shutdown_deadline.into()));
    drop(primary);
    runtime.shutdown();
    let mut ui = ui?;
    shutdown?;
    ui.report["candidateSha256"] = json!(candidate_hash);
    ui.report["pluginSha256"] = json!(plugin_hash);
    ui.report["pluginVersion"] = json!(prepared.version.to_string());
    ui.report["theme"] = json!(options.theme_name);
    ui.report["configDirectory"] = json!(config);
    ui.report["shutdownComplete"] = json!(true);
    fs::write(
        output.join("report.json"),
        serde_json::to_vec_pretty(&ui.report)?,
    )?;
    Ok(())
}

/// 只读取本次启动器设置的三个环境变量；缺省或非法时拒绝，不访问用户配置或网络。
fn options_from_env() -> Result<Options, io::Error> {
    let output = std::env::var_os("KERMINAL_PLUGIN_HOST_FORM_OUTPUT")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("缺少主机表单验收输出根"))?;
    let plugin = std::env::var_os("KERMINAL_PLUGIN_HOST_FORM_PLUGIN")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("缺少主机表单验收插件目录"))?;
    let theme_name = match std::env::var("KERMINAL_PLUGIN_HOST_FORM_THEME").as_deref() {
        Ok("light") => "light",
        Ok("dark") => "dark",
        Ok("system") => "system",
        _ => return Err(io::Error::other("主机表单验收主题无效")),
    };
    if !output.is_absolute() || !plugin.is_absolute() {
        return Err(io::Error::other("主机表单验收路径必须是绝对路径"));
    }
    let theme = match theme_name {
        "light" => ThemeMode::Light,
        "dark" => ThemeMode::Dark,
        _ => ThemeMode::System,
    };
    Ok(Options {
        output,
        plugin,
        theme,
        theme_name,
    })
}

/// 只对真实 Sidecar 计算清单、资源和当前平台入口摘要，不能用包目录名冒充执行内容。
fn plugin_digest(
    prepared: &kerminal_application::external_contributions::PreparedPlugin,
) -> Result<String, Box<dyn Error>> {
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(&prepared.package.manifest)?);
    for (path, bytes) in &prepared.resources {
        digest.update((path.len() as u64).to_le_bytes());
        digest.update(path.as_bytes());
        digest.update(bytes.as_ref());
    }
    let PluginRuntime::Sidecar { main, .. } = &prepared.package.manifest.runtime else {
        return Err(io::Error::other("主机表单验收只支持 Sidecar 插件").into());
    };
    let platform =
        Platform::current().ok_or_else(|| io::Error::other("当前平台无法核对 Sidecar 入口"))?;
    let main = main
        .get(&platform)
        .ok_or_else(|| io::Error::other("主机表单验收插件未声明当前平台 Sidecar"))?;
    digest.update(fs::read(prepared.directory.join(main))?);
    Ok(format!("{:x}", digest.finalize()))
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// 隐藏原生窗口只运行在已证明隔离的桌面，退出等待与截图沿用同一窗口句柄。
fn run_window(
    services: AppServices,
    startup: Arc<StartupPlugins>,
    options: &Options,
    output: &Path,
) -> Result<UiResult, Box<dyn Error>> {
    let options = options.clone();
    let output = output.to_path_buf();
    let result = Arc::new(Mutex::new(None::<Result<Value, String>>));
    let result_for_app = result.clone();
    let deadline = Arc::new(Mutex::new(None::<Instant>));
    let deadline_for_app = deadline.clone();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            kerminal_ui::init(cx);
            let Some(display) = cx.primary_display() else {
                *result_for_app
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) =
                    Some(Err("无可用显示设备".into()));
                cx.quit();
                return;
            };
            let bounds =
                Bounds::centered_at(display.visible_bounds().center(), size(px(1200.), px(800.)));
            let window_options = WindowOptions {
                window_background: WindowBackgroundAppearance::Transparent,
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(1200.), px(800.))),
                titlebar: Some(TitlebarOptions {
                    title: Some(TITLE.into()),
                    ..TitleBar::title_bar_options()
                }),
                show: false,
                focus: false,
                ..TitleBar::window_options()
            };
            let opened = gpui_kit::open_window(window_options, cx, |window, cx| {
                window.resize(size(px(1200.), px(800.)));
                let root = cx.new(|cx| RootView::new(services.clone(), window, cx));
                cx.subscribe(&root, move |_, event: &RootViewEvent, _| {
                    if let RootViewEvent::QuitReady { deadline, .. } = event {
                        *deadline_for_app
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) = Some(*deadline);
                    }
                })
                .detach();
                let handle = window.window_handle();
                let owned_root = root.clone();
                let completed = result_for_app.clone();
                let services_task = services.clone();
                let startup_task = startup.clone();
                let options_task = options.clone();
                let output_task = output.clone();
                cx.spawn(async move |cx| {
                    let outcome = exercise(
                        cx,
                        &owned_root,
                        handle,
                        services_task.clone(),
                        startup_task.clone(),
                        options_task.clone(),
                        output_task.clone(),
                    )
                    .await;
                    if outcome.is_err() {
                        let _ = capture_frame(
                            cx,
                            handle,
                            &output_task.join(format!("{}-failure.png", options_task.theme_name)),
                        )
                        .await;
                    }
                    *completed.lock().unwrap_or_else(|error| error.into_inner()) =
                        Some(outcome.map_err(|error| error.to_string()));
                    cx.update(|cx| cx.quit());
                })
                .detach();
                root
            });
            if let Err(error) = opened {
                *result_for_app
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(Err(error.to_string()));
                cx.quit();
            }
        });
    let report = result
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .ok_or_else(|| io::Error::other("原生验收未返回回执"))?
        .map_err(io::Error::other)?;
    let deadline = deadline
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .ok_or_else(|| io::Error::other("未收到生产退出截止时间"))?;
    Ok(UiResult { report, deadline })
}

/// 走生产面板、表单、测试、保存与打开入口；只读回报文与持久配置，不直接写主机文件。
async fn exercise(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: AppServices,
    startup: Arc<StartupPlugins>,
    options: Options,
    output: PathBuf,
) -> Result<Value, Box<dyn Error>> {
    let isolation = window.update(cx, |_, window, _| checked_isolation(window))??;
    wait(cx, root, window, "工作区和设置", |snapshot| {
        snapshot["workspaceReady"] == true && snapshot["settingsReady"] == true
    })
    .await?;
    // plugin_webview 快照的 registeredContent 固定检查 file-search；这里按本轮插件的真实内容身份断言。
    if services.plugins.content_type(RESULT_CONTENT).is_none() {
        return Err(io::Error::other("http-check 结果内容未注册").into());
    }
    #[cfg(windows)]
    crate::settings_acceptance::prepare_capture_surface_for_title(cx, window, 1200, 800, TITLE)
        .await?;
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.set_right_rail_acceptance_theme(options.theme, cx)
        })
    })?;
    let startup_for_boot = startup.clone();
    services
        .executor
        .run(move |_| async move {
            let startup = startup_for_boot;
            tokio::task::spawn_blocking(move || {
                startup
                    .store
                    .mark_boot_phase(
                        &startup.snapshot.boot_id,
                        kerminal_domain::plugins::PluginBootPhase::FirstFrame,
                    )
                    .map_err(|source| {
                        kerminal_application::AppError::infrastructure_with_source(
                            "主机表单验收首帧提交失败",
                            source,
                        )
                    })
            })
            .await
            .map_err(|source| {
                kerminal_application::AppError::infrastructure_with_source(
                    "主机表单验收首帧任务失败",
                    source,
                )
            })??;
            Ok(())
        })
        .await?;

    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.plugin_host_form_acceptance_activate(HOST_TYPE, window, cx)
        })
    })??;
    let opened = wait(cx, root, window, "插件主机表单", |snapshot| {
        snapshot["hostForm"]["panel"]["typeId"] == HOST_TYPE
            && snapshot["hostForm"]["panel"]["form"]["fields"]
                .as_array()
                .is_some_and(|fields| fields.iter().any(|field| field["path"] == "/url"))
    })
    .await?;
    let host_id = opened["hostForm"]["panel"]["hostId"]
        .as_str()
        .ok_or_else(|| io::Error::other("插件主机表单身份缺失"))?
        .to_owned();
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.plugin_host_form_acceptance_set_field("$name", NAME, window, cx)?;
            root.plugin_host_form_acceptance_set_field("/url", URL, window, cx)
        })
    })??;
    let filled = wait(cx, root, window, "表单字段写入", |snapshot| {
        let form = &snapshot["hostForm"]["panel"]["form"];
        form["name"] == NAME
            && form["fields"].as_array().is_some_and(|fields| {
                fields
                    .iter()
                    .any(|field| field["path"] == "/url" && field["value"] == URL)
            })
    })
    .await?;
    capture_frame(cx, window, &output.join("host-form.png")).await?;

    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.plugin_host_form_acceptance_test(window, cx)
        })
    })??;
    let tested = wait_with(cx, root, window, "host.test 回执", TEST_WAIT, |snapshot| {
        let panel = &snapshot["hostForm"]["panel"];
        panel["phase"] == "idle"
            && panel["status"].as_str().is_some_and(|status| {
                !status.is_empty() && status != "测试已取消。" && status != "Test cancelled."
            })
    })
    .await?;
    let test_status = tested["hostForm"]["panel"]["status"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.plugin_host_form_acceptance_save(window, cx)
        })
    })??;
    let host = wait_persisted_host(cx, services.clone(), host_id.clone()).await?;
    let configuration = serde_json::to_value(
        host.configuration
            .as_ref()
            .ok_or_else(|| io::Error::other("保存主机缺少配置"))?,
    )?;
    if configuration["url"] != URL {
        return Err(
            io::Error::other(format!("保存的主机配置未回读目标地址：{configuration}")).into(),
        );
    }
    let saved = json!({
        "hostId": host.id.as_str(),
        "name": host.name,
        "providerId": host.provider_id,
        "typeId": host.type_id,
        "configuration": configuration,
    });

    wait(cx, root, window, "连接编辑器关闭", |snapshot| {
        snapshot["hostForm"]["modalOpen"] == false
    })
    .await?;
    let open_id = HostId::parse(&host_id).map_err(io::Error::other)?;
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.plugin_acceptance_open_host(open_id.clone(), window, cx)
        })
    })?;
    let result = wait(cx, root, window, "host.open 结果标签", |snapshot| {
        snapshot["tabs"].as_array().is_some_and(|tabs| {
            tabs.iter().any(|tab| {
                tab["contentId"] == RESULT_CONTENT && tab["renderer"]["status"] == "Ready"
            })
        })
    })
    .await?;
    let result_tab = result["tabs"]
        .as_array()
        .and_then(|tabs| tabs.iter().find(|tab| tab["contentId"] == RESULT_CONTENT))
        .cloned()
        .ok_or_else(|| io::Error::other("结果标签缺失"))?;
    capture_frame(cx, window, &output.join("host-result.png")).await?;

    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| root.request_quit(QuitIntent::Exit, cx))
    })?;
    wait(cx, root, window, "生产退出", |snapshot| {
        snapshot["quitReady"] == true
    })
    .await?;
    Ok(json!({
        "schemaVersion": 1,
        "processId": std::process::id(),
        "theme": options.theme_name,
        "mode": "private-desktop-production-host-form",
        "nativeIsolation": isolation,
        "hostType": HOST_TYPE,
        "hostId": host_id,
        "filledForm": filled["hostForm"].clone(),
        "testStatus": test_status,
        "networkProbeScope": "example.com network is unavailable in this environment; the plugin's explicit failure text is accepted as the observed host.test outcome",
        "savedHost": saved,
        "resultTab": result_tab,
        "inputMode": "production panel form controls driven through acceptance hooks; no synthetic pointer or keyboard input",
        "nativeImeTested": false,
        "dpiSwitchTested": false,
        "sleepResumeTested": false,
        "physicalDisplayMeasured": false,
    }))
}

async fn wait_persisted_host(
    cx: &mut AsyncApp,
    services: AppServices,
    host_id: String,
) -> Result<kerminal_application::connections::host_types::PluginHost, Box<dyn Error>> {
    let hosts = services
        .hosts
        .clone()
        .ok_or_else(|| io::Error::other("主机管理服务未装配"))?;
    let id = HostId::parse(&host_id).map_err(io::Error::other)?;
    let deadline = Instant::now() + WAIT;
    loop {
        let result = services
            .executor
            .run({
                let hosts = hosts.clone();
                let id = id.clone();
                move |_| async move { hosts.load_plugin_host(&id).await }
            })
            .await?;
        if let Some(host) = result {
            return Ok(host);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("等待主机持久化超时").into());
        }
        cx.background_executor().timer(POLL).await;
    }
}

/// 全量快照同时包含工作区标签、插件表单和退出回执，避免各阶段观察不同来源。
async fn wait(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    stage: &str,
    mut predicate: impl FnMut(&Value) -> bool,
) -> Result<Value, Box<dyn Error>> {
    wait_with(cx, root, window, stage, WAIT, &mut predicate).await
}

async fn wait_with(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    stage: &str,
    timeout: Duration,
    mut predicate: impl FnMut(&Value) -> bool,
) -> Result<Value, Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        let snapshot = window.update(cx, |_, window, app| {
            checked_isolation(window)?;
            draw(window, app);
            root.update(app, |root, cx| {
                let mut value = root.external_plugin_acceptance_snapshot(window, cx);
                value["hostForm"] = root.plugin_host_form_acceptance_snapshot(cx);
                value["quitReady"] = json!(
                    root.transfer_workspace_acceptance_quit_ready()
                        .map_err(io::Error::other)?
                );
                Ok::<_, io::Error>(value)
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

/// 启动路径在迁移前不初始化日志；这里只输出分类与错误码，不展开路径或数据。
fn report_migration_io_error(error: &kerminal_application::AppError) {
    let mut source: Option<&(dyn Error + 'static)> = Some(error);
    for depth in 0..16 {
        let Some(current) = source else { break };
        if let Some(error) = current.downcast_ref::<io::Error>() {
            eprintln!(
                "主机表单验收迁移系统错误：source_depth={depth} kind={:?} raw_os_error={:?}",
                error.kind(),
                error.raw_os_error()
            );
        }
        source = current.source();
    }
}
