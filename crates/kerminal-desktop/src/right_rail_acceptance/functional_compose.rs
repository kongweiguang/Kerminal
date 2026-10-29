//! Exercises Compose YAML activation against a real fixture host and the production Root event handler.
//!
//! @author kongweiguang

use std::{error::Error, io};

use gpui_kit::{AnyWindowHandle, AsyncApp, Entity};
use kerminal_application::{AppError, ApplicationTaskExecutor, files::FileService};
use kerminal_domain::{
    files::{FileEndpoint, RemotePath},
    identity::{HostId, TabId},
};
use kerminal_ui::{ComposeAcceptanceOpenRequest, RootView};

use super::RightRailCaptureContext;
use super::functional;

const COMPOSE_FIXTURE_READ_LIMIT_BYTES: usize = 1024 * 1024;

/// Reads the existing fixture Compose file and exercises its production read-only workspace flow without mutating remote files.
///
/// @author kongweiguang
pub(super) async fn run(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<functional::FunctionalRecord>,
    files: &FileService,
    executor: &ApplicationTaskExecutor,
) -> Result<(), Box<dyn Error>> {
    let (fixture_root, file_path, fixture_project) =
        fixture_compose_paths(&capture.remote_sftp_directory)?;
    // These paths stay borrowed from one validated manifest derivation throughout activation and reopen.
    let fixture_root = &fixture_root;
    let file_path = &file_path;
    let fixture_project = fixture_project.as_str();
    let host_id = &capture.fixture_host_id;
    let selected = window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.activate_right_rail_tool_for_acceptance(
                kerminal_application::preferences::settings::ToolRailToolId::Containers,
                window,
                root_cx,
            );
            root.right_rail_acceptance_active_tool()
                == Some(kerminal_application::preferences::settings::ToolRailToolId::Containers)
        })
    })?;
    if !selected {
        return Err(io::Error::other("Compose 验收无法激活容器工具").into());
    }
    super::wait_for_panel_readiness(
        cx,
        root,
        window,
        kerminal_application::preferences::settings::ToolRailToolId::Containers,
        host_id,
        "Compose project",
    )
    .await?;

    let terminal_tab_id =
        functional::poll_root(cx, root, window, "fixture terminal tab", |root, _, _| {
            root.right_rail_acceptance_compose_terminal_tab_id(host_id)
        })
        .await?;
    let target =
        functional::poll_root(cx, root, window, "Compose project target", |root, _, cx| {
            root.right_rail_acceptance_compose_target(host_id, fixture_project, cx)
        })
        .await?;
    if target.host_id.as_ref() != Some(host_id)
        || target.project != fixture_project
        || target.working_dir != fixture_root.as_str()
        || target.config_files.len() != 1
        || target.config_files.first().map(String::as_str) != Some(file_path.as_str())
    {
        return Err(io::Error::other(
            "live Compose target 未引用当前 fixture root 下的唯一 compose.yml",
        )
        .into());
    }
    let expected_content = read_fixture_compose_content(
        executor,
        files,
        FileEndpoint::Remote {
            host_id: host_id.clone(),
            path: file_path.clone(),
        },
    )
    .await?;
    let open_request = ComposeAcceptanceOpenRequest::new(
        host_id.clone(),
        terminal_tab_id.clone(),
        target.clone(),
        file_path.clone(),
        "compose.yml".to_owned(),
        fixture_root.clone(),
    );
    functional::poll_root(cx, root, window, "select Compose project", |root, _, cx| {
        root.select_right_rail_acceptance_compose_target(host_id, &target, cx)
            .map(|selected| selected.then_some(()))
    })
    .await?;
    functional::poll_root(cx, root, window, "Compose project detail", |root, _, cx| {
        root.right_rail_acceptance_compose_target_selected(host_id, &target, cx)
            .map(|ready| ready.then_some(()))
    })
    .await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "compose-project-selected",
        "existing_fixture_host_project_selected_from_production_snapshot",
    )
    .await?;

    let tabs_before = functional::poll_root(
        cx,
        root,
        window,
        "initial workspace tab count",
        |root, _, _| Ok(root.right_rail_acceptance_workspace_tab_count()),
    )
    .await?;
    functional::poll_root(
        cx,
        root,
        window,
        "request Compose YAML open",
        |root, _, cx| root.open_right_rail_acceptance_compose_config(&open_request, cx),
    )
    .await?;
    let (compose_tab_id, tabs_after_first) = wait_for_compose_file(
        cx,
        root,
        window,
        host_id,
        file_path,
        fixture_root,
        &expected_content,
    )
    .await?;
    if tabs_after_first != tabs_before + 1 {
        return Err(
            io::Error::other("Compose YAML 首次打开未创建唯一、活动的 WorkspaceFile 标签").into(),
        );
    }
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "compose-yaml-opened",
        "root_handler_opened_exact_readonly_compose_workspace_file",
    )
    .await?;
    functional::poll_root(
        cx,
        root,
        window,
        "Compose workspace host context",
        |root, _, cx| {
            root.right_rail_acceptance_compose_context_ready(host_id, &compose_tab_id, &target, cx)
                .map(|ready| ready.then_some(()))
        },
    )
    .await?;

    functional::poll_root(
        cx,
        root,
        window,
        "restore fixture terminal",
        |root, window, cx| {
            root.activate_right_rail_acceptance_compose_terminal_tab(
                host_id,
                &terminal_tab_id,
                window,
                cx,
            )
            .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    functional::poll_root(
        cx,
        root,
        window,
        "restored Compose host context",
        |root, _, cx| {
            root.right_rail_acceptance_compose_terminal_context_ready(
                host_id,
                &terminal_tab_id,
                &target,
                cx,
            )
            .map(|ready| ready.then_some(()))
        },
    )
    .await?;

    let tabs_before_reopen = functional::poll_root(
        cx,
        root,
        window,
        "repeat workspace tab count",
        |root, _, _| Ok(root.right_rail_acceptance_workspace_tab_count()),
    )
    .await?;
    functional::poll_root(
        cx,
        root,
        window,
        "request Compose YAML reopen",
        |root, _, cx| root.open_right_rail_acceptance_compose_config(&open_request, cx),
    )
    .await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "compose-yaml-reopen-requested",
        "production_project_requested_reactivation_of_existing_readonly_file_tab",
    )
    .await?;
    let (reopened_compose_tab_id, tabs_after_reopen) = wait_for_compose_file(
        cx,
        root,
        window,
        host_id,
        file_path,
        fixture_root,
        &expected_content,
    )
    .await?;
    if reopened_compose_tab_id != compose_tab_id
        || tabs_after_reopen != tabs_before_reopen
        || tabs_after_reopen != tabs_after_first
    {
        return Err(io::Error::other("重复打开 Compose YAML 创建了重复工作区标签").into());
    }
    functional::poll_root(
        cx,
        root,
        window,
        "reopened Compose host context",
        |root, _, cx| {
            root.right_rail_acceptance_compose_context_ready(host_id, &compose_tab_id, &target, cx)
                .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "compose-yaml-reopened",
        "same_readonly_workspace_tab_reactivated_without_duplicate",
    )
    .await?;
    functional::poll_root(
        cx,
        root,
        window,
        "restore fixture terminal after Compose",
        |root, window, cx| {
            root.activate_right_rail_acceptance_compose_terminal_tab(
                host_id,
                &terminal_tab_id,
                window,
                cx,
            )
            .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    functional::poll_root(
        cx,
        root,
        window,
        "final fixture host context",
        |root, _, cx| {
            root.right_rail_acceptance_compose_terminal_context_ready(
                host_id,
                &terminal_tab_id,
                &target,
                cx,
            )
            .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    Ok(())
}

/// Waits for the registered Compose tab and verifies its editor body matches the earlier FileService read.
///
/// @author kongweiguang
async fn wait_for_compose_file(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    file_path: &RemotePath,
    fixture_root: &RemotePath,
    expected_content: &str,
) -> Result<(TabId, usize), Box<dyn Error>> {
    functional::poll_root(
        cx,
        root,
        window,
        "ready read-only Compose editor",
        |root, _, cx| {
            root.right_rail_acceptance_compose_file_ready(
                host_id,
                file_path.as_str(),
                "compose.yml",
                fixture_root.as_str(),
                expected_content,
                cx,
            )
        },
    )
    .await
}

/// Derives the fixture root from its manifest-owned SFTP directory and rejects paths outside this run's isolated root.
///
/// @author kongweiguang
fn fixture_compose_paths(
    remote_sftp_directory: &str,
) -> Result<(RemotePath, RemotePath, String), io::Error> {
    let sftp_directory = RemotePath::parse(remote_sftp_directory)
        .map_err(|_| io::Error::other("验收 Compose SFTP 目录无效"))?;
    if sftp_directory.as_str() != remote_sftp_directory {
        return Err(io::Error::other("验收 Compose SFTP 目录不是规范绝对路径"));
    }
    let root_text = sftp_directory
        .as_str()
        .strip_suffix("/home/sftp")
        .ok_or_else(|| io::Error::other("验收 Compose SFTP 目录不属于隔离 fixture"))?;
    let run_id = root_text
        .strip_prefix("/run/kerminal-right-rail-")
        .filter(|run_id| {
            run_id.len() == 12
                && run_id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or_else(|| io::Error::other("验收 Compose fixture root 缺少有效 run ID"))?;
    let fixture_root = RemotePath::parse(root_text)
        .map_err(|_| io::Error::other("验收 Compose fixture root 无效"))?;
    let expected_sftp_directory = fixture_root
        .join_name("home")
        .and_then(|home| home.join_name("sftp"))
        .map_err(|_| io::Error::other("验收 Compose fixture SFTP 路径无效"))?;
    if expected_sftp_directory != sftp_directory {
        return Err(io::Error::other(
            "验收 Compose SFTP 目录与 fixture root 不一致",
        ));
    }
    let compose_path = fixture_root
        .join_name("compose.yml")
        .map_err(|_| io::Error::other("验收 Compose 配置路径无效"))?;
    Ok((
        fixture_root,
        compose_path,
        format!("kerminal-rr-compose-{run_id}"),
    ))
}

/// Reads the existing Compose endpoint through the shared file service and rejects a truncated or mismatched response.
///
/// @author kongweiguang
async fn read_fixture_compose_content(
    executor: &ApplicationTaskExecutor,
    files: &FileService,
    endpoint: FileEndpoint,
) -> Result<String, Box<dyn Error>> {
    let files = files.clone();
    let document = executor
        .run(move |cancellation| async move {
            let document = files
                .read_text_file(
                    cancellation,
                    endpoint.clone(),
                    COMPOSE_FIXTURE_READ_LIMIT_BYTES,
                )
                .await?;
            if document.endpoint != endpoint
                || document.truncated
                || document.content.trim().is_empty()
            {
                return Err(AppError::conflict(
                    "Compose fixture 文件读取不完整或目标不一致",
                ));
            }
            Ok(document.content)
        })
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(document)
}
