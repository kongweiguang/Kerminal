//! 远端建议 adapter 的公开行为回归测试。
//!
//! @author kongweiguang

use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    commands::CommandSuggestionProvider,
    connections::{
        CommandEvent, CommandInputHandle, CommandStreamHandle, ConnectionIntent, HostKeyChallenge,
        HostKeyPeer, HostKeyTrustReceipt, HostKeyTrustRequest, OpenSftpRequest, RemoteCommandInput,
        RemoteCommandStream, RemoteConnectionHandle, RemoteConnectionLease,
        RemoteConnectionService, RemoteForwardHandle, RemoteForwardRequest, RemoteShellKind,
        SftpChannelHandle, ShellScriptRequest, TerminalOpenRequest, TunnelChannelHandle,
        TunnelRequest,
    },
    files::{FileAccessProvider, FileBookmarkRepository, FileService},
    sessions::SessionTransport,
};
use kerminal_domain::{
    commands::{
        CommandHistoryTarget, CommandSuggestionRequest, SuggestionProviderKind, SuggestionQueryMode,
    },
    connections::{ConnectionCapabilities, RemoteProtocol, RemoteShellDialect},
    files::{
        DeleteTargetKind, DirectoryListing, FileBookmark, FileEndpoint, FileEntry, FileEntryKind,
        FilePreview, TextDocument, WriteTextRequest, WriteTextResult,
    },
    identity::HostId,
};
use kerminal_infrastructure::commands::suggestion_sources::{
    RemoteCommandSuggestionProvider, RemoteSuggestionPolicy,
};
use tokio_util::sync::CancellationToken;

/// 将 ASCII 主机 ID 构造错误集中在测试夹具，避免测试对非法上下文容错。
fn test_host_id() -> HostId {
    HostId::parse("host-test").expect("test host id is valid")
}

/// 为远端路径测试装配只读内存 listing，计数用于验证关闭策略不会发起 I/O。
fn provider(
    runtime_available: bool,
    list_calls: Arc<AtomicUsize>,
) -> (
    RemoteCommandSuggestionProvider,
    RemoteSuggestionPolicy,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<FakeRemoteState>,
) {
    provider_with_shell(
        runtime_available,
        list_calls,
        RemoteShellKind::Bash,
        RemoteShellDialect::Posix,
        false,
    )
}

/// 为连接边界测试设置可信 shell 元数据与可取消行为，仍只使用内存 lease。
fn provider_with_shell(
    runtime_available: bool,
    list_calls: Arc<AtomicUsize>,
    shell_kind: RemoteShellKind,
    shell_dialect: RemoteShellDialect,
    wait_for_exec_cancel: bool,
) -> (
    RemoteCommandSuggestionProvider,
    RemoteSuggestionPolicy,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<FakeRemoteState>,
) {
    let files = FileService::new(
        vec![Arc::new(FakeRemoteFiles {
            list_calls,
            started: None,
            wait_for_cancel: false,
            cancellation_observed: None,
        })],
        Arc::new(FakeBookmarks),
    )
    .expect("fake file service is valid");
    let policy = RemoteSuggestionPolicy::new(runtime_available);
    let connect_calls = Arc::new(AtomicUsize::new(0));
    let command_calls = Arc::new(AtomicUsize::new(0));
    let state = Arc::new(FakeRemoteState {
        exec_calls: command_calls.clone(),
        exec_requests: std::sync::Mutex::new(Vec::new()),
        stream_close_calls: Arc::new(AtomicUsize::new(0)),
        lease_close_calls: Arc::new(AtomicUsize::new(0)),
        exec_started: Arc::new(tokio::sync::Notify::new()),
        wait_for_exec_cancel,
        shell_kind,
        shell_dialect,
    });
    (
        RemoteCommandSuggestionProvider::new(
            files,
            Arc::new(FakeRemoteConnections {
                host_id: test_host_id(),
                connect_calls: connect_calls.clone(),
                state: state.clone(),
            }),
            policy.clone(),
        ),
        policy,
        command_calls,
        connect_calls,
        state,
    )
}

/// 构造绑定 SSH session、明确 provider 和 Unicode 光标位置的建议请求。
fn request(input: &str, providers: Vec<SuggestionProviderKind>) -> CommandSuggestionRequest {
    CommandSuggestionRequest {
        input: input.to_owned(),
        cursor: input.chars().count(),
        target: CommandHistoryTarget::Ssh,
        session_id: Some("session-test".to_owned()),
        pane_id: None,
        profile_id: None,
        remote_host_id: Some(test_host_id().as_str().to_owned()),
        cwd: Some("/home/test".to_owned()),
        shell: None,
        providers: Some(providers),
        limit: Some(20),
        mode: SuggestionQueryMode::Inline,
        generation: Some(3),
        context_key: Some("session-test:prompt:3".to_owned()),
    }
}

/// 文件 mock 仅支持 remote 单层目录读取，其它操作明确失败以防测试掩盖错误路径。
struct FakeRemoteFiles {
    list_calls: Arc<AtomicUsize>,
    started: Option<Arc<tokio::sync::Notify>>,
    wait_for_cancel: bool,
    cancellation_observed: Option<Arc<AtomicUsize>>,
}

#[async_trait]
impl FileAccessProvider for FakeRemoteFiles {
    /// 只匹配远端 endpoint，保持与生产 SFTP provider 相同的文件边界。
    fn supports(&self, endpoint: &FileEndpoint) -> bool {
        matches!(endpoint, FileEndpoint::Remote { .. })
    }

    /// 返回有限夹具目录项，或在取消用例中等待 token 后记录收尾，不访问网络和磁盘。
    async fn list_directory(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
    ) -> AppResult<DirectoryListing> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(started) = &self.started {
            started.notify_one();
        }
        if self.wait_for_cancel {
            cancellation.cancelled().await;
            if let Some(observed) = &self.cancellation_observed {
                observed.fetch_add(1, Ordering::SeqCst);
            }
            return Err(AppError::cancelled("测试远端 listing 已取消"));
        }
        let FileEndpoint::Remote { host_id, path } = &endpoint else {
            return Err(AppError::unsupported("测试只支持远端路径"));
        };
        let entries = [
            ("文档", FileEntryKind::Directory, Some(0o755u32)),
            ("README.md", FileEntryKind::File, Some(0o644u32)),
            ("unsafe name", FileEntryKind::File, Some(0o644u32)),
            ("vim", FileEntryKind::File, Some(0o755u32)),
        ]
        .into_iter()
        .map(|(name, kind, permissions)| FileEntry {
            endpoint: FileEndpoint::Remote {
                host_id: host_id.clone(),
                path: path.join_name(name).expect("fixture path is valid"),
            },
            name: name.to_owned(),
            kind,
            size: None,
            modified: None,
            permissions,
            is_hidden: name.starts_with('.'),
            raw: None,
        })
        .collect();
        Ok(DirectoryListing {
            directory: endpoint,
            entries,
        })
    }

    /// 预览不属于 suggestion adapter 的职责，测试 mock 明确拒绝调用。
    async fn preview_file(
        &self,
        _cancellation: CancellationToken,
        _endpoint: FileEndpoint,
        _max_bytes: usize,
    ) -> AppResult<FilePreview> {
        Err(AppError::unsupported("测试不支持预览"))
    }

    /// 文本读取不属于 suggestion adapter 的职责，测试 mock 明确拒绝调用。
    async fn read_text_file(
        &self,
        _cancellation: CancellationToken,
        _endpoint: FileEndpoint,
        _max_bytes: usize,
    ) -> AppResult<TextDocument> {
        Err(AppError::unsupported("测试不支持文本读取"))
    }

    /// 写入不属于 suggestion adapter 的职责，测试 mock 明确拒绝调用。
    async fn write_text_file(
        &self,
        _cancellation: CancellationToken,
        _request: WriteTextRequest,
    ) -> AppResult<WriteTextResult> {
        Err(AppError::unsupported("测试不支持文本写入"))
    }

    /// 建目录不属于 suggestion adapter 的职责，测试 mock 明确拒绝调用。
    async fn create_directory(
        &self,
        _cancellation: CancellationToken,
        _endpoint: FileEndpoint,
    ) -> AppResult<()> {
        Err(AppError::unsupported("测试不支持目录创建"))
    }

    /// 重命名不属于 suggestion adapter 的职责，测试 mock 明确拒绝调用。
    async fn rename(
        &self,
        _cancellation: CancellationToken,
        _source: FileEndpoint,
        _destination: FileEndpoint,
    ) -> AppResult<()> {
        Err(AppError::unsupported("测试不支持重命名"))
    }

    /// chmod 不属于 suggestion adapter 的职责，测试 mock 明确拒绝调用。
    async fn chmod(
        &self,
        _cancellation: CancellationToken,
        _endpoint: FileEndpoint,
        _mode: u32,
    ) -> AppResult<()> {
        Err(AppError::unsupported("测试不支持权限修改"))
    }

    /// 删除不属于 suggestion adapter 的职责，测试 mock 明确拒绝调用。
    async fn delete(
        &self,
        _cancellation: CancellationToken,
        _endpoint: FileEndpoint,
        _kind: DeleteTargetKind,
    ) -> AppResult<()> {
        Err(AppError::unsupported("测试不支持删除"))
    }
}

/// 不保留书签数据；文件 service 在这些只读 suggestion 测试中不会访问此仓储。
struct FakeBookmarks;

#[async_trait]
impl FileBookmarkRepository for FakeBookmarks {
    /// 未使用的 bookmark 查询明确返回空列表。
    async fn list(
        &self,
        _cancellation: CancellationToken,
        _target: kerminal_domain::files::BookmarkTarget,
    ) -> AppResult<Vec<FileBookmark>> {
        Ok(Vec::new())
    }

    /// 未使用的 bookmark 修改明确拒绝。
    async fn set(
        &self,
        _cancellation: CancellationToken,
        _bookmark: FileBookmark,
        _bookmarked: bool,
    ) -> AppResult<Vec<FileBookmark>> {
        Err(AppError::unsupported("测试不支持书签修改"))
    }
}

/// 连接夹具只记录固定 exec 请求、取消和资源释放，不启动进程或访问真实主机。
struct FakeRemoteState {
    exec_calls: Arc<AtomicUsize>,
    exec_requests: std::sync::Mutex<Vec<kerminal_application::connections::ExecRequest>>,
    stream_close_calls: Arc<AtomicUsize>,
    lease_close_calls: Arc<AtomicUsize>,
    exec_started: Arc<tokio::sync::Notify>,
    wait_for_exec_cancel: bool,
    shell_kind: RemoteShellKind,
    shell_dialect: RemoteShellDialect,
}

struct FakeRemoteConnections {
    host_id: HostId,
    connect_calls: Arc<AtomicUsize>,
    state: Arc<FakeRemoteState>,
}

#[async_trait]
impl RemoteConnectionService for FakeRemoteConnections {
    /// 为明确 SSH HostId 创建独立 lease，连接次数可验证策略和目标边界。
    async fn connect(
        &self,
        host_id: HostId,
        intent: ConnectionIntent,
        cancellation: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("测试连接取消"));
        }
        if host_id != self.host_id || intent != ConnectionIntent::Exec {
            return Err(AppError::invalid("测试连接目标或用途不匹配"));
        }
        self.connect_calls.fetch_add(1, Ordering::SeqCst);
        Ok(RemoteConnectionLease::new(Arc::new(FakeRemoteHandle {
            host_id,
            state: self.state.clone(),
            closed: std::sync::atomic::AtomicBool::new(false),
        })))
    }

    /// 本测试不查询主机指纹。
    async fn inspect_host_key(
        &self,
        _host_id: HostId,
        _peer: HostKeyPeer,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyChallenge> {
        Err(AppError::unsupported("测试未配置主机指纹查询"))
    }

    /// 本测试不修改 known_hosts。
    async fn trust_host_key(
        &self,
        _request: HostKeyTrustRequest,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyTrustReceipt> {
        Err(AppError::unsupported("测试未配置主机信任"))
    }

    /// 此测试替身未覆盖主机密钥轮换，显式拒绝可避免伪造信任回执。
    ///
    /// @author kongweiguang
    async fn replace_host_key(
        &self,
        _request: kerminal_application::connections::HostKeyRotationRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> kerminal_application::AppResult<kerminal_application::connections::HostKeyRotationReceipt>
    {
        Err(kerminal_application::AppError::unsupported(
            "test fake does not support host-key rotation",
        ))
    }
}

struct FakeRemoteHandle {
    host_id: HostId,
    state: Arc<FakeRemoteState>,
    closed: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl RemoteConnectionHandle for FakeRemoteHandle {
    /// 返回 lease 创建时绑定的稳定 HostId。
    fn host_id(&self) -> &HostId {
        &self.host_id
    }

    /// 测试连接只开放 SSH 能力。
    fn capabilities(&self) -> ConnectionCapabilities {
        ConnectionCapabilities::for_protocol(RemoteProtocol::Ssh)
    }

    /// 返回测试显式配置的可信命令方言。
    fn shell_dialect(&self) -> RemoteShellDialect {
        self.state.shell_dialect
    }

    /// 返回测试显式配置的已认证 shell kind。
    fn shell_kind(&self) -> RemoteShellKind {
        self.state.shell_kind
    }

    /// 该 fake 不打开交互 terminal。
    async fn open_terminal(
        &self,
        _request: TerminalOpenRequest,
        _cancel: CancellationToken,
    ) -> AppResult<SessionTransport> {
        Err(AppError::unsupported("测试未配置 terminal"))
    }

    /// 仅回放固定 argv 对应的 PATH/Git 输出，并可验证请求取消确实传到 SSH adapter。
    async fn exec_stream(
        &self,
        request: kerminal_application::connections::ExecRequest,
        cancellation: CancellationToken,
    ) -> AppResult<CommandStreamHandle> {
        self.state.exec_calls.fetch_add(1, Ordering::SeqCst);
        self.state
            .exec_requests
            .lock()
            .map_err(|_| AppError::infrastructure("测试请求记录锁中毒"))?
            .push(request.clone());
        self.state.exec_started.notify_one();
        if self.state.wait_for_exec_cancel {
            cancellation.cancelled().await;
            return Err(AppError::cancelled("测试 exec 已取消"));
        }
        let stdout = if request.program == "printenv" && request.args == ["PATH"] {
            b"/bin\n".to_vec()
        } else if request.program == "Get-Item" && request.args == ["Env:Path"] {
            b"Name Value\n---- -----\nPath C:\\Windows\\System32;C:\\Tools\n".to_vec()
        } else if request.program == "git"
            && request
                .args
                .first()
                .is_some_and(|arg| arg == "for-each-ref")
        {
            b"feature/login\nfeature/$expand\nmain\n".to_vec()
        } else if request.program == "git"
            && request.args.first().is_some_and(|arg| arg == "remote")
        {
            b"origin\n".to_vec()
        } else {
            Vec::new()
        };
        let mut events = VecDeque::new();
        if !stdout.is_empty() {
            events.push_back(CommandEvent::Stdout(stdout));
        }
        events.push_back(CommandEvent::Exited {
            code: Some(0),
            signal: None,
        });
        Ok(CommandStreamHandle::new(Box::new(FakeSuggestionStream {
            events,
            close_calls: self.state.stream_close_calls.clone(),
        })))
    }

    /// suggestion adapter 不执行原始 shell 文本。
    async fn exec_shell(
        &self,
        _request: ShellScriptRequest,
        _cancel: CancellationToken,
    ) -> AppResult<CommandStreamHandle> {
        Err(AppError::unsupported("测试不接受原始 shell"))
    }

    /// suggestion 路径通过 FileService，不在该 command lease 上建立 SFTP。
    async fn open_sftp(
        &self,
        _request: OpenSftpRequest,
        _cancel: CancellationToken,
    ) -> AppResult<SftpChannelHandle> {
        Err(AppError::unsupported("测试未配置 SFTP"))
    }

    /// suggestion adapter 不创建 TCP tunnel。
    async fn open_tunnel(
        &self,
        _request: TunnelRequest,
        _cancel: CancellationToken,
    ) -> AppResult<TunnelChannelHandle> {
        Err(AppError::unsupported("测试未配置 tunnel"))
    }

    /// suggestion adapter 不请求远端 listener。
    async fn request_remote_forward(
        &self,
        _request: RemoteForwardRequest,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteForwardHandle> {
        Err(AppError::unsupported("测试未配置 remote forward"))
    }

    /// 重复关闭只统计一次，验证同一 lease 经清理槽和调用方结束时的幂等性。
    async fn close_until(&self, _deadline: tokio::time::Instant, _reason: &str) -> AppResult<()> {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.state.lease_close_calls.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

struct FakeSuggestionInput;

#[async_trait]
impl RemoteCommandInput for FakeSuggestionInput {
    /// suggestion 查询不会写入 stdin。
    async fn write_stdin(&self, _bytes: Vec<u8>) -> AppResult<()> {
        Err(AppError::unsupported("测试不写入 stdin"))
    }

    /// suggestion 查询不半关闭 stdin。
    async fn close_stdin(&self) -> AppResult<()> {
        Ok(())
    }
}

struct FakeSuggestionStream {
    events: VecDeque<CommandEvent>,
    close_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl RemoteCommandStream for FakeSuggestionStream {
    /// 按顺序回放固定 exec 输出，EOF 与成功退出状态可重复验证。
    async fn next_event(&mut self) -> AppResult<Option<CommandEvent>> {
        Ok(self.events.pop_front())
    }

    /// 测试 stream 不使用 stdin，但保留完整 application stream 契约。
    fn input_handle(&self) -> CommandInputHandle {
        CommandInputHandle::new(Arc::new(FakeSuggestionInput))
    }

    /// 记录显式 channel 清理，确保输出消费结束后总会调用 close。
    async fn close_until(&mut self, _deadline: tokio::time::Instant) -> AppResult<()> {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// 运行态门禁关闭时不访问目录；恢复可写后才访问已绑定 SSH 的一层目录。
#[tokio::test]
async fn runtime_gate_is_checked_before_remote_io() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (service, policy, command_calls, connect_calls, _) = provider(false, calls.clone());
    let query = request("cd 文", vec![SuggestionProviderKind::RemotePath]);
    let cancellation = CancellationToken::new();

    assert!(
        service
            .suggest(query.clone(), cancellation.clone())
            .await
            .expect("disabled remote suggestions return empty")
            .is_empty()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    policy.set_runtime_available(true);
    let candidates = service
        .suggest(query, cancellation)
        .await
        .expect("path suggestion succeeds");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(command_calls.load(Ordering::SeqCst), 0);
    assert_eq!(connect_calls.load(Ordering::SeqCst), 0);
    let candidate = candidates
        .iter()
        .find(|candidate| candidate.display_text == "文档/")
        .expect("directory candidate exists");
    assert_eq!(candidate.replacement_text, "文档/");
    assert_eq!(candidate.replacement_range.start, 3);
    assert_eq!(candidate.replacement_range.end, 4);
}

/// 恢复态门禁关闭时路径、PATH 命令和 Git 三类远端来源都不触发 I/O。
#[tokio::test]
async fn runtime_gate_blocks_all_remote_sources_before_io() {
    let list_calls = Arc::new(AtomicUsize::new(0));
    let (service, _, command_calls, connect_calls, _) = provider(false, list_calls.clone());

    let path = request("cd 文", vec![SuggestionProviderKind::RemotePath]);
    let commands = request("vi", vec![SuggestionProviderKind::RemoteCommand]);
    let git = request("git checkout feat", vec![SuggestionProviderKind::Git]);
    for query in [path, commands, git] {
        assert!(
            service
                .suggest(query, CancellationToken::new())
                .await
                .expect("closed runtime gate returns empty")
                .is_empty()
        );
    }

    assert_eq!(list_calls.load(Ordering::SeqCst), 0);
    assert_eq!(command_calls.load(Ordering::SeqCst), 0);
    assert_eq!(connect_calls.load(Ordering::SeqCst), 0);
}

/// 缺 session/generation 或超长输入的请求不绑定终端，也不触发无界解析。
#[tokio::test]
async fn remote_suggestions_require_bounded_explicit_context() {
    let list_calls = Arc::new(AtomicUsize::new(0));
    let (service, _, command_calls, connect_calls, _) = provider(true, list_calls.clone());
    let mut missing_session = request("cd 文", vec![SuggestionProviderKind::RemotePath]);
    missing_session.session_id = None;
    let mut missing_generation = request("cd 文", vec![SuggestionProviderKind::RemotePath]);
    missing_generation.generation = None;
    let oversized = request(&"x".repeat(4_097), vec![SuggestionProviderKind::RemotePath]);

    assert!(
        service
            .suggest(missing_session, CancellationToken::new())
            .await
            .expect("missing session safely returns no candidates")
            .is_empty()
    );
    assert!(
        service
            .suggest(missing_generation, CancellationToken::new())
            .await
            .expect("missing generation safely returns no candidates")
            .is_empty()
    );
    assert!(
        service
            .suggest(oversized, CancellationToken::new())
            .await
            .expect("oversized input safely returns no candidates")
            .is_empty()
    );
    assert_eq!(list_calls.load(Ordering::SeqCst), 0);
    assert_eq!(command_calls.load(Ordering::SeqCst), 0);
    assert_eq!(connect_calls.load(Ordering::SeqCst), 0);
}

/// Git status 子命令和常用参数来自有限本地词表，不触发远端命令执行。
#[tokio::test]
async fn git_status_suggestions_are_static_and_context_bound() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (service, _, command_calls, connect_calls, _) = provider(true, calls.clone());
    let query = request("git status --s", vec![SuggestionProviderKind::Git]);
    let candidates = service
        .suggest(query, CancellationToken::new())
        .await
        .expect("static Git suggestion succeeds");

    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.display_text == "--short")
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| { candidate.context_key.as_deref() == Some("session-test:prompt:3") })
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(command_calls.load(Ordering::SeqCst), 0);
    assert_eq!(connect_calls.load(Ordering::SeqCst), 0);
}

/// 命令补全只探测结构化 PATH argv，并从一层目录的可执行属性读取名称。
#[tokio::test]
async fn remote_command_suggestions_use_executable_path_entries() {
    let list_calls = Arc::new(AtomicUsize::new(0));
    let (service, _, command_calls, connect_calls, state) = provider(true, list_calls.clone());
    let query = request("vi", vec![SuggestionProviderKind::RemoteCommand]);
    let candidates = service
        .suggest(query, CancellationToken::new())
        .await
        .expect("remote command suggestion succeeds");

    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.display_text == "vim")
    );
    assert_eq!(command_calls.load(Ordering::SeqCst), 1);
    assert_eq!(list_calls.load(Ordering::SeqCst), 1);
    assert_eq!(connect_calls.load(Ordering::SeqCst), 1);
    let exec_requests = state.exec_requests.lock().expect("exec request lock");
    assert_eq!(exec_requests[0].program, "printenv");
    assert_eq!(exec_requests[0].args, ["PATH"]);
}

/// PowerShell 的 PATH 表格按环境变量名解析，Windows drive 路径映射为 SFTP POSIX 路径。
#[tokio::test]
async fn remote_command_suggestions_support_powershell_windows_paths() {
    let list_calls = Arc::new(AtomicUsize::new(0));
    let (service, _, command_calls, connect_calls, state) = provider_with_shell(
        true,
        list_calls.clone(),
        RemoteShellKind::PowerShell,
        RemoteShellDialect::PowerShell,
        false,
    );
    let mut query = request("vi", vec![SuggestionProviderKind::RemoteCommand]);
    query.cwd = Some("C:\\Users\\demo".to_owned());
    let candidates = service
        .suggest(query, CancellationToken::new())
        .await
        .expect("PowerShell remote command suggestion succeeds");

    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.display_text == "vim")
    );
    assert_eq!(command_calls.load(Ordering::SeqCst), 1);
    assert_eq!(list_calls.load(Ordering::SeqCst), 2);
    assert_eq!(connect_calls.load(Ordering::SeqCst), 1);
    let exec_requests = state.exec_requests.lock().expect("exec request lock");
    assert_eq!(exec_requests[0].program, "Get-Item");
    assert_eq!(exec_requests[0].args, ["Env:Path"]);
    assert_eq!(
        exec_requests[0].working_directory.as_deref(),
        Some("C:\\Users\\demo")
    );
}

/// Git refs 通过固定只读 for-each-ref 参数查询，并将候选绑定到 session generation。
///
/// @author kongweiguang
#[tokio::test]
async fn git_reference_suggestions_use_fixed_read_only_query() {
    let list_calls = Arc::new(AtomicUsize::new(0));
    let (service, _, command_calls, connect_calls, state) = provider(true, list_calls);
    let query = request("git checkout feat", vec![SuggestionProviderKind::Git]);
    let candidates = service
        .suggest(query, CancellationToken::new())
        .await
        .expect("Git reference suggestion succeeds");

    let candidate = candidates
        .iter()
        .find(|candidate| candidate.display_text == "feature/login")
        .expect("matching Git ref exists");
    assert_eq!(candidate.replacement_text, "feature/login");
    let quoted_reference = candidates
        .iter()
        .find(|candidate| candidate.display_text == "feature/$expand")
        .expect("shell-sensitive Git ref remains available");
    assert_eq!(quoted_reference.replacement_text, "'feature/$expand'");
    assert_eq!(candidate.replacement_range.start, 13);
    assert_eq!(candidate.replacement_range.end, 17);
    assert_eq!(command_calls.load(Ordering::SeqCst), 1);
    assert_eq!(connect_calls.load(Ordering::SeqCst), 1);
    let exec_requests = state.exec_requests.lock().expect("exec request lock");
    assert_eq!(exec_requests[0].program, "git");
    assert_eq!(exec_requests[0].args[0], "for-each-ref");
    assert!(
        !exec_requests[0]
            .args
            .iter()
            .any(|argument| argument == "feature")
    );
}

/// 输入中的 shell 名称不能覆盖 lease 的可信探测结果，未知实现不执行命令补全。
#[tokio::test]
async fn unknown_remote_shell_is_not_guessed_from_request_metadata() {
    let list_calls = Arc::new(AtomicUsize::new(0));
    let (service, _, command_calls, connect_calls, _) = provider_with_shell(
        true,
        list_calls,
        RemoteShellKind::Unknown,
        RemoteShellDialect::Unknown,
        false,
    );
    let mut query = request("vi", vec![SuggestionProviderKind::RemoteCommand]);
    query.shell = Some("bash".to_owned());

    assert!(
        service
            .suggest(query, CancellationToken::new())
            .await
            .expect("unknown shell degrades safely")
            .is_empty()
    );
    assert_eq!(connect_calls.load(Ordering::SeqCst), 1);
    assert_eq!(command_calls.load(Ordering::SeqCst), 0);
}

/// 已知 SSH 方言仍允许本地 Git 词表；可信类型未知时不得为静态候选打开连接。
#[tokio::test]
async fn static_git_suggestions_do_not_probe_unknown_shell() {
    let (service, _, command_calls, connect_calls, _) = provider_with_shell(
        true,
        Arc::new(AtomicUsize::new(0)),
        RemoteShellKind::Unknown,
        RemoteShellDialect::Unknown,
        false,
    );
    let mut query = request("git status --s", vec![SuggestionProviderKind::Git]);
    query.shell = Some("bash".to_owned());
    let candidates = service
        .suggest(query, CancellationToken::new())
        .await
        .expect("static candidates do not need a remote shell probe");

    assert!(candidates.iter().any(|item| item.display_text == "--short"));
    assert_eq!(connect_calls.load(Ordering::SeqCst), 0);
    assert_eq!(command_calls.load(Ordering::SeqCst), 0);
}

/// 取消在任何远端 probe 前返回明确错误，不将取消转成空结果。
#[tokio::test]
async fn cancellation_is_not_reported_as_successful_empty_result() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (service, _, command_calls, connect_calls, _) = provider(true, calls.clone());
    let query = request("cd 文", vec![SuggestionProviderKind::RemotePath]);
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    assert!(service.suggest(query, cancellation).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(command_calls.load(Ordering::SeqCst), 0);
    assert_eq!(connect_calls.load(Ordering::SeqCst), 0);
}

/// 在 SFTP listing 途中取消时确认 child token 被转发且 adapter 在有界收尾内退出。
#[tokio::test]
async fn cancellation_reaches_in_flight_remote_file_operation() {
    let list_calls = Arc::new(AtomicUsize::new(0));
    let cancellation_observed = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let files = FileService::new(
        vec![Arc::new(FakeRemoteFiles {
            list_calls: list_calls.clone(),
            started: Some(started.clone()),
            wait_for_cancel: true,
            cancellation_observed: Some(cancellation_observed.clone()),
        })],
        Arc::new(FakeBookmarks),
    )
    .expect("blocking fake file service is valid");
    let connect_calls = Arc::new(AtomicUsize::new(0));
    let exec_calls = Arc::new(AtomicUsize::new(0));
    let state = Arc::new(FakeRemoteState {
        exec_calls: exec_calls.clone(),
        exec_requests: std::sync::Mutex::new(Vec::new()),
        stream_close_calls: Arc::new(AtomicUsize::new(0)),
        lease_close_calls: Arc::new(AtomicUsize::new(0)),
        exec_started: Arc::new(tokio::sync::Notify::new()),
        wait_for_exec_cancel: false,
        shell_kind: RemoteShellKind::Bash,
        shell_dialect: RemoteShellDialect::Posix,
    });
    let service = RemoteCommandSuggestionProvider::new(
        files,
        Arc::new(FakeRemoteConnections {
            host_id: test_host_id(),
            connect_calls: connect_calls.clone(),
            state,
        }),
        RemoteSuggestionPolicy::new(true),
    );
    let cancellation = CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let worker = tokio::spawn(async move {
        service
            .suggest(
                request("cd 文", vec![SuggestionProviderKind::RemotePath]),
                worker_cancellation,
            )
            .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), started.notified())
        .await
        .expect("remote file request started");
    cancellation.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), worker)
        .await
        .expect("cancelled provider joined within cleanup bound")
        .expect("provider task did not panic");

    assert!(matches!(result, Err(AppError::Cancelled { .. })));
    assert_eq!(list_calls.load(Ordering::SeqCst), 1);
    assert_eq!(cancellation_observed.load(Ordering::SeqCst), 1);
    assert_eq!(connect_calls.load(Ordering::SeqCst), 0);
    assert_eq!(exec_calls.load(Ordering::SeqCst), 0);
}

/// exec 建立期间取消会传递到 SSH 任务，并在 lease 仍由 provider 持有时完成关闭。
#[tokio::test]
async fn cancellation_joins_and_closes_in_flight_ssh_exec() {
    let (service, _, exec_calls, connect_calls, state) = provider_with_shell(
        true,
        Arc::new(AtomicUsize::new(0)),
        RemoteShellKind::Bash,
        RemoteShellDialect::Posix,
        true,
    );
    let cancellation = CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let started = state.exec_started.clone();
    let worker = tokio::spawn(async move {
        service
            .suggest(
                request("vi", vec![SuggestionProviderKind::RemoteCommand]),
                worker_cancellation,
            )
            .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), started.notified())
        .await
        .expect("SSH exec task started");
    cancellation.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), worker)
        .await
        .expect("cancelled provider and exec task are joined")
        .expect("provider task did not panic");

    assert!(matches!(result, Err(AppError::Cancelled { .. })));
    assert_eq!(connect_calls.load(Ordering::SeqCst), 1);
    assert_eq!(exec_calls.load(Ordering::SeqCst), 1);
    assert_eq!(state.lease_close_calls.load(Ordering::SeqCst), 1);
}
