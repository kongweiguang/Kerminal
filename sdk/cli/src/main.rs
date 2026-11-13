//! 开发命令仅报告已完成的步骤，失败均以非零状态退出。
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use kerminal_plugin_cli::{
    Result, archive, asset_url, build, conformance, create, dev, parse_platform, platform_name,
};
use kerminal_plugin_protocol::{
    ManifestPolicy, Platform, PluginRuntime, parse_manifest, validate_package,
};

/// 错误统一从 stderr 输出，避免调用方把失败当作发布成功。
fn main() {
    if let Err(error) = execute() {
        eprintln!("kerminal-plugin: {error}");
        std::process::exit(1);
    }
}

/// 严格拒绝未知和重复选项，使发布脚本中的笔误不会被忽略。
fn execute() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let command = arguments.next().unwrap_or_else(|| "help".into());
    let command = command.to_str().ok_or("命令必须是 UTF-8")?;
    if command == "--licenses" {
        if arguments.next().is_some() {
            return Err("--licenses 不接受额外参数".into());
        }
        println!(
            "{}\n{}",
            include_str!("../licenses/NOTICE.txt"),
            include_str!("../licenses/esbuild-LICENSE.md")
        );
        return Ok(());
    }
    if matches!(command, "help" | "--help" | "-h") {
        println!(
            "kerminal-plugin create <id> [--template native|webview] [--no-backend] [--out DIR] [--official]\n\
kerminal-plugin build [--project DIR] [--platform PLATFORM] [--official]\n\
kerminal-plugin check [DIR|FILE.kplugin] [--platform PLATFORM] [--official]\n\
kerminal-plugin package [--dir DIR] [--out DIR] [--platform PLATFORM] [--official]\n\
kerminal-plugin dev [--project DIR] [--kerminal EXE] [--official]\n\
kerminal-plugin conformance <exe> [--id ID --version VERSION] [--fixture-mode]\n\
kerminal-plugin asset-url < request.json\n\
kerminal-plugin --licenses"
        );
        return Ok(());
    }
    // asset-url 只从 stdin 读批量 JSON，不接受任何选项或位置参数，
    // 保持纯计算契约：不发网络请求、不读取工作区或凭据。
    if command == "asset-url" {
        if arguments.next().is_some() {
            return Err("asset-url 不接受参数，请求请从 stdin 传入".into());
        }
        return asset_url::run();
    }
    let allowed: &[(&str, bool)] = match command {
        "create" => &[
            ("--template", true),
            ("--no-backend", false),
            ("--out", true),
            ("--official", false),
        ],
        "build" => &[
            ("--project", true),
            ("--platform", true),
            ("--official", false),
        ],
        "check" => &[("--platform", true), ("--official", false)],
        "package" => &[
            ("--dir", true),
            ("--out", true),
            ("--platform", true),
            ("--official", false),
        ],
        "dev" => &[
            ("--project", true),
            ("--kerminal", true),
            ("--official", false),
        ],
        "conformance" => &[
            ("--id", true),
            ("--version", true),
            ("--fixture-mode", false),
        ],
        _ => return Err("未知命令，请运行 --help".into()),
    };
    let mut positionals = Vec::new();
    let mut options = BTreeMap::new();
    while let Some(argument) = arguments.next() {
        if argument.to_str().is_some_and(|text| text.starts_with("--")) {
            let name = argument.to_str().ok_or("选项必须是 UTF-8")?;
            let needs_value = allowed
                .iter()
                .find(|(candidate, _)| *candidate == name)
                .map(|(_, value)| *value)
                .ok_or("未知选项")?;
            let value = if needs_value {
                Some(arguments.next().ok_or("选项缺少值")?)
            } else {
                None
            };
            if options.insert(name.to_owned(), value).is_some() {
                return Err("选项不能重复".into());
            }
        } else {
            positionals.push(argument);
        }
    }
    let policy = if options.contains_key("--official") {
        ManifestPolicy::Official
    } else {
        ManifestPolicy::ThirdParty
    };
    let platform = option_text(&options, "--platform")?
        .map(parse_platform)
        .transpose()?;
    match command {
        "create" => {
            if positionals.len() != 1 {
                return Err("create 需要一个插件 ID".into());
            }
            let id = positionals[0].to_str().ok_or("插件 ID 必须是 UTF-8")?;
            let template = option_text(&options, "--template")?.unwrap_or("native");
            let output = option_path(&options, "--out").unwrap_or_else(|| PathBuf::from(id));
            create::create(
                id,
                template,
                options.contains_key("--no-backend"),
                &output,
                policy,
            )?;
            println!("已创建 {}", output.display());
        }
        "build" => {
            require_no_positionals(&positionals)?;
            let project = option_path(&options, "--project").unwrap_or_else(|| PathBuf::from("."));
            println!("{}", build::build(&project, platform, policy)?.display());
        }
        "check" => {
            if positionals.len() > 1 {
                return Err("check 最多接受一个路径".into());
            }
            let path = positionals
                .first()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".kerminal/build"));
            let inferred = if path.is_file() {
                Some(infer_asset_platform(&path).ok_or("归档文件名缺少有效平台")?)
            } else {
                None
            };
            if inferred.is_some() && platform.is_some() && inferred != platform {
                return Err("归档文件名与目标平台不一致".into());
            }
            let chosen = platform.or(inferred);
            let validated = archive::check(&path, chosen, policy)?;
            if let Some(inferred) = inferred {
                let expected = format!(
                    "{}-{}-{}.kplugin",
                    validated.manifest.id,
                    validated.manifest.version,
                    platform_name(inferred)?
                );
                if path.file_name().and_then(|name| name.to_str()) != Some(expected.as_str())
                    || chosen != Some(inferred)
                {
                    return Err("归档文件名与清单或目标平台不一致".into());
                }
            }
            println!(
                "校验通过 {} {}",
                validated.manifest.id, validated.manifest.version
            );
        }
        "package" => {
            require_no_positionals(&positionals)?;
            let dir =
                option_path(&options, "--dir").unwrap_or_else(|| PathBuf::from(".kerminal/build"));
            let out = option_path(&options, "--out")
                .unwrap_or_else(|| PathBuf::from(".kerminal/packages"));
            let manifest = validate_package(&dir, policy)?.manifest;
            let platform = platform.unwrap_or(match manifest.runtime {
                PluginRuntime::Sidecar { .. } => {
                    Platform::current().ok_or("当前平台不支持 Sidecar")?
                }
                _ => Platform::Any,
            });
            println!(
                "{}",
                archive::package(&dir, &out, platform, policy)?.display()
            );
        }
        "dev" => {
            require_no_positionals(&positionals)?;
            let project = option_path(&options, "--project").unwrap_or_else(|| PathBuf::from("."));
            let executable = option_path(&options, "--kerminal");
            dev::run(&project, executable.as_deref(), policy)?;
        }
        "conformance" => {
            if positionals.len() != 1 {
                return Err("conformance 需要一个可执行文件".into());
            }
            let id = option_text(&options, "--id")?;
            let version = option_text(&options, "--version")?;
            let manifest;
            let (id, version) = match (id, version) {
                (Some(id), Some(version)) => (id, version),
                (None, None) => {
                    manifest = parse_manifest(
                        &fs::read("kerminal-plugin.json")?,
                        ManifestPolicy::Official,
                    )?;
                    (manifest.id.as_str(), manifest.version.as_str())
                }
                _ => return Err("--id 与 --version 必须同时提供".into()),
            };
            conformance::run(
                Path::new(&positionals[0]).canonicalize()?.as_path(),
                id,
                version,
                options.contains_key("--fixture-mode"),
            )?;
        }
        _ => unreachable!(),
    }
    Ok(())
}

/// 只在命令约定的名称后缀中识别平台，不把任意文件名视为可信资产。
fn infer_asset_platform(path: &Path) -> Option<Platform> {
    let name = path.file_name()?.to_str()?;
    [
        "windows-x86_64",
        "windows-aarch64",
        "linux-x86_64",
        "linux-aarch64",
        "macos-x86_64",
        "macos-aarch64",
        "any",
    ]
    .into_iter()
    .find(|platform| name.ends_with(&format!("-{platform}.kplugin")))
    .and_then(|platform| parse_platform(platform).ok())
}

/// 字符串选项拒绝非 UTF-8，路径选项则保留操作系统原始路径。
fn option_text<'a>(
    options: &'a BTreeMap<String, Option<OsString>>,
    name: &str,
) -> Result<Option<&'a str>> {
    options
        .get(name)
        .and_then(Option::as_ref)
        .map(|value| value.to_str().ok_or_else(|| "选项必须是 UTF-8".into()))
        .transpose()
}

/// 路径使用 OsString，避免 Windows 非 ASCII 工作目录被错误转换。
fn option_path(options: &BTreeMap<String, Option<OsString>>, name: &str) -> Option<PathBuf> {
    options
        .get(name)
        .and_then(Option::as_ref)
        .map(PathBuf::from)
}

/// 无位置参数的命令必须拒绝多余输入，避免脚本引用了错误目录却继续执行。
fn require_no_positionals(positionals: &[OsString]) -> Result<()> {
    if positionals.is_empty() {
        Ok(())
    } else {
        Err("该命令不接受位置参数".into())
    }
}
