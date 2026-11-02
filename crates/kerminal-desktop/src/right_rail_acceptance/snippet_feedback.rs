//! 三主题重复验证片段刷新、详情读取及取消期间的实际布局，不发送终端命令。
//!
//! @author kongweiguang

use std::{error::Error, io, time::Duration};

use gpui_kit::{AnyWindowHandle, AsyncApp, Bounds, ElementId, Entity, Pixels};
use gpui_pre::{InputEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent};
use kerminal_domain::identity::HostId;
use kerminal_ui::RootView;
use serde::Serialize;

use super::{
    CaptureRecord, RightRailCaptureContext, ToolRailToolId, activate_dialog_tool, frame_signature,
    functional::poll_root, save_frame, wait_for_stable_frame,
};

const FIRST: &str = "snippet.builtin.core.docker_resources";
const SECOND: &str = "snippet.builtin.core.docker_disk";

/// 保存加载帧和最终展开帧，几何来自相同原生窗口内实际按钮的渲染边界。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SnippetFeedbackRecord {
    row_y_before: f32,
    row_y_refreshing: f32,
    row_y_loading: f32,
    row_y_cancelled: f32,
    row_y_closed: f32,
    late_receipt_preview_phase: &'static str,
    loading: CaptureRecord,
    expanded: CaptureRecord,
}

/// 使用 Kit 注册的稳定 ElementId 查找生产控件，不用固定屏幕坐标或伪造行内容。
fn bounds(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    id: &str,
) -> Result<Bounds<Pixels>, Box<dyn Error>> {
    let id: ElementId = id.to_owned().into();
    window
        .update(cx, |_, window, _| {
            let snapshot = gpui_kit::base::test_support::find(window, &[], &id)
                .ok_or_else(|| io::Error::other(format!("片段验收缺少控件 {id:?}")))?;
            if !snapshot.visible() {
                return Err(io::Error::other(format!("片段验收控件不可见 {id:?}")));
            }
            Ok(snapshot.bounds())
        })?
        .map_err(Into::into)
}

/// 点击当前已绘制按钮的中心，只向所属窗口派发输入，避免移动系统鼠标或抢占桌面。
fn click(cx: &mut AsyncApp, window: AnyWindowHandle, id: &str) -> Result<(), Box<dyn Error>> {
    let position = bounds(cx, window, id)?.center();
    window.update(cx, |_, window, app| {
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
    })?;
    let _ = frame_signature(cx, window)?;
    Ok(())
}

/// 同一主题同一控件的几何应完全稳定，只容纳浮点计算舍入，不放宽到可见布局跳动。
fn same_y(before: f32, after: f32, operation: &str) -> Result<(), Box<dyn Error>> {
    if (before - after).abs() > 0.01 {
        return Err(
            io::Error::other(format!("片段{operation}时位置变化：{before} -> {after}")).into(),
        );
    }
    Ok(())
}

/// 状态读回仍绑定原 fixture host，避免把其它目标或已选但未展开的详情当作成功。
fn phase(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host: &HostId,
) -> Result<&'static str, Box<dyn Error>> {
    window
        .update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.right_rail_acceptance_snippet_preview_phase(host, FIRST, cx)
            })
        })?
        .map_err(|message| io::Error::other(message).into())
}

/// 真实点击后的首帧必须仍保留列表位置；等待回执后再次检查，区分暂态和最终展开的正常位移。
/// 只读取内置模板，不触发 Fill、Run、收藏或用户目录写入。
///
/// @author kongweiguang
pub(super) async fn run(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &RightRailCaptureContext,
    theme: &'static str,
) -> Result<SnippetFeedbackRecord, Box<dyn Error>> {
    let host = &capture.fixture_host_id;
    activate_dialog_tool(
        cx,
        root,
        window,
        ToolRailToolId::Snippets,
        host,
        "snippet feedback",
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "片段布局筛选",
        |root, window, cx| {
            root.prepare_right_rail_acceptance_snippet(
                host,
                "snippet.builtin.core.docker",
                window,
                cx,
            )
            .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    poll_root(cx, root, window, "两条内置片段", |root, _, cx| {
        Ok(
            (root.right_rail_acceptance_snippet_catalog_ready(host, FIRST, cx)?
                && root.right_rail_acceptance_snippet_catalog_ready(host, SECOND, cx)?)
            .then_some(()),
        )
    })
    .await?;
    let _ = frame_signature(cx, window)?;
    let first = format!("snippet-open-builtin-{FIRST}");
    let second = format!("snippet-open-builtin-{SECOND}");
    // 主题切换保留真实详情缓存；先选另一条再收起，保证每轮首击确实读取 FIRST。
    click(cx, window, &second)?;
    poll_root(
        cx,
        root,
        window,
        "片段布局基线选择",
        |root, _, cx| {
            root.right_rail_acceptance_snippet_preview_phase(host, SECOND, cx)
                .map(|phase| (phase == "ready").then_some(()))
        },
    )
    .await?;
    let _ = frame_signature(cx, window)?;
    click(cx, window, &second)?;
    let row_y_before = bounds(cx, window, &second)?.origin.y.as_f32();
    let toolbar_y = bounds(cx, window, "snippet-new")?.origin.y.as_f32();

    click(cx, window, "snippet-catalog-refresh")?;
    let row_y_refreshing = bounds(cx, window, &second)?.origin.y.as_f32();
    same_y(row_y_before, row_y_refreshing, "刷新")?;
    same_y(
        toolbar_y,
        bounds(cx, window, "snippet-new")?.origin.y.as_f32(),
        "刷新工具栏",
    )?;
    poll_root(cx, root, window, "刷新片段回执", |root, _, cx| {
        root.right_rail_acceptance_snippet_catalog_ready(host, FIRST, cx)
            .map(|ready| ready.then_some(()))
    })
    .await?;

    click(cx, window, &first)?;
    if phase(cx, root, window, host)? != "loading" {
        return Err(io::Error::other("片段点击未进入真实详情读取阶段").into());
    }
    let row_y_loading = bounds(cx, window, &second)?.origin.y.as_f32();
    same_y(row_y_before, row_y_loading, "详情读取")?;
    same_y(
        toolbar_y,
        bounds(cx, window, "snippet-new")?.origin.y.as_f32(),
        "读取工具栏",
    )?;
    let loading = save_frame(
        cx,
        window,
        capture
            .output_directory
            .join(format!("snippet-{theme}-loading.png")),
    )?;
    click(cx, window, &first)?;
    let row_y_cancelled = bounds(cx, window, &second)?.origin.y.as_f32();
    same_y(row_y_before, row_y_cancelled, "取消读取")?;
    cx.background_executor()
        .timer(Duration::from_millis(300))
        .await;
    let _ = frame_signature(cx, window)?;
    let late_receipt_preview_phase = phase(cx, root, window, host)?;
    if late_receipt_preview_phase != "collapsed" {
        return Err(io::Error::other("取消后迟到回执重新展开了片段").into());
    }

    click(cx, window, &first)?;
    poll_root(cx, root, window, "片段展开就绪", |root, _, cx| {
        root.right_rail_acceptance_snippet_preview_phase(host, FIRST, cx)
            .map(|phase| (phase == "ready").then_some(()))
    })
    .await?;
    let expanded = wait_for_stable_frame(
        cx,
        window,
        capture
            .output_directory
            .join(format!("snippet-{theme}-expanded.png")),
        None,
        "snippet expanded",
    )
    .await?;
    if bounds(cx, window, &second)?.origin.y.as_f32() <= row_y_before {
        return Err(io::Error::other("片段详情就绪后未展开实际内容").into());
    }
    click(cx, window, &first)?;
    let row_y_closed = bounds(cx, window, &second)?.origin.y.as_f32();
    same_y(row_y_before, row_y_closed, "收起预览")?;
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.restore_right_rail_acceptance_snippet_catalog(host, window, cx)
        })
    })??;
    poll_root(cx, root, window, "恢复片段目录", |root, _, cx| {
        root.right_rail_acceptance_panel_ready(ToolRailToolId::Snippets, host, cx)
            .map(|ready| ready.then_some(()))
    })
    .await?;
    Ok(SnippetFeedbackRecord {
        row_y_before,
        row_y_refreshing,
        row_y_loading,
        row_y_cancelled,
        row_y_closed,
        late_receipt_preview_phase,
        loading,
        expanded,
    })
}
