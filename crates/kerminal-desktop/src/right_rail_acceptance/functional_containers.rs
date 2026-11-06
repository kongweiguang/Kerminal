//! 对固定 HTTP fixture 验证生产容器状态、确认交互和日志入口。
//!
//! @author kongweiguang

use std::{error::Error, io};

use gpui_kit::{AnyWindowHandle, AsyncApp, Entity};
use gpui_pre::{InputEvent, KeyDownEvent, KeyUpEvent, Keystroke};
use kerminal_domain::{
    files::RemotePath,
    identity::{HostId, TabId},
    operations::{ComposeProjectTarget, ContainerAction, ContainerStatus},
};
use kerminal_ui::{ComposeAcceptanceOpenRequest, RootView};

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

    let container_result: Result<(), Box<dyn Error>> = match (flow_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(flow), Ok(())) => Err(flow),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(flow), Err(cleanup)) => Err(io::Error::other(format!(
            "容器功能验收失败：{flow}；fixture 运行态恢复失败：{cleanup}"
        ))
        .into()),
    };
    container_result?;

    run_compose_fixture(cx, root, window, capture, records, &fixture_terminal_tab_id).await
}

/// 只对本轮标签、路径和主机均匹配的 Compose fixture 执行菜单验收，并在失败后尝试恢复。
///
/// @author kongweiguang
async fn run_compose_fixture(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    terminal_tab_id: &TabId,
) -> Result<(), Box<dyn Error>> {
    let host = &capture.fixture_host_id;
    let run_id = capture
        .fixture_container_name
        .strip_prefix("kerminal-rr-")
        .ok_or_else(|| io::Error::other("容器 fixture 名称缺少运行 ID"))?;
    let project_name = format!("kerminal-rr-compose-{run_id}");
    let target = poll_root(cx, root, window, "Compose fixture 目标", |root, _, cx| {
        root.right_rail_acceptance_compose_target(host, &project_name, cx)
    })
    .await?;
    let fixture_root = format!("/run/kerminal-right-rail-{run_id}");
    if capture.remote_sftp_directory != format!("{fixture_root}/home/sftp")
        || target.host_id.as_ref() != Some(host)
        || target.project != project_name
        || target.working_dir != fixture_root
        || target.config_files.len() != 1
        || target.config_files[0] != format!("{fixture_root}/compose.yml")
    {
        return Err(io::Error::other("Compose fixture 的宿主、运行 ID 或配置路径不匹配").into());
    }
    let flow_result =
        run_compose_flow(cx, root, window, capture, records, terminal_tab_id, &target).await;
    let restore_result =
        match activate_compose_terminal(cx, root, window, host, terminal_tab_id).await {
            Ok(()) => restore_compose_fixture(cx, root, window, host, &target).await,
            Err(error) => Err(error),
        };
    match (flow_result, restore_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(flow), Ok(())) => Err(flow),
        (Ok(()), Err(restore)) => Err(restore),
        (Err(flow), Err(restore)) => Err(io::Error::other(format!(
            "Compose 验收失败：{flow}；fixture 恢复失败：{restore}"
        ))
        .into()),
    }
}

/// 每个动作都从真实更多菜单触发，再读取同一项目的生产列表与 inspect 回执。
///
/// @author kongweiguang
async fn run_compose_flow(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    terminal_tab_id: &TabId,
    target: &ComposeProjectTarget,
) -> Result<(), Box<dyn Error>> {
    let host = &capture.fixture_host_id;
    select_compose_fixture(cx, root, window, host, target).await?;
    let original_id = wait_compose_container(
        cx,
        root,
        window,
        (host, target),
        None,
        Some(ContainerStatus::Running),
        "Compose 初始状态",
    )
    .await?
    .ok_or_else(|| io::Error::other("Compose 初始容器缺失"))?;
    verify_compose_file_identity(cx, root, window, capture, records, terminal_tab_id, target)
        .await?;
    verify_compose_yaml_rejections(cx, root, window, host, terminal_tab_id, target).await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "compose-yaml-target-rejected",
        "wrong_host_and_unowned_yaml_path_rejected_without_opening_a_tab",
    )
    .await?;

    open_compose_menu(cx, root, window, host, target).await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "compose-menu-open",
        "production_compose_more_menu_visible_in_the_native_window",
    )
    .await?;
    send_compose_menu_key(cx, window, "escape")?;
    wait_compose_popup(cx, root, window, false).await?;

    choose_compose_menu_action(cx, root, window, host, target, 1).await?;
    wait_compose_container(
        cx,
        root,
        window,
        (host, target),
        Some(&original_id),
        Some(ContainerStatus::Exited),
        "Compose stop 回读",
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "compose-stopped",
        "same_owned_compose_container_exited_after_stop",
    )
    .await?;

    choose_compose_menu_action(cx, root, window, host, target, 0).await?;
    wait_compose_container(
        cx,
        root,
        window,
        (host, target),
        Some(&original_id),
        Some(ContainerStatus::Running),
        "Compose up 回读",
    )
    .await?;
    let started_at = read_started_at(cx, root, window, host, &original_id).await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "compose-up",
        "same_owned_compose_container_running_after_up",
    )
    .await?;

    choose_compose_menu_action(cx, root, window, host, target, 2).await?;
    wait_compose_container(
        cx,
        root,
        window,
        (host, target),
        Some(&original_id),
        Some(ContainerStatus::Running),
        "Compose restart 回读",
    )
    .await?;
    let deadline = std::time::Instant::now() + super::PANEL_FRAME_TIMEOUT;
    let restarted_at = loop {
        let restarted_at = read_started_at(cx, root, window, host, &original_id).await?;
        if restarted_at != started_at {
            break restarted_at;
        }
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::other("Compose restart 未改变真实 StartedAt").into());
        }
        cx.background_executor()
            .timer(super::PANEL_FRAME_POLL)
            .await;
    };
    checkpoint(
        cx,
        window,
        capture,
        records,
        "compose-restarted",
        "same_owned_compose_container_has_new_started_at",
    )
    .await?;

    choose_compose_menu_action(cx, root, window, host, target, 3).await?;
    wait_compose_confirmation(cx, root, window, host, target).await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "compose-down-confirmation",
        "production_down_confirmation_shows_exact_fixture_target",
    )
    .await?;
    poll_root(cx, root, window, "取消 Compose down", |root, _, cx| {
        root.right_rail_acceptance_cancel_compose_down(host, target, cx)
            .map(|()| Some(()))
    })
    .await?;
    wait_compose_container(
        cx,
        root,
        window,
        (host, target),
        Some(&original_id),
        Some(ContainerStatus::Running),
        "取消 Compose down 回读",
    )
    .await?;
    let after_cancel_started_at = read_started_at(cx, root, window, host, &original_id).await?;
    if after_cancel_started_at != restarted_at {
        return Err(io::Error::other("取消 Compose down 后容器启动时间发生变化").into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "compose-down-cancelled",
        "owned_compose_container_remains_running_after_cancel",
    )
    .await?;

    choose_compose_menu_action(cx, root, window, host, target, 3).await?;
    wait_compose_confirmation(cx, root, window, host, target).await?;
    poll_root(cx, root, window, "确认 Compose down", |root, _, cx| {
        root.right_rail_acceptance_confirm_compose_down(host, target, cx)
            .map(|()| Some(()))
    })
    .await?;
    wait_compose_container(
        cx,
        root,
        window,
        (host, target),
        None,
        None,
        "Compose down 回读",
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "compose-down",
        "owned_compose_container_absent_after_confirmed_down",
    )
    .await?;

    choose_compose_menu_action(cx, root, window, host, target, 0).await?;
    let recreated_id = wait_compose_container(
        cx,
        root,
        window,
        (host, target),
        None,
        Some(ContainerStatus::Running),
        "Compose down 后再次 up",
    )
    .await?
    .ok_or_else(|| io::Error::other("Compose 再次启动未发现容器"))?;
    if recreated_id == original_id {
        return Err(
            io::Error::other("Compose down 后容器 ID 未变化，无法证明容器被拆除并重建").into(),
        );
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "compose-up-after-down",
        "retained_exact_project_recreated_owned_running_container",
    )
    .await?;
    Ok(())
}

/// 两种入口依次打开同一远端文件，回读真实内容、唯一标签和只读收紧，避免仅验证事件受理。
///
/// @author kongweiguang
async fn verify_compose_file_identity(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    terminal_tab_id: &TabId,
    target: &ComposeProjectTarget,
) -> Result<(), Box<dyn Error>> {
    let host = &capture.fixture_host_id;
    let content_future = window
        .update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.right_rail_acceptance_read_compose_fixture(host, target, cx)
            })
        })?
        .map_err(io::Error::other)?;
    let expected = content_future.await.map_err(io::Error::other)?;

    // 前一轮 Compose 验收可能保留标签；先通过生产入口激活并关闭，保证两个顺序从空状态开始。
    open_fixture_as_file(cx, root, window, host, terminal_tab_id, target).await?;
    let (_, initial_readonly) = poll_root(
        cx,
        root,
        window,
        "现有 Compose 文件就绪",
        |root, _, cx| {
            if let Some(id) = root.right_rail_acceptance_compose_fixture_file_ready(
                host, target, &expected, true, cx,
            )? {
                return Ok(Some((id, true)));
            }
            Ok(root
                .right_rail_acceptance_compose_fixture_file_ready(
                    host, target, &expected, false, cx,
                )?
                .map(|id| (id, false)))
        },
    )
    .await?;
    close_fixture_file(cx, root, window, host, target, &expected, initial_readonly).await?;
    return_to_compose_terminal(cx, root, window, host, terminal_tab_id, target).await?;

    open_fixture_as_file(cx, root, window, host, terminal_tab_id, target).await?;
    let file_first = wait_fixture_file(cx, root, window, host, target, &expected, false).await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "compose-yaml-file-first",
        "ordinary_file_opened_one_editable_tab_with_exact_fixture_content",
    )
    .await?;
    request_fixture_compose_config(cx, root, window, host, terminal_tab_id, target).await?;
    let restricted = wait_fixture_file(cx, root, window, host, target, &expected, true).await?;
    if restricted != file_first {
        return Err(io::Error::other("Compose 入口未复用普通文件标签").into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "compose-yaml-file-first-reused",
        "compose_reused_the_same_tab_and_restricted_it_to_readonly",
    )
    .await?;
    close_fixture_file(cx, root, window, host, target, &expected, true).await?;
    return_to_compose_terminal(cx, root, window, host, terminal_tab_id, target).await?;

    request_fixture_compose_config(cx, root, window, host, terminal_tab_id, target).await?;
    let compose_first = wait_fixture_file(cx, root, window, host, target, &expected, true).await?;
    open_fixture_as_file(cx, root, window, host, terminal_tab_id, target).await?;
    let reopened = wait_fixture_file(cx, root, window, host, target, &expected, true).await?;
    if reopened != compose_first {
        return Err(io::Error::other("普通文件入口未复用 Compose 只读标签").into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "compose-yaml-compose-first-reused",
        "ordinary_file_reused_the_same_compose_tab_without_relaxing_readonly",
    )
    .await?;
    close_fixture_file(cx, root, window, host, target, &expected, true).await?;
    return_to_compose_terminal(cx, root, window, host, terminal_tab_id, target).await
}

/// 只发起一次经过上下文核对的普通文件打开，完成状态由独立回读判断。
///
/// @author kongweiguang
async fn open_fixture_as_file(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    terminal_tab_id: &TabId,
    target: &ComposeProjectTarget,
) -> Result<(), Box<dyn Error>> {
    poll_root(
        cx,
        root,
        window,
        "普通入口打开 Compose fixture",
        |root, window, cx| {
            root.open_right_rail_acceptance_compose_fixture_as_file(
                host,
                terminal_tab_id,
                target,
                window,
                cx,
            )
            .map(|requested| requested.then_some(()))
        },
    )
    .await
}

/// Compose 请求仍经当前生产 Operations 视图和 Root 目标门禁，不直接创建标签。
///
/// @author kongweiguang
async fn request_fixture_compose_config(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    terminal_tab_id: &TabId,
    target: &ComposeProjectTarget,
) -> Result<(), Box<dyn Error>> {
    poll_root(
        cx,
        root,
        window,
        "Compose 入口打开 fixture",
        |root, _, cx| {
            root.request_right_rail_acceptance_compose_fixture_config(
                host,
                terminal_tab_id,
                target,
                cx,
            )
            .map(|requested| requested.then_some(()))
        },
    )
    .await
}

/// 等待导航和编辑器同时回读唯一身份与正文，权限不符时不能提前通过。
///
/// @author kongweiguang
async fn wait_fixture_file(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    target: &ComposeProjectTarget,
    expected: &str,
    readonly: bool,
) -> Result<TabId, Box<dyn Error>> {
    poll_root(
        cx,
        root,
        window,
        "Compose fixture 文件正文和权限",
        |root, _, cx| {
            root.right_rail_acceptance_compose_fixture_file_ready(
                host, target, expected, readonly, cx,
            )
        },
    )
    .await
}

/// 先确认目标无脏稿，再执行生产关闭并等待导航和编辑缓冲都消失。
///
/// @author kongweiguang
async fn close_fixture_file(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    target: &ComposeProjectTarget,
    expected: &str,
    readonly: bool,
) -> Result<(), Box<dyn Error>> {
    poll_root(
        cx,
        root,
        window,
        "派发 Compose fixture 文件关闭",
        |root, window, cx| {
            root.close_right_rail_acceptance_compose_fixture_file(
                host, target, expected, readonly, window, cx,
            )
            .map(|closed| closed.then_some(()))
        },
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "Compose fixture 文件关闭回读",
        |root, _, cx| {
            root.right_rail_acceptance_compose_fixture_file_closed(host, target, cx)
                .map(|closed| closed.then_some(()))
        },
    )
    .await
}

/// 验收流程切回原 fixture 终端后重新选择项目，确保后续请求使用当前上下文。
///
/// @author kongweiguang
async fn return_to_compose_terminal(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    terminal_tab_id: &TabId,
    target: &ComposeProjectTarget,
) -> Result<(), Box<dyn Error>> {
    activate_compose_terminal(cx, root, window, host, terminal_tab_id).await?;
    select_compose_fixture(cx, root, window, host, target).await?;
    poll_root(
        cx,
        root,
        window,
        "恢复 Compose fixture 上下文",
        |root, _, cx| {
            root.right_rail_acceptance_compose_terminal_context_ready(
                host,
                terminal_tab_id,
                target,
                cx,
            )
            .map(|ready| ready.then_some(()))
        },
    )
    .await
}

/// 失败恢复先回到原终端，保留待处理确认供原清理流程取消，避免选择项目时绕过确认。
///
/// @author kongweiguang
async fn activate_compose_terminal(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    terminal_tab_id: &TabId,
) -> Result<(), Box<dyn Error>> {
    poll_root(
        cx,
        root,
        window,
        "恢复 Compose fixture 终端",
        |root, window, cx| {
            root.activate_right_rail_acceptance_compose_terminal_tab(
                host,
                terminal_tab_id,
                window,
                cx,
            )
            .map(|active| active.then_some(()))
        },
    )
    .await
}

/// 错误主机和错误配置路径都走既有 Root 校验，拒绝请求且不得创建任何文件标签。
///
/// @author kongweiguang
async fn verify_compose_yaml_rejections(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    terminal_tab_id: &TabId,
    target: &ComposeProjectTarget,
) -> Result<(), Box<dyn Error>> {
    let fixture_root = RemotePath::parse(&target.working_dir)
        .map_err(|_| io::Error::other("Compose fixture 根目录无效"))?;
    let path = RemotePath::parse(&target.config_files[0])
        .map_err(|_| io::Error::other("Compose fixture 文件路径无效"))?;
    let wrong_path = fixture_root
        .join_name("missing-compose.yml")
        .map_err(|_| io::Error::other("Compose 拒绝路径无效"))?;
    let tabs_before = poll_root(
        cx,
        root,
        window,
        "Compose 拒绝前标签数",
        |root, _, _| Ok(root.right_rail_acceptance_workspace_tab_count()),
    )
    .await?;
    for request in [
        ComposeAcceptanceOpenRequest::new(
            HostId::new(),
            terminal_tab_id.clone(),
            target.clone(),
            path,
            "compose.yml".to_owned(),
            fixture_root.clone(),
        ),
        ComposeAcceptanceOpenRequest::new(
            host.clone(),
            terminal_tab_id.clone(),
            target.clone(),
            wrong_path,
            "compose.yml".to_owned(),
            fixture_root,
        ),
    ] {
        let rejected = window.update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.open_right_rail_acceptance_compose_config(&request, cx)
            })
        })?;
        if rejected.is_ok() {
            return Err(io::Error::other("错误 Compose 宿主或配置路径未被拒绝").into());
        }
        let tabs_after = poll_root(
            cx,
            root,
            window,
            "Compose 拒绝后标签数",
            |root, _, _| Ok(root.right_rail_acceptance_workspace_tab_count()),
        )
        .await?;
        if tabs_after != tabs_before {
            return Err(io::Error::other("错误 Compose 请求意外创建了文件标签").into());
        }
    }
    Ok(())
}

/// 页面选择始终基于已验证的完整 fixture 目标，拆除后允许选择保留的项目。
///
/// @author kongweiguang
async fn select_compose_fixture(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    target: &ComposeProjectTarget,
) -> Result<(), Box<dyn Error>> {
    poll_root(cx, root, window, "选择 Compose fixture", |root, _, cx| {
        root.right_rail_acceptance_select_owned_compose(host, target, cx)
            .map(|ready| ready.then_some(()))
    })
    .await
}

/// 点击真实项目详情菜单并等待 Kit 弹层出现在当前帧。
///
/// @author kongweiguang
async fn open_compose_menu(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    target: &ComposeProjectTarget,
) -> Result<(), Box<dyn Error>> {
    select_compose_fixture(cx, root, window, host, target).await?;
    poll_root(cx, root, window, "Compose 更多按钮", |_, window, _| {
        Ok(window
            .debug_selector_bounds("rr-compose-project-more")
            .filter(|bounds| !bounds.is_empty())
            .map(|_| ()))
    })
    .await?;
    if window.update(cx, |_, window, _| {
        window.debug_selector_bounds("rr-popup-menu").is_some()
    })? {
        return Err(io::Error::other("打开 Compose 菜单前已有其他弹层").into());
    }
    super::menus::click_selector(cx, window, "rr-compose-project-more")?;
    wait_compose_popup(cx, root, window, true).await
}

/// 菜单必须在当前真实窗口内出现或关闭，不以请求成功代替呈现状态。
///
/// @author kongweiguang
async fn wait_compose_popup(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    expected_open: bool,
) -> Result<(), Box<dyn Error>> {
    poll_root(cx, root, window, "Compose 菜单状态", |_, window, _| {
        let popup = window.debug_selector_bounds("rr-popup-menu");
        if expected_open {
            let Some(bounds) = popup else {
                return Ok(None);
            };
            let viewport = window.visual_viewport_bounds();
            let rounding = gpui_pre::px(0.01);
            if bounds.is_empty()
                || bounds.origin.x < viewport.origin.x - rounding
                || bounds.origin.y < viewport.origin.y - rounding
                || bounds.origin.x + bounds.size.width
                    > viewport.origin.x + viewport.size.width + rounding
                || bounds.origin.y + bounds.size.height
                    > viewport.origin.y + viewport.size.height + rounding
            {
                return Err("Compose 菜单超出当前窗口可视范围".to_owned());
            }
            Ok(Some(()))
        } else {
            Ok(popup.is_none().then_some(()))
        }
    })
    .await
}

/// 向隔离 GPUI 窗口发送键盘事件，不注入系统级输入或触碰用户正在使用的应用。
///
/// @author kongweiguang
fn send_compose_menu_key(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    key: &str,
) -> Result<(), Box<dyn Error>> {
    let keystroke = Keystroke::parse(key)
        .map_err(|error| io::Error::other(format!("Compose 菜单按键无效：{error}")))?;
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
    let _ = super::frame_signature(cx, window)?;
    Ok(())
}

/// PopupMenu 的首个 Down 选择第一项，后续方向键与 Enter 经真实菜单执行生产回调。
///
/// @author kongweiguang
async fn choose_compose_menu_action(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    target: &ComposeProjectTarget,
    action_index: usize,
) -> Result<(), Box<dyn Error>> {
    if action_index > 3 {
        return Err(io::Error::other("Compose 菜单动作序号无效").into());
    }
    open_compose_menu(cx, root, window, host, target).await?;
    for _ in 0..=action_index {
        send_compose_menu_key(cx, window, "down")?;
    }
    send_compose_menu_key(cx, window, "enter")?;
    wait_compose_popup(cx, root, window, false).await
}

/// 列表只接受原完整项目的真实容器；Down 用空集合证明，重建允许产生新的 ID。
///
/// @author kongweiguang
async fn wait_compose_container(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    fixture: (&HostId, &ComposeProjectTarget),
    expected_id: Option<&str>,
    expected_status: Option<ContainerStatus>,
    label: &str,
) -> Result<Option<String>, Box<dyn Error>> {
    let (host, target) = fixture;
    poll_root(cx, root, window, label, |root, _, cx| {
        let Some(rows) = root.right_rail_acceptance_compose_owned_state(host, target, cx)? else {
            return Ok(None);
        };
        match (expected_status, rows.as_slice()) {
            (None, []) => Ok(Some(None)),
            (Some(status), [(id, current)]) => {
                if expected_id.is_some_and(|expected| expected != id.as_str()) {
                    return Err("Compose fixture 容器 ID 与原对象不一致".to_owned());
                }
                if *current == status {
                    Ok(Some(Some(id.clone())))
                } else if matches!(
                    current,
                    ContainerStatus::Running
                        | ContainerStatus::Exited
                        | ContainerStatus::Created
                        | ContainerStatus::Restarting
                ) {
                    Ok(None)
                } else {
                    Err(format!("Compose fixture 进入异常状态 {current:?}"))
                }
            }
            (None, [_]) | (Some(_), []) => Ok(None),
            _ => Err("Compose fixture 容器数量不符合隔离预期".to_owned()),
        }
    })
    .await
}

/// 只把绑定完整项目的生产 Down 模态视为确认准备完成。
///
/// @author kongweiguang
async fn wait_compose_confirmation(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    target: &ComposeProjectTarget,
) -> Result<(), Box<dyn Error>> {
    poll_root(cx, root, window, "Compose down 确认", |root, _, cx| {
        root.right_rail_acceptance_compose_down_confirmation_ready(host, target, cx)
            .map(|ready| ready.then_some(()))
    })
    .await
}

/// 失败后只恢复同一已验证项目；未知回执或无法确认所有权时停止并报告，不盲目重试。
///
/// @author kongweiguang
async fn restore_compose_fixture(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    target: &ComposeProjectTarget,
) -> Result<(), Box<dyn Error>> {
    if window.update(cx, |_, window, _| {
        window.debug_selector_bounds("rr-popup-menu").is_some()
    })? {
        send_compose_menu_key(cx, window, "escape")?;
        wait_compose_popup(cx, root, window, false).await?;
    }
    let confirmation = window
        .update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.right_rail_acceptance_compose_down_confirmation_ready(host, target, cx)
            })
        })?
        .map_err(io::Error::other)?;
    if confirmation {
        poll_root(
            cx,
            root,
            window,
            "取消剩余 Compose 确认",
            |root, _, cx| {
                root.right_rail_acceptance_cancel_compose_down(host, target, cx)
                    .map(|()| Some(()))
            },
        )
        .await?;
    }
    let rows = poll_root(
        cx,
        root,
        window,
        "Compose fixture 恢复前状态",
        |root, _, cx| root.right_rail_acceptance_compose_owned_state(host, target, cx),
    )
    .await?;
    match rows.as_slice() {
        [(_, ContainerStatus::Running)] => Ok(()),
        [] | [(_, ContainerStatus::Exited | ContainerStatus::Created)] => {
            choose_compose_menu_action(cx, root, window, host, target, 0).await?;
            wait_compose_container(
                cx,
                root,
                window,
                (host, target),
                None,
                Some(ContainerStatus::Running),
                "恢复 Compose fixture",
            )
            .await?;
            Ok(())
        }
        _ => Err(io::Error::other("Compose fixture 恢复状态不明确，未重试写入").into()),
    }
}

/// 通过实际详情、确认、状态刷新和 StartedAt 回读，区分取消、停止、启动与重启结果。
/// 日志必须来自精确容器并含 nginx 启动标记或本轮 HTTP fixture 的唯一标记，不能用任意非空输出冒充成功。
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
    let startup_marker = format!(
        "kerminal-fixture-http-starting:{}",
        name.strip_prefix("kerminal-rr-")
            .ok_or_else(|| io::Error::other("HTTP fixture 缺少本轮运行标识"))?
    );
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
            "fixture HTTP 启动日志回读",
            |root, _, _| {
                root.right_rail_acceptance_container_logs_text(host_id, container_id)
                    .map(|text| {
                        text.filter(|text| {
                            text.contains("Configuration complete")
                                || text.contains("start worker process")
                                || text.contains(startup_marker.as_str())
                        })
                    })
            },
        )
        .await?;
        if !logs.contains("Configuration complete")
            && !logs.contains("start worker process")
            && !logs.contains(startup_marker.as_str())
        {
            return Err(io::Error::other("fixture 容器日志缺少已知启动标记").into());
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
            "exact_fixture_log_terminal_contains_known_startup_marker",
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
