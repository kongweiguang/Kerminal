//! 文件工作区仅在显式迁移事务中聚合，普通 reader 不执行旧标签归拢。
//!
//! @author kongweiguang

use std::collections::{BTreeMap, BTreeSet};

use kerminal_application::{
    AppError, AppResult,
    workspace::{WorkspaceSessionDocument, WorkspaceSessionTab, WorkspaceTargetRef},
};
use kerminal_domain::files::RemotePath;
use serde_json::{Map, Value, json};

/// 聚合必须以可确认的文件系统身份为界；不使用 machine 标题或临时会话推断目标。
/// @author kongweiguang
pub(super) fn aggregate(value: &mut Value) -> AppResult<bool> {
    let document =
        WorkspaceSessionDocument::try_from_value(value.clone()).map_err(|_| invalid())?;
    let snapshot = document.workspace_snapshot().map_err(|_| invalid())?;
    let mut groups = BTreeMap::<String, Vec<usize>>::new();
    let mut current = BTreeSet::new();
    for (index, tab) in snapshot.terminal_tabs.iter().enumerate() {
        match tab {
            WorkspaceSessionTab::WorkspaceFile { target, .. } => {
                groups
                    .entry(filesystem_key(target)?)
                    .or_default()
                    .push(index);
            }
            WorkspaceSessionTab::FileWorkspace { target, .. } => {
                // 守卫不能使用 `?`；先解析身份再插入，重复身份仍按损坏阻断。
                let filesystem = filesystem_key(target)?;
                if !current.insert(filesystem) {
                    return Err(invalid());
                }
            }
            _ => {}
        }
    }
    if groups.is_empty() {
        return Ok(false);
    }
    if groups.keys().any(|key| current.contains(key)) {
        return Err(invalid());
    }
    let rows = value
        .get("terminalTabs")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    let mut replacements = BTreeMap::new();
    let mut removed = BTreeSet::new();
    let mut used_ids = snapshot
        .terminal_tabs
        .iter()
        .map(|tab| tab.id().as_str().to_owned())
        .collect::<BTreeSet<_>>();
    for tab in &snapshot.terminal_tabs {
        if let WorkspaceSessionTab::FileWorkspace { state, .. } = tab {
            used_ids.extend(state.items.iter().map(|item| item.id().as_str().to_owned()));
            used_ids.extend(state.source_tab_ids.iter().map(|id| id.as_str().to_owned()));
        }
    }
    for indices in groups.values() {
        let first = indices[0];
        let active = indices.iter().copied().find(|index| {
            snapshot.active_tab_id.as_ref() == Some(snapshot.terminal_tabs[*index].id())
        });
        let owner_index = active.unwrap_or(first);
        let owner = &snapshot.terminal_tabs[owner_index];
        let WorkspaceSessionTab::WorkspaceFile { target, path, .. } = owner else {
            return Err(invalid());
        };
        let mut state_items = Vec::<Value>::new();
        let directory_id = available_directory_id(owner.id().as_str(), &mut used_ids)?;
        state_items.push(
            json!({"id":directory_id,"kind":"directory","path":parent_directory(target, path)?}),
        );
        let mut active_item = None;
        let mut sources = Vec::new();
        let mut document_keys = Vec::<String>::new();
        for index in indices {
            let row = rows[*index].as_object().ok_or_else(invalid)?;
            let id = snapshot.terminal_tabs[*index].id().as_str();
            sources.push(json!(id));
            let mut item = row.clone();
            // 原顶层字段也留在内部项中作为恢复证据，不能只保留最后一项的未知扩展。
            item.insert("kind".into(), json!("document"));
            let key = endpoint_path_key(
                target,
                item.get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid)?,
            )?;
            let compatible = state_items
                .iter()
                .enumerate()
                .skip(1)
                .find(|(offset, _)| document_keys[*offset - 1] == key)
                .map(|(offset, _)| offset);
            let actual_id = if let Some(offset) = compatible {
                // 不相容元数据保留在原文件的可见恢复流程，不能制造同端点的两个内部文档。
                if !compatible_document(&state_items[offset], &item) {
                    return Err(invalid());
                }
                if item.get("access").and_then(Value::as_str) == Some("readonly") {
                    state_items[offset]["access"] = json!("readonly");
                }
                state_items[offset].get("id").cloned().ok_or_else(invalid)?
            } else {
                state_items.push(Value::Object(item));
                document_keys.push(key);
                json!(id)
            };
            if *index == owner_index {
                active_item = Some(actual_id);
            }
            if *index != first {
                removed.insert(*index);
            }
        }
        // 同端点有任一只读来源就收紧所有兼容或保守保留的副本，不因顺序不同扩大权限。
        let readonly_paths = state_items
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, item)| item.get("access").and_then(Value::as_str) == Some("readonly"))
            .map(|(offset, _)| document_keys[offset - 1].clone())
            .collect::<BTreeSet<_>>();
        for (offset, item) in state_items.iter_mut().enumerate().skip(1) {
            if readonly_paths.contains(&document_keys[offset - 1]) {
                item["access"] = json!("readonly");
            }
        }
        let mut row = rows[owner_index].as_object().cloned().ok_or_else(invalid)?;
        for key in ["path", "rootPath", "access", "source"] {
            row.remove(key);
        }
        // 原自定义文件标题已保存在内部文档；宿主标题必须表达主机，不能继续显示单个文件名。
        let label = match target {
            WorkspaceTargetRef::Local { .. } => "本机".to_owned(),
            WorkspaceTargetRef::Ssh { host_id } => host_id.as_str().to_owned(),
            WorkspaceTargetRef::DockerContainer { container_id, .. } => container_id.clone(),
            _ => return Err(invalid()),
        };
        row.insert("title".into(), json!(format!("{label} · 文件")));
        row.insert("kind".into(), json!("fileWorkspace"));
        row.insert("stateVersion".into(), json!(1));
        row.insert(
            "state".into(),
            json!({"items":state_items,"activeItemId":active_item,"sourceTabIds":sources}),
        );
        replacements.insert(first, Value::Object(row));
    }
    let next = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            if let Some(replacement) = replacements.remove(&index) {
                Some(replacement)
            } else if removed.contains(&index) {
                None
            } else {
                Some(row.clone())
            }
        })
        .collect::<Vec<_>>();
    value["terminalTabs"] = Value::Array(next);
    WorkspaceSessionDocument::try_from_value(value.clone()).map_err(|_| invalid())?;
    Ok(true)
}

/// 同一 Local 文件系统忽略 profile，远端和容器保持领域端点的身份隔离。
/// @author kongweiguang
fn filesystem_key(target: &WorkspaceTargetRef) -> AppResult<String> {
    match target {
        WorkspaceTargetRef::Local { .. } => Ok("local".into()),
        WorkspaceTargetRef::Ssh { host_id } => Ok(format!("ssh:{}", host_id.as_str())),
        WorkspaceTargetRef::DockerContainer {
            host_id,
            container_id,
            runtime,
            ..
        } => Ok(json!([
            "container",
            host_id.as_str(),
            format!("{runtime:?}"),
            container_id
        ])
        .to_string()),
        _ => Err(invalid()),
    }
}

/// 去重沿用端点路径的词法语义，不探测真实目录、不消除远端父段或折叠大小写。
/// @author kongweiguang
fn endpoint_path_key(target: &WorkspaceTargetRef, path: &str) -> AppResult<String> {
    match target {
        WorkspaceTargetRef::Local { .. } => {
            if path.is_empty() || path.chars().any(char::is_control) {
                return Err(invalid());
            }
            Ok(path.to_owned())
        }
        WorkspaceTargetRef::Ssh { .. } => RemotePath::parse(path)
            .map(|path| path.as_str().to_owned())
            .map_err(|_| invalid()),
        WorkspaceTargetRef::DockerContainer { .. } => RemotePath::parse_container(path)
            .map(|path| path.as_str().to_owned())
            .map_err(|_| invalid()),
        _ => Err(invalid()),
    }
}

/// 默认目录只由活动文件或首项路径确定，不借用进程当前目录或访问用户文件。
/// @author kongweiguang
fn parent_directory(target: &WorkspaceTargetRef, path: &str) -> AppResult<String> {
    let key = endpoint_path_key(target, path)?;
    if matches!(target, WorkspaceTargetRef::Local { .. }) {
        return Ok(std::path::Path::new(&key)
            .parent()
            .and_then(|parent| parent.to_str())
            .filter(|parent| !parent.is_empty())
            .unwrap_or(".")
            .to_owned());
    }
    let path = RemotePath::parse(&key).map_err(|_| invalid())?;
    Ok(path.parent().unwrap_or(path).as_str().to_owned())
}

/// 只有所有文档语义及扩展相容才折叠重复项；不相容时保留原文件并进入恢复流程。
/// @author kongweiguang
fn compatible_document(existing: &Value, item: &Map<String, Value>) -> bool {
    let Some(existing) = existing.as_object() else {
        return false;
    };
    let mut left = existing.clone();
    let mut right = item.clone();
    for key in ["id", "access", "tabGroupId"] {
        left.remove(key);
        right.remove(key);
    }
    left == right
}

/// 使用确定性且无碰撞的内部 ID，预检重复执行不会产生不同的迁移计划。
/// @author kongweiguang
fn available_directory_id(owner: &str, used: &mut BTreeSet<String>) -> AppResult<String> {
    for suffix in 0u64.. {
        let id = format!("{owner}:directory:{suffix}");
        if used.insert(id.clone()) {
            return Ok(id);
        }
    }
    Err(invalid())
}

/// 错误不含用户路径或原文，交由现有迁移 inspection 保留文件并给出可见恢复状态。
/// @author kongweiguang
fn invalid() -> AppError {
    AppError::configuration_corrupt("工作区文件聚合")
}
