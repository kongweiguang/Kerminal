//! 外部片段文件变更必须保留来源、脏稿和根级编辑器可见状态。
//!
//! @author kongweiguang

use kerminal_domain::commands::{SnippetCatalogOrigin, SnippetPreferenceKey};
use kerminal_domain::commands::{SnippetContextBinding, SnippetContextBindingKind};

use crate::features::commands::configuration_watch::{
    SnippetConfigurationRefreshAction, selected_snippet_preference_key,
    snippet_configuration_refresh_action, snippet_context_bindings_match_draft,
};

/// 同 ID 内置片段不随用户文件变更漂移；干净用户片段精确重载且不重新打开 modal。
///
/// @author kongweiguang
#[test]
fn clean_user_refresh_keeps_exact_origin_and_does_not_reopen_editor() {
    let user_key = selected_snippet_preference_key("shared", true);
    let builtin_key = selected_snippet_preference_key("shared", false);
    let changed_paths = vec!["snippets/shared.toml".to_owned()];

    assert_eq!(user_key.origin, SnippetCatalogOrigin::User);
    assert_eq!(builtin_key.origin, SnippetCatalogOrigin::Builtin);
    assert_eq!(
        snippet_configuration_refresh_action(user_key.clone(), &changed_paths, false, false,),
        SnippetConfigurationRefreshAction::ReloadExact {
            key: user_key,
            reopen_editor: false,
        }
    );
    assert_eq!(
        snippet_configuration_refresh_action(builtin_key, &changed_paths, false, false),
        SnippetConfigurationRefreshAction::Ignore
    );
}

/// dirty 草稿在外部重扫期间保持原样；不因标题相同而换成其它来源定义。
///
/// @author kongweiguang
#[test]
fn dirty_user_refresh_preserves_draft_and_unrelated_path_is_ignored() {
    let key = SnippetPreferenceKey {
        origin: SnippetCatalogOrigin::User,
        snippet_id: "shared".to_owned(),
    };

    assert_eq!(
        snippet_configuration_refresh_action(
            key.clone(),
            &["snippets/shared.toml".to_owned()],
            false,
            true,
        ),
        SnippetConfigurationRefreshAction::PreserveDirtyDraft
    );
    assert_eq!(
        snippet_configuration_refresh_action(
            key,
            &["snippets/other.toml".to_owned()],
            false,
            false,
        ),
        SnippetConfigurationRefreshAction::Ignore
    );
}

/// 空列表与编辑器自动插入的默认 Global 项仅表示全局范围，不得造成虚假的未保存状态。
///
/// @author kongweiguang
#[test]
fn implicit_global_binding_matches_omitted_saved_binding() {
    let global = SnippetContextBinding {
        kind: SnippetContextBindingKind::Global,
        target_id: None,
    };

    assert!(snippet_context_bindings_match_draft(
        &[],
        std::slice::from_ref(&global)
    ));
    assert!(snippet_context_bindings_match_draft(
        std::slice::from_ref(&global),
        &[]
    ));
    assert!(snippet_context_bindings_match_draft(&[], &[]));
    assert!(!snippet_context_bindings_match_draft(
        &[],
        &[SnippetContextBinding {
            kind: SnippetContextBindingKind::Host,
            target_id: Some("host-a".to_owned()),
        }],
    ));
}
