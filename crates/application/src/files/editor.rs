//! 文件编辑用例复用通用 FileService，领域会话统一约束读取预算、revision 与行尾序列化。
//!
//! @author kongweiguang

use kerminal_domain::files::editor::{EditorSaveCommand, FileEditorSession};
use kerminal_domain::files::{FileEndpoint, WriteTextResult};
use tokio_util::sync::CancellationToken;

use super::{FileService, MAX_TEXT_FILE_READ_BYTES};
use crate::AppResult;

/// 文件编辑工作流只编排现有文件端口，不重复实现本地、SFTP 或容器读写。
#[derive(Clone)]
pub struct FileEditorService {
    files: FileService,
}

impl FileEditorService {
    /// 注入唯一文件用例服务，使桌面 UI 不需要依赖基础设施适配器。
    pub fn new(files: FileService) -> Self {
        Self { files }
    }

    /// 以冻结版 10 MiB 编辑上限读取文本，并把任务取消令牌传给唯一文件端口。
    pub async fn open(
        &self,
        endpoint: FileEndpoint,
        cancellation: CancellationToken,
    ) -> AppResult<FileEditorSession> {
        let document = self
            .files
            .read_text_file(cancellation, endpoint, MAX_TEXT_FILE_READ_BYTES)
            .await?;
        Ok(FileEditorSession::open(document))
    }

    /// 透传领域已编码的正文和取消令牌，不在应用层重复改写 BOM 或行尾。
    pub async fn save(
        &self,
        command: EditorSaveCommand,
        cancellation: CancellationToken,
    ) -> AppResult<WriteTextResult> {
        self.files
            .write_text_file(
                cancellation,
                kerminal_domain::files::WriteTextRequest {
                    endpoint: command.endpoint,
                    content: command.content,
                    strategy: command.strategy,
                },
            )
            .await
    }
}
