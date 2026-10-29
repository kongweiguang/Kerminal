//! 固定右栏容器上下文只从活动终端 pane 的真实运行态与同轮服务观察解析。
//!
//! @author kongweiguang

use kerminal_application::{
    sessions::UiSessionBinding,
    workspace::{
        WorkspaceContainerRuntime, WorkspacePaneMode, WorkspaceSessionPane, WorkspaceSessionTab,
        WorkspaceTargetRef,
    },
};
use kerminal_domain::{
    files::ContainerRuntime,
    identity::{HostId, PaneId, SessionId, TabId},
    operations::ContainerTarget,
    sessions::{SessionLifecycle, SessionOwner, SessionTarget},
};

use crate::features::operations::{ContainerPanelContext, ContainerPanelUnavailableReason};

use super::{
    CenterPanel, RootView,
    session_reconnect::SessionPanelObservation,
    workspace_runtime::{RuntimePanePhase, RuntimeTerminalPane},
};

/// 只保留计算目标所需的运行态身份，避免测试和渲染持有 GPUI Entity 或复制 Root 状态。
#[derive(Clone, Copy)]
struct ContainerPanelPane<'a> {
    tab_id: &'a TabId,
    pane_id: &'a PaneId,
    session_id: &'a SessionId,
    target: &'a SessionTarget,
    persisted: &'a WorkspaceSessionPane,
    snapshot_persisted: &'a WorkspaceSessionPane,
    phase: RuntimePanePhase,
    transient: bool,
    external_lease_host_id: Option<&'a HostId>,
}

/// 把 UI 导航、递归布局、运行 pane 与服务观察组成一次不可变解析输入。
struct ContainerPanelCandidate<'a> {
    center_panel: CenterPanel,
    active_tab_id: Option<&'a TabId>,
    active_tab: Option<&'a WorkspaceSessionTab>,
    focused_pane_id: Option<&'a PaneId>,
    pane: Option<ContainerPanelPane<'a>>,
    handler_binding: Option<&'a UiSessionBinding>,
    observation: Option<&'a SessionPanelObservation>,
}

impl RootView {
    /// 固定右栏优先解析活动文件 Tab 的冻结 SSH 宿主，否则沿用活动终端 pane；不借用侧栏选择或标题推断目标。
    ///
    /// @author kongweiguang
    pub(super) fn current_container_panel_context(&self) -> ContainerPanelContext {
        self.current_container_panel_projection().0
    }

    /// 一次校验同时返回容器面板上下文和 Ports 可展示的容器身份，避免不同入口解析出不同 pane。
    ///
    /// @author kongweiguang
    pub(super) fn current_container_panel_projection(
        &self,
    ) -> (ContainerPanelContext, Option<ContainerTarget>) {
        let Some(controller) = self.workspace_controller.as_ref() else {
            return (
                unavailable(ContainerPanelUnavailableReason::NoTerminal),
                None,
            );
        };
        let snapshot = controller.snapshot();
        let Some(active_tab_id) = snapshot.active_tab_id.as_ref() else {
            return (
                unavailable(ContainerPanelUnavailableReason::NoTerminal),
                None,
            );
        };
        let Some(active_tab) = snapshot
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == active_tab_id)
        else {
            return (
                unavailable(ContainerPanelUnavailableReason::NoTerminal),
                None,
            );
        };
        if let Some(context) = workspace_file_host_context(active_tab) {
            return (context, None);
        }
        let Some(layout) = active_tab.terminal_layout() else {
            return (
                unavailable(ContainerPanelUnavailableReason::NoTerminal),
                None,
            );
        };
        let Some(focused_pane_id) = snapshot.focused_pane_id.as_ref() else {
            return (
                unavailable(ContainerPanelUnavailableReason::NoTerminal),
                None,
            );
        };
        if !layout.contains_pane(focused_pane_id) {
            return (
                unavailable(ContainerPanelUnavailableReason::TargetMismatch),
                None,
            );
        }

        let Some(runtime_pane) = self.workspace_runtime.pane(focused_pane_id) else {
            return (unavailable(ContainerPanelUnavailableReason::NotReady), None);
        };
        let Some(snapshot_pane) = snapshot
            .terminal_panes
            .iter()
            .find(|pane| &pane.id == focused_pane_id)
        else {
            return (
                unavailable(ContainerPanelUnavailableReason::TargetMismatch),
                None,
            );
        };
        let Some(session_id) = runtime_pane.session_id.as_ref() else {
            return (unavailable(ContainerPanelUnavailableReason::NotReady), None);
        };
        let Some(target) = runtime_pane.target.as_ref() else {
            return (unavailable(ContainerPanelUnavailableReason::NotReady), None);
        };
        let observation = self.session_reconnect.panel_observation(
            focused_pane_id,
            active_tab_id,
            session_id,
            target,
        );
        let handler_binding = self
            .pane_input_handlers
            .get(session_id)
            .map(|registration| &registration.binding);

        let pane = panel_pane(
            runtime_pane,
            snapshot_pane,
            focused_pane_id,
            session_id,
            target,
        );
        let context = resolve_container_panel_context(ContainerPanelCandidate {
            center_panel: self.center_panel,
            active_tab_id: Some(active_tab_id),
            active_tab: Some(active_tab),
            focused_pane_id: Some(focused_pane_id),
            pane: Some(pane),
            handler_binding,
            observation: observation.as_ref(),
        });
        let ports_target = container_target_for_validated_host_context(&context, pane);
        (context, ports_target)
    }
}

/// WorkspaceFile 直接保留其文件宿主身份，不伪造终端 session binding 或依赖侧栏选择。
///
/// @author kongweiguang
fn workspace_file_host_context(tab: &WorkspaceSessionTab) -> Option<ContainerPanelContext> {
    let WorkspaceSessionTab::WorkspaceFile { id, target, .. } = tab else {
        return None;
    };
    let host_id = match target {
        WorkspaceTargetRef::Ssh { host_id }
        | WorkspaceTargetRef::DockerContainer { host_id, .. } => host_id.clone(),
        WorkspaceTargetRef::Local { .. }
        | WorkspaceTargetRef::Telnet { .. }
        | WorkspaceTargetRef::Serial { .. }
        | WorkspaceTargetRef::Unavailable { .. } => return None,
    };
    Some(ContainerPanelContext::WorkspaceHost {
        tab_id: id.clone(),
        host_id,
    })
}

/// 仅从刚经 Root resolver 校验的 Host binding 取同一 pane 的远程容器目标，并复核持久快照。
///
/// @author kongweiguang
fn container_target_for_validated_host_context(
    context: &ContainerPanelContext,
    pane: ContainerPanelPane<'_>,
) -> Option<ContainerTarget> {
    let ContainerPanelContext::Host {
        binding,
        host_id: Some(host_id),
    } = context
    else {
        return None;
    };
    let SessionOwner::Ui {
        tab_id: bound_tab_id,
        pane_id: bound_pane_id,
    } = &binding.owner
    else {
        return None;
    };
    if pane.phase != RuntimePanePhase::Ready
        || pane.tab_id != bound_tab_id
        || pane.pane_id != bound_pane_id
        || pane.session_id != &binding.session_id
        || !persisted_target_identity_matches(pane.persisted, pane.snapshot_persisted)
        || !persisted_target_matches(pane)
    {
        return None;
    }
    let SessionTarget::Container(target) = pane.target else {
        return None;
    };
    if target.host_id.as_ref() != Some(host_id) {
        return None;
    }
    Some(ContainerTarget {
        host_id: target.host_id.clone(),
        runtime: target.runtime,
        container_id: target.container_id.clone(),
    })
}

/// 将运行态 pane 转成借用快照；调用处先按活动 focused pane 查表，禁止退回布局首项。
///
/// @author kongweiguang
fn panel_pane<'a>(
    pane: &'a RuntimeTerminalPane,
    snapshot_persisted: &'a WorkspaceSessionPane,
    pane_id: &'a PaneId,
    session_id: &'a SessionId,
    target: &'a SessionTarget,
) -> ContainerPanelPane<'a> {
    ContainerPanelPane {
        tab_id: &pane.tab_id,
        pane_id,
        session_id,
        target,
        persisted: &pane.persisted,
        snapshot_persisted,
        phase: pane.phase,
        transient: pane.transient,
        external_lease_host_id: pane.external_lease.as_ref().map(|lease| lease.host_id()),
    }
}

/// 右栏只管理 main 支持的 SSH 宿主；校验 pane 与输入代次后也不把本机容器能力扩展成可见入口。
///
/// @author kongweiguang
fn resolve_container_panel_context(
    candidate: ContainerPanelCandidate<'_>,
) -> ContainerPanelContext {
    // 中心呈现状态不能撤销仍有效的 workspace 活动 pane（例如左栏主机选择会展示 Connections）。
    let _center_presentation = candidate.center_panel;
    let (Some(active_tab_id), Some(active_tab), Some(focused_pane_id)) = (
        candidate.active_tab_id,
        candidate.active_tab,
        candidate.focused_pane_id,
    ) else {
        return unavailable(ContainerPanelUnavailableReason::NoTerminal);
    };
    if active_tab.id() != active_tab_id {
        return unavailable(ContainerPanelUnavailableReason::TargetMismatch);
    }
    let Some(layout) = active_tab.terminal_layout() else {
        return unavailable(ContainerPanelUnavailableReason::NoTerminal);
    };
    if !layout.contains_pane(focused_pane_id) {
        return unavailable(ContainerPanelUnavailableReason::TargetMismatch);
    }
    let Some(pane) = candidate.pane else {
        return unavailable(ContainerPanelUnavailableReason::NotReady);
    };
    if pane.pane_id != focused_pane_id || pane.tab_id != active_tab_id {
        return unavailable(ContainerPanelUnavailableReason::TargetMismatch);
    }
    if pane.phase != RuntimePanePhase::Ready {
        return unavailable(ContainerPanelUnavailableReason::NotReady);
    }
    if !persisted_target_identity_matches(pane.persisted, pane.snapshot_persisted) {
        return unavailable(ContainerPanelUnavailableReason::TargetMismatch);
    }
    let Some(observation) = candidate.observation else {
        return unavailable(ContainerPanelUnavailableReason::NotReady);
    };
    if observation.binding.session_id != *pane.session_id
        || observation.target != *pane.target
        || observation.binding.owner
            != (SessionOwner::Ui {
                tab_id: active_tab_id.clone(),
                pane_id: focused_pane_id.clone(),
            })
    {
        return unavailable(ContainerPanelUnavailableReason::TargetMismatch);
    }
    if !persisted_target_matches(pane) {
        return unavailable(ContainerPanelUnavailableReason::TargetMismatch);
    }

    match pane.target {
        SessionTarget::Local(_) | SessionTarget::Ssh(_) => {
            // 未完成连接可重试；结束态需从终端自己的重连入口恢复，不能管理容器。
            match observation.lifecycle {
                SessionLifecycle::Running => {}
                SessionLifecycle::Starting => {
                    return unavailable(ContainerPanelUnavailableReason::NotReady);
                }
                SessionLifecycle::Exited
                | SessionLifecycle::Failed
                | SessionLifecycle::Closing
                | SessionLifecycle::Closed => {
                    return unavailable(ContainerPanelUnavailableReason::TerminalEnded);
                }
            }
            if !binding_matches_running_handler(candidate.handler_binding, &observation.binding) {
                return unavailable(ContainerPanelUnavailableReason::TargetMismatch);
            }
            let host_id = match pane.target {
                SessionTarget::Local(_) => {
                    return unavailable(ContainerPanelUnavailableReason::UnsupportedProtocol);
                }
                SessionTarget::Ssh(ssh) => Some(ssh.host_id.clone()),
                _ => return unavailable(ContainerPanelUnavailableReason::TargetMismatch),
            };
            ContainerPanelContext::Host {
                binding: observation.binding.clone(),
                host_id,
            }
        }
        SessionTarget::Container(container) => {
            let running = match observation.lifecycle {
                SessionLifecycle::Running => {
                    if !binding_matches_running_handler(
                        candidate.handler_binding,
                        &observation.binding,
                    ) {
                        return unavailable(ContainerPanelUnavailableReason::TargetMismatch);
                    }
                    true
                }
                SessionLifecycle::Exited | SessionLifecycle::Failed => {
                    if candidate
                        .handler_binding
                        .is_some_and(|binding| binding != &observation.binding)
                    {
                        return unavailable(ContainerPanelUnavailableReason::TargetMismatch);
                    }
                    false
                }
                SessionLifecycle::Closed | SessionLifecycle::Closing => {
                    return unavailable(ContainerPanelUnavailableReason::TerminalEnded);
                }
                SessionLifecycle::Starting => {
                    return unavailable(ContainerPanelUnavailableReason::NotReady);
                }
            };
            // 容器终端的右栏仍管理 SSH 宿主，与 main 的 focusedPane host 解析保持一致。
            // 只读详情由列表导航负责，不因打开容器终端改换成第二种右栏页面。
            let _terminal_running = running;
            let Some(host_id) = container.host_id.as_ref() else {
                return unavailable(ContainerPanelUnavailableReason::UnsupportedProtocol);
            };
            ContainerPanelContext::Host {
                binding: observation.binding.clone(),
                host_id: Some(host_id.clone()),
            }
        }
        SessionTarget::Telnet(_) | SessionTarget::Serial(_) => {
            if observation.lifecycle != SessionLifecycle::Running {
                return unavailable(ContainerPanelUnavailableReason::NotReady);
            }
            if !binding_matches_running_handler(candidate.handler_binding, &observation.binding) {
                return unavailable(ContainerPanelUnavailableReason::TargetMismatch);
            }
            unavailable(ContainerPanelUnavailableReason::UnsupportedProtocol)
        }
    }
}

/// 只接受与真实 SessionTarget 无损一致的 v3 持久目标；本机临时容器必须由 transient 证明。
///
/// @author kongweiguang
fn persisted_target_matches(pane: ContainerPanelPane<'_>) -> bool {
    let persisted = pane.persisted;
    match pane.target {
        SessionTarget::Local(_) => {
            persisted.mode == WorkspacePaneMode::Local
                && matches!(
                    persisted.target.as_ref(),
                    Some(WorkspaceTargetRef::Local { .. })
                )
                && persisted.remote_host_id.is_none()
                && persisted.container_id.is_none()
                && pane.external_lease_host_id.is_none()
        }
        SessionTarget::Ssh(ssh) => {
            persisted.mode == WorkspacePaneMode::Ssh
                && matches!(
                    persisted.target.as_ref(),
                    Some(WorkspaceTargetRef::Ssh { host_id }) if host_id == &ssh.host_id
                )
                && persisted.remote_host_id.as_ref() == Some(&ssh.host_id)
                && persisted.container_id.is_none()
                && external_lease_matches(pane.external_lease_host_id, &ssh.host_id)
        }
        SessionTarget::Container(container) => match container.host_id.as_ref() {
            None => {
                persisted.mode == WorkspacePaneMode::Container
                    && persisted.target.is_none()
                    && persisted.remote_host_id.is_none()
                    && persisted.container_id.as_deref() == Some(container.container_id.as_str())
                    && pane.transient
                    && pane.external_lease_host_id.is_none()
            }
            Some(host_id) => {
                persisted.mode == WorkspacePaneMode::Container
                    && matches!(
                        persisted.target.as_ref(),
                        Some(WorkspaceTargetRef::DockerContainer {
                            host_id: persisted_host_id,
                            container_id,
                            runtime,
                            ..
                        }) if persisted_host_id == host_id
                            && container_id == &container.container_id
                            && workspace_runtime(*runtime) == container.runtime
                    )
                    && persisted.remote_host_id.as_ref() == Some(host_id)
                    && persisted.container_id.as_deref() == Some(container.container_id.as_str())
                    && external_lease_matches(pane.external_lease_host_id, host_id)
            }
        },
        SessionTarget::Telnet(_) => {
            persisted.mode == WorkspacePaneMode::Telnet
                && persisted.target.is_none()
                && persisted.remote_host_id.is_none()
                && pane.transient
                && pane.external_lease_host_id.is_none()
        }
        SessionTarget::Serial(_) => {
            persisted.mode == WorkspacePaneMode::Serial
                && persisted.target.is_none()
                && persisted.remote_host_id.is_none()
                && pane.transient
                && pane.external_lease_host_id.is_none()
        }
    }
}

/// 对齐两个 pane 的稳定连接身份字段，忽略标题、状态和当前目录等可独立变化的展示元数据。
///
/// @author kongweiguang
fn persisted_target_identity_matches(
    runtime: &WorkspaceSessionPane,
    snapshot: &WorkspaceSessionPane,
) -> bool {
    if runtime.id != snapshot.id
        || runtime.mode != snapshot.mode
        || runtime.remote_host_id != snapshot.remote_host_id
        || runtime.container_id != snapshot.container_id
    {
        return false;
    }
    match (runtime.target.as_ref(), snapshot.target.as_ref()) {
        (
            Some(WorkspaceTargetRef::Local {
                profile_id: runtime_profile,
            }),
            Some(WorkspaceTargetRef::Local {
                profile_id: snapshot_profile,
            }),
        ) => runtime_profile == snapshot_profile,
        (
            Some(WorkspaceTargetRef::Ssh {
                host_id: runtime_host,
            }),
            Some(WorkspaceTargetRef::Ssh {
                host_id: snapshot_host,
            }),
        )
        | (
            Some(WorkspaceTargetRef::Telnet {
                host_id: runtime_host,
            }),
            Some(WorkspaceTargetRef::Telnet {
                host_id: snapshot_host,
            }),
        )
        | (
            Some(WorkspaceTargetRef::Serial {
                host_id: runtime_host,
            }),
            Some(WorkspaceTargetRef::Serial {
                host_id: snapshot_host,
            }),
        ) => runtime_host == snapshot_host,
        (
            Some(WorkspaceTargetRef::DockerContainer {
                host_id: runtime_host,
                container_id: runtime_container,
                runtime: runtime_kind,
                ..
            }),
            Some(WorkspaceTargetRef::DockerContainer {
                host_id: snapshot_host,
                container_id: snapshot_container,
                runtime: snapshot_kind,
                ..
            }),
        ) => {
            runtime_host == snapshot_host
                && runtime_container == snapshot_container
                && runtime_kind == snapshot_kind
        }
        (
            Some(WorkspaceTargetRef::Unavailable {
                raw: runtime_raw,
                reason: runtime_reason,
            }),
            Some(WorkspaceTargetRef::Unavailable {
                raw: snapshot_raw,
                reason: snapshot_reason,
            }),
        ) => runtime_raw == snapshot_raw && runtime_reason == snapshot_reason,
        (None, None) => true,
        _ => false,
    }
}

/// 持久主机可无 lease，external host 则必须仍由对应的 pane lease 持有。
///
/// @author kongweiguang
fn external_lease_matches(lease_host_id: Option<&HostId>, host_id: &HostId) -> bool {
    match lease_host_id {
        Some(lease_host_id) => lease_host_id == host_id,
        None => !host_id.as_str().starts_with("external:"),
    }
}

/// 把 v3 运行时标签转换成同一领域 runtime 枚举，避免目标比较时默认 Docker。
///
/// @author kongweiguang
fn workspace_runtime(runtime: WorkspaceContainerRuntime) -> ContainerRuntime {
    match runtime {
        WorkspaceContainerRuntime::Docker => ContainerRuntime::Docker,
        WorkspaceContainerRuntime::Podman => ContainerRuntime::Podman,
    }
}

/// Running 目标必须仍由 Root 的同一 handler 代次持有，不能只按 SessionId 通过。
///
/// @author kongweiguang
fn binding_matches_running_handler(
    handler_binding: Option<&UiSessionBinding>,
    observation_binding: &UiSessionBinding,
) -> bool {
    handler_binding == Some(observation_binding)
}

/// 统一构造无目标状态，确保所有错误分支都携带具体且稳定的原因。
///
/// @author kongweiguang
fn unavailable(reason: ContainerPanelUnavailableReason) -> ContainerPanelContext {
    ContainerPanelContext::Unavailable { reason }
}

#[cfg(test)]
#[path = "../../tests/unit/root_container_panel.rs"]
mod tests;
