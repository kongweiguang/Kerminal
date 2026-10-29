//! 外部协议状态读取与用户明确触发的注册变更。
//!
//! @author kongweiguang

use gpui_kit::Context;
use kerminal_application::{AppError, PublicErrorCode, platform::ExternalProtocolStatus};

use super::PreferencesPanel;

/// 协议变更方向作为 UI 回执使用，配置仍由 application 的 typed service 管理。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::features::preferences) enum ExternalProtocolAction {
    Register,
    Unregister,
}

/// 操作异常后携带独立状态回读，避免把旧快照当成操作结果继续开放写入。
enum ExternalProtocolChangeResult {
    Confirmed(ExternalProtocolStatus),
    Mismatch {
        operation_status: ExternalProtocolStatus,
        readback: Result<ExternalProtocolStatus, AppError>,
    },
    Failed {
        error: AppError,
        readback: Result<ExternalProtocolStatus, AppError>,
    },
}

/// 注册/注销只在平台支持、状态已读且无未回读事务时开放，避免基于陈旧状态重试。
///
/// @author kongweiguang
pub(in crate::features::preferences) fn external_protocol_action(
    service_available: bool,
    status: Option<ExternalProtocolStatus>,
    busy: bool,
    readback_required: bool,
) -> Option<ExternalProtocolAction> {
    if !service_available || busy || readback_required {
        return None;
    }

    let status = status?;
    if !status.supported {
        return None;
    }

    Some(if status.registered {
        ExternalProtocolAction::Unregister
    } else {
        ExternalProtocolAction::Register
    })
}

impl PreferencesPanel {
    /// 通过受监督 application executor 读取协议关联，避免平台查询阻塞 GPUI 事件线程。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn refresh_external_protocol(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if self.external_protocol_loading {
            return;
        }
        let Some(service) = self.external_protocol.clone() else {
            self.external_protocol_status = None;
            self.external_protocol_loading = false;
            self.external_protocol_operation = None;
            self.external_protocol_error_is_operation = false;
            self.external_protocol_error = Some(
                crate::locale::text(
                    "外部协议状态服务未由应用装配。",
                    "The external protocol status service is unavailable.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };

        self.external_protocol_generation = self.external_protocol_generation.wrapping_add(1);
        let generation = self.external_protocol_generation;
        let executor = self.executor.clone();
        self.external_protocol_loading = true;
        self.external_protocol_operation = None;
        self.external_protocol_status = None;
        self.external_protocol_error = None;
        self.external_protocol_error_is_operation = false;
        self.external_protocol_last_result = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = executor
                .run(|cancellation| async move {
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled("协议状态读取已取消"));
                    }
                    service.status().await
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                if generation != panel.external_protocol_generation {
                    return;
                }
                panel.external_protocol_loading = false;
                match result {
                    Ok(status) => {
                        panel.external_protocol_status = Some(status);
                        panel.external_protocol_error = None;
                        panel.external_protocol_error_is_operation = false;
                        panel.external_protocol_requires_refresh = false;
                        panel.external_protocol_last_result = None;
                    }
                    Err(error) => {
                        panel.external_protocol_error = Some(super::localized_error(&error));
                        panel.external_protocol_error_is_operation = false;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 操作回执异常时先自动读取真实状态；只有读回成功才按新状态继续开放注册操作。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn change_external_protocol(
        &mut self,
        action: ExternalProtocolAction,
        cx: &mut Context<Self>,
    ) {
        if external_protocol_action(
            self.external_protocol.is_some(),
            self.external_protocol_status,
            self.external_protocol_loading,
            self.external_protocol_requires_refresh,
        ) != Some(action)
        {
            return;
        }
        let Some(service) = self.external_protocol.clone() else {
            return;
        };

        let prior_status = self.external_protocol_status;
        self.external_protocol_generation = self.external_protocol_generation.wrapping_add(1);
        let generation = self.external_protocol_generation;
        let executor = self.executor.clone();
        self.external_protocol_loading = true;
        self.external_protocol_operation = Some(action);
        self.external_protocol_status = None;
        self.external_protocol_error = None;
        self.external_protocol_error_is_operation = false;
        self.external_protocol_last_result = None;
        self.external_protocol_outcome_unknown = false;
        self.external_protocol_requires_refresh = false;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled("协议关联变更已取消"));
                    }
                    let expected_registered = action == ExternalProtocolAction::Register;
                    let operation = match action {
                        ExternalProtocolAction::Register => {
                            service.enable_for_current_user().await
                        }
                        ExternalProtocolAction::Unregister => {
                            service.disable_for_current_user().await
                        }
                    };
                    match operation {
                        Ok(status)
                            if status.supported
                                && status.registered == expected_registered =>
                        {
                            Ok(ExternalProtocolChangeResult::Confirmed(status))
                        }
                        Ok(operation_status) => {
                            let readback = service.status().await;
                            Ok(ExternalProtocolChangeResult::Mismatch {
                                operation_status,
                                readback,
                            })
                        }
                        Err(error) => {
                            let readback = service.status().await;
                            Ok(ExternalProtocolChangeResult::Failed { error, readback })
                        }
                    }
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                if generation != panel.external_protocol_generation {
                    return;
                }
                panel.external_protocol_loading = false;
                panel.external_protocol_operation = None;
                match result {
                    Ok(ExternalProtocolChangeResult::Confirmed(status)) => {
                        panel.external_protocol_status = Some(status);
                        panel.external_protocol_last_result = Some(action);
                        panel.external_protocol_requires_refresh = false;
                        panel.external_protocol_outcome_unknown = false;
                        panel.external_protocol_error_is_operation = false;
                        panel.external_protocol_error = None;
                    }
                    Ok(ExternalProtocolChangeResult::Mismatch {
                        operation_status,
                        readback,
                    }) => {
                        let (status, readback_error) = match readback {
                            Ok(status) => (status, None),
                            Err(error) => (operation_status, Some(super::localized_error(&error))),
                        };
                        panel.external_protocol_status = Some(status);
                        panel.external_protocol_last_result = None;
                        panel.external_protocol_requires_refresh = false;
                        panel.external_protocol_outcome_unknown = false;
                        panel.external_protocol_error_is_operation = true;
                        panel.external_protocol_error = Some(readback_error.map_or_else(|| {
                            crate::locale::text(
                                "操作后的系统状态与预期不符，已读取当前状态。",
                                "The system state did not match the requested change; the current state has been read.",
                            )
                            .to_string()
                        }, |readback_error| {
                            format!("{} {}", crate::locale::text(
                                "操作后的系统回读与预期不符；再次读取失败，当前状态以操作回执为准。",
                                "The system state did not match the requested change. A second read failed; the operation receipt is shown.",
                            ), readback_error)
                        }));
                    }
                    Ok(ExternalProtocolChangeResult::Failed { error, readback }) => {
                        panel.external_protocol_outcome_unknown =
                            error.public_code() == PublicErrorCode::OutcomeUnknown;
                        panel.external_protocol_error_is_operation = true;
                        panel.external_protocol_last_result = None;
                        match readback {
                            Ok(status) => {
                                panel.external_protocol_status = Some(status);
                                panel.external_protocol_requires_refresh = false;
                                panel.external_protocol_error =
                                    Some(localized_external_protocol_error(&error));
                            }
                            Err(readback_error) => {
                                panel.external_protocol_status = None;
                                panel.external_protocol_requires_refresh = true;
                                panel.external_protocol_error = Some(format!(
                                    "{} {} {} {}",
                                    localized_external_protocol_error(&error),
                                    crate::locale::text(
                                        "重新读取状态也失败：",
                                        "Reading the status again also failed:",
                                    ),
                                    super::localized_error(&readback_error),
                                    crate::locale::text(
                                        "请关闭并重新打开设置以再次读取状态。",
                                        "Close and reopen Settings to read the status again.",
                                    ),
                                ));
                            }
                        }
                    }
                    Err(error) => {
                        panel.external_protocol_status = prior_status;
                        panel.external_protocol_error =
                            Some(localized_external_protocol_error(&error));
                        panel.external_protocol_error_is_operation = true;
                        panel.external_protocol_outcome_unknown = false;
                        panel.external_protocol_requires_refresh = false;
                        panel.external_protocol_last_result = None;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

/// 冲突和未知回执使用协议场景文案，其它错误继续遵循偏好页通用脱敏映射。
///
/// @author kongweiguang
fn localized_external_protocol_error(error: &AppError) -> String {
    match error.public_code() {
        PublicErrorCode::Conflict => crate::locale::text(
            "kerminal:// 已由其他程序处理；原有关联未被覆盖或删除。",
            "Another application owns kerminal://; its handler was left unchanged.",
        )
        .to_string(),
        PublicErrorCode::OutcomeUnknown => crate::locale::text(
            "没有收到操作确认，当前无法判断是否已完成。",
            "No operation confirmation was received, so completion cannot be determined yet.",
        )
        .to_string(),
        _ => super::localized_error(error),
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/preferences_external_protocol.rs"]
mod tests;
