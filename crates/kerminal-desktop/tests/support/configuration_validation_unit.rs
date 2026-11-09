//! 离线配置校验 CLI 的参数边界测试，不触碰真实用户配置根。
//!
//! @author kongweiguang

use std::{ffi::OsString, path::PathBuf};

use super::{parse_root_argument, run_from_args, run_validation};

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

/// 样例先满足密钥隔离规则，再验证同一插件文件随目录使用 codec，避免其它诊断掩盖提供方分支。
///
/// @author kongweiguang
#[test]
fn validator_uses_the_supplied_host_catalog() {
    use kerminal_application::connections::host_types::HostConfigurationType;
    use kerminal_infrastructure::configuration::host_configuration_catalog;

    let root = tempfile::tempdir().expect("隔离配置根");
    std::fs::write(root.path().join(".gitignore"), "secrets/vault-key.toml\n")
        .expect("验收配置保留密钥忽略规则");
    let hosts = root.path().join("hosts");
    std::fs::create_dir(&hosts).expect("创建主机目录");
    let path = hosts.join("validation-host.toml");
    let raw = "schema_version = 3\nid = \"validation-host\"\nprotocol = \"example-host\"\nprovider_id = \"example.plugin\"\nname = \"Validation\"\nsort_order = 0\nconfiguration_version = 1\ncreated_at = \"2026-01-01T00:00:00Z\"\nupdated_at = \"2026-01-01T00:00:00Z\"\n\n[configuration]\naddress = \"\"\n";
    std::fs::write(&path, raw).expect("写入插件主机");
    let unknown = host_configuration_catalog(Vec::new()).expect("内置目录");
    let codec = unknown
        .get("kerminal", "ssh")
        .expect("内置 codec")
        .codec
        .clone();
    let extension =
        HostConfigurationType::new("example.plugin", "example-host", codec).expect("扩展类型");
    let known = host_configuration_catalog(vec![extension]).expect("扩展目录");

    assert_eq!(run_validation(root.path(), &known), 1);
    assert_eq!(run_validation(root.path(), &unknown), 0);
    assert_eq!(std::fs::read_to_string(path).expect("原文未改写"), raw);
}
