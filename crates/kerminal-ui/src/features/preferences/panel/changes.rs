//! 配置 watcher 与偏好草稿的并发边界。
//!
//! @author kongweiguang

use gpui_kit::Context;
use kerminal_application::configuration_changes::ConfigurationChangeMessage;
use kerminal_domain::preferences::{ConfigRevision, ConfigurationMode};

use super::super::DraftStatus;
use super::PreferencesPanel;

/// watcher 快照的窄决策结果；只有明确安全的分支才能替换现有草稿。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsRevisionAction {
    Install,
    Preserve,
    SelfWriteEcho,
    ExternalConflict,
    ReadOnlyChanged,
}

impl PreferencesPanel {
    /// 将 watcher 快照归类为保留、安装或冲突，避免后台读回替换受保护草稿。
    ///
    /// @author kongweiguang
    fn settings_revision_action(
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
            DraftStatus::Saving | DraftStatus::OutcomeUnknown | DraftStatus::ExternalConflict => {
                SettingsRevisionAction::Preserve
            }
            DraftStatus::Ready | DraftStatus::Failed => {
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
                        panel.finish_profile_load(result, generation, window, cx)
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

    /// 保存期间错过的 watcher 事件在明确写入回执后补一次 typed 读回，未知结果仍交给用户确认。
    ///
    /// @author kongweiguang
    pub(super) fn spawn_settings_revision_check(&mut self, cx: &mut Context<Self>) {
        if self.settings.as_ref().is_some_and(|draft| {
            matches!(
                draft.status(),
                DraftStatus::Saving | DraftStatus::OutcomeUnknown
            )
        }) {
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

    /// 仅在版本相同的自通知时保留编辑；不同版本冲突，未知/写入中状态不由 watcher 解除。
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

        match result {
            Ok(versioned) => {
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
                        self.settings_error = None;
                    }
                    SettingsRevisionAction::ExternalConflict => {
                        if let Some(draft) = self.settings.as_mut() {
                            draft.mark_external_change();
                        }
                        self.settings_error = Some(crate::locale::text(
                            "配置文件已被外部修改；本地草稿已保留，请重新读取后再保存。",
                            "Settings changed outside the app. Your draft is preserved; reload before saving.",
                        ).to_string());
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
        cx.notify();
    }

    /// 外部 Profile 变化只冲突当前 dirty 草稿；其余情况可通过 profile list typed 查询收敛。
    fn prepare_external_profile_change(&mut self) -> Option<u64> {
        let Some(draft) = self.profile_draft.as_mut() else {
            return Some(self.begin_profile_load());
        };
        match draft.status() {
            super::super::DraftStatus::Saving | super::super::DraftStatus::OutcomeUnknown => None,
            super::super::DraftStatus::ExternalConflict => None,
            super::super::DraftStatus::Ready | super::super::DraftStatus::Failed => {
                if draft.is_dirty() {
                    draft.mark_external_change();
                    self.profile_list_error = Some(crate::locale::text(
                        "Profile 文件已被外部修改；本地草稿已保留，请重新读取后再保存。",
                        "This profile changed outside the app. Your draft is preserved; reload before saving.",
                    ).to_string());
                    None
                } else {
                    Some(self.begin_profile_load())
                }
            }
        }
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
