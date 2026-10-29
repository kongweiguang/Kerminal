//! 设置 watcher 读回与草稿 revision 的状态决策回归。
//!
//! @author kongweiguang

use super::{
    PreferencesPanel, SettingsRevisionAction, merge_json_value, merge_settings_snapshot,
    unknown_settings_readback_matches,
};
use crate::features::preferences::{ConfigDraft, DraftSnapshot, DraftStatus};
use kerminal_application::preferences::SettingsSnapshot;
use kerminal_domain::preferences::ConfigRevision;

/// 同 revision 自通知不覆盖后续编辑；新 revision 则保留草稿并进入显式冲突。
///
/// @author kongweiguang
#[test]
fn settings_watcher_echo_preserves_later_draft_and_external_revision_conflicts() {
    let saved_revision = ConfigRevision::from_sha256("saved-light".to_owned());
    let external_revision = ConfigRevision::from_sha256("external-write".to_owned());
    let mut draft = ConfigDraft::new(DraftSnapshot {
        value: "light".to_owned(),
        revision: Some(saved_revision.clone()),
    });
    draft.replace_value("system".to_owned()).unwrap();

    let echo = PreferencesPanel::settings_revision_action(
        Some(draft.status()),
        draft.is_dirty(),
        false,
        draft.revision().as_ref(),
        Some(&saved_revision),
    );
    assert_eq!(echo, SettingsRevisionAction::SelfWriteEcho);
    assert_eq!(draft.status(), DraftStatus::Ready);
    assert_eq!(draft.value(), "system");
    assert!(draft.is_dirty());

    let external = PreferencesPanel::settings_revision_action(
        Some(draft.status()),
        draft.is_dirty(),
        false,
        draft.revision().as_ref(),
        Some(&external_revision),
    );
    assert_eq!(external, SettingsRevisionAction::ExternalConflict);
    assert!(draft.mark_external_change());
    assert_eq!(draft.status(), DraftStatus::ExternalConflict);
    assert_eq!(draft.value(), "system");
    assert!(draft.is_dirty());
}

/// Saving、未知回执与只读状态不能由 watcher 当作 Ready/可覆盖草稿。
///
/// @author kongweiguang
#[test]
fn settings_watcher_preserves_inflight_unknown_and_read_only_snapshots() {
    let base_revision = ConfigRevision::from_sha256("base".to_owned());
    let changed_revision = ConfigRevision::from_sha256("later".to_owned());
    let mut draft = ConfigDraft::new(DraftSnapshot {
        value: "base".to_owned(),
        revision: Some(base_revision.clone()),
    });
    draft.replace_value("local".to_owned()).unwrap();
    draft.begin_save().unwrap();

    let saving = PreferencesPanel::settings_revision_action(
        Some(draft.status()),
        draft.is_dirty(),
        false,
        draft.revision().as_ref(),
        Some(&changed_revision),
    );
    assert_eq!(saving, SettingsRevisionAction::Preserve);
    assert_eq!(draft.status(), DraftStatus::Saving);
    draft.save_outcome_unknown().unwrap();

    let unknown = PreferencesPanel::settings_revision_action(
        Some(draft.status()),
        draft.is_dirty(),
        false,
        draft.revision().as_ref(),
        Some(&changed_revision),
    );
    assert_eq!(unknown, SettingsRevisionAction::Preserve);
    assert_eq!(draft.status(), DraftStatus::OutcomeUnknown);
    assert_eq!(draft.value(), "local");

    let readonly = PreferencesPanel::settings_revision_action(
        Some(DraftStatus::Ready),
        false,
        true,
        Some(&base_revision),
        Some(&changed_revision),
    );
    assert_eq!(readonly, SettingsRevisionAction::ReadOnlyChanged);
}

/// 三方合并吸收未编辑远端字段，保留本地字段以及双方并发修改的数组字段。
#[test]
fn settings_three_way_merge_rebases_remote_fields_and_keeps_local_conflicts() {
    let baseline = serde_json::json!({
        "theme": "dark",
        "terminal": {"font": "A", "size": 14},
        "rules": [{"pattern": "old"}]
    });
    let local = serde_json::json!({
        "theme": "dark",
        "terminal": {"font": "B", "size": 14},
        "rules": [{"pattern": "local"}]
    });
    let remote = serde_json::json!({
        "theme": "light",
        "terminal": {"font": "remote", "size": 16},
        "rules": [{"pattern": "remote"}]
    });

    assert_eq!(
        merge_json_value(&baseline, &local, &remote),
        serde_json::json!({
            "theme": "light",
            "terminal": {"font": "B", "size": 16},
            "rules": [{"pattern": "local"}]
        })
    );
}

/// 只有远端变化的数组字段整体刷新，不误保留本地基线内容。
#[test]
fn settings_three_way_merge_takes_remote_for_untouched_arrays() {
    let baseline = serde_json::json!({"rules": ["old"]});
    let local = baseline.clone();
    let remote = serde_json::json!({"rules": ["new"]});

    assert_eq!(merge_json_value(&baseline, &local, &remote), remote);
}

/// typed SettingsSnapshot 经 serde 三方合并后保留字段类型与默认完整结构。
#[test]
fn settings_snapshot_three_way_merge_preserves_independent_typed_edits() {
    let baseline = SettingsSnapshot::default();
    let mut local = baseline.clone();
    local.terminal.font_family = "Cascadia Mono".to_owned();
    let mut remote = baseline.clone();
    remote.theme_mode = crate::features::preferences::ThemeMode::Light;

    let merged = merge_settings_snapshot(&baseline, &local, &remote)
        .expect("typed settings should serialize and deserialize for field merging");
    assert_eq!(merged.theme_mode, remote.theme_mode);
    assert_eq!(merged.terminal.font_family, local.terminal.font_family);
}

/// 未知提交只接受完整快照相等的读回，远端多字段变化不能被当作成功回执。
#[test]
fn unknown_settings_readback_requires_full_snapshot_equality() {
    let submitted = SettingsSnapshot::default();
    assert!(unknown_settings_readback_matches(
        &submitted,
        &submitted.clone()
    ));

    let mut changed = submitted.clone();
    changed.background_enabled = true;
    assert!(!unknown_settings_readback_matches(&submitted, &changed));
}

/// 外部更新可合并终端与明暗字段，本地配色编辑及同字段冲突始终保留在草稿中。
///
/// @author kongweiguang
#[test]
fn app_color_scheme_rebase_preserves_local_and_external_independent_changes() {
    use kerminal_application::preferences::{AppColorScheme, TerminalColorScheme, ThemeMode};

    let baseline = SettingsSnapshot::default();
    let mut local = baseline.clone();
    local.app_color_scheme = AppColorScheme::Fleet;
    let mut remote = baseline.clone();
    remote.app_color_scheme = AppColorScheme::Claude;
    remote.theme_mode = ThemeMode::Light;
    remote.terminal.light_color_scheme = TerminalColorScheme::Solarized;

    let merged = merge_settings_snapshot(&baseline, &local, &remote).unwrap();
    assert_eq!(merged.app_color_scheme, AppColorScheme::Fleet);
    assert_eq!(merged.theme_mode, ThemeMode::Light);
    assert_eq!(
        merged.terminal.light_color_scheme,
        TerminalColorScheme::Solarized
    );

    let unedited = merge_settings_snapshot(&baseline, &baseline, &remote).unwrap();
    assert_eq!(unedited.app_color_scheme, AppColorScheme::Claude);
}

/// 未知保存结果必须核对应用色板本身，不能把同明暗或同终端色盘当成已保存。
///
/// @author kongweiguang
#[test]
fn app_color_scheme_unknown_outcome_requires_exact_readback() {
    use kerminal_application::preferences::AppColorScheme;

    let submitted = SettingsSnapshot {
        app_color_scheme: AppColorScheme::Xcode,
        ..SettingsSnapshot::default()
    };
    let old = SettingsSnapshot::default();
    assert!(!unknown_settings_readback_matches(&submitted, &old));
    assert!(unknown_settings_readback_matches(
        &submitted,
        &submitted.clone()
    ));
}

mod profile_watcher {
    use super::*;
    use std::{
        cell::RefCell,
        collections::BTreeMap,
        ffi::OsString,
        rc::Rc,
        sync::{Arc, Mutex},
    };

    use crate::features::preferences::panel::{PreferencesPanelServices, PreferencesSection};
    use async_trait::async_trait;
    use gpui_kit::{AppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext};
    use kerminal_application::{
        AppError, AppResult, ApplicationTaskExecutor,
        preferences::{
            ConfigurationRepository, DefaultLocalShellProvider, PreferencesService, ProfileDraft,
            ProfileRepository, ProfileRevision, ProfileService, SettingsRepository,
            VersionedProfile, VersionedSettings,
        },
    };
    use kerminal_domain::{
        preferences::{
            ConfigDocument, ConfigDocumentPath, ConfigurationMode, ConfigurationRecoveryState,
            TerminalProfile,
        },
        sessions::LocalSessionTarget,
    };
    use tokio::runtime::{Builder, Runtime};

    /// 仓储只保留隔离快照，测试直接控制 watcher 回执的 revision 和到达次序。
    struct Repository(Mutex<Vec<VersionedProfile>>);

    #[async_trait]
    impl SettingsRepository for Repository {
        /// 不读取用户配置，提供可渲染的 typed 默认设置。
        async fn load_settings(&self) -> AppResult<VersionedSettings> {
            Ok(VersionedSettings {
                settings: SettingsSnapshot::default(),
                revision: None,
            })
        }
        /// 本回归不提交设置。
        async fn save_settings(
            &self,
            _: SettingsSnapshot,
            _: Option<ConfigRevision>,
        ) -> AppResult<VersionedSettings> {
            Err(AppError::unsupported("test does not save settings"))
        }
    }

    #[async_trait]
    impl ConfigurationRepository for Repository {
        /// 将隔离 fixture 视为可写配置。
        fn recovery_state(&self) -> ConfigurationRecoveryState {
            ConfigurationRecoveryState {
                mode: ConfigurationMode::ReadWrite,
                issues: Vec::new(),
            }
        }
        /// 此测试不使用原始配置文档。
        async fn read(&self, _: ConfigDocumentPath) -> AppResult<Option<ConfigDocument>> {
            Err(AppError::unsupported("test does not read documents"))
        }
        /// 此测试不写入原始配置文档。
        async fn write(
            &self,
            _: ConfigDocumentPath,
            _: String,
            _: Option<ConfigRevision>,
        ) -> AppResult<ConfigRevision> {
            Err(AppError::unsupported("test does not write documents"))
        }
    }

    #[async_trait]
    impl ProfileRepository for Repository {
        /// 两个读取入口共享同一组带 revision 的隔离 Profile。
        async fn list_profiles(&self) -> AppResult<Vec<TerminalProfile>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .map(|row| row.profile.clone())
                .collect())
        }
        /// watcher 与显式重读均从同一测试仓储获取列表。
        async fn list_versioned_profiles(&self) -> AppResult<Vec<VersionedProfile>> {
            Ok(self.0.lock().unwrap().clone())
        }
        /// 按 ID 返回同一快照而不读取用户文件。
        async fn get_profile(&self, id: &str) -> AppResult<Option<VersionedProfile>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .find(|row| row.profile.id == id)
                .cloned())
        }
        /// watcher 回归不发起保存。
        async fn save_profile(
            &self,
            _: ProfileDraft,
            _: Option<ConfigRevision>,
        ) -> AppResult<VersionedProfile> {
            Err(AppError::unsupported("test does not save profiles"))
        }
        /// watcher 回归不删除记录。
        async fn delete_profile(&self, _: &str, _: ConfigRevision) -> AppResult<bool> {
            Err(AppError::unsupported("test does not delete profiles"))
        }
        /// watcher 回归不修改默认项。
        async fn set_default_profile(
            &self,
            _: &str,
            _: Vec<ProfileRevision>,
        ) -> AppResult<VersionedProfile> {
            Err(AppError::unsupported("test does not change defaults"))
        }
    }

    struct DefaultShell;
    impl DefaultLocalShellProvider for DefaultShell {
        /// 固定虚构 shell，避免探测用户机器环境。
        fn resolve_default(&self) -> AppResult<LocalSessionTarget> {
            Ok(LocalSessionTarget {
                program: OsString::from("test-shell"),
                args: Vec::new(),
                working_directory: None,
                environment: BTreeMap::new(),
            })
        }
    }

    /// 为不同文件提供稳定 ID 与不透明 revision，其他字段仅用于验证表单保留。
    fn profile(id: &str, revision: &str, is_default: bool) -> VersionedProfile {
        VersionedProfile {
            profile: TerminalProfile {
                id: id.into(),
                name: format!("Profile {id}"),
                shell: "cmd.exe".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                is_default,
                sidebar_group_id: None,
                sort_order: 0,
                created_at: "test".into(),
                updated_at: "test".into(),
            },
            revision: ConfigRevision::from_sha256(revision.into()),
        }
    }

    /// 挂载生产设置面板，测试只手动交付 watcher 回执，不重写面板的状态转换逻辑。
    fn fixture(
        app: &mut TestAppContext,
    ) -> (
        Runtime,
        Arc<Repository>,
        Entity<PreferencesPanel>,
        &mut VisualTestContext,
    ) {
        app.update(crate::init);
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let repository = Arc::new(Repository(Mutex::new(vec![
            profile("a", "a1", true),
            profile("b", "b1", false),
        ])));
        let services = PreferencesPanelServices {
            preferences: PreferencesService::new(repository.clone()),
            profiles: ProfileService::new(repository.clone(), Arc::new(DefaultShell)),
            vault_keys: None,
            configuration_sync: None,
            configuration_changes: None,
            external_protocol: None,
            configuration_directory: None,
            mcp_runtime_available: false,
            mcp_runtime: None,
            executor: ApplicationTaskExecutor::new(runtime.handle().clone()),
        };
        let slot = Rc::new(RefCell::new(None));
        let mounted = slot.clone();
        let (_, visual) = app.add_window_view(move |window, cx| {
            let panel = cx.new(|cx| PreferencesPanel::new(services, window, cx));
            *mounted.borrow_mut() = Some(panel.clone());
            gpui_kit::base::Root::new(panel, window, cx)
        });
        let panel = slot.borrow_mut().take().unwrap();
        pump(&runtime, visual);
        visual.update(|window, app| {
            panel.update(app, |panel, cx| {
                panel.section = PreferencesSection::Profiles;
                cx.notify();
                let _ = window;
            })
        });
        pump(&runtime, visual);
        (runtime, repository, panel, visual)
    }

    /// 推进真实 GPUI 帧及异步仓储回执，确保断言读取已呈现状态。
    fn pump(runtime: &Runtime, visual: &mut VisualTestContext) {
        for _ in 0..4 {
            visual.run_until_parked();
            runtime.block_on(tokio::task::yield_now());
            visual.update(|window, app| window.draw(app).clear(app));
        }
    }

    /// watcher 回执期间允许继续输入；相同 revision 和其他 Profile 变更不得覆盖输入或丢失焦点。
    /// 同一 Profile 确有不同 revision 时才保留草稿并进入外部冲突。
    ///
    /// @author kongweiguang
    #[gpui_kit::test]
    fn profile_watcher_preserves_edits_for_echo_and_other_profiles(app: &mut TestAppContext) {
        let (runtime, _, panel, visual) = fixture(app);
        let focus = visual.update(|window, app| {
            panel.update(app, |panel, cx| {
                assert_eq!(panel.selected_profile_id.as_deref(), Some("a"));
                let focus = panel.profile_inputs.name.read(cx).focus_handle(cx);
                window.focus(&focus, cx);
                focus
            })
        });
        let generation = visual.update(|window, app| {
            panel.update(app, |panel, cx| {
                let generation = panel.prepare_external_profile_change().unwrap();
                assert!(!panel.loading_profiles);
                panel
                    .profile_inputs
                    .name
                    .update(cx, |input, cx| input.replace_all("Local edit", window, cx));
                generation
            })
        });
        pump(&runtime, visual);
        visual.update(|window, app| {
            panel.update(app, |panel, cx| {
                panel.finish_profile_revision_check(
                    Ok(vec![profile("a", "a1", true), profile("b", "b2", false)]),
                    generation,
                    window,
                    cx,
                );
                let draft = panel.profile_draft.as_ref().unwrap();
                assert_eq!(draft.status(), DraftStatus::Ready);
                assert_eq!(draft.value().name, "Local edit");
                assert!(draft.is_dirty());
                assert!(panel.profile_list_error.is_none());
                assert_eq!(
                    panel.profile_rows[1].revision,
                    ConfigRevision::from_sha256("b2".into())
                );
                assert_eq!(window.focused(cx), Some(focus.clone()));
            })
        });
        let generation = visual.update(|_, app| {
            panel.update(app, |panel, _| {
                panel.prepare_external_profile_change().unwrap()
            })
        });
        visual.update(|window, app| {
            panel.update(app, |panel, cx| {
                panel.finish_profile_revision_check(
                    Ok(vec![profile("a", "a2", true), profile("b", "b2", false)]),
                    generation,
                    window,
                    cx,
                );
                let draft = panel.profile_draft.as_ref().unwrap();
                assert_eq!(draft.status(), DraftStatus::ExternalConflict);
                assert_eq!(draft.value().name, "Local edit");
                assert!(panel.profile_list_error.is_some());
                assert_eq!(window.focused(cx), Some(focus.clone()));
            })
        });
    }

    /// 未保存新建草稿没有已选 revision，watcher 更新列表时不得选择首项并丢弃草稿。
    ///
    /// @author kongweiguang
    #[gpui_kit::test]
    fn profile_watcher_preserves_new_unsaved_draft(app: &mut TestAppContext) {
        let (runtime, _, panel, visual) = fixture(app);
        let (id, generation) = visual.update(|window, app| {
            panel.update(app, |panel, cx| {
                panel.create_profile_in_group(None, window, cx);
                panel
                    .profile_inputs
                    .name
                    .update(cx, |input, cx| input.replace_all("New draft", window, cx));
                let id = panel.profile_draft.as_ref().unwrap().value().id.clone();
                let generation = panel.prepare_external_profile_change().unwrap();
                assert!(!panel.loading_profiles);
                (id, generation)
            })
        });
        pump(&runtime, visual);
        visual.update(|window, app| {
            panel.update(app, |panel, cx| {
                panel.finish_profile_revision_check(
                    Ok(vec![profile("a", "a2", true)]),
                    generation,
                    window,
                    cx,
                );
                let draft = panel.profile_draft.as_ref().unwrap();
                assert_eq!(draft.value().id, id);
                assert_eq!(draft.value().name, "New draft");
                assert_eq!(draft.status(), DraftStatus::Ready);
                assert!(panel.selected_profile_id.is_none());
            })
        });
    }

    /// 显式重载期间发生的监听变更会在重载完成后再读一次，旧监听回执不能取代显式结果。
    ///
    /// @author kongweiguang
    #[gpui_kit::test]
    fn profile_watcher_rechecks_after_overlapping_explicit_reload(app: &mut TestAppContext) {
        let (runtime, repository, panel, visual) = fixture(app);
        let (watch, reload) = visual.update(|_, app| {
            panel.update(app, |panel, _| {
                let watch = panel.prepare_external_profile_change().unwrap();
                let reload = panel.begin_profile_load();
                assert!(panel.loading_profiles && panel.profiles_watch_pending);
                (watch, reload)
            })
        });
        *repository.0.lock().unwrap() = vec![profile("a", "a2", true)];
        visual.update(|window, app| {
            panel.update(app, |panel, cx| {
                panel.finish_profile_revision_check(
                    Ok(vec![profile("a", "stale", true)]),
                    watch,
                    window,
                    cx,
                );
                assert!(panel.loading_profiles);
                panel.finish_profile_load(Ok(vec![profile("a", "a1", true)]), reload, window, cx);
                assert!(!panel.loading_profiles);
                assert!(panel.profiles_watch_in_flight);
            })
        });
        pump(&runtime, visual);
        visual.update(|_, app| {
            let panel = panel.read(app);
            assert_eq!(
                panel.profile_rows[0].revision,
                ConfigRevision::from_sha256("a2".into())
            );
            assert!(!panel.profiles_watch_in_flight);
        });
    }
}
