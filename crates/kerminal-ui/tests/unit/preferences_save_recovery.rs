//! 设置事务异常必须在当前窗口可见，并能通过真实生产控件继续完成。
//!
//! @author kongweiguang

use super::super::{DraftStatus, panel::PreferencesSection};
use super::*;

/// 同一事务跨页保留归属；小窗口与正文滚动不能带走恢复动作，完成后正文保留焦点。
/// @author kongweiguang
#[gpui_kit::test]
fn settings_failed_save_recovery_stays_visible_across_pages(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    visual.simulate_resize(size(px(800.), px(600.)));
    repository.fail_next_settings_save();
    click(visual, "theme-light");
    pump_runtime(&runtime, visual);
    let attempts = repository.save_attempt_count();
    assert!(visual.update(|_, cx| {
        let draft = panel.read(cx).settings.as_ref().unwrap();
        draft.status() == DraftStatus::Failed
            && draft.value().theme_mode == super::super::ThemeMode::Light
    }));

    for window_size in [size(px(800.), px(600.)), size(px(1440.), px(900.))] {
        visual.simulate_resize(window_size);
        for section in [
            PreferencesSection::Appearance,
            PreferencesSection::Sftp,
            PreferencesSection::Desktop,
            PreferencesSection::Shortcuts,
        ] {
            visual.update(|_, cx| {
                panel.update(cx, |panel, cx| {
                    panel.section = section;
                    cx.notify();
                })
            });
            let (before, visible) = target_geometry(visual, "preferences-retry-save");
            assert!(visible, "retry must be visible in {section:?}");
            let clip = visual.update(|_, cx| panel.read(cx).content_scroll.bounds());
            visual.simulate_event(ScrollWheelEvent {
                position: clip.center(),
                delta: ScrollDelta::Lines(point(0., -40.)),
                ..Default::default()
            });
            let (after, visible) = target_geometry(visual, "preferences-retry-save");
            assert!(
                visible && before == after,
                "body scrolling must preserve recovery in {section:?}"
            );
        }
    }
    click(visual, "preferences-retry-save");
    pump_runtime(&runtime, visual);
    assert_eq!(repository.save_attempt_count(), attempts + 1);
    assert_eq!(
        repository.snapshot().0.theme_mode,
        super::super::ThemeMode::Light
    );
    assert_not_rendered(visual, "preferences-retry-save");
    assert!(visual.update(|window, cx| panel.read(cx).settings_content_focus.is_focused(window)));
}

/// 未确认提交只读回原事务；重复点击不能加读或重发保存，撤掉恢复按钮后仍有有效焦点。
/// @author kongweiguang
#[gpui_kit::test]
fn settings_unknown_recovery_reads_once_without_repeating_save(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    visual.simulate_resize(size(px(800.), px(600.)));
    repository.state.lock().unwrap().unknown_after_settings_save = true;
    click(visual, "theme-light");
    pump_runtime(&runtime, visual);
    assert_eq!(
        visual.update(|_, cx| panel.read(cx).settings.as_ref().unwrap().status()),
        DraftStatus::OutcomeUnknown
    );
    let attempts = repository.save_attempt_count();
    let reads = repository.state.lock().unwrap().settings_read_count;
    for section in [PreferencesSection::Appearance, PreferencesSection::Terminal] {
        visual.update(|_, cx| {
            panel.update(cx, |panel, cx| {
                panel.section = section;
                cx.notify();
            })
        });
        assert!(target_geometry(visual, "preferences-read-back-settings").1);
        assert_not_rendered(visual, "preferences-retry-save");
        assert_not_rendered(visual, "terminal-retry-save");
    }
    click(visual, "preferences-read-back-settings");
    click(visual, "preferences-read-back-settings");
    pump_runtime(&runtime, visual);
    assert_eq!(
        repository.state.lock().unwrap().settings_read_count,
        reads + 1
    );
    assert_eq!(repository.save_attempt_count(), attempts);
    assert!(visual.update(|_, cx| {
        let draft = panel.read(cx).settings.as_ref().unwrap();
        draft.status() == DraftStatus::Ready && !draft.is_dirty()
    }));
    assert_not_rendered(visual, "preferences-read-back-settings");
    assert!(visual.update(|window, cx| panel.read(cx).settings_content_focus.is_focused(window)));
}
