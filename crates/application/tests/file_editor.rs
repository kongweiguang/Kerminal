//! 文件编辑应用用例与现有文件端口的端到端契约测试。
//!
//! @author kongweiguang

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    files::editor::FileEditorService,
    files::{FileAccessProvider, FileBookmarkRepository, FileService},
};
use kerminal_domain::files::editor::{EditorPhase, EditorTransitionError, SaveResolution};
use kerminal_domain::files::{
    BookmarkTarget, DeleteTargetKind, DirectoryListing, FileBookmark, FileEndpoint, FilePreview,
    FileRevision, LineEnding, TextDocument, WriteTextRequest, WriteTextResult, WriteTextStrategy,
};
use tokio_util::sync::CancellationToken;

/// 仅支持本机端点的内存文件端口记录应用用例实际提交的写入策略。
struct MemoryFileProvider {
    endpoint: FileEndpoint,
    state: Mutex<MemoryFileState>,
}

/// 内存文件同时保存正文、revision 和最近一次写入策略供契约断言。
struct MemoryFileState {
    content: String,
    revision: FileRevision,
    version: u64,
    read_count: usize,
    write_count: usize,
    unknown_after_next_write: bool,
    last_strategy: Option<WriteTextStrategy>,
}

impl MemoryFileProvider {
    /// 创建不触碰磁盘的文件夹具，revision 可通过测试显式推进模拟并发修改。
    fn new(content: &str) -> Self {
        Self {
            endpoint: local_endpoint(),
            state: Mutex::new(MemoryFileState {
                content: content.to_owned(),
                revision: revision(content, 1),
                version: 1,
                read_count: 0,
                write_count: 0,
                unknown_after_next_write: false,
                last_strategy: None,
            }),
        }
    }

    /// 模拟另一个进程在编辑器保存前更新同一路径，并保留该变化作为真实冲突基线。
    fn external_write(&self, content: &str) {
        let mut state = self.state.lock().expect("memory file mutex is available");
        state.content = content.to_owned();
        state.version += 1;
        state.revision = revision(content, state.version);
    }

    /// 读取内存端口的最终状态，验证文件用例实际写入而不是只构造命令。
    fn snapshot(&self) -> (String, FileRevision, Option<WriteTextStrategy>) {
        let state = self.state.lock().expect("memory file mutex is available");
        (
            state.content.clone(),
            state.revision.clone(),
            state.last_strategy.clone(),
        )
    }

    /// 返回被真实 FileService 调用的读取次数，确认预取消请求没有进入适配器。
    fn read_count(&self) -> usize {
        self.state
            .lock()
            .expect("memory file mutex is available")
            .read_count
    }

    /// 下一次写入先真实提交再报告未知结果，模拟远端确认响应丢失。
    fn mark_next_write_unknown(&self) {
        self.state
            .lock()
            .expect("memory file mutex is available")
            .unknown_after_next_write = true;
    }

    /// 返回写调用数，确认未知结果没有触发应用层隐式重试。
    fn write_count(&self) -> usize {
        self.state
            .lock()
            .expect("memory file mutex is available")
            .write_count
    }
}

#[async_trait]
impl FileAccessProvider for MemoryFileProvider {
    /// 只接收本测试的稳定本机 endpoint，避免误处理其它协议类型。
    fn supports(&self, endpoint: &FileEndpoint) -> bool {
        endpoint == &self.endpoint
    }

    /// 目录操作不属于编辑契约，显式返回不支持以避免构造无关目录数据。
    async fn list_directory(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
    ) -> AppResult<DirectoryListing> {
        Err(AppError::unsupported("测试文件端口不支持目录列表"))
    }

    /// 预览操作不属于编辑契约，显式返回不支持以保持夹具职责单一。
    async fn preview_file(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: usize,
    ) -> AppResult<FilePreview> {
        Err(AppError::unsupported("测试文件端口不支持预览"))
    }

    /// 返回带当前 revision 的文本文档，供真实 FileEditorService 建立领域会话。
    async fn read_text_file(
        &self,
        _: CancellationToken,
        endpoint: FileEndpoint,
        _: usize,
    ) -> AppResult<TextDocument> {
        let mut state = self.state.lock().expect("memory file mutex is available");
        state.read_count += 1;
        Ok(TextDocument {
            endpoint,
            content: state.content.clone(),
            line_ending: detected_line_ending(&state.content),
            revision: state.revision.clone(),
            readonly: false,
            truncated: false,
        })
    }

    /// 执行 revision 比较或明确覆盖，并返回新 revision 供领域基线推进。
    async fn write_text_file(
        &self,
        _: CancellationToken,
        request: WriteTextRequest,
    ) -> AppResult<WriteTextResult> {
        let mut state = self.state.lock().expect("memory file mutex is available");
        match &request.strategy {
            WriteTextStrategy::CompareRevision(expected) if expected != &state.revision => {
                return Err(AppError::conflict("文本 revision 已变化"));
            }
            WriteTextStrategy::CreateNew => {
                return Err(AppError::unsupported("测试文件已存在"));
            }
            _ => {}
        }

        state.content = request.content;
        state.version += 1;
        state.revision = revision(&state.content, state.version);
        state.write_count += 1;
        state.last_strategy = Some(request.strategy);
        let result = WriteTextResult {
            bytes_written: state.content.len() as u64,
            revision: state.revision.clone(),
        };
        if std::mem::take(&mut state.unknown_after_next_write) {
            return Err(AppError::outcome_unknown(
                "保存结果暂时无法确认，请重新读取文件",
            ));
        }
        Ok(result)
    }

    /// 创建目录不属于文本编辑端口能力。
    async fn create_directory(&self, _: CancellationToken, _: FileEndpoint) -> AppResult<()> {
        Err(AppError::unsupported("测试文件端口不支持创建目录"))
    }

    /// 重命名不属于文本编辑端口能力。
    async fn rename(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: FileEndpoint,
    ) -> AppResult<()> {
        Err(AppError::unsupported("测试文件端口不支持重命名"))
    }

    /// 权限修改不属于文本编辑端口能力。
    async fn chmod(&self, _: CancellationToken, _: FileEndpoint, _: u32) -> AppResult<()> {
        Err(AppError::unsupported("测试文件端口不支持权限修改"))
    }

    /// 删除不属于文本编辑端口能力。
    async fn delete(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: DeleteTargetKind,
    ) -> AppResult<()> {
        Err(AppError::unsupported("测试文件端口不支持删除"))
    }
}

/// 书签端口在这些文件编辑测试中不使用，返回空集合只满足 FileService 装配契约。
struct EmptyBookmarkRepository;

#[async_trait]
impl FileBookmarkRepository for EmptyBookmarkRepository {
    /// 返回空书签，确认编辑流程不会隐式依赖路径收藏。
    async fn list(&self, _: CancellationToken, _: BookmarkTarget) -> AppResult<Vec<FileBookmark>> {
        Ok(Vec::new())
    }

    /// 不创建书签数据，编辑用例不应调用此端口。
    async fn set(
        &self,
        _: CancellationToken,
        _: FileBookmark,
        _: bool,
    ) -> AppResult<Vec<FileBookmark>> {
        Ok(Vec::new())
    }
}

/// 组装现有 FileService 与编辑用例，确保集成路径不绕过正式端口。
fn editor_service(provider: Arc<MemoryFileProvider>) -> FileEditorService {
    let provider_port: Arc<dyn FileAccessProvider> = provider;
    let files = FileService::new(vec![provider_port], Arc::new(EmptyBookmarkRepository))
        .expect("memory provider is registered");
    FileEditorService::new(files)
}

/// 返回固定的本机文件身份，测试端口不访问该路径。
fn local_endpoint() -> FileEndpoint {
    FileEndpoint::Local {
        path: PathBuf::from("editor-flow-contract.txt"),
    }
}

/// 为每个模拟写入生成可供 FileService 接受的 revision digest。
fn revision(content: &str, version: u64) -> FileRevision {
    FileRevision {
        size: content.len() as u64,
        modified: Some(format!("revision-{version}")),
        permissions: Some(0o644),
        content_digest: Some(format!("{version:064x}")),
    }
}

/// 从 fixture 正文推导行尾，令 application 契约测试模拟真实文件适配器。
fn detected_line_ending(content: &str) -> LineEnding {
    let crlf_count = content.matches("\r\n").count();
    let newline_count = content.matches('\n').count();
    let lf_count = newline_count.saturating_sub(crlf_count);
    match (crlf_count > 0, lf_count > 0) {
        (true, true) => LineEnding::Mixed,
        (true, false) => LineEnding::Crlf,
        _ => LineEnding::Lf,
    }
}

/// 编辑打开、revision 保存和外部并发修改经 FileService 完整走通并要求显式覆盖。
#[tokio::test]
async fn open_save_conflict_and_explicit_overwrite_use_file_service() {
    let provider = Arc::new(MemoryFileProvider::new("before"));
    let service = editor_service(Arc::clone(&provider));
    let endpoint = local_endpoint();
    let mut session = service
        .open(endpoint, CancellationToken::new())
        .await
        .expect("file should open");

    let save = session
        .begin_save("mine", SaveResolution::CompareRevision)
        .expect("dirty content should create a compare-revision write");
    let result = service
        .save(save, CancellationToken::new())
        .await
        .expect("first save should succeed");
    session
        .save_succeeded(result)
        .expect("successful adapter result should advance the session");
    assert_eq!(session.phase(), EditorPhase::Ready);
    assert!(!session.is_dirty("mine"));
    assert_eq!(provider.snapshot().0, "mine");
    assert_eq!(
        provider.snapshot().2,
        Some(WriteTextStrategy::CompareRevision(revision("before", 1)))
    );

    let mut conflicted = service
        .open(local_endpoint(), CancellationToken::new())
        .await
        .expect("updated file should reopen");
    provider.external_write("theirs");
    let save = conflicted
        .begin_save("mine again", SaveResolution::CompareRevision)
        .expect("local edits should begin a guarded save");
    let error = service
        .save(save, CancellationToken::new())
        .await
        .expect_err("stale revision must not overwrite the external change");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    conflicted
        .save_failed(true)
        .expect("conflict should be reflected in the aggregate");
    assert_eq!(conflicted.phase(), EditorPhase::Conflict);
    assert_eq!(provider.snapshot().0, "theirs");
    assert_eq!(
        conflicted.begin_save("mine again", SaveResolution::CompareRevision),
        Err(EditorTransitionError::ConflictResolutionRequired)
    );

    let overwrite = conflicted
        .begin_save("mine again", SaveResolution::OverwriteAfterConflict)
        .expect("explicit conflict resolution should produce overwrite intent");
    let result = service
        .save(overwrite, CancellationToken::new())
        .await
        .expect("explicit overwrite should succeed");
    conflicted
        .save_succeeded(result)
        .expect("overwrite result should advance the baseline");
    assert_eq!(provider.snapshot().0, "mine again");
    assert_eq!(
        provider.snapshot().2,
        Some(WriteTextStrategy::ExplicitOverwrite)
    );
}

/// 应用服务原样透传领域转换后的正文，确保 CRLF 与 UTF-8 BOM 落到实际写入端口。
#[tokio::test]
async fn file_editor_service_preserves_crlf_and_bom_when_saving() {
    let initial = "\u{feff}first\r\nsecond";
    let edited = "\u{feff}first\r\nsecond\nthird";
    let provider = Arc::new(MemoryFileProvider::new(initial));
    let service = editor_service(Arc::clone(&provider));
    let mut session = service
        .open(local_endpoint(), CancellationToken::new())
        .await
        .expect("UTF-8 with a BOM should open as a valid text document");
    let command = session
        .begin_save(edited, SaveResolution::CompareRevision)
        .expect("edited text should produce a revision-checked command");

    assert_eq!(command.content, "\u{feff}first\r\nsecond\r\nthird");
    let result = service
        .save(command, CancellationToken::new())
        .await
        .expect("the file service should receive the fully serialized text");
    session
        .save_succeeded(result)
        .expect("confirmed write should rebase the editor snapshot");

    assert_eq!(provider.snapshot().0, "\u{feff}first\r\nsecond\r\nthird");
    assert_eq!(
        detected_line_ending(&provider.snapshot().0),
        LineEnding::Crlf
    );
    assert!(!session.is_dirty(edited));
}

/// 已取消的读取不调用 adapter，供应用关闭时安全清理异步打开任务。
#[tokio::test]
async fn cancelled_open_returns_cancelled_error() {
    let provider = Arc::new(MemoryFileProvider::new("content"));
    let service = editor_service(Arc::clone(&provider));
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let result = service.open(local_endpoint(), cancellation).await;

    assert_eq!(
        result
            .expect_err("cancelled read should not open a document")
            .public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
    assert_eq!(provider.read_count(), 0);
}

/// 未知提交结果必须由显式读回解决，编辑服务不得自动重试造成二次写入。
#[tokio::test]
async fn unknown_save_result_requires_readback_and_is_not_retried() {
    let provider = Arc::new(MemoryFileProvider::new("before"));
    let service = editor_service(Arc::clone(&provider));
    provider.mark_next_write_unknown();
    let mut session = service
        .open(local_endpoint(), CancellationToken::new())
        .await
        .expect("file should open");
    let command = session
        .begin_save("committed but unconfirmed", SaveResolution::CompareRevision)
        .expect("dirty content should begin a guarded save");

    let error = service
        .save(command, CancellationToken::new())
        .await
        .expect_err("the simulated lost confirmation must remain unknown");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::OutcomeUnknown
    );
    assert_eq!(provider.write_count(), 1);
    assert_eq!(provider.snapshot().0, "committed but unconfirmed");

    let readback = service
        .open(local_endpoint(), CancellationToken::new())
        .await
        .expect("the user-requested readback should reveal the committed text");
    assert_eq!(readback.saved_content(), "committed but unconfirmed");
    assert_eq!(provider.write_count(), 1);
}
