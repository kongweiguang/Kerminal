//! HTTP CONNECT 与 SOCKS5 stream 建立；认证材料只短暂存在于零化缓冲中。
//!
//! @author kongweiguang

use std::fmt::Write as _;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use kerminal_application::{
    AppError, AppResult,
    connections::{AsyncDuplex, CredentialResolver, ResolvedCredential},
};
use kerminal_domain::connections::{HostEndpoint, ProxyProfile, ProxyProtocol};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

/// 先连接代理，再通过代理建立到第一跳 SSH peer 的原始字节流；每个协议分支直接返回同一结果类型。
///
/// @author kongweiguang
pub(super) async fn connect_via_proxy(
    proxy: &ProxyProfile,
    target: &HostEndpoint,
    credentials: &dyn CredentialResolver,
    cancel: &CancellationToken,
) -> AppResult<Box<dyn AsyncDuplex>> {
    validate_proxy_endpoint(&proxy.endpoint)?;
    validate_target(target)?;
    let password = if let Some(reference) = &proxy.password {
        match credentials.resolve(reference, cancel).await? {
            ResolvedCredential::Password(secret) => Some(secret),
            _ => return Err(AppError::invalid("代理凭据类型无效")),
        }
    } else {
        None
    };
    let mut stream = tokio::select! {
        _ = cancel.cancelled() => return Err(AppError::cancelled("代理连接已取消")),
        result = TcpStream::connect((proxy.endpoint.host.as_str(), proxy.endpoint.port)) => {
            result.map_err(|error| AppError::infrastructure_with_source("无法连接代理服务器", error))?
        }
    };
    stream
        .set_nodelay(true)
        .map_err(|error| AppError::infrastructure_with_source("代理 socket 初始化失败", error))?;

    match proxy.protocol {
        ProxyProtocol::HttpConnect => {
            http_connect(
                &mut stream,
                proxy,
                target,
                password.as_ref().map(|value| value.as_str()),
                cancel,
            )
            .await?;
            let stream: Box<dyn AsyncDuplex> = Box::new(stream);
            Ok(stream)
        }
        ProxyProtocol::Socks5 => {
            socks5_connect(
                stream,
                proxy,
                target,
                password.as_ref().map(|value| value.as_str()),
                cancel,
            )
            .await
        }
    }
}

/// 使用 CONNECT 建立代理隧道，并逐字节读取响应头以免吞掉 SSH 握手数据。
///
/// @author kongweiguang
async fn http_connect(
    stream: &mut TcpStream,
    proxy: &ProxyProfile,
    target: &HostEndpoint,
    password: Option<&str>,
    cancel: &CancellationToken,
) -> AppResult<()> {
    let authority = format_authority(&target.host, target.port);
    let mut request = Zeroizing::new(String::new());
    write!(
        request,
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n"
    )
    .expect("writing to String cannot fail");
    if let Some(password) = password {
        if proxy
            .endpoint
            .username
            .chars()
            .any(|character| matches!(character, '\r' | '\n'))
            || password
                .chars()
                .any(|character| matches!(character, '\r' | '\n'))
        {
            return Err(AppError::invalid("代理认证字段无效"));
        }
        let basic = Zeroizing::new(format!("{}:{}", proxy.endpoint.username, password));
        let encoded = Zeroizing::new(STANDARD.encode(basic.as_bytes()));
        write!(
            request,
            "Proxy-Authorization: Basic {}\r\n",
            encoded.as_str()
        )
        .expect("writing to String cannot fail");
    }
    request.push_str("Connection: keep-alive\r\n\r\n");
    tokio::select! {
        _ = cancel.cancelled() => return Err(AppError::cancelled("代理握手已取消")),
        result = stream.write_all(request.as_bytes()) => result.map_err(|error| AppError::infrastructure_with_source("HTTP CONNECT 请求发送失败", error))?,
    }
    let mut response = Vec::with_capacity(128);
    let mut byte = [0_u8; 1];
    while response.len() < 8192 {
        tokio::select! {
            _ = cancel.cancelled() => return Err(AppError::cancelled("代理握手已取消")),
            result = stream.read_exact(&mut byte) => result.map_err(|error| AppError::infrastructure_with_source("HTTP CONNECT 响应读取失败", error))?,
        };
        response.push(byte[0]);
        if response.ends_with(b"\r\n\r\n") {
            let status = response
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap_or_default();
            let status = String::from_utf8_lossy(status);
            if status.split_whitespace().nth(1) == Some("200") {
                return Ok(());
            }
            return Err(AppError::infrastructure("HTTP 代理拒绝 CONNECT 请求"));
        }
    }
    Err(AppError::infrastructure("HTTP 代理响应头超出限制"))
}

/// 复用 tokio-socks 的认证和协议实现，并保留本 adapter 的取消与目标地址校验边界。
async fn socks5_connect(
    stream: TcpStream,
    proxy: &ProxyProfile,
    target: &HostEndpoint,
    password: Option<&str>,
    cancel: &CancellationToken,
) -> AppResult<Box<dyn AsyncDuplex>> {
    validate_target(target)?;
    let connected = tokio::select! {
        _ = cancel.cancelled() => return Err(AppError::cancelled("SOCKS5 proxy handshake cancelled")),
        result = connect_socks5(stream, proxy, target, password) => result.map_err(|error| AppError::infrastructure_with_source("SOCKS5 proxy handshake failed", error))?,
    };
    Ok(Box::new(connected.into_inner()))
}

/// 在单一 future 中分派认证方式，避免 opaque future 类型差异泄漏到取消选择。
async fn connect_socks5(
    stream: TcpStream,
    proxy: &ProxyProfile,
    target: &HostEndpoint,
    password: Option<&str>,
) -> Result<tokio_socks::tcp::socks5::Socks5Stream<TcpStream>, tokio_socks::Error> {
    if let Some(password) = password {
        tokio_socks::tcp::socks5::Socks5Stream::connect_with_password_and_socket(
            stream,
            (target.host.as_str(), target.port),
            proxy.endpoint.username.as_str(),
            password,
        )
        .await
    } else {
        tokio_socks::tcp::socks5::Socks5Stream::connect_with_socket(
            stream,
            (target.host.as_str(), target.port),
        )
        .await
    }
}

/// 检查代理只依赖 host/port；其 username 允许为空以表达无认证代理。
fn validate_proxy_endpoint(endpoint: &HostEndpoint) -> AppResult<()> {
    if endpoint.host.trim().is_empty()
        || endpoint.port == 0
        || endpoint.host.chars().any(char::is_control)
        || endpoint.username.chars().any(char::is_control)
    {
        return Err(AppError::invalid("代理 endpoint 无效"));
    }
    Ok(())
}

/// 校验 SSH 目标 host/port 并确保其 authority 不会改变 HTTP 或 SOCKS 协议结构。
fn validate_target(target: &HostEndpoint) -> AppResult<()> {
    target
        .validate()
        .map_err(|_| AppError::invalid("SSH endpoint 无效"))
}

/// 为 IPv6 literal 补上方括号，生成 HTTP CONNECT 所需的 authority。
fn format_authority(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
#[path = "../../tests/unit/proxy.rs"]
mod tests;
