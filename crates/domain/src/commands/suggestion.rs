//! 命令建议的查询和展示契约。
//!
//! @author kongweiguang

use serde::{Deserialize, Serialize};

use super::history::CommandHistoryTarget;

/// 命令建议的来源 provider。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SuggestionProviderKind {
    History,
    RemotePath,
    RemoteCommand,
    Git,
    Spec,
    Snippet,
}

/// 接受建议后需要由界面完成的动作。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandSuggestionActivation {
    #[default]
    Insert,
    OpenSnippetPanel,
}

/// 候选敏感度决定可进入的展示位置。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandSuggestionSensitivity {
    #[default]
    Normal,
    Sensitive,
    Dangerous,
}

/// UI 可将普通候选用于 inline 或 menu，危险候选只允许进入 menu。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SuggestionPresentation {
    Inline,
    Menu,
}

/// 建议替换范围按 Unicode 标量偏移表达，避免中日韩文本偏移错位。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandSuggestionReplacementRange {
    pub start: usize,
    pub end: usize,
}

/// 可展示的命令建议。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandSuggestionCandidate {
    pub id: String,
    pub provider: SuggestionProviderKind,
    pub activation: CommandSuggestionActivation,
    pub display_text: String,
    pub replacement_text: String,
    pub replacement_range: CommandSuggestionReplacementRange,
    pub suffix: String,
    pub score: f64,
    pub sensitivity: CommandSuggestionSensitivity,
    pub description: Option<String>,
    pub source_explanation: Option<String>,
    pub merged_source_explanations: Vec<String>,
    pub source_id: Option<String>,
    pub allowed_presentations: Vec<SuggestionPresentation>,
    pub accept_boundaries: Vec<usize>,
    pub context_key: Option<String>,
}

impl CommandSuggestionCandidate {
    /// 避免敏感候选被展示，危险命令只允许用户主动打开的菜单呈现。
    pub fn allowed_for(sensitivity: CommandSuggestionSensitivity) -> Vec<SuggestionPresentation> {
        match sensitivity {
            CommandSuggestionSensitivity::Normal => {
                vec![SuggestionPresentation::Inline, SuggestionPresentation::Menu]
            }
            CommandSuggestionSensitivity::Sensitive => Vec::new(),
            CommandSuggestionSensitivity::Dangerous => vec![SuggestionPresentation::Menu],
        }
    }
}

/// 查询模式区分低干扰 inline 与主动菜单。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SuggestionQueryMode {
    #[default]
    Inline,
    Menu,
}

/// 建议查询上下文；context key 仅用于缓存/过期响应关联，不是权限依据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandSuggestionRequest {
    pub input: String,
    pub cursor: usize,
    pub target: CommandHistoryTarget,
    pub session_id: Option<String>,
    pub pane_id: Option<String>,
    pub profile_id: Option<String>,
    pub remote_host_id: Option<String>,
    pub cwd: Option<String>,
    pub shell: Option<String>,
    pub providers: Option<Vec<SuggestionProviderKind>>,
    pub limit: Option<usize>,
    pub mode: SuggestionQueryMode,
    pub generation: Option<u64>,
    pub context_key: Option<String>,
}

/// 用户反馈用于本地排序信号，不保存秘密或完整命令上下文以外的凭据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandSuggestionFeedbackAction {
    Accepted,
    Dismissed,
}

/// 用户反馈写入请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandSuggestionFeedbackRequest {
    pub action: CommandSuggestionFeedbackAction,
    pub provider: SuggestionProviderKind,
    pub replacement_text: String,
    pub input: String,
    pub target: CommandHistoryTarget,
    pub source_id: Option<String>,
    pub session_id: Option<String>,
    pub pane_id: Option<String>,
    pub profile_id: Option<String>,
    pub remote_host_id: Option<String>,
    pub cwd: Option<String>,
    pub shell: Option<String>,
}
