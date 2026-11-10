//! 整组关闭先收集逐文档授权，最终复核后一次提交，正文仍只归共享编辑器所有。
//!
//! @author kongweiguang

use gpui_kit::Context;
use kerminal_domain::files::{FileEndpoint, FileRevision, editor::EditorPhase};

use super::workspace::{
    EditorTab, FileEditorWorkspace, FileEditorWorkspaceEvent, SearchPanelMode,
    confirmation_endpoint,
};

/// 授权票据只是关闭期间的不可变核对材料，不创建另一份可编辑正文或保存 owner。
#[derive(Clone, Debug, PartialEq, Eq)]
struct CloseSnapshot {
    load_generation: u64,
    document_generation: u64,
    revision: Option<FileRevision>,
    content: Option<String>,
    dirty: bool,
    loading: bool,
    phase: Option<EditorPhase>,
}

impl CloseSnapshot {
    /// 精确文本核对补足尚未分发的 InputEvent，防止丢弃授权误涵盖保存期间或之后的新输入。
    ///
    /// @author kongweiguang
    fn capture(tab: &EditorTab, cx: &gpui_kit::App) -> Self {
        let content = tab
            .editor
            .as_ref()
            .map(|editor| editor.read(cx).text().to_string());
        Self {
            load_generation: tab.load_generation,
            document_generation: tab.document_generation,
            revision: tab
                .session
                .as_ref()
                .map(|session| session.revision().clone()),
            dirty: tab.dirty
                || tab
                    .session
                    .as_ref()
                    .zip(content.as_deref())
                    .is_some_and(|(session, content)| session.is_dirty(content)),
            content,
            loading: tab.loading,
            phase: tab.session.as_ref().map(|session| session.phase()),
        }
    }

    /// 未知提交、在途保存及替换已有正文的加载都不能以丢弃票据绕过原保护。
    ///
    /// @author kongweiguang
    fn permits_close(&self, discard: bool) -> bool {
        !matches!(
            self.phase,
            Some(EditorPhase::Saving | EditorPhase::OutcomeUnknown)
        ) && !(self.loading && self.phase.is_some())
            && (!self.dirty || discard)
    }
}

/// 保留最初的文件代次；读回只有经原确认绑定的新代次才允许替换当前成员身份。
struct GroupMember {
    endpoint: FileEndpoint,
    load_generation: Option<u64>,
    approval: Option<(Option<CloseSnapshot>, bool)>,
}

/// 整组拥有单一 operation 回执；逐文件批准从不向 Root 发出物理关闭事件。
pub(super) struct PendingWorkspaceGroupClose {
    operation_id: String,
    generation: u64,
    members: Vec<GroupMember>,
    current: usize,
    /// 同步干净文件用循环推进，避免文档数量直接变成调用栈深度。
    advancing: bool,
    /// 授权已收集但正文仍归编辑器所有，等 Root 关闭工具并提交工作区后才可释放。
    prepared: bool,
}

impl FileEditorWorkspace {
    /// 先串行复用单文件保护，全部授权后才移除；已有关闭/保存 owner 不被新的组请求吞掉。
    ///
    /// @author kongweiguang
    pub fn request_workspace_group_close(
        &mut self,
        operation_id: String,
        endpoints: Vec<FileEndpoint>,
        cx: &mut Context<Self>,
    ) {
        let mut unique = Vec::new();
        for endpoint in endpoints {
            if !unique.contains(&endpoint) {
                unique.push(endpoint);
            }
        }
        if let Some(group) = &self.pending_workspace_group_close {
            let same_request = group.operation_id == operation_id
                && group
                    .members
                    .iter()
                    .map(|member| &member.endpoint)
                    .eq(unique.iter());
            if same_request {
                // 控制器失败重试复用 operation id；只重发准备回执让 Root 继续工具与提交阶段。
                if group.prepared {
                    cx.emit(FileEditorWorkspaceEvent::WorkspaceGroupClosePrepared {
                        operation_id: group.operation_id.clone(),
                    });
                }
                return;
            }
            if !group.prepared {
                cx.emit(FileEditorWorkspaceEvent::WorkspaceGroupCloseResolved {
                    operation_id,
                    closed: false,
                });
                return;
            }
        }
        // 已准备组对应的旧尝试已失败或被放弃；新操作撤销旧授权，避免永久占用组槽。
        self.cancel_workspace_group_close(cx);
        if self.pending_workspace_close.is_some()
            || self.confirmation.is_some()
            || self.quit_flow
            || self
                .tabs
                .iter()
                .any(|tab| tab.save_operation.is_some() || tab.close_after_readback.is_pending())
        {
            cx.emit(FileEditorWorkspaceEvent::WorkspaceGroupCloseResolved {
                operation_id,
                closed: false,
            });
            return;
        }
        let members = unique
            .into_iter()
            .map(|endpoint| GroupMember {
                load_generation: self
                    .tabs
                    .iter()
                    .find(|tab| tab.endpoint == endpoint)
                    .map(|tab| tab.load_generation),
                endpoint,
                approval: None,
            })
            .collect();
        let generation = self.next_workspace_group_generation;
        self.next_workspace_group_generation = generation.wrapping_add(1).max(1);
        self.pending_workspace_group_close = Some(PendingWorkspaceGroupClose {
            operation_id,
            generation,
            members,
            current: 0,
            advancing: false,
            prepared: false,
        });
        self.advance_workspace_group_close(cx);
    }

    /// 单文件/退出抢占只撤销组的关闭意图；已提交保存与在途读回继续按原任务结算，不伪造取消。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_workspace_group_close(&mut self, cx: &mut Context<Self>) {
        let Some(group) = self.pending_workspace_group_close.take() else {
            return;
        };
        if self.confirmation.as_ref().is_some_and(|confirmation| {
            group
                .members
                .get(group.current)
                .is_some_and(|member| &member.endpoint == confirmation_endpoint(confirmation))
        }) {
            self.confirmation = None;
        }
        for tab in &mut self.tabs {
            if group
                .members
                .iter()
                .any(|member| member.endpoint == tab.endpoint)
            {
                tab.close_after_readback.clear();
            }
        }
        cx.emit(FileEditorWorkspaceEvent::WorkspaceGroupCloseResolved {
            operation_id: group.operation_id,
            closed: false,
        });
        cx.notify();
    }

    /// 只有当前逐文件步骤的真实 endpoint/读取代次可以取消同一组 owner，旧任务不得误结新组。
    ///
    /// @author kongweiguang
    pub(super) fn workspace_group_close_matches(
        &self,
        endpoint: &FileEndpoint,
        generation: Option<u64>,
    ) -> bool {
        self.pending_workspace_group_close
            .as_ref()
            .and_then(|group| group.members.get(group.current))
            .is_some_and(|member| {
                &member.endpoint == endpoint && member.load_generation == generation
            })
    }

    /// 读回代次只在原确认确实绑定加载时更新，普通重试不能继承关闭授权。
    ///
    /// @author kongweiguang
    pub(super) fn bind_workspace_group_readback(
        &mut self,
        endpoint: &FileEndpoint,
        generation: u64,
    ) {
        if let Some(member) = self
            .pending_workspace_group_close
            .as_mut()
            .and_then(|group| group.members.get_mut(group.current))
            .filter(|member| &member.endpoint == endpoint)
        {
            member.load_generation = Some(generation);
        }
    }

    /// 保存收据携带内部组代次，即使外层重用 operation id，旧收据也不能获得新组的关闭权限。
    ///
    /// @author kongweiguang
    pub(super) fn workspace_group_save_generation(&self, endpoint: &FileEndpoint) -> Option<u64> {
        self.pending_workspace_group_close
            .as_ref()
            .filter(|group| {
                group
                    .members
                    .get(group.current)
                    .is_some_and(|member| &member.endpoint == endpoint)
            })
            .map(|group| group.generation)
    }

    /// remove_tab 的组内含义仅是批准当前快照；丢弃也不提前改写正文，第二项取消可保留第一项。
    ///
    /// @author kongweiguang
    pub(super) fn approve_workspace_group_tab(
        &mut self,
        endpoint: &FileEndpoint,
        discard: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(group) = self.pending_workspace_group_close.as_ref() else {
            return false;
        };
        let Some(member) = group.members.get(group.current) else {
            return false;
        };
        if &member.endpoint != endpoint {
            // 非当前成员的物理移除是另一关闭意图，明确撤销整组而不吞掉该单项请求。
            self.cancel_workspace_group_close(cx);
            return false;
        }
        let snapshot = self
            .tabs
            .iter()
            .find(|tab| &tab.endpoint == endpoint)
            .map(|tab| CloseSnapshot::capture(tab, cx));
        if snapshot.as_ref().map(|snapshot| snapshot.load_generation) != member.load_generation
            || snapshot
                .as_ref()
                .is_some_and(|snapshot| !snapshot.permits_close(discard))
        {
            self.cancel_workspace_group_close(cx);
            return true;
        }
        let Some(group) = self.pending_workspace_group_close.as_mut() else {
            return true;
        };
        group.members[group.current].approval = Some((snapshot, discard));
        group.current += 1;
        let advancing = group.advancing;
        self.confirmation = None;
        if !advancing {
            self.advance_workspace_group_close(cx);
        }
        true
    }

    /// 干净文件同步循环、脏文件等待原确认；最终复核不通过则保留整组，让用户显式重试。
    ///
    /// @author kongweiguang
    fn advance_workspace_group_close(&mut self, cx: &mut Context<Self>) {
        if let Some(group) = self.pending_workspace_group_close.as_mut() {
            if group.prepared || group.advancing {
                return;
            }
            group.advancing = true;
        }
        loop {
            let Some(group) = self.pending_workspace_group_close.as_ref() else {
                return;
            };
            let Some(member) = group.members.get(group.current) else {
                self.commit_workspace_group_close(cx);
                return;
            };
            let endpoint = member.endpoint.clone();
            let current = group.current;
            if member.load_generation.is_none() {
                self.approve_workspace_group_tab(&endpoint, false, cx);
            } else {
                self.active_endpoint = Some(endpoint.clone());
                self.publish_workspace_projection_if_changed(cx);
                self.request_close_impl(endpoint, cx);
            }
            let Some(group) = self.pending_workspace_group_close.as_mut() else {
                return;
            };
            if group.current == current {
                group.advancing = false;
                cx.notify();
                return;
            }
        }
    }

    /// 同一 UI 更新内先验证所有票据再标记准备，一项变化都不允许先删其它已批准成员。
    /// 正文此时仍完整保留，Root 关闭工具或提交工作区失败时可原样恢复。
    ///
    /// @author kongweiguang
    fn commit_workspace_group_close(&mut self, cx: &mut Context<Self>) {
        let Some(group) = self.pending_workspace_group_close.as_ref() else {
            return;
        };
        if group.prepared {
            return;
        }
        let valid = group.members.iter().all(|member| {
            let current = self
                .tabs
                .iter()
                .find(|tab| tab.endpoint == member.endpoint)
                .map(|tab| CloseSnapshot::capture(tab, cx));
            member.approval.as_ref().is_some_and(|(approved, discard)| {
                approved == &current
                    && current
                        .as_ref()
                        .is_none_or(|snapshot| snapshot.permits_close(*discard))
            })
        });
        if !valid {
            self.cancel_workspace_group_close(cx);
            return;
        }
        let Some(group) = self.pending_workspace_group_close.as_mut() else {
            return;
        };
        group.prepared = true;
        group.advancing = false;
        let operation_id = group.operation_id.clone();
        cx.emit(FileEditorWorkspaceEvent::WorkspaceGroupClosePrepared { operation_id });
        cx.notify();
    }

    /// Root 依据该查询区分“仅供参考的组准备”与旧单文件已物理移除的完成回执。
    ///
    /// @author kongweiguang
    pub fn has_prepared_workspace_group_close(&self, operation_id: &str) -> bool {
        self.pending_workspace_group_close
            .as_ref()
            .is_some_and(|group| group.prepared && group.operation_id == operation_id)
    }

    /// 释放前重新核对正文、代次与保存状态，等待工具关闭期间的新输入不能被组票据误丢。
    ///
    /// @author kongweiguang
    pub fn revalidate_workspace_group_close(&self, operation_id: &str, cx: &gpui_kit::App) -> bool {
        self.pending_workspace_group_close
            .as_ref()
            .is_some_and(|group| {
                group.prepared
                    && group.operation_id == operation_id
                    && group.members.iter().all(|member| {
                        let current = self
                            .tabs
                            .iter()
                            .find(|tab| tab.endpoint == member.endpoint)
                            .map(|tab| CloseSnapshot::capture(tab, cx));
                        member.approval.as_ref().is_some_and(|(approved, discard)| {
                            approved == &current
                                && current
                                    .as_ref()
                                    .is_none_or(|snapshot| snapshot.permits_close(*discard))
                        })
                    })
            })
    }

    /// 工具与工作区提交都成功后才真正移除正文；核对失败按撤销处理，不留下半组标签。
    ///
    /// @author kongweiguang
    pub fn release_workspace_group_close(
        &mut self,
        operation_id: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let prepared = self
            .pending_workspace_group_close
            .as_ref()
            .is_some_and(|group| group.prepared && group.operation_id == operation_id);
        if !prepared {
            return false;
        }
        if !self.revalidate_workspace_group_close(operation_id, cx) {
            self.cancel_workspace_group_close(cx);
            return false;
        }
        let Some(group) = self.pending_workspace_group_close.take() else {
            return false;
        };
        self.clear_search_projection(cx);
        self.tabs.retain(|tab| {
            !group
                .members
                .iter()
                .any(|member| member.endpoint == tab.endpoint)
        });
        if self.active_endpoint.as_ref().is_some_and(|endpoint| {
            group
                .members
                .iter()
                .any(|member| &member.endpoint == endpoint)
        }) {
            self.active_endpoint = self.tabs.last().map(|tab| tab.endpoint.clone());
        }
        self.confirmation = None;
        self.sync_target_name_requests(cx);
        if self.tabs.is_empty() {
            self.search_panel_mode = SearchPanelMode::Closed;
            if matches!(
                self.status,
                Some(super::workspace::EditorStatus::SaveCompleted)
            ) {
                self.status = None;
            }
        } else if self.search_visible() {
            let query = self.current_search_query(cx);
            self.sync_search_query(query, cx);
        }
        self.publish_workspace_projection_if_changed(cx);
        cx.emit(FileEditorWorkspaceEvent::WorkspaceGroupCloseResolved {
            operation_id: group.operation_id,
            closed: true,
        });
        cx.notify();
        true
    }

    /// 工具取消、超时或工作区提交失败时撤销准备态；正文和编辑状态保持原样供用户重试。
    ///
    /// @author kongweiguang
    pub fn abort_prepared_workspace_group_close(
        &mut self,
        operation_id: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let prepared = self
            .pending_workspace_group_close
            .as_ref()
            .is_some_and(|group| group.prepared && group.operation_id == operation_id);
        if !prepared {
            return false;
        }
        self.cancel_workspace_group_close(cx);
        true
    }
}
