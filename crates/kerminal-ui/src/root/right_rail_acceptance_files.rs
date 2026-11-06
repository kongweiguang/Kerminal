//! Right-rail SFTP fixture actions routed through the mounted production FilesPane.
//!
//! @author kongweiguang

use gpui_kit::{Context, Entity, Window};
use kerminal_application::{
    preferences::settings::ToolRailToolId,
    sessions::UiSessionBinding,
    workspace::{WorkspaceSessionTab, WorkspaceTargetRef},
};
use kerminal_domain::{
    files::{FileEndpoint, RemotePath},
    identity::{HostId, TabId},
};

use crate::features::{files::FilesPane, workspace_tabs::WorkspaceTabEvent};

use super::{RootConfirmationOwner, RootView};

/// 验收读回固定导航身份、数量、内部索引及活动/脏状态，别名保留现有只读探针契约。
///
/// @author kongweiguang
pub type RightRailFileTabState = (TabId, usize, usize, bool, bool);

impl RootView {
    /// 在挂载文件面板前核对真实终端目录，使验收首个 SFTP 读取也限定在 fixture 内。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_follow_terminal_snapshot(
        &self,
        host_id: &HostId,
    ) -> Result<Option<(UiSessionBinding, Option<String>)>, String> {
        let Some(binding) = self.right_rail_acceptance_fixture_binding(host_id)? else {
            return Ok(None);
        };
        let Some(pane_id) = self.workspace_runtime.pane_for_session(&binding.session_id) else {
            return Ok(None);
        };
        let Some(pane) = self.workspace_runtime.pane(pane_id) else {
            return Ok(None);
        };
        Ok(Some((
            binding,
            pane.current_working_directory.as_deref().map(str::to_owned),
        )))
    }

    /// 同时回读活动 SSH 身份、实际 OSC7 目录和面板开关，防止启动目录回退冒充终端上报。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_follow_snapshot(
        &self,
        host_id: &HostId,
        cx: &Context<Self>,
    ) -> Result<Option<(UiSessionBinding, Option<String>, Option<String>, bool)>, String> {
        let Some((binding, cwd)) = self.right_rail_acceptance_follow_terminal_snapshot(host_id)?
        else {
            return Ok(None);
        };
        let Some(files) = self.right_rail_acceptance_fixture_sftp_panel(host_id)? else {
            return Ok(None);
        };
        let files = files.read(cx);
        let target = match files.right_rail_acceptance_follow_target() {
            Some(FileEndpoint::Remote {
                host_id: actual,
                path,
            }) if actual == host_id => Some(path.as_str().to_owned()),
            Some(_) => return Err("SFTP 跟随目标不属于当前 fixture 主机".to_owned()),
            None => None,
        };
        Ok(Some((
            binding,
            cwd,
            target,
            files.right_rail_acceptance_follow_enabled(),
        )))
    }

    /// 只投影当前 fixture 右栏的刷新状态，不改变目录或替代生产按钮事件。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_sftp_refresh_snapshot(
        &self,
        host_id: &HostId,
        directory: &str,
        cx: &Context<Self>,
    ) -> Result<Option<(bool, usize, bool, u64)>, String> {
        let Some(files) = self.right_rail_acceptance_fixture_sftp_panel(host_id)? else {
            return Ok(None);
        };
        files
            .read(cx)
            .right_rail_acceptance_refresh_snapshot(host_id, directory)
    }

    /// 只读验证当前 fixture 面板的选择已落到目标行。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_sftp_file_selected(
        &self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let Some(files) = self.right_rail_acceptance_fixture_sftp_panel(host_id)? else {
            return Ok(false);
        };
        if self.root_confirmation_owner.is_some() {
            return Ok(false);
        }
        files
            .read(cx)
            .right_rail_acceptance_file_selected(host_id, directory, name)
    }

    /// 只从当前 fixture 终端或其文件 Tab 的真实 FilesPane 打开，连续浏览也必须保持同一主机。
    ///
    /// @author kongweiguang
    pub fn open_right_rail_acceptance_sftp_file(
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
            pane.right_rail_acceptance_open_file(host_id, directory, name, pane_cx)
        })
    }

    /// 返回精确 endpoint 的顶栏数量、内部索引和编辑状态，不把加载中的文件当作就绪。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_file_tab_state(
        &self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        cx: &Context<Self>,
    ) -> Result<Option<RightRailFileTabState>, String> {
        let endpoint = acceptance_file_endpoint(host_id, directory, name)?;
        let Some(controller) = self.workspace_controller.as_ref() else {
            return Ok(None);
        };
        let snapshot = controller.snapshot();
        let matching = snapshot
            .terminal_tabs
            .iter()
            .filter(|tab| self.workspace_files.file_endpoint(tab.id()).as_ref() == Some(&endpoint))
            .map(|tab| tab.id().clone())
            .collect::<Vec<_>>();
        let Some(tab_id) = matching.first().cloned() else {
            return Ok(None);
        };
        let Some(editor) = self.file_editor_workspace.as_ref() else {
            return Err("The file editor is unavailable".to_owned());
        };
        let projection = editor.read(cx).workspace_projection();
        let Some((index, tab)) = projection
            .tabs
            .iter()
            .enumerate()
            .find(|(_, tab)| tab.endpoint == endpoint && !tab.loading)
        else {
            return Ok(None);
        };
        Ok(Some((
            tab_id.clone(),
            matching.len(),
            index,
            snapshot.active_tab_id.as_ref() == Some(&tab_id)
                && tab.active
                && matches!(self.center_panel, super::CenterPanel::Editor),
            tab.dirty,
        )))
    }

    /// 验证 fixture 文件在实际快照中相邻且顺序正确，拖拽后的后继不能靠索引猜测。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_file_tabs_adjacent(
        &self,
        host_id: &HostId,
        directory: &str,
        left_name: &str,
        right_name: &str,
    ) -> Result<bool, String> {
        let left = acceptance_file_endpoint(host_id, directory, left_name)?;
        let right = acceptance_file_endpoint(host_id, directory, right_name)?;
        let Some(controller) = self.workspace_controller.as_ref() else {
            return Ok(false);
        };
        let tabs = &controller.snapshot().terminal_tabs;
        let left_index = tabs
            .iter()
            .position(|tab| self.workspace_files.file_endpoint(tab.id()).as_ref() == Some(&left));
        let right_index = tabs
            .iter()
            .position(|tab| self.workspace_files.file_endpoint(tab.id()).as_ref() == Some(&right));
        Ok(left_index
            .zip(right_index)
            .is_some_and(|(left, right)| left + 1 == right))
    }

    /// 仅当快照仍选中原终端且中心确实显示终端时，才能认定关闭后的终端后继已生效。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_terminal_tab_selected(&self, tab_id: &TabId) -> bool {
        self.workspace_controller
            .as_ref()
            .is_some_and(|controller| {
                let snapshot = controller.snapshot();
                snapshot.active_tab_id.as_ref() == Some(tab_id)
                    && matches!(self.center_panel, super::CenterPanel::Terminal)
                    && snapshot.terminal_tabs.iter().any(|tab| {
                        tab.id() == tab_id && matches!(tab, WorkspaceSessionTab::Terminal { .. })
                    })
            })
    }

    /// 只在当前 fixture SSH 终端有效时发起真实 SFTP 预检，完成由生产连接事件异步回报。
    ///
    /// @author kongweiguang
    pub fn request_right_rail_acceptance_sftp_workspace_tab(
        &mut self,
        host_id: &HostId,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if self
            .right_rail_acceptance_fixture_binding(host_id)?
            .is_none()
            || self.root_confirmation_owner.is_some()
        {
            return Ok(false);
        }
        let Some(panel) = self.connections_panel.clone() else {
            return Err("The fixture connections panel is unavailable".to_owned());
        };
        panel.update(cx, |panel, cx| {
            panel.request_create_sftp_transfer_tab_host(host_id, cx)
        });
        Ok(true)
    }

    /// 记录此 fixture 主机已有的传输标签身份，供异步预检完成后识别新创建的唯一标签。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_sftp_workspace_tab_ids(&self, host_id: &HostId) -> Vec<TabId> {
        self.workspace_controller
            .as_ref()
            .map(|controller| {
                controller
                    .snapshot()
                    .terminal_tabs
                    .iter()
                    .filter_map(|tab| match tab {
                        WorkspaceSessionTab::SftpTransfer {
                            id,
                            right_host_id: Some(candidate),
                            ..
                        } if candidate == host_id => Some(id.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 传输后继必须同时命中快照、独立 FilesPane 和中心页，不能只看活动 TabId。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_sftp_workspace_tab_selected(
        &self,
        host_id: &HostId,
        tab_id: &TabId,
    ) -> bool {
        self.right_rail_acceptance_sftp_workspace_tab_ids(host_id)
            .contains(tab_id)
            && self
                .workspace_controller
                .as_ref()
                .is_some_and(|controller| {
                    controller.snapshot().active_tab_id.as_ref() == Some(tab_id)
                })
            && matches!(self.center_panel, super::CenterPanel::Files)
            && matches!(
                &self.files_workspace_selection,
                super::workspace_files::FilesWorkspaceSelection::TransferTab(selected) if selected == tab_id
            )
            && self.workspace_files.files_pane(tab_id).is_some()
            && matches!(
                self.workspace_files.restore_state(tab_id),
                Some(super::workspace_files::WorkspaceFileRestoreState::Ready)
            )
    }

    /// 关闭回读同时检查导航描述和编辑器缓冲，不能只因其中一个投影消失就判定完成。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_file_closed(
        &self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let endpoint = acceptance_file_endpoint(host_id, directory, name)?;
        let Some(controller) = self.workspace_controller.as_ref() else {
            return Ok(false);
        };
        let nav_present = controller
            .snapshot()
            .terminal_tabs
            .iter()
            .any(|tab| match tab {
                WorkspaceSessionTab::WorkspaceFile {
                    target: WorkspaceTargetRef::Ssh { host_id: candidate },
                    path,
                    ..
                } => {
                    candidate == host_id
                        && endpoint
                            .remote_path()
                            .is_some_and(|expected| expected.as_str() == path)
                }
                _ => false,
            });
        let editor_present = self.file_editor_workspace.as_ref().is_some_and(|editor| {
            editor
                .read(cx)
                .workspace_projection()
                .tabs
                .iter()
                .any(|tab| tab.endpoint == endpoint)
        });
        Ok(!nav_present && !editor_present)
    }

    /// 精确读取 fixture 文件的编辑器状态，失败或只读不能冒充可编辑就绪。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_file_ready(
        &self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        expected: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let endpoint = acceptance_file_endpoint(host_id, directory, name)?;
        let Some((_, count, _, active, _)) =
            self.right_rail_acceptance_file_tab_state(host_id, directory, name, cx)?
        else {
            return Ok(false);
        };
        Ok(count == 1
            && active
            && self.file_editor_workspace.as_ref().is_some_and(|editor| {
                editor
                    .read(cx)
                    .right_rail_acceptance_file_ready(&endpoint, expected, cx)
            }))
    }

    /// revision 必须属于唯一活动文件且与实际远端读回一致，避免接受其他标签的回执。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_file_revision_matches(
        &self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        revision: &kerminal_domain::files::FileRevision,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let endpoint = acceptance_file_endpoint(host_id, directory, name)?;
        let Some((_, count, _, active, dirty)) =
            self.right_rail_acceptance_file_tab_state(host_id, directory, name, cx)?
        else {
            return Ok(false);
        };
        Ok(count == 1
            && active
            && !dirty
            && self.root_confirmation_owner.is_none()
            && self.file_editor_workspace.as_ref().is_some_and(|editor| {
                editor
                    .read(cx)
                    .right_rail_acceptance_file_revision_matches(&endpoint, revision)
            }))
    }

    /// 仅接受目标活动文件持有的冲突和重载确认，取消后仍要求正文与脏状态完整。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_file_conflict(
        &self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        expected: &str,
        reload_confirmation: bool,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let endpoint = acceptance_file_endpoint(host_id, directory, name)?;
        let Some((_, count, _, active, dirty)) =
            self.right_rail_acceptance_file_tab_state(host_id, directory, name, cx)?
        else {
            return Ok(false);
        };
        let owner_matches = if reload_confirmation {
            self.root_confirmation_owner == Some(RootConfirmationOwner::Editor)
        } else {
            self.root_confirmation_owner.is_none()
        };
        Ok(count == 1
            && active
            && dirty
            && owner_matches
            && self.file_editor_workspace.as_ref().is_some_and(|editor| {
                editor.read(cx).right_rail_acceptance_file_conflict(
                    &endpoint,
                    expected,
                    reload_confirmation,
                    cx,
                )
            }))
    }

    /// 修改仅作用于当前 fixture 文档的编辑缓冲，不向远端保存或改写其他标签。
    ///
    /// @author kongweiguang
    pub fn replace_right_rail_acceptance_file_text(
        &mut self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let endpoint = acceptance_file_endpoint(host_id, directory, name)?;
        let Some((_, count, _, active, _)) =
            self.right_rail_acceptance_file_tab_state(host_id, directory, name, cx)?
        else {
            return Ok(false);
        };
        if count != 1 || !active || self.root_confirmation_owner.is_some() {
            return Ok(false);
        }
        let Some(editor) = self.file_editor_workspace.clone() else {
            return Err("The file editor is unavailable".to_owned());
        };
        Ok(editor.update(cx, |editor, cx| {
            editor.right_rail_acceptance_replace_file(&endpoint, text, window, cx)
        }))
    }

    /// 只对当前活动 fixture 文件发起顶栏关闭，脏稿继续由生产关闭事务决定。
    ///
    /// @author kongweiguang
    pub fn request_right_rail_acceptance_file_tab_close(
        &mut self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some((tab_id, count, _, active, _)) =
            self.right_rail_acceptance_file_tab_state(host_id, directory, name, cx)?
        else {
            return Ok(false);
        };
        if count != 1 || !active || self.root_confirmation_owner.is_some() {
            return Ok(false);
        }
        Ok(self.handle_workspace_tab_event(&WorkspaceTabEvent::Close(tab_id), window, cx))
    }

    /// 只取消当前 fixture 文件拥有的编辑器确认，保留脏正文和导航标签。
    ///
    /// @author kongweiguang
    pub fn cancel_right_rail_acceptance_file_close(
        &mut self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let Some((_, count, _, active, dirty)) =
            self.right_rail_acceptance_file_tab_state(host_id, directory, name, cx)?
        else {
            return Ok(false);
        };
        if count != 1
            || !active
            || !dirty
            || self.root_confirmation_owner != Some(RootConfirmationOwner::Editor)
        {
            return Ok(false);
        }
        let Some(editor) = self.file_editor_workspace.clone() else {
            return Ok(false);
        };
        editor.update(cx, |workspace, cx| workspace.cancel_root_confirmation(cx));
        Ok(true)
    }

    /// 只在目标仍是活动脏文档且 Root 确认层由编辑器持有时报告关闭确认已出现。
    ///
    /// @author kongweiguang
    pub fn right_rail_acceptance_file_close_confirmation_ready(
        &self,
        host_id: &HostId,
        directory: &str,
        name: &str,
        cx: &Context<Self>,
    ) -> Result<bool, String> {
        let Some((_, count, _, active, dirty)) =
            self.right_rail_acceptance_file_tab_state(host_id, directory, name, cx)?
        else {
            return Ok(false);
        };
        Ok(count == 1
            && active
            && dirty
            && self.root_confirmation_owner == Some(RootConfirmationOwner::Editor)
            && self
                .file_editor_workspace
                .as_ref()
                .is_some_and(|editor| editor.read(cx).root_confirmation_active()))
    }

    /// 活动 SFTP rail 必须归属当前 fixture SSH pane 或实际显示的文件，不能靠后台终端通过门禁。
    ///
    /// @author kongweiguang
    fn right_rail_acceptance_fixture_sftp_panel(
        &self,
        host_id: &HostId,
    ) -> Result<Option<Entity<FilesPane>>, String> {
        if self.right_rail_acceptance_active_tool() != Some(ToolRailToolId::Sftp) {
            return Err("SFTP 工具不是当前验收面板".to_owned());
        }
        if self.center_panel == super::CenterPanel::Editor {
            match self.active_file_browser_target() {
                Some((
                    FileEndpoint::Remote {
                        host_id: actual, ..
                    },
                    _,
                )) if actual == *host_id => {}
                Some(_) => return Err("活动文件不属于当前 fixture SSH 主机".to_owned()),
                None => return Ok(None),
            }
        } else if self
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

/// 将验收文件限制在 fixture 目录的单个 rr-ui 项，不能以字符串拼接越出目标。
///
/// @author kongweiguang
fn acceptance_file_endpoint(
    host_id: &HostId,
    directory: &str,
    name: &str,
) -> Result<FileEndpoint, String> {
    let suffix = name.strip_prefix("rr-ui-").unwrap_or_default();
    if suffix.is_empty()
        || !suffix
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err("The editor fixture name is invalid".to_owned());
    }
    let directory = RemotePath::parse(directory)
        .map_err(|_| "The editor fixture directory is invalid".to_owned())?;
    let path = directory
        .join_name(name)
        .map_err(|_| "The editor fixture file path is invalid".to_owned())?;
    Ok(FileEndpoint::Remote {
        host_id: host_id.clone(),
        path,
    })
}
