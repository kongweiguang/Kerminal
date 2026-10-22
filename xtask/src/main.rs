//! 开发、质量门禁与本地交付命令入口。
//!
//! @author kongweiguang

mod architecture;
mod artifact_manifest;
mod command;

use std::{env, error::Error, path::PathBuf};

/// 将 CLI 参数映射到少量稳定的工作流，避免 CI 和本地开发各自维护命令顺序。
fn main() {
    if let Err(error) = dispatch(env::args().skip(1).collect()) {
        eprintln!("xtask failed: {error}");
        std::process::exit(1);
    }
}

/// 运行统一任务入口；所有子命令都从工作区根执行并继承调用者的工具链环境。
fn dispatch(arguments: Vec<String>) -> Result<(), Box<dyn Error>> {
    let root = command::workspace_root()?;
    let Some(task) = arguments.first().map(String::as_str) else {
        return Err("expected a task: architecture, fmt, check, clippy, test, bench, smoke, session-acceptance, package --target <triple>, artifact-manifest, source-fingerprint, ci".into());
    };

    match task {
        "architecture" => architecture::check(&root),
        "fmt" => command::cargo(&root, &["--locked", "fmt", "--all", "--", "--check"]),
        "check" => command::cargo(
            &root,
            &["--locked", "check", "--workspace", "--all-targets"],
        ),
        "clippy" => command::cargo(
            &root,
            &[
                "--locked",
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        ),
        "test" => command::cargo(&root, &["--locked", "test", "--workspace"]),
        "bench" => command::cargo(&root, &["--locked", "bench", "--workspace"]),
        "smoke" => command::run_desktop(&root, true),
        "run" => command::run_desktop(&root, false),
        "session-acceptance" => command::run_session_acceptance(&root, &arguments[1..]),
        "package" => {
            let target = parse_package_target(&arguments)?;
            command::package(&root, target)
        }
        "artifact-manifest" => {
            let bundle_dir = arguments
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join("target/bundles"));
            let output = arguments
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join("target/kerminal-artifacts.json"));
            artifact_manifest::write(&root, &bundle_dir, &output)
        }
        "source-fingerprint" => {
            if arguments.len() != 1 {
                return Err("usage: cargo xtask source-fingerprint".into());
            }
            println!("{}", artifact_manifest::source_fingerprint_json(&root)?);
            Ok(())
        }
        "ci" | "verify" => {
            architecture::check(&root)?;
            command::cargo(&root, &["fmt", "--all", "--", "--check"])?;
            command::cargo(
                &root,
                &[
                    "--locked",
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--",
                    "-D",
                    "warnings",
                ],
            )?;
            command::cargo(&root, &["--locked", "test", "--workspace"])
        }
        _ => Err(format!("unknown xtask command: {task}").into()),
    }
}

/// 要求打包调用显式传入 Rust target triple，使 Mac 双架构和 CI 矩阵使用同一选择依据。
fn parse_package_target(arguments: &[String]) -> Result<&str, Box<dyn Error>> {
    match (arguments.get(1).map(String::as_str), arguments.get(2)) {
        (Some("--target"), Some(target)) if arguments.len() == 3 => Ok(target),
        _ => Err("usage: cargo xtask package --target <triple>".into()),
    }
}

#[cfg(test)]
#[path = "../tests/unit/package_arguments.rs"]
mod tests;
