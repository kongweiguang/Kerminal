//! 片段页替换、continuation 串行化与迟到回执的真实请求票据边界。
//!
//! @author kongweiguang

use std::collections::HashSet;

use kerminal_application::AppError;
use kerminal_application::commands::{
    SnippetCatalogCursor, SnippetCatalogRevision, SnippetCatalogSection,
    SnippetExecutionCatalogSnapshot,
};
use kerminal_domain::commands::{
    SnippetCatalogItem, SnippetCatalogOrigin, SnippetContextBinding, SnippetContextBindingKind,
    SnippetDefaultAction, SnippetDefinition, SnippetPreference, SnippetPreferenceKey, SnippetRisk,
    SnippetScope, SnippetVariable, SnippetVariableKind, SnippetVariableRenderStrategy,
};
use kerminal_domain::preferences::ConfigRevision;
use tokio_util::sync::CancellationToken;

use super::{
    PendingBuiltinSnippetCopy, SnippetInlinePreview, SnippetPreviewToggleAction,
    clear_snippet_favorite_error, copy_builtin_definition_to_user, finish_snippet_favorite_request,
    is_exact_user_snippet_copy, is_snippet_search_shortcut, pending_builtin_copy_matches_source,
    record_snippet_favorite_error, snippet_copy_snapshot_matches, snippet_preview_copy_is_allowed,
};
use crate::features::commands::SnippetDraftValues;
use crate::features::commands::snippet_browser::{
    SnippetPageSuccess, SnippetPageSuccessState, apply_snippet_page_success,
    begin_snippet_page_request, finish_snippet_page_request, is_current_snippet_page_request,
    record_snippet_page_failure, should_show_snippet_empty_state, snippet_page_is_stale,
    snippet_page_matches_filter,
};
use kerminal_application::commands::VersionedCommandDefinition;

/// 使用真实目录投影覆盖同名不同来源的收藏回执，避免测试依赖展示排序。
///
/// @author kongweiguang
fn favorite_test_item(key: &SnippetPreferenceKey) -> SnippetCatalogItem {
    SnippetCatalogItem {
        id: key.snippet_id.clone(),
        origin: key.origin,
        title: key.snippet_id.clone(),
        description: String::new(),
        template: "echo ready".to_owned(),
        category: "test".to_owned(),
        pack: "test".to_owned(),
        tags: Vec::new(),
        scope: SnippetScope::Any,
        platforms: Vec::new(),
        shells: Vec::new(),
        capabilities: Vec::new(),
        risk: SnippetRisk::Inspect,
        sensitive: false,
        duration: String::new(),
        default_action: SnippetDefaultAction::Insert,
        variables: Vec::new(),
        context_bindings: Vec::new(),
        derived_from: None,
        catalog_version: None,
        command_spec: None,
        owner: None,
        tested_version: None,
        source_name: None,
        source_url: None,
        deprecated: false,
        favorite: false,
        use_count: 0,
        last_used_at_unix_ms: None,
        sort_order: 0,
        updated_at: String::new(),
    }
}

/// 回执使用真实偏好类型，以检查来源和目标收藏值都与写入请求一致。
///
/// @author kongweiguang
fn favorite_test_receipt(key: &SnippetPreferenceKey, favorite: bool) -> SnippetPreference {
    SnippetPreference {
        key: key.clone(),
        favorite,
        use_count: 0,
        last_action: None,
        last_used_at_unix_ms: None,
    }
}

/// 并发收藏的结果不属于目录查询代次；筛选更换后仍必须接收两个精确回执。
///
/// @author kongweiguang
#[test]
fn favorite_receipts_survive_filter_replacement_and_parallel_completion() {
    let builtin = SnippetPreferenceKey {
        origin: SnippetCatalogOrigin::Builtin,
        snippet_id: "same-id".to_owned(),
    };
    let user = SnippetPreferenceKey {
        origin: SnippetCatalogOrigin::User,
        snippet_id: "same-id".to_owned(),
    };
    let mut pending = HashSet::new();
    assert!(pending.insert(builtin.clone()));
    assert!(!pending.insert(builtin.clone()));
    assert!(pending.insert(user.clone()));
    let mut items = vec![favorite_test_item(&builtin), favorite_test_item(&user)];

    let mut page_generation = 0;
    let mut loading = false;
    let mut cancellation = None;
    let first = begin_snippet_page_request(
        &mut page_generation,
        1,
        &mut loading,
        &mut cancellation,
        false,
    )
    .expect("first page request");
    begin_snippet_page_request(
        &mut page_generation,
        2,
        &mut loading,
        &mut cancellation,
        false,
    )
    .expect("replacement page request");
    assert!(first.cancellation.is_cancelled());

    assert!(
        finish_snippet_favorite_request(
            &mut pending,
            &mut items,
            &builtin,
            true,
            Ok(favorite_test_receipt(&builtin, true)),
        )
        .expect("builtin write is still pending")
        .is_ok()
    );
    assert!(items[0].favorite);
    assert!(!items[1].favorite);
    assert!(pending.contains(&user));
    let error = finish_snippet_favorite_request(
        &mut pending,
        &mut items,
        &user,
        true,
        Err(AppError::infrastructure("preference store unavailable")),
    )
    .expect("user write is still pending")
    .expect_err("the failed write must be surfaced");
    assert_eq!(error.public_message(), "preference store unavailable");
    assert!(!items[1].favorite);
    assert!(pending.is_empty());
    assert!(
        finish_snippet_favorite_request(
            &mut pending,
            &mut items,
            &builtin,
            true,
            Ok(favorite_test_receipt(&builtin, true)),
        )
        .is_none()
    );
}

/// 错误来源或错误目标值的回执不能改变任何行，也不能留下永久忙碌状态。
///
/// @author kongweiguang
#[test]
fn favorite_receipt_requires_exact_identity_and_target_value() {
    let builtin = SnippetPreferenceKey {
        origin: SnippetCatalogOrigin::Builtin,
        snippet_id: "same-id".to_owned(),
    };
    let user = SnippetPreferenceKey {
        origin: SnippetCatalogOrigin::User,
        snippet_id: "same-id".to_owned(),
    };
    for receipt in [
        favorite_test_receipt(&user, true),
        favorite_test_receipt(&builtin, false),
    ] {
        let mut pending = HashSet::from([builtin.clone()]);
        let mut items = vec![favorite_test_item(&builtin), favorite_test_item(&user)];
        assert!(
            finish_snippet_favorite_request(&mut pending, &mut items, &builtin, true, Ok(receipt))
                .expect("write is still pending")
                .is_err()
        );
        assert!(pending.is_empty());
        assert!(items.iter().all(|item| !item.favorite));
    }
}

/// 多个片段的失败要同时保留；重试一项或其他功能更新状态时不能吞掉其余反馈。
///
/// @author kongweiguang
#[test]
fn favorite_errors_are_independent_and_preserve_unrelated_status() {
    let first = SnippetPreferenceKey {
        origin: SnippetCatalogOrigin::Builtin,
        snippet_id: "first".to_owned(),
    };
    let second = SnippetPreferenceKey {
        origin: SnippetCatalogOrigin::User,
        snippet_id: "second".to_owned(),
    };
    let mut errors = Vec::new();
    let mut status = None;
    record_snippet_favorite_error(
        &mut errors,
        &mut status,
        first.clone(),
        "first failed".into(),
    );
    record_snippet_favorite_error(
        &mut errors,
        &mut status,
        second.clone(),
        "second failed".into(),
    );
    assert_eq!(status.as_deref(), Some("first failed；second failed"));
    clear_snippet_favorite_error(&mut errors, &mut status, &first);
    assert_eq!(status.as_deref(), Some("second failed"));
    status = Some("another operation needs attention".to_owned());
    clear_snippet_favorite_error(&mut errors, &mut status, &second);
    assert_eq!(status.as_deref(), Some("another operation needs attention"));
    assert!(errors.is_empty());
}

/// 同一已选片段只切换原位投影，运行变量草稿不触发跨片段保存门禁。
///
/// @author kongweiguang
#[test]
fn selected_snippet_reopens_inline_preview_without_changing_its_key() {
    let key = SnippetPreferenceKey {
        origin: SnippetCatalogOrigin::User,
        snippet_id: "user-disk-usage".to_owned(),
    };
    let collapsed = SnippetInlinePreview::Collapsed;
    assert_eq!(
        collapsed.toggle_action(&key, true),
        SnippetPreviewToggleAction::RevealSelected
    );

    let ready = SnippetInlinePreview::Ready(key.clone());
    assert_eq!(
        ready.toggle_action(&key, true),
        SnippetPreviewToggleAction::Collapse
    );
}

/// 区分片段搜索入口与需要由控件自身处理的普通或带额外修饰键输入。
///
/// @author kongweiguang
#[test]
fn snippet_search_shortcut_accepts_local_search_keys_only() {
    assert!(is_snippet_search_shortcut("/", false, false, false, false));
    assert!(is_snippet_search_shortcut("f", true, false, false, false));
    assert!(is_snippet_search_shortcut("F", false, true, false, false));
    assert!(!is_snippet_search_shortcut("/", true, false, false, false));
    assert!(!is_snippet_search_shortcut("f", false, false, false, false));
    assert!(!is_snippet_search_shortcut("f", true, false, true, false));
    assert!(!is_snippet_search_shortcut("f", true, false, false, true));
}

/// 复制模板前要求异步读回仍是界面展示的用户版本与定义。
///
/// @author kongweiguang
#[test]
fn template_copy_rejects_changed_user_revision() {
    let key = SnippetPreferenceKey {
        origin: SnippetCatalogOrigin::User,
        snippet_id: "user-inspection".to_owned(),
    };
    let definition = SnippetDefinition {
        id: key.snippet_id.clone(),
        title: "Inspect".to_owned(),
        command: "echo {{name}}".to_owned(),
        description: None,
        tags: Vec::new(),
        sort_order: 0,
        scope: SnippetScope::Any,
        category: None,
        risk: SnippetRisk::Inspect,
        default_action: SnippetDefaultAction::Insert,
        variables: Vec::new(),
        context_bindings: Vec::new(),
        derived_from: None,
    };
    let revision = ConfigRevision::from_sha256("revision-one".to_owned());
    let snapshot = SnippetExecutionCatalogSnapshot {
        key: key.clone(),
        definition: definition.clone(),
        platforms: Vec::new(),
        shells: Vec::new(),
        revision: SnippetCatalogRevision::User(revision.clone()),
    };

    assert!(snippet_copy_snapshot_matches(
        &snapshot,
        &key,
        &definition,
        Some(&revision),
    ));
    assert!(!snippet_copy_snapshot_matches(
        &snapshot,
        &key,
        &definition,
        Some(&ConfigRevision::from_sha256("revision-two".to_owned())),
    ));
}

/// 预览复制门禁允许普通命令，并拒绝任何带敏感值的展开结果。
///
/// @author kongweiguang
#[test]
fn rendered_snippet_copy_rejects_sensitive_values() {
    assert!(snippet_preview_copy_is_allowed(false));
    assert!(!snippet_preview_copy_is_allowed(true));
}

/// 收起或改变来源后，迟到的 exact lookup 不能重新展开或借用同 ID 条目。
///
/// @author kongweiguang
#[test]
fn collapsed_or_other_origin_preview_rejects_late_lookup_receipt() {
    let user_key = SnippetPreferenceKey {
        origin: SnippetCatalogOrigin::User,
        snippet_id: "same-id".to_owned(),
    };
    let builtin_key = SnippetPreferenceKey {
        origin: SnippetCatalogOrigin::Builtin,
        snippet_id: "same-id".to_owned(),
    };
    let mut loading = SnippetInlinePreview::Loading(builtin_key.clone());

    assert!(!loading.is_loading_key(&user_key));
    assert!(!loading.finish_loading(&user_key, Ok(())));
    assert_eq!(loading, SnippetInlinePreview::Loading(builtin_key.clone()));

    loading = SnippetInlinePreview::Collapsed;
    assert!(!loading.is_loading_key(&builtin_key));
    assert!(!loading.finish_loading(&builtin_key, Ok(())));
    assert_eq!(loading, SnippetInlinePreview::Collapsed);
}

/// 新搜索和区段切换共用替换语义；旧回执不能清 loading 或覆盖最新页。
///
/// @author kongweiguang
#[test]
fn query_and_section_replacements_cancel_old_pages_and_reject_late_receipts() {
    let mut page_generation = 0;
    let mut query_generation = 1;
    let mut loading = false;
    let mut active_cancellation: Option<CancellationToken> = None;
    let mut page_items = vec!["previous page"];

    let initial = begin_snippet_page_request(
        &mut page_generation,
        query_generation,
        &mut loading,
        &mut active_cancellation,
        false,
    )
    .expect("first page starts");
    query_generation += 1;
    let search = begin_snippet_page_request(
        &mut page_generation,
        query_generation,
        &mut loading,
        &mut active_cancellation,
        false,
    )
    .expect("changed query replaces in-flight page");
    assert!(initial.cancellation.is_cancelled());
    query_generation += 1;
    let section = begin_snippet_page_request(
        &mut page_generation,
        query_generation,
        &mut loading,
        &mut active_cancellation,
        false,
    )
    .expect("changed section replaces in-flight query");
    assert!(search.cancellation.is_cancelled());
    assert!(loading);

    let late_items = vec!["stale page"];
    if finish_snippet_page_request(
        &search,
        page_generation,
        query_generation,
        &mut loading,
        &mut active_cancellation,
    ) {
        page_items = late_items;
    }
    assert_eq!(page_items, ["previous page"]);
    assert!(
        loading,
        "a stale receipt must not release the new request slot"
    );
    assert!(is_current_snippet_page_request(
        &section,
        page_generation,
        query_generation
    ));

    if finish_snippet_page_request(
        &section,
        page_generation,
        query_generation,
        &mut loading,
        &mut active_cancellation,
    ) {
        page_items = vec!["current section"];
    }
    assert_eq!(page_items, ["current section"]);
    assert!(!loading);
}

/// 加载更多只允许单个 continuation；重复请求不能取消当前 continuation。
///
/// @author kongweiguang
#[test]
fn continuation_is_deduplicated_without_cancelling_the_active_page() {
    let mut page_generation = 0;
    let query_generation = 5;
    let mut loading = false;
    let mut active_cancellation: Option<CancellationToken> = None;
    let first_page = begin_snippet_page_request(
        &mut page_generation,
        query_generation,
        &mut loading,
        &mut active_cancellation,
        false,
    )
    .expect("first page starts");
    assert!(finish_snippet_page_request(
        &first_page,
        page_generation,
        query_generation,
        &mut loading,
        &mut active_cancellation,
    ));

    let continuation = begin_snippet_page_request(
        &mut page_generation,
        query_generation,
        &mut loading,
        &mut active_cancellation,
        true,
    )
    .expect("one continuation starts");
    let generation = page_generation;
    assert!(
        begin_snippet_page_request(
            &mut page_generation,
            query_generation,
            &mut loading,
            &mut active_cancellation,
            true,
        )
        .is_none()
    );
    assert!(!continuation.cancellation.is_cancelled());
    assert_eq!(page_generation, generation);
    assert!(loading);
}

/// 目录读取失败保留最近成功结果，同时将其标为旧筛选投影而非真实空目录。
///
/// @author kongweiguang
#[test]
fn failed_catalog_query_keeps_last_success_and_exposes_retryable_error() {
    let items = vec!["previous common item"];
    let total_count = 1;
    let loaded_section = Some(SnippetCatalogSection::Common);
    let loaded_query = Some(String::new());
    let mut page_error = None;
    let mut retry_cursor = None;

    assert!(!snippet_page_matches_filter(
        loaded_section,
        loaded_query.as_deref(),
        SnippetCatalogSection::Library,
        ""
    ));
    assert!(snippet_page_is_stale(
        loaded_section,
        loaded_query.as_deref(),
        SnippetCatalogSection::Library,
        "",
        true,
        false,
    ));
    record_snippet_page_failure(
        &mut page_error,
        &mut retry_cursor,
        "catalog temporarily unavailable".to_owned(),
        None,
    );

    assert_eq!(items, ["previous common item"]);
    assert_eq!(total_count, 1);
    assert_eq!(loaded_section, Some(SnippetCatalogSection::Common));
    assert_eq!(loaded_query.as_deref(), Some(""));
    assert_eq!(
        page_error.as_deref(),
        Some("catalog temporarily unavailable")
    );
    assert!(retry_cursor.is_none());
    assert!(!should_show_snippet_empty_state(
        items.len(),
        false,
        page_error.is_some(),
        false,
    ));
}

/// continuation 失败时保留成功前页，并把重试定位在同一个 cursor 上。
///
/// @author kongweiguang
#[test]
fn failed_continuation_preserves_page_and_exact_retry_cursor() {
    let items = vec!["first page item"];
    let total_count = 101;
    let next_cursor: Option<SnippetCatalogCursor> = Some(
        serde_json::from_str(r#"{"offset":100,"fingerprint":42}"#)
            .expect("opaque cursor fixture is valid"),
    );
    let retry_target = next_cursor.clone();
    let mut error = None;
    let mut retry_cursor = None;

    record_snippet_page_failure(
        &mut error,
        &mut retry_cursor,
        "continuation failed".to_owned(),
        retry_target.clone(),
    );

    assert_eq!(items, ["first page item"]);
    assert_eq!(total_count, 101);
    assert_eq!(next_cursor, retry_target);
    assert_eq!(retry_cursor, retry_target);
    assert_eq!(error.as_deref(), Some("continuation failed"));
    assert!(!snippet_page_is_stale(
        Some(SnippetCatalogSection::Common),
        Some(""),
        SnippetCatalogSection::Common,
        "",
        true,
        retry_cursor.is_some(),
    ));
    assert!(!should_show_snippet_empty_state(
        items.len(),
        false,
        error.is_some(),
        true,
    ));
}

/// 只有匹配当前筛选的成功空页才能显示空态，避免输入搜索词后瞬间出现伪空结果。
///
/// @author kongweiguang
#[test]
fn empty_state_requires_a_successful_page_for_current_filter() {
    assert!(!should_show_snippet_empty_state(0, false, false, false));
    assert!(!should_show_snippet_empty_state(0, true, false, true));
    assert!(!should_show_snippet_empty_state(0, false, true, true));
    assert!(should_show_snippet_empty_state(0, false, false, true));
}

/// 成功的新筛选结果原子替换旧页并清除错误，旧筛选不会残留在身份元数据中。
///
/// @author kongweiguang
#[test]
fn successful_replacement_updates_loaded_filter_and_clears_error() {
    let mut items = vec!["stale item"];
    let mut total_count = 1;
    let mut next_cursor = None;
    let mut loaded_section = Some(SnippetCatalogSection::Common);
    let mut loaded_query = Some(String::new());
    let mut page_error = Some("old failure".to_owned());
    let mut retry_cursor = None;

    apply_snippet_page_success(
        SnippetPageSuccessState {
            items: &mut items,
            total_count: &mut total_count,
            next_cursor: &mut next_cursor,
            loaded_section: &mut loaded_section,
            loaded_query: &mut loaded_query,
            page_error: &mut page_error,
            retry_cursor: &mut retry_cursor,
        },
        SnippetPageSuccess {
            items: vec!["current library item"],
            total: 1,
            next_cursor: None,
            section: SnippetCatalogSection::Library,
            query: "disk".to_owned(),
            append: false,
        },
    );

    assert_eq!(items, ["current library item"]);
    assert_eq!(total_count, 1);
    assert!(next_cursor.is_none());
    assert_eq!(loaded_section, Some(SnippetCatalogSection::Library));
    assert_eq!(loaded_query.as_deref(), Some("disk"));
    assert!(page_error.is_none());
    assert!(retry_cursor.is_none());
}

/// 另存内置定义必须得到独立身份，保留模板元数据，并再次清除敏感默认值。
///
/// @author kongweiguang
#[test]
fn saving_builtin_as_user_preserves_source_and_sanitizes_sensitive_defaults() {
    let source = SnippetDefinition {
        id: "builtin:docker-inspect".to_owned(),
        title: "Inspect container".to_owned(),
        command: "docker inspect {{container}}".to_owned(),
        description: Some("Read container metadata".to_owned()),
        tags: vec!["docker".to_owned(), "inspect".to_owned()],
        sort_order: 12,
        scope: SnippetScope::Ssh,
        category: Some("containers".to_owned()),
        risk: SnippetRisk::Inspect,
        default_action: SnippetDefaultAction::Run,
        variables: vec![
            SnippetVariable {
                name: "container".to_owned(),
                label: "Container".to_owned(),
                description: "Container name or ID".to_owned(),
                kind: SnippetVariableKind::Container,
                required: true,
                default_value: Some("web".to_owned()),
                suggestions: vec!["web".to_owned()],
                validation: Some("^[a-z0-9_-]+$".to_owned()),
                render_strategy: SnippetVariableRenderStrategy::ShellArg,
                sensitive: false,
            },
            SnippetVariable {
                name: "token".to_owned(),
                label: "Token".to_owned(),
                description: "Optional access token".to_owned(),
                kind: SnippetVariableKind::Secret,
                required: false,
                default_value: None,
                suggestions: Vec::new(),
                validation: None,
                render_strategy: SnippetVariableRenderStrategy::ShellArg,
                sensitive: true,
            },
        ],
        context_bindings: vec![SnippetContextBinding {
            kind: SnippetContextBindingKind::Host,
            target_id: Some("host-prod".to_owned()),
        }],
        derived_from: None,
    };
    let source_before = source.clone();
    let mut variables = source.variables.clone();
    variables[1].default_value = Some("must-not-persist".to_owned());
    variables[1].suggestions = vec!["also-sensitive".to_owned()];
    let draft = SnippetDraftValues {
        title: "Inspect container copy".to_owned(),
        command: source.command.clone(),
        description: source.description.clone(),
        tags: source.tags.clone(),
        sort_order: source.sort_order,
        scope: source.scope,
        category: source.category.clone(),
        risk: source.risk,
        default_action: source.default_action,
        variables,
        context_bindings: source.context_bindings.clone(),
    };

    let copied = copy_builtin_definition_to_user(&source, draft, "user-copy-uuid".to_owned());

    assert_ne!(copied.id, source.id);
    assert_eq!(copied.derived_from.as_deref(), Some(source.id.as_str()));
    assert_eq!(copied.title, "Inspect container copy");
    assert_eq!(copied.command, source.command);
    assert_eq!(copied.description, source.description);
    assert_eq!(copied.tags, source.tags);
    assert_eq!(copied.sort_order, source.sort_order);
    assert_eq!(copied.scope, source.scope);
    assert_eq!(copied.category, source.category);
    assert_eq!(copied.risk, source.risk);
    assert_eq!(copied.default_action, source.default_action);
    assert_eq!(copied.context_bindings, source.context_bindings);
    assert_eq!(copied.variables[0], source.variables[0]);
    assert_eq!(copied.variables[1].name, "token");
    assert_eq!(copied.variables[1].description, "Optional access token");
    assert_eq!(copied.variables[1].default_value, None);
    assert!(copied.variables[1].suggestions.is_empty());
    assert!(copied.variables[1].sensitive);
    assert_eq!(copied.variables[1].kind, SnippetVariableKind::Secret);
    assert_eq!(
        source, source_before,
        "the built-in definition stays untouched"
    );

    let source_key = SnippetPreferenceKey {
        origin: SnippetCatalogOrigin::Builtin,
        snippet_id: source.id.clone(),
    };
    let pending = PendingBuiltinSnippetCopy {
        source_key: source_key.clone(),
        expected_definition: copied.clone(),
    };
    assert!(pending_builtin_copy_matches_source(&pending, &source_key));
    assert!(!pending_builtin_copy_matches_source(
        &pending,
        &SnippetPreferenceKey {
            origin: SnippetCatalogOrigin::Builtin,
            snippet_id: "another-builtin".to_owned(),
        }
    ));
    assert!(!pending_builtin_copy_matches_source(
        &pending,
        &SnippetPreferenceKey {
            origin: SnippetCatalogOrigin::User,
            snippet_id: source.id.clone(),
        }
    ));

    let exact_readback = VersionedCommandDefinition {
        definition: copied.clone(),
        revision: Some(ConfigRevision::from_sha256("known-revision".to_owned())),
    };
    assert!(is_exact_user_snippet_copy(&copied, &exact_readback));
    assert!(!is_exact_user_snippet_copy(
        &copied,
        &VersionedCommandDefinition {
            definition: copied.clone(),
            revision: None,
        }
    ));
    let mut different = copied.clone();
    different.command.push_str("; echo changed");
    assert!(!is_exact_user_snippet_copy(
        &copied,
        &VersionedCommandDefinition {
            definition: different,
            revision: Some(ConfigRevision::from_sha256("other-revision".to_owned())),
        }
    ));
}
