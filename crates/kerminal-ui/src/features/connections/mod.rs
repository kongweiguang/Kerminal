//! 原生主机目录、连接动作与外部凭据确认界面。
//!
//! @author kongweiguang

mod editor_text;
mod external;
mod form;
mod form_layout;
mod form_render;
#[cfg(feature = "right-rail-acceptance")]
pub mod host_editor_acceptance;
mod host_navigation;
mod keyboard;
mod local_hosts;
mod panel;
mod plugin_forms;
mod render;
#[cfg(feature = "right-rail-acceptance")]
mod settings_acceptance;

pub(crate) use form_layout::host_editor_size;
pub use host_navigation::{
    HostNavigation, HostNavigationEvent, HostSessionProjection, HostSessionStatus,
};
pub(crate) use panel::HostFilePickerTarget;
pub use panel::{ConnectionsEvent, ConnectionsPanel};
