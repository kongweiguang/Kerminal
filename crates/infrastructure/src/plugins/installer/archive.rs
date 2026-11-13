//! 只接受受限 gzip tar 条目；不调用 tar 的自动解包。
//! @author kongweiguang

use flate2::read::MultiGzDecoder;
use kerminal_application::{AppError, AppResult};
use kerminal_plugin_protocol::{
    MAX_PACKAGE_BYTES, MAX_PACKAGE_ENTRIES, MAX_PACKAGE_FILE_BYTES, validate_package_path,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::Path,
};
use tokio_util::sync::CancellationToken;

const MAX_STREAM_BYTES: u64 = MAX_PACKAGE_BYTES + 32 * 1024 * 1024;

/// 解压读取途中被取消的标记错误；只作类型标记，公开文案由 AppError 层统一给出。
///
/// 故意不使用 `ErrorKind::Interrupted`：`read_exact` 会把 Interrupted 当瞬时错误
/// 吞掉并重试，取消会被静默吞掉；固定 Other 类别加独立标记类型可以穿透解码器、
/// tar 和 read_exact 的重试逻辑，再由映射层按类型精确还原为 AppError::Cancelled。
#[derive(Debug)]
struct CancelledRead;

impl std::fmt::Display for CancelledRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("archive read cancelled")
    }
}
impl std::error::Error for CancelledRead {}

/// 压缩输入预算耗尽的标记错误，携带已读字节数供测试与诊断核对真实读取上限。
#[derive(Debug)]
struct InputBudgetExceeded {
    counted: u64,
}

impl std::fmt::Display for InputBudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "archive compressed input budget exceeded after {} bytes",
            self.counted
        )
    }
}
impl std::error::Error for InputBudgetExceeded {}

/// 解压全程逐块核对的可信 size/SHA-256，数值必须来自签名目录等可信来源。
///
/// 字节摘要按小写十六进制编码，与 installer::verify_archive 落盘的格式一致。
pub(super) struct ArchiveExpectation {
    pub(super) size: u64,
    pub(super) sha256: String,
}

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

/// 文件与 gzip 解码器之间的透明计数、SHA-256 哈希、预算与取消层；哈希器由调用栈持有。
///
/// 必须插在 MultiGzDecoder 之下（File 之上）而不是解码器之外：`read::MultiGzDecoder`
/// 对外输出的是解压后的字节，只有包住 File 才能哈希到实际进入解码器的压缩字节；
/// 解码器内部 BufReader 的预读同样经过本层，因此预读字节也会被计入，不存在被跳过
/// 的“未读压缩尾巴”。`limit` 是验证路径按可信期望压缩大小+1 得到的输入上限：大量
/// 空 gzip 成员可以在几乎不产生解压字节的情况下把压缩输入撑到任意大，解压端
/// MAX_STREAM_BYTES 约束不到压缩输入，必须在读取过程中限制并及时失败。取消检查
/// 放在每次读取边界：MultiGzDecoder 单次 read 可能连续消费大量空成员而不产出解压
/// 字节，外层 entry/tail 循环等不到检查点；每次压缩源读前先看 token，超限与取消
/// 都以专属标记错误上抛，穿透 read_exact 的 Interrupted 自动重试。
struct HashingReader<'a, R> {
    inner: R,
    hasher: &'a mut Sha256,
    counted: &'a mut u64,
    limit: Option<u64>,
    cancellation: &'a CancellationToken,
}
impl<'a, R: Read> HashingReader<'a, R> {
    /// 借用调用栈上的哈希器与计数器；limit 为 None 时保持无期望值解压的无输入上限行为。
    fn new(
        inner: R,
        hasher: &'a mut Sha256,
        counted: &'a mut u64,
        limit: Option<u64>,
        cancellation: &'a CancellationToken,
    ) -> Self {
        Self {
            inner,
            hasher,
            counted,
            limit,
            cancellation,
        }
    }
}
impl<R: Read> Read for HashingReader<'_, R> {
    /// 每次读到多少字节就哈希并计数多少；读取边界先检查取消，到达预算上限后再试读
    /// 一个字节，区分恰好结束和仍有内容（与 Bounded 的限额语义一致）。读取按剩余
    /// 额度截断，这是 Read 语义允许的短读，解码器内部的 BufReader 会正常处理。
    /// 已启动的单次 OS 读不可撤回，取消只能在读与读的间隙生效，这里如实保证。
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.cancellation.is_cancelled() {
            return Err(io::Error::other(CancelledRead));
        }
        if buffer.is_empty() {
            return Ok(0);
        }
        if let Some(limit) = self.limit {
            if *self.counted >= limit {
                let mut byte = [0u8; 1];
                let extra = self.inner.read(&mut byte)?;
                if extra == 0 {
                    // 恰好在预算处结束是合法的；EOF 前不越过预算，交由摘要比对收口。
                    return Ok(0);
                }
                *self.counted = self.counted.saturating_add(1);
                self.hasher.update(byte);
                return Err(io::Error::other(InputBudgetExceeded {
                    counted: *self.counted,
                }));
            }
            let allowed = (limit - *self.counted).min(buffer.len() as u64) as usize;
            let read = self.inner.read(&mut buffer[..allowed])?;
            self.hasher.update(&buffer[..read]);
            *self.counted = self.counted.saturating_add(read as u64);
            return Ok(read);
        }
        let read = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..read]);
        *self.counted = self.counted.saturating_add(read as u64);
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

/// 带流式 size/SHA-256 验证的解压入口，行为与无期望值的解压路径一致，仅在全部尾检通过后比对摘要。
///
/// 摘要与大小按实际进入 gzip 解码器的压缩字节流计算，读取与解压共用同一次 `File::open`，
/// 调用方因此不再需要“先按路径验摘要、后重新打开解包”的两段式流程，消除了两次打开之间
/// 暂存文件被原位替换的窗口；仅当完整解压、gzip CRC、tar 结束块全部通过后才比较期望值，
/// 比对失败必须返回错误。安装编排只在成功返回后继续校验与提交，失败时目的目录可能已
/// 写入部分成员，由调用方随暂存目录整体清理。
pub(super) fn unpack_verified(
    archive_path: &Path,
    destination: &Path,
    cancellation: &CancellationToken,
    expected: &ArchiveExpectation,
) -> AppResult<()> {
    let file = fs::File::open(archive_path)
        .map_err(|e| AppError::infrastructure_with_source("读取插件安装包失败", e))?;
    unpack_stream(file, destination, cancellation, Some(expected))
}

/// 把解压链路上的 io 错误按标记类型精确还原：取消与预算超限各归各类，其余保持
/// 原样作为基础设施错误；绝不用取消伪装普通 I/O 故障，也不反过来。
///
/// 入参必须 owned：`infrastructure_with_source` 要求 `E: Error + Send + Sync +
/// 'static`，借用局部引用无法满足；分类过程只借用，未命中标记时整体移交。
/// 标记由 `io::Error::new` 包装，标准库的 `source()` 只返回内层错误自身的来源而
/// 不返回内层错误本身，因此每层都要查 `get_ref()` 的载荷再沿 `source()` 下钻；
/// flate2 原样上抛底层错误，tar 可能包进 TarError 并挂 source 链。两类标记使用
/// ErrorKind::Other 加专属类型，不会被 read_exact 按 Interrupted 吞掉重试。
fn classify_stream_error(context: &'static str, error: io::Error) -> AppError {
    let mut current: Option<&dyn std::error::Error> = Some(&error);
    while let Some(item) = current {
        // io::Error 的标记藏在 get_ref() 载荷里；其他层（TarError 等）按普通
        // dyn Error 处理，直接尝试 downcast 自身；downcast 要求 'static，
        // 错误链上的项均来自 'static 的 io::Error，满足边界。
        if let Some(io_error) = item.downcast_ref::<io::Error>() {
            if let Some(payload) = io_error.get_ref() {
                if payload.is::<CancelledRead>() {
                    return AppError::cancelled("插件安装已取消");
                }
                if payload.is::<InputBudgetExceeded>() {
                    return AppError::invalid("插件归档压缩输入超过限额");
                }
            }
        } else if item.downcast_ref::<CancelledRead>().is_some() {
            return AppError::cancelled("插件安装已取消");
        } else if item.downcast_ref::<InputBudgetExceeded>().is_some() {
            return AppError::invalid("插件归档压缩输入超过限额");
        }
        current = item.source();
    }
    // 未命中标记的普通错误保留完整 source 链上抛，不静默丢弃诊断信息。
    AppError::infrastructure_with_source(context, error)
}

/// 归档解压的共享核心；期望值为 `None` 时保持无期望值的解压行为，`Some` 时启用流式验证。
///
/// `File` 只打开一次，先经 `HashingReader` 再喂给 `flate2::read::MultiGzDecoder`：
/// 哈希层在解码器之下，读到的正是传给解码器的压缩字节，包括解码器内部 BufReader
/// 预读的部分。解压阶段消费完 gzip 成员后，tar 尾检阶段会从同一句柄继续读到真实
/// EOF（预读同样经过了哈希层），因此文件的全部字节必然流经哈希器；gzip
/// concatenation 由 MultiGzDecoder 展开，各成员 CRC 全部核对后才比对期望值。
/// 泛型源仅供测试注入受控 reader，生产入口只传 `File`。
fn unpack_stream<R: Read>(
    file: R,
    destination: &Path,
    cancellation: &CancellationToken,
    expected: Option<&ArchiveExpectation>,
) -> AppResult<()> {
    let mut hasher = Sha256::new();
    let mut counted = 0u64;
    // 哈希层必须贴着压缩源：MultiGzDecoder 对外吐解压后字节，包在外面会算错对象。
    // 验证路径把可信期望大小+1 作为压缩输入上限传入，输入超限即时失败，不等 EOF；
    // 无期望值路径保持压缩输入不限、仅解压端 MAX_STREAM_BYTES 限流。
    let source = HashingReader::new(
        file,
        &mut hasher,
        &mut counted,
        expected.map(|want| want.size.saturating_add(1)),
        cancellation,
    );
    let reader = Bounded {
        inner: MultiGzDecoder::new(source),
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
        .map_err(|e| classify_stream_error("读取插件归档失败", e))?
        .raw(true)
    {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件安装已取消"));
        }
        let mut entry = entry.map_err(|e| classify_stream_error("读取插件归档条目失败", e))?;
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
                .map_err(|e| classify_stream_error("解压插件文件失败", e))?;
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
    // 每轮读取前检查取消：空 tar 或极短归档会直接落在这里，取消不能被吞掉；
    // 单次 OS 读本身不可中断，只能在读与读的间隙响应取消，不伪称已在读中回收。
    let mut reader = archive.into_inner();
    // tar 迭代在遇到第一个全零块时返回 None，该块已被迭代消费；严格结束块
    // 要求其后还有至少一个完整零块。记录迭代结束时的已解压字节数，用于区分
    // “迭代消费的零块”与“尾部的真实结束块”，避免把末段全零数据误当结束块。
    let consumed_after_entries = reader.consumed;
    let mut buffer = [0u8; 65536];
    loop {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("插件安装已取消"));
        }
        let read = reader
            .read(&mut buffer)
            .map_err(|e| classify_stream_error("验证插件归档尾部失败", e))?;
        if read == 0 {
            break;
        }
        if buffer[..read].iter().any(|b| *b != 0) {
            return Err(AppError::invalid("插件归档尾部含未声明内容"));
        }
    }
    // 迭代结束到 EOF 之间必须至少还有一个完整的 512 字节零块：块对齐但缺少
    // 结束块的归档会在迭代后直接 EOF（尾部零字节），末尾数据为零也不足以证明
    // 格式，因此仅凭“最后 1024 字节为零”会误接受。迭代消费一个零块 + 尾部
    // 至少一个零块，才等价于原约定的 1024 字节结束块。
    let tail_bytes = reader.consumed.saturating_sub(consumed_after_entries);
    if tail_bytes < 512
        || reader.consumed < 1024
        || reader.tail.len() != 1024
        || reader.tail.iter().any(|byte| *byte != 0)
    {
        return Err(AppError::invalid("插件归档结束块缺失"));
    }
    // EOF 与最终成功前的复核：解压/尾读过程中令牌可能才被触发，此时即便数据完整
    // 也必须按取消收口，防止“取消请求发出后仍返回成功”的竞态。
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("插件安装已取消"));
    }
    // 尾检全部通过后才比较可信期望值；比较失败必须报错，不能因为已解压而放行。
    if let Some(expected) = expected
        && (counted != expected.size
            || !format!("{:x}", hasher.finalize()).eq_ignore_ascii_case(&expected.sha256))
    {
        return Err(AppError::invalid("插件归档摘要或大小不匹配"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../../tests/unit/plugin_install_archive.rs"]
mod tests;
