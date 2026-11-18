//! 验证 Agent 右栏的隔离 Custom CLI 新建、继续、改名、输出与归档闭环。
//!
//! @author kongweiguang

use std::{
    collections::BTreeSet,
    error::Error,
    ffi::OsString,
    fmt::{self, Display, Formatter},
    fs,
    io::{self},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use gpui_kit::{
    AnyWindowHandle, AsyncApp, Bounds, Entity, InputEvent, Pixels, ScrollDelta, ScrollWheelEvent,
    TouchPhase, point, px,
};
use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    agents::AgentSessionView,
    preferences::ThemeMode,
    sessions::{
        CreateSessionRequest, RenderSnapshotRequest, SessionDeadline, SessionFilter,
        SessionRuntimeMetadata, TerminalInputEvent,
    },
};
use kerminal_domain::{
    agents::{AgentSessionStatus, AgentTargetBinding},
    identity::{AgentSessionId, HostId, PaneId, SessionId, TabId},
    sessions::{LocalSessionTarget, SessionLifecycle, SessionOwner, SessionTarget, TerminalSize},
};
use kerminal_terminal::snapshot::RenderSnapshot;
use kerminal_ui::{AppServices, RootView};

use super::{
    ToolRailToolId, frame_signature, frame_signature_visible,
    functional::{FunctionalRecord, checkpoint, checkpoint_visible, poll_root},
};

/// 连续性验收目标：SSH fixture 或本轮全新本地模板终端。两者都走同一生产 Root、AgentsPanel
/// 和会话生命周期，仅目标绑定校验不同；本地模式不读取 manifest、vault、key 或任何远程服务。
///
/// @author kongweiguang
#[derive(Clone, Copy)]
enum ContinuityTarget<'a> {
    Fixture(&'a HostId),
    Local,
}

impl<'a> ContinuityTarget<'a> {
    /// 仅供错误信息区分目标来源，不影响生产动作选择。
    ///
    /// @author kongweiguang
    fn label(self) -> &'static str {
        match self {
            Self::Fixture(_) => "fixture SSH",
            Self::Local => "local",
        }
    }
}

/// 本轮流程创建的真实本地会话身份。session ID 证明注册表身份，process_id 证明同一 PTY
/// 进程没有被关闭后重建的同名 session 冒充；两者都缺失时不能声称身份连续。
///
/// @author kongweiguang
#[derive(Clone, Debug)]
struct ContinuitySessionIdentity {
    session_id: SessionId,
    process_id: Option<u32>,
    lifecycle: SessionLifecycle,
}

/// 本轮连续性流程实际创建或发现的会话身份。异常与成功收尾都只按这些 ID 回收，
/// 不再按 Tab owner 批量关闭，避免误伤 fixture 或用户已有会话；A2 成功回收后清空待回收 ID，
/// 但保留关闭前的身份证据，供后续 B 复核“不是 A2 被复用”。
///
/// @author kongweiguang
#[derive(Default)]
struct ContinuityResources {
    launcher_key: Option<String>,
    agent_session_id: Option<AgentSessionId>,
    agent_terminal_session_id: Option<SessionId>,
    a1_session_id: Option<SessionId>,
    a1_pane_id: Option<PaneId>,
    a2_session_id: Option<SessionId>,
    a2_closed_identity: Option<ContinuitySessionIdentity>,
    b_session_id: Option<SessionId>,
    /// 入口选择面显式新建或“无 Running”直接新建产生的额外 Agent 会话；与主 Agent 一样
    /// 按精确身份归档和关闭，避免验收流程留下第二个运行中 CLI。
    extra_agent_sessions: Vec<(AgentSessionId, SessionId)>,
    /// 生产创建请求已发出、但尚未读回唯一会话身份的阶段。此时不能按 Tab/owner 扫描回收，
    /// 失败报告必须明确该窗口只依赖组合根 shutdown 与私有 Job 兜底。
    pending_creation: Option<&'static str>,
}

impl ContinuityResources {
    /// 普通本地 PTY 的清理集合不包含 Agent PTY，后者由 Agent 回收流程按 launcher/terminal
    /// 身份单独处理，避免同一 PTY 被两条路径重复关闭。
    ///
    /// @author kongweiguang
    fn local_terminal_session_ids(&self) -> Vec<SessionId> {
        self.collect_session_ids([&self.a1_session_id, &self.a2_session_id, &self.b_session_id])
    }

    /// 成功路径需要同时证明 Agent PTY 与流程创建的本地 PTY 都已收敛；缺失的 ID 不猜测，
    /// 已回收的 A2 只保留身份证据、不再进入回收集合，因此这里只返回仍有待关闭身份的资源。
    ///
    /// @author kongweiguang
    fn all_terminal_session_ids(&self) -> Vec<SessionId> {
        let mut ids = self
            .collect_session_ids([
                &self.agent_terminal_session_id,
                &self.a1_session_id,
                &self.a2_session_id,
                &self.b_session_id,
            ])
            .into_iter()
            .collect::<BTreeSet<_>>();
        ids.extend(
            self.extra_agent_sessions
                .iter()
                .map(|(_, terminal_session_id)| terminal_session_id.clone()),
        );
        ids.into_iter().collect()
    }

    /// 会话 ID 可能因异常阶段重复记录，去重后再交给 close_until，保持清理顺序稳定。
    ///
    /// @author kongweiguang
    fn collect_session_ids<const N: usize>(&self, ids: [&Option<SessionId>; N]) -> Vec<SessionId> {
        ids.into_iter()
            .flatten()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

/// 在隔离 output/config 中配置无网络 PowerShell marker CLI，并通过生产 AgentPanel 完成生命周期读回。
///
/// @author kongweiguang
pub(super) async fn run(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &super::RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    let fixture_tab_id = window.update(cx, |_, _, app| {
        root.read(app)
            .right_rail_acceptance_fixture_tab_id(&capture.fixture_host_id)
    })??;
    let result = run_agent_acceptance_flow(cx, root, window, capture, records).await;
    let restored = window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.restore_right_rail_acceptance_fixture_tab(&fixture_tab_id, window, root_cx)
        })
    })?;
    if !restored {
        let failure = result
            .as_ref()
            .err()
            .map(|error| format!("; Agent flow also failed: {error}"))
            .unwrap_or_default();
        return Err(io::Error::other(format!(
            "Could not restore the exact original fixture TabId {fixture_tab_id}{failure}"
        ))
        .into());
    }
    result
}

/// Runs the production Agent lifecycle while the outer driver retains responsibility for exact-tab restoration.
///
/// @author kongweiguang
async fn run_agent_acceptance_flow(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &super::RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    let host_id = &capture.fixture_host_id;
    activate_agent_panel(cx, root, window, host_id).await?;

    let setup = window.update(cx, |_, _, app| {
        root.update(app, |root, root_cx| {
            root.configure_right_rail_acceptance_agent(host_id, root_cx)
        })
    })??;
    poll_root(
        cx,
        root,
        window,
        "Agent launcher CAS、validator 和 catalog 读回",
        |root, _, cx| {
            root.right_rail_acceptance_agent_launcher_ready(host_id, setup.launcher_id(), cx)
                .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-custom-launcher-ready",
        "isolated_typed_preferences_cas_settings_validator_and_catalog_readback_confirmed",
    )
    .await?;

    let started = window.update(cx, |_, _, app| {
        root.update(app, |root, root_cx| {
            root.start_right_rail_acceptance_agent(host_id, setup.launcher_id(), root_cx)
        })
    })??;
    if !started {
        return Err(io::Error::other("Agent production create action did not start").into());
    }
    let custom_key = format!("custom:{}", setup.launcher_id());
    let created = wait_for_agent_view(
        cx,
        root,
        window,
        ContinuityTarget::Fixture(host_id),
        None,
        |view| {
            view.session
                .launcher_key
                .as_ref()
                .is_some_and(|key| key.as_str() == custom_key)
        },
        "new Custom Agent projection",
    )
    .await?;
    let agent_session_id = created.session.agent_session_id.clone();
    let created_terminal = wait_for_agent_view(
        cx,
        root,
        window,
        ContinuityTarget::Fixture(host_id),
        Some(&agent_session_id),
        |view| {
            view.terminal
                .as_ref()
                .is_some_and(|terminal| terminal.lifecycle == SessionLifecycle::Running)
        },
        "running Custom Agent PTY",
    )
    .await?;
    let terminal_session_id = created_terminal
        .terminal
        .as_ref()
        .map(|terminal| terminal.session_id.clone())
        .ok_or_else(|| io::Error::other("created Agent has no real terminal session"))?;
    let target_generation = created_terminal
        .session
        .target
        .as_ref()
        .map(|target| target.generation)
        .ok_or_else(|| io::Error::other("created Agent has no persisted target binding"))?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-created-running",
        "production_create_returned_custom_launcher_and_running_agent_owned_pty",
    )
    .await?;

    let continued = window.update(cx, |_, _, app| {
        root.update(app, |root, root_cx| {
            root.continue_right_rail_acceptance_agent(host_id, &agent_session_id, root_cx)
        })
    })??;
    if !continued {
        return Err(io::Error::other(
            "Agent production continue action did not accept the running session",
        )
        .into());
    }
    let continued_view = wait_for_agent_view(
        cx,
        root,
        window,
        ContinuityTarget::Fixture(host_id),
        Some(&agent_session_id),
        |view| {
            view.session
                .target
                .as_ref()
                .is_some_and(|target| target.generation > target_generation)
                && view.terminal.as_ref().is_some_and(|terminal| {
                    terminal.session_id == terminal_session_id
                        && terminal.lifecycle == SessionLifecycle::Running
                })
        },
        "explicit continue target-binding readback",
    )
    .await?;
    let continued_generation = continued_view
        .session
        .target
        .as_ref()
        .map(|target| target.generation)
        .ok_or_else(|| io::Error::other("continued Agent lost its target binding"))?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-continued",
        "explicit_continue_advanced_target_generation_without_replacing_the_running_pty",
    )
    .await?;

    let renamed_title = format!("Acceptance {}", &setup.output_marker()[9..]);
    let rename_started = window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.rename_right_rail_acceptance_agent(
                host_id,
                &agent_session_id,
                renamed_title.clone(),
                window,
                root_cx,
            )
        })
    })??;
    if !rename_started {
        return Err(io::Error::other("Agent production rename action did not start").into());
    }
    wait_for_agent_view(
        cx,
        root,
        window,
        ContinuityTarget::Fixture(host_id),
        Some(&agent_session_id),
        |view| {
            view.session.title == renamed_title
                && view
                    .session
                    .target
                    .as_ref()
                    .is_some_and(|target| target.generation >= continued_generation)
        },
        "renamed Agent title CAS readback",
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-renamed",
        "same_agent_session_id_returned_the_renamed_title_after_production_readback",
    )
    .await?;

    let (lifecycle, terminal_text) = wait_for_agent_output(
        cx,
        root,
        window,
        ContinuityTarget::Fixture(host_id),
        &agent_session_id,
        &terminal_session_id,
        AgentOutputExpectation {
            marker: setup.output_marker(),
            lifecycle: SessionLifecycle::Exited,
        },
    )
    .await?;
    if lifecycle != SessionLifecycle::Exited || !terminal_text.contains(setup.output_marker()) {
        return Err(io::Error::other(
            "Agent terminal did not return the exact marker and Exited state",
        )
        .into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-output-exited",
        "unique_marker_read_from_agent_renderer_snapshot_and_same_session_exited_normally",
    )
    .await?;

    poll_root(
        cx,
        root,
        window,
        "打开生产 Agent 删除确认",
        |root, window, cx| {
            if !root.right_rail_acceptance_agent_archive_confirmation_ready(
                host_id,
                &agent_session_id,
                cx,
            )? {
                root.archive_right_rail_acceptance_agent(host_id, &agent_session_id, window, cx)?;
            }
            root.right_rail_acceptance_agent_archive_confirmation_ready(
                host_id,
                &agent_session_id,
                cx,
            )
            .map(|ready| ready.then_some(()))
        },
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-delete-confirmation",
        "production_delete_confirmation_mounted_for_exact_agent_before_submission",
    )
    .await?;
    assert_agent_delete_geometry(cx, window)?;
    capture_agent_delete_themes(cx, root, window, capture, records).await?;
    poll_root(
        cx,
        root,
        window,
        "生产 Agent 删除确认提交",
        |root, window, cx| {
            root.archive_right_rail_acceptance_agent(host_id, &agent_session_id, window, cx)
                .map(|submitted| submitted.then_some(()))
        },
    )
    .await?;
    let archived_view = wait_for_agent_view(
        cx,
        root,
        window,
        ContinuityTarget::Fixture(host_id),
        Some(&agent_session_id),
        |view| view.session.status == AgentSessionStatus::Archived,
        "archived Agent history readback",
    )
    .await?;
    if archived_view.session.agent_session_id != agent_session_id
        || archived_view.session.title != renamed_title
    {
        return Err(io::Error::other(
            "Agent archive readback changed the accepted identity or title",
        )
        .into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-archived",
        "production_archive_preserved_the_same_renamed_agent_history",
    )
    .await?;

    // 标准 fixture 流程保持原有边界；本地连续性只走独立的 agent-only 入口，
    // 不能在这里追加 pwsh7、分屏或额外会话前置条件。
    Ok(())
}

/// 在同一生产确认框切换应用主题，覆盖保留弹框的主题继承，最后恢复功能轮的深色基线。
/// 不修改用户的系统主题，跟随系统仅按隔离窗口实际 appearance 解析。
///
/// @author kongweiguang
async fn capture_agent_delete_themes(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &super::RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    for (mode, operation) in [
        (ThemeMode::Light, "agent-delete-confirmation-light"),
        (ThemeMode::System, "agent-delete-confirmation-system"),
        (ThemeMode::Dark, "agent-delete-confirmation-dark"),
    ] {
        window.update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.set_right_rail_acceptance_theme(mode, cx)
            });
        })?;
        checkpoint(
            cx,
            window,
            capture,
            records,
            operation,
            "same_production_delete_dialog_keeps_theme_and_centered_bounded_layout",
        )
        .await?;
        assert_agent_delete_geometry(cx, window)?;
    }
    Ok(())
}

/// 在入口落地页切换浅/跟随系统/深色，回读真实居中列 bounds；最后恢复功能轮深色基线。
/// 不改变选择、绑定或历史，只验证主题继承、垂直居中和窄栏边界。
///
/// @author kongweiguang
async fn capture_agent_landing_themes(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &super::RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    for (mode, operation) in [
        (ThemeMode::Light, "agent-continuity-landing-light"),
        (ThemeMode::System, "agent-continuity-landing-system"),
        (ThemeMode::Dark, "agent-continuity-landing-dark"),
    ] {
        window.update(cx, |_, _, app| {
            root.update(app, |root, cx| {
                root.set_right_rail_acceptance_theme(mode, cx)
            });
        })?;
        checkpoint(
            cx,
            window,
            capture,
            records,
            operation,
            "same_production_landing_keeps_theme_and_centered_group",
        )
        .await?;
        assert_landing_geometry(cx, window)?;
    }
    window.update(cx, |_, _, app| {
        root.update(app, |root, cx| {
            root.set_right_rail_acceptance_theme(ThemeMode::Dark, cx)
        });
    })?;
    Ok(())
}

/// 回读真实入口列与滚动内容 bounds，确认列在可用高度内居中且没有超出当前窄栏视口。
///
/// @author kongweiguang
fn assert_landing_geometry(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    let result = window.update(cx, |_, window, _| {
        let column = window
            .debug_selector_bounds("rr-agent-landing-column")
            .ok_or_else(|| "入口列未挂载".to_owned())?;
        let content = window
            .debug_selector_bounds("rr-agent-landing-scroll-content")
            .ok_or_else(|| "入口滚动内容未挂载".to_owned())?;
        let viewport = window.viewport_size();
        if column.size.width.as_f32() > viewport.width.as_f32() + 0.5
            || column.origin.x.as_f32() < -0.5
        {
            return Err(format!(
                "入口列超出窄栏视口：{column:?} viewport={viewport:?}"
            ));
        }
        let top_gap = column.origin.y.as_f32() - content.origin.y.as_f32();
        let bottom_gap =
            content.origin.y.as_f32() + content.size.height.as_f32()
                - (column.origin.y.as_f32() + column.size.height.as_f32());
        if (top_gap - bottom_gap).abs() > 1.0 {
            return Err(format!(
                "入口列未垂直居中：top_gap={top_gap} bottom_gap={bottom_gap} column={column:?} content={content:?}"
            ));
        }
        Ok(())
    })?;
    result.map_err(|detail| io::Error::other(detail).into())
}

/// 读取入口、历史标题、固定视口、列表和当前窗口高度；四条真实记录共用同一组锚点。
///
/// @author kongweiguang
fn history_viewport_bounds(
    window: &mut gpui_kit::Window,
) -> Result<
    (
        Bounds<Pixels>,
        Bounds<Pixels>,
        Bounds<Pixels>,
        Bounds<Pixels>,
        f32,
    ),
    String,
> {
    let picker = window
        .debug_selector_bounds("rr-agent-picker")
        .ok_or_else(|| "历史视口补测缺少入口选择器".to_owned())?;
    let heading = window
        .debug_selector_bounds("rr-agent-history-heading")
        .ok_or_else(|| "历史视口补测缺少历史标题".to_owned())?;
    let body = window
        .debug_selector_bounds("rr-agent-history-body")
        .ok_or_else(|| "历史视口补测缺少固定视口".to_owned())?;
    let list = window
        .debug_selector_bounds("rr-agent-history-list")
        .ok_or_else(|| "历史视口补测缺少历史列表".to_owned())?;
    let viewport = window.viewport_size();
    Ok((picker, heading, body, list, viewport.height.as_f32()))
}

/// 读取指定真实历史行的窗口 bounds，并按 top 排序；动态 selector 只能走 Window 查询。
///
/// @author kongweiguang
fn history_rows_bounds(
    window: &mut gpui_kit::Window,
    agent_ids: &[AgentSessionId],
) -> Result<Vec<Bounds<Pixels>>, String> {
    let mut rows = Vec::with_capacity(agent_ids.len());
    for id in agent_ids {
        let selector = format!("rr-agent-history-row-{}", id.as_str());
        let row = window
            .debug_selector_bounds(&selector)
            .ok_or_else(|| format!("历史视口补测缺少行 {id:?}"))?;
        rows.push(row);
    }
    rows.sort_by(|left, right| left.top().as_f32().total_cmp(&right.top().as_f32()));
    Ok(rows)
}

/// 内部滚动事件必须落在列表与窗口视口的交集内；短窗下列表底部可能在窗口外，
/// 直接用 list.center() 会落到不可输入区域，滚轮无法到达列表。交集为空时返回 None，
/// 由调用方先做安全外层滚动，禁止把窗外坐标注入内部滚动。
///
/// @author kongweiguang
fn history_scroll_position(window: &mut gpui_kit::Window) -> Result<Option<(f32, f32)>, String> {
    let (_, _, _, list, viewport_height) = history_viewport_bounds(window)?;
    let viewport_width = window.viewport_size().width.as_f32();
    let visible_top = list.top().as_f32().max(0.0);
    let visible_bottom = list.bottom().as_f32().min(viewport_height);
    let visible_left = list.left().as_f32().max(0.0);
    let visible_right = list.right().as_f32().min(viewport_width);
    if visible_bottom <= visible_top || visible_right <= visible_left {
        return Ok(None);
    }
    Ok(Some((
        (visible_left + visible_right) / 2.0,
        (visible_top + visible_bottom) / 2.0,
    )))
}

/// 外层滚动事件必须同时落在窗口、外层滚动容器内并避开历史列表；落在列表上会被内层消费，
/// 不能证明外层滚动兜底有效。没有合法坐标时明确失败，不能假装命中外层。
///
/// @author kongweiguang
fn outer_scroll_position(window: &mut gpui_kit::Window) -> Result<(f32, f32), String> {
    let outer = window
        .debug_selector_bounds("rr-agent-landing-scroll")
        .ok_or_else(|| "历史视口补测缺少外层滚动".to_owned())?;
    let list = window.debug_selector_bounds("rr-agent-history-list");
    let viewport = window.viewport_size();
    let (viewport_width, viewport_height) = (viewport.width.as_f32(), viewport.height.as_f32());
    let candidates = [
        (outer.center().x.as_f32(), outer.top().as_f32() + 8.0),
        (outer.center().x.as_f32(), outer.bottom().as_f32() - 8.0),
        (outer.left().as_f32() + 8.0, outer.center().y.as_f32()),
        (outer.right().as_f32() - 8.0, outer.center().y.as_f32()),
    ];
    for (x, y) in candidates {
        if x < 0.5 || x > viewport_width - 0.5 || y < 0.5 || y > viewport_height - 0.5 {
            continue;
        }
        if x < outer.left().as_f32()
            || x > outer.right().as_f32()
            || y < outer.top().as_f32()
            || y > outer.bottom().as_f32()
        {
            continue;
        }
        if list.is_some_and(|list| {
            x >= list.left().as_f32()
                && x <= list.right().as_f32()
                && y >= list.top().as_f32()
                && y <= list.bottom().as_f32()
        }) {
            continue;
        }
        return Ok((x, y));
    }
    Err("外层滚动没有既在窗口内又避开历史列表的可输入坐标".to_owned())
}

/// 统一构造真实滚轮事件；touch_phase 用 Moved，让 GPUI 走与用户滚轮一致的命中路径。
///
/// @author kongweiguang
fn dispatch_scroll(
    window: &mut gpui_kit::Window,
    app: &mut gpui_kit::App,
    x: f32,
    y: f32,
    delta_y: f32,
) {
    let _ = window.dispatch_event(
        ScrollWheelEvent {
            position: point(px(x), px(y)),
            delta: ScrollDelta::Pixels(point(px(0.), px(delta_y))),
            touch_phase: TouchPhase::Moved,
            ..ScrollWheelEvent::default()
        }
        .to_platform_input(),
        app,
    );
}

/// 短窗下若列表整体落在窗口外，内部滚轮没有可输入坐标；先在外层滚动容器内安全下滚，
/// 直到列表与窗口出现交集，再由调用方重取锚点和行，避免把窗外点当成内部滚动。
///
/// @author kongweiguang
async fn outer_scroll_until_list_reachable(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    for _ in 0..20 {
        if window
            .update(cx, |_, window, _| history_scroll_position(window))??
            .is_some()
        {
            return Ok(());
        }
        let (x, y) = window.update(cx, |_, window, _| outer_scroll_position(window))??;
        window.update(cx, |_, window, app| {
            dispatch_scroll(window, app, x, y, -10_000.);
        })?;
        let _ = frame_signature(cx, window)?;
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
    Err(io::Error::other("外层滚动后历史列表仍不在窗口可触达区域").into())
}

/// 四条真实记录时固定视口只完整展示前三条，第四条必须落在列表可视区外；
/// 抽出供外层滚动前后重复校验，避免重取锚点后漏掉布局断言。
///
/// @author kongweiguang
fn assert_history_rows_layout(
    rows: &[Bounds<Pixels>],
    list: &Bounds<Pixels>,
) -> Result<(), Box<dyn Error>> {
    if rows.len() != 4 {
        return Err(io::Error::other(format!(
            "历史视口补测需要恰好四条真实记录，实际 {}",
            rows.len()
        ))
        .into());
    }
    for row in rows.iter().take(3) {
        if row.top().as_f32() < list.top().as_f32() - 0.5
            || row.bottom().as_f32() > list.bottom().as_f32() + 0.5
        {
            return Err(io::Error::other(format!(
                "固定视口前三条历史未完整展示：row={row:?} list={list:?}"
            ))
            .into());
        }
    }
    let fourth = rows[3];
    // 视口高度按三条行高再加 2px 边框补偿，rem1 子像素取整后第四条顶部最多露出约 2px；
    // 因此允许这段边框级露出，同时要求第四条底部仍在列表外，确保它没有被完整展示。
    if fourth.top().as_f32() < list.bottom().as_f32() - 2.5
        || fourth.bottom().as_f32() <= list.bottom().as_f32() + 0.5
    {
        return Err(io::Error::other(format!(
            "固定视口第四条历史不应完整可见：row={fourth:?} list={list:?}"
        ))
        .into());
    }
    Ok(())
}

/// 真实窗口下校验固定三行视口：四条记录时前三条完整、第四条在视口外；内部滚轮滚到底后
/// 末条在列表内可达，入口/标题/视口不移动。窗口高度不足以完整容纳固定三槽时，先做外层
/// 滚动兜底，并在同一帧复核末条同时落在列表和窗口内，不能把滚到窗口外的行当成成功。
///
/// @author kongweiguang
async fn assert_history_fixed_viewport(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    agent_ids: &[AgentSessionId],
) -> Result<&'static str, Box<dyn Error>> {
    let (picker_before, heading_before, body_before, list_before, viewport_height) =
        window.update(cx, |_, window, _| history_viewport_bounds(window))??;
    let rows_before =
        window.update(cx, |_, window, _| history_rows_bounds(window, agent_ids))??;
    assert_history_rows_layout(&rows_before, &list_before)?;
    let initially_fits_window = list_before.top().as_f32() >= -0.5
        && list_before.bottom().as_f32() <= viewport_height + 0.5;

    // 短窗下列表可能整体落在窗口外，此时任何内部滚轮坐标都不可输入；
    // 先安全外滚让列表进入窗口，再以新锚点继续，避免注入窗外坐标制造伪成功。
    let mut anchors = (picker_before, heading_before, body_before);
    if window
        .update(cx, |_, window, _| history_scroll_position(window))??
        .is_none()
    {
        outer_scroll_until_list_reachable(cx, window).await?;
        let (picker, heading, body, list, _) =
            window.update(cx, |_, window, _| history_viewport_bounds(window))??;
        let rows = window.update(cx, |_, window, _| history_rows_bounds(window, agent_ids))??;
        assert_history_rows_layout(&rows, &list)?;
        anchors = (picker, heading, body);
    }
    let (anchor_picker, anchor_heading, anchor_body) = anchors;

    // 内部列表滚动只改变列表内容位置；入口、标题和固定视口必须原地不动。
    let (scroll_x, scroll_y) = window
        .update(cx, |_, window, _| history_scroll_position(window))??
        .ok_or_else(|| io::Error::other("历史列表没有窗口内可输入的滚动坐标"))?;
    window.update(cx, |_, window, app| {
        dispatch_scroll(window, app, scroll_x, scroll_y, -10_000.);
    })?;
    let mut last_reached = false;
    for _ in 0..20 {
        let _ = frame_signature(cx, window)?;
        let (picker, heading, body, list, _) =
            window.update(cx, |_, window, _| history_viewport_bounds(window))??;
        for (label, before, after) in [
            ("入口", anchor_picker.top(), picker.top()),
            ("标题", anchor_heading.top(), heading.top()),
            ("视口", anchor_body.top(), body.top()),
        ] {
            if (before.as_f32() - after.as_f32()).abs() > 0.5 {
                return Err(io::Error::other(format!(
                    "历史内部滚动移动了{label}：before={before:?} after={after:?}"
                ))
                .into());
            }
        }
        let rows = window.update(cx, |_, window, _| history_rows_bounds(window, agent_ids))??;
        let last = rows
            .last()
            .copied()
            .ok_or_else(|| io::Error::other("滚动后历史行为空"))?;
        let within_list = last.top().as_f32() >= list.top().as_f32() - 0.5
            && last.bottom().as_f32() <= list.bottom().as_f32() + 1.5;
        let within_window =
            last.top().as_f32() >= -0.5 && last.bottom().as_f32() <= viewport_height + 1.5;
        // 正常 fit 时末条必须同时进窗口；超窗时内部滚动只证明末条在列表内可达，
        // 窗口可见性留给下面的外层滚动兜底同一帧复核。
        last_reached = within_list && (within_window || !initially_fits_window);
        if last_reached {
            break;
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
    if !last_reached {
        return Err(io::Error::other("历史列表滚到底后末条仍不可达").into());
    }

    if !initially_fits_window {
        // 固定三槽高于窗口时设计允许外层滚动兜底；滚到底后必须在同一帧证明末条
        // 同时落在列表和窗口内，不能只凭列表内部 bounds 把窗口外的行算作可达。
        let (outer_x, outer_y) =
            window.update(cx, |_, window, _| outer_scroll_position(window))??;
        window.update(cx, |_, window, app| {
            dispatch_scroll(window, app, outer_x, outer_y, -10_000.);
        })?;
        let mut bottom_reached = false;
        for _ in 0..20 {
            let _ = frame_signature(cx, window)?;
            let (_, _, _, list, _) =
                window.update(cx, |_, window, _| history_viewport_bounds(window))??;
            let rows =
                window.update(cx, |_, window, _| history_rows_bounds(window, agent_ids))??;
            let last = rows
                .last()
                .copied()
                .ok_or_else(|| io::Error::other("外层滚动后历史行为空"))?;
            let within_list = last.top().as_f32() >= list.top().as_f32() - 0.5
                && last.bottom().as_f32() <= list.bottom().as_f32() + 1.5;
            let within_window =
                last.top().as_f32() >= -0.5 && last.bottom().as_f32() <= viewport_height + 1.5;
            bottom_reached = within_list && within_window;
            if bottom_reached {
                break;
            }
            cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
        }
        if !bottom_reached {
            return Err(io::Error::other(
                "窗口高度不足且外层滚动后末条未同时落在列表和窗口可见区域内",
            )
            .into());
        }
        // 滚回顶部，checkpoint 截图保留默认状态；入口回到原 Y 也证明外层滚动可逆。
        let (restore_x, restore_y) =
            window.update(cx, |_, window, _| outer_scroll_position(window))??;
        window.update(cx, |_, window, app| {
            dispatch_scroll(window, app, restore_x, restore_y, 10_000.);
        })?;
        let mut restored = false;
        for _ in 0..20 {
            let _ = frame_signature(cx, window)?;
            let (picker, _, _, _, _) =
                window.update(cx, |_, window, _| history_viewport_bounds(window))??;
            restored = (picker.top().as_f32() - picker_before.top().as_f32()).abs() <= 0.5;
            if restored {
                break;
            }
            cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
        }
        if !restored {
            return Err(io::Error::other("外层滚动回顶后入口未回到原位置").into());
        }
    }

    // 内部列表滚回顶部，最终截图仍是默认三条完整、第四条在视口外的状态。
    // 外层回顶后列表可能再次整体落在窗口外，先安全外滚到可触达再回顶，滚轮坐标同样
    // 必须位于列表与窗口交集内，不能注入窗外点。
    let mut outer_scrolled_for_top = false;
    if window
        .update(cx, |_, window, _| history_scroll_position(window))??
        .is_none()
    {
        outer_scroll_until_list_reachable(cx, window).await?;
        outer_scrolled_for_top = true;
    }
    let (top_x, top_y) = window
        .update(cx, |_, window, _| history_scroll_position(window))??
        .ok_or_else(|| io::Error::other("历史列表回顶时没有窗口内可输入的滚动坐标"))?;
    window.update(cx, |_, window, app| {
        dispatch_scroll(window, app, top_x, top_y, 10_000.);
    })?;
    let mut top_reached = false;
    for _ in 0..20 {
        let _ = frame_signature(cx, window)?;
        let (_, _, _, list, _) =
            window.update(cx, |_, window, _| history_viewport_bounds(window))??;
        let rows = window.update(cx, |_, window, _| history_rows_bounds(window, agent_ids))??;
        let first = rows
            .first()
            .copied()
            .ok_or_else(|| io::Error::other("滚回顶部后历史行为空"))?;
        let within_list = first.top().as_f32() >= list.top().as_f32() - 0.5;
        // 超窗时列表顶部可能仍被裁剪，此时只要求首条回到列表内，不冒充整行在窗口内。
        let within_window = list.top().as_f32() < -0.5
            || (first.top().as_f32() >= -0.5 && first.bottom().as_f32() <= viewport_height + 1.5);
        top_reached = within_list && within_window;
        if top_reached {
            break;
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
    if !top_reached {
        return Err(io::Error::other("历史列表滚回顶部后首条仍不可见").into());
    }
    if outer_scrolled_for_top {
        // 为回顶临时外滚过，恢复外层顶部，避免最终截图停留在非默认位置。
        let (restore_x, restore_y) =
            window.update(cx, |_, window, _| outer_scroll_position(window))??;
        window.update(cx, |_, window, app| {
            dispatch_scroll(window, app, restore_x, restore_y, 10_000.);
        })?;
        let mut restored = false;
        for _ in 0..20 {
            let _ = frame_signature(cx, window)?;
            let (picker, _, _, _, _) =
                window.update(cx, |_, window, _| history_viewport_bounds(window))??;
            restored = (picker.top().as_f32() - picker_before.top().as_f32()).abs() <= 0.5;
            if restored {
                break;
            }
            cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
        }
        if !restored {
            return Err(io::Error::other("回顶后外层滚动未恢复默认位置").into());
        }
    }

    Ok(if initially_fits_window {
        "four_real_local_history_rows_fit_window_internal_scroll_kept_entry_fixed_three_complete_rows_and_last_row_reachable_in_window"
    } else {
        "four_real_local_history_rows_overflowed_window_outer_scroll_fallback_last_row_reachable_in_window_and_outer_scroll_restored"
    })
}

/// 在连续性流程末尾补一条真实本地历史，使固定视口同时覆盖默认三条和第四条内部滚动；
/// 新会话沿用 extra_agent_sessions 的精确身份归档/关闭，不按 Tab 扫描。
///
/// @author kongweiguang
#[allow(clippy::too_many_arguments)]
async fn verify_history_fixed_viewport(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &super::RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    resources: &mut ContinuityResources,
    target: ContinuityTarget<'_>,
    launcher_id: &str,
    output_marker: &str,
    known_agent_ids: &[AgentSessionId],
) -> Result<(), Box<dyn Error>> {
    resources.pending_creation = Some("历史视口补测 Agent");
    let started = window.update(cx, |_, _, app| {
        root.update(app, |root, cx| match target {
            ContinuityTarget::Fixture(host_id) => {
                root.start_right_rail_acceptance_agent(host_id, launcher_id, cx)
            }
            ContinuityTarget::Local => {
                root.start_right_rail_acceptance_agent_local(launcher_id, cx)
            }
        })
    })??;
    if !started {
        return Err(io::Error::other("历史视口补测没有发起新建").into());
    }
    let created = wait_for_agent_view(
        cx,
        root,
        window,
        target,
        None,
        |view| {
            !known_agent_ids.contains(&view.session.agent_session_id)
                && view
                    .terminal
                    .as_ref()
                    .is_some_and(|terminal| terminal.lifecycle == SessionLifecycle::Running)
        },
        "history viewport fourth Agent",
    )
    .await?;
    let fourth_agent_id = created.session.agent_session_id.clone();
    let fourth_terminal_id = created
        .terminal
        .as_ref()
        .map(|terminal| terminal.session_id.clone())
        .ok_or_else(|| io::Error::other("历史视口补测没有真实终端"))?;
    resources
        .extra_agent_sessions
        .push((fourth_agent_id.clone(), fourth_terminal_id.clone()));
    resources.pending_creation = None;
    wait_for_agent_root_text(
        cx,
        root,
        window,
        target,
        &fourth_agent_id,
        &fourth_terminal_id,
        &[format!("{output_marker}-ready")],
        "history viewport fourth Agent ready",
    )
    .await?;

    if !window.update(cx, |_, _, app| {
        root.update(app, |root, cx| match target {
            ContinuityTarget::Fixture(host_id) => {
                root.prepare_right_rail_acceptance_agent(host_id, cx)
            }
            ContinuityTarget::Local => root.prepare_right_rail_acceptance_local_agent(cx),
        })
    })?? {
        return Err(io::Error::other("历史视口补测无法返回新建视图").into());
    }
    wait_for_agent_panel_idle(cx, root, window, target, "历史视口补测返回列表").await?;

    let mut agent_ids = known_agent_ids.to_vec();
    agent_ids.push(fourth_agent_id);
    let outcome = assert_history_fixed_viewport(cx, window, &agent_ids).await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-continuity-history-fixed-viewport",
        outcome,
    )
    .await?;
    Ok(())
}

/// 读取稳定帧的实际表面，证明自然内容按 main 居中且封顶，入场期间的变换不作为最终布局。
///
/// @author kongweiguang
fn assert_agent_delete_geometry(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, _| {
        let bounds = window
            .debug_selector_bounds("dialog-0")
            .ok_or_else(|| io::Error::other("Agent 删除确认实际表面未挂载"))?;
        let viewport = window.viewport_size();
        let expected_y = (viewport.height.as_f32() - bounds.size.height.as_f32()) / 2.0;
        let expected_x = (viewport.width.as_f32() - bounds.size.width.as_f32()) / 2.0;
        if (bounds.origin.y.as_f32() - expected_y).abs() > 0.75
            || (bounds.origin.x.as_f32() - expected_x).abs() > 0.75
            || bounds.size.height.as_f32() > 288.75
        {
            return Err(io::Error::other(format!(
                "Agent 删除确认未遵守 main 的居中/限高：{bounds:?}"
            )));
        }
        Ok(())
    })??;
    Ok(())
}

/// 激活 Agent 工具并先走正式新建会话入口，消除 fixture 可能携带的其它目标历史选择。
///
/// @author kongweiguang
async fn activate_agent_panel(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    host_id: &HostId,
) -> Result<(), Box<dyn Error>> {
    // compact 抽屉的活动工具取自上一帧渲染投影，激活后同一更新内立刻 prepare 会读到旧工具，
    // 在窄屏误报“不是当前验收面板”；先强制绘制一帧，与其它工具验收的激活路径保持一致。
    window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.activate_right_rail_tool_for_acceptance(
                ToolRailToolId::AgentLauncher,
                window,
                root_cx,
            );
        })
    })?;
    let _ = frame_signature(cx, window)?;
    let selected = window.update(cx, |_, _, app| {
        root.read(app).right_rail_acceptance_active_tool() == Some(ToolRailToolId::AgentLauncher)
    })?;
    if !selected {
        return Err(io::Error::other("Agent 工具未能成为当前验收面板").into());
    }
    let ready = window.update(cx, |_, _, app| {
        root.update(app, |root, root_cx| {
            root.prepare_right_rail_acceptance_agent(host_id, root_cx)
        })
    })??;
    if !ready {
        return Err(
            io::Error::other("Agent panel could not enter its production new-chat state").into(),
        );
    }
    super::wait_for_panel_readiness(
        cx,
        root,
        window,
        ToolRailToolId::AgentLauncher,
        host_id,
        "Agent",
    )
    .await
}

/// 按稳定 Agent ID 读取 production history，并在加载結束后将真实状态错误带回验收报告。
///
/// @author kongweiguang
async fn wait_for_agent_view(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    target: ContinuityTarget<'_>,
    agent_session_id: Option<&AgentSessionId>,
    predicate: impl Fn(&AgentSessionView) -> bool,
    label: &str,
) -> Result<AgentSessionView, Box<dyn Error>> {
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let (view, operation_state) = window.update(cx, |_, _, app| {
            root.update(app, |root, root_cx| {
                let (view, state) = match target {
                    ContinuityTarget::Fixture(host_id) => {
                        let view = root.right_rail_acceptance_agent_view(
                            host_id,
                            agent_session_id,
                            root_cx,
                        )?;
                        let state =
                            root.right_rail_acceptance_agent_operation_state(host_id, root_cx)?;
                        (view, state)
                    }
                    ContinuityTarget::Local => {
                        let view =
                            root.right_rail_acceptance_agent_view_local(agent_session_id, root_cx)?;
                        let state =
                            root.right_rail_acceptance_agent_operation_state_local(root_cx)?;
                        (view, state)
                    }
                };
                Ok::<_, String>((view, state))
            })
        })??;
        if let Some(view) = view.filter(&predicate) {
            return Ok(view);
        }
        let (busy, loading, sessions_verified, status) = operation_state;
        if !busy
            && !loading
            && sessions_verified
            && let Some(status) = status.filter(|message| {
                message.contains("失败")
                    || message.contains("不可用")
                    || message.contains("拒绝")
                    || message.contains("failed")
                    || message.contains("unavailable")
            })
        {
            return Err(io::Error::other(format!("Agent {label} failed: {status}")).into());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("timed out waiting for Agent {label}")).into());
        }
        let _ = frame_signature(cx, window)?;
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// 等待生产面板完成一次历史核验，避免“返回列表”触发的 reload 尚未结束就打开入口选择。
/// 只读取现有 operation state，不触发 reload 或选择。
///
/// @author kongweiguang
async fn wait_for_agent_panel_idle(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    target: ContinuityTarget<'_>,
    label: &str,
) -> Result<(), Box<dyn Error>> {
    poll_root(cx, root, window, label, |root, _, cx| {
        let (busy, loading, verified, status) = match target {
            ContinuityTarget::Fixture(host_id) => {
                root.right_rail_acceptance_agent_operation_state(host_id, cx)?
            }
            ContinuityTarget::Local => {
                root.right_rail_acceptance_agent_operation_state_local(cx)?
            }
        };
        if busy || loading {
            return Ok(None);
        }
        if !verified {
            return Err(status.unwrap_or_else(|| "Agent 面板未完成历史核验".to_owned()));
        }
        Ok(Some(()))
    })
    .await?;
    Ok(())
}

/// Keeps the expected output and terminal lifecycle together without weakening identity checks.
struct AgentOutputExpectation<'a> {
    marker: &'a str,
    lifecycle: SessionLifecycle,
}
/// 仅接受当前选中 Agent 和相同 PTY renderer snapshot 中可见的唯一输出 marker 与终态。
///
/// @author kongweiguang
async fn wait_for_agent_output(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    target: ContinuityTarget<'_>,
    agent_session_id: &AgentSessionId,
    terminal_session_id: &SessionId,
    expectation: AgentOutputExpectation<'_>,
) -> Result<(SessionLifecycle, String), Box<dyn Error>> {
    let AgentOutputExpectation {
        marker: output_marker,
        lifecycle: expected_lifecycle,
    } = expectation;
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let output = window.update(cx, |_, _, app| {
            root.update(app, |root, _| match target {
                ContinuityTarget::Fixture(host_id) => root.right_rail_acceptance_agent_output(
                    host_id,
                    agent_session_id,
                    terminal_session_id,
                ),
                ContinuityTarget::Local => root.right_rail_acceptance_agent_output_local(
                    agent_session_id,
                    terminal_session_id,
                ),
            })
        })??;
        if let Some((lifecycle, text)) = output
            && lifecycle == expected_lifecycle
            && text.contains(output_marker)
        {
            return Ok((lifecycle, text));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "Agent terminal did not return marker {output_marker} with {expected_lifecycle:?} lifecycle"
            ))
            .into());
        }
        let _ = frame_signature(cx, window)?;
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// 文件操作统一交给共享 ApplicationTaskExecutor 的阻塞 worker，避免同步创建/写入/删除触发文件时
/// 阻塞 GPUI 前台；内层 `Result<T, String>` 保留路径和底层原因，避免 AppError 的脱敏文案丢失清理证据。
///
/// @author kongweiguang
async fn run_continuity_file_task<T, F>(
    services: &AppServices,
    operation: F,
) -> Result<T, Box<dyn Error>>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    let outcome = services
        .executor
        .run(move |_| async move {
            let joined = tokio::task::spawn_blocking(operation)
                .await
                .map_err(|error| {
                    AppError::infrastructure_with_source("Agent 连续性文件 worker 未能完成", error)
                })?;
            Ok::<_, AppError>(joined.map_err(|error| error.to_string()))
        })
        .await?;
    outcome.map_err(|detail| io::Error::other(detail).into())
}

/// 每轮在隔离输出目录下创建独占运行目录；`create_dir` 而非 `create_dir_all` 让碰撞显式失败，
/// 有限次改名重试后仍失败就按错误返回，避免复用或覆盖其它轮次的触发文件。
///
/// @author kongweiguang
async fn create_continuity_run_directory(
    services: &AppServices,
    capture: &super::RightRailCaptureContext,
) -> Result<PathBuf, Box<dyn Error>> {
    let base = capture.output_directory.clone();
    if !base.is_absolute() {
        return Err(io::Error::other(
            "Agent 连续性输出目录必须是绝对路径，拒绝在子进程不同 cwd 下解析 stage 文件",
        )
        .into());
    }
    run_continuity_file_task(services, move || {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| io::Error::other(format!("无法读取运行时间戳：{error}")))?
            .as_millis();
        for attempt in 0..8 {
            let candidate = base.join(format!(
                "agent-continuity-{}-{stamp}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => return Ok(candidate),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(io::Error::other(format!(
                        "无法创建 Agent 连续性运行目录 {}：{error}",
                        candidate.display()
                    )));
                }
            }
        }
        Err(io::Error::other(
            "Agent 连续性运行目录连续碰撞，拒绝复用既有目录",
        ))
    })
    .await
}

/// 空文件只承担存在性信号：先在本轮独占目录内 create_new 一个 `.pending`，关闭句柄后原子改名
/// 为 `trigger.stage`，使 pwsh 永远看不到半成品。失败只清理本轮确实创建成功的 pending 文件。
///
/// @author kongweiguang
fn publish_continuity_stage_file(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("Agent 连续性触发目标已存在，拒绝覆盖 {}", path.display()),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let pending = path.with_extension("pending");
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending)?;
    // 先关闭独占句柄，再发布路径；脚本不会在文件仍被占用时尝试删除。
    drop(file);
    match fs::rename(&pending, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let message = format!("无法发布 Agent 连续性触发文件 {}：{error}", path.display());
            match fs::remove_file(&pending) {
                Ok(()) => Err(io::Error::other(message)),
                Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => {
                    Err(io::Error::other(message))
                }
                Err(cleanup_error) => Err(io::Error::other(format!(
                    "{message}；清理 {} 失败：{cleanup_error}",
                    pending.display()
                ))),
            }
        }
    }
}

/// 清理只以本轮独占运行目录为边界：删除可能存在的 stage/pending 后移除空目录。
/// 目录内出现非本轮文件时 remove_dir 会失败并按错误报告，不使用 remove_dir_all。
///
/// @author kongweiguang
fn cleanup_continuity_stage_files(directory: &Path) -> io::Result<()> {
    for name in ["trigger.stage", "trigger.pending"] {
        let path = directory.join(name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(io::Error::other(format!(
                    "无法清理本轮 Agent 触发文件 {}：{error}",
                    path.display()
                )));
            }
        }
    }
    fs::remove_dir(directory).map_err(|error| {
        io::Error::other(format!(
            "无法清理 Agent 连续性运行目录 {}：{error}",
            directory.display()
        ))
    })
}

/// 发布无内容触发信号；调用方不根据返回值推断文件归属，清理统一以本轮 run dir 为边界。
///
/// @author kongweiguang
async fn publish_continuity_stage(
    services: &AppServices,
    path: &Path,
) -> Result<(), Box<dyn Error>> {
    let path = path.to_path_buf();
    run_continuity_file_task(services, move || publish_continuity_stage_file(&path)).await
}

/// 在应用执行器的阻塞 worker 中清理本轮 stage 文件与空 run dir，保留真实清理错误。
///
/// @author kongweiguang
async fn cleanup_continuity_run_directory(
    services: &AppServices,
    directory: &Path,
) -> Result<(), Box<dyn Error>> {
    let directory = directory.to_path_buf();
    run_continuity_file_task(services, move || cleanup_continuity_stage_files(&directory)).await
}

/// 在写入任何夹具文件前确认输出目录内的 `config` 就是本进程实际使用的配置根，
/// 防止命令行指定另一份配置时把触发文件和报告写到不同隔离域。
///
/// @author kongweiguang
async fn ensure_continuity_isolation(
    services: &AppServices,
    capture: &super::RightRailCaptureContext,
) -> Result<(), Box<dyn Error>> {
    let output = capture.output_directory.clone();
    let config = capture.config_directory.clone();
    if !output.is_absolute() {
        return Err(io::Error::other(
            "Agent 连续性输出目录必须是绝对路径，确保 config 与 stage 属于同一身份",
        )
        .into());
    }
    run_continuity_file_task(services, move || {
        let expected = fs::canonicalize(output.join("config"))?;
        let actual = fs::canonicalize(config)?;
        if actual != expected {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Agent 连续性验收要求配置根位于本轮隔离输出目录的 config 下",
            ));
        }
        Ok(())
    })
    .await
}

/// 失败时显式关闭同一 Agent PTY 并回读注册表；`close_until` 的超时/取消/清理错误通过 `?`
/// 原样计入回收失败，后续注册表显示 `None`/`Closed`/`Exited` 也不能把它改写为成功。
/// close 只有在 transport、reader 与 child exit observer 均收敛后才返回 Ok，不依赖脚本自退出。
///
/// @author kongweiguang
async fn reclaim_continuity_agent(
    services: &AppServices,
    launcher_key: Option<&str>,
    known_agent_session_id: Option<&AgentSessionId>,
    known_terminal_session_id: Option<&SessionId>,
) -> Result<String, String> {
    let agents = services
        .agents
        .clone()
        .ok_or_else(|| "连续性验收缺少 AgentService，无法回收集群".to_owned())?;
    let sessions = services.sessions.clone();
    let launcher_key = launcher_key.map(str::to_owned);
    let known_agent = known_agent_session_id.cloned();
    let known_terminal = known_terminal_session_id.cloned();
    services
        .executor
        .run(move |cancellation| async move {
            // 启动请求可能在投影完成前失败；按本轮 launcher key 有界轮询注册表，
            // 找到后立即关闭，找不到才报告“没有注册 PTY”，避免把仍在登记的进程静默遗留。
            let discovery_deadline = SessionDeadline::now() + Duration::from_secs(3);
            let mut agent_id = known_agent;
            if agent_id.is_none()
                && let Some(key) = launcher_key.as_deref()
            {
                loop {
                    agent_id = agents
                        .list(cancellation.clone())
                        .await?
                        .into_iter()
                        .find(|view| {
                            view.session
                                .launcher_key
                                .as_ref()
                                .is_some_and(|candidate| candidate.as_str() == key)
                        })
                        .map(|view| view.session.agent_session_id);
                    if agent_id.is_some() || SessionDeadline::now() >= discovery_deadline {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            let mut terminal_id = known_terminal;
            let mut terminal_discovery_error = None;
            if terminal_id.is_none()
                && let Some(agent_id) = agent_id.clone()
            {
                let terminal_deadline = SessionDeadline::now() + Duration::from_secs(3);
                loop {
                    // 探测期间允许重试，但放弃时必须带回最后一次读取错误；否则会把服务故障
                    // 报成“未暴露 PTY”，让真正的登记/读取失败从回收证据里消失。
                    match agents.get(agent_id.clone(), cancellation.clone()).await {
                        Ok(view) => {
                            terminal_id = view
                                .terminal
                                .as_ref()
                                .map(|terminal| terminal.session_id.clone());
                            terminal_discovery_error = None;
                        }
                        Err(error) => terminal_discovery_error = Some(error),
                    }
                    if terminal_id.is_some() || SessionDeadline::now() >= terminal_deadline {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            let Some(terminal_id) = terminal_id else {
                return if agent_id.is_some() {
                    // 读取失败优先原样返回（保留类型与 source）；只有读取成功但确实没有
                    // terminal 时才用固定冲突文案，避免把服务故障报成“未暴露 PTY”。
                    match terminal_discovery_error {
                        Some(error)
                            if error.public_code()
                                != kerminal_application::PublicErrorCode::NotFound =>
                        {
                            Err(error)
                        }
                        _ => Err(AppError::conflict("Agent 会话已登记但未暴露可回收的 PTY")),
                    }
                } else {
                    Ok("本轮没有注册 Agent PTY".to_owned())
                };
            };
            let close = sessions
                .close_until(
                    terminal_id.clone(),
                    SessionDeadline::now() + Duration::from_secs(10),
                )
                .await;
            // close 失败时不再读注册表，避免“后来看到 Exited/Closed”改写真实回收错误；
            // close 成功后仍读回生命周期，作为同一 session ID 的补充证据。
            let remaining = if close.is_ok() {
                sessions
                    .list(SessionFilter::default())
                    .await
                    .map(|sessions| {
                        sessions
                            .into_iter()
                            .find(|session| session.session_id == terminal_id)
                            .map(|session| session.lifecycle)
                    })
            } else {
                Ok(None)
            };
            continuity_reclaim_result(&terminal_id, close, remaining)
        })
        .await
        .map_err(|error| error.to_string())
}

/// `close_until` 只有在 transport/channel 关闭、reader 与 child exit observer 都收敛后才返回 Ok；
/// 其错误必须原样优先返回，注册表后来显示 `None`/`Closed`/`Exited` 不能覆盖超时或取消失败。
/// close 成功后只有注册表条目已被移除或进入 `Closed` 才算回收：`Exited` 只是子进程结束，
/// ManagedSession 仍留在注册表并继续持有 writer、引擎与待清理所有权，不能据此丢弃回收 ID。
///
/// @author kongweiguang
fn continuity_reclaim_result(
    terminal_id: &SessionId,
    close: AppResult<()>,
    remaining: AppResult<Option<SessionLifecycle>>,
) -> AppResult<String> {
    close?;
    match remaining? {
        None | Some(SessionLifecycle::Closed) => Ok(format!("{} 已回收", terminal_id)),
        Some(_) => Err(AppError::conflict("Agent PTY 在显式关闭期限后仍未回收")),
    }
}

/// 单项矩阵创建与关闭的独立期限；一个挂起的 shell 不能吃掉整轮验收预算。
/// 超时只取消该子项，manager-owned 创建任务仍保留未发布 transport 的清理所有权。
///
/// @author kongweiguang
const LOCAL_PTY_MATRIX_CREATE_TIMEOUT: Duration = Duration::from_secs(10);
const LOCAL_PTY_MATRIX_CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

/// 矩阵中的固定 shell 标签与启动参数；program 缺失时输出 skipped，绝不把缺失当成成功。
struct LocalPtyMatrixTarget {
    label: &'static str,
    program: Option<OsString>,
    args: Vec<OsString>,
}

/// 用系统固定目录和验收脚本显式传入的绝对路径构造矩阵，不按 PATH 搜索，
/// 避免 shim/alias 混入真实安装路径的比较；cmd 与 Windows PowerShell 只作诊断对照。
///
/// @author kongweiguang
fn local_pty_shell_matrix_targets() -> Vec<LocalPtyMatrixTarget> {
    let cmd = std::env::var_os("COMSPEC")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_file())
        .or_else(|| {
            let fallback = PathBuf::from(r"C:\Windows\System32\cmd.exe");
            fallback.is_file().then_some(fallback)
        });
    let system_root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let powershell = system_root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let mut targets = vec![
        LocalPtyMatrixTarget {
            label: "cmd",
            program: cmd.map(OsString::from),
            args: ["/d", "/q", "/c", "exit", "0"]
                .into_iter()
                .map(OsString::from)
                .collect(),
        },
        LocalPtyMatrixTarget {
            label: "powershell",
            program: (powershell.is_absolute() && powershell.is_file())
                .then(|| powershell.into_os_string()),
            args: [
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "exit 0",
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
        },
    ];
    for (label, variable) in [
        ("pwsh_real", "KERMINAL_ACCEPTANCE_PWSH_REAL"),
        ("pwsh_alias", "KERMINAL_ACCEPTANCE_PWSH_ALIAS"),
    ] {
        let program = std::env::var_os(variable)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && path.is_file())
            .map(PathBuf::into_os_string);
        targets.push(LocalPtyMatrixTarget {
            label,
            program,
            args: [
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "exit 0",
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
        });
    }
    targets
}

/// 在真实 acceptance boot 的同一 owned Job、private desktop 与 ConPTY 下，用固定短命命令
/// 分别创建 cmd、Windows PowerShell、真实 pwsh 和可选 App Execution Alias pwsh 的本地 PTY。
/// 只用于区分 run7 的 0xC0070005 发生在哪类可执行文件/启动分支；成功项按同一 SessionId
/// 显式关闭并回读，失败项只输出固定 shell 标签与现有探针行，不输出命令、路径或环境。
/// 创建/关闭各有独立期限，超时或取消只影响当前子项，不阻塞 UI 线程。
///
/// @author kongweiguang
async fn run_local_pty_shell_matrix(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &super::RightRailCaptureContext,
) -> Result<(), Box<dyn Error>> {
    // 与 local_pty 探针共用同一显式开关；普通启动和未批准验收不产生矩阵会话。
    if std::env::var("KERMINAL_ACCEPTANCE_PTY_PROBE")
        .ok()
        .as_deref()
        != Some("1")
    {
        return Ok(());
    }
    let services = window.update(cx, |_, _, app| {
        root.read(app).right_rail_acceptance_agent_services()
    })?;
    let sessions = services.sessions.clone();
    let working_directory = capture.output_directory.join("pty-matrix");
    services
        .executor
        .run(move |cancellation| async move {
            // 目录创建也放在共享 executor 的异步路径，避免同步 IO 占用 GPUI 前台。
            tokio::fs::create_dir_all(&working_directory)
                .await
                .map_err(|error| {
                    AppError::infrastructure_with_source(
                        "本地 PTY 矩阵工作目录创建失败",
                        error,
                    )
                })?;
            let mut close_failure: Option<AppError> = None;
            for target in local_pty_shell_matrix_targets() {
                let label = target.label;
                let Some(program) = target.program else {
                    eprintln!("KERMINAL_ACCEPTANCE_PTY_MATRIX shell={label} result=skipped");
                    continue;
                };
                eprintln!("KERMINAL_ACCEPTANCE_PTY_MATRIX shell={label} result=start");
                let request = CreateSessionRequest {
                    target: SessionTarget::Local(LocalSessionTarget {
                        program,
                        args: target.args,
                        working_directory: Some(working_directory.clone()),
                        environment: Default::default(),
                    }),
                    // 矩阵会话只借用 UI owner 的元数据，不创建 Tab；随机身份避免与真实 pane 冲突。
                    owner: SessionOwner::Ui {
                        tab_id: TabId::new(),
                        pane_id: PaneId::new(),
                    },
                    size: TerminalSize::default(),
                    initial_palette: None,
                };
                let create_cancellation = cancellation.child_token();
                let create = sessions.create_cancellable(request, create_cancellation.clone());
                tokio::pin!(create);
                // 超时/取消只取消子 token 并丢弃等待方；manager-owned 创建任务继续清理未发布
                // transport，因此不能在 UI 线程同步等待或把超时当成已回收。
                let outcome = tokio::select! {
                    biased;
                    result = &mut create => Some(result),
                    _ = tokio::time::sleep(LOCAL_PTY_MATRIX_CREATE_TIMEOUT) => {
                        create_cancellation.cancel();
                        eprintln!("KERMINAL_ACCEPTANCE_PTY_MATRIX shell={label} result=timeout");
                        None
                    }
                    _ = cancellation.cancelled() => {
                        create_cancellation.cancel();
                        eprintln!("KERMINAL_ACCEPTANCE_PTY_MATRIX shell={label} result=cancelled");
                        None
                    }
                };
                match outcome {
                    Some(Ok(session)) => {
                        let session_id = session.session_id;
                        eprintln!(
                            "KERMINAL_ACCEPTANCE_PTY_MATRIX shell={label} result=ok session={session_id}"
                        );
                        let close = sessions
                            .close_until(
                                session_id.clone(),
                                SessionDeadline::now() + LOCAL_PTY_MATRIX_CLOSE_TIMEOUT,
                            )
                            .await;
                        let remaining = if close.is_ok() {
                            sessions
                                .list(SessionFilter::default())
                                .await
                                .map(|listed| {
                                    listed
                                        .into_iter()
                                        .find(|item| item.session_id == session_id)
                                        .map(|item| item.lifecycle)
                                })
                        } else {
                            Ok(None)
                        };
                        match continuity_reclaim_result(&session_id, close, remaining) {
                            Ok(_) => {
                                eprintln!("KERMINAL_ACCEPTANCE_PTY_MATRIX shell={label} close=ok")
                            }
                            Err(error) => {
                                eprintln!(
                                    "KERMINAL_ACCEPTANCE_PTY_MATRIX shell={label} close=failed"
                                );
                                if close_failure.is_none() {
                                    close_failure = Some(error);
                                }
                            }
                        }
                    }
                    Some(Err(_)) => {
                        // local_pty 失败路径已输出固定 stage/code 探针行；这里只补 shell 标签，
                        // 不拼接错误正文，避免命令、路径或环境进入验收日志。
                        eprintln!("KERMINAL_ACCEPTANCE_PTY_MATRIX shell={label} result=failed");
                    }
                    None => {}
                }
            }
            match close_failure {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
        .await?;
    Ok(())
}

/// 同一 Agent/PTY 的服务侧身份快照；切换前后逐项比较，避免把“仍能看到旧画面”当成进程未变。
struct AgentContinuityIdentity {
    terminal_session_id: SessionId,
    lifecycle: SessionLifecycle,
    status: AgentSessionStatus,
    target: Option<AgentTargetBinding>,
    process_id: Option<u32>,
    resume_recovery_present: bool,
    agent_session_ids: BTreeSet<String>,
}

/// 仅本地模板终端的连续性入口：先通过生产本地 Tab 创建真实会话，再进入同一主体。
/// 不读取 fixture manifest、known_hosts、vault/key，也不创建任何远程目标。
///
/// @author kongweiguang
pub(super) async fn run_local_agent_continuity_flow(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &super::RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
) -> Result<(), Box<dyn Error>> {
    // 矩阵只读取显式验收开关，并在真实本地 Tab 创建前完成；它只证明 spawn/回收分支，
    // 不能替代后续生产 Agent 流程的通过判定。
    run_local_pty_shell_matrix(cx, root, window, capture).await?;
    let (fixture_tab_id, a1_pane_id, a1_session_id) = ensure_local_continuity_tab(cx, root, window)
        .await
        .map_err(|error| {
            // A1 创建请求与注册读回之间失败时没有任何可证明的 SessionId；不能按 Tab 扫描回收，
            // 只能依赖组合根 shutdown 与私有 Job 兜底，因此错误文本必须明确这一点。
            io::Error::other(format!(
                "本地 Agent 验收 Tab 创建失败；A1 未登记唯一身份，未按会话回收，仅依赖组合根 shutdown 与 Job 兜底：{error}"
            ))
        })?;
    activate_local_agent_panel(cx, root, window).await?;
    let mut resources = ContinuityResources {
        a1_session_id: Some(a1_session_id),
        a1_pane_id: Some(a1_pane_id),
        ..ContinuityResources::default()
    };
    run_agent_continuity_body(
        cx,
        root,
        window,
        capture,
        records,
        ContinuityTarget::Local,
        fixture_tab_id,
        &mut resources,
    )
    .await
}

/// 先用生产“新建本地终端”入口得到一个真实 Session，再确认它已成为可见且唯一的本地验收 Tab。
/// 未就绪只继续轮询，避免把生产创建中的短暂空窗当成失败或伪造 pane。
///
/// @author kongweiguang
async fn ensure_local_continuity_tab(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
) -> Result<(TabId, PaneId, SessionId), Box<dyn Error>> {
    // 新建入口受正常设置加载和工作区恢复门禁约束；先等同一生产状态，避免一次性请求被启动阶段拒绝。
    poll_root(
        cx,
        root,
        window,
        "等待本地 Agent 验收设置与工作区就绪",
        |root, _, _| {
            Ok(root
                .right_rail_acceptance_local_tab_creation_ready()
                .then_some(()))
        },
    )
    .await?;
    let tabs_before = window.update(cx, |_, _, app| {
        root.update(app, |root, _| {
            root.right_rail_acceptance_shell_snapshot()
                .tab_ids()
                .to_vec()
        })
    })?;
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.request_right_rail_acceptance_local_tab(window, cx)
        })
    })?;
    // request 返回后立即保存一次初态；status 可能在 4 秒 toast 后被清空，轮询期间保留最后一次非空值。
    let initial_state = window.update(cx, |_, _, app| {
        root.read(app)
            .right_rail_acceptance_local_tab_creation_state()
    })?;
    let mut last_state = String::from("no poll");
    let mut last_non_empty_status: Option<String> = None;
    let created = poll_root(
        cx,
        root,
        window,
        "创建并激活本地 Agent 验收 Tab",
        |root, _, _| {
            let snapshot = root.right_rail_acceptance_shell_snapshot();
            let added = snapshot
                .tab_ids()
                .iter()
                .filter(|tab_id| !tabs_before.contains(tab_id))
                .cloned()
                .collect::<Vec<TabId>>();
            let ready = root.right_rail_acceptance_local_terminal_ready_tab_id()?;
            let panes = added
                .first()
                .map(|tab_id| root.right_rail_acceptance_terminal_panes(tab_id))
                .unwrap_or_default();
            // 超时回执保留最后投影，区分 added/active/ready/panes 哪一项未满足；
            // 不延长 30s 期限，也不放宽后续挂载断言。
            let (creation_state, status) =
                root.right_rail_acceptance_local_tab_creation_state();
            if let Some(status) = status.filter(|status| !status.trim().is_empty()) {
                last_non_empty_status = Some(status);
            }
            last_state = format!(
                "{creation_state} tabs_before={tabs_before:?} tabs={:?} active={:?} added={added:?} ready={ready:?} panes={panes:?}",
                snapshot.tab_ids(),
                snapshot.active_tab()
            );
            if added.len() != 1 {
                return Ok(None);
            }
            let Some(added) = added.into_iter().next() else {
                return Ok(None);
            };
            if snapshot.active_tab() != Some(&added) {
                return Ok(None);
            }
            let tab_ready = ready.as_ref() == Some(&added);
            if !tab_ready {
                return Ok(None);
            }
            // 记录本轮真实创建的 A1 pane/session 身份；后续收尾只按这个 ID 关闭，不按 Tab
            // 全量回收，切 Tab 后也能核对原 pane 没有被替换。
            let mut panes = root.right_rail_acceptance_terminal_panes(&added);
            if panes.len() != 1 {
                return Ok(None);
            }
            let (pane_id, session_id) = panes.remove(0);
            Ok(Some((added, pane_id, session_id)))
        },
    )
    .await
    .map_err(|error| {
        io::Error::other(format!(
            "创建并激活本地 Agent 验收 Tab 失败；request 后初态：{initial_state:?}；最后投影：{last_state}；最后非空 status：{last_non_empty_status:?}；原始错误：{error}"
        ))
    })?;
    Ok(created)
}

/// 激活 Agent 工具后等待生产面板挂载并暴露同一 Tab 的本地目标；真正的 launcher 选择与
/// 会话错误仍由后续 catalog/readback 步骤严格返回，这里只等待界面就绪。
///
/// @author kongweiguang
async fn activate_local_agent_panel(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
) -> Result<(), Box<dyn Error>> {
    // 本地入口与 SSH 入口共用 compact 投影规则：先让抽屉渲染出 Agent，再执行生产“新建对话”，
    // 否则窄屏会沿用上一帧的活动工具并拒绝准备面板。
    window.update(cx, |_, window, app| {
        root.update(app, |root, root_cx| {
            root.activate_right_rail_tool_for_acceptance(
                ToolRailToolId::AgentLauncher,
                window,
                root_cx,
            );
        })
    })?;
    let _ = frame_signature(cx, window)?;
    let selected = window.update(cx, |_, _, app| {
        root.read(app).right_rail_acceptance_active_tool() == Some(ToolRailToolId::AgentLauncher)
    })?;
    if !selected {
        return Err(io::Error::other(
            "local Agent panel did not become the active acceptance tool",
        )
        .into());
    }
    let prepared = window.update(cx, |_, _, app| {
        root.update(app, |root, root_cx| {
            root.prepare_right_rail_acceptance_local_agent(root_cx)
        })
    })??;
    if !prepared {
        return Err(io::Error::other(
            "local Agent panel could not enter its production new-chat state",
        )
        .into());
    }
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let ready = window.update(cx, |_, _, app| {
            root.read(app).right_rail_acceptance_local_agent_ready(app)
        })?;
        if ready {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("等待本地 Agent 面板就绪超时").into());
        }
        let _ = frame_signature(cx, window)?;
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// 归档/关闭/目录清理的主错误必须保留类型和 source，同时把清理失败附在同一条链上。
/// 单边失败直接返回原错误，避免为了统一格式丢掉 AppError 类别或底层 cause。
///
/// @author kongweiguang
#[derive(Debug)]
struct ContinuityCombinedError {
    primary: Box<dyn Error>,
    cleanup: Vec<String>,
}

impl Display for ContinuityCombinedError {
    /// 输出主错误原文与全部清理原因；清理详情不替代主失败，便于定位先发生的错误。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.primary, formatter)?;
        if !self.cleanup.is_empty() {
            write!(formatter, "; 连续性收尾未证明：{}", self.cleanup.join("；"))?;
        }
        Ok(())
    }
}

impl Error for ContinuityCombinedError {
    /// 主错误始终作为 source，调用方仍可沿原始错误链读取类型和超时原因。
    ///
    /// @author kongweiguang
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.primary.as_ref())
    }
}

/// 失败路径的清理错误不覆盖原始流程错误；两者都存在时合并展示并保留主错误链。
///
/// @author kongweiguang
fn combine_continuity_failure(failure: Box<dyn Error>, cleanup: Vec<String>) -> Box<dyn Error> {
    if cleanup.is_empty() {
        failure
    } else {
        Box::new(ContinuityCombinedError {
            primary: failure,
            cleanup,
        })
    }
}

/// 归档、会话回收和目录清理按顺序合并：任一侧成功都不吞掉另一侧失败，
/// 单边失败保持原错误类型/source，双边失败把清理原因附到主错误链。
///
/// @author kongweiguang
fn merge_continuity_outcomes(
    primary: Result<(), Box<dyn Error>>,
    cleanup: Result<(), Box<dyn Error>>,
    cleanup_label: &str,
) -> Result<(), Box<dyn Error>> {
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(primary), Err(cleanup)) => Err(combine_continuity_failure(
            primary,
            vec![format!("{cleanup_label}：{cleanup}")],
        )),
    }
}

/// 连续性的公共主体只负责生产动作、身份读回、回收与目录清理；目标差异由 scope 决定。
/// 失败时只回收本轮已记录身份的 Agent/A1/A2/B 会话，任一清理错误都与原始失败合并保留；
/// 成功时同样显式 close_until 并回读，无法证明收敛时返回错误，由组合根 shutdown 继续兜底。
///
/// @author kongweiguang
#[allow(clippy::too_many_arguments)]
async fn run_agent_continuity_body(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &super::RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    target: ContinuityTarget<'_>,
    fixture_tab_id: TabId,
    resources: &mut ContinuityResources,
) -> Result<(), Box<dyn Error>> {
    // 直接复用 Root 组合根注入的同一服务实例，隐藏期间按 session ID 读写仍走生产会话注册表；
    // 隔离核对与夹具文件操作也走它的阻塞 worker，避免同步 IO 占用 GPUI 前台。
    let services = window.update(cx, |_, _, app| {
        root.read(app).right_rail_acceptance_agent_services()
    })?;
    // 清单只读、配置在启动时复制到本轮输出目录；这里再次核对实际配置根，避免写入另一隔离域。
    ensure_continuity_isolation(&services, capture).await?;
    let run_directory = create_continuity_run_directory(&services, capture).await?;
    let stage_path = run_directory.join("trigger.stage");

    let flow = run_continuity_agent_session(
        cx,
        root,
        window,
        capture,
        records,
        &services,
        &stage_path,
        resources,
        target,
        &fixture_tab_id,
    )
    .await;

    if let Err(error) = flow {
        let mut cleanup_failures = Vec::new();
        if let Err(detail) = reclaim_continuity_agent(
            &services,
            resources.launcher_key.as_deref(),
            resources.agent_session_id.as_ref(),
            resources.agent_terminal_session_id.as_ref(),
        )
        .await
        {
            cleanup_failures.push(format!("Agent 回收失败：{detail}"));
        }
        // 入口选择面新建的额外 Agent 只按已读回身份回收；未读回身份的创建窗口由
        // pending_creation 明确报告，仍不按 Tab/owner 扫描。
        for (agent_session_id, terminal_session_id) in &resources.extra_agent_sessions {
            if let Err(detail) = reclaim_continuity_agent(
                &services,
                resources.launcher_key.as_deref(),
                Some(agent_session_id),
                Some(terminal_session_id),
            )
            .await
            {
                cleanup_failures.push(format!("入口新建 Agent 回收失败：{detail}"));
            }
        }
        if let Err(detail) =
            reclaim_tracked_sessions_if_present(&services, resources.local_terminal_session_ids())
                .await
        {
            cleanup_failures.push(format!("本轮本地会话回收失败：{detail}"));
        }
        // 创建请求已发出但尚未读回唯一身份时，任何按 Tab/owner 扫描都可能误伤用户会话；
        // 这里不伪装逐会话回收，明确只依赖组合根 shutdown 与私有 Job 兜底。
        if let Some(phase) = resources.pending_creation {
            cleanup_failures.push(format!(
                "{phase} 创建窗口未登记唯一会话身份，未逐会话回收；仅依赖组合根 shutdown 与 Job 兜底"
            ));
        }
        if let Err(cleanup) = cleanup_continuity_run_directory(&services, &run_directory).await {
            cleanup_failures.push(format!("运行目录清理失败：{cleanup}"));
        }
        return Err(combine_continuity_failure(error, cleanup_failures));
    }

    let mut archived: Result<(), Box<dyn Error>> = match resources.agent_session_id.as_ref() {
        Some(agent_session_id) => {
            archive_continuity_agent(cx, root, window, target, agent_session_id).await
        }
        None => Err(io::Error::other("Agent 连续性流程成功但缺少可归档的 Agent 会话身份").into()),
    };
    // 入口选择面显式新建/无 Running 直接新建的 Agent 也必须归档，避免验收成功后留下
    // 仍在运行的额外 CLI；任一侧失败都保留原错误并继续尝试其余清理。
    for (agent_session_id, _) in &resources.extra_agent_sessions {
        archived = merge_continuity_outcomes(
            archived,
            archive_continuity_agent(cx, root, window, target, agent_session_id).await,
            "入口新建 Agent 归档",
        );
    }
    // 归档可能已移除 Agent 会话；按注册表存在性跳过已消失的 ID，再对本轮仍有身份的
    // 本地 PTY 执行 close_until，避免把“已不在注册表”误报成关闭失败。
    let session_cleanup =
        reclaim_tracked_sessions_if_present(&services, resources.all_terminal_session_ids())
            .await
            .map(|_| ());
    let directory_cleanup = cleanup_continuity_run_directory(&services, &run_directory).await;
    let outcome = merge_continuity_outcomes(archived, session_cleanup, "本轮会话回收");
    merge_continuity_outcomes(outcome, directory_cleanup, "运行目录清理")
}

/// 根据注册表实际存在的本轮 ID 逐个 close_until；不按 Tab 或 owner 扫描，因此不会关闭
/// 不属于本轮的 fixture/用户会话。已不在注册表的 ID 视为无需再关闭，而不是伪造一次成功关闭。
/// 失败时保留首个类型化 `AppError` 作为主错误，全部会话的失败原因经 `ContinuityCombinedError`
/// 的 detail 链保留：`AppError` 的公开文案按安全约定只能是源码常量，动态明细不能进入公开消息。
///
/// @author kongweiguang
async fn reclaim_tracked_sessions_if_present(
    services: &AppServices,
    session_ids: Vec<SessionId>,
) -> Result<String, Box<dyn Error>> {
    if session_ids.is_empty() {
        return Ok("本轮没有可追踪的本地会话".to_owned());
    }
    let sessions = services.sessions.clone();
    let outcome = services
        .executor
        .run(move |_cancellation| async move {
            let listed = sessions.list(SessionFilter::default()).await?;
            let present = listed
                .into_iter()
                .map(|session| session.session_id)
                .collect::<BTreeSet<_>>();
            let mut details = Vec::new();
            let mut failure_reasons = Vec::new();
            let mut first_failure: Option<AppError> = None;
            for session_id in session_ids {
                if !present.contains(&session_id) {
                    details.push(format!("{session_id} 已不在注册表"));
                    continue;
                }
                match reclaim_continuity_session_direct(&sessions, &session_id).await {
                    Ok(detail) => details.push(detail),
                    Err(error) => {
                        failure_reasons.push(format!("{session_id}：{error}"));
                        if first_failure.is_none() {
                            first_failure = Some(error);
                        }
                    }
                }
            }
            // 失败原因作为数据带回外层折叠：executor 的结果错误类型只接受 `AppError`，
            // 而其公开文案必须是固定常量，两者都无法在闭包内携带逐会话的动态明细。
            let outcome = match first_failure {
                None => Ok(details.join("；")),
                Some(primary) => Err((primary, failure_reasons.join("；"))),
            };
            Ok::<_, AppError>(outcome)
        })
        .await?;
    match outcome {
        Ok(detail) => Ok(detail),
        Err((primary, reasons)) => Err(combine_continuity_failure(
            Box::new(primary),
            vec![format!("本轮会话回收失败：{reasons}")],
        )),
    }
}

/// 关闭与收敛判定本身不启动新 task，便于额外会话清理循环复用同一语义。
///
/// @author kongweiguang
async fn reclaim_continuity_session_direct(
    sessions: &std::sync::Arc<dyn kerminal_application::sessions::SessionService>,
    session_id: &SessionId,
) -> AppResult<String> {
    let close = sessions
        .close_until(
            session_id.clone(),
            SessionDeadline::now() + Duration::from_secs(10),
        )
        .await;
    let remaining = if close.is_ok() {
        sessions.list(SessionFilter::default()).await.map(|listed| {
            listed
                .into_iter()
                .find(|item| item.session_id == *session_id)
                .map(|item| item.lifecycle)
        })
    } else {
        Ok(None)
    };
    continuity_reclaim_result(session_id, close, remaining)
}

/// 连续性主体只负责生产动作和身份读回；资源回收、目录清理和归档由外层统一处理，
/// 保证任一阶段失败都不会跳过本轮 PTY 或触发文件的清理。
///
/// @author kongweiguang
#[allow(clippy::too_many_arguments)]
async fn run_continuity_agent_session(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &super::RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    services: &AppServices,
    stage_path: &Path,
    resources: &mut ContinuityResources,
    target: ContinuityTarget<'_>,
    fixture_tab_id: &TabId,
) -> Result<(), Box<dyn Error>> {
    let setup = window.update(cx, |_, _, app| {
        root.update(app, |root, root_cx| match target {
            ContinuityTarget::Fixture(host_id) => root
                .configure_right_rail_acceptance_staged_agent(
                    host_id,
                    stage_path.to_path_buf(),
                    root_cx,
                ),
            ContinuityTarget::Local => root.configure_right_rail_acceptance_staged_local_agent(
                stage_path.to_path_buf(),
                root_cx,
            ),
        })
    })??;
    let custom_key = format!("custom:{}", setup.launcher_id());
    resources.launcher_key = Some(custom_key.clone());
    poll_root(
        cx,
        root,
        window,
        "Agent 连续性 launcher CAS 与 catalog 读回",
        |root, _, cx| match target {
            ContinuityTarget::Fixture(host_id) => root
                .right_rail_acceptance_agent_launcher_ready(host_id, setup.launcher_id(), cx)
                .map(|ready| ready.then_some(())),
            ContinuityTarget::Local => root
                .right_rail_acceptance_agent_launcher_ready_local(setup.launcher_id(), cx)
                .map(|ready| ready.then_some(())),
        },
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-continuity-launcher-ready",
        "isolated_staged_custom_launcher_saved_and_read_back_through_production_catalog",
    )
    .await?;

    let started = window.update(cx, |_, _, app| {
        root.update(app, |root, root_cx| match target {
            ContinuityTarget::Fixture(host_id) => {
                root.start_right_rail_acceptance_agent(host_id, setup.launcher_id(), root_cx)
            }
            ContinuityTarget::Local => {
                root.start_right_rail_acceptance_agent_local(setup.launcher_id(), root_cx)
            }
        })
    })??;
    if !started {
        return Err(io::Error::other(format!(
            "{} Agent continuity create action did not start",
            target.label()
        ))
        .into());
    }
    let created = wait_for_agent_view(
        cx,
        root,
        window,
        target,
        None,
        |view| {
            view.session
                .launcher_key
                .as_ref()
                .is_some_and(|key| key.as_str() == custom_key)
        },
        "continuity Agent projection",
    )
    .await?;
    let agent_session_id = created.session.agent_session_id.clone();
    resources.agent_session_id = Some(agent_session_id.clone());
    let running = wait_for_agent_view(
        cx,
        root,
        window,
        target,
        Some(&agent_session_id),
        |view| {
            view.terminal
                .as_ref()
                .is_some_and(|terminal| terminal.lifecycle == SessionLifecycle::Running)
        },
        "running continuity Agent PTY",
    )
    .await?;
    let terminal_session_id = running
        .terminal
        .as_ref()
        .map(|terminal| terminal.session_id.clone())
        .ok_or_else(|| io::Error::other("continuity Agent has no real terminal session"))?;
    resources.agent_terminal_session_id = Some(terminal_session_id.clone());
    let marker = setup.output_marker().to_owned();
    wait_for_agent_root_text(
        cx,
        root,
        window,
        target,
        &agent_session_id,
        &terminal_session_id,
        &[format!("{marker}-psversion:7"), format!("{marker}-ready")],
        "continuity Agent pwsh 7 ready marker",
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-continuity-started",
        "staged_custom_agent_started_on_pwsh7_with_exact_marker_and_no_profile",
    )
    .await?;

    let panel_before = window.update(cx, |_, _, app| {
        root.read(app).right_rail_acceptance_agent_panel_state(app)
    })?;
    if panel_before.panel_tab_id() != Some(&fixture_tab_id)
        || panel_before.selected_agent_session_id() != Some(&agent_session_id)
        || panel_before.selected_terminal_session_id() != Some(&terminal_session_id)
        || panel_before.terminal_lifecycle() != Some(SessionLifecycle::Running)
    {
        return Err(io::Error::other(format!(
            "continuity Agent panel did not expose the exact running selection: {panel_before:?}"
        ))
        .into());
    }
    let identity_before =
        read_agent_continuity_identity(services, &agent_session_id, &terminal_session_id).await?;
    assert_continuity_identity(&identity_before, &identity_before, "before tab switch")?;

    // 在历史继续涉及的持久化前覆盖真实失焦输出，避免无关绑定写入失败掩盖该回归。
    run_native_window_activity(
        cx,
        root,
        window,
        capture,
        records,
        services,
        target,
        &agent_session_id,
        &terminal_session_id,
        Some(""),
        "agent-continuity-native-window-activity-output",
        "native_second_top_level_window_kept_agent_output_current",
    )
    .await?;
    // 新建成功后生产面板停留在会话视图；主题与居中取证必须先经生产“新建对话”动作
    // 回到入口落地页，否则入口列未挂载，不能把会话视图误当成 landing。
    if !window.update(cx, |_, _, app| {
        root.update(app, |root, cx| match target {
            ContinuityTarget::Fixture(host_id) => {
                root.prepare_right_rail_acceptance_agent(host_id, cx)
            }
            ContinuityTarget::Local => root.prepare_right_rail_acceptance_local_agent(cx),
        })
    })?? {
        return Err(io::Error::other("入口主题取证前无法返回新建视图").into());
    }
    wait_for_agent_panel_idle(cx, root, window, target, "入口主题取证前返回列表").await?;
    capture_agent_landing_themes(cx, root, window, capture, records).await?;
    run_native_window_activity(
        cx,
        root,
        window,
        capture,
        records,
        services,
        target,
        &agent_session_id,
        &terminal_session_id,
        None,
        "agent-continuity-native-window-activity-landing",
        "native_second_top_level_window_kept_landing_and_history_filter",
    )
    .await?;

    // 旧会话只从历史行进入：继续原运行会话，保持同一 Agent/终端身份，不新建或重绑。
    if !window.update(cx, |_, _, app| {
        root.update(app, |root, cx| match target {
            ContinuityTarget::Fixture(host_id) => {
                root.continue_right_rail_acceptance_agent(host_id, &agent_session_id, cx)
            }
            ContinuityTarget::Local => {
                root.continue_right_rail_acceptance_agent_local(&agent_session_id, cx)
            }
        })
    })?? {
        return Err(io::Error::other("历史行继续没有受理运行中会话").into());
    }
    let mut continue_state = String::from("no poll");
    poll_root(
        cx,
        root,
        window,
        "历史行继续保留原 Agent/终端身份",
        |root, _, cx| {
            let state = root.right_rail_acceptance_agent_panel_state(cx);
            let (busy, loading, verified, status) = match target {
                ContinuityTarget::Fixture(host_id) => {
                    root.right_rail_acceptance_agent_operation_state(host_id, cx)?
                }
                ContinuityTarget::Local => {
                    root.right_rail_acceptance_agent_operation_state_local(cx)?
                }
            };
            // 超时回执保留最后投影，区分选择未更新或生命周期不是 Running；
            // 不延长期限，也不放宽后续身份断言。
            continue_state = format!(
                "busy={busy} loading={loading} verified={verified} status={status:?} selected_agent={:?} selected_terminal={:?} lifecycle={:?} target_terminal={:?} target_generation={:?} session_count={}",
                state.selected_agent_session_id(),
                state.selected_terminal_session_id(),
                state.terminal_lifecycle(),
                state.target_terminal_session_id(),
                state.target_generation(),
                state.session_count(),
            );
            Ok((state.selected_agent_session_id() == Some(&agent_session_id)
                && state.selected_terminal_session_id() == Some(&terminal_session_id)
                && state.terminal_lifecycle() == Some(SessionLifecycle::Running))
            .then_some(()))
        },
    )
    .await
    .map_err(|error| {
        io::Error::other(format!(
            "历史行继续保留原 Agent/终端身份失败；最后投影：{continue_state}；原始错误：{error}"
        ))
    })?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-continuity-history-continued",
        "history_continue_kept_the_same_agent_and_terminal_identity",
    )
    .await?;
    wait_for_agent_panel_idle(cx, root, window, target, "历史行继续后刷新完成").await?;
    // 历史继续可能按现有规则持久化首选终端并推进绑定代次；以该动作后的状态作为
    // 后续 Tab 切换连续性基线，避免把显式动作误判为切换期间的重绑。
    let panel_before = window.update(cx, |_, _, app| {
        root.read(app).right_rail_acceptance_agent_panel_state(app)
    })?;
    let identity_before =
        read_agent_continuity_identity(services, &agent_session_id, &terminal_session_id).await?;

    let suffix = &setup.output_marker()[9..];
    let draft_token = format!("continuity-draft-{suffix}");
    write_agent_terminal_input(services, &terminal_session_id, draft_token.clone()).await?;
    wait_for_agent_root_text(
        cx,
        root,
        window,
        target,
        &agent_session_id,
        &terminal_session_id,
        &[draft_token.clone()],
        "Agent CLI draft echo before tab switch",
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-continuity-draft-before-switch",
        "same_pty_echoed_the_unsubmitted_draft_while_the_fixture_tab_was_active",
    )
    .await?;

    // 第二段原生 activity 在草稿已回显、选择已收起后执行，证明草稿/过滤/身份不因窗口切换丢失。
    run_native_window_activity(
        cx,
        root,
        window,
        capture,
        records,
        services,
        target,
        &agent_session_id,
        &terminal_session_id,
        Some(&draft_token),
        "agent-continuity-native-window-activity-draft",
        "native_second_top_level_window_kept_draft_filter_and_identity",
    )
    .await?;

    // 本地隔离入口额外覆盖同一 Tab 的真实分屏：用生产 Split/Focus 事件创建并切换到 A2，
    // 再由服务身份、面板选择和草稿回读证明 Agent 仍绑定 A1，最后显式回收 A2。
    if matches!(target, ContinuityTarget::Local) {
        verify_same_tab_second_pane(
            cx,
            root,
            window,
            capture,
            records,
            services,
            resources,
            fixture_tab_id,
            &agent_session_id,
            &terminal_session_id,
            &draft_token,
        )
        .await?;
    }

    // 切 Tab 前固定 A1 的进程身份；恢复原 Tab 后必须仍是同一 SessionId/PID，且该 Tab 只能
    // 回到原 A1 pane，不能由新建 pane 或另一个本地会话冒充。
    let a1_identity_before = match resources.a1_session_id.as_ref() {
        Some(a1_session_id) => {
            let identity = read_continuity_session_identity(services, a1_session_id).await?;
            if identity.lifecycle != SessionLifecycle::Running || identity.process_id.is_none() {
                return Err(io::Error::other(format!(
                    "A1 在切 Tab 前不是带进程身份的 Running 会话：{identity:?}"
                ))
                .into());
            }
            Some(identity)
        }
        None => None,
    };

    let tabs_before = window.update(cx, |_, _, app| {
        root.update(app, |root, _| {
            root.right_rail_acceptance_shell_snapshot()
                .tab_ids()
                .to_vec()
        })
    })?;
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.request_right_rail_acceptance_local_tab(window, cx)
        })
    })?;
    resources.pending_creation = Some("第二个本地 Tab 的 B");
    let other_tab = poll_root(
        cx,
        root,
        window,
        "创建并激活第二个本地 Tab",
        |root, _, _| {
            let snapshot = root.right_rail_acceptance_shell_snapshot();
            let added = snapshot
                .tab_ids()
                .iter()
                .filter(|tab_id| !tabs_before.contains(tab_id))
                .cloned()
                .collect::<Vec<TabId>>();
            // 只接受恰好新增一个 Tab 且它已成为活动 Tab，避免把重复创建或激活失败当成成功。
            if added.len() != 1 {
                return Ok(None);
            }
            let added = added.into_iter().next().expect("checked one added tab");
            Ok((snapshot.active_tab() == Some(&added)).then_some(added))
        },
    )
    .await?;
    let b_tab = other_tab.clone();
    let b_session = poll_root(
        cx,
        root,
        window,
        "第二个 Tab 的真实本地会话就绪",
        |root, _, _| {
            let panes = root.right_rail_acceptance_terminal_panes(&b_tab);
            if panes.len() != 1 {
                return Ok(None);
            }
            Ok(Some(panes[0].1.clone()))
        },
    )
    .await?;
    if Some(&b_session) == resources.a1_session_id.as_ref()
        || Some(&b_session) == resources.agent_terminal_session_id.as_ref()
        || Some(&b_session) == resources.a2_session_id.as_ref()
    {
        return Err(io::Error::other("第二个 Tab 未暴露独立于本轮既有 PTY 的真实会话").into());
    }
    resources.b_session_id = Some(b_session.clone());
    resources.pending_creation = None;
    // B 必须是带真实进程号的 Running 会话；A2 的关闭前身份保留在 resources 中，
    // 这里同时排除 session ID 和 PID 复用，避免把 A2 的残留或重建误认为新 Tab 的 B。
    let b_identity = read_continuity_session_identity(services, &b_session).await?;
    if b_identity.lifecycle != SessionLifecycle::Running || b_identity.process_id.is_none() {
        return Err(io::Error::other(format!(
            "第二个 Tab 的 B 不是带进程身份的 Running 会话：{b_identity:?}"
        ))
        .into());
    }
    if b_reuses_closed_a2(&b_identity, resources.a2_closed_identity.as_ref()) {
        return Err(io::Error::other(format!(
            "B 复用了已关闭 A2 的 SessionId 或进程身份：B {b_identity:?}, A2 {:?}",
            resources.a2_closed_identity
        ))
        .into());
    }
    let other_tab_for_panel = other_tab.clone();
    poll_root(
        cx,
        root,
        window,
        "第二个 Tab 的 Agent 面板换挂",
        |root, _, cx| {
            let state = root.right_rail_acceptance_agent_panel_state(cx);
            Ok((state.panel_tab_id() == Some(&other_tab_for_panel)
                && state.selected_agent_session_id().is_none()
                && state.panel_entity_id() != panel_before.panel_entity_id())
            .then_some(()))
        },
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-continuity-other-tab-active",
        "second_production_tab_became_active_with_its_own_agent_panel_and_no_selection",
    )
    .await?;

    let identity_hidden =
        read_agent_continuity_identity(services, &agent_session_id, &terminal_session_id).await?;
    assert_continuity_identity(
        &identity_before,
        &identity_hidden,
        "while the other tab is active",
    )?;

    let hidden_marker = format!("{marker}-stage");
    publish_continuity_stage(services, stage_path).await?;
    let hidden_text = wait_for_hidden_agent_text(
        cx,
        services,
        &agent_session_id,
        &terminal_session_id,
        &hidden_marker,
    )
    .await?;
    // 窄栏/200% 下 marker 可能跨行；与等待函数一致按去空白文本复核。
    let normalized_hidden: String = hidden_text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if !normalized_hidden.contains(&hidden_marker) {
        return Err(io::Error::other(
            "hidden Agent PTY did not return the synchronized stage output",
        )
        .into());
    }
    // 第二个受控阶段只证明后台 PTY 能再次响应同一触发文件；这是有界的两阶段验证，
    // 不把两次成功夸大为“无限持续输出”。模板每次消费触发文件后都会再输出一个 stage 标记。
    publish_continuity_stage(services, stage_path).await?;
    // 第二次调用只验证后台 PTY 能再次响应同一触发文件，输出内容已由上一阶段断言覆盖；
    // 超时或读取失败仍由函数内部返回错误，因此这里丢弃返回值是安全的。
    let _hidden_text = wait_for_hidden_stage_count(
        cx,
        services,
        &agent_session_id,
        &terminal_session_id,
        &hidden_marker,
        2,
    )
    .await?;
    let active_after_hidden = window.update(cx, |_, _, app| {
        root.update(app, |root, _| {
            let snapshot = root.right_rail_acceptance_shell_snapshot();
            snapshot.active_tab().cloned()
        })
    })?;
    if active_after_hidden.as_ref() != Some(&other_tab) {
        return Err(io::Error::other(
            "Agent hidden output was observed after the second tab stopped being active",
        )
        .into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-continuity-hidden-output-observed",
        "two_bounded_synchronized_stage_outputs_read_from_the_agent_pty_while_the_other_tab_remained_active",
    )
    .await?;

    let restored = window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.restore_right_rail_acceptance_fixture_tab(&fixture_tab_id, window, cx)
        })
    })?;
    if !restored {
        return Err(
            io::Error::other("continuity flow could not restore the original fixture tab").into(),
        );
    }
    poll_root(
        cx,
        root,
        window,
        "切回原 Tab 并恢复 Agent 面板",
        |root, _, cx| {
            let state = root.right_rail_acceptance_agent_panel_state(cx);
            Ok((state.panel_tab_id() == Some(&fixture_tab_id)
                && state.panel_entity_id() == panel_before.panel_entity_id()
                && state.selected_agent_session_id() == Some(&agent_session_id)
                && state.selected_terminal_session_id() == Some(&terminal_session_id)
                && state.terminal_lifecycle() == Some(SessionLifecycle::Running))
            .then_some(()))
        },
    )
    .await?;
    let returned_text = wait_for_agent_root_text(
        cx,
        root,
        window,
        target,
        &agent_session_id,
        &terminal_session_id,
        &[
            format!("{marker}-ready"),
            draft_token.clone(),
            hidden_marker.clone(),
        ],
        "complete Agent output after switching back",
    )
    .await?;
    // 右栏终端较窄，marker 可能跨行；与 wait_for_agent_root_text 一样按去空白文本复核，
    // 避免把渲染折行误判成切 Tab 后输出或草稿丢失。
    let normalized_returned: String = returned_text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if !normalized_returned.contains(&draft_token) || !normalized_returned.contains(&hidden_marker)
    {
        return Err(io::Error::other(
            "Agent output or draft was lost while another tab was active",
        )
        .into());
    }
    let panel_after = window.update(cx, |_, _, app| {
        root.read(app).right_rail_acceptance_agent_panel_state(app)
    })?;
    if panel_after.panel_entity_id() != panel_before.panel_entity_id()
        || panel_after.selected_agent_session_id() != panel_before.selected_agent_session_id()
        || panel_after.selected_terminal_session_id() != panel_before.selected_terminal_session_id()
        || panel_after.target_generation() != panel_before.target_generation()
        || panel_after.target_terminal_session_id() != panel_before.target_terminal_session_id()
        || panel_after.session_count() != panel_before.session_count()
        || panel_after.history_mode() != panel_before.history_mode()
    {
        return Err(io::Error::other(format!(
            "continuity panel identity changed across tabs: before {panel_before:?}, after {panel_after:?}"
        ))
        .into());
    }
    let identity_after =
        read_agent_continuity_identity(services, &agent_session_id, &terminal_session_id).await?;
    assert_continuity_identity(&identity_before, &identity_after, "after switching back")?;
    // 切回原 Tab 后复核 A1 的 pane/session 与 PID 连续性；仅看到旧输出或面板仍选中同一
    // SessionId 不足以证明原 pane 进程没有被替换。
    if let (Some(a1_pane_id), Some(a1_session_id), Some(a1_before)) = (
        resources.a1_pane_id.as_ref(),
        resources.a1_session_id.as_ref(),
        a1_identity_before.as_ref(),
    ) {
        let panes = window.update(cx, |_, _, app| {
            root.read(app)
                .right_rail_acceptance_terminal_panes(&fixture_tab_id)
        })?;
        if panes != vec![(a1_pane_id.clone(), a1_session_id.clone())] {
            return Err(io::Error::other(format!(
                "切回原 Tab 后 A1 pane/session 身份改变：expected {:?}/{:?}, actual {panes:?}",
                a1_pane_id, a1_session_id
            ))
            .into());
        }
        let a1_after = read_continuity_session_identity(services, a1_session_id).await?;
        if !continuity_running_identity_continues(a1_before, &a1_after) {
            return Err(io::Error::other(format!(
                "切回原 Tab 后 A1 的 Running/PID 身份改变：before {a1_before:?}, after {a1_after:?}"
            ))
            .into());
        }
    }
    if let Some(b_session_id) = resources.b_session_id.as_ref() {
        let b_after = read_continuity_session_identity(services, b_session_id).await?;
        if !continuity_running_identity_continues(&b_identity, &b_after) {
            return Err(io::Error::other(format!(
                "切回原 Tab 后 B 的 Running/PID 身份改变：before {b_identity:?}, after {b_after:?}"
            ))
            .into());
        }
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-continuity-returned",
        "same_panel_entity_agent_pty_target_binding_and_complete_output_returned",
    )
    .await?;

    write_agent_terminal_input(services, &terminal_session_id, "\r".to_owned()).await?;
    wait_for_agent_root_text(
        cx,
        root,
        window,
        target,
        &agent_session_id,
        &terminal_session_id,
        &[format!("{marker}-line:{draft_token}")],
        "submitted Agent draft after switching back",
    )
    .await?;
    let return_stage = format!("return-{suffix}");
    write_agent_terminal_input(services, &terminal_session_id, format!("{return_stage}\r")).await?;
    wait_for_agent_root_text(
        cx,
        root,
        window,
        target,
        &agent_session_id,
        &terminal_session_id,
        &[format!("{marker}-line:{return_stage}")],
        "post-return Agent output",
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-continuity-resumed-input",
        "draft_was_submitted_and_new_input_was_accepted_by_the_same_agent_pty",
    )
    .await?;

    // 返回入口后“进入”始终新建：获得新的 Agent/终端身份，不恢复原会话；随后退出额外会话，
    // 保证后续“无 Running”步骤不会被它干扰。
    if !window.update(cx, |_, _, app| {
        root.update(app, |root, cx| match target {
            ContinuityTarget::Fixture(host_id) => {
                root.prepare_right_rail_acceptance_agent(host_id, cx)
            }
            ContinuityTarget::Local => root.prepare_right_rail_acceptance_local_agent(cx),
        })
    })?? {
        return Err(io::Error::other("入口新建前无法返回新建视图").into());
    }
    wait_for_agent_panel_idle(cx, root, window, target, "入口新建前返回列表").await?;
    resources.pending_creation = Some("入口新建 Agent");
    if !window.update(cx, |_, _, app| {
        root.update(app, |root, cx| match target {
            ContinuityTarget::Fixture(host_id) => {
                root.start_right_rail_acceptance_agent(host_id, setup.launcher_id(), cx)
            }
            ContinuityTarget::Local => {
                root.start_right_rail_acceptance_agent_local(setup.launcher_id(), cx)
            }
        })
    })?? {
        return Err(io::Error::other("入口没有发起新建").into());
    }
    let new_chat = wait_for_agent_view(
        cx,
        root,
        window,
        target,
        None,
        |view| {
            view.session.agent_session_id != agent_session_id
                && view
                    .terminal
                    .as_ref()
                    .is_some_and(|terminal| terminal.lifecycle == SessionLifecycle::Running)
        },
        "entry new chat",
    )
    .await?;
    let new_agent_session_id = new_chat.session.agent_session_id.clone();
    let new_terminal_session_id = new_chat
        .terminal
        .as_ref()
        .map(|terminal| terminal.session_id.clone())
        .ok_or_else(|| io::Error::other("入口新建没有真实终端"))?;
    if new_agent_session_id == agent_session_id || new_terminal_session_id == terminal_session_id {
        return Err(io::Error::other("入口新建复用了原 Agent 或终端身份").into());
    }
    resources.extra_agent_sessions.push((
        new_agent_session_id.clone(),
        new_terminal_session_id.clone(),
    ));
    resources.pending_creation = None;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-continuity-entry-new-chat",
        "entry_created_a_distinct_running_agent_and_terminal",
    )
    .await?;

    // 先等额外 CLI 的 ready marker，再发送 exit，避免把启动中的输入缓冲误判为退出失败。
    wait_for_agent_root_text(
        cx,
        root,
        window,
        target,
        &new_agent_session_id,
        &new_terminal_session_id,
        &[format!("{}-ready", setup.output_marker())],
        "entry explicit new chat marker",
    )
    .await?;
    write_agent_terminal_input(services, &new_terminal_session_id, "exit\r".to_owned()).await?;
    wait_for_agent_output(
        cx,
        root,
        window,
        target,
        &new_agent_session_id,
        &new_terminal_session_id,
        AgentOutputExpectation {
            marker: setup.output_marker(),
            lifecycle: SessionLifecycle::Exited,
        },
    )
    .await?;
    // 显式新建的 CLI 退出后，面板可能仍在刷新历史；先等 busy/loading 收敛并重新核验，
    // 再切回原运行会话，避免把刷新窗口误判成“无法切回”。
    wait_for_agent_panel_idle(cx, root, window, target, "入口显式新建退出后刷新完成").await?;
    if !window.update(cx, |_, _, app| {
        root.update(app, |root, cx| match target {
            ContinuityTarget::Fixture(host_id) => {
                root.continue_right_rail_acceptance_agent(host_id, &agent_session_id, cx)
            }
            ContinuityTarget::Local => {
                root.continue_right_rail_acceptance_agent_local(&agent_session_id, cx)
            }
        })
    })?? {
        return Err(io::Error::other("入口新建后无法从历史切回原运行会话").into());
    }
    poll_root(
        cx,
        root,
        window,
        "入口新建后从历史切回原 Agent",
        |root, _, cx| {
            let state = root.right_rail_acceptance_agent_panel_state(cx);
            Ok(
                (state.selected_agent_session_id() == Some(&agent_session_id)
                    && state.selected_terminal_session_id() == Some(&terminal_session_id)
                    && state.terminal_lifecycle() == Some(SessionLifecycle::Running))
                .then_some(()),
            )
        },
    )
    .await?;

    write_agent_terminal_input(services, &terminal_session_id, "exit\r".to_owned()).await?;
    let (lifecycle, _) = wait_for_agent_output(
        cx,
        root,
        window,
        target,
        &agent_session_id,
        &terminal_session_id,
        AgentOutputExpectation {
            marker: setup.output_marker(),
            lifecycle: SessionLifecycle::Exited,
        },
    )
    .await?;
    if lifecycle != SessionLifecycle::Exited {
        return Err(io::Error::other("continuity Agent did not exit normally").into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-continuity-exited",
        "same_agent_pty_accepted_exit_and_reached_exited_after_resuming_from_the_other_tab",
    )
    .await?;

    // 停止后无 Running（历史仍在）：“进入”仍只新建，不恢复已结束记录。
    if !window.update(cx, |_, _, app| {
        root.update(app, |root, cx| match target {
            ContinuityTarget::Fixture(host_id) => {
                root.prepare_right_rail_acceptance_agent(host_id, cx)
            }
            ContinuityTarget::Local => root.prepare_right_rail_acceptance_local_agent(cx),
        })
    })?? {
        return Err(io::Error::other("无 Running 入口新建前无法返回新建视图").into());
    }
    wait_for_agent_panel_idle(cx, root, window, target, "无 Running 入口新建前返回列表").await?;
    resources.pending_creation = Some("无 Running 入口新建 Agent");
    if !window.update(cx, |_, _, app| {
        root.update(app, |root, cx| match target {
            ContinuityTarget::Fixture(host_id) => {
                root.start_right_rail_acceptance_agent(host_id, setup.launcher_id(), cx)
            }
            ContinuityTarget::Local => {
                root.start_right_rail_acceptance_agent_local(setup.launcher_id(), cx)
            }
        })
    })?? {
        return Err(io::Error::other("无 Running 时“进入”没有直接新建").into());
    }
    let direct_chat = wait_for_agent_view(
        cx,
        root,
        window,
        target,
        None,
        |view| {
            view.session.agent_session_id != agent_session_id
                && view
                    .terminal
                    .as_ref()
                    .is_some_and(|terminal| terminal.lifecycle == SessionLifecycle::Running)
        },
        "entry direct create without running chat",
    )
    .await?;
    let direct_agent_session_id = direct_chat.session.agent_session_id.clone();
    let direct_terminal_session_id = direct_chat
        .terminal
        .as_ref()
        .map(|terminal| terminal.session_id.clone())
        .ok_or_else(|| io::Error::other("无 Running 入口直接新建没有真实终端"))?;
    if direct_agent_session_id == agent_session_id
        || direct_terminal_session_id == terminal_session_id
        || resources
            .extra_agent_sessions
            .iter()
            .any(|(extra_agent_id, extra_terminal_id)| {
                *extra_agent_id == direct_agent_session_id
                    || *extra_terminal_id == direct_terminal_session_id
            })
    {
        return Err(io::Error::other(
            "无 Running 入口直接新建复用了原会话或已退出额外会话的 Agent/PTY 身份",
        )
        .into());
    }
    resources.extra_agent_sessions.push((
        direct_agent_session_id.clone(),
        direct_terminal_session_id.clone(),
    ));
    resources.pending_creation = None;
    wait_for_agent_root_text(
        cx,
        root,
        window,
        target,
        &direct_agent_session_id,
        &direct_terminal_session_id,
        &[format!("{}-ready", setup.output_marker())],
        "entry direct create marker",
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "无 Running 入口直接新建并选中新会话",
        |root, _, cx| {
            let state = root.right_rail_acceptance_agent_panel_state(cx);
            Ok(
                (state.selected_agent_session_id() == Some(&direct_agent_session_id)
                    && state.terminal_lifecycle() == Some(SessionLifecycle::Running))
                .then_some(()),
            )
        },
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-continuity-entry-after-exit-direct-create",
        "entry_created_a_new_running_chat_when_no_running_chat_remained",
    )
    .await?;

    // 四条真实历史下补测固定视口：默认三条完整、第四条在内部滚动范围外，滚轮可达末行。
    let mut known_agent_ids = vec![agent_session_id.clone()];
    known_agent_ids.extend(
        resources
            .extra_agent_sessions
            .iter()
            .map(|(agent_id, _)| agent_id.clone()),
    );
    verify_history_fixed_viewport(
        cx,
        root,
        window,
        capture,
        records,
        resources,
        target,
        setup.launcher_id(),
        setup.output_marker(),
        &known_agent_ids,
    )
    .await?;
    Ok(())
}

/// 同一 Tab 内用生产 Split/Focus 事件创建第二个真实本地 pane，验证 Agent 仍绑定 A1 且草稿、
/// 面板实体和首选绑定不变，最后经生产 pane 关闭路径回收 A2 并回读注册表与 pane 收敛。
/// A2 的创建、焦点和回收都不经过验收 setter 伪造，也不依赖服务级 close 绕过 UI 资源状态。
///
/// @author kongweiguang
#[allow(clippy::too_many_arguments)]
async fn verify_same_tab_second_pane(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &super::RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    services: &AppServices,
    resources: &mut ContinuityResources,
    tab_id: &TabId,
    agent_session_id: &AgentSessionId,
    agent_terminal_session_id: &SessionId,
    draft_token: &str,
) -> Result<(), Box<dyn Error>> {
    let (a1_pane, a1_session) = window.update(cx, |_, _, app| {
        let mut panes = root.read(app).right_rail_acceptance_terminal_panes(tab_id);
        if panes.len() != 1 {
            return Err(format!(
                "同一 Tab 分屏前应恰有一个 Ready 本地 pane，实际 {}",
                panes.len()
            ));
        }
        Ok(panes.remove(0))
    })??;
    if a1_session == *agent_terminal_session_id {
        return Err(io::Error::other("A1 pane session 不能等同于 Agent PTY").into());
    }
    if resources
        .a1_session_id
        .as_ref()
        .is_some_and(|session_id| session_id != &a1_session)
    {
        return Err(io::Error::other("A1 pane 与会话创建阶段记录的身份不一致").into());
    }
    if resources
        .a1_pane_id
        .as_ref()
        .is_some_and(|pane_id| pane_id != &a1_pane)
    {
        return Err(io::Error::other("A1 pane 与会话创建阶段记录的 PaneId 不一致").into());
    }
    resources
        .a1_session_id
        .get_or_insert_with(|| a1_session.clone());
    // 记录 A1 的实际 PaneId，切回原 Tab 后必须仍由同一 pane/session 对承载，不能只看
    // 面板里还选中了同一个 SessionId 就宣称原 pane 未变。
    resources.a1_pane_id = Some(a1_pane.clone());
    let panel_before = window.update(cx, |_, _, app| {
        root.read(app).right_rail_acceptance_agent_panel_state(app)
    })?;
    let identity_before =
        read_agent_continuity_identity(services, agent_session_id, agent_terminal_session_id)
            .await?;
    assert_continuity_identity(&identity_before, &identity_before, "before same-tab split")?;

    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.request_right_rail_acceptance_local_split(
                tab_id.clone(),
                a1_pane.clone(),
                window,
                cx,
            )
        })
    })?;
    resources.pending_creation = Some("同一 Tab 的 A2 分屏");
    let (a2_pane, a2_session) = poll_root(
        cx,
        root,
        window,
        "同一 Tab 创建第二个真实本地 pane",
        |root, _, _| {
            let panes = root.right_rail_acceptance_terminal_panes(tab_id);
            if panes.len() != 2 {
                return Ok(None);
            }
            let added = panes
                .into_iter()
                .filter(|(pane, session)| {
                    pane != &a1_pane
                        && session != &a1_session
                        && session != agent_terminal_session_id
                })
                .collect::<Vec<_>>();
            // 只接受恰好新增一个 pane/session，避免把原 pane 重挂或 Agent PTY 误当成 A2。
            if added.len() != 1 {
                return Ok(None);
            }
            let Some((pane, session)) = added.into_iter().next() else {
                return Ok(None);
            };
            Ok(Some((pane, session)))
        },
    )
    .await?;
    if a2_session == a1_session || a2_session == *agent_terminal_session_id {
        return Err(io::Error::other("A2 必须是独立于 A1 与 Agent PTY 的真实会话").into());
    }
    // 分屏一旦暴露新的真实 session 就立即登记；后续 Running 或焦点校验失败时，
    // 异常收尾仍能按这个身份关闭 A2，而不是依赖整个 Tab 扫描。
    resources.a2_session_id = Some(a2_session.clone());
    resources.pending_creation = None;
    // 先冻结 A2 的会话与进程身份，再显式关闭；后续 B 复核必须同时排除这个已关闭身份，
    // 不能因为 A2 已从清理集合移除就失去“B 不是 A2 复用”的证据。
    let a2_identity = read_continuity_session_identity(services, &a2_session).await?;
    if a2_identity.lifecycle != SessionLifecycle::Running || a2_identity.process_id.is_none() {
        return Err(io::Error::other(format!(
            "A2 分屏后未进入带进程身份的 Running 真实会话：{a2_identity:?}"
        ))
        .into());
    }
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-continuity-same-tab-second-pane-ready",
        "production_split_event_created_a_second_running_local_pane_and_session_in_the_same_tab",
    )
    .await?;

    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.request_right_rail_acceptance_focus_local_pane(
                tab_id.clone(),
                a2_pane.clone(),
                window,
                cx,
            )
        })
    })?;
    poll_root(
        cx,
        root,
        window,
        "生产事件把同一 Tab 焦点切到 A2",
        |root, _, _| {
            let focused = root.right_rail_acceptance_focused_terminal();
            Ok(
                (focused == Some((tab_id.clone(), a2_pane.clone(), a2_session.clone())))
                    .then_some(()),
            )
        },
    )
    .await?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-continuity-same-tab-focus-switched",
        "production_focus_event_moved_the_active_pane_to_the_second_real_local_session",
    )
    .await?;

    wait_for_agent_root_text(
        cx,
        root,
        window,
        ContinuityTarget::Local,
        agent_session_id,
        agent_terminal_session_id,
        &[draft_token.to_owned()],
        "Agent draft while the same-tab second pane is focused",
    )
    .await?;
    let panel_after = window.update(cx, |_, _, app| {
        root.read(app).right_rail_acceptance_agent_panel_state(app)
    })?;
    if panel_after.panel_tab_id() != Some(tab_id)
        || panel_after.panel_entity_id() != panel_before.panel_entity_id()
        || panel_after.selected_agent_session_id() != panel_before.selected_agent_session_id()
        || panel_after.selected_terminal_session_id() != panel_before.selected_terminal_session_id()
        || panel_after.target_generation() != panel_before.target_generation()
        || panel_after.target_terminal_session_id() != panel_before.target_terminal_session_id()
        || panel_after.session_count() != panel_before.session_count()
        || panel_after.history_mode() != panel_before.history_mode()
        || panel_after.terminal_lifecycle() != Some(SessionLifecycle::Running)
    {
        return Err(io::Error::other(format!(
            "聚焦同一 Tab 的 A2 后 Agent 面板或首选绑定发生变化：before {panel_before:?}, after {panel_after:?}"
        ))
        .into());
    }
    let identity_after =
        read_agent_continuity_identity(services, agent_session_id, agent_terminal_session_id)
            .await?;
    assert_continuity_identity(
        &identity_before,
        &identity_after,
        "while the same-tab second pane is focused",
    )?;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-continuity-same-tab-continuity-preserved",
        "same_agent_pty_draft_panel_entity_and_preferred_binding_survived_focusing_a2",
    )
    .await?;

    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.request_right_rail_acceptance_focus_local_pane(
                tab_id.clone(),
                a1_pane.clone(),
                window,
                cx,
            )
        })
    })?;
    poll_root(
        cx,
        root,
        window,
        "生产事件把焦点恢复到 A1",
        |root, _, _| {
            let focused = root.right_rail_acceptance_focused_terminal();
            Ok(
                (focused == Some((tab_id.clone(), a1_pane.clone(), a1_session.clone())))
                    .then_some(()),
            )
        },
    )
    .await?;
    // 通过生产 UI 关闭路径（Close 事件 → controller → Root 资源 close → remove_closed_session）
    // 回收 A2：只有这条路径会把 pane 从 workspace_runtime 和布局中移除。直接调用服务级
    // close_until 只能关闭 PTY，pane 会带着已死会话留在 Ready 状态，后续单 pane 断言必然失败。
    let (a2_pane_for_close, a2_session_for_close) = (a2_pane.clone(), a2_session.clone());
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.right_rail_acceptance_close_pane(
                tab_id.clone(),
                a2_pane_for_close,
                a2_session_for_close,
                window,
                cx,
            )
        })
    })?;
    // 等待真实回收收敛，三个条件必须同时成立：
    // 1) 注册表读回权威生命周期为 Closed（`Exited` 只是子进程结束，ManagedSession 仍持有
    //    writer、引擎与待清理所有权，不能据此认定已回收）；条目被移除时读回同样折叠为 Closed；
    // 2) 运行态 pane 与 session 映射都已消失——`right_rail_acceptance_terminal_panes` 会过滤
    //    布局外与非 Ready pane，不能证明 workspace_runtime 中的 Closing pane 已移除；
    // 3) 已提交布局不再引用 A2。候选布局在 controller `begin_close_pane` 时同步移除 A2，
    //    只看布局或只看 Ready pane 都会在 close 完成前提前通过。
    // list 找到条目后 runtime_metadata 可能恰逢关闭移除而返回 NotFound；这只在绝对期限内允许
    // 重试，期限已到或其它错误必须失败，不能把“读不到”推断成已关闭。
    let convergence_deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    let mut last_registry: Option<ContinuitySessionIdentity> = None;
    // 不预置成功值：只有轮询真正执行过才有“最近一次”结果，避免把未观测当成已收敛。
    let mut last_removal: Option<Result<(), String>>;
    let a2_closed_lifecycle = loop {
        let registry = read_continuity_session_identity(services, &a2_session).await;
        let deadline_reached = Instant::now() >= convergence_deadline;
        let registry = match continuity_convergence_step(registry, deadline_reached)? {
            ContinuityConvergenceStep::Observed(identity) => Some(identity),
            ContinuityConvergenceStep::Retry => None,
        };
        let (removal, panes) = window.update(cx, |_, _, app| {
            let root = root.read(app);
            (
                root.right_rail_acceptance_pane_removed(tab_id, &a2_pane, &a2_session),
                root.right_rail_acceptance_terminal_panes(tab_id),
            )
        })?;
        let registry_converged = registry
            .as_ref()
            .is_some_and(|registry| registry.lifecycle == SessionLifecycle::Closed);
        let panes_converged = panes == vec![(a1_pane.clone(), a1_session.clone())];
        last_registry = registry.or(last_registry);
        last_removal = Some(removal.clone());
        if registry_converged && removal.is_ok() && panes_converged {
            break SessionLifecycle::Closed;
        }
        if deadline_reached {
            return Err(io::Error::other(format!(
                "A2 经生产关闭路径后未同时收敛：注册表 {last_registry:?}，存在性 {last_removal:?}，\
                 Ready pane {panes:?}（期望仅 {:?}/{:?}）",
                a1_pane, a1_session
            ))
            .into());
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    };
    resources.a2_closed_identity = Some(ContinuitySessionIdentity {
        lifecycle: a2_closed_lifecycle,
        ..a2_identity
    });
    resources.a2_session_id = None;
    checkpoint(
        cx,
        window,
        capture,
        records,
        "agent-continuity-same-tab-second-pane-reclaimed",
        "a2_was_closed_through_the_production_ui_close_path_and_both_pane_and_registry_converged",
    )
    .await?;
    Ok(())
}

/// 读取任意真实本地会话的注册表生命周期与 PTY 进程号；只按精确 SessionId 查询，
/// 不扫描 Tab 或猜测目标，避免把同 Tab 的其他 pane 当作本轮资源。
/// 只有注册表已无该条目时才跳过 `runtime_metadata`（关闭成功即移除，缺失代表已收敛）；
/// 条目仍在时元数据读取失败必须原样上抛，不能吞成 `process_id=None` 让 Running/PID 断言
/// 以含混的身份差异失败，掩盖真正的注册表或取消原因。返回类型保留 `AppError` 而不是立即
/// 装箱，使回收轮询能按公开错误码窄化 `NotFound` 竞态，同时其它错误仍原样失败。
///
/// @author kongweiguang
async fn read_continuity_session_identity(
    services: &AppServices,
    session_id: &SessionId,
) -> AppResult<ContinuitySessionIdentity> {
    let sessions = services.sessions.clone();
    let session_id = session_id.clone();
    let lookup_id = session_id.clone();
    let metadata_id = session_id.clone();
    let (lifecycle, metadata) = services
        .executor
        .run(move |_cancellation| async move {
            let lifecycle = sessions
                .list(SessionFilter::default())
                .await?
                .into_iter()
                .find(|item| item.session_id == lookup_id)
                .map(|item| item.lifecycle);
            // 条目仍存在才读元数据；把结果保留为 AppResult 而不是当场 ?，让“读取失败”与
            // “条目已消失”两种形态在纯函数里分别处理，避免失败被折叠成 process_id=None。
            let metadata = match lifecycle {
                Some(_) => Some(
                    sessions
                        .runtime_metadata(metadata_id)
                        .await
                        .map(|metadata| metadata.process_id),
                ),
                None => None,
            };
            Ok::<_, kerminal_application::AppError>((lifecycle, metadata))
        })
        .await?;
    continuity_session_observation(session_id, lifecycle, metadata)
}

/// 回收轮询对一次注册表读取的分类。`list` 找到条目后 `runtime_metadata` 恰逢关闭移除会返回
/// `NotFound`，这是可重试的竞态而不是回收证据；仅在绝对期限内重试，期限到达或其它错误原样失败，
/// 绝不在没有权威读回的情况下推断已关闭。
///
/// @author kongweiguang
#[derive(Debug)]
enum ContinuityConvergenceStep {
    Observed(ContinuitySessionIdentity),
    Retry,
}

/// 只把 `NotFound` 视为可能由并发移除造成的可重试竞态，并要求仍未到达绝对期限；
/// 其余错误（包括取消、超时、基础设施失败）保持类型与 source 立即返回。
///
/// @author kongweiguang
fn continuity_convergence_step(
    observed: AppResult<ContinuitySessionIdentity>,
    deadline_reached: bool,
) -> Result<ContinuityConvergenceStep, AppError> {
    match observed {
        Ok(identity) => Ok(ContinuityConvergenceStep::Observed(identity)),
        Err(error) if !deadline_reached && error.public_code() == PublicErrorCode::NotFound => {
            Ok(ContinuityConvergenceStep::Retry)
        }
        Err(error) => Err(error),
    }
}

/// 把“注册表生命周期 + 可选的元数据结果”折叠成身份证据。抽成纯函数是为了让
/// “条目缺失 = Closed”和“条目存在但读取失败 = 原样失败”两条边界可被单测固定，
/// 不依赖真实会话服务或窗口。元数据结果对应 `SessionRuntimeMetadata.process_id`，
/// 进程号本身可缺失，因此内层仍是 `Option<u32>`，不能把“无元数据结果”与“无进程号”混同。
///
/// @author kongweiguang
fn continuity_session_observation(
    session_id: SessionId,
    lifecycle: Option<SessionLifecycle>,
    metadata: Option<AppResult<Option<u32>>>,
) -> AppResult<ContinuitySessionIdentity> {
    let process_id = match (lifecycle, metadata) {
        // 注册表已移除：关闭成功的收敛形态，没有可读的运行时元数据。
        (None, None) => None,
        // 条目仍在但缺少元数据结果属于内部不一致，明确失败而不是猜成已关闭。
        (Some(_), None) => {
            return Err(AppError::conflict(
                "Agent 连续性会话仍在注册表中，但缺少运行时元数据读取结果",
            ));
        }
        (None, Some(_)) => {
            return Err(AppError::conflict(
                "Agent 连续性会话已不在注册表中，却仍读取了运行时元数据",
            ));
        }
        (Some(_), Some(result)) => result?,
    };
    Ok(ContinuitySessionIdentity {
        session_id,
        process_id,
        // 注册表已移除时按 Closed 处理；这里只用于已关闭资源的证据，Running 断言由调用方明确要求
        // Some(Running)，不会把缺失条目当成仍存活。
        lifecycle: lifecycle.unwrap_or(SessionLifecycle::Closed),
    })
}

/// `RuntimeMetadata` 的 process_id 可缺失，身份连续性必须要求关闭前已有进程号；只有
/// SessionId 相同不足以证明 PTY 没有被关闭后重建，因此这里把生命周期、Running 和 PID 一起比较。
///
/// @author kongweiguang
fn continuity_running_identity_continues(
    before: &ContinuitySessionIdentity,
    current: &ContinuitySessionIdentity,
) -> bool {
    current.lifecycle == SessionLifecycle::Running
        && current.process_id.is_some()
        && current.process_id == before.process_id
        && current.session_id == before.session_id
}

/// B 必须同时排除已关闭 A2 的 SessionId 和进程号；只比较其中一个会把 PID 复用或残留
/// 会话误判为新 Tab 的独立资源。
///
/// @author kongweiguang
fn b_reuses_closed_a2(
    b: &ContinuitySessionIdentity,
    a2: Option<&ContinuitySessionIdentity>,
) -> bool {
    a2.is_some_and(|a2| {
        b.session_id == a2.session_id || (b.process_id.is_some() && b.process_id == a2.process_id)
    })
}

/// 直接读取共享服务的 Agent/PTY 身份；切换期间不依赖 Root 当前选择，也不启动第二个 watcher。
///
/// @author kongweiguang
async fn read_agent_continuity_identity(
    services: &AppServices,
    agent_session_id: &AgentSessionId,
    terminal_session_id: &SessionId,
) -> Result<AgentContinuityIdentity, Box<dyn Error>> {
    let agents = services
        .agents
        .clone()
        .ok_or_else(|| io::Error::other("continuity acceptance lacks AgentService"))?;
    let sessions = services.sessions.clone();
    let agent_id = agent_session_id.clone();
    let terminal_id = terminal_session_id.clone();
    let (view, listed, metadata) = services
        .executor
        .run(move |cancellation| async move {
            let view = agents.get(agent_id, cancellation.clone()).await?;
            let listed = agents.list(cancellation.clone()).await?;
            let metadata: SessionRuntimeMetadata = sessions.runtime_metadata(terminal_id).await?;
            Ok::<_, kerminal_application::AppError>((view, listed, metadata))
        })
        .await?;
    // 流程已经拿到并持续使用同一个 terminal session，投影忽然没有 terminal 说明 Agent
    // 视图与预期不符。这里不能生成一个随机 SessionId 顶替：那只会让后续身份比较报
    // “identity changed”，掩盖真正原因，也无法对应任何真实进程。
    let terminal = view.terminal.as_ref().ok_or_else(|| {
        io::Error::other(format!(
            "Agent continuity view lost its terminal while expecting {terminal_session_id}"
        ))
    })?;
    Ok(AgentContinuityIdentity {
        terminal_session_id: terminal.session_id.clone(),
        lifecycle: terminal.lifecycle,
        status: view.session.status,
        target: view.session.target.clone(),
        process_id: metadata.process_id,
        resume_recovery_present: view.resume_recovery.is_some(),
        agent_session_ids: listed
            .iter()
            .map(|view| view.session.agent_session_id.to_string())
            .collect(),
    })
}

/// 切换前后只允许身份、生命周期、绑定和会话集合保持不变；任一差异都说明发生了创建、恢复、停止或重绑。
///
/// @author kongweiguang
fn assert_continuity_identity(
    before: &AgentContinuityIdentity,
    current: &AgentContinuityIdentity,
    phase: &str,
) -> Result<(), Box<dyn Error>> {
    if current.terminal_session_id != before.terminal_session_id {
        return Err(io::Error::other(format!("Agent PTY identity changed {phase}")).into());
    }
    if current.lifecycle != SessionLifecycle::Running {
        return Err(io::Error::other(format!(
            "Agent PTY stopped {phase}: {:?}",
            current.lifecycle
        ))
        .into());
    }
    if current.status != AgentSessionStatus::Active {
        return Err(io::Error::other(format!(
            "Agent session left Active {phase}: {:?}",
            current.status
        ))
        .into());
    }
    if current.target != before.target {
        return Err(io::Error::other(format!(
            "Agent preferred target changed {phase}: before {:?}, after {:?}",
            before.target, current.target
        ))
        .into());
    }
    if before.process_id.is_none() || current.process_id != before.process_id {
        return Err(io::Error::other(format!(
            "Agent PTY process identity changed {phase}: before {:?}, after {:?}",
            before.process_id, current.process_id
        ))
        .into());
    }
    if current.resume_recovery_present {
        return Err(io::Error::other(format!("Agent entered resume recovery {phase}")).into());
    }
    if current.agent_session_ids != before.agent_session_ids {
        return Err(
            io::Error::other(format!("Agent history gained or lost sessions {phase}")).into(),
        );
    }
    Ok(())
}

/// 通过共享服务写入同一 Agent PTY；隐藏期间不借用 Root 输入绑定，也不改变 owner/generation。
///
/// @author kongweiguang
async fn write_agent_terminal_input(
    services: &AppServices,
    terminal_session_id: &SessionId,
    text: String,
) -> Result<(), Box<dyn Error>> {
    let sessions = services.sessions.clone();
    let session_id = terminal_session_id.clone();
    services
        .executor
        .run(move |_| async move {
            sessions
                .write_input(session_id, TerminalInputEvent::Text(text))
                .await
        })
        .await?;
    Ok(())
}

/// 读取显式 Agent/PTY 的渲染文本与生命周期；B 活跃时用它证明 A 仍在产生新输出。
///
/// @author kongweiguang
async fn read_hidden_agent_terminal(
    services: &AppServices,
    agent_session_id: &AgentSessionId,
    terminal_session_id: &SessionId,
) -> Result<(SessionLifecycle, String), Box<dyn Error>> {
    let sessions = services.sessions.clone();
    let owner = SessionOwner::Agent {
        agent_session_id: agent_session_id.clone(),
    };
    let terminal_id = terminal_session_id.clone();
    let (lifecycle, text) = services
        .executor
        .run(move |_| async move {
            let snapshot = sessions
                .render_snapshot(terminal_id.clone(), RenderSnapshotRequest::default())
                .await?;
            let lifecycle = sessions
                .list(SessionFilter {
                    owner: Some(owner),
                    ..SessionFilter::default()
                })
                .await?
                .into_iter()
                .find(|session| session.session_id == terminal_id)
                .map_or(SessionLifecycle::Closed, |session| session.lifecycle);
            Ok::<_, kerminal_application::AppError>((lifecycle, continuity_render_text(&snapshot)))
        })
        .await?;
    Ok((lifecycle, text))
}

/// 在 B 活跃期间轮询 A 的 PTY 快照，直到同步 marker 真正出现；超时按未观测到新输出失败。
///
/// @author kongweiguang
async fn wait_for_hidden_agent_text(
    cx: &mut AsyncApp,
    services: &AppServices,
    agent_session_id: &AgentSessionId,
    terminal_session_id: &SessionId,
    required: &str,
) -> Result<String, Box<dyn Error>> {
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let (lifecycle, text) =
            read_hidden_agent_terminal(services, agent_session_id, terminal_session_id).await?;
        // 200% UI rem/窄栏下终端列数更少，长 stage marker 可能跨行；marker 不含空白，
        // 按去空白文本检查内容，避免把折行当成后台 PTY 没有输出。
        let normalized: String = text
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        if lifecycle == SessionLifecycle::Running && normalized.contains(required) {
            return Ok(text);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "hidden Agent PTY did not produce {required} while the other tab was active"
            ))
            .into());
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// 有界地等待后台 Agent PTY 对同一触发文件产生指定次数的 stage 输出。只要求达到计数，
/// 不轮询无限流，也不把两个阶段解释为持续输出能力；超时仍返回当前文本便于定位。
///
/// @author kongweiguang
async fn wait_for_hidden_stage_count(
    cx: &mut AsyncApp,
    services: &AppServices,
    agent_session_id: &AgentSessionId,
    terminal_session_id: &SessionId,
    marker: &str,
    expected: usize,
) -> Result<String, Box<dyn Error>> {
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let (lifecycle, text) =
            read_hidden_agent_terminal(services, agent_session_id, terminal_session_id).await?;
        // 与首段一样按去空白文本计数，兼容窄栏/200% 下的行折返。
        let normalized: String = text
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        let count = normalized.matches(marker).count();
        if lifecycle == SessionLifecycle::Running && count >= expected {
            return Ok(text);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "hidden Agent PTY produced {count} of {expected} bounded stage outputs while the other tab was active"
            ))
            .into());
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// 只把 UI 明确报告的“尚未建立 pane/candidate/panel”视为暂态；目标不匹配、工具错误或
/// renderer 身份不一致仍立即失败，避免把生产绑定错误掩盖成超时。
///
/// @author kongweiguang
fn continuity_agent_not_ready(message: &str) -> bool {
    matches!(
        message,
        "Agent 验收没有活动的 fixture SSH pane"
            | "Agent 验收没有活动的本地终端 pane"
            | "Agent 验收没有完整的活动终端候选"
            | "Agent panel 不可用"
    )
}

/// 等待 Root 当前 Agent 画布同时包含全部阶段 marker；只接受生产 watcher 投影。
/// 焦点切换或面板换挂期间，候选尚未建立会短暂报错；仅对上述明确暂态在统一期限内重试，
/// 其它错误立即带回，避免掩盖真实目标变化。参数都是必传的独立身份，压缩成结构体
/// 只会把同文件既有 helper 的直接调用惯例改成本任务独有的包装层。
///
/// @author kongweiguang
#[allow(clippy::too_many_arguments)]
async fn wait_for_agent_root_text(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    target: ContinuityTarget<'_>,
    agent_session_id: &AgentSessionId,
    terminal_session_id: &SessionId,
    required: &[String],
    label: &str,
) -> Result<String, Box<dyn Error>> {
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    let mut last_not_ready = None;
    // 超时回执只保留最后投影的有界尾部，区分“输入未到达/未回显”和“渲染快照未刷新”；
    // 不输出完整终端内容，也不放宽 Running 与 marker 断言。
    let mut last_output: Option<(SessionLifecycle, usize, String)> = None;
    loop {
        let output = window.update(cx, |_, _, app| {
            root.update(app, |root, _| match target {
                ContinuityTarget::Fixture(host_id) => root.right_rail_acceptance_agent_output(
                    host_id,
                    agent_session_id,
                    terminal_session_id,
                ),
                ContinuityTarget::Local => root.right_rail_acceptance_agent_output_local(
                    agent_session_id,
                    terminal_session_id,
                ),
            })
        })?;
        let output = match output {
            Ok(output) => output,
            Err(message) if continuity_agent_not_ready(&message) => {
                last_not_ready = Some(message);
                None
            }
            Err(message) => {
                return Err(io::Error::other(format!(
                    "Agent root projection failed while waiting for {label}: {message}"
                ))
                .into());
            }
        };
        if let Some((lifecycle, text)) = output {
            let head: String = text.chars().take(400).collect();
            let tail: String = text
                .chars()
                .rev()
                .take(200)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            last_output = Some((
                lifecycle,
                text.len(),
                format!("head={head:?} tail={tail:?}"),
            ));
            // 右栏终端较窄，长 marker 可能跨行折行；marker 本身不含空白，按去空白后的文本
            // 检查内容，避免把渲染宽度当成内容缺失。
            let normalized: String = text
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect();
            if lifecycle == SessionLifecycle::Running
                && required.iter().all(|marker| normalized.contains(marker))
            {
                return Ok(text);
            }
        }
        if Instant::now() >= deadline {
            let detail = match last_output {
                Some((lifecycle, len, detail)) => {
                    format!("; last lifecycle={lifecycle:?} text_len={len} {detail}")
                }
                None => last_not_ready
                    .as_deref()
                    .map(|message| format!("; last transient state: {message}"))
                    .unwrap_or_default(),
            };
            return Err(io::Error::other(format!(
                "Agent root projection did not show {label} with a Running lifecycle{detail}"
            ))
            .into());
        }
        let _ = frame_signature(cx, window)?;
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// 只在已证明隔离的私有桌面显示本进程窗口，让 Windows 发出原生显示事件同步 GPUI 可见性。
///
/// @author kongweiguang
fn set_activity_main_window_visible(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    visible: bool,
) -> Result<(), Box<dyn Error>> {
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
        use std::ffi::c_void;
        unsafe extern "system" {
            fn GetCurrentThreadId() -> u32;
            fn GetWindowThreadProcessId(hwnd: *mut c_void, process_id: *mut u32) -> u32;
            fn ShowWindow(hwnd: *mut c_void, command: i32) -> i32;
        }
        handle.update(cx, |_, window, _| {
            super::isolation::capture()?
                .validate_activity_window_state(window.is_visible(), window.is_window_active())?;
            let raw = window.window_handle().map_err(io::Error::other)?;
            let RawWindowHandle::Win32(win32) = raw.as_raw() else {
                return Err(io::Error::other("验收窗口不是 Win32 原生窗口"));
            };
            let hwnd = win32.hwnd.get() as *mut c_void;
            let mut owner_process = 0u32;
            let owner_thread = unsafe { GetWindowThreadProcessId(hwnd, &mut owner_process) };
            if owner_process != std::process::id()
                || owner_thread != unsafe { GetCurrentThreadId() }
            {
                return Err(io::Error::other("验收窗口不属于当前进程/UI 线程"));
            }
            unsafe {
                ShowWindow(hwnd, if visible { 4 } else { 0 });
            }
            Ok::<_, io::Error>(())
        })??;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (cx, handle, visible);
        Err(io::Error::new(io::ErrorKind::Unsupported, "仅支持 Windows 私有桌面").into())
    }
}

/// 等待指定原生窗口到达期望的 visible 状态；open_window/set_visibility 后事件循环可能
/// 尚未提交可见性，先确认再激活，避免把“刚创建但未显示”误判成激活失败。
///
/// @author kongweiguang
async fn wait_for_window_visible(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    expected: bool,
    label: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let visible = handle.update(cx, |_, window, _| window.is_visible())?;
        if visible == expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待{label}超时")).into());
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// 等待指定原生窗口到达期望的 active 状态；只轮询本轮显式句柄，不枚举其它窗口。
///
/// @author kongweiguang
async fn wait_for_window_active(
    cx: &mut AsyncApp,
    handle: AnyWindowHandle,
    expected: bool,
    label: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let active = handle.update(cx, |_, window, _| window.is_window_active())?;
        if active == expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!("等待{label}超时")).into());
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// 在主窗口持续失活时读取生产 Agent 画布，避免切回窗口或直接读取引擎掩盖快照停更。
///
/// @author kongweiguang
#[allow(clippy::too_many_arguments)]
async fn wait_for_inactive_agent_output(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    target: ContinuityTarget<'_>,
    agent_session_id: &AgentSessionId,
    terminal_session_id: &SessionId,
    marker: &str,
    marker_present: bool,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + super::PANEL_FRAME_TIMEOUT;
    loop {
        let (active, visible, output) = window.update(cx, |_, window, app| {
            let output = match target {
                ContinuityTarget::Fixture(host_id) => {
                    root.read(app).right_rail_acceptance_agent_output(
                        host_id,
                        agent_session_id,
                        terminal_session_id,
                    )
                }
                ContinuityTarget::Local => root.read(app).right_rail_acceptance_agent_output_local(
                    agent_session_id,
                    terminal_session_id,
                ),
            };
            (window.is_window_active(), window.is_visible(), output)
        })?;
        if active || !visible {
            return Err(io::Error::other(format!(
                "Agent 输出验收期间主窗口未保持可见且失活：active={active} visible={visible}"
            ))
            .into());
        }
        if let Some((SessionLifecycle::Running, text)) = output.map_err(io::Error::other)? {
            let normalized: String = text
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect();
            if normalized.contains(marker) == marker_present {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "失焦期间 Agent 画布的输出标记 {marker} 未达到预期状态：present={marker_present}"
            ))
            .into());
        }
        cx.background_executor().timer(super::ACCEPTANCE_POLL).await;
    }
}

/// 同一 private desktop 的第二个原生顶层窗口让主窗口真实失活再激活；只证明原生窗口切换，
/// 不冒充其它应用鼠标。步骤内先关闭辅助窗口再激活主窗口，任何失败都由外层补关。
///
/// @author kongweiguang
#[allow(clippy::too_many_arguments)]
async fn run_native_window_activity_steps(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &super::RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    services: &AppServices,
    target: ContinuityTarget<'_>,
    agent_session_id: &AgentSessionId,
    terminal_session_id: &SessionId,
    draft_token: Option<&str>,
    operation: &'static str,
    outcome: &'static str,
    helper: &mut Option<AnyWindowHandle>,
) -> Result<(), Box<dyn Error>> {
    let panel_before = window.update(cx, |_, _, app| {
        root.read(app).right_rail_acceptance_agent_panel_state(app)
    })?;
    let identity_before =
        read_agent_continuity_identity(services, agent_session_id, terminal_session_id).await?;
    set_activity_main_window_visible(cx, window, true)?;
    wait_for_window_visible(cx, window, true, "主窗口显示").await?;
    super::activate_acceptance_window(cx, window)?;
    wait_for_window_active(cx, window, true, "主窗口激活").await?;
    window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.set_right_rail_acceptance_activity_override(false, window, cx);
        });
    })?;
    window.update(cx, |_, _, app| {
        root.update(app, |root, _| {
            root.reset_right_rail_acceptance_window_activation_events();
            root.reset_right_rail_acceptance_unavailable_fallback_render_count();
        })
    })?;
    let helper_handle = helper.ok_or_else(|| io::Error::other("辅助窗口句柄缺失"))?;
    // open_window 的 focus 选项在私有桌面上不保证立即成为 active；显式显示并激活同一句柄，
    // 再等待它真正 active，才能证明主窗口收到失活事件。
    super::set_acceptance_window_visibility(cx, helper_handle, true)?;
    wait_for_window_visible(cx, helper_handle, true, "辅助窗口显示").await?;
    super::activate_acceptance_window(cx, helper_handle)?;
    wait_for_window_active(cx, helper_handle, true, "辅助窗口激活").await?;
    wait_for_window_active(cx, window, false, "主窗口失活").await?;
    if operation == "agent-continuity-native-window-activity-output" {
        let marker = "inactiveprobe";
        write_agent_terminal_input(services, terminal_session_id, marker.to_owned()).await?;
        wait_for_inactive_agent_output(
            cx,
            root,
            window,
            target,
            agent_session_id,
            terminal_session_id,
            marker,
            true,
        )
        .await?;
        checkpoint_visible(
            cx,
            window,
            capture,
            records,
            "agent-continuity-inactive-output",
            "visible_agent_snapshot_updated_while_native_window_inactive",
        )
        .await?;
        wait_for_window_active(cx, window, false, "失焦输出截图后主窗口仍失活").await?;
        write_agent_terminal_input(services, terminal_session_id, "\u{8}".repeat(marker.len()))
            .await?;
        wait_for_inactive_agent_output(
            cx,
            root,
            window,
            target,
            agent_session_id,
            terminal_session_id,
            marker,
            false,
        )
        .await?;
    }
    super::close_acceptance_activity_window(cx, helper_handle)?;
    *helper = None;
    super::activate_acceptance_window(cx, window)?;
    wait_for_window_active(cx, window, true, "主窗口重新激活").await?;
    // 激活 observer 在事件阶段同步首帧投影；这里立刻取一帧并读计数，避免只看稳定后的最终图。
    let _ = frame_signature_visible(cx, window)?;
    let (activations, deactivations) = window.update(cx, |_, _, app| {
        root.read(app)
            .right_rail_acceptance_window_activation_events()
    })?;
    if activations == 0 || deactivations == 0 {
        return Err(io::Error::other(format!(
            "原生 activity observer 未记录完整失活/激活：activations={activations} deactivations={deactivations}"
        ))
        .into());
    }
    let fallback = window.update(cx, |_, _, app| {
        root.read(app)
            .right_rail_acceptance_unavailable_fallback_render_count()
    })?;
    if fallback != 0 {
        return Err(io::Error::other(format!(
            "原生 activity 首帧出现 {fallback} 次 unavailable fallback"
        ))
        .into());
    }
    let panel_after = window.update(cx, |_, _, app| {
        root.read(app).right_rail_acceptance_agent_panel_state(app)
    })?;
    if panel_after.panel_entity_id() != panel_before.panel_entity_id()
        || panel_after.selected_agent_session_id() != panel_before.selected_agent_session_id()
        || panel_after.selected_terminal_session_id() != panel_before.selected_terminal_session_id()
        || panel_after.history_mode() != panel_before.history_mode()
    {
        return Err(io::Error::other(format!(
            "原生 activity 改变了面板/选择/过滤：before {panel_before:?}, after {panel_after:?}"
        ))
        .into());
    }
    let identity_after =
        read_agent_continuity_identity(services, agent_session_id, terminal_session_id).await?;
    assert_continuity_identity(&identity_before, &identity_after, "native window activity")?;
    if let Some(draft_token) = draft_token {
        wait_for_agent_root_text(
            cx,
            root,
            window,
            target,
            agent_session_id,
            terminal_session_id,
            &[draft_token.to_owned()],
            "draft after native window activity",
        )
        .await?;
    }
    // 首帧证据在恢复激活后立即截取，再隐藏主窗口，避免只留稳定后的最终图。
    checkpoint_visible(cx, window, capture, records, operation, outcome).await?;
    Ok(())
}

/// 打开辅助窗口并保证所有路径关闭；主窗口无论成功失败都恢复隐藏，避免占用用户桌面。
///
/// @author kongweiguang
#[allow(clippy::too_many_arguments)]
async fn run_native_window_activity(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    capture: &super::RightRailCaptureContext,
    records: &mut Vec<FunctionalRecord>,
    services: &AppServices,
    target: ContinuityTarget<'_>,
    agent_session_id: &AgentSessionId,
    terminal_session_id: &SessionId,
    draft_token: Option<&str>,
    operation: &'static str,
    outcome: &'static str,
) -> Result<(), Box<dyn Error>> {
    let mut helper = Some(super::open_acceptance_activity_window(cx)?);
    let result = run_native_window_activity_steps(
        cx,
        root,
        window,
        capture,
        records,
        services,
        target,
        agent_session_id,
        terminal_session_id,
        draft_token,
        operation,
        outcome,
        &mut helper,
    )
    .await;
    let close = match helper.take() {
        Some(handle) => super::close_acceptance_activity_window(cx, handle),
        None => Ok(()),
    };
    let _ = window.update(cx, |_, window, app| {
        root.update(app, |root, cx| {
            root.set_right_rail_acceptance_activity_override(true, window, cx);
        });
    });
    let visibility = match set_activity_main_window_visible(cx, window, false) {
        Ok(()) => wait_for_window_visible(cx, window, false, "主窗口恢复隐藏").await,
        Err(error) => Err(error),
    };
    match (result, close, visibility) {
        (Ok(()), Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(()), Ok(())) => Err(error),
        (Ok(()), Err(error), Ok(())) | (Ok(()), Ok(()), Err(error)) => Err(error),
        (Err(primary), Err(close), Ok(())) => Err(combine_continuity_failure(
            primary,
            vec![format!("辅助窗口关闭失败：{close}")],
        )),
        (Err(primary), Err(close), Err(visibility)) => Err(combine_continuity_failure(
            primary,
            vec![
                format!("辅助窗口关闭失败：{close}"),
                format!("主窗口恢复隐藏失败：{visibility}"),
            ],
        )),
        (Err(primary), Ok(()), Err(cleanup)) => Err(combine_continuity_failure(
            primary,
            vec![format!("主窗口恢复隐藏失败：{cleanup}")],
        )),
        (Ok(()), Err(close), Err(visibility)) => Err(combine_continuity_failure(
            close,
            vec![format!("主窗口恢复隐藏失败：{visibility}")],
        )),
    }
}

/// 连续性 Agent 退出后仍走生产归档确认与 CAS，避免隔离验收遗留运行记录。
///
/// @author kongweiguang
async fn archive_continuity_agent(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    target: ContinuityTarget<'_>,
    agent_session_id: &AgentSessionId,
) -> Result<(), Box<dyn Error>> {
    poll_root(
        cx,
        root,
        window,
        "打开连续性 Agent 删除确认",
        |root, window, cx| {
            let ready = match target {
                ContinuityTarget::Fixture(host_id) => root
                    .right_rail_acceptance_agent_archive_confirmation_ready(
                        host_id,
                        agent_session_id,
                        cx,
                    )?,
                ContinuityTarget::Local => root
                    .right_rail_acceptance_agent_archive_confirmation_ready_local(
                        agent_session_id,
                        cx,
                    )?,
            };
            if !ready {
                match target {
                    ContinuityTarget::Fixture(host_id) => {
                        root.archive_right_rail_acceptance_agent(
                            host_id,
                            agent_session_id,
                            window,
                            cx,
                        )?;
                    }
                    ContinuityTarget::Local => {
                        root.archive_right_rail_acceptance_agent_local(
                            agent_session_id,
                            window,
                            cx,
                        )?;
                    }
                }
            }
            let confirmation_ready = match target {
                ContinuityTarget::Fixture(host_id) => root
                    .right_rail_acceptance_agent_archive_confirmation_ready(
                        host_id,
                        agent_session_id,
                        cx,
                    )?,
                ContinuityTarget::Local => root
                    .right_rail_acceptance_agent_archive_confirmation_ready_local(
                        agent_session_id,
                        cx,
                    )?,
            };
            Ok(confirmation_ready.then_some(()))
        },
    )
    .await?;
    poll_root(
        cx,
        root,
        window,
        "提交连续性 Agent 删除确认",
        |root, window, cx| {
            let submitted = match target {
                ContinuityTarget::Fixture(host_id) => {
                    root.archive_right_rail_acceptance_agent(host_id, agent_session_id, window, cx)?
                }
                ContinuityTarget::Local => {
                    root.archive_right_rail_acceptance_agent_local(agent_session_id, window, cx)?
                }
            };
            Ok(submitted.then_some(()))
        },
    )
    .await?;
    wait_for_agent_view(
        cx,
        root,
        window,
        target,
        Some(agent_session_id),
        |view| view.session.status == AgentSessionStatus::Archived,
        "archived continuity Agent history readback",
    )
    .await?;
    Ok(())
}

/// 把 renderer 的可見 run 按 viewport 行順序拼成純文字，供隱藏階段 marker 做精確回讀。
/// 驗收側複製同一拼法而不改動 UI 私有實現，避免為了斷言暴露更多生產接口。
///
/// @author kongweiguang
fn continuity_render_text(snapshot: &RenderSnapshot) -> String {
    snapshot
        .rows
        .iter()
        .map(|row| {
            row.runs
                .iter()
                .map(|run| run.text.as_ref())
                .collect::<Vec<_>>()
                .join("")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
#[path = "../../tests/unit/right_rail_continuity_stage.rs"]
mod continuity_stage_tests;

#[cfg(test)]
#[path = "../../tests/unit/right_rail_continuity_reclaim.rs"]
mod continuity_reclaim_tests;
