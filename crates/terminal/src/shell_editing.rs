//! 在会话输出分流前剥离专用编辑快照帧，防止正文进入日志与终端网格。
//!
//! @author kongweiguang

/// 先按命名空间隔离，再由上层检查版本，未知版本也不能进入日志或终端网格。
pub const SHELL_EDITING_FRAME_PREFIX: &[u8] = b"\x1b]6977;Kerminal;";

/// 输入、目录和编码开销都受限；超限帧保持丢弃直到结束符，不能泄露其后半段。
pub const MAX_SHELL_EDITING_FRAME_BYTES: usize = 128 * 1024;
/// 快照与追加后的输入共享 Unicode 标量上限，避免按 UTF-8 字节误拒绝多字节字符。
pub const MAX_SHELL_EDITING_INPUT_CHARACTERS: usize = 4_000;
const MAX_FRAMES_PER_CHUNK: usize = 32;

/// 只在当前 reader 内保存有界的未完成帧；每次重连都必须新建过滤器。
#[derive(Default)]
pub struct ShellEditingFrameFilter {
    prefix_len: usize,
    in_frame: bool,
    pending_escape: bool,
    overflowed: bool,
    payload: Vec<u8>,
}

/// 专用帧可能包含尚未提交的敏感文本，因此整个值刻意不实现 Debug。
pub struct FilteredShellEditingOutput {
    pub visible: Vec<u8>,
    pub frames: Vec<ShellEditingFrame>,
    /// 一旦超限由 reader 明确使能力不可用，不能将未结束帧的剩余字节当作普通输出。
    pub overflowed: bool,
}

/// 帧的位置以剥离后的可见字节数表示，确保 prompt 标记与快照按原始 PTY 顺序处理。
pub struct ShellEditingFrame {
    pub visible_offset: usize,
    pub payload: Vec<u8>,
}

/// 精确 Shell 种类由 bootstrap 声明，不把通用 POSIX 方言解释成某个具体实现。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditingShell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
}

/// 只接受本版本的有界消息；尚未提交的正文和目录不得通过 Debug 暴露。
pub enum EditingMessage {
    Hello {
        nonce: String,
        shell: EditingShell,
        version: String,
        installed: bool,
    },
    Snapshot {
        nonce: String,
        shell: EditingShell,
        version: String,
        cwd: Option<String>,
        input: String,
        cursor: usize,
        prediction_visible: Option<bool>,
    },
    InputTooLong {
        nonce: String,
        shell: EditingShell,
        version: String,
    },
}

/// 解析经过私有过滤器剥离的帧；任意编码错误都直接拒绝，不回显原始正文。
///
/// 格式为 `1;H;nonce;shell;installed;version` 或
/// `1;S;nonce;shell;unit;cursor;version;cwd;input;prediction`，或无正文超限响应
/// `1;R;nonce;shell;input_too_long;version`。
/// 文本字段以百分号编码的 UTF-8 字节传输；`-` 表示目录未知。
///
/// @author kongweiguang
pub fn parse_editing_message(payload: &[u8]) -> Option<EditingMessage> {
    let text = std::str::from_utf8(payload).ok()?;
    let mut fields = text.split(';');
    if fields.next()? != "1" {
        return None;
    }
    let kind = fields.next()?;
    let nonce = fields.next()?;
    if nonce.len() != 32 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let shell = match fields.next()? {
        "bash" => EditingShell::Bash,
        "zsh" => EditingShell::Zsh,
        "fish" => EditingShell::Fish,
        "powershell" => EditingShell::PowerShell,
        _ => return None,
    };
    match kind {
        "H" => {
            let installed = match fields.next()? {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            let version = decode_field(fields.next()?, 256)?;
            if fields.next().is_some() || version.chars().any(char::is_control) {
                return None;
            }
            Some(EditingMessage::Hello {
                nonce: nonce.to_owned(),
                shell,
                version,
                installed,
            })
        }
        "R" => {
            if fields.next()? != "input_too_long" {
                return None;
            }
            let version = decode_field(fields.next()?, 256)?;
            if fields.next().is_some() || version.chars().any(char::is_control) {
                return None;
            }
            Some(EditingMessage::InputTooLong {
                nonce: nonce.to_owned(),
                shell,
                version,
            })
        }
        "S" => {
            let unit = fields.next()?;
            let position = fields.next()?.parse::<usize>().ok()?;
            let version = decode_field(fields.next()?, 256)?;
            let cwd_field = fields.next()?;
            let cwd = if cwd_field == "-" {
                None
            } else {
                let cwd = decode_field(cwd_field, 16 * 1024)?;
                if cwd.is_empty() || cwd.chars().any(char::is_control) {
                    return None;
                }
                Some(cwd)
            };
            let input = decode_field(fields.next()?, 32 * 1024)?;
            let prediction_visible = match fields.next()? {
                "?" => None,
                "0" => Some(false),
                "1" => Some(true),
                _ => return None,
            };
            if fields.next().is_some()
                || version.chars().any(char::is_control)
                || input.chars().count() > MAX_SHELL_EDITING_INPUT_CHARACTERS
            {
                return None;
            }
            let cursor = normalize_cursor(&input, unit, position)?;
            Some(EditingMessage::Snapshot {
                nonce: nonce.to_owned(),
                shell,
                version,
                cwd,
                input,
                cursor,
                prediction_visible,
            })
        }
        _ => None,
    }
}

/// 严格验证百分号编码和 UTF-8，避免不同编码或分号逃逸改变字段边界。
///
/// @author kongweiguang
fn decode_field(field: &str, max_bytes: usize) -> Option<String> {
    let mut decoded = Vec::with_capacity(field.len().min(max_bytes));
    let bytes = field.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        let byte = if bytes[offset] == b'%' {
            let high = (bytes.get(offset + 1)? as &u8).to_ascii_lowercase();
            let low = (bytes.get(offset + 2)? as &u8).to_ascii_lowercase();
            offset += 3;
            hex_digit(high)? * 16 + hex_digit(low)?
        } else {
            let byte = bytes[offset];
            if !byte.is_ascii_alphanumeric() && !b"-._~/".contains(&byte) {
                return None;
            }
            offset += 1;
            byte
        };
        if decoded.len() == max_bytes {
            return None;
        }
        decoded.push(byte);
    }
    String::from_utf8(decoded).ok()
}

/// 每个编码数字只允许 ASCII 十六进制，拒绝截断及其他字符。
///
/// @author kongweiguang
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// 只接受完整 UTF-8 字节或 UTF-16 code unit 边界，统一转换为 Unicode 标量偏移。
///
/// @author kongweiguang
fn normalize_cursor(input: &str, unit: &str, position: usize) -> Option<usize> {
    if !matches!(unit, "byte" | "utf16" | "scalar") {
        return None;
    }
    let mut consumed = 0;
    for (index, character) in input.chars().enumerate() {
        if consumed == position {
            return Some(index);
        }
        consumed += match unit {
            "byte" => character.len_utf8(),
            "utf16" => character.len_utf16(),
            "scalar" => 1,
            _ => return None,
        };
    }
    if consumed == position {
        Some(input.chars().count())
    } else {
        None
    }
}

impl ShellEditingFrameFilter {
    /// 连续消费原始 reader chunk；只延迟可能属于专用前缀的少数字节，不改变其余顺序。
    ///
    /// @author kongweiguang
    pub fn advance(&mut self, bytes: &[u8]) -> FilteredShellEditingOutput {
        let mut output = FilteredShellEditingOutput {
            visible: Vec::with_capacity(bytes.len()),
            frames: Vec::new(),
            overflowed: false,
        };
        for &byte in bytes {
            if self.in_frame {
                let was_overflowed = self.overflowed;
                self.consume_frame_byte(byte, output.visible.len(), &mut output.frames);
                output.overflowed |= !was_overflowed && self.overflowed;
            } else {
                self.consume_visible_byte(byte, &mut output.visible);
            }
        }
        output
    }

    /// 前缀不匹配时完整退还普通输出；重复 ESC 可立即成为下一次候选前缀。
    ///
    /// @author kongweiguang
    fn consume_visible_byte(&mut self, byte: u8, visible: &mut Vec<u8>) {
        if byte == SHELL_EDITING_FRAME_PREFIX[self.prefix_len] {
            self.prefix_len += 1;
            if self.prefix_len == SHELL_EDITING_FRAME_PREFIX.len() {
                self.prefix_len = 0;
                self.in_frame = true;
                self.pending_escape = false;
                self.overflowed = false;
                self.payload.clear();
            }
            return;
        }
        visible.extend_from_slice(&SHELL_EDITING_FRAME_PREFIX[..self.prefix_len]);
        if byte == SHELL_EDITING_FRAME_PREFIX[0] {
            self.prefix_len = 1;
        } else {
            self.prefix_len = 0;
            visible.push(byte);
        }
    }

    /// BEL 和 ST 均结束私有帧；不完整或非法的帧正文始终留在过滤边界内。
    ///
    /// @author kongweiguang
    fn consume_frame_byte(
        &mut self,
        byte: u8,
        visible_offset: usize,
        frames: &mut Vec<ShellEditingFrame>,
    ) {
        if self.pending_escape {
            self.pending_escape = false;
            if byte == b'\\' {
                self.finish_frame(visible_offset, frames);
                return;
            }
            self.push_payload(0x1b);
        }
        match byte {
            0x07 => self.finish_frame(visible_offset, frames),
            0x1b => self.pending_escape = true,
            byte => self.push_payload(byte),
        }
    }

    /// 超限后不继续分配内存，但仍屏蔽后续正文直到当前 OSC 真正结束。
    ///
    /// @author kongweiguang
    fn push_payload(&mut self, byte: u8) {
        if self.overflowed {
            return;
        }
        if self.payload.len() == MAX_SHELL_EDITING_FRAME_BYTES {
            self.overflowed = true;
            self.payload.clear();
            return;
        }
        self.payload.push(byte);
    }

    /// 单个 chunk 的帧数也有上限，恶意 shell 输出不能积累无限事件对象。
    ///
    /// @author kongweiguang
    fn finish_frame(&mut self, visible_offset: usize, frames: &mut Vec<ShellEditingFrame>) {
        if !self.overflowed && frames.len() < MAX_FRAMES_PER_CHUNK {
            frames.push(ShellEditingFrame {
                visible_offset,
                payload: std::mem::take(&mut self.payload),
            });
        } else {
            self.payload.clear();
        }
        self.in_frame = false;
        self.pending_escape = false;
        self.overflowed = false;
    }

    /// 在 reader 关闭时只释放尚未匹配的普通前缀；私有帧残片不得进入日志。
    ///
    /// @author kongweiguang
    pub fn finish(&mut self) -> Vec<u8> {
        let visible = if self.in_frame {
            Vec::new()
        } else {
            SHELL_EDITING_FRAME_PREFIX[..self.prefix_len].to_vec()
        };
        *self = Self::default();
        visible
    }
}
