//! 隔离原生窗口中的双端主机标签、右键传输与拖拽验收。
//!
//! @author kongweiguang

use std::{error::Error, fs, io, time::Instant};

use gpui_kit::{
    AnyWindowHandle, AsyncApp, Bounds, ElementId, Entity, InputEvent, KeyDownEvent, KeyUpEvent,
    Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    ScrollDelta, ScrollWheelEvent, TouchPhase, point, px,
};
use kerminal_application::{
    AppError, ApplicationTaskExecutor,
    connections::management::HostDraft,
    files::FileService,
    preferences::{ThemeMode, settings::ToolRailToolId},
    workspace::{WorkspaceSessionLoad, WorkspaceSessionService, WorkspaceSessionTab},
};
use kerminal_domain::{
    files::{
        DeleteTargetKind, FileEndpoint, TransferKind, TransferStatus, WriteTextRequest,
        WriteTextStrategy,
    },
    identity::{HostId, TabId},
};
use kerminal_ui::RootView;

use super::{
    RightRailCaptureContext,
    functional::{self, FunctionalRecord},
    menus,
};

const SOURCE_FIXTURE_HOST: &str = "transfer-source-fixture";
const DRAG_PREVIEW_SELECTOR: &str = "files-drag-preview";

/// 仅修改新复制的验收配置，通过现有 CAS 服务生成长名称而不触碰用户配置或 fixture 源。
///
/// @author kongweiguang
pub(super) async fn prepare_long_fixture_host_names(
    services: &kerminal_ui::AppServices,
) -> Result<(), Box<dyn Error>> {
    let hosts = services
        .hosts
        .as_ref()
        .ok_or_else(|| io::Error::other("缺少主机管理服务"))?;
    for (id, name) in [
        (
            SOURCE_FIXTURE_HOST,
            "Transfer source host with a deliberately long descriptive name for narrow panels",
        ),
        (
            super::FIXTURE_HOST_ID,
            "Transfer target host with a deliberately long descriptive name for narrow panels",
        ),
    ] {
        let host_id = HostId::parse(id)?;
        let host = hosts
            .load_host(&host_id)
            .await?
            .ok_or_else(|| io::Error::other("长名称验收缺少 fixture 主机"))?;
        let revision = host.revision.clone();
        let draft = HostDraft {
            id: Some(host.id),
            group_id: host.group_id,
            name: name.to_owned(),
            note: host.note,
            protocol: host.protocol,
            endpoint: host.endpoint,
            authentication: host.authentication,
            authentication_mode: host.authentication_mode,
            rdp_password: host.rdp_password,
            route: host.route,
            serial: host.serial,
            tags: host.tags,
            is_favorite: host.is_favorite,
            sort_order: host.sort_order,
            connect_timeout_ms: host.connect_timeout_ms,
            keepalive_interval_ms: host.keepalive_interval_ms,
            terminal_options: host.terminal_options,
            sftp_options: host.sftp_options,
            rdp_display: host.rdp_display,
        };
        let saved = hosts.save_host(draft, Some(revision)).await?;
        if saved.id != host_id || saved.name != name {
            return Err(io::Error::other("长名称 fixture 保存回读不一致").into());
        }
    }
    Ok(())
}

/// 每个端点都绑定本轮随机目录项，失败时由隔离 fixture 负责后续清理。
struct WorkspaceFixture {
    source_host: HostId,
    target_host: HostId,
    source_root: FileEndpoint,
    browse_directory: FileEndpoint,
    target_directory: FileEndpoint,
    menu_source: FileEndpoint,
    menu_target: FileEndpoint,
    drag_source: FileEndpoint,
    drag_target: FileEndpoint,
    folder_source: FileEndpoint,
    folder_target: FileEndpoint,
    folder_source_file: FileEndpoint,
    folder_target_file: FileEndpoint,
    reverse_source: FileEndpoint,
    reverse_target: FileEndpoint,
    reverse_folder_source: FileEndpoint,
    reverse_folder_target: FileEndpoint,
    reverse_folder_source_file: FileEndpoint,
    reverse_folder_target_file: FileEndpoint,
    browse_name: String,
    target_name: String,
    menu_name: String,
    drag_name: String,
    folder_name: String,
    reverse_name: String,
    reverse_folder_name: String,
    payload: String,
}

/// 通过真实窗口事件完成主机切换、右键复制和两类拖拽，并以端点与正文回读确认。
///
/// @author kongweiguang
pub(super) async fn run(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    services: &kerminal_ui::AppServices,
) -> Result<(), Box<dyn Error>> {
    let files = services
        .files
        .as_ref()
        .ok_or_else(|| io::Error::other("缺少文件服务"))?;
    let executor = &services.executor;
    if !capture
        .fixture_config_source
        .join("hosts")
        .join(format!("{SOURCE_FIXTURE_HOST}.toml"))
        .is_file()
    {
        return Ok(());
    }
    let source_host = HostId::parse(SOURCE_FIXTURE_HOST)?;
    let run_id = capture
        .output_directory
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        })
        .ok_or_else(|| io::Error::other("验收输出目录缺少可安全用于文件名的运行标识"))?;
    let fixture = create_fixture(
        executor,
        files,
        source_host,
        capture.fixture_host_id.clone(),
        &capture.remote_sftp_directory,
        run_id,
    )
    .await?;

    let fixture_tab = functional::poll_root(cx, root, window, "fixture 终端", |root, _, _| {
        root.right_rail_acceptance_fixture_tab_id(&capture.fixture_host_id)
            .map(Some)
    })
    .await?;
    let first_transfer_tab =
        create_transfer_tab(cx, root, window, &capture.fixture_host_id).await?;
    restore_terminal(cx, root, window, &fixture_tab).await?;
    let transfer_tab = create_transfer_tab(cx, root, window, &capture.fixture_host_id).await?;
    restore_terminal(cx, root, window, &fixture_tab).await?;

    let _ = super::frame_signature(cx, window)?;
    if capture.requested_window_width == Some(800) {
        let active = window.update(cx, |_, _, app| {
            root.read(app).right_rail_acceptance_active_tool()
        })?;
        if active != Some(ToolRailToolId::Sftp) {
            click_visible_element(cx, window, "workbench-tool-sftp")?;
        }
        functional::poll_root(
            cx,
            root,
            window,
            "打开紧凑文件抽屉",
            |root, window, _| {
                Ok(
                    (root.right_rail_acceptance_active_tool() == Some(ToolRailToolId::Sftp)
                        && window
                            .debug_selector_bounds("workbench-compact-tool-drawer-surface")
                            .is_some())
                    .then_some(()),
                )
            },
        )
        .await?;
        super::save_frame(
            cx,
            window,
            capture
                .output_directory
                .join("sftp-compact-drawer-before-workbench.png"),
        )?;
    }
    let navigation_visible = window.update(cx, |_, window, _| {
        window
            .debug_selector_bounds("host-navigation-sftp-workbench")
            .is_some()
    })?;
    if !navigation_visible {
        click_visible_element(cx, window, "workbench-toggle-host-sidebar")?;
    }
    wait_selector(cx, window, "host-navigation-sftp-workbench").await?;
    menus::click_selector(cx, window, "host-navigation-sftp-workbench")?;
    if capture.requested_window_width == Some(800) {
        let result = functional::poll_root(
            cx,
            root,
            window,
            "顶部传输台不保留文件抽屉遮罩",
            |root, window, _| {
                Ok((root.right_rail_acceptance_active_tool().is_none()
                    && window
                        .debug_selector_bounds("workbench-compact-tool-drawer-surface")
                        .is_none()
                    && window
                        .debug_selector_bounds("central-files-browser-list-0")
                        .is_some())
                .then_some(()))
            },
        )
        .await;
        let frame = super::save_frame(
            cx,
            window,
            capture
                .output_directory
                .join("sftp-compact-workbench-after-navigation.png"),
        )?;
        let state = window.update(cx, |_, window, app| {
            serde_json::json!({
                "activeTool": format!("{:?}", root.read(app).right_rail_acceptance_active_tool()),
                "drawerBounds": format!("{:?}", window.debug_selector_bounds("workbench-compact-tool-drawer-surface")),
                "leftListBounds": format!("{:?}", window.debug_selector_bounds("central-files-browser-list-0")),
                "frame": frame,
                "result": if result.is_ok() { "drawer_closed" } else { "timeout" },
            })
        })?;
        fs::write(
            capture
                .output_directory
                .join("sftp-compact-workbench-after-navigation.json"),
            serde_json::to_vec_pretty(&state)?,
        )?;
        result?;
    }
    wait_local(cx, root, window, 0).await?;
    functional::poll_root(
        cx,
        root,
        window,
        "共享工作台右侧未选择",
        |root, _, cx| {
            Ok(root
                .transfer_workspace_acceptance_endpoint(1, cx)
                .filter(|(endpoint, _, _)| endpoint.is_none())
                .map(|_| ()))
        },
    )
    .await?;

    select_host(cx, root, window, capture, 0, fixture.source_host.as_str()).await?;
    wait_endpoint(cx, root, window, 0, &fixture.source_root).await?;
    let first_left_tab = wait_active_host_tab(cx, root, window, 0, &fixture.source_host).await?;
    select_host(cx, root, window, capture, 1, fixture.target_host.as_str()).await?;
    let target_root = parent_endpoint(&fixture.target_directory)?;
    wait_endpoint(cx, root, window, 1, &target_root).await?;
    let right_target_tab = wait_active_host_tab(cx, root, window, 1, &fixture.target_host).await?;
    assert_browser_geometry(cx, window)?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-workspace-dual-host",
        "two_panels_selected_distinct_fixture_ssh_endpoints",
    )
    .await?;

    select_host(cx, root, window, capture, 0, fixture.source_host.as_str()).await?;
    wait_endpoint(cx, root, window, 0, &fixture.source_root).await?;
    let second_left_tab = wait_active_host_tab(cx, root, window, 0, &fixture.source_host).await?;
    if first_left_tab == second_left_tab {
        return Err(io::Error::other("重复打开主机未建立独立标签").into());
    }
    double_click_selector(
        cx,
        window,
        capture,
        0,
        &format!("central-files-row-0-{}", fixture.browse_name),
        "browse",
    )
    .await?;
    wait_endpoint(cx, root, window, 0, &fixture.browse_directory).await?;
    click_host_tab(cx, window, 0, &first_left_tab, false).await?;
    wait_endpoint(cx, root, window, 0, &fixture.source_root).await?;
    select_host(cx, root, window, capture, 1, "local-device").await?;
    wait_local(cx, root, window, 1).await?;
    click_host_tab(cx, window, 1, &right_target_tab, false).await?;
    wait_endpoint(cx, root, window, 1, &target_root).await?;
    click_host_tab(cx, window, 0, &second_left_tab, false).await?;
    wait_endpoint(cx, root, window, 0, &fixture.browse_directory).await?;
    if capture.requested_window_width == Some(800) {
        resize_left_panel_to_narrow(cx, window).await?;
        capture_narrow_empty_themes(cx, root, window, capture).await?;
    }
    let chip_evidence =
        capture_active_chip_geometry(cx, window, capture, [&second_left_tab, &right_target_tab])
            .await?;
    click_host_tab(cx, window, 0, &second_left_tab, true).await?;
    wait_local(cx, root, window, 0).await?;
    fs::write(
        capture
            .output_directory
            .join("sftp-workspace-host-chip-bounds.json"),
        serde_json::to_vec_pretty(&chip_evidence)?,
    )?;
    click_host_tab(cx, window, 0, &first_left_tab, false).await?;
    wait_endpoint(cx, root, window, 0, &fixture.source_root).await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-workspace-host-tabs",
        "same_host_tabs_preserved_independent_directories_and_close_successor",
    )
    .await?;

    double_click_selector(
        cx,
        window,
        capture,
        1,
        &format!("central-files-row-1-{}", fixture.target_name),
        "target",
    )
    .await?;
    wait_endpoint(cx, root, window, 1, &fixture.target_directory).await?;
    let source_selector = format!("central-files-row-0-{}", fixture.menu_name);
    click_fixture_row(cx, root, window, capture, 0, &fixture.menu_name, "menu").await?;
    for (mode, name) in [
        (ThemeMode::Light, "sftp-workspace-theme-light"),
        (ThemeMode::Dark, "sftp-workspace-theme-dark"),
        (ThemeMode::System, "sftp-workspace-theme-system"),
    ] {
        window.update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.set_right_rail_acceptance_theme(mode, cx)
            })
        })?;
        open_context_menu(cx, window, capture, 0, &source_selector, name).await?;
        check_file_menu(cx, window)?;
        functional::checkpoint(
            cx,
            window,
            capture,
            records,
            name,
            "central_file_context_menu_open_and_commands_verified",
        )
        .await?;
        if mode != ThemeMode::System {
            dismiss_menu(cx, window).await?;
        }
    }
    click_copy_menu_item(cx, window, 1)?;
    wait_transfer(
        cx,
        root,
        window,
        &fixture.menu_source,
        &fixture.menu_target,
        TransferKind::File,
    )
    .await?;
    assert_text(executor, files, &fixture.menu_source, &fixture.payload).await?;
    assert_text(executor, files, &fixture.menu_target, &fixture.payload).await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-workspace-copy",
        "context_menu_copy_committed_to_exact_destination_and_both_contents_match",
    )
    .await?;

    drag_to_other_side(cx, root, window, capture, &fixture.drag_name).await?;
    wait_transfer(
        cx,
        root,
        window,
        &fixture.drag_source,
        &fixture.drag_target,
        TransferKind::File,
    )
    .await?;
    assert_text(executor, files, &fixture.drag_source, &fixture.payload).await?;
    assert_text(executor, files, &fixture.drag_target, &fixture.payload).await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-workspace-drag-file",
        "file_drag_preview_tracked_pointer_and_exact_target_content_matches",
    )
    .await?;

    drag_to_other_side(cx, root, window, capture, &fixture.folder_name).await?;
    wait_transfer(
        cx,
        root,
        window,
        &fixture.folder_source,
        &fixture.folder_target,
        TransferKind::Directory,
    )
    .await?;
    assert_text(
        executor,
        files,
        &fixture.folder_source_file,
        &fixture.payload,
    )
    .await?;
    assert_text(
        executor,
        files,
        &fixture.folder_target_file,
        &fixture.payload,
    )
    .await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-workspace-drag-directory",
        "directory_drag_preview_tracked_pointer_and_nested_contents_match",
    )
    .await?;

    let mut reverse_evidence = Vec::new();
    for (name, source, destination, source_file, destination_file, kind, evidence_name) in [
        (
            &fixture.reverse_name,
            &fixture.reverse_source,
            &fixture.reverse_target,
            &fixture.reverse_source,
            &fixture.reverse_target,
            TransferKind::File,
            "sftp-workspace-reverse-file.png",
        ),
        (
            &fixture.reverse_folder_name,
            &fixture.reverse_folder_source,
            &fixture.reverse_folder_target,
            &fixture.reverse_folder_source_file,
            &fixture.reverse_folder_target_file,
            TransferKind::Directory,
            "sftp-workspace-reverse-directory.png",
        ),
    ] {
        let selector = format!("central-files-row-1-{name}");
        let stage = if kind == TransferKind::File {
            "reverse-file"
        } else {
            "reverse-directory"
        };
        click_fixture_row(cx, root, window, capture, 1, name, stage).await?;
        open_context_menu(cx, window, capture, 1, &selector, &format!("{stage}-menu")).await?;
        click_copy_menu_item(cx, window, 0)?;
        wait_transfer(cx, root, window, source, destination, kind).await?;
        assert_text(executor, files, source_file, &fixture.payload).await?;
        assert_text(executor, files, destination_file, &fixture.payload).await?;
        reverse_evidence.push(serde_json::json!({
            "operation": if kind == TransferKind::File { "remote-b-to-a-file" } else { "remote-b-to-a-directory" },
            "sourceAndDestinationContentMatch": true,
            "screenshot": super::save_frame(cx, window, capture.output_directory.join(evidence_name))?,
        }));
    }
    fs::write(
        capture.output_directory.join("sftp-workspace-reverse.json"),
        serde_json::to_vec_pretty(&reverse_evidence)?,
    )?;

    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.set_right_rail_acceptance_theme(ThemeMode::Light, cx)
        })
    })?;
    super::functional_transfers::click_visible_workspace_tab(
        cx,
        window,
        capture,
        &transfer_tab,
        false,
        "transfer-activate",
    )
    .await?;
    functional::poll_root(
        cx,
        root,
        window,
        "激活独立传输标签",
        |root, _, _| {
            Ok(root
                .right_rail_acceptance_sftp_workspace_tab_selected(
                    &capture.fixture_host_id,
                    &transfer_tab,
                )
                .then_some(()))
        },
    )
    .await?;
    select_host(cx, root, window, capture, 0, fixture.source_host.as_str()).await?;
    wait_endpoint(cx, root, window, 0, &fixture.source_root).await?;
    functional::poll_root(
        cx,
        root,
        window,
        "独立标签目标描述已更新",
        |root, _, _| {
            Ok(root
                .transfer_workspace_acceptance_tab()
                .and_then(|tab| match tab {
                    WorkspaceSessionTab::SftpTransfer {
                        id,
                        left_host_id: Some(left),
                        right_host_id: Some(right),
                        locked_left_host_id: None,
                        ..
                    } if id == transfer_tab
                        && left == fixture.source_host
                        && right == fixture.target_host =>
                    {
                        Some(())
                    }
                    _ => None,
                }))
        },
    )
    .await?;
    let workspace_sessions = services
        .workspace_sessions
        .as_ref()
        .ok_or_else(|| io::Error::other("缺少工作区会话仓储"))?;
    wait_persisted_transfer_tabs(
        cx,
        window,
        executor,
        workspace_sessions,
        &first_transfer_tab,
        &transfer_tab,
        &fixture.source_host,
        &fixture.target_host,
        &capture.remote_sftp_directory,
    )
    .await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-workspace-tab-persist",
        "two_independent_transfer_tabs_saved_with_distinct_endpoint_pairs",
    )
    .await?;
    super::functional_transfers::click_visible_workspace_tab(
        cx,
        window,
        capture,
        &transfer_tab,
        true,
        "transfer-close",
    )
    .await?;
    wait_transfer_tab_closed(cx, root, window, &capture.fixture_host_id, &transfer_tab).await?;
    super::functional_transfers::click_visible_workspace_tab(
        cx,
        window,
        capture,
        &first_transfer_tab,
        true,
        "first-transfer-close",
    )
    .await?;
    wait_transfer_tab_closed(
        cx,
        root,
        window,
        &capture.fixture_host_id,
        &first_transfer_tab,
    )
    .await?;
    restore_terminal(cx, root, window, &fixture_tab).await?;
    cleanup_fixture(executor, files, &fixture).await?;
    Ok(())
}

/// 以同一真实窄空目录截取三种外观，保证主题切换没有借用另一份布局或夹具状态。
///
/// @author kongweiguang
async fn capture_narrow_empty_themes(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
) -> Result<(), Box<dyn Error>> {
    let mut evidence = Vec::new();
    for (mode, name) in [
        (ThemeMode::Light, "light"),
        (ThemeMode::Dark, "dark"),
        (ThemeMode::System, "system"),
    ] {
        window.update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.set_right_rail_acceptance_theme(mode, cx)
            })
        })?;
        let frame = super::wait_for_stable_frame(
            cx,
            window,
            capture
                .output_directory
                .join(format!("sftp-empty-narrow-{name}.png")),
            None,
            name,
        )
        .await?;
        evidence.push(serde_json::json!({ "theme": name, "frame": frame }));
    }
    fs::write(
        capture
            .output_directory
            .join("sftp-empty-narrow-themes.json"),
        serde_json::to_vec_pretty(&evidence)?,
    )?;
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.set_right_rail_acceptance_theme(ThemeMode::Light, cx)
        })
    })?;
    Ok(())
}

/// 构造单个合法名称的子目标，不把远端 POSIX 路径解释为 Windows 路径。
///
/// @author kongweiguang
fn child(parent: &FileEndpoint, name: &str) -> Result<FileEndpoint, Box<dyn Error>> {
    let FileEndpoint::Remote { host_id, path } = parent else {
        return Err(io::Error::other("验收目标必须是远端目录").into());
    };
    Ok(FileEndpoint::Remote {
        host_id: host_id.clone(),
        path: path
            .join_name(name)
            .map_err(|error| io::Error::other(format!("{error:?}")))?,
    })
}

/// 从已知远端子目标得到父目录，避免用字符串猜测目录边界。
///
/// @author kongweiguang
fn parent_endpoint(endpoint: &FileEndpoint) -> Result<FileEndpoint, Box<dyn Error>> {
    let FileEndpoint::Remote { host_id, path } = endpoint else {
        return Err(io::Error::other("验收目标不是远端路径").into());
    };
    Ok(FileEndpoint::Remote {
        host_id: host_id.clone(),
        path: path
            .parent()
            .ok_or_else(|| io::Error::other("验收目标缺少父目录"))?,
    })
}

/// 在专用根目录下创建互不冲突的本轮数据；每个写入明确要求新建。
///
/// @author kongweiguang
async fn create_fixture(
    executor: &ApplicationTaskExecutor,
    files: &FileService,
    source_host: HostId,
    target_host: HostId,
    directory: &str,
    run_id: &str,
) -> Result<WorkspaceFixture, Box<dyn Error>> {
    let source_root = FileEndpoint::remote(source_host.clone(), directory)
        .map_err(|error| io::Error::other(format!("{error:?}")))?;
    let target_root = FileEndpoint::remote(target_host.clone(), directory)
        .map_err(|error| io::Error::other(format!("{error:?}")))?;
    let prefix = format!("rr-ui-{run_id}-ws");
    let browse_name = format!("{prefix}-browse");
    let target_name = format!("{prefix}-target");
    let menu_name = format!("{prefix}-menu.txt");
    let drag_name = format!("{prefix}-drag.txt");
    let folder_name = format!("{prefix}-folder");
    let reverse_name = format!("{prefix}-reverse.txt");
    let reverse_folder_name = format!("{prefix}-reverse-folder");
    let browse_directory = child(&source_root, &browse_name)?;
    let target_directory = child(&target_root, &target_name)?;
    let menu_source = child(&source_root, &menu_name)?;
    let menu_target = child(&target_directory, &menu_name)?;
    let drag_source = child(&source_root, &drag_name)?;
    let drag_target = child(&target_directory, &drag_name)?;
    let folder_source = child(&source_root, &folder_name)?;
    let folder_target = child(&target_directory, &folder_name)?;
    let folder_source_file = child(&folder_source, "nested.txt")?;
    let folder_target_file = child(&folder_target, "nested.txt")?;
    let reverse_source = child(&target_directory, &reverse_name)?;
    let reverse_target = child(&source_root, &reverse_name)?;
    let reverse_folder_source = child(&target_directory, &reverse_folder_name)?;
    let reverse_folder_target = child(&source_root, &reverse_folder_name)?;
    let reverse_folder_source_file = child(&reverse_folder_source, "nested.txt")?;
    let reverse_folder_target_file = child(&reverse_folder_target, "nested.txt")?;
    let payload = format!("kerminal-workspace-{run_id}\n");
    let service = files.clone();
    let source = source_root.clone();
    let target = target_root.clone();
    let dirs = [
        browse_directory.clone(),
        target_directory.clone(),
        folder_source.clone(),
        reverse_folder_source.clone(),
    ];
    let paths = [
        menu_source.clone(),
        drag_source.clone(),
        folder_source_file.clone(),
        reverse_source.clone(),
        reverse_folder_source_file.clone(),
    ];
    let names = [
        browse_name.clone(),
        target_name.clone(),
        menu_name.clone(),
        drag_name.clone(),
        folder_name.clone(),
        reverse_name.clone(),
        reverse_folder_name.clone(),
    ];
    let content = payload.clone();
    executor
        .run(move |cancellation| async move {
            for root in [source, target] {
                let listing = service.list_directory(cancellation.clone(), root).await?;
                if listing
                    .entries
                    .iter()
                    .any(|entry| names.contains(&entry.name))
                {
                    return Err(AppError::conflict("本轮验收目标已存在"));
                }
            }
            for directory in dirs {
                service
                    .create_directory(cancellation.clone(), directory)
                    .await?;
            }
            for endpoint in paths {
                service
                    .write_text_file(
                        cancellation.clone(),
                        WriteTextRequest {
                            endpoint,
                            content: content.clone(),
                            strategy: WriteTextStrategy::CreateNew,
                        },
                    )
                    .await?;
            }
            Ok(())
        })
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(WorkspaceFixture {
        source_host,
        target_host,
        source_root,
        browse_directory,
        target_directory,
        menu_source,
        menu_target,
        drag_source,
        drag_target,
        folder_source,
        folder_target,
        folder_source_file,
        folder_target_file,
        reverse_source,
        reverse_target,
        reverse_folder_source,
        reverse_folder_target,
        reverse_folder_source_file,
        reverse_folder_target_file,
        browse_name,
        target_name,
        menu_name,
        drag_name,
        folder_name,
        reverse_name,
        reverse_folder_name,
        payload,
    })
}

/// 只在全部任务成功后删除本轮精确创建的文件与空目录，不清理未知提交材料。
///
/// @author kongweiguang
async fn cleanup_fixture(
    executor: &ApplicationTaskExecutor,
    files: &FileService,
    fixture: &WorkspaceFixture,
) -> Result<(), Box<dyn Error>> {
    let service = files.clone();
    let files_to_remove = [
        fixture.menu_source.clone(),
        fixture.menu_target.clone(),
        fixture.drag_source.clone(),
        fixture.drag_target.clone(),
        fixture.folder_source_file.clone(),
        fixture.folder_target_file.clone(),
        fixture.reverse_source.clone(),
        fixture.reverse_target.clone(),
        fixture.reverse_folder_source_file.clone(),
        fixture.reverse_folder_target_file.clone(),
    ];
    let directories = [
        fixture.folder_source.clone(),
        fixture.folder_target.clone(),
        fixture.reverse_folder_source.clone(),
        fixture.reverse_folder_target.clone(),
        fixture.browse_directory.clone(),
        fixture.target_directory.clone(),
    ];
    executor
        .run(move |cancellation| async move {
            for endpoint in files_to_remove {
                service
                    .delete(cancellation.clone(), endpoint, DeleteTargetKind::File)
                    .await?;
            }
            for endpoint in directories {
                service
                    .delete(cancellation.clone(), endpoint, DeleteTargetKind::Directory)
                    .await?;
            }
            Ok(())
        })
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(())
}

/// 复用生产预检创建独立标签，并以稳定 TabId 排除已有标签与重复回执。
///
/// @author kongweiguang
async fn create_transfer_tab(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
) -> Result<TabId, Box<dyn Error>> {
    let previous = window.update(cx, |_, _, app| {
        root.read(app)
            .right_rail_acceptance_sftp_workspace_tab_ids(host)
    })?;
    functional::poll_root(
        cx,
        root,
        window,
        "新建独立传输标签",
        |root, _, cx| {
            root.request_right_rail_acceptance_sftp_workspace_tab(host, cx)
                .map(|requested| requested.then_some(()))
        },
    )
    .await?;
    functional::poll_root(
        cx,
        root,
        window,
        "独立传输标签已恢复",
        |root, _, _| {
            let created = root
                .right_rail_acceptance_sftp_workspace_tab_ids(host)
                .into_iter()
                .filter(|id| !previous.contains(id))
                .collect::<Vec<_>>();
            if created.len() > 1 {
                return Err("创建了多个独立传输标签".to_owned());
            }
            Ok(created
                .into_iter()
                .next()
                .filter(|id| root.right_rail_acceptance_sftp_workspace_tab_selected(host, id)))
        },
    )
    .await
}

/// 等待关闭回执和标签从快照移除，再发出下一次关闭以避免资源事务相互覆盖。
///
/// @author kongweiguang
async fn wait_transfer_tab_closed(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
    tab: &TabId,
) -> Result<(), Box<dyn Error>> {
    functional::poll_root(
        cx,
        root,
        window,
        "独立传输标签已关闭",
        |root, _, _| {
            Ok((!root
                .right_rail_acceptance_sftp_workspace_tab_ids(host)
                .contains(tab))
            .then_some(()))
        },
    )
    .await
}

/// 从真实会话仓储反复回读两个标签，确认左右端点和目录已保存且互不串写。
///
/// @author kongweiguang
#[allow(clippy::too_many_arguments)]
async fn wait_persisted_transfer_tabs(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    executor: &ApplicationTaskExecutor,
    sessions: &WorkspaceSessionService,
    local_tab: &TabId,
    remote_tab: &TabId,
    source: &HostId,
    target: &HostId,
    expected_directory: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let _ = super::frame_signature(cx, window)?;
        let service = sessions.clone();
        let loaded = executor
            .run(move |cancellation| async move { service.load(cancellation).await })
            .await
            .map_err(|error| io::Error::other(error.to_string()))?;
        match loaded {
            WorkspaceSessionLoad::Loaded { document, .. } => {
                let snapshot = document
                    .workspace_snapshot()
                    .map_err(|error| io::Error::other(format!("{error:?}")))?;
                let local_matches = snapshot.terminal_tabs.iter().any(|tab| matches!(tab,
                    WorkspaceSessionTab::SftpTransfer { id, left_host_id: None, right_host_id: Some(right), locked_left_host_id: None, .. }
                        if id == local_tab && right == target));
                let remote_matches = snapshot.terminal_tabs.iter().any(|tab| matches!(tab,
                    WorkspaceSessionTab::SftpTransfer { id, left_host_id: Some(left), right_host_id: Some(right), locked_left_host_id: None, initial_left_path: Some(left_path), initial_right_path: Some(right_path), .. }
                        if id == remote_tab && left == source && right == target && left_path == expected_directory && right_path == expected_directory));
                if local_matches && remote_matches {
                    return Ok(());
                }
            }
            WorkspaceSessionLoad::Missing => {}
            WorkspaceSessionLoad::Invalid { .. } | WorkspaceSessionLoad::Unsupported { .. } => {
                return Err(io::Error::other("隔离工作区仓储不能读取当前格式").into());
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("两个传输标签未按不同端点保存").into());
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// 两侧标题与搜索框必须完整落入中央区域；检测真实绘制边界而不是依赖 flex 声明。
///
/// @author kongweiguang
fn assert_browser_geometry(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    let _ = super::frame_signature(cx, window)?;
    window.update(cx, |_, window, _| {
        let area = window.debug_selector_bounds("files-browser-area-wide")
            .ok_or_else(|| io::Error::other("中央文件区域未绘制"))?
            .intersect(&window.visual_viewport_bounds());
        let within = |inner: gpui_kit::Bounds<Pixels>, outer: gpui_kit::Bounds<Pixels>| {
            let tolerance = px(0.5);
            !inner.is_empty()
                && inner.origin.x >= outer.origin.x - tolerance
                && inner.origin.y >= outer.origin.y - tolerance
                && inner.origin.x + inner.size.width <= outer.origin.x + outer.size.width + tolerance
                && inner.origin.y + inner.size.height <= outer.origin.y + outer.size.height + tolerance
        };
        let mut strips = Vec::new();
        for side in 0..2 {
            let strip = window.debug_selector_bounds(&format!("files-host-strip-{side}"))
                .ok_or_else(|| io::Error::other(format!("第 {side} 侧主机栏未绘制")))?;
            let search = window.debug_selector_bounds(&format!("files-sftp-host-picker-toggle-{side}"))
                .ok_or_else(|| io::Error::other(format!("第 {side} 侧主机搜索未绘制")))?;
            if !within(strip, area) || !within(search, strip) {
                return Err(io::Error::other(format!("第 {side} 侧主机栏或搜索被裁切: area={area:?}, strip={strip:?}, search={search:?}")));
            }
            strips.push(strip);
        }
        if !strips[0].intersect(&strips[1]).is_empty() {
            return Err(io::Error::other("左右主机栏互相重叠"));
        }
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// 窄窗通过右栏图标关闭原 fixture 面板，为双列传输台提供真实可用宽度。
///
/// @author kongweiguang
pub(super) async fn close_fixture_rail_for_narrow(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    let _ = super::frame_signature(cx, window)?;
    let (active, drawer_present) = window.update(cx, |_, window, app| {
        (
            root.read(app).right_rail_acceptance_active_tool(),
            window
                .debug_selector_bounds("workbench-compact-tool-drawer-surface")
                .is_some(),
        )
    })?;
    match active {
        None if !drawer_present => return Ok(()),
        None => return Err(io::Error::other("右栏抽屉仍显示但没有活动文件工具").into()),
        Some(ToolRailToolId::Sftp) => {}
        Some(_) => return Err(io::Error::other("窄窗中存在意外的右栏面板").into()),
    }
    let selector = window.update(cx, |_, window, _| {
        if window
            .debug_selector_bounds("workbench-compact-tool-drawer-surface")
            .is_some()
        {
            "workbench-close-compact-tool-drawer"
        } else {
            "workbench-tool-sftp"
        }
    })?;
    click_visible_element(cx, window, selector)?;
    functional::poll_root(
        cx,
        root,
        window,
        "右栏文件面板关闭",
        |root, window, _| {
            Ok((root.right_rail_acceptance_active_tool().is_none()
                && window
                    .debug_selector_bounds("workbench-compact-tool-drawer-surface")
                    .is_none())
            .then_some(()))
        },
    )
    .await
}

/// 只点击当前实际绘制的可访问控件，避免用内部动作冒充用户输入。
///
/// @author kongweiguang
pub(super) fn click_visible_element(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        let id: ElementId = selector.to_owned().into();
        let node = gpui_kit::base::test_support::find(window, &[], &id)
            .filter(|node| node.visible())
            .ok_or_else(|| io::Error::other(format!("控件未显示: {selector}")))?;
        let position = node.bounds().center();
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
    Ok(())
}

/// 窄窗使用真实分隔条拖到约 200 逻辑像素，再以绘制边界确认，而非假设窗口宽度等于面板宽度。
///
/// @author kongweiguang
async fn resize_left_panel_to_narrow(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    let _ = super::frame_signature(cx, window)?;
    let (left, right) = window.update(cx, |_, window, _| {
        let left = window
            .debug_selector_bounds("files-host-strip-0")
            .ok_or_else(|| io::Error::other("左侧主机栏未绘制"))?;
        let right = window
            .debug_selector_bounds("files-host-strip-1")
            .ok_or_else(|| io::Error::other("右侧主机栏未绘制"))?;
        Ok::<_, io::Error>((left, right))
    })??;
    if (left.origin.y - right.origin.y).abs() > px(2.) {
        return Err(io::Error::other("窄窗已切为上下布局，无法验证双列最小宽度").into());
    }
    if left.size.width > px(220.) {
        let start = point(right.origin.x - px(2.), left.center().y);
        let destination = point(start.x - (left.size.width - px(205.)), start.y);
        window.update(cx, |_, window, app| {
            for event in [
                MouseMoveEvent {
                    position: start,
                    ..Default::default()
                }
                .to_platform_input(),
                MouseDownEvent {
                    button: MouseButton::Left,
                    position: start,
                    click_count: 1,
                    ..Default::default()
                }
                .to_platform_input(),
                MouseMoveEvent {
                    position: destination,
                    pressed_button: Some(MouseButton::Left),
                    ..Default::default()
                }
                .to_platform_input(),
                MouseUpEvent {
                    button: MouseButton::Left,
                    position: destination,
                    click_count: 1,
                    ..Default::default()
                }
                .to_platform_input(),
            ] {
                let _ = window.dispatch_event(event, app);
            }
        })?;
    }
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let _ = super::frame_signature(cx, window)?;
        let width = window.update(cx, |_, window, _| {
            window
                .debug_selector_bounds("files-host-strip-0")
                .map(|bounds| bounds.size.width.as_f32())
        })?;
        if width.is_some_and(|width| (190.0..=220.0).contains(&width)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(
                io::Error::other(format!("左侧面板未收窄至约 200 逻辑像素: {width:?}")).into(),
            );
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// 在重复主机标签仍存在时回读两侧真实绘制边界，并留存关闭命中前的截图。
/// 只有完整 chip、关闭按钮与搜索入口互不遮挡时才接受；等待布局滚动的下一帧。
///
/// @author kongweiguang
async fn capture_active_chip_geometry(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    active_keys: [&str; 2],
) -> Result<serde_json::Value, Box<dyn Error>> {
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let _ = super::frame_signature(cx, window)?;
        let observed = window.update(cx, |_, window, _| {
            let within = |inner: gpui_kit::Bounds<Pixels>, outer: gpui_kit::Bounds<Pixels>| {
                let tolerance = px(0.5);
                !inner.is_empty()
                    && inner.origin.x >= outer.origin.x - tolerance
                    && inner.origin.y >= outer.origin.y - tolerance
                    && inner.origin.x + inner.size.width <= outer.origin.x + outer.size.width + tolerance
                    && inner.origin.y + inner.size.height <= outer.origin.y + outer.size.height + tolerance
            };
            let rect = |bounds: gpui_kit::Bounds<Pixels>| serde_json::json!({
                "x": bounds.origin.x.as_f32(), "y": bounds.origin.y.as_f32(),
                "width": bounds.size.width.as_f32(), "height": bounds.size.height.as_f32(),
            });
            let viewport = window.visual_viewport_bounds();
            let mut sides = Vec::new();
            for (side, key) in active_keys.iter().enumerate() {
                let bounds = |selector: String| window.debug_selector_bounds(&selector)
                    .ok_or_else(|| format!("控件未绘制: {selector}"));
                let strip = bounds(format!("files-host-strip-{side}"))?;
                let tabs = bounds(format!("files-host-tabs-{side}"))?;
                let search = bounds(format!("files-sftp-host-picker-toggle-{side}"))?;
                let chip = bounds(format!("files-host-tab-frame-{side}-{key}"))?;
                let label = bounds(format!("files-host-tab-{side}-{key}"))?;
                let close = bounds(format!("files-host-tab-close-{side}-{key}"))?;
                let label_id: ElementId = format!("files-host-tab-{side}-{key}").into();
                let label_text = gpui_kit::base::test_support::find(window, &[], &label_id)
                    .and_then(|node| node.label().map(str::to_owned))
                    .ok_or_else(|| format!("主机标签缺少无障碍名称: {label_id:?}"))?;
                let clip = tabs.intersect(&strip).intersect(&viewport);
                if !within(chip, clip) || !within(label, chip) || !within(close, chip)
                    || !within(close, clip) || !within(search, strip)
                    || !close.intersect(&search).is_empty() || !chip.intersect(&search).is_empty() {
                    return Err(format!("第 {side} 侧布局被裁切或重叠: strip={strip:?}, clip={clip:?}, chip={chip:?}, label={label:?}, close={close:?}, search={search:?}"));
                }
                sides.push(serde_json::json!({
                    "side": side, "key": key, "strip": rect(strip), "viewport": rect(clip),
                    "chip": rect(chip), "label": rect(label), "labelText": label_text,
                    "close": rect(close), "search": rect(search),
                }));
            }
            Ok::<_, String>(serde_json::json!({ "scaleFactor": window.scale_factor(), "window": rect(viewport), "sides": sides }))
        })?;
        let reason = match observed {
            Ok(mut evidence) => {
                let screenshot = super::save_frame(
                    cx,
                    window,
                    capture
                        .output_directory
                        .join("sftp-workspace-host-chip-bounds.png"),
                )?;
                evidence["screenshot"] = serde_json::to_value(screenshot)?;
                evidence["leftCloseClickedAndSuccessorLoaded"] = serde_json::Value::Bool(true);
                return Ok(evidence);
            }
            Err(reason) => reason,
        };
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("活动主机标签边界验收超时: {reason}")).into());
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// 在生产搜索框输入主机 ID 并验证受控搜索词；窄窗失败保留输入前后焦点和真实几何。
///
/// @author kongweiguang
pub(super) async fn select_host(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    side: usize,
    host: &str,
) -> Result<(), Box<dyn Error>> {
    let option = format!("files-sftp-host-picker-list-{side}-option-{host}");
    let trigger = format!("files-sftp-host-picker-toggle-{side}");
    wait_selector(cx, window, &trigger).await?;
    if capture.requested_window_width == Some(800) {
        capture_host_search_state(cx, root, window, capture, side, host, "before-click")?;
    }
    menus::click_selector(cx, window, &trigger)?;
    let _ = super::frame_signature(cx, window)?;
    if capture.requested_window_width == Some(800) {
        capture_host_search_state(cx, root, window, capture, side, host, "after-click")?;
    }
    window.update(cx, |_, window, app| {
        for key in ["ctrl-a", "backspace"] {
            let key = Keystroke::parse(key).map_err(|error| io::Error::other(error.to_string()))?;
            window.dispatch_keystroke(key, app);
        }
        for character in host.chars() {
            let text = character.to_string();
            window.dispatch_keystroke(
                Keystroke {
                    modifiers: gpui_kit::Modifiers::default(),
                    key: text.clone(),
                    key_char: Some(text),
                },
                app,
            );
        }
        Ok::<_, io::Error>(())
    })??;
    if capture.requested_window_width == Some(800) {
        capture_host_search_state(cx, root, window, capture, side, host, "after-input")?;
    }
    let search_result =
        functional::poll_root(cx, root, window, "主机搜索词", |root, _, cx| {
            Ok(root
                .transfer_workspace_acceptance_query(side, cx)
                .filter(|query| query == host)
                .map(|_| ()))
        })
        .await;
    if search_result.is_err() && capture.requested_window_width == Some(800) {
        capture_host_search_state(cx, root, window, capture, side, host, "timeout")?;
    }
    search_result?;
    wait_selector(cx, window, &option).await?;
    menus::click_selector(cx, window, &option)?;
    Ok(())
}

/// 只读记录当前标签、文件端点、搜索词、焦点和候选项边界；不通过探针改变输入状态。
///
/// @author kongweiguang
fn capture_host_search_state(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    side: usize,
    host: &str,
    phase: &str,
) -> Result<(), Box<dyn Error>> {
    let stem = format!("sftp-host-search-{side}-{host}-{phase}");
    let screenshot = super::save_frame(
        cx,
        window,
        capture.output_directory.join(format!("{stem}.png")),
    )?;
    let diagnostic = window.update(cx, |_, window, app| {
        let trigger = window.debug_selector_bounds(&format!("files-sftp-host-picker-toggle-{side}"));
        let strip = window.debug_selector_bounds(&format!("files-host-strip-{side}"));
        let option = window.debug_selector_bounds(&format!("files-sftp-host-picker-list-{side}-option-{host}"));
        let local = window.debug_selector_bounds(&format!("files-sftp-host-picker-list-{side}-option-local-device"));
        let viewport = window.visual_viewport_bounds();
        let input_id: ElementId = format!("files-sftp-host-picker-input-{side}").into();
        let input = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|item| item.path().contains(&input_id))
            .map(|item| format!("{item:?}"))
            .collect::<Vec<_>>();
        let (query, endpoint, tab, active_tab, active_tool) = root.update(app, |root, cx| {
            let active = root
                .right_rail_acceptance_sftp_workspace_tab_ids(&capture.fixture_host_id)
                .into_iter()
                .find(|id| root.right_rail_acceptance_sftp_workspace_tab_selected(&capture.fixture_host_id, id));
            (
                root.transfer_workspace_acceptance_query(side, cx),
                format!("{:?}", root.transfer_workspace_acceptance_endpoint(side, cx)),
                format!("{:?}", root.transfer_workspace_acceptance_tab()),
                active.map(|id| id.to_string()),
                format!("{:?}", root.right_rail_acceptance_active_tool()),
            )
        });
        let clip = strip.map(|strip| strip.intersect(&viewport));
        serde_json::json!({
            "side": side,
            "requestedHost": host,
            "query": query,
            "endpoint": endpoint,
            "centerTab": tab,
            "activeTransferTabId": active_tab,
            "activeTool": active_tool,
            "drawerBounds": format!("{:?}", window.debug_selector_bounds("workbench-compact-tool-drawer-surface")),
            "triggerBounds": format!("{trigger:?}"),
            "stripBounds": format!("{strip:?}"),
            "triggerVisibleBounds": format!("{:?}", trigger.zip(clip).map(|(trigger, clip)| trigger.intersect(&clip))),
            "clickPoint": trigger.map(|bounds| format!("{:?}", bounds.center())),
            "targetOptionBounds": format!("{option:?}"),
            "localOptionBounds": format!("{local:?}"),
            "viewportBounds": format!("{viewport:?}"),
            "focus": format!("{:?}", window.focused(app)),
            "contextStack": format!("{:?}", window.context_stack()),
            "inputSnapshots": input,
            "screenshot": screenshot,
        })
    })?;
    fs::write(
        capture.output_directory.join(format!("{stem}.json")),
        serde_json::to_vec_pretty(&diagnostic)?,
    )?;
    Ok(())
}

/// 窄窗中先滚动标签视口，再仅点击目标和实际裁剪区域的交集，避免误命中旁边的搜索框。
///
/// @author kongweiguang
async fn click_host_tab(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    side: usize,
    key: &str,
    close: bool,
) -> Result<(), Box<dyn Error>> {
    let container = format!("files-host-tabs-{side}");
    let target = if close {
        format!("files-host-tab-close-{side}-{key}")
    } else {
        format!("files-host-tab-{side}-{key}")
    };
    wait_selector(cx, window, &container).await?;
    for _ in 0..16 {
        let _ = super::frame_signature(cx, window)?;
        let (clip, bounds) = window.update(cx, |_, window, _| {
            let clip = window
                .debug_selector_bounds(&container)
                .ok_or_else(|| io::Error::other("主机标签滚动区域已卸载"))?
                .intersect(&window.visual_viewport_bounds());
            Ok::<_, io::Error>((clip, window.debug_selector_bounds(&target)))
        })??;
        if let Some(bounds) = bounds {
            let visible = bounds.intersect(&clip);
            let fully_visible = visible.size.width + px(0.5) >= bounds.size.width
                && visible.size.height + px(0.5) >= bounds.size.height;
            if f32::from(visible.size.width) >= 6.
                && f32::from(visible.size.height) >= 6.
                && (!close || fully_visible)
            {
                let position = visible.center();
                window.update(cx, |_, window, app| {
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
                })?;
                return Ok(());
            }
        }
        let scroll = match bounds {
            Some(bounds) if bounds.center().x < clip.center().x => px(120.),
            None if key == "local" => px(120.),
            _ => px(-120.),
        };
        window.update(cx, |_, window, app| {
            let _ = window.dispatch_event(
                ScrollWheelEvent {
                    position: clip.center(),
                    delta: ScrollDelta::Pixels(point(scroll, px(0.))),
                    touch_phase: TouchPhase::Moved,
                    ..Default::default()
                }
                .to_platform_input(),
                app,
            );
        })?;
    }
    Err(io::Error::other(format!("主机标签 {target} 无法滚入可视区域")).into())
}

/// 等待当前真实绘制树出现指定控件，防止读取上一帧布局后误点。
///
/// @author kongweiguang
pub(super) async fn wait_selector(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let _ = super::frame_signature(cx, window)?;
        if window.update(cx, |_, window, _| {
            window.debug_selector_bounds(selector).is_some()
        })? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待控件 {selector} 超时")).into());
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// 端点和目录完成必须同时匹配；错误只报告所选目标，不隐式回退。
///
/// @author kongweiguang
pub(super) async fn wait_endpoint(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    side: usize,
    expected: &FileEndpoint,
) -> Result<(), Box<dyn Error>> {
    functional::poll_root(cx, root, window, "双端目录就绪", |root, _, cx| {
        let Some((endpoint, ready, error)) = root.transfer_workspace_acceptance_endpoint(side, cx)
        else {
            return Ok(None);
        };
        if endpoint.as_ref() == Some(expected)
            && let Some(error) = error
        {
            return Err(error);
        }
        Ok((endpoint.as_ref() == Some(expected) && ready).then_some(()))
    })
    .await
}

/// 本机标签只在真实本机目录已读回后计为激活成功。
///
/// @author kongweiguang
async fn wait_local(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    side: usize,
) -> Result<(), Box<dyn Error>> {
    functional::poll_root(cx, root, window, "本机目录就绪", |root, _, cx| {
        Ok(root
            .transfer_workspace_acceptance_endpoint(side, cx)
            .filter(|(endpoint, ready, _)| {
                *ready && matches!(endpoint, Some(FileEndpoint::Local { .. }))
            })
            .map(|_| ()))
    })
    .await
}

/// 通过稳定标签 ID 区分同主机重复标签，禁止用主机名代替当前活动项。
///
/// @author kongweiguang
async fn wait_active_host_tab(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    side: usize,
    host: &HostId,
) -> Result<String, Box<dyn Error>> {
    functional::poll_root(cx, root, window, "主机标签活动项", |root, _, cx| {
        Ok(root
            .transfer_workspace_acceptance_host_tabs(side, cx)
            .and_then(|tabs| {
                tabs.into_iter()
                    .find(|(_, candidate, active)| *active && candidate.as_ref() == Some(host))
                    .map(|(key, _, _)| key)
            }))
    })
    .await
}

/// 真实行点击后核对选择集合，避免把仅有焦点的行当作可传输选择。
///
/// @author kongweiguang
async fn wait_selected(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    side: usize,
    name: &str,
) -> Result<(), Box<dyn Error>> {
    functional::poll_root(cx, root, window, "文件行已选中", |root, _, cx| {
        Ok(root
            .transfer_workspace_acceptance_selected_names(side, cx)
            .filter(|names| names.len() == 1 && names[0] == name)
            .map(|_| ()))
    })
    .await
}

/// 捕获真实行、列表裁切、目录和选择，供每次滚动与点击前后的状态对照。
///
/// @author kongweiguang
fn fixture_row_geometry(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    side: usize,
    name: &str,
) -> Result<
    (
        Option<Bounds<Pixels>>,
        Bounds<Pixels>,
        Bounds<Pixels>,
        serde_json::Value,
    ),
    Box<dyn Error>,
> {
    let _ = super::frame_signature(cx, window)?;
    window
        .update(cx, |_, window, app| {
            let row = window.debug_selector_bounds(&format!("central-files-row-{side}-{name}"));
            let list = window
                .debug_selector_bounds(&format!("central-files-browser-list-{side}"))
                .ok_or_else(|| io::Error::other("文件列表未绘制"))?;
            let viewport = window.visual_viewport_bounds();
            let (endpoint, selection) = root.update(app, |root, cx| {
                (
                    format!(
                        "{:?}",
                        root.transfer_workspace_acceptance_endpoint(side, cx)
                    ),
                    root.transfer_workspace_acceptance_selected_names(side, cx),
                )
            });
            let diagnostic = serde_json::json!({
                "side": side,
                "name": name,
                "endpoint": endpoint,
                "selectedNames": selection,
                "rowBounds": format!("{row:?}"),
                "listBounds": format!("{list:?}"),
                "windowBounds": format!("{viewport:?}"),
                "clickPoint": row.map(|bounds| format!("{:?}", bounds.center())),
            });
            Ok::<_, io::Error>((row, list, viewport, diagnostic))
        })?
        .map_err(Into::into)
}

/// 记录中央列表目录与选区，再使用与右栏相同的真实滚动查找流程。
///
/// @author kongweiguang
async fn reveal_fixture_row(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    side: usize,
    name: &str,
    stage: &str,
) -> Result<(), Box<dyn Error>> {
    let (_, _, _, context) = fixture_row_geometry(cx, root, window, side, name)?;
    fs::write(
        capture
            .output_directory
            .join(format!("sftp-{stage}-row-context.json")),
        serde_json::to_vec_pretty(&context)?,
    )?;
    reveal_file_row(
        cx,
        window,
        capture,
        &format!("central-files-browser-list-{side}"),
        &format!("central-files-row-{side}-{name}"),
        stage,
    )
    .await
}

/// 虚拟行未渲染或被裁切时从列表顶部真实滚动扫描，失败也保留末帧和几何供定位。
///
/// @author kongweiguang
pub(super) async fn reveal_file_row(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    list_selector: &str,
    row_selector: &str,
    stage: &str,
) -> Result<(), Box<dyn Error>> {
    let initial_frame = super::save_frame(
        cx,
        window,
        capture
            .output_directory
            .join(format!("sftp-{stage}-row-before-reveal.png")),
    )?;
    let mut initial = None;
    let mut last_geometry = serde_json::Value::Null;
    let mut scroll_count = 0;
    for attempt in 0..80 {
        let _ = super::frame_signature(cx, window)?;
        let (row, list, viewport) = window.update(cx, |_, window, _| {
            (
                window.debug_selector_bounds(row_selector),
                window.debug_selector_bounds(list_selector),
                window.visual_viewport_bounds(),
            )
        })?;
        let geometry = serde_json::json!({
            "rowSelector": row_selector,
            "listSelector": list_selector,
            "rowBounds": format!("{row:?}"),
            "listBounds": format!("{list:?}"),
            "windowBounds": format!("{viewport:?}"),
            "clickPoint": row.map(|bounds| format!("{:?}", bounds.center())),
        });
        if initial.is_none() {
            initial = Some(geometry.clone());
        }
        last_geometry = geometry.clone();
        let clip = list.map(|list| list.intersect(&viewport));
        if clip.is_some_and(|clip| row.is_some_and(|row| row_inside_clip(row, clip))) {
            fs::write(
                capture
                    .output_directory
                    .join(format!("sftp-{stage}-row-click.json")),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "initial": initial,
                    "beforeClick": geometry,
                    "scrollCount": scroll_count,
                    "frameCount": attempt + 1,
                    "initialFrame": initial_frame,
                }))?,
            )?;
            return Ok(());
        }
        if let Some(clip) = clip.filter(|clip| !clip.is_empty()) {
            let delta = if scroll_count == 0 { 10000. } else { -120. };
            window.update(cx, |_, window, app| {
                let _ = window.dispatch_event(
                    ScrollWheelEvent {
                        position: clip.center(),
                        delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
                        touch_phase: TouchPhase::Moved,
                        ..ScrollWheelEvent::default()
                    }
                    .to_platform_input(),
                    app,
                );
            })?;
            scroll_count += 1;
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
    let final_frame = super::save_frame(
        cx,
        window,
        capture
            .output_directory
            .join(format!("sftp-{stage}-row-not-visible.png")),
    )?;
    fs::write(
        capture
            .output_directory
            .join(format!("sftp-{stage}-row-click.json")),
        serde_json::to_vec_pretty(&serde_json::json!({
            "initial": initial,
            "last": last_geometry,
            "scrollCount": scroll_count,
            "initialFrame": initial_frame,
            "finalFrame": final_frame,
        }))?,
    )?;
    Err(io::Error::other(format!("文件行未进入列表裁切区域: {row_selector}")).into())
}

/// 文件行必须完整处于列表与窗口共同区域，避免滚动区外的调试坐标点击到预览或队列。
///
/// @author kongweiguang
fn row_inside_clip(row: Bounds<Pixels>, clip: Bounds<Pixels>) -> bool {
    let epsilon = px(0.75);
    !row.is_empty()
        && !clip.is_empty()
        && row.origin.x >= clip.origin.x - epsilon
        && row.origin.y >= clip.origin.y - epsilon
        && row.origin.x + row.size.width <= clip.origin.x + clip.size.width + epsilon
        && row.origin.y + row.size.height <= clip.origin.y + clip.size.height + epsilon
}

/// 真实点击后仍以唯一选择回读为准，并保存失败时的最新画面及目录状态。
///
/// @author kongweiguang
async fn click_fixture_row(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    side: usize,
    name: &str,
    stage: &str,
) -> Result<(), Box<dyn Error>> {
    reveal_fixture_row(cx, root, window, capture, side, name, stage).await?;
    menus::click_selector(cx, window, &format!("central-files-row-{side}-{name}"))?;
    let result = wait_selected(cx, root, window, side, name).await;
    let (_, _, _, geometry) = fixture_row_geometry(cx, root, window, side, name)?;
    let frame = super::save_frame(
        cx,
        window,
        capture
            .output_directory
            .join(format!("sftp-{stage}-row-after-click.png")),
    )?;
    fs::write(
        capture
            .output_directory
            .join(format!("sftp-{stage}-row-selection.json")),
        serde_json::to_vec_pretty(&serde_json::json!({
            "result": if result.is_ok() { "selected" } else { "timeout" },
            "geometry": geometry,
            "frame": frame,
        }))?,
    )?;
    result
}

/// 双击前先显露列表行；第二次按下前复核裁切，防止两次点击落到不同位置。
///
/// @author kongweiguang
pub(super) async fn double_click_selector(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    side: usize,
    selector: &str,
    stage: &str,
) -> Result<(), Box<dyn Error>> {
    let list_selector = format!("central-files-browser-list-{side}");
    reveal_file_row(cx, window, capture, &list_selector, selector, stage).await?;
    menus::click_selector(cx, window, selector)?;
    let _ = super::frame_signature(cx, window)?;
    window.update(cx, |_, window, app| {
        let row = window
            .debug_selector_bounds(selector)
            .ok_or_else(|| io::Error::other("双击目标已卸载"))?;
        let list = window
            .debug_selector_bounds(&list_selector)
            .ok_or_else(|| io::Error::other("双击列表已卸载"))?;
        if !row_inside_clip(row, list.intersect(&window.visual_viewport_bounds())) {
            return Err(io::Error::other("双击期间目标移出列表"));
        }
        let position = row.center();
        for event in [
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                click_count: 2,
                ..Default::default()
            }
            .to_platform_input(),
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                click_count: 2,
                ..Default::default()
            }
            .to_platform_input(),
        ] {
            let _ = window.dispatch_event(event, app);
        }
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// 右键前按实际列表裁切显露目标，再使用真实事件打开生产上下文菜单。
///
/// @author kongweiguang
async fn open_context_menu(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    side: usize,
    selector: &str,
    stage: &str,
) -> Result<(), Box<dyn Error>> {
    reveal_file_row(
        cx,
        window,
        capture,
        &format!("central-files-browser-list-{side}"),
        selector,
        stage,
    )
    .await?;
    window.update(cx, |_, window, app| {
        let position = window
            .debug_selector_bounds(selector)
            .ok_or_else(|| io::Error::other("文件菜单行已卸载"))?
            .center();
        for event in [
            MouseMoveEvent {
                position,
                ..Default::default()
            }
            .to_platform_input(),
            MouseDownEvent {
                button: MouseButton::Right,
                position,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            MouseUpEvent {
                button: MouseButton::Right,
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
    wait_selector(cx, window, "rr-popup-menu").await
}

/// 从真实无障碍菜单检查常用命令和移除项，避免截图遮挡导致的误判。
///
/// @author kongweiguang
fn check_file_menu(cx: &mut AsyncApp, window: AnyWindowHandle) -> Result<(), Box<dyn Error>> {
    let labels = window.update(cx, |_, window, _| {
        gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|node| node.role() == Some(gpui_kit::Role::MenuItem))
            .filter_map(|node| node.label().map(str::to_owned))
            .collect::<Vec<_>>()
    })?;
    for expected in [
        ["复制到右侧", "Copy to right panel"],
        ["复制路径", "Copy path"],
        ["重命名", "Rename"],
        ["删除", "Delete"],
    ] {
        if !labels
            .iter()
            .any(|label| expected.contains(&label.as_str()))
        {
            return Err(io::Error::other(format!("菜单缺少 {expected:?}: {labels:?}")).into());
        }
    }
    if labels.iter().any(|label| {
        matches!(
            label.as_str(),
            "工作区打开"
                | "Open in workspace"
                | "复制项目"
                | "Copy item"
                | "粘贴到此目录"
                | "Paste into this folder"
                | "下载到剪贴板"
                | "Download to clipboard"
        )
    }) {
        return Err(io::Error::other(format!("菜单仍包含已移除操作: {labels:?}")).into());
    }
    Ok(())
}

/// 在已打开的真实菜单中按目标侧的无障碍标签定位复制命令并发出鼠标事件。
///
/// @author kongweiguang
fn click_copy_menu_item(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    target_side: usize,
) -> Result<(), Box<dyn Error>> {
    let labels = match target_side {
        0 => ["复制到左侧", "Copy to left panel"],
        1 => ["复制到右侧", "Copy to right panel"],
        _ => return Err(io::Error::other("无效的目标侧").into()),
    };
    window.update(cx, |_, window, app| {
        let node = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .find(|node| {
                node.role() == Some(gpui_kit::Role::MenuItem)
                    && node.label().is_some_and(|label| labels.contains(&label))
            })
            .ok_or_else(|| io::Error::other("真实菜单缺少目标侧复制命令"))?;
        if !node.visible() || node.disabled() == Some(true) {
            return Err(io::Error::other("目标侧复制命令不可用"));
        }
        let position = node.bounds().center();
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
    Ok(())
}

/// Escape 只关闭顶部菜单，并等待控件从真实绘制树消失。
///
/// @author kongweiguang
async fn dismiss_menu(cx: &mut AsyncApp, window: AnyWindowHandle) -> Result<(), Box<dyn Error>> {
    let key = Keystroke::parse("escape").map_err(|error| io::Error::other(error.to_string()))?;
    window.update(cx, |_, window, app| {
        let _ = window.dispatch_event(
            KeyDownEvent {
                keystroke: key.clone(),
                is_held: false,
                prefer_character_input: false,
            }
            .to_platform_input(),
            app,
        );
        let _ = window.dispatch_event(KeyUpEvent { keystroke: key }.to_platform_input(), app);
    })?;
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let _ = super::frame_signature(cx, window)?;
        if !window.update(cx, |_, window, _| {
            window.debug_selector_bounds("rr-popup-menu").is_some()
        })? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("右键菜单未关闭").into());
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// 拖动前后读取预览与鼠标差值，确认小预览随光标而不是停在原文件行。
///
/// @author kongweiguang
async fn drag_to_other_side(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    name: &str,
) -> Result<(), Box<dyn Error>> {
    let source = format!("central-files-row-0-{name}");
    let target = "central-files-browser-list-1";
    let stage = if name.ends_with("-drag.txt") {
        "drag-file"
    } else {
        "drag-directory"
    };
    click_fixture_row(cx, root, window, capture, 0, name, stage).await?;
    reveal_fixture_row(
        cx,
        root,
        window,
        capture,
        0,
        name,
        &format!("{stage}-selected"),
    )
    .await?;
    wait_selector(cx, window, target).await?;
    let _ = super::frame_signature(cx, window)?;
    let (start, destination) = window.update(cx, |_, window, _| {
        let source = window
            .debug_selector_bounds(&source)
            .ok_or_else(|| io::Error::other("拖动源行已卸载"))?;
        let target = window
            .debug_selector_bounds(target)
            .ok_or_else(|| io::Error::other("对侧目录列表已卸载"))?;
        Ok::<_, io::Error>((source.center(), target.center()))
    })??;
    let intermediate = point(start.x + px(30.), start.y + px(12.));
    window.update(cx, |_, window, app| {
        let _ = window.dispatch_event(
            MouseMoveEvent {
                position: start,
                ..Default::default()
            }
            .to_platform_input(),
            app,
        );
        let _ = window.dispatch_event(
            MouseDownEvent {
                button: MouseButton::Left,
                position: start,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            app,
        );
        let _ = window.dispatch_event(
            MouseMoveEvent {
                position: intermediate,
                pressed_button: Some(MouseButton::Left),
                ..Default::default()
            }
            .to_platform_input(),
            app,
        );
    })?;
    let first = wait_drag_preview(cx, window).await?;
    let _ = super::save_frame(
        cx,
        window,
        capture
            .output_directory
            .join(format!("drag-{name}-first.png")),
    )?;
    window.update(cx, |_, window, app| {
        let _ = window.dispatch_event(
            MouseMoveEvent {
                position: destination,
                pressed_button: Some(MouseButton::Left),
                ..Default::default()
            }
            .to_platform_input(),
            app,
        );
    })?;
    let second = wait_drag_preview(cx, window).await?;
    let _ = super::save_frame(
        cx,
        window,
        capture
            .output_directory
            .join(format!("drag-{name}-second.png")),
    )?;
    let first_offset = (
        f32::from(first.x - intermediate.x),
        f32::from(first.y - intermediate.y),
    );
    let second_offset = (
        f32::from(second.x - destination.x),
        f32::from(second.y - destination.y),
    );
    if (first_offset.0 - second_offset.0).abs() > 3.
        || (first_offset.1 - second_offset.1).abs() > 3.
        || second_offset.0.abs() > 48.
        || second_offset.1.abs() > 48.
    {
        return Err(io::Error::other(format!(
            "拖拽预览未跟随鼠标: {first_offset:?} -> {second_offset:?}"
        ))
        .into());
    }
    window.update(cx, |_, window, app| {
        let _ = window.dispatch_event(
            MouseUpEvent {
                button: MouseButton::Left,
                position: destination,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            app,
        );
    })?;
    Ok(())
}

/// 等待拖拽预览真实绘制，不能仅凭开始拖动的事件判断预览存在。
///
/// @author kongweiguang
async fn wait_drag_preview(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<Point<Pixels>, Box<dyn Error>> {
    wait_selector(cx, window, DRAG_PREVIEW_SELECTOR).await?;
    window
        .update(cx, |_, window, _| {
            window
                .debug_selector_bounds(DRAG_PREVIEW_SELECTOR)
                .map(|bounds| bounds.origin)
                .ok_or_else(|| io::Error::other("拖拽预览已消失"))
        })?
        .map_err(Into::into)
}

/// 只接受精确源、目标、类型和已确认提交回执的终态，失败按原始队列状态报告。
///
/// @author kongweiguang
async fn wait_transfer(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    source: &FileEndpoint,
    destination: &FileEndpoint,
    kind: TransferKind,
) -> Result<(), Box<dyn Error>> {
    functional::poll_root(cx, root, window, "传输终态", |root, _, cx| {
        let Some(rows) = root.transfer_workspace_acceptance_rows(cx) else {
            return Ok(None);
        };
        let mut matching = rows.into_iter().filter(|row| {
            row.request.source == *source
                && row.request.destination == *destination
                && row.request.kind == kind
        });
        let Some(row) = matching.next() else {
            return Ok(None);
        };
        if matching.next().is_some() {
            return Err("同一验收操作产生多条传输任务".to_owned());
        }
        match row.status {
            TransferStatus::Succeeded
                if row.actual_destination.as_ref() == Some(destination)
                    && row.commit_summary.committed_entries() > 0 =>
            {
                Ok(Some(()))
            }
            TransferStatus::Succeeded => Err("传输成功但目标或提交回执不匹配".to_owned()),
            TransferStatus::Failed
            | TransferStatus::Cancelled
            | TransferStatus::Skipped
            | TransferStatus::Unchanged => Err(row
                .safe_error
                .unwrap_or_else(|| format!("传输结束状态: {:?}", row.status))),
            _ => Ok(None),
        }
    })
    .await
}

/// 完整读取两端文本并核对原 endpoint 和内容，传输状态不能代替实际数据结果。
///
/// @author kongweiguang
pub(super) async fn assert_text(
    executor: &ApplicationTaskExecutor,
    files: &FileService,
    endpoint: &FileEndpoint,
    expected: &str,
) -> Result<(), Box<dyn Error>> {
    let service = files.clone();
    let requested = endpoint.clone();
    let document = executor
        .run(move |cancellation| async move {
            service.read_text_file(cancellation, requested, 4096).await
        })
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    if &document.endpoint != endpoint || document.truncated || document.content != expected {
        return Err(io::Error::other("验收文件内容或目标不匹配").into());
    }
    Ok(())
}

/// 恢复原 fixture 终端，保持后续右栏验证目标与本轮启动时一致。
///
/// @author kongweiguang
async fn restore_terminal(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    tab: &TabId,
) -> Result<(), Box<dyn Error>> {
    functional::poll_root(cx, root, window, "恢复原终端", |root, window, cx| {
        Ok(root
            .restore_right_rail_acceptance_fixture_tab(tab, window, cx)
            .then_some(()))
    })
    .await
}
