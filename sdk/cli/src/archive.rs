//! 安全读取和写入 gzip tar，清单规则由协议 crate 统一负责。
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use flate2::{Compression, read::MultiGzDecoder, write::GzEncoder};
use kerminal_plugin_protocol::{
    MAX_PACKAGE_BYTES, MAX_PACKAGE_ENTRIES, MAX_PACKAGE_FILE_BYTES, ManifestPolicy, Platform,
    PluginRuntime, ValidatedPackage, validate_package, validate_package_path,
};
use tempfile::TempDir;

use crate::{Result, platform_name};

/// 目录和归档最终都经过同一个协议校验，来源策略由调用方显式指定。
pub fn check(
    path: &Path,
    platform: Option<Platform>,
    policy: ManifestPolicy,
) -> Result<ValidatedPackage> {
    let temporary;
    let root = if path.is_dir() {
        path
    } else {
        temporary = unpack(path)?;
        temporary.path()
    };
    let package = validate_package(root, policy)?;
    if let Some(platform) = platform {
        package.validate_for_platform(platform)?;
    }
    Ok(package)
}

/// 解压只接受普通文件和目录，限制在写入前执行且不信任归档内的链接与大小。
pub fn unpack(path: &Path) -> Result<TempDir> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_PACKAGE_BYTES + 16 * 1024 * 1024
    {
        return Err("归档必须是有界普通文件".into());
    }
    let directory = tempfile::tempdir()?;
    let decoder = MultiGzDecoder::new(File::open(path)?);
    let mut archive = tar::Archive::new(decoder);
    let mut paths = BTreeMap::<String, String>::new();
    let mut explicit = BTreeSet::new();
    let mut count = 0usize;
    let mut total = 0u64;
    for result in archive.entries()?.raw(true) {
        let mut entry = result?;
        let kind = entry.header().entry_type();
        let is_directory = kind.is_dir();
        if !is_directory && !kind.is_file() {
            return Err("归档只允许普通文件和目录".into());
        }
        let raw = entry.path_bytes();
        let name = std::str::from_utf8(raw.as_ref())?;
        let name = if is_directory {
            name.strip_suffix('/').unwrap_or(name)
        } else {
            name
        };
        validate_package_path(name)?;
        count += 1;
        if count > MAX_PACKAGE_ENTRIES || !explicit.insert(name.to_owned()) {
            return Err("归档条目重复或数量超限".into());
        }
        register_path(&mut paths, name)?;
        let size = entry.size();
        if (is_directory && size != 0) || size > MAX_PACKAGE_FILE_BYTES {
            return Err("归档条目大小无效".into());
        }
        total = total.checked_add(size).ok_or("归档大小溢出")?;
        if total > MAX_PACKAGE_BYTES {
            return Err("归档解压大小超限".into());
        }
        let destination = directory.path().join(name);
        if is_directory {
            fs::create_dir_all(destination)?;
        } else {
            fs::create_dir_all(destination.parent().ok_or("归档路径缺少父目录")?)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)?;
            if io::copy(&mut entry, &mut output)? != size {
                return Err("归档文件不完整".into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let executable = entry.header().mode()? & 0o111 != 0;
                fs::set_permissions(
                    &destination,
                    fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
                )?;
            }
        }
    }
    let mut decoder = archive.into_inner();
    let mut tail = Vec::new();
    Read::by_ref(&mut decoder)
        .take(1024 * 1024 + 1)
        .read_to_end(&mut tail)?;
    if tail.len() > 1024 * 1024 || tail.iter().any(|byte| *byte != 0) {
        return Err("归档结束后包含无效数据".into());
    }
    Ok(directory)
}

/// 同时记录祖先路径，避免隐式目录在大小写不敏感平台合并成另一目录。
fn register_path(paths: &mut BTreeMap<String, String>, path: &str) -> Result<()> {
    let mut current = String::new();
    for segment in path.split('/') {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(segment);
        for folded in [current.to_lowercase(), current.to_uppercase()] {
            if paths
                .get(&folded)
                .is_some_and(|existing| existing != &current)
            {
                return Err("归档路径存在大小写冲突".into());
            }
            paths.insert(folded, current.clone());
        }
    }
    Ok(())
}

/// 只打包已校验清单中的文件，且平台包排除其他平台的 Sidecar。
pub fn package(
    root: &Path,
    output_dir: &Path,
    platform: Platform,
    policy: ManifestPolicy,
) -> Result<PathBuf> {
    let validated = validate_package(root, policy)?;
    validated.validate_for_platform(platform)?;
    let name = format!(
        "{}-{}-{}.kplugin",
        validated.manifest.id,
        validated.manifest.version,
        platform_name(platform)?
    );
    fs::create_dir_all(output_dir)?;
    let target = output_dir.join(name);
    let mut staged = tempfile::NamedTempFile::new_in(output_dir)?;
    {
        let gzip = GzEncoder::new(staged.as_file_mut(), Compression::default());
        let mut archive = tar::Builder::new(gzip);
        let sidecars = match &validated.manifest.runtime {
            PluginRuntime::Sidecar { main, .. } => Some(main),
            _ => None,
        };
        let selected = sidecars.and_then(|main| main.get(&platform));
        for (name, recorded) in &validated.files {
            if sidecars.is_some_and(|main| {
                main.values().any(|path| path == name) && selected != Some(name)
            }) {
                continue;
            }
            let path = root.join(name);
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() != recorded.size
            {
                return Err("打包期间文件发生变化".into());
            }
            let mut input = File::open(path)?;
            let mut header = tar::Header::new_ustar();
            header.set_size(recorded.size);
            header.set_mode(if selected == Some(name) { 0o755 } else { 0o644 });
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_cksum();
            archive.append_data(&mut header, name, &mut input)?;
        }
        archive.into_inner()?.finish()?;
    }
    staged.as_file_mut().flush()?;
    staged.as_file().sync_all()?;
    let verified = check(staged.path(), Some(platform), policy)?;
    if verified.manifest != validated.manifest
        || verified.views != validated.views
        || verified.configuration_schemas != validated.configuration_schemas
    {
        return Err("打包期间清单、视图或配置 schema 发生变化".into());
    }
    staged
        .persist_noclobber(&target)
        .map_err(|error| error.error)?;
    Ok(target)
}
