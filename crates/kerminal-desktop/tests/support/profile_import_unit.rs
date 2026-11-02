//! 离线 Profile 导入 CLI 的参数和数据边界。
//!
//! @author kongweiguang

use std::{ffi::OsString, fs};

use super::*;

/// 只接管首个显式命令，并拒绝缺目标、重复模式、空路径及额外参数。
#[test]
fn parser_requires_explicit_roots_and_exactly_one_mode() {
    let args = |items: &[&str]| items.iter().map(OsString::from).collect::<Vec<_>>();
    assert!(parse_arguments(args(&["--validate-config", "ignored"])).is_none());
    for items in [
        vec![COMMAND],
        vec![COMMAND, "source", "--dry-run"],
        vec![COMMAND, "source", "--target", "target"],
        vec![
            COMMAND,
            "source",
            "--target",
            "target",
            "--dry-run",
            "--apply",
        ],
        vec![
            COMMAND,
            "source",
            "--target",
            "target",
            "--dry-run",
            "extra",
        ],
        vec![COMMAND, "--target", "target", "--dry-run"],
    ] {
        assert!(matches!(parse_arguments(args(&items)), Some(Err(()))));
    }
    assert!(matches!(
        parse_arguments(args(&[
            COMMAND,
            "源 目录",
            "--target",
            "目标 目录",
            "--dry-run"
        ])),
        Some(Ok(ImportArguments {
            mode: ImportMode::DryRun,
            ..
        }))
    ));
}

/// 预演不创建锁、密钥或主机；实际导入成功后重复执行不得复制新主机。
#[test]
fn dry_run_is_read_only_and_apply_is_idempotent() {
    let parent = tempfile::tempdir().expect("隔离父目录");
    let source = parent.path().join("旧来源");
    let target = parent.path().join("新目标");
    fs::create_dir_all(source.join("profiles")).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(
        target.join(".kerminal-format.toml"),
        b"data_format = 4\nmigration_contract = 3\n",
    )
    .unwrap();
    fs::write(
        source.join("profiles/work.toml"),
        concat!(
            "schema_version = 1\nid = \"work\"\nname = \"Work\"\nshell = \"cmd.exe\"\nargs = []\n",
            "env = { PASSWORD = \"private-test-value\" }\nis_default = true\nsort_order = 0\n",
            "created_at = \"1\"\nupdated_at = \"1\"\n",
        ),
    )
    .unwrap();
    let (exit, preview) = execute(ImportArguments {
        source: source.clone(),
        target: target.clone(),
        mode: ImportMode::DryRun,
    });
    assert_eq!(exit, 0, "{preview}");
    assert_eq!(preview["preview"]["sourceProfiles"], 1);
    assert_eq!(preview["preview"]["newHosts"], 1);
    assert!(!preview.to_string().contains("private-test-value"));
    assert!(!source.join(".storage.lock").exists());
    assert!(!target.join("hosts").exists());
    assert!(!target.join("secrets").exists());
    let (exit, report) = execute(ImportArguments {
        source: source.clone(),
        target: target.clone(),
        mode: ImportMode::Apply,
    });
    assert_eq!(exit, 0, "{report}");
    assert!(!report.to_string().contains("private-test-value"));
    let (exit, repeated) = execute(ImportArguments {
        source,
        target: target.clone(),
        mode: ImportMode::Apply,
    });
    assert_eq!(exit, 0, "{repeated}");
    assert_eq!(repeated["report"]["outcome"], "alreadyCurrent");
    assert_eq!(fs::read_dir(target.join("hosts")).unwrap().count(), 1);
}
