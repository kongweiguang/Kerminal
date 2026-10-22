//! Fill 回执只可更新仍匹配的命令草稿、变量和编辑器代次。
//!
//! @author kongweiguang

use std::collections::BTreeMap;

use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    commands::{
        SnippetCatalogItem, SnippetCatalogOrigin, SnippetDefaultAction, SnippetRisk, SnippetScope,
        SnippetVariable, SnippetVariableKind, SnippetVariableRenderStrategy,
    },
    identity::{PaneId, SessionId, TabId},
    sessions::SessionOwner,
};

use super::snippet_fill::snippet_fill_completion_matches_current;
use super::{
    CommandsSection, SnippetDraftValues, SnippetFillGuard, SnippetTerminalFillCompleted,
    snippet_definition_from_catalog,
};

/// 来源展示信息不能伪装为派生身份，否则内置片段会在填入的精确版本校验中被误拒绝。
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

/// 构造一次受明确 UI 目标与编辑器草稿保护的填入请求。
///
/// @author kongweiguang
fn guard() -> SnippetFillGuard {
    SnippetFillGuard {
        request_generation: 3,
        session_view_generation: 5,
        session_id: Some(SessionId::parse("session-main").expect("valid session id")),
        binding: Some(UiSessionBinding {
            session_id: SessionId::parse("session-main").expect("valid session id"),
            owner: SessionOwner::Ui {
                tab_id: TabId::parse("tab-main").expect("valid tab id"),
                pane_id: PaneId::parse("pane-main").expect("valid pane id"),
            },
            session_generation: 8,
        }),
        section: CommandsSection::Snippets,
        snippet_generation: 11,
        editor_generation: 4,
        editor_open: true,
        selected_sensitive: false,
        command_draft: "echo before".to_owned(),
        definition_draft: SnippetDraftValues {
            title: "Inspect status".to_owned(),
            command: "echo $name".to_owned(),
            description: None,
            tags: Vec::new(),
            sort_order: 0,
            scope: SnippetScope::Any,
            category: None,
            risk: SnippetRisk::Unknown,
            default_action: SnippetDefaultAction::Insert,
            variables: vec![SnippetVariable {
                name: "name".to_owned(),
                label: "Name".to_owned(),
                description: String::new(),
                kind: SnippetVariableKind::Text,
                required: true,
                default_value: None,
                suggestions: Vec::new(),
                validation: None,
                render_strategy: SnippetVariableRenderStrategy::ShellArg,
                sensitive: false,
            }],
            context_bindings: Vec::new(),
        },
        variable_values: BTreeMap::from([("name".to_owned(), "before".to_owned())]),
    }
}

/// 目标、片段代次或 modal 重开后，旧回执不能改变新的编辑器可见状态。
///
/// @author kongweiguang
#[test]
fn fill_receipt_requires_the_original_target_snippet_and_editor_generation() {
    let original = guard();
    assert!(original.same_request_context(&guard()));

    let mut changed_target = guard();
    changed_target.binding.as_mut().unwrap().session_generation += 1;
    assert!(!original.same_request_context(&changed_target));

    let mut changed_snippet = guard();
    changed_snippet.snippet_generation += 1;
    assert!(!original.same_request_context(&changed_snippet));

    let mut reopened_editor = guard();
    reopened_editor.editor_generation += 1;
    assert!(!original.same_request_context(&reopened_editor));

    let mut superseded_fill = guard();
    superseded_fill.request_generation += 1;
    assert!(!original.same_request_context(&superseded_fill));

    let mut changed_section = guard();
    changed_section.section = CommandsSection::History;
    assert!(!original.same_request_context(&changed_section));
}

/// 用户在异步渲染期间修改命令或变量时，迟到结果不得回填旧快照。
///
/// @author kongweiguang
#[test]
fn fill_receipt_rejects_changed_definition_command_and_runtime_variables() {
    let original = guard();
    assert!(original.drafts_unchanged(&guard()));

    let mut changed_command = guard();
    changed_command.command_draft = "echo newer".to_owned();
    assert!(!original.drafts_unchanged(&changed_command));

    let mut changed_variable = guard();
    changed_variable
        .variable_values
        .insert("name".to_owned(), "newer".to_owned());
    assert!(!original.drafts_unchanged(&changed_variable));

    let mut changed_definition = guard();
    changed_definition.definition_draft.command = "echo $other".to_owned();
    assert!(!original.drafts_unchanged(&changed_definition));

    let mut changed_variable_definition = guard();
    changed_variable_definition.definition_draft.variables[0].kind = SnippetVariableKind::Secret;
    changed_variable_definition.definition_draft.variables[0].sensitive = true;
    assert!(!original.drafts_unchanged(&changed_variable_definition));
}

/// 延迟焦点回执必须匹配同一完整绑定、Fill 请求和右栏视图代次。
///
/// @author kongweiguang
#[test]
fn terminal_fill_completion_rejects_stale_focus_intents() {
    let request = guard();
    let binding = request.binding.clone().expect("UI fill captures a binding");
    let completion = SnippetTerminalFillCompleted {
        binding: binding.clone(),
        snippet_fill_generation: request.request_generation,
        session_view_generation: request.session_view_generation,
        expected_focus: None,
        snippet_editor_generation: None,
    };

    assert!(snippet_fill_completion_matches_current(
        &completion,
        Some(&binding),
        request.request_generation,
        request.session_view_generation,
        None,
    ));
    assert!(!snippet_fill_completion_matches_current(
        &completion,
        None,
        request.request_generation,
        request.session_view_generation,
        None,
    ));
    assert!(!snippet_fill_completion_matches_current(
        &completion,
        Some(&binding),
        request.request_generation + 1,
        request.session_view_generation,
        None,
    ));
    assert!(!snippet_fill_completion_matches_current(
        &completion,
        Some(&binding),
        request.request_generation,
        request.session_view_generation + 1,
        None,
    ));

    let mut reconnected = binding;
    reconnected.session_generation += 1;
    assert!(!snippet_fill_completion_matches_current(
        &completion,
        Some(&reconnected),
        request.request_generation,
        request.session_view_generation,
        None,
    ));
}

/// 已关闭再打开的编辑器即使复用同一输入框焦点，也不接受旧 Fill 的关闭意图。
///
/// @author kongweiguang
#[test]
fn terminal_fill_completion_cannot_close_a_reopened_editor() {
    let request = guard();
    let binding = request.binding.clone().expect("UI fill captures a binding");
    let mut completion = SnippetTerminalFillCompleted {
        binding: binding.clone(),
        snippet_fill_generation: request.request_generation,
        session_view_generation: request.session_view_generation,
        expected_focus: None,
        snippet_editor_generation: Some(7),
    };
    let current = |event: &SnippetTerminalFillCompleted, generation| {
        snippet_fill_completion_matches_current(
            event,
            Some(&binding),
            request.request_generation,
            request.session_view_generation,
            generation,
        )
    };
    assert!(current(&completion, Some(7)));
    assert!(!current(&completion, Some(8)));
    assert!(!current(&completion, None));
    completion.snippet_editor_generation = None;
    assert!(!current(&completion, Some(7)));
}
