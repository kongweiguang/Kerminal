//! 隔离 fixture 的真实右栏操作回读；不向操作系统发送鼠标或键盘事件。
//!
//! @author kongweiguang

use super::*;
use gpui_kit::Window;
use gpui_pre::{InputEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FunctionalRecord {
    operation: &'static str,
    outcome: &'static str,
    evidence: CaptureRecord,
}

/// 每个动作只在生产视图接受后推进，读取等待仍驱动真实绘制与异步回执，不直接改业务快照。
///
/// @author kongweiguang
pub(super) async fn poll_root<T>(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    label: &str,
    mut operation: impl FnMut(
        &mut RootView,
        &mut Window,
        &mut Context<RootView>,
    ) -> Result<Option<T>, String>,
) -> Result<T, Box<dyn Error>> {
    let deadline = Instant::now() + PANEL_FRAME_TIMEOUT;
    loop {
        let result = window
            .update(cx, |_, window, app| {
                root.update(app, |root, root_cx| operation(root, window, root_cx))
            })?
            .map_err(io::Error::other)?;
        if let Some(result) = result {
            let _ = frame_signature(cx, window)?;
            return Ok(result);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待真实操作 {label} 回读超时")).into());
        }
        let _ = frame_signature(cx, window)?;
        cx.background_executor().timer(ACCEPTANCE_POLL).await;
    }
}

/// 只把 readback 条件已满足的状态存为证据；接口受理和弹框打开不冒充业务完成。
///
/// @author kongweiguang
pub(super) async fn checkpoint(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    operation: &'static str,
    outcome: &'static str,
) -> Result<(), Box<dyn Error>> {
    let evidence = wait_for_stable_frame(
        cx,
        window,
        capture
            .output_directory
            .join(format!("functional-{operation}.png")),
        None,
        operation,
    )
    .await?;
    records.push(FunctionalRecord {
        operation,
        outcome,
        evidence,
    });
    Ok(())
}

/// 同一窗口事件循环内绘制刷新中状态，避免快速 SFTP 回执掩盖列表闪烁。
///
/// @author kongweiguang
fn capture_refresh_in_flight(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    operation: &'static str,
) -> Result<(CaptureRecord, usize), Box<dyn Error>> {
    let path = capture
        .output_directory
        .join(format!("functional-{operation}.png"));
    let host = &capture.fixture_host_id;
    let directory = capture.remote_sftp_directory.as_str();
    let result = window.update(cx, |_, window, app| {
        let clear = window.draw(app);
        clear.clear(app);
        let before_state = root
            .update(app, |root, cx| {
                root.right_rail_acceptance_sftp_refresh_snapshot(host, directory, cx)
            })
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("Fixture listing is unavailable before refresh"))?;
        if before_state.0 || before_state.1 == 0 || !before_state.2 {
            return Err(io::Error::other(
                "Fixture listing or operation status is not ready before refresh",
            ));
        }
        let row = "files-row-1-fixture-readme.txt";
        if window.debug_selector_bounds(row).is_none() {
            return Err(io::Error::other(
                "Fixture file row is not visible before refresh",
            ));
        }
        let button_id = gpui_kit::ElementId::from("files-refresh-1");
        let button = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .find(|item| item.path().contains(&button_id))
            .ok_or_else(|| io::Error::other("Refresh button is not mounted"))?;
        if button.disabled() == Some(true) || !button.supports_click() {
            return Err(io::Error::other(
                "Refresh button is not enabled before refresh",
            ));
        }
        let download = window
            .debug_selector_bounds("files-download-1")
            .ok_or_else(|| io::Error::other("Download button geometry is unavailable"))?;
        let more = window
            .debug_selector_bounds("rr-files-view-menu")
            .ok_or_else(|| io::Error::other("File view menu geometry is unavailable"))?;
        let scale = window.scale_factor();
        let left = (download.right().as_f32() * scale).ceil() as u32;
        let right = (more.left().as_f32() * scale).floor() as u32;
        let top = (download.top().as_f32() * scale).ceil() as u32;
        let bottom = (download.bottom().as_f32() * scale).floor() as u32;
        let before_image = window.render_to_image().map_err(io::Error::other)?;
        if right <= left
            || bottom <= top
            || right > before_image.width()
            || bottom > before_image.height()
        {
            return Err(io::Error::other(
                "Refresh count comparison region is invalid",
            ));
        }
        let position = button.bounds().center();
        for event in [
            MouseMoveEvent {
                position,
                ..MouseMoveEvent::default()
            }
            .to_platform_input(),
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..MouseDownEvent::default()
            }
            .to_platform_input(),
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..MouseUpEvent::default()
            }
            .to_platform_input(),
        ] {
            let _ = window.dispatch_event(event, app);
        }
        let during = root
            .update(app, |root, cx| {
                root.right_rail_acceptance_sftp_refresh_snapshot(host, directory, cx)
            })
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("Refresh discarded the committed fixture listing"))?;
        if !during.0 || during.1 != before_state.1 || !during.2 {
            return Err(io::Error::other(
                "Refresh did not retain listing and quiet operation status",
            ));
        }
        let clear = window.draw(app);
        clear.clear(app);
        if window.debug_selector_bounds(row).is_none() {
            return Err(io::Error::other(
                "Refresh removed the visible fixture file row",
            ));
        }
        let busy_button = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .find(|item| item.path().contains(&button_id))
            .ok_or_else(|| io::Error::other("Refresh button disappeared while loading"))?;
        if busy_button.supports_click() {
            return Err(io::Error::other(
                "Refresh button still exposes click while the request is pending",
            ));
        }
        for event in [
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..MouseDownEvent::default()
            }
            .to_platform_input(),
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..MouseUpEvent::default()
            }
            .to_platform_input(),
        ] {
            let _ = window.dispatch_event(event, app);
        }
        let after_retry = root
            .update(app, |root, cx| {
                root.right_rail_acceptance_sftp_refresh_snapshot(host, directory, cx)
            })
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("Repeated refresh discarded the committed listing"))?;
        if !after_retry.0 || after_retry.3 != during.3 {
            return Err(io::Error::other(
                "Busy refresh accepted a duplicate request",
            ));
        }
        let image = window.render_to_image().map_err(io::Error::other)?;
        for y in top..bottom {
            for x in left..right {
                if before_image.get_pixel(x, y) != image.get_pixel(x, y) {
                    return Err(io::Error::other(
                        "Refresh changed the visible item count region",
                    ));
                }
            }
        }
        let width = image.width();
        let height = image.height();
        let pixel_sha256 = format!("{:x}", Sha256::digest(image.as_raw()));
        image.save(&path).map_err(io::Error::other)?;
        Ok::<_, io::Error>((
            CaptureRecord {
                file: path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .ok_or_else(|| io::Error::other("Invalid refresh screenshot path"))?
                    .to_owned(),
                width,
                height,
                pixel_sha256,
            },
            before_state.1,
        ))
    })??;
    Ok(result)
}

/// 独立专项只检查同目录刷新，在浅色、深色和跟随系统主题下等待真实回执。
///
/// @author kongweiguang
async fn run_refresh_only(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
) -> Result<Vec<FunctionalRecord>, Box<dyn Error>> {
    let host = &capture.fixture_host_id;
    let directory = capture.remote_sftp_directory.as_str();
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.activate_right_rail_tool_for_acceptance(ToolRailToolId::Sftp, window, cx)
        });
    })?;
    navigate_and_wait_for_sftp_directory(cx, root, window, host, directory).await?;
    let mut records = Vec::new();
    for (mode, name) in RIGHT_RAIL_THEMES {
        window.update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.set_right_rail_acceptance_theme(mode, cx)
            })
        })?;
        let _ = frame_signature(cx, window)?;
        let (busy, settled) = match name {
            "light" => ("sftp-refresh-light-loading", "sftp-refresh-light-settled"),
            "dark" => ("sftp-refresh-dark-loading", "sftp-refresh-dark-settled"),
            "system" => ("sftp-refresh-system-loading", "sftp-refresh-system-settled"),
            _ => return Err(io::Error::other("Unknown refresh acceptance theme").into()),
        };
        let (evidence, count) = capture_refresh_in_flight(cx, root, window, capture, busy)?;
        records.push(FunctionalRecord {
            operation: busy,
            outcome: "same_directory_row_count_and_quiet_status_retained_while_button_busy",
            evidence,
        });
        poll_root(
            cx,
            root,
            window,
            "same-directory refresh completion",
            |root, _, cx| {
                let Some((loading, current_count, no_status, _generation)) =
                    root.right_rail_acceptance_sftp_refresh_snapshot(host, directory, cx)?
                else {
                    return Ok(None);
                };
                if loading {
                    return Ok(None);
                }
                if current_count != count || !no_status {
                    return Err(
                        "Refresh completed with changed fixture count or operation status"
                            .to_owned(),
                    );
                }
                Ok(Some(()))
            },
        )
        .await?;
        checkpoint(
            cx,
            window,
            capture,
            &mut records,
            settled,
            "same_directory_refresh_completed_without_success_footer",
        )
        .await?;
    }
    Ok(records)
}

/// 名称绑定本轮精确 SFTP fixture，目录创建后实测右键菜单，失败后由归属 fixture 清理兜底。
///
/// @author kongweiguang
/// 确认和取消点击生产按钮，再用精确 fixture listing 回读，避免直接调用 owner 掩盖事件重入。
async fn run_files(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    let host = &capture.fixture_host_id;
    let directory = capture.remote_sftp_directory.as_str();
    let run_name = capture
        .output_directory
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| io::Error::other("验收输出目录缺少唯一名称"))?;
    let name = format!("rr-ui-{run_name}");
    let renamed = format!("{name}-renamed");
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.activate_right_rail_tool_for_acceptance(ToolRailToolId::Sftp, window, cx);
        })
    })?;
    navigate_and_wait_for_sftp_directory(cx, root, window, host, directory).await?;
    poll_root(
        cx,
        root,
        window,
        "打开新建目录",
        |root, window, cx| {
            root.open_right_rail_acceptance_sftp_create_directory(
                host, directory, &name, window, cx,
            )
            .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    super::functional_transfer_workspace::click_visible_element(
        cx,
        window,
        "files-global-dialog-confirm",
    )?;
    poll_root(cx, root, window, "新建目录 listing", |root, _, cx| {
        root.right_rail_acceptance_sftp_file_entry_ready(host, directory, &name, true, cx)
            .map(|ready| ready.then_some(()))
    })
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-created",
        "created_directory_returned_by_production_listing",
    )
    .await?;
    super::functional_transfer_workspace::reveal_file_row(
        cx,
        window,
        capture,
        "files-browser-list-1",
        &format!("files-row-1-{name}"),
        "created-directory-menu",
    )
    .await?;
    super::menus::assert_file_context_menu(
        cx,
        window,
        &format!("files-row-1-{name}"),
        Some(&capture.output_directory.join("sftp-file-menu-focus.json")),
    )
    .await?;
    poll_root(cx, root, window, "打开重命名", |root, window, cx| {
        root.open_right_rail_acceptance_sftp_rename(host, directory, &name, &renamed, window, cx)
            .map(|ready| ready.then_some(()))
    })
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-rename-dialog",
        "production_rename_form_opened_for_created_directory",
    )
    .await?;
    super::functional_transfer_workspace::click_visible_element(
        cx,
        window,
        "files-global-dialog-confirm",
    )?;
    poll_root(cx, root, window, "重命名 listing", |root, _, cx| {
        let old_absent =
            root.right_rail_acceptance_sftp_file_entry_ready(host, directory, &name, false, cx)?;
        let new_present =
            root.right_rail_acceptance_sftp_file_entry_ready(host, directory, &renamed, true, cx)?;
        Ok((old_absent && new_present).then_some(()))
    })
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-renamed",
        "old_endpoint_absent_new_endpoint_present_in_listing",
    )
    .await?;
    for cancel in [true, false] {
        poll_root(cx, root, window, "打开删除确认", |root, _, cx| {
            root.open_right_rail_acceptance_sftp_delete_confirmation(host, directory, &renamed, cx)
                .map(|ready| ready.then_some(()))
        })
        .await?;
        if cancel {
            checkpoint(
                cx,
                window,
                capture,
                records,
                "sftp-delete-dialog",
                "production_delete_confirmation_contains_only_created_directory",
            )
            .await?;
        }
        super::functional_transfer_workspace::click_visible_element(
            cx,
            window,
            if cancel {
                "files-global-dialog-cancel"
            } else {
                "files-global-dialog-confirm"
            },
        )?;
        poll_root(cx, root, window, "删除结果 listing", |root, _, cx| {
            root.right_rail_acceptance_sftp_file_entry_ready(host, directory, &renamed, cancel, cx)
                .map(|ready| ready.then_some(()))
        })
        .await?;
        checkpoint(
            cx,
            window,
            capture,
            records,
            if cancel {
                "sftp-delete-cancelled"
            } else {
                "sftp-deleted"
            },
            if cancel {
                "directory_preserved_after_cancel"
            } else {
                "created_directory_absent_after_delete"
            },
        )
        .await?;
    }
    Ok(())
}

/// 创建、改名和结束都使用相同 session ID 回读；唯一名称从输出目录派生，拒绝复用种子会话。
///
/// @author kongweiguang
async fn run_tmux(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    let host = &capture.fixture_host_id;
    let run_name = capture
        .output_directory
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| io::Error::other("验收输出目录缺少唯一名称"))?;
    let name = format!("rr-ui-{run_name}");
    let renamed = format!("{name}-renamed");
    activate_dialog_tool(
        cx,
        root,
        window,
        ToolRailToolId::Tmux,
        host,
        "tmux functional",
    )
    .await?;
    poll_root(cx, root, window, "创建tmux会话", |root, window, cx| {
        root.create_right_rail_acceptance_tmux_session(host, &name, window, cx)
            .map(|ready| ready.then_some(()))
    })
    .await?;
    let session_id = poll_root(cx, root, window, "tmux创建回读", |root, _, cx| {
        root.right_rail_acceptance_tmux_created_session_id(host, &name, cx)
    })
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "tmux-created",
        "unique_session_name_and_id_returned_by_production_snapshot",
    )
    .await?;
    poll_root(cx, root, window, "改名tmux会话", |root, window, cx| {
        root.rename_right_rail_acceptance_tmux_session(
            host,
            &session_id,
            &name,
            &renamed,
            window,
            cx,
        )
        .map(|ready| ready.then_some(()))
    })
    .await?;
    poll_root(cx, root, window, "tmux改名回读", |root, _, cx| {
        root.right_rail_acceptance_tmux_rename_ready(host, &session_id, &renamed, cx)
            .map(|ready| ready.then_some(()))
    })
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "tmux-renamed",
        "same_session_id_returned_with_derived_name",
    )
    .await?;
    poll_root(cx, root, window, "结束tmux会话", |root, _, cx| {
        root.kill_right_rail_acceptance_tmux_session(host, &session_id, &renamed, cx)
            .map(|ready| ready.then_some(()))
    })
    .await?;
    poll_root(cx, root, window, "tmux结束回读", |root, _, cx| {
        root.right_rail_acceptance_tmux_kill_ready(host, &session_id, cx)
            .map(|ready| ready.then_some(()))
    })
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "tmux-ended",
        "created_session_id_absent_after_production_kill",
    )
    .await?;
    Ok(())
}

/// 原生截图之后串行操作同一 fixture；每一阶段都复核当前工具、主机和冻结目标。
/// 会切换 Tab 的流程必须恢复原终端；最后才测试保留未提交文本的 Fill，避免污染其它动作。
///
/// @author kongweiguang
pub(super) async fn run(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    services: &kerminal_ui::AppServices,
) -> Result<Vec<FunctionalRecord>, Box<dyn Error>> {
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.set_right_rail_acceptance_theme(ThemeMode::Light, cx)
        })
    })?;
    let mut records = Vec::new();
    super::functional_agent::run(cx, root, window, capture, &mut records).await?;
    run_files(cx, root, window, capture, &mut records).await?;
    super::functional_transfers::run(cx, root, window, capture, &mut records, &services.executor)
        .await?;
    super::functional_transfer_workspace::run(cx, root, window, capture, &mut records, services)
        .await?;
    run_tmux(cx, root, window, capture, &mut records).await?;
    super::functional_tmux_attach::run(cx, root, window, capture, &mut records).await?;
    super::functional_logs::run(cx, root, window, capture, &mut records).await?;
    super::functional_forwarding::run(cx, root, window, capture, &mut records, services).await?;
    let files = services
        .files
        .as_ref()
        .ok_or_else(|| io::Error::other("验收组合根缺少共享文件服务"))?;
    super::functional_compose::run(
        cx,
        root,
        window,
        capture,
        &mut records,
        files,
        &services.executor,
    )
    .await?;
    super::functional_containers::run(cx, root, window, capture, &mut records).await?;
    super::functional_snippets::run(cx, root, window, capture, &mut records).await?;
    Ok(records)
}

/// 跟随或刷新专项显式收窄执行范围；完整模式保留目录操作和传输、编辑器闭环。
///
/// @author kongweiguang
pub(super) async fn run_sftp_only(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    services: &kerminal_ui::AppServices,
) -> Result<Vec<FunctionalRecord>, Box<dyn Error>> {
    if std::env::var_os("KERMINAL_RIGHT_RAIL_FOLLOW_ONLY").is_some() {
        return super::functional_follow::run(cx, root, window, capture, services).await;
    }
    if std::env::var_os("KERMINAL_RIGHT_RAIL_REFRESH_ONLY").is_some() {
        return run_refresh_only(cx, root, window, capture).await;
    }
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.set_right_rail_acceptance_theme(ThemeMode::Light, cx)
        })
    })?;
    let mut records = Vec::new();
    run_files(cx, root, window, capture, &mut records).await?;
    super::functional_transfers::run(cx, root, window, capture, &mut records, &services.executor)
        .await?;
    super::functional_transfer_workspace::run(cx, root, window, capture, &mut records, services)
        .await?;
    Ok(records)
}
