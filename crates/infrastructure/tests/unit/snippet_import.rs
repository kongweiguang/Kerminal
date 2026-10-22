//! 片段导入 journal 的清理、容量和路径恢复边界测试。
//!
//! @author kongweiguang

use std::{fs, path::PathBuf};

use kerminal_application::{
    AppError,
    commands::{MAX_SNIPPET_TRANSFER_ITEMS, SnippetImportBatchRequest},
};
use kerminal_domain::commands::{
    SnippetDefaultAction, SnippetDefinition, SnippetRisk, SnippetScope,
};
use uuid::Uuid;

use super::{
    JOURNAL_SCHEMA_VERSION, MAX_JOURNAL_BYTES, MAX_RECEIPT_BYTES, SnippetImportJournal,
    SnippetImportJournalEntry, active_journal_path, commit_receipt, ensure_transaction_layout,
    persist_active_journal, preflight_batch_directories, validate_metadata_budget,
};
use crate::commands::FileCommandCatalogRepository;
use crate::storage::StorageCoordinator;

struct TemporaryRoot(PathBuf);

impl TemporaryRoot {
    /// 每个测试使用新建根目录，避免接触用户配置和其他事务记录。
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("snippet-import-unit-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("创建隔离片段根目录");
        Self(root)
    }
}

impl Drop for TemporaryRoot {
    /// 仅清理本测试创建的 UUID 目录；依靠栈逆序释放 marker 句柄后再回收隔离根目录。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 最长合法 ID 下 journal 和完整回执仍在各自明确预算内。
#[test]
fn max_batch_metadata_fits_declared_caps() {
    let ids = (0..MAX_SNIPPET_TRANSFER_ITEMS)
        .map(|index| format!("{index:04}{}", "a".repeat(156)))
        .collect::<Vec<_>>();
    let journal = SnippetImportJournal {
        schema_version: JOURNAL_SCHEMA_VERSION,
        batch_id: "b".repeat(160),
        payload_sha256: "0".repeat(64),
        imported_ids: ids.clone(),
        entries: ids
            .iter()
            .enumerate()
            .map(|(index, id)| SnippetImportJournalEntry {
                id: id.clone(),
                staging_file: format!("batch-{index:04}.toml.stage"),
                content_sha256: "1".repeat(64),
            })
            .collect(),
    };

    validate_metadata_budget(&journal).expect("完整批次元数据满足持久化预算");
    let journal_size = serde_json::to_vec(&journal).expect("序列化 journal").len() as u64;
    let receipt_size = super::serialize_receipt(&journal)
        .expect("序列化完整回执")
        .len() as u64;
    assert!(journal_size <= MAX_JOURNAL_BYTES);
    assert!(receipt_size <= MAX_RECEIPT_BYTES);
}

/// Windows 删除只读属性可被 Rust 标准库忽略，因此用保留读写共享、拒绝 DELETE 共享的句柄，验证清理失败时写门禁仍关闭，释放后只读恢复而不重放定义。
#[cfg(windows)]
#[tokio::test]
async fn committed_marker_cleanup_failure_blocks_catalog_writes() {
    use kerminal_application::commands::{SnippetImportBatchState, SnippetRepository};
    use std::os::windows::fs::OpenOptionsExt;

    let temporary = TemporaryRoot::new();
    let root = &temporary.0;
    ensure_transaction_layout(root).expect("初始化事务目录");
    let repository = FileCommandCatalogRepository::new(StorageCoordinator::new(root), true);
    let imported_definition = SnippetDefinition {
        id: "snippet-cleanup".to_owned(),
        title: "已提交片段".to_owned(),
        command: "printf committed".to_owned(),
        description: None,
        tags: Vec::new(),
        sort_order: 0,
        scope: SnippetScope::Any,
        category: None,
        risk: SnippetRisk::Inspect,
        default_action: SnippetDefaultAction::Insert,
        variables: Vec::new(),
        context_bindings: Vec::new(),
        derived_from: None,
    };
    repository
        .save_user(imported_definition.clone())
        .await
        .expect("准备已经提交的目录定义");
    let imported_path = root.join("snippets").join("snippet-cleanup.toml");
    let imported_bytes = fs::read(&imported_path).expect("读取已提交定义");
    let request = SnippetImportBatchRequest {
        batch_id: "cleanup-retry".to_owned(),
        definitions: vec![imported_definition],
    };
    let journal = SnippetImportJournal {
        schema_version: JOURNAL_SCHEMA_VERSION,
        batch_id: request.batch_id.clone(),
        payload_sha256: super::payload_fingerprint(&request.definitions).expect("计算提交内容指纹"),
        imported_ids: vec!["snippet-cleanup".to_owned()],
        entries: vec![SnippetImportJournalEntry {
            id: "snippet-cleanup".to_owned(),
            staging_file: "cleanup-000.toml.stage".to_owned(),
            content_sha256: super::sha256_hex(&imported_bytes),
        }],
    };
    assert!(matches!(
        commit_receipt(root, &journal).expect("写入 durable receipt"),
        super::RecordCommit::Committed
    ));
    assert!(matches!(
        persist_active_journal(root, &journal).expect("写入 active marker"),
        super::RecordCommit::Committed
    ));

    let marker = active_journal_path(root);
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    let delete_blocker = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&marker)
        .expect("持有允许读写共享但拒绝 DELETE 共享的 marker 句柄");

    let gate_result = {
        let _lock = repository.lock().expect("取得共享存储锁");
        repository.ensure_import_journal_resolved()
    };
    assert!(matches!(gate_result, Err(AppError::OutcomeUnknown { .. })));
    assert!(marker.exists(), "清理失败时 marker 必须保留");

    let blocked_definition = SnippetDefinition {
        id: "blocked-write".to_owned(),
        title: "不得写入".to_owned(),
        command: "printf blocked".to_owned(),
        description: None,
        tags: Vec::new(),
        sort_order: 0,
        scope: SnippetScope::Any,
        category: None,
        risk: SnippetRisk::Inspect,
        default_action: SnippetDefaultAction::Insert,
        variables: Vec::new(),
        context_bindings: Vec::new(),
        derived_from: None,
    };
    assert!(matches!(
        repository.save_user(blocked_definition).await,
        Err(AppError::OutcomeUnknown { .. })
    ));
    assert!(
        !root.join("snippets").join("blocked-write.toml").exists(),
        "恢复 marker 未清理时 catalog 写入必须被门禁拒绝"
    );
    assert_eq!(
        fs::read(&imported_path).expect("回读已提交定义"),
        imported_bytes
    );

    drop(delete_blocker);
    assert!(matches!(
        repository
            .import_batch_status_sync(request.batch_id.clone())
            .expect("解除删除共享后只恢复事务状态"),
        SnippetImportBatchState::Committed(ref receipt)
            if receipt.batch_id == "cleanup-retry"
                && receipt.imported_ids == vec!["snippet-cleanup".to_owned()]
    ));
    assert!(!marker.exists(), "恢复完成后应清理 marker");
    assert_eq!(
        fs::read(&imported_path).expect("确认恢复未重写定义"),
        imported_bytes
    );

    assert!(matches!(
        repository
            .commit_import_batch_sync(request)
            .expect("同一批次重试只回读既有回执"),
        SnippetImportBatchState::Committed(ref receipt)
            if receipt.batch_id == "cleanup-retry"
                && receipt.imported_ids == vec!["snippet-cleanup".to_owned()]
    ));
    assert_eq!(
        fs::read(&imported_path).expect("确认幂等重试未重写定义"),
        imported_bytes
    );
}

/// 非普通事务父目录被保留并以未知状态拒绝，不把它当作不存在继续建文件。
#[test]
fn non_directory_transaction_parent_is_preserved() {
    let temporary = TemporaryRoot::new();
    let protected_parent = temporary.0.join(".storage-transactions");
    let original = b"user-owned file";
    fs::write(&protected_parent, original).expect("写入合成非目录父节点");
    let repository =
        FileCommandCatalogRepository::new(StorageCoordinator::new(temporary.0.clone()), true);

    assert!(matches!(
        super::recover_active_journal(&repository, true),
        Err(AppError::OutcomeUnknown { .. })
    ));
    assert_eq!(fs::read(protected_parent).expect("回读原节点"), original);
}

/// 导入开始前发现非目录事务父节点时不创建 snippets 或替换用户节点。
#[test]
fn unsafe_parent_preflight_has_no_directory_side_effects() {
    let temporary = TemporaryRoot::new();
    let protected_parent = temporary.0.join(".storage-transactions");
    let original = b"user-owned transaction path";
    fs::write(&protected_parent, original).expect("写入合成非目录父节点");

    assert!(matches!(
        preflight_batch_directories(&temporary.0),
        Err(AppError::OutcomeUnknown { .. })
    ));
    assert!(!temporary.0.join("snippets").exists());
    assert_eq!(fs::read(protected_parent).expect("回读原节点"), original);
}

/// 1001 个定义必须在准备阶段整批拒绝，不创建目录或 staging 文件。
#[test]
fn batch_over_item_budget_is_rejected_before_staging() {
    let definitions = (0..MAX_SNIPPET_TRANSFER_ITEMS + 1)
        .map(|index| SnippetDefinition {
            id: format!("reject-{index:04}"),
            title: format!("Fixture {index}"),
            command: "printf fixture".to_owned(),
            description: None,
            tags: Vec::new(),
            sort_order: index as i64,
            scope: SnippetScope::Any,
            category: None,
            risk: SnippetRisk::Inspect,
            default_action: SnippetDefaultAction::Insert,
            variables: Vec::new(),
            context_bindings: Vec::new(),
            derived_from: None,
        })
        .collect();
    let request = SnippetImportBatchRequest {
        batch_id: "too-many".to_owned(),
        definitions,
    };
    let temporary = TemporaryRoot::new();
    let repository =
        FileCommandCatalogRepository::new(StorageCoordinator::new(temporary.0.clone()), true);

    assert!(repository.commit_import_batch_sync(request).is_err());
    assert!(!temporary.0.join("snippets").exists());
    assert!(!temporary.0.join(".storage-transactions").exists());
}
