//! 验证多来源候选的增量、隔离与通用替换契约。
//!
//! @author kongweiguang

use std::sync::Arc;

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    commands::{CommandSuggestionProvider, CompositeCommandSuggestionProvider},
};
use kerminal_domain::commands::{
    CommandHistoryTarget, CommandSuggestionActivation, CommandSuggestionCandidate,
    CommandSuggestionReplacementRange, CommandSuggestionRequest, CommandSuggestionSensitivity,
    SuggestionProviderKind, SuggestionQueryMode, SuggestionSourceState,
};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

struct Provider {
    kind: SuggestionProviderKind,
    values: Vec<CommandSuggestionCandidate>,
    gate: Option<Arc<Notify>>,
    fail: bool,
}

#[async_trait]
impl CommandSuggestionProvider for Provider {
    /// 每个夹具只占有一个来源，避免测试依赖聚合器内部实现顺序。
    fn supported_providers(&self) -> Vec<SuggestionProviderKind> {
        vec![self.kind]
    }

    /// 用可取消的显式门模拟慢来源，避免依赖不稳定的真实网络。
    async fn suggest(
        &self,
        _request: CommandSuggestionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<CommandSuggestionCandidate>> {
        if let Some(gate) = &self.gate {
            tokio::select! {
                _ = cancellation.cancelled() => return Err(AppError::cancelled("测试来源已取消")),
                _ = gate.notified() => {}
            }
        }
        if self.fail {
            return Err(AppError::infrastructure("测试来源失败"));
        }
        Ok(self.values.clone())
    }
}

/// 测试输入固定为命令起始前缀，便于检查大小写和追加范围。
fn request() -> CommandSuggestionRequest {
    CommandSuggestionRequest {
        input: "gi".to_owned(),
        cursor: 2,
        target: CommandHistoryTarget::Local,
        session_id: Some("session-test".to_owned()),
        pane_id: None,
        profile_id: None,
        remote_host_id: None,
        cwd: None,
        shell: None,
        providers: None,
        limit: Some(20),
        mode: SuggestionQueryMode::Inline,
        generation: Some(1),
        context_key: Some("context".to_owned()),
    }
}

/// 保留可辨认的来源后缀，用于验证共享聚合器不会擅自改写普通草稿候选。
fn candidate(
    kind: SuggestionProviderKind,
    id: &str,
    replacement: &str,
) -> CommandSuggestionCandidate {
    CommandSuggestionCandidate {
        id: id.to_owned(),
        provider: kind,
        activation: CommandSuggestionActivation::Insert,
        display_text: replacement.to_owned(),
        replacement_text: replacement.to_owned(),
        replacement_range: CommandSuggestionReplacementRange { start: 0, end: 2 },
        suffix: "untrusted".to_owned(),
        score: 1.0,
        sensitivity: CommandSuggestionSensitivity::Normal,
        description: None,
        source_explanation: Some(format!("{kind:?}")),
        merged_source_explanations: Vec::new(),
        source_id: None,
        allowed_presentations: CommandSuggestionCandidate::allowed_for(
            CommandSuggestionSensitivity::Normal,
        ),
        accept_boundaries: Vec::new(),
        context_key: None,
    }
}

/// 本地候选先于阻塞来源送达，同时保留该来源的 Pending 状态。
#[tokio::test]
async fn ready_candidates_are_published_before_a_slow_source() {
    let gate = Arc::new(Notify::new());
    let provider = Arc::new(
        CompositeCommandSuggestionProvider::new(vec![
            Arc::new(Provider {
                kind: SuggestionProviderKind::History,
                values: vec![candidate(
                    SuggestionProviderKind::History,
                    "history",
                    "git status",
                )],
                gate: None,
                fail: false,
            }),
            Arc::new(Provider {
                kind: SuggestionProviderKind::Spec,
                values: vec![],
                gate: Some(gate.clone()),
                fail: false,
            }),
        ])
        .unwrap(),
    );
    let (sender, mut receiver) = mpsc::channel(8);
    let task = tokio::spawn(async move {
        provider
            .suggest_with_progress(request(), CancellationToken::new(), Some(sender))
            .await
    });
    let first = tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.candidates[0].suffix, "untrusted");
    assert!(
        first
            .sources
            .iter()
            .any(|source| source.provider == SuggestionProviderKind::Spec
                && source.state == SuggestionSourceState::Pending)
    );
    gate.notify_one();
    let final_result = task.await.unwrap().unwrap();
    assert!(
        final_result
            .sources
            .iter()
            .all(|source| source.state == SuggestionSourceState::Ready)
    );
}

/// 失败来源不清空其它来源，共享聚合仍保留普通草稿可用的非前缀替换。
#[tokio::test]
async fn failures_are_isolated_and_generic_replacements_survive() {
    let provider = CompositeCommandSuggestionProvider::new(vec![
        Arc::new(Provider {
            kind: SuggestionProviderKind::History,
            values: vec![
                candidate(SuggestionProviderKind::History, "history", "git status"),
                candidate(SuggestionProviderKind::History, "wrong-case", "Git status"),
            ],
            gate: None,
            fail: false,
        }),
        Arc::new(Provider {
            kind: SuggestionProviderKind::Spec,
            values: vec![candidate(
                SuggestionProviderKind::Spec,
                "spec",
                "git status",
            )],
            gate: None,
            fail: false,
        }),
        Arc::new(Provider {
            kind: SuggestionProviderKind::Git,
            values: vec![],
            gate: None,
            fail: true,
        }),
    ])
    .unwrap();
    let result = provider
        .suggest_with_progress(request(), CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(result.candidates.len(), 2);
    let history = result
        .candidates
        .iter()
        .find(|item| item.id == "history")
        .unwrap();
    assert_eq!(history.suffix, "untrusted");
    assert_eq!(history.merged_source_explanations.len(), 1);
    assert!(result.candidates.iter().any(|item| item.id == "wrong-case"));
    assert!(
        result
            .sources
            .iter()
            .any(|source| source.provider == SuggestionProviderKind::Git
                && source.state == SuggestionSourceState::Failed)
    );
}

/// 普通草稿可以在光标不在行尾时替换已有范围，不能受终端追加策略限制。
#[tokio::test]
async fn generic_query_preserves_middle_of_line_replacement() {
    let provider = CompositeCommandSuggestionProvider::new(vec![Arc::new(Provider {
        kind: SuggestionProviderKind::History,
        values: vec![candidate(
            SuggestionProviderKind::History,
            "history",
            "git status",
        )],
        gate: None,
        fail: false,
    })])
    .unwrap();
    let mut query = request();
    query.input = "gi tail".to_owned();
    query.cursor = 2;

    let result = provider
        .suggest_with_progress(query, CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(result.candidates.len(), 1);
    assert_eq!(result.candidates[0].replacement_text, "git status");
    assert_eq!(result.candidates[0].suffix, "untrusted");
}

/// 用户取消查询后应把取消传递给慢来源，不能把取消转成空成功。
#[tokio::test]
async fn cancellation_reaches_a_slow_source() {
    let provider = Arc::new(
        CompositeCommandSuggestionProvider::new(vec![Arc::new(Provider {
            kind: SuggestionProviderKind::History,
            values: vec![],
            gate: Some(Arc::new(Notify::new())),
            fail: false,
        })])
        .unwrap(),
    );
    let cancellation = CancellationToken::new();
    let child = cancellation.clone();
    let task =
        tokio::spawn(async move { provider.suggest_with_progress(request(), child, None).await });
    tokio::task::yield_now().await;
    cancellation.cancel();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
}
