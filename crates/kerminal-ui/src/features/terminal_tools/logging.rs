//! 当前终端原始输出日志状态与 start/stop 操作按钮。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, EventEmitter, IntoElement, Render, Window,
    component::{
        Disableable,
        button::{Button, ButtonVariants as _},
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::sessions::logging::{SessionLogFailureReason, SessionLogState};
use kerminal_domain::identity::SessionId;

use crate::theme::ThemeTokens;

/// 日志控件只发布显式操作；start/stop/state 的异步执行由 Root 的日志 use case 负责。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalLogEvent {
    Start { session_id: SessionId },
    Stop { session_id: SessionId },
    Refresh { session_id: SessionId },
}

/// 日志状态视图不访问文件系统，路径和原始终端内容不会直接暴露在工具栏。
pub struct TerminalLogControls {
    session_id: Option<SessionId>,
    state: SessionLogState,
    pending: bool,
    error: Option<String>,
}

impl EventEmitter<TerminalLogEvent> for TerminalLogControls {}

impl TerminalLogControls {
    /// 初始化为未启用状态；服务没有装配时宿主应隐藏该控件而非显示空操作。
    pub fn new(session_id: Option<SessionId>) -> Self {
        Self {
            session_id,
            state: SessionLogState::inactive(),
            pending: false,
            error: None,
        }
    }

    /// 清除旧 session 状态并加载新投影，避免把一个会话的日志提示套到另一个会话。
    pub fn set_session(&mut self, session_id: Option<SessionId>, cx: &mut Context<Self>) {
        if self.session_id != session_id {
            self.session_id = session_id;
            self.state = SessionLogState::inactive();
            self.pending = false;
            self.error = None;
            cx.notify();
        }
    }

    /// 只接受当前 session 的日志状态快照，阻止迟到请求覆盖活动目标状态。
    pub fn set_state(
        &mut self,
        session_id: &SessionId,
        state: SessionLogState,
        cx: &mut Context<Self>,
    ) {
        if self.session_id.as_ref() != Some(session_id) {
            return;
        }
        self.state = state;
        self.pending = false;
        self.error = None;
        cx.notify();
    }

    /// 反馈明确的用例失败并恢复操作按钮，不伪造日志已启动或已停止。
    pub fn set_error(
        &mut self,
        session_id: &SessionId,
        message: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        if self.session_id.as_ref() != Some(session_id) {
            return;
        }
        self.pending = false;
        self.error = Some(message.into());
        cx.notify();
    }

    /// 请求运行中的日志用例时锁定重复提交，界面线程不等待 I/O。
    pub fn set_pending(&mut self, session_id: &SessionId, pending: bool, cx: &mut Context<Self>) {
        if self.session_id.as_ref() != Some(session_id) {
            return;
        }
        self.pending = pending;
        cx.notify();
    }

    /// 按会话 ID 发布日志操作，缺少活动目标时不发送模糊或空目标请求。
    fn emit_action(&self, action: LogAction, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        let event = match action {
            LogAction::Start => TerminalLogEvent::Start { session_id },
            LogAction::Stop => TerminalLogEvent::Stop { session_id },
            LogAction::Refresh => TerminalLogEvent::Refresh { session_id },
        };
        cx.emit(event);
    }
}

impl Render for TerminalLogControls {
    /// 展示录制状态、字节数和截断原因；不显示具体路径或未经脱敏的终端正文。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        let (state_label, state_color) = match log_status_summary(&self.state) {
            LogStatusSummary::Recording { bytes_written } => (
                crate::locale::text(
                    format!("记录中 · {bytes_written} B"),
                    format!("Recording · {bytes_written} B"),
                ),
                theme.accent,
            ),
            LogStatusSummary::RecordingTruncated { bytes_written } => (
                crate::locale::text(
                    format!("记录中，内容已截断 · {bytes_written} B"),
                    format!("Recording is truncated · {bytes_written} B"),
                ),
                theme.error,
            ),
            LogStatusSummary::Saved { bytes_written } => (
                crate::locale::text(
                    format!("已保存 · {bytes_written} B"),
                    format!("Saved · {bytes_written} B"),
                ),
                theme.muted,
            ),
            LogStatusSummary::Truncated { bytes_written } => (
                crate::locale::text(
                    format!("记录已截断 · {bytes_written} B"),
                    format!("Recording was truncated · {bytes_written} B"),
                ),
                theme.error,
            ),
            LogStatusSummary::Failed { bytes_written } => (
                crate::locale::text(
                    format!("记录失败 · {bytes_written} B"),
                    format!("Recording failed · {bytes_written} B"),
                ),
                theme.error,
            ),
            LogStatusSummary::NotRecording => {
                (crate::locale::text("未记录", "Not recording"), theme.muted)
            }
        };
        let failure = self.state.failure_reason.map(log_failure_label);
        let error = self.error.clone();
        let has_session = self.session_id.is_some();
        div()
            .id("terminal-log-controls")
            .w_full()
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .rounded_md()
            .bg(theme.panel)
            .text_color(theme.foreground)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(crate::locale::text("终端日志", "Terminal log"))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(state_color)
                            .child(state_label),
                    ),
            )
            .when_some(failure, |panel, failure| {
                panel.child(div().text_size(px(11.)).text_color(theme.error).child(
                    crate::locale::text(
                        format!("{failure}；日志内容可能不完整"),
                        format!("{failure}; log contents may be incomplete."),
                    ),
                ))
            })
            .when_some(error, |panel, error| {
                panel.child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.error)
                        .child(error),
                )
            })
            .child(
                div()
                    .flex()
                    .gap_1()
                    .child(
                        Button::new("terminal-log-start")
                            .compact()
                            .primary()
                            .label(if self.pending {
                                crate::locale::text("处理中…", "Working…")
                            } else {
                                crate::locale::text("开始记录", "Start recording")
                            })
                            .disabled(!has_session || self.pending || self.state.active)
                            .on_click(cx.listener(|controls, _: &ClickEvent, _, cx| {
                                controls.emit_action(LogAction::Start, cx);
                                controls.pending = true;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("terminal-log-stop")
                            .compact()
                            .label(crate::locale::text("停止并刷新", "Stop and refresh"))
                            .disabled(!has_session || self.pending || !self.state.active)
                            .on_click(cx.listener(|controls, _: &ClickEvent, _, cx| {
                                controls.emit_action(LogAction::Stop, cx);
                                controls.pending = true;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("terminal-log-refresh")
                            .compact()
                            .label(crate::locale::text("刷新状态", "Refresh status"))
                            .disabled(!has_session || self.pending)
                            .on_click(cx.listener(|controls, _: &ClickEvent, _, cx| {
                                controls.emit_action(LogAction::Refresh, cx);
                                controls.pending = true;
                                cx.notify();
                            })),
                    ),
            )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogStatusSummary {
    Recording { bytes_written: u64 },
    RecordingTruncated { bytes_written: u64 },
    Saved { bytes_written: u64 },
    Truncated { bytes_written: u64 },
    Failed { bytes_written: u64 },
    NotRecording,
}

/// 将 use case 的终端日志快照投影为有限 UI 状态，不依赖路径存在或泄露文件位置。
fn log_status_summary(state: &SessionLogState) -> LogStatusSummary {
    if state.active && state.truncated {
        LogStatusSummary::RecordingTruncated {
            bytes_written: state.bytes_written,
        }
    } else if state.active {
        LogStatusSummary::Recording {
            bytes_written: state.bytes_written,
        }
    } else if state.truncated {
        LogStatusSummary::Truncated {
            bytes_written: state.bytes_written,
        }
    } else if state.failure_reason.is_some() {
        LogStatusSummary::Failed {
            bytes_written: state.bytes_written,
        }
    } else if state.path.is_some() {
        LogStatusSummary::Saved {
            bytes_written: state.bytes_written,
        }
    } else {
        LogStatusSummary::NotRecording
    }
}

/// 将内部失败枚举映射成稳定简短提示，不把 OS 路径或低层错误串带入 UI。
fn log_failure_label(reason: SessionLogFailureReason) -> String {
    match reason {
        SessionLogFailureReason::QueueFull => {
            crate::locale::text("日志队列已满", "The log queue is full.").to_string()
        }
        SessionLogFailureReason::SequenceGap => {
            crate::locale::text("输出片段缺失", "An output chunk is missing.").to_string()
        }
        SessionLogFailureReason::WriterIo => {
            crate::locale::text("日志写入失败", "Writing the log failed.").to_string()
        }
        SessionLogFailureReason::WriterStopped => {
            crate::locale::text("日志写入器已停止", "The log writer stopped.").to_string()
        }
        SessionLogFailureReason::Shutdown => {
            crate::locale::text("应用正在关闭", "The application is shutting down.").to_string()
        }
        SessionLogFailureReason::RedactionSafetyLimit => crate::locale::text(
            "脱敏安全限制已触发",
            "The redaction safety limit was reached.",
        )
        .to_string(),
    }
}

#[derive(Clone, Copy)]
enum LogAction {
    Start,
    Stop,
    Refresh,
}

#[cfg(test)]
#[path = "../../../tests/unit/terminal_log_status.rs"]
mod tests;
