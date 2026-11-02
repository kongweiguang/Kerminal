//! 真实 MCP listener 与原生设置按钮的闭环验收，只作用于当前隔离组合根。
//!
//! @author kongweiguang

use kerminal_application::{
    AppError,
    mcp_runtime::{McpRuntimeSnapshot, McpRuntimeState},
    preferences::settings::McpBindMode,
};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
    net::TcpStream,
};

use super::*;

/// 释放本轮保留端口后验证真实 listener；停止超时必须由用户按钮重试原 owner。
///
/// @author kongweiguang
pub(super) async fn verify(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
    capture_lan_themes: bool,
) -> Result<Vec<SettingCheck>, Box<dyn Error>> {
    let reservation = context
        .mcp_port_reservation
        .lock()
        .map_err(|_| io::Error::other("isolated MCP port reservation lock was poisoned"))?
        .take()
        .ok_or_else(|| io::Error::other("isolated MCP port reservation is missing"))?;
    let reserved_port = reservation.local_addr()?.port();
    let settings = load_persisted_settings(services).await?;
    if settings.settings.mcp.port != reserved_port {
        return Err(io::Error::other("isolated MCP port does not match persisted settings").into());
    }
    drop(reservation);
    ensure_page(cx, root, window, "mcp", "mcp").await?;
    reveal_settings_selector(cx, root, window, "mcp", "mcp-settings-toggle").await?;
    let control = services
        .mcp_runtime
        .as_ref()
        .ok_or_else(|| io::Error::other("native MCP acceptance requires the production runtime"))?;
    if control.snapshot().state == McpRuntimeState::Stopped {
        click_selector(cx, window, "mcp-settings-toggle")?;
    }
    let running = wait_for_runtime(cx, window, services, McpRuntimeState::Running).await?;
    let endpoint = running
        .endpoint
        .ok_or_else(|| io::Error::other("running MCP has no endpoint"))?;
    let barrier = services
        .executor
        .run(move |_| open_barrier(endpoint))
        .await?;
    click_selector(cx, window, "mcp-settings-toggle")?;
    let pending = wait_for_runtime(cx, window, services, McpRuntimeState::Stopping).await?;
    if pending.client_json.is_some() || pending.failure_reason.is_none() {
        return Err(io::Error::other("timed-out stop lost its cleanup state").into());
    }
    let label = button(cx, window, "mcp-settings-toggle")?
        .label()
        .unwrap_or_default()
        .to_owned();
    if !["重试停止", "Retry stop"].contains(&label.as_str()) {
        return Err(
            io::Error::other(format!("timed-out stop has no recovery action: {label}")).into(),
        );
    }
    let capture = save_frame(
        cx,
        window,
        context.output_directory.join("settings-mcp-retry-stop.png"),
    )?;
    screenshots.push(capture.clone());
    drop(barrier);
    click_selector(cx, window, "mcp-settings-toggle")?;
    wait_for_runtime(cx, window, services, McpRuntimeState::Stopped).await?;
    let mut checks = vec![SettingCheck {
        action: "mcp-timeout-retry-stops-original-owner",
        passed: true,
        expected: "Stopping → Retry stop → Stopped".to_owned(),
        observed: "Stopping → Retry stop → Stopped".to_owned(),
        capture: Some(capture),
    }];

    // 在三种主题下展开真实连接信息，检查恢复后可启动、可停止和配置数据的就绪状态。
    for (theme, name, selector) in THEMES {
        ensure_page(cx, root, window, "appearance", "appearance").await?;
        reveal_settings_selector(cx, root, window, "appearance", selector).await?;
        click_selector(cx, window, selector)?;
        wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
            state.draft_theme_mode() == Some(theme_mode_id(theme))
                && state.draft_ready()
                && !state.draft_dirty()
        })
        .await?;
        ensure_page(cx, root, window, "mcp", "mcp").await?;
        reveal_settings_selector(cx, root, window, "mcp", "mcp-settings-toggle").await?;
        click_selector(cx, window, "mcp-settings-toggle")?;
        wait_for_runtime(cx, window, services, McpRuntimeState::Running).await?;
        if !root_state(cx, root, window)?.expanded_disclosures()[2] {
            click_selector(cx, window, "mcp-connection-info-toggle")?;
        }
        if control.snapshot().client_json.is_none() {
            return Err(io::Error::other("running MCP configuration is missing").into());
        }
        let _ = button(cx, window, "mcp-settings-copy-json")?;
        let capture = save_frame(
            cx,
            window,
            context
                .output_directory
                .join(format!("settings-mcp-running-{name}.png")),
        )?;
        screenshots.push(capture.clone());
        click_selector(cx, window, "mcp-settings-toggle")?;
        wait_for_runtime(cx, window, services, McpRuntimeState::Stopped).await?;
        if control.snapshot().client_json.is_some() {
            return Err(io::Error::other("stopped MCP configuration remains available").into());
        }
        checks.push(SettingCheck {
            action: match theme {
                ThemeMode::Light => "mcp-start-stop-light",
                ThemeMode::Dark => "mcp-start-stop-dark",
                ThemeMode::System => "mcp-start-stop-system",
            },
            passed: true,
            expected: "Running (JSON available) → Stopped (JSON absent)".to_owned(),
            observed: "Running (JSON available) → Stopped (JSON absent)".to_owned(),
            capture: Some(capture),
        });
    }
    checks.extend(
        verify_lan_mode(
            cx,
            root,
            window,
            services,
            context,
            screenshots,
            capture_lan_themes,
        )
        .await?,
    );
    Ok(checks)
}

/// 局域网设置保存后必须以全部 IPv4 接口监听，并把真实网卡地址提供给复制入口。
/// 结束后恢复本机监听与原端口，后续草稿检查仍使用同一隔离组合根。
async fn verify_lan_mode(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
    capture_themes: bool,
) -> Result<Vec<SettingCheck>, Box<dyn Error>> {
    let original = load_persisted_settings(services).await?.settings.mcp;
    if original.bind_mode != McpBindMode::Loopback {
        return Err(io::Error::other("MCP LAN acceptance requires loopback baseline").into());
    }
    let runtime = services
        .mcp_runtime
        .clone()
        .ok_or_else(|| io::Error::other("native MCP acceptance requires the production runtime"))?;
    ensure_page(cx, root, window, "mcp", "mcp").await?;
    reveal_settings_selector(cx, root, window, "mcp", "mcp-bind-lan").await?;
    click_selector(cx, window, "mcp-bind-lan")?;
    reveal_settings_selector(cx, root, window, "mcp", "mcp-settings-toggle").await?;
    click_selector(cx, window, "mcp-settings-toggle")?;
    wait_for_runtime(cx, window, services, McpRuntimeState::Running).await?;

    let saved = load_persisted_settings(services).await?.settings.mcp;
    if saved.bind_mode != McpBindMode::Lan {
        return Err(io::Error::other("LAN draft was not persisted before start").into());
    }
    if !root_state(cx, root, window)?.expanded_disclosures()[2] {
        click_selector(cx, window, "mcp-connection-info-toggle")?;
    }
    let expected = format!("http://0.0.0.0:{}/mcp", saved.port);
    let listen = button(cx, window, "mcp-listen-address")?
        .label()
        .unwrap_or_default()
        .to_owned();
    if listen != expected {
        return Err(io::Error::other(format!(
            "LAN listen address shows {listen:?} instead of {expected:?}"
        ))
        .into());
    }

    let _ = wait_for_lan_address(cx, window).await?;
    let addresses = services
        .executor
        .run(move |_| async move { runtime.lan_addresses().await })
        .await?;
    if addresses.is_empty() {
        return Err(io::Error::other("LAN listener published no interface addresses").into());
    }
    let Some(selected) = addresses
        .iter()
        .copied()
        .find(|address| address.is_private())
        .or_else(|| addresses.first().copied())
    else {
        return Err(io::Error::other("LAN listener published no selectable address").into());
    };
    let expected = format!("http://{selected}:{}/mcp", saved.port);
    let connect = button(cx, window, "mcp-connect-address")?
        .label()
        .unwrap_or_default()
        .to_owned();
    if connect != expected {
        return Err(io::Error::other(format!(
            "LAN connection preview shows {connect:?} instead of {expected:?}"
        ))
        .into());
    }
    let copy = button(cx, window, "mcp-settings-copy-json")?;
    if copy.disabled() == Some(true) {
        return Err(io::Error::other("LAN address selection did not enable copying").into());
    }
    if !["复制局域网配置", "Copy LAN config"].contains(&copy.label().unwrap_or_default()) {
        return Err(io::Error::other("LAN target does not expose the LAN copy action").into());
    }

    for address in &addresses {
        let target = std::net::SocketAddr::from((*address, saved.port));
        let connected = services
            .executor
            .run(move |_| async move {
                Ok(
                    tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(target))
                        .await
                        .is_ok_and(|result| result.is_ok()),
                )
            })
            .await?;
        if !connected {
            return Err(io::Error::other(format!(
                "LAN listener is not reachable on {address}:{}",
                saved.port
            ))
            .into());
        }
    }
    let capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-mcp-lan-running.png"),
    )?;
    screenshots.push(capture.clone());

    if capture_themes {
        for (theme, name, selector) in THEMES {
            ensure_page(cx, root, window, "appearance", "appearance").await?;
            reveal_settings_selector(cx, root, window, "appearance", selector).await?;
            click_selector(cx, window, selector)?;
            wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
                state.draft_theme_mode() == Some(theme_mode_id(theme))
                    && state.draft_ready()
                    && !state.draft_dirty()
            })
            .await?;
            ensure_page(cx, root, window, "mcp", "mcp").await?;
            if !root_state(cx, root, window)?.expanded_disclosures()[2] {
                click_selector(cx, window, "mcp-connection-info-toggle")?;
            }
            screenshots.push(save_frame(
                cx,
                window,
                context
                    .output_directory
                    .join(format!("settings-mcp-lan-{name}.png")),
            )?);
        }
    }

    reveal_settings_selector(cx, root, window, "mcp", "mcp-settings-toggle").await?;
    click_selector(cx, window, "mcp-settings-toggle")?;
    wait_for_runtime(cx, window, services, McpRuntimeState::Stopped).await?;
    reveal_settings_selector(cx, root, window, "mcp", "mcp-bind-loopback").await?;
    click_selector(cx, window, "mcp-bind-loopback")?;
    reveal_settings_selector(cx, root, window, "mcp", "mcp-settings-toggle").await?;
    click_selector(cx, window, "mcp-settings-toggle")?;
    wait_for_runtime(cx, window, services, McpRuntimeState::Running).await?;
    reveal_settings_selector(cx, root, window, "mcp", "mcp-settings-toggle").await?;
    click_selector(cx, window, "mcp-settings-toggle")?;
    wait_for_runtime(cx, window, services, McpRuntimeState::Stopped).await?;
    let restored = load_persisted_settings(services).await?.settings.mcp;
    if restored.bind_mode != McpBindMode::Loopback || restored.port != original.port {
        return Err(io::Error::other("LAN acceptance did not restore loopback settings").into());
    }

    let summary = format!(
        "0.0.0.0 listener on port {} with {} reachable interface addresses",
        saved.port,
        addresses.len()
    );
    Ok(vec![SettingCheck {
        action: "mcp-lan-binds-all-interfaces",
        passed: true,
        expected: summary.clone(),
        observed: summary,
        capture: Some(capture),
    }])
}

/// 地址按钮由真实运行 listener 回填，不能靠固定等待假定异步列表已经出现。
async fn wait_for_lan_address(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<String, Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let found = window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            window.refresh();
            draw_acceptance_frame(window, app);
            Ok::<_, io::Error>(
                gpui_kit::base::test_support::snapshots(window)
                    .into_iter()
                    .find_map(|snapshot| {
                        snapshot
                            .path()
                            .last()
                            .map(ToString::to_string)
                            .filter(|id| {
                                id.starts_with("mcp-lan-address-")
                                    && id != "mcp-lan-addresses-empty"
                            })
                    }),
            )
        })??;
        if let Some(address) = found {
            return Ok(address);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("LAN address list stayed empty").into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 同时等共享 listener 终态和生产按钮的动作标签，不能把后台状态先到误当 UI 已完成。
async fn wait_for_runtime(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    expected: McpRuntimeState,
) -> Result<McpRuntimeSnapshot, Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let snapshot = services.mcp_runtime.as_ref().unwrap().snapshot();
        let toggle = button(cx, window, "mcp-settings-toggle")?;
        let expected_label = match expected {
            McpRuntimeState::Running => ["停止", "Stop"],
            McpRuntimeState::Stopping => ["重试停止", "Retry stop"],
            McpRuntimeState::Stopped | McpRuntimeState::Failed => ["启动", "Start"],
            _ => return Err(io::Error::other("unsupported MCP acceptance state").into()),
        };
        if snapshot.state == expected
            && expected_label.contains(&toggle.label().unwrap_or_default())
        {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "MCP button did not settle: expected={expected:?}, observed={:?}",
                snapshot.state
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 每次读取当前帧的原生控件快照，避免沿用状态改变前的 label 或 disabled 值。
fn button(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<gpui_kit::base::test_support::ElementSnapshot, Box<dyn Error>> {
    Ok(window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        window.refresh();
        draw_acceptance_frame(window, app);
        gpui_kit::base::test_support::find(window, &[], &ElementId::from(selector.to_owned()))
            .ok_or_else(|| io::Error::other(format!("missing MCP button {selector}")))
    })??)
}

/// 用 100 Continue 确认 listener 已准入请求，再持有缺失 body 的连接稳定复现停止超时。
async fn open_barrier(endpoint: String) -> kerminal_application::AppResult<TcpStream> {
    tokio::time::timeout(Duration::from_secs(3), async move {
        let (authority, path) = endpoint.strip_prefix("http://").and_then(|value| value.split_once('/'))
            .ok_or_else(|| AppError::infrastructure("unexpected MCP endpoint"))?;
        let mut stream = TcpStream::connect(authority).await.map_err(|_| AppError::infrastructure("connect MCP barrier"))?;
        let request = format!("POST /{path} HTTP/1.1\r\nHost: {authority}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: 512\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.map_err(|_| AppError::infrastructure("write MCP barrier"))?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.map_err(|_| AppError::infrastructure("read MCP barrier"))?;
        if line != "HTTP/1.1 100 Continue\r\n" {
            return Err(AppError::infrastructure("MCP did not admit the incomplete body"));
        }
        Ok(reader.into_inner())
    }).await.map_err(|_| AppError::timeout("MCP acceptance barrier"))?
}

/// 专项复用正式设置入口、真实 listener 与私有 desktop，只省略无关页面，不放宽隔离和资源收尾。
pub(super) async fn capture_only(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
) -> Result<(), Box<dyn Error>> {
    let mcp_checks = verify(cx, root, window, services, context, screenshots, true).await?;
    for (_, name, selector) in THEMES {
        ensure_page(cx, root, window, "appearance", "appearance").await?;
        reveal_settings_selector(cx, root, window, "appearance", selector).await?;
        click_selector(cx, window, selector)?;
        wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
            state.draft_theme_mode() == Some(name) && state.draft_ready() && !state.draft_dirty()
        })
        .await?;
        ensure_page(cx, root, window, "mcp", "mcp").await?;
        if root_state(cx, root, window)?.expanded_disclosures()[2] {
            click_selector(cx, window, "mcp-connection-info-toggle")?;
        }
        screenshots.push(save_frame(
            cx,
            window,
            context
                .output_directory
                .join(format!("settings-mcp-compact-{name}.png")),
        )?);
    }
    let draft_checks = verify_draft_start(cx, root, window, services, context, screenshots).await?;
    verify_capture_dimensions(cx, window, context.width, context.height, screenshots)?;
    let modal_generation = root_state(cx, root, window)?.modal_generation();
    click_selector(cx, window, "preferences-close")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        !state.modal_open()
    })
    .await?;
    let (visible, active, bounds, isolation) = window.update(cx, |_, window, _| {
        let isolation = crate::right_rail_acceptance::isolation::capture()?;
        isolation.validate_window_state(window.is_visible(), window.is_window_active())?;
        Ok::<_, io::Error>((
            window.is_visible(),
            window.is_window_active(),
            window.bounds(),
            isolation,
        ))
    })??;
    let report = serde_json::json!({
        "schemaVersion": 6,
        "scope": "mcp",
        "captureMode": "hidden_native_window",
        "captureBackend": "gpui_window_render_to_image",
        "nativeIsolation": isolation,
        "configDirectory": context.output_directory.join("config"),
        "windowVisible": visible,
        "windowActive": active,
        "windowWidth": bounds.size.width.as_f32(),
        "windowHeight": bounds.size.height.as_f32(),
        "settingsModalGeneration": modal_generation,
        "themes": ["light", "dark", "system"],
        "mcpChecks": mcp_checks,
        "draftChecks": draft_checks,
        "screenshots": screenshots,
    });
    fs::write(
        context.output_directory.join(REPORT_FILE),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}

/// 用系统保留端口复现真实占用；配置保存、失败重试、实际新地址及停止都经生产控件。
async fn verify_draft_start(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
) -> Result<Vec<SettingCheck>, Box<dyn Error>> {
    let original = load_persisted_settings(services).await?.settings.mcp;
    let occupied = services
        .executor
        .run(|_| async {
            std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).map_err(|error| {
                AppError::infrastructure_with_source("reserve MCP test port", error)
            })
        })
        .await?;
    let port = occupied.local_addr()?.port();
    reveal_settings_selector(cx, root, window, "mcp", "mcp-settings-port").await?;
    click_and_type(cx, window, "mcp-settings-port", &port.to_string())?;
    click_selector(cx, window, "mcp-settings-toggle")?;
    wait_for_runtime(cx, window, services, McpRuntimeState::Failed).await?;
    let after_failure = load_persisted_settings(services).await?;
    if after_failure.settings.mcp.port != port {
        return Err(io::Error::other("MCP startup did not save the current port").into());
    }
    let failure = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-mcp-port-occupied.png"),
    )?;
    screenshots.push(failure.clone());
    drop(occupied);
    click_selector(cx, window, "mcp-settings-toggle")?;
    let running = wait_for_runtime(cx, window, services, McpRuntimeState::Running).await?;
    let expected = format!("http://127.0.0.1:{port}/mcp");
    if running.endpoint.as_deref() != Some(expected.as_str()) {
        return Err(io::Error::other("MCP retry did not use the latest saved port").into());
    }
    if !root_state(cx, root, window)?.expanded_disclosures()[2] {
        click_selector(cx, window, "mcp-connection-info-toggle")?;
    }
    reveal_settings_selector(cx, root, window, "mcp", "mcp-settings-copy-json").await?;
    click_selector(cx, window, "mcp-settings-copy-json")?;
    let copied = cx
        .update(|app| app.read_from_clipboard().and_then(|item| item.text()))
        .ok_or_else(|| io::Error::other("MCP client JSON was not copied"))?;
    let json: serde_json::Value = serde_json::from_str(&copied)?;
    if json["mcpServers"]["kerminal"]["url"].as_str() != Some(expected.as_str()) {
        return Err(io::Error::other("copied MCP JSON does not match the running listener").into());
    }
    let live = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-mcp-new-port-running.png"),
    )?;
    screenshots.push(live.clone());
    reveal_settings_selector(cx, root, window, "mcp", "mcp-settings-port").await?;
    click_and_type(cx, window, "mcp-settings-port", "0")?;
    // 展开连接信息后正文会滚动；必须先让按钮进入实际裁剪区域，避免点击标题栏上的旧几何。
    reveal_settings_selector(cx, root, window, "mcp", "mcp-settings-toggle").await?;
    click_selector(cx, window, "mcp-settings-toggle")?;
    wait_for_runtime(cx, window, services, McpRuntimeState::Stopped).await?;
    click_selector(cx, window, "mcp-settings-toggle")?;
    cx.background_executor().timer(POLL_INTERVAL).await;
    let state = services.mcp_runtime.as_ref().unwrap().snapshot();
    if state.state != McpRuntimeState::Stopped
        || load_persisted_settings(services).await?.settings.mcp.port != port
    {
        return Err(
            io::Error::other("invalid MCP port started or changed persisted settings").into(),
        );
    }
    let invalid = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-mcp-invalid-port.png"),
    )?;
    screenshots.push(invalid.clone());
    click_and_type(cx, window, "mcp-settings-port", &original.port.to_string())?;
    click_selector(cx, window, "mcp-settings-toggle")?;
    let restored = wait_for_runtime(cx, window, services, McpRuntimeState::Running).await?;
    let expected = format!("http://127.0.0.1:{}/mcp", original.port);
    if restored.endpoint.as_deref() != Some(expected.as_str()) {
        return Err(io::Error::other("MCP did not start from the corrected draft").into());
    }
    let corrected = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-mcp-corrected-port-running.png"),
    )?;
    screenshots.push(corrected.clone());
    click_selector(cx, window, "mcp-settings-toggle")?;
    wait_for_runtime(cx, window, services, McpRuntimeState::Stopped).await?;
    Ok([
        ("mcp-occupied-port-keeps-retry", failure),
        ("mcp-retry-reads-latest-and-copies-live-json", live),
        ("mcp-invalid-draft-allows-stop", invalid.clone()),
        ("mcp-invalid-port-prevents-start", invalid.clone()),
        ("mcp-corrected-draft-starts", corrected),
    ]
    .into_iter()
    .map(|(action, capture)| SettingCheck {
        action,
        passed: true,
        expected: "production controls and persisted runtime agree".to_owned(),
        observed: "production controls and persisted runtime agree".to_owned(),
        capture: Some(capture),
    })
    .collect())
}
