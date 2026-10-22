//! 传输中断恢复只保存可核对事实，不序列化连接租约、凭据或运行时清理对象。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use kerminal_domain::{
    files::{FileEndpoint, TransferRequest},
    identity::TransferId,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::TransferCheckpoint;
use crate::{AppError, AppResult};

/// 核对只表达读取到的证据，Matches 不被解释为原事务成功或允许自动重试。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferRecoveryInspection {
    DestinationMatches,
    DestinationMissing,
    DestinationDiffers,
    Insufficient,
}

/// 暂存登记是创建前的清理责任，不证明对象已创建；检查只读，任何清理都必须重新验证 token。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferStagingOwnership {
    pub staging_destination: FileEndpoint,
    pub actual_destination: FileEndpoint,
    pub staging_owner: TransferId,
    pub staging_token: String,
}

/// 恢复记录与活任务分离；重启不会创建 worker，也不恢复已失效的外部目标 lease。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRecoveryRecord {
    pub id: TransferId,
    pub request: TransferRequest,
    pub actual_destination: Option<FileEndpoint>,
    pub checkpoint: Option<TransferCheckpoint>,
    pub staging_ownership: Vec<TransferStagingOwnership>,
    pub cleanup_pending: bool,
    pub commit_unknown: bool,
    pub inspection: TransferRecoveryInspection,
    /// 人工归档不修改证据结论；未知结果与清理所有权仍可在归档历史查看。
    pub archived: bool,
}

/// 冷归档按仓储稳定顺序分页，不把无限历史一次载入内存，也不声称 ID 是时间顺序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRecoveryPage {
    pub records: Vec<TransferRecoveryRecord>,
    pub next_cursor: Option<TransferId>,
}

/// 单页上限由用例和仓储共同执行，避免调用方绕过 UI 扩大历史读取预算。
pub const MAX_TRANSFER_ARCHIVE_PAGE_SIZE: usize = 100;

/// 仓储使用类型化事实快照；每次 upsert 必须完整原子提交，失败时禁止授予写操作许可。
#[async_trait]
pub trait TransferRecoveryRepository: Send + Sync + 'static {
    /// 只列待恢复项；缺失文件表示首次运行，损坏文档不能回退空记录覆盖。
    async fn list(&self, cancellation: CancellationToken)
    -> AppResult<Vec<TransferRecoveryRecord>>;

    /// 原子保存单个 owner 的最新事实，其他记录不得被丢失。
    async fn upsert(
        &self,
        record: TransferRecoveryRecord,
        cancellation: CancellationToken,
    ) -> AppResult<()>;

    /// 仅在应用已取得完整终态及资源释放回执后删除恢复材料。
    async fn remove(&self, id: &TransferId, cancellation: CancellationToken) -> AppResult<()>;

    /// 先耐久保存完整归档证据再移出待恢复容量；重复调用必须幂等且不得覆写归档事实。
    async fn archive(&self, id: &TransferId, cancellation: CancellationToken) -> AppResult<()>;

    /// cursor 为最后返回的原 ID，按仓储稳定顺序排他续读；归档不能重新进入传输或核对流程。
    async fn list_archived(
        &self,
        cursor: Option<TransferId>,
        limit: usize,
        cancellation: CancellationToken,
    ) -> AppResult<TransferRecoveryPage>;
}

/// 单元测试和非持久化嵌入方使用内存仓储，正式桌面必须显式装配文件实现。
#[derive(Clone, Default)]
pub struct InMemoryTransferRecoveryRepository {
    records: Arc<Mutex<BTreeMap<TransferId, TransferRecoveryRecord>>>,
}

#[async_trait]
impl TransferRecoveryRepository for InMemoryTransferRecoveryRepository {
    /// 只返回待恢复记录，使内存实现也不会把归档重新暴露为可核对事务。
    /// @author kongweiguang
    async fn list(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<TransferRecoveryRecord>> {
        ensure_not_cancelled(&cancellation)?;
        Ok(self
            .records
            .lock()
            .await
            .values()
            .filter(|record| !record.archived)
            .cloned()
            .collect())
    }

    /// 归档是不可变事实，普通进度或迟到回执不能通过 upsert 将其重开或覆盖。
    /// @author kongweiguang
    async fn upsert(
        &self,
        record: TransferRecoveryRecord,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        ensure_not_cancelled(&cancellation)?;
        let mut records = self.records.lock().await;
        if record.archived
            || records
                .get(&record.id)
                .is_some_and(|current| current.archived)
        {
            return Err(AppError::conflict("已归档证据不能由传输更新覆盖"));
        }
        records.insert(record.id.clone(), record);
        Ok(())
    }

    /// 删除只作用于待恢复项，不能由普通终态清理擦掉已经人工归档的证据。
    /// @author kongweiguang
    async fn remove(&self, id: &TransferId, cancellation: CancellationToken) -> AppResult<()> {
        ensure_not_cancelled(&cancellation)?;
        let mut records = self.records.lock().await;
        if records.get(id).is_some_and(|record| !record.archived) {
            records.remove(id);
        }
        Ok(())
    }

    /// 同一锁内只改变归档标记，保留完整证据，重复归档不会重置核对结论。
    /// @author kongweiguang
    async fn archive(&self, id: &TransferId, cancellation: CancellationToken) -> AppResult<()> {
        ensure_not_cancelled(&cancellation)?;
        let mut records = self.records.lock().await;
        let record = records
            .get_mut(id)
            .ok_or_else(|| AppError::not_found("传输恢复记录不存在"))?;
        record.archived = true;
        Ok(())
    }

    /// 内存测试也执行相同页大小和排他游标约束，防止测试替身掩盖无限读取。
    /// @author kongweiguang
    async fn list_archived(
        &self,
        cursor: Option<TransferId>,
        limit: usize,
        cancellation: CancellationToken,
    ) -> AppResult<TransferRecoveryPage> {
        ensure_not_cancelled(&cancellation)?;
        validate_archive_page_limit(limit)?;
        let records = self.records.lock().await;
        let mut page = records
            .values()
            .filter(|record| {
                record.archived && cursor.as_ref().is_none_or(|cursor| &record.id > cursor)
            })
            .take(limit + 1)
            .cloned()
            .collect::<Vec<_>>();
        let has_more = page.len() > limit;
        page.truncate(limit);
        let next_cursor = page
            .last()
            .filter(|_| has_more)
            .map(|record| record.id.clone());
        Ok(TransferRecoveryPage {
            records: page,
            next_cursor,
        })
    }
}

/// 上限不是展示建议；所有实现和服务调用都必须拒绝零页或超预算读取。
/// @author kongweiguang
pub(crate) fn validate_archive_page_limit(limit: usize) -> AppResult<()> {
    if (1..=MAX_TRANSFER_ARCHIVE_PAGE_SIZE).contains(&limit) {
        Ok(())
    } else {
        Err(AppError::invalid("归档分页数量必须在 1–100 之间"))
    }
}

/// 存储取消与任务取消分开处理，已经进入原子提交的写入由仓储回报权威结果。
fn ensure_not_cancelled(cancellation: &CancellationToken) -> AppResult<()> {
    if cancellation.is_cancelled() {
        Err(AppError::cancelled("传输恢复记录操作已取消"))
    } else {
        Ok(())
    }
}
