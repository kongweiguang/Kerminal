//! MCP runtime 同步任务的取消收尾与重试顺序。
//!
//! @author kongweiguang

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    agents::{
        AgentHistoryProjection, AgentHistoryRequest, AgentLauncherCatalog, AgentService,
        AgentSessionView, AgentTargetContext, AgentTargetResolution, AgentTerminalSnapshotRequest,
        AgentTextPreview, AgentTextSubmissionReceipt, CreateAgentSessionFromHistoryRequest,
        CreateAgentSessionRequest, RenameAgentSessionTitleRequest, SendAgentCommandRequest,
    },
};
use kerminal_domain::{agents::AgentTargetBinding, identity::AgentSessionId};
use tokio::sync::{Barrier, oneshot};
use tokio_util::sync::CancellationToken;

use super::super::{ActiveShutdown, request_runtime_shutdown, with_running_publication_gate};
use super::run_endpoint_sync;

const TEST_ENDPOINT: &str = "http://127.0.0.1:37657";

/// 只用于 runtime 任务测试；除 endpoint 同步外的 Agent 用例必须显式失败。
struct FakeAgentService {
    calls: Mutex<Vec<Option<String>>>,
    fail_first: AtomicBool,
    entered: Mutex<Option<oneshot::Sender<()>>>,
    cleanup_completed: AtomicBool,
}

impl FakeAgentService {
    /// 创建可选首调用失败或等待取消的 fake，避免测试依赖文件仓储与真实 CLI。
    fn new(fail_first: bool, entered: Option<oneshot::Sender<()>>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            fail_first: AtomicBool::new(fail_first),
            entered: Mutex::new(entered),
            cleanup_completed: AtomicBool::new(false),
        }
    }

    /// 返回由 Agent endpoint 应用用例实际收到的有序目标序列。
    fn calls(&self) -> Vec<Option<String>> {
        self.calls.lock().expect("test mutex is healthy").clone()
    }
}

#[async_trait]
impl AgentService for FakeAgentService {
    /// 首次失败用来验证同一 endpoint 能通过后续调用显式重试；等待模式验证取消后会收尾。
    async fn configure_mcp_endpoint(
        &self,
        endpoint: Option<String>,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        self.calls
            .lock()
            .map_err(|_| AppError::infrastructure("test mutex poisoned"))?
            .push(endpoint);
        let entered = self
            .entered
            .lock()
            .map_err(|_| AppError::infrastructure("test mutex poisoned"))?
            .take();
        if let Some(entered) = entered {
            let _ = entered.send(());
            cancellation.cancelled().await;
            self.cleanup_completed.store(true, Ordering::Release);
            return Err(AppError::cancelled(
                "fake endpoint sync cancelled after cleanup",
            ));
        }
        if self.fail_first.swap(false, Ordering::AcqRel) {
            return Err(AppError::conflict("fake endpoint CAS conflict"));
        }
        Ok(())
    }

    /// Runtime 同步测试不读取 Launcher 目录，未调用时明确拒绝。
    async fn launcher_catalog(
        &self,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentLauncherCatalog> {
        unused_agent_operation()
    }

    /// Runtime 同步测试不改 Launcher 选择，未调用时明确拒绝。
    async fn select_launcher(
        &self,
        _key: kerminal_domain::agents::AgentLauncherKey,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentLauncherCatalog> {
        unused_agent_operation()
    }

    /// 未被此测试调用的 Agent 创建能力明确返回 unsupported。
    async fn create(
        &self,
        _request: CreateAgentSessionRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_agent_operation()
    }

    /// 未被此测试调用的历史列表能力明确返回 unsupported。
    async fn list(&self, _cancellation: CancellationToken) -> AppResult<Vec<AgentSessionView>> {
        unused_agent_operation()
    }

    /// Runtime 控制测试不访问历史投影。
    async fn history(
        &self,
        _request: AgentHistoryRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentHistoryProjection> {
        unused_agent_operation()
    }

    /// Runtime 控制测试不从旧启动快照创建 Agent。
    async fn create_from_history(
        &self,
        _request: CreateAgentSessionFromHistoryRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_agent_operation()
    }

    /// Runtime 控制测试不改名 Agent 历史。
    async fn rename_title(
        &self,
        _request: RenameAgentSessionTitleRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_agent_operation()
    }

    /// 未被此测试调用的单项查询能力明确返回 unsupported。
    async fn get(
        &self,
        _agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_agent_operation()
    }

    /// 未被此测试调用的恢复能力明确返回 unsupported。
    async fn resume(
        &self,
        _agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_agent_operation()
    }

    /// 未被此测试调用的归档能力明确返回 unsupported。
    async fn archive(
        &self,
        _agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_agent_operation()
    }

    /// 未被此测试调用的目标绑定能力明确返回 unsupported。
    async fn bind_preferred_target(
        &self,
        _agent_session_id: AgentSessionId,
        _binding: AgentTargetBinding,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_agent_operation()
    }

    /// 未被此测试调用的目标解析能力明确返回 unsupported。
    async fn resolve_preferred_target(
        &self,
        _agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentTargetResolution> {
        unused_agent_operation()
    }

    /// 未被此测试调用的上下文读取能力明确返回 unsupported。
    async fn target_context(
        &self,
        _agent_session_id: AgentSessionId,
        _max_snapshot_bytes: usize,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentTargetContext> {
        unused_agent_operation()
    }

    /// 此测试只覆盖 MCP endpoint 生命周期，临时 Agent preview 不在该 fake 的业务范围内。
    ///
    /// @author kongweiguang
    async fn preview_text(
        &self,
        _agent_session_id: AgentSessionId,
        _text: String,
        _max_bytes: usize,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentTextPreview> {
        unused_agent_operation()
    }

    /// 此测试不向 Agent CLI 提交文本，fake 明确拒绝避免伪造 writer ACK。
    ///
    /// @author kongweiguang
    async fn send_command_block_text(
        &self,
        _request: SendAgentCommandRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentTextSubmissionReceipt> {
        unused_agent_operation()
    }

    /// 未被此测试调用的快照写入能力明确返回 unsupported。
    async fn record_terminal_snapshot(
        &self,
        _request: AgentTerminalSnapshotRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<()> {
        unused_agent_operation()
    }
}

/// 将测试范围外的 Agent 操作统一拒绝，避免 fake 意外制造成功状态。
fn unused_agent_operation<T>() -> AppResult<T> {
    Err(AppError::unsupported("unused test Agent operation"))
}

/// 请求取消后，runtime 同步 wrapper 必须等待 Agent workspace 用例完成清理。
#[tokio::test]
async fn endpoint_sync_cancellation_waits_for_agent_cleanup() {
    let (entered_sender, entered_receiver) = oneshot::channel();
    let agent = Arc::new(FakeAgentService::new(false, Some(entered_sender)));
    let request_cancellation = CancellationToken::new();
    let task = tokio::spawn(run_endpoint_sync(
        agent.clone(),
        Some(TEST_ENDPOINT.to_owned()),
        CancellationToken::new(),
        Some(request_cancellation.clone()),
        CancellationToken::new(),
        false,
    ));

    entered_receiver
        .await
        .expect("endpoint update entered the Agent use case");
    request_cancellation.cancel();
    assert!(task.await.expect("sync task joined").is_err());
    assert!(agent.cleanup_completed.load(Ordering::Acquire));
}

/// CAS 失败保留相同 endpoint 供显式重试，最终停止同步按顺序写入 None。
#[tokio::test]
async fn endpoint_sync_failure_can_retry_then_clear_in_order() {
    let agent = Arc::new(FakeAgentService::new(true, None));
    let agent_service: Arc<dyn AgentService> = agent.clone();

    let first = run_endpoint_sync(
        Arc::clone(&agent_service),
        Some(TEST_ENDPOINT.to_owned()),
        CancellationToken::new(),
        None,
        CancellationToken::new(),
        false,
    )
    .await;
    assert!(first.is_err());

    run_endpoint_sync(
        Arc::clone(&agent_service),
        Some(TEST_ENDPOINT.to_owned()),
        CancellationToken::new(),
        None,
        CancellationToken::new(),
        false,
    )
    .await
    .expect("same endpoint can be synchronized after explicit retry");
    run_endpoint_sync(
        agent_service,
        None,
        CancellationToken::new(),
        None,
        CancellationToken::new(),
        false,
    )
    .await
    .expect("stopping listener clears the active endpoint");

    assert_eq!(
        agent.calls(),
        vec![
            Some(TEST_ENDPOINT.to_owned()),
            Some(TEST_ENDPOINT.to_owned()),
            None,
        ]
    );
}

/// endpoint 同步成功后、发布 Running 前若 shutdown 到达，状态必须保留在 Stopping。
#[tokio::test]
async fn shutdown_between_endpoint_ack_and_running_publish_wins() {
    let identity = Arc::new(());
    let cancellation = CancellationToken::new();
    let active_shutdown = Arc::new(Mutex::new(Some(ActiveShutdown {
        identity: identity.clone(),
        cancellation: cancellation.clone(),
    })));
    let shutting_down = Arc::new(AtomicBool::new(false));
    let published_state = Arc::new(Mutex::new("synchronizing"));
    let endpoint_sync_completed = Arc::new(Barrier::new(2));
    let allow_running_publish = Arc::new(Barrier::new(2));

    let task = {
        let active_shutdown = active_shutdown.clone();
        let shutting_down = shutting_down.clone();
        let published_state = published_state.clone();
        let endpoint_sync_completed = endpoint_sync_completed.clone();
        let allow_running_publish = allow_running_publish.clone();
        let identity = identity.clone();
        tokio::spawn(async move {
            // 模拟真实 endpoint 用例已 ACK，但 start 尚未发布最终快照。
            endpoint_sync_completed.wait().await;
            allow_running_publish.wait().await;
            let running_state = published_state.clone();
            let stopping_state = published_state.clone();
            with_running_publication_gate(
                &active_shutdown,
                &shutting_down,
                &identity,
                move || {
                    *running_state.lock().expect("test mutex is healthy") = "running";
                },
                move || {
                    *stopping_state.lock().expect("test mutex is healthy") = "stopping";
                },
            )
        })
    };

    endpoint_sync_completed.wait().await;
    request_runtime_shutdown(&active_shutdown, &shutting_down);
    allow_running_publish.wait().await;

    assert!(!task.await.expect("publication task joins"));
    assert!(cancellation.is_cancelled());
    assert_eq!(
        *published_state.lock().expect("test mutex is healthy"),
        "stopping"
    );
}

/// 旧 listener 的 shutdown token 不能让下一代 listener 发布自己的 Running 状态。
#[test]
fn stale_listener_identity_cannot_publish_running() {
    let active_identity = Arc::new(());
    let stale_identity = Arc::new(());
    let active_shutdown = Mutex::new(Some(ActiveShutdown {
        identity: active_identity,
        cancellation: CancellationToken::new(),
    }));
    let shutting_down = AtomicBool::new(false);
    let published_state = Mutex::new("synchronizing");

    let published = with_running_publication_gate(
        &active_shutdown,
        &shutting_down,
        &stale_identity,
        || *published_state.lock().expect("test mutex is healthy") = "running",
        || *published_state.lock().expect("test mutex is healthy") = "stopping",
    );

    assert!(!published);
    assert_eq!(
        *published_state.lock().expect("test mutex is healthy"),
        "stopping"
    );
}
