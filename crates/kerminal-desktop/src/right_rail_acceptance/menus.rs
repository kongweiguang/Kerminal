//! 通过 GPUI 当前帧 selector 边界和窗口内输入事件捕获真实生产菜单。
//!
//! @author kongweiguang

use std::{error::Error, fs, io, path::Path, time::Instant};

use gpui_pre::{
    AnyWindowHandle, AsyncApp, Bounds, InputEvent, KeyDownEvent, KeyUpEvent, Keystroke,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
};
use serde::Serialize;

use super::{CaptureRecord, frame_signature, wait_for_stable_frame};

const POPUP_MENU_SELECTOR: &str = "rr-popup-menu";

/// 一次菜单打开、关闭及焦点读回的截图证据。
///
/// @author kongweiguang
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MenuCaptureRecord {
    surface: String,
    trigger_selector: String,
    popup_selector: &'static str,
    popup_present_before: bool,
    popup_present_open: bool,
    popup_inside_viewport: bool,
    popup_present_after: bool,
    focus_before: Option<String>,
    focus_open: Option<String>,
    focus_after: Option<String>,
    open_capture: CaptureRecord,
    closed_capture: CaptureRecord,
    dismissal: &'static str,
}

/// 单帧中弹层边界、可视范围与焦点身份的只读快照。
struct MenuFrameState {
    popup_bounds: Option<Bounds<Pixels>>,
    viewport: Bounds<Pixels>,
    focus: Option<String>,
}

/// 通过生产触发器打开菜单，显式检查弹层 selector 与实际视口，再用 Escape 关闭并独立保存关闭帧。
///
/// 点击前由调用方完成面板就绪；打开和关闭均以弹层 selector 为判据，不受焦点环或终端光标像素变化影响。
///
/// @author kongweiguang
pub(super) async fn capture_menu(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    output_directory: &Path,
    theme_name: &str,
    surface: &str,
    file_stem: &str,
    trigger_selector: &str,
) -> Result<MenuCaptureRecord, Box<dyn Error>> {
    if file_stem.is_empty()
        || !file_stem
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "menu capture file stem must contain only ASCII letters, digits, '-' or '_'",
        )
        .into());
    }

    let _ = frame_signature(cx, window)?;
    let before = read_menu_frame_state(cx, window)?;
    if before.popup_bounds.is_some() {
        return Err(io::Error::other(format!(
            "{surface} cannot start while selector {POPUP_MENU_SELECTOR:?} is already present"
        ))
        .into());
    }

    click_selector(cx, window, trigger_selector)?;
    let opened = wait_for_popup_presence(cx, window, true, surface).await?;
    let open_capture_result = wait_for_stable_frame(
        cx,
        window,
        output_directory.join(format!("menu-{theme_name}-{file_stem}.png")),
        None,
        surface,
    )
    .await;
    let open_state_after_capture = read_menu_frame_state(cx, window)?;
    let _closed_state = if open_state_after_capture.popup_bounds.is_some() {
        dismiss_and_wait_for_popup_absent(cx, window, surface, None).await?
    } else {
        return Err(io::Error::other(format!(
            "{surface} popup disappeared before the acceptance driver could dismiss it"
        ))
        .into());
    };
    let closed_capture_result = wait_for_stable_frame(
        cx,
        window,
        output_directory.join(format!("menu-{theme_name}-{file_stem}-closed.png")),
        None,
        surface,
    )
    .await;
    let after = read_menu_frame_state(cx, window)?;
    if after.popup_bounds.is_some() {
        return Err(io::Error::other(format!(
            "{surface} popup selector remained present after its closed-state capture"
        ))
        .into());
    }
    let open_capture = open_capture_result?;
    let closed_capture = closed_capture_result?;

    Ok(MenuCaptureRecord {
        surface: surface.to_owned(),
        trigger_selector: trigger_selector.to_owned(),
        popup_selector: POPUP_MENU_SELECTOR,
        popup_present_before: false,
        popup_present_open: opened.popup_bounds.is_some(),
        popup_inside_viewport: opened
            .popup_bounds
            .is_some_and(|bounds| bounds_inside_viewport(bounds, opened.viewport)),
        popup_present_after: after.popup_bounds.is_some(),
        focus_before: before.focus,
        focus_open: opened.focus,
        focus_after: after.focus,
        open_capture,
        closed_capture,
        dismissal: "escape",
    })
}

/// 只对当前帧中存在且位于可视视口内的真实触发器发送窗口输入，避免硬编码坐标误点。
///
/// @author kongweiguang
pub(super) fn click_selector(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        let viewport = window.visual_viewport_bounds();
        let bounds = window
            .debug_selector_bounds(selector)
            .ok_or_else(|| io::Error::other(format!("missing GPUI debug selector {selector:?}")))?;
        if !bounds_inside_viewport(bounds, viewport) {
            return Err(io::Error::other(format!(
                "GPUI debug selector {selector:?} is outside the visible viewport"
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

/// 在已绘制的 fixture 行触发右键并读取原生菜单标签，确保精简命令未仅仅被视觉遮挡。
///
/// @author kongweiguang
pub(super) async fn assert_file_context_menu(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    selector: &str,
    diagnostic: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let _ = frame_signature(cx, window)?;
    window.update(cx, |_, window, app| {
        let bounds = window
            .debug_selector_bounds(selector)
            .ok_or_else(|| io::Error::other(format!("missing fixture file row {selector:?}")))?;
        if !bounds_inside_viewport(bounds, window.visual_viewport_bounds()) {
            return Err(io::Error::other("fixture file row is outside the viewport"));
        }
        let position = bounds.center();
        for event in [
            MouseMoveEvent {
                position,
                ..MouseMoveEvent::default()
            }
            .to_platform_input(),
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
    wait_for_popup_presence(cx, window, true, "fixture file context menu").await?;
    let labels = window.update(cx, |_, window, _| {
        gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|node| node.role() == Some(gpui_pre::Role::MenuItem))
            .filter_map(|node| node.label().map(str::to_owned))
            .collect::<Vec<_>>()
    })?;
    let menu_result = if !labels
        .iter()
        .any(|label| label == "复制路径" || label == "Copy path")
    {
        Err(io::Error::other(format!(
            "file menu is missing Copy path: {labels:?}"
        )))
    } else if labels.iter().any(|label| {
        matches!(
            label.as_str(),
            "下载到剪贴板"
                | "Download to clipboard"
                | "复制项目"
                | "Copy item"
                | "工作区打开"
                | "Open in workspace"
        )
    }) {
        Err(io::Error::other(format!(
            "file menu contains a removed command: {labels:?}"
        )))
    } else {
        Ok(())
    };
    dismiss_and_wait_for_popup_absent(cx, window, "fixture file context menu", diagnostic).await?;
    menu_result?;
    Ok(())
}

/// 读取当前绘制帧的菜单边界、可视视口和焦点身份，供状态断言与验收报告共用。
///
/// @author kongweiguang
fn read_menu_frame_state(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<MenuFrameState, Box<dyn Error>> {
    let state = window.update(cx, |_, window, app| MenuFrameState {
        popup_bounds: window.debug_selector_bounds(POPUP_MENU_SELECTOR),
        viewport: window.visual_viewport_bounds(),
        focus: window.focused(app).map(|focus| format!("{focus:?}")),
    })?;
    Ok(state)
}

/// 直接比较边缘，避免分数 DPI 下 intersect 的浮点相减把内部边界误判为越界。
/// 0.01 逻辑像素只容纳计算舍入，仍拒绝可见的半像素溢出，不放宽菜单布局要求。
///
/// @author kongweiguang
fn bounds_inside_viewport(bounds: Bounds<Pixels>, viewport: Bounds<Pixels>) -> bool {
    let rounding = gpui_pre::px(0.01);
    !bounds.is_empty()
        && bounds.origin.x >= viewport.origin.x - rounding
        && bounds.origin.y >= viewport.origin.y - rounding
        && bounds.origin.x + bounds.size.width <= viewport.origin.x + viewport.size.width + rounding
        && bounds.origin.y + bounds.size.height
            <= viewport.origin.y + viewport.size.height + rounding
}

/// 根据真实 popup selector 等待菜单出现或消失；打开状态还要求菜单整体位于可视视口内。
///
/// @author kongweiguang
async fn wait_for_popup_presence(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    expected_present: bool,
    surface: &str,
) -> Result<MenuFrameState, Box<dyn Error>> {
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    let mut last_bounds = None;
    loop {
        let _ = frame_signature(cx, window)?;
        let state = read_menu_frame_state(cx, window)?;
        match state.popup_bounds {
            Some(bounds) if expected_present && bounds_inside_viewport(bounds, state.viewport) => {
                return Ok(state);
            }
            None if !expected_present => return Ok(state),
            Some(bounds) => last_bounds = Some(bounds),
            None => {}
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "timed out waiting for {surface} popup presence={expected_present}; last bounds={last_bounds:?}; viewport={:?}",
                state.viewport
            ))
            .into());
        }
        cx.background_executor()
            .timer(super::PANEL_FRAME_POLL)
            .await;
    }
}

/// 通过 GPUI 窗口分发 Escape，并以 popup selector 消失确认关闭，不使用 OS 级输入。
///
/// @author kongweiguang
async fn dismiss_and_wait_for_popup_absent(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    surface: &str,
    diagnostic: Option<&Path>,
) -> Result<MenuFrameState, Box<dyn Error>> {
    let escape = Keystroke::parse("escape").map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("could not construct Escape input: {error}"),
        )
    })?;
    let (before, after_down, after_up) = window.update(cx, |_, window, app| {
        let before = diagnostic.map(|_| menu_focus_diagnostic(window, app));
        let _ = window.dispatch_event(
            KeyDownEvent {
                keystroke: escape.clone(),
                is_held: false,
                prefer_character_input: false,
            }
            .to_platform_input(),
            app,
        );
        let after_down = diagnostic.map(|_| menu_focus_diagnostic(window, app));
        let _ = window.dispatch_event(KeyUpEvent { keystroke: escape }.to_platform_input(), app);
        let after_up = diagnostic.map(|_| menu_focus_diagnostic(window, app));
        (before, after_down, after_up)
    })?;
    let result = wait_for_popup_presence(cx, window, false, surface).await;
    if let Some(path) = diagnostic {
        let after_frame = window.update(cx, |_, window, app| menu_focus_diagnostic(window, app))?;
        fs::write(
            path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "beforeEscape": &before, "afterKeyDown": &after_down,
                "afterKeyUp": &after_up, "afterFrame": &after_frame,
            }))?,
        )?;
        if before
            .as_ref()
            .is_some_and(|state| state["drawerPresent"] == true)
            && (!before
                .as_ref()
                .is_some_and(|state| state["popupContextActive"] == true)
                || after_frame["popupPresent"] != false
                || after_frame["drawerPresent"] != true)
        {
            return Err(io::Error::other(
                "窄窗文件菜单未获得键盘上下文，或一次 Escape 同时关闭了抽屉",
            )
            .into());
        }
    }
    result
}

/// 只读当前焦点、键盘上下文及菜单和抽屉边界，不改变 Escape 的分发路径或焦点。
///
/// @author kongweiguang
fn menu_focus_diagnostic(window: &gpui_pre::Window, app: &gpui_pre::App) -> serde_json::Value {
    let contexts = window.context_stack();
    let popup = window.debug_selector_bounds(POPUP_MENU_SELECTOR);
    let drawer = window.debug_selector_bounds("workbench-compact-tool-drawer-surface");
    serde_json::json!({
        "focus": window.focused(app).map(|focus| format!("{focus:?}")),
        "contextStack": format!("{contexts:?}"),
        "popupContextActive": contexts.iter().any(|context| context.contains("PopupMenu")),
        "popupBounds": format!("{popup:?}"), "popupPresent": popup.is_some(),
        "drawerBounds": format!("{drawer:?}"), "drawerPresent": drawer.is_some(),
    })
}

#[cfg(test)]
#[path = "../../tests/unit/right_rail_menu_geometry.rs"]
mod tests;
