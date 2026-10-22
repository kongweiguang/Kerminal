//! RDP endpoint validation and private temporary connection files.
//!
//! @author kongweiguang

#[cfg(any(target_os = "windows", target_os = "macos", test))]
use std::path::Path;
use std::path::PathBuf;

use kerminal_application::{AppError, AppResult};
use kerminal_domain::connections::HostEndpoint;
#[cfg(any(target_os = "windows", target_os = "macos", test))]
use uuid::Uuid;

#[cfg(any(target_os = "windows", target_os = "macos", test))]
const RDP_ARTIFACT_PREFIX: &str = "connection-";

/// 仅持有配置文件路径，不缓存或格式化任何凭据材料。
pub(super) struct TemporaryRdpArtifact {
    path: PathBuf,
    removed: bool,
}

impl TemporaryRdpArtifact {
    /// 供系统客户端读取配置；进程 owner 持有该值直至客户端退出。
    /// 该路径只在生成临时 RDP 文件的平台暴露，测试构建保留跨平台文件契约验证。
    ///
    /// @author kongweiguang
    #[cfg(any(target_os = "windows", target_os = "macos", test))]
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// 等待客户端退出后删除临时文件；NotFound 表示清理已完成。
    pub(super) async fn cleanup(mut self) -> AppResult<()> {
        let result = match tokio::fs::remove_file(&self.path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AppError::infrastructure_with_source(
                "RDP 临时文件清理失败",
                error,
            )),
        };
        if result.is_ok() {
            self.removed = true;
        }
        result
    }
}

impl Drop for TemporaryRdpArtifact {
    /// 正常路径显式回报清理错误，Drop 只负责异常展开时尽力移除残留文件。
    fn drop(&mut self) {
        if !self.removed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// 检查 endpoint 可被安全写入 RDP 配置或单个 argv 元素。
pub(super) fn validate_endpoint(endpoint: &HostEndpoint) -> AppResult<()> {
    if endpoint.host.trim().is_empty()
        || endpoint.host.chars().any(|character| {
            character.is_whitespace() || character.is_control() || matches!(character, '\r' | '\n')
        })
        || endpoint.port == 0
        || endpoint.username.chars().any(char::is_control)
    {
        return Err(AppError::invalid("RDP 连接地址或用户名无效"));
    }
    Ok(())
}

/// 按 RDP file 格式给 IPv6 地址加方括号，保留端口号的无歧义分隔。
pub(super) fn format_rdp_address(host: &str, port: u16) -> String {
    let host = host.trim();
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// 只把 DPAPI 密文写入 `password 51:b`，不让明文进入 RDP 文件或进程参数。
/// RDP 文件序列化仅供 Win/macOS 客户端使用，单元测试仍覆盖其凭据边界。
///
/// @author kongweiguang
#[cfg(any(target_os = "windows", target_os = "macos", test))]
pub(super) fn rdp_file_content(
    endpoint: &HostEndpoint,
    encrypted_password: Option<&str>,
    display: &kerminal_application::connections::management::RdpDisplayOptions,
) -> String {
    let mut lines = vec![
        format!(
            "full address:s:{}",
            format_rdp_address(&endpoint.host, endpoint.port)
        ),
        "authentication level:i:2".to_owned(),
        "enablecredsspsupport:i:1".to_owned(),
        "redirectclipboard:i:1".to_owned(),
        format!(
            "prompt for credentials:i:{}",
            if encrypted_password.is_some() { 0 } else { 1 }
        ),
        format!(
            "screen mode id:i:{}",
            if display.fullscreen { 2 } else { 1 }
        ),
    ];
    if !display.fullscreen
        && let (Some(width), Some(height)) = (display.width, display.height)
    {
        lines.push(format!("desktopwidth:i:{width}"));
        lines.push(format!("desktopheight:i:{height}"));
    }
    let username = endpoint.username.trim();
    if !username.is_empty() {
        lines.push(format!("username:s:{username}"));
    }
    if let Some(encrypted_password) = encrypted_password {
        lines.push(format!("password 51:b:{encrypted_password}"));
    }
    lines.join("\r\n") + "\r\n"
}

/// 创建唯一临时连接文件；Windows 由 CreateFileW 原子应用 owner-only DACL。
/// 临时文件只供使用 RDP 文件的 Win/macOS launcher 创建，测试保留其原子写入验证。
///
/// @author kongweiguang
#[cfg(any(target_os = "windows", target_os = "macos", test))]
pub(super) fn create_rdp_artifact(
    runtime_directory: &Path,
    endpoint: &HostEndpoint,
    encrypted_password: Option<&str>,
    display: &kerminal_application::connections::management::RdpDisplayOptions,
) -> AppResult<TemporaryRdpArtifact> {
    validate_endpoint(endpoint)?;
    display.validate()?;
    std::fs::create_dir_all(runtime_directory).map_err(|error| {
        AppError::infrastructure_with_source("无法创建 RDP 临时文件目录", error)
    })?;
    let path = runtime_directory.join(format!("{RDP_ARTIFACT_PREFIX}{}.rdp", Uuid::new_v4()));
    let content = rdp_file_content(endpoint, encrypted_password, display);
    write_private_file(&path, content.as_bytes())?;
    Ok(TemporaryRdpArtifact {
        path,
        removed: false,
    })
}

/// 创建文件时先限制访问，再写入内容；Unix 使用 owner-only mode，Windows 使用 owner-only DACL。
/// 仅在需要私有 RDP 文件的平台和单元测试中编译，避免 Linux FreeRDP 构建包含未用写入路径。
///
/// @author kongweiguang
#[cfg(any(target_os = "windows", target_os = "macos", test))]
fn write_private_file(path: &Path, contents: &[u8]) -> AppResult<()> {
    #[cfg(target_os = "windows")]
    {
        super::windows_security::write_owner_only_file(path, contents)
    }
    #[cfg(unix)]
    {
        use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt};
        let mut options = OpenOptions::new();
        options.create_new(true).write(true).mode(0o600);
        let mut file = options.open(path).map_err(|error| {
            AppError::infrastructure_with_source("无法创建受限 RDP 临时文件", error)
        })?;
        let result = file.write_all(contents).and_then(|()| file.flush());
        drop(file);
        finish_partial_file_write(path, result)
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        use std::{fs::OpenOptions, io::Write};
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        let mut file = options.open(path).map_err(|error| {
            AppError::infrastructure_with_source("无法创建 RDP 临时文件", error)
        })?;
        let result = file.write_all(contents).and_then(|()| file.flush());
        drop(file);
        finish_partial_file_write(path, result)
    }
}

/// 对已由当前任务创建的文件执行失败清理；报告写入和清理同时失败的情况。
/// 失败清理只属于临时 RDP 文件创建事务，Linux 生产路径不创建此类文件。
///
/// @author kongweiguang
#[cfg(any(target_os = "windows", target_os = "macos", test))]
pub(super) fn finish_partial_file_write(
    path: &Path,
    write_result: std::io::Result<()>,
) -> AppResult<()> {
    match write_result {
        Ok(()) => Ok(()),
        Err(write_error) => match std::fs::remove_file(path) {
            Ok(()) => Err(AppError::infrastructure_with_source(
                "无法写入 RDP 临时文件",
                write_error,
            )),
            Err(cleanup_error) if cleanup_error.kind() == std::io::ErrorKind::NotFound => Err(
                AppError::infrastructure_with_source("无法写入 RDP 临时文件", write_error),
            ),
            Err(cleanup_error) => Err(AppError::infrastructure_with_source(
                "RDP 临时文件写入与失败清理均未完成",
                FileWriteCleanupFailure {
                    write: write_error,
                    cleanup: cleanup_error,
                },
            )),
        },
    }
}

/// 同时保留临时文件写入错误和清理错误的内部 source。
#[cfg(any(target_os = "windows", target_os = "macos", test))]
#[derive(Debug)]
struct FileWriteCleanupFailure {
    write: std::io::Error,
    cleanup: std::io::Error,
}

#[cfg(any(target_os = "windows", target_os = "macos", test))]
impl std::fmt::Display for FileWriteCleanupFailure {
    /// 错误文本不包含 RDP 端点、文件路径或凭据。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "write: {}; cleanup: {}",
            self.write.kind(),
            self.cleanup.kind()
        )
    }
}

#[cfg(any(target_os = "windows", target_os = "macos", test))]
impl std::error::Error for FileWriteCleanupFailure {}
