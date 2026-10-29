//! 设置下拉选项的稳定 ID 映射测试。
//!
//! @author kongweiguang

use super::{accept_key_option_value, presentation_option_value, remote_refresh_option_value};
use kerminal_application::preferences::{
    TerminalCommandSuggestionPresentation, TerminalCommandSuggestionRemoteRefresh,
    TerminalInlineSuggestionAcceptKey,
};

/// Select values are persisted enum IDs, so their encoding must not depend on localized labels.
/// @author kongweiguang
#[test]
fn suggestion_select_values_map_to_stable_settings_ids() {
    assert_eq!(
        presentation_option_value(TerminalCommandSuggestionPresentation::InlineAndMenu),
        "inlineAndMenu"
    );
    assert_eq!(
        presentation_option_value(TerminalCommandSuggestionPresentation::Inline),
        "inline"
    );
    assert_eq!(
        presentation_option_value(TerminalCommandSuggestionPresentation::Off),
        "off"
    );

    assert_eq!(
        accept_key_option_value(TerminalInlineSuggestionAcceptKey::RightArrow),
        "rightArrow"
    );
    assert_eq!(
        accept_key_option_value(TerminalInlineSuggestionAcceptKey::Disabled),
        "disabled"
    );

    assert_eq!(
        remote_refresh_option_value(TerminalCommandSuggestionRemoteRefresh::Safe),
        "safe"
    );
    assert_eq!(
        remote_refresh_option_value(TerminalCommandSuggestionRemoteRefresh::Off),
        "off"
    );
}
