//! 外部插件设置、状态与秘密的应用端口。
//! @author kongweiguang

use crate::{AppError, AppResult, PublicErrorCode};
use async_trait::async_trait;
use kerminal_domain::{plugins::PluginId, preferences::ConfigRevision};
use kerminal_plugin_protocol::ConfigurationSchema;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use tokio::sync::{Mutex, MutexGuard, broadcast};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

pub(crate) mod schema;
use schema::{clear_secret_values, remove_pointer, secret_paths, set_pointer};

pub const MAX_PLUGIN_STATE_BYTES: usize = 1024 * 1024;

/// 全局 settings revision 防止插件页面覆盖其他页面或外部编辑器的变更。
#[derive(Clone, Debug, PartialEq)]
pub struct PluginSettingsSnapshot {
    pub values: Value,
    pub secret_paths: BTreeSet<String>,
    pub revision: Option<ConfigRevision>,
}

/// 原始快照只在服务与仓储之间传递，秘密字段持有引用而非明文。
#[derive(Clone, Debug, PartialEq)]
pub struct StoredPluginSettings {
    pub values: Option<Value>,
    pub revision: Option<ConfigRevision>,
}

/// 替换秘密由单独输入携带，防止表单快照或调试日志持有其明文。
pub enum PluginSecretEdit {
    Replace(Zeroizing<String>),
    Remove,
}

/// 表单以 JSON Pointer 区分秘密字段；未列出的字段保留原引用。
pub struct PluginSettingsSave {
    pub values: Value,
    pub secrets: BTreeMap<String, PluginSecretEdit>,
    pub expected_revision: Option<ConfigRevision>,
}

/// 已提交事件只包含脱敏快照，不携带秘密或待提交草稿。
#[derive(Clone, Debug)]
pub struct PluginSettingsChanged {
    pub plugin: PluginId,
    pub snapshot: PluginSettingsSnapshot,
}

/// 文件实现必须在相同配置仓储中执行 CAS，取消不能掩盖已开始的提交。
#[async_trait]
pub trait PluginSettingsRepository: Send + Sync + 'static {
    /// 缺失插件节与空对象分别表示尚未配置和明确保存的空设置。
    async fn load(
        &self,
        plugin: &PluginId,
        cancellation: CancellationToken,
    ) -> AppResult<StoredPluginSettings>;
    /// 写入只替换当前插件节，旧 revision 必须与整份文件匹配。
    async fn save(
        &self,
        plugin: &PluginId,
        values: Value,
        expected: Option<ConfigRevision>,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigRevision>;
    /// 删除插件节时仍要求调用方提供读取时的文件 revision。
    async fn delete(
        &self,
        plugin: &PluginId,
        expected: Option<ConfigRevision>,
        cancellation: CancellationToken,
    ) -> AppResult<Option<ConfigRevision>>;
}

/// 状态文件和整个插件数据目录归基础设施管理，不能从插件提供的路径拼接。
#[async_trait]
pub trait PluginStateRepository: Send + Sync + 'static {
    /// 不存在状态时返回 None，损坏状态不得改写成默认值。
    async fn get(
        &self,
        plugin: &PluginId,
        cancellation: CancellationToken,
    ) -> AppResult<Option<Value>>;
    /// 实现必须在原子提交前检查取消，并限制编码后不超过一 MiB。
    async fn set(
        &self,
        plugin: &PluginId,
        state: Value,
        cancellation: CancellationToken,
    ) -> AppResult<()>;
    /// 仅由显式删除插件数据流程调用；普通卸载必须保留数据。
    async fn delete_data(
        &self,
        plugin: &PluginId,
        cancellation: CancellationToken,
    ) -> AppResult<()>;
}

/// Vault 引用由适配器产生和核对，调用方不能传入别的插件的裸引用。
#[async_trait]
pub trait PluginSecretRepository: Send + Sync + 'static {
    /// 新建不可变候选；未知提交结果需保留候选以便读回。
    async fn create_setting(
        &self,
        plugin: &PluginId,
        path: &str,
        secret: Zeroizing<String>,
        cancellation: CancellationToken,
    ) -> AppResult<String>;
    /// 只解密插件、字段和引用三者均匹配的设置项。
    async fn read_setting(
        &self,
        plugin: &PluginId,
        path: &str,
        reference: &str,
        cancellation: CancellationToken,
    ) -> AppResult<Zeroizing<String>>;
    /// API 槽与设置候选分开，运行时必须在调用前检查 secrets 权限。
    async fn get(
        &self,
        plugin: &PluginId,
        slot: &str,
        cancellation: CancellationToken,
    ) -> AppResult<Option<Zeroizing<String>>>;
    /// 覆盖固定槽位后，结果未知时调用方必须先读回而非自动重放。
    async fn set(
        &self,
        plugin: &PluginId,
        slot: &str,
        secret: Zeroizing<String>,
        cancellation: CancellationToken,
    ) -> AppResult<()>;
    /// 缺失槽按幂等操作处理，不能删除其它插件条目。
    async fn delete(
        &self,
        plugin: &PluginId,
        slot: &str,
        cancellation: CancellationToken,
    ) -> AppResult<bool>;
    /// 只清除此插件的 API 槽及设置候选，不清除任何其他 Vault 条目。
    async fn delete_all(&self, plugin: &PluginId, cancellation: CancellationToken)
    -> AppResult<()>;
}

/// 同一个服务实例串行协调删除与提交，调用方共享 clone 才能保持该边界。
#[derive(Clone)]
pub struct PluginSettingsService {
    settings: Arc<dyn PluginSettingsRepository>,
    state: Arc<dyn PluginStateRepository>,
    secrets: Arc<dyn PluginSecretRepository>,
    operations: Arc<Mutex<()>>,
    changes: broadcast::Sender<PluginSettingsChanged>,
}

impl PluginSettingsService {
    /// 等待其他插件操作时仍响应取消，已获得锁后才允许开始副作用。
    async fn lock_operation(
        &self,
        cancellation: &CancellationToken,
    ) -> AppResult<MutexGuard<'_, ()>> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(AppError::cancelled("插件操作已取消")),
            guard = self.operations.lock() => {
                ensure_active(cancellation)?;
                Ok(guard)
            }
        }
    }

    /// 组合根注入共享根的三个端口，不让 UI 或 RPC 直接访问文件和 Vault。
    pub fn new(
        settings: Arc<dyn PluginSettingsRepository>,
        state: Arc<dyn PluginStateRepository>,
        secrets: Arc<dyn PluginSecretRepository>,
    ) -> Self {
        let (changes, _) = broadcast::channel(64);
        Self {
            settings,
            state,
            secrets,
            operations: Arc::new(Mutex::new(())),
            changes,
        }
    }

    /// 订阅落后时应重新读取快照，不能把事件当作权威存储。
    pub fn subscribe(&self) -> broadcast::Receiver<PluginSettingsChanged> {
        self.changes.subscribe()
    }

    /// 用清单 schema 校验实际值后返回脱敏快照；缺失配置留给表单初始化。
    pub async fn load(
        &self,
        plugin: &PluginId,
        schema: &ConfigurationSchema,
        cancellation: CancellationToken,
    ) -> AppResult<PluginSettingsSnapshot> {
        let stored = self.settings.load(plugin, cancellation.clone()).await?;
        self.decode(plugin, schema, stored, cancellation).await
    }

    /// 先校验明文与 schema，再建立独立候选并 CAS 发布引用；失败保留候选供恢复。
    pub async fn save(
        &self,
        plugin: &PluginId,
        schema: &ConfigurationSchema,
        mut request: PluginSettingsSave,
        cancellation: CancellationToken,
    ) -> AppResult<PluginSettingsSnapshot> {
        let _guard = self.lock_operation(&cancellation).await?;
        ensure_active(&cancellation)?;
        let current = self.settings.load(plugin, cancellation.clone()).await?;
        if current.revision != request.expected_revision {
            return Err(AppError::conflict("设置已被其它操作修改，请重新读取后保存"));
        }
        if !request.values.is_object() {
            return Err(AppError::invalid("插件设置必须是对象"));
        }
        let paths = secret_paths(schema, &request.values)?;
        if request.secrets.keys().any(|path| !paths.contains(path)) {
            return Err(AppError::invalid("插件设置秘密字段未声明"));
        }
        let mut plaintext = BTreeMap::new();
        let mut replacements = BTreeSet::new();
        let mut stored = request.values.clone();
        for path in &paths {
            if !request.values.pointer(path).is_some_and(Value::is_null) {
                return Err(AppError::invalid("插件设置秘密只能通过专用输入提交"));
            }
            match request.secrets.remove(path) {
                Some(PluginSecretEdit::Remove) => {
                    remove_pointer(&mut request.values, path)?;
                    remove_pointer(&mut stored, path)?;
                }
                Some(PluginSecretEdit::Replace(secret)) => {
                    if secret.is_empty() {
                        return Err(AppError::invalid("插件秘密不能为空"));
                    }
                    plaintext.insert(path.clone(), secret);
                    replacements.insert(path.clone());
                }
                None => {
                    let reference = current
                        .values
                        .as_ref()
                        .and_then(|value| value.pointer(path))
                        .and_then(Value::as_str)
                        .ok_or_else(|| AppError::invalid("插件设置秘密尚未提供"))?;
                    let secret = self
                        .secrets
                        .read_setting(plugin, path, reference, cancellation.clone())
                        .await?;
                    plaintext.insert(path.clone(), secret);
                    set_pointer(&mut stored, path, Value::String(reference.to_owned()))?;
                }
            }
        }
        let validation = (|| {
            for (path, secret) in &plaintext {
                set_pointer(&mut request.values, path, Value::String(secret.to_string()))?;
            }
            schema
                .validate(&request.values)
                .map_err(|_| AppError::invalid("插件设置不符合声明的 schema"))
        })();
        clear_secret_values(&mut request.values, &paths);
        validation?;
        for path in replacements {
            let secret = plaintext
                .remove(&path)
                .ok_or_else(|| AppError::infrastructure("插件秘密候选缺失"))?;
            let reference = self
                .secrets
                .create_setting(plugin, &path, secret, cancellation.clone())
                .await?;
            set_pointer(&mut stored, &path, Value::String(reference))?;
        }
        ensure_active(&cancellation)?;
        let revision = match self
            .settings
            .save(
                plugin,
                stored.clone(),
                request.expected_revision,
                cancellation,
            )
            .await
        {
            Ok(revision) => revision,
            Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                let observed = self.settings.load(plugin, CancellationToken::new()).await;
                match observed {
                    Ok(StoredPluginSettings {
                        values: Some(values),
                        revision: Some(revision),
                    }) if values == stored => revision,
                    _ => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        let snapshot = self
            .decode(
                plugin,
                schema,
                StoredPluginSettings {
                    values: Some(stored),
                    revision: Some(revision),
                },
                CancellationToken::new(),
            )
            .await
            .map_err(|_| AppError::outcome_unknown("插件设置已提交，状态尚未确认，请读回核对"))?;
        let _ = self.changes.send(PluginSettingsChanged {
            plugin: plugin.clone(),
            snapshot: snapshot.clone(),
        });
        Ok(snapshot)
    }

    /// 状态限制按实际 JSON 字节计算，避免多字节内容绕过上限。
    pub async fn state_get(
        &self,
        plugin: &PluginId,
        cancellation: CancellationToken,
    ) -> AppResult<Option<Value>> {
        self.state.get(plugin, cancellation).await
    }

    /// 状态写入和删除共享用例锁，避免显式删除后迟到写入重新创建目录。
    pub async fn state_set(
        &self,
        plugin: &PluginId,
        state: Value,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        let _guard = self.lock_operation(&cancellation).await?;
        ensure_active(&cancellation)?;
        if serde_json::to_vec(&state)
            .map_err(|_| AppError::invalid("插件状态不能编码"))?
            .len()
            > MAX_PLUGIN_STATE_BYTES
        {
            return Err(AppError::invalid("插件状态超过 1 MiB"));
        }
        self.state.set(plugin, state, cancellation).await
    }

    /// 槽读取仅在运行时完成权限检查后调用，返回值会在释放时清零。
    pub async fn secret_get(
        &self,
        plugin: &PluginId,
        slot: &str,
        cancellation: CancellationToken,
    ) -> AppResult<Option<Zeroizing<String>>> {
        self.secrets.get(plugin, slot, cancellation).await
    }

    /// 槽写入与显式数据删除共用顺序，避免删除结束后迟到回执重建秘密。
    pub async fn secret_set(
        &self,
        plugin: &PluginId,
        slot: &str,
        secret: Zeroizing<String>,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        let _guard = self.lock_operation(&cancellation).await?;
        ensure_active(&cancellation)?;
        self.secrets.set(plugin, slot, secret, cancellation).await
    }

    /// 删除目标仅由插件身份和槽决定，不接受调用方提供的 Vault 引用。
    pub async fn secret_delete(
        &self,
        plugin: &PluginId,
        slot: &str,
        cancellation: CancellationToken,
    ) -> AppResult<bool> {
        let _guard = self.lock_operation(&cancellation).await?;
        ensure_active(&cancellation)?;
        self.secrets.delete(plugin, slot, cancellation).await
    }

    /// 此操作可能分阶段完成；任一失败都保留剩余材料并要求调用方读回再决定是否继续。
    pub async fn delete_data(
        &self,
        plugin: &PluginId,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        let _guard = self.lock_operation(&cancellation).await?;
        ensure_active(&cancellation)?;
        let current = self.settings.load(plugin, cancellation.clone()).await?;
        let settings_existed = current.values.is_some();
        if let Err(error) = self
            .settings
            .delete(plugin, current.revision, cancellation.clone())
            .await
            && (error.public_code() != PublicErrorCode::OutcomeUnknown
                || !matches!(
                    self.settings.load(plugin, CancellationToken::new()).await,
                    Ok(StoredPluginSettings { values: None, .. })
                ))
        {
            return Err(error);
        }
        if let Err(error) = self.state.delete_data(plugin, cancellation.clone()).await {
            return Err(if settings_existed {
                AppError::outcome_unknown_with_source("插件数据可能已部分删除，请读回后继续", error)
            } else {
                error
            });
        }
        self.secrets
            .delete_all(plugin, cancellation)
            .await
            .map_err(|error| {
                AppError::outcome_unknown_with_source("插件数据可能已部分删除，请读回后继续", error)
            })
    }

    /// 解密只用于相同 schema 的真实值校验，返回前清零并用 null 表达已保存状态。
    async fn decode(
        &self,
        plugin: &PluginId,
        schema: &ConfigurationSchema,
        stored: StoredPluginSettings,
        cancellation: CancellationToken,
    ) -> AppResult<PluginSettingsSnapshot> {
        let Some(mut values) = stored.values else {
            return Ok(PluginSettingsSnapshot {
                values: serde_json::json!({}),
                secret_paths: BTreeSet::new(),
                revision: stored.revision,
            });
        };
        if !values.is_object() {
            return Err(AppError::configuration_corrupt("插件设置"));
        }
        let paths = secret_paths(schema, &values)?;
        let mut plaintext = BTreeMap::new();
        for path in &paths {
            let reference = values
                .pointer(path)
                .and_then(Value::as_str)
                .ok_or_else(|| AppError::configuration_corrupt("插件设置秘密引用"))?;
            let secret = self
                .secrets
                .read_setting(plugin, path, reference, cancellation.clone())
                .await?;
            plaintext.insert(path.clone(), secret);
        }
        let validation = (|| {
            for (path, secret) in &plaintext {
                set_pointer(&mut values, path, Value::String(secret.to_string()))?;
            }
            schema
                .validate(&values)
                .map_err(|_| AppError::configuration_corrupt("插件设置不符合声明的 schema"))
        })();
        clear_secret_values(&mut values, &paths);
        validation?;
        Ok(PluginSettingsSnapshot {
            values,
            secret_paths: paths,
            revision: stored.revision,
        })
    }
}

/// 等待共享锁后再次检查取消，已经开始的不可撤销提交由仓储返回真实结果。
fn ensure_active(cancellation: &CancellationToken) -> AppResult<()> {
    if cancellation.is_cancelled() {
        Err(AppError::cancelled("插件操作已取消"))
    } else {
        Ok(())
    }
}
