// @author kongweiguang
use super::{macos_elevated_install_command, MACOS_ELEVATED_INSTALL_SCRIPT};
use std::{ffi::OsStr, path::Path};

/// 确认特殊字符路径只作为 `osascript` argv 传递，并由固定脚本执行 shell 引用。
///
/// 该构造测试可在 Windows 运行；它验证参数边界与 AppleScript 引用入口，不声称 macOS 安装已实测。
// @author kongweiguang
#[test]
fn elevated_install_keeps_hostile_path_text_out_of_script() {
    let source = Path::new(
        "/Applications/Kerminal O'Neil \"release\"\n$(touch source-marker); && | < > *.app",
    );
    let staged =
        Path::new("/var/folders/用户/Temporary Items/Kerminal's \"staged\"\n$HOME; && | < > *.app");
    let command = macos_elevated_install_command(source, staged);
    let args = command.get_args().collect::<Vec<_>>();

    assert_eq!(command.get_program(), OsStr::new("osascript"));
    assert_eq!(args.len(), 5);
    assert_eq!(args[0], OsStr::new("-e"));
    assert_eq!(args[1], OsStr::new(MACOS_ELEVATED_INSTALL_SCRIPT));
    assert_eq!(args[2], OsStr::new("--"));
    assert_eq!(args[3], source.as_os_str());
    assert_eq!(args[4], staged.as_os_str());
    assert!(MACOS_ELEVATED_INSTALL_SCRIPT.contains("on run argv"));
    assert!(MACOS_ELEVATED_INSTALL_SCRIPT.contains("quoted form of (item 1 of argv)"));
    assert!(MACOS_ELEVATED_INSTALL_SCRIPT.contains("quoted form of (item 2 of argv)"));
    assert!(MACOS_ELEVATED_INSTALL_SCRIPT.contains("with administrator privileges"));
    assert!(!MACOS_ELEVATED_INSTALL_SCRIPT.contains(source.to_str().unwrap()));
    assert!(!MACOS_ELEVATED_INSTALL_SCRIPT.contains(staged.to_str().unwrap()));
}
