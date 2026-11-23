//! 文件握手、文件安全及协议状态回归。
//!
//! @author kongweiguang

use super::*;

/// 每个分块边界都必须正确接管握手，同时保留前后普通输出。
#[test]
fn detects_split_lrzsz_headers_without_losing_output() {
    for (header, expected) in [
        (b"**\x18B0100000023be50".as_slice(), ZmodemDirection::Upload),
        (
            b"**\x18B00000000000000".as_slice(),
            ZmodemDirection::Download,
        ),
    ] {
        let mut bytes = b"before ".to_vec();
        bytes.extend_from_slice(header);
        bytes.extend_from_slice(b"\r\nafter");
        for split in 0..=bytes.len() {
            let mut detector = HandshakeDetector::default();
            let first = detector.push(&bytes[..split]);
            if let Some(handshake) = first.handshake {
                assert_eq!(handshake.direction, expected);
                assert_eq!(handshake.frame, header);
                assert_eq!(first.visible, b"before ");
                let mut rest = first.remaining;
                rest.extend_from_slice(&bytes[split..]);
                assert_eq!(rest, b"\r\nafter");
            } else {
                let second = detector.push(&bytes[split..]);
                let handshake = second.handshake.expect("valid split handshake");
                let mut visible = first.visible;
                visible.extend_from_slice(&second.visible);
                assert_eq!(visible, b"before ");
                assert_eq!(handshake.direction, expected);
                assert_eq!(handshake.frame, header);
                assert_eq!(second.remaining, b"\r\nafter");
            }
        }
    }
}

/// CRC 失败或部分前缀不应触发协议，并须能原样释放。
#[test]
fn rejects_bad_headers_and_flushes_partial_text() {
    let mut detector = HandshakeDetector::default();
    let bytes = b"hello **\x18B0100000023be51 world*";
    let detected = detector.push(bytes);
    assert!(detected.handshake.is_none());
    assert!(detector.has_pending());
    let mut visible = detected.visible;
    visible.extend(detector.flush());
    assert_eq!(visible, bytes);
    assert!(!detector.has_pending());
}

/// 下载不能越出目标目录，也不能覆盖已存在文件。
#[test]
fn downloads_are_staged_no_clobber_and_cleanup_on_cancel() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let existing = directory.path().join("测试.txt");
    fs::write(&existing, b"existing").expect("existing file");
    for name in ["../escape", "sub\\escape", "C:evil", "CON.txt", "bad."] {
        assert!(matches!(
            DownloadFile::create(directory.path(), name.as_bytes()),
            Err(TransferError::InvalidName)
        ));
    }
    let mut download =
        DownloadFile::create(directory.path(), "测试.txt".as_bytes()).expect("safe file");
    download.write(b"new file").expect("write temporary file");
    let destination = download
        .commit(&CancellationToken::new())
        .expect("commit new name");
    assert_eq!(
        destination.file_name().and_then(|name| name.to_str()),
        Some("测试 (1).txt")
    );
    assert_eq!(fs::read(&existing).expect("existing contents"), b"existing");
    assert_eq!(fs::read(destination).expect("new contents"), b"new file");
    let mut cancelled =
        DownloadFile::create(directory.path(), b"cancelled.txt").expect("temporary file");
    cancelled.write(b"partial").expect("partial data");
    let token = CancellationToken::new();
    token.cancel();
    assert!(matches!(
        cancelled.commit(&token),
        Err(TransferError::Cancelled)
    ));
    assert!(!directory.path().join("cancelled.txt").exists());
}

/// 协商转义能力必须在分次写入时保持字节一致，且不能破坏帧校验。
#[test]
fn download_requests_control_escaping_across_partial_writes() {
    let mut protocol = ZmodemProtocol::new(ZmodemDirection::Download).expect("create receiver");
    let wire = match protocol.poll() {
        ProtocolAction::WriteWire(wire) => wire,
        other => panic!("expected receiver handshake, got {other:?}"),
    };
    assert_eq!(
        is_initial_header(&wire[..18]),
        Some(ZmodemDirection::Upload)
    );
    assert_eq!(wire[12], b'6');
    protocol.wire_written(7);
    assert_eq!(
        protocol.poll(),
        ProtocolAction::WriteWire(wire[7..].to_vec())
    );
    protocol.wire_written(wire.len() - 7);
    assert_eq!(protocol.poll(), ProtocolAction::Idle);
}

/// 三种 ZMODEM 帧格式都可能携带跳过；分块和错误 CRC 不得改变结果。
#[test]
fn validates_skip_headers_across_all_chunk_boundaries() {
    let payload = [5_u8, 0, 0, 0, 0];
    let checksum = crc16(&payload).to_be_bytes();
    let mut hex = b"**\x18B".to_vec();
    for byte in payload.into_iter().chain(checksum) {
        hex.extend_from_slice(format!("{byte:02x}").as_bytes());
    }
    let mut binary = b"*\x18A".to_vec();
    binary.extend_from_slice(&payload);
    binary.extend_from_slice(&checksum);
    let mut binary32 = b"*\x18C".to_vec();
    binary32.extend_from_slice(&payload);
    binary32.extend_from_slice(&crc32(&payload).to_le_bytes());
    for header in [hex, binary, binary32] {
        for split in 0..=header.len() {
            let mut scanner = HeaderScanner::default();
            let first = scanner.push(&header[..split]);
            let second = scanner.push(&header[split..]);
            assert!(first || second, "missed header split at {split}");
        }
        let mut damaged = header;
        *damaged.last_mut().expect("checksum") ^= 1;
        assert!(!HeaderScanner::default().push(&damaged));
    }
}
