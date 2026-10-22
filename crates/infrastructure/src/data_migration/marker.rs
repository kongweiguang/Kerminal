//! `.kerminal-format.toml` 持久化数据代标记的严格读写规则。
//!
//! @author kongweiguang

use std::{fs, io, path::Path};

use kerminal_application::{AppError, AppResult, data_migration::DataMigrationBlocker};
use serde::{Deserialize, Serialize};

pub(super) const MARKER_FILE_NAME: &str = ".kerminal-format.toml";
pub(super) const CURRENT_DATA_FORMAT: u32 = 4;
pub(super) const CURRENT_MIGRATION_CONTRACT: u32 = 2;
pub(super) const PREVIOUS_MIGRATION_CONTRACT: u32 = 1;
const MAX_MARKER_BYTES: u64 = 4 * 1024;

/// 只为已知的同代上一契约开放显式升级，其他旧代和未来标记仍不能被猜测解释。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum MarkerRead {
    Missing,
    Current,
    PreviousSupported,
    Blocked(DataMigrationBlocker),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FormatMarker {
    data_format: u32,
    migration_contract: u32,
}

/// 只从普通小文件读取 marker，避免跟随链接、读取无界内容或接受未来字段后误写。
pub(super) fn read_marker(root: &Path) -> AppResult<MarkerRead> {
    let path = root.join(MARKER_FILE_NAME);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(MarkerRead::Missing),
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "读取用户数据格式标记失败",
                source,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_MARKER_BYTES
    {
        return Ok(MarkerRead::Blocked(DataMigrationBlocker::MarkerCorrupt));
    }
    let bytes = fs::read(path).map_err(|source| {
        AppError::infrastructure_with_source("读取用户数据格式标记失败", source)
    })?;
    classify_marker_bytes(&bytes)
}

/// 只接受完整的两个字段；精确的 (4, 1) 只进入迁移，不成为业务 reader 的兼容分支。
pub(super) fn classify_marker_bytes(bytes: &[u8]) -> AppResult<MarkerRead> {
    let marker = match toml::from_str::<FormatMarker>(std::str::from_utf8(bytes).unwrap_or("")) {
        Ok(marker) => marker,
        Err(_) => return Ok(MarkerRead::Blocked(DataMigrationBlocker::MarkerCorrupt)),
    };
    if marker.data_format > CURRENT_DATA_FORMAT {
        return Ok(MarkerRead::Blocked(
            DataMigrationBlocker::FutureDataFormat {
                data_format: marker.data_format,
            },
        ));
    }
    if marker.migration_contract > CURRENT_MIGRATION_CONTRACT {
        return Ok(MarkerRead::Blocked(
            DataMigrationBlocker::FutureMigrationContract {
                migration_contract: marker.migration_contract,
            },
        ));
    }
    if marker.data_format != CURRENT_DATA_FORMAT {
        return Ok(MarkerRead::Blocked(
            DataMigrationBlocker::UnsupportedDataFormat {
                data_format: marker.data_format,
            },
        ));
    }
    if marker.migration_contract == PREVIOUS_MIGRATION_CONTRACT {
        return Ok(MarkerRead::PreviousSupported);
    }
    if marker.migration_contract != CURRENT_MIGRATION_CONTRACT {
        return Ok(MarkerRead::Blocked(
            DataMigrationBlocker::UnsupportedMigrationContract {
                migration_contract: marker.migration_contract,
            },
        ));
    }
    Ok(MarkerRead::Current)
}

/// 生成稳定的小型 marker 正文；此内容由 core 最后作为事务提交项写入。
pub(super) fn current_marker_bytes() -> Vec<u8> {
    format!(
        "data_format = {CURRENT_DATA_FORMAT}\nmigration_contract = {CURRENT_MIGRATION_CONTRACT}\n"
    )
    .into_bytes()
}

/// 固定旧 journal 已发布的 marker 摘要，升级时绝不根据新契约重算旧事务的 after。
pub(super) fn previous_marker_bytes() -> Vec<u8> {
    format!(
        "data_format = {CURRENT_DATA_FORMAT}\nmigration_contract = {PREVIOUS_MIGRATION_CONTRACT}\n"
    )
    .into_bytes()
}

#[cfg(test)]
#[path = "../../tests/support/data_migration_marker_unit.rs"]
mod tests;
