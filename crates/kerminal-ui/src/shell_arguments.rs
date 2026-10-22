//! 基于已确认 shell 方言安全引用单个命令参数。
//!
//! @author kongweiguang

use kerminal_domain::connections::RemoteShellDialect;

/// 把不可信参数限制在一个 shell 字面量内；未知方言失败关闭，不猜测目标 shell。
///
/// @author kongweiguang
pub(crate) fn quote_shell_argument(value: &str, dialect: RemoteShellDialect) -> Option<String> {
    match dialect {
        RemoteShellDialect::Posix => Some(format!("'{}'", value.replace('\'', "'\\''"))),
        RemoteShellDialect::Fish => {
            let escaped = value.replace('\\', "\\\\").replace('\'', "\\'");
            Some(format!("'{escaped}'"))
        }
        RemoteShellDialect::PowerShell => Some(format!("'{}'", value.replace('\'', "''"))),
        RemoteShellDialect::Unknown => None,
    }
}
