//! 进程内续传哈希状态，只缓存已经同步的单文件检查点。
//!
//! @author kongweiguang

use std::{collections::HashMap, sync::Mutex};

use kerminal_application::transfers::TransferCheckpoint;
use kerminal_domain::files::FileEndpoint;
use sha2::{Digest, Sha256};

use super::{StagingArtifact, sha256_hex};

const MAX_RESUME_HASHES: usize = 8192;

#[derive(Default)]
pub(super) struct ResumeHashCache {
    entries: Mutex<ResumeHashEntries>,
}

#[derive(Default)]
struct ResumeHashEntries {
    next_sequence: u64,
    by_staging: HashMap<FileEndpoint, ResumeHashEntry>,
}

struct ResumeHashEntry {
    source: FileEndpoint,
    checkpoint: TransferCheckpoint,
    hasher: Sha256,
    sequence: u64,
}

impl ResumeHashCache {
    /// 同一暂存只保留最后一个同步点，容量耗尽时淘汰最早记录并让后续调用从头重试。
    ///
    /// @author kongweiguang
    pub(super) fn insert(
        &self,
        source: &FileEndpoint,
        checkpoint: &TransferCheckpoint,
        hasher: &Sha256,
    ) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !entries
            .by_staging
            .contains_key(&checkpoint.staging_destination)
            && entries.by_staging.len() >= MAX_RESUME_HASHES
            && let Some(oldest) = entries
                .by_staging
                .iter()
                .min_by_key(|(_, entry)| entry.sequence)
                .map(|(key, _)| key.clone())
        {
            entries.by_staging.remove(&oldest);
        }
        let sequence = entries.next_sequence;
        entries.next_sequence = entries.next_sequence.saturating_add(1);
        entries.by_staging.insert(
            checkpoint.staging_destination.clone(),
            ResumeHashEntry {
                source: source.clone(),
                checkpoint: checkpoint.clone(),
                hasher: hasher.clone(),
                sequence,
            },
        );
    }

    /// 必须同时匹配规范源、完整断点和哈希状态，不能仅凭暂存 token 恢复数据流。
    ///
    /// @author kongweiguang
    pub(super) fn get(
        &self,
        source: &FileEndpoint,
        checkpoint: &TransferCheckpoint,
    ) -> Option<Sha256> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = entries.by_staging.get(&checkpoint.staging_destination)?;
        (entry.source == *source
            && entry.checkpoint == *checkpoint
            && sha256_hex(entry.hasher.clone().finalize().as_slice())
                == checkpoint.confirmed_prefix_sha256)
            .then(|| entry.hasher.clone())
    }

    /// 只回收本次已登记 owner 的状态；可恢复的返回断点必须与缓存完整一致。
    ///
    /// @author kongweiguang
    pub(super) fn finish_attempt(
        &self,
        artifacts: &[StagingArtifact],
        original: Option<&TransferCheckpoint>,
        retained: Option<&TransferCheckpoint>,
    ) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.by_staging.retain(|_, entry| {
            let belongs_to_attempt = artifacts.iter().any(|artifact| {
                artifact.endpoint == entry.checkpoint.staging_destination
                    && artifact.owner == entry.checkpoint.staging_owner
                    && artifact.token == entry.checkpoint.staging_token
            }) || original
                .is_some_and(|checkpoint| checkpoint == &entry.checkpoint);
            !belongs_to_attempt
                || retained.is_some_and(|checkpoint| checkpoint == &entry.checkpoint)
        });
    }
}
