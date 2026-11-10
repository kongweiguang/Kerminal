//! 可恢复下载和主动取消的本地传输回归。
//! @author kongweiguang

use kerminal_application::extensions::{CatalogAsset, InstallProgress};
use kerminal_plugin_protocol::Platform;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::watch,
};
use tokio_util::sync::CancellationToken;

use super::download;

/// 首次连接故意截断，第二次断言 Range 后返回与目录大小一致的剩余字节。
#[tokio::test]
async fn interrupted_download_resumes_with_matching_range() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        for index in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = [0u8; 4096];
            let mut read = 0;
            while !request[..read].ends_with(b"\r\n\r\n") {
                let count = stream.read(&mut request[read..]).await.expect("request");
                assert!(count > 0 && read + count < request.len());
                read += count;
            }
            let request = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
            if index == 0 {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"test\"\r\nConnection: close\r\n\r\nabc",
                    )
                    .await
                    .expect("first response");
            } else {
                assert!(request.contains("range: bytes=3-"), "{request}");
                assert!(request.contains("if-range: \"test\""), "{request}");
                stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nETag: \"test\"\r\nConnection: close\r\n\r\ndef").await.expect("second response");
            }
        }
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let asset = CatalogAsset {
        platform: Platform::Any,
        url: format!("http://{address}/plugin"),
        size: 6,
        sha256: "0".repeat(64),
    };
    let (progress, receiver) = watch::channel(InstallProgress::Preparing);
    download(&asset, &path, true, &progress, &CancellationToken::new())
        .await
        .expect("download");
    server.await.expect("server");
    assert_eq!(std::fs::read(path).expect("download bytes"), b"abcdef");
    assert_eq!(
        *receiver.borrow(),
        InstallProgress::Downloading {
            received: 6,
            total: 6
        }
    );
}

/// 已取消请求不应建立网络连接或创建下载文件。
#[tokio::test]
async fn cancelled_download_does_not_create_staging_file() {
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let asset = CatalogAsset {
        platform: Platform::Any,
        url: "http://127.0.0.1:1/plugin".into(),
        size: 1,
        sha256: "0".repeat(64),
    };
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(
        download(&asset, &path, true, &progress, &cancellation)
            .await
            .is_err()
    );
    assert!(!path.exists());
}

/// 服务器保留半响应不结束，确保取消命中真实流读取等待，并验证连接、重试与落盘均收尾。
#[tokio::test]
async fn cancellation_during_partial_response_closes_connection_and_stops_writes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut request = [0u8; 4096];
        let mut read = 0;
        while !request[..read].ends_with(b"\r\n\r\n") {
            let count = stream.read(&mut request[read..]).await.expect("request");
            assert!(count > 0 && read + count < request.len());
            read += count;
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nETag: \"partial\"\r\nConnection: keep-alive\r\n\r\nabc")
            .await
            .expect("partial response");
        stream.flush().await.expect("partial response flush");

        let mut pending = [0u8; 1];
        let closed = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut pending))
            .await
            .expect("cancelled download must close its connection");
        match closed {
            Ok(0) => {}
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            result => panic!("cancelled connection remained active: {result:?}"),
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "cancellation must not start a resumed request"
        );
    });
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("download");
    let asset = CatalogAsset {
        platform: Platform::Any,
        url: format!("http://{address}/plugin"),
        size: 6,
        sha256: "0".repeat(64),
    };
    let (progress, mut receiver) = watch::channel(InstallProgress::Preparing);
    let cancellation = CancellationToken::new();
    let task_path = path.clone();
    let task_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        download(&asset, &task_path, true, &progress, &task_cancellation).await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            receiver.changed().await.expect("download progress");
            if matches!(
                *receiver.borrow_and_update(),
                InstallProgress::Downloading {
                    received: 3,
                    total: 6
                }
            ) {
                break;
            }
        }
    })
    .await
    .expect("download must consume the partial body before cancellation");

    cancellation.cancel();
    let error = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("active stream cancellation must return promptly")
        .expect("download task")
        .expect_err("cancelled download");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Cancelled
    );
    let bytes_after_cancel = tokio::fs::read(&path)
        .await
        .expect("cancelled partial bytes");
    assert_eq!(bytes_after_cancel, b"abc");
    assert_eq!(
        *receiver.borrow(),
        InstallProgress::Downloading {
            received: 3,
            total: 6
        }
    );
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("server must observe closed connection")
        .expect("server");
    assert_eq!(
        tokio::fs::read(&path).await.expect("stable partial bytes"),
        bytes_after_cancel,
        "no further writes may occur after cancellation"
    );
}

/// 恶意服务器不能把错误的 Content-Range 接到已有暂存内容之后。
#[tokio::test]
async fn mismatched_content_range_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = [0; 4096];
        let _ = stream.read(&mut buffer).await.unwrap();
        stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 2-4/6\r\nConnection: close\r\n\r\ndef").await.unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("download");
    std::fs::write(&path, b"abc").unwrap();
    let asset = CatalogAsset {
        platform: Platform::Any,
        url: format!("http://{address}/plugin"),
        size: 6,
        sha256: "0".repeat(64),
    };
    let (progress, _) = watch::channel(InstallProgress::Preparing);
    assert!(
        download(&asset, &path, true, &progress, &CancellationToken::new())
            .await
            .is_err()
    );
    server.await.unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"abc");
}
