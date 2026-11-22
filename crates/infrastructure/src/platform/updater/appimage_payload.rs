//! 已验签 Linux 更新包的格式与解压边界。
//!
//! @author kongweiguang

use std::io::{self, Cursor, Read};

use flate2::read::MultiGzDecoder;

/// 发布签名覆盖完整压缩包，因此只在验签后提取单个有界文件，再读到 gzip 结尾以检查校验和和尾部填充。
pub(super) fn prepare(bytes: Vec<u8>, maximum: u64) -> io::Result<Vec<u8>> {
    if bytes.starts_with(b"\x1f\x8b") {
        let decoder = MultiGzDecoder::new(Cursor::new(bytes));
        let mut archive = tar::Archive::new(decoder.take(maximum.saturating_add(1024 * 1024)));
        let mut entries = archive.entries()?;
        let mut entry = entries.next().ok_or_else(invalid)??;
        let path = entry.path()?;
        if !entry.header().entry_type().is_file()
            || path.components().count() != 1
            || path
                .extension()
                .is_none_or(|extension| extension != "AppImage")
        {
            return Err(invalid());
        }
        let size = entry.size();
        if size == 0 || size > maximum {
            return Err(invalid());
        }
        let size = usize::try_from(size).map_err(|_| invalid())?;
        let mut payload = Vec::new();
        payload.try_reserve_exact(size).map_err(|_| invalid())?;
        payload.resize(size, 0);
        entry.read_exact(&mut payload)?;
        drop(entry);
        if entries.next().is_some() {
            return Err(invalid());
        }
        let mut rest = archive.into_inner();
        let mut padding = [0_u8; 8192];
        let mut padding_bytes = 0_u64;
        loop {
            let count = rest.read(&mut padding)?;
            if count == 0 {
                break;
            }
            padding_bytes += count as u64;
            if padding[..count].iter().any(|byte| *byte != 0) {
                return Err(invalid());
            }
        }
        if rest.limit() == 0 || padding_bytes < 512 || !valid_header(&payload) {
            return Err(invalid());
        }
        Ok(payload)
    } else if bytes.len() as u64 <= maximum && valid_header(&bytes) {
        Ok(bytes)
    } else {
        Err(invalid())
    }
}

/// 校验本项目 x86_64 Type 2 的 ELF 节表和内嵌 SquashFS 边界，避免把非安装包交给 SDK。
fn valid_header(bytes: &[u8]) -> bool {
    if bytes.len() < 64
        || !bytes.starts_with(b"\x7fELF\x02\x01\x01")
        || bytes.get(8..11) != Some(b"AI\x02")
        || !matches!(bytes.get(16..18), Some([2, 0] | [3, 0]))
        || bytes.get(18..20) != Some(&[0x3e, 0x00])
        || bytes.get(20..24) != Some(&[1, 0, 0, 0])
        || bytes.get(52..54) != Some(&[64, 0])
    {
        return false;
    }
    let Some(offset) = bytes
        .get(40..48)
        .and_then(|value| value.try_into().ok())
        .map(u64::from_le_bytes)
    else {
        return false;
    };
    let section_size = u16::from_le_bytes([bytes[58], bytes[59]]);
    let section_count = u16::from_le_bytes([bytes[60], bytes[61]]);
    if offset < 64 || section_size != 64 || section_count == 0 {
        return false;
    }
    let Some(start) = offset
        .checked_add(u64::from(section_size) * u64::from(section_count))
        .and_then(|value| usize::try_from(value).ok())
    else {
        return false;
    };
    let Some(superblock) = bytes.get(start..).and_then(|remaining| remaining.get(..96)) else {
        return false;
    };
    if superblock.get(..4) != Some(b"hsqs") {
        return false;
    }
    let Some(used) = superblock
        .get(40..48)
        .and_then(|value| value.try_into().ok())
        .map(u64::from_le_bytes)
    else {
        return false;
    };
    used >= 96 && used <= (bytes.len() - start) as u64
}

/// 各种结构错误都在目标替换之前返回，不泄露归档路径。
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid AppImage update payload",
    )
}

#[cfg(test)]
#[path = "../../../tests/support/updater_appimage_payload_unit.rs"]
mod tests;
