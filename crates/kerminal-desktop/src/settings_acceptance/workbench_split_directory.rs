//! 隔离原生窗口中的本机分屏目录验收。
//!
//! @author kongweiguang

use super::*;
use kerminal_application::sessions::SessionFilter;
use kerminal_domain::{identity::SessionId, sessions::SessionLifecycle};

const SPLIT_DIRECTORY_ENV: &str = "KERMINAL_SPLIT_DIRECTORY_ACCEPTANCE";
type PaneObservation = (String, String, Option<String>);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SplitDirectoryReport {
    schema_version: u32,
    capture_mode: &'static str,
    capture_backend: &'static str,
    native_isolation: crate::right_rail_acceptance::isolation::DesktopIsolationEvidence,
    config_directory: String,
    local_profile_shell: String,
    window_visible: bool,
    window_active: bool,
    window_width: f32,
    window_height: f32,
    checks: Vec<SplitDirectoryCheck>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SplitDirectoryCheck {
    theme: &'static str,
    effective_theme: String,
    direction: &'static str,
    expected_directory: String,
    source_directory: String,
    split_directory: String,
    source_pane_id: String,
    source_session_id: String,
    split_pane_id: String,
    split_session_id: String,
    closed_session_removed: bool,
    source_session_preserved: bool,
    capture: CaptureRecord,
}

/// 专项入口必须由验收启动器显式开启，普通工作台验收不执行额外的 shell 命令。
///
/// @author kongweiguang
pub(super) fn requested() -> bool {
    std::env::var_os(SPLIT_DIRECTORY_ENV).as_deref() == Some(OsStr::new("1"))
}

/// Shell 历史和临时文件须指向本轮配置根；启动前验证启动器设置并创建其 KnownFolder 目录。
///
/// @author kongweiguang
pub(super) fn prepare_isolated_profile(config_directory: &Path) -> Result<(), Box<dyn Error>> {
    if cfg!(windows) {
        for (name, expected) in [
            ("USERPROFILE", config_directory.to_path_buf()),
            ("APPDATA", config_directory.join("AppData/Roaming")),
            ("LOCALAPPDATA", config_directory.join("AppData/Local")),
        ] {
            if std::env::var_os(name).map(PathBuf::from).as_ref() != Some(&expected) {
                return Err(io::Error::other(format!(
                    "split acceptance requires {name} inside its isolated config directory"
                ))
                .into());
            }
        }
    }
    for directory in ["AppData/Local", "AppData/Roaming", "Documents"] {
        fs::create_dir_all(config_directory.join(directory))?;
    }
    Ok(())
}

/// 使用真实 PowerShell、生产分屏按钮和独立会话回读核对目录，关闭后再核对注册表回收。
/// 验收只访问本次输出目录和隔离配置，窗口与进程回收仍由工作台验收的组合根负责。
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
    if std::env::var_os(WORKBENCH_POWERSHELL_ENV).is_none() {
        return Err(
            io::Error::other("split acceptance requires an explicit PowerShell path").into(),
        );
    }
    prepare_capture_surface(cx, window, context.width, context.height).await?;
    let source_pane = first_session
        .active_local_pane_id
        .ok_or_else(|| io::Error::other("split acceptance source pane is unavailable"))?;
    let source_session = first_session
        .ready_session_ids
        .first()
        .ok_or_else(|| io::Error::other("split acceptance source session is unavailable"))?
        .clone();
    let source_session_id = SessionId::parse(&source_session)?;
    wait_for_panes(cx, root, window, "initial PowerShell prompt", |panes| {
        panes.len() == 1
            && panes[0].0 == source_pane
            && panes[0].1 == source_session
            && panes[0].2.is_some()
    })
    .await?;

    let mut checks = Vec::new();
    for (theme, name, selector) in THEMES {
        let effective_theme =
            change_theme(cx, root, window, services, theme, name, selector).await?;
        let output_directory = if context.output_directory.is_absolute() {
            context.output_directory.clone()
        } else {
            std::env::current_dir()?.join(&context.output_directory)
        };
        let directory = output_directory
            .join("分屏 dir's")
            .join(format!("主题 {name}"));
        fs::create_dir_all(&directory)?;
        let directory_text = directory
            .to_str()
            .ok_or_else(|| io::Error::other("acceptance directory is not valid Unicode"))?;
        let expected = directory_text.replace('\\', "/");
        let command = format!(
            "Set-Location -LiteralPath '{}'\r",
            directory_text.replace('\'', "''")
        );
        let sessions = services.sessions.clone();
        let session_id = source_session_id.clone();
        services
            .executor
            .clone()
            .run(move |_| async move { sessions.write(session_id, command.into_bytes()).await })
            .await?;
        wait_for_panes(cx, root, window, "source directory change", |panes| {
            panes.len() == 1
                && panes[0].0 == source_pane
                && panes[0].1 == source_session
                && directory_matches(panes[0].2.as_deref(), &expected)
        })
        .await?;

        for direction in ["horizontal", "vertical"] {
            click_selector(
                cx,
                window,
                &format!("pane-split-{direction}-after-{source_pane}-trigger"),
            )?;
            let panes = wait_for_panes(cx, root, window, "split directory inheritance", |panes| {
                panes.len() == 2
                    && panes.iter().any(|pane| {
                        pane.0 == source_pane
                            && pane.1 == source_session
                            && directory_matches(pane.2.as_deref(), &expected)
                    })
                    && panes.iter().any(|pane| {
                        pane.0 != source_pane
                            && pane.1 != source_session
                            && directory_matches(pane.2.as_deref(), &expected)
                    })
            })
            .await?;
            let source_directory = panes
                .iter()
                .find(|pane| pane.0 == source_pane)
                .and_then(|pane| pane.2.clone())
                .ok_or_else(|| io::Error::other("source directory was not observed"))?;
            let split = panes
                .into_iter()
                .find(|pane| pane.0 != source_pane)
                .ok_or_else(|| io::Error::other("split pane was not found"))?;
            let split_directory = split
                .2
                .clone()
                .ok_or_else(|| io::Error::other("split directory was not observed"))?;
            let frame = save_frame(
                cx,
                window,
                context
                    .output_directory
                    .join(format!("split-{name}-{direction}.png")),
            )?;
            verify_capture_dimensions(
                cx,
                window,
                context.width,
                context.height,
                std::slice::from_ref(&frame),
            )?;
            click_selector(cx, window, &format!("pane-close-{}", split.0))?;
            wait_for_panes(cx, root, window, "close split pane", |panes| {
                panes.len() == 1
                    && panes[0].0 == source_pane
                    && panes[0].1 == source_session
                    && directory_matches(panes[0].2.as_deref(), &expected)
            })
            .await?;
            wait_for_session_cleanup(
                cx,
                services,
                &source_session_id,
                &SessionId::parse(&split.1)?,
            )
            .await?;
            wait_for_workbench_state(
                cx,
                root,
                window,
                &context.fixture,
                Instant::now() + WORKBENCH_ACTION_TIMEOUT,
                "split close acknowledged",
                |state| {
                    state.local_pane_count == 1
                        && state.ready_session_ids == [source_session.clone()]
                        && state.failed_pane_ids.is_empty()
                },
            )
            .await?;
            checks.push(SplitDirectoryCheck {
                theme: name,
                effective_theme: effective_theme.clone(),
                direction,
                expected_directory: expected.clone(),
                source_directory,
                split_directory,
                source_pane_id: source_pane.clone(),
                source_session_id: source_session.clone(),
                split_pane_id: split.0,
                split_session_id: split.1,
                closed_session_removed: true,
                source_session_preserved: true,
                capture: frame,
            });
        }
    }

    let (window_visible, window_active, window_width, window_height) =
        window.update(cx, |_, window, _| {
            ensure_hidden_window(window)?;
            Ok::<_, io::Error>((
                window.is_visible(),
                window.is_window_active(),
                window.bounds().size.width.as_f32(),
                window.bounds().size.height.as_f32(),
            ))
        })??;
    let native_isolation = crate::right_rail_acceptance::isolation::capture()?;
    native_isolation.validate_window_state(window_visible, window_active)?;
    let report = SplitDirectoryReport {
        schema_version: 1,
        capture_mode: "hidden-native-window",
        capture_backend: "gpui-root-click-and-live-pty-readback",
        native_isolation,
        config_directory: context.config_directory.to_string_lossy().into_owned(),
        local_profile_shell: acceptance_profile_shell(),
        window_visible,
        window_active,
        window_width,
        window_height,
        checks,
    };
    fs::write(
        context.output_directory.join(WORKBENCH_REPORT_FILE),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}

/// 隐藏窗口不会自然收到绘制事件；真实改变尺寸并投递原生绘制，等待 GPU 目标同步后才开始验收。
///
/// @author kongweiguang
pub(super) async fn prepare_capture_surface(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    width: u32,
    height: u32,
) -> Result<(), Box<dyn Error>> {
    prepare_capture_surface_for_title(cx, window, width, height, "Kerminal Workbench Acceptance")
        .await
}

/// 不同隐藏验收窗口共享同一原生初始化门禁；标题只用于定位后再核对当前进程和 UI 线程。
///
/// @author kongweiguang
pub(crate) async fn prepare_capture_surface_for_title(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    width: u32,
    height: u32,
    window_title: &str,
) -> Result<(), Box<dyn Error>> {
    for (target_width, target_height) in [(width + 4, height + 4), (width, height)] {
        resize_acceptance_window(cx, window, target_width, target_height).await?;
        request_hidden_native_paint(window_title)?;
        let deadline = Instant::now() + WINDOW_RESIZE_TIMEOUT;
        loop {
            let (actual_width, actual_height, expected_width, expected_height) =
                window.update(cx, |_, window, app| {
                    ensure_hidden_window(window)?;
                    draw_acceptance_frame(window, app);
                    let scale = window.scale_factor();
                    if !scale.is_finite() || scale <= 0.0 {
                        return Err(io::Error::other("invalid hidden window scale factor"));
                    }
                    let image = window.render_to_image().map_err(io::Error::other)?;
                    Ok::<_, io::Error>((
                        image.width(),
                        image.height(),
                        (target_width as f32 * scale).round() as u32,
                        (target_height as f32 * scale).round() as u32,
                    ))
                })??;
            if actual_width == expected_width && actual_height == expected_height {
                break;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(format!(
                    "hidden GPU surface size mismatch: expected={expected_width}x{expected_height}, observed={actual_width}x{actual_height}"
                ))
                .into());
            }
            cx.background_executor().timer(POLL_INTERVAL).await;
        }
    }
    Ok(())
}

/// 异步触发自身验收窗口的正常 WM_PAINT；线程与进程校验避免误操作其他窗口或重入 GPUI。
///
/// @author kongweiguang
#[cfg(windows)]
fn request_hidden_native_paint(window_title: &str) -> Result<(), Box<dyn Error>> {
    use windows_sys::Win32::{
        System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
        UI::WindowsAndMessaging::{FindWindowW, GetWindowThreadProcessId, PostMessageW, WM_PAINT},
    };

    let title: Vec<u16> = window_title
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: 标题以 NUL 结尾；只向核验过的当前进程和 UI 线程窗口排队无参数绘制消息。
    unsafe {
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if hwnd.is_null() {
            return Err(io::Error::other("hidden workbench window was not found").into());
        }
        let mut process_id = 0;
        let thread_id = GetWindowThreadProcessId(hwnd, &mut process_id);
        if process_id != GetCurrentProcessId() || thread_id != GetCurrentThreadId() {
            return Err(
                io::Error::other("hidden workbench window belongs to another owner").into(),
            );
        }
        if PostMessageW(hwnd, WM_PAINT, 0, 0) == 0 {
            return Err(io::Error::last_os_error().into());
        }
    }
    Ok(())
}

/// 非 Windows 平台沿用原生窗口自己的绘制调度；尺寸门禁仍会拒绝无效截图。
///
/// @author kongweiguang
#[cfg(not(windows))]
fn request_hidden_native_paint(_window_title: &str) -> Result<(), Box<dyn Error>> {
    Ok(())
}

/// 从设置窗口切换并回读主题，保证截图和交互使用真正保存的用户主题状态。
///
/// @author kongweiguang
pub(super) async fn change_theme(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    theme: ThemeMode,
    name: &str,
    selector: &str,
) -> Result<String, Box<dyn Error>> {
    press_key(cx, window, "ctrl-alt-s")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open() && state.settings_ready() && state.draft_ready()
    })
    .await?;
    ensure_page(cx, root, window, "appearance", "appearance").await?;
    reveal_settings_selector(cx, root, window, "appearance", selector).await?;
    click_selector(cx, window, selector)?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.draft_theme_mode() == Some(name) && !state.draft_dirty() && state.draft_ready()
    })
    .await?;
    if load_persisted_settings(services).await?.settings.theme_mode != theme {
        return Err(io::Error::other(format!("split theme {name} was not saved")).into());
    }
    click_selector(cx, window, "preferences-close")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        !state.modal_open()
    })
    .await?;
    let effective = window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        Ok::<_, io::Error>(if gpui_kit::component::Theme::global(app).is_dark() {
            "dark"
        } else {
            "light"
        })
    })??;
    if (theme == ThemeMode::Light && effective != "light")
        || (theme == ThemeMode::Dark && effective != "dark")
    {
        return Err(io::Error::other(format!("split theme {name} rendered as {effective}")).into());
    }
    Ok(effective.to_owned())
}

/// Windows 的绝对路径比较保留 Unicode 并统一分隔符和盘符大小写，不读取磁盘推测终端目录。
///
/// @author kongweiguang
fn directory_matches(observed: Option<&str>, expected: &str) -> bool {
    observed.is_some_and(|path| path.replace('\\', "/").eq_ignore_ascii_case(expected))
}

/// 等待生产 Root 接收 OSC7 并完成布局，超时给出真实 pane 投影而非伪造成功。
///
/// @author kongweiguang
async fn wait_for_panes<F>(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    stage: &str,
    mut condition: F,
) -> Result<Vec<PaneObservation>, Box<dyn Error>>
where
    F: FnMut(&[PaneObservation]) -> bool,
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
                "split acceptance timed out during {stage}: panes={panes:?}"
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 只在会话注册表确认新会话已移除且来源仍运行后记录回收，避免将 UI 消失当作关闭成功。
///
/// @author kongweiguang
async fn wait_for_session_cleanup(
    cx: &mut AsyncApp,
    services: &kerminal_ui::AppServices,
    source_id: &SessionId,
    closed_id: &SessionId,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + WORKBENCH_ACTION_TIMEOUT;
    loop {
        let sessions = services.sessions.clone();
        let observed = services
            .executor
            .clone()
            .run(move |_| async move { sessions.list(SessionFilter::default()).await })
            .await?;
        let source_running = observed.iter().any(|entry| {
            &entry.session_id == source_id && entry.lifecycle == SessionLifecycle::Running
        });
        let closed_removed = observed.iter().all(|entry| &entry.session_id != closed_id);
        if source_running && closed_removed {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "split session cleanup timed out: source_running={source_running}, closed_removed={closed_removed}"
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}
