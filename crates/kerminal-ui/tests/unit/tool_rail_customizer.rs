//! 右栏自定义器的配置兼容与分区排序回归测试。
//!
//! @author kongweiguang

use super::{
    ToolRailSections, customizable_settings, customizer_list_max_height, merge_registered_order,
    move_before_in_sections, move_to_section_end, reset_registered_tools, split_sections,
};
use crate::plugins::tools::BUILTIN_TOOLS;
use crate::theme::UiMetrics;
use gpui_kit::px;
use kerminal_application::preferences::{
    InterfaceDensity, ToolRailPanelPlacement, ToolRailSettings, ToolRailToolId,
};

/// 小窗口或放大字号时仍留出可滚动的内容槽，且列表高度不能超过实际视口。
///
/// @author kongweiguang
#[test]
fn customizer_list_stays_reachable_at_supported_zoom_and_density() {
    for density in [
        InterfaceDensity::Compact,
        InterfaceDensity::Comfortable,
        InterfaceDensity::Spacious,
    ] {
        for rem in [16., 20., 24., 32.] {
            let metrics = UiMetrics::resolve(density, px(rem));
            for viewport in [600., 760., 900.] {
                let height = customizer_list_max_height(px(viewport), metrics);
                assert!(
                    height > px(0.),
                    "{density:?}, rem={rem}, viewport={viewport}"
                );
                assert!(height <= px(viewport));
                assert!(height >= metrics.row_height.min(px(viewport / 3.)));
            }
        }
    }
}

/// 退役 Context 只保留原配置位置，不进入本次八项编辑器的可见分区。
#[test]
fn legacy_context_settings_survive_without_becoming_a_customizer_row() {
    let mut settings = ToolRailSettings::default();
    settings.order.insert(0, ToolRailToolId::Context);
    settings.hidden.push(ToolRailToolId::Context);
    settings.bottom.push(ToolRailToolId::Context);
    settings
        .panel_placements
        .insert(ToolRailToolId::Context, ToolRailPanelPlacement::Center);
    let draft = customizable_settings(settings, &BUILTIN_TOOLS);
    assert_eq!(draft.order.first(), Some(&ToolRailToolId::Context));
    assert!(draft.hidden.contains(&ToolRailToolId::Context));
    assert!(draft.bottom.contains(&ToolRailToolId::Context));
    assert_eq!(
        draft.panel_placements.get(&ToolRailToolId::Context),
        Some(&ToolRailPanelPlacement::Center)
    );
    let sections = split_sections(&draft, &BUILTIN_TOOLS);
    assert_eq!(
        sections.main,
        [
            ToolRailToolId::AgentLauncher,
            ToolRailToolId::Sftp,
            ToolRailToolId::Snippets,
            ToolRailToolId::Tmux,
            ToolRailToolId::Ports,
            ToolRailToolId::System,
            ToolRailToolId::Containers,
        ]
    );
    assert_eq!(sections.bottom, [ToolRailToolId::Logs]);
}

/// 全部八项都隐藏时恢复当前排序第一项，确保 rail 总有可发现入口。
#[test]
fn normalizing_all_hidden_tools_restores_the_first_ordered_entry() {
    let settings = ToolRailSettings {
        hidden: BUILTIN_TOOLS.to_vec(),
        ..ToolRailSettings::default()
    };
    let draft = customizable_settings(settings, &BUILTIN_TOOLS);

    assert!(!draft.hidden.contains(&draft.order[0]));
    assert_eq!(draft.hidden.len(), BUILTIN_TOOLS.len() - 1);
}

/// 已卸载工具穿插在顺序中时，排序和恢复默认都保留其完整配置。
///
/// @author kongweiguang
#[test]
fn unknown_tool_settings_survive_customization_and_reset() {
    let unknown = ToolRailToolId::from_id("vendor.weather").expect("valid plugin id");
    let absent = ToolRailToolId::from_id("vendor.absent").expect("valid plugin id");
    let mut settings = ToolRailSettings::default();
    settings.order.insert(1, unknown);
    settings.hidden.extend([unknown, absent]);
    settings.bottom.extend([unknown, absent]);
    settings
        .panel_placements
        .insert(unknown, ToolRailPanelPlacement::Left);
    settings
        .panel_placements
        .insert(absent, ToolRailPanelPlacement::Center);

    let mut draft = customizable_settings(settings, &BUILTIN_TOOLS);
    let mut sections = split_sections(&draft, &BUILTIN_TOOLS);
    assert!(!sections.main.contains(&unknown));
    assert!(!sections.bottom.contains(&unknown));
    assert!(move_before_in_sections(
        &mut sections,
        ToolRailToolId::Sftp,
        ToolRailToolId::AgentLauncher
    ));
    let reordered = [sections.main, sections.bottom].concat();
    draft.order = merge_registered_order(&draft.order, &reordered, &BUILTIN_TOOLS);
    assert_eq!(draft.order[1], unknown);

    let reset = reset_registered_tools(&draft, &BUILTIN_TOOLS);
    assert_eq!(reset.order[1], unknown);
    for tool in [unknown, absent] {
        assert!(reset.hidden.contains(&tool));
        assert!(reset.bottom.contains(&tool));
        assert_eq!(
            reset.panel_placements.get(&tool),
            draft.panel_placements.get(&tool)
        );
    }
}

/// 新注册工具可进入编辑分区，既有隐藏和停靠设置继续生效。
///
/// @author kongweiguang
#[test]
fn registered_plugin_is_available_without_changing_its_saved_preferences() {
    let plugin = ToolRailToolId::from_id("vendor.weather").expect("valid plugin id");
    let mut registered = BUILTIN_TOOLS.to_vec();
    registered.push(plugin);
    let mut settings = ToolRailSettings::default();
    settings.hidden.push(plugin);
    settings.bottom.push(plugin);
    settings
        .panel_placements
        .insert(plugin, ToolRailPanelPlacement::Left);
    let draft = customizable_settings(settings, &registered);
    let sections = split_sections(&draft, &registered);
    assert!(sections.bottom.contains(&plugin));
    assert!(draft.hidden.contains(&plugin));
    assert_eq!(
        draft.effective_panel_placement(plugin),
        ToolRailPanelPlacement::Left
    );
}

/// 拖到另一分区会随目标移动，区尾固定和同项释放保持幂等。
#[test]
fn drag_drop_preserves_one_global_order_and_moves_between_sections() {
    let mut sections = ToolRailSections {
        main: vec![ToolRailToolId::AgentLauncher, ToolRailToolId::Sftp],
        bottom: vec![ToolRailToolId::Logs, ToolRailToolId::Containers],
    };

    assert!(move_before_in_sections(
        &mut sections,
        ToolRailToolId::Sftp,
        ToolRailToolId::Logs,
    ));
    assert_eq!(sections.main, [ToolRailToolId::AgentLauncher]);
    assert_eq!(
        sections.bottom,
        [
            ToolRailToolId::Sftp,
            ToolRailToolId::Logs,
            ToolRailToolId::Containers
        ]
    );
    assert!(!move_before_in_sections(
        &mut sections,
        ToolRailToolId::Sftp,
        ToolRailToolId::Sftp,
    ));

    move_to_section_end(&mut sections, ToolRailToolId::AgentLauncher, true);
    assert!(sections.main.is_empty());
    assert_eq!(sections.bottom.last(), Some(&ToolRailToolId::AgentLauncher));
}
