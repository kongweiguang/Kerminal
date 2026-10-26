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
mod ui_button;
pub mod window_state;

pub use features::{editor::FileEditorWorkspace, preferences::PreferencesPanel};
pub use root::{AppServices, QuitIntent, RootHostLifecycle, RootView, RootViewEvent};

/// 先注册 Kit 基础按键，再限定选择器和聚焦终端的覆盖，保留其它控件的正常焦点导航。
pub fn init(cx: &mut gpui_kit::App) {
    gpui_kit::init(cx);
    features::input_composition::init(cx);
    terminal_input_handler::init(cx);
}
