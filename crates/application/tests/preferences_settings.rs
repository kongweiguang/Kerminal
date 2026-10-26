//! 设置用例只处理 typed 投影、业务校验和仓储 CAS 结果。
//!
//! @author kongweiguang

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    preferences::{
        ConfigurationGuideStatus, ConfigurationRepository, ConfigurationValidationDiagnostic,
        ConfigurationValidationReport, ConfigurationValidationScope,
        ConfigurationValidationSeverity, KeybindingScope, PreferencesService, SettingsRepository,
        SettingsSnapshot, ThemeMode, ToolRailPanelPlacement, ToolRailSettings, ToolRailToolId,
        VersionedSettings,
    },
};
use kerminal_domain::preferences::{
    ConfigDocument, ConfigDocumentPath, ConfigRevision, ConfigurationMode,
    ConfigurationRecoveryState,
};

/// 错误与警告通过同一诊断记录入口保留定位字段并维护各自计数。
///
/// @author kongweiguang
#[test]
fn validation_report_records_typed_error_and_warning_fields() {
    let mut report = ConfigurationValidationReport::new(ConfigurationValidationScope::All);
    let error = ConfigurationValidationDiagnostic {
        severity: ConfigurationValidationSeverity::Error,
        scope: ConfigurationValidationScope::Settings,
        path: "settings.toml".to_owned(),
        message: "unsupported schema".to_owned(),
        line: Some(4),
        column: Some(2),
        key: Some("schema_version".to_owned()),
        recovery: Some("restore a supported settings file".to_owned()),
    };
    let warning = ConfigurationValidationDiagnostic {
        severity: ConfigurationValidationSeverity::Warning,
        scope: ConfigurationValidationScope::Profiles,
        path: "profiles/work.toml".to_owned(),
        message: "optional setting uses a default".to_owned(),
        line: None,
        column: None,
        key: Some("shell".to_owned()),
        recovery: Some("set shell to an installed executable".to_owned()),
    };

    report.record(error.clone());
    report.record(warning.clone());

    assert!(!report.valid);
    assert_eq!(report.error_count, 1);
    assert_eq!(report.warning_count, 1);
    assert_eq!(report.diagnostics, vec![error, warning]);
}

/// Files 与 Containers 固定使用右栏，但读取旧设置不得改写存量停靠值。
///
/// @author kongweiguang
#[test]
fn effective_tool_rail_placement_keeps_legacy_fixed_tool_values() {
    let mut settings = ToolRailSettings::default();
    settings
        .panel_placements
        .insert(ToolRailToolId::Sftp, ToolRailPanelPlacement::Center);
    settings
        .panel_placements
        .insert(ToolRailToolId::Containers, ToolRailPanelPlacement::Left);
    settings
        .panel_placements
        .insert(ToolRailToolId::Logs, ToolRailPanelPlacement::Bottom);
    let stored_placements = settings.panel_placements.clone();

    assert_eq!(
        settings.effective_panel_placement(ToolRailToolId::Sftp),
        ToolRailPanelPlacement::Attached
    );
    assert_eq!(
        settings.effective_panel_placement(ToolRailToolId::Containers),
        ToolRailPanelPlacement::Attached
    );
    assert_eq!(
        settings.effective_panel_placement(ToolRailToolId::Logs),
        ToolRailPanelPlacement::Bottom
    );
    assert_eq!(
        settings.effective_panel_placement(ToolRailToolId::Snippets),
        ToolRailPanelPlacement::Attached
    );
    assert_eq!(settings.panel_placements, stored_placements);
    assert_eq!(
        settings.panel_placements.get(&ToolRailToolId::Sftp),
        Some(&ToolRailPanelPlacement::Center)
    );
    assert_eq!(
        settings.panel_placements.get(&ToolRailToolId::Containers),
        Some(&ToolRailPanelPlacement::Left)
    );
}

/// 默认入口将容器放在文件之后；旧布局校验会补齐缺项并保留原有顺序。
///
/// @author kongweiguang
#[test]
fn containers_default_order_and_legacy_completion_are_stable() -> AppResult<()> {
    let default = ToolRailSettings::default();
    let files_position = default
        .order
        .iter()
        .position(|tool| *tool == ToolRailToolId::Sftp)
        .expect("Files tool exists by default");
    let containers_position = default
        .order
        .iter()
        .position(|tool| *tool == ToolRailToolId::Containers)
        .expect("Containers tool exists by default");
    assert_eq!(containers_position, files_position + 1);
    assert_eq!(
        serde_json::to_string(&ToolRailToolId::Containers).expect("container tool serializes"),
        "\"containers\""
    );
    assert_eq!(
        serde_json::from_str::<ToolRailToolId>("\"containers\"")
            .expect("container tool deserializes"),
        ToolRailToolId::Containers
    );

    let mut legacy = SettingsSnapshot::default();
    legacy
        .tool_rail
        .order
        .retain(|tool| *tool != ToolRailToolId::Containers);
    legacy
        .tool_rail
        .panel_placements
        .remove(&ToolRailToolId::Containers);
    let mut expected_order = legacy.tool_rail.order.clone();
    expected_order.push(ToolRailToolId::Containers);

    let validated = legacy.validated()?;
    assert_eq!(validated.tool_rail.order, expected_order);
    assert_eq!(
        validated
            .tool_rail
            .effective_panel_placement(ToolRailToolId::Containers),
        ToolRailPanelPlacement::Attached
    );
    Ok(())
}

#[derive(Clone)]
struct SettingsState {
    settings: SettingsSnapshot,
    revision: Option<ConfigRevision>,
    next_revision: u64,
    saves: usize,
}

#[derive(Clone)]
struct MemoryConfigurationRepository {
    state: Arc<Mutex<SettingsState>>,
}

/// 快速打开、退出和终端搜索默认键位保留正确作用范围及平台修饰键。
///
/// @author kongweiguang
#[test]
fn defaults_include_quick_open_and_command_palette_bindings() {
    let settings = SettingsSnapshot::default();
    let quick_open = settings
        .keybindings
        .iter()
        .find(|binding| binding.action == "workspace.quickOpen")
        .expect("Quick Open default exists");
    let command_palette = settings
        .keybindings
        .iter()
        .find(|binding| binding.action == "workspace.commandPalette")
        .expect("Command Palette default exists");
    let terminal_search = settings
        .keybindings
        .iter()
        .find(|binding| binding.action == "terminal.search")
        .expect("Terminal Search default exists");
    let application_quit = settings
        .keybindings
        .iter()
        .find(|binding| binding.action == "application.quit")
        .expect("application quit default exists");

    assert_eq!(quick_open.scope, KeybindingScope::Workspace);
    assert_eq!(quick_open.windows_binding, "Ctrl+P");
    assert_eq!(quick_open.mac_binding, "Cmd+P");
    assert_eq!(command_palette.scope, KeybindingScope::Workspace);
    assert_eq!(command_palette.windows_binding, "Ctrl+Shift+P");
    assert_eq!(command_palette.mac_binding, "Cmd+Shift+P");
    assert_eq!(terminal_search.scope, KeybindingScope::Terminal);
    assert_eq!(terminal_search.windows_binding, "Ctrl+F");
    assert_eq!(terminal_search.mac_binding, "Cmd+F");
    assert_eq!(application_quit.scope, KeybindingScope::Global);
    assert_eq!(application_quit.windows_binding, "Ctrl+Shift+Q");
    assert_eq!(application_quit.mac_binding, "Cmd+Q");

    let terminal_paste = settings
        .keybindings
        .iter()
        .find(|binding| binding.action == "terminal.paste")
        .expect("terminal paste default exists");
    assert_eq!(terminal_paste.binding, "");
    assert_eq!(terminal_paste.windows_binding, "Ctrl+V");
    assert_eq!(
        terminal_paste.windows_alternate_binding.as_deref(),
        Some("Ctrl+Shift+V")
    );
    assert_eq!(
        terminal_paste.linux_binding.as_deref(),
        Some("Ctrl+Shift+V")
    );
    assert_eq!(terminal_paste.mac_binding, "Cmd+V");
    assert_eq!(terminal_paste.scope, KeybindingScope::Terminal);
}

/// 正常校验保留缺失动作和显式空列表，避免把用户移除动作误当成旧数据。
///
/// @author kongweiguang
#[test]
fn validated_settings_preserves_missing_actions_and_empty_keybindings() {
    let mut settings = SettingsSnapshot::default();
    settings.keybindings.retain(|binding| {
        !matches!(
            binding.action.as_str(),
            "application.quit" | "terminal.paste"
        )
    });
    let validated = settings.validated().expect("partial settings validate");
    assert!(validated.keybindings.iter().all(|binding| !matches!(
        binding.action.as_str(),
        "application.quit" | "terminal.paste"
    )));

    let mut intentionally_empty = SettingsSnapshot::default();
    intentionally_empty.keybindings.clear();
    assert!(
        intentionally_empty
            .validated()
            .expect("empty keybindings remain valid")
            .keybindings
            .is_empty()
    );
}

/// 用户已显式配置的 paste 行优先于升级默认值，包括逐平台禁用状态。
///
/// @author kongweiguang
#[test]
fn validated_settings_preserves_explicitly_disabled_paste_bindings() {
    let mut settings = SettingsSnapshot::default();
    let paste = settings
        .keybindings
        .iter_mut()
        .find(|binding| binding.action == "terminal.paste")
        .expect("paste default exists");
    paste.binding.clear();
    paste.windows_binding.clear();
    paste.windows_alternate_binding = Some(String::new());
    paste.linux_binding = Some(String::new());
    paste.mac_binding.clear();

    let validated = settings.validated().expect("explicit disable validates");
    let paste = validated
        .keybindings
        .iter()
        .find(|binding| binding.action == "terminal.paste")
        .expect("explicit paste row remains");

    assert!(paste.binding.is_empty());
    assert!(paste.windows_binding.is_empty());
    assert_eq!(paste.windows_alternate_binding.as_deref(), Some(""));
    assert_eq!(paste.linux_binding.as_deref(), Some(""));
    assert!(paste.mac_binding.is_empty());
}

/// 新字段仍遵守既有输入长度上限，防止设置投影保存无法解析的超长绑定。
///
/// @author kongweiguang
#[test]
fn optional_platform_bindings_obey_the_existing_length_limit() {
    let mut settings = SettingsSnapshot::default();
    let paste = settings
        .keybindings
        .iter_mut()
        .find(|binding| binding.action == "terminal.paste")
        .expect("paste default exists");
    paste.linux_binding = Some("x".repeat(129));
    assert!(settings.validated().is_err());

    let mut settings = SettingsSnapshot::default();
    let paste = settings
        .keybindings
        .iter_mut()
        .find(|binding| binding.action == "terminal.paste")
        .expect("paste default exists");
    paste.windows_alternate_binding = Some("x".repeat(129));
    assert!(settings.validated().is_err());
}

/// 布局尺寸只钳制用户持久意图，不将窗口响应式尺寸带入设置快照。
///
/// @author kongweiguang
#[test]
fn validated_workbench_layout_bounds_logical_content_dimensions() {
    let mut settings = SettingsSnapshot::default();
    settings.workbench_layout.host_sidebar_width = 1;
    settings
        .workbench_layout
        .dock_panel_widths
        .insert(ToolRailToolId::Sftp, u16::MAX);
    settings
        .workbench_layout
        .dock_panel_widths
        .insert(ToolRailToolId::Containers, 1);
    settings.workbench_layout.bottom_panel_height = 0;

    let validated = settings.validated().expect("布局偏好保持可用");
    assert_eq!(validated.workbench_layout.host_sidebar_width, 168);
    assert_eq!(
        validated.workbench_layout.dock_panel_widths[&ToolRailToolId::Sftp],
        1_360
    );
    assert_eq!(
        validated.workbench_layout.dock_panel_widths[&ToolRailToolId::Containers],
        130
    );
    assert_eq!(validated.workbench_layout.bottom_panel_height, 180);
    assert!(!validated.workbench_layout.host_sidebar_collapsed);
}

impl MemoryConfigurationRepository {
    /// 初始化纯 typed 内存端口，避免应用测试维护第二份 TOML 编解码器。
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(SettingsState {
                settings: SettingsSnapshot::default(),
                revision: None,
                next_revision: 1,
                saves: 0,
            })),
        }
    }

    /// 模拟外部应用已经提交新 revision，供用例验证 stale CAS 不会覆盖它。
    fn replace_externally(&self, settings: SettingsSnapshot) {
        let mut state = self.state.lock().expect("settings state is not poisoned");
        state.settings = settings;
        state.revision = Some(ConfigRevision::from_sha256("external-revision".to_owned()));
    }
}

#[async_trait]
impl SettingsRepository for MemoryConfigurationRepository {
    /// 返回当前内存投影及其 revision，不通过序列化模拟磁盘读写。
    async fn load_settings(&self) -> AppResult<VersionedSettings> {
        let state = self.state.lock().expect("settings state is not poisoned");
        Ok(VersionedSettings {
            settings: state.settings.clone(),
            revision: state.revision.clone(),
        })
    }

    /// 仅在 revision 匹配时替换内存快照，使应用层保留仓储冲突语义。
    async fn save_settings(
        &self,
        settings: SettingsSnapshot,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedSettings> {
        let mut state = self.state.lock().expect("settings state is not poisoned");
        if state.revision != expected_revision {
            return Err(AppError::conflict("测试设置 revision 冲突"));
        }
        let revision =
            ConfigRevision::from_sha256(format!("typed-settings-{}", state.next_revision));
        state.next_revision += 1;
        state.saves += 1;
        state.settings = settings.clone();
        state.revision = Some(revision.clone());
        Ok(VersionedSettings {
            settings,
            revision: Some(revision),
        })
    }
}

#[async_trait]
impl ConfigurationRepository for MemoryConfigurationRepository {
    /// 测试配置恒为可写状态，隔离启动恢复逻辑以专测应用用例。
    fn recovery_state(&self) -> ConfigurationRecoveryState {
        ConfigurationRecoveryState {
            mode: ConfigurationMode::ReadWrite,
            issues: Vec::new(),
        }
    }

    /// 设置专用 fake 不承接原文编辑，避免测试重新引入 TOML parsing 路径。
    async fn read(&self, _path: ConfigDocumentPath) -> AppResult<Option<ConfigDocument>> {
        Err(AppError::unsupported("设置用例测试只允许 typed 仓储访问"))
    }

    /// 设置专用 fake 不模拟原始文件写入，CAS 由 typed 端口直接验证。
    async fn write(
        &self,
        _path: ConfigDocumentPath,
        _source: String,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ConfigRevision> {
        Err(AppError::unsupported("设置用例测试只允许 typed 仓储访问"))
    }

    /// 测试仓储不播种应用指南，沿用当前版本状态。
    fn configuration_guide_status(&self) -> ConfigurationGuideStatus {
        ConfigurationGuideStatus::Current
    }
}

/// 成功保存只在应用验证后转发 typed 快照，并返回仓储发出的新 revision。
#[tokio::test]
async fn settings_use_case_saves_typed_projection_with_revision() {
    let repository = MemoryConfigurationRepository::new();
    let service = PreferencesService::new(Arc::new(repository.clone()));
    let initial = service
        .load_settings()
        .await
        .expect("typed default settings load");
    let mut changed = initial.settings;
    changed.theme_mode = ThemeMode::Light;

    let saved = service
        .save_settings(changed.clone(), initial.revision)
        .await
        .expect("typed settings save succeeds");
    let reloaded = service
        .load_settings()
        .await
        .expect("typed settings reload succeeds");

    assert_eq!(saved.settings, changed);
    assert_eq!(reloaded.settings, changed);
    assert!(saved.revision.is_some());
}

/// 非法快照在 application 边界拒绝，仓储写入次数与当前值均保持不变。
#[tokio::test]
async fn invalid_settings_are_rejected_before_repository_write() {
    let repository = MemoryConfigurationRepository::new();
    let service = PreferencesService::new(Arc::new(repository.clone()));
    let loaded = service
        .load_settings()
        .await
        .expect("typed default settings load");
    let mut invalid = loaded.settings.clone();
    invalid.terminal.font_size = 9;

    assert!(
        service
            .save_settings(invalid, loaded.revision)
            .await
            .is_err()
    );
    assert_eq!(
        repository
            .state
            .lock()
            .expect("settings state is not poisoned")
            .saves,
        0
    );
}

/// 外部 revision 替換後保存返回衝突，原快照不被舊頁面內容覆蓋。
#[tokio::test]
async fn stale_settings_revision_is_returned_without_overwriting_external_change() {
    let repository = MemoryConfigurationRepository::new();
    let service = PreferencesService::new(Arc::new(repository.clone()));
    let loaded = service
        .load_settings()
        .await
        .expect("typed default settings load");
    let mut external = loaded.settings.clone();
    external.theme_mode = ThemeMode::Light;
    repository.replace_externally(external.clone());

    service
        .save_settings(loaded.settings, loaded.revision)
        .await
        .expect_err("stale settings revision must conflict");
    assert_eq!(
        service
            .load_settings()
            .await
            .expect("external typed value remains readable")
            .settings,
        external
    );
}
