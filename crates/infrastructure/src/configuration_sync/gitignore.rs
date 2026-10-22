//! 以原子追加方式维护同步所需的本地状态忽略规则。
//!
//! @author kongweiguang

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use kerminal_application::{AppError, AppResult};
use uuid::Uuid;

use crate::storage::{AtomicCommitError, CommitMode, commit_staged_file};

use super::protected::REQUIRED_IGNORE_RULES;

/// 仅补充缺少的托管忽略行，保留用户规则并使用共享原子提交原语。
///
/// 提交前关闭写入句柄，使 Windows 原子替换不依赖打开句柄的共享删除策略。
///
/// @author kongweiguang
pub(super) fn ensure_ignore_rules(root: &Path) -> AppResult<bool> {
    let destination = root.join(".gitignore");
    let metadata = match fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(AppError::conflict(".gitignore 不是可安全更新的普通文件"));
        }
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "读取 Git 忽略规则失败",
                source,
            ));
        }
    };
    let current = match fs::read(&destination) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound && metadata.is_none() => Vec::new(),
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "读取 Git 忽略规则失败",
                source,
            ));
        }
    };
    let existing_lines: Vec<&[u8]> = current
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line).trim_ascii())
        .collect();
    let missing: Vec<&str> = REQUIRED_IGNORE_RULES
        .iter()
        .copied()
        .filter(|rule| !existing_lines.contains(&rule.as_bytes()))
        .collect();
    if missing.is_empty() {
        return Ok(false);
    }

    let original = current.clone();
    let mut updated = current;
    if !updated.is_empty() && !updated.ends_with(b"\n") {
        updated.push(b'\n');
    }
    for rule in missing {
        updated.extend_from_slice(rule.as_bytes());
        updated.push(b'\n');
    }

    let staged = root.join(format!(".gitignore.kerminal-{}.tmp", Uuid::new_v4()));
    let mut staged_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .map_err(|source| AppError::infrastructure_with_source("暂存 Git 忽略规则失败", source))?;
    let write_result = staged_file
        .write_all(&updated)
        .and_then(|()| staged_file.sync_all());
    drop(staged_file);
    if let Err(source) = write_result {
        let _ = fs::remove_file(&staged);
        return Err(AppError::infrastructure_with_source(
            "暂存 Git 忽略规则失败",
            source,
        ));
    }

    let current_before_commit = match fs::read(&destination) {
        Ok(source) => Some(source),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(source) => {
            let _ = fs::remove_file(&staged);
            return Err(AppError::infrastructure_with_source(
                "复核 Git 忽略规则失败",
                source,
            ));
        }
    };
    if current_before_commit.as_deref() != metadata.as_ref().map(|_| original.as_slice()) {
        let _ = fs::remove_file(&staged);
        return Err(AppError::conflict("Git 忽略规则已被外部修改，请刷新后重试"));
    }

    let mode = if metadata.is_some() {
        CommitMode::ReplaceExisting
    } else {
        CommitMode::CreateNew
    };
    match commit_staged_file(&staged, &destination, mode) {
        Ok(()) => Ok(true),
        Err(AtomicCommitError::NotCommitted(source)) => {
            let _ = fs::remove_file(&staged);
            Err(AppError::infrastructure_with_source(
                "提交 Git 忽略规则失败",
                source,
            ))
        }
        Err(AtomicCommitError::CommitUnknown(source)) => match fs::read(&destination) {
            Ok(actual) if actual == updated => Ok(true),
            _ => Err(AppError::outcome_unknown_with_source(
                "Git 忽略规则提交结果未知",
                source,
            )),
        },
    }
}
