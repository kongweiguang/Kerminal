//! Profile 表单校验、切换、保存与事务操作。
//!
//! @author kongweiguang

use std::collections::BTreeMap;

use super::{
    PreferencesPanel, PreferencesSection, ProfileTextField, parse_environment, profile_to_draft,
};
use crate::features::preferences::{
    ConfigDraft, DraftSnapshot, DraftStatus, DraftTransitionError, validate_profile,
};
use gpui_kit::{Context, Window};
use kerminal_application::{
    AppError, PublicErrorCode,
    preferences::{ProfileDraft, ProfileRevision, VersionedProfile},
};
use kerminal_domain::preferences::ConfigurationMode;

impl PreferencesPanel {
    /// Profile 输入即时转换为 typed draft；重载/事务期间拒绝迟到的控件事件以保护新快照。
    pub(super) fn update_profile_text(
        &mut self,
        field: ProfileTextField,
        text: String,
        cx: &mut Context<Self>,
    ) {
        if self.loading_profiles
            || self.profile_operation_running
            || self.profile_draft.as_ref().is_some_and(|draft| {
                matches!(
                    draft.status(),
                    DraftStatus::Saving
                        | DraftStatus::OutcomeUnknown
                        | DraftStatus::ExternalConflict
                )
            })
        {
            return;
        }
        let Some(draft) = self.profile_draft.as_mut() else {
            return;
        };
        let mut value = draft.value().clone();
        let (key, parse_error) = match field {
            ProfileTextField::Name => {
                value.name = text;
                ("name", None)
            }
            ProfileTextField::Shell => {
                value.shell = text;
                ("shell", None)
            }
            ProfileTextField::WorkingDirectory => {
                value.cwd = (!text.trim().is_empty()).then_some(text);
                ("working_directory", None)
            }
            ProfileTextField::Arguments => {
                value.args = if text.is_empty() {
                    Vec::new()
                } else {
                    text.split('\n').map(str::to_owned).collect()
                };
                ("arguments", None)
            }
            ProfileTextField::Environment => match parse_environment(&text) {
                Ok(environment) => {
                    value.env = environment;
                    ("environment", None)
                }
                Err(message) => ("environment", Some(message)),
            },
        };
        if let Some(message) = parse_error {
            self.profile_input_errors.insert(key, message);
        } else {
            self.profile_input_errors.remove(key);
            let _ = draft.replace_value(value);
        }
        cx.notify();
    }

    /// 在指定的已加载分组中新建 Profile，组关系只进入既有 Profile CAS 草稿。
    /// @author kongweiguang
    pub(super) fn create_profile_in_group(
        &mut self,
        group_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.section = PreferencesSection::Profiles;
        let next_order = self
            .profile_rows
            .iter()
            .map(|row| row.profile.sort_order)
            .max()
            .map_or(0, |sort_order| sort_order.saturating_add(1));
        self.selected_profile_id = None;
        self.reset_profile_form(
            ProfileDraft {
                id: None,
                name: String::new(),
                shell: String::new(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                sidebar_group_id: group_id,
                sort_order: next_order,
            },
            None,
            false,
            window,
            cx,
        );
        self.status = Some(
            crate::locale::text(
                "填写名称和 shell 后保存新 Profile。",
                "Enter a name and shell, then save the profile.",
            )
            .to_string(),
        );
        cx.notify();
    }

    /// 切换或新建 Profile 前先检查当前草稿，丢弃动作需由用户在确认条中明确选择。
    pub(in crate::features::preferences) fn request_profile_selection(
        &mut self,
        selection: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_profile_selection_in_group(selection, None, window, cx);
    }

    /// 创建新 Profile 时保留来源分组；切换已保存 Profile 时 group override 必须为空。
    /// @author kongweiguang
    pub(in crate::features::preferences) fn request_profile_selection_in_group(
        &mut self,
        selection: Option<String>,
        new_profile_group: Option<Option<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .profile_draft
            .as_ref()
            .is_some_and(|draft| draft.status() == DraftStatus::OutcomeUnknown)
        {
            self.status = Some(
                crate::locale::text(
                    "Profile 写入结果未知，请先重新读取列表确认。",
                    "The profile write result is unknown. Reload the list and verify it first.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if self
            .profile_draft
            .as_ref()
            .is_some_and(ConfigDraft::is_dirty)
        {
            self.pending_profile_selection = Some(selection);
            self.pending_profile_group_id = new_profile_group;
            self.profile_switch_confirmation = true;
            cx.notify();
            return;
        }
        self.apply_profile_selection(selection, new_profile_group, window, cx);
    }

    /// 切换时重用已加载 revision；新建动作不经过任何配置文件读写。
    fn apply_profile_selection(
        &mut self,
        selection: Option<String>,
        new_profile_group: Option<Option<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.profile_switch_confirmation = false;
        self.pending_profile_selection = None;
        self.pending_profile_group_id = None;
        match selection {
            Some(id) => {
                if let Some(row) = self
                    .profile_rows
                    .iter()
                    .find(|row| row.profile.id == id)
                    .cloned()
                {
                    self.install_profile(row, window, cx);
                }
            }
            None => self.create_profile_in_group(new_profile_group.unwrap_or(None), window, cx),
        }
        cx.notify();
    }

    /// 用户确认丢弃后才切换 Profile，取消则原样保留草稿和 revision。
    pub(in crate::features::preferences) fn confirm_profile_discard(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selection = self.pending_profile_selection.take();
        let new_profile_group = self.pending_profile_group_id.take();
        self.profile_switch_confirmation = false;
        if let Some(draft) = self.profile_draft.as_mut() {
            let _ = draft.discard_changes();
        }
        self.apply_profile_selection(selection.unwrap_or(None), new_profile_group, window, cx);
    }

    /// 分组控件只替换 typed 草稿值；保存/重载期间忽略迟到事件，避免更改 CAS 请求。
    /// @author kongweiguang
    pub(super) fn update_profile_group(
        &mut self,
        group_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.loading_profiles || self.profile_operation_running {
            return;
        }
        let Some(draft) = self.profile_draft.as_mut() else {
            return;
        };
        if matches!(
            draft.status(),
            DraftStatus::Saving | DraftStatus::OutcomeUnknown | DraftStatus::ExternalConflict
        ) {
            return;
        }
        let mut value = draft.value().clone();
        value.sidebar_group_id = group_id;
        if draft.replace_value(value).is_ok() {
            self.profile_group_error = None;
        }
        cx.notify();
    }

    /// 向 application 提交完整 typed Profile draft，保存結果回傳的 ID/revision 才建立新基線。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn save_profile(&mut self, cx: &mut Context<Self>) {
        if self.profile_operation_running {
            return;
        }
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            self.status = Some(
                crate::locale::text(
                    "配置处于只读恢复模式，暂不能保存 Profile。",
                    "Configuration is in read-only recovery mode and profiles cannot be saved yet.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if !self.profile_input_errors.is_empty() {
            self.status = Some(
                crate::locale::text(
                    "请先修正环境变量格式。",
                    "Fix the environment variable format first.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if let Some(group_id) = self
            .profile_draft
            .as_ref()
            .and_then(|draft| draft.value().sidebar_group_id.as_deref())
            && (self.host_management.is_none()
                || self.profile_group_loading
                || !self.profile_groups.iter().any(|group| group.id == group_id))
        {
            self.profile_group_error = Some(crate::locale::text(
                "无法确认所选分组仍然存在；刷新分组或选择未分组后再保存。",
                "The selected group could not be verified. Refresh groups or choose Ungrouped before saving.",
            ).to_string());
            self.status = self.profile_group_error.clone();
            cx.notify();
            return;
        }
        let Some(draft) = self.profile_draft.as_mut() else {
            return;
        };
        self.profile_issues = validate_profile(draft.value());
        if !self.profile_issues.is_empty() {
            self.status = Some(
                crate::locale::text(
                    "Profile 校验未通过，请检查提示项。",
                    "Profile validation failed. Review the messages below.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if let Err(error) = draft.begin_save() {
            self.status = Some(match error {
                DraftTransitionError::NoChanges => crate::locale::text(
                    "Profile 没有待保存的修改。",
                    "There are no profile changes to save.",
                )
                .to_string(),
                DraftTransitionError::ReloadRequired => crate::locale::text(
                    "Profile 已被外部修改，请先重载。",
                    "The profile changed externally. Reload it before saving.",
                )
                .to_string(),
                DraftTransitionError::ReadBackRequired => crate::locale::text(
                    "Profile 写入结果未知，请先重新读取并核对。",
                    "The profile write result is unknown. Read it back and verify it first.",
                )
                .to_string(),
                _ => crate::locale::text(
                    "Profile 正在保存，请稍候。",
                    "The profile is being saved. Please wait.",
                )
                .to_string(),
            });
            cx.notify();
            return;
        }
        let profile = draft.value().clone();
        let revision = draft.revision().clone();
        let service = self.profiles.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { service.save_profile(profile, revision).await })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.finish_profile_save(result, window, cx)
            });
        })
        .detach();
        self.status = Some(crate::locale::text("正在保存 Profile…", "Saving profile…").to_string());
        cx.notify();
    }

    /// 保存确认快照后刷新列表 revision，确保随后的默认切换使用完整的最新 CAS 集合。
    fn finish_profile_save(
        &mut self,
        result: Result<VersionedProfile, AppError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(saved) => {
                let id = saved.profile.id.clone();
                if let Some(draft) = self.profile_draft.as_mut() {
                    let value = profile_to_draft(saved.profile.clone());
                    let _ = draft.save_succeeded(DraftSnapshot {
                        value,
                        revision: Some(saved.revision.clone()),
                    });
                }
                self.selected_profile_id = Some(id);
                self.profile_input_errors.clear();
                self.profile_issues.clear();
                self.status =
                    Some(crate::locale::text("Profile 已保存。", "Profile saved.").to_string());
                self.load_profiles_again(cx);
            }
            Err(error) => {
                if let Some(draft) = self.profile_draft.as_mut() {
                    if error.public_code() == PublicErrorCode::Conflict {
                        let _ = draft.save_conflicted();
                    } else if error.public_code() == PublicErrorCode::OutcomeUnknown {
                        let _ = draft.save_outcome_unknown();
                    } else {
                        let _ = draft.save_failed();
                    }
                }
                self.status = Some(super::localized_error(&error));
            }
        }
        let _ = window;
        cx.notify();
    }

    /// 外部 profile 变更冲突时显式重新读取列表，不把旧草稿当成已保存值。
    pub(in crate::features::preferences) fn reload_profiles(&mut self, cx: &mut Context<Self>) {
        if self.loading_profiles
            || self
                .profile_draft
                .as_ref()
                .is_some_and(|draft| draft.status() == DraftStatus::Saving)
        {
            return;
        }
        if self
            .profile_draft
            .as_ref()
            .is_some_and(|draft| draft.status() == DraftStatus::OutcomeUnknown)
        {
            self.load_profiles_again(cx);
            return;
        }
        if self.profile_draft.as_ref().is_some_and(|draft| {
            draft.is_dirty() || draft.status() == DraftStatus::ExternalConflict
        }) {
            self.profile_reload_confirmation = true;
            cx.notify();
            return;
        }
        self.load_profiles_again(cx);
    }

    /// 放弃本地 Profile 草稿后才刷新外部版本，防止列表查询静默覆盖输入。
    pub(in crate::features::preferences) fn confirm_profile_reload(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if !self.profile_reload_confirmation {
            return;
        }
        self.profile_reload_confirmation = false;
        if let Some(draft) = self.profile_draft.as_mut() {
            let _ = draft.discard_changes();
        }
        self.load_profiles_again(cx);
    }

    /// 关闭 Profile 重载确认时保留当前草稿和外部冲突状态。
    pub(in crate::features::preferences) fn cancel_profile_reload(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        self.profile_reload_confirmation = false;
        cx.notify();
    }

    /// 为保存、删除和默认切换共用受任务 tracker 管理的列表刷新过程。
    fn load_profiles_again(&mut self, cx: &mut Context<Self>) {
        self.loading_profiles = true;
        self.profiles_load_started = true;
        self.spawn_profile_load(cx);
        cx.notify();
    }

    /// 删除前要求用户确认；默认项、脏草稿和恢复模式都由真实服务/写入门禁约束。
    pub(in crate::features::preferences) fn request_delete_profile(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        let can_delete = self.selected_profile_id.is_some()
            && self
                .profile_draft
                .as_ref()
                .is_some_and(|draft| draft.revision().is_some() && !draft.is_dirty());
        if !can_delete || self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            return;
        }
        self.profile_delete_confirmation = true;
        cx.notify();
    }

    /// 取消删除时不触及服务端状态或当前 Profile 草稿。
    pub(in crate::features::preferences) fn cancel_delete_profile(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        self.profile_delete_confirmation = false;
        cx.notify();
    }

    /// 用户确认后按当前 revision 执行 CAS 删除，过期结果要求重新加载列表。
    pub(in crate::features::preferences) fn confirm_delete_profile(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if !self.profile_delete_confirmation {
            return;
        }
        self.profile_delete_confirmation = false;
        self.delete_profile(cx);
    }

    /// 删除使用选中 Profile 的原始 revision；用户新增但未保存的草稿没有删除按钮。
    fn delete_profile(&mut self, cx: &mut Context<Self>) {
        if self.profile_operation_running {
            return;
        }
        let (Some(id), Some(draft), Some(revision)) = (
            self.selected_profile_id.clone(),
            self.profile_draft.as_ref(),
            self.profile_draft
                .as_ref()
                .and_then(|draft| draft.revision().clone()),
        ) else {
            return;
        };
        if draft.is_dirty() || draft.status() == DraftStatus::Saving {
            self.status = Some(
                crate::locale::text(
                    "请先保存或放弃 Profile 修改，再删除。",
                    "Save or discard the profile changes before deleting it.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if self.recovery.mode == ConfigurationMode::ReadOnlyRecovery {
            self.status = Some(crate::locale::text(
                "配置处于只读恢复模式，暂不能删除 Profile。",
                "Configuration is in read-only recovery mode and profiles cannot be deleted yet.",
            ).to_string());
            cx.notify();
            return;
        }
        let service = self.profiles.clone();
        let executor = self.executor.clone();
        self.profile_operation_running = true;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { service.delete_profile(&id, revision).await })
                .await;
            let _ = this.update_in(cx, |panel, _window, cx| {
                panel.profile_operation_running = false;
                match result {
                    Ok(true) => {
                        panel.selected_profile_id = None;
                        panel.profile_draft = None;
                        panel.status = Some(crate::locale::text("Profile 已删除。", "Profile deleted.").to_string());
                        panel.load_profiles_again(cx);
                    }
                    Ok(false) => {
                        panel.status = Some(crate::locale::text(
                            "Profile 已不存在，正在读取最新列表。",
                            "The profile no longer exists. Reloading the list.",
                        ).to_string());
                        panel.load_profiles_again(cx);
                    }
                    Err(error) => {
                        if error.public_code() == PublicErrorCode::OutcomeUnknown {
                            if let Some(draft) = panel.profile_draft.as_mut() {
                                draft.mark_outcome_unknown();
                            }
                            panel.status = Some(crate::locale::text(
                                "删除结果未知；请重新读取 Profile 列表核对后再继续。",
                                "The delete result is unknown. Reload the profile list and verify it before continuing.",
                            ).to_string());
                        } else {
                            panel.status = Some(super::localized_error(&error));
                        }
                        if error.public_code() == PublicErrorCode::Conflict {
                            panel.load_profiles_again(cx);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        self.status =
            Some(crate::locale::text("正在删除 Profile…", "Deleting profile…").to_string());
        cx.notify();
    }

    /// 切换默认项提交整个 profile revision 集合，保证基础设施可原子维护唯一默认状态。
    pub(in crate::features::preferences) fn set_default_profile(&mut self, cx: &mut Context<Self>) {
        if self.profile_operation_running {
            return;
        }
        let Some(id) = self.selected_profile_id.clone() else {
            return;
        };
        if self.selected_profile_is_default
            || self.recovery.mode == ConfigurationMode::ReadOnlyRecovery
        {
            return;
        }
        if self
            .profile_draft
            .as_ref()
            .is_some_and(ConfigDraft::is_dirty)
        {
            self.status = Some(
                crate::locale::text(
                    "请先保存或放弃 Profile 修改，再切换默认项。",
                    "Save or discard the profile changes before changing the default.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        let expected = self
            .profile_rows
            .iter()
            .map(|row| ProfileRevision {
                profile_id: row.profile.id.clone(),
                revision: row.revision.clone(),
            })
            .collect::<Vec<_>>();
        let service = self.profiles.clone();
        let executor = self.executor.clone();
        self.profile_operation_running = true;
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { service.set_default_profile(&id, expected).await })
                .await;
            let _ = this.update_in(cx, |panel, _window, cx| {
                panel.profile_operation_running = false;
                match result {
                    Ok(saved) => {
                        panel.selected_profile_id = Some(saved.profile.id);
                        panel.status = Some(crate::locale::text("默认 Profile 已更新。", "Default profile updated.").to_string());
                        panel.load_profiles_again(cx);
                    }
                    Err(error) => {
                        if error.public_code() == PublicErrorCode::OutcomeUnknown {
                            if let Some(draft) = panel.profile_draft.as_mut() {
                                draft.mark_outcome_unknown();
                            }
                            panel.status = Some(crate::locale::text(
                                "默认 Profile 更新结果未知；请重新读取列表核对后再继续。",
                                "The default profile update result is unknown. Reload the list and verify it before continuing.",
                            ).to_string());
                        } else {
                            panel.status = Some(super::localized_error(&error));
                        }
                        if error.public_code() == PublicErrorCode::Conflict {
                            panel.load_profiles_again(cx);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        self.status = Some(
            crate::locale::text("正在更新默认 Profile…", "Updating the default profile…")
                .to_string(),
        );
        cx.notify();
    }
}
