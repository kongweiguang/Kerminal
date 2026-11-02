//! Agent landing 的真实组件交互与布局回归；所有服务使用隔离内存数据。
//!
//! @author kongweiguang

#[path = "agent_landing_visual_support.rs"]
mod support;

use std::{cell::RefCell, rc::Rc, sync::Arc};

use gpui_kit::{
    AppContext as _, Bounds, ElementId, Entity, Pixels, TestAppContext, VisualTestContext,
    WindowAppearance, point, px, size,
};
use kerminal_application::{
    ApplicationTaskExecutor,
    agents::{AgentResumeCapability, AgentSessionView},
    preferences::ThemeMode,
};
use kerminal_domain::{
    agents::{AgentProvider, AgentSession, AgentSessionLaunch, AgentSessionScope},
    identity::{AgentSessionId, SessionId},
    sessions::{
        LocalSessionTarget, SessionInfo, SessionLifecycle, SessionOwner, SessionTarget,
        TerminalSize,
    },
};
use tokio::runtime::{Builder, Runtime};

use super::{AgentsPanel, history_filter_display_text};
use support::FixtureAgentService;

/// 当前目标和全部筛选都在标签后显示计数，零值也必须可见。
///
/// @author kongweiguang
#[test]
fn history_filter_text_keeps_zero_count_visible() {
    assert_eq!(history_filter_display_text("当前目标", 0), "当前目标 0");
    assert_eq!(history_filter_display_text("全部", 0), "全部 0");
}

/// 固定运行中会话和独立终端 owner，确保点击继续只选择原会话，不启动外部 CLI。
fn running_view() -> AgentSessionView {
    let id = AgentSessionId::new();
    let session = AgentSession::new(
        id.clone(),
        AgentProvider::Codex,
        None,
        "Codex".to_owned(),
        "2026-10-19T00:00:00Z".to_owned(),
        "workspace".to_owned(),
        "workspace/agents/session".to_owned(),
        AgentSessionLaunch {
            command_label: "Codex".to_owned(),
            shell: "codex".to_owned(),
            args: Vec::new(),
            cwd: "workspace/agents/session".to_owned(),
        },
        Some(AgentSessionScope::Global),
        None,
    )
    .expect("valid isolated Agent session");
    AgentSessionView {
        session,
        terminal: Some(SessionInfo {
            session_id: SessionId::new(),
            target: SessionTarget::Local(LocalSessionTarget {
                program: "codex".into(),
                args: Vec::new(),
                working_directory: None,
                environment: Default::default(),
            }),
            owner: SessionOwner::Agent {
                agent_session_id: id,
            },
            size: TerminalSize::default(),
            lifecycle: SessionLifecycle::Running,
            exit_status: None,
        }),
        resume_capability: AgentResumeCapability::MetadataUnavailable,
        resume_recovery: None,
    }
}

/// 挂载生产面板及真实 Kit 根，Tokio 和所有服务仅存在于本次 headless 测试。
fn fixture(
    app: &mut TestAppContext,
) -> (
    Runtime,
    Arc<FixtureAgentService>,
    Entity<AgentsPanel>,
    AgentSessionId,
    &mut VisualTestContext,
) {
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let view = running_view();
    let id = view.session.agent_session_id.clone();
    let service = Arc::new(runtime.block_on(FixtureAgentService::new(view)));
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    app.update(|app| {
        crate::init(app);
        app.set_reduce_motion(true);
    });
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let service_for_panel = service.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let panel = cx.new(|cx| {
            AgentsPanel::new(
                service_for_panel,
                executor,
                AgentSessionScope::Global,
                window,
                cx,
            )
        });
        *mounted.borrow_mut() = Some(panel.clone());
        gpui_kit::base::Root::new(panel, window, cx)
    });
    visual.simulate_resize(size(px(360.), px(700.)));
    let panel = slot
        .borrow_mut()
        .take()
        .expect("production Agent panel mounted");
    pump(&runtime, visual);
    visual.update(|_, app| {
        assert!(!panel.read(app).loading && panel.read(app).sessions_verified);
        assert!(!panel.read(app).launcher_catalog_loading);
    });
    (runtime, service, panel, id, visual)
}

/// 交替推进两个执行器，确保判断来自查询回执和已绘制的生产帧。
fn pump(runtime: &Runtime, visual: &mut VisualTestContext) {
    for _ in 0..5 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
    draw(visual);
}

/// 双帧刷新控件快照，避免使用旧帧几何派发鼠标事件。
fn draw(visual: &mut VisualTestContext) {
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear(cx));
    visual.update(|window, cx| window.draw(cx).clear(cx));
}

/// 从真实 Kit Button 的访问性快照获得命中区域；不可见或缺失明确失败。
fn button_bounds(visual: &mut VisualTestContext, id: &str) -> Bounds<Pixels> {
    draw(visual);
    let id: ElementId = id.to_owned().into();
    visual.update(|window, _| {
        let snapshot = gpui_kit::base::test_support::find(window, &[], &id).unwrap_or_else(|| {
            panic!(
                "button {id:?} missing: {}",
                gpui_kit::base::test_support::registered_paths(window)
            )
        });
        assert!(snapshot.visible(), "button {id:?} must be visible");
        snapshot.bounds()
    })
}

/// 用独立控件中心派发真实鼠标事件，验证菜单按钮不会冒泡为继续会话。
fn click(visual: &mut VisualTestContext, id: &str) {
    let bounds = button_bounds(visual, id);
    visual.simulate_click(bounds.center(), Default::default());
    draw(visual);
}

/// 各主题复用真实色板切换路径；System 按当前系统深色外观解析。
fn apply_theme(
    panel: &Entity<AgentsPanel>,
    visual: &mut VisualTestContext,
    mode: ThemeMode,
    appearance: WindowAppearance,
) {
    visual.update(|_, app| {
        panel.update(app, |panel, cx| {
            crate::theme::apply_theme_mode(mode, appearance, cx);
            panel.set_theme_mode(mode, cx);
        })
    });
    draw(visual);
}

/// 刷新过程中保留列表原位置；状态标签不得与元数据重叠，并始终停留在行右侧。
#[gpui_kit::test]
fn running_history_layout_stays_stable_during_refresh(app: &mut TestAppContext) {
    let (runtime, service, panel, id, visual) = fixture(app);
    let primary = format!("agent-session-continue-{}", id.as_str());
    for (mode, appearance) in [
        (ThemeMode::Light, WindowAppearance::Light),
        (ThemeMode::Dark, WindowAppearance::Dark),
        (ThemeMode::System, WindowAppearance::Dark),
    ] {
        apply_theme(&panel, visual, mode, appearance);
        let row = button_bounds(visual, &primary);
        let badge = visual
            .debug_bounds("rr-agent-history-status")
            .expect("status badge rendered");
        let metadata = visual
            .debug_bounds("rr-agent-history-metadata")
            .expect("metadata rendered");
        assert!(
            badge.origin.x >= metadata.right(),
            "status overlaps metadata: {badge:?} {metadata:?}"
        );
        assert!(
            badge.right() <= row.right(),
            "status escapes row: {badge:?} {row:?}"
        );

        service.hold_history();
        click(visual, "agents-history-refresh");
        visual.update(|_, app| assert!(panel.read(app).loading));
        let during = button_bounds(visual, &primary);
        assert_eq!(during, row, "refresh must not move or resize the row");
        service.release_history();
        pump(&runtime, visual);
        visual.update(|_, app| assert!(!panel.read(app).loading));
        assert_eq!(button_bounds(visual, &primary), row);
    }
}

/// 读取失败保留原记录并拒绝不确定操作；成功重试只清除列表错误，不覆盖其它业务反馈。
///
/// @author kongweiguang
#[gpui_kit::test]
fn history_failure_preserves_records_and_retry_clears_only_the_read_error(
    app: &mut TestAppContext,
) {
    let (runtime, service, panel, id, visual) = fixture(app);
    let primary = format!("agent-session-continue-{}", id.as_str());
    visual.update(|_, app| {
        panel.update(app, |panel, cx| {
            panel.status = Some("Independent operation feedback".to_owned());
            cx.notify();
        });
    });
    let original_row = button_bounds(visual, &primary);
    service.set_history_failure(true);
    click(visual, "agents-history-refresh");
    pump(&runtime, visual);
    visual.update(|_, app| {
        let panel = panel.read(app);
        assert!(!panel.sessions_verified);
        assert!(panel.history_load_error.is_some());
        assert!(panel.visible_session_ids.contains(&id));
        assert_eq!(
            panel.status.as_deref(),
            Some("Independent operation feedback")
        );
    });
    assert!(visual.debug_bounds("rr-agent-history-error").is_some());
    let failed_row = button_bounds(visual, &primary);
    visual.simulate_click(
        point(failed_row.left() + px(16.), failed_row.top() + px(16.)),
        Default::default(),
    );
    visual.update(|_, app| assert!(panel.read(app).selected_agent.is_none()));
    service.set_history_failure(false);
    click(visual, "agents-refresh");
    pump(&runtime, visual);
    visual.update(|_, app| {
        let panel = panel.read(app);
        assert!(panel.sessions_verified);
        assert!(panel.history_load_error.is_none());
        assert!(panel.visible_session_ids.contains(&id));
        assert_eq!(
            panel.status.as_deref(),
            Some("Independent operation feedback")
        );
    });
    assert!(visual.debug_bounds("rr-agent-history-error").is_none());
    assert_eq!(button_bounds(visual, &primary), original_row);
}

/// 行内编辑、菜单、删除与会话继续互不误触；返回后保留同一运行终端。
#[gpui_kit::test]
fn row_actions_and_return_keep_the_same_running_chat(app: &mut TestAppContext) {
    let (runtime, _, panel, id, visual) = fixture(app);
    let rename = format!("agent-session-rename-{}", id.as_str());
    let cancel = format!("agent-session-cancel-{}", id.as_str());
    let primary = format!("agent-session-continue-{}", id.as_str());
    let more = format!("agent-history-actions-{}", id.as_str());
    let delete = format!("agent-session-delete-{}", id.as_str());
    let terminal_id = visual.update(|_, app| {
        panel.read(app).sessions[0]
            .terminal
            .as_ref()
            .unwrap()
            .session_id
            .clone()
    });
    click(visual, &more);
    visual.update(|window, app| {
        assert!(panel.read(app).selected_agent.is_none());
        assert!(
            gpui_kit::base::test_support::snapshots(window)
                .iter()
                .any(|node| node.visible() && node.role() == Some(gpui_kit::Role::Menu))
        );
    });
    visual.simulate_keystrokes("escape");
    draw(visual);
    click(visual, &delete);
    visual.update(|_, app| {
        assert!(panel.read(app).selected_agent.is_none());
        assert!(panel.read(app).archive_confirmation_owner.is_some());
    });
    click(visual, "agent-archive-cancel");
    visual.update(|_, app| assert!(panel.read(app).archive_confirmation_owner.is_none()));
    click(visual, &rename);
    visual.update(|_, app| {
        assert!(panel.read(app).rename_target.is_some());
        assert!(panel.read(app).selected_agent.is_none());
    });
    click(visual, &cancel);
    let row = button_bounds(visual, &primary);
    visual.simulate_click(
        point(row.left() + px(16.), row.top() + px(16.)),
        Default::default(),
    );
    draw(visual);
    visual.update(|_, app| assert_eq!(panel.read(app).selected_agent.as_ref(), Some(&id)));
    assert!(
        visual
            .debug_bounds("rr-agent-terminal-status-row")
            .is_none(),
        "running header must not reserve a status row"
    );
    click(visual, "agents-back");
    pump(&runtime, visual);
    visual.update(|_, app| {
        assert!(panel.read(app).selected_agent.is_none());
        assert!(panel.read(app).visible_session_ids.contains(&id));
        assert_eq!(
            panel.read(app).sessions[0]
                .terminal
                .as_ref()
                .unwrap()
                .session_id,
            terminal_id
        );
        assert_eq!(
            panel.read(app).sessions[0]
                .terminal
                .as_ref()
                .unwrap()
                .lifecycle,
            SessionLifecycle::Running
        );
    });
    button_bounds(visual, &primary);
}

/// 启动器菜单图标与触发器图标共用左侧对齐线，在浅、深和跟随系统主题均验证。
#[gpui_kit::test]
fn launcher_menu_content_aligns_with_picker(app: &mut TestAppContext) {
    let (_, _, panel, _, visual) = fixture(app);
    for (mode, appearance) in [
        (ThemeMode::Light, WindowAppearance::Light),
        (ThemeMode::Dark, WindowAppearance::Dark),
        (ThemeMode::System, WindowAppearance::Dark),
    ] {
        apply_theme(&panel, visual, mode, appearance);
        let icon = visual
            .debug_bounds("rr-agent-launcher-icon")
            .expect("picker icon rendered");
        click(visual, "agents-launcher-selector");
        let option = visual
            .debug_bounds("rr-agent-launcher-option")
            .expect("menu option rendered");
        assert!(
            (option.left() - icon.left()).abs() <= px(8.),
            "menu option is not aligned: {option:?} {icon:?}"
        );
        visual.simulate_keystrokes("escape");
        draw(visual);
    }
}
