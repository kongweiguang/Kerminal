//! 根窗口中的广播与终端日志动作编排。
//!
//! @author kongweiguang

use super::RootView;
use crate::features::terminal_tools::TerminalToolsEvent;
use gpui_kit::{Context, Entity, Window};
use kerminal_application::{
    AppError,
    sessions::{
        SessionFilter, SessionService, TerminalCommandContext, UiSessionBinding,
        broadcast::{BroadcastTargetCheck, validate_broadcast_targets},
        logging::{SessionLogStartRequest, SessionLogState},
    },
};
use kerminal_domain::{
    identity::SessionId,
    sessions::{
        ContainerSessionTarget, LocalSessionTarget, SerialSessionTarget, SessionInfo,
        SessionLifecycle, SessionOwner, SessionTarget, SshSessionTarget, TelnetSessionTarget,
    },
};

pub(super) use kerminal_application::sessions::broadcast::BroadcastTargetAvailability;

use crate::features::terminal_tools::TerminalToolsView;

use super::terminal_tools_runtime::{
    TerminalToolsOwner, TerminalToolsRequestKind, TerminalToolsRequestPolicy, owner_is_current,
    resolve_owner,
};

/// 广播和日志回执回到发起动作的子视图，不借用全局工具实体或当前活动 Tab。
pub(super) fn handle_terminal_tools_event_from(
    root: &mut RootView,
    source: Entity<TerminalToolsView>,
    event: &TerminalToolsEvent,
    _window: &mut Window,
    cx: &mut Context<RootView>,
) {
    match event {
        TerminalToolsEvent::BroadcastStart { session_ids } => {
            start_broadcast(root, source, session_ids.clone(), cx);
        }
        TerminalToolsEvent::BroadcastStop => stop_broadcast(root, source, cx),
        TerminalToolsEvent::BroadcastCancel => cancel_broadcast_draft(root, source, cx),
        TerminalToolsEvent::LogStart { session_id } => {
            run_log_action(root, source, session_id.clone(), LogAction::Start, cx);
        }
        TerminalToolsEvent::LogStop { session_id } => {
            run_log_action(root, source, session_id.clone(), LogAction::Stop, cx);
        }
        TerminalToolsEvent::LogRefresh { session_id } => {
            run_log_action(root, source, session_id.clone(), LogAction::Refresh, cx);
        }
        _ => {}
    }
}

/// 广播绑定在启动时固定为 pane 当前 UI owner/generation；异步验证不得将重连后的新代次悄悄纳入。
fn start_broadcast(
    root: &mut RootView,
    source: Entity<TerminalToolsView>,
    session_ids: Vec<SessionId>,
    cx: &mut Context<RootView>,
) {
    let Some(owner) = source_owner(root, &source, cx) else {
        return;
    };
    root.terminal_tools_runtime
        .cancel(&owner, TerminalToolsRequestKind::Broadcast);
    let generation = root.begin_broadcast_start();
    let bindings = match capture_broadcast_bindings(root, &session_ids) {
        Ok(bindings) => bindings,
        Err(unavailable) => {
            let _ = root.stop_broadcast();
            let message = broadcast_binding_failure_message(&unavailable);
            source.update(cx, |tools, cx| {
                tools.set_broadcast_active(false, cx);
                tools.set_broadcast_pending(false, cx);
                tools.set_broadcast_error(message.clone(), cx);
            });
            root.status = Some(message.into());
            cx.notify();
            return;
        }
    };
    let Some(request) = root.terminal_tools_runtime.begin(
        owner,
        TerminalToolsRequestKind::Broadcast,
        Some(generation),
        TerminalToolsRequestPolicy::ReplaceCancellable,
    ) else {
        return;
    };
    let tools = source;
    tools.update(cx, |tools, cx| tools.set_broadcast_pending(true, cx));

    let sessions = root.services.sessions.clone();
    let executor = root.services.executor.clone();
    let callback_request = request.clone();
    let task = cx.spawn(async move |this, cx| {
        let result = executor
            .run(move |cancellation| async move {
                validate_broadcast_start(sessions.as_ref(), bindings, move || {
                    cancellation.is_cancelled()
                })
                .await
            })
            .await;
        let _ =
            this.update(cx, |root, cx| {
                if !root.terminal_tools_runtime.is_current(&callback_request) {
                    return;
                }
                root.terminal_tools_runtime.finish(&callback_request);
                if !owner_is_current(root, &callback_request.owner, cx) {
                    return;
                }
                if !root.broadcast_start_is_current(generation) {
                    tools.update(cx, |tools, cx| {
                        tools.set_broadcast_pending(false, cx);
                    });
                    cx.notify();
                    return;
                }
                match result {
                    Ok(validation)
                        if validation.checks.iter().all(|check| {
                            check.availability == BroadcastTargetAvailability::Ready
                        }) && validation.binding_failures.is_empty() =>
                    {
                        let stale = stale_registered_broadcast_targets(root, &validation.bindings);
                        if !stale.is_empty() {
                            let _ = root.stop_broadcast();
                            let message = broadcast_binding_failure_message(&stale);
                            tools.update(cx, |tools, cx| {
                                tools.set_broadcast_active(false, cx);
                                tools.set_broadcast_pending(false, cx);
                                tools.set_broadcast_error(message.clone(), cx);
                            });
                            root.status = Some(message.into());
                            cx.notify();
                            return;
                        }
                        if !root.activate_broadcast(generation, validation.bindings) {
                            let _ = root.stop_broadcast();
                            let message = crate::locale::text(
                                "广播目标已变化，请重新选择后重试。",
                                "Broadcast targets changed. Select them again and retry.",
                            )
                            .to_string();
                            tools.update(cx, |tools, cx| {
                                tools.set_broadcast_active(false, cx);
                                tools.set_broadcast_pending(false, cx);
                                tools.set_broadcast_error(message.clone(), cx);
                            });
                            root.status = Some(message.into());
                            cx.notify();
                            return;
                        }
                        tools.update(cx, |tools, cx| {
                            tools.set_broadcast_active(true, cx);
                        });
                    }
                    Ok(validation) => {
                        let _ = root.stop_broadcast();
                        let message = if validation.binding_failures.is_empty() {
                            broadcast_target_failure_message(&validation.checks)
                        } else {
                            broadcast_binding_failure_message(&validation.binding_failures)
                        };
                        tools.update(cx, |tools, cx| {
                            tools.set_broadcast_active(false, cx);
                            tools.set_broadcast_pending(false, cx);
                            tools.set_broadcast_error(message.clone(), cx);
                        });
                        root.status = Some(message.into());
                    }
                    Err(error) => {
                        let _ = root.stop_broadcast();
                        let message = error.public_message().to_owned();
                        tools.update(cx, |tools, cx| {
                            tools.set_broadcast_active(false, cx);
                            tools.set_broadcast_pending(false, cx);
                            tools.set_broadcast_error(message.clone(), cx);
                        });
                        root.status = Some(message.into());
                    }
                }
                cx.notify();
            });
    });
    root.terminal_tools_runtime.install_task(&request, task);
}

pub(super) struct BroadcastStartValidation {
    pub(super) checks: Vec<BroadcastTargetCheck>,
    pub(super) bindings: Vec<UiSessionBinding>,
    pub(super) binding_failures: Vec<SessionId>,
}

/// 先校验会话仍在运行，再逐个确认服务上下文仍匹配启动瞬间捕获的 UI owner 与代次。
pub(super) async fn validate_broadcast_start(
    sessions: &dyn SessionService,
    bindings: Vec<UiSessionBinding>,
    is_cancelled: impl Fn() -> bool + Send + Sync,
) -> kerminal_application::AppResult<BroadcastStartValidation> {
    if is_cancelled() {
        return Err(AppError::cancelled("广播启动已取消"));
    }
    let session_ids = bindings
        .iter()
        .map(|binding| binding.session_id.clone())
        .collect::<Vec<_>>();
    let checks = validate_broadcast_targets(sessions, &session_ids).await?;
    let mut binding_failures = Vec::new();
    for (binding, check) in bindings.iter().zip(&checks) {
        if check.availability != BroadcastTargetAvailability::Ready {
            continue;
        }
        if is_cancelled() {
            return Err(AppError::cancelled("广播启动已取消"));
        }
        let context = sessions
            .terminal_command_context(binding.session_id.clone())
            .await?;
        if !ui_binding_matches_context(binding, &context) {
            binding_failures.push(binding.session_id.clone());
        }
    }
    Ok(BroadcastStartValidation {
        checks,
        bindings,
        binding_failures,
    })
}

/// 只接受启动事件时已注册的 UI pane binding；没有可验证 owner 的会话不能加入广播。
pub(super) fn capture_broadcast_bindings(
    root: &RootView,
    session_ids: &[SessionId],
) -> Result<Vec<UiSessionBinding>, Vec<SessionId>> {
    let mut bindings = Vec::with_capacity(session_ids.len());
    let mut unavailable = Vec::new();
    for session_id in session_ids {
        match root.pane_input_handlers.get(session_id) {
            Some(registration)
                if registration.binding.session_id == *session_id
                    && is_ui_owned_binding(&registration.binding) =>
            {
                bindings.push(registration.binding.clone());
            }
            _ => unavailable.push(session_id.clone()),
        }
    }
    if unavailable.is_empty() {
        Ok(bindings)
    } else {
        Err(unavailable)
    }
}

/// 当前注册项必须仍与启动时快照完全相同，避免迟到校验覆盖 pane 的重连代次。
pub(super) fn stale_registered_broadcast_targets(
    root: &RootView,
    bindings: &[UiSessionBinding],
) -> Vec<SessionId> {
    bindings
        .iter()
        .filter(|binding| {
            !root
                .pane_input_handlers
                .get(&binding.session_id)
                .is_some_and(|registration| {
                    broadcast_binding_is_current(binding, Some(&registration.binding))
                })
        })
        .map(|binding| binding.session_id.clone())
        .collect()
}

/// 保持广播目标必须是精确的 UI owner binding，而不是仅有相同 SessionId 的新注册项。
fn broadcast_binding_is_current(
    expected: &UiSessionBinding,
    registered: Option<&UiSessionBinding>,
) -> bool {
    is_ui_owned_binding(expected) && registered == Some(expected)
}

/// 仅接受明确 UI owner；Agent、MCP 和 service 会话不是可见 pane 的广播目标。
fn is_ui_owned_binding(binding: &UiSessionBinding) -> bool {
    matches!(&binding.owner, SessionOwner::Ui { .. })
}

/// 服务校验必须与捕获时的 session、真实 owner 和重连代次完全相同。
fn ui_binding_matches_context(
    expected: &UiSessionBinding,
    context: &TerminalCommandContext,
) -> bool {
    is_ui_owned_binding(expected)
        && context.session_id == expected.session_id
        && context.owner == expected.owner
        && context.session_generation == expected.session_generation
}

/// 把绑定缺失或重连的目标列给用户，避免静默改绑到新运行时。
fn broadcast_binding_failure_message(session_ids: &[SessionId]) -> String {
    let targets = session_ids
        .iter()
        .map(|session_id| session_id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    crate::locale::text(
        format!("广播未启动：以下目标已退出、重连或不再绑定到 UI pane：{targets}。请重新选择。"),
        format!("Broadcast was not started because these targets exited, reconnected, or are no longer bound to a UI pane: {targets}. Select them again."),
    )
    .to_string()
}

/// 停止接收新广播输入并只更新发起控件，避免状态写到非活动工具视图。
fn stop_broadcast(
    root: &mut RootView,
    source: Entity<TerminalToolsView>,
    cx: &mut Context<RootView>,
) {
    let Some(owner) = source_owner(root, &source, cx) else {
        return;
    };
    root.terminal_tools_runtime
        .cancel(&owner, TerminalToolsRequestKind::Broadcast);
    let dropped = root.stop_broadcast();
    let message = broadcast_stop_summary(dropped.broadcast_events);
    let tools = source;
    tools.update(cx, |tools, cx| {
        tools.set_broadcast_active(false, cx);
        tools.set_broadcast_pending(false, cx);
    });
    root.status = Some(message.into());
    cx.notify();
}

/// 仅清除尚未启动的目标草稿，活动广播必须通过 Stop 明确结束。
fn cancel_broadcast_draft(
    root: &mut RootView,
    source: Entity<TerminalToolsView>,
    cx: &mut Context<RootView>,
) {
    let Some(owner) = source_owner(root, &source, cx) else {
        return;
    };
    if root.broadcast_session_ids.is_some() {
        let message = crate::locale::text(
            "广播正在运行，请先停止广播。",
            "Broadcast input is active. Stop it before cancelling.",
        )
        .to_string();
        let tools = source.clone();
        tools.update(cx, |tools, cx| {
            tools.set_broadcast_error(message.clone(), cx)
        });
        root.status = Some(message.into());
    } else {
        root.terminal_tools_runtime
            .cancel(&owner, TerminalToolsRequestKind::Broadcast);
        let dropped = root.stop_broadcast();
        let tools = source;
        tools.update(cx, |tools, cx| {
            tools.set_broadcast_active(false, cx);
            tools.set_broadcast_pending(false, cx);
        });
        root.status = Some(broadcast_stop_summary(dropped.broadcast_events).into());
    }
    cx.notify();
}

/// 只报告队列确认撤销的待发送事件，不暗示正在执行的写入已中断或全部排空。
fn broadcast_stop_summary(cancelled_events: usize) -> String {
    if cancelled_events == 0 {
        crate::locale::text(
            "已停止接收新的广播输入，没有待撤销的广播输入。",
            "Broadcast input has stopped; no queued events needed cancellation.",
        )
        .to_string()
    } else {
        crate::locale::text(
            format!("已停止接收新的广播输入，撤销了 {cancelled_events} 条待发送输入。"),
            format!("Broadcast input has stopped; cancelled {cancelled_events} queued events."),
        )
        .to_string()
    }
}

/// 将失效目标与具体生命周期一一列出，便于用户调整选择后重试。
fn broadcast_target_failure_message(checks: &[BroadcastTargetCheck]) -> String {
    let failures = checks
        .iter()
        .filter_map(|check| {
            let reason = match check.availability {
                BroadcastTargetAvailability::Ready => return None,
                BroadcastTargetAvailability::Missing => {
                    crate::locale::text("会话已不存在", "session no longer exists").to_string()
                }
                BroadcastTargetAvailability::NotRunning(lifecycle) => {
                    let (lifecycle_zh, lifecycle_en) = match lifecycle {
                        SessionLifecycle::Starting => ("正在启动", "starting"),
                        SessionLifecycle::Running => ("正在运行", "running"),
                        SessionLifecycle::Closing => ("正在关闭", "closing"),
                        SessionLifecycle::Exited => ("已退出", "exited"),
                        SessionLifecycle::Closed => ("已关闭", "closed"),
                        SessionLifecycle::Failed => ("启动失败", "failed"),
                    };
                    crate::locale::text(
                        format!("会话状态：{lifecycle_zh}"),
                        format!("session state: {lifecycle_en}"),
                    )
                    .to_string()
                }
            };
            Some(format!("{}：{reason}", check.session_id.as_str()))
        })
        .collect::<Vec<_>>();
    let prefix = crate::locale::text(
        "广播未启动，以下目标不可用：",
        "Broadcast was not started. These targets are unavailable:",
    );
    format!("{}\n{}", prefix, failures.join("\n"))
}

#[derive(Clone, Copy)]
enum LogAction {
    Start,
    Stop,
    Refresh,
}

struct LogActionResult {
    state: Option<SessionLogState>,
    error: Option<String>,
}

/// 在应用 runtime 中执行日志用例，并在 start/stop 报错后读回状态以闭合不确定结果。
fn run_log_action(
    root: &mut RootView,
    source: Entity<TerminalToolsView>,
    session_id: SessionId,
    action: LogAction,
    cx: &mut Context<RootView>,
) {
    let Some(owner) = resolve_owner(root, &source, &session_id, cx) else {
        return;
    };
    let Some(logs) = root.services.session_logs.clone() else {
        let message = crate::locale::text(
            "终端日志能力尚未装配。",
            "Terminal logging is unavailable in this build.",
        )
        .to_string();
        let tools = source;
        tools.update(cx, |tools, cx| {
            tools.set_log_pending(&session_id, false, cx);
            tools.set_log_error(&session_id, message.clone(), cx);
        });
        root.status = Some(message.into());
        cx.notify();
        return;
    };

    let (request_kind, policy) = match action {
        LogAction::Start | LogAction::Stop => (
            TerminalToolsRequestKind::LogMutation,
            TerminalToolsRequestPolicy::RejectWhileRunning,
        ),
        LogAction::Refresh => (
            TerminalToolsRequestKind::LogRead,
            TerminalToolsRequestPolicy::ReplaceCancellable,
        ),
    };
    if request_kind == TerminalToolsRequestKind::LogMutation {
        root.terminal_tools_runtime
            .cancel(&owner, TerminalToolsRequestKind::LogRead);
    } else if root
        .terminal_tools_runtime
        .is_running(&owner, TerminalToolsRequestKind::LogMutation)
    {
        let message = crate::locale::text(
            "日志状态正在变更，请稍后刷新。",
            "Log state is changing. Refresh after the operation completes.",
        )
        .to_string();
        source.update(cx, |tools, cx| {
            tools.set_log_pending(&session_id, false, cx);
            tools.set_log_error(&session_id, message.clone(), cx);
        });
        root.status = Some(message.into());
        cx.notify();
        return;
    }
    let Some(request) = root
        .terminal_tools_runtime
        .begin(owner, request_kind, None, policy)
    else {
        let message = crate::locale::text(
            "日志操作仍在处理中，请稍后查看状态。",
            "A log operation is still running. Check its status shortly.",
        )
        .to_string();
        source.update(cx, |tools, cx| {
            tools.set_log_pending(&session_id, false, cx);
            tools.set_log_error(&session_id, message.clone(), cx);
        });
        root.status = Some(message.into());
        cx.notify();
        return;
    };

    let tools = source;
    tools.update(cx, |tools, cx| tools.set_log_pending(&session_id, true, cx));
    let sessions = root.services.sessions.clone();
    let executor = root.services.executor.clone();
    let request_session_id = session_id.clone();

    let callback_request = request.clone();
    let task = cx.spawn(async move |this, cx| {
        let result = executor
            .run(move |cancellation| async move {
                let operation_result = match action {
                    LogAction::Start => {
                        let session =
                            find_running_session(sessions.as_ref(), &request_session_id).await?;
                        let (shell, cwd) = session_log_metadata(&session);
                        logs.start(
                            cancellation.clone(),
                            SessionLogStartRequest {
                                session_id: request_session_id.clone(),
                                shell,
                                cwd,
                            },
                        )
                        .await
                    }
                    LogAction::Stop => {
                        logs.stop(cancellation.clone(), request_session_id.clone())
                            .await
                    }
                    LogAction::Refresh => {
                        logs.state(cancellation.clone(), request_session_id.clone())
                            .await
                    }
                };

                match operation_result {
                    Ok(state) => Ok(LogActionResult {
                        state: Some(state),
                        error: None,
                    }),
                    Err(error) => {
                        let message = error.public_message().to_owned();
                        let state = if matches!(action, LogAction::Start | LogAction::Stop)
                            && !cancellation.is_cancelled()
                        {
                            logs.state(cancellation, request_session_id).await.ok()
                        } else {
                            None
                        };
                        Ok(LogActionResult {
                            state,
                            error: Some(message),
                        })
                    }
                }
            })
            .await;

        let _ = this.update(cx, |root, cx| {
            if !root.terminal_tools_runtime.is_current(&callback_request) {
                return;
            }
            root.terminal_tools_runtime.finish(&callback_request);
            if !owner_is_current(root, &callback_request.owner, cx) {
                return;
            }
            tools.update(cx, |tools, cx| match result {
                Ok(outcome) => {
                    if let Some(state) = outcome.state {
                        tools.set_log_state(&session_id, state, cx);
                    } else {
                        tools.set_log_pending(&session_id, false, cx);
                    }
                    if let Some(message) = outcome.error {
                        tools.set_log_error(&session_id, message.clone(), cx);
                        root.status = Some(message.into());
                    }
                }
                Err(error) => {
                    let message = error.public_message().to_owned();
                    tools.set_log_pending(&session_id, false, cx);
                    tools.set_log_error(&session_id, message.clone(), cx);
                    root.status = Some(message.into());
                }
            });
            cx.notify();
        });
    });
    root.terminal_tools_runtime.install_task(&request, task);
}

/// 只从仍然注册的 pane/Agent 工具视图读取 session 绑定，避免广播或日志借用当前焦点推断目标。
fn source_owner(
    root: &RootView,
    source: &Entity<TerminalToolsView>,
    cx: &Context<RootView>,
) -> Option<TerminalToolsOwner> {
    let session_id = source.read(cx).session_id()?.clone();
    resolve_owner(root, source, &session_id, cx)
}

/// 只接受目标仍在共享会话表且 Running 的 start 请求，避免给已关闭会话创建空日志。
async fn find_running_session(
    sessions: &dyn SessionService,
    session_id: &SessionId,
) -> kerminal_application::AppResult<SessionInfo> {
    let session = sessions
        .list(SessionFilter::default())
        .await?
        .into_iter()
        .find(|session| &session.session_id == session_id)
        .ok_or_else(|| AppError::not_found("终端会话不存在"))?;
    if session.lifecycle != SessionLifecycle::Running {
        return Err(AppError::conflict("终端会话未处于运行状态，无法开始日志"));
    }
    Ok(session)
}

/// 从会话创建时可信的 target 元数据生成日志头，不读取终端正文推断 shell 或工作目录。
fn session_log_metadata(session: &SessionInfo) -> (String, Option<String>) {
    match &session.target {
        SessionTarget::Local(LocalSessionTarget {
            program,
            working_directory,
            ..
        }) => (
            program.to_string_lossy().into_owned(),
            working_directory
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        ),
        SessionTarget::Ssh(SshSessionTarget {
            working_directory, ..
        }) => (
            "ssh".to_owned(),
            working_directory
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        ),
        SessionTarget::Telnet(TelnetSessionTarget { .. }) => ("telnet".to_owned(), None),
        SessionTarget::Serial(SerialSessionTarget { .. }) => ("serial".to_owned(), None),
        SessionTarget::Container(ContainerSessionTarget { .. }) => ("container".to_owned(), None),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/terminal_tool_actions.rs"]
mod tests;
