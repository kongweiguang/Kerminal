//! Shell 扫描结果与覆盖确认复用 Profile 写入门禁，避免绕过已有草稿和 revision。
//!
//! @author kongweiguang

use gpui_kit::{
    Anchor, Context, Window,
    component::{WindowExt as _, notification::Notification},
};
use kerminal_application::{
    AppResult, PublicErrorCode,
    preferences::{ProfileScanConflict, ProfileScanReport},
};
use kerminal_domain::preferences::ConfigurationMode;
use tokio_util::sync::CancellationToken;

use super::{PreferencesEvent, PreferencesPanel};
use crate::features::preferences::DraftStatus;

#[derive(Default)]
pub(in crate::features::preferences) struct ProfileScanState {
    cancellation: Option<CancellationToken>,
    pub(in crate::features::preferences) conflicts: Vec<ProfileScanConflict>,
    pub(in crate::features::preferences) requires_readback: bool,
}

impl PreferencesPanel {
    /// 只在草稿可安全刷新时允许扫描或覆盖，连尚未能解析的输入也不能被替换。
    /// @author kongweiguang
    pub(in crate::features::preferences) fn can_rescan_profiles(&self) -> bool {
        self.recovery.mode != ConfigurationMode::ReadOnlyRecovery
            && !self.profile_operation_running
            && !self.loading_profiles
            && !self.profile_scan.requires_readback
            && self.profile_input_errors.is_empty()
            && self.profile_draft.as_ref().is_none_or(|draft| {
                !draft.is_dirty()
                    && !matches!(
                        draft.status(),
                        DraftStatus::Saving
                            | DraftStatus::OutcomeUnknown
                            | DraftStatus::ExternalConflict
                    )
            })
    }

    /// 仅扫描按钮呈现忙碌；Profile 的其它写入共享操作门禁，不另建执行器。
    /// @author kongweiguang
    pub(in crate::features::preferences) fn rescan_profiles_running(&self) -> bool {
        self.profile_scan.cancellation.is_some()
    }

    /// 扫描与配置写入在共享执行器中完成；离开页面只请求取消，不丢弃已提交回执。
    /// @author kongweiguang
    pub(in crate::features::preferences) fn rescan_profiles(&mut self, cx: &mut Context<Self>) {
        if !self.can_rescan_profiles() {
            return;
        }
        let cancellation = CancellationToken::new();
        self.profile_scan.cancellation = Some(cancellation.clone());
        self.profile_scan.conflicts.clear();
        self.profile_operation_running = true;
        let service = self.profiles.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |cancel| async move {
                    service.rescan_system_profiles(cancel).await
                })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.finish_profile_scan(result, window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// 取消只阻止尚未开始的写入，已进入仓储的操作仍必须读到真实结果。
    /// @author kongweiguang
    pub(super) fn cancel_profile_scan(&self) {
        if let Some(cancellation) = &self.profile_scan.cancellation {
            cancellation.cancel();
        }
    }

    /// 部分成功也刷新目录；结果未知需要显式读回，禁止继续写入或自动重试。
    /// @author kongweiguang
    fn finish_profile_scan(
        &mut self,
        result: AppResult<ProfileScanReport>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cancelled = self
            .profile_scan
            .cancellation
            .take()
            .is_some_and(|token| token.is_cancelled());
        self.profile_operation_running = false;
        let (added, skipped, conflicts, stopped) = match result {
            Ok(report) => {
                let (added, skipped) = (report.added(), report.skipped());
                let (conflicts, stopped) = report.into_review();
                (added, skipped, conflicts, stopped)
            }
            Err(error) => (0, 0, Vec::new(), Some(error)),
        };
        let code = stopped
            .as_ref()
            .map(kerminal_application::AppError::public_code);
        if !cancelled
            && !matches!(
                code,
                Some(PublicErrorCode::OutcomeUnknown | PublicErrorCode::Cancelled)
            )
        {
            self.profile_scan.conflicts = conflicts;
        }
        if added > 0 {
            cx.emit(PreferencesEvent::ProfilesChanged);
        }
        if code == Some(PublicErrorCode::OutcomeUnknown) {
            self.profile_scan.requires_readback = true;
            let message = crate::locale::text(
                format!("已新增 {added} 项；有一项写入结果未知，请重新读取 Profile 列表核对。"),
                format!(
                    "{added} added. One write result is unknown. Reload profiles to verify it."
                ),
            );
            self.status = Some(message.to_string());
            notify_profile_scan(Notification::warning(message), window, cx);
        } else {
            if added > 0 || code == Some(PublicErrorCode::Conflict) {
                self.load_profiles_again(cx);
            }
            if cancelled || code == Some(PublicErrorCode::Cancelled) {
                if added > 0 {
                    notify_profile_scan(
                        Notification::info(crate::locale::text(
                            format!("扫描已取消，已新增的 {added} 项已保留。"),
                            format!(
                                "Scan cancelled. The {added} profiles already added were kept."
                            ),
                        )),
                        window,
                        cx,
                    );
                }
            } else if let Some(error) = stopped {
                let reason = super::localized_error(&error);
                notify_profile_scan(
                    Notification::error(crate::locale::text(
                        format!("扫描中止，已新增 {added} 项：{reason}"),
                        format!("Scan stopped after adding {added}: {reason}"),
                    )),
                    window,
                    cx,
                );
            } else {
                let conflicts = self.profile_scan.conflicts.len();
                let summary = if added == 0 && conflicts == 0 && skipped == 0 {
                    crate::locale::text(
                        "没有新的 Shell 或需要更新的 Profile。",
                        "No new shells or profile updates found.",
                    )
                } else {
                    crate::locale::text(
                        format!("新增 {added} 项，待确认 {conflicts} 项，跳过 {skipped} 项。"),
                        format!("{added} added, {conflicts} to review, {skipped} skipped."),
                    )
                };
                notify_profile_scan(Notification::info(summary), window, cx);
            }
        }
        cx.notify();
    }

    /// 用户确认后安装原版本和新启动字段，再复用原保存流程处理冲突、失败与未知结果。
    /// @author kongweiguang
    pub(in crate::features::preferences) fn overwrite_scanned_profile(
        &mut self,
        conflict: ProfileScanConflict,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_rescan_profiles()
            || !self.profile_scan.conflicts.iter().any(|pending| {
                pending.current().profile.id == conflict.current().profile.id
                    && pending.current().revision == conflict.current().revision
                    && pending.suggested() == conflict.suggested()
            })
        {
            return;
        }
        self.profile_scan
            .conflicts
            .retain(|pending| pending.current().profile.id != conflict.current().profile.id);
        self.install_profile(conflict.current().clone(), window, cx);
        let suggested = conflict.suggested();
        if self
            .profile_draft
            .as_mut()
            .is_some_and(|draft| draft.replace_value(suggested.clone()).is_ok())
        {
            self.sync_profile_inputs(suggested, window, cx);
            self.save_profile(cx);
        }
    }
}

/// 扫描结果复用同一通知位置，避免叠放后遮住设置窗右上角的关闭按钮。
/// @author kongweiguang
fn notify_profile_scan(
    notification: Notification,
    window: &mut Window,
    cx: &mut Context<PreferencesPanel>,
) {
    window.push_notification(
        notification
            .id::<ProfileScanState>()
            .placement(Anchor::BottomRight),
        cx,
    );
}
