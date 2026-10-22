//! 容器文件复制使用的流式 tar 打包与受限解包工具。
//!
//! @author kongweiguang

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use kerminal_domain::files::TransferKind;
use tar::{Archive, Builder, EntryType, Header};

/// 将本地文件或目录逐项写入 tar writer，不把整个目录或文件读入内存。
pub(crate) fn build_local_archive(
    source: &Path,
    kind: TransferKind,
    archive_name: &str,
    writer: impl Write,
    mut on_file_bytes: impl FnMut(u64),
) -> io::Result<()> {
    validate_archive_name(archive_name)?;
    let metadata = fs::symlink_metadata(source)?;
    match (
        kind,
        metadata.file_type().is_file(),
        metadata.file_type().is_dir(),
    ) {
        (TransferKind::File, true, _) | (TransferKind::Directory, _, true) => {}
        (_, false, false) => return Err(invalid_data("传输源类型不受支持")),
        _ => return Err(invalid_data("传输源类型与请求不匹配")),
    }

    let mut builder = Builder::new(writer);
    append_source_entry(&mut builder, source, archive_name, &mut on_file_bytes)?;
    builder.finish()
}

/// 只在调用方已独占创建的空目录内解包，保证错误清理不会删到并发创建者的路径。
pub(crate) fn extract_local_archive_into_existing(
    reader: impl Read,
    staging_root: &Path,
    archive_name: &str,
    kind: TransferKind,
    mut on_file_bytes: impl FnMut(u64),
) -> io::Result<PathBuf> {
    validate_archive_name(archive_name)?;
    let staging_metadata = fs::symlink_metadata(staging_root)?;
    if staging_metadata.file_type().is_symlink() || !staging_metadata.is_dir() {
        return Err(invalid_data("staging 根必须是本次创建的普通目录"));
    }
    if fs::read_dir(staging_root)?.next().is_some() {
        return Err(invalid_data("staging 根必须为空"));
    }
    let object_root = staging_root.join(archive_name);
    let mut archive = Archive::new(reader);
    let entries = archive.entries()?;
    let mut root_prefix: Option<bool> = None;
    let mut seen = std::collections::HashSet::<PathBuf>::new();
    let mut file_entries = 0usize;

    for entry in entries {
        let mut entry = entry?;
        let archive_path = entry.path()?.into_owned();
        let components = safe_components(&archive_path)?;
        if components.is_empty() {
            return Err(invalid_data("tar 路径为空"));
        }
        let entry_type = entry.header().entry_type();
        let is_directory = entry_type.is_dir();
        let is_regular_file = entry_type.is_file() || entry_type == EntryType::Continuous;
        if !is_directory && !is_regular_file {
            return Err(invalid_data("tar 中包含不允许的链接或特殊文件"));
        }

        if root_prefix.is_none() {
            let starts_with_archive_root = components[0] == archive_name;
            root_prefix = Some(
                kind == TransferKind::Directory
                    && starts_with_archive_root
                    && (components.len() > 1 || is_directory),
            );
        }
        let relative_components = match (kind, root_prefix.unwrap_or(false)) {
            (TransferKind::Directory, true) => {
                if components[0] != archive_name {
                    return Err(invalid_data("tar 目录包含多个顶层对象"));
                }
                &components[1..]
            }
            (TransferKind::Directory, false) => components.as_slice(),
            (TransferKind::File, _) => {
                if !is_regular_file || components.len() != 1 {
                    return Err(invalid_data("tar 文件包含多个或嵌套对象"));
                }
                file_entries += 1;
                if file_entries > 1 {
                    return Err(invalid_data("tar 文件包含重复对象"));
                }
                &[]
            }
        };

        if kind == TransferKind::Directory && relative_components.is_empty() {
            if !is_directory {
                return Err(invalid_data("tar 目录根对象类型无效"));
            }
            continue;
        }
        let relative = relative_components
            .iter()
            .fold(PathBuf::new(), |mut path, component| {
                path.push(component);
                path
            });
        if !seen.insert(relative.clone()) {
            return Err(invalid_data("tar 中包含重复路径"));
        }
        let destination = if kind == TransferKind::File {
            object_root.clone()
        } else {
            object_root.join(&relative)
        };

        if is_directory {
            if kind == TransferKind::File {
                return Err(invalid_data("tar 文件目标不能是目录"));
            }
            ensure_safe_parent(staging_root, &destination)?;
            match fs::create_dir(&destination) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    if !fs::symlink_metadata(&destination)?.file_type().is_dir() {
                        return Err(invalid_data("tar 路径已被非目录项占用"));
                    }
                }
                Err(error) => return Err(error),
            }
            set_safe_permissions(&destination, entry.header().mode().unwrap_or(0o755))?;
            continue;
        }

        if let Some(parent) = destination.parent() {
            create_safe_directories(staging_root, parent)?;
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)?;
        let expected_size = entry.header().size()?;
        let copied = io::copy(
            &mut CountingReader::new(&mut entry, &mut on_file_bytes),
            &mut output,
        )?;
        if copied != expected_size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "tar 文件长度与 header 不一致",
            ));
        }
        output.flush()?;
        set_safe_permissions(&destination, entry.header().mode().unwrap_or(0o644))?;
    }

    if kind == TransferKind::File && file_entries != 1 {
        return Err(invalid_data("tar 文件缺少唯一文件对象"));
    }
    if kind == TransferKind::Directory {
        create_safe_directories(staging_root, &object_root)?;
    }
    Ok(object_root)
}

/// 递归遍历时拒绝符号链接和设备节点，并按稳定名称顺序写出每个 entry。
fn append_source_entry<W: Write>(
    builder: &mut Builder<W>,
    source: &Path,
    archive_path: &str,
    on_file_bytes: &mut impl FnMut(u64),
) -> io::Result<()> {
    validate_archive_path(archive_path)?;
    let metadata = fs::symlink_metadata(source)?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(invalid_data("目录传输暂不跟随或重建符号链接"));
    }
    if file_type.is_dir() {
        let mut header = tar_header(EntryType::Directory, 0, mode_for(&metadata));
        builder.append_data(&mut header, archive_path, io::empty())?;
        let mut children = fs::read_dir(source)?
            .map(|entry| entry.map(|entry| (entry.file_name(), entry.path())))
            .collect::<Result<Vec<_>, _>>()?;
        children.sort_by(|left, right| left.0.cmp(&right.0));
        for (name, child) in children {
            let name = name
                .to_str()
                .ok_or_else(|| invalid_data("传输路径包含不可编码的目录项"))?;
            if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
                return Err(invalid_data("传输路径包含无效目录项"));
            }
            let child_path = format!("{archive_path}/{name}");
            append_source_entry(builder, &child, &child_path, on_file_bytes)?;
        }
        return Ok(());
    }
    if !file_type.is_file() {
        return Err(invalid_data("目录传输不支持设备、管道或 socket"));
    }

    let mut file = File::open(source)?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file() || opened_metadata.len() != metadata.len() {
        return Err(invalid_data("本地文件在打包期间发生变化"));
    }
    let mut header = tar_header(
        EntryType::Regular,
        opened_metadata.len(),
        mode_for(&opened_metadata),
    );
    builder.append_data(
        &mut header,
        archive_path,
        &mut CountingReader::new(&mut file, on_file_bytes),
    )
}

/// 在后台 tar 读写期间按源文件内容字节更新进度，不把归档头部算作用户文件数据。
struct CountingReader<'a, R, F> {
    inner: R,
    on_bytes: &'a mut F,
}

impl<'a, R, F> CountingReader<'a, R, F> {
    /// 以借用方式包装单个归档文件，避免读取整文件后再统计导致额外内存占用。
    fn new(inner: R, on_bytes: &'a mut F) -> Self {
        Self { inner, on_bytes }
    }
}

impl<R, F> Read for CountingReader<'_, R, F>
where
    R: Read,
    F: FnMut(u64),
{
    /// 只在底层成功读取后报告真实字节数，使取消和短读不会提前推进界面进度。
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        if count > 0 {
            (self.on_bytes)(count as u64);
        }
        Ok(count)
    }
}

/// 固定 GNU header 的类型、长度、受限权限和校验和，避免复制宿主所有权元数据。
fn tar_header(entry_type: EntryType, size: u64, mode: u32) -> Header {
    let mut header = Header::new_gnu();
    header.set_entry_type(entry_type);
    header.set_size(size);
    header.set_mode(mode & 0o777);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    header
}

/// 归档名只允许作为单一 UTF-8 目录项，避免从源文件名构造 tar 绝对路径。
fn validate_archive_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', ':'])
        || name.chars().any(char::is_control)
    {
        return Err(invalid_data("传输对象名称无效"));
    }
    Ok(())
}

/// tar entry 仅允许相对普通路径组件，不接受平台前缀、根目录或父级穿越。
fn safe_components(path: &Path) -> io::Result<Vec<String>> {
    let mut components = Vec::new();
    for component in path.components() {
        let Component::Normal(value) = component else {
            return Err(invalid_data("tar 路径必须是相对普通路径"));
        };
        let value = value
            .to_str()
            .ok_or_else(|| invalid_data("tar 路径包含不可编码目录项"))?;
        if value.is_empty()
            || value.contains(['/', '\\', ':'])
            || value.chars().any(char::is_control)
        {
            return Err(invalid_data("tar 路径目录项无效"));
        }
        components.push(value.to_owned());
    }
    Ok(components)
}

/// 按宿主平台保留常规读写位，丢弃 setuid、所有者和组信息。
fn mode_for(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        if metadata.permissions().readonly() {
            0o444
        } else {
            0o644
        }
    }
}

/// 只在 staging 根内补建目录，并逐级确认没有符号链接或非目录覆盖路径。
fn create_safe_directories(root: &Path, target: &Path) -> io::Result<()> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| invalid_data("staging 路径越界"))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(invalid_data("staging 目录路径无效"));
        };
        current.push(name);
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&current)?;
                if !metadata.file_type().is_dir() {
                    return Err(invalid_data("staging 父路径不是普通目录"));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// 确认待写目标的父目录位于当前 staging 根，避免 tar 中已有路径类型改变提取边界。
fn ensure_safe_parent(root: &Path, target: &Path) -> io::Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| invalid_data("staging 对象缺少父目录"))?;
    create_safe_directories(root, parent)
}

/// 只应用归档中的常规权限位，Windows 无法精确保留时沿用其原生 ACL。
fn set_safe_permissions(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// 验证将本地相对路径写入 POSIX tar namespace 时没有出现根目录或父级段。
fn validate_archive_path(path: &str) -> io::Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains(['\\', ':'])
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || path.chars().any(char::is_control)
    {
        return Err(invalid_data("归档路径不是安全的相对路径"));
    }
    Ok(())
}

/// 使用固定错误类标识不安全归档，不将具体路径回显给上层用户。
fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
