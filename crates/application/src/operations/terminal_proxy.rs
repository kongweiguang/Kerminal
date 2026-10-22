//! 显式代理应用用例：当前会话由真实 OSC 133 证明，后续会话只按用户策略触发。
//!
//! @author kongweiguang

mod policy;
mod reconciliation;
mod scripts;

use std::{sync::Arc, time::Duration};

use kerminal_domain::{
    commands::{CommandHistoryRecordRequest, CommandHistorySource, CommandHistoryTarget},
    connections::RemoteShellDialect,
    identity::{HostId, SessionId},
    operations::{ForwardKind, ForwardState},
    sessions::{SessionInfo, SessionLifecycle, SessionOwner, SessionTarget, SshSessionTarget},
};
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use super::port_forward::{PortForwardService, PortForwardSnapshot};
use crate::{
    AppError, AppResult, PublicErrorCode,
    commands::CommandsService,
    sessions::{
        SessionChangeKind, SessionFilter, SessionService, ShellCommandOutcome, ShellCommandRequest,
        ShellCommandUnknownReason, TerminalCommandAvailability, TerminalCommandContext,
        TerminalCommandUnavailableReason, UiSessionBinding,
    },
};

pub use policy::FutureProxyPolicyStore;
use policy::{
    ApplicationReservation, FutureAttemptClaim, FutureAttemptKey, FutureAttemptRetryClaim,
    ProxyApplicationKey, ProxyApplicationResolution, StoredProxyApplication,
};
use reconciliation::ReconcileDecision;
pub use reconciliation::{TerminalProxyReconcileOutcome, TerminalProxyReconcileResult};

const SHELL_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const FUTURE_PROMPT_WAIT: Duration = Duration::from_secs(45);

/// 后续会话策略携带 revision，避免过期面板覆盖较新的用户选择。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FutureProxyApplyRequest {
    pub host_id: HostId,
    pub expected_revision: u64,
    /// `None` 表示用户显式关闭；转发 ID 只绑定该活动 SOCKS 监听器。
    pub forward_id: Option<String>,
}

/// 每台主机独立的内存策略；初始版本为零且未选择转发。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FutureProxyApplyState {
    pub host_id: HostId,
    pub revision: u64,
    pub forward_id: Option<String>,
}

/// 当前终端操作精确绑定一个转发与一个 UI 会话代次。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CurrentTerminalProxyRequest {
    pub forward_id: String,
    pub binding: UiSessionBinding,
}

/// 供用户审阅的应用与恢复脚本只在内存生成，不修改 shell 启动文件。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewableProxyScript {
    pub shell_dialect: RemoteShellDialect,
    pub execution_context: ProxyScriptExecutionContext,
    pub apply_script: String,
    pub restore_script: String,
}

/// 明确脚本应运行在本机 shell 还是远端 shell，避免把本机 listener 当作远端代理地址。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyScriptExecutionContext {
    LocalShell,
    RemoteShell,
}

/// 描述阻止代理脚本注入的目标或能力边界，供界面准确说明原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalProxyUnavailableReason {
    ForwardNotFound,
    ForwardInactive,
    WrongForwardKind,
    NoProxyEndpoint,
    SessionNotFound,
    SessionNotRunning,
    SessionBindingMismatch,
    TargetNotSsh,
    HostMismatch,
    InitialCommandPresent,
    ShellIntegrationUnavailable,
    PromptNotReady,
    PromptNotEmpty,
    AlternateScreen,
    UnsupportedShellDialect,
    PolicyChanged,
    AttemptAlreadyProcessed,
    AlreadyApplied,
    RestoreStateUnavailable,
    TrackingCapacityReached,
}

/// 当前终端代理结果区分 shell 已完成、失败与无法判定的执行结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalProxyApplyOutcome {
    Applied,
    Restored,
    PartiallyRestored,
    Failed {
        exit_code: i32,
    },
    Unknown {
        reason: ShellCommandUnknownReason,
    },
    NotApplied {
        reason: TerminalProxyUnavailableReason,
    },
}

/// 结果分别保留 shell 执行结论与既有命令历史写入状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalProxyApplyResult {
    pub forward_id: String,
    pub session_id: SessionId,
    pub session_generation: u64,
    pub outcome: TerminalProxyApplyOutcome,
    pub history_recorded: Option<bool>,
    pub history_skip_reason: Option<String>,
    pub history_error_code: Option<PublicErrorCode>,
}

/// 保存一次明确的后续代理意图；字段私有且不实现 Clone，避免 UI 重建或复制重试授权。
pub struct FutureProxyRetryIntent {
    host_id: HostId,
    binding: UiSessionBinding,
    key: FutureAttemptKey,
    capability: uuid::Uuid,
}

/// 保留原始应用结论，并仅在服务确认安全重试时携带不透明的一次性意图。
pub struct FutureProxyAttemptResult {
    pub result: TerminalProxyApplyResult,
    pub retry_intent: Option<FutureProxyRetryIntent>,
}

impl TerminalProxyUnavailableReason {
    /// 只允许提示符短暂不适用的明确状态重试，禁止从展示文案推断副作用状态。
    ///
    /// @author kongweiguang
    pub(super) const fn is_safe_future_retry(self) -> bool {
        matches!(
            self,
            Self::PromptNotReady | Self::PromptNotEmpty | Self::AlternateScreen
        )
    }
}

/// 私有脚本对将恢复 token 与对应转发地址绑定，避免调用方混用不同转发的撤销数据。
struct ProxyScripts {
    apply_script: String,
    restore_script: String,
}

/// 同一次目标检查所得的快照与原始绑定一起传递；它不持有执行租约，注入前仍须由输入门复验。
///
/// @author kongweiguang
struct ProxyCommandTarget<'a> {
    session: &'a SessionInfo,
    context: &'a TerminalCommandContext,
    binding: UiSessionBinding,
}

/// 当前和后续代理用例复用应用唯一的会话、历史与端口转发服务。
#[derive(Clone)]
pub struct TerminalProxyApplication {
    commands: Arc<CommandsService>,
    sessions: Arc<dyn SessionService>,
    port_forwards: PortForwardService,
    future_policy: FutureProxyPolicyStore,
}

impl TerminalProxyApplication {
    /// 复用装配根已有服务，避免代理用例创建并行会话目录或历史入口。
    pub fn new(
        commands: Arc<CommandsService>,
        sessions: Arc<dyn SessionService>,
        port_forwards: PortForwardService,
        future_policy: FutureProxyPolicyStore,
    ) -> Self {
        Self {
            commands,
            sessions,
            port_forwards,
            future_policy,
        }
    }

    /// 将同一策略状态交给端口转发服务，以便停止或故障时同步取消后续应用。
    pub fn future_policy_store(&self) -> FutureProxyPolicyStore {
        self.future_policy.clone()
    }

    /// 返回主机当前策略版本；初始空状态不读取或写入用户配置文件。
    pub async fn future_state(&self, host_id: HostId) -> AppResult<FutureProxyApplyState> {
        Ok(self.future_policy.state(&host_id))
    }

    /// 只有同主机的活动动态转发才可进入显式后续会话策略。
    pub async fn set_future(
        &self,
        request: FutureProxyApplyRequest,
    ) -> AppResult<FutureProxyApplyState> {
        let host_id = request.host_id.clone();
        if let Some(forward_id) = request.forward_id.as_deref() {
            match self.port_forwards.get(forward_id).await {
                Some(snapshot) => match validate_remote_forward(&snapshot, &host_id) {
                    Ok(()) => {}
                    Err(reason) => return Err(forward_reason_error(reason)),
                },
                None => return Err(AppError::not_found("指定 SOCKS 转发不存在")),
            }
        }
        let forward_id = request.forward_id.clone();
        self.future_policy.set(request)?;
        if let Some(forward_id) = forward_id {
            // The forward can stop between the first read and the policy write. Re-read after
            // committing the CAS; stop hooks share this store, so either this check or the stop
            // hook removes a stale policy before it can trigger a later session.
            let still_usable = self
                .port_forwards
                .get(&forward_id)
                .await
                .is_some_and(|snapshot| validate_remote_forward(&snapshot, &host_id).is_ok());
            if !still_usable {
                self.future_policy.cancel_by_forward(&forward_id);
                return Err(AppError::conflict("SOCKS 转发在保存后续策略时已停止"));
            }
        }
        Ok(self.future_policy.state(&host_id))
    }

    /// 仅按精确 UI 会话代次核对唯一原记录；结果不含令牌或环境备份值，也绝不重放脚本。
    ///
    /// @author kongweiguang
    pub async fn reconcile_unique_current_application(
        &self,
        binding: UiSessionBinding,
        cancellation: CancellationToken,
    ) -> AppResult<Option<TerminalProxyReconcileResult>> {
        let Some(forward_id) = self
            .future_policy
            .unique_application_forward_id(&binding.session_id, binding.session_generation)?
        else {
            return Ok(None);
        };
        let result = self
            .reconcile_current(
                CurrentTerminalProxyRequest {
                    forward_id,
                    binding,
                },
                cancellation,
            )
            .await?;
        if result.outcome == TerminalProxyReconcileOutcome::NoTrackedState {
            return Ok(None);
        }
        Ok(Some(result))
    }

    /// 只向精确可见 SSH 会话的已验证空提示符应用代理变量，并保留原绑定供输入门复验。
    ///
    /// @author kongweiguang
    pub async fn apply_current(
        &self,
        request: CurrentTerminalProxyRequest,
        cancellation: CancellationToken,
    ) -> AppResult<TerminalProxyApplyResult> {
        let forward_id = request.forward_id.clone();
        let session_id = request.binding.session_id.clone();
        let generation = request.binding.session_generation;
        let snapshot = match self.port_forwards.get(&forward_id).await {
            Some(snapshot) => snapshot,
            None => {
                return Ok(not_applied(
                    forward_id,
                    session_id,
                    generation,
                    TerminalProxyUnavailableReason::ForwardNotFound,
                ));
            }
        };
        let host_id = match validate_forward_result(&snapshot) {
            Ok(host_id) => host_id,
            Err(reason) => return Ok(not_applied(forward_id, session_id, generation, reason)),
        };
        let (session, context) = match self
            .inspect_binding(&request.binding, Some(&host_id))
            .await?
        {
            Ok(pair) => pair,
            Err(reason) => return Ok(not_applied(forward_id, session_id, generation, reason)),
        };
        if let Some(reason) = clean_prompt_reason(&context) {
            return Ok(not_applied(forward_id, session_id, generation, reason));
        }
        let proxy_url = match snapshot.proxy_url.as_deref() {
            Some(proxy_url) => proxy_url,
            None => {
                return Ok(not_applied(
                    forward_id,
                    session_id,
                    generation,
                    TerminalProxyUnavailableReason::NoProxyEndpoint,
                ));
            }
        };
        self.execute_apply(
            ProxyCommandTarget {
                session: &session,
                context: &context,
                binding: request.binding,
            },
            forward_id,
            proxy_url,
            cancellation,
            None,
        )
        .await
    }

    /// 仅恢复仍等于本应用写入值的变量；原目标快照保留给完成后的回读验证，保护用户后续修改。
    ///
    /// @author kongweiguang
    pub async fn restore_current(
        &self,
        request: CurrentTerminalProxyRequest,
        cancellation: CancellationToken,
    ) -> AppResult<TerminalProxyApplyResult> {
        let forward_id = request.forward_id.clone();
        let session_id = request.binding.session_id.clone();
        let generation = request.binding.session_generation;
        let (session, context) = match self.inspect_binding(&request.binding, None).await? {
            Ok(pair) => pair,
            Err(reason) => return Ok(not_applied(forward_id, session_id, generation, reason)),
        };
        if let Some(reason) = clean_prompt_reason(&context) {
            return Ok(not_applied(forward_id, session_id, generation, reason));
        }
        let key = ProxyApplicationKey {
            session_id: session_id.clone(),
            session_generation: generation,
            forward_id: forward_id.clone(),
        };
        let Some(application) = self.future_policy.begin_restore(&key) else {
            return Ok(not_applied(
                forward_id,
                session_id,
                generation,
                TerminalProxyUnavailableReason::RestoreStateUnavailable,
            ));
        };
        let scripts = match scripts::build_scripts(
            &application.proxy_url,
            application.dialect,
            &application.token,
        ) {
            Ok(scripts) => scripts,
            Err(error) => {
                self.future_policy
                    .resolve_application(&key, ProxyApplicationResolution::ReadyToRestore);
                return Err(error);
            }
        };
        let binding = request.binding.clone();
        let result = self
            .execute_script(
                ProxyCommandTarget {
                    session: &session,
                    context: &context,
                    binding: binding.clone(),
                },
                forward_id.clone(),
                scripts.restore_script,
                ProxyAction::Restore,
                cancellation.clone(),
                None,
            )
            .await;
        match result {
            Ok(mut result) => match result.outcome.clone() {
                TerminalProxyApplyOutcome::Restored => {
                    let verification = self
                        .verify_restore(&session, &context, &binding, &application, cancellation)
                        .await;
                    match verification {
                        Ok(ReconcileDecision::Remove(TerminalProxyReconcileOutcome::Restored)) => {
                            self.future_policy
                                .resolve_application(&key, ProxyApplicationResolution::Remove);
                        }
                        Ok(ReconcileDecision::Remove(
                            TerminalProxyReconcileOutcome::PartiallyRestored,
                        )) => {
                            result.outcome = TerminalProxyApplyOutcome::PartiallyRestored;
                            self.future_policy
                                .resolve_application(&key, ProxyApplicationResolution::Remove);
                        }
                        Ok(ReconcileDecision::Remove(
                            TerminalProxyReconcileOutcome::NotApplied,
                        )) => {
                            result.outcome = TerminalProxyApplyOutcome::NotApplied {
                                reason: TerminalProxyUnavailableReason::RestoreStateUnavailable,
                            };
                            self.future_policy
                                .resolve_application(&key, ProxyApplicationResolution::Remove);
                        }
                        Ok(ReconcileDecision::Remove(_)) => {
                            result.outcome = TerminalProxyApplyOutcome::Unknown {
                                reason: ShellCommandUnknownReason::MissingCompletion,
                            };
                            self.future_policy.resolve_application(
                                &key,
                                ProxyApplicationResolution::NeedsReconciliation,
                            );
                        }
                        Ok(ReconcileDecision::Keep(
                            TerminalProxyReconcileOutcome::Applied
                            | TerminalProxyReconcileOutcome::AppliedWithChanges,
                        )) => {
                            result.outcome = TerminalProxyApplyOutcome::Unknown {
                                reason: ShellCommandUnknownReason::MissingCompletion,
                            };
                            // The exit marker and readback disagree about restoration; require
                            // a separate reconciliation action before exposing restore again.
                            self.future_policy.resolve_application(
                                &key,
                                ProxyApplicationResolution::NeedsReconciliation,
                            );
                        }
                        Ok(ReconcileDecision::Keep(outcome)) => {
                            result.outcome = outcome_as_apply_unknown(outcome);
                            self.future_policy.resolve_application(
                                &key,
                                ProxyApplicationResolution::NeedsReconciliation,
                            );
                        }
                        Err(error) => {
                            self.future_policy.resolve_application(
                                &key,
                                ProxyApplicationResolution::NeedsReconciliation,
                            );
                            return Err(error);
                        }
                    }
                    Ok(result)
                }
                TerminalProxyApplyOutcome::Unknown { .. }
                | TerminalProxyApplyOutcome::Failed { .. } => {
                    self.future_policy
                        .resolve_application(&key, ProxyApplicationResolution::NeedsReconciliation);
                    Ok(result)
                }
                TerminalProxyApplyOutcome::NotApplied { .. } => {
                    self.future_policy
                        .resolve_application(&key, ProxyApplicationResolution::ReadyToRestore);
                    Ok(result)
                }
                TerminalProxyApplyOutcome::Applied
                | TerminalProxyApplyOutcome::PartiallyRestored => {
                    self.future_policy
                        .resolve_application(&key, ProxyApplicationResolution::NeedsReconciliation);
                    result.outcome = TerminalProxyApplyOutcome::Unknown {
                        reason: ShellCommandUnknownReason::MissingCompletion,
                    };
                    Ok(result)
                }
            },
            Err(error) => {
                self.future_policy
                    .resolve_application(&key, ProxyApplicationResolution::NeedsReconciliation);
                Err(error)
            }
        }
    }

    /// 仅为新建的 UI SSH 会话应用用户显式设置的同主机策略，并为安全的提示符失败签发一次性 intent。
    ///
    /// @author kongweiguang
    pub async fn on_ui_session_ready(
        &self,
        binding: UiSessionBinding,
        cancellation: CancellationToken,
    ) -> AppResult<Option<FutureProxyAttemptResult>> {
        let session_id = binding.session_id.clone();
        let session = match self.session_info(&session_id).await? {
            Some(session) => session,
            None => return Ok(None),
        };
        if !matches!(binding.owner, SessionOwner::Ui { .. }) || session.owner != binding.owner {
            return Ok(None);
        }
        let (host_id, initial_command_present) = match &session.target {
            SessionTarget::Ssh(SshSessionTarget {
                host_id, command, ..
            }) => (host_id.clone(), command.is_some()),
            _ => return Ok(None),
        };
        let policy = self.future_policy.state(&host_id);
        let Some(forward_id) = policy.forward_id.clone() else {
            return Ok(None);
        };
        let key = FutureAttemptKey {
            session_id: session_id.clone(),
            session_generation: binding.session_generation,
            policy_revision: policy.revision,
            forward_id: forward_id.clone(),
        };
        let attempt_cancel =
            match self
                .future_policy
                .claim_attempt(&host_id, &forward_id, key.clone())
            {
                Ok(cancel) => cancel,
                Err(FutureAttemptClaim::PolicyChanged) => {
                    return Ok(Some(FutureProxyAttemptResult {
                        result: not_applied(
                            forward_id,
                            session_id,
                            binding.session_generation,
                            TerminalProxyUnavailableReason::PolicyChanged,
                        ),
                        retry_intent: None,
                    }));
                }
                Err(FutureAttemptClaim::AlreadyProcessed) => {
                    return Ok(Some(FutureProxyAttemptResult {
                        result: not_applied(
                            forward_id,
                            session_id,
                            binding.session_generation,
                            TerminalProxyUnavailableReason::AttemptAlreadyProcessed,
                        ),
                        retry_intent: None,
                    }));
                }
                Err(FutureAttemptClaim::TrackingCapacityReached) => {
                    return Ok(Some(FutureProxyAttemptResult {
                        result: not_applied(
                            forward_id,
                            session_id,
                            binding.session_generation,
                            TerminalProxyUnavailableReason::TrackingCapacityReached,
                        ),
                        retry_intent: None,
                    }));
                }
            };
        let operation_result = if initial_command_present {
            Ok(not_applied(
                forward_id.clone(),
                session_id.clone(),
                binding.session_generation,
                TerminalProxyUnavailableReason::InitialCommandPresent,
            ))
        } else {
            self.apply_future_once(
                binding.clone(),
                host_id.clone(),
                forward_id,
                key.clone(),
                attempt_cancel,
                cancellation.clone(),
            )
            .await
        };
        let retry_reason = operation_result
            .as_ref()
            .ok()
            .and_then(retryable_prompt_reason);
        let capability = self.future_policy.finish_attempt(
            &host_id,
            &key,
            retry_reason,
            cancellation.is_cancelled(),
        );
        let result = operation_result?;
        let retry_intent = capability.map(|capability| FutureProxyRetryIntent {
            host_id,
            binding,
            key,
            capability,
        });
        Ok(Some(FutureProxyAttemptResult {
            result,
            retry_intent,
        }))
    }

    /// 消耗服务签发的旧意图；目标和策略始终取自该意图，不回退到当前活动 pane。
    ///
    /// @author kongweiguang
    pub async fn retry_not_applied(
        &self,
        intent: FutureProxyRetryIntent,
        cancellation: CancellationToken,
    ) -> AppResult<FutureProxyAttemptResult> {
        let FutureProxyRetryIntent {
            host_id,
            binding,
            key,
            capability,
        } = intent;
        if key.session_id != binding.session_id
            || key.session_generation != binding.session_generation
        {
            return Err(AppError::conflict("代理重试意图与原终端绑定不一致"));
        }
        let attempt_cancel = self
            .future_policy
            .begin_retry(&host_id, &key, capability)
            .map_err(future_retry_claim_error)?;
        let operation_result = self
            .apply_future_once(
                binding.clone(),
                host_id.clone(),
                key.forward_id.clone(),
                key.clone(),
                attempt_cancel,
                cancellation.clone(),
            )
            .await;
        let retry_reason = operation_result
            .as_ref()
            .ok()
            .and_then(retryable_prompt_reason);
        let next_capability = self.future_policy.finish_attempt(
            &host_id,
            &key,
            retry_reason,
            cancellation.is_cancelled(),
        );
        let result = operation_result?;
        let retry_intent = next_capability.map(|capability| FutureProxyRetryIntent {
            host_id,
            binding,
            key,
            capability,
        });
        Ok(FutureProxyAttemptResult {
            result,
            retry_intent,
        })
    }

    /// 返回可复制的脚本供用户审阅，不修改 Profile、会话或任何远端环境。
    pub async fn reviewable_scripts(
        &self,
        forward_id: String,
        shell_dialect: RemoteShellDialect,
    ) -> AppResult<ReviewableProxyScript> {
        let snapshot = self
            .port_forwards
            .get(&forward_id)
            .await
            .ok_or_else(|| AppError::not_found("指定 SOCKS 转发不存在"))?;
        validate_reviewable_forward(&snapshot, &snapshot.forward.config.host_id)
            .map_err(forward_reason_error)?;
        let proxy_url = snapshot
            .proxy_url
            .as_deref()
            .ok_or_else(|| AppError::capability_not_supported("SOCKS proxy endpoint"))?;
        let execution_context = proxy_script_execution_context(&snapshot.forward.config.kind)
            .map_err(forward_reason_error)?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let scripts = scripts::build_scripts(proxy_url, shell_dialect, &token)?;
        Ok(ReviewableProxyScript {
            shell_dialect,
            execution_context,
            apply_script: scripts.apply_script,
            restore_script: scripts.restore_script,
        })
    }

    /// 在有界期限内等待已验证空提示符，再为相同会话代次执行一次策略动作。
    ///
    /// @author kongweiguang
    async fn apply_future_once(
        &self,
        binding: UiSessionBinding,
        host_id: HostId,
        forward_id: String,
        attempt_key: FutureAttemptKey,
        attempt_cancel: CancellationToken,
        cancellation: CancellationToken,
    ) -> AppResult<TerminalProxyApplyResult> {
        let session_id = binding.session_id.clone();
        let generation = binding.session_generation;
        let snapshot = match self.port_forwards.get(&forward_id).await {
            Some(snapshot) => snapshot,
            None => {
                return Ok(not_applied(
                    forward_id,
                    session_id,
                    generation,
                    TerminalProxyUnavailableReason::ForwardNotFound,
                ));
            }
        };
        if let Err(reason) = validate_remote_forward(&snapshot, &host_id) {
            return Ok(not_applied(forward_id, session_id, generation, reason));
        }
        let deadline = Instant::now() + FUTURE_PROMPT_WAIT;
        let (session, context) = match self
            .wait_for_clean_prompt(
                &binding,
                &host_id,
                &attempt_key,
                &attempt_cancel,
                &cancellation,
                deadline,
            )
            .await?
        {
            Ok(pair) => pair,
            Err(reason) => return Ok(not_applied(forward_id, session_id, generation, reason)),
        };
        let proxy_url = match snapshot.proxy_url.as_deref() {
            Some(proxy_url) => proxy_url,
            None => {
                return Ok(not_applied(
                    forward_id,
                    session_id,
                    generation,
                    TerminalProxyUnavailableReason::NoProxyEndpoint,
                ));
            }
        };
        self.execute_apply(
            ProxyCommandTarget {
                session: &session,
                context: &context,
                binding,
            },
            forward_id,
            proxy_url,
            cancellation,
            Some(attempt_cancel),
        )
        .await
    }

    /// 先订阅再检查状态，避免第一次读取过程中到达的提示符事件被遗漏。
    async fn wait_for_clean_prompt(
        &self,
        binding: &UiSessionBinding,
        host_id: &HostId,
        attempt_key: &FutureAttemptKey,
        attempt_cancel: &CancellationToken,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> AppResult<Result<(SessionInfo, TerminalCommandContext), TerminalProxyUnavailableReason>>
    {
        let mut subscription = self.sessions.subscribe(binding.session_id.clone()).await?;
        loop {
            if cancellation.is_cancelled() || attempt_cancel.is_cancelled() {
                return Ok(Err(TerminalProxyUnavailableReason::PolicyChanged));
            }
            if !self.future_policy.policy_is_current(
                host_id,
                attempt_key.policy_revision,
                attempt_key,
            ) {
                return Ok(Err(TerminalProxyUnavailableReason::PolicyChanged));
            }
            let Some(session) = self.session_info(&binding.session_id).await? else {
                return Ok(Err(TerminalProxyUnavailableReason::SessionNotFound));
            };
            if let Some(reason) = validate_session_target(&session, binding, Some(host_id)) {
                return Ok(Err(reason));
            }
            let context = match self
                .sessions
                .terminal_command_context(binding.session_id.clone())
                .await
            {
                Ok(context) => context,
                Err(error)
                    if matches!(
                        error.public_code(),
                        PublicErrorCode::CapabilityNotSupported | PublicErrorCode::Unsupported
                    ) =>
                {
                    return Ok(Err(
                        TerminalProxyUnavailableReason::ShellIntegrationUnavailable,
                    ));
                }
                Err(error) => return Err(error),
            };
            if context.session_generation != binding.session_generation
                || context.owner != binding.owner
            {
                return Ok(Err(TerminalProxyUnavailableReason::SessionBindingMismatch));
            }
            let last_reason = if let Some(reason) = clean_prompt_reason(&context) {
                if matches!(
                    reason,
                    TerminalProxyUnavailableReason::UnsupportedShellDialect
                        | TerminalProxyUnavailableReason::ShellIntegrationUnavailable
                        | TerminalProxyUnavailableReason::SessionNotRunning
                        | TerminalProxyUnavailableReason::SessionBindingMismatch
                ) {
                    return Ok(Err(reason));
                }
                reason
            } else {
                return Ok(Ok((session, context)));
            };
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Ok(Err(TerminalProxyUnavailableReason::PolicyChanged)),
                _ = attempt_cancel.cancelled() => return Ok(Err(TerminalProxyUnavailableReason::PolicyChanged)),
                _ = sleep_until(deadline) => return Ok(Err(last_reason)),
                change = subscription.recv(cancellation) => {
                    match change {
                        Ok(Some(change)) if change.kind == SessionChangeKind::Closed => {
                            return Ok(Err(TerminalProxyUnavailableReason::SessionNotRunning));
                        }
                        Ok(Some(_)) => {}
                        Ok(None) => return Ok(Err(TerminalProxyUnavailableReason::SessionNotRunning)),
                        Err(error) if error.public_code() == PublicErrorCode::Cancelled => {
                            return Ok(Err(TerminalProxyUnavailableReason::PolicyChanged));
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        }
    }

    /// 构造 shell 命令前核对精确目标、所有者和运行代次。
    async fn inspect_binding(
        &self,
        binding: &UiSessionBinding,
        expected_host: Option<&HostId>,
    ) -> AppResult<Result<(SessionInfo, TerminalCommandContext), TerminalProxyUnavailableReason>>
    {
        let Some(session) = self.session_info(&binding.session_id).await? else {
            return Ok(Err(TerminalProxyUnavailableReason::SessionNotFound));
        };
        if let Some(reason) = validate_session_target(&session, binding, expected_host) {
            return Ok(Err(reason));
        }
        let context = match self
            .sessions
            .terminal_command_context(binding.session_id.clone())
            .await
        {
            Ok(context) => context,
            Err(error)
                if matches!(
                    error.public_code(),
                    PublicErrorCode::CapabilityNotSupported | PublicErrorCode::Unsupported
                ) =>
            {
                return Ok(Err(
                    TerminalProxyUnavailableReason::ShellIntegrationUnavailable,
                ));
            }
            Err(error) => return Err(error),
        };
        if context.session_id != binding.session_id
            || context.owner != binding.owner
            || context.session_generation != binding.session_generation
        {
            return Ok(Err(TerminalProxyUnavailableReason::SessionBindingMismatch));
        }
        Ok(Ok((session, context)))
    }

    /// 从唯一会话目录按 ID 查询，不从当前活动面板猜测目标。
    async fn session_info(&self, session_id: &SessionId) -> AppResult<Option<SessionInfo>> {
        Ok(self
            .sessions
            .list(SessionFilter::default())
            .await?
            .into_iter()
            .find(|session| &session.session_id == session_id))
    }

    /// 在 shell 副作用前预留恢复信息，并按真实 OSC 133 结果更新状态。
    ///
    /// @author kongweiguang
    async fn execute_apply(
        &self,
        target: ProxyCommandTarget<'_>,
        forward_id: String,
        proxy_url: &str,
        cancellation: CancellationToken,
        policy_cancellation: Option<CancellationToken>,
    ) -> AppResult<TerminalProxyApplyResult> {
        let session = target.session;
        let context = target.context;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let scripts = scripts::build_scripts(proxy_url, context.shell_dialect, &token)?;
        let key = ProxyApplicationKey {
            session_id: session.session_id.clone(),
            session_generation: context.session_generation,
            forward_id: forward_id.clone(),
        };
        match self.future_policy.reserve_application(
            key.clone(),
            StoredProxyApplication {
                token,
                proxy_url: proxy_url.to_owned(),
                dialect: context.shell_dialect,
            },
        ) {
            ApplicationReservation::Reserved => {}
            ApplicationReservation::AlreadyApplied => {
                return Ok(not_applied(
                    forward_id,
                    session.session_id.clone(),
                    context.session_generation,
                    TerminalProxyUnavailableReason::AlreadyApplied,
                ));
            }
            ApplicationReservation::TrackingCapacityReached => {
                return Ok(not_applied(
                    forward_id,
                    session.session_id.clone(),
                    context.session_generation,
                    TerminalProxyUnavailableReason::TrackingCapacityReached,
                ));
            }
        }
        let result = self
            .execute_script(
                target,
                forward_id,
                scripts.apply_script,
                ProxyAction::Apply,
                cancellation,
                policy_cancellation,
            )
            .await;
        match result {
            Ok(result) => {
                match result.outcome.clone() {
                    TerminalProxyApplyOutcome::Applied => self
                        .future_policy
                        .resolve_application(&key, ProxyApplicationResolution::ReadyToRestore),
                    TerminalProxyApplyOutcome::Unknown { .. }
                    | TerminalProxyApplyOutcome::Failed { .. }
                    | TerminalProxyApplyOutcome::Restored
                    | TerminalProxyApplyOutcome::PartiallyRestored => self
                        .future_policy
                        .resolve_application(&key, ProxyApplicationResolution::NeedsReconciliation),
                    TerminalProxyApplyOutcome::NotApplied { .. } => self
                        .future_policy
                        .resolve_application(&key, ProxyApplicationResolution::Remove),
                }
                Ok(result)
            }
            Err(error) => {
                // An unexpected adapter error can happen after input admission, so retain the
                // only recovery token and require a readback before another shell mutation.
                self.future_policy
                    .resolve_application(&key, ProxyApplicationResolution::NeedsReconciliation);
                Err(error)
            }
        }
    }

    /// 目标仍由唯一输入门复验；取消后等待同一执行回执，避免把未知写入当作未应用，历史仍只记录一次。
    ///
    /// @author kongweiguang
    async fn execute_script(
        &self,
        target: ProxyCommandTarget<'_>,
        forward_id: String,
        command: String,
        action: ProxyAction,
        cancellation: CancellationToken,
        policy_cancellation: Option<CancellationToken>,
    ) -> AppResult<TerminalProxyApplyResult> {
        let ProxyCommandTarget {
            session,
            context,
            binding,
        } = target;
        let command_id = uuid::Uuid::new_v4().to_string();
        let shell_request = ShellCommandRequest {
            session_id: session.session_id.clone(),
            expected_ui_binding: Some(binding),
            command_id: command_id.clone(),
            command: command.clone(),
            timeout: SHELL_COMMAND_TIMEOUT,
        };
        let shell_cancellation = CancellationToken::new();
        let execution = self
            .sessions
            .execute_shell_command(shell_request, shell_cancellation.clone());
        tokio::pin!(execution);
        let shell_result = if let Some(policy_cancellation) = policy_cancellation.as_ref() {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    shell_cancellation.cancel();
                    (&mut execution).await
                }
                _ = policy_cancellation.cancelled() => {
                    shell_cancellation.cancel();
                    (&mut execution).await
                }
                result = &mut execution => result,
            }
        } else {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    shell_cancellation.cancel();
                    (&mut execution).await
                }
                result = &mut execution => result,
            }
        };
        let outcome = match shell_result {
            Ok(outcome) => outcome,
            Err(error) => match error.public_code() {
                PublicErrorCode::CapabilityNotSupported | PublicErrorCode::Unsupported => {
                    return Ok(not_applied(
                        forward_id,
                        session.session_id.clone(),
                        context.session_generation,
                        TerminalProxyUnavailableReason::ShellIntegrationUnavailable,
                    ));
                }
                PublicErrorCode::Conflict => {
                    return Ok(not_applied(
                        forward_id,
                        session.session_id.clone(),
                        context.session_generation,
                        TerminalProxyUnavailableReason::PromptNotReady,
                    ));
                }
                PublicErrorCode::Cancelled => ShellCommandOutcome::Unknown {
                    command_id,
                    reason: ShellCommandUnknownReason::Cancelled,
                },
                PublicErrorCode::Timeout => ShellCommandOutcome::Unknown {
                    command_id,
                    reason: ShellCommandUnknownReason::TimedOut,
                },
                PublicErrorCode::OutcomeUnknown => ShellCommandOutcome::Unknown {
                    command_id,
                    reason: ShellCommandUnknownReason::MissingCompletion,
                },
                _ => return Err(error),
            },
        };
        let history = self
            .record_history(session, &command, context.shell_dialect)
            .await;
        let outcome = match outcome {
            ShellCommandOutcome::Finished { exit_code: 0, .. } => match action {
                ProxyAction::Apply => TerminalProxyApplyOutcome::Applied,
                ProxyAction::Restore => TerminalProxyApplyOutcome::Restored,
            },
            ShellCommandOutcome::Finished { exit_code, .. } => {
                TerminalProxyApplyOutcome::Failed { exit_code }
            }
            ShellCommandOutcome::Unknown { reason, .. } => {
                TerminalProxyApplyOutcome::Unknown { reason }
            }
        };
        let (history_recorded, history_skip_reason, history_error_code) = match history {
            Ok(history) => (Some(history.recorded), history.skip_reason, None),
            Err(error) => (None, None, Some(error.public_code())),
        };
        Ok(TerminalProxyApplyResult {
            forward_id,
            session_id: session.session_id.clone(),
            session_generation: context.session_generation,
            outcome,
            history_recorded,
            history_skip_reason,
            history_error_code,
        })
    }

    /// 复用终端历史的来源和脱敏规则保存脚本，不记录 shell 原环境值。
    async fn record_history(
        &self,
        session: &SessionInfo,
        command: &str,
        shell_dialect: RemoteShellDialect,
    ) -> AppResult<kerminal_domain::commands::CommandHistoryRecordResult> {
        let SessionTarget::Ssh(target) = &session.target else {
            return Err(AppError::conflict("代理命令历史目标已不再是 SSH"));
        };
        let (tab_id, pane_id) = match &session.owner {
            SessionOwner::Ui { tab_id, pane_id } => (Some(tab_id.clone()), Some(pane_id.clone())),
            _ => (None, None),
        };
        self.commands
            .record_history(CommandHistoryRecordRequest {
                command: command.to_owned(),
                source: CommandHistorySource::Tool,
                target: CommandHistoryTarget::Ssh,
                record: Some(true),
                session_id: Some(session.session_id.clone()),
                pane_id,
                tab_id,
                profile_id: None,
                remote_host_id: Some(target.host_id.clone()),
                cwd: target
                    .working_directory
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                shell: Some(dialect_name(shell_dialect)),
            })
            .await
    }
}

#[derive(Clone, Copy)]
enum ProxyAction {
    Apply,
    Restore,
}

/// 校验会话快照，不在目标失效时选择其他 UI 会话作为替代。
fn validate_session_target(
    session: &SessionInfo,
    binding: &UiSessionBinding,
    expected_host: Option<&HostId>,
) -> Option<TerminalProxyUnavailableReason> {
    if session.lifecycle != SessionLifecycle::Running {
        return Some(TerminalProxyUnavailableReason::SessionNotRunning);
    }
    if !matches!(binding.owner, SessionOwner::Ui { .. }) || session.owner != binding.owner {
        return Some(TerminalProxyUnavailableReason::SessionBindingMismatch);
    }
    let SessionTarget::Ssh(target) = &session.target else {
        return Some(TerminalProxyUnavailableReason::TargetNotSsh);
    };
    if expected_host.is_some_and(|expected| expected != &target.host_id) {
        return Some(TerminalProxyUnavailableReason::HostMismatch);
    }
    if target.command.is_some() {
        return Some(TerminalProxyUnavailableReason::InitialCommandPresent);
    }
    None
}

/// 注入前核对受信任的 shell 状态、空输入行与备用屏幕状态。
fn clean_prompt_reason(context: &TerminalCommandContext) -> Option<TerminalProxyUnavailableReason> {
    if context.alternate_screen {
        return Some(TerminalProxyUnavailableReason::AlternateScreen);
    }
    if context.shell_dialect == RemoteShellDialect::Unknown {
        return Some(TerminalProxyUnavailableReason::UnsupportedShellDialect);
    }
    if !matches!(context.availability, TerminalCommandAvailability::Available) {
        return Some(match context.availability {
            TerminalCommandAvailability::Available => return None,
            TerminalCommandAvailability::Unavailable(reason) => map_context_reason(reason),
        });
    }
    if !context.prompt_ready {
        return Some(TerminalProxyUnavailableReason::PromptNotReady);
    }
    if !context.prompt_line_empty {
        return Some(TerminalProxyUnavailableReason::PromptNotEmpty);
    }
    None
}

/// 将终端状态原因映射为稳定的代理界面结果。
fn map_context_reason(reason: TerminalCommandUnavailableReason) -> TerminalProxyUnavailableReason {
    match reason {
        TerminalCommandUnavailableReason::NonUiOwner => {
            TerminalProxyUnavailableReason::SessionBindingMismatch
        }
        TerminalCommandUnavailableReason::SessionNotRunning => {
            TerminalProxyUnavailableReason::SessionNotRunning
        }
        TerminalCommandUnavailableReason::ShellIntegrationDisabled
        | TerminalCommandUnavailableReason::ShellIntegrationNotObserved => {
            TerminalProxyUnavailableReason::ShellIntegrationUnavailable
        }
        TerminalCommandUnavailableReason::AlternateScreen => {
            TerminalProxyUnavailableReason::AlternateScreen
        }
        TerminalCommandUnavailableReason::UnsupportedShellDialect => {
            TerminalProxyUnavailableReason::UnsupportedShellDialect
        }
        TerminalCommandUnavailableReason::ExternalInputObserved
        | TerminalCommandUnavailableReason::UiInputProjectionInvalidated
        | TerminalCommandUnavailableReason::InputWritePending
        | TerminalCommandUnavailableReason::ManagedCommandActive
        | TerminalCommandUnavailableReason::PromptNotReady => {
            TerminalProxyUnavailableReason::PromptNotReady
        }
    }
}

/// 校验 SOCKS 转发基础条件；只供生成供人工审阅的本机或远端脚本使用。
fn validate_reviewable_forward(
    snapshot: &PortForwardSnapshot,
    host_id: &HostId,
) -> Result<(), TerminalProxyUnavailableReason> {
    if snapshot.forward.config.host_id != *host_id {
        return Err(TerminalProxyUnavailableReason::HostMismatch);
    }
    if snapshot.forward.state != ForwardState::Active {
        return Err(TerminalProxyUnavailableReason::ForwardInactive);
    }
    proxy_script_execution_context(&snapshot.forward.config.kind)?;
    if snapshot.proxy_url.is_none() {
        return Err(TerminalProxyUnavailableReason::NoProxyEndpoint);
    }
    Ok(())
}

/// 仅把本机与远端 SOCKS 转发映射成明确的脚本运行位置。
fn proxy_script_execution_context(
    kind: &ForwardKind,
) -> Result<ProxyScriptExecutionContext, TerminalProxyUnavailableReason> {
    match kind {
        ForwardKind::Dynamic => Ok(ProxyScriptExecutionContext::LocalShell),
        ForwardKind::RemoteDynamic => Ok(ProxyScriptExecutionContext::RemoteShell),
        _ => Err(TerminalProxyUnavailableReason::WrongForwardKind),
    }
}

/// 远端交互 shell 只能访问远端监听器；本机 Dynamic 的 loopback 在远端不是同一网络端点。
fn validate_remote_forward(
    snapshot: &PortForwardSnapshot,
    host_id: &HostId,
) -> Result<(), TerminalProxyUnavailableReason> {
    validate_reviewable_forward(snapshot, host_id)?;
    if snapshot.forward.config.kind != ForwardKind::RemoteDynamic {
        return Err(TerminalProxyUnavailableReason::WrongForwardKind);
    }
    Ok(())
}

/// 校验快照并返回其 SSH 主机 ID，避免调用方另传可能冲突的主机值。
fn validate_forward_result(
    snapshot: &PortForwardSnapshot,
) -> Result<HostId, TerminalProxyUnavailableReason> {
    validate_remote_forward(snapshot, &snapshot.forward.config.host_id)?;
    Ok(snapshot.forward.config.host_id.clone())
}

/// 将不合格的后续策略目标转换为安全的静态应用错误。
fn forward_reason_error(reason: TerminalProxyUnavailableReason) -> AppError {
    match reason {
        TerminalProxyUnavailableReason::ForwardNotFound => {
            AppError::not_found("指定 SOCKS 转发不存在")
        }
        TerminalProxyUnavailableReason::ForwardInactive => {
            AppError::conflict("SOCKS 转发当前未运行")
        }
        TerminalProxyUnavailableReason::WrongForwardKind => AppError::capability_not_supported(
            "remote shell proxy requires a RemoteDynamic SOCKS forward",
        ),
        TerminalProxyUnavailableReason::NoProxyEndpoint => {
            AppError::capability_not_supported("SOCKS proxy endpoint")
        }
        TerminalProxyUnavailableReason::HostMismatch => {
            AppError::conflict("SOCKS 转发与目标主机不一致")
        }
        _ => AppError::conflict("SOCKS 转发不满足应用条件"),
    }
}

/// 构造尚未注入命令的结果，不声称产生了命令历史记录。
fn not_applied(
    forward_id: String,
    session_id: SessionId,
    session_generation: u64,
    reason: TerminalProxyUnavailableReason,
) -> TerminalProxyApplyResult {
    TerminalProxyApplyResult {
        forward_id,
        session_id,
        session_generation,
        outcome: TerminalProxyApplyOutcome::NotApplied { reason },
        history_recorded: None,
        history_skip_reason: None,
        history_error_code: None,
    }
}

/// 只从 typed 结果中选取可安全显式重试的提示符瞬态状态。
///
/// @author kongweiguang
fn retryable_prompt_reason(
    result: &TerminalProxyApplyResult,
) -> Option<TerminalProxyUnavailableReason> {
    match &result.outcome {
        TerminalProxyApplyOutcome::NotApplied { reason } if reason.is_safe_future_retry() => {
            Some(*reason)
        }
        _ => None,
    }
}

/// 将过期、已消费或正在运行的重试授权统一映射成明确的冲突，避免隐式回退到别的 pane。
///
/// @author kongweiguang
fn future_retry_claim_error(claim: FutureAttemptRetryClaim) -> AppError {
    match claim {
        FutureAttemptRetryClaim::PolicyChanged => {
            AppError::conflict("代理策略已变化，不能重试原会话意图")
        }
        FutureAttemptRetryClaim::AttemptMissing => AppError::conflict("原代理尝试已过期，不能重试"),
        FutureAttemptRetryClaim::InFlight => AppError::conflict("原代理尝试仍在执行"),
        FutureAttemptRetryClaim::NotRetryable => AppError::conflict("原代理结果不允许重试"),
        FutureAttemptRetryClaim::CapabilityChanged => {
            AppError::conflict("代理重试意图已使用或失效")
        }
    }
}

/// shell 回读的其它状态都不等于确认撤销，统一保留本次写入的未知结果。
fn outcome_as_apply_unknown(outcome: TerminalProxyReconcileOutcome) -> TerminalProxyApplyOutcome {
    let reason = match outcome {
        TerminalProxyReconcileOutcome::Unknown { reason } => reason,
        _ => ShellCommandUnknownReason::MissingCompletion,
    };
    TerminalProxyApplyOutcome::Unknown { reason }
}

/// 将受支持的方言映射为历史元数据，不从命令输出或用户配置推测 shell 名称。
fn dialect_name(dialect: RemoteShellDialect) -> String {
    match dialect {
        RemoteShellDialect::Posix => "posix-shell",
        RemoteShellDialect::Fish => "fish",
        RemoteShellDialect::PowerShell => "powershell",
        RemoteShellDialect::Unknown => "unknown",
    }
    .to_owned()
}

#[cfg(test)]
#[path = "../../tests/unit/terminal_proxy.rs"]
mod tests;
