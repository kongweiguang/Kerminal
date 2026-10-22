//! OSC 133/7 分块解析、目录校验与容量边界回归。
//!
//! @author kongweiguang

#[path = "../src/shell_integration.rs"]
mod shell_integration;

use shell_integration::{Osc133Scanner, ShellIntegrationMarker};

/// 将 scanner 的真实有序 offset 结果投影为断言使用的 marker 序列。
///
/// @author kongweiguang
fn markers(scanner: &mut Osc133Scanner, bytes: &[u8]) -> Vec<ShellIntegrationMarker> {
    scanner
        .advance_with_offsets(bytes)
        .into_iter()
        .map(|event| event.marker)
        .collect()
}

#[test]
/// 验证 ESC/OSC/ST 与 BEL 被拆到多个 PTY 块时仍按 wire 顺序产生真实标记。
///
/// @author kongweiguang
fn parses_fragmented_osc133_markers() {
    let mut scanner = Osc133Scanner::default();
    assert!(markers(&mut scanner, b"prompt\x1b").is_empty());
    assert!(markers(&mut scanner, b"]133;A\x1b").is_empty());
    assert_eq!(
        markers(&mut scanner, b"\\\x1b]133;B\x07"),
        vec![
            ShellIntegrationMarker::PromptStart,
            ShellIntegrationMarker::PromptEnd
        ],
    );
    assert_eq!(
        markers(&mut scanner, b"\x1b]133;C\x07\x1b]133;D;23;more\x1b\\"),
        vec![
            ShellIntegrationMarker::CommandStart,
            ShellIntegrationMarker::CommandFinished {
                exit_code: Some(23)
            },
        ],
    );
}

#[test]
/// 拒绝相似 OSC 编号；无效退出码仍保留 D 边界但投影为未知状态。
///
/// @author kongweiguang
fn ignores_non_133_and_invalid_exit_status() {
    let mut scanner = Osc133Scanner::default();
    assert_eq!(
        markers(
            &mut scanner,
            b"\x1b]1334;A\x07\x1b]0;133;A\x07\x1b]133;D;x\x07"
        ),
        vec![ShellIntegrationMarker::CommandFinished { exit_code: None }],
    );
    assert_eq!(
        markers(&mut scanner, b"\x1b]133;D\x07"),
        vec![ShellIntegrationMarker::CommandFinished { exit_code: None }],
    );
}

#[test]
/// 超长 OSC 不累积额外内存，结束符后解析器仍能识别后续合法事件。
///
/// @author kongweiguang
fn discards_oversized_osc_and_recovers() {
    let mut scanner = Osc133Scanner::default();
    let mut bytes = b"\x1b]133;".to_vec();
    bytes.extend(std::iter::repeat_n(b'x', 4096));
    bytes.extend_from_slice(b"\x07\x1b]133;A\x07");
    assert_eq!(
        markers(&mut scanner, &bytes),
        vec![ShellIntegrationMarker::PromptStart]
    );
}

/// 验证完整 OSC 的排他 end offset 指向真实 chunk 位置，包括前一块开始的分片序列。
///
/// @author kongweiguang
#[test]
fn reports_marker_completion_offsets_across_chunks() {
    use shell_integration::ShellIntegrationMarker;

    let mut scanner = Osc133Scanner::default();
    assert!(scanner.advance_with_offsets(b"prefix\x1b]133;C").is_empty());
    let events = scanner.advance_with_offsets(b"\x07output\x1b]133;D;4\x1b\\tail");

    assert_eq!(events.len(), 2);
    assert_eq!(events[0].marker, ShellIntegrationMarker::CommandStart);
    assert_eq!(events[0].end_offset, 1);
    assert_eq!(
        events[1].marker,
        ShellIntegrationMarker::CommandFinished { exit_code: Some(4) }
    );
    assert_eq!(events[1].end_offset, b"\x07output\x1b]133;D;4\x1b\\".len());
}

/// URI 的 UTF8 与终止符均可跨 PTY 块；目录事件不得混入 OSC133 的命令边界。
///
/// @author kongweiguang
#[test]
fn parses_fragmented_osc7_without_changing_shell_markers() {
    let mut scanner = Osc133Scanner::default();
    assert!(markers(&mut scanner, b"\x1b]7;file://server/srv/%E4%B8").is_empty());
    assert_eq!(scanner.take_current_working_directory(), None);
    assert!(markers(&mut scanner, b"%AD%E6%96%87%20%23%25\x1b").is_empty());
    assert_eq!(scanner.take_current_working_directory(), None);
    assert_eq!(
        markers(&mut scanner, b"\\\x1b]133;A\x07"),
        vec![ShellIntegrationMarker::PromptStart],
    );
    assert_eq!(
        scanner.take_current_working_directory().as_deref(),
        Some("/srv/中文 #%"),
    );
    assert_eq!(scanner.take_current_working_directory(), None);
}

/// 只解码路径而不把 URI host 当作路由信息；Windows drive URI 投影成原生绝对路径。
///
/// @author kongweiguang
#[test]
fn accepts_absolute_file_uris_and_normalizes_drive_paths() {
    let mut scanner = Osc133Scanner::default();
    for (uri, expected) in [
        ("file:///", "/"),
        ("file://localhost/srv/a%2fb", "/srv/a/b"),
        ("file://[::1]/srv/work", "/srv/work"),
        ("file:///C%3A/Users/a%20b", "C:/Users/a b"),
        ("file:////server/share", "//server/share"),
    ] {
        assert!(markers(&mut scanner, format!("\x1b]7;{uri}\x07").as_bytes()).is_empty());
        assert_eq!(
            scanner.take_current_working_directory().as_deref(),
            Some(expected)
        );
    }
}

/// 非法协议、相对路径、坏 UTF8 与控制字节全部 fail closed，不覆盖此前有效的待消费通知。
///
/// @author kongweiguang
#[test]
fn invalid_directory_notifications_do_not_replace_the_last_valid_path() {
    let mut scanner = Osc133Scanner::default();
    for uri in [
        "http:///srv/work",
        "file:/srv/work",
        "file://server",
        "file://user@server/srv/work",
        "file://server:22/srv/work",
        "file:///srv/raw space",
        "file:///srv/raw#fragment",
        "file:///srv/raw?query",
        "file:///srv/raw\\path",
        "file:///srv/%",
        "file:///srv/%0",
        "file:///srv/%GG",
        "file:///srv/%FF",
        "file:///srv/%C0%AF",
        "file:///srv/%00",
        "file:///srv/%0A",
        "file:///srv/%1B",
        "file:///srv/%7F",
        "file:///srv/%C2%85",
    ] {
        markers(&mut scanner, b"\x1b]7;file:///valid\x07");
        markers(&mut scanner, format!("\x1b]7;{uri}\x07").as_bytes());
        assert_eq!(
            scanner.take_current_working_directory().as_deref(),
            Some("/valid"),
            "invalid URI changed the pending directory: {uri}",
        );
    }
}

/// 单个恶意 OSC7 不能扩大 scanner 内存或吞掉后续目录/命令事件，跨块也沿用同一预算。
///
/// @author kongweiguang
#[test]
fn oversized_directory_is_discarded_and_following_events_recover() {
    let mut scanner = Osc133Scanner::default();
    markers(&mut scanner, b"\x1b]7;file:///old\x07\x1b]7;file:///");
    for _ in 0..32 {
        markers(&mut scanner, &[b'x'; 1024]);
    }
    assert_eq!(
        markers(&mut scanner, b"\x1b\\\x1b]7;file:///next\x07\x1b]133;B\x07"),
        vec![ShellIntegrationMarker::PromptEnd],
    );
    assert_eq!(
        scanner.take_current_working_directory().as_deref(),
        Some("/next")
    );
}
