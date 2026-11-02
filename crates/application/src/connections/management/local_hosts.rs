//! 本地主机快照和加密环境的应用用例。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

use kerminal_domain::{
    connections::{
        CredentialRef,
        credentials::{CredentialKind, CredentialMaterial, CredentialReference, CredentialScope},
    },
    identity::HostId,
    preferences::{ConfigDocumentPath, ConfigRevision},
    sessions::LocalSessionTarget,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroize;

use super::{
    CredentialCleanupStatus, HostDeletionReceipt, HostManagementService, LocalHostDraft,
    LocalHostSaveRecoveryToken, ManagedLocalHost, RecoverableLocalHostSaveOutcome,
};
use crate::{
    AppError, AppResult, PublicErrorCode,
    connections::{
        ResolvedCredential,
        credentials::{SaveCredentialRequest, SecretValue},
    },
};

const LOCAL_ENVIRONMENT_KIND: &str = "local-host";
const LOCAL_ENVIRONMENT_SCOPE: &str = "environment-";

impl HostManagementService {
    /// 只读取本地主机元数据并稳定排序，vault 不可用时侧栏仍可显示配置。
    ///
    /// @author kongweiguang
    pub async fn list_local_hosts(&self) -> AppResult<Vec<ManagedLocalHost>> {
        let mut hosts = self.repository.list_local_hosts().await?;
        hosts.sort_by(|left, right| {
            left.sort_order
                .cmp(&right.sort_order)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.id.as_str().cmp(right.id.as_str()))
        });
        Ok(hosts)
    }

    /// 按本地主机身份读取最新快照，缺失或远程主机不能被解释成模板。
    ///
    /// @author kongweiguang
    pub async fn load_local_host(&self, host_id: &HostId) -> AppResult<Option<ManagedLocalHost>> {
        self.repository.load_local_host(host_id).await
    }

    /// 仅在编辑或启动时解密环境，并验证引用所属主机与材料类别。
    ///
    /// @author kongweiguang
    pub async fn read_local_environment(
        &self,
        host: &ManagedLocalHost,
        cancel: &CancellationToken,
    ) -> AppResult<BTreeMap<String, String>> {
        let _gate = tokio::select! {
            _ = cancel.cancelled() => return Err(AppError::cancelled("本地主机环境读取已取消")),
            guard = self.local_host_gate.lock() => guard,
        };
        let _operation = self.repository.acquire_local_host_operation().await?;
        let current = self
            .repository
            .load_local_host(&host.id)
            .await?
            .ok_or_else(|| AppError::not_found("本地主机不存在"))?;
        if current.revision != host.revision {
            return Err(AppError::conflict("本地主机已变化，请重新读取后编辑"));
        }
        self.read_local_environment_unlocked(&current, cancel).await
    }

    /// 由持有本地主机单飞锁的调用方解密，避免并发保存提前清理正在使用的引用。
    ///
    /// @author kongweiguang
    async fn read_local_environment_unlocked(
        &self,
        host: &ManagedLocalHost,
        cancel: &CancellationToken,
    ) -> AppResult<BTreeMap<String, String>> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("本地主机环境读取已取消"));
        }
        let Some(reference) = host.environment_ref.as_ref() else {
            return Ok(BTreeMap::new());
        };
        validate_local_environment_reference(&host.id, reference)?;
        let resolver = self
            .local_environment_resolver
            .as_ref()
            .ok_or_else(|| AppError::unsupported("本地主机环境解密不可用"))?;
        let plaintext = match resolver.resolve(reference, cancel).await? {
            ResolvedCredential::Password(value) => value,
            _ => return Err(AppError::configuration_corrupt("本地主机环境材料类型无效")),
        };
        let environment = serde_json::from_str::<BTreeMap<String, String>>(&plaintext)
            .map_err(|_| AppError::configuration_corrupt("本地主机环境格式无效"))?;
        validate_local_environment(&environment)?;
        Ok(environment)
    }

    /// 按当前主机快照解析启动目标；环境无法解密时不得改用其他主机或默认模板。
    ///
    /// @author kongweiguang
    pub async fn local_session_target(
        &self,
        host_id: &HostId,
        cancel: &CancellationToken,
    ) -> AppResult<Option<LocalSessionTarget>> {
        let _gate = tokio::select! {
            _ = cancel.cancelled() => return Err(AppError::cancelled("本地主机目标读取已取消")),
            guard = self.local_host_gate.lock() => guard,
        };
        let _operation = self.repository.acquire_local_host_operation().await?;
        let Some(host) = self.repository.load_local_host(host_id).await? else {
            return Ok(None);
        };
        let env = self.read_local_environment_unlocked(&host, cancel).await?;
        Ok(Some(LocalSessionTarget {
            program: OsString::from(host.shell),
            args: host.args.into_iter().map(OsString::from).collect(),
            working_directory: host.cwd.map(PathBuf::from),
            environment: env
                .into_iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value)))
                .collect(),
        }))
    }

    /// 先以唯一引用密封完整环境，再按 host revision 保存；未知结果交给调用方读回。
    ///
    /// @author kongweiguang
    pub async fn save_local_host_recoverable(
        &self,
        mut draft: LocalHostDraft,
        expected_revision: Option<ConfigRevision>,
        cancel: CancellationToken,
    ) -> AppResult<RecoverableLocalHostSaveOutcome> {
        let _gate = tokio::select! {
            _ = cancel.cancelled() => return Err(AppError::cancelled("本地主机保存已取消")),
            guard = self.local_host_gate.lock() => guard,
        };
        let _operation = self.repository.acquire_local_host_operation().await?;
        validate_local_host_draft(&draft)?;
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("本地主机保存已取消"));
        }
        let host_id = draft.id.clone().unwrap_or_default();
        ConfigDocumentPath::Host(host_id.as_str().to_owned())
            .relative_path()
            .map_err(|_| AppError::invalid("本地主机标识不能用于配置文件名"))?;
        draft.id = Some(host_id.clone());
        self.repository.ensure_host_writable().await?;
        if let Some(group_id) = draft.group_id.as_deref() {
            let groups = self.repository.load_groups().await?;
            if !groups.groups.iter().any(|group| group.id == group_id) {
                return Err(AppError::not_found("本地主机所属分组不存在"));
            }
        }
        let existing = self.repository.load_local_host(&host_id).await?;
        match (expected_revision.as_ref(), existing.as_ref()) {
            (None, None) => {}
            (Some(expected), Some(current)) if expected == &current.revision => {}
            _ => return Err(AppError::conflict("本地主机已发生变化，请重新读取后保存")),
        }
        let old_reference = existing.and_then(|host| host.environment_ref);
        if (!draft.env.is_empty() || old_reference.is_some()) && self.credentials.is_none() {
            return Err(AppError::unsupported("本地主机环境密封不可用"));
        }
        let prepared = if !draft.env.is_empty() {
            let encoded = serde_json::to_string(&draft.env)
                .map_err(|_| AppError::invalid("本地主机环境无法编码"))?;
            for value in draft.env.values_mut() {
                value.zeroize();
            }
            draft.env.clear();
            let scope = CredentialScope::parse(&format!(
                "{LOCAL_ENVIRONMENT_SCOPE}{}",
                Uuid::new_v4().simple()
            ))
            .map_err(|_| AppError::invalid("无法生成本地主机环境引用"))?;
            let kind = CredentialKind::parse(LOCAL_ENVIRONMENT_KIND)
                .map_err(|_| AppError::invalid("本地主机环境引用无效"))?;
            let candidate = CredentialReference::new(
                kind.clone(),
                host_id.clone(),
                scope.clone(),
                CredentialMaterial::Password,
            )
            .map_err(|_| AppError::invalid("本地主机环境引用无效"))?
            .reference()
            .clone();
            Some((kind, scope, candidate, SecretValue::new(encoded)))
        } else {
            None
        };
        let recovery = LocalHostSaveRecoveryToken::from_parts(
            Uuid::new_v4().to_string(),
            host_id.clone(),
            expected_revision.clone(),
            old_reference,
            prepared
                .as_ref()
                .map(|(_, _, candidate, _)| candidate.clone()),
        )?;
        if let Err(error) = self.repository.begin_local_host_recovery(&recovery).await {
            return if error.public_code() == PublicErrorCode::OutcomeUnknown {
                Ok(RecoverableLocalHostSaveOutcome::OutcomeUnknown { error, recovery })
            } else {
                Err(error)
            };
        }
        if let Some((kind, scope, candidate, secret)) = prepared {
            let credentials = self
                .credentials
                .as_ref()
                .ok_or_else(|| AppError::unsupported("本地主机环境密封不可用"))?;
            let result = credentials
                .save(SaveCredentialRequest {
                    kind,
                    host_id: host_id.clone(),
                    scope,
                    material: CredentialMaterial::Password,
                    secret,
                })
                .await;
            match result {
                Ok(reference) if reference == candidate => {}
                Ok(_) => {
                    return Ok(RecoverableLocalHostSaveOutcome::OutcomeUnknown {
                        error: AppError::outcome_unknown("本地主机环境引用不一致，请先读回核对"),
                        recovery,
                    });
                }
                Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                    return Ok(RecoverableLocalHostSaveOutcome::OutcomeUnknown { error, recovery });
                }
                Err(error) if error.public_code() == PublicErrorCode::Infrastructure => {
                    return Ok(RecoverableLocalHostSaveOutcome::OutcomeUnknown {
                        error: AppError::outcome_unknown_with_source(
                            "本地主机环境保存结果未知，请先读回核对",
                            error,
                        ),
                        recovery,
                    });
                }
                Err(error) => {
                    return match self.repository.finish_local_host_recovery(&recovery).await {
                        Ok(()) => Err(error),
                        Err(_) => Ok(RecoverableLocalHostSaveOutcome::OutcomeUnknown {
                            error: AppError::outcome_unknown(
                                "本地主机恢复记录清理结果未知，请先读回核对",
                            ),
                            recovery,
                        }),
                    };
                }
            }
        }
        if cancel.is_cancelled() {
            let cleanup = self
                .cleanup_local_environment(&recovery.host_id, recovery.new_reference.as_ref())
                .await;
            return if cleanup == CredentialCleanupStatus::Complete
                && self
                    .repository
                    .finish_local_host_recovery(&recovery)
                    .await
                    .is_ok()
            {
                Err(AppError::cancelled("本地主机保存已取消"))
            } else {
                Ok(RecoverableLocalHostSaveOutcome::OutcomeUnknown {
                    error: AppError::outcome_unknown("本地主机环境清理结果未知，请先读回核对"),
                    recovery,
                })
            };
        }
        let result = self
            .repository
            .save_local_host(
                host_id,
                draft,
                recovery.new_reference.clone(),
                expected_revision,
            )
            .await;
        match result {
            Ok(saved) => {
                let credential_cleanup = self
                    .cleanup_local_environment(&recovery.host_id, recovery.old_reference.as_ref())
                    .await;
                if credential_cleanup == CredentialCleanupStatus::Complete
                    && self
                        .repository
                        .finish_local_host_recovery(&recovery)
                        .await
                        .is_err()
                {
                    return Ok(RecoverableLocalHostSaveOutcome::OutcomeUnknown {
                        error: AppError::outcome_unknown(
                            "本地主机恢复记录清理结果未知，请先读回核对",
                        ),
                        recovery,
                    });
                }
                Ok(RecoverableLocalHostSaveOutcome::Saved {
                    host: saved,
                    credential_cleanup,
                })
            }
            Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                Ok(RecoverableLocalHostSaveOutcome::OutcomeUnknown { error, recovery })
            }
            Err(error) => {
                let cleanup = self
                    .cleanup_local_environment(&recovery.host_id, recovery.new_reference.as_ref())
                    .await;
                if cleanup == CredentialCleanupStatus::Complete
                    && self
                        .repository
                        .finish_local_host_recovery(&recovery)
                        .await
                        .is_ok()
                {
                    Err(error)
                } else {
                    Ok(RecoverableLocalHostSaveOutcome::OutcomeUnknown {
                        error: AppError::outcome_unknown("本地主机环境清理结果未知，请先读回核对"),
                        recovery,
                    })
                }
            }
        }
    }

    /// 只有真实读回确认引用归属后才删除本次未被使用的密文。
    ///
    /// @author kongweiguang
    pub async fn recover_unknown_local_host_save(
        &self,
        recovery: &LocalHostSaveRecoveryToken,
    ) -> AppResult<CredentialCleanupStatus> {
        let _gate = self.local_host_gate.lock().await;
        let _operation = self.repository.acquire_local_host_operation().await?;
        self.recover_local_host_save_unlocked(recovery).await
    }

    /// 在单飞锁内核对磁盘并清理候选，避免恢复与尚在运行的保存互相删除密文。
    ///
    /// @author kongweiguang
    async fn recover_local_host_save_unlocked(
        &self,
        recovery: &LocalHostSaveRecoveryToken,
    ) -> AppResult<CredentialCleanupStatus> {
        let _ = self.repository.load_local_host(&recovery.host_id).await?;
        let old = self
            .cleanup_local_environment(&recovery.host_id, recovery.old_reference.as_ref())
            .await;
        let new = self
            .cleanup_local_environment(&recovery.host_id, recovery.new_reference.as_ref())
            .await;
        let cleanup = match (old, new) {
            (CredentialCleanupStatus::Complete, CredentialCleanupStatus::Complete) => {
                CredentialCleanupStatus::Complete
            }
            (
                CredentialCleanupStatus::Deferred { remaining: left },
                CredentialCleanupStatus::Deferred { remaining: right },
            ) => CredentialCleanupStatus::Deferred {
                remaining: left + right,
            },
            (CredentialCleanupStatus::Deferred { remaining }, _)
            | (_, CredentialCleanupStatus::Deferred { remaining }) => {
                CredentialCleanupStatus::Deferred { remaining }
            }
        };
        if cleanup == CredentialCleanupStatus::Complete {
            self.repository.finish_local_host_recovery(recovery).await?;
        }
        Ok(cleanup)
    }

    /// 重启后读取持久事务并核对真实主机引用；未完成清理继续保留日志供重试。
    ///
    /// @author kongweiguang
    pub async fn recover_pending_local_host_save(&self) -> AppResult<CredentialCleanupStatus> {
        let _gate = self.local_host_gate.lock().await;
        let _operation = self.repository.acquire_local_host_operation().await?;
        match self.repository.pending_local_host_recovery().await? {
            Some(token) => self.recover_local_host_save_unlocked(&token).await,
            None => Ok(CredentialCleanupStatus::Complete),
        }
    }

    /// 确认 host CAS 删除后再清理环境密文；清理延迟不能把已删除主机伪装成失败。
    ///
    /// @author kongweiguang
    pub async fn delete_local_host(
        &self,
        host_id: &HostId,
        expected_revision: &ConfigRevision,
    ) -> AppResult<HostDeletionReceipt> {
        let _gate = self.local_host_gate.lock().await;
        let _operation = self.repository.acquire_local_host_operation().await?;
        self.repository.ensure_host_writable().await?;
        let current = self
            .repository
            .load_local_host(host_id)
            .await?
            .ok_or_else(|| AppError::not_found("本地主机不存在"))?;
        if &current.revision != expected_revision {
            return Err(AppError::conflict("本地主机已变化，请重新读取后删除"));
        }
        let recovery = LocalHostSaveRecoveryToken::from_parts(
            Uuid::new_v4().to_string(),
            host_id.clone(),
            Some(expected_revision.clone()),
            current.environment_ref.clone(),
            None,
        )?;
        self.repository.begin_local_host_recovery(&recovery).await?;
        let deleted = match self
            .repository
            .delete_local_host(host_id, expected_revision)
            .await
        {
            Ok(host) => host,
            Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                return Err(error);
            }
            Err(error) => {
                if self
                    .repository
                    .finish_local_host_recovery(&recovery)
                    .await
                    .is_err()
                {
                    return Err(AppError::outcome_unknown(
                        "本地主机恢复记录清理结果未知，请先读回核对",
                    ));
                }
                return Err(error);
            }
        };
        let credential_cleanup = self
            .cleanup_local_environment(host_id, deleted.environment_ref.as_ref())
            .await;
        if credential_cleanup == CredentialCleanupStatus::Complete {
            self.repository
                .finish_local_host_recovery(&recovery)
                .await
                .map_err(|_| {
                    AppError::outcome_unknown("本地主机删除已提交，但恢复记录清理结果未知")
                })?;
        }
        Ok(HostDeletionReceipt { credential_cleanup })
    }

    /// 清理前重新读取同一 host；错误、错位引用和不确定结果均保留密文。
    ///
    /// @author kongweiguang
    async fn cleanup_local_environment(
        &self,
        host_id: &HostId,
        reference: Option<&CredentialRef>,
    ) -> CredentialCleanupStatus {
        let Some(reference) = reference else {
            return CredentialCleanupStatus::Complete;
        };
        if validate_local_environment_reference(host_id, reference).is_err() {
            return CredentialCleanupStatus::Deferred { remaining: 1 };
        }
        match self.repository.load_local_host(host_id).await {
            Ok(Some(host)) if host.environment_ref.as_ref() == Some(reference) => {
                return CredentialCleanupStatus::Complete;
            }
            Ok(_) => {}
            Err(_) => return CredentialCleanupStatus::Deferred { remaining: 1 },
        }
        let Some(credentials) = self.credentials.as_ref() else {
            return CredentialCleanupStatus::Deferred { remaining: 1 };
        };
        if credentials.remove(reference).await.is_err() {
            CredentialCleanupStatus::Deferred { remaining: 1 }
        } else {
            CredentialCleanupStatus::Complete
        }
    }
}

/// 本地环境引用必须绑定所属主机和独立作用域，不能借用 SSH 密码或其他主机密文。
///
/// @author kongweiguang
pub fn validate_local_environment_reference(
    host_id: &HostId,
    reference: &CredentialRef,
) -> AppResult<()> {
    let parsed = CredentialReference::parse(reference.clone())
        .map_err(|_| AppError::invalid("本地主机环境引用无效"))?;
    let suffix = parsed
        .scope()
        .as_str()
        .strip_prefix(LOCAL_ENVIRONMENT_SCOPE);
    if parsed.kind().as_str() != LOCAL_ENVIRONMENT_KIND
        || parsed.host_id() != host_id
        || parsed.material() != CredentialMaterial::Password
        || !suffix.is_some_and(|value| {
            value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return Err(AppError::invalid("本地主机环境引用与主机不匹配"));
    }
    Ok(())
}

/// 本地启动字段沿用 Profile 的有限输入边界，不允许无意义命令或畸形环境键。
///
/// @author kongweiguang
fn validate_local_host_draft(draft: &LocalHostDraft) -> AppResult<()> {
    if draft.name.trim().is_empty() || draft.name.len() > 256 || draft.name.contains('\0') {
        return Err(AppError::invalid("本地主机名称不能为空或过长"));
    }
    if draft.shell.trim().is_empty() || draft.shell.len() > 1024 || draft.shell.contains('\0') {
        return Err(AppError::invalid("本地主机 shell 路径不能为空或过长"));
    }
    if draft
        .cwd
        .as_ref()
        .is_some_and(|cwd| cwd.len() > 4096 || cwd.contains('\0'))
        || draft
            .args
            .iter()
            .any(|argument| argument.len() > 16 * 1024 || argument.contains('\0'))
    {
        return Err(AppError::invalid("本地主机参数或工作目录超出限制"));
    }
    validate_local_environment(&draft.env)
}

/// 解密读回与新草稿共用同一边界，损坏密文不得成为启动环境。
///
/// @author kongweiguang
fn validate_local_environment(environment: &BTreeMap<String, String>) -> AppResult<()> {
    if environment.len() > 256
        || environment.iter().any(|(key, value)| {
            key.is_empty()
                || key.len() > 1024
                || key.contains('\0')
                || key.contains('=')
                || value.len() > 16 * 1024
                || value.contains('\0')
        })
    {
        return Err(AppError::invalid("本地主机环境变量超出限制"));
    }
    Ok(())
}
