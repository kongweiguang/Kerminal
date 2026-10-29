//! Read-only state projections for the isolated native settings acceptance runner.
//!
//! @author kongweiguang

use super::DraftStatus;
use super::panel::{PreferencesPanel, PreferencesSection};
use gpui_kit::{App, FocusHandle, Focusable as _};

impl PreferencesPanel {
    /// 同步按钮仅在真实仓库就绪且 Git/密钥操作空闲后可用于端到端点击。
    pub(crate) fn settings_acceptance_sync_ready(&self) -> bool {
        self.sync_status.as_ref().is_some_and(|status| {
            status.git_available
                && status.repository
                    == kerminal_application::configuration_sync::SyncRepositoryState::Ready
        }) && !self.settings_acceptance_sync_busy()
    }

    /// 同步后的状态刷新和密钥读写也属于忙碌区间，避免验收误点禁用按钮。
    pub(crate) fn settings_acceptance_sync_busy(&self) -> bool {
        self.sync_loading
            || self.connectivity.sync_running
            || self.connectivity.key_loading
            || self.connectivity.key_saving
    }

    /// 公开生产用例的阶段回执用于验收，不把文字回显当作远端已提交证据。
    pub(crate) fn settings_acceptance_sync_result(
        &self,
    ) -> Option<kerminal_application::configuration_sync::ConfigurationSyncRunResult> {
        self.connectivity.sync_run_result.clone()
    }

    /// 查询失败与业务阶段失败分开投影，便于端到端等待明确终态。
    pub(crate) fn settings_acceptance_sync_error(&self) -> Option<String> {
        self.sync_error.clone()
    }

    /// 裁切区域来自生产滚动句柄，避免验收把部分相交的字段当作可点击。
    pub(crate) fn settings_acceptance_content_bounds(&self) -> gpui_kit::Bounds<gpui_kit::Pixels> {
        self.content_scroll.bounds()
    }

    /// 等待真实协议查询完成后才截图，避免将异步加载帧当成最终注册状态。
    pub(crate) fn settings_acceptance_protocol_settled(&self) -> bool {
        !self.external_protocol_loading
            && (self.external_protocol_status.is_some() || self.external_protocol_error.is_some())
    }

    /// Exposes the rendered section identity so acceptance can verify navigation after a real click.
    pub(crate) fn settings_acceptance_page_id(&self) -> &'static str {
        match self.section {
            PreferencesSection::Appearance => "appearance",
            PreferencesSection::Terminal => "terminal",
            PreferencesSection::KeywordHighlights => "keywords",
            PreferencesSection::CommandSuggestions => "suggestions",
            PreferencesSection::Sftp => "sftp",
            PreferencesSection::ExternalLaunch => "external-launch",
            PreferencesSection::Mcp => "mcp",
            PreferencesSection::ConfigurationSync => "sync",
            PreferencesSection::Desktop => "desktop",
            PreferencesSection::Shortcuts => "shortcuts",
            PreferencesSection::About => "about",
            PreferencesSection::Profiles => "profiles",
            PreferencesSection::Integrations => "integrations",
        }
    }

    /// Requires the typed draft to be idle before a persisted-value readback can be trusted.
    pub(crate) fn settings_acceptance_draft_ready(&self) -> bool {
        !self.loading_settings
            && self
                .settings
                .as_ref()
                .is_some_and(|draft| draft.status() == DraftStatus::Ready)
    }

    /// Keeps initial or explicit settings reload work distinct from an in-flight autosave.
    pub(crate) fn settings_acceptance_loading(&self) -> bool {
        self.loading_settings || self.settings.is_none()
    }

    /// Reports whether the current typed settings differ from their last confirmed revision.
    pub(crate) fn settings_acceptance_draft_dirty(&self) -> bool {
        self.settings.as_ref().is_some_and(|draft| draft.is_dirty())
    }

    /// Returns the draft theme mode for save and readback assertions without mutating the panel.
    pub(crate) fn settings_acceptance_draft_theme(&self) -> Option<&'static str> {
        self.settings
            .as_ref()
            .map(|draft| theme_mode_id(draft.value().theme_mode))
    }

    /// Returns the draft density for save and readback assertions without mutating the panel.
    pub(crate) fn settings_acceptance_draft_density(&self) -> Option<&'static str> {
        self.settings
            .as_ref()
            .map(|draft| density_id(draft.value().interface_density))
    }

    /// Reads the language before and after Escape so dismissal cannot accidentally confirm a selection.
    pub(crate) fn settings_acceptance_draft_language(&self) -> Option<&'static str> {
        self.settings
            .as_ref()
            .map(|draft| interface_language_id(draft.value().interface_language))
    }

    /// Compares the Select's committed value separately from the Settings draft value.
    pub(crate) fn settings_acceptance_selected_language(&self, cx: &App) -> Option<&'static str> {
        self.interface_language_select
            .read(cx)
            .selected_value()
            .copied()
            .map(interface_language_id)
    }

    /// Returns the real Select trigger focus handle used by its Escape dismissal path.
    pub(crate) fn settings_acceptance_language_focus(&self, cx: &App) -> FocusHandle {
        self.interface_language_select.read(cx).focus_handle(cx)
    }

    /// Exposes only disclosure booleans so screenshot actions can verify real expansion state.
    pub(crate) fn settings_acceptance_disclosures(&self) -> [bool; 5] {
        [
            self.terminal_renderer_open,
            self.sftp_advanced_open,
            self.connectivity.external_compatibility_expanded,
            self.connectivity.mcp_connection_expanded,
            self.desktop_about.advanced_expanded,
        ]
    }

    /// Reports the typed rule count so acceptance can distinguish an added rule from a rendered button.
    pub(crate) fn settings_acceptance_draft_keyword_rule_count(&self) -> Option<usize> {
        self.settings
            .as_ref()
            .map(|draft| draft.value().terminal.keyword_highlights.rules.len())
    }

    /// Exposes only the active keyword editor session so acceptance can prove local-draft cancellation.
    pub(crate) fn settings_acceptance_keyword_editor(
        &self,
    ) -> Option<KeywordEditorAcceptanceState> {
        let session = self.keyword_editor.session.as_ref()?;
        Some(KeywordEditorAcceptanceState {
            is_new: session.is_new,
            rule_id: session.draft.id.clone(),
            pattern: session.draft.pattern.clone(),
            note: session.draft.note.clone(),
            save_pending: self.keyword_editor.save_pending.is_some(),
        })
    }
}

/// A bounded projection of unsaved editor fields used only by hidden-window acceptance.
pub(crate) struct KeywordEditorAcceptanceState {
    pub(crate) is_new: bool,
    pub(crate) rule_id: String,
    pub(crate) pattern: String,
    pub(crate) note: String,
    pub(crate) save_pending: bool,
}

/// Keeps persisted display names aligned with the product's stable preference enums.
fn theme_mode_id(mode: super::ThemeMode) -> &'static str {
    match mode {
        super::ThemeMode::System => "system",
        super::ThemeMode::Light => "light",
        super::ThemeMode::Dark => "dark",
    }
}

/// Keeps persisted density names aligned with the product's stable preference enums.
fn density_id(density: super::InterfaceDensity) -> &'static str {
    match density {
        super::InterfaceDensity::Compact => "compact",
        super::InterfaceDensity::Comfortable => "comfortable",
        super::InterfaceDensity::Spacious => "spacious",
    }
}

/// Keeps language verification independent of localized labels and the user's system locale.
fn interface_language_id(language: super::InterfaceLanguage) -> &'static str {
    match language {
        super::InterfaceLanguage::System => "system",
        super::InterfaceLanguage::ZhCn => "zh-CN",
        super::InterfaceLanguage::EnUs => "en-US",
    }
}
