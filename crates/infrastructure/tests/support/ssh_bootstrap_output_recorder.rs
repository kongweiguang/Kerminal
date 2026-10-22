//! SSH bootstrap 验收专用的有界全流统计，不保存、输出或持久化原始终端字节。
//!
//! @author kongweiguang

use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    sessions::logging::{
        OutputRecordResult, SessionLogFailureReason, SessionLogState, SessionOutputRecorder,
    },
};
use kerminal_domain::identity::SessionId;

/// 全流证据只含计数；完整表示 recorder 覆盖连续 reader 回调并完成收尾，不证明远端进程退出。
#[derive(Clone, Copy, Default)]
pub(super) struct OutputStreamEvidence {
    pub(super) bytes: u64,
    pub(super) chunks: u64,
    pub(super) first_sequence: u64,
    pub(super) last_sequence: u64,
    pub(super) sequence_gap_count: u64,
    pub(super) recorder_failed_count: u64,
    pub(super) recorder_finished: bool,
    pub(super) output_after_finish_count: u64,
    pub(super) session_mismatch_count: u64,
    pub(super) literal_octal_prefix_present: bool,
    pub(super) literal_octal_escape_count: u64,
    pub(super) osc133_a_count: u64,
    pub(super) osc133_b_count: u64,
    pub(super) complete: bool,
}

/// 固定数量的模式状态跨 chunk 延续，不缓存终端文本，也不从已裁切的快照推测完整输出。
#[derive(Default)]
struct OutputStreamScan {
    evidence: OutputStreamEvidence,
    octal_digits: u8,
    octal_value: u16,
    octal_prefix_tokens: u8,
    osc_active: bool,
    osc_escape: bool,
    osc_position: u8,
    osc_marker: u8,
    osc_valid: bool,
}

impl OutputStreamScan {
    /// sequence 从一开始且必须连续；缺口次数统计不连续回调，而不是猜测缺失字节数。
    ///
    /// @author kongweiguang
    fn push_chunk(&mut self, sequence: u64, bytes: &[u8]) {
        let expected = self.evidence.last_sequence.saturating_add(1);
        if sequence != expected {
            self.evidence.sequence_gap_count = self.evidence.sequence_gap_count.saturating_add(1);
        }
        if self.evidence.chunks == 0 {
            self.evidence.first_sequence = sequence;
        }
        self.evidence.last_sequence = sequence;
        self.evidence.chunks = self.evidence.chunks.saturating_add(1);
        self.evidence.bytes = self.evidence.bytes.saturating_add(bytes.len() as u64);
        for &byte in bytes {
            self.scan_octal_byte(byte);
            self.scan_osc_byte(byte);
        }
    }

    /// 只保存八进制数字与已匹配 token 数；相邻的三段转义必须连续，避免隔着正文误判前缀。
    ///
    /// @author kongweiguang
    fn scan_octal_byte(&mut self, byte: u8) {
        if byte == b'\\' {
            if self.octal_digits != 0 {
                self.octal_prefix_tokens = 0;
            }
            self.octal_digits = 1;
            self.octal_value = 0;
            return;
        }
        if self.octal_digits != 0 && (b'0'..=b'7').contains(&byte) {
            self.octal_value = self.octal_value * 8 + u16::from(byte - b'0');
            self.octal_digits += 1;
            if self.octal_digits == 4 {
                self.evidence.literal_octal_escape_count =
                    self.evidence.literal_octal_escape_count.saturating_add(1);
                self.octal_prefix_tokens = match (self.octal_prefix_tokens, self.octal_value) {
                    (1, 0o137) => 2,
                    (2, 0o160) => {
                        self.evidence.literal_octal_prefix_present = true;
                        0
                    }
                    (_, 0o154) => 1,
                    _ => 0,
                };
                self.octal_digits = 0;
            }
            return;
        }
        self.octal_digits = 0;
        self.octal_prefix_tokens = 0;
    }

    /// 只认完整结束的 OSC 133 A/B；支持 BEL、ST 与跨 chunk 分隔，残缺前缀不能算 shell 就绪。
    ///
    /// @author kongweiguang
    fn scan_osc_byte(&mut self, byte: u8) {
        if self.osc_active {
            if byte == 0x07 || byte == 0x9c || (self.osc_escape && byte == b'\\') {
                if self.osc_valid && self.osc_position >= 5 {
                    match self.osc_marker {
                        b'A' => {
                            self.evidence.osc133_a_count =
                                self.evidence.osc133_a_count.saturating_add(1)
                        }
                        b'B' => {
                            self.evidence.osc133_b_count =
                                self.evidence.osc133_b_count.saturating_add(1)
                        }
                        _ => {}
                    }
                }
                self.osc_active = false;
                self.osc_escape = false;
                return;
            }
            if self.osc_escape {
                self.osc_valid = false;
                self.osc_escape = false;
            }
            if byte == 0x1b {
                self.osc_escape = true;
                return;
            }
            match self.osc_position {
                0..=3 => self.osc_valid &= byte == b"133;"[usize::from(self.osc_position)],
                4 => {
                    self.osc_marker = byte;
                    self.osc_valid &= matches!(byte, b'A' | b'B');
                }
                5 => self.osc_valid &= byte == b';',
                _ => {}
            }
            self.osc_position = self.osc_position.saturating_add(1);
            return;
        }
        if byte == 0x9d || (self.osc_escape && byte == b']') {
            self.osc_active = true;
            self.osc_escape = false;
            self.osc_position = 0;
            self.osc_marker = 0;
            self.osc_valid = true;
        } else {
            self.osc_escape = byte == 0x1b;
        }
    }
}

#[derive(Default)]
struct OutputRecorderState {
    session_id: Option<SessionId>,
    scan: OutputStreamScan,
}

/// reader 内同步扫描固定状态，无队列、磁盘或原文缓存；锁争用也必须变成显式不完整证据。
#[derive(Default)]
pub(super) struct FullStreamRecorder {
    state: Mutex<OutputRecorderState>,
    failed_callbacks: AtomicU64,
}

impl FullStreamRecorder {
    /// 仅在会话统一关闭后读取统计，失败回调独立计数，不能因丢掉某个 chunk 得出零泄漏结论。
    ///
    /// @author kongweiguang
    pub(super) fn evidence(&self) -> OutputStreamEvidence {
        let Ok(state) = self.state.try_lock() else {
            return OutputStreamEvidence {
                recorder_failed_count: self
                    .failed_callbacks
                    .load(Ordering::Acquire)
                    .saturating_add(1),
                ..OutputStreamEvidence::default()
            };
        };
        let mut evidence = state.scan.evidence;
        evidence.recorder_failed_count = self.failed_callbacks.load(Ordering::Acquire);
        evidence.complete = evidence.bytes > 0
            && evidence.chunks > 0
            && evidence.first_sequence == 1
            && evidence.sequence_gap_count == 0
            && evidence.recorder_failed_count == 0
            && evidence.recorder_finished
            && evidence.output_after_finish_count == 0
            && evidence.session_mismatch_count == 0;
        evidence
    }
}

#[async_trait]
impl SessionOutputRecorder for FullStreamRecorder {
    /// try_lock 保持 reader 不阻塞；争用或毒化会报告失败并永久标记本次扫描不完整。
    ///
    /// @author kongweiguang
    fn try_record_output(
        &self,
        session_id: &SessionId,
        sequence: u64,
        bytes: &[u8],
    ) -> OutputRecordResult {
        let Ok(mut state) = self.state.try_lock() else {
            self.failed_callbacks.fetch_add(1, Ordering::AcqRel);
            return OutputRecordResult::Failed {
                first_missing_sequence: sequence,
                reason: SessionLogFailureReason::QueueFull,
            };
        };
        if let Some(expected) = &state.session_id {
            if expected != session_id {
                state.scan.evidence.session_mismatch_count =
                    state.scan.evidence.session_mismatch_count.saturating_add(1);
            }
        } else {
            state.session_id = Some(session_id.clone());
        }
        if state.scan.evidence.recorder_finished {
            state.scan.evidence.output_after_finish_count = state
                .scan
                .evidence
                .output_after_finish_count
                .saturating_add(1);
        }
        state.scan.push_chunk(sequence, bytes);
        OutputRecordResult::Queued
    }

    /// 此测试没有 writer；仅记录生命周期回调，绝不将它等同远端进程退出确认。
    ///
    /// @author kongweiguang
    async fn finish_session(
        &self,
        session_id: &SessionId,
        _deadline: std::time::Instant,
    ) -> AppResult<SessionLogState> {
        let Ok(mut state) = self.state.try_lock() else {
            self.failed_callbacks.fetch_add(1, Ordering::AcqRel);
            return Err(AppError::infrastructure(
                "acceptance output recorder was unavailable at close",
            ));
        };
        if state
            .session_id
            .as_ref()
            .is_some_and(|expected| expected != session_id)
        {
            state.scan.evidence.session_mismatch_count =
                state.scan.evidence.session_mismatch_count.saturating_add(1);
        }
        state.scan.evidence.recorder_finished = true;
        Ok(SessionLogState::inactive())
    }

    /// 扫描器只有内存中的有限状态，没有额外任务需要等待或生成日志文件。
    ///
    /// @author kongweiguang
    async fn shutdown_until(&self, _deadline: std::time::Instant) -> AppResult<()> {
        Ok(())
    }
}

/// 每个可能的分块边界都必须得到同样结果，避免 bootstrap 回显刚好跨 reader chunk 就漏检。
///
/// @author kongweiguang
#[test]
fn full_stream_scanner_preserves_patterns_across_every_chunk_boundary() {
    let bytes = b"\\154\\154\\137\\160\x1b]133;A;cl=m\x07prompt\x1b]133;B\x1b\\";
    for boundary in 1..bytes.len() {
        let mut scan = OutputStreamScan::default();
        scan.push_chunk(1, &bytes[..boundary]);
        scan.push_chunk(2, &bytes[boundary..]);
        assert_eq!(scan.evidence.bytes, bytes.len() as u64);
        assert_eq!(scan.evidence.literal_octal_escape_count, 4);
        assert!(scan.evidence.literal_octal_prefix_present);
        assert_eq!(scan.evidence.osc133_a_count, 1);
        assert_eq!(scan.evidence.osc133_b_count, 1);
        assert_eq!(scan.evidence.sequence_gap_count, 0);
    }
}

/// 普通正文隔断八进制序列，残缺或错误 OSC 不得升级成已观测到的 shell marker。
///
/// @author kongweiguang
#[test]
fn full_stream_scanner_rejects_interrupted_and_unterminated_markers() {
    let mut scan = OutputStreamScan::default();
    scan.push_chunk(1, b"\\154x\\137\\160\x1b]133;AB\x07\x1b]133;B");
    assert_eq!(scan.evidence.literal_octal_escape_count, 3);
    assert!(!scan.evidence.literal_octal_prefix_present);
    assert_eq!(scan.evidence.osc133_a_count, 0);
    assert_eq!(scan.evidence.osc133_b_count, 0);
    scan.push_chunk(3, b"\x1b\\");
    assert_eq!(scan.evidence.osc133_b_count, 1);
    assert_eq!(scan.evidence.sequence_gap_count, 1);
}

/// 即便跨 chunk 每次只收到一个字节，正常结束的 recorder 也能形成完整脱敏证据。
///
/// @author kongweiguang
#[tokio::test]
async fn full_stream_recorder_finishes_contiguous_bytewise_output() {
    let recorder = FullStreamRecorder::default();
    let session_id = SessionId::new();
    let bytes = b"\x1b]133;A\x07\x9d133;B\x9c";
    for (index, byte) in bytes.iter().enumerate() {
        assert_eq!(
            recorder.try_record_output(&session_id, index as u64 + 1, &[*byte]),
            OutputRecordResult::Queued
        );
    }
    assert!(
        recorder
            .finish_session(&session_id, std::time::Instant::now())
            .await
            .is_ok()
    );
    let stream = recorder.evidence();
    assert!(stream.complete);
    assert_eq!(stream.osc133_a_count, 1);
    assert_eq!(stream.osc133_b_count, 1);
    assert_eq!(stream.bytes, bytes.len() as u64);

    let empty_recorder = FullStreamRecorder::default();
    assert_eq!(
        empty_recorder.try_record_output(&session_id, 1, &[]),
        OutputRecordResult::Queued
    );
    assert!(
        empty_recorder
            .finish_session(&session_id, std::time::Instant::now())
            .await
            .is_ok()
    );
    let empty_stream = empty_recorder.evidence();
    assert_eq!(empty_stream.bytes, 0);
    assert_eq!(empty_stream.chunks, 1);
    assert!(empty_stream.recorder_finished);
    assert!(!empty_stream.complete);
}

/// 重复 sequence、锁争用、序列缺口和收尾后回调全部可见，不能靠零计数伪造完整扫描。
///
/// @author kongweiguang
#[tokio::test]
async fn full_stream_recorder_marks_contention_and_late_output_incomplete() {
    let recorder = FullStreamRecorder::default();
    let session_id = SessionId::new();
    assert_eq!(
        recorder.try_record_output(&session_id, 1, &[]),
        OutputRecordResult::Queued
    );
    assert_eq!(
        recorder.try_record_output(&session_id, 1, b"first"),
        OutputRecordResult::Queued
    );
    {
        let _guard = recorder
            .state
            .lock()
            .expect("test owns the uncontended recorder");
        assert!(matches!(
            recorder.try_record_output(&session_id, 2, b"missing"),
            OutputRecordResult::Failed { .. }
        ));
    }
    assert!(
        recorder
            .finish_session(&session_id, std::time::Instant::now())
            .await
            .is_ok()
    );
    assert_eq!(
        recorder.try_record_output(&session_id, 3, b"late"),
        OutputRecordResult::Queued
    );
    let stream = recorder.evidence();
    assert!(!stream.complete);
    assert_eq!(stream.recorder_failed_count, 1);
    assert_eq!(stream.sequence_gap_count, 2);
    assert_eq!(stream.output_after_finish_count, 1);
    assert!(stream.recorder_finished);
}
