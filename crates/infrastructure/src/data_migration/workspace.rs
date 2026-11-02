//! 仅在旧格式迁移入口归一化已知 v3 工作区缺省字段。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, path::PathBuf};

use kerminal_application::{AppError, AppResult, workspace::WorkspaceSessionDocument};
use serde_json::{Value, json};

use super::FileEdit;

const SESSION_RELATIVE_PATH: &str = "workspace/session.json";

/// 只补已知旧 v3 缺失/null 字段，再由当前 codec 完整校验；未知值和扩展字段原样保留。
///
/// @author kongweiguang
pub(super) fn normalize_workspace_bytes(source: &[u8]) -> AppResult<Option<Vec<u8>>> {
    let mut value: Value = serde_json::from_slice(source)
        .map_err(|_| AppError::configuration_corrupt("工作区会话"))?;
    if value.get("version").and_then(Value::as_u64) != Some(3) {
        return Err(AppError::configuration_corrupt("工作区会话"));
    }
    let panes = value
        .as_object_mut()
        .and_then(|root| root.get_mut("terminalPanes"))
        .and_then(Value::as_array_mut)
        .ok_or_else(|| AppError::configuration_corrupt("工作区会话"))?;

    let mut changed = false;
    for pane in panes {
        let fields = pane
            .as_object_mut()
            .ok_or_else(|| AppError::configuration_corrupt("工作区会话"))?;
        if matches!(fields.get("prompt"), None | Some(Value::Null)) {
            fields.insert("prompt".to_owned(), json!("PS>"));
            changed = true;
        }
        if matches!(fields.get("status"), None | Some(Value::Null)) {
            fields.insert("status".to_owned(), json!("offline"));
            changed = true;
        }
    }

    WorkspaceSessionDocument::try_from_value(value.clone())
        .map_err(|_| AppError::configuration_corrupt("工作区会话"))?;
    if !changed {
        return Ok(None);
    }
    let after = serde_json::to_vec(&value)
        .map_err(|_| AppError::infrastructure("无法编码迁移后的工作区会话"))?;
    Ok(Some(after))
}

/// 将一次性归一化结果封装为固定路径 CAS 编辑，规划阶段不访问磁盘或创建目录。
///
/// @author kongweiguang
pub(super) fn plan_workspace_migration(
    before: Option<Vec<u8>>,
    profile_mapping: &BTreeMap<String, String>,
) -> AppResult<Option<FileEdit>> {
    let Some(before) = before else {
        return Ok(None);
    };
    let normalized = normalize_workspace_bytes(&before)?;
    let mut value: Value = serde_json::from_slice(normalized.as_deref().unwrap_or(&before))
        .map_err(|_| AppError::configuration_corrupt("工作区会话"))?;
    let changed = remap_known_profile_references(&mut value, profile_mapping)?;
    if normalized.is_none() && !changed {
        return Ok(None);
    }
    WorkspaceSessionDocument::try_from_value(value.clone())
        .map_err(|_| AppError::configuration_corrupt("工作区会话"))?;
    let after = serde_json::to_vec(&value)
        .map_err(|_| AppError::infrastructure("无法编码迁移后的工作区会话"))?;
    Ok(Some(FileEdit {
        relative_path: PathBuf::from(SESSION_RELATIVE_PATH),
        before: Some(before),
        after,
    }))
}

/// 只改已知本地主机身份；未知 ID、远程目标与共享 machine-local 原样保留。
fn remap_known_profile_references(
    value: &mut Value,
    mapping: &BTreeMap<String, String>,
) -> AppResult<bool> {
    let root = value
        .as_object_mut()
        .ok_or_else(|| AppError::configuration_corrupt("工作区会话"))?;
    let mut changed = remap_prefixed_field(root, "selectedMachineId", "profile:", mapping, false);
    if let Some(machines) = root
        .get_mut("sidebarMachines")
        .and_then(Value::as_array_mut)
    {
        for machine in machines {
            if let Some(fields) = machine.as_object_mut() {
                changed |= remap_prefixed_field(fields, "id", "profile:", mapping, false);
            }
        }
    }
    if let Some(panes) = root.get_mut("terminalPanes").and_then(Value::as_array_mut) {
        for pane in panes {
            let fields = pane
                .as_object_mut()
                .ok_or_else(|| AppError::configuration_corrupt("工作区会话"))?;
            changed |= remap_prefixed_field(fields, "machineId", "profile:", mapping, false);
            if fields.get("mode").and_then(Value::as_str) == Some("local") {
                changed |= remap_id_field(fields, "profileId", mapping);
            }
            if let Some(target) = fields.get_mut("target").and_then(Value::as_object_mut)
                && target.get("kind").and_then(Value::as_str) == Some("local")
            {
                changed |= remap_id_field(target, "profileId", mapping);
            }
            if let Some(tmux) = fields.get_mut("tmuxBinding").and_then(Value::as_object_mut) {
                changed |= remap_prefixed_field(tmux, "targetRef", "local:", mapping, true);
            }
        }
    }
    if let Some(tabs) = root.get_mut("terminalTabs").and_then(Value::as_array_mut) {
        for tab in tabs {
            if let Some(fields) = tab.as_object_mut() {
                changed |= remap_prefixed_field(fields, "machineId", "profile:", mapping, false);
                if let Some(target) = fields.get_mut("target").and_then(Value::as_object_mut)
                    && target.get("kind").and_then(Value::as_str) == Some("local")
                {
                    changed |= remap_id_field(target, "profileId", mapping);
                }
            }
        }
    }
    Ok(changed)
}

/// 显式本地目标缺失或未知时不作替代，避免转向其他模板或主机。
fn remap_id_field(
    fields: &mut serde_json::Map<String, Value>,
    key: &str,
    mapping: &BTreeMap<String, String>,
) -> bool {
    let Some(replacement) = fields
        .get(key)
        .and_then(Value::as_str)
        .and_then(|id| mapping.get(id))
    else {
        return false;
    };
    fields.insert(key.to_owned(), Value::String(replacement.clone()));
    true
}

/// 仅替换完整已知前缀的 Profile ID，tmux 的 socket 后缀保持原样。
pub(super) fn remap_reference(
    value: &str,
    prefix: &str,
    mapping: &BTreeMap<String, String>,
    socket_suffix: bool,
) -> Option<String> {
    let remaining = value.strip_prefix(prefix)?;
    let (id, suffix) = if socket_suffix {
        remaining
            .split_once('|')
            .map_or((remaining, ""), |(id, _)| (id, &remaining[id.len()..]))
    } else {
        (remaining, "")
    };
    let next = mapping.get(id)?;
    Some(format!("{prefix}{next}{suffix}"))
}

/// 修改 JSON 对象的单个已知引用，不遍历未知扩展字段。
fn remap_prefixed_field(
    fields: &mut serde_json::Map<String, Value>,
    key: &str,
    prefix: &str,
    mapping: &BTreeMap<String, String>,
    socket_suffix: bool,
) -> bool {
    let replacement = fields
        .get(key)
        .and_then(Value::as_str)
        .and_then(|value| remap_reference(value, prefix, mapping, socket_suffix));
    if let Some(replacement) = replacement {
        fields.insert(key.to_owned(), Value::String(replacement));
        true
    } else {
        false
    }
}

#[cfg(test)]
#[path = "../../tests/support/data_migration_workspace_unit.rs"]
mod tests;
