//! 终端剪贴板文本优先级、目标 shell 转义与路径拒绝边界。
//!
//! @author kongweiguang

use std::path::PathBuf;

use gpui_kit::{ClipboardEntry, ClipboardItem, ClipboardString, ExternalPaths};
use kerminal_domain::connections::RemoteShellDialect;

use super::*;

/// 只要有非空 Unicode 文本，GPUI 与终端都应优先保留文本并忽略随附路径条目。
#[test]
fn nonempty_text_takes_precedence_over_external_paths() {
    let item = ClipboardItem {
        entries: vec![
            ClipboardEntry::ExternalPaths(ExternalPaths(
                [PathBuf::from("/ignored/file")].into_iter().collect(),
            )),
            ClipboardEntry::String(ClipboardString::new("literal clipboard text".to_owned())),
        ],
    };

    assert!(!requires_shell_dialect(&item));
    assert_eq!(
        terminal_paste_text(&item, RemoteShellDialect::Unknown),
        Ok(Some("literal clipboard text".to_owned()))
    );
}

/// GPUI 把多个字符串直接合并；维持该现有行为以免改变普通文本剪贴板契约。
#[test]
fn string_entries_keep_gpui_concatenation_order() {
    let item = ClipboardItem {
        entries: vec![
            ClipboardEntry::String(ClipboardString::new("first".to_owned())),
            ClipboardEntry::String(ClipboardString::new(" second".to_owned())),
        ],
    };

    assert_eq!(
        terminal_paste_text(&item, RemoteShellDialect::Unknown),
        Ok(Some("first second".to_owned()))
    );
}

/// 空文本与路径并存时沿用 GPUI 的空文本回退规则，文件路径仍会被目标 shell 引用。
#[test]
fn empty_text_falls_back_to_external_paths() {
    let item = ClipboardItem {
        entries: vec![
            ClipboardEntry::String(ClipboardString::new(String::new())),
            ClipboardEntry::ExternalPaths(ExternalPaths(
                [PathBuf::from("/tmp/work file")].into_iter().collect(),
            )),
        ],
    };

    assert!(requires_shell_dialect(&item));
    assert_eq!(
        terminal_paste_text(&item, RemoteShellDialect::Posix),
        Ok(Some("'/tmp/work file'".to_owned()))
    );
}

/// 文件路径分别按目标 shell 单参数规则编码，最终用空格分隔而不插入执行换行。
#[test]
fn multiple_paths_are_quoted_individually_and_space_separated() {
    let item = paths(&["/tmp/one file", "/tmp/two's file"]);

    assert_eq!(
        terminal_paste_text(&item, RemoteShellDialect::Posix),
        Ok(Some("'/tmp/one file' '/tmp/two'\\''s file'".to_owned()))
    );
}

/// Shell 展开符号留在单引号参数中，不能在粘贴时变成命令替换或控制语法。
#[test]
fn shell_metacharacters_remain_literal_path_data() {
    let item = paths(&["/tmp/$(touch marker); $HOME"]);

    assert_eq!(
        terminal_paste_text(&item, RemoteShellDialect::Posix),
        Ok(Some("'/tmp/$(touch marker); $HOME'".to_owned()))
    );
    assert_eq!(
        terminal_paste_text(&item, RemoteShellDialect::PowerShell),
        Ok(Some("'/tmp/$(touch marker); $HOME'".to_owned()))
    );
}

/// Fish 和 PowerShell 使用已有命令编码器的引号规则，不拿本机 OS 猜远端方言。
#[test]
fn paths_use_the_explicit_fish_or_powershell_dialect() {
    let fish = paths(&[r"/tmp/a\b'c"]);
    let powershell = paths(&[r"C:\Users\O'Reilly\file.txt"]);

    assert_eq!(
        terminal_paste_text(&fish, RemoteShellDialect::Fish),
        Ok(Some(r"'/tmp/a\\b\'c'".to_owned()))
    );
    assert_eq!(
        terminal_paste_text(&powershell, RemoteShellDialect::PowerShell),
        Ok(Some("'C:\\Users\\O''Reilly\\file.txt'".to_owned()))
    );
}

/// 未知 shell 只阻止结构化路径编码，普通 Unicode 文本仍可按原 bracketed-paste 通道使用。
#[test]
fn unknown_dialect_rejects_paths_but_not_plain_text() {
    let item = paths(&["/tmp/file"]);
    assert_eq!(
        terminal_paste_text(&item, RemoteShellDialect::Unknown),
        Err(TerminalClipboardError::UnknownShellDialect)
    );

    let text = ClipboardItem::new_string("plain text".to_owned());
    assert_eq!(
        terminal_paste_text(&text, RemoteShellDialect::Unknown),
        Ok(Some("plain text".to_owned()))
    );
}

/// 不可表示的系统路径必须拒绝，不能以替代字符悄悄改变用户要粘贴的目标。
#[cfg(unix)]
#[test]
fn non_unicode_paths_are_rejected_without_lossy_conversion() {
    use std::os::unix::ffi::OsStringExt;

    let path = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/bad-\xff".to_vec()));
    let item = ClipboardItem {
        entries: vec![ClipboardEntry::ExternalPaths(ExternalPaths(
            [path].into_iter().collect(),
        ))],
    };

    assert_eq!(
        terminal_paste_text(&item, RemoteShellDialect::Posix),
        Err(TerminalClipboardError::NonUnicodePath)
    );
}

/// 任意控制字符都可能改变终端粘贴解释，遇到时拒绝整个多路径输入而不发送部分结果。
#[test]
fn control_characters_in_path_reject_the_whole_paste() {
    for control in ['\0', '\t', '\n', '\x03', '\x1b', '\x7f'] {
        let invalid = format!("/tmp/bad{control}name");
        let item = paths(&["/tmp/good", &invalid]);
        assert_eq!(
            terminal_paste_text(&item, RemoteShellDialect::Posix),
            Err(TerminalClipboardError::ControlCharacterInPath),
            "control character {control:?} must reject every path"
        );
    }
}

/// 路径拒绝只约束结构化文件名，普通文本仍交给终端原有粘贴协议处理。
#[test]
fn control_characters_in_plain_text_are_not_rejected_by_path_validation() {
    let text = ClipboardItem::new_string("line\n\x1b[201~\x03".to_owned());
    assert_eq!(
        terminal_paste_text(&text, RemoteShellDialect::Unknown),
        Ok(Some("line\n\x1b[201~\x03".to_owned()))
    );
}

/// 图片或无文本、无文件的剪贴板条目不产生空终端输入。
#[test]
fn empty_or_non_text_items_produce_no_paste() {
    assert_eq!(
        terminal_paste_text(
            &ClipboardItem { entries: vec![] },
            RemoteShellDialect::Unknown
        ),
        Ok(None)
    );
}

/// 构造 GPUI 的文件路径剪贴板条目，保持输入顺序供独立编码测试使用。
fn paths(values: &[&str]) -> ClipboardItem {
    ClipboardItem {
        entries: vec![ClipboardEntry::ExternalPaths(ExternalPaths(
            values.iter().map(PathBuf::from).collect(),
        ))],
    }
}
