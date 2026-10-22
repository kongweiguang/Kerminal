//! SOCKS 代理面板与应用用例之间的 Root 编排边界。
//!
//! @author kongweiguang

use std::sync::{Arc, Mutex};

use gpui_kit::{Context, Entity, EntityId};
use kerminal_application::{
    AppError,
    operations::{
        CurrentTerminalProxyRequest, FutureProxyApplyRequest, FutureProxyAttemptResult,
        FutureProxyRetryIntent, TerminalProxyApplication, TerminalProxyApplyOutcome,
        TerminalProxyApplyResult, TerminalProxyReconcileOutcome, TerminalProxyReconcileResult,
        TerminalProxyUnavailableReason,
    },
    sessions::{
        TerminalCommandAvailability, TerminalCommandContext, TerminalCommandUnavailableReason,
        UiSessionBinding,
    },
};
use kerminal_domain::{
    connections::RemoteShellDialect,
    identity::{HostId, SessionId},
    sessions::{SessionOwner, SessionTarget},
};
use tokio_util::sync::CancellationToken;

use crate::features::operations::proxy_panel::{
    ProxyApplicationPanel, ProxyCurrentApplication, ProxyCurrentAvailability, ProxyCurrentSession,
    ProxyFutureProjection, ProxyHistoryResult, ProxyOperationStatus, ProxyPanelEvent,
    ProxyReviewScript, ProxyUnavailableReason,
};

use super::{RootView, workspace_runtime::RuntimePanePhase};

/// 同时绑定工作区 pane、目标主机、选中转发与会话 owner，避免结果被转投到新焦点。
#[derive(Clone, Debug, Eq, PartialEq)]
struct ProxyCurrentScope {
    selected_host_id: Option<HostId>,
    host_id: HostId,
    forward_id: Option<String>,
    session_id: SessionId,
    owner: SessionOwner,
}

/// UI 投影与恢复原 ID 由同一回读构造，避免把凭据或恢复意图放入公共终端 DTO。
#[derive(Clone, Debug)]
struct ProxyCurrentReadback {
    session: ProxyCurrentSession,
    recovery_forward_id: Option<String>,
}

/// 在应用执行器尚未给出 token 时也能记住取消，避免快速切 pane 后旧任务仍开始远程探测。
#[derive(Clone, Default)]
struct CancellationSlot {
    state: Arc<Mutex<CancellationState>>,
}

#[derive(Default)]
struct CancellationState {
    cancelled: bool,
    token: Option<CancellationToken>,
}

/// 同一面板最多有当前态读取、未来策略读取及若干带 request id 的操作。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum ProxyTaskKey {
    CurrentRead(EntityId),
    FutureRead(EntityId),
    InitialReady(SessionId),
    PanelRequest(EntityId, u64),
}

/// 异步任务回执由 key 和 generation 双重关联；旧请求不能清除新请求的 pending 状态。
#[derive(Clone, Debug, Eq, PartialEq)]
struct ProxyTaskTicket {
    key: ProxyTaskKey,
    generation: u64,
}

struct ActiveProxyTask {
    generation: u64,
    scope: Option<ProxyCurrentScope>,
    cancellation: Option<CancellationSlot>,
}

/// Root 持有异步回执 generation 和协作取消，不创建第二个代理或终端服务。
/// 保留首次 SSH 策略的安全结果和不透明一次性重试授权。
pub(super) struct InitialProxyNotice {
    pub(super) binding: UiSessionBinding,
    pub(super) message: gpui_kit::SharedString,
    pub(super) retry_intent: Option<FutureProxyRetryIntent>,
    pub(super) retrying: bool,
}

#[derive(Default)]
pub(super) struct ProxyRuntime {
    next_generation: u64,
    active: std::collections::HashMap<ProxyTaskKey, ActiveProxyTask>,
}

impl CancellationSlot {
    /// 注册执行器发放的 token；若用户已先取消，token 在进入应用用例前立即取消。
    fn install(&self, token: CancellationToken) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.cancelled {
            token.cancel();
        } else {
            state.token = Some(token);
        }
    }

    /// 记录取消并唤醒当前应用任务；提交后结果仍由原任务持有并完成必要读回。
    fn cancel(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cancelled = true;
        if let Some(token) = state.token.as_ref() {
            token.cancel();
        }
    }

    /// 读取用户取消意图，用于区分明确取消与一般应用错误。
    fn is_cancelled(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancelled
    }
}

impl ProxyRuntime {
    /// 登记新任务前取消同类旧读取，并使其迟到回执通过 generation 检查失效。
    fn begin(
        &mut self,
        key: ProxyTaskKey,
        scope: Option<ProxyCurrentScope>,
        cancellation: Option<CancellationSlot>,
    ) -> ProxyTaskTicket {
        if let Some(previous) = self.active.remove(&key)
            && let Some(cancellation) = previous.cancellation
        {
            cancellation.cancel();
        }
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        self.active.insert(
            key.clone(),
            ActiveProxyTask {
                generation,
                scope,
                cancellation,
            },
        );
        ProxyTaskTicket { key, generation }
    }

    /// 判断同一个 session 的首次 Ready 策略是否仍在途，阻止重复 ACK 并发启动。
    fn is_active(&self, key: &ProxyTaskKey) -> bool {
        self.active.contains_key(key)
    }

    /// 只接受仍对应最新 key 和 generation 的异步结果。
    fn is_current(&self, ticket: &ProxyTaskTicket) -> bool {
        self.active
            .get(&ticket.key)
            .is_some_and(|active| active.generation == ticket.generation)
    }

    /// 完成本轮任务并释放取消槽，避免状态 map 随历史操作增长。
    fn finish(&mut self, ticket: &ProxyTaskTicket) {
        if self.is_current(ticket) {
            self.active.remove(&ticket.key);
        }
    }

    /// 用户取消只请求协作停止；保留任务登记让结束回执和 readback 仍有原始所有者。
    fn cancel_panel_request(&self, panel_id: EntityId, request_id: u64) {
        if let Some(cancellation) = self
            .active
            .get(&ProxyTaskKey::PanelRequest(panel_id, request_id))
            .and_then(|active| active.cancellation.as_ref())
        {
            cancellation.cancel();
        }
    }

    /// pane、主机或所选转发变化时取消旧 current 读取和有绑定的用户操作。
    fn cancel_stale_current(&mut self, panel_id: EntityId, scope: Option<&ProxyCurrentScope>) {
        let stale_keys = self
            .active
            .iter()
            .filter(|(key, active)| {
                matches!(key, ProxyTaskKey::CurrentRead(id) if *id == panel_id)
                    || (matches!(key, ProxyTaskKey::PanelRequest(id, _) if *id == panel_id)
                        && active.scope.is_some()
                        && active.scope.as_ref() != scope)
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in stale_keys {
            if matches!(key, ProxyTaskKey::CurrentRead(_)) {
                if let Some(active) = self.active.remove(&key)
                    && let Some(cancellation) = active.cancellation
                {
                    cancellation.cancel();
                }
            } else if let Some(cancellation) = self
                .active
                .get(&key)
                .and_then(|active| active.cancellation.as_ref())
            {
                // 保留在途操作的 ticket，取消后仍需消费其最终回执并释放面板 busy 状态。
                cancellation.cancel();
            }
        }
    }

    /// 同一绑定仍在执行用户操作时，不并发启动新的 shell 探针打断命令提示符。
    fn has_current_operation(&self, scope: &ProxyCurrentScope) -> bool {
        self.active.iter().any(|(key, active)| {
            matches!(key, ProxyTaskKey::PanelRequest(_, _))
                && active.scope.as_ref() == Some(scope)
                && active.cancellation.is_some()
        })
    }

    /// 没有可读主机时停止未来策略读取，已有非取消型 CAS 操作仍等待明确结果。
    fn cancel_future_read(&mut self, panel_id: EntityId) {
        if let Some(active) = self.active.remove(&ProxyTaskKey::FutureRead(panel_id))
            && let Some(cancellation) = active.cancellation
        {
            cancellation.cancel();
        }
    }
}

impl Drop for ProxyRuntime {
    /// Root 退出时协作取消当前和脚本读取；已提交的未来策略 CAS 不在运行中途伪称取消。
    fn drop(&mut self) {
        for active in self.active.values() {
            if let Some(cancellation) = active.cancellation.as_ref() {
                cancellation.cancel();
            }
        }
    }
}

/// 由 controller 的活动 tab/focused pane 解析实际 UI owner；Agent 与后台 session 不会成为代理目标。
fn active_ui_ssh_target(root: &RootView) -> Option<(HostId, SessionId, SessionOwner)> {
    let snapshot = root.workspace_controller.as_ref()?.snapshot();
    let tab_id = snapshot.active_tab_id.as_ref()?;
    let layout =
        snapshot.terminal_tabs.iter().find_map(|tab| match tab {
            kerminal_application::workspace::WorkspaceSessionTab::Terminal {
                id, layout, ..
            } if id == tab_id => Some(layout),
            _ => None,
        })?;
    let pane_ids = layout.pane_ids();
    let pane_id = snapshot
        .focused_pane_id
        .as_ref()
        .filter(|focused| pane_ids.contains(focused))
        .cloned()
        .or_else(|| pane_ids.first().cloned())?;
    let pane = root.workspace_runtime.pane(&pane_id)?;
    if pane.tab_id != *tab_id || pane.phase != RuntimePanePhase::Ready {
        return None;
    }
    let session_id = pane.session_id.clone()?;
    let Some(SessionTarget::Ssh(target)) = pane.target.as_ref() else {
        return None;
    };
    Some((
        target.host_id.clone(),
        session_id,
        SessionOwner::Ui {
            tab_id: tab_id.clone(),
            pane_id,
        },
    ))
}

/// 建立不可变 current scope；forward 可以为空，但仍用主机与 pane 身份隔离异步读取。
fn current_scope(
    selected_host_id: Option<HostId>,
    selected_forward_id: Option<String>,
    target: Option<(HostId, SessionId, SessionOwner)>,
) -> Option<ProxyCurrentScope> {
    let (host_id, session_id, owner) = target?;
    Some(ProxyCurrentScope {
        selected_host_id,
        host_id,
        forward_id: selected_forward_id,
        session_id,
        owner,
    })
}

/// 将可信 shell tracker 投影为操作门禁；屏幕文字和旧 UI 焦点都不参与能力推断。
fn current_availability(context: &TerminalCommandContext) -> ProxyCurrentAvailability {
    match context.availability {
        TerminalCommandAvailability::Available
            if context.prompt_ready && context.prompt_line_empty && !context.alternate_screen =>
        {
            ProxyCurrentAvailability::Ready
        }
        TerminalCommandAvailability::Unavailable(
            TerminalCommandUnavailableReason::ShellIntegrationDisabled
            | TerminalCommandUnavailableReason::ShellIntegrationNotObserved,
        ) => ProxyCurrentAvailability::ShellIntegrationUnavailable,
        TerminalCommandAvailability::Unavailable(
            TerminalCommandUnavailableReason::SessionNotRunning,
        ) => ProxyCurrentAvailability::SessionNotReady,
        _ => ProxyCurrentAvailability::PromptNotReady,
    }
}

/// 只有 reconcile 回读能证明已应用、已恢复或从未跟踪时才解锁下一次用户操作。
fn current_application_from_readback(
    request: &CurrentTerminalProxyRequest,
    result: &TerminalProxyReconcileResult,
) -> ProxyCurrentApplication {
    if result.forward_id != request.forward_id
        || result.session_id != request.binding.session_id
        || result.session_generation != request.binding.session_generation
    {
        return ProxyCurrentApplication::OutcomeUnknown;
    }
    match &result.outcome {
        TerminalProxyReconcileOutcome::Applied
        | TerminalProxyReconcileOutcome::AppliedWithChanges => ProxyCurrentApplication::Applied,
        TerminalProxyReconcileOutcome::Restored
        | TerminalProxyReconcileOutcome::NotApplied
        | TerminalProxyReconcileOutcome::NoTrackedState => ProxyCurrentApplication::NotApplied,
        TerminalProxyReconcileOutcome::PartiallyRestored
        | TerminalProxyReconcileOutcome::SessionGenerationEnded
        | TerminalProxyReconcileOutcome::Busy
        | TerminalProxyReconcileOutcome::Unknown { .. }
        | TerminalProxyReconcileOutcome::ProbeFailed { .. }
        | TerminalProxyReconcileOutcome::NotAvailable { .. } => {
            ProxyCurrentApplication::OutcomeUnknown
        }
    }
}

/// 恢复/核对请求必须匹配回读出的原转发、完整 owner/generation 和预期状态。
///
/// @author kongweiguang
fn current_recovery_request_matches(
    recovery_forward_id: Option<&str>,
    current: &ProxyCurrentSession,
    requested_forward_id: &str,
    binding: &UiSessionBinding,
    expected_application: ProxyCurrentApplication,
) -> bool {
    recovery_forward_id == Some(requested_forward_id)
        && current.binding == *binding
        && current.application == expected_application
}

/// 只有仍持有可恢复事务的回执才携带原 ID；终态删除和代次结束不伪造恢复能力。
///
/// @author kongweiguang
fn recovery_forward_id_from_readback(
    request: &CurrentTerminalProxyRequest,
    result: &TerminalProxyReconcileResult,
) -> Option<String> {
    if request.forward_id != result.forward_id
        || request.binding.session_id != result.session_id
        || request.binding.session_generation != result.session_generation
        || matches!(
            &result.outcome,
            TerminalProxyReconcileOutcome::Restored
                | TerminalProxyReconcileOutcome::PartiallyRestored
                | TerminalProxyReconcileOutcome::NotApplied
                | TerminalProxyReconcileOutcome::NoTrackedState
                | TerminalProxyReconcileOutcome::SessionGenerationEnded
        )
    {
        None
    } else {
        Some(request.forward_id.clone())
    }
}

/// 从 session service 取 generation、回读 shell marker，再确认读取期间 session 未重连；无原记录时保持空态。
///
/// @author kongweiguang
async fn read_current_proxy_session(
    proxy: TerminalProxyApplication,
    sessions: Arc<dyn kerminal_application::SessionService>,
    scope: ProxyCurrentScope,
    cancellation: CancellationToken,
) -> kerminal_application::AppResult<ProxyCurrentReadback> {
    let initial = sessions
        .terminal_command_context(scope.session_id.clone())
        .await?;
    if initial.session_id != scope.session_id || initial.owner != scope.owner {
        return Err(AppError::conflict("代理目标已切换到其它 UI 会话"));
    }
    let binding = UiSessionBinding {
        session_id: initial.session_id.clone(),
        owner: initial.owner.clone(),
        session_generation: initial.session_generation,
    };
    let reconciliation = if let Some(result) = proxy
        .reconcile_unique_current_application(binding.clone(), cancellation.clone())
        .await?
    {
        Some((
            CurrentTerminalProxyRequest {
                forward_id: result.forward_id.clone(),
                binding: binding.clone(),
            },
            result,
        ))
    } else if let Some(forward_id) = scope.forward_id.clone() {
        let request = CurrentTerminalProxyRequest {
            forward_id,
            binding: binding.clone(),
        };
        let result = proxy
            .reconcile_current(request.clone(), cancellation.clone())
            .await?;
        Some((request, result))
    } else {
        None
    };

    let verified = sessions
        .terminal_command_context(scope.session_id.clone())
        .await?;
    if verified.session_id != binding.session_id
        || verified.owner != binding.owner
        || verified.session_generation != binding.session_generation
    {
        return Err(AppError::conflict("代理状态读取期间 SSH 会话已重连"));
    }
    // 没有活动 forward 也没有唯一应用记录表示 Kerminal 无待核对事务；不伪造 Unknown 状态。
    let application = reconciliation
        .as_ref()
        .map_or(ProxyCurrentApplication::NotApplied, |(request, result)| {
            current_application_from_readback(request, result)
        });
    let recovery_forward_id = reconciliation
        .as_ref()
        .and_then(|(request, result)| recovery_forward_id_from_readback(request, result));
    Ok(ProxyCurrentReadback {
        session: ProxyCurrentSession {
            host_id: scope.host_id,
            binding,
            availability: current_availability(&verified),
            application,
            shell_dialect: verified.shell_dialect,
        },
        recovery_forward_id,
    })
}

/// 将 use case 未应用原因映射到 UI 稳定分类，不向界面泄露低层命令或 transport 文本。
/// 自动应用/重试只展示结构化安全原因，不把 shell 命令或执行输出放入工作区提示。
fn initial_proxy_outcome_message(
    outcome: &TerminalProxyApplyOutcome,
) -> Option<gpui_kit::SharedString> {
    let message = match outcome {
        TerminalProxyApplyOutcome::Applied
        | TerminalProxyApplyOutcome::Restored
        | TerminalProxyApplyOutcome::PartiallyRestored => return None,
        TerminalProxyApplyOutcome::NotApplied { reason } => match reason {
            TerminalProxyUnavailableReason::PromptNotReady => crate::locale::text(
                "终端提示符尚未就绪；可以重试原 SSH 会话的代理策略。",
                "The terminal prompt is not ready. You can retry the proxy policy for the original SSH session.",
            ),
            TerminalProxyUnavailableReason::PromptNotEmpty => crate::locale::text(
                "终端输入行非空；清空或提交当前输入后再重试。",
                "The terminal input line is not empty. Clear or submit it before retrying.",
            ),
            TerminalProxyUnavailableReason::AlternateScreen => crate::locale::text(
                "终端正在使用备用屏幕；退出该界面后可重试原代理策略。",
                "The terminal is using an alternate screen. Exit it before retrying the original proxy policy.",
            ),
            TerminalProxyUnavailableReason::ForwardNotFound => crate::locale::text(
                "未找到代理转发；请检查主机的端口转发配置。",
                "The proxy forward was not found. Check the host port-forward configuration.",
            ),
            TerminalProxyUnavailableReason::ForwardInactive => crate::locale::text(
                "代理转发当前未运行；请先启动转发。",
                "The proxy forward is not active. Start it before applying the policy.",
            ),
            TerminalProxyUnavailableReason::WrongForwardKind => crate::locale::text(
                "所选转发不是 SOCKS 代理转发。",
                "The selected forward is not a SOCKS proxy.",
            ),
            TerminalProxyUnavailableReason::NoProxyEndpoint => crate::locale::text(
                "代理转发没有可用端点。",
                "The proxy forward has no usable endpoint.",
            ),
            TerminalProxyUnavailableReason::SessionNotFound
            | TerminalProxyUnavailableReason::SessionNotRunning => crate::locale::text(
                "原终端会话不再运行，未应用代理。",
                "The original terminal session is no longer running; the proxy was not applied.",
            ),
            TerminalProxyUnavailableReason::SessionBindingMismatch => crate::locale::text(
                "原终端绑定已变化，未应用代理。",
                "The original terminal binding changed; the proxy was not applied.",
            ),
            TerminalProxyUnavailableReason::TargetNotSsh => crate::locale::text(
                "该会话不是 SSH 终端，不支持此代理策略。",
                "This is not an SSH terminal; the proxy policy is unsupported.",
            ),
            TerminalProxyUnavailableReason::HostMismatch => crate::locale::text(
                "代理转发与原 SSH 主机不匹配。",
                "The proxy forward does not belong to the original SSH host.",
            ),
            TerminalProxyUnavailableReason::InitialCommandPresent => crate::locale::text(
                "此 SSH 会话带有初始命令，不会自动插入代理命令。",
                "This SSH session has an initial command, so no proxy command was injected.",
            ),
            TerminalProxyUnavailableReason::ShellIntegrationUnavailable => crate::locale::text(
                "当前 Shell 集成不可用，未应用代理。",
                "Shell integration is unavailable; the proxy was not applied.",
            ),
            TerminalProxyUnavailableReason::UnsupportedShellDialect => crate::locale::text(
                "当前 Shell 方言不支持代理脚本。",
                "The current shell dialect does not support the proxy script.",
            ),
            TerminalProxyUnavailableReason::PolicyChanged => crate::locale::text(
                "代理策略已变化；请在面板读取最新状态。",
                "The proxy policy changed. Read its latest state in the panel.",
            ),
            TerminalProxyUnavailableReason::AttemptAlreadyProcessed => crate::locale::text(
                "该代理尝试已处理，不能重复应用。",
                "This proxy attempt was already processed and cannot be replayed.",
            ),
            TerminalProxyUnavailableReason::AlreadyApplied => crate::locale::text(
                "代理已应用到此会话。",
                "The proxy is already applied to this session.",
            ),
            TerminalProxyUnavailableReason::RestoreStateUnavailable => crate::locale::text(
                "没有可验证的代理恢复状态。",
                "No verified proxy restore state is available.",
            ),
            TerminalProxyUnavailableReason::TrackingCapacityReached => crate::locale::text(
                "代理状态跟踪容量已满，未执行新的应用。",
                "Proxy state tracking is full; no new application was started.",
            ),
        },
        TerminalProxyApplyOutcome::Failed { .. } => crate::locale::text(
            "代理脚本执行失败；请在代理面板核对终端状态。",
            "The proxy script failed. Check the terminal state in the proxy panel.",
        ),
        TerminalProxyApplyOutcome::Unknown { .. } => crate::locale::text(
            "代理执行结果未知；先核对终端状态，不要再次发送脚本。",
            "The proxy result is unknown. Inspect the terminal before sending another script.",
        ),
    };
    Some(message)
}

fn unavailable_reason(reason: TerminalProxyUnavailableReason) -> ProxyUnavailableReason {
    match reason {
        TerminalProxyUnavailableReason::ForwardNotFound => ProxyUnavailableReason::ForwardNotFound,
        TerminalProxyUnavailableReason::ForwardInactive => ProxyUnavailableReason::ForwardInactive,
        TerminalProxyUnavailableReason::WrongForwardKind => {
            ProxyUnavailableReason::WrongForwardKind
        }
        TerminalProxyUnavailableReason::NoProxyEndpoint => ProxyUnavailableReason::NoProxyEndpoint,
        TerminalProxyUnavailableReason::SessionNotFound => ProxyUnavailableReason::SessionNotFound,
        TerminalProxyUnavailableReason::SessionNotRunning => {
            ProxyUnavailableReason::SessionNotRunning
        }
        TerminalProxyUnavailableReason::SessionBindingMismatch => {
            ProxyUnavailableReason::SessionBindingMismatch
        }
        TerminalProxyUnavailableReason::TargetNotSsh => ProxyUnavailableReason::TargetNotSsh,
        TerminalProxyUnavailableReason::HostMismatch => ProxyUnavailableReason::HostMismatch,
        TerminalProxyUnavailableReason::InitialCommandPresent => {
            ProxyUnavailableReason::InitialCommandPresent
        }
        TerminalProxyUnavailableReason::ShellIntegrationUnavailable => {
            ProxyUnavailableReason::ShellIntegrationUnavailable
        }
        TerminalProxyUnavailableReason::PromptNotReady => ProxyUnavailableReason::PromptNotReady,
        TerminalProxyUnavailableReason::PromptNotEmpty => ProxyUnavailableReason::PromptNotEmpty,
        TerminalProxyUnavailableReason::AlternateScreen => ProxyUnavailableReason::AlternateScreen,
        TerminalProxyUnavailableReason::UnsupportedShellDialect => {
            ProxyUnavailableReason::UnsupportedShellDialect
        }
        TerminalProxyUnavailableReason::PolicyChanged => ProxyUnavailableReason::PolicyChanged,
        TerminalProxyUnavailableReason::AttemptAlreadyProcessed => {
            ProxyUnavailableReason::AttemptAlreadyProcessed
        }
        TerminalProxyUnavailableReason::AlreadyApplied => ProxyUnavailableReason::AlreadyApplied,
        TerminalProxyUnavailableReason::RestoreStateUnavailable => {
            ProxyUnavailableReason::RestoreStateUnavailable
        }
        TerminalProxyUnavailableReason::TrackingCapacityReached => {
            ProxyUnavailableReason::TrackingCapacityReached
        }
    }
}

/// 丢弃敏感应用细节，只把已知历史写入结果和稳定错误类别展示给用户。
fn history_result(result: &TerminalProxyApplyResult) -> Option<ProxyHistoryResult> {
    match result.history_recorded {
        Some(true) => Some(ProxyHistoryResult::Recorded),
        Some(false) => Some(ProxyHistoryResult::Skipped(crate::locale::text(
            "命令历史未记录。",
            "Command history was not recorded.",
        ))),
        None if result.history_error_code.is_some() => Some(ProxyHistoryResult::Failed(
            crate::locale::text("命令历史写入失败。", "Command history could not be saved."),
        )),
        None => None,
    }
}

/// 将应用动作和独立 readback 合并为用户可理解的最终回执，未知状态保持锁定。
fn current_operation_status(
    restoring: bool,
    operation: &kerminal_application::AppResult<TerminalProxyApplyResult>,
    application: ProxyCurrentApplication,
    cancelled: bool,
) -> ProxyOperationStatus {
    match application {
        ProxyCurrentApplication::OutcomeUnknown => ProxyOperationStatus::OutcomeUnknown,
        ProxyCurrentApplication::Applied if !restoring => ProxyOperationStatus::Applied,
        ProxyCurrentApplication::NotApplied if restoring => ProxyOperationStatus::Restored,
        ProxyCurrentApplication::Applied | ProxyCurrentApplication::NotApplied => {
            if cancelled
                && operation.as_ref().is_ok_and(|result| {
                    matches!(
                        &result.outcome,
                        TerminalProxyApplyOutcome::Unknown { .. }
                            | TerminalProxyApplyOutcome::PartiallyRestored
                    )
                })
            {
                return ProxyOperationStatus::Cancelled;
            }
            match operation {
                Ok(result) => match &result.outcome {
                    TerminalProxyApplyOutcome::NotApplied { reason } => {
                        ProxyOperationStatus::NotApplied(unavailable_reason(*reason))
                    }
                    TerminalProxyApplyOutcome::Failed { exit_code } => {
                        ProxyOperationStatus::Failed(crate::locale::text(
                            format!("终端返回退出码 {exit_code}。"),
                            format!("Terminal command exited with code {exit_code}."),
                        ))
                    }
                    TerminalProxyApplyOutcome::PartiallyRestored => {
                        ProxyOperationStatus::OutcomeUnknown
                    }
                    TerminalProxyApplyOutcome::Applied | TerminalProxyApplyOutcome::Restored => {
                        ProxyOperationStatus::Failed(crate::locale::text(
                            "终端回读与代理操作回执不一致。",
                            "The terminal readback did not match the operation receipt.",
                        ))
                    }
                    TerminalProxyApplyOutcome::Unknown { .. } => {
                        ProxyOperationStatus::Failed(crate::locale::text(
                            "状态回读已完成；本次操作没有形成预期代理状态。",
                            "Readback completed, but the operation did not reach the requested proxy state.",
                        ))
                    }
                },
                Err(_error) if cancelled => ProxyOperationStatus::Cancelled,
                Err(error) => {
                    ProxyOperationStatus::Failed(error.public_message().to_owned().into())
                }
            }
        }
    }
}

impl RootView {
    /// 新建 UI SSH pane 首次 Ready 后，按取得的 session generation 应用主机的已保存代理策略。
    ///
    /// 只有直接 SSH 且无初始命令的 Ready owner 才进入此用例；异步期间会再次校验绑定，
    /// 因此切 Tab、关闭 pane 或重连不会把旧策略应用到另一个 session generation。
    ///
    /// @author kongweiguang
    pub(super) fn apply_proxy_policy_after_initial_ssh_ready(
        &mut self,
        binding: UiSessionBinding,
        cx: &mut Context<Self>,
    ) {
        let SessionOwner::Ui { tab_id, pane_id } = binding.owner.clone() else {
            return;
        };
        let session_id = binding.session_id.clone();
        let Some(pane) = self.workspace_runtime.pane(&pane_id) else {
            return;
        };
        if pane.phase != RuntimePanePhase::Ready
            || pane.tab_id != tab_id
            || pane.session_id.as_ref() != Some(&session_id)
            || !matches!(pane.target.as_ref(), Some(SessionTarget::Ssh(target)) if target.command.is_none())
        {
            return;
        }
        let Some(proxy) = self
            .services
            .operations
            .as_ref()
            .and_then(|operations| operations.terminal_proxy())
        else {
            return;
        };
        let key = ProxyTaskKey::InitialReady(session_id.clone());
        if self.proxy_runtime.is_active(&key) {
            return;
        }
        let cancellation = CancellationSlot::default();
        let ticket = self
            .proxy_runtime
            .begin(key, None, Some(cancellation.clone()));
        let executor = self.services.executor.clone();
        let sessions = self.services.sessions.clone();
        let cancellation_for_task = cancellation.clone();
        let expected_binding = binding.clone();
        let completion_binding = binding.clone();
        let task_session_id = session_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |token| {
                    cancellation_for_task.install(token.clone());
                    async move {
                        if token.is_cancelled() {
                            return Err(AppError::cancelled("自动代理应用已取消"));
                        }
                        let context = sessions
                            .terminal_command_context(task_session_id.clone())
                            .await?;
                        if context.session_id != expected_binding.session_id
                            || context.owner != expected_binding.owner
                            || context.session_generation != expected_binding.session_generation
                        {
                            return Ok(None);
                        }
                        proxy.on_ui_session_ready(expected_binding, token).await
                    }
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                if !root.proxy_runtime.is_current(&ticket) {
                    return;
                }
                root.proxy_runtime.finish(&ticket);
                let still_same_ready_pane = root
                    .workspace_runtime
                    .pane(&pane_id)
                    .is_some_and(|pane| {
                        pane.phase == RuntimePanePhase::Ready
                            && pane.tab_id == tab_id
                            && pane.session_id.as_ref() == Some(&session_id)
                            && matches!(pane.target.as_ref(), Some(SessionTarget::Ssh(target)) if target.command.is_none())
                    });
                if !still_same_ready_pane {
                    return;
                }
                match result {
                    Ok(Some(attempt)) => {
                        root.record_initial_proxy_attempt(completion_binding.clone(), attempt, cx);
                    }
                    Ok(None) => {}
                    Err(error) if !cancellation.is_cancelled() => {
                        root.initial_proxy_notice = Some(InitialProxyNotice {
                            binding: completion_binding.clone(),
                            message: error.public_message().to_owned().into(),
                            retry_intent: None,
                            retrying: false,
                        });
                    }
                    Err(_) => {}
                }
                root.refresh_proxy_panel(cx);
                cx.notify();
            });
        });
        task.detach();
    }

    /// 显示自动应用/重试的结构化安全结果，不把未知执行结论当成可重放请求。
    fn record_initial_proxy_attempt(
        &mut self,
        binding: UiSessionBinding,
        attempt: FutureProxyAttemptResult,
        cx: &mut Context<Self>,
    ) {
        let FutureProxyAttemptResult {
            result,
            retry_intent,
        } = attempt;
        if result.session_id != binding.session_id
            || result.session_generation != binding.session_generation
        {
            self.initial_proxy_notice = Some(InitialProxyNotice {
                binding,
                message: crate::locale::text(
                    "代理结果与原终端代次不匹配；请先检查终端状态。",
                    "The proxy result does not match the captured terminal generation. Inspect the terminal first.",
                ),
                retry_intent: None,
                retrying: false,
            });
            cx.notify();
            return;
        }
        match initial_proxy_outcome_message(&result.outcome) {
            Some(message) => {
                self.initial_proxy_notice = Some(InitialProxyNotice {
                    binding,
                    message,
                    retry_intent,
                    retrying: false,
                });
            }
            None => self.initial_proxy_notice = None,
        }
        cx.notify();
    }

    /// 重试只能消费 application 按原 session binding 与策略 revision 签发的单次意图。
    pub(super) fn retry_initial_proxy_policy(&mut self, cx: &mut Context<Self>) {
        let Some(mut notice) = self.initial_proxy_notice.take() else {
            return;
        };
        if notice.retrying {
            self.initial_proxy_notice = Some(notice);
            return;
        }
        let Some(intent) = notice.retry_intent.take() else {
            self.initial_proxy_notice = Some(notice);
            return;
        };
        if !self.initial_proxy_binding_is_current(&notice.binding) {
            notice.message = crate::locale::text(
                "原 SSH 会话已关闭或代次变化，不能改用当前终端重试。",
                "The original SSH session is closed or changed; retry will not switch to the current terminal.",
            );
            notice.retrying = false;
            notice.retry_intent = None;
            self.initial_proxy_notice = Some(notice);
            cx.notify();
            return;
        }
        let Some(proxy) = self
            .services
            .operations
            .as_ref()
            .and_then(|operations| operations.terminal_proxy())
        else {
            notice.message = crate::locale::text(
                "代理服务不可用；本次重试授权已消费。",
                "The proxy service is unavailable; this retry authorization was consumed.",
            );
            notice.retrying = false;
            self.initial_proxy_notice = Some(notice);
            cx.notify();
            return;
        };
        let binding = notice.binding.clone();
        notice.message = crate::locale::text(
            "正在重试原 SSH 会话的代理策略…",
            "Retrying the proxy policy for the original SSH session…",
        );
        notice.retrying = true;
        self.initial_proxy_notice = Some(notice);
        let cancellation = CancellationSlot::default();
        let ticket = self.proxy_runtime.begin(
            ProxyTaskKey::InitialReady(binding.session_id.clone()),
            None,
            Some(cancellation.clone()),
        );
        let executor = self.services.executor.clone();
        let cancellation_for_task = cancellation.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |token| {
                    cancellation_for_task.install(token.clone());
                    async move {
                        if token.is_cancelled() {
                            return Err(AppError::cancelled("代理策略重试已取消"));
                        }
                        proxy.retry_not_applied(intent, token).await
                    }
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                if !root.proxy_runtime.is_current(&ticket) {
                    return;
                }
                root.proxy_runtime.finish(&ticket);
                let same_notice = root
                    .initial_proxy_notice
                    .as_ref()
                    .is_some_and(|notice| notice.binding == binding && notice.retrying);
                if !same_notice {
                    return;
                }
                if !root.initial_proxy_binding_is_current(&binding) {
                    root.initial_proxy_notice = Some(InitialProxyNotice {
                        binding,
                        message: crate::locale::text(
                            "原 SSH 会话已关闭或代次变化；重试结果未转用其它终端。",
                            "The original SSH session changed; the result was not applied to another terminal.",
                        ),
                        retry_intent: None,
                        retrying: false,
                    });
                } else {
                    match result {
                        Ok(attempt) => {
                            root.record_initial_proxy_attempt(binding, attempt, cx);
                        }
                        Err(error) if !cancellation.is_cancelled() => {
                            root.initial_proxy_notice = Some(InitialProxyNotice {
                                binding,
                                message: error.public_message().to_owned().into(),
                                retry_intent: None,
                                retrying: false,
                            });
                        }
                        Err(_) => {
                            root.initial_proxy_notice = Some(InitialProxyNotice {
                                binding,
                                message: crate::locale::text("代理策略重试已取消。", "Proxy policy retry was cancelled."),
                                retry_intent: None,
                                retrying: false,
                            });
                        }
                    }
                }
                root.refresh_proxy_panel(cx);
                cx.notify();
            });
        });
        task.detach();
    }

    /// 确认自动代理结果仍属于同一个 ready SSH pane；新选中 Tab 不会替代其绑定。
    pub(super) fn initial_proxy_binding_is_current(&self, binding: &UiSessionBinding) -> bool {
        let kerminal_domain::sessions::SessionOwner::Ui { tab_id, pane_id } = &binding.owner else {
            return false;
        };
        let pane_is_current = self.workspace_runtime.pane(pane_id).is_some_and(|pane| {
            pane.phase == RuntimePanePhase::Ready
                && pane.tab_id == *tab_id
                && pane.session_id.as_ref() == Some(&binding.session_id)
                && matches!(
                    pane.target.as_ref(),
                    Some(SessionTarget::Ssh(target)) if target.command.is_none()
                )
        });
        if !pane_is_current {
            return false;
        }
        let registered = self
            .pane_input_handlers
            .get(&binding.session_id)
            .map(|registration| &registration.binding);
        let cached = self
            .terminal_command_contexts
            .get(&binding.session_id)
            .map(|context| &context.binding);
        match (registered, cached) {
            (Some(registered), Some(cached)) => registered == binding && cached == binding,
            (Some(registered), None) => registered == binding,
            (None, Some(cached)) => cached == binding,
            // 普通失焦会暂时撤销 pane handler；不据此改绑重试，opaque application token 仍做代次 CAS。
            (None, None) => true,
        }
    }

    /// 逐个刷新主运维页和固定 Ports 面板；每个实体保留独立选择与迟到回执门禁。
    pub(super) fn refresh_proxy_panel(&mut self, cx: &mut Context<Self>) {
        let panels = self.proxy_panels();
        if panels.is_empty() {
            return;
        }
        let proxy = self
            .services
            .operations
            .as_ref()
            .and_then(|operations| operations.terminal_proxy());
        for panel in panels {
            self.refresh_proxy_panel_entity(panel, proxy.clone(), cx);
        }
    }

    /// 返回 Root 当前注册的所有独立代理实体；不同宿主不复用可变叶子状态。
    fn proxy_panels(&self) -> Vec<Entity<ProxyApplicationPanel>> {
        self.proxy_panel
            .iter()
            .cloned()
            .chain(self.tool_rail_proxy_panels.values().cloned())
            .collect()
    }

    /// 对单个代理面板同步 session 与 host policy 状态，任务键始终绑定该 panel Entity。
    fn refresh_proxy_panel_entity(
        &mut self,
        panel: Entity<ProxyApplicationPanel>,
        proxy: Option<TerminalProxyApplication>,
        cx: &mut Context<Self>,
    ) {
        let panel_id = panel.entity_id();
        let Some(proxy) = proxy else {
            panel.update(cx, |panel, cx| {
                panel.set_current_session(None, None, cx);
                panel.set_future_projection(
                    ProxyFutureProjection::Unavailable { host_id: None },
                    cx,
                );
            });
            self.proxy_runtime.cancel_stale_current(panel_id, None);
            self.proxy_runtime.cancel_future_read(panel_id);
            return;
        };

        let selected_host_id = panel.read(cx).host_id().cloned();
        let selected_forward_id = panel.read(cx).selected_forward_id().map(str::to_owned);
        let scope = current_scope(
            selected_host_id.clone(),
            selected_forward_id,
            active_ui_ssh_target(self),
        );
        self.proxy_runtime
            .cancel_stale_current(panel_id, scope.as_ref());
        match scope {
            Some(scope) => {
                if !self.proxy_runtime.has_current_operation(&scope) {
                    panel.update(cx, |panel, cx| panel.set_current_session(None, None, cx));
                    self.refresh_current_proxy_state(panel.clone(), proxy.clone(), scope, cx);
                }
            }
            None => {
                panel.update(cx, |panel, cx| panel.set_current_session(None, None, cx));
            }
        }

        match selected_host_id {
            Some(host_id) => {
                panel.update(cx, |panel, cx| {
                    panel.set_future_projection(
                        ProxyFutureProjection::Loading {
                            host_id: host_id.clone(),
                        },
                        cx,
                    );
                });
                self.refresh_future_proxy_state(panel, proxy, host_id, cx);
            }
            None => {
                self.proxy_runtime.cancel_future_read(panel_id);
                panel.update(cx, |panel, cx| {
                    panel.set_future_projection(
                        ProxyFutureProjection::Unavailable { host_id: None },
                        cx,
                    );
                });
            }
        }
    }

    /// 按明确 panel source 分发意图；任何 session 操作都再次校验焦点 owner 与选择快照。
    pub(super) fn handle_proxy_panel_event(
        &mut self,
        source: Entity<ProxyApplicationPanel>,
        event: &ProxyPanelEvent,
        cx: &mut Context<Self>,
    ) {
        if !self.proxy_panel_is_current(&source) {
            return;
        }
        match event {
            ProxyPanelEvent::SelectionChanged {
                host_id,
                forward_id,
            } => {
                let selected = source.read(cx);
                if selected.host_id() == host_id.as_ref()
                    && selected.selected_forward_id() == forward_id.as_deref()
                {
                    self.refresh_proxy_panel(cx);
                }
            }
            ProxyPanelEvent::ApplyCurrentRequested {
                request_id,
                forward_id,
                binding,
            } => self.start_current_proxy_operation(
                source,
                *request_id,
                forward_id.clone(),
                binding.clone(),
                false,
                cx,
            ),
            ProxyPanelEvent::RestoreCurrentRequested {
                request_id,
                forward_id,
                binding,
            } => self.start_current_proxy_operation(
                source,
                *request_id,
                forward_id.clone(),
                binding.clone(),
                true,
                cx,
            ),
            ProxyPanelEvent::ReconcileCurrentRequested {
                request_id,
                forward_id,
                binding,
            } => self.start_current_proxy_reconciliation(
                source,
                *request_id,
                forward_id.clone(),
                binding.clone(),
                cx,
            ),
            ProxyPanelEvent::SetFutureRequested {
                request_id,
                host_id,
                expected_revision,
                forward_id,
            } => self.start_future_proxy_operation(
                source,
                *request_id,
                host_id.clone(),
                *expected_revision,
                forward_id.clone(),
                cx,
            ),
            ProxyPanelEvent::ReviewProfileScriptRequested {
                request_id,
                forward_id,
                shell_dialect,
            } => self.start_proxy_script_review(
                source,
                *request_id,
                forward_id.clone(),
                *shell_dialect,
                cx,
            ),
            ProxyPanelEvent::CancelRequested { request_id } => {
                self.proxy_runtime
                    .cancel_panel_request(source.entity_id(), *request_id);
            }
        }
    }

    /// 当前 binding 查询只对齐到真实 ready pane，SFTP、Agent、后台及非 SSH pane 均返回 false。
    fn proxy_current_scope_matches(
        &self,
        source: &Entity<ProxyApplicationPanel>,
        scope: &ProxyCurrentScope,
        cx: &Context<Self>,
    ) -> bool {
        if !self.proxy_panel_is_current(source) {
            return false;
        }
        let Some((host_id, session_id, owner)) = active_ui_ssh_target(self) else {
            return false;
        };
        let selected = source.read(cx);
        host_id == scope.host_id
            && session_id == scope.session_id
            && owner == scope.owner
            && selected.host_id() == scope.selected_host_id.as_ref()
            && selected.selected_forward_id() == scope.forward_id.as_deref()
    }

    /// EntityId 由 Root 注册的独立 panel 决定，迟到 subscription 不会操作已替换的面板。
    fn proxy_panel_is_current(&self, source: &Entity<ProxyApplicationPanel>) -> bool {
        self.proxy_panel
            .as_ref()
            .is_some_and(|panel| panel.entity_id() == source.entity_id())
            || self
                .tool_rail_proxy_panels
                .values()
                .any(|panel| panel.entity_id() == source.entity_id())
    }

    /// 读取 selected host 的未来策略；切换主机只取消读取，不会取消已进入 CAS 的写操作。
    fn refresh_future_proxy_state(
        &mut self,
        panel: Entity<ProxyApplicationPanel>,
        proxy: TerminalProxyApplication,
        host_id: HostId,
        cx: &mut Context<Self>,
    ) {
        let panel_id = panel.entity_id();
        let cancellation = CancellationSlot::default();
        let ticket = self.proxy_runtime.begin(
            ProxyTaskKey::FutureRead(panel_id),
            None,
            Some(cancellation.clone()),
        );
        let executor = self.services.executor.clone();
        let cancellation_for_task = cancellation.clone();
        let task = cx.spawn(async move |this, cx| {
            let query_host = host_id.clone();
            let result = executor
                .run(move |token| {
                    cancellation_for_task.install(token.clone());
                    async move {
                        if token.is_cancelled() {
                            return Err(AppError::cancelled("代理策略读取已取消"));
                        }
                        let state = proxy.future_state(query_host.clone()).await?;
                        if token.is_cancelled() {
                            return Err(AppError::cancelled("代理策略读取已取消"));
                        }
                        Ok(state)
                    }
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                if !root.proxy_runtime.is_current(&ticket) {
                    return;
                }
                root.proxy_runtime.finish(&ticket);
                if !root.proxy_panel_is_current(&panel)
                    || panel.read(cx).host_id() != Some(&host_id)
                {
                    return;
                }
                panel.update(cx, |panel, cx| match result {
                    Ok(state) if state.host_id == host_id => panel.set_future_projection(
                        ProxyFutureProjection::Ready {
                            host_id: state.host_id,
                            forward_id: state.forward_id,
                            revision: state.revision,
                        },
                        cx,
                    ),
                    Ok(_) | Err(_) => panel.set_future_projection(
                        ProxyFutureProjection::Unavailable {
                            host_id: Some(host_id.clone()),
                        },
                        cx,
                    ),
                });
            });
        });
        task.detach();
    }

    /// 首次/焦点/转发变化时重新读回 shell 状态；初始值未知，禁止按隧道元数据伪造 NotApplied。
    fn refresh_current_proxy_state(
        &mut self,
        panel: Entity<ProxyApplicationPanel>,
        proxy: TerminalProxyApplication,
        scope: ProxyCurrentScope,
        cx: &mut Context<Self>,
    ) {
        let panel_id = panel.entity_id();
        let cancellation = CancellationSlot::default();
        let ticket = self.proxy_runtime.begin(
            ProxyTaskKey::CurrentRead(panel_id),
            Some(scope.clone()),
            Some(cancellation.clone()),
        );
        let executor = self.services.executor.clone();
        let sessions = self.services.sessions.clone();
        let cancellation_for_task = cancellation.clone();
        let task = cx.spawn(async move |this, cx| {
            let query_proxy = proxy.clone();
            let query_scope = scope.clone();
            let query_sessions = sessions.clone();
            let result = executor
                .run(move |token| {
                    cancellation_for_task.install(token.clone());
                    async move {
                        if token.is_cancelled() {
                            return Err(AppError::cancelled("代理状态读取已取消"));
                        }
                        read_current_proxy_session(query_proxy, query_sessions, query_scope, token)
                            .await
                    }
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                if !root.proxy_runtime.is_current(&ticket) {
                    return;
                }
                root.proxy_runtime.finish(&ticket);
                if !root.proxy_current_scope_matches(&panel, &scope, cx) {
                    return;
                }
                match result {
                    Ok(readback) => {
                        panel.update(cx, |panel, cx| {
                            panel.set_current_session(
                                Some(readback.session),
                                readback.recovery_forward_id,
                                cx,
                            );
                        });
                    }
                    Err(error) => {
                        panel.update(cx, |panel, cx| {
                            panel.set_current_session(None, None, cx);
                        });
                        root.status = Some(error.public_message().to_owned().into());
                    }
                }
            });
        });
        task.detach();
    }

    /// Unknown 核对只检查原事务并复验会话代次，不触发 apply 或 restore 副作用。
    ///
    /// @author kongweiguang
    fn start_current_proxy_reconciliation(
        &mut self,
        source: Entity<ProxyApplicationPanel>,
        request_id: u64,
        forward_id: String,
        binding: UiSessionBinding,
        cx: &mut Context<Self>,
    ) {
        let Some(proxy) = self
            .services
            .operations
            .as_ref()
            .and_then(|operations| operations.terminal_proxy())
        else {
            source.update(cx, |panel, cx| {
                panel.finish_current_operation(
                    request_id,
                    ProxyOperationStatus::Failed(crate::locale::text(
                        "代理服务不可用。",
                        "Proxy service is unavailable.",
                    )),
                    None,
                    cx,
                );
            });
            return;
        };
        let selected = source.read(cx);
        let Some(current) = selected.current_session().cloned() else {
            source.update(cx, |panel, cx| {
                panel.finish_current_operation(
                    request_id,
                    ProxyOperationStatus::Failed(crate::locale::text(
                        "当前 SSH 会话已失效，请刷新后重试。",
                        "The current SSH session is no longer available. Refresh and retry.",
                    )),
                    None,
                    cx,
                );
            });
            return;
        };
        let selected_forward_id = selected.selected_forward_id().map(str::to_owned);
        let scope = ProxyCurrentScope {
            selected_host_id: selected.host_id().cloned(),
            host_id: current.host_id.clone(),
            forward_id: selected_forward_id,
            session_id: binding.session_id.clone(),
            owner: binding.owner.clone(),
        };
        if !current_recovery_request_matches(
            selected.current_recovery_forward_id(),
            &current,
            &forward_id,
            &binding,
            ProxyCurrentApplication::OutcomeUnknown,
        ) || current.availability != ProxyCurrentAvailability::Ready
            || !self.proxy_current_scope_matches(&source, &scope, cx)
        {
            source.update(cx, |panel, cx| {
                panel.finish_current_operation(
                    request_id,
                    ProxyOperationStatus::Cancelled,
                    None,
                    cx,
                );
            });
            return;
        }

        let panel_id = source.entity_id();
        self.proxy_runtime
            .cancel_stale_current(panel_id, Some(&scope));
        let cancellation = CancellationSlot::default();
        let ticket = self.proxy_runtime.begin(
            ProxyTaskKey::PanelRequest(panel_id, request_id),
            Some(scope.clone()),
            Some(cancellation.clone()),
        );
        let executor = self.services.executor.clone();
        let sessions = self.services.sessions.clone();
        let request = CurrentTerminalProxyRequest {
            forward_id,
            binding,
        };
        let cancellation_for_task = cancellation.clone();
        let task = cx.spawn(async move |this, cx| {
            let query_proxy = proxy;
            let query_request = request.clone();
            let query_current = current.clone();
            let query_sessions = sessions.clone();
            let reconciliation = executor
                .run(move |token| {
                    cancellation_for_task.install(token.clone());
                    async move {
                        if token.is_cancelled() {
                            return Err(AppError::cancelled("代理状态核对已取消"));
                        }
                        let result = query_proxy
                            .reconcile_current(query_request.clone(), token)
                            .await?;
                        let verified = query_sessions
                            .terminal_command_context(query_request.binding.session_id.clone())
                            .await?;
                        if verified.session_id != query_request.binding.session_id
                            || verified.owner != query_request.binding.owner
                            || verified.session_generation
                                != query_request.binding.session_generation
                        {
                            return Err(AppError::conflict("代理状态核对期间 SSH 会话已重连"));
                        }
                        let application =
                            current_application_from_readback(&query_request, &result);
                        let recovery_forward_id =
                            recovery_forward_id_from_readback(&query_request, &result);
                        Ok(ProxyCurrentReadback {
                            session: ProxyCurrentSession {
                                host_id: query_current.host_id,
                                binding: query_current.binding,
                                availability: current_availability(&verified),
                                application,
                                shell_dialect: verified.shell_dialect,
                            },
                            recovery_forward_id,
                        })
                    }
                })
                .await;

            let _ = this.update(cx, |root, cx| {
                if !root.proxy_runtime.is_current(&ticket) {
                    return;
                }
                root.proxy_runtime.finish(&ticket);
                if !root.proxy_panel_is_current(&source) {
                    return;
                }
                if !root.proxy_current_scope_matches(&source, &scope, cx) {
                    source.update(cx, |panel, cx| {
                        panel.finish_current_operation(
                            request_id,
                            if cancellation.is_cancelled() {
                                ProxyOperationStatus::Cancelled
                            } else {
                                ProxyOperationStatus::OutcomeUnknown
                            },
                            None,
                            cx,
                        );
                    });
                    return;
                }
                source.update(cx, |panel, cx| {
                    let status = match &reconciliation {
                        Ok(readback) => match readback.session.application {
                            ProxyCurrentApplication::Applied => ProxyOperationStatus::Reconciled,
                            ProxyCurrentApplication::NotApplied => {
                                ProxyOperationStatus::NotApplied(
                                    ProxyUnavailableReason::RestoreStateUnavailable,
                                )
                            }
                            ProxyCurrentApplication::OutcomeUnknown => {
                                ProxyOperationStatus::OutcomeUnknown
                            }
                        },
                        Err(_) if cancellation.is_cancelled() => ProxyOperationStatus::Cancelled,
                        Err(error) => {
                            root.status = Some(error.public_message().to_owned().into());
                            ProxyOperationStatus::Failed(error.public_message().to_owned().into())
                        }
                    };
                    if let Ok(readback) = &reconciliation {
                        panel.set_current_session(
                            Some(readback.session.clone()),
                            readback.recovery_forward_id.clone(),
                            cx,
                        );
                    }
                    panel.finish_current_operation(request_id, status, None, cx);
                });
            });
        });
        task.detach();
    }

    /// Apply/restore 共用冻结 binding；恢复 ID 与当前列表选择分离，避免转发停止后丢失 owner。
    ///
    /// @author kongweiguang
    fn start_current_proxy_operation(
        &mut self,
        source: Entity<ProxyApplicationPanel>,
        request_id: u64,
        forward_id: String,
        binding: UiSessionBinding,
        restoring: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(proxy) = self
            .services
            .operations
            .as_ref()
            .and_then(|operations| operations.terminal_proxy())
        else {
            source.update(cx, |panel, cx| {
                panel.finish_current_operation(
                    request_id,
                    ProxyOperationStatus::Failed(crate::locale::text(
                        "代理服务不可用。",
                        "Proxy service is unavailable.",
                    )),
                    None,
                    cx,
                );
            });
            return;
        };
        let selected = source.read(cx);
        let Some(current) = selected.current_session().cloned() else {
            source.update(cx, |panel, cx| {
                panel.finish_current_operation(
                    request_id,
                    ProxyOperationStatus::Failed(crate::locale::text(
                        "当前 SSH 会话已失效，请刷新后重试。",
                        "The current SSH session is no longer available. Refresh and retry.",
                    )),
                    None,
                    cx,
                );
            });
            return;
        };
        let selected_forward_id = selected.selected_forward_id().map(str::to_owned);
        let target_matches = if restoring {
            current_recovery_request_matches(
                selected.current_recovery_forward_id(),
                &current,
                &forward_id,
                &binding,
                ProxyCurrentApplication::Applied,
            )
        } else {
            selected_forward_id.as_deref() == Some(forward_id.as_str())
                && current.binding == binding
                && current.application == ProxyCurrentApplication::NotApplied
        };
        if !target_matches
            || !self.proxy_current_scope_matches(
                &source,
                &ProxyCurrentScope {
                    selected_host_id: selected.host_id().cloned(),
                    host_id: current.host_id.clone(),
                    forward_id: selected_forward_id.clone(),
                    session_id: binding.session_id.clone(),
                    owner: binding.owner.clone(),
                },
                cx,
            )
        {
            source.update(cx, |panel, cx| {
                panel.finish_current_operation(
                    request_id,
                    ProxyOperationStatus::Cancelled,
                    None,
                    cx,
                );
            });
            return;
        }
        let current_recovery = current.clone();
        let recovery_forward_id = selected.current_recovery_forward_id().map(str::to_owned);
        let scope = ProxyCurrentScope {
            selected_host_id: selected.host_id().cloned(),
            host_id: current.host_id.clone(),
            forward_id: selected_forward_id,
            session_id: binding.session_id.clone(),
            owner: binding.owner.clone(),
        };
        let panel_id = source.entity_id();
        self.proxy_runtime
            .cancel_stale_current(panel_id, Some(&scope));
        let cancellation = CancellationSlot::default();
        let ticket = self.proxy_runtime.begin(
            ProxyTaskKey::PanelRequest(panel_id, request_id),
            Some(scope.clone()),
            Some(cancellation.clone()),
        );
        let executor = self.services.executor.clone();
        let sessions = self.services.sessions.clone();
        let request = CurrentTerminalProxyRequest {
            forward_id,
            binding,
        };
        let cancellation_for_task = cancellation.clone();
        let task = cx.spawn(async move |this, cx| {
            let operation_proxy = proxy.clone();
            let operation_request = request.clone();
            let operation_result = executor
                .run(move |token| {
                    cancellation_for_task.install(token.clone());
                    async move {
                        if token.is_cancelled() {
                            return Err(AppError::cancelled("代理操作已取消"));
                        }
                        if restoring {
                            operation_proxy
                                .restore_current(operation_request, token)
                                .await
                        } else {
                            operation_proxy
                                .apply_current(operation_request, token)
                                .await
                        }
                    }
                })
                .await;

            let readback_proxy = proxy;
            let readback_request = request.clone();
            let readback_scope = scope.clone();
            let readback = executor
                .run(move |token| async move {
                    read_current_proxy_session(readback_proxy, sessions, readback_scope, token)
                        .await
                        .and_then(|current| {
                            if current.session.binding.session_id
                                == readback_request.binding.session_id
                                && current.session.binding.owner == readback_request.binding.owner
                                && current.session.binding.session_generation
                                    == readback_request.binding.session_generation
                            {
                                Ok(current)
                            } else {
                                Err(AppError::conflict("代理操作回读与原会话代次不匹配"))
                            }
                        })
                })
                .await;

            let _ = this.update(cx, |root, cx| {
                if !root.proxy_runtime.is_current(&ticket) {
                    return;
                }
                root.proxy_runtime.finish(&ticket);
                if root.proxy_panel_is_current(&source) {
                    if root.proxy_current_scope_matches(&source, &scope, cx) {
                        source.update(cx, |panel, cx| {
                            if let Ok(readback) = &readback {
                                panel.set_current_session(
                                    Some(readback.session.clone()),
                                    readback.recovery_forward_id.clone(),
                                    cx,
                                );
                            } else if restoring {
                                let mut current = current_recovery.clone();
                                current.application = ProxyCurrentApplication::OutcomeUnknown;
                                panel.set_current_session(
                                    Some(current),
                                    recovery_forward_id.clone(),
                                    cx,
                                );
                            } else {
                                panel.set_current_session(None, None, cx);
                            }
                            let application = readback
                                .as_ref()
                                .map_or(ProxyCurrentApplication::OutcomeUnknown, |current| {
                                    current.session.application
                                });
                            let history = operation_result.as_ref().ok().and_then(history_result);
                            let cancelled = cancellation.is_cancelled();
                            let status = current_operation_status(
                                restoring,
                                &operation_result,
                                application,
                                cancelled,
                            );
                            panel.finish_current_operation(request_id, status, history, cx);
                        });
                    } else {
                        let status = if cancellation.is_cancelled() {
                            ProxyOperationStatus::Cancelled
                        } else {
                            ProxyOperationStatus::OutcomeUnknown
                        };
                        source.update(cx, |panel, cx| {
                            panel.finish_current_operation(request_id, status, None, cx);
                        });
                    }
                    if let Err(error) = readback {
                        root.status = Some(error.public_message().to_owned().into());
                    }
                }
                // 多个代理面板可能指向同一 shell；动作完成后让所有实体各自回读权威状态。
                root.refresh_proxy_panel(cx);
            });
        });
        task.detach();
    }

    /// Future policy 是 CAS 写入且不可取消；无论写入回执如何都立即读取权威状态，未知时不重放。
    fn start_future_proxy_operation(
        &mut self,
        source: Entity<ProxyApplicationPanel>,
        request_id: u64,
        host_id: HostId,
        expected_revision: u64,
        forward_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if !self.proxy_panel_is_current(&source) || source.read(cx).host_id() != Some(&host_id) {
            source.update(cx, |panel, cx| {
                panel.set_operation_status(request_id, ProxyOperationStatus::Cancelled, cx);
            });
            return;
        }
        let Some(proxy) = self
            .services
            .operations
            .as_ref()
            .and_then(|operations| operations.terminal_proxy())
        else {
            source.update(cx, |panel, cx| {
                panel.set_future_projection(
                    ProxyFutureProjection::OutcomeUnknown {
                        host_id: host_id.clone(),
                    },
                    cx,
                );
                panel.set_operation_status(request_id, ProxyOperationStatus::OutcomeUnknown, cx);
            });
            return;
        };
        let panel_id = source.entity_id();
        self.proxy_runtime.cancel_future_read(panel_id);
        let ticket =
            self.proxy_runtime
                .begin(ProxyTaskKey::PanelRequest(panel_id, request_id), None, None);
        let executor = self.services.executor.clone();
        let request = FutureProxyApplyRequest {
            host_id: host_id.clone(),
            expected_revision,
            forward_id,
        };
        let task = cx.spawn(async move |this, cx| {
            let write_proxy = proxy.clone();
            let write_request = request.clone();
            let read_host = host_id.clone();
            let result = executor
                .run(move |_| async move {
                    let write = write_proxy.set_future(write_request).await;
                    let readback = write_proxy.future_state(read_host).await;
                    Ok((write, readback))
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                if !root.proxy_runtime.is_current(&ticket) {
                    return;
                }
                root.proxy_runtime.finish(&ticket);
                if !root.proxy_panel_is_current(&source)
                    || source.read(cx).host_id() != Some(&host_id)
                {
                    return;
                }
                root.proxy_runtime.cancel_future_read(source.entity_id());
                source.update(cx, |panel, cx| match result {
                    Ok((_, Ok(state))) if state.host_id == host_id => {
                        let projection = ProxyFutureProjection::Ready {
                            host_id: state.host_id,
                            forward_id: state.forward_id,
                            revision: state.revision,
                        };
                        if !panel.set_future_result(request_id, projection.clone(), cx) {
                            panel.set_future_projection(projection, cx);
                        }
                    }
                    Ok((_, Ok(_))) | Ok((_, Err(_))) | Err(_) => {
                        panel.set_future_projection(
                            ProxyFutureProjection::OutcomeUnknown {
                                host_id: host_id.clone(),
                            },
                            cx,
                        );
                        panel.set_operation_status(
                            request_id,
                            ProxyOperationStatus::OutcomeUnknown,
                            cx,
                        );
                    }
                });
                // Future policy 由任一 Ports 面板更新后，刷新其它独立面板的 revision 投影。
                root.refresh_proxy_panel(cx);
            });
        });
        task.detach();
    }

    /// 只生成可审阅脚本；事件必须仍绑定当前 SSH pane、所选 SOCKS 转发和可信 shell 方言。
    fn start_proxy_script_review(
        &mut self,
        source: Entity<ProxyApplicationPanel>,
        request_id: u64,
        forward_id: String,
        shell_dialect: RemoteShellDialect,
        cx: &mut Context<Self>,
    ) {
        let current = source.read(cx).current_session().cloned();
        let Some(current) = current else {
            source.update(cx, |panel, cx| {
                panel.set_operation_status(request_id, ProxyOperationStatus::Cancelled, cx);
            });
            return;
        };
        let scope = ProxyCurrentScope {
            selected_host_id: source.read(cx).host_id().cloned(),
            host_id: current.host_id.clone(),
            forward_id: Some(forward_id.clone()),
            session_id: current.binding.session_id.clone(),
            owner: current.binding.owner.clone(),
        };
        if current.shell_dialect != shell_dialect
            || source.read(cx).selected_forward_id() != Some(forward_id.as_str())
            || !self.proxy_current_scope_matches(&source, &scope, cx)
        {
            source.update(cx, |panel, cx| {
                panel.set_operation_status(request_id, ProxyOperationStatus::Cancelled, cx);
            });
            return;
        }
        let Some(proxy) = self
            .services
            .operations
            .as_ref()
            .and_then(|operations| operations.terminal_proxy())
        else {
            source.update(cx, |panel, cx| {
                panel.set_operation_status(
                    request_id,
                    ProxyOperationStatus::Failed(crate::locale::text(
                        "代理服务不可用。",
                        "Proxy service is unavailable.",
                    )),
                    cx,
                );
            });
            return;
        };
        let cancellation = CancellationSlot::default();
        let ticket = self.proxy_runtime.begin(
            ProxyTaskKey::PanelRequest(source.entity_id(), request_id),
            Some(scope.clone()),
            Some(cancellation.clone()),
        );
        let executor = self.services.executor.clone();
        let cancellation_for_task = cancellation.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |token| {
                    cancellation_for_task.install(token.clone());
                    async move {
                        if token.is_cancelled() {
                            return Err(AppError::cancelled("代理脚本审阅已取消"));
                        }
                        let result = proxy.reviewable_scripts(forward_id, shell_dialect).await;
                        if token.is_cancelled() {
                            return Err(AppError::cancelled("代理脚本审阅已取消"));
                        }
                        result
                    }
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                if !root.proxy_runtime.is_current(&ticket) {
                    return;
                }
                root.proxy_runtime.finish(&ticket);
                if !root.proxy_panel_is_current(&source) {
                    return;
                }
                let still_current = root.proxy_current_scope_matches(&source, &scope, cx);
                source.update(cx, |panel, cx| {
                    if !still_current {
                        panel.set_operation_status(request_id, ProxyOperationStatus::Cancelled, cx);
                    } else {
                        match result {
                            Ok(_script) if cancellation.is_cancelled() => panel
                                .set_operation_status(
                                    request_id,
                                    ProxyOperationStatus::Cancelled,
                                    cx,
                                ),
                            Ok(script) => panel.set_review_script(
                                request_id,
                                ProxyReviewScript {
                                    shell_dialect: script.shell_dialect,
                                    apply: script.apply_script,
                                    restore: script.restore_script,
                                },
                                cx,
                            ),
                            Err(error) => panel.set_operation_status(
                                request_id,
                                if cancellation.is_cancelled() {
                                    ProxyOperationStatus::Cancelled
                                } else {
                                    ProxyOperationStatus::Failed(
                                        error.public_message().to_owned().into(),
                                    )
                                },
                                cx,
                            ),
                        }
                    }
                });
            });
        });
        task.detach();
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_proxy_runtime.rs"]
mod tests;
