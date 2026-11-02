//! 关键词多规则滚动、草稿和固定保存区的隔离原生验收。
//!
//! @author kongweiguang

use super::*;

const FIXTURE_RULE_COUNT: usize = 63;
const NEW_PATTERN: &str = "settings-parity-keyword";
const NEW_NOTE: &str = "isolated acceptance";
const PARKED_NOTE: &str = "unsaved fixture note";

/// 在私有配置根中一次性建立 63 条已保存规则，为最后一条真实新增留出空间。
///
/// @author kongweiguang
pub(super) async fn seed_rules(
    services: &kerminal_ui::AppServices,
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut snapshot = load_persisted_settings(services).await?;
    if !snapshot
        .settings
        .terminal
        .keyword_highlights
        .rules
        .is_empty()
    {
        return Err(
            io::Error::other("keyword fixture requires an empty isolated rule list").into(),
        );
    }
    let rules = (0..FIXTURE_RULE_COUNT)
        .map(|index| {
            TerminalKeywordHighlightRule::new(format!("fixture-rule-{index:02}"), "fixture")
        })
        .collect::<Vec<_>>();
    let ids = rules.iter().map(|rule| rule.id.clone()).collect();
    snapshot.settings.terminal.keyword_highlights.rules = rules;
    let preferences = services.preferences.clone();
    services
        .executor
        .run(move |_| async move {
            preferences
                .save_settings(snapshot.settings, snapshot.revision)
                .await
        })
        .await?;
    Ok(ids)
}

/// 通过实际滚轮、输入和选择验证长列表可达及新建、已有规则的草稿互不覆盖。
/// 三主题逐一核对摘要与按钮的真实几何，避免按钮可见却裁掉第二行内容。
///
/// @author kongweiguang
pub(super) async fn verify_draft_switch(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
    new_rule_id: &str,
    fixture_rules: &[String],
) -> Result<Vec<SettingCheck>, Box<dyn Error>> {
    if fixture_rules.len() != FIXTURE_RULE_COUNT {
        return Err(io::Error::other("keyword fixture count changed unexpectedly").into());
    }
    let first_selector = format!("keyword-rule-{}", fixture_rules[0]);
    let last_id = fixture_rules
        .last()
        .ok_or_else(|| io::Error::other("keyword fixture has no final rule"))?;
    let last_selector = format!("keyword-rule-{last_id}");

    let initially_hidden = window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        let viewport = root.update(app, |root, cx| root.settings_acceptance_content_bounds(cx));
        let list = window
            .debug_selector_bounds("keyword-rule-list-viewport")
            .ok_or_else(|| io::Error::other("keyword list viewport is missing"))?;
        Ok::<_, io::Error>(
            selector_bounds(window, &last_selector).map_or(true, |bounds| {
                !bounds_inside_viewport(bounds, list.intersect(&viewport))
            }),
        )
    })??;
    if !initially_hidden {
        return Err(io::Error::other("last keyword rule did not require list scrolling").into());
    }

    reveal_settings_selector(cx, root, window, "keywords", &last_selector).await?;
    click_selector(cx, window, &last_selector)?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.keyword_editor_rule_id() == Some(last_id.as_str())
            && state.keyword_editor_pattern() == Some("fixture-rule-62")
            && state.keyword_editor_note() == Some("fixture")
    })
    .await?;
    verify_summary_layout(cx, root, window, context, screenshots, last_id).await?;
    let scroll_capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-keyword-list-scroll.png"),
    )?;
    screenshots.push(scroll_capture.clone());

    reveal_settings_selector(cx, root, window, "keywords", "keyword-note-field").await?;
    click_and_type(cx, window, "keyword-note-field", PARKED_NOTE)?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.keyword_editor_rule_id() == Some(last_id.as_str())
            && state.keyword_editor_note() == Some(PARKED_NOTE)
    })
    .await?;

    ensure_page(cx, root, window, "appearance", "appearance").await?;
    ensure_page(cx, root, window, "keywords", "keywords").await?;
    reveal_settings_selector(cx, root, window, "keywords", &last_selector).await?;
    click_selector(cx, window, &last_selector)?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.keyword_editor_rule_id() == Some(last_id.as_str())
            && state.keyword_editor_note() == Some(PARKED_NOTE)
    })
    .await?;

    click_selector(cx, window, "preferences-close")?;
    let mut focus_trace = vec![capture_close_focus(cx, root, window, "after close click")?];
    wait_close_confirmation(cx, window, true).await?;
    focus_trace.push(capture_close_focus(
        cx,
        root,
        window,
        "after confirmation frame",
    )?);
    if !root_state(cx, root, window)?.modal_open() {
        return Err(io::Error::other(
            "keyword draft close confirmation did not keep settings open",
        )
        .into());
    }
    focus_trace.push(capture_close_focus(cx, root, window, "after root state")?);
    screenshots.push(save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-keyword-close-confirmation.png"),
    )?);
    focus_trace.push(capture_close_focus(cx, root, window, "after screenshot")?);
    wait_close_button_focus(
        cx,
        window,
        "preferences-close-cancel",
        "initial",
        &focus_trace,
    )
    .await?;
    for (index, (key, expected)) in [
        ("tab", "preferences-close-discard"),
        ("tab", "preferences-close-cancel"),
        ("shift-tab", "preferences-close-discard"),
        ("shift-tab", "preferences-close-cancel"),
        ("tab", "preferences-close-discard"),
        ("shift-tab", "preferences-close-cancel"),
    ]
    .into_iter()
    .enumerate()
    {
        press_key(cx, window, key)?;
        let stage = format!("key {} {key}", index + 1);
        focus_trace.push(capture_close_focus(cx, root, window, &stage)?);
        wait_close_button_focus(cx, window, expected, &stage, &focus_trace).await?;
    }
    press_key(cx, window, "ctrl-f")?;
    focus_trace.push(capture_close_focus(cx, root, window, "after ctrl-f")?);
    wait_close_button_focus(cx, window, "either", "ctrl-f", &focus_trace).await?;
    press_key(cx, window, "escape")?;
    wait_close_confirmation(cx, window, false).await?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open()
            && state.page_id() == "keywords"
            && state.keyword_editor_rule_id() == Some(last_id.as_str())
            && state.keyword_editor_note() == Some(PARKED_NOTE)
    })
    .await?;

    click_selector(cx, window, "preferences-close")?;
    wait_close_confirmation(cx, window, true).await?;
    click_selector(cx, window, "preferences-close-cancel")?;
    wait_close_confirmation(cx, window, false).await?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.modal_open()
            && state.page_id() == "keywords"
            && state.keyword_editor_rule_id() == Some(last_id.as_str())
            && state.keyword_editor_note() == Some(PARKED_NOTE)
            && state.draft_ready()
            && !state.draft_dirty()
            && !state.keyword_save_pending()
    })
    .await?;

    reveal_settings_selector(cx, root, window, "keywords", "keyword-new").await?;
    let before_resume = root_state(cx, root, window)?;
    if before_resume.draft_keyword_rule_count() != Some(FIXTURE_RULE_COUNT)
        || before_resume.keyword_editor_is_new()
    {
        return Err(
            io::Error::other("keyword creation positive control is not at 63 rules").into(),
        );
    }
    assert_keyword_new_click_capability(cx, window, true)?;
    click_selector(cx, window, "keyword-new")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.keyword_editor_is_new()
            && state.keyword_editor_rule_id() == Some(new_rule_id)
            && state.keyword_editor_pattern() == Some(NEW_PATTERN)
            && state.keyword_editor_note() == Some(NEW_NOTE)
    })
    .await?;
    reveal_settings_selector(cx, root, window, "keywords", &last_selector).await?;
    click_selector(cx, window, &last_selector)?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.keyword_editor_rule_id() == Some(last_id.as_str())
            && state.keyword_editor_note() == Some(PARKED_NOTE)
    })
    .await?;

    reveal_settings_selector(cx, root, window, "keywords", "keyword-search-field").await?;
    click_and_type(cx, window, "keyword-search-field", "fixture-rule-62")?;
    reveal_settings_selector(cx, root, window, "keywords", &last_selector).await?;
    let filtering_correct = window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        Ok::<_, io::Error>(
            selector_bounds(window, &last_selector).is_ok()
                && selector_bounds(window, &first_selector).is_err(),
        )
    })??;
    if !filtering_correct {
        return Err(io::Error::other("keyword search did not filter the actual rule list").into());
    }
    let search_capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-keyword-filter.png"),
    )?;
    screenshots.push(search_capture.clone());
    reveal_settings_selector(cx, root, window, "keywords", "keyword-search-field").await?;
    click_and_type(cx, window, "keyword-search-field", "")?;
    reveal_settings_selector(cx, root, window, "keywords", "keyword-new").await?;
    click_selector(cx, window, "keyword-new")?;
    wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
        state.keyword_editor_is_new()
            && state.keyword_editor_rule_id() == Some(new_rule_id)
            && state.keyword_editor_pattern() == Some(NEW_PATTERN)
            && state.keyword_editor_note() == Some(NEW_NOTE)
    })
    .await?;

    Ok(vec![
        SettingCheck {
            action: "keyword-scroll-and-preserve-drafts",
            passed: true,
            expected: "last-of-63|both-drafts-preserved|category-switch|focus-loop|ctrl-f-trapped|escape-and-click-cancel".to_owned(),
            observed: "last-of-63|both-drafts-preserved|category-switch|focus-loop|ctrl-f-trapped|escape-and-click-cancel".to_owned(),
            capture: Some(scroll_capture),
        },
        SettingCheck {
            action: "keyword-search-filter",
            passed: true,
            expected: "fixture-rule-62-only".to_owned(),
            observed: "fixture-rule-62-only".to_owned(),
            capture: Some(search_capture),
        },
    ])
}

/// 真实主题切换后重新定位末行，检查完整摘要并保留原生截图；使用隔离设置根避免改动用户配置。
///
/// @author kongweiguang
async fn verify_summary_layout(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
    rule_id: &str,
) -> Result<(), Box<dyn Error>> {
    let selector = format!("keyword-rule-{rule_id}");
    let summary_selector = format!("keyword-rule-summary-{rule_id}");
    for (_, name, theme_selector) in THEMES {
        ensure_page(cx, root, window, "appearance", "appearance").await?;
        reveal_settings_selector(cx, root, window, "appearance", theme_selector).await?;
        click_selector(cx, window, theme_selector)?;
        wait_for_state(cx, root, window, Instant::now() + ACTION_TIMEOUT, |state| {
            state.draft_theme_mode() == Some(name) && !state.draft_dirty() && state.draft_ready()
        })
        .await?;
        ensure_page(cx, root, window, "keywords", "keywords").await?;
        reveal_settings_selector(cx, root, window, "keywords", &selector).await?;
        window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            draw_acceptance_frame(window, app);
            let button = selector_bounds(window, &selector)?;
            let summary = window
                .debug_selector_bounds(&summary_selector)
                .ok_or_else(|| io::Error::other("keyword summary geometry is missing"))?;
            if !bounds_inside_viewport(summary, button) {
                return Err(io::Error::other(format!(
                    "keyword summary is clipped in {name}: summary={summary:?}, button={button:?}"
                )));
            }
            Ok::<_, io::Error>(())
        })??;
        screenshots.push(save_frame(
            cx,
            window,
            context
                .output_directory
                .join(format!("settings-functional-keyword-summary-{name}.png")),
        )?);
    }
    Ok(())
}

/// 64 条规则时验证新增入口真正禁用，同时测量固定页脚及保存按钮的实际裁切关系。
///
/// @author kongweiguang
pub(super) async fn verify_rule_limit_and_footer(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    services: &kerminal_ui::AppServices,
    context: &AcceptanceContext,
    screenshots: &mut Vec<CaptureRecord>,
    rules: &[TerminalKeywordHighlightRule],
) -> Result<SettingCheck, Box<dyn Error>> {
    if rules.len() != 64 {
        return Err(io::Error::other("keyword save did not reach the 64-rule limit").into());
    }
    let before = root_state(cx, root, window)?;
    if before.draft_keyword_rule_count() != Some(64)
        || before.keyword_editor_is_new()
        || before.keyword_save_pending()
        || before.draft_dirty()
        || !before.draft_ready()
    {
        return Err(
            io::Error::other("keyword limit check requires a settled 64-rule editor").into(),
        );
    }
    let active_rule_id = before.keyword_editor_rule_id().map(str::to_owned);
    let active_pattern = before.keyword_editor_pattern().map(str::to_owned);
    let active_note = before.keyword_editor_note().map(str::to_owned);
    reveal_settings_selector(cx, root, window, "keywords", "keyword-new").await?;
    assert_keyword_new_click_capability(cx, window, false)?;
    click_selector(cx, window, "keyword-new")?;
    let after = root_state(cx, root, window)?;
    if after.keyword_editor_rule_id() != active_rule_id.as_deref()
        || after.keyword_editor_pattern() != active_pattern.as_deref()
        || after.keyword_editor_note() != active_note.as_deref()
        || after.keyword_editor_is_new()
        || after.keyword_save_pending()
        || after.draft_dirty()
        || after.draft_keyword_rule_count() != Some(64)
    {
        return Err(io::Error::other("disabled keyword creation changed the editor").into());
    }
    if load_persisted_settings(services)
        .await?
        .settings
        .terminal
        .keyword_highlights
        .rules
        .as_slice()
        != rules
    {
        return Err(io::Error::other("disabled keyword creation changed persisted rules").into());
    }

    reveal_settings_selector(cx, root, window, "keywords", "keyword-save").await?;
    let footer_visible = window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        let viewport = root.update(app, |root, cx| root.settings_acceptance_content_bounds(cx));
        let footer = window
            .debug_selector_bounds("keyword-rule-editor-footer")
            .ok_or_else(|| io::Error::other("keyword editor footer is missing"))?;
        let save = selector_bounds(window, "keyword-save")?;
        Ok::<_, io::Error>(
            bounds_inside_viewport(footer, viewport) && bounds_inside_viewport(save, footer),
        )
    })??;
    if !footer_visible {
        return Err(io::Error::other("keyword save footer is clipped").into());
    }
    let capture = save_frame(
        cx,
        window,
        context
            .output_directory
            .join("settings-functional-keyword-limit-footer.png"),
    )?;
    screenshots.push(capture.clone());
    Ok(SettingCheck {
        action: "keyword-limit-and-footer",
        passed: true,
        expected: "64-rules|new-disabled|save-visible".to_owned(),
        observed: "64-rules|new-disabled|save-visible".to_owned(),
        capture: Some(capture),
    })
}

/// 同时读取两个真实按钮的焦点；超时时保留窗口原始焦点及绑定路径供定位帧时序问题。
///
/// @author kongweiguang
async fn wait_close_button_focus(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    expected: &str,
    stage: &str,
    focus_trace: &[String],
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let (focused, diagnostic) = window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            draw_acceptance_frame(window, app);
            let read = |selector: &str| {
                let id: ElementId = selector.to_owned().into();
                let button =
                    gpui_kit::base::test_support::find(window, &[], &id).ok_or_else(|| {
                        io::Error::other(format!("close button {selector:?} is missing"))
                    })?;
                if !button.visible() || button.role() != Some(gpui_kit::Role::Button) {
                    return Err(io::Error::other(format!(
                        "close button {selector:?} is not visible"
                    )));
                }
                Ok::<_, io::Error>(button)
            };
            let cancel = read("preferences-close-cancel")?;
            let discard = read("preferences-close-discard")?;
            let cancel_focused = cancel
                .focused()
                .ok_or_else(|| io::Error::other("close cancel button has no observed focus"))?;
            let discard_focused = discard
                .focused()
                .ok_or_else(|| io::Error::other("close discard button has no observed focus"))?;
            let diagnostic = format!(
                "{}, cancel_id={:?}, cancel_path={:?}, discard_id={:?}, discard_path={:?}",
                close_focus_diagnostic(window, app),
                cancel.tracked_focus_id(),
                cancel.path(),
                discard.tracked_focus_id(),
                discard.path(),
            );
            Ok::<_, io::Error>(((cancel_focused, discard_focused), diagnostic))
        })??;
        let matched = match expected {
            "preferences-close-cancel" => focused == (true, false),
            "preferences-close-discard" => focused == (false, true),
            "either" => focused == (true, false) || focused == (false, true),
            _ => return Err(io::Error::other("unknown close confirmation focus target").into()),
        };
        if matched {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "close confirmation focus did not reach {expected:?} at {stage}: cancel={}, discard={}; {diagnostic}; trace={focus_trace:?}",
                focused.0, focused.1,
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 只采样窗口焦点及上一帧观察结果，时间点明确标注且不触发绘制或焦点变更。
///
/// @author kongweiguang
fn capture_close_focus(
    cx: &mut AsyncApp,
    root: &Entity<RootView>,
    window: AnyWindowHandle,
    stage: &str,
) -> Result<String, Box<dyn Error>> {
    let diagnostic = window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        let identities = root.update(app, |root, _| root.settings_acceptance_focus_identities());
        Ok::<_, io::Error>(format!(
            "{}; root_id={:?}, compact_drawer_id={:?}, preferences_modal_id={:?}",
            close_focus_diagnostic(window, app),
            identities[0],
            identities[1],
            identities[2],
        ))
    })??;
    Ok(format!("{stage}: {diagnostic}"))
}

/// 仅在已注册快照中按身份查找焦点控件，缺失时明确标记而不推断控件身份。
///
/// @author kongweiguang
fn close_focus_diagnostic(window: &gpui_pre::Window, app: &App) -> String {
    let actual = window.focused(app);
    let actual_id = actual.as_ref().map(ElementId::from);
    let mut observed = actual_id
        .as_ref()
        .map(|id| {
            gpui_kit::base::test_support::snapshots(window)
                .into_iter()
                .filter(|snapshot| snapshot.tracked_focus_id() == Some(id))
                .map(|snapshot| {
                    format!(
                        "path={:?}, role={:?}, visible={}",
                        snapshot.path(),
                        snapshot.role(),
                        snapshot.visible(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    observed.sort();
    let observed = if actual_id.is_none() {
        "no-window-focus".to_owned()
    } else if observed.is_empty() {
        "no-observed-element".to_owned()
    } else {
        format!("{observed:?}")
    };
    let trap = gpui_kit::base::active_focus_trap(window, app);
    format!(
        "window_focus={actual:?}, actual_id={actual_id:?}, active_trap={trap:?}, observed={observed}"
    )
}

/// 两个真实确认按钮必须同时出现或消失，避免把未绘制的确认框视作关闭成功。
///
/// @author kongweiguang
pub(super) async fn wait_close_confirmation(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    expected_open: bool,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + ACTION_TIMEOUT;
    loop {
        let (cancel, discard) = window.update(cx, |_, window, app| {
            ensure_hidden_window(window)?;
            draw_acceptance_frame(window, app);
            Ok::<_, io::Error>((
                selector_bounds(window, "preferences-close-cancel").is_ok(),
                selector_bounds(window, "preferences-close-discard").is_ok(),
            ))
        })??;
        if cancel == expected_open && discard == expected_open {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "keyword close confirmation did not reach open={expected_open}: cancel={cancel}, discard={discard}"
            ))
            .into());
        }
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// 对照真实按钮的 AccessKit Click 动作，找不到快照或控件不可见均不能视为禁用。
///
/// @author kongweiguang
fn assert_keyword_new_click_capability(
    cx: &mut AsyncApp,
    window: AnyWindowHandle,
    expected: bool,
) -> Result<(), Box<dyn Error>> {
    window.update(cx, |_, window, app| {
        ensure_hidden_window(window)?;
        draw_acceptance_frame(window, app);
        let id: ElementId = "keyword-new".into();
        let button = gpui_kit::base::test_support::find(window, &[], &id)
            .ok_or_else(|| io::Error::other("keyword creation button snapshot is missing"))?;
        if !button.visible() || button.role() != Some(gpui_kit::Role::Button) {
            return Err(io::Error::other(
                "keyword creation snapshot is not a visible button",
            ));
        }
        if button.supports_click() != expected {
            return Err(io::Error::other(format!(
                "keyword creation Click capability did not match expected={expected}"
            )));
        }
        Ok::<_, io::Error>(())
    })??;
    Ok(())
}
