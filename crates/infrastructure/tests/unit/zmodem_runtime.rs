//! 保护同一 SSH 流的协议和 Shell 交接边界。
//!
//! @author kongweiguang

use super::{Stop, ZmodemDirection, active_wire_prefix, exit_prompt};

#[test]
/// 远端 CAN 或 OSC 被拆成多块时，解析器仍保留原字节以便证实退出。
fn active_wire_retains_split_remote_exit() {
    for direction in [ZmodemDirection::Upload, ZmodemDirection::Download] {
        for length in 1..5 {
            let bytes = [b"wire", &[0x18; 5][..length]].concat();
            assert!(matches!(active_wire_prefix(&bytes, direction), Ok(4)));
        }
        assert!(matches!(
            active_wire_prefix(b"\x18\x18\x18\x18\x18\x1b]133;A\x07", direction),
            Err(Stop::PeerAborted)
        ));
    }
    let direction = ZmodemDirection::Upload;
    assert!(matches!(
        active_wire_prefix(b"abc\x1b]133;A\x1b", direction),
        Ok(3)
    ));
    assert!(matches!(
        active_wire_prefix(b"\x1b]133;A\x1b\\", direction),
        Err(Stop::PeerAborted)
    ));
}

#[test]
/// 单个 A 可以是下载内容，退出标记和紧邻的下一次提示符才能组成恢复依据。
fn download_requires_correlated_shell_markers() {
    let direction = ZmodemDirection::Download;
    let data = b"payload\x1b]133;A\x07";
    assert!(matches!(active_wire_prefix(data, direction), Ok(length) if length == data.len()));
    assert_eq!(exit_prompt(data, true), None);
    let prompt = b"\x1b]133;D;128\x07\x1b]133;A\x07";
    let data_then_prompt = [data.as_slice(), prompt.as_slice()].concat();
    assert_eq!(exit_prompt(&data_then_prompt, true), Some(data.len()));
    for length in 1..prompt.len() {
        assert!(matches!(
            active_wire_prefix(&prompt[..length], direction),
            Ok(0)
        ));
    }
    assert!(matches!(
        active_wire_prefix(prompt, direction),
        Err(Stop::PeerAborted)
    ));
}
