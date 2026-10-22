//! 旧 JSON v1 用户片段的专用、可恢复批量导入仓储。
//!
//! @author kongweiguang

use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use kerminal_application::{
    AppError, AppResult,
    commands::{
        MAX_SNIPPET_TRANSFER_BYTES, MAX_SNIPPET_TRANSFER_ITEMS, SnippetImportBatchRequest,
        SnippetImportBatchState, SnippetImportReceipt,
    },
};
use kerminal_domain::commands::SnippetDefinition;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::storage::{
    AtomicCommitError, CommitMode, commit_staged_file, remove_file_with_parent_sync,
};

use super::{
    builtin_snippets::builtin_snippet_catalog,
    catalog::{FileCommandCatalogRepository, io_failure},
    snippets::serialize_new_snippet,
};

const TRANSACTION_DIRECTORY: &str = ".storage-transactions/snippet-import";
const STAGING_DIRECTORY: &str = "staging";
const RECEIPTS_DIRECTORY: &str = "receipts";
const ACTIVE_JOURNAL_FILE: &str = "active.json";
const JOURNAL_SCHEMA_VERSION: u32 = 1;
const MAX_JOURNAL_BYTES: u64 = 1024 * 1024;
const MAX_RECEIPT_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SnippetImportJournal {
    schema_version: u32,
    batch_id: String,
    payload_sha256: String,
    imported_ids: Vec<String>,
    entries: Vec<SnippetImportJournalEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SnippetImportJournalEntry {
    id: String,
    staging_file: String,
    content_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SnippetImportReceiptFile {
    schema_version: u32,
    payload_sha256: String,
    receipt: SnippetImportReceiptDocument,
}

/// 恢复文件使用独立 DTO，避免 application 回执承担 JSON 存储契约。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SnippetImportReceiptDocument {
    batch_id: String,
    imported_ids: Vec<String>,
}

impl SnippetImportReceiptDocument {
    /// 将持久化边界数据投影成用例回执，保持领域/API DTO 与存储格式解耦。
    fn into_application(self) -> SnippetImportReceipt {
        SnippetImportReceipt {
            batch_id: self.batch_id,
            imported_ids: self.imported_ids,
        }
    }
}

enum StoredRecord<T> {
    Missing,
    Valid(T),
    Invalid,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileState {
    Missing,
    Matches,
    Different,
}

struct ActiveRecovery {
    batch_id: String,
    payload_sha256: String,
    state: SnippetImportBatchState,
}

impl FileCommandCatalogRepository {
    /// 在共享锁内先读回旧结果，再提交整个新建批次；未知结果绝不自动重放。
    pub(super) fn commit_import_batch_sync(
        &self,
        request: SnippetImportBatchRequest,
    ) -> AppResult<SnippetImportBatchState> {
        self.ensure_writable()?;
        validate_request(&request)?;
        let _lock = self.lock()?;
        self.ensure_writable()?;
        let payload_sha256 = payload_fingerprint(&request.definitions)?;

        match read_receipt(self.storage.root(), &request.batch_id)? {
            StoredRecord::Valid(record) => {
                return receipt_state(record, &request.batch_id, &payload_sha256);
            }
            StoredRecord::Invalid => return Ok(SnippetImportBatchState::OutcomeUnknown),
            StoredRecord::Missing => {}
        }

        if let Some(active) = recover_active_journal(self, true)? {
            if active.batch_id == request.batch_id {
                if active.payload_sha256 != payload_sha256 {
                    return Err(AppError::conflict(
                        "片段导入批次标识已用于不同内容，请使用新的批次标识",
                    ));
                }
                return Ok(active.state);
            }
            if active.state == SnippetImportBatchState::OutcomeUnknown {
                return Ok(SnippetImportBatchState::OutcomeUnknown);
            }
            if active_journal_exists(self.storage.root())? {
                return Ok(SnippetImportBatchState::OutcomeUnknown);
            }
        }

        match read_receipt(self.storage.root(), &request.batch_id)? {
            StoredRecord::Valid(record) => {
                return receipt_state(record, &request.batch_id, &payload_sha256);
            }
            StoredRecord::Invalid => return Ok(SnippetImportBatchState::OutcomeUnknown),
            StoredRecord::Missing => {}
        }

        let prepared = prepare_batch(self, &request, &payload_sha256)?;
        match persist_active_journal(self.storage.root(), &prepared.journal)? {
            RecordCommit::Committed => {}
            RecordCommit::NotCommitted(source) => {
                cleanup_staging(self.storage.root(), &prepared.journal.entries);
                return Err(io_failure("写入片段导入恢复记录失败", source));
            }
            RecordCommit::Unknown => return Ok(SnippetImportBatchState::OutcomeUnknown),
        }

        for (index, entry) in prepared.journal.entries.iter().enumerate() {
            let source = staging_path(self.storage.root(), &entry.staging_file);
            let destination = self.document_path("snippets", &entry.id)?;
            match commit_staged_file(&source, &destination, CommitMode::MoveStagedNoReplace) {
                Ok(()) => {}
                Err(AtomicCommitError::NotCommitted(error)) if index == 0 => {
                    if cleanup_uncommitted(self.storage.root(), &prepared.journal).is_ok() {
                        if error.kind() == io::ErrorKind::AlreadyExists {
                            return Err(AppError::conflict(
                                "片段导入期间目标已变化，未覆盖现有文件",
                            ));
                        }
                        return Err(io_failure("原子创建导入片段失败", error));
                    }
                    return Ok(SnippetImportBatchState::OutcomeUnknown);
                }
                Err(AtomicCommitError::NotCommitted(_))
                | Err(AtomicCommitError::CommitUnknown(_)) => {
                    return Ok(recover_active_journal(self, true)?
                        .map_or(SnippetImportBatchState::OutcomeUnknown, |active| {
                            active.state
                        }));
                }
            }
        }

        match commit_receipt(self.storage.root(), &prepared.journal)? {
            RecordCommit::Committed => {}
            RecordCommit::NotCommitted(_) | RecordCommit::Unknown => {
                return Ok(SnippetImportBatchState::OutcomeUnknown);
            }
        }
        if cleanup_staging(self.storage.root(), &prepared.journal.entries) {
            cleanup_active_journal(self.storage.root());
        }
        Ok(SnippetImportBatchState::Committed(SnippetImportReceipt {
            batch_id: request.batch_id,
            imported_ids: prepared.journal.imported_ids,
        }))
    }

    /// 查询 durable receipt 或专用 journal；该路径只回读和恢复已知事务，不创建新导入。
    pub(super) fn import_batch_status_sync(
        &self,
        batch_id: String,
    ) -> AppResult<SnippetImportBatchState> {
        validate_batch_id(&batch_id)?;
        let _lock = self.lock()?;
        if let Some(active) = recover_active_journal(self, self.writable)? {
            if active.batch_id == batch_id {
                return Ok(active.state);
            }
            if active.state == SnippetImportBatchState::OutcomeUnknown {
                return Ok(SnippetImportBatchState::OutcomeUnknown);
            }
        }
        match read_receipt(self.storage.root(), &batch_id)? {
            StoredRecord::Missing => Ok(SnippetImportBatchState::NotCommitted),
            StoredRecord::Invalid => Ok(SnippetImportBatchState::OutcomeUnknown),
            StoredRecord::Valid(record) => Ok(SnippetImportBatchState::Committed(
                record.receipt.into_application(),
            )),
        }
    }

    /// 普通用户目录操作须先持同一 storage lock 调用，避免观察到批量创建的中间态。
    pub(super) fn ensure_import_journal_resolved(&self) -> AppResult<()> {
        let active = recover_active_journal(self, self.writable)?;
        if active.is_some_and(|active| active.state == SnippetImportBatchState::OutcomeUnknown)
            || active_journal_exists(self.storage.root())?
        {
            return Err(AppError::outcome_unknown(
                "片段导入恢复记录仍存在，目录操作已暂停并保留事务状态",
            ));
        }
        Ok(())
    }
}

enum RecordCommit {
    Committed,
    NotCommitted(io::Error),
    Unknown,
}

struct PreparedBatch {
    journal: SnippetImportJournal,
}

/// 预先计算完整 journal 与 receipt 字节数，确保元数据 cap 在写入任何 staging 前生效。
fn validate_metadata_budget(journal: &SnippetImportJournal) -> AppResult<()> {
    let journal_bytes = serde_json::to_vec(journal)
        .map_err(|_| AppError::infrastructure("序列化片段导入恢复记录失败"))?;
    if journal_bytes.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(AppError::invalid("片段导入恢复记录超过 1 MiB 元数据预算"));
    }
    let receipt_bytes = serialize_receipt(journal)?;
    if receipt_bytes.len() as u64 > MAX_RECEIPT_BYTES {
        return Err(AppError::invalid("片段导入回执超过 256 KiB 元数据预算"));
    }
    Ok(())
}

/// 把 journal 的终态 projection 序列化为独立持久化 DTO，供预算检查和提交共用。
fn serialize_receipt(journal: &SnippetImportJournal) -> AppResult<Vec<u8>> {
    let receipt = SnippetImportReceiptFile {
        schema_version: JOURNAL_SCHEMA_VERSION,
        payload_sha256: journal.payload_sha256.clone(),
        receipt: SnippetImportReceiptDocument {
            batch_id: journal.batch_id.clone(),
            imported_ids: journal.imported_ids.clone(),
        },
    };
    serde_json::to_vec(&receipt).map_err(|_| AppError::infrastructure("序列化片段导入回执失败"))
}

/// 全量预检与序列化完成后才创建 staging，避免后续验证失败遗留半批输入。
fn prepare_batch(
    repository: &FileCommandCatalogRepository,
    request: &SnippetImportBatchRequest,
    payload_sha256: &str,
) -> AppResult<PreparedBatch> {
    let root = repository.storage.root();
    if !existing_directory(root)? {
        return Err(AppError::conflict("配置目录不存在，无法准备片段导入"));
    }
    let snippets_directory = root.join("snippets");
    let known_paths = repository.list_paths("snippets")?;
    let mut existing = builtin_snippet_catalog()?
        .into_iter()
        .map(|item| item.id)
        .collect::<HashSet<_>>();
    let mut max_order = None::<i64>;
    for path in known_paths {
        let id = super::catalog::document_id(&path)?;
        existing.insert(id);
        let (_, document, _) = repository
            .read_snippet(&path)?
            .ok_or_else(|| AppError::conflict("片段目录在导入准备期间发生变化"))?;
        let sort_order = document.sort_order();
        max_order = Some(max_order.map_or(sort_order, |current| current.max(sort_order)));
    }

    let mut next_order = max_order
        .map(|order| order.checked_add(1))
        .unwrap_or(Some(0))
        .ok_or_else(|| AppError::conflict("片段目录排序值已达到上限"))?;
    let mut request_ids = HashSet::with_capacity(request.definitions.len());
    let mut prepared_files = Vec::with_capacity(request.definitions.len());
    let mut total_bytes = 0usize;
    let tx_id = Uuid::new_v4().simple().to_string();

    for (index, original) in request.definitions.iter().enumerate() {
        original
            .validate()
            .map_err(|_| AppError::invalid("导入片段未通过定义校验"))?;
        repository.document_path("snippets", &original.id)?;
        if !request_ids.insert(original.id.as_str()) || existing.contains(&original.id) {
            return Err(AppError::conflict(
                "片段标识已存在或在本批中重复，未覆盖任何现有文件",
            ));
        }

        let mut definition = original.clone();
        definition.sort_order = next_order;
        next_order = next_order
            .checked_add(1)
            .ok_or_else(|| AppError::conflict("导入批次的排序值已达到上限"))?;
        let source = serialize_new_snippet(definition)?;
        total_bytes = total_bytes.saturating_add(source.len());
        if total_bytes > MAX_SNIPPET_TRANSFER_BYTES {
            return Err(AppError::invalid("导入批次超过 32 MiB 限制"));
        }

        prepared_files.push((
            original.id.clone(),
            format!("{tx_id}-{index:03}.toml.stage"),
            source.into_bytes(),
        ));
    }

    let entries = prepared_files
        .iter()
        .map(|(id, staging_file, bytes)| SnippetImportJournalEntry {
            id: id.clone(),
            staging_file: staging_file.clone(),
            content_sha256: sha256_hex(bytes),
        })
        .collect::<Vec<_>>();
    let journal = SnippetImportJournal {
        schema_version: JOURNAL_SCHEMA_VERSION,
        batch_id: request.batch_id.clone(),
        payload_sha256: payload_sha256.to_owned(),
        imported_ids: request
            .definitions
            .iter()
            .map(|definition| definition.id.clone())
            .collect(),
        entries,
    };
    validate_metadata_budget(&journal)?;

    preflight_batch_directories(root)?;
    ensure_plain_directory(&snippets_directory)?;
    ensure_transaction_layout(root)?;
    let staging_directory = transaction_path(root).join(STAGING_DIRECTORY);
    let mut written_entries = Vec::with_capacity(prepared_files.len());
    for (entry, (_, staging_file, bytes)) in journal.entries.iter().zip(prepared_files) {
        if let Err(error) = write_staging_file(root, &staging_file, &bytes) {
            cleanup_staging(root, &written_entries);
            return Err(error);
        }
        written_entries.push(entry.clone());
    }
    if let Err(source) = sync_directory(&staging_directory) {
        cleanup_staging(root, &written_entries);
        return Err(AppError::infrastructure_with_source(
            "同步片段导入暂存目录失败",
            source,
        ));
    }
    Ok(PreparedBatch { journal })
}

fn validate_request(request: &SnippetImportBatchRequest) -> AppResult<()> {
    validate_batch_id(&request.batch_id)?;
    if request.definitions.is_empty() || request.definitions.len() > MAX_SNIPPET_TRANSFER_ITEMS {
        return Err(AppError::invalid("导入片段数量无效"));
    }
    let mut ids = HashSet::with_capacity(request.definitions.len());
    for definition in &request.definitions {
        definition
            .validate()
            .map_err(|_| AppError::invalid("导入片段未通过定义校验"))?;
        if !ids.insert(definition.id.as_str()) {
            return Err(AppError::invalid("导入片段标识重复"));
        }
    }
    Ok(())
}

/// 批次 ID 仅用于幂等比较，路径一律由其 SHA-256 派生而不拼接用户输入。
fn validate_batch_id(batch_id: &str) -> AppResult<()> {
    if batch_id.is_empty()
        || batch_id.chars().count() > 160
        || batch_id
            .chars()
            .any(|value| value.is_control() || matches!(value, '/' | '\\'))
    {
        return Err(AppError::invalid("片段导入批次标识无效"));
    }
    Ok(())
}

/// 使用结构化 DTO 的稳定字段序列计算请求指纹；有界 writer 防止伪造调用耗尽内存。
fn payload_fingerprint(definitions: &[SnippetDefinition]) -> AppResult<String> {
    let mut writer = BoundedWriter::new(MAX_SNIPPET_TRANSFER_BYTES);
    if serde_json::to_writer(&mut writer, definitions).is_err() {
        return Err(AppError::invalid("导入批次超过 32 MiB 限制或无法序列化"));
    }
    Ok(sha256_hex(&writer.bytes))
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
}

impl Write for BoundedWriter {
    /// 仅接受仍处于传输上限内的数据，避免在计算指纹时复制任意大正文。
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > self.limit {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    /// 本 writer 将每次序列化片段追加到同一个有界缓冲区。
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 先将完整事务清单原子落盘，再允许任何目标 TOML 出现。
fn persist_active_journal(root: &Path, journal: &SnippetImportJournal) -> AppResult<RecordCommit> {
    let bytes = serde_json::to_vec(journal)
        .map_err(|_| AppError::infrastructure("序列化片段导入恢复记录失败"))?;
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(AppError::invalid("片段导入恢复记录超过 1 MiB 元数据预算"));
    }
    let path = active_journal_path(root);
    write_new_record(&path, &bytes, MAX_JOURNAL_BYTES)
}

/// 已确认所有目标文件后持久化不可变回执，供同批次请求幂等回读。
fn commit_receipt(root: &Path, journal: &SnippetImportJournal) -> AppResult<RecordCommit> {
    let bytes = serialize_receipt(journal)?;
    if bytes.len() as u64 > MAX_RECEIPT_BYTES {
        return Err(AppError::invalid("片段导入回执超过 256 KiB 元数据预算"));
    }
    write_new_record(
        &receipt_path(root, &journal.batch_id),
        &bytes,
        MAX_RECEIPT_BYTES,
    )
}

/// 恢复元数据使用 CreateNew 提交；未知持久性通过回读精确字节归类。
fn write_new_record(path: &Path, bytes: &[u8], max_bytes: u64) -> AppResult<RecordCommit> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::invalid("片段导入恢复路径无效"))?;
    ensure_plain_directory(parent)?;
    let staging = parent.join(format!(".record-{}.tmp", Uuid::new_v4().simple()));
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)
    {
        Ok(file) => file,
        Err(source) => return Ok(RecordCommit::NotCommitted(source)),
    };
    if let Err(source) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&staging);
        return Ok(RecordCommit::NotCommitted(source));
    }
    drop(file);

    match commit_staged_file(&staging, path, CommitMode::CreateNew) {
        Ok(()) => Ok(RecordCommit::Committed),
        Err(AtomicCommitError::NotCommitted(source)) => {
            let _ = fs::remove_file(&staging);
            if file_matches(path, bytes, max_bytes) {
                Ok(RecordCommit::Committed)
            } else {
                Ok(RecordCommit::NotCommitted(source))
            }
        }
        Err(AtomicCommitError::CommitUnknown(_)) => Ok(if file_matches(path, bytes, max_bytes) {
            RecordCommit::Committed
        } else {
            RecordCommit::Unknown
        }),
    }
}

/// 缺 marker 表示无事务；损坏或未来格式 marker 保留并作为未知状态阻断目录写入。
fn recover_active_journal(
    repository: &FileCommandCatalogRepository,
    allow_recovery_writes: bool,
) -> AppResult<Option<ActiveRecovery>> {
    let root = repository.storage.root();
    if !existing_transaction_directory(root, &[])? {
        return Ok(None);
    }
    let path = active_journal_path(root);
    let journal = match read_json_record::<SnippetImportJournal>(&path, MAX_JOURNAL_BYTES)? {
        StoredRecord::Missing => return Ok(None),
        StoredRecord::Invalid => {
            return Ok(Some(ActiveRecovery {
                batch_id: String::new(),
                payload_sha256: String::new(),
                state: SnippetImportBatchState::OutcomeUnknown,
            }));
        }
        StoredRecord::Valid(journal) => journal,
    };
    if !valid_journal(repository, &journal) {
        return Ok(Some(ActiveRecovery {
            batch_id: journal.batch_id,
            payload_sha256: journal.payload_sha256,
            state: SnippetImportBatchState::OutcomeUnknown,
        }));
    }

    let recovery = recover_journal(repository, &journal, allow_recovery_writes)?;
    Ok(Some(ActiveRecovery {
        batch_id: journal.batch_id,
        payload_sha256: journal.payload_sha256,
        state: recovery,
    }))
}

/// 检查 active marker 是否仍在，确保已知终态但清理未完成时也阻断后续写入。
fn active_journal_exists(root: &Path) -> AppResult<bool> {
    if !existing_transaction_directory(root, &[])? {
        return Ok(false);
    }
    match fs::symlink_metadata(active_journal_path(root)) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_failure("检查片段导入恢复标记失败", error)),
    }
}

/// 根据目标和 staging 的逐项哈希区分已提交、未提交及无法证明的混合状态。
fn recover_journal(
    repository: &FileCommandCatalogRepository,
    journal: &SnippetImportJournal,
    allow_recovery_writes: bool,
) -> AppResult<SnippetImportBatchState> {
    let root = repository.storage.root();
    match read_receipt(root, &journal.batch_id)? {
        StoredRecord::Valid(receipt) => {
            if receipt.payload_sha256 != journal.payload_sha256
                || receipt.receipt.imported_ids != journal.imported_ids
            {
                return Ok(SnippetImportBatchState::OutcomeUnknown);
            }
            if allow_recovery_writes && cleanup_staging(root, &journal.entries) {
                cleanup_active_journal(root);
            }
            return Ok(SnippetImportBatchState::Committed(
                receipt.receipt.into_application(),
            ));
        }
        StoredRecord::Invalid => return Ok(SnippetImportBatchState::OutcomeUnknown),
        StoredRecord::Missing => {}
    }

    let snippets_directory_exists = existing_directory(&root.join("snippets"))?;
    let staging_directory_exists = existing_transaction_directory(root, &[STAGING_DIRECTORY])?;
    let mut all_targets_missing = true;
    let mut all_targets_match = true;
    let mut all_stages_missing = true;
    let mut all_stages_unmodified = true;
    for entry in &journal.entries {
        let target = match repository.document_path("snippets", &entry.id) {
            Ok(path) => path,
            Err(_) => return Ok(SnippetImportBatchState::OutcomeUnknown),
        };
        let target_state = if snippets_directory_exists {
            file_state(&target, &entry.content_sha256)
        } else {
            FileState::Missing
        };
        match target_state {
            FileState::Missing => all_targets_match = false,
            FileState::Matches => all_targets_missing = false,
            FileState::Different => {
                return Ok(SnippetImportBatchState::OutcomeUnknown);
            }
        }

        let stage = staging_path(root, &entry.staging_file);
        let stage_state = if staging_directory_exists {
            file_state(&stage, &entry.content_sha256)
        } else {
            FileState::Missing
        };
        match stage_state {
            FileState::Missing => all_stages_unmodified = false,
            FileState::Matches => {
                all_stages_missing = false;
            }
            FileState::Different => {
                all_stages_missing = false;
                all_stages_unmodified = false;
            }
        }
    }

    if all_targets_missing && all_stages_unmodified {
        if allow_recovery_writes && cleanup_staging(root, &journal.entries) {
            cleanup_active_journal(root);
        }
        return Ok(SnippetImportBatchState::NotCommitted);
    }
    if all_targets_match && all_stages_missing {
        if allow_recovery_writes {
            if !matches!(commit_receipt(root, journal)?, RecordCommit::Committed) {
                return Ok(SnippetImportBatchState::OutcomeUnknown);
            }
            cleanup_active_journal(root);
        }
        return Ok(SnippetImportBatchState::Committed(SnippetImportReceipt {
            batch_id: journal.batch_id.clone(),
            imported_ids: journal.imported_ids.clone(),
        }));
    }
    Ok(SnippetImportBatchState::OutcomeUnknown)
}

/// 校验 journal 中全部受路径影响字段，防止恢复记录构造任意文件路径。
fn valid_journal(
    repository: &FileCommandCatalogRepository,
    journal: &SnippetImportJournal,
) -> bool {
    if journal.schema_version != JOURNAL_SCHEMA_VERSION
        || validate_batch_id(&journal.batch_id).is_err()
        || !valid_sha256(&journal.payload_sha256)
        || journal.entries.is_empty()
        || journal.entries.len() > MAX_SNIPPET_TRANSFER_ITEMS
        || journal.entries.len() != journal.imported_ids.len()
    {
        return false;
    }
    let mut ids = HashSet::with_capacity(journal.entries.len());
    let mut staging_files = HashSet::with_capacity(journal.entries.len());
    for (entry, imported_id) in journal.entries.iter().zip(&journal.imported_ids) {
        if entry.id != *imported_id
            || !safe_document_id(&entry.id)
            || !valid_sha256(&entry.content_sha256)
            || !valid_stage_name(&entry.staging_file)
            || repository.document_path("snippets", &entry.id).is_err()
            || !ids.insert(entry.id.as_str())
            || !staging_files.insert(entry.staging_file.as_str())
        {
            return false;
        }
    }
    true
}

/// 回执目录缺失等价于尚未提交；不安全目录和损坏记录不会被当成空状态覆盖。
fn read_receipt(root: &Path, batch_id: &str) -> AppResult<StoredRecord<SnippetImportReceiptFile>> {
    if !existing_transaction_directory(root, &[RECEIPTS_DIRECTORY])? {
        return Ok(StoredRecord::Missing);
    }
    let record = read_json_record::<SnippetImportReceiptFile>(
        &receipt_path(root, batch_id),
        MAX_RECEIPT_BYTES,
    )?;
    Ok(match record {
        StoredRecord::Valid(receipt)
            if receipt.schema_version == JOURNAL_SCHEMA_VERSION
                && receipt.receipt.batch_id == batch_id
                && valid_sha256(&receipt.payload_sha256)
                && !receipt.receipt.imported_ids.is_empty()
                && receipt.receipt.imported_ids.len() <= MAX_SNIPPET_TRANSFER_ITEMS
                && receipt
                    .receipt
                    .imported_ids
                    .iter()
                    .all(|id| safe_document_id(id)) =>
        {
            StoredRecord::Valid(receipt)
        }
        StoredRecord::Missing => StoredRecord::Missing,
        _ => StoredRecord::Invalid,
    })
}

/// 相同批次只允许相同 payload 重放，payload 不同作为明确冲突拒绝。
fn receipt_state(
    record: SnippetImportReceiptFile,
    batch_id: &str,
    payload_sha256: &str,
) -> AppResult<SnippetImportBatchState> {
    if record.receipt.batch_id != batch_id {
        return Ok(SnippetImportBatchState::OutcomeUnknown);
    }
    if record.payload_sha256 != payload_sha256 {
        return Err(AppError::conflict(
            "片段导入批次标识已用于不同内容，请使用新的批次标识",
        ));
    }
    Ok(SnippetImportBatchState::Committed(
        record.receipt.into_application(),
    ))
}

/// 有界读取私有恢复 JSON，未来字段或损坏数据统一保留为不可解释记录。
fn read_json_record<T: DeserializeOwned>(
    path: &Path,
    max_bytes: u64,
) -> AppResult<StoredRecord<T>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(StoredRecord::Missing),
        Err(error) => return Err(io_failure("读取片段导入恢复记录失败", error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > max_bytes {
        return Ok(StoredRecord::Invalid);
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(StoredRecord::Missing),
        Err(error) => return Err(io_failure("读取片段导入恢复记录失败", error)),
    };
    if bytes.len() as u64 > max_bytes {
        return Ok(StoredRecord::Invalid);
    }
    Ok(match serde_json::from_slice(&bytes) {
        Ok(record) => StoredRecord::Valid(record),
        Err(_) => StoredRecord::Invalid,
    })
}

/// 只对普通目录继续恢复；符号链接或其它文件类型不能作为事务数据根。
fn existing_directory(path: &Path) -> AppResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => Err(
            AppError::outcome_unknown("片段目录路径不安全，保留文件并暂停恢复"),
        ),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_failure("检查片段目录失败", error)),
    }
}

/// 用普通文件和内容指纹判定事务副本，不跟随符号链接或超过导入上限的文件。
fn file_state(path: &Path, expected_sha256: &str) -> FileState {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return FileState::Missing,
        Err(_) => return FileState::Different,
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_SNIPPET_TRANSFER_BYTES as u64
    {
        return FileState::Different;
    }
    match fs::read(path) {
        Ok(bytes) if sha256_hex(&bytes) == expected_sha256 => FileState::Matches,
        _ => FileState::Different,
    }
}

/// 仅在原子提交结果未知时比较受限元数据文件的精确字节。
fn file_matches(path: &Path, expected: &[u8], max_bytes: u64) -> bool {
    read_json_record::<serde_json::Value>(path, max_bytes)
        .ok()
        .is_some_and(|record| match record {
            StoredRecord::Valid(_) => fs::read(path).is_ok_and(|actual| actual == expected),
            StoredRecord::Missing | StoredRecord::Invalid => false,
        })
}

/// 创建本批唯一 staging 文件并同步正文，父目录在整批写入后统一同步。
fn write_staging_file(root: &Path, name: &str, bytes: &[u8]) -> AppResult<()> {
    if !valid_stage_name(name) {
        return Err(AppError::invalid("片段导入暂存文件标识无效"));
    }
    let directory = transaction_path(root).join(STAGING_DIRECTORY);
    let path = directory.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|source| io_failure("创建片段导入暂存文件失败", source))?;
    if let Err(source) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = remove_file_with_parent_sync(&path);
        return Err(io_failure("写入片段导入暂存文件失败", source));
    }
    Ok(())
}

/// 逐级确认事务父目录均为普通目录；少任一级就证明相应文件不存在。
fn existing_transaction_directory(root: &Path, tail: &[&str]) -> AppResult<bool> {
    if !existing_directory(root)? {
        return Ok(false);
    }
    let mut components = vec![root.join(".storage-transactions"), transaction_path(root)];
    components.extend(
        tail.iter()
            .map(|component| transaction_path(root).join(component)),
    );
    for directory in components {
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(AppError::outcome_unknown(
                    "片段导入恢复目录不安全，保留原文件并暂停操作",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(io_failure("检查片段导入恢复目录失败", error)),
        }
    }
    Ok(true)
}

/// 仅在请求全量通过校验后创建导入专属目录，并同步每一级父目录。
fn ensure_transaction_layout(root: &Path) -> AppResult<()> {
    if !existing_directory(root)? {
        return Err(AppError::conflict(
            "配置目录不存在，无法创建片段导入事务目录",
        ));
    }
    ensure_plain_directory(&root.join(".storage-transactions"))?;
    ensure_plain_directory(&transaction_path(root))?;
    ensure_plain_directory(&transaction_path(root).join(STAGING_DIRECTORY))?;
    ensure_plain_directory(&transaction_path(root).join(RECEIPTS_DIRECTORY))
}

/// 在批次创建任何目录前检查所有受影响路径，避免发现坏父节点后留下半套布局。
fn preflight_batch_directories(root: &Path) -> AppResult<()> {
    if !existing_directory(root)? {
        return Err(AppError::conflict("配置目录不存在，无法准备片段导入"));
    }
    let transaction = transaction_path(root);
    for directory in [
        root.join("snippets"),
        root.join(".storage-transactions"),
        transaction.clone(),
        transaction.join(STAGING_DIRECTORY),
        transaction.join(RECEIPTS_DIRECTORY),
    ] {
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(AppError::outcome_unknown(
                    "片段导入目录状态不安全，保留现有路径并暂停写入",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_failure("检查片段导入目录失败", error)),
        }
    }
    Ok(())
}

/// 创建事务目录时拒绝覆盖已存在的链接或普通文件。
fn ensure_plain_directory(path: &Path) -> AppResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => Err(
            AppError::outcome_unknown("片段导入恢复目录状态不安全，保留原路径并暂停写入"),
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path)
                .map_err(|source| io_failure("创建片段导入恢复目录失败", source))?;
            if let Some(parent) = path.parent() {
                sync_directory(parent).map_err(|source| {
                    AppError::infrastructure_with_source("同步片段导入恢复目录失败", source)
                })?;
            }
            Ok(())
        }
        Err(error) => Err(io_failure("检查片段导入恢复目录失败", error)),
    }
}

/// 只有 staging 清理和 marker 删除都确认后，才能将事务归类为未提交。
fn cleanup_uncommitted(root: &Path, journal: &SnippetImportJournal) -> AppResult<()> {
    if !cleanup_staging(root, &journal.entries) {
        return Err(AppError::outcome_unknown(
            "片段导入暂存清理结果未知，恢复记录已保留",
        ));
    }
    if cleanup_active_journal(root) {
        Ok(())
    } else {
        Err(AppError::outcome_unknown(
            "片段导入恢复记录清理结果未知，原记录已保留",
        ))
    }
}

/// 仅删除内容仍与 journal 指纹一致的本批 staging；不同内容保留并阻止同步清理。
fn cleanup_staging(root: &Path, entries: &[SnippetImportJournalEntry]) -> bool {
    let mut complete = true;
    for entry in entries {
        let path = staging_path(root, &entry.staging_file);
        match file_state(&path, &entry.content_sha256) {
            FileState::Missing => {}
            FileState::Matches => match remove_file_with_parent_sync(&path) {
                Ok(()) => {}
                Err(AtomicCommitError::NotCommitted(error))
                    if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => complete = false,
            },
            FileState::Different => complete = false,
        }
    }
    complete
}

/// 只有文件或父目录同步已确认的删除才能结束 journal；未知结果由后续读取继续判定。
fn cleanup_active_journal(root: &Path) -> bool {
    match remove_file_with_parent_sync(&active_journal_path(root)) {
        Ok(()) => true,
        Err(AtomicCommitError::NotCommitted(error)) if error.kind() == io::ErrorKind::NotFound => {
            true
        }
        Err(_) => false,
    }
}

/// 返回 Git 同步明确保护的片段专用事务目录。
fn transaction_path(root: &Path) -> PathBuf {
    root.join(TRANSACTION_DIRECTORY)
}

/// 返回唯一活动批次 marker 路径，不包含任何用户提供的路径分量。
fn active_journal_path(root: &Path) -> PathBuf {
    transaction_path(root).join(ACTIVE_JOURNAL_FILE)
}

/// stage 文件名先经过内部格式校验，调用方不能用批次输入拼路径。
fn staging_path(root: &Path, name: &str) -> PathBuf {
    transaction_path(root).join(STAGING_DIRECTORY).join(name)
}

/// 回执文件名只用批次 ID 的散列，避免用户输入穿越目录。
fn receipt_path(root: &Path, batch_id: &str) -> PathBuf {
    transaction_path(root)
        .join(RECEIPTS_DIRECTORY)
        .join(format!("{}.json", sha256_hex(batch_id.as_bytes())))
}

/// stage 只接受本仓储生成的 ASCII 文件名集合。
fn valid_stage_name(value: &str) -> bool {
    value.ends_with(".toml.stage")
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
}

/// 私有 journal 中的 digest 必须是完整十六进制 SHA-256。
fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// receipt 回读仅接收文件仓储可表达的稳定 ASCII ID。
fn safe_document_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 160
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
#[path = "../../tests/unit/snippet_import.rs"]
mod unit_tests;

/// 统一使用小写十六进制编码，匹配 journal、回执和文件名约定。
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

/// POSIX 同步目录项持久性；Windows 标准库没有对应目录 fsync，保留其既有提交语义。
fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(path).and_then(|directory| directory.sync_all())
    }
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}
