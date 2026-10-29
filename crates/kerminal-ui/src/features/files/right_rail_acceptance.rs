//! 仅供固定 SFTP 原生验收使用的受控文件操作入口。
//!
//! @author kongweiguang

#[path = "right_rail_acceptance_transfers.rs"]
pub(super) mod transfers;

use gpui_kit::{Context, Window};
use kerminal_domain::{
    files::{DirectoryListing, FileEndpoint, FileEntryKind},
    identity::HostId,
};

use super::{
    FilesPane, NameAction, child_endpoint, create_directory_destination, endpoint_name,
    parent_endpoint, selection::SelectionGesture, valid_item_name, view_mode::FilesViewMode,
};

const ACCEPTANCE_NAME_PREFIX: &str = "rr-ui-";

impl FilesPane {
    /// 验收读回生产关闭按钮的显式句柄，避免只凭表单可见推断初始焦点正确。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_dialog_close_focused(&self, window: &Window) -> bool {
        self.pending_global_dialog()
            && self
                .global_dialog_close_focus
                .as_ref()
                .is_some_and(|focus| focus.is_focused(window))
    }

    /// 只在 fixture 目录的已提交 listing 中创建带验收前缀的目录，实际提交仍走生产表单状态机。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_open_create_directory(
        &mut self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        validate_acceptance_name(name)?;
        if !self.compact_layout {
            return Err("SFTP file acceptance requires the compact rail pane".to_owned());
        }
        if self.name_action_busy || self.pending_global_dialog() {
            return Ok(false);
        }
        let Some(listing) = self.acceptance_directory_listing(host_id, directory)? else {
            return Ok(false);
        };
        if self
            .acceptance_listing_entry(listing, directory, name)?
            .is_some()
        {
            return Err("The fixture directory name already exists".to_owned());
        }

        let target = acceptance_directory_endpoint(host_id, directory)?;
        self.active_side = 1;
        self.prepare_create_directory(window, cx);
        if self.name_action != Some(NameAction::CreateDirectory)
            || self.name_action_side != Some(1)
            || self.name_action_target.as_ref() != Some(&target)
        {
            return Err(
                "The production create-directory form did not bind to the fixture directory"
                    .to_owned(),
            );
        }
        self.set_acceptance_name_input(name, window, cx)?;
        Ok(self.pending_global_dialog())
    }

    /// 只重命名 fixture 目录中的带前缀目录项；来源与目的端点均从当前 listing 精确解析。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_open_rename(
        &mut self,
        host_id: &HostId,
        directory: &str,
        source_name: &str,
        destination_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        validate_acceptance_name(source_name)?;
        validate_acceptance_name(destination_name)?;
        if source_name == destination_name {
            return Err("The fixture rename must change the directory name".to_owned());
        }
        if !self.compact_layout {
            return Err("SFTP file acceptance requires the compact rail pane".to_owned());
        }
        if self.name_action_busy || self.pending_global_dialog() {
            return Ok(false);
        }
        let Some(listing) = self.acceptance_directory_listing(host_id, directory)? else {
            return Ok(false);
        };
        let source = self
            .acceptance_listing_entry(listing, directory, source_name)?
            .ok_or_else(|| "The fixture directory to rename is missing".to_owned())?;
        if source.kind != FileEntryKind::Directory {
            return Err("The fixture rename target is not a directory".to_owned());
        }
        if self
            .acceptance_listing_entry(listing, directory, destination_name)?
            .is_some()
        {
            return Err("The fixture rename destination already exists".to_owned());
        }

        self.select_acceptance_entry(source.clone(), cx)?;
        self.prepare_rename(window, cx);
        if self.name_action != Some(NameAction::Rename)
            || self.name_action_side != Some(1)
            || self.name_action_target.as_ref() != Some(&source.endpoint)
        {
            return Err(
                "The production rename form did not bind to the fixture directory".to_owned(),
            );
        }
        self.set_acceptance_name_input(destination_name, window, cx)?;
        Ok(self.pending_global_dialog())
    }

    /// 确认前重新校验冻结目标和当前输入，避免验收入口替一个普通用户表单提交任意路径。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_confirm_name_action(
        &mut self,
        host_id: &HostId,
        directory: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if self.name_action_busy {
            return Ok(false);
        }
        let Some(listing) = self.acceptance_directory_listing(host_id, directory)? else {
            return Ok(false);
        };
        if !self.compact_layout
            || !self.pending_global_dialog()
            || self.delete_confirmation.is_some()
            || self.pending_transfer_conflict_count().is_some()
            || self.permission_dialog_target.is_some()
        {
            return Ok(false);
        }
        let Some(action) = self.name_action else {
            return Ok(false);
        };
        if self.name_action_side != Some(1) {
            return Err("The file form is not bound to the fixture SFTP pane".to_owned());
        }
        let directory_endpoint = acceptance_directory_endpoint(host_id, directory)?;
        let target = self
            .name_action_target
            .clone()
            .ok_or_else(|| "The file form target is no longer available".to_owned())?;
        let input = self
            .name_input
            .as_ref()
            .ok_or_else(|| "The file form input is unavailable".to_owned())?;
        let name = input.read(cx).value().to_string();
        validate_acceptance_name(&name)?;

        match action {
            NameAction::CreateDirectory => {
                if target != directory_endpoint {
                    return Err(
                        "The create-directory target no longer matches the fixture path".to_owned(),
                    );
                }
                let destination = child_endpoint(&directory_endpoint, &name).ok_or_else(|| {
                    "The fixture directory name is not a valid child path".to_owned()
                })?;
                if create_directory_destination(&directory_endpoint, &name)
                    .ok()
                    .as_ref()
                    != Some(&destination)
                {
                    return Err(
                        "The create-directory input does not resolve inside the fixture path"
                            .to_owned(),
                    );
                }
                if self
                    .acceptance_listing_entry(listing, directory, &name)?
                    .is_some()
                {
                    return Err("The fixture directory name already exists".to_owned());
                }
            }
            NameAction::Rename => {
                if parent_endpoint(&target).as_ref() != Some(&directory_endpoint) {
                    return Err(
                        "The rename source no longer belongs to the fixture directory".to_owned(),
                    );
                }
                let source_name = endpoint_name(&target)
                    .ok_or_else(|| "The fixture rename source has no directory name".to_owned())?;
                validate_acceptance_name(&source_name)?;
                let source = self
                    .acceptance_listing_entry(listing, directory, &source_name)?
                    .ok_or_else(|| "The fixture directory to rename is missing".to_owned())?;
                if source.kind != FileEntryKind::Directory || source.endpoint != target {
                    return Err(
                        "The rename source is not the expected fixture directory".to_owned()
                    );
                }
                if source_name == name {
                    return Err("The fixture rename must change the directory name".to_owned());
                }
                if self
                    .acceptance_listing_entry(listing, directory, &name)?
                    .is_some()
                {
                    return Err("The fixture rename destination already exists".to_owned());
                }
            }
        }

        self.confirm_global_dialog(cx);
        Ok(self.name_action_busy)
    }

    /// 只为当前 listing 中唯一匹配的 fixture 目录打开原删除确认，不在打开阶段发送删除请求。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_open_delete_confirmation(
        &mut self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        validate_acceptance_name(name)?;
        if self.name_action_busy || self.pending_global_dialog() {
            return Ok(false);
        }
        let Some(listing) = self.acceptance_directory_listing(host_id, directory)? else {
            return Ok(false);
        };
        let entry = self
            .acceptance_listing_entry(listing, directory, name)?
            .ok_or_else(|| "The fixture directory to delete is missing".to_owned())?;
        if entry.kind != FileEntryKind::Directory {
            return Err("The fixture delete target is not a directory".to_owned());
        }

        self.select_acceptance_entry(entry.clone(), cx)?;
        self.delete_selected(cx);
        Ok(self.acceptance_delete_confirmation_matches(&entry.endpoint))
    }

    /// 取消仅接受与 fixture endpoint 完全相同的单目录确认快照，保留文件且不发出 IO。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_cancel_delete_confirmation(
        &mut self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        validate_acceptance_name(name)?;
        let Some(listing) = self.acceptance_directory_listing(host_id, directory)? else {
            return Ok(false);
        };
        let entry = self
            .acceptance_listing_entry(listing, directory, name)?
            .ok_or_else(|| "The fixture directory to cancel is missing".to_owned())?;
        if entry.kind != FileEntryKind::Directory {
            return Err("The fixture delete target is not a directory".to_owned());
        }
        if !self.acceptance_delete_confirmation_matches(&entry.endpoint) {
            return Ok(false);
        }

        self.cancel_delete_confirmation(cx);
        Ok(self.delete_confirmation.is_none())
    }

    /// 真实删除只确认已打开且仍指向同一 fixture 目录的生产确认快照。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_confirm_delete(
        &mut self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        validate_acceptance_name(name)?;
        let Some(listing) = self.acceptance_directory_listing(host_id, directory)? else {
            return Ok(false);
        };
        let entry = self
            .acceptance_listing_entry(listing, directory, name)?
            .ok_or_else(|| "The fixture directory to delete is missing".to_owned())?;
        if entry.kind != FileEntryKind::Directory {
            return Err("The fixture delete target is not a directory".to_owned());
        }
        if !self.acceptance_delete_confirmation_matches(&entry.endpoint) {
            return Ok(false);
        }

        self.confirm_delete_confirmation(cx);
        Ok(self.delete_confirmation.is_none()
            && acceptance_delete_in_progress(self.status.as_deref()))
    }

    /// 状态查询同时等待生产 mutation、Root 弹框和 listing reload 收敛，再核对精确目录项。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_file_entry_ready(
        &self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        should_exist: bool,
    ) -> Result<bool, String> {
        validate_acceptance_name(name)?;
        if self.name_action_busy
            || self.pending_global_dialog()
            || acceptance_delete_in_progress(self.status.as_deref())
        {
            return Ok(false);
        }
        let Some(listing) = self.acceptance_directory_listing(host_id, directory)? else {
            return Ok(false);
        };
        let entry = self.acceptance_listing_entry(listing, directory, name)?;
        if entry
            .as_ref()
            .is_some_and(|entry| entry.kind != FileEntryKind::Directory)
        {
            return Err(
                "The fixture path contains a non-directory item with the expected name".to_owned(),
            );
        }
        let exists = entry.is_some();
        if exists == should_exist {
            return Ok(true);
        }
        if let Some(status) = self.status.as_ref() {
            return Err(status.clone());
        }
        Ok(false)
    }

    /// 只返回当前完成 listing 的引用；在途导航等待回执，已停留在其他目录则直接拒绝。
    ///
    /// @author kongweiguang
    fn acceptance_directory_listing<'a>(
        &'a self,
        host_id: &HostId,
        directory: &str,
    ) -> Result<Option<&'a DirectoryListing>, String> {
        let expected = acceptance_directory_endpoint(host_id, directory)?;
        let Some(FileEndpoint::Remote {
            host_id: terminal_host_id,
            ..
        }) = self.current_terminal_endpoint.as_ref()
        else {
            return Err("The active terminal is not bound to an SSH host".to_owned());
        };
        if terminal_host_id != host_id {
            return Err("The active terminal is bound to a different SSH host".to_owned());
        }

        let panel = &self.panels[1];
        if panel.endpoint != expected {
            if panel.loading || panel.requested_endpoint.as_ref() == Some(&expected) {
                return Ok(None);
            }
            return Err("The SFTP pane is not showing the exact fixture directory".to_owned());
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
            return Err(
                "The committed SFTP listing does not match the fixture directory".to_owned(),
            );
        }
        Ok(Some(listing))
    }

    /// 名称必须从当前目录 listing 映射为唯一 child endpoint，拒绝同名的其它路径对象。
    ///
    /// @author kongweiguang
    fn acceptance_listing_entry(
        &self,
        listing: &DirectoryListing,
        directory: &str,
        name: &str,
    ) -> Result<Option<super::FileEntry>, String> {
        let expected_directory = acceptance_directory_endpoint(
            listing
                .directory
                .host_id()
                .ok_or_else(|| "The fixture listing is not remote".to_owned())?,
            directory,
        )?;
        if listing.directory != expected_directory {
            return Err("The listing changed away from the fixture directory".to_owned());
        }
        let expected_endpoint = child_endpoint(&expected_directory, name)
            .ok_or_else(|| "The fixture item name is not a valid child path".to_owned())?;
        let mut matches = listing.entries.iter().filter(|entry| entry.name == name);
        let Some(entry) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            return Err("The fixture directory listing contains duplicate item names".to_owned());
        }
        if entry.endpoint != expected_endpoint {
            return Err("The fixture item name resolves to a different endpoint".to_owned());
        }
        Ok(Some(entry.clone()))
    }

    /// 选中单一 fixture 目录项时复用当前文件列表或树的生产选择更新路径。
    ///
    /// @author kongweiguang
    fn select_acceptance_entry(
        &mut self,
        entry: super::FileEntry,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.view_mode == FilesViewMode::Tree {
            if self.panels[1].tree.entry(&entry.endpoint).is_none() {
                return Err(
                    "The fixture directory is not present in the visible file tree".to_owned(),
                );
            }
            self.select_tree_entry(1, entry.endpoint.clone(), SelectionGesture::Replace, cx);
        } else {
            self.select_entry(
                1,
                entry.clone(),
                SelectionGesture::Replace,
                std::slice::from_ref(&entry.name),
                cx,
            );
        }
        let selected = self.selected_entries(1);
        if selected.len() != 1 || selected[0].endpoint != entry.endpoint {
            return Err(
                "The production file selection did not isolate the fixture directory".to_owned(),
            );
        }
        Ok(())
    }

    /// 用生产输入状态填充已打开的表单，避免验收直接调用 FileService 或绕过名称校验。
    ///
    /// @author kongweiguang
    fn set_acceptance_name_input(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let input = self
            .name_input
            .clone()
            .ok_or_else(|| "The production file form input is unavailable".to_owned())?;
        input.update(cx, |state, cx| state.set_value(name.to_owned(), window, cx));
        Ok(())
    }

    /// 删除状态快照必须恰好包含唯一预期目录，避免确认旧选择或扩展到批量删除。
    ///
    /// @author kongweiguang
    fn acceptance_delete_confirmation_matches(&self, endpoint: &FileEndpoint) -> bool {
        self.delete_confirmation.as_ref().is_some_and(|entries| {
            entries.len() == 1
                && entries[0].endpoint == *endpoint
                && entries[0].kind == FileEntryKind::Directory
        })
    }
}

/// Fixture 目录仅接受绝对 SFTP 路径，保留已保存主机身份并拒绝本机或容器 endpoint。
///
/// @author kongweiguang
fn acceptance_directory_endpoint(
    host_id: &HostId,
    directory: &str,
) -> Result<FileEndpoint, String> {
    let endpoint = FileEndpoint::remote(host_id.clone(), directory)
        .map_err(|_| "The fixture SFTP directory path is invalid".to_owned())?;
    if !matches!(
        &endpoint,
        FileEndpoint::Remote { path, .. } if path.as_str().starts_with('/')
    ) {
        return Err("The fixture SFTP directory path must be absolute".to_owned());
    }
    Ok(endpoint)
}

/// 验收文件名只能使用专用前缀和单一目录项语法，避免测试命令触及 fixture 外路径。
///
/// @author kongweiguang
fn validate_acceptance_name(name: &str) -> Result<(), String> {
    let suffix = name
        .strip_prefix(ACCEPTANCE_NAME_PREFIX)
        .unwrap_or_default();
    if suffix.is_empty()
        || !suffix
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        || !valid_item_name(name)
    {
        return Err(
            "SFTP acceptance accepts only a single rr-ui- fixture directory name".to_owned(),
        );
    }
    Ok(())
}

/// 删除服务没有单独 busy 字段，使用生产删除状态文案延续同一请求的等待窗口。
///
/// @author kongweiguang
fn acceptance_delete_in_progress(status: Option<&str>) -> bool {
    status
        .is_some_and(|status| status.starts_with("Deleting ") || status.starts_with("正在删除所选"))
}
