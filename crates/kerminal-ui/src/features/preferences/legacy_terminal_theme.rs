//! 在同一应用会话内统一旧终端浅深主题的首次归并结果。
//!
//! @author kongweiguang

use gpui_kit::{App, Global, WindowAppearance};
use kerminal_application::preferences::{SettingsSnapshot, TerminalColorScheme, ThemeMode};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LegacyThemeSource {
    light: TerminalColorScheme,
    dark: TerminalColorScheme,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct LegacyThemeResolution {
    source: LegacyThemeSource,
    selected: TerminalColorScheme,
}

/// 缓存只包含当前旧字段来源，不持久化且不影响配置 revision。
#[derive(Clone, Copy, Default)]
struct LegacyTerminalThemeCache {
    resolution: Option<LegacyThemeResolution>,
}

impl Global for LegacyTerminalThemeCache {}

impl LegacyTerminalThemeCache {
    /// 同一旧来源在后台重读时保持首次身份；来源变化才重新根据外观选择。
    ///
    /// @author kongweiguang
    fn resolve(&mut self, source: LegacyThemeSource, dark: bool) -> TerminalColorScheme {
        if let Some(resolution) = self.resolution
            && resolution.source == source
        {
            return resolution.selected;
        }
        let selected = if dark { source.dark } else { source.light };
        self.resolution = Some(LegacyThemeResolution { source, selected });
        selected
    }
}

/// 先在缓存副本上计算候选快照，未采纳的回读不能改变全局主题身份。
///
/// @author kongweiguang
pub(super) fn preview_legacy_terminal_theme(
    snapshot: &mut SettingsSnapshot,
    appearance: WindowAppearance,
    cx: &App,
) -> Option<LegacyThemeResolution> {
    if !snapshot.terminal.legacy_color_scheme_missing {
        return None;
    }
    let dark = match snapshot.theme_mode {
        ThemeMode::Dark => true,
        ThemeMode::Light => false,
        ThemeMode::System => matches!(
            appearance,
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        ),
    };
    let source = LegacyThemeSource {
        light: snapshot.terminal.light_color_scheme,
        dark: snapshot.terminal.dark_color_scheme,
    };
    let mut cache = cx
        .try_global::<LegacyTerminalThemeCache>()
        .copied()
        .unwrap_or_default();
    let selected = cache.resolve(source, dark);
    snapshot
        .terminal
        .resolve_legacy_terminal_scheme(selected == source.dark);
    cache.resolution
}

/// 仅在调用方确认采纳快照后更新共享缓存，避免丢弃的回读影响后续窗口。
///
/// @author kongweiguang
pub(super) fn commit_legacy_terminal_theme(
    resolution: Option<LegacyThemeResolution>,
    cx: &mut App,
) {
    if let Some(resolution) = resolution
        && cx
            .try_global::<LegacyTerminalThemeCache>()
            .and_then(|cache| cache.resolution)
            != Some(resolution)
    {
        cx.set_global(LegacyTerminalThemeCache {
            resolution: Some(resolution),
        });
    }
}

/// Root 与设置面板只对已接纳快照共用一次内存归并，旧字段变化仍可触发新选择。
///
/// @author kongweiguang
pub(crate) fn resolve_legacy_terminal_theme(
    snapshot: &mut SettingsSnapshot,
    appearance: WindowAppearance,
    cx: &mut App,
) {
    let resolution = preview_legacy_terminal_theme(snapshot, appearance, cx);
    commit_legacy_terminal_theme(resolution, cx);
}

#[cfg(test)]
#[path = "../../../tests/unit/legacy_terminal_theme.rs"]
mod tests;
