//! 命令建议反馈与保留期清理的 SQLite 适配器。
//!
//! 反馈和审计复用命令历史数据库及其锁，避免新增数据库带来不一致的备份边界。
//!
//! @author kongweiguang

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    commands::{CommandSuggestionFeedbackRepository, CommandSuggestionRetentionResult},
};
use kerminal_domain::commands::suggestion::CommandSuggestionFeedbackRequest;
use kerminal_domain::commands::{
    CommandHistoryTarget, CommandSuggestionFeedbackAction, NormalizedCommandHistory,
    SuggestionProviderKind, validate_history_context,
};
use rusqlite::{TransactionBehavior, params};
use uuid::Uuid;

use super::SqliteCommandHistoryRepository;

const MAX_RETENTION_DAYS: u32 = 3_650;
const MILLIS_PER_DAY: i64 = 86_400_000;

#[async_trait]
impl CommandSuggestionFeedbackRepository for SqliteCommandHistoryRepository {
    /// 短 SQLite 写入在线程池执行，阻止 UI executor 执行同步磁盘等待。
    async fn record_feedback(&self, request: CommandSuggestionFeedbackRequest) -> AppResult<()> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || repository.record_feedback_sync(request))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("命令建议反馈任务未完成", source)
            })?
    }

    /// 在一笔即时事务内清理过期反馈与审计行，保留周期采用 UTC Unix 毫秒。
    async fn prune_expired(
        &self,
        feedback_retention_days: u32,
        audit_retention_days: u32,
    ) -> AppResult<CommandSuggestionRetentionResult> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || {
            repository.prune_expired_sync(feedback_retention_days, audit_retention_days)
        })
        .await
        .map_err(|source| {
            AppError::infrastructure_with_source("命令建议保留期任务未完成", source)
        })?
    }
}

impl SqliteCommandHistoryRepository {
    /// 校验反馈正文并在 adapter 边界再次拒绝疑似秘密，避免绕过用例时落入历史库。
    fn record_feedback_sync(&self, request: CommandSuggestionFeedbackRequest) -> AppResult<()> {
        let feedback = normalize_feedback(request)?;
        let now = unix_timestamp_millis()?;
        let audit_id = Uuid::new_v4().to_string();

        let inner = self.require_inner()?;
        let _storage_guard = inner
            .storage
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库存储锁不可用"))?;
        let mut connection = inner
            .connection
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库连接状态异常"))?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| sqlite_error("开始命令建议反馈事务失败", source))?;

        transaction
            .execute(
                "INSERT INTO command_suggestion_feedback (
                    id, action, provider, target, replacement_text, input, source_id,
                    session_id, pane_id, profile_id, remote_host_id, cwd, shell,
                    created_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    Uuid::new_v4().to_string(),
                    action_as_str(feedback.action),
                    provider_as_str(feedback.provider),
                    target_as_str(feedback.target),
                    feedback.replacement_text,
                    feedback.input,
                    feedback.source_id,
                    feedback.session_id,
                    feedback.pane_id,
                    feedback.profile_id,
                    feedback.remote_host_id,
                    feedback.cwd,
                    feedback.shell,
                    now,
                ],
            )
            .map_err(|source| sqlite_error("保存命令建议反馈失败", source))?;

        transaction
            .execute(
                "INSERT INTO command_suggestion_audit_events (
                    id, event_kind, provider, target, decision, reason, remote_host_id,
                    cwd, pane_id, session_id, metadata_json, created_at_unix_ms
                 ) VALUES (?1, 'feedback', ?2, ?3, 'recorded', 'feedback-recorded',
                           ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    audit_id,
                    provider_as_str(feedback.provider),
                    target_as_str(feedback.target),
                    feedback.remote_host_id,
                    feedback.cwd,
                    feedback.pane_id,
                    feedback.session_id,
                    action_metadata(feedback.action),
                    now,
                ],
            )
            .map_err(|source| sqlite_error("记录命令建议审计事件失败", source))?;

        transaction
            .commit()
            .map_err(|source| sqlite_error("提交命令建议反馈事务失败", source))
    }

    /// 同时删除两类过期行，避免反馈或审计只清理一半后返回成功。
    fn prune_expired_sync(
        &self,
        feedback_retention_days: u32,
        audit_retention_days: u32,
    ) -> AppResult<CommandSuggestionRetentionResult> {
        let now = unix_timestamp_millis()?;
        let feedback_cutoff = retention_cutoff(now, feedback_retention_days);
        let audit_cutoff = retention_cutoff(now, audit_retention_days);

        let inner = self.require_inner()?;
        let _storage_guard = inner
            .storage
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库存储锁不可用"))?;
        let mut connection = inner
            .connection
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库连接状态异常"))?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| sqlite_error("开始命令建议清理事务失败", source))?;

        let feedback_deleted = transaction
            .execute(
                "DELETE FROM command_suggestion_feedback WHERE created_at_unix_ms < ?1",
                [feedback_cutoff],
            )
            .map_err(|source| sqlite_error("清理过期命令建议反馈失败", source))?;
        let audit_deleted = transaction
            .execute(
                "DELETE FROM command_suggestion_audit_events WHERE created_at_unix_ms < ?1",
                [audit_cutoff],
            )
            .map_err(|source| sqlite_error("清理过期命令建议审计失败", source))?;

        let result = CommandSuggestionRetentionResult {
            feedback_deleted: u64::try_from(feedback_deleted)
                .map_err(|_| AppError::infrastructure("反馈清理计数超出范围"))?,
            audit_deleted: u64::try_from(audit_deleted)
                .map_err(|_| AppError::infrastructure("审计清理计数超出范围"))?,
        };

        transaction
            .commit()
            .map_err(|source| sqlite_error("提交命令建议清理事务失败", source))?;
        Ok(result)
    }
}

/// 将反馈正文转为稳定数据库值，并对 snippet 永不保留展开命令。
fn normalize_feedback(request: CommandSuggestionFeedbackRequest) -> AppResult<NormalizedFeedback> {
    let (replacement_text, input, source_id) = if request.provider
        == SuggestionProviderKind::Snippet
    {
        let source_id = request
            .source_id
            .as_deref()
            .filter(|source_id| safe_snippet_id(source_id))
            .map(str::to_owned);
        let snippet_id = source_id.as_deref().unwrap_or("unknown");
        (
            format!("@snippet:{snippet_id}"),
            "@snippet".to_owned(),
            source_id,
        )
    } else {
        let replacement = NormalizedCommandHistory::new(&request.replacement_text)
            .map_err(|_| AppError::invalid("命令建议反馈候选无效"))?;
        let input = NormalizedCommandHistory::new(&request.input)
            .map_err(|_| AppError::invalid("命令建议反馈输入无效"))?;
        if replacement.sensitive_skip_reason().is_some() || input.sensitive_skip_reason().is_some()
        {
            return Err(AppError::invalid("命令建议反馈包含疑似敏感内容"));
        }
        (
            replacement.command,
            input.command,
            request.source_id.clone(),
        )
    };

    validate_history_context(
        request.cwd.as_deref(),
        request.shell.as_deref(),
        &[
            source_id.as_deref(),
            request.session_id.as_deref(),
            request.pane_id.as_deref(),
            request.profile_id.as_deref(),
            request.remote_host_id.as_deref(),
        ],
    )
    .map_err(|_| AppError::invalid("命令建议反馈上下文超过长度限制"))?;

    Ok(NormalizedFeedback {
        action: request.action,
        provider: request.provider,
        target: request.target,
        replacement_text,
        input,
        source_id,
        session_id: request.session_id,
        pane_id: request.pane_id,
        profile_id: request.profile_id,
        remote_host_id: request.remote_host_id,
        cwd: request.cwd,
        shell: request.shell,
    })
}

/// 反馈内部值只在事务构造期间存在，不对调用方暴露或派生 Debug。
struct NormalizedFeedback {
    action: CommandSuggestionFeedbackAction,
    provider: SuggestionProviderKind,
    target: CommandHistoryTarget,
    replacement_text: String,
    input: String,
    source_id: Option<String>,
    session_id: Option<String>,
    pane_id: Option<String>,
    profile_id: Option<String>,
    remote_host_id: Option<String>,
    cwd: Option<String>,
    shell: Option<String>,
}

/// 仅接受可读的片段标识，避免把任意正文伪装成 snippet 来源 ID。
fn safe_snippet_id(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 160
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// 将保留天数约束在 1 到 3650 天，并以 Unix 毫秒计算 UTC 截止点。
fn retention_cutoff(now_unix_ms: i64, retention_days: u32) -> i64 {
    let days = retention_days.clamp(1, MAX_RETENTION_DAYS) as i64;
    now_unix_ms.saturating_sub(days.saturating_mul(MILLIS_PER_DAY))
}

/// 读取可表示为 SQLite INTEGER 的 UTC Unix 毫秒，时钟异常时显式失败。
fn unix_timestamp_millis() -> AppResult<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|source| {
            AppError::infrastructure_with_source("系统时间早于 Unix epoch", source)
        })?;
    i64::try_from(duration.as_millis())
        .map_err(|_| AppError::infrastructure("系统时间超出命令数据库范围"))
}

/// 映射 action 到 v1 schema 固定文本，避免把 serde 表示作为持久化契约。
const fn action_as_str(action: CommandSuggestionFeedbackAction) -> &'static str {
    match action {
        CommandSuggestionFeedbackAction::Accepted => "accepted",
        CommandSuggestionFeedbackAction::Dismissed => "dismissed",
    }
}

/// 映射 provider 到 v1–v3 SQLite 检查约束使用的历史字符串。
const fn provider_as_str(provider: SuggestionProviderKind) -> &'static str {
    match provider {
        SuggestionProviderKind::History => "history",
        SuggestionProviderKind::RemotePath => "remotePath",
        SuggestionProviderKind::RemoteCommand => "remoteCommand",
        SuggestionProviderKind::Git => "git",
        SuggestionProviderKind::Spec => "spec",
        SuggestionProviderKind::Snippet => "snippet",
    }
}

/// 保持容器目标在旧数据库中的 `dockerContainer` 拼写。
const fn target_as_str(target: CommandHistoryTarget) -> &'static str {
    match target {
        CommandHistoryTarget::Local => "local",
        CommandHistoryTarget::Ssh => "ssh",
        CommandHistoryTarget::Telnet => "telnet",
        CommandHistoryTarget::Serial => "serial",
        CommandHistoryTarget::Container => "dockerContainer",
    }
}

/// 生成最小 action-only 审计 JSON，不包含候选、输入或其他命令正文。
const fn action_metadata(action: CommandSuggestionFeedbackAction) -> &'static str {
    match action {
        CommandSuggestionFeedbackAction::Accepted => r#"{"action":"accepted"}"#,
        CommandSuggestionFeedbackAction::Dismissed => r#"{"action":"dismissed"}"#,
    }
}

/// 将 SQLite 错误包装为稳定错误，不回显 SQL 参数或候选正文。
fn sqlite_error(context: &'static str, source: rusqlite::Error) -> AppError {
    AppError::infrastructure_with_source(context, source)
}
