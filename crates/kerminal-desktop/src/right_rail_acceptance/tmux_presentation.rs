//! tmux 固定栏的真实鼠标、请求代次与刷新几何验收；仅操作隔离夹具。
//!
//! @author kongweiguang

use std::{error::Error, io};

use gpui_kit::{AnyWindowHandle, AsyncApp, Entity};
use gpui_pre::{InputEvent, MouseMoveEvent};
use kerminal_domain::identity::HostId;
use kerminal_ui::RootView;

use super::{
    RIGHT_RAIL_THEMES, RightRailCaptureContext, StateCaptureMetadata, StateCaptureRecord,
    ToolRailToolId, activate_dialog_tool, capture_state, frame_signature, functional::poll_root,
    menus, wait_for_stable_frame,
};

/// 聚焦 tmux 的只读三主题验收，避免其它工具的完整流程遮蔽当前交互的证据。
/// @author kongweiguang
pub(super) async fn capture_themes(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
) -> Result<Vec<StateCaptureRecord>, Box<dyn Error>> {
    let mut states = Vec::new();
    let mut baselines = Vec::new();
    for (mode, theme) in RIGHT_RAIL_THEMES {
        window.update(cx, |_, window, app| {
            root.update(app, |root, cx| {
                root.close_right_rail_acceptance_panels(window, cx);
                root.set_right_rail_acceptance_theme(mode, cx);
            })
        })?;
        let baseline = wait_for_stable_frame(
            cx,
            window,
            capture
                .output_directory
                .join(format!("theme-{theme}-baseline.png")),
            None,
            "tmux theme baseline",
        )
        .await?;
        activate_dialog_tool(
            cx,
            root,
            window,
            ToolRailToolId::Tmux,
            &capture.fixture_host_id,
            "tmux",
        )
        .await?;
        let session_id = poll_root(cx, root, window, "tmux fixture session", |root, _, cx| {
            root.right_rail_acceptance_tmux_session_id(&capture.fixture_host_id, cx)
        })
        .await?;
        check_refresh_and_quickref(
            cx,
            root,
            window,
            capture,
            theme,
            &baseline.pixel_sha256,
            &session_id,
            &mut states,
        )
        .await?;
        baselines.push(baseline.pixel_sha256);
    }
    if baselines[0] == baselines[1] {
        return Err(io::Error::other("tmux 浅色与深色基线相同").into());
    }
    Ok(states)
}

/// 同步回读生产状态，保证 Loading 检查发生在异步网络回执被调度之前。
/// @author kongweiguang
fn presentation(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
) -> Result<(u64, bool, bool, bool, bool), Box<dyn Error>> {
    window
        .update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.right_rail_acceptance_tmux_presentation(host, cx)
            })
        })?
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("tmux 验收目标或面板失效").into())
}

/// 实际元素消失即失败，不允许用可选坐标跳过列表被 loading 卡替换的回归。
/// @author kongweiguang
fn list_y(cx: &mut AsyncApp, window: AnyWindowHandle) -> Result<f32, Box<dyn Error>> {
    window
        .update(cx, |_, window, _| {
            window
                .debug_selector_bounds("rr-tmux-sessions")
                .map(|bounds| bounds.origin.y.as_f32())
                .ok_or_else(|| io::Error::other("tmux 会话列表未挂载"))
        })?
        .map_err(Into::into)
}

/// 每个主题均点击真实标题与刷新按钮，验证静态折叠、单次读取、防重复提交和列表稳定。
/// @author kongweiguang
pub(super) async fn check_refresh_and_quickref(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    theme: &'static str,
    baseline: &str,
    session_id: &str,
    states: &mut Vec<StateCaptureRecord>,
) -> Result<(), Box<dyn Error>> {
    let host = &capture.fixture_host_id;
    let _ = frame_signature(cx, window)?;
    let before = presentation(cx, root, window, host)?;
    if before.1 {
        menus::click_selector(cx, window, "rr-tmux-quickref-toggle")?;
        let _ = frame_signature(cx, window)?;
    }
    let before = presentation(cx, root, window, host)?;
    if before.2 || !before.3 || before.4 {
        return Err(io::Error::other("tmux 静态折叠验收前尚未就绪或仍有提示").into());
    }
    menus::click_selector(cx, window, "rr-tmux-quickref-toggle")?;
    let _ = frame_signature(cx, window)?;
    let expanded = presentation(cx, root, window, host)?;
    if expanded != (before.0, true, false, true, false) {
        return Err(
            io::Error::other(format!("快捷命令展开意外读取或显示提示：{expanded:?}")).into(),
        );
    }
    states.push(
        capture_state(
            cx,
            window,
            &capture.output_directory,
            baseline,
            StateCaptureMetadata {
                theme_name: theme,
                surface: "tmux-quickref-expanded",
                host_id: host.as_str().to_owned(),
                target_id: Some(session_id.to_owned()),
                interaction: "production_click_static_quickref_without_request",
            },
        )
        .await?,
    );
    // 等待稳定截图也给异步事件充分调度机会，避免只检查点击瞬间漏掉迟发读取。
    if presentation(cx, root, window, host)? != expanded {
        return Err(io::Error::other("快捷命令展开后发生迟发刷新").into());
    }
    menus::click_selector(cx, window, "rr-tmux-quickref-toggle")?;
    let _ = frame_signature(cx, window)?;
    if presentation(cx, root, window, host)? != before {
        return Err(io::Error::other("快捷命令收起意外改变请求或反馈").into());
    }

    let original_y = list_y(cx, window)?;
    menus::click_selector(cx, window, "rr-tmux-refresh")?;
    let _ = frame_signature(cx, window)?;
    let loading = presentation(cx, root, window, host)?;
    if loading != (before.0.wrapping_add(1).max(1), false, true, true, false) {
        return Err(
            io::Error::other(format!("刷新没有保留快照或出现额外提示：{loading:?}")).into(),
        );
    }
    let loading_y = list_y(cx, window)?;
    if (original_y - loading_y).abs() > 1.0 {
        return Err(
            io::Error::other(format!("刷新时列表移位：{original_y} -> {loading_y}")).into(),
        );
    }
    menus::click_selector(cx, window, "rr-tmux-refresh")?;
    if presentation(cx, root, window, host)? != loading {
        return Err(io::Error::other("刷新按钮重复点击产生了第二次读取").into());
    }
    poll_root(
        cx,
        root,
        window,
        "tmux 刷新真实回执",
        |root, _, cx| {
            root.right_rail_acceptance_tmux_session_ready(host, session_id, cx)
                .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    let _ = frame_signature(cx, window)?;
    let ready = presentation(cx, root, window, host)?;
    if ready != (loading.0, false, false, true, false)
        || (original_y - list_y(cx, window)?).abs() > 1.0
    {
        return Err(io::Error::other("刷新完成后请求、反馈或列表位置异常").into());
    }
    states.push(
        capture_state(
            cx,
            window,
            &capture.output_directory,
            baseline,
            StateCaptureMetadata {
                theme_name: theme,
                surface: "tmux-refresh-complete",
                host_id: host.as_str().to_owned(),
                target_id: Some(session_id.to_owned()),
                interaction: "production_refresh_retained_list_and_single_request",
            },
        )
        .await?,
    );
    // 恢复中性悬停状态，不能让测试自身遗留的按钮高亮污染后续折叠基线比对。
    window.update(cx, |_, window, app| {
        let _ = window.dispatch_event(MouseMoveEvent::default().to_platform_input(), app);
    })?;
    let _ = frame_signature(cx, window)?;
    Ok(())
}
