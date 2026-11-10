//! 插件归档的路径和条目边界回归。
//! @author kongweiguang

use flate2::{Compression, write::GzEncoder};
use std::io::{Cursor, Write};
use tokio_util::sync::CancellationToken;

use super::unpack;

/// 构造原始 header，允许测试正常 builder 会主动拒绝的危险路径。
fn archive(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (name, kind, bytes) in entries {
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(*kind);
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        let name_bytes = name.as_bytes();
        header.as_mut_bytes()[..name_bytes.len()].copy_from_slice(name_bytes);
        header.set_cksum();
        tar.append(&header, Cursor::new(*bytes)).expect("tar entry");
    }
    let raw = tar.into_inner().expect("tar finish");
    let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
    gzip.write_all(&raw).expect("gzip write");
    gzip.finish().expect("gzip finish")
}

/// 每个样本使用隔离目录，防止残留文件改变后续碰撞结果。
fn attempt(bytes: &[u8]) -> bool {
    let root = tempfile::tempdir().expect("temporary root");
    let source = root.path().join("package.kplugin");
    std::fs::write(&source, bytes).expect("write archive");
    let destination = root.path().join("extract");
    std::fs::create_dir(&destination).expect("extract directory");
    unpack(&source, &destination, &CancellationToken::new()).is_ok()
}

/// 普通嵌套文件应能解压，危险条目则不能依赖平台文件系统自行拒绝。
#[test]
fn archive_rejects_traversal_links_and_case_collisions() {
    assert!(attempt(&archive(&[(
        "views/editor.json",
        tar::EntryType::Regular,
        b"{}"
    )])));
    for entries in [
        vec![("../escape", tar::EntryType::Regular, b"x".as_slice())],
        vec![("folder\\escape", tar::EntryType::Regular, b"x".as_slice())],
        vec![("CON.txt", tar::EntryType::Regular, b"x".as_slice())],
        vec![("link", tar::EntryType::Symlink, b"".as_slice())],
        vec![
            ("Foo/a", tar::EntryType::Regular, b"a".as_slice()),
            ("foo/b", tar::EntryType::Regular, b"b".as_slice()),
        ],
        vec![
            ("file", tar::EntryType::Regular, b"a".as_slice()),
            ("file/child", tar::EntryType::Regular, b"b".as_slice()),
        ],
    ] {
        assert!(!attempt(&archive(&entries)));
    }
}

/// 未读到 gzip 校验尾部不能把截断数据当成已经解压成功。
#[test]
fn archive_rejects_truncated_gzip() {
    let mut bytes = archive(&[("data.txt", tar::EntryType::Regular, b"data")]);
    bytes.truncate(bytes.len() - 5);
    assert!(!attempt(&bytes));
}
