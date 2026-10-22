//! 工作区文件仓储隔离验证首次创建、revision CAS 与提交前取消。
//!
//! @author kongweiguang

use std::{fs, path::PathBuf};

use kerminal_application::{
    AppError, PublicErrorCode,
    workspace::{
        WorkspaceSessionDocument, WorkspaceSessionExpectedRevision, WorkspaceSessionLoad,
        WorkspaceSessionRepository, WorkspaceSessionRevision,
    },
};
use kerminal_infrastructure::{storage::StorageCoordinator, workspace::WorkspaceSessionStore};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

struct TemporaryRoot(PathBuf);

impl TemporaryRoot {
    /// 每个测试独占带随机 ID 的临时目录，避免触碰用户的 `~/.kerminal`。
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("kerminal-workspace-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create isolated workspace root");
        Self(path)
    }
}

impl Drop for TemporaryRoot {
    /// 测试结束仅删除本 fixture 自己创建的随机目录。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 构造真实仓储 adapter，但将所有文件限制在测试自有临时根。
fn store(root: &TemporaryRoot) -> WorkspaceSessionStore {
    WorkspaceSessionStore::new(StorageCoordinator::new(root.0.clone()), true)
}

/// 通过 typed snapshot 改动受 CAS 保护的 owned 字段，不直接拼接数据库或 JSON 写路径。
fn with_selected_machine(
    mut document: WorkspaceSessionDocument,
    machine_id: &str,
) -> WorkspaceSessionDocument {
    let mut snapshot = document.workspace_snapshot().expect("empty v3 snapshot");
    snapshot.selected_machine_id = Some(machine_id.to_owned());
    document
        .update_known_fields(&snapshot)
        .expect("updated v3 snapshot");
    document
}

/// 首次 Missing 保存使用 create-new，读回 revision 与 typed 空/更新 snapshot 保持一致。
#[tokio::test]
async fn missing_revision_creates_and_roundtrips_v3_document() {
    let root = TemporaryRoot::new();
    let store = store(&root);
    assert_eq!(
        store.load(CancellationToken::new()).await.unwrap(),
        WorkspaceSessionLoad::Missing
    );

    let document = with_selected_machine(WorkspaceSessionDocument::empty_v3(), "machine-a");
    let revision = store
        .save(
            CancellationToken::new(),
            &document,
            WorkspaceSessionExpectedRevision::Missing,
        )
        .await
        .unwrap();
    let loaded = store.load(CancellationToken::new()).await.unwrap();

    let WorkspaceSessionLoad::Loaded {
        document: loaded_document,
        revision: loaded_revision,
    } = loaded
    else {
        panic!("saved workspace must load as v3");
    };
    assert_eq!(loaded_revision, revision);
    assert_eq!(
        loaded_document
            .workspace_snapshot()
            .unwrap()
            .selected_machine_id
            .as_deref(),
        Some("machine-a")
    );
}

/// 先提交新 revision 再提交旧 revision，验证冲突且 session 原始字节不变。
///
/// @author kongweiguang
#[tokio::test]
async fn stale_revision_conflicts_without_replacing_latest_document() {
    let root = TemporaryRoot::new();
    let store = store(&root);
    let first = with_selected_machine(WorkspaceSessionDocument::empty_v3(), "machine-a");
    let first_revision = store
        .save(
            CancellationToken::new(),
            &first,
            WorkspaceSessionExpectedRevision::Missing,
        )
        .await
        .unwrap();
    let latest = with_selected_machine(WorkspaceSessionDocument::empty_v3(), "machine-b");
    let latest_revision = store
        .save(
            CancellationToken::new(),
            &latest,
            WorkspaceSessionExpectedRevision::Revision(first_revision.clone()),
        )
        .await
        .unwrap();
    assert_ne!(first_revision.as_str(), latest_revision.as_str());
    let stale_candidate = with_selected_machine(WorkspaceSessionDocument::empty_v3(), "machine-c");
    let session_path = root.0.join("workspace/session.json");
    let latest_bytes = fs::read(&session_path).expect("read latest session bytes");

    let error = store
        .save(
            CancellationToken::new(),
            &stale_candidate,
            WorkspaceSessionExpectedRevision::Revision(first_revision),
        )
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(
        fs::read(&session_path).expect("read session after stale CAS"),
        latest_bytes,
        "rejected stale CAS must preserve the exact committed bytes"
    );

    let WorkspaceSessionLoad::Loaded { document, revision } =
        store.load(CancellationToken::new()).await.unwrap()
    else {
        panic!("latest workspace must remain readable");
    };
    assert_eq!(revision, latest_revision);
    assert_eq!(
        document
            .workspace_snapshot()
            .unwrap()
            .selected_machine_id
            .as_deref(),
        Some("machine-b")
    );
}

/// Missing CAS 在目标文件已经存在时必须拒绝写入，不能退化成无条件替换。
#[tokio::test]
async fn missing_revision_never_clobbers_an_existing_session() {
    let root = TemporaryRoot::new();
    let store = store(&root);
    let first = with_selected_machine(WorkspaceSessionDocument::empty_v3(), "machine-a");
    store
        .save(
            CancellationToken::new(),
            &first,
            WorkspaceSessionExpectedRevision::Missing,
        )
        .await
        .unwrap();
    let replacement = with_selected_machine(WorkspaceSessionDocument::empty_v3(), "machine-b");

    let error = store
        .save(
            CancellationToken::new(),
            &replacement,
            WorkspaceSessionExpectedRevision::Missing,
        )
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);

    let WorkspaceSessionLoad::Loaded { document, .. } =
        store.load(CancellationToken::new()).await.unwrap()
    else {
        panic!("existing workspace must remain readable");
    };
    assert_eq!(
        document
            .workspace_snapshot()
            .unwrap()
            .selected_machine_id
            .as_deref(),
        Some("machine-a")
    );
}

/// 写入提交前取消不创建 session 文件，后续 load 仍精确返回 Missing。
#[tokio::test]
async fn cancellation_before_save_keeps_session_missing() {
    let root = TemporaryRoot::new();
    let store = store(&root);
    let document = with_selected_machine(WorkspaceSessionDocument::empty_v3(), "machine-a");
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let result = store
        .save(
            cancellation,
            &document,
            WorkspaceSessionExpectedRevision::Missing,
        )
        .await;
    assert!(matches!(result, Err(AppError::Cancelled { .. })));
    assert_eq!(
        store.load(CancellationToken::new()).await.unwrap(),
        WorkspaceSessionLoad::Missing
    );
}

/// 0.4 运行态只接受 v3；旧版与未来版读取为不支持，保存不能覆盖原文或改动用户副本。
#[tokio::test]
async fn non_v3_versions_are_read_only_and_preserve_user_files() {
    for version in [1_u64, 2, 4] {
        let root = TemporaryRoot::new();
        let session_path = root.0.join("workspace/session.json");
        fs::create_dir_all(session_path.parent().unwrap()).expect("create workspace directory");
        let session_bytes = format!(r#"{{"version":{version},"tabs":[]}}"#).into_bytes();
        fs::write(&session_path, &session_bytes).expect("write isolated session fixture");
        let backup_path = root.0.join("workspace/session.pre-v3.json");
        let user_backup: Option<&[u8]> = if version == 1 {
            Some(b"user-owned recovery data")
        } else {
            None
        };
        if let Some(bytes) = user_backup {
            fs::write(&backup_path, bytes).expect("write existing user-owned recovery file");
        }
        let store = store(&root);

        let loaded = store.load(CancellationToken::new()).await.unwrap();
        let WorkspaceSessionLoad::Unsupported {
            version: loaded_version,
        } = loaded
        else {
            panic!("non-current session version must be classified as unsupported");
        };
        assert_eq!(loaded_version, version);
        let revision = Sha256::digest(&session_bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let error = store
            .save(
                CancellationToken::new(),
                &WorkspaceSessionDocument::empty_v3(),
                WorkspaceSessionExpectedRevision::Revision(WorkspaceSessionRevision::new(revision)),
            )
            .await
            .unwrap_err();
        assert_eq!(error.public_code(), PublicErrorCode::Unsupported);
        assert_eq!(
            fs::read(&session_path).unwrap().as_slice(),
            session_bytes.as_slice()
        );
        match user_backup {
            Some(bytes) => assert_eq!(fs::read(&backup_path).unwrap(), bytes),
            None => assert!(
                !backup_path.exists(),
                "runtime must not create a legacy backup"
            ),
        }
    }
}

/// 无版本号的文件属于无效当前 schema；Missing CAS 也不能把它替换为空 v3 布局。
#[tokio::test]
async fn session_without_version_is_invalid_and_never_clobbered() {
    let root = TemporaryRoot::new();
    let session_path = root.0.join("workspace/session.json");
    fs::create_dir_all(session_path.parent().unwrap()).expect("create workspace directory");
    let session_bytes = br#"{"tabs":[]}"#.to_vec();
    fs::write(&session_path, &session_bytes).expect("write missing-version fixture");
    let store = store(&root);

    assert_eq!(
        store.load(CancellationToken::new()).await.unwrap(),
        WorkspaceSessionLoad::Invalid {
            reason: kerminal_application::workspace::WorkspaceSessionInvalidReason::InvalidSchema,
        }
    );
    let revision = Sha256::digest(&session_bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let error = store
        .save(
            CancellationToken::new(),
            &WorkspaceSessionDocument::empty_v3(),
            WorkspaceSessionExpectedRevision::Revision(WorkspaceSessionRevision::new(revision)),
        )
        .await
        .unwrap_err();

    assert_eq!(error.public_code(), PublicErrorCode::ConfigurationCorrupt);
    assert_eq!(fs::read(&session_path).unwrap(), session_bytes);
    assert!(
        !root.0.join("workspace/session.pre-v3.json").exists(),
        "invalid data must not create a legacy backup"
    );
}
