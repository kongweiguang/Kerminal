//! Root state queries used by the isolated native settings parity runner.
//!
//! @author kongweiguang

use gpui_kit::{Context, FocusHandle};

use super::RootView;

/// A minimal live-state snapshot; persistence is verified separately through PreferencesService.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsAcceptanceState {
    modal_open: bool,
    modal_generation: u64,
    settings_ready: bool,
    settings_loading: bool,
    draft_ready: bool,
    draft_dirty: bool,
    page_id: &'static str,
    draft_theme_mode: Option<&'static str>,
    draft_density: Option<&'static str>,
    draft_interface_language: Option<&'static str>,
    selected_interface_language: Option<&'static str>,
    language_select_focus: FocusHandle,
    draft_keyword_rule_count: Option<usize>,
    expanded_disclosures: [bool; 5],
    modal_return_focus: Option<FocusHandle>,
    keyword_editor_open: bool,
    keyword_editor_is_new: bool,
    keyword_editor_rule_id: Option<String>,
    keyword_editor_pattern: Option<String>,
    keyword_editor_note: Option<String>,
    keyword_save_pending: bool,
    protocol_settled: bool,
    sync_ready: bool,
    sync_busy: bool,
    sync_result: Option<kerminal_application::configuration_sync::ConfigurationSyncRunResult>,
    sync_error: Option<String>,
}

impl SettingsAcceptanceState {
    /// 只有仓库和同步/密钥状态都稳定时，原生验收才发送真实按钮点击。
    pub fn sync_ready(&self) -> bool {
        self.sync_ready
    }

    /// 包含后台状态刷新，避免把运行中残留回执当成本轮终态。
    pub fn sync_busy(&self) -> bool {
        self.sync_busy
    }

    /// 提供完整阶段结果供验收断言 commit/pull/push 的实际顺序。
    pub fn sync_result(
        &self,
    ) -> Option<&kerminal_application::configuration_sync::ConfigurationSyncRunResult> {
        self.sync_result.as_ref()
    }

    /// 保留独立的读取/启动错误，避免验收超时掩盖真实原因。
    pub fn sync_error(&self) -> Option<&str> {
        self.sync_error.as_deref()
    }

    /// 区分协议查询的终态与加载帧；读取失败仍由生产页面呈现真实错误。
    pub fn protocol_settled(&self) -> bool {
        self.protocol_settled
    }

    /// Returns whether the real Root preferences modal is currently mounted.
    pub fn modal_open(&self) -> bool {
        self.modal_open
    }

    /// Returns the generation used by Root's focus and delayed-close guards.
    pub fn modal_generation(&self) -> u64 {
        self.modal_generation
    }

    /// Returns whether both Root and the PreferencesPanel have completed their initial settings read.
    pub fn settings_ready(&self) -> bool {
        self.settings_ready
    }

    /// Returns whether the panel still has an initial settings read in flight.
    pub fn settings_loading(&self) -> bool {
        self.settings_loading
    }

    /// Distinguishes a stable editable baseline from a pending or uncertain write.
    pub fn draft_ready(&self) -> bool {
        self.draft_ready
    }

    /// Returns whether the typed settings draft differs from its confirmed revision.
    pub fn draft_dirty(&self) -> bool {
        self.draft_dirty
    }

    /// Returns the selected production PreferencesSection as its stable acceptance ID.
    pub fn page_id(&self) -> &'static str {
        self.page_id
    }

    /// Returns the panel's current typed theme selection.
    pub fn draft_theme_mode(&self) -> Option<&'static str> {
        self.draft_theme_mode
    }

    /// Returns the panel's current typed interface density.
    pub fn draft_density(&self) -> Option<&'static str> {
        self.draft_density
    }

    /// Returns the stable language enum stored in the typed settings draft.
    pub fn draft_interface_language(&self) -> Option<&'static str> {
        self.draft_interface_language
    }

    /// Returns the Select's committed enum independently of its backing draft.
    pub fn selected_interface_language(&self) -> Option<&'static str> {
        self.selected_interface_language
    }

    /// Returns the current language Select trigger for verifying its Escape focus restoration.
    pub fn language_select_focus(&self) -> &FocusHandle {
        &self.language_select_focus
    }

    /// Returns expanded-state projections in terminal, SFTP, external, MCP, desktop order.
    pub fn expanded_disclosures(&self) -> [bool; 5] {
        self.expanded_disclosures
    }

    /// Returns the focus target Root captured when opening the settings modal.
    pub fn modal_return_focus(&self) -> Option<&FocusHandle> {
        self.modal_return_focus.as_ref()
    }

    /// Returns the typed keyword-rule count currently shown by the panel.
    pub fn draft_keyword_rule_count(&self) -> Option<usize> {
        self.draft_keyword_rule_count
    }

    /// Reports whether the inline rule editor owns an active local session.
    pub fn keyword_editor_open(&self) -> bool {
        self.keyword_editor_open
    }

    /// Distinguishes a new local rule draft from an edit of a persisted rule.
    pub fn keyword_editor_is_new(&self) -> bool {
        self.keyword_editor_is_new
    }

    /// Returns the stable rule ID of the active editor session.
    pub fn keyword_editor_rule_id(&self) -> Option<&str> {
        self.keyword_editor_rule_id.as_deref()
    }

    /// Returns the current local pattern draft without claiming it has been saved.
    pub fn keyword_editor_pattern(&self) -> Option<&str> {
        self.keyword_editor_pattern.as_deref()
    }

    /// Returns the current local note draft without claiming it has been saved.
    pub fn keyword_editor_note(&self) -> Option<&str> {
        self.keyword_editor_note.as_deref()
    }

    /// Indicates an application settings write that must finish before cancellation is allowed.
    pub fn keyword_save_pending(&self) -> bool {
        self.keyword_save_pending
    }
}

impl RootView {
    /// 只读投影当前正文的实际裁切区域，验收仍使用真实滚轮而不写入滚动位置。
    pub fn settings_acceptance_content_bounds(
        &self,
        cx: &Context<Self>,
    ) -> gpui_kit::Bounds<gpui_kit::Pixels> {
        self.preferences_panel
            .read(cx)
            .settings_acceptance_content_bounds()
    }

    /// Reads modal, draft and sync lifecycle without replacing production state or service results.
    pub fn settings_acceptance_state(&self, cx: &Context<Self>) -> SettingsAcceptanceState {
        let panel = self.preferences_panel.read(cx);
        let settings = self.settings_snapshot.as_ref();
        let draft_ready = panel.settings_acceptance_draft_ready();
        let keyword_editor = panel.settings_acceptance_keyword_editor();
        SettingsAcceptanceState {
            modal_open: self.preferences_modal_open,
            protocol_settled: panel.settings_acceptance_protocol_settled(),
            sync_ready: panel.settings_acceptance_sync_ready(),
            sync_busy: panel.settings_acceptance_sync_busy(),
            sync_result: panel.settings_acceptance_sync_result(),
            sync_error: panel.settings_acceptance_sync_error(),
            modal_generation: self.preferences_modal_generation,
            settings_ready: settings.is_some() && draft_ready,
            settings_loading: panel.settings_acceptance_loading(),
            draft_ready,
            draft_dirty: panel.settings_acceptance_draft_dirty(),
            page_id: panel.settings_acceptance_page_id(),
            draft_theme_mode: panel.settings_acceptance_draft_theme(),
            draft_density: panel.settings_acceptance_draft_density(),
            draft_interface_language: panel.settings_acceptance_draft_language(),
            selected_interface_language: panel.settings_acceptance_selected_language(cx),
            language_select_focus: panel.settings_acceptance_language_focus(cx),
            draft_keyword_rule_count: panel.settings_acceptance_draft_keyword_rule_count(),
            expanded_disclosures: panel.settings_acceptance_disclosures(),
            modal_return_focus: self.preferences_modal_return_focus.clone(),
            keyword_editor_open: keyword_editor.is_some(),
            keyword_editor_is_new: keyword_editor.as_ref().is_some_and(|editor| editor.is_new),
            keyword_editor_rule_id: keyword_editor.as_ref().map(|editor| editor.rule_id.clone()),
            keyword_editor_pattern: keyword_editor.as_ref().map(|editor| editor.pattern.clone()),
            keyword_editor_note: keyword_editor.as_ref().map(|editor| editor.note.clone()),
            keyword_save_pending: keyword_editor
                .as_ref()
                .is_some_and(|editor| editor.save_pending),
        }
    }
}
