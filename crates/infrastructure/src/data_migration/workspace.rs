//! 仅在旧格式迁移入口归一化已知 v3 工作区缺省字段。
//!
//! @author kongweiguang

use std::path::PathBuf;

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
pub(super) fn plan_workspace_migration(before: Option<Vec<u8>>) -> AppResult<Option<FileEdit>> {
    let Some(before) = before else {
        return Ok(None);
    };
    let Some(after) = normalize_workspace_bytes(&before)? else {
        return Ok(None);
    };
    Ok(Some(FileEdit {
        relative_path: PathBuf::from(SESSION_RELATIVE_PATH),
        before: Some(before),
        after,
    }))
}

#[cfg(test)]
#[path = "../../tests/support/data_migration_workspace_unit.rs"]
mod tests;
