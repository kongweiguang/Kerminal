//! 对实际打包文件生成可复核的 SHA-256 清单，不从预期配置推测产物。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    str,
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

type ManifestResult<T> = Result<T, Box<dyn Error>>;
const SOURCE_FINGERPRINT_FORMAT: &str = "kerminal-source-tree-v1";
const SOURCE_FINGERPRINT_ALGORITHM: &str = "sha256";

/// 扫描已存在的打包目录并原子写出产物摘要与对应时刻的源码树指纹。
pub fn write(root: &Path, bundle_dir: &Path, output: &Path) -> ManifestResult<()> {
    if !bundle_dir.is_dir() {
        return Err(format!("bundle directory does not exist: {}", bundle_dir.display()).into());
    }
    let mut artifacts = Vec::new();
    collect_artifacts(bundle_dir, bundle_dir, &mut artifacts)?;
    if artifacts.is_empty() {
        return Err(format!("no package artifacts found under {}", bundle_dir.display()).into());
    }
    artifacts.sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));

    let (revision, dirty, fingerprint) = source_state(root)?;
    let document = json!({
        "schemaVersion": 1,
        "source": {
            "revision": revision,
            "dirty": dirty,
            "fingerprint": fingerprint.as_json()
        },
        "artifacts": artifacts,
        "signing": {"verified": false, "note": "This manifest records hashes only; it does not verify platform signatures."}
    });
    let encoded = serde_json::to_vec_pretty(&document)?;
    write_atomically(output, &encoded)?;
    println!(
        "wrote {} artifact entries to {}",
        document["artifacts"].as_array().map_or(0, Vec::len),
        output.display()
    );
    Ok(())
}

/// 遍历包目录中的普通文件；符号链接被拒绝，避免清单意外读取包目录之外的数据。
fn collect_artifacts(root: &Path, directory: &Path, output: &mut Vec<Value>) -> ManifestResult<()> {
    for item in fs::read_dir(directory)? {
        let item = item?;
        let path = item.path();
        let file_type = item.file_type()?;
        if file_type.is_symlink() {
            return Err(format!(
                "package output contains a symbolic link: {}",
                path.display()
            )
            .into());
        }
        if file_type.is_dir() {
            collect_artifacts(root, &path, output)?;
            continue;
        }
        if !file_type.is_file() {
            return Err(format!(
                "package output contains an unsupported file type: {}",
                path.display()
            )
            .into());
        }

        let (bytes, digest) = hash_file(&path)?;
        if bytes == 0 {
            return Err(format!(
                "package output contains an empty artifact: {}",
                path.display()
            )
            .into());
        }
        let relative = path
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        output.push(json!({"path": relative, "bytes": bytes, "sha256": digest}));
    }
    Ok(())
}

/// 流式计算 SHA-256，避免将大型 AppImage 或安装包整体读入内存。
fn hash_file(path: &Path) -> ManifestResult<(u64, String)> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut bytes = 0_u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        bytes += count as u64;
    }
    let hash = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok((bytes, hash))
}

/// 保留 Git revision/status 作为上下文，并计算可重复的工作树快照摘要。
fn source_state(root: &Path) -> ManifestResult<(String, bool, SourceFingerprint)> {
    let revision = git_output(root, &["rev-parse", "--verify", "HEAD"])?;
    let status = git_output(root, &["status", "--porcelain"])?;
    let fingerprint = source_fingerprint(root)?;
    Ok((revision, !status.is_empty(), fingerprint))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceFingerprint {
    format: &'static str,
    algorithm: &'static str,
    digest: String,
}

impl SourceFingerprint {
    /// 以显式格式和算法标识暴露摘要，便于后续工具安全比较同一算法的结果。
    fn as_json(&self) -> Value {
        json!({
            "format": self.format,
            "algorithm": self.algorithm,
            "digest": self.digest.as_str()
        })
    }
}

/// 为 release 前后调用提供只输出稳定 JSON 的同一指纹实现，不附加 revision 或状态文案。
pub(super) fn source_fingerprint_json(root: &Path) -> ManifestResult<String> {
    Ok(serde_json::to_string(&source_fingerprint(root)?.as_json())?)
}

/// 先固定 canonical workspace root，再对 HEAD、index 和非忽略未跟踪文件取并集。
fn source_fingerprint(root: &Path) -> ManifestResult<SourceFingerprint> {
    let filesystem_root = fs::canonicalize(root)?;
    if !filesystem_root.is_dir() {
        return Err(format!("workspace root is not a directory: {}", root.display()).into());
    }
    let mut paths = BTreeMap::<String, bool>::new();
    for path in git_paths(root, &["ls-tree", "-r", "--name-only", "-z", "HEAD"])? {
        paths.insert(path, true);
    }
    for path in git_paths(root, &["ls-files", "--cached", "-z"])? {
        paths.insert(path, true);
    }
    for path in git_paths(root, &["ls-files", "--others", "--exclude-standard", "-z"])? {
        paths.entry(path).or_insert(false);
    }

    let mut digest = Sha256::new();
    digest.update(SOURCE_FINGERPRINT_FORMAT.as_bytes());
    digest.update([0]);
    for (relative_path, tracked) in paths {
        validate_relative_git_path(&relative_path)?;
        if is_target_path(&relative_path) {
            continue;
        }
        let absolute_path = filesystem_root.join(&relative_path);
        match source_file_state(&filesystem_root, &absolute_path, &relative_path, tracked)? {
            Some((bytes, content_digest)) => {
                digest.update([1]);
                update_length_prefixed(&mut digest, relative_path.as_bytes());
                digest.update(bytes.to_le_bytes());
                digest.update(content_digest.as_bytes());
            }
            None => {
                digest.update([2]);
                update_length_prefixed(&mut digest, relative_path.as_bytes());
            }
        }
    }

    Ok(SourceFingerprint {
        format: SOURCE_FINGERPRINT_FORMAT,
        algorithm: SOURCE_FINGERPRINT_ALGORITHM,
        digest: digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    })
}

/// 从 Git 的 NUL 分隔输出解析路径，避免换行、空格和引号改变路径边界。
fn git_paths(root: &Path, arguments: &[&str]) -> ManifestResult<Vec<String>> {
    let output = git_output_bytes(root, arguments)?;
    output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            str::from_utf8(path)
                .map(str::to_owned)
                .map_err(|error| format!("git returned a non-UTF-8 source path: {error}").into())
        })
        .collect()
}

/// 只排除 workspace 根构建输出，保留业务源码中合法的嵌套 target 目录。
fn is_target_path(relative_path: &str) -> bool {
    relative_path == "target" || relative_path.starts_with("target/")
}

/// 限制 Git 路径只能是规范的仓库相对路径，防止路径前缀逃逸或平台分隔符歧义。
fn validate_relative_git_path(relative_path: &str) -> ManifestResult<()> {
    let path = Path::new(relative_path);
    if relative_path.is_empty()
        || relative_path.contains('\\')
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir
                    | std::path::Component::ParentDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(format!("git returned an unsafe source path: {relative_path:?}").into());
    }
    Ok(())
}

/// 不穿越父目录链接或 reparse point；已跟踪路径缺失时写入 tombstone，未跟踪竞态则失败。
fn source_file_state(
    root: &Path,
    absolute_path: &Path,
    relative_path: &str,
    tracked: bool,
) -> ManifestResult<Option<(u64, String)>> {
    let relative = Path::new(relative_path);
    let mut current = root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(component) = component else {
            return Err(format!("git returned a non-normal source path: {relative_path:?}").into());
        };
        current.push(component);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && tracked => {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        if is_symlink_or_reparse(&metadata) {
            return Err(format!(
                "source tree contains a symbolic link or reparse point: {relative_path}"
            )
            .into());
        }
        if components.peek().is_some() {
            if !metadata.is_dir() {
                return Err(
                    format!("source path has a non-directory parent: {relative_path}").into(),
                );
            }
        } else if !metadata.is_file() {
            return Err(
                format!("source tree contains an unsupported file type: {relative_path}").into(),
            );
        }
    }

    let (bytes, digest) = hash_file(absolute_path)?;
    Ok(Some((bytes, digest)))
}

/// 检查所有平台的符号链接，并在 Windows 额外拒绝可穿越目录的任意 reparse point。
fn is_symlink_or_reparse(metadata: &fs::Metadata) -> bool {
    let file_type = metadata.file_type();
    #[cfg(windows)]
    {
        use std::os::windows::fs::{FileTypeExt, MetadataExt};

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        file_type.is_symlink()
            || file_type.is_symlink_dir()
            || file_type.is_symlink_file()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        file_type.is_symlink()
    }
}

/// 用固定宽度长度字段分隔路径与内容摘要，避免不同字段拼接成相同字节序列。
fn update_length_prefixed(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_le_bytes());
    digest.update(value);
}

/// 执行只读 git 查询并检查退出码；清单依赖本地仓库状态而非环境变量猜测。
fn git_output(root: &Path, arguments: &[&str]) -> ManifestResult<String> {
    let output = git_output_bytes(root, arguments)?;
    Ok(String::from_utf8(output)?.trim().to_owned())
}

/// 运行只读 Git 查询并保留原始字节，NUL 分隔路径不能经过文本 trim 或 quoted 输出。
fn git_output_bytes(root: &Path, arguments: &[&str]) -> ManifestResult<Vec<u8>> {
    let output = Command::new("git")
        .current_dir(root)
        .args(arguments)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(output.stdout)
}

/// 先在目标旁写临时文件再 rename，避免中断时留下格式完整但内容截断的清单。
fn write_atomically(output: &Path, bytes: &[u8]) -> ManifestResult<()> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut temporary = PathBuf::from(output);
    temporary.set_extension("json.tmp");
    fs::write(&temporary, bytes)?;
    match fs::rename(&temporary, output) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error.into())
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/artifact_manifest.rs"]
mod tests;
