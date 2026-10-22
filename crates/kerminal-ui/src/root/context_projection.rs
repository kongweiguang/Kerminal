//! Root 到 Context Inspector 的安全只读投影。
//!
//! @author kongweiguang

use std::path::Path;

use crate::features::context::{
    ContextConnectionKind, ContextConnectionStatus, ContextDiagnosticSeverity,
    ContextDiagnosticSummary, ContextLocationConfidence, ContextLocationSource,
    ContextLocationSummary, ContextPaneSummary, ContextProjectionStatus, ContextRuntimeSummary,
    ContextSourceStatus, ContextSourceSummary, ContextTabSummary, ContextTargetSummary,
    ContextTerminalLinkIndex, ContextWorkspaceProjection, terminal_link_identity_matches,
};
use kerminal_application::workspace::{
    WorkspacePaneMode, WorkspaceSessionPane, WorkspaceSessionTab, WorkspaceTargetRef,
};
use kerminal_domain::{
    identity::{HostId, PaneId, TabId},
    sessions::SessionTarget,
};

use super::{RootView, workspace_runtime::RuntimePanePhase};

impl RootView {
    /// 从已验证 workspace 和真实 pane owner 组装快照；不从终端输出或命令行推断上下文。
    ///
    /// @author kongweiguang
    pub(crate) fn context_workspace_projection(&self) -> ContextWorkspaceProjection {
        let Some(controller) = self.workspace_controller.as_ref() else {
            let mut projection = ContextWorkspaceProjection::unavailable();
            projection.selected_host_id = self.selected_host_id.clone();
            projection.status = if self.workspace_load_pending {
                ContextProjectionStatus::Restoring
            } else if self.workspace_session_error.is_some() {
                ContextProjectionStatus::Unavailable
            } else {
                ContextProjectionStatus::NoWorkspace
            };
            projection.sources[0].status = if self.workspace_load_pending {
                ContextSourceStatus::Loading
            } else if self.workspace_session_error.is_some() {
                ContextSourceStatus::Error
            } else {
                ContextSourceStatus::Unavailable
            };
            projection.diagnostics = self.workspace_load_diagnostic();
            return projection;
        };

        let snapshot = controller.snapshot();
        let active_tab_id = snapshot.active_tab_id.clone();
        let active_tab = active_tab_id
            .as_ref()
            .and_then(|tab_id| snapshot.terminal_tabs.iter().find(|tab| tab.id() == tab_id));
        let active_tab_title = active_tab.map(|tab| safe_display_text(tab.title()));
        let tabs = snapshot
            .terminal_tabs
            .iter()
            .map(|tab| ContextTabSummary {
                tab_id: tab.id().clone(),
                title: safe_display_text(tab.title()),
                kind: tab_kind(tab),
                machine_id: tab_machine_id(tab).map(safe_display_text),
                active: active_tab_id.as_ref() == Some(tab.id()),
            })
            .collect::<Vec<_>>();

        let pane_id = active_tab
            .and_then(WorkspaceSessionTab::terminal_layout)
            .and_then(|layout| {
                snapshot
                    .focused_pane_id
                    .as_ref()
                    .filter(|focused| layout.contains_pane(focused))
                    .cloned()
                    .or_else(|| layout.pane_ids().into_iter().next())
            });
        let persisted_pane = pane_id.as_ref().and_then(|pane_id| {
            snapshot
                .terminal_panes
                .iter()
                .find(|pane| &pane.id == pane_id)
        });
        let runtime_pane = pane_id
            .as_ref()
            .and_then(|pane_id| self.workspace_runtime.pane(pane_id));

        let active_pane = persisted_pane
            .zip(active_tab.map(WorkspaceSessionTab::id))
            .map(|(persisted, tab_id)| project_pane(persisted, runtime_pane, tab_id));
        let active_target = runtime_pane
            .and_then(|runtime| runtime.target.as_ref())
            .map(session_target_summary)
            .or_else(|| {
                persisted_pane
                    .and_then(|pane| pane.target.as_ref())
                    .map(workspace_target_summary)
            })
            .or_else(|| active_tab.and_then(tab_target_summary));
        let location = match (persisted_pane, active_pane.as_ref()) {
            (Some(persisted), Some(pane)) => pane_location(persisted, pane),
            _ => active_tab_location(active_tab),
        };
        let runtime = runtime_summary(
            snapshot.terminal_panes.len(),
            tabs.len(),
            &self.workspace_runtime,
        );

        let mut diagnostics = self.workspace_load_diagnostic();
        if let (Some(persisted), Some(runtime_pane)) = (persisted_pane, runtime_pane) {
            match runtime_pane.phase {
                RuntimePanePhase::RestorePending => diagnostics.push(ContextDiagnosticSummary {
                    id: "active-pane-restoring".to_owned(),
                    code: "pane_restoring".to_owned(),
                    summary: "当前窗格正在恢复连接。".to_owned(),
                    severity: ContextDiagnosticSeverity::Info,
                }),
                RuntimePanePhase::RestoreFailed | RuntimePanePhase::CloseFailed => {
                    diagnostics.push(ContextDiagnosticSummary {
                        id: "active-pane-unavailable".to_owned(),
                        code: "pane_unavailable".to_owned(),
                        summary: "当前窗格资源不可用；可从工作区状态重试或重新连接。".to_owned(),
                        severity: ContextDiagnosticSeverity::Warning,
                    });
                }
                RuntimePanePhase::Closing => diagnostics.push(ContextDiagnosticSummary {
                    id: "active-pane-closing".to_owned(),
                    code: "pane_closing".to_owned(),
                    summary: "当前窗格正在关闭。".to_owned(),
                    severity: ContextDiagnosticSeverity::Info,
                }),
                RuntimePanePhase::Ready if persisted.target.is_none() => {
                    diagnostics.push(ContextDiagnosticSummary {
                        id: "active-target-unresolved".to_owned(),
                        code: "target_unavailable".to_owned(),
                        summary: "当前窗格没有可解析的持久化目标。".to_owned(),
                        severity: ContextDiagnosticSeverity::Warning,
                    });
                }
                RuntimePanePhase::Ready => {}
            }
        } else if active_tab
            .and_then(WorkspaceSessionTab::terminal_layout)
            .is_some()
            && pane_id.is_some()
        {
            diagnostics.push(ContextDiagnosticSummary {
                id: "active-pane-state-missing".to_owned(),
                code: "pane_state_unavailable".to_owned(),
                summary: "活动窗格的运行状态尚未就绪。".to_owned(),
                severity: ContextDiagnosticSeverity::Warning,
            });
        }

        let status = if self.workspace_load_pending {
            ContextProjectionStatus::Restoring
        } else if self.workspace_session_error.is_some()
            || diagnostics.iter().any(|diagnostic| {
                diagnostic.severity == ContextDiagnosticSeverity::Warning
                    || diagnostic.severity == ContextDiagnosticSeverity::Error
            })
        {
            ContextProjectionStatus::Partial
        } else {
            ContextProjectionStatus::Ready
        };
        let machine_id = active_pane
            .as_ref()
            .and(persisted_pane)
            .map(|persisted| persisted.machine_id.clone())
            .or_else(|| active_tab.and_then(tab_machine_id).map(str::to_owned))
            .or_else(|| snapshot.selected_machine_id.clone());
        let terminal_runtime_source_status = match active_pane.as_ref().map(|pane| pane.status) {
            Some(ContextConnectionStatus::Ready) => ContextSourceStatus::Available,
            Some(ContextConnectionStatus::Restoring | ContextConnectionStatus::Starting) => {
                ContextSourceStatus::Loading
            }
            Some(ContextConnectionStatus::Closing) => ContextSourceStatus::Stale,
            Some(ContextConnectionStatus::Failed | ContextConnectionStatus::Offline) => {
                ContextSourceStatus::Error
            }
            None => ContextSourceStatus::Unavailable,
        };

        ContextWorkspaceProjection {
            status,
            subject_title: active_pane
                .as_ref()
                .map(|pane| pane.title.clone())
                .or(active_tab_title)
                .unwrap_or_else(|| "当前工作区".to_owned()),
            machine_id,
            selected_host_id: self.selected_host_id.clone(),
            tabs,
            active_tab_id,
            active_target,
            location,
            active_pane,
            runtime,
            sources: vec![
                ContextSourceSummary {
                    source: "工作区".to_owned(),
                    status: if self.workspace_load_pending {
                        ContextSourceStatus::Loading
                    } else if self.workspace_session_error.is_some() {
                        ContextSourceStatus::Error
                    } else {
                        ContextSourceStatus::Available
                    },
                    updated_at: None,
                },
                ContextSourceSummary {
                    source: "终端运行态".to_owned(),
                    status: terminal_runtime_source_status,
                    updated_at: None,
                },
            ],
            diagnostics,
            terminal_links: terminal_link_projection(self, pane_id.as_ref(), runtime_pane),
        }
    }

    /// 只按固定 workspace 错误类别提示；错误正文可能含系统路径或 transport 细节。
    fn workspace_load_diagnostic(&self) -> Vec<ContextDiagnosticSummary> {
        if self.workspace_load_pending {
            return vec![ContextDiagnosticSummary {
                id: "workspace-loading".to_owned(),
                code: "workspace_loading".to_owned(),
                summary: "正在读取并恢复工作区。".to_owned(),
                severity: ContextDiagnosticSeverity::Info,
            }];
        }
        if self.workspace_session_error.is_some() {
            return vec![ContextDiagnosticSummary {
                id: "workspace-read-failed".to_owned(),
                code: "workspace_unavailable".to_owned(),
                summary: "工作区读取未完成；原数据保持不变。".to_owned(),
                severity: ContextDiagnosticSeverity::Error,
            }];
        }
        Vec::new()
    }
}

/// 只投影 Root 已接受的 pane 快照；当前 binding、视口或代次不匹配时保留为不可点击的过期项。
///
/// @author kongweiguang
fn terminal_link_projection(
    root: &RootView,
    pane_id: Option<&PaneId>,
    runtime_pane: Option<&super::workspace_runtime::RuntimeTerminalPane>,
) -> ContextTerminalLinkIndex {
    let (Some(pane_id), Some(runtime_pane)) = (pane_id, runtime_pane) else {
        return ContextTerminalLinkIndex::unavailable();
    };
    let Some(snapshot) = root.terminal_link_snapshot_for_context(pane_id) else {
        return if matches!(runtime_pane.phase, RuntimePanePhase::Ready)
            && !runtime_pane.last_snapshot_error
        {
            ContextTerminalLinkIndex::loading()
        } else {
            ContextTerminalLinkIndex::unavailable()
        };
    };

    let is_current = root.center_panel == super::CenterPanel::Terminal
        && matches!(runtime_pane.phase, RuntimePanePhase::Ready)
        && runtime_pane
            .session_id
            .as_ref()
            .and_then(|session_id| {
                let current = runtime_pane.snapshot.as_ref()?;
                let registration = root.pane_input_handlers.get(session_id)?;
                Some(
                    terminal_link_identity_matches(
                        &snapshot.identity,
                        &registration.binding,
                        session_id,
                        current.revision,
                        current.scrollback_offset,
                        current.size,
                    ) && current.session_id.as_str() == session_id.as_str(),
                )
            })
            .unwrap_or(false);

    ContextTerminalLinkIndex::from_snapshot(&snapshot, is_current)
}

/// 将 pane 持久意图与真实 session owner 合并，不把 session ID 写回或推断到文件快照。
fn project_pane(
    persisted: &WorkspaceSessionPane,
    runtime: Option<&super::workspace_runtime::RuntimeTerminalPane>,
    tab_id: &TabId,
) -> ContextPaneSummary {
    let phase = runtime.map(|runtime| runtime.phase);
    let target = runtime
        .and_then(|runtime| runtime.target.as_ref())
        .map(session_target_summary)
        .or_else(|| persisted.target.as_ref().map(workspace_target_summary));
    let session_id = runtime.and_then(|runtime| {
        (runtime.phase == RuntimePanePhase::Ready)
            .then(|| runtime.session_id.clone())
            .flatten()
    });
    let (cwd, shell) = runtime
        .and_then(|runtime| runtime.target.as_ref())
        .map(|target| target_live_metadata(target, persisted))
        .unwrap_or_else(|| (persisted_cwd(persisted), persisted_shell(persisted)));
    ContextPaneSummary {
        pane_id: persisted.id.clone(),
        tab_id: tab_id.clone(),
        session_id,
        title: safe_display_text(&persisted.title),
        kind: target
            .as_ref()
            .map(|target| target.kind)
            .unwrap_or_else(|| pane_mode_kind(persisted.mode)),
        host_id: target.as_ref().and_then(|target| target.host_id.clone()),
        target_ref: target.and_then(|target| target.reference),
        status: phase
            .map(pane_phase_status)
            .unwrap_or_else(|| match persisted.status {
                kerminal_application::workspace::WorkspaceMachineStatus::Online => {
                    ContextConnectionStatus::Restoring
                }
                kerminal_application::workspace::WorkspaceMachineStatus::Offline => {
                    ContextConnectionStatus::Offline
                }
                kerminal_application::workspace::WorkspaceMachineStatus::Warning => {
                    ContextConnectionStatus::Failed
                }
            }),
        cwd,
        shell,
        columns: runtime.map(|runtime| runtime.size.columns()).unwrap_or(0),
        rows: runtime.map(|runtime| runtime.size.rows()).unwrap_or(0),
    }
}

/// 用真实 transport kind 构造稳定标签；Local command path 和 Container argv 不进入显示数据。
fn session_target_summary(target: &SessionTarget) -> ContextTargetSummary {
    match target {
        SessionTarget::Local(_) => {
            target_summary(ContextConnectionKind::Local, "本地终端", None, None, None)
        }
        SessionTarget::Ssh(target) => target_summary(
            ContextConnectionKind::Ssh,
            "SSH 主机",
            Some(target.host_id.clone()),
            None,
            Some(format!("ssh:{}", target.host_id.as_str())),
        ),
        SessionTarget::Telnet(target) => target_summary(
            ContextConnectionKind::Telnet,
            "Telnet 目标",
            None,
            None,
            Some(format!(
                "{}:{}",
                safe_display_text(&target.host),
                target.port
            )),
        ),
        SessionTarget::Serial(_) => {
            target_summary(ContextConnectionKind::Serial, "串口目标", None, None, None)
        }
        SessionTarget::Container(target) => target_summary(
            ContextConnectionKind::Container,
            "容器终端",
            target.host_id.clone(),
            Some(safe_display_text(&target.container_id)),
            Some(format!("container:{}", target.container_id)),
        ),
    }
}

/// 只投影已解析的 v3 target；Unavailable 原始 JSON 不会被格式化到用户界面。
fn workspace_target_summary(target: &WorkspaceTargetRef) -> ContextTargetSummary {
    match target {
        WorkspaceTargetRef::Local { profile_id } => target_summary(
            ContextConnectionKind::Local,
            "本地终端",
            None,
            None,
            profile_id
                .as_ref()
                .map(|profile| format!("profile:{profile}")),
        ),
        WorkspaceTargetRef::Ssh { host_id } => target_summary(
            ContextConnectionKind::Ssh,
            "SSH 主机",
            Some(host_id.clone()),
            None,
            Some(format!("ssh:{}", host_id.as_str())),
        ),
        WorkspaceTargetRef::Telnet { host_id } => target_summary(
            ContextConnectionKind::Telnet,
            "Telnet 主机",
            Some(host_id.clone()),
            None,
            Some(format!("telnet:{}", host_id.as_str())),
        ),
        WorkspaceTargetRef::Serial { host_id } => target_summary(
            ContextConnectionKind::Serial,
            "串口配置",
            Some(host_id.clone()),
            None,
            Some(format!("serial:{}", host_id.as_str())),
        ),
        WorkspaceTargetRef::DockerContainer {
            host_id,
            container_id,
            container_name,
            ..
        } => target_summary(
            ContextConnectionKind::Container,
            container_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .map(safe_display_text)
                .unwrap_or_else(|| "容器".to_owned()),
            Some(host_id.clone()),
            Some(safe_display_text(container_id)),
            Some(format!("container:{}", container_id)),
        ),
        WorkspaceTargetRef::Unavailable { .. } => target_summary(
            ContextConnectionKind::Unsupported,
            "目标不可用",
            None,
            None,
            None,
        ),
    }
}

/// 文件和 SFTP tab 按其明确目标投影；未解析 target 不回退到已选主机。
fn tab_target_summary(tab: &WorkspaceSessionTab) -> Option<ContextTargetSummary> {
    match tab {
        WorkspaceSessionTab::WorkspaceFile { target, .. } => Some(workspace_target_summary(target)),
        WorkspaceSessionTab::SftpTransfer {
            left_host_id,
            locked_left_host_id,
            right_host_id,
            ..
        } => {
            let host = locked_left_host_id
                .as_ref()
                .or(left_host_id.as_ref())
                .or(right_host_id.as_ref())?;
            Some(target_summary(
                ContextConnectionKind::FileWorkspace,
                "SFTP 工作区",
                Some(host.clone()),
                None,
                Some(format!("sftp:{}", host.as_str())),
            ))
        }
        WorkspaceSessionTab::Terminal { .. } | WorkspaceSessionTab::Unsupported { .. } => None,
    }
}

/// 只暴露当前 tab 的 kind 和机器 ID；unsupported 只显示冻结的 kind 标签。
fn tab_kind(tab: &WorkspaceSessionTab) -> ContextConnectionKind {
    match tab {
        WorkspaceSessionTab::Terminal { .. } => ContextConnectionKind::Terminal,
        WorkspaceSessionTab::SftpTransfer { .. } | WorkspaceSessionTab::WorkspaceFile { .. } => {
            ContextConnectionKind::FileWorkspace
        }
        WorkspaceSessionTab::Unsupported { .. } => ContextConnectionKind::Unsupported,
    }
}

/// 对没有 runtime target 的恢复占位按冻结 pane mode 分类，不猜测可用连接。
fn pane_mode_kind(mode: WorkspacePaneMode) -> ContextConnectionKind {
    match mode {
        WorkspacePaneMode::Local => ContextConnectionKind::Local,
        WorkspacePaneMode::Ssh => ContextConnectionKind::Ssh,
        WorkspacePaneMode::Telnet => ContextConnectionKind::Telnet,
        WorkspacePaneMode::Serial => ContextConnectionKind::Serial,
        WorkspacePaneMode::Container => ContextConnectionKind::Container,
        WorkspacePaneMode::Preview => ContextConnectionKind::FileWorkspace,
    }
}

/// 从 typed tab 取显示用 machine identity，不触碰其 raw JSON 或传输状态。
fn tab_machine_id(tab: &WorkspaceSessionTab) -> Option<&str> {
    match tab {
        WorkspaceSessionTab::Terminal { machine_id, .. }
        | WorkspaceSessionTab::SftpTransfer { machine_id, .. }
        | WorkspaceSessionTab::WorkspaceFile { machine_id, .. } => Some(machine_id),
        WorkspaceSessionTab::Unsupported { .. } => None,
    }
}

/// 目标元数据优先读取本次 runtime transport；恢复快照只作为较低可信度 fallback。
fn target_live_metadata(
    target: &SessionTarget,
    persisted: &WorkspaceSessionPane,
) -> (Option<String>, Option<String>) {
    let cwd = match target {
        SessionTarget::Local(target) => target
            .working_directory
            .as_deref()
            .map(|path| safe_display_text(&path.to_string_lossy())),
        SessionTarget::Ssh(target) => target
            .working_directory
            .as_deref()
            .map(|path| safe_display_text(&path.to_string_lossy())),
        SessionTarget::Telnet(_) | SessionTarget::Serial(_) | SessionTarget::Container(_) => None,
    }
    .or_else(|| persisted_cwd(persisted));
    let shell = match target {
        SessionTarget::Local(target) => Path::new(&target.program)
            .file_name()
            .map(|name| safe_display_text(&name.to_string_lossy())),
        SessionTarget::Ssh(_) => persisted_shell(persisted),
        SessionTarget::Telnet(_) | SessionTarget::Serial(_) | SessionTarget::Container(_) => {
            persisted_shell(persisted)
        }
    };
    (cwd, shell)
}

/// 持久化当前目录与启动目录使用不同可信度，防止显示时把恢复数据称为 live cwd。
fn persisted_cwd(pane: &WorkspaceSessionPane) -> Option<String> {
    pane.current_cwd
        .as_deref()
        .or(pane.cwd.as_deref())
        .filter(|path| !path.trim().is_empty())
        .map(safe_display_text)
}

/// Shell 只显示 basename，避免把启动路径和参数带入 inspector。
fn persisted_shell(pane: &WorkspaceSessionPane) -> Option<String> {
    pane.shell
        .as_deref()
        .filter(|shell| !shell.trim().is_empty())
        .map(|shell| {
            Path::new(shell)
                .file_name()
                .map(|name| safe_display_text(&name.to_string_lossy()))
                .unwrap_or_else(|| safe_display_text(shell))
        })
}

/// 文件 tab 的路径来源明确是 workspace 文档，不把它误标成终端实时目录。
fn active_tab_location(tab: Option<&WorkspaceSessionTab>) -> ContextLocationSummary {
    if let Some(WorkspaceSessionTab::WorkspaceFile { path, .. }) = tab {
        return ContextLocationSummary {
            path: Some(safe_display_text(path)),
            source: ContextLocationSource::WorkspaceFile,
            confidence: ContextLocationConfidence::High,
        };
    }
    ContextLocationSummary {
        path: None,
        source: ContextLocationSource::Unavailable,
        confidence: ContextLocationConfidence::Unknown,
    }
}

/// 优先当前目录，再显示启动目录；路径来源会在 UI 中同步标识。
fn pane_location(
    persisted: &WorkspaceSessionPane,
    pane: &ContextPaneSummary,
) -> ContextLocationSummary {
    if let Some(path) = pane.cwd.as_ref() {
        let source = if persisted
            .current_cwd
            .as_deref()
            .is_some_and(|current| current == path)
        {
            ContextLocationSource::PersistedCurrentDirectory
        } else {
            ContextLocationSource::ConfiguredStartDirectory
        };
        let confidence = match source {
            ContextLocationSource::PersistedCurrentDirectory => ContextLocationConfidence::Medium,
            ContextLocationSource::ConfiguredStartDirectory => ContextLocationConfidence::Low,
            _ => ContextLocationConfidence::Unknown,
        };
        return ContextLocationSummary {
            path: Some(path.clone()),
            source,
            confidence,
        };
    }
    ContextLocationSummary {
        path: None,
        source: ContextLocationSource::Unavailable,
        confidence: ContextLocationConfidence::Unknown,
    }
}

/// 将运行态 pane phase 映射成安全显示状态，不显示 transport 异常文本。
fn pane_phase_status(phase: RuntimePanePhase) -> ContextConnectionStatus {
    match phase {
        RuntimePanePhase::RestorePending => ContextConnectionStatus::Restoring,
        RuntimePanePhase::Ready => ContextConnectionStatus::Ready,
        RuntimePanePhase::RestoreFailed | RuntimePanePhase::CloseFailed => {
            ContextConnectionStatus::Failed
        }
        RuntimePanePhase::Closing => ContextConnectionStatus::Closing,
    }
}

/// 资源计数只按 Root 当前 runtime map 统计，恢复失败与 pending 分开呈现。
fn runtime_summary(
    pane_count: usize,
    tab_count: usize,
    runtime: &super::workspace_runtime::WorkspaceRuntime,
) -> ContextRuntimeSummary {
    let mut summary = ContextRuntimeSummary {
        tab_count,
        pane_count,
        ..ContextRuntimeSummary::default()
    };
    for (_, pane) in runtime.panes() {
        match pane.phase {
            RuntimePanePhase::Ready => summary.ready_pane_count += 1,
            RuntimePanePhase::RestorePending | RuntimePanePhase::Closing => {
                summary.pending_pane_count += 1
            }
            RuntimePanePhase::RestoreFailed | RuntimePanePhase::CloseFailed => {
                summary.failed_pane_count += 1
            }
        }
    }
    summary
}

/// 通过结构化字段生成 target 标签，所有身份均来自经过解析的领域类型。
fn target_summary(
    kind: ContextConnectionKind,
    label: impl Into<String>,
    host_id: Option<HostId>,
    container_id: Option<String>,
    reference: Option<String>,
) -> ContextTargetSummary {
    ContextTargetSummary {
        kind,
        label: safe_display_text(&label.into()),
        host_id,
        container_id,
        reference: reference.map(|value| safe_display_text(&value)),
    }
}

/// 移除控制字符并限制自由文本长度，避免标题、路径和设备名破坏 inspector 布局。
fn safe_display_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(512)
        .collect()
}
