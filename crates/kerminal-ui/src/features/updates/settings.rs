//! 设置弹框内关于页所需的更新状态投影与受限动作入口。
//!
//! @author kongweiguang

use gpui_kit::Context;

use super::{
    SettingsInstallStep, UpdatePanelState, UpdatesPanel, about::APP_VERSION, about::LICENSE_SPDX,
    can_check_for_updates, settings_install_step,
};
use kerminal_application::updates::UpdateAvailability;

/// About 页只消费可见文案和可用动作，不复制更新事务或下载句柄。
#[derive(Clone, Debug)]
pub(crate) struct AboutUpdateState {
    pub(crate) status: String,
    pub(crate) detail: String,
    pub(crate) can_check: bool,
    pub(crate) checking: bool,
    pub(crate) available: bool,
    pub(crate) can_install: bool,
    pub(crate) restarting: bool,
    pub(crate) failed: bool,
    pub(crate) up_to_date: bool,
}

/// 关于页仅请求更新面板已经支持的事务；重启由现有宿主安装流程负责。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AboutUpdateAction {
    Check,
    Install,
}

impl UpdatesPanel {
    /// 把事务状态压缩成关于页需要的显示字段，保留原更新面板作为唯一状态所有者。
    pub(crate) fn settings_projection(&self) -> AboutUpdateState {
        let mut projection = AboutUpdateState {
            status: localized("可检查", "Ready to check"),
            detail: localized("手动检查更新。", "Check for updates manually."),
            can_check: false,
            checking: false,
            available: false,
            can_install: false,
            restarting: false,
            failed: false,
            up_to_date: false,
        };
        let updater_configured = self
            .service
            .as_ref()
            .is_some_and(|service| service.availability() == UpdateAvailability::Configured);

        match &self.state {
            UpdatePanelState::Idle => {
                if let Some(status) = &self.status {
                    projection.status =
                        idle_update_status_label(updater_configured, self.status_is_error);
                    projection.detail = status.clone();
                    projection.failed = self.status_is_error;
                }
            }
            UpdatePanelState::Checking => {
                projection.status = localized("检查中", "Checking");
                projection.detail = self.status.clone().unwrap_or_else(|| {
                    localized("正在检查 GitHub Releases。", "Checking GitHub Releases.")
                });
                projection.checking = true;
            }
            UpdatePanelState::NoUpdate => {
                projection.up_to_date = true;
                projection.status = localized("已是最新", "Up to date");
                projection.detail = self.status.clone().unwrap_or_else(|| {
                    localized("已是最新版本。", "You are using the latest version.")
                });
            }
            UpdatePanelState::Available(candidate) => {
                projection.available = true;
                projection.status = localized("可更新", "Update available");
                projection.detail = self.status.clone().unwrap_or_else(|| {
                    format!(
                        "{} {}。",
                        localized("发现版本", "Version available:"),
                        version_label(&candidate.version)
                    )
                });
                projection.can_install = true;
            }
            UpdatePanelState::Downloading(candidate) => {
                projection.available = true;
                projection.status = localized("下载中", "Downloading");
                projection.detail = download_detail(self.progress, &candidate.version);
            }
            UpdatePanelState::ReadyToInstall(update) => {
                projection.available = true;
                projection.status = localized("可安装", "Ready to install");
                projection.detail = self.status.clone().unwrap_or_else(|| {
                    format!(
                        "{} {}。",
                        localized("已下载并验证版本", "Verified version"),
                        version_label(&update.version)
                    )
                });
                projection.can_install = true;
            }
            UpdatePanelState::AwaitingInstallGuard(update) => {
                projection.status = localized("准备安装", "Preparing install");
                projection.detail = self
                    .status
                    .clone()
                    .unwrap_or_else(|| update.version.clone());
            }
            UpdatePanelState::Installing(update) => {
                projection.status = localized("安装中", "Installing");
                projection.detail = self
                    .status
                    .clone()
                    .unwrap_or_else(|| update.version.clone());
            }
            UpdatePanelState::Discarding(update) => {
                projection.status = localized("清理中", "Discarding");
                projection.detail = self
                    .status
                    .clone()
                    .unwrap_or_else(|| update.version.clone());
            }
            UpdatePanelState::RestartPending(update) => {
                projection.status = localized("等待重启", "Restart pending");
                projection.detail = self.status.clone().unwrap_or_else(|| {
                    format!(
                        "{} {}。",
                        localized("版本已安装", "Installed version"),
                        version_label(&update.version)
                    )
                });
                projection.restarting = true;
            }
            UpdatePanelState::RestartFailed(update) => {
                projection.status = localized("重启失败", "Restart failed");
                projection.detail = self.status.clone().unwrap_or_else(|| {
                    format!(
                        "{} {}。",
                        localized(
                            "版本已安装，请手动重新打开应用",
                            "Version installed; reopen the app manually:"
                        ),
                        version_label(&update.version)
                    )
                });
                projection.failed = true;
            }
            UpdatePanelState::HostRecoveryFailed { safe_message, .. } => {
                projection.status = localized("需要人工恢复", "Manual recovery required");
                projection.detail = safe_message.clone();
                projection.failed = true;
            }
            UpdatePanelState::StoppedUpdateFailed { update, .. } => {
                projection.status = localized("安装失败", "Install failed");
                projection.detail = self.status.clone().unwrap_or_else(|| {
                    format!(
                        "{} {}。",
                        localized("版本", "Version"),
                        version_label(&update.version)
                    )
                });
                projection.failed = true;
            }
        }

        projection.can_check =
            settings_check_allowed(&self.state, updater_configured, self.is_busy());
        projection
    }

    /// About 保持单击安装；候选先由同一下载服务验签，再交给 Root 安装保护。
    pub(crate) fn settings_action(&mut self, action: AboutUpdateAction, cx: &mut Context<Self>) {
        match action {
            AboutUpdateAction::Check => self.check_for_updates(cx),
            AboutUpdateAction::Install => match settings_install_step(&self.state) {
                SettingsInstallStep::DownloadThenInstall => {
                    let updater_configured = self.service.as_ref().is_some_and(|service| {
                        service.availability() == UpdateAvailability::Configured
                    });
                    if updater_configured {
                        self.settings_auto_install = true;
                        self.download_update(cx);
                    }
                }
                SettingsInstallStep::InstallVerified => self.request_install(cx),
                SettingsInstallStep::Ignore => {}
            },
        }
    }
}

/// 版本号统一补 v 前缀，避免候选与当前 Cargo 版本呈现格式不同。
fn version_label(version: &str) -> String {
    if version.starts_with('v') {
        version.to_owned()
    } else {
        format!("v{version}")
    }
}

/// 下载详情只读取当前最新进度快照，不引入第二个轮询或后台任务。
fn download_detail(
    progress: kerminal_application::updates::UpdateProgress,
    version: &str,
) -> String {
    let progress_text = match progress.total_bytes.filter(|total| *total > 0) {
        Some(total) => format!(
            "{}%",
            progress
                .received_bytes
                .saturating_mul(100)
                .checked_div(total)
                .unwrap_or(0)
                .min(100)
        ),
        None => format!("{} B", progress.received_bytes),
    };
    format!(
        "{} {} · {progress_text}",
        localized("正在下载并验证", "Downloading and verifying"),
        version_label(version)
    )
}

/// 文案在调用时按当前界面语言生成，切换语言后不保留旧语言缓存。
fn localized(zh: &str, en: &str) -> String {
    crate::locale::text(zh, en).to_string()
}

/// 已配置更新源时区分检查失败与可重试状态，不能把网络错误显示成能力不可用。
pub(super) fn idle_update_status_label(updater_configured: bool, failed: bool) -> String {
    match (updater_configured, failed) {
        (false, _) => localized("不可用", "Unavailable"),
        (true, true) => localized("失败", "Failed"),
        (true, false) => localized("可检查", "Ready to check"),
    }
}

/// 仅供 About 行显示真实运行版本与 SPDX 标识，不把截图中的版本固化为配置。
pub(crate) fn about_identity() -> (String, String) {
    (version_label(APP_VERSION), LICENSE_SPDX.to_owned())
}

/// About 页沿用更新面板校验过的 HTTPS 仓库地址，不信任任意 package metadata URI。
pub(crate) fn about_repository_url() -> Option<&'static str> {
    super::about::repository_url()
}

/// 检查按钮只在不会丢弃已验签包或覆盖终态的空闲状态出现。
pub(super) fn settings_check_allowed(
    state: &UpdatePanelState,
    updater_configured: bool,
    busy: bool,
) -> bool {
    updater_configured && !busy && can_check_for_updates(state)
}
