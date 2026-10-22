//! 固定右栏容器工具的目标上下文与读写边界。
//!
//! @author kongweiguang

use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    identity::HostId,
    operations::{ContainerRuntime, ContainerTarget},
};

/// 容器工具无法执行时保留 Root 判定出的原因，视图不根据标题或选择状态猜目标。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerPanelUnavailableReason {
    NoTerminal,
    NotReady,
    TerminalEnded,
    TargetMismatch,
    UnsupportedProtocol,
}

impl ContainerPanelUnavailableReason {
    /// 将明确的 Root 判定转换成适合窄栏阅读的说明，不暴露内部 session 字段。
    pub(super) fn label(self) -> (&'static str, &'static str) {
        match self {
            Self::NoTerminal => ("请先打开一个终端。", "Open a terminal to use containers."),
            Self::NotReady => ("当前终端尚未就绪。", "The current terminal is not ready."),
            Self::TerminalEnded => (
                "当前终端已结束或连接已断开，请使用终端的重新连接入口。",
                "The terminal has ended or disconnected. Use the terminal's reconnect action.",
            ),
            Self::TargetMismatch => (
                "当前终端目标已变化，请重新选择终端。",
                "The terminal target changed; select the terminal again.",
            ),
            Self::UnsupportedProtocol => (
                "当前终端协议不支持容器管理。",
                "Containers are unavailable for this terminal protocol.",
            ),
        }
    }
}

/// Root 校验后的显式容器面板上下文；binding 用于会话关联，不构成 MCP 权限边界。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContainerPanelContext {
    Unavailable {
        reason: ContainerPanelUnavailableReason,
    },
    Host {
        binding: UiSessionBinding,
        host_id: Option<HostId>,
    },
    ContainerInfo {
        binding: UiSessionBinding,
        target: ContainerTarget,
        terminal_running: bool,
    },
}

/// 只读容器页面的按需读取类型；Overview 使用 inspect，其他页分别请求 stats 和 logs。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerReadKind {
    Inspect,
    Stats,
    Logs,
}

/// UI 回调创建时冻结目标/runtime/上下文代次，旧菜单不能在面板切换后提交新操作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ContainerPanelActionTicket {
    pub(super) context: Option<ContainerPanelContext>,
    pub(super) runtime: ContainerRuntime,
    pub(super) generation: u64,
    pub(super) target: Option<ContainerTarget>,
}

/// 判断两个 ContainerInfo 投影是否仅改变了终端运行状态，以便保留同一对象快照。
///
/// @author kongweiguang
pub(super) fn same_container_info_identity(
    current: &ContainerPanelContext,
    next: &ContainerPanelContext,
) -> bool {
    matches!(
        (current, next),
        (
            ContainerPanelContext::ContainerInfo {
                binding: current_binding,
                target: current_target,
                ..
            },
            ContainerPanelContext::ContainerInfo {
                binding: next_binding,
                target: next_target,
                ..
            }
        ) if current_binding == next_binding && current_target == next_target
    )
}

/// 终端 running 边沿只更新提示；同一 binding 和 target 的在途读取仍有效。
///
/// @author kongweiguang
pub(super) fn same_read_context_scope(
    current: Option<&ContainerPanelContext>,
    expected: Option<&ContainerPanelContext>,
) -> bool {
    match (current, expected) {
        (None, None) => true,
        (Some(current), Some(expected)) => {
            current == expected || same_container_info_identity(current, expected)
        }
        _ => false,
    }
}

/// 限定宿主管理请求只能命中 Root 绑定的 host 与当前 Docker/Podman runtime。
///
/// @author kongweiguang
pub(super) fn host_context_matches_target(
    context: &ContainerPanelContext,
    host_id: Option<&HostId>,
    runtime: ContainerRuntime,
    target: &ContainerTarget,
) -> bool {
    matches!(
        context,
        ContainerPanelContext::Host { host_id: bound, .. }
            if bound.as_ref() == host_id
                && target.host_id.as_ref() == host_id
                && target.runtime == runtime
    )
}

#[cfg(test)]
#[path = "../../../tests/unit/operations_container_panel.rs"]
mod tests;
