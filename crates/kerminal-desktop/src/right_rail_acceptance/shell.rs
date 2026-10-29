//! Acceptance of the production rail customizer, unique slots, and per-tab snapshots.
//!
//! @author kongweiguang

use std::{error::Error, io, time::Instant};

use gpui_kit::{
    AnyWindowHandle, AsyncApp, Bounds, Entity, InputEvent, KeyDownEvent, KeyUpEvent, Keystroke,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollDelta,
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

/// Waits for the private window to render the requested logical width and matching responsive projection.
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
    window.update(cx, |_, window, _| window.resize(size(px(width), height)))?;
    let deadline = Instant::now() + super::WINDOW_RESIZE_TIMEOUT;
    loop {
        let _ = super::frame_signature(cx, window)?;
        let (viewport_width, snapshot) = window.update(cx, |_, window, app| {
            let viewport_width = window.viewport_size().width.as_f32();
            let snapshot = root.update(app, |root, _| root.right_rail_acceptance_shell_snapshot());
            (viewport_width, snapshot)
        })?;
        if (viewport_width - width).abs() <= 0.75 && snapshot.compact_panels() == compact {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "等待右栏视口宽度 {width}px / compact={compact} 超时，实际宽度 {viewport_width}px"
            ))
            .into());
        }
        cx.background_executor()
            .timer(super::PANEL_FRAME_POLL)
            .await;
    }
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

/// Changes a draft in the real modal, cancels it with the rendered button, and confirms Root settings did not move.
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
    let current = before.effective_panel_placement(ToolRailToolId::AgentLauncher);
    let draft_placement = if current == ToolRailPanelPlacement::Left {
        ToolRailPanelPlacement::Bottom
    } else {
        ToolRailPanelPlacement::Left
    };
    choose_placement(cx, window, ToolRailToolId::AgentLauncher, draft_placement).await?;
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

/// Selects one native menu item by its stable product order using the menu's own keyboard bindings.
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
    let index = match placement {
        ToolRailPanelPlacement::Attached => 0,
        ToolRailPanelPlacement::Left => 1,
        ToolRailPanelPlacement::Bottom => 2,
        ToolRailPanelPlacement::Center => 3,
    };
    for _ in 0..=index {
        press_key(cx, window, "down")?;
    }
    press_key(cx, window, "enter")?;
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

/// Switches between two existing workspace tabs and proves that one tab's tool changes do not leak into the other.
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
    let second = initial
        .tab_ids
        .iter()
        .find(|tab| **tab != first)
        .cloned()
        .ok_or_else(|| io::Error::other("隔离 fixture 中没有第二个 workspace Tab"))?;
    activate_tab(cx, root, window, &second).await?;
    let second_initial = read_snapshot(cx, root, window)?;
    activate_tab(cx, root, window, &first).await?;

    let exercise = async {
        open_tool(cx, root, window, ToolRailToolId::AgentLauncher).await?;
        let first_with_agent = read_snapshot(cx, root, window)?.slots;
        activate_tab(cx, root, window, &second).await?;
        if read_snapshot(cx, root, window)?.slots != second_initial.slots {
            return Err(io::Error::other("切到第二个 Tab 后右栏打开集合未恢复其快照").into());
        }
        open_tool(cx, root, window, ToolRailToolId::Sftp).await?;
        let expected_second = slot_with_tool(
            second_initial.slots,
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
        activate_tab(cx, root, window, &second).await?;
        restore_tab_slots(cx, root, window, second_initial.slots).await?;
        activate_tab(cx, root, window, &first).await
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

/// Activates the requested existing tab through Root and waits for its normal runtime projection.
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
            return Err(io::Error::other("等待工作区切换 Tab 回读超时").into());
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
