//! 用户显式打开固定配置目录的用例与端口。
//!
//! @author kongweiguang

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::{AppError, AppResult};

/// 目录身份由应用枚举限定，调用者不能向平台适配器传入任意路径。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigurationDirectoryKind {
    /// 应用用于读取与修复配置的固定根目录。
    Root,
    /// 用户维护的命令片段目录。
    Snippets,
}

/// 操作系统接受了打开目录的启动请求；该结果不证明文件管理器已实际显示目录。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenConfigurationDirectoryResult {
    RequestAccepted { kind: ConfigurationDirectoryKind },
}

/// 平台端只负责解析固定目录并请求操作系统打开，不接收用户构造的路径或命令。
#[async_trait]
pub trait ConfigurationDirectoryPort: Send + Sync + 'static {
    /// 在进程启动前响应取消；一旦系统接收请求，调用方只能报告请求已提交。
    async fn request_open(
        &self,
        kind: ConfigurationDirectoryKind,
        cancellation: CancellationToken,
    ) -> AppResult<()>;
}

/// 配置目录打开用例；UI 与外部入口共享固定目录白名单和结果语义。
#[derive(Clone)]
pub struct ConfigurationDirectoryService {
    port: Arc<dyn ConfigurationDirectoryPort>,
}

impl ConfigurationDirectoryService {
    /// 注入唯一平台端口，避免面板直接拼接本机路径或启动外部进程。
    pub fn new(port: Arc<dyn ConfigurationDirectoryPort>) -> Self {
        Self { port }
    }

    /// 请求打开一个固定配置目录；回执仅表示操作系统启动请求已被接受。
    pub async fn open(
        &self,
        kind: ConfigurationDirectoryKind,
        cancellation: CancellationToken,
    ) -> AppResult<OpenConfigurationDirectoryResult> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("配置目录打开请求已取消"));
        }

        self.port.request_open(kind, cancellation).await?;
        Ok(OpenConfigurationDirectoryResult::RequestAccepted { kind })
    }
}
