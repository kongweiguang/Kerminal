//! Agent landing 的真实组件交互与布局回归；所有服务使用隔离内存数据。
//!
//! @author kongweiguang

use std::{
    cell::RefCell,
    path::Path,
    process::{Child, Command, Stdio},
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Local};
use gpui_kit::{
    AppContext as _, Bounds, ElementId, Entity, Pixels, ScrollDelta, ScrollWheelEvent,
    TestAppContext, VisualTestContext, WindowAppearance, component::Theme, point, px, size,
};
use kerminal_application::{
    ApplicationTaskExecutor,
    agents::{
        AgentHistoryMode, AgentResumeCapability, AgentResumeRecovery, AgentResumeRecoveryStatus,
        AgentSessionView,
    },
    preferences::{AppColorScheme, InterfaceLanguage, ThemeMode},
    sessions::TerminalPalette,
};
use kerminal_domain::{
    agents::{
        AgentProvider, AgentSession, AgentSessionLaunch, AgentSessionScope, AgentTargetBinding,
        AgentTargetLiveStatus,
    },
    identity::{AgentSessionId, PaneId, SessionId, TabId},
    sessions::{
        LocalSessionTarget, SessionInfo, SessionLifecycle, SessionOwner, SessionTarget,
        TerminalSize,
    },
};
use tokio::runtime::{Builder, Runtime};

use super::{
    AgentsPanel, format_session_time, format_session_time_for_language,
    history_filter_display_text, history_metadata_text, history_row_surface, lifecycle_label,
};
use crate::features::agents::AgentTargetCandidate;
use crate::locale::UiLanguage;
use crate::theme::ThemeTokens;

use crate::features::agents::test_support::FixtureAgentService;

/// 当前目标和全部筛选都在标签后显示计数，零值也必须可见。
///
/// @author kongweiguang
#[test]
fn history_filter_text_keeps_zero_count_visible() {
    assert_eq!(history_filter_display_text("当前目标", 0), "当前目标 0");
    assert_eq!(history_filter_display_text("全部", 0), "全部 0");
}

/// 当前目标筛选省略重复归属，但 tooltip/可访问名称使用的完整身份始终保留 scope；
/// 全部筛选和非匹配行都不能丢归属。
///
/// @author kongweiguang
#[test]
fn history_metadata_keeps_full_identity_while_omitting_redundant_target() {
    let (display, full) = history_metadata_text(
        "Codex",
        "当前目标",
        "10/19 00:00",
        AgentHistoryMode::CurrentTarget,
        true,
    );
    assert_eq!(display, "Codex · 10/19 00:00");
    assert_eq!(full, "Codex · 当前目标 · 10/19 00:00");

    let (display, full) = history_metadata_text(
        "Claude",
        "整个 Kerminal",
        "10/19 00:00",
        AgentHistoryMode::All,
        false,
    );
    assert_eq!(display, full);
    assert!(display.contains("整个 Kerminal"));

    let (display, _) = history_metadata_text(
        "Codex",
        "当前 Tab",
        "10/19 00:00",
        AgentHistoryMode::CurrentTarget,
        false,
    );
    assert!(
        display.contains("当前 Tab"),
        "non-matching row must keep its scope: {display}"
    );
}

/// 选中表面优先于悬停表面，且两者都不等于透明空闲表面；避免把指针经过误认作已附着。
///
/// @author kongweiguang
#[test]
fn history_row_surfaces_distinguish_selected_from_hover() {
    let theme = ThemeTokens::for_scheme(
        WindowAppearance::Dark,
        ThemeMode::Dark,
        AppColorScheme::ALL[0],
    );
    let idle = history_row_surface(theme, false, false);
    let hover = history_row_surface(theme, false, true);
    let selected = history_row_surface(theme, true, false);
    let selected_hover = history_row_surface(theme, true, true);

    assert_eq!(idle, theme.panel.alpha(0.));
    assert_eq!(hover, theme.surface_hover());
    assert_eq!(selected, theme.surface_selected());
    assert_eq!(selected_hover, selected);
    assert_ne!(selected, hover);
    assert_ne!(hover, idle);
}

/// 固定运行中会话和独立终端 owner，确保点击继续只选择原会话，不启动外部 CLI。
fn running_view() -> AgentSessionView {
    let id = AgentSessionId::new();
    let session = AgentSession::new(
        id.clone(),
        AgentProvider::Codex,
        None,
        "Codex".to_owned(),
        // 持久化格式是 Unix epoch 秒字符串；用真实格式固定历史行取 created_at 的回归。
        "1792368000".to_owned(),
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

/// 复用完整会话视图，只替换两个时间字段，让时间格式化用例聚焦 created_at/updated_at
/// 的选择关系，而不必重复搭终端与恢复能力。
///
/// @author kongweiguang
fn view_with_times(created_at: &str, updated_at: &str) -> AgentSessionView {
    let mut view = running_view();
    view.session.created_at = created_at.to_owned();
    view.session.updated_at = updated_at.to_owned();
    view
}

/// 按样本时刻本身计算本地展示，避免用“当前偏移”判断历史时间，跨夏令时也不会误判。
///
/// @author kongweiguang
fn local_label(seconds: i64) -> String {
    DateTime::from_timestamp(seconds, 0)
        .expect("sample instant must be representable")
        .with_timezone(&Local)
        .format("%-m/%-d %H:%M")
        .to_string()
}

/// created_at 的 Unix 秒必须按本地时区显示；updated_at 即使有效也不能替换创建时间。
///
/// @author kongweiguang
#[test]
fn session_time_uses_created_epoch_seconds_and_local_timezone() {
    let created = 1_792_368_000;
    let updated = created + 86_400;
    let view = view_with_times(&created.to_string(), &updated.to_string());
    let displayed = format_session_time(&view);

    assert_eq!(displayed, local_label(created));
    assert_eq!(
        DateTime::from_timestamp(created, 0)
            .unwrap()
            .format("%-m/%-d %H:%M")
            .to_string(),
        "10/19 00:00"
    );

    // 只有样本时刻的本地偏移非零时才要求与 UTC 文本不同；UTC 系统跳过不算验证非 UTC，
    // 但本地时区换算本身仍由 local_label 的期望值覆盖。
    let local_offset = DateTime::from_timestamp(created, 0)
        .unwrap()
        .with_timezone(&Local)
        .offset()
        .local_minus_utc();
    if local_offset != 0 {
        assert_ne!(displayed, "10/19 00:00");
    }
}

/// updated_at 是业务状态变化时间；只要 created_at 有效，展示必须固定为创建时间。
///
/// @author kongweiguang
#[test]
fn session_time_ignores_updated_at() {
    let created = 1_792_368_000;
    let updated = created + 86_400;
    let displayed =
        format_session_time(&view_with_times(&created.to_string(), &updated.to_string()));

    assert_eq!(displayed, local_label(created));
    assert_ne!(displayed, local_label(updated));
}

/// created_at 损坏时不能回退到有效的 updated_at，否则历史行会把状态变化时间伪装成创建时间。
///
/// @author kongweiguang
#[test]
fn session_time_does_not_fallback_to_valid_updated_when_created_is_invalid() {
    let displayed = format_session_time_for_language(
        &view_with_times("not-a-number", "1792368000"),
        UiLanguage::ZhCn,
    );

    assert_eq!(displayed, "时间未知");
    assert_ne!(displayed, local_label(1_792_368_000));
}

/// 空、非数字、小数和越界值都稳定落到未知文案；中英文都验证，避免回退成当前时间。
///
/// @author kongweiguang
#[test]
fn session_time_unknown_for_empty_invalid_and_out_of_range() {
    let invalid_values = [
        "",
        "   ",
        "abc",
        "1792368000.5",
        "9223372036854775808",
        "9223372036854775807",
        "-9223372036854775808",
    ];

    for (language, expected) in [
        (UiLanguage::ZhCn, "时间未知"),
        (UiLanguage::EnUs, "Time unknown"),
    ] {
        for value in invalid_values {
            // updated_at 保持有效，确认兜底不是靠回退另一个字段得到的。
            let displayed =
                format_session_time_for_language(&view_with_times(value, "1792368000"), language);
            assert_eq!(displayed, expected, "value {value:?} in {language:?}");
        }
    }
}

/// 领域没有 0 哨兵，0 仍按 Unix epoch 语义显示为本地时区对应时刻：UTC 是
/// 1970-01-01 00:00，本地偏移可能改变小时甚至跨日，而不是未知。
///
/// @author kongweiguang
#[test]
fn session_time_zero_uses_epoch_semantics() {
    let displayed = format_session_time(&view_with_times("0", "1792368000"));

    assert_eq!(displayed, local_label(0));
    assert_eq!(
        DateTime::from_timestamp(0, 0)
            .unwrap()
            .format("%-m/%-d %H:%M")
            .to_string(),
        "1/1 00:00"
    );
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

/// Kit 的 Root 插件每帧按主题字号重置窗口 rem；只改窗口 rem 会在下一帧被覆盖，
/// 因此 rem 缩放测试必须同时更新主题字号，才能让 rem2 的真实几何生效。
///
/// @author kongweiguang
fn set_rem(visual: &mut VisualTestContext, rem: f32) {
    visual.update(|window, cx| {
        Theme::update(cx, |theme| theme.font_size = px(rem));
        window.set_rem_size(px(rem));
    });
}

/// 主题字号和界面 locale 是进程级全局状态，而 `#[gpui_kit::test]` 默认并行执行；
/// 涉及 rem/locale 的用例必须串行，否则另一用例的 rem2 或英文文案会串进当前帧，
/// 让几何断言随机失败。锁只覆盖这些用例，不改变生产代码。
///
/// @author kongweiguang
static GLOBAL_STYLE_LOCK: Mutex<()> = Mutex::new(());

/// 获取全局样式锁；某个用例 panic 后仍可继续，避免毒化让后续用例连锁失败。
///
/// @author kongweiguang
fn lock_global_style() -> MutexGuard<'static, ()> {
    GLOBAL_STYLE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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

/// 用独立控件中心派发真实鼠标事件，验证行内操作按钮不会冒泡为继续会话。
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

/// 直接替换面板已核验的历史投影，隔离布局测试与异步服务；不触发筛选、刷新或排序。
/// 0 条也复用同一构造，确保测试只观察渲染几何，不伪造额外卡片或占位。
///
/// @author kongweiguang
fn set_history_count(panel: &Entity<AgentsPanel>, visual: &mut VisualTestContext, count: usize) {
    visual.update(|_, app| {
        panel.update(app, |panel, cx| {
            let base = running_view();
            panel.sessions = (0..count)
                .map(|index| {
                    let mut view = base.clone();
                    view.session.agent_session_id = AgentSessionId::new();
                    view.session.title = format!("Chat {index}");
                    view.terminal = None;
                    view.resume_recovery = None;
                    view
                })
                .collect();
            panel.visible_session_ids = panel
                .sessions
                .iter()
                .map(|view| view.session.agent_session_id.clone())
                .collect();
            panel.current_target_count = count;
            panel.all_count = count;
            panel.loading = false;
            panel.sessions_verified = true;
            panel.history_load_error = None;
            cx.notify();
        });
    });
    draw(visual);
}

/// 读取当前可见历史身份，供测试用稳定 ID 定位真实行和操作按钮。
///
/// @author kongweiguang
fn visible_history_ids(
    panel: &Entity<AgentsPanel>,
    visual: &mut VisualTestContext,
) -> Vec<AgentSessionId> {
    visual.update(|_, app| panel.read(app).visible_session_ids.clone())
}

/// 读取真实行的 debug bounds；行 selector 含会话 ID，必须走可接收动态字符串的窗口查询，
/// 不能用只接受 `&'static str` 的测试上下文入口。
///
/// @author kongweiguang
fn history_row_bounds(visual: &mut VisualTestContext, id: &AgentSessionId) -> Bounds<Pixels> {
    let selector = format!("rr-agent-history-row-{}", id.as_str());
    visual
        .update(|window, _| window.debug_selector_bounds(&selector))
        .unwrap_or_else(|| panic!("history row {id:?} missing"))
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

/// 行内编辑、删除与会话继续互不误触；返回后保留同一运行终端。
#[gpui_kit::test]
fn row_actions_and_return_keep_the_same_running_chat(app: &mut TestAppContext) {
    let (runtime, _, panel, id, visual) = fixture(app);
    let rename = format!("agent-session-rename-{}", id.as_str());
    let cancel = format!("agent-session-cancel-{}", id.as_str());
    let primary = format!("agent-session-continue-{}", id.as_str());
    let delete = format!("agent-session-delete-{}", id.as_str());
    let terminal_id = visual.update(|_, app| {
        panel.read(app).sessions[0]
            .terminal
            .as_ref()
            .unwrap()
            .session_id
            .clone()
    });
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

/// 普通行右侧操作槽的按钮间隙仍属于整行主按钮命中区，必须由槽容器挡住；
/// 点击间隙不能继续/绑定，也不能误进改名或删除确认，正文点击仍保持原行为。
///
/// @author kongweiguang
#[gpui_kit::test]
fn history_action_slot_gap_does_not_continue_or_open_actions(app: &mut TestAppContext) {
    let (_runtime, service, panel, id, visual) = fixture(app);
    draw(visual);
    let slot = visual
        .debug_bounds("rr-agent-history-actions")
        .expect("history action slot");
    let rename = button_bounds(visual, &format!("agent-session-rename-{}", id.as_str()));
    let delete = button_bounds(visual, &format!("agent-session-delete-{}", id.as_str()));
    assert!(
        rename.right() <= delete.left(),
        "rename/delete must leave a real gap: {rename:?} {delete:?}"
    );
    let gap_x = px((rename.right().as_f32() + delete.left().as_f32()) / 2.0);
    // 间隙横向位于两个按钮之间；纵向取槽顶、槽中和槽底，证明整个 gap 都被容器挡住。
    for y in [slot.top() + px(2.), slot.center().y, slot.bottom() - px(2.)] {
        visual.simulate_click(point(gap_x, y), Default::default());
        draw(visual);
        visual.update(|_, app| {
            let panel = panel.read(app);
            assert!(panel.selected_agent.is_none(), "gap click must not bind");
            assert!(panel.rename_target.is_none(), "gap click must not rename");
            assert!(
                panel.archive_confirmation_owner.is_none(),
                "gap click must not open delete confirmation"
            );
        });
    }
    assert_eq!(service.unsupported_operation_count("create"), 0);
    assert_eq!(service.unsupported_operation_count("resume"), 0);

    // 正文仍走原有继续路径，证明槽容器只挡住间隙而不影响整行主按钮。
    let primary = format!("agent-session-continue-{}", id.as_str());
    let row = button_bounds(visual, &primary);
    visual.simulate_click(
        point(row.left() + px(16.), row.top() + px(16.)),
        Default::default(),
    );
    draw(visual);
    visual.update(|_, app| assert_eq!(panel.read(app).selected_agent.as_ref(), Some(&id)));
}

/// 构造与真实 Tab 切换同形的目标候选；测试借此区分“只更新候选”和隐式重读历史。
///
/// @author kongweiguang
fn switched_target_candidate() -> AgentTargetCandidate {
    AgentTargetCandidate {
        binding: AgentTargetBinding {
            binding_id: Some("fixture-target-switch".to_owned()),
            generation: 7,
            pane_id: Some(PaneId::new()),
            tab_id: Some(TabId::new()),
            terminal_session_id: Some(SessionId::new()),
            target_ref: Some("local:fixture".to_owned()),
            target_kind: Some("local".to_owned()),
            cwd: Some("workspace".to_owned()),
            shell: Some("bash".to_owned()),
            live_status: AgentTargetLiveStatus::Ready,
            last_seen_at: None,
        },
        label: "Fixture target".to_owned(),
    }
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

/// 窄工具栏保留启动与筛选的独立命中区；三个主题均检查真实按钮而非内部布局常量。
///
/// @author kongweiguang
#[gpui_kit::test]
fn launcher_and_history_controls_fit_narrow_tool_panels(app: &mut TestAppContext) {
    let (_, _, panel, _, visual) = fixture(app);
    for (mode, appearance) in [
        (ThemeMode::Light, WindowAppearance::Light),
        (ThemeMode::Dark, WindowAppearance::Dark),
        (ThemeMode::System, WindowAppearance::Dark),
    ] {
        apply_theme(&panel, visual, mode, appearance);
        for width in [240., 320., 360.] {
            visual.simulate_resize(size(px(width), px(700.)));
            draw(visual);
            let picker = visual
                .debug_bounds("rr-agent-picker")
                .expect("launcher selector rendered");
            let start = button_bounds(visual, "agents-launcher-start");
            let refresh = button_bounds(visual, "agents-history-refresh");
            let current = button_bounds(visual, "agents-history-current");
            let all = button_bounds(visual, "agents-history-all");
            assert!(
                picker.right() <= start.left() + px(0.5) && start.right() <= px(width) + px(0.5),
                "launcher controls overlap or escape {width}: {picker:?} {start:?}"
            );
            assert!(
                refresh.right() <= current.left() + px(0.5)
                    && current.right() <= all.left() + px(0.5)
                    && all.right() <= px(width) + px(0.5),
                "history controls overlap or escape {width}: {refresh:?} {current:?} {all:?}"
            );
            visual.update(|window, _| {
                let id: ElementId = "agents-launcher-start".into();
                let start = gpui_kit::base::test_support::find(window, &[], &id)
                    .expect("launcher start accessible node");
                assert!(matches!(start.label(), Some("启动 Agent" | "Start Agent")));
            });
        }
    }
}

/// 分屏切换终端只更新首选候选：不重读历史、不重置筛选和改名草稿；真实 scope 变化仍会刷新。
///
/// @author kongweiguang
#[gpui_kit::test]
fn candidate_change_updates_target_without_reloading_history(app: &mut TestAppContext) {
    let (runtime, service, panel, id, visual) = fixture(app);

    click(visual, "agents-history-all");
    pump(&runtime, visual);
    let loads_after_filter = service.history_loads();
    visual.update(|_, app| {
        assert_eq!(panel.read(app).history_mode, AgentHistoryMode::All);
        assert!(loads_after_filter >= 2, "filter switch must reload history");
    });

    let rename = format!("agent-session-rename-{}", id.as_str());
    click(visual, &rename);
    let rename_before = visual.update(|_, app| panel.read(app).rename_target.clone());
    assert!(rename_before.is_some(), "rename editor must be open");

    let candidate = switched_target_candidate();
    visual.update(|_, app| {
        panel.update(app, |panel, cx| {
            panel.set_target_candidate(Some(candidate), cx);
            panel.set_target_summary(Some("Another terminal".to_owned()), cx);
        });
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        let panel = panel.read(app);
        assert_eq!(
            service.history_loads(),
            loads_after_filter,
            "candidate changes must not reload history"
        );
        assert!(!panel.loading);
        assert!(panel.sessions_verified);
        assert_eq!(panel.history_mode, AgentHistoryMode::All);
        assert_eq!(panel.rename_target, rename_before);
    });

    visual.update(|_, app| {
        panel.update(app, |panel, cx| {
            panel.set_scope(
                AgentSessionScope::Tab {
                    tab_id: TabId::new(),
                },
                cx,
            );
        });
    });
    pump(&runtime, visual);
    visual.update(|_, app| {
        let panel = panel.read(app);
        assert_eq!(
            service.history_loads(),
            loads_after_filter + 1,
            "real filter scope changes must still reload history"
        );
        assert_eq!(panel.history_mode, AgentHistoryMode::CurrentTarget);
    });
}

/// 入口“进入”在存在运行中会话时仍只创建新会话；不选中旧会话、不恢复、不重绑。
///
/// @author kongweiguang
#[gpui_kit::test]
fn entry_always_creates_a_new_chat(app: &mut TestAppContext) {
    let (runtime, service, panel, id, visual) = fixture(app);

    click(visual, "agents-launcher-start");
    pump(&runtime, visual);
    visual.update(|_, app| {
        let panel = panel.read(app);
        assert!(panel.selected_agent.is_none());
        assert_eq!(service.unsupported_operation_count("create"), 1);
        assert_eq!(service.unsupported_operation_count("resume"), 0);
        assert_eq!(
            service.unsupported_operation_count("bind_preferred_target"),
            0
        );
    });

    // 历史行仍可显式继续同一运行会话，且不产生第二次创建。
    click(visual, &format!("agent-session-continue-{}", id.as_str()));
    draw(visual);
    visual.update(|_, app| {
        assert_eq!(panel.read(app).selected_agent.as_ref(), Some(&id));
        assert_eq!(service.unsupported_operation_count("create"), 1);
    });
}

/// 历史条数与筛选变化不能移动入口；固定三条视口让第 4 条留在内部滚动范围。
///
/// @author kongweiguang
#[gpui_kit::test]
fn landing_entry_stays_fixed_across_history_counts_and_three_rows_scroll(app: &mut TestAppContext) {
    let _style_guard = lock_global_style();
    let (_runtime, _service, panel, _id, visual) = fixture(app);
    for (rem, height) in [(16., 700.), (32., 1400.)] {
        set_rem(visual, rem);
        visual.simulate_resize(size(px(360.), px(height)));
        draw(visual);
        assert_history_anchor_and_three_row_viewport(&panel, visual, rem);
    }
}

/// 指定 rem 下入口/标题/视口锚点不随条数变化，且三条完整、第四条在内部滚动范围外。
/// rem 会改变 p_2/gap_1/按钮高度，必须分别跑，不能只用默认 rem16 推断 rem2。
///
/// @author kongweiguang
fn assert_history_anchor_and_three_row_viewport(
    panel: &Entity<AgentsPanel>,
    visual: &mut VisualTestContext,
    rem: f32,
) {
    let mut baseline: Option<(f32, f32, f32, f32)> = None;
    let mut list_width: Option<f32> = None;
    // 只比较外层 list 宽不足以证明 3→4 不回流；标题、操作槽和元数据的实际 x/width
    // 必须逐槽稳定，否则 gutter 或状态收缩会把内容横向推挤。
    let mut row_geometry: Option<[f32; 10]> = None;
    for count in [0, 1, 2, 3, 6, 8] {
        set_history_count(panel, visual, count);
        let entry = button_bounds(visual, "agents-launcher-start");
        let picker = visual
            .debug_bounds("rr-agent-picker")
            .expect("launcher picker");
        let heading = visual
            .debug_bounds("rr-agent-history-heading")
            .expect("history heading");
        let body = visual
            .debug_bounds("rr-agent-history-body")
            .expect("history body");
        let current = (
            entry.top().as_f32(),
            picker.top().as_f32(),
            heading.top().as_f32(),
            body.top().as_f32(),
        );
        if let Some(base) = baseline {
            assert!(
                (base.0 - current.0).abs() <= 0.5
                    && (base.1 - current.1).abs() <= 0.5
                    && (base.2 - current.2).abs() <= 0.5
                    && (base.3 - current.3).abs() <= 0.5,
                "history count {count} moved the landing anchor: {base:?} vs {current:?}"
            );
        } else {
            baseline = Some(current);
        }

        let ids = visible_history_ids(panel, visual);
        assert_eq!(ids.len(), count);
        if count == 0 {
            assert!(
                visual.debug_bounds("rr-agent-history-list").is_none(),
                "zero history must not render a fake list"
            );
            assert!(
                visual.debug_bounds("rr-agent-empty-state").is_some(),
                "zero history keeps the reserved viewport empty state"
            );
            continue;
        }
        let list = visual
            .debug_bounds("rr-agent-history-list")
            .expect("history list");
        // 所有条数共用同一视口宽度；滚动条 gutter 常驻，3→4 不横向回流。
        if let Some(base) = list_width {
            assert!(
                (base - list.size.width.as_f32()).abs() <= 0.5,
                "history count {count} changed the list width: {base} vs {}",
                list.size.width.as_f32()
            );
        } else {
            list_width = Some(list.size.width.as_f32());
        }
        if count >= 1 {
            let row = history_row_bounds(visual, &ids[0]);
            // 两行行式列表必须明显低于原三层卡片；同时保留真实按钮/输入的可点击高度。
            assert!(
                row.size.height >= px(rem * 4.0) && row.size.height <= px(rem * 4.5),
                "history row is not compact at rem {rem}: {row:?}"
            );
            let title = visual
                .debug_bounds("rr-agent-history-title")
                .expect("history title");
            assert!(
                title.left() - row.left() <= px(rem * 0.5 + 1.0),
                "title must start at the row padding, not after a duplicate icon: {title:?} {row:?}"
            );
            let status = visual
                .debug_bounds("rr-agent-history-status")
                .expect("history status");
            // 状态只保留文字行高；出现 badge 内外边距或描边时这里会超预算。
            assert!(
                status.size.height <= px(rem * 0.95 + 1.0),
                "status must be text-only at rem {rem}: {status:?}"
            );
            // 右侧 gutter 必须覆盖 Kit 滚动条宽度，thumb 出现时不压住状态文字。
            assert!(
                row.right() - status.right() >= px(15.5),
                "scrollbar gutter missing at rem {rem}: {status:?} {row:?}"
            );
            let rename =
                button_bounds(visual, &format!("agent-session-rename-{}", ids[0].as_str()));
            let delete =
                button_bounds(visual, &format!("agent-session-delete-{}", ids[0].as_str()));
            let metadata = visual
                .debug_bounds("rr-agent-history-metadata")
                .expect("history metadata");
            // 状态可以收缩省略，但不能把元数据挤成零宽；完整身份仍由 tooltip 承载。
            assert!(
                metadata.size.width > px(0.5),
                "metadata budget collapsed at rem {rem}: {metadata:?} {status:?}"
            );
            assert!(
                metadata.right() <= status.left() + px(0.5),
                "metadata overlaps status at rem {rem}: {metadata:?} {status:?}"
            );
            let geometry = [
                title.left().as_f32(),
                title.size.width.as_f32(),
                rename.left().as_f32(),
                rename.size.width.as_f32(),
                delete.left().as_f32(),
                delete.size.width.as_f32(),
                metadata.left().as_f32(),
                metadata.size.width.as_f32(),
                status.left().as_f32(),
                status.size.width.as_f32(),
            ];
            if let Some(base) = row_geometry {
                for (index, (expected, actual)) in base.iter().zip(geometry.iter()).enumerate() {
                    assert!(
                        (expected - actual).abs() <= 0.5,
                        "history count {count} reflowed row slot {index} at rem {rem}: \
                         {expected} vs {actual}"
                    );
                }
            } else {
                row_geometry = Some(geometry);
            }
        }
        if count >= 3 {
            for id in ids.iter().take(3) {
                let row = history_row_bounds(visual, id);
                assert!(
                    row.top() >= list.top() - px(0.5) && row.bottom() <= list.bottom() + px(0.5),
                    "first three rows must be complete at {count}: {row:?} {list:?}"
                );
            }
            if count > 3 {
                let fourth_selector = format!("rr-agent-history-row-{}", ids[3].as_str());
                let fourth =
                    visual.update(|window, _| window.debug_selector_bounds(&fourth_selector));
                assert!(
                    fourth.is_none_or(|row| row.top() >= list.bottom() - px(1.5)),
                    "fourth row must stay below the fixed viewport: {fourth:?} {list:?}"
                );
            }
        }
    }

    // 8 条时从操作槽间隙滚到最后一条：槽容器只挡鼠标点击，滚轮必须继续透传到列表；
    // 若 block_mouse_except_scroll 误挡滚轮，这里会停在顶部并让最后一行不可达。
    set_history_count(panel, visual, 8);
    let entry_before = button_bounds(visual, "agents-launcher-start");
    let heading_before = visual
        .debug_bounds("rr-agent-history-heading")
        .expect("history heading before scroll");
    let list = visual
        .debug_bounds("rr-agent-history-list")
        .expect("history list before scroll");
    // 槽容器是每行重复的 selector，debug_bounds 只能拿到最后一行（视口外），
    // 因此用首条可见行的真实改名/删除按钮定位它们之间的 gap；该 gap 属于被
    // block_mouse_except_scroll 挡住的槽容器，正好验证滚轮仍能透传到列表。
    let first = visible_history_ids(panel, visual)
        .first()
        .expect("visible history id before scroll")
        .clone();
    let rename = button_bounds(visual, &format!("agent-session-rename-{}", first.as_str()));
    let delete = button_bounds(visual, &format!("agent-session-delete-{}", first.as_str()));
    let gap = point(
        px((rename.right().as_f32() + delete.left().as_f32()) / 2.0),
        rename.center().y,
    );
    visual.simulate_event(ScrollWheelEvent {
        position: gap,
        delta: ScrollDelta::Pixels(point(px(0.), px(-10_000.))),
        ..Default::default()
    });
    draw(visual);
    let ids = visible_history_ids(panel, visual);
    let last = history_row_bounds(visual, ids.last().expect("last history id"));
    assert!(
        last.top() >= list.top() - px(0.5) && last.bottom() <= list.bottom() + px(1.5),
        "last history row must be fully reachable: {last:?} {list:?}"
    );
    assert_eq!(
        button_bounds(visual, "agents-launcher-start").top(),
        entry_before.top(),
        "scrolling history must not move the entry"
    );
    assert_eq!(
        visual
            .debug_bounds("rr-agent-history-heading")
            .expect("history heading after scroll")
            .top(),
        heading_before.top(),
        "scrolling history must not move the heading"
    );
}

/// 筛选 6->1->0、读取错误和行内改名都留在固定历史视口内，入口位置不变。
///
/// @author kongweiguang
#[gpui_kit::test]
fn history_filter_error_and_rename_keep_entry_fixed(app: &mut TestAppContext) {
    let _style_guard = lock_global_style();
    let (_runtime, _service, panel, _id, visual) = fixture(app);
    for (rem, height) in [(16., 700.), (32., 1400.)] {
        set_rem(visual, rem);
        visual.simulate_resize(size(px(360.), px(height)));
        draw(visual);
        assert_filter_error_and_rename_keep_entry_fixed(&panel, visual);
    }
}

/// 指定 rem 下筛选、读取错误和行内改名都不移动入口；普通/rename 行的按钮、输入、
/// 元数据和状态文字必须落在同一固定行高内，不能只让行外框固定而让子内容溢出。
///
/// @author kongweiguang
fn assert_filter_error_and_rename_keep_entry_fixed(
    panel: &Entity<AgentsPanel>,
    visual: &mut VisualTestContext,
) {
    set_history_count(panel, visual, 6);
    let entry = button_bounds(visual, "agents-launcher-start");
    let heading = visual
        .debug_bounds("rr-agent-history-heading")
        .expect("history heading");
    for count in [1, 0] {
        set_history_count(panel, visual, count);
        assert_eq!(
            button_bounds(visual, "agents-launcher-start").top(),
            entry.top(),
            "filter to {count} moved the entry"
        );
        assert_eq!(
            visual
                .debug_bounds("rr-agent-history-heading")
                .expect("history heading")
                .top(),
            heading.top(),
            "filter to {count} moved the heading"
        );
    }

    set_history_count(panel, visual, 1);
    let id = visible_history_ids(panel, visual)[0].clone();
    let normal_row = history_row_bounds(visual, &id);
    for button in [
        format!("agent-session-rename-{}", id.as_str()),
        format!("agent-session-delete-{}", id.as_str()),
    ] {
        let bounds = button_bounds(visual, &button);
        assert!(
            bounds.top() >= normal_row.top() && bounds.bottom() <= normal_row.bottom(),
            "normal row action clipped: {button} {bounds:?} {normal_row:?}"
        );
    }
    for selector in ["rr-agent-history-metadata", "rr-agent-history-status"] {
        let bounds = visual.debug_bounds(selector).expect(selector);
        assert!(
            bounds.top() >= normal_row.top() && bounds.bottom() <= normal_row.bottom(),
            "normal row content clipped: {selector} {bounds:?} {normal_row:?}"
        );
    }

    click(visual, &format!("agent-session-rename-{}", id.as_str()));
    let rename_row = history_row_bounds(visual, &id);
    assert_eq!(
        rename_row.size.height, normal_row.size.height,
        "rename row must keep the normal row height"
    );
    for button in [
        format!("agent-session-save-{}", id.as_str()),
        format!("agent-session-cancel-{}", id.as_str()),
    ] {
        let bounds = button_bounds(visual, &button);
        assert!(
            bounds.top() >= rename_row.top() && bounds.bottom() <= rename_row.bottom(),
            "rename action clipped: {button} {bounds:?} {rename_row:?}"
        );
    }
    let input = visual
        .debug_bounds("rr-agent-rename-input")
        .expect("rename input");
    assert!(
        input.top() >= rename_row.top() && input.bottom() <= rename_row.bottom(),
        "rename input clipped: {input:?} {rename_row:?}"
    );
    for selector in ["rr-agent-history-metadata", "rr-agent-history-status"] {
        let bounds = visual.debug_bounds(selector).expect(selector);
        assert!(
            bounds.top() >= rename_row.top() && bounds.bottom() <= rename_row.bottom(),
            "rename row content clipped: {selector} {bounds:?} {rename_row:?}"
        );
    }
    click(visual, &format!("agent-session-cancel-{}", id.as_str()));

    let entry_before_error = button_bounds(visual, "agents-launcher-start");
    let heading_before_error = visual
        .debug_bounds("rr-agent-history-heading")
        .expect("history heading before error");
    visual.update(|_, app| {
        panel.update(app, |panel, cx| {
            panel.loading = false;
            panel.sessions_verified = false;
            panel.history_load_error = Some("isolated history read failed".to_owned());
            cx.notify();
        });
    });
    draw(visual);
    assert!(
        visual.debug_bounds("rr-agent-history-error").is_some(),
        "history read error must stay inside the reserved viewport"
    );
    button_bounds(visual, "agents-refresh");
    assert_eq!(
        button_bounds(visual, "agents-launcher-start").top(),
        entry_before_error.top(),
        "history read error moved the entry"
    );
    assert_eq!(
        visual
            .debug_bounds("rr-agent-history-heading")
            .expect("history heading during error")
            .top(),
        heading_before_error.top(),
        "history read error moved the heading"
    );

    visual.update(|_, app| {
        panel.update(app, |panel, cx| {
            panel.sessions_verified = true;
            panel.history_load_error = None;
            cx.notify();
        });
    });
    draw(visual);
    assert_eq!(
        button_bounds(visual, "agents-launcher-start").top(),
        entry_before_error.top(),
        "retry clearing the error moved the entry"
    );
}

/// 入口与历史作为一组在可用高度内垂直居中；固定三槽历史视口保持入口落点，
/// 极短窗仍由外层滚动兜底，窄栏不横向溢出。
///
/// @author kongweiguang
#[gpui_kit::test]
fn landing_group_is_centered_and_scrolls_when_tall(app: &mut TestAppContext) {
    let (_runtime, _service, _panel, _id, visual) = fixture(app);

    visual.simulate_resize(size(px(320.), px(900.)));
    draw(visual);
    let column = visual
        .debug_bounds("rr-agent-landing-column")
        .expect("landing column");
    let content = visual
        .debug_bounds("rr-agent-landing-scroll-content")
        .expect("scroll content");
    let top_gap = (column.top() - content.top()).as_f32();
    let bottom_gap = (content.bottom() - column.bottom()).as_f32();
    assert!(
        (top_gap - bottom_gap).abs() <= 1.,
        "landing group must be vertically centered: {top_gap} vs {bottom_gap}"
    );
    assert!(top_gap > 0., "centered group must not stick to the top");
    button_bounds(visual, "agents-launcher-start");

    visual.simulate_resize(size(px(240.), px(160.)));
    draw(visual);
    let scroll = visual
        .debug_bounds("rr-agent-landing-scroll")
        .expect("landing scroll viewport");
    let content = visual
        .debug_bounds("rr-agent-landing-scroll-content")
        .expect("scroll content");
    assert!(
        content.size.height > scroll.size.height,
        "tall content must exceed the scroll viewport: {content:?} {scroll:?}"
    );
    let column = visual
        .debug_bounds("rr-agent-landing-column")
        .expect("landing column");
    assert!(
        column.top() >= content.top() - px(0.5),
        "centered content must not clip the column top: {column:?} {content:?}"
    );
    assert!(
        column.left() >= px(0.) && column.right() <= px(240.) + px(0.5),
        "narrow column escaped the rail: {column:?}"
    );
    visual.simulate_event(ScrollWheelEvent {
        position: scroll.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-10_000.))),
        ..Default::default()
    });
    draw(visual);
    let scrolled = visual
        .debug_bounds("rr-agent-landing-column")
        .expect("landing column after scroll");
    assert!(
        scrolled.top() < column.top(),
        "wheel must move the tall landing content: {column:?} {scrolled:?}"
    );
    assert!(
        scrolled.bottom() <= scroll.bottom() + px(0.5),
        "scroll range must reach the column bottom: {scrolled:?} {scroll:?}"
    );
}

/// 进程级 locale 与同测试二进制内并行运行的 connections 用例共享：它们会在渲染和断言时
/// 读取当前语言，而本用例需要切换中英文。父用例只负责启动、限时等待并回收子进程，自身
/// 不读写 locale；子进程用 `--exact` 只执行原用例，避免过滤为空时假绿。
///
/// @author kongweiguang
#[test]
fn history_title_keeps_readable_width_in_narrow_rem_scaled_rows_isolated() {
    let test_name = exact_test_name("history_title_keeps_readable_width_in_narrow_rem_scaled_rows");
    // 输出落到本次独占创建的临时目录而不是管道：即使子进程无法回收，读取也只是有界
    // 文件读取，不需要 join 读取线程，也不会因管道缓冲写满而阻塞；独占目录同时避免
    // 截断 PID 复用后遗留的旧日志。
    let output_dir = child_output_dir();
    let stdout_path = output_dir.join("stdout.log");
    let stderr_path = output_dir.join("stderr.log");
    let stdout_file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&stdout_path)
    {
        Ok(file) => file,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&output_dir);
            panic!("创建子进程 stdout 临时文件失败: {error}");
        }
    };
    let stderr_file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&stderr_path)
    {
        Ok(file) => file,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&output_dir);
            panic!("创建子进程 stderr 临时文件失败: {error}");
        }
    };
    let mut child = match Command::new(std::env::current_exe().expect("当前测试程序"))
        .args(["--exact", test_name.as_str(), "--nocapture"])
        .env(LOCALE_LAYOUT_CHILD_ENV, "1")
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&output_dir);
            panic!("启动隔离语言布局子进程失败: {error}");
        }
    };
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                fail_isolated_child(&mut child, pid, "超时", None, &output_dir);
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                fail_isolated_child(&mut child, pid, "等待失败", Some(&error), &output_dir);
            }
        }
    };
    let stdout = read_child_file(&stdout_path);
    let stderr = read_child_file(&stderr_path);
    let _ = std::fs::remove_dir_all(&output_dir);
    assert!(
        status.success(),
        "隔离语言布局子进程失败: {status:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains(test_name.as_str()) && stdout.contains("1 passed"),
        "隔离子进程必须实际执行 1 个用例，不能因过滤为空假绿:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains(LOCALE_LAYOUT_CHILD_MARKER),
        "隔离子进程未完成双语布局断言:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// 子进程哨兵：只有父用例通过 `Command.env` 注入时才执行真实双语布局；普通并行运行时
/// 直接返回，避免进程级 locale 污染同二进制内的其它用例。
///
/// @author kongweiguang
const LOCALE_LAYOUT_CHILD_ENV: &str = "KERMINAL_AGENT_LOCALE_LAYOUT_CHILD";

/// 子进程完成标记：父用例用它确认 `--exact` 命中的用例确实跑完，而不只是退出码为 0。
///
/// @author kongweiguang
const LOCALE_LAYOUT_CHILD_MARKER: &str = "KERMINAL_AGENT_LOCALE_LAYOUT_CHILD_OK";

/// libtest 的用例名不含 crate 前缀，因此从 `module_path!()` 去掉首段 crate 名，
/// 避免模块搬迁后 `--exact` 静默失配。
///
/// @author kongweiguang
fn exact_test_name(test: &str) -> String {
    let module = module_path!();
    let (_, module) = module
        .split_once("::")
        .unwrap_or_else(|| panic!("测试模块路径缺少 crate 前缀: {module}"));
    format!("{module}::{test}")
}

/// 每次运行独占创建一个临时目录：`create_dir` 在路径已存在时失败，因此不会截断 PID
/// 复用或预置同名路径的旧日志；目录名用 pid + 时间戳 + 重试序号兜底同刻碰撞。
///
/// @author kongweiguang
fn child_output_dir() -> std::path::PathBuf {
    let base = std::env::temp_dir();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    for attempt in 0..100 {
        let path = base.join(format!(
            "kerminal-agent-layout-{}-{stamp}-{attempt}",
            std::process::id()
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => return path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("创建子进程输出目录失败: {path:?}: {error}"),
        }
    }
    panic!("无法创建唯一的子进程输出目录: {base:?}");
}

/// 读取子进程输出文件；读取失败不覆盖原始失败原因，用占位文案保留诊断。
///
/// @author kongweiguang
fn read_child_file(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => format!("<读取失败: {error}>"),
    }
}

/// 超时或 `try_wait` 失败后只回收本进程启动的 child：先 `kill`，再在固定宽限窗口内
/// `try_wait`。`kill` 失败可能表示进程已退出，也可能是权限等原因，不能据此断言已退出；
/// 这里不使用无界 `wait`，窗口结束仍未确认时显式报告 pid、错误和未确认回收，并保留输出。
///
/// @author kongweiguang
fn fail_isolated_child(
    child: &mut Child,
    pid: u32,
    reason: &str,
    cause: Option<&std::io::Error>,
    output_dir: &Path,
) -> ! {
    let kill_error = child.kill().err();
    let grace_deadline = Instant::now() + Duration::from_secs(5);
    let mut reaped = None;
    let mut wait_error = None;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                reaped = Some(status);
                break;
            }
            Ok(None) if Instant::now() < grace_deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => break,
            Err(error) => {
                wait_error = Some(error);
                break;
            }
        }
    }
    let stdout = read_child_file(&output_dir.join("stdout.log"));
    let stderr = read_child_file(&output_dir.join("stderr.log"));
    let _ = std::fs::remove_dir_all(output_dir);
    let reclamation = match reaped {
        Some(status) => format!("已回收: {status:?}"),
        None => "未确认回收".to_owned(),
    };
    panic!(
        "隔离语言布局子进程{reason} (pid={pid}): cause={cause:?} kill={kill_error:?} \
         wait={wait_error:?} {reclamation}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// 窄栏/rem2 下标题与常驻操作同行，标题保留可读预算且不与操作重叠；
/// rename 输入与保存/取消共用同一行和同一右侧槽位，行高不变。
/// 双语布局会写进程级 locale，因此只在父用例启动的隔离子进程中执行。
///
/// @author kongweiguang
#[gpui_kit::test]
fn history_title_keeps_readable_width_in_narrow_rem_scaled_rows(app: &mut TestAppContext) {
    // 进程级 locale 与同测试二进制内并行运行的 connections 用例共享；普通并行运行时
    // 直接返回，只有父用例启动的隔离子进程才执行真实双语布局。
    if std::env::var_os(LOCALE_LAYOUT_CHILD_ENV).is_none() {
        return;
    }
    let _style_guard = lock_global_style();
    let (_runtime, _service, panel, id, visual) = fixture(app);
    visual.update(|_, app| {
        panel.update(app, |panel, cx| {
            panel.sessions[0].session.title =
                "A long running agent session title that must remain readable".to_owned();
            cx.notify();
        });
    });
    // landing 内容列本身 max_w 320px，rem2 下窗口继续加宽也不会超过该列；240px 窗口
    // 在 rem2 下只剩 120 逻辑 px，低于已批准的 240px 默认字号窄栏起点，不承诺可读。
    // rem2 按实际内容列宽验证，标题/输入保留 144px（72 逻辑 px）的诚实预算。
    for (width, rem, min_title) in [
        (320., 16., 160.),
        (240., 16., 100.),
        (480., 32., 140.),
        (640., 32., 140.),
    ] {
        set_rem(visual, rem);
        visual.simulate_resize(size(px(width), px(700.)));
        draw(visual);
        let title = visual
            .debug_bounds("rr-agent-history-title")
            .expect("history title row");
        assert!(
            title.size.width.as_f32() >= min_title,
            "title too narrow at {width}/{rem}: {title:?}"
        );
        // 行高固定后还必须证明子内容仍在行内：rem2 下 p_2/gap_1/图标按钮都会放大。
        let row = history_row_bounds(visual, &id);
        assert!(
            title.top() >= row.top() && title.bottom() <= row.bottom(),
            "title clipped at {width}/{rem}: {title:?} {row:?}"
        );
        let rename_bounds = button_bounds(visual, &format!("agent-session-rename-{}", id.as_str()));
        let delete_bounds = button_bounds(visual, &format!("agent-session-delete-{}", id.as_str()));
        assert!(
            title.right() <= rename_bounds.left() - px(1.),
            "title overlaps the action slot at {width}/{rem}: {title:?} {rename_bounds:?}"
        );
        for button in [
            format!("agent-session-rename-{}", id.as_str()),
            format!("agent-session-delete-{}", id.as_str()),
        ] {
            let bounds = button_bounds(visual, &button);
            assert!(
                bounds.left() >= px(0.) && bounds.right() <= px(width) + px(0.5),
                "action overflow at {width}/{rem}: {button} {bounds:?}"
            );
            assert!(
                bounds.top() >= row.top() && bounds.bottom() <= row.bottom(),
                "action clipped at {width}/{rem}: {button} {bounds:?} {row:?}"
            );
        }
        for selector in ["rr-agent-history-metadata", "rr-agent-history-status"] {
            let bounds = visual.debug_bounds(selector).expect(selector);
            assert!(
                bounds.top() >= row.top() && bounds.bottom() <= row.bottom(),
                "row content clipped at {width}/{rem}: {selector} {bounds:?} {row:?}"
            );
        }
        let metadata = visual
            .debug_bounds("rr-agent-history-metadata")
            .expect("history metadata");
        let status = visual
            .debug_bounds("rr-agent-history-status")
            .expect("history status");
        // 第二行左右两段必须互不重叠；状态不能被单行元数据挤到下一行或压住文字。
        assert!(
            metadata.right() <= status.left() + px(0.5),
            "metadata overlaps status at {width}/{rem}: {metadata:?} {status:?}"
        );
        // 第一行标题和两个操作都必须整体位于第二行之上，不能跨行重叠。
        let first_row_bottom = title
            .bottom()
            .max(rename_bounds.bottom())
            .max(delete_bounds.bottom());
        assert!(
            first_row_bottom <= metadata.top() + px(0.5),
            "first row overlaps the identity row at {width}/{rem}: {first_row_bottom} vs {metadata:?}"
        );
        assert!(
            status.size.height <= px(rem * 0.95 + 1.0),
            "status must stay text-only at {width}/{rem}: {status:?}"
        );
        assert!(
            row.right() - status.right() >= px(15.5),
            "scrollbar gutter missing at {width}/{rem}: {status:?} {row:?}"
        );
    }

    // fixture 默认是 Running 终端，lifecycle_label 会优先返回“运行中”，把
    // MetadataUnavailable 的长状态遮住。这里显式清空 terminal，同时保留
    // MetadataUnavailable 且不设置 recovery，既让状态落到真实长文案，也保留改名入口。
    visual.update(|_, app| {
        panel.update(app, |panel, cx| {
            panel.sessions[0].terminal = None;
            cx.notify();
        });
    });
    let original_locale = gpui_kit::component::locale().to_string();
    // 中英文都验证：英文“Check recovery metadata”更长，最容易在窄栏挤压元数据或越过 gutter。
    // 240px 是默认字号下的窄栏起点；rem2 用当前内容列上限 320px 对应的 480px 窗口验证。
    for (language, locale) in [
        (InterfaceLanguage::ZhCn, "zh-CN"),
        (InterfaceLanguage::EnUs, "en-US"),
    ] {
        crate::locale::apply_interface_language(language, "");
        for (width, rem) in [(240., 16.), (480., 32.)] {
            set_rem(visual, rem);
            visual.simulate_resize(size(px(width), px(700.)));
            draw(visual);
            let expected_status =
                crate::locale::text("恢复信息待检查", "Check recovery metadata").to_string();
            visual.update(|_, app| {
                assert_eq!(
                    lifecycle_label(&panel.read(app).sessions[0]),
                    expected_status,
                    "fixture must render the long lifecycle status at {width}/{rem} {locale}"
                );
            });
            let normal_row = history_row_bounds(visual, &id);
            let normal_rename =
                button_bounds(visual, &format!("agent-session-rename-{}", id.as_str()));
            let normal_delete =
                button_bounds(visual, &format!("agent-session-delete-{}", id.as_str()));
            let normal_metadata = visual
                .debug_bounds("rr-agent-history-metadata")
                .expect("normal metadata");
            let normal_status = visual
                .debug_bounds("rr-agent-history-status")
                .expect("normal status");
            // 长状态在窄栏普通态必须有界：元数据保留非零预算，状态不越过 gutter。
            assert!(
                normal_metadata.size.width > px(0.5),
                "normal metadata collapsed at {width}/{rem} {locale}: {normal_metadata:?} {normal_status:?}"
            );
            assert!(
                normal_metadata.right() <= normal_status.left() + px(0.5),
                "normal metadata overlaps status at {width}/{rem} {locale}: {normal_metadata:?} {normal_status:?}"
            );
            assert!(
                normal_status.top() >= normal_row.top()
                    && normal_status.bottom() <= normal_row.bottom(),
                "normal status clipped at {width}/{rem} {locale}: {normal_status:?} {normal_row:?}"
            );
            assert!(
                normal_row.right() - normal_status.right() >= px(15.5),
                "normal status crosses the scrollbar gutter at {width}/{rem} {locale}: {normal_status:?} {normal_row:?}"
            );
            click(visual, &format!("agent-session-rename-{}", id.as_str()));
            draw(visual);
            let rename_row = history_row_bounds(visual, &id);
            assert_eq!(
                rename_row.size.height, normal_row.size.height,
                "rename row must keep the normal row height at {width}/{rem} {locale}: {rename_row:?} {normal_row:?}"
            );
            let rename_metadata = visual
                .debug_bounds("rr-agent-history-metadata")
                .expect("rename metadata");
            let rename_status = visual
                .debug_bounds("rr-agent-history-status")
                .expect("rename status");
            // rename 态第二行仍保留状态；长状态同样不能把元数据挤成零宽或越过 gutter。
            assert!(
                rename_metadata.size.width > px(0.5),
                "rename metadata collapsed at {width}/{rem} {locale}: {rename_metadata:?} {rename_status:?}"
            );
            assert!(
                rename_metadata.right() <= rename_status.left() + px(0.5),
                "rename metadata overlaps status at {width}/{rem} {locale}: {rename_metadata:?} {rename_status:?}"
            );
            assert!(
                rename_status.top() >= rename_row.top()
                    && rename_status.bottom() <= rename_row.bottom(),
                "rename status clipped at {width}/{rem} {locale}: {rename_status:?} {rename_row:?}"
            );
            assert!(
                rename_row.right() - rename_status.right() >= px(15.5),
                "rename status crosses the scrollbar gutter at {width}/{rem} {locale}: {rename_status:?} {rename_row:?}"
            );
            let input = visual
                .debug_bounds("rr-agent-rename-input")
                .expect("rename input");
            assert!(
                input.size.width.as_f32() >= 60.,
                "rename input too narrow at {width}/{rem} {locale}: {input:?}"
            );
            assert!(
                input.top() >= rename_row.top() && input.bottom() <= rename_row.bottom(),
                "rename input clipped at {width}/{rem} {locale}: {input:?} {rename_row:?}"
            );
            let save = button_bounds(visual, &format!("agent-session-save-{}", id.as_str()));
            let cancel = button_bounds(visual, &format!("agent-session-cancel-{}", id.as_str()));
            // rename 态第一行的输入框和保存/取消也必须整体位于第二行之上，
            // 不能因长状态或窄栏把第一行内容压进身份行。
            let rename_first_row_bottom = input.bottom().max(save.bottom()).max(cancel.bottom());
            assert!(
                rename_first_row_bottom <= rename_metadata.top() + px(0.5),
                "rename first row overlaps the identity row at {width}/{rem} {locale}: \
                 {rename_first_row_bottom} vs {rename_metadata:?}"
            );
            assert!(
                input.right() <= save.left() - px(1.),
                "rename input overlaps save action at {width}/{rem} {locale}: {input:?} {save:?}"
            );
            // 普通态改名/删除与 rename 态保存/取消必须落在同一右侧槽位，切换不横向跳动。
            assert!(
                (save.left() - normal_rename.left()).abs() <= px(0.5)
                    && (save.size.width - normal_rename.size.width).abs() <= px(0.5),
                "save must reuse the rename slot at {width}/{rem} {locale}: {save:?} {normal_rename:?}"
            );
            assert!(
                (cancel.left() - normal_delete.left()).abs() <= px(0.5)
                    && (cancel.size.width - normal_delete.size.width).abs() <= px(0.5),
                "cancel must reuse the delete slot at {width}/{rem} {locale}: {cancel:?} {normal_delete:?}"
            );
            for button in [
                format!("agent-session-save-{}", id.as_str()),
                format!("agent-session-cancel-{}", id.as_str()),
            ] {
                let bounds = button_bounds(visual, &button);
                assert!(
                    bounds.top() >= rename_row.top() && bounds.bottom() <= rename_row.bottom(),
                    "rename action clipped at {width}/{rem} {locale}: {button} {bounds:?} {rename_row:?}"
                );
            }
            for selector in ["rr-agent-history-metadata", "rr-agent-history-status"] {
                let bounds = visual.debug_bounds(selector).expect(selector);
                assert!(
                    bounds.top() >= rename_row.top() && bounds.bottom() <= rename_row.bottom(),
                    "rename row content clipped at {width}/{rem} {locale}: {selector} {bounds:?} {rename_row:?}"
                );
            }
            click(visual, &format!("agent-session-cancel-{}", id.as_str()));
            draw(visual);
        }
    }
    gpui_kit::component::set_locale(&original_locale);
    // 所有断言通过后才输出完成标记；父用例据此区分“真实执行”和“过滤为空”。
    println!("{}", LOCALE_LAYOUT_CHILD_MARKER);
}

/// 有可恢复历史时，“进入”仍只创建新会话；不恢复已结束记录，也不展开任何临时选择。
///
/// @author kongweiguang
#[gpui_kit::test]
fn entry_with_resumable_history_creates_directly(app: &mut TestAppContext) {
    let (runtime, service, panel, _id, visual) = fixture(app);
    visual.update(|_, app| {
        panel.update(app, |panel, cx| {
            panel.sessions[0].terminal.as_mut().unwrap().lifecycle = SessionLifecycle::Exited;
            panel.sessions[0].resume_capability = AgentResumeCapability::Supported;
            cx.notify();
        });
    });
    click(visual, "agents-launcher-start");
    pump(&runtime, visual);
    visual.update(|_, app| {
        let panel = panel.read(app);
        assert_eq!(service.unsupported_operation_count("create"), 1);
        assert_eq!(service.unsupported_operation_count("resume"), 0);
        assert!(panel.selected_agent.is_none());
    });
}

/// 只有 Pending/Unknown 恢复事务时，“进入”仍只创建新会话；未决事务保留在历史中核对，
/// 不自动恢复、不重放、不绑定。
///
/// @author kongweiguang
#[gpui_kit::test]
fn entry_with_pending_recovery_still_creates_without_replay(app: &mut TestAppContext) {
    let (runtime, service, panel, _id, visual) = fixture(app);
    visual.update(|_, app| {
        panel.update(app, |panel, cx| {
            panel.sessions[0].terminal.as_mut().unwrap().lifecycle = SessionLifecycle::Exited;
            cx.notify();
        });
    });
    for status in [
        AgentResumeRecoveryStatus::Pending,
        AgentResumeRecoveryStatus::Unknown,
    ] {
        visual.update(|_, app| {
            panel.update(app, |panel, cx| {
                panel.sessions[0].resume_recovery = Some(AgentResumeRecovery {
                    terminal_session_id: SessionId::new(),
                    status,
                });
                cx.notify();
            });
        });
        let creates_before = service.unsupported_operation_count("create");
        click(visual, "agents-launcher-start");
        pump(&runtime, visual);
        visual.update(|_, app| {
            let panel = panel.read(app);
            assert!(panel.selected_agent.is_none());
            assert_eq!(
                service.unsupported_operation_count("create"),
                creates_before + 1
            );
            assert_eq!(service.unsupported_operation_count("resume"), 0);
            assert_eq!(
                service.unsupported_operation_count("bind_preferred_target"),
                0
            );
            assert!(
                panel.sessions[0].resume_recovery.is_some(),
                "the pending recovery must remain available for history reconciliation"
            );
        });
    }
}

/// 同 launcher/scope 同时存在 Running 与 Pending 时，“进入”只 create，
/// 不 resume/replay，也不因未决事务阻断新建。
///
/// @author kongweiguang
#[gpui_kit::test]
fn entry_with_running_and_pending_only_creates(app: &mut TestAppContext) {
    let (runtime, service, panel, _id, visual) = fixture(app);
    visual.update(|_, app| {
        panel.update(app, |panel, cx| {
            let mut pending = panel.sessions[0].clone();
            pending.session.agent_session_id = AgentSessionId::new();
            pending.session.title = "Pending".to_owned();
            pending.terminal = None;
            pending.resume_recovery = Some(AgentResumeRecovery {
                terminal_session_id: SessionId::new(),
                status: AgentResumeRecoveryStatus::Pending,
            });
            panel.sessions.push(pending);
            cx.notify();
        });
    });

    click(visual, "agents-launcher-start");
    pump(&runtime, visual);
    visual.update(|_, app| {
        let panel = panel.read(app);
        assert!(panel.selected_agent.is_none());
        assert_eq!(service.unsupported_operation_count("create"), 1);
        assert_eq!(service.unsupported_operation_count("resume"), 0);
        assert_eq!(
            service.unsupported_operation_count("bind_preferred_target"),
            0
        );
    });
}

/// 生命周期成功选择（如历史恢复成功）在基线未变时应用所选会话。
///
/// @author kongweiguang
#[gpui_kit::test]
fn lifecycle_success_selection_applies_selected_agent(app: &mut TestAppContext) {
    let (_, _service, panel, id, visual) = fixture(app);
    visual.update(|_, app| {
        panel.update(app, |panel, cx| {
            let view = panel.sessions[0].clone();
            panel.apply_lifecycle_success_selection(&id, &None, &view, true);
            cx.notify();
        });
    });
    draw(visual);
    visual.update(|_, app| assert_eq!(panel.read(app).selected_agent.as_ref(), Some(&id)));
}

/// 保活标签重新挂载只刷新主题、色盘、候选和摘要投影；不重读历史，也不重置筛选、改名草稿或当前会话选择。
///
/// @author kongweiguang
#[gpui_kit::test]
fn kept_alive_tab_remount_updates_only_projection(app: &mut TestAppContext) {
    let (runtime, service, panel, id, visual) = fixture(app);

    click(visual, "agents-history-all");
    pump(&runtime, visual);
    let rename = format!("agent-session-rename-{}", id.as_str());
    click(visual, &rename);

    // 选中会话会切换到终端视图，因此先打开改名草稿再恢复选择，才能同时观察两类非默认状态。
    visual.update(|_, app| {
        panel.update(app, |panel, cx| panel.select_agent(id.clone(), cx));
    });
    draw(visual);

    let (loads_before, rename_before) = visual.update(|_, app| {
        let panel = panel.read(app);
        assert_eq!(panel.history_mode, AgentHistoryMode::All);
        assert_eq!(panel.selected_agent_session_id(), Some(id.clone()));
        let rename_before = panel.rename_target.clone();
        assert!(rename_before.is_some(), "rename editor must be open");
        (service.history_loads(), rename_before)
    });

    let candidate = switched_target_candidate();
    visual.update(|_, app| {
        panel.update(app, |panel, cx| {
            panel.set_theme_mode(ThemeMode::Dark, cx);
            panel.set_terminal_palette(TerminalPalette::default(), cx);
            panel.set_target_candidate(Some(candidate.clone()), cx);
            panel.set_target_summary(Some("Another terminal".to_owned()), cx);
        });
    });
    pump(&runtime, visual);

    visual.update(|_, app| {
        let panel = panel.read(app);
        assert_eq!(
            service.history_loads(),
            loads_before,
            "re-mounting a retained tab must not reload history"
        );
        assert_eq!(panel.history_mode, AgentHistoryMode::All);
        assert_eq!(panel.rename_target, rename_before);
        assert_eq!(panel.selected_agent_session_id(), Some(id.clone()));
        assert_eq!(panel.theme_mode, ThemeMode::Dark);
        assert_eq!(panel.terminal_palette, Some(TerminalPalette::default()));
        assert_eq!(panel.candidate.as_ref(), Some(&candidate));
        assert_eq!(panel.target_summary.as_deref(), Some("Another terminal"));
    });
}
