//! 将运行态临时工作区资源排除在 v3 持久化投影之外。
//!
//! @author kongweiguang

use std::collections::BTreeSet;

use kerminal_application::{
    AppError, AppResult,
    workspace::{
        WorkspaceSessionPane, WorkspaceSessionSnapshot, WorkspaceSessionTab, WorkspaceTargetRef,
    },
};
use kerminal_domain::{
    identity::{PaneId, TabId},
    workspace::TerminalLayoutNode,
};

/// 移除运行态临时 Tab/Pane 及所有 external HostId，得到可安全写回的快照副本。
///
/// 显式 transient ID 是 Root 运行态 owner 的权威标记；额外检查 typed HostId 是 fail-closed 防线，
/// 避免新路径漏标后把进程退出即失效的 external profile ID 写进 `~/.kerminal`。
///
/// @author kongweiguang
pub(super) fn without_transient_resources(
    source: &WorkspaceSessionSnapshot,
    transient_tabs: &BTreeSet<TabId>,
    transient_panes: &BTreeSet<PaneId>,
) -> AppResult<WorkspaceSessionSnapshot> {
    source
        .validate()
        .map_err(|_| AppError::invalid("工作区快照无效，拒绝建立持久化投影"))?;

    let mut removed_tabs = transient_tabs.clone();
    let mut removed_panes = transient_panes.clone();
    for tab in &source.terminal_tabs {
        if tab_has_external_target(tab) {
            removed_tabs.insert(tab.id().clone());
        }
        if removed_tabs.contains(tab.id())
            && let Some(layout) = tab.terminal_layout()
        {
            removed_panes.extend(layout.pane_ids());
        }
    }
    removed_panes.extend(
        source
            .terminal_panes
            .iter()
            .filter(|pane| pane_has_external_target(pane))
            .map(|pane| pane.id.clone()),
    );

    let mut projected = source.clone();
    let mut surviving_tabs = Vec::with_capacity(projected.terminal_tabs.len());
    for mut tab in projected.terminal_tabs {
        if removed_tabs.contains(tab.id()) || tab_has_external_target(&tab) {
            continue;
        }
        if let WorkspaceSessionTab::Terminal { layout, .. } = &mut tab {
            let Some(next_layout) = layout_without_panes(layout, &removed_panes)? else {
                continue;
            };
            *layout = next_layout;
        }
        surviving_tabs.push(tab);
    }
    projected.terminal_tabs = surviving_tabs;

    let referenced_panes = projected
        .terminal_tabs
        .iter()
        .filter_map(WorkspaceSessionTab::terminal_layout)
        .flat_map(TerminalLayoutNode::pane_ids)
        .collect::<BTreeSet<_>>();
    projected.terminal_panes.retain(|pane| {
        referenced_panes.contains(&pane.id)
            && !removed_panes.contains(&pane.id)
            && !pane_has_external_target(pane)
    });
    normalize_selection(&mut projected);
    projected
        .validate()
        .map_err(|_| AppError::invalid("临时资源清理后工作区快照仍无效，拒绝写入"))?;
    Ok(projected)
}

/// 用领域布局删除规则逐个剥离 transient pane，借此折叠单子节点 split 并重映射比例键。
fn layout_without_panes(
    layout: &TerminalLayoutNode,
    removed_panes: &BTreeSet<PaneId>,
) -> AppResult<Option<TerminalLayoutNode>> {
    let mut projected = Some(layout.clone());
    for pane_id in layout.pane_ids() {
        if !removed_panes.contains(&pane_id) {
            continue;
        }
        let Some(current) = projected.take() else {
            break;
        };
        projected = current
            .remove_pane(&pane_id)
            .map_err(|_| AppError::invalid("工作区 split 无法安全移除临时 pane"))?;
    }
    Ok(projected)
}

/// 重新选择合法活动对象；焦点 pane 必须属于活动终端标签，机器 ID 随活动对象更新。
fn normalize_selection(snapshot: &mut WorkspaceSessionSnapshot) {
    let active_tab_id = snapshot
        .active_tab_id
        .as_ref()
        .filter(|active| snapshot.terminal_tabs.iter().any(|tab| tab.id() == *active))
        .cloned()
        .or_else(|| snapshot.terminal_tabs.first().map(|tab| tab.id().clone()));
    snapshot.active_tab_id = active_tab_id.clone();

    let Some(active_tab) = active_tab_id
        .as_ref()
        .and_then(|active| snapshot.terminal_tabs.iter().find(|tab| tab.id() == active))
    else {
        snapshot.focused_pane_id = None;
        snapshot.selected_machine_id = None;
        return;
    };

    if let Some(layout) = active_tab.terminal_layout() {
        let pane_ids = layout.pane_ids();
        let focused = snapshot
            .focused_pane_id
            .as_ref()
            .filter(|focused| pane_ids.contains(focused))
            .cloned()
            .or_else(|| pane_ids.first().cloned());
        snapshot.selected_machine_id = focused.as_ref().and_then(|focused| {
            snapshot
                .terminal_panes
                .iter()
                .find(|pane| &pane.id == focused)
                .map(|pane| pane.machine_id.clone())
        });
        snapshot.focused_pane_id = focused;
    } else {
        snapshot.focused_pane_id = None;
        snapshot.selected_machine_id = tab_machine_id(active_tab).map(str::to_owned);
    }
}

/// 读取文件类标签的 machine ID；未知 tab 没有该字段时保持无选择。
fn tab_machine_id(tab: &WorkspaceSessionTab) -> Option<&str> {
    match tab {
        WorkspaceSessionTab::SftpTransfer { machine_id, .. }
        | WorkspaceSessionTab::WorkspaceFile { machine_id, .. } => Some(machine_id),
        WorkspaceSessionTab::Terminal { .. } | WorkspaceSessionTab::Unsupported { .. } => None,
    }
}

/// 识别文件标签中的临时外部启动身份；该 opaque ID 不保证采用 external HostId 前缀。
fn tab_has_external_target(tab: &WorkspaceSessionTab) -> bool {
    match tab {
        WorkspaceSessionTab::Terminal { machine_id, .. } => machine_id.starts_with("external:"),
        WorkspaceSessionTab::Unsupported { raw, .. } => raw_contains_external_host_reference(raw),
        WorkspaceSessionTab::SftpTransfer {
            left_host_id,
            locked_left_host_id,
            right_host_id,
            external_launch_id,
            ..
        } => {
            [left_host_id, locked_left_host_id, right_host_id]
                .into_iter()
                .flatten()
                .any(host_id_is_external)
                || external_launch_id.is_some()
        }
        WorkspaceSessionTab::WorkspaceFile { target, .. } => target_is_external(target),
    }
}

/// 未来版 tab 保留原始 JSON 时，只检查明确表示主机身份的字段，避免猜测或改写未知业务数据。
fn raw_contains_external_host_reference(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(fields) => fields.iter().any(|(name, value)| {
            let is_host_reference = matches!(
                name.as_str(),
                "hostId"
                    | "machineId"
                    | "remoteHostId"
                    | "leftHostId"
                    | "lockedLeftHostId"
                    | "rightHostId"
                    | "externalLaunchId"
            );
            (is_host_reference
                && value
                    .as_str()
                    .is_some_and(|identity| identity.starts_with("external:")))
                || raw_contains_external_host_reference(value)
        }),
        serde_json::Value::Array(values) => values.iter().any(raw_contains_external_host_reference),
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => false,
    }
}

/// 检查 pane 的所有可持久化连接引用，external registry 的主机凭据不得跨进程落盘。
fn pane_has_external_target(pane: &WorkspaceSessionPane) -> bool {
    pane.remote_host_id
        .as_ref()
        .is_some_and(host_id_is_external)
        || pane.target.as_ref().is_some_and(target_is_external)
}

/// 限定安全过滤到不可持久化的 external ID 前缀，不改变正常保存主机引用。
fn host_id_is_external(host_id: &kerminal_domain::identity::HostId) -> bool {
    host_id.as_str().starts_with("external:")
}

/// 统一判断所有 typed 远端目标中的 host identity，包括可能持有远端宿主的容器文件。
fn target_is_external(target: &WorkspaceTargetRef) -> bool {
    match target {
        WorkspaceTargetRef::Ssh { host_id }
        | WorkspaceTargetRef::Telnet { host_id }
        | WorkspaceTargetRef::Serial { host_id }
        | WorkspaceTargetRef::DockerContainer { host_id, .. } => host_id_is_external(host_id),
        WorkspaceTargetRef::Local { .. } | WorkspaceTargetRef::Unavailable { .. } => false,
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_workspace_transient.rs"]
mod tests;
