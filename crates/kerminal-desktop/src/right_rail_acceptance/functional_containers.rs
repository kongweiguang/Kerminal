//! 对固定 nginx fixture 验证生产容器状态、确认交互和日志入口。
//!
//! @author kongweiguang

use std::{error::Error, io};

use gpui_kit::{AnyWindowHandle, AsyncApp, Entity};
use kerminal_domain::{
    identity::{HostId, TabId},
    operations::{ContainerAction, ContainerStatus},
};
use kerminal_ui::RootView;

use super::{
    RightRailCaptureContext, ToolRailToolId, activate_dialog_tool,
    functional::{FunctionalRecord, checkpoint, poll_root},
};

/// 只允许本轮固定 fixture 容器通过生产入口改变状态；无论验收在哪步失败，都尝试恢复运行态。
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
    let name = &capture.fixture_container_name;
    validate_fixture_container_name(name)?;
    activate_dialog_tool(
        cx,
        root,
        window,
        ToolRailToolId::Containers,
        host_id,
        "containers functional",
    )
    .await?;

    let fixture_terminal_tab_id =
        poll_root(cx, root, window, "fixture terminal tab", |root, _, _| {
            root.right_rail_acceptance_compose_terminal_tab_id(host_id)
        })
        .await?;
    let (container_id, initial_status) = poll_root(
        cx,
        root,
        window,
        "fixture 容器初始状态",
        |root, _, cx| root.right_rail_acceptance_container_state(host_id, name, cx),
    )
    .await?;
    if initial_status != ContainerStatus::Running {
        return Err(io::Error::other(format!(
            "fixture 容器开始时必须运行，当前状态为 {initial_status:?}"
        ))
        .into());
    }

    let flow_result = run_container_flow(
        cx,
        root,
        window,
        capture,
        records,
        (&container_id, name, &fixture_terminal_tab_id),
    )
    .await;
    let cleanup_result =
        restore_fixture_running(cx, root, window, host_id, name, &container_id).await;

    match (flow_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(flow), Ok(())) => Err(flow),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(flow), Err(cleanup)) => Err(io::Error::other(format!(
            "容器功能验收失败：{flow}；fixture 运行态恢复失败：{cleanup}"
        ))
        .into()),
    }
}

/// 通过实际详情、确认、状态刷新和 StartedAt 回读，区分取消、停止、启动与重启结果。
///
/// @author kongweiguang
async fn run_container_flow(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    fixture: (&str, &str, &TabId),
) -> Result<(), Box<dyn Error>> {
    let host_id = &capture.fixture_host_id;
    let (container_id, name, fixture_terminal_tab_id) = fixture;
    let initial_started_at = read_started_at(cx, root, window, host_id, container_id).await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "container-details",
        "production_inspect_matches_fixture_host_runtime_and_container_id",
    )
    .await?;
    poll_root(cx, root, window, "返回容器列表", |root, _, cx| {
        root.return_right_rail_acceptance_container_list(host_id, cx)
            .map(|()| Some(()))
    })
    .await?;

    open_lifecycle_confirmation(cx, root, window, host_id, name, ContainerAction::Stop)?;
    wait_for_confirmation(cx, root, window, host_id, name, "停止确认取消").await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "container-stop-confirmation",
        "production_stop_confirmation_opened_for_exact_fixture_name",
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "取消生产停止确认",
        |root, _, cx| {
            root.right_rail_acceptance_cancel_container_confirmation(host_id, name, cx)
                .map(|()| Some(()))
        },
    )
    .await?;
    wait_for_container_status(
        cx,
        root,
        window,
        (host_id, name, container_id),
        ContainerStatus::Running,
        &[],
        "取消停止后状态回读",
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "container-stop-cancelled",
        "container_remains_running_after_production_confirmation_cancel",
    )
    .await?;

    open_lifecycle_confirmation(cx, root, window, host_id, name, ContainerAction::Stop)?;
    wait_for_confirmation(cx, root, window, host_id, name, "停止确认提交").await?;
    poll_root(
        cx,
        root,
        window,
        "提交生产停止确认",
        |root, _, cx| {
            root.right_rail_acceptance_confirm_container_confirmation(host_id, name, cx)
                .map(|()| Some(()))
        },
    )
    .await?;
    wait_for_container_status(
        cx,
        root,
        window,
        (host_id, name, container_id),
        ContainerStatus::Exited,
        &[ContainerStatus::Running, ContainerStatus::Restarting],
        "停止容器状态回读",
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "container-stopped",
        "same_container_id_is_exited_in_production_list_readback",
    )
    .await?;

    poll_root(cx, root, window, "启动 fixture 容器", |root, _, cx| {
        root.right_rail_acceptance_start_container(host_id, name, cx)
            .map(|()| Some(()))
    })
    .await?;
    wait_for_container_status(
        cx,
        root,
        window,
        (host_id, name, container_id),
        ContainerStatus::Running,
        &[
            ContainerStatus::Exited,
            ContainerStatus::Created,
            ContainerStatus::Restarting,
        ],
        "启动容器状态回读",
    )
    .await?;
    let started_at_after_start = read_started_at(cx, root, window, host_id, container_id).await?;
    if started_at_after_start == initial_started_at {
        return Err(io::Error::other(
            "启动后 StartedAt 未变化，拒绝以 Running 列表状态冒充启动完成",
        )
        .into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "container-started",
        "same_container_id_running_with_new_inspect_started_at",
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "启动后返回容器列表",
        |root, _, cx| {
            root.return_right_rail_acceptance_container_list(host_id, cx)
                .map(|()| Some(()))
        },
    )
    .await?;

    open_lifecycle_confirmation(cx, root, window, host_id, name, ContainerAction::Restart)?;
    wait_for_confirmation(cx, root, window, host_id, name, "重启确认").await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "container-restart-confirmation",
        "production_restart_confirmation_opened_for_exact_fixture_name",
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "提交生产重启确认",
        |root, _, cx| {
            root.right_rail_acceptance_confirm_container_confirmation(host_id, name, cx)
                .map(|()| Some(()))
        },
    )
    .await?;
    wait_for_container_status(
        cx,
        root,
        window,
        (host_id, name, container_id),
        ContainerStatus::Running,
        &[ContainerStatus::Restarting, ContainerStatus::Exited],
        "重启容器状态回读",
    )
    .await?;
    let restarted_at = read_started_at(cx, root, window, host_id, container_id).await?;
    if restarted_at == started_at_after_start {
        return Err(
            io::Error::other("重启后 StartedAt 未变化，拒绝以原 Running 快照冒充重启完成").into(),
        );
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "container-restarted",
        "same_container_id_running_with_new_inspect_started_at",
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "重启后返回容器列表",
        |root, _, cx| {
            root.return_right_rail_acceptance_container_list(host_id, cx)
                .map(|()| Some(()))
        },
    )
    .await?;

    let logs_result = async {
        poll_root(
            cx,
            root,
            window,
            "生产容器日志入口",
            |root, _, cx| {
                root.right_rail_acceptance_request_container_logs(host_id, name, cx)
                    .map(|()| Some(()))
            },
        )
        .await?;
        let logs = poll_root(
            cx,
            root,
            window,
            "fixture nginx 启动日志回读",
            |root, _, _| {
                root.right_rail_acceptance_container_logs_text(host_id, container_id)
                    .map(|text| {
                        text.filter(|text| {
                            text.contains("Configuration complete")
                                || text.contains("start worker process")
                        })
                    })
            },
        )
        .await?;
        if !logs.contains("Configuration complete") && !logs.contains("start worker process") {
            return Err(io::Error::other("fixture 容器日志缺少 nginx 固定启动标记").into());
        }
        wait_for_container_status(
            cx,
            root,
            window,
            (host_id, name, container_id),
            ContainerStatus::Running,
            &[],
            "日志请求后的 fixture 状态回读",
        )
        .await?;
        checkpoint(
            cx,
            window,
            capture,
            records,
            "container-logs-request",
            "exact_fixture_log_terminal_contains_nginx_startup_marker",
        )
        .await?;
        Ok::<(), Box<dyn Error>>(())
    }
    .await;
    let terminal_restore_result =
        restore_fixture_terminal_tab(cx, root, window, host_id, fixture_terminal_tab_id).await;
    match (logs_result, terminal_restore_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(logs), Ok(())) => Err(logs),
        (Ok(()), Err(restore)) => Err(restore),
        (Err(logs), Err(restore)) => Err(io::Error::other(format!(
            "容器日志回读失败：{logs}；fixture terminal tab 恢复失败：{restore}"
        ))
        .into()),
    }
}

/// 打开目标容器的正式 Details 页并等待 StartedAt 回执，保持相同 ID 贯穿状态变更。
///
/// @author kongweiguang
async fn read_started_at(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    container_id: &str,
) -> Result<String, Box<dyn Error>> {
    let opened = window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.open_right_rail_acceptance_container_page(host_id, container_id, "details", cx)
        })
    })?;
    opened.map_err(io::Error::other)?;
    poll_root(
        cx,
        root,
        window,
        "容器 StartedAt inspect 回读",
        |root, _, cx| root.right_rail_acceptance_container_started_at(host_id, container_id, cx),
    )
    .await
}

/// 容器日志会切换到临时 PTY；无论日志回读或截图失败都重新激活原 fixture tab。
///
/// @author kongweiguang
async fn restore_fixture_terminal_tab(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    terminal_tab_id: &TabId,
) -> Result<(), Box<dyn Error>> {
    poll_root(
        cx,
        root,
        window,
        "恢复 fixture terminal tab",
        |root, window, cx| {
            root.activate_right_rail_acceptance_compose_terminal_tab(
                host_id,
                terminal_tab_id,
                window,
                cx,
            )
            .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    Ok(())
}

/// 只经现有 Root 确认入口打开 Stop/Restart 对话框，随后独立等待实际确认状态。
///
/// @author kongweiguang
fn open_lifecycle_confirmation(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    name: &str,
    action: ContainerAction,
) -> Result<(), Box<dyn Error>> {
    let opened = window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.right_rail_acceptance_open_container_lifecycle_confirmation(
                host_id, name, action, window, cx,
            )
        })
    })?;
    opened.map_err(io::Error::other)?;
    Ok(())
}

/// 对话框打开事件与可见确认状态分开读取，失败时保留 Root 的真实目标校验错误。
///
/// @author kongweiguang
async fn wait_for_confirmation(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    name: &str,
    label: &str,
) -> Result<(), Box<dyn Error>> {
    poll_root(cx, root, window, label, |root, _, cx| {
        root.right_rail_acceptance_container_confirmation_ready(host_id, name, cx)
            .map(|ready| ready.then_some(()))
    })
    .await?;
    Ok(())
}

/// 允许有限的运行态过渡，但仅在同一容器 ID 到达预期状态后完成操作。
///
/// @author kongweiguang
async fn wait_for_container_status(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    fixture: (&HostId, &str, &str),
    expected: ContainerStatus,
    transitions: &[ContainerStatus],
    label: &str,
) -> Result<(), Box<dyn Error>> {
    let (host_id, name, expected_id) = fixture;
    poll_root(cx, root, window, label, |root, _, cx| {
        let Some((container_id, status)) =
            root.right_rail_acceptance_container_state(host_id, name, cx)?
        else {
            return Ok(None);
        };
        if container_id != expected_id {
            return Err("fixture 名称已解析为不同容器 ID，停止验收".to_owned());
        }
        if status == expected {
            return Ok(Some(()));
        }
        if transitions.contains(&status) {
            return Ok(None);
        }
        Err(format!(
            "fixture 容器状态不是预期值 {expected:?}，当前为 {status:?}"
        ))
    })
    .await?;
    Ok(())
}

/// 功能流程失败时只把同 ID 的 Exited fixture 启动回运行态，绝不删除或替换共享容器。
///
/// @author kongweiguang
async fn restore_fixture_running(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    name: &str,
    expected_id: &str,
) -> Result<(), Box<dyn Error>> {
    activate_dialog_tool(
        cx,
        root,
        window,
        ToolRailToolId::Containers,
        host_id,
        "container fixture restore",
    )
    .await?;
    let confirmation_open = window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.right_rail_acceptance_container_confirmation_ready(host_id, name, cx)
        })
    })?;
    if confirmation_open.map_err(io::Error::other)? {
        let cancelled = window.update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.right_rail_acceptance_cancel_container_confirmation(host_id, name, cx)
            })
        })?;
        cancelled.map_err(io::Error::other)?;
    }
    let state = poll_root(
        cx,
        root,
        window,
        "fixture 恢复前状态",
        |root, _, cx| root.right_rail_acceptance_container_state(host_id, name, cx),
    )
    .await?;
    let (container_id, status) = state;
    if container_id != expected_id {
        return Err(io::Error::other("fixture 恢复拒绝操作不同容器 ID").into());
    }
    if status == ContainerStatus::Running {
        return Ok(());
    }

    if status == ContainerStatus::Restarting {
        return wait_for_container_status(
            cx,
            root,
            window,
            (host_id, name, expected_id),
            ContainerStatus::Running,
            &[ContainerStatus::Restarting, ContainerStatus::Exited],
            "等待 fixture 重启完成",
        )
        .await;
    }
    if status != ContainerStatus::Exited {
        return Err(io::Error::other(format!(
            "fixture 恢复只接受同 ID 的 Exited 容器，当前为 {status:?}"
        ))
        .into());
    }

    poll_root(
        cx,
        root,
        window,
        "恢复 fixture 容器运行态",
        |root, _, cx| {
            root.right_rail_acceptance_start_container(host_id, name, cx)
                .map(|()| Some(()))
        },
    )
    .await?;
    wait_for_container_status(
        cx,
        root,
        window,
        (host_id, name, expected_id),
        ContainerStatus::Running,
        &[
            ContainerStatus::Exited,
            ContainerStatus::Created,
            ContainerStatus::Restarting,
        ],
        "fixture 运行态恢复回读",
    )
    .await
}

/// 拒绝 manifest 将 driver 指向 fixture 命名空间之外的用户容器。
///
/// @author kongweiguang
fn validate_fixture_container_name(name: &str) -> Result<(), io::Error> {
    let Some(suffix) = name.strip_prefix("kerminal-rr-") else {
        return Err(io::Error::other("fixture 容器名必须以 kerminal-rr- 开头"));
    };
    if suffix.len() != 12 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::other(
            "fixture 容器名必须以 12 位十六进制 ID 结尾",
        ));
    }
    Ok(())
}
