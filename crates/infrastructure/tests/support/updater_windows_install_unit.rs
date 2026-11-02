//! Windows 安装路径资格回归；不读取或写入真实用户注册表。
//!
//! @author kongweiguang

use super::{decode_install_directory, installed_binary_matches};

/// 兼容 NSIS 实际带引号的路径，同时拒绝空值、单个引号和不完整注册值。
#[test]
fn registry_install_path_accepts_only_complete_quoting() {
    let quoted: Vec<_> = "\"C:\\Users\\User Name\\Kerminal\"\0"
        .encode_utf16()
        .collect();
    assert_eq!(
        decode_install_directory(&quoted),
        Some(std::path::PathBuf::from("C:\\Users\\User Name\\Kerminal"))
    );
    for text in ["\0", "\"\0", "\"C:\\Kerminal\0", "C:\\Kerminal\"\0"] {
        assert!(decode_install_directory(&text.encode_utf16().collect::<Vec<_>>()).is_none());
    }
}

/// 只有登记目录内的 Kerminal 主程序可更新，临时测试 EXE 与其他目录一律拒绝。
#[test]
fn executable_must_match_registered_install_directory() {
    let root = tempfile::tempdir().expect("isolated installation fixture");
    let installed = root.path().join("installed");
    let developer = root.path().join("developer");
    std::fs::create_dir_all(&installed).expect("create installed fixture");
    std::fs::create_dir_all(&developer).expect("create developer fixture");
    assert!(installed_binary_matches(
        &installed.join("kerminal.exe"),
        &installed
    ));
    assert!(!installed_binary_matches(
        &developer.join("kerminal.exe"),
        &installed
    ));
    assert!(!installed_binary_matches(
        &installed.join("other.exe"),
        &installed
    ));
    assert!(!installed_binary_matches(
        &installed.join("kerminal.exe"),
        &root.path().join("missing")
    ));
}
