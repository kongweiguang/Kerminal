//! 在既有配置 CAS 上隔离插件节，状态文件单独置于插件数据目录。
//! @author kongweiguang

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    plugin_settings::{PluginSettingsRepository, StoredPluginSettings},
    preferences::ConfigurationRepository,
};
use kerminal_domain::{
    plugins::PluginId,
    preferences::{ConfigDocumentPath, ConfigRevision},
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use toml_edit::{DocumentMut, Item, Table};

use super::ConfigurationStore;

mod state;

/// 只检查命名空间的结构和身份；实际字段由已安装清单的协议 schema 校验。
pub(super) fn validate_namespaces(source: &str) -> AppResult<()> {
    let root: toml::Value =
        toml::from_str(source).map_err(|_| AppError::configuration_corrupt("插件设置"))?;
    let Some(plugins) = root.get("plugins") else {
        return Ok(());
    };
    let plugins = plugins
        .as_table()
        .ok_or_else(|| AppError::configuration_corrupt("插件设置命名空间"))?;
    for (id, value) in plugins {
        PluginId::parse(id.clone()).map_err(|_| AppError::configuration_corrupt("插件设置身份"))?;
        if !value.is_table() {
            return Err(AppError::configuration_corrupt("插件设置必须是表"));
        }
    }
    Ok(())
}

/// 解析当前插件节，不把其他插件的配置暴露给调用方。
fn decode(source: &str, plugin: &PluginId) -> AppResult<Option<Value>> {
    validate_namespaces(source)?;
    let root: toml::Value =
        toml::from_str(source).map_err(|_| AppError::configuration_corrupt("插件设置"))?;
    root.get("plugins")
        .and_then(|plugins| plugins.get(plugin.as_str()))
        .map(|value| {
            serde_json::to_value(value).map_err(|_| AppError::configuration_corrupt("插件设置"))
        })
        .transpose()
}

/// 保留其它设置的原文和注释，仅替换目标插件整个表。
fn patch(
    source: Option<&str>,
    plugin: &PluginId,
    replacement: Option<&Value>,
) -> AppResult<String> {
    let mut document = match source {
        Some(source) => source
            .parse::<DocumentMut>()
            .map_err(|_| AppError::configuration_corrupt("设置"))?,
        None => {
            let mut document = DocumentMut::new();
            document["schema_version"] = toml_edit::value(1);
            document
        }
    };
    if document.get("plugins").is_none() {
        if replacement.is_none() {
            return Ok(document.to_string());
        }
        document["plugins"] = Item::Table(Table::new());
    }
    let plugins = document
        .get_mut("plugins")
        .ok_or_else(|| AppError::configuration_corrupt("插件设置"))?;
    if let Some(inline) = plugins.as_inline_table_mut() {
        *plugins = Item::Table(std::mem::take(inline).into_table());
    }
    let plugins = plugins
        .as_table_mut()
        .ok_or_else(|| AppError::configuration_corrupt("插件设置"))?;
    if let Some(replacement) = replacement {
        if !replacement.is_object() {
            return Err(AppError::invalid("插件设置必须是对象"));
        }
        let projected = toml_edit::ser::to_document(replacement)
            .map_err(|_| AppError::invalid("插件设置不能表示为 TOML"))?;
        plugins.insert(plugin.as_str(), Item::Table(projected.as_table().clone()));
    } else {
        plugins.remove(plugin.as_str());
    }
    Ok(document.to_string())
}

/// 每次 I/O 前检查调用者的取消，不在持久提交之后伪装成已取消。
fn ensure_active(cancellation: &CancellationToken) -> AppResult<()> {
    if cancellation.is_cancelled() {
        Err(AppError::cancelled("插件操作已取消"))
    } else {
        Ok(())
    }
}

#[async_trait]
impl PluginSettingsRepository for ConfigurationStore {
    /// 原文和 revision 来自同一个既有仓储，避免额外维护配置文件锁。
    async fn load(
        &self,
        plugin: &PluginId,
        cancellation: CancellationToken,
    ) -> AppResult<StoredPluginSettings> {
        ensure_active(&cancellation)?;
        let document = self.read(ConfigDocumentPath::Settings).await?;
        ensure_active(&cancellation)?;
        match document {
            Some(document) => Ok(StoredPluginSettings {
                values: decode(&document.source, plugin)?,
                revision: Some(document.revision),
            }),
            None => Ok(StoredPluginSettings {
                values: None,
                revision: None,
            }),
        }
    }

    /// 双重 CAS 保证 patch 窗口中的外部修改也会产生冲突。
    async fn save(
        &self,
        plugin: &PluginId,
        values: Value,
        expected: Option<ConfigRevision>,
        cancellation: CancellationToken,
    ) -> AppResult<ConfigRevision> {
        ensure_active(&cancellation)?;
        let document = self.read(ConfigDocumentPath::Settings).await?;
        if document.as_ref().map(|document| &document.revision) != expected.as_ref() {
            return Err(AppError::conflict("设置已被其它操作修改，请重新读取后保存"));
        }
        let source = patch(
            document.as_ref().map(|document| document.source.as_str()),
            plugin,
            Some(&values),
        )?;
        ensure_active(&cancellation)?;
        self.write(ConfigDocumentPath::Settings, source, expected)
            .await
    }

    /// 无插件节时不创建文件；有节时只在 revision 未变化下提交删除。
    async fn delete(
        &self,
        plugin: &PluginId,
        expected: Option<ConfigRevision>,
        cancellation: CancellationToken,
    ) -> AppResult<Option<ConfigRevision>> {
        ensure_active(&cancellation)?;
        let document = self.read(ConfigDocumentPath::Settings).await?;
        if document.as_ref().map(|document| &document.revision) != expected.as_ref() {
            return Err(AppError::conflict("设置已被其它操作修改，请重新读取后删除"));
        }
        let Some(document) = document else {
            return Ok(None);
        };
        if decode(&document.source, plugin)?.is_none() {
            return Ok(Some(document.revision));
        }
        let source = patch(Some(&document.source), plugin, None)?;
        ensure_active(&cancellation)?;
        self.write(ConfigDocumentPath::Settings, source, expected)
            .await
            .map(Some)
    }
}
