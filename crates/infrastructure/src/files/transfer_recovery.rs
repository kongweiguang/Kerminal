//! 当前传输恢复材料的原子存储；运行态租约、认证和进程句柄不进入磁盘模型。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    transfers::{
        MAX_TRANSFER_ARCHIVE_PAGE_SIZE, TransferCheckpoint, TransferRecoveryInspection,
        TransferRecoveryPage, TransferRecoveryRecord, TransferRecoveryRepository,
        TransferStagingOwnership,
    },
};
use kerminal_domain::{
    files::{
        ConflictPolicy, ContainerRuntime, FileEndpoint, FileRevision, RemotePath, TransferKind,
        TransferRequest,
    },
    identity::{HostId, TransferId},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::storage::{AtomicCommitError, CommitMode, StorageCoordinator, commit_staged_file};

const MAX_DOCUMENT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RECORDS: usize = 5_000;

/// 只接受已解析的隔离或正式配置根；所有实例共用 storage coordinator 的跨进程门禁。
#[derive(Clone)]
pub struct JsonTransferRecoveryRepository {
    storage: Arc<StorageCoordinator>,
    lock: Arc<Mutex<()>>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecoveryDocument {
    schema_version: u32,
    records: Vec<StoredRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRecord {
    id: TransferId,
    source: StoredEndpoint,
    destination: StoredEndpoint,
    kind: String,
    conflict_policy: String,
    idle_timeout_seconds: Option<u16>,
    actual_destination: Option<StoredEndpoint>,
    checkpoint: Option<StoredCheckpoint>,
    staging_ownership: Vec<StoredStagingOwnership>,
    cleanup_pending: bool,
    commit_unknown: bool,
    inspection: String,
    archived: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum StoredEndpoint {
    Local {
        path: PathBuf,
    },
    Remote {
        host_id: HostId,
        path: String,
    },
    Container {
        host_id: Option<HostId>,
        runtime: ContainerRuntime,
        container_id: String,
        path: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredCheckpoint {
    source_size: u64,
    source_modified: Option<String>,
    source_permissions: Option<u32>,
    source_content_digest: Option<String>,
    confirmed_bytes: u64,
    confirmed_prefix_sha256: String,
    staging_owner: TransferId,
    staging_destination: StoredEndpoint,
    staging_token: String,
    actual_destination: StoredEndpoint,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredStagingOwnership {
    staging_destination: StoredEndpoint,
    actual_destination: StoredEndpoint,
    staging_owner: TransferId,
    staging_token: String,
}

impl JsonTransferRecoveryRepository {
    /// 独立使用时构造不访问文件系统；桌面组合根应注入共享 coordinator，避免同进程锁冲突。
    ///
    /// @author kongweiguang
    pub fn new(config_root: impl Into<PathBuf>) -> Self {
        Self::with_storage_coordinator(StorageCoordinator::new(config_root))
    }

    /// 与配置、known_hosts 等 adapter 共用同一进程锁和根目录锁，避免同进程误判为外部活锁。
    /// 不在仓储内新建 coordinator：独立 Mutex 会让合法并发操作 fail-fast 为 Locked。
    ///
    /// @author kongweiguang
    pub fn with_storage_coordinator(storage: StorageCoordinator) -> Self {
        Self {
            storage: Arc::new(storage),
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// active 与冷档共用跨进程根锁；读取不隐式搬迁，明确写操作才执行先存档后移除。
    /// 保留根锁错误的具体来源，交由调用方显式重读，不把锁竞争伪装成空列表或隐式重试写入。
    /// @author kongweiguang
    async fn transact<T: Send + 'static>(
        &self,
        cancellation: CancellationToken,
        update: impl FnOnce(
            &Path,
            &mut Vec<TransferRecoveryRecord>,
            &CancellationToken,
        ) -> AppResult<(T, bool)>
        + Send
        + 'static,
    ) -> AppResult<T> {
        let _guard = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("恢复材料操作已取消")),
            guard = self.lock.lock() => guard,
        };
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || {
            if cancellation.is_cancelled() {
                return Err(AppError::cancelled("恢复材料操作已取消"));
            }
            let _storage_guard = storage.lock().map_err(|source| {
                AppError::infrastructure_with_source("无法取得恢复材料存储锁", source)
            })?;
            let path = storage.root().join("data").join("transfer-recovery.json");
            let mut records = read_records(&path)?;
            let (result, changed) = update(&path, &mut records, &cancellation)?;
            if changed {
                write_records(&path, records, &cancellation)?;
            }
            Ok(result)
        })
        .await
        .map_err(|error| {
            AppError::outcome_unknown_with_source("恢复材料操作回执未知，请重新读取确认", error)
        })?
    }
}

#[async_trait]
impl TransferRecoveryRepository for JsonTransferRecoveryRepository {
    /// 只读投影待恢复项；冷档优先且必须与双份 active 事实严格一致，不能启动时自动改盘。
    /// @author kongweiguang
    async fn list(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<TransferRecoveryRecord>> {
        self.transact(cancellation, |path, records, token| {
            let mut active = Vec::new();
            for record in records.iter() {
                ensure_not_cancelled(token)?;
                match read_archive(path, &record.id)? {
                    Some(archived) => compare_archived_record(record, &archived)?,
                    None if !record.archived => active.push(record.clone()),
                    None => {}
                }
            }
            Ok((active, false))
        })
        .await
    }

    /// 写前幂等搬迁旧归档以释放原容量；已冷归档 ID 不能被迟到进度或普通 upsert 覆写。
    /// @author kongweiguang
    async fn upsert(
        &self,
        record: TransferRecoveryRecord,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        self.transact(cancellation, move |path, records, token| {
            migrate_archived_records(path, records, token)?;
            if record.archived || read_archive(path, &record.id)?.is_some() {
                return Err(AppError::conflict("已归档证据不能由传输更新覆盖"));
            }
            if let Some(current) = records.iter_mut().find(|current| current.id == record.id) {
                *current = record;
            } else {
                records.push(record);
            }
            Ok(((), true))
        })
        .await
    }

    /// 删除只操作 active，不删除冷档；写前搬迁旧归档时先保存全部证据再释放原文档槽位。
    /// @author kongweiguang
    async fn remove(&self, id: &TransferId, cancellation: CancellationToken) -> AppResult<()> {
        let id = id.clone();
        self.transact(cancellation, move |path, records, token| {
            let before = records.len();
            migrate_archived_records(path, records, token)?;
            records.retain(|record| record.id != id);
            Ok(((), records.len() != before))
        })
        .await
    }

    /// 冷档提交确认后才原子移除 active；中断留下的双份只允许完全相同的事实幂等收敛。
    /// @author kongweiguang
    async fn archive(&self, id: &TransferId, cancellation: CancellationToken) -> AppResult<()> {
        let id = id.clone();
        self.transact(cancellation, move |path, records, token| {
            let before = records.len();
            migrate_archived_records(path, records, token)?;
            if let Some(record) = records.iter().find(|record| record.id == id) {
                store_archive(path, record, token)?;
                records.retain(|record| record.id != id);
            } else if read_archive(path, &id)?.is_none() {
                return Err(AppError::not_found("传输恢复记录不存在"));
            }
            Ok(((), records.len() != before))
        })
        .await
    }

    /// 扫描目录只保留 limit+1 个键；仅反序列化返回页，旧 active 归档也参与同一稳定分页。
    /// @author kongweiguang
    async fn list_archived(
        &self,
        cursor: Option<TransferId>,
        limit: usize,
        cancellation: CancellationToken,
    ) -> AppResult<TransferRecoveryPage> {
        if !(1..=MAX_TRANSFER_ARCHIVE_PAGE_SIZE).contains(&limit) {
            return Err(AppError::invalid("归档分页数量必须在 1–100 之间"));
        }
        self.transact(cancellation, move |path, records, token| {
            Ok((
                read_archive_page(path, records, cursor.as_ref(), limit, token)?,
                false,
            ))
        })
        .await
    }
}

/// 路径只使用固定 SHA256 ID 派生键，兼容旧不透明 ID 而不引入路径注入或超长文件名。
/// @author kongweiguang
fn archive_key(id: &TransferId) -> String {
    format!("{:x}", Sha256::digest(id.as_str().as_bytes()))
}

/// 冷档与 active 同属配置根，目录链接或其它对象不能成为恢复材料的新落点。
/// @author kongweiguang
fn archive_directory(active_path: &Path) -> AppResult<PathBuf> {
    let directory = active_path
        .parent()
        .ok_or_else(corrupt)?
        .join("transfer-recovery-archive");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(error)),
        Ok(_) => return Err(corrupt()),
    }
    Ok(directory)
}

/// 单档沿用原 schema 和校验器；文件名、记录 ID、归档标记必须一致，不接收半档或错档。
/// @author kongweiguang
fn read_archive(active_path: &Path, id: &TransferId) -> AppResult<Option<TransferRecoveryRecord>> {
    read_archive_key(active_path, &archive_key(id)).and_then(|record| {
        if record.as_ref().is_some_and(|record| &record.id != id) {
            Err(corrupt())
        } else {
            Ok(record)
        }
    })
}

/// 分页按固定文件键读取一份冷档，拒绝与内容 ID 不匹配的档案，哈希碰撞也不能覆写证据。
/// @author kongweiguang
fn read_archive_key(active_path: &Path, key: &str) -> AppResult<Option<TransferRecoveryRecord>> {
    let path = archive_directory(active_path)?.join(format!("{key}.json"));
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
        Ok(_) => {}
    }
    let mut records = read_records(&path)?;
    if records.len() != 1 || !records[0].archived || archive_key(&records[0].id) != key {
        return Err(corrupt());
    }
    Ok(records.pop())
}

/// 只允许归档标记从 false 变 true；ownership、checkpoint、未知标记和核对事实均逐字段保留。
/// @author kongweiguang
fn compare_archived_record(
    active: &TransferRecoveryRecord,
    archived: &TransferRecoveryRecord,
) -> AppResult<()> {
    let mut expected = active.clone();
    expected.archived = true;
    if &expected == archived {
        Ok(())
    } else {
        Err(AppError::conflict(
            "冷归档与待恢复材料事实不一致；原证据均已保留",
        ))
    }
}

/// 冷档先以现有原子提交保存完整证据，已存在档只比较不更新，未知回执保留 active 供回读。
/// @author kongweiguang
fn store_archive(
    active_path: &Path,
    record: &TransferRecoveryRecord,
    cancellation: &CancellationToken,
) -> AppResult<()> {
    ensure_not_cancelled(cancellation)?;
    if let Some(archived) = read_archive(active_path, &record.id)? {
        compare_archived_record(record, &archived)?;
    } else {
        let mut archived = record.clone();
        archived.archived = true;
        let path =
            archive_directory(active_path)?.join(format!("{}.json", archive_key(&record.id)));
        write_document(&path, vec![archived], cancellation, true)?;
    }
    let path = archive_directory(active_path)?.join(format!("{}.json", archive_key(&record.id)));
    // 回读到冷档不等于确认上次未知提交已耐久；再次同步后才能删除 active 的唯一旧回执。
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .and_then(|file| file.sync_all())
        .map_err(|error| {
            AppError::outcome_unknown_with_source("归档耐久性尚未确认；原待恢复材料保留", error)
        })?;
    #[cfg(unix)]
    for directory in [path.parent(), path.parent().and_then(Path::parent)]
        .into_iter()
        .flatten()
    {
        fs::File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(|error| {
                AppError::outcome_unknown_with_source(
                    "归档目录耐久性尚未确认；原待恢复材料保留",
                    error,
                )
            })?;
    }
    Ok(())
}

/// 只有明确写操作才迁移旧档或收敛双份；任何冷档失败都会阻止新 IO 并保留原 active 文档。
/// @author kongweiguang
fn migrate_archived_records(
    path: &Path,
    records: &mut Vec<TransferRecoveryRecord>,
    cancellation: &CancellationToken,
) -> AppResult<()> {
    let mut archived_ids = HashSet::new();
    for record in records.iter() {
        ensure_not_cancelled(cancellation)?;
        if record.archived || read_archive(path, &record.id)?.is_some() {
            store_archive(path, record, cancellation)?;
            archived_ids.insert(record.id.clone());
        }
    }
    records.retain(|record| !archived_ids.contains(&record.id));
    Ok(())
}

/// 扫描内存为单页加一个哨兵，不建立全历史索引；异常目录项不会被悄悄当作不存在。
/// @author kongweiguang
fn read_archive_page(
    path: &Path,
    active: &[TransferRecoveryRecord],
    cursor: Option<&TransferId>,
    limit: usize,
    cancellation: &CancellationToken,
) -> AppResult<TransferRecoveryPage> {
    let after = cursor.map(archive_key);
    let mut keys = BTreeMap::<String, Option<TransferRecoveryRecord>>::new();
    for record in active.iter().filter(|record| record.archived) {
        let key = archive_key(&record.id);
        if after.as_ref().is_none_or(|after| &key > after) {
            keys.insert(key, Some(record.clone()));
            if keys.len() > limit + 1 {
                keys.pop_last();
            }
        }
    }
    let directory = archive_directory(path)?;
    match fs::read_dir(directory) {
        Ok(entries) => {
            for entry in entries {
                ensure_not_cancelled(cancellation)?;
                let entry = entry.map_err(io_error)?;
                let kind = entry.file_type().map_err(io_error)?;
                if !kind.is_file() || kind.is_symlink() {
                    return Err(corrupt());
                }
                let name = entry.file_name();
                let name = name.to_str().ok_or_else(corrupt)?;
                // 只识别本仓储的固定随机暂存名，未知目录项仍须报损坏并保留。
                if name
                    .strip_prefix(".kr-")
                    .and_then(|nonce| nonce.strip_suffix(".tmp"))
                    .is_some_and(valid_staging_token)
                {
                    continue;
                }
                let key = name
                    .strip_suffix(".json")
                    .filter(|key| {
                        key.len() == 64
                            && key
                                .bytes()
                                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                    })
                    .ok_or_else(corrupt)?;
                if after.as_ref().is_none_or(|after| key > after.as_str()) {
                    keys.entry(key.to_owned()).or_insert(None);
                    if keys.len() > limit + 1 {
                        keys.pop_last();
                    }
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(error)),
    }
    let has_more = keys.len() > limit;
    let mut records = Vec::with_capacity(limit);
    for (key, legacy) in keys.into_iter().take(limit) {
        ensure_not_cancelled(cancellation)?;
        let record = match read_archive_key(path, &key)? {
            Some(archived) => {
                if let Some(original) = legacy.as_ref() {
                    compare_archived_record(original, &archived)?;
                }
                if let Some(original) = active.iter().find(|record| record.id == archived.id) {
                    compare_archived_record(original, &archived)?;
                }
                archived
            }
            None => legacy.ok_or_else(corrupt)?,
        };
        records.push(record);
    }
    let next_cursor = records
        .last()
        .filter(|_| has_more)
        .map(|record| record.id.clone());
    Ok(TransferRecoveryPage {
        records,
        next_cursor,
    })
}

/// 取消只在持久化线性化之前生效，已保存冷档不能因 UI 取消而回滚或丢弃。
/// @author kongweiguang
fn ensure_not_cancelled(cancellation: &CancellationToken) -> AppResult<()> {
    if cancellation.is_cancelled() {
        Err(AppError::cancelled("恢复材料操作已取消"))
    } else {
        Ok(())
    }
}

/// 迁移预检复用同一解析器；只校验当前格式，不从旧 PID、lease 或临时文件猜测任务。
pub fn validate_transfer_recovery_document(source: &str) -> AppResult<()> {
    decode_records(source).map(|_| ())
}

/// 损坏、未来版本及过大数据一律阻止写入，不能回退为空仓储。
fn decode_records(source: &str) -> AppResult<Vec<TransferRecoveryRecord>> {
    if source.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(corrupt());
    }
    let document: RecoveryDocument = serde_json::from_str(source).map_err(|_| corrupt())?;
    if document.schema_version != 1 || document.records.len() > MAX_RECORDS {
        return Err(corrupt());
    }
    let mut identities = HashSet::new();
    document
        .records
        .into_iter()
        .map(|stored| {
            if !identities.insert(stored.id.clone()) {
                return Err(corrupt());
            }
            let record = stored.into_record()?;
            validate_record(&record)?;
            Ok(record)
        })
        .collect()
}

/// 目标必须是普通文件；符号链接或目录不作为可信恢复源，也不被后续原子替换覆盖。
fn read_records(path: &Path) -> AppResult<Vec<TransferRecoveryRecord>> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() <= MAX_DOCUMENT_BYTES => {}
        Ok(_) => return Err(corrupt()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_error(error)),
    }
    decode_records(&fs::read_to_string(path).map_err(io_error)?)
}

/// 写入前先校验全部事实，使用随机短暂存名和现有原子提交；未知结果保留证据不重放。
/// @author kongweiguang
fn write_records(
    path: &Path,
    records: Vec<TransferRecoveryRecord>,
    cancellation: &CancellationToken,
) -> AppResult<()> {
    write_document(path, records, cancellation, false)
}

/// active 可原子替换，冷档只能排他新建；两者复用同一耐久提交及未知回执分类。
/// @author kongweiguang
fn write_document(
    path: &Path,
    records: Vec<TransferRecoveryRecord>,
    cancellation: &CancellationToken,
    create_only: bool,
) -> AppResult<()> {
    if records.len() > MAX_RECORDS {
        return Err(AppError::conflict("传输恢复记录已达上限，请先核对归档"));
    }
    for record in &records {
        validate_record(record)?;
    }
    let document = RecoveryDocument {
        schema_version: 1,
        records: records.iter().map(StoredRecord::from_record).collect(),
    };
    let mut bytes = serde_json::to_vec_pretty(&document)
        .map_err(|_| AppError::infrastructure("恢复材料无法编码"))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(AppError::conflict("传输恢复材料过大，请先核对归档"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| AppError::invalid("恢复材料路径无效"))?;
    if let Ok(metadata) = fs::symlink_metadata(parent)
        && (metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        return Err(AppError::conflict("恢复材料目录不是普通目录"));
    }
    fs::create_dir_all(parent).map_err(io_error)?;
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("恢复材料保存已取消"));
    }
    let temporary = parent.join(format!(".kr-{}.tmp", Uuid::new_v4().simple()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(io_error)?;
    let prepared = file.write_all(&bytes).and_then(|_| file.sync_all());
    drop(file);
    if let Err(error) = prepared {
        let _ = fs::remove_file(&temporary);
        return Err(io_error(error));
    }
    if cancellation.is_cancelled() {
        let _ = fs::remove_file(&temporary);
        return Err(AppError::cancelled("恢复材料保存已取消"));
    }
    let mode = match fs::symlink_metadata(path) {
        Ok(metadata)
            if !create_only && metadata.is_file() && !metadata.file_type().is_symlink() =>
        {
            CommitMode::ReplaceExisting
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => CommitMode::CreateNew,
        _ => {
            let _ = fs::remove_file(&temporary);
            return Err(corrupt());
        }
    };
    match commit_staged_file(&temporary, path, mode) {
        Ok(()) => Ok(()),
        Err(AtomicCommitError::CommitUnknown(_)) => Err(AppError::outcome_unknown(
            "恢复材料提交结果未知，请重新读取确认",
        )),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(AppError::infrastructure_with_source(
                "恢复材料提交失败",
                error,
            ))
        }
    }
}

/// 恢复事实只允许原请求范围内的实际目标；继任任务可保留旧 owner，但必须匹配原暂存命名和 token。
/// @author kongweiguang
fn validate_record(record: &TransferRecoveryRecord) -> AppResult<()> {
    if record.request.source == record.request.destination
        || record
            .request
            .idle_timeout_seconds
            .is_some_and(|value| !(30..=3600).contains(&value))
        || record
            .actual_destination
            .as_ref()
            .is_some_and(|actual| !permitted_root_destination(&record.request, actual))
    {
        return Err(corrupt());
    }
    if record.staging_ownership.len() > 512 {
        return Err(corrupt());
    }
    let mut staging_paths = HashSet::new();
    for ownership in &record.staging_ownership {
        if !staging_paths.insert(ownership.staging_destination.clone())
            || ownership.staging_destination == ownership.actual_destination
            || !permitted_owned_destination(record, &ownership.actual_destination)
            || !same_parent_directory(
                &ownership.staging_destination,
                &ownership.actual_destination,
            )
            || !valid_staging_token(&ownership.staging_token)
            || !staging_name_matches(
                &ownership.staging_destination,
                &ownership.staging_owner,
                record.request.kind == TransferKind::Directory
                    && record.actual_destination.as_ref() != Some(&ownership.actual_destination),
            )
        {
            return Err(corrupt());
        }
    }
    if let Some(checkpoint) = &record.checkpoint
        && (record.request.kind != TransferKind::File
            || checkpoint.confirmed_bytes > checkpoint.source_revision.size
            || checkpoint.confirmed_prefix_sha256.len() != 64
            || !checkpoint
                .confirmed_prefix_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || checkpoint
                .source_revision
                .content_digest
                .as_deref()
                .is_none_or(|digest| {
                    digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            || !valid_staging_token(&checkpoint.staging_token)
            || checkpoint.staging_destination == checkpoint.actual_destination
            || !same_parent_directory(
                &checkpoint.staging_destination,
                &checkpoint.actual_destination,
            )
            || !staging_name_matches(
                &checkpoint.staging_destination,
                &checkpoint.staging_owner,
                false,
            )
            || record.actual_destination.as_ref() != Some(&checkpoint.actual_destination)
            || record.staging_ownership.iter().any(|ownership| {
                ownership.staging_destination == checkpoint.staging_destination
                    && (ownership.staging_owner != checkpoint.staging_owner
                        || ownership.staging_token != checkpoint.staging_token
                        || ownership.actual_destination != checkpoint.actual_destination)
            }))
    {
        return Err(corrupt());
    }
    Ok(())
}

/// 冲突改名只允许在原目标同目录内选名，覆盖和跳过不能借恢复记录改换目标。
/// @author kongweiguang
fn permitted_root_destination(request: &TransferRequest, actual: &FileEndpoint) -> bool {
    if request.conflict_policy == ConflictPolicy::Rename {
        same_parent_directory(&request.destination, actual)
    } else {
        request.destination.same_object(actual)
    }
}

/// 目录合并可登记根下子项，文件传输则只接受固定实际目标；不依赖文件存在或跟随链接。
/// @author kongweiguang
fn permitted_owned_destination(record: &TransferRecoveryRecord, actual: &FileEndpoint) -> bool {
    let Some(root) = record.actual_destination.as_ref() else {
        return false;
    };
    root.same_object(actual)
        || (record.request.kind == TransferKind::Directory && endpoint_is_descendant(root, actual))
}

/// 比较纯路径组件而不是字符串前缀，拒绝目录名相似和含父级跳转的伪后代。
/// @author kongweiguang
fn endpoint_is_descendant(root: &FileEndpoint, candidate: &FileEndpoint) -> bool {
    if !root.same_filesystem(candidate) {
        return false;
    }
    match (root, candidate) {
        (FileEndpoint::Local { path: root }, FileEndpoint::Local { path: candidate }) => {
            candidate.strip_prefix(root).is_ok_and(|relative| {
                let mut components = relative.components().peekable();
                components.peek().is_some()
                    && components
                        .all(|component| matches!(component, std::path::Component::Normal(_)))
            })
        }
        _ => {
            let Some(root) = root.remote_path() else {
                return false;
            };
            let Some(candidate) = candidate.remote_path() else {
                return false;
            };
            let root = root.as_str();
            let candidate = candidate.as_str();
            let suffix = if root == "." && !candidate.starts_with('/') {
                Some(candidate)
            } else if root == "/" {
                candidate.strip_prefix('/')
            } else {
                candidate
                    .strip_prefix(root)
                    .and_then(|suffix| suffix.strip_prefix('/'))
            };
            suffix.is_some_and(|suffix| {
                !suffix.is_empty()
                    && suffix
                        .split('/')
                        .all(|part| !part.is_empty() && part != "." && part != "..")
            })
        }
    }
}

/// 根暂存名绑定原 owner；目录子项使用短随机名，其真实 owner 仍由 token/marker 核验而非当前任务 ID 推测。
/// @author kongweiguang
fn staging_name_matches(endpoint: &FileEndpoint, owner: &TransferId, allow_child: bool) -> bool {
    let name = match endpoint {
        FileEndpoint::Local { path } => path.file_name().and_then(|name| name.to_str()),
        _ => endpoint
            .remote_path()
            .and_then(|path| path.as_str().rsplit('/').next()),
    };
    let Some(name) = name else {
        return false;
    };
    let owner: String = owner
        .as_str()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect();
    let prefix = format!(".kerminal-transfer-{owner}-");
    name.strip_prefix(&prefix)
        .or_else(|| {
            allow_child
                .then(|| name.strip_prefix(".kerminal-file-"))
                .flatten()
        })
        .and_then(|nonce| nonce.strip_suffix(".partial"))
        .is_some_and(valid_staging_token)
}

impl StoredRecord {
    /// 存储 DTO 只映射非秘密业务事实，不把后端或运行时类型泄露进格式。
    fn from_record(record: &TransferRecoveryRecord) -> Self {
        Self {
            id: record.id.clone(),
            source: StoredEndpoint::from_endpoint(&record.request.source),
            destination: StoredEndpoint::from_endpoint(&record.request.destination),
            kind: match record.request.kind {
                TransferKind::File => "file",
                TransferKind::Directory => "directory",
            }
            .into(),
            conflict_policy: match record.request.conflict_policy {
                ConflictPolicy::Overwrite => "overwrite",
                ConflictPolicy::Skip => "skip",
                ConflictPolicy::Rename => "rename",
            }
            .into(),
            idle_timeout_seconds: record.request.idle_timeout_seconds,
            actual_destination: record
                .actual_destination
                .as_ref()
                .map(StoredEndpoint::from_endpoint),
            checkpoint: record
                .checkpoint
                .as_ref()
                .map(StoredCheckpoint::from_checkpoint),
            staging_ownership: record
                .staging_ownership
                .iter()
                .map(StoredStagingOwnership::from_ownership)
                .collect(),
            cleanup_pending: record.cleanup_pending,
            commit_unknown: record.commit_unknown,
            inspection: match record.inspection {
                TransferRecoveryInspection::DestinationMatches => "destinationMatches",
                TransferRecoveryInspection::DestinationMissing => "destinationMissing",
                TransferRecoveryInspection::DestinationDiffers => "destinationDiffers",
                TransferRecoveryInspection::Insufficient => "insufficient",
            }
            .into(),
            archived: record.archived,
        }
    }

    /// 未知枚举不默认解释为安全状态，避免未来格式被旧应用盲目覆盖。
    fn into_record(self) -> AppResult<TransferRecoveryRecord> {
        Ok(TransferRecoveryRecord {
            id: self.id,
            request: TransferRequest {
                source: self.source.into_endpoint()?,
                destination: self.destination.into_endpoint()?,
                kind: match self.kind.as_str() {
                    "file" => TransferKind::File,
                    "directory" => TransferKind::Directory,
                    _ => return Err(corrupt()),
                },
                conflict_policy: match self.conflict_policy.as_str() {
                    "overwrite" => ConflictPolicy::Overwrite,
                    "skip" => ConflictPolicy::Skip,
                    "rename" => ConflictPolicy::Rename,
                    _ => return Err(corrupt()),
                },
                idle_timeout_seconds: self.idle_timeout_seconds,
            },
            actual_destination: self
                .actual_destination
                .map(StoredEndpoint::into_endpoint)
                .transpose()?,
            checkpoint: self
                .checkpoint
                .map(StoredCheckpoint::into_checkpoint)
                .transpose()?,
            staging_ownership: self
                .staging_ownership
                .into_iter()
                .map(StoredStagingOwnership::into_ownership)
                .collect::<AppResult<Vec<_>>>()?,
            cleanup_pending: self.cleanup_pending,
            commit_unknown: self.commit_unknown,
            inspection: match self.inspection.as_str() {
                "destinationMatches" => TransferRecoveryInspection::DestinationMatches,
                "destinationMissing" => TransferRecoveryInspection::DestinationMissing,
                "destinationDiffers" => TransferRecoveryInspection::DestinationDiffers,
                "insufficient" => TransferRecoveryInspection::Insufficient,
                _ => return Err(corrupt()),
            },
            archived: self.archived,
        })
    }
}

impl StoredEndpoint {
    /// 路径按原协议编码，远端 POSIX 路径不套用宿主 Windows 路径规范。
    fn from_endpoint(endpoint: &FileEndpoint) -> Self {
        match endpoint {
            FileEndpoint::Local { path } => Self::Local { path: path.clone() },
            FileEndpoint::Remote { host_id, path } => Self::Remote {
                host_id: host_id.clone(),
                path: path.as_str().into(),
            },
            FileEndpoint::Container {
                host_id,
                runtime,
                container_id,
                path,
            } => Self::Container {
                host_id: host_id.clone(),
                runtime: *runtime,
                container_id: container_id.clone(),
                path: path.as_str().into(),
            },
        }
    }

    /// 反序列化重做路径不变量校验，但不会连接主机或检查目标存在性。
    fn into_endpoint(self) -> AppResult<FileEndpoint> {
        Ok(match self {
            Self::Local { path } if path.as_os_str().is_empty() => return Err(corrupt()),
            Self::Local { path } => FileEndpoint::Local { path },
            Self::Remote { host_id, path } => {
                FileEndpoint::remote(host_id, &path).map_err(|_| corrupt())?
            }
            Self::Container {
                host_id,
                runtime,
                container_id,
                path,
            } => {
                if container_id.is_empty() || container_id.chars().any(char::is_control) {
                    return Err(corrupt());
                }
                FileEndpoint::Container {
                    host_id,
                    runtime,
                    container_id,
                    path: RemotePath::parse_container(&path).map_err(|_| corrupt())?,
                }
            }
        })
    }
}

impl StoredCheckpoint {
    /// 检查点保留原 stage owner 和 token，重启不生成新所有者冒认旧暂存。
    fn from_checkpoint(value: &TransferCheckpoint) -> Self {
        Self {
            source_size: value.source_revision.size,
            source_modified: value.source_revision.modified.clone(),
            source_permissions: value.source_revision.permissions,
            source_content_digest: value.source_revision.content_digest.clone(),
            confirmed_bytes: value.confirmed_bytes,
            confirmed_prefix_sha256: value.confirmed_prefix_sha256.clone(),
            staging_owner: value.staging_owner.clone(),
            staging_destination: StoredEndpoint::from_endpoint(&value.staging_destination),
            staging_token: value.staging_token.clone(),
            actual_destination: StoredEndpoint::from_endpoint(&value.actual_destination),
        }
    }

    /// 所有 stage 和实际目标均从持久材料还原，不从最终文件名推断临时文件位置。
    fn into_checkpoint(self) -> AppResult<TransferCheckpoint> {
        Ok(TransferCheckpoint {
            source_revision: FileRevision {
                size: self.source_size,
                modified: self.source_modified,
                permissions: self.source_permissions,
                content_digest: self.source_content_digest,
            },
            confirmed_bytes: self.confirmed_bytes,
            confirmed_prefix_sha256: self.confirmed_prefix_sha256,
            staging_owner: self.staging_owner,
            staging_destination: self.staging_destination.into_endpoint()?,
            staging_token: self.staging_token,
            actual_destination: self.actual_destination.into_endpoint()?,
        })
    }
}

impl StoredStagingOwnership {
    /// 创建意图独立于可信字节断点，崩溃发生在首块写入前也能定位随机暂存及原 owner。
    fn from_ownership(ownership: &TransferStagingOwnership) -> Self {
        Self {
            staging_destination: StoredEndpoint::from_endpoint(&ownership.staging_destination),
            actual_destination: StoredEndpoint::from_endpoint(&ownership.actual_destination),
            staging_owner: ownership.staging_owner.clone(),
            staging_token: ownership.staging_token.clone(),
        }
    }

    /// 还原只表示登记过的清理责任，不证明 marker 或数据对象确已创建。
    fn into_ownership(self) -> AppResult<TransferStagingOwnership> {
        Ok(TransferStagingOwnership {
            staging_destination: self.staging_destination.into_endpoint()?,
            actual_destination: self.actual_destination.into_endpoint()?,
            staging_owner: self.staging_owner,
            staging_token: self.staging_token,
        })
    }
}

/// 随机暂存必须是同目录对象，不能利用恢复文件把清理所有权扩展到其它目录或文件系统。
fn same_parent_directory(left: &FileEndpoint, right: &FileEndpoint) -> bool {
    if !left.same_filesystem(right) {
        return false;
    }
    match (left, right) {
        (FileEndpoint::Local { path: left }, FileEndpoint::Local { path: right }) => {
            left.parent().is_some() && left.parent() == right.parent()
        }
        _ => {
            let parent = left.remote_path().and_then(RemotePath::parent);
            parent.is_some() && parent == right.remote_path().and_then(RemotePath::parent)
        }
    }
}

/// marker token 保持与队列及执行器一致的固定小写十六进制，不接收任意路径或文本。
fn valid_staging_token(token: &str) -> bool {
    token.len() == 32
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 对外仅给出固定数据集名称，不把敏感目录或 JSON 正文放入错误摘要。
fn corrupt() -> AppError {
    AppError::configuration_corrupt("传输恢复材料")
}

/// IO 原因交给结构化诊断，界面使用脱敏业务提示。
fn io_error(error: io::Error) -> AppError {
    AppError::infrastructure_with_source("传输恢复材料读写失败", error)
}
