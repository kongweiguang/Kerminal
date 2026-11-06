//! 流式断点缓存与真实传输恢复的回归验证。
//!
//! @author kongweiguang

use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use kerminal_application::{
    connections::{
        ConnectionIntent, HostKeyChallenge, HostKeyPeer, HostKeyRotationReceipt,
        HostKeyRotationRequest, HostKeyTrustReceipt, HostKeyTrustRequest, RemoteConnectionLease,
    },
    preferences::SftpPerformanceSettings,
    transfers::{
        InMemoryTransferRecoveryRepository, SftpTransferPolicyStore, TransferQueueOptions,
        TransferRecoveryPage, TransferRecoveryRepository, TransferService,
    },
};
use kerminal_domain::{
    files::{TransferStatus, TransferSummary},
    identity::HostId,
};
use tokio::{sync::Notify, time::timeout};

use super::*;

/// 使用完整断点核对错误转换，确保恢复资格没有与失败类型脱节。
///
/// @author kongweiguang
fn verification_checkpoint_fixture() -> TransferCheckpoint {
    TransferCheckpoint {
        source_revision: FileRevision {
            size: 64,
            modified: None,
            permissions: None,
            content_digest: Some("a".repeat(64)),
        },
        confirmed_bytes: 32,
        confirmed_prefix_sha256: "b".repeat(64),
        staging_owner: TransferId::new(),
        staging_destination: FileEndpoint::Local {
            path: "partial".into(),
        },
        staging_token: "c".repeat(32),
        actual_destination: FileEndpoint::Local {
            path: "destination".into(),
        },
    }
}

/// 仅临时通信错误和取消保留原断点，内容或访问冲突不能被包装为可续传。
///
/// @author kongweiguang
#[test]
fn verification_failure_retains_only_recoverable_checkpoints() {
    let checkpoint = verification_checkpoint_fixture();
    let destination = &checkpoint.actual_destination;
    for (error, expected_kind) in [
        (
            AppError::timeout("test verification timeout"),
            TransferFailureKind::IdleTimeout,
        ),
        (
            map_io_error(io::Error::from(io::ErrorKind::BrokenPipe)),
            TransferFailureKind::Connection,
        ),
    ] {
        let TransferExecutionError::Failed(failure) =
            verification_error(error, Some(checkpoint.clone()), destination)
        else {
            panic!("network failure must remain a failure");
        };
        assert_eq!(failure.kind(), expected_kind);
        assert!(failure.retryable());
        assert!(failure.resumable());
        assert_eq!(failure.checkpoint(), Some(&checkpoint));
    }
    for error in [
        AppError::conflict("test prefix is incomplete"),
        map_io_error(io::Error::from(io::ErrorKind::PermissionDenied)),
        AppError::not_found("test source is missing"),
    ] {
        let TransferExecutionError::Failed(failure) =
            verification_error(error, Some(checkpoint.clone()), destination)
        else {
            panic!("content or access error must remain a failure");
        };
        assert!(!failure.resumable());
        assert!(failure.checkpoint().is_none());
    }
    let TransferExecutionError::Cancelled {
        checkpoint: cancelled_checkpoint,
        ..
    } = verification_error(
        AppError::cancelled("test verification cancellation"),
        Some(checkpoint.clone()),
        destination,
    )
    else {
        panic!("cancellation must remain cancellation");
    };
    assert_eq!(cancelled_checkpoint.as_ref(), Some(&checkpoint));
    let TransferExecutionError::Failed(without_checkpoint) = verification_error(
        AppError::timeout("test without checkpoint"),
        None,
        destination,
    ) else {
        panic!("timeout must remain a failure");
    };
    assert!(without_checkpoint.retryable());
    assert!(!without_checkpoint.resumable());
}

struct NoRemoteConnections;

#[async_trait]
impl RemoteConnectionService for NoRemoteConnections {
    /// 本机续传回归不允许意外发起网络连接。
    ///
    /// @author kongweiguang
    async fn connect(
        &self,
        _host_id: HostId,
        _intent: ConnectionIntent,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        Err(AppError::unsupported("本机测试不连接远端"))
    }

    /// 本机续传不读取远端主机密钥。
    ///
    /// @author kongweiguang
    async fn inspect_host_key(
        &self,
        _host_id: HostId,
        _peer: HostKeyPeer,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyChallenge> {
        Err(AppError::unsupported("本机测试不读取远端密钥"))
    }

    /// 本机续传不修改远端主机信任。
    ///
    /// @author kongweiguang
    async fn trust_host_key(
        &self,
        _request: HostKeyTrustRequest,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyTrustReceipt> {
        Err(AppError::unsupported("本机测试不修改远端信任"))
    }

    /// 本机续传不触发远端主机密钥轮换。
    ///
    /// @author kongweiguang
    async fn replace_host_key(
        &self,
        _request: HostKeyRotationRequest,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyRotationReceipt> {
        Err(AppError::unsupported("本机测试不轮换远端密钥"))
    }
}

struct ObservedBackend {
    backend: Arc<SftpTransferBackend>,
    injected: Option<TransferCheckpoint>,
    paused: StdMutex<Option<TransferCheckpoint>>,
}

#[async_trait]
impl TransferBackend for ObservedBackend {
    /// 由真实后端判断能力，测试包装只记录或注入续传证据。
    ///
    /// @author kongweiguang
    fn supports(&self, request: &TransferRequest) -> bool {
        self.backend.supports(request)
    }

    /// 复用真实队列的进度与提交许可，并在 worker 退出前记录实际返回的暂停断点。
    ///
    /// @author kongweiguang
    async fn execute(
        &self,
        mut context: TransferExecutionContext,
    ) -> Result<TransferExecutionResult, TransferExecutionError> {
        if let Some(checkpoint) = &self.injected {
            context.resume_from = Some(checkpoint.clone());
            context.actual_destination = Some(checkpoint.actual_destination.clone());
        }
        let result = self.backend.execute(context).await;
        if let Err(TransferExecutionError::Cancelled {
            checkpoint: Some(checkpoint),
            ..
        }) = &result
        {
            *self.paused.lock().expect("paused checkpoint lock") = Some(checkpoint.clone());
        }
        result
    }
}

#[derive(Default)]
struct RecordingRecovery {
    inner: InMemoryTransferRecoveryRepository,
    full_checkpoint: StdMutex<Option<TransferCheckpoint>>,
    hold_commit: AtomicBool,
    commit_ready: Notify,
    commit_release: Notify,
}

#[async_trait]
impl TransferRecoveryRepository for RecordingRecovery {
    /// 沿用生产队列的仓储契约，避免测试替身更改恢复列表语义。
    ///
    /// @author kongweiguang
    async fn list(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<TransferRecoveryRecord>> {
        self.inner.list(cancellation).await
    }

    /// 在提交许可前捕获队列实际持久化的完整摘要，必要时暂停许可供测试释放文件锁。
    ///
    /// @author kongweiguang
    async fn upsert(
        &self,
        record: TransferRecoveryRecord,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        if record.commit_unknown
            && let Some(checkpoint) = &record.checkpoint
            && checkpoint.source_revision.content_digest.is_some()
        {
            *self.full_checkpoint.lock().expect("full checkpoint lock") = Some(checkpoint.clone());
            if self.hold_commit.swap(false, Ordering::SeqCst) {
                self.commit_ready.notify_one();
                self.commit_release.notified().await;
            }
        }
        self.inner.upsert(record, cancellation).await
    }

    /// 委托真实内存仓储回收记录，保证测试不留下伪恢复项。
    ///
    /// @author kongweiguang
    async fn remove(&self, id: &TransferId, cancellation: CancellationToken) -> AppResult<()> {
        self.inner.remove(id, cancellation).await
    }

    /// 归档行为委托现有仓储，测试记录器不扩展生命周期。
    ///
    /// @author kongweiguang
    async fn archive(&self, id: &TransferId, cancellation: CancellationToken) -> AppResult<()> {
        self.inner.archive(id, cancellation).await
    }

    /// 归档分页继续使用现有仓储的边界，避免影响传输用例。
    ///
    /// @author kongweiguang
    async fn list_archived(
        &self,
        cursor: Option<TransferId>,
        limit: usize,
        cancellation: CancellationToken,
    ) -> AppResult<TransferRecoveryPage> {
        self.inner.list_archived(cursor, limit, cancellation).await
    }
}

/// 使用可观察的持久化边界装配真实队列和后端，不构造假的提交许可。
///
/// @author kongweiguang
fn observed_service(
    backend: Arc<ObservedBackend>,
    recovery: Arc<RecordingRecovery>,
) -> AppResult<TransferService> {
    TransferService::new_with_recovery_repository(
        vec![backend],
        tokio::runtime::Handle::current(),
        TransferQueueOptions::default(),
        SftpTransferPolicyStore::new(SftpPerformanceSettings::default())?,
        recovery,
        None,
    )
}

/// 构造完全隔离的本机传输请求，目标覆盖策略与实际用户路径一致。
///
/// @author kongweiguang
fn local_request(source: PathBuf, destination: PathBuf) -> TransferRequest {
    TransferRequest {
        source: FileEndpoint::Local { path: source },
        destination: FileEndpoint::Local { path: destination },
        kind: TransferKind::File,
        conflict_policy: ConflictPolicy::Overwrite,
        idle_timeout_seconds: None,
    }
}

/// 等待权威终态并检查期间观察到的阶段，防止把队列受理当作传输完成。
///
/// @author kongweiguang
async fn await_terminal(service: &TransferService, id: &TransferId) -> AppResult<TransferSummary> {
    timeout(Duration::from_secs(30), async {
        loop {
            let summary = service.get(id).await?;
            assert_ne!(summary.phase, Some(TransferPhase::Verifying));
            if matches!(
                summary.status,
                TransferStatus::Succeeded | TransferStatus::Failed | TransferStatus::Cancelled
            ) {
                break Ok(summary);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .map_err(|_| AppError::timeout("测试传输未及时到达终态"))?
}

/// Windows 独占锁覆盖已确认前缀，使任何实际前缀读取失败，同时允许其他句柄读取尾部。
///
/// @author kongweiguang
#[cfg(windows)]
fn lock_prefix(path: &Path, length: u64) -> io::Result<std::fs::File> {
    use std::{
        io::Read,
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    };
    use windows_sys::Win32::{
        Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, LOCKFILE_EXCLUSIVE_LOCK,
            LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
        },
        System::IO::OVERLAPPED,
    };

    assert!(length > 0);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)?;
    let mut overlapped = OVERLAPPED::default();
    // SAFETY: 句柄和 OVERLAPPED 在同步加锁期间有效，锁范围由非零前缀长度限定。
    let locked = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            length as u32,
            (length >> 32) as u32,
            &mut overlapped,
        )
    };
    if locked == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut probe = std::fs::File::open(path)?;
    let mut byte = [0_u8; 1];
    assert!(
        probe.read_exact(&mut byte).is_err(),
        "前缀锁必须阻止独立句柄读取"
    );
    Ok(file)
}

/// 真实后端先流式暂停再从同一任务恢复；提交前锁住前缀并核对完整摘要与最终字节。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_checkpoint_resumes_real_transfer_without_scanning_prefix() -> AppResult<()> {
    let root = tempfile::tempdir().map_err(map_io_error)?;
    let source_path = root.path().join("source.bin");
    let destination_path = root.path().join("destination.bin");
    let mut payload = (0..251_u16)
        .map(|value| value as u8)
        .collect::<Vec<_>>()
        .repeat(64 * 1024 * 1024 / 251 + 1);
    payload.truncate(64 * 1024 * 1024);
    std::fs::write(&source_path, &payload).map_err(map_io_error)?;
    let backend = Arc::new(ObservedBackend {
        backend: Arc::new(SftpTransferBackend::new(Arc::new(NoRemoteConnections))),
        injected: None,
        paused: StdMutex::new(None),
    });
    let recovery = Arc::new(RecordingRecovery::default());
    let service = observed_service(backend.clone(), recovery.clone())?;
    let queued = service
        .enqueue(local_request(source_path.clone(), destination_path.clone()))
        .await?;
    timeout(Duration::from_secs(20), async {
        loop {
            let summary = service.get(&queued.id).await?;
            assert_ne!(summary.phase, Some(TransferPhase::Verifying));
            if summary.bytes_transferred >= CHECKPOINT_INTERVAL_BYTES {
                break Ok::<(), AppError>(());
            }
            assert!(!matches!(
                summary.status,
                TransferStatus::Succeeded | TransferStatus::Failed
            ));
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .map_err(|_| AppError::timeout("测试传输未及时产生断点"))??;
    service.cancel(&queued.id).await?;
    let paused = await_terminal(&service, &queued.id).await?;
    assert_eq!(paused.status, TransferStatus::Cancelled);
    assert!(paused.resumable);
    let checkpoint = backend
        .paused
        .lock()
        .expect("paused checkpoint lock")
        .clone()
        .expect("paused transfer returns a checkpoint");
    assert!(checkpoint.confirmed_bytes > 0);
    assert!(checkpoint.confirmed_bytes < payload.len() as u64);
    let FileEndpoint::Local { path: stage_path } = &checkpoint.staging_destination else {
        panic!("local transfer must use a local stage");
    };
    #[cfg(windows)]
    let source_lock =
        lock_prefix(&source_path, checkpoint.confirmed_bytes).map_err(map_io_error)?;
    #[cfg(windows)]
    let stage_lock = lock_prefix(stage_path, checkpoint.confirmed_bytes).map_err(map_io_error)?;
    #[cfg(not(windows))]
    let _ = stage_path;

    recovery.hold_commit.store(true, Ordering::SeqCst);
    service.resume(&queued.id).await?;
    timeout(Duration::from_secs(30), async {
        loop {
            tokio::select! {
                _ = recovery.commit_ready.notified() => break Ok::<(), AppError>(()),
                summary = service.get(&queued.id) => {
                    let summary = summary?;
                    assert_ne!(summary.phase, Some(TransferPhase::Verifying));
                    assert_ne!(summary.status, TransferStatus::Failed);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        }
    })
    .await
    .map_err(|_| AppError::timeout("续传未到达完整摘要提交屏障"))??;
    #[cfg(windows)]
    drop((source_lock, stage_lock));
    recovery.commit_release.notify_one();
    let completed = await_terminal(&service, &queued.id).await?;
    assert_eq!(completed.status, TransferStatus::Succeeded);
    assert_eq!(
        std::fs::read(&destination_path).map_err(map_io_error)?,
        payload
    );
    let full = recovery
        .full_checkpoint
        .lock()
        .expect("full checkpoint lock")
        .clone()
        .expect("commit records the full digest");
    let expected = sha256_hex(Sha256::digest(&payload).as_slice());
    assert_eq!(full.confirmed_bytes, payload.len() as u64);
    assert_eq!(full.confirmed_prefix_sha256, expected);
    assert_eq!(
        full.source_revision.content_digest.as_deref(),
        Some(expected.as_str())
    );
    service
        .shutdown_until(Instant::now() + Duration::from_secs(10))
        .await?;
    Ok(())
}

/// 缓存命中必须同时绑定源与完整断点；相同暂存的新检查点会替换旧哈希状态。
///
/// @author kongweiguang
#[test]
fn cache_rejects_forged_source_and_checkpoint_fields() {
    let cache = ResumeHashCache::default();
    let source = FileEndpoint::Local {
        path: "source".into(),
    };
    let mut checkpoint = verification_checkpoint_fixture();
    let mut hasher = Sha256::new();
    hasher.update(b"confirmed prefix");
    checkpoint.confirmed_prefix_sha256 = sha256_hex(hasher.clone().finalize().as_slice());
    cache.insert(&source, &checkpoint, &hasher);
    assert!(cache.get(&source, &checkpoint).is_some());
    let other_source = FileEndpoint::Local {
        path: "other-source".into(),
    };
    assert!(cache.get(&other_source, &checkpoint).is_none());
    for field in [
        "offset",
        "digest",
        "token",
        "owner",
        "revision",
        "destination",
        "stage",
    ] {
        let mut forged = checkpoint.clone();
        match field {
            "offset" => forged.confirmed_bytes += 1,
            "digest" => forged.confirmed_prefix_sha256 = "d".repeat(64),
            "token" => forged.staging_token = "e".repeat(32),
            "owner" => forged.staging_owner = TransferId::new(),
            "revision" => forged.source_revision.size += 1,
            "destination" => forged.actual_destination = other_source.clone(),
            "stage" => forged.staging_destination = other_source.clone(),
            _ => unreachable!(),
        }
        assert!(
            cache.get(&source, &forged).is_none(),
            "forged {field} must miss"
        );
    }
    hasher.update(b" followed by more bytes");
    let mut newer = checkpoint.clone();
    newer.confirmed_bytes += 1;
    newer.confirmed_prefix_sha256 = sha256_hex(hasher.clone().finalize().as_slice());
    cache.insert(&source, &newer, &hasher);
    assert!(cache.get(&source, &checkpoint).is_none());
    assert!(cache.get(&source, &newer).is_some());
}

/// 手工断点没有进程内哈希状态时，后端必须拒绝而不是重新扫描源或暂存。
/// 淘汰与缓存缺失均允许从头重试，源属性变化则拒绝续传。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_or_evicted_checkpoint_fails_without_prefix_reads() -> AppResult<()> {
    for scenario in ["missing", "evicted", "source_changed"] {
        let root = tempfile::tempdir().map_err(map_io_error)?;
        let source_path = root.path().join("source.bin");
        let destination_path = root.path().join("destination.bin");
        let original = vec![23_u8; COPY_BUFFER_BYTES * 2];
        let offset = COPY_BUFFER_BYTES;
        std::fs::write(&source_path, &original).map_err(map_io_error)?;
        std::fs::write(&destination_path, b"existing destination").map_err(map_io_error)?;
        let source = FileEndpoint::Local {
            path: source_path.clone(),
        };
        let destination = FileEndpoint::Local {
            path: destination_path.clone(),
        };
        let revision = EndpointSession::Local
            .metadata(&source)
            .await?
            .ok_or_else(|| AppError::not_found("测试源不存在"))?
            .revision();
        let source_identity = EndpointSession::Local.canonicalize_source(&source).await?;
        let owner = TransferId::new();
        let staging = create_staging_sibling(&destination, &owner)?;
        let token = "0123456789abcdef0123456789abcdef".to_owned();
        let artifact = StagingArtifact::new(
            staging.clone(),
            owner.clone(),
            token.clone(),
            StagingKind::File,
        )?;
        EndpointSession::Local
            .create_staging_marker(&artifact, &CancellationToken::new())
            .await?;
        let FileEndpoint::Local { path: stage_path } = &staging else {
            panic!("local destination must create a local stage");
        };
        std::fs::write(stage_path, &original[..offset]).map_err(map_io_error)?;
        let mut hasher = Sha256::new();
        hasher.update(&original[..offset]);
        let checkpoint = TransferCheckpoint {
            source_revision: revision,
            confirmed_bytes: offset as u64,
            confirmed_prefix_sha256: sha256_hex(hasher.clone().finalize().as_slice()),
            staging_owner: owner,
            staging_destination: staging.clone(),
            staging_token: token,
            actual_destination: destination,
        };
        let real_backend = Arc::new(SftpTransferBackend::new(Arc::new(NoRemoteConnections)));
        if scenario != "missing" {
            real_backend
                .resume_hashes
                .insert(&source_identity, &checkpoint, &hasher);
        }
        if scenario == "evicted" {
            for index in 0..8192 {
                let mut other = checkpoint.clone();
                other.staging_destination = FileEndpoint::Local {
                    path: root.path().join(format!("other-{index}.partial")),
                };
                real_backend
                    .resume_hashes
                    .insert(&source_identity, &other, &hasher);
            }
            assert!(
                real_backend
                    .resume_hashes
                    .get(&source_identity, &checkpoint)
                    .is_none()
            );
        }
        if scenario == "source_changed" {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&source_path)
                .and_then(|file| file.set_len(original.len() as u64 + 1))
                .map_err(map_io_error)?;
        }
        #[cfg(windows)]
        let source_lock = lock_prefix(&source_path, offset as u64).map_err(map_io_error)?;
        #[cfg(windows)]
        let stage_lock = lock_prefix(stage_path, offset as u64).map_err(map_io_error)?;
        let backend = Arc::new(ObservedBackend {
            backend: real_backend,
            injected: Some(checkpoint),
            paused: StdMutex::new(None),
        });
        let service = observed_service(backend, Arc::new(RecordingRecovery::default()))?;
        let queued = service
            .enqueue(local_request(source_path.clone(), destination_path.clone()))
            .await?;
        let result = await_terminal(&service, &queued.id).await?;
        assert_eq!(result.status, TransferStatus::Failed, "{scenario}");
        assert_eq!(result.retryable, scenario != "source_changed", "{scenario}");
        assert!(!result.resumable, "{scenario}");
        assert!(!result.cleanup_pending, "{scenario}");
        #[cfg(windows)]
        drop((source_lock, stage_lock));
        let source_bytes = std::fs::read(&source_path).map_err(map_io_error)?;
        assert_eq!(&source_bytes[..original.len()], original.as_slice());
        assert_eq!(
            source_bytes.len(),
            original.len() + usize::from(scenario == "source_changed")
        );
        assert_eq!(
            std::fs::read(&destination_path).map_err(map_io_error)?,
            b"existing destination"
        );
        assert_eq!(
            stage_path.exists(),
            scenario == "source_changed",
            "{scenario}"
        );
        service
            .shutdown_until(Instant::now() + Duration::from_secs(10))
            .await?;
    }
    Ok(())
}
