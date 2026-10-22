//! 应用用例对外返回的稳定错误码与安全文案。
//!
//! @author kongweiguang

use std::{
    error::Error,
    fmt::{Debug, Display, Formatter},
    sync::Arc,
};

use kerminal_domain::DomainError;

/// 桌面与 MCP 共用的稳定错误类别；序列化端只应输出此枚举和 `public_message`。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicErrorCode {
    Invalid,
    NotFound,
    Unsupported,
    CapabilityNotSupported,
    Conflict,
    Cancelled,
    Infrastructure,
    OutcomeUnknown,
    AuthenticationFailed,
    HostKeyUnknown,
    HostKeyChanged,
    HostKeyTrustMismatch,
    Timeout,
    RemoteExit,
    ConfigurationCorrupt,
}

impl PublicErrorCode {
    /// 返回协议稳定的英文机器码，便于桌面和外部 MCP host 做分支处理。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::NotFound => "not_found",
            Self::Unsupported => "unsupported",
            Self::CapabilityNotSupported => "capability_not_supported",
            Self::Conflict => "conflict",
            Self::Cancelled => "cancelled",
            Self::Infrastructure => "infrastructure",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::AuthenticationFailed => "authentication_failed",
            Self::HostKeyUnknown => "host_key_unknown",
            Self::HostKeyChanged => "host_key_changed",
            Self::HostKeyTrustMismatch => "host_key_trust_mismatch",
            Self::Timeout => "timeout",
            Self::RemoteExit => "remote_exit",
            Self::ConfigurationCorrupt => "configuration_corrupt",
        }
    }
}

/// 应用层错误保留可审计的错误源，同时让 Display/Debug 只输出安全业务文案。
#[derive(Clone)]
pub enum AppError {
    Invalid {
        context: &'static str,
    },
    NotFound {
        context: &'static str,
    },
    Unsupported {
        context: &'static str,
    },
    CapabilityNotSupported {
        capability: String,
    },
    Conflict {
        context: &'static str,
    },
    Cancelled {
        context: &'static str,
    },
    Infrastructure {
        context: &'static str,
        source: Option<Arc<dyn Error + Send + Sync>>,
    },
    /// 远端已建立的交互连接意外断开；只有具备此显式类型的错误可触发会话重连。
    RetryableRemoteDisconnect {
        source: Arc<dyn Error + Send + Sync>,
    },
    OutcomeUnknown {
        context: &'static str,
        source: Option<Arc<dyn Error + Send + Sync>>,
    },
    DiagnosticSource {
        code: PublicErrorCode,
        message: &'static str,
        source: Arc<dyn Error + Send + Sync>,
    },
    AuthenticationFailed,
    HostKeyUnknown,
    HostKeyChanged,
    HostKeyTrustMismatch,
    Timeout {
        operation: &'static str,
    },
    RemoteExit {
        status: Option<i32>,
    },
    ConfigurationCorrupt {
        area: &'static str,
    },
}

/// 用例服务共享的错误返回类型。
pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    /// 固定文案只能来自源码常量，避免调用方把主机名、路径或凭据拼进公开错误。
    pub fn invalid(context: &'static str) -> Self {
        Self::Invalid { context }
    }

    /// 保留稳定的“未找到”类别，并要求公开文案使用不含输入值的固定说明。
    pub fn not_found(context: &'static str) -> Self {
        Self::NotFound { context }
    }

    /// 对不支持的业务意图给出明确分类，供 UI 展示能力边界。
    pub fn unsupported(context: &'static str) -> Self {
        Self::Unsupported { context }
    }

    /// 表示当前协议能力不足；能力名是固定协议术语，不包含用户输入。
    pub fn capability_not_supported(capability: &'static str) -> Self {
        Self::CapabilityNotSupported {
            capability: capability.to_owned(),
        }
    }

    /// 表示状态冲突，调用方可按用例决定刷新、重试或人工处理。
    pub fn conflict(context: &'static str) -> Self {
        Self::Conflict { context }
    }

    /// 把用户主动取消与传输故障区分开，避免取消误报为失败。
    pub fn cancelled(context: &'static str) -> Self {
        Self::Cancelled { context }
    }

    /// 记录固定的业务失败上下文；动态底层原因应使用带 source 的构造器。
    pub fn infrastructure(context: &'static str) -> Self {
        Self::Infrastructure {
            context,
            source: None,
        }
    }

    /// 保留底层错误链供内部诊断，同时让 Display 与 Debug 不展开可能敏感的源内容。
    pub fn infrastructure_with_source<E>(context: &'static str, source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self::Infrastructure {
            context,
            source: Some(Arc::new(source)),
        }
    }

    /// 用专用错误分支标记远端交互会话断线，同时保留脱敏之外的 typed 诊断 source。
    ///
    /// 该分类只服务于允许自动恢复的前台会话；调用方不能通过通用 infrastructure 错误或
    /// `with_source` 推导可重连性。
    ///
    /// @author kongweiguang
    pub fn retryable_remote_disconnect<E>(source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self::RetryableRemoteDisconnect {
            source: Arc::new(source),
        }
    }

    /// 只匹配专用断线 variant，避免 Manager 把认证、超时、取消和普通 I/O 错误误当成重连信号。
    ///
    /// @author kongweiguang
    pub fn is_retryable_remote_disconnect(&self) -> bool {
        matches!(self, Self::RetryableRemoteDisconnect { .. })
    }

    /// 提交动作已到达系统边界但回读前无法证明结果，调用方必须保留待确认引用与恢复材料。
    pub fn outcome_unknown(context: &'static str) -> Self {
        Self::OutcomeUnknown {
            context,
            source: None,
        }
    }

    /// 保留未知提交结果的底层原因，确保外围编排不会误当作可安全回滚的确定失败。
    pub fn outcome_unknown_with_source<E>(context: &'static str, source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self::OutcomeUnknown {
            context,
            source: Some(Arc::new(source)),
        }
    }

    /// 给任意稳定业务分类附带脱敏诊断原因，避免校验或恢复错误丢失内部 cause。
    pub fn with_source<E>(self, source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        let code = self.public_code();
        let message = self.public_message();
        Self::DiagnosticSource {
            code,
            message,
            source: Arc::new(source),
        }
    }

    /// 认证失败为单独稳定类别，避免 UI 或 MCP 根据底层 SSH 文案猜测。
    pub fn authentication_failed() -> Self {
        Self::AuthenticationFailed
    }

    /// 未知主机指纹需走显式信任流程，不得自动改写 known_hosts。
    pub fn host_key_unknown() -> Self {
        Self::HostKeyUnknown
    }

    /// 已保存指纹发生变化时必须阻断连接并提示用户复核。
    pub fn host_key_changed() -> Self {
        Self::HostKeyChanged
    }

    /// 二次握手和用户确认值不匹配时阻断写入可信列表。
    pub fn host_key_trust_mismatch() -> Self {
        Self::HostKeyTrustMismatch
    }

    /// 区分操作超时和一般网络故障，便于界面提供重试语义。
    pub fn timeout(operation: &'static str) -> Self {
        Self::Timeout { operation }
    }

    /// 保留远端退出状态供命令结果使用，避免把非零退出误判成连接故障。
    pub fn remote_exit(status: Option<i32>) -> Self {
        Self::RemoteExit { status }
    }

    /// 配置损坏使用单独错误码，允许应用进入只读恢复态而非覆盖原文件。
    pub fn configuration_corrupt(area: &'static str) -> Self {
        Self::ConfigurationCorrupt { area }
    }

    /// 返回不含动态上下文或底层错误文本的稳定机器码。
    ///
    /// @author kongweiguang
    pub const fn public_code(&self) -> PublicErrorCode {
        match self {
            Self::Invalid { .. } => PublicErrorCode::Invalid,
            Self::NotFound { .. } => PublicErrorCode::NotFound,
            Self::Unsupported { .. } => PublicErrorCode::Unsupported,
            Self::CapabilityNotSupported { .. } => PublicErrorCode::CapabilityNotSupported,
            Self::Conflict { .. } => PublicErrorCode::Conflict,
            Self::Cancelled { .. } => PublicErrorCode::Cancelled,
            Self::Infrastructure { .. } | Self::RetryableRemoteDisconnect { .. } => {
                PublicErrorCode::Infrastructure
            }
            Self::OutcomeUnknown { .. } => PublicErrorCode::OutcomeUnknown,
            Self::DiagnosticSource { code, .. } => *code,
            Self::AuthenticationFailed => PublicErrorCode::AuthenticationFailed,
            Self::HostKeyUnknown => PublicErrorCode::HostKeyUnknown,
            Self::HostKeyChanged => PublicErrorCode::HostKeyChanged,
            Self::HostKeyTrustMismatch => PublicErrorCode::HostKeyTrustMismatch,
            Self::Timeout { .. } => PublicErrorCode::Timeout,
            Self::RemoteExit { .. } => PublicErrorCode::RemoteExit,
            Self::ConfigurationCorrupt { .. } => PublicErrorCode::ConfigurationCorrupt,
        }
    }

    /// 返回可直接给用户看的安全文案；只采用受控源码常量，不展示路径、命令或 source。
    ///
    /// @author kongweiguang
    pub const fn public_message(&self) -> &'static str {
        match self {
            Self::Invalid { context }
            | Self::NotFound { context }
            | Self::Unsupported { context }
            | Self::Conflict { context }
            | Self::Cancelled { context }
            | Self::Infrastructure { context, .. }
            | Self::OutcomeUnknown { context, .. } => context,
            Self::RetryableRemoteDisconnect { .. } => "远端终端连接意外中断，可重试",
            Self::DiagnosticSource { message, .. } => message,
            Self::CapabilityNotSupported { .. } => "当前连接不支持此能力",
            Self::AuthenticationFailed => "连接认证失败",
            Self::HostKeyUnknown => "主机指纹尚未确认",
            Self::HostKeyChanged => "主机指纹已变化，连接已阻断",
            Self::HostKeyTrustMismatch => "主机指纹复核不匹配，未保存信任",
            Self::Timeout { .. } => "操作超时",
            Self::RemoteExit { .. } => "远端命令已退出",
            Self::ConfigurationCorrupt { .. } => "配置文件损坏，需要恢复",
        }
    }
}

impl From<DomainError> for AppError {
    /// 转换领域校验错误时不保留未经验证的 ID 内容。
    fn from(_error: DomainError) -> Self {
        Self::invalid("领域标识无效")
    }
}

impl Debug for AppError {
    /// Debug 输出稳定码和安全文案，底层 source 由调用方显式审计时再访问。
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppError")
            .field("code", &self.public_code().as_str())
            .field("message", &self.public_message())
            .finish()
    }
}

impl Display for AppError {
    /// Display 常被 IPC/MCP 直接回传，因此只输出面向用户的脱敏文案。
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.public_message())
    }
}

impl Error for AppError {
    /// 底层 source 只在内部错误链中可见，不参与 Display/Debug 或协议序列化。
    ///
    /// @author kongweiguang
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Infrastructure {
                source: Some(source),
                ..
            }
            | Self::RetryableRemoteDisconnect { source }
            | Self::OutcomeUnknown {
                source: Some(source),
                ..
            }
            | Self::DiagnosticSource { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}
