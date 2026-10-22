//! 通过终端内的 token 标记读取代理应用状态，不重放任何 apply/restore 副作用。
//!
//! @author kongweiguang

use std::time::Duration;

use kerminal_domain::{
    connections::RemoteShellDialect,
    identity::SessionId,
    sessions::{SessionInfo, SessionLifecycle, SessionOwner, SessionTarget},
};
use tokio_util::sync::CancellationToken;

use super::{
    CurrentTerminalProxyRequest, TerminalProxyApplication, TerminalProxyUnavailableReason,
    clean_prompt_reason,
    policy::{
        ProxyApplicationKey, ProxyApplicationResolution, ReconciliationReservation,
        StoredProxyApplication,
    },
};
use crate::{
    AppError, AppResult, PublicErrorCode,
    sessions::{
        ShellCommandOutcome, ShellCommandRequest, ShellCommandUnknownReason,
        TerminalCommandContext, UiSessionBinding,
    },
};

const RECONCILIATION_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// 回读回执精确绑定原转发、session 和 transport generation。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalProxyReconcileResult {
    pub forward_id: String,
    pub session_id: SessionId,
    pub session_generation: u64,
    pub outcome: TerminalProxyReconcileOutcome,
}

/// 回读只在 shell marker 与环境比较能证明状态时返回确定结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalProxyReconcileOutcome {
    Applied,
    AppliedWithChanges,
    Restored,
    PartiallyRestored,
    NotApplied,
    NoTrackedState,
    SessionGenerationEnded,
    Busy,
    Unknown {
        reason: ShellCommandUnknownReason,
    },
    ProbeFailed {
        exit_code: i32,
    },
    NotAvailable {
        reason: TerminalProxyUnavailableReason,
    },
}

/// shell 内部探针只返回布尔退出码，不把环境变量或备份值打印到终端。
pub(super) struct ProxyProbeScripts {
    pub restore_full: String,
    pub restore_partial: String,
    pub active: String,
    pub applied: String,
    pub backups_complete: String,
    pub values_match: String,
    pub clear_restore_marker: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProbeResult {
    Value(bool),
    Unknown(ShellCommandUnknownReason),
    NotAvailable(TerminalProxyUnavailableReason),
    Failed(i32),
}

pub(super) enum ReconcileDecision {
    Keep(TerminalProxyReconcileOutcome),
    Remove(TerminalProxyReconcileOutcome),
}

impl TerminalProxyApplication {
    /// 用同一受管 exec 通道回读未知操作；不会自动重发 apply 或 restore 脚本。
    pub async fn reconcile_current(
        &self,
        request: CurrentTerminalProxyRequest,
        cancellation: CancellationToken,
    ) -> AppResult<TerminalProxyReconcileResult> {
        let binding = request.binding;
        let session_id = binding.session_id.clone();
        let generation = binding.session_generation;
        let result = |outcome| TerminalProxyReconcileResult {
            forward_id: request.forward_id.clone(),
            session_id: session_id.clone(),
            session_generation: generation,
            outcome,
        };
        let Some(session) = self.session_info(&session_id).await? else {
            self.future_policy.discard_session_applications(&session_id);
            return Ok(result(
                TerminalProxyReconcileOutcome::SessionGenerationEnded,
            ));
        };
        if matches!(
            session.lifecycle,
            SessionLifecycle::Exited | SessionLifecycle::Closed | SessionLifecycle::Failed
        ) {
            self.future_policy.discard_session_applications(&session_id);
            return Ok(result(
                TerminalProxyReconcileOutcome::SessionGenerationEnded,
            ));
        }
        if session.lifecycle != SessionLifecycle::Running {
            return Ok(result(TerminalProxyReconcileOutcome::NotAvailable {
                reason: TerminalProxyUnavailableReason::SessionNotRunning,
            }));
        }
        if !matches!(binding.owner, SessionOwner::Ui { .. }) || session.owner != binding.owner {
            return Ok(result(TerminalProxyReconcileOutcome::NotAvailable {
                reason: TerminalProxyUnavailableReason::SessionBindingMismatch,
            }));
        }
        if !matches!(&session.target, SessionTarget::Ssh(_)) {
            return Ok(result(TerminalProxyReconcileOutcome::NotAvailable {
                reason: TerminalProxyUnavailableReason::TargetNotSsh,
            }));
        }
        let context = match self
            .sessions
            .terminal_command_context(session_id.clone())
            .await
        {
            Ok(context) => context,
            Err(error)
                if matches!(
                    error.public_code(),
                    PublicErrorCode::CapabilityNotSupported | PublicErrorCode::Unsupported
                ) =>
            {
                return Ok(result(TerminalProxyReconcileOutcome::NotAvailable {
                    reason: TerminalProxyUnavailableReason::ShellIntegrationUnavailable,
                }));
            }
            Err(error) => return Err(error),
        };
        if context.session_generation > generation {
            self.future_policy
                .discard_older_session_generations(&session_id, context.session_generation);
            return Ok(result(
                TerminalProxyReconcileOutcome::SessionGenerationEnded,
            ));
        }
        if context.session_id != session_id
            || context.owner != binding.owner
            || context.session_generation != generation
        {
            return Ok(result(TerminalProxyReconcileOutcome::NotAvailable {
                reason: TerminalProxyUnavailableReason::SessionBindingMismatch,
            }));
        }
        if let Some(reason) = clean_prompt_reason(&context) {
            return Ok(result(TerminalProxyReconcileOutcome::NotAvailable {
                reason,
            }));
        }

        let key = ProxyApplicationKey {
            session_id: session_id.clone(),
            session_generation: generation,
            forward_id: request.forward_id.clone(),
        };
        let application = match self.future_policy.begin_reconciliation(&key) {
            ReconciliationReservation::Acquired(application) => application,
            ReconciliationReservation::Busy => {
                return Ok(result(TerminalProxyReconcileOutcome::Busy));
            }
            ReconciliationReservation::Missing => {
                return Ok(result(TerminalProxyReconcileOutcome::NoTrackedState));
            }
        };
        let decision = self
            .reconcile_application(&session, &context, &binding, &application, cancellation)
            .await;
        match decision {
            Ok(ReconcileDecision::Keep(outcome)) => {
                let resolution = match outcome {
                    TerminalProxyReconcileOutcome::Applied
                    | TerminalProxyReconcileOutcome::AppliedWithChanges => {
                        ProxyApplicationResolution::ReadyToRestore
                    }
                    _ => ProxyApplicationResolution::NeedsReconciliation,
                };
                self.future_policy.finish_reconciliation(&key, resolution);
                Ok(result(outcome))
            }
            Ok(ReconcileDecision::Remove(outcome)) => {
                self.future_policy
                    .finish_reconciliation(&key, ProxyApplicationResolution::Remove);
                Ok(result(outcome))
            }
            Err(error) => {
                self.future_policy
                    .finish_reconciliation(&key, ProxyApplicationResolution::NeedsReconciliation);
                Err(error)
            }
        }
    }

    /// 验证刚完成的 restore command marker，不把单独的 exit 0 当作完整/部分恢复证明。
    pub(super) async fn verify_restore(
        &self,
        session: &SessionInfo,
        context: &TerminalCommandContext,
        binding: &UiSessionBinding,
        application: &StoredProxyApplication,
        cancellation: CancellationToken,
    ) -> AppResult<ReconcileDecision> {
        self.reconcile_application(session, context, binding, application, cancellation)
            .await
    }

    /// 依序查询 restore、active、backup 与环境匹配标记，任何不确定性都保留恢复状态。
    async fn reconcile_application(
        &self,
        session: &SessionInfo,
        context: &TerminalCommandContext,
        binding: &UiSessionBinding,
        application: &StoredProxyApplication,
        cancellation: CancellationToken,
    ) -> AppResult<ReconcileDecision> {
        let probes = build_probe_scripts(
            &application.proxy_url,
            application.dialect,
            &application.token,
        )?;
        for (script, outcome) in [
            (
                probes.restore_full.as_str(),
                TerminalProxyReconcileOutcome::Restored,
            ),
            (
                probes.restore_partial.as_str(),
                TerminalProxyReconcileOutcome::PartiallyRestored,
            ),
        ] {
            match self
                .run_proxy_probe(session, context, binding, script, cancellation.clone())
                .await?
            {
                ProbeResult::Value(true) => {
                    match self
                        .run_proxy_probe(
                            session,
                            context,
                            binding,
                            &probes.clear_restore_marker,
                            cancellation.clone(),
                        )
                        .await?
                    {
                        ProbeResult::Value(true) => {
                            return Ok(ReconcileDecision::Remove(outcome));
                        }
                        cleanup => return Ok(keep_probe_result(cleanup)),
                    }
                }
                ProbeResult::Value(false) => {}
                other => return Ok(keep_probe_result(other)),
            }
        }

        let active = self
            .run_proxy_probe(
                session,
                context,
                binding,
                &probes.active,
                cancellation.clone(),
            )
            .await?;
        let backups = self
            .run_proxy_probe(
                session,
                context,
                binding,
                &probes.backups_complete,
                cancellation.clone(),
            )
            .await?;
        if let Some(decision) = unresolved_probe_pair(active.clone(), backups.clone()) {
            return Ok(decision);
        }
        let active = match active {
            ProbeResult::Value(active) => active,
            other => return Ok(keep_probe_result(other)),
        };
        let backups = match backups {
            ProbeResult::Value(backups) => backups,
            other => return Ok(keep_probe_result(other)),
        };
        if !active && !backups {
            // A tracked token with no matching shell marker is ambiguous: the script may have
            // been interrupted or the user may have changed private shell variables. Keep the
            // token for later readback or generation-end cleanup instead of claiming no effect.
            return Ok(ReconcileDecision::Keep(
                TerminalProxyReconcileOutcome::Unknown {
                    reason: ShellCommandUnknownReason::MissingCompletion,
                },
            ));
        }
        if !active || !backups {
            return Ok(ReconcileDecision::Keep(
                TerminalProxyReconcileOutcome::Unknown {
                    reason: ShellCommandUnknownReason::MissingCompletion,
                },
            ));
        }
        let applied = self
            .run_proxy_probe(
                session,
                context,
                binding,
                &probes.applied,
                cancellation.clone(),
            )
            .await?;
        if let Some(decision) = unresolved_probe(applied.clone()) {
            return Ok(decision);
        }
        let applied = match applied {
            ProbeResult::Value(applied) => applied,
            other => return Ok(keep_probe_result(other)),
        };
        if !applied {
            return Ok(ReconcileDecision::Keep(
                TerminalProxyReconcileOutcome::Unknown {
                    reason: ShellCommandUnknownReason::MissingCompletion,
                },
            ));
        }
        let values_match = self
            .run_proxy_probe(
                session,
                context,
                binding,
                &probes.values_match,
                cancellation,
            )
            .await?;
        match values_match {
            ProbeResult::Value(true) => Ok(ReconcileDecision::Keep(
                TerminalProxyReconcileOutcome::Applied,
            )),
            ProbeResult::Value(false) => Ok(ReconcileDecision::Keep(
                TerminalProxyReconcileOutcome::AppliedWithChanges,
            )),
            other => Ok(keep_probe_result(other)),
        }
    }

    /// 受管查询使用 expected binding 防止 reconnect 后向新 transport 写入探针。
    async fn run_proxy_probe(
        &self,
        session: &SessionInfo,
        _context: &TerminalCommandContext,
        binding: &UiSessionBinding,
        command: &str,
        cancellation: CancellationToken,
    ) -> AppResult<ProbeResult> {
        if cancellation.is_cancelled() {
            return Ok(ProbeResult::Unknown(ShellCommandUnknownReason::Cancelled));
        }
        let command_id = uuid::Uuid::new_v4().to_string();
        let request = ShellCommandRequest {
            session_id: session.session_id.clone(),
            expected_ui_binding: Some(binding.clone()),
            command_id: command_id.clone(),
            command: command.to_owned(),
            timeout: RECONCILIATION_COMMAND_TIMEOUT,
        };
        match self
            .sessions
            .execute_shell_command(request, cancellation)
            .await
        {
            Ok(ShellCommandOutcome::Finished { exit_code: 0, .. }) => Ok(ProbeResult::Value(true)),
            Ok(ShellCommandOutcome::Finished { exit_code: 1, .. }) => Ok(ProbeResult::Value(false)),
            Ok(ShellCommandOutcome::Finished { exit_code, .. }) => {
                Ok(ProbeResult::Failed(exit_code))
            }
            Ok(ShellCommandOutcome::Unknown { reason, .. }) => Ok(ProbeResult::Unknown(reason)),
            Err(error) => match error.public_code() {
                PublicErrorCode::CapabilityNotSupported | PublicErrorCode::Unsupported => {
                    Ok(ProbeResult::NotAvailable(
                        TerminalProxyUnavailableReason::ShellIntegrationUnavailable,
                    ))
                }
                PublicErrorCode::Conflict => Ok(ProbeResult::NotAvailable(
                    TerminalProxyUnavailableReason::PromptNotReady,
                )),
                PublicErrorCode::Cancelled => {
                    Ok(ProbeResult::Unknown(ShellCommandUnknownReason::Cancelled))
                }
                PublicErrorCode::Timeout => {
                    Ok(ProbeResult::Unknown(ShellCommandUnknownReason::TimedOut))
                }
                PublicErrorCode::OutcomeUnknown => Ok(ProbeResult::Unknown(
                    ShellCommandUnknownReason::MissingCompletion,
                )),
                _ => Err(error),
            },
        }
    }
}

/// 将探针失败、取消和能力缺失转换为保留凭据的回读结果。
fn keep_probe_result(result: ProbeResult) -> ReconcileDecision {
    let outcome = match result {
        ProbeResult::Unknown(reason) => TerminalProxyReconcileOutcome::Unknown { reason },
        ProbeResult::NotAvailable(reason) => TerminalProxyReconcileOutcome::NotAvailable { reason },
        ProbeResult::Failed(exit_code) => TerminalProxyReconcileOutcome::ProbeFailed { exit_code },
        ProbeResult::Value(_) => TerminalProxyReconcileOutcome::Unknown {
            reason: ShellCommandUnknownReason::MissingCompletion,
        },
    };
    ReconcileDecision::Keep(outcome)
}

/// 两个前置布尔探针中有任一不确定值时，不推断 shell 内部代理状态。
fn unresolved_probe_pair(first: ProbeResult, second: ProbeResult) -> Option<ReconcileDecision> {
    unresolved_probe(first).or_else(|| unresolved_probe(second))
}

/// 将探针值之外的执行结果转换为安全的保留状态。
fn unresolved_probe(result: ProbeResult) -> Option<ReconcileDecision> {
    match result {
        ProbeResult::Value(_) => None,
        other => Some(keep_probe_result(other)),
    }
}

/// 根据方言生成无输出检查和私有 restore 标记清理命令。
pub(super) fn build_probe_scripts(
    proxy_url: &str,
    dialect: RemoteShellDialect,
    token: &str,
) -> AppResult<ProxyProbeScripts> {
    if proxy_url.is_empty()
        || proxy_url.len() > 2_048
        || proxy_url.chars().any(char::is_control)
        || token.is_empty()
        || !token.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(AppError::invalid("SOCKS 回读参数无效"));
    }
    let prefix = format!("__KERMINAL_PROXY_{token}");
    let names = [
        "ALL_PROXY_UPPER",
        "ALL_PROXY_LOWER",
        "NO_PROXY_UPPER",
        "NO_PROXY_LOWER",
    ]
    .map(|label| format!("{prefix}_{label}"));
    let (restore_full, restore_partial, active, applied, backups_complete, values_match, clear) =
        match dialect {
            RemoteShellDialect::Posix => {
                let full = posix_param(&format!("{prefix}_RESTORE_RESULT"), "");
                let active = posix_param(&format!("{prefix}_ACTIVE"), "");
                let applied = posix_param(&format!("{prefix}_APPLIED"), "");
                let backups = names
                    .iter()
                    .map(|name| {
                        format!("test \"{}\" = x", posix_param(&format!("{name}_SET"), "+x"))
                    })
                    .collect::<Vec<_>>()
                    .join(" && ");
                let values = ["ALL_PROXY", "all_proxy"]
                    .iter()
                    .map(|name| {
                        format!(
                            "test \"{}\" = {}",
                            posix_param(name, "-"),
                            posix_quote(proxy_url)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" && ");
                let clear_names = std::iter::once(format!("{prefix}_RESTORE_RESULT"))
                    .chain([
                        format!("{prefix}_ACTIVE"),
                        format!("{prefix}_APPLIED"),
                        format!("{prefix}_RESTORE"),
                    ])
                    .chain(
                        names
                            .iter()
                            .flat_map(|name| [format!("{name}_SET"), format!("{name}_VALUE")]),
                    )
                    .collect::<Vec<_>>();
                (
                    format!("test \"{full}\" = full"),
                    format!("test \"{full}\" = partial"),
                    format!("test \"{active}\" = {}", posix_quote(token)),
                    format!("test \"{applied}\" = {}", posix_quote(token)),
                    backups,
                    values,
                    format!("unset {}", clear_names.join(" ")),
                )
            }
            RemoteShellDialect::Fish => {
                let full = fish_var(&format!("{prefix}_RESTORE_RESULT"));
                let active = fish_var(&format!("{prefix}_ACTIVE"));
                let applied = fish_var(&format!("{prefix}_APPLIED"));
                let backups = names
                    .iter()
                    .map(|name| format!("set -q {name}_SET"))
                    .collect::<Vec<_>>()
                    .join("; and ");
                let values = ["ALL_PROXY", "all_proxy"]
                    .iter()
                    .map(|name| format!("test \"{}\" = {}", fish_var(name), fish_quote(proxy_url)))
                    .collect::<Vec<_>>()
                    .join("; and ");
                let clear_names = std::iter::once(format!("{prefix}_RESTORE_RESULT"))
                    .chain([
                        format!("{prefix}_ACTIVE"),
                        format!("{prefix}_APPLIED"),
                        format!("{prefix}_RESTORE"),
                    ])
                    .chain(
                        names
                            .iter()
                            .flat_map(|name| [format!("{name}_SET"), format!("{name}_VALUE")]),
                    )
                    .collect::<Vec<_>>();
                (
                    format!("test \"{full}\" = full"),
                    format!("test \"{full}\" = partial"),
                    format!("test \"{active}\" = {}", fish_quote(token)),
                    format!("test \"{applied}\" = {}", fish_quote(token)),
                    backups,
                    values,
                    format!("set -e {}", clear_names.join(" ")),
                )
            }
            RemoteShellDialect::PowerShell => {
                let full = format!("$global:{prefix}_RESTORE_RESULT -ceq 'full'");
                let partial = format!("$global:{prefix}_RESTORE_RESULT -ceq 'partial'");
                let active = format!("$global:{prefix}_ACTIVE -ceq {}", powershell_quote(token));
                let applied = format!("$global:{prefix}_APPLIED -ceq {}", powershell_quote(token));
                let backups = names
                    .iter()
                    .map(|name| format!("(Test-Path Variable:global:{name}_SET)"))
                    .collect::<Vec<_>>()
                    .join(" -and ");
                let values = ["ALL_PROXY", "all_proxy"]
                    .iter()
                    .map(|name| format!("$env:{name} -ceq {}", powershell_quote(proxy_url)))
                    .collect::<Vec<_>>()
                    .join(" -and ");
                let clear_names = std::iter::once(format!("{prefix}_RESTORE_RESULT"))
                    .chain([
                        format!("{prefix}_ACTIVE"),
                        format!("{prefix}_APPLIED"),
                        format!("{prefix}_RESTORE"),
                    ])
                    .chain(
                        names
                            .iter()
                            .flat_map(|name| [format!("{name}_SET"), format!("{name}_VALUE")]),
                    )
                    .collect::<Vec<_>>();
                (
                    powershell_bool(&full),
                    powershell_bool(&partial),
                    powershell_bool(&active),
                    powershell_bool(&applied),
                    powershell_bool(&backups),
                    powershell_bool(&values),
                    format!(
                        "Remove-Variable -Scope Global -Name {} -ErrorAction SilentlyContinue",
                        clear_names
                            .iter()
                            .map(|name| powershell_quote(name))
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                )
            }
            RemoteShellDialect::Unknown => {
                return Err(AppError::capability_not_supported(
                    "proxy shell dialect is unknown",
                ));
            }
        };
    Ok(ProxyProbeScripts {
        restore_full,
        restore_partial,
        active,
        applied,
        backups_complete,
        values_match,
        clear_restore_marker: clear,
    })
}

/// 用动态变量名构造 POSIX parameter expansion，备份值不会成为命令文本。
fn posix_param(name: &str, suffix: &str) -> String {
    format!("${{{name}{suffix}}}")
}

/// 用已验证名字构造 Fish 变量引用。
fn fish_var(name: &str) -> String {
    format!("${name}")
}

/// 为 PowerShell 布尔探针把 false 映射成静默错误状态，供 OSC 133 D 回传退出码。
fn powershell_bool(expression: &str) -> String {
    format!(
        "if ({expression}) {{ $global:LASTEXITCODE = $null; $null = $true }} else {{ $global:LASTEXITCODE = 1; Write-Error -Message 'probe-failed' -ErrorAction SilentlyContinue }}"
    )
}

/// 按 POSIX 单引号规则编码代理 URL，不允许值影响探针语法。
fn posix_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 按 Fish 单引号规则编码代理 URL。
fn fish_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// 按 PowerShell 单引号规则编码代理 URL。
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
