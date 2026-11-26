//! 内置插件的 GPUI 工厂边界；数据贡献仍定义在 application。
//!
//! @author kongweiguang

pub mod actions;
pub(crate) mod builtin_content;
pub mod content;
pub mod external;
pub mod external_host_form;
pub mod external_settings;
pub mod external_webview;
pub(crate) mod host_api;
pub mod host_forms;
pub(crate) mod icon_names;
#[cfg(feature = "acceptance-metrics")]
pub mod native_probe;
pub mod tools;
pub mod webview;
