//! 设置草稿编辑、校验、保存 CAS 与主题事件。
//!
//! @author kongweiguang

use std::time::Duration;

use super::forms::set_input;
use super::{
    PreferencesEvent, PreferencesPanel, SettingsTextField, ShortcutBindingField, keyboard_platform,
};
use crate::features::preferences::{
    ConfigDraft, DraftSnapshot, DraftStatus, DraftTransitionError, validate_settings,
};
use gpui_kit::{Context, PathPromptOptions, Window};
use kerminal_application::{
    AppError, PublicErrorCode,
    preferences::{SettingsSnapshot, VersionedSettings},
};
use kerminal_domain::preferences::ConfigurationMode;

impl PreferencesPanel {
    /// Browse 成功时一起启用背景并写入路径；取消不改值，配置提交仍由 application 负责。
    pub(in crate::features::preferences) fn choose_background_image(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if self.settings_controls_locked() {
            return;
        }
        let selected_paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(crate::locale::text(
                "选择背景图片",
                "Choose a background image",
            )),
        });
        cx.spawn(async move |this, cx| {
            let selection = selected_paths.await;
            let _ = this.update_in(cx, |panel, window, cx| match selection {
                Ok(Ok(Some(paths))) => {
                    let Some(path) = paths.into_iter().next() else {
                        return;
                    };
                    let Some(path) = path.to_str().map(str::to_owned) else {
                        panel.status = Some(crate::locale::text(
                            "所选路径无法保存为当前配置支持的文本路径。",
                            "The selected path cannot be represented by this configuration format.",
                        ).to_string());
                        cx.notify();
                        return;
                    };
                    set_input(
                        &panel.settings_inputs.background_path,
                        path.clone(),
                        window,
                        cx,
                    );
                    panel.edit_settings(
                        |settings| {
                            settings.background_image_path = path;
                            settings.background_enabled = true;
                        },
                        cx,
                    );
                }
                Ok(Ok(None)) => {}
                Ok(Err(_)) => {
                    panel.status = Some(crate::locale::text(
                        "系统文件选择器暂不可用，请检查桌面文件选择服务。",
                        "The system file picker is unavailable. Check the desktop file picker service.",
                    ).to_string());
                    cx.notify();
                }
                Err(_) => {
                    panel.status = Some(crate::locale::text(
                        "图片选择器未返回结果，请重试。",
                        "The image picker returned no result. Try again.",
                    ).to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 输入有效时先预览再防抖保存；数值中间态单独保留错误，不会提交旧值。
    pub(in crate::features::preferences) fn update_settings_text(
        &mut self,
        field: SettingsTextField,
        text: String,
        cx: &mut Context<Self>,
    ) {
        if self.settings_controls_locked() {
            return;
        }
        let Some(mut value) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return;
        };
        let (key, parse_error) = match field {
            SettingsTextField::FontFamily => {
                value.terminal.font_family = text;
                ("font_family", None)
            }
            SettingsTextField::FontSize => parse_update(&text, "font_size", |parsed| {
                value.terminal.font_size = parsed
            }),
            SettingsTextField::LineHeight => parse_update(&text, "line_height", |parsed| {
                value.terminal.line_height = parsed
            }),
            SettingsTextField::Scrollback => parse_update(&text, "scrollback", |parsed| {
                value.terminal.scrollback = parsed
            }),
            SettingsTextField::BackgroundPath => {
                value.background_image_path = text;
                ("background_path", None)
            }
            SettingsTextField::BackgroundOpacity => {
                parse_update(&text, "background_opacity", |parsed| {
                    value.background_opacity = parsed
                })
            }
            SettingsTextField::WindowOpacity => parse_update(&text, "window_opacity", |parsed| {
                value.window_opacity = parsed
            }),
            SettingsTextField::SftpGlobalTransfers => {
                parse_update(&text, "sftp_global_transfers", |parsed| {
                    value.sftp.global_transfers = parsed
                })
            }
            SettingsTextField::SftpHostTransfers => {
                parse_update(&text, "sftp_host_transfers", |parsed| {
                    value.sftp.host_transfers = parsed
                })
            }
            SettingsTextField::SftpPipelineDepth => {
                parse_update(&text, "sftp_pipeline_depth", |parsed| {
                    value.sftp.pipeline_depth = parsed
                })
            }
            SettingsTextField::SftpPacketBytes => parse_scaled_update(
                &text,
                "sftp_packet_bytes",
                1_048_576.,
                32_768,
                262_144,
                |parsed| value.sftp.packet_bytes = parsed,
            ),
            SettingsTextField::SftpIdleTimeout => {
                parse_update(&text, "sftp_idle_timeout", |parsed| {
                    value.sftp.idle_timeout_seconds = parsed
                })
            }
            SettingsTextField::NotificationMinDuration => parse_scaled_update(
                &text,
                "notification_min_duration",
                1_000.,
                1_000,
                120_000,
                |parsed| value.desktop_notifications.min_duration_ms = parsed,
            ),
            SettingsTextField::NotificationThrottle => parse_scaled_update(
                &text,
                "notification_throttle",
                1_000.,
                0,
                600_000,
                |parsed| value.desktop_notifications.throttle_ms = parsed,
            ),
        };
        if let Some(message) = parse_error {
            self.settings_input_errors.insert(key, message);
        } else {
            self.settings_input_errors.remove(key);
            if self.replace_settings_draft_value(value) {
                self.publish_settings_preview(cx);
            }
        }
        self.status = None;
        self.settings_success_status = None;
        self.refresh_settings_issues();
        cx.notify();
    }

    /// 统一修改 typed settings；保存中沿用冻结提交快照，冲突/未知结果仍由草稿拒绝。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn edit_settings(
        &mut self,
        edit: impl FnOnce(&mut SettingsSnapshot),
        cx: &mut Context<Self>,
    ) {
        if self.settings_controls_locked() {
            return;
        }
        let Some(mut value) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return;
        };
        edit(&mut value);
        if self.replace_settings_draft_value(value) {
            self.status = None;
            self.settings_success_status = None;
            self.refresh_settings_issues();
            self.publish_settings_preview(cx);
            cx.notify();
        }
    }

    /// 设置输入在保存中可以前进，但其它状态继续服从草稿的 revision 与读回门禁。
    fn replace_settings_draft_value(&mut self, value: SettingsSnapshot) -> bool {
        let Some(draft) = self.settings.as_mut() else {
            return false;
        };
        if draft.status() == DraftStatus::Saving {
            draft.replace_value_during_save(value).is_ok()
        } else {
            draft.replace_value(value).is_ok()
        }
    }

    /// 预览事件先更新全局运行态；短暂延后磁盘写入，合并连续输入为一次 CAS。
    fn publish_settings_preview(&mut self, cx: &mut Context<Self>) {
        let Some(settings) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return;
        };
        self.set_theme_mode(settings.theme_mode, cx);
        let generation = self.settings_autosave.edited();
        cx.emit(PreferencesEvent::SettingsPreview(settings));
        self.schedule_settings_autosave(generation, cx);
    }

    /// 旧计时器只等待 400ms 後做一次代次校验，不持有输入或存储状态。
    fn schedule_settings_autosave(&self, generation: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(400))
                .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.settings_autosave.is_current(generation) {
                    panel.save_settings(cx);
                }
            });
        })
        .detach();
    }

    /// 仅更新用户编辑的具体平台字段，避免把旧快照回退值写成显式覆盖。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn update_shortcut(
        &mut self,
        action: &str,
        field: ShortcutBindingField,
        binding: String,
        cx: &mut Context<Self>,
    ) {
        self.edit_settings(
            |settings| {
                if let Some(item) = settings
                    .keybindings
                    .iter_mut()
                    .find(|item| item.action == action)
                {
                    match field {
                        ShortcutBindingField::Windows => item.windows_binding = binding,
                        ShortcutBindingField::WindowsAlternate => {
                            item.windows_alternate_binding = Some(binding);
                        }
                        ShortcutBindingField::Linux => item.linux_binding = Some(binding),
                        ShortcutBindingField::Mac => item.mac_binding = binding,
                    }
                }
            },
            cx,
        );
    }

    /// 校验当前值后以单飞 CAS 保存；保存中到期的较新编辑排队使用回执 revision。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn save_settings(
        &mut self,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(settings_value) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return false;
        };
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            self.status = Some(
                crate::locale::text(
                    "配置处于只读恢复模式，暂不能保存。",
                    "Configuration is in read-only recovery mode and cannot be saved yet.",
                )
                .to_string(),
            );
            cx.notify();
            return false;
        }
        if !self.settings_input_errors.is_empty() {
            self.status = Some(
                crate::locale::text(
                    "请先修正输入框中的格式错误。",
                    "Fix the input format errors before saving.",
                )
                .to_string(),
            );
            cx.notify();
            return false;
        }
        let platform = keyboard_platform();
        self.settings_issues = validate_settings(&settings_value, platform);
        if self.settings_issues.iter().any(|issue| issue.blocks_save()) {
            self.status = Some(
                crate::locale::text(
                    "设置校验未通过，请检查提示项。",
                    "Settings validation failed. Review the messages below.",
                )
                .to_string(),
            );
            cx.notify();
            return false;
        }

        let generation = self.settings_autosave.current_generation();
        let ticket = match self.settings_autosave.request_save(generation) {
            super::autosave::SettingsSaveRequest::Started(ticket) => ticket,
            super::autosave::SettingsSaveRequest::Queued => return true,
            super::autosave::SettingsSaveRequest::Stale => return false,
        };
        let begin_result = self
            .settings
            .as_mut()
            .map(ConfigDraft::begin_save)
            .unwrap_or(Err(DraftTransitionError::SaveNotRunning));
        if let Err(error) = begin_result {
            self.settings_autosave.cancel_save(ticket);
            if error == DraftTransitionError::NoChanges {
                return false;
            }
            self.status = Some(match error {
                DraftTransitionError::ReloadRequired => crate::locale::text(
                    "配置已发生外部变更，请先重载。", "Configuration changed externally. Reload it before saving.",
                ).to_string(),
                DraftTransitionError::ReadBackRequired => crate::locale::text(
                    "上次写入结果未知，请先重新读取并核对配置。", "The previous write result is unknown. Read the configuration back and verify it first.",
                ).to_string(),
                _ => crate::locale::text("设置正在保存，请稍候。", "Settings are being saved. Please wait.").to_string(),
            });
            cx.notify();
            return false;
        }
        self.settings_success_status = None;
        let Some((settings, revision)) = self.settings.as_ref().map(|draft| {
            (
                draft
                    .pending_save_value()
                    .cloned()
                    .unwrap_or_else(|| draft.value().clone()),
                draft.revision().clone(),
            )
        }) else {
            self.settings_autosave.cancel_save(ticket);
            return false;
        };
        let save_generation = self.settings_load_generation;
        let service = self.preferences.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { service.save_settings(settings, revision).await })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.finish_settings_save(result, save_generation, ticket, window, cx)
            });
        })
        .detach();
        self.status = Some(crate::locale::text("正在保存设置…", "Saving settings…").to_string());
        cx.notify();
        true
    }

    /// 仅应用确认的快照能更新基线；布局保存发带 request 的窄收据，不能用旧全快照回滚 Root 新布局。
    ///
    /// @author kongweiguang
    fn finish_settings_save(
        &mut self,
        result: Result<VersionedSettings, AppError>,
        save_generation: u64,
        ticket: super::autosave::SettingsSaveTicket,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.settings_autosave.is_active(ticket) {
            return;
        }
        let tool_rail_save = std::mem::take(&mut self.pending_tool_rail_save);
        let (saved_successfully, allow_follow_up) = match &result {
            Ok(_) => (true, true),
            Err(error) => (
                false,
                !matches!(
                    error.public_code(),
                    PublicErrorCode::Conflict | PublicErrorCode::OutcomeUnknown
                ),
            ),
        };
        let mut verify_after_write = false;
        let reconcile_watcher_change = match result {
            Ok(saved) => {
                if let Some(draft) = self.settings.as_mut() {
                    let snapshot = DraftSnapshot {
                        value: saved.settings.clone(),
                        revision: saved.revision,
                    };
                    let _ = draft.save_succeeded(snapshot);
                }
                self.refresh_settings_issues();
                self.settings_error = None;
                self.status = None;
                let current_settings = self.settings.as_ref().map(|draft| draft.value().clone());
                let has_follow_up_edits = self.settings.as_ref().is_some_and(ConfigDraft::is_dirty);
                self.settings_success_status =
                    (!has_follow_up_edits).then_some(super::SettingsSuccessStatus::Saved);
                if let Some(settings) = current_settings.as_ref() {
                    self.set_theme_mode(settings.theme_mode, cx);
                }
                self.finish_agent_launcher_save_succeeded(
                    saved.settings.agent_launcher.clone(),
                    cx,
                );
                self.confirm_keyword_rule_saved(&saved.settings, window, cx);
                if !self.finish_workbench_layout_saved(saved.settings.workbench_layout.clone(), cx)
                {
                    cx.emit(PreferencesEvent::SettingsSaved(saved.settings.clone()));
                }
                if has_follow_up_edits && let Some(settings) = current_settings {
                    cx.emit(PreferencesEvent::SettingsPreview(settings));
                }
                save_generation != self.settings_load_generation
            }
            Err(error) => {
                verify_after_write = matches!(
                    error.public_code(),
                    PublicErrorCode::Conflict | PublicErrorCode::OutcomeUnknown
                );
                if let Some(draft) = self.settings.as_mut() {
                    if error.public_code() == PublicErrorCode::Conflict {
                        let _ = draft.save_conflicted();
                    } else if error.public_code() == PublicErrorCode::OutcomeUnknown {
                        let _ = draft.save_outcome_unknown();
                    } else {
                        let _ = draft.save_failed();
                    }
                }
                let message = super::localized_error(&error);
                if error.public_code() != PublicErrorCode::OutcomeUnknown {
                    self.fail_keyword_rule_save(cx);
                }
                self.finish_agent_launcher_save_failed(error.public_code(), message.clone(), cx);
                self.fail_workbench_layout_save(error.public_code(), message.clone(), cx);
                self.status = Some(message.clone());
                self.settings_success_status = None;
                if tool_rail_save {
                    cx.emit(PreferencesEvent::ToolRailSaveFailed { message });
                }
                save_generation != self.settings_load_generation
                    && error.public_code() != PublicErrorCode::OutcomeUnknown
            }
        };
        let has_dirty_settings = self.settings.as_ref().is_some_and(ConfigDraft::is_dirty);
        let queued_generation = self.settings_autosave.finish_save(
            ticket,
            allow_follow_up && has_dirty_settings && !reconcile_watcher_change,
        );
        if reconcile_watcher_change || verify_after_write {
            self.spawn_settings_revision_check(cx);
        }
        if saved_successfully && !reconcile_watcher_change {
            self.flush_pending_workbench_layout(cx);
        }
        if queued_generation.is_some() && self.settings.as_ref().is_some_and(ConfigDraft::is_dirty)
        {
            self.save_settings(cx);
        }
        cx.notify();
    }
}

/// 各数字字段统一处理编辑中的空值和格式错误，避免保留此前已解析的旧数值。
fn parse_update<T: std::str::FromStr>(
    text: &str,
    key: &'static str,
    update: impl FnOnce(T),
) -> (&'static str, Option<String>) {
    match text.trim().parse::<T>() {
        Ok(value) => {
            update(value);
            (key, None)
        }
        Err(_) => (
            key,
            Some(crate::locale::text("请输入有效的数值。", "Enter a valid number.").to_string()),
        ),
    }
}

/// 秒数或 MiB 字段先按显示单位转换并四舍五入到存储单位，避免输入时做整数截断。
fn parse_scaled_update(
    text: &str,
    key: &'static str,
    scale: f64,
    minimum: u32,
    maximum: u32,
    update: impl FnOnce(u32),
) -> (&'static str, Option<String>) {
    match parse_scaled_u32(text, scale).and_then(|value| {
        (minimum..=maximum)
            .contains(&value)
            .then_some(value)
            .ok_or(())
    }) {
        Ok(value) => {
            update(value);
            (key, None)
        }
        Err(()) => (
            key,
            Some(crate::locale::text("请输入有效的数值。", "Enter a valid number.").to_string()),
        ),
    }
}

/// 拒绝 NaN、无穷值、负数和转换溢出，保证缩放结果可精确存入配置的 u32 字段。
fn parse_scaled_u32(text: &str, scale: f64) -> Result<u32, ()> {
    let value = text.trim().parse::<f64>().map_err(|_| ())?;
    let scaled = value * scale;
    if !value.is_finite() || value < 0.0 || !scaled.is_finite() {
        return Err(());
    }
    let rounded = scaled.round();
    if rounded > f64::from(u32::MAX) {
        return Err(());
    }
    Ok(rounded as u32)
}

#[cfg(test)]
#[path = "../../../../tests/unit/preferences_settings_inputs.rs"]
mod tests;
