//! 隔离 fixture 的真实右栏操作回读；不向操作系统发送鼠标或键盘事件。
//!
//! @author kongweiguang

use super::*;
use gpui_kit::Window;

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

/// 名称由本轮输出目录派生且只能在 manifest 的精确 SFTP 目录内使用，失败后由归属 fixture 清理兜底。
///
/// @author kongweiguang
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
    poll_root(cx, root, window, "提交新建目录", |root, _, cx| {
        root.confirm_right_rail_acceptance_sftp_name_action(host, directory, cx)
            .map(|ready| ready.then_some(()))
    })
    .await?;
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
    poll_root(cx, root, window, "提交重命名", |root, _, cx| {
        root.confirm_right_rail_acceptance_sftp_name_action(host, directory, cx)
            .map(|ready| ready.then_some(()))
    })
    .await?;
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
        poll_root(
            cx,
            root,
            window,
            if cancel {
                "取消删除"
            } else {
                "确认删除"
            },
            |root, _, cx| {
                if cancel {
                    root.cancel_right_rail_acceptance_sftp_delete_confirmation(
                        host, directory, &renamed, cx,
                    )
                } else {
                    root.confirm_right_rail_acceptance_sftp_delete(host, directory, &renamed, cx)
                }
                .map(|ready| ready.then_some(()))
            },
        )
        .await?;
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
    run_tmux(cx, root, window, capture, &mut records).await?;
    super::functional_tmux_attach::run(cx, root, window, capture, &mut records).await?;
    super::functional_logs::run(cx, root, window, capture, &mut records).await?;
    super::functional_forwarding::run(cx, root, window, capture, &mut records).await?;
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
