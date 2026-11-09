//! 插件已授权调用的宿主应用适配；窗口能力与事件发布分别由组合根注入。
//! @author kongweiguang

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use kerminal_domain::{connections::RemoteProtocol, plugins::PluginId};
use kerminal_plugin_protocol::{ConfigurationSchema, error_code};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::{
    AppError, AppResult, PublicErrorCode,
    connections::management::HostManagementService,
    plugin_ports::{PluginHostApi, PluginHostError, PluginViewIdentity, PreparedPlugin},
    plugin_settings::PluginSettingsService,
};

#[path = "plugin_host_api/events.rs"]
mod events;
pub use events::PluginEventSubscriptions;

/// 窗口操作必须使用已解析的实例身份，不能从原始参数或当前活动 Tab 推断目标。
#[async_trait]
pub trait PluginUiPort: Send + Sync + 'static {
    /// 具体实现负责窗口线程调度、交互取消和终端目标的用户确认。
    async fn invoke(
        &self,
        plugin: &PluginId,
        instance: Option<&PluginViewIdentity>,
        method: &str,
        params: Value,
        cancellation: CancellationToken,
    ) -> Result<Value, PluginHostError>;
}

/// 主机目录只暴露确定安全的字段，新增目录字段不会自动进入插件契约。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginHostSummary {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub group_id: Option<String>,
}

/// 端口返回白名单投影，不能直接返回主机配置或凭据引用。
#[async_trait]
pub trait PluginHostsPort: Send + Sync + 'static {
    /// 调用者的取消令牌只控制当前读取，不改变主机目录。
    async fn list(&self, cancellation: CancellationToken) -> AppResult<Vec<PluginHostSummary>>;
}

#[async_trait]
impl PluginHostsPort for HostManagementService {
    /// 三类主机都投影为相同的只读摘要，绝不序列化完整配置。
    async fn list(&self, cancellation: CancellationToken) -> AppResult<Vec<PluginHostSummary>> {
        ensure_active(&cancellation)?;
        let mut summaries = Vec::new();
        for host in self.list_hosts().await? {
            let kind = match host.protocol {
                RemoteProtocol::Ssh => "ssh",
                RemoteProtocol::Sftp => "sftp",
                RemoteProtocol::Telnet => "telnet",
                RemoteProtocol::Serial => "serial",
                RemoteProtocol::Rdp => "rdp",
            };
            summaries.push(PluginHostSummary {
                id: host.id.as_str().to_owned(),
                name: host.name,
                kind: kind.into(),
                group_id: host.group_id,
            });
        }
        ensure_active(&cancellation)?;
        for host in self.list_local_hosts().await? {
            summaries.push(PluginHostSummary {
                id: host.id.as_str().to_owned(),
                name: host.name,
                kind: "local".into(),
                group_id: host.group_id,
            });
        }
        ensure_active(&cancellation)?;
        for host in self.list_plugin_hosts().await? {
            summaries.push(PluginHostSummary {
                id: host.id.as_str().to_owned(),
                name: host.name,
                kind: host.type_id,
                group_id: host.group_id,
            });
        }
        ensure_active(&cancellation)?;
        Ok(summaries)
    }
}

/// 启动时冻结插件及设置 schema；所有持久化操作复用既有服务的命名空间与事务边界。
pub struct PluginHostApiService {
    schemas: BTreeMap<PluginId, Option<ConfigurationSchema>>,
    settings: PluginSettingsService,
    hosts: Arc<dyn PluginHostsPort>,
    ui: Arc<dyn PluginUiPort>,
    events: Arc<PluginEventSubscriptions>,
}

impl PluginHostApiService {
    /// 接受已验证的启动快照；身份或设置 schema 不一致时禁止产生部分可用服务。
    pub fn new(
        plugins: impl IntoIterator<Item = Arc<PreparedPlugin>>,
        settings: PluginSettingsService,
        hosts: Arc<dyn PluginHostsPort>,
        ui: Arc<dyn PluginUiPort>,
        events: Arc<PluginEventSubscriptions>,
    ) -> AppResult<Self> {
        let mut schemas = BTreeMap::new();
        for plugin in plugins {
            let manifest = &plugin.package.manifest;
            if manifest.id != plugin.id.as_str() || manifest.version != plugin.version.to_string() {
                return Err(AppError::invalid("插件启动快照身份不一致"));
            }
            let schema = manifest
                .contributes
                .settings
                .as_ref()
                .map(|settings| {
                    plugin
                        .package
                        .configuration_schemas
                        .get(&settings.schema)
                        .cloned()
                        .ok_or_else(|| AppError::invalid("插件设置 schema 不存在"))
                })
                .transpose()?;
            if schemas.insert(plugin.id.clone(), schema).is_some() {
                return Err(AppError::conflict("插件启动快照重复"));
            }
        }
        Ok(Self {
            schemas,
            settings,
            hosts,
            ui,
            events,
        })
    }
}

#[async_trait]
impl PluginHostApi for PluginHostApiService {
    /// 权限和进程代次由运行路由验证；此处独立核对插件、实例、参数和取消状态。
    async fn invoke(
        &self,
        plugin: &PluginId,
        generation: u64,
        instance: Option<&PluginViewIdentity>,
        method: &str,
        mut params: Value,
        cancellation: CancellationToken,
    ) -> Result<Value, PluginHostError> {
        if cancellation.is_cancelled() {
            return Err(host_error(error_code::CANCELLED));
        }
        let schema = self
            .schemas
            .get(plugin)
            .ok_or_else(|| host_error(error_code::STALE))?;
        if instance.is_some_and(|identity| &identity.plugin != plugin) {
            return Err(host_error(error_code::STALE));
        }
        let object = params
            .as_object_mut()
            .ok_or_else(|| host_error(error_code::INVALID_PARAMS))?;
        match method {
            "settings.get" => {
                let Some(schema) = schema else {
                    return Ok(json!({}));
                };
                let snapshot = self
                    .settings
                    .load(plugin, schema, cancellation)
                    .await
                    .map_err(map_app_error)?;
                Ok(snapshot.values)
            }
            "state.get" => {
                let state = self
                    .settings
                    .state_get(plugin, cancellation)
                    .await
                    .map_err(map_app_error)?;
                Ok(json!({"state": state}))
            }
            "state.set" => {
                let state = object
                    .remove("state")
                    .ok_or_else(|| host_error(error_code::INVALID_PARAMS))?;
                self.settings
                    .state_set(plugin, state, cancellation)
                    .await
                    .map_err(map_app_error)?;
                Ok(json!({}))
            }
            "secrets.get" => {
                let slot = secret_slot(object)?;
                let value = self
                    .settings
                    .secret_get(plugin, slot, cancellation)
                    .await
                    .map_err(map_app_error)?;
                Ok(json!({"value": value.as_deref().map(String::as_str)}))
            }
            "secrets.set" => {
                let slot = secret_slot(object)?.to_owned();
                let value = match object.remove("value") {
                    Some(Value::String(value)) => value,
                    _ => return Err(host_error(error_code::INVALID_PARAMS)),
                };
                self.settings
                    .secret_set(plugin, &slot, Zeroizing::new(value), cancellation)
                    .await
                    .map_err(map_app_error)?;
                Ok(json!({}))
            }
            "secrets.delete" => {
                let slot = secret_slot(object)?;
                let deleted = self
                    .settings
                    .secret_delete(plugin, slot, cancellation)
                    .await
                    .map_err(map_app_error)?;
                Ok(json!({"deleted": deleted}))
            }
            "hosts.list" => {
                let hosts = self.hosts.list(cancellation).await.map_err(map_app_error)?;
                Ok(json!({"hosts": hosts.into_iter().map(|host| json!({
                    "id": host.id, "name": host.name, "kind": host.kind, "group_id": host.group_id
                })).collect::<Vec<_>>() }))
            }
            "events.subscribe" => {
                let names = object
                    .get("names")
                    .and_then(Value::as_array)
                    .ok_or_else(|| host_error(error_code::INVALID_PARAMS))?;
                let names = names
                    .iter()
                    .map(|name| {
                        name.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| host_error(error_code::INVALID_PARAMS))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                self.events
                    .subscribe(plugin, generation, names, cancellation)?;
                Ok(json!({}))
            }
            "log" => {
                let level = object
                    .get("level")
                    .and_then(Value::as_str)
                    .filter(|level| matches!(*level, "trace" | "debug" | "info" | "warn" | "error"))
                    .ok_or_else(|| host_error(error_code::INVALID_PARAMS))?;
                let message = object
                    .get("message")
                    .and_then(Value::as_str)
                    .ok_or_else(|| host_error(error_code::INVALID_PARAMS))?;
                tracing::info!(plugin = %plugin.as_str(), level, message_bytes = message.len(), "插件日志正文已隐藏");
                Ok(json!({}))
            }
            "ui.notify" | "ui.confirm" | "ui.prompt" | "ui.pickFile" | "clipboard.read"
            | "clipboard.write" | "shell.openExternal" | "terminal.paste" => {
                if matches!(
                    method,
                    "ui.confirm" | "ui.prompt" | "ui.pickFile" | "terminal.paste"
                ) && instance.is_none()
                {
                    return Err(host_error(error_code::INVALID_PARAMS));
                }
                if method == "shell.openExternal" {
                    let valid = object
                        .get("url")
                        .and_then(Value::as_str)
                        .and_then(|text| url::Url::parse(text).ok())
                        .is_some_and(|url| {
                            url.scheme() == "https"
                                && url.host_str().is_some()
                                && url.username().is_empty()
                                && url.password().is_none()
                        });
                    if !valid {
                        return Err(host_error(error_code::INVALID_PARAMS));
                    }
                }
                object.remove("instance");
                self.ui
                    .invoke(plugin, instance, method, params, cancellation)
                    .await
                    .map_err(sanitize_host_error)
            }
            _ => Err(host_error(error_code::METHOD_NOT_FOUND)),
        }
    }
}

/// 长度与字符约束与 Vault 槽一致，避免参数错误落入底层后暴露输入值。
fn secret_slot(object: &serde_json::Map<String, Value>) -> Result<&str, PluginHostError> {
    object
        .get("slot")
        .and_then(Value::as_str)
        .filter(|slot| !slot.is_empty() && slot.len() <= 4096 && !slot.contains('\0'))
        .ok_or_else(|| host_error(error_code::INVALID_PARAMS))
}

/// 仅读取前检查取消；已完成的副作用必须如实返回，不能伪装成未提交。
fn ensure_active(cancellation: &CancellationToken) -> AppResult<()> {
    if cancellation.is_cancelled() {
        Err(AppError::cancelled("插件操作已取消"))
    } else {
        Ok(())
    }
}

/// 宿主内部错误只映射稳定分类，绝不跨协议回显路径、秘密或底层错误源。
fn map_app_error(error: AppError) -> PluginHostError {
    let code = match error.public_code() {
        PublicErrorCode::Invalid => error_code::INVALID_PARAMS,
        PublicErrorCode::Cancelled => error_code::CANCELLED,
        PublicErrorCode::Timeout => error_code::TIMEOUT,
        _ => error_code::INTERNAL_ERROR,
    };
    host_error(code)
}

/// UI 端口同样不能把动态异常或第三方文本作为宿主协议错误返回。
fn sanitize_host_error(error: PluginHostError) -> PluginHostError {
    let code = match error.code {
        error_code::INVALID_PARAMS
        | error_code::METHOD_NOT_FOUND
        | error_code::PERMISSION_DENIED
        | error_code::CANCELLED
        | error_code::OVERLOADED
        | error_code::STALE
        | error_code::TIMEOUT => error.code,
        _ => error_code::INTERNAL_ERROR,
    };
    host_error(code)
}

/// 协议文案固定，避免插件利用错误响应探测宿主内部数据。
fn host_error(code: i32) -> PluginHostError {
    let message = match code {
        error_code::INVALID_PARAMS => "invalid params",
        error_code::METHOD_NOT_FOUND => "method not found",
        error_code::PERMISSION_DENIED => "permission denied",
        error_code::CANCELLED => "cancelled",
        error_code::OVERLOADED => "overloaded",
        error_code::STALE => "stale",
        error_code::TIMEOUT => "timeout",
        _ => "internal error",
    };
    PluginHostError {
        code,
        message: message.into(),
    }
}
