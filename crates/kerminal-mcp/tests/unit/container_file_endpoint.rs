//! 容器 MCP endpoint 的路径归一化契约测试。
//!
//! @author kongweiguang

use kerminal_application::AppError;
use kerminal_domain::files::FileEndpoint;

use super::endpoint as build_endpoint;

/// 相对路径按容器根解析，并折叠 Windows 分隔符与内部父目录段。
///
/// @author kongweiguang
#[test]
fn relative_windows_path_is_anchored_and_normalized() {
    let endpoint = container_endpoint(r"var\logs\..\current\./app.log")
        .expect("a normalized path within the container root is valid");

    assert_eq!(container_path(&endpoint), "/var/current/app.log");
}

/// POSIX 与 Windows 路径拒绝真正越根，同时保留普通的多点文件名。
///
/// @author kongweiguang
#[test]
fn absolute_path_cannot_escape_container_root() {
    assert!(container_endpoint("/var/../../etc/passwd").is_err());
    assert!(container_endpoint(r"\var\..\..\etc\passwd").is_err());
    assert!(container_endpoint(r"..\..\etc\passwd").is_err());
    assert_eq!(
        container_path(
            &container_endpoint("....etcpasswd").expect("dots inside a filename are valid")
        ),
        "/....etcpasswd"
    );
}

/// 返回测试 endpoint 的路径文本，明确只检查 Container 变体。
fn container_path(endpoint: &FileEndpoint) -> &str {
    match endpoint {
        FileEndpoint::Container { path, .. } => path.as_str(),
        _ => panic!("expected a container endpoint"),
    }
}

/// 固定无凭据容器身份，只让路径用例覆盖 endpoint 的路径规则。
///
/// @author kongweiguang
fn container_endpoint(path: &str) -> Result<FileEndpoint, AppError> {
    build_endpoint(
        "00000000-0000-4000-8000-000000000001",
        "app-container",
        None,
        path,
    )
}
