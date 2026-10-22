//! Agent 会话聚合及生命周期转换。
//!
//! @author kongweiguang

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::identity::AgentSessionId;

use super::{
    AgentDomainError, AgentLauncherKey, AgentProvider, AgentSessionScope, AgentTargetBinding,
    AgentTargetLiveStatus,
};

/// 归档状态不会删除历史文件，恢复动作会重新产生运行时 terminal session。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AgentSessionStatus {
    /// 可从历史列表继续的会话。
    Active,
    /// 用户已归档的只读历史。
    Archived,
    /// 元数据可读，但当前运行目标不再能确认。
    Stale,
}

/// 已验证的实际 CLI 启动快照，供恢复时避免受后续 Custom 设置变化影响。
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentSessionLaunch {
    /// 用户可见的命令标签。
    pub command_label: String,
    /// CLI 可执行文件或启动程序。
    pub shell: String,
    /// 精确传递给 CLI 的参数。
    #[serde(default)]
    pub args: Vec<String>,
    /// 该 Agent 会话的稳定工作目录。
    pub cwd: String,
}

impl fmt::Debug for AgentSessionLaunch {
    /// 调试时只展示参数数量，避免启动参数和工作路径泄露命令行中的敏感信息。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentSessionLaunch")
            .field("argument_count", &self.args.len())
            .field("sensitive_values", &"<redacted>")
            .finish()
    }
}

impl AgentSessionLaunch {
    /// 校验可恢复启动所需的最小快照，避免空值在 PTY 创建阶段变成含糊失败。
    pub fn validate(&self) -> Result<(), AgentDomainError> {
        if self.command_label.trim().is_empty() {
            return Err(AgentDomainError::InvalidLaunchSpec("commandLabel"));
        }
        if self.shell.trim().is_empty() {
            return Err(AgentDomainError::InvalidLaunchSpec("shell"));
        }
        if self.cwd.trim().is_empty() {
            return Err(AgentDomainError::InvalidLaunchSpec("cwd"));
        }
        Ok(())
    }
}

/// Agent 会话的持久业务身份；进程和 PTY 属于运行时，不伪装成可跨重启对象。
#[derive(Clone, Serialize, PartialEq, Eq)]
pub struct AgentSession {
    /// 当前受支持的历史文件 schema。
    pub schema_version: u32,
    /// Kerminal 内部 Agent 会话主键。
    pub agent_session_id: AgentSessionId,
    /// 外部 Agent provider。
    pub agent_id: AgentProvider,
    /// 区分同 provider 的不同 Custom 定义。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launcher_key: Option<AgentLauncherKey>,
    /// 用户可见标题。
    pub title: String,
    /// 创建时间。
    pub created_at: String,
    /// 最近一次业务状态变化时间。
    pub updated_at: String,
    /// 生命周期状态。
    pub status: AgentSessionStatus,
    /// Kerminal 全局配置与 MCP workspace 根路径。
    pub workspace_root: String,
    /// 当前受管 Agent 的隔离 cwd。
    pub session_root: String,
    /// 恢复所用的实际命令快照。
    pub launch: AgentSessionLaunch,
    /// 仅作为右栏会话归属信息；运行时工具范围恒为 global。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<AgentSessionScope>,
    /// 首选用户终端，不是 Agent TUI 所在的 terminal session。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<AgentTargetBinding>,
}

impl AgentSession {
    /// 建立自包含的会话元数据，拒绝没有工作区、启动命令或 provider 身份的半成品。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agent_session_id: AgentSessionId,
        agent_id: AgentProvider,
        launcher_key: Option<AgentLauncherKey>,
        title: String,
        created_at: String,
        workspace_root: String,
        session_root: String,
        launch: AgentSessionLaunch,
        scope: Option<AgentSessionScope>,
        target: Option<AgentTargetBinding>,
    ) -> Result<Self, AgentDomainError> {
        launch.validate()?;
        if title.trim().is_empty() {
            return Err(AgentDomainError::EmptyTitle);
        }
        if created_at.trim().is_empty() {
            return Err(AgentDomainError::EmptyTimestamp);
        }
        if workspace_root.trim().is_empty() || session_root.trim().is_empty() {
            return Err(AgentDomainError::EmptyWorkspacePath);
        }
        if let Some(target) = &target {
            target.validate()?;
        }
        if let Some(key) = &launcher_key {
            AgentLauncherKey::new(key.as_str(), agent_id)?;
        }
        Ok(Self {
            schema_version: 1,
            agent_session_id,
            agent_id,
            launcher_key,
            title: title.trim().to_owned(),
            created_at: created_at.clone(),
            updated_at: created_at,
            status: AgentSessionStatus::Active,
            workspace_root,
            session_root,
            launch,
            scope,
            target,
        })
    }

    /// 从已解码的持久化字段恢复历史状态，仍由领域规则校验版本与生命周期数据。
    #[allow(clippy::too_many_arguments)]
    pub fn restore(
        schema_version: u32,
        agent_session_id: AgentSessionId,
        agent_id: AgentProvider,
        launcher_key: Option<AgentLauncherKey>,
        title: String,
        created_at: String,
        updated_at: String,
        status: AgentSessionStatus,
        workspace_root: String,
        session_root: String,
        launch: AgentSessionLaunch,
        scope: Option<AgentSessionScope>,
        target: Option<AgentTargetBinding>,
    ) -> Result<Self, AgentDomainError> {
        if schema_version != 1 {
            return Err(AgentDomainError::UnsupportedSchemaVersion(schema_version));
        }
        if updated_at.trim().is_empty() {
            return Err(AgentDomainError::EmptyTimestamp);
        }
        let mut session = Self::new(
            agent_session_id,
            agent_id,
            launcher_key,
            title,
            created_at,
            workspace_root,
            session_root,
            launch,
            scope,
            target,
        )?;
        session.updated_at = updated_at;
        session.status = status;
        Ok(session)
    }

    /// 保留 tab 作为历史归属，同时固定工具授权投影为 global。
    pub const fn effective_scope(&self) -> AgentSessionScope {
        AgentSessionScope::Global
    }

    /// 绑定显式目标并以 generation 拒绝乱序 UI 更新，普通 Tab 切换不调用该方法。
    pub fn bind_preferred_target(
        &mut self,
        requested: AgentTargetBinding,
        updated_at: String,
    ) -> Result<(), AgentDomainError> {
        requested.validate()?;
        if let Some(current) = &self.target {
            if requested.generation < current.generation {
                return Err(AgentDomainError::StaleBindingGeneration {
                    current: current.generation,
                    requested: requested.generation,
                });
            }
            if requested.generation == current.generation
                && !current.points_to_same_session(&requested)
            {
                return Err(AgentDomainError::BindingGenerationReused(
                    requested.generation,
                ));
            }
        }
        self.target = Some(requested);
        self.updated_at = updated_at;
        Ok(())
    }

    /// 标记绑定目标已失效；保留旧 id 供用户显式重连，不选择其它可见终端。
    pub fn mark_target_stale(
        &mut self,
        status: AgentTargetLiveStatus,
        updated_at: String,
    ) -> Result<(), AgentDomainError> {
        if !matches!(
            status,
            AgentTargetLiveStatus::Stale | AgentTargetLiveStatus::Closed
        ) {
            return Err(AgentDomainError::InvalidTargetInvalidationStatus);
        }
        let target = self
            .target
            .as_mut()
            .ok_or(AgentDomainError::TargetBindingMissing)?;
        target.live_status = status;
        self.updated_at = updated_at;
        Ok(())
    }

    /// 仅在关联 CLI 进程确实结束或不可恢复时标记 Agent 会话失活。
    pub fn mark_cli_stale(&mut self, updated_at: String) {
        self.status = AgentSessionStatus::Stale;
        self.updated_at = updated_at;
    }

    /// 归档保留历史和受管目录；重复归档可安全重试。
    pub fn archive(&mut self, updated_at: String) {
        self.status = AgentSessionStatus::Archived;
        self.updated_at = updated_at;
    }

    /// 只有显式恢复成功后才回到 Active，阻止无效目标被静默当成当前 Tab。
    pub fn mark_resumed(
        &mut self,
        launch: AgentSessionLaunch,
        updated_at: String,
    ) -> Result<(), AgentDomainError> {
        launch.validate()?;
        self.launch = launch;
        self.updated_at = updated_at;
        self.status = AgentSessionStatus::Active;
        Ok(())
    }
}

impl fmt::Debug for AgentSession {
    /// 日志可识别会话身份和状态，但不输出启动快照、环境变量或本机路径。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentSession")
            .field("agent_session_id", &self.agent_session_id)
            .field("agent_id", &self.agent_id)
            .field("status", &self.status)
            .field("sensitive_values", &"<redacted>")
            .finish()
    }
}
