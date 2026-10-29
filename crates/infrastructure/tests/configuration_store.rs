//! 文件化配置恢复、CAS 与默认 profile 的隔离契约测试。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString, fs, path::PathBuf, sync::Arc};

use kerminal_application::{
    AppError, PreferencesService, PublicErrorCode,
    preferences::{
        AppColorScheme, BackgroundFit, CONFIG_GUIDE, CONFIG_GUIDE_REVISION,
        ConfigurationGuideStatus, ConfigurationRepository, ConfigurationValidationRepository,
        ConfigurationValidationScope, ConfigurationValidationService,
        ConfigurationValidationSeverity, DefaultLocalShellProvider, InterfaceLanguage,
        ProfileRepository, ProfileService, SettingsSnapshot, ThemeMode, ToolRailToolId,
    },
};
use kerminal_domain::{
    preferences::{ConfigDocumentPath, ConfigurationMode},
    sessions::LocalSessionTarget,
};
use kerminal_infrastructure::configuration::ConfigurationStore;
use sha2::Digest;

const LEGACY_SETTINGS: &str =
    include_str!("../../../tests/fixtures/configuration/settings-v0.3.45.toml");
const CONFIG_GUIDE_V2: &str =
    include_str!("../../../tests/fixtures/configuration/config-guide-v2.md");

struct TemporaryConfigRoot(PathBuf);

impl TemporaryConfigRoot {
    /// 每项契约测试使用独立临时根，避免触碰当前用户的 `~/.kerminal`。
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("kerminal-config-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).expect("创建隔离配置目录");
        Self(root)
    }
}

impl Drop for TemporaryConfigRoot {
    /// 测试退出后清除自身临时目录，不依赖仓库外的真实用户数据。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 从应用共享指南中提取 TOML fenced example，避免测试维护第二份规则样本。
fn guide_toml_example(name: &str) -> String {
    let marker = format!("<!-- example:{name} -->");
    let mut lines = CONFIG_GUIDE
        .split_once(&marker)
        .unwrap_or_else(|| panic!("配置指南缺少 {name} 示例"))
        .1
        .lines();
    let fence = lines
        .find(|line| !line.trim().is_empty())
        .expect("示例标记后存在 TOML code fence");
    assert_eq!(fence.trim(), "```toml");
    let mut contents = Vec::new();
    for line in lines {
        if line.trim() == "```" {
            return contents.join("\n") + "\n";
        }
        contents.push(line);
    }
    panic!("配置指南中的 {name} 示例未闭合");
}

#[tokio::test]
/// 首次启动不播种默认文件，保存和读取保留注释、未知字段及原始字节 revision。
async fn first_run_preserves_raw_configuration_without_seeding() {
    let temp = TemporaryConfigRoot::new();
    let store = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");
    assert_eq!(store.recovery_state().mode, ConfigurationMode::ReadWrite);
    assert!(!temp.0.join("settings.toml").exists());
    let guide = fs::read_to_string(temp.0.join("kerminal-config.md")).unwrap();
    assert!(guide.starts_with(&format!(
        "<!-- kerminal-generated-config-guide revision={CONFIG_GUIDE_REVISION} sha256="
    )));
    assert!(guide.ends_with(CONFIG_GUIDE));
    assert_eq!(
        store.configuration_guide_status(),
        ConfigurationGuideStatus::Created
    );

    let source = "schema_version = 1\n# caller-owned note\nunknown_extension = \"kept\"\n";
    store
        .write(ConfigDocumentPath::Settings, source.to_owned(), None)
        .await
        .expect("首次创建设置文件");
    let read = store
        .read(ConfigDocumentPath::Settings)
        .await
        .expect("读取设置")
        .expect("设置存在");

    assert_eq!(read.source, source);
    assert_eq!(
        fs::read(temp.0.join("settings.toml")).unwrap(),
        source.as_bytes()
    );
}

#[test]
/// 普通启动保留旧指南原文；历史正文只由显式迁移入口识别和更新。
///
/// @author kongweiguang
fn configuration_guide_runtime_preserves_old_custom_and_future_content() {
    let old = TemporaryConfigRoot::new();
    fs::write(old.0.join("kerminal-config.md"), CONFIG_GUIDE_V2).expect("写入上一版无标记生成指南");
    let old_store = ConfigurationStore::initialize(&old.0).expect("初始化旧指南配置根");
    assert_eq!(
        old_store.configuration_guide_status(),
        ConfigurationGuideStatus::UserModified
    );
    assert_eq!(
        fs::read(old.0.join("kerminal-config.md")).expect("读取保留的旧指南"),
        CONFIG_GUIDE_V2.as_bytes()
    );

    let custom = TemporaryConfigRoot::new();
    let custom_contents = "# User-owned Kerminal notes\nkeep this file\n";
    fs::write(custom.0.join("kerminal-config.md"), custom_contents).expect("写入用户自定义指南");
    let custom_store = ConfigurationStore::initialize(&custom.0).expect("保留用户指南");
    assert_eq!(
        custom_store.configuration_guide_status(),
        ConfigurationGuideStatus::UserModified
    );
    assert_eq!(
        fs::read_to_string(custom.0.join("kerminal-config.md")).unwrap(),
        custom_contents
    );

    let future = TemporaryConfigRoot::new();
    let future_body = "# Guide from a future application\n";
    let digest = sha2::Sha256::digest(future_body.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let future_contents = format!(
        "<!-- kerminal-generated-config-guide revision={} sha256={digest} -->\n{future_body}",
        CONFIG_GUIDE_REVISION + 1
    );
    fs::write(future.0.join("kerminal-config.md"), &future_contents).expect("写入未来版本指南");
    let future_store = ConfigurationStore::initialize(&future.0).expect("保护未来指南");
    assert_eq!(
        future_store.configuration_guide_status(),
        ConfigurationGuideStatus::FutureRevision {
            revision: CONFIG_GUIDE_REVISION + 1
        }
    );
    assert_eq!(
        fs::read_to_string(future.0.join("kerminal-config.md")).unwrap(),
        future_contents
    );
}

#[tokio::test]
/// 通过稳定公开错误码确认损坏配置的直接和 typed 写入口被拒绝，原始文档字节保持不变。
///
/// @author kongweiguang
async fn corrupt_configuration_enters_read_only_recovery() {
    let temp = TemporaryConfigRoot::new();
    let original = "schema_version = 99\nprivate_extension = \"untouched\"\n";
    fs::write(temp.0.join("settings.toml"), original).expect("写入合成的未来 schema fixture");
    let store = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");

    let recovery = store.recovery_state();
    assert_eq!(recovery.mode, ConfigurationMode::ReadOnlyRecovery);
    assert_eq!(recovery.issues[0].relative_path, "settings.toml");
    assert!(matches!(
        store
            .write(
                ConfigDocumentPath::Settings,
                "schema_version = 1\n".to_owned(),
                None,
            )
            .await,
        Err(error) if error.public_code() == PublicErrorCode::ConfigurationCorrupt
    ));
    let service = PreferencesService::new(Arc::new(store.clone()));
    assert!(matches!(
        service.load_settings().await,
        Err(error) if error.public_code() == PublicErrorCode::ConfigurationCorrupt
    ));
    assert!(matches!(
        service
            .save_settings(SettingsSnapshot::default(), None)
            .await,
        Err(error) if error.public_code() == PublicErrorCode::ConfigurationCorrupt
    ));
    assert_eq!(
        fs::read(temp.0.join("settings.toml")).unwrap(),
        original.as_bytes()
    );
}

#[tokio::test]
/// CAS revision 过期时拒绝覆盖外部编辑器写入的内容。
async fn stale_configuration_revision_cannot_overwrite_external_edit() {
    let temp = TemporaryConfigRoot::new();
    let store = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");
    let original = "schema_version = 1\nmarker = \"original\"\n";
    store
        .write(ConfigDocumentPath::Settings, original.to_owned(), None)
        .await
        .expect("创建设置");
    let opened = store
        .read(ConfigDocumentPath::Settings)
        .await
        .expect("读取设置")
        .expect("设置存在");
    let external = "schema_version = 1\nmarker = \"external\"\n";
    fs::write(temp.0.join("settings.toml"), external).expect("模拟外部编辑器保存");

    let error = store
        .write(
            ConfigDocumentPath::Settings,
            "schema_version = 1\nmarker = \"stale\"\n".to_owned(),
            Some(opened.revision),
        )
        .await
        .expect_err("过期 revision 必须冲突");
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(
        fs::read_to_string(temp.0.join("settings.toml")).unwrap(),
        external
    );
}

#[tokio::test]
/// 默认 profile 由应用用例解析，工作目录、参数和环境覆盖都从文件配置保留。
async fn profile_service_resolves_configured_default_target() {
    let temp = TemporaryConfigRoot::new();
    let profiles = temp.0.join("profiles");
    fs::create_dir_all(&profiles).expect("创建 profiles fixture 目录");
    fs::write(
        profiles.join("default.toml"),
        concat!(
            "schema_version = 1\n",
            "id = \"default\"\n",
            "name = \"Configured shell\"\n",
            "shell = \"fixture-shell\"\n",
            "args = [\"--login\"]\n",
            "cwd = \"C:/fixture\"\n",
            "env = { FIXTURE_MODE = \"preserved\" }\n",
            "is_default = true\n",
            "sort_order = 10\n",
            "created_at = \"1\"\n",
            "updated_at = \"1\"\n",
            "unknown_extension = \"preserved on disk\"\n",
        ),
    )
    .expect("写入合成 profile fixture");
    let store = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");
    let repository: Arc<dyn ProfileRepository> = Arc::new(store);
    let shell = Arc::new(FixtureShellProvider);
    let service = ProfileService::new(repository, shell);

    let target = service
        .default_local_session_target()
        .await
        .expect("解析配置默认 shell");
    assert_eq!(target.program, OsString::from("fixture-shell"));
    assert_eq!(target.args, vec![OsString::from("--login")]);
    assert_eq!(target.working_directory, Some(PathBuf::from("C:/fixture")));
    assert_eq!(
        target.environment,
        BTreeMap::from([(OsString::from("FIXTURE_MODE"), OsString::from("preserved"))])
    );
}

#[tokio::test]
/// 新建本地主机预分配稳定 ID，默认 Shell 未修改也应持久化；None revision 绝不能覆盖同名 ID。
/// @author kongweiguang
async fn local_host_preassigned_id_creates_and_survives_reopen_without_clobbering() {
    let temp = TemporaryConfigRoot::new();
    let store = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");
    let service = ProfileService::new(Arc::new(store), Arc::new(FixtureShellProvider));
    let default_target = service
        .default_local_session_target()
        .await
        .expect("解析本地默认目标");
    let draft = kerminal_application::preferences::ProfileDraft {
        id: Some("local-host-stable-id".to_owned()),
        name: "本地主机".to_owned(),
        shell: default_target.program.to_string_lossy().into_owned(),
        args: Vec::new(),
        cwd: None,
        env: BTreeMap::new(),
        sidebar_group_id: None,
        sort_order: 0,
    };
    let saved = service
        .save_profile(draft.clone(), None)
        .await
        .expect("默认参数的新建本地主机仍应写入");
    assert_eq!(saved.profile.id, "local-host-stable-id");
    drop(service);

    let reopened = ConfigurationStore::initialize(&temp.0).expect("重新打开隔离配置仓储");
    let service = ProfileService::new(Arc::new(reopened), Arc::new(FixtureShellProvider));
    let restored = service
        .get_profile("local-host-stable-id")
        .await
        .expect("重启后读回本地主机")
        .expect("持久本地主机存在");
    assert_eq!(restored.revision, saved.revision);
    assert_eq!(restored.profile.name, "本地主机");
    assert_eq!(restored.profile.shell, "fallback-shell");
    let failure = service
        .save_profile(draft, None)
        .await
        .expect_err("无 revision 的重复创建不得覆盖原主机");
    assert_eq!(failure.public_code(), PublicErrorCode::Conflict);
    let unchanged = service
        .get_profile("local-host-stable-id")
        .await
        .expect("冲突后读回原主机")
        .expect("原主机仍存在");
    assert_eq!(unchanged.revision, saved.revision);
}

#[test]
/// 合法 v1/v2 配置分布在所有文件目录时保持可写，保证扫描不会收紧已支持格式。
fn startup_accepts_supported_documents_across_configuration_tree() {
    let temp = TemporaryConfigRoot::new();
    for directory in ["profiles", "hosts", "snippets", "workflows"] {
        fs::create_dir_all(temp.0.join(directory)).expect("创建合成配置目录");
    }
    fs::write(temp.0.join("settings.toml"), "schema_version = 1\n").expect("写入 settings fixture");
    fs::write(
        temp.0.join("hosts/groups.toml"),
        "schema_version = 1\ngroups = []\n",
    )
    .expect("写入 group fixture");
    fs::write(
        temp.0.join("hosts/host-a.toml"),
        concat!(
            "schema_version = 3\n",
            "protocol = \"ssh\"\n",
            "id = \"host-a\"\n",
            "name = \"Host A\"\n",
            "host = \"host-a.internal\"\n",
            "port = 22\n",
            "username = \"deploy\"\n",
            "auth_type = \"agent\"\n",
            "production = true\n",
            "sort_order = 1\n",
            "created_at = \"1\"\n",
            "updated_at = \"1\"\n",
        ),
    )
    .expect("写入兼容 host fixture");
    fs::write(
        temp.0.join("snippets/inspect.toml"),
        concat!(
            "schema_version = 1\n",
            "id = \"inspect\"\n",
            "title = \"Inspect\"\n",
            "command = \"pwd\"\n",
            "scope = \"any\"\n",
            "sort_order = 1\n",
            "created_at = \"1\"\n",
            "updated_at = \"1\"\n",
        ),
    )
    .expect("写入 snippet fixture");
    fs::write(
        temp.0.join("workflows/deploy.toml"),
        concat!(
            "schema_version = 1\n",
            "id = \"deploy\"\n",
            "title = \"Deploy\"\n",
            "scope = \"ssh\"\n",
            "sort_order = 1\n",
            "created_at = \"1\"\n",
            "updated_at = \"1\"\n",
        ),
    )
    .expect("写入 workflow fixture");

    let store = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");
    assert_eq!(store.recovery_state().mode, ConfigurationMode::ReadWrite);
    assert!(store.recovery_state().issues.is_empty());
}

#[tokio::test]
/// 多目录 schema 损坏时按稳定错误码阻止 settings 写入，并逐字节核对损坏源文件未变。
///
/// @author kongweiguang
async fn startup_locates_invalid_configuration_documents_outside_settings_and_profiles() {
    let temp = TemporaryConfigRoot::new();
    for directory in ["hosts", "snippets", "workflows"] {
        fs::create_dir_all(temp.0.join(directory)).expect("创建合成配置目录");
    }
    fs::write(
        temp.0.join("hosts/groups.toml"),
        "schema_version = 1\ngroups = \"invalid\"\n",
    )
    .expect("写入损坏 groups fixture");
    fs::write(
        temp.0.join("hosts/legacy.toml"),
        "schema_version = 1\nid = \"legacy\"\n",
    )
    .expect("写入旧 host schema fixture");
    fs::write(
        temp.0.join("snippets/unsafe.toml"),
        concat!(
            "schema_version = 1\n",
            "id = \"unsafe\"\n",
            "title = \"Unsafe\"\n",
            "command = \"echo test\"\n",
            "scope = \"any\"\n",
            "sort_order = 1\n",
            "created_at = \"1\"\n",
            "updated_at = \"1\"\n",
            "[[variables]]\n",
            "name = \"token\"\n",
            "label = \"Token\"\n",
            "description = \"Sensitive value\"\n",
            "kind = \"secret\"\n",
            "required = true\n",
            "default_value = \"synthetic-only\"\n",
            "render_strategy = \"shellArg\"\n",
        ),
    )
    .expect("写入损坏 snippet fixture");
    fs::write(
        temp.0.join("workflows/unknown-scope.toml"),
        concat!(
            "schema_version = 1\n",
            "id = \"unknown-scope\"\n",
            "title = \"Unknown\"\n",
            "scope = \"everywhere\"\n",
            "sort_order = 1\n",
            "created_at = \"1\"\n",
            "updated_at = \"1\"\n",
        ),
    )
    .expect("写入损坏 workflow fixture");

    let original_documents = [
        PathBuf::from("hosts/groups.toml"),
        PathBuf::from("hosts/legacy.toml"),
        PathBuf::from("snippets/unsafe.toml"),
        PathBuf::from("workflows/unknown-scope.toml"),
    ]
    .into_iter()
    .map(|relative_path| {
        let contents = fs::read(temp.0.join(&relative_path)).expect("读取损坏源文件原始字节");
        (relative_path, contents)
    })
    .collect::<Vec<_>>();

    let store = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");
    let recovery = store.recovery_state();
    assert_eq!(recovery.mode, ConfigurationMode::ReadOnlyRecovery);
    assert!(matches!(
        store
            .write(
                ConfigDocumentPath::Settings,
                "schema_version = 1\n".to_owned(),
                None,
            )
            .await,
        Err(error) if error.public_code() == PublicErrorCode::ConfigurationCorrupt
    ));
    assert!(!temp.0.join("settings.toml").exists());
    for (relative_path, original_contents) in &original_documents {
        assert_eq!(
            fs::read(temp.0.join(relative_path)).expect("读取恢复态源文件"),
            *original_contents,
            "只读恢复不得改写 {}",
            relative_path.display()
        );
    }
    let paths = recovery
        .issues
        .iter()
        .map(|issue| issue.relative_path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            "hosts/groups.toml",
            "hosts/legacy.toml",
            "snippets/unsafe.toml",
            "workflows/unknown-scope.toml",
        ]
    );
    assert!(paths.iter().all(|path| !PathBuf::from(path).is_absolute()));
    assert!(
        paths
            .iter()
            .all(|path| !path.contains("kerminal-config-test-"))
    );
}

#[tokio::test]
/// 默认只读校验报告字段与 MCP 契约同形，且不会修改任何配置文件。
async fn configuration_validator_returns_read_only_report_with_stable_wire_shape() {
    let temp = TemporaryConfigRoot::new();
    fs::write(
        temp.0.join(".gitignore"),
        "secrets/vault-key.toml\nsecrets/vault-key.toml.bak.*\n",
    )
    .expect("写入隔离密钥忽略规则");
    let store = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");
    let repository: Arc<dyn ConfigurationValidationRepository> = Arc::new(store.clone());
    let service = ConfigurationValidationService::new(repository);

    let before = fs::read(temp.0.join(".gitignore")).unwrap();
    let report = service
        .validate(ConfigurationValidationScope::All)
        .await
        .expect("执行只读配置校验");
    let encoded = serde_json::to_value(&report).expect("序列化 validator 结果");

    assert!(report.valid);
    assert_eq!(report.error_count, 0);
    assert_eq!(report.warning_count, 0);
    assert!(report.diagnostics.is_empty());
    assert_eq!(encoded["scope"], "all");
    assert_eq!(encoded["errorCount"], 0);
    assert_eq!(encoded["warningCount"], 0);
    assert!(encoded["diagnostics"].is_array());
    assert_eq!(fs::read(temp.0.join(".gitignore")).unwrap(), before);
}

/// 离线入口可检查尚不存在的配置根，且不创建目录、存储锁或生成指南。
///
/// @author kongweiguang
#[test]
fn standalone_configuration_validator_does_not_initialize_missing_root() {
    let temp = TemporaryConfigRoot::new();
    let root = temp.0.join("not-initialized");

    let report = ConfigurationStore::validate_root(&root, ConfigurationValidationScope::All)
        .expect("缺省配置根可只读校验");

    assert!(report.valid);
    assert!(!root.exists());
}

/// 独立入口复用真实 schema validator，并保留非法 TOML 原文且不落锁或播种文件。
///
/// @author kongweiguang
#[test]
fn standalone_configuration_validator_reports_schema_errors_without_writes() {
    let temp = TemporaryConfigRoot::new();
    let root = temp.0.join("offline-validation");
    fs::create_dir_all(&root).expect("创建隔离校验根");
    let source = b"schema_version = 99\n";
    fs::write(root.join("settings.toml"), source).expect("写入未来 schema 设置");

    let report = ConfigurationStore::validate_root(&root, ConfigurationValidationScope::Settings)
        .expect("schema 问题作为诊断返回");

    assert!(!report.valid);
    assert_eq!(report.error_count, 1);
    assert_eq!(fs::read(root.join("settings.toml")).unwrap(), source);
    assert!(!root.join(".storage.lock").exists());
    assert!(!root.join("kerminal-config.md").exists());
}

/// 离线校验遇到待恢复 journal 时 fail closed，但不接管恢复或改动 journal。
///
/// @author kongweiguang
#[test]
fn standalone_configuration_validator_rejects_pending_transaction_without_recovery() {
    let temp = TemporaryConfigRoot::new();
    let root = temp.0.join("pending-transaction");
    let transaction_directory = root.join(".storage-transactions");
    fs::create_dir_all(&transaction_directory).expect("创建隔离事务目录");
    let journal = transaction_directory.join("profile-default.json");
    let source = br#"{"schema_version":1}"#;
    fs::write(&journal, source).expect("写入模拟待恢复 journal");

    let result = ConfigurationStore::validate_root(&root, ConfigurationValidationScope::All);

    assert!(result.is_err());
    assert_eq!(fs::read(&journal).unwrap(), source);
    assert!(!root.join(".storage.lock").exists());
    assert!(!root.join("kerminal-config.md").exists());
}

#[tokio::test]
/// 直接把唯一配置指南中的六类示例写入隔离根并经真实只读 validator 校验。
async fn configuration_guide_examples_pass_the_configuration_validator() {
    let temp = TemporaryConfigRoot::new();
    for directory in ["profiles", "hosts", "snippets", "workflows"] {
        fs::create_dir_all(temp.0.join(directory)).expect("创建示例目录");
    }
    fs::write(
        temp.0.join(".gitignore"),
        "secrets/vault-key.toml\nsecrets/vault-key.toml.bak.*\n",
    )
    .expect("加入本地密钥忽略规则");
    fs::write(temp.0.join("settings.toml"), guide_toml_example("settings")).expect("写入设置示例");
    fs::write(
        temp.0.join("profiles/work.toml"),
        guide_toml_example("profile"),
    )
    .expect("写入 profile 示例");
    fs::write(
        temp.0.join("hosts/groups.toml"),
        guide_toml_example("groups"),
    )
    .expect("写入分组示例");
    let host = guide_toml_example("host");
    let reference = host
        .lines()
        .find_map(|line| line.strip_prefix("secret_ref = \""))
        .and_then(|line| line.strip_suffix('\"'))
        .expect("Host 示例含凭据引用");
    let segments = reference.split(':').collect::<Vec<_>>();
    assert_eq!(
        segments,
        [
            "credential",
            "kerminal",
            "ssh",
            "build-host",
            "target",
            "password",
            "v1"
        ]
    );
    fs::write(temp.0.join("hosts/build-host.toml"), host).expect("写入主机示例");
    fs::write(
        temp.0.join("snippets/inspect-host.toml"),
        guide_toml_example("snippet"),
    )
    .expect("写入片段示例");
    fs::write(
        temp.0.join("workflows/inspect-release.toml"),
        guide_toml_example("workflow"),
    )
    .expect("写入工作流示例");

    let store = ConfigurationStore::initialize(&temp.0).expect("初始化示例配置仓储");
    let repository: Arc<dyn ConfigurationValidationRepository> = Arc::new(store);
    let report = ConfigurationValidationService::new(repository)
        .validate(ConfigurationValidationScope::All)
        .await
        .expect("校验指南中的全部示例");
    assert!(report.valid, "guide examples should validate: {report:?}");
    assert_eq!(report.error_count, 0);
    assert_eq!(report.warning_count, 0);
}

/// 先校验指南示例的嵌套与布局默认，再经过 typed settings 端口确认磁盘 DTO 的实际解码结果。
///
/// @author kongweiguang
#[tokio::test]
async fn settings_guide_example_decodes_appearance_and_workbench_layout_fields() {
    let mut example =
        toml::from_str::<toml::Value>(&guide_toml_example("settings")).expect("指南设置示例可解析");
    let root = example.as_table().expect("设置示例根为 TOML table");
    let workbench_layout = root
        .get("workbenchLayout")
        .and_then(toml::Value::as_table)
        .expect("工作台布局位于 workbenchLayout 表");
    assert_eq!(
        workbench_layout["hostSidebarCollapsed"].as_bool(),
        Some(false)
    );
    assert_eq!(workbench_layout["hostSidebarWidth"].as_integer(), Some(220));
    assert!(
        workbench_layout["dockPanelWidths"]
            .as_table()
            .expect("工具宽度是稀疏表")
            .is_empty()
    );
    assert_eq!(
        workbench_layout["bottomPanelHeight"].as_integer(),
        Some(240)
    );
    let appearance = root
        .get("appearance")
        .and_then(toml::Value::as_table)
        .expect("外观字段位于 appearance 表");
    for key in [
        "colorScheme",
        "interfaceLanguage",
        "backgroundEnabled",
        "backgroundFit",
        "backgroundImagePath",
        "backgroundOpacity",
        "windowOpacity",
    ] {
        assert!(appearance.contains_key(key), "appearance 缺少 {key}");
        assert!(!root.contains_key(key), "{key} 不应位于根表");
    }
    assert!(root.contains_key("keybindings"), "keybindings 必须保持顶层");
    assert!(root.contains_key("themeMode"));
    assert!(root.contains_key("interfaceDensity"));

    let appearance = example
        .as_table_mut()
        .and_then(|root| root.get_mut("appearance"))
        .and_then(toml::Value::as_table_mut)
        .expect("appearance 表可修改");
    appearance.insert(
        "colorScheme".to_owned(),
        toml::Value::String("jetbrains".to_owned()),
    );
    appearance.insert(
        "interfaceLanguage".to_owned(),
        toml::Value::String("zhCn".to_owned()),
    );
    appearance.insert("backgroundEnabled".to_owned(), toml::Value::Boolean(true));
    appearance.insert(
        "backgroundFit".to_owned(),
        toml::Value::String("contain".to_owned()),
    );
    appearance.insert(
        "backgroundImagePath".to_owned(),
        toml::Value::String("C:/Users/tester/Pictures/terminal.png".to_owned()),
    );
    appearance.insert("backgroundOpacity".to_owned(), toml::Value::Integer(67));
    appearance.insert("windowOpacity".to_owned(), toml::Value::Integer(82));

    let temp = TemporaryConfigRoot::new();
    let store = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");
    store
        .write(
            ConfigDocumentPath::Settings,
            toml::to_string(&example).expect("编码设置示例"),
            None,
        )
        .await
        .expect("真实仓储保存设置示例");
    let loaded = PreferencesService::new(Arc::new(store))
        .load_settings()
        .await
        .expect("typed settings port 读取设置示例");

    assert_eq!(loaded.settings.app_color_scheme, AppColorScheme::JetBrains);
    assert_eq!(loaded.settings.interface_language, InterfaceLanguage::ZhCn);
    assert!(loaded.settings.background_enabled);
    assert_eq!(loaded.settings.background_fit, BackgroundFit::Contain);
    assert_eq!(
        loaded.settings.background_image_path,
        "C:/Users/tester/Pictures/terminal.png"
    );
    assert_eq!(loaded.settings.background_opacity, 67);
    assert_eq!(loaded.settings.window_opacity, 82);
    assert!(!loaded.settings.workbench_layout.host_sidebar_collapsed);
    assert_eq!(loaded.settings.workbench_layout.host_sidebar_width, 220);
    assert!(
        loaded
            .settings
            .workbench_layout
            .dock_panel_widths
            .is_empty()
    );
    assert_eq!(loaded.settings.workbench_layout.bottom_panel_height, 240);
}

#[tokio::test]
/// validator 将未来 schema 返回为 invalid 诊断，并保持原配置字节不变。
///
/// @author kongweiguang
async fn configuration_validator_rejects_future_schema_without_rewriting_it() {
    let temp = TemporaryConfigRoot::new();
    let original = "schema_version = 99\nfuture_extension = \"preserve\"\n";
    fs::write(temp.0.join("settings.toml"), original).expect("写入未来 schema fixture");
    fs::write(
        temp.0.join(".gitignore"),
        "secrets/vault-key.toml\nsecrets/vault-key.toml.bak.*\n",
    )
    .expect("加入本地密钥忽略规则");
    let store = ConfigurationStore::initialize(&temp.0).expect("初始化未来 schema 仓储");
    let repository: Arc<dyn ConfigurationValidationRepository> = Arc::new(store);
    let report = ConfigurationValidationService::new(repository)
        .validate(ConfigurationValidationScope::Settings)
        .await
        .expect("未来 schema 是 validator 诊断而非 transport error");

    assert!(!report.valid);
    assert_eq!(report.error_count, 1);
    assert_eq!(report.warning_count, 0);
    assert_eq!(
        report.diagnostics[0].severity,
        ConfigurationValidationSeverity::Error
    );
    assert_eq!(report.diagnostics[0].path, "settings.toml");
    assert_eq!(
        fs::read_to_string(temp.0.join("settings.toml")).unwrap(),
        original
    );
}

/// 未知工具 ID 必须由严格 settings 解码拒绝，validator 只报告诊断且不改原文件。
///
/// @author kongweiguang
#[tokio::test]
async fn configuration_validator_rejects_unknown_workbench_tool_id_without_rewriting_it() {
    let temp = TemporaryConfigRoot::new();
    let original = concat!(
        "schema_version = 1\n",
        "[workbenchLayout]\n",
        "dockPanelWidths = { unknownTool = 320 }\n",
    );
    fs::write(temp.0.join("settings.toml"), original).expect("写入含未知 ToolId 的设置");
    fs::write(
        temp.0.join(".gitignore"),
        "secrets/vault-key.toml\nsecrets/vault-key.toml.bak.*\n",
    )
    .expect("加入隔离密钥忽略规则");
    let store = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");
    let repository: Arc<dyn ConfigurationValidationRepository> = Arc::new(store);
    let report = ConfigurationValidationService::new(repository)
        .validate(ConfigurationValidationScope::Settings)
        .await
        .expect("未知 ToolId 应作为 validator 诊断返回");

    assert!(!report.valid);
    assert_eq!(report.error_count, 1);
    assert_eq!(report.diagnostics[0].path, "settings.toml");
    assert_eq!(
        fs::read(temp.0.join("settings.toml")).expect("读取仍受保护的原件"),
        original.as_bytes()
    );
}

/// 未知应用色板必须走既有只读恢复与 validator，原始用户文件不能因回退而被覆盖。
///
/// @author kongweiguang
#[tokio::test]
async fn unknown_app_color_scheme_is_reported_without_rewriting_configuration() {
    let temp = TemporaryConfigRoot::new();
    let original = "schema_version = 1\n[appearance]\ncolorScheme = \"future\" # preserve\n";
    fs::write(temp.0.join("settings.toml"), original).expect("写入未知配色设置");
    let store = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置根");
    assert_eq!(
        store.recovery_state().mode,
        ConfigurationMode::ReadOnlyRecovery
    );
    let report = ConfigurationValidationService::new(Arc::new(store.clone()))
        .validate(ConfigurationValidationScope::Settings)
        .await
        .expect("返回未知配色诊断");
    assert!(!report.valid);
    assert!(
        report
            .diagnostics
            .iter()
            .any(|item| item.path == "settings.toml")
    );
    assert!(
        PreferencesService::new(Arc::new(store))
            .load_settings()
            .await
            .is_err()
    );
    assert_eq!(
        fs::read(temp.0.join("settings.toml")).unwrap(),
        original.as_bytes()
    );
}

#[tokio::test]
/// 跨文件引用错误作为脱敏诊断返回，validation 不会改写源 profile。
///
/// @author kongweiguang
async fn configuration_validator_reports_broken_profile_group_reference_without_writing() {
    let temp = TemporaryConfigRoot::new();
    let profiles = temp.0.join("profiles");
    let hosts = temp.0.join("hosts");
    fs::create_dir_all(&profiles).expect("创建 profiles fixture");
    fs::create_dir_all(&hosts).expect("创建 hosts fixture");
    fs::write(
        temp.0.join(".gitignore"),
        "secrets/vault-key.toml\nsecrets/vault-key.toml.bak.*\n",
    )
    .expect("写入隔离密钥忽略规则");
    fs::write(
        hosts.join("groups.toml"),
        "schema_version = 1\ngroups = []\n",
    )
    .expect("写入合成空分组列表");
    let profile = concat!(
        "schema_version = 1\n",
        "id = \"launch\"\n",
        "name = \"Launch\"\n",
        "shell = \"fixture-shell\"\n",
        "is_default = true\n",
        "sidebar_group_id = \"missing-group\"\n",
        "sort_order = 10\n",
        "created_at = \"1\"\n",
        "updated_at = \"1\"\n",
    );
    fs::write(profiles.join("launch.toml"), profile).expect("写入合成 profile");
    let store = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");
    let repository: Arc<dyn ConfigurationValidationRepository> = Arc::new(store);
    let service = ConfigurationValidationService::new(repository);

    let report = service
        .validate(ConfigurationValidationScope::Profiles)
        .await
        .expect("返回配置诊断报告");
    let after = fs::read(profiles.join("launch.toml")).unwrap();

    assert!(!report.valid);
    assert_eq!(report.error_count, 1);
    assert_eq!(report.warning_count, 0);
    assert_eq!(
        report.diagnostics[0].severity,
        ConfigurationValidationSeverity::Error
    );
    assert_eq!(report.diagnostics[0].path, "profiles/launch.toml");
    assert_eq!(
        report.diagnostics[0].key.as_deref(),
        Some("sidebar_group_id")
    );
    assert!(!report.diagnostics[0].message.contains("missing-group"));
    assert_eq!(after, profile.as_bytes());
}

/// 以当前 SFTP key 读取 v0.3.45 共享字段，并保留未知 TOML 元数据和快捷键行。
///
/// @author kongweiguang
#[tokio::test]
async fn typed_settings_roundtrip_preserves_supported_fields_and_unknown_fields() {
    let temp = TemporaryConfigRoot::new();
    let store = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");
    let current_settings =
        LEGACY_SETTINGS.replace("timeoutSeconds = 240", "idleTimeoutSeconds = 240");
    store
        .write(ConfigDocumentPath::Settings, current_settings, None)
        .await
        .expect("当前字段形状的 settings fixture 写入");
    let service = PreferencesService::new(Arc::new(store.clone()));

    let loaded = service
        .load_settings()
        .await
        .expect("旧设置经 typed port 读取");
    assert_eq!(
        loaded.settings.workbench_layout,
        kerminal_application::preferences::WorkbenchLayoutSettings::default()
    );
    assert_eq!(loaded.settings.interface_language, InterfaceLanguage::ZhCn);
    assert!(loaded.settings.background_enabled);
    assert_eq!(
        loaded.settings.background_image_path,
        "C:/Users/tester/Pictures/terminal.png"
    );
    assert_eq!(loaded.settings.window_opacity, 82);
    assert_eq!(loaded.settings.terminal.font_size, 16);
    assert_eq!(loaded.settings.terminal.scrollback, 12_000);
    assert!(loaded.settings.desktop_notifications.enabled);
    assert_eq!(loaded.settings.sftp.global_transfers, 6);
    assert_eq!(loaded.settings.sftp.idle_timeout_seconds, 240);
    assert!(
        loaded
            .settings
            .keybindings
            .iter()
            .any(|binding| binding.action == "terminal.newTab")
    );
    assert!(
        loaded
            .settings
            .keybindings
            .iter()
            .all(|binding| binding.action != "application.quit")
    );
    assert!(
        loaded
            .settings
            .keybindings
            .iter()
            .all(|binding| binding.action != "terminal.paste")
    );

    let mut changed = loaded.settings.clone();
    changed.theme_mode = ThemeMode::Light;
    changed.window_opacity = 91;
    changed.terminal.font_size = 18;
    changed.sftp.global_transfers = 8;
    changed.workbench_layout.host_sidebar_collapsed = true;
    changed.workbench_layout.host_sidebar_width = 304;
    changed
        .workbench_layout
        .dock_panel_widths
        .insert(ToolRailToolId::Sftp, 372);
    changed
        .workbench_layout
        .dock_panel_widths
        .insert(ToolRailToolId::Logs, 296);
    changed.workbench_layout.bottom_panel_height = 312;
    changed
        .keybindings
        .iter_mut()
        .find(|binding| binding.action == "terminal.newTab")
        .expect("旧版 terminal.newTab action 保留")
        .binding = "Ctrl+Alt+T".to_owned();
    service
        .save_settings(changed.clone(), loaded.revision)
        .await
        .expect("typed settings 使用原 revision 保存");
    assert_eq!(
        service
            .load_settings()
            .await
            .expect("保存后重新读取")
            .settings,
        changed
    );

    let source = store
        .read(ConfigDocumentPath::Settings)
        .await
        .expect("读取原始配置")
        .expect("settings 文件存在")
        .source;
    assert!(source.contains("[appearance]\n"));
    assert!(source.contains("windowOpacity = 91 # retain this user-owned inline comment"));
    assert!(source.contains("rendererType = \"gpu\""));
    assert!(source.contains("idleTimeoutSeconds = 240"));
    assert!(!source.contains("timeoutSeconds = 240"));
    assert!(source.contains("binding = \"Ctrl+Alt+T\""));
    let parsed = toml::from_str::<toml::Value>(&source).expect("保存结果保持有效 TOML");
    let layout = parsed
        .get("workbenchLayout")
        .and_then(toml::Value::as_table)
        .expect("布局偏好以当前 settings 表保存");
    assert_eq!(layout["hostSidebarCollapsed"].as_bool(), Some(true));
    assert_eq!(layout["hostSidebarWidth"].as_integer(), Some(304));
    assert_eq!(layout["bottomPanelHeight"].as_integer(), Some(312));
    let dock_widths = layout["dockPanelWidths"]
        .as_table()
        .expect("按 ToolId 保存宽度");
    assert_eq!(dock_widths["sftp"].as_integer(), Some(372));
    assert_eq!(dock_widths["logs"].as_integer(), Some(296));
    let vendor_metadata = parsed
        .get("keybindings")
        .and_then(toml::Value::as_array)
        .and_then(|rows| {
            rows.iter().find(|row| {
                row.get("action").and_then(toml::Value::as_str) == Some("terminal.newTab")
            })
        })
        .and_then(|row| row.get("vendorMetadata"))
        .and_then(toml::Value::as_str);
    assert_eq!(vendor_metadata, Some("keep-me"));
}

/// 各应用色板独立往返并保留旧终端色盘、原始注释与扩展字段。
///
/// @author kongweiguang
#[tokio::test]
async fn app_color_schemes_roundtrip_without_changing_terminal_or_unknown_fields() {
    let temp = TemporaryConfigRoot::new();
    let original = concat!(
        "schema_version = 1\n",
        "[appearance]\n",
        "interfaceLanguage = \"system\" # user note\n",
        "vendorAppearance = \"keep\"\n",
        "[terminal]\n",
        "colorScheme = \"tokyoNight\"\n",
        "lightColorScheme = \"github\"\n",
        "darkColorScheme = \"solarized\"\n",
    );
    fs::write(temp.0.join("settings.toml"), original).expect("写入不含应用配色的旧设置");
    let store = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置根");
    let service = PreferencesService::new(Arc::new(store.clone()));
    let mut current = service.load_settings().await.expect("加载旧设置");
    assert_eq!(current.settings.app_color_scheme, AppColorScheme::Kerminal);
    assert_eq!(
        fs::read(temp.0.join("settings.toml")).unwrap(),
        original.as_bytes()
    );
    let terminal = current.settings.terminal.clone();

    for scheme in AppColorScheme::ALL {
        current.settings.app_color_scheme = scheme;
        let saved = service
            .save_settings(current.settings, current.revision)
            .await
            .expect("按已读 revision 保存应用色板");
        assert_eq!(saved.settings.app_color_scheme, scheme);
        let reloaded = service.load_settings().await.expect("保存后重新读取");
        assert_eq!(reloaded.settings.app_color_scheme, scheme);
        assert_eq!(reloaded.settings.terminal, terminal);
        let raw = fs::read_to_string(temp.0.join("settings.toml")).expect("读取保存后的设置");
        assert!(raw.contains("interfaceLanguage = \"system\" # user note"));
        assert!(raw.contains("vendorAppearance = \"keep\""));
        let parsed: toml::Value = toml::from_str(&raw).expect("保存结果是合法 TOML");
        assert_eq!(
            parsed["appearance"]["colorScheme"].as_str(),
            Some(scheme.as_str())
        );
        assert_eq!(
            parsed["terminal"]["colorScheme"].as_str(),
            Some("tokyoNight")
        );
        assert_eq!(
            parsed["terminal"]["lightColorScheme"].as_str(),
            Some("github")
        );
        assert_eq!(
            parsed["terminal"]["darkColorScheme"].as_str(),
            Some("solarized")
        );
        current = reloaded;
    }

    let reopened = ConfigurationStore::initialize(&temp.0).expect("重新打开隔离配置根");
    let restored = PreferencesService::new(Arc::new(reopened))
        .load_settings()
        .await
        .expect("重新打开后读取偏好");
    assert_eq!(
        restored.settings.app_color_scheme,
        current.settings.app_color_scheme
    );
    assert_eq!(restored.settings.terminal, terminal);
}

/// 旧 SFTP 名称在运行时只作为未知字段保留，不再影响 typed 值或由 saver 特别删除。
///
/// @author kongweiguang
#[tokio::test]
async fn runtime_settings_do_not_interpret_or_strip_retired_sftp_alias() {
    let temp = TemporaryConfigRoot::new();
    let store = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");
    let old_alias = "schema_version = 1\n[sftp]\ntimeoutSeconds = 240 # pre-migration note\n";
    store
        .write(ConfigDocumentPath::Settings, old_alias.to_owned(), None)
        .await
        .expect("旧名称作为未知扩展可被原文仓储保存");
    let service = PreferencesService::new(Arc::new(store.clone()));

    let loaded = service.load_settings().await.expect("读取当前设置 DTO");
    assert_eq!(loaded.settings.sftp.idle_timeout_seconds, 180);
    service
        .save_settings(loaded.settings, loaded.revision)
        .await
        .expect("按当前字段保存设置");
    let source = store
        .read(ConfigDocumentPath::Settings)
        .await
        .expect("读回原文")
        .expect("settings 文件存在")
        .source;

    assert!(source.contains("timeoutSeconds = 240 # pre-migration note"));
    assert!(source.contains("idleTimeoutSeconds = 180"));
}

/// inline keybinding row 也通过真实 codec 更新绑定，并保留扩展 metadata。
#[tokio::test]
async fn typed_settings_patch_preserves_unknown_inline_keybinding_fields() {
    let temp = TemporaryConfigRoot::new();
    let store = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");
    let source = r#"
schema_version = 1
themeMode = "dark"
interfaceDensity = "compact"
keybindings = [{ action = "terminal.newTab", label = "新建终端", description = "", binding = "Ctrl+Shift+T", windowsBinding = "Ctrl+Shift+T", macBinding = "Cmd+Shift+T", scope = "workspace", editable = true, vendorMetadata = "keep-me" }]
"#;
    store
        .write(ConfigDocumentPath::Settings, source.to_owned(), None)
        .await
        .expect("inline-array settings 写入");
    let service = PreferencesService::new(Arc::new(store.clone()));
    let loaded = service
        .load_settings()
        .await
        .expect("inline-array settings 读取");
    let mut changed = loaded.settings;
    changed.keybindings[0].binding = "Ctrl+Alt+T".to_owned();
    service
        .save_settings(changed, loaded.revision)
        .await
        .expect("inline-array settings 保存");

    let saved = store
        .read(ConfigDocumentPath::Settings)
        .await
        .expect("读取保存后的配置")
        .expect("settings 文件存在");
    let parsed =
        toml::from_str::<toml::Value>(&saved.source).expect("inline-array 保存结果保持有效 TOML");
    let row = parsed
        .get("keybindings")
        .and_then(toml::Value::as_array)
        .and_then(|rows| rows.first())
        .expect("keybinding row 保持存在");
    assert_eq!(
        row.get("binding").and_then(toml::Value::as_str),
        Some("Ctrl+Alt+T")
    );
    assert_eq!(
        row.get("vendorMetadata").and_then(toml::Value::as_str),
        Some("keep-me")
    );
    assert!(row.get("linuxBinding").is_none());
    assert!(row.get("windowsAlternateBinding").is_none());
}

/// 可选快捷键字段区分旧配置缺失与用户显式清空，并通过真实 settings codec 往返。
///
/// @author kongweiguang
#[tokio::test]
async fn optional_keybinding_fields_roundtrip_explicit_empty_values() {
    let temp = TemporaryConfigRoot::new();
    let store = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");
    let source = r#"
schema_version = 1
themeMode = "dark"
interfaceDensity = "compact"
keybindings = [{ action = "terminal.paste", label = "粘贴", description = "", binding = "", windowsBinding = "Ctrl+V", macBinding = "Cmd+V", scope = "terminal", editable = true }]
"#;
    store
        .write(ConfigDocumentPath::Settings, source.to_owned(), None)
        .await
        .expect("旧版 paste 行写入");
    let service = PreferencesService::new(Arc::new(store.clone()));
    let loaded = service.load_settings().await.expect("旧设置可读取");
    assert_eq!(loaded.settings.keybindings[0].linux_binding, None);
    assert_eq!(
        loaded.settings.keybindings[0].windows_alternate_binding,
        None
    );

    let mut changed = loaded.settings.clone();
    changed.keybindings[0].linux_binding = Some(String::new());
    changed.keybindings[0].windows_alternate_binding = Some(String::new());
    service
        .save_settings(changed, loaded.revision)
        .await
        .expect("显式空平台绑定保存");

    let saved = store
        .read(ConfigDocumentPath::Settings)
        .await
        .expect("读取保存后的设置")
        .expect("settings 文件仍在");
    let parsed = toml::from_str::<toml::Value>(&saved.source).expect("保存内容是有效 TOML");
    let row = parsed
        .get("keybindings")
        .and_then(toml::Value::as_array)
        .and_then(|rows| rows.first())
        .expect("paste 行保留");
    assert_eq!(
        row.get("linuxBinding").and_then(toml::Value::as_str),
        Some("")
    );
    assert_eq!(
        row.get("windowsAlternateBinding")
            .and_then(toml::Value::as_str),
        Some("")
    );
    let reloaded = service.load_settings().await.expect("保存后可回读");
    assert_eq!(
        reloaded.settings.keybindings[0].linux_binding.as_deref(),
        Some("")
    );
    assert_eq!(
        reloaded.settings.keybindings[0]
            .windows_alternate_binding
            .as_deref(),
        Some("")
    );
}

/// 设置 typed port 使用磁盘原始 revision，旧快照不能覆盖外部写入。
#[tokio::test]
async fn typed_settings_save_rejects_stale_revision_without_overwrite() {
    let temp = TemporaryConfigRoot::new();
    let store = ConfigurationStore::initialize(&temp.0).expect("初始化隔离配置仓储");
    let service = PreferencesService::new(Arc::new(store.clone()));
    let initial = service.load_settings().await.expect("初次读取默认设置");
    let mut first = initial.settings.clone();
    first.theme_mode = ThemeMode::Dark;
    let saved = service
        .save_settings(first, initial.revision)
        .await
        .expect("首次 typed 保存");
    let external_source =
        "schema_version = 1\nthemeMode = \"light\"\ninterfaceDensity = \"compact\"\n";
    store
        .write(
            ConfigDocumentPath::Settings,
            external_source.to_owned(),
            saved.revision.clone(),
        )
        .await
        .expect("模拟外部编辑器按当前 revision 保存");

    assert!(
        service
            .save_settings(SettingsSnapshot::default(), saved.revision)
            .await
            .is_err()
    );
    let source = fs::read_to_string(temp.0.join("settings.toml")).expect("外部原文仍存在");
    assert_eq!(source, external_source);
}

struct FixtureShellProvider;

impl DefaultLocalShellProvider for FixtureShellProvider {
    /// 提供不触碰操作系统 shell 的固定 fixture，验证显式 profile 优先级。
    fn resolve_default(&self) -> Result<LocalSessionTarget, AppError> {
        Ok(LocalSessionTarget {
            program: OsString::from("fallback-shell"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        })
    }
}
