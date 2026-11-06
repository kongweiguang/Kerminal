//! Acceptance of the production rail customizer, unique slots, and per-tab snapshots.
//!
//! @author kongweiguang

use std::{error::Error, io, time::Instant};

use gpui_kit::{
    AnyWindowHandle, AsyncApp, Bounds, ElementId, Entity, InputEvent, KeyDownEvent, KeyUpEvent,
    Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollDelta,
    ScrollWheelEvent, TouchPhase, point, px,
};
use gpui_pre::size;
use kerminal_application::preferences::{ToolRailPanelPlacement, ToolRailSettings, ToolRailToolId};
use kerminal_domain::identity::TabId;
use kerminal_ui::RootView;

use super::{RightRailCaptureContext, functional::FunctionalRecord};

const PLACEMENT_TOOLS: [(ToolRailToolId, ToolRailPanelPlacement); 4] = [
    (ToolRailToolId::AgentLauncher, ToolRailPanelPlacement::Left),
    (ToolRailToolId::Sftp, ToolRailPanelPlacement::Bottom),
    (ToolRailToolId::Snippets, ToolRailPanelPlacement::Center),
    (ToolRailToolId::Tmux, ToolRailPanelPlacement::Attached),
];
const TOOL_ORDER: [ToolRailToolId; 4] = [
    ToolRailToolId::AgentLauncher,
    ToolRailToolId::Sftp,
    ToolRailToolId::Snippets,
    ToolRailToolId::Tmux,
];
const COMPACT_PANEL_WIDTH: f32 = 899.0;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ShellSnapshot {
    slots: [Option<ToolRailToolId>; 4],
    active_tab: Option<TabId>,
    tab_ids: Vec<TabId>,
    settings: ToolRailSettings,
    compact_panels: bool,
    compact_active_tool: Option<ToolRailToolId>,
    compact_tool_drawer_open: bool,
}

/// Runs bounded native-window events against Root and its production dialog, then restores the fixture state.
/// All pointer and key events stay in the private window; failure evidence is captured before restoring the fixture.
///
/// @author kongweiguang
pub(super) async fn run(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    let before = read_snapshot(cx, root, window)?;
    let result = run_checks(cx, root, window, capture, records, &before).await;
    if result.is_err() {
        let _ = super::wait_for_stable_frame(
            cx,
            window,
            capture.output_directory.join("shell-failure.png"),
            None,
            "shell failure diagnostic",
        )
        .await;
        let debug = window.update(cx, |_, window, _| {
            format!(
                "contexts={:?}\nviewport={:?}\nagent_trigger={:?}\nlist={:?}\npopup={:?}\n",
                window.context_stack(),
                window.visual_viewport_bounds(),
                window.debug_selector_bounds("rr-customizer-placement-AgentLauncher"),
                window.debug_selector_bounds("rr-customizer-sections"),
                window.debug_selector_bounds("rr-popup-menu"),
            )
        })?;
        std::fs::write(
            capture.output_directory.join("shell-failure-debug.txt"),
            debug,
        )?;
    }
    let restore = restore_state(cx, root, window, &before).await;
    match (result, restore) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(restore_error)) => Err(io::Error::other(format!(
            "右栏宿主验收失败：{error}；隔离状态恢复也失败：{restore_error}"
        ))
        .into()),
    }
}

/// Exercises cancel/save through the rendered customizer, then verifies unique dock slots and tab isolation.
///
/// @author kongweiguang
async fn run_checks(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    before: &ShellSnapshot,
) -> Result<(), Box<dyn Error>> {
    close_all_tools(cx, root, window).await?;
    exercise_cancel(cx, root, window, capture, records, &before.settings).await?;

    let mut configured = before.settings.clone();
    for (tool, placement) in PLACEMENT_TOOLS {
        configured.panel_placements.insert(tool, placement);
    }
    edit_placements(cx, root, window, &configured).await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "rail-customizer-saved",
        "production_customizer_saved_four_per_tool_placements_and_root_readback_matched",
    )
    .await?;

    open_tool(cx, root, window, ToolRailToolId::AgentLauncher).await?;
    open_tool(cx, root, window, ToolRailToolId::Sftp).await?;
    open_tool(cx, root, window, ToolRailToolId::Snippets).await?;
    open_tool(cx, root, window, ToolRailToolId::Tmux).await?;
    assert_slots(
        cx,
        root,
        window,
        [
            Some(ToolRailToolId::Tmux),
            Some(ToolRailToolId::AgentLauncher),
            Some(ToolRailToolId::Sftp),
            Some(ToolRailToolId::Snippets),
        ],
        "four independent dock slots",
    )?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "rail-distinct-directions-coexist",
        "root_projection_mounted_one_tool_in_each_configured_direction",
    )
    .await?;

    exercise_floating_drag(cx, window, capture, records).await?;
    close_tool(cx, root, window, ToolRailToolId::Tmux).await?;
    assert_slots(
        cx,
        root,
        window,
        [
            None,
            Some(ToolRailToolId::AgentLauncher),
            Some(ToolRailToolId::Sftp),
            Some(ToolRailToolId::Snippets),
        ],
        "repeat click closes the exact tool",
    )?;
    open_tool(cx, root, window, ToolRailToolId::Tmux).await?;
    open_tool(cx, root, window, ToolRailToolId::Containers).await?;
    assert_slots(
        cx,
        root,
        window,
        [
            Some(ToolRailToolId::Containers),
            Some(ToolRailToolId::AgentLauncher),
            Some(ToolRailToolId::Sftp),
            Some(ToolRailToolId::Snippets),
        ],
        "same direction replaces the mounted tool",
    )?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "rail-slot-toggle-and-replacement",
        "repeated_toggle_closed_and_same_direction_open_replaced_without_duplicates",
    )
    .await?;
    close_all_tools(cx, root, window).await?;

    exercise_tab_isolation(cx, root, window, capture, records).await?;
    exercise_compact_escape(cx, root, window, capture, records).await?;
    Ok(())
}

/// 宽转窄后先等待稳定绘制，再验证挂载和焦点并派发 Escape；显式关闭的工具不得跨断点复活。
///
/// @author kongweiguang
async fn exercise_compact_escape(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    let before = read_snapshot(cx, root, window)?;
    let original_size = window.update(cx, |_, window, _| window.viewport_size())?;
    let result = async {
        close_all_tools(cx, root, window).await?;
        resize_viewport(cx, root, window, 1020.0, original_size.height, false).await?;
        let focus_before = window.update(cx, |_, window, cx| window.focused(cx))?;
        open_tool(cx, root, window, ToolRailToolId::Tmux).await?;
        resize_viewport(
            cx,
            root,
            window,
            COMPACT_PANEL_WIDTH,
            original_size.height,
            true,
        )
        .await?;
        checkpoint(
            cx,
            window,
            capture,
            records,
            "rail-compact-resize-restored",
            "wide_open_tool_restored_as_drawer_on_899px_resize_without_extra_toggle",
        )
        .await?;
        assert_compact_tool_drawer(cx, root, window, ToolRailToolId::Tmux)?;
        let drawer_focus = window.update(cx, |_, window, cx| window.focused(cx))?;
        if drawer_focus.is_none() || drawer_focus == focus_before {
            return Err(io::Error::other("compact 工具抽屉未取得键盘焦点").into());
        }

        press_key(cx, window, "escape")?;
        let _ = super::frame_signature(cx, window)?;
        let after_escape = read_snapshot(cx, root, window)?;
        let focus_after = window.update(cx, |_, window, cx| window.focused(cx))?;
        if after_escape.compact_tool_drawer_open
            || after_escape.compact_active_tool.is_some()
            || after_escape.slots.contains(&Some(ToolRailToolId::Tmux))
        {
            return Err(io::Error::other("Escape 未关闭当前 compact 工具及其抽屉").into());
        }
        if focus_before.is_some() && focus_after != focus_before {
            return Err(io::Error::other("Escape 关闭 compact 抽屉后没有恢复原焦点").into());
        }
        if focus_before.is_none() && focus_after.is_none() {
            return Err(io::Error::other("Escape 关闭 compact 抽屉后没有可用的工作区焦点").into());
        }
        resize_viewport(cx, root, window, 1020.0, original_size.height, false).await?;
        resize_viewport(
            cx,
            root,
            window,
            COMPACT_PANEL_WIDTH,
            original_size.height,
            true,
        )
        .await?;
        let after_resize = read_snapshot(cx, root, window)?;
        if after_resize.compact_tool_drawer_open || after_resize.compact_active_tool.is_some() {
            return Err(io::Error::other("显式收起后跨断点重新打开了工具抽屉").into());
        }
        checkpoint(
            cx,
            window,
            capture,
            records,
            "rail-compact-escape",
            "899px_drawer_closed_exact_tool_and_restored_focus_after_private_window_escape",
        )
        .await?;
        Ok::<_, Box<dyn Error>>(())
    }
    .await;

    let restore = async {
        close_all_tools(cx, root, window).await?;
        let original_width = original_size.width.as_f32();
        resize_viewport(
            cx,
            root,
            window,
            original_width,
            original_size.height,
            original_width < 900.0,
        )
        .await?;
        restore_tab_slots(cx, root, window, before.slots).await?;
        let restored = read_snapshot(cx, root, window)?;
        if restored.slots != before.slots || restored.settings != before.settings {
            return Err(io::Error::other("compact 验收未恢复原右栏槽位或设置").into());
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;

    match (result, restore) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(restore_error)) => Err(io::Error::other(format!(
            "compact/Escape 验收失败：{error}；隔离窗口恢复也失败：{restore_error}"
        ))
        .into()),
    }
}

/// 等原生尺寸到位后唤醒隐藏窗口的绘制，再检查实际视口和响应式投影，避免把待提交尺寸当成完成。
///
/// @author kongweiguang
async fn resize_viewport(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    width: f32,
    height: Pixels,
    compact: bool,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, _| {
        let current = window.bounds().size;
        if (current.width.as_f32() - width).abs() > 0.75
            || (current.height.as_f32() - height.as_f32()).abs() > 0.75
        {
            window.resize(size(px(width), height));
        }
    })?;
    let deadline = Instant::now() + super::WINDOW_RESIZE_TIMEOUT;
    let mut native_paint_requested = false;
    loop {
        let platform_size = window.update(cx, |_, window, _| window.bounds().size)?;
        if !native_paint_requested
            && (platform_size.width.as_f32() - width).abs() <= 0.75
            && (platform_size.height.as_f32() - height.as_f32()).abs() <= 0.75
        {
            request_hidden_resize_paint()?;
            native_paint_requested = true;
        }
        if native_paint_requested {
            let _ = super::frame_signature(cx, window)?;
        }
        let (viewport_width, snapshot) = window.update(cx, |_, window, app| {
            let viewport_width = window.viewport_size().width.as_f32();
            let snapshot = root.update(app, |root, _| root.right_rail_acceptance_shell_snapshot());
            (viewport_width, snapshot)
        })?;
        if native_paint_requested
            && (viewport_width - width).abs() <= 0.75
            && snapshot.compact_panels() == compact
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "等待右栏视口宽度 {width}px / compact={compact} 超时，平台尺寸 {platform_size:?}，实际视口宽度 {viewport_width}px / compact={}",
                snapshot.compact_panels(),
            ))
            .into());
        }
        cx.background_executor()
            .timer(super::PANEL_FRAME_POLL)
            .await;
    }
}

/// 隐藏 HWND 无常规帧循环；只向当前进程和 UI 线程持有的验收窗口排队原生绘制，避免重入。
///
/// @author kongweiguang
#[cfg(windows)]
fn request_hidden_resize_paint() -> Result<(), io::Error> {
    use windows_sys::Win32::{
        System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
        UI::WindowsAndMessaging::{FindWindowW, GetWindowThreadProcessId, PostMessageW, WM_PAINT},
    };

    let title: Vec<u16> = "Kerminal Right Rail Acceptance"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: 标题以 NUL 结尾，只向核验过的本进程 UI 线程窗口发送无参数的异步消息。
    unsafe {
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if hwnd.is_null() {
            return Err(io::Error::other("找不到隐藏的右栏验收窗口"));
        }
        let mut process_id = 0;
        let thread_id = GetWindowThreadProcessId(hwnd, &mut process_id);
        if process_id != GetCurrentProcessId() || thread_id != GetCurrentThreadId() {
            return Err(io::Error::other("右栏验收窗口的进程或 UI 线程不匹配"));
        }
        if PostMessageW(hwnd, WM_PAINT, 0, 0) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// 其他平台沿用自身窗口绘制调度，后续视口与投影门禁仍然有效。
///
/// @author kongweiguang
#[cfg(not(windows))]
fn request_hidden_resize_paint() -> Result<(), io::Error> {
    Ok(())
}

/// Verifies the actual compact projection, visible drawer selector, and active tool after a frame.
///
/// @author kongweiguang
fn assert_compact_tool_drawer(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    tool: ToolRailToolId,
) -> Result<(), Box<dyn Error>> {
    let snapshot = read_snapshot(cx, root, window)?;
    let drawer_mounted = window.update(cx, |_, window, _| {
        window
            .debug_selector_bounds("workbench-compact-tool-drawer-surface")
            .is_some()
    })?;
    if !snapshot.compact_panels
        || snapshot.compact_active_tool != Some(tool)
        || !snapshot.compact_tool_drawer_open
        || !drawer_mounted
    {
        return Err(io::Error::other(format!(
            "compact 工具投影不符：tool={tool:?}, snapshot={snapshot:?}, drawer_mounted={drawer_mounted}"
        ))
        .into());
    }
    Ok(())
}

/// 在生产标题拖柄按下后先绘制一帧，让真实拖动监听生效；用位置回读证明移动且 MouseUp 后没有继续跟随。
/// 事件只发往隔离窗口，拖动幅度保留在 main 浮窗的边缘预算内，不依赖系统鼠标位置。
///
/// @author kongweiguang
async fn exercise_floating_drag(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    let (before, start) = window.update(cx, |_, window, _| {
        let panel = window.debug_selector_bounds("workbench-tool-panel-center-floating");
        let handle = window.debug_selector_bounds("workbench-tool-panel-center-drag-handle");
        (panel, handle.map(|bounds| bounds.center()))
    })?;
    let before = before.ok_or_else(|| io::Error::other("没有可拖动的生产工具浮窗"))?;
    let start = start.ok_or_else(|| io::Error::other("没有生产浮窗标题拖柄"))?;
    let destination = start + point(px(12.), px(4.));
    window.update(cx, |_, window, app| {
        let _ = window.dispatch_event(
            MouseDownEvent {
                button: MouseButton::Left,
                position: start,
                click_count: 1,
                ..MouseDownEvent::default()
            }
            .to_platform_input(),
            app,
        );
    })?;
    let _ = super::frame_signature(cx, window)?;
    window.update(cx, |_, window, app| {
        let _ = window.dispatch_event(
            MouseMoveEvent {
                position: destination,
                pressed_button: Some(MouseButton::Left),
                ..MouseMoveEvent::default()
            }
            .to_platform_input(),
            app,
        );
        let _ = window.dispatch_event(
            MouseUpEvent {
                button: MouseButton::Left,
                position: destination,
                click_count: 1,
                ..MouseUpEvent::default()
            }
            .to_platform_input(),
            app,
        );
    })?;
    let _ = super::frame_signature(cx, window)?;
    let moved = window
        .update(cx, |_, window, _| {
            window.debug_selector_bounds("workbench-tool-panel-center-floating")
        })?
        .ok_or_else(|| io::Error::other("拖动后生产浮窗消失"))?;
    if moved.origin.x <= before.origin.x {
        return Err(io::Error::other("生产浮窗标题拖动未改变横向位置").into());
    }
    window.update(cx, |_, window, app| {
        let _ = window.dispatch_event(
            MouseMoveEvent {
                position: destination + point(px(20.), px(20.)),
                ..MouseMoveEvent::default()
            }
            .to_platform_input(),
            app,
        );
    })?;
    let _ = super::frame_signature(cx, window)?;
    let released = window.update(cx, |_, window, _| {
        window.debug_selector_bounds("workbench-tool-panel-center-floating")
    })?;
    if released != Some(moved) {
        return Err(io::Error::other("释放鼠标后生产浮窗仍跟随移动").into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "rail-floating-drag-released",
        "production_header_drag_changed_bounds_and_release_stopped_following",
    )
    .await
}

/// 在真实弹框检查末行操作及显隐槽位，再取消草稿并确认 Root 配置未发生变化。
///
/// @author kongweiguang
async fn exercise_cancel(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    before: &ToolRailSettings,
) -> Result<(), Box<dyn Error>> {
    open_customizer(cx, root, window).await?;
    verify_visibility_geometry(cx, window, ToolRailToolId::AgentLauncher).await?;
    let current = before.effective_panel_placement(ToolRailToolId::AgentLauncher);
    let draft_placement = if current == ToolRailPanelPlacement::Left {
        ToolRailPanelPlacement::Bottom
    } else {
        ToolRailPanelPlacement::Left
    };
    choose_placement(cx, window, ToolRailToolId::AgentLauncher, draft_placement).await?;
    exercise_logs_customizer_row(cx, window, before).await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "rail-customizer-logs-bottom",
        "logs_row_scrolled_visible_visibility_and_placement_restored_footer_visible",
    )
    .await?;
    click_selector(cx, window, "rr-customizer-cancel")?;
    wait_customizer(cx, root, window, false).await?;
    let after = read_snapshot(cx, root, window)?;
    if &after.settings != before {
        return Err(io::Error::other("customizer 取消后 Root 配置发生变化").into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "rail-customizer-cancelled",
        "changed_customizer_draft_closed_without_updating_root_settings",
    )
    .await?;
    Ok(())
}

/// 先滚动真实列表使 Logs 整行进入裁切区，再通过原生开关和位置菜单验证动作并恢复草稿。
///
/// @author kongweiguang
async fn exercise_logs_customizer_row(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    before: &ToolRailSettings,
) -> Result<(), Box<dyn Error>> {
    scroll_selector_into_view(cx, window, "rr-customizer-row-Logs").await?;
    let (initial_visible, initial_pin, initial_placement) = logs_customizer_controls(cx, window)?;
    let initially_pinned =
        initial_pin.contains("移回主区域") || initial_pin.contains("Move to main area");
    if initially_pinned != before.bottom.contains(&ToolRailToolId::Logs) {
        return Err(io::Error::other("Logs 草稿的底部分区与原设置不一致").into());
    }

    if !initial_visible {
        click_selector(cx, window, "rr-customizer-visibility-Logs")?;
        let _ = super::frame_signature(cx, window)?;
    }
    for expected_visible in [false, true] {
        click_selector(cx, window, "rr-customizer-visibility-Logs")?;
        let _ = super::frame_signature(cx, window)?;
        let (visible, pin, _) = logs_customizer_controls(cx, window)?;
        if visible != expected_visible || pin != initial_pin {
            return Err(io::Error::other("Logs 显隐切换未生效或改变了底部身份").into());
        }
    }
    if !initial_visible {
        click_selector(cx, window, "rr-customizer-visibility-Logs")?;
        let _ = super::frame_signature(cx, window)?;
    }

    let original = before.effective_panel_placement(ToolRailToolId::Logs);
    if initial_placement != original {
        return Err(io::Error::other(format!(
            "Logs 初始位置不一致: expected={original:?}, observed={initial_placement:?}"
        ))
        .into());
    }
    let alternative = if original == ToolRailPanelPlacement::Left {
        ToolRailPanelPlacement::Bottom
    } else {
        ToolRailPanelPlacement::Left
    };
    choose_placement(cx, window, ToolRailToolId::Logs, alternative).await?;
    let (_, pin, changed_placement) = logs_customizer_controls(cx, window)?;
    if changed_placement != alternative || pin != initial_pin {
        return Err(io::Error::other(format!(
            "Logs 位置菜单未选中目标或改变了底部身份: expected={alternative:?}, observed={changed_placement:?}"
        ))
        .into());
    }
    choose_placement(cx, window, ToolRailToolId::Logs, original).await?;
    let (visible, pin, placement) = logs_customizer_controls(cx, window)?;
    if visible != initial_visible || pin != initial_pin || placement != original {
        return Err(io::Error::other(format!(
            "Logs 行操作后未恢复原草稿状态: visible={visible}/{initial_visible}, pinned={}, placement={placement:?}/{original:?}",
            pin == initial_pin,
        ))
        .into());
    }
    Ok(())
}

/// 从当前帧核对 Logs 控件和固定页脚，并把无障碍名称映射为实际草稿位置枚举。
///
/// @author kongweiguang
fn logs_customizer_controls(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<(bool, String, ToolRailPanelPlacement), Box<dyn Error>> {
    window
        .update(cx, |_, window, _| {
            let viewport = window.visual_viewport_bounds();
            let list = window
                .debug_selector_bounds("rr-customizer-sections")
                .ok_or_else(|| io::Error::other("customizer 列表不可见"))?;
            let row = window
                .debug_selector_bounds("rr-customizer-row-Logs")
                .ok_or_else(|| io::Error::other("Logs 行不可见"))?;
            let clip = list.intersect(&viewport);
            if !bounds_inside_clip(row, clip) {
                return Err(io::Error::other("Logs 行未完整进入列表裁切区"));
            }
            for selector in ["rr-customizer-cancel", "rr-customizer-save"] {
                let footer = window
                    .debug_selector_bounds(selector)
                    .ok_or_else(|| io::Error::other(format!("缺少页脚控件 {selector}")))?;
                if !bounds_inside_clip(footer, viewport)
                    || row.origin.y + row.size.height > footer.origin.y + px(0.75)
                {
                    return Err(io::Error::other(format!(
                        "Logs 行或页脚控件 {selector} 被裁切"
                    )));
                }
            }
            let control = |name: &str| {
                let id: ElementId = format!("tool-rail-{name}-Logs").into();
                let node = gpui_kit::base::test_support::find(window, &[], &id)
                    .ok_or_else(|| io::Error::other(format!("缺少 Logs 控件 {name}")))?;
                if !node.visible() || !bounds_inside_clip(node.bounds(), clip) {
                    return Err(io::Error::other(format!("Logs 控件 {name} 被裁切")));
                }
                Ok(node)
            };
            let placement = control("placement")?;
            let pin = control("pin")?;
            let _up = control("up")?;
            let _down = control("down")?;
            let visibility = control("visible")?;
            if !pin.supports_click() || !visibility.supports_click() {
                return Err(io::Error::other("Logs 固定或显隐控件不可操作"));
            }
            Ok((
                visibility
                    .checked()
                    .ok_or_else(|| io::Error::other("Logs 显隐状态不可读取"))?,
                pin.label()
                    .ok_or_else(|| io::Error::other("Logs 固定状态不可读取"))?
                    .to_owned(),
                placement
                    .label()
                    .and_then(customizer_placement_from_label)
                    .ok_or_else(|| io::Error::other("Logs 位置的无障碍语义无法识别"))?,
            ))
        })?
        .map_err(Into::into)
}

/// 位置语义来自草稿驱动的完整无障碍名称，不读取可能被截短的按钮可见文案。
///
/// @author kongweiguang
fn customizer_placement_from_label(label: &str) -> Option<ToolRailPanelPlacement> {
    [
        (
            "打开位置：贴靠右栏",
            "placement: Attach to right rail",
            ToolRailPanelPlacement::Attached,
        ),
        (
            "打开位置：左侧栏",
            "placement: Left sidebar",
            ToolRailPanelPlacement::Left,
        ),
        (
            "打开位置：底部面板",
            "placement: Bottom panel",
            ToolRailPanelPlacement::Bottom,
        ),
        (
            "打开位置：自由浮窗",
            "placement: Floating window",
            ToolRailPanelPlacement::Center,
        ),
    ]
    .into_iter()
    .find_map(|(zh, en, placement)| {
        (label.ends_with(zh) || label.ends_with(en)).then_some(placement)
    })
}

/// 显隐切换两次后恢复原草稿；比较本帧的四个生产槽位，避免把布局跳动误判为稳定。
///
/// @author kongweiguang
async fn verify_visibility_geometry(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    tool: ToolRailToolId,
) -> Result<(), Box<dyn Error>> {
    let visibility = format!("rr-customizer-visibility-{tool:?}");
    scroll_selector_into_view(cx, window, &visibility).await?;
    let before = customizer_row_geometry(cx, window, tool)?;
    let mut previous_frame = super::frame_signature(cx, window)?;
    for _ in 0..2 {
        click_selector(cx, window, &visibility)?;
        let next_frame = super::frame_signature(cx, window)?;
        if next_frame == previous_frame {
            return Err(
                io::Error::other(format!("customizer {tool:?} 显隐点击后画面未变化")).into(),
            );
        }
        previous_frame = next_frame;
        let after = customizer_row_geometry(cx, window, tool)?;
        if before.iter().zip(after).any(|(before, after)| {
            let epsilon = px(0.75);
            (before.origin.x - after.origin.x).abs() > epsilon
                || (before.origin.y - after.origin.y).abs() > epsilon
                || (before.size.width - after.size.width).abs() > epsilon
                || (before.size.height - after.size.height).abs() > epsilon
        }) {
            return Err(io::Error::other(format!(
                "customizer {tool:?} 显隐切换导致行或控件槽位移动"
            ))
            .into());
        }
    }
    Ok(())
}

/// 只读取当前帧的生产元素，并拒绝缺失或空尺寸，避免缺少观测点时误报通过。
///
/// @author kongweiguang
fn customizer_row_geometry(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    tool: ToolRailToolId,
) -> Result<[Bounds<Pixels>; 4], Box<dyn Error>> {
    let bounds = window.update(cx, |_, window, _| {
        let mut bounds = [Bounds::default(); 4];
        for (index, name) in ["row", "placement", "visibility", "hidden-indicator"]
            .into_iter()
            .enumerate()
        {
            let selector = format!("rr-customizer-{name}-{tool:?}");
            bounds[index] = window
                .debug_selector_bounds(&selector)
                .filter(|bounds| !bounds.is_empty())
                .ok_or_else(|| io::Error::other(format!("缺少有效的 {selector} 几何")))?;
        }
        Ok::<_, io::Error>(bounds)
    })??;
    Ok(bounds)
}

/// Uses the existing row menus and Save button for every placement change, then waits for the committed settings snapshot.
///
/// @author kongweiguang
async fn edit_placements(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    target: &ToolRailSettings,
) -> Result<(), Box<dyn Error>> {
    open_customizer(cx, root, window).await?;
    for tool in TOOL_ORDER {
        choose_placement(cx, window, tool, target.effective_panel_placement(tool)).await?;
    }
    click_selector(cx, window, "rr-customizer-save")?;
    wait_customizer(cx, root, window, false).await?;
    wait_settings(cx, root, window, target).await
}

/// 按生产菜单项的稳定索引和无障碍语义点击真实选项，不猜测鼠标悬停后的键盘高亮。
///
/// @author kongweiguang
async fn choose_placement(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    tool: ToolRailToolId,
    placement: ToolRailPanelPlacement,
) -> Result<(), Box<dyn Error>> {
    let selector = format!("rr-customizer-placement-{tool:?}");
    scroll_selector_into_view(cx, window, &selector).await?;
    click_selector(cx, window, &selector)?;
    wait_popup(cx, window, true).await?;
    let (index, labels) = match placement {
        ToolRailPanelPlacement::Attached => (0_usize, ["贴靠右栏", "Attach to right rail"]),
        ToolRailPanelPlacement::Left => (1, ["左侧栏", "Left sidebar"]),
        ToolRailPanelPlacement::Bottom => (2, ["底部面板", "Bottom panel"]),
        ToolRailPanelPlacement::Center => (3, ["自由浮窗", "Floating window"]),
    };
    window.update(cx, |_, window, app| {
        let popup = window
            .debug_selector_bounds("rr-popup-menu")
            .ok_or_else(|| io::Error::other("位置菜单未挂载"))?;
        let clip = popup.intersect(&window.visual_viewport_bounds());
        let id: ElementId = index.into();
        let mut items = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|node| {
                node.visible()
                    && node.role() == Some(gpui_kit::Role::MenuItem)
                    && node.path().last() == Some(&id)
                    && node.label().is_some_and(|label| labels.contains(&label))
                    && bounds_inside_clip(node.bounds(), clip)
                    && node.supports_click()
            });
        let item = items
            .next()
            .ok_or_else(|| io::Error::other(format!("位置菜单缺少目标 {placement:?}")))?;
        if items.next().is_some() {
            return Err(io::Error::other(format!(
                "位置菜单目标不唯一 {placement:?}"
            )));
        }
        let position = item.bounds().center();
        for event in [
            MouseMoveEvent {
                position,
                ..Default::default()
            }
            .to_platform_input(),
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
        ] {
            let _ = window.dispatch_event(event, app);
        }
        Ok::<_, io::Error>(())
    })??;
    wait_popup(cx, window, false).await
}

/// Dispatches the existing tool-toggle handler and checks the normalized slot projection after a rendered frame.
///
/// @author kongweiguang
async fn open_tool(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    tool: ToolRailToolId,
) -> Result<(), Box<dyn Error>> {
    dispatch_toggle(cx, root, window, tool)?;
    let _ = super::frame_signature(cx, window)?;
    let snapshot = read_snapshot(cx, root, window)?;
    if !snapshot.slots.contains(&Some(tool)) {
        return Err(io::Error::other(format!("Root 未在预期停靠槽打开 {tool:?}")).into());
    }
    Ok(())
}

/// Reuses the same toggle path for a repeated activation and verifies that it removes the exact tool.
///
/// @author kongweiguang
async fn close_tool(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    tool: ToolRailToolId,
) -> Result<(), Box<dyn Error>> {
    dispatch_toggle(cx, root, window, tool)?;
    let _ = super::frame_signature(cx, window)?;
    if read_snapshot(cx, root, window)?.slots.contains(&Some(tool)) {
        return Err(io::Error::other(format!("Root 重复切换后仍挂载 {tool:?}")).into());
    }
    Ok(())
}

/// 右击本帧真实 rail，使事件经过生产冒泡入口；随后等待 owner，不把成功投递鼠标事件当成打开成功。
///
/// @author kongweiguang
async fn open_customizer(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        let rail = window
            .debug_selector_bounds("workbench-tool-rail")
            .ok_or_else(|| io::Error::other("没有本帧可右击的生产 rail"))?;
        let position = rail.center();
        for event in [
            MouseDownEvent {
                button: MouseButton::Right,
                position,
                click_count: 1,
                ..MouseDownEvent::default()
            }
            .to_platform_input(),
            MouseUpEvent {
                button: MouseButton::Right,
                position,
                click_count: 1,
                ..MouseUpEvent::default()
            }
            .to_platform_input(),
        ] {
            let _ = window.dispatch_event(event, app);
        }
        Ok::<_, io::Error>(())
    })??;
    wait_customizer(cx, root, window, true).await
}

/// Waits for the Root modal state rather than treating a dispatched click or save request as completion.
///
/// @author kongweiguang
async fn wait_customizer(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    expected: bool,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let _ = super::frame_signature(cx, window)?;
        let open = window.update(cx, |_, _, app| {
            root.read(app)
                .right_rail_acceptance_shell_customizer_open(app)
        })?;
        if open == expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(
                io::Error::other(format!("等待 customizer open={expected} 状态超时")).into(),
            );
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// A save is complete only after the settings snapshot reflects the complete requested value.
///
/// @author kongweiguang
async fn wait_settings(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    expected: &ToolRailSettings,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let _ = super::frame_signature(cx, window)?;
        if read_snapshot(cx, root, window)?.settings == *expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("等待工具栏设置保存回读超时").into());
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// 用真实 Tab 验证打开集合隔离；缺少第二个 Tab 时只在已核实的 SSH fixture 中创建受控本地会话。
///
/// @author kongweiguang
async fn exercise_tab_isolation(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    let initial = read_snapshot(cx, root, window)?;
    let first = initial
        .active_tab
        .clone()
        .ok_or_else(|| io::Error::other("工作区没有活动 Tab，无法验证右栏隔离"))?;
    let existing_second = initial.tab_ids.iter().find(|tab| **tab != first).cloned();
    let mut fixture_target = None;
    let mut created_tab = None;
    let second = if let Some(second) = existing_second {
        second
    } else {
        if initial.tab_ids.as_slice() != [first.clone()] {
            return Err(io::Error::other("唯一活动 Tab 与工作区快照不一致").into());
        }
        let (fixture, target) = window.update(cx, |_, _, app| {
            let root = root.read(app);
            (
                root.right_rail_acceptance_fixture_tab_id(&capture.fixture_host_id),
                root.workbench_acceptance_agent_target_key(),
            )
        })?;
        if fixture.map_err(io::Error::other)? != first {
            return Err(io::Error::other("唯一活动 Tab 不是本轮 SSH fixture").into());
        }
        let target = target.ok_or_else(|| io::Error::other("fixture SSH 目标身份不可用"))?;
        if target.tab_id.as_ref() != Some(&first)
            || target.target_kind.as_deref() != Some("ssh")
            || target.target_ref.as_deref() != Some(capture.fixture_host_id.as_str())
            || target.terminal_session_id.is_none()
        {
            return Err(io::Error::other("fixture SSH 目标与活动 Tab 不一致").into());
        }
        fixture_target = Some(target);
        let created = create_fixture_local_tab(cx, root, window, &initial).await?;
        let second = created.tab_id.clone();
        created_tab = Some(created);
        second
    };

    let mut second_initial = None;
    let exercise = async {
        activate_tab(cx, root, window, &second).await?;
        let saved_second = read_snapshot(cx, root, window)?;
        second_initial = Some(saved_second.clone());
        activate_tab(cx, root, window, &first).await?;
        open_tool(cx, root, window, ToolRailToolId::AgentLauncher).await?;
        let first_with_agent = read_snapshot(cx, root, window)?.slots;
        activate_tab(cx, root, window, &second).await?;
        if read_snapshot(cx, root, window)?.slots != saved_second.slots {
            return Err(io::Error::other("切到第二个 Tab 后右栏打开集合未恢复其快照").into());
        }
        open_tool(cx, root, window, ToolRailToolId::Sftp).await?;
        let expected_second = slot_with_tool(
            saved_second.slots,
            initial
                .settings
                .effective_panel_placement(ToolRailToolId::Sftp),
            ToolRailToolId::Sftp,
        );
        if read_snapshot(cx, root, window)?.slots != expected_second {
            return Err(io::Error::other("第二个 Tab 未保留自己的右栏打开状态").into());
        }
        activate_tab(cx, root, window, &first).await?;
        if read_snapshot(cx, root, window)?.slots != first_with_agent {
            return Err(io::Error::other("第二个 Tab 的右栏变化泄漏到了首个 Tab").into());
        }
        checkpoint(
            cx,
            window,
            capture,
            records,
            "rail-tab-isolation",
            "two_existing_workspace_tabs_restored_independent_tool_open_sets",
        )
        .await
    }
    .await;

    let cleanup = async {
        activate_tab(cx, root, window, &first).await?;
        restore_tab_slots(cx, root, window, initial.slots).await?;
        if let Some(second_initial) = second_initial.as_ref() {
            activate_tab(cx, root, window, &second).await?;
            restore_tab_slots(cx, root, window, second_initial.slots).await?;
        }
        if let Some(created) = created_tab.as_ref() {
            close_fixture_local_tab(cx, root, window, created).await?;
        }
        activate_tab(cx, root, window, &first).await?;
        if let Some(expected) = fixture_target.as_ref() {
            let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
            loop {
                let (fixture, target) = window.update(cx, |_, _, app| {
                    let root = root.read(app);
                    (
                        root.right_rail_acceptance_fixture_tab_id(&capture.fixture_host_id),
                        root.workbench_acceptance_agent_target_key(),
                    )
                })?;
                if fixture.as_ref() == Ok(&first)
                    && target.as_ref().is_some_and(|target| {
                        target.tab_id == expected.tab_id
                            && target.pane_id == expected.pane_id
                            && target.terminal_session_id == expected.terminal_session_id
                            && target.target_ref == expected.target_ref
                            && target.target_kind == expected.target_kind
                    })
                {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::other(format!(
                        "恢复原 SSH fixture 目标超时：fixture={fixture:?}, target={target:?}"
                    ))
                    .into());
                }
                let _ = super::frame_signature(cx, window)?;
                cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
            }
        }
        Ok::<_, Box<dyn Error>>(())
    }
    .await;
    match (exercise, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(restore_error)) => Err(io::Error::other(format!(
            "Tab 隔离验证失败：{error}；测试 Tab 状态恢复也失败：{restore_error}"
        ))
        .into()),
    }
}

/// 保留本轮实际创建的 Tab、pane 和 session，清理时不能按当前选择推断资源归属。
struct CreatedFixtureTab {
    tab_id: TabId,
    pane_id: String,
    session_id: String,
}

/// 单击生产加号并等待新增的本地 pane 进入 Ready，拒绝把创建受理或失败占位当作可用 Tab。
///
/// @author kongweiguang
async fn create_fixture_local_tab(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    before: &ShellSnapshot,
) -> Result<CreatedFixtureTab, Box<dyn Error>> {
    let before_local = window.update(cx, |_, _, app| {
        root.read(app).workbench_acceptance_local_state()
    })?;
    click_selector(cx, window, "workspace-tabs-create-terminal")?;
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let _ = super::frame_signature(cx, window)?;
        let snapshot = read_snapshot(cx, root, window)?;
        let (local, pane_id, panes, creation, admission) = window.update(cx, |_, _, app| {
            let root = root.read(app);
            (
                root.workbench_acceptance_local_state(),
                root.workbench_acceptance_active_local_pane_id(),
                root.workbench_acceptance_local_panes(),
                root.workbench_acceptance_terminal_creation_state(),
                root.workbench_acceptance_workspace_admission_state(),
            )
        })?;
        let added = snapshot
            .tab_ids
            .iter()
            .filter(|tab| !before.tab_ids.contains(tab))
            .cloned()
            .collect::<Vec<_>>();
        if added.len() == 1
            && snapshot.tab_ids.len() == before.tab_ids.len() + 1
            && before
                .tab_ids
                .iter()
                .all(|tab| snapshot.tab_ids.contains(tab))
            && snapshot.active_tab.as_ref() == added.first()
            && local.0 == before_local.0 + 1
            && local.1 == before_local.1 + 1
            && local.2.len() == before_local.2.len() + 1
            && local.3 == before_local.3
            && !creation.0
            && !creation.1
            && admission.1.is_none()
            && let Some(pane_id) = pane_id
            && let Some((_, session_id, _)) = panes.iter().find(|pane| pane.0 == pane_id)
            && !before_local.2.contains(session_id)
        {
            return Ok(CreatedFixtureTab {
                tab_id: added[0].clone(),
                pane_id,
                session_id: session_id.clone(),
            });
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "创建 fixture 本地 Tab 超时：before={:?}, tabs={:?}, active={:?}, local={local:?}, creation={creation:?}, admission={admission:?}",
                before.tab_ids, snapshot.tab_ids, snapshot.active_tab,
            ))
            .into());
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// 通过同一生产关闭按钮回收本轮本地 Tab，并以真实 pane/session 消失作为完成条件。
///
/// @author kongweiguang
async fn close_fixture_local_tab(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    created: &CreatedFixtureTab,
) -> Result<(), Box<dyn Error>> {
    activate_tab(cx, root, window, &created.tab_id).await?;
    let (pane_id, panes, dialog) = window.update(cx, |_, window, app| {
        let root = root.read(app);
        (
            root.workbench_acceptance_active_local_pane_id(),
            root.workbench_acceptance_local_panes(),
            window.debug_selector_bounds("dialog-0"),
        )
    })?;
    if pane_id.as_deref() != Some(created.pane_id.as_str())
        || !panes
            .iter()
            .any(|pane| pane.0 == created.pane_id && pane.1 == created.session_id)
        || dialog.is_some()
    {
        return Err(io::Error::other("本轮本地 Tab 身份已变化或存在其他弹框，拒绝关闭").into());
    }
    click_selector(
        cx,
        window,
        &format!("workspace-tab-close-{}", created.tab_id.as_str()),
    )?;
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    let mut confirmed = false;
    loop {
        let _ = super::frame_signature(cx, window)?;
        let snapshot = read_snapshot(cx, root, window)?;
        let (local, admission, dialog) = window.update(cx, |_, window, app| {
            let root = root.read(app);
            (
                root.workbench_acceptance_local_state(),
                root.workbench_acceptance_workspace_admission_state(),
                window.debug_selector_bounds("dialog-0"),
            )
        })?;
        if !snapshot.tab_ids.contains(&created.tab_id)
            && !local.2.contains(&created.session_id)
            && admission.1.is_none()
        {
            return Ok(());
        }
        if !confirmed && dialog.is_some() {
            confirmed = click_fixture_close_confirmation(cx, window)?;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "关闭本轮本地 Tab 超时：tab={:?}, tabs={:?}, local={local:?}, admission={admission:?}, dialog={dialog:?}, confirmed={confirmed}",
                created.tab_id, snapshot.tab_ids,
            ))
            .into());
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// 只点击当前确认框内唯一可操作的 Kit 确认按钮，避免把同名背景节点当成关闭回执。
///
/// @author kongweiguang
fn click_fixture_close_confirmation(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<bool, Box<dyn Error>> {
    Ok(window.update(cx, |_, window, app| {
        let Some(dialog) = window.debug_selector_bounds("dialog-0") else {
            return Ok(false);
        };
        let id: ElementId = "ok".into();
        let mut matches = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|node| {
                node.visible()
                    && node.path().last() == Some(&id)
                    && node.supports_click()
                    && bounds_inside_clip(node.bounds(), dialog)
            });
        let Some(button) = matches.next() else {
            return Ok(false);
        };
        if matches.next().is_some() {
            return Err(io::Error::other("关闭确认按钮不唯一"));
        }
        let position = button.bounds().center();
        for event in [
            MouseMoveEvent {
                position,
                ..Default::default()
            }
            .to_platform_input(),
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
        ] {
            let _ = window.dispatch_event(event, app);
        }
        Ok::<_, io::Error>(true)
    })??)
}

/// 按精确 Tab 身份触发生产切换并等待投影，超时时保留创建与持久化门禁供排查。
///
/// @author kongweiguang
async fn activate_tab(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    tab: &TabId,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.activate_right_rail_acceptance_shell_tab(tab, window, cx)
        })
    })?;
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let _ = super::frame_signature(cx, window)?;
        if read_snapshot(cx, root, window)?.active_tab.as_ref() == Some(tab) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let snapshot = read_snapshot(cx, root, window)?;
            let (creation, admission) = window.update(cx, |_, _, app| {
                let root = root.read(app);
                (
                    root.workbench_acceptance_terminal_creation_state(),
                    root.workbench_acceptance_workspace_admission_state(),
                )
            })?;
            return Err(io::Error::other(format!(
                "等待工作区切换 Tab 回读超时：expected={tab:?}, active={:?}, tabs={:?}, creation={creation:?}, admission={admission:?}",
                snapshot.active_tab, snapshot.tab_ids,
            ))
            .into());
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// Closes only tools represented by the current four-slot projection.
///
/// @author kongweiguang
async fn close_all_tools(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    for tool in read_snapshot(cx, root, window)?.slots.into_iter().flatten() {
        close_tool(cx, root, window, tool).await?;
    }
    Ok(())
}

/// Closes a tab's current tools and reopens its saved slot set through the production toggle handler.
///
/// @author kongweiguang
async fn restore_tab_slots(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    expected: [Option<ToolRailToolId>; 4],
) -> Result<(), Box<dyn Error>> {
    close_all_tools(cx, root, window).await?;
    for tool in expected.into_iter().flatten() {
        open_tool(cx, root, window, tool).await?;
    }
    if read_snapshot(cx, root, window)?.slots != expected {
        return Err(io::Error::other("恢复 Tab 右栏快照后槽位不一致").into());
    }
    Ok(())
}

/// Inserts a tool into exactly one configured slot and preserves the other slots.
///
/// @author kongweiguang
fn slot_with_tool(
    mut slots: [Option<ToolRailToolId>; 4],
    placement: ToolRailPanelPlacement,
    tool: ToolRailToolId,
) -> [Option<ToolRailToolId>; 4] {
    let index = match placement {
        ToolRailPanelPlacement::Attached => 0,
        ToolRailPanelPlacement::Left => 1,
        ToolRailPanelPlacement::Bottom => 2,
        ToolRailPanelPlacement::Center => 3,
    };
    slots[index] = Some(tool);
    slots
}

/// Restores the initial placement configuration and tab-local open sets even when an acceptance assertion fails.
///
/// @author kongweiguang
async fn restore_state(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    before: &ShellSnapshot,
) -> Result<(), Box<dyn Error>> {
    let open = window.update(cx, |_, _, app| {
        root.read(app)
            .right_rail_acceptance_shell_customizer_open(app)
    })?;
    if open {
        window.update(cx, |_, window, app| {
            root.update(app, |root, cx| {
                root.close_right_rail_acceptance_customizer(window, cx)
            })
        })?;
        wait_customizer(cx, root, window, false).await?;
    }
    if let Some(active) = before.active_tab.as_ref() {
        activate_tab(cx, root, window, active).await?;
    }
    close_all_tools(cx, root, window).await?;
    let current = read_snapshot(cx, root, window)?;
    if current.settings != before.settings {
        edit_placements(cx, root, window, &before.settings).await?;
    }
    for tool in before.slots.into_iter().flatten() {
        open_tool(cx, root, window, tool).await?;
    }
    let restored = read_snapshot(cx, root, window)?;
    if restored.slots != before.slots || restored.settings != before.settings {
        return Err(io::Error::other("右栏验收结束后未恢复初始配置和活动 Tab 面板").into());
    }
    Ok(())
}

/// Toggles one tool through the Root-owned production event handler.
///
/// @author kongweiguang
fn dispatch_toggle(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    tool: ToolRailToolId,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.toggle_right_rail_acceptance_shell_tool(tool, window, root_cx)
        })
    })?;
    Ok(())
}

/// Reads a consistent Root projection without constructing parallel test state.
///
/// @author kongweiguang
fn read_snapshot(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
) -> Result<ShellSnapshot, Box<dyn Error>> {
    let snapshot = window.update(cx, |_, _, app| {
        root.update(app, |root, _| root.right_rail_acceptance_shell_snapshot())
    })?;
    Ok(ShellSnapshot {
        slots: snapshot.slots(),
        active_tab: snapshot.active_tab().cloned(),
        tab_ids: snapshot.tab_ids().to_vec(),
        settings: snapshot.settings().clone(),
        compact_panels: snapshot.compact_panels(),
        compact_active_tool: snapshot.compact_active_tool(),
        compact_tool_drawer_open: snapshot.compact_tool_drawer_open(),
    })
}

/// Requires the four actual Root slots to equal their expected unique occupants.
///
/// @author kongweiguang
fn assert_slots(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    expected: [Option<ToolRailToolId>; 4],
    label: &str,
) -> Result<(), Box<dyn Error>> {
    let actual = read_snapshot(cx, root, window)?.slots;
    if actual != expected {
        return Err(io::Error::other(format!(
            "{label}: slots were {actual:?}, expected {expected:?}"
        ))
        .into());
    }
    Ok(())
}

/// 只接受列表裁剪区内的控件；小数 DPI 使用位置容差，避免 intersect 的浮点差异引起多余滚动。
/// 双向滚动可恢复先前滚到标题下的行；不能把仍在窗口内的被裁剪控件当作可点击。
///
/// @author kongweiguang
async fn scroll_selector_into_view(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<(), Box<dyn Error>> {
    for _ in 0..12 {
        let _ = super::frame_signature(cx, window)?;
        let (bounds, viewport, list) = window.update(cx, |_, window, _| {
            (
                window.debug_selector_bounds(selector),
                window.visual_viewport_bounds(),
                window.debug_selector_bounds("rr-customizer-sections"),
            )
        })?;
        let list =
            list.ok_or_else(|| io::Error::other("customizer list selector is unavailable"))?;
        let clip = list.intersect(&viewport);
        if bounds.is_some_and(|bounds| bounds_inside_clip(bounds, clip)) {
            return Ok(());
        }
        let delta = if bounds.is_some_and(|bounds| bounds.origin.y < clip.origin.y) {
            120.
        } else {
            -120.
        };
        let position = list.center();
        window.update(cx, |_, window, app| {
            let _ = window.dispatch_event(
                ScrollWheelEvent {
                    position,
                    delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
                    touch_phase: TouchPhase::Moved,
                    ..ScrollWheelEvent::default()
                }
                .to_platform_input(),
                app,
            );
        })?;
        let _ = super::frame_signature(cx, window)?;
    }
    Err(io::Error::other(format!(
        "customizer placement control {selector:?} did not enter the viewport"
    ))
    .into())
}

/// 仅容纳 1.5 倍 DPI 的亚像素舍入；用四边不等式避免相交宽高相减后的浮点精度损失。
///
/// @author kongweiguang
fn bounds_inside_clip(bounds: Bounds<Pixels>, clip: Bounds<Pixels>) -> bool {
    let epsilon = px(0.75);
    !bounds.is_empty()
        && !clip.is_empty()
        && bounds.origin.x >= clip.origin.x - epsilon
        && bounds.origin.y >= clip.origin.y - epsilon
        && bounds.origin.x + bounds.size.width <= clip.origin.x + clip.size.width + epsilon
        && bounds.origin.y + bounds.size.height <= clip.origin.y + clip.size.height + epsilon
}

/// Sends the pointer gesture to a current-frame selector after verifying its actual visible bounds.
///
/// @author kongweiguang
fn click_selector(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        let viewport = window.visual_viewport_bounds();
        let bounds = window
            .debug_selector_bounds(selector)
            .ok_or_else(|| io::Error::other(format!("missing selector {selector:?}")))?;
        if !bounds_inside_clip(bounds, viewport) {
            return Err(io::Error::other(format!(
                "selector {selector:?} is outside the visible viewport"
            )));
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
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}

/// Presses a named GPUI key in the private window and releases it before the next action.
/// Flush the resulting frame before reading Root's rendered compact projection, which intentionally caches the last layout.
///
/// @author kongweiguang
fn press_key(cx: &mut AsyncApp, window: AnyWindowHandle, key: &str) -> Result<(), Box<dyn Error>> {
    let keystroke = Keystroke::parse(key)
        .map_err(|error| io::Error::other(format!("invalid acceptance key {key}: {error}")))?;
    window.update(cx, |_, window, app| {
        let _ = window.dispatch_event(
            KeyDownEvent {
                keystroke: keystroke.clone(),
                is_held: false,
                prefer_character_input: false,
            }
            .to_platform_input(),
            app,
        );
        let _ = window.dispatch_event(KeyUpEvent { keystroke }.to_platform_input(), app);
    })?;
    let _ = super::frame_signature(cx, window)?;
    Ok(())
}

/// Waits on the production popup selector after a placement trigger or menu-key action.
///
/// @author kongweiguang
async fn wait_popup(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    expected: bool,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let _ = super::frame_signature(cx, window)?;
        let open = window.update(cx, |_, window, _| {
            window.debug_selector_bounds("rr-popup-menu").is_some()
        })?;
        if open == expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待右栏位置菜单 open={expected} 超时")).into());
        }
        cx.background_executor()
            .timer(super::PANEL_FRAME_POLL)
            .await;
    }
}

/// Captures a stable visible result for one completed shell action.
///
/// @author kongweiguang
async fn checkpoint(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    operation: &'static str,
    outcome: &'static str,
) -> Result<(), Box<dyn Error>> {
    super::functional::checkpoint(cx, window, capture, records, operation, outcome).await
}
