//! 左栏折叠与 SFTP 生命周期的原生验收；使用生产鼠标事件、真实 SSH 和隔离数据根。
//!
//! @author kongweiguang

use std::{
    error::Error,
    io,
    time::{Duration, Instant},
};

use gpui_kit::{AnyWindowHandle, AsyncApp, Entity};
use kerminal_ui::RootView;

use super::{
    RightRailCaptureContext, frame_signature,
    functional::{FunctionalRecord, checkpoint, poll_root},
    functional_transfer_workspace::click_visible_element,
    save_frame,
};

/// 每帧比较目录请求代次、现存实体和会话身份；同时等待网格与保存收据，避免忽略短暂刷新。
///
/// @author kongweiguang
pub(super) async fn run_for_theme(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    theme: &str,
) -> Result<(), Box<dyn Error>> {
    let host = &capture.fixture_host_id;
    let directory = capture.remote_sftp_directory.as_str();
    let baseline = poll_root(
        cx,
        root,
        window,
        "左栏折叠前稳定布局",
        |root, _, _| {
            let Some(state) = root.right_rail_acceptance_sidebar_state(host)? else {
                return Ok(None);
            };
            if state.layout_failed {
                return Err("折叠前已有布局保存失败".into());
            }
            Ok((!state.layout_pending && state.palette_ready).then_some(state))
        },
    )
    .await?;
    let listing = window
        .update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.right_rail_acceptance_sftp_refresh_snapshot(host, directory, cx)
            })
        })?
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("折叠前目录未就绪"))?;
    if listing.0 || listing.1 == 0 || !listing.2 || baseline.collapsed {
        return Err(io::Error::other("折叠验收需要展开的主机栏和非空稳定目录").into());
    }
    save_frame(
        cx,
        window,
        capture
            .output_directory
            .join(format!("sidebar-{theme}-before.png")),
    )?;
    let mut previous_columns = baseline.terminal_columns;
    for collapsed in [true, false] {
        let _ = frame_signature(cx, window)?;
        click_visible_element(cx, window, "workbench-toggle-host-sidebar")?;
        let started = Instant::now();
        let mut sampled_frames = 0;
        loop {
            let _ = frame_signature(cx, window)?;
            let state = window.update(cx, |_, window, app| {
                root.update(app, |root, cx| {
                    let state = root.right_rail_acceptance_sidebar_state(host)?
                        .ok_or_else(|| "折叠期间活动会话或 SFTP 实体丢失".to_owned())?;
                    let current = root.right_rail_acceptance_sftp_refresh_snapshot(host, directory, cx)?
                        .ok_or_else(|| "折叠期间目录清单丢失".to_owned())?;
                    if current != listing || state.files_entity != baseline.files_entity
                        || state.binding != baseline.binding
                        || state.palette_generation != baseline.palette_generation
                        || !state.palette_ready
                        || state.collapsed != collapsed || state.layout_failed
                    {
                        return Err(format!("左栏布局影响了右栏或终端设置: {state:?}; listing={current:?}, expected={listing:?}"));
                    }
                    if window.debug_selector_bounds("files-row-1-fixture-readme.txt").is_none() {
                        return Err("折叠期间文件列表的现有行消失".to_owned());
                    }
                    Ok(state)
                })
            })?.map_err(io::Error::other)?;
            sampled_frames += 1;
            if sampled_frames == 1 {
                save_frame(
                    cx,
                    window,
                    capture
                        .output_directory
                        .join(format!("sidebar-{theme}-{collapsed}-immediate.png")),
                )?;
            }
            let resized = if collapsed {
                state.terminal_columns > previous_columns
            } else {
                state.terminal_columns < previous_columns
            };
            if resized && !state.layout_pending && started.elapsed() >= Duration::from_millis(450) {
                previous_columns = state.terminal_columns;
                break;
            }
            if started.elapsed() >= Duration::from_secs(10) {
                return Err(
                    io::Error::other(format!("折叠后网格或布局保存未完成: {state:?}")).into(),
                );
            }
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
        }
        let operation = match (theme, collapsed) {
            ("light", true) => "sftp-sidebar-light-collapsed",
            ("light", false) => "sftp-sidebar-light-expanded",
            ("dark", true) => "sftp-sidebar-dark-collapsed",
            ("dark", false) => "sftp-sidebar-dark-expanded",
            ("system", true) => "sftp-sidebar-system-collapsed",
            ("system", false) => "sftp-sidebar-system-expanded",
            _ => return Err(io::Error::other("未知的侧栏验收主题").into()),
        };
        checkpoint(cx, window, capture, records, operation,
            "sftp_entity_binding_listing_and_request_generation_retained_while_terminal_resized_and_layout_saved").await?;
    }
    Ok(())
}

/// 其它工具逐帧保留同一实体与目标；System 允许自然采样，但必须保留快照和监控世代。
///
/// @author kongweiguang
pub(super) async fn run_other_panels(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
) -> Result<(), Box<dyn Error>> {
    use kerminal_application::preferences::settings::ToolRailToolId;

    let host = &capture.fixture_host_id;
    let mut verified = Vec::new();
    for (tool, name) in super::RIGHT_RAIL_TOOLS {
        if tool == ToolRailToolId::Sftp {
            continue;
        }
        window.update(cx, |_, window, app| {
            root.update(app, |root, cx| {
                root.activate_right_rail_tool_for_acceptance(tool, window, cx)
            })
        })?;
        super::wait_for_panel_readiness(cx, root, window, tool, host, name).await?;
        let baseline = poll_root(
            cx,
            root,
            window,
            "工具切换后的稳定布局",
            |root, _, _| {
                let state = root.right_rail_acceptance_sidebar_state(host)?;
                Ok(state.filter(|state| !state.layout_pending && state.palette_ready))
            },
        )
        .await?;
        if baseline.collapsed || baseline.layout_failed || baseline.tool_entity.is_none() {
            return Err(io::Error::other("工具面板验收没有展开的稳定主机栏").into());
        }
        let system_lifecycle = if tool == ToolRailToolId::System {
            Some(
                window
                    .update(cx, |_, _, app| {
                        root.update(app, |root, cx| {
                            root.right_rail_acceptance_system_lifecycle(host, cx)
                        })
                    })?
                    .map_err(io::Error::other)?,
            )
        } else {
            None
        };
        let mut previous_columns = baseline.terminal_columns;
        for collapsed in [true, false] {
            let _ = frame_signature(cx, window)?;
            click_visible_element(cx, window, "workbench-toggle-host-sidebar")?;
            let started = Instant::now();
            loop {
                let _ = frame_signature(cx, window)?;
                let state = window
                    .update(cx, |_, _, app| {
                        root.update(app, |root, cx| {
                            let state = root
                                .right_rail_acceptance_sidebar_state(host)?
                                .ok_or_else(|| "折叠期间活动终端丢失".to_owned())?;
                            if state.collapsed != collapsed
                                || state.layout_failed
                                || !state.palette_ready
                                || state.binding != baseline.binding
                                || state.files_entity != baseline.files_entity
                                || state.tool_entity != baseline.tool_entity
                                || state.palette_generation != baseline.palette_generation
                                || if let Some(expected) = system_lifecycle {
                                    root.right_rail_acceptance_system_lifecycle(host, cx)?
                                        != expected
                                } else {
                                    !root.right_rail_acceptance_panel_ready(tool, host, cx)?
                                }
                            {
                                return Err(format!("折叠影响了 {name} 面板: {state:?}"));
                            }
                            Ok(state)
                        })
                    })?
                    .map_err(io::Error::other)?;
                let resized = if collapsed {
                    state.terminal_columns > previous_columns
                } else {
                    state.terminal_columns < previous_columns
                };
                if resized
                    && !state.layout_pending
                    && started.elapsed() >= Duration::from_millis(450)
                {
                    previous_columns = state.terminal_columns;
                    break;
                }
                if started.elapsed() >= Duration::from_secs(10) {
                    return Err(
                        io::Error::other(format!("{name} 折叠后布局未完成: {state:?}")).into(),
                    );
                }
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
            }
            save_frame(
                cx,
                window,
                capture
                    .output_directory
                    .join(format!("sidebar-other-{name}-{collapsed}.png")),
            )?;
        }
        verified.push(serde_json::json!({"tool": name, "collapsedAndExpanded": true}));
    }
    std::fs::write(
        capture.output_directory.join("sidebar-other-panels.json"),
        serde_json::to_vec_pretty(&verified)?,
    )?;
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.activate_right_rail_tool_for_acceptance(ToolRailToolId::Sftp, window, cx)
        })
    })?;
    super::navigate_and_wait_for_sftp_directory(
        cx,
        root,
        window,
        host,
        &capture.remote_sftp_directory,
    )
    .await?;
    Ok(())
}
