//! 配置文件恢复时的固定目录打开交互。
//!
//! @author kongweiguang

use gpui_kit::Context;
use kerminal_application::configuration_directory::ConfigurationDirectoryKind;

use super::{PreferencesPanel, RecoveryDirectoryStatus};

impl PreferencesPanel {
    /// 通过固定目录用例请求系统文件管理器打开配置根；界面不拼路径也不写入恢复文件。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn open_recovery_configuration_directory(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if matches!(
            self.recovery_directory_status.as_ref(),
            Some(&RecoveryDirectoryStatus::Requesting)
        ) {
            return;
        }
        let Some(directory) = self.configuration_directory.clone() else {
            self.recovery_directory_status = Some(RecoveryDirectoryStatus::Failed(
                crate::locale::text(
                    "配置目录打开服务尚未装配。",
                    "The configuration directory service is unavailable.",
                )
                .to_string(),
            ));
            cx.notify();
            return;
        };

        self.recovery_directory_status = Some(RecoveryDirectoryStatus::Requesting);
        let executor = self.executor.clone();
        let cancellation = self.configuration_change_cancellation.child_token();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |operation_cancellation| async move {
                    directory
                        .open(ConfigurationDirectoryKind::Root, operation_cancellation)
                        .await
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.recovery_directory_status = Some(match result {
                    Ok(_) => RecoveryDirectoryStatus::Accepted,
                    Err(error) => {
                        RecoveryDirectoryStatus::Failed(error.public_message().to_owned())
                    }
                });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
