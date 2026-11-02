//! 左右主机标签各自持有浏览状态，活动面板仍由文件工作区统一编排。
//!
//! @author kongweiguang

use gpui_kit::Context;
use kerminal_domain::{files::FileEndpoint, identity::HostId};

use super::transfer_refresh::{TransferRefreshScope, panel_needs_transfer_refresh};
use super::{BrowserPanel, FilePaneSide, FilesPane};

/// 标签身份独立于 HostId，同一主机的多个标签可以保存不同的目录。
pub(super) struct HostTab {
    pub(super) id: u64,
    pub(super) target: FileEndpoint,
    dirty: bool,
    panel: Option<BrowserPanel>,
}

/// 当前活动标签的 BrowserPanel 位于 FilesPane，非活动标签独立持有其完整面板。
#[derive(Default)]
pub(super) struct HostTabs {
    pub(super) sides: [Vec<HostTab>; 2],
    pub(super) active: [Option<u64>; 2],
    next_id: u64,
    panel_generations: [u64; 2],
    preferred_right_host: Option<HostId>,
}

impl HostTabs {
    /// 新标签使用进程内稳定身份，不以主机名或数组索引作为控件键。
    ///
    /// @author kongweiguang
    fn next_id(&mut self) -> u64 {
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.next_id
    }
}

impl HostTab {
    /// 活动标签借用当前面板，其他标签读取各自保存的面板，避免提示停留在创建时的目录。
    ///
    /// @author kongweiguang
    pub(super) fn display_endpoint<'a>(
        &'a self,
        active_panel: &'a BrowserPanel,
    ) -> &'a FileEndpoint {
        self.panel
            .as_ref()
            .map_or(&active_panel.endpoint, |panel| &panel.endpoint)
    }

    /// 本机使用稳定标签键；远端重复标签使用独立序号。
    ///
    /// @author kongweiguang
    pub(super) fn key(&self) -> String {
        if self.id == 0 {
            "local".to_owned()
        } else {
            self.id.to_string()
        }
    }
}

impl FilesPane {
    /// 恢复标签直接采用两个已确认端点；左侧始终保留不可关闭的本机标签。
    ///
    /// @author kongweiguang
    pub(super) fn initialize_host_tabs(&mut self) {
        if !self.host_picker.host_tabs.sides[0].is_empty()
            || self.host_picker.host_tabs.active[0].is_some()
        {
            return;
        }
        let left = self.panels[0].endpoint.clone();
        let left_is_local = matches!(left, FileEndpoint::Local { .. });
        let local = FileEndpoint::Local {
            path: self.local_directories[0].clone(),
        };
        self.host_picker.host_tabs.sides[0].push(HostTab {
            id: 0,
            target: local.clone(),
            dirty: false,
            panel: (!left_is_local).then(|| BrowserPanel::new(local)),
        });
        if left_is_local {
            self.host_picker.host_tabs.active[0] = Some(0);
        } else {
            let id = self.host_picker.host_tabs.next_id();
            self.host_picker.host_tabs.sides[0].push(HostTab {
                id,
                target: left,
                dirty: false,
                panel: None,
            });
            self.host_picker.host_tabs.active[0] = Some(id);
        }

        if !self.host_picker.target_unselected[1] {
            let right = self.panels[1].endpoint.clone();
            self.host_picker.host_tabs.preferred_right_host = right.host_id().cloned();
            let id = if matches!(right, FileEndpoint::Local { .. }) {
                0
            } else {
                self.host_picker.host_tabs.next_id()
            };
            self.host_picker.host_tabs.sides[1].push(HostTab {
                id,
                target: right,
                dirty: false,
                panel: None,
            });
            self.host_picker.host_tabs.active[1] = Some(id);
        }
    }

    /// 远端选择按 main 新建独立标签；每侧的本机入口则复用已有标签。
    ///
    /// @author kongweiguang
    pub(super) fn add_host_tab(
        &mut self,
        side: FilePaneSide,
        endpoint: FileEndpoint,
        cx: &mut Context<Self>,
    ) {
        let index = side.index();
        if matches!(endpoint, FileEndpoint::Local { .. })
            && self.host_picker.host_tabs.sides[index]
                .iter()
                .any(|tab| tab.id == 0)
        {
            self.activate_host_tab(side, 0, cx);
            return;
        }
        let id = if matches!(endpoint, FileEndpoint::Local { .. }) {
            0
        } else {
            self.host_picker.host_tabs.next_id()
        };
        self.host_picker.host_tabs.sides[index].push(HostTab {
            id,
            target: endpoint.clone(),
            dirty: false,
            panel: Some(BrowserPanel::new(endpoint)),
        });
        self.activate_host_tab(side, id, cx);
    }

    /// 切标签时移动整个浏览面板，递增代次并撤销旧回执；尚未完成的目录读取在返回时重发。
    ///
    /// @author kongweiguang
    pub(super) fn activate_host_tab(
        &mut self,
        side: FilePaneSide,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) {
        let index = side.index();
        if self.host_picker.host_tabs.active[index] == Some(tab_id) {
            self.cancel_pending_host_selection(side, cx);
            self.activate_side(index, cx);
            cx.notify();
            return;
        }
        let Some(position) = self.host_picker.host_tabs.sides[index]
            .iter()
            .position(|tab| tab.id == tab_id)
        else {
            return;
        };
        let needs_refresh = self.host_picker.host_tabs.sides[index][position].dirty;
        let Some(next_panel) = self.host_picker.host_tabs.sides[index][position]
            .panel
            .take()
        else {
            return;
        };
        self.cancel_pending_host_selection(side, cx);
        self.status = None;
        self.cancel_pending_side_actions(index, cx);
        let previous_id = self.host_picker.host_tabs.active[index];
        let mut previous = std::mem::replace(&mut self.panels[index], next_panel);
        let generation = self.host_picker.host_tabs.panel_generations[index]
            .max(previous.generation)
            .max(self.panels[index].generation)
            .wrapping_add(1)
            .max(1);
        self.host_picker.host_tabs.panel_generations[index] = generation;
        previous.generation = generation;
        self.panels[index].generation = generation;
        self.panels[index].tree.rebind_generation(generation);
        if let Some(requested) = previous.requested_endpoint.take()
            && previous.endpoint != requested
        {
            previous.endpoint = requested;
            previous.listing = None;
            previous.selected = None;
            previous.selection.clear();
            previous.keyboard_focus.clear();
            previous.tree.cancel_pending_requests();
            previous.tree = super::tree::FileTreeBrowser::default();
            previous.tree_widget = None;
        }
        previous.loading = false;
        previous.tree.cancel_pending_requests();
        if let Some(previous_id) = previous_id
            && let Some(tab) = self.host_picker.host_tabs.sides[index]
                .iter_mut()
                .find(|tab| tab.id == previous_id)
        {
            tab.panel = Some(previous);
        }
        self.host_picker.host_tabs.active[index] = Some(tab_id);
        self.host_picker.target_unselected[index] = false;
        self.host_picker.target_errors[index] = None;
        self.host_picker.expanded_side = None;
        self.active_side = index;
        self.sync_target_name_requests(cx);
        self.load_bookmarks(index, cx);
        let ready = self.panels[index]
            .listing
            .as_ref()
            .is_some_and(|listing| listing.directory == self.panels[index].endpoint);
        if !ready || needs_refresh {
            self.load_panel(index, cx);
        }
        self.emit_transfer_endpoints_changed(cx);
        cx.notify();
    }

    /// 关闭活动左侧远端回到本机；右侧优先原始主机、再选第一个剩余标签。
    ///
    /// @author kongweiguang
    pub(super) fn close_host_tab(
        &mut self,
        side: FilePaneSide,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) {
        let index = side.index();
        if side == FilePaneSide::Left && tab_id == 0 {
            return;
        }
        if !self.host_picker.host_tabs.sides[index]
            .iter()
            .any(|tab| tab.id == tab_id)
        {
            return;
        }
        self.cancel_pending_host_selection(side, cx);
        if self.host_picker.host_tabs.active[index] == Some(tab_id) {
            let fallback = if side == FilePaneSide::Left {
                Some(0)
            } else {
                let candidates = self.host_picker.host_tabs.sides[index]
                    .iter()
                    .filter(|tab| tab.id != tab_id);
                self.host_picker
                    .host_tabs
                    .preferred_right_host
                    .as_ref()
                    .and_then(|host| {
                        candidates
                            .clone()
                            .find(|tab| tab.target.host_id() == Some(host))
                    })
                    .or_else(|| candidates.into_iter().next())
                    .map(|tab| tab.id)
            };
            if let Some(next_id) = fallback {
                self.activate_host_tab(side, next_id, cx);
            } else {
                self.status = None;
                self.cancel_pending_side_actions(index, cx);
                let local = FileEndpoint::Local {
                    path: self.local_directories[index].clone(),
                };
                let mut old = std::mem::replace(&mut self.panels[index], BrowserPanel::new(local));
                old.tree.cancel_pending_requests();
                self.host_picker.host_tabs.panel_generations[index] =
                    old.generation.wrapping_add(1).max(1);
                self.panels[index].generation = self.host_picker.host_tabs.panel_generations[index];
                self.host_picker.host_tabs.active[index] = None;
                self.host_picker.target_unselected[index] = true;
                self.emit_transfer_endpoints_changed(cx);
            }
        }
        if let Some(position) = self.host_picker.host_tabs.sides[index]
            .iter()
            .position(|tab| tab.id == tab_id)
        {
            self.host_picker.host_tabs.sides[index].remove(position);
        }
        cx.notify();
    }

    /// 刷新主机目录时移除已删除或停用的已保存目标；临时外部连接仍由独立租约管理。
    ///
    /// @author kongweiguang
    pub(super) fn reconcile_host_tabs(&mut self, cx: &mut Context<Self>) {
        let available = self
            .host_picker
            .hosts
            .iter()
            .map(|host| &host.id)
            .collect::<std::collections::HashSet<_>>();
        let stale = self
            .host_picker
            .host_tabs
            .sides
            .iter()
            .enumerate()
            .flat_map(|(side, tabs)| {
                let available = &available;
                tabs.iter().filter_map(move |tab| match &tab.target {
                    FileEndpoint::Remote { host_id, .. }
                        if !host_id.as_str().starts_with("external:")
                            && !available.contains(host_id) =>
                    {
                        FilePaneSide::from_index(side).map(|side| (side, tab.id))
                    }
                    _ => None,
                })
            })
            .collect::<Vec<_>>();
        for (side, tab_id) in stale {
            self.close_host_tab(side, tab_id, cx);
        }
    }

    /// 已确认传输影响的非活动标签标记待回读；切回后仍使用自己的目录和选择。
    ///
    /// @author kongweiguang
    pub(super) fn invalidate_inactive_transfer_directories(
        &mut self,
        scopes: &[TransferRefreshScope],
    ) {
        for tabs in &mut self.host_picker.host_tabs.sides {
            for tab in tabs {
                let Some(panel) = &tab.panel else { continue };
                if panel_needs_transfer_refresh(
                    &panel.endpoint,
                    panel.requested_endpoint.as_ref(),
                    panel.loading,
                    panel.listing.is_some(),
                    scopes,
                ) {
                    tab.dirty = true;
                }
            }
        }
    }

    /// 只在当前标签的读取成功后清除待回读标记，失败后返回该标签仍可重试。
    ///
    /// @author kongweiguang
    pub(super) fn complete_active_host_tab_refresh(&mut self, side: usize) {
        let Some(id) = self
            .host_picker
            .host_tabs
            .active
            .get(side)
            .copied()
            .flatten()
        else {
            return;
        };
        if let Some(tab) = self.host_picker.host_tabs.sides[side]
            .iter_mut()
            .find(|tab| tab.id == id)
        {
            tab.dirty = false;
        }
    }

    /// 按侧撤销尚未提交的面板动作；入队中的传输和已提交文件操作继续使用原目标。
    ///
    /// @author kongweiguang
    fn cancel_pending_side_actions(&mut self, side: usize, cx: &mut Context<Self>) {
        self.cancel_transfer_preparation_for_side(side, cx);
        if self.name_action_side == Some(side) && !self.name_action_busy {
            self.name_action = None;
            self.name_action_side = None;
            self.name_action_target = None;
        }
        if self
            .permission_dialog_target
            .as_ref()
            .is_some_and(|(owner, _)| *owner == side)
            && !self.permissions.busy
        {
            self.permission_dialog_target = None;
        }
        self.delete_confirmation = None;
    }
}
