//! Windows-only DPAPI and owner-only file creation for MSTSC.
//!
//! @author kongweiguang

use std::{
    ffi::OsStr,
    fs::File,
    io::Write,
    os::windows::{
        ffi::OsStrExt,
        io::{FromRawHandle, RawHandle},
    },
    path::Path,
    ptr::{null, null_mut},
};

use super::artifact::finish_partial_file_write;
use kerminal_application::{AppError, AppResult};
use windows_sys::Win32::{
    Foundation::{GENERIC_WRITE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree},
    Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    },
    Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData},
    Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES},
    Storage::FileSystem::{CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_TEMPORARY, FILE_SHARE_NONE},
};
use zeroize::Zeroizing;

/// 以 user-scope DPAPI 生成 MSTSC 可读的 `password 51:b` 十六进制密文。
pub(super) fn protect_rdp_password(password: &str) -> AppResult<String> {
    if password.contains('\0') {
        return Err(AppError::invalid("RDP 密码包含客户端无法表达的空字符"));
    }
    let mut plaintext = Zeroizing::new(Vec::with_capacity(password.len().saturating_mul(2) + 2));
    for unit in password.encode_utf16() {
        plaintext.extend_from_slice(&unit.to_le_bytes());
    }
    plaintext.extend_from_slice(&[0, 0]);
    let input_length = u32::try_from(plaintext.len())
        .map_err(|_| AppError::invalid("RDP 密码长度超出系统加密接口限制"))?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: input_length,
        pbData: plaintext.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input points to a live UTF-16LE buffer; optional entropy, prompt and reserved pointers are null by contract.
    let succeeded = unsafe {
        CryptProtectData(
            &input,
            null(),
            null(),
            null(),
            null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if succeeded == 0 {
        return Err(AppError::infrastructure_with_source(
            "Windows DPAPI 无法保护 RDP 密码",
            std::io::Error::last_os_error(),
        ));
    }
    let _allocation = LocalAllocation(output.pbData.cast());
    if output.pbData.is_null() || output.cbData == 0 {
        return Err(AppError::infrastructure("Windows DPAPI 返回空 RDP 密文"));
    }
    // SAFETY: CryptProtectData allocated cbData bytes at pbData; LocalAllocation frees them after hex encoding.
    let encrypted = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
    Ok(hex_lower(encrypted))
}

/// 创建时直接指定仅 owner 可访问的 protected DACL，避免先写秘密再收紧继承权限。
///
/// @author kongweiguang
pub(super) fn write_owner_only_file(path: &Path, contents: &[u8]) -> AppResult<()> {
    let sddl = OsStr::new("D:P(A;;FA;;;OW)")
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: sddl is NUL-terminated and descriptor is a valid output pointer for the allocated security descriptor.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    };
    if converted == 0 {
        return Err(AppError::infrastructure_with_source(
            "无法构造 RDP 临时文件访问控制列表",
            std::io::Error::last_os_error(),
        ));
    }
    let descriptor_guard = LocalAllocation(descriptor);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor_guard.0,
        bInheritHandle: 0,
    };
    let wide_path = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: path and SECURITY_ATTRIBUTES remain alive for CreateFileW; CREATE_NEW prevents overwriting another file.
    let handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_NONE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_TEMPORARY,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(AppError::infrastructure_with_source(
            "无法创建受限 RDP 临时文件",
            std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: CreateFileW returned an owned valid handle, transferred exactly once into File.
    let mut file = unsafe { File::from_raw_handle(handle as RawHandle) };
    let result = file.write_all(contents).and_then(|()| file.flush());
    drop(file);
    finish_partial_file_write(path, result)
}

/// 把 DPAPI 字节转换成 RDP 文件要求的纯十六进制，不产生额外秘密副本。
fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

/// 使用系统分配器的释放函数回收 DPAPI 或 security descriptor 输出内存。
struct LocalAllocation(*mut std::ffi::c_void);

impl Drop for LocalAllocation {
    /// 与 CryptProtectData/ConvertStringSecurityDescriptor 的分配器配对释放原生内存。
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: this wrapper is constructed only from LocalAlloc-compatible Win32 API outputs and frees once.
            unsafe {
                LocalFree(self.0 as HLOCAL);
            }
        }
    }
}
