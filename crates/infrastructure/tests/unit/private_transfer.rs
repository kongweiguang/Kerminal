//! SFTP transfer cleanup ownership boundary regression tests.
//!
//! @author kongweiguang

use std::{fs, path::PathBuf, time::Duration};

use kerminal_domain::{
    files::{FileEndpoint, RemotePath},
    identity::{HostId, TransferId},
};
use tokio::time::Instant;
use uuid::Uuid;

use super::*;

/// 远端目录 containment 按 host 身份及 POSIX 组件边界比较，避免前缀相似路径或另一主机误判。
///
/// @author kongweiguang
#[test]
fn canonical_remote_directory_containment_respects_host_and_component_boundaries() -> AppResult<()>
{
    let host_id = HostId::new();
    let source = FileEndpoint::remote(host_id.clone(), "/srv/source/root")
        .map_err(|_| AppError::invalid("test remote source path"))?;
    let child = FileEndpoint::remote(host_id.clone(), "/srv/source/root/child")
        .map_err(|_| AppError::invalid("test remote child path"))?;
    let neighbor = FileEndpoint::remote(host_id.clone(), "/srv/source/root-archive/child")
        .map_err(|_| AppError::invalid("test remote neighbor path"))?;
    let other_host = FileEndpoint::remote(HostId::new(), "/srv/source/root/child")
        .map_err(|_| AppError::invalid("test remote other-host path"))?;

    assert!(endpoint_path_is_within(&source, &child));
    assert!(!endpoint_path_is_within(&source, &neighbor));
    assert!(!endpoint_path_is_within(&source, &other_host));
    Ok(())
}

/// 真实目录 symlink alias 指向源根时，必须在目标 sibling 或 staging 产生前阻止复制进其子树。
/// Unix 用例使用真实链接；Windows 不将缺失的链接权限伪装成策略通过。
///
/// @author kongweiguang
#[cfg(unix)]
#[tokio::test]
async fn canonical_source_directory_alias_rejects_descendant_without_writing() -> AppResult<()> {
    let root = tempfile::tempdir().map_err(map_io_error)?;
    let real_source = root.path().join("source");
    fs::create_dir(&real_source).map_err(map_io_error)?;
    fs::write(real_source.join("payload.txt"), b"preserve").map_err(map_io_error)?;
    let source_alias = root.path().join("source-alias");
    std::os::unix::fs::symlink(&real_source, &source_alias).map_err(map_io_error)?;

    let (canonical_source, metadata) = resolve_source_object(
        &EndpointSession::Local,
        &FileEndpoint::Local { path: source_alias },
        true,
        None,
    )
    .await?;
    assert_eq!(metadata.kind, FileEntryKind::Directory);
    let descendant = FileEndpoint::Local {
        path: real_source.join("copy-inside-source"),
    };
    let mut before = fs::read_dir(&real_source)
        .map_err(map_io_error)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(map_io_error)?;
    before.sort();

    let rejected = ensure_directory_destination_outside_source(
        &EndpointSession::Local,
        &EndpointSession::Local,
        &canonical_source,
        &descendant,
    )
    .await;
    assert!(rejected.is_err());
    let FileEndpoint::Local {
        path: descendant_path,
    } = &descendant
    else {
        unreachable!("the unit destination is local")
    };
    assert!(!descendant_path.exists());
    let mut after = fs::read_dir(&real_source)
        .map_err(map_io_error)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(map_io_error)?;
    after.sort();
    assert_eq!(
        after, before,
        "rejected containment must not create a stage"
    );

    let neighbor = root.path().join("source-archive");
    fs::create_dir(&neighbor).map_err(map_io_error)?;
    let neighbor_destination = FileEndpoint::Local {
        path: neighbor.join("copy"),
    };
    ensure_directory_destination_outside_source(
        &EndpointSession::Local,
        &EndpointSession::Local,
        &canonical_source,
        &neighbor_destination,
    )
    .await?;
    assert!(!neighbor.join("copy").exists());
    Ok(())
}

/// 目录 merge 的进度根不是文件提交目标；恢复事实必须保持暂存与精确目标的同父身份。
///
/// @author kongweiguang
#[test]
fn staging_recovery_record_binds_actual_file_instead_of_progress_root() -> AppResult<()> {
    let root = tempfile::tempdir().map_err(map_io_error)?;
    let progress_root = FileEndpoint::Local {
        path: root.path().join("visible-directory"),
    };
    let actual_destination = FileEndpoint::Local {
        path: root.path().join("visible-directory/nested/payload.bin"),
    };
    let owner = TransferId::new();
    let artifact = StagingArtifact::new(
        temporary_sibling(&actual_destination)?,
        owner.clone(),
        "0123456789abcdef0123456789abcdef".to_owned(),
        StagingKind::File,
    )?;
    let record = artifact.recovery_record(&actual_destination);

    assert_eq!(record.actual_destination, actual_destination);
    assert_eq!(record.staging_destination, artifact.endpoint);
    assert_eq!(record.staging_owner, owner);
    assert_eq!(record.staging_token, artifact.token);
    assert!(same_parent_directory(
        &record.staging_destination,
        &record.actual_destination
    ));
    assert!(!same_parent_directory(
        &record.staging_destination,
        &progress_root
    ));
    Ok(())
}

/// 继任任务沿用原 owner 的暂存身份；只读核对门禁不得把新队列 ID 误当成新的暂存归属。
/// 同时覆盖 owner、token、目标父目录和完整摘要的拒绝分支，不能以放宽 ID 代替证据校验。
///
/// @author kongweiguang
#[test]
fn recovery_evidence_accepts_original_stage_owner_for_a_retry_successor() -> AppResult<()> {
    let original_owner = TransferId::new();
    let successor_id = TransferId::new();
    let host_id = HostId::new();
    let requested_destination = FileEndpoint::remote(host_id.clone(), "/data/payload.bin")
        .map_err(|_| AppError::invalid("test requested destination"))?;
    let actual_destination = FileEndpoint::remote(host_id.clone(), "/data/payload (1).bin")
        .map_err(|_| AppError::invalid("test actual destination"))?;
    let expected_sha256 = "ab".repeat(32);
    let checkpoint = TransferCheckpoint {
        source_revision: FileRevision {
            size: 42,
            modified: None,
            permissions: None,
            content_digest: Some(expected_sha256.clone()),
        },
        confirmed_bytes: 42,
        confirmed_prefix_sha256: expected_sha256.clone(),
        staging_owner: original_owner.clone(),
        staging_destination: create_staging_sibling(&actual_destination, &original_owner)?,
        staging_token: "0123456789abcdef0123456789abcdef".to_owned(),
        actual_destination: actual_destination.clone(),
    };
    let record = TransferRecoveryRecord {
        id: successor_id.clone(),
        request: TransferRequest {
            source: FileEndpoint::Local {
                path: PathBuf::from("source.bin"),
            },
            destination: requested_destination,
            kind: TransferKind::File,
            conflict_policy: ConflictPolicy::Rename,
            idle_timeout_seconds: None,
        },
        actual_destination: Some(actual_destination.clone()),
        staging_ownership: vec![TransferStagingOwnership {
            staging_destination: checkpoint.staging_destination.clone(),
            actual_destination: actual_destination.clone(),
            staging_owner: original_owner.clone(),
            staging_token: checkpoint.staging_token.clone(),
        }],
        checkpoint: Some(checkpoint),
        cleanup_pending: false,
        commit_unknown: true,
        inspection: TransferRecoveryInspection::Insufficient,
        archived: false,
    };
    assert_ne!(original_owner, successor_id);
    assert_eq!(
        recovery_file_evidence(&record),
        Some((&actual_destination, expected_sha256.as_str(), 42))
    );

    let mut wrong_owner = record.clone();
    wrong_owner
        .checkpoint
        .as_mut()
        .expect("checkpoint")
        .staging_owner = successor_id;
    assert!(recovery_file_evidence(&wrong_owner).is_none());

    let mut wrong_token = record.clone();
    wrong_token
        .checkpoint
        .as_mut()
        .expect("checkpoint")
        .staging_token = "../foreign".to_owned();
    assert!(recovery_file_evidence(&wrong_token).is_none());

    let outside_destination = FileEndpoint::remote(host_id, "/outside/payload.bin")
        .map_err(|_| AppError::invalid("test outside destination"))?;
    let mut wrong_parent = record.clone();
    wrong_parent
        .checkpoint
        .as_mut()
        .expect("checkpoint")
        .staging_destination = create_staging_sibling(&outside_destination, &original_owner)?;
    assert!(recovery_file_evidence(&wrong_parent).is_none());

    let mut incomplete = record.clone();
    incomplete
        .checkpoint
        .as_mut()
        .expect("checkpoint")
        .confirmed_bytes = 41;
    assert!(recovery_file_evidence(&incomplete).is_none());

    let mut wrong_digest = record.clone();
    wrong_digest
        .checkpoint
        .as_mut()
        .expect("checkpoint")
        .confirmed_prefix_sha256 = "cd".repeat(32);
    assert!(recovery_file_evidence(&wrong_digest).is_none());

    let mut invalid_digest = record;
    invalid_digest
        .checkpoint
        .as_mut()
        .expect("checkpoint")
        .source_revision
        .content_digest = Some("ab".to_owned());
    assert!(recovery_file_evidence(&invalid_digest).is_none());
    Ok(())
}

/// 原路径已核验消失即释放物理 owner；冻结的持久事实仍可单独补回执，不能依赖已结束的 worker 通道。
///
/// @author kongweiguang
#[tokio::test]
async fn cleaned_staging_keeps_durable_facts_separate_from_physical_owner() -> AppResult<()> {
    let root = tempfile::tempdir().map_err(map_io_error)?;
    let actual_destination = FileEndpoint::Local {
        path: root.path().join("payload.bin"),
    };
    let owner = TransferId::new();
    let mut artifact = StagingArtifact::new(
        create_staging_sibling(&actual_destination, &owner)?,
        owner,
        "0123456789abcdef0123456789abcdef".to_owned(),
        StagingKind::File,
    )?;
    artifact.recovery_ownership = Some(artifact.recovery_record(&actual_destination));
    EndpointSession::Local
        .create_staging_marker(&artifact)
        .await?;
    let FileEndpoint::Local { path: stage_path } = &artifact.endpoint else {
        unreachable!("local stage uses a local data path")
    };
    fs::write(stage_path, b"owned staging bytes").map_err(map_io_error)?;
    let tracker = StagingCleanupTracker::default();
    tracker.register(artifact.clone());
    tracker.mark_stage_created(&artifact.endpoint);
    let facts = tracker.recovery_ownerships();

    tracker
        .cleanup_until(
            &[EndpointSession::Local],
            Instant::now() + Duration::from_secs(2),
        )
        .await?;

    assert!(!tracker.contains(&artifact.endpoint));
    assert!(tracker.recovery_ownerships().is_empty());
    assert!(!stage_path.exists());
    let FileEndpoint::Local { path: marker_path } = &artifact.marker else {
        unreachable!("local stage uses a local marker path")
    };
    assert!(!marker_path.exists());
    assert_eq!(facts, vec![artifact.recovery_record(&actual_destination)]);
    Ok(())
}

/// 模拟 create-new 已可能产生副作用但调用方没有收到所有权回执的路径。
#[tokio::test]
async fn unknown_stage_creation_keeps_data_marker_and_cleanup_owner() -> AppResult<()> {
    let root = tempfile::tempdir().map_err(map_io_error)?;
    let stage_path = root.path().join("payload.partial");
    let endpoint = FileEndpoint::Local {
        path: stage_path.clone(),
    };
    let artifact = StagingArtifact::new(
        endpoint,
        TransferId::new(),
        "0123456789abcdef0123456789abcdef".to_owned(),
        StagingKind::File,
    )?;
    EndpointSession::Local
        .create_staging_marker(&artifact)
        .await?;
    fs::write(&stage_path, b"possibly-created-by-uncertain-request").map_err(map_io_error)?;

    let tracker = StagingCleanupTracker::default();
    tracker.register(artifact.clone());
    let cleanup = tracker
        .cleanup_until(
            &[EndpointSession::Local],
            Instant::now() + Duration::from_secs(2),
        )
        .await;

    assert!(
        cleanup.is_err(),
        "unconfirmed stage ownership must stay pending"
    );
    assert!(
        stage_path.exists(),
        "foreign or uncertain stage must be preserved"
    );
    let FileEndpoint::Local { path: marker_path } = &artifact.marker else {
        unreachable!("local stage creates a local marker")
    };
    assert!(marker_path.exists(), "marker must remain for retry");
    assert_eq!(
        tracker
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1,
        "failed cleanup must retain the artifact owner"
    );
    Ok(())
}

/// 一个 artifact 清理失败时仍处理同一任务中其它独立暂存项，避免全局提前返回扩大残留。
#[tokio::test]
async fn cleanup_continues_after_an_independent_owner_conflict() -> AppResult<()> {
    let root = tempfile::tempdir().map_err(map_io_error)?;
    let owned_path = root.path().join("owned.partial");
    let owned = StagingArtifact::new(
        FileEndpoint::Local {
            path: owned_path.clone(),
        },
        TransferId::new(),
        "0123456789abcdef0123456789abcdef".to_owned(),
        StagingKind::File,
    )?;
    EndpointSession::Local.create_staging_marker(&owned).await?;
    fs::write(&owned_path, b"ours").map_err(map_io_error)?;

    let uncertain_path = root.path().join("uncertain.partial");
    let uncertain = StagingArtifact::new(
        FileEndpoint::Local {
            path: uncertain_path.clone(),
        },
        TransferId::new(),
        "fedcba9876543210fedcba9876543210".to_owned(),
        StagingKind::File,
    )?;
    EndpointSession::Local
        .create_staging_marker(&uncertain)
        .await?;
    fs::write(&uncertain_path, b"not-confirmed-ours").map_err(map_io_error)?;

    let tracker = StagingCleanupTracker::default();
    tracker.register(owned.clone());
    tracker.mark_stage_created(&owned.endpoint);
    tracker.register(uncertain.clone());
    let cleanup = tracker
        .cleanup_until(
            &[EndpointSession::Local],
            Instant::now() + Duration::from_secs(2),
        )
        .await;

    assert!(
        cleanup.is_err(),
        "the uncertain stage remains an explicit cleanup failure"
    );
    assert!(
        !owned_path.exists(),
        "confirmed stage is cleaned despite a sibling failure"
    );
    let FileEndpoint::Local { path: owned_marker } = &owned.marker else {
        unreachable!("local stage creates a local marker")
    };
    assert!(
        !owned_marker.exists(),
        "confirmed marker is cleaned with its stage"
    );
    assert!(
        uncertain_path.exists(),
        "unconfirmed stage remains untouched"
    );
    let FileEndpoint::Local {
        path: uncertain_marker,
    } = &uncertain.marker
    else {
        unreachable!("local stage creates a local marker")
    };
    assert!(
        uncertain_marker.exists(),
        "failed artifact keeps its marker"
    );
    assert_eq!(
        tracker
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1
    );
    Ok(())
}

/// Windows 目录映射必须保留远端遍历位，同时普通文件默认不授予全局写权限。
///
/// @author kongweiguang
#[cfg(windows)]
#[test]
fn windows_directory_transfer_mode_remains_traversable() -> AppResult<()> {
    let root = tempfile::tempdir().map_err(map_io_error)?;
    let file_path = root.path().join("payload.txt");
    fs::write(&file_path, b"safe mode").map_err(map_io_error)?;

    let directory_metadata = fs::metadata(root.path()).map_err(map_io_error)?;
    let file_metadata = fs::metadata(&file_path).map_err(map_io_error)?;

    assert_eq!(local_permissions(&directory_metadata), Some(0o755));
    assert_eq!(local_permissions(&file_metadata), Some(0o644));
    Ok(())
}

/// 只把本次 tracker 精确登记的 sidecar 识别为内部 marker，避免按名字模式误滤用户文件。
///
/// @author kongweiguang
#[test]
fn directory_merge_marker_filter_requires_registered_endpoint() -> AppResult<()> {
    let root = tempfile::tempdir().map_err(map_io_error)?;
    let artifact = StagingArtifact::new(
        FileEndpoint::Local {
            path: root.path().join("payload.partial"),
        },
        TransferId::new(),
        "0123456789abcdef0123456789abcdef".to_owned(),
        StagingKind::File,
    )?;
    let marker = artifact.marker.clone();
    let marker_lookalike = FileEndpoint::Local {
        path: root
            .path()
            .join(".payload.partial.kerminal-owner-unregistered.marker"),
    };
    let legacy_orphan = FileEndpoint::Local {
        path: root
            .path()
            .join(".payload.partial.kerminal-owner-0123456789abcdef.marker"),
    };
    let tracker = StagingCleanupTracker::default();
    tracker.register(artifact.clone());

    assert!(tracker.is_marker_endpoint(&marker));
    assert!(!tracker.is_marker_endpoint(&artifact.endpoint));
    assert!(!tracker.is_marker_endpoint(&marker_lookalike));
    assert!(
        !tracker.is_marker_endpoint(&legacy_orphan),
        "old-style orphan names are never adopted as this attempt's owner"
    );
    Ok(())
}

/// 固定源类型与 follow 开关的预检矩阵，不依赖操作系统创建真实链接的权限。
///
/// @author kongweiguang
#[test]
fn source_symlink_preflight_matrix_is_platform_independent() {
    let metadata = |kind| ObjectMetadata {
        kind,
        size: 8,
        modified: None,
        modified_seconds: None,
        permissions: None,
    };

    for follow_source_symlinks in [false, true] {
        assert!(
            validate_transfer_source(
                TransferKind::File,
                &metadata(FileEntryKind::File),
                follow_source_symlinks,
            )
            .is_ok(),
            "regular files must remain valid regardless of the symlink option"
        );
        assert!(
            validate_transfer_source(
                TransferKind::Directory,
                &metadata(FileEntryKind::Directory),
                follow_source_symlinks,
            )
            .is_ok(),
            "regular directories must remain valid regardless of the symlink option"
        );
        assert!(
            validate_transfer_source(
                TransferKind::File,
                &metadata(FileEntryKind::Directory),
                follow_source_symlinks,
            )
            .is_err(),
            "a directory must not become a file when following is enabled"
        );
        assert!(
            validate_transfer_source(
                TransferKind::Directory,
                &metadata(FileEntryKind::File),
                follow_source_symlinks,
            )
            .is_err(),
            "a file must not become a directory when following is enabled"
        );

        let file_link = validate_transfer_source(
            TransferKind::File,
            &metadata(FileEntryKind::Symlink),
            follow_source_symlinks,
        );
        let directory_link = validate_transfer_source(
            TransferKind::Directory,
            &metadata(FileEntryKind::Symlink),
            follow_source_symlinks,
        );
        assert_eq!(
            file_link.is_ok(),
            follow_source_symlinks,
            "file symlink admission must exactly follow the explicit option"
        );
        assert_eq!(
            directory_link.is_ok(),
            follow_source_symlinks,
            "directory symlink admission must defer type verification only when opted in"
        );
    }
}

/// 确认队列预检只在源端策略 opt-in 后接受链接身份，并继续验证解析目标仍在源目录内。
///
/// @author kongweiguang
#[cfg(unix)]
#[tokio::test]
async fn local_source_symlink_follow_is_opt_in_and_scoped() -> AppResult<()> {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().map_err(map_io_error)?;
    let target = root.path().join("payload.txt");
    let alias = root.path().join("alias.txt");
    fs::write(&target, b"source").map_err(map_io_error)?;
    symlink(&target, &alias).map_err(map_io_error)?;
    let alias_endpoint = FileEndpoint::Local { path: alias };
    let destination = FileEndpoint::Remote {
        host_id: HostId::new(),
        path: RemotePath::parse("/upload/alias.txt")
            .map_err(|_| AppError::invalid("test destination path"))?,
    };
    let default_host_options = HostSftpOptions::default();
    let default_upload_options = effective_sftp_options(
        &alias_endpoint,
        &destination,
        None,
        Some(&default_host_options),
    );

    let alias_metadata = EndpointSession::Local
        .metadata(&alias_endpoint)
        .await?
        .ok_or_else(|| AppError::not_found("test source symlink"))?;
    assert!(
        validate_transfer_source(
            TransferKind::File,
            &alias_metadata,
            default_upload_options.follow_source_symlinks,
        )
        .is_err(),
        "the preflight must reject a link when the effective source policy is disabled"
    );
    assert!(
        resolve_source_object(
            &EndpointSession::Local,
            &alias_endpoint,
            default_upload_options.follow_source_symlinks,
            None,
        )
        .await
        .is_err(),
        "the default no-follow policy must reject a source link"
    );
    let upload_host_options = HostSftpOptions {
        follow_source_symlinks: true,
        ..HostSftpOptions::default()
    };
    let upload_options = effective_sftp_options(
        &alias_endpoint,
        &destination,
        None,
        Some(&upload_host_options),
    );
    assert!(
        validate_transfer_source(
            TransferKind::File,
            &alias_metadata,
            upload_options.follow_source_symlinks,
        )
        .is_ok(),
        "the preflight must defer an opted-in source link to canonicalization and scope checks"
    );
    let scope = EndpointSession::Local
        .canonicalize_source(&FileEndpoint::Local {
            path: root.path().to_path_buf(),
        })
        .await?;
    let (resolved, metadata) = resolve_source_object(
        &EndpointSession::Local,
        &alias_endpoint,
        upload_options.follow_source_symlinks,
        Some(&scope),
    )
    .await?;
    assert_eq!(
        resolved,
        EndpointSession::Local
            .canonicalize_source(&FileEndpoint::Local { path: target })
            .await?
    );
    assert_eq!(metadata.kind, kerminal_domain::files::FileEntryKind::File);
    Ok(())
}

/// 即使本机→远端上传显式启用源链接跟随，也不能读取所选源目录之外的目标。
///
/// @author kongweiguang
#[cfg(unix)]
#[tokio::test]
async fn local_source_symlink_escape_is_rejected() -> AppResult<()> {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().map_err(map_io_error)?;
    let outside = tempfile::tempdir().map_err(map_io_error)?;
    let target = outside.path().join("secret.txt");
    fs::write(&target, b"outside").map_err(map_io_error)?;
    let alias = root.path().join("escape.txt");
    symlink(&target, &alias).map_err(map_io_error)?;
    let scope = EndpointSession::Local
        .canonicalize_source(&FileEndpoint::Local {
            path: root.path().to_path_buf(),
        })
        .await?;

    assert!(
        resolve_source_object(
            &EndpointSession::Local,
            &FileEndpoint::Local { path: alias },
            true,
            Some(&scope),
        )
        .await
        .is_err(),
        "canonical targets outside the selected root must be rejected"
    );
    Ok(())
}

/// 悬空源链接和互相引用的链接均在读取前失败，不进入复制或产生任何目标暂存。
///
/// @author kongweiguang
#[cfg(unix)]
#[tokio::test]
async fn dangling_and_cyclic_local_source_symlinks_are_rejected() -> AppResult<()> {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().map_err(map_io_error)?;
    let dangling = root.path().join("dangling.txt");
    symlink(root.path().join("missing.txt"), &dangling).map_err(map_io_error)?;
    let first = root.path().join("cycle-a");
    let second = root.path().join("cycle-b");
    symlink(&second, &first).map_err(map_io_error)?;
    symlink(&first, &second).map_err(map_io_error)?;
    let scope = EndpointSession::Local
        .canonicalize_source(&FileEndpoint::Local {
            path: root.path().to_path_buf(),
        })
        .await?;

    for path in [dangling, first] {
        assert!(
            resolve_source_object(
                &EndpointSession::Local,
                &FileEndpoint::Local { path },
                true,
                Some(&scope),
            )
            .await
            .is_err(),
            "a dangling or cyclic symlink must not be treated as a file"
        );
    }
    Ok(())
}

/// 目录内回链 canonicalize 后回到已访问根目录时拒绝递归，深度上限同时独立封顶。
///
/// @author kongweiguang
#[cfg(unix)]
#[tokio::test]
async fn local_directory_symlink_loop_and_excessive_depth_are_rejected() -> AppResult<()> {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().map_err(map_io_error)?;
    let child = root.path().join("child");
    fs::create_dir(&child).map_err(map_io_error)?;
    let loop_link = child.join("back-to-root");
    symlink(root.path(), &loop_link).map_err(map_io_error)?;
    let root_endpoint = EndpointSession::Local
        .canonicalize_source(&FileEndpoint::Local {
            path: root.path().to_path_buf(),
        })
        .await?;
    let child_endpoint = EndpointSession::Local
        .canonicalize_source(&FileEndpoint::Local { path: child })
        .await?;
    let loop_target = EndpointSession::Local
        .canonicalize_source(&FileEndpoint::Local { path: loop_link })
        .await?;
    let mut visited = std::collections::HashSet::new();
    register_source_directory(&mut visited, &root_endpoint, 0)?;
    register_source_directory(&mut visited, &child_endpoint, 1)?;
    assert!(
        register_source_directory(&mut visited, &loop_target, 2).is_err(),
        "a link back to an ancestor must not recurse"
    );
    assert!(
        register_source_directory(
            &mut std::collections::HashSet::new(),
            &child_endpoint,
            MAX_SOURCE_DIRECTORY_DEPTH + 1,
        )
        .is_err(),
        "extreme nesting is rejected before unbounded traversal"
    );
    Ok(())
}

/// 路由策略只计算一次：上传 follow 取目标主机，下载/跨远端 follow 取源主机，mtime 取目标策略。
///
/// @author kongweiguang
#[test]
fn sftp_host_options_follow_route_and_mtime_target_rules() -> AppResult<()> {
    let source_host = HostId::new();
    let destination_host = HostId::new();
    let local = FileEndpoint::Local {
        path: PathBuf::from("C:/local/input"),
    };
    let source_remote = FileEndpoint::Remote {
        host_id: source_host.clone(),
        path: RemotePath::parse("/source/input")
            .map_err(|_| AppError::invalid("test source path"))?,
    };
    let destination_remote = FileEndpoint::Remote {
        host_id: destination_host,
        path: RemotePath::parse("/destination/input")
            .map_err(|_| AppError::invalid("test destination path"))?,
    };
    let source_options = HostSftpOptions {
        preserve_mtime: false,
        follow_source_symlinks: true,
        ..HostSftpOptions::default()
    };
    let destination_options = HostSftpOptions {
        preserve_mtime: true,
        follow_source_symlinks: false,
        ..HostSftpOptions::default()
    };

    let upload = effective_sftp_options(
        &local,
        &destination_remote,
        Some(&source_options),
        Some(&destination_options),
    );
    assert_eq!(
        upload,
        EffectiveSftpOptions {
            preserve_mtime: true,
            follow_source_symlinks: false,
        }
    );
    let download = effective_sftp_options(&source_remote, &local, Some(&source_options), None);
    assert_eq!(
        download,
        EffectiveSftpOptions {
            preserve_mtime: false,
            follow_source_symlinks: true,
        }
    );
    let remote_copy = effective_sftp_options(
        &source_remote,
        &destination_remote,
        Some(&source_options),
        Some(&destination_options),
    );
    assert_eq!(
        remote_copy,
        EffectiveSftpOptions {
            preserve_mtime: true,
            follow_source_symlinks: true,
        }
    );
    assert_eq!(
        effective_sftp_options(&local, &destination_remote, None, None),
        EffectiveSftpOptions {
            preserve_mtime: true,
            follow_source_symlinks: false,
        },
        "missing host snapshots preserve safe defaults"
    );
    Ok(())
}

/// 本机暂存文件的 mtime 写回只保留 SFTP 可表达的秒精度，并在读回后核对真实元数据。
///
/// @author kongweiguang
#[tokio::test]
async fn local_staging_mtime_can_be_preserved_at_protocol_precision() -> AppResult<()> {
    let root = tempfile::tempdir().map_err(map_io_error)?;
    let target = root.path().join("stage.partial");
    fs::write(&target, b"payload").map_err(map_io_error)?;
    let endpoint = FileEndpoint::Local { path: target };
    EndpointSession::Local
        .set_modified_time(&endpoint, 1_700_000_000)
        .await?;
    let metadata = EndpointSession::Local
        .metadata(&endpoint)
        .await?
        .ok_or_else(|| AppError::not_found("test staging file"))?;
    assert_eq!(metadata.modified_seconds, Some(1_700_000_000));
    Ok(())
}

/// 未知提交只在最终目标是稳定普通文件且完整 SHA-256 匹配时报告核对一致。
///
/// @author kongweiguang
#[tokio::test]
async fn recovery_inspection_is_read_only_and_bound_to_exact_destination_bytes() -> AppResult<()> {
    let root = tempfile::tempdir().map_err(map_io_error)?;
    let path = root.path().join("committed.bin");
    let bytes = b"possibly committed bytes";
    fs::write(&path, bytes).map_err(map_io_error)?;
    let endpoint = FileEndpoint::Local { path: path.clone() };
    let expected = sha256_hex(Sha256::digest(bytes).as_slice());
    let different_digest = "00".repeat(32);
    let cancellation = tokio_util::sync::CancellationToken::new();

    assert!(matches!(
        inspect_recovery_destination(
            &EndpointSession::Local,
            &endpoint,
            bytes.len() as u64,
            &expected,
            &cancellation,
        )
        .await?,
        TransferRecoveryInspection::DestinationMatches
    ));
    assert!(matches!(
        inspect_recovery_destination(
            &EndpointSession::Local,
            &endpoint,
            bytes.len() as u64,
            &different_digest,
            &cancellation,
        )
        .await?,
        TransferRecoveryInspection::DestinationDiffers
    ));
    assert_eq!(fs::read(&path).map_err(map_io_error)?, bytes);
    fs::remove_file(&path).map_err(map_io_error)?;
    assert!(matches!(
        inspect_recovery_destination(
            &EndpointSession::Local,
            &endpoint,
            0,
            &expected,
            &cancellation,
        )
        .await?,
        TransferRecoveryInspection::DestinationMissing
    ));
    Ok(())
}

/// 恢复检查使用 lstat，不能把链接目的地内容当作原提交产生的文件证据。
///
/// @author kongweiguang
#[cfg(unix)]
#[tokio::test]
async fn recovery_inspection_does_not_follow_destination_symlinks() -> AppResult<()> {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().map_err(map_io_error)?;
    let real_file = root.path().join("real.bin");
    let link = root.path().join("destination.bin");
    let bytes = b"not the committed path";
    fs::write(&real_file, bytes).map_err(map_io_error)?;
    symlink(&real_file, &link).map_err(map_io_error)?;
    let expected = sha256_hex(Sha256::digest(bytes).as_slice());
    assert!(matches!(
        inspect_recovery_destination(
            &EndpointSession::Local,
            &FileEndpoint::Local { path: link },
            bytes.len() as u64,
            &expected,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await?,
        TransferRecoveryInspection::DestinationDiffers
    ));
    Ok(())
}

/// 长 Unicode basename 只留在正式目标和 marker 摘要里，所有 sidecar 名称仍受固定长度约束。
///
/// @author kongweiguang
#[test]
fn long_unicode_local_path_has_a_bounded_marker() -> AppResult<()> {
    let long_parent = format!("C:\\{}", "目录\\".repeat(700));
    let original_name = "界".repeat(100);
    let destination = FileEndpoint::Local {
        path: PathBuf::from(long_parent).join(&original_name),
    };
    let owner = TransferId::new();
    let staging = create_staging_sibling(&destination, &owner)?;
    let child_stage = temporary_sibling(&destination)?;
    let artifact = StagingArtifact::new(
        staging.clone(),
        owner.clone(),
        "fedcba9876543210fedcba9876543210".to_owned(),
        StagingKind::File,
    )?;

    for endpoint in [&staging, &child_stage, &artifact.marker] {
        let name = endpoint_name(endpoint)?;
        assert!(name.len() <= 100, "sidecar component length is bounded");
        assert!(
            !name.contains(&original_name),
            "user basename is not embedded"
        );
    }
    assert!(checkpoint_owner_matches(&staging, &owner));
    let marker = artifact.marker_bytes()?;
    assert!(marker.len() as u64 <= STAGING_MARKER_MAX_BYTES);
    assert!(
        !marker
            .windows("目录".len())
            .any(|window| window == "目录".as_bytes())
    );
    Ok(())
}

/// 并发生成同一目标的 partial 必须始终使用独立随机路径，避免任务互相覆盖或误认归属。
///
/// @author kongweiguang
#[test]
fn concurrent_stage_names_remain_unique_for_the_same_owner_and_target() {
    let destination = FileEndpoint::Local {
        path: PathBuf::from("C:/shared/payload.bin"),
    };
    let owner = TransferId::new();
    let workers = (0..8)
        .map(|_| {
            let destination = destination.clone();
            let owner = owner.clone();
            std::thread::spawn(move || {
                (0..64)
                    .map(|_| {
                        let staging = create_staging_sibling(&destination, &owner)
                            .expect("staging sibling must be valid");
                        endpoint_name(&staging).expect("staging name must be representable")
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();
    let names = workers
        .into_iter()
        .flat_map(|worker| worker.join().expect("staging name worker must complete"))
        .collect::<Vec<_>>();
    let unique = names.iter().collect::<std::collections::HashSet<_>>();

    assert_eq!(names.len(), 512);
    assert_eq!(unique.len(), names.len());
}

/// marker 路径发生冲突时独占创建必须失败关闭，并保留未知 owner 写入的原始内容。
///
/// @author kongweiguang
#[tokio::test]
async fn marker_name_conflict_preserves_the_existing_marker() -> AppResult<()> {
    let root = tempfile::tempdir().map_err(map_io_error)?;
    let destination = FileEndpoint::Local {
        path: root.path().join("payload.bin"),
    };
    let owner = TransferId::new();
    let staging = create_staging_sibling(&destination, &owner)?;
    let artifact = StagingArtifact::new(
        staging.clone(),
        owner,
        "fedcba9876543210fedcba9876543210".to_owned(),
        StagingKind::File,
    )?;
    let FileEndpoint::Local { path: marker_path } = &artifact.marker else {
        unreachable!("local stage creates a local marker")
    };
    fs::write(marker_path, b"foreign-owner").map_err(map_io_error)?;

    let result = EndpointSession::Local
        .create_staging_marker(&artifact)
        .await;

    assert!(
        result.is_err(),
        "an existing marker must not be overwritten"
    );
    assert_eq!(
        fs::read(marker_path).map_err(map_io_error)?,
        b"foreign-owner"
    );
    let FileEndpoint::Local { path: staging_path } = &staging else {
        unreachable!("local stage uses a local path")
    };
    assert!(
        !staging_path.exists(),
        "marker conflict must prevent stage creation"
    );
    Ok(())
}

/// endpoint 摘要保留路径差异，即使 marker 本身不暴露原始本机路径。
#[test]
fn marker_fingerprint_changes_when_native_path_changes() -> AppResult<()> {
    let owner = TransferId::new();
    let token = Uuid::new_v4().simple().to_string();
    let left = StagingArtifact::new(
        FileEndpoint::Local {
            path: PathBuf::from("C:\\work\\left.partial"),
        },
        owner.clone(),
        token.clone(),
        StagingKind::File,
    )?;
    let right = StagingArtifact::new(
        FileEndpoint::Local {
            path: PathBuf::from("C:\\work\\right.partial"),
        },
        owner,
        token,
        StagingKind::File,
    )?;

    assert_ne!(left.marker_bytes()?, right.marker_bytes()?);
    Ok(())
}

/// 子文件 owner 校验失败必须阻止 root rename，不能把未清理的 marker 发布到正式树。
///
/// @author kongweiguang
#[tokio::test]
async fn child_marker_conflict_blocks_directory_root_publication() -> AppResult<()> {
    let root = tempfile::tempdir().map_err(map_io_error)?;
    let staging_path = root.path().join("tree.partial");
    let destination_path = root.path().join("published-tree");
    fs::create_dir(&staging_path).map_err(map_io_error)?;
    let session = EndpointSession::Local;
    let owner = TransferId::new();
    let root_endpoint = FileEndpoint::Local {
        path: staging_path.clone(),
    };
    let root_artifact = StagingArtifact::new(
        root_endpoint.clone(),
        owner.clone(),
        "0123456789abcdef0123456789abcdef".to_owned(),
        StagingKind::Tree,
    )?;
    session.create_staging_marker(&root_artifact).await?;

    let child_stage = staging_path.join(".payload.partial");
    let child_artifact = StagingArtifact::new(
        FileEndpoint::Local { path: child_stage },
        owner,
        "fedcba9876543210fedcba9876543210".to_owned(),
        StagingKind::File,
    )?;
    session.create_staging_marker(&child_artifact).await?;
    fs::write(staging_path.join("payload.txt"), b"payload").map_err(map_io_error)?;

    let tracker = StagingCleanupTracker::default();
    tracker.register(root_artifact.clone());
    tracker.mark_stage_created(&root_endpoint);
    tracker.register(child_artifact.clone());
    tracker.mark_stage_created(&child_artifact.endpoint);
    tracker.mark_committed(&child_artifact.endpoint);
    let FileEndpoint::Local { path: child_marker } = &child_artifact.marker else {
        unreachable!("local child artifacts use local markers")
    };
    fs::write(child_marker, b"replaced-owner-marker").map_err(map_io_error)?;

    let result = tracker
        .retire_committed_file_markers_before_tree_publish(&session, &root_endpoint)
        .await;
    assert!(result.is_err(), "a replaced marker must fail closed");
    assert!(
        staging_path.is_dir(),
        "the root staging tree remains unpublished"
    );
    assert!(
        !destination_path.exists(),
        "the directory commit cannot start after child ownership validation fails"
    );
    assert!(child_marker.exists(), "the untrusted marker is not deleted");
    let FileEndpoint::Local { path: root_marker } = &root_artifact.marker else {
        unreachable!("local tree artifacts use local markers")
    };
    assert!(
        root_marker.exists(),
        "the root recovery marker remains available"
    );
    assert_eq!(
        tracker
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        2,
        "failed marker retirement keeps both cleanup records"
    );
    Ok(())
}

/// 模拟 root rename 已发生但回执丢失；清理只退役 child journal，不删除正式目标或 root recovery marker。
///
/// @author kongweiguang
#[tokio::test]
async fn unknown_directory_root_commit_preserves_recovery_marker_and_target() -> AppResult<()> {
    let root = tempfile::tempdir().map_err(map_io_error)?;
    let staging_path = root.path().join("tree.partial");
    let destination_path = root.path().join("published-tree");
    fs::create_dir(&staging_path).map_err(map_io_error)?;
    fs::write(staging_path.join("payload.txt"), b"payload").map_err(map_io_error)?;
    let session = EndpointSession::Local;
    let owner = TransferId::new();
    let root_endpoint = FileEndpoint::Local {
        path: staging_path.clone(),
    };
    let root_artifact = StagingArtifact::new(
        root_endpoint.clone(),
        owner.clone(),
        "0123456789abcdef0123456789abcdef".to_owned(),
        StagingKind::Tree,
    )?;
    session.create_staging_marker(&root_artifact).await?;

    let child_artifact = StagingArtifact::new(
        FileEndpoint::Local {
            path: staging_path.join(".payload.partial"),
        },
        owner,
        "fedcba9876543210fedcba9876543210".to_owned(),
        StagingKind::File,
    )?;
    session.create_staging_marker(&child_artifact).await?;
    let tracker = StagingCleanupTracker::default();
    tracker.register(root_artifact.clone());
    tracker.mark_stage_created(&root_endpoint);
    tracker.register(child_artifact.clone());
    tracker.mark_stage_created(&child_artifact.endpoint);
    tracker.mark_committed(&child_artifact.endpoint);
    tracker
        .retire_committed_file_markers_before_tree_publish(&session, &root_endpoint)
        .await?;
    let FileEndpoint::Local { path: child_marker } = &child_artifact.marker else {
        unreachable!("local child artifacts use local markers")
    };
    assert!(
        !child_marker.exists(),
        "child sidecars are retired before publish"
    );

    // 本机 rename 成功后不再向 tracker 报 ACK，代表提交回执未知而非确认成功。
    fs::rename(&staging_path, &destination_path).map_err(map_io_error)?;
    tracker.preserve_all();
    tracker
        .cleanup_until(&[session], Instant::now() + Duration::from_secs(2))
        .await?;

    assert_eq!(
        fs::read(destination_path.join("payload.txt")).map_err(map_io_error)?,
        b"payload",
        "cleanup must never delete a destination whose root commit may have succeeded"
    );
    let FileEndpoint::Local { path: root_marker } = &root_artifact.marker else {
        unreachable!("local tree artifacts use local markers")
    };
    assert!(
        root_marker.exists(),
        "unknown root commit retains recovery evidence"
    );
    assert_eq!(
        tracker
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1,
        "the root owner remains retained after the child cleanup record is retired"
    );
    Ok(())
}
