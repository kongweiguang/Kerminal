//! 大文件取消、可信 checkpoint 重试与完整远端内容对账。
//!
//! @author kongweiguang

use std::{
    fs::File,
    io::{self, Read, Write},
    path::Path,
    time::{Duration, Instant as WallClockInstant},
};

use kerminal_application::AppError;
use kerminal_domain::files::{ConflictPolicy, FileEndpoint, TransferKind, TransferStatus};
use sha2::{Digest, Sha256};
use tokio::time::{sleep, timeout};

use super::{
    AcceptanceResult, CANCELLATION_CHECKPOINT_BYTES, HEARTBEAT_INTERVAL, LARGE_FILE_BYTES,
    OPERATION_TIMEOUT, SftpAcceptanceFixture, log_transfer_snapshot, remote_child, request,
    require, run_stage, run_transfer, wait_for_terminal,
};
/// 取消一份大文件并用服务端可信 checkpoint 重试，随后下载并逐字节摘要校验。
///
/// @author kongweiguang
pub(super) async fn exercise_cancel_and_trusted_resume(
    fixture: &SftpAcceptanceFixture,
) -> AcceptanceResult {
    let source = fixture.local_root.path().join("large-resume.bin");
    let expected_digest = run_stage("resume.prepare_local_source", async {
        Ok(create_large_file(&source, LARGE_FILE_BYTES)?)
    })
    .await?;
    let remote_file = remote_child(&fixture.host_id, &fixture.workspace, "large-resume.bin")?;
    let first = run_stage("resume.enqueue_initial_upload", async {
        Ok(fixture
            .transfers
            .enqueue(request(
                FileEndpoint::Local {
                    path: source.clone(),
                },
                remote_file.clone(),
                TransferKind::File,
                ConflictPolicy::Overwrite,
            ))
            .await?)
    })
    .await?;

    let id = first.id.clone();
    run_stage("resume.wait_for_cancel_checkpoint", async {
        let started = WallClockInstant::now();
        timeout(OPERATION_TIMEOUT, async {
            let mut next_heartbeat = tokio::time::Instant::now() + HEARTBEAT_INTERVAL;
            loop {
                let snapshot = fixture.transfers.get(&id).await?;
                if snapshot.bytes_transferred >= CANCELLATION_CHECKPOINT_BYTES
                    && snapshot.bytes_transferred < LARGE_FILE_BYTES
                    && snapshot.status == TransferStatus::Transferring
                {
                    fixture.transfers.cancel(&id).await?;
                    let cancelling = fixture.transfers.get(&id).await?;
                    log_transfer_snapshot(
                        "resume.wait_for_cancel_checkpoint",
                        "cancel_requested",
                        started.elapsed(),
                        &cancelling,
                    );
                    return Ok::<(), AppError>(());
                }
                if matches!(
                    snapshot.status,
                    TransferStatus::Succeeded
                        | TransferStatus::Failed
                        | TransferStatus::Cancelled
                        | TransferStatus::Skipped
                        | TransferStatus::Unchanged
                ) {
                    return Err(AppError::conflict(
                        "large transfer reached a terminal state before cancellation",
                    ));
                }
                let now = tokio::time::Instant::now();
                if now >= next_heartbeat {
                    log_transfer_snapshot(
                        "resume.wait_for_cancel_checkpoint",
                        "progress",
                        started.elapsed(),
                        &snapshot,
                    );
                    next_heartbeat = now + HEARTBEAT_INTERVAL;
                }
                sleep(Duration::from_millis(2)).await;
            }
        })
        .await??;
        Ok(())
    })
    .await?;
    let cancelled = wait_for_terminal(&fixture.transfers, &id, "resume.cancelled_attempt").await?;
    run_stage("resume.verify_cancel_checkpoint", async {
        require(
            cancelled.status == TransferStatus::Cancelled && cancelled.resumable,
            "cancelled transfer did not retain a trusted resumable checkpoint",
        )
    })
    .await?;
    let retry = run_stage("resume.enqueue_retry", async {
        Ok(fixture.transfers.retry(&id).await?)
    })
    .await?;
    let resumed = wait_for_terminal(&fixture.transfers, &retry.id, "resume.retry_upload").await?;
    run_stage("resume.verify_upload_result", async {
        require(
            resumed.status == TransferStatus::Succeeded,
            "trusted SFTP resume did not complete",
        )?;
        require(
            resumed.bytes_transferred == LARGE_FILE_BYTES,
            "resumed transfer byte count does not match the source size",
        )
    })
    .await?;

    let downloaded = fixture.local_root.path().join("large-resumed-download.bin");
    let download = run_transfer(
        fixture,
        "resume.download_resumed_file",
        request(
            remote_file,
            FileEndpoint::Local {
                path: downloaded.clone(),
            },
            TransferKind::File,
            ConflictPolicy::Overwrite,
        ),
    )
    .await?;
    run_stage("resume.verify_download_and_digest", async {
        require(
            download.status == TransferStatus::Succeeded,
            "resumed file download failed",
        )?;
        require(
            sha256_file(&downloaded)? == expected_digest,
            "resumed remote file digest differs from the original source",
        )
    })
    .await?;
    Ok(())
}
/// 流式生成固定长度且含全部字节值的测试源文件，避免为大文件同时保留两份内存缓冲。
fn create_large_file(path: &Path, length: u64) -> io::Result<String> {
    let mut file = File::create(path)?;
    let mut digest = Sha256::new();
    let mut block = [0_u8; 64 * 1024];
    for (index, byte) in block.iter_mut().enumerate() {
        *byte = ((index * 73 + index / 251) % 256) as u8;
    }
    let mut written = 0_u64;
    while written < length {
        let count = (length - written).min(block.len() as u64) as usize;
        file.write_all(&block[..count])?;
        digest.update(&block[..count]);
        written += count as u64;
    }
    file.sync_all()?;
    Ok(format!("{:x}", digest.finalize()))
}

/// 以固定内存块计算对账摘要，使断点恢复验收能够覆盖整个远端文件。
fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
