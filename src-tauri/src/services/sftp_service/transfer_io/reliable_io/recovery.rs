//! recovery reliable transfer helpers.
//! @author kongweiguang

use super::*;

/// 将文件修改时间转换为 SFTP v3 可比较的秒级身份字段。
fn mtime_seconds(metadata: &std::fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
}

/// 计算本地源文件身份；前缀摘要由传输过程按已确认偏移持续更新。
pub(in crate::services::sftp_service) async fn local_source_fingerprint(
    path: &Path,
) -> AppResult<RecoverySourceFingerprint> {
    let metadata = fs::metadata(path).await?;
    Ok(RecoverySourceFingerprint {
        size: metadata.len(),
        mtime_seconds: mtime_seconds(&metadata),
        prefix_sha256: empty_prefix_digest(),
        prefix_length: 0,
    })
}

/// 计算已打开远端源文件的身份并恢复其读指针，摘要在实际传输读取时更新。
pub(in crate::services::sftp_service) async fn remote_source_fingerprint(
    file: &mut SftpFile,
    metadata: &russh_sftp::client::fs::Metadata,
) -> AppResult<RecoverySourceFingerprint> {
    let _ = file;
    Ok(RecoverySourceFingerprint {
        size: metadata
            .size
            .ok_or_else(|| AppError::Sftp("远端源文件缺少大小信息，无法安全传输".into()))?,
        mtime_seconds: metadata.mtime.map(u64::from),
        prefix_sha256: empty_prefix_digest(),
        prefix_length: 0,
    })
}

/// 只输出十六进制摘要，避免 checkpoint 暴露源文件内容。
pub(in crate::services::sftp_service) fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 返回空前缀的确定性摘要，便于 offset=0 的初始 checkpoint 校验。
pub(in crate::services::sftp_service) fn empty_prefix_digest() -> String {
    hex_digest(&Sha256::digest([]))
}

/// 比较源的静态身份；前缀摘要由 `validate_source_prefix` 单独覆盖完整确认范围。
pub(in crate::services::sftp_service) fn source_identity_matches(
    expected: &RecoverySourceFingerprint,
    actual: &RecoverySourceFingerprint,
) -> bool {
    expected.size == actual.size && expected.mtime_seconds == actual.mtime_seconds
}

/// 将摘要复制为 checkpoint 可存储的脱敏十六进制字符串。
pub(in crate::services::sftp_service) fn sha256_hex_digest(hasher: &Sha256) -> String {
    hex_digest(&hasher.clone().finalize())
}

/// 对异步源读取指定前缀并返回 SHA-256；用于恢复前的完整前缀校验。
async fn hash_async_reader<R>(reader: &mut R, bytes: u64) -> AppResult<[u8; 32]>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut remaining = bytes;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; DOWNLOAD_READ_CHUNK_BYTES];
    while remaining > 0 {
        let limit = remaining.min(buffer.len() as u64) as usize;
        let read = reader.read(&mut buffer[..limit]).await?;
        if read == 0 {
            return Err(AppError::Sftp(
                "可靠传输源文件在断点校验期间提前结束".to_owned(),
            ));
        }
        hasher.update(&buffer[..read]);
        remaining = remaining.saturating_sub(read as u64);
    }
    Ok(hasher.finalize().into())
}

/// 提交回执丢失时，对正式文件的完整大小与 SHA-256 对账；只有全量确认记录才可判定成功。
pub(in crate::services::sftp_service) async fn remote_final_matches_checkpoint(
    sftp: &SftpSession,
    record: &RecoveryCheckpointRecord,
) -> AppResult<bool> {
    if record.confirmed_offset != record.source.size
        || record.source.prefix_length != record.source.size
    {
        return Ok(false);
    }
    let metadata = match sftp.symlink_metadata(record.actual_target.clone()).await {
        Ok(metadata) => metadata,
        Err(error) if is_no_such_file_error(&error) => return Ok(false),
        Err(error) => return Err(native_sftp_error(error)),
    };
    if !metadata.file_type().is_file() || metadata.size != Some(record.source.size) {
        return Ok(false);
    }
    let mut file = sftp
        .open(record.actual_target.clone())
        .await
        .map_err(native_sftp_error)?;
    let digest = hash_async_reader(&mut file, record.source.size).await?;
    Ok(hex_digest(&digest) == record.source.prefix_sha256)
}

/// 本地原子替换也可能已成功但应答被取消；重试前按全量摘要确认实际磁盘结果。
pub(in crate::services::sftp_service) async fn local_final_matches_checkpoint(
    record: &RecoveryCheckpointRecord,
) -> AppResult<bool> {
    if record.confirmed_offset != record.source.size
        || record.source.prefix_length != record.source.size
    {
        return Ok(false);
    }
    let metadata = match fs::metadata(&record.actual_target).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if metadata.len() != record.source.size {
        return Ok(false);
    }
    let mut file = fs::File::open(&record.actual_target).await?;
    let digest = hash_async_reader(&mut file, record.source.size).await?;
    Ok(hex_digest(&digest) == record.source.prefix_sha256)
}
