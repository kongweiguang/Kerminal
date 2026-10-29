//! 工作台布局与工具投影测试；不创建 GPUI Entity 或真实终端进程。
//!
//! @author kongweiguang

use std::collections::BTreeMap;

use gpui_kit::px;
use kerminal_application::preferences::{
    InterfaceDensity, SettingsSnapshot, ToolRailPanelPlacement, ToolRailSettings, ToolRailToolId,
    WorkbenchLayoutSettings,
};
use kerminal_application::workspace::{WorkspaceSessionSnapshot, WorkspaceSessionTab};
use kerminal_domain::{
    identity::{PaneId, TabId},
    workspace::{SplitDirection, TerminalLayoutNode},
};

use crate::root::workbench::{
    ToolRailPanelSlots, ToolRailPanelState, attached_dock_default_width,
    compact_tool_drawer_should_be_visible, focused_workspace_pane, sidebar_expansion_needs_drawer,
    tool_rail_button_height, tool_rail_label, tool_rail_partition_min_height,
};

/// 展开主机栏导致空间不足时必须立即给出目录抽屉，显式收起或宽窗不会补开。
///
/// @author kongweiguang
#[test]
fn sidebar_expansion_in_compact_budget_opens_a_reachable_drawer() {
    assert!(sidebar_expansion_needs_drawer(false, true));
    assert!(!sidebar_expansion_needs_drawer(true, true));
    assert!(!sidebar_expansion_needs_drawer(false, false));
    assert!(!sidebar_expansion_needs_drawer(true, false));
}

/// 窄窗工具抽屉使用已记录宽度并夹紧到 440px 上限与剩余区域，不改写用户偏好。
///
/// @author kongweiguang
#[test]
fn compact_drawer_keeps_side_width_and_fits_the_actual_viewport() {
    use crate::root::workbench_drawers::tool_drawer_width;
    assert_eq!(tool_drawer_width(832., 52., 44., 324.), 324.);
    assert_eq!(tool_drawer_width(300., 52., 44., 324.), 204.);
    assert_eq!(tool_drawer_width(40., 52., 44., 324.), 0.);
    assert_eq!(tool_drawer_width(1200., 52., 44., 1000.), 440.);
    assert_eq!(tool_drawer_width(832., 52., 44., f32::NAN), 364.);
    assert_eq!(tool_drawer_width(832., 52., 44., 40.), 134.);
    assert_eq!(tool_drawer_width(2000., 52., 44., 2000.), 440.);
}

/// 文件与容器保留旧配置兼容，同时允许用户将其放到 main 支持的任一停靠位置。
///
/// @author kongweiguang
#[test]
fn file_and_container_tools_follow_the_configured_panel_slot_without_rewriting_settings() {
    for tool in [ToolRailToolId::Sftp, ToolRailToolId::Containers] {
        for placement in [
            ToolRailPanelPlacement::Center,
            ToolRailPanelPlacement::Left,
            ToolRailPanelPlacement::Bottom,
            ToolRailPanelPlacement::Attached,
        ] {
            let mut settings = ToolRailSettings::default();
            settings.panel_placements.insert(tool, placement);
            let original = settings.clone();
            let mut state = ToolRailPanelState::default();
            state.toggle(&settings, tool);
            let panels = state.reproject(&settings);
            assert_eq!(
                panels.attached,
                (placement == ToolRailPanelPlacement::Attached).then_some(tool)
            );
            assert_eq!(
                panels.left,
                (placement == ToolRailPanelPlacement::Left).then_some(tool)
            );
            assert_eq!(
                panels.bottom,
                (placement == ToolRailPanelPlacement::Bottom).then_some(tool)
            );
            assert_eq!(
                panels.center,
                (placement == ToolRailPanelPlacement::Center).then_some(tool)
            );
            assert_eq!(settings, original);
        }
    }
}

/// 建立两个叶子的递归布局，验证工具栏定位依赖 focus 而非 pane 列表顺序。
fn split_layout(first: PaneId, second: PaneId) -> TerminalLayoutNode {
    TerminalLayoutNode::Split {
        id: "workbench-test-split".to_owned(),
        direction: SplitDirection::Horizontal,
        children: vec![
            TerminalLayoutNode::Pane { pane_id: first },
            TerminalLayoutNode::Pane { pane_id: second },
        ],
        sizes: None,
    }
}

/// 创建终端 tab 与测试中的 typed 工作区快照，避免靠 JSON 再实现 codec 逻辑。
fn terminal_tab(id: TabId, layout: TerminalLayoutNode) -> WorkspaceSessionTab {
    WorkspaceSessionTab::Terminal {
        id,
        tab_group_id: None,
        title: "Terminal".to_owned(),
        machine_id: "local".to_owned(),
        layout,
    }
}

/// 仅构造焦点解析所需的最小快照；其余应用校验由 WorkspaceSessionDocument 覆盖。
/// @author kongweiguang
fn snapshot(
    active_tab_id: Option<TabId>,
    focused_pane_id: Option<PaneId>,
    terminal_tabs: Vec<WorkspaceSessionTab>,
) -> WorkspaceSessionSnapshot {
    WorkspaceSessionSnapshot {
        pinned_containers: Vec::new(),
        active_tab_id,
        focused_pane_id,
        selected_machine_id: None,
        terminal_tabs,
        terminal_panes: Vec::new(),
        tab_groups: BTreeMap::new(),
    }
}

/// 文件工具采用协议中性的名称，但内部 SFTP ToolId 保持原样以兼容保存布局。
///
/// @author kongweiguang
#[test]
fn file_tool_rail_label_is_transport_neutral() {
    let label = tool_rail_label(ToolRailToolId::Sftp).to_string();
    assert!(matches!(label.as_str(), "文件" | "Files"));
}

/// 容器入口使用独立可访问身份、专属图标和与 Files 相同的 360px 默认内容宽度。
///
/// @author kongweiguang
#[test]
fn container_tool_rail_uses_container_identity_and_default_width() {
    assert_eq!(
        super::tool_rail_button_id(ToolRailToolId::Containers),
        "workbench-tool-containers"
    );
    assert!(matches!(
        super::tool_rail_icon(ToolRailToolId::Containers),
        gpui_kit::assets::IconName::Container
    ));
    let label = tool_rail_label(ToolRailToolId::Containers).to_string();
    assert!(matches!(label.as_str(), "容器" | "Containers"));
    assert_eq!(attached_dock_default_width(1500.0), px(320.));
}

/// Dock 默认外框宽度复用 main 的 24% 总宽规则，并单独保留 rail 和 gutter 的单位。
///
/// @author kongweiguang
#[test]
fn tool_panel_default_width_uses_main_viewport_ratio_and_bounds() {
    use crate::root::workbench_layout::{
        initial_tool_panel_width, main_attached_dock_total_to_gpui_content_width,
    };

    assert_eq!(initial_tool_panel_width(1200.0), 340.0);
    assert_eq!(initial_tool_panel_width(1500.0), 360.0);
    assert_eq!(initial_tool_panel_width(2000.0), 444.0);
    assert_eq!(attached_dock_default_width(1200.0), px(300.));
    assert_eq!(attached_dock_default_width(1500.0), px(320.));
    assert_eq!(attached_dock_default_width(2000.0), px(404.));
    assert_eq!(main_attached_dock_total_to_gpui_content_width(300.0), 256.0);
    assert_eq!(main_attached_dock_total_to_gpui_content_width(360.0), 316.0);
}

/// 活动终端 split 的显式焦点 pane 应独占底部工具，不随渲染顺序切回第一 pane。
#[test]
fn focused_pane_selects_matching_leaf_in_active_terminal_tab() {
    let tab_id = TabId::parse("tab-active").expect("valid tab ID");
    let first = PaneId::parse("pane-first").expect("valid pane ID");
    let focused = PaneId::parse("pane-focused").expect("valid pane ID");
    let state = snapshot(
        Some(tab_id.clone()),
        Some(focused.clone()),
        vec![terminal_tab(tab_id, split_layout(first, focused.clone()))],
    );

    assert_eq!(focused_workspace_pane(&state), Some(focused));
}

/// 焦点若属于非活动 tab，底部工具栏不得误绑另一个可见或历史 session。
#[test]
fn focus_from_another_tab_is_not_reused() {
    let active_tab = TabId::parse("tab-active").expect("valid tab ID");
    let inactive_tab = TabId::parse("tab-inactive").expect("valid tab ID");
    let active_pane = PaneId::parse("pane-active").expect("valid pane ID");
    let inactive_pane = PaneId::parse("pane-inactive").expect("valid pane ID");
    let state = snapshot(
        Some(active_tab.clone()),
        Some(inactive_pane.clone()),
        vec![
            terminal_tab(active_tab, TerminalLayoutNode::pane(active_pane)),
            terminal_tab(inactive_tab, TerminalLayoutNode::pane(inactive_pane)),
        ],
    );

    assert_eq!(focused_workspace_pane(&state), None);
}

/// 文件/SFTP 活动标签没有 pane focus，不能让旧终端工具栏留在底部。
#[test]
fn nonterminal_active_tab_has_no_terminal_tools_focus() {
    let active_tab = TabId::parse("tab-files").expect("valid tab ID");
    let stale_pane = PaneId::parse("pane-stale").expect("valid pane ID");
    let state = snapshot(
        Some(active_tab.clone()),
        Some(stale_pane.clone()),
        vec![WorkspaceSessionTab::SftpTransfer {
            id: active_tab,
            tab_group_id: None,
            title: "Files".to_owned(),
            machine_id: "local".to_owned(),
            left_host_id: None,
            locked_left_host_id: None,
            right_host_id: None,
            external_launch_id: None,
            initial_left_path: None,
            initial_right_path: None,
            initial_right_selection: None,
        }],
    );

    assert_eq!(focused_workspace_pane(&state), None);
}

/// 没有明确 focusedPaneId 时不猜一个默认 pane，避免把工具动作落到错误终端。
#[test]
fn missing_focus_does_not_fall_back_to_first_pane() {
    let tab_id = TabId::parse("tab-active").expect("valid tab ID");
    let pane_id = PaneId::parse("pane-first").expect("valid pane ID");
    let state = snapshot(
        Some(tab_id.clone()),
        None,
        vec![terminal_tab(tab_id, TerminalLayoutNode::pane(pane_id))],
    );

    assert_eq!(focused_workspace_pane(&state), None);
}

/// 工具入口顺序、隐藏/贴底与四向宿主分别生效；文件位置显式配置，不依赖旧版强制右侧行为。
///
/// @author kongweiguang
#[test]
fn tool_rail_settings_project_entries_and_distinct_panel_hosts() {
    let mut settings = ToolRailSettings {
        order: vec![
            ToolRailToolId::Sftp,
            ToolRailToolId::System,
            ToolRailToolId::Logs,
            ToolRailToolId::Tmux,
            ToolRailToolId::Snippets,
            ToolRailToolId::Ports,
            ToolRailToolId::AgentLauncher,
        ],
        hidden: vec![ToolRailToolId::Snippets],
        bottom: vec![ToolRailToolId::Logs, ToolRailToolId::Tmux],
        ..ToolRailSettings::default()
    };
    settings
        .panel_placements
        .insert(ToolRailToolId::Sftp, ToolRailPanelPlacement::Attached);
    settings
        .panel_placements
        .insert(ToolRailToolId::System, ToolRailPanelPlacement::Left);
    settings
        .panel_placements
        .insert(ToolRailToolId::Logs, ToolRailPanelPlacement::Center);
    settings
        .panel_placements
        .insert(ToolRailToolId::Tmux, ToolRailPanelPlacement::Bottom);
    settings
        .panel_placements
        .insert(ToolRailToolId::Ports, ToolRailPanelPlacement::Left);

    let mut state = ToolRailPanelState::default();
    state.toggle(&settings, ToolRailToolId::Sftp);
    state.toggle(&settings, ToolRailToolId::System);
    state.toggle(&settings, ToolRailToolId::Logs);
    state.toggle(&settings, ToolRailToolId::Tmux);
    let layout = state.project(&settings);

    assert_eq!(
        layout.entries.main,
        [
            ToolRailToolId::Sftp,
            ToolRailToolId::System,
            ToolRailToolId::Ports,
            ToolRailToolId::AgentLauncher,
        ]
    );
    assert_eq!(
        layout.entries.bottom,
        [ToolRailToolId::Logs, ToolRailToolId::Tmux]
    );
    assert_eq!(
        layout.panels,
        ToolRailPanelSlots {
            attached: Some(ToolRailToolId::Sftp),
            left: Some(ToolRailToolId::System),
            bottom: Some(ToolRailToolId::Tmux),
            center: Some(ToolRailToolId::Logs),
        }
    );
    assert!(!layout.entries.main.contains(&ToolRailToolId::Snippets));
}

/// 文件参与正常位置投影，设置移动后同方向只保留最近打开项，不把被替换项藏在右侧。
///
/// @author kongweiguang
#[test]
fn tool_rail_reprojects_open_panels_after_settings_move() {
    let mut settings = ToolRailSettings::default();
    settings
        .panel_placements
        .insert(ToolRailToolId::Sftp, ToolRailPanelPlacement::Left);
    settings
        .panel_placements
        .insert(ToolRailToolId::Snippets, ToolRailPanelPlacement::Center);
    let mut state = ToolRailPanelState::default();
    state.toggle(&settings, ToolRailToolId::Sftp);
    state.toggle(&settings, ToolRailToolId::Snippets);

    assert_eq!(
        state.project(&settings).panels.left,
        Some(ToolRailToolId::Sftp)
    );
    // 两个已打开工具移到同方向时保留较新者；不会凭空生成第二个宿主。
    settings
        .panel_placements
        .insert(ToolRailToolId::Sftp, ToolRailPanelPlacement::Left);
    settings
        .panel_placements
        .insert(ToolRailToolId::Snippets, ToolRailPanelPlacement::Left);
    let slots = state.reproject(&settings);

    assert_eq!(slots.left, Some(ToolRailToolId::Snippets));
    assert_eq!(slots.attached, None);
    assert_eq!(slots.center, None);
    assert_eq!(state.open_order, [ToolRailToolId::Snippets]);
}

/// 文件与其它工具遵循同方向替换；关闭当前工具不复活已被替换的面板，不影响其它方向。
///
/// @author kongweiguang
#[test]
fn tool_rail_toggle_preserves_other_zones_and_closes_current() {
    let mut settings = ToolRailSettings::default();
    settings
        .panel_placements
        .insert(ToolRailToolId::Sftp, ToolRailPanelPlacement::Left);
    settings
        .panel_placements
        .insert(ToolRailToolId::System, ToolRailPanelPlacement::Left);
    settings
        .panel_placements
        .insert(ToolRailToolId::Logs, ToolRailPanelPlacement::Center);
    let mut state = ToolRailPanelState::default();
    state.toggle(&settings, ToolRailToolId::Logs);
    state.toggle(&settings, ToolRailToolId::Sftp);
    state.toggle(&settings, ToolRailToolId::System);

    let slots = state.reproject(&settings);
    assert_eq!(slots.center, Some(ToolRailToolId::Logs));
    assert_eq!(slots.left, Some(ToolRailToolId::System));
    assert_eq!(slots.attached, None);
    assert!(!state.open_order.contains(&ToolRailToolId::Sftp));

    state.toggle(&settings, ToolRailToolId::System);
    assert_eq!(state.reproject(&settings).left, None);
    assert_eq!(
        state.reproject(&settings).center,
        Some(ToolRailToolId::Logs)
    );
}

/// 用户隐藏面板入口后，已打开工具仍可见于入口列表并能通过同一动作关闭。
///
/// @author kongweiguang
#[test]
fn hidden_open_tool_remains_projected_until_closed() {
    let mut settings = ToolRailSettings::default();
    settings.hidden.push(ToolRailToolId::Sftp);
    let mut state = ToolRailPanelState::default();
    state.toggle(&settings, ToolRailToolId::Sftp);

    let layout = state.project(&settings);
    assert!(layout.entries.main.contains(&ToolRailToolId::Sftp));
    assert_eq!(layout.panels.attached, Some(ToolRailToolId::Sftp));

    state.toggle(&settings, ToolRailToolId::Sftp);
    assert!(
        !state
            .project(&settings)
            .entries
            .main
            .contains(&ToolRailToolId::Sftp)
    );
}

/// 首次进入工作区让终端独占中央空间；Agent 仍保留在 Rail 中并可由用户主动打开。
///
/// @author kongweiguang
#[test]
fn default_tool_rail_starts_terminal_first_without_attached_panel() {
    let settings = ToolRailSettings::default();
    let layout = ToolRailPanelState::default().project(&settings);

    assert_eq!(layout.panels.attached, None);
    assert!(layout.entries.main.contains(&ToolRailToolId::AgentLauncher));
    assert!(!layout.entries.main.contains(&ToolRailToolId::Context));
}

/// 旧配置中的 Context 工具身份仍可解析，但不进入 UI 投影或打开面板。
///
/// @author kongweiguang
#[test]
fn context_tool_is_ignored_in_legacy_rail_settings() {
    let mut settings = ToolRailSettings::default();
    let mut state = ToolRailPanelState::default();

    state.toggle(&settings, ToolRailToolId::Context);
    let layout = state.project(&settings);

    assert!(!layout.entries.main.contains(&ToolRailToolId::Context));
    assert!(!layout.entries.bottom.contains(&ToolRailToolId::Context));
    assert!(
        ![
            layout.panels.attached,
            layout.panels.left,
            layout.panels.bottom,
            layout.panels.center,
        ]
        .contains(&Some(ToolRailToolId::Context))
    );

    settings.order.insert(0, ToolRailToolId::Context);
    settings.bottom.push(ToolRailToolId::Context);
    settings
        .panel_placements
        .insert(ToolRailToolId::Context, ToolRailPanelPlacement::Center);
    let layout = state.project(&settings);
    assert!(!layout.entries.main.contains(&ToolRailToolId::Context));
    assert!(!layout.entries.bottom.contains(&ToolRailToolId::Context));
    assert_eq!(layout.panels.center, None);
}

/// 旧配置若只留下 Context 可见，投影仍给出第一个真实工具以维持产品入口。
///
/// @author kongweiguang
#[test]
fn legacy_context_visibility_keeps_one_real_tool_discoverable() {
    let mut settings = ToolRailSettings::default();
    settings
        .order
        .retain(|tool| *tool != ToolRailToolId::AgentLauncher);
    settings.hidden = settings.order.clone();
    let hidden_before_projection = settings.hidden.clone();
    let layout = ToolRailPanelState::default().project(&settings);

    assert_eq!(layout.entries.main, [ToolRailToolId::AgentLauncher]);
    assert!(layout.entries.bottom.is_empty());
    assert_eq!(settings.hidden, hidden_before_projection);
}

/// 所有右侧工具共享一个默认宽度，Agent 运行态变化也不再改变工作区几何。
///
/// @author kongweiguang
#[test]
fn attached_tool_defaults_keep_sftp_and_agent_width_rules() {
    assert_eq!(attached_dock_default_width(1500.0), px(320.));
    assert_eq!(attached_dock_default_width(1500.0), px(320.));
    assert_eq!(attached_dock_default_width(1500.0), px(320.));
    assert_eq!(attached_dock_default_width(1500.0), px(320.));
}

/// 非空 Rail 底部分区保留一个可命中的图标高度，空分区不制造空白占位。
///
/// @author kongweiguang
#[test]
fn rail_bottom_partition_minimum_preserves_one_click_target() {
    for (density, expected_height) in [
        (InterfaceDensity::Compact, 28.0),
        (InterfaceDensity::Comfortable, 32.0),
        (InterfaceDensity::Spacious, 36.0),
    ] {
        let button_height = tool_rail_button_height(density);
        assert_eq!(button_height, expected_height);
        assert_eq!(tool_rail_partition_min_height(&[], button_height), px(0.));
        assert_eq!(
            tool_rail_partition_min_height(&[ToolRailToolId::Logs], button_height),
            px(expected_height)
        );
    }
}

/// 自定义宽度在所有工具间共享；切换宿主不改变尺寸，拖回默认值会清除共享覆盖。
///
/// @author kongweiguang
#[test]
fn attached_width_memory_is_shared_across_tools_and_default_resets_it() {
    let mut state = ToolRailPanelState::default();
    let sftp_default = attached_dock_default_width(1500.0);
    let containers_default = attached_dock_default_width(1500.0);
    let agent_default = attached_dock_default_width(1500.0);

    state.remember_attached_width(ToolRailToolId::Sftp, sftp_default, sftp_default);
    state.remember_attached_width(ToolRailToolId::Containers, px(604.), containers_default);
    state.remember_attached_width(ToolRailToolId::AgentLauncher, px(392.), agent_default);
    state.remember_attached_width(ToolRailToolId::Sftp, px(548.), sftp_default);

    let mut settings = ToolRailSettings::default();
    settings.order.reverse();
    state.project(&settings);
    for tool in [
        ToolRailToolId::Sftp,
        ToolRailToolId::Containers,
        ToolRailToolId::AgentLauncher,
    ] {
        assert_eq!(state.attached_width(tool), Some(px(548.)));
    }

    state.remember_attached_width(ToolRailToolId::Sftp, sftp_default, sftp_default);
    assert_eq!(state.attached_width(ToolRailToolId::Sftp), None);
    assert_eq!(state.attached_width(ToolRailToolId::Containers), None);
    assert_eq!(state.attached_width(ToolRailToolId::AgentLauncher), None);
}

/// 每个 workspace tab 独立记住打开工具；切换时恢复原顺序并清理已关闭标签缓存。
///
/// @author kongweiguang
#[test]
fn tool_open_order_is_cached_per_tab_and_pruned_when_tabs_close() {
    let first_tab = TabId::parse("tab-first").expect("valid tab ID");
    let second_tab = TabId::parse("tab-second").expect("valid tab ID");
    let live_tabs = [first_tab.clone(), second_tab.clone()];
    let mut settings = ToolRailSettings::default();
    settings
        .panel_placements
        .insert(ToolRailToolId::Logs, ToolRailPanelPlacement::Bottom);
    let mut state = ToolRailPanelState::default();

    assert_eq!(
        state.activate_tab(Some(first_tab.clone()), &live_tabs, false),
        Some(false)
    );
    state.toggle(&settings, ToolRailToolId::Sftp);
    state.toggle(&settings, ToolRailToolId::Logs);
    let first_panels = state.project(&settings).panels;
    assert_eq!(first_panels.attached, Some(ToolRailToolId::Sftp));
    assert_eq!(first_panels.bottom, Some(ToolRailToolId::Logs));

    assert_eq!(
        state.activate_tab(Some(second_tab.clone()), &live_tabs, false),
        Some(false)
    );
    assert_eq!(
        state.project(&settings).panels,
        ToolRailPanelSlots {
            attached: Some(ToolRailToolId::Sftp),
            bottom: Some(ToolRailToolId::Logs),
            ..ToolRailPanelSlots::default()
        }
    );
    state.toggle(&settings, ToolRailToolId::Containers);
    assert_eq!(
        state.project(&settings).panels.attached,
        Some(ToolRailToolId::Containers)
    );

    assert_eq!(
        state.activate_tab(Some(first_tab.clone()), &live_tabs, true),
        Some(false)
    );
    let first_panels = state.project(&settings).panels;
    assert_eq!(first_panels.attached, Some(ToolRailToolId::Sftp));
    assert_eq!(first_panels.bottom, Some(ToolRailToolId::Logs));
    assert_eq!(
        state.activate_tab(Some(second_tab.clone()), &live_tabs, false),
        Some(true)
    );
    assert_eq!(
        state.project(&settings).panels.attached,
        Some(ToolRailToolId::Containers)
    );

    assert_eq!(
        state.activate_tab(Some(first_tab.clone()), &[first_tab], true),
        Some(false)
    );
    assert!(
        !state
            .tab_open_orders
            .contains_key(&Some(second_tab.clone()))
    );
    assert!(
        !state
            .tab_right_dock_collapsed
            .contains_key(&Some(second_tab))
    );
}

/// 窄屏抽屉跟随当前 Tab 的活动工具；宽转窄、Tab 恢复和显式关闭都由同一打开集合投影决定。
///
/// @author kongweiguang
#[test]
fn compact_tool_drawer_tracks_viewport_tab_and_explicit_close_state() {
    use crate::root::workbench_layout::{WorkbenchLayoutInput, resolve_workbench_layout};

    let first_tab = TabId::parse("drawer-first").expect("valid tab ID");
    let second_tab = TabId::parse("drawer-second").expect("valid tab ID");
    let live_tabs = [first_tab.clone(), second_tab.clone()];
    let mut settings = ToolRailSettings::default();
    settings
        .panel_placements
        .insert(ToolRailToolId::Sftp, ToolRailPanelPlacement::Attached);
    settings
        .panel_placements
        .insert(ToolRailToolId::Logs, ToolRailPanelPlacement::Left);
    let mut state = ToolRailPanelState::default();
    assert_eq!(
        state.activate_tab(Some(first_tab.clone()), &live_tabs, false),
        Some(false)
    );
    state.toggle(&settings, ToolRailToolId::Sftp);
    state.toggle(&settings, ToolRailToolId::Logs);

    let compact_target = |state: &mut ToolRailPanelState, width: f32| {
        let panels = state.project(&settings).panels;
        let plan = resolve_workbench_layout(&WorkbenchLayoutInput {
            viewport_width: width,
            viewport_height: 900.0,
            host_sidebar_collapsed: false,
            right_dock_collapsed: false,
            host_sidebar_preferred_width: 220.0,
            attached_dock_preferred_width: 420.0,
            left_tool_panel_preferred_width: 320.0,
            left_tool_open: panels.left.is_some(),
            attached_tool_open: panels.attached.is_some(),
            bottom_tool_open: panels.bottom.is_some(),
            focused_tools_height: 0.0,
            focused_tools_expanded_height: 0.0,
            status_bar_height: 26.0,
            reserved_vertical_height: 0.0,
            open_tool_order: &state.open_order,
        });
        let active = plan.compact_active_tool;
        compact_tool_drawer_should_be_visible(plan.compact_panels, active, false)
            .then_some(active)
            .flatten()
    };

    assert_eq!(compact_target(&mut state, 1_800.0), None);
    assert_eq!(
        compact_target(&mut state, 832.0),
        Some(ToolRailToolId::Logs)
    );
    assert!(!compact_tool_drawer_should_be_visible(
        true,
        Some(ToolRailToolId::Logs),
        true,
    ));

    assert_eq!(
        state.activate_tab(Some(second_tab.clone()), &live_tabs, false),
        Some(false)
    );
    state.toggle(&settings, ToolRailToolId::Logs);
    assert_eq!(
        compact_target(&mut state, 832.0),
        Some(ToolRailToolId::Sftp)
    );
    assert_eq!(
        state.activate_tab(Some(first_tab.clone()), &live_tabs, false),
        Some(false)
    );
    assert_eq!(
        compact_target(&mut state, 832.0),
        Some(ToolRailToolId::Logs)
    );

    assert!(state.close(&settings, ToolRailToolId::Logs));
    assert_eq!(
        compact_target(&mut state, 832.0),
        Some(ToolRailToolId::Sftp)
    );
    assert!(state.close(&settings, ToolRailToolId::Sftp));
    assert_eq!(compact_target(&mut state, 832.0), None);
}

/// Main 的浮窗以 Shell 内居中为首态，拖动超出可视区域时保留标题栏和面板边缘。
///
/// @author kongweiguang
#[test]
fn floating_panel_starts_centered_and_drag_stays_inside_the_workbench() {
    use crate::root::workbench_layout::{
        FloatingPanelPosition, floating_panel_geometry, initial_floating_panel_position,
    };

    let geometry = floating_panel_geometry(1440.0, 864.0);
    assert_eq!(geometry.width, 760.0);
    assert_eq!(geometry.height, 720.0);
    let initial = initial_floating_panel_position(1440.0, 864.0, geometry);
    assert_eq!(initial, FloatingPanelPosition { x: 340, y: 76 });

    let tool = ToolRailToolId::Snippets;
    let mut keyboard_state = ToolRailPanelState::default();
    assert!(keyboard_state.nudge_floating_position(
        tool,
        initial,
        (12, -12),
        1440.0,
        864.0,
        geometry
    ));
    assert_eq!(
        keyboard_state.floating_position(tool),
        Some(FloatingPanelPosition { x: 352, y: 64 })
    );
    assert!(keyboard_state.nudge_floating_position(
        tool,
        initial,
        (40, 40),
        1440.0,
        864.0,
        geometry
    ));
    assert_eq!(
        keyboard_state.floating_position(tool),
        Some(FloatingPanelPosition { x: 392, y: 104 })
    );
    keyboard_state.set_floating_position(tool, FloatingPanelPosition { x: 672, y: 136 });
    assert!(!keyboard_state.nudge_floating_position(
        tool,
        initial,
        (12, 12),
        1440.0,
        864.0,
        geometry
    ));

    let mut state = ToolRailPanelState::default();
    state.set_floating_position(tool, initial);
    state.begin_floating_drag(tool, 400.0, 100.0, initial);
    assert!(state.update_floating_drag(3_000.0, 900.0, 1440.0, 864.0, geometry));
    assert_eq!(
        state.floating_position(tool),
        Some(FloatingPanelPosition { x: 672, y: 136 })
    );
    assert!(state.finish_floating_drag());
    assert!(!state.floating_drag_is_active());

    state.begin_floating_drag(tool, 400.0, 100.0, initial);
    assert!(state.update_floating_drag(460.0, 160.0, 1440.0, 864.0, geometry));
    assert!(state.cancel_floating_drag());
    assert_eq!(state.floating_position(tool), Some(initial));
    assert!(!state.floating_drag_is_active());

    let mut settings = ToolRailSettings::default();
    settings
        .panel_placements
        .insert(tool, ToolRailPanelPlacement::Center);
    state.toggle(&settings, tool);
    state.begin_floating_drag(tool, 400.0, 100.0, initial);
    assert!(state.close(&settings, tool));
    assert!(!state.floating_drag_is_active());
}

/// 只有当前活动宿主的显式拖动能写入宽度记忆，旧工具的迟到回调不会污染另一个工具。
///
/// @author kongweiguang
#[test]
fn attached_width_memory_accepts_only_current_user_resize() {
    let mut state = ToolRailPanelState::default();
    let agent_width = px(392.);
    state.set_attached_tool(
        Some(ToolRailToolId::AgentLauncher),
        Some(attached_dock_default_width(1500.0)),
    );

    assert!(!state.remember_user_resize(ToolRailToolId::Sftp, px(548.)));
    assert_eq!(state.attached_width(ToolRailToolId::Sftp), None);

    assert!(state.remember_user_resize(ToolRailToolId::AgentLauncher, agent_width));
    assert_eq!(
        state.attached_width(ToolRailToolId::AgentLauncher),
        Some(agent_width)
    );

    state.set_attached_tool(
        Some(ToolRailToolId::Sftp),
        Some(attached_dock_default_width(1500.0)),
    );
    assert!(!state.remember_user_resize(ToolRailToolId::AgentLauncher, px(420.)));
    assert_eq!(
        state.attached_width(ToolRailToolId::AgentLauncher),
        Some(agent_width)
    );
    assert_eq!(
        state.attached_width(ToolRailToolId::Sftp),
        Some(agent_width)
    );
}

/// Agent 生命周期变化不再改变共享右栏尺寸，用户覆盖与默认值仍按同一基准记忆。
///
/// @author kongweiguang
#[test]
fn agent_default_width_updates_without_erasing_custom_widths() {
    let mut state = ToolRailPanelState::default();
    let empty_default = attached_dock_default_width(1500.0);
    let running_default = attached_dock_default_width(1500.0);
    state.set_attached_tool(Some(ToolRailToolId::AgentLauncher), Some(empty_default));

    state.remember_attached_width(ToolRailToolId::AgentLauncher, px(392.), empty_default);
    assert!(!state.update_attached_default_width(ToolRailToolId::AgentLauncher, running_default));
    assert_eq!(state.attached_tool, Some(ToolRailToolId::AgentLauncher));
    assert_eq!(state.attached_default_width, Some(empty_default));
    assert_eq!(
        state.attached_width(ToolRailToolId::AgentLauncher),
        Some(px(392.))
    );

    state.remember_attached_width(
        ToolRailToolId::AgentLauncher,
        running_default,
        running_default,
    );
    assert!(!state.update_attached_default_width(ToolRailToolId::AgentLauncher, running_default));
    assert_eq!(state.attached_width(ToolRailToolId::AgentLauncher), None);
}

/// 工具默认宽度统一；运行态变化不会再触发尺寸回填，明确拖动仍更新共享尺寸。
///
/// @author kongweiguang
#[test]
fn agent_content_width_transition_requests_one_default_restore() {
    let mut state = ToolRailPanelState::default();
    let tool = ToolRailToolId::AgentLauncher;
    let empty = attached_dock_default_width(1500.0);
    let running = attached_dock_default_width(1500.0);
    state.set_attached_tool(Some(tool), Some(empty));

    assert!(!state.update_attached_default_width(tool, running));
    assert!(!state.update_attached_default_width(tool, running));
    assert!(!state.update_attached_default_width(ToolRailToolId::Sftp, empty));
    assert_eq!(state.attached_default_width, Some(running));

    assert!(state.remember_user_resize(tool, px(392.)));
    assert!(!state.update_attached_default_width(tool, empty));
    assert_eq!(state.attached_width(tool), Some(px(392.)));
}

/// 持久尺寸使用逻辑内容 px；两侧去 4px 槽而底部不减，非法测量不能成为保存意图。
///
/// @author kongweiguang
#[test]
fn persisted_layout_dimensions_exclude_only_horizontal_gutters() {
    use crate::root::workbench::workbench_content_dimension;
    assert_eq!(
        workbench_content_dimension(px(224.), 4.0, 168, 360),
        Some(220)
    );
    assert_eq!(
        workbench_content_dimension(px(424.), 4.0, 130, 1_360),
        Some(420)
    );
    assert_eq!(
        workbench_content_dimension(px(134.), 4.0, 130, 1_360),
        Some(130)
    );
    assert_eq!(
        workbench_content_dimension(px(1_364.), 4.0, 130, 1_360),
        Some(1_360)
    );
    assert_eq!(
        workbench_content_dimension(px(240.), 0.0, 180, 4_000),
        Some(240)
    );
    assert_eq!(
        workbench_content_dimension(px(4_000.), 0.0, 180, 4_000),
        Some(4_000)
    );
    for width in [f32::NAN, f32::INFINITY, 171.0, 365.0] {
        assert_eq!(workbench_content_dimension(px(width), 4.0, 168, 360), None);
    }
}

/// 旧 A 收据只推进配置基线，不领取新 B；其它设置字段与末意图保持原样。
///
/// @author kongweiguang
#[test]
fn old_layout_receipt_updates_only_baseline_until_latest_receipt() {
    use crate::root::workbench::{
        PendingWorkbenchLayout, accept_workbench_layout_baseline,
        workbench_layout_receipts_complete,
    };
    let mut settings = Some(SettingsSnapshot::default());
    let original_terminal = settings.as_ref().expect("baseline").terminal.clone();
    let a = WorkbenchLayoutSettings {
        host_sidebar_width: 200,
        ..Default::default()
    };
    let b = WorkbenchLayoutSettings {
        host_sidebar_width: 280,
        ..Default::default()
    };
    let mut pending = Some(PendingWorkbenchLayout {
        request_id: 2,
        layout: b.clone(),
    });
    assert!(!accept_workbench_layout_baseline(
        &mut settings,
        &mut pending,
        1,
        a.clone()
    ));
    assert_eq!(settings.as_ref().expect("confirmed A").workbench_layout, a);
    assert_eq!(pending.as_ref().expect("RAM B").layout, b);
    assert_eq!(pending.as_ref().expect("RAM B").request_id, 2);
    assert!(
        !workbench_layout_receipts_complete(&pending, false),
        "owner 完成也要等 B 收据"
    );
    assert_eq!(
        settings.as_ref().expect("baseline").terminal,
        original_terminal
    );

    assert!(accept_workbench_layout_baseline(
        &mut settings,
        &mut pending,
        2,
        b.clone()
    ));
    assert_eq!(settings.as_ref().expect("confirmed B").workbench_layout, b);
    assert!(pending.is_none());
    assert!(workbench_layout_receipts_complete(&pending, false));
    assert!(!workbench_layout_receipts_complete(&pending, true));
}

/// 启动及外部读回可恢复布局，但任何未确认本地手势都优先于旧的完整 settings 回执。
///
/// @author kongweiguang
#[test]
fn settings_layout_restore_never_overwrites_pending_ram_intent() {
    use crate::root::workbench::should_restore_workbench_layout;
    let a = WorkbenchLayoutSettings::default();
    let b = WorkbenchLayoutSettings {
        bottom_panel_height: 500,
        ..Default::default()
    };
    assert!(should_restore_workbench_layout(None, &a, false));
    assert!(!should_restore_workbench_layout(Some(&a), &a, false));
    assert!(should_restore_workbench_layout(Some(&a), &b, false));
    assert!(!should_restore_workbench_layout(Some(&a), &b, true));
    assert!(!should_restore_workbench_layout(None, &a, true));
}

/// 恢复仅把稀疏配置的逻辑宽度转换为 Kit 槽宽，不把未设置工具的动态默认补成覆盖。
///
/// @author kongweiguang
#[test]
fn persisted_dock_widths_restore_sparse_overrides_with_one_gutter() {
    let mut state = ToolRailPanelState::default();
    let widths = BTreeMap::from([
        (ToolRailToolId::Sftp, 500),
        (ToolRailToolId::AgentLauncher, 388),
    ]);
    state.restore_attached_widths(&widths);
    assert_eq!(state.attached_width(ToolRailToolId::Sftp), Some(px(504.)));
    assert_eq!(
        state.attached_width(ToolRailToolId::AgentLauncher),
        Some(px(504.))
    );
    assert_eq!(state.attached_width(ToolRailToolId::Logs), Some(px(504.)));
    state.restore_attached_widths(&BTreeMap::new());
    assert_eq!(state.attached_width(ToolRailToolId::Sftp), None);
}

#[cfg(target_os = "windows")]
mod windows_title_bar_hitboxes {
    use crate::{root::windows_title_bar::windows_workbench_title_bar, theme::ThemeTokens};
    use gpui::{
        AppContext as _, Context, InteractiveElement as _, IntoElement, Render, Styled as _,
        TestAppContext, VisualTestContext, Window, div, px, size,
    };
    use gpui_kit as gpui;

    struct TitleBarHarness;

    impl Render for TitleBarHarness {
        /// 只用可伸缩内容夹具，隔离标题栏自身的拖动和系统控件几何。
        ///
        /// @author kongweiguang
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = ThemeTokens::current(window, cx);
            let content = div()
                .id("windows-workbench-test-content")
                .debug_selector(|| "windows-workbench-test-content".to_owned())
                .flex_1()
                .min_w(px(0.))
                .h_full()
                .into_any_element();
            windows_workbench_title_bar(content, theme.materials.chrome, theme, window)
        }
    }

    /// 使用生产标题栏组件和 GPUI 测试窗口，让命中区域回归读取真实布局边界。
    ///
    /// @author kongweiguang
    fn fixture(cx: &mut TestAppContext) -> &mut VisualTestContext {
        cx.update(|app| {
            gpui::init(app);
            app.set_reduce_motion(true);
        });
        let (_, visual) = cx.add_window_view(|window, cx| {
            let harness = cx.new(|_| TitleBarHarness);
            gpui::base::Root::new(harness, window, cx)
        });
        draw(visual);
        visual
    }

    /// 绘制后再读取 debug selector，避免把未挂载或尚未测量的区域误判为零尺寸。
    ///
    /// @author kongweiguang
    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear(app));
    }

    /// 窄窗口先压缩标题内容，保底拖动区与系统按钮仍保持独立、完整的命中边界。
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn fallback_drag_region_stays_separate_from_window_controls(cx: &mut TestAppContext) {
        let visual = fixture(cx);
        let wide_content = visual
            .debug_bounds("windows-workbench-test-content")
            .expect("test content is measured");

        visual.simulate_resize(size(px(320.), px(180.)));
        draw(visual);

        let drag = visual
            .debug_bounds("windows-workbench-title-bar-drag-space")
            .expect("fallback drag region is measured");
        let narrow_content = visual
            .debug_bounds("windows-workbench-test-content")
            .expect("narrow content is measured");
        let mut controls = [
            "windows-workbench-minimize",
            "windows-workbench-maximize",
            "windows-workbench-close",
        ]
        .into_iter()
        .filter_map(|selector| visual.debug_bounds(selector))
        .collect::<Vec<_>>();

        assert_eq!(drag.size.width.as_f32(), 64.);
        assert!(narrow_content.size.width < wide_content.size.width);
        assert_eq!(
            controls.len(),
            3,
            "test window exposes all three system controls"
        );
        controls.sort_by(|left, right| left.origin.x.partial_cmp(&right.origin.x).unwrap());
        assert!(
            drag.origin.x + drag.size.width <= controls[0].origin.x,
            "fallback drag area must end before the minimize hitbox"
        );
        for pair in controls.windows(2) {
            assert!(
                pair[0].origin.x + pair[0].size.width <= pair[1].origin.x,
                "adjacent system control hitboxes must not overlap"
            );
            assert_eq!(pair[0].size.width.as_f32(), 34.);
        }
        assert_eq!(controls[2].size.width.as_f32(), 34.);
    }
}
