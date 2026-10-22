//! 固定真实 Win32 队列过滤语义，不把消息范围过滤与 QS 分类过滤混为一谈。
//!
//! @author kongweiguang

#![cfg(windows)]

use windows::Win32::UI::WindowsAndMessaging::*;

/// GPUI 的 paint/input 分类不会消耗 quit；据此排除错误根因，不为未证实问题改退出控制流。
#[test]
fn nested_paint_and_input_filters_leave_quit_for_outer_loop() {
    std::thread::spawn(|| {
        let mut message = MSG::default();
        // SAFETY: all queue operations are confined to this disposable thread; no HWND is used.
        unsafe {
            let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
            PostQuitMessage(37);
            assert!(!PeekMessageW(&mut message, None, 0, 0, PM_REMOVE | PM_QS_PAINT).as_bool());
            assert!(!PeekMessageW(&mut message, None, 0, 0, PM_REMOVE | PM_QS_INPUT).as_bool());
            assert_eq!(GetMessageW(&mut message, None, 0, 0).0, 0);
        }
        assert_eq!(message.message, WM_QUIT);
        assert_eq!(message.wParam.0, 37);
    })
    .join()
    .expect("isolated Windows message queue regression failed");
}

/// 普通消息范围不会排除 WM_QUIT，与上面的高位 QS 分类约束形成明确对照。
#[test]
fn message_range_filter_still_retrieves_quit() {
    std::thread::spawn(|| {
        let mut message = MSG::default();
        // SAFETY: only this disposable thread's queue is accessed.
        unsafe {
            let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
            PostQuitMessage(23);
            assert!(PeekMessageW(&mut message, None, WM_PAINT, WM_PAINT, PM_REMOVE).as_bool());
        }
        assert_eq!(message.message, WM_QUIT);
        assert_eq!(message.wParam.0, 23);
    })
    .join()
    .expect("isolated Windows message range regression failed");
}
