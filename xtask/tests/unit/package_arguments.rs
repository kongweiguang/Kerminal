// @author kongweiguang

use super::*;
use std::{fs, path::PathBuf};

/// 独占一个带随机后缀的临时工作区，避免测试创建或删除真实仓库文件。
struct TemporaryPackageRoot(PathBuf);

impl TemporaryPackageRoot {
    /// 使用进程号和高精度时间构造隔离目录，不改进程级环境变量。
    fn create() -> Self {
        let root = std::env::temp_dir().join(format!(
            "kerminal-package-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock is after Unix epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("create package config test root");
        Self(root)
    }
}

impl Drop for TemporaryPackageRoot {
    /// 测试退出时只清理刚才创建的唯一临时根。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 静态平台配置把两个 wrapper 放入同一资源目录，便于脚本按自身位置找到 Rust helper。
///
/// @author kongweiguang
#[test]
fn platform_configs_package_migration_wrappers_together() {
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask crate is under the workspace root");
    let expected = [
        ("kerminal-migrate.ps1", "migration/kerminal-migrate.ps1"),
        ("kerminal-migrate.sh", "migration/kerminal-migrate.sh"),
    ];

    for config_name in [
        "Packager.windows.toml",
        "Packager.macos.toml",
        "Packager.linux.toml",
    ] {
        let config: toml::Value = toml::from_str(
            &fs::read_to_string(workspace.join("packaging").join(config_name))
                .expect("read platform package config"),
        )
        .expect("parse platform package config");
        let resources = config
            .get("resources")
            .and_then(toml::Value::as_array)
            .expect("platform resource list");

        for (source, target) in expected {
            assert!(
                resources.iter().any(|resource| {
                    resource.get("src").and_then(toml::Value::as_str) == Some(source)
                        && resource.get("target").and_then(toml::Value::as_str) == Some(target)
                }),
                "{config_name} must map {source} to {target}"
            );
        }
    }
}

/// 每个受支持 target 使用自己的 Rust release helper 文件名，且安装位置与 wrapper 约定一致。
///
/// @author kongweiguang
#[test]
fn migration_helper_resource_name_matches_target_platform() {
    let temporary_root = TemporaryPackageRoot::create();
    let release_directory = temporary_root.0.join("release");
    fs::create_dir_all(&release_directory).expect("create release fixture directory");

    for (target, binary_name, package_path) in [
        (
            "x86_64-pc-windows-msvc",
            "kerminal-migrate.exe",
            "migration/kerminal-migrate.exe",
        ),
        (
            "x86_64-unknown-linux-gnu",
            "kerminal-migrate",
            "migration/kerminal-migrate",
        ),
        (
            "x86_64-apple-darwin",
            "kerminal-migrate",
            "migration/kerminal-migrate",
        ),
        (
            "aarch64-apple-darwin",
            "kerminal-migrate",
            "migration/kerminal-migrate",
        ),
    ] {
        fs::write(release_directory.join(binary_name), "fixture helper")
            .expect("write target helper fixture");
        let mut config = toml::map::Map::new();
        config.insert("resources".to_owned(), toml::Value::Array(Vec::new()));

        command::append_migration_helper_resource(&mut config, target, &release_directory)
            .expect("append target helper resource");
        let resources = config
            .get("resources")
            .and_then(toml::Value::as_array)
            .expect("helper resource list");
        assert_eq!(resources.len(), 1);
        assert_eq!(
            resources[0].get("src").and_then(toml::Value::as_str),
            Some(
                release_directory
                    .join(binary_name)
                    .to_str()
                    .expect("fixture path is UTF-8")
            )
        );
        assert_eq!(
            resources[0].get("target").and_then(toml::Value::as_str),
            Some(package_path)
        );

        fs::remove_file(release_directory.join(binary_name))
            .expect("remove helper before the next target case");
    }
}

/// Windows 包装固定到 MSVC 主机目标，避免从开发者机器架构推断包格式。
#[test]
fn windows_target_selects_windows_configuration() {
    assert_eq!(
        command::packaging_config("x86_64-pc-windows-msvc").unwrap(),
        "Packager.windows.toml"
    );
}

/// macOS 两种受支持架构共用应用格式配置，架构由显式 target 决定。
#[test]
fn macos_targets_select_macos_configuration() {
    for target in ["x86_64-apple-darwin", "aarch64-apple-darwin"] {
        assert_eq!(
            command::packaging_config(target).unwrap(),
            "Packager.macos.toml"
        );
    }
}

/// 未列入交付矩阵的 triple 必须在启动 cargo-packager 前失败。
#[test]
fn unsupported_targets_are_rejected() {
    assert!(command::packaging_config("x86_64-pc-windows-gnu").is_err());
}

/// 打包前必须按显式 target 构建锁定的 release 迁移程序，不能依赖磁盘上残留的 helper。
///
/// @author kongweiguang
#[test]
fn migration_helper_build_uses_selected_locked_release_binary() {
    assert_eq!(
        command::migration_helper_build_arguments("x86_64-pc-windows-msvc").unwrap(),
        vec![
            "--locked",
            "build",
            "--release",
            "--package",
            "kerminal-desktop",
            "--bin",
            "kerminal-migrate",
            "--target",
            "x86_64-pc-windows-msvc",
        ]
    );
    assert!(command::migration_helper_build_arguments("unsupported-target").is_err());
}

/// package 需要显式 triple，防止本地宿主机差异悄悄改变目标格式。
#[test]
fn package_requires_explicit_target_argument() {
    assert!(parse_package_target(&["package".into()]).is_err());
    assert_eq!(
        parse_package_target(&[
            "package".into(),
            "--target".into(),
            "x86_64-unknown-linux-gnu".into(),
        ])
        .unwrap(),
        "x86_64-unknown-linux-gnu"
    );
}

/// 本地 package 命令必须把真实 target 与工作区路径传给候选收集脚本。
#[test]
fn package_collector_receives_explicit_action_target_and_workspace() {
    let temporary_root = TemporaryPackageRoot::create();
    let root = &temporary_root.0;
    fs::create_dir_all(root.join("packaging")).expect("create packaging directory");
    fs::write(root.join("packaging/collect_candidates.py"), "# fixture")
        .expect("write collector fixture");
    let workspace = root.canonicalize().expect("canonical fixture root");

    let invocation = command::package_candidate_collector_command(
        &workspace,
        "x86_64-unknown-linux-gnu",
        "collect",
    )
    .expect("build package collector command");
    let arguments: Vec<String> = invocation
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();

    assert_eq!(invocation.get_program().to_string_lossy(), "python");
    assert_eq!(invocation.get_current_dir(), Some(workspace.as_path()));
    assert_eq!(
        arguments,
        vec![
            workspace
                .join("packaging/collect_candidates.py")
                .display()
                .to_string(),
            "collect".to_owned(),
            "--target".to_owned(),
            "x86_64-unknown-linux-gnu".to_owned(),
            "--workspace".to_owned(),
            workspace.display().to_string(),
        ]
    );
}

/// 未知动作或缺失脚本在启动子进程前失败，不把遗漏候选收集误报为成功。
#[test]
fn package_collector_rejects_missing_script_and_unknown_action() {
    let temporary_root = TemporaryPackageRoot::create();
    let root = temporary_root
        .0
        .canonicalize()
        .expect("canonical fixture root");

    assert!(
        command::package_candidate_collector_command(&root, "x86_64-pc-windows-msvc", "collect")
            .is_err()
    );
    assert!(
        command::package_candidate_collector_command(&root, "x86_64-pc-windows-msvc", "skip")
            .is_err()
    );
}

/// The packaging gate must reject a globally installed or otherwise unpinned cargo-about binary.
#[test]
fn cargo_about_version_is_pinned() {
    assert!(command::is_expected_cargo_about_version(
        "cargo-about 0.9.2\n"
    ));
    assert!(!command::is_expected_cargo_about_version(
        "cargo-about 0.9.1\n"
    ));
    assert!(!command::is_expected_cargo_about_version(
        "cargo-about 0.9.2 (custom)\n"
    ));
}

/// A successful process exit alone is insufficient; the generated package resource must be complete HTML.
#[test]
fn dependency_report_requires_license_sections() {
    let valid_report = "<!doctype html><title>Kerminal third-party dependency licenses</title><h2>Dependency notices and license texts</h2><section id=\"mit-0\">MIT</section>";
    assert!(command::validate_third_party_report(valid_report).is_ok());
    assert!(command::validate_third_party_report("<html></html>").is_err());
}

/// 验证同一 release target 的桌面主程序、迁移 helper 和 wrappers 被映射到包内固定路径。
///
/// @author kongweiguang
#[test]
fn generated_packager_config_resolves_workspace_inputs_and_target_binary() {
    let temporary_root = TemporaryPackageRoot::create();
    let root = &temporary_root.0;
    fs::create_dir_all(root.join("packaging")).expect("create packaging config directory");
    fs::create_dir_all(root.join("resources")).expect("create resource directory");
    fs::create_dir_all(root.join("licenses")).expect("create licenses directory");
    fs::create_dir_all(root.join("target/package-output")).expect("create output directory");

    fs::write(
        root.join("Cargo.toml"),
        "[workspace.package]\nversion = \"0.4.0-dev.0\"\n",
    )
    .expect("write workspace version");
    fs::write(root.join("LICENSE"), "license").expect("write license fixture");
    fs::write(root.join("resources/icon.ico"), "icon").expect("write icon fixture");
    fs::write(root.join("THIRD_PARTY_NOTICES.md"), "notices").expect("write notices fixture");
    fs::write(
        root.join("licenses/THIRD_PARTY_DEPENDENCIES.html"),
        "<!doctype html>",
    )
    .expect("write dependency report fixture");
    fs::write(
        root.join("licenses/GPUI_KIT_LUCIDE_LICENSE.txt"),
        "Lucide license",
    )
    .expect("write Lucide license fixture");
    fs::write(
        root.join("packaging/kerminal-migrate.ps1"),
        "# PowerShell wrapper",
    )
    .expect("write Windows migration wrapper fixture");
    fs::write(root.join("packaging/kerminal-migrate.sh"), "#!/bin/sh\n")
        .expect("write Unix migration wrapper fixture");
    let external_target = TemporaryPackageRoot::create();
    let release_directory = external_target.0.join("x86_64-pc-windows-msvc/release");
    fs::create_dir_all(&release_directory).expect("create target release directory");
    fs::write(release_directory.join("kerminal.exe"), "executable")
        .expect("write built binary fixture");
    fs::write(
        release_directory.join("kerminal-migrate.exe"),
        "migration helper",
    )
    .expect("write built migration helper fixture");
    fs::write(
        root.join("packaging/Packager.windows.toml"),
        r#"
product-name = "Kerminal"
license-file = "../LICENSE"
icons = ["../resources/icon.ico"]
resources = [
  { src = "../THIRD_PARTY_NOTICES.md", target = "licenses/THIRD_PARTY_NOTICES.md" },
  { src = "../licenses/THIRD_PARTY_DEPENDENCIES.html", target = "licenses/THIRD_PARTY_DEPENDENCIES.html" },
  { src = "../licenses/GPUI_KIT_LUCIDE_LICENSE.txt", target = "licenses/GPUI_KIT_LUCIDE_LICENSE.txt" },
  { src = "kerminal-migrate.ps1", target = "migration/kerminal-migrate.ps1" },
  { src = "kerminal-migrate.sh", target = "migration/kerminal-migrate.sh" },
]

[[binaries]]
path = "kerminal"
main = true

[[deep-link-protocols]]
schemes = ["kerminal"]
role = "viewer"
"#,
    )
    .expect("write platform config fixture");

    let prepared = command::prepare_packager_config_at(
        root,
        "x86_64-pc-windows-msvc",
        "Packager.windows.toml",
        &release_directory,
    )
    .expect("prepare absolute packager config");
    let generated: toml::Value =
        toml::from_str(&fs::read_to_string(&prepared.path).expect("read generated config"))
            .expect("parse generated config");
    let canonical_root = root.canonicalize().expect("canonical test root");
    let absolute = |relative: &str| {
        canonical_root
            .join(relative)
            .canonicalize()
            .expect("canonical fixture path")
            .to_string_lossy()
            .into_owned()
    };

    assert_eq!(
        generated.get("version").and_then(toml::Value::as_str),
        Some("0.4.0-dev.0")
    );
    assert_eq!(
        generated.get("name").and_then(toml::Value::as_str),
        Some("kerminal-desktop")
    );
    assert_eq!(
        generated.get("out-dir").and_then(toml::Value::as_str),
        Some(absolute("target/package-output").as_str())
    );
    assert_eq!(
        generated.get("binaries-dir").and_then(toml::Value::as_str),
        Some(
            release_directory
                .canonicalize()
                .expect("canonical external release directory")
                .to_str()
                .expect("release directory is UTF-8")
        )
    );
    assert_eq!(
        generated.get("license-file").and_then(toml::Value::as_str),
        Some(absolute("LICENSE").as_str())
    );
    assert_eq!(
        generated
            .get("icons")
            .and_then(toml::Value::as_array)
            .and_then(|icons| icons.first())
            .and_then(toml::Value::as_str),
        Some(absolute("resources/icon.ico").as_str())
    );
    assert_eq!(
        generated
            .get("resources")
            .and_then(toml::Value::as_array)
            .and_then(|resources| resources.first())
            .and_then(|resource| resource.get("src"))
            .and_then(toml::Value::as_str),
        Some(absolute("THIRD_PARTY_NOTICES.md").as_str())
    );
    assert_eq!(
        generated
            .get("resources")
            .and_then(toml::Value::as_array)
            .and_then(|resources| resources.get(2))
            .and_then(|resource| resource.get("src"))
            .and_then(toml::Value::as_str),
        Some(absolute("licenses/GPUI_KIT_LUCIDE_LICENSE.txt").as_str())
    );
    assert_eq!(
        generated
            .get("resources")
            .and_then(toml::Value::as_array)
            .and_then(|resources| resources.get(2))
            .and_then(|resource| resource.get("target"))
            .and_then(toml::Value::as_str),
        Some("licenses/GPUI_KIT_LUCIDE_LICENSE.txt")
    );
    assert_eq!(
        generated
            .get("binaries")
            .and_then(toml::Value::as_array)
            .and_then(|binaries| binaries.first())
            .and_then(|binary| binary.get("path"))
            .and_then(toml::Value::as_str),
        Some("kerminal")
    );
    let resources = generated
        .get("resources")
        .and_then(toml::Value::as_array)
        .expect("generated resources");
    for (target, source) in [
        (
            "migration/kerminal-migrate.ps1",
            "packaging/kerminal-migrate.ps1",
        ),
        (
            "migration/kerminal-migrate.sh",
            "packaging/kerminal-migrate.sh",
        ),
    ] {
        let resource = resources
            .iter()
            .find(|resource| resource.get("target").and_then(toml::Value::as_str) == Some(target))
            .expect("migration resource mapping");
        assert_eq!(
            resource.get("src").and_then(toml::Value::as_str),
            Some(absolute(source).as_str())
        );
    }
    let helper_resource = resources
        .iter()
        .find(|resource| {
            resource.get("target").and_then(toml::Value::as_str)
                == Some("migration/kerminal-migrate.exe")
        })
        .expect("target migration helper mapping");
    assert_eq!(
        helper_resource.get("src").and_then(toml::Value::as_str),
        Some(
            release_directory
                .join("kerminal-migrate.exe")
                .canonicalize()
                .expect("canonical helper in external target directory")
                .to_str()
                .expect("helper path is UTF-8")
        )
    );
    assert_eq!(
        generated
            .get("deep-link-protocols")
            .and_then(toml::Value::as_array)
            .and_then(|protocols| protocols.first())
            .and_then(|protocol| protocol.get("schemes"))
            .and_then(toml::Value::as_array)
            .and_then(|schemes| schemes.first())
            .and_then(toml::Value::as_str),
        Some("kerminal")
    );

    let generated_path = prepared.path.clone();
    drop(prepared);
    assert!(
        !generated_path.exists(),
        "temporary config is removed on drop"
    );

    fs::remove_file(release_directory.join("kerminal-migrate.exe"))
        .expect("remove helper fixture for fail-closed check");
    let error = match command::prepare_packager_config_at(
        root,
        "x86_64-pc-windows-msvc",
        "Packager.windows.toml",
        &release_directory,
    ) {
        Ok(_) => panic!("packaging must reject a missing target helper"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("migration helper is missing"),
        "missing helper should be diagnosed at its exact release path"
    );
}
