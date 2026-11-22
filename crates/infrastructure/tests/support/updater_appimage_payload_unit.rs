//! 更新包解析只使用内存夹具，不操作用户安装目录。
//!
//! @author kongweiguang

use std::io::Write;

use flate2::{Compression, write::GzEncoder};

use super::prepare;

/// 构造 ELF64 AppImage 标识以覆盖归档边界，不声称其可实际启动。
fn appimage() -> Vec<u8> {
    let mut payload = vec![0_u8; 256];
    payload[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    payload[8..11].copy_from_slice(b"AI\x02");
    payload[18..20].copy_from_slice(&[0x3e, 0]);
    payload[16] = 3;
    payload[20] = 1;
    payload[40] = 64;
    payload[52] = 64;
    payload[58] = 64;
    payload[60] = 1;
    payload[128..132].copy_from_slice(b"hsqs");
    payload[168] = 128;
    payload
}

/// 用常规 tar 打包创建可控条目，模拟已验签的下载字节。
fn archive(entries: &[(&str, &[u8], tar::EntryType)]) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for (name, bytes, kind) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(*kind);
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, name, *bytes)
            .expect("append fixture entry");
    }
    let mut encoder = builder.into_inner().expect("finish tar");
    encoder.flush().expect("flush gzip");
    encoder.finish().expect("finish gzip")
}

/// 单条归档和原始 AppImage 都还原相同数据，其余结构不得进入 SDK。
#[test]
fn accepts_one_signed_archive_member_and_valid_raw() {
    let payload = appimage();
    let archive = archive(&[("test.AppImage", &payload, tar::EntryType::Regular)]);
    assert_eq!(prepare(archive, 1024).expect("extract archive"), payload);
    assert_eq!(prepare(payload.clone(), 1024).expect("accept raw"), payload);
}

/// 有歧义、损坏、越界或不具备可执行格式标记的输入均在安装前拒绝。
#[test]
fn rejects_ambiguous_malformed_and_oversized_payloads() {
    let payload = appimage();
    let cases = [
        archive(&[]),
        archive(&[
            ("one.AppImage", &payload, tar::EntryType::Regular),
            ("two.AppImage", &payload, tar::EntryType::Regular),
        ]),
        archive(&[("file.txt", &payload, tar::EntryType::Regular)]),
        archive(&[("dir/test.AppImage", &payload, tar::EntryType::Regular)]),
        archive(&[("test.AppImage", &[], tar::EntryType::Symlink)]),
        archive(&[("test.AppImage", b"not an ELF", tar::EntryType::Regular)]),
        vec![0x1f, 0x8b, 0x08],
        b"not an AppImage".to_vec(),
    ];
    for bytes in cases {
        assert!(prepare(bytes, 1024).is_err());
    }
    let oversized = archive(&[("test.AppImage", &payload, tar::EntryType::Regular)]);
    assert!(prepare(oversized, 64).is_err());
    let mut concatenated = archive(&[("one.AppImage", &payload, tar::EntryType::Regular)]);
    concatenated.extend(archive(&[(
        "two.AppImage",
        &payload,
        tar::EntryType::Regular,
    )]));
    assert!(prepare(concatenated, 1024).is_err());
    let mut fake = payload.clone();
    fake[128..132].fill(0);
    assert!(prepare(fake, 1024).is_err());
    let mut overflowing = payload.clone();
    overflowing[168..176].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(prepare(overflowing, 1024).is_err());
    assert!(prepare(payload, 64).is_err());
}

/// gzip 校验失败必须拒绝，不能因 tar 已结束而忽略末尾损坏。
#[test]
fn rejects_corrupted_gzip_checksum() {
    let payload = appimage();
    let mut bytes = archive(&[("test.AppImage", &payload, tar::EntryType::Regular)]);
    let index = bytes.len() - 8;
    bytes[index] ^= 1;
    assert!(prepare(bytes, 1024).is_err());
}
