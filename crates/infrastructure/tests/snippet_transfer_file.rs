//! 片段传输文件适配器的有界读写与未知提交回读测试。
//!
//! @author kongweiguang

use std::fs;

#[cfg(unix)]
use std::path::PathBuf;

use kerminal_application::commands::{SnippetTransferFileConfirmation, SnippetTransferFilePort};
use kerminal_infrastructure::commands::SystemSnippetTransferFileAdapter;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

/// 读取接口只接受普通 UTF-8 文件并返回文本供现有 JSON 预览用例解析。
#[tokio::test]
async fn reads_bounded_utf8_import_file() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("snippets.json");
    fs::write(&path, br#"{"schemaVersion":1,"snippets":[]}"#).expect("write import file");

    let source = SystemSnippetTransferFileAdapter
        .read_import_file(path, 128, CancellationToken::new())
        .await
        .expect("read import file");

    assert!(source.contains("schemaVersion"));
}

/// 超出调用方上限的源文件不会被完整读取或返回。
#[tokio::test]
async fn rejects_import_file_over_limit() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("large.json");
    fs::write(&path, b"12345").expect("write import file");

    let result = SystemSnippetTransferFileAdapter
        .read_import_file(path, 4, CancellationToken::new())
        .await;

    assert!(result.is_err());
}

/// 导出新文件后可通过相同目标读回确认精确内容。
#[tokio::test]
async fn atomically_creates_export_file() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("snippets.json");
    let source = r#"{"schemaVersion":1,"snippets":[]}"#.to_owned();

    let receipt = SystemSnippetTransferFileAdapter
        .write_export_file(path.clone(), source.clone(), 128, CancellationToken::new())
        .await
        .expect("write export file");

    assert_eq!(receipt.bytes_written, source.len());
    assert_eq!(
        receipt.confirmation,
        SnippetTransferFileConfirmation::AtomicCommit
    );
    assert_eq!(fs::read_to_string(path).expect("read export file"), source);
}

/// 明确选择已有普通文件作为目标时使用原子替换，不留下半份正文。
#[tokio::test]
async fn atomically_replaces_existing_export_file() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("snippets.json");
    fs::write(&path, b"old").expect("write old file");
    let source = r#"{"schemaVersion":1,"snippets":[{"title":"new"}]}"#.to_owned();

    SystemSnippetTransferFileAdapter
        .write_export_file(path.clone(), source.clone(), 128, CancellationToken::new())
        .await
        .expect("replace export file");

    assert_eq!(fs::read_to_string(path).expect("read export file"), source);
}

/// 符号链接导入目标拒绝跟随，避免路径选择器意图被重定向。
#[cfg(unix)]
#[tokio::test]
async fn rejects_import_symlink() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().expect("temporary directory");
    let source = directory.path().join("source.json");
    let link = directory.path().join("link.json");
    fs::write(&source, b"{}").expect("write source");
    symlink(&source, &link).expect("create symlink");

    let result = SystemSnippetTransferFileAdapter
        .read_import_file(PathBuf::from(link), 128, CancellationToken::new())
        .await;

    assert!(result.is_err());
}
