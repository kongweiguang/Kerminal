//! 原生偏好、profile 与快捷键页面的本地交互模型。
//!
//! @author kongweiguang

mod draft;
mod panel;
mod render;
mod render_profiles;
mod render_settings;
mod render_sync;
mod render_terminal_behavior;
mod render_vault;
mod validation;

pub use draft::{CloseRequest, ConfigDraft, DraftSnapshot, DraftStatus, DraftTransitionError};
pub use kerminal_application::preferences::{
    BackgroundFit, InterfaceDensity, InterfaceLanguage, KeybindingScope, ProfileDraft,
    ProfileRevision, TerminalColorScheme, TerminalCursorStyle, TerminalSettings, ThemeMode,
};
pub(crate) use panel::PreferencesPanelServices;
pub use panel::{PreferencesEvent, PreferencesPanel};
pub use validation::{
    KeybindingDraft, KeyboardPlatform, ProfileIssue, ProfileIssueKind, SettingsDraft,
    SettingsIssue, SettingsIssueKind, validate_profile, validate_settings,
};

#[cfg(test)]
#[path = "../../../tests/unit/preferences_draft.rs"]
mod tests;
