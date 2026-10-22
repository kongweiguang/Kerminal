//! Windows 提交路径的词法转换边界。
//!
//! @author kongweiguang

use std::{ffi::OsString, io::ErrorKind, os::windows::ffi::OsStringExt, path::Path};

use crate::storage::windows_api_path;

/// 去掉 Win32 终止 NUL 并解码测试路径，便于精确核对命名空间前缀。
///
/// @author kongweiguang
fn decode_api_path(encoded: &[u16]) -> String {
    assert_eq!(
        encoded.last(),
        Some(&0),
        "Win32 path must be NUL terminated"
    );
    OsString::from_wide(&encoded[..encoded.len() - 1])
        .to_string_lossy()
        .into_owned()
}

/// 盘符路径扩展为 Win32 长路径形式，正斜杠也在边界统一成反斜杠。
///
/// @author kongweiguang
#[test]
fn drive_paths_use_extended_prefix_and_normalized_separators() {
    let encoded = windows_api_path(Path::new("C:/Users/Example/file.txt"))
        .expect("absolute drive path is valid");

    assert_eq!(decode_api_path(&encoded), r"\\?\C:\Users\Example\file.txt");
}

/// 普通 UNC 路径映射到 UNC 扩展命名空间，保留服务器与共享名。
///
/// @author kongweiguang
#[test]
fn unc_paths_use_extended_unc_prefix() {
    let encoded = windows_api_path(Path::new(r"\\server\share\folder\file.txt"))
        .expect("UNC path with server and share is valid");

    assert_eq!(
        decode_api_path(&encoded),
        r"\\?\UNC\server\share\folder\file.txt"
    );
}

/// 已扩展盘符和 UNC 路径保持原命名空间，不会重复添加前缀。
///
/// @author kongweiguang
#[test]
fn existing_extended_paths_are_preserved() {
    let drive = windows_api_path(Path::new(r"\\?\C:\already\extended.txt"))
        .expect("extended drive path is valid");
    let unc = windows_api_path(Path::new(r"\\?\UNC\server\share\existing.txt"))
        .expect("extended UNC path is valid");

    assert_eq!(decode_api_path(&drive), r"\\?\C:\already\extended.txt");
    assert_eq!(decode_api_path(&unc), r"\\?\UNC\server\share\existing.txt");
}

/// 相对提交路径只按当前工作目录词法绝对化，不要求待提交目标已存在。
///
/// @author kongweiguang
#[test]
fn relative_paths_are_absolutized_without_resolving_the_target() {
    let encoded = windows_api_path(Path::new(r"relative\not-created\target.bin"))
        .expect("relative path can be made absolute");
    let decoded = decode_api_path(&encoded);
    let path = Path::new(&decoded);

    assert!(path.is_absolute());
    assert!(decoded.starts_with(r"\\?\"));
    assert!(!path.exists(), "path conversion must not create the target");
}

/// 嵌入 NUL 在调用元数据或提交 API 前按无效输入拒绝。
///
/// @author kongweiguang
#[test]
fn embedded_nul_is_rejected_as_invalid_input() {
    let error = windows_api_path(Path::new("C:\\safe\\bad\0name"))
        .expect_err("Win32 cannot represent paths with embedded NUL");

    assert_eq!(error.kind(), ErrorKind::InvalidInput);
}

/// 不完整 UNC 名称不得被当成当前工作目录下的普通绝对路径。
///
/// @author kongweiguang
#[test]
fn incomplete_unc_paths_are_rejected() {
    let error = windows_api_path(Path::new(r"\\server"))
        .expect_err("UNC path requires both server and share");

    assert_eq!(error.kind(), ErrorKind::InvalidInput);
}
