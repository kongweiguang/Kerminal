//! 集中处理主机密钥信任与轮换的写入、取消回执和端点回读生命周期。
//!
//! @author kongweiguang

use std::{future::Future, sync::Arc};

use gpui_kit::Context;
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor, PublicErrorCode,
    connections::{
        HostKeyChallenge, HostKeyRotationReceipt, HostKeyStatus, HostKeyTrustReceipt,
        RemoteConnectionService,
    },
};
use kerminal_domain::identity::HostId;
use tokio_util::sync::CancellationToken;

use super::panel::{
    ConnectionRequestCancellation, ConnectionsPanel, PendingTrust, localized_error,
};

/// 两种信任写入使用同一提交闭环，只在确认、成功和失败文案上有所区别。
#[derive(Clone, Copy)]
pub(super) enum HostKeyMutationKind {
    Trust,
    Rotation,
}

impl HostKeyMutationKind {
    /// 返回与具体密钥操作匹配的开始提示，保留已有双语 UI 文案。
    fn progress_message(self) -> String {
        match self {
            Self::Trust => crate::locale::text(
                "正在重新握手并验证主机指纹…",
                "Reconnecting to verify the host key…",
            )
            .to_string(),
            Self::Rotation => crate::locale::text(
                "正在重新握手并核对端点与新指纹…",
                "Reconnecting to verify the endpoint and new fingerprint…",
            )
            .to_string(),
        }
    }

    /// 用操作类型给出明确的成功回执语义，再继续原连接意图。
    fn success_message(self) -> String {
        match self {
            Self::Trust => crate::locale::text(
                "指纹已通过二次握手验证。",
                "Host key was verified by a second handshake.",
            )
            .to_string(),
            Self::Rotation => crate::locale::text(
                "已原子更新该端点的主机指纹，正在按原目标重试连接。",
                "The host key was atomically updated for this endpoint. Retrying the original target.",
            )
            .to_string(),
        }
    }

    /// 将未知提交结果明确标为待核对，禁止 UI 把未知当作失败后直接重放。
    fn unknown_message(self, error: &AppError) -> String {
        let (operation_zh, operation_en) = match self {
            Self::Trust => ("信任写入", "host key trust write"),
            Self::Rotation => ("密钥轮换", "host key rotation"),
        };
        crate::locale::text(
            format!(
                "{} {}结果仍未知，当前端点无法完成回读。请重新连接检查实际指纹后再决定是否重试。",
                localized_error(error), operation_zh
            ),
            format!(
                "{} The {} outcome is still unknown and the endpoint could not be read back. Reconnect to inspect its actual key before retrying.",
                localized_error(error), operation_en
            ),
        )
        .to_string()
    }

    /// 对可安全重试的失败给出相同下一步，避免两类操作产生不同的恢复路径。
    fn retry_message(self, error: &AppError) -> String {
        crate::locale::text(
            format!(
                "{} 可再次确认重试；若服务器或配置已变化，请取消并重新连接以获取最新指纹。",
                localized_error(error)
            ),
            format!(
                "{} You can confirm and retry; if the server or connection profile changed, cancel and reconnect to inspect the latest fingerprint.",
                localized_error(error)
            ),
        )
        .to_string()
    }

    /// 说明未知或取消结果完成回读失败后必须重新检查真实端点。
    fn readback_failure_message(self, error: &AppError) -> String {
        let (operation_zh, operation_en) = match self {
            Self::Trust => ("信任写入", "trust write"),
            Self::Rotation => ("密钥轮换", "key rotation"),
        };
        crate::locale::text(
            format!(
                "{} {}结果仍未核实。请重新连接读取原端点的实际指纹，不要直接重复替换。",
                localized_error(error), operation_zh
            ),
            format!(
                "{} The {} outcome remains unverified. Reconnect to read the original endpoint's actual key before retrying; do not repeat the replacement blindly.",
                localized_error(error), operation_en
            ),
        )
        .to_string()
    }
}

/// 统一比较两类写入回执；两者都绑定完整 endpoint，避免迟到结果跨 host:port 继续执行。
pub(super) trait HostKeyMutationReceipt: Send + 'static {
    /// 仅接受与本次 challenge 完全匹配的 host、peer、endpoint 和指纹回执。
    fn matches_challenge(&self, challenge: &HostKeyChallenge) -> bool;

    /// 成功后返回回执绑定的主机，供原连接意图继续执行。
    fn host_id(&self) -> HostId;
}

impl HostKeyMutationReceipt for HostKeyTrustReceipt {
    /// 检查首次信任回执的 endpoint 绑定，防止同 key 的其他端口混入成功结果。
    fn matches_challenge(&self, challenge: &HostKeyChallenge) -> bool {
        self.host_id == challenge.host_id
            && self.peer == challenge.peer
            && self.endpoint == challenge.endpoint
            && self.fingerprint_sha256 == challenge.fingerprint_sha256
    }

    /// 返回回执本身的 HostId，避免由显示态推断连接目标。
    fn host_id(&self) -> HostId {
        self.host_id.clone()
    }
}

impl HostKeyMutationReceipt for HostKeyRotationReceipt {
    /// 检查轮换回执中的完整目标身份，拒绝跨端点的迟到提交结果。
    fn matches_challenge(&self, challenge: &HostKeyChallenge) -> bool {
        self.host_id == challenge.host_id
            && self.peer == challenge.peer
            && self.endpoint == challenge.endpoint
            && self.fingerprint_sha256 == challenge.fingerprint_sha256
    }

    /// 返回回执本身的 HostId，继续时仍以已校验回执为依据。
    fn host_id(&self) -> HostId {
        self.host_id.clone()
    }
}

/// 按精确 challenge 分类写入后的端点状态，不把模糊回执当作信任成功。
enum HostKeyMutationReadback {
    Trusted,
    Current(HostKeyChallenge),
    EndpointChanged,
    Failed(AppError),
}

/// 只将 HostId、peer、endpoint 和当前指纹完全匹配的 Known 状态视为已确认写入。
fn classify_readback(
    expected: &HostKeyChallenge,
    readback: AppResult<HostKeyChallenge>,
) -> HostKeyMutationReadback {
    match readback {
        Ok(actual)
            if actual.host_id != expected.host_id
                || actual.peer != expected.peer
                || actual.endpoint != expected.endpoint =>
        {
            HostKeyMutationReadback::EndpointChanged
        }
        Ok(actual)
            if actual.fingerprint_sha256 == expected.fingerprint_sha256
                && actual.status == HostKeyStatus::Known =>
        {
            HostKeyMutationReadback::Trusted
        }
        Ok(actual) => HostKeyMutationReadback::Current(actual),
        Err(error) => HostKeyMutationReadback::Failed(error),
    }
}

/// 共享首次信任与轮换的提交等待、取消回执、未知结果回读和 UI 收尾。
///
/// 写入函数只负责构造各自 application 请求；取消时保留其 future 直到真实结果返回。
///
/// @author kongweiguang
pub(super) fn start_host_key_mutation<R, Write, Fut>(
    panel: &mut ConnectionsPanel,
    pending: PendingTrust,
    kind: HostKeyMutationKind,
    cancellation_context: &'static str,
    write: Write,
    cx: &mut Context<ConnectionsPanel>,
) where
    R: HostKeyMutationReceipt,
    Write: FnOnce(Arc<dyn RemoteConnectionService>, CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = AppResult<R>> + Send + 'static,
{
    let challenge = pending.challenge.clone();
    let retry_pending = pending.clone();
    let action = pending.action.clone();
    let write_remote = panel.remote.clone();
    let readback_remote = panel.remote.clone();
    let finalize_remote = panel.remote.clone();
    let late_readback_remote = panel.remote.clone();
    let executor = panel.executor.clone();
    let (request_generation, request_cancellation) = panel.connection_request.snapshot();
    let callback_cancellation = request_cancellation.clone();
    let operation_cancellation = CancellationToken::new();
    let callback_operation_cancellation = operation_cancellation.clone();
    let worker_operation_cancellation = operation_cancellation.clone();
    let business_user_cancellation = worker_operation_cancellation.clone();
    let worker_challenge = challenge.clone();
    if let Some(current) = panel.pending_trust.as_mut() {
        current.operation_busy = true;
        current.operation_cancellation = Some(operation_cancellation);
    }

    cx.spawn(async move |this, cx| {
        let result = executor
            .run_cancellable(
                worker_operation_cancellation,
                move |executor_cancel| async move {
                    let cancel = request_cancellation.bind_to(&executor_cancel);
                    let write_result = if cancel.is_cancelled() {
                        Err(AppError::cancelled(cancellation_context))
                    } else {
                        write(write_remote, cancel).await
                    };
                    let receipt_mismatch = write_result
                        .as_ref()
                        .ok()
                        .is_some_and(|receipt| !receipt.matches_challenge(&worker_challenge));
                    let result_unknown = write_result.as_ref().err().is_some_and(|error| {
                        error.public_code() == PublicErrorCode::OutcomeUnknown
                    });
                    let needs_readback = receipt_mismatch
                        || business_user_cancellation.is_cancelled()
                        || request_cancellation.is_cancelled()
                        || executor_cancel.is_cancelled()
                        || result_unknown;
                    let readback = if needs_readback {
                        Some(
                            readback_remote
                                .inspect_host_key(
                                    worker_challenge.host_id.clone(),
                                    worker_challenge.peer.clone(),
                                    CancellationToken::new(),
                                )
                                .await,
                        )
                    } else {
                        None
                    };
                    Ok((write_result, readback))
                },
            )
            .await;
        let user_cancelled_before_readback = callback_operation_cancellation.is_cancelled();
        let result = finalize_host_key_mutation(
            &executor,
            finalize_remote,
            &challenge,
            result,
            user_cancelled_before_readback,
        )
        .await;

        cx.update(|app| {
            let _ = this.update(app, |panel, cx| {
                let user_cancelled = callback_operation_cancellation.is_cancelled();
                let request_is_current = if user_cancelled {
                    panel
                        .connection_request
                        .is_current_after_user_cancel(request_generation, &callback_cancellation)
                } else {
                    panel
                        .connection_request
                        .is_current(request_generation, &callback_cancellation)
                };
                if !request_is_current {
                    return;
                }
                if user_cancelled && result.1.is_none() {
                    start_late_readback(
                        panel,
                        LateHostKeyReadback {
                            pending: retry_pending.clone(),
                            expected: challenge.clone(),
                            kind,
                            request_generation,
                            request_cancellation: callback_cancellation.clone(),
                        },
                        executor.clone(),
                        late_readback_remote.clone(),
                        cx,
                    );
                    return;
                }

                match result {
                    (_, Some(readback)) => apply_readback(
                        panel,
                        retry_pending,
                        &challenge,
                        kind,
                        readback,
                        user_cancelled,
                        cx,
                    ),
                    (Ok(receipt), None) if receipt.matches_challenge(&challenge) => {
                        panel.pending_trust = None;
                        panel.status = Some(kind.success_message());
                        panel.preflight_pending_action(receipt.host_id(), action, cx);
                    }
                    (Ok(_), None) => {
                        panel.pending_trust = Some(reset_pending(retry_pending));
                        panel.status = Some(localized_error(&AppError::host_key_trust_mismatch()));
                        cx.notify();
                    }
                    (Err(error), None) => {
                        if error.public_code() == PublicErrorCode::OutcomeUnknown {
                            panel.pending_trust = None;
                            panel.status = Some(kind.unknown_message(&error));
                        } else {
                            panel.pending_trust = Some(reset_pending(retry_pending));
                            panel.status = Some(kind.retry_message(&error));
                        }
                        cx.notify();
                    }
                }
            });
        });
    })
    .detach();
    panel.status = Some(kind.progress_message());
    cx.notify();
}

/// 丢失后台回执或在最后一次状态检查时才收到取消时，读取原端点的实际密钥状态。
///
/// @author kongweiguang
async fn finalize_host_key_mutation<R: HostKeyMutationReceipt>(
    executor: &ApplicationTaskExecutor,
    remote: Arc<dyn RemoteConnectionService>,
    expected: &HostKeyChallenge,
    result: AppResult<(AppResult<R>, Option<AppResult<HostKeyChallenge>>)>,
    user_cancelled: bool,
) -> (AppResult<R>, Option<AppResult<HostKeyChallenge>>) {
    let (write_result, mut readback, operation_result_lost) = match result {
        Ok((write_result, readback)) => (write_result, readback, false),
        Err(error) => (Err(error), None, true),
    };
    let result_unknown = write_result
        .as_ref()
        .err()
        .is_some_and(|error| error.public_code() == PublicErrorCode::OutcomeUnknown);
    if readback.is_none() && (user_cancelled || operation_result_lost || result_unknown) {
        let host_id = expected.host_id.clone();
        let peer = expected.peer.clone();
        readback =
            Some(
                executor
                    .run(move |cancel| async move {
                        remote.inspect_host_key(host_id, peer, cancel).await
                    })
                    .await,
            );
    }
    (write_result, readback)
}

/// 清除一次操作的 busy/cancel token 后恢复原确认状态，以便用户安全重试。
fn reset_pending(mut pending: PendingTrust) -> PendingTrust {
    pending.operation_busy = false;
    pending.operation_cancellation = None;
    pending
}

/// 晚到取消仍绑定原写入意图与请求代次，不允许回读结果续接到后来选择的主机。
struct LateHostKeyReadback {
    pending: PendingTrust,
    expected: HostKeyChallenge,
    kind: HostKeyMutationKind,
    request_generation: u64,
    request_cancellation: ConnectionRequestCancellation,
}

/// 晚到取消按同一请求快照回读原 endpoint；保持 busy，直到持久状态核实后再结束原连接意图。
///
/// @author kongweiguang
fn start_late_readback(
    panel: &mut ConnectionsPanel,
    request: LateHostKeyReadback,
    executor: ApplicationTaskExecutor,
    remote: Arc<dyn RemoteConnectionService>,
    cx: &mut Context<ConnectionsPanel>,
) {
    let LateHostKeyReadback {
        pending,
        expected,
        kind,
        request_generation,
        request_cancellation,
    } = request;
    panel.status = Some(crate::locale::text(
        "写入回执已到达但随后收到取消请求；正在回读原端点，暂不继续连接。",
        "The write receipt arrived just before cancellation. Reading back the original endpoint before continuing.",
    ).to_string());
    cx.notify();
    cx.spawn(async move |this, cx| {
        let host_id = expected.host_id.clone();
        let peer = expected.peer.clone();
        let readback = executor
            .run(move |cancel| async move { remote.inspect_host_key(host_id, peer, cancel).await })
            .await;
        cx.update(|app| {
            let _ = this.update(app, |panel, cx| {
                if !panel
                    .connection_request
                    .is_current_after_user_cancel(request_generation, &request_cancellation)
                {
                    return;
                }
                apply_readback(panel, pending, &expected, kind, readback, true, cx);
            });
        });
    })
    .detach();
}

/// 回读状态完成后再关闭 busy；未知、变化或不同端点均要求重新核对。
///
/// @author kongweiguang
fn apply_readback(
    panel: &mut ConnectionsPanel,
    mut pending: PendingTrust,
    expected: &HostKeyChallenge,
    kind: HostKeyMutationKind,
    readback: AppResult<HostKeyChallenge>,
    user_cancelled: bool,
    cx: &mut Context<ConnectionsPanel>,
) {
    if user_cancelled {
        panel.connection_request.cancel();
    }
    pending = reset_pending(pending);
    match classify_readback(expected, readback) {
        HostKeyMutationReadback::Trusted if user_cancelled => {
            panel.pending_trust = None;
            panel.status = Some(crate::locale::text(
                "连接已取消；端点回读确认当前指纹已保存为可信值，没有继续打开终端或文件连接。",
                "Connection cancelled. Endpoint readback confirms the current key is trusted; no terminal or file connection was opened.",
            ).to_string());
            cx.notify();
        }
        HostKeyMutationReadback::Trusted => {
            panel.pending_trust = None;
            panel.status = Some(crate::locale::text(
                "端点回读确认本次指纹已可信，正在继续原连接请求。",
                "Endpoint readback confirms this key is trusted. Continuing the original connection request.",
            ).to_string());
            panel.preflight_pending_action(expected.host_id.clone(), pending.action, cx);
        }
        HostKeyMutationReadback::Current(actual) => {
            let current_status = match &actual.status {
                HostKeyStatus::Unknown => crate::locale::text(
                    format!(
                        "回读显示当前指纹 {} 尚无可信记录。",
                        actual.fingerprint_sha256
                    ),
                    format!(
                        "Readback shows the current fingerprint {} has no trusted record.",
                        actual.fingerprint_sha256
                    ),
                ),
                HostKeyStatus::Changed {
                    previous_fingerprint_sha256,
                } => crate::locale::text(
                    format!(
                        "回读显示当前指纹 {} 与已保存指纹 {} 不同。",
                        actual.fingerprint_sha256, previous_fingerprint_sha256
                    ),
                    format!(
                        "Readback shows current fingerprint {} differs from saved fingerprint {}.",
                        actual.fingerprint_sha256, previous_fingerprint_sha256
                    ),
                ),
                HostKeyStatus::Known => crate::locale::text(
                    format!(
                        "回读显示指纹 {} 已可信，但它与本次确认的指纹不同。",
                        actual.fingerprint_sha256
                    ),
                    format!(
                        "Readback shows fingerprint {} is trusted, but it differs from the key confirmed by this request.",
                        actual.fingerprint_sha256
                    ),
                ),
            };
            if user_cancelled {
                panel.pending_trust = None;
                panel.status = Some(
                    crate::locale::text(
                        format!("连接已取消。{}", current_status),
                        format!("Connection cancelled. {}", current_status),
                    )
                    .to_string(),
                );
            } else if matches!(
                &actual.status,
                HostKeyStatus::Unknown | HostKeyStatus::Changed { .. }
            ) {
                pending.challenge = actual;
                pending.second_confirmation = false;
                panel.pending_trust = Some(pending);
                panel.status = Some(crate::locale::text(
                    format!("{} 请通过独立可信渠道重新核对后再操作。", current_status),
                    format!("{} Verify it through an independent trusted channel before taking another action.", current_status),
                ).to_string());
            } else {
                panel.pending_trust = None;
                panel.status = Some(
                    crate::locale::text(
                        format!("{} 请重新连接以按当前密钥状态检查。", current_status),
                        format!(
                            "{} Reconnect to inspect the current key state before continuing.",
                            current_status
                        ),
                    )
                    .to_string(),
                );
            }
            cx.notify();
        }
        HostKeyMutationReadback::EndpointChanged => {
            panel.pending_trust = None;
            panel.status = Some(crate::locale::text(
                "回读时 HostId、peer 或 host:port 已变化，无法证明原端点的信任状态。请重新连接并检查当前端点后再决定是否重试。",
                "The HostId, peer, or host:port changed during readback, so the original endpoint state cannot be verified. Reconnect and inspect the current endpoint before retrying.",
            ).to_string());
            cx.notify();
        }
        HostKeyMutationReadback::Failed(error) => {
            panel.pending_trust = None;
            panel.status = Some(kind.readback_failure_message(&error));
            cx.notify();
        }
    }
}
