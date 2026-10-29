//! 远端文件复制到系统剪贴板的 application 闭环测试。
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use kerminal_domain::{
    files::{
        BookmarkTarget, DeleteTargetKind, DirectoryListing, FileBookmark, FileEndpoint, FileEntry,
        FileEntryKind, FilePreview, RemotePath, TextDocument, TransferFailureKind, TransferKind,
        TransferRequest, TransferStatus, WriteTextRequest, WriteTextResult,
    },
    identity::HostId,
};
use tokio::{runtime::Handle, sync::Notify};
use tokio_util::sync::CancellationToken;

use crate::{
    AppError, AppResult,
    files::{FileAccessProvider, FileBookmarkRepository, FileService},
    transfers::{
        TransferBackend, TransferCleanupState, TransferExecutionError, TransferExecutionFailure,
        TransferExecutionResult, TransferQueueOptions, TransferService,
    },
};

use super::{
    FileClipboardDownloadService, FileClipboardPort, RemoteClipboardSource,
    entry_matches_transfer_kind, is_terminal,
};

#[derive(Clone, Copy)]
enum BackendMode {
    Succeed,
    Fail,
    CommitUnknown,
    WaitForCancellation,
}

struct TestTransferBackend {
    mode: BackendMode,
    started: Notify,
    cancelled: AtomicBool,
    files: Arc<Mutex<HashMap<PathBuf, Option<Vec<u8>>>>>,
}

struct TestRemoteFiles {
    directory: FileEndpoint,
    entry: FileEntry,
}

struct TestClipboard {
    directory: PathBuf,
    files: Arc<Mutex<HashMap<PathBuf, Option<Vec<u8>>>>>,
    published: Mutex<Option<Vec<PathBuf>>>,
    reserved: Mutex<Vec<PathBuf>>,
    removed: Mutex<Vec<PathBuf>>,
}

impl TestTransferBackend {
    /// 共享虚拟文件状态，让队列覆盖不依赖本机文件系统或 SSH。
    fn new(mode: BackendMode, files: Arc<Mutex<HashMap<PathBuf, Option<Vec<u8>>>>>) -> Self {
        Self {
            mode,
            started: Notify::new(),
            cancelled: AtomicBool::new(false),
            files,
        }
    }
}

#[async_trait]
impl TransferBackend for TestTransferBackend {
    /// 只接受 application 复制服务生成的 SFTP 到本机传输形状。
    fn supports(&self, request: &TransferRequest) -> bool {
        matches!(request.source, FileEndpoint::Remote { .. })
            && matches!(request.destination, FileEndpoint::Local { .. })
    }

    /// 保留真实队列状态迁移，同时通过虚拟目标覆盖成功、失败、未知提交和取消路径。
    async fn execute(
        &self,
        context: crate::transfers::TransferExecutionContext,
    ) -> Result<TransferExecutionResult, TransferExecutionError> {
        let destination = context.request.destination.clone();
        context
            .progress
            .begin_transfer(Some(4))
            .await
            .map_err(|_| TransferExecutionError::Cancelled {
                checkpoint: None,
                actual_destination: Some(destination.clone()),
            })?;
        self.started.notify_one();

        match self.mode {
            BackendMode::WaitForCancellation => {
                context.cancellation.cancelled().await;
                self.cancelled.store(true, Ordering::Release);
                Err(TransferExecutionError::Cancelled {
                    checkpoint: None,
                    actual_destination: Some(destination),
                })
            }
            BackendMode::Fail => Err(TransferExecutionError::Failed(
                TransferExecutionFailure::new(
                    TransferFailureKind::Connection,
                    false,
                    false,
                    None,
                    Some(destination),
                    TransferCleanupState::Released,
                ),
            )),
            BackendMode::CommitUnknown => {
                self.write_destination(&destination);
                context
                    .progress
                    .begin_commit(destination.clone(), 0, Some(4), None)
                    .await
                    .expect("commit uncertainty follows an accepted commit request");
                Err(TransferExecutionError::Failed(
                    TransferExecutionFailure::new(
                        TransferFailureKind::CommitUnknown,
                        false,
                        false,
                        None,
                        Some(destination),
                        TransferCleanupState::Released,
                    ),
                ))
            }
            BackendMode::Succeed => {
                self.write_destination(&destination);
                context
                    .progress
                    .begin_commit(destination.clone(), 4, Some(4), None)
                    .await
                    .expect("success follows an accepted commit request");
                context
                    .progress
                    .record_committed_entry(4)
                    .await
                    .expect("the fake backend confirms its local commit");
                context
                    .progress
                    .finish_commit()
                    .await
                    .expect("the fake backend closes its commit barrier");
                Ok(TransferExecutionResult {
                    actual_destination: destination,
                    total_bytes: Some(4),
                    skipped: false,
                    unchanged: false,
                })
            }
        }
    }
}

impl TestTransferBackend {
    /// 将下载字节记录到预留槽位，模拟后端已经提交本机目标。
    fn write_destination(&self, destination: &FileEndpoint) {
        let FileEndpoint::Local { path } = destination else {
            panic!("clipboard download destination must be local");
        };
        let mut files = self
            .files
            .lock()
            .expect("test virtual file mutex is available");
        *files
            .get_mut(path)
            .expect("transfer target was reserved before enqueue") = Some(b"data".to_vec());
    }
}

#[async_trait]
impl FileAccessProvider for TestRemoteFiles {
    /// 测试 provider 只处理 SFTP endpoint，避免其他文件系统请求意外被接收。
    fn supports(&self, endpoint: &FileEndpoint) -> bool {
        matches!(endpoint, FileEndpoint::Remote { .. })
    }

    /// 返回当前 fixture 的父目录列表供复制服务核对文件身份和类型。
    async fn list_directory(
        &self,
        _: CancellationToken,
        endpoint: FileEndpoint,
    ) -> AppResult<DirectoryListing> {
        if endpoint != self.directory {
            return Err(AppError::not_found("测试远端目录不存在"));
        }
        Ok(DirectoryListing {
            directory: endpoint,
            entries: vec![self.entry.clone()],
        })
    }

    /// 测试流程不预览文件，显式拒绝无关文件能力。
    async fn preview_file(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: usize,
    ) -> AppResult<FilePreview> {
        Err(AppError::unsupported("测试 provider 不支持预览"))
    }

    /// 测试流程不编辑远端文本，显式拒绝无关文件能力。
    async fn read_text_file(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: usize,
    ) -> AppResult<TextDocument> {
        Err(AppError::unsupported("测试 provider 不支持文本读取"))
    }

    /// 测试流程不修改文件，显式拒绝无关文件能力。
    async fn write_text_file(
        &self,
        _: CancellationToken,
        _: WriteTextRequest,
    ) -> AppResult<WriteTextResult> {
        Err(AppError::unsupported("测试 provider 不支持文本写入"))
    }

    /// 测试流程不创建远端目录，显式拒绝无关文件能力。
    async fn create_directory(&self, _: CancellationToken, _: FileEndpoint) -> AppResult<()> {
        Err(AppError::unsupported("测试 provider 不支持目录创建"))
    }

    /// 测试流程不重命名远端项目，显式拒绝无关文件能力。
    async fn rename(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: FileEndpoint,
    ) -> AppResult<()> {
        Err(AppError::unsupported("测试 provider 不支持重命名"))
    }

    /// 测试流程不修改远端权限，显式拒绝无关文件能力。
    async fn chmod(&self, _: CancellationToken, _: FileEndpoint, _: u32) -> AppResult<()> {
        Err(AppError::unsupported("测试 provider 不支持 chmod"))
    }

    /// 测试流程不删除远端项目，显式拒绝无关文件能力。
    async fn delete(
        &self,
        _: CancellationToken,
        _: FileEndpoint,
        _: DeleteTargetKind,
    ) -> AppResult<()> {
        Err(AppError::unsupported("测试 provider 不支持删除"))
    }
}

#[async_trait]
impl FileBookmarkRepository for TestRemoteFiles {
    /// 测试不使用书签，返回空集合保持 repository 契约完整。
    async fn list(&self, _: CancellationToken, _: BookmarkTarget) -> AppResult<Vec<FileBookmark>> {
        Ok(Vec::new())
    }

    /// 测试不写书签，显式拒绝与剪贴板闭环无关的修改。
    async fn set(
        &self,
        _: CancellationToken,
        _: FileBookmark,
        _: bool,
    ) -> AppResult<Vec<FileBookmark>> {
        Err(AppError::unsupported("测试 repository 不支持书签写入"))
    }
}

#[async_trait]
impl FileClipboardPort for TestClipboard {
    /// 预留绝对虚拟目标槽位，让 application 测试不触碰本机文件系统。
    async fn reserve_download_target(
        &self,
        name: String,
        _kind: TransferKind,
    ) -> AppResult<PathBuf> {
        let path = self.directory.join(name);
        let mut files = self
            .files
            .lock()
            .expect("test virtual file mutex is available");
        if files.contains_key(&path) {
            return Err(AppError::conflict("测试虚拟下载路径已被占用"));
        }
        files.insert(path.clone(), None);
        drop(files);
        self.reserved
            .lock()
            .expect("test reservation mutex is available")
            .push(path.clone());
        Ok(path)
    }

    /// 记录已发布路径，验证服务只在所有队列操作成功后触碰 clipboard。
    async fn publish_files(&self, paths: Vec<PathBuf>) -> AppResult<()> {
        let files = self
            .files
            .lock()
            .expect("test virtual file mutex is available");
        if paths
            .iter()
            .any(|path| !matches!(files.get(path), Some(Some(_))))
        {
            return Err(AppError::conflict("测试虚拟下载目标尚未完成"));
        }
        drop(files);
        *self
            .published
            .lock()
            .expect("test publication mutex is available") = Some(paths);
        Ok(())
    }

    /// 清除虚拟目标并记录调用，覆盖失败清理闭环而不访问本机文件系统。
    async fn remove_download_target(&self, path: PathBuf, _kind: TransferKind) -> AppResult<()> {
        self.files
            .lock()
            .expect("test virtual file mutex is available")
            .remove(&path);
        self.removed
            .lock()
            .expect("test removal mutex is available")
            .push(path);
        Ok(())
    }

    /// Fake clipboard capability is available on every test host without touching its clipboard.
    fn ensure_supported(&self) -> AppResult<()> {
        Ok(())
    }
}

/// 创建单个远端文件与其父目录，便于测试真实 FileService 类型核验路径。
fn remote_file_fixture() -> (RemoteClipboardSource, TestRemoteFiles) {
    let host_id = HostId::parse("clipboard-test-host").expect("valid fixture host id");
    let directory_path = RemotePath::parse("/workspace").expect("valid fixture directory");
    let file_path = RemotePath::parse("/workspace/source.txt").expect("valid fixture file");
    let directory = FileEndpoint::Remote {
        host_id: host_id.clone(),
        path: directory_path,
    };
    let endpoint = FileEndpoint::Remote {
        host_id,
        path: file_path,
    };
    (
        RemoteClipboardSource {
            endpoint: endpoint.clone(),
            kind: TransferKind::File,
        },
        TestRemoteFiles {
            directory,
            entry: FileEntry {
                endpoint,
                name: "source.txt".to_owned(),
                kind: FileEntryKind::File,
                size: Some(4),
                modified: None,
                permissions: None,
                is_hidden: false,
                raw: None,
            },
        },
    )
}

/// 组装真实 application services 与内存 adapters，保留队列覆盖但不依赖目标路径存在。
fn service_fixture(
    mode: BackendMode,
) -> (
    FileClipboardDownloadService,
    Arc<TestTransferBackend>,
    Arc<TestClipboard>,
    RemoteClipboardSource,
) {
    let (source, provider) = remote_file_fixture();
    let provider = Arc::new(provider);
    let file_service = FileService::new(
        vec![provider.clone() as Arc<dyn FileAccessProvider>],
        provider as Arc<dyn FileBookmarkRepository>,
    )
    .expect("file service has one provider");
    let virtual_files = Arc::new(Mutex::new(HashMap::new()));
    let backend = Arc::new(TestTransferBackend::new(mode, virtual_files.clone()));
    let transfers = TransferService::new(
        vec![backend.clone()],
        Handle::current(),
        TransferQueueOptions::default(),
    )
    .expect("valid transfer queue options");
    let clipboard = Arc::new(TestClipboard {
        directory: std::env::temp_dir().join("kerminal-clipboard-unit-test"),
        files: virtual_files,
        published: Mutex::new(None),
        reserved: Mutex::new(Vec::new()),
        removed: Mutex::new(Vec::new()),
    });
    let service = FileClipboardDownloadService::new(file_service, transfers, clipboard.clone());
    (service, backend, clipboard, source)
}

/// 确认成功只有在虚拟目标写入且传输回执进入终态后才发布剪贴板路径。
#[tokio::test]
async fn successful_remote_copy_publishes_completed_download() {
    let (service, _, clipboard, source) = service_fixture(BackendMode::Succeed);

    service
        .copy_remote_files_to_clipboard(CancellationToken::new(), vec![source])
        .await
        .expect("remote copy succeeds");

    let published = clipboard
        .published
        .lock()
        .expect("test publication mutex is available")
        .clone()
        .expect("clipboard is updated after success");
    assert_eq!(published.len(), 1);
    let completed = clipboard
        .files
        .lock()
        .expect("test virtual file mutex is available")
        .get(&published[0])
        .cloned();
    assert_eq!(completed, Some(Some(b"data".to_vec())));
    assert!(
        clipboard
            .removed
            .lock()
            .expect("removal mutex is available")
            .is_empty()
    );
}

/// 已知传输失败会清理虚拟预留目标，且绝不发布不完整目标。
#[tokio::test]
async fn known_transfer_failure_cleans_reserved_target_without_publishing() {
    let (service, _, clipboard, source) = service_fixture(BackendMode::Fail);

    assert!(
        service
            .copy_remote_files_to_clipboard(CancellationToken::new(), vec![source])
            .await
            .is_err()
    );

    let reserved = clipboard
        .reserved
        .lock()
        .expect("reservation mutex is available")[0]
        .clone();
    assert!(
        !clipboard
            .files
            .lock()
            .expect("test virtual file mutex is available")
            .contains_key(&reserved)
    );
    assert!(
        clipboard
            .published
            .lock()
            .expect("publication mutex is available")
            .is_none()
    );
    assert_eq!(
        clipboard
            .removed
            .lock()
            .expect("removal mutex is available")
            .len(),
        1
    );
}

/// 用户取消后先等队列 worker 结束，再清理目标并返回取消态。
#[tokio::test]
async fn cancellation_waits_for_worker_before_cleaning_reserved_target() {
    let (service, backend, clipboard, source) = service_fixture(BackendMode::WaitForCancellation);
    let cancellation = CancellationToken::new();
    let task = tokio::spawn({
        let service = service.clone();
        let cancellation = cancellation.clone();
        async move {
            service
                .copy_remote_files_to_clipboard(cancellation, vec![source])
                .await
        }
    });

    backend.started.notified().await;
    cancellation.cancel();
    assert!(task.await.expect("clipboard task joins").is_err());

    let reserved = clipboard
        .reserved
        .lock()
        .expect("reservation mutex is available")[0]
        .clone();
    assert!(backend.cancelled.load(Ordering::Acquire));
    assert!(
        !clipboard
            .files
            .lock()
            .expect("test virtual file mutex is available")
            .contains_key(&reserved)
    );
    assert!(
        clipboard
            .published
            .lock()
            .expect("publication mutex is available")
            .is_none()
    );
}

/// 提交结果未知时保留原目标供 TransferService 恢复核对，不伪报成功或删除证据。
#[tokio::test]
async fn commit_unknown_keeps_download_target_and_does_not_publish() {
    let (service, _, clipboard, source) = service_fixture(BackendMode::CommitUnknown);

    assert!(
        service
            .copy_remote_files_to_clipboard(CancellationToken::new(), vec![source])
            .await
            .is_err()
    );

    let reserved = clipboard
        .reserved
        .lock()
        .expect("reservation mutex is available")[0]
        .clone();
    let retained = clipboard
        .files
        .lock()
        .expect("test virtual file mutex is available")
        .get(&reserved)
        .cloned();
    assert_eq!(retained, Some(Some(b"data".to_vec())));
    assert!(
        clipboard
            .removed
            .lock()
            .expect("removal mutex is available")
            .is_empty()
    );
    assert!(
        clipboard
            .published
            .lock()
            .expect("publication mutex is available")
            .is_none()
    );
}

/// 文件剪贴板只接受普通文件或目录，不允许链接和特殊节点进入下载队列。
#[test]
fn source_kind_matches_only_regular_file_or_directory() {
    assert!(entry_matches_transfer_kind(
        FileEntryKind::File,
        TransferKind::File
    ));
    assert!(entry_matches_transfer_kind(
        FileEntryKind::Directory,
        TransferKind::Directory
    ));
    assert!(!entry_matches_transfer_kind(
        FileEntryKind::Symlink,
        TransferKind::File
    ));
    assert!(!entry_matches_transfer_kind(
        FileEntryKind::Other,
        TransferKind::Directory
    ));
}

/// 只有队列的最终态可释放下载目标，排队、传输和提交状态都必须继续等待。
#[test]
fn only_terminal_transfer_states_allow_cleanup() {
    assert!(is_terminal(TransferStatus::Succeeded));
    assert!(is_terminal(TransferStatus::Failed));
    assert!(is_terminal(TransferStatus::Cancelled));
    assert!(!is_terminal(TransferStatus::Queued));
    assert!(!is_terminal(TransferStatus::Transferring));
    assert!(!is_terminal(TransferStatus::Committing));
}
