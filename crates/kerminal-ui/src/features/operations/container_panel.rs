//! 固定右栏容器工具的目标上下文与读写边界。
//!
//! @author kongweiguang

use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    identity::{HostId, TabId},
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
    /// 不可用状态沿用 main 的无 SSH 宿主说明，不为本机或非 SSH 目标增加特殊引导。
    ///
    /// @author kongweiguang
    pub(super) fn label(self) -> (&'static str, &'static str) {
        match self {
            Self::NoTerminal => (
                "当前没有可用于容器管理的 SSH 主机。",
                "No SSH host is available for container management.",
            ),
            Self::NotReady => ("当前终端尚未就绪。", "The current terminal is not ready."),
            Self::TerminalEnded => (
                "当前 SSH 主机连接已结束。",
                "The SSH host connection has ended.",
            ),
            Self::TargetMismatch => (
                "当前终端与容器 SSH 主机不匹配。",
                "The active terminal does not match the container SSH host.",
            ),
            Self::UnsupportedProtocol => (
                "容器仅支持 SSH 主机上的 Docker 和 Podman。",
                "Containers are supported only on SSH hosts using Docker and Podman.",
            ),
        }
    }
}

/// Root 校验后的显式面板上下文；文件 Tab 用真实 tab/host 身份，不伪造会话 binding。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContainerPanelContext {
    Unavailable {
        reason: ContainerPanelUnavailableReason,
    },
    Host {
        binding: UiSessionBinding,
        host_id: Option<HostId>,
    },
    WorkspaceHost {
        tab_id: TabId,
        host_id: HostId,
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

/// 运行状态边沿只更新提示；同一容器目标或宿主（包括文件 Tab）的在途读取仍有效。
///
/// @author kongweiguang
pub(super) fn same_read_context_scope(
    current: Option<&ContainerPanelContext>,
    expected: Option<&ContainerPanelContext>,
) -> bool {
    match (current, expected) {
        (None, None) => true,
        (Some(current), Some(expected)) => {
            current == expected
                || same_container_info_identity(current, expected)
                || same_host_context_scope(current, expected)
        }
        _ => false,
    }
}

/// 跨 pane 或文件 Tab 时可复用同宿主读取；写操作仍比较完整上下文身份。
///
/// @author kongweiguang
pub(super) fn same_host_context_scope(
    current: &ContainerPanelContext,
    expected: &ContainerPanelContext,
) -> bool {
    match (current, expected) {
        (
            ContainerPanelContext::Host {
                host_id: current, ..
            },
            ContainerPanelContext::Host {
                host_id: expected, ..
            },
        ) => current == expected,
        (
            ContainerPanelContext::Host {
                host_id: Some(current),
                ..
            },
            ContainerPanelContext::WorkspaceHost {
                host_id: expected, ..
            },
        )
        | (
            ContainerPanelContext::WorkspaceHost {
                host_id: current, ..
            },
            ContainerPanelContext::Host {
                host_id: Some(expected),
                ..
            },
        ) => current == expected,
        (
            ContainerPanelContext::WorkspaceHost {
                host_id: current, ..
            },
            ContainerPanelContext::WorkspaceHost {
                host_id: expected, ..
            },
        ) => current == expected,
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
    let context_matches = match context {
        ContainerPanelContext::Host { host_id: bound, .. } => bound.as_ref() == host_id,
        ContainerPanelContext::WorkspaceHost { host_id: bound, .. } => Some(bound) == host_id,
        _ => false,
    };
    context_matches && target.host_id.as_ref() == host_id && target.runtime == runtime
}

#[cfg(test)]
#[path = "../../../tests/unit/operations_container_panel.rs"]
mod tests;
