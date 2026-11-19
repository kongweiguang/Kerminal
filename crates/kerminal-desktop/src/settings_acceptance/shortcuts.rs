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
    const ADD_STROKE: &str = "shortcut-add-stroke-settings.open-windows";
    reveal_settings_selector(cx, root, window, "shortcuts", ADD_STROKE).await?;
    click_selector(cx, window, ADD_STROKE)?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.shortcut_capture_recording()
    })
    .await?;
    press_key(cx, window, "ctrl-f12")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        !state.shortcut_capture_recording()
            && state
                .shortcut_capture_strokes()
                .is_some_and(|keys| keys.len() == 2)
    })
    .await?;
    reveal_settings_selector(cx, root, window, "shortcuts", CONFIRM).await?;
    click_selector(cx, window, CONFIRM)?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.shortcut_capture_action().is_none() && state.draft_ready() && !state.draft_dirty()
    })
    .await?;
    let saved = load_persisted_settings(services).await?;
    let saved_keys = binding(&saved.settings)?
        .replace('+', "-")
        .to_ascii_lowercase();
    let saved_strokes = saved_keys
        .split_whitespace()
        .map(Keystroke::parse)
        .collect::<Result<Vec<_>, _>>()
        .map_err(io::Error::other)?;
    if saved_strokes
        != [
            expected,
            Keystroke::parse("ctrl-f12").map_err(io::Error::other)?,
        ]
    {
        return Err(io::Error::other("recorded chord did not persist").into());
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
    if root_state(cx, root, window)?.modal_open() {
        return Err(io::Error::other("the first stroke opened settings on its own").into());
    }
    press_key(cx, window, "ctrl-f12")?;
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

/// 通过隐藏窗口的生产焦点和真实 PTY 输入代次检查分流，避免 Shell 吞掉控制序列后误判通过。
///
/// @author kongweiguang
pub(super) async fn verify_terminal_input(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
) -> Result<(), Box<dyn Error>> {
    press_key(cx, window, "alt-f12")?;
    let initial = suggestions::wait_state(cx, root, window, "terminal keyboard focus", |state| {
        state.focused && state.input_idle && !state.ssh_fixture
    })
    .await?;
    let read_version = |id: &kerminal_domain::identity::SessionId| {
        let sessions = services.sessions.clone();
        let id = id.clone();
        services
            .executor
            .run(move |_| async move { Ok(sessions.terminal_command_context(id).await?.version()) })
    };
    let before = read_version(&initial.binding.session_id).await?;
    press_key(cx, window, "ctrl-insert")?;
    suggestions::wait_state(cx, root, window, "Ctrl+Insert stays local", |state| {
        state.binding == initial.binding && state.focused && state.input_idle
    })
    .await?;
    if read_version(&initial.binding.session_id).await? != before {
        return Err(io::Error::other("Ctrl+Insert wrote to PTY without a selection").into());
    }
    press_key(cx, window, "ctrl-b")?;
    suggestions::wait_state(cx, root, window, "Shell Ctrl+B completion", |state| {
        state.binding == initial.binding && state.focused && state.input_idle
    })
    .await?;
    let after = read_version(&initial.binding.session_id).await?;
    if after.session_generation != before.session_generation
        || after.input_generation != before.input_generation + 1
    {
        return Err(io::Error::other(format!(
            "Ctrl+Insert and Ctrl+B must produce exactly one PTY input: {before:?} -> {after:?}"
        ))
        .into());
    }

    press_key(cx, window, "ctrl-f")?;
    suggestions::wait_state(cx, root, window, "terminal search focus", |state| {
        state.binding == initial.binding && !state.focused && state.input_idle
    })
    .await?;
    press_key(cx, window, "escape")?;
    suggestions::wait_state(
        cx,
        root,
        window,
        "search restores terminal focus",
        |state| state.binding == initial.binding && state.focused && state.input_idle,
    )
    .await?;
    repeat_then_release_key(cx, window, "escape")?;
    suggestions::wait_state(cx, root, window, "held Escape stays local", |state| {
        state.binding == initial.binding && state.focused && state.input_idle
    })
    .await?;
    if read_version(&initial.binding.session_id).await? != after {
        return Err(io::Error::other("terminal search or held Escape wrote to PTY").into());
    }
    press_key(cx, window, "ctrl-shift-t")?;
    let second = suggestions::wait_state(cx, root, window, "new local tab", |state| {
        state.binding != initial.binding && state.focused && state.input_idle && !state.ssh_fixture
    })
    .await?;
    // 切换会改变窗口布局，Shell 可以重绘提示符；只比较两个 PTY 的输入代次。
    let first_before_switch = read_version(&initial.binding.session_id).await?;
    let second_before_switch = read_version(&second.binding.session_id).await?;
    for (key, target) in [
        ("alt-left", &initial.binding),
        ("alt-right", &second.binding),
    ] {
        press_key(cx, window, key)?;
        suggestions::wait_state(cx, root, window, key, |state| {
            state.binding == *target && state.focused && state.input_idle
        })
        .await?;
    }
    let first_after_switch = read_version(&initial.binding.session_id).await?;
    let second_after_switch = read_version(&second.binding.session_id).await?;
    if first_before_switch.input_generation != after.input_generation
        || first_after_switch.input_generation != first_before_switch.input_generation
        || second_after_switch.input_generation != second_before_switch.input_generation
    {
        return Err(io::Error::other(format!(
            "tab shortcuts changed a PTY input generation: {after:?}, {first_before_switch:?} -> {first_after_switch:?}; {second_before_switch:?} -> {second_after_switch:?}"
        ))
        .into());
    }
    for key in ["ctrl-p", "ctrl-shift-p"] {
        press_key(cx, window, key)?;
        let deadline = Instant::now() + ACTION_TIMEOUT;
        // 等焦点陷阱落在实际窗口，避免仅凭快捷键被消费就判定面板已打开。
        loop {
            let opened = window.update(cx, |_, window, app| {
                draw_acceptance_frame(window, app);
                selector_bounds(window, "quick-open-modal").is_ok()
                    && gpui_kit::base::active_focus_trap(window, app).is_some()
            })?;
            if opened {
                break;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(format!("{key} did not open the palette")).into());
            }
            cx.background_executor()
                .timer(Duration::from_millis(10))
                .await;
        }
        press_key(cx, window, "escape")?;
        suggestions::wait_state(cx, root, window, "palette returns focus", |state| {
            state.binding == second.binding && state.focused && state.input_idle
        })
        .await?;
    }
    if read_version(&second.binding.session_id)
        .await?
        .input_generation
        != second_after_switch.input_generation
    {
        return Err(io::Error::other("palette shortcuts changed the PTY input generation").into());
    }
    let report = serde_json::json!({
        "ctrl_p_quick_open": true,
        "ctrl_shift_p_command_palette": true,
        "ctrl_insert_without_selection_writes": 0,
        "ctrl_b_pty_writes": 1,
        "ctrl_f_and_escape_restore_focus": true,
        "held_escape_after_search_writes": 0,
        "ctrl_shift_t_creates_tab": true,
        "alt_left_previous_tab": true,
        "alt_right_next_tab": true
    });
    std::fs::write(
        context.output_directory.join("terminal-shortcuts.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
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
