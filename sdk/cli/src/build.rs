//! 将作者源码转换为可由宿主独立加载的已验证目录。
//! @author kongweiguang

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

use kerminal_plugin_protocol::{
    ManifestPolicy, Platform, PluginRuntime, parse_manifest, validate_package,
    validate_package_path,
};
use serde_json::Value;

use crate::{Result, platform_name};

const ESBUILD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/kerminal-esbuild"));
const OWNER_MARKER: &[u8] = b"kerminal-plugin build v1\n";

/// 暂存完整结果并通过宿主同一校验后才替换旧构建，失败时保留上一次有效结果。
pub fn build(
    project: &Path,
    selected: Option<Platform>,
    policy: ManifestPolicy,
) -> Result<PathBuf> {
    let project = project.canonicalize()?;
    let metadata = project.join(".kerminal");
    if metadata.exists() && !ordinary_directory(&metadata)? {
        return Err(".kerminal 必须是普通目录".into());
    }
    fs::create_dir_all(&metadata)?;
    let metadata = metadata.canonicalize()?;
    if !metadata.starts_with(&project) {
        return Err("构建目录超出项目根目录".into());
    }
    let owner = metadata.join("build-owner");
    let output = metadata.join("build");
    let had_original = match fs::symlink_metadata(&output) {
        Ok(output_metadata) => {
            verify_owner(&owner)?;
            if !output_metadata.is_dir()
                || is_link(&output_metadata)
                || !output.canonicalize()?.starts_with(&metadata)
            {
                return Err("已有构建目录不属于本工具，拒绝覆盖".into());
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let temporary = tempfile::tempdir_in(&metadata)?;
    let staged = temporary.path().join("package");
    fs::create_dir(&staged)?;
    let declaration = fs::read(project.join("kerminal-plugin.json"))?;
    let manifest = parse_manifest(&declaration, policy)?;
    fs::write(staged.join("kerminal-plugin.json"), declaration)?;
    let resources = project.join("resources");
    match fs::symlink_metadata(&resources) {
        Ok(resource_metadata) if resource_metadata.is_dir() && !is_link(&resource_metadata) => {
            copy_resources(&resources, &staged)?;
        }
        Ok(_) => return Err("resources 必须是普通目录，不能是链接或重解析点".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let executable = extract_esbuild(temporary.path())?;
    match &manifest.runtime {
        PluginRuntime::Script { main, .. } => {
            let source = project.join("src/main.ts");
            if !source.is_file() {
                return Err("脚本入口 src/main.ts 不存在".into());
            }
            validate_package_path(main)?;
            let destination = staged.join(main);
            fs::create_dir_all(destination.parent().ok_or("脚本输出缺少父目录")?)?;
            run_esbuild(
                &executable,
                &project,
                &source,
                &destination,
                "esm",
                "neutral",
            )?;
        }
        PluginRuntime::Sidecar { main, .. } => {
            let platform = selected
                .or_else(Platform::current)
                .ok_or("必须指定支持的 Sidecar 平台")?;
            let output_name = main.get(&platform).ok_or("Sidecar 未声明目标平台")?;
            validate_package_path(output_name)?;
            if project.join("Cargo.toml").is_file() {
                let target = rust_target(platform)?;
                let target_dir = metadata.join("sidecar-target");
                let status = Command::new("cargo")
                    .current_dir(&project)
                    .env("CARGO_TARGET_DIR", &target_dir)
                    .env("KERMINAL_PLUGIN_ID", &manifest.id)
                    .env("KERMINAL_PLUGIN_VERSION", &manifest.version)
                    .args(["build", "--release", "--target", target])
                    .status()?;
                if !status.success() {
                    return Err("Sidecar 构建失败".into());
                }
                let executable_name =
                    if matches!(platform, Platform::WindowsX86_64 | Platform::WindowsAarch64) {
                        "kerminal-plugin-sidecar.exe"
                    } else {
                        "kerminal-plugin-sidecar"
                    };
                let artifact = target_dir
                    .join(target)
                    .join("release")
                    .join(executable_name);
                let destination = staged.join(output_name);
                fs::create_dir_all(destination.parent().ok_or("Sidecar 输出缺少父目录")?)?;
                fs::copy(artifact, destination)?;
            }
        }
        PluginRuntime::None { .. } => {}
    }
    // 浏览器入口与脚本进程入口分开打包，不能把 DOM 代码放入 QuickJS。
    let browser_source = project.join("src/web.ts");
    if browser_source.is_file() {
        let web_output = staged.join("web/main.js");
        fs::create_dir_all(web_output.parent().ok_or("网页输出缺少父目录")?)?;
        run_esbuild(
            &executable,
            &project,
            &browser_source,
            &web_output,
            "esm",
            "browser",
        )?;
    }
    let views = project.join("src/views.ts");
    if views.exists() {
        let bundle = temporary.path().join("views.cjs");
        run_esbuild(&executable, &project, &views, &bundle, "cjs", "node")?;
        write_views(&project, &bundle, &staged)?;
    }
    let package = validate_package(&staged, policy)?;
    let platform = match package.manifest.runtime {
        PluginRuntime::Sidecar { .. } => selected
            .or_else(Platform::current)
            .ok_or("未知 Sidecar 目标平台")?,
        _ => selected.unwrap_or(Platform::Any),
    };
    package.validate_for_platform(platform)?;
    ensure_owner(&owner)?;
    let previous = temporary.path().join("previous");
    let output_now = match fs::symlink_metadata(&output) {
        Ok(output_metadata) => {
            if !had_original || !output_metadata.is_dir() || is_link(&output_metadata) {
                return Err("构建期间目标目录发生变化，拒绝覆盖".into());
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if had_original && !output_now {
        return Err("构建期间原目标目录已消失，拒绝覆盖".into());
    }
    if output_now {
        fs::rename(&output, &previous)?;
    }
    if let Err(error) = fs::rename(&staged, &output) {
        if output_now && let Err(recovery_error) = fs::rename(&previous, &output) {
            let recovery = temporary.keep();
            return Err(format!(
                "构建提交失败: {error}；恢复旧结果失败: {recovery_error}；旧结果保存在 {}",
                recovery.join("previous").display()
            )
            .into());
        }
        return Err(error.into());
    }
    Ok(output)
}

/// 标记必须是本工具写入的普通文件，不能跟随链接或把任意同名文件当作覆盖授权。
fn verify_owner(owner: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(owner)?;
    if !metadata.is_file() || is_link(&metadata) || fs::read(owner)? != OWNER_MARKER {
        return Err("构建目录的所有权标记无效，拒绝覆盖".into());
    }
    Ok(())
}

/// 在替换结果前提交标记，首次构建即使标记创建失败也不会留下无法识别的新结果。
fn ensure_owner(owner: &Path) -> Result<()> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(owner)
    {
        Ok(mut file) => {
            if let Err(error) = file.write_all(OWNER_MARKER).and_then(|_| file.sync_all()) {
                drop(file);
                let _ = fs::remove_file(owner);
                return Err(error.into());
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => verify_owner(owner),
        Err(error) => Err(error.into()),
    }
}

/// 递归复制仅限普通文件的资源树，避免把项目外的符号链接打入插件。
fn copy_resources(source: &Path, destination: &Path) -> Result<()> {
    if !ordinary_directory(source)? {
        return Err("resources 必须是普通目录".into());
    }
    for result in fs::read_dir(source)? {
        let entry = result?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if is_link(&metadata) {
            return Err("resources 不允许链接或重解析点".into());
        }
        let target = destination.join(entry.file_name());
        if metadata.is_dir() {
            fs::create_dir(&target)?;
            copy_resources(&path, &target)?;
        } else if metadata.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.nlink() != 1 {
                    return Err("resources 不允许硬链接".into());
                }
            }
            fs::copy(path, target)?;
        } else {
            return Err("resources 只允许普通文件和目录".into());
        }
    }
    Ok(())
}

/// Windows 的目录连接点也会重定向资源，必须连同普通符号链接拒绝。
fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// 判断目录本身而非跟随链接，避免覆盖工具目录之外的用户资料。
fn ordinary_directory(path: &Path) -> Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    Ok(metadata.is_dir() && !is_link(&metadata))
}

/// 内嵌程序只写入本次私有暂存，用户不需要安装全局 esbuild。
fn extract_esbuild(directory: &Path) -> Result<PathBuf> {
    let path = directory.join(if cfg!(windows) {
        "esbuild.exe"
    } else {
        "esbuild"
    });
    let mut output = fs::File::create(&path)?;
    output.write_all(ESBUILD)?;
    drop(output);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}

/// 明确平台与模块格式，避免 Node 专用内置模块被错误打进 QuickJS 脚本。
fn run_esbuild(
    executable: &Path,
    project: &Path,
    source: &Path,
    destination: &Path,
    format: &str,
    platform: &str,
) -> Result<()> {
    let status = Command::new(executable)
        .current_dir(project)
        .arg(source)
        .args([
            "--bundle",
            &format!("--format={format}"),
            &format!("--platform={platform}"),
            "--target=es2020",
        ])
        .arg(format!("--outfile={}", destination.display()))
        .status()?;
    if !status.success() {
        return Err("esbuild 打包失败".into());
    }
    Ok(())
}

/// 构建入口导出路径到静态视图的映射；Rust 校验所有路径并由协议校验最终内容。
fn write_views(project: &Path, bundle: &Path, destination: &Path) -> Result<()> {
    // Windows canonicalize 返回 \\?\ 路径，Node 的 require 会错误处理此前缀。
    let relative = bundle
        .strip_prefix(project)
        .map_err(|_| "视图构建产物不在项目目录中")?;
    let module = Path::new(".").join(relative);
    let output = Command::new("node")
        .current_dir(project)
        .args(["-e", "const x=require(process.argv[1]);const v=x.default??x;if(!v||typeof v!=='object'||Array.isArray(v))throw Error('views entry must export an object');process.stdout.write(JSON.stringify(v));"])
        .arg(module)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "视图构建入口执行失败: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    if output.stdout.len() > 16 * 1024 * 1024 {
        return Err("生成的视图超过大小限制".into());
    }
    let value: Value = serde_json::from_slice(&output.stdout)?;
    let views = value.as_object().ok_or("视图构建入口必须导出对象")?;
    for (path, view) in views {
        validate_package_path(path)?;
        let file = destination.join(path);
        fs::create_dir_all(file.parent().ok_or("视图缺少父目录")?)?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(file)?;
        serde_json::to_writer_pretty(&mut output, view)?;
        output.write_all(b"\n")?;
    }
    Ok(())
}

/// 发布矩阵的四个目标使用明确映射，其他平台不能错误复用相邻架构。
fn rust_target(platform: Platform) -> Result<&'static str> {
    match platform {
        Platform::WindowsX86_64 => Ok("x86_64-pc-windows-msvc"),
        Platform::LinuxX86_64 => Ok("x86_64-unknown-linux-gnu"),
        Platform::MacosX86_64 => Ok("x86_64-apple-darwin"),
        Platform::MacosAarch64 => Ok("aarch64-apple-darwin"),
        _ => Err(format!("当前构建矩阵未提供 {}", platform_name(platform)?).into()),
    }
}
