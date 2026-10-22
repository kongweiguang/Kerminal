//! 外部启动目标的用户名续接、密码提示、信任预检与消费者移交。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, Focusable as _, IntoElement, Window,
    component::{
        Disableable,
        button::{Button, ButtonVariants},
        input::Input,
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::platform::external_launch::{
    ExternalLaunchIntent, ExternalLaunchOutcome, ExternalLaunchRequest, ExternalRoutedTarget,
    ExternalTargetClassification,
};
use kerminal_domain::connections::RemoteProtocol;

use crate::theme::ThemeTokens;

use super::{panel::ConnectionsPanel, panel::PendingAction};

impl ConnectionsPanel {
    /// Root 观察同一面板的待确认事实，把 trust、一次性认证和本地主机删除提升到全工作区 modal。
    ///
    /// @author kongweiguang
    pub(crate) fn root_confirmation_active(&self) -> bool {
        self.pending_trust.is_some()
            || self.pending_external_username.is_some()
            || self.pending_external_password.is_some()
            || self.local_hosts.pending_delete.is_some()
    }

    /// 全屏 modal 先获得 Root trap 焦点；需要输入时再把焦点交给当前唯一认证字段。
    ///
    /// @author kongweiguang
    pub(crate) fn focus_root_confirmation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = if self.pending_external_username.is_some() {
            Some(&self.external_username_input)
        } else if self.pending_external_password.is_some() {
            Some(&self.external_password_input)
        } else {
            None
        };
        let Some(input) = input else {
            return;
        };
        let focus = input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
    }

    /// Escape 只调用当前确认所有者的取消路径；删除写入中或结果未知时保留原确认快照供核对。
    ///
    /// @author kongweiguang
    pub(crate) fn cancel_root_confirmation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_trust.is_some() {
            self.cancel_pending_trust(cx);
        } else if self.pending_external_username.is_some() {
            self.cancel_external_username(window, cx);
        } else if self.pending_external_password.is_some() {
            self.cancel_external_password(window, cx);
        } else if self.local_hosts.pending_delete.is_some()
            && !self.local_hosts.saving
            && !self.local_hosts.save_outcome_unknown
        {
            self.cancel_delete_local_host(cx);
        }
    }

    /// 保存未物化的外部请求，要求用户显式给出用户名，避免猜当前账户或注册半成品目标。
    pub fn request_external_username(
        &mut self,
        request: ExternalLaunchRequest,
        cx: &mut Context<Self>,
    ) {
        if self.pending_external_username.is_some()
            || self.pending_external_password.is_some()
            || self.pending_trust.is_some()
        {
            self.status = Some(
                crate::locale::text(
                    "请先完成或取消当前连接确认，再处理新的外部请求。",
                    "Finish or cancel the current connection prompt before handling another external request.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if request
            .target
            .username
            .as_deref()
            .is_some_and(|username| !username.trim().is_empty())
        {
            self.status = Some(
                crate::locale::text(
                    "该外部请求已包含用户名，不能作为缺少用户名的请求继续。",
                    "This external request already contains a username and cannot continue through the missing-username prompt.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if !matches!(
            request.target.protocol,
            RemoteProtocol::Ssh | RemoteProtocol::Sftp
        ) {
            self.status = Some(
                crate::locale::text(
                    "当前只支持为 SSH/SFTP 外部请求补齐用户名。",
                    "Username entry is supported only for external SSH/SFTP requests.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if self.external_launch.is_none() {
            self.status = Some(
                crate::locale::text(
                    "外部启动服务不可用；尚未创建连接目标。",
                    "The external launch service is unavailable; no connection target was created.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }

        self.cancel_connection_request();
        self.pending_external_username = Some(request);
        self.external_username_submitting = false;
        self.status = Some(
            crate::locale::text(
                "请输入本次连接的登录用户名；系统不会猜测当前账户。",
                "Enter the login username for this request; the current account will not be guessed.",
            )
            .to_string(),
        );
        cx.notify();
    }

    /// 校验显式账号后用原解析请求续接应用路由，再交给现有密码和 host-key 流程。
    pub(super) fn submit_external_username(
        &mut self,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) {
        if self.external_username_submitting {
            return;
        }
        let Some(request) = self.pending_external_username.as_ref().cloned() else {
            return;
        };
        let Some(service) = self.external_launch.clone() else {
            self.status = Some(
                crate::locale::text(
                    "外部启动服务不可用；尚未创建连接目标。",
                    "The external launch service is unavailable; no connection target was created.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };
        let username = match validate_external_username(&super::form::read_input(
            &self.external_username_input,
            cx,
        )) {
            Ok(username) => username,
            Err(error) => {
                self.status = Some(super::panel::localized_error(&error));
                cx.notify();
                return;
            }
        };

        super::form::clear_input(&self.external_username_input, window, cx);
        self.external_username_generation = self.external_username_generation.wrapping_add(1);
        let generation = self.external_username_generation;
        self.external_username_submitting = true;
        let executor = self.executor.clone();
        let worker_request = request.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    if cancellation.is_cancelled() {
                        return Err(kerminal_application::AppError::cancelled(
                            "外部连接用户名补齐已取消",
                        ));
                    }
                    let outcome = service.supply_username(request, username).await?;
                    if cancellation.is_cancelled() {
                        // 路由可能已注册临时目标；丢弃未移交结果会按 RAII 释放其 lease。
                        drop(outcome);
                        return Err(kerminal_application::AppError::cancelled(
                            "外部连接用户名补齐已取消",
                        ));
                    }
                    Ok(outcome)
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |panel, cx| {
                    let Some(result) = take_current_external_username_result(
                        generation,
                        panel.external_username_generation,
                        result,
                    ) else {
                        return;
                    };
                    panel.external_username_submitting = false;
                    match result {
                        Ok(ExternalLaunchOutcome::Ready { target })
                        | Ok(ExternalLaunchOutcome::PasswordRequired { target }) => {
                            panel.pending_external_username.take();
                            panel.request_external_target(target, cx);
                        }
                        Ok(ExternalLaunchOutcome::UsernameRequired { request }) => {
                            panel.pending_external_username = Some(request);
                            panel.status = Some(crate::locale::text(
                                "目标仍缺少有效用户名，请检查输入后重试。",
                                "The target still needs a valid username; review the input and try again.",
                            ).to_string());
                            cx.notify();
                        }
                        Ok(ExternalLaunchOutcome::Ignored) => {
                            panel.pending_external_username.take();
                            panel.status = Some(crate::locale::text(
                                "外部请求未解析为 SSH/SFTP 连接，未创建目标。",
                                "The external request did not resolve to an SSH/SFTP connection; no target was created.",
                            ).to_string());
                            cx.notify();
                        }
                        Err(error) => {
                            panel.pending_external_username = Some(worker_request);
                            panel.status = Some(super::panel::localized_error(&error));
                            cx.notify();
                        }
                    }
                });
            });
        })
        .detach();
        self.status = Some(
            crate::locale::text(
                "正在使用所填用户名继续外部连接…",
                "Continuing the external connection with the entered username…",
            )
            .to_string(),
        );
        cx.notify();
    }

    /// 取消用户名提示并使在途续接结果失效；请求尚未持有临时目标 lease。
    pub(super) fn cancel_external_username(
        &mut self,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_connection_request();
        self.external_username_generation = self.external_username_generation.wrapping_add(1);
        self.external_username_submitting = false;
        self.pending_external_username.take();
        super::form::clear_input(&self.external_username_input, window, cx);
        self.status = Some(
            crate::locale::text(
                "已取消外部连接用户名提示，尚未创建临时连接目标。",
                "The external username prompt was cancelled; no temporary connection target was created.",
            )
            .to_string(),
        );
        cx.notify();
    }

    /// 外部目标沿用现有 SSH trust service；一个未结束请求独占面板提示以免口令串到其他目标。
    pub fn request_external_target(
        &mut self,
        target: ExternalRoutedTarget,
        cx: &mut Context<Self>,
    ) {
        if self.pending_external_username.is_some()
            || self.pending_external_password.is_some()
            || self.pending_trust.is_some()
        {
            self.status = Some(
                crate::locale::text(
                    "请先完成或取消当前连接确认，再处理新的外部请求。",
                    "Finish or cancel the current connection prompt before handling another external request.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if !external_intent_supported(target.identity.protocol, &target.intent) {
            self.status = Some(
                crate::locale::text(
                    "外部请求的协议与动作不匹配，连接已取消。",
                    "The external request protocol does not match its action; the request was cancelled.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        self.cancel_connection_request();
        if target.authentication_required {
            if self.external_launch.is_none() {
                self.status = Some(
                    crate::locale::text(
                        "外部密码服务不可用；未启动连接。",
                        "The external password service is unavailable; no connection was started.",
                    )
                    .to_string(),
                );
                cx.notify();
                return;
            }
            self.pending_external_password = Some(target);
            self.external_authenticating = false;
            self.status = Some(
                crate::locale::text(
                    "请输入本次外部连接密码。密码只保存在临时内存目标中。",
                    "Enter the password for this external connection. It is stored only in the temporary in-memory target.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        self.preflight_external_target(target, cx);
    }

    /// 只将一次性输入交给同一外部启动服务；应用入口负责立即零化，失败时保留目标供用户重输。
    pub(super) fn submit_external_password(
        &mut self,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) {
        if self.external_authenticating {
            return;
        }
        let Some(target) = self.pending_external_password.as_ref().cloned() else {
            return;
        };
        let Some(service) = self.external_launch.clone() else {
            self.status = Some(
                crate::locale::text(
                    "外部密码服务不可用；未启动连接。",
                    "The external password service is unavailable; no connection was started.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };

        let password = super::form::read_input(&self.external_password_input, cx);
        if password.is_empty() || password.chars().any(char::is_control) {
            self.status = Some(
                crate::locale::text(
                    "密码不能为空，也不能包含控制字符。",
                    "Password cannot be empty or contain control characters.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }

        super::form::clear_input(&self.external_password_input, window, cx);
        self.external_auth_generation = self.external_auth_generation.wrapping_add(1);
        let generation = self.external_auth_generation;
        self.external_authenticating = true;
        let executor = self.executor.clone();
        let worker_lease = target.lease.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    if cancellation.is_cancelled() {
                        return Err(kerminal_application::AppError::cancelled(
                            "外部连接密码输入已取消",
                        ));
                    }
                    service.provide_password(&worker_lease, password).await
                })
                .await;
            cx.update(|app| {
                let _ = this.update(app, |panel, cx| {
                    if panel.external_auth_generation != generation {
                        return;
                    }
                    panel.external_authenticating = false;
                    match result {
                        Ok(()) => {
                            panel.pending_external_password.take();
                            panel.status = Some(crate::locale::text(
                                "临时凭据已保存到内存目标，正在检查 SSH 指纹…",
                                "Temporary credentials are ready in memory; checking SSH host keys…",
                            ).to_string());
                            panel.preflight_external_target(target, cx);
                        }
                        Err(error) => {
                            panel.status = Some(super::panel::localized_error(&error));
                            cx.notify();
                        }
                    }
                });
            });
        })
        .detach();
        self.status = Some(
            crate::locale::text(
                "正在设置本次连接凭据…",
                "Preparing credentials for this connection…",
            )
            .to_string(),
        );
        cx.notify();
    }

    /// 取消密码提示清空输入并释放当前面板持有的临时目标 lease。
    pub(super) fn cancel_external_password(
        &mut self,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_connection_request();
        self.external_auth_generation = self.external_auth_generation.wrapping_add(1);
        self.external_authenticating = false;
        self.pending_external_password.take();
        super::form::clear_input(&self.external_password_input, window, cx);
        self.status = Some(
            crate::locale::text(
                "已取消外部连接请求，临时目标将随最后一个引用释放。",
                "External connection cancelled; the temporary target is released with its final lease.",
            )
            .to_string(),
        );
        cx.notify();
    }

    /// 将密码已就绪的显式目标送入既有 host-key 流程；每个 jump 和目标都检查同一 lease。
    fn preflight_external_target(&mut self, target: ExternalRoutedTarget, cx: &mut Context<Self>) {
        let host_id = target.host_id().clone();
        let jump_count = target.identity.jump_hosts.len();
        self.preflight_host_key_count(host_id, jump_count, PendingAction::External(target), cx);
    }
}

/// 输入框只接受显式、非空且无控制字符的账号名；应用用例仍重复执行业务校验。
fn validate_external_username(value: &str) -> Result<String, kerminal_application::AppError> {
    let username = value.trim();
    if username.is_empty() || username.chars().any(char::is_control) {
        return Err(kerminal_application::AppError::invalid(
            "外部连接用户名无效",
        ));
    }
    Ok(username.to_owned())
}

/// 只把当前提示世代的异步结果交回界面；过期结果在此释放其 RAII 临时目标租约。
fn take_current_external_username_result(
    expected_generation: u64,
    current_generation: u64,
    result: Result<ExternalLaunchOutcome, kerminal_application::AppError>,
) -> Option<Result<ExternalLaunchOutcome, kerminal_application::AppError>> {
    (expected_generation == current_generation).then_some(result)
}

/// 判断协议和启动动作是否成对；不允许把输入转成另一协议或静默忽略参数。
fn external_intent_supported(protocol: RemoteProtocol, intent: &ExternalLaunchIntent) -> bool {
    matches!(
        (protocol, intent),
        (
            RemoteProtocol::Ssh,
            ExternalLaunchIntent::SshTerminal { .. }
        ) | (
            RemoteProtocol::Sftp,
            ExternalLaunchIntent::SftpTransfer { .. }
        )
    )
}

/// SHA-256 Base64 指纹大小写敏感，只允许去除首尾空白后精确相等。
pub(super) fn expected_fingerprint_matches(expected: &str, observed: &str) -> bool {
    expected.trim() == observed.trim()
}

/// 显示未注册目标的显式用户名步骤；没有默认账户、保存凭据或临时连接目标可继承，卡片宽度受根 modal 视口约束。
///
/// @author kongweiguang
pub(super) fn render_external_username_prompt(
    panel: &mut ConnectionsPanel,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> Option<impl IntoElement> {
    let request = panel.pending_external_username.as_ref()?;
    let address = format!("{}:{}", request.target.host, request.target.port);
    let protocol = match request.target.protocol {
        RemoteProtocol::Ssh => "SSH",
        RemoteProtocol::Sftp => "SFTP",
        _ => "Remote",
    };
    Some(
        div()
            .id("connections-external-username")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui_kit::hsla(0., 0., 0., 0.48))
            .child(
                div()
                    .w_full()
                    .max_w(px(470.))
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .bg(theme.panel)
                    .border_1()
                    .border_color(theme.accent)
                    .rounded_md()
                    .text_color(theme.foreground)
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(gpui_kit::FontWeight::BOLD)
                            .child(crate::locale::text(
                                "需要连接用户名",
                                "Connection username required",
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme.muted)
                            .child(format!("{protocol} · {address}")),
                    )
                    .child(Input::new(&panel.external_username_input).w_full())
                    .child(
                        div()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "请明确输入远端登录用户名。系统不会猜测当前用户；输入前尚未创建临时连接目标。",
                                "Enter the remote login username. The current account is never guessed; no temporary target exists before submission.",
                            )),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("connections-external-username-cancel")
                                    .compact()
                                    .label(crate::locale::text("取消", "Cancel"))
                                    .on_click(cx.listener(
                                        |panel, _: &ClickEvent, window, cx| {
                                            panel.cancel_external_username(window, cx)
                                        },
                                    )),
                            )
                            .child(
                                Button::new("connections-external-username-submit")
                                    .compact()
                                    .primary()
                                    .disabled(panel.external_username_submitting)
                                    .label(crate::locale::text("继续", "Continue"))
                                    .on_click(cx.listener(
                                        |panel, _: &ClickEvent, window, cx| {
                                            panel.submit_external_username(window, cx)
                                        },
                                    )),
                            ),
                    ),
            ),
    )
}

/// 绘制一次性密码提示并说明保存匹配分类仅用于安全提示、不继承旧 Vault 凭据；卡片宽度受根 modal 视口约束。
///
/// @author kongweiguang
pub(super) fn render_external_password_prompt(
    panel: &mut ConnectionsPanel,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> Option<impl IntoElement> {
    let target = panel.pending_external_password.as_ref()?;
    let endpoint = &target.identity.endpoint;
    let address = format!("{}@{}:{}", endpoint.username, endpoint.host, endpoint.port);
    let classification = external_classification_text(&target.classification);
    Some(
        div()
            .id("connections-external-password")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui_kit::hsla(0., 0., 0., 0.48))
            .child(
                div()
                    .w_full()
                    .max_w(px(470.))
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .bg(theme.panel)
                    .border_1()
                    .border_color(theme.accent)
                    .rounded_md()
                    .text_color(theme.foreground)
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(gpui_kit::FontWeight::BOLD)
                            .child(crate::locale::text(
                                "外部连接需要密码",
                                "External connection password",
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme.muted)
                            .child(address),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.muted)
                            .child(classification),
                    )
                    .child(Input::new(&panel.external_password_input).w_full())
                    .child(
                        div()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "密码只进入本次进程内目标，关闭全部相关终端或文件面板后会零化。",
                                "The password is held only by this process-local target and is zeroized after its terminal and file consumers release their leases.",
                            )),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("connections-external-password-cancel")
                                    .compact()
                                    .label(crate::locale::text("取消", "Cancel"))
                                    .on_click(cx.listener(
                                        |panel, _: &ClickEvent, window, cx| {
                                            panel.cancel_external_password(window, cx)
                                        },
                                    )),
                            )
                            .child(
                                Button::new("connections-external-password-submit")
                                    .compact()
                                    .primary()
                                    .disabled(panel.external_authenticating)
                                    .label(crate::locale::text(
                                        "验证并继续",
                                        "Verify and continue",
                                    ))
                                    .on_click(cx.listener(
                                        |panel, _: &ClickEvent, window, cx| {
                                            panel.submit_external_password(window, cx)
                                        },
                                    )),
                            ),
                    ),
            ),
    )
}

/// 把 route classification 转成不会误示为凭据授权的本地化说明。
pub(super) fn external_classification_text(
    classification: &ExternalTargetClassification,
) -> gpui_kit::SharedString {
    match classification {
        ExternalTargetClassification::Known { .. } => crate::locale::text(
            "目标与保存的主机匹配；本次仍使用外部请求提供的认证方式。",
            "The target matches a saved host; this request still uses its own authentication method.",
        ),
        ExternalTargetClassification::RestrictedUnknown => crate::locale::text(
            "没有匹配的保存主机；继续前请核对目标和指纹。",
            "No saved host matched; verify the target and key before continuing.",
        ),
        ExternalTargetClassification::LookupUnavailable => crate::locale::text(
            "无法完成保存主机匹配；仍需核对目标和指纹。",
            "Saved-host matching was unavailable; verify the target and key before continuing.",
        ),
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/connections_external.rs"]
mod tests;
