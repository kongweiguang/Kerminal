//! 固定并内嵌目标平台的官方 esbuild。
//! @author kongweiguang

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

/// 构建期仅接收固定摘要的官方归档，交叉编译时按目标而非构建机选择程序。
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-env-changed=KERMINAL_ESBUILD_ARCHIVE");
    let target = env::var("TARGET")?;
    let (package, checksum, entry) = match target.as_str() {
        "x86_64-pc-windows-msvc" => (
            "win32-x64",
            "7286c3611b6f1f4c4d9ec90adcbc478407ff0d28ead96567f361d53e67613e19",
            "package/esbuild.exe",
        ),
        "x86_64-unknown-linux-gnu" => (
            "linux-x64",
            "9573bb2233aab0f9ea7647d5cca9726113cc1768de61d66b17267f4db84488f6",
            "package/bin/esbuild",
        ),
        "x86_64-apple-darwin" => (
            "darwin-x64",
            "abb6a7a895aaf2cc0df36fecc3bf0042479ffec51a643a056d54e9109f27db55",
            "package/bin/esbuild",
        ),
        "aarch64-apple-darwin" => (
            "darwin-arm64",
            "1980cde09749094452b20d36ff267585ccb3f72749c7bc97291cd9996ccf5a2a",
            "package/bin/esbuild",
        ),
        _ => return Err(format!("esbuild 没有为目标 {target} 配置受支持的资产").into()),
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("缺少 OUT_DIR")?);
    let archive = match env::var_os("KERMINAL_ESBUILD_ARCHIVE") {
        Some(path) => {
            let path = PathBuf::from(path);
            println!("cargo:rerun-if-changed={}", path.display());
            verified_archive(&path, checksum)?
        }
        None => cached_archive(&out, package, checksum)?,
    };
    let mut tar = tar::Archive::new(GzDecoder::new(archive.as_slice()));
    let mut found = false;
    for item in tar.entries()? {
        let mut item = item?;
        if item.path_bytes().as_ref() == entry.as_bytes() {
            if found || !item.header().entry_type().is_file() || item.size() > 64 * 1024 * 1024 {
                return Err("esbuild 归档中的程序无效".into());
            }
            let mut destination = fs::File::create(out.join("kerminal-esbuild"))?;
            io::copy(&mut item, &mut destination)?;
            found = true;
        }
    }
    if !found {
        return Err("esbuild 归档缺少目标程序".into());
    }
    Ok(())
}

/// 无论来自缓存还是手动归档都重新验摘要，避免中断或本地篡改变成可执行文件。
fn verified_archive(path: &Path, checksum: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if fs::metadata(path)?.len() > 64 * 1024 * 1024 {
        return Err("esbuild 归档超过大小限制".into());
    }
    let archive = fs::read(path)?;
    if format!("{:x}", Sha256::digest(&archive)) != checksum {
        return Err("esbuild 归档摘要不符，拒绝构建".into());
    }
    Ok(archive)
}

/// 已校验的构建缓存可离线复用；失败或中断的下载不能污染现有缓存。
fn cached_archive(
    out: &Path,
    package: &str,
    checksum: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let destination = out.join("esbuild.tgz");
    if let Ok(archive) = verified_archive(&destination, checksum) {
        return Ok(archive);
    }
    let partial = out.join(format!("esbuild-download-{}.tmp", std::process::id()));
    let url = format!("https://registry.npmjs.org/@esbuild/{package}/-/{package}-0.28.2.tgz");
    let status = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--connect-timeout",
            "10",
            "--max-time",
            "45",
            "--output",
        ])
        .arg(&partial)
        .arg(url)
        .status();
    if !status.is_ok_and(|status| status.success()) {
        let _ = fs::remove_file(&partial);
        return Err("下载固定版本 esbuild 失败；请检查 curl/网络，或用 KERMINAL_ESBUILD_ARCHIVE 指向对应官方归档后重试".into());
    }
    let archive = match verified_archive(&partial, checksum) {
        Ok(archive) => archive,
        Err(error) => {
            let _ = fs::remove_file(&partial);
            return Err(error);
        }
    };
    if destination.exists() {
        fs::remove_file(&destination)?;
    }
    fs::rename(&partial, &destination)?;
    Ok(archive)
}
