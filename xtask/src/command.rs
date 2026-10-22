//! 统一子进程执行边界；参数以数组传递，避免 shell 引号和路径空格歧义。
//!
//! @author kongweiguang

use std::{
    env,
    error::Error,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output},
    time::{SystemTime, UNIX_EPOCH},
};

const CARGO_ABOUT_VERSION: &str = "cargo-about 0.9.2";
const THIRD_PARTY_REPORT: &str = "licenses/THIRD_PARTY_DEPENDENCIES.html";

/// 显式 `--config` 会改变 cargo-packager 的相对路径基准，因此临时配置中的输入路径必须绝对化。
pub(super) struct TemporaryPackagerConfig {
    pub(super) path: PathBuf,
}

impl TemporaryPackagerConfig {
    /// 正常返回时显式删除临时文件；Drop 仍负责覆盖异常退出路径。
    fn remove(self) -> io::Result<()> {
        fs::remove_file(&self.path)
    }
}

impl Drop for TemporaryPackagerConfig {
    /// 打包器异常或进程 panic 时尽力清理配置副本，不影响静态源配置。
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// 从 xtask crate 的 manifest 位置反推 workspace 根，避免依赖调用者当前目录。
pub fn workspace_root() -> Result<PathBuf, Box<dyn Error>> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "xtask is not located under the workspace root".into())
}

/// 通过 cargo 子命令执行门禁，并在失败时保留原始退出码。
pub fn cargo(root: &Path, arguments: &[&str]) -> Result<(), Box<dyn Error>> {
    let status = Command::new("cargo")
        .current_dir(root)
        .args(arguments)
        .status()?;
    require_success("cargo", status)
}

/// 默认隔离配置不接触用户 home；并行原生验收可显式提供绝对 dev 根，避免复用另一候选的会话。
pub fn run_desktop(root: &Path, smoke: bool) -> Result<(), Box<dyn Error>> {
    let config_root = match std::env::var_os("KERMINAL_DEV_CONFIG_ROOT") {
        Some(path) => {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err("KERMINAL_DEV_CONFIG_ROOT must be an absolute isolated path".into());
            }
            path
        }
        None => root.join("target").join(if smoke {
            "kerminal-native-smoke"
        } else {
            "kerminal-dev-config"
        }),
    };
    fs::create_dir_all(&config_root)?;

    let mut command = Command::new("cargo");
    command
        .current_dir(root)
        .args(["--locked", "run", "--package", "kerminal-desktop"])
        .env("KERMINAL_CONFIG_ROOT", &config_root);
    let status = command.status()?;
    require_success("cargo run -p kerminal-desktop", status)
}

/// 在独立示例中运行真实 PTY 压测；参数以 argv 转发，不经 shell 或 UI 假会话。
pub fn run_session_acceptance(root: &Path, arguments: &[String]) -> Result<(), Box<dyn Error>> {
    let mut command = Command::new("cargo");
    command
        .current_dir(root)
        .args([
            "--locked",
            "run",
            "--release",
            "-p",
            "kerminal-infrastructure",
            "--example",
            "session-acceptance",
            "--",
        ])
        .args(arguments);
    let status = command.status()?;
    require_success("cargo run --example session-acceptance", status)
}

/// 从固定目标清单选择平台配置，防止调用者拼出任意路径或将另一平台产物误打包。
pub(super) fn packaging_config(target: &str) -> Result<&'static str, Box<dyn Error>> {
    match target {
        "x86_64-pc-windows-msvc" => Ok("Packager.windows.toml"),
        "x86_64-unknown-linux-gnu" => Ok("Packager.linux.toml"),
        "x86_64-apple-darwin" | "aarch64-apple-darwin" => Ok("Packager.macos.toml"),
        _ => Err(format!("unsupported packaging target: {target}").into()),
    }
}

/// 为所选平台构造迁移 helper 的锁定 release 构建参数。
/// package 必须先完成此构建，避免把 target 目录中残留的旧 helper 当作本次产物。
///
/// @author kongweiguang
pub(super) fn migration_helper_build_arguments(target: &str) -> Result<Vec<&str>, Box<dyn Error>> {
    packaging_config(target)?;
    Ok(vec![
        "--locked",
        "build",
        "--release",
        "--package",
        "kerminal-desktop",
        "--bin",
        "kerminal-migrate",
        "--target",
        target,
    ])
}

/// 在 desktop crate 目录用指定平台配置打包，隔离 workspace 虚拟根的包发现行为。
/// 先构建当前源码和 target 的迁移 helper，再组装资源，确保构建失败时不会继续打包磁盘旧文件。
/// 许可证报告仍由锁文件生成；本地候选包不读取或传入签名私钥。
///
/// @author kongweiguang
pub fn package(root: &Path, target: &str) -> Result<(), Box<dyn Error>> {
    let config = packaging_config(target)?;
    run_package_candidate_collector(root, target, "prepare")?;
    generate_third_party_report(root)?;
    let helper_build_arguments = migration_helper_build_arguments(target)?;
    cargo(root, &helper_build_arguments)
        .map_err(|error| format!("build migration helper for {target}: {error}"))?;
    let temporary_config = prepare_packager_config(root, target, config)?;
    let target_directory = root.join("crates").join("kerminal-desktop");
    let invocation = Command::new("cargo")
        .current_dir(&target_directory)
        .args(["--locked", "packager", "--config"])
        .arg(&temporary_config.path)
        .args(["--release", "--target", target])
        .env_remove("CARGO_PACKAGER_SIGN_PRIVATE_KEY")
        .env_remove("CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD")
        .env_remove("TAURI_SIGNING_PRIVATE_KEY")
        .env_remove("TAURI_SIGNING_PRIVATE_KEY_PASSWORD")
        .status()?;
    temporary_config.remove()?;
    let status = invocation;
    require_success("cargo packager", status)?;
    run_package_candidate_collector(root, target, "collect")
}

/// 让本地 xtask 与手动 CI 使用同一候选收集器，避免打包后漏掉 updater tar 或旧产物。
fn run_package_candidate_collector(
    root: &Path,
    target: &str,
    action: &str,
) -> Result<(), Box<dyn Error>> {
    let mut invocation = package_candidate_collector_command(root, target, action)?;
    let status = invocation.status()?;
    require_success("package candidate collector", status)
}

/// 为收集器显式传递仓库根和 target，避免依赖当前目录或宿主机平台推断。
pub(super) fn package_candidate_collector_command(
    root: &Path,
    target: &str,
    action: &str,
) -> Result<Command, Box<dyn Error>> {
    packaging_config(target)?;
    if !matches!(action, "prepare" | "collect") {
        return Err(format!("unsupported package collector action: {action}").into());
    }
    let workspace = root.canonicalize()?;
    let script = workspace.join("packaging").join("collect_candidates.py");
    if !script.is_file() {
        return Err(format!(
            "package candidate collector is missing: {}",
            script.display()
        )
        .into());
    }
    let mut command = Command::new("python");
    command
        .current_dir(&workspace)
        .arg(&script)
        .arg(action)
        .arg("--target")
        .arg(target)
        .arg("--workspace")
        .arg(workspace);
    Ok(command)
}

/// 基于平台静态策略生成短命配置，补齐绝对资源路径、精确版本和目标 release 二进制目录。
fn prepare_packager_config(
    root: &Path,
    target: &str,
    config_name: &str,
) -> Result<TemporaryPackagerConfig, Box<dyn Error>> {
    let metadata = Command::new("cargo")
        .current_dir(root)
        .args(["metadata", "--locked", "--no-deps", "--format-version", "1"])
        .output()?;
    if !metadata.status.success() {
        return Err(format!(
            "cargo metadata failed while resolving the packaging target directory: {}",
            String::from_utf8_lossy(&metadata.stderr).trim()
        )
        .into());
    }
    let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout)?;
    let target_directory = metadata
        .get("target_directory")
        .and_then(serde_json::Value::as_str)
        .ok_or("cargo metadata did not return a target directory")?;
    let release_directory = PathBuf::from(target_directory)
        .join(target)
        .join("release")
        .canonicalize()?;
    let workspace = root.canonicalize()?;
    prepare_packager_config_at(&workspace, target, config_name, &release_directory)
}

/// 显式写入 Cargo 包名，避免 cargo-packager 从临时配置文件路径探测包名。
/// 迁移 helper 仅从本次 target 的 release 目录加入包资源，缺失时拒绝沿用 PATH 或旧包。
///
/// @author kongweiguang
pub(super) fn prepare_packager_config_at(
    root: &Path,
    target: &str,
    config_name: &str,
    release_directory: &Path,
) -> Result<TemporaryPackagerConfig, Box<dyn Error>> {
    let root = root.canonicalize()?;
    let release_directory = release_directory.canonicalize()?;
    let config_path = root.join("packaging").join(config_name).canonicalize()?;
    let config_directory = config_path
        .parent()
        .ok_or("packager config has no parent directory")?;
    let mut config: toml::Value = toml::from_str(&fs::read_to_string(&config_path)?)?;
    let table = config
        .as_table_mut()
        .ok_or("packager config root must be a table")?;

    table.insert(
        "name".to_owned(),
        toml::Value::String("kerminal-desktop".to_owned()),
    );

    let workspace_manifest: toml::Value =
        toml::from_str(&fs::read_to_string(root.join("Cargo.toml"))?)?;
    let version = workspace_manifest
        .get("workspace")
        .and_then(|value| value.get("package"))
        .and_then(|value| value.get("version"))
        .and_then(toml::Value::as_str)
        .ok_or("workspace package version is missing")?;
    table.insert(
        "version".to_owned(),
        toml::Value::String(version.to_owned()),
    );

    let target_directory = root.join("target").canonicalize()?;
    let output_directory = target_directory.join("package-output");
    fs::create_dir_all(&output_directory)?;
    let output_directory = output_directory.canonicalize()?;
    if !output_directory.starts_with(&target_directory) {
        return Err("packager output directory must remain inside target".into());
    }
    table.insert(
        "out-dir".to_owned(),
        toml::Value::String(path_to_utf8(&output_directory)?.to_owned()),
    );
    table.insert(
        "binaries-dir".to_owned(),
        toml::Value::String(path_to_utf8(&release_directory)?.to_owned()),
    );

    absolutize_packager_input_paths(table, &root, config_directory)?;
    append_migration_helper_resource(table, target, &release_directory)?;
    validate_packager_binary(table, target, &release_directory)?;
    validate_kerminal_deep_link(table)?;

    let serialized = toml::to_string_pretty(&config)?;
    let temporary_directory = root.join("target").canonicalize()?;
    let temporary_path = temporary_directory.join(format!(
        ".packager-config-{}-{}.toml",
        std::process::id(),
        log_run_id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary_path)?;
    let temporary_config = TemporaryPackagerConfig {
        path: temporary_path,
    };
    file.write_all(serialized.as_bytes())?;
    file.sync_all()?;
    Ok(temporary_config)
}

/// 将当前 target 的 Rust 迁移可执行文件放在 wrapper 同目录，避免运行时从 PATH 选错程序。
///
/// 迁移器是独立 console binary，资源映射不改变桌面主程序身份；Windows 保留 `.exe` 后缀，
/// 其它受支持平台使用无后缀文件名。release 目录允许位于 workspace 外的 CARGO_TARGET_DIR。
///
/// @author kongweiguang
pub(super) fn append_migration_helper_resource(
    config: &mut toml::map::Map<String, toml::Value>,
    target: &str,
    release_directory: &Path,
) -> Result<(), Box<dyn Error>> {
    let (binary_name, package_path) = match target {
        "x86_64-pc-windows-msvc" => ("kerminal-migrate.exe", "migration/kerminal-migrate.exe"),
        "x86_64-unknown-linux-gnu" | "x86_64-apple-darwin" | "aarch64-apple-darwin" => {
            ("kerminal-migrate", "migration/kerminal-migrate")
        }
        _ => return Err(format!("unsupported migration helper target: {target}").into()),
    };

    let resources = config
        .get_mut("resources")
        .and_then(toml::Value::as_array_mut)
        .ok_or("packager resources must be an array")?;
    if resources
        .iter()
        .any(|resource| resource.get("target").and_then(toml::Value::as_str) == Some(package_path))
    {
        return Err(
            format!("migration helper resource target already exists: {package_path}").into(),
        );
    }
    validate_relative_package_path(package_path)?;

    let source = release_directory.join(binary_name);
    let metadata = fs::symlink_metadata(&source).map_err(|error| {
        format!(
            "migration helper is missing from this target release directory: {} ({error})",
            source.display()
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "migration helper must be a regular file: {}",
            source.display()
        )
        .into());
    }

    let mut resource = toml::map::Map::new();
    resource.insert(
        "src".to_owned(),
        toml::Value::String(path_to_utf8(&source)?.to_owned()),
    );
    resource.insert(
        "target".to_owned(),
        toml::Value::String(package_path.to_owned()),
    );
    resources.push(toml::Value::Table(resource));
    Ok(())
}

/// 将许可证、图标和每个打包资源的源文件转成受 workspace 根约束的绝对路径。
fn absolutize_packager_input_paths(
    config: &mut toml::map::Map<String, toml::Value>,
    root: &Path,
    config_directory: &Path,
) -> Result<(), Box<dyn Error>> {
    let license = config
        .get("license-file")
        .and_then(toml::Value::as_str)
        .ok_or("packager license-file is missing")?;
    config.insert(
        "license-file".to_owned(),
        toml::Value::String(resolve_workspace_file(root, config_directory, license)?),
    );

    let icons = config
        .get_mut("icons")
        .and_then(toml::Value::as_array_mut)
        .ok_or("packager icons must be an array")?;
    for icon in icons {
        let path = icon
            .as_str()
            .ok_or("each packager icon must be a path string")?;
        *icon = toml::Value::String(resolve_workspace_file(root, config_directory, path)?);
    }

    let resources = config
        .get_mut("resources")
        .and_then(toml::Value::as_array_mut)
        .ok_or("packager resources must be an array")?;
    for resource in resources {
        let resource = resource
            .as_table_mut()
            .ok_or("each packager resource must be a table")?;
        let source = resource
            .get("src")
            .and_then(toml::Value::as_str)
            .ok_or("each packager resource must have a src path")?;
        let target = resource
            .get("target")
            .and_then(toml::Value::as_str)
            .ok_or("each packager resource must have a target path")?;
        validate_relative_package_path(target)?;
        resource.insert(
            "src".to_owned(),
            toml::Value::String(resolve_workspace_file(root, config_directory, source)?),
        );
    }
    Ok(())
}

/// 只接受 workspace 内已存在的资源，避免 config cwd 改变后意外读取其它目录。
fn resolve_workspace_file(
    root: &Path,
    config_directory: &Path,
    source: &str,
) -> Result<String, Box<dyn Error>> {
    let path = PathBuf::from(source);
    let path = if path.is_absolute() {
        path
    } else {
        config_directory.join(path)
    }
    .canonicalize()?;
    if !path.starts_with(root) || !path.is_file() {
        return Err(format!("packager source path is not a workspace file: {source}").into());
    }
    Ok(path_to_utf8(&path)?.to_owned())
}

/// 检查打包资源在安装包内使用相对路径，拒绝绝对路径和越界组件。
fn validate_relative_package_path(path: &str) -> Result<(), Box<dyn Error>> {
    let path = Path::new(path);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Err("packager resource target must remain inside the package".into());
    }
    Ok(())
}

/// 要求平台配置保留一个且仅一个 kerminal 主程序，并验证 release 文件已实际构建。
fn validate_packager_binary(
    config: &toml::map::Map<String, toml::Value>,
    target: &str,
    release_directory: &Path,
) -> Result<(), Box<dyn Error>> {
    let binaries = config
        .get("binaries")
        .and_then(toml::Value::as_array)
        .ok_or("packager binaries must be an array")?;
    let main_binaries = binaries
        .iter()
        .filter(|binary| {
            binary
                .get("main")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    if main_binaries.len() != 1 {
        return Err("packager config must declare exactly one main binary".into());
    }
    let binary_name = main_binaries[0]
        .get("path")
        .and_then(toml::Value::as_str)
        .ok_or("packager main binary path is missing")?;
    let relative = Path::new(binary_name);
    if relative.is_absolute() || relative.components().count() != 1 || binary_name != "kerminal" {
        return Err("packager main binary must be the Kerminal executable".into());
    }
    let executable = if target == "x86_64-pc-windows-msvc" {
        release_directory.join(relative).with_extension("exe")
    } else {
        release_directory.join(relative)
    };
    if !executable.is_file() {
        return Err(format!(
            "packager binary is missing at {}; build the desktop release for target {target} first",
            executable.display()
        )
        .into());
    }
    Ok(())
}

/// 候选安装包只注册应用自有 kerminal scheme，不抢占通用 ssh 或 sftp URL。
fn validate_kerminal_deep_link(
    config: &toml::map::Map<String, toml::Value>,
) -> Result<(), Box<dyn Error>> {
    let protocols = config
        .get("deep-link-protocols")
        .and_then(toml::Value::as_array)
        .ok_or("packager config must declare the kerminal URL scheme")?;
    let only_protocol = protocols.as_slice();
    let valid = matches!(only_protocol, [protocol]
        if protocol.get("schemes").and_then(toml::Value::as_array)
            .is_some_and(|schemes| schemes.len() == 1 && schemes[0].as_str() == Some("kerminal"))
            && protocol.get("role").and_then(toml::Value::as_str) == Some("viewer"));
    if !valid {
        return Err("packager config must register only the kerminal URL scheme".into());
    }
    Ok(())
}

/// 把平台配置引用的路径转成 UTF-8 命令参数；不可表示时拒绝打包而非悄悄改路径。
fn path_to_utf8(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| "packager path is not valid Unicode".into())
}

/// 仅使用仓库私有且固定版本的 cargo-about，避免全局 PATH 工具改变交付许可证集合。
/// 失败时删除临时报告并依赖诊断日志定位原因，不保留未验证的许可证产物。
///
/// @author kongweiguang
fn generate_third_party_report(root: &Path) -> Result<(), Box<dyn Error>> {
    let executable = root
        .join("target")
        .join("tools")
        .join("cargo-about")
        .join("bin")
        .join(if cfg!(windows) {
            "cargo-about.exe"
        } else {
            "cargo-about"
        });
    if !executable.is_file() {
        return Err(format!(
            "required cargo-about 0.9.2 tool is missing at {}; package generation is blocked",
            executable.display()
        )
        .into());
    }

    let version = run_cargo_about_logged(root, &executable, "version", &["--version"])?;
    let version_text = String::from_utf8_lossy(&version.stdout);
    if !version.status.success() || !is_expected_cargo_about_version(&version_text) {
        return Err(format!(
            "cargo-about version check failed (expected {CARGO_ABOUT_VERSION}); see the cargo-about version logs under target/diagnostics"
        )
        .into());
    }

    let report_path = root.join(THIRD_PARTY_REPORT);
    let report_parent = report_path
        .parent()
        .ok_or("third-party report path has no parent directory")?;
    fs::create_dir_all(report_parent)?;
    let temporary_path = report_parent.join(format!(
        ".THIRD_PARTY_DEPENDENCIES.html.{}.{}.tmp",
        std::process::id(),
        log_run_id()
    ));
    let temporary_argument = temporary_path.to_string_lossy().into_owned();
    let arguments = [
        "generate",
        "licenses/third-party-dependencies.hbs",
        "--config",
        "about.toml",
        "--workspace",
        "--locked",
        "--fail",
        "--output-file",
        temporary_argument.as_str(),
    ];
    let generation = run_cargo_about_logged(root, &executable, "generate", &arguments)?;
    if !generation.status.success() {
        let _ = fs::remove_file(&temporary_path);
        return Err("cargo-about failed to resolve or render the locked dependency licenses; see the cargo-about generation logs under target/diagnostics".into());
    }

    let html = match fs::read_to_string(&temporary_path) {
        Ok(html) => html,
        Err(error) => {
            let _ = fs::remove_file(&temporary_path);
            return Err(format!("cargo-about did not produce a readable report: {error}").into());
        }
    };
    if let Err(error) = validate_third_party_report(&html) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    if html.trim().is_empty() {
        let _ = fs::remove_file(&temporary_path);
        return Err("cargo-about generated an empty dependency report".into());
    }

    // Rename from the same directory so the packager only sees a complete report.
    fs::rename(&temporary_path, &report_path)?;
    Ok(())
}

/// 保存 cargo-about 的原始 stdout/stderr，确保许可证失败能在本机复查且不继续打包。
fn run_cargo_about_logged(
    root: &Path,
    executable: &Path,
    action: &str,
    arguments: &[&str],
) -> Result<Output, Box<dyn Error>> {
    let output = Command::new(executable)
        .current_dir(root)
        .args(arguments)
        .output()?;
    let diagnostics = root.join("target").join("diagnostics");
    fs::create_dir_all(&diagnostics)?;
    let run_id = log_run_id();
    fs::write(
        diagnostics.join(format!("cargo-about-{action}-{run_id}.stdout.log")),
        &output.stdout,
    )?;
    fs::write(
        diagnostics.join(format!("cargo-about-{action}-{run_id}.stderr.log")),
        &output.stderr,
    )?;
    Ok(output)
}

/// 给临时报告和原始工具日志生成不易冲突的后缀，避免覆盖前次失败证据。
fn log_run_id() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

/// 固定工具精确版本，因为模板和锁文件处理方式属于打包门禁的一部分。
pub(super) fn is_expected_cargo_about_version(version: &str) -> bool {
    version.trim() == CARGO_ABOUT_VERSION
}

/// 在报告成为安装包资源前拒绝不完整或无关的输出。
pub(super) fn validate_third_party_report(html: &str) -> Result<(), Box<dyn Error>> {
    for required in [
        "<!doctype html>",
        "<title>Kerminal third-party dependency licenses</title>",
        "<h2>Dependency notices and license texts</h2>",
        "<section id=",
    ] {
        if !html.contains(required) {
            return Err(format!(
                "cargo-about report is missing required section marker {required:?}"
            )
            .into());
        }
    }
    Ok(())
}

/// 将失败退出码变成带命令名的错误，防止门禁脚本吞掉子进程失败。
fn require_success(command: &str, status: ExitStatus) -> Result<(), Box<dyn Error>> {
    if status.success() {
        Ok(())
    } else {
        Err(format!("{command} exited with {status}").into())
    }
}
