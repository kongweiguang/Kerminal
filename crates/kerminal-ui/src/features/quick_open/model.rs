//! Quick Open 的稳定目标身份与可搜索工作区投影。
//!
//! @author kongweiguang

use kerminal_application::{
    sessions::{TerminalCommandContext, UiSessionBinding},
    workspace::{WorkspaceSessionSnapshot, WorkspaceSessionTab},
};
use kerminal_domain::identity::{AgentSessionId, HostId, PaneId, TabId};

use crate::keybindings::KeybindingAction;

pub(super) const MAX_QUICK_OPEN_ENTRIES: usize = 200;

/// 两种模式共用检索视图，但模式只影响首批候选，不改变目标选择语义。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuickOpenMode {
    QuickOpen,
    CommandPalette,
}

/// Root 分别提供工作区快照、干净 prompt 上下文和独立捕获的终端动作 binding。
#[derive(Clone, Debug, PartialEq)]
pub struct QuickOpenOpenContext {
    pub workspace: Option<WorkspaceSessionSnapshot>,
    pub terminal: Option<QuickOpenTerminalTarget>,
    /// 搜索等不写 shell 的动作不依赖 prompt cleanliness，但仍绑定打开时的 owner/generation。
    pub terminal_binding: Option<UiSessionBinding>,
}

/// 历史命令候选携带打开面板时的实际 UI binding 与 CAS 版本，不能重连后重新查找目标。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuickOpenTerminalTarget {
    pub binding: UiSessionBinding,
    pub context: TerminalCommandContext,
}

/// 候选保存稳定 ID 或完整打开时上下文，确认时绝不按渲染行号推测资源。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QuickOpenSelection {
    Action(KeybindingAction),
    Host(HostId),
    Tab(TabId),
    Pane {
        tab_id: TabId,
        pane_id: PaneId,
    },
    WorkspaceFile(TabId),
    CurrentDirectory {
        tab_id: TabId,
        pane_id: PaneId,
        path: String,
    },
    HistoryCommand(String),
    Snippet(String),
    Workflow(String),
    Agent(AgentSessionId),
}

/// 统一只读菜单事件；关闭或选择时带回启动窗口时的恢复焦点身份。
#[derive(Clone, Debug, PartialEq)]
pub enum QuickOpenEvent {
    Dismissed {
        origin_binding: Option<UiSessionBinding>,
    },
    Selected {
        selection: QuickOpenSelection,
        origin: Option<Box<QuickOpenTerminalTarget>>,
        /// 动作确认时携带 Quick Open 打开时捕获的目标，Root 必须重新校验而不能查找替代项。
        terminal_binding: Option<UiSessionBinding>,
    },
}

/// Kit Command 的平面行携带原始 payload，避免过滤和异步补数改变目标身份。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuickOpenEntry {
    pub(super) label: String,
    pub(super) detail: String,
    pub(super) keywords: Vec<String>,
    pub(super) selection: Option<QuickOpenSelection>,
    pub(super) disabled: bool,
}

/// Quick Open 页脚区分可用性说明和真实来源故障，避免错误被弱化成普通提示。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum QuickOpenSourceStatus {
    Info(String),
    Error(String),
}

impl QuickOpenEntry {
    /// 只有普通单行文本可作为待编辑命令插入；历史内容永远不会携带执行换行。
    pub(super) fn history_command_is_insertable(command: &str) -> bool {
        !command.trim().is_empty()
            && command.chars().count() <= 8_000
            && command.len() <= 32_000
            && !command.chars().any(char::is_control)
    }

    /// 关键字匹配仅用于已载入的有界候选，远端和持久化读取仍由异步应用用例完成。
    pub(super) fn matches_query(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || self.label.to_lowercase().contains(&query)
            || self.detail.to_lowercase().contains(&query)
            || self
                .keywords
                .iter()
                .any(|keyword| keyword.to_lowercase().contains(&query))
    }
}

/// 从当前 v3 工作区读取有效 tab、pane、打开文件和当前 cwd，不伪称为持久化路径 MRU。
/// machine ID 只留作检索关键词；空白 cwd 不生成空目录行或多余分隔符。
///
/// @author kongweiguang
pub(super) fn workspace_entries(
    snapshot: &WorkspaceSessionSnapshot,
    query: &str,
) -> Vec<QuickOpenEntry> {
    let mut entries = Vec::new();
    for tab in &snapshot.terminal_tabs {
        match tab {
            WorkspaceSessionTab::Terminal {
                id,
                title,
                machine_id,
                layout,
                ..
            } => {
                push_matching(
                    &mut entries,
                    QuickOpenEntry {
                        label: title.clone(),
                        detail: text("终端标签", "Terminal tab"),
                        keywords: vec![id.to_string(), machine_id.clone()],
                        selection: Some(QuickOpenSelection::Tab(id.clone())),
                        disabled: false,
                    },
                    query,
                );
                for pane_id in layout.pane_ids() {
                    let Some(pane) = snapshot
                        .terminal_panes
                        .iter()
                        .find(|pane| pane.id == pane_id)
                    else {
                        continue;
                    };
                    let cwd = pane
                        .current_cwd
                        .as_deref()
                        .filter(|cwd| !cwd.trim().is_empty())
                        .or_else(|| pane.cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()));
                    let detail = cwd.map_or_else(
                        || text("终端分屏", "Terminal pane"),
                        |cwd| format!("{} · {cwd}", text("终端分屏", "Terminal pane")),
                    );
                    push_matching(
                        &mut entries,
                        QuickOpenEntry {
                            label: pane.title.clone(),
                            detail,
                            keywords: vec![pane.id.to_string(), pane.machine_id.clone()],
                            selection: Some(QuickOpenSelection::Pane {
                                tab_id: id.clone(),
                                pane_id: pane.id.clone(),
                            }),
                            disabled: false,
                        },
                        query,
                    );
                    if let Some(path) = cwd {
                        push_matching(
                            &mut entries,
                            QuickOpenEntry {
                                label: path.to_owned(),
                                detail: text("当前终端目录", "Current terminal directory"),
                                keywords: vec![pane.title.clone(), pane.id.to_string()],
                                selection: Some(QuickOpenSelection::CurrentDirectory {
                                    tab_id: id.clone(),
                                    pane_id: pane.id.clone(),
                                    path: path.to_owned(),
                                }),
                                disabled: false,
                            },
                            query,
                        );
                    }
                }
            }
            WorkspaceSessionTab::SftpTransfer {
                id,
                title,
                machine_id,
                ..
            } => push_matching(
                &mut entries,
                QuickOpenEntry {
                    label: title.clone(),
                    detail: text("SFTP 标签", "SFTP tab"),
                    keywords: vec![id.to_string(), machine_id.clone()],
                    selection: Some(QuickOpenSelection::Tab(id.clone())),
                    disabled: false,
                },
                query,
            ),
            WorkspaceSessionTab::WorkspaceFile {
                id, title, path, ..
            } => push_matching(
                &mut entries,
                QuickOpenEntry {
                    label: title.clone(),
                    detail: path.clone(),
                    keywords: vec![id.to_string(), path.clone()],
                    selection: Some(QuickOpenSelection::WorkspaceFile(id.clone())),
                    disabled: false,
                },
                query,
            ),
            WorkspaceSessionTab::Unsupported { .. } => {}
        }
        if entries.len() >= MAX_QUICK_OPEN_ENTRIES {
            break;
        }
    }
    entries
}

/// 本地投影和后端结果使用同一硬上限，避免大 workspace 把菜单布局和搜索成本放大。
pub(super) fn push_matching(entries: &mut Vec<QuickOpenEntry>, entry: QuickOpenEntry, query: &str) {
    if entries.len() < MAX_QUICK_OPEN_ENTRIES && entry.matches_query(query) {
        entries.push(entry);
    }
}

/// 避免 UI 源码中重复生成两种语言的动作标签。
pub(super) fn text(chinese: &'static str, english: &'static str) -> String {
    crate::locale::text(chinese, english).to_string()
}
