//! 只接受受限 gzip tar 条目；不调用 tar 的自动解包。
//! @author kongweiguang

use flate2::read::MultiGzDecoder;
use kerminal_application::{AppError, AppResult};
use kerminal_plugin_protocol::{
    MAX_PACKAGE_BYTES, MAX_PACKAGE_ENTRIES, MAX_PACKAGE_FILE_BYTES, validate_package_path,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::Path,
};
use tokio_util::sync::CancellationToken;

const MAX_STREAM_BYTES: u64 = MAX_PACKAGE_BYTES + 32 * 1024 * 1024;

/// 即使 gzip 声明的压缩比异常，也不能无限读取 tar 的填充或无效尾部。
struct Bounded<R> {
    inner: R,
    remaining: u64,
    tail: VecDeque<u8>,
    consumed: u64,
}
impl<R: Read> Read for Bounded<R> {
    /// 达到限额后再试读一个字节，区分恰好结束和仍有内容。
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut byte = [0];
            return match self.inner.read(&mut byte)? {
                0 => Ok(0),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "archive stream limit",
                )),
            };
        }
        let limit = buffer.len().min(self.remaining as usize);
        let read = self.inner.read(&mut buffer[..limit])?;
        self.remaining -= read as u64;
        self.consumed += read as u64;
        let tail = if read > 1024 {
            &buffer[read - 1024..read]
        } else {
            &buffer[..read]
        };
        let excess = (self.tail.len() + tail.len()).saturating_sub(1024);
        self.tail.drain(..excess);
        self.tail.extend(tail.iter().copied());
        Ok(read)
    }
}

/// 所有路径及父目录按大小写折叠登记，避免 Windows 上的别名和文件目录冲突。
fn remember_path(
    paths: &mut BTreeMap<String, (String, bool)>,
    path: &str,
    is_dir: bool,
) -> AppResult<()> {
    let parts: Vec<_> = path.split('/').collect();
    for index in 1..=parts.len() {
        let name = parts[..index].join("/");
        let directory = index < parts.len() || is_dir;
        let key = name.to_lowercase();
        match paths.get(&key) {
            Some((original, existing_dir)) if original != &name || !*existing_dir || !directory => {
                return Err(AppError::invalid("插件归档路径冲突"));
            }
            Some(_) => {}
            None => {
                paths.insert(key, (name, directory));
            }
        }
    }
    Ok(())
}

/// 目标必须是调用方新建的私有空目录；逐项限额、同步并拒绝所有链接和扩展类型。
pub(super) fn unpack(
    archive_path: &Path,
    destination: &Path,
    cancellation: &CancellationToken,
) -> AppResult<()> {
    let file = fs::File::open(archive_path)
        .map_err(|e| AppError::infrastructure_with_source("读取插件安装包失败", e))?;
    let decoder = MultiGzDecoder::new(file);
    let reader = Bounded {
        inner: decoder,
        remaining: MAX_STREAM_BYTES,
        tail: VecDeque::with_capacity(1024),
        consumed: 0,
    };
    let mut archive = tar::Archive::new(reader);
    let mut paths = BTreeMap::new();
    let mut entries_seen = BTreeSet::new();
    let mut count = 0usize;
    let mut total = 0u64;
    for entry in archive
        .entries()
        .map_err(|e| AppError::infrastructure_with_source("读取插件归档失败", e))?
        .raw(true)
    {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件安装已取消"));
        }
        let mut entry =
            entry.map_err(|e| AppError::infrastructure_with_source("读取插件归档条目失败", e))?;
        count += 1;
        if count > MAX_PACKAGE_ENTRIES {
            return Err(AppError::invalid("插件归档条目超过限额"));
        }
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err(AppError::invalid("插件归档含不支持的条目"));
        }
        if entry
            .header()
            .link_name()
            .map_err(|_| AppError::invalid("插件归档链接无效"))?
            .is_some()
        {
            return Err(AppError::invalid("插件归档不允许链接"));
        }
        let raw = entry.path_bytes();
        let raw =
            std::str::from_utf8(&raw).map_err(|_| AppError::invalid("插件归档路径编码无效"))?;
        let name = if kind.is_dir() {
            raw.strip_suffix('/').unwrap_or(raw)
        } else {
            raw
        };
        validate_package_path(name).map_err(|_| AppError::invalid("插件归档路径无效"))?;
        if !entries_seen.insert(name.to_lowercase()) {
            return Err(AppError::invalid("插件归档条目重复"));
        }
        remember_path(&mut paths, name, kind.is_dir())?;
        let size = entry.size();
        total = total
            .checked_add(size)
            .ok_or_else(|| AppError::invalid("插件归档大小溢出"))?;
        if size > MAX_PACKAGE_FILE_BYTES
            || total > MAX_PACKAGE_BYTES
            || (kind.is_dir() && size != 0)
        {
            return Err(AppError::invalid("插件归档大小超过限额"));
        }
        let target = destination.join(name);
        if kind.is_dir() {
            fs::create_dir_all(&target)
                .map_err(|e| AppError::infrastructure_with_source("创建插件目录失败", e))?;
            continue;
        }
        let parent = target
            .parent()
            .ok_or_else(|| AppError::invalid("插件归档路径无效"))?;
        fs::create_dir_all(parent)
            .map_err(|e| AppError::infrastructure_with_source("创建插件目录失败", e))?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|e| AppError::infrastructure_with_source("创建插件文件失败", e))?;
        let mut left = size;
        let mut buffer = [0u8; 65536];
        while left > 0 {
            if cancellation.is_cancelled() {
                return Err(AppError::cancelled("插件安装已取消"));
            }
            let wanted = buffer.len().min(left as usize);
            let read = entry
                .read(&mut buffer[..wanted])
                .map_err(|e| AppError::infrastructure_with_source("解压插件文件失败", e))?;
            if read == 0 {
                return Err(AppError::invalid("插件归档文件被截断"));
            }
            output
                .write_all(&buffer[..read])
                .map_err(|e| AppError::infrastructure_with_source("写入插件文件失败", e))?;
            left -= read as u64;
        }
        output
            .sync_all()
            .map_err(|e| AppError::infrastructure_with_source("同步插件文件失败", e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = entry
                .header()
                .mode()
                .map_err(|_| AppError::invalid("插件文件权限无效"))?;
            fs::set_permissions(
                &target,
                fs::Permissions::from_mode(if mode & 0o111 != 0 { 0o755 } else { 0o644 }),
            )
            .map_err(|e| AppError::infrastructure_with_source("设置插件文件权限失败", e))?;
        }
    }
    // 读取到 gzip 末尾以核对 CRC，并拒绝 tar 结束块之后的非零内容。
    let mut reader = archive.into_inner();
    let mut buffer = [0u8; 65536];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|e| AppError::infrastructure_with_source("验证插件归档尾部失败", e))?;
        if read == 0 {
            break;
        }
        if buffer[..read].iter().any(|b| *b != 0) {
            return Err(AppError::invalid("插件归档尾部含未声明内容"));
        }
    }
    if reader.consumed < 1024
        || reader.tail.len() != 1024
        || reader.tail.iter().any(|byte| *byte != 0)
    {
        return Err(AppError::invalid("插件归档结束块缺失"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../../tests/unit/plugin_install_archive.rs"]
mod tests;
