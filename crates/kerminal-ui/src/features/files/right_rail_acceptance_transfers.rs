//! Fixture-scoped transfer actions routed through the production batch and conflict lifecycle.
//!
//! @author kongweiguang

use std::path::Path;

use gpui_kit::Context;
use kerminal_application::files::FileService;
use kerminal_domain::{files::*, identity::HostId};

use super::super::{
    BatchEnqueueLabels, TransferBatchPlan, TransferEnqueueItem, endpoint_matches_locked_host,
};
use super::{FilesPane, acceptance_directory_endpoint, validate_acceptance_name};

/// Immutable binding for the isolated SFTP and local fixture directories.
///
/// @author kongweiguang
#[derive(Clone, Copy)]
pub struct RightRailSftpTransferTarget<'a> {
    host_id: &'a HostId,
    remote_directory: &'a str,
    local_root: &'a Path,
}

impl<'a> RightRailSftpTransferTarget<'a> {
    /// Capture one fixture binding without exposing mutable public fields across the UI boundary.
    ///
    /// @author kongweiguang
    pub fn new(host_id: &'a HostId, remote_directory: &'a str, local_root: &'a Path) -> Self {
        Self {
            host_id,
            remote_directory,
            local_root,
        }
    }

    /// Read the exact host and directory binding used by all transfer actions in this fixture.
    ///
    /// @author kongweiguang
    pub fn fixture_binding(&self) -> (&'a HostId, &'a str, &'a Path) {
        (self.host_id, self.remote_directory, self.local_root)
    }
}

/// Immutable one-file intent, including the expected remote conflict state for submission preflight.
///
/// @author kongweiguang
#[derive(Clone, Copy)]
pub struct RightRailSftpTransferItem<'a> {
    source_name: &'a str,
    destination_name: &'a str,
    upload: bool,
    expect_remote_conflict: bool,
}

impl<'a> RightRailSftpTransferItem<'a> {
    /// Capture basenames and direction; the pane still validates names and endpoint ownership before enqueue.
    ///
    /// @author kongweiguang
    pub fn new(
        source_name: &'a str,
        destination_name: &'a str,
        upload: bool,
        expect_remote_conflict: bool,
    ) -> Self {
        Self {
            source_name,
            destination_name,
            upload,
            expect_remote_conflict,
        }
    }

    /// Read the intent values without allowing callers to mutate a queued fixture request.
    ///
    /// @author kongweiguang
    pub fn transfer_intent(&self) -> (&'a str, &'a str, bool, bool) {
        (
            self.source_name,
            self.destination_name,
            self.upload,
            self.expect_remote_conflict,
        )
    }
}

impl FilesPane {
    /// Navigate the hidden local side to the manifest root so production target snapshots stay bound to it.
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_open_local_root(
        &mut self,
        target: &RightRailSftpTransferTarget<'_>,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let (_, _, local_root) = target.fixture_binding();
        let endpoint = acceptance_local_directory_endpoint(local_root)?;
        if !endpoint_matches_locked_host(&endpoint, self.locked_left_host_id.as_ref()) {
            return Err("The fixture local root is outside the locked file panel".to_owned());
        }
        let panel = &self.panels[0];
        let needs_navigation = panel.endpoint != endpoint
            || (!panel.loading
                && panel
                    .listing
                    .as_ref()
                    .is_none_or(|listing| listing.directory != endpoint));
        if needs_navigation && panel.requested_endpoint.as_ref() != Some(&endpoint) {
            self.open_endpoint_on_side(0, endpoint, cx);
        }
        Ok(self
            .acceptance_local_directory_listing(local_root)?
            .is_some())
    }

    /// Submit exactly one fixture file through the same conflict preflight and queue path as the visible pane.
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_submit_transfer(
        &mut self,
        target: &RightRailSftpTransferTarget<'_>,
        item: &RightRailSftpTransferItem<'_>,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let (host_id, remote_directory, local_root) = target.fixture_binding();
        let (source_name, destination_name, upload, expect_remote_conflict) =
            item.transfer_intent();
        if !self.compact_layout {
            return Err("SFTP transfer acceptance requires the compact rail pane".to_owned());
        }
        let Some(remote_listing) = self.acceptance_directory_listing(host_id, remote_directory)?
        else {
            return Ok(false);
        };
        let Some(_) = self.acceptance_local_directory_listing(local_root)? else {
            return Ok(false);
        };
        if self.transfer_submission_busy
            || self.pending_transfer_conflict.is_some()
            || self.pending_global_dialog()
            || self.delete_confirmation.is_some()
            || self.permission_dialog_target.is_some()
        {
            return Ok(false);
        }

        let (request, destination_directory) = acceptance_transfer_request(target, item)?;
        if upload {
            let target =
                self.acceptance_listing_entry(remote_listing, remote_directory, destination_name)?;
            validate_remote_target_state(target.as_ref(), expect_remote_conflict)?;
        } else {
            let source = self
                .acceptance_listing_entry(remote_listing, remote_directory, source_name)?
                .ok_or_else(|| "The fixture SFTP transfer source is missing".to_owned())?;
            if source.kind != FileEntryKind::File || source.endpoint != request.source {
                return Err(
                    "The fixture SFTP transfer source is not the expected regular file".to_owned(),
                );
            }
            if expect_remote_conflict {
                return Err(
                    "Download acceptance does not permit remote conflict fixtures".to_owned(),
                );
            }
        }

        let Some(queue_target) = self.capture_transfer_target_for_directory(&destination_directory)
        else {
            return Ok(false);
        };
        if queue_target.endpoint != destination_directory {
            return Err(
                "The transfer target snapshot does not match the fixture directory".to_owned(),
            );
        }
        self.enqueue_transfer_batch(
            TransferBatchPlan {
                items: vec![TransferEnqueueItem {
                    name: destination_name.to_owned(),
                    request,
                    external_target_lease: None,
                }],
                total: 1,
                included_by_selected_directories: 0,
                issues: Vec::new(),
                labels: BatchEnqueueLabels {
                    action_zh: "右栏验收传输已入队",
                    action_en: "Right-rail fixture transfer queued",
                },
                target: queue_target,
            },
            cx,
        );
        Ok(self.transfer_submission_busy)
    }

    /// Report readiness only for the exact one-file conflict plan currently owned by this SFTP pane.
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_transfer_conflict_ready(
        &self,
        target: &RightRailSftpTransferTarget<'_>,
        item: &RightRailSftpTransferItem<'_>,
    ) -> Result<bool, String> {
        let (host_id, remote_directory, local_root) = target.fixture_binding();
        if self
            .acceptance_directory_listing(host_id, remote_directory)?
            .is_none()
            || self
                .acceptance_local_directory_listing(local_root)?
                .is_none()
        {
            return Ok(false);
        }
        let (request, target_directory) = acceptance_transfer_request(target, item)?;
        acceptance_transfer_conflict_matches(self, &request, &target_directory)
    }

    /// Resolve the matching production conflict plan while preventing policy decisions from crossing fixture paths.
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_resolve_transfer_conflict(
        &mut self,
        target: &RightRailSftpTransferTarget<'_>,
        item: &RightRailSftpTransferItem<'_>,
        policy: ConflictPolicy,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if !self.right_rail_acceptance_transfer_conflict_ready(target, item)? {
            return Ok(false);
        }
        self.resolve_transfer_conflict(policy, cx);
        Ok(self.pending_transfer_conflict.is_none() && self.transfer_submission_busy)
    }

    /// Cancel only the matching unsubmitted conflict plan and leave the existing remote file untouched.
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_cancel_transfer_conflict(
        &mut self,
        target: &RightRailSftpTransferTarget<'_>,
        item: &RightRailSftpTransferItem<'_>,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if !self.right_rail_acceptance_transfer_conflict_ready(target, item)? {
            return Ok(false);
        }
        self.cancel_transfer_conflict(cx);
        Ok(self.pending_transfer_conflict.is_none() && !self.transfer_submission_busy)
    }

    /// Read queue snapshots only while both fixture directories remain the committed browsing targets.
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_transfer_rows(
        &self,
        target: &RightRailSftpTransferTarget<'_>,
    ) -> Result<Option<Vec<TransferSummary>>, String> {
        let (host_id, remote_directory, local_root) = target.fixture_binding();
        if self
            .acceptance_directory_listing(host_id, remote_directory)?
            .is_none()
            || self
                .acceptance_local_directory_listing(local_root)?
                .is_none()
        {
            return Ok(None);
        }
        Ok(Some(self.transfer_rows.clone()))
    }

    /// Return the application file service only after both paths are bound to the isolated fixture listings.
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_file_service(
        &self,
        target: &RightRailSftpTransferTarget<'_>,
    ) -> Result<Option<FileService>, String> {
        let (host_id, remote_directory, local_root) = target.fixture_binding();
        if self
            .acceptance_directory_listing(host_id, remote_directory)?
            .is_none()
            || self
                .acceptance_local_directory_listing(local_root)?
                .is_none()
        {
            return Ok(None);
        }
        Ok(Some(self.files.clone()))
    }

    /// Validate the committed local listing against the canonical fixture root, returning None while it loads.
    ///
    /// @author kongweiguang
    fn acceptance_local_directory_listing<'a>(
        &'a self,
        local_root: &Path,
    ) -> Result<Option<&'a DirectoryListing>, String> {
        let expected = acceptance_local_directory_endpoint(local_root)?;
        if !endpoint_matches_locked_host(&expected, self.locked_left_host_id.as_ref()) {
            return Err("The fixture local root is outside the locked file panel".to_owned());
        }
        let panel = &self.panels[0];
        if panel.endpoint != expected {
            if panel.loading || panel.requested_endpoint.as_ref() == Some(&expected) {
                return Ok(None);
            }
            return Err("The local file pane is not showing the fixture root".to_owned());
        }
        if panel.loading || panel.requested_endpoint.is_some() {
            return Ok(None);
        }
        if let Some(error) = panel.directory_error.as_ref() {
            return Err(error.clone());
        }
        let Some(listing) = panel.listing.as_ref() else {
            return Ok(None);
        };
        if listing.directory != expected {
            return Err("The committed local listing does not match the fixture root".to_owned());
        }
        Ok(Some(listing))
    }
}

/// Build exact source, destination and target-directory endpoints using the fixture root and parsed SFTP path.
///
/// @author kongweiguang
fn acceptance_transfer_request(
    target: &RightRailSftpTransferTarget<'_>,
    item: &RightRailSftpTransferItem<'_>,
) -> Result<(TransferRequest, FileEndpoint), String> {
    let (host_id, remote_directory, local_root) = target.fixture_binding();
    let (source_name, destination_name, upload, _) = item.transfer_intent();
    validate_acceptance_name(source_name)?;
    validate_acceptance_name(destination_name)?;
    let local_directory = acceptance_local_directory_endpoint(local_root)?;
    let remote_directory_endpoint = acceptance_directory_endpoint(host_id, remote_directory)?;
    let remote_source = super::child_endpoint(&remote_directory_endpoint, source_name)
        .ok_or_else(|| "The fixture SFTP source name is not a valid child path".to_owned())?;
    let remote_destination = super::child_endpoint(&remote_directory_endpoint, destination_name)
        .ok_or_else(|| "The fixture SFTP destination name is not a valid child path".to_owned())?;
    let local_source = FileEndpoint::Local {
        path: local_root.join(source_name),
    };
    let local_destination = FileEndpoint::Local {
        path: local_root.join(destination_name),
    };
    let (source, destination, target_directory) = if upload {
        (local_source, remote_destination, remote_directory_endpoint)
    } else {
        (remote_source, local_destination, local_directory)
    };
    Ok((
        TransferRequest {
            source,
            destination,
            kind: TransferKind::File,
            // A late destination race must never turn a fixture enqueue into an unconfirmed overwrite.
            conflict_policy: ConflictPolicy::Skip,
            idle_timeout_seconds: None,
        },
        target_directory,
    ))
}

/// Resolve the fixture local directory only from an absolute root and a single validated child name.
///
/// @author kongweiguang
fn acceptance_local_directory_endpoint(local_root: &Path) -> Result<FileEndpoint, String> {
    if !local_root.is_absolute() {
        return Err("The fixture local root must be an absolute path".to_owned());
    }
    Ok(FileEndpoint::Local {
        path: local_root.to_path_buf(),
    })
}

/// Keep upload conflict fixtures limited to a single existing regular file or an absent target.
///
/// @author kongweiguang
fn validate_remote_target_state(
    target: Option<&super::super::FileEntry>,
    expected_conflict: bool,
) -> Result<(), String> {
    match (expected_conflict, target) {
        (true, Some(entry)) if entry.kind == FileEntryKind::File => Ok(()),
        (false, None) => Ok(()),
        (true, Some(_)) => {
            Err("The fixture upload conflict target is not a regular file".to_owned())
        }
        (true, None) => Err("The expected fixture upload conflict target is missing".to_owned()),
        (false, Some(_)) => Err("A fixture upload target already exists unexpectedly".to_owned()),
    }
}

/// Accept a pending dialog only when it represents one exact fixture item and its original target generation.
///
/// @author kongweiguang
fn acceptance_transfer_conflict_matches(
    pane: &FilesPane,
    request: &TransferRequest,
    destination_directory: &FileEndpoint,
) -> Result<bool, String> {
    let Some(pending) = pane.pending_transfer_conflict.as_ref() else {
        return Ok(false);
    };
    if pending.conflict_count != 1 || pending.plan.items.len() != 1 || pending.plan.total != 1 {
        return Err("The fixture transfer conflict is not a single-file batch".to_owned());
    }
    let item = &pending.plan.items[0];
    if item.request.source != request.source
        || item.request.destination != request.destination
        || item.request.kind != TransferKind::File
        || item.external_target_lease.is_some()
        || pending.plan.target.endpoint != *destination_directory
    {
        return Err("The pending transfer conflict does not match the fixture request".to_owned());
    }
    Ok(pane.transfer_submission_busy && pane.transfer_target_is_current(&pending.plan.target))
}
