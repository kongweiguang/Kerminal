//! 直接调用协议包的唯一目录校验入口，不维护另一套示例清单规则。
//! @author kongweiguang

use std::env;
use std::path::Path;

use kerminal_plugin_protocol::{ManifestPolicy, Platform, validate_package};

/// 调用方显式指定可信来源与目标平台，避免本地测试绕过官方命名空间规则。
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let root = args.next().ok_or("package directory required")?;
    let policy = match args.next().as_deref() {
        Some("third-party") => ManifestPolicy::ThirdParty,
        Some("official") => ManifestPolicy::Official,
        _ => return Err("policy must be third-party or official".into()),
    };
    let platform = match args.next().as_deref() {
        Some("any") => Platform::Any,
        Some("windows-x86_64") => Platform::WindowsX86_64,
        _ => return Err("platform must be any or windows-x86_64".into()),
    };
    if args.next().is_some() {
        return Err("unexpected arguments".into());
    }
    let package = validate_package(Path::new(&root), policy)?;
    package.validate_for_platform(platform)?;
    println!(
        "{} {} validated",
        package.manifest.id, package.manifest.version
    );
    Ok(())
}
