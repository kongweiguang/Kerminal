//! Kerminal 的原生 GPUI 界面层。
//!
//! @author kongweiguang

extern crate gpui_kit as gpui;

#[cfg(feature = "acceptance-metrics")]
pub mod acceptance_metrics;
pub mod features;
mod keybindings;
pub mod locale;
mod root;
mod shell_arguments;
mod terminal_background;
mod terminal_canvas;
mod terminal_input_handler;
mod terminal_interaction;
mod terminal_metrics;
mod theme;
pub mod window_state;

pub use features::{editor::FileEditorWorkspace, preferences::PreferencesPanel};
pub use root::{AppServices, QuitIntent, RootHostLifecycle, RootView, RootViewEvent};

/// 先注册 Kit 的基础按键，再注册应用选择器的局部覆盖，保留普通 Popover 的原始语义。
pub fn init(cx: &mut gpui_kit::App) {
    gpui_kit::init(cx);
    features::input_composition::init(cx);
}
