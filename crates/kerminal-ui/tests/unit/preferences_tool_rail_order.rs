//! 工具栏持久排序的纯回归测试；沿父模块访问私有投影函数。
//!
//! @author kongweiguang

use super::{move_visible_tool_rail_item, visible_tool_rail_indices};
use kerminal_application::preferences::{ToolRailSettings, ToolRailToolId};

/// 右栏与底栏排序保持分区，跳过隐藏项但不改写其持久位置。
///
/// @author kongweiguang
#[test]
fn visible_order_moves_only_within_its_zone_across_hidden_entries() {
    let mut settings = ToolRailSettings::default();
    settings.hidden.push(ToolRailToolId::Sftp);
    settings.bottom = vec![ToolRailToolId::Snippets, ToolRailToolId::Logs];

    assert!(move_visible_tool_rail_item(
        &mut settings,
        ToolRailToolId::AgentLauncher,
        1
    ));
    assert_eq!(
        settings.order,
        vec![
            ToolRailToolId::Tmux,
            ToolRailToolId::Sftp,
            ToolRailToolId::Snippets,
            ToolRailToolId::AgentLauncher,
            ToolRailToolId::Ports,
            ToolRailToolId::System,
            ToolRailToolId::Logs,
            ToolRailToolId::Containers,
        ]
    );
    assert!(move_visible_tool_rail_item(
        &mut settings,
        ToolRailToolId::Snippets,
        1
    ));
    assert_eq!(
        visible_tool_rail_indices(&settings, true)
            .into_iter()
            .map(|index| settings.order[index])
            .collect::<Vec<_>>(),
        vec![ToolRailToolId::Logs, ToolRailToolId::Snippets]
    );
}

/// 旧 Context、隐藏项和不受支持的方向不能进入工作台的可见排序。
///
/// @author kongweiguang
#[test]
fn context_hidden_and_invalid_moves_are_rejected() {
    let mut settings = ToolRailSettings::default();
    settings.hidden.push(ToolRailToolId::Sftp);

    assert!(!move_visible_tool_rail_item(
        &mut settings,
        ToolRailToolId::Context,
        1
    ));
    assert!(!move_visible_tool_rail_item(
        &mut settings,
        ToolRailToolId::Sftp,
        1
    ));
    assert!(!move_visible_tool_rail_item(
        &mut settings,
        ToolRailToolId::AgentLauncher,
        0
    ));
}
