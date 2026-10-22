//! 本机受控 CLI spawn 错误的安全分类回归。
//!
//! @author kongweiguang

use std::{error::Error, io};

use kerminal_application::{AppError, PublicErrorCode};

use super::local_spawn_failure;

/// 分类提示变化时仍保留 Infrastructure code 和底层 spawn source。
///
/// @author kongweiguang
fn assert_infrastructure_source(
    error: AppError,
    expected_message: &str,
    expected_kind: io::ErrorKind,
) {
    assert_eq!(error.public_code(), PublicErrorCode::Infrastructure);
    assert_eq!(error.public_message(), expected_message);
    let source = Error::source(&error).expect("Infrastructure 应保留原始 spawn source");
    assert_eq!(
        source
            .downcast_ref::<io::Error>()
            .expect("source 类型应保持 io::Error")
            .kind(),
        expected_kind
    );
}

/// 仅固定本机 Docker/Podman 命令缺失时给出可恢复的 CLI 安装提示。
///
/// @author kongweiguang
#[test]
fn missing_known_container_cli_uses_safe_install_guidance() {
    assert_infrastructure_source(
        local_spawn_failure("docker", false, io::Error::from(io::ErrorKind::NotFound)),
        "无法找到或启动本机 Docker CLI（docker），请确认已安装且命令位于 PATH 中",
        io::ErrorKind::NotFound,
    );
    assert_infrastructure_source(
        local_spawn_failure("podman", false, io::Error::from(io::ErrorKind::NotFound)),
        "无法找到或启动本机 Podman CLI（podman），请确认已安装且命令位于 PATH 中",
        io::ErrorKind::NotFound,
    );
}

/// 只有固定本机 CLI 的权限拒绝才显示权限/策略提示，不泄露本机路径。
///
/// @author kongweiguang
#[test]
fn known_container_cli_permission_denied_is_actionable_without_path_details() {
    assert_infrastructure_source(
        local_spawn_failure(
            "docker",
            false,
            io::Error::from(io::ErrorKind::PermissionDenied),
        ),
        "本机 Docker CLI（docker）启动被拒绝，请检查可执行权限或系统安全策略",
        io::ErrorKind::PermissionDenied,
    );
    assert_infrastructure_source(
        local_spawn_failure(
            "podman",
            false,
            io::Error::from(io::ErrorKind::PermissionDenied),
        ),
        "本机 Podman CLI（podman）启动被拒绝，请检查可执行权限或系统安全策略",
        io::ErrorKind::PermissionDenied,
    );
}

/// 自定义 cwd、其它程序和其它 OS 错误沿用通用提示，避免把歧义误报为依赖缺失。
///
/// @author kongweiguang
#[test]
fn ambiguous_spawn_errors_keep_generic_message() {
    let generic = "本机运维命令启动失败";
    for kind in [io::ErrorKind::NotFound, io::ErrorKind::PermissionDenied] {
        assert_infrastructure_source(
            local_spawn_failure("docker", true, io::Error::from(kind)),
            generic,
            kind,
        );
        assert_infrastructure_source(
            local_spawn_failure("podman", true, io::Error::from(kind)),
            generic,
            kind,
        );
    }
    assert_infrastructure_source(
        local_spawn_failure(
            "C:\\private\\docker.exe",
            false,
            io::Error::from(io::ErrorKind::NotFound),
        ),
        generic,
        io::ErrorKind::NotFound,
    );
    assert_infrastructure_source(
        local_spawn_failure(
            "other-tool",
            false,
            io::Error::from(io::ErrorKind::NotFound),
        ),
        generic,
        io::ErrorKind::NotFound,
    );
    assert_infrastructure_source(
        local_spawn_failure("docker", false, io::Error::from(io::ErrorKind::Other)),
        generic,
        io::ErrorKind::Other,
    );
}
