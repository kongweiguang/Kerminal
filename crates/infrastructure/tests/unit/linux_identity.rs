//! @author kongweiguang

use super::parse_linux_identity_output;

/// 固定字段键名使 machine 与 release 保持独立，空值仍为空而不会借用相邻字段。
///
/// @author kongweiguang
#[test]
fn linux_identity_output_keeps_machine_and_release_distinct() {
    assert_eq!(
        parse_linux_identity_output(
            "os=Linux\n\narchitecture=x86_64\n\nkernel=5.15.167.4-microsoft-standard-WSL2\n"
        ),
        (
            Some("Linux".to_owned()),
            Some("x86_64".to_owned()),
            Some("5.15.167.4-microsoft-standard-WSL2".to_owned()),
        )
    );
    assert_eq!(
        parse_linux_identity_output("os=Linux\n\narchitecture=\n\nkernel=5.15.0\n"),
        (Some("Linux".to_owned()), None, Some("5.15.0".to_owned()))
    );
}
