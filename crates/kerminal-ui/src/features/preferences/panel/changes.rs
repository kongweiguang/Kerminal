//! 配置 watcher 与偏好草稿的并发边界。
//!
//! @author kongweiguang

use gpui_kit::Context;
use kerminal_application::configuration_changes::ConfigurationChangeMessage;
use kerminal_application::preferences::{SettingsSnapshot, VersionedProfile, VersionedSettings};
use kerminal_domain::preferences::{ConfigRevision, ConfigurationMode};

use crate::features::preferences::{ConfigDraft, DraftSnapshot, DraftStatus};

use super::PreferencesPanel;

/// watcher 快照的窄决策结果；只有明确安全的分支才能替换现有草稿。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsRevisionAction {
    Install,
    Preserve,
    SelfWriteEcho,
    ExternalConflict,
    ReadOnlyChanged,
}

/// 只对结果未知或 CAS 冲突重试只读快照读取，不让回读失败变成重复写入授权。
///
/// @author kongweiguang
pub(super) fn needs_settings_recovery_readback(status: DraftStatus) -> bool {
    matches!(
        status,
        DraftStatus::OutcomeUnknown | DraftStatus::ExternalConflict
    )
}

impl PreferencesPanel {
    /// 将 watcher 快照归类为保留、安装或冲突，避免后台读回替换受保护草稿。
    /// 暴露给同一面板状态机测试，避免回归重复实现 revision 分类条件。
    ///
    /// @author kongweiguang
    pub(super) fn settings_revision_action(
        status: Option<DraftStatus>,
        dirty: bool,
        read_only: bool,
        current_revision: Option<&ConfigRevision>,
        observed_revision: Option<&ConfigRevision>,
    ) -> SettingsRevisionAction {
        let Some(status) = status else {
            return SettingsRevisionAction::Install;
        };
        match status {
            DraftStatus::Saving | DraftStatus::OutcomeUnknown => SettingsRevisionAction::Preserve,
            DraftStatus::ExternalConflict | DraftStatus::Ready | DraftStatus::Failed => {
                if current_revision == observed_revision {
                    return if dirty {
                        SettingsRevisionAction::SelfWriteEcho
                    } else {
                        SettingsRevisionAction::Preserve
                    };
                }
                if dirty && current_revision != observed_revision {
                    SettingsRevisionAction::ExternalConflict
                } else if read_only {
                    SettingsRevisionAction::ReadOnlyChanged
                } else {
                    SettingsRevisionAction::Install
                }
            }
        }
    }

    /// 每个面板建立独立配置订阅；干净表单按 typed 用例重载，dirty 草稿保留并锁住 CAS。
    ///
    /// @author kongweiguang
    pub(super) fn watch_configuration_changes(&mut self, cx: &mut Context<Self>) {
        let Some(service) = self.configuration_changes.clone() else {
            return;
        };
        let mut subscription = service.subscribe();
        let cancellation = self.configuration_change_cancellation.clone();
        let preferences = self.preferences.clone();
        let profiles = self.profiles.clone();
        let executor = self.executor.clone();

        cx.spawn(async move |this, cx| {
            loop {
                let (settings_generation, profiles_generation) =
                    match subscription.recv(cancellation.clone()).await {
                        ConfigurationChangeMessage::Changed(event) => {
                            let settings_changed = event.full_rescan_required
                                || paths_change_settings(&event.relative_paths);
                            let profiles_changed = event.full_rescan_required
                                || paths_change_profiles(&event.relative_paths);
                            let groups_changed = event.full_rescan_required
                                || paths_change_host_groups(&event.relative_paths);
                            let recovery = event.recovery_state;
                            this.update(cx, |panel, cx| {
                                panel.recovery = recovery;
                                if groups_changed {
                                    panel.reload_profile_groups(cx);
                                }
                                let settings_generation =
                                    settings_changed.then(|| panel.begin_settings_revision_check());
                                let profiles_generation = profiles_changed
                                    .then(|| panel.prepare_external_profile_change())
                                    .flatten();
                                cx.notify();
                                (settings_generation, profiles_generation)
                            })
                            .unwrap_or((None, None))
                        }
                        ConfigurationChangeMessage::RescanRequired => {
                            let recovery = service.recovery_state();
                            this.update(cx, |panel, cx| {
                                panel.recovery = recovery;
                                panel.reload_profile_groups(cx);
                                let settings_generation =
                                    Some(panel.begin_settings_revision_check());
                                let profiles_generation = panel.prepare_external_profile_change();
                                cx.notify();
                                (settings_generation, profiles_generation)
                            })
                            .unwrap_or((None, None))
                        }
                        ConfigurationChangeMessage::Cancelled
                        | ConfigurationChangeMessage::Closed => break,
                    };

                if let Some(generation) = settings_generation {
                    let service = preferences.clone();
                    let result = executor
                        .clone()
                        .run(move |_| async move { service.load_settings().await })
                        .await;
                    let _ = this.update_in(cx, |panel, window, cx| {
                        panel.finish_settings_revision_check(result, generation, window, cx)
                    });
                }
                if let Some(generation) = profiles_generation {
                    let service = profiles.clone();
                    let result = executor
                        .clone()
                        .run(move |_| async move { service.list_profiles().await })
                        .await;
                    let _ = this.update_in(cx, |panel, window, cx| {
                        panel.finish_profile_revision_check(result, generation, window, cx)
                    });
                }
            }
        })
        .detach();
    }

    /// watcher 使用独立代次读取 revision，但不锁住表单或提前判定外部冲突。
    ///
    /// @author kongweiguang
    pub(super) fn begin_settings_revision_check(&mut self) -> u64 {
        self.settings_load_generation = self.settings_load_generation.wrapping_add(1).max(1);
        self.settings_load_generation
    }

    /// 保存期间错过的 watcher 事件在回执后 typed 读回；冲突先合并，未知结果先核实。
    ///
    /// @author kongweiguang
    pub(super) fn spawn_settings_revision_check(&mut self, cx: &mut Context<Self>) {
        if self
            .settings
            .as_ref()
            .is_some_and(|draft| draft.status() == DraftStatus::Saving)
        {
            return;
        }
        let generation = self.begin_settings_revision_check();
        let service = self.preferences.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { service.load_settings().await })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.finish_settings_revision_check(result, generation, window, cx)
            });
        })
        .detach();
    }

    /// 每次重新打开设置时重试 unknown/conflict 的只读回读，不重放原写入或增加界面按钮。
    ///
    /// @author kongweiguang
    pub(super) fn recover_pending_settings_on_open(&mut self, cx: &mut Context<Self>) {
        let needs_readback = self
            .settings
            .as_ref()
            .is_some_and(|draft| needs_settings_recovery_readback(draft.status()));
        if needs_readback {
            self.spawn_settings_revision_check(cx);
        }
    }

    /// 外部快照按 baseline/local/remote 三方合并；未知写入只在回读等于提交快照时确认，
    /// 同 revision 的成功写入回声则续存保存期间已排队的后续编辑。
    ///
    /// @author kongweiguang
    fn finish_settings_revision_check(
        &mut self,
        result: Result<
            kerminal_application::preferences::VersionedSettings,
            kerminal_application::AppError,
        >,
        generation: u64,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) {
        if generation != self.settings_load_generation {
            return;
        }

        if self.loading_settings && self.settings.is_some() {
            self.finish_settings_load(result, generation, window, cx);
            return;
        }

        let initial_load = self.settings.is_none();
        if initial_load && self.loading_settings {
            self.loading_settings = false;
        }

        let mut resave_settings_after_readback = false;
        match result {
            Ok(versioned) => {
                if self
                    .settings
                    .as_ref()
                    .is_some_and(|draft| draft.status() == DraftStatus::OutcomeUnknown)
                {
                    self.finish_unknown_settings_readback(versioned, window, cx);
                    cx.notify();
                    return;
                }
                let current = self.settings.as_ref();
                let action = Self::settings_revision_action(
                    current.map(|draft| draft.status()),
                    current.is_some_and(|draft| draft.is_dirty()),
                    self.recovery.mode == ConfigurationMode::ReadOnlyRecovery,
                    current.and_then(|draft| draft.revision().as_ref()),
                    versioned.revision.as_ref(),
                );
                match action {
                    SettingsRevisionAction::Install => {
                        let previous_theme_mode = self.theme_mode;
                        let loaded_theme_mode = versioned.settings.theme_mode;
                        let loaded_settings = versioned.settings.clone();
                        self.install_settings(versioned, window, cx);
                        self.settings_error = None;
                        if !initial_load {
                            self.status = Some(
                                crate::locale::text(
                                    "设置已从外部更新。",
                                    "Settings updated from outside the app.",
                                )
                                .to_string(),
                            );
                        }
                        if previous_theme_mode != loaded_theme_mode {
                            self.set_theme_mode(loaded_theme_mode, cx);
                        }
                        cx.emit(super::PreferencesEvent::SettingsLoaded(loaded_settings));
                    }
                    SettingsRevisionAction::Preserve => {}
                    SettingsRevisionAction::SelfWriteEcho => {
                        if let Some(draft) = self.settings.as_mut()
                            && draft.status() == DraftStatus::ExternalConflict
                        {
                            let local = draft.value().clone();
                            let snapshot = DraftSnapshot {
                                value: versioned.settings.clone(),
                                revision: versioned.revision.clone(),
                            };
                            if draft.rebase(snapshot, local).is_ok() {
                                self.settings_autosave.invalidate_pending();
                                resave_settings_after_readback = draft.is_dirty();
                            }
                        } else if self.settings.as_ref().is_some_and(|draft| {
                            should_resave_ready_self_write_echo(draft.status(), draft.is_dirty())
                        }) {
                            self.settings_autosave.invalidate_pending();
                            resave_settings_after_readback = true;
                        }
                        self.settings_error = None;
                    }
                    SettingsRevisionAction::ExternalConflict => {
                        let merged = self.settings.as_ref().and_then(|draft| {
                            merge_settings_snapshot(
                                draft.baseline_value(),
                                draft.value(),
                                &versioned.settings,
                            )
                            .ok()
                        });
                        if let (Some(draft), Some(merged)) = (self.settings.as_mut(), merged) {
                            let snapshot = DraftSnapshot {
                                value: versioned.settings,
                                revision: versioned.revision,
                            };
                            if draft.rebase(snapshot, merged.clone()).is_ok() {
                                resave_settings_after_readback = draft.is_dirty();
                                self.sync_settings_inputs_preserving_errors(&merged, window, cx);
                                self.rebuild_shortcut_inputs(&merged, window, cx);
                                self.reconcile_keyword_editor(
                                    &merged.terminal.keyword_highlights.rules,
                                    window,
                                    cx,
                                );
                                self.set_theme_mode(merged.theme_mode, cx);
                                self.settings_autosave.invalidate_pending();
                                cx.emit(super::PreferencesEvent::SettingsPreview(merged));
                            }
                            let message = crate::locale::text(
                                "设置已在外部更新，当前编辑内容已保留。",
                                "Settings were updated outside the app; your current edits were preserved.",
                            ).to_string();
                            self.settings_error = Some(message.clone());
                            self.status = Some(message);
                        } else {
                            self.settings_error = Some(crate::locale::text(
                                "设置已在外部更新，但当前草稿无法安全合并；请保留草稿并检查配置。",
                                "Settings changed outside the app, but the current draft could not be merged safely. Keep the draft and inspect the configuration.",
                            ).to_string());
                        }
                    }
                    SettingsRevisionAction::ReadOnlyChanged => {
                        self.settings_error = Some(crate::locale::text(
                            "配置在只读恢复模式下发生变化；当前草稿未被替换。",
                            "Configuration changed while recovery mode is read-only; the current draft was preserved.",
                        ).to_string());
                    }
                }
            }
            Err(error) => {
                self.settings_error = Some(super::localized_error(&error));
                if initial_load {
                    self.status = None;
                }
            }
        }
        if resave_settings_after_readback {
            self.save_settings(cx);
        }
        self.flush_pending_workbench_layout(cx);
        cx.notify();
    }

    /// 未知结果读回必须匹配提交快照；确认基线后再 CAS 保存期间产生的较新编辑。
    ///
    /// @author kongweiguang
    fn finish_unknown_settings_readback(
        &mut self,
        versioned: VersionedSettings,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) {
        let submitted = self.settings.as_ref().and_then(|draft| {
            draft
                .pending_save_value()
                .filter(|_| draft.status() == DraftStatus::OutcomeUnknown)
                .cloned()
        });
        let Some(submitted) = submitted else {
            self.settings_error = Some(crate::locale::text(
                "写入结果未知，且没有可核对的提交快照；当前草稿已保留。",
                "The write result is unknown and its submitted snapshot is unavailable. The draft was preserved.",
            ).to_string());
            return;
        };
        if !unknown_settings_readback_matches(&submitted, &versioned.settings) {
            self.settings_error = Some(crate::locale::text(
                "写入结果仍未确认；回读配置与提交内容不一致，当前草稿已保留。",
                "The write result is still unconfirmed because the readback differs from the submitted settings. The draft was preserved.",
            ).to_string());
            self.status = self.settings_error.clone();
            return;
        }

        let snapshot = DraftSnapshot {
            value: versioned.settings.clone(),
            revision: versioned.revision,
        };
        let confirmed = self
            .settings
            .as_mut()
            .is_some_and(|draft| draft.confirm_outcome_unknown(snapshot).is_ok());
        if !confirmed {
            self.settings_error = Some(crate::locale::text(
                "写入结果未知；回读内容相同，但草稿状态未能安全确认。",
                "The readback matches the submitted settings, but the draft state could not be safely confirmed.",
            ).to_string());
            return;
        }
        self.confirm_keyword_rule_saved(&versioned.settings, window, cx);

        let current = self.settings.as_ref().map(|draft| draft.value().clone());
        if let Some(settings) = current.as_ref() {
            self.sync_settings_inputs_preserving_errors(settings, window, cx);
            self.rebuild_shortcut_inputs(settings, window, cx);
            self.reconcile_keyword_editor(&settings.terminal.keyword_highlights.rules, window, cx);
            self.set_theme_mode(settings.theme_mode, cx);
        }
        let has_follow_up_edits = self.settings.as_ref().is_some_and(ConfigDraft::is_dirty);
        self.settings_error = None;
        self.status = None;
        self.settings_success_status =
            (!has_follow_up_edits).then_some(super::SettingsSuccessStatus::Saved);
        cx.emit(super::PreferencesEvent::SettingsSaved(versioned.settings));
        if has_follow_up_edits && let Some(settings) = current {
            cx.emit(super::PreferencesEvent::SettingsPreview(settings));
        }
        self.settings_autosave.invalidate_pending();
        if has_follow_up_edits {
            self.save_settings(cx);
        }
        self.flush_pending_workbench_layout(cx);
    }

    /// 合并或未知结果回读时同步可用快照，但保留当前未解析数字字段的原始文本。
    fn sync_settings_inputs_preserving_errors(
        &self,
        settings: &SettingsSnapshot,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) {
        let invalid_texts = self
            .settings_input_errors
            .keys()
            .filter_map(|key| {
                let input = match *key {
                    "font_size" => Some(&self.settings_inputs.font_size),
                    "line_height" => Some(&self.settings_inputs.line_height),
                    "scrollback" => Some(&self.settings_inputs.scrollback),
                    "background_opacity" => Some(&self.settings_inputs.background_opacity),
                    "window_opacity" => Some(&self.settings_inputs.window_opacity),
                    "sftp_global_transfers" => Some(&self.settings_inputs.sftp_global_transfers),
                    "sftp_host_transfers" => Some(&self.settings_inputs.sftp_host_transfers),
                    "sftp_pipeline_depth" => Some(&self.settings_inputs.sftp_pipeline_depth),
                    "sftp_packet_bytes" => Some(&self.settings_inputs.sftp_packet_bytes),
                    "sftp_idle_timeout" => Some(&self.settings_inputs.sftp_idle_timeout),
                    "notification_min_duration" => {
                        Some(&self.settings_inputs.notification_min_duration)
                    }
                    "notification_throttle" => Some(&self.settings_inputs.notification_throttle),
                    _ => None,
                }?;
                Some((input.clone(), input.read(cx).value().to_string()))
            })
            .collect::<Vec<_>>();
        self.sync_settings_inputs(settings, window, cx);
        for (input, text) in invalid_texts {
            super::forms::set_input(&input, text, window, cx);
        }
    }

    /// watcher 查询不禁用表单；显式重载期间延后读取，避免旧回执取代用户的恢复动作。
    ///
    /// @author kongweiguang
    fn prepare_external_profile_change(&mut self) -> Option<u64> {
        if self.loading_profiles {
            self.profiles_watch_pending = true;
            return None;
        }
        self.profiles_watch_generation = self.profiles_watch_generation.wrapping_add(1).max(1);
        self.profiles_watch_in_flight = true;
        Some(self.profiles_watch_generation)
    }

    /// 被显式读取覆盖的监听通知在该读取结束后再次核对，不假设第一次快照已经包含后发修改。
    ///
    /// @author kongweiguang
    pub(super) fn resume_deferred_profile_revision_check(&mut self, cx: &mut Context<Self>) {
        if !self.profiles_watch_pending || self.loading_profiles {
            return;
        }
        self.profiles_watch_pending = false;
        let Some(generation) = self.prepare_external_profile_change() else {
            return;
        };
        let profiles = self.profiles.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { profiles.list_profiles().await })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.finish_profile_revision_check(result, generation, window, cx)
            });
        })
        .detach();
    }

    /// 回执到达时重新比较当前选中项 revision；相同版本只更新列表，避免覆盖输入或误报自身回声。
    /// 写入中与未知回执的草稿始终由原事务收尾，外部修改只对真正变化的 dirty Profile 标记冲突。
    ///
    /// @author kongweiguang
    fn finish_profile_revision_check(
        &mut self,
        result: Result<Vec<VersionedProfile>, kerminal_application::AppError>,
        generation: u64,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) {
        if generation != self.profiles_watch_generation {
            return;
        }
        self.profiles_watch_in_flight = false;
        if self.loading_profiles {
            self.profiles_watch_pending = true;
            return;
        }
        let rows = match result {
            Ok(rows) => rows,
            Err(error) => {
                if !self
                    .profile_draft
                    .as_ref()
                    .is_some_and(|draft| draft.status() == DraftStatus::OutcomeUnknown)
                {
                    self.profile_list_error = Some(super::localized_error(&error));
                }
                cx.notify();
                return;
            }
        };
        let protected = self.profile_operation_running
            || self.profile_draft.as_ref().is_some_and(|draft| {
                matches!(
                    draft.status(),
                    DraftStatus::Saving | DraftStatus::OutcomeUnknown
                )
            });
        if protected {
            self.profile_rows = rows;
            cx.notify();
            return;
        }

        let selected = self
            .selected_profile_id
            .as_deref()
            .and_then(|id| rows.iter().find(|row| row.profile.id == id).cloned());
        if let Some(draft) = self.profile_draft.as_mut() {
            if self.selected_profile_id.is_none() {
                self.profile_rows = rows;
                cx.notify();
                return;
            }
            let observed = selected.as_ref().map(|row| &row.revision);
            if draft.revision().as_ref() == observed {
                self.selected_profile_is_default =
                    selected.as_ref().is_some_and(|row| row.profile.is_default);
                self.profile_rows = rows;
                if draft.status() != DraftStatus::ExternalConflict {
                    self.profile_list_error = None;
                }
                cx.notify();
                return;
            }
            if draft.is_dirty() || draft.status() == DraftStatus::ExternalConflict {
                draft.mark_external_change();
                self.profile_rows = rows;
                self.profile_list_error = Some(crate::locale::text(
                    "Profile 文件已被外部修改；本地草稿已保留，请重新读取后再保存。",
                    "This profile changed outside the app. Your draft is preserved; reload before saving.",
                ).to_string());
                cx.notify();
                return;
            }
        }

        let selected = selected
            .or_else(|| rows.iter().find(|row| row.profile.is_default).cloned())
            .or_else(|| rows.first().cloned());
        self.profile_rows = rows;
        self.profile_list_error = None;
        if let Some(profile) = selected {
            self.install_profile(profile, window, cx);
        } else {
            self.selected_profile_id = None;
            self.selected_profile_is_default = false;
            self.profile_draft = None;
        }
        cx.notify();
    }
}

/// 同 revision 回读只在先前保存已成功、草稿回到 Ready 且仍有改动时自动续写。
/// Failed、ExternalConflict 和 OutcomeUnknown 继续服从各自的显式恢复门禁。
///
/// @author kongweiguang
pub(super) fn should_resave_ready_self_write_echo(status: DraftStatus, dirty: bool) -> bool {
    status == DraftStatus::Ready && dirty
}

/// 以序列化对象字段三方合并设置；serde 的结构校验失败时不推进 revision。
fn merge_settings_snapshot(
    baseline: &SettingsSnapshot,
    local: &SettingsSnapshot,
    remote: &SettingsSnapshot,
) -> Result<SettingsSnapshot, ()> {
    let baseline = serde_json::to_value(baseline).map_err(|_| ())?;
    let local = serde_json::to_value(local).map_err(|_| ())?;
    let remote = serde_json::to_value(remote).map_err(|_| ())?;
    serde_json::from_value(merge_json_value(&baseline, &local, &remote)).map_err(|_| ())
}

/// 未知回执只有在完整 typed 快照与提交值一致时才可确认，避免部分相等误判为成功。
fn unknown_settings_readback_matches(
    submitted: &SettingsSnapshot,
    readback: &SettingsSnapshot,
) -> bool {
    submitted == readback
}

/// 未改字段取远端、单边修改取修改方、双边对象递归合并，数组或叶子冲突保留本地值。
fn merge_json_value(
    baseline: &serde_json::Value,
    local: &serde_json::Value,
    remote: &serde_json::Value,
) -> serde_json::Value {
    if local == baseline {
        return remote.clone();
    }
    if remote == baseline || local == remote {
        return local.clone();
    }
    match (baseline, local, remote) {
        (
            serde_json::Value::Object(base),
            serde_json::Value::Object(local),
            serde_json::Value::Object(remote),
        ) => serde_json::Value::Object(merge_json_object(base, local, remote)),
        _ => local.clone(),
    }
}

/// 合并对象键的增删与递归值，冲突时保留 local 包含的用户选择。
fn merge_json_object(
    baseline: &serde_json::Map<String, serde_json::Value>,
    local: &serde_json::Map<String, serde_json::Value>,
    remote: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let keys = baseline
        .keys()
        .chain(local.keys())
        .chain(remote.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut merged = serde_json::Map::new();
    for key in keys {
        if let Some(value) = merge_json_entry(baseline.get(&key), local.get(&key), remote.get(&key))
        {
            merged.insert(key, value);
        }
    }
    merged
}

/// 对单个可选对象成员执行三方选择，删除与并发修改冲突时仍保留本地操作。
fn merge_json_entry(
    baseline: Option<&serde_json::Value>,
    local: Option<&serde_json::Value>,
    remote: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    if local == baseline {
        return remote.cloned();
    }
    if remote == baseline || local == remote {
        return local.cloned();
    }
    match (baseline, local, remote) {
        (
            Some(serde_json::Value::Object(base)),
            Some(serde_json::Value::Object(local)),
            Some(serde_json::Value::Object(remote)),
        ) => Some(serde_json::Value::Object(merge_json_object(
            base, local, remote,
        ))),
        _ => local.cloned(),
    }
}

/// 将配置根相对路径归一化后只匹配 settings.toml，避免把扩展文件误当主设置。
fn paths_change_settings(paths: &[String]) -> bool {
    paths
        .iter()
        .any(|path| normalize_relative_path(path) == "settings.toml")
}

/// Profile watcher 覆盖受控 profile 文件及事务恢复目录，不触碰 hosts 或其它配置草稿。
fn paths_change_profiles(paths: &[String]) -> bool {
    paths.iter().any(|path| {
        let path = normalize_relative_path(path);
        path.starts_with("profiles/") || path.starts_with("profile-transactions/")
    })
}

/// Profile 分组引用受 hosts/groups.toml 管理，变更时刷新选项但不改写 Profile 草稿。
/// @author kongweiguang
fn paths_change_host_groups(paths: &[String]) -> bool {
    paths
        .iter()
        .any(|path| normalize_relative_path(path) == "hosts/groups.toml")
}

/// 把 watcher 的平台分隔符归一化为配置契约中的斜杠路径。
fn normalize_relative_path(path: &str) -> String {
    path.trim_start_matches("./").replace('\\', "/")
}

/// 面板实体销毁时停止接收，使 watcher 不会因 detached future 延长订阅生命周期。
impl Drop for PreferencesPanel {
    fn drop(&mut self) {
        self.configuration_change_cancellation.cancel();
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/preferences_revision.rs"]
mod revision_tests;
