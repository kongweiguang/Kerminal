//! 终端原始输出日志的用例端口与脱敏状态。
//!
//! @author kongweiguang

use std::{
    sync::{Arc, OnceLock},
    time::Instant,
};

use async_trait::async_trait;
use kerminal_domain::identity::SessionId;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::{AppResult, sessions::SessionService};

/// 用一套旧版兼容规则处理 MCP 快照和文件日志，避免两条输出路径出现脱敏差异。
pub fn redact_terminal_text(input: &str) -> (Zeroizing<String>, bool) {
    let assigned = Zeroizing::new(
        secret_assignment_regex()
            .replace_all(input, |captures: &regex::Captures<'_>| {
                format!("{}=[已脱敏]", &captures[1])
            })
            .into_owned(),
    );
    let bearer = Zeroizing::new(
        bearer_regex()
            .replace_all(&assigned, "${1}[已脱敏]")
            .into_owned(),
    );
    let redacted = Zeroizing::new(
        api_key_regex()
            .replace_all(&bearer, |captures: &regex::Captures<'_>| {
                format!("{}[已脱敏:api-key]{}", &captures[1], &captures[3])
            })
            .into_owned(),
    );
    let changed = redacted.as_str() != input;
    (redacted, changed)
}

/// 缓存旧版凭据赋值模式，保证共享脱敏热路径不重复编译固定正则。
fn secret_assignment_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r#"(?i)\b(api[_-]?key|token|password|passwd|secret)\b\s*[:=]\s*["']?[^\s"']+"#)
            .expect("legacy terminal credential assignment pattern is valid")
    })
}

/// 缓存旧版 Bearer token 模式，保持 MCP 与日志的兼容行为完全一致。
fn bearer_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?i)\b(bearer\s+)[a-z0-9._~+/\-=]{10,}")
            .expect("legacy terminal bearer pattern is valid")
    })
}

/// 缓存旧版 sk- key 模式并保留分隔符，避免相邻终端文本被意外改写。
fn api_key_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?i)(^|\\[rnt]|[^A-Za-z0-9_-])(sk-[A-Za-z0-9_-]{10,})([^A-Za-z0-9_-]|$)")
            .expect("legacy terminal API-key pattern is valid")
    })
}

/// 创建日志时使用会话自身稳定元数据，不把旧 scrollback 当成新输出回放。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionLogStartRequest {
    pub session_id: SessionId,
    pub shell: String,
    pub cwd: Option<String>,
}

/// 对齐 legacy terminal.log 状态字段，并显式暴露写入失败造成的截断。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionLogState {
    pub active: bool,
    pub path: Option<String>,
    pub started_at: Option<String>,
    pub bytes_written: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_missing_sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<SessionLogFailureReason>,
}

/// 记录管道失败原因只使用稳定枚举，不把 OS 错误、文件路径或输出正文带入 MCP。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionLogFailureReason {
    QueueFull,
    SequenceGap,
    WriterIo,
    WriterStopped,
    Shutdown,
    RedactionSafetyLimit,
}

/// PTY reader 在不等待磁盘的情况下获知某次原始输出是否已进入受控队列。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputRecordResult {
    Ignored,
    Queued,
    Failed {
        first_missing_sequence: u64,
        reason: SessionLogFailureReason,
    },
}

/// 文件日志持久化端口；业务调用始终显式给出 SessionId。
#[async_trait]
pub trait SessionLogRepository: Send + Sync + 'static {
    /// 幂等启动当前 session 的日志；重复 start 返回已有状态。
    async fn start(
        &self,
        cancellation: CancellationToken,
        request: SessionLogStartRequest,
    ) -> AppResult<SessionLogState>;

    /// 排空有序输出队列、刷新文件并结束录制。
    async fn stop(
        &self,
        cancellation: CancellationToken,
        session_id: SessionId,
    ) -> AppResult<SessionLogState>;

    /// 只查询状态；未启用日志时返回与旧版一致的 inactive 状态。
    async fn state(
        &self,
        cancellation: CancellationToken,
        session_id: SessionId,
    ) -> AppResult<SessionLogState>;
}

/// 原始 PTY reader 的轻量同步接口；实现必须有界、nonblocking，不能反压终端消费。
#[async_trait]
pub trait SessionOutputRecorder: Send + Sync + 'static {
    /// 按 reader sequence 排队刚读取的原始字节，不从 UI 或终端 snapshot 重建数据。
    fn try_record_output(
        &self,
        session_id: &SessionId,
        sequence: u64,
        bytes: &[u8],
    ) -> OutputRecordResult;

    /// 在 session close 前排空并 join writer；超时由调用方传入绝对 deadline。
    async fn finish_session(
        &self,
        session_id: &SessionId,
        deadline: Instant,
    ) -> AppResult<SessionLogState>;

    /// 应用关闭时停止全部 writer，且不让超出 deadline 的关闭挂住桌面退出。
    async fn shutdown_until(&self, deadline: Instant) -> AppResult<()>;
}

/// 提供 MCP 与 UI 共用的日志 start/stop/state 用例，不暴露基础设施路径拼接。
#[derive(Clone)]
pub struct SessionLogService {
    repository: Arc<dyn SessionLogRepository>,
}

impl SessionLogState {
    /// 构造旧契约中的未启用状态，保证 stop/state 不会伪报活动文件。
    pub const fn inactive() -> Self {
        Self {
            active: false,
            path: None,
            started_at: None,
            bytes_written: 0,
            truncated: false,
            first_missing_sequence: None,
            failure_reason: None,
        }
    }
}

/// false 值保持旧 MCP 状态对象的四字段形状，只在实际发生截断时公开诊断扩展字段。
fn is_false(value: &bool) -> bool {
    !value
}

impl SessionLogService {
    /// 注入实际会话目录适配器，避免用例直接创建文件或推测用户日志根路径。
    pub fn new(repository: Arc<dyn SessionLogRepository>) -> Self {
        Self { repository }
    }

    /// 启动会话日志；调用层需先确认该 SessionId 属于当前明确操作范围。
    pub async fn start(
        &self,
        cancellation: CancellationToken,
        request: SessionLogStartRequest,
    ) -> AppResult<SessionLogState> {
        self.repository.start(cancellation, request).await
    }

    /// 停止后要求 adapter flush 已排队的原始输出，再返回最终状态。
    pub async fn stop(
        &self,
        cancellation: CancellationToken,
        session_id: SessionId,
    ) -> AppResult<SessionLogState> {
        self.repository.stop(cancellation, session_id).await
    }

    /// 查询状态本身不创建日志文件，也不读取或输出滚动缓冲数据。
    pub async fn state(
        &self,
        cancellation: CancellationToken,
        session_id: SessionId,
    ) -> AppResult<SessionLogState> {
        self.repository.state(cancellation, session_id).await
    }

    /// 用共享 SessionService 检查日志目标仍处于 runtime registry。
    pub async fn ensure_session_exists(
        sessions: &dyn SessionService,
        session_id: &SessionId,
    ) -> AppResult<()> {
        sessions
            .list(Default::default())
            .await?
            .into_iter()
            .find(|session| &session.session_id == session_id)
            .map(|_| ())
            .ok_or_else(|| crate::AppError::not_found("终端会话不存在"))
    }
}
