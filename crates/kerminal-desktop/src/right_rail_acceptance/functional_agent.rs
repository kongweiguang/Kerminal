//! 验证 Agent 右栏的隔离 Custom CLI 新建、继续、改名、输出与归档闭环。
//!
//! @author kongweiguang

use std::{error::Error, io, time::Instant};

use gpui_kit::{AnyWindowHandle, AsyncApp, Entity};
use kerminal_application::{agents::AgentSessionView, preferences::ThemeMode};
use kerminal_domain::{
    agents::AgentSessionStatus,
    identity::{AgentSessionId, HostId, SessionId},
    sessions::SessionLifecycle,
};
use kerminal_ui::RootView;

use super::{
    ToolRailToolId, frame_signature,
    functional::{FunctionalRecord, checkpoint, poll_root},
};

/// 在隔离 output/config 中配置无网络 PowerShell marker CLI，并通过生产 AgentPanel 完成生命周期读回。
///
/// @author kongweiguang
pub(super) async fn run(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &super::RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    let fixture_tab_id = window.update(cx, |_, _, app| {
        root.read(app)
            .right_rail_acceptance_fixture_tab_id(&capture.fixture_host_id)
    })??;
    let result = run_agent_acceptance_flow(cx, root, window, capture, records).await;
    let restored = window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.restore_right_rail_acceptance_fixture_tab(&fixture_tab_id, window, root_cx)
        })
    })?;
    if !restored {
        let failure = result
            .as_ref()
            .err()
            .map(|error| format!("; Agent flow also failed: {error}"))
            .unwrap_or_default();
        return Err(io::Error::other(format!(
            "Could not restore the exact original fixture TabId {fixture_tab_id}{failure}"
        ))
        .into());
    }
    result
}

/// Runs the production Agent lifecycle while the outer driver retains responsibility for exact-tab restoration.
///
/// @author kongweiguang
async fn run_agent_acceptance_flow(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &super::RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    let host_id = &capture.fixture_host_id;
    activate_agent_panel(cx, root, window, host_id).await?;

    let setup = window.update(cx, |_, _, app| {
        root.update(app, |root, root_cx| {
            root.configure_right_rail_acceptance_agent(host_id, root_cx)
        })
    })??;
    poll_root(
        cx,
        root,
        window,
        "Agent launcher CAS、validator 和 catalog 读回",
        |root, _, cx| {
            root.right_rail_acceptance_agent_launcher_ready(host_id, setup.launcher_id(), cx)
                .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-custom-launcher-ready",
        "isolated_typed_preferences_cas_settings_validator_and_catalog_readback_confirmed",
    )
    .await?;

    let started = window.update(cx, |_, _, app| {
        root.update(app, |root, root_cx| {
            root.start_right_rail_acceptance_agent(host_id, setup.launcher_id(), root_cx)
        })
    })??;
    if !started {
        return Err(io::Error::other("Agent production create action did not start").into());
    }
    let custom_key = format!("custom:{}", setup.launcher_id());
    let created = wait_for_agent_view(
        cx,
        root,
        window,
        host_id,
        None,
        |view| {
            view.session
                .launcher_key
                .as_ref()
                .is_some_and(|key| key.as_str() == custom_key)
        },
        "new Custom Agent projection",
    )
    .await?;
    let agent_session_id = created.session.agent_session_id.clone();
    let created_terminal = wait_for_agent_view(
        cx,
        root,
        window,
        host_id,
        Some(&agent_session_id),
        |view| {
            view.terminal
                .as_ref()
                .is_some_and(|terminal| terminal.lifecycle == SessionLifecycle::Running)
        },
        "running Custom Agent PTY",
    )
    .await?;
    let terminal_session_id = created_terminal
        .terminal
        .as_ref()
        .map(|terminal| terminal.session_id.clone())
        .ok_or_else(|| io::Error::other("created Agent has no real terminal session"))?;
    let target_generation = created_terminal
        .session
        .target
        .as_ref()
        .map(|target| target.generation)
        .ok_or_else(|| io::Error::other("created Agent has no persisted target binding"))?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-created-running",
        "production_create_returned_custom_launcher_and_running_agent_owned_pty",
    )
    .await?;

    let continued = window.update(cx, |_, _, app| {
        root.update(app, |root, root_cx| {
            root.continue_right_rail_acceptance_agent(host_id, &agent_session_id, root_cx)
        })
    })??;
    if !continued {
        return Err(io::Error::other(
            "Agent production continue action did not accept the running session",
        )
        .into());
    }
    let continued_view = wait_for_agent_view(
        cx,
        root,
        window,
        host_id,
        Some(&agent_session_id),
        |view| {
            view.session
                .target
                .as_ref()
                .is_some_and(|target| target.generation > target_generation)
                && view.terminal.as_ref().is_some_and(|terminal| {
                    terminal.session_id == terminal_session_id
                        && terminal.lifecycle == SessionLifecycle::Running
                })
        },
        "explicit continue target-binding readback",
    )
    .await?;
    let continued_generation = continued_view
        .session
        .target
        .as_ref()
        .map(|target| target.generation)
        .ok_or_else(|| io::Error::other("continued Agent lost its target binding"))?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-continued",
        "explicit_continue_advanced_target_generation_without_replacing_the_running_pty",
    )
    .await?;

    let renamed_title = format!("Acceptance {}", &setup.output_marker()[9..]);
    let rename_started = window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.rename_right_rail_acceptance_agent(
                host_id,
                &agent_session_id,
                renamed_title.clone(),
                window,
                root_cx,
            )
        })
    })??;
    if !rename_started {
        return Err(io::Error::other("Agent production rename action did not start").into());
    }
    wait_for_agent_view(
        cx,
        root,
        window,
        host_id,
        Some(&agent_session_id),
        |view| {
            view.session.title == renamed_title
                && view
                    .session
                    .target
                    .as_ref()
                    .is_some_and(|target| target.generation >= continued_generation)
        },
        "renamed Agent title CAS readback",
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-renamed",
        "same_agent_session_id_returned_the_renamed_title_after_production_readback",
    )
    .await?;

    let (lifecycle, terminal_text) = wait_for_agent_output(
        cx,
        root,
        window,
        host_id,
        &agent_session_id,
        &terminal_session_id,
        AgentOutputExpectation {
            marker: setup.output_marker(),
            lifecycle: SessionLifecycle::Exited,
        },
    )
    .await?;
    if lifecycle != SessionLifecycle::Exited || !terminal_text.contains(setup.output_marker()) {
        return Err(io::Error::other(
            "Agent terminal did not return the exact marker and Exited state",
        )
        .into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-output-exited",
        "unique_marker_read_from_agent_renderer_snapshot_and_same_session_exited_normally",
    )
    .await?;

    poll_root(
        cx,
        root,
        window,
        "打开生产 Agent 删除确认",
        |root, window, cx| {
            if !root.right_rail_acceptance_agent_archive_confirmation_ready(
                host_id,
                &agent_session_id,
                cx,
            )? {
                root.archive_right_rail_acceptance_agent(host_id, &agent_session_id, window, cx)?;
            }
            root.right_rail_acceptance_agent_archive_confirmation_ready(
                host_id,
                &agent_session_id,
                cx,
            )
            .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-delete-confirmation",
        "production_delete_confirmation_mounted_for_exact_agent_before_submission",
    )
    .await?;
    assert_agent_delete_geometry(cx, window)?;
    capture_agent_delete_themes(cx, root, window, capture, records).await?;
    poll_root(
        cx,
        root,
        window,
        "生产 Agent 删除确认提交",
        |root, window, cx| {
            root.archive_right_rail_acceptance_agent(host_id, &agent_session_id, window, cx)
                .map(|submitted| submitted.then_some(()))
        },
    )
    .await?;
    let archived_view = wait_for_agent_view(
        cx,
        root,
        window,
        host_id,
        Some(&agent_session_id),
        |view| view.session.status == AgentSessionStatus::Archived,
        "archived Agent history readback",
    )
    .await?;
    if archived_view.session.agent_session_id != agent_session_id
        || archived_view.session.title != renamed_title
    {
        return Err(io::Error::other(
            "Agent archive readback changed the accepted identity or title",
        )
        .into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-archived",
        "production_archive_preserved_the_same_renamed_agent_history",
    )
    .await?;
    Ok(())
}

/// 在同一生产确认框切换应用主题，覆盖保留弹框的主题继承，最后恢复功能轮的深色基线。
/// 不修改用户的系统主题，跟随系统仅按隔离窗口实际 appearance 解析。
///
/// @author kongweiguang
async fn capture_agent_delete_themes(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &super::RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    for (mode, operation) in [
        (ThemeMode::Light, "agent-delete-confirmation-light"),
        (ThemeMode::System, "agent-delete-confirmation-system"),
        (ThemeMode::Dark, "agent-delete-confirmation-dark"),
    ] {
        window.update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.set_right_rail_acceptance_theme(mode, cx)
            });
        })?;
        checkpoint(
            cx,
            window,
            capture,
            records,
            operation,
            "same_production_delete_dialog_keeps_theme_and_centered_bounded_layout",
        )
        .await?;
        assert_agent_delete_geometry(cx, window)?;
    }
    Ok(())
}

/// 读取稳定帧的实际表面，证明自然内容按 main 居中且封顶，入场期间的变换不作为最终布局。
///
/// @author kongweiguang
fn assert_agent_delete_geometry(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, _| {
        let bounds = window
            .debug_selector_bounds("dialog-0")
            .ok_or_else(|| io::Error::other("Agent 删除确认实际表面未挂载"))?;
        let viewport = window.viewport_size();
        let expected_y = (viewport.height.as_f32() - bounds.size.height.as_f32()) / 2.0;
        let expected_x = (viewport.width.as_f32() - bounds.size.width.as_f32()) / 2.0;
        if (bounds.origin.y.as_f32() - expected_y).abs() > 0.75
            || (bounds.origin.x.as_f32() - expected_x).abs() > 0.75
            || bounds.size.height.as_f32() > 288.75
        {
            return Err(io::Error::other(format!(
                "Agent 删除确认未遵守 main 的居中/限高：{bounds:?}"
            )));
        }
        Ok(())
    })??;
    Ok(())
}

/// 激活 Agent 工具并先走正式新建会话入口，消除 fixture 可能携带的其它目标历史选择。
///
/// @author kongweiguang
async fn activate_agent_panel(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
) -> Result<(), Box<dyn Error>> {
    let ready = window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.activate_right_rail_tool_for_acceptance(
                ToolRailToolId::AgentLauncher,
                window,
                root_cx,
            );
            root.prepare_right_rail_acceptance_agent(host_id, root_cx)
        })
    })??;
    if !ready {
        return Err(
            io::Error::other("Agent panel could not enter its production new-chat state").into(),
        );
    }
    super::wait_for_panel_readiness(
        cx,
        root,
        window,
        ToolRailToolId::AgentLauncher,
        host_id,
        "Agent",
    )
    .await
}

/// 按稳定 Agent ID 读取 production history，并在加载結束后将真实状态错误带回验收报告。
///
/// @author kongweiguang
async fn wait_for_agent_view(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    agent_session_id: Option<&AgentSessionId>,
    predicate: impl Fn(&AgentSessionView) -> bool,
    label: &str,
) -> Result<AgentSessionView, Box<dyn Error>> {
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let (view, operation_state) = window.update(cx, |_, _, app| {
            root.update(app, |root, root_cx| {
                let view =
                    root.right_rail_acceptance_agent_view(host_id, agent_session_id, root_cx)?;
                let state = root.right_rail_acceptance_agent_operation_state(host_id, root_cx)?;
                Ok::<_, String>((view, state))
            })
        })??;
        if let Some(view) = view.filter(&predicate) {
            return Ok(view);
        }
        let (busy, loading, sessions_verified, status) = operation_state;
        if !busy
            && !loading
            && sessions_verified
            && let Some(status) = status.filter(|message| {
                message.contains("失败")
                    || message.contains("不可用")
                    || message.contains("拒绝")
                    || message.contains("failed")
                    || message.contains("unavailable")
            })
        {
            return Err(io::Error::other(format!("Agent {label} failed: {status}")).into());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("timed out waiting for Agent {label}")).into());
        }
        let _ = frame_signature(cx, window)?;
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// Keeps the expected output and terminal lifecycle together without weakening identity checks.
struct AgentOutputExpectation<'a> {
    marker: &'a str,
    lifecycle: SessionLifecycle,
}

/// 仅接受当前选中 Agent 和相同 PTY renderer snapshot 中可见的唯一输出 marker 与终态。
///
/// @author kongweiguang
async fn wait_for_agent_output(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    agent_session_id: &AgentSessionId,
    terminal_session_id: &SessionId,
    expectation: AgentOutputExpectation<'_>,
) -> Result<(SessionLifecycle, String), Box<dyn Error>> {
    let AgentOutputExpectation {
        marker: output_marker,
        lifecycle: expected_lifecycle,
    } = expectation;
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let output = window.update(cx, |_, _, app| {
            root.update(app, |root, _| {
                root.right_rail_acceptance_agent_output(
                    host_id,
                    agent_session_id,
                    terminal_session_id,
                )
            })
        })??;
        if let Some((lifecycle, text)) = output
            && lifecycle == expected_lifecycle
            && text.contains(output_marker)
        {
            return Ok((lifecycle, text));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "Agent terminal did not return marker {output_marker} with {expected_lifecycle:?} lifecycle"
            ))
            .into());
        }
        let _ = frame_signature(cx, window)?;
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}
