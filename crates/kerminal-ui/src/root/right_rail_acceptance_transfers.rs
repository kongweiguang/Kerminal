//! Right-rail fixture transfers forwarded to the mounted production FilesPane.
//!
//! @author kongweiguang

use gpui_kit::{Context, Entity};
use kerminal_application::files::FileService;
use kerminal_domain::files::{ConflictPolicy, TransferSummary};

use crate::features::files::{FilesPane, RightRailSftpTransferItem, RightRailSftpTransferTarget};

use super::{RootConfirmationOwner, RootView};

impl RootView {
    /// Resolve the SFTP pane only while it is the active tool for the bound fixture SSH session.
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_sftp_transfer_panel(
        &self,
        target: &RightRailSftpTransferTarget<'_>,
    ) -> Result<Option<Entity<FilesPane>>, String> {
        let (host_id, _, _) = target.fixture_binding();
        if self.right_rail_acceptance_active_tool()
            != Some(kerminal_application::preferences::settings::ToolRailToolId::Sftp)
        {
            return Err("SFTP tool is not the active acceptance panel".to_owned());
        }
        if self
            .right_rail_acceptance_fixture_binding(host_id)?
            .is_none()
        {
            return Ok(None);
        }
        self.sftp_panel
            .clone()
            .map(Some)
            .ok_or_else(|| "SFTP file pane is unavailable".to_owned())
    }

    /// Navigate the local side to the exact fixture root used for downloads and upload sources.
    ///
    /// @author kongweiguang
    pub fn open_right_rail_acceptance_sftp_local_root(
        &mut self,
        target: &RightRailSftpTransferTarget<'_>,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some(files) = self.right_rail_acceptance_sftp_transfer_panel(target)? else {
            return Ok(false);
        };
        if self.root_confirmation_owner.is_some() {
            return Ok(false);
        }
        files.update(cx, |pane, pane_cx| {
            pane.right_rail_acceptance_open_local_root(target, pane_cx)
        })
    }

    /// Enqueue a single fixture transfer through FilesPane's production conflict preflight and queue submission.
    ///
    /// @author kongweiguang
    pub fn submit_right_rail_acceptance_sftp_transfer(
        &mut self,
        target: &RightRailSftpTransferTarget<'_>,
        item: &RightRailSftpTransferItem<'_>,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let (host_id, remote_directory, _) = target.fixture_binding();
        if !self.right_rail_acceptance_sftp_directory_ready(host_id, remote_directory, cx)? {
            return Ok(false);
        }
        let Some(files) = self.right_rail_acceptance_sftp_transfer_panel(target)? else {
            return Ok(false);
        };
        if self.root_confirmation_owner.is_some() {
            return Ok(false);
        }
        files.update(cx, |pane, pane_cx| {
            pane.right_rail_acceptance_submit_transfer(target, item, pane_cx)
        })
    }

    /// Permit the mounted pane's own promoted conflict modal while keeping every other modal owner blocked.
    /// The pane still verifies the exact source, destination, batch count and fixture directory before acting.
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_transfer_modal_available(&self, files: &Entity<FilesPane>) -> bool {
        match self.root_confirmation_owner {
            None => true,
            Some(RootConfirmationOwner::FileDelete) => self
                .root_confirmation_file_pane
                .as_ref()
                .is_some_and(|owner| owner.entity_id() == files.entity_id()),
            Some(_) => false,
        }
    }

    /// Wait for the production conflict dialog only when it owns the exact current fixture request.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_sftp_transfer_conflict_ready(
        &self,
        target: &RightRailSftpTransferTarget<'_>,
        item: &RightRailSftpTransferItem<'_>,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let Some(files) = self.right_rail_acceptance_sftp_transfer_panel(target)? else {
            return Ok(false);
        };
        if !self.right_rail_acceptance_transfer_modal_available(&files) {
            return Ok(false);
        }
        files
            .read(cx)
            .right_rail_acceptance_transfer_conflict_ready(target, item)
    }

    /// Resolve the exact fixture conflict by invoking the production policy callback on its owning pane.
    ///
    /// @author kongweiguang
    pub fn resolve_right_rail_acceptance_sftp_transfer_conflict(
        &mut self,
        target: &RightRailSftpTransferTarget<'_>,
        item: &RightRailSftpTransferItem<'_>,
        policy: ConflictPolicy,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some(files) = self.right_rail_acceptance_sftp_transfer_panel(target)? else {
            return Ok(false);
        };
        if !self.right_rail_acceptance_transfer_modal_available(&files) {
            return Ok(false);
        }
        files.update(cx, |pane, pane_cx| {
            pane.right_rail_acceptance_resolve_transfer_conflict(target, item, policy, pane_cx)
        })
    }

    /// Cancel only the exact unsubmitted fixture conflict through the production no-effect close path.
    ///
    /// @author kongweiguang
    pub fn cancel_right_rail_acceptance_sftp_transfer_conflict(
        &mut self,
        target: &RightRailSftpTransferTarget<'_>,
        item: &RightRailSftpTransferItem<'_>,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some(files) = self.right_rail_acceptance_sftp_transfer_panel(target)? else {
            return Ok(false);
        };
        if !self.right_rail_acceptance_transfer_modal_available(&files) {
            return Ok(false);
        }
        files.update(cx, |pane, pane_cx| {
            pane.right_rail_acceptance_cancel_transfer_conflict(target, item, pane_cx)
        })
    }

    /// Read queue results and services only while the fixture SFTP pane and both directory snapshots are current.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_sftp_transfer_rows(
        &self,
        target: &RightRailSftpTransferTarget<'_>,
        cx: &gpui_kit::Context<Self>,
    ) -> Result<Option<Vec<TransferSummary>>, String> {
        let Some(files) = self.right_rail_acceptance_sftp_transfer_panel(target)? else {
            return Ok(None);
        };
        if self.root_confirmation_owner.is_some() {
            return Ok(None);
        }
        files.read(cx).right_rail_acceptance_transfer_rows(target)
    }

    /// Expose the application file port for bounded SHA readback only after both fixture listings are committed.
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_sftp_file_service(
        &self,
        target: &RightRailSftpTransferTarget<'_>,
        cx: &gpui_kit::Context<Self>,
    ) -> Result<Option<FileService>, String> {
        let Some(files) = self.right_rail_acceptance_sftp_transfer_panel(target)? else {
            return Ok(None);
        };
        if self.root_confirmation_owner.is_some() {
            return Ok(None);
        }
        files.read(cx).right_rail_acceptance_file_service(target)
    }
}
