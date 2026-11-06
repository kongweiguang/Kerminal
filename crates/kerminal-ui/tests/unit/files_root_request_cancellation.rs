//! 根目录读取的 cancellation owner 必须真实释放旧 provider，并保护新 generation。
//!
//! @author kongweiguang

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor, files::FileAccessProvider,
};
use kerminal_domain::{
    files::{
        DeleteTargetKind, DirectoryListing, FileEndpoint, FilePreview, TextDocument,
        WriteTextRequest, WriteTextResult,
    },
    identity::HostId,
};
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};
use tokio_util::sync::CancellationToken;

use super::BrowserPanel;

struct SuspendedFileProvider {
    started: mpsc::UnboundedSender<usize>,
    finished: mpsc::UnboundedSender<usize>,
    next_request: AtomicUsize,
}

#[async_trait]
impl FileAccessProvider for SuspendedFileProvider {
    /// 测试 provider 只接受远端 endpoint，避免把其它协议误认为该 SFTP 读取。
    fn supports(&self, endpoint: &FileEndpoint) -> bool {
        matches!(endpoint, FileEndpoint::Remote { .. })
    }

    /// 每个读取都先报告启动，再等待 executor 传入的 token；取消回执证明 worker 已退出等待。
    async fn list_directory(
        &self,
        cancellation: CancellationToken,
        _endpoint: FileEndpoint,
    ) -> AppResult<DirectoryListing> {
        let request = self.next_request.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.started.send(request);
        cancellation.cancelled().await;
        let _ = self.finished.send(request);
        Err(AppError::cancelled("test root provider cancelled"))
    }

    /// 根读取测试不请求预览。
    async fn preview_file(
        &self,
        _cancellation: CancellationToken,
        _endpoint: FileEndpoint,
        _max_bytes: usize,
    ) -> AppResult<FilePreview> {
        Err(AppError::unsupported("test preview unavailable"))
    }

    /// 根读取测试不读取可编辑文本。
    async fn read_text_file(
        &self,
        _cancellation: CancellationToken,
        _endpoint: FileEndpoint,
        _max_bytes: usize,
    ) -> AppResult<TextDocument> {
        Err(AppError::unsupported("test text read unavailable"))
    }

    /// 根读取测试不写文本。
    async fn write_text_file(
        &self,
        _cancellation: CancellationToken,
        _request: WriteTextRequest,
    ) -> AppResult<WriteTextResult> {
        Err(AppError::unsupported("test text write unavailable"))
    }

    /// 根读取测试不创建目录。
    async fn create_directory(
        &self,
        _cancellation: CancellationToken,
        _endpoint: FileEndpoint,
    ) -> AppResult<()> {
        Err(AppError::unsupported("test create unavailable"))
    }

    /// 根读取测试不重命名文件。
    async fn rename(
        &self,
        _cancellation: CancellationToken,
        _source: FileEndpoint,
        _destination: FileEndpoint,
    ) -> AppResult<()> {
        Err(AppError::unsupported("test rename unavailable"))
    }

    /// 根读取测试不修改权限。
    async fn chmod(
        &self,
        _cancellation: CancellationToken,
        _endpoint: FileEndpoint,
        _mode: u32,
    ) -> AppResult<()> {
        Err(AppError::unsupported("test chmod unavailable"))
    }

    /// 根读取测试不删除文件。
    async fn delete(
        &self,
        _cancellation: CancellationToken,
        _endpoint: FileEndpoint,
        _kind: DeleteTargetKind,
    ) -> AppResult<()> {
        Err(AppError::unsupported("test delete unavailable"))
    }
}

/// 构造只会在收到取消后返回的真实 FileAccessProvider 与可观测启动/释放通道。
///
/// @author kongweiguang
fn suspended_provider() -> (
    Arc<SuspendedFileProvider>,
    mpsc::UnboundedReceiver<usize>,
    mpsc::UnboundedReceiver<usize>,
) {
    let (started, started_rx) = mpsc::unbounded_channel();
    let (finished, finished_rx) = mpsc::unbounded_channel();
    (
        Arc::new(SuspendedFileProvider {
            started,
            finished,
            next_request: AtomicUsize::new(0),
        }),
        started_rx,
        finished_rx,
    )
}

/// 通过 production ApplicationTaskExecutor 运行真实 provider method，保留取消到 worker 的完整路径。
///
/// @author kongweiguang
fn start_root_provider_read(
    executor: ApplicationTaskExecutor,
    provider: Arc<SuspendedFileProvider>,
    cancellation: CancellationToken,
    endpoint: FileEndpoint,
) -> tokio::task::JoinHandle<AppResult<DirectoryListing>> {
    tokio::spawn(async move {
        executor
            .run_cancellable(cancellation, move |operation_cancellation| async move {
                provider
                    .list_directory(operation_cancellation, endpoint)
                    .await
            })
            .await
    })
}

/// 有界等待挂起 provider 启动/释放，失败时让测试报告时序问题而不是无限停住。
///
/// @author kongweiguang
async fn receive_read_event(receiver: &mut mpsc::UnboundedReceiver<usize>) -> usize {
    timeout(Duration::from_secs(2), receiver.recv())
        .await
        .expect("root provider event should arrive")
        .expect("root provider event channel should remain open")
}

/// 有界 join executor 等待者，证明取消后的实际 operation 返回而非只置位 token。
///
/// @author kongweiguang
async fn wait_for_worker(
    worker: tokio::task::JoinHandle<AppResult<DirectoryListing>>,
) -> AppResult<DirectoryListing> {
    timeout(Duration::from_secs(2), worker)
        .await
        .expect("root provider worker should finish after cancellation")
        .expect("root provider worker should remain joinable")
}

/// 显式匹配取消结果，避免测试依赖 DirectoryListing 的调试格式。
///
/// @author kongweiguang
fn assert_cancelled(result: AppResult<DirectoryListing>) {
    let error = match result {
        Ok(_) => panic!("suspended root provider must return cancellation"),
        Err(error) => error,
    };
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
}

/// 旧根读取取消后必须释放 provider；它的回执不能清理新 generation 的 token。
///
/// @author kongweiguang
#[tokio::test]
async fn replacing_root_request_releases_old_provider_without_clearing_new_owner() {
    let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
    let (provider, mut started, mut finished) = suspended_provider();
    let endpoint = FileEndpoint::remote(HostId::new(), "/srv").expect("valid remote endpoint");
    let mut panel = BrowserPanel::new(endpoint.clone());

    let first = panel.begin_root_request_cancellation(1);
    let first_worker = start_root_provider_read(
        executor.clone(),
        provider.clone(),
        first.clone(),
        endpoint.clone(),
    );
    assert_eq!(receive_read_event(&mut started).await, 1);

    let second = panel.begin_root_request_cancellation(2);
    assert!(first.is_cancelled());
    let second_worker = start_root_provider_read(executor, provider, second.clone(), endpoint);
    assert_eq!(receive_read_event(&mut started).await, 2);
    assert_eq!(receive_read_event(&mut finished).await, 1);
    assert_cancelled(wait_for_worker(first_worker).await);

    panel.finish_root_request_cancellation(1);
    let current = panel
        .root_request_cancellation
        .as_ref()
        .expect("new root request owner remains installed");
    assert_eq!(current.generation, 2);
    assert!(!second.is_cancelled());

    panel.cancel_root_request_cancellation();
    assert!(second.is_cancelled());
    assert_eq!(receive_read_event(&mut finished).await, 2);
    assert_cancelled(wait_for_worker(second_worker).await);
}

/// BrowserPanel Drop 取消仍挂起的根 provider，并等待 executor 所有任务持有者回收。
///
/// @author kongweiguang
#[tokio::test]
async fn dropping_browser_panel_releases_root_provider() {
    let executor = ApplicationTaskExecutor::new(tokio::runtime::Handle::current());
    let (provider, mut started, mut finished) = suspended_provider();
    let endpoint = FileEndpoint::remote(HostId::new(), "/srv").expect("valid remote endpoint");
    let mut panel = BrowserPanel::new(endpoint.clone());
    let cancellation = panel.begin_root_request_cancellation(1);
    let worker = start_root_provider_read(executor, provider, cancellation.clone(), endpoint);
    assert_eq!(receive_read_event(&mut started).await, 1);

    drop(panel);

    assert!(cancellation.is_cancelled());
    assert_eq!(receive_read_event(&mut finished).await, 1);
    assert_cancelled(wait_for_worker(worker).await);
}
