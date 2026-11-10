//! 从作者命令到协议校验的回归，使用临时项目避免触碰真实配置。
//! @author kongweiguang

use std::{
    fs,
    io::Cursor,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use flate2::{Compression, write::GzEncoder};
use kerminal_plugin_cli::{archive, conformance};
use kerminal_plugin_protocol::{ManifestPolicy, Platform};
use serde_json::json;

/// 命令继承工具链但固定当前项目，确保不存在对用户配置根的隐式依赖。
fn cli(root: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_kerminal-plugin"))
        .current_dir(root)
        .args(arguments)
        .output()
        .expect("CLI should start")
}

struct TestProcess(Child);

impl Drop for TestProcess {
    /// 即使断言失败也清理测试启动的 CLI，由它的进程组释放模拟桌面。
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
/// 原生模板经内嵌 esbuild、静态视图生成、宿主校验和归档形成可重复的闭环。
fn native_template_builds_checks_and_packages() {
    let directory = tempfile::tempdir().unwrap();
    let created = cli(directory.path(), &["create", "acme.notes"]);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let project = directory.path().join("acme.notes");
    assert!(project.join(".github/workflows/release.yml").is_file());
    let built = cli(&project, &["build"]);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert!(project.join(".kerminal/build/dist/main.js").is_file());
    assert!(project.join(".kerminal/build/views/panel.json").is_file());
    let checked = cli(&project, &["check"]);
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    let packed = cli(&project, &["package"]);
    assert!(
        packed.status.success(),
        "{}",
        String::from_utf8_lossy(&packed.stderr)
    );
    let package = project.join(".kerminal/packages/acme.notes-0.1.0-any.kplugin");
    let checked = cli(&project, &["check", package.to_str().unwrap()]);
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    assert!(!cli(&project, &["package"]).status.success());
    let renamed = project.join("renamed.kplugin");
    fs::copy(&package, &renamed).unwrap();
    assert!(
        !cli(
            &project,
            &["check", renamed.to_str().unwrap(), "--platform", "any"]
        )
        .status
        .success()
    );
}

#[test]
/// 二进制分发须能直接展示内嵌 esbuild 的版权、许可和固定来源。
fn embedded_esbuild_license_is_accessible() {
    let directory = tempfile::tempdir().unwrap();
    let output = cli(directory.path(), &["--licenses"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("esbuild 0.28.2"));
    assert!(text.contains("Copyright (c) 2020 Evan Wallace"));
    assert!(text.contains("Permission is hereby granted"));
}

#[test]
/// WebView 模板的静态资源也必须进入最终包，不能只验证脚本模板。
fn webview_template_builds_with_its_resources() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        cli(
            directory.path(),
            &["create", "acme.web", "--template", "webview"]
        )
        .status
        .success()
    );
    let project = directory.path().join("acme.web");
    let output = cli(&project, &["build"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(project.join(".kerminal/build/web/index.html").is_file());
    assert!(project.join(".kerminal/build/web/style.css").is_file());
    assert!(project.join(".kerminal/build/web/main.js").is_file());
    assert!(
        fs::read_to_string(project.join(".kerminal/build/web/main.js"))
            .unwrap()
            .contains("view.action")
    );
    assert!(project.join("README.md").is_file());
    assert!(cli(&project, &["check"]).status.success());
}

#[test]
/// 以当前已检出的 SDK 模拟作者固定版本，验证模板确实使用 SDK、清单版本和独立 target。
fn sidecar_template_builds_with_manifest_identity() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        cli(
            directory.path(),
            &["create", "acme.sidecar", "--template", "sidecar-rust"]
        )
        .status
        .success()
    );
    let project = directory.path().join("acme.sidecar");
    let cargo_path = project.join("Cargo.toml");
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../rust")
        .to_string_lossy()
        .replace('\\', "/");
    let cargo = fs::read_to_string(&cargo_path).unwrap();
    assert!(cargo.contains("kerminal-plugin-sdk"));
    // 测试不访问网络创建第二个 checkout；路径替换只把模板依赖指向当前测试的同版源码。
    fs::write(
        &cargo_path,
        cargo.replace(".kerminal-sdk/sdk/rust", &checkout),
    )
    .unwrap();
    let manifest_path = project.join("kerminal-plugin.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["version"] = json!("0.2.3");
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kerminal-plugin"))
        .current_dir(&project)
        .arg("build")
        .env("CARGO_NET_OFFLINE", "true")
        .env(
            "CARGO_TARGET_DIR",
            directory.path().join("unrelated-target"),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let executable = project.join(".kerminal/build/bin").join(if cfg!(windows) {
        "sidecar.exe"
    } else {
        "sidecar"
    });
    assert!(conformance::run(&executable, "acme.sidecar", "0.2.3", false).is_ok());
    assert!(!directory.path().join("unrelated-target").exists());
}

#[test]
/// 编译失败不得替换上一份有效视图和脚本，开发模式才可安全继续使用。
fn failed_build_keeps_previous_package() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        cli(directory.path(), &["create", "acme.notes"])
            .status
            .success()
    );
    let project = directory.path().join("acme.notes");
    assert!(cli(&project, &["build"]).status.success());
    let artifact = project.join(".kerminal/build/dist/main.js");
    let previous = fs::read(&artifact).unwrap();
    fs::write(project.join("src/main.ts"), "const = invalid;").unwrap();
    assert!(!cli(&project, &["build"]).status.success());
    assert_eq!(fs::read(artifact).unwrap(), previous);
    assert!(cli(&project, &["check"]).status.success());
}

#[test]
/// 标记遭替换后不得覆盖原目录，即使原产物和标记仍是普通文件。
fn build_refuses_foreign_owner_marker() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        cli(directory.path(), &["create", "acme.notes"])
            .status
            .success()
    );
    let project = directory.path().join("acme.notes");
    assert!(cli(&project, &["build"]).status.success());
    let artifact = project.join(".kerminal/build/dist/main.js");
    let before = fs::read(&artifact).unwrap();
    let marker = project.join(".kerminal/build-owner");
    fs::write(&marker, b"foreign marker").unwrap();
    assert!(!cli(&project, &["build"]).status.success());
    assert_eq!(fs::read(artifact).unwrap(), before);
    assert_eq!(fs::read(marker).unwrap(), b"foreign marker");
}

#[test]
/// 平台资产只携带当前 Sidecar，并且可直接交给同一个协议校验入口。
fn sidecar_package_contains_only_target_executable() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("input");
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(root.join("kerminal-plugin.json"), serde_json::to_vec(&json!({
        "schema":1,"id":"acme.plugin","name":{"en":"Plugin"},"description":{"en":"Test"},
        "version":"1.0.0","api":"^1.0","publisher":"acme",
        "runtime":{"kind":"sidecar","main":{"windows-x86_64":"bin/windows.exe","linux-x86_64":"bin/linux"}},
        "contributes":{},"views":{}
    })).unwrap()).unwrap();
    fs::write(root.join("bin/windows.exe"), b"windows").unwrap();
    fs::write(root.join("bin/linux"), b"linux").unwrap();
    let output = directory.path().join("out");
    let path = archive::package(
        &root,
        &output,
        Platform::WindowsX86_64,
        ManifestPolicy::ThirdParty,
    )
    .unwrap();
    let unpacked = archive::unpack(&path).unwrap();
    assert!(unpacked.path().join("bin/windows.exe").is_file());
    assert!(!unpacked.path().join("bin/linux").exists());
    assert!(
        archive::check(
            &path,
            Some(Platform::WindowsX86_64),
            ManifestPolicy::ThirdParty
        )
        .is_ok()
    );
    assert!(
        archive::check(
            &path,
            Some(Platform::LinuxX86_64),
            ManifestPolicy::ThirdParty
        )
        .is_err()
    );
}

#[test]
/// 归档自身必须拒绝链接、穿越和祖先目录大小写别名，不能依靠解压后的清单补救。
fn archive_rejects_links_traversal_and_case_aliases() {
    let directory = tempfile::tempdir().unwrap();
    for variant in ["link", "traversal", "case"] {
        let path = directory.path().join(format!("{variant}.kplugin"));
        let file = fs::File::create(&path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        match variant {
            "link" => {
                let mut header = tar::Header::new_ustar();
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                header.set_link_name("outside").unwrap();
                header.set_cksum();
                builder
                    .append_data(&mut header, "link", Cursor::new([]))
                    .unwrap();
            }
            "traversal" => {
                let mut header = tar::Header::new_ustar();
                header.set_path("safe").unwrap();
                header.as_mut_bytes()[..8].copy_from_slice(b"../evil\0");
                header.set_size(0);
                header.set_cksum();
                builder.append(&header, Cursor::new([])).unwrap();
            }
            _ => {
                for name in ["Dir/one", "dir/two"] {
                    let mut header = tar::Header::new_ustar();
                    header.set_size(1);
                    header.set_cksum();
                    builder
                        .append_data(&mut header, name, Cursor::new(b"x"))
                        .unwrap();
                }
            }
        }
        builder.into_inner().unwrap().finish().unwrap();
        assert!(archive::unpack(&path).is_err(), "{variant} must fail");
    }
}

#[test]
/// 模板身份和目标目录由协议与无覆盖规则约束，不留部分生成仓库。
fn create_rejects_invalid_ids_and_existing_targets() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        !cli(directory.path(), &["create", "../escape"])
            .status
            .success()
    );
    assert!(
        cli(
            directory.path(),
            &["create", "acme.test", "--template", "webview"]
        )
        .status
        .success()
    );
    let project = directory.path().join("acme.test");
    let before = fs::read(project.join("kerminal-plugin.json")).unwrap();
    assert!(
        !cli(directory.path(), &["create", "acme.test"])
            .status
            .success()
    );
    assert_eq!(
        before,
        fs::read(project.join("kerminal-plugin.json")).unwrap()
    );
    let empty = directory.path().join("empty");
    fs::create_dir(&empty).unwrap();
    assert!(
        !cli(
            directory.path(),
            &["create", "acme.other", "--out", "empty"]
        )
        .status
        .success()
    );
    assert!(empty.is_dir());
    assert_eq!(fs::read_dir(empty).unwrap().count(), 0);
}

#[test]
/// 测试真实 Sidecar 进程，覆盖普通模式、专用压力探针和错误身份。
fn conformance_verifies_protocol_and_fixture_probes() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join(if cfg!(windows) {
        "fixture.exe"
    } else {
        "fixture"
    });
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/conformance_fixture.rs");
    let compiled = Command::new("rustc")
        .args(["--edition", "2024"])
        .arg(source)
        .arg("-o")
        .arg(&executable)
        .status()
        .unwrap();
    assert!(compiled.success());
    assert!(conformance::run(&executable, "acme.test", "1.0.0", false).is_ok());
    assert!(conformance::run(&executable, "acme.test", "1.0.0", true).is_ok());
    assert!(conformance::run(&executable, "acme.wrong", "1.0.0", false).is_err());
}

#[test]
/// 使用无窗口桌面桩验证真实监听、隔离配置根、重载文件和进程退出后的清理。
fn dev_reloads_changed_source_in_an_isolated_root() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        cli(directory.path(), &["create", "acme.notes"])
            .status
            .success()
    );
    let project = directory.path().join("acme.notes");
    let fixture = directory.path().join(if cfg!(windows) {
        "desktop.exe"
    } else {
        "desktop"
    });
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/dev_fixture.rs");
    assert!(
        Command::new("rustc")
            .args(["--edition", "2024"])
            .arg(source)
            .arg("-o")
            .arg(&fixture)
            .status()
            .unwrap()
            .success()
    );
    let control = directory.path().join("control");
    fs::create_dir(&control).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_kerminal-plugin"))
        .current_dir(&project)
        .args(["dev", "--kerminal"])
        .arg(&fixture)
        .env("KERMINAL_DEV_TEST_CONTROL", &control)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut process = TestProcess(child);
    let deadline = Instant::now() + Duration::from_secs(25);
    let ready = control.join("root.txt");
    while !ready.exists() && Instant::now() < deadline {
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "dev exited before desktop started"
        );
        thread::sleep(Duration::from_millis(25));
    }
    assert!(ready.is_file(), "dev did not start the desktop fixture");
    let source = project.join("src/main.ts");
    let original = fs::read_to_string(&source).unwrap();
    let reload = control.join("reload.json");
    let mut attempt = 0;
    while !reload.exists() && Instant::now() < deadline {
        fs::write(&source, format!("{original}\n// reload {attempt}\n")).unwrap();
        attempt += 1;
        thread::sleep(Duration::from_millis(400));
    }
    assert!(reload.is_file(), "dev did not publish a reload");
    while process.0.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        process
            .0
            .try_wait()
            .unwrap()
            .is_some_and(|status| status.success())
    );
    let payload: serde_json::Value = serde_json::from_slice(&fs::read(reload).unwrap()).unwrap();
    assert!(
        payload["generation"]
            .as_u64()
            .is_some_and(|generation| generation > 0)
    );
    assert!(Path::new(payload["plugin_dir"].as_str().unwrap()).is_absolute());
    let isolated_root = fs::read_to_string(ready).unwrap();
    assert!(!Path::new(&isolated_root).exists());
}
