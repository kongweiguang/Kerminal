//! 终端交互策略投影与右键兼容语义的边界测试。
//!
//! @author kongweiguang

use kerminal_application::preferences::SettingsSnapshot;

use super::TerminalInteractionPolicy;

/// 核对仍被生产输入和选择调用的策略投影；右键与标签序号分别由真实 Canvas、Tab 测试覆盖。
///
/// @author kongweiguang
#[test]
fn projects_interaction_preferences_without_reinterpretation() {
    let mut settings = SettingsSnapshot::default();
    settings.terminal.mac_option_is_meta = true;
    settings.terminal.selection_copy = true;
    settings.terminal.show_command_block_rail = false;

    let policy = TerminalInteractionPolicy::from_snapshot(&settings);

    assert!(policy.mac_option_is_meta());
    assert!(policy.copy_on_selection());
    assert!(!policy.command_block_rail_visible(true));
    assert!(!policy.command_block_rail_visible(false));
}

/// Command block rail 只能在用户开启且当前工作区支持 shell integration 时出现。
#[test]
fn command_block_rail_requires_preference_and_shell_assist() {
    let mut settings = SettingsSnapshot::default();
    let mut policy = TerminalInteractionPolicy::from_snapshot(&settings);
    assert!(policy.command_block_rail_visible(true));
    assert!(!policy.command_block_rail_visible(false));

    settings.terminal.show_command_block_rail = false;
    policy = TerminalInteractionPolicy::from_snapshot(&settings);
    assert!(!policy.command_block_rail_visible(true));
}
