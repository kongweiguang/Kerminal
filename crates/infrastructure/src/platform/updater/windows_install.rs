//! Windows 当前用户 NSIS 安装身份检查，只读取注册信息。
//!
//! @author kongweiguang

use std::{
    ffi::OsString,
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    ptr,
};

use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};

/// 只有当前 EXE 与 NSIS 登记目录一致时才允许安装器改写该用户的 Kerminal。
pub(super) fn is_current_install() -> bool {
    let Some(directory) = install_directory() else {
        return false;
    };
    let Ok(executable) = std::env::current_exe() else {
        return false;
    };
    installed_binary_matches(&executable, &directory)
}

/// 从 HKCU 的产品卸载项读取有界 REG_SZ，不根据外部环境变量猜测安装位置。
fn install_directory() -> Option<PathBuf> {
    let key: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Kerminal\0"
        .encode_utf16()
        .collect();
    let name: Vec<u16> = "InstallLocation\0".encode_utf16().collect();
    let mut byte_length = 0u32;
    // SAFETY: 所有指针指向以 NUL 结尾的 UTF-16 文本；首次调用只查询目标所需大小。
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut byte_length,
        )
    };
    if status != 0 || byte_length == 0 || byte_length > 64 * 1024 || !byte_length.is_multiple_of(2)
    {
        return None;
    }
    let mut data = vec![0u16; (byte_length / 2) as usize];
    // SAFETY: data 的长度由第一次调用返回的字节数确定；第二次调用受同一上限约束。
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut byte_length,
        )
    };
    if status != 0 || byte_length == 0 || byte_length as usize > data.len() * 2 {
        return None;
    }
    decode_install_directory(&data)
}

/// NSIS 将安装目录写成带引号的 REG_SZ；拒绝损坏的单侧引号和空值。
fn decode_install_directory(data: &[u16]) -> Option<PathBuf> {
    let end = data
        .iter()
        .position(|item| *item == 0)
        .unwrap_or(data.len());
    let mut value = &data[..end];
    if value.first() == Some(&u16::from(b'"')) || value.last() == Some(&u16::from(b'"')) {
        if value.len() < 2
            || value.first() != Some(&u16::from(b'"'))
            || value.last() != Some(&u16::from(b'"'))
        {
            return None;
        }
        value = &value[1..value.len() - 1];
    }
    (!value.is_empty()).then(|| PathBuf::from(OsString::from_wide(value)))
}

/// 通过文件系统解析比较真实目录，防止相同文件名的开发程序借用已安装产品的注册项。
fn installed_binary_matches(executable: &Path, registered_directory: &Path) -> bool {
    if !executable
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("kerminal.exe"))
    {
        return false;
    }
    let Some(parent) = executable.parent() else {
        return false;
    };
    let Ok(actual_directory) = std::fs::canonicalize(parent) else {
        return false;
    };
    let Ok(expected_directory) = std::fs::canonicalize(registered_directory) else {
        return false;
    };
    actual_directory == expected_directory
}

#[cfg(test)]
#[path = "../../../tests/support/updater_windows_install_unit.rs"]
mod tests;
