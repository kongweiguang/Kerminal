//! Real upload, download, and conflict acceptance against only the manifest fixture directories.
//!
//! @author kongweiguang

use std::{error::Error, fs, io, io::Write, path::Path};

use gpui_kit::{
    AnyWindowHandle, AsyncApp, Bounds, Entity, InputEvent, KeyDownEvent, KeyUpEvent, Keystroke,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollDelta,
    ScrollWheelEvent, TouchPhase, point, px,
};
use kerminal_application::{
    ApplicationTaskExecutor,
    files::{FileService, MAX_FILE_PREVIEW_BYTES, MAX_TEXT_FILE_READ_BYTES},
    preferences::settings::ToolRailToolId,
};
use kerminal_domain::{
    files::{
        ConflictPolicy, DeleteTargetKind, FileEndpoint, FileEntryKind, FileRevision, TransferKind,
        TransferStatus, TransferSummary, WriteTextRequest, WriteTextStrategy,
    },
    identity::{HostId, TransferId},
};
use kerminal_ui::{RightRailSftpTransferItem, RightRailSftpTransferTarget, RootView};
use sha2::{Digest, Sha256};

use super::{
    RightRailCaptureContext,
    functional::{self, FunctionalRecord},
};

/// Keep each expected queue source and destination paired to avoid matching results across fixture cases.
///
/// @author kongweiguang
#[derive(Clone, Copy)]
struct TransferEndpoints<'a> {
    source: &'a FileEndpoint,
    destination: &'a FileEndpoint,
}

impl<'a> TransferEndpoints<'a> {
    /// Bind one source and destination pair for exact queue-result checks.
    ///
    /// @author kongweiguang
    fn new(source: &'a FileEndpoint, destination: &'a FileEndpoint) -> Self {
        Self {
            source,
            destination,
        }
    }
}

/// 使用隔离 fixture 验证真实传输、权限保留、正文隐藏和菜单行为；无论成功或失败均按本轮身份清理。
///
/// @author kongweiguang
pub(super) async fn run(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    executor: &ApplicationTaskExecutor,
) -> Result<(), Box<dyn Error>> {
    let host_id = &capture.fixture_host_id;
    let remote_directory = capture.remote_sftp_directory.as_str();
    let local_root = capture.local_files_root.as_path();
    let target = RightRailSftpTransferTarget::new(host_id, remote_directory, local_root);
    let run_id = capture
        .output_directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("acceptance output directory has no UTF-8 run name"))?;
    let name_prefix = format!("rr-ui-{run_id}-");

    window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.activate_right_rail_tool_for_acceptance(ToolRailToolId::Sftp, window, root_cx);
        })
    })?;
    functional::poll_root(
        cx,
        root,
        window,
        "fixture SFTP transfer directory",
        |root, _, cx| {
            root.navigate_right_rail_acceptance_sftp_directory(host_id, remote_directory, cx)?;
            root.right_rail_acceptance_sftp_directory_ready(host_id, remote_directory, cx)
                .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    functional::poll_root(
        cx,
        root,
        window,
        "fixture local transfer root",
        |root, _, cx| {
            root.open_right_rail_acceptance_sftp_local_root(&target, cx)
                .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    let files = read_file_service(cx, root, window, &target).await?;
    assert_compact_path_controls(cx, window)?;

    let case_result: Result<(), Box<dyn Error>> = async {
        let mut completed_ids = Vec::new();
        let upload_name = format!("{name_prefix}upload");
        let download_name = format!("{name_prefix}download");
        let overwrite_name = format!("{name_prefix}overwrite");
        let rename_name = format!("{name_prefix}rename");
        let editor_first_name = format!("{name_prefix}editor-first");
        let editor_second_name = format!("{name_prefix}editor-second");

        let upload_bytes = fixture_payload(run_id, "upload", "source");
        write_fixture_source(local_root, run_id, &upload_name, &upload_bytes, false)?;
        let upload_source = local_file_endpoint(local_root, &upload_name)?;
        let upload_target = remote_file_endpoint(host_id, remote_directory, &upload_name)?;
        let upload_item = RightRailSftpTransferItem::new(&upload_name, &upload_name, true, false);
        let upload_endpoints = TransferEndpoints::new(&upload_source, &upload_target);
        assert_sha256(executor, &files, upload_source.clone(), &upload_bytes).await?;
        let known_ids = transfer_ids(read_transfer_rows(cx, root, window, &target).await?);
        submit_transfer(cx, root, window, &target, upload_item).await?;
        let upload_result = wait_for_transfer(
            cx,
            root,
            window,
            &target,
            upload_item,
            upload_endpoints,
            &known_ids,
        )
        .await?;
        completed_ids.push(upload_result.id.clone());
        require_actual_destination(&upload_result, &upload_target)?;
        assert_sha256(executor, &files, upload_source, &upload_bytes).await?;
        assert_sha256(executor, &files, upload_target, &upload_bytes).await?;
        verify_selected_upload_without_preview_panel(
            cx,
            root,
            window,
            capture,
            host_id,
            remote_directory,
            &upload_name,
        )
        .await?;
        functional::checkpoint(
            cx,
            window,
            capture,
            records,
            "sftp-transfer-uploaded",
            "production_queue_succeeded_and_remote_sha256_matches_local_source",
        )
        .await?;

        let download_source = upload_result
            .actual_destination
            .clone()
            .ok_or_else(|| io::Error::other("uploaded fixture file has no actual destination"))?;
        let download_target = local_file_endpoint(local_root, &download_name)?;
        let download_item =
            RightRailSftpTransferItem::new(&upload_name, &download_name, false, false);
        let download_endpoints = TransferEndpoints::new(&download_source, &download_target);
        let known_ids = transfer_ids(read_transfer_rows(cx, root, window, &target).await?);
        submit_transfer(cx, root, window, &target, download_item).await?;
        let download_result = wait_for_transfer(
            cx,
            root,
            window,
            &target,
            download_item,
            download_endpoints,
            &known_ids,
        )
        .await?;
        completed_ids.push(download_result.id.clone());
        require_actual_destination(&download_result, &download_target)?;
        assert_sha256(executor, &files, download_source, &upload_bytes).await?;
        assert_sha256(executor, &files, download_target, &upload_bytes).await?;
        functional::checkpoint(
            cx,
            window,
            capture,
            records,
            "sftp-transfer-downloaded",
            "production_queue_succeeded_and_local_sha256_matches_remote_source",
        )
        .await?;

        let overwrite_v1 = fixture_payload(run_id, "overwrite", "old");
        let overwrite_v2 = fixture_payload(run_id, "overwrite", "replacement");
        let overwrite_source = local_file_endpoint(local_root, &overwrite_name)?;
        let overwrite_target = remote_file_endpoint(host_id, remote_directory, &overwrite_name)?;
        let overwrite_seed =
            RightRailSftpTransferItem::new(&overwrite_name, &overwrite_name, true, false);
        let overwrite_conflict =
            RightRailSftpTransferItem::new(&overwrite_name, &overwrite_name, true, true);
        let overwrite_endpoints = TransferEndpoints::new(&overwrite_source, &overwrite_target);
        write_fixture_source(local_root, run_id, &overwrite_name, &overwrite_v1, false)?;
        let known_ids = transfer_ids(read_transfer_rows(cx, root, window, &target).await?);
        submit_transfer(cx, root, window, &target, overwrite_seed).await?;
        let seeded = wait_for_transfer(
            cx,
            root,
            window,
            &target,
            overwrite_seed,
            overwrite_endpoints,
            &known_ids,
        )
        .await?;
        completed_ids.push(seeded.id.clone());
        require_actual_destination(&seeded, &overwrite_target)?;
        assert_sha256(executor, &files, overwrite_target.clone(), &overwrite_v1).await?;
        let permission_files = files.clone();
        let permission_target = overwrite_target.clone();
        executor
            .run(move |cancellation| async move {
                permission_files
                    .chmod(cancellation, permission_target, 0o600)
                    .await
            })
            .await
            .map_err(|error| io::Error::other(error.to_string()))?;
        let permission_directory = FileEndpoint::remote(host_id.clone(), remote_directory)
            .map_err(|error| io::Error::other(format!("invalid fixture directory: {error:?}")))?;
        assert_remote_mode(
            executor,
            &files,
            &permission_directory,
            &overwrite_target,
            0o600,
        )
        .await?;

        write_fixture_source(local_root, run_id, &overwrite_name, &overwrite_v2, true)?;
        assert_sha256(executor, &files, overwrite_source.clone(), &overwrite_v2).await?;
        let known_ids = transfer_ids(read_transfer_rows(cx, root, window, &target).await?);
        submit_transfer(cx, root, window, &target, overwrite_conflict).await?;
        wait_for_conflict(cx, root, window, &target, overwrite_conflict).await?;
        functional::checkpoint(
            cx,
            window,
            capture,
            records,
            "sftp-transfer-cancel-dialog",
            "production_conflict_dialog_open_for_one_fixture_upload",
        )
        .await?;
        cancel_conflict(cx, root, window, &target, overwrite_conflict).await?;
        assert_no_new_transfer(cx, root, window, &target, overwrite_endpoints, &known_ids).await?;
        assert_sha256(executor, &files, overwrite_target.clone(), &overwrite_v1).await?;
        functional::checkpoint(
            cx,
            window,
            capture,
            records,
            "sftp-transfer-cancelled",
            "cancel_left_existing_remote_sha256_unchanged_and_enqueued_no_task",
        )
        .await?;

        let known_ids = transfer_ids(read_transfer_rows(cx, root, window, &target).await?);
        submit_transfer(cx, root, window, &target, overwrite_conflict).await?;
        wait_for_conflict(cx, root, window, &target, overwrite_conflict).await?;
        functional::checkpoint(
            cx,
            window,
            capture,
            records,
            "sftp-transfer-overwrite-dialog",
            "production_conflict_dialog_open_before_explicit_overwrite",
        )
        .await?;
        resolve_conflict(
            cx,
            root,
            window,
            &target,
            overwrite_conflict,
            ConflictPolicy::Overwrite,
        )
        .await?;
        let overwritten = wait_for_transfer(
            cx,
            root,
            window,
            &target,
            overwrite_conflict,
            overwrite_endpoints,
            &known_ids,
        )
        .await?;
        completed_ids.push(overwritten.id.clone());
        require_actual_destination(&overwritten, &overwrite_target)?;
        assert_sha256(executor, &files, overwrite_target.clone(), &overwrite_v2).await?;
        assert_remote_mode(
            executor,
            &files,
            &permission_directory,
            &overwrite_target,
            0o600,
        )
        .await?;
        functional::checkpoint(
            cx,
            window,
            capture,
            records,
            "sftp-transfer-overwritten",
            "overwrite_replaced_fixture_content_and_preserved_existing_0600_mode",
        )
        .await?;

        let rename_v1 = fixture_payload(run_id, "rename", "old");
        let rename_v2 = fixture_payload(run_id, "rename", "replacement");
        let rename_source = local_file_endpoint(local_root, &rename_name)?;
        let rename_target = remote_file_endpoint(host_id, remote_directory, &rename_name)?;
        let rename_seed = RightRailSftpTransferItem::new(&rename_name, &rename_name, true, false);
        let rename_conflict =
            RightRailSftpTransferItem::new(&rename_name, &rename_name, true, true);
        let rename_endpoints = TransferEndpoints::new(&rename_source, &rename_target);
        write_fixture_source(local_root, run_id, &rename_name, &rename_v1, false)?;
        let known_ids = transfer_ids(read_transfer_rows(cx, root, window, &target).await?);
        submit_transfer(cx, root, window, &target, rename_seed).await?;
        let seeded = wait_for_transfer(
            cx,
            root,
            window,
            &target,
            rename_seed,
            rename_endpoints,
            &known_ids,
        )
        .await?;
        completed_ids.push(seeded.id.clone());
        require_actual_destination(&seeded, &rename_target)?;
        assert_sha256(executor, &files, rename_target.clone(), &rename_v1).await?;

        write_fixture_source(local_root, run_id, &rename_name, &rename_v2, true)?;
        assert_sha256(executor, &files, rename_source.clone(), &rename_v2).await?;
        let known_ids = transfer_ids(read_transfer_rows(cx, root, window, &target).await?);
        submit_transfer(cx, root, window, &target, rename_conflict).await?;
        wait_for_conflict(cx, root, window, &target, rename_conflict).await?;
        functional::checkpoint(
            cx,
            window,
            capture,
            records,
            "sftp-transfer-rename-dialog",
            "production_conflict_dialog_open_before_explicit_rename",
        )
        .await?;
        resolve_conflict(
            cx,
            root,
            window,
            &target,
            rename_conflict,
            ConflictPolicy::Rename,
        )
        .await?;
        let renamed = wait_for_transfer(
            cx,
            root,
            window,
            &target,
            rename_conflict,
            rename_endpoints,
            &known_ids,
        )
        .await?;
        completed_ids.push(renamed.id.clone());
        let renamed_target = renamed
            .actual_destination
            .clone()
            .ok_or_else(|| io::Error::other("renamed fixture upload has no actual destination"))?;
        if renamed_target == rename_target
            || !remote_parent_is(&renamed_target, host_id, remote_directory)?
        {
            return Err(
                io::Error::other("rename did not stay beside its fixture conflict target").into(),
            );
        }
        assert_sha256(executor, &files, rename_target.clone(), &rename_v1).await?;
        assert_sha256(executor, &files, renamed_target, &rename_v2).await?;
        functional::checkpoint(
            cx,
            window,
            capture,
            records,
            "sftp-transfer-renamed",
            "rename_preserved_the_old_fixture_target_and_created_a_new_sha256_verified_target",
        )
        .await?;

        let editor_first_text = format!("right-rail-editor:{run_id}:first\n");
        let editor_second_text = format!("right-rail-editor:{run_id}:second\n");
        for (name, text) in [
            (&editor_first_name, &editor_first_text),
            (&editor_second_name, &editor_second_text),
        ] {
            write_fixture_source(local_root, run_id, name, text.as_bytes(), false)?;
            let source = local_file_endpoint(local_root, name)?;
            let destination = remote_file_endpoint(host_id, remote_directory, name)?;
            let item = RightRailSftpTransferItem::new(name, name, true, false);
            let known_ids = transfer_ids(read_transfer_rows(cx, root, window, &target).await?);
            submit_transfer(cx, root, window, &target, item).await?;
            let result = wait_for_transfer(
                cx,
                root,
                window,
                &target,
                item,
                TransferEndpoints::new(&source, &destination),
                &known_ids,
            )
            .await?;
            require_actual_destination(&result, &destination)?;
            assert_sha256(executor, &files, destination, text.as_bytes()).await?;
            completed_ids.push(result.id);
        }

        verify_transfer_history_controls(
            cx,
            root,
            window,
            capture,
            records,
            &target,
            &completed_ids,
        )
        .await?;
        let file_tabs_result = verify_workspace_file_tabs(
            cx,
            root,
            window,
            capture,
            records,
            &target,
            executor,
            &files,
            &name_prefix,
            [
                (&editor_first_name, &editor_first_text),
                (&editor_second_name, &editor_second_text),
            ],
        )
        .await;
        if file_tabs_result.is_err() {
            let _ = capture_file_tabs_diagnostic(
                cx,
                root,
                window,
                capture,
                host_id,
                remote_directory,
                [
                    (&editor_first_name, &editor_first_text),
                    (&editor_second_name, &editor_second_text),
                ],
                "sftp-file-tabs-failure",
            );
        }
        file_tabs_result?;
        for (name, text) in [
            (&editor_first_name, &editor_first_text),
            (&editor_second_name, &editor_second_text),
        ] {
            assert_sha256(
                executor,
                &files,
                remote_file_endpoint(host_id, remote_directory, name)?,
                text.as_bytes(),
            )
            .await?;
        }

        Ok(())
    }
    .await;
    let cleanup_result = cleanup_fixture_files(executor, &files, &target, &name_prefix).await;
    case_result?;
    cleanup_result?;
    Ok(())
}

/// 在实际窗口校验输入框与书签菜单同一行且互不遮挡，避免只依赖声明式布局配置。
///
/// @author kongweiguang
pub(super) fn assert_compact_path_controls(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    let _ = super::frame_signature(cx, window)?;
    window.update(cx, |_, window, _| {
        let path = window
            .debug_selector_bounds("rr-files-path-input-1")
            .ok_or_else(|| io::Error::other("Path input is not visible"))?;
        let bookmark = window
            .debug_selector_bounds("rr-files-bookmarks-1")
            .ok_or_else(|| io::Error::other("Bookmark control is not visible"))?;
        let viewport = window.visual_viewport_bounds();
        if path.size.width < px(20.)
            || bookmark.origin.x < path.origin.x + path.size.width
            || (path.center().y - bookmark.center().y).as_f32().abs() > 1.0
            || !bounds_inside_clip(path, viewport)
            || !bounds_inside_clip(bookmark, viewport)
        {
            return Err(io::Error::other(format!(
                "Path and bookmark controls must share one unclipped row: path={path:?}, bookmark={bookmark:?}, viewport={viewport:?}"
            )));
        }
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// 先在右栏真实滚动区显露上传行，再选中并确认未绘制正文框。
///
/// @author kongweiguang
async fn verify_selected_upload_without_preview_panel(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    host_id: &HostId,
    directory: &str,
    name: &str,
) -> Result<(), Box<dyn Error>> {
    let selector = format!("files-row-1-{name}");
    super::functional_transfer_workspace::reveal_file_row(
        cx,
        window,
        capture,
        "files-browser-list-1",
        &selector,
        "upload",
    )
    .await?;
    super::menus::click_selector(cx, window, &selector)?;
    functional::poll_root(
        cx,
        root,
        window,
        "selected upload without preview panel",
        |root, _, cx| {
            root.right_rail_acceptance_sftp_file_selected(host_id, directory, name, cx)
                .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    if window.update(cx, |_, window, _| {
        window.debug_selector_bounds("rr-files-preview").is_some()
    })? {
        return Err(io::Error::other("Selected upload rendered a preview box in the rail").into());
    }
    super::functional_transfer_workspace::reveal_file_row(
        cx,
        window,
        capture,
        "files-browser-list-1",
        &selector,
        "upload-menu",
    )
    .await?;
    super::menus::assert_file_context_menu(cx, window, &selector, None).await?;
    Ok(())
}

/// 记录真实活动标签、编辑器就绪、抽屉边界和当前帧；不通过磁盘快照推断窗口状态。
///
/// @author kongweiguang
#[allow(clippy::too_many_arguments)]
fn capture_file_tabs_diagnostic(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    host_id: &HostId,
    directory: &str,
    files: [(&str, &str); 2],
    stem: &str,
) -> Result<(), Box<dyn Error>> {
    let screenshot = super::save_frame(
        cx,
        window,
        capture.output_directory.join(format!("{stem}.png")),
    )?;
    let diagnostic = window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            let file_states = files.iter().map(|(name, text)| {
                let state = root.right_rail_acceptance_file_tab_state(host_id, directory, name, cx);
                let ready = root.right_rail_acceptance_file_ready(host_id, directory, name, text, cx);
                let editor_tab_bounds = state.as_ref().ok().and_then(|state| state.as_ref())
                    .and_then(|state| window.debug_selector_bounds(&format!("rr-file-editor-tab-{}", state.2)));
                serde_json::json!({
                    "name": name, "tabState": format!("{state:?}"), "ready": format!("{ready:?}"),
                    "editorTabBounds": format!("{editor_tab_bounds:?}"),
                })
            }).collect::<Vec<_>>();
            serde_json::json!({
                "files": file_states,
                "activeTool": format!("{:?}", root.right_rail_acceptance_active_tool()),
                "drawerBounds": format!("{:?}", window.debug_selector_bounds("workbench-compact-tool-drawer-surface")),
                "focus": format!("{:?}", window.focused(cx)),
                "contextStack": format!("{:?}", window.context_stack()),
                "screenshot": screenshot,
            })
        })
    })?;
    fs::write(
        capture.output_directory.join(format!("{stem}.json")),
        serde_json::to_vec_pretty(&diagnostic)?,
    )?;
    Ok(())
}

/// 在文件标签激活期间继续使用同一右栏浏览目标，核对连续打开、去重及关闭后继。
/// 真实打开入口校验普通文件和正文，并覆盖双向激活、取消脏关闭与文件版本闭环。
///
/// @author kongweiguang
#[allow(clippy::too_many_arguments)]
async fn verify_workspace_file_tabs(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    target: &RightRailSftpTransferTarget<'_>,
    executor: &ApplicationTaskExecutor,
    file_service: &FileService,
    name_prefix: &str,
    files: [(&str, &str); 2],
) -> Result<(), Box<dyn Error>> {
    let (host_id, directory, _) = target.fixture_binding();
    let fixture_tab =
        functional::poll_root(cx, root, window, "fixture terminal tab", |root, _, _| {
            root.right_rail_acceptance_fixture_tab_id(host_id).map(Some)
        })
        .await?;
    open_fixture_file(cx, root, window, host_id, directory, files[0].0).await?;
    let first = wait_fixture_file(cx, root, window, host_id, directory, files[0], false).await?;
    open_fixture_file(cx, root, window, host_id, directory, files[0].0).await?;
    let reopened = wait_fixture_file(cx, root, window, host_id, directory, files[0], false).await?;
    if first.0 != reopened.0 || reopened.1 != 1 {
        return Err(
            io::Error::other("Reopening the same file created a duplicate workspace tab").into(),
        );
    }
    open_fixture_file(cx, root, window, host_id, directory, files[1].0).await?;
    let second = wait_fixture_file(cx, root, window, host_id, directory, files[1], false).await?;
    if first.0 == second.0 {
        return Err(io::Error::other("Different fixture files share a workspace tab").into());
    }

    drag_workspace_tab_before(cx, window, &second.0, &first.0)?;
    functional::poll_root(
        cx,
        root,
        window,
        "reordered fixture file tabs",
        |root, _, _| {
            root.right_rail_acceptance_file_tabs_adjacent(
                host_id, directory, files[1].0, files[0].0,
            )
            .map(|adjacent| adjacent.then_some(()))
        },
    )
    .await?;
    wait_fixture_file(cx, root, window, host_id, directory, files[1], false).await?;

    if capture.requested_window_width == Some(800) {
        capture_file_tabs_diagnostic(
            cx,
            root,
            window,
            capture,
            host_id,
            directory,
            files,
            "sftp-editor-before-drawer-close",
        )?;
        super::functional_transfer_workspace::close_fixture_rail_for_narrow(cx, root, window)
            .await?;
    }
    super::menus::click_selector(cx, window, &format!("rr-file-editor-tab-{}", first.2))?;
    wait_fixture_file(cx, root, window, host_id, directory, files[0], false).await?;
    click_visible_workspace_tab(
        cx,
        window,
        capture,
        &second.0,
        false,
        "second-file-activate",
    )
    .await?;
    wait_fixture_file(cx, root, window, host_id, directory, files[1], false).await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-file-tabs",
        "continuous_sftp_browser_reopen_dedup_and_distinct_file_tab_activation",
    )
    .await?;

    let modified = format!("{}unsaved fixture change\n", files[1].1);
    functional::poll_root(
        cx,
        root,
        window,
        "edit fixture file buffer",
        |root, window, cx| {
            root.replace_right_rail_acceptance_file_text(
                host_id,
                directory,
                files[1].0,
                modified.clone(),
                window,
                cx,
            )
            .map(|changed| changed.then_some(()))
        },
    )
    .await?;
    wait_fixture_file(
        cx,
        root,
        window,
        host_id,
        directory,
        (files[1].0, &modified),
        true,
    )
    .await?;
    click_visible_workspace_tab(cx, window, capture, &second.0, true, "dirty-file").await?;
    functional::poll_root(
        cx,
        root,
        window,
        "dirty file close confirmation",
        |root, _, cx| {
            root.right_rail_acceptance_file_close_confirmation_ready(
                host_id, directory, files[1].0, cx,
            )
            .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-file-dirty-close",
        "dirty_file_close_prompts_before_any_write",
    )
    .await?;
    press_escape(cx, window)?;
    functional::poll_root(
        cx,
        root,
        window,
        "cancel dirty file close",
        |root, _, cx| {
            let confirmation = root.right_rail_acceptance_file_close_confirmation_ready(
                host_id, directory, files[1].0, cx,
            )?;
            let state =
                root.right_rail_acceptance_file_tab_state(host_id, directory, files[1].0, cx)?;
            Ok(state
                .filter(|state| {
                    !confirmation && state.0 == second.0 && state.1 == 1 && state.3 && state.4
                })
                .map(|_| ()))
        },
    )
    .await?;
    wait_fixture_file(
        cx,
        root,
        window,
        host_id,
        directory,
        (files[1].0, &modified),
        true,
    )
    .await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-file-close-cancelled",
        "cancel_preserved_dirty_buffer_and_workspace_tab",
    )
    .await?;

    verify_fixture_file_save_conflict(
        cx,
        root,
        window,
        capture,
        records,
        target,
        executor,
        file_service,
        name_prefix,
        (files[1].0, files[1].1, &modified),
    )
    .await?;
    wait_fixture_file(cx, root, window, host_id, directory, files[1], false).await?;
    click_visible_workspace_tab(cx, window, capture, &second.0, true, "saved-file").await?;
    wait_fixture_file_closed(cx, root, window, host_id, directory, files[1].0).await?;
    wait_fixture_file(cx, root, window, host_id, directory, files[0], false).await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-file-successor",
        "closing_reordered_file_selected_the_remaining_file_without_manual_activation",
    )
    .await?;

    restore_fixture_terminal(cx, root, window, host_id, directory, &fixture_tab).await?;
    let previous_transfer_tabs = window.update(cx, |_, _, app| {
        root.read(app)
            .right_rail_acceptance_sftp_workspace_tab_ids(host_id)
    })?;
    functional::poll_root(
        cx,
        root,
        window,
        "request fixture SFTP workspace",
        |root, _, cx| {
            root.request_right_rail_acceptance_sftp_workspace_tab(host_id, cx)
                .map(|requested| requested.then_some(()))
        },
    )
    .await?;
    let transfer_tab = functional::poll_root(
        cx,
        root,
        window,
        "fixture SFTP workspace ready",
        |root, _, _| {
            let new_tabs = root
                .right_rail_acceptance_sftp_workspace_tab_ids(host_id)
                .into_iter()
                .filter(|id| !previous_transfer_tabs.contains(id))
                .collect::<Vec<_>>();
            if new_tabs.len() > 1 {
                return Err("Fixture SFTP preflight created multiple workspace tabs".to_owned());
            }
            Ok(new_tabs
                .into_iter()
                .next()
                .filter(|id| root.right_rail_acceptance_sftp_workspace_tab_selected(host_id, id)))
        },
    )
    .await?;
    restore_fixture_terminal(cx, root, window, host_id, directory, &fixture_tab).await?;
    open_fixture_file(cx, root, window, host_id, directory, files[1].0).await?;
    let second_again =
        wait_fixture_file(cx, root, window, host_id, directory, files[1], false).await?;
    if capture.requested_window_width == Some(800) {
        super::functional_transfer_workspace::close_fixture_rail_for_narrow(cx, root, window)
            .await?;
    }
    click_visible_workspace_tab(cx, window, capture, &second_again.0, true, "reopened-file")
        .await?;
    wait_fixture_file_closed(cx, root, window, host_id, directory, files[1].0).await?;
    functional::poll_root(
        cx,
        root,
        window,
        "SFTP workspace selected after file close",
        |root, _, _| {
            Ok(root
                .right_rail_acceptance_sftp_workspace_tab_selected(host_id, &transfer_tab)
                .then_some(()))
        },
    )
    .await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-workspace-successor",
        "closing_file_selected_sftp_center_while_another_editor_remained_open",
    )
    .await?;
    click_visible_workspace_tab(cx, window, capture, &transfer_tab, true, "transfer").await?;
    wait_fixture_file(cx, root, window, host_id, directory, files[0], false).await?;
    click_visible_workspace_tab(cx, window, capture, &first.0, true, "first-file").await?;
    wait_fixture_file_closed(cx, root, window, host_id, directory, files[0].0).await?;
    functional::poll_root(
        cx,
        root,
        window,
        "fixture terminal selected after last file close",
        |root, _, _| {
            Ok(root
                .right_rail_acceptance_terminal_tab_selected(&fixture_tab)
                .then_some(()))
        },
    )
    .await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-terminal-successor",
        "closing_last_file_selected_original_terminal_without_manual_restore",
    )
    .await?;
    Ok(())
}

/// 通过真实按钮验证 CAS 保存、外部冲突和重载；仅允许写本轮专属 fixture 并恢复原文。
///
/// @author kongweiguang
#[allow(clippy::too_many_arguments)]
async fn verify_fixture_file_save_conflict(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    target: &RightRailSftpTransferTarget<'_>,
    executor: &ApplicationTaskExecutor,
    file_service: &FileService,
    name_prefix: &str,
    file: (&str, &str, &str),
) -> Result<(), Box<dyn Error>> {
    let (host_id, directory, _) = target.fixture_binding();
    if file.0 != format!("{name_prefix}editor-second") {
        return Err(io::Error::other("editor fixture is outside this acceptance run").into());
    }
    let endpoint = remote_file_endpoint(host_id, directory, file.0)?;
    if !remote_parent_is(&endpoint, host_id, directory)? {
        return Err(
            io::Error::other("editor fixture is outside the bound remote directory").into(),
        );
    }

    super::menus::click_selector(cx, window, "rr-file-editor-save")?;
    wait_fixture_file(
        cx,
        root,
        window,
        host_id,
        directory,
        (file.0, file.2),
        false,
    )
    .await?;
    assert_sha256(executor, file_service, endpoint.clone(), file.2.as_bytes()).await?;
    let saved_revision = read_fixture_revision(executor, file_service, &endpoint, file.2).await?;
    wait_fixture_revision(
        cx,
        root,
        window,
        host_id,
        directory,
        file.0,
        &saved_revision,
    )
    .await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-file-saved",
        "editor_save_confirmed_revision_and_remote_sha256",
    )
    .await?;

    let external = format!("{}external fixture update\n", file.1);
    let external_length = external.len() as u64;
    let write_endpoint = endpoint.clone();
    let write_content = external.clone();
    let write_service = file_service.clone();
    let write_result = executor
        .run(move |cancellation| async move {
            write_service
                .write_text_file(
                    cancellation,
                    WriteTextRequest {
                        endpoint: write_endpoint,
                        content: write_content,
                        strategy: WriteTextStrategy::CompareRevision(saved_revision),
                    },
                )
                .await
        })
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    if write_result.bytes_written != external_length {
        return Err(
            io::Error::other("external fixture write returned an incomplete receipt").into(),
        );
    }
    assert_sha256(
        executor,
        file_service,
        endpoint.clone(),
        external.as_bytes(),
    )
    .await?;

    let draft = format!("{}local fixture draft\n", file.1);
    replace_fixture_buffer(cx, root, window, host_id, directory, file.0, &draft).await?;
    super::menus::click_selector(cx, window, "rr-file-editor-save")?;
    wait_fixture_conflict(cx, root, window, host_id, directory, file.0, &draft, false).await?;
    assert_sha256(
        executor,
        file_service,
        endpoint.clone(),
        external.as_bytes(),
    )
    .await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-file-save-conflict",
        "stale_save_preserved_remote_bytes_and_dirty_editor_draft",
    )
    .await?;

    super::menus::click_selector(cx, window, "rr-file-editor-reload-conflict")?;
    wait_fixture_conflict(cx, root, window, host_id, directory, file.0, &draft, true).await?;
    press_escape(cx, window)?;
    wait_fixture_conflict(cx, root, window, host_id, directory, file.0, &draft, false).await?;
    assert_sha256(
        executor,
        file_service,
        endpoint.clone(),
        external.as_bytes(),
    )
    .await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-file-reload-cancelled",
        "cancelled_reload_preserved_the_conflict_and_local_draft",
    )
    .await?;

    super::menus::click_selector(cx, window, "rr-file-editor-reload-conflict")?;
    wait_fixture_conflict(cx, root, window, host_id, directory, file.0, &draft, true).await?;
    super::menus::click_selector(cx, window, "rr-file-editor-reload-confirm")?;
    wait_fixture_file(
        cx,
        root,
        window,
        host_id,
        directory,
        (file.0, &external),
        false,
    )
    .await?;
    let reloaded_revision =
        read_fixture_revision(executor, file_service, &endpoint, &external).await?;
    wait_fixture_revision(
        cx,
        root,
        window,
        host_id,
        directory,
        file.0,
        &reloaded_revision,
    )
    .await?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-file-reloaded",
        "confirmed_reload_adopted_remote_content_and_revision",
    )
    .await?;

    replace_fixture_buffer(cx, root, window, host_id, directory, file.0, file.1).await?;
    super::menus::click_selector(cx, window, "rr-file-editor-save")?;
    wait_fixture_file(
        cx,
        root,
        window,
        host_id,
        directory,
        (file.0, file.1),
        false,
    )
    .await?;
    assert_sha256(executor, file_service, endpoint.clone(), file.1.as_bytes()).await?;
    let restored_revision =
        read_fixture_revision(executor, file_service, &endpoint, file.1).await?;
    wait_fixture_revision(
        cx,
        root,
        window,
        host_id,
        directory,
        file.0,
        &restored_revision,
    )
    .await?;
    Ok(())
}

/// 修改只投递到活动 fixture 的编辑器，等待真实订阅发布脏状态后再执行下一动作。
///
/// @author kongweiguang
async fn replace_fixture_buffer(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    directory: &str,
    name: &str,
    text: &str,
) -> Result<(), Box<dyn Error>> {
    functional::poll_root(
        cx,
        root,
        window,
        "edit fixture file buffer",
        |root, window, cx| {
            root.replace_right_rail_acceptance_file_text(
                host_id,
                directory,
                name,
                text.to_owned(),
                window,
                cx,
            )
            .map(|changed| changed.then_some(()))
        },
    )
    .await?;
    wait_fixture_file(cx, root, window, host_id, directory, (name, text), true).await?;
    Ok(())
}

/// 读取完整且可写的精确远端文档，只有正文符合当前步骤才返回可用于比较的 revision。
///
/// @author kongweiguang
async fn read_fixture_revision(
    executor: &ApplicationTaskExecutor,
    files: &FileService,
    endpoint: &FileEndpoint,
    expected: &str,
) -> Result<FileRevision, Box<dyn Error>> {
    let service = files.clone();
    let read_endpoint = endpoint.clone();
    let document = executor
        .run(move |cancellation| async move {
            service
                .read_text_file(cancellation, read_endpoint, MAX_TEXT_FILE_READ_BYTES)
                .await
        })
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    if document.endpoint != *endpoint
        || document.readonly
        || document.truncated
        || document.content != expected
    {
        return Err(io::Error::other(
            "fixture text revision did not match a complete writable document",
        )
        .into());
    }
    Ok(document.revision)
}

/// 编辑器的已确认 revision 与远端读回必须一致，防止仅凭内容或脏标记判断保存完成。
///
/// @author kongweiguang
async fn wait_fixture_revision(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    directory: &str,
    name: &str,
    revision: &FileRevision,
) -> Result<(), Box<dyn Error>> {
    functional::poll_root(
        cx,
        root,
        window,
        "fixture editor revision",
        |root, _, cx| {
            root.right_rail_acceptance_file_revision_matches(host_id, directory, name, revision, cx)
                .map(|ready| ready.then_some(()))
        },
    )
    .await
}

/// 确认冲突、草稿与重载确认共同绑定同一活动 endpoint，取消动作不能丢失任何状态。
///
/// @author kongweiguang
#[allow(clippy::too_many_arguments)]
async fn wait_fixture_conflict(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    directory: &str,
    name: &str,
    draft: &str,
    reload_confirmation: bool,
) -> Result<(), Box<dyn Error>> {
    functional::poll_root(
        cx,
        root,
        window,
        "fixture editor conflict",
        |root, _, cx| {
            root.right_rail_acceptance_file_conflict(
                host_id,
                directory,
                name,
                draft,
                reload_confirmation,
                cx,
            )
            .map(|ready| ready.then_some(()))
        },
    )
    .await
}

/// 从真实标签命中区域拖到目标左半边；每步绘制新帧以使 GPUI 完成拖动状态迁移。
///
/// @author kongweiguang
fn drag_workspace_tab_before(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    source: &kerminal_domain::identity::TabId,
    target: &kerminal_domain::identity::TabId,
) -> Result<(), Box<dyn Error>> {
    let _ = super::frame_signature(cx, window)?;
    let (start, end) = window.update(cx, |_, window, _| {
        let source = window
            .debug_selector_bounds(&format!("workspace-tab-hitbox-{source}"))
            .ok_or_else(|| io::Error::other("Fixture source tab is not mounted"))?;
        let target = window
            .debug_selector_bounds(&format!("workspace-tab-hitbox-{target}"))
            .ok_or_else(|| io::Error::other("Fixture target tab is not mounted"))?;
        let viewport = window.visual_viewport_bounds();
        if !bounds_inside_clip(source, viewport) || !bounds_inside_clip(target, viewport) {
            return Err(io::Error::other("Fixture tabs are outside the viewport"));
        }
        Ok((
            source.center(),
            point(target.origin.x + px(12.), target.center().y),
        ))
    })??;
    window.update(cx, |_, window, app| {
        let _ = window.dispatch_event(
            MouseMoveEvent {
                position: start,
                ..MouseMoveEvent::default()
            }
            .to_platform_input(),
            app,
        );
        let _ = window.dispatch_event(
            MouseDownEvent {
                button: MouseButton::Left,
                position: start,
                click_count: 1,
                ..MouseDownEvent::default()
            }
            .to_platform_input(),
            app,
        );
    })?;
    let _ = super::frame_signature(cx, window)?;
    for position in [point(start.x - px(10.), start.y), end] {
        window.update(cx, |_, window, app| {
            let _ = window.dispatch_event(
                MouseMoveEvent {
                    position,
                    pressed_button: Some(MouseButton::Left),
                    ..MouseMoveEvent::default()
                }
                .to_platform_input(),
                app,
            );
        })?;
        let _ = super::frame_signature(cx, window)?;
    }
    window.update(cx, |_, window, app| {
        let _ = window.dispatch_event(
            MouseUpEvent {
                button: MouseButton::Left,
                position: end,
                click_count: 1,
                ..MouseUpEvent::default()
            }
            .to_platform_input(),
            app,
        );
    })?;
    let _ = super::frame_signature(cx, window)?;
    Ok(())
}

/// 尊重标签栏自身的裁切区域；活动的重开文件必须自动显露，其余标签可用真实横滚显露。
/// 几何和截图在输入前留证，避免把窗口内但滚动区域外的旧坐标当作命中。
///
/// @author kongweiguang
pub(super) async fn click_visible_workspace_tab(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    tab_id: &kerminal_domain::identity::TabId,
    close: bool,
    stage: &str,
) -> Result<(), Box<dyn Error>> {
    let control = if close { "close" } else { "hitbox" };
    let selector = format!("workspace-tab-{control}-{tab_id}");
    let tab_selector = format!("workspace-tab-hitbox-{tab_id}");
    let initial_frame = super::save_frame(
        cx,
        window,
        capture
            .output_directory
            .join(format!("sftp-{stage}-tab-before-reveal.png")),
    )?;
    let mut initial = None;
    let mut scroll_count = 0;
    for attempt in 0..48 {
        let _ = super::frame_signature(cx, window)?;
        let (button, tab, scroll, viewport) = window.update(cx, |_, window, _| {
            (
                window.debug_selector_bounds(&selector),
                window.debug_selector_bounds(&tab_selector),
                window.debug_selector_bounds("workspace-tabs-scroll-area"),
                window.visual_viewport_bounds(),
            )
        })?;
        let geometry = serde_json::json!({
            "targetId": tab_id.to_string(),
            "selector": selector,
            "buttonBounds": format!("{button:?}"),
            "tabBounds": format!("{tab:?}"),
            "scrollBounds": format!("{scroll:?}"),
            "windowBounds": format!("{viewport:?}"),
            "clickPoint": button.map(|bounds| format!("{:?}", bounds.center())),
        });
        if initial.is_none() {
            initial = Some(geometry.clone());
        }
        if attempt == 0
            && stage == "reopened-file"
            && !button.zip(scroll).is_some_and(|(button, scroll)| {
                bounds_inside_clip(button, scroll.intersect(&viewport))
            })
        {
            fs::write(
                capture
                    .output_directory
                    .join(format!("sftp-{stage}-tab-click.json")),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "initial": initial,
                    "manualScrollCount": 0,
                    "initialFrame": initial_frame,
                    "result": "active_close_clipped",
                }))?,
            )?;
            return Err(io::Error::other("活动文件标签关闭按钮未自动显露").into());
        }
        if let (Some(button), Some(scroll)) = (button, scroll) {
            let clip = scroll.intersect(&viewport);
            if bounds_inside_clip(button, clip) {
                fs::write(
                    capture
                        .output_directory
                        .join(format!("sftp-{stage}-tab-click.json")),
                    serde_json::to_vec_pretty(&serde_json::json!({
                        "initial": initial,
                        "beforeClick": geometry,
                        "manualScrollCount": scroll_count,
                        "initialFrame": initial_frame,
                    }))?,
                )?;
                return super::menus::click_selector(cx, window, &selector);
            }
            if attempt >= 3 {
                let delta = if button.origin.x < clip.origin.x {
                    120.
                } else {
                    -120.
                };
                window.update(cx, |_, window, app| {
                    let _ = window.dispatch_event(
                        ScrollWheelEvent {
                            position: clip.center(),
                            delta: ScrollDelta::Pixels(point(px(delta), px(0.))),
                            touch_phase: TouchPhase::Moved,
                            ..ScrollWheelEvent::default()
                        }
                        .to_platform_input(),
                        app,
                    );
                })?;
                scroll_count += 1;
            }
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
    Err(io::Error::other(format!("标签控件未进入滚动区域: {tab_id} {control}")).into())
}

/// 窄窗打开编辑器后抽屉会收起；每次经真实按钮恢复后才从已绑定列表发起打开。
///
/// @author kongweiguang
async fn open_fixture_file(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    directory: &str,
    name: &str,
) -> Result<(), Box<dyn Error>> {
    ensure_fixture_rail_open(cx, root, window).await?;
    functional::poll_root(cx, root, window, "open fixture file", |root, _, cx| {
        root.open_right_rail_acceptance_sftp_file(host_id, directory, name, cx)
            .map(|opened| opened.then_some(()))
    })
    .await
}

/// 等待同一 endpoint 的唯一顶栏和活动编辑缓冲同时稳定，避免异步加载中的假阳性。
///
/// @author kongweiguang
async fn wait_fixture_file(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    directory: &str,
    file: (&str, &str),
    dirty: bool,
) -> Result<(kerminal_domain::identity::TabId, usize, usize, bool, bool), Box<dyn Error>> {
    functional::poll_root(
        cx,
        root,
        window,
        "fixture file tab and editor",
        |root, _, cx| {
            let Some(state) =
                root.right_rail_acceptance_file_tab_state(host_id, directory, file.0, cx)?
            else {
                return Ok(None);
            };
            if state.1 != 1 {
                return Err("Fixture endpoint does not have exactly one workspace tab".to_owned());
            }
            if !state.3
                || state.4 != dirty
                || !root.right_rail_acceptance_file_ready(host_id, directory, file.0, file.1, cx)?
            {
                return Ok(None);
            }
            Ok(Some(state))
        },
    )
    .await
}

/// 关闭后检查快照与编辑投影均不再含该 fixture endpoint。
///
/// @author kongweiguang
async fn wait_fixture_file_closed(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    directory: &str,
    name: &str,
) -> Result<(), Box<dyn Error>> {
    functional::poll_root(
        cx,
        root,
        window,
        "closed fixture file tab",
        |root, _, cx| {
            root.right_rail_acceptance_file_closed(host_id, directory, name, cx)
                .map(|closed| closed.then_some(()))
        },
    )
    .await
}

/// 回到原 fixture 终端并重新核对当前文件面板，避免后续测试沿用失效目标。
///
/// @author kongweiguang
async fn restore_fixture_terminal(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
    directory: &str,
    tab_id: &kerminal_domain::identity::TabId,
) -> Result<(), Box<dyn Error>> {
    functional::poll_root(
        cx,
        root,
        window,
        "restore fixture terminal",
        |root, window, cx| {
            Ok(root
                .restore_right_rail_acceptance_fixture_tab(tab_id, window, cx)
                .then_some(()))
        },
    )
    .await?;
    ensure_fixture_rail_open(cx, root, window).await?;
    functional::poll_root(
        cx,
        root,
        window,
        "restore fixture file panel",
        |root, _, cx| {
            root.navigate_right_rail_acceptance_sftp_directory(host_id, directory, cx)?;
            root.right_rail_acceptance_sftp_directory_ready(host_id, directory, cx)
                .map(|ready| ready.then_some(()))
        },
    )
    .await
}

/// 已关闭的窄窗抽屉通过真实右栏按钮重新打开；等待活动工具后才允许导航原 fixture。
///
/// @author kongweiguang
async fn ensure_fixture_rail_open(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    let active = window.update(cx, |_, _, app| {
        root.read(app).right_rail_acceptance_active_tool()
    })?;
    match active {
        Some(ToolRailToolId::Sftp) => return Ok(()),
        Some(_) => return Err(io::Error::other("恢复 fixture 时出现其他右栏工具").into()),
        None => {}
    }
    let _ = super::frame_signature(cx, window)?;
    super::functional_transfer_workspace::click_visible_element(cx, window, "workbench-tool-sftp")?;
    functional::poll_root(
        cx,
        root,
        window,
        "重新打开右栏文件",
        |root, _, _| {
            Ok(
                (root.right_rail_acceptance_active_tool() == Some(ToolRailToolId::Sftp))
                    .then_some(()),
            )
        },
    )
    .await
}

/// 通过原生窗口按键取消最上层确认，并在读回状态前绘制新帧。
///
/// @author kongweiguang
fn press_escape(cx: &mut AsyncApp, window: AnyWindowHandle) -> Result<(), Box<dyn Error>> {
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
    let _ = super::frame_signature(cx, window)?;
    Ok(())
}

/// 验证标题完整控制队列显隐；仅在队列全由本次隔离 fixture 创建且均完成时清理。
///
/// @author kongweiguang
async fn verify_transfer_history_controls(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    target: &RightRailSftpTransferTarget<'_>,
    completed_ids: &[TransferId],
) -> Result<(), Box<dyn Error>> {
    let rows = functional::poll_root(
        cx,
        root,
        window,
        "completed fixture transfer queue",
        |root, _, cx| {
            let Some(rows) = root.right_rail_acceptance_sftp_transfer_rows(target, cx)? else {
                return Ok(None);
            };
            if rows.iter().any(|row| !completed_ids.contains(&row.id)) {
                return Err(
                    "The transfer queue contains a task outside this fixture run".to_owned(),
                );
            }
            if rows.len() != completed_ids.len()
                || rows
                    .iter()
                    .any(|row| row.status != TransferStatus::Succeeded || row.cleanup_pending)
            {
                return Ok(None);
            }
            Ok(Some(rows))
        },
    )
    .await?;
    let expected = completed_ids.iter().rev().collect::<Vec<_>>();
    if rows.iter().map(|row| &row.id).collect::<Vec<_>>() != expected || rows.len() <= 3 {
        return Err(
            io::Error::other("The transfer queue is not ordered by newest fixture task").into(),
        );
    }
    let expanded = functional::poll_root(
        cx,
        root,
        window,
        "transfer history initial state",
        |root, _, cx| root.right_rail_acceptance_sftp_transfer_history_expanded(target, cx),
    )
    .await?;
    if !expanded {
        return Err(io::Error::other("The transfer history did not start expanded").into());
    }
    let fourth_selector = format!("rr-files-transfer-{}", rows[3].id);
    scroll_queue_to_selector(
        cx,
        window,
        capture,
        &fourth_selector,
        "transfer-history-expanded",
    )
    .await?;
    assert_transfer_percentage_visible(cx, window, &rows[3].id)?;
    assert_archive_control_absent(cx, window)?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-transfer-history-expanded",
        "older_transfer_row_reachable_in_default_expanded_history",
    )
    .await?;

    super::menus::click_selector(cx, window, "rr-files-transfer-history")?;
    functional::poll_root(
        cx,
        root,
        window,
        "transfer history collapsed",
        |root, _, cx| {
            Ok(root
                .right_rail_acceptance_sftp_transfer_history_expanded(target, cx)?
                .filter(|expanded| !*expanded))
        },
    )
    .await?;
    assert_collapsed_transfer_rows(cx, window, &rows)?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-transfer-history-collapsed",
        "whole_transfer_list_unmounted_after_header_click",
    )
    .await?;

    super::menus::click_selector(cx, window, "rr-files-transfer-history")?;
    functional::poll_root(
        cx,
        root,
        window,
        "transfer history reopened",
        |root, _, cx| {
            Ok(root
                .right_rail_acceptance_sftp_transfer_history_expanded(target, cx)?
                .filter(|expanded| *expanded))
        },
    )
    .await?;
    scroll_queue_to_selector(
        cx,
        window,
        capture,
        &fourth_selector,
        "transfer-history-reopened",
    )
    .await?;
    super::menus::click_selector(cx, window, "rr-files-transfer-clear")?;
    functional::poll_root(
        cx,
        root,
        window,
        "cleared fixture transfer queue",
        |root, _, cx| {
            Ok(root
                .right_rail_acceptance_sftp_transfer_rows(target, cx)?
                .filter(Vec::is_empty))
        },
    )
    .await?;
    let expanded_after_clear = functional::poll_root(
        cx,
        root,
        window,
        "transfer history after clearing",
        |root, _, cx| root.right_rail_acceptance_sftp_transfer_history_expanded(target, cx),
    )
    .await?;
    if !expanded_after_clear {
        return Err(io::Error::other("Clear button also toggled transfer history").into());
    }
    assert_archive_control_absent(cx, window)?;
    functional::checkpoint(
        cx,
        window,
        capture,
        records,
        "sftp-transfer-history-cleared",
        "fixture_finished_queue_cleared_after_real_button_click",
    )
    .await?;
    Ok(())
}

/// 检查真实绘制元素的身份，确保归档证据入口已移除，而不是被压缩或滚动裁剪。
///
/// @author kongweiguang
fn assert_archive_control_absent(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    let _ = super::frame_signature(cx, window)?;
    let has_archive = window.update(cx, |_, window, _| {
        let archive = gpui_kit::ElementId::from("transfer-archive-toggle");
        gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .any(|element| element.path().contains(&archive))
    })?;
    if has_archive {
        return Err(io::Error::other("Transfer archive entry is still mounted").into());
    }
    Ok(())
}

/// 折叠后列表和各任务均不应挂载，避免用裁剪或减少行数冒充关闭。
///
/// @author kongweiguang
fn assert_collapsed_transfer_rows(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    rows: &[TransferSummary],
) -> Result<(), Box<dyn Error>> {
    let _ = super::frame_signature(cx, window)?;
    window.update(cx, |_, window, _| {
        if window
            .debug_selector_bounds("rr-files-transfer-list")
            .is_some()
        {
            return Err(io::Error::other("Collapsed transfer list is still mounted"));
        }
        for row in rows {
            let selector = format!("rr-files-transfer-{}", row.id);
            if window.debug_selector_bounds(&selector).is_some() {
                return Err(io::Error::other("Collapsed history mounted a transfer row"));
            }
        }
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// 百分比要在任务行和滚动视口内完整绘制；真实数值仍由传输摘要和投影测试负责。
///
/// @author kongweiguang
fn assert_transfer_percentage_visible(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    id: &TransferId,
) -> Result<(), Box<dyn Error>> {
    let _ = super::frame_signature(cx, window)?;
    window.update(cx, |_, window, _| {
        let list = window
            .debug_selector_bounds("rr-files-transfer-list")
            .ok_or_else(|| io::Error::other("Transfer list is not mounted"))?;
        let row = window
            .debug_selector_bounds(&format!("rr-files-transfer-{id}"))
            .ok_or_else(|| io::Error::other("Transfer row is not mounted"))?;
        let percentage = window
            .debug_selector_bounds(&format!("rr-files-transfer-percent-{id}"))
            .ok_or_else(|| io::Error::other("Transfer percentage is not mounted"))?;
        if !bounds_inside_clip(percentage, row.intersect(&list)) {
            return Err(io::Error::other(format!(
                "Transfer percentage is clipped: percentage={percentage:?}, row={row:?}, list={list:?}"
            )));
        }
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// 复用逐帧几何驱动的真实滚轮扫描，避免窄队列的固定步长跨过整行并保留诊断证据。
///
/// @author kongweiguang
async fn scroll_queue_to_selector(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    selector: &str,
    stage: &str,
) -> Result<(), Box<dyn Error>> {
    super::functional_transfer_workspace::reveal_file_row(
        cx,
        window,
        capture,
        "rr-files-transfer-list",
        selector,
        stage,
    )
    .await
}

/// 容忍设备像素舍入，同时要求整行处于滚动区和窗口的共同可视范围。
///
/// @author kongweiguang
fn bounds_inside_clip(bounds: Bounds<Pixels>, clip: Bounds<Pixels>) -> bool {
    let epsilon = px(0.75);
    !bounds.is_empty()
        && !clip.is_empty()
        && bounds.origin.x >= clip.origin.x - epsilon
        && bounds.origin.y >= clip.origin.y - epsilon
        && bounds.origin.x + bounds.size.width <= clip.origin.x + clip.size.width + epsilon
        && bounds.origin.y + bounds.size.height <= clip.origin.y + clip.size.height + epsilon
}

/// Return the fixture file service only after the real SFTP and local panels both commit their requested directories.
///
/// @author kongweiguang
async fn read_file_service(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    target: &RightRailSftpTransferTarget<'_>,
) -> Result<kerminal_application::files::FileService, Box<dyn Error>> {
    functional::poll_root(cx, root, window, "fixture file service", |root, _, cx| {
        root.right_rail_acceptance_sftp_file_service(target, cx)
    })
    .await
}

/// Return queue snapshots through the mounted production pane, not by creating a second transfer service.
///
/// @author kongweiguang
async fn read_transfer_rows(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    target: &RightRailSftpTransferTarget<'_>,
) -> Result<Vec<TransferSummary>, Box<dyn Error>> {
    functional::poll_root(
        cx,
        root,
        window,
        "fixture transfer queue snapshot",
        |root, _, cx| root.right_rail_acceptance_sftp_transfer_rows(target, cx),
    )
    .await
}

/// Submit one safe basename and wait for the production pane to accept the frozen transfer plan.
///
/// @author kongweiguang
async fn submit_transfer(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    target: &RightRailSftpTransferTarget<'_>,
    item: RightRailSftpTransferItem<'_>,
) -> Result<(), Box<dyn Error>> {
    functional::poll_root(
        cx,
        root,
        window,
        "submit fixture transfer",
        |root, _, cx| {
            root.submit_right_rail_acceptance_sftp_transfer(target, &item, cx)
                .map(|accepted| accepted.then_some(()))
        },
    )
    .await
}

/// Wait for the production single-item conflict projection before asking it to cancel or resolve the batch.
///
/// @author kongweiguang
async fn wait_for_conflict(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    target: &RightRailSftpTransferTarget<'_>,
    item: RightRailSftpTransferItem<'_>,
) -> Result<(), Box<dyn Error>> {
    functional::poll_root(
        cx,
        root,
        window,
        "fixture transfer conflict dialog",
        |root, _, cx| {
            root.right_rail_acceptance_sftp_transfer_conflict_ready(target, &item, cx)
                .map(|ready| ready.then_some(()))
        },
    )
    .await
}

/// 先核对精确 fixture 冲突，再点击生产取消按钮，真实事件不能被 owner 方法调用代替。
///
/// @author kongweiguang
async fn cancel_conflict(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    target: &RightRailSftpTransferTarget<'_>,
    item: RightRailSftpTransferItem<'_>,
) -> Result<(), Box<dyn Error>> {
    wait_for_conflict(cx, root, window, target, item).await?;
    super::functional_transfer_workspace::click_visible_element(
        cx,
        window,
        "files-transfer-conflict-cancel",
    )
}

/// 精确冲突计划就绪后点击对应策略；后续队列回读证明受理和结果，按钮回调自身也必须可靠。
///
/// @author kongweiguang
async fn resolve_conflict(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    target: &RightRailSftpTransferTarget<'_>,
    item: RightRailSftpTransferItem<'_>,
    policy: ConflictPolicy,
) -> Result<(), Box<dyn Error>> {
    wait_for_conflict(cx, root, window, target, item).await?;
    let selector = match policy {
        ConflictPolicy::Overwrite => "files-transfer-conflict-overwrite",
        ConflictPolicy::Rename => "files-transfer-conflict-rename",
        ConflictPolicy::Skip => "files-transfer-conflict-skip",
    };
    super::functional_transfer_workspace::click_visible_element(cx, window, selector)
}

/// Wait for a new queue ID with exact endpoints and require the worker's terminal result to be a committed file.
///
/// @author kongweiguang
async fn wait_for_transfer(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    target: &RightRailSftpTransferTarget<'_>,
    item: RightRailSftpTransferItem<'_>,
    endpoints: TransferEndpoints<'_>,
    known_ids: &[TransferId],
) -> Result<TransferSummary, Box<dyn Error>> {
    functional::poll_root(
        cx,
        root,
        window,
        "fixture transfer terminal result",
        |root, _, cx| {
            let Some(rows) = root.right_rail_acceptance_sftp_transfer_rows(target, cx)? else {
                return Ok(None);
            };
            if root.right_rail_acceptance_sftp_transfer_conflict_ready(target, &item, cx)? {
                return Err("The transfer unexpectedly paused for a conflict decision".to_owned());
            }
            let mut matching = rows.into_iter().filter(|summary| {
                !known_ids.contains(&summary.id)
                    && summary.request.source == *endpoints.source
                    && summary.request.destination == *endpoints.destination
                    && summary.request.kind == TransferKind::File
            });
            let Some(summary) = matching.next() else {
                return Ok(None);
            };
            if matching.next().is_some() {
                return Err("Multiple new queue items match one fixture transfer".to_owned());
            }
            match summary.status {
                TransferStatus::Succeeded => {
                    if summary.commit_summary.committed_entries() != 1 {
                        return Err(
                            "The fixture transfer completed without one committed file".to_owned()
                        );
                    }
                    Ok(Some(summary))
                }
                TransferStatus::Skipped | TransferStatus::Unchanged => {
                    Err("The fixture transfer did not commit its expected file".to_owned())
                }
                TransferStatus::Failed | TransferStatus::Cancelled => Err(summary
                    .safe_error
                    .unwrap_or_else(|| "The fixture transfer failed".to_owned())),
                TransferStatus::Queued
                | TransferStatus::Connecting
                | TransferStatus::Transferring
                | TransferStatus::WaitingForRecovery
                | TransferStatus::Committing
                | TransferStatus::Cancelling => Ok(None),
            }
        },
    )
    .await
}

/// Confirm conflict cancellation did not create a queue record for the exact source/destination pair.
///
/// @author kongweiguang
async fn assert_no_new_transfer(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    target: &RightRailSftpTransferTarget<'_>,
    endpoints: TransferEndpoints<'_>,
    known_ids: &[TransferId],
) -> Result<(), Box<dyn Error>> {
    let rows = read_transfer_rows(cx, root, window, target).await?;
    if rows.iter().any(|summary| {
        !known_ids.contains(&summary.id)
            && summary.request.source == *endpoints.source
            && summary.request.destination == *endpoints.destination
    }) {
        return Err(io::Error::other("canceled fixture transfer was unexpectedly enqueued").into());
    }
    Ok(())
}

/// 在共享执行器回读真实目录属性，防止覆盖权限策略虽有单测却未接入实际远端发布路径。
///
/// @author kongweiguang
async fn assert_remote_mode(
    executor: &ApplicationTaskExecutor,
    files: &FileService,
    directory: &FileEndpoint,
    endpoint: &FileEndpoint,
    expected: u32,
) -> Result<(), Box<dyn Error>> {
    let files = files.clone();
    let directory = directory.clone();
    let listing = executor
        .run(move |cancellation| async move { files.list_directory(cancellation, directory).await })
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    let actual = listing
        .entries
        .iter()
        .find(|entry| entry.endpoint == *endpoint)
        .and_then(|entry| entry.permissions)
        .map(|mode| mode & 0o7777);
    if actual != Some(expected) {
        return Err(io::Error::other(format!(
            "fixture permissions changed: expected {expected:04o}, actual {actual:?}"
        ))
        .into());
    }
    Ok(())
}

/// Read a complete bounded file on the shared Tokio executor, then compare its SHA-256 with fixture bytes.
///
/// @author kongweiguang
async fn assert_sha256(
    executor: &ApplicationTaskExecutor,
    files: &kerminal_application::files::FileService,
    endpoint: FileEndpoint,
    expected: &[u8],
) -> Result<(), Box<dyn Error>> {
    let files = files.clone();
    let preview_endpoint = endpoint.clone();
    let preview = executor
        .run(move |cancellation| async move {
            files
                .preview_file(cancellation, preview_endpoint, MAX_FILE_PREVIEW_BYTES)
                .await
        })
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    if preview.endpoint != endpoint
        || preview.truncated
        || preview
            .total_size
            .is_some_and(|total| total != preview.bytes.len() as u64)
    {
        return Err(
            io::Error::other("fixture SHA readback was not a complete endpoint snapshot").into(),
        );
    }
    let actual_sha256 = sha256(&preview.bytes);
    let expected_sha256 = sha256(expected);
    if actual_sha256 != expected_sha256 {
        return Err(io::Error::other(format!(
            "fixture SHA-256 mismatch for {}",
            endpoint_label(&endpoint)
        ))
        .into());
    }
    Ok(())
}

/// Ensure the actual destination reported by the queue is the explicitly chosen fixture endpoint.
///
/// @author kongweiguang
fn require_actual_destination(
    summary: &TransferSummary,
    expected: &FileEndpoint,
) -> Result<(), Box<dyn Error>> {
    if summary.actual_destination.as_ref() != Some(expected) {
        return Err(
            io::Error::other("fixture transfer committed to an unexpected endpoint").into(),
        );
    }
    Ok(())
}

/// Hash a known fixture byte slice using the same SHA-256 representation recorded for readback comparisons.
///
/// @author kongweiguang
fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Generate small binary payloads with run-specific content so each conflict assertion has distinct bytes.
///
/// @author kongweiguang
fn fixture_payload(run_id: &str, name: &str, revision: &str) -> Vec<u8> {
    format!("kerminal-right-rail:{run_id}:{name}:{revision}\0fixture").into_bytes()
}

/// Create a new local source exclusively or replace only the regular file already created by this run.
///
/// @author kongweiguang
fn write_fixture_source(
    local_root: &Path,
    run_id: &str,
    name: &str,
    bytes: &[u8],
    replace: bool,
) -> Result<(), Box<dyn Error>> {
    let path = local_fixture_file_path(local_root, run_id, name)?;
    if replace {
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fixture source replacement is not a regular file",
            )
            .into());
        }
        fs::write(path, bytes)?;
    } else {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    Ok(())
}

/// Join a validated run-prefixed name directly below localFilesRoot, rejecting traversal and alternate namespaces.
///
/// @author kongweiguang
fn local_fixture_file_path(
    local_root: &Path,
    run_id: &str,
    name: &str,
) -> Result<std::path::PathBuf, io::Error> {
    let run_prefix = format!("rr-ui-{run_id}-");
    if !name.starts_with(&run_prefix)
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fixture file name is outside this acceptance run",
        ));
    }
    let path = local_root.join(name);
    if path.parent() != Some(local_root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fixture file must be a direct child of localFilesRoot",
        ));
    }
    Ok(path)
}

/// Build a local endpoint below the manifest root without opening a native save chooser.
///
/// @author kongweiguang
fn local_file_endpoint(local_root: &Path, name: &str) -> Result<FileEndpoint, Box<dyn Error>> {
    Ok(FileEndpoint::Local {
        path: local_root.join(name),
    })
}

/// Join a single filename to the parsed remote fixture directory so POSIX path rules remain authoritative.
///
/// @author kongweiguang
fn remote_file_endpoint(
    host_id: &HostId,
    remote_directory: &str,
    name: &str,
) -> Result<FileEndpoint, Box<dyn Error>> {
    let directory = FileEndpoint::remote(host_id.clone(), remote_directory)
        .map_err(|error| io::Error::other(format!("{error:?}")))?;
    let path = directory
        .remote_path()
        .ok_or_else(|| io::Error::other("fixture directory is not a remote endpoint"))?
        .join_name(name)
        .map_err(|error| io::Error::other(format!("{error:?}")))?;
    Ok(FileEndpoint::remote(host_id.clone(), path.as_str())
        .map_err(|error| io::Error::other(format!("{error:?}")))?)
}

/// Compare request identities without treating the source task's prior seed upload as a new conflict result.
///
/// @author kongweiguang
fn transfer_ids(rows: Vec<TransferSummary>) -> Vec<TransferId> {
    rows.into_iter().map(|summary| summary.id).collect()
}

/// Verify a rename endpoint remains on the fixture host and inside the exact remote fixture directory.
///
/// @author kongweiguang
fn remote_parent_is(
    endpoint: &FileEndpoint,
    host_id: &HostId,
    remote_directory: &str,
) -> Result<bool, Box<dyn Error>> {
    let FileEndpoint::Remote {
        host_id: actual_host,
        path: actual_path,
    } = endpoint
    else {
        return Ok(false);
    };
    let directory = FileEndpoint::remote(host_id.clone(), remote_directory)
        .map_err(|error| io::Error::other(format!("{error:?}")))?;
    Ok(actual_host == host_id && actual_path.parent().as_ref() == directory.remote_path())
}

/// Remove only regular files in this run's namespace through the shared Tokio executor.
///
/// @author kongweiguang
async fn cleanup_fixture_files(
    executor: &ApplicationTaskExecutor,
    files: &kerminal_application::files::FileService,
    target: &RightRailSftpTransferTarget<'_>,
    name_prefix: &str,
) -> Result<(), Box<dyn Error>> {
    let (host_id, remote_directory, local_root) = target.fixture_binding();
    let directories = [
        FileEndpoint::remote(host_id.clone(), remote_directory)
            .map_err(|error| io::Error::other(format!("{error:?}")))?,
        FileEndpoint::Local {
            path: local_root.to_path_buf(),
        },
    ];
    let files = files.clone();
    let name_prefix = name_prefix.to_owned();
    executor
        .run(move |cancellation| async move {
            for directory in directories {
                let listing = files
                    .list_directory(cancellation.clone(), directory)
                    .await?;
                for entry in listing.entries.into_iter().filter(|entry| {
                    entry.name.starts_with(&name_prefix) && entry.kind == FileEntryKind::File
                }) {
                    files
                        .delete(cancellation.clone(), entry.endpoint, DeleteTargetKind::File)
                        .await?;
                }
            }
            Ok(())
        })
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(())
}

/// Resolve a transfer endpoint's user-visible basename for conflict readiness without changing its path.
///
/// @author kongweiguang
fn endpoint_name(endpoint: &FileEndpoint) -> Option<&str> {
    match endpoint {
        FileEndpoint::Local { path } => path.file_name()?.to_str(),
        FileEndpoint::Remote { path, .. } | FileEndpoint::Container { path, .. } => {
            path.as_str().rsplit('/').next()
        }
    }
}

/// Produce a bounded label for fixture mismatch diagnostics without serializing credentials or host metadata.
///
/// @author kongweiguang
fn endpoint_label(endpoint: &FileEndpoint) -> String {
    endpoint_name(endpoint)
        .unwrap_or("fixture endpoint")
        .to_owned()
}
