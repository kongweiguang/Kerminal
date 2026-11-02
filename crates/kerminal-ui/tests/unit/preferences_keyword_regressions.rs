//! 关键词规则滚动、草稿身份和保存回执的生产界面回归。
//!
//! @author kongweiguang

use super::*;
use gpui_kit::{Context, IntoElement, Render, Window, base::ElementExt as _, div, prelude::*};
use kerminal_application::preferences::{InterfaceDensity, TerminalKeywordHighlightRule};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{cell::Cell, rc::Rc};

#[derive(Clone)]
struct FailOnceSettingsRepository {
    backing: MemorySettingsRepository,
    fail_next: Arc<AtomicBool>,
}

#[async_trait]
impl SettingsRepository for FailOnceSettingsRepository {
    /// 读取沿用同一内存仓储，使重试验证保持真实 revision。
    ///
    /// @author kongweiguang
    async fn load_settings(&self) -> AppResult<VersionedSettings> {
        self.backing.load_settings().await
    }

    /// 首次明确失败不写入，后续调用仍经过正常仓储 CAS。
    ///
    /// @author kongweiguang
    async fn save_settings(
        &self,
        settings: SettingsSnapshot,
        revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedSettings> {
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(AppError::infrastructure("测试中的设置保存失败"));
        }
        self.backing.save_settings(settings, revision).await
    }
}

#[async_trait]
impl ConfigurationRepository for FailOnceSettingsRepository {
    /// 将恢复状态交给底层仓储，失败注入只作用于一次设置保存。
    ///
    /// @author kongweiguang
    fn recovery_state(&self) -> ConfigurationRecoveryState {
        self.backing.recovery_state()
    }

    /// 关键词测试不读取原始配置文档。
    ///
    /// @author kongweiguang
    async fn read(&self, path: ConfigDocumentPath) -> AppResult<Option<ConfigDocument>> {
        self.backing.read(path).await
    }

    /// 关键词测试不通过原始文档接口写入。
    ///
    /// @author kongweiguang
    async fn write(
        &self,
        path: ConfigDocumentPath,
        source: String,
        revision: Option<ConfigRevision>,
    ) -> AppResult<ConfigRevision> {
        self.backing.write(path, source, revision).await
    }
}

/// 复用真实面板与内存仓储，只在设置保存边界注入一次可重试失败。
///
/// @author kongweiguang
fn fixture_with_save_failure(
    app: &mut TestAppContext,
) -> (
    Runtime,
    Entity<PreferencesPanel>,
    MemorySettingsRepository,
    Arc<AtomicBool>,
    &mut VisualTestContext,
) {
    app.update(crate::init);
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let repository = MemorySettingsRepository::new();
    let fail_next = Arc::new(AtomicBool::new(false));
    let preferences = PreferencesService::new(Arc::new(FailOnceSettingsRepository {
        backing: repository.clone(),
        fail_next: fail_next.clone(),
    }));
    let profiles = ProfileService::new(Arc::new(repository.clone()), Arc::new(TestDefaultShell));
    let services = PreferencesPanelServices {
        preferences,
        profiles,
        vault_keys: None,
        configuration_sync: None,
        configuration_changes: None,
        external_protocol: None,
        configuration_directory: None,
        mcp_runtime_available: false,
        mcp_runtime: None,
        executor,
    };
    let mounted = std::rc::Rc::new(std::cell::RefCell::new(None));
    let mounted_panel = mounted.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let panel = cx.new(|cx| PreferencesPanel::new(services, window, cx));
        *mounted_panel.borrow_mut() = Some(panel.clone());
        gpui_kit::base::Root::new(panel, window, cx)
    });
    let panel = mounted.borrow_mut().take().unwrap();
    pump_runtime(&runtime, visual);
    (runtime, panel, repository, fail_next, visual)
}

/// 用规则内容查找稳定身份，避免把列表顺序误当成用户选择的对象。
///
/// @author kongweiguang
fn saved_keyword_id(rules: &[TerminalKeywordHighlightRule], pattern: &str) -> String {
    rules
        .iter()
        .find(|rule| rule.pattern == pattern)
        .unwrap_or_else(|| panic!("saved keyword rule {pattern:?} is missing"))
        .id
        .clone()
}

/// 输入和选择必须同时落在预期身份及草稿上，避免后续断言掩盖未命中的 UI 操作。
///
/// @author kongweiguang
fn assert_keyword_session(
    visual: &mut VisualTestContext,
    panel: &Entity<PreferencesPanel>,
    rule_id: &str,
    pattern: &str,
) {
    let (actual_id, actual_pattern) = visual.update(|_, cx| {
        let session = panel.read(cx).keyword_editor.session.as_ref().unwrap();
        (session.draft.id.clone(), session.draft.pattern.clone())
    });
    assert_eq!(
        actual_id, rule_id,
        "keyword selection did not reach its target"
    );
    assert_eq!(
        actual_pattern, pattern,
        "unexpected draft for keyword {rule_id}"
    );
}

/// 按真实规则按钮切换后立即核对身份，确保竞态测试不会把未命中当成回执覆盖。
///
/// @author kongweiguang
fn select_saved_keyword(
    visual: &mut VisualTestContext,
    panel: &Entity<PreferencesPanel>,
    rule_id: &str,
    pattern: &str,
    delta_y: f32,
) {
    let selector = format!("keyword-rule-{rule_id}");
    scroll_until_visible(visual, &selector, "keyword-rule-list-viewport", delta_y);
    click(visual, &selector);
    assert_keyword_session(visual, panel, rule_id, pattern);
}

/// 已保存规则间切换必须保留各自的编辑内容，也不能把草稿提前写入仓储。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_rule_switch_preserves_drafts_by_identity(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-keywords");
    create_keyword_rule(&runtime, visual, "first-rule", "first-note", None);
    create_keyword_rule(
        &runtime,
        visual,
        "second-rule",
        "second-note",
        Some("keyword-save"),
    );
    let saved = repository.snapshot().0.terminal.keyword_highlights.rules;
    let first = saved_keyword_id(&saved, "first-rule");
    let second = saved_keyword_id(&saved, "second-rule");

    select_saved_keyword(visual, &panel, &first, "first-rule", 8.);
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "first-draft",
    );
    assert_keyword_session(visual, &panel, &first, "first-draft");
    select_saved_keyword(visual, &panel, &second, "second-rule", -8.);
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "second-draft",
    );
    assert_keyword_session(visual, &panel, &second, "second-draft");
    select_saved_keyword(visual, &panel, &first, "first-draft", 8.);
    assert_eq!(
        repository.snapshot().0.terminal.keyword_highlights.rules,
        saved
    );
    select_saved_keyword(visual, &panel, &second, "second-draft", -8.);
}

/// 新规则切走后仍由原 ID 恢复，避免重复点击新建产生不可找回的草稿。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_new_rule_draft_resumes_after_switch(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-keywords");
    create_keyword_rule(&runtime, visual, "existing-rule", "", None);
    let existing_id = repository.snapshot().0.terminal.keyword_highlights.rules[0]
        .id
        .clone();
    click(visual, "keyword-new");
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "new-rule-draft",
    );
    let draft_id = visual.update(|_, cx| {
        panel
            .read(cx)
            .keyword_editor
            .session
            .as_ref()
            .unwrap()
            .draft
            .id
            .clone()
    });
    click(visual, &format!("keyword-rule-{existing_id}"));
    click(visual, "keyword-new");
    let (current_id, current_pattern) = visual.update(|_, cx| {
        let session = panel.read(cx).keyword_editor.session.as_ref().unwrap();
        (session.draft.id.clone(), session.draft.pattern.clone())
    });
    assert_eq!(current_id, draft_id);
    assert_eq!(current_pattern, "new-rule-draft");
}

/// 保存回执可能在切换规则后到达，必须只更新发起保存的会话。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_save_receipt_does_not_replace_switched_session(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-keywords");
    create_keyword_rule(&runtime, visual, "pending-first", "", None);
    create_keyword_rule(&runtime, visual, "pending-second", "", Some("keyword-save"));
    let rules = repository.snapshot().0.terminal.keyword_highlights.rules;
    let first = saved_keyword_id(&rules, "pending-first");
    let second = saved_keyword_id(&rules, "pending-second");
    select_saved_keyword(visual, &panel, &first, "pending-first", 8.);
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "saved-first",
    );
    assert_keyword_session(visual, &panel, &first, "saved-first");
    click(visual, "keyword-save");
    assert!(visual.update(|_, cx| panel.read(cx).keyword_editor.save_pending.is_some()));
    select_saved_keyword(visual, &panel, &second, "pending-second", -8.);
    assert!(visual.update(|_, cx| panel.read(cx).keyword_editor.save_pending.is_some()));
    pump_runtime(&runtime, visual);

    assert_keyword_session(visual, &panel, &second, "pending-second");
    assert_eq!(
        repository
            .snapshot()
            .0
            .terminal
            .keyword_highlights
            .rules
            .iter()
            .find(|rule| rule.id == first)
            .unwrap()
            .pattern,
        "saved-first"
    );
    select_saved_keyword(visual, &panel, &first, "saved-first", 8.);
}

/// 外部更新同一条规则后拒绝覆盖，用户草稿必须仍可见。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_external_change_keeps_draft_without_overwrite(app: &mut TestAppContext) {
    let (runtime, panel, _repository, visual) = fixture(app);
    click(visual, "preferences-nav-keywords");
    create_keyword_rule(&runtime, visual, "external-rule", "", None);
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "local-draft",
    );
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.edit_settings(
                |settings| {
                    settings.terminal.keyword_highlights.rules[0].note =
                        "external-update".to_owned()
                },
                cx,
            );
        })
    });
    click(visual, "keyword-save");
    let (draft, error, pending) = visual.update(|_, cx| {
        let editor = &panel.read(cx).keyword_editor;
        (
            editor.session.as_ref().unwrap().draft.pattern.clone(),
            editor.error.clone(),
            editor.save_pending.is_some(),
        )
    });
    assert_eq!(draft, "local-draft");
    assert!(error.is_some());
    assert!(!pending);
}

/// 一次已知失败后的重试复用原规则 ID，不插入重复规则或丢弃修改。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_failed_new_rule_save_can_retry_without_duplicate(app: &mut TestAppContext) {
    let (runtime, panel, repository, fail_next, visual) = fixture_with_save_failure(app);
    click(visual, "preferences-nav-keywords");
    click(visual, "keyword-new");
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "first-attempt",
    );
    let draft_id = visual.update(|_, cx| {
        panel
            .read(cx)
            .keyword_editor
            .session
            .as_ref()
            .unwrap()
            .draft
            .id
            .clone()
    });
    fail_next.store(true, Ordering::SeqCst);
    click(visual, "keyword-save");
    pump_runtime(&runtime, visual);
    assert!(
        repository
            .snapshot()
            .0
            .terminal
            .keyword_highlights
            .rules
            .is_empty()
    );
    assert!(visual.update(|_, cx| panel.read(cx).keyword_editor.error.is_some()));
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "retried-rule",
    );
    click(visual, "keyword-save");
    pump_runtime(&runtime, visual);
    let saved = repository.snapshot().0.terminal.keyword_highlights.rules;
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].id, draft_id);
    assert_eq!(saved[0].pattern, "retried-rule");
}

/// 放弃失败的新规则时清除未确认的设置投影，避免后续普通自动保存意外提交。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_failed_new_rule_cancel_does_not_persist(app: &mut TestAppContext) {
    let (runtime, panel, repository, fail_next, visual) = fixture_with_save_failure(app);
    click(visual, "preferences-nav-keywords");
    click(visual, "keyword-new");
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "cancel-after-failure",
    );
    fail_next.store(true, Ordering::SeqCst);
    click(visual, "keyword-save");
    pump_runtime(&runtime, visual);
    click(visual, "keyword-cancel");
    pump_runtime(&runtime, visual);
    assert!(visual.update(|_, cx| {
        panel
            .read(cx)
            .settings_snapshot()
            .unwrap()
            .terminal
            .keyword_highlights
            .rules
            .is_empty()
    }));
    assert!(
        repository
            .snapshot()
            .0
            .terminal
            .keyword_highlights
            .rules
            .is_empty()
    );
}

/// 外部字段导致 CAS 冲突时，失败规则只能留在局部草稿，不能借合并自动重提。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_conflict_does_not_resubmit_before_explicit_retry(app: &mut TestAppContext) {
    use kerminal_application::preferences::ThemeMode;

    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-keywords");
    click(visual, "keyword-new");
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "conflicted-rule",
    );

    let mut external = repository.snapshot().0;
    external.theme_mode = if external.theme_mode == ThemeMode::Dark {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    };
    runtime
        .block_on(repository.save_settings(external.clone(), None))
        .unwrap();
    click(visual, "keyword-save");
    pump_runtime(&runtime, visual);

    let (stored, saves) = repository.snapshot();
    assert_eq!(saves, 1, "a rejected keyword must not be replayed");
    assert_eq!(stored.theme_mode, external.theme_mode);
    assert!(stored.terminal.keyword_highlights.rules.is_empty());
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        let session = panel.keyword_editor.session.as_ref().unwrap();
        assert!(session.is_new);
        assert_eq!(session.draft.pattern, "conflicted-rule");
        assert!(panel.keyword_editor.error.is_some());
        assert!(
            panel
                .settings_snapshot()
                .unwrap()
                .terminal
                .keyword_highlights
                .rules
                .is_empty()
        );
    });

    click(visual, "keyword-save");
    pump_runtime(&runtime, visual);
    let (stored, saves) = repository.snapshot();
    assert_eq!(saves, 2);
    assert_eq!(stored.theme_mode, external.theme_mode);
    assert_eq!(stored.terminal.keyword_highlights.rules.len(), 1);
    assert_eq!(
        stored.terminal.keyword_highlights.rules[0].pattern,
        "conflicted-rule"
    );
}

/// 达到上限后仍可恢复已有新草稿，但保存必须等待腾出容量。
/// 本用例只验证容量与草稿身份，冻结撤销夹具期限，避免 debug 的 64 行布局耗时冒充产品到期行为。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_parked_new_rule_remains_accessible_at_limit(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-keywords");
    let rules = (0..64)
        .map(|index| TerminalKeywordHighlightRule::new(format!("limit-{index:02}"), ""))
        .collect::<Vec<_>>();
    let selected = format!("keyword-rule-{}", rules[0].id);
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.edit_settings(
                |settings| settings.terminal.keyword_highlights.rules = rules,
                cx,
            );
        })
    });
    pump_runtime(&runtime, visual);
    click(visual, &selected);
    click(visual, "keyword-delete");
    visual.update(|_, cx| {
        panel.update(cx, |panel, _| {
            panel
                .keyword_editor
                .undo_receipt
                .as_mut()
                .expect("delete must offer an undo receipt")
                .expires_at = std::time::Instant::now() + std::time::Duration::from_secs(300);
        });
    });
    click(visual, "keyword-new");
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "unsaved-at-limit",
    );
    click(visual, "keyword-undo-delete");
    assert!(visual.update(|_, cx| panel.read(cx).keyword_editor.has_parked_new()));
    click(visual, "keyword-new");
    visual.update(|_, cx| {
        let session = panel.read(cx).keyword_editor.session.as_ref().unwrap();
        assert!(session.is_new);
        assert_eq!(session.draft.pattern, "unsaved-at-limit");
    });
    click(visual, "keyword-save");
    assert!(visual.update(|_, cx| panel.read(cx).keyword_editor.error.is_some()));
    click(visual, "keyword-cancel");
    pump_runtime(&runtime, visual);
    assert_eq!(
        repository
            .snapshot()
            .0
            .terminal
            .keyword_highlights
            .rules
            .len(),
        64
    );
    assert!(!visual.update(|_, cx| panel.read(cx).keyword_editor.has_parked_new()));
}

/// 切换设置分组保留新建和已有规则的独立草稿，不提前写入仓储。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_drafts_survive_settings_section_navigation(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-keywords");
    create_keyword_rule(&runtime, visual, "saved-rule", "", None);
    let saved_id = repository.snapshot().0.terminal.keyword_highlights.rules[0]
        .id
        .clone();
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "edited-existing",
    );
    click(visual, "keyword-new");
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "edited-new",
    );
    click(visual, "preferences-nav-terminal");
    click(visual, "preferences-nav-keywords");
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .keyword_editor
            .session
            .as_ref()
            .unwrap()
            .draft
            .pattern
            .clone()),
        "edited-new"
    );
    click(visual, &format!("keyword-rule-{saved_id}"));
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .keyword_editor
            .session
            .as_ref()
            .unwrap()
            .draft
            .pattern
            .clone()),
        "edited-existing"
    );
    click(visual, "keyword-new");
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .keyword_editor
            .session
            .as_ref()
            .unwrap()
            .draft
            .pattern
            .clone()),
        "edited-new"
    );
    assert_eq!(
        repository.snapshot().0.terminal.keyword_highlights.rules[0].pattern,
        "saved-rule"
    );
}

/// 关闭设置必须确认放弃局部草稿，继续编辑保留输入，确认后才清空。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_close_confirmation_preserves_or_discards_draft(app: &mut TestAppContext) {
    let (_runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-keywords");
    click(visual, "keyword-new");
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "close-draft",
    );
    click(visual, "preferences-close");
    assert_rendered(visual, "preferences-close-cancel");
    click(visual, "preferences-close-cancel");
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .keyword_editor
            .session
            .as_ref()
            .unwrap()
            .draft
            .pattern
            .clone()),
        "close-draft"
    );
    click(visual, "preferences-close");
    click(visual, "preferences-close-discard");
    assert!(visual.update(|_, cx| panel.read(cx).keyword_editor.session.is_none()));
    assert!(
        repository
            .snapshot()
            .0
            .terminal
            .keyword_highlights
            .rules
            .is_empty()
    );
}

/// 测试窗口没有平台帧循环，显式交付一次下一帧队列并绘制结果。
///
/// @author kongweiguang
fn deliver_confirmation_frame(visual: &mut VisualTestContext) {
    visual.run_until_parked();
    visual.update(|window, cx| {
        assert!(
            window.simulate_next_frame(cx) > 0,
            "opening the confirmation must schedule a frame callback"
        );
        window.draw(cx).clear(cx);
    });
    visual.run_until_parked();
}

/// 确认层只在两个真实按钮之间循环 Tab，保证键盘不会触及遮罩后的表单。
///
/// @author kongweiguang
fn assert_leave_confirmation_tab_cycle(
    visual: &mut VisualTestContext,
    keep: &gpui_kit::FocusHandle,
    discard: &gpui_kit::FocusHandle,
) {
    assert!(visual.update(|window, _| keep.is_focused(window)));
    for (key, expected) in [
        ("tab", discard),
        ("tab", keep),
        ("shift-tab", discard),
        ("shift-tab", keep),
    ] {
        visual.simulate_keystrokes(key);
        draw(visual);
        assert!(
            visual.update(|window, _| expected.is_focused(window)),
            "{key}"
        );
    }
}

/// 关闭确认使用独立焦点陷阱；Escape 保留规则草稿并恢复原输入焦点。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_close_confirmation_traps_focus_and_escape_restores_input(app: &mut TestAppContext) {
    let (_runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-keywords");
    click(visual, "keyword-new");
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "close-focus-draft",
    );
    let previous = visual.update(|window, cx| window.focused(cx)).unwrap();
    visual.update(|_, cx| panel.update(cx, |panel, cx| panel.request_close(cx)));
    deliver_confirmation_frame(visual);
    let (keep, discard) = visual.update(|_, cx| {
        let focus = &panel.read(cx).close_confirmation_focus;
        (focus.keep_editing.clone(), focus.discard.clone())
    });
    assert_leave_confirmation_tab_cycle(visual, &keep, &discard);
    visual.simulate_keystrokes("escape");
    draw(visual);
    assert!(!visual.update(|_, cx| panel.read(cx).close_confirmation));
    assert_eq!(
        visual.update(|window, cx| window.focused(cx)),
        Some(previous)
    );
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .keyword_editor
            .session
            .as_ref()
            .unwrap()
            .draft
            .pattern
            .clone()),
        "close-focus-draft"
    );
    assert!(
        repository
            .snapshot()
            .0
            .terminal
            .keyword_highlights
            .rules
            .is_empty()
    );
}

/// 延迟的设置初始焦点不能覆盖退出确认，取消后回到原输入并保留草稿。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_quit_confirmation_traps_focus_and_escape_restores_input(app: &mut TestAppContext) {
    let (_runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-keywords");
    click(visual, "keyword-new");
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "quit-focus-draft",
    );
    let previous = visual.update(|window, cx| window.focused(cx)).unwrap();
    assert!(!visual.update(|_, cx| panel.update(cx, |panel, cx| panel.request_quit(cx))));
    deliver_confirmation_frame(visual);
    visual.update(|_, cx| panel.update(cx, |panel, cx| panel.request_close(cx)));
    assert!(!visual.update(|_, cx| panel.read(cx).close_confirmation));
    let (keep, discard) = visual.update(|_, cx| {
        let focus = &panel.read(cx).quit_confirmation_focus;
        (focus.keep_editing.clone(), focus.discard.clone())
    });
    assert_leave_confirmation_tab_cycle(visual, &keep, &discard);
    visual.simulate_keystrokes("tab");
    draw(visual);
    assert!(visual.update(|window, _| discard.is_focused(window)));
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.focus_initial_control(window, cx));
    });
    assert!(visual.update(|window, _| discard.is_focused(window)));
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.focus_navigation_search(window, cx));
    });
    assert!(visual.update(|window, _| discard.is_focused(window)));
    visual.simulate_keystrokes("escape");
    draw(visual);
    assert!(!visual.update(|_, cx| panel.read(cx).quit_confirmation));
    assert_eq!(
        visual.update(|window, cx| window.focused(cx)),
        Some(previous)
    );
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .keyword_editor
            .session
            .as_ref()
            .unwrap()
            .draft
            .pattern
            .clone()),
        "quit-focus-draft"
    );
    assert!(
        repository
            .snapshot()
            .0
            .terminal
            .keyword_highlights
            .rules
            .is_empty()
    );
}

/// 密集规则列表的焦点目标超过一百个时，关闭和退出确认仍必须双向循环并保留取消后的草稿。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_dense_list_keeps_close_and_quit_confirmation_focus(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-keywords");
    let rules = (0..64)
        .map(|index| TerminalKeywordHighlightRule::new(format!("focus-{index:02}"), ""))
        .collect::<Vec<_>>();
    let first = format!("keyword-rule-{}", rules[0].id);
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.edit_settings(
                |settings| settings.terminal.keyword_highlights.rules = rules,
                cx,
            );
        });
    });
    pump_runtime(&runtime, visual);
    click(visual, &first);
    replace_keyword_field(
        visual,
        "keyword-note-field",
        "keyword-rule-editor-viewport",
        8.,
        "dense-focus-draft",
    );
    let observed_targets = visual.update(|window, _| {
        gpui_kit::base::test_support::snapshots(window)
            .iter()
            .filter(|element| element.tracked_focus_id().is_some())
            .count()
    });
    assert!(
        observed_targets > 100,
        "dense focus fixture has only {observed_targets} targets"
    );

    click(visual, "preferences-close");
    deliver_confirmation_frame(visual);
    let (keep, discard) = visual.update(|_, cx| {
        let focus = &panel.read(cx).close_confirmation_focus;
        (focus.keep_editing.clone(), focus.discard.clone())
    });
    for _ in 0..3 {
        assert_leave_confirmation_tab_cycle(visual, &keep, &discard);
    }
    visual.simulate_keystrokes("escape");
    draw(visual);
    assert!(!visual.update(|_, cx| panel.read(cx).close_confirmation));

    assert!(!visual.update(|_, cx| panel.update(cx, |panel, cx| panel.request_quit(cx))));
    deliver_confirmation_frame(visual);
    let (keep, discard) = visual.update(|_, cx| {
        let focus = &panel.read(cx).quit_confirmation_focus;
        (focus.keep_editing.clone(), focus.discard.clone())
    });
    for _ in 0..3 {
        assert_leave_confirmation_tab_cycle(visual, &keep, &discard);
    }
    visual.simulate_keystrokes("escape");
    draw(visual);
    assert!(!visual.update(|_, cx| panel.read(cx).quit_confirmation));
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .keyword_editor
            .session
            .as_ref()
            .unwrap()
            .draft
            .note
            .clone()),
        "dense-focus-draft"
    );
    let saved = repository.snapshot().0.terminal.keyword_highlights.rules;
    assert_eq!(saved.len(), 64);
    assert_eq!(saved[0].note, "");
}

struct NarrowKeywordFixtureView {
    panel: Entity<PreferencesPanel>,
    painted_line_height: Rc<Cell<Pixels>>,
}

impl Render for NarrowKeywordFixtureView {
    /// 复用生产工作区的 14px 继承字号，使行滚轮回归能暴露绘制期与事件期样式不同的问题。
    ///
    /// @author kongweiguang
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let painted_line_height = self.painted_line_height.clone();
        div()
            .size_full()
            .text_size(px(14.))
            .on_prepaint(move |_, window, _| {
                painted_line_height.set(window.line_height());
            })
            .child(self.panel.clone())
    }
}

/// 单独构造继承生产字号的隔离面板，不改变其他设置回归的默认窗口和样式。
///
/// @author kongweiguang
fn narrow_keyword_fixture(
    app: &mut TestAppContext,
) -> (
    Runtime,
    Entity<PreferencesPanel>,
    MemorySettingsRepository,
    Rc<Cell<Pixels>>,
    &mut VisualTestContext,
) {
    app.update(crate::init);
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let repository = MemorySettingsRepository::new();
    let services = PreferencesPanelServices {
        preferences: PreferencesService::new(Arc::new(repository.clone())),
        profiles: ProfileService::new(Arc::new(repository.clone()), Arc::new(TestDefaultShell)),
        vault_keys: None,
        configuration_sync: None,
        configuration_changes: None,
        external_protocol: None,
        configuration_directory: None,
        mcp_runtime_available: false,
        mcp_runtime: None,
        executor,
    };
    let mounted = Rc::new(std::cell::RefCell::new(None));
    let mounted_panel = mounted.clone();
    let painted_line_height = Rc::new(Cell::new(px(0.)));
    let view_line_height = painted_line_height.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let panel = cx.new(|cx| PreferencesPanel::new(services, window, cx));
        *mounted_panel.borrow_mut() = Some(panel.clone());
        let view = cx.new(|_| NarrowKeywordFixtureView {
            panel,
            painted_line_height: view_line_height,
        });
        gpui_kit::base::Root::new(view, window, cx)
    });
    let panel = mounted.borrow_mut().take().unwrap();
    pump_runtime(&runtime, visual);
    (runtime, panel, repository, painted_line_height, visual)
}

/// 800×600 窗口中的弹层扣除边距后，规则列表和编辑器都必须容纳一个完整控件。
/// 只有滚动区自身足够高，用户才有机会通过真实滚轮将输入框或规则行完整露出。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_narrow_window_keeps_scroll_regions_operable(app: &mut TestAppContext) {
    let (runtime, panel, repository, painted_line_height, visual) = narrow_keyword_fixture(app);
    visual.simulate_resize(size(px(768.), px(552.)));
    draw(visual);
    for density in [InterfaceDensity::Spacious, InterfaceDensity::Compact] {
        visual.update(|_, cx| {
            panel.update(cx, |panel, cx| {
                panel.edit_settings(|settings| settings.interface_density = density, cx);
            });
        });
        pump_runtime(&runtime, visual);
    }
    assert_eq!(
        repository.snapshot().0.interface_density,
        InterfaceDensity::Compact
    );
    assert!(painted_line_height.get() > px(0.));
    assert_ne!(
        painted_line_height.get(),
        visual.update(|window, _| window.line_height()),
        "fixture must distinguish inherited paint and default event line heights"
    );
    click(visual, "preferences-nav-keywords");
    let rules = (0..63)
        .map(|index| TerminalKeywordHighlightRule::new(format!("narrow-{index:02}"), "fixture"))
        .collect::<Vec<_>>();
    let first_selector = format!("keyword-rule-{}", rules[0].id);
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.edit_settings(
                |settings| settings.terminal.keyword_highlights.rules = rules,
                cx,
            );
        });
    });
    pump_runtime(&runtime, visual);
    click(visual, "keyword-new");
    visual.update(|window, cx| {
        window.simulate_next_frame(cx);
        window.draw(cx).clear(cx);
    });
    draw(visual);

    let (list, editor, content) = visual.update(|_, cx| {
        let panel = panel.read(cx);
        (
            panel.keyword_editor.list_scroll.bounds(),
            panel.keyword_editor.editor_scroll.bounds(),
            panel.content_scroll.bounds(),
        )
    });
    let rule = target_geometry(visual, &first_selector).0;
    let pattern = target_geometry(visual, "keyword-pattern-field").0;
    let note = target_geometry(visual, "keyword-note-field").0;
    assert!(
        list.size.height >= px(64.) && list.size.height >= rule.size.height,
        "keyword list cannot show a full rule: list={list:?}, rule={rule:?}, content={content:?}"
    );
    assert!(
        editor.size.height >= pattern.size.height && editor.size.height >= note.size.height,
        "keyword editor cannot show a full field: editor={editor:?}, pattern={pattern:?}, note={note:?}, content={content:?}"
    );

    reveal_narrow_keyword_outer(visual, &panel, "keyword-rule-editor-viewport");
    for (selector, value) in [
        ("keyword-pattern-field", "narrow-new-rule"),
        ("keyword-note-field", "narrow note"),
    ] {
        reveal_narrow_keyword_editor_field(visual, &panel, selector);
        click(visual, selector);
        visual.simulate_keystrokes("secondary-a backspace");
        visual.simulate_input(value);
        draw(visual);
    }
    let new_id = visual.update(|_, cx| {
        let session = panel.read(cx).keyword_editor.session.as_ref().unwrap();
        assert_eq!(session.draft.pattern, "narrow-new-rule");
        assert_eq!(session.draft.note, "narrow note");
        session.draft.id.clone()
    });
    reveal_narrow_keyword_outer(visual, &panel, "keyword-rule-editor-footer");
    let save = target_geometry(visual, "keyword-save");
    let content = visual.update(|_, cx| panel.read(cx).content_scroll.bounds());
    assert!(save.1 && keyword_bounds_inside(save.0, content));
    click(visual, "keyword-save");
    pump_runtime(&runtime, visual);
    let saved = repository.snapshot().0.terminal.keyword_highlights.rules;
    assert_eq!(saved.len(), 64);
    assert!(saved.iter().any(|rule| {
        rule.id == new_id && rule.pattern == "narrow-new-rule" && rule.note == "narrow note"
    }));

    reveal_narrow_keyword_outer(visual, &panel, "keyword-rule-list-viewport");
    let before = visual.update(|_, cx| panel.read(cx).content_scroll.offset());
    let list = visual.update(|_, cx| panel.read(cx).keyword_editor.list_scroll.bounds());
    visual.simulate_event(ScrollWheelEvent {
        position: list.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-5_000.))),
        ..Default::default()
    });
    draw(visual);
    let after = visual.update(|_, cx| panel.read(cx).content_scroll.offset());
    assert_eq!(
        before, after,
        "list scrolling moved the outer settings page"
    );
    let last_selector = format!("keyword-rule-{new_id}");
    let last = target_geometry(visual, &last_selector);
    let (content, list) = visual.update(|_, cx| {
        let panel = panel.read(cx);
        (
            panel.content_scroll.bounds(),
            panel.keyword_editor.list_scroll.bounds(),
        )
    });
    assert!(
        last.1 && keyword_bounds_inside(last.0, content.intersect(&list)),
        "saved final rule is clipped: rule={:?}, list={list:?}, content={content:?}",
        last.0
    );
    click(visual, &last_selector);
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .keyword_editor
            .session
            .as_ref()
            .map(|session| session.draft.id.clone())),
        Some(new_id)
    );

    let (outer_before, list_before, outer_max, list) = visual.update(|_, cx| {
        let panel = panel.read(cx);
        (
            panel.content_scroll.offset(),
            panel.keyword_editor.list_scroll.offset(),
            panel.content_scroll.max_offset(),
            panel.keyword_editor.list_scroll.bounds(),
        )
    });
    assert!(
        outer_before.y + outer_max.y > px(12.),
        "outer page needs remaining scroll space to test list edge propagation"
    );
    visual.simulate_event(ScrollWheelEvent {
        position: list.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-12.))),
        ..Default::default()
    });
    draw(visual);
    let (outer_after, list_after) = visual.update(|_, cx| {
        let panel = panel.read(cx);
        (
            panel.content_scroll.offset(),
            panel.keyword_editor.list_scroll.offset(),
        )
    });
    assert_eq!(list_before, list_after, "list is already at the lower edge");
    assert!(
        outer_after.y < outer_before.y,
        "scrolling beyond the list edge must continue the outer page"
    );

    let (outer_before, list_before, position) = visual.update(|_, cx| {
        let panel = panel.read(cx);
        let content = panel.content_scroll.bounds();
        let list = panel.keyword_editor.list_scroll.bounds();
        assert!(!content.intersect(&list).is_empty());
        (
            panel.content_scroll.offset(),
            panel.keyword_editor.list_scroll.offset(),
            content.intersect(&list).center(),
        )
    });
    visual.simulate_event(ScrollWheelEvent {
        position,
        delta: ScrollDelta::Lines(point(0., -3.)),
        ..Default::default()
    });
    draw(visual);
    let (outer_after, list_after) = visual.update(|_, cx| {
        let panel = panel.read(cx);
        (
            panel.content_scroll.offset(),
            panel.keyword_editor.list_scroll.offset(),
        )
    });
    assert_eq!(list_before, list_after, "line scroll is at the list bottom");
    assert!(
        outer_after.y < outer_before.y,
        "line scrolling at the lower edge must continue the outer page"
    );

    let (outer_before, position) = visual.update(|_, cx| {
        let panel = panel.read(cx);
        let clip = panel
            .content_scroll
            .bounds()
            .intersect(&panel.keyword_editor.list_scroll.bounds());
        assert!(!clip.is_empty());
        (panel.content_scroll.offset(), clip.center())
    });
    visual.simulate_event(ScrollWheelEvent {
        position,
        delta: ScrollDelta::Lines(point(0., 1_000.)),
        ..Default::default()
    });
    draw(visual);
    let (outer_after, list_after) = visual.update(|_, cx| {
        let panel = panel.read(cx);
        (
            panel.content_scroll.offset(),
            panel.keyword_editor.list_scroll.offset(),
        )
    });
    assert_eq!(outer_before, outer_after, "line scrolling moved both areas");
    assert_eq!(list_after.y, px(0.), "line scrolling must reach list top");

    let (outer_before, position) = visual.update(|_, cx| {
        let panel = panel.read(cx);
        let clip = panel
            .content_scroll
            .bounds()
            .intersect(&panel.keyword_editor.list_scroll.bounds());
        assert!(!clip.is_empty());
        (panel.content_scroll.offset(), clip.center())
    });
    visual.simulate_event(ScrollWheelEvent {
        position,
        delta: ScrollDelta::Lines(point(0., 1.)),
        ..Default::default()
    });
    draw(visual);
    let (outer_after, list_after) = visual.update(|_, cx| {
        let panel = panel.read(cx);
        (
            panel.content_scroll.offset(),
            panel.keyword_editor.list_scroll.offset(),
        )
    });
    assert_eq!(list_after.y, px(0.), "line scroll is at the list top");
    assert!(
        outer_after.y > outer_before.y,
        "line scrolling at the upper edge must continue the outer page"
    );
}

/// 外层按真实可见边界滚动，将整个子区域或固定页脚放入正文，再进行子区交互。
///
/// @author kongweiguang
fn reveal_narrow_keyword_outer(
    visual: &mut VisualTestContext,
    panel: &Entity<PreferencesPanel>,
    selector: &str,
) {
    for _ in 0..24 {
        draw(visual);
        let (target, content) = visual.update(|window, cx| {
            (
                window.debug_selector_bounds(selector).unwrap(),
                panel.read(cx).content_scroll.bounds(),
            )
        });
        if keyword_bounds_inside(target, content) {
            return;
        }
        let delta = if target.top() < content.top() {
            (content.top() - target.top()).as_f32() + 4.
        } else {
            -((target.bottom() - content.bottom()).as_f32() + 4.)
        };
        visual.simulate_event(ScrollWheelEvent {
            position: point(content.left() + px(2.), content.center().y),
            delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
            ..Default::default()
        });
    }
    panic!("{selector} could not be brought into the narrow settings body");
}

/// 在编辑区内用真实滚轮定位字段，验证子滚动不会同时移动其外层页面。
///
/// @author kongweiguang
fn reveal_narrow_keyword_editor_field(
    visual: &mut VisualTestContext,
    panel: &Entity<PreferencesPanel>,
    selector: &str,
) {
    for _ in 0..32 {
        let target = target_geometry(visual, selector);
        let (content, editor, outer_before) = visual.update(|_, cx| {
            let panel = panel.read(cx);
            (
                panel.content_scroll.bounds(),
                panel.keyword_editor.editor_scroll.bounds(),
                panel.content_scroll.offset(),
            )
        });
        let clip = content.intersect(&editor);
        if target.1 && keyword_bounds_inside(target.0, clip) {
            return;
        }
        assert!(
            clip.size.height >= target.0.size.height,
            "{selector} cannot fit: target={:?}, editor={editor:?}, content={content:?}",
            target.0
        );
        let delta = if target.0.top() < clip.top() {
            (clip.top() - target.0.top()).as_f32() + 4.
        } else {
            -((target.0.bottom() - clip.bottom()).as_f32() + 4.)
        };
        visual.simulate_event(ScrollWheelEvent {
            position: clip.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
            ..Default::default()
        });
        draw(visual);
        let outer_after = visual.update(|_, cx| panel.read(cx).content_scroll.offset());
        assert_eq!(
            outer_before, outer_after,
            "editor scrolling moved the outer settings page while revealing {selector}"
        );
    }
    panic!("{selector} could not be fully revealed in the narrow editor");
}

/// 真实完整可见包含四个边界，部分相交不能作为可操作结果。
///
/// @author kongweiguang
fn keyword_bounds_inside(bounds: Bounds<Pixels>, clip: Bounds<Pixels>) -> bool {
    !bounds.is_empty()
        && bounds.left() >= clip.left()
        && bounds.right() <= clip.right()
        && bounds.top() >= clip.top()
        && bounds.bottom() <= clip.bottom()
}

/// 在确认自身的延迟回调入队前交付旧帧，防止后续焦点修正掩盖旧回调抢焦点。
///
/// @author kongweiguang
fn assert_pending_frame_preserves_confirmation(
    visual: &mut VisualTestContext,
    panel: &Entity<PreferencesPanel>,
    quit: bool,
) {
    visual.update(|window, app| {
        let keep = if quit {
            panel.read(app).quit_confirmation_focus.keep_editing.clone()
        } else {
            panel
                .read(app)
                .close_confirmation_focus
                .keep_editing
                .clone()
        };
        panel.update(app, |panel, cx| {
            if quit {
                assert!(!panel.request_quit(cx));
                assert!(panel.quit_confirmation);
            } else {
                panel.request_close(cx);
                assert!(panel.close_confirmation);
            }
            panel.focus_initial_control(window, cx);
        });
        assert!(keep.is_focused(window));
        assert!(window.simulate_next_frame(app) > 0);
        assert!(
            keep.is_focused(window),
            "a previous frame callback moved focus outside the confirmation"
        );
    });
}

/// 导航仍在普通页面聚焦内容，已排队的导航焦点不得覆盖关闭确认。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_close_confirmation_blocks_deferred_navigation_focus(app: &mut TestAppContext) {
    let (_runtime, panel, _repository, visual) = fixture(app);
    click(visual, "preferences-nav-keywords");
    click(visual, "keyword-new");
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "navigation-focus-draft",
    );

    click(visual, "preferences-nav-appearance");
    let content = visual.update(|_, cx| panel.read(cx).settings_content_focus.clone());
    visual.update(|window, app| {
        assert!(window.simulate_next_frame(app) > 0);
        assert!(content.is_focused(window));
    });

    click(visual, "preferences-nav-appearance");
    assert_pending_frame_preserves_confirmation(visual, &panel, false);
}

/// 录制正常结束仍恢复原焦点，迟到的恢复回调不得覆盖退出确认。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_quit_confirmation_blocks_deferred_shortcut_focus(app: &mut TestAppContext) {
    let (_runtime, panel, _repository, visual) = fixture(app);
    click(visual, "preferences-nav-keywords");
    click(visual, "keyword-new");
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "shortcut-focus-draft",
    );
    click(visual, "preferences-nav-shortcuts");
    let content = visual.update(|_, cx| panel.read(cx).settings_content_focus.clone());
    visual.update(|window, app| {
        assert!(window.simulate_next_frame(app) > 0);
        assert!(content.is_focused(window));
    });

    let platform = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "windows"
    };
    let record = format!("shortcut-record-settings.open-{platform}");
    scroll_until_visible(visual, &record, "settings-shortcut-header", -8.);
    click(visual, &record);
    assert!(visual.update(|_, cx| panel.read(cx).shortcut_capture.is_some()));
    visual.simulate_keystrokes("escape");
    assert!(visual.update(|_, cx| panel.read(cx).shortcut_capture.is_none()));
    visual.update(|window, app| {
        assert!(window.simulate_next_frame(app) > 0);
        assert!(content.is_focused(window));
    });

    click(visual, &record);
    assert!(visual.update(|_, cx| panel.read(cx).shortcut_capture.is_some()));
    visual.simulate_keystrokes("escape");
    assert!(visual.update(|_, cx| panel.read(cx).shortcut_capture.is_none()));
    assert_pending_frame_preserves_confirmation(visual, &panel, true);
}

/// 已提交规则仍等待回执时不误报未提交，关闭后继续保留用于确认的会话。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_submitted_save_survives_close_without_discard_prompt(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-keywords");
    click(visual, "keyword-new");
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "pending-save",
    );
    click(visual, "keyword-save");
    assert!(visual.update(|_, cx| panel.read(cx).keyword_editor.save_pending.is_some()));
    click(visual, "preferences-close");
    assert!(!visual.update(|_, cx| panel.read(cx).close_confirmation));
    assert!(visual.update(|_, cx| panel.read(cx).keyword_editor.session.is_some()));
    pump_runtime(&runtime, visual);
    assert_eq!(
        repository.snapshot().0.terminal.keyword_highlights.rules[0].pattern,
        "pending-save"
    );
    assert!(visual.update(|_, cx| panel.read(cx).keyword_editor.save_pending.is_none()));
}

/// 关闭时只丢弃其他草稿，提交失败后的退出确认和重开仍保留原请求内容。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_close_discards_other_drafts_but_retains_pending_failure(app: &mut TestAppContext) {
    let (runtime, panel, repository, fail_next, visual) = fixture_with_save_failure(app);
    click(visual, "preferences-nav-keywords");
    create_keyword_rule(&runtime, visual, "first", "", None);
    create_keyword_rule(&runtime, visual, "second", "", Some("keyword-save"));
    let rules = repository.snapshot().0.terminal.keyword_highlights.rules;
    let first = saved_keyword_id(&rules, "first");
    let second = saved_keyword_id(&rules, "second");
    select_saved_keyword(visual, &panel, &first, "first", 8.);
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "discard-me",
    );
    assert_keyword_session(visual, &panel, &first, "discard-me");
    select_saved_keyword(visual, &panel, &second, "second", -8.);
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "retain-on-failure",
    );
    assert_keyword_session(visual, &panel, &second, "retain-on-failure");
    fail_next.store(true, Ordering::SeqCst);
    click(visual, "keyword-save");
    assert!(visual.update(|_, cx| panel.read(cx).keyword_editor.save_pending.is_some()));
    click(visual, "preferences-close");
    assert!(
        visual.update(|_, cx| panel.read(cx).close_confirmation),
        "the other unsaved keyword draft must require confirmation"
    );
    click(visual, "preferences-close-discard");
    pump_runtime(&runtime, visual);

    assert!(visual.update(|_, cx| panel.read(cx).keyword_editor.error.is_some()));
    assert_eq!(
        repository.snapshot().0.terminal.keyword_highlights.rules,
        rules
    );
    assert!(!visual.update(|_, cx| panel.update(cx, |panel, cx| panel.request_quit(cx))));
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.begin_modal_session(window, cx));
    });
    assert!(visual.update(|_, cx| panel.read(cx).quit_confirmation));
    assert_keyword_session(visual, &panel, &second, "retain-on-failure");
    draw(visual);
    click(visual, "preferences-quit-cancel");
    click(visual, "preferences-nav-keywords");
    select_saved_keyword(visual, &panel, &first, "first", 8.);
    select_saved_keyword(visual, &panel, &second, "retain-on-failure", -8.);
}

/// 仅暂存的规则草稿也参与退出确认，取消退出不能丢失该草稿。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_parked_draft_requires_quit_confirmation(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-keywords");
    create_keyword_rule(&runtime, visual, "first", "", None);
    create_keyword_rule(&runtime, visual, "second", "", Some("keyword-save"));
    let rules = repository.snapshot().0.terminal.keyword_highlights.rules;
    let first = format!("keyword-rule-{}", rules[0].id);
    let second = format!("keyword-rule-{}", rules[1].id);
    scroll_until_visible(visual, &first, "keyword-rule-list-viewport", 8.);
    click(visual, &first);
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "parked-edit",
    );
    scroll_until_visible(visual, &second, "keyword-rule-list-viewport", -8.);
    click(visual, &second);
    assert!(!visual.update(|_, cx| panel.update(cx, |panel, cx| panel.request_quit(cx))));
    draw(visual);
    click(visual, "preferences-quit-cancel");
    scroll_until_visible(visual, &first, "keyword-rule-list-viewport", 8.);
    click(visual, &first);
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .keyword_editor
            .session
            .as_ref()
            .unwrap()
            .draft
            .pattern
            .clone()),
        "parked-edit"
    );
    assert!(!visual.update(|_, cx| panel.update(cx, |panel, cx| panel.request_quit(cx))));
    draw(visual);
    click(visual, "preferences-quit-confirm");
    assert_eq!(
        repository.snapshot().0.terminal.keyword_highlights.rules,
        rules
    );
}

/// 撤销删除既恢复已保存规则，也恢复删除之前的未保存草稿。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_delete_undo_restores_edited_draft(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-keywords");
    create_keyword_rule(&runtime, visual, "deleted-rule", "", None);
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-rule-editor-viewport",
        8.,
        "unsaved-draft",
    );
    click(visual, "keyword-delete");
    click(visual, "keyword-undo-delete");
    let current = visual.update(|_, cx| {
        panel
            .read(cx)
            .keyword_editor
            .session
            .as_ref()
            .unwrap()
            .draft
            .pattern
            .clone()
    });
    assert_eq!(current, "unsaved-draft");
    pump_runtime(&runtime, visual);
    assert_eq!(
        repository.snapshot().0.terminal.keyword_highlights.rules[0].pattern,
        "deleted-rule"
    );
}

/// 在生产字号、三种主题和宽窄布局中检查真实摘要几何，防止两行内容被单行按钮裁切。
/// 同时覆盖单条与满列表、末行选择和操作区分离，不以按钮可点击代替内容完整可见。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_rule_summary_fits_button_across_themes_and_list_sizes(app: &mut TestAppContext) {
    use kerminal_application::preferences::ThemeMode;

    let (runtime, panel, _repository, _line_height, visual) = narrow_keyword_fixture(app);
    click(visual, "preferences-nav-keywords");
    for window_size in [size(px(1360.), px(850.)), size(px(768.), px(552.))] {
        visual.simulate_resize(window_size);
        for mode in [ThemeMode::Light, ThemeMode::Dark, ThemeMode::System] {
            for count in [1, 64] {
                let rules = (0..count)
                    .map(|index| {
                        TerminalKeywordHighlightRule::new(
                            format!("规则-{index:02}-{}", "long-pattern-".repeat(8)),
                            "用于核对长备注的省略与完整两行布局".repeat(8),
                        )
                    })
                    .collect::<Vec<_>>();
                let first = rules[0].clone();
                let last = rules.last().unwrap().clone();
                visual.update(|window, cx| {
                    panel.update(cx, |panel, cx| {
                        panel
                            .keyword_editor
                            .list_scroll
                            .set_offset(point(px(0.), px(0.)));
                        panel.content_scroll.set_offset(point(px(0.), px(0.)));
                        panel.edit_settings(
                            |settings| {
                                settings.theme_mode = mode;
                                settings.terminal.keyword_highlights.rules = rules;
                            },
                            cx,
                        );
                        crate::theme::apply_theme_mode(mode, window.appearance(), cx);
                    });
                });
                pump_runtime(&runtime, visual);
                for rule in [&first, &last] {
                    let selector = format!("keyword-rule-{}", rule.id);
                    scroll_until_visible(visual, &selector, "keyword-rule-list-viewport", -20.);
                    let button = target_geometry(visual, &selector).0;
                    let summary = visual.update(|window, _| {
                        window
                            .debug_selector_bounds(&format!("keyword-rule-summary-{}", rule.id))
                            .expect("keyword summary must be laid out")
                    });
                    assert!(
                        keyword_bounds_inside(summary, button),
                        "two-line summary {summary:?} must fit button {button:?}: {mode:?}, {window_size:?}, {count} rules"
                    );
                    let toggle =
                        target_geometry(visual, &format!("keyword-rule-enabled-{}", rule.id)).0;
                    assert!(
                        button.right() <= toggle.left(),
                        "summary must leave room for actions"
                    );
                    click(visual, &selector);
                    assert_keyword_session(visual, &panel, &rule.id, &rule.pattern);
                }
            }
        }
    }
}

/// 64 条规则通过独立列表滚动到末尾，选择后固定保存区仍在设置视口内。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_rule_list_scrolls_to_last_without_hiding_save(app: &mut TestAppContext) {
    let (_runtime, panel, _repository, visual) = fixture(app);
    click(visual, "preferences-nav-keywords");
    let rules = (0..64)
        .map(|index| TerminalKeywordHighlightRule::new(format!("rule-{index:02}"), ""))
        .collect::<Vec<_>>();
    let last = format!("keyword-rule-{}", rules.last().unwrap().id);
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.edit_settings(
                |settings| settings.terminal.keyword_highlights.rules = rules,
                cx,
            );
        })
    });
    draw(visual);
    let list_bounds = visual.update(|_, cx| panel.read(cx).keyword_editor.list_scroll.bounds());
    assert!(list_bounds.size.height > px(0.));
    for _ in 0..20 {
        visual.simulate_event(ScrollWheelEvent {
            position: list_bounds.center(),
            delta: ScrollDelta::Lines(point(0., -20.)),
            ..Default::default()
        });
        draw(visual);
    }
    click(visual, &last);
    let save = target_geometry(visual, "keyword-save").0;
    let content = visual.update(|_, cx| panel.read(cx).content_scroll.bounds());
    assert!(
        bounds_within_viewport(save, content),
        "save button must remain inside settings viewport"
    );
}
