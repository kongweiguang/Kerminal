//! 迁移前对旧版桌面单实例标记进行只读探测。
//!
//! 探测只覆盖调用瞬间可见的旧进程，不能阻止旧版在探测完成后启动；迁移入口仍需要求用户
//! 先退出旧版，并在迁移后不要再使用旧版写入同一数据根。
//!
//! @author kongweiguang

use kerminal_application::{AppError, AppResult};

#[cfg(windows)]
const WINDOWS_MUTEX_NAME: &str = "io.github.kongweiguang.kerminal-sim";
#[cfg(target_os = "linux")]
const LINUX_DBUS_NAME: &str = "io.github.kongweiguang.kerminal.SingleInstance";
#[cfg(target_os = "macos")]
const MACOS_SOCKET_PATH: &str = "/tmp/io_github_kongweiguang_kerminal_si.sock";

const LEGACY_INSTANCE_RUNNING: &str = "旧版 Kerminal 仍在运行，请先退出旧版再迁移";
const MACOS_SOCKET_PRESENT: &str =
    "macOS 旧版单实例 socket 仍存在；请确认旧版已退出并检查残留标记后再迁移";
const LEGACY_INSTANCE_STATUS_UNKNOWN: &str = "无法确认旧版 Kerminal 是否运行，请退出旧版后重试迁移";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProbeStatus {
    Stopped,
    Running,
    Unknown,
}

/// 按当前平台探测旧版单实例标记；只检查不抢占锁，也不访问用户配置内容。
///
/// 调用方只能在确认数据为旧格式且即将执行迁移时调用；此检查与旧版后续启动之间仍有
/// TOCTOU 窗口，因此用户关闭旧版是迁移前提，而不是可由本函数代替的系统排他保证。
///
/// @author kongweiguang
pub(super) fn ensure_legacy_instance_stopped() -> AppResult<()> {
    #[cfg(windows)]
    let status = probe_windows_instance()?;

    #[cfg(target_os = "macos")]
    let status = probe_macos_instance()?;

    #[cfg(target_os = "linux")]
    let status = probe_linux_instance()?;

    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    let status = ProbeStatus::Unknown;

    let context = if cfg!(target_os = "macos") {
        MACOS_SOCKET_PRESENT
    } else {
        LEGACY_INSTANCE_RUNNING
    };
    enforce_probe_status(status, context)
}

/// 将探测结果收敛为可供迁移用例处理的稳定错误类别。
///
/// 冲突要求退出或人工检查，未知状态则直接拒绝迁移；不能把权限错误、总线不可用或平台
/// 未支持解释成“旧版已退出”，否则会把探测失败变成可能覆盖用户数据的伪成功。
///
/// @author kongweiguang
fn enforce_probe_status(status: ProbeStatus, conflict_context: &'static str) -> AppResult<()> {
    match status {
        ProbeStatus::Stopped => Ok(()),
        ProbeStatus::Running => Err(AppError::conflict(conflict_context)),
        ProbeStatus::Unknown => Err(AppError::infrastructure(LEGACY_INSTANCE_STATUS_UNKNOWN)),
    }
}

/// 只把旧版 mutex 明确不存在的系统错误视为已停止，其余结果都要求 fail-closed。
///
/// @author kongweiguang
#[cfg(windows)]
fn classify_windows_open_error(error_code: u32) -> ProbeStatus {
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;

    if error_code == ERROR_FILE_NOT_FOUND {
        ProbeStatus::Stopped
    } else {
        ProbeStatus::Unknown
    }
}

/// 查询旧版命名 mutex 是否仍被某个进程持有；只开关同步句柄，不等待或获取 mutex。
///
/// Windows 命名对象在最后一个句柄关闭后即消失，因而 OpenMutex 成功是活跃对象证据；
/// ACL 拒绝等错误不能证明对象不存在。名称对应旧版 Tauri 插件未启用 semver 的默认值。
///
/// @author kongweiguang
#[cfg(windows)]
fn probe_windows_instance() -> AppResult<ProbeStatus> {
    probe_windows_mutex(WINDOWS_MUTEX_NAME)
}

/// 通过指定的命名 mutex 执行只读探测，独立参数也让测试只创建随机合成对象。
///
/// @author kongweiguang
#[cfg(windows)]
fn probe_windows_mutex(mutex_name: &str) -> AppResult<ProbeStatus> {
    use std::{ffi::OsStr, io, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError},
        System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE},
    };

    let name = OsStr::new(mutex_name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: 名称是固定的 NUL 结尾 UTF-16 常量；仅请求同步权限，不会获取或修改 mutex。
    let handle = unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr()) };
    if handle.is_null() {
        // SAFETY: 立即读取刚才失败的 OpenMutexW 对应错误码。
        let error_code = unsafe { GetLastError() };
        return match classify_windows_open_error(error_code) {
            ProbeStatus::Stopped => Ok(ProbeStatus::Stopped),
            ProbeStatus::Running => Ok(ProbeStatus::Running),
            ProbeStatus::Unknown => Err(AppError::infrastructure_with_source(
                LEGACY_INSTANCE_STATUS_UNKNOWN,
                io::Error::from_raw_os_error(error_code as i32),
            )),
        };
    }

    // SAFETY: 句柄由本函数 OpenMutexW 成功返回且仅在此处关闭；检测到对象本身即要求迁移停止。
    if unsafe { CloseHandle(handle) } == 0 {
        return Err(AppError::infrastructure_with_source(
            LEGACY_INSTANCE_STATUS_UNKNOWN,
            io::Error::last_os_error(),
        ));
    }
    Ok(ProbeStatus::Running)
}

/// 只有不存在旧版 socket 路径才视为未发现实例；存在时不连接、不删除，也不触发旧版回调。
///
/// 该路径可能是异常退出留下的 socket，因此冲突提示要求人工检查；本工具不会据此擅自清理。
///
/// @author kongweiguang
#[cfg(target_os = "macos")]
fn probe_macos_instance() -> AppResult<ProbeStatus> {
    use std::fs;

    match fs::symlink_metadata(MACOS_SOCKET_PATH) {
        Ok(_) => Ok(ProbeStatus::Running),
        Err(error) if classify_macos_socket_error(error.kind()) == ProbeStatus::Stopped => {
            Ok(ProbeStatus::Stopped)
        }
        Err(error) => Err(AppError::infrastructure_with_source(
            LEGACY_INSTANCE_STATUS_UNKNOWN,
            error,
        )),
    }
}

/// 将 socket 路径元数据查询错误映射为保守状态，测试可用合成错误验证而不触碰真实 socket。
///
/// @author kongweiguang
#[cfg(any(test, target_os = "macos"))]
fn classify_macos_socket_error(error_kind: std::io::ErrorKind) -> ProbeStatus {
    if error_kind == std::io::ErrorKind::NotFound {
        ProbeStatus::Stopped
    } else {
        ProbeStatus::Unknown
    }
}

/// 通过当前登录 session bus 的 NameHasOwner 查询旧版 well-known name。
///
/// 查询失败不能解释为旧版退出；D-Bus 身份与旧插件使用的 application identifier 完全一致。
/// 该检查只覆盖当前 session bus，并不封锁其它 session 中启动的旧版。
///
/// @author kongweiguang
#[cfg(target_os = "linux")]
fn probe_linux_instance() -> AppResult<ProbeStatus> {
    use zbus::{blocking::Connection, blocking::fdo::DBusProxy, names::BusName};

    let connection = Connection::session().map_err(|error| {
        AppError::infrastructure_with_source(LEGACY_INSTANCE_STATUS_UNKNOWN, error)
    })?;
    let proxy = DBusProxy::new(&connection).map_err(|error| {
        AppError::infrastructure_with_source(LEGACY_INSTANCE_STATUS_UNKNOWN, error)
    })?;
    let name = BusName::try_from(LINUX_DBUS_NAME).map_err(|error| {
        AppError::infrastructure_with_source(LEGACY_INSTANCE_STATUS_UNKNOWN, error)
    })?;
    let has_owner = proxy.name_has_owner(name).map_err(|error| {
        AppError::infrastructure_with_source(LEGACY_INSTANCE_STATUS_UNKNOWN, error)
    })?;

    Ok(classify_linux_name_owner(has_owner))
}

/// 将总线对旧版名称持有状态的布尔回答转为平台无关结果，避免错误路径误报已停止。
///
/// @author kongweiguang
#[cfg(any(test, target_os = "linux"))]
fn classify_linux_name_owner(has_owner: bool) -> ProbeStatus {
    if has_owner {
        ProbeStatus::Running
    } else {
        ProbeStatus::Stopped
    }
}

#[cfg(test)]
#[path = "../../tests/support/data_migration_offline_guard_unit.rs"]
mod unit;
