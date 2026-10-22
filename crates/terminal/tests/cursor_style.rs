//! 验证终端 cursor shape 来源位跨 PTY 分块和 reset 序列保持准确。
//!
//! @author kongweiguang

use kerminal_domain::identity::SessionId;
use kerminal_domain::sessions::TerminalSize;
use kerminal_terminal::engine::{TerminalEngine, TerminalLimits};
use kerminal_terminal::snapshot::RenderSnapshotRequest;

/// 创建受限网格，使测试直接检查真实 parser 输出而非暴露私有扫描器。
///
/// @author kongweiguang
fn engine() -> TerminalEngine {
    let size = TerminalSize::new(80, 24, 0, 0).expect("positive terminal size");
    TerminalEngine::new(SessionId::new(), size, TerminalLimits::default())
        .expect("bounded terminal engine")
}

/// 只有完整有效的 DECSCUSR 可置位来源，且 RIS/DECSTR 均恢复用户默认优先级。
///
/// @author kongweiguang
#[test]
fn cursor_style_source_tracks_fragmented_decscusr_and_resets() {
    let mut engine = engine();
    assert!(
        !engine
            .render_snapshot(RenderSnapshotRequest::default())
            .cursor
            .style_from_terminal
    );

    engine.process_output(b"\x1b[");
    engine.process_output(b"5 ");
    assert!(
        !engine
            .render_snapshot(RenderSnapshotRequest::default())
            .cursor
            .style_from_terminal
    );
    engine.process_output(b"q");
    assert!(
        engine
            .render_snapshot(RenderSnapshotRequest::default())
            .cursor
            .style_from_terminal
    );

    engine.process_output(b"\x1b[!");
    assert!(
        engine
            .render_snapshot(RenderSnapshotRequest::default())
            .cursor
            .style_from_terminal
    );
    engine.process_output(b"p");
    assert!(
        !engine
            .render_snapshot(RenderSnapshotRequest::default())
            .cursor
            .style_from_terminal
    );

    engine.process_output(b"\x1b[3 q");
    assert!(
        engine
            .render_snapshot(RenderSnapshotRequest::default())
            .cursor
            .style_from_terminal
    );
    engine.process_output(b"\x1b");
    engine.process_output(b"c");
    assert!(
        !engine
            .render_snapshot(RenderSnapshotRequest::default())
            .cursor
            .style_from_terminal
    );
}

/// 忽略 OSC 中伪造的 CSI 字节和超出 DEC 标准范围的光标参数。
///
/// @author kongweiguang
#[test]
fn cursor_style_source_ignores_osc_payload_and_invalid_parameters() {
    let mut engine = engine();
    engine.process_output(b"\x1b]0;\x1b[5 q\x07");
    engine.process_output(b"\x1b[7 q");
    engine.process_output(b"\x1b[1;2 q");

    assert!(
        !engine
            .render_snapshot(RenderSnapshotRequest::default())
            .cursor
            .style_from_terminal
    );
}
