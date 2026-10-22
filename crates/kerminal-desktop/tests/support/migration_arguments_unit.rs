//! 维护入口的 argv 契约，不访问环境变量或用户目录。
//!
//! @author kongweiguang

use std::{ffi::OsString, path::PathBuf};

use super::{MigrationAction, MigrationArguments, parse_migration_arguments};

/// 路径作为一个原始 OS 参数传入，中文和空格不会被再次拆词或转码。
///
/// @author kongweiguang
#[test]
fn explicit_root_preserves_unicode_and_spaces() {
    let root = OsString::from("隔离 数据/中文配置");
    let parsed = parse_migration_arguments([
        OsString::from("--root"),
        root.clone(),
        OsString::from("--dry-run"),
    ])
    .expect("valid dry run arguments");
    assert_eq!(
        parsed,
        MigrationArguments {
            root: Some(PathBuf::from(root)),
            action: MigrationAction::DryRun,
        }
    );
}

/// 两种模式均可在根参数前出现，未指定根时只保留默认根策略而不提前解析或创建目录。
///
/// @author kongweiguang
#[test]
fn mode_order_does_not_change_the_root_contract() {
    let parsed =
        parse_migration_arguments(["--apply", "--root", "isolated root"].map(OsString::from))
            .expect("apply before root");
    assert_eq!(parsed.action, MigrationAction::Apply);
    assert_eq!(parsed.root, Some(PathBuf::from("isolated root")));
    let default = parse_migration_arguments([OsString::from("--dry-run")]).expect("default root");
    assert!(default.root.is_none());
    assert_eq!(default.action, MigrationAction::DryRun);
}

/// 缺失、冲突或未知参数必须在服务构造前失败，不能猜测维护模式或吞掉参数。
///
/// @author kongweiguang
#[test]
fn ambiguous_or_incomplete_arguments_are_rejected() {
    for arguments in [
        vec![],
        vec!["--root", "isolated"],
        vec!["--dry-run", "--apply"],
        vec!["--apply", "--apply"],
        vec!["--dry-run", "--root"],
        vec!["--root", "--dry-run"],
        vec!["--root", "", "--apply"],
        vec!["--root", "a", "--root", "b", "--apply"],
        vec!["--apply", "--unknown"],
        vec!["--apply", "unexpected"],
    ] {
        let result = parse_migration_arguments(arguments.into_iter().map(OsString::from));
        assert!(
            result.is_err(),
            "invalid argv must not select a migration action"
        );
    }
}

/// Unix 允许非 UTF-8 路径，维护入口不能用有损字符串替代真实目录身份。
///
/// @author kongweiguang
#[cfg(unix)]
#[test]
fn explicit_root_preserves_non_utf8_os_bytes() {
    use std::os::unix::ffi::OsStringExt;
    let root = OsString::from_vec(b"isolated-\xff-root".to_vec());
    let parsed = parse_migration_arguments([
        OsString::from("--apply"),
        OsString::from("--root"),
        root.clone(),
    ])
    .expect("non UTF-8 root is valid OS input");
    assert_eq!(parsed.root, Some(PathBuf::from(root)));
}
