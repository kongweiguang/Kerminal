//! 设置草稿编辑、校验、保存 CAS 与主题事件。
//!
//! @author kongweiguang

use std::time::Duration;

use super::forms::set_input;
use super::{
    PreferencesEvent, PreferencesPanel, SettingsTextField, ShortcutBindingField,
    ShortcutBindingUpdate, keyboard_platform,
};
use crate::features::preferences::{
    ConfigDraft, DraftSnapshot, DraftStatus, DraftTransitionError, validate_settings, validation,
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

    /// 录制与恢复默认共用一次候选提交：先按目标平台编译，再整体替换草稿。
    ///
    /// 只有 Linux 接受 `Inherit`；Windows 和 macOS 始终要求显式值，
    /// 避免把空字符串误当成禁用写回。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn commit_shortcut_binding(
        &mut self,
        action: &str,
        field: ShortcutBindingField,
        update: ShortcutBindingUpdate,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.settings_controls_locked() {
            return Err(crate::locale::text(
                "当前设置处于只读状态，暂时不能修改快捷键。",
                "Shortcuts are read-only until the settings are editable again.",
            )
            .to_string());
        }
        let Some(mut candidate) = self.settings.as_ref().map(|draft| draft.value().clone()) else {
            return Err(crate::locale::text(
                "设置尚未载入，请稍后重试。",
                "Settings are not loaded yet. Try again shortly.",
            )
            .to_string());
        };
        let Some(item) = candidate
            .keybindings
            .iter_mut()
            .find(|item| item.action == action)
        else {
            return Err(crate::locale::text(
                "快捷键动作已不存在，请重新载入设置。",
                "The shortcut action no longer exists. Reload the settings.",
            )
            .to_string());
        };
        set_shortcut_binding(item, field, update.clone());
        let effective = validation::binding_for_platform(item, field.editor_platform());
        if let Some(message) =
            shortcut_binding_error(&candidate.keybindings, action, field, &effective)
        {
            return Err(message);
        }
        self.shortcut_field_errors
            .remove(&shortcut_error_key(action, field));
        let committed = update.clone();
        self.edit_settings(
            |settings| {
                if let Some(item) = settings
                    .keybindings
                    .iter_mut()
                    .find(|item| item.action == action)
                {
                    set_shortcut_binding(item, field, update);
                }
            },
            cx,
        );
        if self.settings.as_ref().is_none_or(|draft| {
            !draft.value().keybindings.iter().any(|item| {
                item.action == action && shortcut_binding_matches(item, field, &committed)
            })
        }) {
            return Err(crate::locale::text(
                "快捷键未能写入当前草稿，请重新载入设置后重试。",
                "The shortcut could not be written to the current draft. Reload the settings and try again.",
            )
            .to_string());
        }
        Ok(())
    }

    /// 只恢复所选平台的默认值；Linux 的缺省绑定清除覆盖并重新跟随 Windows 回退。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn reset_shortcut(
        &mut self,
        action: &str,
        platform: crate::features::preferences::KeyboardPlatform,
        cx: &mut Context<Self>,
    ) {
        let Some(default) = SettingsSnapshot::default()
            .keybindings
            .into_iter()
            .find(|binding| binding.action == action)
        else {
            return;
        };
        let field = ShortcutBindingField::from_editor_platform(platform);
        let update = match field {
            ShortcutBindingField::Windows => ShortcutBindingUpdate::Set(
                validation::effective_binding(&default.windows_binding, &default.binding),
            ),
            ShortcutBindingField::Mac => ShortcutBindingUpdate::Set(validation::effective_binding(
                &default.mac_binding,
                &default.binding,
            )),
            ShortcutBindingField::Linux => default
                .linux_binding
                .map_or(ShortcutBindingUpdate::Inherit, ShortcutBindingUpdate::Set),
        };
        if let Err(message) = self.commit_shortcut_binding(action, field, update, cx) {
            self.shortcut_field_errors
                .insert(shortcut_error_key(action, field), message);
            cx.notify();
        }
    }

    /// 来源保存复用设置草稿的 CAS 与权威回读，不另建配置写入通道。
    ///
    /// URL 先用默认快照单独校验；URL 已在基线时只做权威回读，不因其它字段的临时
    /// 输入错误或脏值重写整份设置。URL 未落盘时才替换草稿 URL 并走整份 CAS 保存，
    /// 因此其它字段的校验仍会约束这次写入；写入未启动时撤回未提交的 URL 字段，
    /// 避免之后被无关保存带走。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn request_catalog_source_save(
        &mut self,
        url: String,
        cx: &mut Context<Self>,
    ) {
        if self.settings_controls_locked() {
            self.extensions.fail_catalog_source_save(
                crate::locale::text(
                    "当前设置处于只读或保存冲突状态，暂时不能修改商店来源。",
                    "The store source cannot be changed while settings are read-only or conflicted.",
                )
                .to_string(),
                cx,
            );
            return;
        }
        if self.settings.is_none() {
            self.extensions.fail_catalog_source_save(
                crate::locale::text(
                    "设置尚未载入，请稍后重试。",
                    "Settings are not loaded yet. Try again shortly.",
                )
                .to_string(),
                cx,
            );
            return;
        }
        // 默认快照只承载 URL：校验与规范化规则仍由 application 统一提供，
        // 不在这里复制一份 URL 语法，也不让其它字段的编辑中错误参与判断。
        let probe = SettingsSnapshot {
            plugin_catalog_url: url,
            ..SettingsSnapshot::default()
        };
        let normalized = match probe.validated() {
            Ok(value) => value.plugin_catalog_url,
            Err(error) => {
                self.extensions
                    .fail_catalog_source_save(super::localized_error(&error), cx);
                return;
            }
        };
        // URL 已在基线（上次成功保存或加载确认）时，来源意图与其它草稿字段解耦：
        // 不重写无关设置，直接真实回读确认；同时撤回草稿里可能残留的未提交 URL。
        let url_confirmed = self.settings.as_ref().is_some_and(|draft| {
            draft.baseline_value().plugin_catalog_url == normalized
                && matches!(
                    draft.status(),
                    DraftStatus::Ready | DraftStatus::Failed | DraftStatus::Saving
                )
        });
        if url_confirmed {
            let _ = self.withdraw_unsubmitted_catalog_source_field();
            self.extensions.begin_catalog_source_save(normalized);
            self.spawn_catalog_source_readback(cx);
            return;
        }
        let candidate = {
            let draft = self.settings.as_ref().expect("draft checked above");
            let mut candidate = draft.value().clone();
            candidate.plugin_catalog_url = normalized.clone();
            candidate
        };
        if !self.replace_settings_draft_value(candidate) {
            self.extensions.fail_catalog_source_save(
                crate::locale::text(
                    "设置草稿状态已变化，请重新读取后再保存。",
                    "The settings draft changed. Reload it before saving.",
                )
                .to_string(),
                cx,
            );
            return;
        }
        self.extensions.begin_catalog_source_save(normalized);
        self.settings_success_status = None;
        // 必须推进编辑代次：若已有设置在途，单飞协调器只会为“更新的代次”排队；
        // 复用旧代次会让本次 URL 永远等不到后续保存，最终被误判成来源被覆盖。
        self.settings_autosave.edited();
        if !self.save_settings(cx) {
            // 保存没有启动时提交值只存在于草稿里；撤回未提交的 URL 字段，防止失败
            // 地址被之后无关的 autosave 静默写入。若 URL 已由基线或在途快照确认，
            // 保留来源意图，由权威回读或该在途回执继续确认。
            if !self.withdraw_unsubmitted_catalog_source_field() {
                let message = self
                    .status
                    .clone()
                    .filter(|message| !message.is_empty())
                    .unwrap_or_else(|| {
                        crate::locale::text(
                            "商店来源未能启动保存。",
                            "The store source save could not be started.",
                        )
                        .to_string()
                    });
                self.extensions.fail_catalog_source_save(message, cx);
            } else if self.settings.as_ref().is_some_and(|draft| {
                matches!(draft.status(), DraftStatus::Ready | DraftStatus::Failed)
            }) {
                self.spawn_catalog_source_readback(cx);
            }
        }
    }

    /// 撤回未提交的来源 URL 字段；返回 true 表示待确认 URL 已由基线或在途快照确认。
    ///
    /// 判断对象是来源待确认地址而不是可编辑副本：其它字段或 URL 输入框的后续编辑
    /// 不能改变“本次来源是否已落盘”。Saving 时以不可变提交快照判断，快照已含该
    /// URL 说明在途保存可能落盘；否则把可编辑副本恢复到快照 URL，保留其它编辑与
    /// 在途快照。Ready/Failed 以基线判断；Conflict/OutcomeUnknown 不触碰，交给
    /// 权威恢复流程。这样已知未提交的失败地址不会被之后无关的 autosave 静默写入，
    /// 也不会误伤可能已落盘的来源意图。
    ///
    /// @author kongweiguang
    fn withdraw_unsubmitted_catalog_source_field(&mut self) -> bool {
        let pending_url = self.extensions.catalog_source_pending().map(str::to_owned);
        let Some(draft) = self.settings.as_mut() else {
            return false;
        };
        match draft.status() {
            DraftStatus::Saving => {
                let Some(snapshot) = draft.pending_save_value().cloned() else {
                    return false;
                };
                let confirmed = pending_url
                    .as_deref()
                    .is_some_and(|url| snapshot.plugin_catalog_url == url);
                if draft.value().plugin_catalog_url != snapshot.plugin_catalog_url {
                    let mut value = draft.value().clone();
                    value.plugin_catalog_url = snapshot.plugin_catalog_url;
                    let _ = draft.replace_value_during_save(value);
                }
                confirmed
            }
            DraftStatus::Failed | DraftStatus::Ready => {
                let baseline_url = draft.baseline_value().plugin_catalog_url.clone();
                let confirmed = pending_url
                    .as_deref()
                    .is_some_and(|url| baseline_url == url);
                if draft.value().plugin_catalog_url != baseline_url {
                    let mut value = draft.value().clone();
                    value.plugin_catalog_url = baseline_url;
                    // 走既有 replace_value 迁移：Failed→Ready 由状态机负责，不另造草稿语义。
                    let _ = draft.replace_value(value);
                }
                confirmed
            }
            DraftStatus::ExternalConflict | DraftStatus::OutcomeUnknown => true,
        }
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

    /// 已知失败先撤回关键词投影再锁定冲突；未知结果保留提交快照等待回读。
    /// 仅应用确认的快照能更新基线，布局收据不能回滚 Root 的较新布局。
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
                if error.public_code() != PublicErrorCode::OutcomeUnknown {
                    self.fail_keyword_rule_save(cx);
                }
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
                // 已知失败时来源提交值没有落盘：撤回未提交的 URL 字段，避免之后
                // 无关 autosave 静默写入失败地址。URL 已由成功回执进入基线时保留
                // 来源意图，由后续权威回读确认；未知/冲突不回退，交由既有读回
                // 恢复流程确认磁盘状态；保存成功但回读失败也不进入这里。
                if error.public_code() != PublicErrorCode::Conflict
                    && error.public_code() != PublicErrorCode::OutcomeUnknown
                    && self.extensions.catalog_source_pending().is_some()
                    && !self.withdraw_unsubmitted_catalog_source_field()
                {
                    self.extensions
                        .fail_catalog_source_save(message.clone(), cx);
                }
                if tool_rail_save {
                    cx.emit(PreferencesEvent::ToolRailSaveFailed { message });
                }
                save_generation != self.settings_load_generation
                    && error.public_code() != PublicErrorCode::OutcomeUnknown
            }
        };
        let has_dirty_settings = self.settings.as_ref().is_some_and(ConfigDraft::is_dirty);
        let source_pending = self.extensions.catalog_source_pending().is_some();
        // 来源提交失败时不能借自动排队保存重试：那会在用户看不到失败的情况下
        // 再次写入并把来源切走，等同于盲目重写。失败原因交给来源弹层，由用户
        // 显式重试；父草稿的其它脏值仍保留，下一次编辑或保存会继续提交。
        let source_save_failed = source_pending && !saved_successfully;
        let queued_generation = self.settings_autosave.finish_save(
            ticket,
            allow_follow_up
                && has_dirty_settings
                && !reconcile_watcher_change
                && !source_save_failed,
        );
        if reconcile_watcher_change || verify_after_write {
            self.spawn_settings_revision_check(cx);
        }
        if saved_successfully && !reconcile_watcher_change {
            self.flush_pending_workbench_layout(cx);
        }
        let follow_up_expected = queued_generation.is_some() && has_dirty_settings;
        if follow_up_expected && !self.save_settings(cx) {
            // 排队保存若因校验或草稿门禁无法启动，来源请求不能永远停在“保存中”；
            // 此时没有后续权威回执。与冲突合并续写共用同一收口，避免两处判断漂移。
            self.finish_unstarted_catalog_source_save(cx);
        }
        // 来源必须经过真实 load_settings 回读才能切换：保存回执只证明提交值，
        // 不证明磁盘内容。单飞保存的中间回执由 follow_up_expected 继续等待；
        // 最终回执到达后无论其 URL 是否与提交一致，都以回读结果为准。
        if self.extensions.catalog_source_pending().is_some() && !follow_up_expected {
            if saved_successfully && !reconcile_watcher_change {
                self.spawn_catalog_source_readback(cx);
            } else if verify_after_write && !reconcile_watcher_change {
                // 冲突/未知：保留来源意图，由权威恢复读回确认或显式失败；
                // 在这里清 pending 会取消恢复代次，导致写入已确认却无法切源。
            } else if !reconcile_watcher_change && self.catalog_source_url_is_baseline_confirmed() {
                // 来源 URL 已由成功回执写入基线，本次失败只涉及其它字段；
                // 仍必须真实回读确认后才切换，不能把整份设置保存失败当成来源失败。
                self.spawn_catalog_source_readback(cx);
            } else {
                let message = if reconcile_watcher_change {
                    crate::locale::text(
                        "设置已被外部修改，来源未切换，请重试。",
                        "Settings changed externally, so the source was not switched. Try again.",
                    )
                    .to_string()
                } else {
                    self.source_save_failure_message()
                };
                self.extensions.fail_catalog_source_save(message, cx);
            }
        }
        cx.notify();
    }

    /// 保存未启动时收口来源意图：未确认则撤回并显式失败，基线/在途快照已确认则真实回读。
    ///
    /// 冲突合并续写与 follow-up 排队共用同一判断；只处理来源，不改其它草稿字段，
    /// 未知/冲突交给既有恢复流程，避免在这里盲目重写。调用方必须已经确认保存
    /// 没有启动（`save_settings` 返回 false），否则会把在途保存误判成未启动。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn finish_unstarted_catalog_source_save(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if self.extensions.catalog_source_pending().is_none() {
            return;
        }
        if !self.withdraw_unsubmitted_catalog_source_field() {
            let message = self.source_save_failure_message();
            self.extensions.fail_catalog_source_save(message, cx);
        } else if self
            .settings
            .as_ref()
            .is_some_and(|draft| matches!(draft.status(), DraftStatus::Ready | DraftStatus::Failed))
        {
            self.spawn_catalog_source_readback(cx);
        }
    }

    /// 判断待确认来源是否已进入草稿基线：成功回执或加载确认只证明 URL 已落盘，
    /// 切换运行源仍以真实回读为准。
    ///
    /// @author kongweiguang
    fn catalog_source_url_is_baseline_confirmed(&self) -> bool {
        let Some(pending) = self.extensions.catalog_source_pending() else {
            return false;
        };
        self.settings
            .as_ref()
            .is_some_and(|draft| draft.baseline_value().plugin_catalog_url == pending)
    }

    /// 保存成功后异步读取权威设置，核对来源地址后才允许切换运行态。
    ///
    /// 回读与保存使用同一共享执行器，不另建运行时；代次与待确认地址在回读
    /// 完成时复核，迟到的旧结果不会覆盖新提交。
    ///
    /// @author kongweiguang
    fn spawn_catalog_source_readback(&mut self, cx: &mut Context<Self>) {
        let Some((url, generation)) = self.extensions.catalog_source_readback_token() else {
            return;
        };
        let service = self.preferences.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { service.load_settings().await })
                .await;
            let _ = this.update_in(cx, |panel, _, cx| {
                panel
                    .extensions
                    .finish_catalog_source_readback(&url, generation, result, cx);
            });
        })
        .detach();
    }

    /// 来源保存失败时优先复用设置保存的已本地化原因；没有原因时给出可重试提示。
    ///
    /// @author kongweiguang
    fn source_save_failure_message(&self) -> String {
        self.status
            .clone()
            .filter(|message| !message.is_empty())
            .unwrap_or_else(|| {
                crate::locale::text(
                    "商店来源未能确认保存，请重试。",
                    "The store source save could not be confirmed. Try again.",
                )
                .to_string()
            })
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

/// 平台字段必须按原格式更新，避免普通修改覆盖其他平台或丢失显式空值。
///
/// @author kongweiguang
fn set_shortcut_binding(
    preference: &mut kerminal_application::preferences::KeybindingPreference,
    field: ShortcutBindingField,
    update: ShortcutBindingUpdate,
) {
    match (field, update) {
        (ShortcutBindingField::Windows, ShortcutBindingUpdate::Set(binding)) => {
            preference.windows_binding = binding;
        }
        (ShortcutBindingField::Mac, ShortcutBindingUpdate::Set(binding)) => {
            preference.mac_binding = binding;
        }
        (ShortcutBindingField::Linux, update) => {
            preference.linux_binding = match update {
                ShortcutBindingUpdate::Set(binding) => Some(binding),
                ShortcutBindingUpdate::Inherit => None,
            };
        }
        // Windows 与 macOS 没有可清除的覆盖字段，忽略无意义上写请求。
        (_, ShortcutBindingUpdate::Inherit) => {}
    }
}

/// 提交流程必须能读回同一平台的写入意图，避免草稿状态拒绝替换后仍显示成功。
///
/// @author kongweiguang
fn shortcut_binding_matches(
    preference: &kerminal_application::preferences::KeybindingPreference,
    field: ShortcutBindingField,
    update: &ShortcutBindingUpdate,
) -> bool {
    match (field, update) {
        (ShortcutBindingField::Windows, ShortcutBindingUpdate::Set(binding)) => {
            preference.windows_binding == *binding
        }
        (ShortcutBindingField::Mac, ShortcutBindingUpdate::Set(binding)) => {
            preference.mac_binding == *binding
        }
        (ShortcutBindingField::Linux, ShortcutBindingUpdate::Set(binding)) => {
            preference.linux_binding.as_deref() == Some(binding.as_str())
        }
        (ShortcutBindingField::Linux, ShortcutBindingUpdate::Inherit) => {
            preference.linux_binding.is_none()
        }
        (_, ShortcutBindingUpdate::Inherit) => false,
    }
}

/// 目标平台的有效绑定必须能编译且不与其他动作冲突；录制与恢复默认共用。
///
/// @author kongweiguang
fn shortcut_binding_error(
    keybindings: &[kerminal_application::preferences::KeybindingPreference],
    action: &str,
    field: ShortcutBindingField,
    effective: &str,
) -> Option<String> {
    if let Some(message) = super::shortcut_capture::shortcut_sequence_issue(effective) {
        return Some(message);
    }
    let compiled = crate::keybindings::CompiledKeybindings::compile(keybindings, field.platform());
    compiled
        .diagnostics()
        .iter()
        .find(|diagnostic| {
            diagnostic.kind != crate::keybindings::KeybindingDiagnosticKind::UnknownAction
                && (diagnostic.action == action
                    || diagnostic.conflicts_with.as_deref() == Some(action))
        })
        .map(|diagnostic| diagnostic.message.clone())
}

/// action 与平台字段共同组成错误身份，避免不同平台的就地提示互相覆盖。
///
/// @author kongweiguang
fn shortcut_error_key(action: &str, field: ShortcutBindingField) -> String {
    let field = match field {
        ShortcutBindingField::Windows => "windows",
        ShortcutBindingField::Linux => "linux",
        ShortcutBindingField::Mac => "macos",
    };
    format!("{action}:{field}")
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
