//! 应用层终端会话用例与端口。
//!
//! UI、MCP、Agent 与运维功能共享本服务；所有操作都显式使用 SessionId，避免隐式切换 Tab。
//!
//! @author kongweiguang

pub mod agent_input;
pub mod broadcast;
pub mod command_blocks;
pub mod logging;
mod transport;

pub use agent_input::{AgentTerminalSubmissionRequest, MAX_AGENT_TERMINAL_SUBMISSION_BYTES};
pub use command_blocks::{
    COMMAND_BLOCK_RETENTION_LIMIT, CommandBlock, CommandBlockId,
    CommandBlockOutputIncompleteReason, CommandBlockOutputSnapshot, CommandBlockOutputState,
    CommandBlockOutputText, CommandBlockSnapshot, CommandBlockState, CommandBlockText,
    CommandBlockTextUnknownReason, CommandBlockUnknownReason, MAX_COMMAND_BLOCK_OUTPUT_BYTES,
    MAX_COMMAND_BLOCK_TEXT_BYTES, UiTerminalCommandSubmissionRequest, classify_command_block_text,
    is_plain_command_enter,
};

pub use kerminal_terminal::input::TerminalInputEvent;
pub use kerminal_terminal::snapshot::{
    OutputRevision, PlainTextSnapshot, PlainTextSnapshotRequest, RenderSnapshot,
    RenderSnapshotRequest, SelectionViewport, TerminalPalette, TerminalSearchRequest,
    TerminalSearchResult, TerminalSelection,
};
pub use tokio::time::Instant as SessionDeadline;
pub use transport::{
    SessionOpenCleanupOwner, SessionTransport, SessionTransportControl, SessionTransportFactory,
    SessionTransportOpenError, TerminalResizeCapability, io_transport_error,
};

use async_trait::async_trait;
use kerminal_domain::sessions::{
    SessionInfo, SessionLifecycle, SessionOwner, SessionTarget, TerminalSize,
};
use kerminal_domain::{connections::RemoteShellDialect, identity::SessionId};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{AppError, AppResult};

/// 默认 session 关闭预算；桌面批量关闭应据此只生成一个共享绝对 deadline。
///
/// @author kongweiguang
pub const DEFAULT_SESSION_CLOSE_BUDGET: Duration = Duration::from_secs(5);

/// 会话创建请求；owner 决定显示策略，target 独立决定实际 transport。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateSessionRequest {
    pub target: SessionTarget,
    pub owner: SessionOwner,
    pub size: TerminalSize,
}

/// 已建立 transport 接入唯一 runtime registry 时使用的请求。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttachTransportRequest {
    pub target: SessionTarget,
    pub owner: SessionOwner,
    pub size: TerminalSize,
}

/// 清理失败时 manager 仍持有资源 owner，并会在后续 shutdown 中继续回收。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionCleanupPending {
    pub primary_code: crate::PublicErrorCode,
    pub cleanup_code: crate::PublicErrorCode,
}

impl std::fmt::Display for SessionCleanupPending {
    /// 只暴露稳定错误类别，底层主机、命令或路径仍留在受控诊断 source 链中。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "terminal operation {} failed while cleanup {} remains pending",
            self.primary_code.as_str(),
            self.cleanup_code.as_str()
        )
    }
}

impl std::error::Error for SessionCleanupPending {}

/// 会话列表筛选条件；未指定的字段不推断当前 UI 目标。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionFilter {
    pub owner: Option<SessionOwner>,
    pub target: Option<SessionTarget>,
}

/// 终端输出、生命周期或几何变化；watch 只保留最新状态，不堆积无界 UI 通知。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionChange {
    pub revision: u64,
    pub kind: SessionChangeKind,
}

/// 为 UI、MCP 与 Agent 提供取消友好的变化订阅，不把 Tokio watch receiver 暴露到消费循环。
pub struct SessionSubscription {
    receiver: watch::Receiver<SessionChange>,
}

impl SessionSubscription {
    /// 将唯一 runtime 的合并状态流封装为应用端口，消费者只通过 `recv` 等待变化。
    ///
    /// @author kongweiguang
    pub fn from_receiver(receiver: watch::Receiver<SessionChange>) -> Self {
        Self { receiver }
    }

    /// 等待一个新 revision、显式取消或 sender 关闭；消费 revision 后不会重复返回旧值。
    ///
    /// @author kongweiguang
    pub async fn recv(
        &mut self,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<SessionChange>> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(AppError::cancelled("终端订阅已取消")),
            changed = self.receiver.changed() => match changed {
                Ok(()) => Ok(Some(*self.receiver.borrow_and_update())),
                Err(_) => Ok(None),
            },
        }
    }
}

/// 仅包含 transport 和 OSC 133 tracker 能直接确认的运行态元数据。
///
/// PID 对远端 transport 留空；prompt_ready 只在真实 shell integration marker 到达后为真。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionRuntimeMetadata {
    pub process_id: Option<u32>,
    pub shell_dialect: RemoteShellDialect,
    pub shell_integration_enabled: bool,
    pub shell_integration_observed: bool,
    pub prompt_ready: bool,
}

/// 将可见终端视图的 UI 或 Agent owner 与同一 SessionId 的 runtime generation 绑定。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiSessionBinding {
    /// 此可见终端 view 实际绑定的稳定终端会话。
    pub session_id: SessionId,
    /// 必须精确匹配该会话的 Ui 或 Agent owner；后台、MCP 与 service owner 不可借此写入。
    pub owner: SessionOwner,
    /// 取自最新 context；重连代次变化后旧 view 请求必须刷新绑定，避免写入新 runtime。
    pub session_generation: u64,
}

/// 命令建议请求期间需要保持不变的会话、prompt 与输入版本。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalCommandContextVersion {
    pub session_generation: u64,
    pub prompt_epoch: u64,
    pub input_generation: u64,
}

/// UI 是否能按本次输入事件准确维护自己的 transient command-line projection。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiInputProjection {
    /// 调用方已按同一事件更新 UI 私有输入 buffer；该 buffer 不进入 SessionService。
    Tracked,
    /// 事件带有未知编辑副作用，当前 prompt 的建议上下文要等下一个 OSC 133 A 才恢复。
    Invalidates,
}

/// 提示符状态足够可信时才允许展示 inline suggestions；正文始终由 UI 的显式输入状态维护。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalCommandAvailability {
    Available,
    Unavailable(TerminalCommandUnavailableReason),
}

/// 无法证明当前处于受支持 shell prompt 时的原因；不读取屏幕文本或推测 password 状态。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalCommandUnavailableReason {
    NonUiOwner,
    SessionNotRunning,
    ShellIntegrationDisabled,
    ShellIntegrationNotObserved,
    /// 包括 shell 尚未发出 A/B、正在运行命令、密码提示或其它交互式程序。
    PromptNotReady,
    AlternateScreen,
    UnsupportedShellDialect,
    ExternalInputObserved,
    InputWritePending,
    UiInputProjectionInvalidated,
    ManagedCommandActive,
}

/// 终端命令建议只投影 shell tracker、owner 与 generations，不含推断或缓存的命令行 buffer。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalCommandContext {
    pub session_id: SessionId,
    pub owner: SessionOwner,
    /// 每次同 SessionId 重连后递增，隔离旧 transport 的异步结果。
    pub session_generation: u64,
    /// 每次真实 OSC 133 A prompt-start 递增。
    pub prompt_epoch: u64,
    /// 所有已送入 transport 的终端输入共享递增；用于丢弃过期 suggestion。
    pub input_generation: u64,
    pub shell_dialect: RemoteShellDialect,
    /// OSC 133 A/B tracker 状态；availability 还会考虑提交中和交互式上下文。
    pub prompt_ready: bool,
    /// 只有自 OSC 133 A 后 runtime 未接受输入字节时为 true；不读取 readline buffer。
    pub prompt_line_empty: bool,
    pub alternate_screen: bool,
    /// 命令建议只能在 Available 时显示或接受。
    pub availability: TerminalCommandAvailability,
}

/// 同一次 UI command writer ACK；关联位只由 writer gate 在精确输入版本上提交候选后置为 true。
///
/// 该回执不代表 shell 已执行或完成命令；false 表示输入仍可能已发送，但正文不可作为可信历史使用。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiTerminalCommandSubmissionReceipt {
    pub context: TerminalCommandContext,
    pub command_associated: bool,
}

/// 单次 raw write 被 runtime 接受时的可信前态与 generation 回执，不保存命令正文。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalWriteAdmission {
    pub session_id: SessionId,
    pub owner: SessionOwner,
    pub lifecycle: SessionLifecycle,
    pub session_generation: u64,
    pub prompt_epoch: u64,
    pub input_generation_before: u64,
    pub accepted_input_generation: u64,
    pub shell_dialect: RemoteShellDialect,
    pub shell_integration_enabled: bool,
    pub shell_integration_observed: bool,
    pub prompt_ready: bool,
    pub prompt_line_empty: bool,
    pub alternate_screen: bool,
}

/// writer 完整 ack 的字节数，以及仅由可信 runtime 实现提供的输入准入快照。
///
/// `admission == None` 表示 adapter 只确认写入成功，不能用于推断 shell 命令历史。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalWriteReceipt {
    pub acknowledged_bytes: usize,
    pub admission: Option<TerminalWriteAdmission>,
}

impl TerminalCommandContext {
    /// 返回请求 inline suggestion 前后用于防止旧结果覆盖当前输入的 generation 三元组。
    ///
    /// @author kongweiguang
    pub const fn version(&self) -> TerminalCommandContextVersion {
        TerminalCommandContextVersion {
            session_generation: self.session_generation,
            prompt_epoch: self.prompt_epoch,
            input_generation: self.input_generation,
        }
    }
}

/// 用户从可见终端 view 输入；Agent owner 可写但其 projection 会失效，只有 UI 同步维护状态时才能标为 Tracked。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiTerminalInputRequest {
    pub binding: UiSessionBinding,
    pub expected_context: Option<TerminalCommandContextVersion>,
    pub projection: UiInputProjection,
    pub event: TerminalInputEvent,
}

/// 从可见工作台发起的普通命令行提交；绑定在请求内固定，供服务端原子复核 writer 所有权。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiCommandLineRequest {
    pub binding: UiSessionBinding,
    pub command: String,
}

/// 从可见片段入口直接提交模板文本；其多行编码允许沿用普通终端 Paste 语义。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiSnippetCommandLineRequest {
    pub binding: UiSessionBinding,
    pub command: String,
}

/// UI 更新原因；终端字节仍在有界输出路径中完整消费。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionChangeKind {
    Output,
    Resized,
    Title,
    Lifecycle,
    Closed,
}

/// 显式滚动命令；正值向 scrollback 前方移动，负值回到较新的内容。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScrollRequest {
    pub lines: i32,
}

/// UI 搜索视口策略；输入预览保留视口，显式导航可给浮层预留顶部安全行。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiTerminalSearchViewport {
    PreserveViewport,
    Reveal { safe_top_rows: u16 },
}

/// 搜索导航的同步一次性提交门；取消与滚动提交竞争时，先完成 CAS 的一方拥有终态。
///
/// @author kongweiguang
#[derive(Clone, Debug)]
pub struct UiTerminalSearchCommitGate {
    state: Arc<AtomicU8>,
}

impl UiTerminalSearchCommitGate {
    const PENDING: u8 = 0;
    const CANCELLED: u8 = 1;
    const COMMITTED: u8 = 2;

    /// 创建未提交的导航门；调用方必须在请求替换、关闭或失效时同步调用 `cancel`。
    ///
    /// @author kongweiguang
    pub fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(Self::PENDING)),
        }
    }

    /// 仅允许 pending 转为 cancelled；false 表示请求已进入 cancelled 或 committed 终态。
    ///
    /// @author kongweiguang
    pub fn cancel(&self) -> bool {
        self.state
            .compare_exchange(
                Self::PENDING,
                Self::CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// 仅允许 pending 转为 committed；服务必须在持有 engine 锁时提交后才能改变 viewport。
    ///
    /// @author kongweiguang
    pub fn try_commit(&self) -> bool {
        self.state
            .compare_exchange(
                Self::PENDING,
                Self::COMMITTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// 查询取消是否先赢得终态，用于在进入扫描或定位前快速拒绝失效请求。
    ///
    /// @author kongweiguang
    pub fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == Self::CANCELLED
    }
}

impl Default for UiTerminalSearchCommitGate {
    /// 默认构造一个 pending 门，保持普通 fixture 构造直观。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self::new()
    }
}

/// UI 搜索请求；`selected_match` 是针对本次新结果的目标索引，None 表示无选中命中。
///
/// @author kongweiguang
#[derive(Clone, Debug)]
pub struct UiTerminalSearchRequest {
    pub request: TerminalSearchRequest,
    pub selected_match: Option<usize>,
    pub viewport: UiTerminalSearchViewport,
    pub commit_gate: UiTerminalSearchCommitGate,
}

/// UI 搜索命中与定位完成后的 engine revision，供绘制层拒绝过期坐标。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiTerminalSearchResult {
    pub result: TerminalSearchResult,
    pub revision: OutputRevision,
    /// Reveal 时指示命中当前已处于浮层安全行下方；无历史可避让时为 false。
    pub reveal_safe_margin_reached: Option<bool>,
}

/// 仅在当前 shell integration 已就绪且输入行为空时注入的一条应用命令。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellCommandRequest {
    pub session_id: SessionId,
    /// UI proxy 提交时固定的 pane/session generation；后台与 MCP 调用保持 `None`。
    pub expected_ui_binding: Option<UiSessionBinding>,
    pub command_id: String,
    pub command: String,
    pub timeout: Duration,
}

/// OSC 133 命令结果；缺少真实 C/D 时使用 Unknown，禁止用 write 成功伪造退出码。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellCommandOutcome {
    Finished {
        command_id: String,
        exit_code: i32,
    },
    Unknown {
        command_id: String,
        reason: ShellCommandUnknownReason,
    },
}

/// 终端 shell 执行结果不可证明时的稳定原因枚举。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellCommandUnknownReason {
    Cancelled,
    TimedOut,
    Disconnected,
    InputInterrupted,
    MissingCommandStart,
    MissingCompletion,
    EventLagged,
}

/// 唯一终端会话服务端口，供桌面、Agent、MCP 与运维功能共享。
///
/// @author kongweiguang
#[async_trait]
pub trait SessionService: Send + Sync {
    /// 通过注入的 transport factory 为 Local、SSH 或其它已配置目标建立唯一 runtime 会话。
    ///
    /// @author kongweiguang
    async fn create(&self, request: CreateSessionRequest) -> AppResult<SessionInfo>;

    /// 创建终端时必须传播 UI 或 MCP 的取消信号；实现方负责等待 provider 清理未发布资源。
    ///
    /// @author kongweiguang
    async fn create_cancellable(
        &self,
        request: CreateSessionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<SessionInfo>;

    /// 为内部恢复事务预分配稳定 SessionId；默认拒绝不具备 registry 原子预留能力的 adapter。
    ///
    /// @author kongweiguang
    async fn create_with_session_id(
        &self,
        _request: CreateSessionRequest,
        _session_id: SessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        Err(AppError::unsupported(
            "当前会话服务不支持预分配 SessionId 创建",
        ))
    }

    /// 在容量 permit 归 manager 所有后才打开一次性 transport，避免预建通道无法纳入关闭回收。
    ///
    /// 默认拒绝未实现该 ownership 契约的 adapter；静态 factory 必须在 manager 路径内调用。
    ///
    /// @author kongweiguang
    async fn create_with_transport_factory(
        &self,
        _request: CreateSessionRequest,
        _factory: Arc<dyn SessionTransportFactory>,
        _cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        Err(AppError::unsupported(
            "当前会话服务不支持 manager-owned transport 创建",
        ))
    }

    /// 按明确筛选条件列出会话，不读取或猜测当前 Tab。
    ///
    /// @author kongweiguang
    async fn list(&self, filter: SessionFilter) -> AppResult<Vec<SessionInfo>>;

    /// 返回 transport PID、已确认 shell dialect 与实际观察到的 OSC 133 状态，不推断启动脚本路径。
    ///
    /// @author kongweiguang
    async fn runtime_metadata(&self, session_id: SessionId) -> AppResult<SessionRuntimeMetadata>;

    /// 返回真实 OSC 133 prompt 状态与 generation；后台 owner、password/交互提示及未知 shell 为 Unavailable。
    ///
    /// @author kongweiguang
    async fn terminal_command_context(
        &self,
        _session_id: SessionId,
    ) -> AppResult<TerminalCommandContext> {
        Err(AppError::unsupported(
            "当前会话服务不提供可信终端输入上下文",
        ))
    }

    /// 向显式会话写入协议字节；调用方应把输入编码交给 terminal engine。
    ///
    /// @author kongweiguang
    async fn write(&self, session_id: SessionId, bytes: Vec<u8>) -> AppResult<()>;

    /// 写入 raw bytes 并返回 admission 证据；默认仅回报写入 ack，不伪造 prompt 状态。
    ///
    /// @author kongweiguang
    async fn write_with_receipt(
        &self,
        session_id: SessionId,
        bytes: Vec<u8>,
    ) -> AppResult<TerminalWriteReceipt> {
        let acknowledged_bytes = bytes.len();
        self.write(session_id, bytes).await?;
        Ok(TerminalWriteReceipt {
            acknowledged_bytes,
            admission: None,
        })
    }

    /// 接收已加载的实时偏好值，使关闭 autoReconnect 能立即取消等待中的重试。
    ///
    /// 默认实现明确拒绝不消费该偏好的 adapter，避免 UI 误以为已应用设置。
    ///
    /// @author kongweiguang
    async fn set_auto_reconnect_enabled(&self, _enabled: bool) -> AppResult<()> {
        Err(AppError::unsupported(
            "当前会话服务不支持运行时更新 autoReconnect 偏好",
        ))
    }

    /// 把后台或未跟踪输入按当前 terminal mode 编码后写入，并使当前 UI prompt projection 失效。
    ///
    /// @author kongweiguang
    async fn write_input(&self, session_id: SessionId, event: TerminalInputEvent) -> AppResult<()>;

    /// 以可见 UI/Agent owner 与 generation 写入终端；仅 UI 可携带有效的 ghost-text context。
    ///
    /// Agent view 的输入必须使建议投影失效，且不会改变其 NonUiOwner 能力状态。
    ///
    /// @author kongweiguang
    async fn write_ui_input(
        &self,
        _request: UiTerminalInputRequest,
    ) -> AppResult<TerminalCommandContext> {
        Err(AppError::unsupported(
            "当前会话服务不支持受 owner 约束的终端 UI 输入",
        ))
    }

    /// 以一次受 writer gate 保护的粘贴加 Enter 提交可见 UI 命令，避免检查与写入之间的 owner 竞态。
    ///
    /// 该能力不要求 shell prompt marker；调用方仅可在完整 writer ack 后记录发送成功，错误不得自动重放。
    ///
    /// @author kongweiguang
    async fn submit_ui_command_line(
        &self,
        request: UiCommandLineRequest,
    ) -> AppResult<TerminalCommandContext>;

    /// 直接发送已由用户点击选择的片段；保留精确 UI owner/generation CAS，不要求 prompt 或 shell marker。
    ///
    /// 该窄能力沿用 Paste+Enter 的单次 writer 事务，并允许普通粘贴模式下的多行片段；旧命令入口仍执行其安全门禁。
    /// 未覆盖此能力的 adapter 不得假装片段已发送。
    ///
    /// @author kongweiguang
    async fn submit_ui_snippet_command_line(
        &self,
        _request: UiSnippetCommandLineRequest,
    ) -> AppResult<TerminalCommandContext> {
        Err(AppError::unsupported(
            "当前会话服务不支持直接发送 UI 命令片段",
        ))
    }

    /// 为可见 UI 的单次提交附带输入投影；正文只有经绑定、版本与完整 writer ACK 后才能关联 OSC C。
    ///
    /// 若正文未跟踪、疑似敏感或提交事件不明确，仍按原 UI 输入路径发送事件，命令块保留未知正文。
    ///
    /// @author kongweiguang
    async fn write_ui_command_submission(
        &self,
        _request: UiTerminalCommandSubmissionRequest,
    ) -> AppResult<TerminalCommandContext> {
        Err(AppError::unsupported(
            "当前会话服务不支持带可信命令投影的 UI 输入",
        ))
    }

    /// 返回同一次 writer 事务中的正文关联证明；未实现此窄回执的 adapter 必须 fail closed。
    ///
    /// @author kongweiguang
    async fn write_ui_command_submission_with_receipt(
        &self,
        request: UiTerminalCommandSubmissionRequest,
    ) -> AppResult<UiTerminalCommandSubmissionReceipt> {
        let context = self.write_ui_command_submission(request).await?;
        Ok(UiTerminalCommandSubmissionReceipt {
            context,
            command_associated: false,
        })
    }

    /// 以 AgentId、SessionId 和 runtime generation 做 writer 准入 CAS，再以一次 paste+Enter 提交。
    ///
    /// adapter 必须在任何字节进入 writer 前复核精确 Agent owner、Running 状态与 generation；
    /// 开始写后若取消、部分写入或 flush 结果不明，只能返回未知结果，调用方不得自动重放。
    ///
    /// @author kongweiguang
    async fn write_agent_submission(
        &self,
        _request: AgentTerminalSubmissionRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<TerminalWriteReceipt> {
        Err(AppError::unsupported(
            "当前会话服务不支持代次受限的 Agent 终端提交",
        ))
    }

    /// 查询显式 session 的最新有界 OSC 133 命令块；订阅流仅作失效提示。
    ///
    /// @author kongweiguang
    async fn command_block_snapshot(
        &self,
        _session_id: SessionId,
        _cancellation: &CancellationToken,
    ) -> AppResult<CommandBlockSnapshot> {
        Err(AppError::unsupported(
            "当前会话服务不提供 OSC 133 命令块快照",
        ))
    }

    /// 按真实 OSC C/D 建立的 capture 关联查询有限命令输出，不从当前网格或 scrollback 反推。
    ///
    /// @author kongweiguang
    async fn command_block_output(
        &self,
        _session_id: SessionId,
        _block_id: CommandBlockId,
        _max_bytes: usize,
        _cancellation: &CancellationToken,
    ) -> AppResult<CommandBlockOutputSnapshot> {
        Err(AppError::unsupported(
            "当前会话服务不提供按命令块关联的输出快照",
        ))
    }

    /// 调整终端网格与 transport 尺寸；无效的零行列会被领域层拒绝。
    ///
    /// @author kongweiguang
    async fn resize(&self, session_id: SessionId, size: TerminalSize) -> AppResult<()>;

    /// 按可见终端绑定调整尺寸；默认兼容旧 adapter，生产 runtime 应原子校验 owner/generation。
    ///
    /// Exited/Failed 会话仍可重排保留的历史网格；adapter 不应为已结束的 transport 发送 resize。
    ///
    /// @author kongweiguang
    async fn resize_ui_view(&self, binding: UiSessionBinding, size: TerminalSize) -> AppResult<()> {
        self.resize(binding.session_id, size).await
    }

    /// 返回只含请求视口和已分组 style runs 的 UI 渲染快照。
    ///
    /// @author kongweiguang
    async fn render_snapshot(
        &self,
        session_id: SessionId,
        request: RenderSnapshotRequest,
    ) -> AppResult<RenderSnapshot>;

    /// Reads a terminal view snapshot under its captured owner and generation for delayed UI refreshes.
    ///
    /// Default adapters retain compatibility; production managers override this to enforce view lifecycle access.
    ///
    /// @author kongweiguang
    async fn render_snapshot_ui(
        &self,
        binding: UiSessionBinding,
        request: RenderSnapshotRequest,
    ) -> AppResult<RenderSnapshot> {
        self.render_snapshot(binding.session_id, request).await
    }

    /// 以一个调色板同时更新既有快照绘制和 OSC 颜色响应，避免 UI 自行改写终端颜色语义。
    ///
    /// @author kongweiguang
    async fn set_palette(&self, session_id: SessionId, palette: TerminalPalette) -> AppResult<()>;

    /// 将用户 scrollback 偏好应用到同一引擎，返回受网格容量上限裁剪后的实际行数。
    ///
    /// @author kongweiguang
    async fn set_scrollback_limit(&self, session_id: SessionId, lines: usize) -> AppResult<usize>;

    /// 返回独立的纯文本快照与截断元数据，禁止 MCP 从渲染行重建文本。
    ///
    /// @author kongweiguang
    async fn plaintext_snapshot(
        &self,
        session_id: SessionId,
        request: PlainTextSnapshotRequest,
    ) -> AppResult<PlainTextSnapshot>;

    /// 在当前 session 的 scrollback 视口中滚动，终端进程与连接保持不变。
    ///
    /// @author kongweiguang
    async fn scroll(&self, session_id: SessionId, request: ScrollRequest) -> AppResult<()>;

    /// 按可见 UI/Agent owner 与 runtime generation 原子滚动；Exited/Failed 仍可查看保留历史。
    ///
    /// 视口滚动只读终端引擎，不向进程写入；Starting、Closing 与 Closed 会话拒绝此操作。
    ///
    /// @author kongweiguang
    async fn scroll_ui(&self, binding: UiSessionBinding, request: ScrollRequest) -> AppResult<()>;

    /// 按绝对历史 offset 跳转可见终端；默认拒绝不具备该原子绑定能力的 adapter。
    ///
    /// UI 快照只读取 engine viewport，只有经过 owner/generation 校验的显式命令能改变它；
    /// 保留默认实现可让尚未支持跳转的独立 SessionService adapter 继续编译并安全失败。
    ///
    /// @author kongweiguang
    async fn scroll_ui_to_offset(
        &self,
        _binding: UiSessionBinding,
        _offset: usize,
    ) -> AppResult<()> {
        Err(AppError::unsupported(
            "当前会话服务不支持按可见终端绑定跳转视口",
        ))
    }

    /// 设置显式网格选择；调用方共享同一个 session 时看到同一选择状态。
    ///
    /// @author kongweiguang
    async fn set_selection(
        &self,
        session_id: SessionId,
        selection: Option<TerminalSelection>,
    ) -> AppResult<()>;

    /// 将同一次 UI 拖选按 gesture id 更新，避免每个 pointer move 重置 Alacritty anchor。
    ///
    /// 默认委托兼容旧 adapter；具备终端引擎访问权的实现应在验证 binding/generation 后调用
    /// `TerminalEngine::update_selection_gesture`，并将已被 scrollback 裁掉的选择作为失效返回。
    ///
    /// @author kongweiguang
    async fn set_ui_selection(
        &self,
        binding: UiSessionBinding,
        selection: Option<TerminalSelection>,
        gesture_id: Option<u64>,
    ) -> AppResult<()> {
        let _ = gesture_id;
        self.set_selection(binding.session_id, selection).await
    }

    /// 允许调用方取消等待中的 pointer move，adapter 必须在 binding 锁定后再次检查取消状态。
    ///
    /// 默认先拒绝已取消请求，再委托兼容旧 selection adapter 的绑定入口；有并发重连的实现应覆写并
    /// 在 owner/generation 锁保持期间检查 token，防止等待锁的旧拖选事件覆盖新选择。
    ///
    /// @author kongweiguang
    async fn set_ui_selection_cancellable(
        &self,
        binding: UiSessionBinding,
        selection: Option<TerminalSelection>,
        gesture_id: Option<u64>,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("终端选择更新已取消"));
        }
        self.set_ui_selection(binding, selection, gesture_id).await
    }

    /// 为新拖选起点绑定读取快照的 revision，避免异步等待期间输出推进后按旧坐标锚定。
    ///
    /// 默认适配器保留现有行为；直接持有 terminal engine 的实现应在相同会话锁内执行 revision CAS，
    /// 并将过期的手势 token 记为 tombstone，防止迟到的 Move 绕过拒绝结果。
    /// 同一活动手势的 Move 应传 None，使输出后的端点更新继续沿用原 anchor。
    ///
    /// @author kongweiguang
    async fn set_ui_selection_cancellable_with_revision(
        &self,
        binding: UiSessionBinding,
        selection: Option<TerminalSelection>,
        gesture_id: Option<u64>,
        expected_revision: Option<OutputRevision>,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("终端选择更新已取消"));
        }
        let _ = expected_revision;
        self.set_ui_selection_cancellable(binding, selection, gesture_id, cancellation)
            .await
    }

    /// 用产生坐标的 viewport 解释新手势，使持续输出时的快照行可映射到当前网格。
    ///
    /// 默认适配器沿用旧绑定选择契约；拥有 terminal engine 的 runtime 应在同一 state 锁内使用 viewport
    /// 的 scroll origin、网格 epoch 和端点行 witness 验证坐标。普通全屏输出可重基准原内容，裁剪或改写拒绝猜测。
    ///
    /// @author kongweiguang
    async fn set_ui_selection_cancellable_in_viewport(
        &self,
        binding: UiSessionBinding,
        selection: Option<TerminalSelection>,
        gesture_id: Option<u64>,
        viewport: SelectionViewport,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("终端选择更新已取消"));
        }
        let _ = viewport;
        self.set_ui_selection_cancellable(binding, selection, gesture_id, cancellation)
            .await
    }

    /// 按同一可见终端绑定选中当前活动网格，避免快照边界与写入选择之间出现输出竞态。
    ///
    /// 适配器必须在验证 owner/generation 与更新 selection 的同一 session 锁期间操作；
    /// 已失效绑定不能改选到其它会话。默认拒绝未实现该原子能力的独立 adapter。
    ///
    /// @author kongweiguang
    async fn select_all_ui(&self, _binding: UiSessionBinding) -> AppResult<()> {
        Err(AppError::unsupported(
            "当前会话服务不支持按可见终端绑定全选",
        ))
    }

    /// 将当前选择按终端 wrap 与宽字符规则提取为文本。
    ///
    /// @author kongweiguang
    async fn selected_text(&self, session_id: SessionId) -> AppResult<Option<String>>;

    /// 用可见 view binding 读取选择，防止迟到 UI 复制请求跨 session generation 读到新选择。
    ///
    /// 默认 adapter 保留既有实现兼容；生产 manager 必须覆盖并原子校验 owner/generation/lifecycle。
    ///
    /// @author kongweiguang
    async fn selected_text_ui(&self, binding: UiSessionBinding) -> AppResult<Option<String>> {
        self.selected_text(binding.session_id).await
    }

    /// 在 scrollback 与当前屏幕内执行显式请求的搜索。
    ///
    /// @author kongweiguang
    async fn search(
        &self,
        session_id: SessionId,
        request: TerminalSearchRequest,
    ) -> AppResult<TerminalSearchResult>;

    /// 允许调用方取消全历史搜索；默认适配器只能可靠拒绝已取消请求，生产实现可覆盖扫描中取消。
    ///
    /// 保留旧 `search` 端口供未迁移调用方使用，避免把取消支持变成所有独立 adapter 的破坏性要求。
    ///
    /// @author kongweiguang
    async fn search_cancellable(
        &self,
        session_id: SessionId,
        request: TerminalSearchRequest,
        cancellation: CancellationToken,
    ) -> AppResult<TerminalSearchResult> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("终端搜索已取消"));
        }
        self.search(session_id, request).await
    }

    /// 按当前可见终端绑定重搜并定位命中，返回与 viewport scroll 原子关联的 revision。
    ///
    /// 普通 `search` adapter 无法保证 owner/generation 校验、重搜与滚动处于同一 engine 锁内，
    /// 因此默认拒绝；具备该原子能力的 runtime 应覆写，避免 UI 绘制旧坐标。
    ///
    /// @author kongweiguang
    async fn search_ui_cancellable(
        &self,
        _binding: UiSessionBinding,
        _request: UiTerminalSearchRequest,
        cancellation: CancellationToken,
    ) -> AppResult<UiTerminalSearchResult> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("终端搜索已取消"));
        }
        Err(AppError::unsupported(
            "当前会话服务不支持按可见终端绑定搜索",
        ))
    }

    /// 对已有远程 session 执行显式重连；失效目标应返回错误，不换成其他会话。
    ///
    /// @author kongweiguang
    async fn reconnect(&self, session_id: SessionId) -> AppResult<SessionInfo> {
        self.reconnect_cancellable(session_id, CancellationToken::new())
            .await
    }

    /// 允许 UI 与 MCP 取消远程重连；实现方须清理取消前已建立的 transport/channel。
    ///
    /// @author kongweiguang
    async fn reconnect_cancellable(
        &self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AppResult<SessionInfo>;

    /// 显式重启当前 session 时保留其身份与目标；默认拒绝未实现该生命周期操作的适配器。
    ///
    /// 单独提供入口可让 UI 重启运行中的会话，同时不扩大 MCP reconnect 的状态契约。
    ///
    /// @author kongweiguang
    async fn restart_cancellable(
        &self,
        _session_id: SessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        Err(AppError::unsupported("终端服务不支持显式重启会话"))
    }

    /// 取消、关闭 transport 并等待子进程或远端 channel 清理完成。
    ///
    /// @author kongweiguang
    async fn close(&self, session_id: SessionId) -> AppResult<()>;

    /// 在调用方提供的绝对 deadline 内关闭会话，避免外层 timeout 丢弃正在执行的资源收尾 future。
    ///
    /// @author kongweiguang
    async fn close_until(&self, session_id: SessionId, deadline: SessionDeadline) -> AppResult<()>;

    /// 关闭应用退出时仍由该 runtime 持有的全部会话。
    ///
    /// @author kongweiguang
    async fn close_all(&self) -> AppResult<()>;

    /// 订阅合并后的最新变化，用于安排重绘而不缓存无限 UI 事件。
    ///
    /// @author kongweiguang
    async fn subscribe(&self, session_id: SessionId) -> AppResult<SessionSubscription>;

    /// 串行化单条 shell 命令并将真实 OSC 133 C/D 关联到 request ID；有 UI binding 时在注入前复验 generation。
    ///
    /// @author kongweiguang
    async fn execute_shell_command(
        &self,
        request: ShellCommandRequest,
        cancellation: CancellationToken,
    ) -> AppResult<ShellCommandOutcome>;
}
