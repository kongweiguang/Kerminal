//! OSC 8 外链协议白名单；终端内容不会直接获得文件、应用或脚本执行权限。
//!
//! @author kongweiguang

use url::Url;

const MAX_TERMINAL_LINK_BYTES: usize = 4096;

/// 只允许通过验证的 HTTP(S) 链接进入桌面外部打开 API。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SafeTerminalLink(String);

impl SafeTerminalLink {
    /// 返回规范化 URI，供宿主 OS 打开器使用；构造只能通过白名单验证函数。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 消费已验证链接并交给宿主处理，不创建额外 URI 包装或重复解析。
    pub fn into_string(self) -> String {
        self.0
    }
}

/// 外部终端内容链接被拒绝的稳定原因，不携带原始 URI 或其可能包含的敏感信息。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalLinkError {
    TooLong,
    ForbiddenCharacters,
    InvalidUri,
    UnsupportedScheme,
    MissingHost,
    UserInfo,
}

/// 用 URL 标准解析器归一化 OSC 8 内容，并只接受无凭据的 http/https 主机链接。
pub fn validate_terminal_link(raw_uri: &str) -> Result<SafeTerminalLink, TerminalLinkError> {
    if raw_uri.len() > MAX_TERMINAL_LINK_BYTES {
        return Err(TerminalLinkError::TooLong);
    }
    if raw_uri
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
        || raw_uri.contains('\\')
    {
        return Err(TerminalLinkError::ForbiddenCharacters);
    }

    let parsed = Url::parse(raw_uri).map_err(|_| TerminalLinkError::InvalidUri)?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(TerminalLinkError::UnsupportedScheme);
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(TerminalLinkError::MissingHost);
    }
    if !parsed.username().is_empty() || parsed.password().is_some() || has_user_info(raw_uri) {
        return Err(TerminalLinkError::UserInfo);
    }
    Ok(SafeTerminalLink(parsed.to_string()))
}

/// 拒绝 authority 中任何 @，包含空用户名形式以避免 URL parser 归一化时丢失提示。
fn has_user_info(raw_uri: &str) -> bool {
    let Some((_, authority)) = raw_uri.split_once("://") else {
        return false;
    };
    let authority = authority.split(['/', '?', '#']).next().unwrap_or_default();
    authority.contains('@')
}
