//! ZMODEM 握手、无界面协议状态机及本地文件边界。
//!
//! @author kongweiguang

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use tempfile::NamedTempFile;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use zmodem2::{Action, Event, FileInfo, Position, Receiver, Sender};

use crate::storage::{AtomicCommitError, CommitMode, commit_staged_file};
use kerminal_application::sessions::ZmodemDirection;

/// 与 lrzsz 的 canit 相同，调用方须完整发送并等待对端退出。
pub(crate) const CANCEL_SEQUENCE: [u8; 20] = [
    0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08,
    0x08, 0x08, 0x08, 0x08,
];

/// 首帧与其后的同 read 数据分离，确保可在切换所有权后按顺序处理。
#[derive(Debug)]
pub(crate) struct Handshake {
    pub direction: ZmodemDirection,
    pub frame: Vec<u8>,
}

/// 单次检测保留普通文本和握手后的字节顺序。
#[derive(Debug, Default)]
pub(crate) struct Detection {
    pub visible: Vec<u8>,
    pub handshake: Option<Handshake>,
    pub remaining: Vec<u8>,
}

/// 只暂存可能构成 ZHEX 握手的前缀，避免普通终端输出无限滞留。
#[derive(Default)]
pub(crate) struct HandshakeDetector {
    pending: Vec<u8>,
}

impl HandshakeDetector {
    /// 逐字节识别 CRC 正确的初始帧，跨 read 分块时保持不丢字节。
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Detection {
        let mut result = Detection::default();
        for (index, &byte) in bytes.iter().enumerate() {
            self.pending.push(byte);
            loop {
                if let Some(direction) = is_initial_header(&self.pending) {
                    result.handshake = Some(Handshake {
                        direction,
                        frame: std::mem::take(&mut self.pending),
                    });
                    result.remaining.extend_from_slice(&bytes[index + 1..]);
                    return result;
                }
                if is_possible_initial_header(&self.pending) {
                    break;
                }
                result.visible.push(self.pending.remove(0));
                if self.pending.is_empty() {
                    break;
                }
            }
        }
        result
    }

    /// 会话关闭或前缀等待超时时归还缓存，避免普通输出永久消失。
    pub(crate) fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    /// 有未决前缀时由 reader 启动有限计时，避免普通星号长时间不可见。
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

/// 仅允许标准 ZHEX 初始帧前缀，缩短普通输出的延迟和误判窗口。
fn is_possible_initial_header(bytes: &[u8]) -> bool {
    const PREFIX: &[u8] = b"**\x18B0";
    if bytes.len() <= PREFIX.len() {
        return PREFIX.starts_with(bytes);
    }
    if !bytes.starts_with(PREFIX) || bytes.len() > 18 || !matches!(bytes[5], b'0' | b'1') {
        return false;
    }
    bytes[6..].iter().all(u8::is_ascii_hexdigit)
}

/// 验证完整帧及 CRC，避免仅凭终端中常见文本启动文件传输。
fn is_initial_header(bytes: &[u8]) -> Option<ZmodemDirection> {
    if bytes.len() != 18 || !is_possible_initial_header(bytes) {
        return None;
    }
    let mut decoded = [0_u8; 7];
    for (offset, pair) in bytes[4..].as_chunks::<2>().0.iter().enumerate() {
        let hi = (pair[0] as char).to_digit(16)? as u8;
        let lo = (pair[1] as char).to_digit(16)? as u8;
        decoded[offset] = (hi << 4) | lo;
    }
    let mut crc = 0_u16;
    for byte in &decoded {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    if crc != 0 {
        return None;
    }
    match decoded[0] {
        0 => Some(ZmodemDirection::Download),
        1 => Some(ZmodemDirection::Upload),
        _ => None,
    }
}

/// 仅观察已由发送端状态机消费的远端控制帧，跨读块识别合法 ZSKIP。
#[derive(Default)]
struct HeaderScanner {
    candidate: Vec<u8>,
}

impl HeaderScanner {
    /// 接收端方向只包含控制帧；CRC 确认后才纠正上游把 skip 当完成的事件。
    fn push(&mut self, bytes: &[u8]) -> bool {
        let mut skipped = false;
        for byte in bytes {
            self.candidate.push(*byte);
            loop {
                match parse_header(&self.candidate) {
                    HeaderParse::Pending => break,
                    HeaderParse::Valid(frame) => {
                        skipped |= frame == 5;
                        self.candidate.clear();
                        break;
                    }
                    HeaderParse::Invalid => {
                        self.candidate.remove(0);
                        if self.candidate.is_empty() {
                            break;
                        }
                    }
                }
            }
        }
        skipped
    }
}

enum HeaderParse {
    Pending,
    Invalid,
    Valid(u8),
}

/// 识别 ZHEX、ZBIN 和 ZBIN32，包括 ZDLE 转义；畸形和校验失败均不得产生 skip。
fn parse_header(bytes: &[u8]) -> HeaderParse {
    const HEX_START: &[u8] = b"**\x18B";
    const BIN_START: &[u8] = b"*\x18A";
    const BIN32_START: &[u8] = b"*\x18C";
    if [HEX_START, BIN_START, BIN32_START]
        .iter()
        .any(|prefix| prefix.starts_with(bytes))
    {
        return HeaderParse::Pending;
    }
    if bytes.starts_with(HEX_START) {
        if bytes.len() < 18 {
            return if bytes[4..].iter().all(u8::is_ascii_hexdigit) {
                HeaderParse::Pending
            } else {
                HeaderParse::Invalid
            };
        }
        if bytes.len() != 18 {
            return HeaderParse::Invalid;
        }
        let mut decoded = [0_u8; 7];
        for (index, pair) in bytes[4..].as_chunks::<2>().0.iter().enumerate() {
            let Some(high) = (pair[0] as char).to_digit(16) else {
                return HeaderParse::Invalid;
            };
            let Some(low) = (pair[1] as char).to_digit(16) else {
                return HeaderParse::Invalid;
            };
            decoded[index] = ((high << 4) | low) as u8;
        }
        return if crc16(&decoded) == 0 {
            HeaderParse::Valid(decoded[0])
        } else {
            HeaderParse::Invalid
        };
    }
    let is_crc32 = if bytes.starts_with(BIN_START) {
        false
    } else if bytes.starts_with(BIN32_START) {
        true
    } else {
        return HeaderParse::Invalid;
    };
    let expected = if is_crc32 { 9 } else { 7 };
    let mut decoded = Vec::with_capacity(expected);
    let mut index = 3;
    while index < bytes.len() && decoded.len() < expected {
        let byte = bytes[index];
        index += 1;
        if byte == 0x18 {
            if index == bytes.len() {
                return HeaderParse::Pending;
            }
            let escaped = bytes[index];
            index += 1;
            decoded.push(match escaped {
                b'l' => 0x7f,
                b'm' => 0xff,
                _ if escaped & 0x60 == 0x40 => escaped ^ 0x40,
                _ => return HeaderParse::Invalid,
            });
        } else {
            decoded.push(byte);
        }
    }
    if decoded.len() < expected {
        return HeaderParse::Pending;
    }
    if index != bytes.len() {
        return HeaderParse::Invalid;
    }
    let valid = if is_crc32 {
        crc32(&decoded[..5]).to_le_bytes() == decoded[5..9]
    } else {
        crc16(&decoded) == 0
    };
    if valid {
        HeaderParse::Valid(decoded[0])
    } else {
        HeaderParse::Invalid
    }
}

/// ZMODEM 的 16 位帧校验使用 CRC-16/XMODEM，初值为零。
fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0_u16;
    for byte in bytes {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// ZBIN32 使用 CRC-32/ISO-HDLC，只有完整校验正确的帧才产生控制事件。
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// 复制库内借用的短缓冲，允许调用方异步执行操作而不持有状态机借用。
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ProtocolAction {
    WriteWire(Vec<u8>),
    ReadFile { offset: u32, max_len: usize },
    WriteFile(Vec<u8>),
    FileStarted { name: Vec<u8>, size: Option<u32> },
    FileCompleted,
    FileSkipped,
    SessionCompleted,
    Aborted,
    Idle,
}

/// 协议错误保留来源；路径校验及本地提交错误不伪装成远端成功。
#[derive(Debug, Error)]
pub(crate) enum TransferError {
    #[error("ZMODEM 协议错误: {0}")]
    Protocol(#[from] zmodem2::Error),
    #[error("本地文件错误: {0}")]
    Io(#[from] io::Error),
    #[error("文件名不安全或不受支持")]
    InvalidName,
    #[error("文件超过 ZMODEM 文件大小限制")]
    FileTooLarge,
    #[error("当前协议方向不支持此操作")]
    InvalidDirection,
    #[error("本地提交结果未知: {0}")]
    CommitUnknown(io::Error),
    #[error("文件操作已取消")]
    Cancelled,
}

enum ProtocolInner {
    Upload(Sender),
    Download(Receiver),
}

/// 每个 SSH 会话只有一个协议实例，由 reader 所属任务串行推进。
pub(crate) struct ZmodemProtocol {
    inner: ProtocolInner,
    pending_wire: Option<Vec<u8>>,
    wire_offset: usize,
    incoming_headers: HeaderScanner,
    skipped_file: bool,
}

impl ZmodemProtocol {
    /// 下载使用手动接收，先完成本地路径检查再回应远端。
    pub(crate) fn new(direction: ZmodemDirection) -> Result<Self, TransferError> {
        let inner = match direction {
            ZmodemDirection::Upload => ProtocolInner::Upload(Sender::new()?),
            ZmodemDirection::Download => {
                let mut receiver = Receiver::new()?;
                receiver.set_manual_file_accept(true);
                ProtocolInner::Download(receiver)
            }
        };
        Ok(Self {
            inner,
            pending_wire: None,
            wire_offset: 0,
            incoming_headers: HeaderScanner::default(),
            skipped_file: false,
        })
    }

    /// 返回实际消费长度，剩余输入必须在后续 poll 后重试或交回终端。
    pub(crate) fn feed_wire(&mut self, bytes: &[u8]) -> Result<usize, TransferError> {
        let consumed = match &mut self.inner {
            ProtocolInner::Upload(sender) => sender.submit_wire(bytes)?,
            ProtocolInner::Download(receiver) => receiver.submit_wire(bytes)?,
        };
        if matches!(self.inner, ProtocolInner::Upload(_))
            && self.incoming_headers.push(&bytes[..consumed])
        {
            self.skipped_file = true;
        }
        Ok(consumed)
    }

    /// 先处理输出动作，再交由调用方读取更多输入，尊重协议背压。
    pub(crate) fn poll(&mut self) -> ProtocolAction {
        if let Some(pending) = &self.pending_wire {
            return ProtocolAction::WriteWire(pending[self.wire_offset..].to_vec());
        }
        let download = matches!(self.inner, ProtocolInner::Download(_));
        let action = match &mut self.inner {
            ProtocolInner::Upload(sender) => sender.poll(),
            ProtocolInner::Download(receiver) => receiver.poll(),
        };
        match action {
            Action::WriteWire(bytes) => {
                let mut wire = bytes.to_vec();
                if download {
                    enable_escaped_control_characters(&mut wire);
                }
                self.pending_wire = Some(wire.clone());
                self.wire_offset = 0;
                ProtocolAction::WriteWire(wire)
            }
            Action::ReadFile { offset, max_len } => ProtocolAction::ReadFile {
                offset: offset.get(),
                max_len,
            },
            Action::WriteFile(bytes) => ProtocolAction::WriteFile(bytes.to_vec()),
            Action::Event(Event::FileStarted(info)) => ProtocolAction::FileStarted {
                name: info.name.to_vec(),
                size: info.size.map(Position::get),
            },
            Action::Event(Event::FileCompleted) => {
                if std::mem::take(&mut self.skipped_file) {
                    ProtocolAction::FileSkipped
                } else {
                    ProtocolAction::FileCompleted
                }
            }
            Action::Event(Event::SessionCompleted) => ProtocolAction::SessionCompleted,
            Action::Event(Event::Aborted) => ProtocolAction::Aborted,
            _ => ProtocolAction::Idle,
        }
    }

    /// 只确认真正写入传输层的字节，部分写入由下一次 poll 补齐。
    pub(crate) fn wire_written(&mut self, count: usize) {
        let accepted = self
            .pending_wire
            .as_ref()
            .map_or(0, |pending| count.min(pending.len() - self.wire_offset));
        match &mut self.inner {
            ProtocolInner::Upload(sender) => sender.wire_written(accepted),
            ProtocolInner::Download(receiver) => receiver.wire_written(accepted),
        }
        self.wire_offset += accepted;
        if self
            .pending_wire
            .as_ref()
            .is_some_and(|pending| self.wire_offset == pending.len())
        {
            self.pending_wire = None;
            self.wire_offset = 0;
        }
    }

    /// 上传时只使用经本地文件检查过的名称和长度。
    pub(crate) fn start_file(&mut self, name: &[u8], size: u32) -> Result<(), TransferError> {
        match &mut self.inner {
            ProtocolInner::Upload(sender) => {
                Ok(sender.start_file(FileInfo::new(name, Some(Position::new(size))))?)
            }
            _ => Err(TransferError::InvalidDirection),
        }
    }

    /// 通知发送端当前批次结束，由协议完成关闭握手。
    pub(crate) fn finish(&mut self) -> Result<(), TransferError> {
        match &mut self.inner {
            ProtocolInner::Upload(sender) => Ok(sender.finish()?),
            _ => Err(TransferError::InvalidDirection),
        }
    }

    /// 把指定偏移的文件块交回状态机，避免重复缓存整文件。
    pub(crate) fn submit_file(&mut self, data: &[u8]) -> Result<(), TransferError> {
        match &mut self.inner {
            ProtocolInner::Upload(sender) => Ok(sender.submit_file(data)?),
            _ => Err(TransferError::InvalidDirection),
        }
    }

    /// 本地暂存成功后才确认数据，防止磁盘失败被协议当成完成。
    pub(crate) fn file_written(&mut self, count: usize) -> Result<(), TransferError> {
        match &mut self.inner {
            ProtocolInner::Download(receiver) => Ok(receiver.file_written(count)?),
            _ => Err(TransferError::InvalidDirection),
        }
    }

    /// 从零开始接收，首版不恢复不可信的部分文件。
    pub(crate) fn accept_file(&mut self) -> Result<(), TransferError> {
        match &mut self.inner {
            ProtocolInner::Download(receiver) => Ok(receiver.accept_file_at(0)?),
            _ => Err(TransferError::InvalidDirection),
        }
    }

    /// 将超时交给协议状态机处理，调用方负责最终的有界等待。
    pub(crate) fn timeout(&mut self) -> Result<(), TransferError> {
        match &mut self.inner {
            ProtocolInner::Upload(sender) => Ok(sender.timeout()?),
            ProtocolInner::Download(receiver) => Ok(receiver.timeout()?),
        }
    }
}

/// 上游没有 ESCCTL 配置；仅修改 CRC 正确的 ZRINIT 帧并重算校验，要求对端转义控制字符。
fn enable_escaped_control_characters(wire: &mut [u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    if wire.len() < 18 {
        return;
    }
    for start in 0..=wire.len() - 18 {
        let frame = &mut wire[start..start + 18];
        if is_initial_header(frame) != Some(ZmodemDirection::Upload) {
            continue;
        }
        let mut decoded = [0_u8; 5];
        for (index, pair) in frame[4..14].as_chunks::<2>().0.iter().enumerate() {
            let Some(high) = (pair[0] as char).to_digit(16) else {
                return;
            };
            let Some(low) = (pair[1] as char).to_digit(16) else {
                return;
            };
            decoded[index] = ((high << 4) | low) as u8;
        }
        decoded[4] |= 0x40;
        let mut crc = 0_u16;
        for byte in decoded {
            crc ^= u16::from(byte) << 8;
            for _ in 0..8 {
                crc = if crc & 0x8000 != 0 {
                    (crc << 1) ^ 0x1021
                } else {
                    crc << 1
                };
            }
        }
        let replacement = [decoded[4], (crc >> 8) as u8, crc as u8];
        for (index, byte) in replacement.into_iter().enumerate() {
            frame[12 + index * 2] = HEX[usize::from(byte >> 4)];
            frame[13 + index * 2] = HEX[usize::from(byte & 0x0f)];
        }
    }
}

/// 上传始终绑定已打开的普通文件，避免每块重新解析用户路径。
pub(crate) struct UploadFile {
    file: File,
    pub name: Vec<u8>,
    pub size: u32,
}

impl UploadFile {
    /// 拒绝目录和超限文件，避免协议偏移被截断。
    pub(crate) fn open(path: &Path) -> Result<Self, TransferError> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(TransferError::InvalidName);
        }
        let size = u32::try_from(metadata.len()).map_err(|_| TransferError::FileTooLarge)?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(TransferError::InvalidName)?;
        validate_name(name)?;
        Ok(Self {
            file,
            name: name.as_bytes().to_vec(),
            size,
        })
    }

    /// 响应协议指定偏移并限制读取量，避免大文件整体进入内存。
    pub(crate) fn read(&mut self, offset: u32, max_len: usize) -> Result<Vec<u8>, TransferError> {
        self.file.seek(SeekFrom::Start(u64::from(offset)))?;
        let mut bytes = vec![0; max_len.min(1024 * 1024)];
        let count = self.file.read(&mut bytes)?;
        bytes.truncate(count);
        Ok(bytes)
    }
}

/// 下载暂存在目标目录，drop 自动清理未完成的文件。
pub(crate) struct DownloadFile {
    temp: NamedTempFile,
    directory: PathBuf,
    name: String,
}

impl DownloadFile {
    /// 只允许单层 UTF-8 文件名，目标目录由用户明确选择。
    pub(crate) fn create(directory: &Path, name: &[u8]) -> Result<Self, TransferError> {
        let name = std::str::from_utf8(name).map_err(|_| TransferError::InvalidName)?;
        validate_name(name)?;
        if !fs::metadata(directory)?.is_dir() {
            return Err(TransferError::InvalidName);
        }
        let temp = NamedTempFile::new_in(directory)?;
        Ok(Self {
            temp,
            directory: directory.to_path_buf(),
            name: name.to_owned(),
        })
    }

    /// 写入临时文件后由调用方确认协议数据，不直接覆盖目标文件。
    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<(), TransferError> {
        self.temp.write_all(bytes)?;
        Ok(())
    }

    /// 原子无覆盖提交；并发同名创建时尝试下一个名称。
    pub(crate) fn commit(
        mut self,
        cancellation: &CancellationToken,
    ) -> Result<PathBuf, TransferError> {
        self.temp.flush()?;
        self.temp.as_file().sync_all()?;
        for index in 0..10_000 {
            if cancellation.is_cancelled() {
                return Err(TransferError::Cancelled);
            }
            let candidate = if index == 0 {
                self.name.clone()
            } else {
                numbered_name(&self.name, index)
            };
            let destination = self.directory.join(candidate);
            match commit_staged_file(
                self.temp.path(),
                &destination,
                CommitMode::MoveStagedNoReplace,
            ) {
                Ok(()) => return Ok(destination),
                Err(AtomicCommitError::NotCommitted(error))
                    if error.kind() == io::ErrorKind::AlreadyExists =>
                {
                    continue;
                }
                Err(AtomicCommitError::NotCommitted(error)) => return Err(error.into()),
                Err(AtomicCommitError::CommitUnknown(error)) => {
                    return Err(TransferError::CommitUnknown(error));
                }
            }
        }
        Err(io::Error::new(io::ErrorKind::AlreadyExists, "同名文件过多").into())
    }
}

/// 跨平台拒绝路径、设备名和 Windows 规范化会改变含义的名称。
fn validate_name(name: &str) -> Result<(), TransferError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.ends_with(['.', ' '])
        || name.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
        })
        || !matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        )
    {
        return Err(TransferError::InvalidName);
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        return Err(TransferError::InvalidName);
    }
    Ok(())
}

/// 在扩展名前追加序号，便于用户识别自动保留的副本。
fn numbered_name(name: &str, index: usize) -> String {
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => format!("{stem} ({index}).{extension}"),
        _ => format!("{name} ({index})"),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/zmodem_protocol.rs"]
mod tests;
