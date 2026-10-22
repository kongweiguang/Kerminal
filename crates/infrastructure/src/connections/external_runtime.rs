//! 外部启动目标的短期内存 host 与凭据仓储。
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{CredentialResolver, HostConnectionRepository, ResolvedCredential},
    platform::external_launch::{
        ExternalAuthenticationHint, ExternalTargetLease, ExternalTargetLeaseOwner,
        ExternalTemporaryTargetRegistry, ExternalTemporaryTargetSpec,
    },
};
use kerminal_domain::{
    connections::{
        CredentialRef, HostConnectionProfile, RemoteProtocol, RemoteShellDialect,
        SshAuthentication, SshJumpHost, SshRoute, credentials::CredentialMaterial,
    },
    identity::HostId,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroizing;

const MAX_TEMPORARY_TARGETS: usize = 64;
const MAX_PASSWORD_BYTES: usize = 1024 * 1024;

struct TemporaryEntry {
    profile: HostConnectionProfile,
    password_reference: Option<CredentialRef>,
    password: Option<Zeroizing<String>>,
    lease_scopes: usize,
}

/// 同一短期仓储解析临时 profile 和密码，并按每个独立消费者的 RAII scope 管理生命周期。
#[derive(Clone)]
pub struct ExternalRuntimeRegistry {
    saved_hosts: Arc<dyn HostConnectionRepository>,
    saved_credentials: Arc<dyn CredentialResolver>,
    temporary: Arc<Mutex<HashMap<HostId, TemporaryEntry>>>,
}

impl ExternalRuntimeRegistry {
    /// 以现有 host/vault 端口作 fallback；临时目标始终优先由 external ID 内存映射处理。
    pub fn new(
        saved_hosts: Arc<dyn HostConnectionRepository>,
        saved_credentials: Arc<dyn CredentialResolver>,
    ) -> Self {
        Self {
            saved_hosts,
            saved_credentials,
            temporary: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 返回仍有消费者持有的外部目标数，供容量边界测试和运行态检查使用。
    pub async fn active_target_count(&self) -> usize {
        self.temporary.lock().map_or(0, |entries| entries.len())
    }
}

#[async_trait]
impl ExternalTemporaryTargetRegistry for ExternalRuntimeRegistry {
    /// 只创建进程内 profile 并登记初始 owner；唯一性、容量和引用计数由同一短锁保证。
    async fn register(
        &self,
        target: ExternalTemporaryTargetSpec,
    ) -> AppResult<ExternalTargetLease> {
        validate_target(&target)?;
        let host_id_text = format!("external:{}", Uuid::new_v4());
        let host_id = HostId::parse(&host_id_text)
            .map_err(|_| AppError::infrastructure("无法创建临时外部主机标识"))?;
        let (authentication, password_reference) =
            authentication_for(&host_id, target.authentication)?;
        let jump_hosts = target
            .identity
            .jump_hosts
            .into_iter()
            .map(|endpoint| SshJumpHost {
                endpoint,
                authentication: SshAuthentication::Agent,
            })
            .collect();
        let profile = HostConnectionProfile {
            host_id: host_id.clone(),
            protocol: target.identity.protocol,
            endpoint: target.identity.endpoint,
            authentication: Some(authentication),
            rdp_password: None,
            route: SshRoute {
                proxy: None,
                jump_hosts,
            },
            shell_dialect: RemoteShellDialect::Unknown,
            connect_timeout_ms: 30_000,
            keepalive_interval_ms: Some(60_000),
        };

        let mut entries = self
            .temporary
            .lock()
            .map_err(|_| AppError::infrastructure("临时外部连接状态不可用"))?;
        if entries.len() >= MAX_TEMPORARY_TARGETS {
            return Err(AppError::unsupported(
                "临时外部连接已达到 64 个上限，请先关闭旧连接",
            ));
        }
        entries.insert(
            host_id.clone(),
            TemporaryEntry {
                profile,
                password_reference,
                password: None,
                lease_scopes: 1,
            },
        );
        drop(entries);
        ExternalTargetLease::new_registered(host_id, Arc::new(self.clone()))
    }

    /// 密码只按本次 external host ID 写入零化内存槽，不允许与持久化主机共享引用。
    async fn set_password(&self, host_id: &HostId, password: Zeroizing<String>) -> AppResult<()> {
        if password.is_empty()
            || password.len() > MAX_PASSWORD_BYTES
            || password.chars().any(char::is_control)
        {
            return Err(AppError::invalid("外部连接密码无效或超出安全限制"));
        }
        let mut entries = self
            .temporary
            .lock()
            .map_err(|_| AppError::infrastructure("临时外部连接状态不可用"))?;
        let entry = entries
            .get_mut(host_id)
            .ok_or_else(|| AppError::not_found("临时外部连接已关闭"))?;
        if entry.password_reference.is_none() {
            return Err(AppError::invalid("此临时连接未请求密码认证"));
        }
        entry.password = Some(password);
        Ok(())
    }

    /// 应用退出时清空所有临时 profile 与 Zeroizing 密码；旧 lease 后续 Drop 安全忽略。
    async fn clear_all(&self) -> AppResult<()> {
        let removed = {
            let mut entries = self
                .temporary
                .lock()
                .map_err(|_| AppError::infrastructure("临时外部连接状态不可用"))?;
            std::mem::take(&mut *entries)
        };
        drop(removed);
        Ok(())
    }
}

impl ExternalTargetLeaseOwner for ExternalRuntimeRegistry {
    /// 只允许有效 external ID 增加一个独立消费者计数，清理后的目标不可复活。
    fn retain_scope(&self, host_id: &HostId) -> AppResult<()> {
        if !host_id.as_str().starts_with("external:") {
            return Err(AppError::invalid("只能保留临时外部连接"));
        }
        let mut entries = self
            .temporary
            .lock()
            .map_err(|_| AppError::infrastructure("临时外部连接状态不可用"))?;
        let entry = entries
            .get_mut(host_id)
            .ok_or_else(|| AppError::not_found("临时外部连接已关闭"))?;
        entry.lease_scopes = entry
            .lease_scopes
            .checked_add(1)
            .ok_or_else(|| AppError::conflict("临时外部连接引用数已耗尽"))?;
        Ok(())
    }

    /// 最后一个独立消费者释放时同步删除 entry，使凭据析构立即执行零化。
    fn release_scope(&self, host_id: &HostId) {
        if !host_id.as_str().starts_with("external:") {
            return;
        }
        let mut entries = self
            .temporary
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let should_remove = match entries.get_mut(host_id) {
            Some(entry) if entry.lease_scopes > 1 => {
                entry.lease_scopes -= 1;
                false
            }
            Some(_) => true,
            None => false,
        };
        if should_remove {
            entries.remove(host_id);
        }
    }
}

#[async_trait]
impl HostConnectionRepository for ExternalRuntimeRegistry {
    /// external ID 只能命中进程内快照；调用方必须持有 lease 才能保证重连期间目标有效。
    async fn load(&self, host_id: &HostId) -> AppResult<HostConnectionProfile> {
        if host_id.as_str().starts_with("external:") {
            return self
                .temporary
                .lock()
                .map_err(|_| AppError::infrastructure("临时外部连接状态不可用"))?
                .get(host_id)
                .map(|entry| entry.profile.clone())
                .ok_or_else(|| AppError::not_found("临时外部连接已关闭"));
        }
        self.saved_hosts.load(host_id).await
    }
}

#[async_trait]
impl CredentialResolver for ExternalRuntimeRegistry {
    /// 临时 ref 只在注册项内解开；普通 ref 委托加密 vault，取消状态不跨越 resolver。
    async fn resolve(
        &self,
        reference: &CredentialRef,
        cancel: &CancellationToken,
    ) -> AppResult<ResolvedCredential> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("外部连接凭据解析已取消"));
        }
        if reference.as_str().starts_with("external:") {
            let entries = self
                .temporary
                .lock()
                .map_err(|_| AppError::infrastructure("临时外部连接状态不可用"))?;
            let Some(entry) = entries
                .values()
                .find(|entry| entry.password_reference.as_ref() == Some(reference))
            else {
                return Err(AppError::not_found("临时外部凭据已关闭"));
            };
            let password = entry
                .password
                .as_ref()
                .ok_or_else(AppError::authentication_failed)?;
            return Ok(ResolvedCredential::Password(password.clone()));
        }
        self.saved_credentials.resolve(reference, cancel).await
    }
}

/// 接受外部启动用例已经补全用户名的 SSH/SFTP 目标，并再次限制内存端口的输入边界。
fn validate_target(target: &ExternalTemporaryTargetSpec) -> AppResult<()> {
    if !matches!(
        target.identity.protocol,
        RemoteProtocol::Ssh | RemoteProtocol::Sftp
    ) {
        return Err(AppError::capability_not_supported(
            "external launch only supports SSH and SFTP targets",
        ));
    }
    target
        .identity
        .endpoint
        .validate()
        .map_err(|_| AppError::invalid("外部连接目标地址无效"))?;
    if target.identity.jump_hosts.len() > 8
        || target
            .identity
            .jump_hosts
            .iter()
            .any(|endpoint| endpoint.validate().is_err())
    {
        return Err(AppError::invalid("外部跳板链无效或超出 8 个跳板限制"));
    }
    if target
        .display_name
        .as_deref()
        .is_some_and(|name| name.chars().any(char::is_control) || name.chars().count() > 128)
    {
        return Err(AppError::invalid("外部连接显示名称无效"));
    }
    Ok(())
}

/// 将外部认证提示映射为运行 profile 与不可持久化密码引用；私钥路径已是拥有值，无需再包装复制。
///
/// @author kongweiguang
fn authentication_for(
    host_id: &HostId,
    hint: ExternalAuthenticationHint,
) -> AppResult<(SshAuthentication, Option<CredentialRef>)> {
    match hint {
        ExternalAuthenticationHint::Agent => Ok((SshAuthentication::Agent, None)),
        ExternalAuthenticationHint::PrivateKey { path } => {
            if path.as_os_str().is_empty() {
                return Err(AppError::invalid("外部私钥路径为空"));
            }
            Ok((
                SshAuthentication::PrivateKey {
                    path: Some(path),
                    secret: None,
                    passphrase: None,
                },
                None,
            ))
        }
        ExternalAuthenticationHint::PasswordPrompt => {
            let reference = CredentialRef::new(format!(
                "external:{}:target:{}",
                host_id.as_str(),
                CredentialMaterial::Password.as_str(),
            ))
            .map_err(|_| AppError::invalid("外部临时凭据引用无效"))?;
            Ok((
                SshAuthentication::Password {
                    secret: reference.clone(),
                },
                Some(reference),
            ))
        }
    }
}
