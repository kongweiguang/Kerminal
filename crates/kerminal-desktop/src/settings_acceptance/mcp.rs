//! 真实 MCP listener 与原生设置按钮的闭环验收，只作用于当前隔离组合根。
//!
//! @author kongweiguang

use kerminal_application::{
    AppError,
    mcp_runtime::{McpRuntimeSnapshot, McpRuntimeState},
};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
    net::TcpStream,
};

use super::*;

/// 真实未完成 HTTP body 迫使停止超时，再由用户按钮重试同一 owner，禁止测试直接调用 stop。
pub(super) async fn verify(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
) -> Result<Vec<SettingCheck>, Box<dyn Error>> {
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
        if !root_state(cx, root, window)?.expanded_disclosures()[3] {
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
    Ok(checks)
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
            McpRuntimeState::Stopped => ["启动", "Start"],
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
