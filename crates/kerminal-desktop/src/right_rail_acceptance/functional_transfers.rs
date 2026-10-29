//! Real upload, download, and conflict acceptance against only the manifest fixture directories.
//!
//! @author kongweiguang

use std::{error::Error, fs, io, io::Write, path::Path};

use gpui_kit::{AnyWindowHandle, AsyncApp, Entity};
use kerminal_application::{
    ApplicationTaskExecutor, files::MAX_FILE_PREVIEW_BYTES, preferences::settings::ToolRailToolId,
};
use kerminal_domain::{
    files::{
        ConflictPolicy, DeleteTargetKind, FileEndpoint, FileEntryKind, TransferKind,
        TransferStatus, TransferSummary,
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

/// Drive transfers through production queue handlers; FileService I/O uses the shared Tokio executor and cleanup runs on both outcomes.
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

    let case_result: Result<(), Box<dyn Error>> = async {
        let upload_name = format!("{name_prefix}upload");
        let download_name = format!("{name_prefix}download");
        let overwrite_name = format!("{name_prefix}overwrite");
        let rename_name = format!("{name_prefix}rename");

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
        require_actual_destination(&upload_result, &upload_target)?;
        assert_sha256(executor, &files, upload_source, &upload_bytes).await?;
        assert_sha256(executor, &files, upload_target, &upload_bytes).await?;
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
        require_actual_destination(&seeded, &overwrite_target)?;
        assert_sha256(executor, &files, overwrite_target.clone(), &overwrite_v1).await?;

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
        require_actual_destination(&overwritten, &overwrite_target)?;
        assert_sha256(executor, &files, overwrite_target.clone(), &overwrite_v2).await?;
        functional::checkpoint(
            cx,
            window,
            capture,
            records,
            "sftp-transfer-overwritten",
            "overwrite_changed_only_the_current_run_remote_fixture_file",
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

        Ok(())
    }
    .await;
    let cleanup_result = cleanup_fixture_files(executor, &files, &target, &name_prefix).await;
    case_result?;
    cleanup_result?;
    Ok(())
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

/// Cancel only the conflict request whose exact fixture endpoints were verified by the modal readiness probe.
///
/// @author kongweiguang
async fn cancel_conflict(
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
        "cancel fixture transfer conflict",
        |root, _, cx| {
            root.cancel_right_rail_acceptance_sftp_transfer_conflict(target, &item, cx)
                .map(|cancelled| cancelled.then_some(()))
        },
    )
    .await
}

/// Apply the selected policy only after the same production conflict plan is still current.
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
    functional::poll_root(
        cx,
        root,
        window,
        "resolve fixture transfer conflict",
        |root, _, cx| {
            root.resolve_right_rail_acceptance_sftp_transfer_conflict(target, &item, policy, cx)
                .map(|resolved| resolved.then_some(()))
        },
    )
    .await
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
