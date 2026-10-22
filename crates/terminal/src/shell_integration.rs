//! Shell Integration 的有界 OSC 133/7 解析；只接受同一 PTY 实际输出的标记与目录。
//!
//! @author kongweiguang

const MAX_OSC_BYTES: usize = 16 * 1024;
const MAX_SHELL_MARKER_BYTES: usize = 256;

/// OSC 133 可证明 shell prompt 与命令生命周期边界的标准标记。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellIntegrationMarker {
    PromptStart,
    PromptEnd,
    CommandStart,
    CommandFinished { exit_code: Option<i32> },
}

/// 一个完整 OSC 133 标记在当前 read chunk 中结束的位置，`end_offset` 为排他字节索引。
///
/// 当序列跨块时，offset 仍准确指向终止符所在 chunk，供 capture 在终止后切分字节流。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ScannedShellMarker {
    pub marker: ShellIntegrationMarker,
    pub end_offset: usize,
}

/// 跨 PTY 读取块保留的有限状态机，避免 chunk 边界切断 OSC 或复制无限 payload。
///
/// @author kongweiguang
#[derive(Default)]
pub(crate) struct Osc133Scanner {
    in_osc: bool,
    pending_start_escape: bool,
    pending_escape: bool,
    overflowed: bool,
    payload: Vec<u8>,
    /// 仅暂存本次扫描最后一个有效目录，取走后不形成第二份持久目录状态。
    current_working_directory: Option<String>,
}

impl Osc133Scanner {
    /// 共用一次跨块扫描处理 OSC133 和 OSC7，保留命令捕获原有的精确 marker offset。
    ///
    /// @author kongweiguang
    pub(crate) fn advance_with_offsets(&mut self, bytes: &[u8]) -> Vec<ScannedShellMarker> {
        let mut markers = Vec::new();
        for (index, byte) in bytes.iter().enumerate() {
            if !self.in_osc {
                if self.pending_start_escape {
                    if *byte == b']' {
                        self.start_osc();
                        self.pending_start_escape = false;
                        continue;
                    }
                    self.pending_start_escape = *byte == 0x1b;
                } else if *byte == 0x1b {
                    self.pending_start_escape = true;
                }
                continue;
            }

            if self.pending_escape {
                if *byte == b'\\' {
                    self.finish_osc(&mut markers, index + 1);
                    continue;
                }
                self.push_payload(0x1b);
                self.pending_escape = false;
            }
            match *byte {
                0x07 => self.finish_osc(&mut markers, index + 1),
                0x1b => self.pending_escape = true,
                value => self.push_payload(value),
            }
        }
        markers
    }

    /// 将合法目录作为一次性解析事件交给 engine，scanner 本身不持有会话目录真值。
    ///
    /// @author kongweiguang
    pub(crate) fn take_current_working_directory(&mut self) -> Option<String> {
        self.current_working_directory.take()
    }

    /// 在发现完整 ESC ] 序列后开始一个有最大 payload 的 OSC 候选。
    ///
    /// @author kongweiguang
    fn start_osc(&mut self) {
        self.in_osc = true;
        self.pending_start_escape = false;
        self.pending_escape = false;
        self.overflowed = false;
        self.payload.clear();
    }

    /// 限制 untrusted OSC payload 的内存占用，超限后丢弃到本序列终止符。
    ///
    /// @author kongweiguang
    fn push_payload(&mut self, byte: u8) {
        if self.payload.len() == MAX_OSC_BYTES {
            self.overflowed = true;
            return;
        }
        if !self.overflowed {
            self.payload.push(byte);
        }
    }

    /// OSC7 只更新待消费的目录事件；它不进入 OSC133 捕获状态机，也不改变命令完成语义。
    ///
    /// @author kongweiguang
    fn finish_osc(&mut self, markers: &mut Vec<ScannedShellMarker>, end_offset: usize) {
        if !self.overflowed {
            if let Some(marker) = parse_marker(&self.payload) {
                markers.push(ScannedShellMarker { marker, end_offset });
            } else if let Some(directory) = parse_working_directory(&self.payload) {
                self.current_working_directory = Some(directory);
            }
        }
        self.in_osc = false;
        self.pending_escape = false;
        self.overflowed = false;
        self.payload.clear();
    }
}

/// 从 OSC payload 解析 OSC 133 子类型和可选退出码，忽略其它 OSC 与扩展字段。
///
/// @author kongweiguang
fn parse_marker(payload: &[u8]) -> Option<ShellIntegrationMarker> {
    if payload.len() > MAX_SHELL_MARKER_BYTES {
        return None;
    }
    let suffix = payload.strip_prefix(b"133;")?;
    let mut fields = suffix.split(|byte| *byte == b';');
    match fields.next()? {
        b"A" => Some(ShellIntegrationMarker::PromptStart),
        b"B" => Some(ShellIntegrationMarker::PromptEnd),
        b"C" => Some(ShellIntegrationMarker::CommandStart),
        b"D" => {
            let exit_code = fields.next().and_then(parse_exit_code);
            Some(ShellIntegrationMarker::CommandFinished { exit_code })
        }
        _ => None,
    }
}

/// 只接收 file URI 的绝对路径；authority 不参与会话路由，非法编码不覆盖上一合法目录。
///
/// @author kongweiguang
fn parse_working_directory(payload: &[u8]) -> Option<String> {
    let uri = payload.strip_prefix(b"7;file://")?;
    let path_start = uri.iter().position(|byte| *byte == b'/')?;
    let authority = std::str::from_utf8(&uri[..path_start]).ok()?;
    if !valid_file_authority(authority) {
        return None;
    }
    let encoded = &uri[path_start..];
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        let byte = encoded[index];
        if byte == b'%' {
            let high = hex_digit(*encoded.get(index + 1)?)?;
            let low = hex_digit(*encoded.get(index + 2)?)?;
            decoded.push(high * 16 + low);
            index += 3;
        } else if byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&byte) {
            decoded.push(byte);
            index += 1;
        } else {
            return None;
        }
    }
    let path = String::from_utf8(decoded).ok()?;
    if !path.starts_with('/') || path.chars().any(char::is_control) {
        return None;
    }
    // file:///C:/... is the standard drive URI; expose a native-compatible path without
    // treating the URI authority as another host or changing the PTY's existing target.
    let bytes = path.as_bytes();
    if bytes.len() >= 4 && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' && bytes[3] == b'/' {
        Some(path[1..].to_owned())
    } else {
        Some(path)
    }
}

/// 允许空 host、普通主机名和括号 IPv6，但拒绝端口、用户信息及 URI 查询等歧义。
///
/// @author kongweiguang
fn valid_file_authority(authority: &str) -> bool {
    if let Some(address) = authority
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
    {
        address.parse::<std::net::Ipv6Addr>().is_ok()
    } else {
        authority
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    }
}

/// 百分号编码必须完整且为 ASCII 十六进制，禁止替换字符掩盖坏 UTF8 或截断路径。
///
/// @author kongweiguang
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 只接受协议字段中无符号十进制的退出状态，拒绝溢出和注入式文本。
///
/// @author kongweiguang
fn parse_exit_code(value: &[u8]) -> Option<i32> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}
