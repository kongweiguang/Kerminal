//! 识别 DEC 私有光标可见性（`?25h/l` 与 RIS）在每段输入前后的状态。
//!
//! ConPTY 在普通输入后先发送 `ESC[?25l`、再发送重绘和 `ESC[?25h`，两次读取只相隔十几毫秒。
//! 合批只关心片段结尾是否可见，以及是否留下新的隐藏周期。跟踪真实切换可以区分同一
//! 片段内的 `show→hide` 和重复隐藏，避免在持续隐藏的输出上重复等待。
//!
//! @author kongweiguang

use vte::{Params, Perform};

/// 一段输入处理后的可见性结果。
///
/// `newly_hidden` 只由真实的可见→隐藏转换产生，并保证片段结尾仍隐藏；重复的 `?25l`
/// 不会触发它。因此隐藏超时释放后，同一片段里的 `show→hide` 仍会被识别为新周期，
/// 而单纯重复隐藏不会重新开始等待。`after_visible` 用于判断等待中的隐藏是否已被
/// show 或 RIS 闭合，从而立即放行。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorVisibilityChange {
    pub after_visible: bool,
    pub newly_hidden: bool,
}

/// 跨 chunk 保持 vte 状态机与可见性状态；分包位置不影响结果。
///
/// 使用真实 CSI 解析而不是字节搜索，OSC/DCS/APC 字符串中的同样文本不会被误判；
/// 未完成的转义序列会保留到下一次 `advance`。
pub struct CursorVisibilityTracker {
    parser: vte::Parser,
    visible: bool,
}

impl Default for CursorVisibilityTracker {
    /// 终端默认显示光标，首个 `?25l` 才会形成一次真实隐藏。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            parser: vte::Parser::new(),
            visible: true,
        }
    }
}

impl CursorVisibilityTracker {
    /// 消费连续字节并返回本片段最后的可见性和是否留下新的隐藏周期。
    ///
    /// @author kongweiguang
    pub fn advance(&mut self, bytes: &[u8]) -> CursorVisibilityChange {
        let Self { parser, visible } = self;
        let mut performer = VisibilityPerformer {
            visible,
            hid: false,
        };
        parser.advance(&mut performer, bytes);
        CursorVisibilityChange {
            after_visible: *performer.visible,
            newly_hidden: performer.hid && !*performer.visible,
        }
    }
}

/// 只关心 CSI 私有模式 25 与 RIS 的 vte 回调；其余控制序列交给解析器正常跳过。
struct VisibilityPerformer<'a> {
    visible: &'a mut bool,
    hid: bool,
}

impl Perform for VisibilityPerformer<'_> {
    /// `?25` 可能和其他私有模式写在同一个 CSI 中；与 vte ansi 的 DECSET/DECRST
    /// 处理一致，逐参数识别 25，因此 `?25;1049h` 也会正常关闭一次隐藏。
    ///
    /// @author kongweiguang
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore
            || intermediates.len() != 1
            || intermediates[0] != b'?'
            || !matches!(action, 'h' | 'l')
            || !params.iter().any(|param| param.first() == Some(&25))
        {
            return;
        }
        self.set_visible(action == 'h');
    }

    /// ESC c 是 RIS；它把终端恢复默认状态，其中光标可见。
    ///
    /// 隐藏期间的 RIS 必须被识别为恢复，否则已经关闭的等待无法结束。
    ///
    /// @author kongweiguang
    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if !ignore && intermediates.is_empty() && byte == b'c' {
            self.set_visible(true);
        }
    }
}

impl VisibilityPerformer<'_> {
    /// 记录真实转换；只有隐藏方向算作新周期，重复序列保持幂等。
    ///
    /// @author kongweiguang
    fn set_visible(&mut self, visible: bool) {
        if *self.visible == visible {
            return;
        }
        if !visible {
            self.hid = true;
        }
        *self.visible = visible;
    }
}
