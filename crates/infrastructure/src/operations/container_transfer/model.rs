//! 容器流式复制的路径、目标和本地清单校验。
//!
//! @author kongweiguang

use std::{
    fs,
    path::{Path, PathBuf},
};

use kerminal_domain::files::{ContainerRuntime, FileEndpoint, TransferKind};

use kerminal_application::{AppError, AppResult, operations::CommandExecutionTarget};

/// 一个归档对象及其树内普通文件/目录项；相对段保持独立以便作为 argv 安全传递。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LocalManifest {
    pub root_name: String,
    pub entries: Vec<LocalManifestEntry>,
    pub total_bytes: u64,
}

/// 清单项只包含可被 tar 解包器接受的普通目录或文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LocalManifestEntry {
    pub relative: Vec<String>,
    pub directory: bool,
    pub bytes: u64,
}

/// 记录本次远端容器命令需要的已校验引用，不含凭据或任意程序名。
#[derive(Debug, Clone)]
pub(super) struct ContainerEndpoint<'a> {
    pub host_id: Option<&'a kerminal_domain::identity::HostId>,
    pub runtime: ContainerRuntime,
    pub container_id: &'a str,
    pub path: &'a kerminal_domain::files::RemotePath,
}

/// 把容器 endpoint 映射为本机或已保存主机的命令目标，并验证 GNU/Linux 容器路径约束。
pub(super) fn container_endpoint(endpoint: &FileEndpoint) -> AppResult<ContainerEndpoint<'_>> {
    let FileEndpoint::Container {
        host_id,
        runtime,
        container_id,
        path,
    } = endpoint
    else {
        return Err(AppError::unsupported("容器传输只处理容器文件端点"));
    };
    validate_container_id(container_id)?;
    if !path.as_str().starts_with('/')
        || path.as_str().len() > 4096
        || path.as_str().split('/').any(|segment| segment == "..")
    {
        return Err(AppError::invalid("容器传输路径必须是安全的绝对路径"));
    }
    Ok(ContainerEndpoint {
        host_id: host_id.as_ref(),
        runtime: *runtime,
        container_id,
        path,
    })
}

/// 运行时枚举只映射固定 CLI 名称，配置和 MCP 字符串不能指定可执行程序。
pub(super) fn runtime_program(runtime: ContainerRuntime) -> &'static str {
    match runtime {
        ContainerRuntime::Docker => "docker",
        ContainerRuntime::Podman => "podman",
    }
}

/// 容器 ID 使用允许 Docker/Podman name 或 ID 的 ASCII 子集，拒绝 flag 与路径注入。
pub(super) fn validate_container_id(value: &str) -> AppResult<()> {
    let mut characters = value.chars();
    if !characters
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric())
        || value.len() > 128
        || !characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-')
        })
    {
        return Err(AppError::invalid("容器标识格式无效"));
    }
    Ok(())
}

/// 按主机 ID 显式选择本地 CLI 或已保存 SSH 主机，不制造临时 HostId。
pub(super) fn command_target(
    host_id: Option<&kerminal_domain::identity::HostId>,
) -> CommandExecutionTarget {
    host_id.map_or(CommandExecutionTarget::Local, |host| {
        CommandExecutionTarget::Host(host.clone())
    })
}

/// 从普通本机源收集稳定顺序的归档清单，拒绝符号链接和特殊文件而不跟随它们。
pub(super) fn collect_local_manifest(
    source: &Path,
    kind: TransferKind,
) -> AppResult<LocalManifest> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| AppError::infrastructure_with_source("读取本机传输源失败", error))?;
    let root_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| AppError::invalid("本机传输源必须有可编码的文件名"))?
        .to_owned();
    validate_archive_name(&root_name)?;
    let mut entries = Vec::new();
    let total_bytes;
    match (
        kind,
        metadata.file_type().is_file(),
        metadata.file_type().is_dir(),
        metadata.file_type().is_symlink(),
    ) {
        (TransferKind::File, true, _, false) => {
            total_bytes = metadata.len();
            entries.push(LocalManifestEntry {
                relative: Vec::new(),
                directory: false,
                bytes: metadata.len(),
            });
        }
        (TransferKind::Directory, _, true, false) => {
            let mut directory_bytes = 0_u64;
            collect_directory(source, Vec::new(), &mut entries, &mut directory_bytes)?;
            total_bytes = directory_bytes;
        }
        (_, _, _, true) => return Err(AppError::unsupported("传输暂不跟随或重建符号链接")),
        _ => return Err(AppError::invalid("本机源类型与传输请求不匹配")),
    }
    Ok(LocalManifest {
        root_name,
        entries,
        total_bytes,
    })
}

/// 按文件名排序递归记录目录项，确保归档顺序确定且总大小只累计普通文件内容。
fn collect_directory(
    directory: &Path,
    relative: Vec<String>,
    entries: &mut Vec<LocalManifestEntry>,
    total_bytes: &mut u64,
) -> AppResult<()> {
    let metadata = fs::symlink_metadata(directory)
        .map_err(|error| AppError::infrastructure_with_source("读取本机传输目录失败", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::unsupported("目录传输只接受普通目录与文件"));
    }
    entries.push(LocalManifestEntry {
        relative: relative.clone(),
        directory: true,
        bytes: 0,
    });
    let mut children = fs::read_dir(directory)
        .map_err(|error| AppError::infrastructure_with_source("读取本机传输目录失败", error))?
        .map(|entry| entry.map(|entry| (entry.file_name(), entry.path())))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| AppError::infrastructure_with_source("读取本机目录项失败", error))?;
    children.sort_by(|left, right| left.0.cmp(&right.0));
    for (name, path) in children {
        let name = name
            .to_str()
            .ok_or_else(|| AppError::unsupported("本机路径包含不可编码名称"))?
            .to_owned();
        validate_archive_name(&name)?;
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| AppError::infrastructure_with_source("读取本机目录项失败", error))?;
        let mut child_relative = relative.clone();
        child_relative.push(name);
        if metadata.file_type().is_symlink() {
            return Err(AppError::unsupported("目录传输暂不跟随或重建符号链接"));
        }
        if metadata.is_dir() {
            collect_directory(&path, child_relative, entries, total_bytes)?;
        } else if metadata.is_file() {
            let opened = fs::File::open(&path)
                .and_then(|file| file.metadata())
                .map_err(|error| {
                    AppError::infrastructure_with_source("打开本机传输文件失败", error)
                })?;
            if !opened.is_file() || opened.len() != metadata.len() {
                return Err(AppError::conflict("本机传输文件在读取前发生变化"));
            }
            *total_bytes = total_bytes.saturating_add(opened.len());
            entries.push(LocalManifestEntry {
                relative: child_relative,
                directory: false,
                bytes: opened.len(),
            });
        } else {
            return Err(AppError::unsupported("目录传输不支持设备、管道或 socket"));
        }
    }
    Ok(())
}

/// 归档项只允许一个 UTF-8 目录名，避免宿主路径语法进入容器 POSIX namespace。
pub(super) fn validate_archive_name(name: &str) -> AppResult<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', ':'])
        || name.chars().any(char::is_control)
    {
        return Err(AppError::invalid("传输对象名称无效"));
    }
    Ok(())
}

/// 对容器目标附加一个安全目录项，不在本机按 Windows 路径规则拼接远端路径。
pub(super) fn remote_child(
    path: &kerminal_domain::files::RemotePath,
    name: &str,
) -> AppResult<kerminal_domain::files::RemotePath> {
    path.join_name(name)
        .map_err(|_| AppError::invalid("容器传输路径项无效"))
}

/// 将本机目标父目录固定为 canonical 路径，避免相对路径和父级符号链接在传输期间改变含义。
pub(super) fn canonical_local_target(path: &Path) -> AppResult<PathBuf> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| AppError::invalid("本机目标必须有可编码的文件名"))?;
    validate_archive_name(name)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let canonical_parent = fs::canonicalize(parent)
        .map_err(|error| AppError::infrastructure_with_source("解析本机目标目录失败", error))?;
    let metadata = fs::symlink_metadata(&canonical_parent)
        .map_err(|error| AppError::infrastructure_with_source("读取本机目标目录失败", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::invalid("本机目标父路径必须是普通目录"));
    }
    Ok(canonical_parent.join(name))
}

/// 按同目录序号生成稳定的复制重命名候选，实际结果由调用方先检查存在性后固定。
pub(super) fn renamed_local_candidate(path: &Path, index: u32) -> AppResult<PathBuf> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| AppError::invalid("本机目标必须有可编码的文件名"))?;
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            (stem, format!(".{extension}"))
        }
        _ => (name, String::new()),
    };
    let parent = path
        .parent()
        .ok_or_else(|| AppError::invalid("本机目标缺少父目录"))?;
    Ok(parent.join(format!("{stem} ({index}){extension}")))
}

/// 对容器目标生成同目录 Rename 候选，扩展名规则与本机复制保持一致。
pub(super) fn renamed_remote_candidate(
    path: &kerminal_domain::files::RemotePath,
    index: u32,
) -> AppResult<kerminal_domain::files::RemotePath> {
    let name = path
        .as_str()
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| AppError::invalid("容器目标必须有文件名"))?;
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            (stem, format!(".{extension}"))
        }
        _ => (name, String::new()),
    };
    let parent = path
        .parent()
        .ok_or_else(|| AppError::invalid("容器目标缺少父目录"))?;
    remote_child(&parent, &format!("{stem} ({index}){extension}"))
}

/// 把传输归档名加入容器 staging 目录，调用方仍通过结构化 argv 传递整个路径。
pub(super) fn joined_remote_path(
    base: &kerminal_domain::files::RemotePath,
    archive_name: &str,
    relative: &[String],
) -> AppResult<kerminal_domain::files::RemotePath> {
    let mut path = remote_child(base, archive_name)?;
    for part in relative {
        path = remote_child(&path, part)?;
    }
    Ok(path)
}
