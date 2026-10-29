//! `SystemFileClipboard` 的路径分配、边界和 Win32 payload 单元测试。
//!
//! @author kongweiguang

use std::path::Path;

use kerminal_domain::files::TransferKind;

use super::{
    remove_download_target, reserve_unique_download_path, safe_local_entry_name,
    split_local_file_name,
};

/// 将 Windows 不可用字符改为下划线，并丢弃路径分隔以阻止下载目录逃逸。
#[test]
fn local_name_sanitization_removes_path_components_and_invalid_chars() {
    assert_eq!(
        safe_local_entry_name("../bad:name?.txt", "remote-file"),
        "_bad_name_.txt"
    );
    assert_eq!(safe_local_entry_name("...", "remote-file"), "remote-file");
}

/// 同名目标通过原子占名获得编号后缀，并保留扩展名。
#[test]
fn download_targets_are_unique_and_keep_file_extension() {
    let directory = tempfile::tempdir().expect("temporary downloads directory");
    let first = reserve_unique_download_path(directory.path(), "report.txt", TransferKind::File)
        .expect("first download target");
    let second = reserve_unique_download_path(directory.path(), "report.txt", TransferKind::File)
        .expect("second download target");
    assert_eq!(
        first.file_name().and_then(|name| name.to_str()),
        Some("report.txt")
    );
    assert_eq!(
        second.file_name().and_then(|name| name.to_str()),
        Some("report (2).txt")
    );
    assert!(Path::new(&first).is_file());
    assert!(Path::new(&second).is_file());
}

/// 目录目标按 main 的行为也占用唯一目录名，避免空目录 placeholder 与真实目录冲突。
#[test]
fn directory_download_targets_reserve_unique_directories() {
    let directory = tempfile::tempdir().expect("temporary downloads directory");
    let first = reserve_unique_download_path(directory.path(), "archive", TransferKind::Directory)
        .expect("first directory target");
    let second = reserve_unique_download_path(directory.path(), "archive", TransferKind::Directory)
        .expect("second directory target");
    assert_eq!(
        first.file_name().and_then(|name| name.to_str()),
        Some("archive")
    );
    assert_eq!(
        second.file_name().and_then(|name| name.to_str()),
        Some("archive (2)")
    );
    assert!(first.is_dir());
    assert!(second.is_dir());
}

/// 编号操作只拆分最后一个扩展名，兼容无扩展和点开头文件。
#[test]
fn local_file_names_split_extensions_without_losing_dotfiles() {
    assert_eq!(
        split_local_file_name("report.tar.gz"),
        ("report.tar".to_owned(), ".gz".to_owned())
    );
    assert_eq!(
        split_local_file_name(".env"),
        (".env".to_owned(), String::new())
    );
}

/// 失败清理只接受本服务下载目录的直接子项，避免调用方路径误删到其它目录。
#[test]
fn cleanup_rejects_targets_outside_the_download_directory() {
    let downloads = tempfile::tempdir().expect("temporary downloads directory");
    let elsewhere = tempfile::tempdir().expect("temporary unrelated directory");
    let target = elsewhere.path().join("keep.txt");
    std::fs::write(&target, "keep").expect("create unrelated file");
    assert!(remove_download_target(downloads.path(), &target, TransferKind::File).is_err());
    assert!(target.exists());
}

#[cfg(windows)]
mod windows_tests {
    use std::{mem::size_of, path::PathBuf};

    use super::super::{DropFiles, dropfiles_payload};

    /// Shell 文件列表使用 UTF-16 并由双 NUL 结束，保证路径保持完整且没有拼接歧义。
    #[test]
    fn dropfiles_payload_contains_unicode_paths_and_double_terminator() {
        let paths = [
            PathBuf::from(r"C:\Downloads\one.txt"),
            PathBuf::from(r"C:\下载\two.txt"),
        ];
        let payload = dropfiles_payload(&paths).expect("valid CF_HDROP payload");
        let body_offset = size_of::<DropFiles>();
        assert_eq!(
            u32::from_le_bytes(payload[..4].try_into().expect("DROPFILES offset")) as usize,
            body_offset
        );
        let body = payload[body_offset..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
            .collect::<Vec<_>>();
        let mut expected = Vec::new();
        for path in [r"C:\Downloads\one.txt", r"C:\下载\two.txt"] {
            expected.extend(path.encode_utf16());
            expected.push(0);
        }
        expected.push(0);
        assert_eq!(body, expected);
    }
}
