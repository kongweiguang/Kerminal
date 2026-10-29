//! 按可信终端输入上下文查询 inline 与菜单建议。
//!
//! @author kongweiguang

use kerminal_domain::{
    commands::{
        CommandSuggestionCandidate, CommandSuggestionRequest, NormalizedCommandHistory,
        SuggestionProviderKind, SuggestionQueryMode,
    },
    connections::RemoteShellDialect,
    preferences::ConfigurationMode,
    sessions::{SessionLifecycle, SessionOwner, SessionTarget},
};
use tokio_util::sync::CancellationToken;

use crate::{
    AppError, AppResult,
    preferences::{
        TerminalCommandSuggestionPresentation, TerminalCommandSuggestionRemoteRefresh,
        TerminalInlineSuggestionSettings,
    },
    sessions::{TerminalCommandAvailability, TerminalCommandContextVersion, UiSessionBinding},
};

use super::{CommandsService, execution};

/// 查询绑定会话 owner、prompt 和输入代次；代次不匹配时不得展示返回候选。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalSuggestionQuery {
    pub binding: UiSessionBinding,
    pub expected_context: TerminalCommandContextVersion,
    pub request_generation: u64,
    pub input: String,
    /// 光标位置使用 Unicode 标量索引，避免中文和组合输入产生 UTF-8 字节偏移。
    pub cursor: usize,
    pub mode: SuggestionQueryMode,
}

/// 查询结果与完整 session/prompt/input 版本绑定，不能跨 Tab 或重连复用。
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalSuggestionBatch {
    pub binding: UiSessionBinding,
    pub expected_context: TerminalCommandContextVersion,
    pub request_generation: u64,
    pub candidates: Vec<CommandSuggestionCandidate>,
}

impl CommandsService {
    /// 合并 UI 新查询取消与应用退出取消，并把同一个 token 传到远端 provider。
    pub async fn terminal_suggestions(
        &self,
        query: TerminalSuggestionQuery,
        request_cancellation: CancellationToken,
        task_cancellation: CancellationToken,
    ) -> AppResult<TerminalSuggestionBatch> {
        let provider_cancellation = CancellationToken::new();
        let query_future = self.query_terminal_suggestions(query, provider_cancellation.clone());
        tokio::select! {
            biased;
            _ = request_cancellation.cancelled() => {
                provider_cancellation.cancel();
                Err(AppError::cancelled("终端建议查询已过期"))
            }
            _ = task_cancellation.cancelled() => {
                provider_cancellation.cancel();
                Err(AppError::cancelled("终端建议任务已停止"))
            }
            result = query_future => result,
        }
    }

    /// 使用最新 UI 设置预览，并在 provider 前后复核 UI owner 与可信上下文，迟到结果统一返回空候选。
    async fn query_terminal_suggestions(
        &self,
        query: TerminalSuggestionQuery,
        cancellation: CancellationToken,
    ) -> AppResult<TerminalSuggestionBatch> {
        validate_query(&query)?;
        let empty = || TerminalSuggestionBatch {
            binding: query.binding.clone(),
            expected_context: query.expected_context,
            request_generation: query.request_generation,
            candidates: Vec::new(),
        };
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("终端建议查询已取消"));
        }
        if !query.input.is_empty() && is_sensitive_input(&query.input) {
            return Ok(empty());
        }

        let preferences = self
            .preferences
            .as_ref()
            .ok_or_else(|| AppError::capability_not_supported("终端建议未装配设置读取端口"))?;
        let settings = match self.suggestion_settings_preview.current() {
            Some(settings) => settings,
            None => {
                preferences
                    .load_settings()
                    .await?
                    .settings
                    .terminal
                    .inline_suggestion
            }
        };
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("终端建议查询已取消"));
        }
        if !settings.enabled || !presentation_enabled(settings.presentation, query.mode) {
            return Ok(empty());
        }
        if !matches!(&query.binding.owner, SessionOwner::Ui { .. }) {
            return Ok(empty());
        }

        let session = match self.session_info(query.binding.session_id.clone()).await {
            Ok(session) => session,
            Err(error) if error.public_code() == crate::PublicErrorCode::NotFound => {
                return Ok(empty());
            }
            Err(error) => return Err(error),
        };
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("终端建议查询已取消"));
        }
        if session.lifecycle != SessionLifecycle::Running || session.owner != query.binding.owner {
            return Ok(empty());
        }
        let runtime = self
            .sessions
            .terminal_command_context(query.binding.session_id.clone())
            .await?;
        if !context_matches(&query, &runtime) {
            return Ok(empty());
        }

        let runtime_available = preferences.recovery_state().mode == ConfigurationMode::ReadWrite;
        let providers = selected_providers(
            &settings,
            query.mode,
            &query.input,
            matches!(&session.target, SessionTarget::Ssh(_)),
            runtime_available,
        );
        if providers.is_empty() {
            return Ok(empty());
        }
        let context_key = format!(
            "{}:{}:{}:{}:{}",
            query.binding.session_id.as_str(),
            query.expected_context.session_generation,
            query.expected_context.prompt_epoch,
            query.expected_context.input_generation,
            query.request_generation
        );
        let request = CommandSuggestionRequest {
            input: query.input.clone(),
            cursor: query.cursor,
            target: execution::history_target(&session.target),
            session_id: Some(query.binding.session_id.as_str().to_owned()),
            pane_id: execution::owner_pane(&session.owner).map(|id| id.as_str().to_owned()),
            profile_id: None,
            remote_host_id: execution::target_host(&session.target)
                .map(|id| id.as_str().to_owned()),
            cwd: execution::target_cwd(&session.target),
            shell: suggestion_shell(&session.target, runtime.shell_dialect),
            providers: Some(providers),
            limit: Some(20),
            mode: query.mode,
            generation: Some(query.request_generation),
            context_key: Some(context_key),
        };
        let candidates = self
            .suggestions
            .suggest(request, cancellation.clone())
            .await?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("终端建议查询已取消"));
        }
        let latest = self
            .sessions
            .terminal_command_context(query.binding.session_id.clone())
            .await?;
        if !context_matches(&query, &latest) {
            return Ok(empty());
        }
        Ok(TerminalSuggestionBatch {
            binding: query.binding,
            expected_context: query.expected_context,
            request_generation: query.request_generation,
            candidates,
        })
    }
}

/// 限制输入长度、控制字符与 Unicode 光标边界，避免 provider 收到不完整或不可见上下文。
fn validate_query(query: &TerminalSuggestionQuery) -> AppResult<()> {
    let char_count = query.input.chars().count();
    if char_count > 4_000
        || query.input.len() > 32_000
        || query.cursor > char_count
        || query.input.chars().any(char::is_control)
    {
        return Err(AppError::invalid("终端建议输入上下文无效或超过长度限制"));
    }
    Ok(())
}

/// 只在 B 提供的完整可用状态与 query 三元代次完全匹配时接受 runtime。
fn context_matches(
    query: &TerminalSuggestionQuery,
    runtime: &crate::sessions::TerminalCommandContext,
) -> bool {
    runtime.availability == TerminalCommandAvailability::Available
        && runtime.session_id == query.binding.session_id
        && runtime.owner == query.binding.owner
        && runtime.session_generation == query.binding.session_generation
        && runtime.version() == query.expected_context
        && runtime.prompt_ready
        && !runtime.alternate_screen
}

/// 沿用领域历史过滤器；解析失败时拒绝把可能的凭据交给 provider。
fn is_sensitive_input(input: &str) -> bool {
    match NormalizedCommandHistory::new(input) {
        Ok(normalized) => normalized.sensitive_skip_reason().is_some(),
        Err(_) => true,
    }
}

/// 依据全局呈现偏好限制 inline 和主动菜单的查询入口。
fn presentation_enabled(
    configured: TerminalCommandSuggestionPresentation,
    requested: SuggestionQueryMode,
) -> bool {
    matches!(
        (configured, requested),
        (
            TerminalCommandSuggestionPresentation::Inline
                | TerminalCommandSuggestionPresentation::InlineAndMenu,
            SuggestionQueryMode::Inline
        ) | (
            TerminalCommandSuggestionPresentation::InlineAndMenu,
            SuggestionQueryMode::Menu
        )
    )
}

/// 只按当前查询配置选择来源；远端探测还需满足 SSH、安全刷新和可写恢复态。
fn selected_providers(
    settings: &TerminalInlineSuggestionSettings,
    mode: SuggestionQueryMode,
    input: &str,
    is_ssh: bool,
    runtime_available: bool,
) -> Vec<SuggestionProviderKind> {
    let mut providers = Vec::with_capacity(6);
    if settings.providers.history {
        providers.push(SuggestionProviderKind::History);
    }
    if settings.providers.spec {
        providers.push(SuggestionProviderKind::Spec);
    }
    if mode == SuggestionQueryMode::Menu && input.starts_with("::") {
        providers.push(SuggestionProviderKind::Snippet);
    }
    let remote_enabled = runtime_available
        && settings.remote_probe_enabled
        && settings.remote_refresh == TerminalCommandSuggestionRemoteRefresh::Safe;
    if remote_enabled && is_ssh {
        if settings.providers.remote_path {
            providers.push(SuggestionProviderKind::RemotePath);
        }
        if settings.providers.remote_command {
            providers.push(SuggestionProviderKind::RemoteCommand);
        }
        if settings.providers.git {
            providers.push(SuggestionProviderKind::Git);
        }
    }
    providers
}

/// 不把泛化 POSIX 方言伪称为 Bash；远程 shell 名只在方言有唯一可用名称时投影。
fn suggestion_shell(target: &SessionTarget, dialect: RemoteShellDialect) -> Option<String> {
    if let Some(shell) = execution::local_shell(target) {
        return Some(shell);
    }
    match dialect {
        RemoteShellDialect::Fish => Some("fish".to_owned()),
        RemoteShellDialect::PowerShell => Some("powershell".to_owned()),
        RemoteShellDialect::Posix | RemoteShellDialect::Unknown => None,
    }
}

#[cfg(test)]
#[path = "../../tests/unit/terminal_suggestion_policy.rs"]
mod tests;
