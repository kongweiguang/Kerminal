//! SOCKS 代理应用控制叶子；只消费显式快照并向 Root 发布带原始绑定的意图。
//!
//! @author kongweiguang

use gpui_kit::{Context, EventEmitter, SharedString};
use kerminal_application::{operations::PortForwardSnapshot, sessions::UiSessionBinding};
use kerminal_domain::{
    connections::RemoteShellDialect,
    identity::{HostId, SessionId},
    operations::{ForwardKind, ForwardState},
    sessions::SessionOwner,
};

/// Root 在产生快照时冻结的终端身份；按钮点击不再查询当前活动 Tab。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProxyCurrentSession {
    pub host_id: HostId,
    pub binding: UiSessionBinding,
    pub availability: ProxyCurrentAvailability,
    pub application: ProxyCurrentApplication,
    /// 仅来自可信运行态元数据；未知方言不作为 profile 脚本默认值。
    pub shell_dialect: RemoteShellDialect,
}

/// 当前 shell 中最近一次代理回执的投影；未知结果必须由 Root 刷新后才能再操作。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyCurrentApplication {
    NotApplied,
    Applied,
    OutcomeUnknown,
}

/// 当前终端不可应用代理时的已知原因，避免 UI 根据屏幕内容或活动标签猜测能力。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyCurrentAvailability {
    Ready,
    SessionNotReady,
    ShellIntegrationUnavailable,
    PromptNotReady,
    UnsupportedTarget,
}

/// Future policy 只展示应用层回读结果；UI 不把隧道启动或旧 scope 元数据当作已启用。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProxyFutureProjection {
    Loading {
        host_id: HostId,
    },
    Ready {
        host_id: HostId,
        forward_id: Option<String>,
        revision: u64,
    },
    OutcomeUnknown {
        host_id: HostId,
    },
    Unavailable {
        host_id: Option<HostId>,
    },
}

/// 已由应用层生成、可供人工审阅的用户配置脚本；面板只展示和复制，不会写入用户配置。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProxyReviewScript {
    pub shell_dialect: RemoteShellDialect,
    pub apply: String,
    pub restore: String,
}

/// 应用层回执经 Root 映射为 UI 投影；包含精确终端身份，避免旧请求污染新 pane。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProxyCurrentOperationResult {
    pub forward_id: String,
    pub session_id: SessionId,
    pub session_generation: u64,
    pub outcome: ProxyCurrentResultOutcome,
    pub history: Option<ProxyHistoryResult>,
}

/// Current apply/restore 的结果分类与应用层语义保持一一对应。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProxyCurrentResultOutcome {
    Applied,
    Restored,
    Failed { exit_code: i32 },
    OutcomeUnknown,
    NotApplied(ProxyUnavailableReason),
}

/// 命令历史回执单独展示，不把历史写入失败误报成 shell 代理应用失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProxyHistoryResult {
    Recorded,
    NotSubmitted,
    Skipped(SharedString),
    Failed(SharedString),
    Unavailable,
}

/// Root 向叶子同步的只读投影；所有会话绑定必须来自同一时刻的 runtime 快照。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProxyPanelProjection {
    pub host_id: Option<HostId>,
    pub forwards: Vec<PortForwardSnapshot>,
    pub current_session: Option<ProxyCurrentSession>,
    pub future: ProxyFutureProjection,
}

impl Default for ProxyFutureProjection {
    /// 缺少策略回读时禁用修改操作，而不是猜测未启用或继承历史调用意图。
    fn default() -> Self {
        Self::Unavailable { host_id: None }
    }
}

/// 叶子只发出明确的应用操作；Root 持有取消令牌并调用统一的 TerminalProxyApplication。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProxyPanelEvent {
    /// 选中的活动 SOCKS forward 变化后，Root 必须重新核对当前 shell 的跟踪状态。
    SelectionChanged {
        host_id: Option<HostId>,
        forward_id: Option<String>,
    },
    ApplyCurrentRequested {
        request_id: u64,
        forward_id: String,
        binding: UiSessionBinding,
    },
    RestoreCurrentRequested {
        request_id: u64,
        forward_id: String,
        binding: UiSessionBinding,
    },
    ReconcileCurrentRequested {
        request_id: u64,
        forward_id: String,
        binding: UiSessionBinding,
    },
    SetFutureRequested {
        request_id: u64,
        host_id: HostId,
        expected_revision: u64,
        forward_id: Option<String>,
    },
    ReviewProfileScriptRequested {
        request_id: u64,
        forward_id: String,
        shell_dialect: RemoteShellDialect,
    },
    CancelRequested {
        request_id: u64,
    },
}

/// 回执状态刻意区分“未应用”和“提交结果未知”；未知时不发起自动重试。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProxyOperationStatus {
    Applying,
    Applied,
    NotApplied(ProxyUnavailableReason),
    OutcomeUnknown,
    Failed(SharedString),
    CancelRequested,
    Cancelled,
    FutureEnabled,
    FutureDisabled,
    ScriptReady,
    Restored,
    Reconciled,
}

/// 用例未执行或拒绝代理应用时，展示稳定原因并允许用户继续查看安全脚本。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyUnavailableReason {
    NoExplicitSession,
    SessionNotFound,
    SessionNotReady,
    SessionNotRunning,
    ShellIntegrationUnavailable,
    PromptNotReady,
    PromptNotEmpty,
    AlternateScreen,
    UnsupportedTarget,
    TargetNotSsh,
    HostMismatch,
    SessionBindingMismatch,
    InitialCommandPresent,
    UnsupportedShellDialect,
    ForwardUnavailable,
    ForwardNotFound,
    ForwardInactive,
    WrongForwardKind,
    NoProxyEndpoint,
    FutureIntegrationUnavailable,
    PolicyChanged,
    AttemptAlreadyProcessed,
    AlreadyApplied,
    AlreadyRestored,
    RestoreStateUnavailable,
    TrackingCapacityReached,
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProxyOperationKind {
    ApplyCurrent,
    RestoreCurrent,
    ReconcileCurrent,
    SetFuture,
    ReviewScript,
}

#[derive(Clone, Debug)]
struct PendingProxyOperation {
    request_id: u64,
    kind: ProxyOperationKind,
    host_id: Option<HostId>,
    forward_name: String,
    forward_id: Option<String>,
    binding: Option<UiSessionBinding>,
    expected_revision: Option<u64>,
    desired_future_forward: Option<Option<String>>,
    cancellable: bool,
}

/// 一次代理操作的完整目标与并发预期，避免调用时拆散需共同冻结的上下文。
///
/// @author kongweiguang
struct ProxyOperationRequest {
    kind: ProxyOperationKind,
    host_id: Option<HostId>,
    forward_name: String,
    forward_id: Option<String>,
    binding: Option<UiSessionBinding>,
    expected_revision: Option<u64>,
    desired_future_forward: Option<Option<String>>,
    cancellable: bool,
}

/// SOCKS 控制区只持有用户选择、请求序号和回执展示，不拥有 SSH 或端口转发资源。
pub struct ProxyApplicationPanel {
    projection: ProxyPanelProjection,
    selected_forward_id: Option<String>,
    recovery_forward_id: Option<String>,
    pending: Option<PendingProxyOperation>,
    next_request_id: u64,
    last_status: Option<ProxyOperationStatus>,
    last_status_context: Option<(Option<HostId>, String)>,
    last_history_result: Option<ProxyHistoryResult>,
    review_script: Option<ProxyReviewScript>,
}

impl EventEmitter<ProxyPanelEvent> for ProxyApplicationPanel {}

impl ProxyApplicationPanel {
    /// Profile 脚本只对可信 shell 方言开放；Unknown 不能被静默当作 POSIX。
    ///
    /// @author kongweiguang
    fn profile_script_supported(&self) -> bool {
        self.projection
            .current_session
            .as_ref()
            .is_some_and(|session| session.shell_dialect != RemoteShellDialect::Unknown)
    }

    /// 初始化为只读、未应用状态；构造器不访问 Root、终端服务或用户配置。
    pub fn new(projection: ProxyPanelProjection) -> Self {
        let selected_forward_id = eligible_socks_forwards(&projection)
            .first()
            .map(|forward| forward.forward.id.clone());
        Self {
            projection,
            selected_forward_id,
            recovery_forward_id: None,
            pending: None,
            next_request_id: 0,
            last_status: None,
            last_status_context: None,
            last_history_result: None,
            review_script: None,
        }
    }

    /// 同步只读快照并保留仍有效的选择；待处理请求继续绑定原主机和原 session generation。
    pub fn set_projection(&mut self, projection: ProxyPanelProjection, cx: &mut Context<Self>) {
        let previous_host_id = self.projection.host_id.clone();
        let previous_forward_id = self.selected_forward_id.clone();
        let visible_ids = eligible_socks_forwards(&projection)
            .into_iter()
            .map(|forward| forward.forward.id.as_str())
            .collect::<Vec<_>>();
        if self
            .selected_forward_id
            .as_deref()
            .is_none_or(|selected| !visible_ids.contains(&selected))
        {
            self.selected_forward_id = eligible_socks_forwards(&projection)
                .first()
                .map(|forward| forward.forward.id.clone());
        }
        if self.projection.host_id != projection.host_id {
            self.recovery_forward_id = None;
            self.review_script = None;
            self.last_status = None;
            self.last_status_context = None;
            self.last_history_result = None;
        }
        self.projection = projection;
        if previous_host_id != self.projection.host_id
            || previous_forward_id != self.selected_forward_id
        {
            self.emit_selection_changed(cx);
        }
        cx.notify();
    }

    /// OperationsView 仅同步运行列表；空态可收起展示，但异步 owner 和 Root 的 session binding 保持有效。
    ///
    /// @author kongweiguang
    pub fn set_host_forwards(
        &mut self,
        host_id: Option<HostId>,
        forwards: Vec<PortForwardSnapshot>,
        cx: &mut Context<Self>,
    ) {
        let previous_host_id = self.projection.host_id.clone();
        let previous_forward_id = self.selected_forward_id.clone();
        if self.projection.host_id != host_id {
            self.projection.current_session = None;
            self.recovery_forward_id = None;
            self.projection.future = ProxyFutureProjection::Unavailable {
                host_id: host_id.clone(),
            };
            self.review_script = None;
            self.last_status = None;
            self.last_status_context = None;
            self.last_history_result = None;
        }
        self.projection.host_id = host_id;
        self.projection.forwards = forwards;
        if self.selected_forward_id.as_deref().is_none_or(|selected| {
            !eligible_socks_forwards(&self.projection)
                .iter()
                .any(|forward| forward.forward.id == selected)
        }) {
            self.selected_forward_id = eligible_socks_forwards(&self.projection)
                .first()
                .map(|forward| forward.forward.id.clone());
        }
        if previous_host_id != self.projection.host_id
            || previous_forward_id != self.selected_forward_id
        {
            self.emit_selection_changed(cx);
        }
        cx.notify();
    }

    /// 返回投影主机，Root 通过它读取同一主机的未来策略，不能借用全局连接选择。
    pub fn host_id(&self) -> Option<&HostId> {
        self.projection.host_id.as_ref()
    }

    /// 公开当前人工选择，Root 用同一 ID 做 reconcile；不能用列表首项代替用户选择。
    pub fn selected_forward_id(&self) -> Option<&str> {
        self.selected_forward_id.as_deref()
    }

    /// 返回当前冻结的 UI binding，Root 用于拒绝过期的脚本审阅事件。
    pub fn current_session(&self) -> Option<&ProxyCurrentSession> {
        self.projection.current_session.as_ref()
    }

    /// Root 只可使用回读证明的原转发 ID；停止态转发不会由列表选择重新推断。
    pub fn current_recovery_forward_id(&self) -> Option<&str> {
        self.recovery_forward_id.as_deref()
    }

    /// Root 同步冻结 binding 与回读证明的恢复 ID；面板不从活动列表重建历史事务。
    ///
    /// @author kongweiguang
    pub fn set_current_session(
        &mut self,
        current_session: Option<ProxyCurrentSession>,
        recovery_forward_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.projection.current_session != current_session
            || self.recovery_forward_id != recovery_forward_id
        {
            self.review_script = None;
        }
        self.projection.current_session = current_session;
        self.recovery_forward_id = recovery_forward_id;
        cx.notify();
    }

    /// Root 只回写与当前请求序号相同的结果，迟到回执不会覆盖后续操作状态。
    pub fn set_operation_status(
        &mut self,
        request_id: u64,
        status: ProxyOperationStatus,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        if pending.request_id != request_id {
            return;
        }
        let pending = pending.clone();
        if matches!(
            &status,
            ProxyOperationStatus::Applying | ProxyOperationStatus::CancelRequested
        ) {
            self.last_status = Some(status);
        } else {
            self.last_status_context =
                Some((pending.host_id.clone(), pending.forward_name.clone()));
            if status == ProxyOperationStatus::OutcomeUnknown {
                self.mark_operation_outcome_unknown(&pending);
            }
            self.last_status = Some(status);
            self.pending = None;
        }
        cx.notify();
    }

    /// 完成应用、恢复或只读核对并保留独立历史回执，避免异步状态互相覆盖。
    ///
    /// @author kongweiguang
    pub fn finish_current_operation(
        &mut self,
        request_id: u64,
        status: ProxyOperationStatus,
        history: Option<ProxyHistoryResult>,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        if pending.request_id != request_id
            || !matches!(
                pending.kind,
                ProxyOperationKind::ApplyCurrent
                    | ProxyOperationKind::RestoreCurrent
                    | ProxyOperationKind::ReconcileCurrent
            )
        {
            return;
        }
        let pending = pending.clone();
        self.last_status_context = Some((pending.host_id.clone(), pending.forward_name.clone()));
        self.last_history_result = history;
        if status == ProxyOperationStatus::OutcomeUnknown {
            self.mark_operation_outcome_unknown(&pending);
        }
        self.last_status = Some(status);
        self.pending = None;
        cx.notify();
    }

    /// 只接受 request ID、forward 和完整 session binding 均匹配的 current 回执。
    ///
    /// 身份错配视为结果未知并释放 busy 状态；禁止把旧 pane 的成功写入当前会话投影。
    pub fn set_current_result(
        &mut self,
        request_id: u64,
        result: ProxyCurrentOperationResult,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(pending) = self.pending.as_ref() else {
            return false;
        };
        if pending.request_id != request_id
            || !matches!(
                pending.kind,
                ProxyOperationKind::ApplyCurrent | ProxyOperationKind::RestoreCurrent
            )
        {
            return false;
        }
        let pending = pending.clone();
        self.last_status_context = Some((pending.host_id.clone(), pending.forward_name.clone()));
        if !pending_current_result_matches(&pending, &result)
            || !current_result_kind_matches(&pending, &result.outcome)
        {
            self.last_status = Some(ProxyOperationStatus::OutcomeUnknown);
            self.last_history_result = None;
            self.mark_current_application_unknown(&pending);
            self.pending = None;
            cx.notify();
            return true;
        }

        self.last_history_result = result.history;
        match result.outcome {
            ProxyCurrentResultOutcome::Applied => {
                self.last_status = Some(ProxyOperationStatus::Applied);
                self.update_current_application(&pending, ProxyCurrentApplication::Applied);
            }
            ProxyCurrentResultOutcome::Restored => {
                self.last_status = Some(ProxyOperationStatus::Restored);
                self.update_current_application(&pending, ProxyCurrentApplication::NotApplied);
            }
            ProxyCurrentResultOutcome::Failed { exit_code } => {
                self.last_status = Some(ProxyOperationStatus::Failed(crate::locale::text(
                    format!("终端返回退出码 {exit_code}。"),
                    format!("Terminal command exited with code {exit_code}."),
                )));
            }
            ProxyCurrentResultOutcome::OutcomeUnknown => {
                self.last_status = Some(ProxyOperationStatus::OutcomeUnknown);
                self.mark_current_application_unknown(&pending);
            }
            ProxyCurrentResultOutcome::NotApplied(reason) => {
                self.last_status = Some(ProxyOperationStatus::NotApplied(reason));
            }
        }
        self.pending = None;
        cx.notify();
        true
    }

    /// 只接受当前人工请求的脚本回执，并保留在 UI 供审阅或复制。
    pub fn set_review_script(
        &mut self,
        request_id: u64,
        script: ProxyReviewScript,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        if pending.request_id != request_id || pending.kind != ProxyOperationKind::ReviewScript {
            return;
        }
        if pending.host_id != self.projection.host_id {
            self.pending = None;
            cx.notify();
            return;
        }
        self.review_script = Some(script);
        self.last_status_context = Some((pending.host_id.clone(), pending.forward_name.clone()));
        self.last_status = Some(ProxyOperationStatus::ScriptReady);
        self.pending = None;
        cx.notify();
    }

    /// 更新未来终端策略的应用层回读，阻止表面 toggle 状态取代持久策略事实。
    pub fn set_future_projection(&mut self, future: ProxyFutureProjection, cx: &mut Context<Self>) {
        if future
            .host_id()
            .is_some_and(|host_id| self.projection.host_id.as_ref() != Some(host_id))
        {
            return;
        }
        self.projection.future = future;
        cx.notify();
    }

    /// 校验 Future policy 的 CAS 回执 revision 与期望值；迟到或竞态结果不会显示为成功。
    pub fn set_future_result(
        &mut self,
        request_id: u64,
        state: ProxyFutureProjection,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(pending) = self.pending.as_ref() else {
            return false;
        };
        if pending.request_id != request_id || pending.kind != ProxyOperationKind::SetFuture {
            return false;
        }
        let pending = pending.clone();
        let ProxyFutureProjection::Ready {
            host_id,
            forward_id,
            revision,
        } = &state
        else {
            return false;
        };
        let result_host_id = host_id.clone();
        let result_forward_id = forward_id.clone();
        let result_revision = *revision;
        if pending.host_id.as_ref() != Some(&result_host_id) {
            return false;
        }
        let result_matches = future_result_matches(
            &pending,
            &result_host_id,
            result_forward_id.as_deref(),
            result_revision,
        );
        self.last_status_context = Some((pending.host_id.clone(), pending.forward_name.clone()));
        if self.projection.host_id.as_ref() == Some(&result_host_id) {
            self.projection.future = state;
        }
        self.last_status = Some(if result_matches {
            if result_forward_id.is_some() {
                ProxyOperationStatus::FutureEnabled
            } else {
                ProxyOperationStatus::FutureDisabled
            }
        } else {
            ProxyOperationStatus::NotApplied(ProxyUnavailableReason::PolicyChanged)
        });
        self.pending = None;
        cx.notify();
        true
    }

    /// 当前会话绑定变更时旧脚本不再作为新目标的建议展示。
    pub fn clear_review_script(&mut self, cx: &mut Context<Self>) {
        self.review_script = None;
        self.last_history_result = None;
        cx.notify();
    }

    /// 创建唯一在途操作；请求 ID 允许 Root 忽略旧请求的迟到异步回执。
    ///
    /// @author kongweiguang
    fn begin_operation(&mut self, request: ProxyOperationRequest) -> Option<u64> {
        if self.pending.is_some() {
            return None;
        }
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        let request_id = self.next_request_id;
        self.last_status_context = Some((request.host_id.clone(), request.forward_name.clone()));
        self.pending = Some(PendingProxyOperation {
            request_id,
            kind: request.kind,
            host_id: request.host_id,
            forward_name: request.forward_name,
            forward_id: request.forward_id,
            binding: request.binding,
            expected_revision: request.expected_revision,
            desired_future_forward: request.desired_future_forward,
            cancellable: request.cancellable,
        });
        self.last_status = Some(ProxyOperationStatus::Applying);
        self.last_history_result = None;
        Some(request_id)
    }

    /// 取消仅请求 Root 停止当前应用任务；直到用例回执到达前仍保留 busy 状态。
    fn request_cancel(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.as_mut() else {
            return;
        };
        if !pending.cancellable
            || matches!(
                self.last_status,
                Some(ProxyOperationStatus::CancelRequested)
            )
        {
            return;
        }
        pending.cancellable = false;
        self.last_status = Some(ProxyOperationStatus::CancelRequested);
        cx.emit(ProxyPanelEvent::CancelRequested {
            request_id: pending.request_id,
        });
        cx.notify();
    }

    /// 用户选择必须仍存在于当前 Active 动态转发投影，旧的停止/失败转发不能被提交。
    fn selected_forward(&self) -> Option<&PortForwardSnapshot> {
        let selected = self.selected_forward_id.as_deref()?;
        eligible_socks_forwards(&self.projection)
            .into_iter()
            .find(|forward| forward.forward.id == selected)
    }

    /// 当前 Apply 只携带拍摄快照时的 UiSessionBinding，要求同主机 SSH 且真实 shell integration 可用。
    fn current_binding(&self) -> Result<UiSessionBinding, ProxyUnavailableReason> {
        let forward = self
            .selected_forward()
            .ok_or(ProxyUnavailableReason::ForwardUnavailable)?;
        let current = self
            .projection
            .current_session
            .as_ref()
            .ok_or(ProxyUnavailableReason::NoExplicitSession)?;
        if current.host_id != forward.forward.config.host_id
            || self.projection.host_id.as_ref() != Some(&current.host_id)
            || !matches!(&current.binding.owner, SessionOwner::Ui { .. })
        {
            return Err(ProxyUnavailableReason::UnsupportedTarget);
        }
        if current.application == ProxyCurrentApplication::OutcomeUnknown {
            return Err(ProxyUnavailableReason::OutcomeUnknown);
        }
        match current.availability {
            ProxyCurrentAvailability::Ready => Ok(current.binding.clone()),
            ProxyCurrentAvailability::SessionNotReady => {
                Err(ProxyUnavailableReason::SessionNotReady)
            }
            ProxyCurrentAvailability::ShellIntegrationUnavailable => {
                Err(ProxyUnavailableReason::ShellIntegrationUnavailable)
            }
            ProxyCurrentAvailability::PromptNotReady => Err(ProxyUnavailableReason::PromptNotReady),
            ProxyCurrentAvailability::UnsupportedTarget => {
                Err(ProxyUnavailableReason::UnsupportedTarget)
            }
        }
    }

    /// 恢复按原记录定位而非活动转发；同时要求 UI owner、主机、generation 与提示符状态仍可信。
    fn recovery_binding(
        &self,
        expected_application: ProxyCurrentApplication,
    ) -> Result<(String, HostId, UiSessionBinding), ProxyUnavailableReason> {
        let current = self
            .projection
            .current_session
            .as_ref()
            .ok_or(ProxyUnavailableReason::NoExplicitSession)?;
        let forward_id = self
            .recovery_forward_id
            .clone()
            .ok_or(ProxyUnavailableReason::RestoreStateUnavailable)?;
        if self.projection.host_id.as_ref() != Some(&current.host_id)
            || !matches!(&current.binding.owner, SessionOwner::Ui { .. })
        {
            return Err(ProxyUnavailableReason::UnsupportedTarget);
        }
        if current.application != expected_application {
            return Err(match expected_application {
                ProxyCurrentApplication::Applied => ProxyUnavailableReason::AlreadyRestored,
                ProxyCurrentApplication::OutcomeUnknown => ProxyUnavailableReason::OutcomeUnknown,
                ProxyCurrentApplication::NotApplied => ProxyUnavailableReason::AlreadyApplied,
            });
        }
        match current.availability {
            ProxyCurrentAvailability::Ready => {}
            ProxyCurrentAvailability::SessionNotReady => {
                return Err(ProxyUnavailableReason::SessionNotReady);
            }
            ProxyCurrentAvailability::ShellIntegrationUnavailable => {
                return Err(ProxyUnavailableReason::ShellIntegrationUnavailable);
            }
            ProxyCurrentAvailability::PromptNotReady => {
                return Err(ProxyUnavailableReason::PromptNotReady);
            }
            ProxyCurrentAvailability::UnsupportedTarget => {
                return Err(ProxyUnavailableReason::UnsupportedTarget);
            }
        }
        Ok((forward_id, current.host_id.clone(), current.binding.clone()))
    }

    /// 把明确的转发目标和冻结 session binding 发送给 Root，不读取绘制时活动 pane。
    fn apply_to_current(&mut self, cx: &mut Context<Self>) {
        let Some(forward) = self.selected_forward().cloned() else {
            self.set_local_status(
                ProxyOperationStatus::NotApplied(ProxyUnavailableReason::ForwardUnavailable),
                cx,
            );
            return;
        };
        let binding = match self.current_binding() {
            Ok(binding) => binding,
            Err(reason) => {
                self.set_local_status(ProxyOperationStatus::NotApplied(reason), cx);
                return;
            }
        };
        if self
            .projection
            .current_session
            .as_ref()
            .is_some_and(|session| session.application == ProxyCurrentApplication::Applied)
        {
            self.set_local_status(
                ProxyOperationStatus::NotApplied(ProxyUnavailableReason::AlreadyApplied),
                cx,
            );
            return;
        }
        let Some(request_id) = self.begin_operation(ProxyOperationRequest {
            kind: ProxyOperationKind::ApplyCurrent,
            host_id: Some(forward.forward.config.host_id.clone()),
            forward_name: forward.forward.config.name.clone(),
            forward_id: Some(forward.forward.id.clone()),
            binding: Some(binding.clone()),
            expected_revision: None,
            desired_future_forward: None,
            cancellable: true,
        }) else {
            return;
        };
        cx.emit(ProxyPanelEvent::ApplyCurrentRequested {
            request_id,
            forward_id: forward.forward.id,
            binding,
        });
        cx.notify();
    }

    /// 只恢复唯一回读出的原记录，不要求原转发仍活动，避免停止隧道后丢失清理入口。
    fn restore_current(&mut self, cx: &mut Context<Self>) {
        let (forward_id, host_id, binding) =
            match self.recovery_binding(ProxyCurrentApplication::Applied) {
                Ok(target) => target,
                Err(reason) => {
                    self.set_local_status(ProxyOperationStatus::NotApplied(reason), cx);
                    return;
                }
            };
        let Some(request_id) = self.begin_operation(ProxyOperationRequest {
            kind: ProxyOperationKind::RestoreCurrent,
            host_id: Some(host_id),
            forward_name: format!("原转发 {forward_id}"),
            forward_id: Some(forward_id.clone()),
            binding: Some(binding.clone()),
            expected_revision: None,
            desired_future_forward: None,
            cancellable: true,
        }) else {
            return;
        };
        cx.emit(ProxyPanelEvent::RestoreCurrentRequested {
            request_id,
            forward_id,
            binding,
        });
        cx.notify();
    }

    /// 未知状态只对原事务做只读核对；核对完成前不发起恢复或新的应用。
    fn reconcile_current(&mut self, cx: &mut Context<Self>) {
        let (forward_id, host_id, binding) =
            match self.recovery_binding(ProxyCurrentApplication::OutcomeUnknown) {
                Ok(target) => target,
                Err(reason) => {
                    self.set_local_status(ProxyOperationStatus::NotApplied(reason), cx);
                    return;
                }
            };
        let Some(request_id) = self.begin_operation(ProxyOperationRequest {
            kind: ProxyOperationKind::ReconcileCurrent,
            host_id: Some(host_id),
            forward_name: format!("核对原代理记录 {forward_id}"),
            forward_id: Some(forward_id.clone()),
            binding: Some(binding.clone()),
            expected_revision: None,
            desired_future_forward: None,
            cancellable: true,
        }) else {
            return;
        };
        cx.emit(ProxyPanelEvent::ReconcileCurrentRequested {
            request_id,
            forward_id,
            binding,
        });
        cx.notify();
    }

    /// 修改未来应用策略时传递选中 SOCKS forward 和同一主机 ID，禁止将当前 Tab 当作目标。
    fn toggle_future(&mut self, cx: &mut Context<Self>) {
        let Some(forward) = self.selected_forward().cloned() else {
            self.set_local_status(
                ProxyOperationStatus::NotApplied(ProxyUnavailableReason::ForwardUnavailable),
                cx,
            );
            return;
        };
        let host_id = forward.forward.config.host_id.clone();
        if self.projection.host_id.as_ref() != Some(&host_id) {
            self.set_local_status(
                ProxyOperationStatus::NotApplied(ProxyUnavailableReason::UnsupportedTarget),
                cx,
            );
            return;
        }
        let (revision, desired_forward) =
            match desired_future_target(&host_id, &forward.forward.id, &self.projection.future) {
                Ok(target) => target,
                Err(reason) => {
                    self.set_local_status(ProxyOperationStatus::NotApplied(reason), cx);
                    return;
                }
            };
        let Some(request_id) = self.begin_operation(ProxyOperationRequest {
            kind: ProxyOperationKind::SetFuture,
            host_id: Some(host_id.clone()),
            forward_name: forward.forward.config.name.clone(),
            forward_id: Some(forward.forward.id.clone()),
            binding: None,
            expected_revision: Some(revision),
            desired_future_forward: Some(desired_forward.clone()),
            cancellable: false,
        }) else {
            return;
        };
        cx.emit(ProxyPanelEvent::SetFutureRequested {
            request_id,
            host_id,
            expected_revision: revision,
            forward_id: desired_forward,
        });
        cx.notify();
    }

    /// 请求生成配置示例前要求同一主机有可信 shell 方言；未知时不回退到 POSIX。
    fn request_profile_script(&mut self, cx: &mut Context<Self>) {
        let Some(forward) = self.selected_forward().cloned() else {
            self.set_local_status(
                ProxyOperationStatus::NotApplied(ProxyUnavailableReason::ForwardUnavailable),
                cx,
            );
            return;
        };
        let Some(current) = self.projection.current_session.as_ref() else {
            self.set_local_status(
                ProxyOperationStatus::NotApplied(ProxyUnavailableReason::NoExplicitSession),
                cx,
            );
            return;
        };
        if current.host_id != forward.forward.config.host_id {
            self.set_local_status(
                ProxyOperationStatus::NotApplied(ProxyUnavailableReason::UnsupportedTarget),
                cx,
            );
            return;
        }
        if current.shell_dialect == RemoteShellDialect::Unknown {
            self.set_local_status(
                ProxyOperationStatus::NotApplied(ProxyUnavailableReason::UnsupportedTarget),
                cx,
            );
            return;
        }
        let dialect = current.shell_dialect;
        let Some(request_id) = self.begin_operation(ProxyOperationRequest {
            kind: ProxyOperationKind::ReviewScript,
            host_id: Some(current.host_id.clone()),
            forward_name: forward.forward.config.name.clone(),
            forward_id: Some(forward.forward.id.clone()),
            binding: Some(current.binding.clone()),
            expected_revision: None,
            desired_future_forward: None,
            cancellable: true,
        }) else {
            return;
        };
        cx.emit(ProxyPanelEvent::ReviewProfileScriptRequested {
            request_id,
            forward_id: forward.forward.id.clone(),
            shell_dialect: dialect,
        });
        cx.notify();
    }

    /// 本地校验失败只更新可读原因，不创建虚假的 pending 请求。
    fn set_local_status(&mut self, status: ProxyOperationStatus, cx: &mut Context<Self>) {
        if self.pending.is_none() {
            self.last_status_context = self.selected_forward().map(|forward| {
                (
                    Some(forward.forward.config.host_id.clone()),
                    forward.forward.config.name.clone(),
                )
            });
            self.last_status = Some(status);
            cx.notify();
        }
    }

    /// 选择器与隧道详情入口共用活动 SOCKS5 资格校验，不读取旧 proxy_apply_scope 字段。
    /// @author kongweiguang
    pub(super) fn select_forward(&mut self, forward_id: String, cx: &mut Context<Self>) {
        if eligible_socks_forwards(&self.projection)
            .iter()
            .any(|forward| forward.forward.id == forward_id)
            && self.selected_forward_id.as_deref() != Some(forward_id.as_str())
        {
            self.selected_forward_id = Some(forward_id);
            self.review_script = None;
            self.last_status = None;
            self.last_status_context = None;
            self.emit_selection_changed(cx);
            cx.notify();
        }
    }

    /// 通知 Root 只读回当前明确选择对应的 shell 状态，避免误把旧 forward 绑定带给新选择。
    fn emit_selection_changed(&self, cx: &mut Context<Self>) {
        cx.emit(ProxyPanelEvent::SelectionChanged {
            host_id: self.projection.host_id.clone(),
            forward_id: self.selected_forward_id.clone(),
        });
    }

    /// 回执仅更新仍指向原 binding 的当前会话投影，避免切换 pane 后污染新目标。
    fn update_current_application(
        &mut self,
        pending: &PendingProxyOperation,
        application: ProxyCurrentApplication,
    ) {
        let Some(binding) = pending.binding.as_ref() else {
            return;
        };
        if let Some(current) = self.projection.current_session.as_mut()
            && &current.binding == binding
        {
            current.application = application;
        }
    }

    /// Unknown 结果会锁住同一会话的再次注入入口，其他 session 的投影保持原样。
    fn mark_current_application_unknown(&mut self, pending: &PendingProxyOperation) {
        self.update_current_application(pending, ProxyCurrentApplication::OutcomeUnknown);
    }

    /// 应用用例返回通用未知错误时锁住同一操作目标，等待 Root 重新读取真实状态。
    ///
    /// @author kongweiguang
    fn mark_operation_outcome_unknown(&mut self, pending: &PendingProxyOperation) {
        match pending.kind {
            ProxyOperationKind::ApplyCurrent
            | ProxyOperationKind::RestoreCurrent
            | ProxyOperationKind::ReconcileCurrent => {
                self.mark_current_application_unknown(pending);
            }
            ProxyOperationKind::SetFuture => {
                if let Some(host_id) = pending.host_id.as_ref()
                    && self.projection.host_id.as_ref() == Some(host_id)
                {
                    self.projection.future = ProxyFutureProjection::OutcomeUnknown {
                        host_id: host_id.clone(),
                    };
                }
            }
            ProxyOperationKind::ReviewScript => {}
        }
    }
}

/// 只暴露目标主机当前活动且已返回 SOCKS endpoint 的 Dynamic/RemoteDynamic forward。
fn eligible_socks_forwards(projection: &ProxyPanelProjection) -> Vec<&PortForwardSnapshot> {
    let Some(host_id) = projection.host_id.as_ref() else {
        return Vec::new();
    };
    projection
        .forwards
        .iter()
        .filter(|snapshot| {
            snapshot.forward.config.host_id == *host_id
                && matches!(
                    snapshot.forward.config.kind,
                    ForwardKind::Dynamic | ForwardKind::RemoteDynamic
                )
                && snapshot.forward.state == ForwardState::Active
                && snapshot.proxy_url.is_some()
        })
        .collect()
}

impl ProxyFutureProjection {
    /// 每种异步状态都携带主机身份，使切换主机后的迟到回读可以直接丢弃。
    fn host_id(&self) -> Option<&HostId> {
        match self {
            Self::Loading { host_id }
            | Self::Ready { host_id, .. }
            | Self::OutcomeUnknown { host_id } => Some(host_id),
            Self::Unavailable { host_id } => host_id.as_ref(),
        }
    }
}

/// 生成 Future policy 的 CAS 修改目标；未知/加载状态不得伪造 revision 或默认关闭。
fn desired_future_target(
    host_id: &HostId,
    selected_forward_id: &str,
    projection: &ProxyFutureProjection,
) -> Result<(u64, Option<String>), ProxyUnavailableReason> {
    let ProxyFutureProjection::Ready {
        host_id: state_host_id,
        forward_id,
        revision,
    } = projection
    else {
        return Err(
            if matches!(projection, ProxyFutureProjection::OutcomeUnknown { .. }) {
                ProxyUnavailableReason::OutcomeUnknown
            } else {
                ProxyUnavailableReason::FutureIntegrationUnavailable
            },
        );
    };
    if state_host_id != host_id {
        return Err(ProxyUnavailableReason::FutureIntegrationUnavailable);
    }
    let desired = if forward_id.as_deref() == Some(selected_forward_id) {
        None
    } else {
        Some(selected_forward_id.to_owned())
    };
    Ok((*revision, desired))
}

/// 只接受完全匹配的应用回执，避免不同 forward 或旧 generation 冒充当前操作成功。
fn pending_current_result_matches(
    pending: &PendingProxyOperation,
    result: &ProxyCurrentOperationResult,
) -> bool {
    pending.forward_id.as_deref() == Some(result.forward_id.as_str())
        && pending.binding.as_ref().is_some_and(|binding| {
            binding.session_id == result.session_id
                && binding.session_generation == result.session_generation
        })
}

/// 仅把明确递增一次且目标与请求一致的 Future CAS 回执显示为成功。
fn future_result_matches(
    pending: &PendingProxyOperation,
    host_id: &HostId,
    forward_id: Option<&str>,
    revision: u64,
) -> bool {
    pending.kind == ProxyOperationKind::SetFuture
        && pending.host_id.as_ref() == Some(host_id)
        && pending
            .expected_revision
            .and_then(|expected| expected.checked_add(1))
            == Some(revision)
        && pending
            .desired_future_forward
            .as_ref()
            .is_some_and(|desired| desired.as_deref() == forward_id)
}

/// 只有 Apply 对应 Applied、Restore 对应 Restored 才接受成功结果。
fn current_result_kind_matches(
    pending: &PendingProxyOperation,
    outcome: &ProxyCurrentResultOutcome,
) -> bool {
    matches!(
        (&pending.kind, outcome),
        (
            ProxyOperationKind::ApplyCurrent,
            ProxyCurrentResultOutcome::Applied
        ) | (
            ProxyOperationKind::RestoreCurrent,
            ProxyCurrentResultOutcome::Restored
        ) | (_, ProxyCurrentResultOutcome::Failed { .. })
            | (_, ProxyCurrentResultOutcome::OutcomeUnknown)
            | (_, ProxyCurrentResultOutcome::NotApplied(_))
    )
}

#[path = "proxy_panel_render.rs"]
mod proxy_panel_render;

/// 过滤条件与 application 再次校验保持保守一致，操作层不会把旧 proxy scope 当成动作。
fn unavailable_reason_text(reason: ProxyUnavailableReason) -> SharedString {
    match reason {
        ProxyUnavailableReason::NoExplicitSession | ProxyUnavailableReason::SessionNotFound => {
            crate::locale::text(
                "没有明确的当前终端会话。",
                "No explicit current terminal session.",
            )
        }
        ProxyUnavailableReason::SessionNotReady | ProxyUnavailableReason::SessionNotRunning => {
            crate::locale::text(
                "当前 SSH 会话尚未就绪。",
                "The current SSH session is not ready.",
            )
        }
        ProxyUnavailableReason::ShellIntegrationUnavailable => crate::locale::text(
            "shell integration 未确认；可以审阅配置脚本或稍后重试。",
            "Shell integration is not confirmed. Review the profile script or retry later.",
        ),
        ProxyUnavailableReason::PromptNotReady => crate::locale::text(
            "终端当前没有可确认的命令提示符。",
            "The terminal has no confirmed command prompt.",
        ),
        ProxyUnavailableReason::PromptNotEmpty => crate::locale::text(
            "命令提示符中已有输入；请清空后再应用代理。",
            "The command prompt already contains input. Clear it before applying the proxy.",
        ),
        ProxyUnavailableReason::AlternateScreen => crate::locale::text(
            "终端正在使用备用屏幕，当前无法安全注入命令。",
            "The terminal is using the alternate screen, so command injection is unavailable.",
        ),
        ProxyUnavailableReason::UnsupportedTarget
        | ProxyUnavailableReason::TargetNotSsh
        | ProxyUnavailableReason::HostMismatch
        | ProxyUnavailableReason::SessionBindingMismatch
        | ProxyUnavailableReason::InitialCommandPresent
        | ProxyUnavailableReason::UnsupportedShellDialect => crate::locale::text(
            "目标不是当前同主机 SSH 终端。",
            "The target is not a same-host SSH terminal.",
        ),
        ProxyUnavailableReason::ForwardUnavailable
        | ProxyUnavailableReason::ForwardNotFound
        | ProxyUnavailableReason::ForwardInactive
        | ProxyUnavailableReason::WrongForwardKind
        | ProxyUnavailableReason::NoProxyEndpoint => crate::locale::text(
            "所选 SOCKS 转发已停止或不再可用。",
            "The selected SOCKS forward has stopped or is no longer available.",
        ),
        ProxyUnavailableReason::FutureIntegrationUnavailable => crate::locale::text(
            "后续终端代理策略当前不可用。",
            "Future terminal proxy policy is unavailable.",
        ),
        ProxyUnavailableReason::PolicyChanged => crate::locale::text(
            "后续终端代理策略已被其他操作修改；请重新读取后再选择。",
            "The future-terminal policy changed elsewhere. Refresh it before choosing again.",
        ),
        ProxyUnavailableReason::AttemptAlreadyProcessed => crate::locale::text(
            "此终端已经处理过该代理请求。",
            "This terminal has already processed the proxy request.",
        ),
        ProxyUnavailableReason::AlreadyApplied => crate::locale::text(
            "代理已经应用；请使用恢复操作后再重新应用。",
            "The proxy is already applied. Restore it before applying again.",
        ),
        ProxyUnavailableReason::AlreadyRestored
        | ProxyUnavailableReason::RestoreStateUnavailable => crate::locale::text(
            "当前终端没有待恢复的代理修改。",
            "The current terminal has no proxy change to restore.",
        ),
        ProxyUnavailableReason::TrackingCapacityReached => crate::locale::text(
            "代理状态跟踪容量已满；请先关闭或恢复已有操作。",
            "Proxy state tracking is full. Close or restore an existing operation first.",
        ),
        ProxyUnavailableReason::OutcomeUnknown => crate::locale::text(
            "上次操作结果未知；先刷新终端状态，不要重复发送。",
            "The previous result is unknown. Refresh terminal state before sending again.",
        ),
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/operations_proxy_panel.rs"]
mod tests;
