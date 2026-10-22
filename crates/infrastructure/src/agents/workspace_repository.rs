//! Agent workspace 文件适配器；业务文档生成和生命周期编排留在 application。
//!
//! @author kongweiguang

use std::{collections::HashSet, path::PathBuf};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    agents::{
        AgentProviderSessionSnapshot, AgentResumeCheckpointSnapshot, AgentWorkspaceDocument,
        AgentWorkspaceDocumentContent, AgentWorkspaceDocumentKind, AgentWorkspaceDocumentSnapshot,
        AgentWorkspaceRepository, AgentWorkspaceSessionSnapshot, AgentWorkspaceWriteReceipt,
        CommitAgentResumeCheckpointRequest, PrepareAgentResumeCheckpointRequest,
    },
};
use kerminal_domain::{agents::AgentProviderSession, identity::AgentSessionId};
use tokio_util::sync::CancellationToken;

use crate::storage::StorageCoordinator;

use super::store::{
    AgentSessionStore, AgentWorkspaceFile, AgentWorkspaceFileKind, relative_document_path,
};

/// 只负责受管 session 路径、安全读写和 journal 恢复，不保存 Agent 业务状态。
pub struct FileAgentWorkspaceRepository {
    store: AgentSessionStore,
}

impl FileAgentWorkspaceRepository {
    /// 保留兼容构造入口；独立测试或非组合根调用者使用以工作区为根的 coordinator。
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            store: AgentSessionStore::new(workspace_root),
        }
    }

    /// 复用组合根共享锁，避免 Agent journal 和配置、vault 等写入彼此越过事务边界。
    pub fn with_storage_coordinator(workspace_root: PathBuf, storage: StorageCoordinator) -> Self {
        Self {
            store: AgentSessionStore::with_storage_coordinator(workspace_root, storage),
        }
    }

    /// 将固定文档 kind 映射到 session 根内的相对路径，调用方不能提交任意路径。
    fn document_path(
        &self,
        agent_session_id: &AgentSessionId,
        kind: AgentWorkspaceDocumentKind,
    ) -> AppResult<PathBuf> {
        let mut path = self.store.session_root(agent_session_id)?;
        for component in relative_document_path(kind) {
            path.push(component);
        }
        Ok(path)
    }
}

#[async_trait]
impl AgentWorkspaceRepository for FileAgentWorkspaceRepository {
    /// 返回配置根的稳定路径快照，由领域会话记录用于后续恢复与显示。
    fn workspace_root(&self) -> String {
        self.store.workspace_root().to_string_lossy().into_owned()
    }

    /// 创建独占的新工作区；完成后即使令牌刚取消也返回路径，让 application 负责回滚。
    async fn create_session_root(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<String> {
        ensure_not_cancelled(&cancellation)?;
        let root = self.store.create_session_root(&agent_session_id).await?;
        Ok(root.to_string_lossy().into_owned())
    }

    /// 仅回滚本次用例创建的准确 ID，保留其它 session 与用户工作区内容。
    async fn rollback_created_session_root(
        &self,
        agent_session_id: AgentSessionId,
    ) -> AppResult<()> {
        self.store
            .remove_created_session_root(&agent_session_id)
            .await
    }

    /// 读取经显式 DTO 和领域 restore 校验的 session，不接受损坏或未来 schema。
    async fn read_session(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentWorkspaceSessionSnapshot> {
        ensure_not_cancelled(&cancellation)?;
        let (session, revision) = self.store.read_session(&agent_session_id).await?;
        ensure_not_cancelled(&cancellation)?;
        Ok(AgentWorkspaceSessionSnapshot { session, revision })
    }

    /// 枚举持久会话并保留 store 对损坏记录的显式失败语义。
    async fn list_sessions(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<AgentWorkspaceSessionSnapshot>> {
        ensure_not_cancelled(&cancellation)?;
        let sessions = self
            .store
            .list_sessions()
            .await?
            .into_iter()
            .map(|(session, revision)| AgentWorkspaceSessionSnapshot { session, revision })
            .collect();
        ensure_not_cancelled(&cancellation)?;
        Ok(sessions)
    }

    /// 只读取固定 kind 对应的普通文件，拒绝符号链接与越出配置根的父路径。
    async fn read_document(
        &self,
        agent_session_id: AgentSessionId,
        kind: AgentWorkspaceDocumentKind,
        cancellation: CancellationToken,
    ) -> AppResult<AgentWorkspaceDocumentSnapshot> {
        ensure_not_cancelled(&cancellation)?;
        let path = self.document_path(&agent_session_id, kind)?;
        let (contents, revision) = self
            .store
            .read_document(&agent_session_id, kind, path)
            .await?;
        ensure_not_cancelled(&cancellation)?;
        Ok(AgentWorkspaceDocumentSnapshot { contents, revision })
    }

    /// 以受锁读取返回 provider 的诊断状态和 CAS 基线，不把坏文件伪装为缺失。
    async fn read_provider_session(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentProviderSessionSnapshot> {
        ensure_not_cancelled(&cancellation)?;
        let snapshot = self.store.read_provider_session(&agent_session_id).await?;
        ensure_not_cancelled(&cancellation)?;
        Ok(snapshot)
    }

    /// 将 typed provider 值送入统一 session journal，禁止该专用端口绕开 CAS 提交路径。
    ///
    /// @author kongweiguang
    async fn write_provider_session(
        &self,
        agent_session_id: AgentSessionId,
        provider_session: AgentProviderSession,
        expected_revision: kerminal_application::agents::AgentWorkspaceRevision,
        cancellation: CancellationToken,
    ) -> AppResult<AgentWorkspaceWriteReceipt> {
        ensure_not_cancelled(&cancellation)?;
        let contents = super::store::encode_agent_provider_document(&provider_session)?;
        let path = self.document_path(&agent_session_id, AgentWorkspaceDocumentKind::Provider)?;
        let receipts = self
            .store
            .write_files(
                agent_session_id,
                vec![AgentWorkspaceFile {
                    kind: AgentWorkspaceFileKind::Document(AgentWorkspaceDocumentKind::Provider),
                    path,
                    contents,
                    expected_revision,
                }],
            )
            .await?;
        receipts.into_iter().next().ok_or_else(|| {
            AppError::infrastructure("Agent provider session commit returned no receipt")
        })
    }

    /// 将读取时 revision 一并交给 journal store；写入启动后不丢弃 future 以保留真实结果。
    ///
    /// @author kongweiguang
    async fn write_documents(
        &self,
        agent_session_id: AgentSessionId,
        documents: Vec<AgentWorkspaceDocument>,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<AgentWorkspaceWriteReceipt>> {
        ensure_not_cancelled(&cancellation)?;
        let mut kinds = HashSet::with_capacity(documents.len());
        let mut files = Vec::with_capacity(documents.len());
        for document in documents {
            if !kinds.insert(document.kind) {
                return Err(AppError::invalid(
                    "Agent workspace transaction contains a duplicate document kind",
                ));
            }
            let contents = match (document.kind, document.content) {
                (
                    AgentWorkspaceDocumentKind::Session,
                    AgentWorkspaceDocumentContent::Session(session),
                ) => super::store::encode_agent_session_document(&session)?,
                (
                    AgentWorkspaceDocumentKind::Provider,
                    AgentWorkspaceDocumentContent::Provider(provider),
                ) => super::store::encode_agent_provider_document(&provider)?,
                (AgentWorkspaceDocumentKind::Session | AgentWorkspaceDocumentKind::Provider, _) => {
                    return Err(AppError::invalid(
                        "Agent metadata documents require typed session/provider content",
                    ));
                }
                (_, AgentWorkspaceDocumentContent::Text(contents)) => contents.into_bytes(),
                (_, _) => {
                    return Err(AppError::invalid(
                        "Text documents cannot contain typed session/provider content",
                    ));
                }
            };
            files.push(AgentWorkspaceFile {
                kind: AgentWorkspaceFileKind::Document(document.kind),
                path: self.document_path(&agent_session_id, document.kind)?,
                contents,
                expected_revision: document.expected_revision,
            });
        }
        self.store.write_files(agent_session_id, files).await
    }

    /// 复用受管仓储在启动 CLI 前持久化脱敏 checkpoint，防止失败时业务状态无恢复基线。
    ///
    /// @author kongweiguang
    async fn prepare_resume_checkpoint(
        &self,
        request: PrepareAgentResumeCheckpointRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentResumeCheckpointSnapshot> {
        ensure_not_cancelled(&cancellation)?;
        self.store.prepare_resume_checkpoint(request).await
    }

    /// 将原 typed 意图与 Session/Provider 一并写入既有 journal，避免重建参数覆盖第三方编辑。
    ///
    /// @author kongweiguang
    async fn commit_resume_checkpoint(
        &self,
        request: CommitAgentResumeCheckpointRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentResumeCheckpointSnapshot> {
        ensure_not_cancelled(&cancellation)?;
        self.store.commit_resume_checkpoint(request).await
    }

    /// 返回 marker 的 typed 投影和原始 revision，损坏状态由仓储报错而非伪装成不存在。
    ///
    /// @author kongweiguang
    async fn read_resume_checkpoint(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<Option<AgentResumeCheckpointSnapshot>> {
        ensure_not_cancelled(&cancellation)?;
        let snapshot = self.store.read_resume_checkpoint(&agent_session_id).await?;
        ensure_not_cancelled(&cancellation)?;
        Ok(snapshot)
    }

    /// 只清除调用方提供的精确 attempt/revision；运行 owner 安全判断仍由 application 负责。
    ///
    /// @author kongweiguang
    async fn clear_resume_checkpoint(
        &self,
        agent_session_id: AgentSessionId,
        expected: AgentResumeCheckpointSnapshot,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        ensure_not_cancelled(&cancellation)?;
        self.store
            .clear_resume_checkpoint(&agent_session_id, expected)
            .await
    }
}

/// 只在 I/O 边界检查取消；提交开始后保留 future 直至写盘事务返回其真实状态。
fn ensure_not_cancelled(cancellation: &CancellationToken) -> AppResult<()> {
    if cancellation.is_cancelled() {
        Err(AppError::cancelled(
            "Agent workspace operation was cancelled",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/support/agent_workspace_repository_unit.rs"]
mod tests;
