//! 共享用户数据根上的进程内与跨进程互斥。
//!
//! @author kongweiguang

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, System};
use uuid::Uuid;

const STORAGE_LOCK_FILE: &str = ".storage.lock";
const STORAGE_LOCK_SCHEMA_VERSION: u32 = 1;
const MAX_STORAGE_LOCK_BYTES: u64 = 4 * 1024;

#[cfg(windows)]
const WINDOWS_LOCK_SENTINEL_OFFSET_HIGH: u32 = 0x4000_0000;

/// 多个持久化 adapter 共用一个锁文件，避免配置、vault 与数据库迁移互相踩写。
#[derive(Clone)]
pub struct StorageCoordinator {
    root: Arc<PathBuf>,
    process_lock: Arc<Mutex<()>>,
}

impl StorageCoordinator {
    /// 将同一个解析后的用户数据根绑定到共享进程锁，调用方无需自行拼锁路径。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Arc::new(root.into()),
            process_lock: Arc::new(Mutex::new(())),
        }
    }

    /// 暴露已解析根目录，让同一 adapter 按固定相对布局定位自己的数据文件。
    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    /// 按旧版可读的 v1 owner metadata 发布根目录锁，跨版本进程对活锁与陈旧锁作出相同判断。
    pub fn lock(&self) -> Result<StorageLock<'_>, StorageLockError> {
        let process_guard = self
            .process_lock
            .lock()
            .map_err(|_| StorageLockError::ProcessLockPoisoned)?;
        fs::create_dir_all(self.root.as_path()).map_err(StorageLockError::Io)?;
        let path = self.root.join(STORAGE_LOCK_FILE);
        let owner = current_lock_metadata().map_err(StorageLockError::Io)?;

        let mut lock_file = None;
        for _ in 0..3 {
            ensure_safe_lock_path(&path)?;
            match publish_lock_metadata(&self.root, &path, &owner) {
                Ok(file) => {
                    lock_file = Some(file);
                    break;
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    match reclaim_stale_lock(&self.root, &path, &owner)? {
                        StaleLockRecovery::Acquired(file) => {
                            lock_file = Some(file);
                            break;
                        }
                        StaleLockRecovery::Retry => continue,
                        StaleLockRecovery::Locked => return Err(StorageLockError::Locked),
                    }
                }
                Err(error) => return Err(StorageLockError::Io(error)),
            }
        }

        let file = lock_file.ok_or(StorageLockError::Locked)?;
        if let Err(error) = sync_commit_directories(None, Some(self.root())) {
            tracing::warn!(
                event = "storage_lock_directory_sync_failed",
                error_kind = ?error.kind(),
                os_error_code = error.raw_os_error(),
            );
        }
        Ok(StorageLock {
            file,
            _process_guard: process_guard,
            path,
            nonce: owner.nonce,
        })
    }
}

/// 持有期间阻止本进程与其他使用同一锁文件的进程执行存储写事务。
pub struct StorageLock<'a> {
    // 先声明 OS 锁句柄，Drop 体完成后先释放跨进程锁，再放开同 coordinator 的进程锁。
    file: LockedStorageFile,
    _process_guard: MutexGuard<'a, ()>,
    path: PathBuf,
    nonce: String,
}

impl Drop for StorageLock<'_> {
    /// 只移除仍带有本守卫 nonce 的 owner 文件；无法确认所有权时保留证据并记录安全诊断。
    fn drop(&mut self) {
        let source = match read_locked_owner_source(self.file.as_file_mut()) {
            Ok(source) => source,
            Err(StorageLockError::Io(error)) => {
                tracing::warn!(
                    event = "storage_lock_release_failed",
                    reason = "read_owner_failed",
                    error_kind = ?error.kind(),
                    os_error_code = error.raw_os_error(),
                );
                return;
            }
            Err(_) => {
                tracing::warn!(
                    event = "storage_lock_release_failed",
                    reason = "owner_metadata_invalid",
                );
                return;
            }
        };
        let owner = match decode_lock_metadata(&source) {
            Ok(owner) => owner,
            Err(()) => {
                tracing::warn!(
                    event = "storage_lock_release_failed",
                    reason = "owner_metadata_invalid",
                );
                return;
            }
        };
        if owner.nonce != self.nonce {
            tracing::warn!(
                event = "storage_lock_release_skipped",
                reason = "owner_nonce_changed",
            );
            return;
        }
        match read_locked_owner_source(self.file.as_file_mut()) {
            Ok(current_source) if current_source == source => {}
            Ok(_) => {
                tracing::warn!(
                    event = "storage_lock_release_skipped",
                    reason = "owner_record_changed",
                );
                return;
            }
            Err(StorageLockError::Io(error)) => {
                tracing::warn!(
                    event = "storage_lock_release_failed",
                    reason = "recheck_owner_failed",
                    error_kind = ?error.kind(),
                    os_error_code = error.raw_os_error(),
                );
                return;
            }
            Err(_) => {
                tracing::warn!(
                    event = "storage_lock_release_failed",
                    reason = "owner_metadata_invalid",
                );
                return;
            }
        }
        match path_refers_to_open_file(&self.path, self.file.as_file()) {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(
                    event = "storage_lock_release_skipped",
                    reason = "owner_path_changed",
                );
                return;
            }
            Err(StorageLockError::Io(error)) => {
                tracing::warn!(
                    event = "storage_lock_release_failed",
                    reason = "recheck_owner_path_failed",
                    error_kind = ?error.kind(),
                    os_error_code = error.raw_os_error(),
                );
                return;
            }
            Err(_) => {
                tracing::warn!(
                    event = "storage_lock_release_failed",
                    reason = "owner_path_unsafe",
                );
                return;
            }
        }

        match fs::remove_file(&self.path) {
            Ok(()) => {
                if let Some(parent) = self.path.parent()
                    && let Err(error) = sync_commit_directories(None, Some(parent))
                {
                    tracing::warn!(
                        event = "storage_lock_release_sync_failed",
                        error_kind = ?error.kind(),
                        os_error_code = error.raw_os_error(),
                    );
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(
                    event = "storage_lock_release_failed",
                    reason = "remove_owner_failed",
                    error_kind = ?error.kind(),
                    os_error_code = error.raw_os_error(),
                );
            }
        }
    }
}

/// 存储锁失败分类；底层 I/O 原因留在 source 中，公开文案不带绝对路径。
#[derive(Debug)]
pub enum StorageLockError {
    ProcessLockPoisoned,
    UnsafeLockPath,
    Locked,
    InvalidMetadata,
    Io(io::Error),
}

impl std::fmt::Display for StorageLockError {
    /// 只返回安全摘要，具体 OS 错误通过 `source` 提供给内部诊断。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ProcessLockPoisoned => "存储进程锁状态异常",
            Self::UnsafeLockPath => "存储锁路径不是普通文件",
            Self::Locked => "存储锁由其他进程持有",
            Self::InvalidMetadata => "存储锁元数据损坏或版本未知",
            Self::Io(_) => "无法取得存储锁",
        })
    }
}

impl std::error::Error for StorageLockError {
    /// 保留锁文件 I/O 原因，但不把路径拼进稳定公开消息。
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::ProcessLockPoisoned
            | Self::UnsafeLockPath
            | Self::Locked
            | Self::InvalidMetadata => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LockMetadata {
    schema_version: u32,
    pid: u32,
    process_started_at_unix_seconds: u64,
    created_at_unix_ms: u64,
    nonce: String,
}

/// 生成与冻结旧版 FileStore 一致的 schema v1 owner 元数据，确保降级读取可识别正常锁文件。
fn current_lock_metadata() -> io::Result<LockMetadata> {
    let pid = std::process::id();
    let process_started_at_unix_seconds = current_process_started_at()?;
    let created_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis()
        .try_into()
        .map_err(|_| io::Error::other("storage lock timestamp overflow"))?;
    Ok(LockMetadata {
        schema_version: STORAGE_LOCK_SCHEMA_VERSION,
        pid,
        process_started_at_unix_seconds,
        created_at_unix_ms,
        nonce: Uuid::new_v4().to_string(),
    })
}

/// 缓存进程启动时间，避免每次短事务锁都刷新整张系统进程表。
fn current_process_started_at() -> io::Result<u64> {
    static START_TIME: OnceLock<u64> = OnceLock::new();
    if let Some(started_at) = START_TIME.get() {
        return Ok(*started_at);
    }

    let pid = std::process::id();
    let system = System::new_all();
    let started_at = system
        .process(Pid::from_u32(pid))
        .map(|process| process.start_time())
        .filter(|started_at| *started_at > 0)
        .ok_or_else(|| io::Error::other("cannot determine current process start time"))?;
    let _ = START_TIME.set(started_at);
    Ok(*START_TIME.get().unwrap_or(&started_at))
}

/// 只有普通锁文件可参与旧版元数据协议，符号链接和目录一律拒绝跟随。
fn ensure_safe_lock_path(path: &Path) -> Result<(), StorageLockError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(StorageLockError::UnsafeLockPath)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StorageLockError::Io(error)),
    }
}

/// 先同步完整 pending TOML，再用 hard-link 原子发布，避免崩溃留下空的可见 owner 文件。
fn publish_lock_metadata(
    root: &Path,
    lock_path: &Path,
    owner: &LockMetadata,
) -> io::Result<LockedStorageFile> {
    let encoded = toml::to_string_pretty(owner)
        .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))?;
    let pending_path = root.join(format!("{STORAGE_LOCK_FILE}.pending-{}", owner.nonce));
    let mut pending = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&pending_path)?;
    let staged = pending
        .write_all(encoded.as_bytes())
        .and_then(|()| pending.sync_all());
    if let Err(error) = staged {
        drop(pending);
        cleanup_pending_lock_file(&pending_path);
        return Err(error);
    }

    let pending = match LockedStorageFile::try_acquire(pending) {
        Ok(pending) => pending,
        Err(AdvisoryLockError::WouldBlock) => {
            cleanup_pending_lock_file(&pending_path);
            return Err(io::Error::new(
                ErrorKind::WouldBlock,
                "cannot lock staged storage owner",
            ));
        }
        Err(AdvisoryLockError::Io(error)) => {
            cleanup_pending_lock_file(&pending_path);
            return Err(error);
        }
    };

    let published = fs::hard_link(&pending_path, lock_path);
    cleanup_pending_lock_file(&pending_path);
    published?;
    Ok(pending)
}

/// 仅清理本次 create_new 成功创建的 pending 文件，避免写入或同步失败留下垃圾目录项。
fn cleanup_pending_lock_file(path: &Path) {
    if let Err(error) = fs::remove_file(path)
        && error.kind() != ErrorKind::NotFound
    {
        tracing::warn!(
            event = "storage_lock_pending_cleanup_failed",
            error_kind = ?error.kind(),
            os_error_code = error.raw_os_error(),
        );
    }
}

/// 解码并校验旧版 v1 owner；未知字段和未来 schema 均保留在磁盘并 fail closed。
fn decode_lock_metadata(source: &str) -> Result<LockMetadata, ()> {
    let metadata = toml::from_str::<LockMetadata>(source).map_err(|_| ())?;
    if metadata.schema_version != STORAGE_LOCK_SCHEMA_VERSION
        || metadata.pid == 0
        || metadata.process_started_at_unix_seconds == 0
        || metadata.created_at_unix_ms == 0
        || metadata.nonce.trim().is_empty()
    {
        return Err(());
    }
    Ok(metadata)
}

/// 仅当 PID 不存在或已被不同启动时间的进程复用时认定陈旧，无法表示的 PID 一律保留。
fn owner_is_provably_dead(owner: &LockMetadata) -> bool {
    #[cfg(unix)]
    if libc::pid_t::try_from(owner.pid).is_err() {
        return false;
    }

    if owner.pid == std::process::id() {
        return current_process_started_at()
            .map(|started_at| started_at != owner.process_started_at_unix_seconds)
            .unwrap_or(false);
    }

    let system = System::new_all();
    match system.process(Pid::from_u32(owner.pid)) {
        Some(process) => {
            let observed_start_time = process.start_time();
            observed_start_time > 0 && observed_start_time != owner.process_started_at_unix_seconds
        }
        None => matches!(probe_target_pid(owner.pid), TargetPidStatus::Missing),
    }
}

/// 目标进程探测结果；只有 Missing 能作为安全回收锁文件的证据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetPidStatus {
    Running,
    Missing,
    Unknown,
}

/// 在 sysinfo 快照未列出 PID 时进行有限的系统探测，未知结果保持锁定以避免误删活 owner。
#[cfg(windows)]
fn probe_target_pid(pid: u32) -> TargetPidStatus {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    // SAFETY: OpenProcess 只获取有限查询权限的句柄；成功后立即关闭，不读写目标进程。
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return classify_windows_open_process_error(unsafe { GetLastError() });
    }

    // SAFETY: 该句柄刚由 OpenProcess 返回且只在本函数内使用。
    if unsafe { CloseHandle(handle) } == 0 {
        tracing::warn!(
            event = "storage_lock_pid_probe_close_failed",
            os_error_code = unsafe { GetLastError() },
        );
    }
    TargetPidStatus::Running
}

/// 将 OpenProcess 失败原因收敛为三态；除明确的无效 PID 外一律不能证明进程不存在。
#[cfg(windows)]
fn classify_windows_open_process_error(error_code: u32) -> TargetPidStatus {
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;

    if error_code == ERROR_INVALID_PARAMETER {
        TargetPidStatus::Missing
    } else {
        TargetPidStatus::Unknown
    }
}

/// 以 kill(pid, 0) 直接探测目标；仅 ESRCH 表示不存在，权限和其它错误均保守保留锁。
#[cfg(unix)]
fn probe_target_pid(pid: u32) -> TargetPidStatus {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return TargetPidStatus::Unknown;
    };
    if pid <= 0 {
        return TargetPidStatus::Unknown;
    }

    // SAFETY: signal 0 不发送信号，只请求内核检查 PID 是否存在及调用方权限。
    if unsafe { libc::kill(pid, 0) } == 0 {
        return TargetPidStatus::Running;
    }
    classify_unix_kill_error(io::Error::last_os_error().raw_os_error())
}

/// 仅把 ESRCH 映射为不存在，避免 EPERM、EINTR 等探测失败被误作陈旧 owner。
#[cfg(unix)]
fn classify_unix_kill_error(error_code: Option<i32>) -> TargetPidStatus {
    if error_code == Some(libc::ESRCH) {
        TargetPidStatus::Missing
    } else {
        TargetPidStatus::Unknown
    }
}

/// 在没有受支持的目标 PID 探测 API 的平台上拒绝回收，保持 fail closed。
#[cfg(not(any(windows, unix)))]
fn probe_target_pid(_pid: u32) -> TargetPidStatus {
    TargetPidStatus::Unknown
}

/// 标记陈旧锁回收结果，区分已发布 owner、路径竞争和不能证明可回收的活动锁。
enum StaleLockRecovery {
    Acquired(LockedStorageFile),
    Retry,
    Locked,
}

/// 持有旧文件对象的 OS 排他锁期间验证 owner、身份与路径，再原子接管 v1 锁文件。
fn reclaim_stale_lock(
    root: &Path,
    lock_path: &Path,
    replacement: &LockMetadata,
) -> Result<StaleLockRecovery, StorageLockError> {
    ensure_safe_lock_path(lock_path)?;
    let file = match OpenOptions::new().read(true).open(lock_path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(StaleLockRecovery::Retry),
        Err(error) => return Err(StorageLockError::Io(error)),
    };
    let mut locked_file = match LockedStorageFile::try_acquire(file) {
        Ok(file) => file,
        Err(AdvisoryLockError::WouldBlock) => return Ok(StaleLockRecovery::Locked),
        Err(AdvisoryLockError::Io(error)) => return Err(StorageLockError::Io(error)),
    };

    if !path_refers_to_open_file(lock_path, locked_file.as_file())? {
        return Ok(StaleLockRecovery::Retry);
    }
    let first_source = read_locked_owner_source(locked_file.as_file_mut())?;
    let first_owner =
        decode_lock_metadata(&first_source).map_err(|_| StorageLockError::InvalidMetadata)?;
    if !owner_is_provably_dead(&first_owner) {
        return Ok(StaleLockRecovery::Locked);
    }

    let confirmed_source = read_locked_owner_source(locked_file.as_file_mut())?;
    let confirmed_owner =
        decode_lock_metadata(&confirmed_source).map_err(|_| StorageLockError::InvalidMetadata)?;
    if confirmed_source != first_source
        || confirmed_owner.nonce != first_owner.nonce
        || !owner_is_provably_dead(&confirmed_owner)
    {
        return Ok(StaleLockRecovery::Locked);
    }
    if !path_refers_to_open_file(lock_path, locked_file.as_file())? {
        return Ok(StaleLockRecovery::Retry);
    }

    match fs::remove_file(lock_path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(StaleLockRecovery::Retry);
        }
        Err(error) => return Err(StorageLockError::Io(error)),
    }
    if let Some(parent) = lock_path.parent() {
        sync_commit_directories(None, Some(parent)).map_err(StorageLockError::Io)?;
    }

    match publish_lock_metadata(root, lock_path, replacement) {
        Ok(file) => Ok(StaleLockRecovery::Acquired(file)),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(StaleLockRecovery::Locked),
        Err(error) => Err(StorageLockError::Io(error)),
    }
}

/// 从已经取得 OS 排他锁的同一文件句柄读取完整 v1 记录，避免 Windows 锁区读冲突。
fn read_locked_owner_source(file: &mut File) -> Result<String, StorageLockError> {
    file.seek(SeekFrom::Start(0))
        .map_err(StorageLockError::Io)?;
    if file.metadata().map_err(StorageLockError::Io)?.len() > MAX_STORAGE_LOCK_BYTES {
        return Err(StorageLockError::InvalidMetadata);
    }
    let mut bytes = Vec::new();
    (&mut *file)
        .take(MAX_STORAGE_LOCK_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(StorageLockError::Io)?;
    if bytes.len() as u64 > MAX_STORAGE_LOCK_BYTES {
        return Err(StorageLockError::InvalidMetadata);
    }
    String::from_utf8(bytes).map_err(|_| StorageLockError::InvalidMetadata)
}

/// 将 OS 排他锁和用于读取 v1 元数据的同一文件句柄绑定，避免路径换主时误释放他人锁。
struct LockedStorageFile {
    file: File,
    #[cfg(windows)]
    overlapped: windows_sys::Win32::System::IO::OVERLAPPED,
}

/// 只区分锁竞争与系统调用失败，让调用方保留锁文件并给出可判定错误。
enum AdvisoryLockError {
    WouldBlock,
    Io(io::Error),
}

impl LockedStorageFile {
    /// Unix 使用 advisory flock；Windows 锁住 EOF 后的 sentinel 字节以保留旧 reader 的正文读取能力。
    /// 统一的文件锁语义让调用方可跨平台沿用同一所有权与错误分类。
    ///
    /// @author kongweiguang
    fn try_acquire(file: File) -> Result<Self, AdvisoryLockError> {
        #[cfg(unix)]
        {
            match file.try_lock() {
                Ok(()) => Ok(Self { file }),
                Err(std::fs::TryLockError::WouldBlock) => Err(AdvisoryLockError::WouldBlock),
                Err(std::fs::TryLockError::Error(error)) => Err(AdvisoryLockError::Io(error)),
            }
        }

        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;

            use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
            use windows_sys::Win32::Storage::FileSystem::{
                LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
            };
            use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};

            let mut overlapped = OVERLAPPED {
                Anonymous: OVERLAPPED_0 {
                    Anonymous: OVERLAPPED_0_0 {
                        Offset: 0,
                        OffsetHigh: WINDOWS_LOCK_SENTINEL_OFFSET_HIGH,
                    },
                },
                ..OVERLAPPED::default()
            };
            // SAFETY: OVERLAPPED 在同步锁期间保持有效，范围固定在 EOF 后的一个字节。
            let locked = unsafe {
                LockFileEx(
                    file.as_raw_handle(),
                    LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                    0,
                    1,
                    0,
                    &mut overlapped,
                )
            };
            if locked == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
                    return Err(AdvisoryLockError::WouldBlock);
                }
                return Err(AdvisoryLockError::Io(error));
            }
            Ok(Self { file, overlapped })
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = file;
            Err(AdvisoryLockError::Io(io::Error::new(
                ErrorKind::Unsupported,
                "advisory storage lock is unavailable on this platform",
            )))
        }
    }

    /// 仅开放给锁内 metadata 与路径身份校验读取，不转移 OS 锁所有权。
    fn as_file(&self) -> &File {
        &self.file
    }

    /// 保持读取位置修改作用于锁句柄本身，避免 Windows 另开句柄触发锁区读冲突。
    fn as_file_mut(&mut self) -> &mut File {
        &mut self.file
    }
}

impl Drop for LockedStorageFile {
    /// Windows 显式释放 sentinel 锁；失败时记录安全诊断，随后关闭句柄仍由系统回收锁。
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;

            use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;

            // SAFETY: `overlapped` 与申请锁时相同，句柄仍然打开且锁只覆盖该字节。
            let unlocked =
                unsafe { UnlockFileEx(self.file.as_raw_handle(), 0, 1, 0, &mut self.overlapped) };
            if unlocked == 0 {
                let error = io::Error::last_os_error();
                tracing::warn!(
                    event = "storage_lock_os_unlock_failed",
                    error_kind = ?error.kind(),
                    os_error_code = error.raw_os_error(),
                );
            }
        }
    }
}

/// 比较当前路径句柄与已锁句柄的文件身份；路径缺失或换主时调用方不得删除目录项。
fn path_refers_to_open_file(path: &Path, opened_file: &File) -> Result<bool, StorageLockError> {
    ensure_safe_lock_path(path)?;
    let current_path_file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(StorageLockError::Io(error)),
    };
    ensure_safe_lock_path(path)?;
    same_file_object(opened_file, &current_path_file).map_err(StorageLockError::Io)
}

/// Unix 通过仍然打开的句柄比较设备与 inode，避免仅依赖可复用路径名。
#[cfg(unix)]
fn same_file_object(left: &File, right: &File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let left = left.metadata()?;
    let right = right.metadata()?;
    if left.ino() == 0 || right.ino() == 0 {
        return Err(io::Error::new(
            ErrorKind::Unsupported,
            "stable file identity is unavailable",
        ));
    }
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

/// Windows 使用 128 位 FileIdInfo 比较两个仍打开的句柄，不在锁定句柄上二次读取。
#[cfg(windows)]
fn same_file_object(left: &File, right: &File) -> io::Result<bool> {
    /// 读取 Windows 128 位文件身份，避免复用 64 位 index 产生误判。
    fn file_identity(file: &File) -> io::Result<(u64, [u8; 16])> {
        use std::os::windows::io::AsRawHandle;

        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
        };

        let mut info = FILE_ID_INFO::default();
        // SAFETY: `info` 是足够大的可写 FILE_ID_INFO，句柄在调用期间保持打开。
        let success = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                (&mut info as *mut FILE_ID_INFO).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if success == 0 {
            return Err(io::Error::last_os_error());
        }
        let identifier = info.FileId.Identifier;
        if identifier.iter().all(|byte| *byte == 0) {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                "stable file identity is unavailable",
            ));
        }
        Ok((info.VolumeSerialNumber, identifier))
    }

    let left_identity = file_identity(left)?;
    let right_identity = file_identity(right)?;
    Ok(left_identity == right_identity)
}

/// 未提供稳定文件身份 API 的目标平台禁止回收，以免把不确定性变成误删除。
#[cfg(not(any(unix, windows)))]
fn same_file_object(_left: &File, _right: &File) -> io::Result<bool> {
    Err(io::Error::new(
        ErrorKind::Unsupported,
        "stable file identity is unavailable on this platform",
    ))
}

/// 已写好同目录 staging 文件提交时使用的原子操作；覆盖与新建语义由调用方显式选择。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitMode {
    /// 安装同目录内容 staging；提交前同步 staging，目标已存在时不覆盖。
    CreateNew,
    /// 用同目录内容 staging 原子替换现有普通文件；保留目标已有权限。
    ReplaceExisting,
    /// 移动既有文件或目录；仅变更目录项，不打开文件、不刷新内容或更改源属性。
    MoveNoReplace,
    /// 提交本机生成的普通文件 staging；可跨父目录但由系统拒绝跨卷移动，先同步内容。
    MoveStagedNoReplace,
}

/// 提交失败可能发生在系统替换调用或目录同步阶段，未知结果要求调用方回读再决定。
#[derive(Debug)]
pub enum AtomicCommitError {
    NotCommitted(io::Error),
    CommitUnknown(io::Error),
}

impl std::fmt::Display for AtomicCommitError {
    /// 公开摘要不含路径；调用方可单独检查 source 和失败阶段。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotCommitted(_) => "原子提交未完成",
            Self::CommitUnknown(_) => "原子提交结果未知",
        })
    }
}

impl std::error::Error for AtomicCommitError {
    /// 保留平台错误供内部诊断，不通过 Display 回显目标路径。
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotCommitted(error) | Self::CommitUnknown(error) => Some(error),
        }
    }
}

/// 提交内容 staging 或移动既有路径；只有显式 staging 模式会同步文件内容，避免改写用户原件属性。
///
/// @author kongweiguang
pub fn commit_staged_file(
    source: &Path,
    destination: &Path,
    mode: CommitMode,
) -> Result<(), AtomicCommitError> {
    #[cfg(windows)]
    {
        // 在系统提交 API 前验证 Win32 路径表示；相对路径只做词法绝对化，目标可尚不存在。
        windows_api_path(source).map_err(AtomicCommitError::NotCommitted)?;
        windows_api_path(destination).map_err(AtomicCommitError::NotCommitted)?;
    }

    let source_metadata = fs::symlink_metadata(source).map_err(AtomicCommitError::NotCommitted)?;
    if source_metadata.file_type().is_symlink() {
        return Err(AtomicCommitError::NotCommitted(io::Error::new(
            io::ErrorKind::InvalidInput,
            "staging source must not be a symbolic link",
        )));
    }
    if mode != CommitMode::MoveNoReplace && !source_metadata.is_file() {
        return Err(AtomicCommitError::NotCommitted(io::Error::new(
            io::ErrorKind::InvalidInput,
            "staging source must be a regular file",
        )));
    }
    if matches!(mode, CommitMode::CreateNew | CommitMode::ReplaceExisting)
        && source.parent() != destination.parent()
    {
        return Err(AtomicCommitError::NotCommitted(io::Error::new(
            io::ErrorKind::InvalidInput,
            "staging file must share its destination directory",
        )));
    }

    if mode == CommitMode::ReplaceExisting {
        let metadata =
            fs::symlink_metadata(destination).map_err(AtomicCommitError::NotCommitted)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(AtomicCommitError::NotCommitted(io::Error::new(
                io::ErrorKind::InvalidInput,
                "replacement destination must be a regular file",
            )));
        }
        preserve_replacement_permissions(source, &metadata)
            .map_err(AtomicCommitError::NotCommitted)?;
    }

    if mode != CommitMode::MoveNoReplace {
        sync_staged_file(source).map_err(AtomicCommitError::NotCommitted)?;
    }

    let committed = match mode {
        CommitMode::CreateNew | CommitMode::MoveNoReplace | CommitMode::MoveStagedNoReplace => {
            rename_no_replace(source, destination).map_err(AtomicCommitError::NotCommitted)
        }
        CommitMode::ReplaceExisting => {
            replace_existing_file(source, destination).map_err(classify_replace_failure)
        }
    };
    committed?;

    sync_commit_directories(source.parent(), destination.parent())
        .map_err(AtomicCommitError::CommitUnknown)
}

/// 删除已由调用方校验并持锁的普通文件；Unix 父目录同步失败按提交结果未知返回。
pub(crate) fn remove_file_with_parent_sync(path: &Path) -> Result<(), AtomicCommitError> {
    remove_file_with_parent_sync_using(path, |parent| sync_commit_directories(None, Some(parent)))
}

/// 把 unlink 和目录同步拆开，使恢复调用方能区分未删除与删除后耐久性未知。
fn remove_file_with_parent_sync_using(
    path: &Path,
    sync_parent: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<(), AtomicCommitError> {
    let metadata = fs::symlink_metadata(path).map_err(AtomicCommitError::NotCommitted)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AtomicCommitError::NotCommitted(io::Error::new(
            io::ErrorKind::InvalidInput,
            "delete target must be a regular file",
        )));
    }
    let parent = path.parent().ok_or_else(|| {
        AtomicCommitError::NotCommitted(io::Error::new(
            io::ErrorKind::InvalidInput,
            "delete target has no parent directory",
        ))
    })?;
    fs::remove_file(path).map_err(AtomicCommitError::NotCommitted)?;
    sync_parent(parent).map_err(AtomicCommitError::CommitUnknown)
}

#[cfg(test)]
#[path = "../tests/support/storage_remove_unit.rs"]
mod remove_file_tests;

/// 在内容提交前同步 staging；Windows 临时清除只读属性以取得写句柄，并在同步后恢复原 Permissions。
/// 该标准库调用只存在于 Windows 条件分支，避免对 Unix 权限语义作无关调整。
///
/// @author kongweiguang
fn sync_staged_file(source: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let metadata = fs::symlink_metadata(source)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "staging source changed before durable sync",
            ));
        }
        let permissions = metadata.permissions();
        let was_readonly = permissions.readonly();
        if was_readonly {
            let mut writable_permissions = permissions.clone();
            // 只在 Windows 暂时清除自有 staging 的只读位，随后仍恢复完整原 Permissions。
            #[expect(
                clippy::permissions_set_readonly_false,
                reason = "仅 Windows 分支为同步 staging 临时移除只读属性；Unix 分支不经过此代码"
            )]
            writable_permissions.set_readonly(false);
            fs::set_permissions(source, writable_permissions)?;
        }

        let sync_result = OpenOptions::new()
            .write(true)
            .open(source)
            .and_then(|file| file.sync_all());
        if !was_readonly {
            return sync_result;
        }

        let restore_result = fs::set_permissions(source, permissions);
        match (sync_result, restore_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(sync_error), Ok(())) => Err(sync_error),
            (Ok(()), Err(restore_error)) => Err(restore_error),
            (Err(sync_error), Err(restore_error)) => {
                Err(io::Error::other(StagedSyncAndRestoreError {
                    sync_error,
                    restore_error,
                }))
            }
        }
    }
    #[cfg(not(windows))]
    {
        File::open(source).and_then(|file| file.sync_all())
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct StagedSyncAndRestoreError {
    sync_error: io::Error,
    restore_error: io::Error,
}

#[cfg(windows)]
impl std::fmt::Display for StagedSyncAndRestoreError {
    /// 同时暴露 flush 和属性恢复的 OS 分类，避免把只读 stage 留下的次生故障隐藏。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "staging flush failed ({:?}, {:?}) and read-only restoration failed ({:?}, {:?})",
            self.sync_error.kind(),
            self.sync_error.raw_os_error(),
            self.restore_error.kind(),
            self.restore_error.raw_os_error(),
        )
    }
}

#[cfg(windows)]
impl std::error::Error for StagedSyncAndRestoreError {
    /// 保留导致提交不继续的 flush 错误，恢复失败的 OS 分类已包含在摘要中。
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.sync_error)
    }
}

/// 替换时复用旧文件权限位，避免用临时文件的默认权限意外放宽秘密配置。
fn preserve_replacement_permissions(source: &Path, destination: &fs::Metadata) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::set_permissions(source, destination.permissions())
    }
    #[cfg(windows)]
    {
        let _ = (source, destination);
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        fs::set_permissions(source, destination.permissions())
    }
}

/// Unix 目录项同步失败发生在 rename 已成功之后，因此报告 commit-unknown 供调用方回读。
fn sync_commit_directories(source: Option<&Path>, destination: Option<&Path>) -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut directories = Vec::new();
        if let Some(source) = source {
            directories.push(source);
        }
        if let Some(destination) = destination
            && !directories.contains(&destination)
        {
            directories.push(destination);
        }
        for directory in directories {
            File::open(directory)?.sync_all()?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (source, destination);
    }
    Ok(())
}

/// Windows ReplaceFileW 对错误后的文件状态不作强承诺，因此系统替换失败需作为未知结果回读。
fn classify_replace_failure(error: io::Error) -> AtomicCommitError {
    #[cfg(windows)]
    {
        AtomicCommitError::CommitUnknown(error)
    }
    #[cfg(not(windows))]
    {
        AtomicCommitError::NotCommitted(error)
    }
}

/// 用操作系统原语完成同卷 no-clobber 重命名，禁止 check-then-rename 覆盖竞态文件。
///
/// @author kongweiguang
fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

        let source = windows_api_path(source)?;
        let destination = windows_api_path(destination)?;
        // 未传 REPLACE_EXISTING，目标已存在时由系统原子拒绝。
        let result = unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        };
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let source = CString::new(source.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        let destination = CString::new(destination.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        // RENAME_NOREPLACE 使检查与移动由一个内核调用完成。
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let source = CString::new(source.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        let destination = CString::new(destination.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        // renamex_np(RENAME_EXCL) 是 macOS 的原子 no-clobber rename。
        let result =
            unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        let _ = (source, destination);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic no-replace rename is unavailable on this Unix target",
        ))
    }
}

/// Unix 同目录 rename 原子替换；Windows ReplaceFileW 保留目标 DACL 和安全属性。
///
/// @author kongweiguang
fn replace_existing_file(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

        let destination = windows_api_path(destination)?;
        let source = windows_api_path(source)?;
        // ReplaceFileW 保留被替换目标的 ACL；source 已在同目录完成写入和同步。
        let result = unsafe {
            ReplaceFileW(
                destination.as_ptr(),
                source.as_ptr(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    #[cfg(not(windows))]
    {
        fs::rename(source, destination)
    }
}

/// 把盘符、UNC 或相对路径转换成 Win32 扩展长度形式，保留已扩展路径且拒绝 NUL。
///
/// 目标经常尚未创建，故只规范分隔符和命名空间前缀；不 canonicalize、解析或替换实际路径项。
///
/// @author kongweiguang
#[cfg(windows)]
pub(crate) fn windows_api_path(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    const SEPARATOR: u16 = b'\\' as u16;
    const EXTENDED_PREFIX: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, SEPARATOR];

    let encoded_input = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if encoded_input.contains(&0) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "path contains an embedded NUL",
        ));
    }
    let is_extended = encoded_input.starts_with(&EXTENDED_PREFIX);
    let absolute = if is_extended {
        if !path.is_absolute() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "extended path must be absolute",
            ));
        }
        path.to_path_buf()
    } else {
        // 仅词法补成绝对路径；canonicalize 会要求目标存在，且会改变链接别名语义。
        std::path::absolute(path)?
    };
    let mut path_wide = absolute.as_os_str().encode_wide().collect::<Vec<_>>();
    if path_wide.contains(&0) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "path contains an embedded NUL",
        ));
    }
    for unit in &mut path_wide {
        if *unit == b'/' as u16 {
            *unit = SEPARATOR;
        }
    }

    if path_wide.starts_with(&EXTENDED_PREFIX) {
        let inner = &path_wide[EXTENDED_PREFIX.len()..];
        let is_drive_root = inner.len() >= 3
            && ((inner[0] >= b'A' as u16 && inner[0] <= b'Z' as u16)
                || (inner[0] >= b'a' as u16 && inner[0] <= b'z' as u16))
            && inner[1] == b':' as u16
            && inner[2] == SEPARATOR;
        let is_unc_marker = inner.len() >= 3
            && (inner[0] == b'U' as u16 || inner[0] == b'u' as u16)
            && (inner[1] == b'N' as u16 || inner[1] == b'n' as u16)
            && (inner[2] == b'C' as u16 || inner[2] == b'c' as u16);
        if is_unc_marker {
            if inner.get(3) != Some(&SEPARATOR) {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "extended UNC path requires a server and share",
                ));
            }
            let mut parts = inner[4..]
                .split(|unit| *unit == SEPARATOR)
                .filter(|part| !part.is_empty());
            if parts.next().is_none() || parts.next().is_none() {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "extended UNC path requires a server and share",
                ));
            }
        } else if !is_drive_root && (inner.is_empty() || !inner.contains(&SEPARATOR)) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "extended path has no rooted namespace",
            ));
        }
    } else if path_wide.starts_with(&[SEPARATOR, SEPARATOR, b'.' as u16, SEPARATOR]) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "device namespace paths are not file paths",
        ));
    } else if path_wide.starts_with(&[SEPARATOR, SEPARATOR]) {
        let unc_tail = &path_wide[2..];
        let mut parts = unc_tail
            .split(|unit| *unit == SEPARATOR)
            .filter(|part| !part.is_empty());
        if parts.next().is_none() || parts.next().is_none() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "UNC path requires a server and share",
            ));
        }
        let mut extended = EXTENDED_PREFIX.to_vec();
        extended.extend([b'U' as u16, b'N' as u16, b'C' as u16, SEPARATOR]);
        extended.extend_from_slice(unc_tail);
        path_wide = extended;
    } else if path_wide.len() < 3
        || !((path_wide[0] >= b'A' as u16 && path_wide[0] <= b'Z' as u16)
            || (path_wide[0] >= b'a' as u16 && path_wide[0] <= b'z' as u16))
        || path_wide[1] != b':' as u16
        || path_wide[2] != SEPARATOR
    {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "an absolute drive, UNC, or extended path is required",
        ));
    } else {
        let mut extended = EXTENDED_PREFIX.to_vec();
        extended.extend_from_slice(&path_wide);
        path_wide = extended;
    }

    path_wide.push(0);
    Ok(path_wide)
}

#[cfg(test)]
#[path = "../tests/support/storage_lock_probe_unit.rs"]
mod storage_lock_probe_unit;

#[cfg(all(test, windows))]
#[path = "../tests/support/storage_windows_path_unit.rs"]
mod storage_windows_path_unit;
