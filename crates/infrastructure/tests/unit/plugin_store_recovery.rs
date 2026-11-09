//! 验证未知提交的持久记录、保守读回和显式处理门禁。
//! @author kongweiguang

use super::*;

/// 使用独立配置根和新协调器，模拟后续进程重新打开同一份插件状态。
fn recovery_store(root: &Path) -> PluginStore {
    PluginStore::new(StorageCoordinator::new(root)).unwrap()
}

/// 在原子提交前后注入同一种未知结果，确保判断依赖读回而非错误本身。
fn inject_unknown(store: &PluginStore, apply: bool) -> (PathBuf, PluginStoreState) {
    let current = store.load().unwrap();
    let mut candidate = current.state.clone();
    candidate.revision += 1;
    let directory = store.root().join("plugins");
    fs::create_dir_all(&directory).unwrap();
    let staged = directory.join("test-state-staged.json");
    fs::write(&staged, serde_json::to_vec_pretty(&candidate).unwrap()).unwrap();
    let mode = if current.fingerprint.is_some() {
        CommitMode::ReplaceExisting
    } else {
        CommitMode::CreateNew
    };
    let result = commit_with_recovery(
        &directory,
        &staged,
        mode,
        current.fingerprint,
        &candidate,
        |source, destination, mode| {
            if apply {
                commit_staged_file(source, destination, mode)?;
            }
            Err(AtomicCommitError::CommitUnknown(io::Error::other(
                "injected",
            )))
        },
    );
    let Err(PluginStoreError::OutcomeUnknown {
        recovery_path,
        recovery_persisted: true,
    }) = result
    else {
        panic!("expected unknown outcome and persistent recovery record");
    };
    (recovery_path, candidate)
}

#[test]
/// 已生效的未知提交在重启后可被识别，处理前不能写入或清理版本。
fn applied_unknown_survives_restart_and_blocks_writes_and_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let store = recovery_store(root.path());
    let (path, candidate) = inject_unknown(&store, true);
    drop(store);

    let reopened = recovery_store(root.path());
    assert_eq!(reopened.list_recoveries().unwrap(), vec![path.clone()]);
    let current = reopened.load().unwrap();
    assert_eq!(current.state, candidate);
    assert!(matches!(
        reopened.compare_and_swap(&current, current.state.clone()),
        Err(PluginStoreError::RecoveryRequired { .. })
    ));
    let called = std::cell::Cell::new(false);
    assert!(matches!(
        reopened.with_locked_state(|_| called.set(true)),
        Err(PluginStoreError::RecoveryRequired { .. })
    ));
    assert!(!called.get());

    let inspection = reopened.inspect_recovery(&path).unwrap();
    assert_eq!(inspection.path(), path);
    assert_eq!(inspection.disposition(), PluginRecoveryDisposition::Applied);
    assert_eq!(inspection.candidate(), &candidate);
    assert_eq!(inspection.current().state, candidate);
    let accepted = reopened.resolve_recovery(&inspection).unwrap();
    assert_eq!(accepted.state, candidate);
    assert!(reopened.list_recoveries().unwrap().is_empty());
    assert_eq!(
        reopened
            .compare_and_swap(&accepted, accepted.state.clone())
            .unwrap()
            .state
            .revision,
        candidate.revision + 1
    );
}

#[test]
/// 权威文件仍等于基线时只确认未生效，处理记录不会偷偷执行候选。
fn unchanged_baseline_is_not_applied_and_resolution_never_replays() {
    let root = tempfile::tempdir().unwrap();
    let store = recovery_store(root.path());
    let initial = store.load().unwrap();
    let baseline = store
        .compare_and_swap(&initial, initial.state.clone())
        .unwrap();
    let (path, candidate) = inject_unknown(&store, false);
    drop(store);
    let reopened = recovery_store(root.path());
    let inspection = reopened.inspect_recovery(&path).unwrap();
    assert_eq!(
        inspection.disposition(),
        PluginRecoveryDisposition::NotApplied
    );
    assert_eq!(inspection.current().state, baseline.state);
    assert_eq!(inspection.candidate(), &candidate);
    assert_eq!(
        reopened.resolve_recovery(&inspection).unwrap().state,
        baseline.state
    );
    assert_eq!(reopened.load().unwrap().state, baseline.state);
}

#[test]
/// 首次创建的缺失基线与旧格式缺失基线证据必须分别判断。
fn absent_known_baseline_differs_from_legacy_unknown() {
    let root = tempfile::tempdir().unwrap();
    let store = recovery_store(root.path());
    let (path, candidate) = inject_unknown(&store, false);
    let inspection = store.inspect_recovery(&path).unwrap();
    assert_eq!(
        inspection.disposition(),
        PluginRecoveryDisposition::NotApplied
    );
    store.resolve_recovery(&inspection).unwrap();

    let legacy = root.path().join("plugins/state-recovery-legacy.json");
    fs::write(&legacy, serde_json::to_vec_pretty(&candidate).unwrap()).unwrap();
    let inspection = store.inspect_recovery(&legacy).unwrap();
    assert_eq!(
        inspection.disposition(),
        PluginRecoveryDisposition::Indeterminate
    );
    fs::write(
        root.path().join("plugins/state.json"),
        serde_json::to_vec_pretty(&candidate).unwrap(),
    )
    .unwrap();
    let inspection = store.inspect_recovery(&legacy).unwrap();
    assert_eq!(inspection.disposition(), PluginRecoveryDisposition::Applied);
}

#[test]
/// 两端都不相等时保留分叉证据；显式接受当前状态不会覆盖当前文件。
fn diverged_authoritative_state_is_preserved() {
    let root = tempfile::tempdir().unwrap();
    let store = recovery_store(root.path());
    let (path, candidate) = inject_unknown(&store, false);
    let mut changed = candidate.clone();
    changed.revision += 1;
    let state_path = root.path().join("plugins/state.json");
    fs::write(&state_path, serde_json::to_vec_pretty(&changed).unwrap()).unwrap();
    let inspection = store.inspect_recovery(&path).unwrap();
    assert_eq!(
        inspection.disposition(),
        PluginRecoveryDisposition::Diverged
    );
    assert_eq!(inspection.candidate(), &candidate);
    store.resolve_recovery(&inspection).unwrap();
    assert_eq!(store.load().unwrap().state, changed);
}

#[test]
/// 检查之后权威内容变化会使处理凭据失效，恢复材料必须保留。
fn changed_authoritative_state_rejects_stale_inspection() {
    let root = tempfile::tempdir().unwrap();
    let store = recovery_store(root.path());
    let (path, mut candidate) = inject_unknown(&store, false);
    let inspection = store.inspect_recovery(&path).unwrap();
    candidate.revision += 1;
    fs::write(
        root.path().join("plugins/state.json"),
        serde_json::to_vec_pretty(&candidate).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        store.resolve_recovery(&inspection),
        Err(PluginStoreError::Conflict)
    ));
    assert!(path.exists());
}

#[test]
/// 即使新材料同样合法，只要内容摘要变化也不能用旧凭据删除它。
fn changed_recovery_record_rejects_stale_inspection() {
    let root = tempfile::tempdir().unwrap();
    let store = recovery_store(root.path());
    let (path, mut candidate) = inject_unknown(&store, false);
    let inspection = store.inspect_recovery(&path).unwrap();
    candidate.revision += 1;
    fs::write(&path, serde_json::to_vec_pretty(&candidate).unwrap()).unwrap();
    assert!(matches!(
        store.resolve_recovery(&inspection),
        Err(PluginStoreError::Conflict)
    ));
    assert!(path.exists());
}

#[test]
/// 损坏、目录和过大的同名材料都阻断写入且不能通过恢复接口删除。
fn invalid_recovery_entries_are_preserved_and_block_writes() {
    let root = tempfile::tempdir().unwrap();
    let store = recovery_store(root.path());
    let directory = root.path().join("plugins");
    fs::create_dir_all(&directory).unwrap();
    let corrupt = directory.join("state-recovery-corrupt.json");
    let unsafe_directory = directory.join("STATE-RECOVERY-directory.JSON");
    let oversized = directory.join("state-recovery-large.json");
    fs::write(&corrupt, b"{broken").unwrap();
    fs::create_dir(&unsafe_directory).unwrap();
    File::create(&oversized)
        .unwrap()
        .set_len(MAX_RECOVERY_BYTES + 1)
        .unwrap();
    let discovered = store.list_recoveries().unwrap();
    assert_eq!(discovered.len(), 3);
    for path in [&corrupt, &unsafe_directory, &oversized] {
        assert!(discovered.contains(path));
        assert!(matches!(
            store.inspect_recovery(path),
            Err(PluginStoreError::InvalidRecovery { .. })
        ));
        assert!(path.exists());
    }
    let current = store.load().unwrap();
    assert!(matches!(
        store.compare_and_swap(&current, current.state.clone()),
        Err(PluginStoreError::RecoveryRequired { .. })
    ));
    assert_eq!(fs::read(&corrupt).unwrap(), b"{broken");
}

#[test]
/// 未知格式、缺失基线、重复字段及异常摘要都不能被当成可接受的恢复证据。
fn malformed_or_future_recovery_envelopes_remain_blocking() {
    let root = tempfile::tempdir().unwrap();
    let store = recovery_store(root.path());
    let directory = root.path().join("plugins");
    fs::create_dir_all(&directory).unwrap();
    let candidate = PluginStoreState {
        revision: 1,
        ..PluginStoreState::default()
    };
    let candidate = serde_json::to_string(&candidate).unwrap();
    let cases = [
        format!(r#"{{"recovery_schema":2,"base_fingerprint":null,"candidate":{candidate}}}"#),
        format!(r#"{{"recovery_schema":1,"candidate":{candidate}}}"#),
        format!(
            r#"{{"recovery_schema":1,"base_fingerprint":null,"base_fingerprint":null,"candidate":{candidate}}}"#
        ),
        format!(r#"{{"recovery_schema":1,"base_fingerprint":[1],"candidate":{candidate}}}"#),
        format!(
            r#"{{"recovery_schema":1,"base_fingerprint":null,"candidate":{candidate},"extra":0}}"#
        ),
    ];
    for (index, body) in cases.iter().enumerate() {
        let path = directory.join(format!("state-recovery-format-{index}.json"));
        fs::write(&path, body).unwrap();
        assert!(matches!(
            store.inspect_recovery(&path),
            Err(PluginStoreError::InvalidRecovery { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), body.as_bytes());
    }
    let current = store.load().unwrap();
    assert!(matches!(
        store.compare_and_swap(&current, current.state.clone()),
        Err(PluginStoreError::RecoveryRequired { .. })
    ));
}

#[test]
/// 权威状态损坏时不能核对或移除候选，必须保留两份原始文件等待恢复。
fn corrupt_authoritative_state_preserves_recovery_record() {
    let root = tempfile::tempdir().unwrap();
    let store = recovery_store(root.path());
    let (path, _) = inject_unknown(&store, false);
    let state_path = root.path().join("plugins/state.json");
    fs::write(&state_path, b"{broken").unwrap();
    assert!(matches!(
        store.inspect_recovery(&path),
        Err(PluginStoreError::InvalidState)
    ));
    assert!(path.exists());
    assert_eq!(fs::read(state_path).unwrap(), b"{broken");
}

#[test]
/// 多条记录必须逐条显式处理，解决其中一条不能解除其他记录的写入门禁。
fn multiple_records_keep_gate_until_each_is_resolved() {
    let root = tempfile::tempdir().unwrap();
    let store = recovery_store(root.path());
    let (first, candidate) = inject_unknown(&store, false);
    let second = root.path().join("plugins/state-recovery-legacy.json");
    fs::write(&second, serde_json::to_vec_pretty(&candidate).unwrap()).unwrap();
    assert_eq!(store.list_recoveries().unwrap().len(), 2);
    store
        .resolve_recovery(&store.inspect_recovery(&first).unwrap())
        .unwrap();
    let current = store.load().unwrap();
    assert!(matches!(
        store.compare_and_swap(&current, current.state.clone()),
        Err(PluginStoreError::RecoveryRequired { .. })
    ));
    store
        .resolve_recovery(&store.inspect_recovery(&second).unwrap())
        .unwrap();
    assert!(store.list_recoveries().unwrap().is_empty());
    store
        .compare_and_swap(&current, current.state.clone())
        .unwrap();
}

#[test]
/// 明确未提交和正常提交都清理恢复记录，不把可判定失败伪装成未知结果。
fn known_outcomes_clear_recovery_records() {
    let root = tempfile::tempdir().unwrap();
    let store = recovery_store(root.path());
    let directory = root.path().join("plugins");
    fs::create_dir_all(&directory).unwrap();
    let staged = directory.join("test-state-staged.json");
    let candidate = PluginStoreState {
        revision: 1,
        ..PluginStoreState::default()
    };
    fs::write(&staged, serde_json::to_vec_pretty(&candidate).unwrap()).unwrap();
    let result = commit_with_recovery(
        &directory,
        &staged,
        CommitMode::CreateNew,
        None,
        &candidate,
        |_, _, _| {
            Err(AtomicCommitError::NotCommitted(io::Error::other(
                "injected",
            )))
        },
    );
    assert!(matches!(result, Err(PluginStoreError::Io(_))));
    assert!(store.list_recoveries().unwrap().is_empty());
    let current = store.load().unwrap();
    store
        .compare_and_swap(&current, current.state.clone())
        .unwrap();
    assert!(store.list_recoveries().unwrap().is_empty());
}

#[test]
/// 恢复记录发布本身未知时状态尚未提交；已出现的材料仍需显式读回处理。
fn unknown_recovery_publication_does_not_commit_state() {
    let root = tempfile::tempdir().unwrap();
    let store = recovery_store(root.path());
    let directory = root.path().join("plugins");
    fs::create_dir_all(&directory).unwrap();
    let candidate = PluginStoreState {
        revision: 1,
        ..PluginStoreState::default()
    };
    let result = prepare_recovery(&directory, None, &candidate, |source, target, mode| {
        commit_staged_file(source, target, mode)?;
        Err(AtomicCommitError::CommitUnknown(io::Error::other(
            "injected",
        )))
    });
    let Err(PluginStoreError::RecoveryPreparationUnknown { recovery_path }) = result else {
        panic!("expected unknown preparation");
    };
    assert!(!directory.join("state.json").exists());
    assert_eq!(
        store.list_recoveries().unwrap(),
        vec![recovery_path.clone()]
    );
    let inspection = store.inspect_recovery(&recovery_path).unwrap();
    assert_eq!(
        inspection.disposition(),
        PluginRecoveryDisposition::NotApplied
    );
    store.resolve_recovery(&inspection).unwrap();
    assert_eq!(store.load().unwrap().state, PluginStoreState::default());
}

#[test]
/// 已提交后的清理失败必须暴露状态已生效，避免调用方据错误盲目重放。
fn failed_recovery_cleanup_reports_committed_state() {
    let root = tempfile::tempdir().unwrap();
    let store = recovery_store(root.path());
    let directory = root.path().join("plugins");
    fs::create_dir_all(&directory).unwrap();
    let candidate = PluginStoreState {
        revision: 1,
        ..PluginStoreState::default()
    };
    let path = prepare_recovery(&directory, None, &candidate, commit_staged_file).unwrap();
    fs::write(
        directory.join("state.json"),
        serde_json::to_vec_pretty(&candidate).unwrap(),
    )
    .unwrap();
    let result = finish_recovery_cleanup(Ok(()), path.clone(), |_| {
        Err(AtomicCommitError::NotCommitted(io::Error::other(
            "injected",
        )))
    });
    assert!(matches!(
        result,
        Err(PluginStoreError::RecoveryCleanupFailed {
            state_committed: true,
            deletion_unknown: false,
            ..
        })
    ));
    assert_eq!(store.list_recoveries().unwrap(), vec![path.clone()]);
    let current = store.load().unwrap();
    assert!(matches!(
        store.compare_and_swap(&current, current.state.clone()),
        Err(PluginStoreError::RecoveryRequired { .. })
    ));
    assert_eq!(
        store.inspect_recovery(&path).unwrap().disposition(),
        PluginRecoveryDisposition::Applied
    );
}

#[test]
/// 删除完成后的同步未知与原状态是否提交分别报告，随后必须重新发现记录。
fn unknown_recovery_cleanup_reports_both_outcomes() {
    let root = tempfile::tempdir().unwrap();
    let store = recovery_store(root.path());
    let directory = root.path().join("plugins");
    fs::create_dir_all(&directory).unwrap();
    let candidate = PluginStoreState {
        revision: 1,
        ..PluginStoreState::default()
    };
    let path = prepare_recovery(&directory, None, &candidate, commit_staged_file).unwrap();
    let result =
        finish_recovery_cleanup(Err(io::Error::other("state failed")), path.clone(), |p| {
            fs::remove_file(p).unwrap();
            Err(AtomicCommitError::CommitUnknown(io::Error::other(
                "injected",
            )))
        });
    assert!(matches!(
        result,
        Err(PluginStoreError::RecoveryCleanupFailed {
            state_committed: false,
            deletion_unknown: true,
            ..
        })
    ));
    assert!(store.list_recoveries().unwrap().is_empty());
    assert_eq!(store.load().unwrap().state, PluginStoreState::default());
}

#[cfg(windows)]
#[test]
/// Windows junction 不应被当作可读取的恢复文件，也不能触碰其目录目标。
fn junction_recovery_is_preserved() {
    let root = tempfile::tempdir().unwrap();
    let store = recovery_store(root.path());
    let directory = root.path().join("plugins");
    fs::create_dir_all(&directory).unwrap();
    let target = root.path().join("outside");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("keep.txt"), b"keep").unwrap();
    let link = directory.join("state-recovery-junction.json");
    let output = std::process::Command::new("cmd.exe")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(&link)
        .arg(&target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "junction creation failed: {output:?}"
    );
    assert_eq!(store.list_recoveries().unwrap(), vec![link.clone()]);
    assert!(matches!(
        store.inspect_recovery(&link),
        Err(PluginStoreError::InvalidRecovery { .. })
    ));
    let current = store.load().unwrap();
    assert!(matches!(
        store.compare_and_swap(&current, current.state.clone()),
        Err(PluginStoreError::RecoveryRequired { .. })
    ));
    assert_eq!(fs::read(target.join("keep.txt")).unwrap(), b"keep");
}

#[cfg(unix)]
#[test]
/// 静态符号链接必须作为危险材料保留，不能读取或删除其目标。
fn symbolic_link_recovery_is_preserved() {
    let root = tempfile::tempdir().unwrap();
    let store = recovery_store(root.path());
    let directory = root.path().join("plugins");
    fs::create_dir_all(&directory).unwrap();
    let target = root.path().join("outside.json");
    fs::write(&target, b"keep").unwrap();
    let link = directory.join("state-recovery-link.json");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(matches!(
        store.inspect_recovery(&link),
        Err(PluginStoreError::InvalidRecovery { .. })
    ));
    assert_eq!(fs::read(target).unwrap(), b"keep");
    assert_eq!(store.list_recoveries().unwrap(), vec![link]);
}
