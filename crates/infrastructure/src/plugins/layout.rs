//! 外置插件的单目录布局、目录树指纹与启动事务 journal。
//!
//! 插件状态和目录必须一起前进或还原，因此所有改名、校验和清理都先写入
//! `plugins/.migration/<operation>/journal.json`，重启后只能依据持久证据决定
//! 前滚还是保留现场；没有证据的目录一律不删除。

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use kerminal_domain::plugins::{PluginFingerprint, PluginId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::storage::{AtomicCommitError, CommitMode, commit_staged_file};

pub const PLUGINS_DIRECTORY: &str = "plugins";
pub const DATA_DIRECTORY: &str = "data";
pub const LOGS_DIRECTORY: &str = "logs";
/// 目录客户端把下载缓存放在插件根下；它不是插件材料，但迁移预检必须与读取端
/// 共用同一个目录名，否则产品自建缓存会被误判成未知外部材料。
pub const CATALOG_CACHE_DIRECTORY: &str = "catalog-cache";
pub const STAGING_DIRECTORY: &str = ".staging";
pub const INCOMING_DIRECTORY: &str = "_incoming";
pub const BACKUP_DIRECTORY: &str = ".backup";
pub const MIGRATION_DIRECTORY: &str = ".migration";
pub const STATE_FILE: &str = "state.json";
pub const JOURNAL_FILE: &str = "journal.json";
pub const JOURNAL_SCHEMA: u32 = 1;
/// 摘要算法一旦发布就不能改写语义；升级规则必须换前缀，让旧记录明确不匹配。
pub const FINGERPRINT_ALGORITHM: &str = "v1";

const MAX_JOURNAL_BYTES: u64 = 1024 * 1024;
const MAX_FINGERPRINT_BYTES: u64 = 64 * 1024 * 1024;

/// 布局错误区分“确定未完成”和“结果未知”；后者必须保留 journal 与现场。
#[derive(Debug)]
pub enum LayoutError {
    Io(io::Error),
    Unsafe(&'static str),
    Missing(&'static str),
    Conflict(&'static str),
    Fingerprint(io::Error),
    RecoveryRequired {
        journal_path: PathBuf,
        reason: &'static str,
    },
    OutcomeUnknown {
        operation_id: String,
        source: io::Error,
    },
}

impl std::fmt::Display for LayoutError {
    /// 不向界面暴露本机绝对路径，只保留固定类别。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(_) => formatter.write_str("插件布局读写失败"),
            Self::Unsafe(reason) | Self::Conflict(reason) => formatter.write_str(reason),
            Self::Missing(reason) => formatter.write_str(reason),
            Self::Fingerprint(_) => formatter.write_str("插件目录内容无法安全摘要"),
            Self::RecoveryRequired { .. } => formatter.write_str("插件启动事务需要人工恢复"),
            Self::OutcomeUnknown { .. } => formatter.write_str("插件目录变更结果未知"),
        }
    }
}

impl std::error::Error for LayoutError {
    /// 保留底层错误供内部诊断，不通过 Display 回显路径。
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) | Self::Fingerprint(error) => Some(error),
            Self::OutcomeUnknown { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for LayoutError {
    /// 普通 IO 失败默认视为未完成；调用方在改名/删除边界另行检查未知结果。
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// 插件根、包目录和私有数据保持同级；插件身份只占一个路径段。
pub fn plugin_root(root: &Path) -> PathBuf {
    root.join(PLUGINS_DIRECTORY)
}

/// `plugins/<id>` 是唯一的活动版本目录，更新时整目录替换。
pub fn package_directory(root: &Path, id: &PluginId) -> PathBuf {
    plugin_root(root).join(id.storage_segment())
}

/// 数据目录独立于包目录，更新和卸载默认都不会替换用户数据。
pub fn data_directory(root: &Path, id: &PluginId) -> PathBuf {
    plugin_root(root)
        .join(DATA_DIRECTORY)
        .join(id.storage_segment())
}

/// 日志目录为所有插件共享，不由单个插件身份拼接。
pub fn log_directory(root: &Path) -> PathBuf {
    plugin_root(root).join(LOGS_DIRECTORY)
}

/// 安装预热目录使用独立 UUID，解析出插件 ID 后才移动到按 ID 分组的暂存槽。
pub fn incoming_directory(root: &Path, operation_id: &str) -> Result<PathBuf, LayoutError> {
    let operation_id = validate_uuid_segment(operation_id)?;
    Ok(plugin_root(root)
        .join(STAGING_DIRECTORY)
        .join(INCOMING_DIRECTORY)
        .join(operation_id))
}

/// 启动取得写锁后回收解包前崩溃留下的 `_incoming` 材料。
///
/// 这里只清理普通文件和安全目录；链接、重解析点或未知类型原样保留并报告失败。
/// 按 id 分组的 `.staging/<id>/<uuid>` 槽可能仍被 pending 引用，绝不在本函数触碰。
pub fn cleanup_incoming(root: &Path) -> Result<bool, LayoutError> {
    let directory = plugin_root(root)
        .join(STAGING_DIRECTORY)
        .join(INCOMING_DIRECTORY);
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(LayoutError::Io(error)),
    };
    if !metadata.is_dir() || !safe_metadata(&metadata) {
        return Ok(false);
    }
    let mut complete = true;
    for entry in fs::read_dir(&directory)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        if metadata.is_dir() && safe_metadata(&metadata) {
            if remove_tree_verified(&path).is_err() {
                complete = false;
            }
        } else if metadata.is_file() && safe_metadata(&metadata) {
            if fs::remove_file(&path).is_err() {
                complete = false;
            }
        } else {
            complete = false;
        }
    }
    Ok(complete)
}

/// 同 ID 多次安装必须保留各自的 UUID 证据，固定路径会让未知结果互相覆盖。
pub fn staging_directory(
    root: &Path,
    id: &PluginId,
    operation_id: &str,
) -> Result<PathBuf, LayoutError> {
    let operation_id = validate_uuid_segment(operation_id)?;
    Ok(plugin_root(root)
        .join(STAGING_DIRECTORY)
        .join(id.storage_segment())
        .join(operation_id))
}

/// 更新换下的旧目录按操作 UUID 保存，提交后才能证明它不再被引用。
pub fn backup_directory(
    root: &Path,
    id: &PluginId,
    operation_id: &str,
) -> Result<PathBuf, LayoutError> {
    let operation_id = validate_uuid_segment(operation_id)?;
    Ok(plugin_root(root)
        .join(BACKUP_DIRECTORY)
        .join(id.storage_segment())
        .join(operation_id))
}

/// 每个事务独占一个迁移目录，journal 与随后清理的材料不会互相覆盖。
pub fn migration_directory(root: &Path, operation_id: &str) -> Result<PathBuf, LayoutError> {
    let operation_id = validate_uuid_segment(operation_id)?;
    Ok(plugin_root(root)
        .join(MIGRATION_DIRECTORY)
        .join(operation_id))
}

/// journal 路径始终由操作 UUID 推导，不接受 pending 中的任意路径。
pub fn journal_path(root: &Path, operation_id: &str) -> Result<PathBuf, LayoutError> {
    Ok(migration_directory(root, operation_id)?.join(JOURNAL_FILE))
}

/// UUID 同时用作目录名；先按解析器验证再拼路径，拒绝分隔符和点段。
fn validate_uuid_segment(value: &str) -> Result<&str, LayoutError> {
    if uuid::Uuid::parse_str(value).is_err() {
        return Err(LayoutError::Conflict("插件操作标识无效"));
    }
    Ok(value)
}

/// Windows junction 也带重解析点属性，不能只看 `is_symlink`。
pub fn safe_metadata(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        !metadata.file_type().is_symlink() && metadata.file_attributes() & 0x400 == 0
    }
    #[cfg(not(windows))]
    {
        !metadata.file_type().is_symlink()
    }
}

/// 逐级创建目录并复核每级身份；外部输入只允许出现在已通过身份校验的段中。
pub fn ensure_directory(root: &Path, segments: &[&str]) -> Result<PathBuf, LayoutError> {
    let mut path = root.to_path_buf();
    for segment in segments {
        path.push(segment);
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(LayoutError::Io(error)),
        }
        verify_directory(&path)?;
    }
    Ok(path)
}

/// 只接受真实目录；缺失与不安全路径必须由调用方明确区分处理。
pub fn verify_directory(path: &Path) -> Result<(), LayoutError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || !safe_metadata(&metadata) {
        return Err(LayoutError::Unsafe("插件目录路径不安全"));
    }
    Ok(())
}

/// 目录树摘要：排序后的 UTF-8 相对路径、大小和文件 SHA-256 参与固定算法哈希。
/// 链接、重解析点、硬链接与未知类型都会失败，避免摘要把危险内容当成普通包。
pub fn fingerprint_tree(root: &Path) -> Result<PluginFingerprint, LayoutError> {
    verify_directory(root)?;
    let mut files = BTreeMap::new();
    let mut pending = vec![(root.to_path_buf(), String::new())];
    while let Some((directory, prefix)) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if !safe_metadata(&metadata) {
                return Err(LayoutError::Unsafe("插件包含链接或重解析点"));
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| LayoutError::Fingerprint(io::Error::other("插件路径不是 UTF-8")))?;
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            if metadata.is_dir() {
                pending.push((entry.path(), relative));
            } else if metadata.is_file() {
                let digest = fingerprint_file(&entry.path(), &metadata)?;
                if files.insert(relative, (metadata.len(), digest)).is_some() {
                    return Err(LayoutError::Unsafe("插件包路径重复"));
                }
                if files.len() > kerminal_plugin_protocol::MAX_PACKAGE_ENTRIES {
                    return Err(LayoutError::Unsafe("插件包文件数量超过限额"));
                }
            } else {
                return Err(LayoutError::Unsafe("插件包含未知文件类型"));
            }
        }
    }
    let mut hasher = Sha256::new();
    hasher.update(FINGERPRINT_ALGORITHM.as_bytes());
    hasher.update((files.len() as u64).to_le_bytes());
    for (relative, (size, digest)) in files {
        hasher.update((relative.len() as u64).to_le_bytes());
        hasher.update(relative.as_bytes());
        hasher.update(size.to_le_bytes());
        hasher.update(digest);
    }
    let value = format!("{FINGERPRINT_ALGORITHM}:{:x}", hasher.finalize());
    PluginFingerprint::parse(value)
        .map_err(|_| LayoutError::Fingerprint(io::Error::other("插件摘要格式无效")))
}

/// 摘要读取打开后复核句柄类型与链接数，拒绝在检查与读取之间被替换的文件。
fn fingerprint_file(path: &Path, metadata: &fs::Metadata) -> Result<[u8; 32], LayoutError> {
    ensure_single_link(path, metadata)?;
    let file = open_read_no_follow(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || !safe_metadata(&opened) || opened.len() != metadata.len() {
        return Err(LayoutError::Unsafe("插件文件在摘要期间发生变化"));
    }
    ensure_single_link(path, &opened)?;
    let mut file = file;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > MAX_FINGERPRINT_BYTES {
            return Err(LayoutError::Unsafe("插件包文件超过摘要限额"));
        }
        hasher.update(&buffer[..read]);
    }
    if total != metadata.len() {
        return Err(LayoutError::Unsafe("插件文件在摘要期间发生变化"));
    }
    Ok(hasher.finalize().into())
}

/// 打开最终路径时禁止跟随链接，避免检查元数据后被替换而读取链接目标。
fn open_read_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT 令句柄指向重解析点本身，再检查句柄元数据。
        options.custom_flags(0x0020_0000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

/// Unix 从实际句柄核对链接数；Windows 标准库不暴露 nlink，复用 Win32 文件信息。
#[cfg(unix)]
fn ensure_single_link(_path: &Path, metadata: &fs::Metadata) -> Result<(), LayoutError> {
    use std::os::unix::fs::MetadataExt;
    if metadata.nlink() != 1 {
        return Err(LayoutError::Unsafe("插件包含硬链接"));
    }
    Ok(())
}

/// Windows 摘要期间重新打开文件并检查句柄链接数。
#[cfg(windows)]
fn ensure_single_link(path: &Path, _metadata: &fs::Metadata) -> Result<(), LayoutError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let file = open_read_no_follow(path)?;
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(LayoutError::Io(io::Error::last_os_error()));
    }
    if information.nNumberOfLinks != 1 {
        return Err(LayoutError::Unsafe("插件包含硬链接"));
    }
    Ok(())
}

/// 无法核查硬链接的平台拒绝摘要，而不是把未知风险当成已验证。
#[cfg(not(any(unix, windows)))]
fn ensure_single_link(_path: &Path, _metadata: &fs::Metadata) -> Result<(), LayoutError> {
    Err(LayoutError::Unsafe("当前平台无法核查插件硬链接"))
}

/// 目录移动只用 no-replace 改名；已知或未知结果分开报告，调用方决定前滚或还原。
pub fn move_directory(
    source: &Path,
    destination: &Path,
    operation_id: &str,
) -> Result<(), LayoutError> {
    verify_directory(source)?;
    ensure_parent_directory(destination)?;
    match fs::symlink_metadata(destination) {
        Ok(_) => return Err(LayoutError::Conflict("插件目录目标已存在")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(LayoutError::Io(error)),
    }
    commit_staged_file(source, destination, CommitMode::MoveNoReplace).map_err(
        |error| match error {
            AtomicCommitError::NotCommitted(source) => LayoutError::Io(source),
            AtomicCommitError::CommitUnknown(source) => LayoutError::OutcomeUnknown {
                operation_id: operation_id.to_owned(),
                source,
            },
        },
    )
}

/// 目标父目录（`.backup/<id>` 等）可能尚不存在；逐级创建并复核，禁止借链接越出数据根。
fn ensure_parent_directory(destination: &Path) -> Result<(), LayoutError> {
    let parent = destination
        .parent()
        .ok_or(LayoutError::Unsafe("插件移动目标路径无效"))?;
    let mut missing = Vec::new();
    let mut current = parent;
    loop {
        match fs::symlink_metadata(current) {
            Ok(metadata) => {
                if !metadata.is_dir() || !safe_metadata(&metadata) {
                    return Err(LayoutError::Unsafe("插件移动目标父目录不安全"));
                }
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = current
                    .file_name()
                    .ok_or(LayoutError::Unsafe("插件移动目标父目录无效"))?;
                missing.push(name.to_owned());
                current = current
                    .parent()
                    .ok_or(LayoutError::Unsafe("插件移动目标父目录无效"))?;
            }
            Err(error) => return Err(LayoutError::Io(error)),
        }
    }
    let mut path = current.to_path_buf();
    for name in missing.into_iter().rev() {
        path.push(name);
        fs::create_dir(&path)?;
        verify_directory(&path)?;
    }
    Ok(())
}

/// 删除结果未知时先回读；目录仍存在且无法删除时返回“未完成”，而不是猜测已删除。
pub fn remove_tree_verified(path: &Path) -> Result<(), LayoutError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && safe_metadata(&metadata) => {}
        Ok(_) => return Err(LayoutError::Unsafe("插件清理路径不是安全目录")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(LayoutError::Io(error)),
    }
    validate_removable_tree(path)?;
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) => match fs::symlink_metadata(path) {
            Err(not_found) if not_found.kind() == io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(LayoutError::Io(error)),
            Err(probe) => Err(LayoutError::OutcomeUnknown {
                operation_id: path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_owned(),
                source: probe,
            }),
        },
    }
}

/// 删除前有界清点整棵目录，拒绝链接、重解析点和未知类型。
fn validate_removable_tree(root: &Path) -> Result<(), LayoutError> {
    let mut directories = vec![root.to_path_buf()];
    let mut count = 0usize;
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            count += 1;
            if count > kerminal_plugin_protocol::MAX_PACKAGE_ENTRIES {
                return Err(LayoutError::Unsafe("插件清理目录条目超过限额"));
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if !safe_metadata(&metadata) {
                return Err(LayoutError::Unsafe("插件清理路径含链接或重解析点"));
            }
            if metadata.is_dir() {
                directories.push(entry.path());
            } else if !metadata.is_file() {
                return Err(LayoutError::Unsafe("插件清理路径含未知文件类型"));
            }
        }
    }
    Ok(())
}

/// 文件存在性只用 no-follow 元数据判断，不打开插件内容。
pub fn exists(path: &Path) -> Result<bool, LayoutError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(LayoutError::Io(error)),
    }
}

/// 事务阶段只允许单向前进；恢复代码据持久阶段决定动作。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutPhase {
    Prepared,
    OldMoved,
    NewInstalled,
    StateCommitted,
    Committed,
    Cleaned,
}

/// 操作类型决定哪些目录身份是必需的。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutAction {
    Install,
    Update,
    Uninstall,
}

/// 证据同时记录相对路径与内容摘要；恢复前必须重新计算并核对。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PathEvidence {
    pub relative: String,
    pub fingerprint: PluginFingerprint,
}

/// 持久 journal 是重启后的唯一依据，任何字段缺失都不能被默认值掩盖。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginLayoutJournal {
    pub schema: u32,
    pub operation_id: String,
    pub phase: LayoutPhase,
    pub id: PluginId,
    pub action: LayoutAction,
    /// 目标目录（`plugins/<id>`）变更前的内容；安装时为 None。
    pub before: Option<PathEvidence>,
    /// 目标目录变更后的内容；卸载时为 None。
    pub after: Option<PathEvidence>,
    pub staging: Option<PathEvidence>,
    pub backup: Option<PathEvidence>,
}

impl PluginLayoutJournal {
    /// 安装只有暂存目录到活动目录的移动；提交前目标必须不存在。
    pub fn install(
        root: &Path,
        id: &PluginId,
        operation_id: &str,
        staging_fingerprint: PluginFingerprint,
        after: PluginFingerprint,
    ) -> Result<Self, LayoutError> {
        let staging = staging_directory(root, id, operation_id)?;
        Ok(Self {
            schema: JOURNAL_SCHEMA,
            operation_id: operation_id.to_owned(),
            phase: LayoutPhase::Prepared,
            id: id.clone(),
            action: LayoutAction::Install,
            before: None,
            after: Some(PathEvidence {
                relative: relative_path(root, &package_directory(root, id))?,
                fingerprint: after,
            }),
            staging: Some(PathEvidence {
                relative: relative_path(root, &staging)?,
                fingerprint: staging_fingerprint,
            }),
            backup: None,
        })
    }

    /// 更新同时记录旧目录、暂存目录和备份身份，恢复时可逐项核对。
    pub fn update(
        root: &Path,
        id: &PluginId,
        operation_id: &str,
        before: PluginFingerprint,
        staging_fingerprint: PluginFingerprint,
        after: PluginFingerprint,
    ) -> Result<Self, LayoutError> {
        let staging = staging_directory(root, id, operation_id)?;
        let backup = backup_directory(root, id, operation_id)?;
        Ok(Self {
            schema: JOURNAL_SCHEMA,
            operation_id: operation_id.to_owned(),
            phase: LayoutPhase::Prepared,
            id: id.clone(),
            action: LayoutAction::Update,
            before: Some(PathEvidence {
                relative: relative_path(root, &package_directory(root, id))?,
                fingerprint: before.clone(),
            }),
            after: Some(PathEvidence {
                relative: relative_path(root, &package_directory(root, id))?,
                fingerprint: after,
            }),
            staging: Some(PathEvidence {
                relative: relative_path(root, &staging)?,
                fingerprint: staging_fingerprint,
            }),
            backup: Some(PathEvidence {
                relative: relative_path(root, &backup)?,
                fingerprint: before,
            }),
        })
    }

    /// 卸载先把活动目录移入备份再提交状态，因此只记录旧内容。
    pub fn uninstall(
        root: &Path,
        id: &PluginId,
        operation_id: &str,
        before: PluginFingerprint,
    ) -> Result<Self, LayoutError> {
        let backup = backup_directory(root, id, operation_id)?;
        Ok(Self {
            schema: JOURNAL_SCHEMA,
            operation_id: operation_id.to_owned(),
            phase: LayoutPhase::Prepared,
            id: id.clone(),
            action: LayoutAction::Uninstall,
            before: Some(PathEvidence {
                relative: relative_path(root, &package_directory(root, id))?,
                fingerprint: before.clone(),
            }),
            after: None,
            staging: None,
            backup: Some(PathEvidence {
                relative: relative_path(root, &backup)?,
                fingerprint: before,
            }),
        })
    }

    /// 解析 journal 中的相对路径，只接受普通段并强制留在插件根内。
    pub fn resolve(&self, root: &Path, relative: &str) -> Result<PathBuf, LayoutError> {
        resolve_relative(root, relative)
    }
}

/// 把受控路径转换为 `/` 分隔的相对字符串，便于跨平台恢复与人工检查。
fn relative_path(root: &Path, path: &Path) -> Result<String, LayoutError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| LayoutError::Unsafe("插件路径不在数据根内"))?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => parts.push(
                value
                    .to_str()
                    .ok_or(LayoutError::Unsafe("插件路径不是 UTF-8"))?,
            ),
            _ => return Err(LayoutError::Unsafe("插件路径包含非普通段")),
        }
    }
    Ok(parts.join("/"))
}

/// 不允许绝对路径或 `..`，恢复动作只能触及数据根内的受控位置。
pub fn resolve_relative(root: &Path, relative: &str) -> Result<PathBuf, LayoutError> {
    let mut path = root.to_path_buf();
    for segment in relative.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." || segment.contains('\\') {
            return Err(LayoutError::Unsafe("插件 journal 路径不安全"));
        }
        path.push(segment);
    }
    Ok(path)
}

/// journal 先写同目录临时文件并同步，再用原子替换发布，避免半写状态被恢复。
pub fn write_journal(root: &Path, journal: &PluginLayoutJournal) -> Result<PathBuf, LayoutError> {
    let directory = migration_directory(root, &journal.operation_id)?;
    ensure_directory(root, &[PLUGINS_DIRECTORY, MIGRATION_DIRECTORY])?;
    let _ = ensure_directory(
        root,
        &[
            PLUGINS_DIRECTORY,
            MIGRATION_DIRECTORY,
            &journal.operation_id,
        ],
    );
    let target = directory.join(JOURNAL_FILE);
    let bytes = serde_json::to_vec_pretty(journal)
        .map_err(|_| LayoutError::Io(io::Error::other("插件 journal 序列化失败")))?;
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(LayoutError::Conflict("插件 journal 超过限额"));
    }
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    let temporary = temporary.into_temp_path();
    let mode = if exists(&target)? {
        CommitMode::ReplaceExisting
    } else {
        CommitMode::CreateNew
    };
    commit_staged_file(&temporary, &target, mode).map_err(|error| match error {
        AtomicCommitError::NotCommitted(source) => LayoutError::Io(source),
        AtomicCommitError::CommitUnknown(source) => LayoutError::OutcomeUnknown {
            operation_id: journal.operation_id.clone(),
            source,
        },
    })?;
    Ok(target)
}

/// 读取并校验 journal；未知阶段或伪造路径一律作为恢复材料保留并阻断启动。
pub fn read_journal(path: &Path) -> Result<PluginLayoutJournal, LayoutError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || !safe_metadata(&metadata) || metadata.len() > MAX_JOURNAL_BYTES {
        return Err(LayoutError::Unsafe("插件 journal 不是安全普通文件"));
    }
    let file = open_read_no_follow(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || !safe_metadata(&opened) || opened.len() > MAX_JOURNAL_BYTES {
        return Err(LayoutError::Unsafe("插件 journal 不是安全普通文件"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_JOURNAL_BYTES + 1).read_to_end(&mut bytes)?;
    let journal: PluginLayoutJournal =
        serde_json::from_slice(&bytes).map_err(|_| LayoutError::Unsafe("插件 journal 格式无效"))?;
    if journal.schema != JOURNAL_SCHEMA
        || journal.operation_id.is_empty()
        || journal.operation_id != journal.operation_id.replace(['/', '\\'], "")
        || uuid::Uuid::parse_str(&journal.operation_id).is_err()
    {
        return Err(LayoutError::Unsafe("插件 journal 身份无效"));
    }
    Ok(journal)
}

/// 扫描全部事务目录；出现无 journal 的未知目录时报告恢复错误而不猜测。
pub fn scan_journals(root: &Path) -> Result<Vec<(PathBuf, PluginLayoutJournal)>, LayoutError> {
    let directory = plugin_root(root).join(MIGRATION_DIRECTORY);
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(LayoutError::Io(error)),
    };
    if !metadata.is_dir() || !safe_metadata(&metadata) {
        return Err(LayoutError::Unsafe("插件迁移目录不安全"));
    }
    let mut journals = Vec::new();
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or(LayoutError::Unsafe("插件迁移目录名无效"))?;
        if uuid::Uuid::parse_str(name).is_err() {
            return Err(LayoutError::RecoveryRequired {
                journal_path: entry.path(),
                reason: "插件迁移目录含未知材料",
            });
        }
        let path = entry.path().join(JOURNAL_FILE);
        journals.push((path.clone(), read_journal(&path)?));
    }
    journals.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(journals)
}

/// 计算目录摘要并返回类型化结果；路径不安全时不回退为“视为不同”。
pub fn fingerprint(path: &Path) -> Result<PluginFingerprint, LayoutError> {
    fingerprint_tree(path)
}

/// 活动目录的身份摘要只要不匹配就禁用插件，不能继续加载旧会话内容。
pub fn matches_fingerprint(path: &Path, expected: &PluginFingerprint) -> Result<bool, LayoutError> {
    if !exists(path)? {
        return Ok(false);
    }
    Ok(&fingerprint_tree(path)? == expected)
}

/// 重新计算证据位置的内容；缺失或摘要变化都意味着不能执行下一步移动。
pub fn verify_evidence(root: &Path, evidence: &PathEvidence) -> Result<PathBuf, LayoutError> {
    let path = resolve_relative(root, &evidence.relative)?;
    if !exists(&path)? {
        return Err(LayoutError::Missing("插件事务引用的目录不存在"));
    }
    if fingerprint_tree(&path)? != evidence.fingerprint {
        return Err(LayoutError::Conflict("插件事务目录内容与 journal 不一致"));
    }
    Ok(path)
}

/// 读取指定操作已有的 journal；缺失不算错误，存在则必须仍可安全解析。
pub fn read_operation_journal(
    root: &Path,
    operation_id: &str,
) -> Result<Option<(PathBuf, PluginLayoutJournal)>, LayoutError> {
    let path = journal_path(root, operation_id)?;
    match fs::symlink_metadata(&path) {
        Ok(_) => read_journal(&path).map(|journal| Some((path, journal))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(LayoutError::Io(error)),
    }
}

/// 启动恢复依据持久阶段前滚或保留现场；返回清理失败供启动快照上报。
///
/// 只有 journal 明确证明状态已提交时才删除 staging/backup；阶段、指纹或
/// state 记录任一不符都返回恢复错误，绝不猜测“未提交”并直接回退。
pub fn recover_journals(
    root: &Path,
    state: &kerminal_domain::plugins::PluginStoreState,
) -> Result<bool, LayoutError> {
    let mut cleanup_failed = false;
    for (path, mut journal) in scan_journals(root)? {
        if !journal_matches_layout(root, &journal)? {
            return Err(LayoutError::RecoveryRequired {
                journal_path: path,
                reason: "插件 journal 路径与操作身份不一致",
            });
        }
        match journal.phase {
            LayoutPhase::Cleaned => {
                if remove_tree_verified(path.parent().unwrap_or(&path)).is_err() {
                    cleanup_failed = true;
                }
            }
            LayoutPhase::Committed => {
                cleanup_failed |= !finish_committed(root, &mut journal)?;
            }
            LayoutPhase::StateCommitted => {
                if !state_matches_operation(state, &journal) {
                    return Err(LayoutError::RecoveryRequired {
                        journal_path: path,
                        reason: "插件状态与已提交 journal 不一致",
                    });
                }
                journal.phase = LayoutPhase::Committed;
                write_journal(root, &journal)?;
                cleanup_failed |= !finish_committed(root, &mut journal)?;
            }
            LayoutPhase::NewInstalled => {
                if pending_references_operation(state, &journal) {
                    // 状态尚未提交，保留 journal 让本次启动的 pending 应用复用当前阶段。
                    continue;
                }
                if !state_matches_operation(state, &journal) {
                    return Err(LayoutError::RecoveryRequired {
                        journal_path: path,
                        reason: "插件目录已换但状态没有对应记录",
                    });
                }
                journal.phase = LayoutPhase::StateCommitted;
                write_journal(root, &journal)?;
                journal.phase = LayoutPhase::Committed;
                write_journal(root, &journal)?;
                cleanup_failed |= !finish_committed(root, &mut journal)?;
            }
            LayoutPhase::OldMoved => {
                match journal.action {
                    LayoutAction::Update => {
                        let final_path = package_directory(root, &journal.id);
                        if exists(&final_path)? {
                            if !evidence_matches(root, journal.after.as_ref())? {
                                return Err(LayoutError::RecoveryRequired {
                                    journal_path: path,
                                    reason: "插件更新目录内容与 journal 不一致",
                                });
                            }
                        } else {
                            let staging =
                                journal
                                    .staging
                                    .as_ref()
                                    .ok_or(LayoutError::RecoveryRequired {
                                        journal_path: path.clone(),
                                        reason: "插件更新缺少暂存证据",
                                    })?;
                            let staging_path = verify_evidence(root, staging)
                                .map_err(|error| insufficient_evidence(&path, error))?;
                            verify_evidence(
                                root,
                                journal
                                    .backup
                                    .as_ref()
                                    .ok_or(LayoutError::RecoveryRequired {
                                        journal_path: path.clone(),
                                        reason: "插件更新缺少备份证据",
                                    })?,
                            )
                            .map_err(|error| insufficient_evidence(&path, error))?;
                            move_directory(&staging_path, &final_path, &journal.operation_id)?;
                        }
                        journal.phase = LayoutPhase::NewInstalled;
                        write_journal(root, &journal)?;
                    }
                    LayoutAction::Uninstall => {
                        verify_evidence(
                            root,
                            journal
                                .backup
                                .as_ref()
                                .ok_or(LayoutError::RecoveryRequired {
                                    journal_path: path.clone(),
                                    reason: "插件卸载缺少备份证据",
                                })?,
                        )
                        .map_err(|error| insufficient_evidence(&path, error))?;
                        journal.phase = LayoutPhase::NewInstalled;
                        write_journal(root, &journal)?;
                    }
                    LayoutAction::Install => {
                        return Err(LayoutError::RecoveryRequired {
                            journal_path: path,
                            reason: "插件安装事务出现旧目录阶段",
                        });
                    }
                }
                if pending_references_operation(state, &journal) {
                    continue;
                }
                if !state_matches_operation(state, &journal) {
                    return Err(LayoutError::RecoveryRequired {
                        journal_path: path,
                        reason: "插件目录已换但状态没有对应记录",
                    });
                }
                journal.phase = LayoutPhase::NewInstalled;
                write_journal(root, &journal)?;
                journal.phase = LayoutPhase::StateCommitted;
                write_journal(root, &journal)?;
                journal.phase = LayoutPhase::Committed;
                write_journal(root, &journal)?;
                cleanup_failed |= !finish_committed(root, &mut journal)?;
            }
            LayoutPhase::Prepared => {
                if pending_references_operation(state, &journal) {
                    continue;
                }
                // 预备阶段没有状态提交的可能，却已无 pending：材料来历不明，保留现场。
                return Err(LayoutError::RecoveryRequired {
                    journal_path: path,
                    reason: "插件事务未完成但状态没有对应待处理操作",
                });
            }
        }
    }
    Ok(cleanup_failed)
}

/// 证据缺失或摘要变化不足以继续时，把底层错误升级为保留现场的恢复错误。
fn insufficient_evidence(path: &Path, error: LayoutError) -> LayoutError {
    match error {
        LayoutError::Missing(_) | LayoutError::Conflict(_) | LayoutError::Unsafe(_) => {
            LayoutError::RecoveryRequired {
                journal_path: path.to_path_buf(),
                reason: "插件事务证据与 journal 不一致",
            }
        }
        other => other,
    }
}

/// journal 中的身份必须与按操作 UUID 推导出的固定路径完全一致。
fn journal_matches_layout(root: &Path, journal: &PluginLayoutJournal) -> Result<bool, LayoutError> {
    let package = relative_path(root, &package_directory(root, &journal.id))?;
    let expected_staging = journal
        .staging
        .as_ref()
        .map(|_| {
            relative_path(
                root,
                &staging_directory(root, &journal.id, &journal.operation_id)?,
            )
        })
        .transpose()?;
    let expected_backup = journal
        .backup
        .as_ref()
        .map(|_| {
            relative_path(
                root,
                &backup_directory(root, &journal.id, &journal.operation_id)?,
            )
        })
        .transpose()?;
    let after_ok = match (&journal.after, journal.action) {
        (Some(after), LayoutAction::Install | LayoutAction::Update) => after.relative == package,
        (None, LayoutAction::Uninstall) => true,
        _ => false,
    };
    let before_ok = match (&journal.before, journal.action) {
        (Some(before), LayoutAction::Update | LayoutAction::Uninstall) => {
            before.relative == package
        }
        (None, LayoutAction::Install) => true,
        _ => false,
    };
    Ok(after_ok
        && before_ok
        && journal.staging.as_ref().map(|value| &value.relative) == expected_staging.as_ref()
        && journal.backup.as_ref().map(|value| &value.relative) == expected_backup.as_ref())
}

/// state 中是否仍存在让本次事务继续有效的 pending。
fn pending_references_operation(
    state: &kerminal_domain::plugins::PluginStoreState,
    journal: &PluginLayoutJournal,
) -> bool {
    state.pending.iter().any(|change| match change {
        kerminal_domain::plugins::PendingPluginChange::Install { staged, .. }
        | kerminal_domain::plugins::PendingPluginChange::Update { staged, .. } => {
            matches!(journal.action, LayoutAction::Install | LayoutAction::Update)
                && staged == &journal.operation_id
        }
        kerminal_domain::plugins::PendingPluginChange::Uninstall { operation_id, .. } => {
            journal.action == LayoutAction::Uninstall && operation_id == &journal.operation_id
        }
        _ => false,
    })
}

/// 已提交事务必须体现在权威状态中：安装/更新有记录，卸载没有记录。
fn state_matches_operation(
    state: &kerminal_domain::plugins::PluginStoreState,
    journal: &PluginLayoutJournal,
) -> bool {
    match journal.action {
        LayoutAction::Uninstall => !state.plugins.contains_key(&journal.id),
        LayoutAction::Install | LayoutAction::Update => {
            journal.after.as_ref().is_some_and(|after| {
                state.plugins.get(&journal.id).is_some_and(|installed| {
                    installed.fingerprint.as_ref() == Some(&after.fingerprint)
                })
            })
        }
    }
}

/// 证据可选时缺失即不匹配；存在时必须通过实际目录摘要复核。
fn evidence_matches(root: &Path, evidence: Option<&PathEvidence>) -> Result<bool, LayoutError> {
    match evidence {
        Some(evidence) => match verify_evidence(root, evidence) {
            Ok(_) => Ok(true),
            Err(LayoutError::Missing(_)) | Err(LayoutError::Conflict(_)) => Ok(false),
            Err(error) => Err(error),
        },
        None => Ok(false),
    }
}

/// 只在 journal 为 Committed 后清理它引用的材料；后续阶段写入失败会留下可重试证据。
/// 启动提交成功后由 boot 调用，恢复路径也复用同一清理顺序。
pub fn finish_committed(
    root: &Path,
    journal: &mut PluginLayoutJournal,
) -> Result<bool, LayoutError> {
    let mut complete = true;
    for evidence in [journal.staging.as_ref(), journal.backup.as_ref()]
        .into_iter()
        .flatten()
    {
        let path = resolve_relative(root, &evidence.relative)?;
        if remove_tree_verified(&path).is_err() {
            complete = false;
        } else {
            // 操作子目录清空后同步收掉空父目录，避免长期残留误导用户以为仍有备份。
            prune_empty_parents(root, &path);
        }
    }
    if !complete {
        return Ok(false);
    }
    journal.phase = LayoutPhase::Cleaned;
    write_journal(root, journal)?;
    let directory = migration_directory(root, &journal.operation_id)?;
    remove_tree_verified(&directory)?;
    prune_empty_parents(root, &directory);
    Ok(true)
}

/// 只删除确实为空的受控祖先目录；遇到非空、链接或读取失败立即停手。
///
/// 清理范围限定在 `plugins/` 下，防止 journal 数据异常时误删数据根其他目录。
fn prune_empty_parents(root: &Path, removed: &Path) {
    let plugins = plugin_root(root);
    let mut current = removed.parent();
    while let Some(directory) = current {
        if directory == plugins || !directory.starts_with(&plugins) {
            break;
        }
        let removable = match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.is_dir() && safe_metadata(&metadata) => {
                match fs::read_dir(directory) {
                    Ok(mut entries) => entries.next().is_none(),
                    Err(_) => false,
                }
            }
            _ => false,
        };
        if !removable || fs::remove_dir(directory).is_err() {
            break;
        }
        current = directory.parent();
    }
}
