//! Single-file replacement permission and read-only checkpoint regressions.
//!
//! @author kongweiguang

use std::{fs, sync::Arc, time::Duration};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        ConnectionIntent, HostKeyChallenge, HostKeyPeer, HostKeyRotationReceipt,
        HostKeyRotationRequest, HostKeyTrustReceipt, HostKeyTrustRequest, RemoteConnectionLease,
        RemoteConnectionService,
    },
    transfers::{
        TransferBackend, TransferExecutionContext, TransferExecutionError, TransferExecutionResult,
        TransferQueueOptions, TransferService,
    },
};
use kerminal_domain::{
    files::{
        ConflictPolicy, FileEndpoint, FileEntryKind, TransferKind, TransferRequest, TransferStatus,
    },
    identity::{HostId, TransferId},
};
use sha2::{Digest, Sha256};
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;

use super::*;

/// 覆盖使用目标明示的 POSIX mode，新建才继承源 mode；权限缺失时拒绝猜测。
///
/// @author kongweiguang
#[test]
fn overwrite_permissions_preserve_target_and_new_files_inherit_source() -> AppResult<()> {
    for target_mode in [0o600, 0o755] {
        let existing = ObjectMetadata {
            kind: FileEntryKind::File,
            size: 0,
            modified: None,
            modified_seconds: None,
            permissions: Some(target_mode),
        };
        assert_eq!(
            file_staging_permissions(Some(0o644), Some(&existing))?,
            Some(target_mode)
        );
    }

    assert_eq!(
        file_staging_permissions(Some(0o644), None)?,
        Some(0o644),
        "new files inherit source permissions"
    );
    assert_eq!(
        file_staging_permissions(None, None)?,
        None,
        "unknown source permissions remain unknown"
    );

    let unknown_target = ObjectMetadata {
        kind: FileEntryKind::File,
        size: 0,
        modified: None,
        modified_seconds: None,
        permissions: None,
    };
    let error = file_staging_permissions(Some(0o644), Some(&unknown_target))
        .expect_err("an existing target without mode must not inherit guessed source permissions");
    assert_eq!(error.public_code(), PublicErrorCode::Unsupported);
    Ok(())
}

/// owner-write 缺失时即使 mode 带 group-write 也须补 owner 位，避免依赖未知组成员关系。
///
/// @author kongweiguang
#[tokio::test]
async fn checkpoint_write_recovery_does_not_assume_group_membership() -> AppResult<()> {
    let root = tempfile::tempdir().map_err(map_io_error)?;
    let path = root.path().join("group-writable.partial");
    fs::write(&path, b"checkpoint").map_err(map_io_error)?;
    let staging = FileEndpoint::Local { path };
    EndpointSession::Local
        .set_permissions(&staging, Some(0o022))
        .await?;

    make_checkpoint_staging_writable(&EndpointSession::Local, &staging, Some(0o022)).await?;
    let metadata = EndpointSession::Local
        .metadata(&staging)
        .await?
        .ok_or_else(|| AppError::not_found("测试 checkpoint 暂存文件不存在"))?;
    assert_ne!(
        metadata.permissions.unwrap_or_default() & 0o200,
        0,
        "owner-write is needed even when group-write is present"
    );
    Ok(())
}

struct LocalOnlyConnections;

#[async_trait]
impl RemoteConnectionService for LocalOnlyConnections {
    /// 续传回归只访问本机端点，任何远端连接尝试都应失败关闭。
    ///
    /// @author kongweiguang
    async fn connect(
        &self,
        _host_id: HostId,
        _intent: ConnectionIntent,
        _cancel: CancellationToken,
    ) -> AppResult<RemoteConnectionLease> {
        Err(AppError::unsupported("本机续传测试不连接远端"))
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
        Err(AppError::unsupported("本机续传测试不读取远端密钥"))
    }

    /// 本机续传不修改远端主机信任。
    ///
    /// @author kongweiguang
    async fn trust_host_key(
        &self,
        _request: HostKeyTrustRequest,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyTrustReceipt> {
        Err(AppError::unsupported("本机续传测试不修改远端信任"))
    }

    /// 本机续传不触发远端主机密钥轮换。
    ///
    /// @author kongweiguang
    async fn replace_host_key(
        &self,
        _request: HostKeyRotationRequest,
        _cancel: CancellationToken,
    ) -> AppResult<HostKeyRotationReceipt> {
        Err(AppError::unsupported("本机续传测试不轮换远端密钥"))
    }
}

struct ResumeFixtureBackend {
    backend: SftpTransferBackend,
    checkpoint: TransferCheckpoint,
}

#[async_trait]
impl TransferBackend for ResumeFixtureBackend {
    /// 复用生产后端的真实续传流程，只由 fixture 注入已登记的 checkpoint。
    ///
    /// @author kongweiguang
    fn supports(&self, request: &TransferRequest) -> bool {
        self.backend.supports(request)
    }

    /// 使用队列创建的执行上下文，保留真实提交许可与资源清理路径。
    ///
    /// @author kongweiguang
    async fn execute(
        &self,
        mut context: TransferExecutionContext,
    ) -> Result<TransferExecutionResult, TransferExecutionError> {
        context.resume_from = Some(self.checkpoint.clone());
        context.actual_destination = Some(self.checkpoint.actual_destination.clone());
        self.backend.execute(context).await
    }
}

/// 用已同步的摘要缓存从不可读/只读暂存恢复，只补写权限并在原子发布前还原目标权限。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_checkpoint_resumes_and_commits_complete_source() -> AppResult<()> {
    for staging_mode in [0o000, 0o200, 0o040, 0o444] {
        let root = tempfile::tempdir().map_err(map_io_error)?;
        let source_path = root.path().join("source.bin");
        let destination_path = root.path().join("destination.bin");
        let original = (0..COPY_BUFFER_BYTES / 2 + 31)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        fs::write(&source_path, &original).map_err(map_io_error)?;
        fs::write(&destination_path, b"old destination").map_err(map_io_error)?;

        let source = FileEndpoint::Local {
            path: source_path.clone(),
        };
        let destination = FileEndpoint::Local {
            path: destination_path.clone(),
        };
        let original_destination_permissions = EndpointSession::Local
            .metadata(&destination)
            .await?
            .ok_or_else(|| AppError::not_found("测试目标不存在"))?
            .permissions;
        let source_metadata = EndpointSession::Local
            .metadata(&source)
            .await?
            .ok_or_else(|| AppError::not_found("测试源不存在"))?;
        let mut source_revision = source_metadata.revision();
        source_revision.content_digest = Some(sha256_hex(Sha256::digest(&original).as_slice()));

        let confirmed_bytes = original.len() / 2;
        let owner = TransferId::new();
        let staging = create_staging_sibling(&destination, &owner)?;
        let staging_token = "0123456789abcdef0123456789abcdef".to_owned();
        let artifact = StagingArtifact::new(
            staging.clone(),
            owner.clone(),
            staging_token.clone(),
            StagingKind::File,
        )?;
        EndpointSession::Local
            .create_staging_marker(&artifact, &CancellationToken::new())
            .await?;
        let FileEndpoint::Local { path: staging_path } = &staging else {
            unreachable!("local destination creates a local stage")
        };
        fs::write(staging_path, &original[..confirmed_bytes]).map_err(map_io_error)?;
        EndpointSession::Local
            .set_permissions(&staging, Some(staging_mode))
            .await?;
        let staged_metadata = EndpointSession::Local
            .metadata(&staging)
            .await?
            .ok_or_else(|| AppError::not_found("测试暂存文件不存在"))?;
        #[cfg(unix)]
        assert_eq!(
            staged_metadata.permissions.map(|mode| mode & 0o777),
            Some(staging_mode),
            "fixture must begin with the requested POSIX staging mode"
        );
        #[cfg(not(unix))]
        assert_eq!(
            staged_metadata.permissions.map(|mode| mode & 0o222 == 0),
            Some(staging_mode & 0o222 == 0),
            "fixture must reflect the requested Windows read-only state"
        );

        let checkpoint = TransferCheckpoint {
            source_revision,
            confirmed_bytes: confirmed_bytes as u64,
            confirmed_prefix_sha256: sha256_hex(
                Sha256::digest(&original[..confirmed_bytes]).as_slice(),
            ),
            staging_owner: owner,
            staging_destination: staging,
            staging_token,
            actual_destination: destination.clone(),
        };
        let backend = SftpTransferBackend::new(Arc::new(LocalOnlyConnections));
        let canonical_source = EndpointSession::Local.canonicalize_source(&source).await?;
        let mut hasher = Sha256::new();
        hasher.update(&original[..confirmed_bytes]);
        backend
            .resume_hashes
            .insert(&canonical_source, &checkpoint, &hasher);
        let backend = ResumeFixtureBackend {
            backend,
            checkpoint,
        };
        let service = TransferService::new(
            vec![Arc::new(backend)],
            tokio::runtime::Handle::current(),
            TransferQueueOptions::default(),
        )?;
        let queued = service
            .enqueue(TransferRequest {
                source,
                destination: destination.clone(),
                kind: TransferKind::File,
                conflict_policy: ConflictPolicy::Overwrite,
                idle_timeout_seconds: None,
            })
            .await?;
        let summary = timeout(Duration::from_secs(10), async {
            loop {
                let summary = service.get(&queued.id).await?;
                if matches!(
                    summary.status,
                    TransferStatus::Succeeded | TransferStatus::Failed
                ) {
                    break Ok::<_, AppError>(summary);
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .map_err(|_| AppError::infrastructure("只读 checkpoint 续传未及时结束"))??;
        let destination_bytes = fs::read(&destination_path).map_err(map_io_error)?;
        let final_destination_permissions = EndpointSession::Local
            .metadata(&destination)
            .await?
            .ok_or_else(|| AppError::not_found("提交后的测试目标不存在"))?
            .permissions;
        service
            .shutdown_until(Instant::now() + Duration::from_secs(10))
            .await?;

        assert_eq!(
            summary.status,
            TransferStatus::Succeeded,
            "read-only staging mode {staging_mode:o} resumes"
        );
        assert_eq!(destination_bytes, original);
        assert_eq!(
            final_destination_permissions, original_destination_permissions,
            "the published target retains its pre-transfer mode"
        );
    }
    Ok(())
}
