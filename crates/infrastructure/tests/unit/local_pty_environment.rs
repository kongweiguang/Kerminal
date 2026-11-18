//! 本地 PTY 默认终端类型与显式环境优先级测试。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use kerminal_domain::{connections::RemoteShellDialect, sessions::LocalSessionTarget};

use super::*;

/// 创建带 `usr/bin` 与 AppRun 标记的临时 AppDir，复现 raw FUSE 挂载和直接执行解包
/// AppRun 两种启动方式共有的目录契约，供识别与环境恢复测试使用。
///
/// @author kongweiguang
fn fake_appdir() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("临时 AppDir");
    std::fs::create_dir_all(directory.path().join("usr/bin")).expect("创建 usr/bin");
    std::fs::write(directory.path().join("AppRun"), b"stub").expect("写入 AppRun 标记");
    directory
}

/// raw FUSE 挂载路径和直接执行解包后的 AppRun 都由同一 `<AppDir>/usr/bin` + AppRun
/// 契约识别；缺少 AppRun 的 DEB 式布局和异常层级必须保持不处理。
///
/// @author kongweiguang
#[test]
fn appimage_appdir_detection_requires_apprun_marker() {
    use super::appimage_env::appdir_from_executable;

    let directory = tempfile::tempdir().expect("临时目录");
    let binary = directory.path().join("usr/bin/kerminal");
    std::fs::create_dir_all(binary.parent().expect("bin 目录")).expect("创建 bin 目录");
    assert_eq!(appdir_from_executable(&binary), None);

    std::fs::write(directory.path().join("AppRun"), b"stub").expect("写入 AppRun");
    assert_eq!(
        appdir_from_executable(&binary),
        Some(directory.path().to_path_buf())
    );

    // DEB 的 /usr/bin/kerminal 没有 /AppRun；开发构建的 target/debug 也不满足 usr/bin 层级。
    assert_eq!(appdir_from_executable(Path::new("/usr/bin/kerminal")), None);
    assert_eq!(
        appdir_from_executable(&directory.path().join("usr/lib/kerminal")),
        None
    );
}

/// 按 AppRun 模板补上目录尾部平台分隔符，确保回归覆盖真实注入值而不是规范路径。
///
/// @author kongweiguang
fn apprun_path(path: PathBuf) -> PathBuf {
    let mut value = path.into_os_string();
    value.push(std::path::MAIN_SEPARATOR.to_string());
    PathBuf::from(value)
}

/// AppRun 追加在宿主原值前的 AppDir 组件必须被剔除，宿主搜索顺序保持不变；
/// profile 随后写入的显式值仍要覆盖恢复结果。
///
/// @author kongweiguang
#[test]
fn appimage_restore_recovers_host_paths_before_profile_environment() {
    let directory = fake_appdir();
    let appdir = directory.path();
    let host_path = std::env::join_paths([Path::new("/opt/host/bin"), Path::new("/usr/bin")])
        .expect("宿主 PATH");
    let injected_path = std::env::join_paths([
        apprun_path(appdir.join("usr/bin")),
        apprun_path(appdir.join("usr/sbin")),
        PathBuf::from("/opt/host/bin"),
        PathBuf::from("/usr/bin"),
    ])
    .expect("注入 PATH");
    let host_ld_library_path =
        std::env::join_paths([Path::new("/opt/host/lib")]).expect("宿主 LD_LIBRARY_PATH");
    let injected_ld_library_path = std::env::join_paths([
        apprun_path(appdir.join("usr/lib")),
        PathBuf::from("/opt/host/lib"),
    ])
    .expect("注入 LD_LIBRARY_PATH");
    let injected_python_path = std::env::join_paths([
        apprun_path(appdir.join("usr/share/pyshared")),
        PathBuf::from(""),
    ])
    .expect("注入 PYTHONPATH");
    let host_perl_lib = std::env::join_paths([Path::new("/opt/host/perl")]).expect("宿主 PERLLIB");
    let injected_perl_lib = std::env::join_paths([
        apprun_path(appdir.join("usr/share/perl5")),
        PathBuf::from("/opt/host/perl"),
    ])
    .expect("注入 PERLLIB");

    let mut command = portable_pty::CommandBuilder::new("selected-shell");
    command.env("PATH", &injected_path);
    command.env("LD_LIBRARY_PATH", &injected_ld_library_path);
    command.env("PYTHONPATH", &injected_python_path);
    command.env("PYTHONHOME", apprun_path(appdir.join("usr")));
    command.env("PERLLIB", &injected_perl_lib);

    let mut profile_env = BTreeMap::new();
    profile_env.insert(
        OsString::from("PYTHONHOME"),
        OsString::from("/profile/python"),
    );
    let session_target = target(profile_env);
    super::apply_environment(
        &mut command,
        &session_target,
        &BTreeMap::new(),
        Some(appdir),
    );

    assert_eq!(command.get_env("PATH"), Some(host_path.as_os_str()));
    assert_eq!(
        command.get_env("LD_LIBRARY_PATH"),
        Some(host_ld_library_path.as_os_str())
    );
    assert_eq!(command.get_env("PERLLIB"), Some(host_perl_lib.as_os_str()));
    assert_eq!(command.get_env("PYTHONPATH"), None);
    // AppRun 无条件覆盖 PYTHONHOME；恢复阶段移除伪造根，随后 profile 的显式值生效。
    assert_eq!(
        command.get_env("PYTHONHOME"),
        Some(OsStr::new("/profile/python"))
    );
}

/// 只过滤 AppDir 内组件：同名前缀目录、非 AppDir 的显式 PYTHONHOME 以及未检测到
/// AppImage 的普通 DEB/开发环境都必须保持原值。
///
/// @author kongweiguang
#[test]
fn appimage_restore_leaves_unrelated_and_non_appimage_environment_untouched() {
    let directory = fake_appdir();
    let appdir = directory.path();
    let sibling = appdir.with_file_name(format!(
        "{}-sibling",
        appdir.file_name().expect("AppDir 名称").to_string_lossy()
    ));
    let sibling_path =
        std::env::join_paths([sibling.clone(), PathBuf::from("/usr/bin")]).expect("同前缀 PATH");

    let mut command = portable_pty::CommandBuilder::new("selected-shell");
    command.env("PATH", &sibling_path);
    command.env("PYTHONHOME", "/opt/host/python");
    super::appimage_env::restore_host_environment(&mut command, appdir);
    assert_eq!(command.get_env("PATH"), Some(sibling_path.as_os_str()));
    assert_eq!(
        command.get_env("PYTHONHOME"),
        Some(OsStr::new("/opt/host/python"))
    );

    let mut untouched = portable_pty::CommandBuilder::new("selected-shell");
    untouched.env("PATH", appdir.join("usr/bin"));
    super::apply_environment(
        &mut untouched,
        &target(BTreeMap::new()),
        &BTreeMap::new(),
        None,
    );
    assert_eq!(
        untouched.get_env("PATH"),
        Some(appdir.join("usr/bin").as_os_str())
    );
}

/// 创建只供 CommandBuilder 环境投影测试使用的最小本地目标，不启动子进程。
///
/// @author kongweiguang
fn target(environment: BTreeMap<OsString, OsString>) -> LocalSessionTarget {
    LocalSessionTarget {
        program: OsString::from("selected-shell"),
        args: Vec::new(),
        working_directory: None,
        environment,
    }
}

/// 确认真实 PTY 在 profile 未指定时覆盖继承的 dumb 或缺失 TERM。
///
/// @author kongweiguang
#[test]
fn pty_defaults_term_to_supported_xterm_profile() {
    let builder = command_builder(
        &target(BTreeMap::new()),
        &[],
        &BTreeMap::new(),
        RemoteShellDialect::Unknown,
    );

    assert_eq!(builder.get_env("TERM"), Some(OsStr::new("xterm-256color")));
}

/// 确认 profile 与 shell startup 环境的 TERM 都属于显式用户值并保留既有合并顺序。
///
/// @author kongweiguang
#[test]
fn explicit_term_environment_overrides_default_and_startup_wins() {
    let mut profile_env = BTreeMap::new();
    profile_env.insert(OsString::from("TERM"), OsString::from("profile-term"));
    let mut startup_env = BTreeMap::new();
    startup_env.insert(OsString::from("TERM"), OsString::from("startup-term"));

    let profile_only = command_builder(
        &target(profile_env.clone()),
        &[],
        &BTreeMap::new(),
        RemoteShellDialect::Unknown,
    );
    let merged = command_builder(
        &target(profile_env),
        &[],
        &startup_env,
        RemoteShellDialect::Unknown,
    );

    assert_eq!(
        profile_only.get_env("TERM"),
        Some(OsStr::new("profile-term"))
    );
    assert_eq!(merged.get_env("TERM"), Some(OsStr::new("startup-term")));
}

/// 确认识别出的 Unix shell 使用实际启动程序作为 SHELL，而显式 profile 值优先。
///
/// @author kongweiguang
#[cfg(unix)]
#[test]
fn unix_shell_environment_matches_program_unless_profile_overrides_it() {
    let mut shell_target = target(BTreeMap::new());
    shell_target.program = OsString::from("/opt/kerminal-shell");
    let inferred = command_builder(
        &shell_target,
        &[],
        &BTreeMap::new(),
        RemoteShellDialect::Posix,
    );

    let mut explicit_env = BTreeMap::new();
    explicit_env.insert(
        OsString::from("SHELL"),
        OsString::from("/custom/user-shell"),
    );
    let explicit = command_builder(
        &target(explicit_env),
        &[],
        &BTreeMap::new(),
        RemoteShellDialect::Posix,
    );

    assert_eq!(
        inferred.get_env("SHELL"),
        Some(shell_target.program.as_os_str())
    );
    assert_eq!(
        explicit.get_env("SHELL"),
        Some(OsStr::new("/custom/user-shell"))
    );
}

/// 确认环境键在 Unix 保持大小写敏感，在 Windows 按原生规则忽略大小写。
///
/// @author kongweiguang
#[test]
fn environment_key_matching_follows_platform_rules() {
    #[cfg(unix)]
    assert!(!environment_key_matches(OsStr::new("term"), "TERM"));
    #[cfg(windows)]
    assert!(environment_key_matches(OsStr::new("term"), "TERM"));
}

/// 确认 Windows 可执行发现使用进程环境，且后续目标环境仍可显式覆盖搜索顺序。
///
/// @author kongweiguang
#[cfg(windows)]
#[test]
fn windows_launcher_search_environment_is_explicit_and_overridable() {
    let mut command = portable_pty::CommandBuilder::new("selected-shell");
    super::windows_launcher::set_command_search_environment(
        &mut command,
        Some(OsString::from(r"C:\selected\first;C:\selected\second")),
        Some(OsString::from(".CMD;.EXE")),
    );

    assert_eq!(
        command.get_env("PATH"),
        Some(OsStr::new(r"C:\selected\first;C:\selected\second"))
    );
    assert_eq!(command.get_env("PATHEXT"), Some(OsStr::new(".CMD;.EXE")));

    command.env("PATH", r"C:\target\override");
    command.env("PATHEXT", ".EXE;.CMD");
    assert_eq!(
        command.get_env("PATH"),
        Some(OsStr::new(r"C:\target\override"))
    );
    assert_eq!(command.get_env("PATHEXT"), Some(OsStr::new(".EXE;.CMD")));

    let mut absent = portable_pty::CommandBuilder::new("selected-shell");
    super::windows_launcher::set_command_search_environment(&mut absent, None, None);
    assert_eq!(absent.get_env("PATH"), None);
    assert_eq!(absent.get_env("PATHEXT"), None);
}

/// 测试专用的 source 桥接错误：外层 io 的 source() 返回本错误，本错误的 source() 再返回
/// 内层 os error；用于验证标准 Error::source 链确实包含内层 io。不是 std::io::Error 嵌套
/// 容器本体的覆盖——后者 source() 会跳过内层本体。
///
/// @author kongweiguang
#[derive(Debug)]
struct OuterIo(std::io::Error);

impl std::fmt::Display for OuterIo {
    /// 固定安全占位文本，避免测试错误信息携带路径或命令；只用于构造 source 链。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("outer io")
    }
}

impl std::error::Error for OuterIo {
    /// 把内层 os error 作为标准 source 暴露，使 anyhow 链可继续 downcast 到内层 io。
    ///
    /// @author kongweiguang
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// 探针在 anyhow 链中逐层 downcast io::Error 并取 raw_os_error；外层 raw=None 时不能
/// 提前停止，必须继续找到内层 OS 错误。输出行只含固定枚举和数字。
///
/// @author kongweiguang
#[test]
fn spawn_probe_extracts_nested_io_and_classifies_fixed_stage() {
    let outer = std::io::Error::other(OuterIo(std::io::Error::from_raw_os_error(5)));
    let error = anyhow::Error::new(outer).context("CreateProcessW failed");
    assert!(
        error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.raw_os_error() == Some(5))
        }),
        "standard Error::source chain must include the inner io"
    );
    let (stage, raw_os_error) = classify_spawn_error(&error);
    assert_eq!(stage, SpawnProbeStage::CreateProcess);
    assert_eq!(raw_os_error, Some(5));
    let line = format_spawn_probe_line(stage, raw_os_error);
    assert_eq!(
        line,
        "KERMINAL_ACCEPTANCE_PTY_PROBE stage=create_process raw_os_error=5"
    );
}

/// 真实 vendor 固定 context 必须精确分类；同前缀后的动态 payload 与敏感文本不进入输出。
///
/// @author kongweiguang
#[test]
fn spawn_probe_matches_real_vendor_contexts_and_never_leaks_text() {
    for (text, expected) in [
        (
            r"UpdateProcThreadAttribute for Job List failed: Access is denied. (os error 5) C:\secret\cmd.exe --token abc",
            SpawnProbeStage::Attribute,
        ),
        ("AssignProcessToJobObject failed", SpawnProbeStage::Job),
        ("CreateProcessW failed", SpawnProbeStage::CreateProcess),
        (
            r"unclassified vendor failure C:\secret\cmd.exe --token abc",
            SpawnProbeStage::Unknown,
        ),
    ] {
        let error = anyhow::anyhow!(text);
        let (stage, raw_os_error) = classify_spawn_error(&error);
        assert_eq!(stage, expected);
        assert_eq!(raw_os_error, None);
        let line = format_spawn_probe_line(stage, raw_os_error);
        assert!(line.contains("raw_os_error=unknown"));
        assert!(
            !line.contains("secret") && !line.contains("token") && !line.contains("cmd.exe"),
            "probe line must not leak source text: {line}"
        );
    }
}
