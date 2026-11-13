//! 验收子模式必须互斥，避免专项结果被误标为完整验收。
//!
//! @author kongweiguang

use super::{
    LOCAL_AGENT_CAPTURE_MODE, absolute_agent_continuity_output_base, parse_options,
    read_fixture_manifest_from, resolve_fixture_local_files_root, toml_basic_string,
    write_local_agent_profile,
};
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
        "--agent-continuity-only",
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
        assert_eq!(
            options.agent_continuity_only,
            mode == "--agent-continuity-only"
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
        "--agent-continuity-only",
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
        "--agent-continuity-only",
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

/// 仅 Agent 连续性入口必须使用全新本地配置，不能与 fixture 清单、配置目录或长主机名混用。
///
/// @author kongweiguang
#[test]
fn agent_continuity_mode_rejects_fixture_inputs_in_any_order() {
    let manifest = std::env::temp_dir().join("agent-continuity-manifest.json");
    for arguments in [
        vec![
            OsString::from("--agent-continuity-only"),
            OsString::from("--fixture-config-dir"),
            OsString::from("fixture-config"),
        ],
        vec![
            OsString::from("--fixture-config-dir"),
            OsString::from("fixture-config"),
            OsString::from("--agent-continuity-only"),
        ],
        vec![
            OsString::from("--fixture-manifest"),
            manifest.into_os_string(),
            OsString::from("--agent-continuity-only"),
        ],
    ] {
        assert!(parse_options(arguments).is_err());
    }

    let options = parse_options(["--agent-continuity-only"].map(OsString::from))
        .expect("standalone local agent mode");
    assert!(options.agent_continuity_only);
    assert!(options.fixture_config_directory.is_none());
    assert!(options.fixture_manifest.is_none());
    assert_eq!(
        LOCAL_AGENT_CAPTURE_MODE,
        "native_local_agent_continuity_same_tab_split_without_fixture"
    );
}

/// 本地 Agent 验收配置必须默认选择 pwsh 7、显式不加载用户 profile，并把 PTY 起点限制在
/// 本轮隔离输出目录；KnownFolder 不服从环境重定向，profile cwd 是能收紧的启动边界。
///
/// @author kongweiguang
#[test]
fn local_agent_profile_selects_pwsh7_without_profile_and_isolated_cwd() {
    let directory = tempfile::tempdir().expect("temporary profile directory");
    let path = directory.path().join("agent-continuity-local.toml");
    let cwd = directory.path().join("isolated cwd");
    fs::create_dir(&cwd).expect("temporary working directory");
    write_local_agent_profile(&path, &cwd).expect("write local agent profile");
    let document = fs::read_to_string(path).expect("read local agent profile");
    assert!(document.contains("shell = \"pwsh.exe\""));
    assert!(document.contains("\"-NoLogo\", \"-NoProfile\""));
    assert!(document.contains("is_default = true"));
    assert!(!document.contains("powershell.exe"));
    assert!(
        document.contains(&format!(
            "cwd = {}",
            toml_basic_string(cwd.to_str().expect("temporary cwd is valid UTF-8"))
        )),
        "profile cwd must escape the isolated working directory as a TOML basic string"
    );
}

/// Agent-only 默认与显式相对输出都必须在创建前锚定为同一绝对身份；否则子进程会按自己的
/// cwd 解析 config/stage，导致触发文件写入另一位置。测试不依赖 Windows 盘符字面量。
///
/// @author kongweiguang
#[test]
fn agent_continuity_output_base_is_absolute_for_default_and_relative_request() {
    let current = std::env::current_dir().expect("current directory");
    let default = absolute_agent_continuity_output_base(None).expect("default output base");
    assert!(default.is_absolute());
    assert_eq!(
        default,
        current
            .join("target")
            .join("acceptance")
            .join("right-rail-agent-continuity")
    );

    let relative = Path::new("target")
        .join("acceptance")
        .join("relative agent run");
    let resolved =
        absolute_agent_continuity_output_base(Some(&relative)).expect("relative output base");
    assert!(resolved.is_absolute());
    assert_eq!(resolved, current.join(relative));
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
