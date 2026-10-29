//! 右栏自定义器的配置兼容与分区排序回归测试。
//!
//! @author kongweiguang

use super::{
    CUSTOMIZABLE_TOOLS, LegacyContextSettings, ToolRailSections, customizable_settings,
    move_before_in_sections, move_to_section_end, split_sections,
};
use kerminal_application::preferences::{ToolRailPanelPlacement, ToolRailSettings, ToolRailToolId};

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
    let legacy = LegacyContextSettings {
        order_index: Some(0),
        hidden: true,
        bottom: true,
        placement: Some(ToolRailPanelPlacement::Center),
    };

    let draft = customizable_settings(settings, legacy);
    assert_eq!(draft.order.first(), Some(&ToolRailToolId::Context));
    assert!(draft.hidden.contains(&ToolRailToolId::Context));
    assert!(draft.bottom.contains(&ToolRailToolId::Context));
    assert_eq!(
        draft.panel_placements.get(&ToolRailToolId::Context),
        Some(&ToolRailPanelPlacement::Center)
    );
    let sections = split_sections(&draft);
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
        hidden: CUSTOMIZABLE_TOOLS.to_vec(),
        ..ToolRailSettings::default()
    };
    let draft = customizable_settings(
        settings,
        LegacyContextSettings {
            order_index: None,
            hidden: false,
            bottom: false,
            placement: None,
        },
    );

    assert!(!draft.hidden.contains(&draft.order[0]));
    assert_eq!(draft.hidden.len(), CUSTOMIZABLE_TOOLS.len() - 1);
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
