//! 持久运行定义的一次性转换；历史 PID、监听器和在途传输不成为可重放的当前任务。
//!
//! @author kongweiguang

use std::{collections::BTreeSet, path::Path};

use kerminal_application::{
    AppError, AppResult, data_migration::DataMigrationBlocker, operations::PortForwardDefinition,
};
use kerminal_domain::{
    identity::HostId,
    operations::{
        ForwardEndpoint, ForwardKind, PortForwardConfig, ProxyApplyScope, ProxyProtocol,
        RemoteAccessScope,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{FileEdit, inspection::read_optional_regular_file};

pub(super) const LEGACY_FORWARDS: &str = "data/port-forwards/sessions.json";
const CURRENT_FORWARDS: &str = "data/port-forwards/definitions.json";
const TRANSFER_RECOVERY: &str = "data/transfer-recovery.json";
const MAX_FORWARD_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TRANSFER_RECOVERY_BYTES: u64 = 16 * 1024 * 1024;
const MAX_FORWARD_DEFINITIONS: usize = 2048;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ForwardDefinitionsDocument {
    schema_version: u32,
    definitions: Vec<PortForwardDefinition>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyForwardDocument {
    schema_version: u32,
    sessions: Vec<LegacyForwardSummary>,
}

/// 仅稳定字段进入新定义；仍严格解码旧摘要已知的运行字段以阻断损坏或未来记录。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyForwardSummary {
    id: String,
    host_id: String,
    #[serde(rename = "hostName")]
    _host_name: String,
    name: String,
    kind: ForwardKind,
    #[serde(default, rename = "origin")]
    _origin: Option<LegacyOrigin>,
    bind_host: String,
    local_bind_host: Option<String>,
    remote_bind_host: Option<String>,
    source_port: u16,
    target_host: Option<String>,
    target_port: Option<u16>,
    #[serde(rename = "localEndpoint")]
    _local_endpoint: Option<LegacyEndpoint>,
    #[serde(rename = "remoteEndpoint")]
    _remote_endpoint: Option<LegacyEndpoint>,
    proxy_protocol: Option<LegacyProxyProtocol>,
    remote_access_scope: Option<RemoteAccessScope>,
    #[serde(rename = "proxyUrl")]
    _proxy_url: Option<String>,
    #[serde(default)]
    proxy_apply_scope: ProxyApplyScope,
    #[serde(default, rename = "commandPreview")]
    _command_preview: String,
    #[serde(rename = "lastError")]
    _last_error: Option<String>,
    #[serde(rename = "runtime")]
    _runtime: Option<LegacyRuntime>,
    #[serde(rename = "pid")]
    _pid: Option<u32>,
    #[serde(rename = "status")]
    _status: LegacyStatus,
    #[serde(rename = "createdAt")]
    _created_at: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum LegacyOrigin {
    User,
    McpTool,
    NetworkAssist,
    HostPreset,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum LegacyProxyProtocol {
    Http,
    Socks5,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum LegacyStatus {
    Running,
    Exited,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyEndpoint {
    #[serde(default, rename = "host")]
    _host: String,
    #[serde(rename = "port")]
    _port: Option<u16>,
    #[serde(rename = "label")]
    _label: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyRuntime {
    #[serde(rename = "mode")]
    _mode: LegacyRuntimeMode,
    #[serde(default, rename = "backend")]
    _backend: String,
    #[serde(default, rename = "tunnelKind")]
    _tunnel_kind: String,
    #[serde(rename = "managedSessionId")]
    _managed_session_id: Option<String>,
    #[serde(rename = "managedChannelId")]
    _managed_channel_id: Option<String>,
    #[serde(rename = "managedChannelKind")]
    _managed_channel_kind: Option<String>,
    #[serde(rename = "managedTunnelId")]
    _managed_tunnel_id: Option<String>,
    #[serde(rename = "fallbackReason")]
    _fallback_reason: Option<String>,
    #[serde(rename = "recentFailure")]
    _recent_failure: Option<String>,
    #[serde(default, rename = "cleanupStatus")]
    _cleanup_status: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum LegacyRuntimeMode {
    Unknown,
    ManagedSshRuntime,
    OpenSshProcess,
    OpenSshPty,
    Restored,
}

/// 转发只转换为停止态定义；原摘要作为无写入的 CAS 项备份，旧运行信息永远不自动复活。
pub(super) fn plan_operational_state_migration(root: &Path) -> AppResult<Vec<FileEdit>> {
    validate_current_operational_state(root)
        .map_err(|_| AppError::conflict("当前转发定义或传输恢复记录预检失败"))?;
    let Some(original) =
        read_optional_regular_file(root, Path::new(LEGACY_FORWARDS), MAX_FORWARD_BYTES)?
    else {
        return Ok(Vec::new());
    };
    let legacy: LegacyForwardDocument = serde_json::from_slice(&original)
        .map_err(|_| AppError::invalid("旧转发摘要不是受支持的完整文档"))?;
    if legacy.schema_version != 1 || legacy.sessions.len() > MAX_FORWARD_DEFINITIONS {
        return Err(AppError::invalid("旧转发摘要版本或数量不受支持"));
    }
    let mut definitions = legacy
        .sessions
        .into_iter()
        .map(convert_legacy_forward)
        .collect::<AppResult<Vec<_>>>()?;
    definitions.sort_by(|left, right| left.id.cmp(&right.id));
    let current = ForwardDefinitionsDocument {
        schema_version: 1,
        definitions,
    };
    let after = serde_json::to_vec_pretty(&current)
        .map_err(|_| AppError::infrastructure("编码当前转发定义失败"))?;
    validate_forward_definitions(&after)?;
    let before = read_optional_regular_file(root, Path::new(CURRENT_FORWARDS), MAX_FORWARD_BYTES)?;
    let mut edits = vec![FileEdit {
        relative_path: LEGACY_FORWARDS.into(),
        before: Some(original.clone()),
        after: original,
    }];
    if let Some(ref bytes) = before {
        let mut existing = validate_forward_definitions(bytes)?;
        existing
            .definitions
            .sort_by(|left, right| left.id.cmp(&right.id));
        if existing.definitions != current.definitions {
            return Err(AppError::conflict(
                "当前转发定义与旧摘要冲突，未覆盖任何定义",
            ));
        }
        edits.push(FileEdit {
            relative_path: CURRENT_FORWARDS.into(),
            before: Some(bytes.clone()),
            after: bytes.clone(),
        });
    } else {
        edits.push(FileEdit {
            relative_path: CURRENT_FORWARDS.into(),
            before,
            after,
        });
    }
    Ok(edits)
}

/// 当前恢复记录只做完整校验，不把旧 PID、旧传输 checkpoint 或未知提交猜成可重试任务。
pub(super) fn validate_current_operational_state(root: &Path) -> Result<(), DataMigrationBlocker> {
    if let Some(bytes) =
        read_optional_regular_file(root, Path::new(CURRENT_FORWARDS), MAX_FORWARD_BYTES)
            .map_err(|_| invalid_document(CURRENT_FORWARDS))?
    {
        validate_forward_definitions(&bytes).map_err(|_| invalid_document(CURRENT_FORWARDS))?;
    }
    if let Some(bytes) = read_optional_regular_file(
        root,
        Path::new(TRANSFER_RECOVERY),
        MAX_TRANSFER_RECOVERY_BYTES,
    )
    .map_err(|_| invalid_document(TRANSFER_RECOVERY))?
    {
        let source =
            std::str::from_utf8(&bytes).map_err(|_| invalid_document(TRANSFER_RECOVERY))?;
        crate::files::validate_transfer_recovery_document(source)
            .map_err(|_| invalid_document(TRANSFER_RECOVERY))?;
    }
    Ok(())
}

/// 只返回固定受管路径，不把解析器错误中的用户内容或绝对路径带入公开预检结果。
fn invalid_document(relative_path: &'static str) -> DataMigrationBlocker {
    DataMigrationBlocker::ManagedDataInvalid {
        relative_path: relative_path.to_owned(),
    }
}

/// 新文档拒绝未知字段及重复 ID，避免 serde 的忽略策略把未来配置静默降级成当前格式。
fn validate_forward_definitions(bytes: &[u8]) -> AppResult<ForwardDefinitionsDocument> {
    if bytes.len() as u64 > MAX_FORWARD_BYTES {
        return Err(AppError::invalid("转发定义文档过大"));
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| AppError::invalid("转发定义文档不是有效 JSON"))?;
    if let Some(definitions) = value.get("definitions").and_then(Value::as_array) {
        for definition in definitions {
            if let Some(config) = definition.get("config") {
                require_known_fields(
                    config,
                    &[
                        "hostId",
                        "name",
                        "kind",
                        "bind",
                        "destination",
                        "proxyProtocol",
                        "remoteAccessScope",
                        "proxyApplyScope",
                    ],
                )?;
                for endpoint in [config.get("bind"), config.get("destination")]
                    .into_iter()
                    .flatten()
                {
                    if !endpoint.is_null() {
                        require_known_fields(endpoint, &["host", "port"])?;
                    }
                }
            }
        }
    }
    let document: ForwardDefinitionsDocument =
        serde_json::from_value(value).map_err(|_| AppError::invalid("转发定义文档结构无效"))?;
    if document.schema_version != 1 || document.definitions.len() > MAX_FORWARD_DEFINITIONS {
        return Err(AppError::invalid("转发定义文档版本或数量不受支持"));
    }
    let mut ids = BTreeSet::new();
    for definition in &document.definitions {
        definition.validate()?;
        if !ids.insert(&definition.id) {
            return Err(AppError::conflict("转发定义包含重复 ID"));
        }
    }
    Ok(document)
}

/// 字段白名单只用于已知当前定义的嵌套对象，不试图容错解释未来配置。
fn require_known_fields(value: &Value, allowed: &[&str]) -> AppResult<()> {
    let object = value
        .as_object()
        .ok_or_else(|| AppError::invalid("转发定义对象无效"))?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(AppError::invalid("转发定义包含未知字段"));
    }
    Ok(())
}

/// 解析并保留旧主机稳定 ID，绝不生成替代身份；已知 remote+SOCKS 显式转换而不推断 HTTP。
fn convert_legacy_forward(summary: LegacyForwardSummary) -> AppResult<PortForwardDefinition> {
    let proxy_protocol = match summary.proxy_protocol {
        None => None,
        Some(LegacyProxyProtocol::Socks5) => Some(ProxyProtocol::Socks5),
        Some(LegacyProxyProtocol::Http) => {
            return Err(AppError::unsupported("旧 HTTP 转发须人工处理"));
        }
    };
    let kind = if summary.kind == ForwardKind::Remote
        && proxy_protocol == Some(ProxyProtocol::Socks5)
        && summary.target_host.is_none()
        && summary.target_port.is_none()
    {
        ForwardKind::RemoteDynamic
    } else {
        summary.kind
    };
    let bind = ForwardEndpoint::new(summary.bind_host, summary.source_port)
        .map_err(|_| AppError::invalid("旧转发监听端点无效"))?;
    let specific_bind = match kind {
        ForwardKind::Local | ForwardKind::Dynamic => summary.local_bind_host,
        ForwardKind::Remote | ForwardKind::RemoteDynamic => summary.remote_bind_host,
    };
    if let Some(host) = specific_bind.filter(|host| !host.trim().is_empty()) {
        let specific = ForwardEndpoint::new(host, summary.source_port)
            .map_err(|_| AppError::invalid("旧转发专用监听端点无效"))?;
        if specific != bind {
            return Err(AppError::conflict("旧转发监听地址互相冲突"));
        }
    }
    let destination = match (summary.target_host, summary.target_port) {
        (Some(host), Some(port)) => Some(
            ForwardEndpoint::new(host, port)
                .map_err(|_| AppError::invalid("旧转发目标端点无效"))?,
        ),
        (None, None) => None,
        _ => return Err(AppError::invalid("旧转发目标端点不完整")),
    };
    let definition = PortForwardDefinition {
        id: summary.id,
        config: PortForwardConfig {
            host_id: HostId::parse(&summary.host_id)
                .map_err(|_| AppError::invalid("旧转发主机 ID 无效"))?,
            name: summary.name,
            kind,
            bind,
            destination,
            proxy_protocol,
            remote_access_scope: summary.remote_access_scope,
            proxy_apply_scope: summary.proxy_apply_scope,
        },
    };
    definition.validate()?;
    Ok(definition)
}

#[cfg(test)]
#[path = "../../tests/support/data_migration_operational_state_unit.rs"]
mod tests;
