//! 旧 Tauri 窗口状态的一次迁移规划；普通窗口仓储不再保留旧解码路径。
//!
//! @author kongweiguang

use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use kerminal_application::{
    AppError, AppResult,
    platform::window_state::{WindowRect, WindowStateSnapshot},
};
use serde::Deserialize;

use super::FileEdit;
use crate::platform::window_state::validate_current_window_state;

const CURRENT_FILE_NAME: &str = "window-state.json";
const MAX_STATE_BYTES: u64 = 16 * 1024;

/// 当前文件优先且必须有效；旧源只生成 before=None 的新文件计划，真正备份、CAS 和提交统一交 core。
///
/// @author kongweiguang
pub(super) fn plan_window_state_migration(
    root: &Path,
    legacy_path: Option<&Path>,
) -> AppResult<Vec<FileEdit>> {
    if validate_current_window_state(&root.join(CURRENT_FILE_NAME))? {
        return Ok(Vec::new());
    }
    let Some(legacy_path) = legacy_path else {
        return Ok(Vec::new());
    };
    let Some(snapshot) = read_legacy_file(legacy_path)? else {
        return Ok(Vec::new());
    };
    let mut after = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "revision": 1,
        "state": {
            "normal_bounds": snapshot.normal_bounds,
            "maximized": snapshot.maximized,
            "fullscreen": snapshot.fullscreen,
        },
    }))
    .map_err(|_| AppError::infrastructure("无法编码迁移后的窗口状态"))?;
    after.push(b'\n');
    Ok(vec![FileEdit {
        relative_path: PathBuf::from(CURRENT_FILE_NAME),
        before: None,
        after,
    }])
}

#[derive(Deserialize)]
struct LegacyWindowStateFile {
    main: LegacyWindowState,
}

#[derive(Deserialize)]
struct LegacyWindowState {
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    #[serde(default)]
    prev_x: Option<i32>,
    #[serde(default)]
    prev_y: Option<i32>,
    #[serde(default)]
    maximized: bool,
    #[serde(default)]
    fullscreen: bool,
}

/// 保留原 Tauri main 解码与最大化前坐标规则；只读原文件，损坏或非法几何均阻断迁移。
///
/// @author kongweiguang
fn read_legacy_file(path: &Path) -> AppResult<Option<WindowStateSnapshot>> {
    let Some(bytes) = read_bounded_file(path)? else {
        return Ok(None);
    };
    let file = serde_json::from_slice::<LegacyWindowStateFile>(&bytes)
        .map_err(|_| AppError::configuration_corrupt("旧窗口状态"))?;
    let x = if file.main.maximized {
        file.main.prev_x.unwrap_or(file.main.x)
    } else {
        file.main.x
    };
    let y = if file.main.maximized {
        file.main.prev_y.unwrap_or(file.main.y)
    } else {
        file.main.y
    };
    let snapshot = WindowStateSnapshot {
        normal_bounds: WindowRect {
            x,
            y,
            width: file.main.width,
            height: file.main.height,
        },
        maximized: file.main.maximized,
        fullscreen: file.main.fullscreen,
    };
    if !snapshot.normal_bounds.is_valid() {
        return Err(AppError::configuration_corrupt("旧窗口状态"));
    }
    Ok(Some(snapshot))
}

/// 旧窗口输入沿用原尺寸和非链接约束；读取失败保留原文件，不能按缺失状态继续初始化。
///
/// @author kongweiguang
fn read_bounded_file(path: &Path) -> AppResult<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(AppError::infrastructure("无法读取旧窗口状态")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_STATE_BYTES
    {
        return Err(AppError::configuration_corrupt("旧窗口状态"));
    }
    let file = fs::File::open(path).map_err(|_| AppError::infrastructure("无法读取旧窗口状态"))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| AppError::infrastructure("无法读取旧窗口状态"))?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(AppError::configuration_corrupt("旧窗口状态"));
    }
    Ok(Some(bytes))
}

#[cfg(test)]
#[path = "../../tests/support/data_migration_window_state_unit.rs"]
mod tests;
