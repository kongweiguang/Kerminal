//! 内置片段适用性只按目录声明与可信 shell 方言判断。
//!
//! @author kongweiguang

use kerminal_domain::{
    commands::{
        SnippetApplicabilityReason, SnippetApplicabilityStatus, SnippetCatalogOrigin,
        assess_snippet_applicability,
    },
    connections::RemoteShellDialect,
};

/// 固定轻量目录声明，避免规则测试依赖运行环境或完整片段目录。
fn declarations(platforms: &[&str], shells: &[&str]) -> (Vec<String>, Vec<String>) {
    (
        platforms.iter().map(|value| (*value).to_owned()).collect(),
        shells.iter().map(|value| (*value).to_owned()).collect(),
    )
}

#[test]
fn named_shell_mismatch_is_rejected_but_posix_cannot_identify_bash_or_zsh() {
    let (platforms, windows_shells) = declarations(&[], &["powershell"]);
    let mismatch = assess_snippet_applicability(
        SnippetCatalogOrigin::Builtin,
        &platforms,
        &windows_shells,
        Some(RemoteShellDialect::Posix),
    );
    assert_eq!(mismatch.status, SnippetApplicabilityStatus::Incompatible);
    assert_eq!(
        mismatch.reason,
        Some(SnippetApplicabilityReason::ShellDialectMismatch)
    );

    let (platforms, posix_shells) = declarations(&[], &["bash", "zsh"]);
    let ambiguous = assess_snippet_applicability(
        SnippetCatalogOrigin::Builtin,
        &platforms,
        &posix_shells,
        Some(RemoteShellDialect::Posix),
    );
    assert_eq!(ambiguous.status, SnippetApplicabilityStatus::Unknown);
    assert_eq!(
        ambiguous.reason,
        Some(SnippetApplicabilityReason::ExactShellNotReported)
    );
}

#[test]
fn exact_named_shell_match_is_compatible_when_platform_is_unrestricted() {
    let (platforms, shells) = declarations(&[], &["Fish"]);
    let result = assess_snippet_applicability(
        SnippetCatalogOrigin::Builtin,
        &platforms,
        &shells,
        Some(RemoteShellDialect::Fish),
    );
    assert_eq!(result.status, SnippetApplicabilityStatus::Compatible);
    assert_eq!(result.reason, None);
}

#[test]
fn platform_declaration_stays_unknown_without_trusted_platform_metadata() {
    let (platforms, shells) = declarations(&["linux", "macos"], &["fish"]);
    let result = assess_snippet_applicability(
        SnippetCatalogOrigin::Builtin,
        &platforms,
        &shells,
        Some(RemoteShellDialect::Fish),
    );
    assert_eq!(result.status, SnippetApplicabilityStatus::Unknown);
    assert_eq!(
        result.reason,
        Some(SnippetApplicabilityReason::PlatformNotReported)
    );
}

#[test]
fn missing_target_is_unknown_and_user_definitions_do_not_inherit_builtin_limits() {
    let (platforms, shells) = declarations(&["linux"], &["bash", "zsh", "fish"]);
    let missing_target =
        assess_snippet_applicability(SnippetCatalogOrigin::Builtin, &platforms, &shells, None);
    assert_eq!(missing_target.status, SnippetApplicabilityStatus::Unknown);
    assert_eq!(
        missing_target.reason,
        Some(SnippetApplicabilityReason::TargetNotAvailable)
    );

    let unavailable_shell = assess_snippet_applicability(
        SnippetCatalogOrigin::Builtin,
        &[],
        &["fish".to_owned()],
        Some(RemoteShellDialect::Unknown),
    );
    assert_eq!(
        unavailable_shell.status,
        SnippetApplicabilityStatus::Unknown
    );
    assert_eq!(
        unavailable_shell.reason,
        Some(SnippetApplicabilityReason::ExactShellNotReported)
    );

    let no_constraints =
        assess_snippet_applicability(SnippetCatalogOrigin::Builtin, &[], &[], None);
    assert_eq!(
        no_constraints.status,
        SnippetApplicabilityStatus::Unrestricted
    );
    assert_eq!(no_constraints.reason, None);

    let copied_user_definition = assess_snippet_applicability(
        SnippetCatalogOrigin::User,
        &platforms,
        &shells,
        Some(RemoteShellDialect::PowerShell),
    );
    assert_eq!(
        copied_user_definition.status,
        SnippetApplicabilityStatus::Unrestricted
    );
    assert_eq!(copied_user_definition.reason, None);
}
