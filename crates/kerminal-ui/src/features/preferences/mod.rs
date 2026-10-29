//! 原生偏好、profile 与快捷键页面的本地交互模型。
//!
//! @author kongweiguang

mod color_picker;
mod draft;
mod panel;
mod render;
mod render_app_palette;
mod render_command_suggestions;
mod render_connectivity;
mod render_desktop_about;
mod render_keyword_highlights;
mod render_profiles;
mod render_settings;
mod render_sftp;
mod render_sync;
mod render_terminal;
mod validation;

#[cfg(feature = "right-rail-acceptance")]
pub(crate) mod settings_acceptance;

#[cfg(test)]
#[path = "../../../tests/unit/preferences_settings_ui.rs"]
mod settings_ui_tests;

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
