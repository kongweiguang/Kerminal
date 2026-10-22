//! `kerminal://` 用户级协议关联适配器；协议注册只允许设置界面的显式操作触发。  
//!
//! @author kongweiguang

use kerminal_application::{
    AppError, AppResult,
    platform::{ExternalProtocolPort, ExternalProtocolStatus},
};

/// 当前进程的 Windows 用户级协议关联实现；macOS/Linux 保留统一接口并明确报告不支持。
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemExternalProtocolAdapter;

impl ExternalProtocolPort for SystemExternalProtocolAdapter {
    /// 只回读当前用户的协议状态；平台分支直接提供状态，不为不支持平台猜测回退。
    ///
    /// @author kongweiguang
    fn status(&self) -> AppResult<ExternalProtocolStatus> {
        #[cfg(windows)]
        {
            windows_registry::status()
        }
        #[cfg(not(windows))]
        {
            Ok(ExternalProtocolStatus {
                supported: false,
                registered: false,
            })
        }
    }

    /// 只响应用户主动注册且保留冲突保护；非 Windows 平台仍显式返回不支持。
    ///
    /// @author kongweiguang
    fn register_for_current_user(&self) -> AppResult<ExternalProtocolStatus> {
        #[cfg(windows)]
        {
            windows_registry::register_for_current_user()
        }
        #[cfg(not(windows))]
        {
            Err(AppError::unsupported(
                "kerminal:// 协议动态注册当前仅在 Windows 上支持",
            ))
        }
    }

    /// 只清理指向当前可执行文件的用户关联；其他平台继续显式报告不支持。
    ///
    /// @author kongweiguang
    fn unregister_for_current_user(&self) -> AppResult<ExternalProtocolStatus> {
        #[cfg(windows)]
        {
            windows_registry::unregister_for_current_user()
        }
        #[cfg(not(windows))]
        {
            Err(AppError::unsupported(
                "kerminal:// 协议动态注销当前仅在 Windows 上支持",
            ))
        }
    }
}

#[cfg(any(windows, test))]
/// 按 Windows 引号规则生成 ShellExecute 命令；路径和 URI 参数均保留为独立参数。
/// Windows 注册适配器在生产中使用；test 构建保留跨平台纯字符串回归覆盖。
fn protocol_command(executable: &str) -> AppResult<String> {
    if executable.trim().is_empty() || executable.contains('"') || executable.contains('\0') {
        return Err(AppError::invalid("Kerminal 可执行文件路径无效"));
    }
    Ok(format!("\"{executable}\" \"%1\""))
}

#[cfg(any(windows, test))]
/// 从已注册的 Windows command string 提取首个完整引用路径，非本程序或不安全格式均拒绝。
/// 非 Windows 生产构建不编译该解析器，test 构建仍验证其边界规则。
fn command_executable(command: &str) -> Option<&str> {
    let mut chars = command.trim_start().char_indices();
    if chars.next()?.1 != '"' {
        return None;
    }
    let end = chars.find_map(|(index, character)| (character == '"').then_some(index))?;
    let executable = &command.trim_start()[1..end];
    (!executable.is_empty()).then_some(executable)
}

#[cfg(any(windows, test))]
/// Windows 路径比较忽略大小写和分隔符差异，但不接受可执行文件后缀的模糊匹配。
/// 所有权比较由 Windows 注册流程调用，纯函数测试在非 Windows 上仍可运行。
fn command_targets_executable(command: &str, executable: &str) -> bool {
    let Some(command_executable) = command_executable(command) else {
        return false;
    };
    normalize_windows_path(command_executable) == normalize_windows_path(executable)
}

#[cfg(any(windows, test))]
/// 统一普通路径和 Win32 扩展路径前缀，只保留完整路径语义用于所有权比较。
/// 该比较只服务 Windows handler 所有权判定；test 构建保留跨平台校验。
fn normalize_windows_path(path: &str) -> String {
    let normalized = path.trim().replace('/', "\\");
    let normalized = normalized.strip_prefix("\\\\?\\").unwrap_or(&normalized);
    normalized.trim_end_matches('\\').to_ascii_lowercase()
}

#[cfg(windows)]
mod windows_registry {
    use std::{io, path::PathBuf, ptr};

    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS},
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_CREATE_SUB_KEY, KEY_QUERY_VALUE, KEY_SET_VALUE,
            REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW,
            RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
        },
    };

    use super::*;

    const PROTOCOL_KEY: &str = "Software\\Classes\\kerminal";
    const COMMAND_KEY: &str = "Software\\Classes\\kerminal\\shell\\open\\command";
    const MAX_REGISTRY_STRING_BYTES: u32 = 32 * 1024;

    /// RAII 关闭打开的注册表键，错误返回和正常路径都不泄漏系统句柄。
    struct RegistryKey(HKEY);

    impl Drop for RegistryKey {
        /// Windows 注册表键为系统句柄，集中在析构时关闭而不依赖每个调用分支。
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }

    /// 仅使用当前用户注册表，不要求管理员权限，也不写 HKLM 系统级协议关联。
    pub(super) fn status() -> AppResult<ExternalProtocolStatus> {
        let executable = current_executable()?;
        let command = read_default_string(COMMAND_KEY)?;
        let marker = read_named_string(PROTOCOL_KEY, "URL Protocol")?;
        Ok(ExternalProtocolStatus {
            supported: true,
            registered: command.as_deref().is_some_and(|value| {
                command_targets_executable(value, &executable.to_string_lossy())
            }) && marker.is_some(),
        })
    }

    /// 显式创建 HKCU 协议键和命令键；现有关联若属于其他程序则拒绝覆盖。
    pub(super) fn register_for_current_user() -> AppResult<ExternalProtocolStatus> {
        let executable = current_executable()?;
        let command = protocol_command(&executable.to_string_lossy())?;
        if let Some(existing) = read_default_string(COMMAND_KEY)?
            && !command_targets_executable(&existing, &executable.to_string_lossy())
        {
            return Err(AppError::conflict(
                "kerminal:// 已由其他程序处理，未覆盖原有设置",
            ));
        }

        // 命令值作为 handler 的提交标记，最后写入，避免前两步失败时留下可调用的半注册状态。
        write_string(PROTOCOL_KEY, Some("URL Protocol"), "")?;
        write_string(PROTOCOL_KEY, None, "URL:Kerminal Protocol")?;
        write_string(COMMAND_KEY, None, &command)?;
        status()
    }

    /// 用户显式注销时复核命令所有权，再删除 Kerminal 命令键和协议标记值。
    pub(super) fn unregister_for_current_user() -> AppResult<ExternalProtocolStatus> {
        let executable = current_executable()?;
        let current_command = read_default_string(COMMAND_KEY)?;
        let Some(current_command) = current_command else {
            return status();
        };
        if !command_targets_executable(&current_command, &executable.to_string_lossy()) {
            return Err(AppError::conflict(
                "kerminal:// 已由其他程序处理，未移除原有设置",
            ));
        }

        let command_key = wide_null(COMMAND_KEY);
        let result = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, command_key.as_ptr()) };
        if result != ERROR_SUCCESS && result != ERROR_FILE_NOT_FOUND {
            return Err(registry_error(result));
        }
        delete_value_if_present(PROTOCOL_KEY, "URL Protocol")?;
        delete_value_if_present(PROTOCOL_KEY, "")?;
        status()
    }

    /// 返回当前程序的实际可执行文件路径；无法读取时中止而不猜测安装位置。
    fn current_executable() -> AppResult<PathBuf> {
        std::env::current_exe().map_err(|error| {
            AppError::infrastructure_with_source("无法确定 Kerminal 可执行文件", error)
        })
    }

    /// 打开现有注册表键读取；键不存在与其他注册表故障使用不同结果。
    fn open_key(path: &str, access: u32) -> AppResult<Option<RegistryKey>> {
        let path = wide_null(path);
        let mut key = ptr::null_mut();
        let result =
            unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, access, &mut key) };
        match result {
            ERROR_SUCCESS => Ok(Some(RegistryKey(key))),
            ERROR_FILE_NOT_FOUND => Ok(None),
            other => Err(registry_error(other)),
        }
    }

    /// 查询注册表字符串后校验边界，并按固定 UTF-16 单元配对解码；不完整尾部仍按原逻辑忽略。
    ///
    /// @author kongweiguang
    fn read_named_string(path: &str, name: &str) -> AppResult<Option<String>> {
        let Some(key) = open_key(path, KEY_QUERY_VALUE)? else {
            return Ok(None);
        };
        let name = wide_null(name);
        let mut value_type = 0;
        let mut byte_count = 0;
        let result = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                ptr::null(),
                &mut value_type,
                ptr::null_mut(),
                &mut byte_count,
            )
        };
        if result == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if result != ERROR_SUCCESS && result != ERROR_MORE_DATA {
            return Err(registry_error(result));
        }
        if byte_count > MAX_REGISTRY_STRING_BYTES || byte_count % 2 != 0 {
            return Err(AppError::configuration_corrupt("Windows 协议注册信息"));
        }

        let mut bytes = vec![0_u8; byte_count as usize];
        let result = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                ptr::null(),
                &mut value_type,
                bytes.as_mut_ptr(),
                &mut byte_count,
            )
        };
        if result == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if result != ERROR_SUCCESS {
            return Err(registry_error(result));
        }
        if (value_type != REG_SZ && value_type != REG_EXPAND_SZ) || byte_count > bytes.len() as u32
        {
            return Err(AppError::configuration_corrupt("Windows 协议注册信息"));
        }

        let wide = bytes[..byte_count as usize]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_ne_bytes(*pair))
            .take_while(|unit| *unit != 0)
            .collect::<Vec<_>>();
        String::from_utf16(&wide)
            .map(Some)
            .map_err(|_| AppError::configuration_corrupt("Windows 协议注册信息"))
    }

    /// 读取默认字符串，和具名值共用同一安全校验逻辑。
    fn read_default_string(path: &str) -> AppResult<Option<String>> {
        read_named_string(path, "")
    }

    /// 以 REG_SZ 写入用户级协议字段；父键和子键按需创建，不改动 machine hive。
    fn write_string(path: &str, name: Option<&str>, value: &str) -> AppResult<()> {
        let path = wide_null(path);
        let mut key = ptr::null_mut();
        let mut _disposition = 0;
        let result = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_CREATE_SUB_KEY,
                ptr::null(),
                &mut key,
                &mut _disposition,
            )
        };
        if result != ERROR_SUCCESS {
            return Err(registry_error(result));
        }
        let key = RegistryKey(key);
        let value_name = name.map(wide_null);
        let value_name_ptr = value_name
            .as_ref()
            .map_or(ptr::null(), |name| name.as_ptr());
        let wide_value = value
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let byte_count = u32::try_from(wide_value.len() * 2)
            .map_err(|_| AppError::invalid("Windows 协议注册值过长"))?;
        let result = unsafe {
            RegSetValueExW(
                key.0,
                value_name_ptr,
                0,
                REG_SZ,
                wide_value.as_ptr().cast(),
                byte_count,
            )
        };
        if result != ERROR_SUCCESS {
            return Err(registry_error(result));
        }
        Ok(())
    }

    /// 删除单个关联值；缺失值视为幂等成功，其他错误保留 Windows 原因供内部诊断。
    fn delete_value_if_present(path: &str, name: &str) -> AppResult<()> {
        let Some(key) = open_key(path, KEY_SET_VALUE)? else {
            return Ok(());
        };
        let name = wide_null(name);
        let result = unsafe { RegDeleteValueW(key.0, name.as_ptr()) };
        if result != ERROR_SUCCESS && result != ERROR_FILE_NOT_FOUND {
            return Err(registry_error(result));
        }
        Ok(())
    }

    /// 把 Win32 错误码保留为内部 source，公开错误仍只返回固定脱敏文案。
    fn registry_error(code: u32) -> AppError {
        AppError::infrastructure_with_source(
            "Windows 协议设置访问失败",
            io::Error::from_raw_os_error(code as i32),
        )
    }

    /// Windows API 需要 NUL 结尾 UTF-16；内部常量和路径已在调用点拒绝嵌入 NUL。
    fn wide_null(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(test)]
#[path = "../../tests/support/platform_external_launch_unit.rs"]
mod tests;
