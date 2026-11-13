//! 外部启动目标的用户名续接、密码提示与消费者移交。
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

use super::panel::{ConnectionsEvent, ConnectionsPanel, PendingAction};

impl ConnectionsPanel {
    /// 凭据卡片的失败/进度只写专用字段；普通面板 status 在 prompt-only 模式下不可见。
    ///
    /// @author kongweiguang
    fn set_external_prompt_error(&mut self, text: impl Into<gpui_kit::SharedString>) {
        self.external_prompt_message = Some(super::panel::ExternalPromptMessage {
            text: text.into(),
            is_error: true,
        });
    }

    /// 进行中消息使用普通文本色，避免用户把“正在验证”误读成失败。
    ///
    /// @author kongweiguang
    fn set_external_prompt_progress(&mut self, text: impl Into<gpui_kit::SharedString>) {
        self.external_prompt_message = Some(super::panel::ExternalPromptMessage {
            text: text.into(),
            is_error: false,
        });
    }

    /// 打开、取消、成功移交或关闭提示时清空，避免下一条请求继承上一条错误。
    ///
    /// @author kongweiguang
    fn clear_external_prompt_message(&mut self) {
        self.external_prompt_message = None;
    }

    /// 远端主机删除只在目标行仍可见时提升为模态；配置重载移除该行后不能留下不可见 modal。
    ///
    /// @author kongweiguang
    pub(super) fn host_deletion_confirmation_visible(&self) -> bool {
        self.pending_host_deletion
            .as_ref()
            .is_some_and(|pending| self.rows.iter().any(|host| host.id == pending.host_id))
    }

    /// 分组删除同样需要当前分组快照仍包含目标，避免被替换后的确认层保持不可见。
    ///
    /// @author kongweiguang
    pub(super) fn group_deletion_confirmation_visible(&self) -> bool {
        self.pending_group_deletion
            .as_ref()
            .is_some_and(|pending| self.groups.iter().any(|group| group.id == pending.group_id))
    }

    /// Root 观察同一面板的待确认事实，把一次性认证和本地主机删除提升到全工作区 modal。
    ///
    /// @author kongweiguang
    pub(crate) fn root_confirmation_active(&self) -> bool {
        self.pending_external_username.is_some()
            || self.pending_external_password.is_some()
            || self.host_deletion_confirmation_visible()
            || self.group_deletion_confirmation_visible()
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
        if self.pending_external_username.is_some() {
            self.cancel_external_username(window, cx);
        } else if self.pending_external_password.is_some() {
            self.cancel_external_password(window, cx);
        } else if self.pending_host_deletion.is_some() {
            self.cancel_host_deletion(cx);
            if self.pending_host_deletion.is_none() {
                self.status = Some(
                    crate::locale::text(
                        "已取消删除主机，配置未改变。",
                        "Host deletion was cancelled; configuration is unchanged.",
                    )
                    .to_string(),
                );
            }
            cx.notify();
        } else if self.pending_group_deletion.is_some() {
            self.cancel_group_deletion(cx);
            if self.pending_group_deletion.is_none() {
                self.status = Some(
                    crate::locale::text(
                        "已取消删除分组，配置未改变。",
                        "Group deletion was cancelled; configuration is unchanged.",
                    )
                    .to_string(),
                );
            }
            cx.notify();
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
        if self.pending_external_username.is_some() || self.pending_external_password.is_some() {
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

        self.cancel_connection_request(cx);
        self.pending_external_username = Some(request);
        self.external_username_submitting = false;
        self.clear_external_prompt_message();
        self.status = Some(
            crate::locale::text(
                "请输入本次连接的登录用户名；系统不会猜测当前账户。",
                "Enter the login username for this request; the current account will not be guessed.",
            )
            .to_string(),
        );
        cx.notify();
    }

    /// 校验显式账号后用原解析请求续接应用路由，再交给现有密码与移交流程。
    /// 普通账号在受理期间保留且禁止编辑，失败可直接重试；仅成功移交或取消清空，避免丢稿或串到下一请求。
    /// @author kongweiguang
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
            self.set_external_prompt_error(crate::locale::text(
                "外部启动服务不可用；尚未创建连接目标。",
                "The external launch service is unavailable; no connection target was created.",
            ));
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
                let message = super::panel::localized_error(&error);
                self.set_external_prompt_error(message.clone());
                self.status = Some(message);
                cx.notify();
                return;
            }
        };

        self.external_username_generation = self.external_username_generation.wrapping_add(1);
        let generation = self.external_username_generation;
        self.external_username_submitting = true;
        self.set_external_prompt_progress(crate::locale::text(
            "正在解析目标…",
            "Resolving the target…",
        ));
        let executor = self.executor.clone();
        let worker_request = request.clone();
        cx.spawn_in(window, async move |this, cx| {
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
            let _ = cx.update(|window, app| {
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
                            super::form::clear_input(&panel.external_username_input, window, cx);
                            panel.pending_external_username.take();
                            panel.clear_external_prompt_message();
                            panel.request_external_target(target, cx);
                        }
                        Ok(ExternalLaunchOutcome::UsernameRequired { request }) => {
                            panel.pending_external_username = Some(request);
                            let message = crate::locale::text(
                                "目标仍缺少有效用户名，请检查输入后重试。",
                                "The target still needs a valid username; review the input and try again.",
                            );
                            panel.set_external_prompt_error(message.clone());
                            panel.status = Some(message.to_string());
                            cx.notify();
                        }
                        Ok(ExternalLaunchOutcome::Ignored) => {
                            super::form::clear_input(&panel.external_username_input, window, cx);
                            panel.pending_external_username.take();
                            panel.clear_external_prompt_message();
                            panel.status = Some(crate::locale::text(
                                "外部请求未解析为 SSH/SFTP 连接，未创建目标。",
                                "The external request did not resolve to an SSH/SFTP connection; no target was created.",
                            ).to_string());
                            cx.notify();
                        }
                        Err(error) => {
                            panel.pending_external_username = Some(worker_request);
                            let message = super::panel::localized_error(&error);
                            panel.set_external_prompt_error(message.clone());
                            panel.status = Some(message);
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
        self.cancel_connection_request(cx);
        self.external_username_generation = self.external_username_generation.wrapping_add(1);
        self.external_username_submitting = false;
        self.pending_external_username.take();
        self.clear_external_prompt_message();
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

    /// 外部目标需要密码时先收齐一次性凭据；一个未结束请求独占面板提示以免口令串到其他目标。
    pub fn request_external_target(
        &mut self,
        target: ExternalRoutedTarget,
        cx: &mut Context<Self>,
    ) {
        if self.pending_external_username.is_some() || self.pending_external_password.is_some() {
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
        self.cancel_connection_request(cx);
        if target.authentication_required {
            if self.external_launch.is_none() {
                self.set_external_prompt_error(crate::locale::text(
                    "外部密码服务不可用；未启动连接。",
                    "The external password service is unavailable; no connection was started.",
                ));
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
            self.clear_external_prompt_message();
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
        self.clear_external_prompt_message();
        self.emit_action(PendingAction::External(target), cx);
    }

    /// 只有同一 SSH agent 目标可进入密码重试；面板忙时返回未受理以保留上层待处理请求。
    ///
    /// @author kongweiguang
    pub(crate) fn request_external_password_retry(
        &mut self,
        mut target: ExternalRoutedTarget,
        cx: &mut Context<Self>,
    ) -> bool {
        if !target.password_retry_available
            || target.identity.protocol != RemoteProtocol::Ssh
            || !matches!(&target.intent, ExternalLaunchIntent::SshTerminal { .. })
        {
            self.status = Some(
                crate::locale::text(
                    "该外部连接不能切换为密码重试。",
                    "This external connection cannot retry using a password.",
                )
                .to_string(),
            );
            cx.notify();
            return false;
        }
        if self.root_confirmation_active() || self.external_launch.is_none() {
            return false;
        }
        target.authentication_required = true;
        target.password_retry_available = false;
        self.request_external_target(target, cx);
        self.pending_external_password.is_some()
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
            self.set_external_prompt_error(crate::locale::text(
                "外部密码服务不可用；未启动连接。",
                "The external password service is unavailable; no connection was started.",
            ));
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
            let message = crate::locale::text(
                "密码不能为空，也不能包含控制字符。",
                "Password cannot be empty or contain control characters.",
            );
            self.set_external_prompt_error(message.clone());
            self.status = Some(message.to_string());
            cx.notify();
            return;
        }

        super::form::clear_input(&self.external_password_input, window, cx);
        self.external_auth_generation = self.external_auth_generation.wrapping_add(1);
        let generation = self.external_auth_generation;
        self.external_authenticating = true;
        self.set_external_prompt_progress(crate::locale::text(
            "正在验证凭据…",
            "Verifying credentials…",
        ));
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
                            panel.clear_external_prompt_message();
                            panel.status = Some(crate::locale::text(
                                "临时凭据已保存到内存目标，正在移交外部连接…",
                                "Temporary credentials are ready in memory; handing off the external connection…",
                            ).to_string());
                            panel.emit_action(PendingAction::External(target), cx);
                        }
                        Err(error) => {
                            let message = super::panel::localized_error(&error);
                            panel.set_external_prompt_error(message.clone());
                            panel.status = Some(message);
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

    /// 内存凭据提交期间不能取消，避免提交结果迟到后悄然改变仍持有的外部目标。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_external_password(
        &mut self,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) {
        if self.external_authenticating {
            return;
        }
        self.cancel_connection_request(cx);
        self.external_auth_generation = self.external_auth_generation.wrapping_add(1);
        self.external_authenticating = false;
        let host_id = self
            .pending_external_password
            .take()
            .map(|target| target.host_id().clone());
        super::form::clear_input(&self.external_password_input, window, cx);
        self.clear_external_prompt_message();
        self.status = Some(
            crate::locale::text(
                "已取消外部连接请求，临时目标将随最后一个引用释放。",
                "External connection cancelled; the temporary target is released with its final lease.",
            )
            .to_string(),
        );
        if let Some(host_id) = host_id {
            cx.emit(ConnectionsEvent::ExternalTargetCancelled { host_id });
        }
        cx.notify();
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

/// 显示未注册目标的显式用户名步骤；没有默认账户、保存凭据或临时连接目标可继承，卡片宽度受根 modal 视口约束。
/// 遮罩使用应用决策层令牌，保持浅深和自定义内置色板中的一致层次。
/// 续接期间冻结账号编辑，保留原输入供明确失败后继续；取消仍使该请求代次失效。
///
/// @author kongweiguang
pub(super) fn render_external_username_prompt(
    panel: &mut ConnectionsPanel,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> Option<impl IntoElement> {
    let request = panel.pending_external_username.as_ref()?;
    let prompt_message = panel.external_prompt_message.clone();
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
            .bg(theme.dialog_scrim)
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
                    .child(
                        Input::new(&panel.external_username_input)
                            .disabled(panel.external_username_submitting)
                            .w_full(),
                    )
                    .child(
                        div()
                            .text_size(theme.text_xs)
                            .text_color(theme.muted)
                            .child(crate::locale::text(
                                "请明确输入远端登录用户名。系统不会猜测当前用户；输入前尚未创建临时连接目标。",
                                "Enter the remote login username. The current account is never guessed; no temporary target exists before submission.",
                            )),
                    )
                    .children(prompt_message.map(|message| {
                        div()
                            .debug_selector(|| {
                                "connections-external-username-message".to_owned()
                            })
                            .text_size(theme.text_xs)
                            .text_color(if message.is_error {
                                theme.error
                            } else {
                                theme.muted
                            })
                            .child(message.text)
                    }))
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
/// 遮罩与同级确认共用主题令牌，不把单一主题的黑色强度写入业务视图。
///
/// @author kongweiguang
pub(super) fn render_external_password_prompt(
    panel: &mut ConnectionsPanel,
    theme: ThemeTokens,
    cx: &mut Context<ConnectionsPanel>,
) -> Option<impl IntoElement> {
    let target = panel.pending_external_password.as_ref()?;
    let prompt_message = panel.external_prompt_message.clone();
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
            .bg(theme.dialog_scrim)
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
                    .child(
                        Input::new(&panel.external_password_input)
                            .disabled(panel.external_authenticating)
                            .w_full(),
                    )
                    .children(prompt_message.map(|message| {
                        div()
                            .debug_selector(|| {
                                "connections-external-password-message".to_owned()
                            })
                            .text_size(theme.text_xs)
                            .text_color(if message.is_error {
                                theme.error
                            } else {
                                theme.muted
                            })
                            .child(message.text)
                    }))
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
                                    .disabled(panel.external_authenticating)
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
            "没有匹配的保存主机；继续前请核对目标地址和账号。",
            "No saved host matched; verify the target and account before continuing.",
        ),
        ExternalTargetClassification::LookupUnavailable => crate::locale::text(
            "无法完成保存主机匹配；仍需核对目标地址和账号。",
            "Saved-host matching was unavailable; verify the target and account before continuing.",
        ),
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/connections_external.rs"]
mod tests;

#[cfg(test)]
#[path = "../../../tests/unit/external_prompt_support.rs"]
mod external_prompt_support;
