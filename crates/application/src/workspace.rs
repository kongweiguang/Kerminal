//! 原生界面与持久化适配器共用的工作区布局用例。
//!
//! @author kongweiguang

mod session_document;

pub use session_document::{
    WorkspaceContainerRuntime, WorkspaceFileAccess, WorkspaceFileSource,
    WorkspaceLocalMachineScope, WorkspaceMachineStatus, WorkspacePaneMode,
    WorkspaceSessionDocument, WorkspaceSessionPane, WorkspaceSessionSnapshot, WorkspaceSessionTab,
    WorkspaceTabGroup, WorkspaceTabGroupColor, WorkspaceTabGroupId, WorkspaceTargetRef,
    WorkspaceTargetUnavailableReason, WorkspaceTmuxBinding,
};

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use kerminal_domain::{
    identity::PaneId,
    workspace::{SplitDirection, SplitLayoutSizes, TerminalLayoutNode, WorkspaceLayoutError},
};
use tokio_util::sync::CancellationToken;

use crate::{AppError, AppResult};

/// 由仓储对当前原始文件字节计算的 CAS token；内容不暴露路径或文件元信息。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WorkspaceSessionRevision(String);

impl WorkspaceSessionRevision {
    /// 接受仓储生成的不透明 token，不在应用层绑定 SHA 算法或存储实现。
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// 返回 CAS 比较值，调用方不得解析或将它拼入路径。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkspaceSessionRevision {
    /// 保留不透明比较值给 infrastructure 适配器使用，不暴露工作区文件位置。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// 明确区分首次创建与基于旧 revision 更新，避免缺失文件语义退化为无条件覆盖。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceSessionExpectedRevision {
    Missing,
    Revision(WorkspaceSessionRevision),
}

/// load 的无效原因只包含安全分类；磁盘原文始终留在原位置供人工恢复。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceSessionInvalidReason {
    MalformedJson,
    InvalidRoot,
    InvalidSchema,
    CredentialMaterial,
}

impl From<WorkspaceSessionInvalidReason> for AppError {
    /// 将 codec 错误映射为只读恢复所用的固定安全文案，不回传原始字段值。
    fn from(_reason: WorkspaceSessionInvalidReason) -> Self {
        AppError::configuration_corrupt("工作区 session")
    }
}

/// 仓储只暴露当前 v3、缺失、损坏或不支持版本，旧格式转换由独立迁移入口负责。
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceSessionLoad {
    Missing,
    Loaded {
        document: WorkspaceSessionDocument,
        revision: WorkspaceSessionRevision,
    },
    Invalid {
        reason: WorkspaceSessionInvalidReason,
    },
    Unsupported {
        version: u64,
    },
}

/// UI 显示保存闭环结果；冲突携带只读 latest 分类，不自动覆盖运行态。
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceSessionSaveOutcome {
    Saved { revision: WorkspaceSessionRevision },
    Conflict { latest: WorkspaceSessionLoad },
    OutcomeUnknown,
}

/// 唯一工作区 session 文件的持久化端口；实现需使用原子写和 revision CAS。
#[async_trait]
pub trait WorkspaceSessionRepository: Send + Sync + 'static {
    /// 将当前原文归为缺失、v3、损坏或不支持版本；读取取消不产生写入副作用。
    async fn load(&self, cancellation: CancellationToken) -> AppResult<WorkspaceSessionLoad>;

    /// 仅保存已验证的 v3；提交开始后协作取消不能丢弃 future 或伪报取消。
    async fn save(
        &self,
        cancellation: CancellationToken,
        document: &WorkspaceSessionDocument,
        expected_revision: WorkspaceSessionExpectedRevision,
    ) -> AppResult<WorkspaceSessionRevision>;
}

/// UI 与其它 adapter 共用同一仓储句柄，所有写入都通过显式恢复及 CAS 用例。
#[derive(Clone)]
pub struct WorkspaceSessionService {
    repository: Arc<dyn WorkspaceSessionRepository>,
}

impl WorkspaceSessionService {
    /// 注入唯一工作区仓储，避免 UI 与 MCP 分别决定损坏/旧版 session 的写入策略。
    pub fn new(repository: Arc<dyn WorkspaceSessionRepository>) -> Self {
        Self { repository }
    }

    /// 读取时保留损坏和不支持版本分类，旧格式须先经过独立迁移入口。
    pub async fn load(&self, cancellation: CancellationToken) -> AppResult<WorkspaceSessionLoad> {
        self.repository.load(cancellation).await
    }

    /// 按明确 revision 保存；调用者必须处理 conflict 与 outcome-unknown 两种终态。
    pub async fn save(
        &self,
        cancellation: CancellationToken,
        document: &WorkspaceSessionDocument,
        expected_revision: WorkspaceSessionExpectedRevision,
    ) -> AppResult<WorkspaceSessionRevision> {
        self.repository
            .save(cancellation, document, expected_revision)
            .await
    }

    /// 仅当最新 owned workspace snapshot 与 base 相同，才把外部未知字段合并后重试一次。
    pub async fn save_workspace_snapshot(
        &self,
        cancellation: CancellationToken,
        base_document: &WorkspaceSessionDocument,
        expected_revision: WorkspaceSessionExpectedRevision,
        base_snapshot: &WorkspaceSessionSnapshot,
        desired_snapshot: &WorkspaceSessionSnapshot,
    ) -> AppResult<WorkspaceSessionSaveOutcome> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("工作区保存已取消"));
        }
        if base_document.workspace_snapshot()? != *base_snapshot {
            return Err(AppError::conflict("工作区保存基线已变化，请重新读取"));
        }
        if expected_revision == WorkspaceSessionExpectedRevision::Missing
            && base_document != &WorkspaceSessionDocument::empty_v3()
        {
            return Err(AppError::conflict(
                "缺失工作区不能携带已加载文档作为保存基线",
            ));
        }
        desired_snapshot.validate()?;

        let mut proposed = base_document.clone();
        proposed.update_known_fields(desired_snapshot)?;
        match self
            .repository
            .save(cancellation.clone(), &proposed, expected_revision.clone())
            .await
        {
            Ok(revision) => return Ok(WorkspaceSessionSaveOutcome::Saved { revision }),
            Err(error) if error.public_code() == crate::PublicErrorCode::OutcomeUnknown => {
                return Ok(WorkspaceSessionSaveOutcome::OutcomeUnknown);
            }
            Err(error) if error.public_code() != crate::PublicErrorCode::Conflict => {
                return Err(error);
            }
            Err(_) => {}
        }

        let latest = self.repository.load(cancellation.clone()).await?;
        let (latest_document, retry_revision) = match &latest {
            WorkspaceSessionLoad::Loaded { document, revision }
                if document.workspace_snapshot()? == *base_snapshot =>
            {
                (
                    document.clone(),
                    WorkspaceSessionExpectedRevision::Revision(revision.clone()),
                )
            }
            WorkspaceSessionLoad::Missing
                if expected_revision == WorkspaceSessionExpectedRevision::Missing =>
            {
                (
                    WorkspaceSessionDocument::empty_v3(),
                    WorkspaceSessionExpectedRevision::Missing,
                )
            }
            _ => return Ok(WorkspaceSessionSaveOutcome::Conflict { latest }),
        };
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("工作区保存已取消"));
        }

        let mut merged = latest_document;
        merged.update_known_fields(desired_snapshot)?;
        match self
            .repository
            .save(cancellation, &merged, retry_revision)
            .await
        {
            Ok(revision) => Ok(WorkspaceSessionSaveOutcome::Saved { revision }),
            Err(error) if error.public_code() == crate::PublicErrorCode::OutcomeUnknown => {
                Ok(WorkspaceSessionSaveOutcome::OutcomeUnknown)
            }
            Err(error) if error.public_code() == crate::PublicErrorCode::Conflict => {
                Ok(WorkspaceSessionSaveOutcome::Conflict { latest })
            }
            Err(error) => Err(error),
        }
    }
}

/// 无状态的工作区服务句柄，只执行经过领域验证的终端布局变更。
///
/// 持久化留在独立仓储适配器后面，避免界面校验和暂存布局时受磁盘时序或 GPUI 窗口牵连。
#[derive(Clone, Copy, Debug, Default)]
pub struct WorkspaceService;

impl WorkspaceService {
    /// 创建共享用例句柄；布局规则保持纯数据，不在服务中保留会话或视图状态。
    pub const fn new() -> Self {
        Self
    }

    /// 用工作区 session v3 相同的单 pane 结构创建终端标签初始布局。
    pub fn new_terminal_layout(&self, pane_id: PaneId) -> TerminalLayoutNode {
        TerminalLayoutNode::pane(pane_id)
    }

    /// 布局进入渲染器或持久化边界前先验证，阻止损坏的恢复数据继续传播。
    pub fn validate_layout(&self, layout: &TerminalLayoutNode) -> AppResult<()> {
        layout.validate().map_err(map_layout_error)
    }

    /// 在目标 pane 旁新增子 pane，同时保留父 split 已保存的槽位比例。
    pub fn split_pane(
        &self,
        layout: &TerminalLayoutNode,
        target: &PaneId,
        new_pane: PaneId,
        direction: SplitDirection,
        split_id: impl Into<String>,
        before: bool,
    ) -> AppResult<TerminalLayoutNode> {
        layout
            .split_pane(target, new_pane, direction, split_id, before)
            .map_err(map_layout_error)
    }

    /// 只有权重键与 split 的直接子节点完全匹配时才接受拖拽尺寸更新。
    pub fn resize_split(
        &self,
        layout: &TerminalLayoutNode,
        split_id: &str,
        sizes: SplitLayoutSizes,
    ) -> AppResult<TerminalLayoutNode> {
        layout
            .resize_split(split_id, sizes)
            .map_err(map_layout_error)
    }

    /// 删除单个 pane 并折叠空余层级；仅当整个终端标签为空时返回 None。
    pub fn close_pane(
        &self,
        layout: &TerminalLayoutNode,
        pane_id: &PaneId,
    ) -> AppResult<Option<TerminalLayoutNode>> {
        layout.remove_pane(pane_id).map_err(map_layout_error)
    }

    /// 交换 pane 的布局位置，比例继续留在原槽位而不跟随 pane 内容移动。
    pub fn swap_panes(
        &self,
        layout: &TerminalLayoutNode,
        first: &PaneId,
        second: &PaneId,
    ) -> AppResult<TerminalLayoutNode> {
        layout.swap_panes(first, second).map_err(map_layout_error)
    }
}

/// 把领域错误映射为固定公开文案，避免持久化标识进入界面错误提示。
fn map_layout_error(error: WorkspaceLayoutError) -> AppError {
    match error {
        WorkspaceLayoutError::PaneNotFound(_) => AppError::not_found("终端 pane 不存在"),
        WorkspaceLayoutError::DuplicatePane(_) | WorkspaceLayoutError::DuplicateSplit(_) => {
            AppError::conflict("终端布局标识已存在")
        }
        WorkspaceLayoutError::SplitNotFound(_) => AppError::not_found("终端分屏不存在"),
        WorkspaceLayoutError::InvalidStructure(_) | WorkspaceLayoutError::InvalidSplitSizes(_) => {
            AppError::invalid("终端布局无法验证")
        }
    }
}
