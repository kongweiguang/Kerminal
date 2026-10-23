//! 可靠传输的断点身份、确认偏移和实际目标记录。
//!
//! 断点不能把 partial 文件长度当作安全偏移：并发写入可能已经落盘但尚未被客户端确认，
//! 因此这里只在整批请求收到确认后推进 `confirmed_offset`。记录同时保存源文件身份和实际
//! rename 目标，重试时可以拒绝源文件变化并避免目标名称漂移。
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};

/// 参与断点恢复校验的源文件稳定身份。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(in crate::services::sftp_service) struct RecoverySourceFingerprint {
    /// 源文件的完整大小。
    pub(in crate::services::sftp_service) size: u64,
    /// 源文件修改时间（秒）；远端 SFTP v3 精度只到秒，因此统一使用秒。
    pub(in crate::services::sftp_service) mtime_seconds: Option<u64>,
    /// 源文件前缀 SHA-256，避免把文件内容写进 checkpoint。
    pub(in crate::services::sftp_service) prefix_sha256: String,
    /// 摘要覆盖的前缀长度；恢复时必须校验 `[0, confirmed_offset)` 全段。
    pub(in crate::services::sftp_service) prefix_length: u64,
}

/// 单个文件的可恢复断点。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(in crate::services::sftp_service) struct RecoveryCheckpointRecord {
    /// 首次选择后固定的正式目标路径。
    pub(in crate::services::sftp_service) actual_target: String,
    /// 临时 partial 路径。
    pub(in crate::services::sftp_service) partial_path: String,
    /// 已经得到服务端/本地写入确认的连续偏移。
    pub(in crate::services::sftp_service) confirmed_offset: u64,
    /// 源文件身份。
    pub(in crate::services::sftp_service) source: RecoverySourceFingerprint,
    /// 是否已经确认正式提交；提交后不会再次写入同一记录。
    pub(in crate::services::sftp_service) committed: bool,
    /// 非原子覆盖事务中暂存原目标的同目录路径；供 future 被取消后继续恢复。
    #[serde(default)]
    pub(in crate::services::sftp_service) backup_path: Option<String>,
    /// 上传内容的完整摘要；未知提交结果只在正式目标与它完全一致时收敛成功。
    #[serde(default)]
    pub(in crate::services::sftp_service) expected_sha256: Option<String>,
}

/// 任务级恢复记录集合；backend 可把它放进 `TransferTask`，自动恢复和人工 retry 共享。
#[derive(Clone, Debug, Default)]
pub(in crate::services::sftp_service) struct TransferRecoveryState {
    records: HashMap<String, RecoveryCheckpointRecord>,
    directories: HashMap<String, String>,
}

/// 传给 `TransferProgress`/`TransferTask` 的轻量 checkpoint holder。
pub(in crate::services::sftp_service) type RecoveryCheckpoints = Arc<Mutex<TransferRecoveryState>>;

impl TransferRecoveryState {
    /// 人工重试新任务从可信连续偏移初始化进度；不能读取 partial 长度，因为尾部可能未确认。
    pub(in crate::services::sftp_service) fn confirmed_bytes(&self) -> u64 {
        self.records.values().fold(0_u64, |total, record| {
            total.saturating_add(record.confirmed_offset.min(record.source.size))
        })
    }

    /// 提交成功的单文件允许调用方确认取消竞态；空记录绝不当作已提交。
    pub(in crate::services::sftp_service) fn record_committed(&self, key: &str) -> bool {
        self.records.get(key).is_some_and(|record| record.committed)
    }

    /// 目录 rename 的实际根目录固定在任务内，重试不会漂移到另一个编号。
    pub(in crate::services::sftp_service) fn directory(&self, key: &str) -> Option<String> {
        self.directories.get(key).cloned()
    }

    /// 首次选择的目录是子文件 checkpoint 的稳定路径来源。
    pub(in crate::services::sftp_service) fn remember_directory(
        &mut self,
        key: String,
        target: String,
    ) {
        self.directories.entry(key).or_insert(target);
    }
    /// 只在至少一个文件已登记且全部提交时允许取消与提交竞态收敛成成功。
    pub(in crate::services::sftp_service) fn all_committed(&self) -> bool {
        !self.records.is_empty() && self.records.values().all(|record| record.committed)
    }

    /// 自动恢复可从零确认偏移重新建立文件，但必须有完整源身份和连续前缀记录。
    pub(in crate::services::sftp_service) fn has_safe_recovery_checkpoint(&self) -> bool {
        self.records.values().any(|record| {
            !record.committed
                && record.source.mtime_seconds.is_some()
                && record.confirmed_offset <= record.source.size
                && record.source.prefix_length == record.confirmed_offset
                && valid_digest(&record.source.prefix_sha256)
        })
    }

    /// UI 只有实际确认过非零字节时才承诺“续传”；零偏移只能重新传输。
    pub(in crate::services::sftp_service) fn has_resumable(&self) -> bool {
        self.records.values().any(|record| {
            !record.committed
                && record.confirmed_offset > 0
                && record.confirmed_offset <= record.source.size
                && record.source.mtime_seconds.is_some()
                && record.source.prefix_length == record.confirmed_offset
                && valid_digest(&record.source.prefix_sha256)
        })
    }
    /// 创建空状态；任务入队时创建一次，后续同 ID 自动恢复不重置。
    pub(in crate::services::sftp_service) fn new() -> Self {
        Self::default()
    }

    /// 返回逻辑文件 key 对应的记录快照，避免调用方长期持有锁。
    pub(in crate::services::sftp_service) fn snapshot(
        &self,
        key: &str,
    ) -> Option<RecoveryCheckpointRecord> {
        self.records.get(key).cloned()
    }

    /// 仅首次登记文件；重连不得以未经确认的新记录覆盖已验证摘要或实际目标。
    pub(in crate::services::sftp_service) fn upsert(
        &mut self,
        key: impl Into<String>,
        record: RecoveryCheckpointRecord,
    ) {
        let key = key.into();
        if self.records.contains_key(&key) {
            return;
        }
        self.records.insert(key, record);
    }

    /// 在首个远端 rename 前登记事务三条路径/内容身份，监督器丢弃 future 后仍可安全恢复。
    pub(in crate::services::sftp_service) fn remember_commit_attempt(
        &mut self,
        key: &str,
        backup_path: Option<String>,
        expected_sha256: String,
    ) {
        if let Some(record) = self.records.get_mut(key) {
            record.backup_path = backup_path;
            record.expected_sha256 = Some(expected_sha256);
        }
    }

    /// 确认回滚或提交收敛后移除事务 journal，避免后继重试误处理已结束的备份。
    pub(in crate::services::sftp_service) fn clear_commit_backup(&mut self, key: &str) {
        if let Some(record) = self.records.get_mut(key) {
            record.backup_path = None;
            record.expected_sha256 = None;
        }
    }

    /// 批量 ACK 后同时推进偏移和整个已确认前缀摘要，避免只验证前 64 KiB。
    pub(in crate::services::sftp_service) fn record_confirmed_batch_with_prefix(
        &mut self,
        key: &str,
        confirmed_offset: u64,
        prefix_sha256: String,
    ) {
        if let Some(record) = self.records.get_mut(key) {
            if !record.committed
                && confirmed_offset > record.confirmed_offset
                && confirmed_offset <= record.source.size
                && valid_digest(&prefix_sha256)
            {
                record.confirmed_offset = confirmed_offset;
                record.source.prefix_length = confirmed_offset;
                record.source.prefix_sha256 = prefix_sha256;
            }
        }
    }

    /// 标记正式提交完成，迟到的取消/失败回调不能把它重新变成可重试 partial。
    pub(in crate::services::sftp_service) fn mark_committed(&mut self, key: &str) {
        if let Some(record) = self.records.get_mut(key) {
            record.committed = true;
            record.confirmed_offset = record.source.size;
        }
    }
}

/// 摘要必须是完整 SHA-256 十六进制；内容是否匹配仍由恢复前的全前缀重读确认。
fn valid_digest(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// 创建一个可由任务状态和 IO helper 共享的 holder。
pub(in crate::services::sftp_service) fn new_recovery_checkpoints() -> RecoveryCheckpoints {
    Arc::new(Mutex::new(TransferRecoveryState::new()))
}

/// 用稳定字段生成单文件 key；不包含凭据或主机私密信息。
pub(in crate::services::sftp_service) fn recovery_key(
    source_label: &str,
    actual_target: &str,
) -> String {
    format!("{source_label}\n{actual_target}")
}

#[cfg(test)]
mod tests {
    use super::{RecoveryCheckpointRecord, RecoverySourceFingerprint, TransferRecoveryState};

    /// 零确认可安全重建但不能向用户承诺从已有字节续传；ACK 后两种判断才同时成立。
    #[test]
    fn zero_offset_recovery_is_not_resumable() {
        let mut state = TransferRecoveryState::new();
        state.upsert(
            "file",
            RecoveryCheckpointRecord {
                actual_target: "target".into(),
                partial_path: "target.kerminal-part".into(),
                confirmed_offset: 0,
                source: RecoverySourceFingerprint {
                    size: 10,
                    mtime_seconds: Some(123),
                    prefix_sha256:
                        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
                    prefix_length: 0,
                },
                committed: false,
                backup_path: None,
                expected_sha256: None,
            },
        );
        assert!(state.has_safe_recovery_checkpoint());
        assert!(!state.has_resumable());
        assert_eq!(state.confirmed_bytes(), 0);

        state.record_confirmed_batch_with_prefix("file", 4, "a".repeat(64));
        assert!(state.has_resumable());
        assert_eq!(state.confirmed_bytes(), 4);
        state.record_confirmed_batch_with_prefix("file", 2, "b".repeat(64));
        assert_eq!(
            state.snapshot("file").expect("record").source.prefix_sha256,
            "a".repeat(64)
        );
        state.mark_committed("file");
        assert!(!state.has_safe_recovery_checkpoint());
        assert!(!state.has_resumable());
    }
}
