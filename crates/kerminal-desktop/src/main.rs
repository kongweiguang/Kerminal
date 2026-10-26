//! GPUI desktop composition root.
//!
//! @author kongweiguang

#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

// gpui-kit 0.7.3 的 Action derive 固定展开 gpui:: 路径，因此仅在有该动作的 macOS 构建提供 facade 别名。
#[cfg(target_os = "macos")]
extern crate gpui_kit as gpui;

#[cfg(feature = "acceptance-metrics")]
mod acceptance_metrics;
mod bootstrap;
mod composition;
mod config_root;
mod external_activation;
#[cfg(target_os = "linux")]
mod fonts;
mod host_coordinator;
mod logging;
mod platform_lifecycle;
mod runtime;

use std::{
    error::Error,
    future::Future,
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use gpui_kit::AppContext as _;
use gpui_kit::component::TitleBar;
#[cfg(target_os = "linux")]
use gpui_pre::WindowDecorations;
use gpui_pre::{
    App, Bounds, TitlebarOptions, WindowBackgroundAppearance, WindowBounds, WindowOptions, px, size,
};
use kerminal_application::{
    AppError, AppResult,
    platform::ExternalActivation,
    platform::window_state::{
        WindowStateLoad, WindowStateRepository, WindowStateUnavailableReason,
    },
};
use kerminal_infrastructure::platform::window_state::WindowStateStore;
use kerminal_ui::{
    AppServices, RootView, RootViewEvent,
    window_state::{WindowStateBinding, restored_window_bounds},
};
use sha2::{Digest, Sha256};
use single_application::{PrimaryInstance, SecondaryInstance, SingleApplication};
use tokio::{
    sync::{mpsc, oneshot},
    time::Instant as TokioInstant,
};

use crate::composition::DesktopComposition;
use crate::config_root::normalize_path;
#[cfg(target_os = "macos")]
use crate::external_activation::ActivationQueueSender;
use crate::external_activation::{
    ActivationRequest, activation_from_os, activation_queue, encode_message,
};
#[cfg(target_os = "windows")]
use crate::host_coordinator::PrimaryListenerRecovery;
use crate::host_coordinator::{
    HostCommand, HostCoordinator, HostCoordinatorSender, HostResponse, PrimaryListener,
    UpdateHostAction,
};
use crate::runtime::ApplicationRuntime;
#[cfg(target_os = "macos")]
use gpui_kit::{KeyBinding, Menu, MenuItem};

const APPLICATION_ID: &str = "io.github.kongweiguang.kerminal";
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(12);
const WINDOW_STATE_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
const SINGLE_INSTANCE_SEND_TIMEOUT: Duration = Duration::from_secs(2);

type StartupError = Arc<Mutex<Option<String>>>;

/// macOS 菜单退出动作只桥接到 Root 关闭门禁，不直接结束 GPUI 应用。
/// @author kongweiguang
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq, gpui_kit::Action)]
#[action(name = "kerminal.requestQuit", no_json, no_register)]
struct MacApplicationQuit;

/// 解析根目录后先完成同一 Rust 迁移用例；失败时不得初始化 runtime、单实例、日志或业务服务。
///
/// @author kongweiguang
fn main() {
    let config_root = match config_root::ConfigRoot::resolve() {
        Ok(config_root) => config_root,
        Err(error) => {
            report_fatal_startup_error(&format!("Kerminal 无法解析数据目录：{error}"));
            std::process::exit(1);
        }
    };
    let legacy_window_state = bootstrap::legacy_window_state_path(false);
    let migration =
        bootstrap::migration_service(config_root.as_path(), legacy_window_state.as_deref());
    if let Err(error) = bootstrap::run_after_migration(&migration, |_| run_desktop(config_root)) {
        report_fatal_startup_error(&format!(
            "Kerminal 数据迁移未完成：{}\n\n请保持旧版 Kerminal 关闭，先运行安装包内的 migration 脚本做只读预检（Windows 用 -DryRun，macOS/Linux 用 --dry-run）；按报告处理并保留备份与恢复材料。",
            error.public_message()
        ));
        std::process::exit(1);
    }
}

/// 只有迁移门禁成功后才创建共享 runtime；静态阶段回执区分宿主、UI 事件循环与 runtime 收尾。
///
/// @author kongweiguang
fn run_desktop(config_root: config_root::ConfigRoot) {
    let runtime = match ApplicationRuntime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            report_fatal_startup_error(&format!("Kerminal 无法创建应用 runtime：{error}"));
            std::process::exit(1);
        }
    };
    let mut log_guard = None;
    let result = catch_unwind(AssertUnwindSafe(|| {
        run_application(&runtime, config_root, &mut log_guard)
    }));
    tracing::info!(event = "DESKTOP_APPLICATION_RETURNED");
    tracing::info!(event = "DESKTOP_RUNTIME_STOP_BEGIN");
    runtime.shutdown();
    tracing::info!(event = "DESKTOP_RUNTIME_STOP_COMPLETE");

    let failure_message = match result {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(format!("Kerminal 启动或退出失败：{error}")),
        Err(_) => Some("Kerminal 桌面事件循环意外终止".to_owned()),
    };
    if failure_message.is_some() && log_guard.is_some() {
        tracing::error!(event = "DESKTOP_APPLICATION_STOPPED_AFTER_ERROR");
    }
    if let Some(log_guard) = log_guard.take() {
        log_guard.flush();
    }
    if let Some(message) = failure_message {
        report_fatal_startup_error(&message);
        std::process::exit(1);
    }
}

/// GUI 初始化前优先展示原生错误提示；Windows 用系统 API，其他平台尽力调用成熟 dialog 后端。
fn report_fatal_startup_error(message: &str) {
    eprintln!("{message}");

    #[cfg(windows)]
    {
        use std::{iter::once, os::windows::ffi::OsStrExt};
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};

        let body = std::ffi::OsStr::new(message)
            .encode_wide()
            .chain(once(0))
            .collect::<Vec<_>>();
        let title = std::ffi::OsStr::new("Kerminal 启动失败")
            .encode_wide()
            .chain(once(0))
            .collect::<Vec<_>>();
        // Windows 接口只读取这两个在调用期间存活的 NUL 结尾缓冲区。
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                body.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
    }

    #[cfg(not(windows))]
    {
        let _ = rfd::MessageDialog::new()
            .set_title("Kerminal 启动失败")
            .set_description(message)
            .set_level(rfd::MessageLevel::Error)
            .show();
    }
}

/// 只提取标准 IO 错误的系统数字码，避免把任意错误文本写入长期诊断日志。
fn source_raw_os_error(error: &(dyn Error + 'static)) -> Option<i32> {
    let mut source = Some(error);
    while let Some(current) = source {
        if let Some(io_error) = current.downcast_ref::<io::Error>() {
            return io_error.raw_os_error();
        }
        source = current.source();
    }
    None
}

/// 仅主实例初始化共享滚动日志并装配服务，避免二次实例并发写同一个活动日志文件。
/// @author kongweiguang
fn run_application(
    runtime: &ApplicationRuntime,
    config_root: config_root::ConfigRoot,
    log_guard: &mut Option<logging::DesktopLogGuard>,
) -> Result<(), Box<dyn Error>> {
    let instance_id = single_instance_id(config_root.as_path())?;
    let activation = current_process_external_activation()?;
    match acquire_single_instance(runtime, &instance_id)? {
        SingleApplication::SecondaryInstance(secondary) => {
            activate_existing_instance(runtime, secondary, activation)?;
            Ok(())
        }
        SingleApplication::PrimaryInstance(primary) => {
            *log_guard = Some(logging::initialize(config_root.as_path()).map_err(|error| {
                io::Error::other(format!("Kerminal 无法初始化诊断日志：{error}"))
            })?);
            run_primary_instance(runtime, instance_id, primary, config_root, activation)
        }
    }
}

/// 仅由进程入口读取原生 argv 与 cwd；解析规则仍复用共享 activation codec。
fn current_process_external_activation() -> AppResult<Option<ExternalActivation>> {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    let working_directory = if arguments.len() > 1 {
        Some(
            std::env::current_dir()
                .map_err(|_| AppError::invalid("无法读取外部启动工作目录"))?
                .into_os_string(),
        )
    } else {
        None
    };
    activation_from_os(arguments, working_directory)
}

/// 在 Tokio context 中注册既有产品身份；依赖 crate 的 panic 型错误在桌面边界收敛。
fn acquire_single_instance(
    runtime: &ApplicationRuntime,
    instance_id: &str,
) -> Result<SingleApplication<String>, io::Error> {
    catch_unwind(AssertUnwindSafe(|| {
        runtime.block_on(async { SingleApplication::<String>::new(instance_id) })
    }))
    .map_err(|_| io::Error::other("无法取得 Kerminal 单实例监听器"))
}

/// Windows 隔离根先建立真实目录再规范化，避免大小写或 extended-path 别名争用不同实例。
/// 首次启动也必须得到可解析的目录；准备或解析失败直接拒绝启动，不能退回有歧义的字符串身份。
/// debug/release 都按数据根区分实例，避免 GPUI 测试版复用旧版正式身份而无法同时运行。
///
/// @author kongweiguang
fn single_instance_id(config_root: &Path) -> Result<String, io::Error> {
    let absolute_root = if config_root.is_absolute() {
        config_root.to_path_buf()
    } else {
        std::env::current_dir()?.join(config_root)
    };
    #[cfg(windows)]
    let absolute_root = {
        std::fs::create_dir_all(&absolute_root).map_err(|error| {
            io::Error::new(error.kind(), format!("无法为单实例准备配置目录：{error}"))
        })?;
        std::fs::canonicalize(&absolute_root).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("无法确认单实例配置目录的真实路径：{error}"),
            )
        })?
    };
    let normalized_root = normalize_path(&absolute_root);
    let digest = Sha256::digest(normalized_root.to_string_lossy().as_bytes());
    let suffix = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("{APPLICATION_ID}.dev-{suffix}"))
}

/// 二次实例在启动边界有界发送；写入成功不代表已处理，超时可能已受理且不可自动重试。
fn activate_existing_instance(
    runtime: &ApplicationRuntime,
    secondary: SecondaryInstance<String>,
    activation: Option<ExternalActivation>,
) -> Result<(), Box<dyn Error>> {
    let request = activation
        .map(ActivationRequest::External)
        .unwrap_or(ActivationRequest::FocusWindow);
    let message = encode_message(&request)?;
    match runtime.block_on(async {
        tokio::time::timeout(
            SINGLE_INSTANCE_SEND_TIMEOUT,
            secondary.try_send_message(message),
        )
        .await
    }) {
        Ok(result) => result?,
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "向 Kerminal 主实例发送请求超时；请求可能已受理，请先确认主实例状态，不要自动重试",
            )
            .into());
        }
    }
    Ok(())
}

/// 迁移完成后主实例只绑定当前窗口文件；运行中恢复故障保持可见，不再回读旧来源或阻断终端启动。
///
/// @author kongweiguang
fn run_primary_instance(
    runtime: &ApplicationRuntime,
    instance_id: String,
    primary: PrimaryInstance<String>,
    config_root: config_root::ConfigRoot,
    initial_activation: Option<ExternalActivation>,
) -> Result<(), Box<dyn Error>> {
    let composition = runtime.block_on(DesktopComposition::build(
        runtime.handle(),
        config_root.as_path(),
    ))?;
    let mcp_snapshot = composition.mcp_runtime_snapshot();
    let mcp_state_event = match mcp_snapshot.state {
        kerminal_application::mcp_runtime::McpRuntimeState::Unavailable => {
            "MCP_RUNTIME_INITIAL_UNAVAILABLE"
        }
        kerminal_application::mcp_runtime::McpRuntimeState::Stopped => {
            "MCP_RUNTIME_INITIAL_STOPPED"
        }
        kerminal_application::mcp_runtime::McpRuntimeState::Starting => {
            "MCP_RUNTIME_INITIAL_STARTING"
        }
        kerminal_application::mcp_runtime::McpRuntimeState::Synchronizing => {
            "MCP_RUNTIME_INITIAL_SYNCHRONIZING"
        }
        kerminal_application::mcp_runtime::McpRuntimeState::Running => {
            "MCP_RUNTIME_INITIAL_RUNNING"
        }
        kerminal_application::mcp_runtime::McpRuntimeState::Stopping => {
            "MCP_RUNTIME_INITIAL_STOPPING"
        }
        kerminal_application::mcp_runtime::McpRuntimeState::Failed => "MCP_RUNTIME_INITIAL_FAILED",
    };
    tracing::info!(event = mcp_state_event);
    let window_state_repository: Arc<dyn WindowStateRepository> =
        Arc::new(WindowStateStore::new(composition.storage_coordinator()));
    let window_state_load = match runtime.block_on(window_state_repository.load()) {
        Ok(load) => load,
        Err(error) => {
            if let Some(raw_os_error) = source_raw_os_error(&error) {
                tracing::warn!(
                    event = "WINDOW_STATE_LOAD_FAILED",
                    code = error.public_code().as_str(),
                    raw_os_error,
                );
            } else {
                tracing::warn!(
                    event = "WINDOW_STATE_LOAD_FAILED",
                    code = error.public_code().as_str(),
                );
            }
            WindowStateLoad::Unavailable {
                reason: WindowStateUnavailableReason::StorageUnavailable,
            }
        }
    };
    let services = composition.app_services();
    let update_service = composition.update_service().cloned();
    let composition = Arc::new(tokio::sync::Mutex::new(composition));
    let (activation_sender, activation_receiver) = activation_queue();
    if let Some(activation) = initial_activation {
        activation_sender.try_submit(ActivationRequest::External(activation))?;
    }
    let listener = PrimaryListener::new(
        instance_id,
        activation_sender.clone(),
        runtime.handle().clone(),
        primary,
    );
    let mut host = HostCoordinator::start(runtime.handle(), composition, update_service, listener);
    let startup_error = Arc::new(Mutex::new(None));
    let ui_result = catch_unwind(AssertUnwindSafe(|| {
        run_native_ui(
            services,
            #[cfg(target_os = "macos")]
            activation_sender,
            activation_receiver,
            host.sender(),
            runtime.handle(),
            window_state_repository,
            window_state_load,
            startup_error.clone(),
        );
    }));
    tracing::info!(event = "DESKTOP_NATIVE_UI_RETURNED");
    let shutdown_deadline = TokioInstant::now() + SHUTDOWN_TIMEOUT;
    tracing::info!(event = "DESKTOP_HOST_FINALIZE_BEGIN");
    runtime.block_on(host.finalize(shutdown_deadline))?;
    tracing::info!(event = "DESKTOP_HOST_FINALIZE_COMPLETE");
    if ui_result.is_err() {
        return Err(io::Error::other("GPUI 桌面事件循环意外终止").into());
    }
    if let Some(error) = take_startup_error(&startup_error) {
        return Err(io::Error::other(error).into());
    }
    Ok(())
}

/// 无可恢复的已保存位置时，按主显示器可用区域居中创建有边距的合理首窗。
///
/// @author kongweiguang
fn first_run_window_bounds(cx: &App) -> Option<WindowBounds> {
    const INITIAL_WIDTH: f32 = 1280.0;
    const INITIAL_HEIGHT: f32 = 800.0;
    const MAX_MARGIN: f32 = 24.0;

    let visible = cx.primary_display()?.visible_bounds();
    let visible_width = visible.size.width.as_f32();
    let visible_height = visible.size.height.as_f32();
    if !visible_width.is_finite()
        || !visible_height.is_finite()
        || visible_width <= 0.0
        || visible_height <= 0.0
    {
        return None;
    }

    let horizontal_margin = MAX_MARGIN.min(visible_width * 0.05);
    let vertical_margin = MAX_MARGIN.min(visible_height * 0.05);
    let width = INITIAL_WIDTH.min((visible_width - horizontal_margin * 2.0).max(1.0));
    let height = INITIAL_HEIGHT.min((visible_height - vertical_margin * 2.0).max(1.0));
    Some(WindowBounds::Windowed(Bounds::centered_at(
        visible.center(),
        size(px(width), px(height)),
    )))
}

/// macOS Dock reopen 复用已创建的窗口；GPUI 初始窗口在启动回调中同步创建，重开回调只恢复已持有句柄。
/// Linux 在首窗前注册匹配平台 Han fallback 的简中字体，再注册 Kit 图标，避免首帧缺字或缺图。
/// 宿主回执消费先于 app.update 单独记录，区分前台 future 未恢复与原生退出尚未入队。
/// 验收构建在此登记真实窗口线程的 profiler 句柄，后台退出采样不会猜测 Tokio worker 身份。
/// 验收窗口可使用独立标题，避免并行工作区把同名窗口误认成自己的目标。
/// 验收配置错误仅输出固定原因码，预热时长错误也不能回显原始环境值。
/// @author kongweiguang
fn run_native_ui(
    services: AppServices,
    #[cfg(target_os = "macos")] activation_sender: ActivationQueueSender,
    activation_receiver: mpsc::Receiver<ActivationRequest>,
    host_sender: HostCoordinatorSender,
    runtime_handle: tokio::runtime::Handle,
    window_state_repository: Arc<dyn WindowStateRepository>,
    window_state_load: WindowStateLoad,
    startup_error: StartupError,
) {
    #[cfg(all(target_os = "windows", feature = "acceptance-metrics"))]
    set_diagnostic_process_identity();
    #[cfg(feature = "acceptance-metrics")]
    host_coordinator::quit_diagnostics::register_main_thread();
    let application = gpui_kit::application().with_assets(gpui_kit::assets::AllAssets);
    #[cfg(target_os = "macos")]
    let reopen_window_handle = Arc::new(Mutex::new(None::<gpui_kit::AnyWindowHandle>));
    #[cfg(target_os = "macos")]
    {
        let reopen_window_handle = reopen_window_handle.clone();
        application.on_reopen(move |cx| {
            let window_handle = *reopen_window_handle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(window_handle) = window_handle else {
                tracing::warn!(event = "MACOS_DOCK_REOPEN_WINDOW_NOT_READY");
                return;
            };
            match window_handle.update(cx, |_, window, _| show_and_activate_window(window)) {
                Ok(true) => tracing::info!(event = "MACOS_DOCK_REOPENED_EXISTING_WINDOW"),
                Ok(false) => tracing::warn!(event = "MACOS_DOCK_REOPEN_VISIBILITY_FAILED"),
                Err(_) => tracing::warn!(event = "MACOS_DOCK_REOPEN_WINDOW_INVALIDATED"),
            }
        });
    }
    application.run(move |cx| {
        #[cfg(target_os = "linux")]
        if let Err(error) = fonts::register_noto_sans_cjk_sc(cx.text_system()) {
            record_startup_error(
                &startup_error,
                format!("无法注册 Linux 简体中文字体：{error}"),
            );
            cx.quit();
            return;
        }
        kerminal_ui::init(cx);
        #[cfg(target_os = "macos")]
        install_open_url_listener(cx, activation_sender.clone(), runtime_handle.clone());

        #[cfg(feature = "acceptance-metrics")]
        let window_title = acceptance_metrics::diagnostic_window_title()
            .unwrap_or_else(|| "Kerminal".to_owned());
        #[cfg(not(feature = "acceptance-metrics"))]
        let window_title = "Kerminal";
        let window_options = WindowOptions {
            window_background: WindowBackgroundAppearance::Transparent,
            window_bounds: restored_window_bounds(&window_state_load, cx)
                .or_else(|| first_run_window_bounds(cx)),
            titlebar: Some(TitlebarOptions {
                title: Some(window_title.into()),
                ..TitleBar::title_bar_options()
            }),
            #[cfg(target_os = "linux")]
            window_decorations: Some(WindowDecorations::Client),
            ..TitleBar::window_options()
        };
        if let Err(error) = gpui_kit::open_window(window_options, cx, |window, cx| {
            window.set_background_appearance(WindowBackgroundAppearance::Transparent);
            let root = cx.new(|cx| RootView::new(services.clone(), window, cx));
            #[cfg(target_os = "macos")]
            {
                *reopen_window_handle
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(window.window_handle());

                let quit_root = root.clone();
                cx.on_action(move |_: &MacApplicationQuit, app| {
                    quit_root.update(app, |root, cx| {
                        root.request_quit(kerminal_ui::QuitIntent::Exit, cx);
                    });
                });
                cx.bind_keys([KeyBinding::new("cmd-q", MacApplicationQuit, None)]);
                cx.set_menus([Menu::new("Kerminal").items([MenuItem::action(
                    "Quit Kerminal",
                    MacApplicationQuit,
                )])]);
            }
            #[cfg(feature = "acceptance-metrics")]
            match acceptance_metrics::install(window, cx, services.executor.clone()) {
                Ok(acceptance_metrics::InstallDisposition::Disabled) => {}
                Ok(acceptance_metrics::InstallDisposition::Scheduled) => {
                    tracing::info!(event = "ACCEPTANCE_METRICS_SCHEDULED");
                }
                Err(error) => {
                    let reason = match error {
                        acceptance_metrics::AcceptanceMetricsConfigError::InvalidEnableValue => {
                            "invalid_enable_value"
                        }
                        acceptance_metrics::AcceptanceMetricsConfigError::MissingDuration => {
                            "missing_duration"
                        }
                        acceptance_metrics::AcceptanceMetricsConfigError::InvalidDuration => {
                            "invalid_duration"
                        }
                        acceptance_metrics::AcceptanceMetricsConfigError::DurationOutOfRange => {
                            "duration_out_of_range"
                        }
                        acceptance_metrics::AcceptanceMetricsConfigError::InvalidWarmupDuration => {
                            "invalid_warmup_duration"
                        }
                        acceptance_metrics::AcceptanceMetricsConfigError::WarmupDurationOutOfRange => {
                            "warmup_duration_out_of_range"
                        }
                        acceptance_metrics::AcceptanceMetricsConfigError::MissingOutput => {
                            "missing_output"
                        }
                        acceptance_metrics::AcceptanceMetricsConfigError::OutputMustBeAbsoluteFile => {
                            "output_must_be_absolute_file"
                        }
                        acceptance_metrics::AcceptanceMetricsConfigError::InvalidTextEnvironmentValue => {
                            "invalid_text_environment_value"
                        }
                        acceptance_metrics::AcceptanceMetricsConfigError::InvalidLabel => {
                            "invalid_label"
                        }
                        acceptance_metrics::AcceptanceMetricsConfigError::InvalidExpectedSessionCount => {
                            "expected_session_count_must_be_1_10_or_50"
                        }
                    };
                    tracing::warn!(
                        event = "ACCEPTANCE_METRICS_CONFIGURATION_REJECTED",
                        reason = reason,
                    );
                }
            }
            let window_state_binding = root.update(cx, |root_view, root_cx| {
                WindowStateBinding::install(
                    root_view,
                    window,
                    services.executor.clone(),
                    window_state_repository.clone(),
                    window_state_load.clone(),
                    services.notifications.clone(),
                    root_cx,
                )
            });
            let window_state_handle = window_state_binding.handle();
            cx.set_global(window_state_binding);
            install_activation_listener(
                activation_receiver,
                root.clone(),
                window.window_handle(),
                cx,
            );
            let completion_signal = host_sender.clone();
            let completion_subscription = cx.subscribe(&root, move |root_entity, event, app| {
                let event = event.clone();
                let deadline = TokioInstant::now() + SHUTDOWN_TIMEOUT;
                let should_flush_window_state =
                    !matches!(&event, RootViewEvent::RetryInstallReady { .. });
                let command = match &event {
                    RootViewEvent::QuitReady { intent } => HostCommand::Quit {
                        intent: intent.clone(),
                        deadline,
                    },
                    RootViewEvent::InstallReady { update_id, version } => HostCommand::Install {
                        update_id: *update_id,
                        version: version.clone(),
                        deadline,
                        retry: false,
                    },
                    RootViewEvent::RetryInstallReady { update_id, version } => {
                        HostCommand::Install {
                            update_id: *update_id,
                            version: version.clone(),
                            deadline,
                            retry: true,
                        }
                    }
                };
                let root_entity = root_entity.clone();
                let request_sender = completion_signal.clone();
                // 每次事件各持有独立句柄，避免把订阅回调借用的 flush future 逃逸到 runtime。
                let flush_handle = window_state_handle.clone();
                let flush_receipt = should_flush_window_state.then(|| {
                    spawn_window_state_flush(
                        runtime_handle.clone(),
                        async move { flush_handle.flush().await },
                        deadline,
                    )
                });
                app.spawn(async move |app| {
                    if let Some(flush_receipt) = flush_receipt {
                        let failure_message = match flush_receipt.await {
                            Ok(message) => message,
                            Err(_) => Some(
                                "关闭前窗口状态保存任务意外终止；将保留原状态并继续关闭。"
                                    .to_owned(),
                            ),
                        };
                        if let Some(message) = failure_message {
                            app.update(|app| {
                                root_entity.update(app, |root, cx| {
                                    root.report_host_status(message, cx);
                                });
                            });
                        }
                    }
                    let response = request_sender.submit(command).await;
                    if response.is_ok() {
                        tracing::info!(event = "DESKTOP_HOST_RESPONSE_RECEIVED");
                    }
                    app.update(|app| {
                        apply_host_response(root_entity, event, response, app);
                    });
                })
                .detach();
            });
            completion_subscription.detach();
            let tray_owner_installed =
                match platform_lifecycle::install(cx, root.clone(), window.window_handle()) {
                    Ok(receipt) => {
                        let event = match receipt.availability {
                            kerminal_application::platform::PlatformAvailability::Available => {
                                "NATIVE_TRAY_SETUP_FINISHED"
                            }
                            kerminal_application::platform::PlatformAvailability::Conditional {
                                ..
                            } => "NATIVE_TRAY_CONDITIONAL",
                            kerminal_application::platform::PlatformAvailability::Unsupported {
                                ..
                            } => "NATIVE_TRAY_UNSUPPORTED",
                        };
                        tracing::info!(event = event);
                        receipt.owner_installed
                    }
                    Err(error) => {
                        if let Some(raw_os_error) = source_raw_os_error(&error) {
                            tracing::warn!(
                                event = "NATIVE_TRAY_INSTALL_FAILED",
                                code = error.public_code().as_str(),
                                raw_os_error,
                            );
                        } else {
                            tracing::warn!(
                                event = "NATIVE_TRAY_INSTALL_FAILED",
                                code = error.public_code().as_str(),
                            );
                        }
                        false
                    }
                };
            root.update(cx, |root_view, root_cx| {
                root_view.set_tray_owner_installed(tray_owner_installed, root_cx);
            });
            root
        }) {
            record_startup_error(&startup_error, format!("无法打开 Kerminal 主窗口：{error}"));
            cx.quit();
        }
    });
}

/// 显式验收实例使用进程独立的 Windows 身份，避免继承启动宿主的 AppUserModelID，
/// 令窗口定位、输入干扰检测及任务栏分组能区分验收窗口与宿主窗口。
/// 只有有效的验收标题启用该路径；缓冲区在同步系统调用期间保持有效。
///
/// @author kongweiguang
#[cfg(all(target_os = "windows", feature = "acceptance-metrics"))]
fn set_diagnostic_process_identity() {
    if acceptance_metrics::diagnostic_window_title().is_none() {
        return;
    }
    let identity = format!("{APPLICATION_ID}.acceptance.{}", std::process::id())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // API 不保留指针；只设置当前进程身份，不注册应用或修改系统配置。
    let result = unsafe {
        windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(identity.as_ptr())
    };
    if result < 0 {
        tracing::warn!(
            event = "ACCEPTANCE_WINDOW_IDENTITY_FAILED",
            hresult = result
        );
    }
}

/// 将 Tokio timer 与窗口持久化等待放进共享 runtime，GPUI 只接收无 runtime 要求的 oneshot 回执。
fn spawn_window_state_flush<Flush>(
    runtime: tokio::runtime::Handle,
    flush: Flush,
    host_deadline: TokioInstant,
) -> oneshot::Receiver<Option<String>>
where
    Flush: Future<Output = kerminal_application::AppResult<()>> + Send + 'static,
{
    let (sender, receiver) = oneshot::channel();
    runtime.spawn(async move {
        let flush_deadline = host_deadline.min(TokioInstant::now() + WINDOW_STATE_FLUSH_TIMEOUT);
        let failure_message = match tokio::time::timeout_at(flush_deadline, flush).await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.public_message().to_owned()),
            Err(_) => Some("关闭前保存窗口位置超时；将保留原状态并继续关闭。".to_owned()),
        };
        let _ = sender.send(failure_message);
    });
    receiver
}

/// 二次启动和外部 URL 都可能唤醒已隐藏窗口；先要求后端恢复，再激活并分发请求。
fn show_and_activate_window(window: &gpui_kit::Window) -> bool {
    if window.supports_visibility_control() && window.set_window_visibility(true).is_err() {
        return false;
    }
    window.activate_window();
    true
}

/// 首次启动、二次启动和系统 URL 共用 Root 用例；显式检查拒收，避免把排队失败当成受理。
fn install_activation_listener(
    mut receiver: mpsc::Receiver<ActivationRequest>,
    root: gpui_kit::Entity<RootView>,
    window: gpui_kit::AnyWindowHandle,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        while let Some(request) = receiver.recv().await {
            match request {
                ActivationRequest::FocusWindow => {
                    match window.update(cx, |_, window, _| show_and_activate_window(window)) {
                        Ok(true) => {}
                        Ok(false) | Err(_) => {
                            tracing::warn!(event = "SINGLE_INSTANCE_ACTIVATION_FAILED");
                        }
                    }
                }
                ActivationRequest::External(activation) => {
                    match window.update(cx, |_, window, cx| {
                        if !show_and_activate_window(window) {
                            return (false, false);
                        }
                        let queued = root.update(cx, |root, cx| {
                            root.queue_external_activation(activation, window, cx)
                        });
                        (true, queued)
                    }) {
                        Ok((true, true)) => {}
                        Ok((true, false)) => {
                            tracing::warn!(event = "EXTERNAL_ACTIVATION_REJECTED_BY_ROOT")
                        }
                        Ok((false, _)) => {
                            tracing::warn!(event = "EXTERNAL_ACTIVATION_WINDOW_RESTORE_FAILED");
                        }
                        Err(_) => {
                            tracing::warn!(event = "EXTERNAL_ACTIVATION_ROOT_DISPATCH_FAILED");
                        }
                    }
                }
            }
        }
    })
    .detach();
}

/// 将宿主结果投影到 Root 生命周期；退出阶段独立留痕，不把宿主清理成功等同于原生事件循环已结束。
/// @author kongweiguang
fn apply_host_response(
    root_entity: gpui_kit::Entity<RootView>,
    event: RootViewEvent,
    response: kerminal_application::AppResult<HostResponse>,
    app: &mut App,
) {
    match response {
        Ok(HostResponse::Quit { intent, result }) => match result {
            Ok(()) => {
                tracing::info!(event = "DESKTOP_NATIVE_QUIT_BEGIN");
                app.quit();
                tracing::info!(event = "DESKTOP_NATIVE_QUIT_REQUEST_QUEUED");
            }
            Err(message) => {
                root_entity.update(app, |root, cx| {
                    root.report_quit_failure(intent, message, cx);
                });
            }
        },
        Ok(HostResponse::UpdateQuiesceFailed {
            update_id,
            version,
            message,
        }) => {
            root_entity.update(app, |root, cx| {
                root.report_update_quiesce_failure(update_id, &version, message, cx);
            });
        }
        Ok(HostResponse::UpdateCompleted {
            update_id,
            version,
            result,
            action,
        }) => {
            let should_quit = matches!(&action, UpdateHostAction::Quit);
            root_entity.update(app, |root, cx| {
                match action {
                    #[cfg(target_os = "windows")]
                    UpdateHostAction::ListenerRecovery(PrimaryListenerRecovery::Restored) => {
                        root.complete_update_install(update_id, result, cx);
                    }
                    #[cfg(target_os = "windows")]
                    UpdateHostAction::ListenerRecovery(PrimaryListenerRecovery::Occupied) => {
                        root.report_update_listener_recovery_failure(
                            update_id,
                            &version,
                            result,
                            "更新后，Kerminal 单实例身份已由另一个进程占用。请关闭当前窗口并手动重新启动。",
                            cx,
                        );
                    }
                    #[cfg(target_os = "windows")]
                    UpdateHostAction::ListenerRecovery(PrimaryListenerRecovery::Failed(error)) => {
                        let safe_message = format!(
                            "更新后无法恢复 Kerminal 单实例监听器：{}。请关闭当前窗口并手动重新启动。",
                            error.public_message()
                        );
                        root.report_update_listener_recovery_failure(
                            update_id,
                            &version,
                            result,
                            safe_message,
                            cx,
                        );
                    }
                    #[cfg(not(target_os = "windows"))]
                    UpdateHostAction::RestartFailed(message) => {
                        root.complete_update_install(update_id, result, cx);
                        root.report_update_restart_failure(update_id, &version, message, cx);
                    }
                    UpdateHostAction::KeepOpen | UpdateHostAction::Quit => {
                        root.complete_update_install(update_id, result, cx);
                    }
                }
            });
            if should_quit {
                app.quit();
            }
        }
        Ok(HostResponse::Finalized(_)) => {
            let _ = event;
        }
        Err(error) => {
            let message = error.public_message().to_owned();
            root_entity.update(app, |root, cx| match event {
                RootViewEvent::QuitReady { intent } => {
                    root.report_quit_failure(intent, message, cx)
                }
                RootViewEvent::InstallReady { update_id, version }
                | RootViewEvent::RetryInstallReady { update_id, version } => {
                    root.report_update_quiesce_failure(update_id, &version, message, cx)
                }
            });
        }
    }
}

/// 注册 macOS URL 打开回调；只把原生 URL 封装为参数，业务解析仍由 Root 的同一 service 执行。
#[cfg(target_os = "macos")]
fn install_open_url_listener(
    cx: &App,
    sender: ActivationQueueSender,
    runtime: tokio::runtime::Handle,
) {
    let executable = match std::env::current_exe() {
        Ok(path) => path.into_os_string(),
        Err(_) => {
            tracing::warn!(event = "MACOS_URL_LAUNCH_TARGET_UNAVAILABLE");
            return;
        }
    };
    cx.on_open_urls(move |urls| {
        for url in urls {
            let activation =
                activation_from_os([executable.clone(), std::ffi::OsString::from(url)], None);
            match activation {
                Ok(Some(activation)) => {
                    let sender = sender.clone();
                    runtime.spawn(async move {
                        if let Err(error) =
                            sender.submit(ActivationRequest::External(activation)).await
                        {
                            if let Some(raw_os_error) = source_raw_os_error(&error) {
                                tracing::warn!(
                                    event = "MACOS_URL_ACTIVATION_QUEUE_REJECTED",
                                    code = error.public_code().as_str(),
                                    raw_os_error,
                                );
                            } else {
                                tracing::warn!(
                                    event = "MACOS_URL_ACTIVATION_QUEUE_REJECTED",
                                    code = error.public_code().as_str(),
                                );
                            }
                        }
                    });
                }
                Ok(None) => {}
                Err(_) => tracing::warn!(event = "MACOS_URL_ACTIVATION_INVALID"),
            }
        }
    });
}

/// 保存主窗口创建失败原因，退出清理后由进程入口给出可读诊断。
fn record_startup_error(error: &StartupError, message: String) {
    if let Ok(mut error) = error.lock() {
        *error = Some(message);
    }
}

/// 取出 UI 初始化失败消息；毒化时返回固定错误而不让启动问题静默消失。
fn take_startup_error(error: &StartupError) -> Option<String> {
    match error.lock() {
        Ok(mut error) => error.take(),
        Err(_) => Some("桌面启动诊断状态不可用".to_owned()),
    }
}

#[cfg(test)]
#[path = "../tests/support/instance_identity_unit.rs"]
mod tests;
