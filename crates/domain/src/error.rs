//! 值对象共享的领域校验错误。
//!
//! @author kongweiguang

use std::fmt::{Display, Formatter};

/// 只记录错误类别，不保留可能含敏感信息的 ID 原文。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DomainError {
    /// 标识为空。
    EmptyIdentity,
    /// 标识含控制字符；文件名限制由具体存储适配器单独执行。
    UnsafeIdentity,
}

impl Display for DomainError {
    /// 使用不包含原始 ID 的稳定文案，避免校验失败时泄漏输入内容。
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::EmptyIdentity => "标识不能为空",
            Self::UnsafeIdentity => "标识不能包含控制字符",
        })
    }
}

impl std::error::Error for DomainError {}
