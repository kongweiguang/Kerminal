//! 容器写入与权威刷新之间的状态，不以按钮点击或命令回执代替最终列表。
//!
//! @author kongweiguang

use kerminal_domain::{
    identity::HostId,
    operations::{
        ComposeAction, ComposeProjectTarget, ContainerAction, ContainerRuntime, ContainerTarget,
    },
};

/// 按完整对象记录动作，避免同名项目或切换宿主后把反馈挂到另一行。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ContainerOperation {
    Container {
        target: ContainerTarget,
        action: ContainerAction,
    },
    Compose {
        target: ComposeProjectTarget,
        action: ComposeAction,
    },
}

impl ContainerOperation {
    /// 一次宿主列表回读能核对该宿主和运行时的所有容器变更。
    ///
    /// @author kongweiguang
    fn matches_scope(&self, host: &Option<HostId>, runtime: ContainerRuntime) -> bool {
        match self {
            Self::Container { target, .. } => &target.host_id == host && target.runtime == runtime,
            Self::Compose { target, .. } => &target.host_id == host && target.runtime == runtime,
        }
    }
}

/// 未知结果在刷新前始终阻止再次写入同一宿主，避免意外重复执行。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ContainerOperationOutcome {
    Applied,
    NoChange,
    Failed(String),
    Unknown(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperationPhase {
    Executing,
    Refreshing,
    NeedsRefresh,
}

#[derive(Clone, Debug)]
struct PendingOperation {
    operation: ContainerOperation,
    phase: OperationPhase,
    outcome: Option<ContainerOperationOutcome>,
}

/// 不同宿主可以分别保留待核对结果；同一宿主仍按现有 busy 策略串行。
#[derive(Default)]
pub(super) struct ContainerMutationTracker {
    pending: Vec<PendingOperation>,
}

impl ContainerMutationTracker {
    /// 同一宿主在上次结果核对前不接受新写操作，避免基于陈旧状态重复执行。
    ///
    /// @author kongweiguang
    pub(super) fn begin(&mut self, operation: ContainerOperation) -> bool {
        let blocked = match &operation {
            ContainerOperation::Container { target, .. } => {
                self.blocks_scope(&target.host_id, target.runtime)
            }
            ContainerOperation::Compose { target, .. } => {
                self.blocks_scope(&target.host_id, target.runtime)
            }
        };
        if blocked {
            return false;
        }
        self.pending.push(PendingOperation {
            operation,
            phase: OperationPhase::Executing,
            outcome: None,
        });
        true
    }

    /// 命令返回后仍保留对象直到其宿主列表回读，取消和异常不能直接解锁重试。
    ///
    /// @author kongweiguang
    pub(super) fn complete(
        &mut self,
        operation: &ContainerOperation,
        outcome: ContainerOperationOutcome,
    ) {
        if let Some(pending) = self
            .pending
            .iter_mut()
            .find(|pending| &pending.operation == operation)
        {
            pending.phase = OperationPhase::Refreshing;
            pending.outcome = Some(outcome);
        }
    }

    /// 同宿主写入可能影响项目及其容器，因此以宿主和运行时作为串行边界。
    ///
    /// @author kongweiguang
    pub(super) fn blocks_scope(&self, host: &Option<HostId>, runtime: ContainerRuntime) -> bool {
        self.pending
            .iter()
            .any(|pending| pending.operation.matches_scope(host, runtime))
    }

    /// 回读必须在命令回执之后发起；更早发出的读取即使晚到也不能核对该次写入。
    ///
    /// @author kongweiguang
    pub(super) fn needs_refresh(&self, host: &Option<HostId>, runtime: ContainerRuntime) -> bool {
        self.pending.iter().any(|pending| {
            pending.phase != OperationPhase::Executing
                && pending.operation.matches_scope(host, runtime)
        })
    }

    /// 只有执行和等待回读阶段显示动画，失败后的静态提示允许用户主动刷新。
    ///
    /// @author kongweiguang
    pub(super) fn container_loading(&self, target: &ContainerTarget) -> bool {
        self.pending.iter().any(|pending| {
            pending.phase != OperationPhase::NeedsRefresh
                && matches!(&pending.operation, ContainerOperation::Container { target: current, .. } if current == target)
        })
    }

    /// 取消只能指向仍在执行的对象，等待刷新不能借用其它宿主的取消令牌。
    ///
    /// @author kongweiguang
    pub(super) fn container_executing(&self, target: &ContainerTarget) -> bool {
        self.pending.iter().any(|pending| {
            pending.phase == OperationPhase::Executing
                && matches!(&pending.operation, ContainerOperation::Container { target: current, .. } if current == target)
        })
    }

    /// 项目动画绑定完整 Compose 身份，不按显示名称推断。
    ///
    /// @author kongweiguang
    pub(super) fn compose_loading(&self, target: &ComposeProjectTarget) -> bool {
        self.pending.iter().any(|pending| {
            pending.phase != OperationPhase::NeedsRefresh
                && matches!(&pending.operation, ContainerOperation::Compose { target: current, .. } if current == target)
        })
    }

    /// 项目取消按钮只绑定原项目尚未返回的命令，刷新阶段没有可取消的写入。
    ///
    /// @author kongweiguang
    pub(super) fn compose_executing(&self, target: &ComposeProjectTarget) -> bool {
        self.pending.iter().any(|pending| {
            pending.phase == OperationPhase::Executing
                && matches!(&pending.operation, ContainerOperation::Compose { target: current, .. } if current == target)
        })
    }

    /// 只消费命令返回之后的同宿主权威读取；失败时保留门禁和原始结果。
    ///
    /// @author kongweiguang
    pub(super) fn finish_refresh(
        &mut self,
        host: &Option<HostId>,
        runtime: ContainerRuntime,
        success: bool,
    ) -> Option<(ContainerOperationOutcome, bool)> {
        let index = self.pending.iter().position(|pending| {
            pending.phase != OperationPhase::Executing
                && pending.operation.matches_scope(host, runtime)
        })?;
        if success {
            let pending = self.pending.remove(index);
            pending.outcome.map(|outcome| (outcome, true))
        } else {
            let pending = &mut self.pending[index];
            pending.phase = OperationPhase::NeedsRefresh;
            pending.outcome.clone().map(|outcome| (outcome, false))
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/operations_container_mutation.rs"]
mod tests;
