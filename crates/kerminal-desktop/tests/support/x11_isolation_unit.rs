//! 插件验收 X11 进程证据的纯解析回归。
//!
//! @author kongweiguang

use super::{ProcessStatus, matches_command, normalize_display, parse_status, server_is_owned};

/// 不接受远程显示、其他屏幕或前后缀混淆，避免把普通桌面识别为私有 Xvfb。
///
/// @author kongweiguang
#[test]
fn local_display_is_normalized_without_accepting_remote_forms() {
    assert_eq!(normalize_display(":99"), Some(":99"));
    assert_eq!(normalize_display(":99.0"), Some(":99"));
    for invalid in [
        "",
        ":",
        ":99.1",
        ":99.00",
        "localhost:99",
        "unix:99",
        ":99x",
        ":-1",
        ":4294967296",
    ] {
        assert_eq!(normalize_display(invalid), None, "{invalid}");
    }
}

/// 参数按 NUL 分隔逐项比较，不能把其他显示或 TCP 选项中的子串误认成有效证据。
///
/// @author kongweiguang
#[test]
fn command_requires_exact_xvfb_display_and_tcp_disable() {
    assert!(matches_command(
        b"/usr/bin/Xvfb\0:99\0-screen\0\x30\0-nolisten\0tcp\0",
        ":99"
    ));
    assert!(matches_command(b"Xvfb\0:99.0\0-nolisten\0tcp\0", ":99"));
    for command in [
        b"/usr/bin/Xvfb\0:990\0-nolisten\0tcp\0".as_slice(),
        b"/usr/bin/not-Xvfb\0:99\0-nolisten\0tcp\0",
        b"/usr/bin/Xvfb\0:99\0-nolisten\0unix\0",
        b"/usr/bin/Xvfb\0:99\0-nolisten\0tcp\0-listen\0tcp\0",
        b"/usr/bin/Xvfb\0:99\0-nolisten\0tcp",
    ] {
        assert!(!matches_command(command, ":99"));
    }
}

/// 完整四项 UID 和单个 PPid 都是归属判断的必要输入，损坏时不能退回默认值。
///
/// @author kongweiguang
#[test]
fn status_requires_unique_parent_and_all_uid_fields() {
    assert_eq!(
        parse_status("Name:\tXvfb\nPPid:\t12\nUid:\t1000\t1000\t1000\t1000\n"),
        Some(ProcessStatus {
            parent_pid: 12,
            uids: [1000; 4]
        })
    );
    assert!(parse_status("PPid: 12\nUid: 1000 1000 1000\n").is_none());
    assert!(parse_status("PPid: 12\nPPid: 13\nUid: 1000 1000 1000 1000\n").is_none());
    assert!(parse_status("PPid: 12\nUid: 1000 1000 1000 1000 1000\n").is_none());
}

/// 即使显示与参数匹配，也不能接受其他用户、PID 1 或祖先链之外的服务。
///
/// @author kongweiguang
#[test]
fn server_must_be_a_same_uid_sibling_of_an_actual_ancestor() {
    let current = ProcessStatus {
        parent_pid: 20,
        uids: [1000; 4],
    };
    let parents = [20, 12, 8];
    assert!(server_is_owned(
        &ProcessStatus {
            parent_pid: 12,
            uids: [1000; 4]
        },
        &current,
        &parents
    ));
    assert!(!server_is_owned(
        &ProcessStatus {
            parent_pid: 1,
            uids: [1000; 4]
        },
        &current,
        &[1, 20]
    ));
    assert!(!server_is_owned(
        &ProcessStatus {
            parent_pid: 21,
            uids: [1000; 4]
        },
        &current,
        &parents
    ));
    assert!(!server_is_owned(
        &ProcessStatus {
            parent_pid: 12,
            uids: [1001; 4]
        },
        &current,
        &parents
    ));
}
