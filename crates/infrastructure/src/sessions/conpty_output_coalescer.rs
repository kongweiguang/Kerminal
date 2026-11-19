//! 对 Windows ConPTY 和已识别的远端 PowerShell 合并分段重绘，保留私有帧在批次中的位置。
//!
//! 普通输出立即交付，只有可见光标被隐藏时才等待恢复。
//!
//! @author kongweiguang

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

/// 包装 PTY reader，把真实读取、光标合批和终止信号排队集中在一个 helper 内。
///
/// 调用方复用原始读取缓冲，过滤器把可见字节和私有帧直接追加到同一个交付批次。
/// EOF、错误或取消发生在持有期间时，先返回已读批次，再报告终止事件，保留消费顺序。
pub(super) struct SessionOutputReader {
    reader: Pin<Box<dyn AsyncRead + Send + 'static>>,
    cancellation: CancellationToken,
    filter: ShellEditingFrameFilter,
    tracker: Option<CursorVisibilityTracker>,
    terminated: Option<ReaderEvent>,
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
            terminated: None,
        }
    }

    /// EOF 和读取失败时，释放过滤器暂存的普通前缀，私有帧残片仍应丢弃。
    ///
    /// @author kongweiguang
    pub(super) fn finish(&mut self) -> Vec<u8> {
        self.filter.finish()
    }

    /// 关闭本代的光标合批但保留私有帧过滤，避免确认窗口中的定时读取被丢弃时带走已读字节。
    ///
    /// @author kongweiguang
    pub(super) fn disable_coalescing(&mut self) {
        self.tracker = None;
    }

    /// 先过滤私有帧，再用可见字节判定重绘，避免私有 OSC 正文进入 VTE 缓存。
    /// 原始字节计入批次预算，即使持续收到私有帧也不能超过时间或容量上限。
    ///
    /// @author kongweiguang
    pub(super) async fn read(&mut self, buffer: &mut [u8]) -> ReaderEvent {
        if let Some(event) = self.terminated.take() {
            return event;
        }
        let mut output = FilteredShellEditingOutput {
            visible: Vec::new(),
            frames: Vec::new(),
            overflowed: false,
        };
        let mut filled = 0;
        let mut hold_deadline = None;
        loop {
            let hold = async move {
                match hold_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            };
            let terminal_event = tokio::select! {
                // 截止或取消已就绪时先交付已读字节，避免持续可读的 PTY 延长本批等待。
                biased;
                _ = self.cancellation.cancelled() => ReaderEvent::Cancelled,
                _ = hold => return ReaderEvent::Data(output),
                result = self.reader.read(&mut buffer[filled..]) => match result {
                    Ok(0) => ReaderEvent::End,
                    Err(error) => ReaderEvent::Failed(error),
                    Ok(read) => {
                        let visible_start = output.visible.len();
                        output.visible.reserve(read);
                        self.filter.advance_into(&buffer[filled..filled + read], &mut output);
                        filled += read;
                        let should_hold = self.tracker.as_mut().is_some_and(|tracker| {
                            let change = tracker.advance(&output.visible[visible_start..]);
                            change.newly_hidden || (hold_deadline.is_some() && !change.after_visible)
                        });
                        if should_hold && filled < buffer.len() {
                            // 继续读取时保留本批最初的截止时间，重复隐藏不会延长它。
                            hold_deadline.get_or_insert_with(|| Instant::now() + CURSOR_HOLD_DEADLINE);
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
