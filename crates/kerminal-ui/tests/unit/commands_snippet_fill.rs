//! 片段直发状态保留目标身份，并阻止未知结果被无意重放。
//!
//! @author kongweiguang

use std::sync::{Arc, Mutex};

use futures::executor::block_on;
use kerminal_application::sessions::UiSessionBinding;
use kerminal_application::{AppError, ApplicationTaskExecutor};
use kerminal_domain::{
    commands::{
        SnippetCatalogItem, SnippetCatalogOrigin, SnippetDefaultAction, SnippetRisk, SnippetScope,
    },
    identity::{PaneId, SessionId, TabId},
    sessions::SessionOwner,
};
use tokio_util::sync::CancellationToken;

use super::snippet_definition_from_catalog;
use super::snippet_fill::{TerminalFillState, record_snippet_usage_best_effort};

/// 测试记录独立调用线程、service future 构造及执行阶段的 Tokio runtime 边界。
#[derive(Clone, Default)]
struct SnippetUsageRuntimeEvidence {
    service_builder_has_tokio_runtime: bool,
    service_has_tokio_runtime: bool,
    blocking_result: Option<u8>,
}

/// 来源展示信息不能伪装为派生身份，否则保存或发送会错误改变片段的来源语义。
///
/// @author kongweiguang
#[test]
fn catalog_projection_preserves_derived_identity_independently_of_source_metadata() {
    for derived_from in [None, Some("builtin-original".to_owned())] {
        let item = SnippetCatalogItem {
            id: "docker-resource-usage".to_owned(),
            origin: SnippetCatalogOrigin::Builtin,
            title: "Docker 资源占用".to_owned(),
            description: "读取容器单次资源快照".to_owned(),
            template: "docker stats --no-stream".to_owned(),
            category: "container".to_owned(),
            pack: "docker".to_owned(),
            tags: Vec::new(),
            scope: SnippetScope::Any,
            platforms: vec!["linux".to_owned()],
            shells: Vec::new(),
            capabilities: Vec::new(),
            risk: SnippetRisk::Unknown,
            sensitive: false,
            duration: "short".to_owned(),
            default_action: SnippetDefaultAction::Insert,
            variables: Vec::new(),
            context_bindings: Vec::new(),
            derived_from: derived_from.clone(),
            catalog_version: Some("2".to_owned()),
            command_spec: None,
            owner: None,
            tested_version: None,
            source_name: Some("Docker documentation".to_owned()),
            source_url: Some(
                "https://docs.docker.com/reference/cli/docker/container/stats/".to_owned(),
            ),
            deprecated: false,
            favorite: false,
            use_count: 0,
            last_used_at_unix_ms: None,
            sort_order: 0,
            updated_at: String::new(),
        };
        let projected = snippet_definition_from_catalog(item);
        assert_eq!(projected.derived_from, derived_from);
    }
}

/// Unknown 回执必须绑定到发送时的原目标，并在用户确认检查前阻止重复发送。
///
/// @author kongweiguang
#[test]
fn unknown_send_result_keeps_original_target_and_blocks_replay_until_review() {
    let binding = UiSessionBinding {
        session_id: SessionId::parse("session-main").expect("valid session id"),
        owner: SessionOwner::Ui {
            tab_id: TabId::parse("tab-main").expect("valid tab id"),
            pane_id: PaneId::parse("pane-main").expect("valid pane id"),
        },
        session_generation: 8,
    };
    let mut state = TerminalFillState {
        cancellation: None,
        unknown_target: Some(binding.clone()),
    };

    assert!(state.blocks_new_send());
    assert_eq!(state.unknown_target.as_ref(), Some(&binding));

    state.unknown_target = None;
    assert!(!state.blocks_new_send());
}

/// 活跃发送的取消令牌仍占有唯一请求槽，避免同时提交两份命令。
///
/// @author kongweiguang
#[test]
fn active_send_blocks_a_second_submission() {
    let state = TerminalFillState {
        cancellation: Some(CancellationToken::new()),
        unknown_target: None,
    };
    assert!(state.blocks_new_send());
}

/// 无 Tokio 的调用线程等待 helper，由应用 runtime 构造并执行 service future；回写失败不影响调用方完成。
///
/// @author kongweiguang
#[test]
fn snippet_usage_follow_up_uses_application_runtime_and_ignores_failure() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("snippet follow-up runtime should start");
    runtime.block_on(tokio::task::yield_now());
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let evidence = Arc::new(Mutex::new(SnippetUsageRuntimeEvidence::default()));
    assert!(tokio::runtime::Handle::try_current().is_err());

    let builder_evidence = evidence.clone();
    block_on(record_snippet_usage_best_effort(executor, move || {
        let builder_has_tokio_runtime = tokio::runtime::Handle::try_current().is_ok();
        builder_evidence
            .lock()
            .expect("test evidence lock remains available")
            .service_builder_has_tokio_runtime = builder_has_tokio_runtime;

        let service_evidence = builder_evidence;
        async move {
            let runtime_available = tokio::runtime::Handle::try_current().is_ok();
            let blocking_result = if runtime_available {
                tokio::task::spawn_blocking(|| 23_u8).await.ok()
            } else {
                None
            };
            let mut evidence = service_evidence
                .lock()
                .expect("test evidence lock remains available");
            evidence.service_has_tokio_runtime = runtime_available;
            evidence.blocking_result = blocking_result;
            Err(AppError::infrastructure(
                "simulated preference write failure",
            ))
        }
    }));

    let evidence = evidence
        .lock()
        .expect("test evidence lock remains available")
        .clone();
    assert!(evidence.service_builder_has_tokio_runtime);
    assert!(evidence.service_has_tokio_runtime);
    assert_eq!(evidence.blocking_result, Some(23));
}
