//! 应用配色单选组的键盘边界与稳定枚举映射。
//!
//! @author kongweiguang

use super::{AppColorScheme, color_scheme_name, palette_key_target};

/// 边界循环的焦点步数受六个实际单选项约束，非方向键应继续交由组件自身处理。
///
/// @author kongweiguang
#[test]
fn palette_arrow_keys_wrap_without_capturing_other_keys() {
    assert_eq!(palette_key_target(0, "left"), Some((5, true, 5)));
    assert_eq!(palette_key_target(5, "right"), Some((0, false, 5)));
    assert_eq!(palette_key_target(2, "up"), Some((1, false, 1)));
    assert_eq!(palette_key_target(2, "down"), Some((3, true, 1)));
    assert_eq!(palette_key_target(2, "space"), None);
    assert_eq!(palette_key_target(6, "right"), None);
}

/// 展示名称与配置身份使用同一枚举顺序，避免点击与保存映射到不同色板。
///
/// @author kongweiguang
#[test]
fn palette_names_and_stable_ids_cover_each_builtin_scheme() {
    assert_eq!(
        AppColorScheme::ALL.map(color_scheme_name),
        [
            "Kerminal",
            "JetBrains",
            "Fleet",
            "Xcode",
            "Claude",
            "VS Code"
        ]
    );
    assert_eq!(
        AppColorScheme::ALL.map(AppColorScheme::as_str),
        [
            "kerminal",
            "jetbrains",
            "fleet",
            "xcode",
            "claude",
            "vscode"
        ]
    );
}
