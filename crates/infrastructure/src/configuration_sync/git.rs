//! 可取消的 Git 子进程执行器，不返回原始 stderr 或远端 URL。
//!
//! @author kongweiguang

use std::path::Path;
use std::{ffi::OsString, io};
use tokio_util::sync::CancellationToken;

/// 对 Git 故障分类，但不保留可能包含凭据的诊断文本。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GitFailureKind {
    Authentication,
    Network,
    Conflict,
    MissingUpstream,
    InvalidRepository,
    IdentityNotConfigured,
    Other,
}

/// 将嵌套清理错误归并到不会暴露 Git 原始输出的应用错误类别。
#[derive(Clone, Copy)]
enum GitAppErrorClass {
    Cancelled,
    Authentication,
    Network,
    Conflict,
    MissingUpstream,
    InvalidRepository,
    IdentityNotConfigured,
    OutcomeUnknown,
    GitUnavailable,
    Other,
}

/// 脱敏后的 Git 故障；未知结果可以内部保留已排空的有界输出供恢复审查。
pub(super) enum GitCommandError {
    Launch(io::Error),
    Executor(kerminal_application::AppError),
    OutputLimit,
    Cancelled,
    /// 不完整输入时保留已排空的有界回执；cause 维持原执行错误链，格式化始终脱敏。
    OutcomeUnknown {
        receipt: Option<Box<GitOutput>>,
        cause: Option<kerminal_application::AppError>,
    },
    Failed {
        kind: GitFailureKind,
    },
}

impl std::fmt::Debug for GitCommandError {
    /// Debug 仅暴露错误码与安全元数据，避免 URL、路径和 Git 输出进入日志或断言。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = formatter.debug_struct("GitCommandError");
        match self {
            Self::Launch(error) => debug.field("kind", &error.kind()).finish(),
            Self::Executor(error) => debug
                .field("kind", &"Executor")
                .field("code", &error.public_code().as_str())
                .finish(),
            Self::OutputLimit => debug.field("kind", &"OutputLimit").finish(),
            Self::Cancelled => debug.field("kind", &"Cancelled").finish(),
            Self::OutcomeUnknown { receipt, cause } => {
                debug.field("kind", &"OutcomeUnknown");
                debug.field("receipt", receipt);
                if let Some(cause) = cause {
                    debug.field("cause_code", &cause.public_code().as_str());
                }
                debug.finish()
            }
            Self::Failed { kind } => debug
                .field("kind", &"Failed")
                .field("failure_kind", kind)
                .finish(),
        }
    }
}

impl std::fmt::Display for GitCommandError {
    /// 不把 URL、路径、用户名或 Git 原始输出放入面向用户的错误文案。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Launch(_) => "无法启动 Git",
            Self::Executor(_) => "Git 命令执行失败",
            Self::OutputLimit => "Git 结果超过安全读取上限",
            Self::Cancelled => "Git 操作已取消",
            Self::OutcomeUnknown { .. } => "Git 命令结果未知",
            Self::Failed { kind } => match kind {
                GitFailureKind::Authentication => "Git 远端认证失败",
                GitFailureKind::Network => "Git 远端网络操作失败",
                GitFailureKind::Conflict => "Git 操作遇到冲突",
                GitFailureKind::MissingUpstream => "Git 尚未配置上游分支",
                GitFailureKind::InvalidRepository => "配置目录不是可用 Git 仓库",
                GitFailureKind::IdentityNotConfigured => "Git 尚未配置提交身份",
                GitFailureKind::Other => "Git 操作失败",
            },
        })
    }
}

impl std::error::Error for GitCommandError {
    /// 仅保留操作系统进程和管道错误作为 cause，Git stderr 不会透出。
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Launch(error) => Some(error),
            Self::Executor(error) => Some(error),
            Self::OutcomeUnknown { cause, .. } => cause
                .as_ref()
                .map(|source| source as &(dyn std::error::Error + 'static)),
            Self::OutputLimit | Self::Cancelled | Self::Failed { .. } => None,
        }
    }
}

/// 捕获结果仅供基础设施内部使用，不可将 stderr 转发给 UI 或 MCP。
pub(super) struct GitOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr_prefix: Vec<u8>,
}

impl std::fmt::Debug for GitOutput {
    /// 输出诊断只包含有界字节数，不把 Git stdout/stderr 内容写入错误日志。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GitOutput")
            .field("code", &self.code)
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_prefix_bytes", &self.stderr_prefix.len())
            .finish()
    }
}

/// 不经 shell 启动 Git，持续排空两个输出管道，并在子进程运行期间检查取消。
pub(super) fn run_git(
    runner: &super::runner::GitCommandRuntime,
    root: &Path,
    arguments: &[OsString],
    stdin_data: Option<Vec<u8>>,
    cancellation: &CancellationToken,
    use_editor_sentinel: bool,
    allow_nonzero: bool,
) -> Result<GitOutput, GitCommandError> {
    runner.run_git(
        root,
        arguments,
        stdin_data,
        cancellation,
        use_editor_sentinel,
        allow_nonzero,
    )
}

/// 构造拥有所有权的操作系统参数，使路径数据从 stdin 传入而不经过 shell 解析。
pub(super) fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

/// 将进程故障转换为稳定应用错误分类，并保留脱敏后的 cause。
pub(super) fn to_app_error(
    context: &'static str,
    error: GitCommandError,
) -> kerminal_application::AppError {
    use kerminal_application::AppError;

    match public_error_class(&error) {
        GitAppErrorClass::Cancelled => AppError::cancelled("Git 操作已取消").with_source(error),
        GitAppErrorClass::Authentication => AppError::authentication_failed().with_source(error),
        GitAppErrorClass::Network => {
            AppError::infrastructure_with_source("Git 远端网络操作失败", error)
        }
        GitAppErrorClass::Conflict => AppError::conflict("Git 操作遇到冲突").with_source(error),
        GitAppErrorClass::MissingUpstream => {
            AppError::conflict("Git 尚未配置上游分支").with_source(error)
        }
        GitAppErrorClass::InvalidRepository => {
            AppError::conflict("配置目录不是可用 Git 仓库").with_source(error)
        }
        GitAppErrorClass::IdentityNotConfigured => {
            AppError::invalid("请先配置 Git user.name 和 user.email").with_source(error)
        }
        GitAppErrorClass::OutcomeUnknown => AppError::outcome_unknown(context).with_source(error),
        GitAppErrorClass::GitUnavailable => {
            AppError::unsupported("系统未安装 Git").with_source(error)
        }
        GitAppErrorClass::Other => AppError::infrastructure_with_source(context, error),
    }
}

/// 将运维执行器的取消与未知结果保留为 Git 同步的对应类型，其他失败保留原始 source。
pub(super) fn map_executor_error(error: kerminal_application::AppError) -> GitCommandError {
    match error.public_code() {
        kerminal_application::PublicErrorCode::Cancelled => GitCommandError::Cancelled,
        kerminal_application::PublicErrorCode::OutcomeUnknown => GitCommandError::OutcomeUnknown {
            receipt: None,
            cause: Some(error),
        },
        _ if source_has_kind(&error, io::ErrorKind::NotFound) => GitCommandError::Launch(
            io::Error::new(io::ErrorKind::NotFound, "Git executable not found"),
        ),
        _ => GitCommandError::Executor(error),
    }
}

/// 找出运维命令错误链中的启动缺失，使现有“未安装 Git”状态继续可区分。
fn source_has_kind(error: &kerminal_application::AppError, kind: io::ErrorKind) -> bool {
    let mut source = std::error::Error::source(error);
    while let Some(error) = source {
        if error
            .downcast_ref::<io::Error>()
            .is_some_and(|source| source.kind() == kind)
        {
            return true;
        }
        source = error.source();
    }
    false
}

/// 清理出错时仍按最初的取消、网络或冲突原因选择稳定公开错误码。
fn public_error_class(error: &GitCommandError) -> GitAppErrorClass {
    match error {
        GitCommandError::Cancelled => GitAppErrorClass::Cancelled,
        GitCommandError::Failed { kind, .. } => match kind {
            GitFailureKind::Authentication => GitAppErrorClass::Authentication,
            GitFailureKind::Network => GitAppErrorClass::Network,
            GitFailureKind::Conflict => GitAppErrorClass::Conflict,
            GitFailureKind::MissingUpstream => GitAppErrorClass::MissingUpstream,
            GitFailureKind::InvalidRepository => GitAppErrorClass::InvalidRepository,
            GitFailureKind::IdentityNotConfigured => GitAppErrorClass::IdentityNotConfigured,
            GitFailureKind::Other => GitAppErrorClass::Other,
        },
        GitCommandError::OutcomeUnknown { .. } => GitAppErrorClass::OutcomeUnknown,
        GitCommandError::Executor(error)
            if error.public_code() == kerminal_application::PublicErrorCode::OutcomeUnknown =>
        {
            GitAppErrorClass::OutcomeUnknown
        }
        GitCommandError::Executor(error)
            if error.public_code() == kerminal_application::PublicErrorCode::Cancelled =>
        {
            GitAppErrorClass::Cancelled
        }
        GitCommandError::Launch(source) if source.kind() == io::ErrorKind::NotFound => {
            GitAppErrorClass::GitUnavailable
        }
        _ => GitAppErrorClass::Other,
    }
}

/// 仅对有限诊断前缀做分类，完成后丢弃原始字节。
pub(super) fn classify_failure(stderr: &[u8]) -> GitFailureKind {
    let diagnostic = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if [
        "authentication failed",
        "could not read username",
        "permission denied (publickey)",
        "access denied",
    ]
    .iter()
    .any(|needle| diagnostic.contains(needle))
    {
        GitFailureKind::Authentication
    } else if [
        "could not resolve host",
        "failed to connect",
        "connection timed out",
        "network is unreachable",
        "connection reset",
    ]
    .iter()
    .any(|needle| diagnostic.contains(needle))
    {
        GitFailureKind::Network
    } else if [
        "conflict",
        "would be overwritten",
        "non-fast-forward",
        "updates were rejected",
    ]
    .iter()
    .any(|needle| diagnostic.contains(needle))
    {
        GitFailureKind::Conflict
    } else if diagnostic.contains("no upstream") || diagnostic.contains("no tracking information") {
        GitFailureKind::MissingUpstream
    } else if diagnostic.contains("not a git repository") {
        GitFailureKind::InvalidRepository
    } else if diagnostic.contains("author identity unknown")
        || diagnostic.contains("unable to auto-detect email address")
    {
        GitFailureKind::IdentityNotConfigured
    } else {
        GitFailureKind::Other
    }
}

#[cfg(test)]
#[path = "../../tests/unit/configuration_sync_git.rs"]
mod tests;
