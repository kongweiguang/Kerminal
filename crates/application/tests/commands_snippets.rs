//! 片段渲染在拼接命令前执行变量校验和目标 shell 转义。
//!
//! @author kongweiguang

use std::collections::BTreeMap;

use kerminal_application::commands::{SnippetRenderDialect, render_snippet_definition};
use kerminal_domain::commands::{
    SnippetDefaultAction, SnippetDefinition, SnippetRisk, SnippetScope, SnippetVariable,
    SnippetVariableKind, SnippetVariableRenderStrategy,
};
use kerminal_domain::connections::RemoteShellDialect;

/// POSIX 路径值始终作为一个参数引用，空格不会变成命令分隔符。
#[test]
fn render_quotes_shell_argument() {
    let definition = snippet(SnippetVariableRenderStrategy::ShellArg);
    let values = BTreeMap::from([("path".to_owned(), "folder with spaces".to_owned())]);

    let result = render_snippet_definition(&definition, &values, SnippetRenderDialect::Posix)
        .expect("valid path should render");

    assert_eq!(result.command, "echo 'folder with spaces'");
    assert!(!result.sensitive);
}

/// `ValidatedRaw` 拒绝命令分隔符，即使定义遗漏了额外的正则规则也不会执行注入文本。
#[test]
fn validated_raw_rejects_shell_metacharacters() {
    let definition = snippet(SnippetVariableRenderStrategy::ValidatedRaw);
    let values = BTreeMap::from([("path".to_owned(), "value; touch marker".to_owned())]);

    assert!(render_snippet_definition(&definition, &values, SnippetRenderDialect::Posix).is_err());
}

/// 必填变量缺失时拒绝生成可执行命令，避免把模板占位符留给 shell 展开。
#[test]
fn render_rejects_missing_required_variable() {
    let definition = snippet(SnippetVariableRenderStrategy::ShellArg);

    assert!(
        render_snippet_definition(&definition, &BTreeMap::new(), SnippetRenderDialect::Posix)
            .is_err()
    );
}

/// Fish 单引号中的反斜杠和单引号按方言引用，命令替换语法仍保持字面值。
///
/// @author kongweiguang
#[test]
fn fish_argument_quotes_backslash_quote_and_substitution() {
    let definition = snippet(SnippetVariableRenderStrategy::ShellArg);
    let values = BTreeMap::from([("path".to_owned(), "目录\\子项'$(touch marker)".to_owned())]);

    let rendered = render_snippet_definition(&definition, &values, SnippetRenderDialect::Fish)
        .expect("Fish path argument should be rendered");

    assert_eq!(rendered.command, r"echo '目录\\子项\'$(touch marker)'");
}

/// Path 是值类型而 ShellArg 是渲染策略；两者都不放宽旧契约对行分隔和 NUL 的拒绝。
///
/// @author kongweiguang
#[test]
fn shell_argument_path_rejects_line_and_nul_controls() {
    let definition = snippet(SnippetVariableRenderStrategy::ShellArg);

    for (label, control) in [("LF", "\n"), ("CR", "\r"), ("NUL", "\0")] {
        let values = BTreeMap::from([("path".to_owned(), format!("目录{control}子项"))]);
        let result = render_snippet_definition(&definition, &values, SnippetRenderDialect::Fish);

        assert!(
            result.is_err(),
            "{label} must remain invalid for a path argument"
        );
    }
}

/// 未确认的 shell 方言不能回退到 POSIX quoting，避免错误转义形成可执行注入。
///
/// @author kongweiguang
#[test]
fn unknown_shell_dialect_is_rejected() {
    assert!(SnippetRenderDialect::from_trusted_shell(RemoteShellDialect::Unknown).is_err());
}

/// 构造带一个路径参数的最小合法片段，固定测试目标只需改变渲染策略。
fn snippet(render_strategy: SnippetVariableRenderStrategy) -> SnippetDefinition {
    SnippetDefinition {
        id: "echo-path".to_owned(),
        title: "输出路径".to_owned(),
        command: "echo {{path}}".to_owned(),
        description: None,
        tags: Vec::new(),
        sort_order: 0,
        scope: SnippetScope::Any,
        category: None,
        risk: SnippetRisk::Inspect,
        default_action: SnippetDefaultAction::Insert,
        variables: vec![SnippetVariable {
            name: "path".to_owned(),
            label: "路径".to_owned(),
            description: String::new(),
            kind: SnippetVariableKind::Path,
            required: true,
            default_value: None,
            suggestions: Vec::new(),
            validation: None,
            render_strategy,
            sensitive: false,
        }],
        context_bindings: Vec::new(),
        derived_from: None,
    }
}
