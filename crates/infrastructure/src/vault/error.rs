//! Vault adapter 的安全错误边界。
//!
//! @author kongweiguang

use std::{error::Error, fmt, io};

use kerminal_application::AppError;

/// 将未提交、提交未知和恢复故障分开表达，不携带 key、plaintext 或 TOML 原文。
#[derive(Debug)]
pub enum VaultError {
    Io(io::Error),
    CommitNotCommitted(Box<VaultError>),
    CommitOutcomeUnknown(Box<VaultError>),
    InvalidData,
    MissingKey,
    KeyMismatch,
    MissingEntry,
    UnsafePath,
    ExportTargetExists,
    ExportTargetMissing,
    RecoveryRequired,
    LockUnavailable,
}

impl fmt::Display for VaultError {
    /// 返回稳定摘要，避免把解析上下文或凭据标识拼入公开错误文本。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Io(_) => "vault storage I/O failed",
            Self::CommitNotCommitted(_) => "vault change was not committed",
            Self::CommitOutcomeUnknown(_) => "vault commit outcome is unknown",
            Self::InvalidData => "vault data is invalid",
            Self::MissingKey => "vault key is missing",
            Self::KeyMismatch => "vault key does not match encrypted entries",
            Self::MissingEntry => "vault entry is missing",
            Self::UnsafePath => "vault path is not a regular file",
            Self::ExportTargetExists => "vault key export destination already exists",
            Self::ExportTargetMissing => "vault key export destination does not exist",
            Self::RecoveryRequired => "vault transaction recovery is required",
            Self::LockUnavailable => "vault storage lock is unavailable",
        })
    }
}

impl Error for VaultError {
    /// 只保留文件系统错误作为诊断来源，不暴露解析错误中的 TOML 文本。
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::CommitNotCommitted(error) | Self::CommitOutcomeUnknown(error) => {
                Some(error.as_ref())
            }
            _ => None,
        }
    }
}

impl From<io::Error> for VaultError {
    /// 将文件错误保留为内部诊断，但让 Display 始终不含路径或文件内容。
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// 让上层按确定未提交、结果未知或恢复状态选择后续动作，不把未知映射成可盲重试冲突。
pub(super) fn to_app_error(context: &'static str, error: VaultError) -> AppError {
    match error {
        error @ VaultError::CommitNotCommitted(_) => {
            AppError::conflict("凭据修改未提交，可在确认状态后重试").with_source(error)
        }
        error @ (VaultError::ExportTargetExists | VaultError::ExportTargetMissing) => {
            AppError::conflict("密钥导出目标状态与明确选择的覆盖方式不匹配").with_source(error)
        }
        error @ (VaultError::CommitOutcomeUnknown(_) | VaultError::RecoveryRequired) => {
            AppError::outcome_unknown_with_source(
                "凭据修改结果未知，请先检查恢复状态，不要重试",
                error,
            )
        }
        error => AppError::infrastructure_with_source(context, error),
    }
}
