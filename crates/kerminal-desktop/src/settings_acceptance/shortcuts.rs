//! 隔离窗口中通过实际键盘事件验证录制、取消和运行态快捷键替换。
//!
//! @author kongweiguang

use super::*;

const ACTION: &str = "settings.open";
const RECORD: &str = "shortcut-record-settings.open-windows";
const CONFIRM: &str = "shortcut-confirm-settings.open-windows";

/// 验证录制期间动作不穿透、取消不写入以及确认后新旧快捷键的真实行为。
///
/// @author kongweiguang
pub(super) async fn verify(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
) -> Result<Vec<SettingCheck>, Box<dyn Error>> {
    let initial = load_persisted_settings(services).await?;
    let initial_binding = initial
        .settings
        .keybindings
        .iter()
        .find(|binding| binding.action == ACTION)
        .ok_or_else(|| io::Error::other("settings shortcut is missing"))?
        .windows_binding
        .clone();
    let old = Keystroke::parse(&initial_binding.replace('+', "-").to_ascii_lowercase())
        .map_err(|error| io::Error::other(format!("invalid initial settings shortcut: {error}")))?;
    if old != Keystroke::parse("ctrl-alt-s").map_err(io::Error::other)? {
        return Err(
            io::Error::other("isolated settings shortcut is not the expected default").into(),
        );
    }

    ensure_page(cx, root, window, "shortcuts", "shortcuts").await?;
    begin_capture(cx, root, window).await?;
    press_key(cx, window, "ctrl-alt-s")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open()
            && state.page_id() == "shortcuts"
            && state.shortcut_capture_action() == Some(ACTION)
            && state
                .shortcut_capture_strokes()
                .is_some_and(|strokes| strokes.len() == 1)
            && !state.shortcut_capture_recording()
    })
    .await?;
    let capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-shortcut-capture.png"),
    )?;
    screenshots.push(capture.clone());
    press_key(cx, window, "escape")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open() && state.shortcut_capture_action().is_none()
    })
    .await?;
    let cancelled = load_persisted_settings(services).await?;
    if binding(&cancelled.settings)? != initial_binding {
        return Err(
            io::Error::other("cancelled shortcut capture changed persisted settings").into(),
        );
    }

    begin_capture(cx, root, window).await?;
    press_key(cx, window, "ctrl-shift-f12")?;
    let recorded = wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.shortcut_capture_action() == Some(ACTION)
            && state
                .shortcut_capture_strokes()
                .is_some_and(|strokes| strokes.len() == 1)
            && !state.shortcut_capture_recording()
    })
    .await?;
    if let Some(error) = recorded.shortcut_capture_error() {
        return Err(
            io::Error::other(format!("shortcut recording rejected the test key: {error}")).into(),
        );
    }
    let expected = Keystroke::parse("ctrl-shift-f12").map_err(io::Error::other)?;
    let recorded_key = recorded
        .shortcut_capture_strokes()
        .and_then(|strokes| strokes.first())
        .ok_or_else(|| io::Error::other("shortcut recorder did not expose the captured key"))?;
    if Keystroke::parse(recorded_key).map_err(io::Error::other)? != expected {
        return Err(io::Error::other("shortcut recorder captured a different key").into());
    }
    reveal_settings_selector(cx, root, window, "shortcuts", CONFIRM).await?;
    click_selector(cx, window, CONFIRM)?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.shortcut_capture_action().is_none() && state.draft_ready() && !state.draft_dirty()
    })
    .await?;
    let saved = load_persisted_settings(services).await?;
    if Keystroke::parse(
        &binding(&saved.settings)?
            .replace('+', "-")
            .to_ascii_lowercase(),
    )
    .map_err(io::Error::other)?
        != expected
    {
        return Err(io::Error::other("recorded shortcut did not persist").into());
    }

    click_selector(cx, window, "preferences-close")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        !state.modal_open()
    })
    .await?;
    press_key(cx, window, "ctrl-alt-s")?;
    cx.background_executor()
        .timer(Duration::from_millis(300))
        .await;
    if root_state(cx, root, window)?.modal_open() {
        return Err(io::Error::other("old settings shortcut still opens the modal").into());
    }
    press_key(cx, window, "ctrl-shift-f12")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open() && state.settings_ready() && state.draft_ready()
    })
    .await?;
    ensure_page(cx, root, window, "shortcuts", "shortcuts").await?;
    reveal_settings_selector(cx, root, window, "shortcuts", RECORD).await?;
    let live_capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-shortcut-live.png"),
    )?;
    screenshots.push(live_capture.clone());

    let mut checks = vec![
        SettingCheck {
            action: "shortcut-capture-and-cancel",
            passed: true,
            expected: "intercepted|cancel-preserves-binding".to_owned(),
            observed: "intercepted|cancel-preserves-binding".to_owned(),
            capture: Some(capture),
        },
        SettingCheck {
            action: "shortcut-custom-binding-live",
            passed: true,
            expected: "saved|new-opens|old-inactive".to_owned(),
            observed: "saved|new-opens|old-inactive".to_owned(),
            capture: Some(live_capture),
        },
    ];

    // 非当前系统平台必须暴露与 Windows 相同的录制入口，而不是退回手输框。
    for (platform, action, file) in [
        (
            "linux",
            "shortcut-platform-linux-recorder",
            "settings-functional-shortcut-linux.png",
        ),
        (
            "macos",
            "shortcut-platform-macos-recorder",
            "settings-functional-shortcut-macos.png",
        ),
    ] {
        let platform_selector = format!("settings-shortcut-platform-{platform}");
        reveal_settings_selector(cx, root, window, "shortcuts", &platform_selector).await?;
        click_selector(cx, window, &platform_selector)?;
        let record = format!("shortcut-record-settings.open-{platform}");
        reveal_settings_selector(cx, root, window, "shortcuts", &record).await?;
        let capture = save_frame(cx, window, context.output_directory.join(file))?;
        screenshots.push(capture.clone());
        checks.push(SettingCheck {
            action,
            passed: true,
            expected: "record-control-visible".to_owned(),
            observed: "record-control-visible".to_owned(),
            capture: Some(capture),
        });
    }

    Ok(checks)
}

/// 等待录制专用焦点真正挂载，避免隐藏窗口将组合键发送给旧焦点。
///
/// @author kongweiguang
async fn begin_capture(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    reveal_settings_selector(cx, root, window, "shortcuts", RECORD).await?;
    click_selector(cx, window, RECORD)?;
    let state = wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.shortcut_capture_action() == Some(ACTION) && state.shortcut_capture_recording()
    })
    .await?;
    let focus = state
        .shortcut_capture_focus()
        .ok_or_else(|| io::Error::other("shortcut recorder has no focus"))?
        .clone();
    wait_for_window_focus(cx, window, &focus, Instant::now() + ACTION_TIMEOUT).await
}

/// 从完整设置快照查找 Windows 主绑定，其它动作不参与测试。
///
/// @author kongweiguang
fn binding(
    settings: &kerminal_application::preferences::SettingsSnapshot,
) -> Result<&str, io::Error> {
    settings
        .keybindings
        .iter()
        .find(|binding| binding.action == ACTION)
        .map(|binding| binding.windows_binding.as_str())
        .ok_or_else(|| io::Error::other("settings shortcut is missing"))
}
