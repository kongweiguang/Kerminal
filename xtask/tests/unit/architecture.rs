//! Architecture-gate regression cases for renamed dependencies and duplicate lock entries.
//!
//! @author kongweiguang

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::json;

use super::{
    EXPECTED_WORKSPACE_PACKAGES, dependency_names, is_external_test_path,
    is_legacy_runtime_package, validate_forbidden_lock_packages, validate_internal_edges,
    validate_legacy_web_inputs, validate_locked_version, validate_source_test_policy,
    validate_ui_resource_sources, validate_workspace_package_set,
};

static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

/// 为文件系统架构规则创建并行安全的临时样本目录，不依赖额外测试 crate。
fn fixture_root(label: &str) -> PathBuf {
    let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "kerminal-architecture-{label}-{}-{id}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("fixture root should be created");
    path
}

/// 创建真实文件供路径归属和递归扫描规则验证，避免只测试字符串判定。
fn write_fixture(root: &Path, relative: &str, contents: &str) -> PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("fixture path should have a parent"))
        .expect("fixture parents should be created");
    fs::write(&path, contents).expect("fixture file should be written");
    path
}

/// Cargo package renames must preserve both the real package name and the local dependency key.
#[test]
fn dependency_names_include_package_and_rename_but_skip_dev_dependencies() {
    let package = json!({
        "dependencies": [
            { "name": "bssh-russh-sftp", "rename": "russh_sftp", "kind": "normal" },
            { "name": "justelnet-core", "rename": "telnet", "kind": null },
            { "name": "test-only-crate", "kind": "dev" }
        ]
    });

    let names = dependency_names(&package);

    assert_eq!(
        names,
        BTreeSet::from([
            "bssh-russh-sftp".to_owned(),
            "justelnet-core".to_owned(),
            "russh_sftp".to_owned(),
            "telnet".to_owned(),
        ])
    );
}

/// 验证包集合门禁同时拒绝缺失包与未经划分的新增包。
#[test]
fn workspace_package_set_rejects_missing_and_unexpected_crates() {
    let exact = EXPECTED_WORKSPACE_PACKAGES
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<BTreeSet<_>>();
    let mut violations = Vec::new();
    validate_workspace_package_set(&exact, &mut violations);
    assert!(violations.is_empty());

    let mut expanded = exact.clone();
    expanded.insert("kerminal-unowned-addon".to_owned());
    validate_workspace_package_set(&expanded, &mut violations);
    assert!(
        violations.contains(&"unexpected workspace package: kerminal-unowned-addon".to_owned())
    );

    let mut reduced = exact;
    reduced.remove("xtask");
    let mut missing = Vec::new();
    validate_workspace_package_set(&reduced, &mut missing);
    assert!(missing.contains(&"missing workspace package: xtask".to_owned()));
}

/// UI may depend on CancellationToken's pure utility crate while Tokio runtime remains forbidden.
#[test]
fn ui_allows_tokio_util_but_rejects_tokio_runtime_dependency() {
    let mut violations = Vec::new();
    validate_internal_edges(
        "kerminal-ui",
        &BTreeSet::from(["tokio-util".to_owned()]),
        &mut violations,
    );
    assert!(violations.is_empty());

    validate_internal_edges(
        "kerminal-ui",
        &BTreeSet::from(["tokio".to_owned()]),
        &mut violations,
    );
    assert_eq!(
        violations,
        vec!["kerminal-ui has forbidden dependency tokio".to_owned()]
    );
}

/// Real package names remain blocked even when Cargo metadata reports a renamed dependency key.
#[test]
fn ui_rejects_actual_sftp_telnet_and_serial_package_names() {
    let deps = BTreeSet::from([
        "bssh-russh-sftp".to_owned(),
        "justelnet-core".to_owned(),
        "tokio-serial".to_owned(),
    ]);
    let mut violations = Vec::new();

    validate_internal_edges("kerminal-ui", &deps, &mut violations);

    assert_eq!(violations.len(), 3);
}

/// 验证 GPUI 工具依赖可用，同时阻止 UI 直接引入资源 SDK。
#[test]
fn ui_rejects_direct_resource_sdks_but_keeps_gpui_and_pure_utilities() {
    let allowed = BTreeSet::from([
        "gpui-kit".to_owned(),
        "gpui-pre".to_owned(),
        "tokio-util".to_owned(),
        "url".to_owned(),
    ]);
    let mut violations = Vec::new();
    validate_internal_edges("kerminal-ui", &allowed, &mut violations);
    assert!(violations.is_empty());

    let forbidden = BTreeSet::from([
        "dirs".to_owned(),
        "reqwest".to_owned(),
        "windows-sys".to_owned(),
    ]);
    validate_internal_edges("kerminal-ui", &forbidden, &mut violations);
    assert_eq!(violations.len(), forbidden.len());
}

/// 验证仅桌面组合根可使用启动早期致命错误对话框，UI 的资源 SDK 禁令保持有效。
#[test]
fn desktop_allows_only_the_startup_fatal_dialog_dependency() {
    let mut violations = Vec::new();
    validate_internal_edges(
        "kerminal-desktop",
        &BTreeSet::from(["rfd".to_owned()]),
        &mut violations,
    );
    assert!(violations.is_empty());

    validate_internal_edges(
        "kerminal-desktop",
        &BTreeSet::from(["rfd".to_owned(), "rusqlite".to_owned()]),
        &mut violations,
    );
    assert_eq!(
        violations,
        vec!["kerminal-desktop has forbidden dependency rusqlite".to_owned()]
    );

    let mut ui_violations = Vec::new();
    validate_internal_edges(
        "kerminal-ui",
        &BTreeSet::from(["rfd".to_owned()]),
        &mut ui_violations,
    );
    assert_eq!(
        ui_violations,
        vec!["kerminal-ui has forbidden dependency rfd".to_owned()]
    );
}

/// 验证资源 API 被拒绝，GPUI 剪贴板和纯地址类型仍可正常使用。
#[test]
fn ui_source_gate_rejects_resource_apis_and_allows_framework_calls() {
    let root = fixture_root("ui-api");
    let manifest = write_fixture(&root, "Cargo.toml", "[package]\nname='kerminal-ui'\n");
    write_fixture(
        &root,
        "src/framework.rs",
        "// std::fs::write is only mentioned in this comment\nuse std::net::IpAddr;\nfn copy(window: &mut Window) { window.write_to_clipboard(item); }\n",
    );
    let package = json!({"manifest_path": manifest});
    let mut violations = Vec::new();
    validate_ui_resource_sources(&package, &mut violations);
    assert!(violations.is_empty());

    write_fixture(
        &root,
        "src/io.rs",
        "use tokio::process::Command;\nfn connect() { let _ = std::net::TcpStream::connect(\"localhost:1\"); }\n",
    );
    validate_ui_resource_sources(&package, &mut violations);
    assert_eq!(violations.len(), 2);
    assert!(
        violations
            .iter()
            .any(|line| line.contains("filesystem/process"))
    );
    assert!(
        violations
            .iter()
            .any(|line| line.contains("network socket"))
    );
    let _ = fs::remove_dir_all(root);
}

/// 验证 Cargo.lock 不再包含旧运行栈，同时保留独立 Windows 通知后端。
#[test]
fn legacy_runtime_lock_packages_are_rejected_with_one_platform_exception() {
    let lock: toml::Value = toml::from_str(
        r#"
[[package]]
name = "tauri"
version = "2.0.0"

[[package]]
name = "tauri-plugin-dialog"
version = "2.0.0"

[[package]]
name = "tauri-winrt-notification"
version = "0.8.1"

[[package]]
name = "react-dom"
version = "18.0.0"

[[package]]
name = "xterm-addon-fit"
version = "0.8.0"

[[package]]
name = "monaco-editor"
version = "0.50.0"
"#,
    )
    .expect("synthetic lockfile should be valid TOML");
    let packages = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .expect("synthetic lockfile should have package entries");
    let mut violations = Vec::new();

    validate_forbidden_lock_packages(packages, &mut violations);

    assert_eq!(violations.len(), 5);
    assert!(!is_legacy_runtime_package("tauri-winrt-notification"));
    assert!(is_legacy_runtime_package("tauri-plugin-dialog"));
}

/// 验证递归产品扫描能发现嵌套入口并排除样例、供应链与文档资源。
#[test]
fn legacy_web_scan_is_recursive_and_keeps_non_product_assets() {
    let root = fixture_root("legacy-web");
    write_fixture(&root, "crates/kerminal-ui/src/nested/old.tsx", "export {};");
    write_fixture(&root, "tests/fixtures/package.json", "{}");
    write_fixture(&root, "vendor/gpui-pre/src/example.ts", "export {};");
    write_fixture(&root, "docs/assets/example.html", "<main />");
    write_fixture(&root, "licenses/THIRD_PARTY_DEPENDENCIES.html", "<main />");
    write_fixture(&root, "target/old/index.html", "<main />");
    let mut violations = Vec::new();

    validate_legacy_web_inputs(&root, &mut violations);

    assert_eq!(violations.len(), 1);
    assert!(violations[0].contains("crates"));
    assert!(violations[0].contains("old.tsx"));
    let _ = fs::remove_dir_all(root);
}

/// 固定 17c 回读 harness 可使用旧 workspace decoder，但同目录普通 Web 入口仍须被拒绝。
#[test]
fn legacy_web_scan_allows_only_the_pinned_workspace_decoder_helper() {
    let root = fixture_root("legacy-readback-decoder");
    write_fixture(
        &root,
        "scripts/acceptance/legacy-readback/legacy_workspace_readback.mjs",
        "const decoded = decodeWorkspaceSessionSnapshot(source);\nassert.equal(source.version, 3);\nassert.equal(decoded.session.activeTabId, expected.workspace.activeTabId);\n",
    );
    write_fixture(
        &root,
        "scripts/acceptance/legacy-readback/ordinary_entry.mjs",
        "export {};\n",
    );
    write_fixture(
        &root,
        "scripts/acceptance/legacy-readback.ps1",
        "$script:BaselineCommit = '17c5d4be46172ca508f92546bfae5fe5ef477f11'\n$nodeTest = Join-Path $PSScriptRoot 'legacy-readback\\legacy_workspace_readback.mjs'\n& $nodeExe $nodeTest\n",
    );

    let mut violations = Vec::new();
    validate_legacy_web_inputs(&root, &mut violations);

    assert_eq!(violations.len(), 1);
    assert!(violations[0].contains("ordinary_entry.mjs"));
    assert!(!violations[0].contains("legacy_workspace_readback.mjs"));

    write_fixture(
        &root,
        "scripts/acceptance/legacy-readback.ps1",
        "$script:BaselineCommit = 'different-commit'\n$nodeTest = Join-Path $PSScriptRoot 'legacy-readback\\legacy_workspace_readback.mjs'\n& $nodeExe $nodeTest\n",
    );
    violations.clear();
    validate_legacy_web_inputs(&root, &mut violations);

    assert_eq!(violations.len(), 2);
    assert!(
        violations
            .iter()
            .any(|line| line.contains("ordinary_entry.mjs"))
    );
    assert!(
        violations
            .iter()
            .any(|line| line.contains("legacy_workspace_readback.mjs"))
    );

    let _ = fs::remove_dir_all(root);
}

/// 验证测试实现留在外置文件，并识别平台门控下的异步测试宏。
#[test]
fn source_test_gate_rejects_inline_helpers_and_tokio_tests() {
    let root = fixture_root("test-policy");
    let source = write_fixture(&root, "src/lib.rs", "");
    write_fixture(&root, "tests/unit/cases.rs", "");
    assert!(is_external_test_path(
        &root,
        &source,
        "../tests/unit/cases.rs"
    ));
    assert!(!is_external_test_path(&root, &source, "../src/lib.rs"));

    let valid_module = "#[cfg(test)]\n#[path = \"../tests/unit/cases.rs\"]\nmod tests;\n#[cfg(not(test))]\nfn product_code() {}\n#[cfg(test)]\nuse crate::PrivateType;\n";
    let mut violations = Vec::new();
    validate_source_test_policy("fixture", &root, &source, valid_module, &mut violations);
    assert!(violations.is_empty());

    let invalid_items = "#[cfg(test)]\nfn helper() {}\n#[cfg(windows)]\n#[tokio::test]\nasync fn async_case() {}\n#[cfg(test)]\nmod inline { fn body() {} }\n";
    validate_source_test_policy("fixture", &root, &source, invalid_items, &mut violations);
    assert_eq!(violations.len(), 3);
    assert!(
        violations
            .iter()
            .any(|line| line.contains("test-only source item"))
    );
    assert!(
        violations
            .iter()
            .any(|line| line.contains("inline test function"))
    );
    assert!(
        violations
            .iter()
            .any(|line| line.contains("src may keep only"))
    );
    let _ = fs::remove_dir_all(root);
}

/// 区分测试专属项与仍可用于目标平台生产构建的跨平台解析器。
#[test]
fn source_test_gate_allows_platform_items_also_built_for_tests() {
    let root = fixture_root("test-platform-cfg");
    let source = write_fixture(&root, "src/lib.rs", "");
    let production_items = "#[cfg(any(target_os = \"windows\", test))]\nfn windows_parser() {}\n#[cfg(all(any(test, target_os = \"linux\"), not(target_os = \"windows\")))]\nfn linux_parser() {}\n#[cfg(not(test))]\nfn product_code() {}\n";
    let mut violations = Vec::new();

    validate_source_test_policy("fixture", &root, &source, production_items, &mut violations);

    assert!(violations.is_empty());
    let _ = fs::remove_dir_all(root);
}

/// 继续拒绝组合属性中所有只可能在 test 构建启用的实现项。
#[test]
fn source_test_gate_rejects_nested_test_only_cfg_items() {
    let root = fixture_root("test-nested-cfg");
    let source = write_fixture(&root, "src/lib.rs", "");
    let test_only_items = "#[cfg(test)]\nfn unit_helper() {}\n#[cfg(all(test, target_os = \"windows\"))]\nfn windows_test_helper() {}\n#[cfg(any(all(test, target_os = \"windows\"), all(test, target_os = \"linux\")))]\nfn portable_test_helper() {}\n#[cfg(not(any(not(test), target_os = \"windows\")))]\nfn nested_not_test_helper() {}\n";
    let mut violations = Vec::new();

    validate_source_test_policy("fixture", &root, &source, test_only_items, &mut violations);

    assert_eq!(violations.len(), 4);
    assert!(
        violations
            .iter()
            .all(|line| line.contains("test-only source item"))
    );
    let _ = fs::remove_dir_all(root);
}

/// 验证外置模块路径必须真实落在当前 crate 的 tests 子树。
#[test]
fn source_test_gate_rejects_external_module_paths_outside_tests() {
    let root = fixture_root("test-path");
    let source = write_fixture(&root, "src/lib.rs", "");
    write_fixture(&root, "src/test_impl.rs", "");
    let mut violations = Vec::new();

    validate_source_test_policy(
        "fixture",
        &root,
        &source,
        "#[cfg(test)]\n#[path = \"test_impl.rs\"]\nmod tests;\n",
        &mut violations,
    );

    assert_eq!(violations.len(), 1);
    let _ = fs::remove_dir_all(root);
}

/// Version equality alone is insufficient when Cargo resolves a package from two distinct sources.
#[test]
fn duplicate_same_version_lock_entries_are_rejected() {
    let lock: toml::Value = toml::from_str(
        r#"
[[package]]
name = "gpui-kit"
version = "0.7.3"
source = "registry+one"

[[package]]
name = "gpui-kit"
version = "0.7.3"
source = "git+two"
"#,
    )
    .expect("synthetic lockfile should be valid TOML");
    let packages = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .expect("synthetic lockfile should have package entries");
    let mut violations = Vec::new();

    validate_locked_version(packages, "gpui-kit", "0.7.3", &mut violations);

    assert_eq!(violations.len(), 1);
}
