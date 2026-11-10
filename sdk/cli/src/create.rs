//! 模板生成不覆盖现有仓库，输出在完全准备后一次性发布。
//! @author kongweiguang

use std::{fs, path::Path};

use kerminal_plugin_protocol::{ManifestPolicy, parse_manifest};

use crate::Result;

const RELEASE: &str = include_str!("../../templates/release.yml");

/// 先由协议验证身份，再在同一父目录暂存；目标已存在时绝不改写。
pub fn create(id: &str, template: &str, output: &Path, policy: ManifestPolicy) -> Result<()> {
    let publisher = id
        .split_once('.')
        .map(|(publisher, _)| publisher)
        .ok_or("插件 ID 必须是 publisher.name")?;
    let (manifest, files): (&str, &[(&str, &str)]) = match template {
        "native" => (
            include_str!("../../templates/native/manifest.json"),
            &[
                (
                    "src/main.ts",
                    include_str!("../../templates/native/main.ts"),
                ),
                (
                    "src/views.ts",
                    include_str!("../../templates/native/views.ts"),
                ),
                (
                    "resources/l10n/en.json",
                    include_str!("../../templates/native/en.json"),
                ),
            ],
        ),
        "webview" => (
            include_str!("../../templates/webview/manifest.json"),
            &[
                (
                    "src/main.ts",
                    include_str!("../../templates/webview/main.ts"),
                ),
                ("src/web.ts", include_str!("../../templates/webview/web.ts")),
                (
                    "resources/l10n/en.json",
                    include_str!("../../templates/webview/en.json"),
                ),
                (
                    "resources/web/index.html",
                    include_str!("../../templates/webview/index.html"),
                ),
                (
                    "resources/web/style.css",
                    include_str!("../../templates/webview/style.css"),
                ),
            ],
        ),
        "sidecar-rust" => (
            include_str!("../../templates/sidecar-rust/manifest.json"),
            &[
                (
                    "src/main.rs",
                    include_str!("../../templates/sidecar-rust/main.rs"),
                ),
                (
                    "src/views.ts",
                    include_str!("../../templates/sidecar-rust/views.ts"),
                ),
                (
                    "resources/l10n/en.json",
                    include_str!("../../templates/sidecar-rust/en.json"),
                ),
                (
                    "Cargo.toml",
                    include_str!("../../templates/sidecar-rust/Cargo.toml.in"),
                ),
            ],
        ),
        _ => return Err("模板必须是 native、webview 或 sidecar-rust".into()),
    };
    let declaration = manifest
        .replace("__PLUGIN_ID__", id)
        .replace("__PUBLISHER__", publisher);
    parse_manifest(declaration.as_bytes(), policy)?;
    if output.exists() {
        return Err("目标目录已存在，拒绝覆盖".into());
    }
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let temporary = tempfile::tempdir_in(parent)?;
    let staged = temporary.path().join("plugin");
    fs::create_dir(&staged)?;
    write_file(&staged, "kerminal-plugin.json", &declaration)?;
    for (path, text) in files {
        write_file(&staged, path, &text.replace("__PLUGIN_ID__", id))?;
    }
    write_file(&staged, ".github/workflows/release.yml", RELEASE)?;
    write_file(
        &staged,
        "README.md",
        &include_str!("../../templates/README.md").replace("__PLUGIN_ID__", id),
    )?;
    write_file(
        &staged,
        ".gitignore",
        "# @author kongweiguang\n.kerminal/\n.kerminal-sdk/\nnode_modules/\ntarget/\n",
    )?;
    write_file(
        &staged,
        "package.json",
        "{\n  \"private\": true,\n  \"type\": \"module\"\n}\n",
    )?;
    // 目录竞争不能依赖预先 exists 检查；无覆盖重命名不支持时安全失败。
    tempfile::TempPath::try_from_path(staged)?
        .persist_noclobber(output)
        .map_err(|error| error.error)?;
    Ok(())
}

/// 路径只来自静态模板，父目录在私有暂存中创建而不会改写用户文件。
fn write_file(root: &Path, path: &str, content: &str) -> Result<()> {
    let target = root.join(path);
    fs::create_dir_all(target.parent().ok_or("模板路径缺少父目录")?)?;
    fs::write(target, content)?;
    Ok(())
}
