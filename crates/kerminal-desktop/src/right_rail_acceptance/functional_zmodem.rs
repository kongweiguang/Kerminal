//! 私有桌面与隔离 SSH fixture 的 ZMODEM 原生验收。
//!
//! @author kongweiguang

use std::{error::Error, io};

use gpui_kit::{
    AnyWindowHandle, AsyncApp, Entity, InputEvent, KeyDownEvent, KeyUpEvent, Keystroke,
};
use kerminal_application::sessions::{
    UiSessionBinding, ZMODEM_INPUT_BUSY, ZmodemDecision, ZmodemDirection, ZmodemPhase,
};
use kerminal_ui::{AppServices, RootView};

use super::{
    RIGHT_RAIL_THEMES, RightRailCaptureContext, frame_signature,
    functional::{FunctionalRecord, checkpoint, poll_root},
    functional_transfer_workspace::click_visible_element,
};

/// 只使用本轮 fixture 的 SSH 会话与路径，取消和恢复后的回车都走窗口输入路径。
pub(super) async fn run(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    services: &AppServices,
) -> Result<Vec<FunctionalRecord>, Box<dyn Error>> {
    let host = &capture.fixture_host_id;
    let remote = &capture.remote_sftp_directory;
    let (rz, sz) = fixture_commands()?;
    if !remote.starts_with("/run/kerminal-sftp-follow-")
        || !remote.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '/' | '-' | '_')
        })
    {
        return Err(io::Error::other("ZMODEM 验收只接受本轮隔离 SSH fixture 目录").into());
    }
    let binding = poll_root(cx, root, window, "ZMODEM fixture 会话", |root, _, _| {
        root.right_rail_acceptance_zmodem_observe(host)
            .map(|state| state.map(|(binding, _, _)| binding))
    })
    .await?;
    send(services, &binding, &format!("{rz}\r")).await?;
    poll_root(cx, root, window, "rz 上传提示", |root, window, cx| {
        let Some((actual, Some(status), _)) = root.right_rail_acceptance_zmodem_observe(host)?
        else {
            return Ok(None);
        };
        if actual != binding {
            return Err("ZMODEM 测试会话已变化".to_owned());
        }
        if status.direction != ZmodemDirection::Upload
            || status.phase != ZmodemPhase::WaitingForSelection
        {
            return Ok(None);
        }
        if !root.right_rail_acceptance_zmodem_focus(host, &binding, window, cx)? {
            return Ok(None);
        }
        Ok(Some(()))
    })
    .await?;
    assert_prompt(cx, window, &binding)?;
    let mut records = Vec::new();
    for ((mode, _), operation) in RIGHT_RAIL_THEMES.into_iter().zip([
        "zmodem-rz-prompt-light",
        "zmodem-rz-prompt-dark",
        "zmodem-rz-prompt",
    ]) {
        window.update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.set_right_rail_acceptance_theme(mode, cx);
            });
        })?;
        checkpoint(
            cx,
            window,
            capture,
            &mut records,
            operation,
            "same_pane_upload_prompt",
        )
        .await?;
    }
    press_terminal_key(cx, window, "escape")?;
    poll_root(cx, root, window, "rz 键盘取消", |root, _, _| {
        let Some((actual, Some(status), _)) = root.right_rail_acceptance_zmodem_observe(host)?
        else {
            return Ok(None);
        };
        if actual != binding {
            return Err("取消后终端会话已变化".to_owned());
        }
        if status.phase == ZmodemPhase::NeedsReconnect {
            return Err("取消后远端未退出".to_owned());
        }
        Ok((status.phase == ZmodemPhase::Cancelled).then_some(()))
    })
    .await?;
    send(services, &binding, "printf 'KERM_ZMOD_%s\\n' 'RECOVERED'").await?;
    press_terminal_key(cx, window, "enter")?;
    poll_root(
        cx,
        root,
        window,
        "取消后同一 shell 继续执行",
        |root, _, _| {
            let Some((actual, _, text)) = root.right_rail_acceptance_zmodem_observe(host)? else {
                return Ok(None);
            };
            if actual != binding {
                return Err("恢复后终端会话已变化".to_owned());
            }
            Ok(text.contains("KERM_ZMOD_RECOVERED").then_some(()))
        },
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        &mut records,
        "zmodem-rz-cancel",
        "escape_restores_same_shell",
    )
    .await?;

    let upload = capture.local_files_root.join("zmodem-upload.txt");
    let upload_path = upload.clone();
    services
        .executor
        .run(move |_| async move {
            tokio::fs::write(upload_path, b"kerminal-zmodem-upload")
                .await
                .map_err(|error| {
                    kerminal_application::AppError::infrastructure_with_source(
                        "无法创建验收上传文件",
                        error,
                    )
                })
        })
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    send(services, &binding, &format!("cd '{remote}' && {rz}\r")).await?;
    let transfer_id = poll_root(cx, root, window, "rz 上传选择", |root, _, _| {
        let Some((actual, Some(status), _)) = root.right_rail_acceptance_zmodem_observe(host)?
        else {
            return Ok(None);
        };
        if actual != binding {
            return Err("上传期间终端会话已变化".to_owned());
        }
        Ok((status.direction == ZmodemDirection::Upload
            && status.phase == ZmodemPhase::WaitingForSelection)
            .then_some(status.id))
    })
    .await?;
    let sessions = services.sessions.clone();
    let upload_binding = binding.clone();
    services
        .executor
        .run(move |_| async move {
            sessions
                .respond_zmodem(
                    upload_binding,
                    transfer_id,
                    ZmodemDecision::Upload(vec![upload]),
                )
                .await
        })
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    poll_root(cx, root, window, "上传完成", |root, _, _| {
        let Some((actual, Some(status), _)) = root.right_rail_acceptance_zmodem_observe(host)?
        else {
            return Ok(None);
        };
        if actual != binding || status.id != transfer_id {
            return Err("上传后终端会话或传输已变化".to_owned());
        }
        if matches!(
            status.phase,
            ZmodemPhase::Failed | ZmodemPhase::NeedsReconnect
        ) {
            return Err(format!("上传失败：{:?}", status.error));
        }
        Ok((status.phase == ZmodemPhase::Finished && status.completed == 1).then_some(()))
    })
    .await?;
    send(services, &binding, "printf 'kerminal-zmodem-upload' | cmp - zmodem-upload.txt && printf 'KERM_ZMOD_%s\\n' 'UPLOAD_MATCH'\r").await?;
    poll_root(cx, root, window, "上传字节校验", |root, _, _| {
        let Some((actual, _, text)) = root.right_rail_acceptance_zmodem_observe(host)? else {
            return Ok(None);
        };
        if actual != binding {
            return Err("上传校验时终端会话已变化".to_owned());
        }
        Ok(text.contains("KERM_ZMOD_UPLOAD_MATCH").then_some(()))
    })
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        &mut records,
        "zmodem-rz-upload",
        "uploaded_bytes_match",
    )
    .await?;

    let command = format!(
        "printf 'kerminal-zmodem-native' > '{remote}/zmodem-smoke.txt'; {sz} '{remote}/zmodem-smoke.txt'\r"
    );
    send(services, &binding, &command).await?;
    poll_root(cx, root, window, "sz 下载提示", |root, _, _| {
        let Some((actual, Some(status), _)) = root.right_rail_acceptance_zmodem_observe(host)?
        else {
            return Ok(None);
        };
        if actual != binding {
            return Err("下载期间终端会话已变化".to_owned());
        }
        Ok((status.direction == ZmodemDirection::Download
            && status.phase == ZmodemPhase::WaitingForSelection)
            .then_some(()))
    })
    .await?;
    assert_prompt(cx, window, &binding)?;
    checkpoint(
        cx,
        window,
        capture,
        &mut records,
        "zmodem-sz-prompt",
        "same_pane_download_prompt",
    )
    .await?;
    let directory = capture.local_files_root.clone();
    poll_root(
        cx,
        root,
        window,
        "选中隔离下载目录",
        |root, _, cx| {
            root.right_rail_acceptance_zmodem_download(host, &binding, directory.clone(), cx)
                .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    poll_root(cx, root, window, "下载完成", |root, _, _| {
        let Some((actual, Some(status), _)) = root.right_rail_acceptance_zmodem_observe(host)?
        else {
            return Ok(None);
        };
        if actual != binding {
            return Err("下载后终端会话已变化".to_owned());
        }
        if matches!(
            status.phase,
            ZmodemPhase::Failed | ZmodemPhase::NeedsReconnect
        ) {
            return Err(format!("下载失败：{:?}", status.error));
        }
        Ok((status.phase == ZmodemPhase::Finished && status.completed == 1).then_some(()))
    })
    .await?;
    let downloaded = directory.join("zmodem-smoke.txt");
    let bytes = services
        .executor
        .run(move |_| async move {
            tokio::fs::read(downloaded).await.map_err(|error| {
                kerminal_application::AppError::infrastructure_with_source(
                    "无法读取验收下载文件",
                    error,
                )
            })
        })
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    if bytes != b"kerminal-zmodem-native" {
        return Err(io::Error::other("下载内容与 fixture 字节不一致").into());
    }
    checkpoint(
        cx,
        window,
        capture,
        &mut records,
        "zmodem-sz-download",
        "downloaded_bytes_match",
    )
    .await?;
    verify_reconnect(
        cx,
        root,
        window,
        capture,
        services,
        &binding,
        &rz,
        &mut records,
    )
    .await?;
    Ok(records)
}

/// 用仍在执行的前台命令阻止新提示符，验证取消超时后可经生产按钮重连到同一个分屏。
#[expect(
    clippy::too_many_arguments,
    reason = "隔离验收需要同一窗口、fixture、会话和结果"
)]
async fn verify_reconnect(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    services: &AppServices,
    binding: &UiSessionBinding,
    rz: &str,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    let host = &capture.fixture_host_id;
    send(services, binding, &format!("{rz}; sleep 30\r")).await?;
    poll_root(
        cx,
        root,
        window,
        "等待需要重连的上传",
        |root, window, cx| {
            let Some((actual, Some(status), _)) =
                root.right_rail_acceptance_zmodem_observe(host)?
            else {
                return Ok(None);
            };
            if actual != *binding {
                return Err("取消前会话意外变化".to_owned());
            }
            Ok((status.direction == ZmodemDirection::Upload
                && status.phase == ZmodemPhase::WaitingForSelection
                && root.right_rail_acceptance_zmodem_focus(host, binding, window, cx)?)
            .then_some(()))
        },
    )
    .await?;
    press_terminal_key(cx, window, "escape")?;
    poll_root(
        cx,
        root,
        window,
        "未确认退出时显示重连",
        |root, _, _| {
            let Some((actual, Some(status), _)) =
                root.right_rail_acceptance_zmodem_observe(host)?
            else {
                return Ok(None);
            };
            if actual != *binding {
                return Err("重连前会话意外变化".to_owned());
            }
            Ok((status.phase == ZmodemPhase::NeedsReconnect).then_some(()))
        },
    )
    .await?;
    let blocked = send(services, binding, "printf 'KERM_ZMOD_%s\\n' 'UNEXPECTED'\r").await;
    if !blocked.is_err_and(|error| error.to_string().contains(ZMODEM_INPUT_BUSY)) {
        return Err(io::Error::other("未确认退出时仍可写入终端").into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "zmodem-reconnect-prompt",
        "input_held_until_reconnect",
    )
    .await?;
    click_visible_element(
        cx,
        window,
        &format!("zmodem-reconnect-{}", binding.session_id.as_str()),
    )?;
    let replacement = poll_root(
        cx,
        root,
        window,
        "重连后同一分屏获得新会话",
        |root, _, _| {
            if let Some(error) = root.right_rail_acceptance_reconnect_error(binding) {
                return Err(format!("ZMODEM 同分屏重连失败：{error}"));
            }
            let Some((actual, status, _)) = root.right_rail_acceptance_zmodem_observe(host)? else {
                return Ok(None);
            };
            if actual.owner != binding.owner {
                return Err("重连切换了分屏".to_owned());
            }
            Ok((actual != *binding && status.is_none()).then_some(actual))
        },
    )
    .await;
    if replacement.is_err() {
        checkpoint(
            cx,
            window,
            capture,
            records,
            "zmodem-reconnect-timeout",
            "replacement_not_observed",
        )
        .await?;
    }
    let replacement = replacement?;
    send(
        services,
        &replacement,
        "printf 'KERM_ZMOD_%s\\n' 'RECONNECTED'",
    )
    .await?;
    press_terminal_key(cx, window, "enter")?;
    poll_root(
        cx,
        root,
        window,
        "重连后新 shell 继续执行",
        |root, _, _| {
            let Some((actual, _, text)) = root.right_rail_acceptance_zmodem_observe(host)? else {
                return Ok(None);
            };
            Ok((actual == replacement && text.contains("KERM_ZMOD_RECONNECTED")).then_some(()))
        },
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "zmodem-reconnect",
        "same_pane_shell_after_reconnect",
    )
    .await
}

/// 默认使用远端 PATH；自定义路径仅接受安全绝对目录，避免拼接出 shell 语法。
fn fixture_commands() -> Result<(String, String), Box<dyn Error>> {
    let Some(directory) = std::env::var_os("KERMINAL_ZMODEM_TEST_LRZSZ_DIR") else {
        return Ok(("rz".into(), "sz".into()));
    };
    let directory = directory
        .to_str()
        .ok_or_else(|| io::Error::other("lrzsz 目录不是 UTF-8"))?;
    if !directory.starts_with('/')
        || !directory.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '/' | '-' | '_' | '.')
        })
    {
        return Err(io::Error::other("lrzsz 目录必须是安全的绝对路径").into());
    }
    let directory = directory.trim_end_matches('/');
    Ok((format!("{directory}/rz"), format!("{directory}/sz")))
}

/// 读取 Kit 的实际可见按钮，防止只凭后台状态误认浮层已显示。
fn assert_prompt(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    binding: &UiSessionBinding,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, _| {
        let id = format!("zmodem-select-{}", binding.session_id.as_str()).into();
        let button = gpui_kit::base::test_support::find(window, &[], &id)
            .ok_or_else(|| io::Error::other("未找到 ZMODEM 选择按钮"))?;
        if !button.visible() || button.label().is_none() {
            return Err(io::Error::other("ZMODEM 选择按钮不可见或缺少无障碍名称"));
        }
        Ok(())
    })??;
    Ok(())
}

/// 命令只写入已核对的 fixture 会话，等待真实终端和协议结果作为验收依据。
async fn send(
    services: &AppServices,
    binding: &UiSessionBinding,
    command: &str,
) -> Result<(), Box<dyn Error>> {
    let sessions = services.sessions.clone();
    let session_id = binding.session_id.clone();
    let bytes = command.as_bytes().to_vec();
    services
        .executor
        .run(move |_| async move { sessions.write(session_id, bytes).await })
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(())
}

/// 向私有窗口分发完整按键事件，由生产焦点和键盘处理决定按键去向。
fn press_terminal_key(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    key: &str,
) -> Result<(), Box<dyn Error>> {
    let _ = frame_signature(cx, window)?;
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
