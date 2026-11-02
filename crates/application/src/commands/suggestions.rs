//! 基于本地历史和用户片段构建安全、目标相关的建议候选。
//!
//! @author kongweiguang

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use kerminal_domain::{
    commands::{
        BroadcastAnalysis, BroadcastRisk, CommandHistoryListRequest, CommandSuggestionActivation,
        CommandSuggestionCandidate, CommandSuggestionReplacementRange, CommandSuggestionRequest,
        CommandSuggestionResult, CommandSuggestionSensitivity, NormalizedCommandHistory,
        SnippetCatalogListRequest, SnippetCatalogOrigin, SuggestionPresentation,
        SuggestionProviderKind, SuggestionQueryMode, SuggestionSourceState, SuggestionSourceStatus,
    },
    identity::{HostId, PaneId, SessionId},
};
use tokio::{sync::mpsc, task::JoinSet};
use tokio_util::sync::CancellationToken;

use super::{
    AppError, AppResult, CommandHistoryRepository, CommandSuggestionProvider, SnippetRepository,
};

/// 聚合各独立建议来源，并在同一请求里统一校验、排序、去重和裁剪。
pub struct CompositeCommandSuggestionProvider {
    providers: Vec<Arc<dyn CommandSuggestionProvider>>,
}

impl CompositeCommandSuggestionProvider {
    /// 检查 provider kind 唯一归属，避免同一来源由两个 adapter 重复输出或竞争排序。
    pub fn new(providers: Vec<Arc<dyn CommandSuggestionProvider>>) -> AppResult<Self> {
        let mut seen = HashSet::new();
        for provider in &providers {
            let supported = provider.supported_providers();
            if supported.is_empty() || supported.iter().any(|kind| !seen.insert(*kind)) {
                return Err(AppError::invalid(
                    "命令建议来源为空或由多个 adapter 重复注册",
                ));
            }
        }
        Ok(Self { providers })
    }

    /// 每个来源独立运行并先推送已就绪快照；超时先协作取消，再有界等待资源回收。
    async fn collect(
        &self,
        request: CommandSuggestionRequest,
        cancellation: CancellationToken,
        updates: Option<mpsc::Sender<CommandSuggestionResult>>,
    ) -> AppResult<CommandSuggestionResult> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("命令建议查询已取消"));
        }
        let operation = cancellation.child_token();
        let mut tasks = JoinSet::new();
        let mut task_sources = HashMap::new();
        let mut sources = Vec::new();
        for provider in &self.providers {
            for kind in provider.supported_providers() {
                if !provider_requested(&request, kind)
                    || !provider.supports_provider_target(kind, request.target)
                {
                    continue;
                }
                let mut source_request = request.clone();
                source_request.providers = Some(vec![kind]);
                let source_provider = Arc::clone(provider);
                let token = operation.clone();
                let handle = tasks
                    .spawn(async move { source_provider.suggest(source_request, token).await });
                task_sources.insert(handle.id(), kind);
                sources.push(SuggestionSourceStatus {
                    provider: kind,
                    state: SuggestionSourceState::Pending,
                });
            }
        }
        let mut candidates = Vec::new();
        let deadline = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(deadline);
        let mut cancelled = false;
        while !tasks.is_empty() {
            let joined = tokio::select! {
                biased;
                _ = cancellation.cancelled() => { cancelled = true; break; }
                _ = &mut deadline => break,
                result = tasks.join_next_with_id() => result,
            };
            let Some(joined) = joined else { break };
            let (id, result) = match joined {
                Ok((id, result)) => (id, result),
                Err(error) => (
                    error.id(),
                    Err(AppError::infrastructure("命令建议来源任务异常终止")),
                ),
            };
            let Some(kind) = task_sources.remove(&id) else {
                continue;
            };
            if let Some(status) = sources.iter_mut().find(|status| status.provider == kind) {
                status.state = if result.is_ok() {
                    SuggestionSourceState::Ready
                } else {
                    SuggestionSourceState::Failed
                };
            }
            if let Ok(source_candidates) = result {
                candidates.extend(
                    source_candidates
                        .into_iter()
                        .filter(|item| item.provider == kind),
                );
            }
            if let Some(updates) = &updates {
                let mut snapshot = candidates.clone();
                filter_rank_and_deduplicate(&request, &mut snapshot);
                let _ = updates.try_send(CommandSuggestionResult {
                    candidates: snapshot,
                    sources: sources.clone(),
                });
            }
        }
        if !tasks.is_empty() {
            operation.cancel();
            let cleanup = async { while tasks.join_next().await.is_some() {} };
            if tokio::time::timeout(Duration::from_millis(4_500), cleanup)
                .await
                .is_err()
            {
                tasks.abort_all();
                let _ = tokio::time::timeout(Duration::from_millis(100), async {
                    while tasks.join_next().await.is_some() {}
                })
                .await;
            }
            for status in &mut sources {
                if status.state == SuggestionSourceState::Pending {
                    status.state = SuggestionSourceState::Failed;
                }
            }
        }
        if cancelled || cancellation.is_cancelled() {
            return Err(AppError::cancelled("命令建议查询已取消"));
        }
        filter_rank_and_deduplicate(&request, &mut candidates);
        Ok(CommandSuggestionResult {
            candidates,
            sources,
        })
    }
}

#[async_trait]
impl CommandSuggestionProvider for CompositeCommandSuggestionProvider {
    /// 按确定性来源顺序返回注册能力，便于 composition root 校验实际启用的五类 provider。
    fn supported_providers(&self) -> Vec<SuggestionProviderKind> {
        self.providers
            .iter()
            .flat_map(|provider| provider.supported_providers())
            .collect()
    }

    /// 旧调用方仍接收单批候选；全部来源失败时保留可见错误而不伪装成空结果。
    async fn suggest(
        &self,
        request: CommandSuggestionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<CommandSuggestionCandidate>> {
        let result = self.collect(request, cancellation, None).await?;
        if !result.sources.is_empty()
            && result
                .sources
                .iter()
                .all(|source| source.state == SuggestionSourceState::Failed)
        {
            return Err(AppError::infrastructure("所有命令建议来源均不可用"));
        }
        Ok(result.candidates)
    }

    /// 增量快照不包含底层错误正文，调用方通过来源状态提示部分失败。
    async fn suggest_with_progress(
        &self,
        request: CommandSuggestionRequest,
        cancellation: CancellationToken,
        updates: Option<mpsc::Sender<CommandSuggestionResult>>,
    ) -> AppResult<CommandSuggestionResult> {
        self.collect(request, cancellation, updates).await
    }
}

/// 只使用注入的本地仓储生成建议，不访问网络、当前 Tab 或终端进程。
pub struct HistoryCommandSuggestionProvider {
    history: Arc<dyn CommandHistoryRepository>,
    snippets: Arc<dyn SnippetRepository>,
}

impl HistoryCommandSuggestionProvider {
    /// 共享 command history 与文件化片段仓储，使 UI 与 MCP 查询结果来自同一数据源。
    pub fn new(
        history: Arc<dyn CommandHistoryRepository>,
        snippets: Arc<dyn SnippetRepository>,
    ) -> Self {
        Self { history, snippets }
    }
}

#[async_trait]
impl CommandSuggestionProvider for HistoryCommandSuggestionProvider {
    /// 合并历史前缀与显式片段面板候选，长查询与片段读取均响应同一取消 token。
    fn supported_providers(&self) -> Vec<SuggestionProviderKind> {
        vec![
            SuggestionProviderKind::History,
            SuggestionProviderKind::Snippet,
        ]
    }

    /// 合并历史前缀与显式片段面板候选，并按候选策略过滤敏感和危险展示位。
    async fn suggest(
        &self,
        request: CommandSuggestionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<CommandSuggestionCandidate>> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("命令建议查询已取消"));
        }
        let cursor = request.cursor.min(request.input.chars().count());
        let prefix = request.input.chars().take(cursor).collect::<String>();
        let mut candidates = Vec::new();
        if provider_requested(&request, SuggestionProviderKind::History)
            && !(request.mode == SuggestionQueryMode::Inline && prefix.trim().is_empty())
        {
            candidates.extend(
                self.history_candidates(&request, &prefix, cursor, &cancellation)
                    .await?,
            );
        }
        if provider_requested(&request, SuggestionProviderKind::Snippet) {
            candidates.extend(
                self.snippet_candidates(&request, &prefix, cursor, &cancellation)
                    .await?,
            );
        }
        candidates.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.display_text.cmp(&right.display_text))
        });
        candidates.truncate(request.limit.unwrap_or(20).clamp(1, 100));
        Ok(candidates)
    }
}

impl HistoryCommandSuggestionProvider {
    /// 本地按环境召回、SSH 严格按主机召回；会话和目录只影响排序而不阻断新会话复用。
    async fn history_candidates(
        &self,
        request: &CommandSuggestionRequest,
        prefix: &str,
        cursor: usize,
        cancellation: &CancellationToken,
    ) -> AppResult<Vec<CommandSuggestionCandidate>> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("命令历史建议已取消"));
        }
        let session_id = parse_optional_id(request.session_id.as_deref(), SessionId::parse)?;
        let pane_id = parse_optional_id(request.pane_id.as_deref(), PaneId::parse)?;
        let remote_host_id = parse_optional_id(request.remote_host_id.as_deref(), HostId::parse)?;
        if request.target == kerminal_domain::commands::CommandHistoryTarget::Ssh
            && remote_host_id.is_none()
        {
            return Ok(Vec::new());
        }
        let reusable_target = matches!(
            request.target,
            kerminal_domain::commands::CommandHistoryTarget::Local
                | kerminal_domain::commands::CommandHistoryTarget::Ssh
        );
        let entries = self
            .history
            .list_prefix(
                CommandHistoryListRequest {
                    query: None,
                    source: None,
                    target: Some(request.target),
                    pane_id: if reusable_target { None } else { pane_id },
                    remote_host_id,
                    session_id: if reusable_target {
                        None
                    } else {
                        session_id.clone()
                    },
                    limit: Some(100),
                },
                prefix.to_owned(),
            )
            .await?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("命令历史建议已取消"));
        }
        let mut frequency = HashMap::<String, usize>::new();
        for entry in &entries {
            if let Ok(command) = NormalizedCommandHistory::new(&entry.command)
                && command.sensitive_skip_reason().is_none()
                && command.command.starts_with(prefix)
                && !command.command.chars().any(char::is_control)
            {
                *frequency.entry(command.command).or_default() += 1;
            }
        }
        let mut candidates = entries
            .into_iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let command = match NormalizedCommandHistory::new(&entry.command) {
                    Ok(command) if command.sensitive_skip_reason().is_none() => command.command,
                    _ => return None,
                };
                if !command.starts_with(prefix) || command.chars().any(char::is_control) {
                    return None;
                }
                let danger = BroadcastAnalysis::analyze(&command, Vec::new())
                    .risks
                    .contains(&BroadcastRisk::Destructive);
                let sensitivity = if danger {
                    CommandSuggestionSensitivity::Dangerous
                } else {
                    CommandSuggestionSensitivity::Normal
                };
                let allowed_presentations = CommandSuggestionCandidate::allowed_for(sensitivity);
                let required_presentation = match request.mode {
                    SuggestionQueryMode::Inline => SuggestionPresentation::Inline,
                    SuggestionQueryMode::Menu => SuggestionPresentation::Menu,
                };
                if !allowed_presentations.contains(&required_presentation) {
                    return None;
                }
                Some(CommandSuggestionCandidate {
                    id: format!("history:{}", entry.id),
                    provider: SuggestionProviderKind::History,
                    activation: CommandSuggestionActivation::Insert,
                    display_text: command.clone(),
                    replacement_text: command.clone(),
                    replacement_range: CommandSuggestionReplacementRange {
                        start: 0,
                        end: cursor,
                    },
                    suffix: command.chars().skip(cursor).collect(),
                    score: 0.5
                        + if request.cwd.is_some() && entry.cwd == request.cwd {
                            0.3
                        } else {
                            0.0
                        }
                        + if request.shell.is_some() && entry.shell == request.shell {
                            0.06
                        } else {
                            0.0
                        }
                        + if session_id.is_some() && entry.session_id == session_id {
                            0.03
                        } else {
                            0.0
                        }
                        + (frequency.get(&command).copied().unwrap_or_default().min(10) as f64
                            * 0.005)
                        + (0.05 / (index as f64 + 1.0)),
                    sensitivity,
                    description: Some("命令历史".to_owned()),
                    source_explanation: Some("来自当前目标的本地命令历史".to_owned()),
                    merged_source_explanations: Vec::new(),
                    source_id: Some(entry.id),
                    allowed_presentations,
                    accept_boundaries: Vec::new(),
                    context_key: request.context_key.clone(),
                })
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.id.cmp(&right.id))
        });
        let mut seen = HashSet::new();
        candidates.retain(|candidate| seen.insert(candidate.replacement_text.clone()));
        Ok(candidates)
    }

    /// 只在用户显式输入 `::` 前缀时建议片段，避免把模板变量误当成可直接执行命令。
    async fn snippet_candidates(
        &self,
        request: &CommandSuggestionRequest,
        prefix: &str,
        cursor: usize,
        cancellation: &CancellationToken,
    ) -> AppResult<Vec<CommandSuggestionCandidate>> {
        let Some(query) = prefix.strip_prefix("::") else {
            return Ok(Vec::new());
        };
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("命令片段建议已取消"));
        }
        let items = self
            .snippets
            .list(SnippetCatalogListRequest {
                query: (!query.is_empty()).then(|| query.to_owned()),
                origin: Some(SnippetCatalogOrigin::User),
                scope: None,
                limit: Some(50),
            })
            .await?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("命令片段建议已取消"));
        }
        Ok(items
            .into_iter()
            .filter(|item| item.title.to_lowercase().starts_with(&query.to_lowercase()))
            .map(|item| CommandSuggestionCandidate {
                id: format!("snippet:{}", item.id),
                provider: SuggestionProviderKind::Snippet,
                activation: CommandSuggestionActivation::OpenSnippetPanel,
                display_text: item.title,
                replacement_text: item.id.clone(),
                replacement_range: CommandSuggestionReplacementRange {
                    start: 0,
                    end: cursor,
                },
                suffix: String::new(),
                score: 0.75,
                sensitivity: if item.sensitive {
                    CommandSuggestionSensitivity::Sensitive
                } else {
                    CommandSuggestionSensitivity::Normal
                },
                description: Some(item.description),
                source_explanation: Some("打开用户片段面板并填写变量".to_owned()),
                merged_source_explanations: Vec::new(),
                source_id: Some(item.id),
                allowed_presentations: CommandSuggestionCandidate::allowed_for(if item.sensitive {
                    CommandSuggestionSensitivity::Sensitive
                } else {
                    CommandSuggestionSensitivity::Normal
                }),
                accept_boundaries: Vec::new(),
                context_key: request.context_key.clone(),
            })
            .filter(|candidate| {
                let required = match request.mode {
                    SuggestionQueryMode::Inline => SuggestionPresentation::Inline,
                    SuggestionQueryMode::Menu => SuggestionPresentation::Menu,
                };
                candidate.allowed_presentations.contains(&required)
            })
            .collect())
    }
}

/// 只保留当前展示模式允许、范围有效且属于本次上下文的候选，再稳定排序和合并重复项。
/// @author kongweiguang
fn filter_rank_and_deduplicate(
    request: &CommandSuggestionRequest,
    candidates: &mut Vec<CommandSuggestionCandidate>,
) {
    let input_chars = request.input.chars().count();
    let wanted = match request.mode {
        SuggestionQueryMode::Inline => SuggestionPresentation::Inline,
        SuggestionQueryMode::Menu => SuggestionPresentation::Menu,
    };
    candidates.retain_mut(|candidate| {
        if !candidate.score.is_finite()
            || candidate.id.is_empty()
            || candidate.id.chars().count() > 256
            || candidate.replacement_text.chars().count() > 8_000
            || candidate.display_text.chars().count() > 8_000
            || candidate.replacement_range.start > candidate.replacement_range.end
            || candidate.replacement_range.end > input_chars
            || !candidate.allowed_presentations.contains(&wanted)
        {
            return false;
        }
        candidate.context_key = request.context_key.clone();
        candidate.accept_boundaries.retain(|boundary| {
            *boundary > 0 && *boundary <= candidate.replacement_text.chars().count()
        });
        true
    });
    candidates.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| provider_order(left.provider).cmp(&provider_order(right.provider)))
            .then_with(|| left.display_text.cmp(&right.display_text))
            .then_with(|| left.id.cmp(&right.id))
    });

    let mut deduplicated = Vec::<CommandSuggestionCandidate>::with_capacity(candidates.len());
    for candidate in candidates.drain(..) {
        let existing = deduplicated.iter_mut().find(|existing| {
            existing.activation == candidate.activation
                && existing.replacement_range == candidate.replacement_range
                && existing.replacement_text == candidate.replacement_text
        });
        if let Some(existing) = existing {
            if let Some(explanation) = candidate.source_explanation
                && !existing.merged_source_explanations.contains(&explanation)
            {
                existing.merged_source_explanations.push(explanation);
            }
        } else {
            deduplicated.push(candidate);
        }
    }
    deduplicated.truncate(request.limit.unwrap_or(20).clamp(1, 100));
    *candidates = deduplicated;
}

/// 使用固定产品顺序解除同分候选平局，避免 provider 注册次序影响键盘接受结果。
fn provider_order(provider: SuggestionProviderKind) -> u8 {
    match provider {
        SuggestionProviderKind::History => 0,
        SuggestionProviderKind::LocalPath | SuggestionProviderKind::RemotePath => 1,
        SuggestionProviderKind::LocalCommand | SuggestionProviderKind::RemoteCommand => 2,
        SuggestionProviderKind::Git => 3,
        SuggestionProviderKind::Spec => 4,
        SuggestionProviderKind::Snippet => 5,
    }
}

/// Provider filter absent means “all available local providers”; explicit empty means none.
fn provider_requested(
    request: &CommandSuggestionRequest,
    provider: SuggestionProviderKind,
) -> bool {
    request
        .providers
        .as_ref()
        .is_none_or(|providers| providers.contains(&provider))
}

/// 把外部字符串 ID 转换为类型化标识，拒绝损坏的上下文而不改投其它终端。
fn parse_optional_id<T>(
    value: Option<&str>,
    parse: impl FnOnce(&str) -> Result<T, kerminal_domain::DomainError>,
) -> AppResult<Option<T>> {
    value.map(parse).transpose().map_err(Into::into)
}
