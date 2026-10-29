//! Files 与 Containers 的停靠位置和其他工具保持一致的纯回归测试。
//!
//! @author kongweiguang

use super::{can_change_tool_rail_placement, placement_options};
use kerminal_application::preferences::{ToolRailPanelPlacement, ToolRailToolId};

/// Files 与 Containers 可在右侧、左侧、底部和中央打开，且位置修改入口保持可用。
///
/// @author kongweiguang
#[test]
fn files_and_containers_support_all_panel_placements() {
    for tool in [ToolRailToolId::Sftp, ToolRailToolId::Containers] {
        let options = placement_options(tool);
        assert_eq!(options.len(), 4);
        assert_eq!(
            options.iter().map(|option| option.2).collect::<Vec<_>>(),
            vec![
                ToolRailPanelPlacement::Attached,
                ToolRailPanelPlacement::Left,
                ToolRailPanelPlacement::Bottom,
                ToolRailPanelPlacement::Center,
            ]
        );
        assert!(can_change_tool_rail_placement(tool));
    }
}

/// 所有其他工具也保留同一套四种停靠方式，包括居中。
///
/// @author kongweiguang
#[test]
fn every_tool_placement_remains_user_selectable() {
    let options = placement_options(ToolRailToolId::Logs);
    assert_eq!(options.len(), 4);
    assert!(
        options
            .iter()
            .any(|option| option.2 == ToolRailPanelPlacement::Center)
    );
    assert!(can_change_tool_rail_placement(ToolRailToolId::Logs));
}
