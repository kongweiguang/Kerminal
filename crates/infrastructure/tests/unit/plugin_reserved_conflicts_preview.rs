//! 保留身份只读预览的隔离回归。
//!
//! 所有 fixture 都建在一次性临时目录中，不读取或修改真实用户配置；测试不执行
//! 任何清理/隔离动作，只验证预览证据、阻断原因和数据不变。
//!
//! 本模块目前不在编译图中（`plugins/mod.rs` 尚未声明 `quarantine`），因此这些
//! 测试尚未编译或运行；本文件只做静态审阅与 rustfmt 解析。
//!
//! @author kongweiguang

use super::*;
// 显式引入父模块的私有导入，避免依赖 glob 的可见性细节，也便于单独阅读。
use crate::{
    plugins::{
        layout::{self, LayoutError},
        store::PluginStore,
    },
    storage::StorageCoordinator,
};
use kerminal_domain::plugins::{
    InstalledPlugin, PendingPluginChange, PluginFingerprint, PluginId, PluginInstallStatus,
    PluginSource, PluginStoreState, PluginUnsupportedReason, PluginVersion,
};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// 构造一份只读记录；installed_at 只是字段占位，预览不会解析时间。
fn installed(fingerprint: Option<PluginFingerprint>) -> InstalledPlugin {
    InstalledPlugin {
        active: PluginVersion::parse("1.0.0").unwrap(),
        previous: None,
        enabled: true,
        source: PluginSource::Local,
        installed_at: "2026-01-01T00:00:00Z".to_owned(),
        fingerprint,
        status: PluginInstallStatus::Ready,
        confirmed: true,
    }
}

/// 只放入目标记录，revision 由调用方指定，确保预览绑定同一快照。
fn state_with_record(
    id: &PluginId,
    fingerprint: Option<PluginFingerprint>,
    revision: u64,
) -> PluginStoreState {
    let mut state = PluginStoreState {
        revision,
        ..PluginStoreState::default()
    };
    // 不校验返回值：测试只准备单条记录，重复插入本应不可达。
    let _ = state.plugins.insert(id.clone(), installed(fingerprint));
    state
}

/// 存储层只允许缺失/不支持记录缺省指纹；这里构造一个合法的“已登记无指纹”旧记录。
fn state_with_unsupported_record(id: &PluginId, revision: u64) -> PluginStoreState {
    let mut state = empty_state(revision);
    let mut record = installed(None);
    record.status = PluginInstallStatus::Unsupported {
        reason: PluginUnsupportedReason::ReservedIdentity,
    };
    record.enabled = false;
    let _ = state.plugins.insert(id.clone(), record);
    state
}

/// 空状态只带 revision，用来证明“无记录”与“有记录”走不同分类。
fn empty_state(revision: u64) -> PluginStoreState {
    PluginStoreState {
        revision,
        ..PluginStoreState::default()
    }
}

/// 在 `plugins/<name>` 下创建真实普通文件，并使用布局模块的同一摘要算法。
fn create_package(root: &Path, name: &str) -> PluginFingerprint {
    let directory = root.join("plugins").join(name);
    fs::create_dir_all(directory.join("assets")).unwrap();
    fs::write(directory.join("manifest.json"), br#"{"schema":1}"#).unwrap();
    fs::write(directory.join("assets/view.json"), br#"{"view":1}"#).unwrap();
    layout::fingerprint_tree(&directory).unwrap()
}

/// 与任何实际目录都不同的合法摘要，用于“已登记但指纹不符”场景。
fn other_fingerprint() -> PluginFingerprint {
    PluginFingerprint::parse(format!("v1:{}", "0".repeat(64))).unwrap()
}

/// 在隔离根上建立权威 store，并写入指定状态；这是唯一允许“根 + 状态”组合的入口。
fn store_with_state(root: &Path, state: &PluginStoreState) -> PluginStore {
    fs::create_dir_all(root.join("plugins")).unwrap();
    fs::write(
        root.join("plugins/state.json"),
        serde_json::to_vec_pretty(state).unwrap(),
    )
    .unwrap();
    PluginStore::new(StorageCoordinator::new(root)).unwrap()
}

fn preview_for(store: &PluginStore, state: &PluginStoreState) -> ReservedConflictPreview {
    preview_named(store, state, "kerminal.core")
}

fn preview_named(
    store: &PluginStore,
    state: &PluginStoreState,
    name: &str,
) -> ReservedConflictPreview {
    preview_reserved_conflict(store, state.revision, name).unwrap()
}

/// 递归列出相对路径，用于证明预览没有创建目录或改写文件。
fn relative_entries(root: &Path) -> Vec<PathBuf> {
    fn walk(base: &Path, current: &Path, entries: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(current).unwrap() {
            let path = entry.unwrap().path();
            entries.push(path.strip_prefix(base).unwrap().to_path_buf());
            if path.is_dir() {
                walk(base, &path, entries);
            }
        }
    }
    let mut entries = Vec::new();
    walk(root, root, &mut entries);
    entries.sort();
    entries
}

#[test]
/// provider 只能按原始精确保留名接受，内置单元才映射为 PluginId。
fn reserved_identity_accepts_only_known_builtin_units_and_exact_provider() {
    let provider = ReservedPluginIdentity::parse("kerminal").unwrap();
    assert_eq!(provider.raw_name(), "kerminal");
    assert!(provider.plugin_id().is_none());
    assert!(is_reserved_plugin_id(provider.raw_name()));
    assert!(PluginId::parse("kerminal").is_err());

    for raw in ["kerminal.core", "kerminal.ssh", "kerminal.logs"] {
        let identity = ReservedPluginIdentity::parse(raw).unwrap();
        assert_eq!(identity.raw_name(), raw);
        assert_eq!(identity.plugin_id().map(PluginId::as_str), Some(raw));
        assert!(is_reserved_plugin_id(raw));
    }

    // 目标名派生必须落在 `plugins/` 的直接子级，不能拼出第二层路径。
    let plugin_root = Path::new("/fixture/plugins");
    let unit = ReservedPluginIdentity::parse("kerminal.core").unwrap();
    assert_eq!(
        unit.directory_path(plugin_root).unwrap(),
        plugin_root.join("kerminal.core")
    );
    let provider = ReservedPluginIdentity::parse("kerminal").unwrap();
    assert_eq!(
        provider.directory_path(plugin_root).unwrap(),
        plugin_root.join("kerminal")
    );

    for raw in [
        "acme.notes",
        "KERMINAL.CORE",
        "kerminal.",
        "../kerminal.core",
        "kerminal.core/../data",
        "kerminal\\core",
        "",
    ] {
        assert_eq!(
            ReservedPluginIdentity::parse(raw),
            Err(ReservedPreviewError::NotReservedIdentity),
            "{raw}"
        );
    }
}

#[test]
/// 单入口只接受 store 自己的根；两个隔离根的状态不能互相冒充。
fn preview_derives_state_from_store_root_and_rejects_cross_root_mismatch() {
    let root_a = tempfile::tempdir().unwrap();
    let root_b = tempfile::tempdir().unwrap();
    let id = PluginId::parse("kerminal.core").unwrap();
    let actual = create_package(root_a.path(), id.as_str());
    let state_a = state_with_record(&id, Some(actual.clone()), 7);
    let store_a = store_with_state(root_a.path(), &state_a);
    // B 根没有任何插件目录，也不含 A 的记录；其状态故意给出同样 revision。
    let store_b = store_with_state(root_b.path(), &empty_state(7));

    let from_a = preview_for(&store_a, &state_a);
    assert_eq!(
        from_a.classification,
        ReservedConflictClassification::RegisteredFingerprintMatch
    );

    // 用 A 的期望 revision 读 B 的根：状态来自 B，因此 B 根下的目标目录必须缺失，
    // 不能借用 A 的目录证据。
    let from_b = preview_reserved_conflict(&store_b, 7, "kerminal.core").unwrap();
    assert_eq!(
        from_b.classification,
        ReservedConflictClassification::UnrecordedDirectoryMissing
    );
    assert_eq!(from_b.observed_fingerprint(), None);
    assert_ne!(from_b.evidence_digest, from_a.evidence_digest);
}

#[test]
/// 证据摘要不能冒充授权或插件指纹：内容变化会改摘要，且格式与 `PluginFingerprint` 不同。
fn evidence_digest_is_stale_detection_not_authorization() {
    let root = tempfile::tempdir().unwrap();
    let id = PluginId::parse("kerminal.core").unwrap();
    create_package(root.path(), id.as_str());
    let state = empty_state(8);
    let store = store_with_state(root.path(), &state);

    let first = preview_for(&store, &state);
    assert!(!first.evidence_digest.is_empty());
    assert!(
        PluginFingerprint::parse(first.evidence_digest.clone()).is_err(),
        "证据摘要不能被当成插件指纹"
    );
    assert!(!production_readiness().grants_authorization);

    // 内容变化但 revision 不变时摘要必须变化；这只能说明快照陈旧，不是授权。
    fs::write(root.path().join("plugins/kerminal.core/extra.json"), b"{}").unwrap();
    let second = preview_for(&store, &state);
    assert_eq!(second.revision, first.revision);
    assert_ne!(second.evidence_digest, first.evidence_digest);
}

#[test]
/// 状态加载失败、旧 schema 和损坏文件都必须显式失败，不能退化成空状态。
fn state_failures_are_typed_and_never_default_to_empty() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.join("plugins")).unwrap();
    fs::write(root.join("plugins/state.json"), b"{ not json").unwrap();
    let store = PluginStore::new(StorageCoordinator::new(root.path())).unwrap();
    assert_eq!(
        preview_reserved_conflict(&store, 0, "kerminal.core"),
        Err(ReservedPreviewError::StateInvalid)
    );

    // 旧 schema 属于“不受支持”，同样不能把旧材料当成空状态丢掉。
    let mut legacy = empty_state(3);
    legacy.schema = 1;
    fs::write(
        root.join("plugins/state.json"),
        serde_json::to_vec_pretty(&legacy).unwrap(),
    )
    .unwrap();
    assert_eq!(
        preview_reserved_conflict(&store, 3, "kerminal.core"),
        Err(ReservedPreviewError::StateInvalid)
    );

    // 缺失状态文件是存储层定义的“首次启动”，预览按空状态处理但不创建任何文件。
    let empty_root = tempfile::tempdir().unwrap();
    let empty_store = PluginStore::new(StorageCoordinator::new(empty_root.path())).unwrap();
    let preview = preview_reserved_conflict(&empty_store, 0, "kerminal.core").unwrap();
    assert_eq!(
        preview.classification,
        ReservedConflictClassification::UnrecordedDirectoryMissing
    );
    assert!(!empty_root.path().join("plugins").exists());
}

#[test]
/// 期望 revision 落后时返回可比较的陈旧错误，而不是悄悄使用最新状态。
fn stale_expected_revision_is_reported_with_both_values() {
    let root = tempfile::tempdir().unwrap();
    let store = store_with_state(root.path(), &empty_state(9));
    assert_eq!(
        preview_reserved_conflict(&store, 8, "kerminal.core"),
        Err(ReservedPreviewError::StaleRevision {
            expected: 8,
            actual: 9,
        })
    );
    let preview = preview_reserved_conflict(&store, 9, "kerminal.core").unwrap();
    assert_eq!(preview.revision, 9);
}

#[test]
/// 已登记记录的匹配、无指纹和指纹不符必须来自实际目录，而不是状态字段。
fn registered_directory_evidence_distinguishes_match_missing_and_fingerprint_states() {
    let root = tempfile::tempdir().unwrap();
    let id = PluginId::parse("kerminal.core").unwrap();
    let actual = create_package(root.path(), id.as_str());

    let matched_state = state_with_record(&id, Some(actual.clone()), 41);
    let matched = preview_for(
        &store_with_state(root.path(), &matched_state),
        &matched_state,
    );
    assert_eq!(
        matched.classification,
        ReservedConflictClassification::RegisteredFingerprintMatch
    );
    assert_eq!(matched.observed_fingerprint(), Some(&actual));
    assert_eq!(matched.recorded_fingerprint(), Some(&actual));
    assert_eq!(
        matched.suggested_cleanup,
        PreviewCleanupSuggestion::RemoveExternalRecordAndQuarantineDirectory
    );

    let different = other_fingerprint();
    let mismatched_state = state_with_record(&id, Some(different.clone()), 42);
    let mismatched = preview_for(
        &store_with_state(root.path(), &mismatched_state),
        &mismatched_state,
    );
    assert_eq!(
        mismatched.classification,
        ReservedConflictClassification::RegisteredFingerprintMismatch
    );
    assert_eq!(mismatched.observed_fingerprint(), Some(&actual));
    assert_eq!(mismatched.recorded_fingerprint(), Some(&different));

    let unproven_state = state_with_unsupported_record(&id, 43);
    let unproven = preview_for(
        &store_with_state(root.path(), &unproven_state),
        &unproven_state,
    );
    assert_eq!(
        unproven.classification,
        ReservedConflictClassification::RegisteredWithoutFingerprint
    );
    assert_eq!(unproven.recorded_fingerprint(), None);
    // 记录无指纹时实际目录无法与安装内容建立身份关联，清理建议必须保守阻断，
    // 不能建议移动一个无法证明来源的目录。
    assert!(
        unproven
            .blockers
            .contains(&PreviewBlocker::RecordIdentityUnverifiable)
    );
    assert!(unproven.is_blocked());
    assert_eq!(
        unproven.suggested_cleanup,
        PreviewCleanupSuggestion::Blocked
    );
}

#[test]
/// 已登记缺失、无记录缺失和无记录安全目录是三种不同证据。
fn missing_registered_and_unrecorded_directories_are_distinct() {
    let root = tempfile::tempdir().unwrap();
    let id = PluginId::parse("kerminal.core").unwrap();

    let registered_state = state_with_record(&id, Some(other_fingerprint()), 51);
    let registered = preview_for(
        &store_with_state(root.path(), &registered_state),
        &registered_state,
    );
    assert_eq!(
        registered.classification,
        ReservedConflictClassification::RegisteredDirectoryMissing
    );
    assert_eq!(
        registered.suggested_cleanup,
        PreviewCleanupSuggestion::RemoveStaleExternalRecord
    );

    let unrecorded_state = empty_state(52);
    let unrecorded = preview_for(
        &store_with_state(root.path(), &unrecorded_state),
        &unrecorded_state,
    );
    assert_eq!(
        unrecorded.classification,
        ReservedConflictClassification::UnrecordedDirectoryMissing
    );
    assert_eq!(unrecorded.suggested_cleanup, PreviewCleanupSuggestion::None);

    create_package(root.path(), id.as_str());
    let safe_state = empty_state(53);
    let safe = preview_for(&store_with_state(root.path(), &safe_state), &safe_state);
    assert_eq!(
        safe.classification,
        ReservedConflictClassification::UnrecordedDirectorySafe
    );
    assert_eq!(
        safe.suggested_cleanup,
        PreviewCleanupSuggestion::QuarantineDirectory
    );
}

#[test]
/// 目标不是安全目录时必须显式阻断，不能当成空目录或默认成功。
fn unsafe_target_is_classified_and_blocked() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("plugins")).unwrap();
    fs::write(
        root.path().join("plugins/kerminal.core"),
        b"not a directory",
    )
    .unwrap();
    let state = empty_state(61);
    let store = store_with_state(root.path(), &state);

    let preview = preview_for(&store, &state);
    assert_eq!(
        preview.classification,
        ReservedConflictClassification::UnsafeOrUnreadable
    );
    assert!(matches!(
        &preview.directory,
        ReservedDirectoryEvidence::UnsafeOrUnreadable {
            reason: DirectoryInspectionFailure::NotDirectory
        }
    ));
    assert!(
        preview
            .blockers
            .contains(&PreviewBlocker::DirectoryUnsafeOrUnreadable)
    );
    assert_eq!(preview.suggested_cleanup, PreviewCleanupSuggestion::Blocked);
}

#[test]
/// `plugins/` 根自身不是安全目录时公开入口直接报专用错误，不下探子路径。
fn unsafe_plugin_root_is_reported_by_public_entry_without_reading_state() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("plugins"), b"not a directory").unwrap();
    let id = PluginId::parse("kerminal.core").unwrap();
    // 根不安全时状态文件同样无法按安全路径读取；存储层会把该情况折叠成
    // StateInvalid，公开入口必须先行核对根并返回专用错误，不冒充状态损坏。
    let state = state_with_record(&id, Some(other_fingerprint()), 66);
    let store = PluginStore::new(StorageCoordinator::new(root.path())).unwrap();

    assert_eq!(
        preview_reserved_conflict(&store, state.revision, "kerminal.core"),
        Err(ReservedPreviewError::PluginRootUnsafe)
    );
    // 根不安全，不应在任何位置创建锁文件或目录。
    assert!(!root.path().join(".storage.lock").exists());
}

#[test]
/// 双重检查之间的根替换（TOCTOU 窗口）仍由纯计算入口兜底：不下探、保持未核对。
fn unsafe_root_during_second_check_stays_blocked_and_unverified() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("plugins"), b"not a directory").unwrap();
    // 根不安全时状态文件也无法按安全路径读取；直接验证纯计算分支。
    let state = empty_state(67);

    let preview = preview_from_state(root.path(), &state, "kerminal.core").unwrap();
    assert_eq!(
        preview.classification,
        ReservedConflictClassification::UnsafeOrUnreadable
    );
    assert!(matches!(
        &preview.directory,
        ReservedDirectoryEvidence::UnsafeOrUnreadable {
            reason: DirectoryInspectionFailure::ParentDirectoryUnsafe
        }
    ));
    // 无法枚举子项时，所有恢复维度都必须保持“未核对”，不能当成没有材料。
    assert!(preview.recovery.state_recovery_unverified);
    assert!(preview.recovery.layout_journal_unverified);
    assert!(preview.recovery.staging_unverified);
    assert!(preview.recovery.backup_unverified);
    for blocker in [
        PreviewBlocker::DirectoryUnsafeOrUnreadable,
        PreviewBlocker::StateRecoveryUnverified,
        PreviewBlocker::LayoutJournalUnverified,
        PreviewBlocker::StagingUnverified,
        PreviewBlocker::BackupUnverified,
    ] {
        assert!(preview.blockers.contains(&blocker), "{blocker:?}");
    }
    assert_eq!(preview.suggested_cleanup, PreviewCleanupSuggestion::Blocked);
}

#[test]
/// 外部记录、内置启停状态和用户数据分别保留，预览只读且不凭 ID 改写另一套状态。
fn builtin_toggle_and_external_record_remain_independent() {
    let root = tempfile::tempdir().unwrap();
    // 用可停用的内置单元：core 被存储层校验为不可停用，无法成为合法持久状态。
    let id = PluginId::parse("kerminal.ssh").unwrap();
    let actual = create_package(root.path(), id.as_str());
    let mut state = state_with_record(&id, Some(actual), 71);
    let _ = state.builtins.insert(id.clone(), false);

    let data = root.path().join("plugins/data/kerminal.ssh");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("private.txt"), b"user data").unwrap();
    let before_state = state.clone();
    // 必须在完整 fixture 初始化之后采样：store_with_state 会写入 plugins/state.json，
    // 之前采样会把这次写入误当成预览造成的改写，断言必然失败。
    let store = store_with_state(root.path(), &state);
    let before_entries = relative_entries(root.path());

    let preview = preview_named(&store, &state, "kerminal.ssh");
    assert_eq!(
        preview.classification,
        ReservedConflictClassification::RegisteredFingerprintMatch
    );
    assert!(preview.external_record.as_ref().unwrap().enabled);
    assert_eq!(
        preview.builtin_toggle,
        BuiltinToggleEvidence::ExplicitDisabled
    );

    assert_eq!(state, before_state);
    assert_eq!(
        fs::read(data.join("private.txt")).unwrap(),
        b"user data".to_vec()
    );
    assert_eq!(relative_entries(root.path()), before_entries);
    assert!(!root.path().join("plugins/.migration").exists());
    assert!(!root.path().join("plugins/.staging").exists());
    // 预览不取存储写锁：`lock()` 会创建根目录并写锁文件，这里必须保持未创建。
    assert!(!root.path().join(".storage.lock").exists());
}

#[test]
/// 内置单元的 Enable/Disable 只作用于 `state.builtins`，不能描述成外部目录冲突。
fn builtin_toggle_pending_is_scoped_to_builtins_not_external_records() {
    let root = tempfile::tempdir().unwrap();
    // core 不可停用，存储层会拒绝该状态；用可停用的内置单元验证启停作用域。
    let id = PluginId::parse("kerminal.ssh").unwrap();
    let actual = create_package(root.path(), id.as_str());
    let mut state = state_with_record(&id, Some(actual), 81);
    state
        .pending
        .push(PendingPluginChange::Disable { id: id.clone() });
    let store = store_with_state(root.path(), &state);

    let preview = preview_named(&store, &state, "kerminal.ssh");
    assert_eq!(
        preview.pending_changes,
        vec![PendingEvidence {
            kind: PendingChangeKind::Disable,
            scope: PendingChangeScope::BuiltinToggle,
        }]
    );
    assert!(
        preview
            .blockers
            .contains(&PreviewBlocker::PendingBuiltinToggle)
    );
    // 不能把这条 pending 说成需要清理外部目录的冲突。
    assert!(
        !preview
            .blockers
            .contains(&PreviewBlocker::PendingExternalChange)
    );
    assert_eq!(preview.suggested_cleanup, PreviewCleanupSuggestion::Blocked);
}

#[test]
/// 外部记录意图（安装/卸载等）与内置开关分开；其他 ID 的 pending 不冒充目标阻断。
fn external_pending_is_distinguished_from_builtin_toggle_and_other_ids() {
    let root = tempfile::tempdir().unwrap();
    let id = PluginId::parse("kerminal.core").unwrap();
    let actual = create_package(root.path(), id.as_str());
    let mut state = state_with_record(&id, Some(actual), 91);

    let other = PluginId::parse("acme.notes").unwrap();
    state
        .pending
        .push(PendingPluginChange::Enable { id: other });
    let unblocked_state = state.clone();
    let unblocked = preview_for(
        &store_with_state(root.path(), &unblocked_state),
        &unblocked_state,
    );
    assert!(!unblocked.is_blocked());
    assert_eq!(
        unblocked.suggested_cleanup,
        PreviewCleanupSuggestion::RemoveExternalRecordAndQuarantineDirectory
    );

    state.pending.push(PendingPluginChange::Uninstall {
        id: id.clone(),
        // 存储层校验要求规范化 UUID；空串会让状态被判非法而不是进入预览。
        operation_id: uuid::Uuid::new_v4().to_string(),
    });
    let blocked_state = state.clone();
    let blocked = preview_for(
        &store_with_state(root.path(), &blocked_state),
        &blocked_state,
    );
    assert!(
        blocked
            .blockers
            .contains(&PreviewBlocker::PendingExternalChange)
    );
    assert!(
        !blocked
            .blockers
            .contains(&PreviewBlocker::PendingBuiltinToggle)
    );
    assert_eq!(blocked.suggested_cleanup, PreviewCleanupSuggestion::Blocked);
}

#[test]
/// 状态恢复材料存在时必须阻断，不能给出可清理结论。
fn state_recovery_material_blocks_preview() {
    let root = tempfile::tempdir().unwrap();
    let id = PluginId::parse("kerminal.core").unwrap();
    create_package(root.path(), id.as_str());
    fs::write(
        root.path().join("plugins/state-recovery-fixture.json"),
        b"{}",
    )
    .unwrap();
    let state = empty_state(101);
    let store = store_with_state(root.path(), &state);

    let preview = preview_for(&store, &state);
    assert!(preview.recovery.state_recovery_present);
    assert!(!preview.recovery.state_recovery_unverified);
    assert!(
        preview
            .blockers
            .contains(&PreviewBlocker::StateRecoveryPresent)
    );
    assert_eq!(preview.suggested_cleanup, PreviewCleanupSuggestion::Blocked);
}

#[test]
/// 未完成 journal 与无法解析的 journal 都阻断；后者标记为未核对而非允许。
fn unfinished_and_unreadable_journal_evidence_blocks_preview() {
    let root = tempfile::tempdir().unwrap();
    let id = PluginId::parse("kerminal.core").unwrap();
    let fingerprint = create_package(root.path(), id.as_str());
    let operation = uuid::Uuid::new_v4().to_string();
    let journal = layout::PluginLayoutJournal::install(
        root.path(),
        &id,
        &operation,
        fingerprint.clone(),
        fingerprint,
    )
    .unwrap();
    layout::write_journal(root.path(), &journal).unwrap();
    let pending_state = empty_state(111);
    let pending = preview_for(
        &store_with_state(root.path(), &pending_state),
        &pending_state,
    );
    assert!(pending.recovery.pending_layout_journal);
    assert!(!pending.recovery.layout_journal_unverified);
    assert!(
        pending
            .blockers
            .contains(&PreviewBlocker::LayoutJournalPending)
    );
    assert_eq!(pending.suggested_cleanup, PreviewCleanupSuggestion::Blocked);

    let migration = root.path().join("plugins/.migration");
    fs::remove_dir_all(&migration).unwrap();
    let broken = migration.join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&broken).unwrap();
    fs::write(broken.join("journal.json"), b"{").unwrap();

    let unverified_state = empty_state(112);
    let unverified = preview_for(
        &store_with_state(root.path(), &unverified_state),
        &unverified_state,
    );
    assert!(unverified.recovery.layout_journal_unverified);
    assert!(
        unverified
            .blockers
            .contains(&PreviewBlocker::LayoutJournalUnverified)
    );
    assert_eq!(
        unverified.suggested_cleanup,
        PreviewCleanupSuggestion::Blocked
    );
}

#[test]
/// 同身份 `.staging` / `.backup` 残留必须阻断；`.backup` 仍可能被恢复引用。
fn identity_staging_and_backup_residue_blocks_preview() {
    let root = tempfile::tempdir().unwrap();
    let id = PluginId::parse("kerminal.core").unwrap();
    create_package(root.path(), id.as_str());
    fs::create_dir_all(root.path().join("plugins/.staging/kerminal.core/op")).unwrap();
    fs::create_dir_all(root.path().join("plugins/.backup/kerminal.core/op")).unwrap();
    let state = empty_state(121);
    let store = store_with_state(root.path(), &state);

    let preview = preview_for(&store, &state);
    assert!(preview.recovery.staging_present);
    assert!(preview.recovery.backup_present);
    for blocker in [
        PreviewBlocker::StagingForIdentity,
        PreviewBlocker::BackupForIdentity,
    ] {
        assert!(preview.blockers.contains(&blocker), "{blocker:?}");
    }
    assert_eq!(preview.suggested_cleanup, PreviewCleanupSuggestion::Blocked);
    // 探测只读：暂存与备份内容未被删除或移动。
    assert!(
        root.path()
            .join("plugins/.staging/kerminal.core/op")
            .is_dir()
    );
    assert!(
        root.path()
            .join("plugins/.backup/kerminal.core/op")
            .is_dir()
    );
}

#[test]
/// 目录在 revision 不变时被替换，实际指纹和证据摘要必须变化并转为不符。
fn directory_replacement_invalidates_snapshot_evidence() {
    let root = tempfile::tempdir().unwrap();
    let id = PluginId::parse("kerminal.core").unwrap();
    let actual = create_package(root.path(), id.as_str());
    let state = state_with_record(&id, Some(actual), 131);
    let store = store_with_state(root.path(), &state);

    let before = preview_for(&store, &state);
    assert_eq!(
        before.classification,
        ReservedConflictClassification::RegisteredFingerprintMatch
    );

    fs::write(
        root.path().join("plugins/kerminal.core/manifest.json"),
        br#"{"schema":2}"#,
    )
    .unwrap();
    let after = preview_for(&store, &state);

    assert_eq!(after.revision, before.revision);
    assert_eq!(
        after.classification,
        ReservedConflictClassification::RegisteredFingerprintMismatch
    );
    assert_ne!(after.observed_fingerprint(), before.observed_fingerprint());
    assert_ne!(after.evidence_digest, before.evidence_digest);
}

#[test]
/// `Unsupported` 的具体 reason 必须进入摘要，否则两种拒绝会被摘要成同一证据。
fn unsupported_reason_participates_in_evidence_digest() {
    let root = tempfile::tempdir().unwrap();
    let id = PluginId::parse("kerminal.core").unwrap();
    let actual = create_package(root.path(), id.as_str());

    let mut script = installed(Some(actual.clone()));
    script.status = PluginInstallStatus::Unsupported {
        reason: PluginUnsupportedReason::ScriptRemoved,
    };
    let mut reserved = installed(Some(actual));
    reserved.status = PluginInstallStatus::Unsupported {
        reason: PluginUnsupportedReason::ReservedIdentity,
    };

    let mut script_state = empty_state(141);
    let _ = script_state.plugins.insert(id.clone(), script);
    let mut reserved_state = empty_state(141);
    let _ = reserved_state.plugins.insert(id.clone(), reserved);

    let script_preview = preview_for(&store_with_state(root.path(), &script_state), &script_state);
    let reserved_preview = preview_for(
        &store_with_state(root.path(), &reserved_state),
        &reserved_state,
    );
    assert_eq!(
        script_preview.classification,
        reserved_preview.classification
    );
    assert_ne!(
        script_preview.evidence_digest,
        reserved_preview.evidence_digest
    );
}

#[test]
/// 布局层内部问题只折叠成“不安全内容”，不解析文案去伪造具体原因。
fn internal_layout_rejection_is_not_claimed_as_a_specific_cause() {
    // 未知文件类型（命名管道在 Unix 上可创建）与链接都走同一种固定折叠；
    // 布局层对内部问题只提供同一种 `Unsafe` 变体，本模块不解析其文案。
    assert_eq!(
        classify_layout_failure(LayoutError::Unsafe("任意内部文案")),
        DirectoryInspectionFailure::UnsafeTreeContent
    );
    assert_eq!(
        classify_layout_failure(LayoutError::Fingerprint(io::Error::other("x"))),
        DirectoryInspectionFailure::FingerprintUnavailable
    );
    assert_eq!(
        classify_layout_failure(LayoutError::Missing("插件安装缺少暂存目录")),
        DirectoryInspectionFailure::Unreadable
    );
}

#[test]
/// 生产就绪状态必须显式报告预算与门禁未解决，未阻断不等于可执行清理。
fn readiness_reports_budget_and_startup_gates_unresolved() {
    let readiness = production_readiness();
    assert!(!readiness.fingerprint_total_budget_resolved);
    assert!(!readiness.startup_gates_verified);
    assert!(!readiness.grants_authorization);

    let root = tempfile::tempdir().unwrap();
    let state = empty_state(151);
    let store = store_with_state(root.path(), &state);
    let preview = preview_for(&store, &state);
    assert!(!preview.is_blocked());
    assert!(preview.is_clean_for_cleanup());
    // 即使未阻断，也不代表迁移/启动门禁通过。
    assert!(!production_readiness().startup_gates_verified);
}

#[test]
/// provider 名字只作为 `plugins/kerminal` 直接子级处理，绝不变成插件记录身份。
fn provider_identity_uses_raw_direct_child_and_never_becomes_plugin_id() {
    let root = tempfile::tempdir().unwrap();
    create_package(root.path(), "kerminal");
    let data = root.path().join("plugins/data/kerminal");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("secret.txt"), b"unread").unwrap();
    let state = empty_state(161);
    let store = store_with_state(root.path(), &state);

    let preview = preview_named(&store, &state, "kerminal");
    assert!(matches!(
        &preview.identity,
        ReservedPluginIdentity::BuiltinProvider
    ));
    assert!(preview.external_record.is_none());
    assert_eq!(preview.builtin_toggle, BuiltinToggleEvidence::NotApplicable);
    assert_eq!(
        preview.classification,
        ReservedConflictClassification::UnrecordedDirectorySafe
    );
    assert_eq!(
        preview.suggested_cleanup,
        PreviewCleanupSuggestion::QuarantineDirectory
    );
    assert_eq!(
        fs::read(data.join("secret.txt")).unwrap(),
        b"unread".to_vec()
    );
}

#[cfg(any(unix, windows))]
fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link)
    }
}

#[cfg(any(unix, windows))]
#[test]
/// 包目录内出现符号链接或重解析点时报告固定原因；平台拒绝创建 fixture 时显式失败。
fn symlink_inside_reserved_package_is_reported_unsafe() {
    let root = tempfile::tempdir().unwrap();
    let id = PluginId::parse("kerminal.core").unwrap();
    create_package(root.path(), id.as_str());
    let target = root.path().join("plugins/kerminal.core/real.json");
    fs::write(&target, b"{}").unwrap();
    let link = root.path().join("plugins/kerminal.core/alias.json");
    // 不再静默跳过：Windows 需开发者模式或相应权限，无法创建即视为本用例未覆盖并失败。
    create_file_symlink(&target, &link)
        .expect("无法创建符号链接 fixture；本用例视为未覆盖，需在支持链接的环境重跑");
    let state = empty_state(171);
    let store = store_with_state(root.path(), &state);

    let preview = preview_for(&store, &state);
    assert_eq!(
        preview.classification,
        ReservedConflictClassification::UnsafeOrUnreadable
    );
    assert!(matches!(
        &preview.directory,
        ReservedDirectoryEvidence::UnsafeOrUnreadable {
            reason: DirectoryInspectionFailure::UnsafeTreeContent
                | DirectoryInspectionFailure::SymlinkOrReparsePoint
        }
    ));
    assert!(preview.is_blocked());
}

#[cfg(any(unix, windows))]
#[test]
/// 包目录内出现硬链接时摘要必须失败，不能把同一内容当成多个普通文件。
fn hard_link_inside_reserved_package_is_reported_unsafe() {
    let root = tempfile::tempdir().unwrap();
    let id = PluginId::parse("kerminal.core").unwrap();
    create_package(root.path(), id.as_str());
    let original = root.path().join("plugins/kerminal.core/original.json");
    fs::write(&original, b"{}").unwrap();
    let alias = root.path().join("plugins/kerminal.core/alias.json");
    // 与符号链接相同：文件系统不支持硬链接时显式失败，不把跳过当成通过。
    fs::hard_link(&original, &alias)
        .expect("无法创建硬链接 fixture；本用例视为未覆盖，需在支持硬链接的环境重跑");
    let state = empty_state(181);
    let store = store_with_state(root.path(), &state);

    let preview = preview_for(&store, &state);
    assert_eq!(
        preview.classification,
        ReservedConflictClassification::UnsafeOrUnreadable
    );
    assert!(matches!(
        &preview.directory,
        ReservedDirectoryEvidence::UnsafeOrUnreadable {
            reason: DirectoryInspectionFailure::UnsafeTreeContent
                | DirectoryInspectionFailure::SymlinkOrReparsePoint
        }
    ));
    assert!(preview.is_blocked());
}
