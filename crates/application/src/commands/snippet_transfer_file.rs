//! 用户片段 JSON 文件的受限读取与原子导出端口。
//!
//! @author kongweiguang

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::{AppError, AppResult, MAX_SNIPPET_TRANSFER_BYTES};

/// 导出持久化方式供 UI 如实说明是否经过提交后的读回确认。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnippetTransferFileConfirmation {
    AtomicCommit,
    ReadBackConfirmed,
}

/// 仅返回安全摘要，不把片段正文复制到文件操作回执或日志。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnippetTransferFileWriteReceipt {
    pub bytes_written: usize,
    pub confirmation: SnippetTransferFileConfirmation,
}

/// 文件端口只承载用户经系统选择器明确选中的导入/导出路径。
#[async_trait]
pub trait SnippetTransferFilePort: Send + Sync + 'static {
    /// 有界读取 UTF-8 文本；结构校验仍由现有片段导入用例负责。
    async fn read_import_file(
        &self,
        path: PathBuf,
        max_bytes: usize,
        cancellation: CancellationToken,
    ) -> AppResult<String>;

    /// 原子写入导出文档；未知提交须读回确认，不能在适配器中重放写入。
    async fn write_export_file(
        &self,
        path: PathBuf,
        source: String,
        max_bytes: usize,
        cancellation: CancellationToken,
    ) -> AppResult<SnippetTransferFileWriteReceipt>;
}

/// 将系统文件选择器返回的路径交给应用用例，UI 不直接读取或写入文件。
#[derive(Clone)]
pub struct SnippetTransferFileService {
    port: Arc<dyn SnippetTransferFilePort>,
}

impl SnippetTransferFileService {
    /// 注入桌面组合根创建的文件端口，确保 UI 与应用共享同一大小和回执契约。
    pub fn new(port: Arc<dyn SnippetTransferFilePort>) -> Self {
        Self { port }
    }

    /// 读取一个经系统对话框选中的文件，并在应用边界再次执行 32 MiB 上限。
    pub async fn read_import_file(
        &self,
        path: PathBuf,
        cancellation: CancellationToken,
    ) -> AppResult<String> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("片段导入文件读取已取消"));
        }
        if path.as_os_str().is_empty() {
            return Err(AppError::invalid("片段导入文件路径无效"));
        }

        let source = self
            .port
            .read_import_file(path, MAX_SNIPPET_TRANSFER_BYTES, cancellation)
            .await?;
        if source.len() > MAX_SNIPPET_TRANSFER_BYTES {
            return Err(AppError::invalid("片段导入文件超过 32 MiB 限制"));
        }
        Ok(source)
    }

    /// 原子导出有界 JSON；提交结果不明时只返回未知状态，不自动覆盖或重试。
    pub async fn write_export_file(
        &self,
        path: PathBuf,
        source: String,
        cancellation: CancellationToken,
    ) -> AppResult<SnippetTransferFileWriteReceipt> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("片段导出文件写入已取消"));
        }
        if path.as_os_str().is_empty() {
            return Err(AppError::invalid("片段导出文件路径无效"));
        }
        if source.len() > MAX_SNIPPET_TRANSFER_BYTES {
            return Err(AppError::invalid("片段导出文件超过 32 MiB 限制"));
        }

        self.port
            .write_export_file(path, source, MAX_SNIPPET_TRANSFER_BYTES, cancellation)
            .await
    }
}
