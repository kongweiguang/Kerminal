//! 插件主机适配器：统一目录投影、codec 和宿主 revision 事务。
//!
//! @author kongweiguang

use std::{ffi::OsStr, fs, io, path::Path};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        host_types::{PluginHost, PluginHostDraft},
        management::{
            CredentialCleanupStatus, HostCredentialCleanup, HostCredentialReferenceCoverage,
            ReferencedCredentialBehavior,
        },
    },
};
use kerminal_domain::{
    connections::CredentialRef,
    identity::HostId,
    preferences::{ConfigDocument, ConfigDocumentPath, ConfigRevision},
};
use tokio::task;
use toml_edit::{DocumentMut, Item};

use crate::{configuration::ConfigurationStore, storage::StorageLock, vault::VaultStore};

use super::{
    HostConfigurationRepository, corrupt_host, set_integer, set_optional_string, set_string,
    timestamp_now,
};

impl HostConfigurationRepository {
    /// 扫描放入阻塞池并取得共享锁，与生产清理使用完全相同的覆盖规则。
    ///
    /// @author kongweiguang
    pub(super) async fn scan_credential_reference_coverage(
        &self,
    ) -> AppResult<HostCredentialReferenceCoverage> {
        let repository = self.clone();
        task::spawn_blocking(move || {
            let storage = repository.store.storage_coordinator();
            let lock = storage.lock().map_err(|source| {
                AppError::infrastructure_with_source("无法锁定主机凭据目录", source)
            })?;
            repository.scan_credential_reference_coverage_locked(&lock)
        })
        .await
        .map_err(|source| AppError::infrastructure_with_source("扫描主机凭据引用失败", source))?
    }

    /// 全部类型及目录复核在同一锁内完成；外部绕过共享锁的写入仍需单独协调。
    ///
    /// @author kongweiguang
    fn scan_credential_reference_coverage_locked(
        &self,
        lock: &StorageLock<'_>,
    ) -> AppResult<HostCredentialReferenceCoverage> {
        self.store.ensure_host_documents_readable_locked(lock)?;
        let ids = enumerate_credential_host_ids(&self.root.join("hosts"))?;
        let mut revisions = Vec::with_capacity(ids.len());
        let mut remote_hosts = Vec::new();
        let mut additional_references = Vec::new();
        for id in &ids {
            let document = self
                .store
                .read_host_document_locked(lock, id.as_str())?
                .ok_or_else(|| AppError::conflict("主机目录在读取期间发生变化，请刷新后重试"))?;
            self.store
                .validate_host_document(id.as_str(), &document.source)?;
            revisions.push(document.revision.clone());
            if Self::document_is_local(&document)? {
                additional_references
                    .extend(Self::decode_local_host(document, id)?.environment_ref);
            } else if Self::document_is_plugin(&document)? {
                let host = self.decode_plugin_host(document, id)?;
                let Some(host_type) = self
                    .store
                    .host_types()
                    .get(&host.provider_id, &host.type_id)
                else {
                    return Ok(HostCredentialReferenceCoverage::Unproven);
                };
                let Some(configuration) = host.configuration.as_ref() else {
                    return Ok(HostCredentialReferenceCoverage::Unproven);
                };
                let Some(references) = host_type
                    .codec
                    .credential_references(host.configuration_version, configuration)?
                else {
                    return Ok(HostCredentialReferenceCoverage::Unproven);
                };
                additional_references.extend(references);
            } else {
                remote_hosts.push(Self::decode_host(document, id)?);
            }
        }
        for (id, revision) in ids.iter().zip(revisions) {
            let current = self.store.read_host_document_locked(lock, id.as_str())?;
            if current.is_none_or(|document| document.revision != revision) {
                return Ok(HostCredentialReferenceCoverage::Unproven);
            }
        }
        if enumerate_credential_host_ids(&self.root.join("hosts"))? != ids {
            return Ok(HostCredentialReferenceCoverage::Unproven);
        }
        Ok(HostCredentialReferenceCoverage::Complete {
            remote_hosts,
            additional_references,
        })
    }

    /// 扩展类型与六类内置 token 分开；非法文档必须先经过统一 validator 拒绝。
    pub(super) fn document_is_plugin(document: &ConfigDocument) -> AppResult<bool> {
        let value = toml::from_str::<toml::Value>(&document.source).map_err(corrupt_host)?;
        let protocol = value
            .get("protocol")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| AppError::configuration_corrupt("主机配置"))?;
        Ok(!matches!(
            protocol,
            "local" | "ssh" | "sftp" | "rdp" | "telnet" | "serial"
        ))
    }

    /// 已安装 codec 才生成业务投影；未知提供方只展示元数据并保留磁盘原文。
    pub(super) fn decode_plugin_host(
        &self,
        document: ConfigDocument,
        id: &HostId,
    ) -> AppResult<PluginHost> {
        self.store
            .validate_host_document(id.as_str(), &document.source)?;
        let value = toml::from_str::<toml::Value>(&document.source).map_err(corrupt_host)?;
        let field = |name: &str| {
            value
                .get(name)
                .and_then(toml::Value::as_str)
                .ok_or_else(|| AppError::configuration_corrupt("插件主机配置"))
        };
        let provider_id = field("provider_id")?.to_owned();
        let type_id = field("protocol")?.to_owned();
        let name = field("name")?.to_owned();
        let version = value
            .get("configuration_version")
            .and_then(toml::Value::as_integer)
            .and_then(|version| u32::try_from(version).ok())
            .ok_or_else(|| AppError::configuration_corrupt("插件主机配置"))?;
        let registered_type = self.store.host_types().get(&provider_id, &type_id);
        let runtime_available = self
            .store
            .host_types()
            .runtime_provider(&provider_id, &type_id)?
            .is_some();
        let configuration = match registered_type {
            Some(host_type) => Some(
                host_type.codec.decode(
                    version,
                    value
                        .get("configuration")
                        .ok_or_else(|| AppError::configuration_corrupt("插件主机配置"))?,
                )?,
            ),
            None => None,
        };
        Ok(PluginHost {
            id: id.clone(),
            provider_id,
            type_id,
            name,
            group_id: value
                .get("group_id")
                .and_then(toml::Value::as_str)
                .map(str::to_owned),
            sort_order: value
                .get("sort_order")
                .and_then(toml::Value::as_integer)
                .ok_or_else(|| AppError::configuration_corrupt("插件主机配置"))?,
            configuration_version: version,
            configuration,
            runtime_available,
            revision: document.revision,
        })
    }

    /// 列表逐项校验外部编辑；只有合法文档才能参与任一目录投影。
    ///
    /// 原始文档复用与远程/本地列表相同的单次锁内快照，插件 codec 仍在锁外执行，
    /// 避免按文件重复获取跨进程锁，同时保持未知提供方和坏文档的原有分类。
    ///
    /// @author kongweiguang
    pub(super) async fn list_plugin_host_records(&self) -> AppResult<Vec<PluginHost>> {
        let snapshot = self.host_document_snapshot().await?;
        let mut hosts = Vec::with_capacity(snapshot.len());
        for (id, document) in snapshot {
            self.store
                .validate_host_document(id.as_str(), &document.source)?;
            if Self::document_is_plugin(&document)? {
                hosts.push(self.decode_plugin_host(document, &id)?);
            }
        }
        Ok(hosts)
    }

    /// 以主机 ID 精确回读，其他类型不会被解释为扩展主机。
    pub(super) async fn load_plugin_host_record(
        &self,
        id: &HostId,
    ) -> AppResult<Option<PluginHost>> {
        let Some(document) = self
            .configuration
            .read(ConfigDocumentPath::Host(id.as_str().to_owned()))
            .await?
        else {
            return Ok(None);
        };
        self.store
            .validate_host_document(id.as_str(), &document.source)?;
        if Self::document_is_plugin(&document)? {
            self.decode_plugin_host(document, id).map(Some)
        } else {
            Ok(None)
        }
    }

    /// 保留文件顶层未知字段，插件字段由其 codec 生成后经共享 CAS 提交并读回核对。
    pub(super) async fn save_plugin_host_record(
        &self,
        id: HostId,
        draft: PluginHostDraft,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<PluginHost> {
        let host_type = self
            .store
            .host_types()
            .get(&draft.provider_id, &draft.type_id)
            .ok_or_else(|| AppError::unsupported("插件主机提供方不可用"))?;
        let encoded = host_type
            .codec
            .encode(draft.configuration_version, &draft.configuration)?;
        if !encoded.is_table() {
            return Err(AppError::invalid("插件主机配置必须是表"));
        }
        host_type
            .codec
            .decode(draft.configuration_version, &encoded)?;
        let (mut document, existing) = self
            .editable_document(&id, expected_revision.as_ref())
            .await?;
        if let Some(current) = &existing {
            self.store
                .validate_host_document(id.as_str(), &current.source)?;
            let previous = self.decode_plugin_host(current.clone(), &id)?;
            if previous.provider_id != draft.provider_id
                || previous.type_id != draft.type_id
                || !previous.is_available()
            {
                return Err(AppError::conflict("主机类型或提供方已变化，请重新读取"));
            }
        }
        let now = timestamp_now();
        let root = document.as_table_mut();
        if existing.is_none() {
            set_integer(
                root,
                "schema_version",
                i64::from(super::HOST_SCHEMA_VERSION),
            );
            set_string(root, "created_at", &now);
        }
        set_string(root, "id", id.as_str());
        set_string(root, "protocol", &draft.type_id);
        set_string(root, "provider_id", &draft.provider_id);
        set_string(root, "name", &draft.name);
        set_optional_string(root, "group_id", draft.group_id.as_deref());
        set_integer(root, "sort_order", draft.sort_order);
        set_integer(
            root,
            "configuration_version",
            i64::from(draft.configuration_version),
        );
        set_string(root, "updated_at", &now);
        let source = toml::to_string(&encoded).map_err(corrupt_host)?;
        let configuration = source.parse::<DocumentMut>().map_err(corrupt_host)?;
        root.insert(
            "configuration",
            Item::Table(configuration.as_table().clone()),
        );
        let path = ConfigDocumentPath::Host(id.as_str().to_owned());
        let revision = self
            .configuration
            .write(path.clone(), document.to_string(), expected_revision)
            .await?;
        let saved = self
            .configuration
            .read(path)
            .await
            .map_err(|error| {
                AppError::outcome_unknown_with_source("插件主机保存结果需读回确认", error)
            })?
            .ok_or_else(|| AppError::outcome_unknown("插件主机保存结果需读回确认"))?;
        if saved.revision != revision {
            return Err(AppError::outcome_unknown(
                "插件主机保存后已变化，请重新读取",
            ));
        }
        self.decode_plugin_host(saved, &id).map_err(|error| {
            AppError::outcome_unknown_with_source("插件主机保存结果需读回确认", error)
        })
    }
}

/// 使用配置仓储的同一个协调器构造 Vault，保证扫描和清理属于同一数据根及锁域。
///
/// @author kongweiguang
#[derive(Clone)]
pub struct AtomicHostCredentialCleanup {
    repository: HostConfigurationRepository,
    vault: VaultStore,
}

impl AtomicHostCredentialCleanup {
    /// 不接受外部 Vault，避免不同数据根或不同协调器破坏锁内清理约束。
    ///
    /// @author kongweiguang
    pub fn new(store: ConfigurationStore) -> Self {
        Self {
            vault: VaultStore::open(store.storage_coordinator()),
            repository: HostConfigurationRepository::new(store),
        }
    }

    /// 保持锁直到删除结束；任何删除错误都停止后续候选并保留待核对状态。
    ///
    /// @author kongweiguang
    fn cleanup_locked(
        &self,
        candidates: &[CredentialRef],
        referenced: ReferencedCredentialBehavior,
    ) -> AppResult<CredentialCleanupStatus> {
        let storage = self.repository.store.storage_coordinator();
        let lock = storage.lock().map_err(|source| {
            AppError::infrastructure_with_source("无法锁定主机凭据目录", source)
        })?;
        let coverage = self
            .repository
            .scan_credential_reference_coverage_locked(&lock)?;
        if matches!(coverage, HostCredentialReferenceCoverage::Unproven) {
            return Ok(CredentialCleanupStatus::Deferred {
                remaining: candidates.len(),
            });
        }
        let mut remaining = 0;
        for (index, reference) in candidates.iter().enumerate() {
            if coverage.contains_reference(reference) == Some(true) {
                if referenced == ReferencedCredentialBehavior::Defer {
                    remaining += 1;
                }
            } else if self
                .vault
                .remove_with_storage_lock(&lock, reference)
                .is_err()
            {
                remaining += candidates.len() - index;
                break;
            }
        }
        Ok(if remaining == 0 {
            CredentialCleanupStatus::Complete
        } else {
            CredentialCleanupStatus::Deferred { remaining }
        })
    }
}

#[async_trait]
impl HostCredentialCleanup for AtomicHostCredentialCleanup {
    /// 失败和结果未知都按候选数量延迟，避免在异步任务失败后伪称清理成功。
    ///
    /// @author kongweiguang
    async fn cleanup(
        &self,
        candidates: &[CredentialRef],
        referenced: ReferencedCredentialBehavior,
    ) -> AppResult<CredentialCleanupStatus> {
        if candidates.is_empty() {
            return Ok(CredentialCleanupStatus::Complete);
        }
        let count = candidates.len();
        let candidates = candidates.to_vec();
        let adapter = self.clone();
        Ok(
            match task::spawn_blocking(move || adapter.cleanup_locked(&candidates, referenced))
                .await
            {
                Ok(Ok(status)) => status,
                Ok(Err(_)) | Err(_) => CredentialCleanupStatus::Deferred { remaining: count },
            },
        )
    }
}

/// 清理扫描不能跳过可疑 TOML、链接或非法名称，否则无法证明引用集合完整。
///
/// @author kongweiguang
fn enumerate_credential_host_ids(directory: &Path) -> AppResult<Vec<HostId>> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {}
        Ok(_) => return Err(AppError::configuration_corrupt("主机目录")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(AppError::infrastructure_with_source(
                "读取主机目录失败",
                source,
            ));
        }
    }
    let entries = fs::read_dir(directory)
        .map_err(|source| AppError::infrastructure_with_source("读取主机目录失败", source))?;
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry
            .map_err(|source| AppError::infrastructure_with_source("枚举主机目录失败", source))?;
        let path = entry.path();
        if path.extension() != Some(OsStr::new("toml"))
            || entry.file_name() == OsStr::new("groups.toml")
        {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|source| AppError::infrastructure_with_source("检查主机配置失败", source))?;
        if !file_type.is_file() {
            return Err(AppError::configuration_corrupt("主机配置"));
        }
        let id = path
            .file_stem()
            .and_then(OsStr::to_str)
            .ok_or_else(|| AppError::configuration_corrupt("主机配置标识"))?;
        ConfigDocumentPath::Host(id.to_owned())
            .relative_path()
            .map_err(|_| AppError::configuration_corrupt("主机配置标识"))?;
        ids.push(HostId::parse(id).map_err(|_| AppError::configuration_corrupt("主机配置标识"))?);
    }
    ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    Ok(ids)
}
