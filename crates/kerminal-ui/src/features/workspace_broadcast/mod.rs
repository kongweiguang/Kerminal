//! 分屏群发的 pane/header 投影与工具栏组件。
//!
//! @author kongweiguang

mod toolbar;

/// 草稿校验与发送准入共享既有字节预算，避免多字节文本在两层得到不同结论。
/// 保留不合法草稿供用户修正；这里只返回原因，不截断或替换输入。
///
/// @author kongweiguang
pub(crate) fn broadcast_draft_error(draft: &str) -> Option<gpui_kit::SharedString> {
    if draft.len() > 16 * 1024 {
        Some(crate::locale::text(
            "命令超过 16 KiB，请缩短后发送；草稿已保留。",
            "Command exceeds 16 KiB. Shorten it before sending; the draft is preserved.",
        ))
    } else if draft.contains('\0') {
        Some(crate::locale::text(
            "命令包含不支持的空字符，请移除后发送。",
            "Command contains a null character. Remove it before sending.",
        ))
    } else {
        None
    }
}

pub(crate) use toolbar::{
    WorkspaceBroadcastBar, WorkspaceBroadcastBarEvent, WorkspaceBroadcastBarProjection,
    WorkspaceBroadcastDeliveryProjection, WorkspaceBroadcastTargetProjection,
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
