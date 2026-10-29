//! 独立的应用配色原生验收；所有输入、设置保存和终端均走生产路径。
//!
//! @author kongweiguang

use std::collections::{BTreeMap, BTreeSet};

use gpui_pre::WindowAppearance;
use kerminal_application::preferences::{
    AppColorScheme, ProfileDraft, ProfileRevision, TerminalColorScheme,
};

use super::*;

const APP_THEME_ENV: &str = "KERMINAL_APP_THEME_ACCEPTANCE";
const APP_THEME_REPORT: &str = "app-theme-report.json";
const PROFILE_ID: &str = "app-theme-acceptance";

struct ThemeContext {
    output_directory: PathBuf,
    width: u32,
    height: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThemeReport {
    schema_version: u32,
    capture_mode: &'static str,
    native_isolation: crate::right_rail_acceptance::isolation::DesktopIsolationEvidence,
    config_directory: String,
    terminal_session_id: String,
    terminal_settings_unchanged: bool,
    cases: Vec<ThemeCase>,
    rapid_selection: RapidCheck,
    keyboard: KeyboardCheck,
    narrow_window: NarrowCheck,
    screenshots: Vec<CaptureRecord>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThemeCase {
    mode: &'static str,
    scheme: &'static str,
    effective_dark: bool,
    kit_theme_fingerprint: String,
    terminal_palette_generation: u64,
    settings_capture: String,
    workbench_capture: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KeyboardCheck {
    tab_focused_kerminal: bool,
    right_selected_jetbrains: bool,
    tab_focused_fleet: bool,
    space_selected_fleet: bool,
    escape_closed_and_restored_focus: bool,
    reopened_fleet: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RapidCheck {
    requested: Vec<&'static str>,
    persisted: &'static str,
    terminal_unchanged: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NarrowCheck {
    width: u32,
    height: u32,
    all_choices_inside_content: bool,
    choices_do_not_overlap: bool,
    screenshot: String,
}

/// 仅显式选择主题专项时进入该验收，普通设置验收保持既有完整流程。
///
/// @author kongweiguang
pub(super) fn requested() -> bool {
    std::env::var_os(APP_THEME_ENV).as_deref() == Some(OsStr::new("1"))
}

/// 使用全新配置根和真实服务组合；窗口退出后仍等待相同的资源回收流程。
///
/// @author kongweiguang
pub(super) fn run() -> Result<(), Box<dyn Error>> {
    let options = requested_options()?;
    let output_directory = match options.output_directory {
        Some(path) => create_requested_output_directory(&path)?,
        None => create_unique_output_directory(
            Path::new("target")
                .join("diagnostics")
                .join("app-theme-acceptance"),
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
    let ui_result = match runtime.block_on(prepare_fixture(&services)) {
        Ok(()) => {
            let context = ThemeContext {
                output_directory: output_directory.clone(),
                width: options.width.unwrap_or(DEFAULT_WIDTH),
                height: options.height.unwrap_or(DEFAULT_HEIGHT),
            };
            catch_unwind(AssertUnwindSafe(|| run_hidden_window(services, context)))
                .unwrap_or_else(|_| Err(io::Error::other("app theme acceptance panicked").into()))
        }
        Err(error) => Err(error.into()),
    };
    let shutdown_result = runtime
        .block_on(composition.shutdown_until(tokio::time::Instant::now() + SHUTDOWN_TIMEOUT));
    runtime.shutdown();
    ui_result?;
    shutdown_result?;
    println!(
        "App theme report: {}",
        output_directory.join(APP_THEME_REPORT).display()
    );
    Ok(())
}

/// 为真实本地 PTY 建立唯一默认 Profile，并预设不同的浅深终端色盘以检测误联动。
///
/// @author kongweiguang
async fn prepare_fixture(
    services: &kerminal_ui::AppServices,
) -> kerminal_application::AppResult<()> {
    let profiles = services.profiles.clone();
    services
        .executor
        .run(move |_| async move {
            let profile = profiles
                .save_profile(
                    ProfileDraft {
                        id: Some(PROFILE_ID.to_owned()),
                        name: "App theme acceptance".to_owned(),
                        shell: if cfg!(windows) { "cmd.exe" } else { "/bin/sh" }.to_owned(),
                        args: if cfg!(windows) {
                            vec!["/Q".to_owned()]
                        } else {
                            Vec::new()
                        },
                        cwd: None,
                        env: BTreeMap::new(),
                        sidebar_group_id: None,
                        sort_order: 0,
                    },
                    None,
                )
                .await?;
            profiles
                .set_default_profile(
                    PROFILE_ID,
                    vec![ProfileRevision {
                        profile_id: profile.profile.id,
                        revision: profile.revision,
                    }],
                )
                .await?;
            Ok::<(), kerminal_application::AppError>(())
        })
        .await?;

    let preferences = services.preferences.clone();
    services
        .executor
        .run(move |_| async move {
            let versioned = preferences.load_settings().await?;
            let mut settings = versioned.settings;
            settings.terminal.color_scheme = TerminalColorScheme::TokyoNight;
            settings.terminal.light_color_scheme = TerminalColorScheme::Github;
            settings.terminal.dark_color_scheme = TerminalColorScheme::Solarized;
            preferences
                .save_settings(settings, versioned.revision)
                .await?;
            Ok::<(), kerminal_application::AppError>(())
        })
        .await
}

/// 在隔离桌面创建隐藏的生产 Root；最小尺寸允许后续窄窗验收。
///
/// @author kongweiguang
fn run_hidden_window(
    services: kerminal_ui::AppServices,
    context: ThemeContext,
) -> Result<(), Box<dyn Error>> {
    let failure = Arc::new(Mutex::new(None::<String>));
    let failure_for_app = Arc::clone(&failure);
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            kerminal_ui::init(cx);
            let bounds = match acceptance_bounds(cx, context.width, context.height) {
                Ok(bounds) => bounds,
                Err(error) => {
                    set_failure(&failure_for_app, error.to_string());
                    cx.quit();
                    return;
                }
            };
            let options = WindowOptions {
                window_background: WindowBackgroundAppearance::Transparent,
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(MIN_WIDTH as f32), px(MIN_HEIGHT as f32))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Kerminal App Theme Acceptance".into()),
                    ..TitleBar::title_bar_options()
                }),
                show: false,
                focus: false,
                ..TitleBar::window_options()
            };
            let result = gpui_kit::open_window(options, cx, |window, cx| {
                window.set_background_appearance(WindowBackgroundAppearance::Transparent);
                window.resize(size(px(context.width as f32), px(context.height as f32)));
                let root = cx.new(|cx| RootView::new(services.clone(), window, cx));
                if let Err(error) =
                    crate::right_rail_acceptance::isolation::capture().and_then(|evidence| {
                        evidence
                            .validate_window_state(window.is_visible(), window.is_window_active())
                    })
                {
                    set_failure(
                        &failure_for_app,
                        format!("app theme window isolation failed: {error}"),
                    );
                    cx.quit();
                    return root;
                }
                let handle = window.window_handle();
                let capture_root = root.clone();
                let capture_services = services.clone();
                let capture_failure = Arc::clone(&failure_for_app);
                cx.spawn(async move |cx| {
                    if let Err(error) =
                        capture_theme(cx, capture_root, handle, capture_services, context).await
                    {
                        set_failure(&capture_failure, error.to_string());
                    }
                    cx.update(|cx| cx.quit());
                })
                .detach();
                root
            });
            if let Err(error) = result {
                set_failure(&failure_for_app, error.to_string());
                cx.quit();
            }
        });
    if let Some(message) = failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        return Err(io::Error::other(message).into());
    }
    Ok(())
}

/// 验证每种应用色板的生产单选、Kit 投影、磁盘回读及终端身份，截图保留可审阅证据。
///
/// @author kongweiguang
async fn capture_theme(
    cx: &mut AsyncApp,
    root: Entity<RootView>,
    window: AnyWindowHandle,
    services: kerminal_ui::AppServices,
    context: ThemeContext,
) -> Result<(), Box<dyn Error>> {
    wait_for_state(cx, &root, window, Instant::now() + READY_TIMEOUT, |state| {
        state.settings_ready() && state.workspace_ready()
    })
    .await?;
    resize_acceptance_window(cx, window, context.width, context.height).await?;
    let before_session = root_state(cx, &root, window)?;
    if before_session.ready_local_session_ids().is_empty() {
        click_selector(cx, window, "workspace-tabs-create-terminal")?;
    }
    let started = wait_for_state(cx, &root, window, Instant::now() + READY_TIMEOUT, |state| {
        state.ready_local_session_ids().len() == 1 && state.rendered_terminal_palette().is_some()
    })
    .await?;
    let session_id = started.ready_local_session_ids()[0].clone();
    let terminal_settings = load_persisted_settings(&services).await?.settings.terminal;
    click_selector(cx, window, "workbench-tool-snippets")?;
    wait_for_state(
        cx,
        &root,
        window,
        Instant::now() + ACTION_TIMEOUT,
        |state| state.snippets_tool_open(),
    )
    .await?;
    let mut screenshots = vec![save_frame(
        cx,
        window,
        context.output_directory.join("workbench-initial.png"),
    )?];
    press_key(cx, window, "ctrl-alt-s")?;
    wait_for_state(
        cx,
        &root,
        window,
        Instant::now() + ACTION_TIMEOUT,
        |state| state.modal_open() && state.settings_ready() && state.draft_ready(),
    )
    .await?;
    let mut cases = Vec::new();

    for (mode, mode_name, mode_selector) in THEMES {
        ensure_page(cx, &root, window, "appearance", "appearance").await?;
        reveal_settings_selector(cx, &root, window, "appearance", mode_selector).await?;
        click_selector(cx, window, mode_selector)?;
        wait_for_state(
            cx,
            &root,
            window,
            Instant::now() + ACTION_TIMEOUT,
            |state| {
                state.draft_theme_mode() == Some(mode_name)
                    && state.draft_ready()
                    && !state.draft_dirty()
            },
        )
        .await?;
        let mode_baseline = wait_for_state(
            cx,
            &root,
            window,
            Instant::now() + ACTION_TIMEOUT,
            |state| state.rendered_terminal_palette() == Some(state.terminal_palette()),
        )
        .await?;
        if mode_baseline.ready_local_session_ids() != std::slice::from_ref(&session_id) {
            return Err(io::Error::other("mode change replaced the local terminal session").into());
        }
        let generation = mode_baseline.terminal_palette_update_generation();
        let terminal_palette = *mode_baseline.terminal_palette();
        let rendered_palette = *mode_baseline
            .rendered_terminal_palette()
            .ok_or_else(|| io::Error::other("active terminal has no rendered palette"))?;
        let mut fingerprints = BTreeSet::new();

        for scheme in AppColorScheme::ALL {
            let selector = format!("app-color-{}", scheme.as_str());
            reveal_settings_selector(cx, &root, window, "appearance", &selector).await?;
            click_selector(cx, window, &selector)?;
            let current = wait_for_state_with_stage(
                cx,
                &root,
                window,
                Instant::now() + ACTION_TIMEOUT,
                &format!("{mode_name} app palette {}", scheme.as_str()),
                |state| {
                    state.draft_app_color_scheme() == Some(scheme.as_str())
                        && state.applied_app_color_scheme() == Some(scheme.as_str())
                        && state.kit_theme_matches_app_color()
                        && state.draft_ready()
                        && !state.draft_dirty()
                },
            )
            .await?;
            if current.terminal_palette_update_generation() != generation
                || current.terminal_palette() != &terminal_palette
                || current.rendered_terminal_palette() != Some(&rendered_palette)
                || current.ready_local_session_ids() != std::slice::from_ref(&session_id)
            {
                return Err(io::Error::other(format!(
                    "{mode_name}/{} altered terminal palette or session",
                    scheme.as_str()
                ))
                .into());
            }
            let persisted = load_persisted_settings(&services).await?.settings;
            if persisted.app_color_scheme != scheme
                || persisted.theme_mode != mode
                || persisted.terminal != terminal_settings
            {
                return Err(io::Error::other(format!(
                    "{mode_name}/{} settings readback mismatch",
                    scheme.as_str()
                ))
                .into());
            }
            let (effective_dark, appearance_dark) = window.update(cx, |_, window, app| {
                ensure_hidden_window(window)?;
                Ok::<_, io::Error>((
                    gpui_kit::component::Theme::global(app).is_dark(),
                    matches!(
                        window.appearance(),
                        WindowAppearance::Dark | WindowAppearance::VibrantDark
                    ),
                ))
            })??;
            let expected_dark = match mode {
                ThemeMode::Dark => true,
                ThemeMode::Light => false,
                ThemeMode::System => appearance_dark,
            };
            if effective_dark != expected_dark {
                return Err(io::Error::other(format!(
                    "{mode_name}/{} resolved wrong light/dark theme",
                    scheme.as_str()
                ))
                .into());
            }
            assert_choice_checked(cx, window, scheme)?;
            fingerprints.insert(current.kit_theme_fingerprint().to_owned());
            let settings_file = format!("settings-{mode_name}-{}.png", scheme.as_str());
            screenshots.push(save_frame(
                cx,
                window,
                context.output_directory.join(&settings_file),
            )?);
            if matches!(scheme, AppColorScheme::Kerminal | AppColorScheme::Claude) {
                capture_popup(
                    cx,
                    &root,
                    window,
                    &context,
                    mode_name,
                    scheme,
                    &mut screenshots,
                )
                .await?;
            }
            click_selector(cx, window, "preferences-close")?;
            wait_for_state(
                cx,
                &root,
                window,
                Instant::now() + ACTION_TIMEOUT,
                |state| !state.modal_open() && state.snippets_tool_open(),
            )
            .await?;
            let workbench_file = format!("workbench-{mode_name}-{}.png", scheme.as_str());
            screenshots.push(save_frame(
                cx,
                window,
                context.output_directory.join(&workbench_file),
            )?);
            press_key(cx, window, "ctrl-alt-s")?;
            wait_for_state(
                cx,
                &root,
                window,
                Instant::now() + ACTION_TIMEOUT,
                |state| {
                    state.modal_open()
                        && state.draft_ready()
                        && state.draft_app_color_scheme() == Some(scheme.as_str())
                },
            )
            .await?;
            cases.push(ThemeCase {
                mode: mode_name,
                scheme: scheme.as_str(),
                effective_dark,
                kit_theme_fingerprint: current.kit_theme_fingerprint().to_owned(),
                terminal_palette_generation: generation,
                settings_capture: settings_file,
                workbench_capture: workbench_file,
            });
        }
        if fingerprints.len() != AppColorScheme::ALL.len() {
            return Err(io::Error::other(format!(
                "{mode_name} did not resolve distinct Kit palettes for every scheme"
            ))
            .into());
        }
    }
    let rapid_selection = verify_rapid_selection(
        cx,
        &root,
        window,
        &services,
        &terminal_settings,
        &session_id,
    )
    .await?;
    let keyboard = verify_keyboard(
        cx,
        &root,
        window,
        &services,
        &terminal_settings,
        &session_id,
    )
    .await?;
    let narrow_window = verify_narrow_window(cx, &root, window, &context, &mut screenshots).await?;
    let final_settings = load_persisted_settings(&services).await?.settings;
    if final_settings.terminal != terminal_settings {
        return Err(
            io::Error::other("terminal settings changed during app theme acceptance").into(),
        );
    }
    let native_isolation = window.update(cx, |_, window, _| {
        let evidence = crate::right_rail_acceptance::isolation::capture()?;
        evidence.validate_window_state(window.is_visible(), window.is_window_active())?;
        Ok::<_, io::Error>(evidence)
    })??;
    let report = ThemeReport {
        schema_version: 1,
        capture_mode: "hidden_native_window",
        native_isolation,
        config_directory: context
            .output_directory
            .join("config")
            .display()
            .to_string(),
        terminal_session_id: session_id,
        terminal_settings_unchanged: true,
        cases,
        rapid_selection,
        keyboard,
        narrow_window,
        screenshots,
    };
    fs::write(
        context.output_directory.join(APP_THEME_REPORT),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}

/// 在单个防抖周期内连续请求不同色板，只有末次意图允许成为持久结果。
///
/// @author kongweiguang
async fn verify_rapid_selection(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    terminal_settings: &kerminal_application::preferences::TerminalSettings,
    session_id: &str,
) -> Result<RapidCheck, Box<dyn Error>> {
    ensure_page(cx, root, window, "appearance", "appearance").await?;
    reveal_settings_selector(cx, root, window, "appearance", "app-color-kerminal").await?;
    let initial = root_state(cx, root, window)?;
    let generation = initial.terminal_palette_update_generation();
    let palette = *initial.terminal_palette();
    let rendered_palette = *initial
        .rendered_terminal_palette()
        .ok_or_else(|| io::Error::other("rapid check has no rendered terminal palette"))?;
    let choices = [
        AppColorScheme::Kerminal,
        AppColorScheme::JetBrains,
        AppColorScheme::Xcode,
        AppColorScheme::Fleet,
    ];
    let started = Instant::now();
    for scheme in choices {
        click_selector(cx, window, &format!("app-color-{}", scheme.as_str()))?;
    }
    if started.elapsed() >= Duration::from_millis(400) {
        return Err(
            io::Error::other("rapid palette actions exceeded the 400ms debounce window").into(),
        );
    }
    let saved = wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.draft_app_color_scheme() == Some("fleet")
            && state.applied_app_color_scheme() == Some("fleet")
            && state.kit_theme_matches_app_color()
            && state.draft_ready()
            && !state.draft_dirty()
    })
    .await?;
    let persisted = load_persisted_settings(services).await?.settings;
    let terminal_unchanged = persisted.terminal == *terminal_settings
        && saved.terminal_palette() == &palette
        && saved.rendered_terminal_palette() == Some(&rendered_palette)
        && saved.terminal_palette_update_generation() == generation
        && saved.ready_local_session_ids().len() == 1
        && saved.ready_local_session_ids()[0] == session_id;
    if persisted.app_color_scheme != AppColorScheme::Fleet || !terminal_unchanged {
        return Err(io::Error::other(
            "rapid palette changes lost the last intent or changed the terminal",
        )
        .into());
    }
    Ok(RapidCheck {
        requested: choices.into_iter().map(AppColorScheme::as_str).collect(),
        persisted: persisted.app_color_scheme.as_str(),
        terminal_unchanged,
    })
}

/// 通过可访问名称查询真实 Radio，选中和焦点都来自最近一次生产渲染。
///
/// @author kongweiguang
fn choice_observation(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    scheme: AppColorScheme,
) -> Result<(bool, bool), Box<dyn Error>> {
    let name = match scheme {
        AppColorScheme::Kerminal => "Kerminal",
        AppColorScheme::JetBrains => "JetBrains",
        AppColorScheme::Fleet => "Fleet",
        AppColorScheme::Xcode => "Xcode",
        AppColorScheme::Claude => "Claude",
        AppColorScheme::VsCode => "VS Code",
    };
    Ok(window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        let chinese = format!("应用配色：{name}");
        let english = format!("App color scheme: {name}");
        let item = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .find(|item| {
                item.label()
                    .is_some_and(|label| label == chinese || label == english)
            })
            .ok_or_else(|| io::Error::other(format!("missing accessible app palette {name}")))?;
        if item.role() != Some(gpui_pre::Role::RadioButton) {
            return Err(io::Error::other(format!(
                "app palette {name} has the wrong accessibility role"
            )));
        }
        Ok::<_, io::Error>((item.checked() == Some(true), item.focused() == Some(true)))
    })??)
}

/// 选中态必须能由无障碍观察读出，不能仅以颜色或设置草稿推断。
///
/// @author kongweiguang
fn assert_choice_checked(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    scheme: AppColorScheme,
) -> Result<(), Box<dyn Error>> {
    if !choice_observation(cx, window, scheme)?.0 {
        return Err(io::Error::other(format!("{} radio is not checked", scheme.as_str())).into());
    }
    Ok(())
}

/// 通过真正的语言选择浮层检查深浅色板下的覆盖层渲染与 Escape 焦点恢复。
///
/// @author kongweiguang
async fn capture_popup(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    context: &ThemeContext,
    mode_name: &str,
    scheme: AppColorScheme,
    screenshots: &mut Vec<CaptureRecord>,
) -> Result<(), Box<dyn Error>> {
    reveal_settings_selector(
        cx,
        root,
        window,
        "appearance",
        "settings-interface-language-select",
    )
    .await?;
    let focus = root_state(cx, root, window)?
        .language_select_focus()
        .clone();
    click_selector(cx, window, "settings-interface-language-select")?;
    wait_for_deferred_context(cx, window, true, Instant::now() + ACTION_TIMEOUT).await?;
    screenshots.push(save_frame(
        cx,
        window,
        context
            .output_directory
            .join(format!("popup-{mode_name}-{}.png", scheme.as_str())),
    )?);
    press_key(cx, window, "escape")?;
    wait_for_deferred_context(cx, window, false, Instant::now() + ACTION_TIMEOUT).await?;
    wait_for_window_focus(cx, window, &focus, Instant::now() + ACTION_TIMEOUT).await?;
    Ok(())
}

/// 真实键盘事件逐项验证单选组导航、确认、退出以及弹框再次打开后的持久选择。
///
/// @author kongweiguang
async fn verify_keyboard(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    terminal_settings: &kerminal_application::preferences::TerminalSettings,
    session_id: &str,
) -> Result<KeyboardCheck, Box<dyn Error>> {
    ensure_page(cx, root, window, "appearance", "appearance").await?;
    reveal_settings_selector(cx, root, window, "appearance", "app-color-kerminal").await?;
    click_selector(cx, window, "app-color-kerminal")?;
    let initial = wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.draft_app_color_scheme() == Some("kerminal")
            && state.draft_ready()
            && !state.draft_dirty()
    })
    .await?;
    let generation = initial.terminal_palette_update_generation();
    let terminal_palette = *initial.terminal_palette();
    let rendered_palette = *initial
        .rendered_terminal_palette()
        .ok_or_else(|| io::Error::other("keyboard check has no rendered terminal palette"))?;

    let mut tab_focused_kerminal = false;
    for _ in 0..80 {
        press_key(cx, window, "tab")?;
        if choice_observation(cx, window, AppColorScheme::Kerminal)?.1 {
            tab_focused_kerminal = true;
            break;
        }
    }
    if !tab_focused_kerminal {
        return Err(io::Error::other("Tab did not reach the Kerminal palette radio").into());
    }
    press_key(cx, window, "right")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.draft_app_color_scheme() == Some("jetbrains")
            && state.applied_app_color_scheme() == Some("jetbrains")
    })
    .await?;
    let right_selected_jetbrains = choice_observation(cx, window, AppColorScheme::JetBrains)?.1;
    if !right_selected_jetbrains {
        return Err(io::Error::other("Right did not focus the next palette radio").into());
    }
    press_key(cx, window, "tab")?;
    let tab_focused_fleet = choice_observation(cx, window, AppColorScheme::Fleet)?.1;
    if !tab_focused_fleet {
        return Err(io::Error::other("Tab did not reach the Fleet palette radio").into());
    }
    press_activation_key(cx, window, "space")?;
    let saved = wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.draft_app_color_scheme() == Some("fleet")
            && state.applied_app_color_scheme() == Some("fleet")
            && state.kit_theme_matches_app_color()
            && state.draft_ready()
            && !state.draft_dirty()
    })
    .await?;
    let persisted = load_persisted_settings(services).await?.settings;
    let space_selected_fleet = persisted.app_color_scheme == AppColorScheme::Fleet
        && persisted.terminal == *terminal_settings
        && saved.terminal_palette() == &terminal_palette
        && saved.rendered_terminal_palette() == Some(&rendered_palette)
        && saved.terminal_palette_update_generation() == generation
        && saved.ready_local_session_ids().len() == 1
        && saved.ready_local_session_ids()[0] == session_id;
    if !space_selected_fleet {
        return Err(
            io::Error::other("Space selection changed the terminal or was not saved").into(),
        );
    }
    let return_focus = saved
        .modal_return_focus()
        .cloned()
        .ok_or_else(|| io::Error::other("settings modal has no focus restoration target"))?;
    press_key(cx, window, "escape")?;
    let closed = wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        !state.modal_open()
    })
    .await?;
    wait_for_window_focus(cx, window, &return_focus, Instant::now() + ACTION_TIMEOUT).await?;
    let escape_closed_and_restored_focus = !closed.modal_open();
    press_key(cx, window, "ctrl-alt-s")?;
    let reopened = wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open() && state.draft_ready() && state.draft_app_color_scheme() == Some("fleet")
    })
    .await?;
    let reopened_fleet = reopened.applied_app_color_scheme() == Some("fleet");
    if !reopened_fleet {
        return Err(io::Error::other("reopened settings did not restore the saved palette").into());
    }
    Ok(KeyboardCheck {
        tab_focused_kerminal,
        right_selected_jetbrains,
        tab_focused_fleet,
        space_selected_fleet,
        escape_closed_and_restored_focus,
        reopened_fleet,
    })
}

/// 在真实窄窗口中检查每项相对于正文裁切区域的边界与相邻单选项几何。
///
/// @author kongweiguang
async fn verify_narrow_window(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    context: &ThemeContext,
    screenshots: &mut Vec<CaptureRecord>,
) -> Result<NarrowCheck, Box<dyn Error>> {
    const WIDTH: u32 = 800;
    const HEIGHT: u32 = 600;
    resize_acceptance_window(cx, window, WIDTH, HEIGHT).await?;
    ensure_page(cx, root, window, "appearance", "appearance").await?;
    for scheme in AppColorScheme::ALL {
        let selector = format!("app-color-{}", scheme.as_str());
        reveal_settings_selector(cx, root, window, "appearance", &selector).await?;
        window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            draw_acceptance_frame(window, app);
            let target = selector_bounds(window, &selector)?;
            let viewport = root.update(app, |root, cx| root.settings_acceptance_content_bounds(cx));
            if !bounds_inside_viewport(target, viewport) {
                return Err(io::Error::other(format!(
                    "{selector} is clipped in the narrow settings window"
                )));
            }
            for other in AppColorScheme::ALL
                .into_iter()
                .filter(|other| *other != scheme)
            {
                let other_selector = format!("app-color-{}", other.as_str());
                if let Ok(other_bounds) = selector_bounds(window, &other_selector)
                    && bounds_overlap(target, other_bounds)
                {
                    return Err(io::Error::other(format!(
                        "{selector} overlaps {other_selector}"
                    )));
                }
            }
            Ok::<_, io::Error>(())
        })??;
    }
    let screenshot = "settings-narrow-app-palettes.png".to_owned();
    screenshots.push(save_frame(
        cx,
        window,
        context.output_directory.join(&screenshot),
    )?);
    verify_capture_dimensions(
        cx,
        window,
        WIDTH,
        HEIGHT,
        &screenshots[screenshots.len() - 1..],
    )?;
    Ok(NarrowCheck {
        width: WIDTH,
        height: HEIGHT,
        all_choices_inside_content: true,
        choices_do_not_overlap: true,
        screenshot,
    })
}

/// 仅将正面积交叠视为错误，共边或正常行间距不算碰撞。
///
/// @author kongweiguang
fn bounds_overlap(a: Bounds<gpui_pre::Pixels>, b: Bounds<gpui_pre::Pixels>) -> bool {
    a.origin.x < b.origin.x + b.size.width
        && b.origin.x < a.origin.x + a.size.width
        && a.origin.y < b.origin.y + b.size.height
        && b.origin.y < a.origin.y + a.size.height
}

/// Kit 的单选点击在按键释放时提交，验收必须派发完整按下与释放序列。
///
/// @author kongweiguang
fn press_activation_key(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    key: &str,
) -> Result<(), Box<dyn Error>> {
    let keystroke = Keystroke::parse(key)
        .map_err(|error| io::Error::other(format!("invalid activation key {key}: {error}")))?;
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        window.dispatch_keystroke(keystroke.clone(), app);
        let _ = window.dispatch_event(gpui_pre::KeyUpEvent { keystroke }.to_platform_input(), app);
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}
