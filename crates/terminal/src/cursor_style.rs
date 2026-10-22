//! 追踪 DECSCUSR 的来源状态，让 UI 用户默认形状不覆盖终端显式设置。
//!
//! @author kongweiguang

const MAX_CSI_PARAMETERS: usize = 64;
const MAX_CSI_INTERMEDIATES: usize = 8;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ScanState {
    #[default]
    Ground,
    Escape,
    Csi,
    CsiIntermediate,
    Osc,
    OscEscape,
    ControlString,
    ControlStringEscape,
}

/// 有界扫描 VT 控制序列，只标记有效 DECSCUSR，避免文本与 OSC 内容误设 UI 优先级。
///
/// @author kongweiguang
#[derive(Default)]
pub(crate) struct CursorStyleTracker {
    state: ScanState,
    parameters: Vec<u8>,
    intermediates: Vec<u8>,
    overflowed: bool,
    terminal_styled: bool,
}

impl CursorStyleTracker {
    /// 按原始字节顺序跨 PTY 分块识别 DECSCUSR、RIS 和 DECSTR；序列缓冲固定有界。
    ///
    /// @author kongweiguang
    pub(crate) fn advance(&mut self, bytes: &[u8]) {
        for byte in bytes.iter().copied() {
            match self.state {
                ScanState::Ground => {
                    if byte == 0x1b {
                        self.state = ScanState::Escape;
                    }
                }
                ScanState::Escape => match byte {
                    b'[' => self.begin_csi(),
                    b']' => self.state = ScanState::Osc,
                    b'P' | b'X' | b'^' | b'_' => self.state = ScanState::ControlString,
                    b'c' => {
                        self.terminal_styled = false;
                        self.state = ScanState::Ground;
                    }
                    0x1b => self.state = ScanState::Escape,
                    _ => self.state = ScanState::Ground,
                },
                ScanState::Csi => self.advance_csi(byte, false),
                ScanState::CsiIntermediate => self.advance_csi(byte, true),
                ScanState::Osc => match byte {
                    0x07 => self.state = ScanState::Ground,
                    0x1b => self.state = ScanState::OscEscape,
                    _ => {}
                },
                ScanState::OscEscape => match byte {
                    b'\\' => self.state = ScanState::Ground,
                    0x1b => self.state = ScanState::OscEscape,
                    _ => self.state = ScanState::Osc,
                },
                ScanState::ControlString => {
                    if byte == 0x1b {
                        self.state = ScanState::ControlStringEscape;
                    }
                }
                ScanState::ControlStringEscape => match byte {
                    b'\\' => self.state = ScanState::Ground,
                    0x1b => self.state = ScanState::ControlStringEscape,
                    _ => self.state = ScanState::ControlString,
                },
            }
        }
    }

    /// 暴露终端最近一次 reset 后是否显式设置了 cursor shape。
    ///
    /// @author kongweiguang
    pub(crate) const fn is_terminal_styled(&self) -> bool {
        self.terminal_styled
    }

    /// 初始化一个全新 CSI 候选并清空上次有界参数缓存。
    ///
    /// @author kongweiguang
    fn begin_csi(&mut self) {
        self.parameters.clear();
        self.intermediates.clear();
        self.overflowed = false;
        self.state = ScanState::Csi;
    }

    /// 解析 CSI 参数/中间字节直到 final；仅接受明确的 DECSCUSR/DECSTR 形式。
    ///
    /// @author kongweiguang
    fn advance_csi(&mut self, byte: u8, in_intermediate: bool) {
        match byte {
            0x18 | 0x1a => self.state = ScanState::Ground,
            0x1b => self.state = ScanState::Escape,
            0x20..=0x2f if in_intermediate => {
                if self.intermediates.len() >= MAX_CSI_INTERMEDIATES {
                    self.overflowed = true;
                } else {
                    self.intermediates.push(byte);
                }
            }
            0x20..=0x2f => {
                if self.intermediates.len() >= MAX_CSI_INTERMEDIATES {
                    self.overflowed = true;
                } else {
                    self.intermediates.push(byte);
                }
                self.state = ScanState::CsiIntermediate;
            }
            0x30..=0x3f if !in_intermediate => {
                if self.parameters.len() >= MAX_CSI_PARAMETERS {
                    self.overflowed = true;
                } else {
                    self.parameters.push(byte);
                }
            }
            0x40..=0x7e => {
                self.finish_csi(byte);
                self.state = ScanState::Ground;
            }
            _ => self.state = ScanState::Ground,
        }
    }

    /// 只有单个合法光标参数会切换来源位，RIS/DECSTR 则恢复用户默认优先级。
    ///
    /// @author kongweiguang
    fn finish_csi(&mut self, final_byte: u8) {
        if self.overflowed {
            return;
        }

        if final_byte == b'q'
            && self.intermediates.as_slice() == b" "
            && (self.parameters.is_empty()
                || (self.parameters.len() == 1 && (b'0'..=b'6').contains(&self.parameters[0])))
        {
            self.terminal_styled = true;
        } else if final_byte == b'p'
            && self.intermediates.as_slice() == b"!"
            && self.parameters.is_empty()
        {
            self.terminal_styled = false;
        }
    }
}
