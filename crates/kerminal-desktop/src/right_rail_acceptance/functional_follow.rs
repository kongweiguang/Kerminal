//! 隔离 SSH 会话中的 SFTP 终端目录跟随原生验收。
//!
//! @author kongweiguang

use std::{error::Error, io, time::Duration};

use gpui_kit::{
    AnyWindowHandle, AsyncApp, Entity, InputEvent, KeyDownEvent, KeyUpEvent, Keystroke,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
};
use kerminal_application::{
    preferences::{ThemeMode, settings::ToolRailToolId},
    sessions::UiSessionBinding,
};
use kerminal_domain::identity::HostId;
use kerminal_ui::{AppServices, RootView};

use super::{
    RightRailCaptureContext, frame_signature,
    functional::{FunctionalRecord, checkpoint, poll_root},
    functional_transfer_workspace::click_visible_element,
    navigate_and_wait_for_sftp_directory,
};

#[derive(Clone, Copy)]
struct ExpectedState<'a> {
    host: &'a HostId,
    binding: &'a UiSessionBinding,
    terminal_directory: &'a str,
    enabled: bool,
    listing_directory: &'a str,
    marker: &'a str,
}

/// 验收只向本轮隔离 SSH 写入 cd 和 OSC7，由真实鼠标、键盘、开关语义及 SFTP listing 回读结果。
///
/// @author kongweiguang
pub(super) async fn run(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    services: &AppServices,
) -> Result<Vec<FunctionalRecord>, Box<dyn Error>> {
    let host = &capture.fixture_host_id;
    let base = &capture.remote_sftp_directory;
    if !base.starts_with("/run/kerminal-sftp-follow-")
        || base.contains("//")
        || base.split('/').any(|part| part == "." || part == "..")
        || !base.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '/' | '-' | '_')
        })
    {
        return Err(io::Error::other("跟随验收目录不是本轮隔离 fixture 路径").into());
    }
    let first = format!("{base}/first");
    let unicode = format!("{base}/目录 空间");
    let last = format!("{base}/last");
    let binding = poll_root(cx, root, window, "初始 fixture 会话", |root, _, _| {
        root.right_rail_acceptance_follow_terminal_snapshot(host)
            .map(|state| state.map(|(binding, _)| binding))
    })
    .await?;
    send_cd(cx, root, window, services, host, &binding, base).await?;
    poll_root(
        cx,
        root,
        window,
        "初始 fixture 终端目录",
        |root, _, _| {
            let Some((actual, cwd)) = root.right_rail_acceptance_follow_terminal_snapshot(host)?
            else {
                return Ok(None);
            };
            if actual != binding {
                return Err("跟随验收期间终端会话身份已变化".to_owned());
            }
            Ok((cwd.as_deref() == Some(base.as_str())).then_some(()))
        },
    )
    .await?;
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.set_right_rail_acceptance_theme(ThemeMode::Light, cx);
            root.activate_right_rail_tool_for_acceptance(ToolRailToolId::Sftp, window, cx);
        });
    })?;
    navigate_and_wait_for_sftp_directory(cx, root, window, host, base).await?;
    poll_root(
        cx,
        root,
        window,
        "关闭状态和 fixture 会话",
        |root, _, cx| {
            let Some((actual, _, _, enabled)) =
                root.right_rail_acceptance_follow_snapshot(host, cx)?
            else {
                return Ok(None);
            };
            if actual != binding {
                return Err("跟随验收期间终端会话身份已变化".to_owned());
            }
            if enabled {
                return Err("跟随验收初始开关不是关闭状态".to_owned());
            }
            Ok(Some(()))
        },
    )
    .await?;
    let mut records = Vec::new();

    send_cd(cx, root, window, services, host, &binding, &first).await?;
    let disabled = ExpectedState {
        host,
        binding: &binding,
        terminal_directory: &first,
        enabled: false,
        listing_directory: base,
        marker: "rr-ui-base",
    };
    wait_state(cx, root, window, disabled).await?;
    assert_toggle_state(cx, window, false)?;
    assert_stable(cx, root, window, disabled).await?;
    checkpoint(
        cx,
        window,
        capture,
        &mut records,
        "sftp-follow-disabled",
        "osc7_changed_browser_preserved",
    )
    .await?;

    click_visible_element(cx, window, "files-follow-terminal-1")?;
    let first_enabled = ExpectedState {
        host,
        binding: &binding,
        terminal_directory: &first,
        enabled: true,
        listing_directory: &first,
        marker: "rr-ui-first",
    };
    wait_state(cx, root, window, first_enabled).await?;
    assert_toggle_state(cx, window, true)?;
    checkpoint(
        cx,
        window,
        capture,
        &mut records,
        "sftp-follow-enabled",
        "enabled_synced_current_terminal_directory",
    )
    .await?;

    send_cd(cx, root, window, services, host, &binding, &unicode).await?;
    let unicode_enabled = ExpectedState {
        host,
        binding: &binding,
        terminal_directory: &unicode,
        enabled: true,
        listing_directory: &unicode,
        marker: "rr-ui-unicode",
    };
    wait_state(cx, root, window, unicode_enabled).await?;
    checkpoint(
        cx,
        window,
        capture,
        &mut records,
        "sftp-follow-unicode",
        "unicode_and_space_listing_committed",
    )
    .await?;

    for directory in [&first, &unicode, &first] {
        send_cd(cx, root, window, services, host, &binding, directory).await?;
    }
    wait_state(cx, root, window, first_enabled).await?;
    assert_stable(cx, root, window, first_enabled).await?;
    checkpoint(
        cx,
        window,
        capture,
        &mut records,
        "sftp-follow-rapid",
        "rapid_switch_converged_and_remained_stable",
    )
    .await?;

    click_visible_element(cx, window, "files-follow-terminal-1")?;
    send_cd(cx, root, window, services, host, &binding, &last).await?;
    let last_disabled = ExpectedState {
        host,
        binding: &binding,
        terminal_directory: &last,
        enabled: false,
        listing_directory: &first,
        marker: "rr-ui-first",
    };
    wait_state(cx, root, window, last_disabled).await?;
    assert_stable(cx, root, window, last_disabled).await?;
    click_visible_element(cx, window, "files-follow-terminal-1")?;
    let last_enabled = ExpectedState {
        host,
        binding: &binding,
        terminal_directory: &last,
        enabled: true,
        listing_directory: &last,
        marker: "rr-ui-last",
    };
    wait_state(cx, root, window, last_enabled).await?;
    checkpoint(
        cx,
        window,
        capture,
        &mut records,
        "sftp-follow-reenabled",
        "disabled_preserved_then_reenabled_synced",
    )
    .await?;

    // 从真实地址输入框开始遍历，避免把终端内用于补全的 Tab 误当作工具栏焦点导航。
    click_path_field(cx, window)?;
    let mut focused = false;
    for _ in 0..16 {
        focused = window.update(cx, |_, window, _| {
            let id = "files-follow-terminal-1".into();
            gpui_kit::base::test_support::find(window, &[], &id)
                .is_some_and(|button| button.focused() == Some(true))
        })?;
        if focused {
            break;
        }
        press_key(cx, window, "tab")?;
    }
    if !focused {
        return Err(io::Error::other("跟随开关无法通过 Tab 聚焦").into());
    }
    press_key(cx, window, "space")?;
    wait_state(
        cx,
        root,
        window,
        ExpectedState {
            enabled: false,
            ..last_enabled
        },
    )
    .await?;
    assert_toggle_state(cx, window, false)?;
    press_key(cx, window, "enter")?;
    wait_state(cx, root, window, last_enabled).await?;
    assert_toggle_state(cx, window, true)?;
    checkpoint(
        cx,
        window,
        capture,
        &mut records,
        "sftp-follow-keyboard",
        "tab_space_enter_and_accessible_toggle_state_verified",
    )
    .await?;

    for (theme, operation) in [
        (ThemeMode::Light, "sftp-follow-light"),
        (ThemeMode::Dark, "sftp-follow-dark"),
        (ThemeMode::System, "sftp-follow-system"),
    ] {
        window.update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.set_right_rail_acceptance_theme(theme, cx)
            });
        })?;
        let _ = frame_signature(cx, window)?;
        window.update(cx, |_, window, _| {
            let follow_id: gpui_kit::ElementId = "files-follow-terminal-1".into();
            let obsolete_id: gpui_kit::ElementId = "files-setup-follow-terminal-1".into();
            if gpui_kit::base::test_support::find(window, &[], &follow_id)
                .is_none_or(|button| !button.visible() || button.checked() != Some(true))
                || gpui_kit::base::test_support::find(window, &[], &obsolete_id).is_some()
            {
                return Err(io::Error::other("跟随开关缺失或旧配置按钮仍然存在"));
            }
            Ok(())
        })??;
        checkpoint(
            cx,
            window,
            capture,
            &mut records,
            operation,
            "follow_visible_setup_button_absent",
        )
        .await?;
    }
    Ok(records)
}

/// 按真实无障碍节点点击地址框，避免依赖 Release 不保留的调试标签或直接设置焦点。
///
/// @author kongweiguang
fn click_path_field(cx: &mut AsyncApp, window: AnyWindowHandle) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        let position = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .find(|node| {
                node.visible() && matches!(node.label(), Some("目录路径" | "Directory path"))
            })
            .ok_or_else(|| io::Error::other("未找到目录地址输入框"))?
            .bounds()
            .center();
        for event in [
            MouseMoveEvent {
                position,
                ..Default::default()
            }
            .to_platform_input(),
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
        ] {
            let _ = window.dispatch_event(event, app);
        }
        Ok::<_, io::Error>(())
    })??;
    let _ = frame_signature(cx, window)?;
    Ok(())
}

/// 从实际绘制节点读取开关状态，确保辅助技术获得的信息与目录跟随行为一致。
///
/// @author kongweiguang
fn assert_toggle_state(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    enabled: bool,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, _| {
        let id = "files-follow-terminal-1".into();
        let button = gpui_kit::base::test_support::find(window, &[], &id)
            .ok_or_else(|| io::Error::other("未找到真实目录跟随开关"))?;
        if !button.visible() || button.checked() != Some(enabled) || button.label().is_none() {
            return Err(io::Error::other("目录跟随开关的可见或无障碍状态错误"));
        }
        Ok(())
    })??;
    Ok(())
}

/// 键盘输入只分发到私有窗口，完整按下与释放让生产焦点和按键绑定决定动作。
///
/// @author kongweiguang
fn press_key(cx: &mut AsyncApp, window: AnyWindowHandle, key: &str) -> Result<(), Box<dyn Error>> {
    let keystroke = Keystroke::parse(key).map_err(io::Error::other)?;
    window.update(cx, |_, window, app| {
        let _ = window.dispatch_event(
            KeyDownEvent {
                keystroke: keystroke.clone(),
                is_held: false,
                prefer_character_input: false,
            }
            .to_platform_input(),
            app,
        );
        let _ = window.dispatch_event(KeyUpEvent { keystroke }.to_platform_input(), app);
    })?;
    let _ = frame_signature(cx, window)?;
    Ok(())
}

/// 会话身份必须仍与起始 pane 一致；输入经现有 SessionService 顺序写入真实 SSH shell。
///
/// @author kongweiguang
async fn send_cd(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &AppServices,
    host: &HostId,
    expected: &UiSessionBinding,
    directory: &str,
) -> Result<(), Box<dyn Error>> {
    poll_root(cx, root, window, "核对终端绑定", |root, _, _| {
        let Some((binding, _)) = root.right_rail_acceptance_follow_terminal_snapshot(host)? else {
            return Ok(None);
        };
        if &binding != expected {
            return Err("跟随验收期间终端会话身份已变化".to_owned());
        }
        Ok(Some(()))
    })
    .await?;
    if directory
        .chars()
        .any(|character| matches!(character, '\'' | '\n' | '\r'))
    {
        return Err(io::Error::other("fixture 目录不能包含 shell 引号或换行").into());
    }
    let uri = encode_osc7_path(directory);
    let command = format!(
        "builtin cd -- '{directory}' && builtin printf '\\033]7;file://localhost%s\\007' '{uri}'\r"
    );
    let session_id = expected.session_id.clone();
    let sessions = services.sessions.clone();
    services
        .executor
        .run(move |_| async move { sessions.write(session_id, command.into_bytes()).await })
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(())
}

/// 按 UTF-8 字节编码 URI 路径，保留斜杠并覆盖空格与非 ASCII 目录名。
///
/// @author kongweiguang
fn encode_osc7_path(path: &str) -> String {
    let mut encoded = String::new();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// 将 OSC7 元数据和已提交 listing 独立核对，不能以地址栏或异步请求受理替代结果。
///
/// @author kongweiguang
async fn wait_state(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    expected: ExpectedState<'_>,
) -> Result<(), Box<dyn Error>> {
    poll_root(
        cx,
        root,
        window,
        "OSC7 与 SFTP listing 收敛",
        |root, _, cx| state_matches(root, cx, expected).map(|ready| ready.then_some(())),
    )
    .await
}

/// 对目标主机、会话代次、开关和独有目录项逐项核对，避免两个不同目录的空清单误判。
///
/// @author kongweiguang
fn state_matches(
    root: &RootView,
    cx: &gpui_kit::Context<RootView>,
    expected: ExpectedState<'_>,
) -> Result<bool, String> {
    let Some((actual, cwd, target, actual_enabled)) =
        root.right_rail_acceptance_follow_snapshot(expected.host, cx)?
    else {
        return Ok(false);
    };
    if &actual != expected.binding {
        return Err("跟随验收期间终端会话身份已变化".to_owned());
    }
    if cwd.as_deref() != Some(expected.terminal_directory)
        || target.as_deref() != Some(expected.terminal_directory)
        || actual_enabled != expected.enabled
        || !root.right_rail_acceptance_sftp_directory_ready(
            expected.host,
            expected.listing_directory,
            cx,
        )?
    {
        return Ok(false);
    }
    root.right_rail_acceptance_sftp_file_entry_ready(
        expected.host,
        expected.listing_directory,
        expected.marker,
        true,
        cx,
    )
}

/// 收敛后继续抽样，捕捉迟到的旧目录读取覆盖最终状态。
///
/// @author kongweiguang
async fn assert_stable(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    expected: ExpectedState<'_>,
) -> Result<(), Box<dyn Error>> {
    for _ in 0..8 {
        cx.background_executor()
            .timer(Duration::from_millis(150))
            .await;
        let _ = frame_signature(cx, window)?;
        let ready = window
            .update(cx, |_, _, app| {
                root.update(app, |root, cx| state_matches(root, cx, expected))
            })?
            .map_err(io::Error::other)?;
        if !ready {
            return Err(io::Error::other("跟随目录在收敛后发生回退").into());
        }
    }
    Ok(())
}
