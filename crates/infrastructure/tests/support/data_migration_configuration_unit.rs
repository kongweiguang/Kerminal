//! 配置迁移规划的隔离测试；测试源树使用临时根目录且不调用提交设施。
//!
//! @author kongweiguang

use std::fs;

use kerminal_application::preferences::{CONFIG_GUIDE, CONFIG_GUIDE_REVISION};
use tempfile::tempdir;

use super::super::FileEdit;
use super::plan_configuration_migration;
use crate::configuration;

const PREVIOUS_GUIDE: &str =
    include_str!("../../../../tests/fixtures/configuration/config-guide-v2.md");
const REVISION_3_GUIDE: &str =
    include_str!("../../../../tests/fixtures/configuration/config-guide-v3.md");
const REVISION_5_GUIDE: &str =
    include_str!("../../../../tests/fixtures/configuration/config-guide-v5.md");
const REVISION_6_GUIDE: &str =
    include_str!("../../../../tests/fixtures/configuration/config-guide-v6.md");
const REVISION_7_GUIDE: &str =
    include_str!("../../../../tests/fixtures/configuration/config-guide-v7.md");
const REVISION_8_GUIDE: &str =
    include_str!("../../../../tests/fixtures/configuration/config-guide-v8.md");
const REVISION_9_GUIDE: &str =
    include_str!("../../../../tests/fixtures/configuration/config-guide-v9.md");
const REVISION_10_GUIDE: &str =
    include_str!("../../../../tests/fixtures/configuration/config-guide-v10.md");
const REVISION_11_GUIDE: &str =
    include_str!("../../../../tests/fixtures/configuration/config-guide-v11.md");
const REVISION_12_GUIDE: &str =
    include_str!("../../../../tests/fixtures/configuration/config-guide-v12.md");
const HOST_V2_SERIAL: &str =
    include_str!("../../../../tests/fixtures/configuration/host-schema-v2-serial-migration.toml");
const HOST_V3_SERIAL: &str =
    include_str!("../../../../tests/fixtures/configuration/host-schema-v3-serial.toml");

/// 写入隔离配置 fixture；父目录只在测试临时根下创建。
///
/// @author kongweiguang
fn write_fixture(root: &std::path::Path, relative_path: &str, contents: &str) {
    let path = root.join(relative_path);
    fs::create_dir_all(path.parent().expect("fixture 有父目录")).expect("创建 fixture 目录");
    fs::write(path, contents).expect("写入 fixture");
}

/// 按固定相对路径读取一项计划，避免依赖 planner 内部文件枚举顺序。
///
/// @author kongweiguang
fn edit<'a>(edits: &'a [FileEdit], relative_path: &str) -> Option<&'a FileEdit> {
    edits
        .iter()
        .find(|edit| edit.relative_path == std::path::Path::new(relative_path))
}

/// 首次使用时无配置仍为空计划，且 dry-run 不创建根目录或生成指南。
///
/// @author kongweiguang
#[test]
fn missing_documents_are_valid_and_planning_has_no_side_effects() {
    let parent = tempdir().expect("创建隔离父目录");
    let root = parent.path().join("not-created");

    let edits = plan_configuration_migration(&root).expect("空配置根可预检");

    assert!(edits.is_empty());
    assert!(!root.exists());
}

/// 已存在文档缺少 schema marker 时拒绝迁移，不猜测其来源版本，也不改原字节。
///
/// @author kongweiguang
#[test]
fn missing_schema_marker_is_rejected_without_editing_source() {
    let root = tempdir().expect("创建隔离配置根");
    let original = "themeMode = \"dark\"\n";
    write_fixture(root.path(), "settings.toml", original);

    assert!(plan_configuration_migration(root.path()).is_err());
    assert_eq!(
        fs::read(root.path().join("settings.toml")).expect("原文件仍存在"),
        original.as_bytes()
    );
}

/// 当前格式只做只读校验，不因 TOML 往返改写其换行、注释或缺省字段。
///
/// @author kongweiguang
#[test]
fn canonical_configuration_is_not_rewritten() {
    let root = tempdir().expect("创建隔离配置根");
    let original =
        "schema_version = 1\r\n[sftp] # 保留的说明\r\nidleTimeoutSeconds = 240\r\n".as_bytes();
    fs::write(root.path().join("settings.toml"), original).expect("写入当前格式设置");

    let edits = plan_configuration_migration(root.path()).expect("当前设置仍按 canonical 验证");

    assert!(edits.is_empty());
    assert_eq!(
        fs::read(root.path().join("settings.toml")).expect("读取原设置"),
        original
    );
}

/// 需要迁移的符号链接配置必须拒绝，链接和其目标内容都保持原样。
///
/// @author kongweiguang
#[cfg(unix)]
#[test]
fn migration_rejects_symlinked_configuration_without_touching_target() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("创建隔离配置根");
    let target = root.path().join("linked-settings.toml");
    let link = root.path().join("settings.toml");
    let original = "schema_version = 1\n[sftp]\ntimeoutSeconds = 240\n";
    fs::write(&target, original).expect("写入链接目标");
    symlink(&target, &link).expect("创建配置链接");

    assert!(plan_configuration_migration(root.path()).is_err());
    assert!(
        fs::symlink_metadata(&link)
            .expect("原配置路径仍存在")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read(&target).expect("链接目标仍存在"),
        original.as_bytes()
    );
}

/// SFTP 旧键迁移到 camelCase canonical 键，保留原注释、未知字段且不补可选默认值。
///
/// @author kongweiguang
#[test]
fn settings_alias_is_moved_without_default_expansion_or_comment_loss() {
    let root = tempdir().expect("创建隔离配置根");
    let original = concat!(
        "schema_version = 1\n",
        "[sftp]\n",
        "timeoutSeconds = 240 # 用户保留的说明\n",
        "vendor_window = 17\n",
    );
    write_fixture(root.path(), "settings.toml", original);

    let edits = plan_configuration_migration(root.path()).expect("旧 SFTP key 可迁移");
    let planned = edit(&edits, "settings.toml").expect("settings 有编辑计划");
    let after = String::from_utf8(planned.after.clone()).expect("迁移结果为 UTF-8");

    assert!(after.contains("idleTimeoutSeconds = 240 # 用户保留的说明"));
    assert!(after.contains("vendor_window = 17"));
    assert!(!after.contains("timeoutSeconds"));
    assert!(!after.contains("globalTransfers"));
    assert_eq!(planned.before.as_deref(), Some(original.as_bytes()));
    assert_eq!(
        fs::read(root.path().join("settings.toml")).expect("planner 不写源文件"),
        original.as_bytes()
    );
}

/// 迁移只为已存在的非空快捷键数组补动作，保留原行和逐平台冲突策略。
///
/// @author kongweiguang
#[test]
fn settings_migration_adds_missing_actions_without_rewriting_existing_rows() {
    let root = tempdir().expect("创建隔离配置根");
    let original = concat!(
        "schema_version = 1\n",
        "keybindings = [{ action = \"settings.open\", label = \"Open settings\", description = \"\", binding = \"Ctrl+V\", windowsBinding = \"Ctrl+V\", windowsAlternateBinding = \"Ctrl+Shift+V\", linuxBinding = \"Ctrl+Shift+V\", macBinding = \"Cmd+V\", scope = \"global\", editable = true, vendorMetadata = \"keep\" }]\n",
    );
    write_fixture(root.path(), "settings.toml", original);

    let edits = plan_configuration_migration(root.path()).expect("旧快捷键动作可迁移");
    let settings = String::from_utf8(
        edit(&edits, "settings.toml")
            .expect("缺失动作产生编辑")
            .after
            .clone(),
    )
    .expect("迁移结果为 UTF-8");
    let parsed = toml::from_str::<toml::Value>(&settings).expect("迁移结果是有效 TOML");
    let rows = parsed
        .get("keybindings")
        .and_then(toml::Value::as_array)
        .expect("快捷键仍是数组");
    let row = rows
        .iter()
        .find(|row| row.get("action").and_then(toml::Value::as_str) == Some("settings.open"))
        .expect("原快捷键行仍在");
    assert_eq!(
        row.get("windowsBinding").and_then(toml::Value::as_str),
        Some("Ctrl+V")
    );
    assert_eq!(
        row.get("linuxBinding").and_then(toml::Value::as_str),
        Some("Ctrl+Shift+V")
    );
    assert_eq!(
        row.get("vendorMetadata").and_then(toml::Value::as_str),
        Some("keep")
    );
    let paste = rows
        .iter()
        .find(|row| row.get("action").and_then(toml::Value::as_str) == Some("terminal.paste"))
        .expect("缺失粘贴动作已补入");
    assert_eq!(
        paste.get("windowsBinding").and_then(toml::Value::as_str),
        Some("")
    );
    assert_eq!(
        paste
            .get("windowsAlternateBinding")
            .and_then(toml::Value::as_str),
        Some("")
    );
    assert_eq!(
        paste.get("linuxBinding").and_then(toml::Value::as_str),
        Some("")
    );
    assert_eq!(
        paste.get("macBinding").and_then(toml::Value::as_str),
        Some("")
    );
    assert!(
        rows.iter()
            .any(|row| row.get("action").and_then(toml::Value::as_str) == Some("application.quit"))
    );
    assert_eq!(
        fs::read(root.path().join("settings.toml")).expect("规划不写源文档"),
        original.as_bytes()
    );
}

/// 新增单键粘贴会避开旧 chord 的首 stroke，且不改写用户原绑定。
///
/// @author kongweiguang
#[test]
fn settings_migration_preserves_existing_chord_prefixes() {
    let root = tempdir().expect("创建隔离配置根");
    let original = concat!(
        "schema_version = 1\n",
        "keybindings = [{ action = \"settings.open\", label = \"Open settings\", binding = \"Ctrl+V X\", windowsBinding = \"Ctrl+V X\", windowsAlternateBinding = \"Ctrl+Shift+V X\", linuxBinding = \"Ctrl+Shift+V X\", macBinding = \"Cmd+V X\", scope = \"global\", editable = true }]\n",
    );
    write_fixture(root.path(), "settings.toml", original);

    let edits = plan_configuration_migration(root.path()).expect("chord 前缀可安全避让");
    let settings = String::from_utf8(
        edit(&edits, "settings.toml")
            .expect("快捷键补全产生编辑")
            .after
            .clone(),
    )
    .expect("迁移结果为 UTF-8");
    let parsed = toml::from_str::<toml::Value>(&settings).expect("迁移结果是有效 TOML");
    let rows = parsed
        .get("keybindings")
        .and_then(toml::Value::as_array)
        .expect("快捷键仍是数组");
    let paste = rows
        .iter()
        .find(|row| row.get("action").and_then(toml::Value::as_str) == Some("terminal.paste"))
        .expect("粘贴动作已补入");
    let settings_open = rows
        .iter()
        .find(|row| row.get("action").and_then(toml::Value::as_str) == Some("settings.open"))
        .expect("原有设置快捷键保留");

    for field in [
        "windowsBinding",
        "windowsAlternateBinding",
        "linuxBinding",
        "macBinding",
    ] {
        assert_eq!(paste.get(field).and_then(toml::Value::as_str), Some(""));
    }
    assert_eq!(
        settings_open
            .get("windowsBinding")
            .and_then(toml::Value::as_str),
        Some("Ctrl+V X")
    );
}

/// 用户显式的空快捷键列表表示不启用动作，迁移必须按字节保留。
///
/// @author kongweiguang
#[test]
fn explicit_empty_keybindings_are_not_populated_by_migration() {
    let root = tempdir().expect("创建隔离配置根");
    let original = "schema_version = 1\nkeybindings = []\n";
    write_fixture(root.path(), "settings.toml", original);

    let edits = plan_configuration_migration(root.path()).expect("空列表设置仍有效");

    assert!(edit(&edits, "settings.toml").is_none());
    assert_eq!(
        fs::read(root.path().join("settings.toml")).expect("读取原设置"),
        original.as_bytes()
    );
}

/// 一次性迁移尊重已存在的空绑定行，不把用户主动禁用的动作重新启用。
///
/// @author kongweiguang
#[test]
fn existing_disabled_keybinding_actions_are_not_overwritten() {
    let root = tempdir().expect("创建隔离配置根");
    let original = concat!(
        "schema_version = 1\n",
        "keybindings = [{ action = \"application.quit\", label = \"Exit\", binding = \"\", windowsBinding = \"\", macBinding = \"\", scope = \"global\", editable = true }]\n",
    );
    write_fixture(root.path(), "settings.toml", original);

    let edits = plan_configuration_migration(root.path()).expect("禁用的已有动作仍可迁移");
    let settings = String::from_utf8(
        edit(&edits, "settings.toml")
            .expect("还缺失粘贴动作所以产生计划")
            .after
            .clone(),
    )
    .expect("迁移结果为 UTF-8");
    let parsed = toml::from_str::<toml::Value>(&settings).expect("迁移结果是有效 TOML");
    let quit = parsed
        .get("keybindings")
        .and_then(toml::Value::as_array)
        .and_then(|rows| {
            rows.iter().find(|row| {
                row.get("action").and_then(toml::Value::as_str) == Some("application.quit")
            })
        })
        .expect("退出动作行保留");

    assert_eq!(quit.get("binding").and_then(toml::Value::as_str), Some(""));
    assert_eq!(
        quit.get("windowsBinding").and_then(toml::Value::as_str),
        Some("")
    );
    assert_eq!(
        quit.get("macBinding").and_then(toml::Value::as_str),
        Some("")
    );
    assert_eq!(
        quit.get("label").and_then(toml::Value::as_str),
        Some("Exit")
    );
}

/// TOML 表数组样式也能追加默认行，并继续保留既有 table 装饰与值。
///
/// @author kongweiguang
#[test]
fn settings_migration_appends_actions_to_array_of_tables() {
    let root = tempdir().expect("创建隔离配置根");
    write_fixture(
        root.path(),
        "settings.toml",
        concat!(
            "schema_version = 1\n",
            "[[keybindings]] # 保留表注释\n",
            "action = \"settings.open\"\n",
            "label = \"Open settings\"\n",
            "binding = \"Ctrl+Alt+S\"\n",
            "windowsBinding = \"Ctrl+Alt+S\"\n",
            "macBinding = \"Cmd+,\"\n",
            "scope = \"global\"\n",
            "editable = true\n",
        ),
    );

    let edits = plan_configuration_migration(root.path()).expect("表数组快捷键可迁移");
    let settings = String::from_utf8(
        edit(&edits, "settings.toml")
            .expect("缺失动作产生编辑")
            .after
            .clone(),
    )
    .expect("迁移结果为 UTF-8");

    assert!(settings.contains("[[keybindings]] # 保留表注释"));
    assert!(settings.contains("action = \"application.quit\""));
    assert!(settings.contains("action = \"terminal.paste\""));
}

/// 新旧 SFTP key 同时出现且值不同時拒绝，不挑选任一值。
///
/// @author kongweiguang
#[test]
fn conflicting_settings_alias_values_are_rejected() {
    let root = tempdir().expect("创建隔离配置根");
    let original = "schema_version = 1\n[sftp]\ntimeoutSeconds = 240\nidleTimeoutSeconds = 180\n";
    write_fixture(root.path(), "settings.toml", original);

    assert!(plan_configuration_migration(root.path()).is_err());
    assert_eq!(
        fs::read(root.path().join("settings.toml")).expect("源文件仍存在"),
        original.as_bytes()
    );
}

/// 新旧 SFTP key 等值时折叠为一个 canonical 字段，不产生值选择歧义。
///
/// @author kongweiguang
#[test]
fn equal_settings_alias_values_collapse_to_canonical_key() {
    let root = tempdir().expect("创建隔离配置根");
    write_fixture(
        root.path(),
        "settings.toml",
        "schema_version = 1\n[sftp]\ntimeoutSeconds = 240\nidleTimeoutSeconds = 240\n",
    );

    let edits = plan_configuration_migration(root.path()).expect("等值别名可收敛");
    let settings = String::from_utf8(edit(&edits, "settings.toml").unwrap().after.clone())
        .expect("settings 结果为 UTF-8");
    assert_eq!(settings.matches("idleTimeoutSeconds").count(), 1);
    assert!(!settings.contains("timeoutSeconds"));
}

/// 两种 snippet TOML 数组布局都迁移字段名，并保留其它用户字段。
///
/// @author kongweiguang
#[test]
fn snippet_aliases_are_moved_in_table_and_inline_table_arrays() {
    let root = tempdir().expect("创建隔离配置根");
    write_fixture(
        root.path(),
        "snippets/table.toml",
        concat!(
            "schema_version = 1\n",
            "id = \"table\"\n",
            "title = \"Table\"\n",
            "command = \"echo {{path}}\"\n",
            "scope = \"any\"\n",
            "sort_order = 1\n",
            "created_at = \"1\"\n",
            "updated_at = \"1\"\n",
            "vendor_note = \"keep\"\n",
            "[[variables]]\n",
            "name = \"path\"\n",
            "label = \"Path\"\n",
            "description = \"\"\n",
            "kind = \"path\"\n",
            "required = true\n",
            "defaultValue = \"src\"\n",
            "renderStrategy = \"shellArg\"\n",
            "[[context_bindings]]\n",
            "kind = \"host\"\n",
            "targetId = \"build\"\n",
        ),
    );
    write_fixture(
        root.path(),
        "snippets/inline.toml",
        concat!(
            "schema_version = 1\n",
            "id = \"inline\"\n",
            "title = \"Inline\"\n",
            "command = \"echo {{path}}\"\n",
            "scope = \"any\"\n",
            "sort_order = 2\n",
            "created_at = \"1\"\n",
            "updated_at = \"1\"\n",
            "variables = [{ name = \"path\", label = \"Path\", description = \"\", kind = \"path\", required = true, defaultValue = \"src\", renderStrategy = \"shellArg\", vendor = \"keep\" }]\n",
            "context_bindings = [{ kind = \"host\", targetId = \"build\" }]\n",
        ),
    );

    let edits = plan_configuration_migration(root.path()).expect("snippet aliases 可迁移");
    let table = String::from_utf8(edit(&edits, "snippets/table.toml").unwrap().after.clone())
        .expect("table 结果为 UTF-8");
    let inline = String::from_utf8(edit(&edits, "snippets/inline.toml").unwrap().after.clone())
        .expect("inline 结果为 UTF-8");

    for result in [&table, &inline] {
        assert!(result.contains("default_value = \"src\""));
        assert!(result.contains("render_strategy = \"shellArg\""));
        assert!(result.contains("target_id = \"build\""));
        assert!(!result.contains("defaultValue"));
        assert!(!result.contains("renderStrategy"));
        assert!(!result.contains("targetId"));
    }
    assert!(table.contains("vendor_note = \"keep\""));
    assert!(inline.contains("vendor = \"keep\""));
}

/// 只在一次性迁移中为旧变量补齐原 reader 的类型与转义策略缺省。
///
/// @author kongweiguang
#[test]
fn snippet_migration_materializes_missing_variable_semantics() {
    let root = tempdir().expect("创建隔离配置根");
    write_fixture(
        root.path(),
        "snippets/table.toml",
        concat!(
            "schema_version = 1\n",
            "id = \"table\"\n",
            "title = \"Table\"\n",
            "command = \"echo {{path}}\"\n",
            "scope = \"any\"\n",
            "sort_order = 1\n",
            "created_at = \"1\"\n",
            "updated_at = \"1\"\n",
            "[[variables]]\n",
            "name = \"path\"\n",
            "label = \"Path\"\n",
            "description = \"\"\n",
            "required = true\n",
        ),
    );
    write_fixture(
        root.path(),
        "snippets/inline.toml",
        concat!(
            "schema_version = 1\n",
            "id = \"inline\"\n",
            "title = \"Inline\"\n",
            "command = \"echo {{path}}\"\n",
            "scope = \"any\"\n",
            "sort_order = 2\n",
            "created_at = \"1\"\n",
            "updated_at = \"1\"\n",
            "variables = [{ name = \"path\", label = \"Path\", description = \"\", kind = \"path\", required = true, vendor = \"keep\" }]\n",
        ),
    );

    let edits = plan_configuration_migration(root.path()).expect("旧变量缺项可显式迁移");
    let table = String::from_utf8(edit(&edits, "snippets/table.toml").unwrap().after.clone())
        .expect("表数组迁移结果为 UTF-8");
    let inline = String::from_utf8(edit(&edits, "snippets/inline.toml").unwrap().after.clone())
        .expect("内联数组迁移结果为 UTF-8");

    assert!(table.contains("kind = \"text\""));
    assert!(table.contains("render_strategy = \"shellArg\""));
    assert!(inline.contains("kind = \"path\""));
    assert!(inline.contains("render_strategy = \"shellArg\""));
    assert!(inline.contains("vendor = \"keep\""));
}

/// 确保前项补缺后仍检查数组后项，避免短路遍历把旧变量留成不完整定义。
///
/// @author kongweiguang
#[test]
fn snippet_migration_visits_every_variable_after_earlier_defaults_are_added() {
    let root = tempdir().expect("创建隔离配置根");
    write_fixture(
        root.path(),
        "snippets/table.toml",
        concat!(
            "schema_version = 1\n",
            "id = \"table\"\n",
            "title = \"Table\"\n",
            "command = \"echo {{first}} {{second}}\"\n",
            "scope = \"any\"\n",
            "sort_order = 1\n",
            "created_at = \"1\"\n",
            "updated_at = \"1\"\n",
            "[[variables]]\n",
            "name = \"first\"\n",
            "label = \"First\"\n",
            "description = \"\"\n",
            "render_strategy = \"literal\"\n",
            "required = true\n",
            "vendor = \"table-first\"\n",
            "[[variables]]\n",
            "name = \"second\"\n",
            "label = \"Second\"\n",
            "description = \"\"\n",
            "kind = \"path\"\n",
            "required = true\n",
            "vendor = \"table-second\"\n",
        ),
    );
    write_fixture(
        root.path(),
        "snippets/inline.toml",
        concat!(
            "schema_version = 1\n",
            "id = \"inline\"\n",
            "title = \"Inline\"\n",
            "command = \"echo {{first}} {{second}}\"\n",
            "scope = \"any\"\n",
            "sort_order = 2\n",
            "created_at = \"1\"\n",
            "updated_at = \"1\"\n",
            "variables = [{ name = \"first\", label = \"First\", description = \"\", render_strategy = \"literal\", required = true, vendor = \"inline-first\" }, { name = \"second\", label = \"Second\", description = \"\", kind = \"url\", required = true, vendor = \"inline-second\" }]\n",
        ),
    );

    let edits = plan_configuration_migration(root.path()).expect("多变量缺项可显式迁移");
    let table = String::from_utf8(edit(&edits, "snippets/table.toml").unwrap().after.clone())
        .expect("表数组迁移结果为 UTF-8");
    let inline = String::from_utf8(edit(&edits, "snippets/inline.toml").unwrap().after.clone())
        .expect("内联数组迁移结果为 UTF-8");

    assert!(table.contains("kind = \"text\""));
    assert!(table.contains("render_strategy = \"literal\""));
    assert!(table.contains("vendor = \"table-first\""));
    assert!(table.contains("kind = \"path\""));
    assert!(table.contains("render_strategy = \"shellArg\""));
    assert!(table.contains("vendor = \"table-second\""));

    assert!(inline.contains("kind = \"text\""));
    assert!(inline.contains("render_strategy = \"literal\""));
    assert!(inline.contains("vendor = \"inline-first\""));
    assert!(inline.contains("kind = \"url\""));
    assert!(inline.contains("render_strategy = \"shellArg\""));
    assert!(inline.contains("vendor = \"inline-second\""));
}

/// 对同一 snippet 字段的冲突 alias 拒绝迁移，不能以新键或旧键覆盖另一值。
///
/// @author kongweiguang
#[test]
fn conflicting_snippet_alias_values_are_rejected() {
    let root = tempdir().expect("创建隔离配置根");
    let original = concat!(
        "schema_version = 1\n",
        "id = \"conflict\"\n",
        "title = \"Conflict\"\n",
        "command = \"echo {{path}}\"\n",
        "scope = \"any\"\n",
        "sort_order = 1\n",
        "created_at = \"1\"\n",
        "updated_at = \"1\"\n",
        "[[variables]]\n",
        "name = \"path\"\n",
        "label = \"Path\"\n",
        "description = \"\"\n",
        "kind = \"path\"\n",
        "required = true\n",
        "defaultValue = \"old\"\n",
        "default_value = \"new\"\n",
        "renderStrategy = \"shellArg\"\n",
    );
    write_fixture(root.path(), "snippets/conflict.toml", original);

    assert!(plan_configuration_migration(root.path()).is_err());
    assert_eq!(
        fs::read(root.path().join("snippets/conflict.toml")).expect("源片段仍存在"),
        original.as_bytes()
    );
}

/// Host v1 根据 tags 优先级补 protocol、升级到 schema v3，并把串口参数移出 tags。
///
/// @author kongweiguang
#[test]
fn host_v1_is_upgraded_and_retired_production_is_removed() {
    let root = tempdir().expect("创建隔离配置根");
    write_fixture(
        root.path(),
        "hosts/legacy.toml",
        concat!(
            "schema_version = 1 # schema note\n",
            "id = \"legacy\"\n",
            "name = \"Legacy\"\n",
            "host = \"legacy.example.invalid\"\n",
            "port = 3389\n",
            "username = \"user\"\n",
            "auth_type = \"agent\"\n",
            "tags = [\"RDP\", \"serial\", \"telnet\"]\n",
            "production = true\n",
            "sort_order = 1\n",
            "created_at = \"1\"\n",
            "updated_at = \"1\"\n",
            "vendor_extension = \"preserve\"\n",
        ),
    );
    write_fixture(
        root.path(),
        "hosts/current.toml",
        concat!(
            "schema_version = 2\n",
            "protocol = \"ssh\"\n",
            "id = \"current\"\n",
            "name = \"Current\"\n",
            "host = \"current.example.invalid\"\n",
            "port = 22\n",
            "username = \"user\"\n",
            "auth_type = \"agent\"\n",
            "production = true\n",
            "vendor_extension = \"preserve\"\n",
            "sort_order = 2\n",
            "created_at = \"1\"\n",
            "updated_at = \"1\"\n",
        ),
    );

    let edits = plan_configuration_migration(root.path()).expect("旧 host 可迁移");
    let host = edit(&edits, "hosts/legacy.toml").expect("host 有编辑计划");
    let after = String::from_utf8(host.after.clone()).expect("host 结果为 UTF-8");

    assert!(after.contains("schema_version = 3 # schema note"));
    assert!(after.contains("protocol = \"serial\""));
    assert!(after.contains("[serial]"));
    assert!(after.contains("device = \"legacy.example.invalid\""));
    assert!(after.contains("baud_rate = 9600"));
    assert!(after.contains("data_bits = 8"));
    assert!(after.contains("parity = \"none\""));
    assert!(after.contains("flow_control = \"none\""));
    assert!(after.contains("vendor_extension = \"preserve\""));
    assert!(!after.contains("host = \"legacy.example.invalid\""));
    assert!(!after.contains("production"));
    assert!(!after.contains("serial-port:"));
    assert!(after.contains("tags = [\"RDP\", \"telnet\"]"));
    assert!(!after.contains("connect_timeout_seconds"));
    let current = String::from_utf8(edit(&edits, "hosts/current.toml").unwrap().after.clone())
        .expect("schema2 host 结果为 UTF-8");
    assert!(current.contains("schema_version = 3"));
    assert!(!current.contains("production"));
    assert!(current.contains("vendor_extension = \"preserve\""));
}

/// Schema-v2 串口 fixture 的短别名只在迁移时归一化，并保留普通标签和扩展字段。
///
/// @author kongweiguang
#[test]
fn host_v2_serial_tags_are_migrated_to_typed_schema_v3() {
    let root = tempdir().expect("创建隔离配置根");
    write_fixture(root.path(), "hosts/serial-legacy.toml", HOST_V2_SERIAL);

    let edits = plan_configuration_migration(root.path()).expect("旧串口格式可迁移");
    let edit = edit(&edits, "hosts/serial-legacy.toml").expect("旧串口主机有迁移计划");
    let after = String::from_utf8(edit.after.clone()).expect("迁移结果为 UTF-8");
    let parsed: toml::Value = toml::from_str(&after).expect("迁移结果为有效 TOML");
    let serial = parsed
        .get("serial")
        .and_then(toml::Value::as_table)
        .expect("串口字段进入 typed table");
    assert_eq!(
        parsed
            .get("schema_version")
            .and_then(toml::Value::as_integer),
        Some(3)
    );
    assert_eq!(
        serial.get("device").and_then(toml::Value::as_str),
        Some("COM11")
    );
    assert_eq!(
        serial.get("baud_rate").and_then(toml::Value::as_integer),
        Some(115_200)
    );
    assert_eq!(
        serial.get("data_bits").and_then(toml::Value::as_integer),
        Some(7)
    );
    assert_eq!(
        serial.get("stop_bits").and_then(toml::Value::as_integer),
        Some(2)
    );
    assert_eq!(
        serial.get("parity").and_then(toml::Value::as_str),
        Some("even")
    );
    assert_eq!(
        serial.get("flow_control").and_then(toml::Value::as_str),
        Some("xonxoff")
    );
    assert!(parsed.get("host").is_none());
    assert!(parsed.get("port").is_none());
    assert!(parsed.get("username").is_none());
    assert_eq!(
        parsed.get("vendor_extension").and_then(toml::Value::as_str),
        Some("preserve-me")
    );
    let tags = parsed
        .get("tags")
        .and_then(toml::Value::as_array)
        .expect("普通 tags 保留")
        .iter()
        .filter_map(toml::Value::as_str)
        .collect::<Vec<_>>();
    assert_eq!(tags, ["lab"]);
    assert!(after.contains("schema_version = 3 # keep migration note"));
    assert!(!after.contains("serial-flow:"));
}

/// 当前 schema-v3 串口文档只校验，不被迁移入口自动补字段或重写。
///
/// @author kongweiguang
#[test]
fn current_serial_host_is_not_repaired_or_rewritten() {
    let root = tempdir().expect("创建隔离配置根");
    write_fixture(root.path(), "hosts/serial-current.toml", HOST_V3_SERIAL);

    let edits = plan_configuration_migration(root.path()).expect("当前串口格式保持有效");

    assert!(edits.is_empty());
    assert_eq!(
        fs::read(root.path().join("hosts/serial-current.toml")).expect("读取当前主机原文"),
        HOST_V3_SERIAL.as_bytes()
    );
}

/// 旧串口值不合法时迁移失败，并保持源文件字节不变供用户修复。
///
/// @author kongweiguang
#[test]
fn invalid_legacy_serial_value_blocks_migration_without_editing_source() {
    let root = tempdir().expect("创建隔离配置根");
    let original = concat!(
        "schema_version = 2\nprotocol = \"serial\"\nid = \"invalid-serial\"\n",
        "name = \"Invalid console\"\nhost = \"COM4\"\nport = 22\nusername = \"\"\n",
        "auth_type = \"agent\"\ntags = [\"serial\", \"serial-baud:fast\"]\n",
        "sort_order = 0\ncreated_at = \"1\"\nupdated_at = \"1\"\n",
    );
    write_fixture(root.path(), "hosts/invalid-serial.toml", original);

    assert!(plan_configuration_migration(root.path()).is_err());
    assert_eq!(
        fs::read(root.path().join("hosts/invalid-serial.toml")).expect("原文件仍存在"),
        original.as_bytes()
    );
}

/// Schema v1/v2 的串口 tag 不能掩盖缺失或损坏的旧 endpoint 字段，规划失败时保留原字节。
///
/// @author kongweiguang
#[test]
fn invalid_legacy_serial_endpoint_blocks_migration_without_editing_source() {
    let invalid_fields = [
        ("host type", "host = \"COM4\"\n", "host = 4\n"),
        ("port type", "port = 22\n", "port = \"22\"\n"),
        ("port below range", "port = 22\n", "port = -1\n"),
        ("port above range", "port = 22\n", "port = 65536\n"),
        ("username type", "username = \"\"\n", "username = false\n"),
        ("missing host", "host = \"COM4\"\n", ""),
        ("missing port", "port = 22\n", ""),
        ("missing username", "username = \"\"\n", ""),
    ];

    for schema_version in [1, 2] {
        let protocol = if schema_version == 1 {
            ""
        } else {
            "protocol = \"serial\"\n"
        };
        let template = format!(
            concat!(
                "schema_version = {schema_version}\n{protocol}",
                "id = \"invalid-serial\"\nname = \"Invalid console\"\n",
                "host = \"COM4\"\nport = 22\nusername = \"\"\n",
                "auth_type = \"agent\"\ntags = [\"serial\", \"serial-port:COM5\"]\n",
                "sort_order = 0\ncreated_at = \"1\"\nupdated_at = \"1\"\n",
            ),
            schema_version = schema_version,
            protocol = protocol,
        );

        for (case, old_field, replacement) in invalid_fields {
            let original = template.replace(old_field, replacement);
            assert_ne!(original, template, "{case} fixture must alter source");
            let root = tempdir().expect("创建隔离配置根");
            write_fixture(root.path(), "hosts/invalid-serial.toml", &original);

            assert!(
                plan_configuration_migration(root.path()).is_err(),
                "schema v{schema_version} must reject {case}"
            );
            assert_eq!(
                fs::read(root.path().join("hosts/invalid-serial.toml")).expect("原文件仍存在"),
                original.as_bytes(),
                "schema v{schema_version} must preserve source for {case}"
            );
        }
    }
}

/// 一次性迁移不把非串口主机中的串口保留 tag 默认为用户标签继续携带。
///
/// @author kongweiguang
#[test]
fn nonserial_legacy_host_with_serial_tags_is_rejected_without_editing_source() {
    let root = tempdir().expect("创建隔离配置根");
    let original = concat!(
        "schema_version = 2\nprotocol = \"ssh\"\nid = \"ssh-legacy\"\n",
        "name = \"Legacy SSH\"\nhost = \"example.test\"\nport = 22\n",
        "username = \"user\"\nauth_type = \"agent\"\ntags = [\"serial-baud:9600\"]\n",
        "sort_order = 0\ncreated_at = \"1\"\nupdated_at = \"1\"\n",
    );
    write_fixture(root.path(), "hosts/ssh-legacy.toml", original);

    assert!(plan_configuration_migration(root.path()).is_err());
    assert_eq!(
        fs::read(root.path().join("hosts/ssh-legacy.toml")).expect("原文件仍存在"),
        original.as_bytes()
    );
}

/// schema v1 已包含 protocol 表示迁移中断，planner 不补写或覆盖该值。
///
/// @author kongweiguang
#[test]
fn partially_migrated_host_is_rejected_without_editing_source() {
    let root = tempdir().expect("创建隔离配置根");
    let original = concat!(
        "schema_version = 1\n",
        "protocol = \"ssh\"\n",
        "id = \"partial\"\n",
        "name = \"Partial\"\n",
        "host = \"partial.example.invalid\"\n",
        "port = 22\n",
        "username = \"user\"\n",
        "auth_type = \"agent\"\n",
        "sort_order = 1\n",
        "created_at = \"1\"\n",
        "updated_at = \"1\"\n",
    );
    write_fixture(root.path(), "hosts/partial.toml", original);

    assert!(plan_configuration_migration(root.path()).is_err());
    assert_eq!(
        fs::read(root.path().join("hosts/partial.toml")).expect("源 host 仍存在"),
        original.as_bytes()
    );
}

/// 只有摘要白名单内的旧生成指南进入编辑计划；自定义与未来正文逐字节保留。
///
/// @author kongweiguang
#[test]
fn only_known_old_generated_guides_are_planned_for_update() {
    let old_root = tempdir().expect("创建旧指南配置根");
    write_fixture(old_root.path(), "kerminal-config.md", PREVIOUS_GUIDE);
    let old_edits = plan_configuration_migration(old_root.path()).expect("识别冻结的旧正文");
    let old = edit(&old_edits, "kerminal-config.md").expect("已知旧指南有编辑计划");
    assert_eq!(old.after, configuration::managed_config_guide());

    let marked_root = tempdir().expect("创建带标记旧指南配置根");
    let old_digest = configuration::config_guide_digest(PREVIOUS_GUIDE);
    let marked_old = format!(
        "{}2 sha256={old_digest} -->\n{PREVIOUS_GUIDE}",
        configuration::CONFIG_GUIDE_MARKER_PREFIX
    );
    write_fixture(marked_root.path(), "kerminal-config.md", &marked_old);
    let marked_edits =
        plan_configuration_migration(marked_root.path()).expect("识别旧 revision 标记");
    assert!(edit(&marked_edits, "kerminal-config.md").is_some());

    let custom_root = tempdir().expect("创建自定义指南配置根");
    let custom = "# User notes\nKeep this text.\n";
    write_fixture(custom_root.path(), "kerminal-config.md", custom);
    let custom_edits =
        plan_configuration_migration(custom_root.path()).expect("自定义指南不阻断其它配置");
    assert!(edit(&custom_edits, "kerminal-config.md").is_none());
    assert_eq!(
        fs::read(custom_root.path().join("kerminal-config.md")).expect("自定义指南仍存在"),
        custom.as_bytes()
    );

    let future_root = tempdir().expect("创建未来指南配置根");
    let future_body = "# Future guide\n";
    let future_digest = configuration::config_guide_digest(future_body);
    let future = format!(
        "{}{} sha256={future_digest} -->\n{future_body}",
        configuration::CONFIG_GUIDE_MARKER_PREFIX,
        CONFIG_GUIDE_REVISION + 1
    );
    write_fixture(future_root.path(), "kerminal-config.md", &future);
    let future_edits = plan_configuration_migration(future_root.path()).expect("未来指南保留");
    assert!(edit(&future_edits, "kerminal-config.md").is_none());
    assert!(future.contains("Future guide"));
    assert_ne!(CONFIG_GUIDE, future_body);

    let revision_3_root = tempdir().expect("创建无标记 revision 3 指南根");
    write_fixture(
        revision_3_root.path(),
        "kerminal-config.md",
        REVISION_3_GUIDE,
    );
    let revision_3_edits =
        plan_configuration_migration(revision_3_root.path()).expect("识别冻结的 revision 3 正文");
    assert!(edit(&revision_3_edits, "kerminal-config.md").is_some());

    let marked_revision_3_root = tempdir().expect("创建带标记 revision 3 指南根");
    let revision_3_digest = configuration::config_guide_digest(REVISION_3_GUIDE);
    let marked_revision_3 = format!(
        "{}3 sha256={revision_3_digest} -->\n{REVISION_3_GUIDE}",
        configuration::CONFIG_GUIDE_MARKER_PREFIX
    );
    write_fixture(
        marked_revision_3_root.path(),
        "kerminal-config.md",
        &marked_revision_3,
    );
    let marked_revision_3_edits = plan_configuration_migration(marked_revision_3_root.path())
        .expect("识别带标记的 revision 3 正文");
    assert!(edit(&marked_revision_3_edits, "kerminal-config.md").is_some());
}

/// 固定摘要才证明旧生成正文的归属；Windows 换行转换不改变迁移资格，规划仍不写原件。
///
/// @author kongweiguang
#[test]
fn frozen_revision_five_guides_are_planned_for_current_upgrade() {
    const { assert!(CONFIG_GUIDE_REVISION > 5) };
    let digest = configuration::config_guide_digest(REVISION_5_GUIDE);
    assert_eq!(digest, super::CONFIG_GUIDE_REVISION_5_SHA256);

    for body in [
        REVISION_5_GUIDE.to_owned(),
        REVISION_5_GUIDE.replace('\n', "\r\n"),
    ] {
        for marked in [false, true] {
            let original = if marked {
                format!(
                    "{}5 sha256={digest} -->\n{body}",
                    configuration::CONFIG_GUIDE_MARKER_PREFIX
                )
            } else {
                body.clone()
            };
            let root = tempdir().expect("创建隔离 revision 5 指南根");
            write_fixture(root.path(), "kerminal-config.md", &original);

            let edits = plan_configuration_migration(root.path()).expect("冻结的旧指南可规划");
            let planned = edit(&edits, "kerminal-config.md").expect("已知 revision 5 有升级计划");
            assert_eq!(planned.before.as_deref(), Some(original.as_bytes()));
            assert_eq!(planned.after, configuration::managed_config_guide());
            assert_eq!(
                fs::read(root.path().join("kerminal-config.md")).expect("规划保留旧正文"),
                original.as_bytes()
            );
        }
    }
}

/// The exact shipped revision-six bytes remain the only rev-6 guide eligible for replacement.
///
/// @author kongweiguang
#[test]
fn frozen_revision_six_guides_are_planned_for_current_upgrade() {
    const { assert!(CONFIG_GUIDE_REVISION > 6) };
    let digest = configuration::config_guide_digest(REVISION_6_GUIDE);
    assert_eq!(digest, super::CONFIG_GUIDE_REVISION_6_SHA256);

    for body in [
        REVISION_6_GUIDE.to_owned(),
        REVISION_6_GUIDE.replace('\n', "\r\n"),
    ] {
        for marked in [false, true] {
            let original = if marked {
                format!(
                    "{}6 sha256={digest} -->\n{body}",
                    configuration::CONFIG_GUIDE_MARKER_PREFIX
                )
            } else {
                body.clone()
            };
            let root = tempdir().expect("创建隔离 revision 6 指南根");
            write_fixture(root.path(), "kerminal-config.md", &original);

            let edits = plan_configuration_migration(root.path()).expect("冻结的旧指南可规划");
            let planned = edit(&edits, "kerminal-config.md").expect("已知 revision 6 有升级计划");
            assert_eq!(planned.before.as_deref(), Some(original.as_bytes()));
            assert_eq!(planned.after, configuration::managed_config_guide());
            assert_eq!(
                fs::read(root.path().join("kerminal-config.md")).expect("规划保留旧正文"),
                original.as_bytes()
            );
        }
    }
}

/// 只有冻结的 rev7 正文可进入显式升级计划；CRLF 与生成标记不改变摘要归属。
///
/// @author kongweiguang
#[test]
fn frozen_revision_seven_guides_are_planned_for_current_upgrade() {
    const { assert!(CONFIG_GUIDE_REVISION > 7) };
    let digest = configuration::config_guide_digest(REVISION_7_GUIDE);
    assert_eq!(digest, super::CONFIG_GUIDE_REVISION_7_SHA256);

    for body in [
        REVISION_7_GUIDE.to_owned(),
        REVISION_7_GUIDE.replace('\n', "\r\n"),
    ] {
        for marked in [false, true] {
            let original = if marked {
                format!(
                    "{}7 sha256={digest} -->\n{body}",
                    configuration::CONFIG_GUIDE_MARKER_PREFIX
                )
            } else {
                body.clone()
            };
            let root = tempdir().expect("创建隔离 revision 7 指南根");
            write_fixture(root.path(), "kerminal-config.md", &original);

            let edits = plan_configuration_migration(root.path()).expect("冻结的旧指南可规划");
            let planned = edit(&edits, "kerminal-config.md").expect("已知 revision 7 有升级计划");
            assert_eq!(planned.before.as_deref(), Some(original.as_bytes()));
            assert_eq!(planned.after, configuration::managed_config_guide());
            assert_eq!(
                fs::read(root.path().join("kerminal-config.md")).expect("规划保留旧正文"),
                original.as_bytes()
            );
        }
    }
}

/// 仅冻结的 revision 8 正文可升级到当前指南，换行格式与可信生成标记不改变其身份。
///
/// @author kongweiguang
#[test]
fn frozen_revision_eight_guides_are_planned_for_current_upgrade() {
    const { assert!(CONFIG_GUIDE_REVISION > 8) };
    let digest = configuration::config_guide_digest(REVISION_8_GUIDE);
    assert_eq!(digest, super::CONFIG_GUIDE_REVISION_8_SHA256);

    for body in [
        REVISION_8_GUIDE.to_owned(),
        REVISION_8_GUIDE.replace('\n', "\r\n"),
    ] {
        for marked in [false, true] {
            let original = if marked {
                format!(
                    "{}8 sha256={digest} -->\n{body}",
                    configuration::CONFIG_GUIDE_MARKER_PREFIX
                )
            } else {
                body.clone()
            };
            let root = tempdir().expect("创建隔离 revision 8 指南根");
            write_fixture(root.path(), "kerminal-config.md", &original);

            let edits = plan_configuration_migration(root.path()).expect("冻结旧指南可规划");
            let planned = edit(&edits, "kerminal-config.md").expect("revision 8 有升级计划");
            assert_eq!(planned.before.as_deref(), Some(original.as_bytes()));
            assert_eq!(planned.after, configuration::managed_config_guide());
            assert_eq!(
                fs::read(root.path().join("kerminal-config.md")).expect("规划保留旧正文"),
                original.as_bytes()
            );
        }
    }
}

/// 只有冻结的 revision 9 正文可升级到当前指南，换行格式与可信生成标记不改变其身份。
///
/// @author kongweiguang
#[test]
fn frozen_revision_nine_guides_are_planned_for_current_upgrade() {
    const { assert!(CONFIG_GUIDE_REVISION > 9) };
    let digest = configuration::config_guide_digest(REVISION_9_GUIDE);
    assert_eq!(digest, super::CONFIG_GUIDE_REVISION_9_SHA256);

    for body in [
        REVISION_9_GUIDE.to_owned(),
        REVISION_9_GUIDE.replace('\n', "\r\n"),
    ] {
        for marked in [false, true] {
            let original = if marked {
                format!(
                    "{}9 sha256={digest} -->\n{body}",
                    configuration::CONFIG_GUIDE_MARKER_PREFIX
                )
            } else {
                body.clone()
            };
            let root = tempdir().expect("创建隔离 revision 9 指南根");
            write_fixture(root.path(), "kerminal-config.md", &original);

            let edits = plan_configuration_migration(root.path()).expect("冻结旧指南可规划");
            let planned = edit(&edits, "kerminal-config.md").expect("revision 9 有升级计划");
            assert_eq!(planned.before.as_deref(), Some(original.as_bytes()));
            assert_eq!(planned.after, configuration::managed_config_guide());
            assert_eq!(
                fs::read(root.path().join("kerminal-config.md")).expect("规划保留旧正文"),
                original.as_bytes()
            );
        }
    }
}

/// 只有冻结的 revision 10 正文可升级到当前指南，换行格式与可信生成标记不改变其身份。
///
/// @author kongweiguang
#[test]
fn frozen_revision_ten_guides_are_planned_for_current_upgrade() {
    const { assert!(CONFIG_GUIDE_REVISION > 10) };
    let digest = configuration::config_guide_digest(REVISION_10_GUIDE);
    assert_eq!(digest, super::CONFIG_GUIDE_REVISION_10_SHA256);

    for body in [
        REVISION_10_GUIDE.to_owned(),
        REVISION_10_GUIDE.replace('\n', "\r\n"),
    ] {
        for marked in [false, true] {
            let original = if marked {
                format!(
                    "{}10 sha256={digest} -->\n{body}",
                    configuration::CONFIG_GUIDE_MARKER_PREFIX
                )
            } else {
                body.clone()
            };
            let root = tempdir().expect("创建隔离 revision 10 指南根");
            write_fixture(root.path(), "kerminal-config.md", &original);

            let edits = plan_configuration_migration(root.path()).expect("冻结旧指南可规划");
            let planned = edit(&edits, "kerminal-config.md").expect("revision 10 有升级计划");
            assert_eq!(planned.before.as_deref(), Some(original.as_bytes()));
            assert_eq!(planned.after, configuration::managed_config_guide());
            assert_eq!(
                fs::read(root.path().join("kerminal-config.md")).expect("规划保留旧正文"),
                original.as_bytes()
            );
        }
    }
}

/// 新增应用配色规则后，只有冻结的 revision 11 正文允许规划升级，用户改写仍保留。
///
/// @author kongweiguang
#[test]
fn frozen_revision_eleven_guide_upgrades_without_overwriting_user_edits() {
    const { assert!(CONFIG_GUIDE_REVISION > 11) };
    let digest = configuration::config_guide_digest(REVISION_11_GUIDE);
    assert_eq!(digest, super::CONFIG_GUIDE_REVISION_11_SHA256);
    let normalized = REVISION_11_GUIDE.replace("\r\n", "\n").replace('\r', "\n");

    for body in [normalized.clone(), normalized.replace('\n', "\r\n")] {
        for marked in [false, true] {
            let original = if marked {
                format!(
                    "{}11 sha256={digest} -->\n{body}",
                    configuration::CONFIG_GUIDE_MARKER_PREFIX
                )
            } else {
                body.clone()
            };
            let root = tempdir().expect("创建隔离 revision 11 指南根");
            write_fixture(root.path(), "kerminal-config.md", &original);
            let edits = plan_configuration_migration(root.path()).expect("规划冻结指南升级");
            let planned = edit(&edits, "kerminal-config.md").expect("冻结正文有升级计划");
            assert_eq!(planned.before.as_deref(), Some(original.as_bytes()));
            assert_eq!(planned.after, configuration::managed_config_guide());
            assert_eq!(
                fs::read(root.path().join("kerminal-config.md")).unwrap(),
                original.as_bytes()
            );
        }
    }

    let custom = format!("{normalized}\n## User notes\nPreserve these notes.\n");
    let custom_digest = configuration::config_guide_digest(&custom);
    for original in [
        custom.clone(),
        format!(
            "{}11 sha256={custom_digest} -->\n{custom}",
            configuration::CONFIG_GUIDE_MARKER_PREFIX
        ),
    ] {
        let root = tempdir().expect("创建隔离自定义指南根");
        write_fixture(root.path(), "kerminal-config.md", &original);
        let edits = plan_configuration_migration(root.path()).expect("检查自定义指南");
        assert!(edit(&edits, "kerminal-config.md").is_none());
        assert_eq!(
            fs::read(root.path().join("kerminal-config.md")).unwrap(),
            original.as_bytes()
        );
    }
}

/// 用户复制旧标记或重算摘要都不授予覆盖权；只有被冻结的原始正文可以进入升级计划。
///
/// @author kongweiguang
#[test]
fn revision_five_user_edits_are_preserved_even_with_recomputed_marker() {
    let custom_body = format!("{REVISION_5_GUIDE}\n## User supplied notes\nKeep this text.\n");
    let custom_digest = configuration::config_guide_digest(&custom_body);
    let old_digest = configuration::config_guide_digest(REVISION_5_GUIDE);

    for original in [
        custom_body.clone(),
        format!(
            "{}5 sha256={old_digest} -->\n{custom_body}",
            configuration::CONFIG_GUIDE_MARKER_PREFIX
        ),
        format!(
            "{}5 sha256={custom_digest} -->\n{custom_body}",
            configuration::CONFIG_GUIDE_MARKER_PREFIX
        ),
    ] {
        let root = tempdir().expect("创建隔离用户指南根");
        write_fixture(root.path(), "kerminal-config.md", &original);

        let edits = plan_configuration_migration(root.path()).expect("用户指南不阻断其它迁移");
        assert!(edit(&edits, "kerminal-config.md").is_none());
        assert_eq!(
            fs::read(root.path().join("kerminal-config.md")).expect("用户正文仍存在"),
            original.as_bytes()
        );
    }
}

/// A user edit stays caller-owned even when it copies the frozen body or recomputes its marker.
///
/// @author kongweiguang
#[test]
fn revision_six_user_edits_are_preserved_even_with_recomputed_marker() {
    let custom_body = format!("{REVISION_6_GUIDE}\n## User supplied notes\nKeep this text.\n");
    let custom_digest = configuration::config_guide_digest(&custom_body);
    let old_digest = configuration::config_guide_digest(REVISION_6_GUIDE);

    for original in [
        custom_body.clone(),
        format!(
            "{}6 sha256={old_digest} -->\n{custom_body}",
            configuration::CONFIG_GUIDE_MARKER_PREFIX
        ),
        format!(
            "{}6 sha256={custom_digest} -->\n{custom_body}",
            configuration::CONFIG_GUIDE_MARKER_PREFIX
        ),
    ] {
        let root = tempdir().expect("创建隔离用户指南根");
        write_fixture(root.path(), "kerminal-config.md", &original);

        let edits = plan_configuration_migration(root.path()).expect("用户指南不阻断其它迁移");
        assert!(edit(&edits, "kerminal-config.md").is_none());
        assert_eq!(
            fs::read(root.path().join("kerminal-config.md")).expect("用户正文仍存在"),
            original.as_bytes()
        );
    }
}

/// 旧标记、正文副注或用户自行重签都不授予覆盖权，避免迁移抹掉 rev7 后的手工内容。
///
/// @author kongweiguang
#[test]
fn revision_seven_user_edits_are_preserved_even_with_recomputed_marker() {
    let custom_body = format!("{REVISION_7_GUIDE}\n## User supplied notes\nKeep this text.\n");
    let custom_digest = configuration::config_guide_digest(&custom_body);
    let old_digest = configuration::config_guide_digest(REVISION_7_GUIDE);

    for original in [
        custom_body.clone(),
        format!(
            "{}7 sha256={old_digest} -->\n{custom_body}",
            configuration::CONFIG_GUIDE_MARKER_PREFIX
        ),
        format!(
            "{}7 sha256={custom_digest} -->\n{custom_body}",
            configuration::CONFIG_GUIDE_MARKER_PREFIX
        ),
    ] {
        let root = tempdir().expect("创建隔离用户指南根");
        write_fixture(root.path(), "kerminal-config.md", &original);

        let edits = plan_configuration_migration(root.path()).expect("用户指南不阻断其它迁移");
        assert!(edit(&edits, "kerminal-config.md").is_none());
        assert_eq!(
            fs::read(root.path().join("kerminal-config.md")).expect("用户正文仍存在"),
            original.as_bytes()
        );
    }
}

/// 用户对 revision 8 正文的追加内容仍归用户所有，不能因重算标记摘要而被升级覆盖。
///
/// @author kongweiguang
#[test]
fn revision_eight_user_edits_are_preserved_even_with_recomputed_marker() {
    let custom_body = format!("{REVISION_8_GUIDE}\n## User supplied notes\nKeep this text.\n");
    let custom_digest = configuration::config_guide_digest(&custom_body);
    let old_digest = configuration::config_guide_digest(REVISION_8_GUIDE);

    for original in [
        custom_body.clone(),
        format!(
            "{}8 sha256={old_digest} -->\n{custom_body}",
            configuration::CONFIG_GUIDE_MARKER_PREFIX
        ),
        format!(
            "{}8 sha256={custom_digest} -->\n{custom_body}",
            configuration::CONFIG_GUIDE_MARKER_PREFIX
        ),
    ] {
        let root = tempdir().expect("创建隔离用户指南根");
        write_fixture(root.path(), "kerminal-config.md", &original);

        let edits = plan_configuration_migration(root.path()).expect("用户指南不阻断其它迁移");
        assert!(edit(&edits, "kerminal-config.md").is_none());
        assert_eq!(
            fs::read(root.path().join("kerminal-config.md")).expect("用户正文仍存在"),
            original.as_bytes()
        );
    }
}

/// 与旧正式 HostTomlDocument 一致地保存 camelCase SSH 子表，而不是从旧 UI 状态臆造字段。
fn legacy_ssh_options_host(options: &str) -> String {
    format!(
        concat!(
            "schema_version = 2\nprotocol = \"ssh\"\nid = \"legacy-options\"\n",
            "name = \"Legacy options\"\nhost = \"example.invalid\"\nport = 22\n",
            "username = \"tester\"\nauth_type = \"agent\"\ntags = []\n",
            "sort_order = 0\ncreated_at = \"1\"\nupdated_at = \"1\"\n{}",
        ),
        options,
    )
}

/// 显式保留旧值和注释，原无效空目录变成继承，不展开未配置的当前缺省或丢弃旧扩展。
#[test]
fn old_saved_terminal_and_transfer_options_are_migrated_to_current_fields() {
    let root = tempdir().expect("创建隔离根");
    let original = legacy_ssh_options_host(concat!(
        "[ssh_options.terminal]\n",
        "terminalType = \"screen-256color\" # saved TERM\n",
        "connectTimeoutSeconds = 45\nkeepaliveSeconds = 0\n",
        "startupCommand = \"legacy-not-automatically-executed\"\n",
        "[ssh_options.transfer]\n",
        "enabled = true\nlocalStartDirectory = \"\"\nremoteStartDirectory = \"/var/tmp\"\n",
        "preserveTimestamps = false\nfollowSymlinks = true\nmaxConcurrentTransfers = 4\n",
    ));
    write_fixture(root.path(), "hosts/legacy-options.toml", &original);
    let edits = plan_configuration_migration(root.path()).expect("旧真实选项可规划");
    let planned = edit(&edits, "hosts/legacy-options.toml").expect("旧 host 有编辑计划");
    let after = std::str::from_utf8(&planned.after).expect("当前 host 为 UTF-8");
    let value: toml::Value = toml::from_str(after).expect("当前 host 是有效 TOML");
    let terminal = &value["ssh_options"]["terminal"];
    assert_eq!(terminal["term"].as_str(), Some("screen-256color"));
    assert_eq!(terminal["connect_timeout_seconds"].as_integer(), Some(45));
    assert_eq!(terminal["keepalive_seconds"].as_integer(), Some(0));
    assert_eq!(
        terminal["startupCommand"].as_str(),
        Some("legacy-not-automatically-executed")
    );
    let sftp = &value["ssh_options"]["sftp"];
    assert_eq!(sftp["remote_start_directory"].as_str(), Some("/var/tmp"));
    assert_eq!(sftp["preserve_mtime"].as_bool(), Some(false));
    assert_eq!(sftp["follow_source_symlinks"].as_bool(), Some(true));
    assert_eq!(sftp["concurrency"].as_integer(), Some(4));
    assert!(sftp.get("local_start_directory").is_none());
    assert!(value["ssh_options"].get("transfer").is_none());
    assert!(after.contains("# saved TERM"));
    assert_eq!(planned.before.as_deref(), Some(original.as_bytes()));
    assert_eq!(
        fs::read(root.path().join("hosts/legacy-options.toml")).expect("规划不写源文件"),
        original.as_bytes()
    );
}

/// contract 1 的 schema 3 也可能保留尚未消费的旧参数，显式升级须转换而不只改 marker。
#[test]
fn schema_three_with_saved_legacy_option_names_still_receives_explicit_upgrade() {
    let root = tempdir().expect("创建隔离根");
    let original = legacy_ssh_options_host("[ssh_options.terminal]\nterminalType = \"vt100\"\n")
        .replacen("schema_version = 2", "schema_version = 3", 1);
    write_fixture(root.path(), "hosts/legacy-options.toml", &original);
    let edits = plan_configuration_migration(root.path()).expect("同代旧参数可规划");
    let after = std::str::from_utf8(
        &edit(&edits, "hosts/legacy-options.toml")
            .expect("有参数转换")
            .after,
    )
    .expect("当前 host 为 UTF-8");
    assert!(after.contains("term = \"vt100\""));
    assert!(!after.contains("terminalType"));
}

/// 冲突配置和越界并发必须阻断，不选择任意一侧，也不夹断为碰巧合法的当前值。
#[test]
fn conflicting_or_invalid_saved_host_options_are_not_silently_normalized() {
    for options in [
        "[ssh_options.terminal]\nterminalType = \"vt100\"\nterm = \"xterm\"\n",
        "[ssh_options.transfer]\nenabled = false\n[ssh_options.sftp]\nenabled = true\n",
        "[ssh_options.transfer]\nmaxConcurrentTransfers = 99\n",
    ] {
        let root = tempdir().expect("创建隔离根");
        let original = legacy_ssh_options_host(options);
        write_fixture(root.path(), "hosts/legacy-options.toml", &original);
        assert!(plan_configuration_migration(root.path()).is_err());
        assert_eq!(
            fs::read(root.path().join("hosts/legacy-options.toml")).expect("源配置保留"),
            original.as_bytes()
        );
    }
}

/// 新增 VS Code 配色规则后，只有冻结的 revision 12 正文允许规划升级，用户改写仍保留。
///
/// @author kongweiguang
#[test]
fn frozen_revision_twelve_guide_upgrades_without_overwriting_user_edits() {
    const { assert!(CONFIG_GUIDE_REVISION > 12) };
    let digest = configuration::config_guide_digest(REVISION_12_GUIDE);
    assert_eq!(digest, super::CONFIG_GUIDE_REVISION_12_SHA256);
    let normalized = REVISION_12_GUIDE.replace("\r\n", "\n").replace('\r', "\n");

    for body in [normalized.clone(), normalized.replace('\n', "\r\n")] {
        for marked in [false, true] {
            let original = if marked {
                format!(
                    "{}12 sha256={digest} -->\n{body}",
                    configuration::CONFIG_GUIDE_MARKER_PREFIX
                )
            } else {
                body.clone()
            };
            let root = tempdir().expect("创建隔离 revision 12 指南根");
            write_fixture(root.path(), "kerminal-config.md", &original);
            let edits = plan_configuration_migration(root.path()).expect("规划冻结指南升级");
            let planned = edit(&edits, "kerminal-config.md").expect("冻结正文有升级计划");
            assert_eq!(planned.before.as_deref(), Some(original.as_bytes()));
            assert_eq!(planned.after, configuration::managed_config_guide());
            assert_eq!(
                fs::read(root.path().join("kerminal-config.md")).unwrap(),
                original.as_bytes()
            );
        }
    }

    let custom = format!("{normalized}\n## User notes\nPreserve these notes.\n");
    let custom_digest = configuration::config_guide_digest(&custom);
    for original in [
        custom.clone(),
        format!(
            "{}12 sha256={custom_digest} -->\n{custom}",
            configuration::CONFIG_GUIDE_MARKER_PREFIX
        ),
    ] {
        let root = tempdir().expect("创建隔离自定义指南根");
        write_fixture(root.path(), "kerminal-config.md", &original);
        let edits = plan_configuration_migration(root.path()).expect("检查自定义指南");
        assert!(edit(&edits, "kerminal-config.md").is_none());
        assert_eq!(
            fs::read(root.path().join("kerminal-config.md")).unwrap(),
            original.as_bytes()
        );
    }
}
