//! 固定 Files 与 Containers 面板停靠展示和其他工具位置之间的纯回归测试。
//!
//! @author kongweiguang

use super::{can_change_tool_rail_placement, placement_options};
use kerminal_application::preferences::{ToolRailPanelPlacement, ToolRailToolId};

/// Files 与 Containers 的存量停靠位置只显示固定右侧抽屉，设置页不允许改动。
///
/// @author kongweiguang
#[test]
fn files_and_containers_placement_is_fixed_to_the_right_drawer() {
    for tool in [ToolRailToolId::Sftp, ToolRailToolId::Containers] {
        let options = placement_options(tool);
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].0, "attached");
        assert_eq!(options[0].2, ToolRailPanelPlacement::Attached);
        let label = options[0].1.to_string();
        assert!(label.contains("固定") || label.contains("fixed"));
        assert!(!can_change_tool_rail_placement(tool));
    }
}

/// 其他工具仍保留原四种停靠方式，包括居中。
///
/// @author kongweiguang
#[test]
fn other_tool_placements_remain_user_selectable() {
    let options = placement_options(ToolRailToolId::Logs);
    assert_eq!(options.len(), 4);
    assert!(
        options
            .iter()
            .any(|option| option.2 == ToolRailPanelPlacement::Center)
    );
    assert!(can_change_tool_rail_placement(ToolRailToolId::Logs));
}
