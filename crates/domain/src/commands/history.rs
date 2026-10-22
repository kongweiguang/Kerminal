//! 命令历史的记录规则与查询条件。
//!
//! @author kongweiguang

use serde::{Deserialize, Serialize};

use crate::identity::{HostId, PaneId, SessionId, TabId};

const MAX_COMMAND_CHARS: usize = 4_000;
const MAX_ID_CHARS: usize = 160;
const MAX_PATH_CHARS: usize = 1_000;
const MAX_SHELL_CHARS: usize = 400;

/// 命令是从终端、片段、工作流还是内部工具进入执行链。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandHistorySource {
    #[default]
    User,
    Snippet,
    Workflow,
    Broadcast,
    Tool,
}

/// 执行命令的目标协议。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandHistoryTarget {
    #[default]
    Local,
    Ssh,
    Telnet,
    Serial,
    #[serde(rename = "dockerContainer")]
    Container,
}

/// 一条已脱敏的命令历史记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandHistoryEntry {
    pub id: String,
    pub command: String,
    pub source: CommandHistorySource,
    pub target: CommandHistoryTarget,
    pub session_id: Option<SessionId>,
    pub pane_id: Option<PaneId>,
    pub tab_id: Option<TabId>,
    pub profile_id: Option<String>,
    pub remote_host_id: Option<HostId>,
    pub cwd: Option<String>,
    pub shell: Option<String>,
    pub created_at: String,
}

/// 查询历史时可组合使用的筛选条件。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandHistoryListRequest {
    pub query: Option<String>,
    pub source: Option<CommandHistorySource>,
    pub target: Option<CommandHistoryTarget>,
    pub pane_id: Option<PaneId>,
    pub remote_host_id: Option<HostId>,
    pub session_id: Option<SessionId>,
    pub limit: Option<usize>,
}

/// 记录请求保留 `record=false`，因为用户和会话偏好需要阻止持久化。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandHistoryRecordRequest {
    pub command: String,
    pub source: CommandHistorySource,
    pub target: CommandHistoryTarget,
    pub record: Option<bool>,
    pub session_id: Option<SessionId>,
    pub pane_id: Option<PaneId>,
    pub tab_id: Option<TabId>,
    pub profile_id: Option<String>,
    pub remote_host_id: Option<HostId>,
    pub cwd: Option<String>,
    pub shell: Option<String>,
}

/// 记录操作区分已写入与按策略跳过，调用方不能把跳过当作持久化成功。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandHistoryRecordResult {
    pub recorded: bool,
    pub entry: Option<CommandHistoryEntry>,
    pub skip_reason: Option<String>,
}

/// 经规范化且符合记录策略的历史写入值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedCommandHistory {
    pub command: String,
}

/// 历史字段超限或空命令时拒绝写入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandHistoryValidationError {
    EmptyCommand,
    CommandTooLong,
    ContextTooLong,
}

impl NormalizedCommandHistory {
    /// CRLF 与 CR 归一为 LF，使历史搜索不受终端平台换行影响。
    pub fn new(command: &str) -> Result<Self, CommandHistoryValidationError> {
        let command = command.replace("\r\n", "\n").replace('\r', "\n");
        let command = command.trim().to_owned();
        if command.is_empty() {
            return Err(CommandHistoryValidationError::EmptyCommand);
        }
        if command.chars().count() > MAX_COMMAND_CHARS {
            return Err(CommandHistoryValidationError::CommandTooLong);
        }
        Ok(Self { command })
    }

    /// 在任何存储调用前跳过敏感凭据命令，避免数据库成为秘密副本。
    pub fn sensitive_skip_reason(&self) -> Option<&'static str> {
        let lower = self.command.to_ascii_lowercase();
        let markers = [
            "password",
            "passwd",
            "api_key",
            "apikey",
            "access_token",
            "auth_token",
            "secret",
            "private_key",
            "authorization:",
            "bearer ",
            "-----begin",
            "ssh-rsa ",
            "id_rsa",
            "id_ed25519",
        ];
        markers
            .iter()
            .any(|marker| lower.contains(marker))
            .then_some("命令包含疑似凭据内容，已跳过历史记录")
    }
}

/// 检查可选上下文字段长度，防止损坏或异常请求撑大存储记录。
pub fn validate_history_context(
    cwd: Option<&str>,
    shell: Option<&str>,
    ids: &[Option<&str>],
) -> Result<(), CommandHistoryValidationError> {
    if cwd.is_some_and(|value| value.chars().count() > MAX_PATH_CHARS)
        || shell.is_some_and(|value| value.chars().count() > MAX_SHELL_CHARS)
        || ids
            .iter()
            .flatten()
            .any(|value| value.chars().count() > MAX_ID_CHARS)
    {
        return Err(CommandHistoryValidationError::ContextTooLong);
    }
    Ok(())
}
