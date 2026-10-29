//! Agent 历史筛选、标题维护和从启动快照派生新会话的用例。
//!
//! @author kongweiguang

use std::path::Path;

use kerminal_domain::{
    agents::{
        AgentLauncherKey, AgentProvider, AgentSession, AgentSessionScope, AgentSessionStatus,
    },
    identity::AgentSessionId,
};
use tokio_util::sync::CancellationToken;

use crate::{AppError, AppResult, sessions::SessionFilter};

use super::{
    AgentHistoryMode, AgentHistoryProjection, AgentHistoryRequest, AgentSessionService,
    AgentSessionView, CreateAgentSessionFromHistoryRequest, RenameAgentSessionTitleRequest,
    session_service::{current_timestamp, domain_error, find_agent_terminal},
};

impl AgentSessionService {
    /// 按当前右栏归属和稳定目标身份过滤历史；无候选目标时当前目标视图必须为空，不能把缺省误当作全匹配。
    ///
    /// @author kongweiguang
    pub async fn history(
        &self,
        request: AgentHistoryRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentHistoryProjection> {
        let sessions = self.list(cancellation).await?;
        let mut current_target_count = 0;
        let mut all_count = 0;
        let mut archived_count = 0;
        let mut visible_session_ids = Vec::new();
        for view in &sessions {
            let session = &view.session;
            if session.status == AgentSessionStatus::Archived {
                archived_count += 1;
                if request.mode == AgentHistoryMode::Archived {
                    visible_session_ids.push(session.agent_session_id.clone());
                }
                continue;
            }
            all_count += 1;
            let in_scope = session_history_scope(session)? == request.scope;
            let same_candidate = request.target_candidate.as_ref().is_some_and(|candidate| {
                session
                    .target
                    .as_ref()
                    .is_some_and(|binding| binding.points_to_same_session(candidate))
            });
            if in_scope && same_candidate {
                current_target_count += 1;
            }
            match request.mode {
                AgentHistoryMode::CurrentTarget if in_scope && same_candidate => {
                    visible_session_ids.push(session.agent_session_id.clone());
                }
                AgentHistoryMode::All => {
                    visible_session_ids.push(session.agent_session_id.clone());
                }
                AgentHistoryMode::CurrentTarget | AgentHistoryMode::Archived => {}
            }
        }
        Ok(AgentHistoryProjection {
            sessions,
            visible_session_ids,
            current_target_count,
            all_count,
            archived_count,
        })
    }

    /// 按 caller 提供的旧标题做 CAS 改名，冲突时不覆盖磁盘上的并发更新。
    /// 恢复能力读取仅补充操作入口，不改变改名提交的成功条件。
    ///
    /// @author kongweiguang
    pub async fn rename_title(
        &self,
        request: RenameAgentSessionTitleRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        let _gate = self
            .acquire_operation(&cancellation, "Agent rename was cancelled")
            .await?;
        self.ensure_request_active(&cancellation, "Agent rename was cancelled")?;
        let mut snapshot = self
            .read_session(request.agent_session_id.clone(), &cancellation)
            .await?;
        self.ensure_resume_recovery_resolved_before_mutation(&snapshot.session, &cancellation)
            .await?;
        if snapshot.session.title != request.expected_title {
            return Err(AppError::conflict("Agent title changed before rename"));
        }
        let title = request.title.trim();
        if title.is_empty() {
            return Err(AppError::invalid("Agent title is required"));
        }
        let current = snapshot.session.clone();
        let permission_mode = current.permission_mode;
        snapshot.session = AgentSession::restore(
            current.schema_version,
            current.agent_session_id,
            current.agent_id,
            current.launcher_key,
            title.to_owned(),
            current.created_at,
            current_timestamp(),
            current.status,
            current.workspace_root,
            current.session_root,
            current.launch,
            current.scope,
            current.target,
        )
        .map_err(domain_error)?;
        snapshot.session.permission_mode = permission_mode;
        snapshot.revision = self
            .persist_session_only(&snapshot.session, snapshot.revision, &cancellation)
            .await?;
        let live = self.sessions.list(SessionFilter::default()).await?;
        let terminal = find_agent_terminal(&live, &request.agent_session_id);
        let resume_recovery = self
            .resume_recovery_for_view(&snapshot.session, terminal.as_ref(), &cancellation)
            .await?;
        if resume_recovery.is_none() {
            self.reconcile_cli_state(&mut snapshot, terminal.as_ref(), &cancellation)
                .await?;
        }
        let resume_capability = self
            .read_resume_capability(&snapshot.session, &cancellation)
            .await?;
        Ok(AgentSessionView {
            session: snapshot.session,
            terminal,
            resume_recovery,
            resume_capability,
        })
    }

    /// 历史派生只复制有效启动意图；目录、会话身份、provider 会话元数据和目标均重新生成。
    ///
    /// @author kongweiguang
    pub async fn create_from_history(
        &self,
        request: CreateAgentSessionFromHistoryRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        self.ensure_mcp_runtime_started(&cancellation).await?;
        let _gate = self
            .acquire_operation(&cancellation, "Agent history creation was cancelled")
            .await?;
        self.ensure_request_active(&cancellation, "Agent history creation was cancelled")?;
        let endpoint = self.endpoint().await?;
        let source = self
            .read_session(request.source_agent_session_id.clone(), &cancellation)
            .await?
            .session;
        request.target.validate().map_err(domain_error)?;
        self.validate_target_binding(&request.target, &cancellation)
            .await?;

        let id = AgentSessionId::new();
        let root = self
            .await_cancellable(&cancellation, |token| {
                self.workspace.create_session_root(id.clone(), token)
            })
            .await?;
        let mut terminal = None;
        let created = async {
            if !Path::new(&root).is_absolute() {
                return Err(AppError::invalid(
                    "Agent session workspace must be an absolute path",
                ));
            }
            let (launcher_key, launch) = if source.agent_id == AgentProvider::Custom {
                let mut launch = source.launch.clone();
                launch.cwd = root.clone();
                launch.validate().map_err(domain_error)?;
                (source.launcher_key.clone(), launch)
            } else {
                let launcher_key =
                    AgentLauncherKey::builtin(source.agent_id).map_err(domain_error)?;
                let launcher = self
                    .launch_resolver
                    .resolve_for_key(Some(&launcher_key), &cancellation)
                    .await?;
                (
                    Some(launcher.launcher_key.clone()),
                    launcher.into_session_launch(root.clone())?,
                )
            };
            let title = request.title.unwrap_or_else(|| source.title.clone());
            let session = AgentSession::new(
                id.clone(),
                source.agent_id,
                launcher_key,
                title,
                current_timestamp(),
                self.workspace.workspace_root(),
                root.clone(),
                launch,
                Some(request.scope),
                Some(request.target),
            )
            .map_err(domain_error)?;
            self.start_created_session(
                session,
                &endpoint,
                &cancellation,
                &mut terminal,
                request.initial_palette,
            )
            .await
        }
        .await;

        match created {
            Ok(view) => Ok(view),
            Err(error) => Err(self.rollback_failed_creation(id, terminal, error).await),
        }
    }
}

/// 普通历史查询只使用当前 session 的明确归属，缺失持久字段按损坏处理而不猜测。
///
/// @author kongweiguang
fn session_history_scope(session: &AgentSession) -> AppResult<AgentSessionScope> {
    session
        .scope
        .clone()
        .ok_or_else(|| AppError::configuration_corrupt("Agent session scope"))
}
