// @author kongweiguang
use super::{
    is_nsis_install_directory_argument, nsis_start_process_script, powershell_single_quoted,
    windows_process_argument,
};
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};

/// 验证命令字面量把 apostrophe 转义为 PowerShell 单引号规则，同时保留空格、分号和 Unicode。
#[test]
fn single_quoted_literal_keeps_dynamic_path_as_data() {
    let path = OsStr::new("C:\\Temp\\O'Neil; 会话\\setup.exe");

    assert_eq!(
        powershell_single_quoted(path),
        OsString::from("'C:\\Temp\\O''Neil; 会话\\setup.exe'"),
    );
}

/// 验证无法表示为 UTF-8 的 Windows UTF-16 路径单元不会因命令构造而被替换。
#[test]
fn single_quoted_literal_preserves_unpaired_utf16_units() {
    let raw_path = OsString::from_wide(&[
        b'C' as u16,
        b':' as u16,
        b'\\' as u16,
        0xD800,
        b'\\' as u16,
        b'x' as u16,
    ]);
    let mut expected = vec![b'\'' as u16];
    expected.extend(raw_path.encode_wide());
    expected.push(b'\'' as u16);

    assert_eq!(
        powershell_single_quoted(raw_path.as_os_str())
            .encode_wide()
            .collect::<Vec<_>>(),
        expected,
    );
}

/// 验证 ArgumentList 扁平化后的 Windows 命令行能恢复空格、引号、空参数和尾部反斜杠。
#[test]
fn windows_process_argument_preserves_argument_boundaries() {
    assert_eq!(windows_process_argument("/S"), "/S");
    assert_eq!(
        windows_process_argument("--directory=用户目录 with space"),
        r#""--directory=用户目录 with space""#,
    );
    assert_eq!(
        windows_process_argument(r#"quote"inside"#),
        r#""quote\"inside""#,
    );
    assert_eq!(windows_process_argument(""), r#""""#);
    assert_eq!(
        windows_process_argument(r"C:\folder with space\"),
        r#""C:\folder with space\\""#,
    );
}

/// 验证路径和每个 NSIS 参数均是独立 PowerShell 字面量，特殊字符只作为参数数据。
#[test]
fn nsis_script_quotes_path_and_each_argument() {
    let args = [
        "/S",
        "/R",
        "--directory=用户目录 with space",
        "--name=O'Neil; $env:TEMP",
    ];

    assert_eq!(
        nsis_start_process_script(OsStr::new("C:\\Temp\\setup files\\update.exe"), &args).unwrap(),
        OsString::from(
            r#"Start-Process -FilePath 'C:\Temp\setup files\update.exe' -ArgumentList @('/S', '/R', '"--directory=用户目录 with space"', '"--name=O''Neil; $env:TEMP"')"#,
        ),
    );
}

/// 验证 NSIS 安装目录开关必须保持未加双引号并作为最后一个参数。
#[test]
fn nsis_directory_switch_keeps_native_special_semantics() {
    let args = ["/S", "/R", r"/D=C:\Program Files\Kerminal"];

    assert!(is_nsis_install_directory_argument(args[2]));
    assert_eq!(
        nsis_start_process_script(OsStr::new("C:\\Temp\\update.exe"), &args).unwrap(),
        OsString::from(
            r#"Start-Process -FilePath 'C:\Temp\update.exe' -ArgumentList @('/S', '/R', '/D=C:\Program Files\Kerminal')"#,
        ),
    );
}

/// 验证非法 NSIS 安装目录参数在启动 PowerShell 前被拒绝。
#[test]
fn nsis_directory_switch_rejects_invalid_position_or_quotes() {
    assert!(nsis_start_process_script(
        OsStr::new("C:\\Temp\\update.exe"),
        &["/D=C:\\Kerminal", "/S"],
    )
    .is_err());
    assert!(nsis_start_process_script(
        OsStr::new("C:\\Temp\\update.exe"),
        &[r#"/D=C:\Program " Files"#],
    )
    .is_err());
}

/// 验证无额外安装参数时保持 SDK 的无 `ArgumentList` 调用形式。
#[test]
fn nsis_script_omits_empty_argument_array() {
    assert_eq!(
        nsis_start_process_script(OsStr::new("C:\\Temp\\update.exe"), &[]).unwrap(),
        OsString::from("Start-Process -FilePath 'C:\\Temp\\update.exe'"),
    );
}
