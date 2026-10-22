//! 设置草稿编辑、校验、保存 CAS 与主题事件。
//!
//! @author kongweiguang

use super::forms::set_input;
use super::{
    PreferencesEvent, PreferencesPanel, SettingsTextField, ShortcutBindingField, keyboard_platform,
};
use crate::features::preferences::{
    ConfigDraft, DraftSnapshot, DraftStatus, DraftTransitionError, validate_settings,
};
use gpui_kit::{Context, PathPromptOptions, Window, component::input::InputState};
use kerminal_application::{
    AppError, PublicErrorCode,
    preferences::{SettingsSnapshot, VersionedSettings},
};
use kerminal_domain::preferences::ConfigurationMode;

impl PreferencesPanel {
    /// 使用 GPUI 原生文件对话框选图；I/O 与配置提交仍交给 application，不读取所选文件。
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
                    panel.update_settings_text(SettingsTextField::BackgroundPath, path, cx);
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

    /// 字段输入即时更新 typed 草稿；数值中间态单独保留错误，不能偷偷保存上一个数值。
    pub(in crate::features::preferences) fn update_settings_text(
        &mut self,
        field: SettingsTextField,
        text: String,
        cx: &mut Context<Self>,
    ) {
        if self.settings_controls_locked() {
            return;
        }
        let Some(settings) = self.settings.as_mut() else {
            return;
        };
        let mut value = settings.value().clone();
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
            SettingsTextField::SftpPacketBytes => {
                parse_update(&text, "sftp_packet_bytes", |parsed| {
                    value.sftp.packet_bytes = parsed
                })
            }
            SettingsTextField::SftpIdleTimeout => {
                parse_update(&text, "sftp_idle_timeout", |parsed| {
                    value.sftp.idle_timeout_seconds = parsed
                })
            }
            SettingsTextField::NotificationMinDuration => {
                parse_update(&text, "notification_min_duration", |parsed| {
                    value.desktop_notifications.min_duration_ms = parsed
                })
            }
            SettingsTextField::NotificationThrottle => {
                parse_update(&text, "notification_throttle", |parsed| {
                    value.desktop_notifications.throttle_ms = parsed
                })
            }
            SettingsTextField::SuggestionAuditRetention => {
                parse_update(&text, "suggestion_audit_retention", |parsed| {
                    value.terminal.inline_suggestion.audit_retention_days = parsed
                })
            }
            SettingsTextField::SuggestionFeedbackRetention => {
                parse_update(&text, "suggestion_feedback_retention", |parsed| {
                    value.terminal.inline_suggestion.feedback_retention_days = parsed
                })
            }
        };
        if let Some(message) = parse_error {
            self.settings_input_errors.insert(key, message);
        } else {
            self.settings_input_errors.remove(key);
            let _ = settings.replace_value(value);
        }
        self.status = None;
        self.settings_success_status = None;
        self.refresh_settings_issues();
        cx.notify();
    }

    /// 统一修改 typed settings，保存中或外部冲突时由草稿状态机拒绝而非覆盖 revision。
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
        if let Some(settings) = self.settings.as_mut() {
            let mut value = settings.value().clone();
            edit(&mut value);
            if settings.replace_value(value).is_ok() {
                self.status = None;
                self.settings_success_status = None;
                self.refresh_settings_issues();
                cx.notify();
            }
        }
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

    /// 在旧 Linux 继承态与显式绑定间切换；程序化清空不发输入 Change 事件，因此先同步草稿。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn toggle_linux_binding_inheritance(
        &mut self,
        action: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_controls_locked() {
            return;
        }
        let Some(binding) = self.settings.as_ref().and_then(|draft| {
            draft
                .value()
                .keybindings
                .iter()
                .find(|binding| binding.action == action)
        }) else {
            return;
        };

        if binding.linux_binding.is_none() {
            self.update_shortcut(action, ShortcutBindingField::Linux, String::new(), cx);
            return;
        }

        let Some(input) = self
            .shortcut_inputs
            .iter()
            .find(|inputs| inputs.action == action)
            .map(|inputs| inputs.linux.clone())
        else {
            return;
        };
        self.edit_settings(
            |settings| {
                if let Some(binding) = settings
                    .keybindings
                    .iter_mut()
                    .find(|binding| binding.action == action)
                {
                    binding.linux_binding = None;
                }
            },
            cx,
        );
        input.update(cx, |input: &mut InputState, cx| {
            input.set_value(String::new(), window, cx)
        });
    }

    /// 按当前平台校验并通过 application service 做 CAS 保存；返回值只表示异步 owner 已启动。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn save_settings(
        &mut self,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(draft) = self.settings.as_mut() else {
            return false;
        };
        self.settings_success_status = None;
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
        self.settings_issues = validate_settings(draft.value(), platform);
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
        if let Err(error) = draft.begin_save() {
            self.status = Some(match error {
                DraftTransitionError::NoChanges => crate::locale::text(
                    "没有待保存的设置。", "There are no settings changes to save.",
                ).to_string(),
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
        let settings = draft.value().clone();
        let revision = draft.revision().clone();
        let save_generation = self.settings_load_generation;
        let service = self.preferences.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { service.save_settings(settings, revision).await })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.finish_settings_save(result, save_generation, window, cx)
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
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tool_rail_save = std::mem::take(&mut self.pending_tool_rail_save);
        let saved_successfully = result.is_ok();
        let reconcile_watcher_change = match result {
            Ok(saved) => {
                if let Some(draft) = self.settings.as_mut() {
                    let snapshot = DraftSnapshot {
                        value: saved.settings.clone(),
                        revision: saved.revision,
                    };
                    let _ = draft.save_succeeded(snapshot);
                }
                self.settings_input_errors.clear();
                self.refresh_settings_issues();
                self.settings_error = None;
                self.status = None;
                self.settings_success_status = Some(super::SettingsSuccessStatus::Saved);
                self.set_theme_mode(saved.settings.theme_mode, cx);
                self.finish_agent_launcher_save_succeeded(
                    saved.settings.agent_launcher.clone(),
                    cx,
                );
                if !self.finish_workbench_layout_saved(saved.settings.workbench_layout.clone(), cx)
                {
                    cx.emit(PreferencesEvent::SettingsSaved(saved.settings));
                }
                save_generation != self.settings_load_generation
            }
            Err(error) => {
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
        if reconcile_watcher_change {
            self.spawn_settings_revision_check(cx);
        }
        if saved_successfully {
            self.flush_pending_workbench_layout(cx);
        }
        cx.notify();
    }

    /// 重载只对失败或冲突提供入口；存在本地草稿时先取得明确丢弃确认。
    pub(in crate::features::preferences) fn request_reload_settings(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if self.loading_settings
            || self
                .settings
                .as_ref()
                .is_some_and(|draft| draft.status() == DraftStatus::Saving)
        {
            return;
        }
        let outcome_unknown = self
            .settings
            .as_ref()
            .is_some_and(|draft| draft.status() == DraftStatus::OutcomeUnknown);
        if !outcome_unknown && self.settings.as_ref().is_some_and(ConfigDraft::is_dirty) {
            self.settings_reload_confirmation = true;
            cx.notify();
            return;
        }
        self.start_settings_reload(cx);
    }

    /// 用户确认后才丢弃本地旧快照并读取当前 revision。
    pub(in crate::features::preferences) fn confirm_reload_settings(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if !self.settings_reload_confirmation {
            return;
        }
        self.settings_reload_confirmation = false;
        self.start_settings_reload(cx);
    }

    /// 取消重载保留本地编辑值、错误提示和外部冲突状态。
    pub(in crate::features::preferences) fn cancel_reload_settings(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        self.settings_reload_confirmation = false;
        cx.notify();
    }

    /// 共用受 application executor 跟踪的 settings 读取过程，不在 UI 线程访问文件。
    ///
    /// @author kongweiguang
    fn start_settings_reload(&mut self, cx: &mut Context<Self>) {
        self.settings_success_status = None;
        self.status =
            Some(crate::locale::text("正在重新读取配置…", "Reloading configuration…").to_string());
        self.spawn_settings_load(cx);
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
