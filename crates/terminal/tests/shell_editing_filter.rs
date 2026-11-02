//! 验证私有编辑快照在日志和终端解析之前可跨任意输出分包安全剥离。
//!
//! @author kongweiguang

use kerminal_terminal::shell_editing::{
    MAX_SHELL_EDITING_FRAME_BYTES, SHELL_EDITING_FRAME_PREFIX, ShellEditingFrameFilter,
};

/// 每个分割点均保持普通输出顺序，且私有正文只出现在显式帧结果中。
///
/// @author kongweiguang
#[test]
fn private_frame_is_removed_across_every_two_chunk_split() {
    let mut wire = b"before\x1b[31m".to_vec();
    wire.extend_from_slice(SHELL_EDITING_FRAME_PREFIX);
    wire.extend_from_slice(b"nonce;secret-command\x07after");
    for split in 0..=wire.len() {
        let mut filter = ShellEditingFrameFilter::default();
        let first = filter.advance(&wire[..split]);
        let second = filter.advance(&wire[split..]);
        let visible = [first.visible, second.visible, filter.finish()].concat();
        assert_eq!(visible, b"before\x1b[31mafter", "split {split}");
        assert_eq!(
            first
                .frames
                .into_iter()
                .chain(second.frames)
                .map(|frame| frame.payload)
                .collect::<Vec<_>>(),
            vec![b"nonce;secret-command".to_vec()],
            "split {split}"
        );
    }
}

/// 普通 OSC 和重复 ESC 与专用前缀相邻时都必须逐字节退回。
///
/// @author kongweiguang
#[test]
fn ordinary_escape_sequences_and_incomplete_prefix_are_preserved() {
    let wire = b"\x1b\x1b]7;file:///tmp\x07\x1b]6977;other\x07\x1b]6977;Ker";
    let mut filter = ShellEditingFrameFilter::default();
    let mut visible = Vec::new();
    for byte in wire {
        let result = filter.advance(&[*byte]);
        assert!(result.frames.is_empty());
        visible.extend(result.visible);
    }
    visible.extend(filter.finish());
    assert_eq!(visible, wire);
}

/// 超限内容和未闭合帧即使没有解码成功也不能泄露到普通输出。
///
/// @author kongweiguang
#[test]
fn oversized_and_unterminated_frames_are_dropped_without_leaking() {
    let mut filter = ShellEditingFrameFilter::default();
    let mut wire = SHELL_EDITING_FRAME_PREFIX.to_vec();
    wire.extend(vec![b'x'; MAX_SHELL_EDITING_FRAME_BYTES + 1]);
    let first = filter.advance(&wire);
    assert!(first.visible.is_empty());
    assert!(first.frames.is_empty());
    assert!(first.overflowed);
    let second = filter.advance(b"secret-tail\x1b\\ok");
    assert_eq!(second.visible, b"ok");
    assert!(second.frames.is_empty());

    let mut incomplete = SHELL_EDITING_FRAME_PREFIX.to_vec();
    incomplete.extend_from_slice(b"private-unfinished");
    assert!(filter.advance(&incomplete).visible.is_empty());
    assert!(filter.finish().is_empty());
}

/// ST 可以跨 chunk，非法 ESC 仅作为私有帧正文，不会串入终端输出。
///
/// @author kongweiguang
#[test]
fn split_st_and_embedded_escape_stay_inside_the_private_frame() {
    let mut filter = ShellEditingFrameFilter::default();
    let mut first = SHELL_EDITING_FRAME_PREFIX.to_vec();
    first.extend_from_slice(b"a\x1bXb\x1b");
    let first = filter.advance(&first);
    let second = filter.advance(b"\\ordinary");
    assert!(first.visible.is_empty());
    assert_eq!(second.visible, b"ordinary");
    assert_eq!(second.frames.len(), 1);
    assert_eq!(second.frames[0].visible_offset, 0);
    assert_eq!(second.frames[0].payload, b"a\x1bXb");
}

/// 可见内容与多个私有帧交错时必须保留相对位置，避免把旧 prompt 快照关联到新 prompt。
///
/// @author kongweiguang
#[test]
fn frame_offsets_preserve_original_order() {
    let mut wire = b"a".to_vec();
    wire.extend_from_slice(SHELL_EDITING_FRAME_PREFIX);
    wire.extend_from_slice(b"one\x07bc");
    wire.extend_from_slice(SHELL_EDITING_FRAME_PREFIX);
    wire.extend_from_slice(b"two\x07d");
    let output = ShellEditingFrameFilter::default().advance(&wire);
    assert_eq!(output.visible, b"abcd");
    assert_eq!(output.frames.len(), 2);
    assert_eq!(output.frames[0].visible_offset, 1);
    assert_eq!(output.frames[1].visible_offset, 3);
}

/// 未知或损坏的版本仍属于私有命名空间，正文绝不能透传到日志与显示路径。
///
/// @author kongweiguang
#[test]
fn unknown_version_is_filtered_before_validation() {
    let mut wire = SHELL_EDITING_FRAME_PREFIX.to_vec();
    wire.extend_from_slice(b"999;private-command\x07visible");
    for split in 0..=wire.len() {
        let mut filter = ShellEditingFrameFilter::default();
        let first = filter.advance(&wire[..split]);
        let second = filter.advance(&wire[split..]);
        assert_eq!(
            [first.visible, second.visible, filter.finish()].concat(),
            b"visible"
        );
        assert_eq!(first.frames.len() + second.frames.len(), 1);
    }
}
