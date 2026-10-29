//! Verifies the production tmux Attach and header exit against one isolated fixture session.
//!
//! @author kongweiguang

use std::{error::Error, ffi::OsStr, io, path::Path};

use gpui_kit::{AnyWindowHandle, AsyncApp, Entity};
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::identity::HostId;
use kerminal_ui::RootView;
use sha2::{Digest, Sha256};

use super::{
    RightRailCaptureContext, ToolRailToolId, activate_dialog_tool,
    functional::{FunctionalRecord, checkpoint, poll_root},
};

/// Runs a real tmux client through the production Attach and header exit requests, then removes only its session.
///
/// @author kongweiguang
pub(super) async fn run(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    let host_id = &capture.fixture_host_id;
    let session_name = fixture_tmux_session_name(&capture.output_directory)?;
    activate_dialog_tool(
        cx,
        root,
        window,
        ToolRailToolId::Tmux,
        host_id,
        "tmux attach functional",
    )
    .await?;
    let fixture_binding = poll_root(
        cx,
        root,
        window,
        "fixture tmux pane binding",
        |root, _, _| root.right_rail_acceptance_tmux_fixture_binding(host_id),
    )
    .await?;

    let preexisting_session = poll_root(
        cx,
        root,
        window,
        "unique tmux session preflight",
        |root, _, cx| {
            root.right_rail_acceptance_tmux_session_id_for_name(
                host_id,
                &session_name,
                &fixture_binding,
                cx,
            )
        },
    )
    .await?;
    if let Some(session_id) = preexisting_session {
        return Err(io::Error::other(format!(
            "唯一验收 session 名称已存在，拒绝复用：{session_id}"
        ))
        .into());
    }

    let mut cleanup = TmuxCleanupState::default();
    let flow_result = run_attach_detach_flow(
        cx,
        root,
        window,
        capture,
        (&fixture_binding, &session_name),
        records,
        &mut cleanup,
    )
    .await;

    let restore_before_cleanup =
        restore_fixture_tab(cx, root, window, host_id, &fixture_binding).await;
    let cleanup_result = if cleanup.creation_attempted {
        cleanup_fixture_tmux_session(
            cx,
            root,
            window,
            host_id,
            &fixture_binding,
            &session_name,
            cleanup.created_session_id.as_deref(),
        )
        .await
    } else {
        Ok(())
    };
    let restore_after_cleanup =
        restore_fixture_tab(cx, root, window, host_id, &fixture_binding).await;

    let mut failures = Vec::new();
    if let Err(error) = flow_result {
        failures.push(format!("Attach/detach flow: {error}"));
    }
    if let Err(error) = restore_before_cleanup {
        failures.push(format!("fixture tab restore before cleanup: {error}"));
    }
    if let Err(error) = cleanup_result {
        failures.push(format!("fixture tmux cleanup: {error}"));
    }
    if let Err(error) = restore_after_cleanup {
        failures.push(format!("fixture tab restore after cleanup: {error}"));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(failures.join("; ")).into())
    }
}

/// Tracks cleanup ownership even when the creation acknowledgement or subsequent readback fails.
#[derive(Default)]
struct TmuxCleanupState {
    creation_attempted: bool,
    created_session_id: Option<String>,
}

/// Creates one detached session, attaches through the production event, and requires service-side client readback.
///
/// @author kongweiguang
async fn run_attach_detach_flow(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    fixture: (&UiSessionBinding, &str),
    records: &mut Vec<FunctionalRecord>,
    cleanup: &mut TmuxCleanupState,
) -> Result<(), Box<dyn Error>> {
    let host_id = &capture.fixture_host_id;
    let (fixture_binding, session_name) = fixture;
    cleanup.creation_attempted = true;
    poll_root(
        cx,
        root,
        window,
        "create unique tmux session",
        |root, window, cx| {
            root.create_right_rail_acceptance_tmux_session(host_id, session_name, window, cx)
                .map(|accepted| accepted.then_some(()))
        },
    )
    .await?;

    let session_id = poll_root(
        cx,
        root,
        window,
        "read created tmux session id",
        |root, _, cx| match root.right_rail_acceptance_tmux_session_id_for_name(
            host_id,
            session_name,
            fixture_binding,
            cx,
        )? {
            Some(Some(session_id)) => Ok(Some(session_id)),
            None | Some(None) => Ok(None),
        },
    )
    .await?;
    cleanup.created_session_id = Some(session_id.clone());

    wait_for_tmux_client_state(
        cx,
        root,
        window,
        (host_id, fixture_binding, &session_id, session_name),
        "new tmux session has no clients",
        |attached, clients| !attached && clients == 0,
    )
    .await?;

    poll_root(
        cx,
        root,
        window,
        "production tmux Attach request",
        |root, _, cx| {
            root.request_right_rail_acceptance_tmux_attach(
                host_id,
                &session_id,
                session_name,
                fixture_binding,
                cx,
            )
            .map(|accepted| accepted.then_some(()))
        },
    )
    .await?;

    wait_for_tmux_client_state(
        cx,
        root,
        window,
        (host_id, fixture_binding, &session_id, session_name),
        "production Attach client readback",
        |attached, clients| attached || clients > 0,
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "tmux-attached-client",
        "production_attach_and_service_attached_or_positive_client_count",
    )
    .await?;

    let exit_requested = window.update(cx, |_, _, app| {
        root.update(app, |root, root_cx| {
            root.request_right_rail_acceptance_tmux_header_exit(
                host_id,
                &session_id,
                session_name,
                fixture_binding,
                root_cx,
            )
        })
    })?;
    if !exit_requested.map_err(io::Error::other)? {
        return Err(io::Error::other("生产 tmux header exit request was not accepted").into());
    }

    poll_root(
        cx,
        root,
        window,
        "production header exit readback",
        |root, _, cx| {
            let idle = root.right_rail_acceptance_tmux_detach_idle(host_id, fixture_binding, cx)?;
            let state = root.right_rail_acceptance_tmux_session_clients(
                host_id,
                &session_id,
                session_name,
                fixture_binding,
                cx,
            )?;
            if idle && state.is_some_and(|(attached, clients)| !attached && clients == 0) {
                Ok(Some(()))
            } else {
                root.refresh_right_rail_acceptance_tmux_snapshot(host_id, fixture_binding, cx)?;
                Ok(None)
            }
        },
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "tmux-header-detached",
        "same_session_service_clients_zero_after_production_ctrl_b_d",
    )
    .await?;
    Ok(())
}

/// Reads one real session snapshot before requesting another, so polling never cancels a completed read.
///
/// @author kongweiguang
async fn wait_for_tmux_client_state(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    fixture: (&HostId, &UiSessionBinding, &str, &str),
    label: &'static str,
    matches: impl Fn(bool, u32) -> bool,
) -> Result<(bool, u32), Box<dyn Error>> {
    let (host_id, binding, session_id, session_name) = fixture;
    poll_root(cx, root, window, label, |root, _, cx| {
        let state = root.right_rail_acceptance_tmux_session_clients(
            host_id,
            session_id,
            session_name,
            binding,
            cx,
        )?;
        if let Some((attached, clients)) = state
            && matches(attached, clients)
        {
            return Ok(Some((attached, clients)));
        }
        root.refresh_right_rail_acceptance_tmux_snapshot(host_id, binding, cx)?;
        Ok(None)
    })
    .await
}

/// Removes only the session returned for this run's preflighted unique name and verifies its ID disappeared.
///
/// @author kongweiguang
async fn cleanup_fixture_tmux_session(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    binding: &UiSessionBinding,
    session_name: &str,
    expected_session_id: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let current_session_id = poll_root(
        cx,
        root,
        window,
        "lookup owned tmux session for cleanup",
        |root, _, cx| {
            root.right_rail_acceptance_tmux_session_id_for_name(host_id, session_name, binding, cx)
        },
    )
    .await?;
    let (session_id, already_absent) = match (expected_session_id, current_session_id) {
        (Some(expected), Some(current)) if expected != current => {
            return Err(io::Error::other(
                "cleanup refused because the unique fixture name resolved to another session ID",
            )
            .into());
        }
        (Some(expected), None) => (expected.to_owned(), true),
        (Some(expected), Some(_)) => (expected.to_owned(), false),
        (None, Some(current)) => (current, false),
        (None, None) => return Ok(()),
    };

    let detach_idle_error = poll_root(
        cx,
        root,
        window,
        "wait for tmux exit handler",
        |root, _, cx| {
            root.right_rail_acceptance_tmux_detach_idle(host_id, binding, cx)
                .map(|idle| idle.then_some(()))
        },
    )
    .await
    .err()
    .map(|error| error.to_string());
    if already_absent {
        poll_root(
            cx,
            root,
            window,
            "fixture tmux session already absent",
            |root, _, cx| {
                root.right_rail_acceptance_tmux_kill_ready(host_id, &session_id, cx)
                    .map(|ended| ended.then_some(()))
            },
        )
        .await?;
        if let Some(error) = detach_idle_error {
            return Err(io::Error::other(format!(
                "tmux session is absent but its exit acknowledgement did not settle: {error}"
            ))
            .into());
        }
        return Ok(());
    }
    poll_root(
        cx,
        root,
        window,
        "production cleanup of fixture tmux session",
        |root, _, cx| {
            root.kill_right_rail_acceptance_tmux_session(host_id, &session_id, session_name, cx)
                .map(|accepted| accepted.then_some(()))
        },
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "fixture tmux session cleanup readback",
        |root, _, cx| {
            root.right_rail_acceptance_tmux_kill_ready(host_id, &session_id, cx)
                .map(|ended| ended.then_some(()))
        },
    )
    .await?;
    if let Some(error) = detach_idle_error {
        return Err(io::Error::other(format!(
            "fixture tmux session was removed, but its exit acknowledgement did not settle: {error}"
        ))
        .into());
    }
    Ok(())
}

/// Restores and then reads back the exact original tab and pane binding after resource cleanup.
///
/// @author kongweiguang
async fn restore_fixture_tab(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    binding: &UiSessionBinding,
) -> Result<(), Box<dyn Error>> {
    window
        .update(cx, |_, window, app| {
            root.update(app, |root, root_cx| {
                root.restore_right_rail_acceptance_tmux_fixture_tab(
                    host_id, binding, window, root_cx,
                )
            })
        })?
        .map_err(io::Error::other)?;
    poll_root(
        cx,
        root,
        window,
        "restore original fixture tab binding",
        |root, _, _| {
            root.right_rail_acceptance_tmux_fixture_binding(host_id)
                .map(|current| current.filter(|current| current == binding).map(|_| ()))
        },
    )
    .await
}

/// Uses a stable lowercase digest of the newly created output directory so names stay unique and tmux-safe.
///
/// @author kongweiguang
fn fixture_tmux_session_name(output_directory: &Path) -> Result<String, io::Error> {
    let run_id = output_directory
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| io::Error::other("验收输出目录缺少唯一名称"))?;
    let digest = Sha256::digest(output_directory.as_os_str().to_string_lossy().as_bytes());
    let suffix = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if run_id.is_empty() {
        return Err(io::Error::other("验收输出目录名称不能为空"));
    }
    Ok(format!("rr-ui-{suffix}-attach"))
}
