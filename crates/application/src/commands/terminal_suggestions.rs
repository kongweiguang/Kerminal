//! 按可信终端输入上下文查询 inline 与菜单建议。
//!
//! @author kongweiguang

use std::time::Duration;

use kerminal_domain::{
    commands::{
        CommandSuggestionActivation, CommandSuggestionCandidate, CommandSuggestionRequest,
        NormalizedCommandHistory, SuggestionProviderKind, SuggestionQueryMode,
        SuggestionSourceState, SuggestionSourceStatus,
    },
    preferences::ConfigurationMode,
    sessions::{SessionLifecycle, SessionOwner, SessionTarget},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    AppError, AppResult,
    preferences::{
        TerminalCommandSuggestionPresentation, TerminalCommandSuggestionRemoteRefresh,
        TerminalInlineSuggestionSettings,
    },
    sessions::{
        SHELL_EDITING_MAX_INPUT_CHARACTERS, SHELL_EDITING_MAX_SUFFIX_BYTES, ShellEditingShell,
        ShellEditingSnapshot, TerminalCommandAvailability, TerminalCommandContextVersion,
        UiSessionBinding,
    },
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

/// 一次可信编辑快照对应的增量或最终结果；来源状态不包含底层错误正文。
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalSuggestionProgress {
    pub batch: TerminalSuggestionBatch,
    pub snapshot_revision: u64,
    pub sources: Vec<SuggestionSourceStatus>,
}

/// 有界进度出口隐藏运行时通道类型，界面无需依赖 Tokio 或拥有额外执行器。
#[derive(Clone)]
pub struct TerminalSuggestionProgressPublisher(mpsc::Sender<TerminalSuggestionProgress>);

/// 订阅者只消费当前查询的增量；最终结果仍由原用例回执提供，通道拥塞不会丢失完成态。
pub struct TerminalSuggestionProgressSubscription(mpsc::Receiver<TerminalSuggestionProgress>);

impl TerminalSuggestionProgressPublisher {
    /// 固定小容量限制慢界面的积压；来源发布不等待主线程，避免查询反向阻塞输入。
    pub fn channel() -> (Self, TerminalSuggestionProgressSubscription) {
        let (sender, receiver) = mpsc::channel(8);
        (
            Self(sender),
            TerminalSuggestionProgressSubscription(receiver),
        )
    }

    /// 中间帧可被后续完整投影覆盖；终态由用例返回，丢弃拥塞帧不会伪造成功或失败。
    fn try_publish(&self, progress: TerminalSuggestionProgress) {
        let _ = self.0.try_send(progress);
    }
}

impl TerminalSuggestionProgressSubscription {
    /// 等待属于这次查询的下一帧，全部发布者退出后结束，不创建独立运行时。
    pub async fn recv(&mut self) -> Option<TerminalSuggestionProgress> {
        self.0.recv().await
    }
}

impl CommandsService {
    /// 旧入口不再把启动目录冒充实时目录；新增界面应使用带 Shell 快照的入口。
    pub async fn terminal_suggestions(
        &self,
        query: TerminalSuggestionQuery,
        request_cancellation: CancellationToken,
        task_cancellation: CancellationToken,
    ) -> AppResult<TerminalSuggestionBatch> {
        let result = self
            .terminal_suggestions_internal(
                query,
                None,
                request_cancellation,
                task_cancellation,
                None,
            )
            .await?;
        if !result.sources.is_empty()
            && result
                .sources
                .iter()
                .all(|source| source.state == SuggestionSourceState::Failed)
        {
            return Err(AppError::infrastructure("所有终端建议来源均不可用"));
        }
        Ok(result.batch)
    }

    /// 绑定真实 Shell 缓冲、实时目录和代次；增量结果及最终结果采用同一快照版本。
    pub async fn terminal_suggestions_from_snapshot(
        &self,
        snapshot: ShellEditingSnapshot,
        request_generation: u64,
        mode: SuggestionQueryMode,
        request_cancellation: CancellationToken,
        task_cancellation: CancellationToken,
        updates: Option<TerminalSuggestionProgressPublisher>,
    ) -> AppResult<TerminalSuggestionProgress> {
        let query = TerminalSuggestionQuery {
            binding: snapshot.binding.clone(),
            expected_context: snapshot.context,
            request_generation,
            input: snapshot.input.clone(),
            cursor: snapshot.cursor,
            mode,
        };
        self.terminal_suggestions_internal(
            query,
            Some(snapshot),
            request_cancellation,
            task_cancellation,
            updates,
        )
        .await
    }

    /// 合并新输入与应用退出取消，并给来源有限时间完成取消与资源回收。
    async fn terminal_suggestions_internal(
        &self,
        query: TerminalSuggestionQuery,
        snapshot: Option<ShellEditingSnapshot>,
        request_cancellation: CancellationToken,
        task_cancellation: CancellationToken,
        updates: Option<TerminalSuggestionProgressPublisher>,
    ) -> AppResult<TerminalSuggestionProgress> {
        let provider_cancellation = CancellationToken::new();
        let query_future = self.query_terminal_suggestions(
            query,
            snapshot,
            provider_cancellation.clone(),
            updates,
        );
        tokio::pin!(query_future);
        tokio::select! {
            biased;
            _ = request_cancellation.cancelled() => {
                provider_cancellation.cancel();
                let _ = tokio::time::timeout(Duration::from_millis(4_750), &mut query_future).await;
                Err(AppError::cancelled("终端建议查询已过期"))
            }
            _ = task_cancellation.cancelled() => {
                provider_cancellation.cancel();
                let _ = tokio::time::timeout(Duration::from_millis(4_750), &mut query_future).await;
                Err(AppError::cancelled("终端建议任务已停止"))
            }
            result = &mut query_future => result,
        }
    }

    /// 使用最新 UI 设置预览，并在 provider 前后复核 UI owner 与可信上下文，迟到结果统一返回空候选。
    async fn query_terminal_suggestions(
        &self,
        query: TerminalSuggestionQuery,
        snapshot: Option<ShellEditingSnapshot>,
        cancellation: CancellationToken,
        updates: Option<TerminalSuggestionProgressPublisher>,
    ) -> AppResult<TerminalSuggestionProgress> {
        validate_query(&query)?;
        let revision = snapshot.as_ref().map_or(0, |snapshot| snapshot.revision);
        let empty = || TerminalSuggestionProgress {
            batch: TerminalSuggestionBatch {
                binding: query.binding.clone(),
                expected_context: query.expected_context,
                request_generation: query.request_generation,
                candidates: Vec::new(),
            },
            snapshot_revision: revision,
            sources: Vec::new(),
        };
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("终端建议查询已取消"));
        }
        if !query.input.is_empty() && is_sensitive_input(&query.input) {
            return Ok(empty());
        }
        if snapshot
            .as_ref()
            .is_some_and(|snapshot| !snapshot.cursor_at_end())
        {
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
            matches!(&session.target, SessionTarget::Local(_)),
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
            cwd: snapshot.as_ref().and_then(|snapshot| snapshot.cwd.clone()),
            shell: snapshot
                .as_ref()
                .map(|snapshot| snapshot_shell_name(snapshot.shell).to_owned()),
            providers: Some(providers),
            // 留出过滤余量，避免非终端替换候选提前挤掉可追加候选。
            limit: Some(100),
            mode: query.mode,
            generation: Some(query.request_generation),
            context_key: Some(context_key),
        };
        let (progress_sender, mut progress_receiver) = mpsc::channel(8);
        let provider = self.suggestions.suggest_with_progress(
            request,
            cancellation.clone(),
            updates.as_ref().map(|_| progress_sender),
        );
        tokio::pin!(provider);
        let result = loop {
            tokio::select! {
                result = &mut provider => break result?,
                Some(partial) = progress_receiver.recv(), if updates.is_some() => {
                    let latest = self.sessions.terminal_command_context(query.binding.session_id.clone()).await;
                    if !latest.as_ref().is_ok_and(|latest| context_matches(&query, latest)) {
                        cancellation.cancel();
                        let _ = tokio::time::timeout(Duration::from_millis(4_600), &mut provider).await;
                        return match latest {
                            Ok(_) => Ok(empty()),
                            Err(error) => Err(error),
                        };
                    }
                    if let Some(sender) = &updates {
                        let mut progress = empty();
                        progress.batch.candidates = terminal_candidates(&query, partial.candidates);
                        progress.sources = partial.sources;
                        sender.try_publish(progress);
                    }
                }
            }
        };
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
        let mut progress = empty();
        progress.batch.candidates = terminal_candidates(&query, result.candidates);
        progress.sources = result.sources;
        Ok(progress)
    }
}

/// 终端只允许在 writer 的字节与整行上限内追加，展示前过滤确定不可接受的结果；共享草稿仍可替换。
fn terminal_candidates(
    query: &TerminalSuggestionQuery,
    candidates: Vec<CommandSuggestionCandidate>,
) -> Vec<CommandSuggestionCandidate> {
    let mut retained = Vec::<CommandSuggestionCandidate>::new();
    for mut candidate in candidates {
        if candidate.activation == CommandSuggestionActivation::Insert {
            if query.cursor != query.input.chars().count()
                || candidate.replacement_range.end != query.cursor
                || candidate.replacement_range.start > query.cursor
            {
                continue;
            }
            let typed = query
                .input
                .chars()
                .skip(candidate.replacement_range.start)
                .collect::<String>();
            let Some(suffix) = candidate.replacement_text.strip_prefix(&typed) else {
                continue;
            };
            if suffix.is_empty()
                || suffix.len() > SHELL_EDITING_MAX_SUFFIX_BYTES
                || query.cursor + suffix.chars().count() > SHELL_EDITING_MAX_INPUT_CHARACTERS
                || suffix.chars().any(char::is_control)
            {
                continue;
            }
            candidate.suffix = suffix.to_owned();
        } else if query.mode != SuggestionQueryMode::Menu || !query.input.starts_with("::") {
            continue;
        }

        let existing = retained.iter_mut().find(|existing| {
            existing.activation == candidate.activation
                && if candidate.activation == CommandSuggestionActivation::Insert {
                    existing.suffix == candidate.suffix
                } else {
                    existing.replacement_range == candidate.replacement_range
                        && existing.replacement_text == candidate.replacement_text
                }
        });
        if let Some(existing) = existing {
            for explanation in candidate
                .source_explanation
                .into_iter()
                .chain(candidate.merged_source_explanations)
            {
                if !existing.merged_source_explanations.contains(&explanation) {
                    existing.merged_source_explanations.push(explanation);
                }
            }
        } else {
            retained.push(candidate);
        }
    }
    retained.truncate(20);
    retained
}

/// 限制输入长度、控制字符与 Unicode 光标边界，避免 provider 收到不完整或不可见上下文。
fn validate_query(query: &TerminalSuggestionQuery) -> AppResult<()> {
    let char_count = query.input.chars().count();
    if char_count > SHELL_EDITING_MAX_INPUT_CHARACTERS
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
    is_local: bool,
    is_ssh: bool,
    runtime_available: bool,
) -> Vec<SuggestionProviderKind> {
    let mut providers = Vec::with_capacity(8);
    if settings.providers.history {
        providers.push(SuggestionProviderKind::History);
    }
    if settings.providers.spec {
        providers.push(SuggestionProviderKind::Spec);
    }
    if mode == SuggestionQueryMode::Menu && input.starts_with("::") {
        providers.push(SuggestionProviderKind::Snippet);
    }
    if is_local {
        providers.push(SuggestionProviderKind::LocalPath);
        providers.push(SuggestionProviderKind::LocalCommand);
        if settings.providers.git {
            providers.push(SuggestionProviderKind::Git);
        }
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

/// 只投影会话内已确认的精确 Shell 身份，不把泛化 POSIX 方言猜成 Bash。
fn snapshot_shell_name(shell: ShellEditingShell) -> &'static str {
    match shell {
        ShellEditingShell::Bash => "bash",
        ShellEditingShell::Zsh => "zsh",
        ShellEditingShell::Fish => "fish",
        ShellEditingShell::PowerShell => "powershell",
    }
}

#[cfg(test)]
#[path = "../../tests/unit/terminal_suggestion_policy.rs"]
mod tests;
