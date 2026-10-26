//! 用户数据目录选择规则的隔离测试，不读取宿主机用户配置。
//!
//! @author kongweiguang

use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};
#[cfg(windows)]
use std::{os::windows::process::CommandExt, process::Command};

use super::{ConfigRootError, points_to_formal_default_from_inputs, resolve_from_inputs};

/// 使用隔离临时路径覆盖显式根来源判断，不读取或修改真实用户配置。
///
/// @author kongweiguang
#[test]
fn formal_root_check_resolves_relative_dot_segments() {
    #[cfg(windows)]
    let (home, current, relative, isolated) = (
        r"C:\owned-test-home",
        r"C:\owned-test-home\work",
        r"..\.kerminal\.",
        r"C:\owned-test-home\target\isolated-879fe9f8-config",
    );
    #[cfg(not(windows))]
    let (home, current, relative, isolated) = (
        "/owned-test-home",
        "/owned-test-home/work",
        "../.kerminal/.",
        "/owned-test-home/target/isolated-879fe9f8-config",
    );
    assert!(points_to_formal_default_from_inputs(
        Path::new(relative),
        Some(PathBuf::from(home)),
        Some(PathBuf::from(current)),
    ));
    assert!(!points_to_formal_default_from_inputs(
        Path::new(isolated),
        Some(PathBuf::from(home)),
        Some(PathBuf::from(current)),
    ));
}

/// Windows 用目录 junction 覆盖普通用户无需 symlink 特权的路径别名场景；Unix 使用原生目录链接。
///
/// @author kongweiguang
fn create_directory_alias(link: &Path, target: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const SCRIPT: &str = "$ErrorActionPreference = 'Stop'; try { New-Item -ItemType Junction -Path $env:KERMINAL_TEST_ALIAS -Target $env:KERMINAL_TEST_SOURCE | Out-Null; exit 0 } catch { [Console]::Error.WriteLine($_.ToString()); exit 1 }";
        let run = |shell: &str| {
            let mut command = Command::new(shell);
            command
                .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
                .env("KERMINAL_TEST_ALIAS", link)
                .env("KERMINAL_TEST_SOURCE", target)
                .creation_flags(CREATE_NO_WINDOW);
            command.output()
        };
        let output = match run("pwsh") {
            Err(error) if error.kind() == io::ErrorKind::NotFound => run("powershell.exe")?,
            Err(error) => return Err(error),
            Ok(output) => output,
        };
        if output.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "PowerShell failed to create isolated directory junction: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
}

/// 已存在的数据根经父目录链接到达时，离线检查仍必须识别其正式目录身份。
///
/// @author kongweiguang
#[test]
fn formal_root_check_resolves_existing_directory_alias() {
    let fixture = tempfile::tempdir().expect("isolated root identity fixture");
    let home = fixture.path().join("user home");
    let formal_root = home.join(".kerminal");
    fs::create_dir_all(&formal_root).expect("isolated formal root");
    let alias = fixture.path().join("home alias");
    create_directory_alias(&alias, &home).expect("create isolated directory junction");

    assert!(points_to_formal_default_from_inputs(
        &alias.join(".kerminal"),
        Some(home),
        Some(fixture.path().to_path_buf()),
    ));
}

/// 符号链接后的 `..` 按链接目标解析；比较时必须保留文件系统实际处理路径的顺序。
///
/// @author kongweiguang
#[test]
fn formal_root_check_resolves_parent_segments_after_directory_alias() {
    let fixture = tempfile::tempdir().expect("isolated root identity fixture");
    let home = fixture.path().join("real home");
    let linked_child = home.join("child");
    let formal_root = home.join(".kerminal");
    fs::create_dir_all(&linked_child).expect("isolated linked child");
    fs::create_dir_all(&formal_root).expect("isolated formal root");
    let alias = fixture.path().join("home child alias");
    create_directory_alias(&alias, &linked_child).expect("create isolated directory junction");

    let aliased_root = alias.join("..").join(".kerminal");
    assert!(points_to_formal_default_from_inputs(
        &aliased_root,
        Some(home),
        Some(fixture.path().to_path_buf()),
    ));
}

/// 缺失组件被 `..` 抵消后重新解析后续链接，防止词法后缀掩盖正式目录别名。
///
/// @author kongweiguang
#[test]
fn formal_root_check_resolves_alias_after_cancelled_missing_component() {
    let fixture = tempfile::tempdir().expect("isolated root identity fixture");
    let home = fixture.path().join("real home");
    let formal_root = home.join(".kerminal");
    fs::create_dir_all(&formal_root).expect("isolated formal root");
    let alias = fixture.path().join("home child alias");
    create_directory_alias(&alias, &home).expect("create isolated directory junction");
    let root_after_cancelled_missing = fixture
        .path()
        .join("missing component")
        .join("..")
        .join("home child alias")
        .join(".kerminal");

    assert!(points_to_formal_default_from_inputs(
        &root_after_cancelled_missing,
        Some(home),
        Some(fixture.path().to_path_buf()),
    ));
}

/// 正式根尚未创建时也要沿已有父链接解析，且身份探测不能为了比较路径而建目录。
///
/// @author kongweiguang
#[test]
fn missing_formal_root_resolves_parent_alias_without_creating_root() {
    let fixture = tempfile::tempdir().expect("isolated root identity fixture");
    let home = fixture.path().join("user home");
    fs::create_dir_all(&home).expect("isolated home directory");
    let formal_root = home.join(".kerminal");
    let alias = fixture.path().join("home alias");
    create_directory_alias(&alias, &home).expect("create isolated directory junction");
    let aliased_root = alias.join(".kerminal");

    assert!(!formal_root.exists());
    assert!(points_to_formal_default_from_inputs(
        &aliased_root,
        Some(home),
        Some(fixture.path().to_path_buf()),
    ));
    assert!(!formal_root.exists());
}

/// 无法确定根身份时不能把离线检查关闭，避免缺失 home/cwd 被当成已证明隔离。
///
/// @author kongweiguang
#[test]
fn uncertain_root_origin_conservatively_requires_legacy_instance_check() {
    assert!(points_to_formal_default_from_inputs(
        Path::new("relative"),
        None,
        None
    ));
    assert!(points_to_formal_default_from_inputs(
        Path::new("relative"),
        Some(PathBuf::from("relative-home")),
        None,
    ));
    #[cfg(windows)]
    assert!(points_to_formal_default_from_inputs(
        Path::new("C:\\isolated\0root"),
        Some(PathBuf::from(r"C:\unavailable-user-home")),
        None,
    ));
    #[cfg(not(windows))]
    assert!(points_to_formal_default_from_inputs(
        Path::new("/isolated\0root"),
        Some(PathBuf::from("/unavailable-user-home")),
        None,
    ));
}

/// Windows 使用相同盘符与 UNC 词法身份，大小写/前缀别名不能使显式正式根绕过旧实例检查。
///
/// @author kongweiguang
#[cfg(windows)]
#[test]
fn windows_formal_root_check_accepts_ascii_case_and_extended_prefixes() {
    for root in [
        r"c:/owned-test-home/.Kerminal",
        r"\\?\C:\owned-test-home\.kerminal",
        r"C:\owned-test-home\nested\..\.kerminal",
    ] {
        assert!(points_to_formal_default_from_inputs(
            Path::new(root),
            Some(PathBuf::from(r"C:\owned-test-home")),
            None,
        ));
    }
    assert!(points_to_formal_default_from_inputs(
        Path::new(r"\\?\UNC\server\share\owned\.kerminal"),
        Some(PathBuf::from(r"\\server\share\owned")),
        None,
    ));
    assert!(
        points_to_formal_default_from_inputs(
            Path::new(r"C:\Usér\.kerminal"),
            Some(PathBuf::from(r"C:\USÉR")),
            None,
        ),
        "Unicode case ambiguity must enable, not bypass, the old-instance guard"
    );
    assert!(
        points_to_formal_default_from_inputs(
            Path::new(r"\\.\C:\owned-test-home\.kerminal"),
            Some(PathBuf::from(r"C:\owned-test-home")),
            None,
        ),
        "unrecognized device path identity must not bypass the old-instance guard"
    );
    assert!(
        !points_to_formal_default_from_inputs(
            Path::new(r"C:\含中文的home\isolated-879fe9f8-config"),
            Some(PathBuf::from(r"C:\含中文的home")),
            None,
        ),
        "known isolated UUID root must not probe the real legacy instance"
    );
}

/// Unix 的不同大小写始终是不同词法路径，不受 Windows 离线保护策略影响。
///
/// @author kongweiguang
#[cfg(not(windows))]
#[test]
fn unix_formal_root_check_preserves_case_sensitive_identity() {
    assert!(!points_to_formal_default_from_inputs(
        Path::new("/owned-home/.Kerminal"),
        Some(PathBuf::from("/owned-home")),
        None,
    ));
    assert!(points_to_formal_default_from_inputs(
        Path::new("/owned-home/.kerminal"),
        Some(PathBuf::from("/owned-home")),
        None,
    ));
}

/// Unix 将盘符与反斜杠视为普通文件名，不能套用 Windows 前缀规则误判配置根身份。
///
/// @author kongweiguang
#[cfg(unix)]
#[test]
fn unix_formal_root_check_preserves_windows_looking_directory_names() {
    let fixture = tempfile::tempdir().expect("isolated Unix root identity fixture");
    let home = fixture.path().join(r"C:\owned-home");
    let formal_root = home.join(".kerminal");
    fs::create_dir_all(&formal_root).expect("isolated Unix formal root");

    assert!(points_to_formal_default_from_inputs(
        &formal_root,
        Some(home),
        Some(fixture.path().to_path_buf()),
    ));
    assert!(!points_to_formal_default_from_inputs(
        &fixture.path().join("C:").join("owned-home/.kerminal"),
        Some(fixture.path().join(r"C:\owned-home")),
        Some(fixture.path().to_path_buf()),
    ));
}

/// 环境变量显式覆盖优先级最高，方便开发和安装测试隔离真实用户数据。
///
/// @author kongweiguang
#[test]
fn explicit_root_is_used_without_normalizing_it() {
    let result = resolve_from_inputs(
        Some(OsString::from("sandbox/config")),
        Some(PathBuf::from("home")),
    );

    assert_eq!(
        result.expect("explicit path is valid"),
        PathBuf::from("sandbox/config")
    );
}

/// 显式空路径很可能是部署配置错误，应阻止启动而不是静默回落到 home。
///
/// @author kongweiguang
#[test]
fn empty_override_is_rejected() {
    let result = resolve_from_inputs(Some(OsString::new()), Some(PathBuf::from("home")));

    assert!(matches!(result, Err(ConfigRootError::EmptyOverride)));
}

/// GPUI 两种构建模式均选择独立目录，且该目录不能误触发旧版正式根的迁移门禁。
///
/// @author kongweiguang
#[test]
fn default_uses_separate_gpui_home() {
    let result = resolve_from_inputs(None, Some(PathBuf::from("home")));

    assert_eq!(
        result.expect("default needs an available home directory"),
        PathBuf::from("home/.kerminal-gpui"),
    );
    let fixture = tempfile::tempdir().expect("隔离用户 home");
    let root = resolve_from_inputs(None, Some(fixture.path().to_path_buf())).unwrap();
    assert!(!points_to_formal_default_from_inputs(
        &root,
        Some(fixture.path().to_path_buf()),
        None,
    ));
}

/// home 不可用时拒绝默认启动，但显式隔离根仍可用，避免静默写入其它账户目录。
///
/// @author kongweiguang
#[test]
fn missing_home_requires_explicit_root() {
    let path = resolve_from_inputs(Some(OsString::from("sandbox/config")), None);
    let missing_home = resolve_from_inputs(None, None);

    assert_eq!(
        path.expect("explicit root does not need a home"),
        PathBuf::from("sandbox/config")
    );
    assert!(matches!(
        missing_home,
        Err(ConfigRootError::HomeDirectoryUnavailable)
    ));
}
