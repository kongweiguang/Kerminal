//! 设置初始化期间读取 Vault readiness；同步密钥编辑由独立的 connectivity 状态管理。
//!
//! @author kongweiguang

use gpui_kit::Context;
use kerminal_application::{PublicErrorCode, connections::credentials::VaultKeyAvailability};

use super::PreferencesPanel;

impl PreferencesPanel {
    /// 初始化只刷新 typed readiness，不创建或修改密钥，避免设置面板打开产生写副作用。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn refresh_vault_availability(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if self.vault_loading || self.vault_busy {
            return;
        }
        let Some(service) = self.vault_keys.clone() else {
            self.vault_error = Some(
                crate::locale::text(
                    "Vault 密钥服务未由应用装配。",
                    "The Vault key service is unavailable.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };

        self.vault_loading = true;
        self.vault_error = None;
        let executor = self.executor.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(|_| async move { service.availability().await })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.vault_loading = false;
                match result {
                    Ok(availability) => {
                        let ready = matches!(
                            availability,
                            VaultKeyAvailability::Ready { .. }
                                | VaultKeyAvailability::Created { .. }
                        );
                        panel.vault_availability = Some(availability);
                        panel.vault_error = None;
                        if ready {
                            panel.vault_outcome_unknown = false;
                            panel.vault_status = None;
                        }
                    }
                    Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                        panel.vault_outcome_unknown = true;
                        panel.vault_availability = None;
                        panel.vault_status = None;
                        panel.vault_error = Some(super::localized_error(&error));
                    }
                    Err(error) => panel.vault_error = Some(super::localized_error(&error)),
                }
                cx.notify();
            });
        })
        .detach();
    }
}
