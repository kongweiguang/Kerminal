//! 离线配置校验 CLI 的参数边界测试，不触碰真实用户配置根。
//!
//! @author kongweiguang

use std::{ffi::OsString, path::PathBuf};

use super::{parse_root_argument, run_from_args};

/// Windows 路径以单个原生参数保留空格，不由 helper 再拼接或拆分。
///
/// @author kongweiguang
#[test]
fn validator_command_preserves_a_root_path_with_spaces() {
    let root = PathBuf::from(r"C:\Users\Name With Spaces\.kerminal-gpui");
    let parsed = parse_root_argument([
        OsString::from("--validate-config"),
        root.clone().into_os_string(),
    ]);

    assert_eq!(parsed, Some(Ok(root)));
}

/// 非 validator 启动参数必须交还给主程序的其它早期命令处理。
///
/// @author kongweiguang
#[test]
fn validator_command_ignores_other_first_arguments() {
    let parsed = parse_root_argument([OsString::from("--right-rail-acceptance")]);

    assert_eq!(parsed, None);
}

/// 缺少根、空根或多余参数都拒绝执行，避免错误验证当前工作目录。
///
/// @author kongweiguang
#[test]
fn validator_command_rejects_missing_empty_and_extra_arguments() {
    for arguments in [
        vec![OsString::from("--validate-config")],
        vec![OsString::from("--validate-config"), OsString::new()],
        vec![
            OsString::from("--validate-config"),
            OsString::from("config root"),
            OsString::from("unexpected"),
        ],
    ] {
        assert_eq!(parse_root_argument(arguments), Some(Err(())));
    }
}

/// CLI 返回码区分参数错误，不能把缺少根路径当作正常启动或当前目录校验。
///
/// @author kongweiguang
#[test]
fn validator_command_returns_usage_error_for_missing_root() {
    let exit_code = run_from_args([OsString::from("--validate-config")]);

    assert_eq!(exit_code, Some(2));
}

/// 只读 CLI 对缺省根输出有效报告并返回成功，同时不创建该根目录。
///
/// @author kongweiguang
#[test]
fn validator_command_returns_success_without_creating_a_missing_root() {
    let parent = tempfile::tempdir().expect("创建隔离父目录");
    let root = parent.path().join("root with spaces");
    let arguments = [
        OsString::from("--validate-config"),
        root.clone().into_os_string(),
    ];

    let exit_code = run_from_args(arguments);

    assert_eq!(exit_code, Some(0));
    assert!(!root.exists());
}

/// 配置诊断作为有效 JSON 输出并返回非零验证失败码，且不触碰原始文档。
///
/// @author kongweiguang
#[test]
fn validator_command_returns_invalid_status_without_rewriting_the_document() {
    let parent = tempfile::tempdir().expect("创建隔离父目录");
    let root = parent.path().join("invalid config");
    std::fs::create_dir(&root).expect("创建隔离配置根");
    let settings = root.join("settings.toml");
    let source = b"schema_version = 99\n";
    std::fs::write(&settings, source).expect("写入未来 schema 设置");
    let arguments = [OsString::from("--validate-config"), root.into_os_string()];

    let exit_code = run_from_args(arguments);

    assert_eq!(exit_code, Some(1));
    assert_eq!(std::fs::read(settings).unwrap(), source);
}
