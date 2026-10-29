//! 片段变量编辑转换必须维持领域允许的转义策略与敏感值约束。
//!
//! @author kongweiguang

use kerminal_domain::commands::{
    SnippetVariable, SnippetVariableKind, SnippetVariableRenderStrategy,
};

use super::{
    snippet_template_variable_names, supports_validated_raw, transition_snippet_variable_kind,
};

/// compact 编辑器沿模板首次出现顺序同步参数，并只保留一份同名定义。
///
/// @author kongweiguang
#[test]
fn compact_template_variables_follow_unique_placeholder_names() {
    assert_eq!(
        snippet_template_variable_names("echo {{ host }} {{port}} {{host}}"),
        vec!["host".to_owned(), "port".to_owned()]
    );
}

/// 无效名称不生成持久定义，且嵌套花括号遵循 main 的搜索匹配行为。
///
/// @author kongweiguang
#[test]
fn compact_template_variables_skip_invalid_placeholders() {
    assert_eq!(
        snippet_template_variable_names("{{_bad}} {{bad-name}} {{{valid_2}}}"),
        vec!["valid_2".to_owned()]
    );
}

/// 构造会覆盖定义级敏感字段和原始插值的变量，验证 UI 转换使用同一领域规则。
///
/// @author kongweiguang
fn variable(
    kind: SnippetVariableKind,
    render_strategy: SnippetVariableRenderStrategy,
) -> SnippetVariable {
    SnippetVariable {
        name: "target".to_owned(),
        label: "Target".to_owned(),
        description: String::new(),
        kind,
        required: false,
        default_value: Some("persisted value".to_owned()),
        suggestions: vec!["first".to_owned(), "second".to_owned()],
        validation: Some("^[a-z]+$".to_owned()),
        render_strategy,
        sensitive: false,
    }
}

/// Secret 切换必须立即清除默认值和建议，切回普通类型也不能恢复旧内容。
///
/// @author kongweiguang
#[test]
fn secret_transition_removes_persistable_values_permanently() {
    let initial = variable(
        SnippetVariableKind::Port,
        SnippetVariableRenderStrategy::ValidatedRaw,
    );
    let secret = transition_snippet_variable_kind(initial, SnippetVariableKind::Secret);

    assert_eq!(secret.kind, SnippetVariableKind::Secret);
    assert!(secret.sensitive);
    assert_eq!(secret.default_value, None);
    assert!(secret.suggestions.is_empty());
    assert_eq!(
        secret.render_strategy,
        SnippetVariableRenderStrategy::ShellArg
    );

    let text = transition_snippet_variable_kind(secret, SnippetVariableKind::Text);
    assert_eq!(text.kind, SnippetVariableKind::Text);
    assert!(!text.sensitive);
    assert_eq!(text.default_value, None);
    assert!(text.suggestions.is_empty());
}

/// ValidatedRaw 与领域允许集保持一致，其它类型切换后退回安全的 ShellArg。
///
/// @author kongweiguang
#[test]
fn validated_raw_is_limited_to_domain_supported_kinds() {
    let supported = [
        SnippetVariableKind::Port,
        SnippetVariableKind::Integer,
        SnippetVariableKind::Host,
        SnippetVariableKind::Service,
        SnippetVariableKind::Container,
        SnippetVariableKind::Enum,
    ];
    let unsupported = [
        SnippetVariableKind::Text,
        SnippetVariableKind::Path,
        SnippetVariableKind::Url,
        SnippetVariableKind::Secret,
        SnippetVariableKind::Raw,
    ];

    assert!(supported.into_iter().all(supports_validated_raw));
    assert!(
        unsupported
            .into_iter()
            .all(|kind| !supports_validated_raw(kind))
    );

    let transitioned = transition_snippet_variable_kind(
        variable(
            SnippetVariableKind::Port,
            SnippetVariableRenderStrategy::ValidatedRaw,
        ),
        SnippetVariableKind::Path,
    );
    assert_eq!(
        transitioned.render_strategy,
        SnippetVariableRenderStrategy::ShellArg
    );
}
