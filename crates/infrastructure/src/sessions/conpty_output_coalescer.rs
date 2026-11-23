//! 对 Windows ConPTY、已识别的远端 PowerShell 和 Bash 探测合批分段重绘，保留私有帧在批次中的位置。
//!
//! 普通输出立即交付，只有可见光标被隐藏或 Bash 探测擦除提示符行时才等待恢复。
//!
//! @author kongweiguang

use super::editing_runtime::BashProbeGate;
use super::zmodem_protocol::{Handshake, HandshakeDetector};
use kerminal_application::sessions::{SessionTransport, TerminalResizeCapability};
use kerminal_domain::{connections::RemoteShellDialect, sessions::SessionTarget};
use kerminal_terminal::{
    cursor_visibility::CursorVisibilityTracker,
    shell_editing::{FilteredShellEditingOutput, ShellEditingFrameFilter},
};
use std::{io, pin::Pin, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

/// 隐藏光标后最多等待重绘的绝对时间；一次输入的重绘实测在 10–23ms 内到达。
pub(super) const CURSOR_HOLD_DEADLINE: Duration = Duration::from_millis(32);

/// Bash 只读探测执行前 readline 会擦除当前提示符行，恢复重绘可能在后续 SSH 分段才到达。
///
/// 32ms 与光标合批一致：覆盖一次本机或局域网往返，超过后放行擦除，避免拖慢普通输出。
pub(super) const BASH_REDRAW_HOLD_DEADLINE: Duration = Duration::from_millis(32);

const ZMODEM_PREFIX_DEADLINE: Duration = Duration::from_millis(250);

/// readline 在调用按键绑定函数前擦除整行；真正闪动来自擦除先于提示符恢复被发布。
const BASH_REDRAW_ERASE: &[u8] = b"\r\x1b[K";

/// Bash hook 把 OSC 133 B 放在 PS1 内，标签先打印一行，提示符恢复后该标记才到达。
/// 结束符既可能是 BEL，也可能是 ST，二者都表示提示符已可交互。
const PROMPT_BOUNDARY_PREFIX: &[u8] = b"\x1b]133;B";

/// SSH 不保证一次远端重绘由同一条消息交付；只给已识别的 PowerShell 和本地 ConPTY 启用等待。
///
/// @author kongweiguang
pub(super) fn enabled(target: &SessionTarget, transport: &SessionTransport) -> bool {
    transport.control.terminal_resize_capability() == TerminalResizeCapability::WindowsConPty
        || (matches!(target, SessionTarget::Ssh(_))
            && transport.shell_dialect == RemoteShellDialect::PowerShell)
}

/// 合批 reader 的单个事件；终止事件会延迟到已缓存字节交付之后。
pub(super) enum ReaderEvent {
    Data(FilteredShellEditingOutput),
    End,
    Failed(io::Error),
    Cancelled,
}

/// 包装 PTY reader，把真实读取、重绘合批和终止信号排队集中在一个 helper 内。
///
/// 调用方复用原始读取缓冲，过滤器把可见字节和私有帧直接追加到同一个交付批次。
/// EOF、错误或取消发生在持有期间时，先返回已读批次，再报告终止事件，保留消费顺序。
pub(super) struct SessionOutputReader {
    reader: Pin<Box<dyn AsyncRead + Send + 'static>>,
    cancellation: CancellationToken,
    filter: ShellEditingFrameFilter,
    tracker: Option<CursorVisibilityTracker>,
    /// Bash 探测门控只在本代已安装 Bash 且探测在途时开启；None 表示不参与该合批。
    bash_probe: Option<BashProbeGate>,
    /// 擦除序列可能被 transport 拆到两次读取，必须跨批保留匹配进度。
    erase: EraseScanner,
    terminated: Option<ReaderEvent>,
    zmodem: Option<HandshakeDetector>,
    zmodem_detected: Option<(Handshake, Vec<u8>)>,
    resumed_bytes: Vec<u8>,
}

impl SessionOutputReader {
    /// 初始化本代读取状态，避免重连沿用上一个 transport 的私有帧或光标状态。
    ///
    /// @author kongweiguang
    pub(super) fn new(
        reader: Pin<Box<dyn AsyncRead + Send + 'static>>,
        cancellation: CancellationToken,
        coalesce_cursor: bool,
    ) -> Self {
        Self {
            reader,
            cancellation,
            filter: ShellEditingFrameFilter::default(),
            tracker: coalesce_cursor.then(CursorVisibilityTracker::default),
            bash_probe: None,
            erase: EraseScanner::default(),
            terminated: None,
            zmodem: None,
            zmodem_detected: None,
            resumed_bytes: Vec::new(),
        }
    }

    /// 只有正常的桌面 SSH reader 识别协议，其它 reader 保持直通。
    pub(super) fn enable_zmodem(&mut self) {
        self.zmodem = Some(HandshakeDetector::default());
    }

    /// 本批普通输出先交付，再把同一 transport 的原始字节交给协议状态机。
    pub(super) fn take_zmodem(&mut self) -> Option<(Handshake, Vec<u8>)> {
        self.zmodem_detected.take()
    }

    /// 协议驱动借用原 reader；不存在第二个读取者，也不经过 Shell OSC 过滤器。
    pub(super) fn zmodem_reader(&mut self) -> &mut Pin<Box<dyn AsyncRead + Send + 'static>> {
        &mut self.reader
    }

    /// 结束帧后的字节仍通过正常过滤器，随后按既有路径处理 EOF 或读错误。
    pub(super) fn resume_zmodem(&mut self, terminal: Vec<u8>, end: Option<ReaderEvent>) {
        self.resumed_bytes = terminal;
        self.terminated = end;
    }

    /// 统一处理新读块和上次协议结束后的尾部，连续握手仍能够被识别。
    fn filter_transport(&mut self, raw: &[u8], output: &mut FilteredShellEditingOutput) {
        if let Some(detector) = &mut self.zmodem {
            let detected = detector.push(raw);
            self.filter.advance_into(&detected.visible, output);
            if let Some(handshake) = detected.handshake {
                self.zmodem_detected = Some((handshake, detected.remaining));
            }
        } else {
            self.filter.advance_into(raw, output);
        }
    }

    /// 绑定当前代的 Bash 探测门控；没有安装编辑能力时保持 None，普通输出路径完全不变。
    ///
    /// @author kongweiguang
    pub(super) fn with_bash_probe(mut self, gate: Option<BashProbeGate>) -> Self {
        self.bash_probe = gate;
        self
    }

    /// EOF 和读取失败时，释放过滤器暂存的普通前缀，私有帧残片仍应丢弃。
    ///
    /// @author kongweiguang
    pub(super) fn finish(&mut self) -> Vec<u8> {
        if let Some(detector) = &mut self.zmodem {
            let mut output = FilteredShellEditingOutput {
                visible: Vec::new(),
                frames: Vec::new(),
                overflowed: false,
            };
            self.filter.advance_into(&detector.flush(), &mut output);
            output.visible.extend(self.filter.finish());
            return output.visible;
        }
        self.filter.finish()
    }

    /// 关闭本代的重绘合批但保留私有帧过滤，避免确认窗口中的定时读取被丢弃时带走已读字节。
    ///
    /// @author kongweiguang
    pub(super) fn disable_coalescing(&mut self) {
        self.tracker = None;
        self.bash_probe = None;
        self.zmodem = None;
    }

    /// 先过滤私有帧，再用可见字节判定重绘，避免私有 OSC 正文进入 VTE 缓存。
    /// 原始字节计入批次预算，即使持续收到私有帧也不能超过时间或容量上限。
    ///
    /// Bash 探测的擦除与恢复可能被 SSH 分段交付；只在探测在途时从擦除处继续读取，
    /// 等到 OSC 133 B 或绝对截止时间再一次性交付，避免提示符闪空。
    ///
    /// @author kongweiguang
    pub(super) async fn read(&mut self, buffer: &mut [u8]) -> ReaderEvent {
        let mut output = FilteredShellEditingOutput {
            visible: Vec::new(),
            frames: Vec::new(),
            overflowed: false,
        };
        if !self.resumed_bytes.is_empty() {
            let resumed = std::mem::take(&mut self.resumed_bytes);
            self.filter_transport(&resumed, &mut output);
            return ReaderEvent::Data(output);
        }
        if let Some(event) = self.terminated.take() {
            return event;
        }
        let mut filled = 0;
        let mut cursor_deadline: Option<Instant> = None;
        let mut handshake_deadline = self
            .zmodem
            .as_ref()
            .filter(|detector| detector.has_pending())
            .map(|_| Instant::now() + ZMODEM_PREFIX_DEADLINE);
        // None 表示本批没有等待提示符恢复；Some 时保留首次擦除时刻，后续分段不能延长。
        let mut bash_hold: Option<BashRedrawHold> = None;
        loop {
            let deadline = match (
                cursor_deadline,
                bash_hold.as_ref().map(|hold| hold.deadline),
            ) {
                (Some(cursor), Some(bash)) => Some(cursor.min(bash)),
                (cursor, bash) => cursor.or(bash),
            };
            let hold = async move {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            };
            let terminal_event = tokio::select! {
                // 截止或取消已就绪时先交付已读字节，避免持续可读的 PTY 延长本批等待。
                biased;
                _ = self.cancellation.cancelled() => ReaderEvent::Cancelled,
                _ = hold => return ReaderEvent::Data(output),
                _ = async {
                    if let Some(deadline) = handshake_deadline {
                        tokio::time::sleep_until(deadline).await;
                    }
                }, if handshake_deadline.is_some() => {
                    if let Some(detector) = &mut self.zmodem {
                        self.filter.advance_into(&detector.flush(), &mut output);
                    }
                    return ReaderEvent::Data(output);
                }
                result = self.reader.read(&mut buffer[filled..]) => match result {
                    Ok(0) => ReaderEvent::End,
                    Err(error) => ReaderEvent::Failed(error),
                    Ok(read) => {
                        let visible_start = output.visible.len();
                        output.visible.reserve(read);
                        let raw = &buffer[filled..filled + read];
                        self.filter_transport(raw, &mut output);
                        if self.zmodem_detected.is_some() {
                            return ReaderEvent::Data(output);
                        }
                        if self.zmodem.as_ref().is_some_and(HandshakeDetector::has_pending) {
                            handshake_deadline.get_or_insert(Instant::now() + ZMODEM_PREFIX_DEADLINE);
                        } else {
                            handshake_deadline = None;
                        }
                        filled += read;
                        let visible = &output.visible[visible_start..];
                        let cursor_change = self
                            .tracker
                            .as_mut()
                            .map(|tracker| tracker.advance(visible));
                        let cursor_should_hold = cursor_change.is_some_and(|change| {
                            change.newly_hidden || (cursor_deadline.is_some() && !change.after_visible)
                        });
                        match cursor_change {
                            // 隐藏周期已被 show 闭合时必须清掉旧截止，否则它会在 Bash 继续等待时
                            // 提前放行同一批次，重新暴露擦除后的空行。
                            Some(change) if change.after_visible => cursor_deadline = None,
                            Some(change) if change.newly_hidden => {
                                cursor_deadline
                                    .get_or_insert_with(|| Instant::now() + CURSOR_HOLD_DEADLINE);
                            }
                            _ => {}
                        }
                        let mut boundary = false;
                        if let Some(hold) = bash_hold.as_mut() {
                            boundary = hold.boundary.scan(visible);
                        } else if self.bash_probe.as_ref().is_some_and(BashProbeGate::in_flight) {
                            // 只有探测在途时才值得等待；其它输出与 Shell 保持立即交付。
                            if let Some(end) = self.erase.scan(visible) {
                                let mut scanner = PromptBoundaryScanner::default();
                                boundary = scanner.scan(&visible[end..]);
                                bash_hold = Some(BashRedrawHold {
                                    deadline: Instant::now() + BASH_REDRAW_HOLD_DEADLINE,
                                    boundary: scanner,
                                });
                            }
                        } else {
                            // 探测不在途时完全跳过状态机；擦除序列只可能出现在探测发送之后，
                            // 因此这里丢掉分段进度不会漏掉真实响应。
                            self.erase.reset();
                        }
                        if boundary {
                            bash_hold = None;
                        }
                        let has_room = filled < buffer.len();
                        if handshake_deadline.is_some() && output.visible.is_empty() && output.frames.is_empty() {
                            filled = 0;
                            continue;
                        }
                        if has_room && (cursor_should_hold || bash_hold.is_some()) {
                            continue;
                        }
                        return ReaderEvent::Data(output);
                    }
                },
            };
            if filled == 0 {
                return terminal_event;
            }
            self.terminated = Some(terminal_event);
            return ReaderEvent::Data(output);
        }
    }
}

/// 一次 Bash 探测擦除后仍未恢复提示符的批次；截止时间与边界扫描只在本次读取内有效。
struct BashRedrawHold {
    deadline: Instant,
    boundary: PromptBoundaryScanner,
}

/// 跨读取识别 `\r ESC [ K`；不匹配时按当前字节重新开始，覆盖被拆开的 ESC 或 CSI 前缀。
#[derive(Default)]
struct EraseScanner {
    matched: usize,
}

impl EraseScanner {
    /// 返回完整擦除序列的结束位置（相对本次可见字节）；未完成时只保留进度。
    ///
    /// 空状态下先找 `\r`；普通输出大多不含回车，避免每个 chunk 都跑完整状态机。
    ///
    /// @author kongweiguang
    fn scan(&mut self, bytes: &[u8]) -> Option<usize> {
        let start = if self.matched == 0 {
            bytes
                .iter()
                .position(|&byte| byte == BASH_REDRAW_ERASE[0])?
        } else {
            0
        };
        for (index, &byte) in bytes.iter().enumerate().skip(start) {
            if byte == BASH_REDRAW_ERASE[self.matched] {
                self.matched += 1;
                if self.matched == BASH_REDRAW_ERASE.len() {
                    self.matched = 0;
                    return Some(index + 1);
                }
            } else {
                self.matched = if byte == BASH_REDRAW_ERASE[0] { 1 } else { 0 };
            }
        }
        None
    }

    /// 探测不在途时丢弃跨批进度；此后到达的擦除序列不会再被拼接到旧前缀上。
    ///
    /// @author kongweiguang
    fn reset(&mut self) {
        self.matched = 0;
    }
}

/// 识别 PS1 的 OSC 133 B 结束标记；接受 BEL 与 ST，匹配状态可跨读取分段保留。
#[derive(Default)]
struct PromptBoundaryScanner {
    phase: BoundaryPhase,
}

#[derive(Clone, Copy)]
enum BoundaryPhase {
    Prefix(usize),
    Terminator,
    Escape,
}

impl Default for BoundaryPhase {
    /// 新 Scanner 从空匹配开始；`#[default]` 不能标注带字段的变体。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self::Prefix(0)
    }
}

impl PromptBoundaryScanner {
    /// 只有完整结束符才算提示符恢复；单独的前缀字节不能提前放行擦除批次。
    ///
    /// @author kongweiguang
    fn scan(&mut self, bytes: &[u8]) -> bool {
        for &byte in bytes {
            self.phase = match self.phase {
                BoundaryPhase::Prefix(index) if byte == PROMPT_BOUNDARY_PREFIX[index] => {
                    let next = index + 1;
                    if next == PROMPT_BOUNDARY_PREFIX.len() {
                        BoundaryPhase::Terminator
                    } else {
                        BoundaryPhase::Prefix(next)
                    }
                }
                BoundaryPhase::Terminator if byte == b'\x07' => return true,
                BoundaryPhase::Terminator if byte == b'\x1b' => BoundaryPhase::Escape,
                BoundaryPhase::Escape if byte == b'\\' => return true,
                BoundaryPhase::Escape if byte == b'\x1b' => BoundaryPhase::Escape,
                _ => restart_boundary(byte),
            };
        }
        false
    }
}

/// 失配后若当前字节本身是 ESC，就把它当作新标记的第一个字节，避免漏掉紧邻的重绘。
fn restart_boundary(byte: u8) -> BoundaryPhase {
    if byte == PROMPT_BOUNDARY_PREFIX[0] {
        BoundaryPhase::Prefix(1)
    } else {
        BoundaryPhase::Prefix(0)
    }
}
