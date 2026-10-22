//! 文件型配置文档的稳定路径、revision 与启动恢复状态。
//!
//! @author kongweiguang

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// 当前文件型配置支持的业务文档；枚举白名单禁止仓储把任意路径写入配置根。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum ConfigDocumentPath {
    Settings,
    Profile(String),
    HostGroups,
    Host(String),
    Snippet(String),
    Workflow(String),
}

/// 文件名受限于当前磁盘布局；通用领域 ID 仍保持 opaque，不在此类型中改写。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigPathError {
    EmptyId,
    InvalidId,
    ReservedHostId,
}

/// SHA-256 revision 用不透明文本传递，避免 UI 对磁盘并发策略作自己的猜测。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConfigRevision(String);

/// 已读配置快照；revision 绑定原始字节而非重新序列化后的 TOML。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigDocument {
    pub path: ConfigDocumentPath,
    pub revision: ConfigRevision,
    pub source: String,
}

/// 启动发现可修复配置异常时的进程级访问模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConfigurationMode {
    ReadWrite,
    ReadOnlyRecovery,
}

/// 损坏原因仅暴露分类和位置，不携带 TOML 原文、字段值或凭据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConfigurationIssueKind {
    MalformedToml,
    UnsupportedSchema,
    Unreadable,
}

/// 可安全展示的相对配置诊断；路径由根目录之下已枚举文件构成。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurationIssue {
    pub relative_path: String,
    pub kind: ConfigurationIssueKind,
    pub line: Option<usize>,
    pub column: Option<usize>,
}

/// 初始化状态需在 UI 禁用写入之外由仓储强制执行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurationRecoveryState {
    pub mode: ConfigurationMode,
    pub issues: Vec<ConfigurationIssue>,
}

/// 当前 schema 1 的本地启动 profile；未知字段仍由原始 TOML 文本仓储保留。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalProfile {
    pub id: String,
    pub name: String,
    pub shell: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub is_default: bool,
    pub sidebar_group_id: Option<String>,
    pub sort_order: i64,
    pub created_at: String,
    pub updated_at: String,
}

impl ConfigDocumentPath {
    /// 只允许当前文件化配置的已知目录和扩展名，避免相对路径逃逸。
    pub fn relative_path(&self) -> Result<PathBuf, ConfigPathError> {
        match self {
            Self::Settings => Ok(PathBuf::from("settings.toml")),
            Self::Profile(id) => {
                Ok(PathBuf::from("profiles").join(format!("{}.toml", file_id(id, false)?)))
            }
            Self::HostGroups => Ok(PathBuf::from("hosts").join("groups.toml")),
            Self::Host(id) => {
                Ok(PathBuf::from("hosts").join(format!("{}.toml", file_id(id, true)?)))
            }
            Self::Snippet(id) => {
                Ok(PathBuf::from("snippets").join(format!("{}.toml", file_id(id, false)?)))
            }
            Self::Workflow(id) => {
                Ok(PathBuf::from("workflows").join(format!("{}.toml", file_id(id, false)?)))
            }
        }
    }
}

/// 按旧配置仓储的文件名字符集校验 ID；拒绝而不静默 trim，避免碰撞或错读文件。
fn file_id(value: &str, host_id: bool) -> Result<&str, ConfigPathError> {
    if value.is_empty() {
        return Err(ConfigPathError::EmptyId);
    }
    if value == "."
        || value == ".."
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return Err(ConfigPathError::InvalidId);
    }
    if host_id && value == "groups" {
        return Err(ConfigPathError::ReservedHostId);
    }
    Ok(value)
}

impl ConfigRevision {
    /// 从配置仓储生成 revision；调用方只能持有它并在下一次写入时原样提交。
    pub fn from_sha256(hex_digest: String) -> Self {
        Self(hex_digest)
    }

    /// 返回不透明 revision 文本，用于 compare-and-swap 保存。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
