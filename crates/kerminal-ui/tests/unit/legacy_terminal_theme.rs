//! 旧浅深主题的会话内归并不受外观变化或后台回读影响。
//!
//! @author kongweiguang

use super::*;

/// 同一来源跨系统明暗和后台重读保持身份，外部修改旧字段后才重新选择。
///
/// @author kongweiguang
#[test]
fn legacy_resolution_is_stable_until_source_changes() {
    let mut cache = LegacyTerminalThemeCache::default();
    let source = LegacyThemeSource {
        light: TerminalColorScheme::Github,
        dark: TerminalColorScheme::TokyoNight,
    };
    assert_eq!(cache.resolve(source, false), TerminalColorScheme::Github);
    assert_eq!(cache.resolve(source, true), TerminalColorScheme::Github);

    let externally_edited = LegacyThemeSource {
        dark: TerminalColorScheme::Solarized,
        ..source
    };
    assert_eq!(
        cache.resolve(externally_edited, true),
        TerminalColorScheme::Solarized
    );
    assert_eq!(
        cache.resolve(externally_edited, false),
        TerminalColorScheme::Solarized
    );
}
