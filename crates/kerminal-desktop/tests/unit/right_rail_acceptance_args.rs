//! 验收子模式必须互斥，避免专项结果被误标为完整验收。
//!
//! @author kongweiguang

use super::{parse_options, read_fixture_manifest_from, resolve_fixture_local_files_root};
use std::ffi::OsString;
use std::{fs, path::Path};

/// 每个单独子模式仍可组合窗口参数，防止新增专项入口影响已有命令行。
///
/// @author kongweiguang
#[test]
fn each_submode_accepts_window_options() {
    for mode in [
        "--capture-only",
        "--functional-only",
        "--sftp-functional-only",
    ] {
        let options =
            parse_options([mode, "--width", "1020", "--height", "720"].map(OsString::from))
                .expect("a single submode is valid");
        assert_eq!(options.window_width, Some(1020));
        assert_eq!(options.window_height, Some(720));
        assert_eq!(options.capture_only, mode == "--capture-only");
        assert_eq!(options.functional_only, mode == "--functional-only");
        assert_eq!(
            options.sftp_functional_only,
            mode == "--sftp-functional-only"
        );
    }
}

/// 任意顺序的重复或混用均拒绝，防止专项结果被误标为完整验收。
///
/// @author kongweiguang
#[test]
fn submodes_reject_duplicates_and_mixed_pairs() {
    let modes = [
        "--capture-only",
        "--functional-only",
        "--sftp-functional-only",
    ];
    for first in modes {
        for second in modes {
            assert!(parse_options([first, second].map(OsString::from)).is_err());
        }
    }
}

/// 冷重启的两个阶段各占一个模式，不能与任何既有专项或彼此重复组合。
///
/// @author kongweiguang
#[test]
fn restart_submodes_are_exclusive() {
    let prepare =
        parse_options(["--transfer-restart-prepare"].map(OsString::from)).expect("prepare mode");
    assert!(prepare.transfer_restart_prepare);
    let previous = std::env::temp_dir().join("restart-example");
    let verify = parse_options([
        OsString::from("--transfer-restart-verify"),
        previous.clone().into_os_string(),
    ])
    .expect("verify mode");
    assert_eq!(verify.transfer_restart_verify, Some(previous.clone()));
    assert!(parse_options(["--transfer-restart-verify"].map(OsString::from)).is_err());
    for other in [
        "--capture-only",
        "--functional-only",
        "--sftp-functional-only",
        "--transfer-restart-prepare",
    ] {
        assert!(parse_options(["--transfer-restart-prepare", other].map(OsString::from)).is_err());
        assert!(parse_options([other, "--transfer-restart-prepare"].map(OsString::from)).is_err());
        assert!(
            parse_options([
                OsString::from(other),
                OsString::from("--transfer-restart-verify"),
                previous.clone().into_os_string(),
            ])
            .is_err()
        );
        assert!(
            parse_options([
                OsString::from("--transfer-restart-verify"),
                previous.clone().into_os_string(),
                OsString::from(other),
            ])
            .is_err()
        );
    }
    assert!(
        parse_options([
            OsString::from("--transfer-restart-verify"),
            previous.clone().into_os_string(),
            OsString::from("--transfer-restart-verify"),
            previous.into_os_string(),
        ])
        .is_err()
    );
}

/// 长名称只允许作用于隔离 SFTP 专项，避免改变其他验收输入或用户配置。
///
/// @author kongweiguang
#[test]
fn long_fixture_names_require_sftp_submode() {
    let options =
        parse_options(["--long-fixture-host-names", "--sftp-functional-only"].map(OsString::from))
            .expect("long fixture names with SFTP mode");
    assert!(options.long_fixture_host_names);
    assert!(parse_options(["--long-fixture-host-names"].map(OsString::from)).is_err());
    assert!(
        parse_options(
            [
                "--sftp-functional-only",
                "--long-fixture-host-names",
                "--long-fixture-host-names"
            ]
            .map(OsString::from)
        )
        .is_err()
    );
}

/// 清单参数不能重复或退回相对路径，避免验收意外读取另一个工作目录中的夹具。
///
/// @author kongweiguang
#[test]
fn fixture_manifest_requires_explicit_absolute_identity() {
    let absolute = std::env::temp_dir().join("acceptance-manifest.json");
    let options = parse_options([
        OsString::from("--fixture-manifest"),
        absolute.clone().into_os_string(),
    ])
    .expect("an explicit manifest is accepted");
    assert_eq!(options.fixture_manifest.as_ref(), Some(&absolute));
    assert!(
        parse_options([
            OsString::from("--fixture-manifest"),
            absolute.clone().into_os_string(),
            OsString::from("--fixture-manifest"),
            absolute.into_os_string(),
        ])
        .is_err()
    );
    assert!(read_fixture_manifest_from(Some(Path::new("relative-manifest.json"))).is_err());
}

/// 本地文件根必须属于清单所在夹具，而不是另一个验收运行或用户目录。
///
/// @author kongweiguang
#[test]
fn fixture_local_files_belong_to_supplied_manifest_directory() {
    let fixture = tempfile::tempdir().expect("temporary fixture directory");
    let local = fixture.path().join("local-files");
    let unrelated = fixture.path().join("unrelated");
    fs::create_dir(&local).expect("fixture files directory");
    fs::create_dir(&unrelated).expect("unrelated directory");
    let manifest = fixture.path().join("manifest.json");
    assert_eq!(
        resolve_fixture_local_files_root(&manifest, &local).expect("fixture files root"),
        fs::canonicalize(&local).expect("canonical fixture files root")
    );
    assert!(resolve_fixture_local_files_root(&manifest, &unrelated).is_err());
}
