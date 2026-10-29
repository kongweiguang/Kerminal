//! Right-rail SFTP fixture actions routed through the mounted production FilesPane.
//!
//! @author kongweiguang

use gpui_kit::{Context, Entity, Window};
use kerminal_application::preferences::settings::ToolRailToolId;
use kerminal_domain::identity::HostId;

use crate::features::files::FilesPane;

use super::{RootConfirmationOwner, RootView};

impl RootView {
    /// 只有活动 SFTP rail 和当前 fixture SSH pane 可以取得验收文件面板实体。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_fixture_sftp_panel(
        &self,
        host_id: &HostId,
    ) -> Result<Option<Entity<FilesPane>>, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Sftp) {
            return Err("SFTP 工具不是当前验收面板".to_owned());
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
            .ok_or_else(|| "SFTP 文件面板不可用".to_owned())
    }

    /// 文件弹层动作只能操作 Root 当前实际拥有遮罩的同一个 SFTP FilesPane。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_sftp_confirmation_is_owned(&self, files: &Entity<FilesPane>) -> bool {
        self.root_confirmation_owner == Some(RootConfirmationOwner::FileDelete)
            && self
                .root_confirmation_file_pane
                .as_ref()
                .is_some_and(|owner| owner.entity_id() == files.entity_id())
    }

    /// 通过生产新建表单打开 fixture 目录操作，Root 已占用全局弹框时保持等待。
    ///
    /// @author kongweiguang
    pub fn open_right_rail_acceptance_sftp_create_directory(
        &mut self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some(files) = self.right_rail_acceptance_fixture_sftp_panel(host_id)? else {
            return Ok(false);
        };
        if self.root_confirmation_owner.is_some() {
            return Ok(false);
        }
        files.update(cx, |pane, pane_cx| {
            pane.right_rail_acceptance_open_create_directory(
                host_id, directory, name, window, pane_cx,
            )
        })
    }

    /// 通过 production selection 与 rename 表单打开单一 fixture 目录重命名操作。
    ///
    /// @author kongweiguang
    pub fn open_right_rail_acceptance_sftp_rename(
        &mut self,
        host_id: &HostId,
        directory: &str,
        source_name: &str,
        destination_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some(files) = self.right_rail_acceptance_fixture_sftp_panel(host_id)? else {
            return Ok(false);
        };
        if self.root_confirmation_owner.is_some() {
            return Ok(false);
        }
        files.update(cx, |pane, pane_cx| {
            pane.right_rail_acceptance_open_rename(
                host_id,
                directory,
                source_name,
                destination_name,
                window,
                pane_cx,
            )
        })
    }

    /// 二次校验当前 fixture 表单后才调用 FilesPane 的原确认入口执行 create 或 rename。
    ///
    /// @author kongweiguang
    pub fn confirm_right_rail_acceptance_sftp_name_action(
        &mut self,
        host_id: &HostId,
        directory: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some(files) = self.right_rail_acceptance_fixture_sftp_panel(host_id)? else {
            return Ok(false);
        };
        if !self.right_rail_acceptance_sftp_confirmation_is_owned(&files) {
            return Ok(false);
        }
        files.update(cx, |pane, pane_cx| {
            pane.right_rail_acceptance_confirm_name_action(host_id, directory, pane_cx)
        })
    }

    /// 通过生产单项选择与删除状态机打开确认，打开本身不发送远端删除请求。
    ///
    /// @author kongweiguang
    pub fn open_right_rail_acceptance_sftp_delete_confirmation(
        &mut self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some(files) = self.right_rail_acceptance_fixture_sftp_panel(host_id)? else {
            return Ok(false);
        };
        if self.root_confirmation_owner.is_some() {
            return Ok(false);
        }
        files.update(cx, |pane, pane_cx| {
            pane.right_rail_acceptance_open_delete_confirmation(host_id, directory, name, pane_cx)
        })
    }

    /// 取消精确 fixture 删除确认并回到 FilesPane 原有的无副作用取消路径。
    ///
    /// @author kongweiguang
    pub fn cancel_right_rail_acceptance_sftp_delete_confirmation(
        &mut self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some(files) = self.right_rail_acceptance_fixture_sftp_panel(host_id)? else {
            return Ok(false);
        };
        if !self.right_rail_acceptance_sftp_confirmation_is_owned(&files) {
            return Ok(false);
        }
        files.update(cx, |pane, pane_cx| {
            pane.right_rail_acceptance_cancel_delete_confirmation(host_id, directory, name, pane_cx)
        })
    }

    /// 确认前再次核对精确 fixture 目录，再由 FilesPane 提交原真实删除用例。
    ///
    /// @author kongweiguang
    pub fn confirm_right_rail_acceptance_sftp_delete(
        &mut self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some(files) = self.right_rail_acceptance_fixture_sftp_panel(host_id)? else {
            return Ok(false);
        };
        if !self.right_rail_acceptance_sftp_confirmation_is_owned(&files) {
            return Ok(false);
        }
        files.update(cx, |pane, pane_cx| {
            pane.right_rail_acceptance_confirm_delete(host_id, directory, name, pane_cx)
        })
    }

    /// 仅当 Root modal 已释放且当前已提交 SFTP listing 与预期存在状态一致时报告 ready。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_sftp_file_entry_ready(
        &self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        should_exist: bool,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let Some(files) = self.right_rail_acceptance_fixture_sftp_panel(host_id)? else {
            return Ok(false);
        };
        if self.root_confirmation_owner.is_some() {
            return Ok(false);
        }
        files.read(cx).right_rail_acceptance_file_entry_ready(
            host_id,
            directory,
            name,
            should_exist,
        )
    }
}
