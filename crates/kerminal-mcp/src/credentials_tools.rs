//! MCP 凭据工具到主机管理与加密 Vault 用例的显式适配。
//!
//! @author kongweiguang

use kerminal_application::{
    AppError,
    connections::{
        credentials::{SaveCredentialRequest, SecretValue},
        management::{
            HostAuthenticationMode, HostCredentialChange, HostCredentialSlot, HostDraft,
            HostManagementService, HostRouteSettings,
        },
    },
};
use kerminal_domain::{
    connections::{
        CredentialRef, RemoteProtocol, SshAuthentication,
        credentials::{CredentialKind, CredentialMaterial, CredentialScope},
    },
    identity::HostId,
};
use rmcp::model::CallToolResult;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::{
    McpServices,
    terminal_tools::{failure, parse_args, success},
};

const MAX_SECRET_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HostUpsertArgs {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    group_id: Option<String>,
    name: String,
    host: String,
    #[serde(default)]
    port: Option<u16>,
    username: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EncryptSecretArgs {
    kind: String,
    host_id: String,
    scope: String,
    material: String,
    plaintext: String,
}

/// 按冻结工具 ID 分派到应用用例；MCP 层不访问配置文件或 Vault adapter。
pub(crate) async fn dispatch(
    tool_id: &str,
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("MCP request was cancelled"));
    }
    match tool_id {
        "kerminal.host.upsert_with_credential" => {
            upsert_host(services, arguments, cancellation).await
        }
        "kerminal.vault.encrypt_secret" => encrypt_secret(services, arguments, cancellation).await,
        _ => failure(AppError::invalid("Unknown credential tool")),
    }
}

/// 保持冻结的凭据输入契约；更新仅替换声明字段，不能重置已有备注、终端、传输和显示选项。
/// 凭据发布仍经过 vault-save→host-CAS，失败时不丢失可核对的旧引用。
///
/// @author kongweiguang
async fn upsert_host(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let mut args = match parse_args::<HostUpsertArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let password = match secret_value(std::mem::take(&mut args.password)) {
        Ok(password) => password,
        Err(error) => return failure(error),
    };
    let hosts = match services.require_host_management() {
        Ok(hosts) => hosts,
        Err(error) => return failure(error),
    };
    let existing = match find_existing_host(hosts, &args).await {
        Ok(host) => host,
        Err(error) => return failure(error),
    };
    let port = args.port.unwrap_or(22);
    let protocol = existing
        .as_ref()
        .map_or(RemoteProtocol::Ssh, |host| host.protocol);
    if !matches!(
        protocol,
        RemoteProtocol::Ssh | RemoteProtocol::Sftp | RemoteProtocol::Rdp
    ) {
        return failure(AppError::capability_not_supported(
            "credential upsert supports SSH, SFTP, and RDP hosts",
        ));
    }
    let host_id = existing.as_ref().map(|host| host.id.clone());
    let group_id = requested_group(
        args.group_id,
        existing.as_ref().and_then(|host| host.group_id.clone()),
    );
    let draft = HostDraft {
        id: host_id,
        group_id,
        name: args.name.trim().to_owned(),
        note: existing.as_ref().and_then(|host| host.note.clone()),
        protocol,
        endpoint: kerminal_domain::connections::HostEndpoint {
            host: args.host.trim().to_owned(),
            port,
            username: args.username.trim().to_owned(),
        },
        authentication: None,
        authentication_mode: HostAuthenticationMode::Password,
        rdp_password: existing.as_ref().and_then(|host| host.rdp_password.clone()),
        route: existing
            .as_ref()
            .map_or_else(HostRouteSettings::default, |host| host.route.clone()),
        serial: None,
        tags: existing
            .as_ref()
            .map_or_else(Vec::new, |host| host.tags.clone()),
        is_favorite: existing.as_ref().is_some_and(|host| host.is_favorite),
        sort_order: existing.as_ref().map_or(0, |host| host.sort_order),
        connect_timeout_ms: existing
            .as_ref()
            .map_or(30_000, |host| host.connect_timeout_ms),
        keepalive_interval_ms: existing
            .as_ref()
            .map_or(Some(60_000), |host| host.keepalive_interval_ms),
        terminal_options: existing
            .as_ref()
            .map_or_else(Default::default, |host| host.terminal_options.clone()),
        sftp_options: existing
            .as_ref()
            .map_or_else(Default::default, |host| host.sftp_options.clone()),
        rdp_display: existing
            .as_ref()
            .map_or_else(Default::default, |host| host.rdp_display.clone()),
    };
    let expected_revision = existing.as_ref().map(|host| host.revision.clone());
    let slot = if protocol == RemoteProtocol::Rdp {
        HostCredentialSlot::RdpPassword
    } else {
        HostCredentialSlot::TargetPassword
    };
    let receipt = match hosts
        .save_host_with_credentials(
            draft,
            expected_revision,
            vec![HostCredentialChange::Replace {
                slot,
                secret: password,
            }],
            cancellation,
        )
        .await
    {
        Ok(receipt) => receipt,
        Err(error) => return failure(error),
    };
    let secret_ref = credential_reference(&receipt.host);
    let Some(secret_ref) = secret_ref else {
        return failure(AppError::infrastructure(
            "已保存主机缺少凭据引用，配置需要重新读取确认",
        ));
    };
    let cleanup_note = match receipt.cleanup {
        kerminal_application::connections::management::CredentialCleanupStatus::Complete => {
            String::new()
        }
        kerminal_application::connections::management::CredentialCleanupStatus::Deferred {
            remaining,
        } => format!("旧凭据清理暂缓（待核对 {} 条）。", remaining),
    };
    let host = receipt.host;
    success(
        format!(
            "主机已保存：{}@{}:{}，凭据已写入 encrypted vault。{}",
            host.endpoint.username, host.endpoint.host, host.endpoint.port, cleanup_note
        ),
        json!({
            "hostId": host.id.as_str(),
            "name": host.name,
            "host": host.endpoint.host,
            "port": host.endpoint.port,
            "username": host.endpoint.username,
            "authType": "password",
            "secretRef": secret_ref.as_str(),
            "credentialStatus": "vault"
        }),
        vec![],
    )
}

/// 加密保存独立凭据并返回旧版 secretRef 与索引元数据字段。
async fn encrypt_secret(
    services: &McpServices,
    arguments: Map<String, Value>,
    cancellation: CancellationToken,
) -> CallToolResult {
    let args = match parse_args::<EncryptSecretArgs>(arguments) {
        Ok(args) => args,
        Err(error) => return failure(error),
    };
    let plaintext = match secret_value(args.plaintext) {
        Ok(plaintext) => plaintext,
        Err(error) => return failure(error),
    };
    let kind_value = args.kind.trim();
    if !matches!(kind_value, "ssh-host" | "jump-host" | "rdp-host") {
        return failure(AppError::invalid("credential kind"));
    }
    let kind = match CredentialKind::parse(kind_value) {
        Ok(kind) => kind,
        Err(_) => return failure(AppError::invalid("credential kind")),
    };
    let host_id = match HostId::parse(args.host_id.trim()) {
        Ok(host_id) => host_id,
        Err(_) => return failure(AppError::invalid("host id")),
    };
    let scope = match CredentialScope::parse(args.scope.trim()) {
        Ok(scope) => scope,
        Err(_) => return failure(AppError::invalid("credential scope")),
    };
    let material = match CredentialMaterial::parse(args.material.trim()) {
        Ok(material) => material,
        Err(_) => return failure(AppError::invalid("credential material")),
    };
    let credentials = match services.require_credentials() {
        Ok(credentials) => credentials,
        Err(error) => return failure(error),
    };
    if cancellation.is_cancelled() {
        return failure(AppError::cancelled("MCP request was cancelled"));
    }
    let receipt = match credentials
        .save_with_metadata(SaveCredentialRequest {
            kind,
            host_id,
            scope,
            material,
            secret: plaintext,
        })
        .await
    {
        Ok(receipt) => receipt,
        Err(error) => return failure(error),
    };
    success(
        format!(
            "Secret 已加密保存到 Kerminal vault：{}。",
            receipt.reference.as_str()
        ),
        json!({
            "secretRef": receipt.reference.as_str(),
            "entryId": receipt.entry_id,
            "kind": receipt.kind,
            "keyId": receipt.key_id,
            "updatedAt": receipt.updated_at
        }),
        vec![],
    )
}

/// 用显式 ID 或历史 host/user/port 精确匹配既有条目，避免误建重复主机。
async fn find_existing_host(
    hosts: &HostManagementService,
    args: &HostUpsertArgs,
) -> kerminal_application::AppResult<
    Option<kerminal_application::connections::management::ManagedHost>,
> {
    if let Some(value) = args
        .id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let id = HostId::parse(value).map_err(|_| AppError::invalid("host id"))?;
        return hosts
            .load_host(&id)
            .await?
            .map(Some)
            .ok_or_else(|| AppError::not_found("主机不存在"));
    }
    let port = args.port.unwrap_or(22);
    Ok(hosts.list_hosts().await?.into_iter().find(|host| {
        host.endpoint.host == args.host.trim()
            && host.endpoint.username == args.username.trim()
            && host.endpoint.port == port
    }))
}

/// 空 groupId 表示清除分组，省略时保留现有分组归属。
fn requested_group(requested: Option<String>, existing: Option<String>) -> Option<String> {
    match requested {
        Some(group) if group.trim().is_empty() => None,
        Some(group) => Some(group.trim().to_owned()),
        None => existing,
    }
}

/// 从成功保存的类型化主机快照取出当前认证模式对应的 opaque 凭据引用。
fn credential_reference(
    host: &kerminal_application::connections::management::ManagedHost,
) -> Option<CredentialRef> {
    if host.protocol == RemoteProtocol::Rdp {
        return host.rdp_password.clone();
    }
    match host.authentication.as_ref() {
        Some(SshAuthentication::Password { secret }) => Some(secret.clone()),
        _ => None,
    }
}

/// 先把一次性输入纳入零化容器，再校验大小，避免失败分支遗留普通明文字符串。
fn secret_value(secret: String) -> Result<SecretValue, AppError> {
    let mut secret = Zeroizing::new(secret);
    if secret.trim().is_empty() || secret.len() > MAX_SECRET_BYTES {
        return Err(AppError::invalid("凭据不能为空或超过 1 MiB 的安全限制"));
    }
    Ok(SecretValue::new(std::mem::take(&mut *secret)))
}
