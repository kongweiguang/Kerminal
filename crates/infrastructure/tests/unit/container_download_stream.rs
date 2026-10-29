//! 容器归档 reader 已完成时仍可能收到 SSH 后续数据，必须先核对真实解包回执。
//!
//! @author kongweiguang

use std::{io, path::PathBuf};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::send_archive_chunk;

/// 已成功解包的 reader 关闭后收到 tar padding，不应误判为解包失败。
///
/// @author kongweiguang
#[tokio::test]
async fn trailing_archive_bytes_use_successful_extractor_receipt() {
    let (sender, receiver) = mpsc::channel(1);
    drop(receiver);
    let expected = PathBuf::from("staged-file");
    let result = expected.clone();
    let mut extractor = Some(tokio::spawn(async move { Ok(result) }));
    let actual = send_archive_chunk(
        &sender,
        &mut extractor,
        vec![0; 1024],
        &CancellationToken::new(),
    )
    .await
    .expect("finished extractor permits trailing archive bytes");
    assert_eq!(actual, Some(expected));
    assert!(extractor.is_none(), "completion is consumed exactly once");
}

/// 相同的通道关闭若源自损坏归档，必须保留解包错误，不能把所有关闭都当成成功。
///
/// @author kongweiguang
#[tokio::test]
async fn closed_reader_preserves_extractor_error() {
    let (sender, receiver) = mpsc::channel(1);
    drop(receiver);
    let mut extractor = Some(tokio::spawn(async {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid archive",
        ))
    }));
    let error = send_archive_chunk(
        &sender,
        &mut extractor,
        vec![0; 512],
        &CancellationToken::new(),
    )
    .await
    .expect_err("a failed extractor must not be accepted");
    assert_eq!(
        error.public_message(),
        "容器 tar 归档不完整或包含不安全路径"
    );
    assert!(extractor.is_none(), "failed completion is consumed once");
}
