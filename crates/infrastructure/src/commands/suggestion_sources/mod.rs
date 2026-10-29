//! 面向已绑定 SSH session 的路径、可执行命令与 Git 建议适配器。
//!
//! 本模块仅执行固定只读探测，通过 FileService 与已认证 SSH lease 访问远端；
//! 用户输入只用于本地前缀筛选，不会拼入远端命令。
//!
//! @author kongweiguang

mod cache;
mod git;
mod projection;
mod remote_command;
mod remote_path;
mod syntax;

use projection::{
    CandidateContext, candidate, command_cache_key_for_git, parse_remote_path, prefix_score, quote,
    requested,
};

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use kerminal_application::{
    AppError, AppResult,
    commands::CommandSuggestionProvider,
    connections::{
        CommandEvent, CommandStreamHandle, ConnectionIntent, ExecRequest, RemoteConnectionLease,
        RemoteConnectionService,
    },
    files::FileService,
};
use kerminal_domain::{
    commands::{
        CommandHistoryTarget, CommandSuggestionCandidate, CommandSuggestionRequest,
        SuggestionProviderKind,
    },
    connections::RemoteShellDialect,
    files::{FileEndpoint, FileEntryKind, RemotePath},
    identity::HostId,
};
use tokio::time::{Instant, sleep_until, timeout, timeout_at};
use tokio::{sync::Mutex, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use self::cache::{BoundedAsyncCache, lock_gate};

const REMOTE_QUERY_BUDGET: Duration = Duration::from_secs(4);
const REMOTE_EXECUTION_RESERVE: Duration = Duration::from_millis(500);
const REMOTE_OPERATION_CLEANUP_BUDGET: Duration = Duration::from_millis(250);
const REMOTE_FAILURE_BACKOFF: Duration = Duration::from_secs(4);
const DIRECTORY_CACHE_TTL: Duration = Duration::from_secs(30);
const COMMAND_CACHE_TTL: Duration = Duration::from_secs(300);
const GIT_CACHE_TTL: Duration = Duration::from_secs(60);
const DIRECTORY_CACHE_CAPACITY: usize = 32;
const COMMAND_CACHE_CAPACITY: usize = 64;
const GIT_CACHE_CAPACITY: usize = 64;
const MAX_DIRECTORY_CACHE_ENTRIES: usize = 2_048;
const MAX_PATH_ENTRIES: usize = 80;
const MAX_COMMAND_ENTRIES: usize = 1_024;
const MAX_GIT_REFS: usize = 500;
const MAX_PATH_DIRECTORIES: usize = 12;
const MAX_PROBE_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_REQUEST_INPUT_CHARACTERS: usize = 4_096;
const MAX_REQUEST_CWD_CHARACTERS: usize = 4_096;
const MAX_REQUEST_CONTEXT_CHARACTERS: usize = 1_024;

/// 将一次建议请求的目标绑定、取消令牌和共享操作/清理期限一起传给各来源。
/// 来源只读取同一份上下文，避免拆散会话验证与资源清理预算。
struct RemoteSuggestionQuery<'a> {
    request: &'a CommandSuggestionRequest,
    prefix: &'a syntax::ParsedPrefix,
    host_id: &'a HostId,
    lease: Option<&'a RemoteConnectionLease>,
    cancellation: &'a CancellationToken,
    operation_deadline: Instant,
    cleanup_deadline: Instant,
}

/// 固定只读远端命令的来源、argv 与工作目录，不接受用户输入拼接到程序参数。
pub(super) struct ReadOnlyCommand {
    pub(super) source: SuggestionProviderKind,
    pub(super) program: &'static str,
    pub(super) arguments: Vec<String>,
    pub(super) working_directory: Option<String>,
}

/// Host 上的一层目录投影；保留只读补全所需字段，不缓存远端属性和文件内容。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CachedDirectoryEntry {
    pub(super) name: String,
    pub(super) kind: FileEntryKind,
    pub(super) executable: bool,
}

/// 只控制配置仓储是否处于可用读写态；用户来源选择由 application 按请求快照传入。
#[derive(Clone, Default)]
pub struct RemoteSuggestionPolicy {
    runtime_available: Arc<AtomicBool>,
}

impl RemoteSuggestionPolicy {
    /// 根据设置仓储恢复态初始化硬门禁，不把持久用户偏好混入共享 provider 状态。
    pub fn new(runtime_available: bool) -> Self {
        Self {
            runtime_available: Arc::new(AtomicBool::new(runtime_available)),
        }
    }

    /// 配置恢复或有效重载后更新唯一硬门禁，不受 UI 预览值或旧 watcher 快照影响。
    pub fn set_runtime_available(&self, available: bool) {
        self.runtime_available.store(available, Ordering::Release);
    }

    /// 在每个远端连接和 I/O 前读取恢复态门禁，关闭时任何远端来源都不能继续探测。
    pub fn runtime_available(&self) -> bool {
        self.runtime_available.load(Ordering::Acquire)
    }

    /// 为远端 provider 提供恢复态的最后一道门禁；具体来源是否被用户选中由请求列表决定。
    pub(super) fn allows(&self, provider: SuggestionProviderKind) -> bool {
        self.runtime_available()
            && matches!(
                provider,
                SuggestionProviderKind::RemotePath
                    | SuggestionProviderKind::RemoteCommand
                    | SuggestionProviderKind::Git
            )
    }
}

/// 连接文件与结构化命令端口，缓存受容量和 TTL 双重限制，不持久化远端清单。
pub struct RemoteCommandSuggestionProvider {
    files: FileService,
    connections: Arc<dyn RemoteConnectionService>,
    policy: RemoteSuggestionPolicy,
    remote_exec_gate: Mutex<()>,
    pending_cleanup: Mutex<Option<PendingRemoteCleanup>>,
    directories: BoundedAsyncCache<(String, String), Vec<CachedDirectoryEntry>>,
    executables: BoundedAsyncCache<(String, String, String), Vec<String>>,
    git_refs: BoundedAsyncCache<(String, String), Vec<String>>,
    git_remotes: BoundedAsyncCache<(String, String), Vec<String>>,
}

/// 保留超时中的连接/channel task 与 SSH owner，后续建议或桌面退出时继续 join 和清理。
struct PendingRemoteCleanup {
    operation: Option<PendingRemoteOperation>,
    stream: Option<CommandStreamHandle>,
    lease: Option<RemoteConnectionLease>,
}

/// 一次连接或 exec 若跨过请求 deadline，JoinHandle 仍由 provider 持有并可继续等待。
enum PendingRemoteOperation {
    Connecting(JoinHandle<AppResult<RemoteConnectionLease>>),
    Executing {
        task: JoinHandle<AppResult<CommandStreamHandle>>,
        lease: RemoteConnectionLease,
    },
}

impl PendingRemoteCleanup {
    /// 在同一绝对期限内先关闭 exec channel、再关闭持有它的 SSH lease；任一失败都保留 owner。
    async fn close_until(&mut self, deadline: Instant) -> AppResult<()> {
        if let Some(mut operation) = self.operation.take() {
            let mut pending_error = None;
            match &mut operation {
                PendingRemoteOperation::Connecting(task) => {
                    match timeout_at(deadline, task).await {
                        Ok(Ok(Ok(lease))) => self.lease = Some(lease),
                        Ok(Ok(Err(_))) | Ok(Err(_)) => {}
                        Err(_) => pending_error = Some("远端建议 SSH 连接清理仍在进行"),
                    }
                }
                PendingRemoteOperation::Executing { task, lease } => {
                    match timeout_at(deadline, task).await {
                        Ok(Ok(Ok(stream))) => self.stream = Some(stream),
                        Ok(Ok(Err(_))) | Ok(Err(_)) => {}
                        Err(_) => pending_error = Some("远端建议 exec channel 清理仍在进行"),
                    }
                    self.lease = Some(lease.clone());
                }
            }
            if let Some(message) = pending_error {
                self.operation = Some(operation);
                return Err(AppError::timeout(message));
            }
        }
        if let Some(stream) = self.stream.as_mut() {
            stream.close_until(deadline).await?;
            self.stream = None;
        }
        if let Some(lease) = self.lease.as_ref() {
            lease.close_until(deadline, "远端建议查询结束").await?;
            self.lease = None;
        }
        Ok(())
    }
}

impl RemoteCommandSuggestionProvider {
    /// 复用正式 SFTP 与 SSH 连接 adapter；缓存仅服务补全，不形成第二套认证体系。
    pub fn new(
        files: FileService,
        connections: Arc<dyn RemoteConnectionService>,
        policy: RemoteSuggestionPolicy,
    ) -> Self {
        Self {
            files,
            connections,
            policy,
            remote_exec_gate: Mutex::new(()),
            pending_cleanup: Mutex::new(None),
            directories: BoundedAsyncCache::new(DIRECTORY_CACHE_CAPACITY, DIRECTORY_CACHE_TTL),
            executables: BoundedAsyncCache::new(COMMAND_CACHE_CAPACITY, COMMAND_CACHE_TTL),
            git_refs: BoundedAsyncCache::new(GIT_CACHE_CAPACITY, GIT_CACHE_TTL),
            git_remotes: BoundedAsyncCache::new(GIT_CACHE_CAPACITY, GIT_CACHE_TTL),
        }
    }

    /// 在应用退出阶段 join 仍待关闭的建议通道，避免 deadline 超时时丢弃底层 SSH owner。
    pub async fn shutdown_until(&self, deadline: Instant) -> AppResult<()> {
        let _guard = timeout_at(deadline, self.remote_exec_gate.lock())
            .await
            .map_err(|_| AppError::timeout("远端建议连接退出清理"))?;
        self.flush_pending_cleanup(deadline).await
    }

    /// 重试唯一待清理 owner；仍未关闭时阻止新连接，避免累积无法 join 的 SSH 资源。
    async fn flush_pending_cleanup(&self, deadline: Instant) -> AppResult<()> {
        let pending = self.pending_cleanup.lock().await.take();
        let Some(mut pending) = pending else {
            return Ok(());
        };
        if let Err(error) = pending.close_until(deadline).await {
            *self.pending_cleanup.lock().await = Some(pending);
            return Err(error);
        }
        Ok(())
    }

    /// 关闭活动 lease；失败时将拥有式句柄交回单槽清理 owner，禁止 fire-and-forget。
    async fn close_lease(&self, lease: RemoteConnectionLease, deadline: Instant) -> AppResult<()> {
        // channel 关闭失败时 pending slot 已持有 lease clone；重复关闭会破坏后续重试。
        if self.pending_cleanup.lock().await.is_some() {
            return Ok(());
        }
        match lease.close_until(deadline, "远端建议查询结束").await {
            Ok(()) => Ok(()),
            Err(error) => {
                let mut pending = self.pending_cleanup.lock().await;
                if pending.is_none() {
                    *pending = Some(PendingRemoteCleanup {
                        operation: None,
                        stream: None,
                        lease: Some(lease),
                    });
                }
                Err(error)
            }
        }
    }

    /// 只在取消时显式取消连接任务；若任务未及时完成，将 JoinHandle 收回清理槽而不 detach。
    async fn connect_for_suggestions(
        &self,
        host_id: &HostId,
        cancellation: &CancellationToken,
        operation_deadline: Instant,
        cleanup_deadline: Instant,
    ) -> AppResult<Option<RemoteConnectionLease>> {
        let operation_cancel = cancellation.child_token();
        let task_cancellation = operation_cancel.clone();
        let connections = self.connections.clone();
        let host_id = host_id.clone();
        let mut task = tokio::spawn(async move {
            connections
                .connect(host_id, ConnectionIntent::Exec, task_cancellation)
                .await
        });
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                operation_cancel.cancel();
                self.retain_or_close(PendingRemoteCleanup {
                    operation: Some(PendingRemoteOperation::Connecting(task)),
                    stream: None,
                    lease: None,
                }, cleanup_deadline).await;
                Err(AppError::cancelled("远端建议查询已取消"))
            }
            _ = sleep_until(operation_deadline) => {
                operation_cancel.cancel();
                self.retain_or_close(PendingRemoteCleanup {
                    operation: Some(PendingRemoteOperation::Connecting(task)),
                    stream: None,
                    lease: None,
                }, cleanup_deadline).await;
                Ok(None)
            }
            result = &mut task => match result {
                Ok(Ok(lease)) => Ok(Some(lease)),
                _ => Ok(None),
            }
        }
    }

    /// 先在本请求清理预算内等待并释放资源；期限不足时持有同一 JoinHandle 供退出流程继续 join。
    async fn retain_or_close(
        &self,
        mut pending_operation: PendingRemoteCleanup,
        deadline: Instant,
    ) {
        if pending_operation.close_until(deadline).await.is_ok() {
            return;
        }
        let mut pending = self.pending_cleanup.lock().await;
        if pending.is_none() {
            *pending = Some(pending_operation);
        }
    }

    /// 通过同一个已认证 lease 执行固定 argv，并把超时/取消/输出上限纳入单一请求预算。
    async fn execute_read_only(
        &self,
        query: &RemoteSuggestionQuery<'_>,
        command: ReadOnlyCommand,
    ) -> AppResult<Option<Vec<u8>>> {
        let Some(lease) = query.lease else {
            return Ok(None);
        };
        if !self.policy.allows(command.source)
            || lease.shell_dialect() == RemoteShellDialect::Unknown
        {
            return Ok(None);
        }
        let request = ExecRequest {
            program: command.program.to_owned(),
            args: command.arguments,
            environment: Default::default(),
            working_directory: command.working_directory,
        };
        let operation_cancel = query.cancellation.child_token();
        let task_cancellation = operation_cancel.clone();
        let owned_lease = lease.clone();
        let mut task =
            tokio::spawn(async move { owned_lease.exec_stream(request, task_cancellation).await });
        let mut stream = tokio::select! {
            biased;
            _ = query.cancellation.cancelled() => {
                operation_cancel.cancel();
                self.retain_or_close(PendingRemoteCleanup {
                    operation: Some(PendingRemoteOperation::Executing { task, lease: lease.clone() }),
                    stream: None,
                    lease: None,
                }, query.cleanup_deadline).await;
                return Err(AppError::cancelled("远端建议查询已取消"));
            }
            _ = sleep_until(query.operation_deadline) => {
                operation_cancel.cancel();
                self.retain_or_close(PendingRemoteCleanup {
                    operation: Some(PendingRemoteOperation::Executing { task, lease: lease.clone() }),
                    stream: None,
                    lease: None,
                }, query.cleanup_deadline).await;
                return Ok(None);
            }
            result = &mut task => match result {
                Ok(Ok(stream)) => stream,
                _ => return Ok(None),
            }
        };
        let output = collect_command_output(
            &mut stream,
            query.cancellation,
            query.operation_deadline,
            MAX_PROBE_OUTPUT_BYTES,
        )
        .await;
        if let Err(error) = stream.close_until(query.cleanup_deadline).await {
            let mut pending = self.pending_cleanup.lock().await;
            if pending.is_none() {
                *pending = Some(PendingRemoteCleanup {
                    operation: None,
                    stream: Some(stream),
                    lease: Some(lease.clone()),
                });
            }
            let _ = error;
            if query.cancellation.is_cancelled() {
                return Err(AppError::cancelled("远端建议查询已取消"));
            }
            return Ok(None);
        }
        if query.cancellation.is_cancelled() {
            return Err(AppError::cancelled("远端建议查询已取消"));
        }
        if !self.policy.allows(command.source) {
            return Ok(None);
        }
        output
    }

    /// 返回缓存命中或单目录 SFTP 结果，超时、失败与取消都会阻止失败快照进入缓存。
    pub(super) async fn directory_entries(
        &self,
        source: SuggestionProviderKind,
        host_id: &HostId,
        path: &RemotePath,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> AppResult<Option<Arc<Vec<CachedDirectoryEntry>>>> {
        if !self.policy.allows(source) {
            return Ok(None);
        }
        let key = (host_id.as_str().to_owned(), path.as_str().to_owned());
        if let Some(entries) = self.directories.get(&key).await {
            return Ok(Some(entries));
        }
        if self.directories.is_backing_off(&key).await {
            return Ok(None);
        }
        let gate = self.directories.gate(&key);
        let guard = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("远端建议查询已取消")),
            _ = sleep_until(deadline) => return Ok(None),
            guard = lock_gate(gate, cancellation) => guard,
        };
        let Some(_guard) = guard else {
            return Err(AppError::cancelled("远端建议查询已取消"));
        };
        if let Some(entries) = self.directories.get(&key).await {
            return Ok(Some(entries));
        }
        if self.directories.is_backing_off(&key).await {
            return Ok(None);
        }

        let endpoint = FileEndpoint::Remote {
            host_id: host_id.clone(),
            path: path.clone(),
        };
        if !self.policy.allows(source) {
            return Ok(None);
        }
        let listing = self
            .run_bounded(cancellation, deadline, |cancel| {
                self.files.list_directory(cancel, endpoint.clone())
            })
            .await?;
        let Some(listing) = listing else {
            self.directories
                .mark_failure(key, REMOTE_FAILURE_BACKOFF)
                .await;
            return Ok(None);
        };
        if !self.policy.allows(source) {
            return Ok(None);
        }
        if listing.directory != endpoint {
            self.directories
                .mark_failure(key, REMOTE_FAILURE_BACKOFF)
                .await;
            return Ok(None);
        }
        let entries = listing
            .entries
            .into_iter()
            .take(MAX_DIRECTORY_CACHE_ENTRIES)
            .filter(|entry| {
                !entry.name.is_empty()
                    && entry.name != "."
                    && entry.name != ".."
                    && !entry.name.chars().any(char::is_control)
            })
            .map(|entry| CachedDirectoryEntry {
                name: entry.name,
                kind: entry.kind,
                executable: entry
                    .permissions
                    .is_some_and(|permissions| permissions & 0o111 != 0),
            })
            .collect::<Vec<_>>();
        let entries = Arc::new(entries);
        self.directories.insert(key, (*entries).clone()).await;
        Ok(Some(entries))
    }

    /// 执行总预算内的只读操作；取消或超时时先通知子任务，再给协议适配器短暂收尾窗口。
    pub(super) async fn run_bounded<T, Build, Fut>(
        &self,
        cancellation: &CancellationToken,
        deadline: Instant,
        build: Build,
    ) -> AppResult<Option<T>>
    where
        Build: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = AppResult<T>>,
    {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("远端建议查询已取消"));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let operation_cancel = cancellation.child_token();
        let operation = build(operation_cancel.clone());
        tokio::pin!(operation);
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                operation_cancel.cancel();
                let _ = timeout(REMOTE_OPERATION_CLEANUP_BUDGET, &mut operation).await;
                Err(AppError::cancelled("远端建议查询已取消"))
            }
            _ = sleep_until(deadline) => {
                operation_cancel.cancel();
                let _ = timeout(REMOTE_OPERATION_CLEANUP_BUDGET, &mut operation).await;
                Ok(None)
            }
            result = &mut operation => match result {
                Ok(value) => Ok(Some(value)),
                Err(_) => Ok(None),
            }
        }
    }

    /// 远端 host 执行路径中的任意错误都降级为无补全，不能拖垮本地历史或片段 provider。
    pub(super) fn request_host(
        request: &CommandSuggestionRequest,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<HostId>> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("远端建议查询已取消"));
        }
        if request.target != CommandHistoryTarget::Ssh {
            return Ok(None);
        }
        let Some(value) = request.remote_host_id.as_deref() else {
            return Ok(None);
        };
        Ok(HostId::parse(value).ok())
    }

    /// 计算缓存键时同时绑定 session 与 cwd，避免在目标切换或 PATH/CWD 变化后串用候选。
    pub(super) fn command_cache_key(
        host_id: &HostId,
        request: &CommandSuggestionRequest,
    ) -> (String, String, String) {
        (
            host_id.as_str().to_owned(),
            request.session_id.clone().unwrap_or_default(),
            request.cwd.clone().unwrap_or_default(),
        )
    }

    /// 计算工作目录缓存键，保留主机边界并统一缺省 cwd 的表示。
    pub(super) fn cwd_cache_key(
        host_id: &HostId,
        request: &CommandSuggestionRequest,
    ) -> (String, String) {
        (
            host_id.as_str().to_owned(),
            request.cwd.clone().unwrap_or_default(),
        )
    }
}

#[async_trait::async_trait]
impl CommandSuggestionProvider for RemoteCommandSuggestionProvider {
    /// 三类建议由同一 adapter 协调，避免为相同远端目标重复装配连接服务。
    fn supported_providers(&self) -> Vec<SuggestionProviderKind> {
        vec![
            SuggestionProviderKind::RemotePath,
            SuggestionProviderKind::RemoteCommand,
            SuggestionProviderKind::Git,
        ]
    }

    /// 仅处理明确 SSH session 和用户输入前缀，过期、超时、未知 shell 或恢复态关闭均安全降级。
    async fn suggest(
        &self,
        request: CommandSuggestionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<CommandSuggestionCandidate>> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("远端建议查询已取消"));
        }
        let input_characters = request
            .input
            .chars()
            .take(MAX_REQUEST_INPUT_CHARACTERS + 1)
            .count();
        if !self.policy.runtime_available()
            || request.target != CommandHistoryTarget::Ssh
            || request.session_id.as_deref().is_none_or(|session| {
                session.trim().is_empty()
                    || session
                        .chars()
                        .take(MAX_REQUEST_CONTEXT_CHARACTERS + 1)
                        .count()
                        > MAX_REQUEST_CONTEXT_CHARACTERS
                    || session.chars().any(char::is_control)
            })
            || request.generation.is_none()
            || input_characters > MAX_REQUEST_INPUT_CHARACTERS
            || request.cursor != input_characters
            || request.input.chars().any(char::is_control)
            || request.cwd.as_deref().is_some_and(|cwd| {
                cwd.chars().take(MAX_REQUEST_CWD_CHARACTERS + 1).count()
                    > MAX_REQUEST_CWD_CHARACTERS
                    || cwd.chars().any(char::is_control)
            })
            || request.context_key.as_deref().is_some_and(|context| {
                context
                    .chars()
                    .take(MAX_REQUEST_CONTEXT_CHARACTERS + 1)
                    .count()
                    > MAX_REQUEST_CONTEXT_CHARACTERS
                    || context.chars().any(char::is_control)
            })
            || request.shell.as_deref().is_some_and(|shell| {
                shell.chars().take(257).count() > 256 || shell.chars().any(char::is_control)
            })
            || request
                .providers
                .as_ref()
                .is_some_and(|providers| providers.is_empty())
        {
            return Ok(Vec::new());
        }
        let Some(host_id) = Self::request_host(&request, &cancellation)? else {
            return Ok(Vec::new());
        };
        // SSH request.shell 来自终端投影且可能缺失或过期；先用未知语法决定是否需认证探测。
        let Some(initial_prefix) =
            syntax::parse_prefix(&request.input, request.cursor, syntax::ShellFamily::Unknown)
        else {
            return Ok(Vec::new());
        };
        let deadline = Instant::now() + REMOTE_QUERY_BUDGET;
        let operation_deadline = deadline - REMOTE_EXECUTION_RESERVE;
        let dynamic_command = requested(&request, SuggestionProviderKind::RemoteCommand)
            && self.policy.allows(SuggestionProviderKind::RemoteCommand)
            && remote_command::requires_remote_query(&initial_prefix);
        let dynamic_git = requested(&request, SuggestionProviderKind::Git)
            && self.policy.allows(SuggestionProviderKind::Git)
            && git::requires_remote_query(&initial_prefix);
        let needs_ssh = dynamic_command || dynamic_git;
        let _remote_exec_guard = if needs_ssh {
            let guard = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(AppError::cancelled("远端建议查询已取消")),
                _ = sleep_until(operation_deadline) => return Ok(Vec::new()),
                guard = self.remote_exec_gate.lock() => guard,
            };
            Some(guard)
        } else {
            None
        };
        if needs_ssh && self.flush_pending_cleanup(deadline).await.is_err() {
            return Ok(Vec::new());
        }
        let lease = if needs_ssh {
            self.connect_for_suggestions(&host_id, &cancellation, operation_deadline, deadline)
                .await?
        } else {
            None
        };
        let shell = lease
            .as_ref()
            .map_or(syntax::ShellFamily::Unknown, |lease| {
                syntax::remote_shell_family(lease.shell_kind())
            });
        let Some(prefix) = syntax::parse_prefix(&request.input, request.cursor, shell) else {
            if let Some(lease) = lease {
                let _ = self.close_lease(lease, deadline).await;
            }
            return Ok(Vec::new());
        };
        let query = RemoteSuggestionQuery {
            request: &request,
            prefix: &prefix,
            host_id: &host_id,
            lease: lease.as_ref(),
            cancellation: &cancellation,
            operation_deadline,
            cleanup_deadline: deadline,
        };
        let candidates = async {
            let mut candidates = Vec::new();
            if self.policy.allows(SuggestionProviderKind::RemotePath)
                && requested(&request, SuggestionProviderKind::RemotePath)
            {
                let source_candidates = remote_path::suggestions(self, &query).await?;
                if self.policy.allows(SuggestionProviderKind::RemotePath) {
                    candidates.extend(source_candidates);
                }
            }
            if self.policy.allows(SuggestionProviderKind::RemoteCommand)
                && requested(&request, SuggestionProviderKind::RemoteCommand)
            {
                let source_candidates = remote_command::suggestions(self, &query).await?;
                if self.policy.allows(SuggestionProviderKind::RemoteCommand) {
                    candidates.extend(source_candidates);
                }
            }
            if self.policy.allows(SuggestionProviderKind::Git)
                && requested(&request, SuggestionProviderKind::Git)
            {
                let source_candidates = git::suggestions(self, &query).await?;
                if self.policy.allows(SuggestionProviderKind::Git) {
                    candidates.extend(source_candidates);
                }
            }
            Ok::<_, AppError>(candidates)
        }
        .await;
        if let Some(lease) = lease {
            let close_result = self.close_lease(lease, deadline).await;
            if candidates.is_ok() {
                close_result?;
            }
        }
        let mut candidates = candidates?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("远端建议查询已取消"));
        }
        candidates.truncate(request.limit.unwrap_or(20).clamp(1, 100));
        Ok(candidates)
    }
}

/// 以单一期限读取有限 stdout，非零退出、截断输出、错误流或取消均不能伪装成有效候选。
async fn collect_command_output(
    stream: &mut CommandStreamHandle,
    cancellation: &CancellationToken,
    deadline: Instant,
    maximum_bytes: usize,
) -> AppResult<Option<Vec<u8>>> {
    let mut stdout = Vec::new();
    let mut total_bytes = 0usize;
    loop {
        let event = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("远端建议查询已取消")),
            _ = sleep_until(deadline) => return Ok(None),
            event = stream.next_event() => event,
        };
        let Ok(Some(event)) = event else {
            return Ok(None);
        };
        match event {
            CommandEvent::Stdout(bytes) => {
                total_bytes = total_bytes.saturating_add(bytes.len());
                if total_bytes > maximum_bytes {
                    return Ok(None);
                }
                stdout.extend(bytes);
            }
            CommandEvent::Stderr(bytes) => {
                total_bytes = total_bytes.saturating_add(bytes.len());
                if total_bytes > maximum_bytes {
                    return Ok(None);
                }
            }
            CommandEvent::Exited { code, signal } => {
                return Ok((code == Some(0) && signal.is_none()).then_some(stdout));
            }
        }
    }
}
