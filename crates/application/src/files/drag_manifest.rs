//! 为 Windows 虚拟文件拖出建立有界、可复核的目录清单。
//!
//! @author kongweiguang

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use kerminal_domain::files::{FileEndpoint, FileEntry, FileEntryKind, TransferKind};
use tokio_util::sync::CancellationToken;

use crate::{
    AppError, AppResult,
    files::{FileService, drag_out::DragOutSource},
};

// FILEDESCRIPTORW 的 260 个 UTF-16 单元包括结尾的 NUL。
const MAX_RELATIVE_PATH_UNITS: usize = 259;
const MAX_COMPONENT_UNITS: usize = 255;
const MAX_ENTRIES: usize = 10_000;
const MAX_DEPTH: usize = 64;

/// 一次拖动的完整快照；entries 的顺序即 Windows FileContents 索引。
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct DragManifest {
    pub entries: Vec<DragManifestEntry>,
}

/// 记录原始来源和相对目标路径，root_index 与冻结的选择及暂存路径一一对应。
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct DragManifestEntry {
    pub source: FileEndpoint,
    pub relative_path: String,
    pub kind: TransferKind,
    pub size: Option<u64>,
    pub root_index: usize,
}

impl DragManifest {
    /// 允许其他调用方构造清单；向系统发布前仍须经过来源和暂存校验。
    ///
    /// @author kongweiguang
    pub fn new(entries: Vec<DragManifestEntry>) -> Self {
        Self { entries }
    }
}

impl DragManifestEntry {
    /// 调用方保留源端点与根索引，避免仅凭目标字符串推断原始文件身份。
    ///
    /// @author kongweiguang
    pub fn new(
        source: FileEndpoint,
        relative_path: String,
        kind: TransferKind,
        size: Option<u64>,
        root_index: usize,
    ) -> Self {
        Self {
            source,
            relative_path,
            kind,
            size,
            root_index,
        }
    }
}

/// 从冻结的根选择递归构建清单；任何不可表达的条目都使整批失败，绝不静默省略。
/// 返回前没有读取文件正文，目标放置后才需要启动实际传输。
///
/// @author kongweiguang
pub async fn build_drag_manifest(
    files: FileService,
    cancellation: CancellationToken,
    roots: Vec<DragOutSource>,
) -> AppResult<DragManifest> {
    if roots.is_empty() {
        return Err(AppError::invalid("没有可拖出的文件或目录"));
    }
    if roots.len() > MAX_ENTRIES {
        return Err(AppError::invalid("拖出项目超过 10000 项上限"));
    }

    let mut entries = Vec::new();
    // Unicode 大小写映射可能不是一对一；任一种投影冲突就拒绝，避免目标同名覆盖。
    let mut uppercase_targets = HashSet::new();
    let mut lowercase_targets = HashSet::new();
    // 同一远端父目录只读取一次，并按端点索引，避免多选时重复请求和线性扫描。
    let mut parent_listings: HashMap<FileEndpoint, HashMap<FileEndpoint, FileEntry>> =
        HashMap::new();
    for (root_index, root) in roots.into_iter().enumerate() {
        ensure_active(&cancellation)?;
        validate_component(&root.name)?;
        let parent = parent_endpoint(&root.endpoint)?;
        if child_endpoint(&parent, &root.name)? != root.endpoint {
            return Err(AppError::conflict("拖出来源名称与路径不一致，请刷新后重试"));
        }
        if !parent_listings.contains_key(&parent) {
            let listing = read_verified_directory(&files, &cancellation, parent.clone()).await?;
            let mut index = HashMap::with_capacity(listing.len());
            for entry in listing {
                ensure_active(&cancellation)?;
                if index.insert(entry.endpoint.clone(), entry).is_some() {
                    return Err(AppError::conflict("拖出来源目录包含重复项，请刷新后重试"));
                }
            }
            parent_listings.insert(parent.clone(), index);
        }
        let listing = parent_listings
            .get(&parent)
            .ok_or_else(|| AppError::infrastructure("拖出来源目录快照不可用"))?;
        let root_entry = listing
            .get(&root.endpoint)
            .filter(|entry| entry.name == root.name)
            .cloned()
            .ok_or_else(|| AppError::not_found("拖出来源已不存在，请刷新后重试"))?;
        if entry_kind(&root_entry)? != root.kind {
            return Err(AppError::conflict("拖出来源类型已变化，请刷新后重试"));
        }

        let mut pending = vec![(root_entry, root.name, 1_usize)];
        while let Some((entry, relative_path, depth)) = pending.pop() {
            ensure_active(&cancellation)?;
            if depth > MAX_DEPTH {
                return Err(AppError::invalid("拖出目录超过 64 层上限"));
            }
            if relative_path.encode_utf16().count() > MAX_RELATIVE_PATH_UNITS {
                return Err(AppError::invalid("拖出相对路径超过 Windows 长度上限"));
            }
            if entries.len() >= MAX_ENTRIES {
                return Err(AppError::invalid("拖出项目超过 10000 项上限"));
            }
            if !uppercase_targets.insert(relative_path.to_uppercase())
                || !lowercase_targets.insert(relative_path.to_lowercase())
            {
                return Err(AppError::conflict("拖出项目在 Windows 目标中存在同名冲突"));
            }
            let kind = entry_kind(&entry)?;
            entries.push(DragManifestEntry::new(
                entry.endpoint.clone(),
                relative_path.clone(),
                kind,
                (kind == TransferKind::File).then_some(entry.size).flatten(),
                root_index,
            ));

            if kind == TransferKind::Directory {
                let mut children =
                    read_verified_directory(&files, &cancellation, entry.endpoint.clone()).await?;
                if entries
                    .len()
                    .saturating_add(pending.len())
                    .saturating_add(children.len())
                    > MAX_ENTRIES
                {
                    return Err(AppError::invalid("拖出项目超过 10000 项上限"));
                }
                for child in &children {
                    validate_component(&child.name)?;
                    if child_endpoint(&entry.endpoint, &child.name)? != child.endpoint {
                        return Err(AppError::conflict("拖出目录项路径不一致，请刷新后重试"));
                    }
                    // Reject unsupported children before sorting or descending into siblings.
                    entry_kind(child)?;
                }
                children.sort_by(|left, right| {
                    left.name
                        .to_uppercase()
                        .cmp(&right.name.to_uppercase())
                        .then_with(|| left.name.cmp(&right.name))
                });
                for child in children.into_iter().rev() {
                    let child_path = format!("{relative_path}/{}", child.name);
                    pending.push((child, child_path, depth + 1));
                }
            }
        }
    }
    ensure_active(&cancellation)?;
    Ok(DragManifest::new(entries))
}

/// 再次枚举已完成的本机暂存树，防止发布清单后来源增加、减少或大小改变造成目标漏文件。
/// 仅比较来源可提供的文件大小；未知大小不凭空猜测，正文仍由传输用例校验。
///
/// @author kongweiguang
pub async fn validate_prepared_drag_manifest(
    files: FileService,
    cancellation: CancellationToken,
    manifest: &DragManifest,
    paths: &[(PathBuf, bool)],
) -> AppResult<()> {
    ensure_active(&cancellation)?;
    if paths.is_empty() || paths.len() > MAX_ENTRIES || manifest.entries.is_empty() {
        return Err(AppError::invalid("拖出暂存清单无效"));
    }

    let mut roots: Vec<Option<&DragManifestEntry>> = vec![None; paths.len()];
    for entry in &manifest.entries {
        if entry.root_index >= paths.len() {
            return Err(AppError::invalid("拖出暂存根索引无效"));
        }
        if !entry.relative_path.contains('/') && roots[entry.root_index].replace(entry).is_some() {
            return Err(AppError::invalid("拖出暂存根项目重复"));
        }
    }
    let mut prepared_roots = Vec::with_capacity(paths.len());
    for ((path, is_directory), root) in paths.iter().zip(roots) {
        let root = root.ok_or_else(|| AppError::invalid("拖出暂存根项目缺失"))?;
        validate_component(&root.relative_path)?;
        if !path.is_absolute()
            || path.file_name().and_then(|name| name.to_str()) != Some(root.relative_path.as_str())
            || *is_directory != (root.kind == TransferKind::Directory)
        {
            return Err(AppError::conflict("拖出暂存目标与原清单不一致"));
        }
        prepared_roots.push(DragOutSource {
            endpoint: FileEndpoint::Local { path: path.clone() },
            name: root.relative_path.clone(),
            kind: root.kind,
            external_target_lease: None,
        });
    }
    let prepared = build_drag_manifest(files, cancellation, prepared_roots).await?;
    if manifest.entries.len() != prepared.entries.len()
        || manifest
            .entries
            .iter()
            .zip(&prepared.entries)
            .any(|(expected, actual)| {
                expected.root_index != actual.root_index
                    || expected.relative_path != actual.relative_path
                    || expected.kind != actual.kind
                    || (expected.kind == TransferKind::File
                        && expected.size.is_some()
                        && expected.size != actual.size)
            })
    {
        return Err(AppError::conflict("拖出来源内容已变化，请重新拖出"));
    }
    Ok(())
}

/// 调用 provider 前后都检查取消，迟到的成功回执不能继续扩展清单。
///
/// @author kongweiguang
async fn read_verified_directory(
    files: &FileService,
    cancellation: &CancellationToken,
    endpoint: FileEndpoint,
) -> AppResult<Vec<FileEntry>> {
    ensure_active(cancellation)?;
    let listing = files
        .list_directory(cancellation.clone(), endpoint.clone())
        .await?;
    ensure_active(cancellation)?;
    if listing.directory != endpoint {
        return Err(AppError::conflict("拖出目录响应与来源不一致，请刷新后重试"));
    }
    Ok(listing.entries)
}

/// 仅普通文件和目录可映射到 Windows 文件描述符；链接不能悄悄解引用。
///
/// @author kongweiguang
fn entry_kind(entry: &FileEntry) -> AppResult<TransferKind> {
    match entry.kind {
        FileEntryKind::File => Ok(TransferKind::File),
        FileEntryKind::Directory => Ok(TransferKind::Directory),
        FileEntryKind::Symlink | FileEntryKind::Other => Err(AppError::unsupported(
            "拖出不支持符号链接或特殊文件，请调整选择后重试",
        )),
    }
}

/// 按 Windows 文件命名约束检查单层名称，避免设备名、数据流或路径穿越改变目标语义。
///
/// @author kongweiguang
fn validate_component(name: &str) -> AppResult<()> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.ends_with([' ', '.'])
        || name.encode_utf16().count() > MAX_COMPONENT_UNITS
        || name.chars().any(|ch| {
            ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        })
    {
        return Err(AppError::invalid(
            "文件名不能安全保存到 Windows，请重命名后重试",
        ));
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    let reserved = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    ) || stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
    if reserved {
        return Err(AppError::invalid(
            "文件名是 Windows 保留设备名，请重命名后重试",
        ));
    }
    Ok(())
}

/// 来源父目录必须保留同一主机、容器和路径语义，不能依赖宿主机解析远端路径。
///
/// @author kongweiguang
fn parent_endpoint(endpoint: &FileEndpoint) -> AppResult<FileEndpoint> {
    match endpoint {
        FileEndpoint::Local { path } => {
            if !path.is_absolute() {
                return Err(AppError::invalid("拖出本机来源必须使用绝对路径"));
            }
            Ok(FileEndpoint::Local {
                path: path
                    .parent()
                    .ok_or_else(|| AppError::invalid("不能拖出文件系统根目录"))?
                    .to_path_buf(),
            })
        }
        FileEndpoint::Remote { host_id, path } => Ok(FileEndpoint::Remote {
            host_id: host_id.clone(),
            path: path
                .parent()
                .ok_or_else(|| AppError::invalid("不能拖出文件系统根目录"))?,
        }),
        FileEndpoint::Container {
            host_id,
            runtime,
            container_id,
            path,
        } => Ok(FileEndpoint::Container {
            host_id: host_id.clone(),
            runtime: *runtime,
            container_id: container_id.clone(),
            path: path
                .parent()
                .ok_or_else(|| AppError::invalid("不能拖出文件系统根目录"))?,
        }),
    }
}

/// 仅构造直接子项并与 provider 返回值逐项比较，阻止同一树下跨层或跨目标替换。
///
/// @author kongweiguang
fn child_endpoint(parent: &FileEndpoint, name: &str) -> AppResult<FileEndpoint> {
    validate_component(name)?;
    match parent {
        FileEndpoint::Local { path } => Ok(FileEndpoint::Local {
            path: path.join(name),
        }),
        FileEndpoint::Remote { host_id, path } => Ok(FileEndpoint::Remote {
            host_id: host_id.clone(),
            path: path
                .join_name(name)
                .map_err(|_| AppError::invalid("拖出目录项名称无效"))?,
        }),
        FileEndpoint::Container {
            host_id,
            runtime,
            container_id,
            path,
        } => Ok(FileEndpoint::Container {
            host_id: host_id.clone(),
            runtime: *runtime,
            container_id: container_id.clone(),
            path: path
                .join_name(name)
                .map_err(|_| AppError::invalid("拖出目录项名称无效"))?,
        }),
    }
}

/// 边界统一使用业务取消错误，避免取消被误报成来源丢失。
///
/// @author kongweiguang
fn ensure_active(cancellation: &CancellationToken) -> AppResult<()> {
    if cancellation.is_cancelled() {
        Err(AppError::cancelled("文件拖出已取消"))
    } else {
        Ok(())
    }
}
