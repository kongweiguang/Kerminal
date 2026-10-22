//! 分屏群发的 pane/header 投影与工具栏组件。
//!
//! @author kongweiguang

mod toolbar;

pub(crate) use toolbar::{
    WorkspaceBroadcastBar, WorkspaceBroadcastBarEvent, WorkspaceBroadcastBarProjection,
    WorkspaceBroadcastDeliveryProjection, WorkspaceBroadcastSnippetProjection,
};

/// pane header 只呈现当前 Tab 的群发目标状态，不持有任何 PTY 或业务 binding。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PaneBroadcastProjection {
    pub visible: bool,
    pub selected: bool,
    pub enabled: bool,
}

/// 广播目标选择始终是显式状态，避免焦点变化意外重写用户的自定义选择。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum WorkspaceBroadcastTargetMode {
    #[default]
    All,
    Current,
    Custom,
}
