//! Read-only evidence that a hidden acceptance window cannot take focus from the input desktop.
//!
//! @author kongweiguang

use std::io;

use serde::Serialize;

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DesktopIsolationEvidence {
    pub(super) platform_supported: bool,
    pub(super) ui_thread_id: Option<u32>,
    pub(super) ui_thread_desktop_name: Option<String>,
    pub(super) input_desktop_name: Option<String>,
    pub(super) foreground_window_handle: Option<usize>,
    pub(super) foreground_window_pid: Option<u32>,
    pub(super) process_id: Option<u32>,
    /// The raw calling-thread result is retained for diagnostics; null is not itself an isolation failure.
    pub(super) get_active_window_handle: Option<usize>,
    pub(super) isolation_proven: bool,
}

impl DesktopIsolationEvidence {
    /// 可见窗口始终拒绝；只有 Win32 证明线程位于非输入桌面后，才允许隐藏窗口的线程内活动状态。
    ///
    /// @author kongweiguang
    pub(crate) fn validate_window_state(
        &self,
        visible: bool,
        active: bool,
    ) -> Result<(), io::Error> {
        if visible {
            return Err(io::Error::other("右栏验收窗口可见，拒绝继续隐藏窗口采样"));
        }
        if active && !self.isolation_proven {
            return Err(io::Error::other(format!(
                "右栏验收窗口处于活动状态，但未证明其位于独立的非输入桌面：{self:?}",
            )));
        }
        Ok(())
    }
}

/// Samples the calling GPUI UI thread and input desktop without switching desktops or sending window input.
/// The caller must stay on the owning event thread so this snapshot describes the window being checked.
///
/// @author kongweiguang
#[cfg(windows)]
pub(crate) fn capture() -> Result<DesktopIsolationEvidence, io::Error> {
    use windows_sys::Win32::{
        System::{
            StationsAndDesktops::{
                CloseDesktop, DESKTOP_READOBJECTS, GetThreadDesktop, OpenInputDesktop,
            },
            Threading::{GetCurrentProcessId, GetCurrentThreadId},
        },
        UI::{
            Input::KeyboardAndMouse::GetActiveWindow,
            WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId},
        },
    };

    let ui_thread_id = unsafe { GetCurrentThreadId() };
    if ui_thread_id == 0 {
        return Err(last_os_error("GetCurrentThreadId"));
    }
    let ui_desktop = unsafe { GetThreadDesktop(ui_thread_id) };
    if ui_desktop.is_null() {
        return Err(last_os_error("GetThreadDesktop"));
    }
    let ui_thread_desktop_name = desktop_name(ui_desktop)?;

    let input_desktop = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
    if input_desktop.is_null() {
        return Err(last_os_error("OpenInputDesktop"));
    }
    let input_name_result = desktop_name(input_desktop);
    let input_desktop_closed = unsafe { CloseDesktop(input_desktop) } != 0;
    let input_desktop_name = match (input_name_result, input_desktop_closed) {
        (Err(error), _) => return Err(error),
        (Ok(_), false) => return Err(last_os_error("CloseDesktop")),
        (Ok(name), true) => name,
    };

    let foreground_window = unsafe { GetForegroundWindow() };
    let foreground_window_pid = if foreground_window.is_null() {
        None
    } else {
        let mut process_id = 0;
        let foreground_thread_id =
            unsafe { GetWindowThreadProcessId(foreground_window, &mut process_id) };
        if foreground_thread_id == 0 || process_id == 0 {
            return Err(last_os_error("GetWindowThreadProcessId"));
        }
        Some(process_id)
    };

    let process_id = unsafe { GetCurrentProcessId() };
    if process_id == 0 {
        return Err(last_os_error("GetCurrentProcessId"));
    }
    let active_window = unsafe { GetActiveWindow() };
    // GetForegroundWindow 在调用线程所属 desktop 上查询；私有 desktop 上的活动 HWND
    // 不能代表用户输入 desktop 的前台。隔离以 owning thread 与 OpenInputDesktop
    // 的实际名称不同为依据，外层脚本另在 Default desktop 采样并检查本轮进程归属。
    let isolation_proven = ui_thread_desktop_name.starts_with("KerminalRail-")
        && !ui_thread_desktop_name.eq_ignore_ascii_case(&input_desktop_name);

    Ok(DesktopIsolationEvidence {
        platform_supported: true,
        ui_thread_id: Some(ui_thread_id),
        ui_thread_desktop_name: Some(ui_thread_desktop_name),
        input_desktop_name: Some(input_desktop_name),
        foreground_window_handle: (!foreground_window.is_null())
            .then_some(foreground_window as usize),
        foreground_window_pid,
        process_id: Some(process_id),
        get_active_window_handle: (!active_window.is_null()).then_some(active_window as usize),
        isolation_proven,
    })
}

/// 非 Windows 显式返回未证明隔离，缺少平台证据不能被误当成安全放行。
///
/// @author kongweiguang
#[cfg(not(windows))]
pub(crate) fn capture() -> Result<DesktopIsolationEvidence, io::Error> {
    Ok(DesktopIsolationEvidence::default())
}

/// 桌面名称使用有上限的 UTF-16 缓冲区，仅按 Win32 提供的必要长度重试，拒绝损坏或空白结果。
///
/// @author kongweiguang
#[cfg(windows)]
fn desktop_name(
    desktop: windows_sys::Win32::System::StationsAndDesktops::HDESK,
) -> Result<String, io::Error> {
    use std::mem::size_of;

    use windows_sys::Win32::System::StationsAndDesktops::{GetUserObjectInformationW, UOI_NAME};

    const INITIAL_CAPACITY: usize = 128;
    const MAX_CAPACITY: usize = 32_768;

    let mut buffer = vec![0_u16; INITIAL_CAPACITY];
    loop {
        let byte_capacity = buffer
            .len()
            .checked_mul(size_of::<u16>())
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "桌面名称缓冲区尺寸溢出"))?;
        let mut bytes_needed = 0_u32;
        let succeeded = unsafe {
            GetUserObjectInformationW(
                desktop,
                UOI_NAME,
                buffer.as_mut_ptr().cast(),
                byte_capacity,
                &mut bytes_needed,
            )
        } != 0;
        if succeeded {
            let name_len = buffer.iter().position(|unit| *unit == 0).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Win32 桌面名称缺少 UTF-16 结束符",
                )
            })?;
            if name_len == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Win32 返回空桌面名称",
                ));
            }
            return String::from_utf16(&buffer[..name_len]).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "Win32 桌面名称不是有效 UTF-16")
            });
        }

        let error = last_os_error("GetUserObjectInformationW(UOI_NAME)");
        let required_units = usize::try_from(bytes_needed / size_of::<u16>() as u32)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "桌面名称长度溢出"))?;
        if bytes_needed == 0
            || !bytes_needed.is_multiple_of(size_of::<u16>() as u32)
            || required_units <= buffer.len()
            || required_units > MAX_CAPACITY
        {
            return Err(error);
        }
        buffer.resize(required_units, 0);
    }
}

/// 保留原始操作系统错误种类并补上失败 API 名称，防止清理或重试掩盖最初的隔离诊断。
///
/// @author kongweiguang
#[cfg(windows)]
fn last_os_error(operation: &str) -> io::Error {
    let source = io::Error::last_os_error();
    io::Error::new(source.kind(), format!("{operation} 失败：{source}"))
}
