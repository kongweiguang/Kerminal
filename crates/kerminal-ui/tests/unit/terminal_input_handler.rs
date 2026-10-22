//! Option-as-Meta 对输入法按键优先级的纯策略契约。
//!
//! @author kongweiguang

use super::TerminalImePreferenceHandler;

/// 只有实际输入包装器在 macOS 设置开启且按下 Option 时覆盖 IME 优先路由，其它情形仍委托原 handler。
///
/// @author kongweiguang
#[test]
fn registered_wrapper_overrides_ime_only_for_enabled_macos_option() {
    let enabled = TerminalImePreferenceHandler::new((), true);
    assert_eq!(enabled.ime_override_for_option(true, true), Some(false));
    assert_eq!(enabled.ime_override_for_option(true, false), None);
    assert_eq!(enabled.ime_override_for_option(false, true), None);

    let disabled = TerminalImePreferenceHandler::new((), false);
    assert_eq!(disabled.ime_override_for_option(true, true), None);
}
