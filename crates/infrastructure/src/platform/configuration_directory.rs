//! 固定用户配置目录的安全解析与操作系统打开器。
//!
//! @author kongweiguang

use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::Stdio,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    configuration_directory::{ConfigurationDirectoryKind, ConfigurationDirectoryPort},
};
use tokio_util::sync::CancellationToken;

/// 只持有 desktop composition 已解析的数据根，不接受 UI 提供的路径。
#[derive(Clone, Debug)]
pub struct SystemConfigurationDirectoryOpener {
    config_root: PathBuf,
}

impl SystemConfigurationDirectoryOpener {
    /// 将固定配置根注入平台适配器，使目录解析不依赖当前工作目录或用户环境变量。
    pub fn new(config_root: impl Into<PathBuf>) -> Self {
        Self {
            config_root: config_root.into(),
        }
    }
}

#[async_trait]
impl ConfigurationDirectoryPort for SystemConfigurationDirectoryOpener {
    /// 目录创建与规范化放入阻塞池，启动器进程由 Tokio 回收且只在启动前响应取消。
    async fn request_open(
        &self,
        kind: ConfigurationDirectoryKind,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("配置目录打开请求已取消"));
        }

        let config_root = self.config_root.clone();
        let resolution_cancellation = cancellation.clone();
        let directory = tokio::task::spawn_blocking(move || {
            if resolution_cancellation.is_cancelled() {
                return Err(AppError::cancelled("配置目录打开请求已取消"));
            }

            resolve_configuration_directory(&config_root, kind)
        })
        .await
        .map_err(|source| {
            AppError::infrastructure_with_source("配置目录打开任务意外终止", source)
        })??;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("配置目录打开请求已取消"));
        }
        spawn_directory_opener(&directory).await
    }
}

/// 只将受限配置目录身份映射到数据根或固定子目录，并拒绝逃逸或根目录别名。
///
/// @author kongweiguang
fn resolve_configuration_directory(
    config_root: &Path,
    kind: ConfigurationDirectoryKind,
) -> AppResult<PathBuf> {
    let canonical_root = fs::canonicalize(config_root).map_err(|source| {
        AppError::infrastructure_with_source("无法访问 Kerminal 配置目录", source)
    })?;
    if kind == ConfigurationDirectoryKind::Root {
        // Root 只负责打开已经存在的数据目录；与首次创建的片段目录分支隔离，避免竞态重建用户根目录。
        let metadata = fs::symlink_metadata(&canonical_root).map_err(|source| {
            AppError::infrastructure_with_source("无法检查 Kerminal 配置目录", source)
        })?;
        if metadata.file_type().is_symlink() {
            return Err(AppError::invalid("Kerminal 配置目录不能是符号链接"));
        }
        if !metadata.is_dir() {
            return Err(AppError::invalid("Kerminal 配置路径不是目录"));
        }
        return Ok(canonical_root);
    }

    let directory = canonical_root.join("snippets");

    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(AppError::invalid("片段配置目录不能是符号链接"));
        }
        Ok(_) => return Err(AppError::invalid("片段配置路径不是目录")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => match fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(AppError::infrastructure_with_source(
                    "无法创建片段配置目录",
                    source,
                ));
            }
        },
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "无法检查片段配置目录",
                source,
            ));
        }
    }

    let canonical_directory = fs::canonicalize(&directory)
        .map_err(|source| AppError::infrastructure_with_source("无法解析片段配置目录", source))?;
    let metadata = fs::metadata(&canonical_directory)
        .map_err(|source| AppError::infrastructure_with_source("无法检查片段配置目录", source))?;
    if canonical_directory != directory || !metadata.is_dir() {
        return Err(AppError::invalid(
            "片段配置目录必须是配置根下的真实 snippets 目录",
        ));
    }
    Ok(canonical_directory)
}

/// 使用系统默认文件管理器启动目录请求并回收短命令进程，不声称窗口已显示。
async fn spawn_directory_opener(directory: &Path) -> AppResult<()> {
    let invocation = opener_invocation(directory)?;
    let mut child = tokio::process::Command::new(&invocation.program)
        .args(&invocation.arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| {
            AppError::infrastructure_with_source("操作系统未能接受目录打开请求", source)
        })?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

/// 将路径作为单独 OS 参数传递，避免 shell 解释空格、引号或控制字符。
fn opener_invocation(directory: &Path) -> AppResult<OpenerInvocation> {
    let program = if cfg!(windows) {
        OsString::from("explorer.exe")
    } else if cfg!(target_os = "macos") {
        OsString::from("open")
    } else if cfg!(target_os = "linux") {
        OsString::from("xdg-open")
    } else {
        return Err(AppError::unsupported(
            "当前平台没有配置目录文件管理器适配器",
        ));
    };
    Ok(OpenerInvocation {
        program,
        arguments: vec![directory.as_os_str().to_owned()],
    })
}

struct OpenerInvocation {
    program: OsString,
    arguments: Vec<OsString>,
}

#[cfg(test)]
#[path = "../../tests/unit/configuration_directory.rs"]
mod tests;
