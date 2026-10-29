//! 自定义 Agent 管理器经偏好面板的唯一 Settings CAS owner 保存和核对。
//!
//! @author kongweiguang

use gpui_kit::Context;
use kerminal_application::{
    PublicErrorCode,
    preferences::{AgentLauncherSettings, SettingsSnapshot, VersionedSettings},
};
use kerminal_domain::preferences::{ConfigRevision, ConfigurationMode};

use crate::features::preferences::{DraftStatus, validate_settings};

use super::{PreferencesEvent, PreferencesPanel, keyboard_platform, localized_error};

/// 管理器版本链只由自身确认的 CAS/读回推进；恢复读回不能擅自重置共享偏好草稿。
#[derive(Default)]
pub(super) struct AgentLauncherManagerState {
    confirmed_baseline: Option<VersionedSettings>,
    pending_save: Option<AgentLauncherPendingSave>,
    recovery: Option<AgentLauncherRecovery>,
    readback_generation: u64,
    readback_active: bool,
    readback_context: Option<AgentLauncherReadBackContext>,
}

impl AgentLauncherManagerState {
    /// Root 保持打开时的原始基线；仅管理器自己确认的写入或精确读回能推进当前版本。
    ///
    /// @author kongweiguang
    fn baseline_for<'a>(&'a self, opening: &'a VersionedSettings) -> &'a VersionedSettings {
        self.confirmed_baseline.as_ref().unwrap_or(opening)
    }

    /// 记录与管理器已确认结果同源的完整 Settings/revision，供下一次 CAS 延续版本链。
    ///
    /// @author kongweiguang
    fn confirm_baseline(&mut self, versioned: VersionedSettings) {
        self.confirmed_baseline = Some(versioned);
    }

    /// 只有提交值与 CAS 后已安装的干净快照一致时，才用其真实 revision 推进管理器版本链。
    ///
    /// @author kongweiguang
    fn confirm_save_baseline(
        &mut self,
        pending: &AgentLauncherPendingSave,
        submitted: &AgentLauncherSettings,
        installed: Option<VersionedSettings>,
    ) {
        if pending.submitted.agent_launcher != *submitted {
            return;
        }
        if let Some(installed) =
            installed.filter(|installed| installed.settings.agent_launcher == *submitted)
        {
            self.confirm_baseline(installed);
        }
    }

    /// 管理器关闭后丢弃局部版本链，下次打开必须重新捕获 Root 提供的当前基线。
    ///
    /// @author kongweiguang
    fn release_confirmed_baseline(&mut self) {
        self.confirmed_baseline = None;
    }
}

/// 一次正在写入的管理器请求，完整快照用于识别是否混入了其它偏好改动。
#[derive(Clone)]
struct AgentLauncherPendingSave {
    baseline: SettingsSnapshot,
    baseline_revision: Option<ConfigRevision>,
    submitted: SettingsSnapshot,
}

/// 提交失败或同步拒绝后的核对状态；没有精确读回前不允许管理器再次提交。
#[derive(Clone)]
struct AgentLauncherRecovery {
    baseline: SettingsSnapshot,
    baseline_revision: Option<ConfigRevision>,
    submitted: SettingsSnapshot,
    submitted_write: bool,
    failure_code: PublicErrorCode,
    readback: Option<AgentLauncherReadBackReceipt>,
}

/// 读回启动时的完整草稿、revision 和状态；异步完成时据此识别并行编辑。
#[derive(Clone)]
struct AgentLauncherReadBackContext {
    settings: SettingsSnapshot,
    revision: Option<ConfigRevision>,
    status: DraftStatus,
    dirty: bool,
    recovery: AgentLauncherRecovery,
}

/// 回读快照和 revision 同源；重试必须与当前干净草稿逐字值和版本一致。
#[derive(Clone)]
struct AgentLauncherReadBackReceipt {
    versioned: VersionedSettings,
    retry_allowed: bool,
}

impl PreferencesPanel {
    /// 固定打开时的完整快照与版本，防止 watcher 更新主偏好后让独立管理器草稿悄悄重定位。
    ///
    /// @author kongweiguang
    pub(crate) fn agent_launcher_settings_for_manager(&self) -> Result<VersionedSettings, String> {
        self.ensure_agent_launcher_manager_ready()?;
        if self.agent_launcher_manager.pending_save.is_some()
            || self.agent_launcher_manager.readback_active
        {
            return Err(manager_message(
                "自定义 Agent 配置正在处理中，请等待当前操作完成。",
                "The custom Agent settings are busy. Wait for the current operation to finish.",
            ));
        }
        self.settings
            .as_ref()
            .map(|draft| VersionedSettings {
                settings: draft.value().clone(),
                revision: draft.revision().clone(),
            })
            .ok_or_else(|| {
                manager_message("偏好设置尚未读取完成。", "Settings are not loaded yet.")
            })
    }

    /// 首次提交匹配 Root 打开版本，后续只沿管理器确认的版本继续；外部 revision 仍进入读回门禁。
    ///
    /// @author kongweiguang
    pub(crate) fn apply_agent_launcher_change(
        &mut self,
        baseline: &VersionedSettings,
        settings: AgentLauncherSettings,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.ensure_agent_launcher_manager_ready()?;
        if self.agent_launcher_manager.pending_save.is_some()
            || self.agent_launcher_manager.readback_active
        {
            return Err(manager_message(
                "自定义 Agent 配置正在处理中，请等待当前操作完成。",
                "The custom Agent settings are busy. Wait for the current operation to finish.",
            ));
        }

        let Some(original) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return Err(manager_message(
                "偏好设置尚未读取完成。",
                "Settings are not loaded yet.",
            ));
        };
        let recovery = self.agent_launcher_manager.recovery.clone();
        let Some(recovery) = recovery else {
            let current = VersionedSettings {
                settings: original.clone(),
                revision: self
                    .settings
                    .as_ref()
                    .and_then(|draft| draft.revision().clone()),
            };
            if let Some(conflict) = stale_agent_launcher_change(
                self.agent_launcher_manager.baseline_for(baseline),
                &current,
                &settings,
            ) {
                self.agent_launcher_manager.recovery = Some(conflict);
                cx.emit(PreferencesEvent::AgentLauncherSaveFailed {
                    code: PublicErrorCode::Conflict,
                    message: manager_message(
                        "配置已在外部更新，本地 Agent 草稿已保留。请先读回核对。",
                        "Settings changed externally. The local Agent draft is preserved. Read settings back before continuing.",
                    ),
                });
                cx.notify();
                return Ok(());
            }
            return self.submit_agent_launcher_change(original, settings, cx);
        };
        let Some(receipt) = recovery.readback.as_ref() else {
            let (zh_cn, en_us) = match recovery.failure_code {
                PublicErrorCode::Conflict => (
                    "保存遇到并发冲突，请先读回并核对最新配置。",
                    "The save conflicted. Read back and review the latest settings first.",
                ),
                PublicErrorCode::OutcomeUnknown => (
                    "上次保存结果未知，必须先读回核对，暂不能重试。",
                    "The previous save outcome is unknown. Read it back before retrying.",
                ),
                _ => (
                    "上次保存结果尚未读回核对，暂不能重试。",
                    "The previous save has not been read back yet. Retrying is blocked.",
                ),
            };
            return Err(manager_message(zh_cn, en_us));
        };
        if !receipt.retry_allowed
            || receipt.versioned.settings != original
            || self.settings.as_ref().is_none_or(|draft| {
                draft.revision() != &receipt.versioned.revision
                    || draft.status() != DraftStatus::Ready
                    || draft.is_dirty()
            })
        {
            return Err(manager_message(
                "读回状态与当前偏好草稿不一致，请重新读取后再操作。",
                "The read-back state no longer matches Preferences. Reload before continuing.",
            ));
        }
        self.agent_launcher_manager.recovery = None;
        self.submit_agent_launcher_change(original, settings, cx)
    }

    /// 未读回确认前沿用打开或最近一次自有确认版本；共享草稿另行保留以识别并行编辑。
    ///
    /// @author kongweiguang
    pub(crate) fn read_back_agent_launcher_for_manager(
        &mut self,
        baseline: &VersionedSettings,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.agent_launcher_manager.pending_save.is_some()
            || self.agent_launcher_manager.readback_active
            || self.loading_settings
            || self
                .settings
                .as_ref()
                .is_some_and(|draft| draft.status() == DraftStatus::Saving)
        {
            return Err(manager_message(
                "偏好设置正在读取或写入，请稍候再核对。",
                "Preferences are loading or saving. Wait before reading back.",
            ));
        }
        let Some(draft) = self.settings.as_ref() else {
            return Err(manager_message(
                "偏好设置尚未读取完成，暂不能核对自定义 Agent。",
                "Preferences are not loaded yet. The custom Agent settings cannot be reconciled.",
            ));
        };
        let context = AgentLauncherReadBackContext {
            settings: draft.value().clone(),
            revision: draft.revision().clone(),
            status: draft.status(),
            dirty: draft.is_dirty(),
            recovery: agent_launcher_readback_recovery(
                self.agent_launcher_manager.baseline_for(baseline),
                self.agent_launcher_manager.recovery.clone(),
            ),
        };

        let generation = self
            .agent_launcher_manager
            .readback_generation
            .wrapping_add(1)
            .max(1);
        self.agent_launcher_manager.readback_generation = generation;
        self.agent_launcher_manager.readback_active = true;
        self.agent_launcher_manager.readback_context = Some(context);
        if let Some(recovery) = self.agent_launcher_manager.recovery.as_mut() {
            recovery.readback = None;
        }
        let service = self.preferences.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { service.load_settings().await })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.finish_agent_launcher_readback(result, generation, window, cx);
            });
        })
        .detach();
        cx.notify();
        Ok(())
    }

    /// 关闭时释放局部确认版本；未知提交仍保留原恢复门禁，未完成请求期间不提前清理。
    ///
    /// @author kongweiguang
    pub(crate) fn release_agent_launcher_manager_after_close(&mut self) {
        if self.agent_launcher_manager.pending_save.is_some()
            || self.agent_launcher_manager.readback_active
        {
            return;
        }
        self.agent_launcher_manager.release_confirmed_baseline();
        if self.ensure_agent_launcher_manager_ready().is_ok() {
            self.agent_launcher_manager.recovery = None;
        }
    }

    /// 把验证后的候选安装进现有草稿后复用普通保存 owner；即时失败时恢复原快照。
    ///
    /// @author kongweiguang
    fn submit_agent_launcher_change(
        &mut self,
        original: SettingsSnapshot,
        requested: AgentLauncherSettings,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let mut candidate = original.clone();
        candidate.agent_launcher = requested;
        let normalized = candidate
            .clone()
            .validated()
            .map_err(|error| localized_error(&error))?;
        candidate.agent_launcher = normalized.agent_launcher;

        let issues = validate_settings(&candidate, keyboard_platform());
        if let Some(issue) = issues.iter().find(|issue| issue.blocks_save()) {
            return Err(issue.message.clone());
        }
        if !self.settings_input_errors.is_empty() {
            return Err(manager_message(
                "请先修正偏好页中的输入格式错误。",
                "Fix the Preferences input errors before saving.",
            ));
        }

        let Some(baseline_revision) = self.settings.as_ref().map(|draft| draft.revision().clone())
        else {
            return Err(manager_message(
                "偏好设置尚未读取完成。",
                "Settings are not loaded yet.",
            ));
        };

        if candidate.agent_launcher == original.agent_launcher {
            self.agent_launcher_manager.recovery = None;
            cx.emit(PreferencesEvent::AgentLauncherSaved(
                original.agent_launcher.clone(),
            ));
            cx.notify();
            return Ok(());
        }

        let replaced = self
            .settings
            .as_mut()
            .is_some_and(|draft| draft.replace_value(candidate.clone()).is_ok());
        if !replaced {
            return Err(manager_message(
                "偏好草稿状态已变化，请重新读取后再保存。",
                "The Preferences draft changed. Reload it before saving.",
            ));
        }
        self.agent_launcher_manager.pending_save = Some(AgentLauncherPendingSave {
            baseline: original.clone(),
            baseline_revision,
            submitted: candidate,
        });
        self.agent_launcher_manager.recovery = None;
        self.settings_success_status = None;
        if self.save_settings(cx) {
            return Ok(());
        }

        self.agent_launcher_manager.pending_save = None;
        if let Some(draft) = self.settings.as_mut() {
            let _ = draft.replace_value(original);
        }
        let message = self.status.clone().unwrap_or_else(|| {
            manager_message(
                "自定义 Agent 设置未能启动保存。",
                "The custom Agent save could not be started.",
            )
        });
        self.refresh_settings_issues();
        cx.notify();
        Err(message)
    }

    /// 根据应用最终回执区分 CAS 冲突、结果未知和普通失败，并保留该次请求供后续读回。
    ///
    /// @author kongweiguang
    pub(super) fn finish_agent_launcher_save_failed(
        &mut self,
        code: PublicErrorCode,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(pending) = self.agent_launcher_manager.pending_save.take() {
            self.agent_launcher_manager.recovery = Some(AgentLauncherRecovery {
                baseline: pending.baseline,
                baseline_revision: pending.baseline_revision,
                submitted: pending.submitted,
                submitted_write: true,
                failure_code: code,
                readback: None,
            });
            cx.emit(PreferencesEvent::AgentLauncherSaveFailed { code, message });
        }
    }

    /// 仅专属 CAS 回执与已安装的 clean Settings/revision 一致时推进版本链，普通 SettingsSaved 不算收据。
    ///
    /// @author kongweiguang
    pub(super) fn finish_agent_launcher_save_succeeded(
        &mut self,
        settings: AgentLauncherSettings,
        cx: &mut Context<Self>,
    ) {
        if let Some(pending) = self.agent_launcher_manager.pending_save.take() {
            let installed = self
                .settings
                .as_ref()
                .filter(|draft| {
                    draft.status() == DraftStatus::Ready
                        && !draft.is_dirty()
                        && draft.value().agent_launcher == settings
                })
                .map(|draft| VersionedSettings {
                    settings: draft.value().clone(),
                    revision: draft.revision().clone(),
                });
            self.agent_launcher_manager
                .confirm_save_baseline(&pending, &settings, installed);
            self.agent_launcher_manager.recovery = None;
            cx.emit(PreferencesEvent::AgentLauncherSaved(settings));
        }
    }

    /// 真实读回只在精确确认管理器提交时推进版本链；安装共享草稿仍要求无并行编辑。
    ///
    /// @author kongweiguang
    fn finish_agent_launcher_readback(
        &mut self,
        result: Result<VersionedSettings, kerminal_application::AppError>,
        generation: u64,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) {
        if generation != self.agent_launcher_manager.readback_generation
            || !self.agent_launcher_manager.readback_active
        {
            return;
        }
        self.agent_launcher_manager.readback_active = false;
        let Some(context) = self.agent_launcher_manager.readback_context.take() else {
            return;
        };
        let mut recovery = context.recovery.clone();
        match result {
            Ok(versioned) => {
                let current_unchanged = self.settings.as_ref().is_some_and(|draft| {
                    draft.value() == &context.settings
                        && draft.revision() == &context.revision
                        && draft.status() == context.status
                        && draft.is_dirty() == context.dirty
                });
                let no_pending_write = self.agent_launcher_manager.pending_save.is_none()
                    && !self.pending_tool_rail_save
                    && !self.loading_settings;
                let clean_ready_unchanged = no_pending_write
                    && current_unchanged
                    && context.status == DraftStatus::Ready
                    && !context.dirty
                    && self.agent_launcher_preferences_ready_for_write().is_ok();
                // 只在当前脏草稿与本次提交完全相同且仅 manager 改过 agent_launcher 时，
                // 才把读回快照安装为新基线；这样可清除管理器自己造成的 CAS 状态而不覆盖旁支编辑。
                let manager_only_draft = no_pending_write
                    && recovery.submitted_write
                    && current_unchanged
                    && self.settings.as_ref().is_some_and(|draft| {
                        let mut expected_submission = recovery.baseline.clone();
                        expected_submission.agent_launcher =
                            recovery.submitted.agent_launcher.clone();
                        draft.value() == &recovery.submitted
                            && draft.revision() == &recovery.baseline_revision
                            && draft.is_dirty()
                            && matches!(
                                draft.status(),
                                DraftStatus::Failed
                                    | DraftStatus::ExternalConflict
                                    | DraftStatus::OutcomeUnknown
                            )
                            && expected_submission == recovery.submitted
                            && self.settings_input_errors.is_empty()
                    });
                let may_install_readback = clean_ready_unchanged || manager_only_draft;
                if may_install_readback {
                    self.install_settings(versioned.clone(), window, cx);
                    self.settings_error = None;
                    self.status = None;
                    self.settings_success_status = None;
                }
                let launcher_settings = versioned.settings.agent_launcher.clone();
                let confirmed_submission =
                    agent_launcher_readback_confirms_write(&recovery, &launcher_settings);
                if confirmed_submission && no_pending_write {
                    self.agent_launcher_manager
                        .confirm_baseline(versioned.clone());
                }
                let retry_allowed = !confirmed_submission
                    && agent_launcher_readback_preserves_baseline(&recovery, &launcher_settings)
                    && may_install_readback
                    && no_pending_write
                    && self.agent_launcher_preferences_ready_for_write().is_ok()
                    && self.settings.as_ref().is_some_and(|draft| {
                        readback_matches_clean_preferences(
                            draft.value(),
                            draft.revision(),
                            draft.status(),
                            draft.is_dirty(),
                            &versioned,
                        )
                    });
                if confirmed_submission {
                    // 当前 Agent launcher 已与本次提交一致，未知/冲突状态已由读回最终核实。
                    self.agent_launcher_manager.recovery = None;
                } else {
                    recovery.readback = Some(AgentLauncherReadBackReceipt {
                        versioned,
                        retry_allowed,
                    });
                    self.agent_launcher_manager.recovery = Some(recovery);
                }
                cx.emit(PreferencesEvent::AgentLauncherReadBack {
                    settings: launcher_settings,
                    retry_allowed,
                });
            }
            Err(error) => {
                recovery.readback = None;
                self.agent_launcher_manager.recovery = Some(recovery);
                cx.emit(PreferencesEvent::AgentLauncherReadBackFailed {
                    message: localized_error(&error),
                });
            }
        }
        cx.notify();
    }

    /// 复用主偏好草稿的写门禁；这里不自动清理输入错误、dirty 值或冲突状态。
    ///
    /// @author kongweiguang
    fn ensure_agent_launcher_manager_ready(&self) -> Result<(), String> {
        self.agent_launcher_preferences_ready_for_write()?;
        if self
            .settings
            .as_ref()
            .is_some_and(|draft| draft.status() != DraftStatus::Ready || draft.is_dirty())
        {
            return Err(manager_message(
                "其它偏好存在未保存改动或冲突，请先处理后再打开 Agent 管理器。",
                "Preferences has unsaved changes or a conflict. Resolve it before opening Agent management.",
            ));
        }
        Ok(())
    }

    /// 共享“可写”条件供管理器入口和读回重试判断，完整字段校验仍由普通保存 owner 执行。
    ///
    /// @author kongweiguang
    fn agent_launcher_preferences_ready_for_write(&self) -> Result<(), String> {
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            return Err(manager_message(
                "配置处于只读恢复状态，暂不能修改自定义 Agent。",
                "Configuration is in read-only recovery mode. Custom Agents cannot be changed yet.",
            ));
        }
        if self.loading_settings {
            return Err(manager_message(
                "偏好设置仍在读取，请稍候。",
                "Preferences is still loading. Wait a moment.",
            ));
        }
        if self.settings.is_none() {
            return Err(manager_message(
                "偏好设置尚未读取完成。",
                "Preferences has not finished loading.",
            ));
        }
        if !self.settings_input_errors.is_empty() {
            return Err(manager_message(
                "请先修正偏好页中的输入格式错误。",
                "Fix the Preferences input errors before saving.",
            ));
        }
        if let Some(issue) = self
            .settings
            .as_ref()
            .map(|draft| validate_settings(draft.value(), keyboard_platform()))
            .and_then(|issues| issues.into_iter().find(|issue| issue.blocks_save()))
        {
            return Err(issue.message);
        }
        self.settings
            .as_ref()
            .ok_or_else(|| {
                manager_message(
                    "偏好设置尚未读取完成。",
                    "Preferences has not finished loading.",
                )
            })?
            .value()
            .clone()
            .validated()
            .map(|_| ())
            .map_err(|error| localized_error(&error))
    }
}

/// 同步拒绝可能早于保存恢复记录；此时仍固定打开版本，避免首次读回借用 watcher 的新 Agent 基线。
///
/// @author kongweiguang
fn agent_launcher_readback_recovery(
    baseline: &VersionedSettings,
    recovery: Option<AgentLauncherRecovery>,
) -> AgentLauncherRecovery {
    recovery.unwrap_or_else(|| AgentLauncherRecovery {
        baseline: baseline.settings.clone(),
        baseline_revision: baseline.revision.clone(),
        submitted: baseline.settings.clone(),
        submitted_write: false,
        failure_code: PublicErrorCode::Invalid,
        readback: None,
    })
}

/// 即使 watcher 已安装新版本，旧管理器也不能借用新 revision 覆盖磁盘；同步拒绝必须标记为未写入。
///
/// @author kongweiguang
fn stale_agent_launcher_change(
    baseline: &VersionedSettings,
    current: &VersionedSettings,
    requested: &AgentLauncherSettings,
) -> Option<AgentLauncherRecovery> {
    if baseline.settings == current.settings && baseline.revision == current.revision {
        return None;
    }
    let mut submitted = baseline.settings.clone();
    submitted.agent_launcher = requested.clone();
    Some(AgentLauncherRecovery {
        baseline: baseline.settings.clone(),
        baseline_revision: baseline.revision.clone(),
        submitted,
        submitted_write: false,
        failure_code: PublicErrorCode::Conflict,
        readback: None,
    })
}

/// 读回只允许在 Agent 基线未被外部改动时重试；外部目录变更须核对并重开管理器，不能一键覆盖整份列表。
///
/// @author kongweiguang
fn agent_launcher_readback_preserves_baseline(
    recovery: &AgentLauncherRecovery,
    readback: &AgentLauncherSettings,
) -> bool {
    recovery.baseline.agent_launcher == *readback
}

/// 只有确实发起过持久化写入时，读回相同 Agent 值才可确认该次提交。
///
/// @author kongweiguang
fn agent_launcher_readback_confirms_write(
    recovery: &AgentLauncherRecovery,
    readback: &AgentLauncherSettings,
) -> bool {
    recovery.submitted_write && recovery.submitted.agent_launcher == *readback
}

/// 重试收据必须与 Preferences 当前 clean Ready 全量快照和 revision 同时一致。
///
/// @author kongweiguang
fn readback_matches_clean_preferences(
    settings: &SettingsSnapshot,
    revision: &Option<ConfigRevision>,
    status: DraftStatus,
    dirty: bool,
    versioned: &VersionedSettings,
) -> bool {
    status == DraftStatus::Ready
        && !dirty
        && settings == &versioned.settings
        && revision == &versioned.revision
}

#[cfg(test)]
#[path = "../../../../tests/unit/preferences_agent_launcher_recovery.rs"]
mod tests;

/// 面向调用方返回本地化且不含文件路径的门禁原因。
///
/// @author kongweiguang
fn manager_message(zh_cn: &str, en_us: &str) -> String {
    crate::locale::text(zh_cn, en_us).to_string()
}
