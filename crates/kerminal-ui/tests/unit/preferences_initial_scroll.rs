//! 设置首次挂载与重新打开时的真实滚动几何回归。
//!
//! @author kongweiguang

use super::*;
use gpui_kit::{Context, IntoElement, Render, Window, div, prelude::*};

/// 首次挂载尚无视口 bounds；复用生产面板和打开入口，避免已绘制的 fixture 掩盖初始偏移。
/// @author kongweiguang
#[gpui_kit::test]
fn initial_and_reopened_settings_keep_first_field_visible(app: &mut TestAppContext) {
    struct InsetPanel(Entity<PreferencesPanel>);

    impl Render for InsetPanel {
        /// 模拟工作台弹层的外边距，使滚动区原点与窗口原点不同，覆盖首帧定位的坐标约束。
        /// @author kongweiguang
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().p_6().child(self.0.clone())
        }
    }

    app.update(crate::init);
    for window_size in [
        size(px(800.), px(600.)),
        size(px(1020.), px(760.)),
        size(px(1440.), px(900.)),
    ] {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let repository = MemorySettingsRepository::new();
        let services = PreferencesPanelServices {
            preferences: PreferencesService::new(Arc::new(repository.clone())),
            profiles: ProfileService::new(Arc::new(repository), Arc::new(TestDefaultShell)),
            vault_keys: None,
            configuration_sync: None,
            configuration_changes: None,
            external_protocol: None,
            configuration_directory: None,
            mcp_runtime_available: false,
            mcp_runtime: None,
            executor: ApplicationTaskExecutor::new(runtime.handle().clone()),
        };
        let mounted = std::rc::Rc::new(std::cell::RefCell::new(None));
        let mounted_panel = mounted.clone();
        let (_, visual) = app.add_window_view(move |window, cx| {
            let panel = cx.new(|cx| PreferencesPanel::new(services, window, cx));
            panel.update(cx, |panel, cx| panel.begin_modal_session(window, cx));
            *mounted_panel.borrow_mut() = Some(panel.clone());
            let inset = cx.new(|_| InsetPanel(panel));
            gpui_kit::base::Root::new(inset, window, cx)
        });
        let panel = mounted.borrow_mut().take().unwrap();
        visual.simulate_resize(window_size);
        pump_runtime(&runtime, visual);
        for reopened in [false, true] {
            let (clip, offset) = visual.update(|_, cx| {
                let scroll = &panel.read(cx).content_scroll;
                (scroll.bounds(), scroll.offset())
            });
            let language = target_geometry(visual, "settings-interface-language-select").0;
            assert!(
                offset.y.abs() <= px(0.75),
                "settings must open at top: {window_size:?}, reopened={reopened}, {offset:?}"
            );
            assert!(
                bounds_within_viewport(language, clip),
                "first field must be fully visible: {window_size:?}, reopened={reopened}, {language:?}, {clip:?}"
            );
            if !reopened {
                visual.simulate_event(ScrollWheelEvent {
                    position: clip.center(),
                    delta: ScrollDelta::Lines(point(0., -20.)),
                    ..Default::default()
                });
                draw(visual);
                assert!(visual.update(|_, cx| panel.read(cx).content_scroll.offset().y < px(0.)));
                visual.update(|window, cx| {
                    panel.update(cx, |panel, cx| {
                        panel.begin_modal_session(window, cx);
                        panel.focus_initial_control(window, cx);
                    });
                });
                pump_runtime(&runtime, visual);
            }
        }
    }
}
