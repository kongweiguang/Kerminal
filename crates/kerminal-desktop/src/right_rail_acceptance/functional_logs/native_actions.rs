//! 隔离日志验收的窗口内输入与可还原剪贴板探针。
//!
//! @author kongweiguang

use std::{error::Error, io};

use gpui_kit::{
    AnyWindowHandle, App, AsyncApp, ClipboardEntry, ClipboardItem, InputEvent as _, Window,
};
use gpui_pre::{MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent};

/// 仅对可还原的文本或空剪贴板执行真实复制，并在同一个 UI 回调内回读和还原。
/// 无法判断原生格式时返回 false，保留当前剪贴板并明确记录此项未验收。
///
/// @author kongweiguang
pub(super) fn verify_copy_with_clipboard_restore(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    entry_id: &str,
    expected: &str,
) -> Result<bool, Box<dyn Error>> {
    let selector = format!("rr-logs-copy-{entry_id}");
    let verified = window.update(cx, |_, window, app| {
        let Some((originally_empty, original_sequence)) = clipboard_restore_preflight()? else {
            return Ok(false);
        };
        let original = app.read_from_clipboard();
        match (&original, originally_empty) {
            (None, true) => {}
            (Some(item), false)
                if item.metadata().is_none()
                    && item.entries().len() == 1
                    && matches!(&item.entries()[0], ClipboardEntry::String(_)) => {}
            _ => return Ok(false),
        }
        if clipboard_sequence()? != original_sequence {
            return Ok(false);
        }
        click_current_frame_selector(window, app, &selector)?;
        let copy_result = observe_copy(app, expected, original_sequence);
        let confirmed_sequence = copy_result.as_ref().ok().copied();
        let cleanup_result = if copy_result
            .as_ref()
            .err()
            .is_some_and(|error| error.kind() == io::ErrorKind::Interrupted)
        {
            Err(io::Error::other("已检测到并发更新，保留当前剪贴板"))
        } else {
            restore_after_copy(
                app,
                original.as_ref(),
                expected,
                original_sequence,
                confirmed_sequence,
            )
        };
        match (copy_result, cleanup_result) {
            (Ok(_), Ok(())) => Ok(true),
            (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(cleanup_error)) => Err(io::Error::other(format!(
                "{error}；剪贴板收尾未确认：{cleanup_error}"
            ))),
        }
    })??;
    Ok(verified)
}

/// 仅在序号稳定且确实发生变化时确认本轮写入，读回失败不跳过后续清理。
///
/// @author kongweiguang
fn observe_copy(app: &App, expected: &str, original_sequence: u32) -> Result<u32, io::Error> {
    let before = clipboard_sequence()?;
    let observed = app.read_from_clipboard();
    let after = clipboard_sequence()?;
    if before != after {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "复制读回期间剪贴板发生并发更新",
        ));
    }
    if after == original_sequence
        || observed.as_ref().and_then(ClipboardItem::text).as_deref() != Some(expected)
    {
        return Err(io::Error::other("历史复制按钮未写入预期的本轮命令"));
    }
    Ok(after)
}

/// 无论读回成功或失败都尝试收尾；只恢复序号稳定且内容属于本轮的唯一种子。
///
/// @author kongweiguang
fn restore_after_copy(
    app: &App,
    original: Option<&ClipboardItem>,
    expected: &str,
    original_sequence: u32,
    confirmed_sequence: Option<u32>,
) -> Result<(), io::Error> {
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let Ok(before) = clipboard_sequence() else {
            continue;
        };
        if confirmed_sequence.is_some_and(|sequence| sequence != before) {
            return Err(io::Error::other("其他程序已更新剪贴板，保留其内容"));
        }
        let observed = app.read_from_clipboard();
        let Ok(after) = clipboard_sequence() else {
            continue;
        };
        if before != after {
            return Err(io::Error::other("收尾读取期间剪贴板已更新，保留其内容"));
        }
        if observed.as_ref() == original && (original.is_some() || after == original_sequence) {
            return Ok(());
        }
        if observed.as_ref().and_then(ClipboardItem::text).as_deref() == Some(expected)
            && after != original_sequence
        {
            match restore_clipboard_if_unchanged(after, original) {
                Ok(true) => {
                    let restored = app.read_from_clipboard();
                    if original.is_some() && restored.as_ref() == original {
                        return Ok(());
                    }
                    if original.is_none()
                        && matches!(clipboard_restore_preflight(), Ok(Some((true, _))))
                    {
                        return Ok(());
                    }
                    return Err(io::Error::other("无法确认原剪贴板已恢复"));
                }
                Ok(false) => {
                    return Err(io::Error::other("恢复前剪贴板已更新，保留其内容"));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) => return Err(error),
            }
        }
        if observed.is_some() {
            return Err(io::Error::other("剪贴板内容不属于本轮，保留其内容"));
        }
        if let Ok(Some((true, empty_sequence))) = clipboard_restore_preflight()
            && empty_sequence == after
        {
            return Err(io::Error::other("剪贴板已被清空，保留其当前状态"));
        }
    }
    Err(io::Error::other("无法确认剪贴板当前内容，未覆盖未知更新"))
}

/// Windows 在剪贴板锁内核对序号并恢复原文本或空状态，缩小检查与写入之间的竞态。
///
/// @author kongweiguang
#[cfg(windows)]
fn restore_clipboard_if_unchanged(
    expected_sequence: u32,
    original: Option<&ClipboardItem>,
) -> Result<bool, io::Error> {
    use windows_sys::Win32::{
        Foundation::GlobalFree,
        System::{
            DataExchange::{
                CloseClipboard, EmptyClipboard, GetClipboardSequenceNumber, OpenClipboard,
                SetClipboardData,
            },
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
        },
        UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE},
    };

    let mut memory = std::ptr::null_mut();
    if let Some(item) = original {
        let [ClipboardEntry::String(text)] = item.entries() else {
            return Err(io::Error::other("原剪贴板不是可恢复的单项纯文本"));
        };
        let wide: Vec<u16> = text.text.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: 为 Win32 剪贴板准备可移动全局内存；写入失败时由本函数释放。
        memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of_val(wide.as_slice())) };
        if memory.is_null() {
            return Err(io::Error::other("无法分配剪贴板恢复内存"));
        }
        let destination = unsafe { GlobalLock(memory) }.cast::<u16>();
        if destination.is_null() {
            unsafe { GlobalFree(memory) };
            return Err(io::Error::other("无法锁定剪贴板恢复内存"));
        }
        // SAFETY: destination 指向刚申请的同等大小内存，wide 至少含终止符。
        unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), destination, wide.len()) };
        unsafe { GlobalUnlock(memory) };
    }

    // 空 owner 在 EmptyClipboard 后不能可靠设置数据，使用私有桌面的消息窗口作为 owner。
    let class = [83_u16, 84, 65, 84, 73, 67, 0];
    // SAFETY: STATIC 是系统内置类；消息窗口不可见，且在本线程完成后销毁。
    let owner = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    if owner.is_null() {
        if !memory.is_null() {
            unsafe { GlobalFree(memory) };
        }
        return Err(io::Error::other("无法创建剪贴板恢复所需的后台窗口"));
    }
    // SAFETY: 打开后在持锁期间比较序号，只有同一剪贴板内容才允许替换。
    if unsafe { OpenClipboard(owner) } == 0 {
        unsafe { DestroyWindow(owner) };
        if !memory.is_null() {
            unsafe { GlobalFree(memory) };
        }
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "剪贴板正被其他程序使用",
        ));
    }
    let unchanged = unsafe { GetClipboardSequenceNumber() } == expected_sequence;
    let mut result = Ok(false);
    if unchanged {
        if unsafe { EmptyClipboard() } == 0 {
            result = Err(io::Error::other("无法清空本轮测试剪贴板"));
        } else if !memory.is_null() {
            // CF_UNICODETEXT 为 13；成功后由系统接管内存句柄。
            if unsafe { SetClipboardData(13, memory) }.is_null() {
                result = Err(io::Error::other("无法恢复原剪贴板文本"));
            } else {
                memory = std::ptr::null_mut();
                result = Ok(true);
            }
        } else {
            result = Ok(true);
        }
    }
    let closed = unsafe { CloseClipboard() };
    let destroyed = unsafe { DestroyWindow(owner) };
    if !memory.is_null() {
        unsafe { GlobalFree(memory) };
    }
    if closed == 0 {
        return Err(io::Error::other("无法关闭剪贴板恢复句柄"));
    }
    if destroyed == 0 {
        return Err(io::Error::other("无法释放剪贴板恢复后台窗口"));
    }
    result
}

/// 非 Windows 不执行复制探针，保留同一调用签名以维持跨平台构建。
///
/// @author kongweiguang
#[cfg(not(windows))]
fn restore_clipboard_if_unchanged(
    _expected_sequence: u32,
    _original: Option<&ClipboardItem>,
) -> Result<bool, io::Error> {
    Err(io::Error::other("当前平台无法安全恢复剪贴板"))
}

/// 在当前绘制帧中解析实际按钮边界，仅向验收窗口分发一次完整鼠标点击。
///
/// @author kongweiguang
fn click_current_frame_selector(
    window: &mut Window,
    app: &mut App,
    selector: &str,
) -> Result<(), io::Error> {
    let bounds = window
        .debug_selector_bounds(selector)
        .ok_or_else(|| io::Error::other(format!("缺少日志按钮 selector {selector:?}")))?;
    let viewport = window.visual_viewport_bounds();
    let rounding = gpui_pre::px(0.01);
    if bounds.is_empty()
        || bounds.origin.x < viewport.origin.x - rounding
        || bounds.origin.y < viewport.origin.y - rounding
        || bounds.origin.x + bounds.size.width > viewport.origin.x + viewport.size.width + rounding
        || bounds.origin.y + bounds.size.height
            > viewport.origin.y + viewport.size.height + rounding
    {
        return Err(io::Error::other("日志按钮位于窗口可视范围之外"));
    }
    let position = bounds.center();
    for event in [
        MouseMoveEvent {
            position,
            ..MouseMoveEvent::default()
        }
        .to_platform_input(),
        MouseDownEvent {
            button: MouseButton::Left,
            position,
            click_count: 1,
            ..MouseDownEvent::default()
        }
        .to_platform_input(),
        MouseUpEvent {
            button: MouseButton::Left,
            position,
            click_count: 1,
            ..MouseUpEvent::default()
        }
        .to_platform_input(),
    ] {
        let _ = window.dispatch_event(event, app);
    }
    Ok(())
}

/// 将窗口句柄转换为内部事件目标，避免向用户桌面注入系统鼠标。
///
/// @author kongweiguang
pub(super) fn click_selector(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        click_current_frame_selector(window, app, selector)
    })??;
    Ok(())
}

/// Windows 仅允许空或系统纯文本格式，避免覆盖无法还原的富文本、文件或私有格式。
/// 返回剪贴板序号用于检测预检与点击之间的并发修改。
///
/// @author kongweiguang
#[cfg(windows)]
fn clipboard_restore_preflight() -> Result<Option<(bool, u32)>, io::Error> {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, CountClipboardFormats, EnumClipboardFormats, GetClipboardSequenceNumber,
        OpenClipboard,
    };

    // SAFETY: 空 owner 表示只打开当前线程所在 window station 的剪贴板，成功后始终关闭。
    if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
        return Ok(None);
    }
    let count = unsafe { CountClipboardFormats() };
    let mut format = 0;
    let mut plain_text = true;
    let mut unicode_text = false;
    for _ in 0..count {
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            plain_text = false;
            break;
        }
        // CF_TEXT、CF_OEMTEXT、CF_UNICODETEXT 与 CF_LOCALE 是 Windows 的标准文本族。
        if !matches!(format, 1 | 7 | 13 | 16) {
            plain_text = false;
        }
        unicode_text |= format == 13;
    }
    let sequence = unsafe { GetClipboardSequenceNumber() };
    let closed = unsafe { CloseClipboard() };
    if closed == 0 {
        return Err(io::Error::other("无法关闭剪贴板预检句柄"));
    }
    if count < 0
        || !plain_text
        || (count > 0 && !unicode_text)
        || sequence == 0
        || unsafe { GetClipboardSequenceNumber() } != sequence
    {
        return Ok(None);
    }
    Ok(Some((count == 0, sequence)))
}

/// 非 Windows 暂不能枚举所有原生剪贴板格式，因此跳过可能丢失用户内容的复制探针。
///
/// @author kongweiguang
#[cfg(not(windows))]
fn clipboard_restore_preflight() -> Result<Option<(bool, u32)>, io::Error> {
    Ok(None)
}

/// 只用于检测验收期间其他进程的剪贴板更新，不读取或记录用户内容。
///
/// @author kongweiguang
#[cfg(windows)]
fn clipboard_sequence() -> Result<u32, io::Error> {
    let sequence =
        unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() };
    if sequence == 0 {
        return Err(io::Error::other("无法读取剪贴板序号"));
    }
    Ok(sequence)
}

/// 非 Windows 预检必定跳过，保留同一调用契约以便跨平台编译。
///
/// @author kongweiguang
#[cfg(not(windows))]
fn clipboard_sequence() -> Result<u32, io::Error> {
    Err(io::Error::other("当前平台无法验证剪贴板序号"))
}
