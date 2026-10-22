//! 片段收藏与使用统计的应用端口。
//!
//! @author kongweiguang

use async_trait::async_trait;
use kerminal_domain::commands::{
    SnippetPreference, SnippetPreferenceKey, SnippetUsageClearResult, SnippetUsageReceipt,
    SnippetUsageRecordRequest,
};

use super::{AppError, AppResult, CommandsService, validate_catalog_id};

/// 偏好存储状态独立于目录读取；不可用时 UI 仍可浏览片段，但写入会明确失败。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnippetPreferenceAvailability {
    Available,
    Unavailable {
        reason: SnippetPreferenceUnavailableReason,
    },
}

/// 暴露稳定故障类别，不泄漏配置路径、SQL 或本地内容。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnippetPreferenceUnavailableReason {
    NotConfigured,
    /// 共享的命令数据库仍需使用独立迁移工具升级。
    MigrationRequired(u32),
    UnsupportedSchemaVersion(u32),
    CorruptDatabase,
    StorageUnavailable,
    BackupFailed,
    StorageLockUnavailable,
}

/// SQLite v3 adapter 实现此端口；每个操作在有限数据集上完成并通过 async port 返回。
#[async_trait]
pub trait SnippetPreferenceRepository: Send + Sync + 'static {
    /// 返回初始化时的稳定可用状态，避免把存储故障伪装成空收藏列表。
    fn availability(&self) -> SnippetPreferenceAvailability;

    /// 按目录当前页的稳定 key 批量查询，缺少的数据库行由调用方解释为默认偏好。
    async fn list(&self, keys: Vec<SnippetPreferenceKey>) -> AppResult<Vec<SnippetPreference>>;

    /// 收藏状态按 origin 与 ID upsert，不能覆盖既有使用计数和最近动作。
    async fn set_favorite(
        &self,
        key: SnippetPreferenceKey,
        favorite: bool,
    ) -> AppResult<SnippetPreference>;

    /// 按调用方稳定 receipt 幂等写入已完成动作，重复请求不得增加第二次计数。
    async fn record_usage(
        &self,
        request: SnippetUsageRecordRequest,
    ) -> AppResult<SnippetUsageReceipt>;

    /// 原子清理使用回执并重置统计，但保留所有 favorite 状态。
    async fn clear_usage(&self) -> AppResult<SnippetUsageClearResult>;
}

impl CommandsService {
    /// 返回收藏/使用统计的存储状态，让入口能显示目录可用而偏好不可用的降级原因。
    pub fn snippet_preference_availability(&self) -> SnippetPreferenceAvailability {
        self.snippet_preferences.as_ref().map_or(
            SnippetPreferenceAvailability::Unavailable {
                reason: SnippetPreferenceUnavailableReason::NotConfigured,
            },
            |repository| repository.availability(),
        )
    }

    /// 注入与命令历史共享同一 SQLite 实例的偏好仓储，不创建第二连接或本地缓存。
    pub fn with_snippet_preference_repository(
        mut self,
        repository: std::sync::Arc<dyn SnippetPreferenceRepository>,
    ) -> Self {
        self.snippet_preferences = Some(repository);
        self
    }

    /// 只写用户明确点击收藏的状态，存储异常作为可见错误返回而不伪造成功。
    pub async fn set_snippet_favorite(
        &self,
        key: SnippetPreferenceKey,
        favorite: bool,
    ) -> AppResult<SnippetPreference> {
        validate_catalog_id(&key.snippet_id)?;
        let repository = self.require_snippet_preferences()?;
        if !self
            .snippets
            .catalog_identity_exists(key.origin, key.snippet_id.clone())
            .await?
        {
            return Err(AppError::not_found("命令片段目录身份不存在"));
        }
        repository.set_favorite(key, favorite).await
    }

    /// 记录已完成的 Insert/Run/CopyRendered 动作；调用方需在 ACK/成功后才调用并重用 receipt。
    pub async fn record_snippet_usage(
        &self,
        request: SnippetUsageRecordRequest,
    ) -> AppResult<SnippetUsageReceipt> {
        validate_catalog_id(&request.key.snippet_id)?;
        if request.receipt_id.is_empty()
            || request.receipt_id.chars().count() > 160
            || request.receipt_id.chars().any(char::is_control)
        {
            return Err(AppError::invalid("片段使用回执标识无效"));
        }
        let repository = self.require_snippet_preferences()?;
        if !self
            .snippets
            .catalog_identity_exists(request.key.origin, request.key.snippet_id.clone())
            .await?
        {
            return Err(AppError::not_found("命令片段目录身份不存在"));
        }
        repository.record_usage(request).await
    }

    /// 清除使用统计时保留收藏，返回仓储确认的真实删除与重置数量。
    pub async fn clear_snippet_usage(&self) -> AppResult<SnippetUsageClearResult> {
        self.require_snippet_preferences()?.clear_usage().await
    }

    /// 收藏或统计写入要求已配置且可用的仓储，不把缺少 adapter 转为无操作。
    fn require_snippet_preferences(&self) -> AppResult<&dyn SnippetPreferenceRepository> {
        let repository = self
            .snippet_preferences
            .as_deref()
            .ok_or_else(|| AppError::infrastructure("片段收藏与使用统计存储未配置"))?;
        if repository.availability() != SnippetPreferenceAvailability::Available {
            return Err(AppError::infrastructure("片段收藏与使用统计当前不可用"));
        }
        Ok(repository)
    }
}
