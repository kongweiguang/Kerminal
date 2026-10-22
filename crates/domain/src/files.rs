// @author kongweiguang

use crate::identity::{HostId, TransferId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub mod editor;
pub mod transfer_commit_protocol;
pub mod transfer_outcome;

use self::transfer_commit_protocol::{TransferCommitProtocol, TransferCommitTransitionError};
pub use self::transfer_outcome::TransferCommitSummary;

/// 容器运行时标识沿用基础设施层已验证的产品枚举，避免在文件目标中接收任意命令文本。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerRuntime {
    Docker,
    Podman,
}

/// 规范化的 POSIX 远端路径；不保存用户名、认证材料或 SSH 连接选项。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemotePath(String);

/// 文件路径拒绝原因，保持领域层不依赖应用层错误或协议 SDK。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilePathError {
    Empty,
    ControlCharacter,
    InvalidName,
}

impl RemotePath {
    /// 统一分隔符并移除无意义的斜杠和当前目录段；保留相对路径及父目录语义以兼容 SFTP 服务端。
    pub fn parse(value: &str) -> Result<Self, FilePathError> {
        if value.is_empty() {
            return Err(FilePathError::Empty);
        }
        if value.chars().any(char::is_control) {
            return Err(FilePathError::ControlCharacter);
        }

        let absolute = value.starts_with('/');
        let mut segments = Vec::new();
        for segment in value.split('/') {
            match segment {
                "" | "." => {}
                _ => segments.push(segment),
            }
        }

        let joined = segments.join("/");
        let normalized = if absolute {
            if joined.is_empty() {
                "/".to_owned()
            } else {
                format!("/{joined}")
            }
        } else if joined.is_empty() {
            ".".to_owned()
        } else {
            joined
        };
        Ok(Self(normalized))
    }

    /// 将容器用户路径锚定到容器根，并折叠内部父段；通用 SFTP 路径仍保留原有相对语义。
    ///
    /// @author kongweiguang
    pub fn parse_container(value: &str) -> Result<Self, FilePathError> {
        if value.is_empty() {
            return Err(FilePathError::Empty);
        }
        if value.chars().any(char::is_control) {
            return Err(FilePathError::ControlCharacter);
        }

        let normalized_input = value.replace('\\', "/");
        let mut segments = Vec::new();
        for segment in normalized_input.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    if segments.pop().is_none() {
                        return Err(FilePathError::InvalidName);
                    }
                }
                value => segments.push(value),
            }
        }

        let normalized = if segments.is_empty() {
            "/".to_owned()
        } else {
            format!("/{}", segments.join("/"))
        };
        Self::parse(&normalized)
    }

    /// 返回规范路径文本，供协议适配器在调用边界编码。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 按 POSIX 斜杠计算父目录；相对单段路径的父目录是当前目录，不借用宿主系统路径规则。
    pub fn parent(&self) -> Option<Self> {
        if self.0 == "/" || self.0 == "." {
            return None;
        }
        let path = self.0.trim_end_matches('/');
        let Some(separator) = path.rfind('/') else {
            return Self::parse(".").ok();
        };
        if separator == 0 {
            return Self::parse("/").ok();
        }
        Self::parse(&path[..separator]).ok()
    }

    /// 将单个目录项名称追加到路径，拒绝带路径分隔符的名称以防跨目录写入。
    pub fn join_name(&self, name: &str) -> Result<Self, FilePathError> {
        if name.is_empty() {
            return Err(FilePathError::Empty);
        }
        if name == "." || name == ".." || name.contains('/') || name.contains('\\') {
            return Err(FilePathError::InvalidName);
        }
        if name.chars().any(char::is_control) {
            return Err(FilePathError::ControlCharacter);
        }
        let base = self.as_str().trim_end_matches('/');
        let combined = if base.is_empty() {
            format!("/{name}")
        } else if base == "." {
            name.to_owned()
        } else {
            format!("{base}/{name}")
        };
        Self::parse(&combined)
    }
}

impl std::fmt::Display for RemotePath {
    /// 显示已规范化的路径，避免每个界面和日志调用方重复清理字符串。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// 文件操作目标统一表达本机、SSH/SFTP 和容器文件系统，连接凭据只由连接领域解析。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FileEndpoint {
    Local {
        path: PathBuf,
    },
    Remote {
        host_id: HostId,
        path: RemotePath,
    },
    Container {
        /// None 表示本机容器 CLI，Some 表示已保存主机上的容器运行时。
        host_id: Option<HostId>,
        runtime: ContainerRuntime,
        container_id: String,
        path: RemotePath,
    },
}

impl FileEndpoint {
    /// 为远端目标构造连接引用；只接受已保存主机 ID，不让文件用例接触 secret。
    pub fn remote(host_id: HostId, path: &str) -> Result<Self, FilePathError> {
        Ok(Self::Remote {
            host_id,
            path: RemotePath::parse(path)?,
        })
    }

    /// 返回远端路径文本，本机端点由 PathBuf 单独处理。
    pub fn remote_path(&self) -> Option<&RemotePath> {
        match self {
            Self::Remote { path, .. } | Self::Container { path, .. } => Some(path),
            Self::Local { .. } => None,
        }
    }

    /// 返回连接并发分组所需的主机身份，跨目标复制因此可按稳定顺序申请连接配额。
    pub fn host_id(&self) -> Option<&HostId> {
        match self {
            Self::Remote { host_id, .. } => Some(host_id),
            Self::Container { host_id, .. } => host_id.as_ref(),
            Self::Local { .. } => None,
        }
    }

    /// 判断两个端点是否指向同一逻辑对象，用于拒绝自覆盖和空操作复制。
    pub fn same_object(&self, other: &Self) -> bool {
        self == other
    }

    /// Rename 必须留在同一物理文件系统，容器和 SSH 主机即使共用连接也不可互换。
    pub fn same_filesystem(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Local { .. }, Self::Local { .. }) => true,
            (Self::Remote { host_id: left, .. }, Self::Remote { host_id: right, .. }) => {
                left == right
            }
            (
                Self::Container {
                    host_id: left_host,
                    runtime: left_runtime,
                    container_id: left_id,
                    ..
                },
                Self::Container {
                    host_id: right_host,
                    runtime: right_runtime,
                    container_id: right_id,
                    ..
                },
            ) => left_host == right_host && left_runtime == right_runtime && left_id == right_id,
            _ => false,
        }
    }
}

/// 目录条目类型保留符号链接身份，递归删除和传输不得默默跟随链接。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// 目录行是 UI/MCP 可共享的只读投影，不暴露底层 SSH/SFTP metadata 类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub endpoint: FileEndpoint,
    pub name: String,
    pub kind: FileEntryKind,
    pub size: Option<u64>,
    pub modified: Option<String>,
    pub permissions: Option<u32>,
    /// 来源报告的隐藏属性；领域对象不参与 serde 序列化，协议投影由各 adapter 显式维护。
    pub is_hidden: bool,
    /// 协议原文或仅由已收到属性生成的稳定列表行；不可得的值必须显式留空。
    pub raw: Option<String>,
}

/// 文件列表携带当前规范路径，避免请求完成时 UI 误将旧目录结果写入新选择。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryListing {
    pub directory: FileEndpoint,
    pub entries: Vec<FileEntry>,
}

/// 用元数据和可选内容摘要标识编辑时读到的版本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRevision {
    pub size: u64,
    pub modified: Option<String>,
    pub permissions: Option<u32>,
    pub content_digest: Option<String>,
}

/// 预览只返回有界内容，total_size 和 truncated 使 UI 不会误把截断结果当成完整文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePreview {
    pub endpoint: FileEndpoint,
    pub bytes: Vec<u8>,
    pub total_size: Option<u64>,
    pub truncated: bool,
    pub binary: bool,
    pub revision: Option<FileRevision>,
}

/// 文本编辑器读取结果保留原始行尾和 revision，保存时可做冲突判断与稳定往返。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextDocument {
    pub endpoint: FileEndpoint,
    pub content: String,
    pub line_ending: LineEnding,
    pub revision: FileRevision,
    pub readonly: bool,
    pub truncated: bool,
}

/// 仅支持编辑器明确处理的行尾类型，混合行尾在保存前保持不变。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
    Mixed,
}

/// 写入策略将并发校验、新建与用户明确覆盖分开，避免用伪造 revision 绕过冲突确认。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteTextStrategy {
    /// 按编辑器打开时的 revision 做乐观并发检查。
    CompareRevision(FileRevision),
    /// 仅当路径不存在时创建，已有路径必须保持原样。
    CreateNew,
    /// 用户已明确确认覆盖当前内容，不声称执行 revision CAS。
    ExplicitOverwrite,
}

/// 编辑写入命令携带明确策略，适配器不能自行猜测是否允许丢弃并发修改。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteTextRequest {
    pub endpoint: FileEndpoint,
    pub content: String,
    pub strategy: WriteTextStrategy,
}

/// 保存成功后返回新 revision，编辑器据此推进基线并保持 dirty 状态闭环。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteTextResult {
    pub bytes_written: u64,
    pub revision: FileRevision,
}

/// 目录删除策略明确拒绝跟随符号链接，避免递归删除越出用户选定的目录树。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteTargetKind {
    File,
    Directory,
}

/// 文件冲突策略只表达复制，不会因上传/下载删除源文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    Overwrite,
    Skip,
    Rename,
}

/// 上传、下载、同主机复制和跨主机复制共用同一任务契约。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferKind {
    File,
    Directory,
}

/// 队列请求只携带主机引用及路径，不接受密码、私钥正文或 passphrase。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRequest {
    pub source: FileEndpoint,
    pub destination: FileEndpoint,
    pub kind: TransferKind,
    pub conflict_policy: ConflictPolicy,
    /// 覆盖全局连续无进度阈值；重试沿用入队时选定的值。
    pub idle_timeout_seconds: Option<u16>,
}

/// 任务状态是显式生命周期，UI 可以区分等待、恢复、提交、取消和最终结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferStatus {
    Queued,
    Connecting,
    Transferring,
    WaitingForRecovery,
    Committing,
    Cancelling,
    Succeeded,
    /// 冲突策略明确选择 Skip 时形成独立终态，不计为成功传输或失败。
    Skipped,
    /// 完整目录遍历确认目标无需改动时结束，不计入任何目标提交回执。
    Unchanged,
    Failed,
    Cancelled,
}

/// 后端运行阶段只参与当前进程内的传输展示，不进入持久化或外部协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferPhase {
    Preparing,
    Transferring,
    Verifying,
    Committing,
}

/// 失败类别供界面和 MCP 决定恢复动作；原始网络错误只写脱敏日志。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferFailureKind {
    IdleTimeout,
    Connection,
    Permission,
    Conflict,
    InvalidSource,
    Unsupported,
    CommitUnknown,
    Internal,
}

/// 单一传输任务聚合维护状态迁移和可信检查点，不把队列 DTO 当领域对象使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferTask {
    id: TransferId,
    request: TransferRequest,
    status: TransferStatus,
    bytes_transferred: u64,
    total_bytes: Option<u64>,
    retryable: bool,
    resumable: bool,
    failure_kind: Option<TransferFailureKind>,
    safe_error: Option<String>,
    actual_destination: Option<FileEndpoint>,
    automatic_recovery_used: bool,
    retry_successor: Option<TransferId>,
    commit_protocol: TransferCommitProtocol,
}

/// 队列只向 UI/MCP 返回此不可变视图，避免调用方直接修改聚合生命周期。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferSummary {
    pub id: TransferId,
    pub request: TransferRequest,
    pub status: TransferStatus,
    /// 当前运行阶段；终态与等待恢复状态必须为空，确认字节仍由 bytes_transferred 表示。
    pub phase: Option<TransferPhase>,
    pub bytes_transferred: u64,
    pub total_bytes: Option<u64>,
    /// 基于本次新增确认字节和采样间隔计算的吞吐量；恢复前缀不计入。
    pub speed_bytes_per_second: u64,
    /// 最近一次确认字节增长的 Unix epoch 毫秒时间戳。
    pub last_progress_at: Option<u64>,
    /// 应用队列在创建任务时固定的无进度保护秒数；裸领域聚合没有运行策略时为空。
    pub effective_idle_timeout_seconds: Option<u64>,
    pub retryable: bool,
    pub resumable: bool,
    pub failure_kind: Option<TransferFailureKind>,
    pub safe_error: Option<String>,
    pub actual_destination: Option<FileEndpoint>,
    pub automatic_recovery_used: bool,
    pub retry_successor: Option<TransferId>,
    pub commit_summary: TransferCommitSummary,
    pub partial_commit: bool,
    /// 数据结果已终结，但队列仍保留需要显式重试的资源清理 owner。
    pub cleanup_pending: bool,
}

/// 队列状态错误使调用方能将非法迁移和外部操作失败分别呈现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferTransitionError {
    InvalidTransition,
    SameSourceAndDestination,
    ProgressRegressed,
    ProgressExceededTotal,
    RetryNotAllowed,
    RecoveryNotAllowed,
}

impl TransferTask {
    /// 入队时拒绝源目标相同的复制，避免依赖传输后端解释覆盖自身的行为。
    pub fn queued(
        id: TransferId,
        request: TransferRequest,
    ) -> Result<Self, TransferTransitionError> {
        if request.source.same_object(&request.destination) {
            return Err(TransferTransitionError::SameSourceAndDestination);
        }
        Ok(Self {
            id,
            request,
            status: TransferStatus::Queued,
            bytes_transferred: 0,
            total_bytes: None,
            retryable: false,
            resumable: false,
            failure_kind: None,
            safe_error: None,
            actual_destination: None,
            automatic_recovery_used: false,
            retry_successor: None,
            commit_protocol: TransferCommitProtocol::default(),
        })
    }

    /// Connecting 只允许从队列或一次安全恢复等待进入，保持任务阶段可审计。
    pub fn begin_connecting(&mut self) -> Result<(), TransferTransitionError> {
        match self.status {
            TransferStatus::Queued | TransferStatus::WaitingForRecovery => {
                self.status = TransferStatus::Connecting;
                self.failure_kind = None;
                self.safe_error = None;
                Ok(())
            }
            _ => Err(TransferTransitionError::InvalidTransition),
        }
    }

    /// 队列 worker 读取不可变身份时不暴露聚合字段写入口。
    pub fn id(&self) -> &TransferId {
        &self.id
    }

    /// 执行后继任务时复用已校验请求，避免 UI 伪造重试参数。
    pub fn request(&self) -> &TransferRequest {
        &self.request
    }

    /// 队列调度读取状态以决定是否可启动或释放并发槽。
    pub const fn status(&self) -> TransferStatus {
        self.status
    }

    /// 查询确认字节数用于进度投影和断点诊断。
    pub const fn bytes_transferred(&self) -> u64 {
        self.bytes_transferred
    }

    /// 查询文件大小上界；目录传输允许未知总量。
    pub const fn total_bytes(&self) -> Option<u64> {
        self.total_bytes
    }

    /// 仅在用户触发重试时读取此判断，提交结果不明会被领域规则禁止。
    pub const fn retryable(&self) -> bool {
        self.retryable
    }

    /// 续传 UI 只能展示可信服务端断点，不能根据 partial 文件长度自行推测。
    pub const fn resumable(&self) -> bool {
        self.resumable
    }

    /// 读取自动恢复标记以确保 idle timeout 最多重连一次。
    pub const fn automatic_recovery_used(&self) -> bool {
        self.automatic_recovery_used
    }

    /// 后继任务继承稳定目标，rename 策略不会在重试时挑选另一个名字。
    pub fn actual_destination(&self) -> Option<&FileEndpoint> {
        self.actual_destination.as_ref()
    }

    /// 读取 retry 子任务以实现对同一旧任务的幂等响应。
    pub fn retry_successor(&self) -> Option<&TransferId> {
        self.retry_successor.as_ref()
    }

    /// 队列按目标能力检查错误恢复策略，错误文本始终通过只读摘要公开。
    pub fn failure_kind(&self) -> Option<TransferFailureKind> {
        self.failure_kind
    }

    /// 断点恢复可从已确认偏移初始化后继任务，未经校验的 partial 大小不得调用此方法。
    pub fn restore_confirmed_progress(
        &mut self,
        bytes: u64,
        total: Option<u64>,
    ) -> Result<(), TransferTransitionError> {
        if self.status != TransferStatus::Queued || total.is_some_and(|limit| bytes > limit) {
            return Err(TransferTransitionError::InvalidTransition);
        }
        self.bytes_transferred = bytes;
        self.total_bytes = total;
        Ok(())
    }

    /// 传输阶段重置当次速度统计，但累计字节不得回退到已确认检查点之前。
    pub fn begin_transferring(
        &mut self,
        total_bytes: Option<u64>,
    ) -> Result<(), TransferTransitionError> {
        if self.status != TransferStatus::Connecting {
            return Err(TransferTransitionError::InvalidTransition);
        }
        self.status = TransferStatus::Transferring;
        self.total_bytes = total_bytes;
        Ok(())
    }

    /// 一旦后端选定实际目标便固定它，重连或人工重试不能重新选择另一个 rename 名称。
    pub fn select_actual_destination(
        &mut self,
        destination: FileEndpoint,
    ) -> Result<(), TransferTransitionError> {
        if !matches!(
            self.status,
            TransferStatus::Queued | TransferStatus::Connecting | TransferStatus::Transferring
        ) {
            return Err(TransferTransitionError::InvalidTransition);
        }
        if self
            .actual_destination
            .as_ref()
            .is_some_and(|existing| existing != &destination)
        {
            return Err(TransferTransitionError::InvalidTransition);
        }
        self.actual_destination = Some(destination);
        Ok(())
    }

    /// 只接受单调递增的已确认字节数，未确认缓冲区不能成为断点。
    pub fn record_progress(
        &mut self,
        bytes: u64,
        total: Option<u64>,
    ) -> Result<(), TransferTransitionError> {
        if self.status != TransferStatus::Transferring {
            return Err(TransferTransitionError::InvalidTransition);
        }
        if bytes < self.bytes_transferred {
            return Err(TransferTransitionError::ProgressRegressed);
        }
        let effective_total = total.or(self.total_bytes);
        if effective_total.is_some_and(|limit| bytes > limit) {
            return Err(TransferTransitionError::ProgressExceededTotal);
        }
        self.bytes_transferred = bytes;
        self.total_bytes = effective_total;
        Ok(())
    }

    /// 每次目标项原子替换前申请提交许可，项间继续使用同一任务状态。
    pub fn begin_commit(
        &mut self,
        actual_destination: FileEndpoint,
    ) -> Result<(), TransferTransitionError> {
        match self.status {
            TransferStatus::Transferring => {
                self.commit_protocol
                    .begin_item()
                    .map_err(map_commit_protocol_error)?;
                self.status = TransferStatus::Committing;
                self.actual_destination = Some(actual_destination);
                Ok(())
            }
            TransferStatus::Committing
                if self.commit_protocol.between_items()
                    && self.actual_destination.as_ref() == Some(&actual_destination) =>
            {
                self.commit_protocol
                    .begin_item()
                    .map_err(map_commit_protocol_error)
            }
            _ => Err(TransferTransitionError::InvalidTransition),
        }
    }

    /// 在当前项目取得明确回执后累计目标变化，避免把 staging 进度误报为已提交。
    pub fn record_committed_entry(
        &mut self,
        committed_bytes: u64,
    ) -> Result<TransferCommitSummary, TransferTransitionError> {
        if self.status != TransferStatus::Committing {
            return Err(TransferTransitionError::InvalidTransition);
        }
        let summary = self
            .commit_protocol
            .record_confirmed_receipt(committed_bytes)
            .map_err(map_commit_protocol_error)?;
        if self.commit_protocol.cancellation_requested() {
            self.status = TransferStatus::Cancelling;
        }
        Ok(summary)
    }

    /// 目标项收到明确无副作用回执时释放提交许可，不增加已提交项目数或字节数。
    pub fn confirm_commit_item_unchanged(
        &mut self,
    ) -> Result<TransferCommitSummary, TransferTransitionError> {
        if self.status != TransferStatus::Committing {
            return Err(TransferTransitionError::InvalidTransition);
        }
        let summary = self
            .commit_protocol
            .confirm_item_unchanged()
            .map_err(map_commit_protocol_error)?;
        if self.commit_protocol.cancellation_requested() {
            self.status = TransferStatus::Cancelling;
        } else if summary.is_empty() {
            self.status = TransferStatus::Transferring;
        }
        Ok(summary)
    }

    /// 全部目标项收到回执后进入只等待最终结果的保护区，不启动额外目标提交。
    pub fn finish_commit(&mut self) -> Result<TransferCommitSummary, TransferTransitionError> {
        if !matches!(
            self.status,
            TransferStatus::Committing | TransferStatus::Cancelling
        ) {
            return Err(TransferTransitionError::InvalidTransition);
        }
        self.commit_protocol
            .finish_receipts()
            .map_err(map_commit_protocol_error)
    }

    /// 查询当前目标项是否还没有回执，队列据此区分 Unknown 与已知部分结果。
    pub const fn commit_item_in_progress(&self) -> bool {
        self.commit_protocol.item_in_progress()
    }

    /// 查询已收到上一项目标回执的可取消窗口。
    pub const fn commit_between_items(&self) -> bool {
        self.commit_protocol.between_items()
    }

    /// 查询后端是否需要在当前项收到回执后停止后续目标修改。
    pub const fn commit_cancel_requested(&self) -> bool {
        self.commit_protocol.cancellation_requested()
    }

    /// 撤销尚未送达后端的提交许可；仅在可靠控制通道证明发送失败时调用。
    pub fn abort_unacknowledged_commit_item(&mut self) -> Result<(), TransferTransitionError> {
        let summary = self
            .commit_protocol
            .abort_unacknowledged_item()
            .map_err(map_commit_protocol_error)?;
        if self.commit_protocol.cancellation_requested() {
            self.status = TransferStatus::Cancelling;
        } else if summary.is_empty() {
            self.status = TransferStatus::Transferring;
        }
        Ok(())
    }

    /// 兼容旧的单项领域入口，并将其显式收敛为 begin、receipt 和最终屏障。
    pub fn confirm_commit(
        &mut self,
        actual_destination: FileEndpoint,
    ) -> Result<(), TransferTransitionError> {
        self.begin_commit(actual_destination)?;
        self.record_committed_entry(0)?;
        self.finish_commit()?;
        Ok(())
    }

    /// 只有所有目标项的明确回执越过最终屏障后才能标记成功。
    pub fn complete(&mut self) -> Result<(), TransferTransitionError> {
        if !matches!(
            self.status,
            TransferStatus::Committing | TransferStatus::Cancelling
        ) || !self.commit_protocol.is_finalized()
        {
            return Err(TransferTransitionError::InvalidTransition);
        }
        self.status = TransferStatus::Succeeded;
        self.retryable = false;
        self.resumable = false;
        Ok(())
    }

    /// 完整检查后确认没有目标变化时结束；拒绝取消中、已有回执或进行中的提交。
    pub fn finish_no_changes(&mut self) -> Result<TransferCommitSummary, TransferTransitionError> {
        if self.status != TransferStatus::Transferring
            || self.request.kind != TransferKind::Directory
            || !self.commit_protocol.summary().is_empty()
            || self.commit_protocol.cancellation_requested()
        {
            return Err(TransferTransitionError::InvalidTransition);
        }
        let summary = self
            .commit_protocol
            .finish_no_changes()
            .map_err(map_commit_protocol_error)?;
        self.status = TransferStatus::Unchanged;
        self.retryable = false;
        self.resumable = false;
        Ok(summary)
    }

    /// 仅当冲突策略在开始写入前跳过目标时结束任务，防止部分复制被伪装为跳过。
    pub fn skip(&mut self) -> Result<(), TransferTransitionError> {
        if self.status != TransferStatus::Transferring || self.bytes_transferred != 0 {
            return Err(TransferTransitionError::InvalidTransition);
        }
        self.status = TransferStatus::Skipped;
        self.retryable = false;
        self.resumable = false;
        Ok(())
    }

    /// 当前项目提交时只登记取消意图，项间立即取消，避免打断已获许可的原子替换。
    pub fn request_cancel(&mut self) -> bool {
        match self.status {
            TransferStatus::Queued => {
                self.status = TransferStatus::Cancelled;
                self.retryable = false;
                true
            }
            TransferStatus::Connecting
            | TransferStatus::Transferring
            | TransferStatus::WaitingForRecovery => {
                self.status = TransferStatus::Cancelling;
                true
            }
            TransferStatus::Committing => {
                if !self.commit_protocol.item_in_progress() && !self.commit_protocol.between_items()
                {
                    return false;
                }
                self.commit_protocol.request_cancel();
                if self.commit_protocol.between_items() {
                    self.status = TransferStatus::Cancelling;
                }
                true
            }
            TransferStatus::Cancelling
            | TransferStatus::Succeeded
            | TransferStatus::Skipped
            | TransferStatus::Unchanged
            | TransferStatus::Failed
            | TransferStatus::Cancelled => false,
        }
    }

    /// 只有 worker 收到取消并释放本任务资源后才可确认取消终态。
    pub fn confirm_cancelled(&mut self, resumable: bool) -> Result<(), TransferTransitionError> {
        if self.status != TransferStatus::Cancelling
            || self.commit_protocol.item_in_progress()
            || self.commit_protocol.is_outcome_unknown()
        {
            return Err(TransferTransitionError::InvalidTransition);
        }
        self.status = TransferStatus::Cancelled;
        self.retryable = self.commit_protocol.summary().is_empty();
        self.resumable = resumable && self.retryable;
        Ok(())
    }

    /// 仅允许一次带可信断点的 idle 自动恢复，避免循环重连和无休止占用连接槽。
    pub fn begin_automatic_recovery(
        &mut self,
        resumable: bool,
    ) -> Result<(), TransferTransitionError> {
        let has_resumed_bytes =
            self.status == TransferStatus::Connecting && self.bytes_transferred > 0;
        if (self.status != TransferStatus::Transferring && !has_resumed_bytes)
            || !resumable
            || self.automatic_recovery_used
        {
            return Err(TransferTransitionError::RecoveryNotAllowed);
        }
        self.status = TransferStatus::WaitingForRecovery;
        self.resumable = true;
        self.automatic_recovery_used = true;
        Ok(())
    }

    /// 失败时 commitUnknown 永远禁止自动 retry；其它失败由后端可信断点能力决定。
    pub fn fail(
        &mut self,
        kind: TransferFailureKind,
        safe_error: String,
        retryable: bool,
        resumable: bool,
    ) -> Result<(), TransferTransitionError> {
        if matches!(
            self.status,
            TransferStatus::Succeeded
                | TransferStatus::Skipped
                | TransferStatus::Unchanged
                | TransferStatus::Failed
                | TransferStatus::Cancelled
        ) {
            return Err(TransferTransitionError::InvalidTransition);
        }
        if kind == TransferFailureKind::CommitUnknown
            && self.commit_protocol.mark_outcome_unknown().is_err()
        {
            return Err(TransferTransitionError::InvalidTransition);
        }
        let no_committed_entries = self.commit_protocol.summary().is_empty();
        self.status = TransferStatus::Failed;
        self.failure_kind = Some(kind);
        self.safe_error = Some(safe_error);
        self.resumable =
            resumable && no_committed_entries && kind != TransferFailureKind::CommitUnknown;
        self.retryable =
            retryable && no_committed_entries && kind != TransferFailureKind::CommitUnknown;
        Ok(())
    }

    /// 保存后继 ID 使重复 retry 请求能返回同一任务，而不会复制 partial 或并发提交。
    pub fn set_retry_successor(
        &mut self,
        successor: TransferId,
    ) -> Result<(), TransferTransitionError> {
        if let Some(existing) = &self.retry_successor {
            return if existing == &successor {
                Ok(())
            } else {
                Err(TransferTransitionError::RetryNotAllowed)
            };
        }
        if !self.retryable
            || !matches!(
                self.status,
                TransferStatus::Failed | TransferStatus::Cancelled
            )
        {
            return Err(TransferTransitionError::RetryNotAllowed);
        }
        self.retry_successor = Some(successor);
        Ok(())
    }

    /// 为消费端创建只读投影，领域状态迁移与展示字段保持单一来源。
    ///
    /// @author kongweiguang
    pub fn summary(&self) -> TransferSummary {
        let commit_summary = self.commit_protocol.summary();
        let partial_commit = !commit_summary.is_empty()
            && !self.commit_protocol.is_finalized()
            && !matches!(
                self.status,
                TransferStatus::Succeeded | TransferStatus::Skipped | TransferStatus::Unchanged
            );
        TransferSummary {
            id: self.id.clone(),
            request: self.request.clone(),
            status: self.status,
            phase: None,
            bytes_transferred: self.bytes_transferred,
            total_bytes: self.total_bytes,
            speed_bytes_per_second: 0,
            last_progress_at: None,
            effective_idle_timeout_seconds: self.request.idle_timeout_seconds.map(u64::from),
            retryable: self.retryable,
            resumable: self.resumable,
            failure_kind: self.failure_kind,
            safe_error: self.safe_error.clone(),
            actual_destination: self.actual_destination.clone(),
            automatic_recovery_used: self.automatic_recovery_used,
            retry_successor: self.retry_successor.clone(),
            commit_summary,
            partial_commit,
            cleanup_pending: false,
        }
    }
}

/// 把内部提交协议状态错误折叠为领域迁移错误，避免调用方依赖协议实现细节。
fn map_commit_protocol_error(_error: TransferCommitTransitionError) -> TransferTransitionError {
    TransferTransitionError::InvalidTransition
}

/// 书签作用域隔离本机、SSH 主机和容器，避免不同远端的同名路径串用。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum BookmarkTarget {
    Local,
    #[serde(rename = "ssh")]
    Remote {
        host_id: HostId,
    },
    #[serde(rename = "dockerContainer")]
    Container {
        /// None 表示本机 CLI 的收藏作用域，避免构造虚假的持久主机身份。
        #[serde(skip_serializing_if = "Option::is_none")]
        host_id: Option<HostId>,
        runtime: ContainerRuntime,
        container_id: String,
    },
}

/// 路径收藏是用户数据，不因路径暂时不存在而随目录刷新被删除。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileBookmark {
    pub created_at_unix_ms: i64,
    pub target: BookmarkTarget,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}
