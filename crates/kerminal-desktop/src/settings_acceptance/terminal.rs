//! 终端设置通过真实控件、持久化与活动 PTY 投影进行隔离验收。
//!
//! @author kongweiguang

use kerminal_application::preferences::TerminalColorScheme;

use super::*;

/// 验证主题随应用明暗切换，并逐项核对系统字体和数值控件的实际结果。
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
    let mut checks = Vec::with_capacity(4);
    checks.push(verify_theme(cx, root, window, services, context, screenshots).await?);
    checks.push(verify_font(cx, root, window, services, context, screenshots).await?);
    checks.push(verify_font_size(cx, root, window, services, context, screenshots).await?);
    checks.push(verify_line_height(cx, root, window, services, context, screenshots).await?);
    Ok(checks)
}

/// 选定一个与初始值不同的终端主题，再核对深浅应用主题下的 PTY 实际色盘。
///
/// @author kongweiguang
async fn verify_theme(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
) -> Result<SettingCheck, Box<dyn Error>> {
    let before = load_persisted_settings(services).await?;
    let (scheme, selector) = if before.settings.terminal.color_scheme == TerminalColorScheme::Github
    {
        (
            TerminalColorScheme::TokyoNight,
            "terminal-scheme-tokyo-night",
        )
    } else {
        (TerminalColorScheme::Github, "terminal-scheme-github")
    };
    let sessions = root_state(cx, root, window)?
        .ready_local_session_ids()
        .to_vec();
    if sessions.is_empty() {
        return Err(io::Error::other("terminal theme acceptance requires a live PTY").into());
    }
    ensure_page(cx, root, window, "terminal", "terminal").await?;
    reveal_settings_selector(cx, root, window, "terminal", selector).await?;
    click_selector(cx, window, selector)?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.terminal_color_scheme() == scheme
            && state.draft_ready()
            && !state.draft_dirty()
            && state.rendered_terminal_palette() == Some(state.terminal_palette())
            && state.ready_local_session_ids() == sessions.as_slice()
    })
    .await?;
    let selected = load_persisted_settings(services).await?;
    if selected.settings.terminal.color_scheme != scheme {
        return Err(io::Error::other("terminal theme did not survive persisted readback").into());
    }

    let mut palettes = Vec::with_capacity(2);
    for (mode, selector) in [
        (ThemeMode::Dark, "theme-dark"),
        (ThemeMode::Light, "theme-light"),
    ] {
        ensure_page(cx, root, window, "appearance", "appearance").await?;
        reveal_settings_selector(cx, root, window, "appearance", selector).await?;
        click_selector(cx, window, selector)?;
        let expected = theme_mode_id(mode);
        let state = wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
            state.draft_theme_mode() == Some(expected)
                && state.terminal_color_scheme() == scheme
                && state.draft_ready()
                && !state.draft_dirty()
                && state.rendered_terminal_palette() == Some(state.terminal_palette())
                && state.ready_local_session_ids() == sessions.as_slice()
        })
        .await?;
        palettes.push(*state.terminal_palette());
        if load_persisted_settings(services)
            .await?
            .settings
            .terminal
            .color_scheme
            != scheme
        {
            return Err(
                io::Error::other("application theme changed the terminal selection").into(),
            );
        }
    }
    if palettes[0] == palettes[1] {
        return Err(io::Error::other("terminal dark and light palettes did not change").into());
    }
    ensure_page(cx, root, window, "terminal", "terminal").await?;
    reveal_settings_selector(cx, root, window, "terminal", selector).await?;
    let capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-terminal-theme.png"),
    )?;
    screenshots.push(capture.clone());
    Ok(SettingCheck {
        action: "terminal-theme-and-live-palette",
        passed: true,
        expected: "single-theme|dark-light|same-pty".to_owned(),
        observed: "single-theme|dark-light|same-pty".to_owned(),
        capture: Some(capture),
    })
}

/// 从系统清单选择唯一可搜索字体，避免预置字体列表或回退值掩盖持久化错误。
///
/// @author kongweiguang
async fn verify_font(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
) -> Result<SettingCheck, Box<dyn Error>> {
    let before = load_persisted_settings(services).await?;
    let installed = window.update(cx, |_, window, _| window.text_system().all_font_names())?;
    let target = installed
        .iter()
        .find(|name| {
            name.is_ascii()
                && !name.is_empty()
                && *name != &before.settings.terminal.font_family
                && installed
                    .iter()
                    .filter(|other| other.to_lowercase().contains(&name.to_lowercase()))
                    .count()
                    == 1
        })
        .cloned()
        .ok_or_else(|| io::Error::other("no distinct searchable system font is available"))?;

    ensure_page(cx, root, window, "terminal", "terminal").await?;
    reveal_settings_selector(cx, root, window, "terminal", "terminal-font-family").await?;
    click_selector(cx, window, "terminal-font-family")?;
    wait_for_deferred_context(cx, window, true, Instant::now() + ACTION_TIMEOUT).await?;
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        for character in target.chars() {
            let key = character.to_string();
            window.dispatch_keystroke(
                Keystroke {
                    modifiers: gpui_pre::Modifiers::default(),
                    key: key.clone(),
                    key_char: Some(key),
                },
                app,
            );
        }
        Ok::<_, io::Error>(())
    })??;
    cx.background_executor()
        .timer(Duration::from_millis(350))
        .await;
    window.update(cx, |_, window, app| draw_acceptance_frame(window, app))?;
    press_key(cx, window, "enter")?;
    wait_for_deferred_context(cx, window, false, Instant::now() + ACTION_TIMEOUT).await?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.terminal_font_family() == target && state.draft_ready() && !state.draft_dirty()
    })
    .await?;
    let persisted = load_persisted_settings(services).await?;
    if persisted.settings.terminal.font_family != target {
        return Err(io::Error::other("selected system font did not survive readback").into());
    }
    let capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-terminal-font.png"),
    )?;
    screenshots.push(capture.clone());
    Ok(SettingCheck {
        action: "terminal-system-font-search",
        passed: true,
        expected: target.clone(),
        observed: persisted.settings.terminal.font_family,
        capture: Some(capture),
    })
}

/// 真实点击字号步进按钮并测试上下界，再将实际终端投影与持久化值比较。
///
/// @author kongweiguang
async fn verify_font_size(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
) -> Result<SettingCheck, Box<dyn Error>> {
    let initial = load_persisted_settings(services)
        .await?
        .settings
        .terminal
        .font_size;
    let (direction, stepped) = if initial == 24 {
        ("decrement", initial - 1)
    } else {
        ("increment", initial + 1)
    };
    reveal_settings_selector(cx, root, window, "terminal", "terminal-font-size").await?;
    click_scoped_selector(cx, window, "terminal-font-size", direction)?;
    wait_for_font_size(cx, root, window, stepped).await?;
    if load_persisted_settings(services)
        .await?
        .settings
        .terminal
        .font_size
        != stepped
    {
        return Err(io::Error::other("font-size step did not persist").into());
    }
    for (value, direction) in [(24, "increment"), (10, "decrement")] {
        reveal_settings_selector(cx, root, window, "terminal", "terminal-font-size").await?;
        click_and_type(cx, window, "terminal-font-size", &value.to_string())?;
        wait_for_font_size(cx, root, window, value).await?;
        click_scoped_selector(cx, window, "terminal-font-size", direction)?;
        wait_for_font_size(cx, root, window, value).await?;
        if load_persisted_settings(services)
            .await?
            .settings
            .terminal
            .font_size
            != value
        {
            return Err(io::Error::other("font-size bound did not persist").into());
        }
    }
    let capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-terminal-font-size.png"),
    )?;
    screenshots.push(capture.clone());
    Ok(SettingCheck {
        action: "terminal-font-size-step-and-bounds",
        passed: true,
        expected: "step=1|bounds=10..24".to_owned(),
        observed: "step=1|bounds=10..24".to_owned(),
        capture: Some(capture),
    })
}

/// 将行高比率投影为千分数，验证 0.05 步进和 1.0–1.8 两端的真实行为。
///
/// @author kongweiguang
async fn verify_line_height(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
) -> Result<SettingCheck, Box<dyn Error>> {
    let initial = load_persisted_settings(services)
        .await?
        .settings
        .terminal
        .line_height;
    let initial_milli = (initial * 1_000.).round() as u16;
    let (direction, stepped) = if initial_milli >= 1_800 {
        ("decrement", initial_milli - 50)
    } else {
        ("increment", initial_milli + 50)
    };
    reveal_settings_selector(cx, root, window, "terminal", "terminal-line-height").await?;
    click_scoped_selector(cx, window, "terminal-line-height", direction)?;
    wait_for_line_height(cx, root, window, stepped).await?;
    let persisted = load_persisted_settings(services).await?;
    if (persisted.settings.terminal.line_height * 1_000.).round() as u16 != stepped {
        return Err(io::Error::other("line-height step did not persist").into());
    }
    for (value, milli, direction) in [("1.8", 1_800, "increment"), ("1.0", 1_000, "decrement")] {
        reveal_settings_selector(cx, root, window, "terminal", "terminal-line-height").await?;
        click_and_type(cx, window, "terminal-line-height", value)?;
        wait_for_line_height(cx, root, window, milli).await?;
        click_scoped_selector(cx, window, "terminal-line-height", direction)?;
        wait_for_line_height(cx, root, window, milli).await?;
        if (load_persisted_settings(services)
            .await?
            .settings
            .terminal
            .line_height
            * 1_000.)
            .round() as u16
            != milli
        {
            return Err(io::Error::other("line-height bound did not persist").into());
        }
    }
    let capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-terminal-line-height.png"),
    )?;
    screenshots.push(capture.clone());
    Ok(SettingCheck {
        action: "terminal-line-height-step-and-bounds",
        passed: true,
        expected: "step=0.05|bounds=1.0..1.8".to_owned(),
        observed: "step=0.05|bounds=1.0..1.8".to_owned(),
        capture: Some(capture),
    })
}

/// 等待输入、实际终端字体度量和 CAS 保存同时收敛，避免用输入回显代替生效。
///
/// @author kongweiguang
async fn wait_for_font_size(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    expected: u16,
) -> Result<(), Box<dyn Error>> {
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.terminal_font_size() == expected && state.draft_ready() && !state.draft_dirty()
    })
    .await?;
    Ok(())
}

/// 用舍入后的真实渲染比例等待行高生效，浮点误差不影响上下界判断。
///
/// @author kongweiguang
async fn wait_for_line_height(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    expected: u16,
) -> Result<(), Box<dyn Error>> {
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.terminal_line_height_milli() == expected
            && state.draft_ready()
            && !state.draft_dirty()
    })
    .await?;
    Ok(())
}
